//! Argus entry point: bootstrap, logging, the tokio runtime and the GPUI window.
//!
//! Keep this crate thin: services live in the library crates so they can be
//! exercised without a window (spec §4.1). This crate only wires them up.

mod logging;
mod paths;

use anyhow::Context as _;

fn main() -> anyhow::Result<()> {
    let argus_home = paths::argus_home();
    logging::init(&argus_home).with_context(|| {
        format!(
            "failed to initialize logging under {}",
            argus_home.display()
        )
    })?;

    // One multi-threaded runtime for all service work. The UI thread never
    // performs git I/O, diff computation, highlighting, SQLite writes or
    // process I/O (spec §4.2).
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("argus-worker")
        .build()
        .context("failed to start the tokio runtime")?;

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        home = %argus_home.display(),
        "starting Argus"
    );

    argus_ui::run_gpui_app(runtime.handle().clone());

    tracing::info!("Argus exited");
    Ok(())
}
