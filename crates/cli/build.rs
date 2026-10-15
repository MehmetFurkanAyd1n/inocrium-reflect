fn main() {
    // Windows PE has no iSCSI initiator. Loading it on first use lets the rescue media run the CLI
    // for restores; only mounts need it.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bin=inocrium=/DELAYLOAD:iscsidsc.dll");
        println!("cargo:rustc-link-arg-bin=inocrium=delayimp.lib");
    }
}
