use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Error};

/// Termination signal handler subscribed to SIGINT, SIGTERM and SIGHUP
pub fn setup_interrupt_handler(shutdown: Arc<AtomicBool>) -> Result<(), Error> {
    ctrlc::set_handler(move || {
        shutdown.store(true, Ordering::SeqCst);
    })
    .context("Unable to set Ctrl-C handler")?;

    Ok(())
}
