# findlargedir

[![GitHub license](https://img.shields.io/github/license/dkorunic/findlargedir.svg)](https://github.com/dkorunic/findlargedir/blob/master/LICENSE.txt)
[![GitHub release](https://img.shields.io/github/release/dkorunic/findlargedir.svg)](https://github.com/dkorunic/findlargedir/releases/latest)
[![Rust Report Card](https://rust-reportcard.xuri.me/badge/github.com/dkorunic/findlargedir)](https://rust-reportcard.xuri.me/report/github.com/dkorunic/findlargedir)

## About

Findlargedir is a quick hack intended to help identifying "black hole" directories on an any filesystem having more than 100,000 entries in a single flat structure. Program will attempt to identify any number of such events and report on them based on heuristics, ie. how many assumed directory entries are packed in each directory inode.

Program will **not follow symlinks** and **requires r/w permissions** to calibrate directory to be able to calculate a directory inode size to number of entries ratio and estimate a number of entries in a directory without actually counting them. While this method is just an approximation of the actual number of entries in a directory, it is good enough to quickly scan for offending directories.

[![asciicast](https://asciinema.org/a/524314.svg)](https://asciinema.org/a/524314)

## Caveats

- requires r/w privileges for an each filesystem being tested, it will also create a temporary directory with a lot of temporary files which are cleaned up afterwards
- accurate mode (`-a`) can cause an excessive I/O and an excessive memory use; only use when appropriate


## Usage

```shell
USAGE:
    findlargedir [OPTIONS] <PATH>...

ARGS:
    <PATH>...    Paths to check for large directories

OPTIONS:
    -a, --accurate <ACCURATE>
            Perform accurate directory entry counting [default: false] [possible values: true,
            false]

    -A, --alert-threshold <ALERT_THRESHOLD>
            Alert threshold count (print the estimate) [default: 10000]

    -B, --blacklist-threshold <BLACKLIST_THRESHOLD>
            Blacklist threshold count (print the estimate and stop deeper scan) [default: 100000]

    -c, --calibration-count <CALIBRATION_COUNT>
            Calibration directory file count [default: 100000]

    -h, --help
            Print help information

    -o, --one-filesystem <ONE_FILESYSTEM>
            Do not cross mount points [default: true] [possible values: true, false]

    -p, --updates <UPDATES>
            Seconds between status updates, set to 0 to disable [default: 30]

    -s, --skip-path <SKIP_PATH>
            Directories to exclude from scanning

    -t, --calibration-path <CALIBRATION_PATH>
            Custom calibration directory path

    -V, --version
            Print version information

    -x, --threads <THREADS>
            Number of threads to use when calibrating and scanning [default: 24]
```

When using **accurate mode** (`-a` parameter) beware that large directory lookups will stall the process completely for extended periods of time. What this mode does is basically a secondary fully accurate pass on a possibly offending directory calculating exact number of entries.

To avoid descending into mounted filesystems (as in find -xdev option), parameter **one-filesystem mode** is toggled by default, but it can be disabled if necessary.

