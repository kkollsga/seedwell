//! A minimal, bounded XML tokenizer: just enough for UPnP device descriptions and
//! SOAP replies.
//!
//! It never expands entities beyond the five predefined ones and numeric character
//! references, never resolves DTDs or external references (a `DOCTYPE` with an
//! internal subset is refused), ignores namespaces by reporting local names, and
//! enforces caps on input size, depth, event count and text length. It is lenient
//! about mismatched end tags because IGDs ship sloppy XML.

/// Largest document we tokenize.
pub const MAX_INPUT: usize = 256 * 1024;
/// Deepest nesting.
pub const MAX_DEPTH: usize = 32;
/// Most events per document.
pub const MAX_EVENTS: usize = 20_000;
/// Longest text node we keep.
pub const MAX_TEXT: usize = 4096;

/// A parsing event (local names only, namespace prefixes stripped).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Event<'a> {
    /// `<name ...>`
    Open(&'a str),
    /// `</name>` or the implicit end of `<name/>`
    Close(&'a str),
    /// Non-blank character data, entities decoded.
    Text(String),
}

/// Tokenizer failure.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum XmlError {
    /// Input exceeds [`MAX_INPUT`].
    TooLarge,
    /// Nesting deeper than [`MAX_DEPTH`].
    TooDeep,
    /// More than [`MAX_EVENTS`] events.
    TooManyEvents,
    /// A text node longer than [`MAX_TEXT`].
    TextTooLong,
    /// Not well-formed enough to continue.
    Malformed,
    /// A DOCTYPE with an internal subset.
    Doctype,
}

fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest.find(';').filter(|&e| e <= 10);
        let decoded = end.and_then(|e| {
            let ent = &rest[1..e];
            let c = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => ent
                    .strip_prefix('#')
                    .and_then(|n| match n.strip_prefix(['x', 'X']) {
                        Some(h) => u32::from_str_radix(h, 16).ok(),
                        None => n.parse().ok(),
                    })
                    .and_then(char::from_u32),
            };
            c.map(|c| (c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &rest[e + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Tokenize `input`, calling `f` for each event. Stops at the first error.
pub fn walk<'a>(input: &'a str, mut f: impl FnMut(Event<'a>)) -> Result<(), XmlError> {
    if input.len() > MAX_INPUT {
        return Err(XmlError::TooLarge);
    }
    let mut events = 0usize;
    let mut emit = |e: Event<'a>, f: &mut dyn FnMut(Event<'a>)| -> Result<(), XmlError> {
        events += 1;
        if events > MAX_EVENTS {
            return Err(XmlError::TooManyEvents);
        }
        f(e);
        Ok(())
    };
    let mut stack: Vec<&'a str> = Vec::new();
    let mut pos = 0usize;
    while pos < input.len() {
        let rest = &input[pos..];
        let Some(lt) = rest.find('<') else {
            if let Some(t) = text(rest)? {
                emit(Event::Text(t), &mut f)?;
            }
            break;
        };
        if let Some(t) = text(&rest[..lt])? {
            emit(Event::Text(t), &mut f)?;
        }
        pos += lt;
        let rest = &input[pos..];
        if let Some(r) = rest.strip_prefix("<!--") {
            let end = r.find("-->").ok_or(XmlError::Malformed)?;
            pos += 4 + end + 3;
        } else if let Some(r) = rest.strip_prefix("<![CDATA[") {
            let end = r.find("]]>").ok_or(XmlError::Malformed)?;
            let t = &r[..end];
            if t.len() > MAX_TEXT {
                return Err(XmlError::TextTooLong);
            }
            if !t.trim().is_empty() {
                emit(Event::Text(t.to_owned()), &mut f)?;
            }
            pos += 9 + end + 3;
        } else if rest.starts_with("<?") {
            let end = rest.find("?>").ok_or(XmlError::Malformed)?;
            pos += end + 2;
        } else if rest.starts_with("<!") {
            let end = rest.find('>').ok_or(XmlError::Malformed)?;
            if rest[..end].contains('[') {
                return Err(XmlError::Doctype);
            }
            pos += end + 1;
        } else if let Some(r) = rest.strip_prefix("</") {
            let end = r.find('>').ok_or(XmlError::Malformed)?;
            let name = local(r[..end].trim());
            if let Some(i) = stack.iter().rposition(|&n| n == name) {
                while stack.len() > i {
                    let n = stack.pop().unwrap_or("");
                    emit(Event::Close(n), &mut f)?;
                }
            }
            pos += 2 + end + 1;
        } else {
            // start tag: find the closing '>' outside quotes
            let bytes = rest.as_bytes();
            let mut i = 1;
            let mut quote = 0u8;
            let mut end = None;
            while i < bytes.len() {
                let b = bytes[i];
                if quote != 0 {
                    if b == quote {
                        quote = 0;
                    }
                } else if b == b'"' || b == b'\'' {
                    quote = b;
                } else if b == b'>' {
                    end = Some(i);
                    break;
                }
                i += 1;
            }
            let end = end.ok_or(XmlError::Malformed)?;
            let inner = &rest[1..end];
            let self_closing = inner.ends_with('/');
            let inner = inner.strip_suffix('/').unwrap_or(inner);
            let name_end = inner
                .find(|c: char| c.is_ascii_whitespace())
                .unwrap_or(inner.len());
            let name = local(&inner[..name_end]);
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
            {
                return Err(XmlError::Malformed);
            }
            if stack.len() >= MAX_DEPTH {
                return Err(XmlError::TooDeep);
            }
            emit(Event::Open(name), &mut f)?;
            if self_closing {
                emit(Event::Close(name), &mut f)?;
            } else {
                stack.push(name);
            }
            pos += end + 1;
        }
    }
    while let Some(n) = stack.pop() {
        emit(Event::Close(n), &mut f)?;
    }
    Ok(())
}

fn text(t: &str) -> Result<Option<String>, XmlError> {
    if t.trim().is_empty() {
        return Ok(None);
    }
    if t.len() > MAX_TEXT {
        return Err(XmlError::TextTooLong);
    }
    Ok(Some(decode_entities(t.trim())))
}

/// Escape text for inclusion in an XML body we generate.
pub fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c if c.is_control() => {}
            c => o.push(c),
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(s: &str) -> Result<Vec<String>, XmlError> {
        let mut v = Vec::new();
        walk(s, |e| {
            v.push(match e {
                Event::Open(n) => format!("<{n}"),
                Event::Close(n) => format!("</{n}"),
                Event::Text(t) => format!("'{t}'"),
            })
        })?;
        Ok(v)
    }

    #[test]
    fn basic_document() {
        let x = r#"<?xml version="1.0"?><!-- c --><root xmlns="u:x"><a:b k="v>"> hi &amp; &#65;&#x42; </a:b><e/><![CDATA[<raw>]]></root>"#;
        assert_eq!(
            events(x).unwrap(),
            [
                "<root",
                "<b",
                "'hi & AB'",
                "</b",
                "<e",
                "</e",
                "'<raw>'",
                "</root"
            ]
        );
    }

    #[test]
    fn lenient_about_structure() {
        assert_eq!(
            events("<a><b>x</a>").unwrap(),
            ["<a", "<b", "'x'", "</b", "</a"]
        );
        assert_eq!(events("<a>x</zzz></a>").unwrap(), ["<a", "'x'", "</a"]);
        assert_eq!(events("<a><b>").unwrap(), ["<a", "<b", "</b", "</a"]);
        assert_eq!(events("text only").unwrap(), ["'text only'"]);
        assert_eq!(
            events("<a>&bogus; &amp</a>").unwrap(),
            ["<a", "'&bogus; &amp'", "</a"]
        );
    }

    #[test]
    fn refuses_hostile_input() {
        assert_eq!(
            events("<!DOCTYPE x [<!ENTITY a \"b\">]><x/>"),
            Err(XmlError::Doctype)
        );
        assert_eq!(events("<!DOCTYPE x><x/>").unwrap(), ["<x", "</x"]);
        assert_eq!(events("<a"), Err(XmlError::Malformed));
        assert_eq!(events("<a b='>"), Err(XmlError::Malformed));
        assert_eq!(events("<!-- x"), Err(XmlError::Malformed));
        assert_eq!(events("<![CDATA[x"), Err(XmlError::Malformed));
        assert_eq!(events("<>"), Err(XmlError::Malformed));
        assert_eq!(events("<a b=c d>").unwrap(), ["<a", "</a"]);
        assert_eq!(events(&"<a>".repeat(MAX_DEPTH + 1)), Err(XmlError::TooDeep));
        assert_eq!(
            events(&"<a/>".repeat(MAX_EVENTS)),
            Err(XmlError::TooManyEvents)
        );
        assert_eq!(
            events(&format!("<a>{}</a>", "x".repeat(MAX_TEXT + 1))),
            Err(XmlError::TextTooLong)
        );
        assert_eq!(events(&" ".repeat(MAX_INPUT + 1)), Err(XmlError::TooLarge));
        // billion-laughs style entities are never expanded
        let laughs = "<!DOCTYPE l [<!ENTITY a \"aaaa\">]><l>&a;&a;</l>";
        assert_eq!(events(laughs), Err(XmlError::Doctype));
        for s in [
            "<",
            "</",
            "<!",
            "<?",
            "&",
            "<a/",
            "<a></",
            "\u{0}",
            "<\u{e9}/>",
        ] {
            let _ = events(s);
        }
    }

    #[test]
    fn escaping() {
        assert_eq!(escape("a<b>&\"'\n"), "a&lt;b&gt;&amp;&quot;&apos;");
    }
}
