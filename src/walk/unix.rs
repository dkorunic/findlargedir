// SPDX-FileCopyrightText: 2022 Dinko Korunic <dinko.korunic@gmail.com>
//
// SPDX-License-Identifier: MIT

//! Unix leaf I/O for the walker: getdents/statat-based via rustix.
//!
//! Paths longer than `PATH_MAX` (a per-syscall limit, not a filesystem one)
//! are handled by a fallback that opens the directory component by component
//! from the scan root's fd — each component name is bounded by `NAME_MAX`,
//! the only length the kernel checks per `openat`. The fallback costs O(depth)
//! transient opens per deep directory and engages only on `ENAMETOOLONG`, so
//! the common path stays at one `statat` + one `openat` per directory.

use std::ffi::OsStr;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use rustix::fs::{self, AtFlags, CWD, FileType as RFileType, Mode, OFlags};
use rustix::io::Errno;

use super::ChildKind;

/// An open directory file descriptor.
pub(crate) struct DirHandle(OwnedFd);

/// Anchor for the >`PATH_MAX` fallback: the walk root's open fd (held for the
/// whole walk) plus its path, for computing root-relative descendant paths.
pub(crate) struct RootCtx<'a> {
    pub fd: Option<&'a OwnedFd>,
    pub path: &'a Path,
}

/// Opens the walk root for [`RootCtx`], resolving a final symlink (a
/// command-line root that is a symlink-to-dir is descended, find(1)
/// behavior). Best-effort: if the root can't be opened, the deep fallback is
/// unavailable and deep directories error out as before.
pub(crate) fn open_root(root: &Path) -> io::Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    Ok(fs::openat(CWD, root, flags, Mode::empty())?)
}

/// `(dev, ino, size, is_dir)` of `path`. `follow` resolves a final symlink.
/// Falls back to component-wise descent from the root fd past `PATH_MAX`.
pub(crate) fn stat_dir(
    path: &Path,
    follow: bool,
    root: &RootCtx,
) -> io::Result<(u64, u64, u64, bool)> {
    let flags =
        if follow { AtFlags::empty() } else { AtFlags::SYMLINK_NOFOLLOW };
    match fs::statat(CWD, path, flags) {
        Ok(st) => Ok(stat_tuple(&st)),
        Err(Errno::NAMETOOLONG) => {
            Ok(stat_tuple(&fstat_deep(path, follow, root)?))
        }
        Err(e) => Err(e.into()),
    }
}

/// Opens `path` as a directory. `follow` controls whether a final symlink is
/// resolved (true) or rejected with `O_NOFOLLOW` (false). Falls back to
/// component-wise descent from the root fd past `PATH_MAX`.
pub(crate) fn open_dir(
    path: &Path,
    follow: bool,
    root: &RootCtx,
) -> io::Result<DirHandle> {
    let mut flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    if !follow {
        flags |= OFlags::NOFOLLOW;
    }
    match fs::openat(CWD, path, flags, Mode::empty()) {
        Ok(fd) => Ok(DirHandle(fd)),
        Err(Errno::NAMETOOLONG) => {
            let (root_fd, rel) = deep_anchor(path, root)?;
            Ok(DirHandle(open_chain(root_fd, rel, flags)?))
        }
        Err(e) => Err(e.into()),
    }
}

// rustix's `Stat` field types are platform-dependent: the `as u64` casts are
// necessary on some targets (signed `st_size`) and redundant on others
// (`st_dev`/`st_ino` already `u64` on Linux), so allow the whole cast family.
#[allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::unnecessary_cast
)]
fn stat_tuple(st: &fs::Stat) -> (u64, u64, u64, bool) {
    let is_dir = RFileType::from_raw_mode(st.st_mode) == RFileType::Directory;
    (st.st_dev as u64, st.st_ino as u64, st.st_size as u64, is_dir)
}

/// Root fd and root-relative path for the deep fallback. Unavailable (no root
/// fd — e.g. the root itself failed to open) or a non-descendant path maps to
/// an error, letting the caller's normal failure path count it.
fn deep_anchor<'a>(
    path: &'a Path,
    root: &RootCtx<'a>,
) -> io::Result<(&'a OwnedFd, &'a Path)> {
    let root_fd =
        root.fd.ok_or_else(|| io::Error::from(Errno::NAMETOOLONG))?;
    let rel = path
        .strip_prefix(root.path)
        .map_err(|_| io::Error::from(Errno::INVAL))?;
    Ok((root_fd, rel))
}

/// Flags for intermediate chain components: `O_PATH` where it exists (needs
/// no read permission on transit directories, only search), else a plain
/// read-only directory fd.
#[cfg(target_os = "linux")]
fn deep_link_flags() -> OFlags {
    OFlags::PATH | OFlags::CLOEXEC
}

#[cfg(not(target_os = "linux"))]
fn deep_link_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC
}

/// stat of a >`PATH_MAX` entry: reach it component-wise from the root fd and
/// `fstat` the result. `follow` controls `O_NOFOLLOW` on the final component.
fn fstat_deep(
    path: &Path,
    follow: bool,
    root: &RootCtx,
) -> io::Result<fs::Stat> {
    let (root_fd, rel) = deep_anchor(path, root)?;
    let mut flags = deep_link_flags();
    if !follow {
        flags |= OFlags::NOFOLLOW;
    }
    let fd = open_chain(root_fd, rel, flags)?;
    Ok(fs::fstat(&fd)?)
}

/// Opens `rel` (a root-relative descendant path) component by component from
/// `root_fd`: per-component names are bounded by `NAME_MAX`, so directories
/// deeper than `PATH_MAX` stay reachable. Intermediate components resolve
/// symlinks, matching kernel path resolution; the final component takes
/// `final_flags`. All fds but the result are closed on return.
fn open_chain(
    root_fd: &OwnedFd,
    rel: &Path,
    final_flags: OFlags,
) -> io::Result<OwnedFd> {
    let mut cur: Option<OwnedFd> = None;
    let mut comps = rel.components().peekable();
    while let Some(comp) = comps.next() {
        let name = match comp {
            Component::Normal(c) => c,
            Component::CurDir => continue,
            // A root-relative descendant has no prefix/root/parent components.
            _ => return Err(io::Error::from(Errno::INVAL)),
        };
        let flags = if comps.peek().is_some() {
            deep_link_flags()
        } else {
            final_flags
        };
        let dirfd = cur.as_ref().unwrap_or(root_fd);
        cur = Some(fs::openat(dirfd, name, flags, Mode::empty())?);
    }
    match cur {
        Some(fd) => Ok(fd),
        // `rel` is the root itself.
        None => Ok(root_fd.try_clone()?),
    }
}

/// Invokes `f` for each directory/symlink entry with its full path — built
/// directly from the raw name — and its `d_type` (`None` on `DT_UNKNOWN`).
/// Plain non-traversable entries (files, sockets, …) are filtered out here so
/// their child paths are never allocated; `.`/`..` are skipped. `shutdown` is
/// polled per entry — including the file entries that never reach `f`, which
/// dominate a black hole — and stops the enumeration early. Returns the entry
/// count (every name bar `.`/`..`, traversable or not) — the exact live
/// count, harvested from the `getdents` pass the descent already pays for —
/// alongside the enumeration result. On a mid-read error or an early stop the
/// count covers the entries read up to that point, so the caller reports a
/// partial count rather than nothing.
pub(crate) fn for_each_entry(
    d: DirHandle,
    parent: &Path,
    shutdown: &AtomicBool,
    mut f: impl FnMut(PathBuf, Option<ChildKind>),
) -> (u64, io::Result<()>) {
    // `Dir::new` adopts the fd directly; `read_from` would fcntl+openat a
    // fresh one (two extra syscalls per directory).
    let dir = match fs::Dir::new(d.0) {
        Ok(dir) => dir,
        Err(e) => return (0, Err(e.into())),
    };
    let mut count = 0;
    for entry in dir {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => return (count, Err(e.into())),
        };
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        count += 1;
        let kind = map_type(entry.file_type());
        // Skip entries the walker never traverses before paying for the path
        // allocation; `None` (DT_UNKNOWN) is resolved by the caller via lstat.
        if matches!(kind, Some(ChildKind::Other)) {
            continue;
        }
        f(parent.join(OsStr::from_bytes(bytes)), kind);
    }
    (count, Ok(()))
}

/// Resolves a `DT_UNKNOWN` entry's own type via lstat, with the same
/// >`PATH_MAX` fallback as [`stat_dir`].
pub(crate) fn lstat_kind(
    path: &Path,
    root: &RootCtx,
) -> io::Result<ChildKind> {
    let st = match fs::statat(CWD, path, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => st,
        Err(Errno::NAMETOOLONG) => fstat_deep(path, false, root)?,
        Err(e) => return Err(e.into()),
    };
    Ok(map_type(RFileType::from_raw_mode(st.st_mode))
        .unwrap_or(ChildKind::Other))
}

/// Exact live entry count of `path`, via the same open + getdents pass the
/// walk uses (no descent). `follow` carries the walk's symlink semantics:
/// with `O_NOFOLLOW` a directory swapped for a symlink mid-scan errors out
/// instead of counting the wrong tree (TOCTOU). A mid-read error is
/// propagated for the caller to fall back to the size estimate.
pub(crate) fn count_entries(
    path: &Path,
    follow: bool,
    root: &RootCtx,
    shutdown: &AtomicBool,
) -> io::Result<u64> {
    let dir = open_dir(path, follow, root)?;
    let (count, res) = for_each_entry(dir, path, shutdown, |_, _| {});
    res.map(|()| count)
}

fn map_type(ft: RFileType) -> Option<ChildKind> {
    match ft {
        RFileType::Directory => Some(ChildKind::Dir),
        RFileType::Symlink => Some(ChildKind::Symlink),
        RFileType::Unknown => None,
        _ => Some(ChildKind::Other),
    }
}
