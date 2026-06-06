//! Link the Hypervisor framework, but only on macOS. Off macOS the backend is
//! cfg'd to an empty module, so there is nothing to link and the workspace still
//! builds for a quick cross-platform `cargo check`.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-lib=framework=Hypervisor");
    }
}
