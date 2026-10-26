//! The mount table on macOS and FreeBSD, from `getfsstat(2)`.
//!
//! The BSDs list mounts in the order they were made, each with its source, mount point,
//! filesystem type and flags — no ids, no parents, no device numbers. [`entries`] gives them
//! what a [`MountTree`] reads: ids in that order, each one's parent the mount its mount point led
//! into when it was made, and a device number of its own, anonymous, so that a mount is matched
//! to a device by its source alone.

use std::path::PathBuf;

use crate::mountinfo::{DevNum, MountEntry};
use crate::resolve::MountTree;

/// A mount as `getfsstat` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mounted {
    /// `f_mntfromname`: a device node (`/dev/disk2s1`), or whatever else the filesystem names.
    pub source: String,
    /// `f_mntonname`.
    pub mount_point: PathBuf,
    /// `f_fstypename`: `apfs`, `msdos`, `ufs`, `msdosfs`, …
    pub fstype: String,
    /// `MNT_RDONLY` is set.
    pub read_only: bool,
}

/// The mounts, listed in the order they were made, as a mount table.
pub fn entries(mounts: Vec<Mounted>) -> Vec<MountEntry> {
    let mut entries: Vec<MountEntry> = Vec::with_capacity(mounts.len());
    for (id, mounted) in (1..).zip(mounts) {
        // What a lookup of the mount point ended in, among the mounts made before it.
        let parent_id =
            MountTree::new(&entries).mount_at(&mounted.mount_point).map_or(0, |parent| parent.id);
        entries.push(MountEntry {
            id,
            parent_id,
            devnum: DevNum::new(0, id),
            root: PathBuf::from("/"),
            mount_point: mounted.mount_point,
            options: if mounted.read_only { "ro" } else { "rw" }.into(),
            fstype: mounted.fstype,
            source: mounted.source,
            super_options: String::new(),
        });
    }
    entries
}

/// Reads the mounts with `MNT_NOWAIT`, which answers from what the kernel holds, so that an
/// unresponsive network filesystem can't stall it (only its statistics may be stale).
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
pub fn mount_table() -> std::io::Result<Vec<Mounted>> {
    use std::io;
    // Room for mounts made between counting them and reading them.
    const SPARE: usize = 8;
    // SAFETY: with no buffer, getfsstat only counts the mounts.
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    let mut room = usize::try_from(count).map_err(|_| io::Error::last_os_error())? + SPARE;
    loop {
        let mut buf: Vec<libc::statfs> = Vec::with_capacity(room);
        let size = room * std::mem::size_of::<libc::statfs>();
        // SAFETY: getfsstat writes whole records, at most `size` bytes of them, into the buffer,
        // which has room for that many, and returns how many it wrote.
        let filled = unsafe { libc::getfsstat(buf.as_mut_ptr(), size as _, libc::MNT_NOWAIT) };
        let filled = usize::try_from(filled).map_err(|_| io::Error::last_os_error())?;
        if filled < room {
            // SAFETY: the first `filled` records were written.
            unsafe { buf.set_len(filled) };
            return Ok(buf.iter().map(mounted).collect());
        }
        // A full buffer may have been cut short: ask again with more room.
        room *= 2;
    }
}

#[cfg(any(target_os = "macos", target_os = "freebsd"))]
fn mounted(fs: &libc::statfs) -> Mounted {
    use std::os::unix::ffi::OsStringExt;
    let text = |field: &[libc::c_char]| String::from_utf8_lossy(&c_bytes(field)).into_owned();
    Mounted {
        source: text(&fs.f_mntfromname),
        mount_point: PathBuf::from(std::ffi::OsString::from_vec(c_bytes(&fs.f_mntonname))),
        fstype: text(&fs.f_fstypename),
        read_only: flag_set(fs.f_flags, libc::MNT_RDONLY),
    }
}

/// Whether `flag` is among a mount's `f_flags`, 32 bits wide on macOS and 64 on FreeBSD.
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
fn flag_set(flags: impl Into<u64>, flag: libc::c_int) -> bool {
    let flags: u64 = flags.into();
    flags & flag as u64 != 0
}

/// A C string in a fixed-size field: up to its NUL, or all of it without one.
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
fn c_bytes(field: &[libc::c_char]) -> Vec<u8> {
    field.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn mounted(source: &str, at: &str) -> Mounted {
        Mounted {
            source: source.into(),
            mount_point: at.into(),
            fstype: "ufs".into(),
            read_only: false,
        }
    }

    /// Each mount's parent is the mount its mount point was in when it was made — the latest on
    /// a mount point covering the ones before — so the tree sees what a lookup sees.
    #[test]
    fn mounts_get_the_parents_a_lookup_would_find() {
        let entries = entries(vec![
            mounted("/dev/ada0p2", "/"),
            mounted("devfs", "/dev"),
            mounted("/dev/da0s1", "/media/a"),
            mounted("/dev/da1s1", "/media/a"),
            mounted("/dev/da2s1", "/media/a/inner"),
            mounted("/dev/da3s1", "/elsewhere/deep"),
        ]);
        let parents: Vec<(u32, u32)> = entries.iter().map(|e| (e.id, e.parent_id)).collect();
        assert_eq!(parents, [(1, 0), (2, 1), (3, 1), (4, 3), (5, 4), (6, 1)]);
        let tree = MountTree::new(&entries);
        assert_eq!(tree.mount_at(Path::new("/media/a/x")).map(|e| e.id), Some(4), "the top one");
        assert_eq!(tree.mount_at(Path::new("/media/a/inner/x")).map(|e| e.id), Some(5));
        assert!(!tree.is_reachable(&entries[2]), "covered by the second mount");
    }

    /// Every mount has a device number of its own, anonymous: devices are matched by source.
    #[test]
    fn device_numbers_are_anonymous_and_unique() {
        let entries = entries(vec![mounted("/dev/a", "/"), mounted("/dev/a", "/b")]);
        assert!(entries.iter().all(|e| e.devnum.is_anonymous()));
        assert_ne!(entries[0].devnum, entries[1].devnum);
    }

    #[test]
    fn what_a_mount_is_carries_over() {
        let ro = Mounted {
            fstype: "msdos".into(),
            read_only: true,
            ..mounted("/dev/disk4s1", "/Volumes/USB")
        };
        let [entry] = entries(vec![ro]).try_into().unwrap();
        assert_eq!(
            (
                entry.source.as_str(),
                entry.mount_point.as_path(),
                entry.fstype.as_str(),
                entry.root.as_path()
            ),
            ("/dev/disk4s1", Path::new("/Volumes/USB"), "msdos", Path::new("/"))
        );
        assert!(entry.read_only());
        assert!(!entries(vec![mounted("/dev/a", "/")])[0].read_only());
    }
}
