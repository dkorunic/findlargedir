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
use std::thread::sleep;
use std::time::Duration;

use ahash::AHashSet;
use ansi_term::Colour::{Green, Red, Yellow};
use anyhow::Context;
use human_format::Formatter;
use ignore::{DirEntry, Error, WalkBuilder, WalkState};
use indicatif::HumanBytes;

use crate::args::Args;

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
///
/// # Errors
/// Returns an error if the status-reporting thread pool cannot be built.
pub fn parallel_search(
    path: &Path,
    path_metadata: &Metadata,
    size_inode_ratio: u64,
    shutdown_walk: &Arc<AtomicBool>,
    args: &Args,
    skip_path: &AHashSet<PathBuf>,
) -> anyhow::Result<u64> {
    // Single dedicated thread for periodic progress reporting.
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .context("Unable to spawn reporting thread pool")?;

    let dir_count = Arc::new(AtomicU64::new(0));

    if args.updates > 0 {
        let dir_count = dir_count.clone();
        let sleep_delay = args.updates;

        pool.spawn(move || loop {
            sleep(Duration::from_secs(sleep_delay));

            let count = dir_count.load(Ordering::Relaxed);
            println!(
                "Processed {} directories so far, next update in {} seconds",
                Green.paint(count.to_string()),
                sleep_delay
            );
        });
    }

    WalkBuilder::new(path)
        .hidden(false)
        .standard_filters(false)
        .follow_links(args.follow_symlinks)
        .threads(args.threads)
        .build_parallel()
        .run(|| {
            let dir_count = dir_count.clone();
            Box::new(move |dir_entry_result| {
                if shutdown_walk.load(Ordering::Relaxed) {
                    return WalkState::Quit;
                }

                process_dir_entry(
                    path_metadata,
                    size_inode_ratio,
                    &dir_entry_result,
                    skip_path,
                    args,
                    &dir_count,
                )
            })
        });

    Ok(dir_count.load(Ordering::Relaxed))
}

/// Classifies a single directory entry and returns the resulting
/// [`WalkState`]: skips entries listed in `skip_path`, skips filesystem
/// boundaries under `--one-filesystem`, and flags directories whose
/// estimated entry count (`size / size_inode_ratio`) crosses the alert or
/// blacklist thresholds. Non-directory entries and a zero ratio yield
/// `WalkState::Continue`.
fn process_dir_entry(
    path_metadata: &Metadata,
    size_inode_ratio: u64,
    dir_entry_result: &Result<DirEntry, Error>,
    skip_path: &AHashSet<PathBuf>,
    args: &Args,
    dir_count: &AtomicU64,
) -> WalkState {
    let Ok(dir_entry) = dir_entry_result else {
        return WalkState::Continue;
    };
    let Some(dir_entry_type) = dir_entry.file_type() else {
        return WalkState::Continue;
    };
    if !dir_entry_type.is_dir() {
        return WalkState::Continue;
    }

    let full_path = dir_entry.path();

    // User-excluded dirs, typically virtual filesystems (/proc, /sys, /dev).
    if !skip_path.is_empty() && skip_path.contains(full_path) {
        println!(
            "Skipping further scan at {} as requested",
            full_path.display()
        );

        return WalkState::Skip;
    }

    let Ok(dir_entry_metadata) = dir_entry.metadata() else {
        return WalkState::Continue;
    };

    // Don't cross mount points when confined to one filesystem.
    if args.one_filesystem && (dir_entry_metadata.dev() != path_metadata.dev())
    {
        println!(
            "Identified filesystem boundary at {}, skipping...",
            full_path.display()
        );

        return WalkState::Skip;
    }

    // Counts only directories that survive every filter above.
    dir_count.fetch_add(1, Ordering::Relaxed);

    let size = dir_entry_metadata.size();
    // A zero ratio (interrupted calibration) disables flagging entirely.
    if size_inode_ratio == 0 {
        return WalkState::Continue;
    }
    let approx_files = size / size_inode_ratio;

    if approx_files > args.blacklist_threshold {
        print_offender(full_path, size, approx_files, args.accurate, true);

        return WalkState::Skip;
    } else if approx_files > args.alert_threshold {
        print_offender(full_path, size, approx_files, args.accurate, false);

        return WalkState::Continue;
    }

    WalkState::Continue
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
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use ahash::AHashSet;
    use tempfile::TempDir;

    use super::parallel_search;
    use crate::args::Args;

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

    mod parallel_search {
        use super::*;

        /// A `size_inode_ratio` of 0 must not cause a divide-by-zero panic; the
        /// zero-ratio guard returns `WalkState::Continue` for every directory.
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
                0,
                &no_shutdown(),
                &args,
                &AHashSet::new(),
            )
            .unwrap();
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
                1,
                &no_shutdown(),
                &args,
                &skip_set,
            )
            .unwrap();
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
                1,
                &no_shutdown(),
                &args,
                &AHashSet::new(),
            )
            .unwrap();
            // Both root and sub share the same device and must be counted.
            assert!(count >= 2);
        }

        /// An alert-level directory (above `alert_threshold` but below
        /// `blacklist_threshold`) returns `WalkState::Continue` so its children
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
                1,
                &no_shutdown(),
                &args,
                &AHashSet::new(),
            )
            .unwrap();
            // root triggers alert but must Continue; child is then visited.
            assert_eq!(count, 2);
        }

        /// A blacklist-level directory returns `WalkState::Skip` so its subtree
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
                1,
                &no_shutdown(),
                &args,
                &AHashSet::new(),
            )
            .unwrap();
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
                1_000_000,
                &no_shutdown(),
                &args,
                &AHashSet::new(),
            )
            .unwrap();
            // Correct division: approx_files = 0 → no threshold fires →
            // child is visited → count = 2.
            assert_eq!(count, 2);
        }
    }
}
