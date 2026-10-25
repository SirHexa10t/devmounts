//! The command line, as a library: all the `devmounts` binary is, and what a program embedding
//! devmounts reuses rather than restating the flags.
//!
//! Each subcommand's arguments are a struct of their own ([`RunArgs`], [`RemoveArgs`],
//! [`DevicesArgs`]), with the flags they share flattened into each rather than declared global: a
//! program that lifts one subcommand into a command of its own gets every option it takes.
//! [`Command::run`] does what a subcommand says and returns the exit status as a plain number.

use std::ffi::OsStr;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};

use crate::{Colors, Config, Escalation, LinuxSystem, Options, Report, System};

/// Give each device a root link (`/<devmount>`) that follows it wherever the OS mounts it,
/// plus links into it (`/<name> -> /<devmount>/<path>`) that go through that root link.
///
/// `devmounts <CONFIG>` is short for `devmounts apply <CONFIG>`.
#[derive(Parser, Debug)]
#[command(
    name = "devmounts",
    version,
    arg_required_else_help = true,
    override_usage = "devmounts [OPTIONS] <CONFIG>\n       devmounts <COMMAND> [OPTIONS] <CONFIG>\n       devmounts devices [--json]"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    /// TOML file describing the devices and links.
    pub config: Option<PathBuf>,
    #[command(flatten)]
    pub flags: RunFlags,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Mount devices as needed and create or update the devmounts and mounts.
    Apply(RunArgs),
    /// Show devices, mounts and links without changing anything.
    Status(RunArgs),
    /// Remove the links (only ones that are dangling or lead into their device).
    Remove(RemoveArgs),
    /// List block devices with their identifiers and mount points.
    Devices(DevicesArgs),
}

/// What `apply` and `status` take.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct RunArgs {
    /// TOML file describing the devices and links.
    pub config: PathBuf,
    #[command(flatten)]
    pub flags: RunFlags,
}

/// The options `apply`, `status` and `remove` share.
#[derive(Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct RunFlags {
    /// Show what would be done without doing it.
    #[arg(short = 'n', long)]
    pub dry_run: bool,
    /// Don't mount devices that aren't mounted (overrides `automount`).
    #[arg(long)]
    pub no_mount: bool,
    /// Replace symlinks that point somewhere else (never anything that isn't a symlink).
    #[arg(long)]
    pub force: bool,
    /// Create the links in DIR instead of the configured root.
    #[arg(long, value_name = "DIR", value_parser = plain_absolute)]
    pub root: Option<PathBuf>,
    /// How to get privileges: auto, none, or a command such as "sudo" or "doas -n".
    #[arg(long, value_name = "HOW", value_parser = Escalation::parse)]
    pub escalate: Option<Escalation>,
    /// Print the report as JSON.
    #[arg(long)]
    pub json: bool,
}

/// What `remove` takes.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct RemoveArgs {
    #[command(flatten)]
    pub run: RunArgs,
    /// Also unmount the devices (only what an automounter or devmounts itself mounted).
    #[arg(long)]
    pub unmount: bool,
}

/// What `devices` takes.
#[derive(Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct DevicesArgs {
    /// Print the listing as JSON.
    #[arg(long)]
    pub json: bool,
}

/// What a run's exit status says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Everything is in place (for `remove`: every link is down).
    Clean,
    /// The run went through, with problems; the report says which.
    Problems,
    /// It couldn't run at all — a bad config or usage, or a machine that couldn't be read — so
    /// there's no report.
    CouldNotRun,
}

impl Exit {
    pub fn code(self) -> u8 {
        match self {
            Exit::Clean => 0,
            Exit::Problems => 1,
            Exit::CouldNotRun => 2,
        }
    }
}

impl Cli {
    /// Does what the command line says; returns the exit status.
    pub fn run(self) -> u8 {
        let command = match (self.command, self.config) {
            (Some(command), None) if self.flags == RunFlags::default() => command,
            (Some(_), _) => {
                eprintln!("devmounts: options and the CONFIG go after the subcommand, as in `devmounts status --json <CONFIG>`");
                return Exit::CouldNotRun.code();
            }
            (None, Some(config)) => Command::Apply(RunArgs { config, flags: self.flags }),
            (None, None) => {
                eprintln!("devmounts: which CONFIG? `devmounts <CONFIG>` applies one (see `devmounts --help`)");
                return Exit::CouldNotRun.code();
            }
        };
        command.run()
    }
}

impl Command {
    /// Does what the subcommand says, on this machine; returns the exit status.
    pub fn run(self) -> u8 {
        let system = LinuxSystem::from_env();
        let exit = match self {
            Command::Apply(args) => report(crate::apply, args, false, &system),
            Command::Status(args) => report(crate::status, args, false, &system),
            Command::Remove(RemoveArgs { run, unmount }) => report(crate::remove, run, unmount, &system),
            Command::Devices(DevicesArgs { json }) => list_devices(&system, json),
        };
        exit.code()
    }
}

type Operation = fn(&Config, &Options, &dyn System) -> crate::Result<Report>;

fn report(operation: Operation, args: RunArgs, unmount: bool, system: &LinuxSystem) -> Exit {
    let config = match Config::from_path(&args.config) {
        Ok(config) => config,
        Err(err) => return fatal(&err),
    };
    let RunFlags { dry_run, no_mount, force, root, escalate, json } = args.flags;
    let options = Options { dry_run, force, no_mount, unmount, root, escalation: escalate, helper: None };
    match operation(&config, &options, system) {
        Ok(report) => print_report(&report, json),
        Err(err) => fatal(&err),
    }
}

fn print_report(report: &Report, json: bool) -> Exit {
    if json {
        println!("{}", serde_json::to_string_pretty(report).expect("reports serialize"));
    } else {
        print!("{}", report.render(colors()));
    }
    if report.ok() { Exit::Clean } else { Exit::Problems }
}

fn list_devices(system: &LinuxSystem, json: bool) -> Exit {
    let listing = match crate::list_devices(system) {
        Ok(listing) => listing,
        Err(err) => return fatal(&err),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&listing).expect("listings serialize"));
        return Exit::Clean;
    }
    let dash = || "-".to_owned();
    let mut rows = vec![["DEVICE", "TYPE", "LABEL", "UUID", "PARTUUID", "PARTLABEL", "MOUNTED AT"].map(String::from)];
    for entry in &listing {
        let d = &entry.device;
        let mounts: Vec<String> = entry
            .mounts
            .iter()
            .map(|m| {
                if m.root == Path::new("/") {
                    m.mount_point.display().to_string()
                } else {
                    format!("{} ({})", m.mount_point.display(), m.root.display())
                }
            })
            .collect();
        rows.push([
            d.devnode.display().to_string(),
            d.fstype.clone().unwrap_or_else(dash),
            d.label.clone().unwrap_or_else(dash),
            d.uuid.clone().unwrap_or_else(dash),
            d.partuuid.clone().unwrap_or_else(dash),
            d.partlabel.clone().unwrap_or_else(dash),
            if mounts.is_empty() { dash() } else { mounts.join(", ") },
        ]);
    }
    let widths: Vec<usize> = (0..7).map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0)).collect();
    for row in rows {
        let line: Vec<String> = row.iter().zip(&widths).map(|(cell, w)| format!("{cell:<w$}")).collect();
        println!("{}", line.join("  ").trim_end());
    }
    Exit::Clean
}

fn fatal(err: &crate::Error) -> Exit {
    eprintln!("devmounts: {err}");
    Exit::CouldNotRun
}

fn plain_absolute(text: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(text);
    if crate::config::is_plain_absolute(&path) { Ok(path) } else { Err("must be an absolute path without '..'".into()) }
}

/// What `NO_COLOR` asks for (<https://no-color.org>): a value turns colour off, an empty one doesn't.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoColor {
    Unset,
    Empty,
    Set,
}

impl NoColor {
    fn of(value: Option<&OsStr>) -> Self {
        match value {
            None => NoColor::Unset,
            Some(value) if value.is_empty() => NoColor::Empty,
            Some(_) => NoColor::Set,
        }
    }
}

fn colors() -> Colors {
    if !std::io::stdout().is_terminal() {
        return Colors::Plain;
    }
    match NoColor::of(std::env::var_os("NO_COLOR").as_deref()) {
        NoColor::Unset | NoColor::Empty => Colors::Ansi,
        NoColor::Set => Colors::Plain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(argv: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("devmounts").chain(argv.iter().copied()))
    }

    #[test]
    fn the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn each_subcommand_takes_its_own_config_and_options() {
        let Some(Command::Remove(RemoveArgs { run, unmount: true })) = parse(&["remove", "--unmount", "--json", "c.toml"]).unwrap().command
        else {
            panic!("a remove");
        };
        assert_eq!(run, RunArgs { config: "c.toml".into(), flags: RunFlags { json: true, ..RunFlags::default() } });
        let missing = parse(&["status"]).unwrap_err();
        assert!(missing.to_string().contains("<CONFIG>"), "{missing}");
        let bare = parse(&["c.toml", "-n"]).unwrap();
        assert!(bare.command.is_none() && bare.config.as_deref() == Some(Path::new("c.toml")) && bare.flags.dry_run);
    }

    #[test]
    fn bad_values_are_refused_with_the_reason() {
        for (argv, reason) in [
            (&["c.toml", "--root", "links"][..], "absolute path without '..'"),
            (&["c.toml", "--root", "/tmp/.."], "absolute path without '..'"),
            (&["c.toml", "--escalate", " "], "is empty"),
        ] {
            let err = parse(argv).unwrap_err().to_string();
            assert!(err.contains(reason), "{argv:?}: {err}");
        }
    }

    #[test]
    fn options_before_the_subcommand_are_refused_not_ignored() {
        for argv in [&["--json", "status", "c.toml"][..], &["c.toml", "status", "d.toml"]] {
            assert_eq!(parse(argv).unwrap().run(), Exit::CouldNotRun.code(), "{argv:?}");
        }
    }

    #[test]
    fn no_color_set_turns_colour_off_and_empty_does_not() {
        assert_eq!(NoColor::of(None), NoColor::Unset);
        assert_eq!(NoColor::of(Some(OsStr::new(""))), NoColor::Empty);
        assert_eq!(NoColor::of(Some(OsStr::new("1"))), NoColor::Set);
    }

    #[test]
    fn exit_codes() {
        assert_eq!([Exit::Clean, Exit::Problems, Exit::CouldNotRun].map(Exit::code), [0, 1, 2]);
    }
}
