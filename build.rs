//! Embed app.manifest into the exe (MSVC linker). It sets
//! consoleAllocationPolicy=detached (no console window when started from Explorer or
//! the Run key; a terminal launch still shares the terminal's console), Common
//! Controls v6, per-monitor DPI awareness and Windows 10/11 compatibility.
fn main() {
    let manifest = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("app.manifest");
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());
    }
}
