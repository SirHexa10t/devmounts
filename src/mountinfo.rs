//! Parser for `/proc/<pid>/mountinfo` (see `proc_pid_mountinfo(5)`).
//!
//! ```text
//! 36 35 98:0 /mnt1 /mnt/parent rw,noatime master:1 - ext3 /dev/root rw,errors=continue
//! (1)(2)(3)   (4)   (5)         (6)       (7)     (8) (9)   (10)      (11)
//! ```

use std::ffi::OsString;
use std::fmt;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use serde::Serialize;

/// A device number, as in `stat`'s `st_dev`/`st_rdev` or sysfs `dev` files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct DevNum {
    pub major: u32,
    pub minor: u32,
}

impl DevNum {
    pub const fn new(major: u32, minor: u32) -> Self {
        DevNum { major, minor }
    }

    /// Splits a `dev_t` the way glibc's `major()`/`minor()` do.
    pub const fn from_dev_t(dev: u64) -> Self {
        DevNum {
            major: (((dev >> 32) & 0xffff_f000) | ((dev >> 8) & 0x0000_0fff)) as u32,
            minor: (((dev >> 12) & 0xffff_ff00) | (dev & 0x0000_00ff)) as u32,
        }
    }

    /// Parses `"8:17"`.
    pub fn parse(text: &str) -> Option<Self> {
        let (major, minor) = text.trim().split_once(':')?;
        Some(DevNum { major: major.parse().ok()?, minor: minor.parse().ok()? })
    }

    /// Anonymous device numbers (major 0) are used by btrfs, FUSE, tmpfs, …, so they never
    /// equal the number of the block device a filesystem lives on.
    pub const fn is_anonymous(self) -> bool {
        self.major == 0
    }
}

impl fmt::Display for DevNum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.major, self.minor)
    }
}

/// One line of the mount table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MountEntry {
    pub id: u32,
    pub parent_id: u32,
    /// `st_dev` of files in this filesystem (anonymous for btrfs, FUSE, …).
    pub devnum: DevNum,
    /// Directory of the filesystem that is mounted here: `/` normally, a subdirectory for
    /// bind mounts, the subvolume path for btrfs subvolume mounts.
    pub root: PathBuf,
    pub mount_point: PathBuf,
    /// Per-mount options (`rw`/`ro`, `nosuid`, …).
    pub options: String,
    pub fstype: String,
    /// Filesystem-specific source, usually a device path such as `/dev/sdb1`.
    pub source: String,
    pub super_options: String,
}

impl MountEntry {
    pub fn read_only(&self) -> bool {
        self.options.split(',').any(|option| option == "ro")
    }
}

/// Parses the whole table. Any malformed line makes the parse fail rather than being
/// skipped, since a silently missing mount would lead to wrong decisions.
pub fn parse(text: &[u8]) -> Result<Vec<MountEntry>, String> {
    text.split(|&b| b == b'\n')
        .enumerate()
        .filter(|(_, line)| !line.is_empty())
        .map(|(index, line)| {
            parse_line(line).ok_or_else(|| {
                format!("malformed line {}: {}", index + 1, String::from_utf8_lossy(line))
            })
        })
        .collect()
}

fn parse_line(line: &[u8]) -> Option<MountEntry> {
    let fields: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
    let separator = fields.iter().skip(6).position(|f| *f == b"-")? + 6;
    let [fstype, source, super_options] = fields.get(separator + 1..)? else { return None };
    let text = |field: &[u8]| String::from_utf8_lossy(&unescape(field)).into_owned();
    Some(MountEntry {
        id: std::str::from_utf8(fields[0]).ok()?.parse().ok()?,
        parent_id: std::str::from_utf8(fields[1]).ok()?.parse().ok()?,
        devnum: DevNum::parse(std::str::from_utf8(fields[2]).ok()?)?,
        root: path(fields[3]),
        mount_point: path(fields[4]),
        options: text(fields[5]),
        fstype: text(fstype),
        source: text(source),
        super_options: text(super_options),
    })
}

fn path(field: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(unescape(field)))
}

/// Undoes the kernel's octal escaping (`\040` space, `\011` tab, `\012` newline, `\134` `\`).
pub fn unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        if field[i] == b'\\'
            && let Some(digits) = field.get(i + 1..i + 4)
            && digits.iter().all(|d| (b'0'..=b'7').contains(d))
        {
            let value = digits.iter().fold(0u16, |acc, d| acc * 8 + u16::from(d - b'0'));
            if let Ok(byte) = u8::try_from(value) {
                out.push(byte);
                i += 4;
                continue;
            }
        }
        out.push(field[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // A desktop's table, abridged and with made-up values: btrfs subvolumes, a udisks2 mount,
    // autofs, FUSE.
    const REAL: &str = "\
40 1 0:33 /@ / rw,noatime shared:1 - btrfs /dev/nvme0n1p2 rw,compress=zstd:1,ssd,discard=async,space_cache=v2,subvolid=256,subvol=/@
50 40 0:28 / /run rw,nosuid,nodev shared:12 - tmpfs tmpfs rw,size=4096000k,nr_inodes=819200,mode=755,inode64,huge=advise
26 49 0:34 / /proc/sys/fs/binfmt_misc rw,relatime shared:13 - autofs systemd-1 rw,fd=44,pgrp=1,timeout=0,minproto=5,maxproto=5,direct,pipe_ino=833
61 40 0:33 /@home /home rw,noatime shared:227 - btrfs /dev/nvme0n1p2 rw,compress=zstd:1,ssd,discard=async,space_cache=v2,subvolid=257,subvol=/@home
269 40 259:1 / /boot rw,relatime shared:233 - vfat /dev/nvme0n1p1 rw,fmask=0077,dmask=0077,codepage=437,iocharset=ascii,shortname=mixed,utf8,errors=remount-ro
456 436 0:91 / /run/user/1000/doc rw,nosuid,nodev,relatime shared:815 - fuse.portal portal rw,user_id=1000,group_id=1000
28 50 0:161 / /run/media/user/DATA rw,nosuid,nodev,relatime shared:556 - btrfs /dev/sda1 rw,ssd,discard=async,space_cache=v2,subvolid=5,subvol=/
";

    #[test]
    fn parses_real_table() {
        let entries = parse(REAL.as_bytes()).unwrap();
        assert_eq!(entries.len(), 7);

        let root = &entries[0];
        assert_eq!((root.id, root.parent_id), (40, 1));
        assert_eq!(root.devnum, DevNum::new(0, 33));
        assert_eq!(root.root, Path::new("/@"));
        assert_eq!(root.mount_point, Path::new("/"));
        assert_eq!(root.fstype, "btrfs");
        assert_eq!(root.source, "/dev/nvme0n1p2");
        assert!(root.super_options.contains("subvol=/@"));
        assert!(!root.read_only());

        let home = &entries[3];
        assert_eq!(home.root, Path::new("/@home"));
        assert_eq!(home.mount_point, Path::new("/home"));

        let boot = &entries[4];
        assert_eq!(boot.devnum, DevNum::new(259, 1));
        assert!(!boot.devnum.is_anonymous());
        assert!(root.devnum.is_anonymous());

        assert_eq!(entries[2].source, "systemd-1");
        assert_eq!(entries[5].fstype, "fuse.portal");
        assert_eq!(entries[6].mount_point, Path::new("/run/media/user/DATA"));
    }

    #[test]
    fn unescapes_special_characters() {
        let line = r"30 28 8:17 /a\040b /run/media/u/My\040Disk\011tab\012nl\134bs ro,nosuid - ext4 /dev/sdb1 ro";
        let entry = &parse(line.as_bytes()).unwrap()[0];
        assert_eq!(entry.root, Path::new("/a b"));
        assert_eq!(entry.mount_point, Path::new("/run/media/u/My Disk\ttab\nnl\\bs"));
        assert!(entry.read_only());
    }

    #[test]
    fn unescape_leaves_other_backslashes_alone() {
        assert_eq!(unescape(br"a\x20b"), br"a\x20b");
        assert_eq!(unescape(br"a\04"), br"a\04");
        assert_eq!(unescape(br"a\"), br"a\");
        assert_eq!(unescape(br"\777"), br"\777"); // doesn't fit in a byte
        assert_eq!(unescape(br"\101\102"), b"AB");
    }

    #[test]
    fn optional_fields_vary() {
        let none = "1 0 8:1 / / rw - ext4 /dev/sda1 rw";
        let many =
            "1 0 8:1 / / rw shared:1 master:2 propagate_from:3 unbindable - ext4 /dev/sda1 rw";
        for line in [none, many] {
            let entry = &parse(line.as_bytes()).unwrap()[0];
            assert_eq!(
                (entry.fstype.as_str(), entry.source.as_str(), entry.super_options.as_str()),
                ("ext4", "/dev/sda1", "rw")
            );
        }
    }

    #[test]
    fn a_dash_mount_point_is_not_the_separator() {
        let entry = &parse(b"1 0 8:1 / /- rw - ext4 /dev/sda1 rw").unwrap()[0];
        assert_eq!(entry.mount_point, Path::new("/-"));
        assert_eq!(entry.fstype, "ext4");
    }

    #[test]
    fn non_utf8_mount_points_survive() {
        let entry = &parse(b"1 0 8:1 / /mnt/\xff\xfe rw - ext4 /dev/sda1 rw").unwrap()[0];
        assert_eq!(entry.mount_point.as_os_str().as_encoded_bytes(), b"/mnt/\xff\xfe");
    }

    #[test]
    fn empty_source_field() {
        let entry = &parse(b"1 0 0:5 / /x rw - tmpfs  rw").unwrap()[0];
        assert_eq!(entry.source, "");
        assert_eq!(entry.super_options, "rw");
    }

    #[test]
    fn rejects_malformed_lines() {
        assert!(parse(b"").unwrap().is_empty());
        assert!(parse(b"1 0 8:1 / / rw ext4 /dev/sda1 rw").is_err()); // no separator
        assert!(parse(b"1 0 8:1 / / rw - ext4 /dev/sda1").is_err()); // missing field
        assert!(parse(b"1 0 8:1 / / rw - ext4 /dev/sda1 rw extra").is_err());
        assert!(parse(b"x 0 8:1 / / rw - ext4 /dev/sda1 rw").is_err());
        assert!(parse(b"1 0 81 / / rw - ext4 /dev/sda1 rw").is_err());
        let err = parse(b"1 0 8:1 / / rw - ext4 /dev/sda1 rw\ngarbage\n").unwrap_err();
        assert!(err.contains("line 2"), "{err}");
    }

    #[test]
    fn dev_numbers() {
        assert_eq!(DevNum::parse("259:2"), Some(DevNum::new(259, 2)));
        assert_eq!(DevNum::parse(" 8:17\n"), Some(DevNum::new(8, 17)));
        assert_eq!(DevNum::parse("8"), None);
        assert_eq!(DevNum::parse("a:b"), None);
        // Large numbers use the split encoding.
        let dev = libc::makedev(4095 + 1, 300);
        assert_eq!(DevNum::from_dev_t(dev), DevNum::new(4096, 300));
        assert_eq!(DevNum::from_dev_t(libc::makedev(8, 17)), DevNum::new(8, 17));
        assert_eq!(DevNum::new(8, 17).to_string(), "8:17");
    }
}
