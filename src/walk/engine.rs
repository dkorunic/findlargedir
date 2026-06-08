// SPDX-FileCopyrightText: 2022 Dinko Korunic <dinko.korunic@gmail.com>
//
// SPDX-License-Identifier: MIT

//! Custom parallel directory walker: a `crossbeam-deque` work-stealing engine
//! over a `cfg`-split leaf (`unix`/`fallback`). Specialized for findlargedir —
//! it visits directories only, harvests one `statat` per directory for the
//! caller's heuristic, and never opens a subtree the classifier marks `Skip`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    pub size: u64,
}

struct Task {
    path: PathBuf,
    // Resolve a final symlink when opening/statting: true for command-line
    // roots and for symlinked dirs reached under --follow-symlinks.
    follow: bool,
    // (dev, ino) of each ancestor directory; Some only when following
    // symlinks, so the common path stays allocation-free.
    ancestors: Option<Arc<Vec<(u64, u64)>>>,
}

/// Walks `root` in parallel, calling `classify` exactly once per directory
/// (including `root`). A directory's children are enumerated and queued only
/// when the classifier returns `Decision::Descend`. Non-directory entries are
/// ignored; stat/open errors skip that directory. The walk stops early once
/// `shutdown` is set.
pub fn walk_dirs<C>(
    root: &Path,
    threads: usize,
    follow_symlinks: bool,
    shutdown: &AtomicBool,
    classify: C,
) where
    C: Fn(DirInfo) -> Decision + Sync,
{
    let n_workers = threads.saturating_sub(1).max(1);
    let injector = Injector::new();
    // Outstanding tasks (pushed but not yet fully processed); reaching 0 is
    // the termination signal.
    let pending = AtomicUsize::new(0);

    let ancestors = follow_symlinks.then(|| Arc::new(Vec::new()));
    pending.fetch_add(1, Ordering::SeqCst);
    injector.push(Task { path: root.to_path_buf(), follow: true, ancestors });

    let workers: Vec<Worker<Task>> =
        (0..n_workers).map(|_| Worker::new_lifo()).collect();
    let stealers: Vec<Stealer<Task>> =
        workers.iter().map(Worker::stealer).collect();

    thread::scope(|scope| {
        for worker in workers {
            let injector = &injector;
            let stealers = &stealers;
            let pending = &pending;
            let classify = &classify;
            scope.spawn(move || {
                run_worker(
                    &worker,
                    injector,
                    stealers,
                    pending,
                    shutdown,
                    follow_symlinks,
                    classify,
                );
            });
        }
    });
}

fn run_worker<C: Fn(DirInfo) -> Decision + Sync>(
    local: &Worker<Task>,
    injector: &Injector<Task>,
    stealers: &[Stealer<Task>],
    pending: &AtomicUsize,
    shutdown: &AtomicBool,
    follow_symlinks: bool,
    classify: &C,
) {
    let backoff = Backoff::new();
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        if let Some(task) = find_task(local, injector, stealers) {
            backoff.reset();
            process(&task, local, pending, follow_symlinks, classify);
            pending.fetch_sub(1, Ordering::SeqCst);
        } else {
            if pending.load(Ordering::SeqCst) == 0 {
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

fn process<C: Fn(DirInfo) -> Decision + Sync>(
    task: &Task,
    local: &Worker<Task>,
    pending: &AtomicUsize,
    follow_symlinks: bool,
    classify: &C,
) {
    // Decide from a single stat before opening — this is what lets a black-hole
    // subtree be skipped without ever reading it. A stat failure (permissions,
    // races) drops the directory: with no size there is nothing to classify, so
    // its subtree is not descended either.
    let Ok((dev, ino, size, is_dir)) =
        platform::stat_dir(&task.path, task.follow)
    else {
        return;
    };
    // A followed symlink may resolve to a non-directory; ignore it.
    if !is_dir {
        return;
    }

    // Symlink-cycle guard (only populated when following symlinks): a cyclic
    // path resolves to an ancestor directory, so bail before classifying it —
    // classify must fire exactly once per physical directory.
    if let Some(anc) = &task.ancestors
        && anc.contains(&(dev, ino))
    {
        return;
    }

    if let Decision::Skip = classify(DirInfo { path: &task.path, dev, size }) {
        return;
    }

    let Ok(dir) = platform::open_dir(&task.path, task.follow) else {
        return;
    };

    let child_ancestors = task.ancestors.as_ref().map(|a| {
        let mut v = (**a).clone();
        v.push((dev, ino));
        Arc::new(v)
    });

    let _ = platform::for_each_entry(dir, &task.path, |path, kind| {
        let kind = match kind {
            Some(k) => k,
            // DT_UNKNOWN: resolve the entry's own type; skip on failure.
            None => match platform::lstat_kind(&path) {
                Ok(k) => k,
                Err(_) => return,
            },
        };
        let follow = match kind {
            ChildKind::Dir => false,
            ChildKind::Symlink if follow_symlinks => true,
            // Plain files, sockets, etc., and unfollowed symlinks: skip.
            _ => return,
        };
        pending.fetch_add(1, Ordering::SeqCst);
        local.push(Task { path, follow, ancestors: child_ancestors.clone() });
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tempfile::TempDir;

    use super::*;

    fn collect(root: &Path, threads: usize, follow: bool) -> Vec<PathBuf> {
        let sink = Mutex::new(Vec::new());
        let shutdown = AtomicBool::new(false);
        walk_dirs(root, threads, follow, &shutdown, |info| {
            sink.lock().unwrap().push(info.path.to_path_buf());
            Decision::Descend
        });
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
        walk_dirs(tmp.path(), 4, false, &shutdown, |info| {
            sink.lock().unwrap().push(info.path.to_path_buf());
            if info.path.ends_with("big") {
                Decision::Skip
            } else {
                Decision::Descend
            }
        });
        let got = sink.into_inner().unwrap();
        assert!(got.iter().any(|p| p.ends_with("big")));
        // The subtree under a Skip is never opened.
        assert!(!got.iter().any(|p| p.ends_with("child")));
    }

    #[test]
    fn shutdown_stops_walk_early() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("a")).unwrap();
        let sink = Mutex::new(Vec::new());
        let shutdown = AtomicBool::new(true);
        walk_dirs(tmp.path(), 4, false, &shutdown, |info| {
            sink.lock().unwrap().push(info.path.to_path_buf());
            Decision::Descend
        });
        // Workers see shutdown at the top of the loop and return at once.
        assert!(sink.into_inner().unwrap().is_empty());
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
}
