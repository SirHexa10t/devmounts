//! The operations behind the commands. Each takes the parsed [`Config`], run-time
//! [`Options`] and a [`System`], and returns a [`Report`] with a result per device and link;
//! one device failing doesn't stop the others.
//!
//! Per device: `<root>/<devmount>` points at the device's root wherever it's mounted, and
//! every mount link points *through* it (`<root>/<name> -> <root>/<devmount>/<path>`), so a
//! device that moves or is swapped only needs its devmount re-pointed.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::{Config, DevicePath, DeviceSpec, is_plain_absolute, validate_link_name};
use crate::error::{Error, Result};
use crate::escalate::Escalation;
use crate::link::{self, LinkState};
use crate::mount;
use crate::mountinfo::MountEntry;
use crate::privileged::Privileges;
use crate::report::{
    Command, DeviceReport, LinkOutcome, LinkReport, MountChange, MountSummary, Report,
};
use crate::resolve::{self, MountTree, ResolvedDevice};
use crate::system::{BlockDevice, MountRequest, System, UnmountRequest};

/// Run-time switches; `None`/`false` means "as configured".
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Report what would happen without changing anything.
    pub dry_run: bool,
    /// Replace symlinks that point at things not on the device (never non-symlinks).
    pub force: bool,
    /// Never mount, whatever the config says.
    pub no_mount: bool,
    /// `remove` only: also unmount the devices (only automounter/`mount_base` mounts).
    pub unmount: bool,
    /// Overrides `settings.root`.
    pub root: Option<PathBuf>,
    /// Overrides `settings.escalate`.
    pub escalation: Option<Escalation>,
    /// The program started as the privileged helper — one that calls
    /// [`privileged::try_serve`](crate::privileged::try_serve) first thing; the running program
    /// when unset.
    pub helper: Option<PathBuf>,
}

/// Mounts devices as needed and points the devmounts and mount links at them.
pub fn apply(config: &Config, options: &Options, system: &dyn System) -> Result<Report> {
    run(Command::Apply, config, options, system)
}

/// Reports the state of devices and links without changing anything.
pub fn status(config: &Config, options: &Options, system: &dyn System) -> Result<Report> {
    run(Command::Status, config, options, system)
}

/// Removes the links (only ones that are dangling or lead into their device, unless
/// forced), and with [`Options::unmount`] unmounts the devices.
pub fn remove(config: &Config, options: &Options, system: &dyn System) -> Result<Report> {
    run(Command::Remove, config, options, system)
}

/// A block device and where it's mounted, for `devmounts devices`.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceListing {
    #[serde(flatten)]
    pub device: BlockDevice,
    pub mounts: Vec<MountSummary>,
}

/// All block devices with their identifiers and reachable mounts.
pub fn list_devices(system: &dyn System) -> Result<Vec<DeviceListing>> {
    let devices = system.block_devices()?;
    let table = system.mounts()?;
    let tree = MountTree::new(&table);
    Ok(devices
        .into_iter()
        .map(|device| {
            let single = ResolvedDevice {
                device: device.clone(),
                members: vec![device.clone()],
                containers: Vec::new(),
            };
            let mounts = resolve::device_mounts(&single, &tree, |p| system.device_number(p));
            DeviceListing { device, mounts: summarize(&mounts) }
        })
        .collect())
}

fn run(
    command: Command,
    config: &Config,
    options: &Options,
    system: &dyn System,
) -> Result<Report> {
    if let Some(root) = &options.root
        && !is_plain_absolute(root)
    {
        let problem =
            format!("the link root must be an absolute path without '..' (got {})", root.display());
        return Err(Error::ConfigInvalid { problems: vec![problem] });
    }
    let ctx = Context {
        command,
        config,
        options,
        system,
        devices: system.block_devices()?,
        automount_dirs: system.automount_dirs(),
        root: options.root.clone().unwrap_or_else(|| config.settings.root.clone()),
        privileges: Privileges::new(
            options.escalation.clone().unwrap_or_else(|| config.settings.escalation.clone()),
            options.helper.clone(),
        ),
    };
    // A machine whose mount table can't be read stops the run here, before anything is changed.
    // Each device reads the table again when its turn comes (one before it may have mounted or
    // unmounted), and a read failing then is that device's problem: the report keeps everything
    // done before it.
    system.mounts()?;
    let devices = config
        .devices
        .iter()
        .map(|spec| match command {
            Command::Apply => ctx.apply_device(spec),
            Command::Status => ctx.status_device(spec),
            Command::Remove => ctx.remove_device(spec),
        })
        .collect();
    Ok(Report { command, dry_run: options.dry_run, root: ctx.root, devices })
}

struct Context<'a> {
    command: Command,
    config: &'a Config,
    options: &'a Options,
    system: &'a dyn System,
    devices: Vec<BlockDevice>,
    automount_dirs: Vec<PathBuf>,
    root: PathBuf,
    /// Shared by every step of the run, so it asks for a password at most once.
    privileges: Privileges,
}

/// What a device's mounts look like, for one device in one run.
struct Mounted<'t> {
    tree: MountTree<'t>,
    mounts: Vec<&'t MountEntry>,
    ids: HashSet<u32>,
}

impl<'t> Mounted<'t> {
    fn new(ctx: &Context<'_>, device: Option<&ResolvedDevice>, table: &'t [MountEntry]) -> Self {
        let tree = MountTree::new(table);
        let mounts = device
            .map(|d| resolve::device_mounts(d, &tree, |p| ctx.system.device_number(p)))
            .unwrap_or_default();
        let ids = mounts.iter().map(|m| m.id).collect();
        Mounted { tree, mounts, ids }
    }

    /// Whether a resolved path lies on the device.
    fn on_device(&self, path: &Path) -> bool {
        self.tree.mount_at(path).is_some_and(|m| self.ids.contains(&m.id))
    }

    /// Whether a devmount pointing at `path` may be re-pointed: on the device, or where
    /// devices get mounted (a stale mount directory, or another device it was swapped for).
    fn devmount_may_replace(
        &self,
        path: &Path,
        mount_base: &Path,
        automount_dirs: &[PathBuf],
    ) -> bool {
        self.on_device(path) || mount::is_mount_location(path, mount_base, automount_dirs)
    }

    /// The device's root directory, resolved, or why it can't be reached.
    fn device_root(&self, device: &ResolvedDevice) -> Result<PathBuf> {
        match resolve::locate(&DevicePath::root(), &self.mounts) {
            None => Err(unreachable_error(device, &self.mounts)),
            Some(location) => resolve::check_target(&location.path, &self.mounts, &self.tree),
        }
    }

    /// Checks that `path` (relative to the device root) exists there as a directory.
    fn check_inside(&self, device_root: &Path, path: &DevicePath) -> Result<PathBuf> {
        resolve::check_target(&device_root.join(path.relative()), &self.mounts, &self.tree)
    }
}

impl Context<'_> {
    fn find(&self, spec: &DeviceSpec, report: &mut DeviceReport) -> Option<ResolvedDevice> {
        match resolve::find_device(&spec.id, &self.devices) {
            Ok(found) => {
                report.device = Some(found.device.describe());
                report.containers = found.containers.iter().map(BlockDevice::describe).collect();
                Some(found)
            }
            Err(err) => {
                report.not_connected = matches!(err, Error::DeviceNotFound { .. });
                // Not being connected is no failure for `remove`: its links come down all the same.
                if !(report.not_connected && self.command == Command::Remove) {
                    report.error = Some(err);
                }
                None
            }
        }
    }

    /// The mount table as it is now, or `None` with the failed read recorded as the device's
    /// problem.
    fn mount_table(&self, report: &mut DeviceReport) -> Option<Vec<MountEntry>> {
        match self.system.mounts() {
            Ok(table) => Some(table),
            Err(err) => {
                report.error = Some(err);
                None
            }
        }
    }

    /// The name check again, as the root may have been overridden to `/` at run time.
    fn check_name(&self, name: &str) -> Result<()> {
        validate_link_name(name, self.root == Path::new("/"))
            .map_err(|why| Error::ConfigInvalid { problems: vec![format!("link '{name}': {why}")] })
    }

    // --- apply ---------------------------------------------------------------------------

    fn apply_device(&self, spec: &DeviceSpec) -> DeviceReport {
        let mut report = DeviceReport::new(&self.root, spec);
        let Some(device) = self.find(spec, &mut report) else { return report };
        let Some(mut table) = self.mount_table(&mut report) else { return report };
        // The devmount needs the device's root; mount it if no mount shows it. If the device
        // is mounted, but only in parts (btrfs subvolumes), its top level is mounted too.
        let needs_mount = {
            let mounted = Mounted::new(self, Some(&device), &table);
            resolve::locate(&DevicePath::root(), &mounted.mounts)
                .is_none()
                .then_some(!mounted.mounts.is_empty())
        };
        let mut pending = false;
        if let Some(top_level) = needs_mount
            && spec.automount(&self.config.settings)
            && !self.options.no_mount
        {
            if self.options.dry_run {
                report.mount_changes.push(MountChange::WouldMount { top_level });
                pending = true;
            } else {
                let settings = &self.config.settings;
                let request = MountRequest {
                    device: &device.device,
                    options: spec.mount_options(settings).to_vec(),
                    method: settings.mount_method,
                    mount_base: settings.mount_base.clone(),
                    top_level,
                    privileges: &self.privileges,
                };
                match self.system.mount(&request) {
                    Ok(()) => {
                        report.mount_changes.push(MountChange::Mounted { top_level });
                        // Where it was mounted is only known from the table read again.
                        let Some(mounted) = self.mount_table(&mut report) else { return report };
                        table = mounted;
                    }
                    Err(err) => report.error = Some(err),
                }
            }
        }

        let mounted = Mounted::new(self, Some(&device), &table);
        report.mounted_at = summarize(&mounted.mounts);
        let on_device = |p: &Path| mounted.on_device(p);
        let base = &self.config.settings.mount_base;
        let devmount_owned = |p: &Path| mounted.devmount_may_replace(p, base, &self.automount_dirs);

        // A pending mount (dry run) means the root isn't known yet.
        let device_root = if pending { Ok(None) } else { mounted.device_root(&device).map(Some) };
        report.devmount = match &device_root {
            Ok(root) => self.put_link(report.devmount.clone(), root.as_deref(), &devmount_owned),
            Err(err) => report.devmount.clone().fail(err.clone()),
        };
        if !report.devmount.ok {
            return report; // the mount links would lead nowhere (or somewhere wrong)
        }

        for (link, mount) in report.mounts.iter_mut().zip(&spec.mounts) {
            if let Ok(Some(root)) = &device_root
                && let Err(err) = mounted.check_inside(root, &mount.path)
            {
                *link = link.clone().fail(err);
                continue;
            }
            let target = spec.link_target(&self.root, &mount.path);
            *link = self.put_link(link.clone(), Some(&target), &on_device);
        }
        report
    }

    /// Makes `report.link` point at `target`. Without a target (dry run waiting for a mount)
    /// only reports roughly what would happen.
    fn put_link(
        &self,
        mut report: LinkReport,
        target: Option<&Path>,
        on_device: &dyn Fn(&Path) -> bool,
    ) -> LinkReport {
        if let Err(err) = self.check_name(&report.name) {
            return report.fail(err);
        }
        report.target = target.map(Path::to_path_buf);
        let state = match link::inspect(&report.link, target, on_device) {
            Ok(state) => state,
            Err(err) => return report.fail(err),
        };
        report.current = current_of(&state);

        let create = match state {
            LinkState::Correct => return report.finish(LinkOutcome::Unchanged, true),
            LinkState::Missing => true,
            LinkState::Dangling { .. } | LinkState::Elsewhere { on_device: true, .. } => false,
            LinkState::Elsewhere { current, on_device: false } => {
                if !self.options.force {
                    let link = report.link.clone();
                    return report.fail(Error::LinkConflict { link, current });
                }
                false
            }
            LinkState::Blocked { kind } => {
                let link = report.link.clone();
                return report.fail(Error::LinkBlocked { link, kind });
            }
        };

        let planned = if create { LinkOutcome::WouldCreate } else { LinkOutcome::WouldUpdate };
        let Some(target) = target.filter(|_| !self.options.dry_run) else {
            return report.finish(planned, true);
        };
        match link::set_link(&report.link, target, &self.privileges) {
            Ok(()) => report
                .finish(if create { LinkOutcome::Created } else { LinkOutcome::Updated }, true),
            Err(err) => report.fail(err),
        }
    }

    // --- status --------------------------------------------------------------------------

    fn status_device(&self, spec: &DeviceSpec) -> DeviceReport {
        let mut report = DeviceReport::new(&self.root, spec);
        let Some(table) = self.mount_table(&mut report) else { return report };
        let found = self.find(spec, &mut report);
        let mounted = Mounted::new(self, found.as_ref(), &table);
        report.mounted_at = summarize(&mounted.mounts);
        let on_device = |p: &Path| mounted.on_device(p);
        let base = &self.config.settings.mount_base;
        let devmount_owned = |p: &Path| mounted.devmount_may_replace(p, base, &self.automount_dirs);

        // `Ok(None)`: the device is missing (reported for the device); compare what's there.
        let device_root = match &found {
            None => Ok(None),
            Some(device) => mounted.device_root(device).map(Some),
        };
        report.devmount =
            self.check_link(report.devmount.clone(), device_root.clone(), &devmount_owned);

        for (link, mount) in report.mounts.iter_mut().zip(&spec.mounts) {
            let target = spec.link_target(&self.root, &mount.path);
            let expected = match &device_root {
                Ok(Some(root)) => mounted.check_inside(root, &mount.path).map(|_| Some(target)),
                _ => Ok(Some(target)),
            };
            *link = self.check_link(link.clone(), expected, &on_device);
        }
        report
    }

    fn check_link(
        &self,
        mut report: LinkReport,
        expected: Result<Option<PathBuf>>,
        on_device: &dyn Fn(&Path) -> bool,
    ) -> LinkReport {
        if let Err(err) = self.check_name(&report.name) {
            return report.fail(err);
        }
        let target = expected.as_ref().ok().and_then(|t| t.as_deref());
        report.target = target.map(Path::to_path_buf);
        let state = match link::inspect(&report.link, target, on_device) {
            Ok(state) => state,
            Err(err) => return report.fail(err),
        };
        report.current = current_of(&state);
        match (state, expected) {
            (LinkState::Blocked { kind }, _) => {
                let link = report.link.clone();
                report.fail(Error::LinkBlocked { link, kind })
            }
            (_, Err(err)) => report.fail(err),
            (LinkState::Correct, Ok(_)) => report.finish(LinkOutcome::Unchanged, true),
            (LinkState::Missing, Ok(_)) => report.finish(LinkOutcome::Missing, false),
            (LinkState::Elsewhere { current, on_device: false }, Ok(Some(_))) => {
                let link = report.link.clone();
                report.fail(Error::LinkConflict { link, current })
            }
            (_, Ok(_)) => report.finish(LinkOutcome::Outdated, false),
        }
    }

    // --- remove --------------------------------------------------------------------------

    fn remove_device(&self, spec: &DeviceSpec) -> DeviceReport {
        let mut report = DeviceReport::new(&self.root, spec);
        let Some(table) = self.mount_table(&mut report) else { return report };
        let found = self.find(spec, &mut report);
        let mounted = Mounted::new(self, found.as_ref(), &table);
        let on_device = |p: &Path| mounted.on_device(p);
        let base = &self.config.settings.mount_base;
        let devmount_owned = |p: &Path| mounted.devmount_may_replace(p, base, &self.automount_dirs);

        // The mount links first, then the devmount they lead through.
        for (link, mount) in report.mounts.iter_mut().zip(&spec.mounts) {
            let target = spec.link_target(&self.root, &mount.path);
            *link = self.take_down(link.clone(), Some(&target), &on_device);
        }
        report.devmount = self.take_down(report.devmount.clone(), None, &devmount_owned);

        if self.options.unmount
            && let Some(device) = &found
        {
            self.unmount_device(device, &mounted.mounts, &mut report);
            // Where it's still mounted is only known from the table read again; each unmount's
            // own outcome is recorded already, so a failed read takes nothing with it.
            if let Some(table) = self.mount_table(&mut report) {
                report.mounted_at = summarize(&Mounted::new(self, Some(device), &table).mounts);
            }
        } else {
            report.mounted_at = summarize(&mounted.mounts);
        }
        report
    }

    fn take_down(
        &self,
        mut report: LinkReport,
        target: Option<&Path>,
        on_device: &dyn Fn(&Path) -> bool,
    ) -> LinkReport {
        if let Err(err) = self.check_name(&report.name) {
            return report.fail(err);
        }
        let state = match link::inspect(&report.link, target, on_device) {
            Ok(state) => state,
            Err(err) => return report.fail(err),
        };
        report.current = current_of(&state);
        if matches!(state, LinkState::Correct) {
            report.target = target.map(Path::to_path_buf);
        }
        match state {
            LinkState::Missing => report.finish(LinkOutcome::Missing, true),
            LinkState::Blocked { kind } => {
                let link = report.link.clone();
                report.fail(Error::LinkBlocked { link, kind })
            }
            LinkState::Elsewhere { current, on_device: false } if !self.options.force => {
                let link = report.link.clone();
                report.fail(Error::LinkConflict { link, current })
            }
            _ if self.options.dry_run => report.finish(LinkOutcome::WouldRemove, true),
            _ => match link::remove_link(&report.link, &self.privileges) {
                Ok(()) => report.finish(LinkOutcome::Removed, true),
                Err(err) => report.fail(err),
            },
        }
    }

    fn unmount_device(
        &self,
        device: &ResolvedDevice,
        mounts: &[&MountEntry],
        report: &mut DeviceReport,
    ) {
        let base = &self.config.settings.mount_base;
        let mut ordered: Vec<&MountEntry> = mounts.to_vec();
        // Deepest first, so nested mounts don't keep their parents busy.
        ordered.sort_by_key(|m| std::cmp::Reverse(m.mount_point.components().count()));
        for mount in ordered {
            let mount_point = mount.mount_point.clone();
            if !mount::may_unmount(&mount_point, &device.device, base, &self.automount_dirs) {
                report.mount_changes.push(MountChange::LeftMounted { mount_point });
            } else if self.options.dry_run {
                report.mount_changes.push(MountChange::WouldUnmount { mount_point });
            } else {
                let request = UnmountRequest {
                    device: &device.device,
                    mount,
                    method: self.config.settings.mount_method,
                    mount_base: base.clone(),
                    privileges: &self.privileges,
                };
                // Each mount's outcome is its own: one failing doesn't hide another, nor stop it
                // being tried.
                report.mount_changes.push(match self.system.unmount(&request) {
                    Ok(()) => MountChange::Unmounted { mount_point },
                    Err(error) => MountChange::UnmountFailed { mount_point, error },
                });
            }
        }
    }
}

fn current_of(state: &LinkState) -> Option<PathBuf> {
    match state {
        LinkState::Dangling { current } | LinkState::Elsewhere { current, .. } => {
            Some(current.clone())
        }
        _ => None,
    }
}

fn unreachable_error(device: &ResolvedDevice, mounts: &[&MountEntry]) -> Error {
    let name = device.device.describe();
    if mounts.is_empty() {
        Error::NotMounted { device: name }
    } else {
        Error::PathNotExposed {
            device: name,
            path: ".".into(),
            exposed: resolve::exposed_parts(mounts),
        }
    }
}

pub fn summarize(mounts: &[&MountEntry]) -> Vec<MountSummary> {
    mounts
        .iter()
        .map(|m| MountSummary {
            mount_point: m.mount_point.clone(),
            root: m.root.clone(),
            fstype: m.fstype.clone(),
            read_only: m.read_only(),
        })
        .collect()
}
