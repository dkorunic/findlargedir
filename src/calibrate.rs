use anyhow::{Context, Error};
use fs_err as fs;
use rayon::prelude::*;
use rm_rf::ensure_removed;
use spinach::Spinach;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const DEFAULT_TEST_COUNT: u64 = 100_000;
const ERROR_EXIT: i32 = 1;

pub fn get_inode_ratio(
    test_path: &Path,
    shutdown: &Arc<AtomicBool>,
    test_count: u64,
) -> Result<u64, Error> {
    println!(
        "Running test directory calibration in: {}",
        test_path.display(),
    );

    let s = Spinach::new("Starting calibration...");

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(num_cpus::get())
        .build()
        .context("Unable to spawn calibration thread pool")?;

    pool.install(|| {
        (0..test_count).into_par_iter().for_each(|i| {
            if !shutdown.load(Ordering::SeqCst) {
                File::create(test_path.join(i.to_string())).expect("Unable to create files");
            }
        });
    });

    if shutdown.load(Ordering::SeqCst) {
        s.stop();
        println!("Requested program exit, stopping and deleting temporary files...",);
        ensure_removed(test_path)
            .expect("Unable to completely delete calibration directory, exiting");
        process::exit(ERROR_EXIT);
    }

    s.text("Done, getting total size and deleting temp folder");

    let tmp_dir_size = fs::metadata(test_path)?.size();

    s.succeed("Finished with calibration.");

    let size_inode_ratio = tmp_dir_size / test_count;
    println!("Calculated size-to-inode ratio: {}", size_inode_ratio);

    Ok(size_inode_ratio)
}
