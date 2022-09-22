# findlargedir

[![GitHub license](https://img.shields.io/github/license/dkorunic/findlargedir.svg)](https://github.com/dkorunic/findlargedir/blob/master/LICENSE.txt)
[![GitHub release](https://img.shields.io/github/release/dkorunic/findlargedir.svg)](https://github.com/dkorunic/findlargedir/releases/latest)
[![Rust Report Card](https://rust-reportcard.xuri.me/badge/github.com/dkorunic/findlargedir)](https://rust-reportcard.xuri.me/report/github.com/dkorunic/findlargedir)

## About

Findlargedir is a quick hack intended to help identifying "black hole" directories on an any filesystem having more than 100,000 entries in a single flat structure. Program will attempt to identify any number of such events and report on them based on heuristics, ie. how many assumed directory entries are packed in each directory inode.

Program will **not follow symlinks** and **requires r/w permissions** to calibrate directory to be able to calculate a directory inode size to number of entries ratio and estimate a number of entries in a directory without actually counting them. While this method is just an approximation of the actual number of entries in a directory, it is good enough to quickly scan for offending directories.

[![asciicast](https://asciinema.org/a/boGSGyxVZ8oY2K0XqhYcdWNGl.svg)](https://asciinema.org/a/boGSGyxVZ8oY2K0XqhYcdWNGl)

## Caveats

- requires r/w privileges for an each filesystem being tested, it will also create a temporary directory with a lot of temporary files which are cleaned up afterwards
- accurate mode (`-a`) can cause an excessive I/O and an excessive memory use; only use when appropriate


## Usage

```shell
USAGE:
    findlargedir [OPTIONS] <PATH>...

ARGS:
    <PATH>...

OPTIONS:
    -a, --accurate <ACCURATE>
            [default: false] [possible values: true, false]

    -A, --alert-threshold <ALERT_THRESHOLD>
            [default: 10000]

    -B, --blacklist-threshold <BLACKLIST_THRESHOLD>
            [default: 100000]

    -c, --calibration-count <CALIBRATION_COUNT>
            [default: 100000]

    -h, --help
            Print help information

    -o, --one-filesystem <ONE_FILESYSTEM>
            [default: true] [possible values: true, false]

    -t, --calibration-path <CALIBRATION_PATH>


    -V, --version
            Print version information
```

(Note: This is still not merged) When using **accurate mode** (`-a` parameter) beware that large directory lookups will stall the process completely for extended periods of time. What this mode does is basically a secondary fully accurate pass on a possibly offending directory calculating exact number of entries.

If you want to avoid descending into mounted filesystems (as in find -xdev option), use **one-filesystem mode** with `-o` parameter and this toggled by default.

