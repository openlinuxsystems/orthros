//! Passes the kernel linker script to the linker for bare-metal builds.

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "none" {
        let dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
        println!("cargo:rustc-link-arg-bins=-T{dir}/linker.ld");
    }
    println!("cargo:rerun-if-changed=linker.ld");
}
