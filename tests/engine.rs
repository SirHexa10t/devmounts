//! The commands end to end, against a fake machine with real directories and links.

mod common;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use common::{FakeSystem, device, mkdir, tmpdir};
use devmounts::engine::link_root;
use devmounts::{
    BlockDevice, Colors, Config, Error, Escalation, LinkOutcome, LinkReport, MountChange, Options,
    Platform, Report,
};
use tempfile::TempDir;

struct Setup {
    _tmp: TempDir,
    base: PathBuf,
    root: PathBuf,
    /// The automounter's directory: where devices "already mounted" are.
    media: PathBuf,
    /// The configured `mount_base`: where devmounts' own (`mount` method) mounts go.
    mnt: PathBuf,
    sys: FakeSystem,
}

impl Setup {
    fn new() -> Self {
        let tmp = tmpdir();
        let base = fs::canonicalize(tmp.path()).unwrap();
        let root = mkdir(&base, "root");
        let media = mkdir(&base, "media");
        let mnt = mkdir(&base, "mnt");
        let sys = FakeSystem::new(
            vec![
                device("sdb1", 8, 17, "ext4", "uuid-data", Some("DATA")),
                device("sdc1", 8, 33, "btrfs", "uuid-pool", Some("POOL")),
                device("sdz1", 8, 241, "ext4", "uuid-other", Some("OTHER")),
            ],
            media.clone(),
        );
        Setup { _tmp: tmp, base, root, media, mnt, sys }
    }

    /// Mounts a device at `media/<dir>` with the given directories inside.
    fn mount(&self, kname: &str, dir: &str, dirs: &[&str]) -> PathBuf {
        let at = mkdir(&self.media, dir);
        for d in dirs {
            mkdir(&at, d);
        }
        self.sys.add_mount(kname, "/", &at);
        at
    }

    fn mount_data(&self, dirs: &[&str]) -> PathBuf {
        self.mount("sdb1", "DATA", dirs)
    }

    fn unmount(&self, at: &Path) {
        self.sys.mounts.borrow_mut().retain(|m| m.mount_point != at);
    }

    fn config(&self, devices: &str) -> Config {
        format!("[settings]\nroot = {:?}\nmount_base = {:?}\n{devices}", self.root, self.mnt)
            .parse()
            .unwrap()
    }

    /// `[[device]]` with an id line, a devmount and a mounts table.
    fn device(&self, id: &str, devmount: &str, mounts: &str) -> String {
        format!("[[device]]\n{id}\ndevmount = '{devmount}'\nmounts = {{ {mounts} }}\n")
    }

    fn link(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn apply(&self, config: &Config, options: &Options) -> Report {
        devmounts::apply(config, options, &self.sys).unwrap()
    }
}

/// Never escalate in tests: nothing may run sudo.
fn opts() -> Options {
    Options { escalation: Some(Escalation::Never), ..Options::default() }
}

fn outcome(link: &LinkReport) -> String {
    match &link.outcome {
        LinkOutcome::Failed { error } => format!("failed:{}", error.kind()),
        other => serde_json::to_value(other).unwrap()["outcome"].as_str().unwrap().to_owned(),
    }
}

/// `(name, outcome)` for every devmount and mount link, in order.
fn outcomes(report: &Report) -> Vec<(String, String)> {
    report
        .devices
        .iter()
        .flat_map(|d| std::iter::once(&d.devmount).chain(&d.mounts))
        .map(|l| (l.name.clone(), outcome(l)))
        .collect()
}

fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
}

fn read_link(path: &Path) -> PathBuf {
    fs::read_link(path).unwrap()
}

#[test]
fn mounts_go_through_the_devmount() {
    let s = Setup::new();
    let data = s.mount_data(&["backups", "photos/2024"]);
    let config = s.config(&s.device(
        "label = 'DATA'",
        "data",
        "backups = 'backups', pics = '/photos/2024/'",
    ));

    let report = s.apply(&config, &opts());
    assert_eq!(
        outcomes(&report),
        pairs(&[("data", "created"), ("backups", "created"), ("pics", "created")])
    );
    assert!(report.ok());
    assert_eq!(read_link(&s.link("data")), data);
    assert_eq!(read_link(&s.link("backups")), s.root.join("data/backups"));
    assert_eq!(read_link(&s.link("pics")), s.root.join("data/photos/2024"));
    assert_eq!(fs::canonicalize(s.link("pics")).unwrap(), data.join("photos/2024"));
    assert_eq!(report.devices[0].mounted_at.len(), 1);

    let again = s.apply(&config, &opts());
    assert_eq!(
        outcomes(&again),
        pairs(&[("data", "unchanged"), ("backups", "unchanged"), ("pics", "unchanged")])
    );
    assert!(s.sys.calls.borrow().is_empty(), "nothing needed mounting");
}

#[test]
fn a_device_with_only_its_devmount() {
    let s = Setup::new();
    let data = s.mount_data(&[]);
    let report = s.apply(&s.config("[[device]]\nuuid = 'UUID-DATA'\ndevmount = 'gaming'"), &opts());
    assert_eq!(outcomes(&report), pairs(&[("gaming", "created")]));
    assert_eq!(read_link(&s.link("gaming")), data);
}

#[test]
fn a_mount_can_alias_the_device_root() {
    let s = Setup::new();
    s.mount_data(&[]);
    let report = s.apply(&s.config(&s.device("label = 'DATA'", "data", "again = '.'")), &opts());
    assert_eq!(outcomes(&report), pairs(&[("data", "created"), ("again", "created")]));
    assert_eq!(read_link(&s.link("again")), s.link("data"));
}

#[test]
fn moving_the_device_only_repoints_the_devmount() {
    let s = Setup::new();
    let old = s.mount_data(&["a"]);
    let config = s.config(&s.device("label = 'DATA'", "data", "a = 'a'"));
    s.apply(&config, &opts());

    // Remounted elsewhere; the old mount directory stays behind, empty.
    s.unmount(&old);
    fs::remove_dir(old.join("a")).unwrap();
    let new = s.mount("sdb1", "DATA1", &["a"]);

    let report = s.apply(&config, &opts());
    assert_eq!(outcomes(&report), pairs(&[("data", "updated"), ("a", "unchanged")]));
    assert_eq!(read_link(&s.link("data")), new);
    assert_eq!(report.devices[0].devmount.current, Some(old));
    assert!(s.link("a").join(".").is_dir());
}

#[test]
fn swapping_devices_only_repoints_the_devmount() {
    let s = Setup::new();
    s.mount_data(&["music"]);
    let other = s.mount("sdz1", "OTHER", &["music"]);
    s.apply(&s.config(&s.device("label = 'DATA'", "x", "music = 'music'")), &opts());

    // Same layout on another drive (both still connected).
    let report = s.apply(&s.config(&s.device("label = 'OTHER'", "x", "music = 'music'")), &opts());
    assert_eq!(outcomes(&report), pairs(&[("x", "updated"), ("music", "unchanged")]));
    assert_eq!(fs::canonicalize(s.link("music")).unwrap(), other.join("music"));
}

#[test]
fn mounts_devices_that_are_not_mounted() {
    let s = Setup::new();
    // The fake mount puts sdb1 where the `mount` method would: mnt/<its uuid>.
    mkdir(&s.mnt, "uuid-data/backups");
    let config = s.config("[[device]]\nlabel = 'DATA'\ndevmount = 'data'\nmount_options = ['ro']\nmounts = { b = 'backups' }");
    let report = s.apply(&config, &opts());
    assert_eq!(*s.sys.calls.borrow(), ["mount sdb1 [ro] top_level=false"]);
    assert_eq!(report.devices[0].mount_changes, [MountChange::Mounted { top_level: false }]);
    assert_eq!(outcomes(&report), pairs(&[("data", "created"), ("b", "created")]));
    assert_eq!(read_link(&s.link("data")), s.mnt.join("uuid-data"));
}

#[test]
fn automount_can_be_disabled() {
    let s = Setup::new();
    let config = s.config("[[device]]\nlabel = 'DATA'\ndevmount = 'data'\nautomount = false\nmounts = { b = 'backups' }");
    let report = s.apply(&config, &opts());
    assert_eq!(outcomes(&report), pairs(&[("data", "failed:not_mounted"), ("b", "skipped")]));

    let config = s.config(&s.device("label = 'DATA'", "data", "b = 'backups'"));
    let report = s.apply(&config, &Options { no_mount: true, ..opts() });
    assert_eq!(outcomes(&report), pairs(&[("data", "failed:not_mounted"), ("b", "skipped")]));
    assert!(s.sys.calls.borrow().is_empty());
    assert!(fs::read_dir(&s.root).unwrap().next().is_none());
}

#[test]
fn mount_failures_are_reported_per_device() {
    let mut s = Setup::new();
    s.sys.fail_mounts = true;
    let report = s.apply(&s.config(&s.device("label = 'DATA'", "data", "b = 'backups'")), &opts());
    assert_eq!(report.devices[0].error.as_ref().map(Error::kind), Some("mount_failed"));
    assert_eq!(outcomes(&report), pairs(&[("data", "failed:not_mounted"), ("b", "skipped")]));
    assert!(!report.ok());
}

#[test]
fn subvolume_only_devices_get_a_top_level_mount() {
    let s = Setup::new();
    // Only the @data subvolume is mounted.
    let data = mkdir(&s.media, "pool-data");
    s.sys.add_mount("sdc1", "/@data", &data);
    // What the fake top-level mount will show.
    mkdir(&s.mnt, "uuid-pool/@data/x");

    let report = s.apply(&s.config(&s.device("label = 'POOL'", "pool", "x = '@data/x'")), &opts());
    assert_eq!(*s.sys.calls.borrow(), ["mount sdc1 [] top_level=true"]);
    assert_eq!(outcomes(&report), pairs(&[("pool", "created"), ("x", "created")]));
    assert_eq!(read_link(&s.link("pool")), s.mnt.join("uuid-pool"));
    assert_eq!(read_link(&s.link("x")), s.root.join("pool/@data/x"));
}

#[test]
fn partially_mounted_devices_without_automount() {
    let s = Setup::new();
    let data = mkdir(&s.media, "pool-data");
    s.sys.add_mount("sdc1", "/@data", &data);
    let config = s.config("[[device]]\nlabel = 'POOL'\ndevmount = 'pool'\nautomount = false");
    let report = s.apply(&config, &opts());
    match &report.devices[0].devmount.outcome {
        LinkOutcome::Failed { error: Error::PathNotExposed { exposed, .. } } => {
            assert_eq!(exposed, &[format!("/@data at {}", data.display())]);
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn old_style_and_stale_links_are_updated() {
    let s = Setup::new();
    let data = s.mount_data(&["a", "b"]);
    // Pointing straight at the real path (what a hand-made link would do).
    symlink(data.join("a"), s.link("x")).unwrap();
    // Into the device, but at the wrong directory; and relative.
    symlink(data.join("b"), s.link("y")).unwrap();
    symlink("../media/DATA/b", s.link("z")).unwrap();
    // Dangling: the device used to be mounted somewhere else.
    symlink(s.media.join("OLD/a"), s.link("w")).unwrap();

    let config =
        s.config(&s.device("label = 'DATA'", "data", "x = 'a', y = 'a', z = 'a', w = 'a'"));
    let report = s.apply(&config, &opts());
    assert_eq!(
        outcomes(&report),
        pairs(&[
            ("data", "created"),
            ("x", "updated"),
            ("y", "updated"),
            ("z", "updated"),
            ("w", "updated")
        ])
    );
    for name in ["x", "y", "z", "w"] {
        assert_eq!(read_link(&s.link(name)), s.root.join("data/a"), "{name}");
    }
    assert_eq!(report.devices[0].mounts[0].current, Some(data.join("a")));
}

#[test]
fn links_to_other_things_need_force() {
    let s = Setup::new();
    let data = s.mount_data(&["a"]);
    let elsewhere = mkdir(&s.base, "elsewhere");
    symlink(&elsewhere, s.link("x")).unwrap();
    let config = s.config(&s.device("label = 'DATA'", "data", "x = 'a'"));

    let report = s.apply(&config, &opts());
    assert_eq!(outcomes(&report), pairs(&[("data", "created"), ("x", "failed:link_conflict")]));
    assert_eq!(read_link(&s.link("x")), elsewhere);

    let report = s.apply(&config, &Options { force: true, ..opts() });
    assert_eq!(outcomes(&report), pairs(&[("data", "unchanged"), ("x", "updated")]));
    assert_eq!(fs::canonicalize(s.link("x")).unwrap(), data.join("a"));
}

#[test]
fn a_devmount_pointing_elsewhere_holds_back_its_mounts() {
    let s = Setup::new();
    s.mount_data(&["a"]);
    let elsewhere = mkdir(&s.base, "elsewhere");
    symlink(&elsewhere, s.link("data")).unwrap();
    let report = s.apply(&s.config(&s.device("label = 'DATA'", "data", "a = 'a'")), &opts());
    assert_eq!(outcomes(&report), pairs(&[("data", "failed:link_conflict"), ("a", "skipped")]));
    assert!(!s.link("a").is_symlink(), "a link through /data would lead into the wrong place");
}

#[test]
fn non_symlinks_are_never_touched() {
    let s = Setup::new();
    s.mount_data(&["a"]);
    mkdir(&s.root, "dir");
    fs::write(s.link("file"), "keep me").unwrap();
    for force in [false, true] {
        let options = Options { force, ..opts() };
        let report =
            s.apply(&s.config(&s.device("label = 'DATA'", "data", "file = 'a'")), &options);
        assert_eq!(outcome(&report.devices[0].mounts[0]), "failed:link_blocked");
        let report = s.apply(&s.config(&s.device("label = 'DATA'", "dir", "m = 'a'")), &options);
        assert_eq!(outcomes(&report), pairs(&[("dir", "failed:link_blocked"), ("m", "skipped")]));
    }
    assert!(s.link("dir").is_dir() && !s.link("dir").is_symlink());
    assert_eq!(fs::read_to_string(s.link("file")).unwrap(), "keep me");
}

#[test]
fn bad_targets() {
    let s = Setup::new();
    let data = s.mount_data(&["real"]);
    let outside = mkdir(&s.base, "outside");
    fs::write(data.join("file.txt"), "").unwrap();
    symlink(&outside, data.join("escape")).unwrap();
    symlink(data.join("real"), data.join("alias")).unwrap();

    let config = s.config(&s.device(
        "label = 'DATA'",
        "data",
        "missing = 'nope', file = 'file.txt', through_file = 'file.txt/x', escape = 'escape', alias = 'alias'",
    ));
    let report = s.apply(&config, &opts());
    assert_eq!(
        outcomes(&report),
        pairs(&[
            ("data", "created"),
            ("missing", "failed:target_missing"),
            ("file", "failed:target_not_directory"),
            ("through_file", "failed:target_missing"),
            ("escape", "failed:target_escapes_device"),
            ("alias", "created"),
        ])
    );
    // Links still go through the devmount; symlinks inside the device do the rest.
    assert_eq!(read_link(&s.link("alias")), s.root.join("data/alias"));
    assert!(!s.link("escape").is_symlink());
}

#[test]
fn paths_covered_by_another_mount() {
    let s = Setup::new();
    let data = s.mount_data(&["sub"]);
    s.sys.add_mount("sdz1", "/", &data.join("sub"));
    let report = s.apply(&s.config(&s.device("label = 'DATA'", "data", "sub = 'sub'")), &opts());
    assert_eq!(outcomes(&report), pairs(&[("data", "created"), ("sub", "failed:target_covered")]));
}

#[test]
fn over_mounted_devices_count_as_unmounted() {
    let s = Setup::new();
    let data = s.mount_data(&["a"]);
    s.sys.add_mount("sdz1", "/", &data);
    let config = s.config("[[device]]\nlabel = 'DATA'\ndevmount = 'data'\nautomount = false");
    let report = s.apply(&config, &opts());
    assert_eq!(outcomes(&report), pairs(&[("data", "failed:not_mounted")]));
}

#[test]
fn dry_run_changes_nothing() {
    let s = Setup::new();
    let config = s.config(&s.device("label = 'DATA'", "data", "a = 'a'"));
    let options = Options { dry_run: true, ..opts() };

    let report = s.apply(&config, &options);
    assert!(report.dry_run);
    assert_eq!(report.devices[0].mount_changes, [MountChange::WouldMount { top_level: false }]);
    assert_eq!(outcomes(&report), pairs(&[("data", "would_create"), ("a", "would_create")]));
    assert_eq!(report.devices[0].devmount.target, None, "not known before mounting");
    assert_eq!(report.devices[0].mounts[0].target, Some(s.root.join("data/a")));
    assert!(s.sys.calls.borrow().is_empty());
    assert!(fs::read_dir(&s.root).unwrap().next().is_none());

    // Mounted, with a dangling devmount to fix.
    let data = s.mount_data(&["a"]);
    symlink(s.media.join("gone"), s.link("data")).unwrap();
    let report = s.apply(&config, &options);
    assert_eq!(outcomes(&report), pairs(&[("data", "would_update"), ("a", "would_create")]));
    assert_eq!(report.devices[0].devmount.target, Some(data));
    assert_eq!(read_link(&s.link("data")), s.media.join("gone"));
}

#[test]
fn status_reports_without_changing_anything() {
    let s = Setup::new();
    let data = s.mount_data(&["a", "b", "c"]);
    symlink(&data, s.link("data")).unwrap();
    symlink(s.root.join("data/a"), s.link("a")).unwrap();
    symlink(data.join("c"), s.link("c")).unwrap(); // old style
    let config = s.config(&format!(
        "{}{}",
        s.device("label = 'DATA'", "data", "a = 'a', b = 'b', c = 'c', d = 'nope'"),
        s.device("label = 'MISSING'", "gone", "m = 'x'"),
    ));
    let report = devmounts::status(&config, &opts(), &s.sys).unwrap();
    assert_eq!(
        outcomes(&report),
        pairs(&[
            ("data", "unchanged"),
            ("a", "unchanged"),
            ("b", "missing"),
            ("c", "outdated"),
            ("d", "failed:target_missing"),
            ("gone", "missing"),
            ("m", "missing"),
        ])
    );
    assert!(!report.ok());
    assert!(report.devices[1].not_connected);
    assert_eq!(
        report.devices[1].error.as_ref().map(Error::kind),
        Some("device_not_found"),
        "a problem for status"
    );
    assert!(!s.link("b").exists(), "status doesn't create links");
    assert_eq!(read_link(&s.link("c")), data.join("c"));
}

#[test]
fn remove_takes_down_only_our_links() {
    let s = Setup::new();
    let data = s.mount_data(&["a", "b"]);
    let elsewhere = mkdir(&s.base, "elsewhere");
    symlink(&data, s.link("data")).unwrap();
    symlink(s.root.join("data/a"), s.link("a")).unwrap();
    symlink(s.media.join("gone/b"), s.link("b")).unwrap();
    symlink(&elsewhere, s.link("c")).unwrap();
    mkdir(&s.root, "d");
    let config = s.config(&s.device(
        "label = 'DATA'",
        "data",
        "a = 'a', b = 'b', c = 'a', d = 'a', e = 'a'",
    ));

    let report = devmounts::remove(&config, &opts(), &s.sys).unwrap();
    assert_eq!(
        outcomes(&report),
        pairs(&[
            ("data", "removed"),
            ("a", "removed"),
            ("b", "removed"),
            ("c", "failed:link_conflict"),
            ("d", "failed:link_blocked"),
            ("e", "missing"),
        ])
    );
    assert!(!s.link("data").is_symlink() && !s.link("a").is_symlink() && !s.link("b").is_symlink());
    assert!(s.link("c").is_symlink());
    assert!(data.join("a").is_dir(), "targets are never removed");

    let report = devmounts::remove(&config, &Options { force: true, ..opts() }, &s.sys).unwrap();
    assert_eq!(outcome(&report.devices[0].mounts[2]), "removed");
    assert!(elsewhere.is_dir());
}

#[test]
fn remove_works_while_the_device_is_unplugged() {
    let s = Setup::new();
    symlink(s.media.join("DATA"), s.link("data")).unwrap();
    symlink(s.root.join("data/a"), s.link("a")).unwrap();
    let config = s.config(&s.device("label = 'MISSING'", "data", "a = 'a'"));
    let report = devmounts::remove(&config, &opts(), &s.sys).unwrap();
    assert_eq!(outcomes(&report), pairs(&[("data", "removed"), ("a", "removed")]));
    let device = &report.devices[0];
    assert!(
        device.not_connected && device.error.is_none(),
        "not connected is no failure for remove"
    );
    assert!(report.ok());
    let text = report.render(Colors::Plain);
    assert!(text.contains("• not connected — its links are taken down all the same"), "{text}");
    assert!(text.ends_with("✓ every link is down\n"), "{text}");
}

#[test]
fn remove_and_unmount() {
    let s = Setup::new();
    let data = s.mount_data(&["a"]);
    // Ours: where the `mount` method mounts this very device.
    let own = mkdir(&s.mnt, "uuid-data");
    s.sys.add_mount("sdb1", "/", &own);
    // Not ours, though one sits in mount_base: an fstab mount, and one somewhere else.
    let fstab = mkdir(&s.mnt, "data");
    s.sys.add_mount("sdb1", "/", &fstab);
    let fixed = mkdir(&s.base, "fixed/DATA");
    s.sys.add_mount("sdb1", "/", &fixed);
    symlink(&data, s.link("data")).unwrap();
    let config = s.config(&s.device("label = 'DATA'", "data", ""));

    let dry =
        devmounts::remove(&config, &Options { unmount: true, dry_run: true, ..opts() }, &s.sys)
            .unwrap();
    assert!(
        dry.devices[0]
            .mount_changes
            .contains(&MountChange::WouldUnmount { mount_point: data.clone() })
    );
    assert!(s.sys.calls.borrow().is_empty());

    let report = devmounts::remove(&config, &Options { unmount: true, ..opts() }, &s.sys).unwrap();
    let mut unmounted = s.sys.calls.borrow().clone();
    unmounted.sort();
    let mut expected =
        [format!("unmount {}", data.display()), format!("unmount {}", own.display())];
    expected.sort();
    assert_eq!(unmounted, expected, "the automounter's mount and devmounts' own");
    let changes = &report.devices[0].mount_changes;
    assert!(changes.contains(&MountChange::Unmounted { mount_point: data.clone() }));
    assert!(changes.contains(&MountChange::Unmounted { mount_point: own.clone() }));
    assert!(changes.contains(&MountChange::LeftMounted { mount_point: fstab.clone() }));
    assert!(changes.contains(&MountChange::LeftMounted { mount_point: fixed.clone() }));
    assert_eq!(report.devices[0].mounted_at.len(), 2, "the fstab mount and the fixed one are left");
}

/// A machine whose mount table can't be read at all can't be run against: every command stops
/// before anything is changed, as for a config that doesn't parse.
#[test]
fn a_mount_table_unreadable_from_the_start_stops_the_run_before_anything_changes() {
    let s = Setup::new();
    s.mount_data(&["a"]);
    let config = s.config(&s.device("label = 'DATA'", "data", "a = 'a'"));
    for run in [devmounts::apply, devmounts::status, devmounts::remove] {
        s.sys.mount_table_reads.set(Some(0));
        let result = run(&config, &Options { unmount: true, ..opts() }, &s.sys);
        assert!(matches!(result, Err(Error::MountTable { .. })), "{result:?}");
    }
    assert!(fs::read_dir(&s.root).unwrap().next().is_none(), "nothing was linked");
    assert!(s.sys.calls.borrow().is_empty(), "nothing was mounted or unmounted");
}

/// The mount table becoming unreadable partway through a run is the problem of each device from
/// there on, and of nothing before: the report keeps every change already made. The table is read
/// once before the run and once per device, so two reads take the run through its first device.
#[test]
fn a_mount_table_unreadable_partway_fails_only_the_devices_from_there_on() {
    let s = Setup::new();
    s.mount_data(&["a"]);
    s.mount("sdz1", "OTHER", &["b"]);
    let config = s.config(&format!(
        "{}{}",
        s.device("label = 'DATA'", "data", "a = 'a'"),
        s.device("label = 'OTHER'", "other", "b = 'b'")
    ));
    let unreadable = |device: &devmounts::DeviceReport| {
        device.error.as_ref().map(Error::kind) == Some("mount_table")
    };

    s.sys.mount_table_reads.set(Some(2));
    let applied = s.apply(&config, &opts());
    assert_eq!(
        outcomes(&applied),
        pairs(&[("data", "created"), ("a", "created"), ("other", "skipped"), ("b", "skipped")])
    );
    assert!(unreadable(&applied.devices[1]) && applied.problems() == 1, "{applied:?}");
    assert!(!s.link("other").exists());

    s.sys.mount_table_reads.set(None);
    assert!(s.apply(&config, &opts()).ok(), "readable again, the rest is linked");
    s.sys.mount_table_reads.set(Some(2));
    let status = devmounts::status(&config, &opts(), &s.sys).unwrap();
    assert_eq!(
        outcomes(&status),
        pairs(&[("data", "unchanged"), ("a", "unchanged"), ("other", "skipped"), ("b", "skipped")])
    );
    assert!(unreadable(&status.devices[1]) && status.problems() == 1, "{status:?}");

    s.sys.mount_table_reads.set(Some(2));
    let removed = devmounts::remove(&config, &opts(), &s.sys).unwrap();
    assert_eq!(
        outcomes(&removed),
        pairs(&[("data", "removed"), ("a", "removed"), ("other", "skipped"), ("b", "skipped")])
    );
    assert!(unreadable(&removed.devices[1]) && removed.problems() == 1, "{removed:?}");
    assert!(
        s.link("other").is_symlink() && !s.link("data").exists(),
        "only the first device's links came down"
    );
}

/// A read failing right after devmounts mounted a device leaves the mount in the report: it
/// happened, even though where is no longer known and the links can't be set.
#[test]
fn a_mount_table_unreadable_right_after_mounting_still_reports_the_mount() {
    let s = Setup::new();
    let config = s.config(&s.device("label = 'DATA'", "data", "a = 'a'"));
    s.sys.mount_table_reads.set(Some(2)); // before the run and the device's own; not the next
    let report = s.apply(&config, &opts());
    let device = &report.devices[0];
    assert_eq!(device.mount_changes, [MountChange::Mounted { top_level: false }]);
    assert_eq!(device.error.as_ref().map(Error::kind), Some("mount_table"));
    assert_eq!(outcomes(&report), pairs(&[("data", "skipped"), ("a", "skipped")]));
    assert_eq!(*s.sys.calls.borrow(), ["mount sdb1 [] top_level=false"]);
    assert!(report.render(Colors::Plain).contains("mounted it"));
}

/// Likewise after unmounting: each unmount is in the report, and where the device is still
/// mounted is left unsaid rather than guessed.
#[test]
fn a_mount_table_unreadable_after_unmounting_keeps_every_unmount_in_the_report() {
    let s = Setup::new();
    let data = s.mount_data(&[]);
    let config = s.config(&s.device("label = 'DATA'", "data", ""));
    s.sys.mount_table_reads.set(Some(2));
    let report = devmounts::remove(&config, &Options { unmount: true, ..opts() }, &s.sys).unwrap();
    let device = &report.devices[0];
    assert_eq!(device.mount_changes, [MountChange::Unmounted { mount_point: data }]);
    assert_eq!(device.error.as_ref().map(Error::kind), Some("mount_table"));
    assert!(device.mounted_at.is_empty());
}

/// Each mount's unmount is an outcome of its own: two failing are both reported — neither hides
/// the other or stops it being tried — and each counts as a problem.
#[test]
fn every_failed_unmount_is_reported_and_the_rest_still_tried() {
    let mut s = Setup::new();
    let data = s.mount_data(&[]);
    let own = mkdir(&s.mnt, "uuid-data");
    s.sys.add_mount("sdb1", "/", &own);
    s.sys.fail_unmounts = true;
    let config = s.config(&s.device("label = 'DATA'", "data", ""));
    let report = devmounts::remove(&config, &Options { unmount: true, ..opts() }, &s.sys).unwrap();

    let mut failed: Vec<PathBuf> = report.devices[0]
        .mount_changes
        .iter()
        .filter_map(|change| match change {
            MountChange::UnmountFailed { mount_point, error } => {
                assert_eq!(error.kind(), "unmount_failed");
                Some(mount_point.clone())
            }
            _ => None,
        })
        .collect();
    failed.sort();
    let mut expected = vec![data, own];
    expected.sort();
    assert_eq!(failed, expected);
    assert_eq!(s.sys.calls.borrow().len(), 2, "both were tried");
    assert!(report.devices[0].error.is_none(), "no device-level problem");
    assert_eq!(report.problems(), 2);
    let text = report.render(Colors::Plain);
    assert_eq!(text.matches("✗ unmounting ").count(), 2, "{text}");
    assert!(text.ends_with("✗ 2 problems — see the ✗ lines above\n"), "{text}");
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["devices"][0]["mount_changes"][0]["change"], "unmount_failed");
    assert_eq!(json["devices"][0]["mount_changes"][0]["error"]["kind"], "unmount_failed");
}

/// A locked LUKS container, `sdd1`, holding the ext4 filesystem `INSIDE` with `docs/` in it.
/// Returns where the `mount` method will mount that filesystem — made ready, holding `docs/`.
fn add_vault(s: &Setup) -> PathBuf {
    let container = device("sdd1", 8, 49, "crypto_LUKS", "uuid-vault", None);
    s.sys.add_locked(container, device("dm-0", 253, 0, "ext4", "uuid-inside", Some("INSIDE")));
    let inside = s.mnt.join("uuid-inside");
    mkdir(&inside, "docs");
    inside
}

/// The vault by its container's UUID — what can be seen of it while it's locked.
fn vault_config(s: &Setup, mounts: &str) -> Config {
    s.config(&s.device("uuid = 'uuid-vault'", "vault", mounts))
}

/// A device in a locked LUKS container — its filesystem out of sight until the container is
/// open — is reached by unlocking on the way, then mounted and linked as any other.
#[test]
fn a_locked_luks_container_is_unlocked_on_the_way_to_its_device() {
    let s = Setup::new();
    let inside = add_vault(&s);
    let report = s.apply(&vault_config(&s, "docs = 'docs'"), &opts());
    assert_eq!(*s.sys.calls.borrow(), ["unlock sdd1", "mount dm-0 [] top_level=false"]);
    let device = &report.devices[0];
    let unlocked = MountChange::Unlocked { container: "/dev/sdd1".into() };
    assert_eq!(device.mount_changes, [unlocked, MountChange::Mounted { top_level: false }]);
    assert_eq!(device.containers, ["/dev/sdd1 (crypto_LUKS)"]);
    assert_eq!(outcomes(&report), pairs(&[("vault", "created"), ("docs", "created")]));
    assert_eq!(read_link(&s.link("vault")), inside);
    assert!(report.ok(), "{}", report.render(Colors::Plain));
    assert!(report.render(Colors::Plain).contains("  unlocked /dev/sdd1\n  mounted it\n"));
    let json = serde_json::to_value(&report).unwrap();
    let change = &json["devices"][0]["mount_changes"][0];
    assert_eq!(*change, serde_json::json!({"change": "unlocked", "container": "/dev/sdd1"}));
}

/// A dry run unlocks nothing: it says it would, and the links are what they'd be once the
/// device in the container is mounted.
#[test]
fn a_dry_run_says_it_would_unlock() {
    let s = Setup::new();
    add_vault(&s);
    let report = s.apply(&vault_config(&s, "docs = 'docs'"), &Options { dry_run: true, ..opts() });
    assert!(s.sys.calls.borrow().is_empty());
    let would = MountChange::WouldUnlock { container: "/dev/sdd1".into() };
    assert_eq!(report.devices[0].mount_changes, [would]);
    assert_eq!(outcomes(&report), pairs(&[("vault", "would_create"), ("docs", "would_create")]));
    assert_eq!(report.devices[0].devmount.target, None, "not known before unlocking");
    assert!(report.ok());
    assert!(fs::read_dir(&s.root).unwrap().next().is_none(), "nothing linked");
}

/// Where the device may not be mounted — `automount = false`, or `--no-mount` — its container
/// isn't unlocked either: the device's problem, saying how to unlock it.
#[test]
fn a_locked_container_stays_locked_where_mounting_is_off() {
    let s = Setup::new();
    add_vault(&s);
    let off = s.config("[[device]]\nuuid = 'uuid-vault'\ndevmount = 'vault'\nautomount = false\n");
    let refused = [
        s.apply(&off, &opts()),
        s.apply(&vault_config(&s, ""), &Options { no_mount: true, ..opts() }),
    ];
    for report in refused {
        let error = report.devices[0].error.as_ref().expect("a problem");
        assert_eq!(error.kind(), "inactive_container");
        assert!(error.to_string().contains("udisksctl unlock -b /dev/sdd1"), "{error}");
    }
    assert!(s.sys.calls.borrow().is_empty());
}

/// An unlock that fails — a wrong passphrase, a refusal — is the device's problem, and nothing
/// more is tried for it.
#[test]
fn a_failed_unlock_is_the_devices_problem() {
    let mut s = Setup::new();
    add_vault(&s);
    s.sys.fail_unlocks = true;
    let report = s.apply(&vault_config(&s, "docs = 'docs'"), &opts());
    assert_eq!(report.devices[0].error.as_ref().map(Error::kind), Some("unlock_failed"));
    assert_eq!(*s.sys.calls.borrow(), ["unlock sdd1"]);
    assert!(report.devices[0].mount_changes.is_empty());
    assert_eq!(outcomes(&report), pairs(&[("vault", "skipped"), ("docs", "skipped")]));
}

/// Only a LUKS container is unlocked: any other that isn't active (an LVM volume group, a RAID
/// array) stays the device's problem, with its own hint.
#[test]
fn only_luks_containers_are_unlocked() {
    let s = Setup::new();
    s.sys.devices.borrow_mut().push(device("sde1", 8, 65, "LVM2_member", "uuid-pv", None));
    let report = s.apply(&s.config(&s.device("uuid = 'uuid-pv'", "pv", "")), &opts());
    let error = report.devices[0].error.as_ref().expect("a problem");
    assert_eq!(error.kind(), "inactive_container");
    assert!(error.to_string().contains("vgchange -ay"), "{error}");
    assert!(s.sys.calls.borrow().is_empty());
}

/// `remove --unmount` locks the container again once the last mount of the device in it is
/// down — the drive is then safe to unplug — leaving it as locked as it was found.
#[test]
fn remove_unmount_locks_the_container_once_its_last_mount_is_down() {
    let s = Setup::new();
    let inside = add_vault(&s);
    let config = vault_config(&s, "docs = 'docs'");
    assert!(s.apply(&config, &opts()).ok());
    s.sys.calls.borrow_mut().clear();
    let report = devmounts::remove(&config, &Options { unmount: true, ..opts() }, &s.sys).unwrap();
    assert_eq!(
        *s.sys.calls.borrow(),
        [format!("unmount {}", inside.display()), "lock sdd1".to_owned()]
    );
    let locked = MountChange::Locked { container: "/dev/sdd1".into() };
    assert_eq!(
        report.devices[0].mount_changes,
        [MountChange::Unmounted { mount_point: inside }, locked]
    );
    assert!(report.ok(), "{}", report.render(Colors::Plain));
    let status = devmounts::status(&config, &opts(), &s.sys).unwrap();
    assert_eq!(status.devices[0].error.as_ref().map(Error::kind), Some("inactive_container"));
}

/// A GELI provider, declared: the partition `sdf1`, named `safe` — to the inventory the UFS it
/// was made for while it's detached — whose attaching opens `sdf1.eli`, holding `docs/`.
/// Returns where the `mount` method will mount that.
fn add_geli(s: &Setup) -> PathBuf {
    let provider = BlockDevice {
        partlabel: Some("safe".into()),
        uuid: None,
        ..device("sdf1", 8, 81, "ufs", "", None)
    };
    s.sys.add_locked(provider, device("sdf1.eli", 8, 82, "ufs", "uuid-safe", None));
    let inside = s.mnt.join("uuid-safe");
    mkdir(&inside, "docs");
    inside
}

/// A config for the `safe` partition, read as on FreeBSD — declaring it a GELI provider or not.
fn safe_config(s: &Setup, geli: bool) -> Config {
    let text = format!(
        "[settings]\nroot = {:?}\nmount_base = {:?}\n[[device]]\npartlabel = 'safe'\n\
         geli = {geli}\ndevmount = 'safe'\nmounts = {{ docs = 'docs' }}\n",
        s.root, s.mnt
    );
    Config::parse_for(&text, Platform::FreeBsd).unwrap()
}

/// A declared GELI provider — to the inventory the partition it is, while it's detached — is
/// attached on the way to its device, which is mounted and linked; `remove --unmount` detaches it
/// again once its last mount is down.
#[test]
fn a_declared_geli_provider_is_attached_on_the_way_and_detached_after() {
    let s = Setup::new();
    let inside = add_geli(&s);
    let config = safe_config(&s, true);
    let report = s.apply(&config, &opts());
    assert_eq!(*s.sys.calls.borrow(), ["unlock sdf1", "mount sdf1.eli [] top_level=false"]);
    assert_eq!(report.devices[0].containers, ["/dev/sdf1 (geli)"]);
    assert_eq!(read_link(&s.link("safe")), inside);
    assert!(report.ok(), "{}", report.render(Colors::Plain));
    s.sys.calls.borrow_mut().clear();
    let removed = devmounts::remove(&config, &Options { unmount: true, ..opts() }, &s.sys).unwrap();
    assert_eq!(
        *s.sys.calls.borrow(),
        [format!("unmount {}", inside.display()), "lock sdf1".to_owned()]
    );
    assert!(removed.ok(), "{}", removed.render(Colors::Plain));
}

/// Where it may not be mounted, a declared GELI provider stays detached: the device's problem,
/// saying how to attach it.
#[test]
fn a_declared_geli_provider_stays_detached_where_mounting_is_off() {
    let s = Setup::new();
    add_geli(&s);
    let report = devmounts::status(&safe_config(&s, true), &opts(), &s.sys).unwrap();
    let error = report.devices[0].error.as_ref().expect("a problem");
    assert_eq!(error.kind(), "inactive_container");
    assert!(error.to_string().contains("geli attach /dev/sdf1"), "{error}");
    assert!(s.sys.calls.borrow().is_empty());
}

/// Undeclared, the same provider is the partition it looks like — mounted as that, never
/// attached: what GEOM doesn't show, the config says.
#[test]
fn an_undeclared_geli_provider_is_the_partition_it_looks_like() {
    let s = Setup::new();
    add_geli(&s);
    s.apply(&safe_config(&s, false), &opts());
    assert_eq!(*s.sys.calls.borrow(), ["mount sdf1 [] top_level=false"]);
}

/// A locked APFS volume is unlocked in place on the way — the same device, APFS then — and
/// mounted and linked; nothing locks it after `remove --unmount`, as macOS does on unmount.
#[test]
fn a_locked_apfs_volume_is_unlocked_in_place() {
    let s = Setup::new();
    let volume = |fstype| device("sdg1", 8, 97, fstype, "uuid-apfs", Some("Backup"));
    s.sys.add_locked(volume("apfs_locked"), volume("apfs"));
    let inside = s.mnt.join("uuid-apfs");
    mkdir(&inside, "docs");
    let config = s.config(&s.device("label = 'Backup'", "backup", "docs = 'docs'"));
    let report = s.apply(&config, &opts());
    assert_eq!(*s.sys.calls.borrow(), ["unlock sdg1", "mount sdg1 [] top_level=false"]);
    let unlocked = MountChange::Unlocked { container: "/dev/sdg1".into() };
    assert_eq!(report.devices[0].mount_changes[0], unlocked);
    assert!(report.devices[0].containers.is_empty(), "it's the device itself");
    assert_eq!(read_link(&s.link("backup")), inside);
    s.sys.calls.borrow_mut().clear();
    devmounts::remove(&config, &Options { unmount: true, ..opts() }, &s.sys).unwrap();
    assert_eq!(*s.sys.calls.borrow(), [format!("unmount {}", inside.display())]);
}

/// A dry run says it would lock the container, as the unmounts it would make leave nothing.
#[test]
fn a_dry_run_says_it_would_lock() {
    let s = Setup::new();
    let inside = add_vault(&s);
    let config = vault_config(&s, "");
    assert!(s.apply(&config, &opts()).ok());
    s.sys.calls.borrow_mut().clear();
    let dry = Options { unmount: true, dry_run: true, ..opts() };
    let report = devmounts::remove(&config, &dry, &s.sys).unwrap();
    assert!(s.sys.calls.borrow().is_empty());
    let would = MountChange::WouldLock { container: "/dev/sdd1".into() };
    assert_eq!(
        report.devices[0].mount_changes,
        [MountChange::WouldUnmount { mount_point: inside }, would]
    );
}

/// While the device in it is still mounted somewhere devmounts doesn't unmount (fstab's), the
/// container stays open: left unlocked, with where it's still mounted — no problem, as a mount
/// left alone is none.
#[test]
fn a_container_whose_device_stays_mounted_is_left_unlocked() {
    let s = Setup::new();
    add_vault(&s);
    let config = vault_config(&s, "");
    assert!(s.apply(&config, &opts()).ok());
    let fixed = mkdir(&s.base, "fixed/INSIDE");
    s.sys.add_mount("dm-0", "/", &fixed);
    for dry_run in [true, false] {
        let options = Options { unmount: true, dry_run, ..opts() };
        let report = devmounts::remove(&config, &options, &s.sys).unwrap();
        let changes = &report.devices[0].mount_changes;
        let left = MountChange::LeftUnlocked {
            container: "/dev/sdd1".into(),
            mounted_at: vec![fixed.clone()],
        };
        assert!(changes.contains(&left), "dry run {dry_run}: {changes:?}");
        assert!(report.ok());
        let text = report.render(Colors::Plain);
        assert!(
            text.contains(&format!(
                "left /dev/sdd1 unlocked: still mounted at {}",
                fixed.display()
            )),
            "{text}"
        );
    }
    assert!(
        !s.sys.calls.borrow().iter().any(|call| call.starts_with("lock")),
        "{:?}",
        s.sys.calls.borrow()
    );
}

/// Containers nested in each other come down innermost first: an outer one can't close while one
/// it holds is open.
#[test]
fn nested_luks_containers_are_locked_innermost_first() {
    let s = Setup::new();
    let held = |mut d: devmounts::BlockDevice, by: &str| {
        d.holders = vec![by.to_owned()];
        d
    };
    s.sys.devices.borrow_mut().extend([
        held(device("sdd1", 8, 49, "crypto_LUKS", "uuid-outer", None), "dm-0"),
        held(device("dm-0", 253, 0, "crypto_LUKS", "uuid-inner", None), "dm-1"),
        device("dm-1", 253, 1, "ext4", "uuid-inside", Some("INSIDE")),
    ]);
    let config = s.config(&s.device("uuid = 'uuid-outer'", "vault", ""));
    let report = devmounts::remove(&config, &Options { unmount: true, ..opts() }, &s.sys).unwrap();
    assert_eq!(*s.sys.calls.borrow(), ["lock dm-0", "lock sdd1"]);
    let locked = |at: &str| MountChange::Locked { container: at.into() };
    assert_eq!(report.devices[0].mount_changes, [locked("/dev/dm-0"), locked("/dev/sdd1")]);
}

/// A lock that fails is a problem of its own, as a failed unmount is.
#[test]
fn a_failed_lock_is_a_problem() {
    let mut s = Setup::new();
    add_vault(&s);
    let config = vault_config(&s, "");
    assert!(s.apply(&config, &opts()).ok());
    s.sys.fail_locks = true;
    let report = devmounts::remove(&config, &Options { unmount: true, ..opts() }, &s.sys).unwrap();
    let failed = report.devices[0].mount_changes.last();
    assert!(
        matches!(failed, Some(MountChange::LockFailed { error, .. }) if error.kind() == "lock_failed"),
        "{failed:?}"
    );
    assert_eq!(report.problems(), 1);
    assert!(report.render(Colors::Plain).contains("✗ locking /dev/sdd1 failed: simulated"));
}

#[test]
fn one_failing_device_does_not_stop_the_others() {
    let s = Setup::new();
    s.mount_data(&["a"]);
    let config = s.config(&format!(
        "{}{}",
        s.device("label = 'MISSING'", "gone", "m = 'x'"),
        s.device("label = 'DATA'", "data", "d = 'a'")
    ));
    let report = s.apply(&config, &opts());
    assert_eq!(
        outcomes(&report),
        pairs(&[("gone", "skipped"), ("m", "skipped"), ("data", "created"), ("d", "created")])
    );
    assert!(!report.ok());
    assert!(report.devices[1].ok());
}

/// (Not on macOS, where `/` is refused as the link root altogether.)
#[cfg(not(target_os = "macos"))]
#[test]
fn reserved_names_are_checked_against_the_runtime_root() {
    let s = Setup::new();
    s.mount_data(&["a"]);
    // Valid under the configured root, dangerous in / (on Arch, /bin is a symlink into the
    // system disk, so `remove` would otherwise take it for one of ours). Every name here is
    // reserved, so the test can't touch anything in the real /.
    let config = s.config(&s.device("label = 'DATA'", "usr", "bin = 'a'"));
    let slash = Options { root: Some("/".into()), ..opts() };
    let applied = s.apply(&config, &slash);
    assert_eq!(outcomes(&applied), pairs(&[("usr", "failed:config_invalid"), ("bin", "skipped")]));
    for report in [
        devmounts::status(&config, &slash, &s.sys).unwrap(),
        devmounts::remove(&config, &slash, &s.sys).unwrap(),
    ] {
        assert_eq!(
            outcomes(&report),
            pairs(&[("usr", "failed:config_invalid"), ("bin", "failed:config_invalid")]),
            "{:?}",
            report.command
        );
    }
}

#[test]
fn root_overrides_must_be_absolute_and_free_of_dotdot() {
    let s = Setup::new();
    let config = s.config(&s.device("label = 'DATA'", "data", ""));
    for bad in ["links", "/tmp/.."] {
        let result =
            devmounts::apply(&config, &Options { root: Some(bad.into()), ..opts() }, &s.sys);
        assert!(matches!(result, Err(Error::ConfigInvalid { .. })), "{bad}");
    }
}

/// Where `/` can't hold links (macOS), it can't be the link root, given or configured — a config
/// made by a program rather than read has its default — and the run says why, and how names can
/// go in `/` all the same.
#[test]
fn a_root_the_system_refuses_stops_the_run() {
    let s = Setup::new();
    let config = s.config(&s.device("label = 'DATA'", "data", ""));
    let slash = Options { root: Some("/".into()), ..opts() };
    let made =
        Config { settings: devmounts::Settings::defaults_for(Platform::MacOs), ..config.clone() };
    for (config, options) in [(&config, &slash), (&made, &opts())] {
        match link_root(config, options, Platform::MacOs) {
            Err(Error::ConfigInvalid { problems }) => {
                assert_eq!(problems.len(), 1, "{problems:?}");
                assert!(
                    problems[0].starts_with("the link root can't be /: on macOS"),
                    "{problems:?}"
                );
                assert!(problems[0].contains("/etc/synthetic.conf"), "{problems:?}");
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(link_root(&config, &opts(), Platform::MacOs).unwrap(), s.root, "a root of its own");
    assert_eq!(link_root(&config, &slash, Platform::Linux).unwrap(), Path::new("/"));
    assert_eq!(link_root(&config, &slash, Platform::FreeBsd).unwrap(), Path::new("/"));
}

#[test]
fn permission_errors_without_escalation() {
    if devmounts::escalate::is_root() {
        return; // root ignores directory permissions
    }
    let s = Setup::new();
    s.mount_data(&[]);
    fs::set_permissions(&s.root, fs::Permissions::from_mode(0o555)).unwrap();
    let report = s.apply(&s.config(&s.device("label = 'DATA'", "data", "")), &opts());
    fs::set_permissions(&s.root, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(outcomes(&report), pairs(&[("data", "failed:io")]));
    assert!(report.render(Colors::Plain).contains("Permission denied"));
}

/// A stand-in for sudo at `<base>/<name>`: logs its arguments to `<base>/escalation.log`, then runs
/// them with the link root writable — read-only again once they're done.
#[cfg(feature = "cli")]
fn fake_sudo(s: &Setup, name: &str) -> PathBuf {
    let sudo = s.base.join(name);
    let script = format!(
        "#!/bin/sh\necho \"$*\" >> '{log}'\nchmod u+w '{root}' && \"$@\"\nrc=$?\nchmod u-w '{root}'\nexit $rc\n",
        log = s.base.join("escalation.log").display(),
        root = s.root.display()
    );
    fs::write(&sudo, script).unwrap();
    fs::set_permissions(&sudo, fs::Permissions::from_mode(0o755)).unwrap();
    sudo
}

fn escalation_log(s: &Setup) -> Vec<String> {
    fs::read_to_string(s.base.join("escalation.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Every privileged step of a run goes through one helper, so a run asks for a password at most
/// once: two links set in a read-only root, then both taken down again, start the stand-in for
/// sudo exactly once per run.
#[cfg(feature = "cli")]
#[test]
fn permission_errors_go_through_one_privileged_helper_per_run() {
    if devmounts::escalate::is_root() {
        return;
    }
    let s = Setup::new();
    let data = s.mount_data(&["a"]);
    let sudo = fake_sudo(&s, "fake-sudo");
    fs::set_permissions(&s.root, fs::Permissions::from_mode(0o555)).unwrap();
    let helper = PathBuf::from(env!("CARGO_BIN_EXE_devmounts"));
    let options = Options {
        escalation: Some(Escalation::command([sudo.display().to_string()]).unwrap()),
        helper: Some(helper.clone()),
        ..Options::default()
    };
    let config = s.config(&s.device("label = 'DATA'", "data", "a = 'a'"));

    let applied = s.apply(&config, &options);
    let created = (read_link(&s.link("data")), read_link(&s.link("a")));
    let removed = devmounts::remove(&config, &options, &s.sys).unwrap();
    fs::set_permissions(&s.root, fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(outcomes(&applied), pairs(&[("data", "created"), ("a", "created")]));
    assert_eq!(created, (data, s.link("data").join("a")));
    assert_eq!(outcomes(&removed), pairs(&[("data", "removed"), ("a", "removed")]));
    assert!(!s.link("data").is_symlink() && !s.link("a").is_symlink());
    let started = format!("{} --devmounts-privileged-helper", helper.display());
    assert_eq!(
        escalation_log(&s),
        [started.clone(), started],
        "one helper per run, however many steps"
    );
}

/// An escalation that's refused — a wrong password, say — is tried once. Every privileged step of
/// the run then fails, saying why, and nothing asks again.
#[test]
fn a_refused_escalation_is_tried_once_and_every_privileged_step_says_so() {
    if devmounts::escalate::is_root() {
        return;
    }
    let s = Setup::new();
    s.mount_data(&["a"]);
    s.mount("sdz1", "OTHER", &[]);
    let refusing = s.base.join("refusing-sudo");
    let script = format!(
        "#!/bin/sh\necho refused >> '{}'\necho 'sudo: 3 incorrect password attempts' >&2\nexit 1\n",
        s.base.join("escalation.log").display()
    );
    fs::write(&refusing, script).unwrap();
    fs::set_permissions(&refusing, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&s.root, fs::Permissions::from_mode(0o555)).unwrap();
    let options = Options {
        escalation: Some(Escalation::command([refusing.display().to_string()]).unwrap()),
        helper: Some(s.base.join("never-started")),
        ..Options::default()
    };
    let config = s.config(&format!(
        "{}{}",
        s.device("label = 'DATA'", "data", "a = 'a'"),
        s.device("label = 'OTHER'", "other", "")
    ));
    let report = s.apply(&config, &options);
    fs::set_permissions(&s.root, fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(
        outcomes(&report),
        pairs(&[("data", "failed:escalation"), ("a", "skipped"), ("other", "failed:escalation")])
    );
    assert_eq!(escalation_log(&s), ["refused"], "asked once for the whole run");
    let text = report.render(Colors::Plain);
    assert!(
        text.contains("can't get privileges: `") && text.contains("` ended without answering"),
        "{text}"
    );
}

#[test]
fn reports_render_and_serialize() {
    let s = Setup::new();
    s.mount_data(&["a"]);
    let config = s.config(&s.device("label = 'DATA'", "data", "a = 'a', b = 'nope'"));
    let report = s.apply(&config, &opts());
    let text = report.render(Colors::Plain);
    assert!(text.contains("LABEL=\"DATA\" → /dev/sdb1 (ext4 \"DATA\")"), "{text}");
    assert!(
        text.contains(&format!(
            "    ✓ {} → {}  created",
            s.link("a").display(),
            s.root.join("data/a").display()
        )),
        "{text}"
    );
    assert!(text.contains("doesn't exist"), "{text}");
    assert!(text.ends_with("✗ 1 problem — see the ✗ line above\n"), "{text}");
    assert_eq!(report.problems(), 1);
    assert!(!text.contains('\x1b'));
    assert!(report.render(Colors::Ansi).contains('\x1b'));

    let json = serde_json::to_value(&report).unwrap();
    let device = &json["devices"][0];
    assert_eq!(json["command"], "apply");
    assert_eq!(device["id"], serde_json::json!({"by": "label", "value": "DATA"}));
    assert_eq!(device["devmount"]["path"], ".");
    assert_eq!(device["devmount"]["outcome"]["outcome"], "created");
    assert_eq!(device["mounts"][1]["outcome"]["error"]["kind"], "target_missing");
}
