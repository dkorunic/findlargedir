use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Error};
use fs_err as fs;
use rayon::prelude::*;
use rm_rf::ensure_removed;

use crate::{args, progress};

/// Default number of files to create in the calibration directory
pub const DEFAULT_TEST_COUNT: u64 = 100_000;

/// Default exit error code in case of premature termination
const ERROR_EXIT: i32 = 1;

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
/// delete the directory, or retrieve metadata from the test directory.
///
/// # Examples
/// ```
/// let test_path = Path::new("/tmp/test_dir");
/// let shutdown = Arc::new(AtomicBool::new(false));
/// let args = Arc::new(args::Args {
///     threads: 4,
///     calibration_count: 1000,
/// });
/// let ratio = get_inode_ratio(&test_path, &shutdown, &args);
/// match ratio {
///     Ok(ratio) => println!("Size-to-inode ratio: {}", ratio),
///     Err(e) => println!("Failed to calculate size-to-inode ratio: {}", e),
/// }
/// ```
pub fn get_inode_ratio(
    test_path: &Path,
    shutdown: &Arc<AtomicBool>,
    args: &Arc<args::Args>,
) -> Result<u64, Error> {
    println!(
        "Starting test directory calibration in {}",
        test_path.display(),
    );

    // Thread pool for mass file creation
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(args.threads)
        .build()
        .context("Unable to spawn calibration thread pool")?;

    let pb = progress::new_spinner("Creating test files in progress...");

    // Mass create files; filenames are short to get minimal size to inode ratio
    pool.install(|| {
        (0..args.calibration_count).into_par_iter().for_each(|i| {
            if !shutdown.load(Ordering::Acquire) {
                File::create(test_path.join(i.to_string())).expect("Unable to create files");
            }
        });
    });

    pb.finish_with_message("Done.");

    // Terminate on received interrupt signal
    if shutdown.load(Ordering::Acquire) {
        println!("Requested program exit, stopping and deleting temporary files...",);
        ensure_removed(test_path)
            .expect("Unable to completely delete calibration directory, exiting");
        process::exit(ERROR_EXIT);
    }

    let size_inode_ratio = fs::metadata(test_path)?.size() / args.calibration_count;
    println!("Calibration done. Calculated size-to-inode ratio: {size_inode_ratio}");

    Ok(size_inode_ratio)
}
