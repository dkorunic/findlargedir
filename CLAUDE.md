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
```

`edition = "2024"`, `rust-version = "1.88.0"` (MSRV). There is **no** `rust-toolchain.toml` — the toolchain is not pinned, so pin it manually if building elsewhere. Lint levels are centralized in `Cargo.toml`'s `[lints]` table (`clippy::all = deny`, `clippy::pedantic = warn`, `clippy::redundant_clone = deny`, `nonstandard_style = deny`).

A sibling `AGENTS.md` carries the same guidance in condensed form; keep the two in sync when editing either.

## Architecture

`findlargedir` is a single-binary CLI tool that scans filesystems for "black hole" directories — directories with an extremely large number of entries that cause performance problems. It avoids doing expensive full `readdir` passes by using inode-size heuristics.

### Two-phase operation

**Phase 1 — Calibration (`src/calibrate.rs`)**
Creates empty files on the target filesystem in batches (a floor of 1 000 per batch; `-c` raises it), re-`stat`ing the temp directory after each batch until its inode size has grown a few times (`STEP_TARGET`) or a 50 000-file cap (`FILE_CAP`) is hit. A least-squares fit (`fit_calibration`) over the `(files, size)` samples gives the **marginal** bytes-per-entry (slope) and **fixed overhead** (intercept) as a `Calibration`; the walk then estimates `approx_entries = (dir_size − overhead) / per_entry`. Because the slope is the *marginal* cost (not `total/count`, which folds in the fixed block), estimates run higher — and more accurate — than before on block filesystems. A filesystem whose directory size never grows is detected (slope ≤ 0.5) and reported, with flagging disabled (`per_entry = 0`, the same sentinel as a shutdown mid-calibration). Calibration can be skipped with `-i <ratio>` (per-entry only, overhead 0) or pointed at a custom dir with `-t`. `classify_dir` guards against the zero-`per_entry` divide.

**Phase 2 — Parallel walk (`src/walk.rs`)**
Uses a custom `crossbeam-deque` work-stealing engine (`src/walk/engine.rs`, adapted from the sibling `minifind` project) to walk the filesystem in parallel, visiting directories only; a separate single-thread `rayon` pool prints periodic progress (`-p`). For each directory it computes `approx_entries = (dir_inode_size − overhead) / per_entry`. Directories **strictly exceeding** (`>`):
- `alert_threshold` (default 10 000) → yellow warning, scanning continues (`Decision::Descend`)
- `blacklist_threshold` (default 100 000) → red warning, subtree is **skipped** (`Decision::Skip`)

`main.rs` bails at startup if `alert_threshold >= blacklist_threshold` (the yellow branch would be unreachable).

Accurate mode (`-a`) replaces the estimate with an exact `std::fs::read_dir().count()` for each flagged directory.

### Module layout

| File | Responsibility |
|---|---|
| `src/main.rs` | CLI entry point; loops over paths, orchestrates calibration + walk |
| `src/args.rs` | Clap-derive argument definitions and path/thread validation |
| `src/calibrate.rs` | Adaptive batch calibration + `fit_calibration` regression; returns `Calibration` |
| `src/walk.rs` | `parallel_search` policy + `classify_dir` + `print_offender` |
| `src/walk/engine.rs` | `crossbeam-deque` work-stealing scheduler; `walk_dirs` + `Decision`/`DirInfo` |
| `src/walk/unix.rs` | Unix leaf I/O via `rustix` (statat/getdents) |
| `src/walk/fallback.rs` | Non-Unix leaf I/O via `std::fs` |
| `src/interrupt.rs` | SIGINT/SIGTERM/SIGQUIT handler via `signal_hook` |
| `src/progress.rs` | `indicatif` spinner helper |

### Key design decisions

- **`mimalloc`** is set as `#[global_allocator]` for allocation performance.
- **`ahash::AHashSet`** is used for the skip-path and visited-path sets (non-cryptographic, fast).
- **`fs-err`** wraps `std::fs` to add path context to IO errors automatically.
- Shutdown is coordinated via a shared `Arc<AtomicBool>` checked at each walk step and in calibration loops.
- One-filesystem mode (`-o`, default on) uses `MetadataExt::dev()` comparisons to detect mount boundaries.

### Performance profile (I/O-bound, not CPU-bound)

The tool's runtime is dominated by **filesystem I/O** — one `stat` per directory plus `readdir`/`getdents` traversal — not by its own computation. Profiling a 183 k-directory traversal confirmed this (measured with the previous `ignore`-based walker; the conclusion still holds for the current engine):
- **Cold cache:** ~95 % of wall time blocked on disk (≈4 % CPU); our own code is ~0.5 % of wall time.
- **Warm cache (no disk waits):** still ~77 % kernel syscall handling vs ~17 % in our binary, and that 17 % is mostly walker path bookkeeping + allocator/`Arc` churn. The core heuristic (the `(size − overhead) / per_entry` division, threshold checks, `AHashSet` lookup, `AtomicU64` increment) is below the profiler's noise floor.

**Implication for changes:** micro-optimizing the per-directory arithmetic or hot path (branchless tricks, SoA, lock-free sharding, manual inlining) buys nothing measurable here — there is no CPU time there to reclaim. The only levers that move the needle are *reducing syscalls/seeks* (the `Decision::Skip` on blacklisted subtrees is the big one) and, in the warm case, the choice of walk framework. Follow Chapter 3's rule — measure (e.g. `perf record` / flamegraph on a real tree) before optimizing.

### Release / distribution

Releases are built with `cargo-dist` (v0.31.0). The `dist-workspace.toml` and `.github/workflows/release.yml` are autogenerated by dist. To publish, push a semver tag (`v0.x.y`).
