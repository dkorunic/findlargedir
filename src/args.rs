use std::path::{Path, PathBuf};
use std::thread;

use anstyle::AnsiColor;
use anyhow::{Error, anyhow};
use clap::Parser;
use clap::ValueHint;
use clap::builder::{ValueParser, styling::Styles};
use normpath::PathExt;

const STYLES: Styles = Styles::styled()
    .header(AnsiColor::Yellow.on_default())
    .usage(AnsiColor::Green.on_default())
    .literal(AnsiColor::Green.on_default())
    .placeholder(AnsiColor::Green.on_default());

#[derive(Parser, Default, Debug, Clone)]
#[clap(author, version, about, long_about = None, styles=STYLES)]
pub struct Args {
    /// Follow symlinks
    #[clap(short = 'f', long, action = clap::ArgAction::Set, default_value_t = false)]
    pub follow_symlinks: bool,

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
    #[clap(short = 'x', long, value_parser = ValueParser::new(parse_threads), default_value_t = thread::available_parallelism().map(| n | n.get()).unwrap_or(2)
    )]
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
    #[clap(required = true, value_parser = ValueParser::new(parse_paths), value_hint = ValueHint::AnyPath
    )]
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

/// Parses a string into a `PathBuf`, checking if the path is a directory and exists.
///
/// # Arguments
///
/// * `x` - A string slice to be parsed into a `PathBuf`.
///
/// # Returns
///
/// * `Result<PathBuf, Error>` - An `Ok` variant containing a normalized `PathBuf` if the path is an existing directory,
///    or an `Err` variant with an error message if the path does not exist or is not a directory.
fn parse_paths(x: &str) -> Result<PathBuf, Error> {
    let p = Path::new(x);

    if directory_exists(p) {
        Ok(p.normalize()?.into_path_buf())
    } else {
        Err(anyhow!("'{x}' is not an existing directory"))
    }
}

/// Checks if the given path is a directory and exists.
///
/// # Arguments
///
/// * `x` - A reference to the path to check.
///
/// # Returns
///
/// * `bool` - `true` if the path is an existing directory, `false` otherwise.
#[inline]
fn directory_exists(x: &Path) -> bool {
    x.is_dir() && x.normalize().is_ok()
}
