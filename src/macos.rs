//! macOS: devices as `diskutil` lists them, the mount table from `getfsstat(2)`, and mounting
//! with `diskutil mount` — into `/Volumes`, as the calling user, the way the desktop mounts — or
//! with `mount -t` into `mount_base`, through the run's privileged helper.
//!
//! `diskutil list -plist` names every disk, partition and APFS volume, with the identity of what's
//! on it: the volume's name and UUID (`label`, `uuid`) and a partition's own UUID (`partuuid`).
//! What filesystem a partition holds only `diskutil info -plist <disk>` says, so each partition,
//! and each disk without a partition map, is asked about once; an APFS volume is APFS by what it
//! is. Neither shows a GPT partition's name, so on macOS no device has a `partlabel`.

use std::ffi::OsString;
use std::path::Path;

#[cfg(any(target_os = "macos", test))]
use crate::config::MountMethod;
use crate::plist::Value;
use crate::system::BlockDevice;

/// `diskutil list -plist`.
pub fn list_argv() -> Vec<OsString> {
    ["diskutil", "list", "-plist"].map(OsString::from).to_vec()
}

/// `diskutil info -plist <disk>`.
pub fn info_argv(disk: &str) -> Vec<OsString> {
    ["diskutil", "info", "-plist", disk].map(OsString::from).to_vec()
}

/// `diskutil mount [-mountOptions <options>] <device>`: into `/Volumes`, as the calling user.
pub fn diskutil_mount_argv(device: &Path, options: &[String]) -> Vec<OsString> {
    let mut argv = vec![OsString::from("diskutil"), "mount".into()];
    if !options.is_empty() {
        argv.push("-mountOptions".into());
        argv.push(options.join(",").into());
    }
    argv.push(device.into());
    argv
}

/// `diskutil unmount <mount_point>`.
pub fn diskutil_unmount_argv(mount_point: &Path) -> Vec<OsString> {
    vec!["diskutil".into(), "unmount".into(), mount_point.into()]
}

/// The devices a `diskutil list -plist` describes, in its order: each whole disk, then its
/// partitions or, for an APFS container, its volumes. `fstype` says what filesystem the disk it's
/// given holds — a partition, or a disk without a partition map — as [`filesystem_type`] reads it
/// from `diskutil info -plist`.
pub fn devices(
    list: &Value,
    mut fstype: impl FnMut(&str) -> std::result::Result<Option<String>, String>,
) -> std::result::Result<Vec<BlockDevice>, String> {
    let disks = list
        .get("AllDisksAndPartitions")
        .and_then(Value::as_array)
        .ok_or("the list has no AllDisksAndPartitions")?;
    let mut devices = Vec::new();
    for disk in disks {
        let (partitions, volumes) = (entries(disk, "Partitions"), entries(disk, "APFSVolumes"));
        let mut whole = device(disk, false)?;
        if partitions.is_empty() && volumes.is_empty() {
            whole.fstype = fstype(&whole.kname)?;
        }
        devices.push(whole);
        for entry in partitions {
            let mut partition = device(entry, true)?;
            if !holds_apfs_container(entry) {
                partition.fstype = fstype(&partition.kname)?;
            }
            devices.push(partition);
        }
        for entry in volumes {
            devices.push(BlockDevice { fstype: Some("apfs".into()), ..device(entry, false)? });
        }
    }
    Ok(devices)
}

/// The filesystem `diskutil info -plist` says a disk holds: its `FilesystemType` (`apfs`, `hfs`,
/// `msdos`, `exfat`, `ntfs`, …), which is also the type `mount -t` takes.
pub fn filesystem_type(info: &Value) -> Option<String> {
    text(info, "FilesystemType")
}

/// A disk, partition or APFS volume of the list. Only a partition has a UUID of its own (an APFS
/// volume's `DiskUUID` is its volume UUID).
fn device(entry: &Value, partition: bool) -> std::result::Result<BlockDevice, String> {
    let kname = text(entry, "DeviceIdentifier").ok_or("a disk without its DeviceIdentifier")?;
    Ok(BlockDevice {
        devnode: Path::new("/dev").join(&kname),
        uuid: text(entry, "VolumeUUID"),
        label: text(entry, "VolumeName"),
        partuuid: partition.then(|| text(entry, "DiskUUID")).flatten(),
        partition,
        kname,
        ..BlockDevice::default()
    })
}

fn entries<'a>(disk: &'a Value, key: &str) -> &'a [Value] {
    disk.get(key).and_then(Value::as_array).unwrap_or_default()
}

/// A string value, unless it's empty.
fn text(entry: &Value, key: &str) -> Option<String> {
    entry.get(key).and_then(Value::as_str).filter(|value| !value.is_empty()).map(str::to_owned)
}

/// Whether a partition is the physical store of an APFS container (`Apple_APFS`, `Apple_APFS_ISC`,
/// `Apple_APFS_Recovery`), whose volumes the list shows as disks of their own.
fn holds_apfs_container(partition: &Value) -> bool {
    partition.get("Content").and_then(Value::as_str).is_some_and(|c| c.starts_with("Apple_APFS"))
}

/// Whether `diskutil` mounts — into `/Volumes`, as the calling user — rather than `mount` into
/// `mount_base`: for `auto`, unless the device is mounted somewhere already, which Disk Arbitration
/// refuses to add to. (`udisks` is refused when the config is read.)
#[cfg(any(target_os = "macos", test))]
fn by_diskutil(method: MountMethod, top_level: bool) -> bool {
    !top_level && method != MountMethod::Mount
}

#[cfg(target_os = "macos")]
pub use native::MacOsSystem;

/// What only builds on macOS: the real machine, and its calls into the system.
#[cfg(target_os = "macos")]
mod native {
    use std::os::unix::fs::FileTypeExt;
    use std::path::PathBuf;

    use super::*;
    use crate::bsd;
    use crate::error::{Error, Result};
    use crate::escalate;
    use crate::mount;
    use crate::mountinfo::{DevNum, MountEntry};
    use crate::platform::Platform;
    use crate::plist;
    use crate::system::{self, ContainerRequest, MountRequest, System, UnmountRequest};

    /// The real machine, on macOS.
    #[derive(Debug, Clone)]
    pub struct MacOsSystem {
        /// Where Disk Arbitration mounts devices (`/Volumes`).
        pub automount_dirs: Vec<PathBuf>,
    }

    impl Default for MacOsSystem {
        fn default() -> Self {
            MacOsSystem {
                automount_dirs: Platform::MacOs
                    .automount_dirs()
                    .iter()
                    .map(PathBuf::from)
                    .collect(),
            }
        }
    }

    impl MacOsSystem {
        pub fn new() -> Self {
            Self::default()
        }

        /// Like [`MacOsSystem::new`], but `DEVMOUNTS_AUTOMOUNT_DIRS` (`:`-separated, like `PATH`)
        /// overrides the automounters' directories.
        pub fn from_env() -> Self {
            MacOsSystem {
                automount_dirs: system::automount_dirs_from_env(Self::default().automount_dirs),
            }
        }
    }

    /// What a `diskutil … -plist` prints, read.
    fn plist_output(argv: &[OsString]) -> std::result::Result<Value, String> {
        let command = || escalate::display_command(argv);
        let output = escalate::output(argv).map_err(|err| err.to_string())?;
        if !output.status.success() {
            return Err(format!("{}: {}", command(), escalate::failure_message(&output)));
        }
        let text =
            String::from_utf8(output.stdout).map_err(|_| format!("{}: not UTF-8", command()))?;
        plist::parse(&text).map_err(|why| format!("{}: {why}", command()))
    }

    impl System for MacOsSystem {
        fn block_devices(&self) -> Result<Vec<BlockDevice>> {
            let inventory = |message: String| Error::Inventory { message };
            let list = plist_output(&list_argv()).map_err(inventory)?;
            let mut devices = devices(&list, |disk| {
                plist_output(&info_argv(disk)).map(|info| filesystem_type(&info))
            })
            .map_err(inventory)?;
            for device in &mut devices {
                device.devnum = self.device_number(&device.devnode).unwrap_or_default();
            }
            Ok(devices)
        }

        fn mounts(&self) -> Result<Vec<MountEntry>> {
            let mounted = bsd::mount_table()
                .map_err(|err| Error::MountTable { message: format!("getfsstat: {err}") })?;
            Ok(bsd::entries(mounted))
        }

        fn device_number(&self, path: &Path) -> Option<DevNum> {
            system::node_number(path, FileTypeExt::is_block_device)
        }

        fn automount_dirs(&self) -> Vec<PathBuf> {
            self.automount_dirs.clone()
        }

        fn mount(&self, request: &MountRequest<'_>) -> Result<()> {
            let device = request.device;
            let fail = |err: Error| Error::MountFailed {
                device: device.describe(),
                message: mount::inner_message(err),
            };
            if by_diskutil(request.method, request.top_level) {
                escalate::run(&diskutil_mount_argv(&device.devnode, &request.options)).map_err(fail)
            } else {
                mount::mount_into_base(request).map_err(fail)
            }
        }

        /// devmounts' own mount comes down with `umount`, as `mount` put it up; any other — Disk
        /// Arbitration's, in `/Volumes` — with `diskutil`, as the calling user.
        fn unmount(&self, request: &UnmountRequest<'_>) -> Result<()> {
            if mount::is_own_mount(request) {
                return mount::unmount_with_umount(request);
            }
            let mount_point = &request.mount.mount_point;
            escalate::run(&diskutil_unmount_argv(mount_point)).map_err(|err| Error::UnmountFailed {
                mount_point: mount_point.clone(),
                message: mount::inner_message(err),
            })
        }

        fn unlock(&self, request: &ContainerRequest<'_>) -> Result<()> {
            let container = request.container.devnode.clone();
            Err(Error::UnlockFailed { container, message: mount::LUKS_NEEDS_UDISKS.into() })
        }

        fn lock(&self, request: &ContainerRequest<'_>) -> Result<()> {
            let container = request.container.devnode.clone();
            Err(Error::LockFailed { container, message: mount::LUKS_NEEDS_UDISKS.into() })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plist;

    /// A `diskutil list -plist` of an Apple-silicon Mac with an external drive: the internal
    /// disk's APFS stores, the APFS container with its volumes, and a GPT drive with an exFAT
    /// partition and an unformatted one. Shaped after `diskutil`'s own output (a sample — see the
    /// README: no Mac has been at hand to record one).
    const LIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>AllDisks</key>
	<array>
		<string>disk0</string>
		<string>disk0s1</string>
		<string>disk0s2</string>
		<string>disk3</string>
		<string>disk3s1</string>
		<string>disk3s5</string>
		<string>disk4</string>
		<string>disk4s1</string>
		<string>disk4s2</string>
		<string>disk5</string>
	</array>
	<key>AllDisksAndPartitions</key>
	<array>
		<dict>
			<key>Content</key>
			<string>GUID_partition_scheme</string>
			<key>DeviceIdentifier</key>
			<string>disk0</string>
			<key>OSInternal</key>
			<false/>
			<key>Partitions</key>
			<array>
				<dict>
					<key>Content</key>
					<string>Apple_APFS_ISC</string>
					<key>DeviceIdentifier</key>
					<string>disk0s1</string>
					<key>DiskUUID</key>
					<string>0A1B2C3D-0000-4000-8000-000000000001</string>
					<key>Size</key>
					<integer>524288000</integer>
				</dict>
				<dict>
					<key>Content</key>
					<string>Apple_APFS</string>
					<key>DeviceIdentifier</key>
					<string>disk0s2</string>
					<key>DiskUUID</key>
					<string>0A1B2C3D-0000-4000-8000-000000000002</string>
					<key>Size</key>
					<integer>494384795648</integer>
				</dict>
			</array>
			<key>Size</key>
			<integer>500277790720</integer>
		</dict>
		<dict>
			<key>APFSPhysicalStores</key>
			<array>
				<dict>
					<key>DeviceIdentifier</key>
					<string>disk0s2</string>
				</dict>
			</array>
			<key>APFSVolumes</key>
			<array>
				<dict>
					<key>DeviceIdentifier</key>
					<string>disk3s1</string>
					<key>DiskUUID</key>
					<string>5D8E6E5A-1111-4111-8111-111111111111</string>
					<key>MountPoint</key>
					<string>/System/Volumes/Data</string>
					<key>VolumeName</key>
					<string>Macintosh HD - Data</string>
					<key>VolumeUUID</key>
					<string>5D8E6E5A-1111-4111-8111-111111111111</string>
				</dict>
				<dict>
					<key>DeviceIdentifier</key>
					<string>disk3s5</string>
					<key>DiskUUID</key>
					<string>7F00AA55-2222-4222-8222-222222222222</string>
					<key>VolumeName</key>
					<string>Backup &amp; Co</string>
					<key>VolumeUUID</key>
					<string>7F00AA55-2222-4222-8222-222222222222</string>
				</dict>
			</array>
			<key>Content</key>
			<string>EF57347C-0000-11AA-AA11-00306543ECAC</string>
			<key>DeviceIdentifier</key>
			<string>disk3</string>
			<key>OSInternal</key>
			<false/>
			<key>Size</key>
			<integer>494384795648</integer>
		</dict>
		<dict>
			<key>Content</key>
			<string>GUID_partition_scheme</string>
			<key>DeviceIdentifier</key>
			<string>disk4</string>
			<key>OSInternal</key>
			<false/>
			<key>Partitions</key>
			<array>
				<dict>
					<key>Content</key>
					<string>Microsoft Basic Data</string>
					<key>DeviceIdentifier</key>
					<string>disk4s1</string>
					<key>DiskUUID</key>
					<string>3C2B1A00-3333-4333-8333-333333333333</string>
					<key>MountPoint</key>
					<string>/Volumes/MEDIA</string>
					<key>Size</key>
					<integer>1000000000000</integer>
					<key>VolumeName</key>
					<string>MEDIA</string>
					<key>VolumeUUID</key>
					<string>8E4C2F10-4444-3444-9444-444444444444</string>
				</dict>
				<dict>
					<key>Content</key>
					<string>Linux Filesystem</string>
					<key>DeviceIdentifier</key>
					<string>disk4s2</string>
					<key>DiskUUID</key>
					<string>3C2B1A00-5555-4555-8555-555555555555</string>
					<key>Size</key>
					<integer>200000000000</integer>
				</dict>
			</array>
			<key>Size</key>
			<integer>1200000000000</integer>
		</dict>
		<dict>
			<key>Content</key>
			<string></string>
			<key>DeviceIdentifier</key>
			<string>disk5</string>
			<key>MountPoint</key>
			<string>/Volumes/STICK</string>
			<key>OSInternal</key>
			<false/>
			<key>Size</key>
			<integer>16008609792</integer>
			<key>VolumeName</key>
			<string>STICK</string>
			<key>VolumeUUID</key>
			<string>1B2C3D4E-6666-3666-9666-666666666666</string>
		</dict>
	</array>
	<key>VolumesFromDisks</key>
	<array>
		<string>Macintosh HD - Data</string>
		<string>MEDIA</string>
		<string>STICK</string>
	</array>
	<key>WholeDisks</key>
	<array>
		<string>disk0</string>
		<string>disk3</string>
		<string>disk4</string>
		<string>disk5</string>
	</array>
</dict>
</plist>
"#;

    /// A `diskutil info -plist disk4s1`, cut down to a few of its many keys.
    const INFO: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Content</key>
	<string>Microsoft Basic Data</string>
	<key>DeviceIdentifier</key>
	<string>disk4s1</string>
	<key>DeviceNode</key>
	<string>/dev/disk4s1</string>
	<key>FilesystemName</key>
	<string>ExFAT</string>
	<key>FilesystemType</key>
	<string>exfat</string>
	<key>FilesystemUserVisibleName</key>
	<string>ExFAT</string>
	<key>MountPoint</key>
	<string>/Volumes/MEDIA</string>
	<key>VolumeName</key>
	<string>MEDIA</string>
	<key>WholeDisk</key>
	<false/>
</dict>
</plist>
"#;

    fn listed(fstypes: &[(&str, Option<&str>)]) -> (Vec<BlockDevice>, Vec<String>) {
        let mut asked = Vec::new();
        let devices = devices(&plist::parse(LIST).unwrap(), |disk| {
            asked.push(disk.to_owned());
            let fstype = fstypes.iter().find(|(d, _)| *d == disk).and_then(|(_, t)| *t);
            Ok(fstype.map(str::to_owned))
        })
        .unwrap();
        (devices, asked)
    }

    /// Every disk, partition and APFS volume is a device, with what identifies it: the volume's
    /// name and UUID, and for a partition its own UUID too.
    #[test]
    fn every_disk_partition_and_volume_is_a_device() {
        let (devices, _) = listed(&[("disk4s1", Some("exfat")), ("disk5", Some("msdos"))]);
        let knames: Vec<&str> = devices.iter().map(|d| d.kname.as_str()).collect();
        assert_eq!(
            knames,
            [
                "disk0", "disk0s1", "disk0s2", "disk3", "disk3s1", "disk3s5", "disk4", "disk4s1",
                "disk4s2", "disk5"
            ]
        );
        let get = |kname: &str| devices.iter().find(|d| d.kname == kname).unwrap();

        let media = get("disk4s1");
        assert_eq!(media.devnode, Path::new("/dev/disk4s1"));
        assert_eq!(media.fstype.as_deref(), Some("exfat"));
        assert_eq!(media.label.as_deref(), Some("MEDIA"));
        assert_eq!(media.uuid.as_deref(), Some("8E4C2F10-4444-3444-9444-444444444444"));
        assert_eq!(media.partuuid.as_deref(), Some("3C2B1A00-3333-4333-8333-333333333333"));
        assert!(media.partition && media.partlabel.is_none());

        let volume = get("disk3s5");
        assert_eq!(volume.fstype.as_deref(), Some("apfs"));
        assert_eq!(volume.label.as_deref(), Some("Backup & Co"));
        assert_eq!(volume.uuid.as_deref(), Some("7F00AA55-2222-4222-8222-222222222222"));
        assert!(!volume.partition && volume.partuuid.is_none(), "a volume, not a partition");

        let stick = get("disk5");
        assert_eq!(
            (stick.fstype.as_deref(), stick.label.as_deref()),
            (Some("msdos"), Some("STICK"))
        );
        assert!(!stick.partition, "a filesystem on the whole disk");

        let blank = get("disk4s2");
        assert_eq!(
            (blank.fstype.as_deref(), blank.label.as_deref(), blank.uuid.as_deref()),
            (None, None, None)
        );
        assert!(
            get("disk0").fstype.is_none() && get("disk3").fstype.is_none(),
            "maps and containers"
        );
    }

    /// `diskutil info` is asked about exactly what the list can't tell: each partition that isn't
    /// an APFS store, and each disk without a partition map or volumes.
    #[test]
    fn only_what_the_list_cannot_tell_is_asked_about() {
        let (_, asked) = listed(&[]);
        assert_eq!(asked, ["disk4s1", "disk4s2", "disk5"]);
    }

    #[test]
    fn an_unreadable_list_is_an_error() {
        let failing = devices(&plist::parse(LIST).unwrap(), |disk| Err(format!("{disk}: gone")));
        assert_eq!(failing.unwrap_err(), "disk4s1: gone", "a failed question fails the listing");
        let empty = plist::parse("<plist><dict/></plist>").unwrap();
        assert!(devices(&empty, |_| Ok(None)).unwrap_err().contains("AllDisksAndPartitions"));
        let nameless = plist::parse(
            "<plist><dict><key>AllDisksAndPartitions</key><array><dict/></array></dict></plist>",
        )
        .unwrap();
        assert!(devices(&nameless, |_| Ok(None)).unwrap_err().contains("DeviceIdentifier"));
    }

    #[test]
    fn info_tells_the_filesystem_type() {
        assert_eq!(filesystem_type(&plist::parse(INFO).unwrap()).as_deref(), Some("exfat"));
        let blank =
            plist::parse("<plist><dict><key>FilesystemType</key><string></string></dict></plist>");
        assert_eq!(filesystem_type(&blank.unwrap()), None);
    }

    #[test]
    fn diskutil_command_lines() {
        let args = |argv: Vec<OsString>| {
            argv.into_iter().map(|a| a.into_string().unwrap()).collect::<Vec<_>>()
        };
        assert_eq!(args(list_argv()), ["diskutil", "list", "-plist"]);
        assert_eq!(args(info_argv("disk4s1")), ["diskutil", "info", "-plist", "disk4s1"]);
        let disk = Path::new("/dev/disk4s1");
        assert_eq!(args(diskutil_mount_argv(disk, &[])), ["diskutil", "mount", "/dev/disk4s1"]);
        assert_eq!(
            args(diskutil_mount_argv(disk, &["ro".into(), "noatime".into()])),
            ["diskutil", "mount", "-mountOptions", "ro,noatime", "/dev/disk4s1"]
        );
        assert_eq!(
            args(diskutil_unmount_argv(Path::new("/Volumes/My Disk"))),
            ["diskutil", "unmount", "/Volumes/My Disk"]
        );
    }

    /// `auto` mounts with `diskutil`, as the desktop does; `mount`, or a device that's mounted
    /// already, with `mount`.
    #[test]
    fn auto_mounts_with_diskutil() {
        assert!(by_diskutil(MountMethod::Auto, false));
        assert!(!by_diskutil(MountMethod::Mount, false));
        assert!(!by_diskutil(MountMethod::Auto, true));
    }
}
