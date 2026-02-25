# findlargedir

[![GitHub license](https://img.shields.io/github/license/dkorunic/findlargedir.svg)](https://github.com/dkorunic/findlargedir/blob/master/LICENSE.txt)
[![GitHub release](https://img.shields.io/github/release/dkorunic/findlargedir.svg)](https://github.com/dkorunic/findlargedir/releases/latest)
[![release](https://github.com/dkorunic/findlargedir/actions/workflows/release.yml/badge.svg)](https://github.com/dkorunic/findlargedir/actions/workflows/release.yml)

![](ferris.png)

(Ferris the Detective by [Esther Arzola](https://www.redbubble.com/people/earzola/shop), original design by [Karen Rustad Tölva](https://www.rustacean.net))

## About

`findlargedir` is a tool written specifically to help **quickly** identify "black hole" directories on any filesystem — directories with an extremely large number of entries in a flat structure (100k+). When a directory contains **many entries** (files or subdirectories), listing its contents becomes progressively slower, degrading the performance of every process that needs to read it. Processes reading large directory inodes can freeze in **uninterruptible sleep** ("D" state) for extended periods. Depending on the filesystem, this may start becoming noticeable around 100k entries and can be a severe performance problem at 1M+ entries.

Such directories mostly **cannot shrink back** even after their contents are cleaned up, because most Linux and Unix filesystems do not support directory inode shrinking (ext3/ext4 being a prime example). This situation commonly arises with forgotten web session directories (e.g. PHP session folders with GC intervals set to several days), CMS cache and compiled template directories, or POSIX filesystem emulations over object storage.

The program identifies these directories using **calibration** — it measures how many directory entries correspond to each byte of inode size on the target filesystem, then uses that ratio to quickly scan without performing expensive full directory reads. While many tools exist to scan filesystems (`find`, `du`, `ncdu`, etc.), none of them use heuristics to skip expensive lookups because they are designed for **full accuracy**. This tool is instead designed to use heuristics and alert on problems **without getting stuck** on the very directories it is trying to find.

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
  -c, --calibration-count <CALIBRATION_COUNT>      Calibration directory file count [default: 100]
  -A, --alert-threshold <ALERT_THRESHOLD>          Alert threshold count (print the estimate) [default: 10000]
  -B, --blacklist-threshold <BLACKLIST_THRESHOLD>  Blacklist threshold count (print the estimate and stop deeper scan) [default: 100000]
  -x, --threads <THREADS>                          Number of threads to use when calibrating and scanning [default: 20]
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

### findlargedir vs GNU find

#### Mid-range server / mechanical storage

Hardware: 8-core Xeon E5-1630 with a 4-drive SATA RAID-10 array

Benchmark setup:

```shell
$ cat bench1.sh
#!/bin/dash
exec /usr/bin/find / -xdev -type d -size +200000c

$ cat bench2.sh
#!/bin/dash
exec /usr/local/sbin/findlargedir /
```

Results measured with [hyperfine](https://github.com/sharkdp/hyperfine):

```shell
$ hyperfine --prepare 'echo 3 | tee /proc/sys/vm/drop_caches' \
  ./bench1.sh ./bench2.sh

Benchmark 1: ./bench1.sh
  Time (mean ± σ):     357.040 s ±  7.176 s    [User: 2.324 s, System: 13.881 s]
  Range (min … max):   349.639 s … 367.636 s    10 runs

Benchmark 2: ./bench2.sh
  Time (mean ± σ):     199.751 s ±  4.431 s    [User: 75.163 s, System: 141.271 s]
  Range (min … max):   190.136 s … 203.432 s    10 runs

Summary
  './bench2.sh' ran
    1.79 ± 0.05 times faster than './bench1.sh'
```

#### High-end server / SSD storage

Hardware: 48-core Xeon Silver 4214, 7-drive SM883 SATA RAID-5 array, 2 TB of content (many containers with small files)

Same benchmark setup. Results:

```shell
$ hyperfine --prepare 'echo 3 | tee /proc/sys/vm/drop_caches' \
  ./bench1.sh ./bench2.sh

Benchmark 1: ./bench1.sh
  Time (mean ± σ):     392.433 s ±  1.952 s    [User: 16.056 s, System: 81.994 s]
  Range (min … max):   390.284 s … 395.732 s    10 runs

Benchmark 2: ./bench2.sh
  Time (mean ± σ):     34.650 s ±  0.469 s    [User: 79.441 s, System: 528.939 s]
  Range (min … max):   34.049 s … 35.388 s    10 runs

Summary
  './bench2.sh' ran
   11.33 ± 0.16 times faster than './bench1.sh'
```

## Star history

[![Star History Chart](https://api.star-history.com/svg?repos=dkorunic/findlargedir&type=Date)](https://star-history.com/#dkorunic/findlargedir&Date)
