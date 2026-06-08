// SPDX-FileCopyrightText: 2022 Dinko Korunic <dinko.korunic@gmail.com>
// SPDX-License-Identifier: MIT

//! End-to-end `findlargedir` vs GNU `find` over a Linux kernel tree, mirroring
//! the sibling `minifind` benchmark and <https://github.com/dkorunic/bench_walk>.
//!
//! Both run as subprocesses so the comparison is fair (each pays process
//! startup). `findlargedir` runs with defaults, so every iteration calibrates
//! (creating/cleaning temp files in the tree) and then walks — the tool's true
//! end-to-end cost. `find` uses the size filter that approximates the same job.
//!
//! Two groups run: `walk_linux_kernel` (warm cache) and
//! `walk_linux_kernel_cold`, which frees the page/dentry/inode cache before
//! every traversal via `/proc/sys/vm/drop_caches` so each run pays real disk
//! I/O — the regime findlargedir is built for. The cold group needs root on
//! Linux; it is skipped with a warning when the drop is not permitted.
//!
//! The corpus is shallow-cloned once into `benches/linux_root`; set
//! `BENCH_WALK_DIR` to reuse a checkout (CI/offline). Heavy by design (several
//! minutes); shorten with `cargo bench --bench walk -- --measurement-time 20`.

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};

const TEST_DIR: &str = "benches/linux_root";
const WARMUP_TIME: u64 = 80;
const MEASURE_TIME: u64 = 400;

/// Returns the corpus path, shallow-cloning the kernel on first use unless
/// `BENCH_WALK_DIR` points at an existing checkout.
fn prepare_test_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("BENCH_WALK_DIR") {
        return PathBuf::from(dir);
    }

    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join(TEST_DIR);
    if !target.exists() {
        eprintln!("Cloning Linux kernel into {} ...", target.display());
        let status = Command::new("git")
            .args([
                "clone",
                "https://github.com/torvalds/linux.git",
                "--depth",
                "1",
            ])
            .arg(&target)
            .status()
            .expect("failed to spawn git; is it installed?");
        assert!(status.success(), "git clone of the Linux kernel failed");
        let _ = Command::new("sync").status();
        eprintln!("Clone complete.");
    }

    target
}

/// Scans `root` with the compiled `findlargedir` binary (defaults: calibrates
/// then walks), discarding output.
fn findlargedir_walk(root: &Path) {
    Command::new(env!("CARGO_BIN_EXE_findlargedir"))
        .arg(root)
        .output()
        .expect("failed to spawn findlargedir");
}

/// Scans `root` with system GNU `find`, flagging large directories by inode
/// size — the functional analogue of findlargedir — and discarding output.
fn find_walk(root: &Path) {
    Command::new("find")
        .arg(root)
        .args(["-xdev", "-type", "d", "-size", "+200000c"])
        .output()
        .expect("failed to spawn find");
}

/// Frees page cache, dentries and inodes so the next traversal reads cold from
/// disk. Runs `sync` first so dirty pages become reclaimable. Returns whether
/// the drop succeeded — writing `/proc/sys/vm/drop_caches` needs root on Linux
/// — so callers can skip cold benches rather than mislabel warm runs as cold.
fn drop_caches() -> bool {
    let _ = Command::new("sync").status();
    std::fs::write("/proc/sys/vm/drop_caches", b"3").is_ok()
}

/// Times `iters` cold runs of `walk`, dropping caches *outside* the timed
/// region before each so only the traversal itself is measured.
fn cold_iter(iters: u64, work_dir: &Path, walk: fn(&Path)) -> Duration {
    let mut total = Duration::ZERO;
    for _ in 0..iters {
        drop_caches();
        let start = Instant::now();
        walk(black_box(work_dir));
        total += start.elapsed();
    }
    total
}

fn bench_walk(c: &mut Criterion) {
    let work_dir = prepare_test_dir();

    let mut g = c.benchmark_group("walk_linux_kernel");
    g.bench_function("findlargedir", |b| {
        b.iter(|| findlargedir_walk(black_box(&work_dir)));
    });
    g.bench_function("find", |b| {
        b.iter(|| find_walk(black_box(&work_dir)));
    });
    g.finish();
}

/// Cold-cache counterpart to [`bench_walk`]: the cache is dropped before every
/// traversal, so each run pays real disk I/O. Uses `iter_custom` to keep the
/// drop out of the measurement, plus a small sample count and short warm-up,
/// since cold runs are slow and system-wide disruptive. Skipped (with a
/// warning) when caches cannot be dropped.
fn bench_walk_cold(c: &mut Criterion) {
    if !drop_caches() {
        eprintln!(
            "walk_linux_kernel_cold: cannot write /proc/sys/vm/drop_caches \
             (needs root on Linux); skipping cold-cache benches."
        );
        return;
    }

    let work_dir = prepare_test_dir();

    let mut g = c.benchmark_group("walk_linux_kernel_cold");
    g.sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(60));
    g.bench_function("findlargedir", |b| {
        b.iter_custom(|iters| cold_iter(iters, &work_dir, findlargedir_walk));
    });
    g.bench_function("find", |b| {
        b.iter_custom(|iters| cold_iter(iters, &work_dir, find_walk));
    });
    g.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(WARMUP_TIME))
        .measurement_time(Duration::from_secs(MEASURE_TIME));
    targets = bench_walk, bench_walk_cold
}
criterion_main!(benches);
