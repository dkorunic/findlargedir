use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};

/// Spinner animation frames.
const PROGRESS_CHARS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";

/// Spinner tick interval in milliseconds.
const PROGRESS_TICK: u64 = 80;

/// Builds a self-ticking spinner so long phases (calibration, walking) show
/// liveness without the caller having to pump progress updates.
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
