//! The systems devmounts runs on, and what differs between them that isn't machinery: where
//! things go by default, the automounters' directories, what `/` allows, whether udisks2 is there.
//! The machinery itself — reading devices and mounts, mounting — is each system's
//! [`System`](crate::System) ([`NativeSystem`](crate::NativeSystem) for the one built for).

use std::path::{Path, PathBuf};

use crate::config::DeviceId;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
compile_error!("devmounts runs on Linux, macOS and FreeBSD");

/// A system devmounts runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Linux,
    MacOs,
    FreeBsd,
}

impl Platform {
    /// The system this program was built for.
    #[cfg(target_os = "linux")]
    pub const CURRENT: Platform = Platform::Linux;
    /// The system this program was built for.
    #[cfg(target_os = "macos")]
    pub const CURRENT: Platform = Platform::MacOs;
    /// The system this program was built for.
    #[cfg(target_os = "freebsd")]
    pub const CURRENT: Platform = Platform::FreeBsd;

    /// Where the `mount` method mounts by default: beside the system's other run-time state.
    /// macOS's is spelled the way its mount table spells it, as `/var` is a symlink there.
    pub fn default_mount_base(self) -> PathBuf {
        PathBuf::from(match self {
            Platform::Linux => "/run/devmounts",
            Platform::MacOs => "/private/var/run/devmounts",
            Platform::FreeBsd => "/var/run/devmounts",
        })
    }

    /// The directories the system's automounters mount devices into.
    pub fn automount_dirs(self) -> &'static [&'static str] {
        match self {
            Platform::Linux => &["/run/media", "/media"],
            Platform::MacOs => &["/Volumes"],
            Platform::FreeBsd => &["/media"],
        }
    }

    /// Why no links can be made in `root` on this system, when none can. The default root, `/`, is
    /// one such on macOS, so a config there names its own.
    pub fn root_refusal(self, root: &Path) -> Option<String> {
        match self {
            Platform::MacOs if root == Path::new("/") => Some(
                "on macOS `/` is read-only, so name a writable directory (names right in `/` can \
                 then be made by hand: symlinks into it, listed in /etc/synthetic.conf)"
                    .into(),
            ),
            Platform::Linux | Platform::MacOs | Platform::FreeBsd => None,
        }
    }

    /// What this system can't show of the device `id` names, said when no device matches it: on
    /// FreeBSD a UUID but UFS's, and a label while the device is mounted under another name; on
    /// macOS a GPT partition's name. Working within that is the config's to do.
    pub fn identifier_hint(self, id: &DeviceId) -> Option<&'static str> {
        match (self, id) {
            (Platform::FreeBsd, DeviceId::Uuid(_)) => Some(
                "FreeBSD shows a UUID only for UFS, its ufsid: name a FAT, ext2/3/4 or NTFS \
                 filesystem by its label, partuuid or partlabel",
            ),
            (Platform::FreeBsd, DeviceId::Label(_)) => Some(
                "FreeBSD hides a device's label while it's mounted under another name; its \
                 partuuid or partlabel finds it either way",
            ),
            (Platform::MacOs, DeviceId::PartLabel(_)) => Some(
                "macOS doesn't show GPT partition names: name the device by its uuid, label or \
                 partuuid",
            ),
            _ => None,
        }
    }

    /// Whether `mount` must be told the filesystem's type (`-t`): macOS's refuses to go on
    /// without one, FreeBSD's takes UFS for granted, Linux's finds it out.
    pub fn mount_needs_type(self) -> bool {
        match self {
            Platform::Linux => false,
            Platform::MacOs | Platform::FreeBsd => true,
        }
    }

    /// Whether GELI, which `geli = true` names, is this system's.
    pub fn has_geli(self) -> bool {
        match self {
            Platform::FreeBsd => true,
            Platform::Linux | Platform::MacOs => false,
        }
    }

    /// Whether udisks2, which `mount_method = "udisks"` names, is this system's.
    pub fn has_udisks(self) -> bool {
        match self {
            Platform::Linux => true,
            Platform::MacOs | Platform::FreeBsd => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::is_plain_absolute;

    const ALL: [Platform; 3] = [Platform::Linux, Platform::MacOs, Platform::FreeBsd];

    /// Every system has places of its own, each a plain absolute path below `/`: what the config
    /// demands of a `mount_base`, and what an automounter's directory needs to count as one.
    #[test]
    fn every_system_has_places_of_its_own() {
        let below_slash = |path: &Path| is_plain_absolute(path) && path.parent().is_some();
        for platform in ALL {
            assert!(below_slash(&platform.default_mount_base()), "{platform:?}");
            let dirs = platform.automount_dirs();
            assert!(!dirs.is_empty(), "{platform:?}");
            assert!(dirs.iter().all(|dir| below_slash(Path::new(dir))), "{platform:?}: {dirs:?}");
        }
    }

    /// Where a system can't show what an identifier names, a miss says so — and only there.
    #[test]
    fn what_a_system_cannot_show_is_said_where_a_device_is_missed() {
        let ids = [
            DeviceId::Uuid("1234-ABCD".into()),
            DeviceId::Label("X".into()),
            DeviceId::PartUuid("6c9fd5a1-01".into()),
            DeviceId::PartLabel("data".into()),
        ];
        let said = |platform: Platform| -> Vec<bool> {
            ids.iter().map(|id| platform.identifier_hint(id).is_some()).collect()
        };
        assert_eq!(said(Platform::Linux), [false, false, false, false]);
        assert_eq!(said(Platform::FreeBsd), [true, true, false, false]);
        assert_eq!(said(Platform::MacOs), [false, false, false, true]);
        let hint = |platform: Platform, id: usize| platform.identifier_hint(&ids[id]).unwrap();
        assert!(hint(Platform::FreeBsd, 0).contains("only for UFS"));
        assert!(hint(Platform::FreeBsd, 1).contains("mounted under another name"));
        assert!(hint(Platform::MacOs, 3).contains("GPT partition names"));
    }

    /// macOS refuses `/` — and says how names can be put there by hand — and nothing else.
    #[test]
    fn only_macos_refuses_slash() {
        let why = Platform::MacOs.root_refusal(Path::new("/")).expect("refused");
        assert!(why.contains("read-only") && why.contains("/etc/synthetic.conf"), "{why}");
        assert_eq!(Platform::MacOs.root_refusal(Path::new("/Users/me/links")), None);
        assert_eq!(Platform::Linux.root_refusal(Path::new("/")), None);
        assert_eq!(Platform::FreeBsd.root_refusal(Path::new("/")), None);
    }
}
