#![warn(clippy::all, clippy::pedantic)]

use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use ahash::AHashSet;
use anyhow::{Context, Error, Result};
use clap::Parser;
use fdlimit::{Outcome, raise_fd_limit};
use fs_err as fs;
use indicatif::HumanDuration;
use tempfile::TempDir;

mod args;
mod calibrate;
mod interrupt;
mod progress;
mod walk;

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// Entry point for the filesystem scanning application.
///
/// This function sets up necessary configurations and initiates the parallel filesystem scan
/// by calling `parallel_search`. It handles command-line arguments and sets up the environment
/// for the application to run.
///
/// # Behavior:
/// - Parses command-line arguments to configure the scanning process.
/// - Sets up signal handling for graceful shutdowns.
/// - Initiates the filesystem scan by calling `parallel_search` with appropriate parameters.
/// - Handles any errors returned by `parallel_search` and exits with an appropriate status code.
///
/// # Returns:
/// - Typically does not return and calls `std::process::exit` to terminate the program.
fn main() -> Result<(), Error> {
    let args = Arc::new(args::Args::parse());

    // Setup termination signal (SIGINT, SIGTERM and SIGQUIT) handlers that will cause program to stop
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_walk = shutdown.clone();
    interrupt::setup_interrupt_handler(&shutdown)?;

    println!("Using {} threads for calibration and scanning", args.threads);

    // Attempt to raise FD limit
    if let Ok(Outcome::LimitRaised { to: x, .. }) = raise_fd_limit() {
        println!("Maximum number of file descriptors available: {x}");
    }

    // Search only unique paths
    let mut visited_paths = AHashSet::with_capacity(args.path.len());

    for path in args.path.clone() {
        // Keep order of provided path arguments, but skip already visited paths
        match visited_paths.get(&path) {
            None => visited_paths.insert(path.clone()),
            _ => continue,
        };

        println!("Started analysis for path {}", path.display());

        // Retrieve Unix metadata for top search path
        let path_metadata = fs::metadata(&path)
            .context("Unable to retrieve top search directory metadata")?;

        // Directory inode size to number of entries ratio is either manually provided in
        // `args.size_inode_ratio` or determined from manually provided calibration path
        // `args.calibration_path` or determined from calibration directory created in search root
        // `TempDir::new_in(path.as_path())`
        let size_inode_ratio = if args.size_inode_ratio > 0 {
            args.size_inode_ratio
        } else if let Some(ref user_path) = args.calibration_path {
            // User has specified his calibration directory so attempt to check if it resides on
            // the same device
            if fs::metadata(user_path.as_path()).context(
                "Unable to retrieve user-specified calibration directory metadata",
            )?.dev() != path_metadata.dev()
            {
                println!(
                    "Oops, test directory resides on a different device than path {}, results are possibly unreliable!",
                    path.display()
                );
            }

            // Prepare temporary calibration directory in user path
            let tmp_dir =
                Arc::new(TempDir::new_in(user_path.as_path()).context(
                    "Unable to setup/create calibration test directory",
                )?);

            calibrate::get_inode_ratio(tmp_dir.path(), &shutdown_walk, &args)
                .context("Unable to calibrate inode to size ratio")?
        } else {
            // Prepare temporary calibration directory in root of the search path
            let tmp_dir = Arc::new(TempDir::new_in(path.as_path()).context(
                "Unable to setup/create calibration test directory",
            )?);

            calibrate::get_inode_ratio(tmp_dir.path(), &shutdown_walk, &args)
                .context("Unable to calibrate inode to size ratio")?
        };

        let start = Instant::now();
        let pb = progress::new_spinner(format!(
            "Scanning path {} in progress...",
            path.display()
        ));

        let dir_count = walk::parallel_search(
            &path,
            &path_metadata,
            size_inode_ratio,
            &shutdown_walk,
            &args,
        );

        pb.finish_with_message("Done.");

        println!(
            "Scanning path {} completed. Directories scanned: {}, Time elapsed: {}",
            path.display(),
            dir_count,
            HumanDuration(start.elapsed())
        );
    }

    Ok(())
}
