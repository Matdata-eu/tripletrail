//! JSON: objects and arrays are containers, object members are keys, array items are slots.
//! Strings are `xsd:string` and booleans `xsd:boolean`. Numbers keep the numeral as written in
//! the file, as `xsd:double` when it has an exponent and `xsd:decimal` otherwise, as SPARQL
//! Anything does (#378). Nulls give no triple, or `xyz:null` with
//! `json.include-null-values=true`.
//!
//! With `geo=true`, GeoJSON geometries also get `geo:asWKT` and features
//! `geo:hasGeometry` (see [`crate::geo`]).
//!
//! The file is read by the small parser below rather than `serde_json`, whose numbers are
//! parsed to `f64` (losing the written form, and off in the last digit without its
//! `float_roundtrip` feature).

use crate::FxError;
use crate::geo;
use crate::options::Options;
use crate::tree::{Sink, escape_iri};
use oxigraph::model::vocab::xsd;
use oxigraph::model::{NamedNode, NamedOrBlankNode, Term};
use std::borrow::Cow;

pub(crate) fn triplify(
    bytes: &[u8],
    options: &Options,
    sink: &mut Sink<'_>,
) -> Result<(), FxError> {
    let value = Parser::parse(bytes)?;
    let nulls = options.flag("json.include-null-values", false)?;
    let geo = if options.flag("geo", false)? {
        Some(Geo {
            crs: geo::crs_prefix(options)?,
            as_wkt: sink.predicate(geo::AS_WKT.to_owned()),
            has_geometry: sink.predicate(geo::HAS_GEOMETRY.to_owned()),
        })
    } else {
        None
    };
    let mut json = Json { nulls, geo };
    match &value {
        Value::Object(_) | Value::Array(_) => {
            let root = sink.root();
            json.members(sink, &root, &value, "");
        }
        scalar => {
            let root = sink.root();
            if let Some(o) = json.value(sink, scalar, "_1") {
                sink.slot(&root, 1, o);
            }
        }
    }
    Ok(())
}

struct Json {
    nulls: bool,
    /// With `geo=true`: GeoJSON geometries get WKT.
    geo: Option<Geo>,
}

struct Geo {
    crs: String,
    /// The predicates, when the pattern can match them.
    as_wkt: Option<NamedNode>,
    has_geometry: Option<NamedNode>,
}

impl Json {
    /// The triples of the members or items of `value`, a container at `path`.
    fn members(&mut self, sink: &mut Sink<'_>, c: &NamedOrBlankNode, value: &Value, path: &str) {
        match value {
            Value::Object(members) => {
                let feature = self.geo.is_some() && member(value, "type") == Some("Feature");
                for (k, v) in members {
                    let child = || child_path(path, &key_path(k));
                    if let Some(o) = self.value(sink, v, &lazy(sink, child)) {
                        if feature
                            && k == "geometry"
                            && matches!(v, Value::Object(_))
                            && let Some(p) = self.geo.as_ref().and_then(|g| g.has_geometry.clone())
                        {
                            sink.emit(c, &p, o.clone());
                        }
                        sink.key(c, k, o);
                    }
                }
                if let Some(g) = &self.geo
                    && let Some(p) = &g.as_wkt
                    && let Some(wkt) = geometry_wkt(value)
                {
                    let literal = oxigraph::model::Literal::new_typed_literal(
                        format!("{}{wkt}", g.crs),
                        NamedNode::new_unchecked(geo::WKT_LITERAL),
                    );
                    sink.emit(c, p, literal.into());
                }
            }
            Value::Array(items) => {
                for (i, v) in items.iter().enumerate() {
                    let child = || child_path(path, &format!("_{}", i + 1));
                    if let Some(o) = self.value(sink, v, &lazy(sink, child)) {
                        sink.slot(c, i + 1, o);
                    }
                }
            }
            _ => {}
        }
    }

    /// The node for `value` at `path`, after the triples of its members.
    fn value(&mut self, sink: &mut Sink<'_>, value: &Value, path: &str) -> Option<Term> {
        match value {
            Value::Null => self
                .nulls
                .then(|| NamedNode::new_unchecked(format!("{}null", crate::XYZ)).into()),
            Value::Bool(b) => Some(oxigraph::model::Literal::from(*b).into()),
            Value::Number(n) => {
                let datatype = if n.contains(['e', 'E']) {
                    xsd::DOUBLE
                } else {
                    xsd::DECIMAL
                };
                sink.typed(n, datatype)
            }
            Value::String(s) => sink.string(s),
            Value::Object(_) | Value::Array(_) => {
                let c = sink.container(|| path.to_owned());
                self.members(sink, &c, value, path);
                Some(c.into())
            }
        }
    }
}

/// The string member `key` of an object.
fn member<'v>(value: &'v Value<'_>, key: &str) -> Option<&'v str> {
    let Value::Object(members) = value else {
        return None;
    };
    members.iter().find_map(|(k, v)| match v {
        Value::String(s) if k == key => Some(s.as_ref()),
        _ => None,
    })
}

/// The WKT of a GeoJSON geometry object.
fn geometry_wkt(value: &Value<'_>) -> Option<String> {
    let kind = member(value, "type")?;
    let Value::Object(members) = value else {
        return None;
    };
    let get = |key: &str| members.iter().find(|(k, _)| k == key).map(|(_, v)| v);
    if kind == "GeometryCollection" {
        let Value::Array(items) = get("geometries")? else {
            return None;
        };
        if items.is_empty() {
            return Some("GEOMETRYCOLLECTION EMPTY".into());
        }
        let parts: Option<Vec<String>> = items.iter().map(geometry_wkt).collect();
        return Some(format!("GEOMETRYCOLLECTION({})", parts?.join(", ")));
    }
    geo::geojson_wkt(kind, Some(&coords(get("coordinates")?)))
}

fn coords<'a>(value: &'a Value<'_>) -> geo::Coords<'a> {
    match value {
        Value::Number(n) => geo::Coords::Number(n),
        Value::Array(items) => geo::Coords::List(items.iter().map(coords).collect()),
        _ => geo::Coords::Other,
    }
}

/// The path of a child, only worked out when containers are IRIs.
fn lazy(sink: &Sink<'_>, path: impl FnOnce() -> String) -> String {
    if sink.common.blank_nodes {
        String::new()
    } else {
        path()
    }
}

fn child_path(parent: &str, segment: &str) -> String {
    if parent.is_empty() {
        segment.to_owned()
    } else {
        format!("{parent}/{segment}")
    }
}

fn key_path(key: &str) -> String {
    let mut s = String::new();
    escape_iri(key, &mut s);
    s
}

/// A JSON value, its strings and numerals borrowed from the file where they need no unescaping.
#[derive(Debug, PartialEq)]
enum Value<'a> {
    Null,
    Bool(bool),
    /// The numeral as written.
    Number(&'a str),
    String(Cow<'a, str>),
    /// The members in the order of the file (a repeated key gives a triple for each).
    Object(Vec<(Cow<'a, str>, Value<'a>)>),
    Array(Vec<Value<'a>>),
}

/// A recursive-descent parser of RFC 8259 JSON.
struct Parser<'a> {
    text: &'a str,
    pos: usize,
    depth: usize,
}

/// Deeper nesting is refused rather than overflowing the stack.
const MAX_DEPTH: usize = 512;

impl<'a> Parser<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Value<'a>, FxError> {
        let text = std::str::from_utf8(bytes)
            .map_err(|e| FxError::Parse(format!("JSON: not UTF-8: {e}")))?;
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut p = Parser {
            text,
            pos: 0,
            depth: 0,
        };
        let value = p.value()?;
        p.space();
        if p.pos < p.text.len() {
            return Err(p.error("trailing characters"));
        }
        Ok(value)
    }

    fn error(&self, what: &str) -> FxError {
        let before = &self.text[..self.pos.min(self.text.len())];
        let line = before.matches('\n').count() + 1;
        let column = before.len() - before.rfind('\n').map_or(0, |i| i + 1) + 1;
        FxError::Parse(format!("JSON: {what} at line {line} column {column}"))
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.pos).copied()
    }

    fn space(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.peek() {
            self.pos += 1;
        }
    }

    fn expect(&mut self, b: u8) -> Result<(), FxError> {
        self.space();
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.error(&format!("expected `{}`", b as char)))
        }
    }

    fn literal(&mut self, word: &str, value: Value<'a>) -> Result<Value<'a>, FxError> {
        if self.text[self.pos..].starts_with(word) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(self.error("unexpected character"))
        }
    }

    fn value(&mut self) -> Result<Value<'a>, FxError> {
        self.space();
        match self.peek() {
            Some(b'{') => self.nested(Self::object),
            Some(b'[') => self.nested(Self::array),
            Some(b'"') => Ok(Value::String(self.string()?)),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'n') => self.literal("null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("unexpected character")),
            None => Err(self.error("unexpected end")),
        }
    }

    fn nested(
        &mut self,
        f: fn(&mut Self) -> Result<Value<'a>, FxError>,
    ) -> Result<Value<'a>, FxError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error("nesting too deep"));
        }
        let v = f(self);
        self.depth -= 1;
        v
    }

    fn object(&mut self) -> Result<Value<'a>, FxError> {
        self.pos += 1;
        let mut members = Vec::new();
        self.space();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(members));
        }
        loop {
            self.space();
            if self.peek() != Some(b'"') {
                return Err(self.error("expected a key"));
            }
            let key = self.string()?;
            self.expect(b':')?;
            members.push((key, self.value()?));
            self.space();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(members));
                }
                _ => return Err(self.error("expected `,` or `}`")),
            }
        }
    }

    fn array(&mut self) -> Result<Value<'a>, FxError> {
        self.pos += 1;
        let mut items = Vec::new();
        self.space();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.space();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(self.error("expected `,` or `]`")),
            }
        }
    }

    /// A numeral: `-? (0 | [1-9][0-9]*) (. [0-9]+)? ([eE] [+-]? [0-9]+)?`.
    fn number(&mut self) -> Result<Value<'a>, FxError> {
        let start = self.pos;
        let bytes = self.text.as_bytes();
        let digits = |p: &mut Self| {
            let from = p.pos;
            while bytes.get(p.pos).is_some_and(u8::is_ascii_digit) {
                p.pos += 1;
            }
            p.pos > from
        };
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                digits(self);
            }
            _ => return Err(self.error("invalid number")),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !digits(self) {
                return Err(self.error("invalid number"));
            }
        }
        if let Some(b'e' | b'E') = self.peek() {
            self.pos += 1;
            if let Some(b'+' | b'-') = self.peek() {
                self.pos += 1;
            }
            if !digits(self) {
                return Err(self.error("invalid number"));
            }
        }
        Ok(Value::Number(&self.text[start..self.pos]))
    }

    /// A string, at its opening quote; borrowed when it has no escapes.
    fn string(&mut self) -> Result<Cow<'a, str>, FxError> {
        self.pos += 1;
        let start = self.pos;
        let bytes = self.text.as_bytes();
        loop {
            match bytes.get(self.pos) {
                Some(b'"') => {
                    let s = &self.text[start..self.pos];
                    self.pos += 1;
                    return Ok(Cow::Borrowed(s));
                }
                Some(b'\\') => break,
                Some(0..=0x1f) => return Err(self.error("control character in a string")),
                Some(_) => self.pos += 1,
                None => return Err(self.error("unterminated string")),
            }
        }
        let mut out = String::from(&self.text[start..self.pos]);
        loop {
            match bytes.get(self.pos) {
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(Cow::Owned(out));
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let c = match bytes.get(self.pos) {
                        Some(b'"') => '"',
                        Some(b'\\') => '\\',
                        Some(b'/') => '/',
                        Some(b'b') => '\u{8}',
                        Some(b'f') => '\u{c}',
                        Some(b'n') => '\n',
                        Some(b'r') => '\r',
                        Some(b't') => '\t',
                        Some(b'u') => {
                            self.pos += 1;
                            let hi = self.hex4()?;
                            let c = if (0xD800..0xDC00).contains(&hi)
                                && self.text[self.pos..].starts_with("\\u")
                            {
                                let save = self.pos;
                                self.pos += 2;
                                let lo = self.hex4()?;
                                if (0xDC00..0xE000).contains(&lo) {
                                    char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00))
                                } else {
                                    self.pos = save;
                                    None
                                }
                            } else {
                                char::from_u32(hi)
                            };
                            // A lone surrogate becomes U+FFFD, as in most readers.
                            out.push(c.unwrap_or('\u{fffd}'));
                            continue;
                        }
                        _ => return Err(self.error("invalid escape")),
                    };
                    out.push(c);
                    self.pos += 1;
                }
                Some(0..=0x1f) => return Err(self.error("control character in a string")),
                Some(_) => {
                    let rest = &self.text[self.pos..];
                    let end = rest
                        .find(['"', '\\'])
                        .ok_or_else(|| self.error("unterminated string"))?;
                    let run = &rest[..end];
                    if run.bytes().any(|b| b < 0x20) {
                        return Err(self.error("control character in a string"));
                    }
                    out.push_str(run);
                    self.pos += end;
                }
                None => return Err(self.error("unterminated string")),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, FxError> {
        let h = self
            .text
            .get(self.pos..self.pos + 4)
            .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| self.error("invalid \\u escape"))?;
        self.pos += 4;
        Ok(u32::from_str_radix(h, 16).expect("hex digits"))
    }
}

#[cfg(test)]
mod tests {
    use super::{Parser, Value};
    use std::borrow::Cow;

    fn parse(s: &str) -> Value<'_> {
        Parser::parse(s.as_bytes()).unwrap()
    }

    #[test]
    fn numerals_are_kept_as_written() {
        let text =
            r#"[4.6, 12.50, 31.26999999999999957, 9.187028266000027, 1e-7, -0, 100, 7.1E+3]"#;
        let Value::Array(items) = parse(text) else {
            panic!()
        };
        let numerals: Vec<_> = items
            .iter()
            .map(|v| match v {
                Value::Number(n) => *n,
                _ => panic!(),
            })
            .collect();
        assert_eq!(
            numerals,
            [
                "4.6",
                "12.50",
                "31.26999999999999957",
                "9.187028266000027",
                "1e-7",
                "-0",
                "100",
                "7.1E+3"
            ]
        );
    }

    #[test]
    fn strings_objects_and_errors() {
        assert_eq!(
            parse(r#" {"a\"b": "x\u00e9\ud83d\ude00\n", "a": [true, null, {}], "a": []} "#),
            Value::Object(vec![
                (
                    Cow::Borrowed("a\"b"),
                    Value::String("x\u{e9}\u{1f600}\n".into())
                ),
                (
                    Cow::Borrowed("a"),
                    Value::Array(vec![Value::Bool(true), Value::Null, Value::Object(vec![])])
                ),
                (Cow::Borrowed("a"), Value::Array(vec![])),
            ])
        );
        assert_eq!(parse("\u{feff}\"s\""), Value::String("s".into()));
        for bad in [
            "",
            "01",
            "1.",
            "-",
            "1e",
            "[1,]",
            "{\"a\" 1}",
            "\"a",
            "[1] x",
            "\"\t\"",
            "\"\\x\"",
            "tru",
            "{1: 2}",
            ".5",
            "+1",
        ] {
            assert!(Parser::parse(bad.as_bytes()).is_err(), "{bad:?}");
        }
        let deep = "[".repeat(10_000);
        assert!(Parser::parse(deep.as_bytes()).is_err());
    }
}
