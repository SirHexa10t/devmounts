//! Inspecting, creating and removing the links.
//!
//! A link is only ever changed in place of a symlink, and atomically: the new link is swapped in
//! with `renameat2(RENAME_EXCHANGE)`, and what it replaced is checked — a directory or file that
//! appeared since [`inspect`] looked is swapped straight back, untouched. A link is taken down the
//! same way: moved aside, checked, deleted. Changes are made directly when possible, and through
//! the run's privileged helper ([`crate::privileged`]) only after a permission error.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Error, Result};
use crate::privileged::{Op, Privileges};

/// What's currently at a link's path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LinkState {
    /// Nothing there.
    Missing,
    /// A symlink pointing exactly at the wanted target.
    Correct,
    /// A symlink whose target doesn't exist (e.g. the device was unplugged or remounted).
    Dangling { current: PathBuf },
    /// A symlink to something that exists. `on_device` tells whether that is on the
    /// device the link belongs to; links to anything else are someone else's.
    Elsewhere { current: PathBuf, on_device: bool },
    /// Something that isn't a symlink. Never touched.
    Blocked { kind: String },
}

/// Looks at `link`. `target` is the wanted target, if known; `on_device` says whether a
/// resolved path lies on the link's device.
pub fn inspect(
    link: &Path,
    target: Option<&Path>,
    on_device: impl Fn(&Path) -> bool,
) -> Result<LinkState> {
    let meta = match fs::symlink_metadata(link) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(LinkState::Missing),
        Err(err) => return Err(Error::io(format!("can't inspect {}", link.display()), &err)),
    };
    if !meta.file_type().is_symlink() {
        return Ok(LinkState::Blocked { kind: kind_name(&meta).into() });
    }
    let raw = fs::read_link(link)
        .map_err(|err| Error::io(format!("can't read {}", link.display()), &err))?;
    // A relative link is relative to the directory it's in.
    let current = match link.parent() {
        Some(parent) if raw.is_relative() => parent.join(&raw),
        _ => raw,
    };
    if target.is_some_and(|t| t == current) {
        return Ok(LinkState::Correct);
    }
    match fs::canonicalize(link) {
        Ok(resolved) => Ok(LinkState::Elsewhere { on_device: on_device(&resolved), current }),
        Err(err)
            if matches!(err.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) =>
        {
            Ok(LinkState::Dangling { current })
        }
        // Loops, permission problems on the way: treat like a link we can't vouch for.
        Err(_) => Ok(LinkState::Elsewhere { on_device: false, current }),
    }
}

fn kind_name(meta: &fs::Metadata) -> &'static str {
    use std::os::unix::fs::FileTypeExt;
    let t = meta.file_type();
    if t.is_dir() {
        "directory"
    } else if t.is_file() {
        "file"
    } else if t.is_block_device() || t.is_char_device() {
        "device node"
    } else if t.is_fifo() {
        "fifo"
    } else if t.is_socket() {
        "socket"
    } else {
        "special file"
    }
}

/// What a change at a link's path came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placed {
    Done,
    /// Something other than a symlink is there (a directory, a file, …), left as it was.
    Blocked {
        kind: String,
    },
}

/// Points `link` at `target`, in place of the symlink that's there, if any.
pub fn set_link(link: &Path, target: &Path, privileges: &Privileges) -> Result<()> {
    let context = || format!("can't create {}", link.display());
    let placed = match put_symlink(link, target) {
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            let op = Op::SetLink { link: link.to_owned(), target: target.to_owned() };
            privileges.request(&op, Error::io(context(), &err))?.placed(context)?
        }
        result => result.map_err(|err| Error::io(context(), &err))?,
    };
    in_place(link, placed)
}

/// Removes the symlink at `link` (never what it points to, and never anything but a symlink).
pub fn remove_link(link: &Path, privileges: &Privileges) -> Result<()> {
    let context = || format!("can't remove {}", link.display());
    let placed = match take_symlink(link) {
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => privileges
            .request(&Op::RemoveLink { link: link.to_owned() }, Error::io(context(), &err))?
            .placed(context)?,
        result => result.map_err(|err| Error::io(context(), &err))?,
    };
    in_place(link, placed)
}

fn in_place(link: &Path, placed: Placed) -> Result<()> {
    match placed {
        Placed::Done => Ok(()),
        Placed::Blocked { kind } => Err(Error::LinkBlocked { link: link.to_owned(), kind }),
    }
}

/// [`set_link`]'s work, wherever it's done: a symlink to `target` put at `link`, atomically, and
/// only in place of a symlink (or of nothing).
pub(crate) fn put_symlink(link: &Path, target: &Path) -> io::Result<Placed> {
    let temp = aside(link)?;
    let _ = fs::remove_file(&temp); // a leftover of this very process id
    std::os::unix::fs::symlink(target, &temp)?;
    // A second round only when something appeared at `link` in between.
    for _ in 0..2 {
        match renameat2(&temp, link, libc::RENAME_EXCHANGE) {
            Ok(()) => {
                // `temp` now holds what was at `link`.
                let old = fs::symlink_metadata(&temp)?;
                if old.file_type().is_symlink() {
                    fs::remove_file(&temp)?;
                    return Ok(Placed::Done);
                }
                renameat2(&temp, link, libc::RENAME_EXCHANGE)?;
                fs::remove_file(&temp)?;
                return Ok(Placed::Blocked { kind: kind_name(&old).into() });
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                match renameat2(&temp, link, libc::RENAME_NOREPLACE) {
                    Ok(()) => return Ok(Placed::Done),
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(err) => return Err(discard(&temp, err)),
                }
            }
            Err(err) if unsupported(&err) => return replace_checked(&temp, link),
            Err(err) => return Err(discard(&temp, err)),
        }
    }
    Err(discard(
        &temp,
        io::Error::other(format!("{} kept changing while the link was set", link.display())),
    ))
}

/// Where the filesystem can't exchange two names: the same check, right before a plain rename —
/// a far narrower window than between [`inspect`] and the change, if not none.
fn replace_checked(temp: &Path, link: &Path) -> io::Result<Placed> {
    match fs::symlink_metadata(link) {
        Ok(meta) if !meta.file_type().is_symlink() => {
            fs::remove_file(temp)?;
            Ok(Placed::Blocked { kind: kind_name(&meta).into() })
        }
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(discard(temp, err)),
        _ => fs::rename(temp, link).map(|()| Placed::Done).map_err(|err| discard(temp, err)),
    }
}

/// [`remove_link`]'s work, wherever it's done: the symlink at `link` moved aside, checked, and
/// deleted — anything else is moved straight back. Nothing there counts as done.
pub(crate) fn take_symlink(link: &Path) -> io::Result<Placed> {
    let temp = aside(link)?;
    let _ = fs::remove_file(&temp);
    match renameat2(link, &temp, libc::RENAME_NOREPLACE) {
        Ok(()) => {
            let taken = fs::symlink_metadata(&temp)?;
            if taken.file_type().is_symlink() {
                fs::remove_file(&temp)?;
                return Ok(Placed::Done);
            }
            renameat2(&temp, link, libc::RENAME_NOREPLACE)?;
            Ok(Placed::Blocked { kind: kind_name(&taken).into() })
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Placed::Done),
        Err(err) if unsupported(&err) => remove_checked(link),
        Err(err) => Err(err),
    }
}

/// [`take_symlink`] where the filesystem can't rename without replacing: check, then delete.
fn remove_checked(link: &Path) -> io::Result<Placed> {
    match fs::symlink_metadata(link) {
        Ok(meta) if !meta.file_type().is_symlink() => {
            Ok(Placed::Blocked { kind: kind_name(&meta).into() })
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Placed::Done),
        Err(err) => Err(err),
        Ok(_) => match fs::remove_file(link) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Placed::Done),
            other => other.map(|()| Placed::Done),
        },
    }
}

/// The name a link is moved through: `.<name>.devmounts-<pid>`, beside it (so a rename stays on
/// one filesystem).
fn aside(link: &Path) -> io::Result<PathBuf> {
    let name = link
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "link has no name"))?;
    let mut temp = std::ffi::OsString::from(".");
    temp.push(name);
    temp.push(format!(".devmounts-{}", std::process::id()));
    Ok(link.with_file_name(temp))
}

/// Our own temporary symlink is in the way of nothing any more: removes it, and returns `err`.
fn discard(temp: &Path, err: io::Error) -> io::Error {
    let _ = fs::remove_file(temp);
    err
}

/// The filesystem (or kernel) can't do what was asked of `renameat2`.
fn unsupported(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP))
}

fn renameat2(from: &Path, to: &Path, flags: libc::c_uint) -> io::Result<()> {
    let nul = |_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte");
    let from = CString::new(from.as_os_str().as_bytes()).map_err(nul)?;
    let to = CString::new(to.as_os_str().as_bytes()).map_err(nul)?;
    // SAFETY: both pointers are to NUL-terminated strings that outlive the call, and
    // renameat2 reads nothing else.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            flags,
        )
    };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn read(link: &Path) -> PathBuf {
        fs::read_link(link).unwrap()
    }

    #[test]
    fn a_link_is_put_where_nothing_or_a_symlink_was() {
        let tmp = tempfile::tempdir().unwrap();
        let (target, other) = (tmp.path().join("target"), tmp.path().join("other"));
        fs::create_dir(&target).unwrap();
        let link = tmp.path().join("x");
        assert_eq!(put_symlink(&link, &target).unwrap(), Placed::Done, "nothing there");
        assert_eq!(read(&link), target);
        assert_eq!(put_symlink(&link, &other).unwrap(), Placed::Done, "a symlink there");
        assert_eq!(read(&link), other);
        let names: Vec<_> =
            fs::read_dir(tmp.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 2, "no temporary link left behind: {names:?}");
    }

    #[test]
    fn a_link_is_never_put_in_place_of_a_directory_or_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dir");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("inside"), "kept").unwrap();
        let file = tmp.path().join("file");
        fs::write(&file, "kept").unwrap();
        assert_eq!(put_symlink(&dir, &file).unwrap(), Placed::Blocked { kind: "directory".into() });
        assert_eq!(put_symlink(&file, &dir).unwrap(), Placed::Blocked { kind: "file".into() });
        assert_eq!(
            fs::read_to_string(dir.join("inside")).unwrap(),
            "kept",
            "nothing written inside it either"
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), "kept");
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 2, "no temporary link left behind");
    }

    #[test]
    fn only_a_symlink_is_ever_taken_down() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("x");
        symlink("/nowhere", &link).unwrap();
        assert_eq!(take_symlink(&link).unwrap(), Placed::Done);
        assert!(fs::symlink_metadata(&link).is_err());
        assert_eq!(take_symlink(&link).unwrap(), Placed::Done, "nothing there is done already");
        let dir = tmp.path().join("dir");
        fs::create_dir(&dir).unwrap();
        let file = tmp.path().join("file");
        fs::write(&file, "kept").unwrap();
        assert_eq!(take_symlink(&dir).unwrap(), Placed::Blocked { kind: "directory".into() });
        assert_eq!(take_symlink(&file).unwrap(), Placed::Blocked { kind: "file".into() });
        assert!(dir.is_dir() && fs::read_to_string(&file).unwrap() == "kept");
        assert_eq!(
            fs::read_dir(tmp.path()).unwrap().count(),
            2,
            "nothing moved aside and forgotten"
        );
    }

    #[test]
    fn without_renameat2_the_same_checks_hold() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("x");
        let temp = aside(&link).unwrap();
        symlink("/a", &temp).unwrap();
        assert_eq!(replace_checked(&temp, &link).unwrap(), Placed::Done);
        assert_eq!(read(&link), Path::new("/a"));
        let dir = tmp.path().join("dir");
        fs::create_dir(&dir).unwrap();
        let temp = aside(&dir).unwrap();
        symlink("/a", &temp).unwrap();
        assert_eq!(
            replace_checked(&temp, &dir).unwrap(),
            Placed::Blocked { kind: "directory".into() }
        );
        assert!(dir.is_dir() && fs::symlink_metadata(&temp).is_err());
        assert_eq!(remove_checked(&dir).unwrap(), Placed::Blocked { kind: "directory".into() });
        assert_eq!(remove_checked(&link).unwrap(), Placed::Done);
        assert_eq!(remove_checked(&link).unwrap(), Placed::Done);
    }
}
