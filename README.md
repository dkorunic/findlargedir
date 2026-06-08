# findlargedir

[![GitHub license](https://img.shields.io/github/license/dkorunic/findlargedir.svg)](https://github.com/dkorunic/findlargedir/blob/master/LICENSE.txt)
[![GitHub release](https://img.shields.io/github/release/dkorunic/findlargedir.svg)](https://github.com/dkorunic/findlargedir/releases/latest)
[![release](https://github.com/dkorunic/findlargedir/actions/workflows/release.yml/badge.svg)](https://github.com/dkorunic/findlargedir/actions/workflows/release.yml)

![](ferris.png)

(Ferris the Detective by [Esther Arzola](https://www.redbubble.com/people/earzola/shop), original design by [Karen Rustad Tölva](https://www.rustacean.net))

## About

`findlargedir` is a tool written specifically to help **quickly** identify "black hole" directories on any filesystem — directories with an extremely large number of entries in a flat structure (100k+). When a directory contains **many entries** (files or subdirectories), listing its contents becomes progressively slower, degrading the performance of every process that needs to read it. Processes reading large directory inodes can freeze in **uninterruptible sleep** ("D" state) for extended periods. Depending on the filesystem, this may start becoming noticeable around 100k entries and can be a severe performance problem at 1M+ entries.

Such directories mostly **cannot shrink back** even after their contents are cleaned up, because most Linux and Unix filesystems do not support directory inode shrinking (ext3/ext4 being a prime example). This situation commonly arises with forgotten web session directories (e.g. PHP session folders with GC intervals set to several days), CMS cache and compiled template directories, or POSIX filesystem emulations over object storage.

The program identifies these directories using **calibration** — it creates files in a temporary directory on the target filesystem and fits a line to how the directory's inode size grows, recovering the marginal bytes-per-entry cost (and fixed overhead) for that filesystem. It then estimates each directory's entry count from a single `stat`, scanning without performing expensive full directory reads. While many tools exist to scan filesystems (`find`, `du`, `ncdu`, etc.), none of them use heuristics to skip expensive lookups because they are designed for **full accuracy**. This tool is instead designed to use heuristics and alert on problems **without getting stuck** on the very directories it is trying to find.

By default, the program **does not follow symlinks** (use `-f` to enable) and **requires read/write permissions** on the filesystem being calibrated, in order to create temporary files and measure the resulting inode size.

![Demo](demo.gif)

## Caveats

- Requires read/write privileges on each filesystem being tested. A temporary directory with many small files is created during calibration and cleaned up afterwards.
- Accurate mode (`-a`) can cause excessive I/O and high memory usage; use it only when needed.

## Usage

```shell
find all blackhole directories with a huge amount of filesystem entries in a flat structure

Usage: findlargedir [OPTIONS] <PATH>...

Arguments:
  <PATH>...  Paths to check for large directories

Options:
  -f, --follow-symlinks <FOLLOW_SYMLINKS>          Follow symlinks [default: false] [possible values: true, false]
  -a, --accurate <ACCURATE>                        Perform accurate directory entry counting [default: false] [possible values: true, false]
  -o, --one-filesystem <ONE_FILESYSTEM>            Do not cross mount points [default: true] [possible values: true, false]
  -c, --calibration-count <CALIBRATION_COUNT>      Calibration batch size (raised to a 1000-file minimum) [default: 100]
  -A, --alert-threshold <ALERT_THRESHOLD>          Alert threshold count (print the estimate) [default: 10000]
  -B, --blacklist-threshold <BLACKLIST_THRESHOLD>  Blacklist threshold count (print the estimate and stop deeper scan) [default: 100000]
  -x, --threads <THREADS>                          Number of threads to use when calibrating and scanning (2..=65535) [default: 20]
  -p, --updates <UPDATES>                          Seconds between status updates, set to 0 to disable [default: 20]
  -i, --size-inode-ratio <SIZE_INODE_RATIO>        Skip calibration and provide directory entry to inode size ratio (typically ~21-32) [default: 0]
  -t, --calibration-path <CALIBRATION_PATH>        Custom calibration directory path
  -s, --skip-path <SKIP_PATH>                      Directories to exclude from scanning
  -h, --help                                       Print help
  -V, --version                                    Print version
```

**Accurate mode** (`-a`) performs a secondary, fully accurate pass over any flagged directories to get exact entry counts. Be aware that large directories will stall the process entirely for extended periods during this pass.

**One-filesystem mode** (`-o`) prevents the scan from descending into mounted filesystems, similar to `find -xdev`. It is enabled by default but can be disabled when scanning across mount points is desired.

**Skipping calibration** is possible by supplying the inode-size-to-entry ratio directly with `-i`. This is useful when the ratio is already known from a previous run on the same filesystem.

Setting `-p 0` disables periodic status updates.

## Benchmarks

A [Criterion](https://github.com/bheisler/criterion.rs) harness lives in
[`benches/walk.rs`](benches/walk.rs). It runs both `findlargedir` and GNU
`find` **as subprocesses** so the comparison is fair — each pays full process
startup plus a complete traversal — and times them over a shallow clone of the
Linux kernel source tree, in two scenarios: warm cache (data in RAM) and cold
cache (caches dropped before every run).

```shell
# Clones torvalds/linux into benches/linux_root on first run; reuse a
# checkout with BENCH_WALK_DIR=/path. Shorten a run with --measurement-time.
cargo bench --bench walk
```

The two commands measured are the functional equivalents of one another:

```shell
findlargedir <root>                                # calibrate, then walk
find <root> -xdev -type d -size +200000c           # flag large dir inodes
```

### Results

Measured on an 8-core Xeon E5-1630 v3 @ 3.70 GHz, ext4 on local SSD, against
the kernel tree (≈6,160 directories, 2.0 GB), `find` = GNU findutils 4.9.0.

**Warm cache** — Criterion warms up before sampling, so these numbers isolate
CPU and syscall cost with disk latency removed:

```text
walk_linux_kernel/findlargedir   time:   [106.78 ms  107.56 ms  108.37 ms]
walk_linux_kernel/find           time:   [ 80.60 ms   81.03 ms   81.49 ms]
```

| Command (warm) | Median | Notes |
|---|---|---|
| GNU `find` | **81.0 ms** | read-only `readdir` + `stat`, size filter |
| `findlargedir` (default) | **107.6 ms** | calibration + parallel walk |
| `findlargedir -i <ratio>` | **~20–40 ms** | calibration skipped — walk only |

**Cold cache** — the `walk_linux_kernel_cold` group drops the page, dentry and
inode caches (`sync; echo 3 > /proc/sys/vm/drop_caches`) before *every*
traversal, so each run pays real disk I/O. Needs root; skipped with a warning
otherwise:

```text
walk_linux_kernel_cold/findlargedir   time:   [1.7572 s  1.8655 s  2.0078 s]
walk_linux_kernel_cold/find           time:   [2.3978 s  2.4156 s  2.4342 s]
```

| Command (cold) | Median | vs `find` |
|---|---|---|
| `findlargedir` (default) | **1.87 s** | **1.30× faster** |
| GNU `find` | **2.42 s** | — |

### What the numbers mean

The two scenarios tell opposite stories, and both are expected.

**Warm cache — `find` wins (~1.3×).** With every inode already in RAM there is
no disk latency to hide, so the comparison reduces to raw work done, and a
default `findlargedir` run does *more*: it first creates and deletes files to
calibrate the filesystem's bytes-per-entry ratio. That one-time write is the
bulk of its time — skipping it with `-i` (when the ratio is already known)
drops the whole run to ~20–40 ms, *faster* than `find`. So the traversal itself
was never the bottleneck here; calibration was.

**Cold cache — `findlargedir` wins (~1.3×).** Once the data must come off disk,
`findlargedir`'s 20-thread parallel walk overlaps the per-directory `stat`
seeks that single-threaded `find` issues one after another, and that overlap
more than repays the calibration cost. Disk latency — not CPU — now dominates,
which is the state real filesystems are usually in.

And this corpus *understates* the real-world gap, because the kernel tree has
only a few thousand directories and **no "black hole" directories at all**.
`findlargedir`'s core trick is to estimate a directory's entry count from its
inode size — one `O(1)` `stat` — instead of enumerating it. On a tree that
actually holds directories with hundreds of thousands to millions of entries,
`find` must `readdir` every one of those entries while `findlargedir` reads a
single inode size and moves on. There the modest 1.3× widens into the
order-of-magnitude range, and grows further on slow or high-latency storage
(spinning disks, RAID, network/object filesystems) — the workloads the tool is
built for.

## Star history

[![Star History Chart](https://api.star-history.com/svg?repos=dkorunic/findlargedir&type=Date)](https://star-history.com/#dkorunic/findlargedir&Date)
