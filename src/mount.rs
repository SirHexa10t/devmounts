//! Mounting and unmounting the way every system has — `mount` into `<mount_base>/<uuid>`,
//! through the run's privileged helper, and `umount` — and where devices are mounted on demand,
//! and may be unmounted. Each system's own way — udisks2 on Linux, `diskutil` on macOS — is in
//! its module ([`crate::linux`], [`crate::macos`]).

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::config::is_plain_absolute;
use crate::error::{Error, Result};
use crate::escalate;
use crate::platform::Platform;
use crate::privileged::{Op, Privileges};
use crate::system::{BlockDevice, MountRequest, UnmountRequest};

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

pub(crate) fn umount_as_root(mount_point: &Path, privileges: &Privileges) -> Result<()> {
    as_root(
        &umount_argv(mount_point),
        Op::Unmount { mount_point: mount_point.to_owned() },
        privileges,
    )
}

/// A step that needs root whatever the permissions (`mount`, `umount`, `geli`): run directly when
/// we are root, by the run's privileged helper otherwise.
pub(crate) fn as_root(argv: &[OsString], op: Op, privileges: &Privileges) -> Result<()> {
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

pub(crate) fn inner_message(err: Error) -> String {
    match err {
        Error::CommandFailed { command, message } => format!("{message} (from `{command}`)"),
        other => other.to_string(),
    }
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
