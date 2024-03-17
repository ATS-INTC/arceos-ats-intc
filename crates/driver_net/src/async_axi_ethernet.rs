use core::{convert::From, future::poll_fn};
use core::ptr::NonNull;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec;
use ats_intc::{AtsIntc, Task};
use driver_common::{BaseDriverOps, DevError, DevResult, DeviceType};
use axi_dma::*;
use axi_ethernet::*;
use spin::{Mutex, Lazy, Once};
use crossbeam::queue::SegQueue;

use crate::{EthernetAddress, NetBufPtr, NetDriverOps};

extern crate alloc;

const BD_CNT: usize = 1024;
const MAC_ADDR: [u8; 6] = [0x00, 0x0A, 0x35, 0x01, 0x02, 0x03];
const RX_BUFFER_SIZE: usize = 9000;

pub struct AsyncDriver {
    pub dma: Arc<AxiDma>,
    eth: Arc<Mutex<AxiEthernet>>,
}

pub static ASYNC_DRIVER: Once<AsyncDriver> = Once::new();
pub static RX_BUFS: Lazy<SegQueue<BufPtr>> = Lazy::new(|| SegQueue::new());

/// The Axi Ethernet device driver
pub struct AxiEth {
    tx_transfers: VecDeque<Transfer>,
    mac_address: [u8; 6]
}

impl AxiEth {
    /// Creates a net Axi NIC instance and initialize, or returns a error if
    /// any step fails.
    pub fn init(eth_base: usize, dma_base: usize) -> DevResult<Self> {
        // The default configuration of the AxiDMA
        let cfg = AxiDmaConfig {
            base_address: dma_base,
            rx_channel_offset: 0x30,
            tx_channel_offset: 0,
            has_sts_cntrl_strm: false,
            is_micro_dma: false,
            has_mm2s: true,
            has_mm2s_dre: false,
            mm2s_data_width: 32,
            mm2s_burst_size: 16,
            has_s2mm: true,
            has_s2mm_dre: false,
            s2mm_data_width: 32,
            s2mm_burst_size: 16,
            has_sg: true,
            sg_length_width: 16,
            addr_width: 32,
        };
        let dma = Arc::new(AxiDma::new(cfg));

        dma.reset().map_err(|_| DevError::ResourceBusy)?;
        // enable cyclic mode
        dma.cyclic_enable();
        // init cyclic block descriptor
        dma.tx_channel_create(BD_CNT).map_err(|_| DevError::Unsupported)?;
        dma.rx_channel_create(BD_CNT).map_err(|_| DevError::Unsupported)?;
        // enable tx & rx intr
        dma.intr_enable();


        let eth = Arc::new(Mutex::new(AxiEthernet::new(eth_base, dma_base)));

        let mut eth_inner = eth.lock();
        eth_inner.reset();
        let options = eth_inner.get_options();
        eth_inner.set_options(options | XAE_JUMBO_OPTION);
        eth_inner.detect_phy();
        let speed = eth_inner.get_phy_speed_ksz9031();
        log::trace!("speed is: {}", speed);
        eth_inner.set_operating_speed(speed as u16);
        if speed == 0 {
            eth_inner.link_status = LinkStatus::EthLinkDown;
        } else {
            eth_inner.link_status = LinkStatus::EthLinkUp;
        }
        eth_inner.set_mac_address(&MAC_ADDR);
        log::trace!("link_status: {:?}", eth_inner.link_status);
        eth_inner.enable_rx_memovr();
        eth_inner.enable_rx_rject();
        eth_inner.enable_rx_cmplt();
        eth_inner.start();
        drop(eth_inner);
        ASYNC_DRIVER.call_once(|| AsyncDriver {
            dma,
            eth
        });
        Ok(Self {
            tx_transfers: VecDeque::new(),
            mac_address: MAC_ADDR
        })
    }
}

impl BaseDriverOps for AxiEth {
    fn device_name(&self) -> &str {
        "axi-ethernet"
    }

    fn device_type(&self) -> DeviceType {
        DeviceType::Net
    }
}

impl NetDriverOps for AxiEth {
    fn mac_address(&self) -> EthernetAddress {
        EthernetAddress(self.mac_address)
    }

    fn rx_queue_size(&self) -> usize {
        0x8000
    }

    fn tx_queue_size(&self) -> usize {
        0x8000
    }

    fn can_receive(&self) -> bool {
        !RX_BUFS.is_empty()
    }

    fn can_transmit(&self) -> bool {
        // Default implementation is return true forever.
        true
    }

    fn recycle_rx_buffer(&mut self, rx_buf: NetBufPtr) -> DevResult {
        let rx_buf = netbuf2buf(rx_buf)?;
        drop_buf(rx_buf);
        Ok(())
    }

    fn recycle_tx_buffers(&mut self) -> DevResult {
        if let Some(transfer) = self.tx_transfers.pop_front() {
            let buf = transfer.wait().map_err(|_| panic!("Unexpected error"))?;
            drop_buf(buf);
        }
        Ok(())
    }

    fn receive(&mut self) -> DevResult<NetBufPtr> {
        if !self.can_receive() {
            return Err(DevError::Again);
        }
        Ok(NetBufPtr::from(RX_BUFS.pop().unwrap()))
    }

    fn transmit(&mut self, tx_buf: NetBufPtr) -> DevResult {
        let tx_buf = netbuf2buf(tx_buf)?;
        match ASYNC_DRIVER.get().unwrap().dma.tx_submit(tx_buf) {
            Ok(transfer) => {
                self.tx_transfers.push_back(transfer);
                Ok(())
            },
            Err(err) => panic!("Unexpected err: {:?}", err),
        }
    }

    fn alloc_tx_buffer(&mut self, size: usize) -> DevResult<NetBufPtr> {
        let slice = vec![0u8; size].into_boxed_slice();
        Ok(NetBufPtr::from(slice))
    }

    fn is_async(&self) -> bool {
        true
    }

    fn async_driver_run() -> impl FnOnce() + Send + 'static {
        || {
            static ATSINTC: AtsIntc = AtsIntc::new(0xffff_ffc0_0000_0000 + 0x1000_0000);
            let task_ref = Task::new(Box::pin(receive()), 0, ats_intc::TaskType::Other, &ATSINTC);
            ATSINTC.ps_push(task_ref, 0);
            loop{
                if let Some(task) = ATSINTC.ps_fetch() {
                    let _ = task.clone().poll();
                    ATSINTC.intr_push(4, task);
                }
            }
        }
    }
}

impl From<Box<[u8]>> for NetBufPtr {
    fn from(value: Box<[u8]>) -> Self {
        let len = value.len();
        let buf_ptr = Box::into_raw(value) as *mut u8;
        Self { 
            raw_ptr: NonNull::dangling(), 
            buf_ptr: NonNull::new(buf_ptr).unwrap(), 
            len,
        }
    }
}

impl From<BufPtr> for NetBufPtr {
    fn from(mut value: BufPtr) -> Self {
        let len = value.len();
        Self { 
            raw_ptr: NonNull::dangling(), 
            buf_ptr: NonNull::new(value.as_mut_ptr()).unwrap(), 
            len,
        }
    }
}

fn netbuf2buf(netbuf: NetBufPtr) -> DevResult<BufPtr> {
    let buf_ptr = netbuf.buf_ptr;
    let len = netbuf.len;
    Ok(BufPtr::new(buf_ptr, len))
}

fn drop_buf(mut bufptr: BufPtr) {
    let len = bufptr.len();
    let raw_ptr = bufptr.as_mut_ptr();
    let slice = unsafe {
        core::slice::from_raw_parts_mut(raw_ptr, len)
    };
    let _buf = unsafe { Box::from_raw(slice) };
}

async fn receive() -> i32 {
    let axi_net = ASYNC_DRIVER.get().unwrap();
    loop {
        let slice = vec![0u8; RX_BUFFER_SIZE].into_boxed_slice();
        let len = slice.len();
        let rx_buf = BufPtr::new(NonNull::new(Box::into_raw(slice) as *mut u8).unwrap(), len);
        let buf = axi_net.dma.rx_submit(rx_buf).unwrap().await;
        RX_BUFS.push(buf);
    }
}

