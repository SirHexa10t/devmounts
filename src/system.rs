//! Access to the machine: block devices, the mount table, and mounting.
//!
//! Everything machine-specific goes through the [`System`] trait, so the logic in
//! [`crate::resolve`] and [`crate::engine`] can be tested against fake machines. Each system
//! devmounts runs on has its own, in its module — [`crate::linux`], [`crate::macos`],
//! [`crate::freebsd`] — and [`NativeSystem`] is the one this program was built for.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::MountMethod;
use crate::error::Result;
use crate::mountinfo::{DevNum, MountEntry};
use crate::privileged::Privileges;
use crate::udev::Identity;

/// A block device (disk, partition, device-mapper volume, …) as the kernel sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BlockDevice {
    /// Kernel name, e.g. `sdb1`, `nvme0n1p2`, `mmcblk0p1`, `dm-0`.
    pub kname: String,
    /// Device node, e.g. `/dev/sdb1`.
    pub devnode: PathBuf,
    pub devnum: DevNum,
    /// What's on the device: `ext4`, `btrfs`, `crypto_LUKS`, `swap`, …
    pub fstype: Option<String>,
    pub uuid: Option<String>,
    pub label: Option<String>,
    pub partuuid: Option<String>,
    pub partlabel: Option<String>,
    pub partition: bool,
    /// Kernel names of devices stacked on top of this one (dm-crypt, LVM, md).
    pub holders: Vec<String>,
    /// Device-mapper name, i.e. the `/dev/mapper/<name>` alias.
    pub dm_name: Option<String>,
}

impl BlockDevice {
    pub fn set_identity(&mut self, identity: Identity) {
        self.fstype = identity.fstype;
        self.uuid = identity.uuid;
        self.label = identity.label;
        self.partuuid = identity.partuuid;
        self.partlabel = identity.partlabel;
    }

    /// `/dev/sdb1 (btrfs "BACKUP")`, for messages.
    pub fn describe(&self) -> String {
        let mut details = Vec::new();
        if let Some(fstype) = &self.fstype {
            details.push(fstype.clone());
        }
        if let Some(label) = &self.label {
            details.push(format!("{label:?}"));
        }
        if details.is_empty() {
            self.devnode.display().to_string()
        } else {
            format!("{} ({})", self.devnode.display(), details.join(" "))
        }
    }
}

/// What to mount and how; see [`System::mount`].
#[derive(Debug, Clone)]
pub struct MountRequest<'a> {
    pub device: &'a BlockDevice,
    pub options: Vec<String>,
    pub method: MountMethod,
    /// Parent directory for mount points created by the `mount` method.
    pub mount_base: PathBuf,
    /// The device is already mounted, but not in a way that exposes the whole filesystem
    /// (e.g. only btrfs subvolumes): mount its top level in addition.
    pub top_level: bool,
    /// The run's way to root, for `mount` and the mount point.
    pub privileges: &'a Privileges,
}

/// What to unmount; see [`System::unmount`].
#[derive(Debug, Clone)]
pub struct UnmountRequest<'a> {
    pub device: &'a BlockDevice,
    pub mount: &'a MountEntry,
    /// How devices are mounted (`mount_method`): with `udisks`, an automounter's mount comes down
    /// through udisks2 alone, its own password prompt included.
    pub method: MountMethod,
    pub mount_base: PathBuf,
    /// The run's way to root, for `umount` and the mount point.
    pub privileges: &'a Privileges,
}

/// A container to open or close — a LUKS container, a GELI provider, a locked APFS volume
/// ([`UNLOCKABLE`](crate::resolve::UNLOCKABLE)); see [`System::unlock`] and [`System::lock`].
#[derive(Debug, Clone)]
pub struct ContainerRequest<'a> {
    pub container: &'a BlockDevice,
    /// How devices are mounted (`mount_method`): with `udisks`, udisks2 may ask for an
    /// administrator's password itself, as when mounting.
    pub method: MountMethod,
    /// The run's way to root, for what needs it (`geli attach`, `geli detach`).
    pub privileges: &'a Privileges,
}

pub trait System {
    /// All block devices with their identifiers.
    fn block_devices(&self) -> Result<Vec<BlockDevice>>;
    /// The mount table of the current mount namespace.
    fn mounts(&self) -> Result<Vec<MountEntry>>;
    /// The device number of a block device node (`None` if the path isn't one).
    fn device_number(&self, path: &Path) -> Option<DevNum>;
    /// The directories an automounter (udisks2) mounts devices into.
    fn automount_dirs(&self) -> Vec<PathBuf>;
    fn mount(&self, request: &MountRequest<'_>) -> Result<()>;
    fn unmount(&self, request: &UnmountRequest<'_>) -> Result<()>;
    /// Opens the locked container `request.container`; what opens it asks for the passphrase
    /// itself.
    fn unlock(&self, request: &ContainerRequest<'_>) -> Result<()>;
    /// Closes the container `request.container` again.
    fn lock(&self, request: &ContainerRequest<'_>) -> Result<()>;
}

/// The machine this program was built for.
#[cfg(target_os = "linux")]
pub type NativeSystem = crate::linux::LinuxSystem;
/// The machine this program was built for.
#[cfg(target_os = "macos")]
pub type NativeSystem = crate::macos::MacOsSystem;
/// The machine this program was built for.
#[cfg(target_os = "freebsd")]
pub type NativeSystem = crate::freebsd::FreeBsdSystem;

/// The automounters' directories from `DEVMOUNTS_AUTOMOUNT_DIRS` (`:`-separated, like `PATH`),
/// when it's set — for every system's `from_env`.
pub(crate) fn automount_dirs_from_env(default: Vec<PathBuf>) -> Vec<PathBuf> {
    std::env::var_os("DEVMOUNTS_AUTOMOUNT_DIRS")
        .map(|dirs| std::env::split_paths(&dirs).collect())
        .unwrap_or(default)
}

/// The device number of the node `path`, when `is_node` takes it for a device's: a block device
/// on Linux and macOS, a character device on FreeBSD, which has no others.
pub(crate) fn node_number(path: &Path, is_node: impl Fn(&fs::FileType) -> bool) -> Option<DevNum> {
    let meta = fs::metadata(path).ok()?;
    is_node(&meta.file_type()).then(|| DevNum::from_dev_t(meta.rdev()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe() {
        let mut dev = BlockDevice { devnode: "/dev/sdb1".into(), ..Default::default() };
        assert_eq!(dev.describe(), "/dev/sdb1");
        dev.fstype = Some("btrfs".into());
        dev.label = Some("BACKUP".into());
        assert_eq!(dev.describe(), "/dev/sdb1 (btrfs \"BACKUP\")");
    }
}
