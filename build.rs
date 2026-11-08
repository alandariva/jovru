//! Embeds the product icon and the file's version details in the Windows
//! executable. GPUI loads icon resource 1 for its windows, so the window,
//! the taskbar and Explorer show `assets/icon/jykre.ico`.

const ICON: &str = "assets/icon/jykre.ico";

fn main() -> std::io::Result<()> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={ICON}");
    // The target's OS, not the one running this script.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return Ok(());
    }
    winresource::WindowsResource::new()
        .set_icon(ICON)
        // Task Manager and the file's properties show these.
        .set("ProductName", "Jykre Studio")
        .set("FileDescription", "Jykre Studio")
        // Only the application: tests and examples keep no icon.
        .compile_for("jykre")
}
