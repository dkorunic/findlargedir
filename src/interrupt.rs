use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Error};
use signal_hook::consts::TERM_SIGNALS;
use signal_hook::flag::register;

/// Wires every terminating signal in `TERM_SIGNALS` to flip `shutdown`, giving
/// the calibration and walk loops a single flag to poll for graceful exit.
///
/// # Errors
/// Fails if a signal handler cannot be registered.
pub fn setup_interrupt_handler(
    shutdown: &Arc<AtomicBool>,
) -> Result<(), Error> {
    for sig in TERM_SIGNALS {
        let name =
            signal_hook::low_level::signal_name(*sig).unwrap_or_default();
        register(*sig, shutdown.clone()).with_context(|| {
            format!("Unable to register signal handler for {name}/{sig}")
        })?;
    }

    Ok(())
}
