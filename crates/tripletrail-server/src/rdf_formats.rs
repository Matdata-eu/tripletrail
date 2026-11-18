//! RDF/JSON and TriX (#831): Jena's two RDF formats that Oxigraph's parsers lack.
//!
//! - [RDF/JSON](https://www.w3.org/TR/rdf-json/) (`application/rdf+json`, `.rj`): one graph,
//!   `{ subject: { predicate: [ object, … ] } }`, as Jena reads and writes it (RDF 1.2 triple
//!   terms as `{"type": "triple", "value": {"subject", "predicate", "object"}}`).
//! - [TriX](https://www.hpl.hp.com/techreports/2004/HPL-2004-56.html) (`application/trix`,
//!   `.trix`): XML, a dataset as `<graph>`s of `<triple>`s.
//!
//! A document is read whole into quads; the server and the command line hand them on as
//! N-Quads text, so every loader and upload path reads them as it reads N-Quads. TriX and Jena's
//! binary formats are written as the quads come; RDF/JSON groups by subject, so it is written
//! at the end, of at most [`MAX_RDF_JSON`] triples.

use quick_xml::XmlVersion;
use quick_xml::events::Event;
use std::io::Write;
use tripletrail::oxigraph::io::{RdfFormat, RdfSerializer, WriterQuadSerializer};
use tripletrail::oxigraph::model::{
    BlankNode, GraphName, GraphNameRef, Literal, NamedNode, NamedOrBlankNode, Quad, QuadRef, Term,
    TermRef, Triple, TripleRef,
};

/// A format to read or write a graph or a dataset in: Oxigraph's, or one of this module's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Rdf(RdfFormat),
    RdfJson,
    TriX,
    /// Jena's RDF Thrift and RDF Protobuf ([`crate::jena_binary`]).
    Thrift,
    Protobuf,
}

const THRIFT: &str = "application/rdf+thrift";
const PROTOBUF: &str = "application/rdf+protobuf";
const RDF_JSON: &str = "application/rdf+json";
const TRIX: &str = "application/trix";
const TRIX_NS: &str = "http://www.w3.org/2004/03/trix/trix-1/";

impl Format {
    /// A media type (parameters stripped, lower case), Oxigraph's or this module's.
    pub fn from_media_type(media: &str) -> Option<Self> {
        match media {
            RDF_JSON => Some(Self::RdfJson),
            TRIX | "application/trix+xml" => Some(Self::TriX),
            THRIFT => Some(Self::Thrift),
            PROTOBUF => Some(Self::Protobuf),
            m => RdfFormat::from_media_type(m).map(Self::Rdf),
        }
    }

    /// A file extension, without the dot.
    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension.to_ascii_lowercase().as_str() {
            "rj" => Some(Self::RdfJson),
            "trix" => Some(Self::TriX),
            "trdf" | "rt" => Some(Self::Thrift),
            "prdf" | "rpb" => Some(Self::Protobuf),
            e => RdfFormat::from_extension(e).map(Self::Rdf),
        }
    }

    /// A short name, as `?format=` or `--to` takes it.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "rdfjson" | "rdf/json" | "rj" | "json-rdf" => Some(Self::RdfJson),
            "trix" => Some(Self::TriX),
            "thrift" | "rdf-thrift" | "trdf" => Some(Self::Thrift),
            "protobuf" | "rdf-protobuf" | "proto" | "prdf" => Some(Self::Protobuf),
            _ => None,
        }
    }

    pub fn media_type(self) -> &'static str {
        match self {
            Self::Rdf(f) => f.media_type(),
            Self::RdfJson => RDF_JSON,
            Self::TriX => TRIX,
            Self::Thrift => THRIFT,
            Self::Protobuf => PROTOBUF,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Rdf(f) => f.name(),
            Self::RdfJson => "RDF/JSON",
            Self::TriX => "TriX",
            Self::Thrift => "RDF Thrift",
            Self::Protobuf => "RDF Protobuf",
        }
    }

    pub fn supports_datasets(self) -> bool {
        match self {
            Self::Rdf(f) => f.supports_datasets(),
            Self::RdfJson => false,
            Self::TriX | Self::Thrift | Self::Protobuf => true,
        }
    }
}

/// The quads of an RDF/JSON or TriX document. RDF/JSON's triples are in the default graph.
pub fn parse(format: Format, bytes: &[u8]) -> Result<Vec<Quad>, String> {
    match format {
        Format::RdfJson => parse_rdf_json(bytes),
        Format::TriX => parse_trix(bytes),
        Format::Thrift => crate::jena_binary::read(crate::jena_binary::Encoding::Thrift, bytes),
        Format::Protobuf => crate::jena_binary::read(crate::jena_binary::Encoding::Protobuf, bytes),
        Format::Rdf(f) => Err(format!("{f} is read by Oxigraph's parser")),
    }
}

/// The quads of a file in `format`, read as they stream for Oxigraph's formats (with
/// `parser`'s options) and whole for this module's.
pub fn read_file(
    format: Format,
    parser: impl FnOnce(RdfFormat) -> Result<tripletrail::oxigraph::io::RdfParser, String>,
    path: &std::path::Path,
) -> Result<Box<dyn Iterator<Item = Result<Quad, String>>>, String> {
    let open = |e: std::io::Error| format!("{}: {e}", path.display());
    Ok(match format {
        Format::Rdf(f) => {
            let file = std::fs::File::open(path).map_err(open)?;
            Box::new(
                parser(f)?
                    .for_reader(std::io::BufReader::with_capacity(1 << 20, file))
                    .map(|q| q.map_err(|e| e.to_string())),
            )
        }
        extra => {
            let bytes = std::fs::read(path).map_err(open)?;
            Box::new(parse(extra, &bytes)?.into_iter().map(Ok))
        }
    })
}

/// `bytes` in `format` as N-Quads text, which Oxigraph's parsers and the bulk loader read.
/// Oxigraph's formats are left as they are.
pub fn as_oxigraph(
    format: Format,
    bytes: &[u8],
) -> Result<(RdfFormat, std::borrow::Cow<'_, [u8]>), String> {
    if let Format::Rdf(f) = format {
        return Ok((f, std::borrow::Cow::Borrowed(bytes)));
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut serializer = RdfSerializer::from_format(RdfFormat::NQuads).for_writer(&mut out);
    for q in parse(format, bytes)? {
        serializer.serialize_quad(&q).map_err(|e| e.to_string())?;
    }
    serializer.finish().map_err(|e| e.to_string())?;
    Ok((RdfFormat::NQuads, std::borrow::Cow::Owned(out)))
}

// --- RDF/JSON ---

fn parse_rdf_json(bytes: &[u8]) -> Result<Vec<Quad>, String> {
    use serde_json::Value;
    let err = |m: String| format!("RDF/JSON: {m}");
    let doc: Value = serde_json::from_slice(bytes).map_err(|e| err(e.to_string()))?;
    let Value::Object(subjects) = doc else {
        return Err(err("the document is not an object".to_owned()));
    };
    let mut out = Vec::new();
    for (s, predicates) in subjects {
        let subject = subject_of(&s).map_err(err)?;
        let Value::Object(predicates) = predicates else {
            return Err(err(format!("the value of {s} is not an object")));
        };
        for (p, objects) in predicates {
            let predicate = NamedNode::new(&p).map_err(|e| err(format!("{p}: {e}")))?;
            let Value::Array(objects) = objects else {
                return Err(err(format!("the objects of {s} {p} are not an array")));
            };
            for o in &objects {
                let object = json_term(o).map_err(err)?;
                out.push(Quad::new(
                    subject.clone(),
                    predicate.clone(),
                    object,
                    GraphName::DefaultGraph,
                ));
            }
        }
    }
    Ok(out)
}

fn subject_of(s: &str) -> Result<NamedOrBlankNode, String> {
    match s.strip_prefix("_:") {
        Some(label) => Ok(BlankNode::new(label)
            .map_err(|e| format!("{s}: {e}"))?
            .into()),
        None => Ok(NamedNode::new(s).map_err(|e| format!("{s}: {e}"))?.into()),
    }
}

fn json_term(o: &serde_json::Value) -> Result<Term, String> {
    let field = |name: &str| o.get(name).and_then(|v| v.as_str());
    let value = || field("value").ok_or_else(|| format!("{o}: no \"value\" string"));
    match field("type") {
        Some("uri") => Ok(NamedNode::new(value()?).map_err(|e| e.to_string())?.into()),
        Some("bnode") => {
            let v = value()?;
            Ok(BlankNode::new(v.strip_prefix("_:").unwrap_or(v))
                .map_err(|e| format!("{v}: {e}"))?
                .into())
        }
        Some("literal") => {
            let v = value()?;
            Ok(match (field("lang"), field("datatype")) {
                (Some(lang), _) => Literal::new_language_tagged_literal(v, lang)
                    .map_err(|e| format!("{lang}: {e}"))?,
                (None, Some(dt)) => Literal::new_typed_literal(
                    v,
                    NamedNode::new(dt).map_err(|e| format!("{dt}: {e}"))?,
                ),
                (None, None) => Literal::new_simple_literal(v),
            }
            .into())
        }
        Some("triple") => {
            let t = o.get("value").ok_or_else(|| format!("{o}: no \"value\""))?;
            let part = |name: &str| {
                t.get(name)
                    .ok_or_else(|| format!("{t}: no \"{name}\""))
                    .and_then(json_term)
            };
            let subject = match part("subject")? {
                Term::NamedNode(n) => NamedOrBlankNode::from(n),
                Term::BlankNode(b) => b.into(),
                other => return Err(format!("{other} is not a subject")),
            };
            let Term::NamedNode(predicate) = part("predicate")? else {
                return Err(format!("{t}: the predicate is not an IRI"));
            };
            Ok(Triple::new(subject, predicate, part("object")?).into())
        }
        _ => Err(format!(
            "{o}: \"type\" is not \"uri\", \"bnode\", \"literal\" or \"triple\""
        )),
    }
}

fn json_string(s: &str) -> String {
    serde_json::Value::from(s).to_string()
}

fn write_json_term(out: &mut String, t: TermRef<'_>) -> std::io::Result<()> {
    match t {
        TermRef::NamedNode(n) => {
            out.push_str(&format!(
                "{{\"type\":\"uri\",\"value\":{}}}",
                json_string(n.as_str())
            ));
        }
        TermRef::BlankNode(b) => {
            out.push_str(&format!(
                "{{\"type\":\"bnode\",\"value\":{}}}",
                json_string(&format!("_:{}", b.as_str()))
            ));
        }
        TermRef::Literal(l) => {
            if l.direction().is_some() {
                return Err(std::io::Error::other(
                    "RDF/JSON has no form for a literal with a base direction",
                ));
            }
            out.push_str(&format!(
                "{{\"type\":\"literal\",\"value\":{}",
                json_string(l.value())
            ));
            if let Some(lang) = l.language() {
                out.push_str(&format!(",\"lang\":{}", json_string(lang)));
            } else if l.datatype().as_str() != "http://www.w3.org/2001/XMLSchema#string" {
                out.push_str(&format!(
                    ",\"datatype\":{}",
                    json_string(l.datatype().as_str())
                ));
            }
            out.push('}');
        }
        TermRef::Triple(t) => {
            out.push_str("{\"type\":\"triple\",\"value\":{\"subject\":");
            write_json_term(out, t.subject.as_ref().into())?;
            out.push_str(",\"predicate\":");
            write_json_term(out, t.predicate.as_ref().into())?;
            out.push_str(",\"object\":");
            write_json_term(out, t.object.as_ref())?;
            out.push_str("}}");
        }
    }
    Ok(())
}

/// A predicate and its objects, written as RDF/JSON.
type Objects = (String, Vec<String>);

fn write_rdf_json(out: &mut impl Write, triples: &[Triple]) -> std::io::Result<()> {
    // Subjects and their predicates in the order they come, objects grouped under them.
    let mut subjects: Vec<(String, Vec<Objects>)> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for t in triples {
        let s = match &t.subject {
            NamedOrBlankNode::NamedNode(n) => n.as_str().to_owned(),
            NamedOrBlankNode::BlankNode(b) => format!("_:{}", b.as_str()),
        };
        let i = *index.entry(s.clone()).or_insert_with(|| {
            subjects.push((s, Vec::new()));
            subjects.len() - 1
        });
        let mut object = String::new();
        write_json_term(&mut object, t.object.as_ref())?;
        let predicates = &mut subjects[i].1;
        match predicates
            .iter_mut()
            .find(|(p, _)| p == t.predicate.as_str())
        {
            Some((_, objects)) => objects.push(object),
            None => predicates.push((t.predicate.as_str().to_owned(), vec![object])),
        }
    }
    out.write_all(b"{")?;
    for (i, (s, predicates)) in subjects.iter().enumerate() {
        if i > 0 {
            out.write_all(b",")?;
        }
        write!(out, "\n  {}: {{", json_string(s))?;
        for (j, (p, objects)) in predicates.iter().enumerate() {
            if j > 0 {
                out.write_all(b",")?;
            }
            write!(out, "\n    {}: [ {} ]", json_string(p), objects.join(" , "))?;
        }
        out.write_all(b"\n  }")?;
    }
    out.write_all(b"\n}\n")
}

// --- TriX ---

/// An element of a TriX document: its local name, `xml:lang` and `datatype`, its text and
/// its child elements.
#[derive(Default)]
struct Element {
    name: String,
    lang: Option<String>,
    datatype: Option<String>,
    text: String,
    children: Vec<Element>,
}

/// How deeply TriX elements may nest (triple terms in triple terms...).
const MAX_DEPTH: usize = 128;

fn xml_tree(bytes: &[u8]) -> Result<Element, String> {
    let err = |e: &dyn std::fmt::Display| format!("TriX: {e}");
    let mut reader = quick_xml::Reader::from_reader(bytes);
    reader.config_mut().expand_empty_elements = true;
    let mut buf = Vec::new();
    let mut stack: Vec<Element> = vec![Element::default()];
    loop {
        buf.clear();
        match reader.read_event_into(&mut buf).map_err(|e| err(&e))? {
            Event::Start(e) => {
                if stack.len() > MAX_DEPTH {
                    return Err(format!("TriX: elements nest more than {MAX_DEPTH} deep"));
                }
                let mut element = Element {
                    name: String::from_utf8_lossy(e.local_name().as_ref()).into_owned(),
                    ..Element::default()
                };
                for a in e.attributes() {
                    let a = a.map_err(|e| err(&e))?;
                    let value = a
                        .normalized_value(XmlVersion::Implicit1_0)
                        .map_err(|e| err(&e))?
                        .into_owned();
                    match a.key.as_ref() {
                        b"xml:lang" => element.lang = Some(value),
                        k if k == b"datatype" || k.ends_with(b":datatype") => {
                            element.datatype = Some(value)
                        }
                        _ => {}
                    }
                }
                stack.push(element);
            }
            Event::End(_) => {
                let element = stack.pop().expect("an open element");
                stack
                    .last_mut()
                    .ok_or_else(|| "TriX: unbalanced elements".to_owned())?
                    .children
                    .push(element);
            }
            Event::Text(t) => {
                let s = t
                    .xml_content(XmlVersion::Implicit1_0)
                    .map_err(|e| err(&e))?;
                stack.last_mut().expect("the document").text.push_str(&s);
            }
            Event::CData(t) => {
                let s = String::from_utf8_lossy(&t).into_owned();
                stack.last_mut().expect("the document").text.push_str(&s);
            }
            Event::GeneralRef(r) => {
                let resolved = match r.resolve_char_ref().map_err(|e| err(&e))? {
                    Some(c) => c.to_string(),
                    None => {
                        let name = r.decode().map_err(|e| err(&e))?;
                        quick_xml::escape::resolve_predefined_entity(&name)
                            .ok_or_else(|| format!("TriX: unknown entity &{name};"))?
                            .to_owned()
                    }
                };
                stack
                    .last_mut()
                    .expect("the document")
                    .text
                    .push_str(&resolved);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let mut document = stack
        .pop()
        .filter(|_| stack.is_empty())
        .ok_or_else(|| "TriX: the document ends inside an element".to_owned())?;
    match document.children.len() {
        1 => Ok(document.children.remove(0)),
        _ => Err("TriX: the document has no single root element".to_owned()),
    }
}

fn trix_term(e: &Element) -> Result<Term, String> {
    match e.name.as_str() {
        "uri" => Ok(NamedNode::new(e.text.trim())
            .map_err(|x| format!("TriX: <uri>{}</uri>: {x}", e.text))?
            .into()),
        "id" => Ok(BlankNode::new(e.text.trim())
            .map_err(|x| format!("TriX: <id>{}</id>: {x}", e.text))?
            .into()),
        "plainLiteral" => Ok(match &e.lang {
            Some(lang) => Literal::new_language_tagged_literal(&e.text, lang)
                .map_err(|x| format!("TriX: xml:lang {lang}: {x}"))?,
            None => Literal::new_simple_literal(&e.text),
        }
        .into()),
        "typedLiteral" => {
            let dt = e
                .datatype
                .as_deref()
                .ok_or("TriX: <typedLiteral> without datatype")?;
            Ok(Literal::new_typed_literal(
                &e.text,
                NamedNode::new(dt).map_err(|x| format!("TriX: datatype {dt}: {x}"))?,
            )
            .into())
        }
        "triple" => Ok(trix_triple(e)?.into()),
        other => Err(format!("TriX: <{other}> is not a term")),
    }
}

fn trix_triple(e: &Element) -> Result<Triple, String> {
    let [s, p, o] = e.children.as_slice() else {
        return Err(format!(
            "TriX: a <triple> has {} terms, not 3",
            e.children.len()
        ));
    };
    let subject = match trix_term(s)? {
        Term::NamedNode(n) => NamedOrBlankNode::from(n),
        Term::BlankNode(b) => b.into(),
        other => return Err(format!("TriX: {other} is not a subject")),
    };
    let Term::NamedNode(predicate) = trix_term(p)? else {
        return Err("TriX: a predicate is not a <uri>".to_owned());
    };
    Ok(Triple::new(subject, predicate, trix_term(o)?))
}

fn parse_trix(bytes: &[u8]) -> Result<Vec<Quad>, String> {
    let root = xml_tree(bytes)?;
    if root.name != "TriX" {
        return Err(format!(
            "TriX: the root element is <{}>, not <TriX>",
            root.name
        ));
    }
    let mut out = Vec::new();
    for g in &root.children {
        if g.name != "graph" {
            return Err(format!("TriX: <{}> in <TriX>, not <graph>", g.name));
        }
        let mut graph = GraphName::DefaultGraph;
        for (i, child) in g.children.iter().enumerate() {
            match child.name.as_str() {
                "uri" | "id" if i == 0 => {
                    graph = match trix_term(child)? {
                        Term::NamedNode(n) => n.into(),
                        Term::BlankNode(b) => b.into(),
                        _ => unreachable!("a <uri> or an <id>"),
                    };
                }
                "triple" => {
                    let t = trix_triple(child)?;
                    out.push(t.in_graph(graph.clone()));
                }
                other => return Err(format!("TriX: <{other}> in <graph>")),
            }
        }
    }
    Ok(out)
}

fn xml_escape(s: &str) -> std::borrow::Cow<'_, str> {
    quick_xml::escape::escape(s)
}

fn write_trix_term(out: &mut impl Write, t: TermRef<'_>, indent: &str) -> std::io::Result<()> {
    match t {
        TermRef::NamedNode(n) => writeln!(out, "{indent}<uri>{}</uri>", xml_escape(n.as_str())),
        TermRef::BlankNode(b) => writeln!(out, "{indent}<id>{}</id>", xml_escape(b.as_str())),
        TermRef::Literal(l) => {
            if l.direction().is_some() {
                return Err(std::io::Error::other(
                    "TriX has no form for a literal with a base direction",
                ));
            }
            let value = xml_escape(l.value());
            match l.language() {
                Some(lang) => writeln!(
                    out,
                    "{indent}<plainLiteral xml:lang=\"{}\">{value}</plainLiteral>",
                    xml_escape(lang)
                ),
                None if l.datatype().as_str() == "http://www.w3.org/2001/XMLSchema#string" => {
                    writeln!(out, "{indent}<plainLiteral>{value}</plainLiteral>")
                }
                None => writeln!(
                    out,
                    "{indent}<typedLiteral datatype=\"{}\">{value}</typedLiteral>",
                    xml_escape(l.datatype().as_str())
                ),
            }
        }
        TermRef::Triple(t) => write_trix_triple(out, t.as_ref(), indent),
    }
}

fn write_trix_triple(out: &mut impl Write, t: TripleRef<'_>, indent: &str) -> std::io::Result<()> {
    writeln!(out, "{indent}<triple>")?;
    let inner = format!("{indent}  ");
    write_trix_term(out, t.subject.into(), &inner)?;
    write_trix_term(out, t.predicate.into(), &inner)?;
    write_trix_term(out, t.object, &inner)?;
    writeln!(out, "{indent}</triple>")
}

// --- Writing ---

/// Most triples an RDF/JSON answer holds: it groups the triples by subject, so it is built in
/// memory before its first byte.
pub const MAX_RDF_JSON: usize = 1_000_000;

/// Writes triples or quads in a [`Format`], as they come; RDF/JSON at [`QuadWriter::finish`].
pub enum QuadWriter<W: Write> {
    Rdf(Box<WriterQuadSerializer<W>>),
    RdfJson {
        out: W,
        triples: Vec<Triple>,
        /// [`MAX_RDF_JSON`], less in tests.
        limit: usize,
    },
    TriX {
        out: W,
        started: bool,
        /// The `<graph>` open now.
        graph: Option<GraphName>,
    },
    /// One row a quad: a triple row in the default graph, a quad row in a named one.
    Binary {
        out: W,
        encoding: crate::jena_binary::Encoding,
    },
}

fn trix_header(out: &mut impl Write) -> std::io::Result<()> {
    writeln!(out, "<?xml version=\"1.0\" encoding=\"utf-8\"?>")?;
    writeln!(out, "<TriX xmlns=\"{TRIX_NS}\">")
}

impl<W: Write> QuadWriter<W> {
    pub fn new(format: Format, out: W) -> Self {
        use crate::jena_binary::Encoding;
        match format {
            Format::Rdf(f) => Self::Rdf(Box::new(RdfSerializer::from_format(f).for_writer(out))),
            Format::RdfJson => Self::RdfJson {
                out,
                triples: Vec::new(),
                limit: MAX_RDF_JSON,
            },
            Format::TriX => Self::TriX {
                out,
                started: false,
                graph: None,
            },
            Format::Thrift => Self::Binary {
                out,
                encoding: Encoding::Thrift,
            },
            Format::Protobuf => Self::Binary {
                out,
                encoding: Encoding::Protobuf,
            },
        }
    }

    pub fn serialize_quad<'a>(&mut self, q: impl Into<QuadRef<'a>>) -> std::io::Result<()> {
        let q = q.into();
        match self {
            Self::Rdf(s) => s.serialize_quad(q),
            Self::RdfJson { triples, limit, .. } => {
                if triples.len() >= *limit {
                    return Err(std::io::Error::other(format!(
                        "RDF/JSON is built in memory, of at most {limit} triples: ask for \
                         N-Triples, Turtle, TriX or RDF Thrift for more"
                    )));
                }
                triples.push(TripleRef::new(q.subject, q.predicate, q.object).into_owned());
                Ok(())
            }
            Self::TriX {
                out,
                started,
                graph,
            } => {
                if !*started {
                    trix_header(out)?;
                    *started = true;
                }
                if graph.as_ref().map(GraphName::as_ref) != Some(q.graph_name) {
                    if graph.is_some() {
                        writeln!(out, "  </graph>")?;
                    }
                    writeln!(out, "  <graph>")?;
                    match q.graph_name {
                        GraphNameRef::NamedNode(n) => write_trix_term(out, n.into(), "    ")?,
                        GraphNameRef::BlankNode(b) => write_trix_term(out, b.into(), "    ")?,
                        GraphNameRef::DefaultGraph => {}
                    }
                    *graph = Some(q.graph_name.into_owned());
                }
                let t = TripleRef::new(q.subject, q.predicate, q.object);
                write_trix_triple(out, t, "    ")
            }
            Self::Binary { out, encoding } => {
                let named = !q.graph_name.is_default_graph();
                let row = crate::jena_binary::write(*encoding, &[q.into_owned()], named)
                    .map_err(std::io::Error::other)?;
                out.write_all(&row)
            }
        }
    }

    pub fn serialize_triple<'a>(&mut self, t: impl Into<TripleRef<'a>>) -> std::io::Result<()> {
        let t = t.into();
        self.serialize_quad(QuadRef::new(
            t.subject,
            t.predicate,
            t.object,
            GraphNameRef::DefaultGraph,
        ))
    }

    pub fn finish(self) -> std::io::Result<W> {
        let mut out = match self {
            Self::Rdf(s) => return s.finish(),
            Self::RdfJson {
                mut out, triples, ..
            } => {
                write_rdf_json(&mut out, &triples)?;
                out
            }
            Self::TriX {
                mut out,
                started,
                graph,
            } => {
                if !started {
                    trix_header(&mut out)?;
                }
                if graph.is_some() {
                    writeln!(out, "  </graph>")?;
                }
                writeln!(out, "</TriX>")?;
                out
            }
            Self::Binary { out, .. } => out,
        };
        out.flush()?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nq(quads: &[Quad]) -> Vec<String> {
        let mut v: Vec<String> = quads.iter().map(|q| format!("{q} .")).collect();
        v.sort();
        v
    }

    fn roundtrip(format: Format, quads: &[Quad]) -> Vec<Quad> {
        let mut w = QuadWriter::new(format, Vec::new());
        for q in quads {
            w.serialize_quad(q).unwrap();
        }
        let bytes = w.finish().unwrap();
        parse(format, &bytes).unwrap_or_else(|e| panic!("{e}\n{}", String::from_utf8_lossy(&bytes)))
    }

    fn sample() -> Vec<Quad> {
        let text = r#"<http://e/s> <http://e/p> "plain" .
<http://e/s> <http://e/p> "chat"@fr .
<http://e/s> <http://e/q> "1"^^<http://www.w3.org/2001/XMLSchema#integer> .
<http://e/s> <http://e/q> _:b1 .
_:b1 <http://e/p> "a < b & \"c\"" .
_:b1 <http://e/r> <<( <http://e/s> <http://e/p> _:b1 )>> .
"#;
        tripletrail::oxigraph::io::RdfParser::from_format(RdfFormat::NQuads)
            .for_slice(text.as_bytes())
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn rdf_json_reads_back_what_it_writes() {
        let quads = sample();
        assert_eq!(nq(&roundtrip(Format::RdfJson, &quads)), nq(&quads));
    }

    #[test]
    fn trix_reads_back_what_it_writes_with_graphs() {
        let mut quads = sample();
        let g = NamedNode::new_unchecked("http://e/g");
        quads.push(Quad::new(
            g.clone(),
            g.clone(),
            Literal::new_simple_literal("in g"),
            g.clone(),
        ));
        assert_eq!(nq(&roundtrip(Format::TriX, &quads)), nq(&quads));
    }

    #[test]
    fn reads_the_w3c_note_and_jena_examples() {
        // From the RDF/JSON note, section 3.
        let doc = r#"{ "http://example.org/about" : {
            "http://purl.org/dc/terms/title" : [ { "value" : "Anna's Homepage", "type" : "literal", "lang" : "en" } ] } }"#;
        assert_eq!(
            nq(&parse(Format::RdfJson, doc.as_bytes()).unwrap()),
            [
                "<http://example.org/about> <http://purl.org/dc/terms/title> \"Anna's Homepage\"@en ."
            ]
        );
        // As Jena writes TriX: a namespace, a named graph, a typed literal.
        let doc = r#"<?xml version="1.0" encoding="utf-8"?>
<TriX xmlns="http://www.w3.org/2004/03/trix/trix-1/">
  <graph>
    <uri>http://e/g</uri>
    <triple><uri>http://e/s</uri><uri>http://e/p</uri><typedLiteral datatype="http://www.w3.org/2001/XMLSchema#integer">7</typedLiteral></triple>
  </graph>
  <graph><triple><id>x</id><uri>http://e/p</uri><plainLiteral xml:lang="en">a &amp; b</plainLiteral></triple></graph>
</TriX>"#;
        assert_eq!(
            nq(&parse(Format::TriX, doc.as_bytes()).unwrap()),
            [
                "<http://e/s> <http://e/p> \"7\"^^<http://www.w3.org/2001/XMLSchema#integer> <http://e/g> .",
                "_:x <http://e/p> \"a & b\"@en .",
            ]
        );
    }

    #[test]
    fn bad_documents_say_what_is_wrong() {
        for (format, doc, says) in [
            (Format::RdfJson, "[]", "not an object"),
            (
                Format::RdfJson,
                r#"{"http://e/s":{"http://e/p":[{"value":"x"}]}}"#,
                "\"type\"",
            ),
            (
                Format::RdfJson,
                r#"{"http://e/s":{"http://e/p":{}}}"#,
                "not an array",
            ),
            (
                Format::TriX,
                "<TriX><graph><triple><uri>http://e/s</uri></triple></graph></TriX>",
                "1 terms",
            ),
            (Format::TriX, "<RDF/>", "not <TriX>"),
            (Format::TriX, "<TriX><graph>", "ends inside"),
        ] {
            let e = parse(format, doc.as_bytes()).unwrap_err();
            assert!(e.contains(says), "{doc}: {e}");
        }
    }

    #[test]
    fn trix_and_binary_rows_are_written_as_they_come_and_rdf_json_is_bounded() {
        let q = |s: &str, g: Option<&str>| {
            Quad::new(
                NamedNode::new_unchecked(format!("http://e/{s}")),
                NamedNode::new_unchecked("http://e/p"),
                Literal::new_simple_literal(s),
                g.map_or(GraphName::DefaultGraph, |g| {
                    NamedNode::new_unchecked(format!("http://e/{g}")).into()
                }),
            )
        };
        let quads = [
            q("a", None),
            q("b", Some("g")),
            q("c", Some("g")),
            q("d", None),
        ];
        for format in [Format::TriX, Format::Thrift, Format::Protobuf] {
            let mut w = QuadWriter::new(format, Vec::new());
            let mut sizes = Vec::new();
            for x in &quads {
                w.serialize_quad(x).unwrap();
                sizes.push(match &w {
                    QuadWriter::TriX { out, .. } | QuadWriter::Binary { out, .. } => out.len(),
                    _ => unreachable!(),
                });
            }
            // Each quad adds to the output before `finish`.
            assert!(
                sizes.windows(2).all(|w| w[0] < w[1]) && sizes[0] > 0,
                "{format:?}"
            );
            let bytes = w.finish().unwrap();
            assert_eq!(
                nq(&parse(format, &bytes).unwrap()),
                nq(&quads),
                "{format:?}"
            );
        }
        let mut w = QuadWriter::new(Format::RdfJson, Vec::new());
        if let QuadWriter::RdfJson { limit, .. } = &mut w {
            *limit = 2;
        }
        w.serialize_quad(&quads[0]).unwrap();
        w.serialize_quad(&quads[1]).unwrap();
        let e = w.serialize_quad(&quads[2]).unwrap_err();
        assert!(e.to_string().contains("at most 2 triples"), "{e}");
    }
}
