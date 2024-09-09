use std::path::PathBuf;
use std::thread;

use anyhow::{anyhow, Error};
use clap::builder::ValueParser;
use clap::Parser;
use clap::ValueHint;

#[derive(Parser, Default, Debug, Clone)]
#[clap(author, version, about, long_about = None)]
pub struct Args {
    /// Perform accurate directory entry counting
    #[clap(short = 'a', long, action = clap::ArgAction::Set, default_value_t = false)]
    pub accurate: bool,

    /// Do not cross mount points
    #[clap(short = 'o', long, action = clap::ArgAction::Set, default_value_t = true)]
    pub one_filesystem: bool,

    /// Calibration directory file count
    #[clap(short = 'c', long, value_parser, default_value_t = crate::calibrate::DEFAULT_TEST_COUNT)]
    pub calibration_count: u64,

    /// Alert threshold count (print the estimate)
    #[clap(short = 'A', long, value_parser, default_value_t = crate::walk::ALERT_COUNT)]
    pub alert_threshold: u64,

    /// Blacklist threshold count (print the estimate and stop deeper scan)
    #[clap(short = 'B', long, value_parser, default_value_t = crate::walk::BLACKLIST_COUNT)]
    pub blacklist_threshold: u64,

    /// Number of threads to use when calibrating and scanning
    #[clap(short = 'x', long, value_parser = ValueParser::new(parse_threads), default_value_t = thread::available_parallelism().map(| n | n.get()).unwrap_or(1))]
    pub threads: usize,

    /// Seconds between status updates, set to 0 to disable
    #[clap(short = 'p', long, value_parser, default_value_t = crate::walk::STATUS_SECONDS)]
    pub updates: u64,

    /// Skip calibration and provide directory entry to inode size ratio (typically ~21-32)
    #[clap(short = 'i', long, value_parser, default_value_t = 0u64)]
    pub size_inode_ratio: u64,

    /// Custom calibration directory path
    #[clap(short = 't', long, value_parser, value_hint = ValueHint::AnyPath)]
    pub calibration_path: Option<PathBuf>,

    /// Directories to exclude from scanning
    #[clap(short = 's', long, value_parser, value_hint = ValueHint::AnyPath)]
    pub skip_path: Vec<PathBuf>,

    /// Paths to check for large directories
    #[clap(required = true, value_parser, value_hint = ValueHint::AnyPath)]
    pub path: Vec<PathBuf>,
}

/// Parse and validate threads option
fn parse_threads(x: &str) -> Result<usize, Error> {
    match x.parse::<usize>() {
        Ok(v) => match v {
            v if !(2..=65535).contains(&v) => {
                Err(anyhow!("threads should be in (2..65536) range"))
            }
            v => Ok(v),
        },
        Err(e) => Err(Error::from(e)),
    }
}
