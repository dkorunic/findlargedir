// SPDX-FileCopyrightText: 2022 Dinko Korunic <dinko.korunic@gmail.com>
//
// SPDX-License-Identifier: MIT

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Instant;

use ahash::AHashSet;
use anyhow::{Context, Error, Result};
use fdlimit::{Outcome, raise_fd_limit};
use fs_err as fs;
use indicatif::HumanDuration;
use tempfile::TempDir;

mod args;
mod calibrate;
mod compat;
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
    // Before anything prints or spawns a thread: die on EPIPE like a normal
    // Unix tool rather than panicking inside println!.
    interrupt::restore_sigpipe_default();

    let args = Arc::new(args::Args::parse());

    // A non-strict ordering makes the yellow-alert branch unreachable.
    if args.alert_threshold >= args.blacklist_threshold {
        anyhow::bail!(
            "alert threshold ({}) must be less than blacklist threshold ({})",
            args.alert_threshold,
            args.blacklist_threshold
        );
    }

    if let Some(w) = args::ignored_calibration_path_warning(&args) {
        eprintln!("findlargedir: {w}");
    }

    let shutdown_walk = Arc::new(AtomicBool::new(false));
    interrupt::setup_interrupt_handler(&shutdown_walk)?;

    // Honor the requested thread count, but warn past the core count.
    let available =
        thread::available_parallelism().map_or(2, std::num::NonZeroUsize::get);
    if let Some(w) = args::oversubscription_warning(args.threads, available) {
        eprintln!("findlargedir: {w}");
    }

    println!("Using {} threads for scanning", args.threads);

    // The walk holds one open directory fd per worker, and -a re-reads large
    // directories; raise the ceiling on platforms with small defaults.
    if let Ok(Outcome::LimitRaised { to: x, .. }) = raise_fd_limit() {
        println!("Maximum number of file descriptors available: {x}");
    }

    // Built once, shared across every search root.
    let skip_path_set: AHashSet<PathBuf> =
        args.skip_path.iter().cloned().collect();

    let mut visited_paths: Vec<PathBuf> = Vec::with_capacity(args.path.len());

    'paths: for path in &args.path {
        // Canonicalize so symlinked aliases of one directory scan once; on
        // failure (permissions, broken links) the normalised path still
        // dedupes. A root nested inside an already scanned one would be
        // walked twice.
        let canonical =
            fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        if is_subsumed(&visited_paths, &canonical) {
            println!(
                "Path {} is inside an already scanned root, skipping",
                path.display()
            );
            continue;
        }
        visited_paths.push(canonical);

        println!(
            "Started analysis for path {}, filesystem {}",
            path.display(),
            calibrate::fs_type_name(path)
        );

        let path_metadata = fs::metadata(path)
            .context("Unable to retrieve top search directory metadata")?;

        let calibration =
            resolve_calibration(path, &path_metadata, &shutdown_walk, &args)?;

        // Don't start a walk if calibration was interrupted.
        if shutdown_walk.load(Ordering::Relaxed) {
            println!("Requested program exit, stopping scan...");
            break 'paths;
        }

        let start = Instant::now();

        // parallel_search owns the scan spinner so it can pause it while a
        // crossed filesystem is calibrated mid-walk.
        let dir_count = walk::parallel_search(
            path,
            &path_metadata,
            calibration,
            &shutdown_walk,
            &args,
            &skip_path_set,
        );

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

/// Whether `canon` is inside (or equal to) an already scanned root.
/// `Path::starts_with` is component-aware, so `/a/b` matches root `/a` but
/// `/ab` does not.
fn is_subsumed(visited: &[PathBuf], canon: &Path) -> bool {
    visited.iter().any(|v| canon.starts_with(v))
}

/// Resolves the calibration for `path`'s filesystem, in priority order: a fixed
/// `-i` ratio (no files written), else calibration in the `-t` dir if given,
/// else a temp dir at the search root. A read-only, unwritable, or otherwise
/// uncalibratable directory degrades to flagging-disabled for this path rather
/// than aborting the run — the same policy the walk applies to crossed
/// filesystems.
///
/// # Errors
/// Fails only on bad user input: unreadable `-t` metadata, or a `-t` dir on a
/// different device than the scan root — that would fit the wrong filesystem
/// and the misfitted ratio would drive skip-subtree decisions.
fn resolve_calibration(
    path: &std::path::Path,
    path_metadata: &std::fs::Metadata,
    shutdown: &Arc<AtomicBool>,
    args: &args::Args,
) -> Result<calibrate::Calibration, Error> {
    if args.size_inode_ratio > 0 {
        // -i escape hatch: per-entry only, no measured overhead.
        return Ok(calibrate::Calibration {
            per_entry: args.size_inode_ratio,
            overhead: 0,
        });
    }

    let cal_dir = args.calibration_path.as_deref().unwrap_or(path);

    // A `-t` dir on a different device calibrates the wrong fs.
    if let Some(user_path) = args.calibration_path.as_deref()
        && compat::dev(&fs::metadata(user_path).context(
            "Unable to retrieve user-specified calibration directory metadata",
        )?) != compat::dev(path_metadata)
    {
        anyhow::bail!(
            "calibration directory {} is on a different filesystem than {}; \
             use a directory on the same filesystem or omit -t",
            user_path.display(),
            path.display()
        );
    }

    if calibrate::is_read_only(cal_dir) {
        println!(
            "Skipping calibration on read-only filesystem {}; size-based flagging disabled",
            cal_dir.display()
        );
        return Ok(calibrate::Calibration::DISABLED);
    }

    let tmp_dir = match TempDir::new_in(cal_dir) {
        Ok(t) => t,
        Err(e) => {
            println!(
                "Warning: cannot create calibration directory in {} ({e}); \
                 size-based flagging disabled for this path",
                cal_dir.display()
            );
            return Ok(calibrate::Calibration::DISABLED);
        }
    };
    match calibrate::get_inode_ratio(tmp_dir.path(), cal_dir, shutdown, args) {
        Ok(cal) => Ok(cal),
        Err(e) => {
            println!(
                "Warning: calibration failed at {} ({e:#}); \
                 size-based flagging disabled for this path",
                cal_dir.display()
            );
            Ok(calibrate::Calibration::DISABLED)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use tempfile::TempDir;

    use super::{is_subsumed, resolve_calibration};
    use crate::args;
    use crate::calibrate::Calibration;

    mod is_subsumed {
        use std::path::{Path, PathBuf};

        use super::is_subsumed;

        /// A root equal to or nested inside a scanned one is subsumed.
        #[test]
        fn equal_and_nested_are_subsumed() {
            let v = vec![PathBuf::from("/a")];
            assert!(is_subsumed(&v, Path::new("/a")));
            assert!(is_subsumed(&v, Path::new("/a/b/c")));
        }

        /// `/` as a scanned root subsumes everything below it.
        #[test]
        fn fs_root_subsumes_everything() {
            let v = vec![PathBuf::from("/")];
            assert!(is_subsumed(&v, Path::new("/home")));
        }

        /// Component-aware matching: `/ab` is not under `/a`, and an empty
        /// history subsumes nothing.
        #[test]
        fn prefix_strings_do_not_subsume() {
            let v = vec![PathBuf::from("/a")];
            assert!(!is_subsumed(&v, Path::new("/ab")));
            assert!(!is_subsumed(&v, Path::new("/b")));
            assert!(!is_subsumed(&[], Path::new("/a")));
        }
    }

    fn base_args() -> args::Args {
        args::Args {
            calibration_count: 100,
            calibration_name_length: crate::calibrate::DEFAULT_NAME_LEN,
            threads: 2,
            updates: 0,
            alert_threshold: 10_000,
            blacklist_threshold: 100_000,
            one_filesystem: false,
            follow_symlinks: false,
            accurate: false,
            size_inode_ratio: 0,
            calibration_path: None,
            skip_path: vec![],
            path: vec![],
        }
    }

    mod resolve_calibration {
        use super::*;

        /// A calibration target that can't hold a temp dir (here: `-t` naming
        /// a regular file) degrades to flagging-disabled instead of aborting
        /// the run — the same policy the walk applies to crossed filesystems.
        #[test]
        fn uncreatable_temp_dir_degrades_to_disabled() {
            let tmp = TempDir::new().unwrap();
            let file = tmp.path().join("not-a-dir");
            std::fs::write(&file, b"x").unwrap();
            let root = tmp.path().join("root");
            std::fs::create_dir(&root).unwrap();
            let meta = std::fs::metadata(&root).unwrap();

            let mut a = base_args();
            a.calibration_path = Some(file);
            let shutdown = Arc::new(AtomicBool::new(false));

            let cal =
                resolve_calibration(&root, &meta, &shutdown, &a).unwrap();
            assert_eq!(cal, Calibration::DISABLED);
        }

        /// A `-t` directory that can't be read at all is bad user input and
        /// stays fatal rather than silently degrading.
        #[test]
        fn unreadable_t_dir_is_fatal() {
            let tmp = TempDir::new().unwrap();
            let root = tmp.path().join("root");
            std::fs::create_dir(&root).unwrap();
            let meta = std::fs::metadata(&root).unwrap();

            let mut a = base_args();
            a.calibration_path = Some(tmp.path().join("missing"));
            let shutdown = Arc::new(AtomicBool::new(false));

            assert!(resolve_calibration(&root, &meta, &shutdown, &a).is_err());
        }

        /// The `-i` fixed ratio short-circuits before any filesystem touch.
        #[test]
        fn fixed_ratio_short_circuits() {
            let tmp = TempDir::new().unwrap();
            let meta = std::fs::metadata(tmp.path()).unwrap();
            let mut a = base_args();
            a.size_inode_ratio = 33;
            let shutdown = Arc::new(AtomicBool::new(false));

            let cal =
                resolve_calibration(tmp.path(), &meta, &shutdown, &a).unwrap();
            assert_eq!(cal, Calibration { per_entry: 33, overhead: 0 });
            assert_eq!(
                std::fs::read_dir(tmp.path()).unwrap().count(),
                0,
                "fixed ratio must not create calibration files"
            );
        }
    }
}
