//! The run's privileged helper. Every step that needs root — a link in `/`, `mount`, `umount`, a
//! mount point under `mount_base` — goes through one helper process, started with the escalation
//! command the first time a step needs it and kept for the rest of the run. However many steps a
//! run takes, it asks for a password at most once.
//!
//! The helper is this same program, started again as
//! `<escalation> <program> --devmounts-privileged-helper` ([`helper_argv`]) and claimed by
//! [`try_serve`] before anything else runs. It reads one [`Op`] per line on stdin and answers each
//! with a [`Reply`] line on stdout, doing the work with the very functions a direct change uses —
//! a link is replaced atomically, and only in place of a symlink, either way. A program embedding
//! devmounts calls [`try_serve`] first thing in `main`, or names a program that does in
//! [`Options::helper`](crate::Options::helper).

use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::escalate::{self, Escalation};
use crate::link::{self, Placed};
use crate::mount;

/// `argv[1]` of a program started as devmounts' privileged helper.
pub const HELPER_MARKER: &str = "--devmounts-privileged-helper";

/// One privileged step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Point `link` at `target`, in place of a symlink (or of nothing).
    SetLink {
        #[serde(with = "bytes")]
        link: PathBuf,
        #[serde(with = "bytes")]
        target: PathBuf,
    },
    /// Take down the symlink at `link`.
    RemoveLink {
        #[serde(with = "bytes")]
        link: PathBuf,
    },
    /// Create `dir`, and its parents.
    CreateDir {
        #[serde(with = "bytes")]
        dir: PathBuf,
    },
    /// Remove `dir`, which must be empty.
    RemoveDir {
        #[serde(with = "bytes")]
        dir: PathBuf,
    },
    /// Mount `device` at `dir` ([`mount::mount_argv`]).
    Mount {
        options: Vec<String>,
        #[serde(with = "bytes")]
        device: PathBuf,
        #[serde(with = "bytes")]
        dir: PathBuf,
    },
    /// Unmount `mount_point` ([`mount::umount_argv`]).
    Unmount {
        #[serde(with = "bytes")]
        mount_point: PathBuf,
    },
}

/// How a step went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Done,
    /// Something other than a symlink is at the link's path (a directory, a file, …), left as it
    /// was.
    Blocked {
        kind: String,
    },
    Failed {
        message: String,
    },
}

impl Op {
    /// Does the step, here and now: the helper's side of a request.
    pub fn perform(&self) -> Reply {
        let outcome = match self {
            Op::SetLink { link, target } => link::put_symlink(link, target).map(Reply::from),
            Op::RemoveLink { link } => link::take_symlink(link).map(Reply::from),
            Op::CreateDir { dir } => fs::create_dir_all(dir).map(|()| Reply::Done),
            Op::RemoveDir { dir } => fs::remove_dir(dir).map(|()| Reply::Done),
            Op::Mount { options, device, dir } => {
                return run(&mount::mount_argv(device, dir, options));
            }
            Op::Unmount { mount_point } => return run(&mount::umount_argv(mount_point)),
        };
        outcome.unwrap_or_else(|err| Reply::Failed { message: err.to_string() })
    }
}

fn run(argv: &[OsString]) -> Reply {
    match escalate::run(argv) {
        Ok(()) => Reply::Done,
        Err(Error::CommandFailed { message, .. }) => Reply::Failed { message },
        Err(other) => Reply::Failed { message: other.to_string() },
    }
}

impl From<Placed> for Reply {
    fn from(placed: Placed) -> Self {
        match placed {
            Placed::Done => Reply::Done,
            Placed::Blocked { kind } => Reply::Blocked { kind },
        }
    }
}

impl Reply {
    /// A link step's reply, as what the step came to; a failure becomes an I/O error described
    /// by `context`.
    pub(crate) fn placed(self, context: impl FnOnce() -> String) -> Result<Placed> {
        match self {
            Reply::Done => Ok(Placed::Done),
            Reply::Blocked { kind } => Ok(Placed::Blocked { kind }),
            Reply::Failed { message } => Err(Error::Io { context: context(), message }),
        }
    }

    /// Any other step's reply: done, or the error `failure` makes of what went wrong.
    pub(crate) fn done(self, failure: impl FnOnce(String) -> Error) -> Result<()> {
        match self {
            Reply::Done => Ok(()),
            Reply::Blocked { kind } => Err(failure(format!("a {kind} is in the way"))),
            Reply::Failed { message } => Err(failure(message)),
        }
    }
}

/// What a run's privileged steps go through: the escalation it was given, and — once a step has
/// needed it — the helper holding the privileges.
#[derive(Debug)]
pub struct Privileges {
    escalation: Escalation,
    /// The program started as the helper; the running one when `None`.
    helper: Option<PathBuf>,
    /// The helper, once started; why it isn't there, once that failed (it isn't tried again, which
    /// would only ask for the password once more).
    session: RefCell<Option<std::result::Result<Session, String>>>,
}

impl Privileges {
    pub fn new(escalation: Escalation, helper: Option<PathBuf>) -> Self {
        Privileges { escalation, helper, session: RefCell::new(None) }
    }

    /// Has the helper do `op`. `denied` is what the step met without privileges, returned as it is
    /// when there's no way to escalate: never, or already root.
    pub(crate) fn request(&self, op: &Op, denied: Error) -> Result<Reply> {
        let Some(prefix) = self.escalation.prefix()? else { return Err(denied) };
        let mut slot = self.session.borrow_mut();
        let session = slot.get_or_insert_with(|| Session::start(&prefix, self.helper.as_deref()));
        let gone = match session {
            Ok(session) => match session.ask(op) {
                Ok(reply) => return Ok(reply),
                Err(why) => why,
            },
            Err(why) => return Err(unavailable(why.clone())),
        };
        *slot = Some(Err(gone.clone()));
        Err(unavailable(gone))
    }
}

fn unavailable(message: String) -> Error {
    Error::Escalation { message }
}

/// A running helper.
#[derive(Debug)]
struct Session {
    /// How it was started, for messages.
    command: String,
    child: Child,
    to_helper: Option<ChildStdin>,
    from_helper: BufReader<ChildStdout>,
}

impl Session {
    fn start(prefix: &[String], helper: Option<&Path>) -> std::result::Result<Session, String> {
        let program = match helper {
            Some(program) => program.to_owned(),
            None => std::env::current_exe().map_err(|err| {
                format!("can't find this program to run as its privileged helper: {err}")
            })?,
        };
        let argv = helper_argv(prefix, &program);
        let command = escalate::display_command(&argv);
        // stderr is the terminal's: the password prompt, and why a refused one was refused.
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|err| format!("can't run `{command}`: {err}"))?;
        let to_helper = child.stdin.take();
        let from_helper =
            BufReader::new(child.stdout.take().expect("the helper's stdout is piped"));
        Ok(Session { command, child, to_helper, from_helper })
    }

    fn ask(&mut self, op: &Op) -> std::result::Result<Reply, String> {
        let ended =
            format!("`{}` ended without answering (any reason it gave is above)", self.command);
        let to_helper = self.to_helper.as_mut().ok_or_else(|| ended.clone())?;
        let request = serde_json::to_string(op).expect("steps serialize");
        writeln!(to_helper, "{request}")
            .and_then(|()| to_helper.flush())
            .map_err(|_| ended.clone())?;
        let mut line = String::new();
        match self.from_helper.read_line(&mut line) {
            Ok(0) | Err(_) => Err(ended),
            Ok(_) => serde_json::from_str(line.trim()).map_err(|err| {
                format!("`{}` answered {:?}, which isn't a reply: {err}", self.command, line.trim())
            }),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // The end of its input is the helper's cue to exit.
        drop(self.to_helper.take());
        let _ = self.child.wait();
    }
}

/// The command line the helper is started with: the escalation's own words, then `program` and
/// [`HELPER_MARKER`].
pub fn helper_argv(prefix: &[String], program: &Path) -> Vec<OsString> {
    let mut argv: Vec<OsString> = prefix.iter().map(OsString::from).collect();
    argv.extend([program.as_os_str().to_owned(), HELPER_MARKER.into()]);
    argv
}

/// Serves as the privileged helper, and then returns `true`, when this process was started as one
/// (`argv[1]` is [`HELPER_MARKER`]); returns `false` straight away otherwise.
pub fn try_serve() -> bool {
    if std::env::args_os().nth(1).as_deref() != Some(OsStr::new(HELPER_MARKER)) {
        return false;
    }
    let _ = serve(io::stdin().lock(), io::stdout().lock());
    true
}

/// The helper's loop: a [`Reply`] line for every [`Op`] line, until the input ends.
pub fn serve(input: impl BufRead, mut output: impl Write) -> io::Result<()> {
    for line in input.lines() {
        let line = line?;
        let reply = match serde_json::from_str::<Op>(&line) {
            Ok(op) => op.perform(),
            Err(err) => Reply::Failed { message: format!("not a step: {err}") },
        };
        writeln!(output, "{}", serde_json::to_string(&reply).expect("replies serialize"))?;
        output.flush()?;
    }
    Ok(())
}

/// Paths as their bytes: a Linux path needn't be UTF-8, and a JSON string must be.
mod bytes {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(path.as_os_str().as_bytes())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
        Vec::<u8>::deserialize(deserializer).map(|bytes| PathBuf::from(OsString::from_vec(bytes)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn steps_and_replies_survive_the_wire_even_with_paths_that_arent_utf8() {
        let odd = PathBuf::from(OsString::from_vec(b"/mnt/\xff\xfe".to_vec()));
        let ops = [
            Op::SetLink { link: "/x".into(), target: odd.clone() },
            Op::RemoveLink { link: odd.clone() },
            Op::CreateDir { dir: "/run/devmounts/u".into() },
            Op::RemoveDir { dir: "/run/devmounts/u".into() },
            Op::Mount { options: vec!["ro".into()], device: "/dev/sdb1".into(), dir: odd.clone() },
            Op::Unmount { mount_point: odd },
        ];
        for op in ops {
            let line = serde_json::to_string(&op).unwrap();
            assert!(!line.contains('\n'), "one step, one line: {line}");
            assert_eq!(serde_json::from_str::<Op>(&line).unwrap(), op);
        }
        for reply in [
            Reply::Done,
            Reply::Blocked { kind: "directory".into() },
            Reply::Failed { message: "no\nway".into() },
        ] {
            let line = serde_json::to_string(&reply).unwrap();
            assert!(!line.contains('\n'), "{line}");
            assert_eq!(serde_json::from_str::<Reply>(&line).unwrap(), reply);
        }
    }

    #[test]
    fn the_helper_answers_every_step_in_order_and_refuses_what_isnt_one() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("mnt/dev");
        let link = tmp.path().join("x");
        let lines = [
            serde_json::to_string(&Op::CreateDir { dir: dir.clone() }).unwrap(),
            serde_json::to_string(&Op::SetLink { link: link.clone(), target: dir.clone() })
                .unwrap(),
            "garbage".to_owned(),
            serde_json::to_string(&Op::SetLink {
                link: tmp.path().join("mnt"),
                target: dir.clone(),
            })
            .unwrap(),
            serde_json::to_string(&Op::RemoveLink { link: link.clone() }).unwrap(),
        ];
        let mut output = Vec::new();
        serve(io::Cursor::new(lines.join("\n")), &mut output).unwrap();
        let replies: Vec<Reply> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(replies.len(), 5);
        assert_eq!(replies[0], Reply::Done);
        assert_eq!(replies[1], Reply::Done);
        assert!(
            matches!(&replies[2], Reply::Failed { message } if message.starts_with("not a step"))
        );
        assert_eq!(
            replies[3],
            Reply::Blocked { kind: "directory".into() },
            "a directory is never replaced"
        );
        assert_eq!(replies[4], Reply::Done);
        assert!(dir.is_dir() && !link.exists());
    }

    /// The helper's command line is shown — in every message about it — exactly as it is run.
    #[test]
    fn the_helper_is_started_and_shown_as_one_command_line() {
        let argv = helper_argv(&["doas".into(), "-n".into()], Path::new("/opt/my tools/devmounts"));
        let words: Vec<&OsStr> = argv.iter().map(OsString::as_os_str).collect();
        assert_eq!(words, ["doas", "-n", "/opt/my tools/devmounts", HELPER_MARKER]);
        assert_eq!(
            escalate::display_command(&argv),
            "doas -n '/opt/my tools/devmounts' --devmounts-privileged-helper"
        );
    }

    #[test]
    fn without_a_way_to_escalate_the_step_keeps_its_own_error() {
        let privileges = Privileges::new(Escalation::Never, None);
        let denied =
            Error::Io { context: "can't create /x".into(), message: "Permission denied".into() };
        let result = privileges.request(&Op::RemoveLink { link: "/x".into() }, denied.clone());
        assert_eq!(result.unwrap_err(), denied);
    }
}
