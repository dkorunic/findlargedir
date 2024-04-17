#![warn(clippy::all, clippy::pedantic)]

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use anyhow::{Context, Error, Result};
use cfg_if::cfg_if;
use clap::Parser;
use fdlimit::{Outcome, raise_fd_limit};
use fs_err as fs;
use humantime::Duration as HumanDuration;
use tempfile::TempDir;

mod args;
mod calibrate;
mod interrupt;
mod progress;
mod walk;

cfg_if! {
    if #[cfg(all(target_os = "linux", target_arch = "x86_64"))] {
        use_jemalloc!();
    } else if #[cfg(all(target_os = "linux", target_arch = "aarch64"))] {
        use_jemalloc!();
    } else if #[cfg(target_os = "macos")] {
        use_jemalloc!();
    }
}

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

    // Setup SIGINT, SIGTERM and SIGHUP signal handler that will cause calibration to stop
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_scan = shutdown.clone();
    interrupt::setup_interrupt_handler(shutdown)?;

    println!(
        "Using {} threads for calibration and scanning",
        args.threads
    );

    // Attempt to raise FD limit
    if let Ok(Outcome::LimitRaised { to: x, .. }) = raise_fd_limit() {
        println!("Maximum number of file descriptors available: {x}");
    }

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
/// Macro to enable the use of the jemalloc allocator in a Rust project.
///
/// This macro configures the project to use jemalloc instead of the default
/// allocator. jemalloc is often preferred for its performance characteristics,
/// especially in multithreaded environments.
///
/// # Usage
/// Place this macro at the top of your main.rs or lib.rs to enable jemalloc
/// for your entire Rust project.
///
/// # Example
/// ```
/// use_jemalloc!();
/// ```
///
/// Note: Ensure that the `jemalloc` crate is included in your project's
/// dependencies.
#[macro_export]
macro_rules! use_jemalloc {
    () => {
        use tikv_jemallocator::Jemalloc;

        #[global_allocator]
        static GLOBAL: Jemalloc = Jemalloc;
    };
}
