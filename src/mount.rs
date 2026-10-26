//! Mounting and unmounting: the `mount` way every system has — `mount` into
//! `<mount_base>/<uuid>`, through the run's privileged helper, and `umount` — and Linux's own,
//! through udisks2 (`udisksctl`). macOS's `diskutil` is in [`crate::macos`].
//!
//! udisks2 mounts as the calling user, at the place the desktop would use
//! (`/run/media/$USER/…` or `/media/$USER/…`) and without a password for removable drives.
//! A drive udisks2 wants an administrator's password for (an internal one), a machine without
//! udisks2, or a run as root (where udisks2 would use `/run/media/root`) is mounted the `mount`
//! way instead. Unmounting is decided the same way: what `mount` put up comes down with
//! `umount`, an automounter's mount through udisks2 where it can.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::config::{MountMethod, is_plain_absolute};
use crate::error::{Error, Result};
use crate::escalate;
use crate::platform::Platform;
use crate::privileged::{Op, Privileges};
use crate::system::{BlockDevice, ContainerRequest, MountRequest, UnmountRequest};

/// What a system without udisks2 says to an unlock or a lock — neither of which comes to pass
/// there, as only Linux's inventory tells a LUKS container from any other partition.
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
pub(crate) const LUKS_NEEDS_UDISKS: &str =
    "devmounts opens and closes LUKS containers with udisks2, which only Linux has";

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

/// The `mount` way: at `<mount_base>/<uuid>`, made for it, and removed again if mounting fails.
/// The filesystem type is named where the system's `mount` doesn't find it out itself.
pub(crate) fn mount_into_base(request: &MountRequest<'_>) -> Result<()> {
    let device = request.device;
    let dir = mount_dir(&request.mount_base, device);
    create_dir(&dir, request.privileges)?;
    let fstype = device.fstype.clone().filter(|_| Platform::CURRENT.mount_needs_type());
    let options = mount_options(device, &request.options, request.top_level);
    mount_as_root(&device.devnode, &dir, fstype, options, request.privileges).inspect_err(|_| {
        // Don't leave an empty mount point behind.
        let _ = remove_dir(&dir, request.privileges);
    })
}

/// Whether the mount to come down is devmounts' own, at the `<mount_base>/<uuid>` the `mount`
/// way makes for the device.
pub(crate) fn is_own_mount(request: &UnmountRequest<'_>) -> bool {
    request.mount.mount_point == mount_dir(&request.mount_base, request.device)
}

/// The `umount` way, through the run's privileged helper; devmounts' own mount point goes with
/// the mount, as it was made for it.
pub(crate) fn unmount_with_umount(request: &UnmountRequest<'_>) -> Result<()> {
    let mount_point = &request.mount.mount_point;
    umount_as_root(mount_point, request.privileges).map_err(|err| Error::UnmountFailed {
        mount_point: mount_point.clone(),
        message: inner_message(err),
    })?;
    if is_own_mount(request) {
        let _ = remove_dir(mount_point, request.privileges);
    }
    Ok(())
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

/// `mount [-t <fstype>] [-o <options>] -- <device> <dir>`.
pub fn mount_argv(
    device: &Path,
    dir: &Path,
    fstype: Option<&str>,
    options: &[String],
) -> Vec<OsString> {
    let mut argv = vec![OsString::from("mount")];
    if let Some(fstype) = fstype {
        argv.push("-t".into());
        argv.push(fstype.into());
    }
    if !options.is_empty() {
        argv.push("-o".into());
        argv.push(options.join(",").into());
    }
    argv.extend([OsString::from("--"), device.into(), dir.into()]);
    argv
}

/// `umount -- <mount_point>`.
pub fn umount_argv(mount_point: &Path) -> Vec<OsString> {
    vec!["umount".into(), "--".into(), mount_point.into()]
}

/// Whether udisks2 refused for want of an authorization (an administrator's password).
fn not_authorized(err: &Error) -> bool {
    matches!(err, Error::CommandFailed { message, .. } if message.contains("NotAuthorized"))
}

/// `<mount_base>/<uuid>`, or `<mount_base>/<kernel name>` for devices without a usable UUID.
pub fn mount_dir(base: &Path, device: &BlockDevice) -> PathBuf {
    let name = device
        .uuid
        .as_deref()
        .filter(|u| !u.is_empty() && u.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .map(str::to_owned)
        .unwrap_or_else(|| device.kname.replace('/', "_"));
    base.join(name)
}

/// The options for the `mount` method; a btrfs top-level mount needs `subvolid=5` unless the
/// caller chose a subvolume explicitly.
pub fn mount_options(device: &BlockDevice, options: &[String], top_level: bool) -> Vec<String> {
    let mut out = options.to_vec();
    let picks_subvolume =
        options.iter().flat_map(|o| o.split(',')).any(|o| o.starts_with("subvol"));
    if top_level && device.fstype.as_deref() == Some("btrfs") && !picks_subvolume {
        out.push("subvolid=5".into());
    }
    out
}

/// Whether `path` is a place devices get mounted on demand: inside an automounter's directory, or
/// right below `mount_base`, where the `mount` method mounts ([`mount_dir`]). A devmount pointing
/// at one may be re-pointed: it's a stale mount point, or another device's — one this device was
/// swapped for.
pub fn is_mount_location(path: &Path, mount_base: &Path, automount_dirs: &[PathBuf]) -> bool {
    path.parent() == Some(mount_base) || in_automount_dir(path, automount_dirs)
}

/// Whether devmounts may unmount `mount_point`, a mount of `device`: one an automounter made, or
/// the very one the `mount` method makes for that device ([`mount_dir`]). Any other mount —
/// fstab's, the system's — is left alone, wherever `mount_base` is.
pub fn may_unmount(
    mount_point: &Path,
    device: &BlockDevice,
    mount_base: &Path,
    automount_dirs: &[PathBuf],
) -> bool {
    mount_point == mount_dir(mount_base, device) || in_automount_dir(mount_point, automount_dirs)
}

fn in_automount_dir(path: &Path, automount_dirs: &[PathBuf]) -> bool {
    // `/`, or a relative or empty entry, would make every mount point an automounter's.
    let usable = |dir: &&PathBuf| is_plain_absolute(dir) && dir.parent().is_some();
    automount_dirs.iter().filter(usable).any(|dir| path.starts_with(dir) && path != dir)
}

fn create_dir(dir: &Path, privileges: &Privileges) -> Result<()> {
    let context = || format!("can't create {}", dir.display());
    match fs::create_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => privileges
            .request(&Op::CreateDir { dir: dir.to_owned() }, Error::io(context(), &err))?
            .done(|message| Error::Io { context: context(), message }),
        Err(err) => Err(Error::io(context(), &err)),
    }
}

fn remove_dir(dir: &Path, privileges: &Privileges) -> Result<()> {
    let context = || format!("can't remove {}", dir.display());
    match fs::remove_dir(dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => privileges
            .request(&Op::RemoveDir { dir: dir.to_owned() }, Error::io(context(), &err))?
            .done(|message| Error::Io { context: context(), message }),
        Err(err) => Err(Error::io(context(), &err)),
    }
}

fn mount_as_root(
    device: &Path,
    dir: &Path,
    fstype: Option<String>,
    options: Vec<String>,
    privileges: &Privileges,
) -> Result<()> {
    let argv = mount_argv(device, dir, fstype.as_deref(), &options);
    let op = Op::Mount { fstype, options, device: device.to_owned(), dir: dir.to_owned() };
    as_root(&argv, op, privileges)
}

fn umount_as_root(mount_point: &Path, privileges: &Privileges) -> Result<()> {
    as_root(
        &umount_argv(mount_point),
        Op::Unmount { mount_point: mount_point.to_owned() },
        privileges,
    )
}

/// A step that needs root whatever the permissions (`mount`, `umount`): run directly when we are
/// root, by the run's privileged helper otherwise.
fn as_root(argv: &[OsString], op: Op, privileges: &Privileges) -> Result<()> {
    if escalate::is_root() {
        return escalate::run(argv);
    }
    let failed = |message: String| Error::CommandFailed {
        command: escalate::display_command(argv),
        message,
    };
    privileges
        .request(&op, failed("needs root, and privilege escalation is off".into()))?
        .done(failed)
}

fn os_args<const N: usize>(args: [&str; N]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

pub(crate) fn inner_message(err: Error) -> String {
    match err {
        Error::CommandFailed { command, message } => format!("{message} (from `{command}`)"),
        other => other.to_string(),
    }
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

    fn device(fstype: &str, uuid: Option<&str>) -> BlockDevice {
        BlockDevice {
            kname: "sdb1".into(),
            devnode: "/dev/sdb1".into(),
            fstype: Some(fstype.into()),
            uuid: uuid.map(Into::into),
            ..Default::default()
        }
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
        assert_eq!(
            words(mount_argv(dev, Path::new("/run/devmounts/u"), None, &opts)),
            ["mount", "-o", "ro,noatime", "--", "/dev/sdb1", "/run/devmounts/u"]
        );
        assert_eq!(
            words(mount_argv(dev, Path::new("/m"), None, &[])),
            ["mount", "--", "/dev/sdb1", "/m"]
        );
        assert_eq!(
            words(mount_argv(Path::new("/dev/msdosfs/X"), Path::new("/m"), Some("msdosfs"), &opts)),
            ["mount", "-t", "msdosfs", "-o", "ro,noatime", "--", "/dev/msdosfs/X", "/m"]
        );
        assert_eq!(words(umount_argv(Path::new("/-odd dir"))), ["umount", "--", "/-odd dir"]);
    }

    /// A failed command is named in its error exactly as it ran, quoted where a shell would need
    /// it.
    #[test]
    fn the_commands_run_are_shown_as_they_run() {
        let shown = |argv: Vec<OsString>| escalate::display_command(&argv);
        assert_eq!(
            shown(mount_argv(
                Path::new("/dev/sdb1"),
                Path::new("/run/devmounts/a b"),
                None,
                &["ro".into()]
            )),
            "mount -o ro -- /dev/sdb1 '/run/devmounts/a b'"
        );
        assert_eq!(
            shown(umount_argv(Path::new("/run/media/u/it's"))),
            r"umount -- '/run/media/u/it'\''s'"
        );
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

    #[test]
    fn mount_dirs() {
        let base = Path::new("/run/devmounts");
        assert_eq!(
            mount_dir(base, &device("ext4", Some("46b4-89"))),
            Path::new("/run/devmounts/46b4-89")
        );
        assert_eq!(mount_dir(base, &device("ext4", None)), Path::new("/run/devmounts/sdb1"));
        assert_eq!(
            mount_dir(base, &device("ext4", Some("../../etc"))),
            Path::new("/run/devmounts/sdb1")
        );
        assert_eq!(mount_dir(base, &device("ext4", Some(""))), Path::new("/run/devmounts/sdb1"));
    }

    #[test]
    fn btrfs_top_level_option() {
        let opts = |fstype: &str, given: &[&str], top: bool| {
            let given: Vec<String> = given.iter().map(|s| s.to_string()).collect();
            mount_options(&device(fstype, None), &given, top)
        };
        assert_eq!(opts("btrfs", &["ro"], true), ["ro", "subvolid=5"]);
        assert_eq!(opts("btrfs", &["ro"], false), ["ro"]);
        assert_eq!(opts("btrfs", &["ro,subvol=/@data"], true), ["ro,subvol=/@data"]);
        assert_eq!(opts("ext4", &[], true), Vec::<String>::new());
    }

    fn automount_dirs() -> Vec<PathBuf> {
        Platform::Linux.automount_dirs().iter().map(PathBuf::from).collect()
    }

    #[test]
    fn mount_locations_are_automounter_dirs_and_right_below_the_base() {
        let base = Path::new("/run/devmounts");
        let at = |p: &str| is_mount_location(Path::new(p), base, &automount_dirs());
        assert!(at("/run/media/u/X"));
        assert!(at("/media/u/X"));
        assert!(at("/run/devmounts/abc"));
        assert!(!at("/run/devmounts/abc/deeper"), "the mount method only makes <base>/<name>");
        assert!(!at("/run/media"));
        assert!(!at("/run/devmounts"));
        assert!(!at("/"));
        assert!(!at("/home"));
        assert!(!at("/mediafiles"));
        for unusable in ["/", "", "media"] {
            let dirs = [PathBuf::from(unusable)];
            assert!(
                !is_mount_location(Path::new("/home/u"), base, &dirs),
                "{unusable:?} claims every mount point"
            );
            assert!(
                !may_unmount(Path::new("/home"), &device("ext4", None), base, &dirs),
                "{unusable:?}"
            );
        }
    }

    #[test]
    fn only_automounter_mounts_and_our_own_are_unmounted() {
        let base = Path::new("/mnt");
        let data = device("ext4", Some("46b4-89"));
        let may = |p: &str| may_unmount(Path::new(p), &data, base, &automount_dirs());
        assert!(may("/mnt/46b4-89"), "what the mount method made for this device");
        assert!(may("/run/media/u/X"));
        assert!(!may("/mnt/data"), "an fstab mount that happens to sit in mount_base");
        assert!(!may("/mnt"));
        assert!(!may("/"));
        assert!(!may("/home"));
    }
}
