# AGENTS.md

`findlargedir` scans filesystems for "black hole" directories — directories with extreme numbers of flat entries that degrade `readdir` performance. Single binary, no subcommand.

## Commands

```sh
cargo fmt           # rustfmt.toml: max_width=79 + use_small_heuristics="max"
cargo clippy -- -D warnings
cargo test
cargo build --release
```

## Pinning

Edition 2024, MSRV 1.88–1.93. No `rust-toolchain.toml` — pin toolchain manually if building elsewhere.

## Architecture

| File | Responsibility |
|---|---|
| `src/main.rs` | CLI entry; loops over paths, calibration, walk |
| `src/args.rs` | Clap-derive args; path/thread validation |
| `src/calibrate.rs` | Calibration (mass file create, inode-size ratio) |
| `src/walk.rs` | `parallel_search` + `process_dir_entry` + `print_offender` |
| `src/interrupt.rs` | SIGINT/SIGTERM/SIGQUIT via `signal_hook` |
| `src/progress.rs` | `indicatif` spinner |

## Gotchas

- `#[global_allocator]` is `mimalloc`; do not change it without understanding allocation footprint.
- `walk.rs` uses `ahash::AHashSet` for skip-path and visited-path sets.
- `fs-err` wraps `std::fs` globally (`use fs = fs_err`).
- `alert_threshold` must be strictly `< blacklist_threshold` or `main.rs` bails at startup.
- `-o` (one-filesystem, on by default) uses `MetadataExt::dev()` to skip mount points.
- `-a` (accurate mode) triggers `std::fs::read_dir` on flagged dirs — heavy I/O.
- Calibration creates `calibration_count` (default 100) temporary files and deletes them. Requires r/w on target filesystem.
- No `tests/` directory; integration test strategy is not in-tree.
