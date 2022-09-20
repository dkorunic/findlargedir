use anyhow::{Context, Error};
use rm_rf::ensure_removed;
use spinach::Spinach;
use std::fs;
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

    for i in 0..test_count {
        if shutdown.load(Ordering::Relaxed) {
            s.stop();
            println!("Requested program exit, stopping and deleting temporary files...",);
            ensure_removed(test_path)
                .expect("Unable to completely delete calibration directory, exiting");
            process::exit(ERROR_EXIT);
        }

        File::create(test_path.join(i.to_string()))
            .with_context(|| format!("Unable to create calibration test file {}", i))?;
        if i % 1000 == 0 {
            s.text(format!("Created {} files...", i));
        }
    }

    s.text("Done, getting total size and deleting temp folder");

    let tmp_dir_size = fs::metadata(test_path)
        .with_context(|| format!("Unable to stat {} directory", test_path.display()))?
        .size();

    s.succeed("Finished with calibration.");

    let size_inode_ratio = tmp_dir_size / test_count;
    println!("Calculated size-to-inode ratio: {}", size_inode_ratio);

    Ok(size_inode_ratio)
}
