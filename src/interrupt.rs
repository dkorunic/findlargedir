use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::{Context, Error};
use signal_hook::consts::TERM_SIGNALS;
use signal_hook::flag::register;

/// Sets up a handler for process interruption signals (each signal in `TERM_SIGNALS`).
/// This function configures a handler that will set a shared atomic boolean to `true`
/// whenever an interruption signal is received, indicating that the process should shut down.
///
/// # Arguments
/// * `shutdown` - An `&Arc<AtomicBool>` shared among threads, used to signal shutdown when set to `true`.
///
/// # Returns
/// Returns `Ok(())` if the handler is successfully set, or an `Error` if any issues occur during setup.
///
/// # Errors
/// Returns an error if the Ctrl-C handler cannot be set, encapsulated in an `anyhow::Error`.
pub fn setup_interrupt_handler(
    shutdown: &Arc<AtomicBool>,
) -> Result<(), Error> {
    for sig in TERM_SIGNALS {
        register(*sig, shutdown.clone())
            .context("Unable to set {sig} handler")?;
    }

    Ok(())
}
