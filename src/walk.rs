// SPDX-FileCopyrightText: 2022 Dinko Korunic <dinko.korunic@gmail.com>
//
// SPDX-License-Identifier: MIT

use std::fs::Metadata;
use std::fs::read_dir;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use ahash::{AHashMap, AHashSet};
use human_format::Formatter;
use indicatif::{HumanBytes, ProgressBar};
use owo_colors::OwoColorize;
use tempfile::TempDir;

use crate::args::Args;
use crate::calibrate::{self, Calibration};
use crate::progress;

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

/// Resolves the right `Calibration` per directory's filesystem, since per-entry
/// geometry differs per fs. The scan-root fs uses the up-front `root` with no
/// locking (the common path); other filesystems — crossed only when
/// `--one-filesystem` is off — are calibrated on first encounter and cached.
struct CalContext<'a> {
    root_dev: u64,
    root: Calibration,
    foreign: Mutex<AHashMap<u64, Calibration>>,
    shutdown: &'a Arc<AtomicBool>,
    args: &'a Args,
    // Clone of the scan spinner; suspended while a foreign filesystem is
    // calibrated so its messages and own spinner aren't clobbered.
    scan_pb: ProgressBar,
}

impl CalContext<'_> {
    /// Calibration for `dev`, calibrating `sample_path`'s filesystem in place on
    /// first encounter. Precondition: gate behind the filesystem-boundary check
    /// so a foreign filesystem that would be skipped is never written to.
    fn resolve(&self, dev: u64, sample_path: &Path) -> Calibration {
        if dev == self.root_dev {
            return self.root;
        }
        let mut foreign = self.foreign.lock().unwrap();
        if let Some(&cal) = foreign.get(&dev) {
            return cal;
        }
        let cal = self.calibrate(sample_path);
        foreign.insert(dev, cal);
        cal
    }

    /// Calibrates the filesystem holding `sample_path` in a temp dir there. A
    /// fixed `-i` ratio short-circuits without touching the filesystem; an
    /// unwritable filesystem or a failed calibration disables flagging for it.
    /// The scan spinner is paused for the duration so calibration's own output
    /// (start/done lines and its spinner) renders cleanly.
    fn calibrate(&self, sample_path: &Path) -> Calibration {
        const DISABLED: Calibration =
            Calibration { per_entry: 0, overhead: 0 };

        if self.args.size_inode_ratio > 0 {
            return Calibration {
                per_entry: self.args.size_inode_ratio,
                overhead: 0,
            };
        }

        self.scan_pb.suspend(|| {
            if calibrate::is_read_only(sample_path) {
                println!(
                    "Skipping calibration on read-only filesystem at {}; \
                     size-based flagging disabled there",
                    sample_path.display()
                );
                return DISABLED;
            }

            let tmp = match TempDir::new_in(sample_path) {
                Ok(t) => t,
                Err(e) => {
                    println!(
                        "Warning: cannot calibrate filesystem at {} ({e}); \
                         size-based flagging disabled there",
                        sample_path.display()
                    );
                    return DISABLED;
                }
            };
            calibrate::get_inode_ratio(
                tmp.path(),
                sample_path,
                self.shutdown,
                self.args,
            )
            .unwrap_or(DISABLED)
        })
    }
}

/// Walks `path` in parallel and returns the number of directories analyzed.
/// Each descended directory is flagged on its exact entry count; a subtree whose
/// *estimated* count exceeds the blacklist threshold is skipped unread (and
/// reported from the estimate). The walk stops early once `shutdown_walk` is set.
pub fn parallel_search(
    path: &Path,
    path_metadata: &Metadata,
    calibration: Calibration,
    shutdown_walk: &Arc<AtomicBool>,
    args: &Args,
    skip_path: &AHashSet<PathBuf>,
) -> u64 {
    let dir_count = Arc::new(AtomicU64::new(0));

    let scan_pb = progress::new_spinner(format!(
        "Scanning path {} in progress...",
        path.display()
    ));

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
                    count.to_string().green(),
                    sleep_delay
                );
            }
        });
        (tx, handle)
    });

    let cal = CalContext {
        root_dev: path_metadata.dev(),
        root: calibration,
        foreign: Mutex::new(AHashMap::new()),
        shutdown: shutdown_walk,
        args,
        scan_pb: scan_pb.clone(),
    };
    let classify = |info: engine::DirInfo| {
        classify_dir(&info, &cal, args, skip_path, &dir_count)
    };
    let report = |path: &Path, ino: u64, size: u64, entries: u64| {
        report_dir(path, ino, size, entries, args);
    };

    engine::walk_dirs(
        path,
        args.threads,
        args.follow_symlinks,
        shutdown_walk,
        classify,
        report,
    );

    // Stop and join the progress thread before returning.
    if let Some((tx, handle)) = progress {
        drop(tx);
        let _ = handle.join();
    }

    scan_pb.finish_with_message("Done.");

    dir_count.load(Ordering::Relaxed)
}

/// Decides, from a single `stat` and before reading, whether to descend a
/// directory. Skips a subtree whose *estimated* entry count exceeds the
/// blacklist threshold — the one case the size heuristic must handle alone,
/// since reading a true black hole is the cost we refuse to pay. Everything else
/// descends and is judged on its exact count by [`report_dir`]. A blacklisted
/// (skipped) subtree is reported here from the estimate, or, with `-a`, an
/// explicit `read_dir` the caller opts into despite the cost.
fn classify_dir(
    info: &engine::DirInfo,
    cal: &CalContext,
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
    if args.one_filesystem && info.dev != cal.root_dev {
        println!(
            "Identified filesystem boundary at {}, filesystem {}, skipping (use -m/--cross-filesystem to cross)",
            full_path.display(),
            calibrate::fs_type_name(full_path)
        );

        return engine::Decision::Skip;
    }

    // Counts only directories that survive every filter above.
    dir_count.fetch_add(1, Ordering::Relaxed);

    // After the boundary check, so we never calibrate a fs we'd skip.
    let calibration = cal.resolve(info.dev, full_path);

    // Zero per-entry (interrupted/degenerate calibration) disables skipping;
    // descend and let the exact count speak.
    if calibration.per_entry == 0 {
        return engine::Decision::Descend;
    }
    let approx_files =
        info.size.saturating_sub(calibration.overhead) / calibration.per_entry;

    if approx_files > args.blacklist_threshold {
        let (count, exact) = if args.accurate {
            match read_dir(full_path) {
                Ok(r) => (r.count() as u64, true),
                Err(e) => {
                    println!(
                        "Warning: unable to get exact count for {}, using estimate: {e}",
                        full_path.display()
                    );
                    (approx_files, false)
                }
            }
        } else {
            (approx_files, false)
        };
        print_offender(full_path, info.ino, info.size, count, exact, true);

        return engine::Decision::Skip;
    }

    engine::Decision::Descend
}

/// Severity of a directory's entry count against the thresholds, or `None` when
/// it clears both. Both comparisons are strictly `>`, so a count equal to a
/// threshold does not trip it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tier {
    Alert,
    Blacklist,
}

fn offender_tier(count: u64, alert: u64, blacklist: u64) -> Option<Tier> {
    if count > blacklist {
        Some(Tier::Blacklist)
    } else if count > alert {
        Some(Tier::Alert)
    } else {
        None
    }
}

/// Reports a descended directory on its **exact** live entry count, harvested
/// for free from the enumeration the walk already performs. Red above the
/// blacklist threshold, yellow above the alert threshold, silent below. Judging
/// the alert tier on truth rather than the estimate is what stops size-inflated
/// directories — e.g. ones whose blocks never shrank after deletions — from
/// raising false alarms.
fn report_dir(path: &Path, ino: u64, size: u64, entries: u64, args: &Args) {
    let Some(tier) =
        offender_tier(entries, args.alert_threshold, args.blacklist_threshold)
    else {
        return;
    };
    print_offender(path, ino, size, entries, true, tier == Tier::Blacklist);
}

/// Prints a flagged directory, coloured red for a blacklist hit or yellow for
/// an alert. An `exact` count prints as-is; an estimate is labelled an upper
/// bound, since a directory's size is its high-water mark and can exceed the
/// live count.
#[allow(clippy::cast_precision_loss)]
fn print_offender(
    full_path: &Path,
    ino: u64,
    size: u64,
    count: u64,
    exact: bool,
    red: bool,
) {
    let human = FORMATTER.with(|f| f.format(count as f64));
    let human =
        if red { human.red().to_string() } else { human.yellow().to_string() };

    println!(
        "Found directory {} with inode number {}, inode size {} and {} files{}",
        full_path.display(),
        ino,
        HumanBytes(size),
        human,
        if exact { "" } else { " (size-based upper bound)" }
    );
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use ahash::AHashSet;
    use tempfile::TempDir;

    use super::{CalContext, classify_dir, engine, parallel_search};
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
            calibration_name_length: crate::calibrate::DEFAULT_NAME_LEN,
            size_inode_ratio: 0,
            calibration_path: None,
            skip_path: vec![],
            path: vec![],
        })
    }

    fn no_shutdown() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    mod cal_context {
        use std::sync::Mutex;

        use ahash::AHashMap;
        use indicatif::ProgressBar;
        use tempfile::TempDir;

        use super::super::CalContext;
        use super::{make_args, no_shutdown};
        use crate::calibrate::Calibration;

        /// The scan-root filesystem uses the up-front seed and is never
        /// recalibrated: `resolve` for the root dev returns the seed and leaves
        /// the foreign cache empty.
        #[test]
        fn root_device_uses_seed_without_calibrating() {
            let args = make_args(10_000, 100_000, false);
            let sd = no_shutdown();
            let cx = CalContext {
                root_dev: 7,
                root: Calibration { per_entry: 42, overhead: 9 },
                foreign: Mutex::new(AHashMap::new()),
                shutdown: &sd,
                args: &args,
                scan_pb: ProgressBar::hidden(),
            };
            assert_eq!(
                cx.resolve(7, std::path::Path::new("/nonexistent")),
                Calibration { per_entry: 42, overhead: 9 }
            );
            assert!(cx.foreign.lock().unwrap().is_empty());
        }

        /// A foreign filesystem is calibrated on first encounter and cached, so
        /// a second lookup returns the same value without recalibrating.
        #[test]
        fn foreign_device_is_calibrated_and_cached() {
            let args = make_args(10_000, 100_000, false);
            let sd = no_shutdown();
            let cx = CalContext {
                root_dev: 0,
                root: Calibration { per_entry: 0, overhead: 0 },
                foreign: Mutex::new(AHashMap::new()),
                shutdown: &sd,
                args: &args,
                scan_pb: ProgressBar::hidden(),
            };
            let tmp = TempDir::new().unwrap();
            let first = cx.resolve(99, tmp.path());
            assert!(cx.foreign.lock().unwrap().contains_key(&99));
            assert_eq!(first, cx.resolve(99, tmp.path()));
        }

        /// With a fixed `-i` ratio, a foreign filesystem uses that ratio directly
        /// and no calibration files are created on it.
        #[test]
        fn fixed_ratio_skips_calibration_files() {
            let mut a = (*make_args(10_000, 100_000, false)).clone();
            a.size_inode_ratio = 21;
            let sd = no_shutdown();
            let cx = CalContext {
                root_dev: 0,
                root: Calibration { per_entry: 0, overhead: 0 },
                foreign: Mutex::new(AHashMap::new()),
                shutdown: &sd,
                args: &a,
                scan_pb: ProgressBar::hidden(),
            };
            let tmp = TempDir::new().unwrap();
            assert_eq!(
                cx.resolve(99, tmp.path()),
                Calibration { per_entry: 21, overhead: 0 }
            );
            assert_eq!(
                std::fs::read_dir(tmp.path()).unwrap().count(),
                0,
                "fixed ratio must not create calibration files"
            );
        }
    }

    mod offender_tier {
        use super::super::{Tier, offender_tier};

        /// A count clearing the alert threshold is silent — this is what
        /// suppresses false alarms once the exact count replaces the estimate.
        #[test]
        fn below_alert_is_silent() {
            assert_eq!(offender_tier(7_722, 10_000, 100_000), None);
        }

        /// Both thresholds are exclusive: a count equal to either does not trip.
        #[test]
        fn thresholds_are_exclusive() {
            assert_eq!(offender_tier(10_000, 10_000, 100_000), None);
            assert_eq!(
                offender_tier(100_000, 10_000, 100_000),
                Some(Tier::Alert)
            );
        }

        /// Between the thresholds → alert; strictly above blacklist → blacklist.
        #[test]
        fn alert_and_blacklist_bands() {
            assert_eq!(
                offender_tier(10_001, 10_000, 100_000),
                Some(Tier::Alert)
            );
            assert_eq!(
                offender_tier(100_001, 10_000, 100_000),
                Some(Tier::Blacklist)
            );
        }
    }

    mod classify_dir {
        use super::*;

        fn cal(per_entry: u64, overhead: u64) -> Calibration {
            Calibration { per_entry, overhead }
        }

        /// A single-filesystem `CalContext`: `resolve` returns `root` for the
        /// scan-root device and never calibrates, which is all these tests need
        /// (per-filesystem calibration is covered in `mod cal_context`).
        fn ctx<'a>(
            root_dev: u64,
            root: Calibration,
            args: &'a Args,
            shutdown: &'a Arc<AtomicBool>,
        ) -> CalContext<'a> {
            CalContext {
                root_dev,
                root,
                foreign: std::sync::Mutex::new(ahash::AHashMap::new()),
                shutdown,
                args,
                scan_pb: indicatif::ProgressBar::hidden(),
            }
        }

        /// Overhead is subtracted before dividing: a directory whose *raw* size
        /// would blacklist is not skipped once the fixed overhead is removed.
        /// Guards against dropping the subtraction or using the wrong field.
        #[test]
        fn overhead_is_subtracted_before_dividing() {
            // per_entry=1, blacklist=100, alert never fires.
            // (1050-1000)/1 = 50 (≤100) → Descend; raw 1050 would Skip.
            let args = make_args(u64::MAX, 100, false);
            let sd = no_shutdown();
            let count = AtomicU64::new(0);
            let info = engine::DirInfo {
                path: Path::new("/d"),
                dev: 0,
                ino: 0,
                size: 1050,
            };
            let d = classify_dir(
                &info,
                &ctx(0, cal(1, 1000), &args, &sd),
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
            let sd = no_shutdown();
            let count = AtomicU64::new(0);
            let info = engine::DirInfo {
                path: Path::new("/d"),
                dev: 0,
                ino: 0,
                size: 5000,
            };
            let d = classify_dir(
                &info,
                &ctx(0, cal(1, u64::MAX), &args, &sd),
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
            let sd = no_shutdown();
            let skip = AHashSet::new();
            let eq = AtomicU64::new(0);
            let at_eq = engine::DirInfo {
                path: Path::new("/d"),
                dev: 0,
                ino: 0,
                size: 100,
            };
            assert!(matches!(
                classify_dir(
                    &at_eq,
                    &ctx(0, cal(1, 0), &args, &sd),
                    &args,
                    &skip,
                    &eq
                ),
                engine::Decision::Descend
            ));
            let above = AtomicU64::new(0);
            let at_gt = engine::DirInfo {
                path: Path::new("/d"),
                dev: 0,
                ino: 0,
                size: 101,
            };
            assert!(matches!(
                classify_dir(
                    &at_gt,
                    &ctx(0, cal(1, 0), &args, &sd),
                    &args,
                    &skip,
                    &above
                ),
                engine::Decision::Skip
            ));
        }

        /// A listed skip path is skipped and not counted.
        #[test]
        fn skip_path_returns_skip_uncounted() {
            let args = make_args(u64::MAX, u64::MAX, false);
            let sd = no_shutdown();
            let p = std::path::PathBuf::from("/skip/me");
            let mut skip = AHashSet::new();
            skip.insert(p.clone());
            let count = AtomicU64::new(0);
            let info =
                engine::DirInfo { path: &p, dev: 0, ino: 0, size: 9999 };
            let d = classify_dir(
                &info,
                &ctx(0, cal(1, 0), &args, &sd),
                &args,
                &skip,
                &count,
            );
            assert!(matches!(d, engine::Decision::Skip));
            assert_eq!(count.load(Ordering::Relaxed), 0);
        }

        /// Under one-filesystem, a directory on a different device than the
        /// root is skipped, not counted, and — crucially — not calibrated (the
        /// foreign cache stays empty, so no files are written to it).
        #[test]
        fn foreign_device_skipped_uncounted_under_one_fs() {
            let args = make_args(u64::MAX, u64::MAX, true);
            let sd = no_shutdown();
            let cx = ctx(1, cal(1, 0), &args, &sd);
            let count = AtomicU64::new(0);
            let info = engine::DirInfo {
                path: Path::new("/mnt"),
                dev: 99,
                ino: 0,
                size: 9999,
            };
            let d = classify_dir(&info, &cx, &args, &AHashSet::new(), &count);
            assert!(matches!(d, engine::Decision::Skip));
            assert_eq!(count.load(Ordering::Relaxed), 0);
            assert!(
                cx.foreign.lock().unwrap().is_empty(),
                "a skipped foreign filesystem must not be calibrated"
            );
        }

        /// A zero `per_entry` (interrupted/degenerate calibration) disables
        /// flagging and never divides, even with zero thresholds.
        #[test]
        fn zero_per_entry_descends_without_flagging() {
            let args = make_args(0, 0, false);
            let sd = no_shutdown();
            let count = AtomicU64::new(0);
            let info = engine::DirInfo {
                path: Path::new("/d"),
                dev: 0,
                ino: 0,
                size: u64::MAX,
            };
            let d = classify_dir(
                &info,
                &ctx(0, cal(0, 0), &args, &sd),
                &args,
                &AHashSet::new(),
                &count,
            );
            assert!(matches!(d, engine::Decision::Descend));
            assert_eq!(count.load(Ordering::Relaxed), 1);
        }

        /// Accurate mode reads a *blacklisted* (skipped) directory via
        /// `read_dir` — the one place it still counts exactly, since the walk
        /// never enumerates a skipped subtree. Exercises that path on a real dir
        /// without panicking; descended dirs get exact counts for free instead.
        #[test]
        fn accurate_mode_reads_blacklisted_dir() {
            let tmp = TempDir::new().unwrap();
            std::fs::create_dir(tmp.path().join("child")).unwrap();
            // blacklist=1, ratio=1: estimate 4096 > 1 → skip + accurate read.
            let mut args = (*make_args(0, 1, false)).clone();
            args.accurate = true;
            let sd = no_shutdown();
            let count = AtomicU64::new(0);
            let info = engine::DirInfo {
                path: tmp.path(),
                dev: 0,
                ino: 0,
                size: 4096,
            };
            let d = classify_dir(
                &info,
                &ctx(0, cal(1, 0), &args, &sd),
                &args,
                &AHashSet::new(),
                &count,
            );
            assert!(matches!(d, engine::Decision::Skip));
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
