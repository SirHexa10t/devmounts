//! Finding a configured device, its mounts, and where paths inside it can be reached.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::{DeviceId, DevicePath};
use crate::error::{Error, Result};
use crate::mountinfo::{DevNum, MountEntry};
use crate::system::BlockDevice;

/// Types whose contents only become mountable through a device stacked on top.
const CONTAINERS: &[&str] = &[LUKS, "BitLocker", "linux_raid_member", "LVM2_member", "bcache"];

/// A LUKS container's type: the one kind devmounts opens itself (with udisks2), and locks again.
pub const LUKS: &str = "crypto_LUKS";

/// Recognized but not mountable as a filesystem.
const NOT_FILESYSTEMS: &[&str] = &["swap", "zfs_member", "ceph", "drbd", "VMFS_volume_member"];

/// How deep container stacks may go (RAID → LUKS → LVM → … ).
const MAX_STACK: usize = 16;

/// A configured device, found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedDevice {
    /// The device holding the filesystem.
    pub device: BlockDevice,
    /// Every device the filesystem lives on (several for multi-device btrfs).
    pub members: Vec<BlockDevice>,
    /// Containers passed on the way, outermost first (e.g. the LUKS partition the id named).
    pub containers: Vec<BlockDevice>,
}

pub fn id_matches(id: &DeviceId, device: &BlockDevice) -> bool {
    let eq_ci = |value: &Option<String>, wanted: &str| {
        value.as_deref().is_some_and(|v| v.eq_ignore_ascii_case(wanted))
    };
    match id {
        DeviceId::Uuid(uuid) => eq_ci(&device.uuid, uuid),
        DeviceId::PartUuid(uuid) => eq_ci(&device.partuuid, uuid),
        DeviceId::Label(label) => device.label.as_deref() == Some(label.as_str()),
        DeviceId::PartLabel(label) => device.partlabel.as_deref() == Some(label.as_str()),
    }
}

/// Finds the device an id refers to and the filesystem on it.
pub fn find_device(id: &DeviceId, devices: &[BlockDevice]) -> Result<ResolvedDevice> {
    let by_kname: HashMap<&str, &BlockDevice> =
        devices.iter().map(|d| (d.kname.as_str(), d)).collect();
    let mut candidates: Vec<&BlockDevice> = devices.iter().filter(|d| id_matches(id, d)).collect();

    if candidates.is_empty() {
        return Err(Error::DeviceNotFound { id: id.to_string(), hint: near_misses(id, devices) });
    }

    // A device stacked on another candidate (multipath, dm-linear over a partition, …) shows
    // the same identity; the topmost one is what gets mounted.
    let kept: Vec<&BlockDevice> = candidates
        .iter()
        .copied()
        .filter(|c| {
            !candidates
                .iter()
                .any(|other| other.kname != c.kname && stacked_on(other, c, &by_kname))
        })
        .collect();
    candidates = kept;

    let device = match candidates.as_slice() {
        [one] => *one,
        [first, rest @ ..] if is_one_btrfs(first, rest) => {
            let members: Vec<BlockDevice> = candidates.iter().map(|d| (*d).clone()).collect();
            return Ok(ResolvedDevice {
                device: (*first).clone(),
                members,
                containers: Vec::new(),
            });
        }
        many => {
            return Err(Error::AmbiguousDevice {
                id: id.to_string(),
                candidates: many.iter().map(|d| d.describe()).collect(),
            });
        }
    };

    let (device, containers) = unwrap_containers(device, &by_kname)?;
    let members = vec![device.clone()];
    Ok(ResolvedDevice { device, members, containers })
}

/// Whether `upper` is built on top of `lower`, directly or through other layers.
fn stacked_on(
    upper: &BlockDevice,
    lower: &BlockDevice,
    by_kname: &HashMap<&str, &BlockDevice>,
) -> bool {
    let mut pending: Vec<&str> = lower.holders.iter().map(String::as_str).collect();
    let mut seen = HashSet::new();
    while let Some(kname) = pending.pop() {
        if kname == upper.kname {
            return true;
        }
        if seen.insert(kname)
            && let Some(dev) = by_kname.get(kname)
        {
            pending.extend(dev.holders.iter().map(String::as_str));
        }
    }
    false
}

fn is_one_btrfs(first: &BlockDevice, rest: &[&BlockDevice]) -> bool {
    first.fstype.as_deref() == Some("btrfs")
        && first.uuid.is_some()
        && rest.iter().all(|d| {
            d.fstype == first.fstype
                && d.uuid.as_deref().map(str::to_ascii_lowercase)
                    == first.uuid.as_deref().map(str::to_ascii_lowercase)
        })
}

fn unwrap_containers(
    start: &BlockDevice,
    by_kname: &HashMap<&str, &BlockDevice>,
) -> Result<(BlockDevice, Vec<BlockDevice>)> {
    let mut current = start;
    let mut containers = Vec::new();
    while let Some(fstype) = current.fstype.as_deref().filter(|t| CONTAINERS.contains(t)) {
        if containers.len() >= MAX_STACK {
            break;
        }
        let holder = match current.holders.as_slice() {
            [] => {
                return Err(Error::InactiveContainer {
                    device: current.devnode.clone(),
                    fstype: fstype.to_owned(),
                    hint: container_hint(fstype, &current.devnode),
                });
            }
            [one] => *by_kname.get(one.as_str()).ok_or_else(|| Error::Inventory {
                message: format!("{} is held by {one}, which isn't listed", current.kname),
            })?,
            many => {
                return Err(Error::MultipleHolders {
                    device: current.devnode.display().to_string(),
                    holders: many.iter().map(|k| format!("/dev/{k}")).collect(),
                });
            }
        };
        containers.push(current.clone());
        current = holder;
    }
    match current.fstype.as_deref() {
        Some(fstype) if !NOT_FILESYSTEMS.contains(&fstype) && !CONTAINERS.contains(&fstype) => {
            Ok((current.clone(), containers))
        }
        other => Err(Error::NoFilesystem {
            device: current.devnode.display().to_string(),
            fstype: other.map(str::to_owned),
        }),
    }
}

fn container_hint(fstype: &str, devnode: &Path) -> String {
    match fstype {
        LUKS | "BitLocker" => {
            format!("unlock it first, e.g. `udisksctl unlock -b {}`", devnode.display())
        }
        "linux_raid_member" => "assemble the RAID array first (`mdadm --assemble`)".into(),
        "LVM2_member" => "activate the volume group first (`vgchange -ay`)".into(),
        _ => "activate it first".into(),
    }
}

/// Mentions identifiers that only differ in case, which is a common reason for a miss.
fn near_misses(id: &DeviceId, devices: &[BlockDevice]) -> Option<String> {
    let (wanted, field): (&str, fn(&BlockDevice) -> Option<&str>) = match id {
        DeviceId::Label(l) => (l, |d| d.label.as_deref()),
        DeviceId::PartLabel(l) => (l, |d| d.partlabel.as_deref()),
        _ => return None,
    };
    let similar: Vec<String> = devices
        .iter()
        .filter_map(field)
        .filter(|v| v.eq_ignore_ascii_case(wanted) || v.trim() == wanted.trim())
        .map(|v| format!("{v:?}"))
        .collect();
    (!similar.is_empty())
        .then(|| format!("labels are case-sensitive; found {}", similar.join(", ")))
}

/// The mount tree, for working out which mount a path lookup really ends in.
pub struct MountTree<'a> {
    entries: &'a [MountEntry],
    children: HashMap<u32, Vec<&'a MountEntry>>,
    root: Option<&'a MountEntry>,
}

impl<'a> MountTree<'a> {
    pub fn new(entries: &'a [MountEntry]) -> Self {
        let ids: HashSet<u32> = entries.iter().map(|e| e.id).collect();
        let mut children: HashMap<u32, Vec<&MountEntry>> = HashMap::new();
        for entry in entries {
            if entry.parent_id != entry.id {
                children.entry(entry.parent_id).or_default().push(entry);
            }
        }
        let at_slash = || entries.iter().filter(|e| e.mount_point == Path::new("/"));
        // The namespace root is the "/" mount whose parent isn't in the table.
        let root = at_slash()
            .find(|e| !ids.contains(&e.parent_id) || e.parent_id == e.id)
            .or_else(|| at_slash().next());
        let mut tree = MountTree { entries, children, root: None };
        tree.root = root.map(|r| tree.topmost(r));
        tree
    }

    pub fn entries(&self) -> &'a [MountEntry] {
        self.entries
    }

    /// The mount stacked last on the same mount point as `entry`.
    fn topmost(&self, mut entry: &'a MountEntry) -> &'a MountEntry {
        for _ in 0..self.entries.len() {
            match self.child_at(entry.id, &entry.mount_point) {
                Some(over) => entry = over,
                None => break,
            }
        }
        entry
    }

    fn child_at(&self, parent: u32, mount_point: &Path) -> Option<&'a MountEntry> {
        self.children.get(&parent)?.iter().copied().rfind(|c| c.mount_point == mount_point)
    }

    /// The mount a lookup of `path` ends in. `path` must be absolute and free of symlinks
    /// (e.g. from `fs::canonicalize`), as mount points are.
    pub fn mount_at(&self, path: &Path) -> Option<&'a MountEntry> {
        let mut current = self.root?;
        let mut prefix = PathBuf::from("/");
        for component in path.components().skip(1) {
            prefix.push(component);
            if let Some(child) = self.child_at(current.id, &prefix) {
                current = self.topmost(child);
            }
        }
        Some(current)
    }

    /// Whether `entry` can be reached at its mount point (it isn't over-mounted, and no
    /// directory above it is).
    pub fn is_reachable(&self, entry: &MountEntry) -> bool {
        self.mount_at(&entry.mount_point).is_some_and(|m| m.id == entry.id)
    }
}

/// The reachable mounts of a device's filesystem.
///
/// A mount belongs to the device when its device number matches, or when its source path
/// is one of the device's nodes; the latter is needed for btrfs and FUSE (`ntfs-3g`), which
/// report anonymous device numbers.
pub fn device_mounts<'a>(
    device: &ResolvedDevice,
    tree: &MountTree<'a>,
    device_number: impl Fn(&Path) -> Option<DevNum>,
) -> Vec<&'a MountEntry> {
    let numbers: HashSet<DevNum> = device.members.iter().map(|m| m.devnum).collect();
    let source_is_member = |source: &str| {
        if !source.starts_with('/') {
            return false;
        }
        let source = Path::new(source);
        match device_number(source) {
            Some(number) => numbers.contains(&number),
            // The node can't be inspected (e.g. no /dev in a test or container): compare names.
            None => device.members.iter().any(|m| {
                source == m.devnode
                    || m.dm_name
                        .as_ref()
                        .is_some_and(|n| source == Path::new("/dev/mapper").join(n))
            }),
        }
    };

    let matched: Vec<&MountEntry> = tree
        .entries()
        .iter()
        .filter(|e| {
            (!e.devnum.is_anonymous() && numbers.contains(&e.devnum)) || source_is_member(&e.source)
        })
        .collect();
    // Every mount of the same superblock (same anonymous number) is the same filesystem.
    let superblocks: HashSet<DevNum> = matched.iter().map(|e| e.devnum).collect();
    tree.entries()
        .iter()
        .filter(|e| superblocks.contains(&e.devnum))
        .filter(|e| tree.is_reachable(e))
        .collect()
}

/// Where a path inside a device can be reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location<'a> {
    pub mount: &'a MountEntry,
    pub path: PathBuf,
}

/// Maps a path inside the device onto one of its mounts, through each mount's `root`
/// (btrfs subvolume or bind-mounted subdirectory). Prefers the most specific root, then
/// the shortest mount point, so the choice is stable.
pub fn locate<'a>(path: &DevicePath, mounts: &[&'a MountEntry]) -> Option<Location<'a>> {
    let wanted = path.in_device();
    mounts
        .iter()
        .filter_map(|m| Some((*m, wanted.strip_prefix(&m.root).ok()?)))
        .min_by_key(|(m, _)| {
            (Reverse(m.root.components().count()), m.mount_point.components().count(), m.id)
        })
        .map(|(mount, rest)| {
            let path = if rest.as_os_str().is_empty() {
                mount.mount_point.clone()
            } else {
                mount.mount_point.join(rest)
            };
            Location { mount, path }
        })
}

/// Describes what parts of a device its mounts expose, for error messages.
pub fn exposed_parts(mounts: &[&MountEntry]) -> Vec<String> {
    mounts.iter().map(|m| format!("{} at {}", m.root.display(), m.mount_point.display())).collect()
}

/// Checks a target on the real filesystem: it must be an existing directory, and after
/// resolving symlinks it must still be on one of the device's mounts (not led elsewhere by
/// a symlink inside the device, nor covered by another mount). Returns the resolved path.
pub fn check_target(
    target: &Path,
    device_mounts: &[&MountEntry],
    tree: &MountTree<'_>,
) -> Result<PathBuf> {
    let meta = fs::metadata(target).map_err(|err| match err.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => {
            Error::TargetMissing { target: target.to_owned() }
        }
        _ => Error::io(format!("can't inspect {}", target.display()), &err),
    })?;
    if !meta.is_dir() {
        return Err(Error::TargetNotDirectory { target: target.to_owned() });
    }
    let resolved = fs::canonicalize(target)
        .map_err(|err| Error::io(format!("can't resolve {}", target.display()), &err))?;
    let ours: HashSet<u32> = device_mounts.iter().map(|m| m.id).collect();
    match tree.mount_at(&resolved) {
        Some(m) if ours.contains(&m.id) => Ok(resolved),
        Some(m) if resolved == target => Err(Error::TargetCovered {
            target: target.to_owned(),
            mount_point: m.mount_point.clone(),
        }),
        _ => Err(Error::TargetEscapesDevice { target: target.to_owned(), resolved }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(kname: &str, major: u32, minor: u32) -> BlockDevice {
        BlockDevice {
            kname: kname.into(),
            devnode: PathBuf::from("/dev").join(kname),
            devnum: DevNum::new(major, minor),
            ..Default::default()
        }
    }

    fn fs(mut d: BlockDevice, fstype: &str, uuid: &str, label: Option<&str>) -> BlockDevice {
        d.fstype = Some(fstype.into());
        d.uuid = Some(uuid.into());
        d.label = label.map(Into::into);
        d
    }

    fn held_by(mut d: BlockDevice, holders: &[&str]) -> BlockDevice {
        d.holders = holders.iter().map(|h| h.to_string()).collect();
        d
    }

    fn mount(
        id: u32,
        parent: u32,
        devnum: (u32, u32),
        root: &str,
        at: &str,
        fstype: &str,
        source: &str,
    ) -> MountEntry {
        MountEntry {
            id,
            parent_id: parent,
            devnum: DevNum::new(devnum.0, devnum.1),
            root: root.into(),
            mount_point: at.into(),
            options: "rw".into(),
            fstype: fstype.into(),
            source: source.into(),
            super_options: "rw".into(),
        }
    }

    fn label(l: &str) -> DeviceId {
        DeviceId::Label(l.into())
    }

    fn no_nodes(_: &Path) -> Option<DevNum> {
        None
    }

    // --- finding devices -------------------------------------------------------------

    #[test]
    fn finds_by_each_kind_of_id() {
        let mut part = fs(dev("nvme0n1p2", 259, 2), "ext4", "AbCd-0001", Some("Data"));
        part.partuuid = Some("PART-UUID".into());
        part.partlabel = Some("data part".into());
        let devices = vec![dev("nvme0n1", 259, 0), part];
        for id in [
            DeviceId::Uuid("abcd-0001".into()),
            DeviceId::PartUuid("part-uuid".into()),
            label("Data"),
            DeviceId::PartLabel("data part".into()),
        ] {
            assert_eq!(find_device(&id, &devices).unwrap().device.kname, "nvme0n1p2", "{id}");
        }
    }

    #[test]
    fn labels_are_case_sensitive_with_a_hint() {
        let devices = vec![fs(dev("sdb1", 8, 17), "vfat", "1234-ABCD", Some("BACKUP"))];
        match find_device(&label("backup"), &devices) {
            Err(Error::DeviceNotFound { hint: Some(hint), .. }) => {
                assert!(hint.contains("\"BACKUP\""), "{hint}")
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            find_device(&label("other"), &devices),
            Err(Error::DeviceNotFound { hint: None, .. })
        ));
    }

    #[test]
    fn duplicate_labels_are_ambiguous() {
        // Two drives that both shipped as "Example Drive".
        let devices = vec![
            fs(dev("sdb1", 8, 17), "exfat", "1111-1111", Some("Example Drive")),
            fs(dev("sdc1", 8, 33), "exfat", "2222-2222", Some("Example Drive")),
        ];
        match find_device(&label("Example Drive"), &devices) {
            Err(Error::AmbiguousDevice { candidates, .. }) => assert_eq!(candidates.len(), 2),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn cloned_uuids_are_ambiguous() {
        let devices = vec![
            fs(dev("sdb1", 8, 17), "ext4", "same", None),
            fs(dev("sdc1", 8, 33), "ext4", "same", None),
        ];
        assert!(matches!(
            find_device(&DeviceId::Uuid("same".into()), &devices),
            Err(Error::AmbiguousDevice { .. })
        ));
    }

    #[test]
    fn multi_device_btrfs_is_one_filesystem() {
        let devices = vec![
            fs(dev("sdb1", 8, 17), "btrfs", "fs-1", Some("pool")),
            fs(dev("sdc1", 8, 33), "btrfs", "FS-1", Some("pool")),
        ];
        let found = find_device(&label("pool"), &devices).unwrap();
        assert_eq!(found.members.len(), 2);
    }

    #[test]
    fn stacked_duplicates_prefer_the_top() {
        // Multipath: two paths to the same LUN and the dm device on top all show the fs.
        let devices = vec![
            held_by(fs(dev("sdb1", 8, 17), "xfs", "u", None), &["dm-2"]),
            held_by(fs(dev("sdc1", 8, 33), "xfs", "u", None), &["dm-2"]),
            fs(dev("dm-2", 254, 2), "xfs", "u", None),
        ];
        assert_eq!(
            find_device(&DeviceId::Uuid("u".into()), &devices).unwrap().device.kname,
            "dm-2"
        );
    }

    #[test]
    fn whole_disk_filesystems() {
        let devices = vec![fs(dev("sdb", 8, 16), "vfat", "AB12-CD34", Some("STICK"))];
        let found = find_device(&label("STICK"), &devices).unwrap();
        assert_eq!(found.device.kname, "sdb");
        assert!(!found.device.partition);
    }

    #[test]
    fn open_luks_is_followed_to_the_filesystem() {
        let mut inner = fs(dev("dm-0", 254, 0), "ext4", "inner", Some("secret"));
        inner.dm_name = Some("luks-outer".into());
        let devices =
            vec![held_by(fs(dev("sdb1", 8, 17), "crypto_LUKS", "outer", None), &["dm-0"]), inner];
        let found = find_device(&DeviceId::Uuid("outer".into()), &devices).unwrap();
        assert_eq!(found.device.kname, "dm-0");
        assert_eq!(found.containers.len(), 1);
        assert_eq!(found.containers[0].kname, "sdb1");
        // The inner filesystem can be named directly, too.
        assert_eq!(find_device(&label("secret"), &devices).unwrap().device.kname, "dm-0");
    }

    #[test]
    fn locked_luks_explains_itself() {
        let devices = vec![fs(dev("sdb1", 8, 17), "crypto_LUKS", "outer", None)];
        match find_device(&DeviceId::Uuid("outer".into()), &devices) {
            Err(Error::InactiveContainer { hint, .. }) => {
                assert!(hint.contains("udisksctl unlock -b /dev/sdb1"))
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn raid_member_then_luks_then_filesystem() {
        let devices = vec![
            held_by(fs(dev("sda1", 8, 1), "linux_raid_member", "raid", Some("host:0")), &["md0"]),
            held_by(fs(dev("md0", 9, 0), "crypto_LUKS", "luks", None), &["dm-3"]),
            fs(dev("dm-3", 254, 3), "btrfs", "data", Some("DATA")),
        ];
        let found = find_device(&DeviceId::Uuid("raid".into()), &devices).unwrap();
        assert_eq!(found.device.kname, "dm-3");
        assert_eq!(
            found.containers.iter().map(|c| c.kname.as_str()).collect::<Vec<_>>(),
            ["sda1", "md0"]
        );
    }

    #[test]
    fn lvm_with_several_volumes_needs_a_specific_id() {
        let devices = vec![
            held_by(fs(dev("sdb2", 8, 18), "LVM2_member", "pv", None), &["dm-1", "dm-2"]),
            fs(dev("dm-1", 254, 1), "ext4", "lv1", None),
            fs(dev("dm-2", 254, 2), "ext4", "lv2", None),
        ];
        assert!(matches!(
            find_device(&DeviceId::Uuid("pv".into()), &devices),
            Err(Error::MultipleHolders { .. })
        ));
    }

    #[test]
    fn nothing_to_search() {
        assert!(matches!(
            find_device(&DeviceId::PartUuid("p".into()), &[]),
            Err(Error::DeviceNotFound { .. })
        ));
    }

    #[test]
    fn swap_and_empty_devices_have_no_filesystem() {
        let swap = fs(dev("zram0", 253, 0), "swap", "s", Some("zram0"));
        assert!(matches!(
            find_device(&label("zram0"), &[swap]),
            Err(Error::NoFilesystem { fstype: Some(t), .. }) if t == "swap"
        ));
        let mut bare = dev("sdb1", 8, 17);
        bare.partlabel = Some("empty".into());
        assert!(matches!(
            find_device(&DeviceId::PartLabel("empty".into()), &[bare]),
            Err(Error::NoFilesystem { fstype: None, .. })
        ));
    }

    #[test]
    fn container_cycles_terminate() {
        let devices = vec![
            held_by(fs(dev("a", 1, 1), "crypto_LUKS", "x", None), &["b"]),
            held_by(fs(dev("b", 1, 2), "crypto_LUKS", "y", None), &["a"]),
        ];
        assert!(find_device(&DeviceId::Uuid("x".into()), &devices).is_err());
    }

    // --- mount tree ----------------------------------------------------------------------

    fn basic_table() -> Vec<MountEntry> {
        vec![
            mount(40, 1, (0, 33), "/@", "/", "btrfs", "/dev/nvme0n1p2"),
            mount(61, 40, (0, 33), "/@home", "/home", "btrfs", "/dev/nvme0n1p2"),
            mount(50, 40, (0, 28), "/", "/run", "tmpfs", "tmpfs"),
            mount(269, 40, (259, 1), "/", "/boot", "vfat", "/dev/nvme0n1p1"),
            mount(28, 50, (0, 161), "/", "/run/media/u/DATA", "btrfs", "/dev/sda1"),
        ]
    }

    #[test]
    fn mount_at_walks_the_tree() {
        let table = basic_table();
        let tree = MountTree::new(&table);
        let at = |p: &str| tree.mount_at(Path::new(p)).unwrap().id;
        assert_eq!(at("/"), 40);
        assert_eq!(at("/etc/fstab"), 40);
        assert_eq!(at("/home/u/x"), 61);
        assert_eq!(at("/homework"), 40);
        assert_eq!(at("/run/media/u/DATA/a/b"), 28);
        assert_eq!(at("/run/media/u"), 50);
        assert!(table.iter().all(|e| tree.is_reachable(e)));
    }

    #[test]
    fn over_mounts_hide_what_is_below() {
        let mut table = basic_table();
        // Something mounted on top of DATA's mount point …
        table.push(mount(90, 28, (0, 99), "/", "/run/media/u/DATA", "tmpfs", "tmpfs"));
        // … and something on top of /run/media hides everything below it.
        table.push(mount(91, 50, (0, 98), "/", "/run/media", "tmpfs", "tmpfs"));
        let tree = MountTree::new(&table);
        let data = table.iter().find(|e| e.id == 28).unwrap();
        assert!(!tree.is_reachable(data));
        let over = table.iter().find(|e| e.id == 90).unwrap();
        assert!(!tree.is_reachable(over), "/run/media itself is covered");
        assert_eq!(tree.mount_at(Path::new("/run/media/u/DATA")).unwrap().id, 91);
    }

    #[test]
    fn stacked_mounts_at_the_root() {
        let table = vec![
            mount(1, 0, (8, 1), "/", "/", "ext4", "/dev/sda1"),
            mount(2, 1, (0, 5), "/", "/", "overlay", "overlay"),
        ];
        let tree = MountTree::new(&table);
        assert_eq!(tree.mount_at(Path::new("/x")).unwrap().id, 2);
    }

    #[test]
    fn empty_table() {
        let tree = MountTree::new(&[]);
        assert!(tree.mount_at(Path::new("/")).is_none());
    }

    // --- device mounts ---------------------------------------------------------------------

    fn system_disk() -> ResolvedDevice {
        let d = fs(dev("nvme0n1p2", 259, 2), "btrfs", "root-fs", None);
        ResolvedDevice { device: d.clone(), members: vec![d], containers: vec![] }
    }

    #[test]
    fn btrfs_mounts_match_by_source() {
        let table = basic_table();
        let tree = MountTree::new(&table);
        let ids: Vec<u32> =
            device_mounts(&system_disk(), &tree, no_nodes).iter().map(|m| m.id).collect();
        assert_eq!(ids, [40, 61]);
    }

    #[test]
    fn sources_are_compared_by_device_number_when_possible() {
        let table = vec![
            mount(1, 0, (8, 1), "/", "/", "ext4", "/dev/sda1"),
            // ntfs-3g: anonymous FUSE number, source spelled through a by-uuid link.
            mount(2, 1, (0, 77), "/", "/media/u/WIN", "fuseblk", "/dev/disk/by-uuid/0123ABCD"),
        ];
        let tree = MountTree::new(&table);
        let ntfs = fs(dev("sdc1", 8, 33), "ntfs", "0123ABCD", Some("WIN"));
        let resolved =
            ResolvedDevice { device: ntfs.clone(), members: vec![ntfs], containers: vec![] };
        let lookup =
            |p: &Path| (p == Path::new("/dev/disk/by-uuid/0123ABCD")).then_some(DevNum::new(8, 33));
        let found = device_mounts(&resolved, &tree, lookup);
        assert_eq!(found.iter().map(|m| m.id).collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn mapper_names_match_dm_devices() {
        let table = vec![
            mount(1, 0, (8, 1), "/", "/", "ext4", "/dev/sda1"),
            mount(2, 1, (0, 40), "/", "/media/u/secret", "btrfs", "/dev/mapper/luks-abc"),
        ];
        let tree = MountTree::new(&table);
        let mut inner = fs(dev("dm-0", 254, 0), "btrfs", "in", None);
        inner.dm_name = Some("luks-abc".into());
        let resolved =
            ResolvedDevice { device: inner.clone(), members: vec![inner], containers: vec![] };
        assert_eq!(device_mounts(&resolved, &tree, no_nodes).len(), 1);
    }

    #[test]
    fn regular_filesystems_match_by_number_including_bind_mounts() {
        let table = vec![
            mount(1, 0, (8, 1), "/", "/", "ext4", "/dev/sda1"),
            mount(2, 1, (8, 17), "/", "/media/u/DATA", "ext4", "/dev/sdb1"),
            mount(3, 1, (8, 17), "/photos", "/srv/photos", "ext4", "/dev/sdb1"),
        ];
        let tree = MountTree::new(&table);
        let d = fs(dev("sdb1", 8, 17), "ext4", "d", None);
        let resolved = ResolvedDevice { device: d.clone(), members: vec![d], containers: vec![] };
        assert_eq!(device_mounts(&resolved, &tree, no_nodes).len(), 2);
    }

    #[test]
    fn multi_device_btrfs_mounted_through_another_member() {
        let table = vec![
            mount(1, 0, (8, 1), "/", "/", "ext4", "/dev/sda1"),
            mount(2, 1, (0, 50), "/", "/media/u/pool", "btrfs", "/dev/sdc1"),
        ];
        let tree = MountTree::new(&table);
        let devices = vec![
            fs(dev("sdb1", 8, 17), "btrfs", "fs-1", Some("pool")),
            fs(dev("sdc1", 8, 33), "btrfs", "fs-1", Some("pool")),
        ];
        let resolved = find_device(&label("pool"), &devices).unwrap();
        assert_eq!(resolved.device.kname, "sdb1");
        assert_eq!(device_mounts(&resolved, &tree, no_nodes).len(), 1);
    }

    #[test]
    fn hidden_mounts_are_left_out() {
        let mut table = basic_table();
        table.push(mount(90, 28, (0, 99), "/", "/run/media/u/DATA", "tmpfs", "tmpfs"));
        let tree = MountTree::new(&table);
        let d = fs(dev("sda1", 8, 1), "btrfs", "a", Some("DATA"));
        let resolved = ResolvedDevice { device: d.clone(), members: vec![d], containers: vec![] };
        assert!(device_mounts(&resolved, &tree, no_nodes).is_empty());
    }

    // --- locating paths --------------------------------------------------------------------

    fn path(p: &str) -> DevicePath {
        DevicePath::parse(p).unwrap()
    }

    #[test]
    fn subvolume_paths_go_through_their_mounts() {
        let table = basic_table();
        let tree = MountTree::new(&table);
        let mounts = device_mounts(&system_disk(), &tree, no_nodes);
        let at = |p: &str| locate(&path(p), &mounts).map(|l| l.path);
        assert_eq!(at("@home/u/x"), Some("/home/u/x".into()));
        assert_eq!(at("@home"), Some("/home".into()));
        assert_eq!(at("@/etc"), Some("/etc".into()));
        assert_eq!(at("@homework"), None, "prefixes are matched by component");
        assert_eq!(at("."), None, "the top level isn't mounted");
        assert_eq!(at("@snapshots/1"), None);
    }

    #[test]
    fn the_most_specific_mount_wins() {
        let table = vec![
            mount(1, 0, (8, 1), "/", "/", "ext4", "/dev/sda1"),
            mount(2, 1, (0, 9), "/", "/run/media/u/D", "btrfs", "/dev/sdb1"),
            mount(3, 1, (0, 9), "/@data", "/data", "btrfs", "/dev/sdb1"),
        ];
        let tree = MountTree::new(&table);
        let d = fs(dev("sdb1", 8, 17), "btrfs", "x", None);
        let resolved = ResolvedDevice { device: d.clone(), members: vec![d], containers: vec![] };
        let mounts = device_mounts(&resolved, &tree, no_nodes);
        assert_eq!(locate(&path("@data/a"), &mounts).unwrap().path, Path::new("/data/a"));
        assert_eq!(
            locate(&path("other"), &mounts).unwrap().path,
            Path::new("/run/media/u/D/other")
        );
        assert_eq!(locate(&path("."), &mounts).unwrap().path, Path::new("/run/media/u/D"));
    }

    #[test]
    fn equal_roots_prefer_the_shorter_mount_point() {
        let table = vec![
            mount(1, 0, (8, 1), "/", "/", "ext4", "/dev/sda1"),
            mount(5, 1, (8, 17), "/", "/run/media/u/D", "ext4", "/dev/sdb1"),
            mount(4, 1, (8, 17), "/", "/mnt/d", "ext4", "/dev/sdb1"),
        ];
        let tree = MountTree::new(&table);
        let d = fs(dev("sdb1", 8, 17), "ext4", "x", None);
        let resolved = ResolvedDevice { device: d.clone(), members: vec![d], containers: vec![] };
        let mounts = device_mounts(&resolved, &tree, no_nodes);
        assert_eq!(locate(&path("a"), &mounts).unwrap().path, Path::new("/mnt/d/a"));
    }

    #[test]
    fn exposed_parts_for_messages() {
        let table = basic_table();
        let tree = MountTree::new(&table);
        let mounts = device_mounts(&system_disk(), &tree, no_nodes);
        assert_eq!(exposed_parts(&mounts), ["/@ at /", "/@home at /home"]);
    }
}
