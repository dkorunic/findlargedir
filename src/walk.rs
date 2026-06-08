// SPDX-FileCopyrightText: 2022 Dinko Korunic <dinko.korunic@gmail.com>
//
// SPDX-License-Identifier: MIT

use std::fs::Metadata;
use std::fs::read_dir;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use ahash::AHashSet;
use ansi_term::Colour::{Green, Red, Yellow};
use human_format::Formatter;
use indicatif::HumanBytes;

use crate::args::Args;
use crate::calibrate::Calibration;

mod engine;

thread_local! {
    static FORMATTER: Formatter = Formatter::new();
}

/// Entry-count estimate above which a directory is flagged (yellow).
pub const ALERT_COUNT: u64 = 10_000;

/// Entry-count estimate above which a directory is flagged (red) and its
/// subtree skipped.
pub const BLACKLIST_COUNT: u64 = 100_000;

/// Default seconds between progress updates.
pub const STATUS_SECONDS: u64 = 20;

/// Walks `path` in parallel, flagging directories whose estimated entry
/// count exceeds the alert/blacklist thresholds, and returns the number of
/// directories analyzed. A blacklisted subtree is skipped, and the walk
/// stops early once `shutdown_walk` is set.
pub fn parallel_search(
    path: &Path,
    path_metadata: &Metadata,
    calibration: Calibration,
    shutdown_walk: &Arc<AtomicBool>,
    args: &Args,
    skip_path: &AHashSet<PathBuf>,
) -> u64 {
    let dir_count = Arc::new(AtomicU64::new(0));

    // Periodic progress on a dedicated thread. The channel doubles as a stop
    // signal: dropping the sender after the walk wakes the thread immediately
    // (via `Disconnected`), so it never lingers printing a stale count into a
    // later scan root.
    let progress = (args.updates > 0).then(|| {
        let dir_count = dir_count.clone();
        let sleep_delay = args.updates;
        let (tx, rx) = mpsc::channel::<()>();
        let handle = thread::spawn(move || {
            while matches!(
                rx.recv_timeout(Duration::from_secs(sleep_delay)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                let count = dir_count.load(Ordering::Relaxed);
                println!(
                    "Processed {} directories so far, next update in {} seconds",
                    Green.paint(count.to_string()),
                    sleep_delay
                );
            }
        });
        (tx, handle)
    });

    let root_dev = path_metadata.dev();
    let classify = |info: engine::DirInfo| {
        classify_dir(&info, root_dev, calibration, args, skip_path, &dir_count)
    };

    engine::walk_dirs(
        path,
        args.threads,
        args.follow_symlinks,
        shutdown_walk,
        classify,
    );

    // Stop and join the progress thread before returning.
    if let Some((tx, handle)) = progress {
        drop(tx);
        let _ = handle.join();
    }

    dir_count.load(Ordering::Relaxed)
}

/// Classifies a single directory and returns the resulting
/// [`engine::Decision`]: skips entries listed in `skip_path`, skips filesystem
/// boundaries under `--one-filesystem`, and flags directories whose estimated
/// entry count (`(size − overhead) / per_entry`) crosses the alert or blacklist
/// thresholds. A blacklist hit returns `Skip`; everything else `Descend`.
fn classify_dir(
    info: &engine::DirInfo,
    root_dev: u64,
    calibration: Calibration,
    args: &Args,
    skip_path: &AHashSet<PathBuf>,
    dir_count: &AtomicU64,
) -> engine::Decision {
    let full_path = info.path;

    // User-excluded dirs, typically virtual filesystems (/proc, /sys, /dev).
    if !skip_path.is_empty() && skip_path.contains(full_path) {
        println!(
            "Skipping further scan at {} as requested",
            full_path.display()
        );

        return engine::Decision::Skip;
    }

    // Don't cross mount points when confined to one filesystem.
    if args.one_filesystem && info.dev != root_dev {
        println!(
            "Identified filesystem boundary at {}, skipping...",
            full_path.display()
        );

        return engine::Decision::Skip;
    }

    // Counts only directories that survive every filter above.
    dir_count.fetch_add(1, Ordering::Relaxed);

    // A zero per-entry cost (interrupted or degenerate calibration) disables
    // flagging entirely.
    if calibration.per_entry == 0 {
        return engine::Decision::Descend;
    }
    let approx_files =
        info.size.saturating_sub(calibration.overhead) / calibration.per_entry;

    if approx_files > args.blacklist_threshold {
        print_offender(
            full_path,
            info.size,
            approx_files,
            args.accurate,
            true,
        );

        return engine::Decision::Skip;
    } else if approx_files > args.alert_threshold {
        print_offender(
            full_path,
            info.size,
            approx_files,
            args.accurate,
            false,
        );
    }

    engine::Decision::Descend
}

/// Prints a flagged directory: its inode size and entry count (exact via
/// `read_dir` when `accurate`, otherwise the estimate), coloured red for a
/// blacklist hit (`red_alert`) or yellow for an alert.
#[allow(clippy::cast_precision_loss)]
fn print_offender(
    full_path: &Path,
    size: u64,
    approx_files: u64,
    accurate: bool,
    red_alert: bool,
) {
    let human_files = if accurate {
        let exact_files = match read_dir(full_path) {
            Ok(r) => r.count() as u64,
            Err(e) => {
                println!(
                    "Warning: unable to get exact count for {}, falling back to approximation: {e}",
                    full_path.display()
                );
                approx_files
            }
        };
        FORMATTER.with(|f| f.format(exact_files as f64))
    } else {
        FORMATTER.with(|f| f.format(approx_files as f64))
    };

    println!(
        "Found directory {} with inode size {} and {}{} files",
        full_path.display(),
        HumanBytes(size),
        if accurate { "" } else { "approx " },
        if red_alert {
            Red.paint(human_files)
        } else {
            Yellow.paint(human_files)
        }
    );
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use ahash::AHashSet;
    use tempfile::TempDir;

    use super::{classify_dir, engine, parallel_search};
    use crate::args::Args;
    use crate::calibrate::Calibration;

    fn make_args(alert: u64, blacklist: u64, one_fs: bool) -> Arc<Args> {
        Arc::new(Args {
            alert_threshold: alert,
            blacklist_threshold: blacklist,
            one_filesystem: one_fs,
            threads: 2,
            updates: 0,
            follow_symlinks: false,
            accurate: false,
            calibration_count: 100,
            size_inode_ratio: 0,
            calibration_path: None,
            skip_path: vec![],
            path: vec![],
        })
    }

    fn no_shutdown() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    mod classify_dir {
        use super::*;

        fn cal(per_entry: u64, overhead: u64) -> Calibration {
            Calibration { per_entry, overhead }
        }

        /// Overhead is subtracted before dividing: a directory whose *raw* size
        /// would blacklist is not skipped once the fixed overhead is removed.
        /// Guards against dropping the subtraction or using the wrong field.
        #[test]
        fn overhead_is_subtracted_before_dividing() {
            // per_entry=1, blacklist=100, alert never fires.
            // (1050-1000)/1 = 50 (≤100) → Descend; raw 1050 would Skip.
            let args = make_args(u64::MAX, 100, false);
            let count = AtomicU64::new(0);
            let info =
                engine::DirInfo { path: Path::new("/d"), dev: 0, size: 1050 };
            let d = classify_dir(
                &info,
                0,
                cal(1, 1000),
                &args,
                &AHashSet::new(),
                &count,
            );
            assert!(matches!(d, engine::Decision::Descend));
            assert_eq!(count.load(Ordering::Relaxed), 1);
        }

        /// Overhead larger than the size saturates to 0 — no underflow panic,
        /// nothing flagged.
        #[test]
        fn overhead_exceeding_size_saturates_to_zero() {
            let args = make_args(0, 100, false);
            let count = AtomicU64::new(0);
            let info =
                engine::DirInfo { path: Path::new("/d"), dev: 0, size: 5000 };
            let d = classify_dir(
                &info,
                0,
                cal(1, u64::MAX),
                &args,
                &AHashSet::new(),
                &count,
            );
            assert!(matches!(d, engine::Decision::Descend));
            assert_eq!(count.load(Ordering::Relaxed), 1);
        }

        /// The blacklist comparison is strictly `>`: an estimate equal to the
        /// threshold descends; one above it skips the subtree.
        #[test]
        fn blacklist_threshold_is_exclusive() {
            let args = make_args(u64::MAX, 100, false);
            let skip = AHashSet::new();
            let eq = AtomicU64::new(0);
            let at_eq =
                engine::DirInfo { path: Path::new("/d"), dev: 0, size: 100 };
            assert!(matches!(
                classify_dir(&at_eq, 0, cal(1, 0), &args, &skip, &eq),
                engine::Decision::Descend
            ));
            let above = AtomicU64::new(0);
            let at_gt =
                engine::DirInfo { path: Path::new("/d"), dev: 0, size: 101 };
            assert!(matches!(
                classify_dir(&at_gt, 0, cal(1, 0), &args, &skip, &above),
                engine::Decision::Skip
            ));
        }

        /// A listed skip path is skipped and not counted.
        #[test]
        fn skip_path_returns_skip_uncounted() {
            let args = make_args(u64::MAX, u64::MAX, false);
            let p = std::path::PathBuf::from("/skip/me");
            let mut skip = AHashSet::new();
            skip.insert(p.clone());
            let count = AtomicU64::new(0);
            let info = engine::DirInfo { path: &p, dev: 0, size: 9999 };
            let d = classify_dir(&info, 0, cal(1, 0), &args, &skip, &count);
            assert!(matches!(d, engine::Decision::Skip));
            assert_eq!(count.load(Ordering::Relaxed), 0);
        }

        /// Under one-filesystem, a directory on a different device than the
        /// root is skipped and not counted.
        #[test]
        fn foreign_device_skipped_uncounted_under_one_fs() {
            let args = make_args(u64::MAX, u64::MAX, true);
            let count = AtomicU64::new(0);
            let info = engine::DirInfo {
                path: Path::new("/mnt"),
                dev: 99,
                size: 9999,
            };
            let d = classify_dir(
                &info,
                1,
                cal(1, 0),
                &args,
                &AHashSet::new(),
                &count,
            );
            assert!(matches!(d, engine::Decision::Skip));
            assert_eq!(count.load(Ordering::Relaxed), 0);
        }

        /// A zero `per_entry` (interrupted/degenerate calibration) disables
        /// flagging and never divides, even with zero thresholds.
        #[test]
        fn zero_per_entry_descends_without_flagging() {
            let args = make_args(0, 0, false);
            let count = AtomicU64::new(0);
            let info = engine::DirInfo {
                path: Path::new("/d"),
                dev: 0,
                size: u64::MAX,
            };
            let d = classify_dir(
                &info,
                0,
                cal(0, 0),
                &args,
                &AHashSet::new(),
                &count,
            );
            assert!(matches!(d, engine::Decision::Descend));
            assert_eq!(count.load(Ordering::Relaxed), 1);
        }

        /// Accurate mode counts entries via `read_dir`; exercise that path on a
        /// real flagged directory without panicking.
        #[test]
        fn accurate_mode_reads_real_dir() {
            let tmp = TempDir::new().unwrap();
            std::fs::create_dir(tmp.path().join("child")).unwrap();
            let mut args = (*make_args(0, u64::MAX, false)).clone();
            args.accurate = true;
            let count = AtomicU64::new(0);
            let info =
                engine::DirInfo { path: tmp.path(), dev: 0, size: 4096 };
            let d = classify_dir(
                &info,
                0,
                cal(1, 0),
                &args,
                &AHashSet::new(),
                &count,
            );
            assert!(matches!(d, engine::Decision::Descend));
            assert_eq!(count.load(Ordering::Relaxed), 1);
        }
    }

    mod parallel_search {
        use super::*;

        /// A `size_inode_ratio` of 0 must not cause a divide-by-zero panic; the
        /// zero-ratio guard returns `Decision::Descend` for every directory.
        #[test]
        fn zero_ratio_does_not_panic() {
            let tmp = TempDir::new().unwrap();
            std::fs::create_dir(tmp.path().join("sub")).unwrap();
            let meta = std::fs::metadata(tmp.path()).unwrap();
            let args = make_args(10_000, 100_000, false);

            // Must not panic; root should at minimum be counted.
            let count = parallel_search(
                tmp.path(),
                &meta,
                Calibration { per_entry: 0, overhead: 0 },
                &no_shutdown(),
                &args,
                &AHashSet::new(),
            );
            assert!(count >= 1);
        }

        /// Only directories explicitly listed in `skip_path` are excluded;
        /// every other directory, including the root, is still counted.
        #[test]
        fn skip_path_skips_only_listed_dirs() {
            let tmp = TempDir::new().unwrap();
            let keep = tmp.path().join("keep");
            let skip_me = tmp.path().join("skip_me");
            std::fs::create_dir(&keep).unwrap();
            std::fs::create_dir(&skip_me).unwrap();

            let meta = std::fs::metadata(tmp.path()).unwrap();
            // Thresholds set high enough that no directory is ever flagged.
            let args = make_args(u64::MAX, u64::MAX, false);

            let mut skip_set = AHashSet::new();
            skip_set.insert(skip_me);

            let count = parallel_search(
                tmp.path(),
                &meta,
                Calibration { per_entry: 1, overhead: 0 },
                &no_shutdown(),
                &args,
                &skip_set,
            );
            // root + keep = 2; skip_me is excluded and not counted.
            assert_eq!(count, 2);
        }

        /// With `one_filesystem = true`, directories on the same device as the
        /// scan root must not be skipped.
        #[test]
        fn one_filesystem_allows_same_device_dirs() {
            let tmp = TempDir::new().unwrap();
            std::fs::create_dir(tmp.path().join("sub")).unwrap();
            let meta = std::fs::metadata(tmp.path()).unwrap();
            let args = make_args(u64::MAX, u64::MAX, true);

            let count = parallel_search(
                tmp.path(),
                &meta,
                Calibration { per_entry: 1, overhead: 0 },
                &no_shutdown(),
                &args,
                &AHashSet::new(),
            );
            // Both root and sub share the same device and must be counted.
            assert!(count >= 2);
        }

        /// An alert-level directory (above `alert_threshold` but below
        /// `blacklist_threshold`) returns `Decision::Descend` so its children
        /// are still scanned.
        ///
        /// Relies on directories having a non-zero inode size, which is
        /// guaranteed on Linux but not on macOS/APFS.
        #[test]
        #[cfg(target_os = "linux")]
        fn alert_dir_subtree_is_still_scanned() {
            let tmp = TempDir::new().unwrap();
            std::fs::create_dir(tmp.path().join("child")).unwrap();
            let meta = std::fs::metadata(tmp.path()).unwrap();
            // alert=0: every directory with inode size > 0 triggers the
            // alert branch. blacklist=MAX: nothing is ever blacklisted.
            // ratio=1: approx_files = raw inode size in bytes.
            let args = make_args(0, u64::MAX, false);

            let count = parallel_search(
                tmp.path(),
                &meta,
                Calibration { per_entry: 1, overhead: 0 },
                &no_shutdown(),
                &args,
                &AHashSet::new(),
            );
            // root triggers alert but must Continue; child is then visited.
            assert_eq!(count, 2);
        }

        /// A blacklist-level directory returns `Decision::Skip` so its subtree
        /// is not scanned.
        ///
        /// Relies on directories having a non-zero inode size, which is
        /// guaranteed on Linux but not on macOS/APFS.
        #[test]
        #[cfg(target_os = "linux")]
        fn blacklist_dir_subtree_is_not_scanned() {
            let tmp = TempDir::new().unwrap();
            std::fs::create_dir(tmp.path().join("child")).unwrap();
            let meta = std::fs::metadata(tmp.path()).unwrap();
            // blacklist=1: any directory whose inode size > 1 byte is
            // blacklisted (Linux directories are typically 4096 bytes).
            // ratio=1: approx_files = raw inode size in bytes.
            let args = make_args(0, 1, false);

            let count = parallel_search(
                tmp.path(),
                &meta,
                Calibration { per_entry: 1, overhead: 0 },
                &no_shutdown(),
                &args,
                &AHashSet::new(),
            );
            // root is blacklisted → Skip → child is never visited;
            // root itself is counted before Skip is returned.
            assert_eq!(count, 1);
        }

        /// `approx_files` must be computed as `size / size_inode_ratio`. With a
        /// large ratio, correct division yields 0 (no threshold exceeded), so
        /// the child directory is still visited.
        ///
        /// Relies on directories having a non-zero inode size, which is
        /// guaranteed on Linux but not on macOS/APFS.
        #[test]
        #[cfg(target_os = "linux")]
        fn approx_files_uses_division_not_multiplication() {
            let tmp = TempDir::new().unwrap();
            std::fs::create_dir(tmp.path().join("child")).unwrap();
            let meta = std::fs::metadata(tmp.path()).unwrap();
            // ratio = 1_000_000: correct → 4096/1_000_000 = 0 (no flag);
            // bug    → 4096*1_000_000 = 4_096_000_000 >> blacklist (100_000)
            let args = make_args(0, 100_000, false);

            let count = parallel_search(
                tmp.path(),
                &meta,
                Calibration { per_entry: 1_000_000, overhead: 0 },
                &no_shutdown(),
                &args,
                &AHashSet::new(),
            );
            // Correct division: approx_files = 0 → no threshold fires →
            // child is visited → count = 2.
            assert_eq!(count, 2);
        }
    }
}
