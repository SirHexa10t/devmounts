//! `LinuxSystem` against fake sysfs / udev database / mountinfo trees.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{mkdir, tmpdir};
use devmounts::{Colors, Config, DevNum, Error, Escalation, LinkOutcome, LinuxSystem, Options, System};
use tempfile::TempDir;

struct Machine {
    _tmp: TempDir,
    base: PathBuf,
    system: LinuxSystem,
}

struct Dev<'a> {
    kname: &'a str,
    num: (u32, u32),
    devname: Option<&'a str>,
    partition: bool,
    holders: &'a [&'a str],
    dm_name: Option<&'a str>,
    udev: Option<&'a str>,
}

impl Default for Dev<'_> {
    fn default() -> Self {
        Dev { kname: "", num: (0, 0), devname: None, partition: true, holders: &[], dm_name: None, udev: None }
    }
}

impl Machine {
    fn new() -> Self {
        let tmp = tmpdir();
        let base = fs::canonicalize(tmp.path()).unwrap();
        let system = LinuxSystem {
            sysfs: mkdir(&base, "sys"),
            udev_data: mkdir(&base, "udev"),
            mountinfo: base.join("mountinfo"),
            dev: mkdir(&base, "dev"),
            automount_dirs: vec![mkdir(&base, "media")],
        };
        fs::write(&system.mountinfo, "1 0 0:1 / / rw - rootfs none rw\n").unwrap();
        Machine { _tmp: tmp, base, system }
    }

    fn add(&self, dev: Dev<'_>) {
        let dir = mkdir(&self.system.sysfs, &format!("class/block/{}", dev.kname));
        let (major, minor) = dev.num;
        fs::write(dir.join("dev"), format!("{major}:{minor}\n")).unwrap();
        let mut uevent = format!("MAJOR={major}\nMINOR={minor}\n");
        if let Some(name) = dev.devname {
            uevent.push_str(&format!("DEVNAME={name}\n"));
        }
        fs::write(dir.join("uevent"), uevent).unwrap();
        if dev.partition {
            fs::write(dir.join("partition"), "1\n").unwrap();
        }
        mkdir(&dir, "holders");
        for holder in dev.holders {
            fs::write(dir.join("holders").join(holder), "").unwrap();
        }
        if let Some(name) = dev.dm_name {
            mkdir(&dir, "dm");
            fs::write(dir.join("dm/name"), format!("{name}\n")).unwrap();
        }
        if let Some(props) = dev.udev {
            fs::write(self.system.udev_data.join(format!("b{major}:{minor}")), props).unwrap();
        }
    }

    fn mountinfo(&self, extra: &str) {
        fs::write(&self.system.mountinfo, format!("1 0 0:1 / / rw - rootfs none rw\n{extra}")).unwrap();
    }

    fn devnode(&self, name: &str) -> PathBuf {
        self.system.dev.join(name)
    }
}

const NVME_ROOT: &str = "E:ID_FS_TYPE=btrfs\nE:ID_FS_UUID=0a0a0a0a\nE:ID_FS_UUID_ENC=0a0a0a0a\nE:ID_PART_ENTRY_UUID=0b0b0b0b\nE:ID_PART_ENTRY_NAME=root\n";

#[test]
fn reads_all_kinds_of_devices() {
    let m = Machine::new();
    m.add(Dev { kname: "nvme0n1", num: (259, 0), devname: Some("nvme0n1"), partition: false, ..Dev::default() });
    m.add(Dev { kname: "nvme0n1p2", num: (259, 2), devname: Some("nvme0n1p2"), udev: Some(NVME_ROOT), ..Dev::default() });
    m.add(Dev {
        kname: "mmcblk0p1",
        num: (179, 1),
        devname: Some("mmcblk0p1"),
        udev: Some("E:ID_FS_TYPE=vfat\nE:ID_FS_UUID=1234-ABCD\nE:ID_FS_LABEL=My_Disk\nE:ID_FS_LABEL_ENC=My\\x20Disk\n"),
        ..Dev::default()
    });
    // A whole-disk filesystem ("superfloppy").
    m.add(Dev {
        kname: "sdb",
        num: (8, 16),
        devname: Some("sdb"),
        partition: false,
        udev: Some("E:ID_FS_TYPE=exfat\nE:ID_FS_LABEL=STICK\n"),
        ..Dev::default()
    });
    m.add(Dev {
        kname: "sdc1",
        num: (8, 33),
        devname: Some("sdc1"),
        holders: &["dm-0"],
        udev: Some("E:ID_FS_TYPE=crypto_LUKS\nE:ID_FS_UUID=outer\n"),
        ..Dev::default()
    });
    m.add(Dev {
        kname: "dm-0",
        num: (254, 0),
        devname: Some("dm-0"),
        partition: false,
        dm_name: Some("luks-outer"),
        udev: Some("E:ID_FS_TYPE=ext4\nE:ID_FS_UUID=inner\n"),
        ..Dev::default()
    });
    // sysfs writes '/' in names as '!'; DEVNAME has the real path.
    m.add(Dev { kname: "cciss!c0d0p1", num: (104, 1), devname: Some("cciss/c0d0p1"), ..Dev::default() });
    m.add(Dev { kname: "cciss!c0d1", num: (104, 16), partition: false, ..Dev::default() });

    let devices = m.system.block_devices().unwrap();
    let get = |k: &str| devices.iter().find(|d| d.kname == k).unwrap_or_else(|| panic!("{k} missing"));

    let root = get("nvme0n1p2");
    assert_eq!(root.devnode, m.devnode("nvme0n1p2"));
    assert_eq!(root.devnum, DevNum::new(259, 2));
    assert_eq!(root.fstype.as_deref(), Some("btrfs"));
    assert_eq!(root.partlabel.as_deref(), Some("root"));
    assert!(root.partition);
    assert!(!get("nvme0n1").partition);
    assert_eq!(get("nvme0n1").fstype, None);

    assert_eq!(get("mmcblk0p1").label.as_deref(), Some("My Disk"));
    assert!(!get("sdb").partition);
    assert_eq!(get("sdc1").holders, ["dm-0"]);
    assert_eq!(get("dm-0").dm_name.as_deref(), Some("luks-outer"));
    assert_eq!(get("cciss!c0d0p1").devnode, m.devnode("cciss/c0d0p1"));
    assert_eq!(get("cciss!c0d1").devnode, m.devnode("cciss/c0d1"));
}

#[test]
fn devices_that_vanish_are_skipped() {
    let m = Machine::new();
    m.add(Dev { kname: "sda1", num: (8, 1), ..Dev::default() });
    // A directory without a `dev` file: the device went away while we looked.
    mkdir(&m.system.sysfs, "class/block/ghost");
    let devices = m.system.block_devices().unwrap();
    assert_eq!(devices.iter().map(|d| d.kname.as_str()).collect::<Vec<_>>(), ["sda1"]);
    assert_eq!(devices[0].fstype, None, "no udev record means no identity");
}

#[test]
fn missing_sources_are_errors() {
    let m = Machine::new();
    let broken = LinuxSystem { sysfs: m.base.join("nope"), mountinfo: m.base.join("nope"), ..m.system.clone() };
    assert!(matches!(broken.block_devices(), Err(Error::Inventory { .. })));
    assert!(matches!(broken.mounts(), Err(Error::MountTable { .. })));
    fs::write(&m.system.mountinfo, "garbage\n").unwrap();
    assert!(matches!(m.system.mounts(), Err(Error::MountTable { .. })));
}

#[test]
fn device_numbers_only_for_block_devices() {
    let system = LinuxSystem::new();
    assert_eq!(system.device_number(Path::new("/dev/null")), None, "character device");
    assert_eq!(system.device_number(Path::new("/")), None);
    assert_eq!(system.device_number(Path::new("/no/such/path")), None);
}

#[test]
fn applies_through_the_linux_backend() {
    let m = Machine::new();
    m.add(Dev {
        kname: "sdb1",
        num: (8, 17),
        devname: Some("sdb1"),
        udev: Some("E:ID_FS_TYPE=ext4\nE:ID_FS_UUID=u1\nE:ID_FS_LABEL_ENC=My\\x20Backup\n"),
        ..Dev::default()
    });
    m.add(Dev { kname: "nvme0n1p2", num: (259, 2), devname: Some("nvme0n1p2"), udev: Some(NVME_ROOT), ..Dev::default() });

    let backup = mkdir(&m.base, "media/My Backup");
    mkdir(&backup, "docs");
    let home = mkdir(&m.base, "home");
    // The btrfs top level is mounted too, so the system disk has a root for its devmount.
    let top = mkdir(&m.base, "top");
    mkdir(&top, "@home/me/projects");
    // ext4 matches by device number; btrfs (anonymous 0:33) by its source path. Spaces in
    // mount points are escaped in mountinfo.
    m.mountinfo(&format!(
        "30 1 8:17 / {} rw,nosuid - ext4 {} rw\n61 1 0:33 /@home {} rw - btrfs {} rw\n62 1 0:33 / {} rw - btrfs {} rw\n",
        backup.display().to_string().replace(' ', "\\040"),
        m.devnode("sdb1").display(),
        home.display(),
        m.devnode("nvme0n1p2").display(),
        top.display(),
        m.devnode("nvme0n1p2").display(),
    ));

    let root = mkdir(&m.base, "links");
    let config: Config = format!(
        "settings.root = {root:?}\n\
         [[device]]\nlabel = 'My Backup'\ndevmount = 'backup'\nmounts = {{ docs = 'docs' }}\n\
         [[device]]\npartlabel = 'root'\ndevmount = 'system'\nmounts = {{ proj = '@home/me/projects' }}"
    )
    .parse()
    .unwrap();
    let options = Options { escalation: Some(Escalation::Never), ..Options::default() };
    let report = devmounts::apply(&config, &options, &m.system).unwrap();
    assert!(report.ok(), "{}", report.render(Colors::Plain));
    assert_eq!(fs::read_link(root.join("backup")).unwrap(), backup);
    assert_eq!(fs::read_link(root.join("docs")).unwrap(), root.join("backup/docs"));
    assert_eq!(fs::read_link(root.join("system")).unwrap(), top, "the whole filesystem, not a subvolume");
    assert_eq!(fs::read_link(root.join("proj")).unwrap(), root.join("system/@home/me/projects"));

    let listing = devmounts::list_devices(&m.system).unwrap();
    let nvme = listing.iter().find(|l| l.device.kname == "nvme0n1p2").unwrap();
    assert_eq!(nvme.mounts[0].root, Path::new("/@home"));
}

#[test]
fn not_mounted_and_not_allowed_to_mount() {
    let m = Machine::new();
    m.add(Dev { kname: "sdb1", num: (8, 17), udev: Some("E:ID_FS_TYPE=ext4\nE:ID_FS_LABEL=X\n"), ..Dev::default() });
    let config: Config =
        format!("settings.root = {:?}\n[[device]]\nlabel = 'X'\ndevmount = 'x'\nmounts = {{ y = 'y' }}", m.base)
            .parse()
            .unwrap();
    let options = Options { no_mount: true, escalation: Some(Escalation::Never), ..Options::default() };
    let report = devmounts::apply(&config, &options, &m.system).unwrap();
    assert!(matches!(report.devices[0].mounts[0].outcome, LinkOutcome::Skipped));
    match &report.devices[0].devmount.outcome {
        LinkOutcome::Failed { error: Error::NotMounted { .. } } => {}
        other => panic!("unexpected {other:?}"),
    }
}
