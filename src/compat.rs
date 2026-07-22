// SPDX-FileCopyrightText: 2022 Dinko Korunic <dinko.korunic@gmail.com>
//
// SPDX-License-Identifier: MIT

//! Platform shims for the two `MetadataExt` uses stable std can't express
//! off-unix. Everything else platform-specific lives behind the walk's
//! `cfg`-split leaf (`walk/engine.rs` `platform`).

use std::fs::Metadata;

/// Device id for filesystem-boundary comparisons. Off-unix stable std has no
/// portable device id; 0 makes every directory "same device", so
/// one-filesystem mode and the `-t` device check become inert — the
/// documented non-unix limitation.
#[cfg(unix)]
pub fn dev(md: &Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::dev(md)
}

/// Off-unix: no portable device id, see the unix variant's docs.
#[cfg(not(unix))]
pub fn dev(_md: &Metadata) -> u64 {
    0
}

/// Directory size in bytes for calibration sampling (`st_size`). Off-unix
/// `len()` bears no fixed relation to entry count, so the fit goes degenerate
/// and size-based flagging disables itself — the documented non-unix
/// limitation.
#[cfg(unix)]
pub fn dir_size(md: &Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::size(md)
}

/// Off-unix: best available stand-in for `st_size`, see the unix variant's
/// docs.
#[cfg(not(unix))]
pub fn dir_size(md: &Metadata) -> u64 {
    md.len()
}

#[cfg(test)]
mod tests {
    use std::fs::Metadata;

    use super::{dev, dir_size};

    /// The shims return something for any real directory without panicking;
    /// platform-specific values are asserted in the unix leaf tests.
    #[test]
    fn shims_work_on_any_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let md: Metadata = std::fs::metadata(tmp.path()).unwrap();
        assert!(dir_size(&md) < u64::MAX);
        let _ = dev(&md);
    }
}
