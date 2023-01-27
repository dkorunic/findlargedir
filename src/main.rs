#![warn(clippy::all, clippy::pedantic)]
mod args;
mod calibrate;
mod interrupt;
mod progress;
mod walk;

use anyhow::{Context, Error, Result};
use cfg_if::cfg_if;
use clap::Parser;
use fs_err as fs;
use humantime::Duration as HumanDuration;
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;
use tempfile::TempDir;

cfg_if! {
    if #[cfg(linux)] {
        if #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))] {
            use tikv_jemallocator::Jemalloc;

            #[global_allocator]
            static GLOBAL: Jemalloc = Jemalloc;
        }
    }
}

fn main() -> Result<(), Error> {
    let args = Arc::new(args::Args::parse());

    // Setup SIGINT, SIGTERM and SIGHUP signal handler that will cause calibration to stop
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_scan = shutdown.clone();
    interrupt::setup_interrupt_handler(shutdown)?;

    // Search only unique paths
    let mut visited_paths = HashSet::with_capacity(args.path.len());

    for path in args.path.clone() {
        // Keep order of provided path arguments, but skip already visited paths
        match visited_paths.get(&path) {
            None => visited_paths.insert(path.clone()),
            _ => continue,
        };

        println!("Started analysis for path {}", path.display());

        // Retrieve Unix metadata for top search path
        let path_metadata = fs::metadata(&path)?;

        // Directory inode size to number of entries ratio is either manually provided in
        // `args.size_inode_ratio` or determined from manually provided calibration path
        // `args.calibration_path` or determined from calibration directory created in search root
        // `TempDir::new_in(path.as_path())`
        let size_inode_ratio = if args.size_inode_ratio > 0 {
            args.size_inode_ratio
        } else if let Some(ref user_path) = args.calibration_path {
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

        let start = Instant::now();
        let pb = progress::new_spinner(format!("Scanning path {} in progress...", path.display()));

        walk::parallel_search(
            &path,
            path_metadata,
            size_inode_ratio,
            shutdown_scan.clone(),
            args.clone(),
        )?;

        pb.finish_with_message("Done.");

        println!(
            "Scanning path {} completed. Time elapsed: {}",
            path.display(),
            HumanDuration::from(start.elapsed())
        );
    }

    Ok(())
}
