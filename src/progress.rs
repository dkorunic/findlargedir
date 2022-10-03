use indicatif::{ProgressBar, ProgressStyle};
use std::time::Duration;

/// Default tick chars
const PROGRESS_CHARS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";

/// Default tick in milliseconds
const PROGRESS_TICK: u64 = 80;

/// Setup a `ProgressBar` with spinner, setup `PROGRESS_CHARS` for spinner and enable steady tick
/// every `PROGRESS_TICK` seconds
pub fn new_spinner(msg: String) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.enable_steady_tick(Duration::from_millis(PROGRESS_TICK));
    pb.set_style(ProgressStyle::default_spinner().tick_chars(PROGRESS_CHARS));
    pb.set_message(msg);

    pb
}
