// SPDX-FileCopyrightText: 2022 Dinko Korunic <dinko.korunic@gmail.com>
//
// SPDX-License-Identifier: MIT

//! Non-Unix leaf I/O for the walker, built on std::fs.

use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};

use super::ChildKind;

pub(crate) struct DirHandle(std::fs::ReadDir);

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
) -> io::Result<(u64, u64, u64, bool)> {
    let md = if follow {
        std::fs::metadata(path)?
    } else {
        std::fs::symlink_metadata(path)?
    };
    Ok((0, path_hash(path), md.len(), md.is_dir()))
}

pub(crate) fn open_dir(path: &Path, _follow: bool) -> io::Result<DirHandle> {
    Ok(DirHandle(std::fs::read_dir(path)?))
}

pub(crate) fn for_each_entry(
    d: DirHandle,
    parent: &Path,
    mut f: impl FnMut(PathBuf, Option<ChildKind>),
) -> io::Result<()> {
    for entry in d.0 {
        let entry = entry?;
        let kind = entry.file_type().ok().map(map_type);
        // Mirror the unix leaf: don't allocate child paths for entries the
        // walker never traverses.
        if matches!(kind, Some(ChildKind::Other)) {
            continue;
        }
        f(parent.join(entry.file_name()), kind);
    }
    Ok(())
}

pub(crate) fn lstat_kind(path: &Path) -> io::Result<ChildKind> {
    Ok(map_type(std::fs::symlink_metadata(path)?.file_type()))
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
