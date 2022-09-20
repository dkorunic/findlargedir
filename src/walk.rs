use ansi_term::Colour::{Red, Yellow};
use anyhow::Context;
use human_bytes::human_bytes;
use human_format::Formatter;
use jwalk::{DirEntry, Parallelism, WalkDir};
use std::fs;
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;

pub const ALERT_COUNT: u64 = 10_000;
pub const BLACKLIST_COUNT: u64 = 100_000;

pub fn parallel_search(
    path: &String,
    path_metadata: Metadata,
    size_inode_ratio: u64,
    accurate: bool,
    one_filesystem: bool,
    alert_threshold: u64,
    blacklist_threshold: u64,
) {
    for _ in WalkDir::new(&path)
        .skip_hidden(false)
        .sort(false)
        .parallelism(Parallelism::RayonNewPool(num_cpus::get()))
        .process_read_dir(move |_depth, _path, _read_dir_state, children| {
            for dir_entry_result in children.iter_mut() {
                process_dir_entry(
                    &path_metadata,
                    size_inode_ratio,
                    dir_entry_result,
                    accurate,
                    one_filesystem,
                    alert_threshold,
                    blacklist_threshold,
                );
            }
        })
    {}
}

#[allow(clippy::cast_precision_loss)]
fn process_dir_entry<E>(
    path_metadata: &Metadata,
    size_inode_ratio: u64,
    dir_entry_result: &mut Result<DirEntry<((), ())>, E>,
    _accurate: bool,
    one_filesystem: bool,
    alert_threshold: u64,
    blacklist_threshold: u64,
) {
    if let Ok(dir_entry) = dir_entry_result {
        if dir_entry.file_type.is_dir() {
            if let Some(full_path) = dir_entry.read_children_path.as_ref() {
                let dir_entry_metadata = fs::metadata(&full_path)
                    .with_context(|| format!("Unable to stat {} directory", &full_path.display()))
                    .unwrap();
                if dir_entry_metadata.dev() == path_metadata.dev() {
                    let size = dir_entry_metadata.size();
                    let approx_files = size / size_inode_ratio;

                    if approx_files > blacklist_threshold {
                        println!(
                            "Found very large directory {} with inode size {}, approx {} files",
                            full_path.display(),
                            human_bytes(size as f64),
                            Red.paint(Formatter::new().format(approx_files as f64)),
                        );
                        dir_entry.read_children_path = None;
                    } else if approx_files > alert_threshold {
                        println!(
                            "Found large directory {} with inode size {}, approx {} files",
                            full_path.display(),
                            human_bytes(size as f64),
                            Yellow.paint(Formatter::new().format(approx_files as f64)),
                        );
                    }
                } else if one_filesystem {
                    println!(
                        "Identified filesystem boundary at {}, skipping...",
                        full_path.display()
                    );
                    dir_entry.read_children_path = None;
                }
            }
        }
    }
}
