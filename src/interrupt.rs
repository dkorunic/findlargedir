use anyhow::{Context, Error};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub fn setup_interrupt_handler(shutdown: Arc<AtomicBool>) -> Result<(), Error> {
    ctrlc::set_handler(move || {
        shutdown.store(true, Ordering::SeqCst);
    })
    .context("Unable to set Ctrl-C handler")?;

    Ok(())
}
