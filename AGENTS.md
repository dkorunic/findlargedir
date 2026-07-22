# AGENTS.md

`findlargedir` scans filesystems for "black hole" directories — directories with extreme numbers of flat entries that degrade `readdir` performance. Single binary, no subcommand.

## Commands

```sh
cargo fmt           # rustfmt.toml: max_width=79 + use_small_heuristics="max"
cargo clippy -- -D warnings
cargo test
cargo build --release
cargo bench --bench walk   # findlargedir vs GNU find; clones Linux kernel into benches/linux_root (BENCH_WALK_DIR to reuse). Warm + cold groups; cold drops caches via /proc/sys/vm/drop_caches (needs root, else skipped)
scdoc < man/findlargedir.1.scd > man/findlargedir.1   # regenerate the man page
```

Man page: authored in `man/findlargedir.1.scd` (scdoc), generated roff committed and bundled into release archives via `dist-workspace.toml` `include`. `OPTIONS`/`EXAMPLES` are hand-maintained — keep in sync with `args.rs` `print_help` and the README.

## Pinning

Edition 2024, MSRV 1.88–1.93. No `rust-toolchain.toml` — pin toolchain manually if building elsewhere.

## Architecture

| File | Responsibility |
|---|---|
| `src/main.rs` | CLI entry; loops over paths, calibration, walk |
| `src/args.rs` | `lexopt` hand-rolled args (`Args::from_parser` loop); path/thread validation; hand-maintained `--help`/`--version` |
| `src/compat.rs` | `cfg`-shims for the two `MetadataExt` uses (`dev`, `dir_size`) stable std lacks off-unix |
| `src/calibrate.rs` | Calibration: ordered geometric batch file-create to `FILE_CAP` + large-N `fit_calibration` + `fill_corrected` → `Calibration{per_entry,overhead}` |
| `src/walk.rs` | `parallel_search` policy: `CalContext` (per-fs calibration) + `classify_dir` (estimate skip) + `report_dir`/`offender_tier` (exact reporting) + `print_offender` |
| `src/walk/engine.rs` | `crossbeam-deque` work-stealing scheduler; `walk_dirs` (`classify`+`report`) + `Decision`/`DirInfo`; unconditional `(dev,ino)` ancestor-chain loop guard; `WalkStats` dropped-dir counters |
| `src/walk/unix.rs` | Unix leaf I/O via `rustix` (statat/getdents); `for_each_entry` returns live entry count + result (partial count on mid-read error) and polls `shutdown` per entry; `ENAMETOOLONG` → component-wise `openat` chain from the root fd |
| `src/walk/fallback.rs` | Non-Unix leaf I/O via `std::fs` |
| `src/interrupt.rs` | SIGINT/SIGTERM via `signal_hook` (2nd = immediate `_exit(128+sig)`); SIGQUIT left to default (core dump); SIGPIPE restored to `SIG_DFL` |
| `src/progress.rs` | `indicatif` spinner |

## Gotchas

- `#[global_allocator]` is `mimalloc`; do not change it without understanding allocation footprint.
- `walk.rs` uses `ahash::AHashSet` for skip-path and visited-path sets.
- `fs-err` wraps `std::fs` globally (`use fs = fs_err`).
- `alert_threshold` must be strictly `< blacklist_threshold` or `main.rs` bails at startup.
- `-o` (one-filesystem, on by default) uses `MetadataExt::dev()` to skip mount points; `-m`/`--cross-filesystem` turns it off. Booleans (`-f`/`-a`/`-o`) are presence flags (no `true`/`false` value).
- Per-filesystem calibration (`walk.rs` `CalContext`): root fs uses the up-front calibration; with one-filesystem off (`-m`), each *other* filesystem crossed is calibrated in place on first encounter and cached by `dev` (`Mutex<AHashMap>`). Resolved only after the boundary check, so a skipped fs is never written; read-only (detected via `calibrate::is_read_only`/`statvfs`), unwritable, or failed → flagging disabled for it. Only affects the skip decision (descended dirs report exact counts). The scan spinner is owned by `parallel_search` (not `main`) and paused (`ProgressBar::suspend`) while a crossed fs calibrates.
- Reporting is two-stage: `classify_dir` makes the **skip** decision on the *estimate* (only blacklisted subtrees are skipped unread, since reading a true black hole risks an uninterruptible `D`-state hang), while `report_dir` flags every *descended* dir on its **exact** live count, harvested for free from the walk's own `getdents` (leaf `for_each_entry` returns it). So estimates are user-facing only for skipped (blacklisted) dirs, labelled a size-based upper bound.
- `-a` (accurate mode) now only affects the skipped/blacklisted tier — an opt-in re-read of a dir the walk won't enumerate, done through the walk's own leaf (`DirInfo::count_entries` → `platform::count_entries`) so `O_NOFOLLOW` semantics match (a mid-scan swap to a symlink errors out instead of counting the wrong tree) and mid-read errors fall back to the estimate; descended dirs are exact regardless.
- Calibration creates files in **geometric** batches (initial floor 1 000, doubling) always up to `cap 50 000` (no early stop), **in order, not in parallel** — both for reproducibility across runs (a prior adaptive stop + parallel creation made successive results disagree). `-c` is the initial-batch floor, `-n` sets filename length (default 24, padded so per-entry cost is representative). `fit_calibration` least-squares-fits **only the upper-N half** of samples → asymptotic per-entry cost + overhead, then `fill_corrected` divides the slope by `FILL_FACTOR` (0.75) for real-dir leaf fill. Degenerate filesystems (no large-N growth, slope ≤ 0.5) disable flagging. Read-only (`is_read_only`), unwritable, or failed calibration degrades to `Calibration::DISABLED` (flagging disabled) — for the root path too, never fatal; only bad `-t` input is (unreadable metadata, or a `-t` dir on a different device than the scan root, which would fit the wrong fs). (`FILE_CAP` is lowered under `#[cfg(test)]` to keep tests fast.)
- Filesystem-loop detection in `engine.rs` is an unconditional `(dev,ino)` ancestor chain: symlink cycles under `-f` *and* bind mounts nested in their own source (same dev+ino, no symlink involved) both resolve to an ancestor. A chain, not a global set — non-cyclic aliases of one directory are still walked per path, like `find(1)`.
- SIGPIPE is restored to `SIG_DFL` first thing in `main` (Rust ignores it, and worker-thread `println!` would panic on EPIPE); do not "clean up" the `libc` dep or the `unsafe` in `interrupt.rs`.
- `-x N` is a **total** thread budget: `engine.rs` spawns N−1 walk workers, one thread reserved for coordination (calibration, output, progress). Deliberate and documented in `--help`/man — do not "fix" the off-by-one.
- stat/open failures drop a directory **and its subtree**, mid-enumeration read errors leave it under-counted, and loop-guard cuts skip cyclic paths — all are counted in `WalkStats` (`stat_errors`/`open_errors`/`read_errors`/`loop_hits`) and summarised at scan end — a scanner must not under-report silently. Shutdown is polled per `getdents` entry in the leaf (files never reach the engine callback, and black holes are file-heavy), so SIGINT latency is bounded by one entry, not one directory.
- Path-taking args (`<PATH>`, `-s`) are parsed from raw `OsStr` — non-UTF-8 roots are legal on Linux (untestable on macOS: APFS rejects non-UTF-8 filenames with EILSEQ).
- Search roots are deduped by **subsumption** (`main.rs` `is_subsumed`, canonicalized): a root nested in an earlier one is skipped. `-s` matching is literal post-normalisation — under `-f`, a dir reached through a symlink matches by the reached-through path, not the target.
- Non-unix fallback (`walk/fallback.rs`): the crate does compile off-unix (verified via `cargo check --target x86_64-pc-windows-msvc` with mimalloc temporarily excised — its C build needs a cross toolchain, not a code issue). Limitations: `compat::dev` = 0 (one-filesystem and the `-t` device check inert), `compat::dir_size` = `len()` (calibration goes degenerate → flagging disabled), `ino=path_hash` (collision → false loop-guard positive under `-f`), no dirfd API in stable std (no >`PATH_MAX` traversal).
- >`PATH_MAX` traversal: queued tasks hold **no fds** (a wide tree must not exhaust `RLIMIT_NOFILE`); the walk holds exactly one fd — the scan root, opened once in `walk_dirs` (`RootCtx`). Only on `ENAMETOOLONG` does the unix leaf `openat`-walk component-by-component from that root fd (O(depth) transient opens per deep dir; intermediates use `O_PATH` on Linux, `O_RDONLY|O_DIRECTORY` elsewhere — macOS has no `O_PATH`, so transit dirs need **read permission** there). Fast path is unchanged: 1 `statat` + 1 `openat` per dir. Non-unix fallback has no dirfd API → deep dirs keep failing into `WalkStats`. A >`PATH_MAX` *root* can't anchor (its own fd open fails) → counted, documented limitation.
- No `tests/` directory; integration test strategy is not in-tree.
- **I/O-bound, not CPU-bound.** Runtime is dominated by per-directory `stat` + `readdir` syscalls; profiling shows ~95 % of cold-cache wall time blocked on disk and ~77 % kernel even warm, with the core heuristic below the noise floor. Don't micro-optimize the per-dir arithmetic — the only real levers are reducing syscalls (the `Decision::Skip` on blacklisted subtrees) and the walk framework. Measure (perf/flamegraph) before optimizing.
