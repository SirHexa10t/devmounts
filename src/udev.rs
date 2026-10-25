//! Reading device identities from the udev database (`/run/udev/data/b<major>:<minor>`).
//!
//! The same files are written by systemd-udevd and eudev. Only `E:` (property) lines are
//! used. Values in `*_ENC` properties and `ID_PART_ENTRY_NAME` are `\xNN`-escaped by
//! libblkid's `blkid_encode_string`.

use std::collections::HashMap;

/// Filesystem and partition identifiers of one block device.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    pub fstype: Option<String>,
    pub uuid: Option<String>,
    pub label: Option<String>,
    pub partuuid: Option<String>,
    pub partlabel: Option<String>,
}

impl Identity {
    pub fn is_empty(&self) -> bool {
        *self == Identity::default()
    }
}

/// Collects the `E:KEY=value` properties of a udev database file.
pub fn parse_properties(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| line.strip_prefix("E:"))
        .filter_map(|prop| prop.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

/// Picks the identifiers out of udev properties, preferring the losslessly encoded variants.
pub fn identity(props: &HashMap<String, String>) -> Identity {
    let get = |key: &str| props.get(key).filter(|v| !v.is_empty());
    let encoded = |enc: &str, plain: &str| get(enc).map(|v| decode(v)).or_else(|| get(plain).cloned());
    Identity {
        fstype: get("ID_FS_TYPE").cloned(),
        uuid: encoded("ID_FS_UUID_ENC", "ID_FS_UUID"),
        label: encoded("ID_FS_LABEL_ENC", "ID_FS_LABEL"),
        partuuid: get("ID_PART_ENTRY_UUID").cloned(),
        partlabel: get("ID_PART_ENTRY_NAME").map(|v| decode(v)),
    }
}

/// Decodes `\xNN` escapes; the result is interpreted as UTF-8 (lossily).
pub fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && bytes.get(i + 1) == Some(&b'x')
            && let Some(hex) = bytes.get(i + 2..i + 4)
            && let Ok(hex) = std::str::from_utf8(hex)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 4;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Encodes a string the way udev names `/dev/disk/by-label/*` links (for messages/tests).
pub fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if c.is_ascii_alphanumeric() || "#+-.:=@_".contains(c) || !c.is_ascii() {
            out.push(c);
        } else {
            let mut buf = [0; 4];
            for byte in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("\\x{byte:02x}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // The shape of /run/udev/data/b8:17 for a USB drive, abridged; every value is made up.
    const DB: &str = "\
S:disk/by-uuid/11111111-2222-4333-8444-555555555555
S:disk/by-label/BACKUP
I:12345678901
E:ID_MODEL_ENC=Example\\x20Drive\\x202TB
E:ID_FS_LABEL=BACKUP
E:ID_FS_LABEL_ENC=BACKUP
E:ID_FS_UUID=11111111-2222-4333-8444-555555555555
E:ID_FS_UUID_ENC=11111111-2222-4333-8444-555555555555
E:ID_FS_TYPE=btrfs
E:ID_FS_USAGE=filesystem
E:ID_PART_ENTRY_UUID=66666666-7777-4888-9999-aaaaaaaaaaaa
E:ID_PART_ENTRY_NAME=
G:systemd
Q:systemd
V:1
";

    #[test]
    fn reads_identity() {
        let props = parse_properties(DB);
        assert_eq!(props["ID_MODEL_ENC"], r"Example\x20Drive\x202TB");
        let id = identity(&props);
        assert_eq!(id.fstype.as_deref(), Some("btrfs"));
        assert_eq!(id.uuid.as_deref(), Some("11111111-2222-4333-8444-555555555555"));
        assert_eq!(id.label.as_deref(), Some("BACKUP"));
        assert_eq!(id.partuuid.as_deref(), Some("66666666-7777-4888-9999-aaaaaaaaaaaa"));
        assert_eq!(id.partlabel, None, "empty values count as missing");
    }

    #[test]
    fn decodes_escaped_labels() {
        let props = parse_properties(
            "E:ID_FS_LABEL=Example_Drive\nE:ID_FS_LABEL_ENC=Example\\x20Drive\nE:ID_PART_ENTRY_NAME=EFI\\x20system\\x20partition\n",
        );
        let id = identity(&props);
        assert_eq!(id.label.as_deref(), Some("Example Drive"), "the _ENC value wins over the mangled one");
        assert_eq!(id.partlabel.as_deref(), Some("EFI system partition"));
    }

    #[test]
    fn falls_back_to_plain_values() {
        let id = identity(&parse_properties("E:ID_FS_LABEL=DATA\nE:ID_FS_UUID=1234-ABCD\n"));
        assert_eq!(id.label.as_deref(), Some("DATA"));
        assert_eq!(id.uuid.as_deref(), Some("1234-ABCD"));
    }

    #[test]
    fn decode_edge_cases() {
        assert_eq!(decode(r"a\x2fb"), "a/b");
        assert_eq!(decode(r"back\x5cslash"), r"back\slash");
        assert_eq!(decode(r"\xc3\xa9t\xc3\xa9"), "été");
        assert_eq!(decode("été"), "été");
        assert_eq!(decode(r"bad\xZZ"), r"bad\xZZ");
        assert_eq!(decode(r"short\x4"), r"short\x4");
        assert_eq!(decode(r"\x"), r"\x");
        assert_eq!(decode(""), "");
    }

    #[test]
    fn encode_round_trips() {
        for label in ["Example Drive", "a/b", r"back\slash", "été 2", "#+-.:=@_"] {
            assert_eq!(decode(&encode(label)), label);
        }
        assert_eq!(encode("Example Drive"), r"Example\x20Drive");
    }

    #[test]
    fn ignores_other_lines() {
        let props = parse_properties("S:disk/by-label/X\nE:NOEQUALS\nW:3\nE:A=b=c\n");
        assert_eq!(props.len(), 1);
        assert_eq!(props["A"], "b=c");
        assert!(identity(&props).is_empty());
    }
}
