#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
// Release builds are GUI-only on Windows; debug builds keep the console for logs.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;

fn main() {
    // `--test-performance[=grid,editor,…]`: the isolated performance window.
    if let Some(paths) = app::performance::requested() {
        app::performance::run(paths);
        return;
    }
    // Without a writable data folder the application still runs, without logs.
    let _logs = jovru::logging::init(&jovru::connection_store::data_dir()).ok();
    jovru::telemetry::startup_began();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "Jovru started");
    app::run();
    tracing::info!("Jovru exited");
}
