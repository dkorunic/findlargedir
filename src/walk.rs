use std::fs::Metadata;
use std::fs::read_dir;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::sleep;
use std::time::Duration;

use crate::args::Args;
use ahash::AHashSet;
use ansi_term::Colour::{Green, Red, Yellow};
use human_format::Formatter;
use ignore::{DirEntry, Error, WalkBuilder, WalkState};
use indicatif::HumanBytes;

thread_local! {
    static FORMATTER: Formatter = Formatter::new();
}

/// Default number of files in a folder to cause alert
pub const ALERT_COUNT: u64 = 10_000;

/// Default number of files in a folder to cause red alert and further blacklist from the deeper
/// scan
pub const BLACKLIST_COUNT: u64 = 100_000;

/// Default status update period in seconds
pub const STATUS_SECONDS: u64 = 20;

/// Perform a parallel filesystem search based on specified criteria and arguments.
///
/// # Arguments
/// * `path` - A reference to the starting path for the filesystem search.
/// * `path_metadata` - A reference to the metadata of the starting path.
/// * `size_inode_ratio` - The ratio used to calculate the approximate number of files in a directory.
/// * `shutdown_walk` - A shared reference to a boolean flag indicating if the search should be terminated.
/// * `args` - A shared reference to the command-line arguments provided.
/// * `skip_path` - A reference to the set of paths to be excluded from scanning.
///
/// # Returns
/// The total count of analyzed directories during the filesystem search.
///
/// # Behaviors
/// - Uses the provided hash set of paths to exclude from scanning.
/// - Initializes a thread pool for status reporting and filesystem traversal.
/// - Updates the processed directory count based on the status update interval.
/// - Initiates the parallel filesystem walk using specified parameters.
/// - Terminates the search if a shutdown signal is received.
/// - Processes each directory entry encountered during the search.
///
/// # Types
/// * `path` - `&Path`
/// * `path_metadata` - `&Metadata`
/// * `size_inode_ratio` - `u64`
/// * `shutdown_walk` - `&Arc<AtomicBool>`
/// * `args` - `&Arc<Args>`
/// * `skip_path` - `&AHashSet<PathBuf>`
/// * Return Type - `u64`
pub fn parallel_search(
    path: &Path,
    path_metadata: &Metadata,
    size_inode_ratio: u64,
    shutdown_walk: &Arc<AtomicBool>,
    args: &Arc<Args>,
    skip_path: &AHashSet<PathBuf>,
) -> u64 {
    // Thread pool for status reporting
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .expect("Unable to spawn reporting thread pool");

    // Processed directory count
    let dir_count = Arc::new(AtomicU64::new(0));

    // Status update thread
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

    // Perform target filesystem walking
    WalkBuilder::new(path)
        .hidden(false)
        .standard_filters(false)
        .follow_links(args.follow_symlinks)
        .threads(args.threads)
        .build_parallel()
        .run(|| {
            let dir_count = dir_count.clone();
            Box::new(move |dir_entry_result| {
                // Terminate on received interrupt signal
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

    dir_count.load(Ordering::Relaxed)
}

/// Processes a directory entry based on specified criteria and arguments.
///
/// # Arguments
/// * `path_metadata` - A reference to the metadata of the current directory.
/// * `size_inode_ratio` - The ratio used to calculate the approximate number of files in the directory.
/// * `dir_entry_result` - The result of attempting to read a directory entry.
/// * `skip_path` - A set of paths to be excluded from scanning.
/// * `args` - A shared reference to the command-line arguments provided.
/// * `dir_count` - A reference to the atomic counter for analyzed directories.
///
/// # Returns
/// The state of the directory processing, indicating whether to continue, skip, or stop scanning.
///
/// # Behaviors
/// - Checks if the directory entry is a directory; if not, continues to the next entry.
/// - Skips scanning if the directory is in the skip path list.
/// - Skips scanning if the directory is on a different filesystem and the `one_filesystem` flag is set.
/// - Increments the scanned directory count.
/// - Calculates the size and approximate file count of the directory entry.
/// - Prints warnings and potentially marks the directory as an offender based on file count thresholds.
/// - Returns the appropriate state for further scanning based on the calculated conditions.
///
/// # Types
/// * `path_metadata` - `&Metadata`
/// * `size_inode_ratio` - `u64`
/// * `dir_entry_result` - `&Result<DirEntry, ignore::Error>`
/// * `skip_path` - `&AHashSet<PathBuf>`
/// * `args` - `&Arc<Args>`
/// * `dir_count` - `&AtomicU64`
/// * Return Type - `WalkState`
fn process_dir_entry(
    path_metadata: &Metadata,
    size_inode_ratio: u64,
    dir_entry_result: &Result<DirEntry, Error>,
    skip_path: &AHashSet<PathBuf>,
    args: &Arc<Args>,
    dir_count: &AtomicU64,
) -> WalkState {
    if let Ok(dir_entry) = dir_entry_result
        && let Some(dir_entry_type) = dir_entry.file_type()
    {
        if !dir_entry_type.is_dir() {
            return WalkState::Continue;
        }

        let full_path = dir_entry.path();

        // Ignore skip paths, typically being virtual filesystems (/proc, /dev, /sys, /run)
        if !skip_path.is_empty() && skip_path.contains(full_path) {
            println!(
                "Skipping further scan at {} as requested",
                full_path.display()
            );

            return WalkState::Skip;
        }

        // Retrieve Unix metadata for a given directory
        if let Ok(dir_entry_metadata) = dir_entry.metadata() {
            // If `one_filesystem` flag has been set and if directory is not residing
            // on the same device as top search path, print warning and abort deeper
            // scanning
            if args.one_filesystem
                && (dir_entry_metadata.dev() != path_metadata.dev())
            {
                println!(
                    "Identified filesystem boundary at {}, skipping...",
                    full_path.display()
                );

                return WalkState::Skip;
            }

            // Count only directories that pass all filters and are actually analyzed
            dir_count.fetch_add(1, Ordering::Relaxed);

            // Identify size and calculate approximate directory entry count
            let size = dir_entry_metadata.size();
            if size_inode_ratio == 0 {
                return WalkState::Continue;
            }
            let approx_files = size / size_inode_ratio;

            // Print count warnings if necessary
            if approx_files > args.blacklist_threshold {
                print_offender(
                    full_path,
                    size,
                    approx_files,
                    args.accurate,
                    true,
                );

                return WalkState::Skip;
            } else if approx_files > args.alert_threshold {
                print_offender(
                    full_path,
                    size,
                    approx_files,
                    args.accurate,
                    false,
                );

                return WalkState::Continue;
            }
        }
    }

    WalkState::Continue
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

    /// Bug #7: a `size_inode_ratio` of 0 must not cause a
    /// divide-by-zero panic; the zero-ratio guard should return
    /// `WalkState::Continue` for every directory.
    #[test]
    fn test_zero_ratio_does_not_panic() {
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
        );
        assert!(count >= 1);
    }

    /// Bug #9: only directories explicitly listed in `skip_path` must
    /// be excluded.  With the inverted bug every directory that is
    /// *not* in the set is skipped instead, so the root itself is
    /// skipped and the count drops to 0.
    #[test]
    fn test_skip_path_skips_only_listed_dirs() {
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
        );
        // root + keep = 2; skip_me is excluded and not counted.
        // Bug #9 makes root (not in skip_set) also skip → count = 0.
        assert_eq!(count, 2);
    }

    /// Bug #8: with `one_filesystem = true`, directories that reside
    /// on the *same* device as the scan root must not be skipped.
    /// The inverted bug skips them (dev() == dev()), pruning the
    /// entire tree.
    #[test]
    fn test_one_filesystem_allows_same_device_dirs() {
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
        );
        // Both root and sub share the same device and must be counted.
        // Bug #8 skips same-device dirs → count falls to 0.
        assert!(count >= 2);
    }

    /// Bug #3: an alert-level directory (above `alert_threshold` but
    /// below `blacklist_threshold`) must return `WalkState::Continue`
    /// so that its children are still scanned.
    ///
    /// This test relies on directories having a non-zero inode size,
    /// which is guaranteed on Linux but not on macOS/APFS.
    #[test]
    #[cfg(target_os = "linux")]
    fn test_alert_dir_subtree_is_still_scanned() {
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
        );
        // root triggers alert but must Continue; child is then visited.
        // Bug #3 returns Skip for alert-level dirs → child never seen.
        assert_eq!(count, 2);
    }

    /// Bugs #4 and #10: a blacklist-level directory must return
    /// `WalkState::Skip` so that its subtree is not scanned.
    ///
    /// This test relies on directories having a non-zero inode size,
    /// which is guaranteed on Linux but not on macOS/APFS.
    #[test]
    #[cfg(target_os = "linux")]
    fn test_blacklist_dir_subtree_is_not_scanned() {
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
        );
        // root is blacklisted → Skip → child is never visited.
        // root itself is counted before Skip is returned.
        // Bug #4 (Continue instead of Skip) or Bug #10 (wrong branch
        // order) would both let child be visited → count = 2.
        assert_eq!(count, 1);
    }

    /// Bug #15: `approx_files` must be computed as
    /// `size / size_inode_ratio`, not `size * size_inode_ratio`.
    ///
    /// With a large ratio, correct division yields 0 (no threshold
    /// exceeded); wrong multiplication yields an astronomically large
    /// value that triggers the blacklist and skips every subtree.
    ///
    /// This test relies on directories having a non-zero inode size,
    /// which is guaranteed on Linux but not on macOS/APFS.
    #[test]
    #[cfg(target_os = "linux")]
    fn test_approx_files_uses_division_not_multiplication() {
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
        );
        // Correct division: approx_files = 0 → no threshold fires →
        // child is visited → count = 2.
        // Wrong multiplication: every dir blacklisted → child skipped.
        assert_eq!(count, 2);
    }
}

/// Prints information about directories that exceed specified thresholds.
///
/// This function is called when the estimated number of files in a directory exceeds either the alert or blacklist thresholds.
/// It outputs details about the directory and its file count, and can optionally mark the directory as an offender based on its size.
///
/// # Arguments
/// * `path` - The path of the directory being evaluated.
/// * `size` - The size of the directory in bytes.
/// * `file_count` - The estimated number of files in the directory.
/// * `accurate` - A boolean flag indicating whether the size estimation is considered accurate.
/// * `red_alert` - A boolean flag indicating whether the directory exceeds the blacklist threshold.
#[allow(clippy::cast_precision_loss)]
fn print_offender(
    full_path: &Path,
    size: u64,
    approx_files: u64,
    accurate: bool,
    red_alert: bool,
) {
    // Pretty print either the accurate directory count or the approximation
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
