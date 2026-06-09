# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
# Build
cargo build
cargo build --release

# Run all tests
cargo test

# Run a single test (tests are nested: mod tests > mod <fn-under-test>)
cargo test get_inode_ratio::returns_zero_on_shutdown
cargo test parallel_search::skip_path_skips_only_listed_dirs

# Lint (lint levels live in Cargo.toml [lints]; the flag is belt-and-suspenders)
cargo clippy -- -D warnings

# Format (rustfmt.toml enforces max_width=79)
cargo fmt

# Check formatting without modifying
cargo fmt -- --check

# Benchmark findlargedir vs GNU find (Criterion, harness = false)
# Heavy: shallow-clones the Linux kernel into benches/linux_root on first run.
# Set BENCH_WALK_DIR to reuse a checkout; shorten a run with --measurement-time.
# Two groups: walk_linux_kernel (warm) and walk_linux_kernel_cold (drops caches
# via /proc/sys/vm/drop_caches each run — needs root, else skipped with a warning).
cargo bench --bench walk
cargo bench --bench walk -- --measurement-time 20
cargo bench --bench walk -- walk_linux_kernel_cold   # cold-cache group only (root)
```

`edition = "2024"`, `rust-version = "1.88.0"` (MSRV). There is **no** `rust-toolchain.toml` — the toolchain is not pinned, so pin it manually if building elsewhere. Lint levels are centralized in `Cargo.toml`'s `[lints]` table (`clippy::all = deny`, `clippy::pedantic = warn`, `clippy::redundant_clone = deny`, `nonstandard_style = deny`).

A sibling `AGENTS.md` carries the same guidance in condensed form; keep the two in sync when editing either.

## Architecture

`findlargedir` is a single-binary CLI tool that scans filesystems for "black hole" directories — directories with an extremely large number of entries that cause performance problems. It avoids doing expensive full `readdir` passes by using inode-size heuristics.

### Two-phase operation

**Phase 1 — Calibration (`src/calibrate.rs`)**
Creates empty files on the target filesystem in **geometrically growing** batches — the first batch is a floor of 1 000 (`-c` raises it) and each subsequent batch doubles — re-`stat`ing the temp directory after each batch, always sampling the full fixed schedule up to a 50 000-file cap (`FILE_CAP`). Geometric spacing makes the samples span the large-N range the ratio is later extrapolated onto, instead of clustering at low N. Two choices make the result **reproducible across runs** (an earlier adaptive early-stop and parallel creation made successive calibrations disagree): the schedule is fixed (no data-dependent early stop, which varied which regime got fit), and files are created **in order, not in parallel** (parallel insertion order jittered the htree layout, hence the per-`N` size, by a few percent). Files are named zero-padded to `calibration_name_length` (`-n`, default 24) so per-entry cost reflects representative entries rather than the minimal-name floor (which biased estimates high → false positives). A least-squares fit (`fit_calibration`) **over the upper-N half of the samples** gives the **asymptotic marginal** bytes-per-entry (slope) and **fixed overhead** (intercept); `fill_corrected` then divides the slope by `FILL_FACTOR` (0.75) because sequential calibration packs htree leaves tighter than real churned directories, which under-measures per-entry cost. The result is a `Calibration`. Fitting only the large-N window keeps the cheap first blocks (htree linear→hashed transition, block-size rounding) from skewing the slope used for million-entry directories. A filesystem whose large-N directory size never grows is detected (slope ≤ 0.5) and reported, with flagging disabled (`per_entry = 0`, the same sentinel as a shutdown mid-calibration). Calibration can be skipped with `-i <ratio>` (per-entry only, overhead 0) or pointed at a custom dir with `-t`. `classify_dir` guards against the zero-`per_entry` divide.

**Phase 2 — Parallel walk (`src/walk.rs`)**
Uses a custom `crossbeam-deque` work-stealing engine (`src/walk/engine.rs`, adapted from the sibling `minifind` project) to walk the filesystem in parallel, visiting directories only; a dedicated thread prints periodic progress (`-p`). The size estimate is split into two roles:

- **Skip decision (estimate, pre-read), in `classify_dir`:** a subtree whose estimated `(dir_size − overhead) / per_entry` exceeds `blacklist_threshold` (default 100 000) is **skipped** unread (`Decision::Skip`) and reported red from the estimate (labelled a *size-based upper bound* — a directory's `st_size` is its high-water mark and never shrinks, so it overstates the live count for churned dirs). This is the only path that relies on the estimate alone, because reading a true black hole is the cost we refuse to pay (`getdents` on a multi-million-entry dir can wedge in uninterruptible `D` state).
- **Reporting (exact, post-read), in `report_dir`:** every descended directory is reported on its **exact** live entry count, harvested for free from the `getdents` enumeration the walk already performs to find subdirectories (the leaf `for_each_entry` returns the count). `offender_tier` flags it yellow above `alert_threshold` (default 10 000) or red above `blacklist_threshold`, silent below. Judging the alert tier on truth, not the estimate, is what stops size-inflated directories from raising false alarms.

`main.rs` bails at startup if `alert_threshold >= blacklist_threshold`.

Accurate mode (`-a`) only matters for the skipped (blacklisted) tier: it does an explicit `std::fs::read_dir().count()` the caller opts into despite the cost. Descended dirs are exact regardless.

### Module layout

| File | Responsibility |
|---|---|
| `src/main.rs` | CLI entry point; loops over paths, orchestrates calibration + walk |
| `src/args.rs` | Clap-derive argument definitions and path/thread validation |
| `src/calibrate.rs` | Adaptive batch calibration + `fit_calibration` regression; returns `Calibration` |
| `src/walk.rs` | `parallel_search` policy: `classify_dir` (estimate skip decision) + `report_dir`/`offender_tier` (exact reporting) + `print_offender` |
| `src/walk/engine.rs` | `crossbeam-deque` work-stealing scheduler; `walk_dirs` (`classify` + `report` callbacks) + `Decision`/`DirInfo` |
| `src/walk/unix.rs` | Unix leaf I/O via `rustix` (statat/getdents); `for_each_entry` returns the live entry count |
| `src/walk/fallback.rs` | Non-Unix leaf I/O via `std::fs` |
| `src/interrupt.rs` | SIGINT/SIGTERM/SIGQUIT handler via `signal_hook` |
| `src/progress.rs` | `indicatif` spinner helper |

### Key design decisions

- **`mimalloc`** is set as `#[global_allocator]` for allocation performance.
- **`ahash::AHashSet`** is used for the skip-path and visited-path sets (non-cryptographic, fast).
- **`fs-err`** wraps `std::fs` to add path context to IO errors automatically.
- Shutdown is coordinated via a shared `Arc<AtomicBool>` checked at each walk step and in calibration loops.
- One-filesystem mode (`-o`, default on) uses `MetadataExt::dev()` comparisons to detect mount boundaries.
- **Per-filesystem calibration** (`walk.rs`'s `CalContext`): the scan-root fs uses the up-front calibration (no locking, the common path); when `-o` is off and the walk crosses into another filesystem, that fs is calibrated *in place* on first encounter and cached by device id (behind a `Mutex<AHashMap<dev, Calibration>>`). Different filesystems have very different per-entry geometry (e.g. ext4 ~57 B/entry + multi-KiB overhead vs tmpfs ~27 B/entry + tiny overhead), so reusing the root's ratio everywhere mis-estimates foreign dirs. Resolution happens *after* the boundary check, so a skipped foreign fs is never written to. The scan progress spinner (created and owned by `parallel_search`, not `main`) is paused via `ProgressBar::suspend` while a crossed filesystem calibrates, so the calibration's own messages and spinner render cleanly. A **read-only** filesystem is detected up front (`calibrate::is_read_only`, via `statvfs` mount flags) and calibration is skipped rather than attempted-and-failed; an unwritable fs or failed calibration likewise disables flagging for it (`per_entry = 0`). The same read-only guard applies to the scan-root calibration in `main.rs`, so a read-only root no longer aborts the run. Note this only affects the skip decision/upper-bound estimate — descended dirs are reported on exact counts regardless of fs.

### Performance profile (I/O-bound, not CPU-bound)

The tool's runtime is dominated by **filesystem I/O** — one `stat` per directory plus `readdir`/`getdents` traversal — not by its own computation. Profiling a 183 k-directory traversal confirmed this (measured with the previous `ignore`-based walker; the conclusion still holds for the current engine):
- **Cold cache:** ~95 % of wall time blocked on disk (≈4 % CPU); our own code is ~0.5 % of wall time.
- **Warm cache (no disk waits):** still ~77 % kernel syscall handling vs ~17 % in our binary, and that 17 % is mostly walker path bookkeeping + allocator/`Arc` churn. The core heuristic (the `(size − overhead) / per_entry` division, threshold checks, `AHashSet` lookup, `AtomicU64` increment) is below the profiler's noise floor.

**Implication for changes:** micro-optimizing the per-directory arithmetic or hot path (branchless tricks, SoA, lock-free sharding, manual inlining) buys nothing measurable here — there is no CPU time there to reclaim. The only levers that move the needle are *reducing syscalls/seeks* (the `Decision::Skip` on blacklisted subtrees is the big one) and, in the warm case, the choice of walk framework. Follow Chapter 3's rule — measure (e.g. `perf record` / flamegraph on a real tree) before optimizing.

### Release / distribution

Releases are built with `cargo-dist` (v0.31.0). The `dist-workspace.toml` and `.github/workflows/release.yml` are autogenerated by dist. To publish, push a semver tag (`v0.x.y`).
