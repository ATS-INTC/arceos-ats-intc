const NET_DEV_FEATURES: &[&str] = &["virtio-net"];
const BLOCK_DEV_FEATURES: &[&str] = &["ramdisk", "virtio-blk"];
const DISPLAY_DEV_FEATURES: &[&str] = &["virtio-gpu"];

fn has_feature(feature: &str) -> bool {
    std::env::var(format!(
        "CARGO_FEATURE_{}",
        feature.to_uppercase().replace('-', "_")
    ))
    .is_ok()
}

fn enable_cfg(key: &str, value: &str) {
    println!("cargo:rustc-cfg={key}=\"{value}\"");
}

fn main() {
    // Generate cfgs like `net_dev="virtio-net"`. For each device category, if
    // no device is selected, `dummy` is selected.
    for (dev_kind, feat_list) in [
        ("net", NET_DEV_FEATURES),
        ("block", BLOCK_DEV_FEATURES),
        ("display", DISPLAY_DEV_FEATURES),
    ] {
        if !has_feature(dev_kind) {
            continue;
        }

        let mut selected = false;
        for feat in feat_list {
            let env_var = format!("CARGO_FEATURE_{}", feat.to_uppercase().replace('-', "_"));
            if std::env::var(env_var).is_ok() {
                enable_cfg(&format!("{dev_kind}_dev"), feat);
                selected = true;
                break;
            }
        }
        if !selected {
            enable_cfg(&format!("{dev_kind}_dev"), "dummy");
        }
    }
}
