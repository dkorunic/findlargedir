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
Uses a `rayon` thread pool to mass-create `calibration_count` (default 100) empty files in a temporary directory on the target filesystem, then reads the directory's inode size. The ratio `inode_size / calibration_count` gives bytes-per-entry for that specific filesystem. Can be skipped by passing `-i <ratio>` directly, or pointed at a custom dir with `-t`. A `size_inode_ratio` of `0` (e.g. shutdown mid-calibration) disables flagging — `process_dir_entry` guards against the divide-by-zero.

**Phase 2 — Parallel walk (`src/walk.rs`)**
Uses `ignore::WalkBuilder` (the same engine as ripgrep) to walk the filesystem in parallel; a separate single-thread `rayon` pool prints periodic progress (`-p`). For each directory it computes `approx_entries = dir_inode_size / size_inode_ratio`. Directories **strictly exceeding** (`>`):
- `alert_threshold` (default 10 000) → yellow warning, scanning continues (`WalkState::Continue`)
- `blacklist_threshold` (default 100 000) → red warning, subtree is **skipped** (`WalkState::Skip`)

`main.rs` bails at startup if `alert_threshold >= blacklist_threshold` (the yellow branch would be unreachable).

Accurate mode (`-a`) replaces the estimate with an exact `std::fs::read_dir().count()` for each flagged directory.

### Module layout

| File | Responsibility |
|---|---|
| `src/main.rs` | CLI entry point; loops over paths, orchestrates calibration + walk |
| `src/args.rs` | Clap-derive argument definitions and path/thread validation |
| `src/calibrate.rs` | Calibration via mass file creation; returns `size_inode_ratio` |
| `src/walk.rs` | `parallel_search` + `process_dir_entry` + `print_offender` |
| `src/interrupt.rs` | SIGINT/SIGTERM/SIGQUIT handler via `signal_hook` |
| `src/progress.rs` | `indicatif` spinner helper |

### Key design decisions

- **`mimalloc`** is set as `#[global_allocator]` for allocation performance.
- **`ahash::AHashSet`** is used for the skip-path and visited-path sets (non-cryptographic, fast).
- **`fs-err`** wraps `std::fs` to add path context to IO errors automatically.
- Shutdown is coordinated via a shared `Arc<AtomicBool>` checked at each walk step and in calibration loops.
- One-filesystem mode (`-o`, default on) uses `MetadataExt::dev()` comparisons to detect mount boundaries.

### Performance profile (I/O-bound, not CPU-bound)

The tool's runtime is dominated by **filesystem I/O** — one `stat` per directory plus `readdir`/`getdents` traversal — not by its own computation. Profiling a 183 k-directory traversal confirmed this:
- **Cold cache:** ~95 % of wall time blocked on disk (≈4 % CPU); our own code is ~0.5 % of wall time.
- **Warm cache (no disk waits):** still ~77 % kernel syscall handling vs ~17 % in our binary, and that 17 % is mostly `ignore`-crate path bookkeeping + allocator/`Arc` churn. The core heuristic (the `size / size_inode_ratio` division, threshold checks, `AHashSet` lookup, `AtomicU64` increment) is below the profiler's noise floor.

**Implication for changes:** micro-optimizing the per-directory arithmetic or hot path (branchless tricks, SoA, lock-free sharding, manual inlining) buys nothing measurable here — there is no CPU time there to reclaim. The only levers that move the needle are *reducing syscalls/seeks* (the `WalkState::Skip` on blacklisted subtrees is the big one) and, in the warm case, the choice of walk framework. Follow Chapter 3's rule — measure (e.g. `perf record` / flamegraph on a real tree) before optimizing.

### Release / distribution

Releases are built with `cargo-dist` (v0.31.0). The `dist-workspace.toml` and `.github/workflows/release.yml` are autogenerated by dist. To publish, push a semver tag (`v0.x.y`).
