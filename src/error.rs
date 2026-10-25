//! The crate's error type.
//!
//! Errors are `Clone` so they can be stored in reports (one device failing doesn't stop the
//! others), and serialize as `{"kind": ..., "message": ...}` for JSON output.

use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;

use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("can't read config file {}: {message}", path.display())]
    ConfigRead { path: PathBuf, message: String },

    #[error("can't parse config: {message}")]
    ConfigParse { message: String },

    #[error("invalid config:{}", bullet_list(problems))]
    ConfigInvalid { problems: Vec<String> },

    #[error("can't list block devices: {message}")]
    Inventory { message: String },

    #[error("can't read the mount table: {message}")]
    MountTable { message: String },

    #[error("no device with {id} is connected{}", hint.as_deref().map(|h| format!(" ({h})")).unwrap_or_default())]
    DeviceNotFound { id: String, hint: Option<String> },

    #[error("{id} matches more than one device: {}", candidates.join(", "))]
    AmbiguousDevice { id: String, candidates: Vec<String> },

    #[error("{device} is a {fstype} container that isn't open/active ({hint})")]
    InactiveContainer { device: String, fstype: String, hint: String },

    #[error("{device} is used by several devices ({}); identify the filesystem you want instead", holders.join(", "))]
    MultipleHolders { device: String, holders: Vec<String> },

    #[error("{device} has no mountable filesystem{}", fstype.as_deref().map(|t| format!(" (it holds {t})")).unwrap_or_default())]
    NoFilesystem { device: String, fstype: Option<String> },

    #[error("{device} isn't mounted")]
    NotMounted { device: String },

    #[error("'{path}' isn't reachable through any mount of {device}{}", exposed_hint(exposed))]
    PathNotExposed { device: String, path: String, exposed: Vec<String> },

    #[error("{} doesn't exist", target.display())]
    TargetMissing { target: PathBuf },

    #[error("{} isn't a directory", target.display())]
    TargetNotDirectory { target: PathBuf },

    #[error("{} resolves to {}, which is outside the device", target.display(), resolved.display())]
    TargetEscapesDevice { target: PathBuf, resolved: PathBuf },

    #[error("{} is covered by another mount at {}", target.display(), mount_point.display())]
    TargetCovered { target: PathBuf, mount_point: PathBuf },

    #[error("{} already points to {}, which isn't on this device (use --force to replace it)", link.display(), current.display())]
    LinkConflict { link: PathBuf, current: PathBuf },

    #[error("{} exists and is a {kind}, not a symlink; leaving it alone", link.display())]
    LinkBlocked { link: PathBuf, kind: String },

    #[error("mounting {device} failed: {message}")]
    MountFailed { device: String, message: String },

    #[error("unmounting {} failed: {message}", mount_point.display())]
    UnmountFailed { mount_point: PathBuf, message: String },

    #[error("`{command}` failed: {message}")]
    CommandFailed { command: String, message: String },

    #[error("can't get privileges: {message}")]
    Escalation { message: String },

    #[error("{context}: {message}")]
    Io { context: String, message: String },
}

impl Error {
    /// Wraps an I/O error with a description of what was being attempted.
    pub fn io(context: impl Into<String>, err: &io::Error) -> Self {
        Error::Io { context: context.into(), message: err.to_string() }
    }

    /// A stable, machine-readable name for the variant.
    pub fn kind(&self) -> &'static str {
        match self {
            Error::ConfigRead { .. } => "config_read",
            Error::ConfigParse { .. } => "config_parse",
            Error::ConfigInvalid { .. } => "config_invalid",
            Error::Inventory { .. } => "inventory",
            Error::MountTable { .. } => "mount_table",
            Error::DeviceNotFound { .. } => "device_not_found",
            Error::AmbiguousDevice { .. } => "ambiguous_device",
            Error::InactiveContainer { .. } => "inactive_container",
            Error::MultipleHolders { .. } => "multiple_holders",
            Error::NoFilesystem { .. } => "no_filesystem",
            Error::NotMounted { .. } => "not_mounted",
            Error::PathNotExposed { .. } => "path_not_exposed",
            Error::TargetMissing { .. } => "target_missing",
            Error::TargetNotDirectory { .. } => "target_not_directory",
            Error::TargetEscapesDevice { .. } => "target_escapes_device",
            Error::TargetCovered { .. } => "target_covered",
            Error::LinkConflict { .. } => "link_conflict",
            Error::LinkBlocked { .. } => "link_blocked",
            Error::MountFailed { .. } => "mount_failed",
            Error::UnmountFailed { .. } => "unmount_failed",
            Error::CommandFailed { .. } => "command_failed",
            Error::Escalation { .. } => "escalation",
            Error::Io { .. } => "io",
        }
    }
}

impl Serialize for Error {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("Error", 2)?;
        s.serialize_field("kind", self.kind())?;
        s.serialize_field("message", &self.to_string())?;
        s.end()
    }
}

fn bullet_list(items: &[String]) -> String {
    let mut out = String::new();
    for item in items {
        let _ = write!(out, "\n  - {item}");
    }
    out
}

fn exposed_hint(exposed: &[String]) -> String {
    if exposed.is_empty() {
        String::new()
    } else {
        format!(" (mounted parts of the device: {})", exposed.join(", "))
    }
}
