//! The TOML configuration: which devices to look for, the root link of each (`devmount`),
//! and the links that lead into it (`mounts`).
//!
//! ```toml
//! [[device]]
//! uuid = "11111111-2222-4333-8444-555555555555"   # the data drive
//! devmount = "x"                  # /x -> wherever the device is mounted
//! mounts = {                      # /<name> -> /x/<path>
//!     music = "media/music",
//!     photos = "pictures/photos",
//! }
//! ```

use std::collections::HashSet;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::escalate::Escalation;

/// Names refused as link names when links are created directly in `/`: the system's directories,
/// and the links some systems keep there themselves (Debian's `vmlinuz` and `initrd.img`).
pub const RESERVED_ROOT_NAMES: &[&str] = &[
    "afs",
    "bin",
    "boot",
    "cdrom",
    "dev",
    "efi",
    "etc",
    "gnu",
    "home",
    "initrd.img",
    "initrd.img.old",
    "lib",
    "lib32",
    "lib64",
    "libx32",
    "lost+found",
    "media",
    "mnt",
    "nix",
    "opt",
    "ostree",
    "proc",
    "root",
    "run",
    "sbin",
    "selinux",
    "snap",
    "srv",
    "swap",
    "swapfile",
    "sys",
    "sysroot",
    "tmp",
    "usr",
    "var",
    "vmlinuz",
    "vmlinuz.old",
];

/// Longest file name Linux accepts (`NAME_MAX`).
const NAME_MAX: usize = 255;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub settings: Settings,
    pub devices: Vec<DeviceSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Settings {
    /// Directory the links are created in.
    pub root: PathBuf,
    /// Whether devices that aren't mounted get mounted.
    pub automount: bool,
    pub mount_options: Vec<String>,
    pub mount_method: MountMethod,
    /// Where the `mount` method creates mount points (`<mount_base>/<uuid>`).
    pub mount_base: PathBuf,
    pub escalation: Escalation,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            root: PathBuf::from("/"),
            automount: true,
            mount_options: Vec::new(),
            mount_method: MountMethod::Auto,
            mount_base: PathBuf::from("/run/devmounts"),
            escalation: Escalation::Auto,
        }
    }
}

/// How devices get mounted when they aren't already.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MountMethod {
    /// udisks2 when it's available and we aren't root, otherwise `mount`.
    #[default]
    Auto,
    /// Only udisks2 (mounts where the desktop would, as the calling user).
    Udisks,
    /// Only `mount` into `mount_base` (needs root or escalation).
    Mount,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSpec {
    pub id: DeviceId,
    /// Name of the device's root link: `<root>/<devmount>` points at the device itself.
    pub devmount: String,
    /// Links into the device, all going through the devmount:
    /// `<root>/<name>` points at `<root>/<devmount>/<path>`.
    pub mounts: Vec<LinkSpec>,
    /// Per-device override of [`Settings::automount`].
    pub automount: Option<bool>,
    /// Per-device override of [`Settings::mount_options`].
    pub mount_options: Option<Vec<String>>,
}

impl DeviceSpec {
    pub fn automount(&self, settings: &Settings) -> bool {
        self.automount.unwrap_or(settings.automount)
    }

    pub fn mount_options<'a>(&'a self, settings: &'a Settings) -> &'a [String] {
        self.mount_options.as_deref().unwrap_or(&settings.mount_options)
    }

    /// Where the link to `path` points: through the devmount, never at the real location,
    /// so swapping the device only means re-pointing the devmount.
    pub fn link_target(&self, root: &Path, path: &DevicePath) -> PathBuf {
        let devmount = root.join(&self.devmount);
        if path.is_root() { devmount } else { devmount.join(path.relative()) }
    }
}

/// How a device is recognized. UUIDs compare case-insensitively, labels exactly.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "by", content = "value", rename_all = "lowercase")]
pub enum DeviceId {
    /// Filesystem (or container) UUID, as in `/dev/disk/by-uuid`.
    Uuid(String),
    /// Filesystem label.
    Label(String),
    /// Partition UUID (GPT, or `xxxxxxxx-NN` on MBR).
    PartUuid(String),
    /// GPT partition name.
    PartLabel(String),
}

impl DeviceId {
    /// The key used to spot the same device listed twice.
    fn normalized(&self) -> DeviceId {
        match self {
            DeviceId::Uuid(u) => DeviceId::Uuid(u.to_ascii_lowercase()),
            DeviceId::PartUuid(u) => DeviceId::PartUuid(u.to_ascii_lowercase()),
            other => other.clone(),
        }
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceId::Uuid(v) => write!(f, "UUID={v}"),
            DeviceId::Label(v) => write!(f, "LABEL={v:?}"),
            DeviceId::PartUuid(v) => write!(f, "PARTUUID={v}"),
            DeviceId::PartLabel(v) => write!(f, "PARTLABEL={v:?}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LinkSpec {
    /// File name of the link inside the root directory.
    pub name: String,
    pub path: DevicePath,
}

/// A path inside a device, relative to its filesystem root (`.`). For btrfs that's the
/// top-level subvolume, so subvolumes appear as directories (`@home/me`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DevicePath(PathBuf);

impl DevicePath {
    /// Validates and normalizes: `//`, `.` and leading/trailing `/` are dropped; `..`, NUL
    /// bytes and the empty string are refused. `.` is the device root.
    pub fn parse(text: &str) -> Result<DevicePath, String> {
        if text.is_empty() {
            return Err("is empty (use \".\" for the device root)".into());
        }
        if text.contains('\0') {
            return Err("contains a NUL byte".into());
        }
        let mut rel = PathBuf::new();
        for part in text.split('/') {
            match part {
                "" | "." => {}
                ".." => return Err("must not contain '..'".into()),
                _ => rel.push(part),
            }
        }
        Ok(DevicePath(rel))
    }

    pub fn root() -> DevicePath {
        DevicePath(PathBuf::new())
    }

    /// The path relative to the filesystem root (empty for the root itself).
    pub fn relative(&self) -> &Path {
        &self.0
    }

    /// The path as an absolute path inside the filesystem, e.g. `/@home/me`.
    pub fn in_device(&self) -> PathBuf {
        Path::new("/").join(&self.0)
    }

    pub fn is_root(&self) -> bool {
        self.0.as_os_str().is_empty()
    }
}

/// Shown the way it's written in the config: `media/music`, or `.` for the root.
impl fmt::Display for DevicePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_root() { f.write_str(".") } else { write!(f, "{}", self.0.display()) }
    }
}

impl Serialize for DevicePath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Whether `path` is absolute and free of `..`: one that names the directory it seems to — a root
/// of `/tmp/..` is `/`, and would slip past the checks made for `/`.
pub fn is_plain_absolute(path: &Path) -> bool {
    path.is_absolute() && !path.components().any(|c| c == Component::ParentDir)
}

/// Checks a link name; `in_slash` adds the reserved-name check for links created in `/`.
pub fn validate_link_name(name: &str, in_slash: bool) -> Result<(), String> {
    if name.is_empty() {
        return Err("name is empty".into());
    }
    if name.len() > NAME_MAX {
        return Err(format!("name is longer than {NAME_MAX} bytes"));
    }
    if name == "." || name == ".." {
        return Err("name can't be '.' or '..'".into());
    }
    if let Some(bad) = name.chars().find(|c| !(c.is_ascii_alphanumeric() || "._-".contains(*c))) {
        return Err(format!("name contains {bad:?}; use only letters, digits, '_', '-' and '.'"));
    }
    if !name.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
        return Err("name must start with a letter, digit or '_'".into());
    }
    if in_slash && RESERVED_ROOT_NAMES.contains(&name) {
        return Err(format!("'/{name}' is reserved for the system"));
    }
    Ok(())
}

impl Config {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Config> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|err| Error::ConfigRead { path: path.to_owned(), message: err.to_string() })?;
        text.parse()
    }
}

impl FromStr for Config {
    type Err = Error;

    fn from_str(text: &str) -> Result<Config> {
        let raw: RawConfig =
            toml::from_str(text).map_err(|err| Error::ConfigParse { message: err.to_string() })?;
        raw.validate()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    settings: RawSettings,
    #[serde(default)]
    device: Vec<RawDevice>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSettings {
    root: Option<PathBuf>,
    automount: Option<bool>,
    mount_options: Option<Vec<String>>,
    mount_method: Option<MountMethod>,
    mount_base: Option<PathBuf>,
    escalate: Option<RawEscalate>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawEscalate {
    Text(String),
    Command(Vec<String>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDevice {
    uuid: Option<String>,
    label: Option<String>,
    partuuid: Option<String>,
    partlabel: Option<String>,
    devmount: Option<String>,
    #[serde(default)]
    mounts: IndexMap<String, String>,
    automount: Option<bool>,
    mount_options: Option<Vec<String>>,
}

impl RawConfig {
    fn validate(self) -> Result<Config> {
        let mut problems = Vec::new();
        let settings = self.settings.validate(&mut problems);
        let in_slash = settings.root == Path::new("/");

        if self.device.is_empty() {
            problems.push("no [[device]] entries".into());
        }

        let mut seen_ids = HashSet::new();
        let mut seen_names = HashSet::new();
        let mut devices = Vec::new();
        for (index, raw) in self.device.into_iter().enumerate() {
            let at = format!("device #{}", index + 1);
            let Some(device) = raw.validate(&at, in_slash, &mut problems) else { continue };
            if !seen_ids.insert(device.id.normalized()) {
                problems.push(format!("{at}: {} is listed more than once", device.id));
            }
            let names =
                std::iter::once(&device.devmount).chain(device.mounts.iter().map(|m| &m.name));
            for name in names {
                if !seen_names.insert(name.clone()) {
                    problems.push(format!("{at}: the name '{name}' is used more than once"));
                }
            }
            devices.push(device);
        }

        if problems.is_empty() {
            Ok(Config { settings, devices })
        } else {
            Err(Error::ConfigInvalid { problems })
        }
    }
}

impl RawSettings {
    fn validate(self, problems: &mut Vec<String>) -> Settings {
        let defaults = Settings::default();
        let root = self.root.unwrap_or(defaults.root);
        if !is_plain_absolute(&root) {
            problems.push(format!(
                "settings.root must be an absolute path without '..' (got {})",
                root.display()
            ));
        }
        let mount_base = self.mount_base.unwrap_or(defaults.mount_base);
        if !is_plain_absolute(&mount_base) {
            problems.push(format!(
                "settings.mount_base must be an absolute path without '..' (got {})",
                mount_base.display()
            ));
        } else if mount_base == Path::new("/") {
            problems.push("settings.mount_base can't be / — what's right below it counts as devmounts' mount points".into());
        }
        let mount_options = self.mount_options.unwrap_or_default();
        check_mount_options("settings.mount_options", &mount_options, problems);
        let escalation = match self.escalate {
            None => Ok(defaults.escalation),
            Some(RawEscalate::Text(text)) => Escalation::parse(&text),
            Some(RawEscalate::Command(words)) => Escalation::command(words),
        };
        let escalation = escalation.unwrap_or_else(|why| {
            problems.push(format!("settings.escalate {why}"));
            Escalation::Never
        });
        Settings {
            root,
            automount: self.automount.unwrap_or(defaults.automount),
            mount_options,
            mount_method: self.mount_method.unwrap_or_default(),
            mount_base,
            escalation,
        }
    }
}

impl RawDevice {
    fn validate(self, at: &str, in_slash: bool, problems: &mut Vec<String>) -> Option<DeviceSpec> {
        let ids: Vec<DeviceId> = [
            self.uuid.map(DeviceId::Uuid),
            self.label.map(DeviceId::Label),
            self.partuuid.map(DeviceId::PartUuid),
            self.partlabel.map(DeviceId::PartLabel),
        ]
        .into_iter()
        .flatten()
        .collect();

        let id = match ids.as_slice() {
            [] => {
                problems.push(format!("{at}: needs one of uuid, label, partuuid or partlabel"));
                return None;
            }
            [id] => id.clone(),
            _ => {
                problems.push(format!("{at}: give only one of uuid, label, partuuid or partlabel"));
                return None;
            }
        };
        let at = format!("{at} ({id})");
        check_id(&at, &id, problems);

        if let Some(options) = &self.mount_options {
            check_mount_options(&format!("{at}: mount_options"), options, problems);
        }

        let devmount = match self.devmount {
            None => {
                problems
                    .push(format!("{at}: needs a devmount (the name of the device's root link)"));
                String::new()
            }
            Some(name) => {
                if let Err(why) = validate_link_name(&name, in_slash) {
                    problems.push(format!("{at}: devmount '{name}': {why}"));
                }
                name
            }
        };

        let mut mounts = Vec::new();
        for (name, path) in self.mounts {
            if let Err(why) = validate_link_name(&name, in_slash) {
                problems.push(format!("{at}: mount '{name}': {why}"));
                continue;
            }
            match DevicePath::parse(&path) {
                Ok(path) => mounts.push(LinkSpec { name, path }),
                Err(why) => problems.push(format!("{at}: mount '{name}': path {path:?} {why}")),
            }
        }

        Some(DeviceSpec {
            id,
            devmount,
            mounts,
            automount: self.automount,
            mount_options: self.mount_options,
        })
    }
}

fn check_id(at: &str, id: &DeviceId, problems: &mut Vec<String>) {
    let (value, is_uuid) = match id {
        DeviceId::Uuid(v) | DeviceId::PartUuid(v) => (v, true),
        DeviceId::Label(v) | DeviceId::PartLabel(v) => (v, false),
    };
    if value.is_empty() {
        problems.push(format!("{at}: the identifier is empty"));
    } else if value.contains('\0') {
        problems.push(format!("{at}: the identifier contains a NUL byte"));
    } else if is_uuid && value.chars().any(char::is_whitespace) {
        problems.push(format!("{at}: UUIDs don't contain whitespace"));
    }
}

fn check_mount_options(at: &str, options: &[String], problems: &mut Vec<String>) {
    for option in options {
        if option.trim().is_empty() {
            problems.push(format!("{at}: empty option"));
        } else if option.chars().any(char::is_whitespace) {
            problems.push(format!("{at}: option {option:?} contains whitespace"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config> {
        text.parse()
    }

    fn problems(text: &str) -> Vec<String> {
        match parse(text) {
            Err(Error::ConfigInvalid { problems }) => problems,
            other => panic!("expected ConfigInvalid, got {other:?}"),
        }
    }

    fn assert_problem(text: &str, needle: &str) {
        let problems = problems(text);
        assert!(
            problems.iter().any(|p| p.contains(needle)),
            "no problem containing {needle:?} in {problems:#?}"
        );
    }

    /// A device entry with the given id line and mounts table.
    fn dev(id: &str, mounts: &str) -> String {
        format!("[[device]]\n{id}\ndevmount = 'd_{}'\nmounts = {{ {mounts} }}\n", id.len())
    }

    #[test]
    fn minimal_config_gets_defaults() {
        let config = parse("[[device]]\nlabel = 'X'\ndevmount = 'x'").unwrap();
        assert_eq!(config.settings, Settings::default());
        let device = &config.devices[0];
        assert_eq!(device.id, DeviceId::Label("X".into()));
        assert_eq!(device.devmount, "x");
        assert!(device.mounts.is_empty(), "a device may have only its devmount");
        assert!(device.automount(&config.settings));
        assert!(device.mount_options(&config.settings).is_empty());
    }

    #[test]
    fn the_users_style_parses() {
        // Multi-line inline tables need TOML 1.1, which the parser supports.
        let config = parse(
            r#"
            [[device]]
            uuid = "<uuid>"  # comment for the human reader about the nature of this device
            devmount = "media_dev"  # will translate into a "/media_dev" symlink
            mounts = {
                music = "media/music",  # /music -> /media_dev/media/music
                movies = "media/films",
                photos = "pictures/photos",
            }
            "#,
        )
        .unwrap();
        let device = &config.devices[0];
        let names: Vec<_> = device.mounts.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["music", "movies", "photos"], "file order is kept");
        let root = Path::new("/");
        assert_eq!(
            device.link_target(root, &device.mounts[0].path),
            Path::new("/media_dev/media/music")
        );
        assert_eq!(device.link_target(root, &DevicePath::root()), Path::new("/media_dev"));
        assert_eq!(
            device.link_target(Path::new("/srv/l"), &device.mounts[2].path),
            Path::new("/srv/l/media_dev/pictures/photos")
        );
    }

    #[test]
    fn the_example_config_parses() {
        let config = parse(include_str!("../example.toml")).unwrap();
        assert_eq!(
            config.settings,
            Settings::default(),
            "its settings section spells out the defaults"
        );
        let ids: Vec<String> = config.devices.iter().map(|d| d.id.to_string()).collect();
        assert_eq!(
            ids,
            [
                "UUID=11111111-2222-4333-8444-555555555555",
                "LABEL=\"STICK\"",
                "PARTLABEL=\"storage\""
            ]
        );
        assert!(config.devices[2].mounts.iter().any(|m| m.path.is_root()));
    }

    #[test]
    fn full_config() {
        let config = parse(
            r#"
            [settings]
            root = "/srv/links"
            automount = false
            mount_options = ["noatime"]
            mount_method = "mount"
            mount_base = "/mnt/dm"
            escalate = ["doas", "-n"]

            [[device]]
            uuid = "ABCDEF12-3456-4789-8abc-def012345678"
            devmount = "work"
            automount = true
            mount_options = ["ro"]
            [device.mounts]
            scratch = "work/scratch area"
            logs = "/logfiles/"
            again = "."

            [[device]]
            partlabel = "data part"
            devmount = "data"
            "#,
        )
        .unwrap();
        let s = &config.settings;
        assert_eq!(s.root, Path::new("/srv/links"));
        assert!(!s.automount);
        assert_eq!(s.mount_method, MountMethod::Mount);
        assert_eq!(s.mount_base, Path::new("/mnt/dm"));
        assert_eq!(
            s.escalation,
            Escalation::command(["doas".to_owned(), "-n".to_owned()]).unwrap()
        );

        let first = &config.devices[0];
        assert!(first.automount(s));
        assert_eq!(first.mount_options(s), ["ro"]);
        assert_eq!(first.mounts[1].path.relative(), Path::new("logfiles"));
        assert!(first.mounts[2].path.is_root());
        assert!(!config.devices[1].automount(s));
    }

    #[test]
    fn escalate_spellings() {
        let with = |value: &str| {
            parse(&format!("settings.escalate = {value}\n[[device]]\nlabel='X'\ndevmount='x'"))
        };
        assert_eq!(with("'none'").unwrap().settings.escalation, Escalation::Never);
        assert_eq!(with("'auto'").unwrap().settings.escalation, Escalation::Auto);
        assert_eq!(
            with("'sudo -n'").unwrap().settings.escalation,
            Escalation::command(["sudo".to_owned(), "-n".to_owned()]).unwrap()
        );
        for empty in ["''", "' '", "[]", "['']"] {
            assert!(with(empty).is_err(), "{empty} names no program");
        }
    }

    #[test]
    fn syntax_errors_and_unknown_keys() {
        assert!(matches!(parse("[[device]"), Err(Error::ConfigParse { .. })));
        // Typos are errors rather than silently ignored keys.
        assert!(matches!(parse("[[devices]]\nlabel='X'"), Err(Error::ConfigParse { .. })));
        assert!(matches!(
            parse("[[device]]\nlabel='X'\ndevmount='x'\nmount={a='b'}"),
            Err(Error::ConfigParse { .. })
        ));
        assert!(matches!(parse("[settings]\nrot='/'"), Err(Error::ConfigParse { .. })));
        // A single [device] table instead of an array of them.
        assert!(matches!(
            parse("[device]\nlabel='X'\ndevmount='x'"),
            Err(Error::ConfigParse { .. })
        ));
        // Paths must be strings.
        assert!(matches!(
            parse("[[device]]\nlabel='X'\ndevmount='x'\nmounts={a=1}"),
            Err(Error::ConfigParse { .. })
        ));
        // Multi-line inline tables still need commas.
        assert!(matches!(
            parse("[[device]]\nlabel='X'\ndevmount='x'\nmounts = {\n a = 'b'\n c = 'd'\n}"),
            Err(Error::ConfigParse { .. })
        ));
        // TOML itself rejects a name used twice in one table.
        assert!(matches!(
            parse("[[device]]\nlabel='X'\ndevmount='x'\n[device.mounts]\na='b'\na='c'"),
            Err(Error::ConfigParse { .. })
        ));
    }

    #[test]
    fn no_devices() {
        assert_problem("", "no [[device]] entries");
    }

    #[test]
    fn devmount_is_required_and_checked() {
        assert_problem("[[device]]\nlabel='X'\nmounts={a='b'}", "needs a devmount");
        assert_problem("[[device]]\nlabel='X'\ndevmount='usr'", "reserved");
        assert_problem("[[device]]\nlabel='X'\ndevmount='a/b'", "devmount 'a/b'");
        assert_problem("[[device]]\nlabel='X'\ndevmount=''", "name is empty");
    }

    #[test]
    fn identifiers() {
        assert_problem("[[device]]\ndevmount='x'", "needs one of uuid");
        assert_problem("[[device]]\nuuid='a'\nlabel='b'\ndevmount='x'", "only one of");
        assert_problem(&dev("label=''", ""), "identifier is empty");
        assert_problem(&dev("uuid='ab cd'", ""), "whitespace");
        assert_problem(&dev("label=\"a\\u0000b\"", ""), "NUL");
        // Labels may contain spaces.
        assert!(parse(&dev("label='Example Drive'", "")).is_ok());
    }

    #[test]
    fn same_device_twice() {
        assert_problem(
            "[[device]]\nuuid='ABCD-1234'\ndevmount='a'\n[[device]]\nuuid='abcd-1234'\ndevmount='b'",
            "listed more than once",
        );
        // Different kinds of id may name the same device; that can only be detected later.
        assert!(
            parse("[[device]]\nuuid='abcd'\ndevmount='a'\n[[device]]\nlabel='abcd'\ndevmount='b'")
                .is_ok()
        );
    }

    #[test]
    fn names_are_unique_across_devmounts_and_mounts() {
        assert_problem(
            "[[device]]\nlabel='A'\ndevmount='x'\n[[device]]\nlabel='B'\ndevmount='x'",
            "used more than once",
        );
        assert_problem(
            "[[device]]\nlabel='A'\ndevmount='x'\nmounts={x='sub'}",
            "used more than once",
        );
        assert_problem(
            "[[device]]\nlabel='A'\ndevmount='a'\nmounts={m='1'}\n[[device]]\nlabel='B'\ndevmount='b'\nmounts={m='2'}",
            "used more than once",
        );
    }

    #[test]
    fn link_name_rules() {
        let in_slash = |n: &str| validate_link_name(n, true);
        assert!(in_slash("scratch_area").is_ok());
        assert!(in_slash("my-dev.2").is_ok());
        assert!(in_slash("_x").is_ok());
        assert!(in_slash("").is_err());
        assert!(in_slash(".").is_err());
        assert!(in_slash("..").is_err());
        assert!(in_slash(".hidden").is_err());
        assert!(in_slash("-x").is_err());
        assert!(in_slash("a/b").is_err());
        assert!(in_slash("a b").is_err());
        assert!(in_slash("a\0b").is_err());
        assert!(in_slash("ünï").is_err());
        assert!(in_slash(&"x".repeat(255)).is_ok());
        assert!(in_slash(&"x".repeat(256)).is_err());
        for reserved in [
            "bin",
            "usr",
            "lib64",
            "home",
            "boot",
            "proc",
            "selinux",
            "vmlinuz",
            "vmlinuz.old",
            "initrd.img",
            "initrd.img.old",
        ] {
            assert!(in_slash(reserved).is_err(), "{reserved} should be reserved");
            assert!(validate_link_name(reserved, false).is_ok(), "{reserved} is fine outside /");
        }
    }

    #[test]
    fn reserved_names_only_matter_in_slash() {
        assert_problem(&dev("label='A'", "usr='x'"), "reserved");
        assert!(parse(&format!("settings.root='/srv/l'\n{}", dev("label='A'", "usr='x'"))).is_ok());
    }

    #[test]
    fn device_paths() {
        let rel = |t: &str| DevicePath::parse(t).map(|p| p.relative().to_owned());
        assert_eq!(rel("a/b").unwrap(), Path::new("a/b"));
        assert_eq!(rel("/a//b/./c/").unwrap(), Path::new("a/b/c"));
        assert_eq!(rel("work/scratch area").unwrap(), Path::new("work/scratch area"));
        assert_eq!(rel("ünïcödé/ x ").unwrap(), Path::new("ünïcödé/ x "));
        assert_eq!(rel("projects/[workspace]").unwrap(), Path::new("projects/[workspace]"));
        assert_eq!(rel("@home/me").unwrap(), Path::new("@home/me"));
        assert!(DevicePath::parse(".").unwrap().is_root());
        assert!(DevicePath::parse("./").unwrap().is_root());
        assert!(DevicePath::parse("/").unwrap().is_root());
        assert!(DevicePath::parse("").is_err(), "the root is spelled '.'");
        assert!(DevicePath::parse("a/../b").is_err());
        assert!(DevicePath::parse("..").is_err());
        assert!(DevicePath::parse("a\0b").is_err());
        // `...` is an ordinary name.
        assert_eq!(rel("...").unwrap(), Path::new("..."));
        assert_eq!(DevicePath::parse("a/b").unwrap().in_device(), Path::new("/a/b"));
        assert_eq!(DevicePath::parse("/a/b/").unwrap().to_string(), "a/b");
        assert_eq!(DevicePath::parse(".").unwrap().to_string(), ".");
        assert_problem(&dev("label='A'", "x='../etc'"), "'..'");
        assert_problem(&dev("label='A'", "x=''"), "use \".\" for the device root");
    }

    #[test]
    fn settings_paths_must_be_absolute_and_free_of_dotdot() {
        for bad in ["links", "/tmp/..", "/srv/../etc"] {
            assert_problem(
                &format!("settings.root={bad:?}\n{}", dev("label='A'", "")),
                "settings.root",
            );
            assert_problem(
                &format!("settings.mount_base={bad:?}\n{}", dev("label='A'", "")),
                "settings.mount_base",
            );
        }
        assert!(
            parse(&format!("settings.root='/srv/./links'\n{}", dev("label='A'", ""))).is_ok(),
            "'.' changes nothing"
        );
    }

    #[test]
    fn the_mount_base_cannot_be_the_root_directory() {
        for root in ["/", "//", "/."] {
            assert_problem(
                &format!("settings.mount_base={root:?}\n{}", dev("label='A'", "")),
                "can't be /",
            );
        }
    }

    #[test]
    fn mount_options_are_checked() {
        assert_problem(
            &format!("settings.mount_options=['']\n{}", dev("label='A'", "")),
            "empty option",
        );
        assert_problem(
            "[[device]]\nlabel='A'\ndevmount='a'\nmount_options=['ro', 'a b']",
            "whitespace",
        );
    }

    #[test]
    fn all_problems_are_reported_at_once() {
        let problems = problems(
            "[[device]]\nlabel='A'\ndevmount='a'\nmounts={'a/b'='x', ok='../y'}\n[[device]]\ndevmount='z'",
        );
        assert_eq!(problems.len(), 3, "{problems:#?}");
    }

    #[test]
    fn device_id_display() {
        assert_eq!(DeviceId::Uuid("ab".into()).to_string(), "UUID=ab");
        assert_eq!(DeviceId::Label("My Disk".into()).to_string(), "LABEL=\"My Disk\"");
        assert_eq!(DeviceId::PartUuid("cd".into()).to_string(), "PARTUUID=cd");
        assert_eq!(DeviceId::PartLabel("root".into()).to_string(), "PARTLABEL=\"root\"");
    }
}
