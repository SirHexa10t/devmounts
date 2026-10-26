# devmounts

Gives each device a root link that follows it wherever the OS mounts it, plus short links
into the device that go *through* that root link:

```
/x      -> /run/media/user/DATA   (the devmount: re-pointed as needed)
/music  -> /x/media/music         (mounts: never need to change)
/photos -> /x/pictures/photos
```

If the device moves (another mount point, another distro, `/media` vs `/run/media`) or is
swapped for another drive with the same layout, only the devmount changes.

```sh
devmounts config.toml             # = devmounts apply config.toml
devmounts status config.toml      # report only
devmounts --dry-run config.toml   # show what apply would do
devmounts remove config.toml [--unmount]
devmounts devices                 # identifiers and mount points of all block devices
```

Options go after the subcommand (`devmounts status --json config.toml`); in front of it
they're refused, not ignored. Every report ends with a line that says how it went —
`✓ everything is in place`, `✓ every link is down`, `✗ 2 problems — see the ✗ lines
above` — and `--json` prints a machine-readable report instead.

Exit codes: 0 no problems, 1 the run went through with problems (the report says which),
2 it couldn't run at all — a bad config or command line, or a machine whose devices or
mount table couldn't be read before anything was done. Whatever goes wrong partway through
a run — a mount, an unmount, the mount table itself — is the problem of the device it
happened to, and the report keeps everything done before it.

`remove` takes a device's links down even while the device isn't connected; the report
says so (`"not_connected": true` in JSON), and it doesn't count as a problem.

## Configuration

```toml
[[device]]
uuid = "11111111-2222-4333-8444-555555555555"   # or label / partuuid / partlabel
devmount = "x"                                  # /x -> wherever the device is mounted
mounts = {                                      # /<name> -> /x/<path>
    music = "media/music",
    photos = "pictures/photos",
}
```

- `devmount` is required; `mounts` is optional (a device may only need its root link).
- Paths are relative to the device root, which is written `.`. For btrfs the root is the
  top-level subvolume, so subvolumes are directories (`@home/me`). `..` is refused.
- Names: letters, digits, `_`, `-` and `.`, starting with a letter, digit or `_`. When the
  links go into `/`, the names the system uses there are refused (`bin`, `usr`, `home`,
  `vmlinuz`, …: [`RESERVED_ROOT_NAMES`](src/config.rs)). Every name may appear only once
  in the file.
- Multi-line `{ … }` tables (TOML 1.1) need commas between entries; `[device.mounts]`
  sub-tables work too.
- Per device: `automount = false` (don't mount it if it isn't mounted), `mount_options`.
  Global defaults go in `[settings]`; [`example.toml`](example.toml) shows every setting.
  `root` and `mount_base` must be absolute paths without `..`.

## What it does with existing things

| At `<root>/<name>`                                             | apply              | remove             |
|----------------------------------------------------------------|--------------------|--------------------|
| nothing                                                        | create             | –                  |
| the right symlink                                              | leave              | remove             |
| dangling symlink                                               | re-point           | remove             |
| symlink into the same device                                   | re-point           | remove             |
| devmount into `/run/media/…`, `/media/…`, `<mount_base>/<dir>` | re-point           | remove             |
| symlink to anything else                                       | refuse (`--force`) | refuse (`--force`) |
| directory, file, anything not a symlink                        | never touched      | never touched      |

A devmount may be re-pointed from where devices get mounted — anywhere inside
`/run/media` or `/media`, or right below `mount_base` — as that's a stale mount point, or
the drive this one was swapped for. Links that point straight at the real path (as a
hand-made link would) count as "into the same device", so they're migrated to go through
the devmount. If a devmount can't be set, its mounts are left alone, since they would lead
somewhere wrong.

A link is only ever replaced or removed in place of a symlink, and atomically
(`renameat2`), so a directory or file that appears at the last moment is still left alone.
On a filesystem that can't do that, the check is made right before a plain rename.

Mount targets must be existing directories on the device: they're checked against the
real location, and refused if a symlink inside the device leads out of it or another mount
covers them.

## Mounting

- `mount_method = "auto"` (default): udisks2 as the calling user, when available and not
  running as root; this mounts where the desktop would, without a password for removable
  drives. A drive udisks2 wants an administrator's password for (an internal one) is
  mounted the `mount` way instead, through the run's privileged helper, so one run never
  asks twice. Without udisks2, or as root, `auto` is `mount`.
- `"mount"`: `mount` into `<mount_base>/<uuid>` (`mount_base` is `/run/devmounts` by
  default), a directory made for it and removed again when it's unmounted.
- `"udisks"`: udisks2 only, for unmounting, unlocking and locking too, its own password
  prompt included.
- If a device is mounted only in parts (e.g. btrfs subvolumes, like a system disk), its
  top level is mounted into `mount_base` (`subvolid=5`) for the devmount.
- `remove --unmount` unmounts the device where it's mounted inside `/run/media` or
  `/media` (an automounter's mounts) or at the `<mount_base>/<uuid>` devmounts makes. Any
  other mount (fstab's, the system's) is left alone, and the report lists it as left
  mounted. For that reason `mount_base` can't be `/`. An automounter's mount comes down
  the way `auto` mounts (udisks2, or `umount` through the helper where udisks2 wants a
  password), devmounts' own with `umount`; each unmount is reported on its own, and one
  failing doesn't stop the others.
- Containers (LUKS, RAID members, single-LV LVM) are followed to the filesystem on top. A
  locked LUKS container is unlocked with udisks2 when the device may be mounted
  (`automount`, and no `--no-mount`); a dry run says it would. udisksctl asks for the
  passphrase itself, on the terminal, so devmounts never sees it. Name such a device by its
  container (its LUKS `uuid`, `partuuid` or `partlabel`): what's inside can't be seen while
  it's locked. `remove --unmount` locks it again once none of its mounts is left; one still
  mounted elsewhere is left unlocked, and the report says where. Other inactive containers
  are reported with a hint (`mdadm --assemble`, `vgchange -ay`).

## Privileges

Links and mount points are made directly when the permissions allow it; a link root of
your own needs nothing more. Steps that meet a permission error (a link in `/`, say), and
`mount` and `umount`, which always need root, go to the run's privileged helper instead.
The helper is devmounts itself, started through `sudo`, `doas` or `run0` (the first one
found, or the `escalate` setting / `--escalate`) when a step first needs it, and kept for
the rest of the run. So a run asks for a password at most once (udisks2's own prompts
aside: a LUKS passphrase, and its authorization with `mount_method = "udisks"`), and not
at all when nothing needs root. If the helper can't be started or the password is refused,
each step that needed it says so (`can't get privileges: …`), and there's no second
prompt. `escalate = "none"` never escalates, and those steps fail with their own error. As
root, everything is done directly.

## As a library

Every command is a function: `apply`, `status`, `remove`, `list_devices`, plus the pieces
(`find_device`, `device_mounts`, `locate`, `check_target`, `link::set_link`, …).

```rust
use devmounts::{Colors, Config, LinuxSystem, Options};

let config = Config::from_path("config.toml")?;
let report = devmounts::apply(&config, &Options::default(), &LinuxSystem::new())?;
print!("{}", report.render(Colors::Plain));
```

A step that needs root starts the running program again, as the privileged helper
(`sudo <program> --devmounts-privileged-helper`). A program using devmounts therefore
calls `devmounts::privileged::try_serve()` first thing in `main` and exits if it returns
`true`, or names a program that does in `Options::helper`.

The command line is part of the library too, as `devmounts::cli` (the default `cli`
feature). Each subcommand's arguments are a clap `Args` struct (`RunArgs`, `RemoveArgs`,
`DevicesArgs`), so another program can offer a subcommand as a command of its own;
`cli::Command::run` carries it out and returns the exit status. `RunArgs::config` is
required on devmounts' own command line, yet an `Option`: a program with a default config
of its own relaxes the requirement and fills it in. Left `None`, the run says the CONFIG
is missing.

Machine access goes through the `System` trait, so the logic runs against fakes in tests.
Build with `default-features = false` to leave out the CLI (and clap).

## Tests

`cargo test` runs everything that doesn't need hardware: fake devices, fixture
sysfs/udev/mountinfo trees, real links in temporary directories, and the binary itself.
The binary reads the machine from `DEVMOUNTS_SYSFS`, `DEVMOUNTS_UDEV_DATA`,
`DEVMOUNTS_MOUNTINFO` and `DEVMOUNTS_DEV` when they're set, and its automounters'
directories from `DEVMOUNTS_AUTOMOUNT_DIRS` (`:`-separated). Some tests run it against
stand-in `udisksctl`, `sudo`, `mount` and `umount` commands and check exactly what it
runs. `cargo test --no-default-features` tests the library without the CLI.

The real-device round trip is opt-in. It mounts read-only if needed, and restores the
previous state:

```sh
DEVMOUNTS_TEST_LABEL=MY_DRIVE cargo test --test real_device -- --ignored --nocapture
```

## Licence

GPL-3.0-only. See [LICENSE](LICENSE).
