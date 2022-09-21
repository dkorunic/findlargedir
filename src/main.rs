#![warn(clippy::all, clippy::pedantic)]
mod args;
mod calibrate;
mod interrupt;
mod walk;

use anyhow::{Context, Error, Result};
use clap::Parser;
use fs_err as fs;
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tempfile::TempDir;
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

fn main() -> Result<(), Error> {
    let args = args::Args::parse();

    // Setup SIGINT, SIGTERM and SIGHUP signal handler that will cause calibration to stop
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_calibrate = shutdown.clone();
    let shutdown_scan = shutdown.clone();
    interrupt::setup_interrupt_handler(shutdown)?;

    let mut visited_paths = HashSet::new();

    for path in args.path.clone() {
        match visited_paths.get(&path) {
            None => visited_paths.insert(path.clone()),
            _ => continue,
        };

        let path_metadata = fs::metadata(&path)?;

        let tmp_dir = Arc::new(
            TempDir::new_in(&path).context("Unable to setup/create calibration test directory")?,
        );

        let size_inode_ratio = if let Some(ref calibration_path) = args.calibration_path {
            if fs::metadata(calibration_path.as_path())?.dev() != path_metadata.dev() {
                println!(
                    "Warning: test directory resides on a different device than path {}",
                    path.display()
                );
            }

            let tmp_dir = Arc::new(
                TempDir::new_in(calibration_path.as_path())
                    .context("Unable to setup/create calibration test directory")?,
            );

            calibrate::get_inode_ratio(tmp_dir.path(), &shutdown_calibrate, args.calibration_count)
                .context("Unable to calibrate inode to size ratio")?
        } else {
            let tmp_dir = Arc::new(
                TempDir::new_in(tmp_dir.path())
                    .context("Unable to setup/create calibration test directory")?,
            );

            calibrate::get_inode_ratio(tmp_dir.path(), &shutdown_calibrate, args.calibration_count)
                .context("Unable to calibrate inode to size ratio")?
        };

        println!("Scanning filesystem path {} started", path.display());

        walk::parallel_search(
            &path,
            path_metadata,
            size_inode_ratio,
            shutdown_scan.clone(),
            &args,
        );

        println!("Scanning filesystem path {} completed", path.display());
    }

    Ok(())
}
