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
| `src/calibrate.rs` | Calibration: adaptive batch file-create + least-squares `fit_calibration` → `Calibration{per_entry,overhead}` |
| `src/walk.rs` | `parallel_search` policy + `classify_dir` + `print_offender` |
| `src/walk/engine.rs` | `crossbeam-deque` work-stealing scheduler; `walk_dirs` + `Decision`/`DirInfo` |
| `src/walk/unix.rs` | Unix leaf I/O via `rustix` (statat/getdents) |
| `src/walk/fallback.rs` | Non-Unix leaf I/O via `std::fs` |
| `src/interrupt.rs` | SIGINT/SIGTERM/SIGQUIT via `signal_hook` |
| `src/progress.rs` | `indicatif` spinner |

## Gotchas

- `#[global_allocator]` is `mimalloc`; do not change it without understanding allocation footprint.
- `walk.rs` uses `ahash::AHashSet` for skip-path and visited-path sets.
- `fs-err` wraps `std::fs` globally (`use fs = fs_err`).
- `alert_threshold` must be strictly `< blacklist_threshold` or `main.rs` bails at startup.
- `-o` (one-filesystem, on by default) uses `MetadataExt::dev()` to skip mount points.
- `-a` (accurate mode) triggers `std::fs::read_dir` on flagged dirs — heavy I/O.
- Calibration creates files in batches (floor 1 000, cap 50 000) until the dir grows, then least-squares-fits per-entry cost + overhead; `-c` is a batch floor. Degenerate filesystems (no growth) disable flagging. Requires r/w on target filesystem.
- No `tests/` directory; integration test strategy is not in-tree.
- **I/O-bound, not CPU-bound.** Runtime is dominated by per-directory `stat` + `readdir` syscalls; profiling shows ~95 % of cold-cache wall time blocked on disk and ~77 % kernel even warm, with the core heuristic below the noise floor. Don't micro-optimize the per-dir arithmetic — the only real levers are reducing syscalls (the `Decision::Skip` on blacklisted subtrees) and the walk framework. Measure (perf/flamegraph) before optimizing.
