//! Access to the machine: block devices, the mount table, and mounting.
//!
//! Everything machine-specific goes through the [`System`] trait, so the logic in
//! [`crate::resolve`] and [`crate::engine`] can be tested against fake machines. Each system
//! devmounts runs on has its own: [`LinuxSystem`] here, and macOS's and FreeBSD's in their
//! modules; [`NativeSystem`] is the one this program was built for.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::MountMethod;
use crate::error::{Error, Result};
use crate::escalate;
use crate::mountinfo::{self, DevNum, MountEntry};
use crate::platform::Platform;
use crate::privileged::Privileges;
use crate::udev::{self, Identity};

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

/// A LUKS container to open or close; see [`System::unlock`] and [`System::lock`].
#[derive(Debug, Clone)]
pub struct ContainerRequest<'a> {
    pub container: &'a BlockDevice,
    /// How devices are mounted (`mount_method`): with `udisks`, udisks2 may ask for an
    /// administrator's password itself, as when mounting.
    pub method: MountMethod,
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
    /// Opens the locked LUKS container `request.container`; what opens it asks for the
    /// passphrase itself.
    fn unlock(&self, request: &ContainerRequest<'_>) -> Result<()>;
    /// Closes the LUKS container `request.container` again.
    fn lock(&self, request: &ContainerRequest<'_>) -> Result<()>;
}

/// The machine this program was built for.
#[cfg(target_os = "linux")]
pub type NativeSystem = LinuxSystem;
/// The machine this program was built for.
#[cfg(target_os = "macos")]
pub type NativeSystem = crate::macos::MacOsSystem;
/// The machine this program was built for.
#[cfg(target_os = "freebsd")]
pub type NativeSystem = crate::freebsd::FreeBsdSystem;

/// The real machine, on Linux. Paths can be redirected for tests or unusual setups.
#[derive(Debug, Clone)]
pub struct LinuxSystem {
    pub sysfs: PathBuf,
    pub udev_data: PathBuf,
    pub mountinfo: PathBuf,
    pub dev: PathBuf,
    /// Where an automounter mounts devices ([`Platform::automount_dirs`]).
    pub automount_dirs: Vec<PathBuf>,
}

impl Default for LinuxSystem {
    fn default() -> Self {
        LinuxSystem {
            sysfs: "/sys".into(),
            udev_data: "/run/udev/data".into(),
            mountinfo: "/proc/self/mountinfo".into(),
            dev: "/dev".into(),
            automount_dirs: Platform::Linux.automount_dirs().iter().map(PathBuf::from).collect(),
        }
    }
}

impl LinuxSystem {
    pub fn new() -> Self {
        Self::default()
    }

    /// Like [`LinuxSystem::new`], but `DEVMOUNTS_SYSFS`, `DEVMOUNTS_UDEV_DATA`,
    /// `DEVMOUNTS_MOUNTINFO` and `DEVMOUNTS_DEV` override the respective paths, and
    /// `DEVMOUNTS_AUTOMOUNT_DIRS` (`:`-separated, like `PATH`) the automounters' directories.
    pub fn from_env() -> Self {
        let var = |name: &str, default: PathBuf| {
            std::env::var_os(name).map(PathBuf::from).unwrap_or(default)
        };
        let d = Self::default();
        LinuxSystem {
            sysfs: var("DEVMOUNTS_SYSFS", d.sysfs),
            udev_data: var("DEVMOUNTS_UDEV_DATA", d.udev_data),
            mountinfo: var("DEVMOUNTS_MOUNTINFO", d.mountinfo),
            dev: var("DEVMOUNTS_DEV", d.dev),
            automount_dirs: automount_dirs_from_env(d.automount_dirs),
        }
    }

    fn read_device(&self, kname: &str) -> io::Result<BlockDevice> {
        let dir = self.sysfs.join("class/block").join(kname);
        let devnum = DevNum::parse(&fs::read_to_string(dir.join("dev"))?)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad dev file"))?;
        // DEVNAME is the node path below /dev; sysfs spells '/' as '!' (cciss!c0d0).
        let devname = fs::read_to_string(dir.join("uevent"))
            .ok()
            .and_then(|uevent| {
                uevent.lines().find_map(|l| l.strip_prefix("DEVNAME=")).map(str::to_owned)
            })
            .unwrap_or_else(|| kname.replace('!', "/"));
        let mut holders: Vec<String> = match fs::read_dir(dir.join("holders")) {
            Ok(entries) => entries.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect(),
            Err(_) => Vec::new(),
        };
        holders.sort();
        let dm_name = fs::read_to_string(dir.join("dm/name"))
            .ok()
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty());

        let mut device = BlockDevice {
            kname: kname.to_owned(),
            devnode: self.dev.join(devname),
            devnum,
            partition: dir.join("partition").exists(),
            holders,
            dm_name,
            ..BlockDevice::default()
        };
        let db = self.udev_data.join(format!("b{}:{}", devnum.major, devnum.minor));
        if let Ok(bytes) = fs::read(db) {
            device.set_identity(udev::identity(&udev::parse_properties(&String::from_utf8_lossy(
                &bytes,
            ))));
        }
        Ok(device)
    }
}

impl System for LinuxSystem {
    fn block_devices(&self) -> Result<Vec<BlockDevice>> {
        let class = self.sysfs.join("class/block");
        let entries = fs::read_dir(&class)
            .map_err(|err| Error::Inventory { message: format!("{}: {err}", class.display()) })?;
        let mut knames: Vec<String> =
            entries.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect();
        knames.sort();

        let mut devices = Vec::new();
        for kname in knames {
            match self.read_device(&kname) {
                Ok(device) => devices.push(device),
                // Devices can disappear while we look at them.
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(Error::Inventory { message: format!("{kname}: {err}") }),
            }
        }

        // Without a udev database (no udev, some containers), ask lsblk, which probes the
        // devices itself (that needs root).
        if !self.udev_data.is_dir()
            && let Some(identities) = lsblk_identities()
        {
            for device in &mut devices {
                if let Some(identity) = identities.get(&device.kname) {
                    device.set_identity(identity.clone());
                }
            }
        }
        Ok(devices)
    }

    fn mounts(&self) -> Result<Vec<MountEntry>> {
        let bytes = fs::read(&self.mountinfo).map_err(|err| Error::MountTable {
            message: format!("{}: {err}", self.mountinfo.display()),
        })?;
        mountinfo::parse(&bytes).map_err(|message| Error::MountTable { message })
    }

    fn device_number(&self, path: &Path) -> Option<DevNum> {
        node_number(path, FileTypeExt::is_block_device)
    }

    fn automount_dirs(&self) -> Vec<PathBuf> {
        self.automount_dirs.clone()
    }

    fn mount(&self, request: &MountRequest<'_>) -> Result<()> {
        crate::mount::mount(request)
    }

    fn unmount(&self, request: &UnmountRequest<'_>) -> Result<()> {
        crate::mount::unmount(request)
    }

    fn unlock(&self, request: &ContainerRequest<'_>) -> Result<()> {
        crate::mount::unlock(request)
    }

    fn lock(&self, request: &ContainerRequest<'_>) -> Result<()> {
        crate::mount::lock(request)
    }
}

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

fn lsblk_identities() -> Option<HashMap<String, Identity>> {
    let argv: Vec<OsString> =
        ["lsblk", "-J", "-l", "-o", "KNAME,FSTYPE,UUID,LABEL,PARTUUID,PARTLABEL"]
            .iter()
            .map(OsString::from)
            .collect();
    let output = escalate::output(&argv).ok().filter(|o| o.status.success())?;
    parse_lsblk_json(&String::from_utf8_lossy(&output.stdout))
}

/// Parses `lsblk -J -l -o KNAME,FSTYPE,UUID,LABEL,PARTUUID,PARTLABEL`.
pub fn parse_lsblk_json(text: &str) -> Option<HashMap<String, Identity>> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let mut out = HashMap::new();
    for dev in value.get("blockdevices")?.as_array()? {
        let field = |name: &str| {
            dev.get(name).and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(str::to_owned)
        };
        let Some(kname) = field("kname") else { continue };
        out.insert(
            kname,
            Identity {
                fstype: field("fstype"),
                uuid: field("uuid"),
                label: field("label"),
                partuuid: field("partuuid"),
                partlabel: field("partlabel"),
            },
        );
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsblk_json() {
        let text = r#"{"blockdevices": [
            {"kname":"sda", "fstype":null, "uuid":null, "label":null, "partuuid":null, "partlabel":null},
            {"kname":"sda1", "fstype":"vfat", "uuid":"83F8-2DA0", "label":"My Disk", "partuuid":"1a14", "partlabel":""}
        ]}"#;
        let ids = parse_lsblk_json(text).unwrap();
        assert!(ids["sda"].is_empty());
        let part = &ids["sda1"];
        assert_eq!(part.fstype.as_deref(), Some("vfat"));
        assert_eq!(part.label.as_deref(), Some("My Disk"));
        assert_eq!(part.partlabel, None);
        assert!(parse_lsblk_json("not json").is_none());
        assert!(parse_lsblk_json("{}").is_none());
    }

    #[test]
    fn describe() {
        let mut dev = BlockDevice { devnode: "/dev/sdb1".into(), ..Default::default() };
        assert_eq!(dev.describe(), "/dev/sdb1");
        dev.fstype = Some("btrfs".into());
        dev.label = Some("BACKUP".into());
        assert_eq!(dev.describe(), "/dev/sdb1 (btrfs \"BACKUP\")");
    }
}
