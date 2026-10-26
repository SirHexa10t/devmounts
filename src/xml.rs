//! A reader for the XML devmounts gets from the system: FreeBSD's GEOM configuration
//! (`kern.geom.confxml`) and macOS's property lists (`diskutil … -plist`).
//!
//! It reads a subset — elements, attributes, and text with the predefined entities and character
//! references; comments, processing instructions and a DOCTYPE without declarations of its own
//! are skipped — and refuses the rest (CDATA, other entities) rather than guess: a document it
//! can't read in full is an error, as a mount table is, since a silently missing device would
//! lead to wrong decisions.

/// How deeply elements may nest: far deeper than what the system writes (GEOM's mesh is 6 deep,
/// `diskutil`'s lists about 8), and shallow enough that reading a tree — and dropping it, which
/// recurses too — never risks the stack.
const MAX_DEPTH: usize = 100;

/// An element: its name, its attributes in document order, and its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub attributes: Vec<(String, String)>,
    pub children: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Element(Element),
    Text(String),
}

impl Element {
    /// The value of the attribute `name`.
    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    /// The child elements, in order; the text between them is passed over.
    pub fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|node| match node {
            Node::Element(element) => Some(element),
            Node::Text(_) => None,
        })
    }

    /// The first child element called `name`.
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.elements().find(|element| element.name == name)
    }

    /// The text right inside, all of it (`<a>x<b/>y</a>` holds `xy`).
    pub fn text(&self) -> String {
        self.children
            .iter()
            .filter_map(|node| match node {
                Node::Text(text) => Some(text.as_str()),
                Node::Element(_) => None,
            })
            .collect()
    }
}

/// Reads a whole document, and returns its root element.
pub fn parse(text: &str) -> Result<Element, String> {
    let mut reader = Reader { text, pos: 0 };
    reader.skip_misc(true)?;
    if !reader.rest().starts_with('<') {
        return Err(reader.error("expected the root element"));
    }
    let root = reader.element(1)?;
    reader.skip_misc(false)?;
    if reader.pos < text.len() {
        return Err(reader.error("more after the root element"));
    }
    Ok(root)
}

struct Reader<'a> {
    text: &'a str,
    pos: usize,
}

impl Reader<'_> {
    fn rest(&self) -> &str {
        &self.text[self.pos..]
    }

    fn error(&self, what: &str) -> String {
        let line = self.text[..self.pos].matches('\n').count() + 1;
        format!("line {line}: {what}")
    }

    /// Moves past `token` if it comes next.
    fn eat(&mut self, token: &str) -> bool {
        let next = self.rest().starts_with(token);
        if next {
            self.pos += token.len();
        }
        next
    }

    fn expect(&mut self, token: &str) -> Result<(), String> {
        if self.eat(token) { Ok(()) } else { Err(self.error(&format!("expected '{token}'"))) }
    }

    /// Moves past whitespace; says whether there was any.
    fn skip_space(&mut self) -> bool {
        let rest = self.rest();
        let skipped = rest.len() - rest.trim_start_matches(is_space).len();
        self.pos += skipped;
        skipped > 0
    }

    /// Moves past the next `end`, which closes `what`.
    fn skip_past(&mut self, end: &str, what: &str) -> Result<(), String> {
        match self.rest().find(end) {
            Some(at) => {
                self.pos += at + end.len();
                Ok(())
            }
            None => Err(self.error(&format!("{what} isn't closed"))),
        }
    }

    /// Whitespace, comments and processing instructions, outside the root element — and in front
    /// of it (`prolog`), a DOCTYPE.
    fn skip_misc(&mut self, prolog: bool) -> Result<(), String> {
        loop {
            self.skip_space();
            if self.eat("<!--") {
                self.skip_past("-->", "a comment")?;
            } else if self.eat("<?") {
                self.skip_past("?>", "a processing instruction")?;
            } else if prolog && self.rest().starts_with("<!DOCTYPE") {
                let end =
                    self.rest().find('>').ok_or_else(|| self.error("the DOCTYPE isn't closed"))?;
                // Declarations of its own could define entities, which this reader doesn't know.
                if self.rest()[..end].contains('[') {
                    return Err(self.error("a DOCTYPE with declarations of its own"));
                }
                self.pos += end + 1;
            } else {
                return Ok(());
            }
        }
    }

    /// The element whose start tag comes next, with everything in it, at nesting level `depth`.
    fn element(&mut self, depth: usize) -> Result<Element, String> {
        if depth > MAX_DEPTH {
            return Err(self.error(&format!("elements nested more than {MAX_DEPTH} deep")));
        }
        let (mut element, empty) = self.start_tag()?;
        if empty {
            return Ok(element);
        }
        loop {
            if self.eat("</") {
                let name = self.name()?;
                self.skip_space();
                self.expect(">")?;
                if name != element.name {
                    let open = &element.name;
                    return Err(self.error(&format!("</{name}> where </{open}> belongs")));
                }
                return Ok(element);
            } else if self.eat("<!--") {
                self.skip_past("-->", "a comment")?;
            } else if self.eat("<?") {
                self.skip_past("?>", "a processing instruction")?;
            } else if self.rest().starts_with("<!") {
                return Err(self.error("CDATA and declarations aren't read"));
            } else if self.rest().starts_with('<') {
                element.children.push(Node::Element(self.element(depth + 1)?));
            } else {
                let Some(end) = self.rest().find('<') else {
                    return Err(self.error(&format!("<{}> isn't closed", element.name)));
                };
                let text = unescape(&self.rest()[..end]).map_err(|why| self.error(&why))?;
                self.pos += end;
                element.children.push(Node::Text(text));
            }
        }
    }

    /// `<name key="value" …>`, or `<name …/>`: the element, and whether it's empty (`/>`).
    fn start_tag(&mut self) -> Result<(Element, bool), String> {
        self.expect("<")?;
        let name = self.name()?;
        let mut attributes: Vec<(String, String)> = Vec::new();
        loop {
            let spaced = self.skip_space();
            for (end, empty) in [("/>", true), (">", false)] {
                if self.eat(end) {
                    return Ok((Element { name, attributes, children: Vec::new() }, empty));
                }
            }
            if !spaced {
                return Err(self.error(&format!("<{name}> runs on without a space or '>'")));
            }
            let key = self.name()?;
            if attributes.iter().any(|(seen, _)| *seen == key) {
                return Err(self.error(&format!("<{name}> has {key} twice")));
            }
            self.skip_space();
            self.expect("=")?;
            self.skip_space();
            let quote = match self.rest().chars().next() {
                Some(quote @ ('"' | '\'')) => quote,
                _ => return Err(self.error(&format!("{key}'s value isn't quoted"))),
            };
            self.pos += 1;
            let Some(end) = self.rest().find(quote) else {
                return Err(self.error(&format!("{key}'s value isn't closed")));
            };
            let raw = &self.rest()[..end];
            if raw.contains('<') {
                return Err(self.error(&format!("'<' in {key}'s value")));
            }
            let value = unescape(raw).map_err(|why| self.error(&why))?;
            self.pos += end + 1;
            attributes.push((key, value));
        }
    }

    fn name(&mut self) -> Result<String, String> {
        let rest = self.rest();
        let name = &rest[..rest.find(|c: char| !is_name_char(c)).unwrap_or(rest.len())];
        match name.chars().next() {
            Some(first) if !(first.is_ascii_digit() || first == '-' || first == '.') => {
                let name = name.to_owned();
                self.pos += name.len();
                Ok(name)
            }
            _ => Err(self.error("expected a name")),
        }
    }
}

fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | ':' | '-' | '.')
}

/// Text with its references replaced: the five predefined entities, `&#…;` and `&#x…;`.
fn unescape(raw: &str) -> Result<String, String> {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let end = after.find(';').ok_or("an '&' without its ';'")?;
        let reference = &after[..end];
        out.push(match reference {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "apos" => '\'',
            "quot" => '"',
            _ => character(reference).ok_or_else(|| format!("unknown reference &{reference};"))?,
        });
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// The character `#65` or `#x41` stands for.
fn character(reference: &str) -> Option<char> {
    let code = match reference.strip_prefix("#x") {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => reference.strip_prefix('#')?.parse().ok()?,
    };
    char::from_u32(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(name: &str, attributes: &[(&str, &str)], children: Vec<Node>) -> Node {
        Node::Element(Element {
            name: name.into(),
            attributes: attributes.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            children,
        })
    }

    fn text(text: &str) -> Node {
        Node::Text(text.into())
    }

    /// A document is its root element: elements, attributes in either quotes, text and empty
    /// elements, exactly as written — the whitespace between elements included.
    #[test]
    fn a_document_reads_as_its_tree() {
        let doc = parse("<a x=\"1\" y='two'><b/>\n  <c>hi <d>there</d>!</c></a>").unwrap();
        let expected = element(
            "a",
            &[("x", "1"), ("y", "two")],
            vec![
                element("b", &[], vec![]),
                text("\n  "),
                element(
                    "c",
                    &[],
                    vec![text("hi "), element("d", &[], vec![text("there")]), text("!")],
                ),
            ],
        );
        assert_eq!(Node::Element(doc), expected);
    }

    /// The prolog — a declaration, comments, a DOCTYPE — and comments and processing instructions
    /// anywhere are passed over, as a property list from `diskutil` has them.
    #[test]
    fn prolog_comments_and_instructions_are_passed_over() {
        let doc = parse(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <!-- before -->\n<plist version=\"1.0\"><!-- inside --><true/><?pi x?></plist>\n<!-- after -->\n",
        )
        .unwrap();
        assert_eq!(doc.name, "plist");
        assert_eq!(doc.attribute("version"), Some("1.0"));
        assert_eq!(doc.children, vec![element("true", &[], vec![])]);
    }

    /// The predefined entities and character references, decimal and hexadecimal, in text and in
    /// attribute values alike; `>` needs no escaping in text.
    #[test]
    fn references_are_replaced() {
        let doc =
            parse("<a v='&lt;&amp;&gt;&apos;&quot;'>&#65;&#x42;&#xE9;&#x1F600; > ok</a>").unwrap();
        assert_eq!(doc.attribute("v"), Some("<&>'\""));
        assert_eq!(doc.text(), "ABé😀 > ok");
    }

    #[test]
    fn helpers_find_what_is_inside() {
        let doc = parse("<a>x<b n='1'/>y<c/><b n='2'/>z</a>").unwrap();
        assert_eq!(doc.text(), "xyz", "only the text right inside");
        let names: Vec<&str> = doc.elements().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["b", "c", "b"]);
        assert_eq!(doc.child("b").and_then(|b| b.attribute("n")), Some("1"), "the first one");
        assert!(doc.child("d").is_none() && doc.attribute("n").is_none());
    }

    /// Whatever isn't well-formed, or is beyond the subset, is refused — saying where.
    #[test]
    fn what_cannot_be_read_in_full_is_refused() {
        let refused = [
            ("", "expected the root element"),
            ("text", "expected the root element"),
            ("<a>", "<a> isn't closed"),
            ("<a><b></a>", "</a> where </b> belongs"),
            ("<a></a><b/>", "more after the root element"),
            ("<a>&nbsp;</a>", "unknown reference &nbsp;"),
            ("<a>&#xZZ;</a>", "unknown reference &#xZZ;"),
            ("<a>& b</a>", "an '&' without its ';'"),
            ("<a><![CDATA[x]]></a>", "CDATA"),
            ("<!DOCTYPE a [<!ENTITY e 'x'>]><a/>", "declarations of its own"),
            ("<a x='1' x='2'/>", "has x twice"),
            ("<a x=1/>", "isn't quoted"),
            ("<a x='1/>", "isn't closed"),
            ("<a x='<'/>", "'<' in x's value"),
            ("<a x='1'y='2'/>", "runs on"),
            ("<1a/>", "expected a name"),
            ("<a><!-- open</a>", "a comment isn't closed"),
        ];
        for (doc, why) in refused {
            let err = parse(doc).expect_err(doc);
            assert!(err.contains(why), "{doc:?}: {err}");
        }
        let err = parse("<a>\n<b>\n</c></a>").unwrap_err();
        assert!(err.starts_with("line 3: "), "{err}");
    }

    /// Nesting is bounded, so no document can exhaust the stack — reading it or dropping it.
    #[test]
    fn nesting_is_bounded() {
        let nested = |depth: usize| format!("{}{}", "<a>".repeat(depth), "</a>".repeat(depth));
        assert!(parse(&nested(MAX_DEPTH)).is_ok());
        let err = parse(&nested(MAX_DEPTH + 1)).unwrap_err();
        assert!(err.contains("nested more than"), "{err}");
        assert!(parse(&nested(1_000_000)).is_err(), "refused without a crash");
    }
}
