use crate::{args, progress};

use anyhow::{Context, Error};
use fs_err as fs;
use rayon::prelude::*;
use rm_rf::ensure_removed;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Default number of files to create in the calibration directory
pub const DEFAULT_TEST_COUNT: u64 = 100_000;

/// Default exit error code in case of premature termination
const ERROR_EXIT: i32 = 1;

/// Creates `test_count` files in `test_path` as fast as possible, gets final test directory
/// inode size and establishes directory total size to directory count ratio that can be used
/// to guess directory counts directly from directory inode size. Temporary calibration directory
/// is erased both on success and termination through interrupt signals.
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
            if !shutdown.load(Ordering::SeqCst) {
                File::create(test_path.join(i.to_string())).expect("Unable to create files");
            }
        });
    });

    pb.finish_with_message("Done.");

    // Terminate on received interrupt signal
    if shutdown.load(Ordering::SeqCst) {
        println!("Requested program exit, stopping and deleting temporary files...",);
        ensure_removed(test_path)
            .expect("Unable to completely delete calibration directory, exiting");
        process::exit(ERROR_EXIT);
    }

    let size_inode_ratio = fs::metadata(test_path)?.size() / args.calibration_count;
    println!(
        "Calibration done. Calculated size-to-inode ratio: {}",
        size_inode_ratio
    );

    Ok(size_inode_ratio)
}
