//! XML, as SPARQL Anything's XML triplifier.
//!
//! Without `xml.path`, the root element is the root container (typed `fx:Root` and by its
//! name). An element is a container typed by its name: `xyz:<local name>` without a
//! namespace, else `<namespace><local name>` (with `#` added when the namespace does not end
//! with `/` or `#`). Attributes are keys named the same way; child elements and text are
//! slots in document order (whitespace-only text is skipped, text around comments is joined).
//! Text and attribute values in the scope of `xml:lang` are language-tagged, unless
//! `xml.lang-tags=false`. With `blank-nodes=false` the containers are IRIs under the root:
//! `<root>/<root element>/1:child/2:grandchild`.
//!
//! With `xml.path` (and `xml.path.1`, `xml.path.2`, ...), each node an XPath expression
//! selects is a slot of the root, in document order: an element is a container typed
//! `xyz:<name as written>`, with its attributes as keys and its content as slots; an
//! attribute is a key of the root; a text node a slot of the root. This mode follows SPARQL
//! Anything's (VTD-XML based) output closely, including its quirks: the first text of the
//! selected element is also slot 1 of the element and of every element under it, comments
//! and processing instructions take a slot number without a triple, attributes beyond the
//! selected element's count take slot numbers too, and text after the element up to the
//! next element start belongs to it. Values are not language-tagged in this mode. XPath
//! is a subset: `/` and `//` steps, names, `*`, `@name`, `text()`, `node()`, `.`, `..`, and
//! predicates `[n]`, `[last()]`, `[@a]`, `[@a='v']`, `[child]` and `[child='v']`.

use crate::FxError;
use crate::options::Options;
use crate::tree::{Sink, escape_iri};
use oxigraph::model::{NamedOrBlankNode, Term};
use quick_xml::NsReader;
use quick_xml::XmlVersion;
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;

const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

pub(crate) fn triplify(
    bytes: &[u8],
    options: &Options,
    sink: &mut Sink<'_>,
) -> Result<(), FxError> {
    let events = parse(bytes)?;
    let paths = xpaths(options);
    if paths.is_empty() {
        let lang_tags = options.flag("xml.lang-tags", true)?;
        stream(&events, sink, lang_tags);
        Ok(())
    } else {
        let tokens = Tokens::new(&events);
        let root = sink.root();
        let mut count = 1;
        for path in paths {
            let path = XPath::parse(&path).map_err(|e| match e {
                FxError::Unsupported(m) => FxError::Unsupported(format!("xml.path: {m}")),
                e => e,
            })?;
            for hit in path.evaluate(&tokens) {
                tokens.transform_selected(hit, count, &root, sink);
                count += 1;
            }
        }
        Ok(())
    }
}

/// `xml.path`, then `xml.path.1`, `xml.path.2`, ... in order.
fn xpaths(options: &Options) -> Vec<String> {
    let mut out: Vec<String> = options
        .get("xml.path")
        .map(str::to_owned)
        .into_iter()
        .collect();
    let mut numbered: Vec<(u64, String)> = options
        .0
        .iter()
        .filter_map(|(k, v)| {
            let n = k.strip_prefix("xml.path.")?.parse().ok()?;
            Some((n, v.clone()))
        })
        .collect();
    numbered.sort();
    out.extend(numbered.into_iter().map(|(_, v)| v));
    out
}

#[derive(Debug)]
struct Attribute {
    qname: String,
    local: String,
    namespace: Option<String>,
    value: String,
    /// `xmlns` or `xmlns:p`.
    declaration: bool,
}

#[derive(Debug)]
enum Ev {
    Start {
        qname: String,
        local: String,
        namespace: Option<String>,
        attributes: Vec<Attribute>,
        depth: usize,
    },
    End,
    /// Character data between two pieces of markup, with references resolved.
    Text {
        text: String,
        depth: usize,
    },
    CData {
        text: String,
        depth: usize,
    },
    Comment,
    Pi {
        has_value: bool,
    },
}

fn utf8(bytes: &[u8]) -> Result<String, FxError> {
    String::from_utf8(bytes.to_vec()).map_err(|e| FxError::Parse(format!("XML: {e}")))
}

fn namespace(r: ResolveResult<'_>) -> Result<Option<String>, FxError> {
    match r {
        ResolveResult::Bound(n) => Ok(Some(utf8(n.as_ref())?)),
        ResolveResult::Unbound => Ok(None),
        ResolveResult::Unknown(p) => Err(FxError::Parse(format!(
            "XML: unknown namespace prefix {}",
            String::from_utf8_lossy(&p)
        ))),
    }
}

/// How deeply elements may nest.
const MAX_DEPTH: usize = 512;

/// The document as events, empty elements expanded. Text outside the root element is
/// dropped; DTDs are not read.
///
/// Elements may nest at most [`MAX_DEPTH`] deep: a container's IRI names all its ancestors,
/// so the graph grows with the square of the nesting (140 KB of nested elements made 4.6 GB).
fn parse(bytes: &[u8]) -> Result<Vec<Ev>, FxError> {
    let err = |e: &dyn std::fmt::Display| FxError::Parse(format!("XML: {e}"));
    let mut reader = NsReader::from_reader(bytes);
    reader.config_mut().expand_empty_elements = true;
    let mut buf = Vec::new();
    let mut out = Vec::new();
    // The number of open elements.
    let mut depth = 0;
    let mut text: Option<String> = None;
    loop {
        buf.clear();
        let (ns, event) = reader
            .read_resolved_event_into(&mut buf)
            .map_err(|e| err(&e))?;
        let ns = match &event {
            Event::Start(_) => namespace(ns)?,
            _ => None,
        };
        match &event {
            Event::Text(t) => {
                let s = t
                    .xml_content(XmlVersion::Implicit1_0)
                    .map_err(|e| err(&e))?;
                text.get_or_insert_with(String::new).push_str(&s);
                continue;
            }
            Event::GeneralRef(r) => {
                let resolved = match r.resolve_char_ref().map_err(|e| err(&e))? {
                    Some(c) => c.to_string(),
                    None => {
                        let name = r.decode().map_err(|e| err(&e))?;
                        quick_xml::escape::resolve_predefined_entity(&name)
                            .ok_or_else(|| FxError::Parse(format!("XML: unknown entity &{name};")))?
                            .to_owned()
                    }
                };
                text.get_or_insert_with(String::new).push_str(&resolved);
                continue;
            }
            _ => {}
        }
        if let Some(t) = text.take()
            && depth > 0
        {
            out.push(Ev::Text {
                text: t,
                depth: depth - 1,
            });
        }
        match event {
            Event::Start(_) if depth >= MAX_DEPTH => {
                return Err(FxError::Parse(format!(
                    "XML: elements nest more than {MAX_DEPTH} deep"
                )));
            }
            Event::Start(e) => {
                let mut attributes = Vec::new();
                for a in e.attributes() {
                    let a = a.map_err(|e| err(&e))?;
                    let qname = utf8(a.key.as_ref())?;
                    let declaration = qname == "xmlns" || qname.starts_with("xmlns:");
                    let (ans, local) = reader.resolver().resolve_attribute(a.key);
                    let namespace = if declaration { None } else { namespace(ans)? };
                    let local = utf8(local.as_ref())?;
                    let value = a
                        .normalized_value(XmlVersion::Implicit1_0)
                        .map_err(|e| err(&e))?
                        .into_owned();
                    attributes.push(Attribute {
                        qname,
                        local,
                        namespace,
                        value,
                        declaration,
                    });
                }
                out.push(Ev::Start {
                    qname: utf8(e.name().as_ref())?,
                    local: utf8(e.local_name().as_ref())?,
                    namespace: ns,
                    attributes,
                    depth,
                });
                depth += 1;
            }
            Event::End(_) => {
                depth -= 1;
                out.push(Ev::End);
            }
            Event::CData(c) if depth > 0 => out.push(Ev::CData {
                text: utf8(&c.into_inner())?,
                depth: depth - 1,
            }),
            Event::Comment(_) if depth > 0 => out.push(Ev::Comment),
            Event::PI(p) if depth > 0 => out.push(Ev::Pi {
                has_value: !p.content().iter().all(u8::is_ascii_whitespace),
            }),
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

/// `namespace` + `local`, with `#` after a namespace that ends with neither `/` nor `#`.
fn namespaced(namespace: &str, local: &str) -> String {
    let mut iri = namespace.to_owned();
    if !iri.ends_with('/') && !iri.ends_with('#') {
        iri.push('#');
    }
    iri.push_str(local);
    iri
}

/// The language of an `xml:lang` value: itself when it is a well-formed tag, else none.
fn language_of(value: &str) -> String {
    if oxigraph::model::Literal::new_language_tagged_literal("", value).is_ok() {
        value.to_owned()
    } else {
        String::new()
    }
}

struct Open {
    node: NamedOrBlankNode,
    path: String,
    members: usize,
    lang: String,
}

/// Without `xml.path`: the whole document, as SPARQL Anything's streaming (StAX) reader.
fn stream(events: &[Ev], sink: &mut Sink<'_>, lang_tags: bool) {
    let mut stack: Vec<Open> = Vec::new();
    let mut text: Option<String> = None;
    let value = |sink: &Sink<'_>, value: &str, lang: &str, tag: bool| -> Option<Term> {
        if tag && !lang.is_empty() {
            sink.lang_string(value, lang)
        } else {
            sink.string(value)
        }
    };
    let flush = |sink: &mut Sink<'_>, stack: &mut Vec<Open>, text: &mut Option<String>| {
        if let (Some(t), Some(top)) = (text.take(), stack.last_mut())
            && !t.trim().is_empty()
        {
            top.members += 1;
            if let Some(o) = value(sink, &t, &top.lang, lang_tags) {
                sink.slot(&top.node, top.members, o);
            }
        }
    };
    for ev in events {
        match ev {
            Ev::Text { text: t, .. } | Ev::CData { text: t, .. } => {
                text.get_or_insert_with(String::new).push_str(t);
            }
            Ev::Comment | Ev::Pi { .. } => {}
            Ev::Start {
                qname,
                local,
                namespace,
                attributes,
                ..
            } => {
                flush(sink, &mut stack, &mut text);
                let (node, path, lang) = match stack.last_mut() {
                    None => {
                        let mut path = String::new();
                        escape_iri(qname, &mut path);
                        (sink.root(), path, String::new())
                    }
                    Some(parent) => {
                        parent.members += 1;
                        let mut path = format!("{}/{}:", parent.path, parent.members);
                        escape_iri(qname, &mut path);
                        let node = sink.container(|| path.clone());
                        sink.slot(&parent.node, parent.members, node.clone().into());
                        (node, path, parent.lang.clone())
                    }
                };
                let class = match namespace {
                    None => sink.key_iri(local),
                    Some(ns) => namespaced(ns, local),
                };
                sink.class(&node, class);
                let lang = attributes
                    .iter()
                    .find(|a| a.namespace.as_deref() == Some(XML_NS) && a.local == "lang")
                    .map_or(lang, |a| language_of(&a.value));
                for a in attributes.iter().filter(|a| !a.declaration) {
                    let p = match &a.namespace {
                        None => sink.key_iri(&a.local),
                        Some(ns) => namespaced(ns, &a.local),
                    };
                    let tag = lang_tags && a.namespace.as_deref() != Some(XML_NS);
                    if let Some(p) = sink.predicate(p)
                        && let Some(o) = value(sink, &a.value, &lang, tag)
                    {
                        sink.emit(&node, &p, o);
                    }
                }
                stack.push(Open {
                    node,
                    path,
                    members: 0,
                    lang,
                });
            }
            Ev::End => {
                flush(sink, &mut stack, &mut text);
                stack.pop();
            }
        }
    }
}

/// A token of VTD-XML's flat index of a document, which SPARQL Anything's `xml.path` mode
/// walks. Whitespace-only text is not a token.
#[derive(Debug)]
enum Tok<'a> {
    Start { qname: &'a str, depth: usize },
    AttrName { qname: &'a str, value: &'a str },
    AttrValue,
    NsName,
    NsValue { value: &'a str },
    Text { text: &'a str, depth: usize },
    CData { text: &'a str, depth: usize },
    Other,
}

#[derive(Debug, Default)]
struct Element {
    /// The index of its start token.
    token: usize,
    parent: Option<usize>,
    /// Child elements and text tokens, in document order.
    children: Vec<Child>,
    /// Its attributes (not namespace declarations): the index of their name token.
    attributes: Vec<usize>,
}

#[derive(Debug, Clone, Copy)]
enum Child {
    Element(usize),
    Text(usize),
}

/// The tokens and the element tree over them.
struct Tokens<'a> {
    tokens: Vec<Tok<'a>>,
    elements: Vec<Element>,
}

impl<'a> Tokens<'a> {
    fn new(events: &'a [Ev]) -> Self {
        let mut tokens = Vec::new();
        let mut elements: Vec<Element> = Vec::new();
        let mut open: Vec<usize> = Vec::new();
        for ev in events {
            match ev {
                Ev::Start {
                    qname,
                    attributes,
                    depth,
                    ..
                } => {
                    let e = elements.len();
                    let parent = open.last().copied();
                    if let Some(p) = parent {
                        elements[p].children.push(Child::Element(e));
                    }
                    let mut element = Element {
                        token: tokens.len(),
                        parent,
                        ..Element::default()
                    };
                    tokens.push(Tok::Start {
                        qname,
                        depth: *depth,
                    });
                    for a in attributes {
                        if a.declaration {
                            tokens.push(Tok::NsName);
                            tokens.push(Tok::NsValue { value: &a.value });
                        } else {
                            element.attributes.push(tokens.len());
                            tokens.push(Tok::AttrName {
                                qname: &a.qname,
                                value: &a.value,
                            });
                            tokens.push(Tok::AttrValue);
                        }
                    }
                    elements.push(element);
                    open.push(e);
                }
                Ev::End => {
                    open.pop();
                }
                Ev::Text { text, depth } => {
                    if !text.trim().is_empty() {
                        if let Some(&p) = open.last() {
                            elements[p].children.push(Child::Text(tokens.len()));
                        }
                        tokens.push(Tok::Text {
                            text,
                            depth: *depth,
                        });
                    }
                }
                Ev::CData { text, depth } => {
                    if let Some(&p) = open.last() {
                        elements[p].children.push(Child::Text(tokens.len()));
                    }
                    tokens.push(Tok::CData {
                        text,
                        depth: *depth,
                    });
                }
                Ev::Comment => tokens.push(Tok::Other),
                Ev::Pi { has_value } => {
                    tokens.push(Tok::Other);
                    if *has_value {
                        tokens.push(Tok::Other);
                    }
                }
            }
        }
        Self { tokens, elements }
    }

    /// The selected node `token` as slot `n` of the root (`transformFromXPath`).
    fn transform_selected(
        &self,
        token: usize,
        n: usize,
        root: &NamedOrBlankNode,
        sink: &mut Sink<'_>,
    ) {
        // VTD-XML's cursor stays on the selected element while its content is walked, so
        // its first text and attribute count apply to every element under it.
        let (text, attributes) = match &self.tokens[token] {
            Tok::Start { .. } => self.first_text_and_attributes(token),
            _ => (None, 0),
        };
        let walk = Walk {
            tokens: &self.tokens,
            text,
            attributes,
        };
        walk.transform(token, n, "", root, sink);
    }

    /// The normalized first text of the element at `token` (before any child element), and
    /// its number of attributes.
    fn first_text_and_attributes(&self, token: usize) -> (Option<String>, usize) {
        let Tok::Start { depth, .. } = self.tokens[token] else {
            return (None, 0);
        };
        let mut attributes = 0;
        let mut i = token + 1;
        while i < self.tokens.len() {
            match &self.tokens[i] {
                Tok::AttrName { .. } => attributes += 1,
                Tok::AttrValue | Tok::NsName | Tok::NsValue { .. } => {}
                _ => break,
            }
            i += 1;
        }
        while i < self.tokens.len() {
            match &self.tokens[i] {
                Tok::Text { text, depth: d } | Tok::CData { text, depth: d } => {
                    return ((*d == depth).then(|| normalize(text)), attributes);
                }
                Tok::Other => i += 1,
                _ => break,
            }
        }
        (None, attributes)
    }
}

/// Whitespace runs collapsed to one space, and trimmed.
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

struct Walk<'t, 'a> {
    tokens: &'t [Tok<'a>],
    text: Option<String>,
    attributes: usize,
}

impl Walk<'_, '_> {
    /// The node at token `i` as slot `child` of `parent` (whose path is `parent_path`);
    /// returns the index of its last token.
    fn transform(
        &self,
        i: usize,
        child: usize,
        parent_path: &str,
        parent: &NamedOrBlankNode,
        sink: &mut Sink<'_>,
    ) -> usize {
        match &self.tokens[i] {
            Tok::Start { qname, depth } => {
                let mut path = format!("{parent_path}/{child}:");
                escape_iri(qname, &mut path);
                let node = sink.container(|| path[1..].to_owned());
                sink.slot(parent, child, node.clone().into());
                let class = sink.key_iri(qname);
                sink.class(&node, class);
                let mut increment = 0;
                for j in (i + 1..=i + self.attributes * 2).step_by(2) {
                    let Some(Tok::AttrName { qname, value }) = self.tokens.get(j) else {
                        break;
                    };
                    if let Some(o) = sink.string(value) {
                        sink.key(&node, qname, o);
                    }
                    increment += 2;
                }
                if let Some(text) = &self.text
                    && let Some(o) = sink.string(text)
                {
                    sink.slot(&node, 1, o);
                }
                let mut index = i + increment;
                let mut children = 1;
                loop {
                    index += 1;
                    match self.tokens.get(index) {
                        None => break,
                        Some(Tok::Start { depth: d, .. }) if d <= depth => break,
                        Some(_) => {}
                    }
                    index = self.transform(index, children, &path, &node, sink);
                    children += 1;
                }
                index - 1
            }
            Tok::AttrName { qname, value } => {
                if let Some(o) = sink.string(value) {
                    sink.key(parent, qname, o);
                }
                i + 1
            }
            Tok::NsValue { value } => {
                if let Some(o) = sink.string(value) {
                    sink.slot(parent, child, o);
                }
                i
            }
            Tok::Text { text, .. } => {
                if let Some(o) = sink.string(text) {
                    sink.slot(parent, child, o);
                }
                i
            }
            Tok::AttrValue | Tok::NsName | Tok::CData { .. } | Tok::Other => i,
        }
    }
}

/// The XPath subset of `xml.path`.
#[derive(Debug)]
struct XPath {
    /// Starts at the document (`/…`), not at the context node.
    absolute: bool,
    steps: Vec<Step>,
}

#[derive(Debug)]
struct Step {
    /// After `//`: any descendant (or self, for `.`), not only children.
    descendants: bool,
    test: Test,
    predicates: Vec<Predicate>,
}

#[derive(Debug)]
enum Test {
    Name(String),
    AnyElement,
    Attribute(Option<String>),
    Text,
    Node,
    Current,
    Parent,
}

#[derive(Debug)]
enum Predicate {
    Position(usize),
    Last,
    Attribute(String, Option<String>),
    Child(String, Option<String>),
}

/// A node of the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Node {
    Document,
    Element(usize),
    /// An attribute, by its name token.
    Attribute(usize),
    /// A text node, by its token.
    Text(usize),
}

impl XPath {
    fn parse(source: &str) -> Result<Self, FxError> {
        let unsupported = || {
            FxError::Unsupported(format!(
                "XPath {source:?}: only `/`, `//`, names, `*`, `@name`, `text()`, `node()`, \
                 `.`, `..` and the predicates [n], [last()], [@a], [@a='v'], [child], \
                 [child='v'] are supported"
            ))
        };
        let s = source.trim();
        let absolute = s.starts_with('/');
        let mut steps = Vec::new();
        let mut rest = s;
        let mut first = true;
        while !rest.is_empty() || first {
            let descendants = if let Some(r) = rest.strip_prefix("//") {
                rest = r;
                true
            } else if let Some(r) = rest.strip_prefix('/') {
                rest = r;
                false
            } else if first {
                false
            } else {
                return Err(unsupported());
            };
            first = false;
            if rest.is_empty() {
                if steps.is_empty() && !descendants {
                    // `/`: the document itself, which is not a node to triplify.
                    return Err(unsupported());
                }
                break;
            }
            let end = step_end(rest);
            let (step, after) = rest.split_at(end);
            rest = after;
            let (test, mut predicates_text) = match step.find('[') {
                Some(b) => (&step[..b], &step[b..]),
                None => (step, ""),
            };
            let test = match test {
                "." => Test::Current,
                ".." => Test::Parent,
                "*" => Test::AnyElement,
                "@*" => Test::Attribute(None),
                "text()" => Test::Text,
                "node()" => Test::Node,
                t if t.starts_with('@') && is_name(&t[1..]) => {
                    Test::Attribute(Some(t[1..].to_owned()))
                }
                t if is_name(t) => Test::Name(t.to_owned()),
                _ => return Err(unsupported()),
            };
            let mut predicates = Vec::new();
            while let Some(p) = predicates_text.strip_prefix('[') {
                let close = closing(p).ok_or_else(unsupported)?;
                predicates.push(predicate(p[..close].trim()).ok_or_else(unsupported)?);
                predicates_text = &p[close + 1..];
            }
            if !predicates_text.is_empty() {
                return Err(unsupported());
            }
            steps.push(Step {
                descendants,
                test,
                predicates,
            });
        }
        Ok(Self { absolute, steps })
    }

    /// The selected nodes, in document order, as their token index.
    fn evaluate(&self, doc: &Tokens<'_>) -> Vec<usize> {
        self.select(doc, Node::Document)
            .into_iter()
            .filter_map(|n| match n {
                Node::Document => None,
                Node::Element(e) => Some(doc.elements[e].token),
                Node::Attribute(t) | Node::Text(t) => Some(t),
            })
            .collect()
    }

    /// The selected nodes in document order, from `context` unless the path is absolute.
    fn select(&self, doc: &Tokens<'_>, context: Node) -> Vec<Node> {
        let mut context = vec![if self.absolute {
            Node::Document
        } else {
            context
        }];
        for step in &self.steps {
            let mut next = Vec::new();
            for &c in &context {
                // `//step` is `descendant-or-self::node()/step`: each of those nodes is a
                // context of its own for the predicates.
                let bases: Vec<Node> = if step.descendants {
                    descendants_or_self(doc, c)
                } else {
                    vec![c]
                };
                for base in bases {
                    let mut candidates = step_nodes(doc, base, &step.test);
                    for p in &step.predicates {
                        let len = candidates.len();
                        candidates = candidates
                            .into_iter()
                            .enumerate()
                            .filter(|(i, n)| matches(doc, p, *i, len, *n))
                            .map(|(_, n)| n)
                            .collect();
                    }
                    next.extend(candidates);
                }
            }
            next.sort_by_key(|n| order(doc, *n));
            next.dedup();
            context = next;
        }
        context
    }
}

/// The records an XPath `iterator` selects in an XML document, and per record the string
/// values of each of `references` (XPaths relative to the record): the text of an element
/// (with its descendants'), the value of an attribute, a text node. For RML's XPath sources.
pub(crate) fn records(
    bytes: &[u8],
    iterator: &str,
    references: &[&str],
) -> Result<Vec<Vec<Vec<String>>>, FxError> {
    let events = parse(bytes)?;
    let doc = Tokens::new(&events);
    let iterator = XPath::parse(iterator)?;
    let references: Vec<XPath> = references
        .iter()
        .map(|r| XPath::parse(r))
        .collect::<Result<_, _>>()?;
    Ok(iterator
        .select(&doc, Node::Document)
        .into_iter()
        .map(|record| {
            references
                .iter()
                .map(|r| {
                    r.select(&doc, record)
                        .into_iter()
                        .map(|n| string_value(&doc, n))
                        .collect()
                })
                .collect()
        })
        .collect())
}

fn string_value(doc: &Tokens<'_>, n: Node) -> String {
    match n {
        Node::Document => doc
            .elements
            .iter()
            .position(|e| e.parent.is_none())
            .map(|e| text_of(doc, e))
            .unwrap_or_default(),
        Node::Element(e) => text_of(doc, e),
        Node::Attribute(t) => match doc.tokens[t] {
            Tok::AttrName { value, .. } => value.to_owned(),
            _ => String::new(),
        },
        Node::Text(t) => match doc.tokens[t] {
            Tok::Text { text, .. } | Tok::CData { text, .. } => text.to_owned(),
            _ => String::new(),
        },
    }
}

/// A name, or `prefix:*`.
fn is_name(s: &str) -> bool {
    let s = s.strip_suffix(":*").unwrap_or(s);
    !s.contains("::")
        && s.chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
        && s.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
}

/// The length of the step at the start of `s`: up to the next `/` outside brackets and quotes.
fn step_end(s: &str) -> usize {
    let mut depth = 0;
    let mut quote = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '[') => depth += 1,
            (None, ']') => depth -= 1,
            (None, '/') if depth == 0 => return i,
            _ => {}
        }
    }
    s.len()
}

/// The index of the `]` closing a predicate that starts at the beginning of `s`.
fn closing(s: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, ']') => return Some(i),
            _ => {}
        }
    }
    None
}

fn predicate(p: &str) -> Option<Predicate> {
    if p == "last()" {
        return Some(Predicate::Last);
    }
    if let Ok(n) = p.parse::<usize>() {
        return (n > 0).then_some(Predicate::Position(n));
    }
    let (name, value) = match p.split_once('=') {
        Some((name, value)) => {
            let value = value.trim();
            let unquoted = value
                .strip_prefix('\'')
                .and_then(|v| v.strip_suffix('\''))
                .or_else(|| value.strip_prefix('"').and_then(|v| v.strip_suffix('"')))?;
            (name.trim(), Some(unquoted.to_owned()))
        }
        None => (p, None),
    };
    match name.strip_prefix('@') {
        Some(a) if is_name(a) => Some(Predicate::Attribute(a.to_owned(), value)),
        None if is_name(name) => Some(Predicate::Child(name.to_owned(), value)),
        _ => None,
    }
}

fn order(doc: &Tokens<'_>, n: Node) -> usize {
    match n {
        Node::Document => 0,
        Node::Element(e) => doc.elements[e].token + 1,
        Node::Attribute(t) | Node::Text(t) => t + 1,
    }
}

fn children(doc: &Tokens<'_>, n: Node) -> Vec<Node> {
    match n {
        Node::Document => doc
            .elements
            .iter()
            .position(|e| e.parent.is_none())
            .map(Node::Element)
            .into_iter()
            .collect(),
        Node::Element(e) => doc.elements[e]
            .children
            .iter()
            .map(|c| match *c {
                Child::Element(e) => Node::Element(e),
                Child::Text(t) => Node::Text(t),
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn descendants_or_self(doc: &Tokens<'_>, n: Node) -> Vec<Node> {
    let mut out = vec![n];
    let mut i = 0;
    while i < out.len() {
        let c = children(doc, out[i]);
        out.extend(c.into_iter().filter(|c| matches!(c, Node::Element(_))));
        i += 1;
    }
    out
}

fn element_name<'a>(doc: &Tokens<'a>, e: usize) -> &'a str {
    match doc.tokens[doc.elements[e].token] {
        Tok::Start { qname, .. } => qname,
        _ => "",
    }
}

fn name_matches(test: &str, name: &str) -> bool {
    match test.strip_suffix(":*") {
        Some(prefix) => name.split_once(':').is_some_and(|(p, _)| p == prefix),
        None => test == name,
    }
}

fn step_nodes(doc: &Tokens<'_>, base: Node, test: &Test) -> Vec<Node> {
    match test {
        Test::Current => vec![base],
        Test::Parent => match base {
            Node::Element(e) => doc.elements[e]
                .parent
                .map(Node::Element)
                .into_iter()
                .collect(),
            _ => Vec::new(),
        },
        Test::Attribute(name) => match base {
            Node::Element(e) => doc.elements[e]
                .attributes
                .iter()
                .filter(|&&t| match (&doc.tokens[t], name) {
                    (Tok::AttrName { qname, .. }, Some(n)) => name_matches(n, qname),
                    _ => true,
                })
                .map(|&t| Node::Attribute(t))
                .collect(),
            _ => Vec::new(),
        },
        Test::Name(name) => children(doc, base)
            .into_iter()
            .filter(|c| matches!(c, Node::Element(e) if name_matches(name, element_name(doc, *e))))
            .collect(),
        Test::AnyElement => children(doc, base)
            .into_iter()
            .filter(|c| matches!(c, Node::Element(_)))
            .collect(),
        Test::Text => children(doc, base)
            .into_iter()
            .filter(|c| matches!(c, Node::Text(_)))
            .collect(),
        Test::Node => children(doc, base),
    }
}

fn text_of(doc: &Tokens<'_>, e: usize) -> String {
    let mut s = String::new();
    for c in &doc.elements[e].children {
        match *c {
            Child::Text(t) => {
                if let Tok::Text { text, .. } | Tok::CData { text, .. } = doc.tokens[t] {
                    s.push_str(text);
                }
            }
            Child::Element(c) => s.push_str(&text_of(doc, c)),
        }
    }
    s
}

fn matches(doc: &Tokens<'_>, p: &Predicate, i: usize, len: usize, n: Node) -> bool {
    match p {
        Predicate::Position(k) => i + 1 == *k,
        Predicate::Last => i + 1 == len,
        Predicate::Attribute(name, value) => match n {
            Node::Element(e) => doc.elements[e].attributes.iter().any(|&t| {
                matches!(&doc.tokens[t], Tok::AttrName { qname, value: v }
                    if name_matches(name, qname) && value.as_deref().is_none_or(|w| w == *v))
            }),
            _ => false,
        },
        Predicate::Child(name, value) => match n {
            Node::Element(e) => doc.elements[e].children.iter().any(|c| {
                matches!(*c, Child::Element(c) if name_matches(name, element_name(doc, c))
                    && value.as_deref().is_none_or(|w| text_of(doc, c) == w))
            }),
            _ => false,
        },
    }
}
