//! FreeBSD: devices from GEOM's configuration (`kern.geom.confxml`), the mount table from
//! `getfsstat(2)`, and mounting with `mount -t <type>` into `mount_base`, through the run's
//! privileged helper — FreeBSD has no automounter to ask.
//!
//! GEOM shows each partition's GPT name and UUID (`partlabel`, `partuuid`) — for MBR, the disk's
//! signature, of which the `partuuid` Linux shows is made — and a filesystem's label, or a UFS
//! filesystem's id (its `uuid` here), as an alias node: `msdosfs/NAME`, `ufs/NAME`, `ufsid/ID`, …
//! A FAT, ext2/3/4 or NTFS filesystem's UUID isn't shown. What filesystem a device holds is told
//! by those aliases, or by its mount, or else guessed from its partition type
//! ([`fstype_for_partition_type`]).
//!
//! While one of a device's names is open, GEOM hides its other aliases ("withering"). So a device
//! is mounted through the alias of its label ([`Geom::mount_node`]), which keeps the label visible
//! to the next run, and a mount made through an alias is matched to the device under it
//! ([`Geom::underlying_node`]). One mounted under another name can only be found by `partuuid` or
//! `partlabel`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::bsd::{self, Mounted};
use crate::mountinfo::MountEntry;
use crate::system::BlockDevice;
use crate::xml::{self, Element};

/// GEOM's alias directories for a filesystem's label, in the order they're preferred to name a
/// device by, each with the filesystem type it tells — but glabel(8)'s own `label/`, which any
/// filesystem can be under.
const LABEL_DIRS: [(&str, Option<&str>); 6] = [
    ("msdosfs/", Some("msdosfs")),
    ("ufs/", Some("ufs")),
    ("ext2fs/", Some("ext2fs")),
    ("ntfs/", Some("ntfs")),
    ("iso9660/", Some("cd9660")),
    ("label/", None),
];

/// GEOM's alias directory for a UFS filesystem's id, which devmounts takes for its `uuid`.
const UFS_ID_DIR: &str = "ufsid/";

/// What GEOM's configuration says about the devices.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Geom {
    /// Every provider that isn't an alias — disks, partitions, and what's stacked on them
    /// (`ada0p2.eli`, `mirror/gm0`, …) — with what its aliases tell about it.
    devices: Vec<BlockDevice>,
    /// Each alias (`msdosfs/NAME`, `gpt/NAME`, …) with the provider it's an alias of: a device, or
    /// another alias (a filesystem's label inside a glabel'd device).
    aliases: Vec<(String, String)>,
    /// Each partition's `gpart` type (`freebsd-ufs`, `ms-basic-data`, …), by its name.
    partition_types: HashMap<String, String>,
}

impl Geom {
    /// The node of the device under an alias node (`/dev/msdosfs/X` → `/dev/da0s1`), for a mount
    /// made through one; `None` when `source` isn't an alias's node.
    pub fn underlying_node(&self, source: &str) -> Option<PathBuf> {
        let name = source.strip_prefix("/dev/")?;
        let is_alias = self.aliases.iter().any(|(alias, _)| alias == name);
        is_alias.then(|| Path::new("/dev").join(self.device_of(name)))
    }

    /// The node to mount `device` through: the alias of its label, or else of its UFS id — while
    /// one is open, GEOM hides the other aliases, but not that one — or the device's own node.
    pub fn mount_node(&self, device: &BlockDevice) -> PathBuf {
        let aliases = self.aliases_of(&device.kname);
        let naming = LABEL_DIRS
            .iter()
            .map(|(dir, _)| *dir)
            .chain([UFS_ID_DIR])
            .find_map(|dir| aliases.iter().find(|alias| alias.starts_with(dir)));
        naming.map_or_else(|| device.devnode.clone(), |alias| Path::new("/dev").join(alias))
    }

    /// The mounts as a mount table, each made through an alias given the device under it as its
    /// source.
    pub fn mount_entries(&self, mut mounted: Vec<Mounted>) -> Vec<MountEntry> {
        for mount in &mut mounted {
            if let Some(node) = self.underlying_node(&mount.source) {
                mount.source = node.to_string_lossy().into_owned();
            }
        }
        bsd::entries(mounted)
    }

    /// The devices, each with the filesystem type its aliases tell — or else what it's mounted
    /// as in `mounts` ([`Geom::mount_entries`]), or else what its partition type usually holds.
    pub fn into_devices(self, mounts: &[MountEntry]) -> Vec<BlockDevice> {
        let Geom { mut devices, partition_types, .. } = self;
        for device in devices.iter_mut().filter(|d| d.fstype.is_none()) {
            let mounted = mounts.iter().find(|m| Path::new(&m.source) == device.devnode);
            device.fstype = mounted.map(|m| m.fstype.clone()).or_else(|| {
                let part_type = partition_types.get(&device.kname)?;
                fstype_for_partition_type(part_type).map(str::to_owned)
            });
        }
        devices
    }

    /// The device `name` is, or is an alias of, through aliases of aliases.
    fn device_of<'a>(&'a self, mut name: &'a str) -> &'a str {
        // Bounded, so that no cycle can hold it.
        for _ in 0..=self.aliases.len() {
            match self.aliases.iter().find(|(alias, _)| alias == name) {
                Some((_, of)) => name = of,
                None => break,
            }
        }
        name
    }

    /// The aliases of the device `kname`, in GEOM's order.
    fn aliases_of(&self, kname: &str) -> Vec<&str> {
        self.aliases
            .iter()
            .map(|(alias, _)| alias.as_str())
            .filter(|alias| self.device_of(alias) == kname)
            .collect()
    }

    /// Each device's label, UFS id and filesystem type, as its aliases tell them.
    fn identify(&mut self) {
        let mut devices = std::mem::take(&mut self.devices);
        for device in &mut devices {
            let aliases = self.aliases_of(&device.kname);
            for (dir, fstype) in LABEL_DIRS {
                if let Some(label) = aliases.iter().find_map(|alias| alias.strip_prefix(dir)) {
                    device.label = Some(label.to_owned());
                    device.fstype = fstype.map(str::to_owned);
                    break;
                }
            }
            if let Some(id) = aliases.iter().find_map(|alias| alias.strip_prefix(UFS_ID_DIR)) {
                device.uuid = Some(id.to_owned());
                device.fstype.get_or_insert_with(|| "ufs".into());
            }
        }
        self.devices = devices;
    }
}

/// Reads GEOM's configuration: the providers of every class but `LABEL` are devices, `LABEL`'s
/// are aliases of the provider their geom consumes. Withered providers — on their way out —
/// count as gone.
pub fn parse_confxml(text: &str) -> Result<Geom, String> {
    let mesh = xml::parse(text)?;
    if mesh.name != "mesh" {
        return Err(format!("<{}> where <mesh> belongs", mesh.name));
    }
    let mut geom = Geom::default();
    let mut names: HashMap<&str, String> = HashMap::new();
    let mut label_geoms = Vec::new();
    for class in named(&mesh, "class") {
        let class_name = text_of(class, "name");
        for gp in named(class, "geom").filter(|gp| gp.child("wither").is_none()) {
            for provider in providers(gp) {
                let name = text_of(provider, "name");
                if let Some(id) = provider.attribute("id") {
                    names.insert(id, name.clone());
                }
                if class_name != "LABEL" {
                    let config = provider.child("config");
                    let partition = class_name == "PART";
                    geom.devices.push(device(&name, partition, config, &mut geom.partition_types));
                }
            }
            if class_name == "LABEL" {
                label_geoms.push(gp);
            }
        }
    }
    for gp in label_geoms {
        let consumed =
            gp.child("consumer").and_then(|c| c.child("provider")).and_then(|p| p.attribute("ref"));
        let Some(of) = consumed.and_then(|id| names.get(id)) else { continue };
        for provider in providers(gp) {
            geom.aliases.push((text_of(provider, "name"), of.clone()));
        }
    }
    geom.identify();
    Ok(geom)
}

/// A device for the provider `name`, with what a partition's `config` says about it.
fn device(
    name: &str,
    partition: bool,
    config: Option<&Element>,
    partition_types: &mut HashMap<String, String>,
) -> BlockDevice {
    let mut device = BlockDevice {
        kname: name.to_owned(),
        devnode: Path::new("/dev").join(name),
        partition,
        ..BlockDevice::default()
    };
    if partition && let Some(config) = config {
        let field = |key: &str| Some(text_of(config, key)).filter(|value| !value.is_empty());
        device.partlabel = field("label");
        device.partuuid = field("rawuuid").or_else(|| mbr_partuuid(&field("efimedia")?));
        if let Some(part_type) = field("type") {
            partition_types.insert(name.to_owned(), part_type);
        }
    }
    device
}

fn named<'a>(element: &'a Element, name: &'a str) -> impl Iterator<Item = &'a Element> {
    element.elements().filter(move |child| child.name == name)
}

fn providers(gp: &Element) -> impl Iterator<Item = &Element> {
    named(gp, "provider").filter(|provider| provider.child("wither").is_none())
}

/// The text of `element`'s child `name`, as GEOM meant it ([`geom_text`]).
fn text_of(element: &Element, name: &str) -> String {
    element.child(name).map(|child| geom_text(&child.text())).unwrap_or_default()
}

/// GEOM's text as it was: GEOM writes each byte above 0x7e as a character reference of its own
/// (`&#xC3;&#xA9;` for `é`), so the characters those read as are the bytes of UTF-8 text.
fn geom_text(read: &str) -> String {
    let bytes: Vec<u8> = read.chars().map(|c| u8::try_from(c).unwrap_or(b'?')).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The PARTUUID Linux gives an MBR partition — the disk's signature and the partition's number,
/// `6c9fd5a1-01` — from GEOM's EFI device path for it, `HD(1,MBR,0x6c9fd5a1,0x800,0x1dcd000)`.
/// A disk without a signature (0) has none, as on Linux.
fn mbr_partuuid(efimedia: &str) -> Option<String> {
    let fields: Vec<&str> = efimedia.strip_prefix("HD(")?.strip_suffix(')')?.split(',').collect();
    let [index, "MBR", signature, ..] = fields[..] else { return None };
    let index: u8 = index.parse().ok()?;
    // Written `%#08x`, which prefixes `0x` to every number but 0.
    let digits = signature.strip_prefix("0x").unwrap_or(signature);
    let signature = u32::from_str_radix(digits, 16).ok()?;
    (signature != 0).then(|| format!("{signature:08x}-{index:02x}"))
}

/// What a partition of `gpart` type `part_type` usually holds, for one that neither an alias nor
/// a mount says more about: FreeBSD's own types, and for Windows' and EFI's, FAT — what removable
/// drives mostly carry. (An NTFS or exFAT one then fails to mount, saying so.)
pub fn fstype_for_partition_type(part_type: &str) -> Option<&'static str> {
    Some(match part_type {
        "freebsd-ufs" => "ufs",
        "efi" | "fat16" | "fat32" | "fat32lba" | "ms-basic-data" => "msdosfs",
        "linux-data" => "ext2fs",
        "ntfs" => "ntfs",
        "freebsd-swap" | "linux-swap" => "swap",
        "freebsd-zfs" => "zfs_member",
        _ => return None,
    })
}

#[cfg(target_os = "freebsd")]
pub use native::FreeBsdSystem;

/// What only builds on FreeBSD: the real machine, and its calls into the system.
#[cfg(target_os = "freebsd")]
mod native {
    use std::io;
    use std::os::unix::fs::FileTypeExt;

    use super::*;
    use crate::error::{Error, Result};
    use crate::mount;
    use crate::mountinfo::DevNum;
    use crate::platform::Platform;
    use crate::system::{self, ContainerRequest, MountRequest, System, UnmountRequest};

    /// The real machine, on FreeBSD.
    #[derive(Debug, Clone)]
    pub struct FreeBsdSystem {
        /// Where an automounter mounts devices (autofs' `/media`).
        pub automount_dirs: Vec<PathBuf>,
    }

    impl Default for FreeBsdSystem {
        fn default() -> Self {
            FreeBsdSystem {
                automount_dirs: Platform::FreeBsd
                    .automount_dirs()
                    .iter()
                    .map(PathBuf::from)
                    .collect(),
            }
        }
    }

    impl FreeBsdSystem {
        pub fn new() -> Self {
            Self::default()
        }

        /// Like [`FreeBsdSystem::new`], but `DEVMOUNTS_AUTOMOUNT_DIRS` (`:`-separated, like
        /// `PATH`) overrides the automounters' directories.
        pub fn from_env() -> Self {
            FreeBsdSystem {
                automount_dirs: system::automount_dirs_from_env(Self::default().automount_dirs),
            }
        }
    }

    /// GEOM's configuration, read.
    fn geom() -> std::result::Result<Geom, String> {
        let text = confxml().map_err(|err| format!("kern.geom.confxml: {err}"))?;
        parse_confxml(&text).map_err(|why| format!("kern.geom.confxml: {why}"))
    }

    /// The `kern.geom.confxml` sysctl: GEOM's whole configuration, as XML.
    fn confxml() -> io::Result<String> {
        let name = c"kern.geom.confxml";
        loop {
            let mut len = 0;
            // SAFETY: without a buffer, sysctlbyname only says how long the value is.
            let rc = unsafe {
                libc::sysctlbyname(
                    name.as_ptr(),
                    std::ptr::null_mut(),
                    &mut len,
                    std::ptr::null(),
                    0,
                )
            };
            if rc != 0 {
                return Err(io::Error::last_os_error());
            }
            // Room for the configuration to grow between asking its length and reading it.
            let mut buf = vec![0u8; len + len / 8 + 4096];
            let mut filled = buf.len();
            // SAFETY: sysctlbyname writes at most `filled` bytes into the buffer, which has that
            // many, and sets `filled` to how many it wrote.
            let rc = unsafe {
                libc::sysctlbyname(
                    name.as_ptr(),
                    buf.as_mut_ptr().cast(),
                    &mut filled,
                    std::ptr::null(),
                    0,
                )
            };
            if rc != 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::ENOMEM) {
                    continue; // it grew past the room: again
                }
                return Err(err);
            }
            buf.truncate(filled);
            if buf.last() == Some(&0) {
                buf.pop();
            }
            return String::from_utf8(buf)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not UTF-8"));
        }
    }

    fn mounted() -> Result<Vec<Mounted>> {
        bsd::mount_table().map_err(|err| Error::MountTable { message: format!("getfsstat: {err}") })
    }

    impl System for FreeBsdSystem {
        fn block_devices(&self) -> Result<Vec<BlockDevice>> {
            let geom = geom().map_err(|message| Error::Inventory { message })?;
            let mounts = geom.mount_entries(mounted()?);
            let mut devices = geom.into_devices(&mounts);
            for device in &mut devices {
                device.devnum = self.device_number(&device.devnode).unwrap_or_default();
            }
            Ok(devices)
        }

        /// Read with GEOM's configuration, without which a mount through an alias couldn't be told
        /// for whose it is.
        fn mounts(&self) -> Result<Vec<MountEntry>> {
            let geom = geom().map_err(|message| Error::MountTable { message })?;
            Ok(geom.mount_entries(mounted()?))
        }

        /// FreeBSD has no block devices: its disks are character devices.
        fn device_number(&self, path: &Path) -> Option<DevNum> {
            system::node_number(path, FileTypeExt::is_char_device)
        }

        fn automount_dirs(&self) -> Vec<PathBuf> {
            self.automount_dirs.clone()
        }

        /// Through the alias of the device's label, so that the label stays visible
        /// ([`Geom::mount_node`]); with GEOM unreadable, through the device node.
        fn mount(&self, request: &MountRequest<'_>) -> Result<()> {
            let devnode = geom()
                .map(|geom| geom.mount_node(request.device))
                .unwrap_or_else(|_| request.device.devnode.clone());
            let device = BlockDevice { devnode, ..request.device.clone() };
            mount::mount_into_base(&MountRequest { device: &device, ..request.clone() }).map_err(
                |err| Error::MountFailed {
                    device: device.describe(),
                    message: mount::inner_message(err),
                },
            )
        }

        fn unmount(&self, request: &UnmountRequest<'_>) -> Result<()> {
            mount::unmount_with_umount(request)
        }

        fn unlock(&self, request: &ContainerRequest<'_>) -> Result<()> {
            let container = request.container.devnode.clone();
            Err(Error::UnlockFailed { container, message: mount::LUKS_NEEDS_UDISKS.into() })
        }

        fn lock(&self, request: &ContainerRequest<'_>) -> Result<()> {
            let container = request.container.devnode.clone();
            Err(Error::LockFailed { container, message: mount::LUKS_NEEDS_UDISKS.into() })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GEOM's configuration of a machine with an internal disk (GPT: EFI, swap, and UFS mounted as
    /// `/` through its GPT label, so its other aliases withered — one of them caught going), a USB
    /// stick (MBR, FAT, labelled `STICK`) and a USB drive (GPT: a FAT partition named `Données`,
    /// mounted through its own node, so its label withered; ext2fs labelled `backup`; a glabel'd
    /// UFS labelled `VAULT` inside; a UFS with only its id). Written the way `geom_dump.c` writes
    /// it — a sample: no FreeBSD machine has been at hand to record one (see the README).
    const CONFXML: &str = "<mesh>
  <class id=\"0xffffffff81c2a6a0\">
    <name>FD</name>
  </class>
  <class id=\"0xffffffff81c2b0d0\">
    <name>DISK</name>
    <geom id=\"0xfffff80003a1e100\">
      <class ref=\"0xffffffff81c2b0d0\"/>
      <name>ada0</name>
      <rank>1</rank>
      <config>
      </config>
\t<provider id=\"0xfffff80003a1e000\">
\t  <geom ref=\"0xfffff80003a1e100\"/>
\t  <mode>r2w2e3</mode>
\t  <name>ada0</name>
\t  <mediasize>256060514304</mediasize>
\t  <sectorsize>512</sectorsize>
\t  <stripesize>4096</stripesize>
\t  <stripeoffset>0</stripeoffset>
\t  <config>
\t    <fwheads>16</fwheads>
\t    <fwsectors>63</fwsectors>
\t    <rotationrate>0</rotationrate>
\t    <ident>S3Z9NB0K123456</ident>
\t    <lunid>5002538e40000000</lunid>
\t    <descr>Samsung SSD 860 EVO 250GB</descr>
\t  </config>
\t</provider>
    </geom>
    <geom id=\"0xfffff80003b2e100\">
      <class ref=\"0xffffffff81c2b0d0\"/>
      <name>da0</name>
      <rank>1</rank>
      <config>
      </config>
\t<provider id=\"0xfffff80003b2e000\">
\t  <geom ref=\"0xfffff80003b2e100\"/>
\t  <mode>r0w0e0</mode>
\t  <name>da0</name>
\t  <mediasize>16008609792</mediasize>
\t  <sectorsize>512</sectorsize>
\t  <stripesize>0</stripesize>
\t  <stripeoffset>0</stripeoffset>
\t  <config>
\t    <descr>SanDisk Cruzer Blade</descr>
\t  </config>
\t</provider>
    </geom>
    <geom id=\"0xfffff80003c3e100\">
      <class ref=\"0xffffffff81c2b0d0\"/>
      <name>da1</name>
      <rank>1</rank>
      <config>
      </config>
\t<provider id=\"0xfffff80003c3e000\">
\t  <geom ref=\"0xfffff80003c3e100\"/>
\t  <mode>r1w1e2</mode>
\t  <name>da1</name>
\t  <mediasize>2000398934016</mediasize>
\t  <sectorsize>512</sectorsize>
\t  <stripesize>4096</stripesize>
\t  <stripeoffset>0</stripeoffset>
\t  <config>
\t    <descr>WD Elements 25A2</descr>
\t  </config>
\t</provider>
    </geom>
  </class>
  <class id=\"0xffffffff81c2c1e0\">
    <name>PART</name>
    <geom id=\"0xfffff80003d4e100\">
      <class ref=\"0xffffffff81c2c1e0\"/>
      <name>ada0</name>
      <rank>2</rank>
      <config>
\t<scheme>GPT</scheme>
\t<entries>128</entries>
\t<first>40</first>
\t<last>500118152</last>
\t<fwsectors>63</fwsectors>
\t<fwheads>16</fwheads>
\t<state>OK</state>
\t<modified>false</modified>
      </config>
\t<consumer id=\"0xfffff80003d4e200\">
\t  <geom ref=\"0xfffff80003d4e100\"/>
\t  <provider ref=\"0xfffff80003a1e000\"/>
\t  <mode>r2w2e5</mode>
\t  <config>
\t  </config>
\t</consumer>
\t<provider id=\"0xfffff80003d4e001\">
\t  <geom ref=\"0xfffff80003d4e100\"/>
\t  <mode>r0w0e0</mode>
\t  <name>ada0p1</name>
\t  <mediasize>272629760</mediasize>
\t  <sectorsize>512</sectorsize>
\t  <stripesize>4096</stripesize>
\t  <stripeoffset>0</stripeoffset>
\t  <config>
\t    <start>40</start>
\t    <end>532519</end>
\t    <index>1</index>
\t    <type>efi</type>
\t    <offset>20480</offset>
\t    <length>272629760</length>
\t    <label>efiboot0</label>
\t    <rawtype>c12a7328-f81f-11d2-ba4b-00a0c93ec93b</rawtype>
\t    <rawuuid>4a2c0d6e-1d2b-11ef-9c1a-001b21aabbcc</rawuuid>
\t    <efimedia>HD(1,GPT,4a2c0d6e-1d2b-11ef-9c1a-001b21aabbcc,0x28,0x82000)</efimedia>
\t  </config>
\t</provider>
\t<provider id=\"0xfffff80003d4e002\">
\t  <geom ref=\"0xfffff80003d4e100\"/>
\t  <mode>r1w1e1</mode>
\t  <name>ada0p2</name>
\t  <mediasize>8589934592</mediasize>
\t  <sectorsize>512</sectorsize>
\t  <stripesize>4096</stripesize>
\t  <stripeoffset>0</stripeoffset>
\t  <config>
\t    <start>532520</start>
\t    <end>17309735</end>
\t    <index>2</index>
\t    <type>freebsd-swap</type>
\t    <offset>272650240</offset>
\t    <length>8589934592</length>
\t    <label>swap0</label>
\t    <rawtype>516e7cb5-6ecf-11d6-8ff8-00022d09712b</rawtype>
\t    <rawuuid>4a2c0d6e-1d2b-11ef-9c1a-001b21aabbcd</rawuuid>
\t    <efimedia>HD(2,GPT,4a2c0d6e-1d2b-11ef-9c1a-001b21aabbcd,0x82028,0x1000000)</efimedia>
\t  </config>
\t</provider>
\t<provider id=\"0xfffff80003d4e003\">
\t  <geom ref=\"0xfffff80003d4e100\"/>
\t  <mode>r1w1e1</mode>
\t  <name>ada0p3</name>
\t  <mediasize>247198433280</mediasize>
\t  <sectorsize>512</sectorsize>
\t  <stripesize>4096</stripesize>
\t  <stripeoffset>0</stripeoffset>
\t  <config>
\t    <start>17309736</start>
\t    <end>500118151</end>
\t    <index>3</index>
\t    <type>freebsd-ufs</type>
\t    <offset>8862584832</offset>
\t    <length>247198433280</length>
\t    <label>rootfs</label>
\t    <rawtype>516e7cb6-6ecf-11d6-8ff8-00022d09712b</rawtype>
\t    <rawuuid>4a2c0d6e-1d2b-11ef-9c1a-001b21aabbce</rawuuid>
\t    <efimedia>HD(3,GPT,4a2c0d6e-1d2b-11ef-9c1a-001b21aabbce,0x1082028,0x1cc8c7c8)</efimedia>
\t  </config>
\t</provider>
    </geom>
    <geom id=\"0xfffff80003e5e100\">
      <class ref=\"0xffffffff81c2c1e0\"/>
      <name>da0</name>
      <rank>2</rank>
      <config>
\t<scheme>MBR</scheme>
\t<entries>4</entries>
\t<first>63</first>
\t<last>31266815</last>
\t<fwsectors>63</fwsectors>
\t<fwheads>255</fwheads>
\t<state>OK</state>
\t<modified>false</modified>
      </config>
\t<consumer id=\"0xfffff80003e5e200\">
\t  <geom ref=\"0xfffff80003e5e100\"/>
\t  <provider ref=\"0xfffff80003b2e000\"/>
\t  <mode>r0w0e0</mode>
\t  <config>
\t  </config>
\t</consumer>
\t<provider id=\"0xfffff80003e5e001\">
\t  <geom ref=\"0xfffff80003e5e100\"/>
\t  <mode>r0w0e0</mode>
\t  <name>da0s1</name>
\t  <mediasize>16007561216</mediasize>
\t  <sectorsize>512</sectorsize>
\t  <stripesize>0</stripesize>
\t  <stripeoffset>1048576</stripeoffset>
\t  <config>
\t    <start>2048</start>
\t    <end>31266815</end>
\t    <index>1</index>
\t    <type>fat32lba</type>
\t    <offset>1048576</offset>
\t    <length>16007561216</length>
\t    <rawtype>12</rawtype>
\t    <attrib>active</attrib>
\t    <efimedia>HD(1,MBR,0x6c9fd5a1,0x800,0x1dcd000)</efimedia>
\t  </config>
\t</provider>
    </geom>
    <geom id=\"0xfffff80003f6e100\">
      <class ref=\"0xffffffff81c2c1e0\"/>
      <name>da1</name>
      <rank>2</rank>
      <config>
\t<scheme>GPT</scheme>
\t<entries>128</entries>
\t<first>34</first>
\t<last>3907029134</last>
\t<fwsectors>63</fwsectors>
\t<fwheads>255</fwheads>
\t<state>OK</state>
\t<modified>false</modified>
      </config>
\t<consumer id=\"0xfffff80003f6e200\">
\t  <geom ref=\"0xfffff80003f6e100\"/>
\t  <provider ref=\"0xfffff80003c3e000\"/>
\t  <mode>r1w1e2</mode>
\t  <config>
\t  </config>
\t</consumer>
\t<provider id=\"0xfffff80003f6e001\">
\t  <geom ref=\"0xfffff80003f6e100\"/>
\t  <mode>r1w1e1</mode>
\t  <name>da1p1</name>
\t  <mediasize>536870912000</mediasize>
\t  <sectorsize>512</sectorsize>
\t  <stripesize>4096</stripesize>
\t  <stripeoffset>0</stripeoffset>
\t  <config>
\t    <start>2048</start>
\t    <end>1048578047</end>
\t    <index>1</index>
\t    <type>ms-basic-data</type>
\t    <offset>1048576</offset>
\t    <length>536870912000</length>
\t    <label>Donn&#xC3;&#xA9;es</label>
\t    <rawtype>ebd0a0a2-b9e5-4433-87c0-68b6b72699c7</rawtype>
\t    <rawuuid>9b1f6d3e-5c4a-4e2b-8f7d-0123456789ab</rawuuid>
\t    <efimedia>HD(1,GPT,9b1f6d3e-5c4a-4e2b-8f7d-0123456789ab,0x800,0x3e800000)</efimedia>
\t  </config>
\t</provider>
\t<provider id=\"0xfffff80003f6e002\">
\t  <geom ref=\"0xfffff80003f6e100\"/>
\t  <mode>r0w0e0</mode>
\t  <name>da1p2</name>
\t  <mediasize>536870912000</mediasize>
\t  <sectorsize>512</sectorsize>
\t  <stripesize>4096</stripesize>
\t  <stripeoffset>0</stripeoffset>
\t  <config>
\t    <start>1048578048</start>
\t    <end>2097154047</end>
\t    <index>2</index>
\t    <type>linux-data</type>
\t    <offset>536871960576</offset>
\t    <length>536870912000</length>
\t    <label></label>
\t    <rawtype>0fc63daf-8483-4772-8e79-3d69d8477de4</rawtype>
\t    <rawuuid>9b1f6d3e-5c4a-4e2b-8f7d-0123456789ac</rawuuid>
\t    <efimedia>HD(2,GPT,9b1f6d3e-5c4a-4e2b-8f7d-0123456789ac,0x3e800800,0x3e800000)</efimedia>
\t  </config>
\t</provider>
\t<provider id=\"0xfffff80003f6e003\">
\t  <geom ref=\"0xfffff80003f6e100\"/>
\t  <mode>r0w0e0</mode>
\t  <name>da1p3</name>
\t  <mediasize>107374182400</mediasize>
\t  <sectorsize>512</sectorsize>
\t  <stripesize>4096</stripesize>
\t  <stripeoffset>0</stripeoffset>
\t  <config>
\t    <start>2097154048</start>
\t    <end>2306869247</end>
\t    <index>3</index>
\t    <type>freebsd-ufs</type>
\t    <offset>1073742872576</offset>
\t    <length>107374182400</length>
\t    <label></label>
\t    <rawtype>516e7cb6-6ecf-11d6-8ff8-00022d09712b</rawtype>
\t    <rawuuid>9b1f6d3e-5c4a-4e2b-8f7d-0123456789ad</rawuuid>
\t    <efimedia>HD(3,GPT,9b1f6d3e-5c4a-4e2b-8f7d-0123456789ad,0x7d001000,0xc800000)</efimedia>
\t  </config>
\t</provider>
\t<provider id=\"0xfffff80003f6e004\">
\t  <geom ref=\"0xfffff80003f6e100\"/>
\t  <mode>r0w0e0</mode>
\t  <name>da1p4</name>
\t  <mediasize>107374182400</mediasize>
\t  <sectorsize>512</sectorsize>
\t  <stripesize>4096</stripesize>
\t  <stripeoffset>0</stripeoffset>
\t  <config>
\t    <start>2306869248</start>
\t    <end>2516584447</end>
\t    <index>4</index>
\t    <type>freebsd-ufs</type>
\t    <offset>1181117054976</offset>
\t    <length>107374182400</length>
\t    <label></label>
\t    <rawtype>516e7cb6-6ecf-11d6-8ff8-00022d09712b</rawtype>
\t    <rawuuid>9b1f6d3e-5c4a-4e2b-8f7d-0123456789ae</rawuuid>
\t    <efimedia>HD(4,GPT,9b1f6d3e-5c4a-4e2b-8f7d-0123456789ae,0x89801000,0xc800000)</efimedia>
\t  </config>
\t</provider>
    </geom>
  </class>
  <class id=\"0xffffffff81c2d2f0\">
    <name>LABEL</name>
    <geom id=\"0xfffff80004a1e100\">
      <class ref=\"0xffffffff81c2d2f0\"/>
      <name>ada0p1</name>
      <rank>3</rank>
      <config>
      </config>
\t<consumer id=\"0xfffff80004a1e200\">
\t  <geom ref=\"0xfffff80004a1e100\"/>
\t  <provider ref=\"0xfffff80003d4e001\"/>
\t  <mode>r0w0e0</mode>
\t  <config>
\t  </config>
\t</consumer>
\t<provider id=\"0xfffff80004a1e001\">
\t  <geom ref=\"0xfffff80004a1e100\"/>
\t  <mode>r0w0e0</mode>
\t  <name>msdosfs/EFISYS</name>
\t  <mediasize>272629760</mediasize>
\t  <sectorsize>512</sectorsize>
\t  <stripesize>4096</stripesize>
\t  <stripeoffset>0</stripeoffset>
\t  <config>
\t    <length>272629760</length>
\t    <offset>0</offset>
\t    <seclength>532480</seclength>
\t    <secoffset>0</secoffset>
\t  </config>
\t</provider>
    </geom>
    <geom id=\"0xfffff80004a2e100\">
      <class ref=\"0xffffffff81c2d2f0\"/>
      <name>ada0p1</name>
      <rank>3</rank>
      <config>
      </config>
\t<consumer id=\"0xfffff80004a2e200\">
\t  <geom ref=\"0xfffff80004a2e100\"/>
\t  <provider ref=\"0xfffff80003d4e001\"/>
\t  <mode>r0w0e0</mode>
\t  <config>
\t  </config>
\t</consumer>
\t<provider id=\"0xfffff80004a2e001\">
\t  <geom ref=\"0xfffff80004a2e100\"/>
\t  <mode>r0w0e0</mode>
\t  <name>gpt/efiboot0</name>
\t  <mediasize>272629760</mediasize>
\t  <sectorsize>512</sectorsize>
\t  <stripesize>4096</stripesize>
\t  <stripeoffset>0</stripeoffset>
\t  <config>
\t    <length>272629760</length>
\t    <offset>0</offset>
\t    <seclength>532480</seclength>
\t    <secoffset>0</secoffset>
\t  </config>
\t</provider>
    </geom>
    <geom id=\"0xfffff80004a3e100\">
      <class ref=\"0xffffffff81c2d2f0\"/>
      <name>ada0p3</name>
      <rank>3</rank>
      <config>
      </config>
\t<consumer id=\"0xfffff80004a3e200\">
\t  <geom ref=\"0xfffff80004a3e100\"/>
\t  <provider ref=\"0xfffff80003d4e003\"/>
\t  <mode>r1w1e1</mode>
\t  <config>
\t  </config>
\t</consumer>
\t<provider id=\"0xfffff80004a3e001\">
\t  <geom ref=\"0xfffff80004a3e100\"/>
\t  <mode>r1w1e1</mode>
\t  <name>gpt/rootfs</name>
\t  <mediasize>247198433280</mediasize>
\t  <sectorsize>512</sectorsize>
\t  <stripesize>4096</stripesize>
\t  <stripeoffset>0</stripeoffset>
\t  <config>
\t    <length>247198433280</length>
\t    <offset>0</offset>
\t    <seclength>482809440</seclength>
\t    <secoffset>0</secoffset>
\t  </config>
\t</provider>
    </geom>
    <geom id=\"0xfffff80004a4e100\">
      <class ref=\"0xffffffff81c2d2f0\"/>
      <name>ada0p3</name>
      <rank>3</rank>
      <config>
      </config>
\t<consumer id=\"0xfffff80004a4e200\">
\t  <geom ref=\"0xfffff80004a4e100\"/>
\t  <provider ref=\"0xfffff80003d4e003\"/>
\t  <mode>r0w0e0</mode>
\t  <config>
\t  </config>
\t</consumer>
\t<provider id=\"0xfffff80004a4e001\">
\t  <geom ref=\"0xfffff80004a4e100\"/>
\t  <mode>r0w0e0</mode>
\t  <name>ufsid/5f3a9c0e12ab34cd</name>
\t  <mediasize>247198433280</mediasize>
\t  <sectorsize>512</sectorsize>
\t  <stripesize>4096</stripesize>
\t  <stripeoffset>0</stripeoffset>
\t  <wither/>
\t</provider>
    </geom>
    <geom id=\"0xfffff80004b1e100\">
      <class ref=\"0xffffffff81c2d2f0\"/>
      <name>da0s1</name>
      <rank>3</rank>
      <config>
      </config>
\t<consumer id=\"0xfffff80004b1e200\">
\t  <geom ref=\"0xfffff80004b1e100\"/>
\t  <provider ref=\"0xfffff80003e5e001\"/>
\t  <mode>r0w0e0</mode>
\t  <config>
\t  </config>
\t</consumer>
\t<provider id=\"0xfffff80004b1e001\">
\t  <geom ref=\"0xfffff80004b1e100\"/>
\t  <mode>r0w0e0</mode>
\t  <name>msdosfs/STICK</name>
\t  <mediasize>16007561216</mediasize>
\t  <sectorsize>512</sectorsize>
\t  <stripesize>0</stripesize>
\t  <stripeoffset>1048576</stripeoffset>
\t  <config>
\t    <length>16007561216</length>
\t    <offset>0</offset>
\t    <seclength>31264768</seclength>
\t    <secoffset>0</secoffset>
\t  </config>
\t</provider>
    </geom>
    <geom id=\"0xfffff80004c2e100\">
      <class ref=\"0xffffffff81c2d2f0\"/>
      <name>da1p2</name>
      <rank>3</rank>
      <config>
      </config>
\t<consumer id=\"0xfffff80004c2e200\">
\t  <geom ref=\"0xfffff80004c2e100\"/>
\t  <provider ref=\"0xfffff80003f6e002\"/>
\t  <mode>r0w0e0</mode>
\t  <config>
\t  </config>
\t</consumer>
\t<provider id=\"0xfffff80004c2e001\">
\t  <geom ref=\"0xfffff80004c2e100\"/>
\t  <mode>r0w0e0</mode>
\t  <name>ext2fs/backup</name>
\t  <mediasize>536870912000</mediasize>
\t  <sectorsize>512</sectorsize>
\t  <stripesize>4096</stripesize>
\t  <stripeoffset>0</stripeoffset>
\t  <config>
\t    <length>536870912000</length>
\t    <offset>0</offset>
\t    <seclength>1048576000</seclength>
\t    <secoffset>0</secoffset>
\t  </config>
\t</provider>
    </geom>
    <geom id=\"0xfffff80004c3e100\">
      <class ref=\"0xffffffff81c2d2f0\"/>
      <name>da1p3</name>
      <rank>3</rank>
      <config>
      </config>
\t<consumer id=\"0xfffff80004c3e200\">
\t  <geom ref=\"0xfffff80004c3e100\"/>
\t  <provider ref=\"0xfffff80003f6e003\"/>
\t  <mode>r0w0e0</mode>
\t  <config>
\t  </config>
\t</consumer>
\t<provider id=\"0xfffff80004c3e001\">
\t  <geom ref=\"0xfffff80004c3e100\"/>
\t  <mode>r0w0e0</mode>
\t  <name>label/vault</name>
\t  <mediasize>107374181888</mediasize>
\t  <sectorsize>512</sectorsize>
\t  <stripesize>4096</stripesize>
\t  <stripeoffset>0</stripeoffset>
\t  <config>
\t    <length>107374181888</length>
\t    <offset>0</offset>
\t    <seclength>209715199</seclength>
\t    <secoffset>0</secoffset>
\t  </config>
\t</provider>
    </geom>
    <geom id=\"0xfffff80004c4e100\">
      <class ref=\"0xffffffff81c2d2f0\"/>
      <name>label/vault</name>
      <rank>4</rank>
      <config>
      </config>
\t<consumer id=\"0xfffff80004c4e200\">
\t  <geom ref=\"0xfffff80004c4e100\"/>
\t  <provider ref=\"0xfffff80004c3e001\"/>
\t  <mode>r0w0e0</mode>
\t  <config>
\t  </config>
\t</consumer>
\t<provider id=\"0xfffff80004c4e001\">
\t  <geom ref=\"0xfffff80004c4e100\"/>
\t  <mode>r0w0e0</mode>
\t  <name>ufs/VAULT</name>
\t  <mediasize>107374181888</mediasize>
\t  <sectorsize>512</sectorsize>
\t  <stripesize>4096</stripesize>
\t  <stripeoffset>0</stripeoffset>
\t  <config>
\t    <length>107374181888</length>
\t    <offset>0</offset>
\t    <seclength>209715199</seclength>
\t    <secoffset>0</secoffset>
\t  </config>
\t</provider>
    </geom>
    <geom id=\"0xfffff80004c5e100\">
      <class ref=\"0xffffffff81c2d2f0\"/>
      <name>da1p4</name>
      <rank>3</rank>
      <config>
      </config>
\t<consumer id=\"0xfffff80004c5e200\">
\t  <geom ref=\"0xfffff80004c5e100\"/>
\t  <provider ref=\"0xfffff80003f6e004\"/>
\t  <mode>r0w0e0</mode>
\t  <config>
\t  </config>
\t</consumer>
\t<provider id=\"0xfffff80004c5e001\">
\t  <geom ref=\"0xfffff80004c5e100\"/>
\t  <mode>r0w0e0</mode>
\t  <name>ufsid/61b0c2d4e5f60718</name>
\t  <mediasize>107374182400</mediasize>
\t  <sectorsize>512</sectorsize>
\t  <stripesize>4096</stripesize>
\t  <stripeoffset>0</stripeoffset>
\t  <config>
\t    <length>107374182400</length>
\t    <offset>0</offset>
\t    <seclength>209715200</seclength>
\t    <secoffset>0</secoffset>
\t  </config>
\t</provider>
    </geom>
  </class>
  <class id=\"0xffffffff81c2e300\">
    <name>DEV</name>
    <geom id=\"0xfffff80005a1e100\">
      <class ref=\"0xffffffff81c2e300\"/>
      <name>ada0p3</name>
      <rank>3</rank>
\t<consumer id=\"0xfffff80005a1e200\">
\t  <geom ref=\"0xfffff80005a1e100\"/>
\t  <provider ref=\"0xfffff80003d4e003\"/>
\t  <mode>r0w0e0</mode>
\t</consumer>
    </geom>
  </class>
</mesh>
";

    fn mounted(source: &str, at: &str, fstype: &str) -> Mounted {
        Mounted {
            source: source.into(),
            mount_point: at.into(),
            fstype: fstype.into(),
            read_only: false,
        }
    }

    /// The sample's mount table: `/` through its GPT label, the drive's FAT partition through its
    /// own node (autofs), and its ext2fs through its label (devmounts).
    fn mount_table(geom: &Geom) -> Vec<MountEntry> {
        geom.mount_entries(vec![
            mounted("/dev/gpt/rootfs", "/", "ufs"),
            mounted("devfs", "/dev", "devfs"),
            mounted("/dev/da1p1", "/media/da1p1", "msdosfs"),
            mounted("/dev/ext2fs/backup", "/var/run/devmounts/da1p2", "ext2fs"),
        ])
    }

    fn devices() -> Vec<BlockDevice> {
        let geom = parse_confxml(CONFXML).unwrap();
        let mounts = mount_table(&geom);
        geom.into_devices(&mounts)
    }

    fn get<'a>(devices: &'a [BlockDevice], kname: &str) -> &'a BlockDevice {
        devices.iter().find(|d| d.kname == kname).unwrap_or_else(|| panic!("{kname}"))
    }

    /// Disks and partitions are devices — aliases aren't, whatever class holds no providers adds
    /// nothing, and a withered alias counts as gone.
    #[test]
    fn providers_are_devices_and_aliases_are_not() {
        let devices = devices();
        let knames: Vec<&str> = devices.iter().map(|d| d.kname.as_str()).collect();
        assert_eq!(
            knames,
            [
                "ada0", "da0", "da1", "ada0p1", "ada0p2", "ada0p3", "da0s1", "da1p1", "da1p2",
                "da1p3", "da1p4"
            ]
        );
        for device in &devices {
            assert_eq!(device.devnode, Path::new("/dev").join(&device.kname));
            let whole = ["ada0", "da0", "da1"].contains(&device.kname.as_str());
            assert_eq!(device.partition, !whole, "{}", device.kname);
        }
        let disk = get(&devices, "da1");
        assert_eq!((disk.fstype.as_deref(), disk.label.as_deref()), (None, None));
        assert_eq!(get(&devices, "ada0p3").uuid, None, "its ufsid withered");
    }

    /// A partition's GPT name and UUID always show; an MBR partition gets Linux's PARTUUID.
    #[test]
    fn partitions_are_named_by_their_table() {
        let devices = devices();
        let esp = get(&devices, "ada0p1");
        assert_eq!(esp.partlabel.as_deref(), Some("efiboot0"));
        assert_eq!(esp.partuuid.as_deref(), Some("4a2c0d6e-1d2b-11ef-9c1a-001b21aabbcc"));
        let data = get(&devices, "da1p1");
        assert_eq!(data.partlabel.as_deref(), Some("Données"), "GEOM's bytes read back as UTF-8");
        assert_eq!(get(&devices, "da1p2").partlabel, None, "an empty name is none");
        let stick = get(&devices, "da0s1");
        assert_eq!(
            (stick.partuuid.as_deref(), stick.partlabel.as_deref()),
            (Some("6c9fd5a1-01"), None)
        );
    }

    /// Filesystems are named by their aliases — a label, through a glabel too, and a UFS id — and
    /// typed by them, or else by their mount, or else by their partition type.
    #[test]
    fn filesystems_are_named_and_typed() {
        let devices = devices();
        let identity = |kname: &str| {
            let d = get(&devices, kname);
            (d.fstype.as_deref(), d.label.as_deref(), d.uuid.as_deref())
        };
        assert_eq!(identity("ada0p1"), (Some("msdosfs"), Some("EFISYS"), None));
        assert_eq!(identity("da0s1"), (Some("msdosfs"), Some("STICK"), None));
        assert_eq!(identity("da1p2"), (Some("ext2fs"), Some("backup"), None));
        assert_eq!(
            identity("da1p3"),
            (Some("ufs"), Some("VAULT"), None),
            "the label inside, not glabel's"
        );
        assert_eq!(identity("da1p4"), (Some("ufs"), None, Some("61b0c2d4e5f60718")));
        assert_eq!(identity("ada0p3"), (Some("ufs"), None, None), "typed by its mount");
        assert_eq!(identity("da1p1"), (Some("msdosfs"), None, None), "typed by its mount");
        assert_eq!(identity("ada0p2"), (Some("swap"), None, None), "typed by its partition type");
    }

    /// A mount through an alias is the device's — whatever alias it went through — and one through
    /// anything else stays as it is.
    #[test]
    fn mounts_through_aliases_are_the_devices_under_them() {
        let geom = parse_confxml(CONFXML).unwrap();
        let sources: Vec<String> = mount_table(&geom).into_iter().map(|m| m.source).collect();
        assert_eq!(sources, ["/dev/ada0p3", "devfs", "/dev/da1p1", "/dev/da1p2"]);
        assert_eq!(
            geom.underlying_node("/dev/ufs/VAULT"),
            Some("/dev/da1p3".into()),
            "alias of an alias"
        );
        assert_eq!(geom.underlying_node("/dev/da0s1"), None);
        assert_eq!(geom.underlying_node("/dev/ufsid/5f3a9c0e12ab34cd"), None, "withered");
    }

    /// A device is mounted through the alias that names it — its label's, else its UFS id's — so
    /// that alias stays while it's mounted; one without is mounted through its own node.
    #[test]
    fn a_device_is_mounted_through_the_alias_that_names_it() {
        let geom = parse_confxml(CONFXML).unwrap();
        let devices = geom.clone().into_devices(&[]);
        let node = |kname: &str| geom.mount_node(get(&devices, kname));
        assert_eq!(node("da0s1"), Path::new("/dev/msdosfs/STICK"));
        assert_eq!(
            node("ada0p1"),
            Path::new("/dev/msdosfs/EFISYS"),
            "its label's, not its GPT name's"
        );
        assert_eq!(node("da1p3"), Path::new("/dev/ufs/VAULT"));
        assert_eq!(node("da1p4"), Path::new("/dev/ufsid/61b0c2d4e5f60718"));
        assert_eq!(node("da1p1"), Path::new("/dev/da1p1"));
    }

    #[test]
    fn what_is_not_a_mesh_is_refused() {
        assert!(parse_confxml("<plist/>").unwrap_err().contains("where <mesh> belongs"));
        assert!(parse_confxml("<mesh>").unwrap_err().contains("isn't closed"));
        assert_eq!(parse_confxml("<mesh/>").unwrap(), Geom::default());
    }

    #[test]
    fn mbr_partuuids_are_linuxs() {
        assert_eq!(
            mbr_partuuid("HD(1,MBR,0x6c9fd5a1,0x800,0x1dcd000)").as_deref(),
            Some("6c9fd5a1-01")
        );
        assert_eq!(mbr_partuuid("HD(4,MBR,0x001234,0x800,0x10)").as_deref(), Some("00001234-04"));
        assert_eq!(mbr_partuuid("HD(1,MBR,00000000,0x800,0x10)"), None, "no signature");
        assert_eq!(
            mbr_partuuid("HD(1,GPT,4a2c0d6e-1d2b-11ef-9c1a-001b21aabbcc,0x28,0x82000)"),
            None
        );
        assert_eq!(mbr_partuuid("garbage"), None);
    }

    #[test]
    fn partition_types_tell_what_they_usually_hold() {
        let types = [
            ("freebsd-ufs", Some("ufs")),
            ("efi", Some("msdosfs")),
            ("fat32lba", Some("msdosfs")),
            ("ms-basic-data", Some("msdosfs")),
            ("linux-data", Some("ext2fs")),
            ("freebsd-swap", Some("swap")),
            ("freebsd-zfs", Some("zfs_member")),
            ("apple-apfs", None),
        ];
        for (part_type, fstype) in types {
            assert_eq!(fstype_for_partition_type(part_type), fstype, "{part_type}");
        }
    }

    #[test]
    fn geom_text_is_utf8_written_bytewise() {
        assert_eq!(geom_text("Donn\u{c3}\u{a9}es"), "Données");
        assert_eq!(geom_text("plain"), "plain");
        assert_eq!(geom_text("\u{e9}"), "\u{fffd}", "a lone byte isn't UTF-8");
    }
}
