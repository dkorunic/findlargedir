use anyhow::{Context, Error};
use spinach::term;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Termination signal handler subscribed to SIGINT, SIGTERM and SIGHUP
pub fn setup_interrupt_handler(shutdown: Arc<AtomicBool>) -> Result<(), Error> {
    ctrlc::set_handler(move || {
        term::show_cursor();
        shutdown.store(true, Ordering::SeqCst);
    })
    .context("Unable to set Ctrl-C handler")?;

    Ok(())
}
