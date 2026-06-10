// SPDX-FileCopyrightText: 2022 Dinko Korunic <dinko.korunic@gmail.com>
//
// SPDX-License-Identifier: MIT

use std::io;
use std::path::{Path, PathBuf};
use std::thread;

use anyhow::{Error, anyhow, bail};
use lexopt::prelude::*;
use normpath::PathExt;

#[derive(Default, Debug, Clone)]
pub struct Args {
    /// Follow symlinks
    pub follow_symlinks: bool,

    /// Perform accurate directory entry counting
    pub accurate: bool,

    /// Do not cross mount points
    pub one_filesystem: bool,

    /// Calibration batch size (raised to a 1000-file minimum)
    pub calibration_count: u64,

    /// Calibration filename length, matched to typical entries (1..=255)
    pub calibration_name_length: usize,

    /// Alert threshold count (print the estimate)
    pub alert_threshold: u64,

    /// Blacklist threshold count (print the estimate and stop deeper scan)
    pub blacklist_threshold: u64,

    /// Number of threads to use when scanning (2..=65535)
    pub threads: usize,

    /// Seconds between status updates, set to 0 to disable
    pub updates: u64,

    /// Skip calibration and use this bytes-per-entry ratio directly
    pub size_inode_ratio: u64,

    /// Custom calibration directory path
    pub calibration_path: Option<PathBuf>,

    /// Directories to exclude from scanning
    pub skip_path: Vec<PathBuf>,

    /// Paths to check for large directories
    pub path: Vec<PathBuf>,
}

impl Args {
    /// Parses arguments from the process environment, printing an error and
    /// exiting with status 2 on failure (mirroring clap's exit-on-error).
    #[must_use]
    pub fn parse() -> Args {
        match Self::from_parser(lexopt::Parser::from_env()) {
            Ok(args) => args,
            Err(e) => {
                eprintln!("findlargedir: {e}");
                std::process::exit(2);
            }
        }
    }

    /// Parses arguments from an iterator that does **not** include the binary
    /// name. Test entry point that shares the exact production parsing path.
    #[cfg(test)]
    fn try_parse_from<I>(args: I) -> Result<Args, Error>
    where
        I: IntoIterator,
        I::Item: Into<std::ffi::OsString>,
    {
        Self::from_parser(lexopt::Parser::from_args(args))
    }

    /// The single parsing routine driving the lexopt loop. Defaults are seeded
    /// up front, then each recognised option overrides its slot; `--help` and
    /// `--version` print and exit, and at least one `<PATH>` is required.
    fn from_parser(mut parser: lexopt::Parser) -> Result<Args, Error> {
        let mut args = Args {
            one_filesystem: true,
            calibration_count: default_calibration_count(),
            calibration_name_length: crate::calibrate::DEFAULT_NAME_LEN,
            alert_threshold: crate::walk::ALERT_COUNT,
            blacklist_threshold: crate::walk::BLACKLIST_COUNT,
            threads: default_threads(),
            updates: crate::walk::STATUS_SECONDS,
            ..Args::default()
        };

        while let Some(arg) = parser.next()? {
            match arg {
                Short('f') | Long("follow-symlinks") => {
                    args.follow_symlinks = true;
                }
                Short('a') | Long("accurate") => args.accurate = true,
                Short('o') | Long("one-filesystem") => {
                    args.one_filesystem = true;
                }
                Short('m') | Long("cross-filesystem") => {
                    args.one_filesystem = false;
                }
                Short('c') | Long("calibration-count") => {
                    args.calibration_count =
                        parse_calibration_count(&parser.value()?.string()?)?;
                }
                Short('n') | Long("calibration-name-length") => {
                    args.calibration_name_length =
                        parse_name_length(&parser.value()?.string()?)?;
                }
                Short('A') | Long("alert-threshold") => {
                    args.alert_threshold = parser.value()?.parse()?;
                }
                Short('B') | Long("blacklist-threshold") => {
                    args.blacklist_threshold = parser.value()?.parse()?;
                }
                Short('x') | Long("threads") => {
                    args.threads = parse_threads(&parser.value()?.string()?)?;
                }
                Short('p') | Long("updates") => {
                    args.updates = parser.value()?.parse()?;
                }
                Short('i') | Long("size-inode-ratio") => {
                    args.size_inode_ratio = parser.value()?.parse()?;
                }
                Short('t') | Long("calibration-path") => {
                    args.calibration_path =
                        Some(PathBuf::from(parser.value()?));
                }
                Short('s') | Long("skip-path") => {
                    args.skip_path
                        .push(parse_skip_paths(&parser.value()?.string()?)?);
                }
                Short('h') | Long("help") => {
                    print_help();
                    std::process::exit(0);
                }
                Short('V') | Long("version") => {
                    print_version();
                    std::process::exit(0);
                }
                Value(path) => {
                    args.path.push(parse_paths(&path.string()?)?);
                }
                _ => return Err(arg.unexpected().into()),
            }
        }

        if args.path.is_empty() {
            bail!("at least one PATH argument is required");
        }

        Ok(args)
    }
}

/// Default `-c` calibration batch size; matches [`crate::calibrate`].
fn default_calibration_count() -> u64 {
    crate::calibrate::DEFAULT_TEST_COUNT
}

/// Default thread count: the available parallelism, falling back to 2.
fn default_threads() -> usize {
    thread::available_parallelism().map_or(2, std::num::NonZeroUsize::get)
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

/// Rejects a calibration count below 1 (clap previously enforced `1..`).
fn parse_calibration_count(x: &str) -> Result<u64, Error> {
    let v = x.parse::<u64>()?;
    if v >= 1 {
        Ok(v)
    } else {
        Err(anyhow!("calibration count should be at least 1"))
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

/// Prints the program version to stdout.
fn print_version() {
    println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
}

/// Prints the usage/help text to stdout. lexopt generates none, so this is
/// hand-maintained to match the documented option set.
fn print_help() {
    println!("{}", env!("CARGO_PKG_DESCRIPTION"));
    println!();
    println!("Usage: {} [OPTIONS] <PATH>...", env!("CARGO_PKG_NAME"));
    println!();
    println!("Arguments:");
    println!("  <PATH>...  Paths to check for large directories");
    println!();
    println!("Options:");
    println!("  -f, --follow-symlinks              Follow symlinks");
    println!(
        "  -a, --accurate                     Perform accurate directory entry counting"
    );
    println!(
        "  -o, --one-filesystem               Do not cross mount points (default)"
    );
    println!(
        "  -m, --cross-filesystem             Cross mount points (calibrate each filesystem)"
    );
    println!(
        "  -c, --calibration-count <N>        Calibration batch size (raised to a 1000-file minimum) [default: {}]",
        default_calibration_count()
    );
    println!(
        "  -n, --calibration-name-length <N>  Calibration filename length (1..=255) [default: {}]",
        crate::calibrate::DEFAULT_NAME_LEN
    );
    println!(
        "  -A, --alert-threshold <N>          Alert threshold count (print the estimate) [default: {}]",
        crate::walk::ALERT_COUNT
    );
    println!(
        "  -B, --blacklist-threshold <N>      Blacklist threshold count (print the estimate and stop deeper scan) [default: {}]",
        crate::walk::BLACKLIST_COUNT
    );
    println!(
        "  -x, --threads <N>                  Number of threads to use when scanning (2..=65535) [default: CPUs]"
    );
    println!(
        "  -p, --updates <SECONDS>            Seconds between status updates, set to 0 to disable [default: {}]",
        crate::walk::STATUS_SECONDS
    );
    println!(
        "  -i, --size-inode-ratio <N>         Skip calibration and use this bytes-per-entry ratio directly [default: 0]"
    );
    println!(
        "  -t, --calibration-path <PATH>      Custom calibration directory path"
    );
    println!(
        "  -s, --skip-path <PATH>             Directories to exclude from scanning (repeatable)"
    );
    println!("  -h, --help                         Print help");
    println!("  -V, --version                      Print version");
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
        Args, oversubscription_warning, parse_calibration_count,
        parse_name_length, parse_paths, parse_skip_paths, parse_threads,
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

    mod parse_calibration_count {
        use super::*;

        /// One is the documented floor; it must be accepted.
        #[test]
        fn accepts_minimum() {
            assert_eq!(parse_calibration_count("1").unwrap(), 1);
        }

        /// Zero was rejected by clap's `range(1..)`; preserve that.
        #[test]
        fn rejects_zero() {
            assert!(parse_calibration_count("0").is_err());
        }

        /// Non-numeric input is a parse error.
        #[test]
        fn rejects_non_numeric() {
            assert!(parse_calibration_count("abc").is_err());
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

    mod parse {
        use super::*;

        /// Helper: a fresh existing directory usable as a positional path.
        fn dir() -> TempDir {
            TempDir::new().unwrap()
        }

        /// At least one `<PATH>` is required; none is an error.
        #[test]
        fn requires_at_least_one_path() {
            assert!(Args::try_parse_from(Vec::<&str>::new()).is_err());
        }

        /// A lone existing directory parses into a single search root.
        #[test]
        fn parses_single_positional_path() {
            let d = dir();
            let a =
                Args::try_parse_from([d.path().to_str().unwrap()]).unwrap();
            assert_eq!(a.path.len(), 1);
        }

        /// Multiple positionals accumulate in order.
        #[test]
        fn parses_multiple_positional_paths() {
            let d1 = dir();
            let d2 = dir();
            let a = Args::try_parse_from([
                d1.path().to_str().unwrap(),
                d2.path().to_str().unwrap(),
            ])
            .unwrap();
            assert_eq!(a.path.len(), 2);
        }

        /// With no flags, every field falls back to its documented default.
        #[test]
        fn applies_defaults_when_flags_absent() {
            let d = dir();
            let a =
                Args::try_parse_from([d.path().to_str().unwrap()]).unwrap();
            assert!(!a.follow_symlinks);
            assert!(!a.accurate);
            assert!(a.one_filesystem);
            assert_eq!(a.size_inode_ratio, 0);
            assert_eq!(
                a.calibration_name_length,
                crate::calibrate::DEFAULT_NAME_LEN
            );
            assert_eq!(
                a.calibration_count,
                crate::calibrate::DEFAULT_TEST_COUNT
            );
            assert_eq!(a.alert_threshold, crate::walk::ALERT_COUNT);
            assert_eq!(a.blacklist_threshold, crate::walk::BLACKLIST_COUNT);
            assert_eq!(a.updates, crate::walk::STATUS_SECONDS);
            assert!(a.calibration_path.is_none());
            assert!(a.skip_path.is_empty());
        }

        /// Short presence flags set their booleans to true.
        #[test]
        fn short_presence_flags_set_true() {
            let d = dir();
            let p = d.path().to_str().unwrap();
            let a = Args::try_parse_from(["-f", "-a", p]).unwrap();
            assert!(a.follow_symlinks);
            assert!(a.accurate);
        }

        /// Long presence flags set their booleans to true.
        #[test]
        fn long_presence_flags_set_true() {
            let d = dir();
            let p = d.path().to_str().unwrap();
            let a =
                Args::try_parse_from(["--follow-symlinks", "--accurate", p])
                    .unwrap();
            assert!(a.follow_symlinks);
            assert!(a.accurate);
        }

        /// `-m`/`--cross-filesystem` flips the default-on one-filesystem off.
        #[test]
        fn cross_filesystem_disables_one_filesystem() {
            let d = dir();
            let p = d.path().to_str().unwrap();
            assert!(!Args::try_parse_from(["-m", p]).unwrap().one_filesystem);
            assert!(
                !Args::try_parse_from(["--cross-filesystem", p])
                    .unwrap()
                    .one_filesystem
            );
        }

        /// `-o` is accepted and keeps one-filesystem on (its default).
        #[test]
        fn one_filesystem_flag_keeps_true() {
            let d = dir();
            let p = d.path().to_str().unwrap();
            assert!(Args::try_parse_from(["-o", p]).unwrap().one_filesystem);
        }

        /// Numeric options parse into their slots.
        #[test]
        fn parses_numeric_options() {
            let d = dir();
            let p = d.path().to_str().unwrap();
            let a = Args::try_parse_from([
                "-c", "5", "-A", "7", "-B", "9", "-x", "4", "-p", "3", "-i",
                "42", p,
            ])
            .unwrap();
            assert_eq!(a.calibration_count, 5);
            assert_eq!(a.alert_threshold, 7);
            assert_eq!(a.blacklist_threshold, 9);
            assert_eq!(a.threads, 4);
            assert_eq!(a.updates, 3);
            assert_eq!(a.size_inode_ratio, 42);
        }

        /// `--long=value` syntax is supported for option values.
        #[test]
        fn long_value_with_equals() {
            let d = dir();
            let p = d.path().to_str().unwrap();
            let a = Args::try_parse_from(["--threads=4", p]).unwrap();
            assert_eq!(a.threads, 4);
        }

        /// `-s` is repeatable and accumulates skip paths.
        #[test]
        fn skip_path_repeats_into_vec() {
            let d = dir();
            let p = d.path().to_str().unwrap();
            let a = Args::try_parse_from(["-s", "/a", "-s", "/b", p]).unwrap();
            assert_eq!(a.skip_path.len(), 2);
        }

        /// `-t` sets the optional calibration directory.
        #[test]
        fn calibration_path_sets_option() {
            let d = dir();
            let p = d.path().to_str().unwrap();
            let a = Args::try_parse_from(["-t", "/tmp", p]).unwrap();
            assert!(a.calibration_path.is_some());
        }

        /// An unknown option is an error, not silently ignored.
        #[test]
        fn rejects_unknown_flag() {
            let d = dir();
            let p = d.path().to_str().unwrap();
            assert!(Args::try_parse_from(["--bogus", p]).is_err());
        }

        /// A non-numeric value for a numeric option is an error.
        #[test]
        fn rejects_invalid_numeric() {
            let d = dir();
            let p = d.path().to_str().unwrap();
            assert!(Args::try_parse_from(["-x", "abc", p]).is_err());
        }

        /// The thread range (2..=65535) is enforced at parse time.
        #[test]
        fn enforces_thread_range() {
            let d = dir();
            let p = d.path().to_str().unwrap();
            assert!(Args::try_parse_from(["-x", "1", p]).is_err());
        }

        /// The name-length range (1..=255) is enforced at parse time.
        #[test]
        fn enforces_name_length_range() {
            let d = dir();
            let p = d.path().to_str().unwrap();
            assert!(Args::try_parse_from(["-n", "0", p]).is_err());
        }

        /// The calibration-count floor (>=1) is enforced at parse time.
        #[test]
        fn enforces_calibration_count_floor() {
            let d = dir();
            let p = d.path().to_str().unwrap();
            assert!(Args::try_parse_from(["-c", "0", p]).is_err());
        }

        /// A positional that is not an existing directory is rejected.
        #[test]
        fn rejects_nonexistent_positional() {
            assert!(
                Args::try_parse_from(["/nonexistent/xyz/abc123"]).is_err()
            );
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
