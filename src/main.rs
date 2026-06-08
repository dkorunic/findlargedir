// SPDX-FileCopyrightText: 2022 Dinko Korunic <dinko.korunic@gmail.com>
//
// SPDX-License-Identifier: MIT

use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
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

/// Parses arguments, installs signal handlers, then for each unique path
/// calibrates the size-to-inode ratio (unless one is supplied) and runs the
/// parallel scan, printing flagged directories as it goes.
fn main() -> Result<(), Error> {
    let args = Arc::new(args::Args::parse());

    // A non-strict ordering makes the yellow-alert branch unreachable.
    if args.alert_threshold >= args.blacklist_threshold {
        anyhow::bail!(
            "alert threshold ({}) must be less than blacklist threshold ({})",
            args.alert_threshold,
            args.blacklist_threshold
        );
    }

    let shutdown_walk = Arc::new(AtomicBool::new(false));
    interrupt::setup_interrupt_handler(&shutdown_walk)?;

    // Honor the requested thread count, but warn past the core count.
    let available =
        thread::available_parallelism().map_or(2, std::num::NonZeroUsize::get);
    if let Some(w) = args::oversubscription_warning(args.threads, available) {
        eprintln!("findlargedir: {w}");
    }

    println!("Using {} threads for calibration and scanning", args.threads);

    // Mass file creation and parallel walking are FD-hungry.
    if let Ok(Outcome::LimitRaised { to: x, .. }) = raise_fd_limit() {
        println!("Maximum number of file descriptors available: {x}");
    }

    // Built once, shared across every search root.
    let skip_path_set: AHashSet<PathBuf> =
        args.skip_path.iter().cloned().collect();

    let mut visited_paths = AHashSet::with_capacity(args.path.len());

    'paths: for path in &args.path {
        // Canonicalize so symlinked aliases of one directory scan once; on
        // failure (permissions, broken links) the normalised path still dedupes.
        let canonical =
            fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        if !visited_paths.insert(canonical) {
            continue;
        }

        println!("Started analysis for path {}", path.display());

        let path_metadata = fs::metadata(path)
            .context("Unable to retrieve top search directory metadata")?;

        // Ratio source, in priority order: caller-supplied, calibrated in a
        // user-chosen dir, or calibrated in a temp dir at the search root.
        let calibration = if args.size_inode_ratio > 0 {
            // -i escape hatch: per-entry only, no measured overhead.
            calibrate::Calibration {
                per_entry: args.size_inode_ratio,
                overhead: 0,
            }
        } else if let Some(ref user_path) = args.calibration_path {
            // A different device would calibrate the wrong filesystem.
            if fs::metadata(user_path.as_path()).context(
                "Unable to retrieve user-specified calibration directory metadata",
            )?.dev() != path_metadata.dev()
            {
                println!(
                    "Oops, test directory resides on a different device than path {}, results are possibly unreliable!",
                    path.display()
                );
            }

            let tmp_dir = TempDir::new_in(user_path.as_path()).context(
                "Unable to setup/create calibration test directory",
            )?;

            calibrate::get_inode_ratio(tmp_dir.path(), &shutdown_walk, &args)
                .context("Unable to calibrate inode to size ratio")?
        } else {
            let tmp_dir = TempDir::new_in(path.as_path()).context(
                "Unable to setup/create calibration test directory",
            )?;

            calibrate::get_inode_ratio(tmp_dir.path(), &shutdown_walk, &args)
                .context("Unable to calibrate inode to size ratio")?
        };

        // Don't start a walk if calibration was interrupted.
        if shutdown_walk.load(Ordering::Relaxed) {
            println!("Requested program exit, stopping scan...");
            break 'paths;
        }

        let start = Instant::now();
        let pb = progress::new_spinner(format!(
            "Scanning path {} in progress...",
            path.display()
        ));

        let dir_count = walk::parallel_search(
            path,
            &path_metadata,
            calibration,
            &shutdown_walk,
            &args,
            &skip_path_set,
        );

        pb.finish_with_message("Done.");

        if shutdown_walk.load(Ordering::Relaxed) {
            println!("Requested program exit, stopping scan...");
            break 'paths;
        }

        println!(
            "Scanning path {} completed. Directories scanned: {}, Time elapsed: {}",
            path.display(),
            dir_count,
            HumanDuration(start.elapsed())
        );
    }

    Ok(())
}
