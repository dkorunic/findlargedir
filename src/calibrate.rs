// SPDX-FileCopyrightText: 2022 Dinko Korunic <dinko.korunic@gmail.com>
//
// SPDX-License-Identifier: MIT

use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Error};
use fs_err as fs;
use rayon::prelude::*;

use crate::{args, progress};

/// Default `-c` batch size; the effective batch is at least [`MIN_BATCH`].
pub const DEFAULT_TEST_COUNT: u64 = 100;

/// Minimum files created per calibration batch (a floor on `-c`).
const MIN_BATCH: u64 = 1000;
/// Stop calibrating once the directory size has grown this many times.
const STEP_TARGET: usize = 3;
/// Hard ceiling on files created during calibration (degenerate-fs guard).
const FILE_CAP: u64 = 50_000;

/// Per-entry cost and fixed overhead of a filesystem's directory inodes,
/// derived during calibration and consumed by the walk's entry-count estimate.
/// `per_entry == 0` is the sentinel for "flagging disabled" (degenerate
/// filesystem or interrupted calibration).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Calibration {
    /// Marginal bytes per directory entry.
    pub per_entry: u64,
    /// Fixed directory overhead in bytes, subtracted before dividing.
    pub overhead: u64,
}

/// Fits `size ≈ overhead + per_entry·n` over `(n_files, dir_size)` samples by
/// ordinary least squares. Returns the rounded slope as `per_entry` and the
/// clamped intercept as `overhead`. A slope at or below 0.5 byte/entry (or
/// fewer than two samples) means the filesystem exposes no usable per-entry
/// growth → `per_entry: 0`, which disables flagging.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn fit_calibration(points: &[(u64, u64)]) -> Calibration {
    let n = points.len() as f64;
    if n < 2.0 {
        return Calibration { per_entry: 0, overhead: 0 };
    }

    let mean_x = points.iter().map(|&(x, _)| x as f64).sum::<f64>() / n;
    let mean_y = points.iter().map(|&(_, y)| y as f64).sum::<f64>() / n;

    let mut num = 0.0;
    let mut den = 0.0;
    for &(x, y) in points {
        let dx = x as f64 - mean_x;
        num += dx * (y as f64 - mean_y);
        den += dx * dx;
    }
    if den == 0.0 {
        return Calibration { per_entry: 0, overhead: 0 };
    }

    let slope = num / den;
    if slope <= 0.5 {
        return Calibration { per_entry: 0, overhead: 0 };
    }

    let intercept = mean_y - slope * mean_x;
    Calibration {
        per_entry: slope.round() as u64,
        overhead: intercept.round().max(0.0) as u64,
    }
}

/// Derives a filesystem's directory `Calibration` (marginal bytes per entry +
/// fixed overhead) by creating files in `test_path` in batches, re-`stat`ing the
/// directory after each batch, and least-squares-fitting the resulting
/// `(files, size)` samples (see [`fit_calibration`]). This lets the walk
/// estimate entry counts from a single `stat` instead of an expensive `readdir`.
///
/// Returns `Calibration { per_entry: 0, overhead: 0 }` if interrupted
/// mid-calibration or if the filesystem exposes no per-entry growth, which the
/// caller treats as "flagging disabled" rather than a real ratio.
///
/// # Errors
/// Fails if the thread pool cannot be built, a file cannot be created, or the
/// directory metadata cannot be read.
pub fn get_inode_ratio(
    test_path: &Path,
    shutdown: &Arc<AtomicBool>,
    args: &args::Args,
) -> Result<Calibration, Error> {
    println!("Starting test directory calibration in {}", test_path.display());

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(args.threads)
        .build()
        .context("Unable to spawn calibration thread pool")?;

    let pb = progress::new_spinner("Creating test files in progress...");

    // `-c` is a per-batch floor, not an exact target.
    let batch = args.calibration_count.max(MIN_BATCH);

    // The empty-directory baseline anchors the regression's overhead term.
    let mut points: Vec<(u64, u64)> = vec![(
        0,
        fs::metadata(test_path)
            .context("Unable to retrieve calibration directory metadata")?
            .size(),
    )];
    let mut created: u64 = 0;
    let mut increases: usize = 0;

    // Short filenames keep per-entry cost minimal; the running offset keeps
    // names unique across batches. Stop once the directory has grown enough
    // times for a stable slope, or the safety cap is hit.
    let res: Result<(), Error> = pool.install(|| {
        loop {
            if shutdown.load(Ordering::Relaxed) {
                return Err(anyhow::anyhow!("shutdown requested"));
            }

            let start = created;
            (start..start + batch).into_par_iter().try_for_each(|i| {
                if shutdown.load(Ordering::Relaxed) {
                    return Err(anyhow::anyhow!("shutdown requested"));
                }
                File::create(test_path.join(i.to_string()))
                    .context("Unable to create test file")?;
                Ok(())
            })?;
            created += batch;

            let size = fs::metadata(test_path)
                .context("Unable to retrieve calibration directory metadata")?
                .size();
            if size > points.last().map_or(0, |&(_, s)| s) {
                increases += 1;
            }
            points.push((created, size));

            if increases >= STEP_TARGET || created >= FILE_CAP {
                return Ok(());
            }
        }
    });

    pb.finish_with_message("Done.");

    // Propagate real errors; ignore the sentinel emitted on shutdown.
    if let Err(e) = res
        && !shutdown.load(Ordering::Relaxed)
    {
        return Err(e);
    }
    // Caller's TempDir cleans itself up on drop, so bailing out here is safe.
    if shutdown.load(Ordering::Relaxed) {
        return Ok(Calibration { per_entry: 0, overhead: 0 });
    }

    let cal = fit_calibration(&points);
    if cal.per_entry == 0 {
        println!(
            "Warning: filesystem does not expose per-entry directory growth; \
             size-based flagging disabled for this path."
        );
    } else {
        println!(
            "Calibration done. Bytes per entry: {}, fixed overhead: {} bytes",
            cal.per_entry, cal.overhead
        );
    }

    Ok(cal)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use tempfile::TempDir;

    use super::{
        Calibration, FILE_CAP, MIN_BATCH, fit_calibration, get_inode_ratio,
    };
    use crate::args::Args;

    fn make_args(calibration_count: u64) -> Arc<Args> {
        Arc::new(Args {
            calibration_count,
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
        })
    }

    mod get_inode_ratio {
        use super::*;

        /// A calibration run cut short by a shutdown signal disables flagging
        /// (`per_entry == 0`) rather than erroring or guessing a ratio.
        #[test]
        fn returns_zero_on_shutdown() {
            let tmp = TempDir::new().unwrap();
            let shutdown = Arc::new(AtomicBool::new(false));
            // Signal shutdown before the function even begins its loop.
            shutdown.store(true, Ordering::Relaxed);

            let result =
                get_inode_ratio(tmp.path(), &shutdown, &make_args(100));

            assert_eq!(
                result.unwrap(),
                Calibration { per_entry: 0, overhead: 0 }
            );
        }

        /// Sanity: calibration completes without error when no shutdown
        /// signal is set.
        #[test]
        fn completes_without_error() {
            let tmp = TempDir::new().unwrap();
            let shutdown = Arc::new(AtomicBool::new(false));

            let result =
                get_inode_ratio(tmp.path(), &shutdown, &make_args(10));

            assert!(
                result.is_ok(),
                "calibration should succeed when not interrupted"
            );
        }

        /// A small `calibration_count` is raised to the batch floor, so
        /// calibration still runs without panicking.
        #[test]
        fn small_calibration_count_uses_batch_floor() {
            let tmp = TempDir::new().unwrap();
            let shutdown = Arc::new(AtomicBool::new(false));

            let result = get_inode_ratio(tmp.path(), &shutdown, &make_args(1));

            assert!(result.is_ok(), "calibration_count=1 must not panic");
        }

        /// Calibration creates files in batches (a floor of `MIN_BATCH`) until
        /// the directory grows enough or the cap is hit — so the file count is
        /// a positive multiple of the batch, never exceeding `FILE_CAP`.
        #[test]
        fn creates_in_batches_within_cap() {
            let tmp = TempDir::new().unwrap();
            let shutdown = Arc::new(AtomicBool::new(false));

            get_inode_ratio(tmp.path(), &shutdown, &make_args(100)).unwrap();

            let created =
                std::fs::read_dir(tmp.path()).unwrap().count() as u64;
            assert!(
                created >= MIN_BATCH,
                "at least one full batch ({MIN_BATCH}) is created"
            );
            assert!(
                created <= FILE_CAP,
                "file creation never exceeds the safety cap"
            );
        }
    }

    mod fit_calibration {
        use super::*;

        /// A perfectly linear sample set recovers the exact slope (`per_entry`)
        /// and intercept (`overhead`).
        #[test]
        fn clean_line_recovers_slope_and_intercept() {
            let pts: Vec<(u64, u64)> = (0..=4u64)
                .map(|k| (k * 1000, 4096 + 12 * (k * 1000)))
                .collect();
            let cal = fit_calibration(&pts);
            assert_eq!(cal, Calibration { per_entry: 12, overhead: 4096 });
        }

        /// A directory whose size never changes is degenerate → `per_entry` 0.
        #[test]
        fn flat_points_are_degenerate() {
            let pts = [(0u64, 4096u64), (1000, 4096), (2000, 4096)];
            assert_eq!(
                fit_calibration(&pts),
                Calibration { per_entry: 0, overhead: 0 }
            );
        }

        /// Two points are the minimum viable fit.
        #[test]
        fn two_points_minimum_fit() {
            let pts = [(0u64, 100u64), (10, 200)];
            assert_eq!(
                fit_calibration(&pts),
                Calibration { per_entry: 10, overhead: 100 }
            );
        }

        /// A slope at or below 0.5 byte/entry is treated as degenerate.
        #[test]
        fn sub_half_slope_is_degenerate() {
            let pts = [(0u64, 0u64), (10, 4)]; // slope 0.4
            assert_eq!(fit_calibration(&pts).per_entry, 0);
        }

        /// A line extrapolating to a negative intercept clamps overhead to 0.
        #[test]
        fn negative_intercept_clamped_to_zero() {
            let pts = [(10u64, 50u64), (20, 150), (30, 250)]; // y = 10x - 50
            assert_eq!(
                fit_calibration(&pts),
                Calibration { per_entry: 10, overhead: 0 }
            );
        }

        /// Fewer than two points cannot define a line → degenerate.
        #[test]
        fn fewer_than_two_points_are_degenerate() {
            assert_eq!(
                fit_calibration(&[(0, 4096)]),
                Calibration { per_entry: 0, overhead: 0 }
            );
            assert_eq!(
                fit_calibration(&[]),
                Calibration { per_entry: 0, overhead: 0 }
            );
        }
    }
}
