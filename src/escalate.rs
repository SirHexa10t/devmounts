//! Running external commands, and how privileges are gained (`sudo`, `doas`, `run0`).
//!
//! Privileged work is always tried directly first by the callers; only a permission error (or a
//! step that always needs root, like `mount`) goes to the run's privileged helper
//! ([`crate::privileged`]), which is started with this module's [`Escalation`]. Running as root, or
//! into a writable root, needs no helper at all.

use std::env;
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::Serialize;

use crate::error::{Error, Result};

/// How to gain the privileges needed for links in `/` and for `mount`/`umount`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Escalation {
    /// Nothing if already root; otherwise the first of `sudo`, `doas`, `run0` found in `PATH`.
    #[default]
    Auto,
    /// Never escalate; steps that need privileges fail.
    Never,
    /// Start the privileged helper with this command line, e.g. `doas -n`. There's always a
    /// program: an empty command can't be spelled.
    Command { program: String, args: Vec<String> },
}

const AUTO_CANDIDATES: [&str; 3] = ["sudo", "doas", "run0"];

impl Escalation {
    /// Parses the CLI/config spelling: `auto`, `none`/`never`, or a command line split at
    /// whitespace.
    pub fn parse(text: &str) -> std::result::Result<Self, String> {
        match text.trim() {
            "auto" => Ok(Escalation::Auto),
            "none" | "never" => Ok(Escalation::Never),
            other => Escalation::command(other.split_whitespace().map(str::to_owned)),
        }
    }

    /// A command line, given as its words; refused when it has no program.
    pub fn command(words: impl IntoIterator<Item = String>) -> std::result::Result<Self, String> {
        let mut words = words.into_iter();
        match words.next() {
            Some(program) if !program.trim().is_empty() => {
                Ok(Escalation::Command { program, args: words.collect() })
            }
            _ => Err("is empty (use \"none\" to never escalate)".into()),
        }
    }

    /// The command line the privileged helper is started with (`None` = no helper: privileged
    /// steps run directly, as root, or fail).
    pub fn prefix(&self) -> Result<Option<Vec<String>>> {
        match self {
            Escalation::Never => Ok(None),
            Escalation::Command { program, args } => {
                Ok(Some(std::iter::once(program.clone()).chain(args.iter().cloned()).collect()))
            }
            Escalation::Auto if is_root() => Ok(None),
            Escalation::Auto => AUTO_CANDIDATES
                .iter()
                .find(|name| find_program(name).is_some())
                .map(|name| Some(vec![(*name).to_owned()]))
                .ok_or_else(|| Error::Escalation {
                    message:
                        "none of sudo, doas or run0 is installed; run as root or set `escalate`"
                            .into(),
                }),
        }
    }
}

/// Runs a command, inheriting stdin (password prompts) and capturing stderr for the error.
pub fn run(argv: &[OsString]) -> Result<()> {
    let output = output(argv)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::CommandFailed {
            command: display_command(argv),
            message: failure_message(&output),
        })
    }
}

/// Runs a command that asks the user something itself (a passphrase): stdin is the terminal's,
/// and everything it prints goes to stderr — where a prompt belongs, and never into stdout, which
/// may be carrying a report. None of it is captured, so a failure says only how the command
/// ended: its own words were shown as it ran.
pub fn run_on_terminal(argv: &[OsString]) -> Result<()> {
    let (program, args) = split_program(argv)?;
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(io::stderr())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|err| Error::CommandFailed {
            command: display_command(argv),
            message: err.to_string(),
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::CommandFailed { command: display_command(argv), message: status.to_string() })
    }
}

/// Runs a command and returns its output regardless of the exit status.
pub fn output(argv: &[OsString]) -> Result<std::process::Output> {
    let (program, args) = split_program(argv)?;
    Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| Error::CommandFailed {
            command: display_command(argv),
            message: err.to_string(),
        })
}

/// A command line's program and its arguments, or the error for a command line with neither.
fn split_program(argv: &[OsString]) -> Result<(&OsString, &[OsString])> {
    argv.split_first().ok_or_else(|| Error::CommandFailed {
        command: String::new(),
        message: "empty command".into(),
    })
}

pub(crate) fn failure_message(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() { output.status.to_string() } else { stderr.to_owned() }
}

/// A shell-like rendering of a command line, for messages only.
pub fn display_command(argv: &[OsString]) -> String {
    argv.iter()
        .map(|arg| {
            let arg = arg.to_string_lossy();
            if !arg.is_empty()
                && arg.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,@+".contains(c))
            {
                arg.into_owned()
            } else {
                format!("'{}'", arg.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and can't fail.
    unsafe { libc::geteuid() == 0 }
}

/// Looks a program up in `PATH`.
pub fn find_program(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path).map(|dir| dir.join(name)).find(|candidate| is_executable(candidate))
}

fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(words: &[&str]) -> Escalation {
        Escalation::command(words.iter().map(|w| w.to_string())).unwrap()
    }

    #[test]
    fn parses_spellings() {
        assert_eq!(Escalation::parse("auto"), Ok(Escalation::Auto));
        assert_eq!(Escalation::parse("none"), Ok(Escalation::Never));
        assert_eq!(Escalation::parse(" never "), Ok(Escalation::Never));
        assert_eq!(Escalation::parse("sudo -n"), Ok(command(&["sudo", "-n"])));
        for empty in ["", "   "] {
            assert!(Escalation::parse(empty).is_err(), "{empty:?} names no program");
        }
        assert!(Escalation::command([]).is_err());
        assert!(Escalation::command([" ".to_owned(), "-n".to_owned()]).is_err());
    }

    #[test]
    fn explicit_prefixes() {
        assert_eq!(Escalation::Never.prefix().unwrap(), None);
        assert_eq!(command(&["doas"]).prefix().unwrap(), Some(vec!["doas".to_owned()]));
        assert_eq!(
            command(&["doas", "-n"]).prefix().unwrap(),
            Some(vec!["doas".to_owned(), "-n".to_owned()])
        );
    }

    #[test]
    fn reports_stderr_of_failed_commands() {
        let argv: Vec<OsString> =
            ["sh", "-c", "echo boom >&2; exit 3"].iter().map(OsString::from).collect();
        match run(&argv) {
            Err(Error::CommandFailed { message, .. }) => assert_eq!(message, "boom"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn missing_programs_are_errors() {
        let argv = vec![OsString::from("devmounts-no-such-program")];
        assert!(matches!(run(&argv), Err(Error::CommandFailed { .. })));
    }
}
