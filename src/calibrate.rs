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

use crate::args;

/// Default `-c` batch size; the effective batch is at least [`MIN_BATCH`].
pub const DEFAULT_TEST_COUNT: u64 = 100;

/// Default calibration filename length (`-n`), a rough stand-in for real entry
/// names. Minimal names measure only the per-entry cost floor and bias the
/// walk's estimate high; this leans toward typical lengths (configurable).
pub const DEFAULT_NAME_LEN: usize = 24;

/// Minimum files created per calibration batch (a floor on `-c`).
const MIN_BATCH: u64 = 1000;
/// Assumed live fill of a real directory's htree leaves. Sequential calibration
/// packs leaves nearly full; churned ones settle lower, so the raw slope
/// under-measures real per-entry cost — dividing by this scales it up.
const FILL_FACTOR: f64 = 0.75;
/// Hard ceiling on files created during calibration (degenerate-fs guard).
#[cfg(not(test))]
const FILE_CAP: u64 = 50_000;
/// Lower under test to keep the file-creation-heavy calibration tests fast; the
/// sampling and fit logic is identical at any cap.
#[cfg(test)]
const FILE_CAP: u64 = 5_000;

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

/// Least-squares-fits `size ≈ overhead + per_entry·n` over the large-N half of
/// the `(n_files, dir_size)` samples. The ratio is extrapolated onto
/// million-entry directories, so only the asymptotic slope matters; a global fit
/// would let the cheap first blocks (ext4 htree linear→hashed transition,
/// block-size rounding) skew it. A slope ≤ 0.5 byte/entry, or fewer than two
/// usable samples, yields `per_entry: 0` (flagging disabled).
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn fit_calibration(points: &[(u64, u64)]) -> Calibration {
    // Upper half by index — geometric sampling puts the largest N there. Fall
    // back to all points when that leaves fewer than two.
    let start = points.len() / 2;
    let window = match points.get(start..) {
        Some(w) if w.len() >= 2 => w,
        _ => points,
    };

    let n = window.len() as f64;
    if n < 2.0 {
        return Calibration { per_entry: 0, overhead: 0 };
    }

    let mean_x = window.iter().map(|&(x, _)| x as f64).sum::<f64>() / n;
    let mean_y = window.iter().map(|&(_, y)| y as f64).sum::<f64>() / n;

    let mut num = 0.0;
    let mut den = 0.0;
    for &(x, y) in window {
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

/// Scales the densely-packed calibration slope up to real directories (see
/// [`FILL_FACTOR`]). Preserves the `per_entry == 0` flagging-disabled sentinel.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn fill_corrected(cal: Calibration) -> Calibration {
    if cal.per_entry == 0 {
        return cal;
    }
    Calibration {
        per_entry: (cal.per_entry as f64 / FILL_FACTOR).round() as u64,
        overhead: cal.overhead,
    }
}

/// Whether `path` is on a read-only mount, where calibration — which must
/// create files — cannot run. Probes the mount flags rather than attempting a
/// write, so the skip reads as intent ("read-only") not a failed write. A
/// filesystem that can't be queried is assumed writable, letting calibration try
/// and surface any real error.
#[cfg(unix)]
pub fn is_read_only(path: &Path) -> bool {
    rustix::fs::statvfs(path).is_ok_and(|s| {
        s.f_flag.contains(rustix::fs::StatVfsMountFlags::RDONLY)
    })
}

#[cfg(not(unix))]
pub fn is_read_only(_path: &Path) -> bool {
    false
}

/// Human name for a statfs filesystem magic number; the raw hex for anything
/// not in the table. Kept pure (separate from the `statfs` call) so the mapping
/// is testable without a real mount.
fn fs_type_from_magic(magic: u64) -> String {
    let name = match magic {
        0xEF53 => "ext2/3/4",
        0x5846_5342 => "xfs",
        0x9123_683E => "btrfs",
        0x2FC1_2FC1 => "zfs",
        0xF2F5_2010 => "f2fs",
        0xCA45_1A4E => "bcachefs",
        0x0102_1994 => "tmpfs",
        0x8584_58F6 => "ramfs",
        0x6969 => "nfs",
        0xFF53_4D42 => "cifs",
        0xFE53_4D42 => "smb2",
        0x794C_7630 => "overlayfs",
        0x6573_5546 => "fuse",
        0x7371_7368 => "squashfs",
        0x5265_4973 => "reiserfs",
        0x3153_464A => "jfs",
        0x0116_1970 => "gfs2",
        0x7461_636F => "ocfs2",
        0x00C3_6400 => "ceph",
        0x4D44 => "vfat",
        0x5346_544E => "ntfs",
        0x2011_BAB0 => "exfat",
        0x9FA0 => "proc",
        0x6265_6572 => "sysfs",
        0x6367_7270 => "cgroup2",
        _ => return format!("unknown ({magic:#x})"),
    };
    name.to_owned()
}

/// Filesystem type holding `path`, derived from its statfs magic (see
/// [`fs_type_from_magic`]). `"unknown"` if the filesystem cannot be queried.
#[cfg(unix)]
#[allow(clippy::cast_sign_loss)]
pub fn fs_type_name(path: &Path) -> String {
    rustix::fs::statfs(path).map_or_else(
        |_| "unknown".to_owned(),
        |st| fs_type_from_magic(st.f_type as u64),
    )
}

#[cfg(not(unix))]
pub fn fs_type_name(_path: &Path) -> String {
    "unknown".to_owned()
}

/// Derives the target filesystem's directory `Calibration` so the walk can
/// estimate entry counts from a single `stat` instead of an expensive `readdir`.
/// Creates files in `test_path` on the fixed geometric schedule up to
/// [`FILE_CAP`], then fits the samples. Files are created in order (not in
/// parallel) so the directory's on-disk layout — and hence the fitted ratio — is
/// reproducible across runs. `mount` is the filesystem location being calibrated
/// for (the scan root or a crossed mount point); it is used only in the start
/// message, while `test_path` is the temp dir the files actually go into.
///
/// Returns `per_entry: 0` (flagging disabled) if interrupted mid-calibration or
/// if the filesystem exposes no per-entry growth.
///
/// # Errors
/// Fails if a file cannot be created or the directory metadata cannot be read.
pub fn get_inode_ratio(
    test_path: &Path,
    mount: &Path,
    shutdown: &Arc<AtomicBool>,
    args: &args::Args,
) -> Result<Calibration, Error> {
    println!(
        "Starting test directory calibration for mount {}, filesystem {}",
        mount.display(),
        fs_type_name(mount)
    );

    // `-c` floors the initial batch; doubling it each round (geometric sampling)
    // spreads samples across the large-N range the ratio is extrapolated onto.
    let mut batch = args.calibration_count.max(MIN_BATCH);

    // Padded names (see `DEFAULT_NAME_LEN`); the full index is always present,
    // so they stay unique past the pad.
    let name_len = args.calibration_name_length;

    // The empty-directory baseline anchors the regression's overhead term.
    let mut points: Vec<(u64, u64)> = vec![(
        0,
        fs::metadata(test_path)
            .context("Unable to retrieve calibration directory metadata")?
            .size(),
    )];
    let mut created: u64 = 0;

    let res: Result<(), Error> = (|| {
        loop {
            if shutdown.load(Ordering::Relaxed) {
                return Err(anyhow::anyhow!("shutdown requested"));
            }

            // Clamp the doubling batch so it never overshoots the cap.
            let this_batch = batch.min(FILE_CAP - created);
            let start = created;
            for i in start..start + this_batch {
                if shutdown.load(Ordering::Relaxed) {
                    return Err(anyhow::anyhow!("shutdown requested"));
                }
                File::create(test_path.join(format!("{i:0name_len$}")))
                    .context("Unable to create test file")?;
            }
            created += this_batch;

            let size = fs::metadata(test_path)
                .context("Unable to retrieve calibration directory metadata")?
                .size();
            points.push((created, size));

            if created >= FILE_CAP {
                return Ok(());
            }

            batch = batch.saturating_mul(2);
        }
    })();

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

    let cal = fill_corrected(fit_calibration(&points));
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

    use super::{Calibration, FILE_CAP, fit_calibration, get_inode_ratio};
    use crate::args::Args;

    fn make_args(calibration_count: u64) -> Arc<Args> {
        Arc::new(Args {
            calibration_count,
            calibration_name_length: super::DEFAULT_NAME_LEN,
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

            let result = get_inode_ratio(
                tmp.path(),
                tmp.path(),
                &shutdown,
                &make_args(100),
            );

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

            let result = get_inode_ratio(
                tmp.path(),
                tmp.path(),
                &shutdown,
                &make_args(10),
            );

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

            let result = get_inode_ratio(
                tmp.path(),
                tmp.path(),
                &shutdown,
                &make_args(1),
            );

            assert!(result.is_ok(), "calibration_count=1 must not panic");
        }

        /// Calibration samples the fixed schedule all the way to `FILE_CAP`
        /// every run — no variable early stop. Sampling the same span each time
        /// is what makes the fitted ratio reproducible across runs.
        #[test]
        fn samples_to_file_cap() {
            let tmp = TempDir::new().unwrap();
            let shutdown = Arc::new(AtomicBool::new(false));

            get_inode_ratio(
                tmp.path(),
                tmp.path(),
                &shutdown,
                &make_args(100),
            )
            .unwrap();

            let created =
                std::fs::read_dir(tmp.path()).unwrap().count() as u64;
            assert_eq!(
                created, FILE_CAP,
                "calibration must sample the full schedule to the cap"
            );
        }

        /// Calibration pads filenames to `calibration_name_length` so per-entry
        /// cost is measured against representative entries.
        #[test]
        fn pads_filenames_to_configured_length() {
            let tmp = TempDir::new().unwrap();
            let shutdown = Arc::new(AtomicBool::new(false));
            let mut args = (*make_args(100)).clone();
            args.calibration_name_length = 24;

            get_inode_ratio(tmp.path(), tmp.path(), &shutdown, &args).unwrap();

            let mut saw_file = false;
            for entry in std::fs::read_dir(tmp.path()).unwrap() {
                let name = entry.unwrap().file_name();
                assert_eq!(
                    name.to_string_lossy().len(),
                    24,
                    "calibration filename {name:?} must be padded to 24 chars"
                );
                saw_file = true;
            }
            assert!(saw_file, "calibration must create at least one file");
        }
    }

    mod is_read_only {
        use tempfile::TempDir;

        use super::super::is_read_only;

        /// A normal writable temp dir is not read-only — guards against the
        /// probe misfiring and disabling calibration everywhere. (The read-only
        /// case needs a privileged mount and is exercised in manual validation.)
        #[test]
        fn writable_dir_is_not_read_only() {
            let tmp = TempDir::new().unwrap();
            assert!(!is_read_only(tmp.path()));
        }
    }

    mod fill_corrected {
        use super::super::{Calibration, fill_corrected};

        /// The raw slope is scaled up by `1/FILL_FACTOR` (0.75 → ×1.33):
        /// 33 → 44. Overhead is a fixed term and left untouched.
        #[test]
        fn scales_per_entry_up_leaving_overhead() {
            assert_eq!(
                fill_corrected(Calibration { per_entry: 33, overhead: 100 }),
                Calibration { per_entry: 44, overhead: 100 }
            );
        }

        /// The flagging-disabled sentinel must survive the correction unchanged
        /// rather than being scaled into a spurious non-zero ratio.
        #[test]
        fn preserves_disabled_sentinel() {
            assert_eq!(
                fill_corrected(Calibration { per_entry: 0, overhead: 0 }),
                Calibration { per_entry: 0, overhead: 0 }
            );
        }
    }

    mod fs_type_from_magic {
        use super::super::fs_type_from_magic;

        /// A known magic maps to its filesystem name.
        #[test]
        fn known_magic_maps_to_name() {
            assert_eq!(fs_type_from_magic(0xEF53), "ext2/3/4");
            assert_eq!(fs_type_from_magic(0x0102_1994), "tmpfs");
        }

        /// An unrecognized magic falls back to its raw hex value.
        #[test]
        fn unknown_magic_falls_back_to_hex() {
            assert_eq!(fs_type_from_magic(0xDEAD), "unknown (0xdead)");
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

        /// The fit tracks the large-N (asymptotic) regime, not a global average
        /// the cheap small-N samples would skew — that regime governs the
        /// million-entry directories the ratio is extrapolated onto.
        #[test]
        fn large_n_regime_sets_slope_and_overhead() {
            let pts = [
                (0u64, 1_000u64),
                (1_000, 3_000),
                (2_000, 5_000),
                (3_000, 7_000), // small-N slope 2
                (4_000, 85_000),
                (5_000, 105_000),
                (6_000, 125_000),
                (7_000, 145_000), // large-N slope 20, intercept 5000
            ];
            assert_eq!(
                fit_calibration(&pts),
                Calibration { per_entry: 20, overhead: 5_000 }
            );
        }

        /// Degeneracy is judged on the large-N regime: small-N growth that
        /// plateaus later exposes no usable per-entry signal where it matters.
        #[test]
        fn flat_large_n_is_degenerate() {
            let pts = [
                (0u64, 1000u64),
                (1000, 5000),
                (2000, 9000), // grows early
                (3000, 9000),
                (4000, 9000),
                (5000, 9000), // plateaus at large N
            ];
            assert_eq!(fit_calibration(&pts).per_entry, 0);
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
