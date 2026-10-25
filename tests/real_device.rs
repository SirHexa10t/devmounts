//! Opt-in round trip against a real device, through the real mount mechanism.
//!
//! ```sh
//! DEVMOUNTS_TEST_LABEL=MY_DRIVE cargo test --test real_device -- --ignored --nocapture
//! ```
//!
//! If the device isn't mounted, it's mounted read-only (nothing is written to it) and
//! unmounted again at the end. Its root is linked into a temporary directory.

mod common;

use std::fs;

use common::tmpdir;
use devmounts::{Colors, Config, Escalation, LinuxSystem, Options};

#[test]
#[ignore = "needs a real device: set DEVMOUNTS_TEST_LABEL and run with --ignored"]
fn real_device_round_trip() {
    let label = std::env::var("DEVMOUNTS_TEST_LABEL").expect("set DEVMOUNTS_TEST_LABEL to a filesystem label");
    let system = LinuxSystem::new();
    let tmp = tmpdir();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let config: Config = format!(
        "settings.root = {root:?}\n[[device]]\nlabel = {label:?}\nmount_options = [\"ro\"]\ndevmount = 'dev'\nmounts = {{ again = '.' }}"
    )
    .parse()
    .unwrap();
    let options = Options { escalation: Some(Escalation::Never), ..Options::default() };

    let before = devmounts::status(&config, &options, &system).unwrap();
    assert!(before.devices[0].error.is_none(), "{}", before.render(Colors::Plain));
    let was_mounted = !before.devices[0].mounted_at.is_empty();

    let applied = devmounts::apply(&config, &options, &system).unwrap();
    println!("{}", applied.render(Colors::Plain));
    assert!(applied.ok());
    let mount = &applied.devices[0].mounted_at[0];
    if !was_mounted {
        assert!(mount.read_only, "a mount made by the test must be read-only");
    }
    let link = root.join("dev");
    assert_eq!(fs::read_link(&link).unwrap(), mount.mount_point);
    assert!(link.is_dir());
    assert_eq!(fs::read_link(root.join("again")).unwrap(), link, "mounts go through the devmount");

    // Put everything back the way it was.
    let removed = devmounts::remove(&config, &Options { unmount: !was_mounted, ..options.clone() }, &system).unwrap();
    println!("{}", removed.render(Colors::Plain));
    assert!(removed.ok());
    assert!(!link.is_symlink() && !root.join("again").is_symlink());
    let after = devmounts::status(&config, &options, &system).unwrap();
    assert_eq!(after.devices[0].mounted_at.is_empty(), !was_mounted);
}
