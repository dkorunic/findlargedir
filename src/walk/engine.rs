// SPDX-FileCopyrightText: 2022 Dinko Korunic <dinko.korunic@gmail.com>
//
// SPDX-License-Identifier: MIT

//! Custom parallel directory walker: a `crossbeam-deque` work-stealing engine
//! over a `cfg`-split leaf (`unix`/`fallback`). Specialized for findlargedir —
//! it visits directories only, harvests one `statat` per directory for the
//! caller's heuristic, and never opens a subtree the classifier marks `Skip`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread;

use crossbeam_deque::{Injector, Steal, Stealer, Worker};
use crossbeam_utils::Backoff;

#[cfg(unix)]
#[path = "unix.rs"]
mod platform;
#[cfg(not(unix))]
#[path = "fallback.rs"]
mod platform;

/// Kind of a child directory entry relevant to directory-only traversal.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ChildKind {
    Dir,
    Symlink,
    Other,
}

/// Whether to enumerate a directory's children or skip its subtree entirely.
pub enum Decision {
    Descend,
    Skip,
}

/// A directory presented to the classifier, with its harvested `statat` data.
pub struct DirInfo<'a> {
    pub path: &'a Path,
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    /// Live entry counter wired to the walk's leaf; see
    /// [`DirInfo::count_entries`].
    pub count: &'a dyn Fn(&Path) -> io::Result<u64>,
}

impl DirInfo<'_> {
    /// Exact live entry count of this directory (no descent), via the walk's
    /// own leaf with its symlink semantics (`O_NOFOLLOW` when not following):
    /// a directory swapped for a symlink mid-scan errors out instead of
    /// counting the wrong tree (TOCTOU). A mid-read error is propagated for
    /// the caller to fall back to the size estimate.
    pub fn count_entries(&self) -> io::Result<u64> {
        (self.count)(self.path)
    }
}

/// Directories the walk had to drop, counted so under-scanning is visible to
/// the caller instead of silent (permissions, fd exhaustion, races). Written
/// by workers, read after the thread scope joins; relaxed ordering suffices.
#[derive(Default)]
pub struct WalkStats {
    /// `stat` failed: with no size there is nothing to classify, so the
    /// directory and its whole subtree were dropped.
    pub stat_errors: AtomicU64,
    /// `open` failed after classification said descend: subtree dropped.
    pub open_errors: AtomicU64,
    /// `getdents` failed mid-enumeration: the directory was reported on a
    /// partial count, which may understate a real offender.
    pub read_errors: AtomicU64,
    /// Filesystem loops cut by the ancestor chain (bind mounts nested in
    /// their own source, symlink cycles under --follow-symlinks): the cyclic
    /// path was dropped without re-walking it. Not an error per se, but
    /// invisible loops — and false positives from duplicate-inode
    /// filesystems like overlayfs — must not go unnoticed either.
    pub loop_hits: AtomicU64,
}

struct Task {
    path: PathBuf,
    // Resolve a final symlink when opening/statting: true for command-line
    // roots and for symlinked dirs reached under --follow-symlinks.
    follow: bool,
    // (dev, ino) of each ancestor directory, carried unconditionally:
    // filesystem loops arise without symlinks too (a bind mount nested in
    // its own source shares dev+ino with its ancestor), and an unguarded
    // loop re-scans the aliased tree at every level until ENAMETOOLONG.
    // A chain, not a global set — non-cyclic aliases of one directory are
    // still each walked, matching find(1).
    ancestors: Arc<Vec<(u64, u64)>>,
}

/// Scheduler context shared by reference across the scoped worker threads, so
/// each worker takes just its own queue plus a `&Shared`.
struct Shared<'a, C, R> {
    injector: &'a Injector<Task>,
    stealers: &'a [Stealer<Task>],
    pending: &'a AtomicUsize,
    shutdown: &'a AtomicBool,
    stats: &'a WalkStats,
    root: &'a platform::RootCtx<'a>,
    follow_symlinks: bool,
    classify: &'a C,
    report: &'a R,
}

/// Walks `root` in parallel, calling `classify` exactly once per directory
/// (including `root`) to decide — from a single `stat`, before reading — whether
/// to enumerate its children. For each directory that *is* enumerated (i.e.
/// `Decision::Descend`), `report` is then called once with the directory's
/// entry count, harvested for free from that same enumeration (partial if the
/// read fails mid-way). Skipped subtrees are never read, so `report` never
/// fires for them. Non-directory entries are ignored; stat/open failures drop
/// that directory and are counted in `stats`. Directories nested past
/// `PATH_MAX` are still reached on unix: the leaf falls back to component-wise
/// `openat` from the scan root's fd. The walk stops early once `shutdown` is
/// set, including mid-enumeration of a large directory.
pub fn walk_dirs<C, R>(
    root: &Path,
    threads: usize,
    follow_symlinks: bool,
    shutdown: &AtomicBool,
    stats: &WalkStats,
    classify: C,
    report: R,
) where
    C: Fn(DirInfo) -> Decision + Sync,
    R: Fn(&Path, u64, u64, u64) + Sync,
{
    // The thread budget is total, not worker-only: one thread is reserved
    // for coordination (calibration, output, progress), so N threads spawn
    // N-1 walk workers. Deliberate and documented in --help/the man page.
    let n_workers = threads.saturating_sub(1).max(1);
    let injector = Injector::new();
    // Outstanding tasks (pushed but not yet fully processed); reaching 0 is
    // the termination signal.
    let pending = AtomicUsize::new(0);

    pending.fetch_add(1, Ordering::SeqCst);
    injector.push(Task {
        path: root.to_path_buf(),
        follow: true,
        ancestors: Arc::new(Vec::new()),
    });

    let workers: Vec<Worker<Task>> =
        (0..n_workers).map(|_| Worker::new_lifo()).collect();
    let stealers: Vec<Stealer<Task>> =
        workers.iter().map(Worker::stealer).collect();

    // Anchor for the leaf's >PATH_MAX component-wise fallback. One fd held
    // per walk, best-effort: if the root can't be opened, the fallback is
    // unavailable and deep directories error out as usual. Queued tasks
    // deliberately hold no fds — a wide tree must not exhaust RLIMIT_NOFILE.
    let root_fd = platform::open_root(root).ok();
    let root_ctx = platform::RootCtx { fd: root_fd.as_ref(), path: root };

    let shared = Shared {
        injector: &injector,
        stealers: &stealers,
        pending: &pending,
        shutdown,
        stats,
        root: &root_ctx,
        follow_symlinks,
        classify: &classify,
        report: &report,
    };

    thread::scope(|scope| {
        for worker in workers {
            let shared = &shared;
            scope.spawn(move || run_worker(&worker, shared));
        }
    });
}

fn run_worker<C, R>(local: &Worker<Task>, shared: &Shared<C, R>)
where
    C: Fn(DirInfo) -> Decision + Sync,
    R: Fn(&Path, u64, u64, u64) + Sync,
{
    let backoff = Backoff::new();
    loop {
        if shared.shutdown.load(Ordering::Relaxed) {
            return;
        }
        if let Some(task) = find_task(local, shared.injector, shared.stealers)
        {
            backoff.reset();
            process(&task, local, shared);
            shared.pending.fetch_sub(1, Ordering::SeqCst);
        } else {
            if shared.pending.load(Ordering::SeqCst) == 0 {
                return;
            }
            backoff.snooze();
        }
    }
}

fn find_task(
    local: &Worker<Task>,
    injector: &Injector<Task>,
    stealers: &[Stealer<Task>],
) -> Option<Task> {
    local.pop().or_else(|| {
        std::iter::repeat_with(|| {
            injector
                .steal_batch_and_pop(local)
                .or_else(|| stealers.iter().map(Stealer::steal).collect())
        })
        .find(|s| !s.is_retry())
        .and_then(Steal::success)
    })
}

fn process<C, R>(task: &Task, local: &Worker<Task>, shared: &Shared<C, R>)
where
    C: Fn(DirInfo) -> Decision + Sync,
    R: Fn(&Path, u64, u64, u64) + Sync,
{
    // Decide from a single stat before opening — this is what lets a black-hole
    // subtree be skipped without ever reading it. A stat failure (permissions,
    // races) drops the directory: with no size there is nothing to classify, so
    // its subtree is not descended either. Counted so under-scanning surfaces.
    let Ok((dev, ino, size, is_dir)) =
        platform::stat_dir(&task.path, task.follow, shared.root)
    else {
        shared.stats.stat_errors.fetch_add(1, Ordering::Relaxed);
        return;
    };
    // A followed symlink may resolve to a non-directory; ignore it.
    if !is_dir {
        return;
    }

    // Filesystem-loop guard: a cyclic path (a symlink cycle under
    // --follow-symlinks, or a bind mount nested in its own source) resolves
    // to an ancestor directory, so bail before classifying it — classify
    // must fire exactly once per physical directory. Counted so loops (and
    // duplicate-inode false positives) surface instead of passing silently.
    if task.ancestors.contains(&(dev, ino)) {
        shared.stats.loop_hits.fetch_add(1, Ordering::Relaxed);
        return;
    }

    // Wired per directory so the classifier can count entries (accurate mode
    // for blacklisted dirs) through the leaf, with the walk's own follow
    // semantics and root-fd anchor.
    let count = |p: &Path| {
        platform::count_entries(p, task.follow, shared.root, shared.shutdown)
    };
    if let Decision::Skip = (shared.classify)(DirInfo {
        path: &task.path,
        dev,
        ino,
        size,
        count: &count,
    }) {
        return;
    }

    let Ok(dir) = platform::open_dir(&task.path, task.follow, shared.root)
    else {
        shared.stats.open_errors.fetch_add(1, Ordering::Relaxed);
        return;
    };

    let child_ancestors = {
        let mut chain = (*task.ancestors).clone();
        chain.push((dev, ino));
        Arc::new(chain)
    };

    // A read error mid-enumeration keeps the partial (under-)count: the
    // directory is still reported on what was read, and children already
    // queued continue to walk. The alternative — reporting 0 — would hide a
    // potential offender entirely; the error is counted so the under-count
    // is visible. The leaf polls shutdown per entry, so a huge directory
    // doesn't delay exit past its next entry.
    let (entries, read_result) = platform::for_each_entry(
        dir,
        &task.path,
        shared.shutdown,
        |path, kind| {
            let kind = match kind {
                Some(k) => k,
                // DT_UNKNOWN: resolve the entry's own type; skip on failure.
                None => match platform::lstat_kind(&path, shared.root) {
                    Ok(k) => k,
                    Err(_) => return,
                },
            };
            let follow = match kind {
                ChildKind::Dir => false,
                ChildKind::Symlink if shared.follow_symlinks => true,
                // Plain files, sockets, etc., and unfollowed symlinks: skip.
                _ => return,
            };
            shared.pending.fetch_add(1, Ordering::SeqCst);
            local.push(Task {
                path,
                follow,
                ancestors: child_ancestors.clone(),
            });
        },
    );

    if read_result.is_err() {
        shared.stats.read_errors.fetch_add(1, Ordering::Relaxed);
    }

    (shared.report)(&task.path, ino, size, entries);
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tempfile::TempDir;

    use super::*;

    fn collect(root: &Path, threads: usize, follow: bool) -> Vec<PathBuf> {
        let sink = Mutex::new(Vec::new());
        let shutdown = AtomicBool::new(false);
        walk_dirs(
            root,
            threads,
            follow,
            &shutdown,
            &WalkStats::default(),
            |info| {
                sink.lock().unwrap().push(info.path.to_path_buf());
                Decision::Descend
            },
            |_, _, _, _| {},
        );
        sink.into_inner().unwrap()
    }

    #[test]
    fn emits_root_and_descendant_dirs_only() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("a")).unwrap();
        std::fs::write(tmp.path().join("a/f.txt"), b"x").unwrap();
        std::fs::write(tmp.path().join("g.txt"), b"x").unwrap();
        let got = collect(tmp.path(), 4, false);
        assert!(got.iter().any(|p| p == tmp.path()));
        assert!(got.iter().any(|p| p.ends_with("a")));
        // Files are never visited.
        assert!(!got.iter().any(|p| p.ends_with("f.txt")));
        assert!(!got.iter().any(|p| p.ends_with("g.txt")));
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn terminates_on_empty_directory() {
        let tmp = TempDir::new().unwrap();
        let got = collect(tmp.path(), 4, false);
        assert_eq!(got, vec![tmp.path().to_path_buf()]);
    }

    /// `threads = 1` exercises the `max(1)` worker-count clamp: a single worker
    /// must still traverse the whole tree and terminate (no empty pool / hang).
    #[test]
    fn terminates_with_single_worker() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("a")).unwrap();
        let got = collect(tmp.path(), 1, false);
        assert!(got.iter().any(|p| p == tmp.path()));
        assert!(got.iter().any(|p| p.ends_with("a")));
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn terminates_on_deep_chain() {
        let tmp = TempDir::new().unwrap();
        let mut p = tmp.path().to_path_buf();
        for i in 0..50 {
            p = p.join(format!("d{i}"));
            std::fs::create_dir(&p).unwrap();
        }
        let got = collect(tmp.path(), 4, false);
        assert_eq!(got.len(), 51);
    }

    #[test]
    fn no_duplicate_or_lost_dirs() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("a")).unwrap();
        std::fs::create_dir(tmp.path().join("b")).unwrap();
        std::fs::write(tmp.path().join("a/x.txt"), b"x").unwrap();
        std::fs::write(tmp.path().join("c.txt"), b"x").unwrap();
        let mut got = collect(tmp.path(), 8, false);
        let mut expected = vec![
            tmp.path().to_path_buf(),
            tmp.path().join("a"),
            tmp.path().join("b"),
        ];
        got.sort();
        expected.sort();
        assert_eq!(got, expected, "each dir visited exactly once");
    }

    #[test]
    fn skip_prevents_descent() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("big/child")).unwrap();
        let sink = Mutex::new(Vec::new());
        let shutdown = AtomicBool::new(false);
        let reports = Mutex::new(Vec::new());
        walk_dirs(
            tmp.path(),
            4,
            false,
            &shutdown,
            &WalkStats::default(),
            |info| {
                sink.lock().unwrap().push(info.path.to_path_buf());
                if info.path.ends_with("big") {
                    Decision::Skip
                } else {
                    Decision::Descend
                }
            },
            |path, _, _, _| reports.lock().unwrap().push(path.to_path_buf()),
        );
        let got = sink.into_inner().unwrap();
        assert!(got.iter().any(|p| p.ends_with("big")));
        // The subtree under a Skip is never opened.
        assert!(!got.iter().any(|p| p.ends_with("child")));
        // A skipped directory is never enumerated, so it is never reported.
        assert!(
            !reports.into_inner().unwrap().iter().any(|p| p.ends_with("big"))
        );
    }

    #[test]
    fn shutdown_stops_walk_early() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("a")).unwrap();
        let sink = Mutex::new(Vec::new());
        let shutdown = AtomicBool::new(true);
        walk_dirs(
            tmp.path(),
            4,
            false,
            &shutdown,
            &WalkStats::default(),
            |info| {
                sink.lock().unwrap().push(info.path.to_path_buf());
                Decision::Descend
            },
            |_, _, _, _| {},
        );
        // Workers see shutdown at the top of the loop and return at once.
        assert!(sink.into_inner().unwrap().is_empty());
    }

    /// Shutdown observed mid-enumeration stops it at the next entry: no more
    /// children are queued and the count stays partial instead of running a
    /// huge directory to completion before exiting.
    #[test]
    fn shutdown_during_enumeration_stops_it() {
        let tmp = TempDir::new().unwrap();
        for i in 0..10 {
            std::fs::create_dir(tmp.path().join(format!("d{i}"))).unwrap();
        }
        let shutdown = AtomicBool::new(false);
        let reports = Mutex::new(Vec::new());
        walk_dirs(
            tmp.path(),
            2,
            false,
            &shutdown,
            &WalkStats::default(),
            |info| {
                // The only queued task is the root; flipping shutdown in its
                // classify makes its own enumeration abort immediately.
                if info.path == tmp.path() {
                    shutdown.store(true, Ordering::SeqCst);
                }
                Decision::Descend
            },
            |path, _, _, n| {
                reports.lock().unwrap().push((path.to_path_buf(), n));
            },
        );
        let got = reports.into_inner().unwrap();
        // Only the root was ever classified; its enumeration stopped after at
        // most the entry in flight when the flag landed, so no child was
        // visited.
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, tmp.path());
        assert!(got[0].1 <= 1);
    }

    /// A directory that cannot be opened (permission denied) is dropped but
    /// counted, so under-scanning is visible instead of silent. Skipped under
    /// root, which bypasses permission checks and would open it anyway.
    #[cfg(unix)]
    #[test]
    fn unopenable_dir_is_counted_in_stats() {
        use std::os::unix::fs::PermissionsExt;

        // SAFETY: geteuid is always safe to call.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let blocked = tmp.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::set_permissions(
            &blocked,
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();

        let stats = WalkStats::default();
        let shutdown = AtomicBool::new(false);
        walk_dirs(
            tmp.path(),
            2,
            false,
            &shutdown,
            &stats,
            |_| Decision::Descend,
            |_, _, _, _| {},
        );
        // Restore so the TempDir cleanup can't trip on the mode.
        std::fs::set_permissions(
            &blocked,
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        assert_eq!(stats.open_errors.load(Ordering::Relaxed), 1);
        assert_eq!(stats.stat_errors.load(Ordering::Relaxed), 0);
    }

    /// Every descended directory is reported with its real inode number and its
    /// exact live entry count (files + subdirs), both harvested for free from the
    /// `stat` + enumeration the walk already performs.
    #[test]
    fn reports_inode_and_exact_count_per_descended_dir() {
        use std::os::unix::fs::MetadataExt;

        let tmp = TempDir::new().unwrap();
        for f in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(tmp.path().join(f), b"x").unwrap();
        }
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub/d.txt"), b"x").unwrap();
        std::fs::write(tmp.path().join("sub/e.txt"), b"x").unwrap();

        let reports = Mutex::new(Vec::new());
        let shutdown = AtomicBool::new(false);
        walk_dirs(
            tmp.path(),
            4,
            false,
            &shutdown,
            &WalkStats::default(),
            |_| Decision::Descend,
            |path, ino, _, n| {
                reports.lock().unwrap().push((path.to_path_buf(), ino, n));
            },
        );

        let got = reports.into_inner().unwrap();
        let root_ino = std::fs::metadata(tmp.path()).unwrap().ino();
        // root: 3 files + 1 subdir = 4 entries, with its real inode; sub: 2.
        assert!(got.iter().any(|(p, ino, n)| p == tmp.path()
            && *ino == root_ino
            && *n == 4));
        assert!(got.iter().any(|(p, _, n)| p.ends_with("sub") && *n == 2));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_root_is_traversed() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::create_dir(real.join("inside")).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // A command-line root that is a symlink-to-dir is descended even with
        // follow=false (find(1) behavior).
        let got = collect(&link, 4, false);
        assert!(got.iter().any(|p| p.ends_with("inside")));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_dir_not_followed_by_default() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::create_dir(real.join("inside")).unwrap();
        std::os::unix::fs::symlink(&real, tmp.path().join("link")).unwrap();
        let got = collect(tmp.path(), 4, false);
        // Nothing is reached *through* the link path.
        assert!(!got.iter().any(|p| p.starts_with(tmp.path().join("link"))));
    }

    #[cfg(unix)]
    #[test]
    fn follow_symlinks_descends_into_symlinked_dir() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::create_dir(real.join("inside")).unwrap();
        std::os::unix::fs::symlink(&real, tmp.path().join("link")).unwrap();
        let got = collect(tmp.path(), 4, true);
        assert!(
            got.iter().any(|p| p.starts_with(tmp.path().join("link"))
                && p.ends_with("inside")),
            "follow_symlinks must descend through the link"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_cycle_does_not_hang() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        std::fs::create_dir(&a).unwrap();
        std::os::unix::fs::symlink(&a, a.join("loop")).unwrap();
        let got = collect(tmp.path(), 4, true);
        assert!(got.iter().any(|p| p.ends_with("a")));
        // The cyclic symlink resolves to an ancestor; it must not be
        // classified as a separate directory.
        assert!(!got.iter().any(|p| p.ends_with("loop")));
        assert_eq!(got.len(), 2, "only the tmp root and `a` are visited");
    }

    /// A cut filesystem loop is counted: invisible loops (or duplicate-inode
    /// false positives) would otherwise silently skip real subtrees.
    #[cfg(unix)]
    #[test]
    fn cut_loops_are_counted() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        std::fs::create_dir(&a).unwrap();
        std::os::unix::fs::symlink(&a, a.join("loop")).unwrap();
        let stats = WalkStats::default();
        let shutdown = AtomicBool::new(false);
        walk_dirs(
            tmp.path(),
            4,
            true,
            &shutdown,
            &stats,
            |_| Decision::Descend,
            |_, _, _, _| {},
        );
        assert_eq!(stats.loop_hits.load(Ordering::Relaxed), 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_file_root_yields_nothing() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), b"x").unwrap();
        let link = tmp.path().join("flink");
        std::os::unix::fs::symlink(tmp.path().join("f.txt"), &link).unwrap();
        // A root that resolves to a non-directory is ignored entirely.
        let got = collect(&link, 4, false);
        assert!(got.is_empty());
    }

    /// Directories nested deeper than `PATH_MAX` stay traversable: the leaf
    /// falls back to component-wise `openat` from the scan root's fd. The
    /// fixture itself must be built with `mkdirat` (the full path is
    /// unusable), and cleanup shells out to `rm -rf` (fts, dirfd-relative) —
    /// `TempDir`'s recursive removal can't express the deep path.
    #[cfg(unix)]
    #[test]
    fn traverses_beyond_path_max() {
        use rustix::fs::{self as rfs, CWD, Mode, OFlags};

        let tmp = TempDir::new().unwrap();
        let dir_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let mut fd =
            rfs::openat(CWD, tmp.path(), dir_flags, Mode::empty()).unwrap();
        let mut full = tmp.path().to_path_buf();
        // 230 levels of 20-char names: full path ≈ 4.8k, past PATH_MAX on
        // both Linux (4096) and macOS (1024).
        for i in 0..230u32 {
            let name = format!("d{i:017}");
            rfs::mkdirat(&fd, &name, Mode::from_raw_mode(0o755)).unwrap();
            fd = rfs::openat(&fd, &name, dir_flags, Mode::empty()).unwrap();
            full.push(&name);
        }
        drop(fd);

        let got = collect(tmp.path(), 2, false);
        assert_eq!(got.len(), 231, "root plus 230 nested levels");
        assert!(
            got.iter().any(|p| p == &full),
            "deepest directory must be reached"
        );

        assert!(
            std::process::Command::new("rm")
                .args(["-rf", tmp.path().to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
    }
}
