#![warn(clippy::all, clippy::pedantic)]
mod args;
mod calibrate;
mod interrupt;
mod walk;

use anyhow::{Context, Error, Result};
use clap::Parser;
use fs_err as fs;
use humantime::Duration;
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;
use tempfile::TempDir;
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

fn main() -> Result<(), Error> {
    let args = args::Args::parse();

    // Setup SIGINT, SIGTERM and SIGHUP signal handler that will cause calibration to stop
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_scan = shutdown.clone();
    interrupt::setup_interrupt_handler(shutdown)?;

    // Search only unique paths
    let mut visited_paths = HashSet::new();

    for path in args.path.clone() {
        match visited_paths.get(&path) {
            None => visited_paths.insert(path.clone()),
            _ => continue,
        };

        // Retrieve Unix metadata for top search path
        let path_metadata = fs::metadata(&path)?;

        let size_inode_ratio = if let Some(ref user_path) = args.calibration_path {
            // User has specified his calibration directory so attempt to check if it resides on
            // the same device
            if fs::metadata(user_path.as_path())?.dev() != path_metadata.dev() {
                println!(
                    "Oops, test directory resides on a different device than path {}, results are possibly unreliable!",
                    path.display()
                );
            }

            // Prepare temporary calibration directory in user path
            let tmp_dir = Arc::new(
                TempDir::new_in(user_path.as_path())
                    .context("Unable to setup/create calibration test directory")?,
            );

            calibrate::get_inode_ratio(tmp_dir.path(), &shutdown_scan, &args)
                .context("Unable to calibrate inode to size ratio")?
        } else {
            // Prepare temporary calibration directory in root of the search path
            let tmp_dir = Arc::new(
                TempDir::new_in(path.as_path())
                    .context("Unable to setup/create calibration test directory")?,
            );

            calibrate::get_inode_ratio(tmp_dir.path(), &shutdown_scan, &args)
                .context("Unable to calibrate inode to size ratio")?
        };

        println!("Scanning filesystem path {} started", path.display());

        let start = Instant::now();

        walk::parallel_search(
            &path,
            path_metadata,
            size_inode_ratio,
            shutdown_scan.clone(),
            &args,
        )?;

        println!(
            "Scanning filesystem path {} completed. Time elapsed: {}",
            path.display(),
            Duration::from(start.elapsed())
        );
    }

    Ok(())
}
