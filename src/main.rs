#![warn(clippy::all, clippy::pedantic)]
mod calibrate;
mod interrupt;
mod walk;

use anyhow::{Context, Error, Result};
use clap::Parser;
use std::collections::HashSet;
use std::fs;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tempfile::TempDir;
use tikv_jemallocator::Jemalloc;

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    #[clap(short, long, action = clap::ArgAction::Set, default_value_t = false)]
    accurate: bool,

    #[clap(short, long, action = clap::ArgAction::Set, default_value_t = true)]
    one_filesystem: bool,

    #[clap(short, long, value_parser, default_value_t = calibrate::DEFAULT_TEST_COUNT)]
    calibration_count: u64,

    #[clap(short = 'A', long, value_parser, default_value_t = walk::ALERT_COUNT)]
    alert_threshold: u64,

    #[clap(short = 'B', long, value_parser, default_value_t = walk::BLACKLIST_COUNT)]
    blacklist_threshold: u64,

    #[clap(required = true, value_parser)]
    path: Vec<String>,
}

fn main() -> Result<(), Error> {
    let args = Args::parse();

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_calibrate = shutdown.clone();
    interrupt::setup_interrupt_handler(shutdown)?;

    let mut visited_paths = HashSet::new();

    for path in args.path {
        match visited_paths.get(&path) {
            None => visited_paths.insert(path.clone()),
            _ => continue,
        };

        let path_metadata =
            fs::metadata(&path).with_context(|| format!("Unable to stat {} directory", &path))?;

        let tmp_dir = Arc::new(
            TempDir::new_in(&path).context("Unable to setup/create calibration test directory")?,
        );

        let size_inode_ratio =
            calibrate::get_inode_ratio(tmp_dir.path(), &shutdown_calibrate, args.calibration_count)
                .context("Unable to calibrate inode to size ratio")?;

        println!("Please wait a few seconds while removing calibration test directory...");
        drop(tmp_dir);

        println!("Scanning filesystem path {} started", &path);

        walk::parallel_search(
            &path,
            path_metadata,
            size_inode_ratio,
            args.accurate,
            args.one_filesystem,
            args.alert_threshold,
            args.blacklist_threshold,
        );

        println!("Scanning filesystem path {} completed", &path);
    }

    Ok(())
}
