//! A fake machine for driving the engine: devices and the mount table are in memory, while
//! "mount points" and link roots are real directories under `target/tmp`. It mounts where the
//! real `mount` method would (`<mount_base>/<uuid>`), and its automounter's directory is a
//! directory of the test's own.

#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use devmounts::{
    BlockDevice, ContainerRequest, DevNum, MountEntry, MountRequest, Result, System, UnmountRequest,
};
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

pub fn device(
    kname: &str,
    major: u32,
    minor: u32,
    fstype: &str,
    uuid: &str,
    label: Option<&str>,
) -> BlockDevice {
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
    pub devices: RefCell<Vec<BlockDevice>>,
    /// Locked LUKS containers, by kernel name, with the device unlocking each one opens.
    locked: RefCell<HashMap<String, BlockDevice>>,
    pub mounts: RefCell<Vec<MountEntry>>,
    /// Where the machine's automounter mounts devices.
    pub automount_dir: PathBuf,
    /// Requests seen, as "mount sdb1 [ro] top_level=false" / "unmount <dir>".
    pub calls: RefCell<Vec<String>>,
    /// Make the next mounts fail.
    pub fail_mounts: bool,
    /// Make every unmount fail.
    pub fail_unmounts: bool,
    /// How many more reads of the mount table succeed before every one fails; `None`: all do.
    pub mount_table_reads: Cell<Option<usize>>,
    /// Make every unlock fail.
    pub fail_unlocks: bool,
    /// Make every lock fail.
    pub fail_locks: bool,
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
            devices: RefCell::new(devices),
            locked: RefCell::new(HashMap::new()),
            mounts: RefCell::new(vec![root]),
            automount_dir,
            calls: RefCell::new(Vec::new()),
            fail_mounts: false,
            fail_unmounts: false,
            mount_table_reads: Cell::new(None),
            fail_unlocks: false,
            fail_locks: false,
            next_id: RefCell::new(100),
        }
    }

    fn id(&self) -> u32 {
        let mut next = self.next_id.borrow_mut();
        *next += 1;
        *next
    }

    /// A locked container, `container`, whose unlocking opens `inside` — stacked on it, as
    /// dm-crypt's cleartext device or GELI's `.eli` is, or, given the container's own kernel
    /// name, the container itself unlocked in place, as an APFS volume is.
    pub fn add_locked(&self, container: BlockDevice, inside: BlockDevice) {
        self.locked.borrow_mut().insert(container.kname.clone(), inside);
        self.devices.borrow_mut().push(container);
    }

    /// Mounts `root` (a path inside the device) of `kname` at `at`.
    pub fn add_mount(&self, kname: &str, root: &str, at: &Path) -> u32 {
        let dev =
            self.devices.borrow().iter().find(|d| d.kname == kname).expect("known device").clone();
        let devnum = if dev.fstype.as_deref() == Some("btrfs") {
            DevNum::new(0, 500 + dev.devnum.minor)
        } else {
            dev.devnum
        };
        self.add_entry(
            devnum,
            root,
            at,
            dev.fstype.as_deref().unwrap_or("?"),
            &dev.devnode.display().to_string(),
        )
    }

    pub fn add_entry(
        &self,
        devnum: DevNum,
        root: &str,
        at: &Path,
        fstype: &str,
        source: &str,
    ) -> u32 {
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
        Ok(self.devices.borrow().clone())
    }

    fn mounts(&self) -> Result<Vec<MountEntry>> {
        match self.mount_table_reads.get() {
            Some(0) => return Err(devmounts::Error::MountTable { message: "simulated".into() }),
            Some(left) => self.mount_table_reads.set(Some(left - 1)),
            None => {}
        }
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
            return Err(devmounts::Error::MountFailed {
                device: dev.describe(),
                message: "simulated".into(),
            });
        }
        let at = devmounts::mount::mount_dir(&request.mount_base, dev);
        fs::create_dir_all(&at).unwrap();
        self.add_mount(&dev.kname, "/", &at);
        Ok(())
    }

    fn unmount(&self, request: &UnmountRequest<'_>) -> Result<()> {
        let mount_point = &request.mount.mount_point;
        self.calls.borrow_mut().push(format!("unmount {}", mount_point.display()));
        if self.fail_unmounts {
            return Err(devmounts::Error::UnmountFailed {
                mount_point: mount_point.clone(),
                message: "simulated".into(),
            });
        }
        self.mounts.borrow_mut().retain(|m| m.id != request.mount.id);
        Ok(())
    }

    fn unlock(&self, request: &ContainerRequest<'_>) -> Result<()> {
        let container = request.container;
        self.calls.borrow_mut().push(format!("unlock {}", container.kname));
        if self.fail_unlocks {
            return Err(devmounts::Error::UnlockFailed {
                container: container.devnode.clone(),
                message: "simulated".into(),
            });
        }
        let inside = self.locked.borrow_mut().remove(&container.kname).expect("a locked container");
        let mut devices = self.devices.borrow_mut();
        let held =
            devices.iter_mut().find(|d| d.kname == container.kname).expect("known container");
        if inside.kname == container.kname {
            *held = inside;
        } else {
            held.holders = vec![inside.kname.clone()];
            devices.push(inside);
        }
        Ok(())
    }

    fn lock(&self, request: &ContainerRequest<'_>) -> Result<()> {
        let container = request.container;
        self.calls.borrow_mut().push(format!("lock {}", container.kname));
        if self.fail_locks {
            return Err(devmounts::Error::LockFailed {
                container: container.devnode.clone(),
                message: "simulated".into(),
            });
        }
        let mut devices = self.devices.borrow_mut();
        let held =
            devices.iter_mut().find(|d| d.kname == container.kname).expect("known container");
        let [inside] = std::mem::take(&mut held.holders).try_into().expect("one device inside");
        let at = devices.iter().position(|d| d.kname == inside).expect("the device inside");
        let inside = devices.remove(at);
        self.locked.borrow_mut().insert(container.kname.clone(), inside);
        Ok(())
    }
}
