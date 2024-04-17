use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};

/// Default tick chars
const PROGRESS_CHARS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";

/// Default tick in milliseconds
const PROGRESS_TICK: u64 = 80;

/// Initializes a new `ProgressBar` with a spinner style.
///
/// # Arguments
/// * `msg` - A message of generic type `S` that implements `Into<String>`, which will be displayed on the spinner.
///
/// # Returns
/// Returns a `ProgressBar` object configured with a steady tick and custom spinner style.
///
/// # Examples
/// ```
/// let spinner = new_spinner("Loading...");
/// ```
pub fn new_spinner<S>(msg: S) -> ProgressBar
    where S: Into<String>
{
    // function implementation
}

pub fn new_spinner<S>(msg: S) -> ProgressBar
    where
        S: Into<String>,
{
    let pb = ProgressBar::new_spinner();
    pb.enable_steady_tick(Duration::from_millis(PROGRESS_TICK));
    pb.set_style(ProgressStyle::default_spinner().tick_chars(PROGRESS_CHARS));
    pb.set_message(msg.into());

    pb
}
