//! Property lists in their XML form, as macOS tools print them (`diskutil … -plist`), read
//! with [`crate::xml`].

use crate::xml::{self, Element};

/// A property list value. A dictionary keeps its keys in document order.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Dict(Vec<(String, Value)>),
    Array(Vec<Value>),
    String(String),
    Integer(i64),
    Real(f64),
    Bool(bool),
    /// `<date>`, as written (ISO 8601).
    Date(String),
    /// `<data>`: its base64, without the whitespace, undecoded — nothing here needs the bytes.
    Data(String),
}

impl Value {
    /// The value of `key`, when this is a dictionary that has it.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Dict(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }
}

/// Reads a property list: a `<plist>` document holding one value.
pub fn parse(text: &str) -> Result<Value, String> {
    let root = xml::parse(text)?;
    if root.name != "plist" {
        return Err(format!("<{}> where <plist> belongs", root.name));
    }
    only_elements(&root)?;
    let mut values = root.elements();
    match (values.next(), values.next()) {
        (Some(value), None) => value_of(value),
        _ => Err("a <plist> holds exactly one value".into()),
    }
}

fn value_of(element: &Element) -> Result<Value, String> {
    Ok(match element.name.as_str() {
        "dict" => dict(element)?,
        "array" => {
            only_elements(element)?;
            Value::Array(element.elements().map(value_of).collect::<Result<_, _>>()?)
        }
        "string" => Value::String(scalar(element)?),
        "integer" => Value::Integer(number(element)?),
        "real" => Value::Real(number(element)?),
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "date" => Value::Date(scalar(element)?.trim().to_owned()),
        "data" => Value::Data(scalar(element)?.split_whitespace().collect()),
        other => return Err(format!("<{other}> isn't a property list value")),
    })
}

/// `<key>…</key>` and a value, pair after pair.
fn dict(element: &Element) -> Result<Value, String> {
    only_elements(element)?;
    let mut entries = Vec::new();
    let mut items = element.elements();
    while let Some(key) = items.next() {
        if key.name != "key" {
            return Err(format!("<{}> where a <key> belongs in a <dict>", key.name));
        }
        let key = scalar(key)?;
        let value = items.next().ok_or_else(|| format!("<key>{key}</key> has no value"))?;
        entries.push((key, value_of(value)?));
    }
    Ok(Value::Dict(entries))
}

/// A container's content: elements, with nothing but whitespace between them.
fn only_elements(element: &Element) -> Result<(), String> {
    let text = element.text();
    match text.trim() {
        "" => Ok(()),
        stray => Err(format!("text in a <{}>: {stray:?}", element.name)),
    }
}

/// A scalar's text, which no element may interrupt.
fn scalar(element: &Element) -> Result<String, String> {
    match element.elements().next() {
        None => Ok(element.text()),
        Some(inner) => Err(format!("<{}> in a <{}>", inner.name, element.name)),
    }
}

fn number<T: std::str::FromStr>(element: &Element) -> Result<T, String> {
    let text = scalar(element)?;
    text.trim().parse().map_err(|_| format!("<{0}>{text}</{0}> isn't a number", element.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
        <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n";

    fn plist(body: &str) -> Result<Value, String> {
        parse(&format!("{HEADER}<plist version=\"1.0\">\n{body}\n</plist>\n"))
    }

    /// Every kind of value, nested, as a property list writes it.
    #[test]
    fn every_kind_of_value_is_read() {
        let value = plist(
            "<dict>\n\t<key>Name</key>\n\t<string>  My &amp; Disk </string>\n\
             \t<key>Size</key>\n\t<integer>-512</integer>\n\
             \t<key>Ratio</key>\n\t<real>0.5</real>\n\
             \t<key>Internal</key>\n\t<true/>\n\
             \t<key>Removable</key>\n\t<false/>\n\
             \t<key>When</key>\n\t<date>2026-01-02T03:04:05Z</date>\n\
             \t<key>Blob</key>\n\t<data>\n\tAAEC\n\tAw==\n\t</data>\n\
             \t<key>Empty</key>\n\t<string></string>\n\
             \t<key>Parts</key>\n\t<array>\n\t\t<dict/>\n\t\t<string>x</string>\n\t</array>\n\
             </dict>",
        )
        .unwrap();
        let expected = Value::Dict(vec![
            ("Name".into(), Value::String("  My & Disk ".into())),
            ("Size".into(), Value::Integer(-512)),
            ("Ratio".into(), Value::Real(0.5)),
            ("Internal".into(), Value::Bool(true)),
            ("Removable".into(), Value::Bool(false)),
            ("When".into(), Value::Date("2026-01-02T03:04:05Z".into())),
            ("Blob".into(), Value::Data("AAECAw==".into())),
            ("Empty".into(), Value::String(String::new())),
            ("Parts".into(), Value::Array(vec![Value::Dict(vec![]), Value::String("x".into())])),
        ]);
        assert_eq!(value, expected);
    }

    #[test]
    fn values_are_looked_up_by_key_and_kind() {
        let value = plist(
            "<dict><key>A</key><string>a</string><key>B</key><true/><key>C</key><array/></dict>",
        )
        .unwrap();
        assert_eq!(value.get("A").and_then(Value::as_str), Some("a"));
        assert_eq!(value.get("B").and_then(Value::as_bool), Some(true));
        assert_eq!(value.get("C").and_then(Value::as_array).map(<[Value]>::len), Some(0));
        assert_eq!(value.get("A").and_then(Value::as_bool), None, "the wrong kind is no value");
        assert_eq!(value.get("Z"), None);
        assert_eq!(Value::String("s".into()).get("A"), None, "only a dictionary has keys");
    }

    /// What isn't a property list, or not one this reader can be sure of, is refused.
    #[test]
    fn malformed_lists_are_refused() {
        let refused = [
            ("<dict><key>A</key></dict>", "<key>A</key> has no value"),
            ("<dict><string>a</string></dict>", "where a <key> belongs"),
            ("<dict>stray<key>A</key><true/></dict>", "text in a <dict>"),
            ("<array><true/>stray</array>", "text in a <array>"),
            ("<integer>12a</integer>", "isn't a number"),
            ("<string>a<b/>c</string>", "<b> in a <string>"),
            ("<set/>", "<set> isn't a property list value"),
            ("<true/><false/>", "exactly one value"),
            ("", "exactly one value"),
        ];
        for (body, why) in refused {
            let err = plist(body).expect_err(body);
            assert!(err.contains(why), "{body:?}: {err}");
        }
        assert!(parse("<dict/>").unwrap_err().contains("where <plist> belongs"));
        assert!(parse("<plist><dict>").unwrap_err().contains("isn't closed"), "XML errors too");
    }
}
