//! The `devmounts` binary, run against fixture sysfs/udev/mountinfo trees.

mod common;

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::{mkdir, tmpdir};
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    base: PathBuf,
    root: PathBuf,
    data: PathBuf,
}

impl Fixture {
    /// One ext4 device labelled X, containing `docs/`, mounted at `media/X` — `media/` being the
    /// machine's automounter directory (`DEVMOUNTS_AUTOMOUNT_DIRS`).
    fn new() -> Self {
        let tmp = tmpdir();
        let base = fs::canonicalize(tmp.path()).unwrap();
        let block = mkdir(&base, "sys/class/block/sdb1");
        fs::write(block.join("dev"), "8:17\n").unwrap();
        fs::write(block.join("uevent"), "DEVNAME=sdb1\n").unwrap();
        fs::write(block.join("partition"), "1\n").unwrap();
        let udev = mkdir(&base, "udev");
        fs::write(udev.join("b8:17"), "E:ID_FS_TYPE=ext4\nE:ID_FS_UUID=u-1\nE:ID_FS_LABEL=X\n")
            .unwrap();
        let data = mkdir(&base, "media/X");
        mkdir(&data, "docs");
        fs::write(
            base.join("mountinfo"),
            format!(
                "1 0 0:1 / / rw - rootfs none rw\n30 1 8:17 / {} rw - ext4 /dev/sdb1 rw\n",
                data.display()
            ),
        )
        .unwrap();
        let root = mkdir(&base, "links");
        Fixture { _tmp: tmp, base, root, data }
    }

    fn config(&self, body: &str) -> PathBuf {
        let path = self.base.join("config.toml");
        fs::write(&path, body).unwrap();
        path
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_devmounts"))
            .args(args)
            .arg("--escalate=none")
            .env("DEVMOUNTS_SYSFS", self.base.join("sys"))
            .env("DEVMOUNTS_UDEV_DATA", self.base.join("udev"))
            .env("DEVMOUNTS_MOUNTINFO", self.base.join("mountinfo"))
            .env("DEVMOUNTS_DEV", self.base.join("dev"))
            .env("DEVMOUNTS_AUTOMOUNT_DIRS", self.base.join("media"))
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    }

    /// No device is mounted any more.
    fn unmount_all(&self) {
        fs::write(self.base.join("mountinfo"), "1 0 0:1 / / rw - rootfs none rw\n").unwrap();
    }

    fn run_plain(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_devmounts")).args(args).output().unwrap()
    }

    /// Runs the binary with nothing but `bin` ([`fake_commands`]) as its `PATH`.
    fn run_with(&self, bin: &Path, args: &[&str]) -> Output {
        self.run_typing(bin, args, "")
    }

    /// [`run_with`](Self::run_with), with `typed` as what the terminal gives it.
    fn run_typing(&self, bin: &Path, args: &[&str], typed: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_devmounts"))
            .args(args)
            .env_clear()
            .env("PATH", bin)
            .env("DEVMOUNTS_SYSFS", self.base.join("sys"))
            .env("DEVMOUNTS_UDEV_DATA", self.base.join("udev"))
            .env("DEVMOUNTS_MOUNTINFO", self.base.join("mountinfo"))
            .env("DEVMOUNTS_DEV", self.base.join("dev"))
            .env("DEVMOUNTS_AUTOMOUNT_DIRS", self.base.join("media"))
            .env("DEVMOUNTS_TEST_LOG", self.command_log())
            .env("NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(typed.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    }

    /// A second device, `sdb2`: a locked LUKS container (UUID `luks-1`). What unlocking it opens,
    /// `dm-0`, is in the kernel's list already, but neither stacked on it nor identified: the
    /// stand-in `udisksctl unlock` does both, as unlocking would.
    fn add_vault(&self) {
        let block = mkdir(&self.base, "sys/class/block/sdb2");
        fs::write(block.join("dev"), "8:18\n").unwrap();
        fs::write(block.join("uevent"), "DEVNAME=sdb2\n").unwrap();
        fs::write(block.join("partition"), "2\n").unwrap();
        mkdir(&block, "holders");
        fs::write(self.base.join("udev/b8:18"), "E:ID_FS_TYPE=crypto_LUKS\nE:ID_FS_UUID=luks-1\n")
            .unwrap();
        let inside = mkdir(&self.base, "sys/class/block/dm-0");
        fs::write(inside.join("dev"), "253:0\n").unwrap();
        fs::write(inside.join("uevent"), "DEVNAME=dm-0\n").unwrap();
    }

    /// The commands [`fake_commands`] were run with, one per line.
    fn command_log(&self) -> PathBuf {
        self.base.join("commands.log")
    }

    fn commands_run(&self) -> Vec<String> {
        fs::read_to_string(self.command_log())
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

const CONFIG: &str = "[[device]]\nlabel = 'X'\ndevmount = 'x'\nmounts = { docs = 'docs' }\n";

#[test]
fn no_arguments_prints_help() {
    let f = Fixture::new();
    let out = f.run_plain(&[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("Usage:"),
        "help for a missing CONFIG is a usage error: {}",
        stderr(&out)
    );
}

/// Each usage mistake says what's wrong, not just that something is — and says it as an `error:`,
/// the way clap words its own, whichever of the two caught it.
#[test]
fn usage_mistakes_are_explained() {
    let f = Fixture::new();
    let config = f.config(CONFIG);
    let config = config.to_str().unwrap();
    for (argv, explained) in [
        (&["status"][..], "<CONFIG>"),
        (&["--json"], "no CONFIG given"),
        (&["--json", "status", config], "go after the subcommand"),
        (&[config, "--root", "relative"], "absolute path without '..'"),
        (&[config, "--escalate", ""], "is empty"),
    ] {
        let out = f.run_plain(argv);
        assert_eq!(out.status.code(), Some(2), "{argv:?}");
        assert!(stderr(&out).contains(explained), "{argv:?}: {}", stderr(&out));
        assert!(stderr(&out).starts_with("error: "), "{argv:?}: {}", stderr(&out));
    }
}

#[test]
fn config_errors_exit_with_2() {
    let f = Fixture::new();
    let out = f.run_plain(&[f.base.join("missing.toml").to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).starts_with("error: can't read config file"), "{}", stderr(&out));

    let bad = f.config("[[device]]\ndevmount = 'x'\nmounts = { 'a/b' = 'x' }\n");
    let out = f.run_plain(&["status", bad.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    assert!(err.starts_with("error: invalid config") && err.contains("needs one of uuid"), "{err}");

    let out = f.run_plain(&[f.config(CONFIG).to_str().unwrap(), "--root", "relative"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn apply_status_remove_cycle() {
    let f = Fixture::new();
    let config = f.config(CONFIG);
    let config = config.to_str().unwrap();
    let root = f.root.to_str().unwrap();
    let devmount = f.root.join("x");
    let link = f.root.join("docs");

    // Status before anything exists: not ok.
    let out = f.run(&["status", config, "--root", root]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(stdout(&out).contains("missing"));

    // Dry run: nothing happens.
    let out = f.run(&["apply", "--dry-run", config, "--root", root]);
    assert_eq!(out.status.code(), Some(0), "{}{}", stdout(&out), stderr(&out));
    assert!(stdout(&out).contains("would create"), "{}", stdout(&out));
    assert!(!devmount.is_symlink() && !link.is_symlink());

    // The bare form means apply.
    let out = f.run(&[config, "--root", root]);
    assert_eq!(out.status.code(), Some(0), "{}{}", stdout(&out), stderr(&out));
    assert!(stdout(&out).contains("created"), "{}", stdout(&out));
    assert_eq!(fs::read_link(&devmount).unwrap(), f.data);
    assert_eq!(fs::read_link(&link).unwrap(), devmount.join("docs"));

    let out = f.run(&["status", config, "--root", root, "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["command"], "status");
    assert_eq!(json["devices"][0]["devmount"]["outcome"]["outcome"], "unchanged");
    assert_eq!(json["devices"][0]["mounts"][0]["outcome"]["outcome"], "unchanged");
    assert_eq!(json["devices"][0]["mounted_at"][0]["mount_point"], f.data.to_str().unwrap());

    let out = f.run(&["remove", config, "--root", root]);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(!devmount.is_symlink() && !link.is_symlink());
}

#[test]
fn problems_exit_with_1() {
    let f = Fixture::new();
    let config = f.config(
        "[[device]]\nlabel = 'X'\ndevmount = 'x'\nmounts = { docs = 'docs', nope = 'nope' }\n\
         [[device]]\nlabel = 'GONE'\ndevmount = 'gone'\nmounts = { g = 'g' }\n",
    );
    let out = f.run(&[config.to_str().unwrap(), "--root", f.root.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(text.contains("created"), "{text}");
    assert!(text.contains("doesn't exist"), "{text}");
    assert!(text.contains("no device with LABEL=\"GONE\""), "{text}");
}

#[test]
fn lists_devices() {
    let f = Fixture::new();
    let out = f.run(&["devices", "--json"]);
    // `--escalate` is passed by `run` but isn't a `devices` option.
    assert_eq!(out.status.code(), Some(2));

    let out = Command::new(env!("CARGO_BIN_EXE_devmounts"))
        .args(["devices", "--json"])
        .env("DEVMOUNTS_SYSFS", f.base.join("sys"))
        .env("DEVMOUNTS_UDEV_DATA", f.base.join("udev"))
        .env("DEVMOUNTS_MOUNTINFO", f.base.join("mountinfo"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json[0]["kname"], "sdb1");
    assert_eq!(json[0]["label"], "X");
    assert_eq!(json[0]["mounts"][0]["mount_point"], f.data.to_str().unwrap());
}

/// Stand-ins for every command devmounts runs, each logging its arguments to `$DEVMOUNTS_TEST_LOG`,
/// in a directory that will be the whole `PATH` (the real commands can't be reached), so they
/// make do with the shell's builtins: `udisksctl`, refusing to mount for want of a password, as
/// for an internal drive, and unlocking as udisksctl does — asking for the passphrase on stdout,
/// reading it from stdin (`secret` opens it) — by stacking `dm-0` on the container in the
/// fixture's sysfs; a `sudo` that just runs what it's given; and a `mount`/`umount` that put the
/// mount into, and take it out of, the fixture's mountinfo.
fn fake_commands(f: &Fixture) -> PathBuf {
    let bin = mkdir(&f.base, "bin");
    let script = |name: &str, body: &str| {
        let path = bin.join(name);
        fs::write(
            &path,
            format!("#!/bin/sh\necho \"{name} $*\" >> \"$DEVMOUNTS_TEST_LOG\"\n{body}"),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    };
    script(
        "udisksctl",
        "case \"$1\" in\n\
         status) exit 0 ;;\n\
         unlock)\n\
             printf 'Passphrase: '\n\
             read -r passphrase\n\
             if [ \"$passphrase\" != secret ]; then\n\
                 echo \"Error unlocking $3: Failed to activate device: Operation not permitted\" >&2\n\
                 exit 1\n\
             fi\n\
             : > \"$DEVMOUNTS_SYSFS/class/block/${3##*/}/holders/dm-0\"\n\
             printf 'E:ID_FS_TYPE=ext4\\nE:ID_FS_UUID=fs-1\\nE:ID_FS_LABEL=INSIDE\\n' > \"$DEVMOUNTS_UDEV_DATA/b253:0\"\n\
             echo \"Unlocked $3 as /dev/dm-0.\"\n\
             exit 0 ;;\n\
         lock) echo \"Locked $3.\"; exit 0 ;;\n\
         esac\n\
         echo 'Error mounting: GDBus.Error:org.freedesktop.UDisks2.Error.NotAuthorizedCanObtain: Not authorized' >&2\n\
         exit 1\n",
    );
    script("sudo", "exec \"$@\"\n");
    script(
        "mount",
        "while [ \"$1\" != -- ]; do shift; done\n\
         read -r devnum < \"$DEVMOUNTS_SYSFS/class/block/${2##*/}/dev\"\n\
         printf '31 1 %s / %s rw - ext4 %s rw\\n' \"$devnum\" \"$3\" \"$2\" >> \"$DEVMOUNTS_MOUNTINFO\"\n",
    );
    script(
        "umount",
        "kept=''\n\
         while IFS= read -r line; do case \"$line\" in *\" $2 \"*) ;; *) kept=\"$kept$line\n\" ;; esac; done < \"$DEVMOUNTS_MOUNTINFO\"\n\
         printf '%s' \"$kept\" > \"$DEVMOUNTS_MOUNTINFO\"\n",
    );
    bin
}

/// `auto` on a drive udisks2 wants an administrator for (an internal one) mounts it the `mount`
/// way instead: with exactly these commands, through one privileged helper per run. `remove
/// --unmount` takes that mount down the same way.
#[test]
fn an_internal_drive_is_mounted_through_one_privileged_helper_per_run() {
    if devmounts::escalate::is_root() {
        return; // root mounts directly: no udisks2, no helper
    }
    let f = Fixture::new();
    f.unmount_all();
    let bin = fake_commands(&f);
    let mnt = mkdir(&f.base, "mnt");
    mkdir(&mnt, "u-1/docs"); // what the mounted drive will show
    let config = f.config(&format!("settings.mount_base = {mnt:?}\n{CONFIG}"));
    let (config, root) = (config.to_str().unwrap(), f.root.to_str().unwrap());
    let sudo = bin.join("sudo");
    let run = |args: &[&str]| {
        f.run_with(&bin, &[args, &["--root", root, "--escalate", sudo.to_str().unwrap()]].concat())
    };

    let applied = run(&["apply", config]);
    assert_eq!(applied.status.code(), Some(0), "{}{}", stdout(&applied), stderr(&applied));
    assert!(stdout(&applied).contains("mounted it"), "{}", stdout(&applied));
    assert_eq!(fs::read_link(f.root.join("x")).unwrap(), mnt.join("u-1"));
    let removed = run(&["remove", "--unmount", config]);
    assert_eq!(removed.status.code(), Some(0), "{}{}", stdout(&removed), stderr(&removed));
    assert!(
        stdout(&removed).contains(&format!("unmounted {}", mnt.join("u-1").display())),
        "{}",
        stdout(&removed)
    );

    let devnode = f.base.join("dev/sdb1");
    let helper = format!("sudo {} --devmounts-privileged-helper", env!("CARGO_BIN_EXE_devmounts"));
    assert_eq!(
        f.commands_run(),
        [
            "udisksctl status".to_owned(),
            format!("udisksctl mount --block-device {} --no-user-interaction", devnode.display()),
            helper.clone(),
            format!("mount -- {} {}", devnode.display(), mnt.join("u-1").display()),
            helper,
            format!("umount -- {}", mnt.join("u-1").display()),
        ]
    );
}

/// When udisks2 wants a password and the `mount` way can't get root either, the error says what
/// both attempts met — and nothing is started that could ask for a password.
#[test]
fn a_drive_neither_way_can_mount_says_why_for_both() {
    if devmounts::escalate::is_root() {
        return; // root mounts directly: no udisks2, no helper
    }
    let f = Fixture::new();
    f.unmount_all();
    let bin = fake_commands(&f);
    let mnt = mkdir(&f.base, "mnt");
    let config = f.config(&format!("settings.mount_base = {mnt:?}\n{CONFIG}"));
    let out = f.run_with(
        &bin,
        &[
            "apply",
            config.to_str().unwrap(),
            "--root",
            f.root.to_str().unwrap(),
            "--escalate",
            "none",
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{}{}", stdout(&out), stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("udisks2 wants an administrator's password for it, and mounting it with `mount` instead failed"), "{text}");
    assert!(text.contains("needs root, and privilege escalation is off"), "{text}");
    assert!(!mnt.join("u-1").exists(), "the mount point made for it is removed again");
    let devnode = f.base.join("dev/sdb1");
    assert_eq!(
        f.commands_run(),
        [
            "udisksctl status".to_owned(),
            format!("udisksctl mount --block-device {} --no-user-interaction", devnode.display())
        ]
    );
}

/// `auto` takes an automounter's mount down through udisks2 without letting it prompt, and where
/// udisks2 wants a password, with `umount` through the run's helper instead.
#[test]
fn auto_unmounts_with_umount_where_udisks2_wants_a_password() {
    if devmounts::escalate::is_root() {
        return; // root unmounts directly: no udisks2, no helper
    }
    let f = Fixture::new();
    let bin = fake_commands(&f);
    let config = f.config(CONFIG);
    let sudo = bin.join("sudo");
    let args =
        ["remove", "--unmount", config.to_str().unwrap(), "--root", f.root.to_str().unwrap()];

    // With no way to root, the error says what both attempts met.
    let out = f.run_with(&bin, &[&args[..], &["--escalate", "none"]].concat());
    assert_eq!(out.status.code(), Some(1), "{}{}", stdout(&out), stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("udisks2 wants an administrator's password for it, and unmounting it with `umount` instead failed"), "{text}");
    assert!(text.contains("needs root, and privilege escalation is off"), "{text}");
    fs::remove_file(f.command_log()).unwrap();

    let out = f.run_with(&bin, &[&args[..], &["--escalate", sudo.to_str().unwrap()]].concat());
    assert_eq!(out.status.code(), Some(0), "{}{}", stdout(&out), stderr(&out));
    assert!(stdout(&out).contains(&format!("unmounted {}", f.data.display())), "{}", stdout(&out));
    let devnode = f.base.join("dev/sdb1");
    assert_eq!(
        f.commands_run(),
        [
            "udisksctl status".to_owned(),
            format!("udisksctl unmount --block-device {} --no-user-interaction", devnode.display()),
            format!("sudo {} --devmounts-privileged-helper", env!("CARGO_BIN_EXE_devmounts")),
            format!("umount -- {}", f.data.display()),
        ]
    );
}

/// With `mount_method = "udisks"`, an automounter's mount comes down through udisks2 alone, as it
/// would go up: its own prompt allowed (no `--no-user-interaction`), and nothing else tried when it
/// refuses — no helper, no `umount`.
#[test]
fn with_the_udisks_method_an_automounters_mount_comes_down_through_udisks2_alone() {
    let f = Fixture::new();
    let bin = fake_commands(&f);
    let config = f.config(&format!("settings.mount_method = 'udisks'\n{CONFIG}"));
    let sudo = bin.join("sudo");
    let args =
        ["remove", "--unmount", config.to_str().unwrap(), "--root", f.root.to_str().unwrap()];
    let out = f.run_with(&bin, &[&args[..], &["--escalate", sudo.to_str().unwrap()]].concat());
    assert_eq!(out.status.code(), Some(1), "{}{}", stdout(&out), stderr(&out));
    let text = stdout(&out);
    assert!(text.contains(&format!("✗ unmounting {} failed", f.data.display())), "{text}");
    assert!(text.contains("NotAuthorized"), "udisks2's own refusal is what's reported: {text}");
    let devnode = f.base.join("dev/sdb1");
    assert_eq!(
        f.commands_run(),
        [format!("udisksctl unmount --block-device {}", devnode.display())]
    );
}

/// A device in a locked LUKS container, end to end. udisksctl unlocks it, asking for the
/// passphrase itself on the terminal — its prompt on stderr, never in the report on stdout, its
/// answer read from stdin. The device list, read again, shows what it opened, and that is mounted
/// and linked; `remove --unmount` then locks it again once its last mount is down.
#[test]
fn a_luks_device_is_unlocked_asking_on_the_terminal_and_locked_again() {
    if devmounts::escalate::is_root() {
        return; // root mounts directly: no helper
    }
    let f = Fixture::new();
    f.unmount_all();
    f.add_vault();
    let bin = fake_commands(&f);
    let mnt = mkdir(&f.base, "mnt");
    mkdir(&mnt, "fs-1/docs"); // what the opened filesystem will show
    let config = f.config(&format!(
        "settings.mount_base = {mnt:?}\n[[device]]\nuuid = 'luks-1'\ndevmount = 'v'\nmounts = {{ docs = 'docs' }}\n"
    ));
    let (config, root) = (config.to_str().unwrap(), f.root.to_str().unwrap());
    let sudo = bin.join("sudo");
    let tail = ["--root", root, "--escalate", sudo.to_str().unwrap()];

    let applied =
        f.run_typing(&bin, &[&["apply", "--json", config][..], &tail].concat(), "secret\n");
    assert_eq!(applied.status.code(), Some(0), "{}{}", stdout(&applied), stderr(&applied));
    let report: serde_json::Value =
        serde_json::from_slice(&applied.stdout).expect("stdout is the report, and only it");
    let (vault, inside) = (f.base.join("dev/sdb2"), f.base.join("dev/dm-0"));
    let changes = &report["devices"][0]["mount_changes"];
    assert_eq!(changes[0], serde_json::json!({"change": "unlocked", "container": vault}));
    assert_eq!(changes[1], serde_json::json!({"change": "mounted", "top_level": false}));
    let asked = stderr(&applied);
    assert!(
        asked.contains("Passphrase: ") && asked.contains("Unlocked "),
        "udisksctl's own words, on stderr: {asked}"
    );
    assert_eq!(fs::read_link(f.root.join("v")).unwrap(), mnt.join("fs-1"));
    assert_eq!(fs::read_link(f.root.join("docs")).unwrap(), f.root.join("v/docs"));

    let removed = f.run_with(&bin, &[&["remove", "--unmount", config][..], &tail].concat());
    assert_eq!(removed.status.code(), Some(0), "{}{}", stdout(&removed), stderr(&removed));
    assert!(
        stdout(&removed).contains(&format!("locked {}", vault.display())),
        "{}",
        stdout(&removed)
    );

    let helper = format!("sudo {} --devmounts-privileged-helper", env!("CARGO_BIN_EXE_devmounts"));
    assert_eq!(
        f.commands_run(),
        [
            format!("udisksctl unlock --block-device {} --no-user-interaction", vault.display()),
            "udisksctl status".to_owned(),
            format!("udisksctl mount --block-device {} --no-user-interaction", inside.display()),
            helper.clone(),
            format!("mount -- {} {}", inside.display(), mnt.join("fs-1").display()),
            helper,
            format!("umount -- {}", mnt.join("fs-1").display()),
            format!("udisksctl lock --block-device {} --no-user-interaction", vault.display()),
        ]
    );
}

/// A refused passphrase is the device's problem: the report says unlocking failed, udisksctl's own
/// reason having been shown on the terminal, and nothing more is done for it.
#[test]
fn a_refused_passphrase_unlocks_nothing_and_says_so() {
    let f = Fixture::new();
    f.unmount_all();
    f.add_vault();
    let bin = fake_commands(&f);
    let config = f.config("[[device]]\nuuid = 'luks-1'\ndevmount = 'v'\n");
    let args = [
        "apply",
        config.to_str().unwrap(),
        "--root",
        f.root.to_str().unwrap(),
        "--escalate",
        "none",
    ];
    let out = f.run_typing(&bin, &args, "wrong\n");
    assert_eq!(out.status.code(), Some(1), "{}{}", stdout(&out), stderr(&out));
    let vault = f.base.join("dev/sdb2");
    let text = stdout(&out);
    assert!(
        text.contains(&format!("✗ unlocking {} failed: exit status: 1", vault.display())),
        "{text}"
    );
    assert!(text.contains("its reason is above"), "{text}");
    assert!(
        stderr(&out).contains("Error unlocking"),
        "udisksctl's reason, on the terminal: {}",
        stderr(&out)
    );
    assert_eq!(
        f.commands_run(),
        [format!("udisksctl unlock --block-device {} --no-user-interaction", vault.display())]
    );
    assert!(!f.root.join("v").exists());
}
