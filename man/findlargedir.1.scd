findlargedir(1)

# NAME

findlargedir - find blackhole directories with huge numbers of entries

# SYNOPSIS

*findlargedir* [_OPTIONS_] _PATH_...

# DESCRIPTION

*findlargedir* scans each _PATH_ for "blackhole" directories — directories
holding an extremely large number of entries in a single flat level, which
degrade filesystem performance and, on most Unix filesystems (ext3/ext4 among
them), *cannot shrink back* once grown. They typically arise from forgotten web
session stores, CMS cache or compiled-template directories, or POSIX emulations
over object storage.

To avoid the very cost it is hunting for — a full *readdir*(3) over a
multi-million-entry directory can wedge a process in uninterruptible sleep —
*findlargedir* works in two phases:

. *Calibration.* It creates and then removes empty files on the target
  filesystem to learn that filesystem's bytes-per-entry inode growth ratio.
. *Parallel walk.* It walks the tree with its own work-stealing engine,
  visiting directories only. A directory whose *estimated* entry count (from
  its inode size and the calibrated ratio) exceeds the blacklist threshold is
  reported and its subtree is *skipped unread*. Every directory that is read is
  additionally reported on its *exact* live entry count, harvested for free
  from the same enumeration the walk already performs.

Each _PATH_ must be an existing directory. By default symbolic links are not
followed and mount points are not crossed; one worker thread is spawned per
logical CPU.

Calibration writes to the target filesystem, so *findlargedir* generally needs
to run as a user who can create files there (often *root*). A read-only or
unwritable filesystem is detected and has flagging disabled rather than
aborting the run; *-i* skips calibration entirely.

# OPTIONS

## Traversal

*-f*, *--follow-symlinks*
	Follow symbolic links. Off by default.

*-o*, *--one-filesystem*
	Do not descend into directories on other filesystems (the default).

*-m*, *--cross-filesystem*
	Cross mount points. Each filesystem encountered is calibrated separately,
	since per-entry inode geometry differs between filesystems.

*-x*, *--threads* _N_
	Number of worker threads (2..=65535). Default: the logical CPU count.

*-s*, *--skip-path* _PATH_
	Exclude _PATH_ from scanning (repeatable). A skip path that is absent on
	this host is accepted, so virtual or optional paths can be listed
	unconditionally.

## Detection thresholds

*-A*, *--alert-threshold* _N_
	Report a directory whose entry count reaches _N_ as a yellow alert, judged
	on the *exact* count of every directory that is read. Default: _10000_.
	Must be strictly less than the blacklist threshold.

*-B*, *--blacklist-threshold* _N_
	Report a directory whose count reaches _N_ as a red offender and *skip its
	subtree unread*. The skip decision uses the pre-read size estimate (an
	upper bound, since a directory's inode size never shrinks). Default:
	_100000_.

*-a*, *--accurate*
	For blacklisted (skipped) directories only, perform an explicit entry count
	instead of reporting the estimate, accepting the extra cost. Directories
	that are descended are always reported on their exact count regardless.

## Calibration

*-c*, *--calibration-count* _N_
	Calibration batch size, raised to a 1000-file minimum. Default: _100_.

*-n*, *--calibration-name-length* _N_
	Length of the zero-padded names used for calibration files (1..=255),
	matched to typical real entry names so the per-entry cost is
	representative. Default: _24_.

*-t*, *--calibration-path* _PATH_
	Calibrate in _PATH_ instead of the scan root (for example a writable
	directory on the same filesystem).

*-i*, *--size-inode-ratio* _N_
	Skip calibration and use _N_ bytes per entry directly (fixed overhead is
	taken as zero). Default: _0_ (calibrate normally).

## Output

*-p*, *--updates* _SECONDS_
	Seconds between progress status updates; _0_ disables them. Default: _20_.

## Information

*-h*, *--help*
	Print a short usage summary and exit.

*-V*, *--version*
	Print version information and exit.

# EXAMPLES

Calibrate the filesystem and scan a single tree:

```
findlargedir /var/www
```

Scan as *root* so calibration can write to the target filesystem:

```
sudo findlargedir /
```

Skip calibration with a known bytes-per-entry ratio — the fastest mode, useful
when the ratio for this filesystem is already known:

```
findlargedir -i 1024 /var/www
```

Cross mount points, calibrating each filesystem it reaches:

```
findlargedir -m /
```

Exclude virtual and irrelevant trees from the scan:

```
findlargedir -s /proc -s /sys -s /var/lib/docker /
```

Lower the alert threshold to surface smaller offenders, and report exact counts
even for blacklisted directories:

```
findlargedir -A 1000 -B 50000 -a /srv
```

Calibrate in a writable scratch directory when the scan root is awkward to
write into:

```
findlargedir -t /var/tmp /mnt/objectstore
```

Run quietly, without periodic progress updates:

```
findlargedir -p 0 /data
```

# EXIT STATUS

*0*
	Success.

*1*
	A runtime error occurred — for example the alert threshold was not strictly
	less than the blacklist threshold, or the interrupt handler could not be
	installed.

*2*
	Invalid command-line usage — an unrecognized option, a bad numeric value, a
	calibration name length outside 1..=255, a thread count outside 2..=65535,
	a _PATH_ that is not an existing directory, or no _PATH_ at all.

# SEE ALSO

*find*(1), *du*(1), *stat*(2), *readdir*(3)

# AUTHORS

Maintained by Dinko Korunic <dinko.korunic@gmail.com>. Source, bug reports and
the complete option list: <https://github.com/dkorunic/findlargedir>.
