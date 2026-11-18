//! Jena's binary RDF (#831): RDF Thrift and RDF Protobuf, for graphs, datasets and SPARQL
//! results, written by hand on Jena's schemas (`BinaryRDF.thrift`, `binary-rdf.proto`).
//!
//! - **Thrift**: the compact protocol, one `RDF_StreamRow` after another (results: one
//!   `RDF_VarTuple`, then one `RDF_DataTuple` per row).
//! - **Protobuf**: each `RDF_StreamRow` (results: the `RDF_VarTuple`, then each
//!   `RDF_DataTuple`) prefixed by its length, as Java's `writeDelimitedTo` writes them.
//!
//! Reading takes every form Jena may write: prefix names after a prefix declaration, the value
//! forms of integers, doubles and decimals, `REPEAT` (the term of the row before, in that
//! place) and `UNDEF` (unbound). Writing uses the full forms only: IRIs, blank nodes and
//! literals.

use std::collections::HashMap;
use tripletrail::oxigraph::model::{
    BlankNode, GraphName, Literal, NamedNode, NamedOrBlankNode, Quad, Term, Triple, Variable,
};

/// The media types of SPARQL results in these encodings, as Jena names them.
pub const RESULTS_THRIFT: &str = "application/sparql-results+thrift";
pub const RESULTS_PROTOBUF: &str = "application/sparql-results+protobuf";

/// Thrift or Protobuf.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Thrift,
    Protobuf,
}

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

/// A term as read, before prefix names and repeats are resolved.
enum Read {
    Term(Term),
    PrefixName(String, String),
    Repeat,
    Undef,
}

/// The state of a stream being read: its prefixes, and the terms of the row before.
#[derive(Default)]
struct Context {
    prefixes: HashMap<String, String>,
    previous: Vec<Option<Term>>,
}

impl Context {
    /// The term at `place` of a row.
    fn resolve(&mut self, read: Read, place: usize) -> Result<Option<Term>, String> {
        let term = match read {
            Read::Term(t) => Some(t),
            Read::PrefixName(p, local) => {
                let ns = self
                    .prefixes
                    .get(&p)
                    .ok_or_else(|| format!("the prefix {p}: is not declared"))?;
                Some(
                    NamedNode::new(format!("{ns}{local}"))
                        .map_err(|e| e.to_string())?
                        .into(),
                )
            }
            Read::Repeat => Some(
                self.previous
                    .get(place)
                    .cloned()
                    .flatten()
                    .ok_or("REPEAT with no term before it")?,
            ),
            Read::Undef => None,
        };
        if self.previous.len() <= place {
            self.previous.resize(place + 1, None);
        }
        self.previous[place] = term.clone();
        Ok(term)
    }

    fn quad(&mut self, terms: [Option<Read>; 4]) -> Result<Quad, String> {
        let [s, p, o, g] = terms;
        let need = |t: Option<Read>, what: &str| t.ok_or_else(|| format!("a row without {what}"));
        let s = self
            .resolve(need(s, "a subject")?, 0)?
            .ok_or("an undefined subject")?;
        let p = self
            .resolve(need(p, "a predicate")?, 1)?
            .ok_or("an undefined predicate")?;
        let o = self
            .resolve(need(o, "an object")?, 2)?
            .ok_or("an undefined object")?;
        let g = match g {
            Some(g) => self.resolve(g, 3)?,
            None => None,
        };
        let subject = match s {
            Term::NamedNode(n) => NamedOrBlankNode::from(n),
            Term::BlankNode(b) => b.into(),
            t => return Err(format!("{t} is not a subject")),
        };
        let Term::NamedNode(predicate) = p else {
            return Err(format!("{p} is not a predicate"));
        };
        let graph = match g {
            None => GraphName::DefaultGraph,
            Some(Term::NamedNode(n)) if n.as_str() == "urn:x-arq:DefaultGraphNode" => {
                GraphName::DefaultGraph
            }
            Some(Term::NamedNode(n)) => n.into(),
            Some(Term::BlankNode(b)) => b.into(),
            Some(t) => return Err(format!("{t} is not a graph name")),
        };
        Ok(Quad::new(subject, predicate, o, graph))
    }
}

fn literal(
    lex: String,
    lang: Option<String>,
    dir: Option<String>,
    dt: Option<String>,
) -> Result<Term, String> {
    Ok(match (lang, dt) {
        (Some(lang), _) => match dir {
            Some(dir) => {
                let dir = match dir.as_str() {
                    "ltr" => tripletrail::oxigraph::model::BaseDirection::Ltr,
                    "rtl" => tripletrail::oxigraph::model::BaseDirection::Rtl,
                    d => return Err(format!("the base direction {d} is not ltr or rtl")),
                };
                Literal::new_directional_language_tagged_literal(lex, lang, dir)
                    .map_err(|e| e.to_string())?
            }
            None => Literal::new_language_tagged_literal(lex, lang).map_err(|e| e.to_string())?,
        },
        (None, Some(dt)) => {
            Literal::new_typed_literal(lex, NamedNode::new(dt).map_err(|e| e.to_string())?)
        }
        (None, None) => Literal::new_simple_literal(lex),
    }
    .into())
}

/// The lexical form of a decimal `value` × 10^-`scale`, as Jena writes it.
/// The largest scale of a decimal value (`value × 10^-scale`) read: its digits are written out.
const MAX_SCALE: i32 = 1000;

fn decimal(value: i64, scale: i32) -> Result<String, String> {
    if !(-MAX_SCALE..=MAX_SCALE).contains(&scale) {
        return Err(format!(
            "A decimal's scale is {scale}, beyond the {MAX_SCALE} digits read"
        ));
    }
    Ok(decimal_digits(value, scale))
}

fn decimal_digits(value: i64, scale: i32) -> String {
    if scale <= 0 {
        let zeros = "0".repeat(scale.unsigned_abs() as usize);
        return format!("{value}{zeros}.0");
    }
    let negative = value < 0;
    let digits = value.unsigned_abs().to_string();
    let scale = scale as usize;
    let digits = if digits.len() <= scale {
        format!("{}{digits}", "0".repeat(scale + 1 - digits.len()))
    } else {
        digits
    };
    let (int, frac) = digits.split_at(digits.len() - scale);
    format!("{}{int}.{frac}", if negative { "-" } else { "" })
}

fn typed(lex: String, local: &str) -> Term {
    Literal::new_typed_literal(lex, NamedNode::new_unchecked(format!("{XSD}{local}"))).into()
}

/// Jena prints doubles as `1.0E0`.
fn double_lex(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_owned();
    }
    if v.is_infinite() {
        return if v > 0.0 { "INF" } else { "-INF" }.to_owned();
    }
    let s = format!("{v:E}");
    match s.split_once('E') {
        Some((m, e)) if !m.contains('.') => format!("{m}.0E{e}"),
        _ => s,
    }
}

// ===== Thrift compact protocol =====

mod thrift {
    pub const STOP: u8 = 0;
    pub const TRUE: u8 = 1;
    pub const FALSE: u8 = 2;
    pub const BYTE: u8 = 3;
    pub const I16: u8 = 4;
    pub const I32: u8 = 5;
    pub const I64: u8 = 6;
    pub const DOUBLE: u8 = 7;
    pub const BINARY: u8 = 8;
    pub const LIST: u8 = 9;
    pub const SET: u8 = 10;
    pub const MAP: u8 = 11;
    pub const STRUCT: u8 = 12;

    /// How deeply structs may nest (triple terms in triple terms...).
    pub const MAX_DEPTH: usize = 64;

    pub struct Reader<'a> {
        pub bytes: &'a [u8],
        pub at: usize,
    }

    impl Reader<'_> {
        pub fn done(&self) -> bool {
            self.at >= self.bytes.len()
        }

        pub fn byte(&mut self) -> Result<u8, String> {
            let b = *self
                .bytes
                .get(self.at)
                .ok_or("Thrift: the data ends inside a row")?;
            self.at += 1;
            Ok(b)
        }

        pub fn varint(&mut self) -> Result<u64, String> {
            let mut v = 0u64;
            for shift in (0..70).step_by(7) {
                let b = self.byte()?;
                if shift == 63 && b > 1 {
                    return Err("Thrift: a varint over 64 bits".to_owned());
                }
                v |= u64::from(b & 0x7f) << shift;
                if b & 0x80 == 0 {
                    return Ok(v);
                }
            }
            Err("Thrift: a varint over 64 bits".to_owned())
        }

        pub fn zigzag(&mut self) -> Result<i64, String> {
            let v = self.varint()?;
            Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
        }

        pub fn binary(&mut self) -> Result<&[u8], String> {
            let n = usize::try_from(self.varint()?).map_err(|e| e.to_string())?;
            let end = self.at.checked_add(n).filter(|&e| e <= self.bytes.len());
            let end = end.ok_or("Thrift: a string longer than the data")?;
            let s = &self.bytes[self.at..end];
            self.at = end;
            Ok(s)
        }

        pub fn string(&mut self) -> Result<String, String> {
            String::from_utf8(self.binary()?.to_vec()).map_err(|e| format!("Thrift: {e}"))
        }

        pub fn double(&mut self) -> Result<f64, String> {
            let mut b = [0u8; 8];
            for x in &mut b {
                *x = self.byte()?;
            }
            Ok(f64::from_le_bytes(b))
        }

        /// The next field of a struct: its id and type, `None` at the end of the struct.
        pub fn field(&mut self, last: &mut i16) -> Result<Option<(i16, u8)>, String> {
            let b = self.byte()?;
            let kind = b & 0x0f;
            if kind == STOP {
                return Ok(None);
            }
            let delta = b >> 4;
            let id = if delta == 0 {
                i16::try_from(self.zigzag()?).map_err(|e| e.to_string())?
            } else {
                last.checked_add(i16::from(delta))
                    .ok_or("Thrift: a field id over 32767")?
            };
            *last = id;
            Ok(Some((id, kind)))
        }

        /// A list header: element type and size.
        pub fn list(&mut self) -> Result<(u8, usize), String> {
            let b = self.byte()?;
            let size = match b >> 4 {
                15 => usize::try_from(self.varint()?).map_err(|e| e.to_string())?,
                n => usize::from(n),
            };
            Ok((b & 0x0f, size))
        }

        /// An element of a list, set or map: a boolean there is a byte of its own (in a struct,
        /// the field header holds it), so every element reads at least one byte.
        fn skip_element(&mut self, kind: u8, depth: usize) -> Result<(), String> {
            if matches!(kind, TRUE | FALSE) {
                self.byte()?;
                Ok(())
            } else {
                self.skip(kind, depth)
            }
        }

        /// Skips a value of type `kind` (an unknown field).
        pub fn skip(&mut self, kind: u8, depth: usize) -> Result<(), String> {
            if depth > MAX_DEPTH {
                return Err(format!("Thrift: structs nest more than {MAX_DEPTH} deep"));
            }
            match kind {
                TRUE | FALSE => {}
                BYTE => {
                    self.byte()?;
                }
                I16 | I32 | I64 => {
                    self.varint()?;
                }
                DOUBLE => {
                    self.double()?;
                }
                BINARY => {
                    self.binary()?;
                }
                LIST | SET => {
                    let (k, n) = self.list()?;
                    for _ in 0..n {
                        self.skip_element(k, depth + 1)?;
                    }
                }
                MAP => {
                    let n = self.varint()?;
                    if n > 0 {
                        let kinds = self.byte()?;
                        for _ in 0..n {
                            self.skip_element(kinds >> 4, depth + 1)?;
                            self.skip_element(kinds & 0x0f, depth + 1)?;
                        }
                    }
                }
                STRUCT => {
                    let mut last = 0;
                    while let Some((_, k)) = self.field(&mut last)? {
                        self.skip(k, depth + 1)?;
                    }
                }
                k => return Err(format!("Thrift: unknown type {k}")),
            }
            Ok(())
        }
    }

    #[derive(Default)]
    pub struct Writer {
        pub out: Vec<u8>,
        last: Vec<i16>,
    }

    impl Writer {
        pub fn varint(&mut self, mut v: u64) {
            loop {
                let b = (v & 0x7f) as u8;
                v >>= 7;
                if v == 0 {
                    self.out.push(b);
                    return;
                }
                self.out.push(b | 0x80);
            }
        }

        pub fn begin(&mut self) {
            self.last.push(0);
        }

        pub fn end(&mut self) {
            self.out.push(STOP);
            self.last.pop();
        }

        pub fn field(&mut self, id: i16, kind: u8) {
            let last = self.last.last_mut().expect("inside a struct");
            let delta = id - *last;
            *last = id;
            if (1..=15).contains(&delta) {
                self.out.push(((delta as u8) << 4) | kind);
            } else {
                self.out.push(kind);
                self.varint(((i64::from(id) << 1) ^ (i64::from(id) >> 63)) as u64);
            }
        }

        pub fn string(&mut self, id: i16, s: &str) {
            self.field(id, BINARY);
            self.varint(s.len() as u64);
            self.out.extend_from_slice(s.as_bytes());
        }

        pub fn list_header(&mut self, kind: u8, size: usize) {
            if size < 15 {
                self.out.push(((size as u8) << 4) | kind);
            } else {
                self.out.push(0xf0 | kind);
                self.varint(size as u64);
            }
        }
    }
}

fn thrift_prefix_name(r: &mut thrift::Reader<'_>) -> Result<(String, String), String> {
    let (mut prefix, mut local) = (None, None);
    let mut last = 0;
    while let Some((id, kind)) = r.field(&mut last)? {
        match (id, kind) {
            (1, thrift::BINARY) => prefix = Some(r.string()?),
            (2, thrift::BINARY) => local = Some(r.string()?),
            (_, k) => r.skip(k, 1)?,
        }
    }
    Ok((
        prefix.ok_or("Thrift: a prefix name without its prefix")?,
        local.ok_or("Thrift: a prefix name without its local name")?,
    ))
}

/// One string field `1` of a struct (`RDF_IRI`, `RDF_BNode`, `RDF_VAR`).
fn thrift_string_struct(r: &mut thrift::Reader<'_>) -> Result<String, String> {
    let mut value = None;
    let mut last = 0;
    while let Some((id, kind)) = r.field(&mut last)? {
        match (id, kind) {
            (1, thrift::BINARY) => value = Some(r.string()?),
            (_, k) => r.skip(k, 1)?,
        }
    }
    value.ok_or_else(|| "Thrift: a struct without its string".to_owned())
}

fn thrift_term(r: &mut thrift::Reader<'_>, cx: &mut Context, depth: usize) -> Result<Read, String> {
    if depth > thrift::MAX_DEPTH {
        return Err(format!(
            "Thrift: terms nest more than {} deep",
            thrift::MAX_DEPTH
        ));
    }
    let mut out = None;
    let mut last = 0;
    while let Some((id, kind)) = r.field(&mut last)? {
        let read = match (id, kind) {
            (1, thrift::STRUCT) => {
                let iri = thrift_string_struct(r)?;
                Read::Term(NamedNode::new(iri).map_err(|e| e.to_string())?.into())
            }
            (2, thrift::STRUCT) => {
                let label = thrift_string_struct(r)?;
                Read::Term(BlankNode::new(label).map_err(|e| e.to_string())?.into())
            }
            (3, thrift::STRUCT) => {
                let (mut lex, mut lang, mut dir, mut dt, mut dtp) = (None, None, None, None, None);
                let mut last = 0;
                while let Some((id, kind)) = r.field(&mut last)? {
                    match (id, kind) {
                        (1, thrift::BINARY) => lex = Some(r.string()?),
                        (2, thrift::BINARY) => lang = Some(r.string()?),
                        (3, thrift::BINARY) => dt = Some(r.string()?),
                        (4, thrift::STRUCT) => dtp = Some(thrift_prefix_name(r)?),
                        (5, thrift::BINARY) => dir = Some(r.string()?),
                        (_, k) => r.skip(k, 1)?,
                    }
                }
                if let Some((p, local)) = dtp {
                    let ns = cx
                        .prefixes
                        .get(&p)
                        .ok_or_else(|| format!("the prefix {p}: is not declared"))?;
                    dt = Some(format!("{ns}{local}"));
                }
                Read::Term(literal(
                    lex.ok_or("Thrift: a literal without its lexical form")?,
                    lang,
                    dir,
                    dt,
                )?)
            }
            (4, thrift::STRUCT) => {
                let (p, local) = thrift_prefix_name(r)?;
                Read::PrefixName(p, local)
            }
            (5, thrift::STRUCT) => {
                let name = thrift_string_struct(r)?;
                return Err(format!("Thrift: the variable ?{name} in data"));
            }
            (6, thrift::STRUCT) => {
                r.skip(thrift::STRUCT, 1)?;
                return Err("Thrift: ANY in data".to_owned());
            }
            (7, thrift::STRUCT) => {
                r.skip(thrift::STRUCT, 1)?;
                Read::Undef
            }
            (8, thrift::STRUCT) => {
                r.skip(thrift::STRUCT, 1)?;
                Read::Repeat
            }
            (9, thrift::STRUCT) => {
                // A triple term: its own places for REPEAT do not apply.
                let mut inner = Context {
                    prefixes: cx.prefixes.clone(),
                    previous: Vec::new(),
                };
                let t = thrift_triple(r, &mut inner, depth + 1)?;
                Read::Term(Triple::from(t).into())
            }
            (10, thrift::I64) => Read::Term(typed(r.zigzag()?.to_string(), "integer")),
            (11, thrift::DOUBLE) => Read::Term(typed(double_lex(r.double()?), "double")),
            (12, thrift::STRUCT) => {
                let (mut value, mut scale) = (None, None);
                let mut last = 0;
                while let Some((id, kind)) = r.field(&mut last)? {
                    match (id, kind) {
                        (1, thrift::I64) => value = Some(r.zigzag()?),
                        (2, thrift::I32) => scale = Some(r.zigzag()?),
                        (_, k) => r.skip(k, 1)?,
                    }
                }
                let value = value.ok_or("Thrift: a decimal without its value")?;
                let scale = i32::try_from(scale.ok_or("Thrift: a decimal without its scale")?)
                    .map_err(|e| e.to_string())?;
                Read::Term(typed(decimal(value, scale)?, "decimal"))
            }
            (_, k) => {
                r.skip(k, 1)?;
                continue;
            }
        };
        out = Some(read);
    }
    out.ok_or_else(|| "Thrift: an empty term".to_owned())
}

fn thrift_triple(
    r: &mut thrift::Reader<'_>,
    cx: &mut Context,
    depth: usize,
) -> Result<Quad, String> {
    let mut terms: [Option<Read>; 4] = [None, None, None, None];
    let mut last = 0;
    while let Some((id, kind)) = r.field(&mut last)? {
        match (id, kind) {
            (1..=4, thrift::STRUCT) => terms[id as usize - 1] = Some(thrift_term(r, cx, depth)?),
            (_, k) => r.skip(k, 1)?,
        }
    }
    cx.quad(terms)
}

/// The quads of an RDF Thrift document.
fn read_thrift(bytes: &[u8]) -> Result<Vec<Quad>, String> {
    let mut r = thrift::Reader { bytes, at: 0 };
    let mut cx = Context::default();
    let mut out = Vec::new();
    while !r.done() {
        let mut last = 0;
        let mut seen = false;
        while let Some((id, kind)) = r.field(&mut last)? {
            seen = true;
            match (id, kind) {
                (1, thrift::STRUCT) => {
                    let (mut prefix, mut uri) = (None, None);
                    let mut last = 0;
                    while let Some((id, kind)) = r.field(&mut last)? {
                        match (id, kind) {
                            (1, thrift::BINARY) => prefix = Some(r.string()?),
                            (2, thrift::BINARY) => uri = Some(r.string()?),
                            (_, k) => r.skip(k, 1)?,
                        }
                    }
                    if let (Some(p), Some(u)) = (prefix, uri) {
                        cx.prefixes.insert(p, u);
                    }
                }
                (2 | 3, thrift::STRUCT) => out.push(thrift_triple(&mut r, &mut cx, 0)?),
                (_, k) => r.skip(k, 0)?,
            }
        }
        if !seen {
            return Err("Thrift: an empty row".to_owned());
        }
    }
    Ok(out)
}

fn thrift_write_term(w: &mut thrift::Writer, id: i16, t: &Term) -> Result<(), String> {
    w.field(id, thrift::STRUCT);
    w.begin();
    match t {
        Term::NamedNode(n) => {
            w.field(1, thrift::STRUCT);
            w.begin();
            w.string(1, n.as_str());
            w.end();
        }
        Term::BlankNode(b) => {
            w.field(2, thrift::STRUCT);
            w.begin();
            w.string(1, b.as_str());
            w.end();
        }
        Term::Literal(l) => {
            w.field(3, thrift::STRUCT);
            w.begin();
            w.string(1, l.value());
            match l.language() {
                Some(lang) => {
                    w.string(2, lang);
                    if let Some(dir) = l.direction() {
                        w.string(5, &dir.to_string());
                    }
                }
                None => {
                    if l.datatype().as_str() != "http://www.w3.org/2001/XMLSchema#string" {
                        w.string(3, l.datatype().as_str());
                    }
                }
            }
            w.end();
        }
        Term::Triple(t) => {
            w.field(9, thrift::STRUCT);
            w.begin();
            thrift_write_spo(
                w,
                &t.subject.clone().into(),
                &t.predicate.clone().into(),
                &t.object,
            )?;
            w.end();
        }
    }
    w.end();
    Ok(())
}

fn thrift_write_spo(w: &mut thrift::Writer, s: &Term, p: &Term, o: &Term) -> Result<(), String> {
    thrift_write_term(w, 1, s)?;
    thrift_write_term(w, 2, p)?;
    thrift_write_term(w, 3, o)
}

fn write_thrift(quads: &[Quad], datasets: bool) -> Result<Vec<u8>, String> {
    let mut w = thrift::Writer::default();
    for q in quads {
        w.begin();
        let named = !q.graph_name.is_default_graph();
        w.field(if datasets { 3 } else { 2 }, thrift::STRUCT);
        w.begin();
        thrift_write_spo(
            &mut w,
            &q.subject.clone().into(),
            &q.predicate.clone().into(),
            &q.object,
        )?;
        if datasets && named {
            let g: Term = match &q.graph_name {
                GraphName::NamedNode(n) => n.clone().into(),
                GraphName::BlankNode(b) => b.clone().into(),
                GraphName::DefaultGraph => unreachable!("named"),
            };
            thrift_write_term(&mut w, 4, &g)?;
        }
        w.end();
        w.end();
    }
    Ok(w.out)
}

// ===== Protocol Buffers =====

mod proto {
    pub const VARINT: u8 = 0;
    pub const I64: u8 = 1;
    pub const LEN: u8 = 2;
    pub const I32: u8 = 5;

    #[derive(Clone, Copy)]
    pub struct Reader<'a> {
        pub bytes: &'a [u8],
        pub at: usize,
    }

    impl<'a> Reader<'a> {
        pub fn done(&self) -> bool {
            self.at >= self.bytes.len()
        }

        pub fn varint(&mut self) -> Result<u64, String> {
            let mut v = 0u64;
            for shift in (0..70).step_by(7) {
                let b = *self
                    .bytes
                    .get(self.at)
                    .ok_or("Protobuf: the data ends inside a message")?;
                self.at += 1;
                v |= u64::from(b & 0x7f) << shift;
                if b & 0x80 == 0 {
                    return Ok(v);
                }
            }
            Err("Protobuf: a varint over 64 bits".to_owned())
        }

        pub fn zigzag(&mut self) -> Result<i64, String> {
            let v = self.varint()?;
            Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
        }

        pub fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
            let end = self.at.checked_add(n).filter(|&e| e <= self.bytes.len());
            let end = end.ok_or("Protobuf: a field longer than the data")?;
            let s = &self.bytes[self.at..end];
            self.at = end;
            Ok(s)
        }

        pub fn len(&mut self) -> Result<Reader<'a>, String> {
            let n = usize::try_from(self.varint()?).map_err(|e| e.to_string())?;
            Ok(Reader {
                bytes: self.take(n)?,
                at: 0,
            })
        }

        /// The next field: its number and wire type.
        pub fn field(&mut self) -> Result<Option<(u64, u8)>, String> {
            if self.done() {
                return Ok(None);
            }
            let tag = self.varint()?;
            Ok(Some((tag >> 3, (tag & 7) as u8)))
        }

        pub fn skip(&mut self, wire: u8) -> Result<(), String> {
            match wire {
                VARINT => {
                    self.varint()?;
                }
                I64 => {
                    self.take(8)?;
                }
                LEN => {
                    self.len()?;
                }
                I32 => {
                    self.take(4)?;
                }
                w => return Err(format!("Protobuf: wire type {w} is not supported")),
            }
            Ok(())
        }

        pub fn string(&mut self) -> Result<String, String> {
            String::from_utf8(self.len()?.bytes.to_vec()).map_err(|e| format!("Protobuf: {e}"))
        }
    }

    pub fn varint(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }

    pub fn tag(out: &mut Vec<u8>, field: u64, wire: u8) {
        varint(out, (field << 3) | u64::from(wire));
    }

    pub fn bytes(out: &mut Vec<u8>, field: u64, b: &[u8]) {
        tag(out, field, LEN);
        varint(out, b.len() as u64);
        out.extend_from_slice(b);
    }
}

fn proto_string_message(mut r: proto::Reader<'_>) -> Result<String, String> {
    let mut value = String::new();
    while let Some((f, wire)) = r.field()? {
        match (f, wire) {
            (1, proto::LEN) => value = r.string()?,
            (_, w) => r.skip(w)?,
        }
    }
    Ok(value)
}

fn proto_prefix_name(mut r: proto::Reader<'_>) -> Result<(String, String), String> {
    let (mut prefix, mut local) = (String::new(), String::new());
    while let Some((f, wire)) = r.field()? {
        match (f, wire) {
            (1, proto::LEN) => prefix = r.string()?,
            (2, proto::LEN) => local = r.string()?,
            (_, w) => r.skip(w)?,
        }
    }
    Ok((prefix, local))
}

fn proto_term(mut r: proto::Reader<'_>, cx: &Context, depth: usize) -> Result<Read, String> {
    if depth > thrift::MAX_DEPTH {
        return Err(format!(
            "Protobuf: terms nest more than {} deep",
            thrift::MAX_DEPTH
        ));
    }
    let mut out = None;
    while let Some((f, wire)) = r.field()? {
        let read = match (f, wire) {
            (1, proto::LEN) => {
                let iri = proto_string_message(r.len()?)?;
                Read::Term(NamedNode::new(iri).map_err(|e| e.to_string())?.into())
            }
            (2, proto::LEN) => {
                let label = proto_string_message(r.len()?)?;
                Read::Term(BlankNode::new(label).map_err(|e| e.to_string())?.into())
            }
            (3, proto::LEN) => {
                let mut l = r.len()?;
                let (mut lex, mut lang, mut dir, mut dt) = (String::new(), None, None, None);
                while let Some((f, wire)) = l.field()? {
                    match (f, wire) {
                        (1, proto::LEN) => lex = l.string()?,
                        (2, proto::LEN) => lang = Some(l.string()?),
                        (3, proto::LEN) => dt = Some(l.string()?),
                        (4, proto::LEN) => {
                            let (p, local) = proto_prefix_name(l.len()?)?;
                            let ns = cx
                                .prefixes
                                .get(&p)
                                .ok_or_else(|| format!("the prefix {p}: is not declared"))?;
                            dt = Some(format!("{ns}{local}"));
                        }
                        (5, proto::LEN) => dir = Some(l.string()?),
                        (_, w) => l.skip(w)?,
                    }
                }
                Read::Term(literal(lex, lang, dir, dt)?)
            }
            (4, proto::LEN) => {
                let (p, local) = proto_prefix_name(r.len()?)?;
                Read::PrefixName(p, local)
            }
            (5, proto::LEN) => {
                let name = proto_string_message(r.len()?)?;
                return Err(format!("Protobuf: the variable ?{name} in data"));
            }
            (6, proto::LEN) => {
                let mut inner = Context {
                    prefixes: cx.prefixes.clone(),
                    previous: Vec::new(),
                };
                let q = proto_triple(r.len()?, &mut inner, depth + 1)?;
                Read::Term(Triple::from(q).into())
            }
            (7, proto::LEN) => return Err("Protobuf: ANY in data".to_owned()),
            (8, proto::LEN) => {
                r.len()?;
                Read::Undef
            }
            (9, proto::LEN) => {
                r.len()?;
                Read::Repeat
            }
            (20, proto::VARINT) => Read::Term(typed(r.zigzag()?.to_string(), "integer")),
            (21, proto::I64) => {
                let b: [u8; 8] = r.take(8)?.try_into().expect("8 bytes");
                Read::Term(typed(double_lex(f64::from_le_bytes(b)), "double"))
            }
            (22, proto::LEN) => {
                let mut d = r.len()?;
                let (mut value, mut scale) = (0i64, 0i64);
                while let Some((f, wire)) = d.field()? {
                    match (f, wire) {
                        (1, proto::VARINT) => value = d.zigzag()?,
                        (2, proto::VARINT) => scale = d.zigzag()?,
                        (_, w) => d.skip(w)?,
                    }
                }
                let scale = i32::try_from(scale).map_err(|e| e.to_string())?;
                Read::Term(typed(decimal(value, scale)?, "decimal"))
            }
            (_, w) => {
                r.skip(w)?;
                continue;
            }
        };
        out = Some(read);
    }
    out.ok_or_else(|| "Protobuf: an empty term".to_owned())
}

fn proto_triple(mut r: proto::Reader<'_>, cx: &mut Context, depth: usize) -> Result<Quad, String> {
    let mut terms: [Option<Read>; 4] = [None, None, None, None];
    while let Some((f, wire)) = r.field()? {
        match (f, wire) {
            (1..=4, proto::LEN) => terms[f as usize - 1] = Some(proto_term(r.len()?, cx, depth)?),
            (_, w) => r.skip(w)?,
        }
    }
    cx.quad(terms)
}

/// The quads of an RDF Protobuf document: delimited `RDF_StreamRow`s.
fn read_protobuf(bytes: &[u8]) -> Result<Vec<Quad>, String> {
    let mut r = proto::Reader { bytes, at: 0 };
    let mut cx = Context::default();
    let mut out = Vec::new();
    while !r.done() {
        let mut row = r.len()?;
        while let Some((f, wire)) = row.field()? {
            match (f, wire) {
                (1, proto::LEN) => {
                    let (p, u) = proto_prefix_name(row.len()?)?;
                    cx.prefixes.insert(p, u);
                }
                (2 | 3, proto::LEN) => out.push(proto_triple(row.len()?, &mut cx, 0)?),
                (_, w) => row.skip(w)?,
            }
        }
    }
    Ok(out)
}

fn proto_write_term(t: &Term) -> Vec<u8> {
    let mut out = Vec::new();
    match t {
        Term::NamedNode(n) => {
            let mut m = Vec::new();
            proto::bytes(&mut m, 1, n.as_str().as_bytes());
            proto::bytes(&mut out, 1, &m);
        }
        Term::BlankNode(b) => {
            let mut m = Vec::new();
            proto::bytes(&mut m, 1, b.as_str().as_bytes());
            proto::bytes(&mut out, 2, &m);
        }
        Term::Literal(l) => {
            let mut m = Vec::new();
            proto::bytes(&mut m, 1, l.value().as_bytes());
            match l.language() {
                Some(lang) => {
                    proto::bytes(&mut m, 2, lang.as_bytes());
                    if let Some(dir) = l.direction() {
                        proto::bytes(&mut m, 5, dir.to_string().as_bytes());
                    }
                }
                None if l.datatype().as_str() == "http://www.w3.org/2001/XMLSchema#string" => {
                    // `simple = true`.
                    proto::tag(&mut m, 9, proto::VARINT);
                    m.push(1);
                }
                None => proto::bytes(&mut m, 3, l.datatype().as_str().as_bytes()),
            }
            proto::bytes(&mut out, 3, &m);
        }
        Term::Triple(t) => {
            let m = proto_write_spo(
                &t.subject.clone().into(),
                &t.predicate.clone().into(),
                &t.object,
            );
            proto::bytes(&mut out, 6, &m);
        }
    }
    out
}

fn proto_write_spo(s: &Term, p: &Term, o: &Term) -> Vec<u8> {
    let mut m = Vec::new();
    proto::bytes(&mut m, 1, &proto_write_term(s));
    proto::bytes(&mut m, 2, &proto_write_term(p));
    proto::bytes(&mut m, 3, &proto_write_term(o));
    m
}

fn write_protobuf(quads: &[Quad], datasets: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for q in quads {
        let mut m = proto_write_spo(
            &q.subject.clone().into(),
            &q.predicate.clone().into(),
            &q.object,
        );
        if datasets {
            match &q.graph_name {
                GraphName::NamedNode(n) => {
                    proto::bytes(&mut m, 4, &proto_write_term(&n.clone().into()))
                }
                GraphName::BlankNode(b) => {
                    proto::bytes(&mut m, 4, &proto_write_term(&b.clone().into()))
                }
                GraphName::DefaultGraph => {}
            }
        }
        let mut row = Vec::new();
        proto::bytes(&mut row, if datasets { 3 } else { 2 }, &m);
        proto::varint(&mut out, row.len() as u64);
        out.extend_from_slice(&row);
    }
    out
}

// ===== Public =====

/// The quads of an RDF Thrift or Protobuf document. Triple rows are in the default graph.
pub fn read(encoding: Encoding, bytes: &[u8]) -> Result<Vec<Quad>, String> {
    match encoding {
        Encoding::Thrift => read_thrift(bytes),
        Encoding::Protobuf => read_protobuf(bytes),
    }
}

/// `quads` as RDF Thrift or Protobuf: quad rows when `datasets`, else triple rows.
pub fn write(encoding: Encoding, quads: &[Quad], datasets: bool) -> Result<Vec<u8>, String> {
    match encoding {
        Encoding::Thrift => write_thrift(quads, datasets),
        Encoding::Protobuf => Ok(write_protobuf(quads, datasets)),
    }
}

/// SPARQL results: the header, then the rows, unbound as `UNDEF`.
pub struct ResultsWriter {
    encoding: Encoding,
    pub out: Vec<u8>,
}

impl ResultsWriter {
    pub fn new(encoding: Encoding, variables: &[Variable]) -> Self {
        let mut out = Vec::new();
        match encoding {
            Encoding::Thrift => {
                let mut w = thrift::Writer::default();
                w.begin();
                w.field(1, thrift::LIST);
                w.list_header(thrift::STRUCT, variables.len());
                for v in variables {
                    w.begin();
                    w.string(1, v.as_str());
                    w.end();
                }
                w.end();
                out = w.out;
            }
            Encoding::Protobuf => {
                let mut m = Vec::new();
                for v in variables {
                    let mut var = Vec::new();
                    proto::bytes(&mut var, 1, v.as_str().as_bytes());
                    proto::bytes(&mut m, 1, &var);
                }
                proto::varint(&mut out, m.len() as u64);
                out.extend_from_slice(&m);
            }
        }
        Self { encoding, out }
    }

    /// A row: a value or `None` for each variable of the header, in its order.
    pub fn row(&mut self, values: &[Option<&Term>]) -> Result<(), String> {
        match self.encoding {
            Encoding::Thrift => {
                let mut w = thrift::Writer::default();
                w.begin();
                w.field(1, thrift::LIST);
                w.list_header(thrift::STRUCT, values.len());
                for v in values {
                    match v {
                        Some(t) => {
                            // A list element is the struct alone: write it as field 1 of a
                            // throwaway struct and keep the inside.
                            let mut inner = thrift::Writer::default();
                            inner.begin();
                            thrift_write_term(&mut inner, 1, t)?;
                            w.out.extend_from_slice(&inner.out[1..]);
                        }
                        None => {
                            w.begin();
                            w.field(7, thrift::STRUCT);
                            w.begin();
                            w.end();
                            w.end();
                        }
                    }
                }
                w.end();
                self.out.extend_from_slice(&w.out);
            }
            Encoding::Protobuf => {
                let mut m = Vec::new();
                for v in values {
                    let term = match v {
                        Some(t) => proto_write_term(t),
                        None => {
                            let mut u = Vec::new();
                            proto::bytes(&mut u, 8, &[]);
                            u
                        }
                    };
                    proto::bytes(&mut m, 1, &term);
                }
                proto::varint(&mut self.out, m.len() as u64);
                self.out.extend_from_slice(&m);
            }
        }
        Ok(())
    }
}

/// SPARQL results: the variables, then the rows.
pub type Results = (Vec<String>, Vec<Vec<Option<Term>>>);

/// SPARQL results read back.
pub fn read_results(encoding: Encoding, bytes: &[u8]) -> Result<Results, String> {
    let mut cx = Context::default();
    let mut vars = Vec::new();
    let mut rows = Vec::new();
    match encoding {
        Encoding::Thrift => {
            let mut r = thrift::Reader { bytes, at: 0 };
            let mut first = true;
            while !r.done() {
                let mut last = 0;
                let mut row = Vec::new();
                while let Some((id, kind)) = r.field(&mut last)? {
                    if (id, kind) != (1, thrift::LIST) {
                        r.skip(kind, 0)?;
                        continue;
                    }
                    let (_, n) = r.list()?;
                    for place in 0..n {
                        if first {
                            vars.push(thrift_string_struct(&mut r)?);
                        } else {
                            let t = thrift_term(&mut r, &mut cx, 0)?;
                            row.push(cx.resolve(t, place)?);
                        }
                    }
                }
                if !first {
                    rows.push(row);
                }
                first = false;
            }
        }
        Encoding::Protobuf => {
            let mut r = proto::Reader { bytes, at: 0 };
            let mut first = true;
            while !r.done() {
                let mut m = r.len()?;
                let mut row = Vec::new();
                let mut place = 0;
                while let Some((f, wire)) = m.field()? {
                    if (f, wire) != (1, proto::LEN) {
                        m.skip(wire)?;
                        continue;
                    }
                    let item = m.len()?;
                    if first {
                        vars.push(proto_string_message(item)?);
                    } else {
                        let t = proto_term(item, &cx, 0)?;
                        row.push(cx.resolve(t, place)?);
                        place += 1;
                    }
                }
                if !first {
                    rows.push(row);
                }
                first = false;
            }
        }
    }
    Ok((vars, rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Quad> {
        let text = r#"<http://e/s> <http://e/p> "plain" .
<http://e/s> <http://e/p> "chat"@fr .
<http://e/s> <http://e/p> "salaam"@ar--rtl .
<http://e/s> <http://e/q> "1"^^<http://www.w3.org/2001/XMLSchema#integer> .
<http://e/s> <http://e/q> _:b1 <http://e/g> .
_:b1 <http://e/r> <<( <http://e/s> <http://e/p> _:b1 )>> _:g .
"#;
        tripletrail::oxigraph::io::RdfParser::from_format(
            tripletrail::oxigraph::io::RdfFormat::NQuads,
        )
        .for_slice(text.as_bytes())
        .collect::<Result<_, _>>()
        .unwrap()
    }

    #[test]
    fn reads_back_what_it_writes() {
        let quads = sample();
        for encoding in [Encoding::Thrift, Encoding::Protobuf] {
            let bytes = write(encoding, &quads, true).unwrap();
            assert_eq!(read(encoding, &bytes).unwrap(), quads, "{encoding:?}");
            // Triple rows: the graph names are dropped.
            let bytes = write(encoding, &quads, false).unwrap();
            let triples: Vec<Quad> = quads
                .iter()
                .map(|q| Triple::from(q.clone()).in_graph(GraphName::DefaultGraph))
                .collect();
            assert_eq!(read(encoding, &bytes).unwrap(), triples, "{encoding:?}");
        }
    }

    #[test]
    fn thrift_bytes_are_those_of_the_compact_protocol() {
        // One triple <a> <b> "c": row field 2 (struct), its S field 1 (struct) holding the
        // term's field 1 (RDF_IRI) holding field 1 (string "a"), and so on.
        let q = Quad::new(
            NamedNode::new_unchecked("a:a"),
            NamedNode::new_unchecked("a:b"),
            Literal::new_simple_literal("c"),
            GraphName::DefaultGraph,
        );
        let bytes = write(Encoding::Thrift, &[q], false).unwrap();
        #[rustfmt::skip]
        let expected = [
            0x2c,                                     // row: field 2, struct (triple)
              0x1c, 0x1c, 0x18, 3, b'a', b':', b'a', 0, 0, // S: term field 1 (iri) { 1: "a:a" }
              0x1c, 0x1c, 0x18, 3, b'a', b':', b'b', 0, 0, // P
              0x1c, 0x3c, 0x18, 1, b'c', 0, 0,             // O: term field 3 (literal) { 1: "c" }
            0,                                        // end of triple
            0,                                        // end of row
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn reads_prefix_names_value_forms_and_repeats() {
        // Thrift rows as Jena writes them with prefixes and values: a prefix declaration,
        // then <ex:s> <ex:p> 42, then REPEAT REPEAT 1.5 (double) and REPEAT REPEAT 2.50.
        let mut w = thrift::Writer::default();
        w.begin();
        w.field(1, thrift::STRUCT);
        w.begin();
        w.string(1, "ex");
        w.string(2, "http://e/");
        w.end();
        w.end();
        let prefix_name = |w: &mut thrift::Writer, id: i16, local: &str| {
            w.field(id, thrift::STRUCT);
            w.begin();
            w.field(4, thrift::STRUCT);
            w.begin();
            w.string(1, "ex");
            w.string(2, local);
            w.end();
            w.end();
        };
        let repeat = |w: &mut thrift::Writer, id: i16| {
            w.field(id, thrift::STRUCT);
            w.begin();
            w.field(8, thrift::STRUCT);
            w.begin();
            w.end();
            w.end();
        };
        // Row 1.
        w.begin();
        w.field(2, thrift::STRUCT);
        w.begin();
        prefix_name(&mut w, 1, "s");
        prefix_name(&mut w, 2, "p");
        w.field(3, thrift::STRUCT);
        w.begin();
        w.field(10, thrift::I64);
        w.varint(84); // zigzag 42
        w.end();
        w.end();
        w.end();
        // Row 2: a double.
        w.begin();
        w.field(2, thrift::STRUCT);
        w.begin();
        repeat(&mut w, 1);
        repeat(&mut w, 2);
        w.field(3, thrift::STRUCT);
        w.begin();
        w.field(11, thrift::DOUBLE);
        w.out.extend_from_slice(&1.5f64.to_le_bytes());
        w.end();
        w.end();
        w.end();
        // Row 3: a decimal 250 × 10^-2.
        w.begin();
        w.field(2, thrift::STRUCT);
        w.begin();
        repeat(&mut w, 1);
        repeat(&mut w, 2);
        w.field(3, thrift::STRUCT);
        w.begin();
        w.field(12, thrift::STRUCT);
        w.begin();
        w.field(1, thrift::I64);
        w.varint(500);
        w.field(2, thrift::I32);
        w.varint(4);
        w.end();
        w.end();
        w.end();
        w.end();
        let quads = read(Encoding::Thrift, &w.out).unwrap();
        let text: Vec<String> = quads.iter().map(|q| q.to_string()).collect();
        assert_eq!(
            text,
            [
                "<http://e/s> <http://e/p> \"42\"^^<http://www.w3.org/2001/XMLSchema#integer>",
                "<http://e/s> <http://e/p> \"1.5E0\"^^<http://www.w3.org/2001/XMLSchema#double>",
                "<http://e/s> <http://e/p> \"2.50\"^^<http://www.w3.org/2001/XMLSchema#decimal>",
            ]
        );
    }

    #[test]
    fn results_read_back_with_unbound_values() {
        let vars = [Variable::new_unchecked("a"), Variable::new_unchecked("b")];
        let x: Term = NamedNode::new_unchecked("http://e/x").into();
        let y: Term = Literal::new_language_tagged_literal("y", "en")
            .unwrap()
            .into();
        for encoding in [Encoding::Thrift, Encoding::Protobuf] {
            let mut w = ResultsWriter::new(encoding, &vars);
            w.row(&[Some(&x), None]).unwrap();
            w.row(&[Some(&y), Some(&x)]).unwrap();
            let (names, rows) = read_results(encoding, &w.out).unwrap();
            assert_eq!(names, ["a", "b"]);
            assert_eq!(
                rows,
                [
                    vec![Some(x.clone()), None],
                    vec![Some(y.clone()), Some(x.clone())]
                ]
            );
        }
    }

    #[test]
    fn decimals_are_written_as_jena_writes_them() {
        assert_eq!(decimal_digits(250, 2), "2.50");
        assert_eq!(decimal_digits(-5, 3), "-0.005");
        assert_eq!(decimal_digits(7, 0), "7.0");
        assert_eq!(decimal_digits(7, -2), "700.0");
        assert!(decimal(1, i32::MAX).is_err() && decimal(1, i32::MIN).is_err());
        assert_eq!(decimal(1, -MAX_SCALE).unwrap().len(), 1 + 1000 + 2);
        assert_eq!(double_lex(1.5), "1.5E0");
        assert_eq!(double_lex(100.0), "1.0E2");
    }

    #[test]
    fn bad_data_is_an_error_not_a_panic() {
        for bytes in [
            &[0x2c][..],
            &[0x2c, 0x1c, 0x1c, 0x18, 200][..],
            &[0xff; 12][..],
            &[0x0c, 0x00][..],
        ] {
            assert!(read(Encoding::Thrift, bytes).is_err(), "{bytes:?}");
        }
        for bytes in [&[5, 0x12][..], &[0x80; 12][..], &[2, 0x12, 9][..]] {
            assert!(read(Encoding::Protobuf, bytes).is_err(), "{bytes:?}");
        }
        // Deep nesting of triple terms stops at the limit.
        // A row (field 2) whose object (field 3) is a triple term (field 9) whose object is
        // a triple term...
        let mut nested = vec![0x2c];
        for _ in 0..1000 {
            nested.extend_from_slice(&[0x3c, 0x9c]);
        }
        assert!(read(Encoding::Thrift, &nested).is_err());
    }

    #[test]
    fn a_huge_list_of_booleans_runs_out_of_bytes() {
        // An unknown field 10 of a row, a list of ~4 billion booleans (each a byte of its own
        // in a list), and nothing after; field 11, a map of as many boolean pairs.
        let start = std::time::Instant::now();
        let list = [0xa9, 0xf1, 0xff, 0xff, 0xff, 0xff, 0x0f, 1, 1];
        let map = [0xbb, 0xff, 0xff, 0xff, 0xff, 0x0f, 0x11, 1, 1];
        assert!(read(Encoding::Thrift, &list).is_err());
        assert!(read(Encoding::Thrift, &map).is_err());
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }
}
