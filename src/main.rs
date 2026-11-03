#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
// Release builds are GUI-only on Windows; debug builds keep the console for logs.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;

fn main() {
    if std::env::args_os().any(|arg| arg == "--test-performance") {
        app::performance::run();
        return;
    }
    // Without a writable data folder the application still runs, without logs.
    let _logs = jykre::logging::init(&jykre::connection_store::data_dir()).ok();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "Jykre Studio started");
    app::run();
    tracing::info!("Jykre Studio exited");
}
