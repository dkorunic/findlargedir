// SPDX-FileCopyrightText: 2022 Dinko Korunic <dinko.korunic@gmail.com>
//
// SPDX-License-Identifier: MIT

//! Non-Unix leaf I/O for the walker, built on std::fs.

use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use super::ChildKind;

pub(crate) struct DirHandle(std::fs::ReadDir);

/// Mirrors the unix leaf's anchor context. Unused here: std has no stable
/// dirfd API, so >PATH_MAX traversal is unsupported on this platform and such
/// directories keep failing into the engine's error counters.
#[allow(dead_code)]
pub(crate) struct RootCtx<'a> {
    pub fd: Option<&'a ()>,
    pub path: &'a Path,
}

/// Mirrors the unix leaf; nothing to anchor on this platform.
pub(crate) fn open_root(_root: &Path) -> io::Result<()> {
    Ok(())
}

fn path_hash(path: &Path) -> u64 {
    let canon =
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut h = std::collections::hash_map::DefaultHasher::new();
    canon.hash(&mut h);
    h.finish()
}

/// `(dev=0, ino=path-hash, size, is_dir)`. `size` is `Metadata::len()`, which
/// for a directory bears no relation to inode allocation size, so findlargedir's
/// byte-size heuristic is effectively inert on non-unix (a documented limitation).
pub(crate) fn stat_dir(
    path: &Path,
    follow: bool,
    _root: &RootCtx,
) -> io::Result<(u64, u64, u64, bool)> {
    let md = if follow {
        std::fs::metadata(path)?
    } else {
        std::fs::symlink_metadata(path)?
    };
    Ok((0, path_hash(path), md.len(), md.is_dir()))
}

pub(crate) fn open_dir(
    path: &Path,
    _follow: bool,
    _root: &RootCtx,
) -> io::Result<DirHandle> {
    Ok(DirHandle(std::fs::read_dir(path)?))
}

/// Returns the entry count alongside the enumeration result, invoking `f` per
/// traversable entry and polling `shutdown` per entry for an early stop;
/// `ReadDir` already omits `.`/`..`. See the unix leaf for the full contract.
pub(crate) fn for_each_entry(
    d: DirHandle,
    parent: &Path,
    shutdown: &AtomicBool,
    mut f: impl FnMut(PathBuf, Option<ChildKind>),
) -> (u64, io::Result<()>) {
    let mut count = 0;
    for entry in d.0 {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => return (count, Err(e)),
        };
        count += 1;
        let kind = entry.file_type().ok().map(map_type);
        // Mirror the unix leaf: don't allocate child paths for entries the
        // walker never traverses.
        if matches!(kind, Some(ChildKind::Other)) {
            continue;
        }
        f(parent.join(entry.file_name()), kind);
    }
    (count, Ok(()))
}

pub(crate) fn lstat_kind(
    path: &Path,
    _root: &RootCtx,
) -> io::Result<ChildKind> {
    Ok(map_type(std::fs::symlink_metadata(path)?.file_type()))
}

/// Exact live entry count of `path` (no descent). Mirrors the unix leaf; a
/// mid-read error is propagated for the caller to fall back to the estimate.
pub(crate) fn count_entries(
    path: &Path,
    _follow: bool,
    _root: &RootCtx,
    _shutdown: &AtomicBool,
) -> io::Result<u64> {
    let mut count = 0;
    for entry in std::fs::read_dir(path)? {
        entry?;
        count += 1;
    }
    Ok(count)
}

fn map_type(ft: std::fs::FileType) -> ChildKind {
    if ft.is_dir() {
        ChildKind::Dir
    } else if ft.is_symlink() {
        ChildKind::Symlink
    } else {
        ChildKind::Other
    }
}
