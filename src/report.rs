//! What a run found and did, per device and per link. Serializable for `--json`, and
//! renderable as text for terminals.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::{DeviceId, DevicePath, DeviceSpec, LinkSpec};
use crate::error::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Command {
    Apply,
    Status,
    Remove,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub command: Command,
    pub dry_run: bool,
    pub root: PathBuf,
    pub devices: Vec<DeviceReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceReport {
    pub id: DeviceId,
    /// The device found for the id, e.g. `/dev/sdb1 (btrfs "BACKUP")`.
    pub device: Option<String>,
    /// No device with the id is connected. A problem for `apply` and `status` (it's in `error` then
    /// too), but none for `remove`, which takes the links down all the same.
    pub not_connected: bool,
    /// Containers on the way to the filesystem (an unlocked LUKS partition, …).
    pub containers: Vec<String>,
    /// Where the device's filesystem is mounted (after any mounting done in this run).
    pub mounted_at: Vec<MountSummary>,
    /// What this run mounted and unmounted, or would (dry run) — every unmount's outcome its own.
    pub mount_changes: Vec<MountChange>,
    /// A device-level problem (not found, ambiguous, mount failed, the mount table unreadable, …).
    pub error: Option<Error>,
    /// The device's root link.
    pub devmount: LinkReport,
    /// The links that lead into the device through the devmount.
    pub mounts: Vec<LinkReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MountSummary {
    pub mount_point: PathBuf,
    /// What part of the filesystem is mounted there (`/`, a subvolume, a subdirectory).
    pub root: PathBuf,
    pub fstype: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum MountChange {
    /// Mounted in this run; `top_level` when added next to existing (subvolume) mounts.
    Mounted {
        top_level: bool,
    },
    WouldMount {
        top_level: bool,
    },
    Unmounted {
        mount_point: PathBuf,
    },
    WouldUnmount {
        mount_point: PathBuf,
    },
    /// Unmounting it failed; the device's other mounts were still tried.
    UnmountFailed {
        mount_point: PathBuf,
        error: Error,
    },
    /// Not ours to unmount: neither an automounter's mount nor one devmounts made (fstab, system).
    LeftMounted {
        mount_point: PathBuf,
    },
}

impl MountChange {
    /// Whether this change counts as a problem: an unmount that failed.
    pub fn is_problem(&self) -> bool {
        matches!(self, MountChange::UnmountFailed { .. })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LinkReport {
    pub name: String,
    pub link: PathBuf,
    /// The path inside the device (`.` for the devmount).
    pub path: DevicePath,
    /// Where the link should point, when that could be worked out.
    pub target: Option<PathBuf>,
    /// Where it pointed before this run (for symlinks).
    pub current: Option<PathBuf>,
    pub outcome: LinkOutcome,
    /// Whether this outcome counts as success for the command that produced it.
    pub ok: bool,
}

impl LinkReport {
    /// Whether this link counts as a problem: it isn't `ok`, and wasn't merely skipped — a skipped
    /// link's cause (its device or devmount) is a problem of its own.
    pub fn is_problem(&self) -> bool {
        !self.ok && !matches!(self.outcome, LinkOutcome::Skipped)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum LinkOutcome {
    /// Already correct.
    Unchanged,
    Created,
    Updated,
    Removed,
    /// Dry run: what would happen.
    WouldCreate,
    WouldUpdate,
    WouldRemove,
    /// Nothing at the link path.
    Missing,
    /// The link exists but points somewhere else on the device, or nowhere.
    Outdated,
    /// Not looked at because of a problem with the device or its devmount.
    Skipped,
    Failed {
        error: Error,
    },
}

impl LinkReport {
    pub fn new(root: &Path, spec: &LinkSpec) -> Self {
        LinkReport {
            name: spec.name.clone(),
            link: root.join(&spec.name),
            path: spec.path.clone(),
            target: None,
            current: None,
            outcome: LinkOutcome::Skipped,
            ok: false,
        }
    }

    pub(crate) fn finish(mut self, outcome: LinkOutcome, ok: bool) -> Self {
        self.outcome = outcome;
        self.ok = ok;
        self
    }

    pub(crate) fn fail(self, error: Error) -> Self {
        self.finish(LinkOutcome::Failed { error }, false)
    }
}

impl DeviceReport {
    /// A report with every link still `Skipped`.
    pub fn new(root: &Path, spec: &DeviceSpec) -> Self {
        let devmount = LinkSpec { name: spec.devmount.clone(), path: DevicePath::root() };
        DeviceReport {
            id: spec.id.clone(),
            device: None,
            not_connected: false,
            containers: Vec::new(),
            mounted_at: Vec::new(),
            mount_changes: Vec::new(),
            error: None,
            devmount: LinkReport::new(root, &devmount),
            mounts: spec.mounts.iter().map(|m| LinkReport::new(root, m)).collect(),
        }
    }

    pub fn ok(&self) -> bool {
        self.problems() == 0
    }

    /// The device-level error, if any, plus every failed unmount and every link that is a problem.
    pub fn problems(&self) -> usize {
        let changes = self.mount_changes.iter().filter(|change| change.is_problem()).count();
        usize::from(self.error.is_some())
            + changes
            + self.links().filter(|link| link.is_problem()).count()
    }

    /// The devmount, then the mount links.
    pub fn links(&self) -> impl Iterator<Item = &LinkReport> {
        std::iter::once(&self.devmount).chain(&self.mounts)
    }
}

impl Report {
    pub fn ok(&self) -> bool {
        self.problems() == 0
    }

    pub fn problems(&self) -> usize {
        self.devices.iter().map(DeviceReport::problems).sum()
    }

    /// Human-readable text, ending in a line that says plainly whether it all worked.
    pub fn render(&self, colors: Colors) -> String {
        let style = Style { colors };
        let mut out = String::new();
        if self.dry_run {
            let _ = writeln!(out, "{}", style.note("dry run: nothing is changed"));
        }
        for device in &self.devices {
            render_device(&mut out, &style, device, self.dry_run);
        }
        let _ = writeln!(out, "{}", self.verdict(&style));
        out
    }

    fn verdict(&self, s: &Style) -> String {
        match self.problems() {
            0 if self.dry_run => s.good("✓ no problems — and nothing was changed (dry run)"),
            0 => s.good(match self.command {
                Command::Apply | Command::Status => "✓ everything is in place",
                Command::Remove => "✓ every link is down",
            }),
            1 => s.bad("✗ 1 problem — see the ✗ line above"),
            n => s.bad(&format!("✗ {n} problems — see the ✗ lines above")),
        }
    }
}

/// Whether [`Report::render`] colours its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Colors {
    /// ANSI colours, for a terminal.
    Ansi,
    /// Plain text.
    Plain,
}

struct Style {
    colors: Colors,
}

impl Style {
    fn paint(&self, code: &str, text: &str) -> String {
        match self.colors {
            Colors::Ansi => format!("\x1b[{code}m{text}\x1b[0m"),
            Colors::Plain => text.to_owned(),
        }
    }
    fn good(&self, text: &str) -> String {
        self.paint("32", text)
    }
    fn bad(&self, text: &str) -> String {
        self.paint("31", text)
    }
    fn note(&self, text: &str) -> String {
        self.paint("33", text)
    }
    fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }
}

fn render_device(out: &mut String, s: &Style, device: &DeviceReport, dry_run: bool) {
    let found = device.device.as_deref().map(|d| format!(" → {d}")).unwrap_or_default();
    let _ = writeln!(out, "{}{found}", s.paint("1", &device.id.to_string()));
    for container in &device.containers {
        let _ = writeln!(out, "  {}", s.dim(&format!("inside {container}")));
    }
    if device.not_connected && device.error.is_none() {
        let handled = if dry_run {
            "would be taken down all the same"
        } else {
            "are taken down all the same"
        };
        let _ = writeln!(out, "  {}", s.note(&format!("• not connected — its links {handled}")));
    }

    let mut left = Vec::new();
    for change in &device.mount_changes {
        let line = match change {
            MountChange::Mounted { top_level: false } => s.good("mounted it"),
            MountChange::Mounted { top_level: true } => s.good("mounted its top level"),
            MountChange::WouldMount { top_level: false } => s.note("would mount it"),
            MountChange::WouldMount { top_level: true } => s.note("would mount its top level"),
            MountChange::Unmounted { mount_point } => {
                s.good(&format!("unmounted {}", mount_point.display()))
            }
            MountChange::WouldUnmount { mount_point } => {
                s.note(&format!("would unmount {}", mount_point.display()))
            }
            MountChange::UnmountFailed { error, .. } => format!("{} {error}", s.bad("✗")),
            MountChange::LeftMounted { mount_point } => {
                left.push(mount_point.display().to_string());
                continue;
            }
        };
        let _ = writeln!(out, "  {line}");
    }
    if !left.is_empty() {
        let _ = writeln!(
            out,
            "  {}",
            s.dim(&format!(
                "left mounted (not mounted by devmounts or an automounter): {}",
                left.join(", ")
            ))
        );
    }
    if !device.mounted_at.is_empty() {
        let mounts: Vec<String> = device.mounted_at.iter().map(describe_mount).collect();
        let _ = writeln!(out, "  {}", s.dim(&format!("mounted at {}", mounts.join(", "))));
    }
    if let Some(error) = &device.error {
        let _ = writeln!(out, "  {} {error}", s.bad("✗"));
    }

    render_link(out, s, &device.devmount, "  ");
    for link in &device.mounts {
        render_link(out, s, link, "    ");
    }
}

fn describe_mount(m: &MountSummary) -> String {
    let mut notes = Vec::new();
    if m.root != Path::new("/") {
        notes.push(m.root.display().to_string());
    }
    if m.read_only {
        notes.push("read-only".into());
    }
    if notes.is_empty() {
        m.mount_point.display().to_string()
    } else {
        format!("{} ({})", m.mount_point.display(), notes.join(", "))
    }
}

fn render_link(out: &mut String, s: &Style, link: &LinkReport, indent: &str) {
    let arrow = match link.target.as_ref().or(link.current.as_ref()) {
        Some(path) => format!(" → {}", path.display()),
        None => String::new(),
    };
    let was =
        || link.current.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "?".into());
    let (mark, what) = match &link.outcome {
        LinkOutcome::Unchanged => (s.good("✓"), s.dim("ok")),
        LinkOutcome::Created => (s.good("✓"), s.good("created")),
        LinkOutcome::Updated => (s.good("✓"), s.good(&format!("updated (was {})", was()))),
        LinkOutcome::Removed => (s.good("✓"), s.good("removed")),
        LinkOutcome::WouldCreate if link.target.is_none() => {
            (s.note("•"), s.note("would create once mounted"))
        }
        LinkOutcome::WouldCreate => (s.note("•"), s.note("would create")),
        LinkOutcome::WouldUpdate => (s.note("•"), s.note(&format!("would update (was {})", was()))),
        LinkOutcome::WouldRemove => (s.note("•"), s.note("would remove")),
        LinkOutcome::Missing if link.ok => (s.good("✓"), s.dim("not there")),
        LinkOutcome::Missing => (s.bad("✗"), s.bad("missing")),
        LinkOutcome::Outdated => (s.bad("✗"), s.bad(&format!("points to {}", was()))),
        LinkOutcome::Skipped => (s.dim("-"), s.dim("skipped")),
        LinkOutcome::Failed { error } => (s.bad("✗"), s.bad(&error.to_string())),
    };
    let _ = writeln!(out, "{indent}{mark} {}{arrow}  {what}", link.link.display());
}
