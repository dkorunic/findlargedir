// SPDX-FileCopyrightText: 2022 Dinko Korunic <dinko.korunic@gmail.com>
//
// SPDX-License-Identifier: MIT

use std::io;
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

    /// Calibration batch size (raised to a 1000-file minimum)
    #[clap(short = 'c', long, value_parser = clap::value_parser!(u64).range(1..), default_value_t = crate::calibrate::DEFAULT_TEST_COUNT)]
    pub calibration_count: u64,

    /// Calibration filename length, matched to typical entries (1..=255)
    #[clap(short = 'n', long, value_parser = ValueParser::new(parse_name_length), default_value_t = crate::calibrate::DEFAULT_NAME_LEN)]
    pub calibration_name_length: usize,

    /// Alert threshold count (print the estimate)
    #[clap(short = 'A', long, value_parser, default_value_t = crate::walk::ALERT_COUNT)]
    pub alert_threshold: u64,

    /// Blacklist threshold count (print the estimate and stop deeper scan)
    #[clap(short = 'B', long, value_parser, default_value_t = crate::walk::BLACKLIST_COUNT)]
    pub blacklist_threshold: u64,

    /// Number of threads to use when scanning (2..=65535)
    #[clap(short = 'x', long, value_parser = ValueParser::new(parse_threads), default_value_t = thread::available_parallelism().map(| n | n.get()).unwrap_or(2)
    )]
    pub threads: usize,

    /// Seconds between status updates, set to 0 to disable
    #[clap(short = 'p', long, value_parser, default_value_t = crate::walk::STATUS_SECONDS)]
    pub updates: u64,

    /// Skip calibration and use this bytes-per-entry ratio directly (e.g. the value a prior run reported)
    #[clap(short = 'i', long, value_parser, default_value_t = 0u64)]
    pub size_inode_ratio: u64,

    /// Custom calibration directory path
    #[clap(short = 't', long, value_parser, value_hint = ValueHint::AnyPath)]
    pub calibration_path: Option<PathBuf>,

    /// Directories to exclude from scanning
    #[clap(short = 's', long, value_parser = ValueParser::new(parse_skip_paths), value_hint = ValueHint::AnyPath)]
    pub skip_path: Vec<PathBuf>,

    /// Paths to check for large directories
    #[clap(required = true, value_parser = ValueParser::new(parse_paths), value_hint = ValueHint::AnyPath
    )]
    pub path: Vec<PathBuf>,
}

/// Rejects calibration name lengths outside `1..=255`; 255 is `NAME_MAX` on
/// most filesystems, and a length of 0 would create empty-named files.
fn parse_name_length(x: &str) -> Result<usize, Error> {
    let v = x.parse::<usize>()?;
    if (1..=255).contains(&v) {
        Ok(v)
    } else {
        Err(anyhow!("calibration name length should be in (1..=255) range"))
    }
}

/// Rejects thread counts outside `2..=65535`.
fn parse_threads(x: &str) -> Result<usize, Error> {
    let v = x.parse::<usize>()?;
    if (2..=65535).contains(&v) {
        Ok(v)
    } else {
        Err(anyhow!("threads should be in (2..=65535) range"))
    }
}

/// Normalises a search root, rejecting anything that is not an existing
/// directory so the walk never starts from a missing or non-dir path.
fn parse_paths(x: &str) -> Result<PathBuf, Error> {
    let p = Path::new(x);

    if p.is_dir() {
        Ok(p.normalize()?.into_path_buf())
    } else {
        Err(anyhow!("'{x}' is not an existing directory"))
    }
}

/// Normalises a skip-path string without requiring the path to exist, so a
/// caller can exclude a path (e.g. a virtual filesystem) that is absent on this
/// host without aborting the whole scan. Existing paths are normalised as
/// usual; an absent one falls back to its lexical form (normpath's
/// `normalize_virtually` is Windows-only). Other errors still surface.
fn parse_skip_paths(x: &str) -> Result<PathBuf, Error> {
    let p = Path::new(x);
    match p.normalize() {
        Ok(n) => Ok(n.into_path_buf()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(p.to_path_buf()),
        Err(e) => Err(e.into()),
    }
}

/// Warns (the caller still honors the value) when `threads > available`:
/// throughput typically drops past the core count as calibration and the walk
/// already saturate the available cores.
#[must_use]
pub fn oversubscription_warning(
    threads: usize,
    available: usize,
) -> Option<String> {
    (threads > available).then(|| {
        format!(
            "--threads {threads} exceeds available parallelism \
             ({available}); throughput may decrease"
        )
    })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::{
        oversubscription_warning, parse_name_length, parse_paths,
        parse_skip_paths, parse_threads,
    };

    mod parse_name_length {
        use super::*;

        /// The accepted range is `1..=255`; the bounds must be inclusive.
        #[test]
        fn accepts_inclusive_bounds() {
            assert_eq!(parse_name_length("1").unwrap(), 1);
            assert_eq!(parse_name_length("255").unwrap(), 255);
        }

        /// Zero would create empty-named files; reject it.
        #[test]
        fn rejects_zero() {
            assert!(parse_name_length("0").is_err());
        }

        /// Above `NAME_MAX` (255) is rejected rather than silently clamped.
        #[test]
        fn rejects_above_name_max() {
            assert!(parse_name_length("256").is_err());
        }

        /// Non-numeric and empty input is a parse error, not a default.
        #[test]
        fn rejects_non_numeric() {
            assert!(parse_name_length("abc").is_err());
            assert!(parse_name_length("").is_err());
        }
    }

    mod parse_threads {
        use super::*;

        /// The accepted range is `2..=65535`; the bounds must be inclusive.
        #[test]
        fn accepts_inclusive_bounds() {
            assert_eq!(parse_threads("2").unwrap(), 2);
            assert_eq!(parse_threads("65535").unwrap(), 65535);
        }

        /// One thread is rejected: the walk reserves a worker, so 2 is the floor.
        #[test]
        fn rejects_below_minimum() {
            assert!(parse_threads("1").is_err());
            assert!(parse_threads("0").is_err());
        }

        /// Values above the range are rejected rather than silently clamped.
        #[test]
        fn rejects_above_maximum() {
            assert!(parse_threads("65536").is_err());
        }

        /// Non-numeric and empty input is a parse error, not a default.
        #[test]
        fn rejects_non_numeric() {
            assert!(parse_threads("abc").is_err());
            assert!(parse_threads("").is_err());
            assert!(parse_threads("-1").is_err());
        }
    }

    mod parse_paths {
        use super::*;

        /// An existing directory is accepted and normalised.
        #[test]
        fn accepts_existing_dir() {
            let tmp = TempDir::new().unwrap();
            assert!(parse_paths(tmp.path().to_str().unwrap()).is_ok());
        }

        /// A regular file is not a valid search root.
        #[test]
        fn rejects_regular_file() {
            let tmp = TempDir::new().unwrap();
            let file = tmp.path().join("f.txt");
            std::fs::write(&file, b"x").unwrap();
            assert!(parse_paths(file.to_str().unwrap()).is_err());
        }

        /// A missing path is rejected so the walk never starts from nothing.
        #[test]
        fn rejects_missing_path() {
            assert!(parse_paths("/nonexistent/xyz/abc123").is_err());
        }
    }

    mod parse_skip_paths {
        use super::*;

        /// An existing directory is accepted and normalised.
        #[test]
        fn accepts_existing_dir() {
            let tmp = TempDir::new().unwrap();
            assert!(parse_skip_paths(tmp.path().to_str().unwrap()).is_ok());
        }

        /// A skip path need not exist — excluding a path that is absent on this
        /// host must not abort the whole scan.
        #[test]
        fn accepts_nonexistent_path() {
            assert!(parse_skip_paths("/nonexistent/xyz/abc123").is_ok());
        }
    }

    mod oversubscription_warning {
        use super::*;

        /// A thread count above the core count returns a message naming both
        /// numbers so the user understands the tradeoff.
        #[test]
        fn warns_when_above_available() {
            let w = oversubscription_warning(16, 8);
            assert!(w.is_some());
            let msg = w.unwrap();
            assert!(msg.contains("16") && msg.contains('8'));
        }

        /// At or below the core count there is no oversubscription, so no
        /// warning is produced.
        #[test]
        fn silent_when_at_or_below_available() {
            assert!(oversubscription_warning(8, 8).is_none());
            assert!(oversubscription_warning(4, 8).is_none());
            assert!(oversubscription_warning(2, 8).is_none());
        }
    }
}
