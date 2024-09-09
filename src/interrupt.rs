use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Error};

/// Sets up a handler for process interruption signals (SIGINT, SIGTERM, SIGHUP).
/// This function configures a handler that will set a shared atomic boolean to `true`
/// whenever an interruption signal is received, indicating that the process should shut down.
///
/// # Arguments
/// * `shutdown` - An `Arc<AtomicBool>` shared among threads, used to signal shutdown when set to `true`.
///
/// # Returns
/// Returns `Ok(())` if the handler is successfully set, or an `Error` if any issues occur during setup.
///
/// # Errors
/// Returns an error if the Ctrl-C handler cannot be set, encapsulated in an `anyhow::Error`.
pub fn setup_interrupt_handler(
    shutdown: Arc<AtomicBool>,
) -> Result<(), Error> {
    ctrlc::set_handler(move || {
        shutdown.store(true, Ordering::Release);
    })
    .context("Unable to set Ctrl-C handler")?;

    Ok(())
}
