//! A fake machine for driving the engine: devices and the mount table are in memory, while
//! "mount points" and link roots are real directories under `target/tmp`. It mounts where the
//! real `mount` method would (`<mount_base>/<uuid>`), and its automounter's directory is a
//! directory of the test's own.

#![allow(dead_code)]

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};

use devmounts::{BlockDevice, DevNum, MountEntry, MountRequest, Result, System, UnmountRequest};
use tempfile::TempDir;

pub fn tmpdir() -> TempDir {
    tempfile::Builder::new().prefix("devmounts-").tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

/// A canonical path for a fresh directory under `base`.
pub fn mkdir(base: &Path, rel: &str) -> PathBuf {
    let dir = base.join(rel);
    fs::create_dir_all(&dir).unwrap();
    fs::canonicalize(dir).unwrap()
}

pub fn device(kname: &str, major: u32, minor: u32, fstype: &str, uuid: &str, label: Option<&str>) -> BlockDevice {
    BlockDevice {
        kname: kname.into(),
        devnode: PathBuf::from("/dev").join(kname),
        devnum: DevNum::new(major, minor),
        fstype: Some(fstype.into()),
        uuid: Some(uuid.into()),
        label: label.map(Into::into),
        partition: true,
        ..Default::default()
    }
}

pub struct FakeSystem {
    pub devices: Vec<BlockDevice>,
    pub mounts: RefCell<Vec<MountEntry>>,
    /// Where the machine's automounter mounts devices.
    pub automount_dir: PathBuf,
    /// Requests seen, as "mount sdb1 [ro] top_level=false" / "unmount <dir>".
    pub calls: RefCell<Vec<String>>,
    /// Make the next mounts fail.
    pub fail_mounts: bool,
    next_id: RefCell<u32>,
}

impl FakeSystem {
    pub fn new(devices: Vec<BlockDevice>, automount_dir: PathBuf) -> Self {
        let root = MountEntry {
            id: 1,
            parent_id: 0,
            devnum: DevNum::new(0, 1),
            root: "/".into(),
            mount_point: "/".into(),
            options: "rw".into(),
            fstype: "rootfs".into(),
            source: "none".into(),
            super_options: "rw".into(),
        };
        FakeSystem {
            devices,
            mounts: RefCell::new(vec![root]),
            automount_dir,
            calls: RefCell::new(Vec::new()),
            fail_mounts: false,
            next_id: RefCell::new(100),
        }
    }

    fn id(&self) -> u32 {
        let mut next = self.next_id.borrow_mut();
        *next += 1;
        *next
    }

    /// Mounts `root` (a path inside the device) of `kname` at `at`.
    pub fn add_mount(&self, kname: &str, root: &str, at: &Path) -> u32 {
        let dev = self.devices.iter().find(|d| d.kname == kname).expect("known device");
        let devnum = if dev.fstype.as_deref() == Some("btrfs") { DevNum::new(0, 500 + dev.devnum.minor) } else { dev.devnum };
        self.add_entry(devnum, root, at, dev.fstype.as_deref().unwrap_or("?"), &dev.devnode.display().to_string())
    }

    pub fn add_entry(&self, devnum: DevNum, root: &str, at: &Path, fstype: &str, source: &str) -> u32 {
        let id = self.id();
        // The parent is whatever the mount point currently lives in.
        let parent_id = {
            let mounts = self.mounts.borrow();
            let tree = devmounts::MountTree::new(&mounts);
            tree.mount_at(at).map(|m| m.id).unwrap_or(1)
        };
        self.mounts.borrow_mut().push(MountEntry {
            id,
            parent_id,
            devnum,
            root: root.into(),
            mount_point: at.to_path_buf(),
            options: "rw".into(),
            fstype: fstype.into(),
            source: source.into(),
            super_options: "rw".into(),
        });
        id
    }
}

impl System for FakeSystem {
    fn block_devices(&self) -> Result<Vec<BlockDevice>> {
        Ok(self.devices.clone())
    }

    fn mounts(&self) -> Result<Vec<MountEntry>> {
        Ok(self.mounts.borrow().clone())
    }

    fn device_number(&self, _path: &Path) -> Option<DevNum> {
        None
    }

    fn automount_dirs(&self) -> Vec<PathBuf> {
        vec![self.automount_dir.clone()]
    }

    fn mount(&self, request: &MountRequest<'_>) -> Result<()> {
        let dev = request.device;
        self.calls.borrow_mut().push(format!(
            "mount {} [{}] top_level={}",
            dev.kname,
            request.options.join(","),
            request.top_level
        ));
        if self.fail_mounts {
            return Err(devmounts::Error::MountFailed { device: dev.describe(), message: "simulated".into() });
        }
        let at = devmounts::mount::mount_dir(&request.mount_base, dev);
        fs::create_dir_all(&at).unwrap();
        self.add_mount(&dev.kname, "/", &at);
        Ok(())
    }

    fn unmount(&self, request: &UnmountRequest<'_>) -> Result<()> {
        self.calls.borrow_mut().push(format!("unmount {}", request.mount.mount_point.display()));
        self.mounts.borrow_mut().retain(|m| m.id != request.mount.id);
        Ok(())
    }
}
