use std::collections::HashSet;
use std::fs::Metadata;
use std::fs::read_dir;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::sleep;
use std::time::Duration;

use ansi_term::Colour::{Green, Red, Yellow};
use anyhow::{Context, Error};
use fs_err as fs;
use human_bytes::human_bytes;
use human_format::Formatter;
use jwalk::{DirEntry, Parallelism, WalkDir};

use crate::args;

/// Default number of files in a folder to cause alert
pub const ALERT_COUNT: u64 = 10_000;

/// Default number of files in a folder to cause red alert and further blacklist from the deeper
/// scan
pub const BLACKLIST_COUNT: u64 = 100_000;

/// Default exit error code in case of premature termination
const ERROR_EXIT: i32 = 1;

/// Default status update period in seconds
pub const STATUS_SECONDS: u64 = 20;

/// Performs a parallel filesystem scan from a specified path using a thread pool.
///
/// This function scans directories starting from the given path, processing each directory entry
/// and updating the status periodically. It checks for a shutdown signal to gracefully stop the scan.
///
/// # Parameters:
/// - `path: &PathBuf`: The root path from where the scan starts.
/// - `path_metadata: Metadata`: Metadata of the root path for comparison and checks during the scan.
/// - `size_inode_ratio: u64`: Ratio to estimate file counts in directories based on inode size.
/// - `shutdown: Arc<AtomicBool>`: Shared flag to signal a shutdown, set by an interrupt handler.
/// - `args: Arc<args::Args>`: Contains configuration such as thread count, update intervals, and exclusion paths.
///
/// # Behavior:
/// - Initializes exclusion paths and sets up a thread pool for directory processing and status updates.
/// - Checks the `shutdown` flag periodically and exits with `ERROR_EXIT` code if set.
/// - Processes each directory entry to evaluate conditions like file count thresholds.
/// - Provides periodic status updates if enabled in `args`.
///
/// # Error Handling:
/// - Exits with an error code if unable to create the thread pool.
/// - Handles errors during directory traversal and metadata access.
///
/// # Returns:
/// - `Result<(), Error>`: Ok if the scan completes successfully, or an error wrapped in `Err` otherwise.
pub fn parallel_search(
    path: &PathBuf,
    path_metadata: Metadata,
    size_inode_ratio: u64,
    shutdown: Arc<AtomicBool>,
    args: Arc<args::Args>,
) -> Result<(), Error> {
    // Create hash set for path exclusions
    let skip_path = args.skip_path.iter().cloned().collect::<HashSet<_>>();

    // Thread pool for status reporting and filesystem walk
    let pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(args.threads)
            .build()
            .context("Unable to spawn calibration thread pool")?,
    );

    // Processed directory count
    let dir_count = Arc::new(AtomicU64::new(0));

    // Status update thread
    if args.updates > 0 {
        let dir_count_status = dir_count.clone();
        let sleep_delay = args.updates;

        pool.spawn(move || loop {
            sleep(Duration::from_secs(sleep_delay));

            let count = dir_count_status.load(Ordering::SeqCst);
            println!(
                "Processed {} directories so far, next update in {} seconds",
                Green.paint(count.to_string()),
                sleep_delay
            );
        });
    }

    // Perform target filesystem walking
    for _ in WalkDir::new(path)
        .skip_hidden(false)
        .sort(false)
        .parallelism(Parallelism::RayonExistingPool {
            pool,
            busy_timeout: None,
        })
        .process_read_dir(move |_, _, (), children| {
            // Terminate on received interrupt signal
            if shutdown.load(Ordering::SeqCst) {
                println!("Requested program exit, stopping scan...");
                process::exit(ERROR_EXIT);
            }

            for dir_entry_result in &mut *children {
                process_dir_entry(
                    &path_metadata,
                    size_inode_ratio,
                    dir_entry_result,
                    &skip_path,
                    &args,
                    &dir_count,
                );
            }
        })
    {}

    Ok(())
}

/// Processes each directory entry and in case of a directory inode, determines directory inode
/// size and possible directory entry count. If count exceeds `blacklist_threshold`, it will
/// give out fatal warning and abort deeper scanning, and in case of count being just above
/// `alert_threshold` but below `blacklist_threshold` it will print just an informative warning.
fn process_dir_entry<E>(
    path_metadata: &Metadata,
    size_inode_ratio: u64,
    dir_entry_result: &mut Result<DirEntry<((), ())>, E>,
    skip_path: &HashSet<PathBuf>,
    args: &Arc<args::Args>,
    dir_count_walk: &Arc<AtomicU64>,
) {
    if let Ok(dir_entry) = dir_entry_result {
        if let Some(ref err) = dir_entry.read_children_error {
            println!("Fatal program error, exiting: {err}");
            process::exit(ERROR_EXIT)
        }

        if dir_entry.file_type.is_dir() {
            if let Some(full_path) = dir_entry.read_children_path.as_ref() {
                // Visited directory count
                dir_count_walk.fetch_add(1, Ordering::SeqCst);

                // Ignore skip paths, typically being virtual filesystems (/proc, /dev, /sys, /run)
                if !skip_path.is_empty() && skip_path.contains(&full_path.to_path_buf()) {
                    println!(
                        "Skipping further scan at {} as requested",
                        full_path.display()
                    );

                    dir_entry.read_children_path = None;
                    return;
                }

                // Retrieve Unix metadata for a given directory
                if let Ok(dir_entry_metadata) = fs::metadata(full_path) {
                    // If `one_filesystem` flag has been set and if directory is not residing
                    // on the same device as top search path, print warning and abort deeper
                    // scanning
                    if args.one_filesystem && (dir_entry_metadata.dev() != path_metadata.dev()) {
                        println!(
                            "Identified filesystem boundary at {}, skipping...",
                            full_path.display()
                        );
                        dir_entry.read_children_path = None;

                        return;
                    }

                    // Identify size and calculate approximate directory entry count
                    let size = dir_entry_metadata.size();
                    let approx_files = size / size_inode_ratio;

                    // Print count warnings if necessary
                    if approx_files > args.blacklist_threshold {
                        print_offender(full_path, size, approx_files, args.accurate, true);
                        dir_entry.read_children_path = None;
                    } else if approx_files > args.alert_threshold {
                        print_offender(full_path, size, approx_files, args.accurate, false);
                    }
                }
            }
        }
    }
}

#[allow(clippy::cast_precision_loss)]
/// Print directory information with inode size from metadata and approximate directory entry
/// count.
fn print_offender(
    full_path: &Arc<Path>,
    size: u64,
    approx_files: u64,
    accurate: bool,
    red_alert: bool,
) {
    // Pretty print either the accurate directory count or the approximation
    let human_files = if accurate {
        let exact_files = match read_dir(full_path) {
            Ok(r) => r.count() as u64,
            Err(_) => approx_files,
        };
        Formatter::new().format(exact_files as f64)
    } else {
        Formatter::new().format(approx_files as f64)
    };

    println!(
        "Found directory {} with inode size {} and {}{} files",
        full_path.display(),
        human_bytes(size as f64),
        if accurate { "" } else { "approx " },
        if red_alert {
            Red.paint(human_files)
        } else {
            Yellow.paint(human_files)
        }
    );
}
