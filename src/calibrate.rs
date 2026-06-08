// SPDX-FileCopyrightText: 2022 Dinko Korunic <dinko.korunic@gmail.com>
//
// SPDX-License-Identifier: MIT

use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Error};
use fs_err as fs;
use rayon::prelude::*;

use crate::{args, progress};

/// Default number of files to create during calibration.
pub const DEFAULT_TEST_COUNT: u64 = 100;

/// Derives a filesystem's bytes-per-directory-entry by creating
/// `calibration_count` files in `test_path` and dividing the directory's
/// resulting inode size by that count. This ratio lets the walk estimate entry
/// counts from a single `stat` instead of an expensive `readdir`.
///
/// Returns `Ok(0)` if interrupted mid-calibration, which the caller treats as
/// "flagging disabled" rather than a real ratio.
///
/// # Errors
/// Fails if the thread pool cannot be built, a file cannot be created, or the
/// directory metadata cannot be read.
pub fn get_inode_ratio(
    test_path: &Path,
    shutdown: &Arc<AtomicBool>,
    args: &args::Args,
) -> Result<u64, Error> {
    println!("Starting test directory calibration in {}", test_path.display());

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(args.threads)
        .build()
        .context("Unable to spawn calibration thread pool")?;

    let pb = progress::new_spinner("Creating test files in progress...");

    // Short filenames keep per-entry inode cost minimal, sharpening the ratio.
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

    // Caller's TempDir cleans itself up on drop, so bailing out here is safe.
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

    mod get_inode_ratio {
        use super::*;

        /// A calibration run cut short by a shutdown signal must return
        /// `Ok(0)` — not an error and not a non-zero ratio.
        #[test]
        fn returns_zero_on_shutdown() {
            let tmp = TempDir::new().unwrap();
            let shutdown = Arc::new(AtomicBool::new(false));
            // Signal shutdown before the function even begins its loop.
            shutdown.store(true, Ordering::Relaxed);

            let result =
                get_inode_ratio(tmp.path(), &shutdown, &make_args(100));

            assert_eq!(result.unwrap(), 0);
        }

        /// Sanity: calibration completes without error when no shutdown
        /// signal is set.
        #[test]
        fn completes_without_error() {
            let tmp = TempDir::new().unwrap();
            let shutdown = Arc::new(AtomicBool::new(false));

            let result =
                get_inode_ratio(tmp.path(), &shutdown, &make_args(10));

            assert!(
                result.is_ok(),
                "calibration should succeed when not interrupted"
            );
        }

        /// The divisor must be `calibration_count`, not `calibration_count - 1`;
        /// with `count = 1` the latter would divide by zero and panic.
        #[test]
        fn divisor_of_one_does_not_panic() {
            let tmp = TempDir::new().unwrap();
            let shutdown = Arc::new(AtomicBool::new(false));

            let result = get_inode_ratio(tmp.path(), &shutdown, &make_args(1));

            assert!(result.is_ok(), "calibration_count=1 must not panic");
        }

        /// The parallel iterator must create exactly `calibration_count` files;
        /// creating fewer while still dividing by `count` inflates the ratio.
        #[test]
        fn creates_exact_number_of_files() {
            let tmp = TempDir::new().unwrap();
            let shutdown = Arc::new(AtomicBool::new(false));
            let count: u64 = 5;

            get_inode_ratio(tmp.path(), &shutdown, &make_args(count)).unwrap();

            let created =
                std::fs::read_dir(tmp.path()).unwrap().count() as u64;
            assert_eq!(
                created, count,
                "exactly calibration_count files must be created"
            );
        }
    }
}
