//! Linux: devices from sysfs and the udev database (or, without udev, `lsblk`), the mount table
//! from `/proc/self/mountinfo`, and mounting through udisks2 (`udisksctl`) or the `mount` way
//! ([`crate::mount`]).
//!
//! udisks2 mounts as the calling user, at the place the desktop would use
//! (`/run/media/$USER/…` or `/media/$USER/…`) and without a password for removable drives.
//! A drive udisks2 wants an administrator's password for (an internal one), a machine without
//! udisks2, or a run as root (where udisks2 would use `/run/media/root`) is mounted the `mount`
//! way instead. Unmounting is decided the same way: what `mount` put up comes down with
//! `umount`, an automounter's mount through udisks2 where it can. LUKS containers are opened and
//! closed with udisks2 too.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

use crate::config::MountMethod;
use crate::error::{Error, Result};
use crate::escalate;
use crate::mount::{
    inner_message, is_own_mount, mount_into_base, umount_as_root, unmount_with_umount,
};
use crate::mountinfo::{self, DevNum, MountEntry};
use crate::platform::Platform;
use crate::system::{
    BlockDevice, ContainerRequest, MountRequest, System, UnmountRequest, automount_dirs_from_env,
    node_number,
};
use crate::udev::{self, Identity};

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
        mount(request)
    }

    fn unmount(&self, request: &UnmountRequest<'_>) -> Result<()> {
        unmount(request)
    }

    fn unlock(&self, request: &ContainerRequest<'_>) -> Result<()> {
        unlock(request)
    }

    fn lock(&self, request: &ContainerRequest<'_>) -> Result<()> {
        lock(request)
    }
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

/// What actually mounts or unmounts, once `auto` is decided: udisks2, or `mount` / `umount`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Udisks,
    Mount,
}

/// Whether udisks2 may ask for a password itself (a polkit prompt) or must refuse instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Polkit {
    /// `mount_method = "udisks"`: udisks2 is what was asked for, its own prompt included —
    /// mounting and unmounting alike.
    MayAsk,
    /// Any other method: a drive that needs a password is done the `mount` / `umount` way instead,
    /// through the run's one privileged helper, so a run never asks twice.
    MustNotAsk,
}

impl Polkit {
    /// What `method` lets udisks2 do.
    pub fn of(method: MountMethod) -> Self {
        match method {
            MountMethod::Udisks => Polkit::MayAsk,
            MountMethod::Auto | MountMethod::Mount => Polkit::MustNotAsk,
        }
    }
}

/// Decides how to mount: `top_level` mounts always use `mount`, because udisks2 refuses to
/// mount a device that is already mounted somewhere.
pub fn choose(method: MountMethod, top_level: bool) -> Via {
    match method {
        _ if top_level => Via::Mount,
        MountMethod::Udisks => Via::Udisks,
        MountMethod::Mount => Via::Mount,
        MountMethod::Auto if !escalate::is_root() && udisks_available() => Via::Udisks,
        MountMethod::Auto => Via::Mount,
    }
}

/// Decides how to unmount: devmounts' own `<mount_base>/<uuid>` mount always with `umount`, as
/// `mount` put it up; any other — an automounter's — through udisks2 when that's the method, or
/// when it's there.
pub fn choose_unmount(method: MountMethod, own_mount: bool) -> Via {
    match method {
        _ if own_mount => Via::Mount,
        MountMethod::Udisks => Via::Udisks,
        MountMethod::Auto | MountMethod::Mount if udisks_available() => Via::Udisks,
        MountMethod::Auto | MountMethod::Mount => Via::Mount,
    }
}

pub fn udisks_available() -> bool {
    escalate::find_program("udisksctl").is_some()
        && escalate::output(&[OsString::from("udisksctl"), OsString::from("status")])
            .is_ok_and(|output| output.status.success())
}

pub fn mount(request: &MountRequest<'_>) -> Result<()> {
    let device = request.device;
    let fail = |message: String| Error::MountFailed { device: device.describe(), message };
    match choose(request.method, request.top_level) {
        Via::Udisks => {
            if let Some(why) = udisksctl_missing() {
                return Err(fail(why));
            }
            let polkit = Polkit::of(request.method);
            match escalate::run(&udisks_mount_argv(&device.devnode, &request.options, polkit)) {
                Ok(()) => Ok(()),
                Err(err) if polkit == Polkit::MustNotAsk && not_authorized(&err) => {
                    mount_into_base(request)
                        .map_err(|err| fail(after_refusal("mounting it with `mount`", err)))
                }
                Err(err) => Err(fail(inner_message(err))),
            }
        }
        Via::Mount => mount_into_base(request).map_err(|err| fail(inner_message(err))),
    }
}

pub fn unmount(request: &UnmountRequest<'_>) -> Result<()> {
    let mount_point = &request.mount.mount_point;
    let fail = |message: String| Error::UnmountFailed { mount_point: mount_point.clone(), message };
    match choose_unmount(request.method, is_own_mount(request)) {
        Via::Mount => unmount_with_umount(request),
        Via::Udisks => {
            if let Some(why) = udisksctl_missing() {
                return Err(fail(why));
            }
            let polkit = Polkit::of(request.method);
            match escalate::run(&udisks_unmount_argv(&request.device.devnode, polkit)) {
                Ok(()) => Ok(()),
                Err(err) if polkit == Polkit::MustNotAsk && not_authorized(&err) => {
                    umount_as_root(mount_point, request.privileges)
                        .map_err(|err| fail(after_refusal("unmounting it with `umount`", err)))
                }
                Err(err) => Err(fail(inner_message(err))),
            }
        }
    }
}

/// Why `udisksctl` can't be run, when it isn't installed.
fn udisksctl_missing() -> Option<String> {
    escalate::find_program("udisksctl")
        .is_none()
        .then(|| "udisks2 isn't installed (from `udisksctl`)".to_owned())
}

/// Opens a locked LUKS container with udisks2, which asks for the passphrase itself: on the
/// terminal, where the command runs ([`escalate::run_on_terminal`]), so the passphrase never
/// passes through devmounts.
pub fn unlock(request: &ContainerRequest<'_>) -> Result<()> {
    let container = &request.container.devnode;
    let fail = |message: String| Error::UnlockFailed { container: container.clone(), message };
    if let Some(why) = udisksctl_missing() {
        return Err(fail(format!("{why}; open it yourself first")));
    }
    let polkit = Polkit::of(request.method);
    escalate::run_on_terminal(&udisks_unlock_argv(container, polkit)).map_err(|err| {
        let ask = match polkit {
            Polkit::MustNotAsk => {
                "; if it wanted an administrator's password, `mount_method = \"udisks\"` lets it ask"
            }
            Polkit::MayAsk => "",
        };
        fail(format!("{} — its reason is above{ask}", inner_message(err)))
    })
}

/// Closes a LUKS container with udisks2 again.
pub fn lock(request: &ContainerRequest<'_>) -> Result<()> {
    let container = &request.container.devnode;
    let fail = |message: String| Error::LockFailed { container: container.clone(), message };
    if let Some(why) = udisksctl_missing() {
        return Err(fail(why));
    }
    escalate::run(&udisks_lock_argv(container, Polkit::of(request.method)))
        .map_err(|err| fail(inner_message(err)))
}

/// `udisksctl mount --block-device <device> [--options <options>] [--no-user-interaction]`.
pub fn udisks_mount_argv(device: &Path, options: &[String], polkit: Polkit) -> Vec<OsString> {
    let mut argv = os_args(["udisksctl", "mount", "--block-device"]);
    argv.push(device.into());
    if !options.is_empty() {
        argv.push("--options".into());
        argv.push(options.join(",").into());
    }
    argv.extend(no_interaction(polkit));
    argv
}

/// `udisksctl unmount --block-device <device> [--no-user-interaction]`.
pub fn udisks_unmount_argv(device: &Path, polkit: Polkit) -> Vec<OsString> {
    udisks_device_argv("unmount", device, polkit)
}

/// `udisksctl unlock --block-device <container> [--no-user-interaction]`.
pub fn udisks_unlock_argv(container: &Path, polkit: Polkit) -> Vec<OsString> {
    udisks_device_argv("unlock", container, polkit)
}

/// `udisksctl lock --block-device <container> [--no-user-interaction]`.
pub fn udisks_lock_argv(container: &Path, polkit: Polkit) -> Vec<OsString> {
    udisks_device_argv("lock", container, polkit)
}

/// `udisksctl <verb> --block-device <device> [--no-user-interaction]`: every udisks2 step but a
/// mount, which takes options too.
fn udisks_device_argv(verb: &str, device: &Path, polkit: Polkit) -> Vec<OsString> {
    let mut argv = os_args(["udisksctl", verb, "--block-device"]);
    argv.push(device.into());
    argv.extend(no_interaction(polkit));
    argv
}

fn no_interaction(polkit: Polkit) -> Option<OsString> {
    match polkit {
        Polkit::MayAsk => None,
        Polkit::MustNotAsk => Some("--no-user-interaction".into()),
    }
}

/// Whether udisks2 refused for want of an authorization (an administrator's password).
fn not_authorized(err: &Error) -> bool {
    matches!(err, Error::CommandFailed { message, .. } if message.contains("NotAuthorized"))
}

fn os_args<const N: usize>(args: [&str; N]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

/// Why the fallback for a drive udisks2 refused failed — said with the refusal, so the first of the
/// two attempts isn't hidden by the second.
fn after_refusal(fallback: &str, err: Error) -> String {
    format!(
        "udisks2 wants an administrator's password for it, and {fallback} instead failed: {}",
        inner_message(err)
    )
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
    fn the_commands_run_are_exactly_these() {
        let words = |argv: Vec<OsString>| {
            argv.into_iter().map(|a| a.into_string().unwrap()).collect::<Vec<_>>()
        };
        let dev = Path::new("/dev/sdb1");
        let opts: Vec<String> = vec!["ro".into(), "noatime".into()];
        assert_eq!(
            words(udisks_mount_argv(dev, &opts, Polkit::MustNotAsk)),
            [
                "udisksctl",
                "mount",
                "--block-device",
                "/dev/sdb1",
                "--options",
                "ro,noatime",
                "--no-user-interaction"
            ]
        );
        assert_eq!(
            words(udisks_mount_argv(dev, &[], Polkit::MayAsk)),
            ["udisksctl", "mount", "--block-device", "/dev/sdb1"]
        );
        assert_eq!(
            words(udisks_unmount_argv(dev, Polkit::MustNotAsk)),
            ["udisksctl", "unmount", "--block-device", "/dev/sdb1", "--no-user-interaction"]
        );
        assert_eq!(
            words(udisks_unmount_argv(dev, Polkit::MayAsk)),
            ["udisksctl", "unmount", "--block-device", "/dev/sdb1"]
        );
        assert_eq!(
            words(udisks_unlock_argv(dev, Polkit::MustNotAsk)),
            ["udisksctl", "unlock", "--block-device", "/dev/sdb1", "--no-user-interaction"]
        );
        assert_eq!(
            words(udisks_unlock_argv(dev, Polkit::MayAsk)),
            ["udisksctl", "unlock", "--block-device", "/dev/sdb1"]
        );
        assert_eq!(
            words(udisks_lock_argv(dev, Polkit::MustNotAsk)),
            ["udisksctl", "lock", "--block-device", "/dev/sdb1", "--no-user-interaction"]
        );
        assert_eq!(
            words(udisks_lock_argv(dev, Polkit::MayAsk)),
            ["udisksctl", "lock", "--block-device", "/dev/sdb1"]
        );
    }

    /// A failed command is named in its error exactly as it ran, quoted where a shell would need
    /// it.
    #[test]
    fn the_commands_run_are_shown_as_they_run() {
        let shown = |argv: Vec<OsString>| escalate::display_command(&argv);
        assert_eq!(
            shown(udisks_mount_argv(Path::new("/dev/sdb1"), &[], Polkit::MustNotAsk)),
            "udisksctl mount --block-device /dev/sdb1 --no-user-interaction"
        );
    }

    #[test]
    fn udisks_refusing_for_want_of_a_password_is_told_apart() {
        let refusal = |message: &str| Error::CommandFailed {
            command: "udisksctl mount".into(),
            message: message.into(),
        };
        assert!(not_authorized(&refusal(
            "Error mounting /dev/sdb1: GDBus.Error:org.freedesktop.UDisks2.Error.NotAuthorizedCanObtain: Not authorized to perform operation"
        )));
        assert!(!not_authorized(&refusal("Error mounting /dev/sdb1: wrong fs type")));
        assert!(!not_authorized(&Error::Io {
            context: "x".into(),
            message: "NotAuthorized".into()
        }));
    }

    #[test]
    fn top_level_mounts_never_use_udisks() {
        assert_eq!(choose(MountMethod::Udisks, true), Via::Mount);
        assert_eq!(choose(MountMethod::Auto, true), Via::Mount);
        assert_eq!(choose(MountMethod::Udisks, false), Via::Udisks);
        assert_eq!(choose(MountMethod::Mount, false), Via::Mount);
    }

    /// What `mount` put up comes down with `umount`, whatever the method; with `udisks`, an
    /// automounter's mount comes down through udisks2 alone, its own prompt allowed — as it would
    /// have gone up. (`auto` and `mount` ask whether udisks2 is there, so the machine decides
    /// those.)
    #[test]
    fn unmounting_is_decided_the_way_mounting_is() {
        for method in [MountMethod::Auto, MountMethod::Udisks, MountMethod::Mount] {
            assert_eq!(choose_unmount(method, true), Via::Mount, "{method:?}");
        }
        assert_eq!(choose_unmount(MountMethod::Udisks, false), Via::Udisks);
        assert_eq!(Polkit::of(MountMethod::Udisks), Polkit::MayAsk);
        assert_eq!(Polkit::of(MountMethod::Auto), Polkit::MustNotAsk);
        assert_eq!(Polkit::of(MountMethod::Mount), Polkit::MustNotAsk);
    }
}
