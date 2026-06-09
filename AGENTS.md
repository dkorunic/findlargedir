# AGENTS.md

`findlargedir` scans filesystems for "black hole" directories — directories with extreme numbers of flat entries that degrade `readdir` performance. Single binary, no subcommand.

## Commands

```sh
cargo fmt           # rustfmt.toml: max_width=79 + use_small_heuristics="max"
cargo clippy -- -D warnings
cargo test
cargo build --release
cargo bench --bench walk   # findlargedir vs GNU find; clones Linux kernel into benches/linux_root (BENCH_WALK_DIR to reuse). Warm + cold groups; cold drops caches via /proc/sys/vm/drop_caches (needs root, else skipped)
```

## Pinning

Edition 2024, MSRV 1.88–1.93. No `rust-toolchain.toml` — pin toolchain manually if building elsewhere.

## Architecture

| File | Responsibility |
|---|---|
| `src/main.rs` | CLI entry; loops over paths, calibration, walk |
| `src/args.rs` | Clap-derive args; path/thread validation |
| `src/calibrate.rs` | Calibration: ordered geometric batch file-create to `FILE_CAP` + large-N `fit_calibration` + `fill_corrected` → `Calibration{per_entry,overhead}` |
| `src/walk.rs` | `parallel_search` policy: `CalContext` (per-fs calibration) + `classify_dir` (estimate skip) + `report_dir`/`offender_tier` (exact reporting) + `print_offender` |
| `src/walk/engine.rs` | `crossbeam-deque` work-stealing scheduler; `walk_dirs` (`classify`+`report`) + `Decision`/`DirInfo` |
| `src/walk/unix.rs` | Unix leaf I/O via `rustix` (statat/getdents); `for_each_entry` returns live entry count |
| `src/walk/fallback.rs` | Non-Unix leaf I/O via `std::fs` |
| `src/interrupt.rs` | SIGINT/SIGTERM/SIGQUIT via `signal_hook` |
| `src/progress.rs` | `indicatif` spinner |

## Gotchas

- `#[global_allocator]` is `mimalloc`; do not change it without understanding allocation footprint.
- `walk.rs` uses `ahash::AHashSet` for skip-path and visited-path sets.
- `fs-err` wraps `std::fs` globally (`use fs = fs_err`).
- `alert_threshold` must be strictly `< blacklist_threshold` or `main.rs` bails at startup.
- `-o` (one-filesystem, on by default) uses `MetadataExt::dev()` to skip mount points.
- Per-filesystem calibration (`walk.rs` `CalContext`): root fs uses the up-front calibration; with `-o` off, each *other* filesystem crossed is calibrated in place on first encounter and cached by `dev` (`Mutex<AHashMap>`). Resolved only after the boundary check, so a skipped fs is never written; read-only (detected via `calibrate::is_read_only`/`statvfs`), unwritable, or failed → flagging disabled for it. Only affects the skip decision (descended dirs report exact counts).
- Reporting is two-stage: `classify_dir` makes the **skip** decision on the *estimate* (only blacklisted subtrees are skipped unread, since reading a true black hole risks an uninterruptible `D`-state hang), while `report_dir` flags every *descended* dir on its **exact** live count, harvested for free from the walk's own `getdents` (leaf `for_each_entry` returns it). So estimates are user-facing only for skipped (blacklisted) dirs, labelled a size-based upper bound.
- `-a` (accurate mode) now only affects the skipped/blacklisted tier — an opt-in `read_dir` on a dir the walk won't enumerate; descended dirs are exact regardless.
- Calibration creates files in **geometric** batches (initial floor 1 000, doubling) always up to `cap 50 000` (no early stop), **in order, not in parallel** — both for reproducibility across runs (a prior adaptive stop + parallel creation made successive results disagree). `-c` is the initial-batch floor, `-n` sets filename length (default 24, padded so per-entry cost is representative). `fit_calibration` least-squares-fits **only the upper-N half** of samples → asymptotic per-entry cost + overhead, then `fill_corrected` divides the slope by `FILL_FACTOR` (0.75) for real-dir leaf fill. Degenerate filesystems (no large-N growth, slope ≤ 0.5) disable flagging. Read-only filesystems skip calibration (`is_read_only`) rather than failing; otherwise needs r/w on the target. (`FILE_CAP` is lowered under `#[cfg(test)]` to keep tests fast.)
- No `tests/` directory; integration test strategy is not in-tree.
- **I/O-bound, not CPU-bound.** Runtime is dominated by per-directory `stat` + `readdir` syscalls; profiling shows ~95 % of cold-cache wall time blocked on disk and ~77 % kernel even warm, with the core heuristic below the noise floor. Don't micro-optimize the per-dir arithmetic — the only real levers are reducing syscalls (the `Decision::Skip` on blacklisted subtrees) and the walk framework. Measure (perf/flamegraph) before optimizing.
