use crate::args;
use ansi_term::Colour::{Red, Yellow};
use fs_err as fs;
use human_bytes::human_bytes;
use human_format::Formatter;
use jwalk::{DirEntry, Parallelism, WalkDir};
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const ALERT_COUNT: u64 = 10_000;
pub const BLACKLIST_COUNT: u64 = 100_000;
const ERROR_EXIT: i32 = 1;

pub fn parallel_search(
    path: &PathBuf,
    path_metadata: Metadata,
    size_inode_ratio: u64,
    shutdown: Arc<AtomicBool>,
    args: &args::Args,
) {
    let (one_filesystem, alert_threshold, blacklist_threshold) = (
        args.one_filesystem,
        args.alert_threshold,
        args.blacklist_threshold,
    );

    for _ in WalkDir::new(path)
        .skip_hidden(false)
        .sort(false)
        .parallelism(Parallelism::RayonNewPool(num_cpus::get()))
        .process_read_dir(move |_, _, _, children| {
            if shutdown.load(Ordering::SeqCst) {
                println!("Requested program exit, stopping scan...");
                process::exit(ERROR_EXIT);
            }

            for dir_entry_result in children.iter_mut() {
                process_dir_entry(
                    &path_metadata,
                    size_inode_ratio,
                    dir_entry_result,
                    one_filesystem,
                    alert_threshold,
                    blacklist_threshold,
                );
            }
        })
    {}
}

fn process_dir_entry<E>(
    path_metadata: &Metadata,
    size_inode_ratio: u64,
    dir_entry_result: &mut Result<DirEntry<((), ())>, E>,
    one_filesystem: bool,
    alert_threshold: u64,
    blacklist_threshold: u64,
) {
    if let Ok(dir_entry) = dir_entry_result {
        if dir_entry.file_type.is_dir() {
            if let Some(full_path) = dir_entry.read_children_path.as_ref() {
                if let Ok(dir_entry_metadata) = fs::metadata(full_path) {
                    if one_filesystem && (dir_entry_metadata.dev() != path_metadata.dev()) {
                        println!(
                            "Identified filesystem boundary at {}, skipping...",
                            full_path.display()
                        );
                        dir_entry.read_children_path = None;

                        return;
                    }

                    let size = dir_entry_metadata.size();
                    let approx_files = size / size_inode_ratio;

                    if approx_files > blacklist_threshold {
                        print_offender(full_path, size, approx_files, true);
                        dir_entry.read_children_path = None;
                    } else if approx_files > alert_threshold {
                        print_offender(full_path, size, approx_files, false);
                    }
                }
            }
        }
    }
}

#[allow(clippy::cast_precision_loss)]
fn print_offender(full_path: &Arc<Path>, size: u64, approx_files: u64, red_alert: bool) {
    let human_files = Formatter::new().format(approx_files as f64);
    println!(
        "Found directory {} with inode size {}, approx {} files",
        full_path.display(),
        human_bytes(size as f64),
        if red_alert {
            Red.paint(human_files)
        } else {
            Yellow.paint(human_files)
        }
    );
}
