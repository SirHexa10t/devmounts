//! Give each device a root link that follows it wherever the OS mounts it, and links into
//! it that go through that root link.
//!
//! A [`Config`] lists devices (by UUID, label, PARTUUID or PARTLABEL). For each, `devmount`
//! names the root link, `<root>/<devmount> -> <where the device is mounted>`, and `mounts`
//! name links into it, `<root>/<name> -> <root>/<devmount>/<path>`. If a device moves or is
//! swapped, only its devmount is re-pointed. [`apply`] finds every device, mounts it if
//! needed and sets up the links; [`status`] reports without changing anything; [`remove`]
//! takes the links down again.
//!
//! ```no_run
//! use devmounts::{Colors, Config, LinuxSystem, Options};
//!
//! let config: Config = r#"
//!     [[device]]
//!     label = "BACKUP"
//!     devmount = "backup"
//!     mount_options = ["ro"]
//!     mounts = { docs = "documents" }
//! "#.parse()?;
//! let report = devmounts::apply(&config, &Options::default(), &LinuxSystem::new())?;
//! print!("{}", report.render(Colors::Plain));
//! # Ok::<(), devmounts::Error>(())
//! ```
//!
//! Everything machine-specific goes through the [`System`] trait ([`LinuxSystem`] for the
//! real machine), so the logic can be driven against other implementations.

#[cfg(feature = "cli")]
pub mod cli;
pub mod config;
pub mod engine;
pub mod error;
pub mod escalate;
pub mod link;
pub mod mount;
pub mod mountinfo;
pub mod privileged;
pub mod report;
pub mod resolve;
pub mod system;
pub mod udev;

pub use config::{Config, DeviceId, DevicePath, DeviceSpec, LinkSpec, MountMethod, Settings};
pub use engine::{DeviceListing, Options, apply, list_devices, remove, status};
pub use error::{Error, Result};
pub use escalate::Escalation;
pub use link::LinkState;
pub use mountinfo::{DevNum, MountEntry};
pub use privileged::Privileges;
pub use report::{
    Colors, Command, DeviceReport, LinkOutcome, LinkReport, MountChange, MountSummary, Report,
};
pub use resolve::{
    Location, MountTree, ResolvedDevice, check_target, device_mounts, find_device, locate,
};
pub use system::{BlockDevice, LinuxSystem, MountRequest, System, UnmountRequest};
