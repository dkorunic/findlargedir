# findlargedir

[![GitHub license](https://img.shields.io/github/license/dkorunic/findlargedir.svg)](https://github.com/dkorunic/findlargedir/blob/master/LICENSE.txt)
[![GitHub release](https://img.shields.io/github/release/dkorunic/findlargedir.svg)](https://github.com/dkorunic/findlargedir/releases/latest)
[![Rust Report Card](https://rust-reportcard.xuri.me/badge/github.com/dkorunic/findlargedir)](https://rust-reportcard.xuri.me/report/github.com/dkorunic/findlargedir)

## About

Findlargedir is a tool specifically written to help **quickly** identify "black hole" directories on an any filesystem having more than 100k entries in a single flat structure. When a directory has **many entries** (directories or files), getting directory listing gets slower and slower, impacting performance of all processes attempting to get a directory listing (for instance to delete some files and/or to find some specific files). Processes reading large directory inodes get frozen while doing so and end up in the **uninterruptible sleep** ("D" state) for longer and longer periods of time. Depending on the filesystem, this might start to become visible with 100k entries and starts being a very noticeable performance impact with 1M+ entries.

Such directories mostly **cannot shrink back** even if content gets cleaned up due to the fact that most Linux and Un\*x filesystems do not support directory inode shrinking (for instance very common ext3/ext4). This often happens with forgotten Web sessions directory (PHP sessions folder where GC interval was configured to several days), various cache folders (CMS compiled templates and caches), POSIX filesystem emulating object storage, etc.

Program will attempt to identify any number of such events and report on them based on **calibration**, ie. how many assumed directory entries are packed in each directory inode for each filesystem. While doing so, it will determine directory inode growth ratio to number of entries/inodes and will use that ratio to quickly scan filesystem, avoiding doing expensive/slow directory lookups. While there are many tools that scan the filesystem (`find`, `du`, `ncdu`, etc.), none of them use heuristics to avoid expensive lookups, since they are designed to be **fully accurate**, while this tool is meant to use heuristics and alert on issues **without getting stuck** on problematic folders.

Program will **not follow symlinks** and **requires r/w permissions** to calibrate directory to be able to calculate a directory inode size to number of entries ratio and estimate a number of entries in a directory without actually counting them. While this method is just an approximation of the actual number of entries in a directory, it is good enough to quickly scan for offending directories.

![Demo](demo.gif)

## Caveats

- requires r/w privileges for an each filesystem being tested, it will also create a temporary directory with a lot of temporary files which are cleaned up afterwards
- accurate mode (`-a`) can cause an excessive I/O and an excessive memory use; only use when appropriate

## Usage

```shell
Usage: findlargedir [OPTIONS] <PATH>...

Arguments:
  <PATH>...  Paths to check for large directories

Options:
  -a, --accurate <ACCURATE>
          Perform accurate directory entry counting [default: false] [possible values: true, false]
  -o, --one-filesystem <ONE_FILESYSTEM>
          Do not cross mount points [default: true] [possible values: true, false]
  -c, --calibration-count <CALIBRATION_COUNT>
          Calibration directory file count [default: 100000]
  -A, --alert-threshold <ALERT_THRESHOLD>
          Alert threshold count (print the estimate) [default: 10000]
  -B, --blacklist-threshold <BLACKLIST_THRESHOLD>
          Blacklist threshold count (print the estimate and stop deeper scan) [default: 100000]
  -x, --threads <THREADS>
          Number of threads to use when calibrating and scanning [default: 24]
  -p, --updates <UPDATES>
          Seconds between status updates, set to 0 to disable [default: 20]
  -i, --size-inode-ratio <SIZE_INODE_RATIO>
          Skip calibration and provide directory entry to inode size ratio (typically ~21-32) [default: 0]
  -t, --calibration-path <CALIBRATION_PATH>
          Custom calibration directory path
  -s, --skip-path <SKIP_PATH>
          Directories to exclude from scanning
  -h, --help
          Print help information
  -V, --version
          Print version information
```

When using **accurate mode** (`-a` parameter) beware that large directory lookups will stall the process completely for extended periods of time. What this mode does is basically a secondary fully accurate pass on a possibly offending directory calculating exact number of entries.

To avoid descending into mounted filesystems (as in find -xdev option), parameter **one-filesystem mode** (`-o` parameter) is toggled by default, but it can be disabled if necessary.

It is possible to completely skip calibration phase by manually providing directory inode size to number of entries ratio with `-i` parameter. It makes sense only when you already know the ratio, for example from previous runs.

Setting `-p` paramter to 0 will stop program from giving occasional status updates.

## Benchmarks

### Findlargedir vs GNU find

Benchmark setup:

```shell
$ cat bench1.sh
#!/bin/sh
find / -xdev -type d -size +200000c

$ cat bench2.sh
#!/bin/sh
/usr/local/sbin/findlargedir /
```

Actual results:

```shell
$ hyperfine --prepare 'echo 3 | tee /proc/sys/vm/drop_caches' ./bench1.sh ./bench2.sh

Benchmark 1: ./bench1.sh
  Time (mean ± σ):     216.122 s ±  1.118 s    [User: 1.707 s, System: 10.359 s]
  Range (min … max):   214.701 s … 217.845 s    10 runs

Benchmark 1: ./bench2.sh
  Time (mean ± σ):     120.427 s ±  1.978 s    [User: 45.438 s, System: 92.729 s]
  Range (min … max):   116.673 s … 123.187 s    10 runs
```