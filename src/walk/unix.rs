// SPDX-FileCopyrightText: 2022 Dinko Korunic <dinko.korunic@gmail.com>
//
// SPDX-License-Identifier: MIT

//! Unix leaf I/O for the walker: getdents/statat-based via rustix.

use std::ffi::OsStr;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{self, AtFlags, CWD, FileType as RFileType, Mode, OFlags};

use super::ChildKind;

/// An open directory file descriptor.
pub(crate) struct DirHandle(OwnedFd);

/// `(dev, ino, size, is_dir)` of `path`. `follow` resolves a final symlink.
// rustix's `Stat` field types are platform-dependent: the `as u64` casts are
// necessary on some targets (signed `st_size`) and redundant on others
// (`st_dev`/`st_ino` already `u64` on Linux), so allow the whole cast family.
#[allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::unnecessary_cast
)]
pub(crate) fn stat_dir(
    path: &Path,
    follow: bool,
) -> io::Result<(u64, u64, u64, bool)> {
    let flags =
        if follow { AtFlags::empty() } else { AtFlags::SYMLINK_NOFOLLOW };
    let st = fs::statat(CWD, path, flags)?;
    let is_dir = RFileType::from_raw_mode(st.st_mode) == RFileType::Directory;
    Ok((st.st_dev as u64, st.st_ino as u64, st.st_size as u64, is_dir))
}

/// Opens `path` as a directory. `follow` controls whether a final symlink is
/// resolved (true) or rejected with `O_NOFOLLOW` (false).
pub(crate) fn open_dir(path: &Path, follow: bool) -> io::Result<DirHandle> {
    let mut flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    if !follow {
        flags |= OFlags::NOFOLLOW;
    }
    let fd = fs::openat(CWD, path, flags, Mode::empty())?;
    Ok(DirHandle(fd))
}

/// Invokes `f` for each directory/symlink entry with its full path — built
/// directly from the raw name — and its `d_type` (`None` on `DT_UNKNOWN`).
/// Plain non-traversable entries (files, sockets, …) are filtered out here so
/// their child paths are never allocated; `.`/`..` are skipped.
pub(crate) fn for_each_entry(
    d: DirHandle,
    parent: &Path,
    mut f: impl FnMut(PathBuf, Option<ChildKind>),
) -> io::Result<()> {
    // `Dir::new` adopts the fd directly; `read_from` would fcntl+openat a
    // fresh one (two extra syscalls per directory).
    let dir = fs::Dir::new(d.0)?;
    for entry in dir {
        let entry = entry?;
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let kind = map_type(entry.file_type());
        // Skip entries the walker never traverses before paying for the path
        // allocation; `None` (DT_UNKNOWN) is resolved by the caller via lstat.
        if matches!(kind, Some(ChildKind::Other)) {
            continue;
        }
        f(parent.join(OsStr::from_bytes(bytes)), kind);
    }
    Ok(())
}

/// Resolves a `DT_UNKNOWN` entry's own type via lstat.
pub(crate) fn lstat_kind(path: &Path) -> io::Result<ChildKind> {
    let st = fs::statat(CWD, path, AtFlags::SYMLINK_NOFOLLOW)?;
    Ok(map_type(RFileType::from_raw_mode(st.st_mode))
        .unwrap_or(ChildKind::Other))
}

fn map_type(ft: RFileType) -> Option<ChildKind> {
    match ft {
        RFileType::Directory => Some(ChildKind::Dir),
        RFileType::Symlink => Some(ChildKind::Symlink),
        RFileType::Unknown => None,
        _ => Some(ChildKind::Other),
    }
}
