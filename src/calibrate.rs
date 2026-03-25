use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Error};
use fs_err as fs;
use rayon::prelude::*;

use crate::{args, progress};

/// Default number of files to create in the calibration directory
pub const DEFAULT_TEST_COUNT: u64 = 100;

/// Calculates the size-to-inode ratio for a given directory.
///
/// This function initiates a calibration process by creating a specified number of files
/// within the `test_path` directory to determine the average file size to inode ratio.
/// It uses a multi-threaded approach to create files and monitors for a shutdown signal
/// to safely terminate and clean up if necessary.
///
/// # Arguments
/// * `test_path` - A reference to the path where test files will be created.
/// * `shutdown` - A shared atomic boolean to signal shutdown and cleanup.
/// * `args` - A shared structure containing runtime arguments such as the number of threads
///   and the number of files to create for calibration.
///
/// # Returns
/// Returns a `Result<u64, Error>` which is the calculated size-to-inode ratio if successful,
/// or an error if the operation fails at any step.
///
/// # Errors
/// This function can return an error if it fails to create the thread pool, create files,
/// or retrieve metadata from the test directory.
pub fn get_inode_ratio(
    test_path: &Path,
    shutdown: &Arc<AtomicBool>,
    args: &Arc<args::Args>,
) -> Result<u64, Error> {
    println!("Starting test directory calibration in {}", test_path.display());

    // Thread pool for mass file creation
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(args.threads)
        .build()
        .context("Unable to spawn calibration thread pool")?;

    let pb = progress::new_spinner("Creating test files in progress...");

    // Mass create files; filenames are short to get minimal size to inode ratio
    let res: Result<(), Error> = pool.install(|| {
        (0..args.calibration_count).into_par_iter().try_for_each(|i| {
            if shutdown.load(Ordering::Relaxed) {
                return Err(anyhow::anyhow!("shutdown requested"));
            }

            File::create(test_path.join(i.to_string()))
                .context("Unable to create test file")?;

            Ok(())
        })
    });

    pb.finish_with_message("Done.");

    // Propagate real errors; ignore the sentinel error emitted on shutdown
    if let Err(e) = res
        && !shutdown.load(Ordering::Relaxed)
    {
        return Err(e);
    }

    // Terminate on received interrupt signal; TempDir owned by the caller
    // is dropped automatically, so no explicit cleanup is needed here.
    if shutdown.load(Ordering::Relaxed) {
        return Ok(0);
    }

    let size_inode_ratio = fs::metadata(test_path)
        .context("Unable to retrieve calibration directory metadata")?
        .size()
        / args.calibration_count;
    println!(
        "Calibration done. Calculated size-to-inode ratio: {size_inode_ratio}"
    );

    Ok(size_inode_ratio)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use tempfile::TempDir;

    use super::get_inode_ratio;
    use crate::args::Args;

    fn make_args(calibration_count: u64) -> Arc<Args> {
        Arc::new(Args {
            calibration_count,
            threads: 2,
            updates: 0,
            alert_threshold: 10_000,
            blacklist_threshold: 100_000,
            one_filesystem: false,
            follow_symlinks: false,
            accurate: false,
            size_inode_ratio: 0,
            calibration_path: None,
            skip_path: vec![],
            path: vec![],
        })
    }

    /// Bugs #11 and #12: a calibration run that is cut short by a
    /// shutdown signal must return `Ok(0)` — not an error and not 1.
    #[test]
    fn test_calibration_returns_zero_on_shutdown() {
        let tmp = TempDir::new().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        // Signal shutdown before the function even begins its loop.
        shutdown.store(true, Ordering::Relaxed);

        let result = get_inode_ratio(tmp.path(), &shutdown, &make_args(100));

        // Bug #12 returns Ok(1) instead of Ok(0), defeating the
        // zero-ratio guard and causing mass false positives.
        // Bug #11 propagates the sentinel Err instead of Ok(0).
        assert_eq!(result.unwrap(), 0);
    }

    /// Sanity: calibration completes without error when no shutdown
    /// signal is set.
    #[test]
    fn test_calibration_completes_without_error() {
        let tmp = TempDir::new().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));

        let result = get_inode_ratio(tmp.path(), &shutdown, &make_args(10));

        assert!(
            result.is_ok(),
            "calibration should succeed when not interrupted"
        );
    }

    /// Bug #1: the divisor must be `calibration_count`, not
    /// `calibration_count - 1`.  With `count = 1` the mutated
    /// expression evaluates to `size / 0`, which panics.
    #[test]
    fn test_calibration_divisor_of_one_does_not_panic() {
        let tmp = TempDir::new().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));

        // Bug #1 mutation: size / (calibration_count - 1) = size / 0
        let result = get_inode_ratio(tmp.path(), &shutdown, &make_args(1));

        assert!(result.is_ok(), "calibration_count=1 must not panic");
    }

    /// Bug #2: the parallel iterator must create exactly
    /// `calibration_count` files.  If it creates `count - 1` files
    /// but still divides by `count`, the ratio is inflated.
    #[test]
    fn test_calibration_creates_exact_number_of_files() {
        let tmp = TempDir::new().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let count: u64 = 5;

        get_inode_ratio(tmp.path(), &shutdown, &make_args(count)).unwrap();

        let created = std::fs::read_dir(tmp.path()).unwrap().count() as u64;
        // Bug #2: iterator runs 0..count-1, creating count-1 files.
        assert_eq!(
            created, count,
            "exactly calibration_count files must be created"
        );
    }
}
