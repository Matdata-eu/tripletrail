//! HTML, as SPARQL Anything's HTML triplifier (Jsoup based).
//!
//! The elements `html.selector` selects (by default `:root`, the `html` element) are
//! containers. When it selects one, that element is the root container; when it selects
//! several, the root holds them as slots in document order. An element is typed
//! `xhtml:<tag>` (`http://www.w3.org/1999/xhtml#`), its attributes are `xhtml:<name>` keys,
//! and its child elements, text, comments and script or style text are slots in document
//! order (text that is only whitespace is skipped). A prefixed name (`og:title`) whose
//! prefix an `xmlns:og` attribute declares on the element or above takes that namespace.
//! Each element also has `whatwg:innerHTML` and `whatwg:innerText`
//! (`https://html.spec.whatwg.org/#`) when they are not blank. With `blank-nodes=false` the
//! containers are IRIs under the root that follow the element's path: `html/body/div.note`,
//! with `:<n>` (the element's position among its parent's elements) when the parent has more
//! than one child of that tag and those classes.
//!
//! Text values, `innerHTML` and `innerText` are written as Jsoup 1.23 writes them: `innerHTML`
//! pretty-printed with one space of indent per level, text with runs of whitespace collapsed
//! and `&`, `<`, `>` and the no-break space escaped, `innerText` with whitespace collapsed and
//! a space between blocks. The document is parsed by html5ever, as browsers parse HTML.
//!
//! `html.selector` is a subset of CSS: type selectors (`p`, `*`, `svg|rect`), `#id`,
//! `.class`, attribute selectors (`[a]`, `[a=v]`, `[a^=v]`, `[a$=v]`, `[a*=v]`, `[a~=v]`),
//! `:root`, `:first-child`, `:last-child`, `:only-child`, `:nth-child(n)`, the combinators
//! ` `, `>`, `+` and `~`, and lists (`h1, h2`). `html.parser=xml` reads the file with an XML parser
//! (quick-xml) and writes `innerHTML` as Jsoup's XML mode does, not pretty-printed.
//! `html.metadata` and `html.browser` are not supported.
//!
//! Limits: a start tag may have at most 10,000 attributes, elements may nest at most 512 deep, and the `innerHTML` and `innerText` values
//! together may hold at most 256 MiB (each element repeats the markup of all it holds).

use crate::FxError;
use crate::options::Options;
use crate::tree::{Sink, escape_iri};
use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use oxigraph::model::{NamedNode, NamedOrBlankNode};
use std::collections::HashMap;

const XHTML: &str = "http://www.w3.org/1999/xhtml#";
const WHATWG: &str = "https://html.spec.whatwg.org/#";
const MAX_DEPTH: usize = 512;
const MAX_TEXT: usize = 256 << 20;

pub(crate) fn triplify(
    bytes: &[u8],
    options: &Options,
    sink: &mut Sink<'_>,
) -> Result<(), FxError> {
    for unsupported in ["html.browser", "html.metadata"] {
        if options.get(unsupported).is_some_and(|v| v != "false") {
            return Err(FxError::Unsupported(unsupported.into()));
        }
    }
    let xml = match options.get("html.parser") {
        None | Some("html") => false,
        Some("xml") => true,
        Some(p) => return Err(FxError::Unsupported(format!("html.parser={p}"))),
    };
    let selector = Selector::parse(options.get("html.selector").unwrap_or(":root"))?;
    let doc = if xml {
        Doc::parse_xml(bytes)?
    } else {
        Doc::parse(bytes)?
    };
    let selected = doc.select(&selector);
    let mut writer = Writer {
        doc: &doc,
        html: sink.predicate(format!("{WHATWG}innerHTML")),
        text: sink.predicate(format!("{WHATWG}innerText")),
        spent: 0,
        max: MAX_TEXT,
        names: HashMap::new(),
        groups: HashMap::new(),
        counts: HashMap::new(),
    };
    let root = sink.root();
    if selected.len() == 1 {
        writer.populate(sink, selected[0], &root)?;
    } else {
        for (n, &e) in selected.iter().enumerate() {
            let node = writer.container(sink, e);
            sink.slot(&root, n + 1, node.clone().into());
            writer.populate(sink, e, &node)?;
        }
    }
    Ok(())
}

/// An XML element's name and attributes, as written.
fn xml_element(e: &quick_xml::events::BytesStart<'_>) -> Result<Kind, FxError> {
    let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
    let mut attrs = Vec::new();
    for a in e.attributes() {
        let a = a.map_err(|e| FxError::Parse(format!("XML: {e}")))?;
        let value = a
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(|e| FxError::Parse(format!("XML: {e}")))?
            .into_owned();
        attrs.push((String::from_utf8_lossy(a.key.as_ref()).into_owned(), value));
    }
    Ok(Kind::Element {
        name,
        ns: Ns::Other,
        attrs,
    })
}

/// An attribute: its name and value.
type Attr = (String, String);

/// What a node is.
enum Kind {
    Document,
    Element {
        /// As Jsoup names it: `prefix:local` for prefixed foreign elements.
        name: String,
        ns: Ns,
        attrs: Vec<Attr>,
    },
    Text(String),
    /// The text of `script`, `style` and the other raw text elements: written as is.
    Data(String),
    Comment(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ns {
    Html,
    Svg,
    MathMl,
    Other,
}

struct Node {
    kind: Kind,
    parent: Option<usize>,
    /// Position among the parent's children.
    pos: usize,
    children: Vec<usize>,
    /// For an element, its position among its parent's elements.
    elem_pos: usize,
    /// How many of the children are elements.
    elements: usize,
}

/// A parsed document, its nodes in document order (the document itself first).
struct Doc {
    nodes: Vec<Node>,
    /// Read with `html.parser=xml`: `innerHTML` is written as is, not pretty-printed.
    xml: bool,
}

impl Doc {
    fn parse(bytes: &[u8]) -> Result<Doc, FxError> {
        let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
        check_attributes(bytes)?;
        let text = String::from_utf8_lossy(bytes);
        let opts = html5ever::ParseOpts {
            tree_builder: html5ever::tree_builder::TreeBuilderOpts {
                // As Jsoup: the content of <noscript> is markup.
                scripting_enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let dom = html5ever::parse_document(RcDom::default(), opts).one(text.as_ref());
        let mut doc = Doc {
            xml: false,
            nodes: vec![Node {
                kind: Kind::Document,
                parent: None,
                pos: 0,
                children: Vec::new(),
                elem_pos: 0,
                elements: 0,
            }],
        };
        // (node, parent id, depth), children pushed in reverse to keep document order.
        let mut stack: Vec<(Handle, usize, usize)> = children(&dom.document)
            .into_iter()
            .rev()
            .map(|c| (c, 0, 1))
            .collect();
        while let Some((handle, parent, depth)) = stack.pop() {
            let kind = match &handle.data {
                NodeData::Element { name, attrs, .. } => {
                    if depth > MAX_DEPTH {
                        return Err(FxError::Unsupported(format!(
                            "HTML nested more than {MAX_DEPTH} elements deep"
                        )));
                    }
                    let ns = match &*name.ns {
                        "http://www.w3.org/1999/xhtml" => Ns::Html,
                        "http://www.w3.org/2000/svg" => Ns::Svg,
                        "http://www.w3.org/1998/Math/MathML" => Ns::MathMl,
                        _ => Ns::Other,
                    };
                    let attrs = attrs
                        .borrow()
                        .iter()
                        .map(|a| {
                            let key = match &a.name.prefix {
                                Some(p) => format!("{p}:{}", a.name.local),
                                None => a.name.local.to_string(),
                            };
                            (key, a.value.to_string())
                        })
                        .collect();
                    let name = match &name.prefix {
                        Some(p) => format!("{p}:{}", name.local),
                        None => name.local.to_string(),
                    };
                    Kind::Element { name, ns, attrs }
                }
                NodeData::Text { contents } => {
                    let text = contents.borrow().to_string();
                    if doc.is_data_parent(parent) {
                        Kind::Data(text)
                    } else {
                        Kind::Text(text)
                    }
                }
                NodeData::Comment { contents } => Kind::Comment(contents.to_string()),
                _ => continue,
            };
            let id = doc.nodes.len();
            let pos = doc.nodes[parent].children.len();
            let elem_pos = doc.nodes[parent].elements;
            if matches!(kind, Kind::Element { .. }) {
                doc.nodes[parent].elements += 1;
            }
            doc.nodes[parent].children.push(id);
            doc.nodes.push(Node {
                kind,
                parent: Some(parent),
                pos,
                children: Vec::new(),
                elem_pos,
                elements: 0,
            });
            for c in children(&handle).into_iter().rev() {
                stack.push((c, id, depth + 1));
            }
        }
        Ok(doc)
    }

    /// `html.parser=xml`: the document as XML, as Jsoup's XML parser reads it. Names keep
    /// their case and are taken as written, nothing is added (no `html`, `head` or `body`),
    /// and no element has HTML's rules: all are foreign ([`Ns::Other`]). CDATA is text;
    /// the XML declaration, processing instructions and the doctype are left out.
    fn parse_xml(bytes: &[u8]) -> Result<Doc, FxError> {
        let mut doc = Self::read_xml(bytes)?;
        doc.xml = true;
        Ok(doc)
    }

    fn read_xml(bytes: &[u8]) -> Result<Doc, FxError> {
        use quick_xml::events::Event;
        let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
        check_attributes(bytes)?;
        let parse = |e: &dyn std::fmt::Display| FxError::Parse(format!("XML: {e}"));
        let mut reader = quick_xml::Reader::from_reader(bytes);
        let mut doc = Doc {
            xml: false,
            nodes: vec![Node {
                kind: Kind::Document,
                parent: None,
                pos: 0,
                children: Vec::new(),
                elem_pos: 0,
                elements: 0,
            }],
        };
        let mut open: Vec<usize> = vec![0];
        let mut buf = Vec::new();
        loop {
            let event = reader.read_event_into(&mut buf).map_err(|e| parse(&e))?;
            let parent = *open.last().unwrap_or(&0);
            let (kind, opens) = match event {
                Event::Eof => break,
                Event::Start(e) | Event::Empty(e) if open.len() > MAX_DEPTH => {
                    let _ = e;
                    return Err(FxError::Unsupported(format!(
                        "XML (html.parser=xml) nested more than {MAX_DEPTH} elements deep"
                    )));
                }
                Event::Start(e) => (xml_element(&e)?, true),
                Event::Empty(e) => (xml_element(&e)?, false),
                Event::End(_) => {
                    if open.len() > 1 {
                        open.pop();
                    }
                    buf.clear();
                    continue;
                }
                Event::Text(t) => (
                    Kind::Text(
                        t.xml_content(quick_xml::XmlVersion::Implicit1_0)
                            .map_err(|e| parse(&e))?
                            .into_owned(),
                    ),
                    false,
                ),
                Event::GeneralRef(r) => {
                    let text = match r.resolve_char_ref().ok().flatten() {
                        Some(c) => c.to_string(),
                        None => {
                            let name = r.decode().map_err(|e| parse(&e))?;
                            match quick_xml::escape::resolve_predefined_entity(&name) {
                                Some(t) => t.to_owned(),
                                None => format!("&{name};"),
                            }
                        }
                    };
                    (Kind::Text(text), false)
                }
                Event::CData(t) => (
                    Kind::Text(String::from_utf8_lossy(&t.into_inner()).into_owned()),
                    false,
                ),
                Event::Comment(t) => (
                    Kind::Comment(String::from_utf8_lossy(&t.into_inner()).into_owned()),
                    false,
                ),
                _ => {
                    buf.clear();
                    continue;
                }
            };
            buf.clear();
            // Adjacent text (around entity references) is one text node.
            if let Kind::Text(more) = &kind
                && let Some(&last) = doc.nodes[parent].children.last()
                && let Kind::Text(text) = &mut doc.nodes[last].kind
            {
                text.push_str(more);
                continue;
            }
            let id = doc.nodes.len();
            let pos = doc.nodes[parent].children.len();
            let elem_pos = doc.nodes[parent].elements;
            if matches!(kind, Kind::Element { .. }) {
                doc.nodes[parent].elements += 1;
            }
            doc.nodes[parent].children.push(id);
            doc.nodes.push(Node {
                kind,
                parent: Some(parent),
                pos,
                children: Vec::new(),
                elem_pos,
                elements: 0,
            });
            if opens {
                open.push(id);
            }
        }
        Ok(doc)
    }

    /// Whether text in `parent` is data: script and style text and the like.
    fn is_data_parent(&self, parent: usize) -> bool {
        match &self.nodes[parent].kind {
            Kind::Element {
                name, ns: Ns::Html, ..
            } => matches!(
                name.as_str(),
                "iframe" | "noembed" | "noframes" | "script" | "style" | "xmp"
            ),
            Kind::Element {
                name, ns: Ns::Svg, ..
            } => name == "script",
            _ => false,
        }
    }

    fn element(&self, i: usize) -> Option<(&str, Ns, &[Attr])> {
        match &self.nodes[i].kind {
            Kind::Element { name, ns, attrs } => Some((name, *ns, attrs)),
            _ => None,
        }
    }

    fn is_element(&self, i: usize) -> bool {
        matches!(self.nodes[i].kind, Kind::Element { .. })
    }

    fn is_text(&self, i: usize) -> bool {
        matches!(self.nodes[i].kind, Kind::Text(_))
    }

    /// A text node of whitespace only.
    fn is_blank_text(&self, i: Option<usize>) -> bool {
        matches!(i.map(|i| &self.nodes[i].kind), Some(Kind::Text(t)) if is_blank(t))
    }

    fn parent_element(&self, i: usize) -> Option<usize> {
        self.nodes[i].parent.filter(|&p| self.is_element(p))
    }

    fn prev(&self, i: usize) -> Option<usize> {
        let n = &self.nodes[i];
        let p = n.parent?;
        n.pos.checked_sub(1).map(|k| self.nodes[p].children[k])
    }

    fn next(&self, i: usize) -> Option<usize> {
        let n = &self.nodes[i];
        let p = n.parent?;
        self.nodes[p].children.get(n.pos + 1).copied()
    }

    fn tag(&self, i: usize) -> Tag {
        match self.element(i) {
            Some((name, ns, _)) => Tag::of(name, ns),
            None => Tag::default(),
        }
    }

    fn attr(&self, i: usize, key: &str) -> Option<&str> {
        self.element(i)?
            .2
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    fn classes(&self, i: usize) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for c in self
            .attr(i, "class")
            .unwrap_or_default()
            .split(|c: char| is_whitespace(c))
            .filter(|c| !c.is_empty())
        {
            if !out.contains(&c) {
                out.push(c);
            }
        }
        out
    }

    fn child_elements(&self, i: usize) -> impl Iterator<Item = usize> + '_ {
        self.nodes[i]
            .children
            .iter()
            .copied()
            .filter(|&c| self.is_element(c))
    }

    /// Whether a text node under `i` is not blank.
    fn has_text(&self, i: usize) -> bool {
        let mut stack = vec![i];
        while let Some(n) = stack.pop() {
            match &self.nodes[n].kind {
                Kind::Text(t) if !is_blank(t) => return true,
                _ => stack.extend(&self.nodes[n].children),
            }
        }
        false
    }

    /// The elements `selector` matches, in document order.
    fn select(&self, selector: &Selector) -> Vec<usize> {
        let mut memo = Memo::default();
        (1..self.nodes.len())
            .filter(|&i| self.is_element(i) && selector.matches(self, i, &mut memo))
            .collect()
    }
}

/// How many attributes one start tag may have.
const MAX_ATTRIBUTES: usize = 10_000;

/// Refuses a start tag with more than [`MAX_ATTRIBUTES`] attributes before html5ever reads
/// it: html5ever compares each attribute of a tag with the ones before it, so a tag with
/// 200,000 attributes takes half a minute. The scan is rough on purpose: it also counts in
/// comments, scripts and other text, and in end tags, which only matters past the limit (a
/// `<` followed by 10,000 words with no `>`). Skipping such regions would have to follow
/// html5ever's tokenizer states exactly (comments that end early, raw text that is not raw
/// in SVG or MathML, start tags the tree builder ignores), or a tag inside one would get past
/// the scan.
fn check_attributes(bytes: &[u8]) -> Result<(), FxError> {
    let mut i = 0;
    let n = bytes.len();
    while i < n {
        // A start or end tag (html5ever reads attributes on both).
        let name = if bytes.get(i + 1) == Some(&b'/') {
            i + 2
        } else {
            i + 1
        };
        if bytes[i] != b'<' || !bytes.get(name).is_some_and(u8::is_ascii_alphabetic) {
            i += 1;
            continue;
        }
        // The tag name.
        i = name;
        while i < n && !matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0c | b'/' | b'>') {
            i += 1;
        }
        let mut count = 0;
        while i < n && bytes[i] != b'>' {
            match bytes[i] {
                b' ' | b'\t' | b'\n' | b'\r' | 0x0c | b'/' => i += 1,
                b'=' => {
                    // A value: quoted, or up to whitespace or `>`.
                    i += 1;
                    while i < n && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0c) {
                        i += 1;
                    }
                    match bytes.get(i) {
                        Some(&q @ (b'"' | b'\'')) => {
                            i += 1;
                            while i < n && bytes[i] != q {
                                i += 1;
                            }
                            i += 1;
                        }
                        _ => {
                            while i < n
                                && !matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0c | b'>')
                            {
                                i += 1;
                            }
                        }
                    }
                }
                _ => {
                    // An attribute name.
                    count += 1;
                    if count > MAX_ATTRIBUTES {
                        return Err(FxError::Unsupported(format!(
                            "HTML: a tag with more than {MAX_ATTRIBUTES} attributes"
                        )));
                    }
                    while i < n
                        && !matches!(
                            bytes[i],
                            b' ' | b'\t' | b'\n' | b'\r' | 0x0c | b'/' | b'>' | b'='
                        )
                    {
                        i += 1;
                    }
                }
            }
        }
    }
    Ok(())
}

fn children(handle: &Handle) -> Vec<Handle> {
    let mut out = handle.children.borrow().clone();
    // The content of a <template> is its children, as in Jsoup.
    if let NodeData::Element {
        template_contents, ..
    } = &handle.data
        && let Some(t) = &*template_contents.borrow()
    {
        out.extend(t.children.borrow().iter().cloned());
    }
    out
}

/// Jsoup's whitespace: space, tab, line feed, form feed, carriage return.
fn is_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\u{c}' | '\r')
}

fn is_blank(s: &str) -> bool {
    s.chars().all(is_whitespace)
}

/// Java's `String.trim`: drops characters up to U+0020 at both ends.
fn java_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c <= ' ')
}

/// What Jsoup's default tag set says of a tag.
#[derive(Default, Clone, Copy)]
struct Tag {
    known: bool,
    block: bool,
    void: bool,
    inline_container: bool,
    preserve_whitespace: bool,
    text_boundary: bool,
}

impl Tag {
    fn of(name: &str, ns: Ns) -> Tag {
        let lower = name.to_ascii_lowercase();
        let n = lower.as_str();
        match ns {
            Ns::Html => {
                const BLOCK: &[&str] = &[
                    "html",
                    "head",
                    "body",
                    "frameset",
                    "script",
                    "noscript",
                    "style",
                    "meta",
                    "link",
                    "title",
                    "frame",
                    "noframes",
                    "section",
                    "nav",
                    "aside",
                    "hgroup",
                    "header",
                    "footer",
                    "p",
                    "h1",
                    "h2",
                    "h3",
                    "h4",
                    "h5",
                    "h6",
                    "dialog",
                    "search",
                    "ul",
                    "ol",
                    "pre",
                    "div",
                    "blockquote",
                    "hr",
                    "address",
                    "figure",
                    "figcaption",
                    "form",
                    "fieldset",
                    "dl",
                    "dt",
                    "dd",
                    "li",
                    "table",
                    "caption",
                    "thead",
                    "tfoot",
                    "tbody",
                    "colgroup",
                    "col",
                    "tr",
                    "th",
                    "td",
                    "details",
                    "menu",
                    "plaintext",
                    "template",
                    "article",
                    "main",
                    "center",
                    "dir",
                    "applet",
                    "marquee",
                    "listing",
                ];
                const INLINE: &[&str] = &[
                    "object", "base", "font", "tt", "i", "b", "u", "big", "small", "em", "strong",
                    "dfn", "code", "samp", "kbd", "var", "cite", "abbr", "time", "acronym", "mark",
                    "ruby", "rt", "rp", "rtc", "a", "img", "wbr", "map", "q", "sub", "sup", "bdo",
                    "iframe", "embed", "span", "input", "select", "textarea", "label", "audio",
                    "video", "canvas", "optgroup", "option", "legend", "datalist", "keygen",
                    "output", "progress", "meter", "area", "param", "source", "track", "summary",
                    "command", "device", "basefont", "bgsound", "menuitem", "data", "bdi", "s",
                    "strike", "nobr", "ins", "del", "button", "picture", "slot", "rb",
                ];
                const INLINE_CONTAINER: &[&str] = &[
                    "title", "p", "h1", "h2", "h3", "h4", "h5", "h6", "pre", "address", "li", "th",
                    "td", "script", "style",
                ];
                const VOID: &[&str] = &[
                    "meta", "link", "base", "frame", "img", "br", "wbr", "embed", "hr", "input",
                    "keygen", "col", "command", "device", "area", "basefont", "bgsound",
                    "menuitem", "param", "source", "track",
                ];
                const PRESERVE: &[&str] = &["pre", "plaintext", "title", "textarea", "script"];
                const OTHER: &[&str] = &["noembed", "xmp"];
                const BOUNDARY: &[&str] = &[
                    "button", "input", "select", "textarea", "option", "output", "progress",
                    "meter", "img", "picture", "audio", "video", "canvas", "object", "embed",
                    "iframe",
                ];
                let block = BLOCK.contains(&n);
                Tag {
                    known: block || INLINE.contains(&n) || VOID.contains(&n) || OTHER.contains(&n),
                    block,
                    void: VOID.contains(&n),
                    inline_container: INLINE_CONTAINER.contains(&n),
                    preserve_whitespace: PRESERVE.contains(&n),
                    text_boundary: BOUNDARY.contains(&n),
                }
            }
            Ns::MathMl => Tag {
                known: matches!(n, "math" | "mi" | "mo" | "msup" | "mn" | "mtext"),
                block: n == "math",
                ..Tag::default()
            },
            Ns::Svg => Tag {
                known: matches!(n, "svg" | "femerge" | "femergenode" | "text" | "script"),
                block: matches!(n, "svg" | "femerge" | "femergenode"),
                ..Tag::default()
            },
            Ns::Other => Tag::default(),
        }
    }
}

/// Writes the containers of the selected elements.
struct Writer<'d> {
    doc: &'d Doc,
    html: Option<NamedNode>,
    text: Option<NamedNode>,
    /// Bytes of `innerHTML` and `innerText` written, and how many may be.
    spent: usize,
    max: usize,
    /// The path of each element made so far.
    names: HashMap<usize, String>,
    /// Child elements by parent and tag, and the counts [`Writer::same`] found.
    groups: HashMap<(usize, String), Group>,
    counts: HashMap<(usize, String, String), usize>,
}

/// The child elements of one tag of a parent: how many, and which have each class.
#[derive(Default)]
struct Group {
    all: usize,
    by_class: HashMap<String, Vec<usize>>,
}

impl Writer<'_> {
    fn container(&mut self, sink: &Sink<'_>, e: usize) -> NamedOrBlankNode {
        sink.container(|| {
            let mut iri = String::new();
            escape_iri(&self.local_name(e), &mut iri);
            iri
        })
    }

    /// SPARQL Anything's path of an element: `html/body/div.a.b/p:2`.
    fn local_name(&mut self, e: usize) -> String {
        if let Some(n) = self.names.get(&e) {
            return n.clone();
        }
        let doc = self.doc;
        let (name, _, _) = doc.element(e).expect("an element");
        let mut own = name.replace(':', "|");
        let classes = doc.classes(e);
        for c in &classes {
            own.push('.');
            own.push_str(c);
        }
        let out = match doc.parent_element(e) {
            None => own,
            Some(p) => {
                if self.same(p, name, &classes) > 1 {
                    own.push_str(&format!(":{}", doc.nodes[e].elem_pos + 1));
                }
                format!("{}/{own}", self.local_name(p))
            }
        };
        self.names.insert(e, out.clone());
        out
    }

    /// How many child elements of `p` have the tag `name` and all of `classes` (ignoring
    /// case), as Jsoup's `p.select(" > name.class1.class2")`. Counted once per parent, tag
    /// and set of classes, from an index of the parent's children by tag and class, so that
    /// a parent with many children costs no more than a few passes over them.
    fn same(&mut self, p: usize, name: &str, classes: &[&str]) -> usize {
        let doc = self.doc;
        let name = name.replace(':', "|").to_lowercase();
        let mut set: Vec<String> = classes.iter().map(|c| c.to_lowercase()).collect();
        set.sort();
        set.dedup();
        let key = (p, name.clone(), set.join(" "));
        if let Some(&n) = self.counts.get(&key) {
            return n;
        }
        let group = self.groups.entry((p, name.clone())).or_insert_with(|| {
            let mut g = Group::default();
            for s in doc.child_elements(p) {
                let (n, _, _) = doc.element(s).expect("an element");
                if n.replace(':', "|").to_lowercase() != name {
                    continue;
                }
                g.all += 1;
                let mut own: Vec<String> =
                    doc.classes(s).iter().map(|c| c.to_lowercase()).collect();
                own.sort();
                own.dedup();
                for c in own {
                    g.by_class.entry(c).or_default().push(s);
                }
            }
            g
        });
        let n = if set.is_empty() {
            group.all
        } else {
            let smallest = set
                .iter()
                .map(|c| group.by_class.get(c).map_or(&[][..], Vec::as_slice))
                .min_by_key(|l| l.len())
                .unwrap_or(&[]);
            smallest
                .iter()
                .filter(|&&s| {
                    let theirs = doc.classes(s);
                    set.iter()
                        .all(|c| theirs.iter().any(|t| t.to_lowercase() == *c))
                })
                .count()
        };
        self.counts.insert(key, n);
        n
    }

    fn spend(&mut self, n: usize) -> Result<(), FxError> {
        self.spent = self.spent.saturating_add(n);
        if self.spent > self.max {
            return Err(FxError::Unsupported(format!(
                "HTML: the innerHTML and innerText of its elements hold more than {} bytes; \
                 select fewer elements (html.selector) or leave those predicates out",
                self.max
            )));
        }
        Ok(())
    }

    fn populate(
        &mut self,
        sink: &mut Sink<'_>,
        e: usize,
        node: &NamedOrBlankNode,
    ) -> Result<(), FxError> {
        // Iterative, children after their parent in document order.
        let mut todo = vec![(e, node.clone())];
        while let Some((e, node)) = todo.pop() {
            let doc = self.doc;
            if let Some(p) = self.html.clone() {
                let html = inner_html(doc, e);
                self.spend(html.len())?;
                if !java_trim(&html).is_empty()
                    && let Some(v) = sink.string(&html)
                {
                    sink.emit(&node, &p, v);
                }
            }
            if let Some(p) = self.text.clone() {
                let text = inner_text(doc, e);
                self.spend(text.len())?;
                if !java_trim(&text).is_empty()
                    && let Some(v) = sink.string(&text)
                {
                    sink.emit(&node, &p, v);
                }
            }
            let (name, _, attrs) = doc.element(e).expect("an element");
            sink.class(&node, self.uri(e, name));
            for (k, v) in attrs {
                if let (Some(p), Some(v)) = (sink.predicate(self.uri(e, k)), sink.string(v)) {
                    sink.emit(&node, &p, v);
                }
            }
            let mut n = 0;
            let mut later = Vec::new();
            for &c in &doc.nodes[e].children {
                let value = match &doc.nodes[c].kind {
                    Kind::Element { .. } => None,
                    Kind::Text(_) => Some(outer_text(doc, c)),
                    Kind::Data(d) => Some(d.clone()),
                    Kind::Comment(t) => Some(format!("<!--{t}-->")),
                    Kind::Document => continue,
                };
                if value.as_deref().is_some_and(|v| java_trim(v).is_empty()) {
                    continue;
                }
                n += 1;
                match value {
                    None => {
                        let child = self.container(sink, c);
                        sink.slot(&node, n, child.clone().into());
                        later.push((c, child));
                    }
                    Some(v) => {
                        if let Some(v) = sink.string(&v) {
                            sink.slot(&node, n, v);
                        }
                    }
                }
            }
            todo.extend(later.into_iter().rev());
        }
        Ok(())
    }

    /// The IRI of a tag or attribute name on element `e`: in the namespace an `xmlns:` attribute
    /// declares for its prefix, else in the XHTML one.
    fn uri(&self, e: usize, name: &str) -> String {
        // As Java's `split(":")`: trailing empty parts dropped.
        let mut parts: Vec<&str> = name.split(':').collect();
        while parts.last() == Some(&"") {
            parts.pop();
        }
        let declared = match parts.as_slice() {
            [prefix, local] => {
                let key = format!("xmlns:{prefix}");
                let mut at = Some(e);
                let mut ns = None;
                while let Some(i) = at {
                    if let Some(v) = self.doc.attr_exact(i, &key) {
                        ns = Some((v.to_owned(), *local));
                        break;
                    }
                    at = self.doc.parent_element(i);
                }
                ns
            }
            _ => None,
        };
        let (mut ns, local) = declared.unwrap_or_else(|| (XHTML.to_owned(), name));
        if !ns.ends_with('#') {
            ns.push('#');
        }
        url_encode(local, &mut ns);
        ns
    }
}

impl Doc {
    /// An attribute by its exact name.
    fn attr_exact(&self, i: usize, key: &str) -> Option<&str> {
        self.element(i)?
            .2
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// Java's `URLEncoder.encode(s, UTF_8)`: letters, digits and `.-*_` kept, space as `+`, the rest
/// percent-encoded.
fn url_encode(s: &str, out: &mut String) {
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'-' | b'*' | b'_' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
}

/// Jsoup's `Element.text()`.
fn inner_text(doc: &Doc, e: usize) -> String {
    let mut out = String::new();
    text_of(doc, e, &mut out);
    java_trim(&out).to_owned()
}

fn text_of(doc: &Doc, n: usize, out: &mut String) {
    let last_white = |out: &String| out.ends_with(' ');
    match &doc.nodes[n].kind {
        Kind::Text(t) => {
            if preserves_whitespace(doc, doc.nodes[n].parent) {
                out.push_str(t);
            } else {
                let strip = last_white(out);
                let (mut last, mut reached) = (false, false);
                for c in t.chars() {
                    if is_whitespace(c) || c == '\u{a0}' {
                        if (strip && !reached) || last {
                            continue;
                        }
                        out.push(' ');
                        last = true;
                    } else if c != '\u{200b}' && c != '\u{ad}' {
                        out.push(c);
                        last = false;
                        reached = true;
                    }
                }
            }
        }
        Kind::Element { .. } => {
            let tag = doc.tag(n);
            let (name, _, _) = doc.element(n).expect("an element");
            let leading = tag.block
                || name == "br"
                || tag.text_boundary && !doc.nodes[n].children.is_empty() && doc.has_text(n);
            if !out.is_empty() && leading && !last_white(out) {
                out.push(' ');
            }
            for &c in &doc.nodes[n].children {
                text_of(doc, c, out);
            }
            let trailing = tag.text_boundary
                || tag.block
                || doc.nodes[n]
                    .children
                    .iter()
                    .any(|&c| doc.is_element(c) && doc.tag(c).block);
            let next = doc.next(n);
            let next_ok =
                next.is_some_and(|x| doc.is_text(x) || doc.is_element(x) && !doc.tag(x).block);
            if trailing && next_ok && !last_white(out) {
                out.push(' ');
            }
        }
        _ => {}
    }
}

/// Jsoup's `preserveWhitespace(node)`: the node or one of its five nearest ancestors keeps
/// whitespace.
fn preserves_whitespace(doc: &Doc, node: Option<usize>) -> bool {
    let mut at = node.filter(|&n| doc.is_element(n));
    for _ in 0..6 {
        let Some(e) = at else { return false };
        if doc.tag(e).preserve_whitespace {
            return true;
        }
        at = doc.parent_element(e);
    }
    false
}

/// Jsoup's `Element.html()`: the children, pretty-printed, trimmed; for XML, as they are.
fn inner_html(doc: &Doc, e: usize) -> String {
    if doc.xml {
        let mut out = String::new();
        for &c in &doc.nodes[e].children {
            xml_html(doc, c, &mut out);
        }
        return out;
    }
    let children = &doc.nodes[e].children;
    let Some(&first) = children.first() else {
        return String::new();
    };
    let mut p = Printer::new(doc, first);
    for &c in children {
        p.traverse(c, 0);
    }
    java_trim(&p.out).to_owned()
}

/// A node as Jsoup writes a document from its XML parser: not pretty-printed, text escaped,
/// every attribute with a value, and an empty element as `<a></a>`.
fn xml_html(doc: &Doc, n: usize, out: &mut String) {
    match &doc.nodes[n].kind {
        Kind::Text(t) | Kind::Data(t) => {
            for c in t.chars() {
                match c {
                    '\u{a0}' => out.push_str("&#xa0;"),
                    c => escape_char(c, out, true),
                }
            }
        }
        Kind::Comment(c) => {
            out.push_str("<!--");
            out.push_str(c);
            out.push_str("-->");
        }
        Kind::Element { name, attrs, .. } => {
            out.push('<');
            out.push_str(name);
            for (k, v) in attrs {
                out.push(' ');
                out.push_str(&valid_key(k));
                out.push_str("=\"");
                escape(v, out, false);
                out.push('"');
            }
            let children = &doc.nodes[n].children;
            out.push('>');
            for &c in children {
                xml_html(doc, c, out);
            }
            out.push_str("</");
            out.push_str(name);
            out.push('>');
        }
        Kind::Document => {}
    }
}

/// Jsoup's `TextNode.outerHtml()`.
fn outer_text(doc: &Doc, t: usize) -> String {
    let mut p = Printer::new(doc, t);
    p.traverse(t, 0);
    p.out
}

/// Jsoup 1.23's pretty printer (`Printer.Pretty`), indenting by one space.
struct Printer<'d> {
    doc: &'d Doc,
    root: usize,
    preserve: bool,
    out: String,
}

impl<'d> Printer<'d> {
    fn new(doc: &'d Doc, root: usize) -> Self {
        let mut preserve = false;
        let mut at = Some(root);
        while let Some(n) = at {
            if doc.is_element(n) && doc.tag(n).preserve_whitespace {
                preserve = true;
                break;
            }
            at = doc.nodes[n].parent;
        }
        Printer {
            doc,
            root,
            preserve,
            out: String::new(),
        }
    }

    fn traverse(&mut self, n: usize, depth: usize) {
        let doc = self.doc;
        match &doc.nodes[n].kind {
            Kind::Text(t) => self.text(n, t, depth),
            Kind::Element { name, attrs, .. } => {
                if self.should_indent(Some(n)) {
                    self.indent(depth);
                }
                self.out.push('<');
                self.out.push_str(name);
                for (k, v) in attrs {
                    self.out.push(' ');
                    self.out.push_str(&valid_key(k));
                    // html5ever gives `a` and `a=""` the same empty value; Jsoup writes the
                    // first as `a`, which is by far the more common in pages.
                    if !v.is_empty() && !(v.eq_ignore_ascii_case(k) && is_boolean_attribute(k)) {
                        self.out.push_str("=\"");
                        escape(v, &mut self.out, false);
                        self.out.push('"');
                    }
                }
                let tag = doc.tag(n);
                let empty = doc.nodes[n].children.is_empty();
                let foreign = !matches!(doc.element(n).map(|e| e.1), Some(Ns::Html));
                if empty {
                    // Jsoup closes an empty foreign element (SVG, MathML) with ` />` when its
                    // tag was written self-closing, as it nearly always is.
                    if foreign {
                        self.out.push_str(" />");
                    } else if tag.void {
                        self.out.push('>');
                    } else {
                        self.out.push_str(&format!("></{name}>"));
                    }
                } else {
                    self.out.push('>');
                }
                if tag.preserve_whitespace {
                    self.preserve = true;
                }
                for &c in &doc.nodes[n].children {
                    self.traverse(c, depth + 1);
                }
                // Tail.
                let first = doc.nodes[n].children.first().copied();
                if self.should_indent(self.next_non_blank(first)) {
                    self.indent(depth);
                }
                if !empty {
                    self.out.push_str(&format!("</{name}>"));
                }
                if self.preserve && tag.preserve_whitespace {
                    let mut at = doc.parent_element(n);
                    let mut keep = false;
                    while let Some(p) = at {
                        if doc.tag(p).preserve_whitespace {
                            keep = true;
                            break;
                        }
                        at = doc.parent_element(p);
                    }
                    if !keep {
                        self.preserve = false;
                    }
                }
            }
            Kind::Data(d) => {
                if self.should_indent(Some(n)) {
                    self.indent(depth);
                }
                self.out.push_str(d);
            }
            Kind::Comment(c) => {
                if self.should_indent(Some(n)) {
                    self.indent(depth);
                }
                self.out.push_str("<!--");
                self.out.push_str(c);
                self.out.push_str("-->");
            }
            Kind::Document => {}
        }
    }

    fn text(&mut self, n: usize, t: &str, depth: usize) {
        let doc = self.doc;
        let mut options = Text::default();
        if !self.preserve {
            options.normalise = true;
            self.trim(n, &mut options);
            let parent = doc.nodes[n].parent;
            if !is_blank(t) && self.is_block(parent) && self.should_indent(Some(n)) {
                self.indent(depth);
            }
        }
        let (mut last_white, mut reached, mut skipped) = (false, false, false);
        for c in t.chars() {
            if options.normalise {
                if is_whitespace(c) {
                    if options.trim_leading && !reached {
                        continue;
                    }
                    if last_white {
                        continue;
                    }
                    if options.trim_trailing {
                        skipped = true;
                        continue;
                    }
                    self.out.push(' ');
                    last_white = true;
                    continue;
                }
                last_white = false;
                reached = true;
                if skipped {
                    self.out.push(' ');
                    skipped = false;
                }
            }
            escape_char(c, &mut self.out, true);
        }
    }

    fn trim(&self, n: usize, options: &mut Text) {
        let doc = self.doc;
        if !self.is_block(doc.nodes[n].parent) {
            return;
        }
        let prev = doc.prev(n);
        let next = doc.next(n);
        if !prev.is_some_and(|p| doc.is_element(p) && !self.is_block(Some(p)))
            && (prev.is_none() || prev.is_some_and(|p| !doc.is_text(p)) && self.should_indent(prev))
        {
            options.trim_leading = true;
        }
        if next.is_none() || next.is_some_and(|x| !doc.is_text(x)) && self.should_indent(next) {
            options.trim_trailing = true;
        } else {
            let next = self.next_non_blank(next);
            if let Some(x) = next
                && let Kind::Text(t) = &doc.nodes[x].kind
                && t.starts_with(is_whitespace)
            {
                options.trim_trailing = true;
            }
        }
    }

    fn indent(&mut self, depth: usize) {
        self.out.push('\n');
        for _ in 0..depth.min(30) {
            self.out.push(' ');
        }
    }

    fn should_indent(&self, node: Option<usize>) -> bool {
        let doc = self.doc;
        let Some(n) = node else { return false };
        if n == self.root || self.preserve || doc.is_blank_text(Some(n)) {
            return false;
        }
        if self.is_block(Some(n)) {
            return true;
        }
        let prev = self.previous_non_blank(n);
        if self.is_block(prev) {
            return true;
        }
        let parent = doc.parent_element(n);
        let Some(p) = parent else { return false };
        if !self.is_block(Some(p)) || doc.tag(p).inline_container || !has_non_text_nodes(doc, p) {
            return false;
        }
        match prev {
            None => true,
            Some(s) => !doc.is_text(s) && (self.is_block(Some(s)) || !doc.is_element(s)),
        }
    }

    fn is_block(&self, node: Option<usize>) -> bool {
        let doc = self.doc;
        let Some(n) = node else { return false };
        let Some((name, _, _)) = doc.element(n) else {
            return false;
        };
        if name == "br" {
            return true;
        }
        let tag = doc.tag(n);
        tag.block
            || !tag.known
                && (matches!(
                    doc.nodes[n].parent.map(|p| &doc.nodes[p].kind),
                    Some(Kind::Document)
                ) || has_child_blocks(doc, n))
    }

    fn previous_non_blank(&self, n: usize) -> Option<usize> {
        let mut prev = self.doc.prev(n);
        while self.doc.is_blank_text(prev) {
            prev = prev.and_then(|p| self.doc.prev(p));
        }
        prev
    }

    fn next_non_blank(&self, mut n: Option<usize>) -> Option<usize> {
        while self.doc.is_blank_text(n) {
            n = n.and_then(|x| self.doc.next(x));
        }
        n
    }
}

#[derive(Default)]
struct Text {
    normalise: bool,
    trim_leading: bool,
    trim_trailing: bool,
}

/// Whether one of the first five child elements is a block or an unknown tag.
fn has_child_blocks(doc: &Doc, e: usize) -> bool {
    doc.child_elements(e).take(5).any(|c| {
        let t = doc.tag(c);
        t.block || !t.known
    })
}

/// Whether one of the first five children is not text.
fn has_non_text_nodes(doc: &Doc, e: usize) -> bool {
    doc.nodes[e]
        .children
        .iter()
        .take(5)
        .any(|&c| !doc.is_text(c))
}

fn is_boolean_attribute(key: &str) -> bool {
    const BOOLEAN: &[&str] = &[
        "allowfullscreen",
        "async",
        "autofocus",
        "checked",
        "compact",
        "declare",
        "default",
        "defer",
        "disabled",
        "formnovalidate",
        "hidden",
        "inert",
        "ismap",
        "itemscope",
        "multiple",
        "muted",
        "nohref",
        "noresize",
        "noshade",
        "novalidate",
        "nowrap",
        "open",
        "readonly",
        "required",
        "reversed",
        "seamless",
        "selected",
        "sortable",
        "truespeed",
        "typemustmatch",
    ];
    BOOLEAN.contains(&key.to_ascii_lowercase().as_str())
}

/// An attribute name as Jsoup writes it in HTML: runs of control characters, space, `"`, `'`,
/// `/` and `=` become `_`.
fn valid_key(key: &str) -> String {
    if key.is_empty() {
        return "_".into();
    }
    let bad = |c: char| {
        ('\0'..='\u{1f}').contains(&c)
            || ('\u{7f}'..='\u{9f}').contains(&c)
            || matches!(c, ' ' | '"' | '\'' | '/' | '=')
    };
    let mut out = String::with_capacity(key.len());
    let mut in_run = false;
    for c in key.chars() {
        if bad(c) {
            if !in_run {
                out.push('_');
            }
            in_run = true;
        } else {
            out.push(c);
            in_run = false;
        }
    }
    out
}

fn escape(s: &str, out: &mut String, text: bool) {
    for c in s.chars() {
        escape_char(c, out, text);
    }
}

/// Jsoup's base escaping, for text or (without `text`) an attribute value.
fn escape_char(c: char, out: &mut String, text: bool) {
    match c {
        '&' => out.push_str("&amp;"),
        '\u{a0}' => out.push_str("&nbsp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        '"' if !text => out.push_str("&quot;"),
        '\t' | '\n' | '\r' => out.push(c),
        c if (c as u32) < 0x20 => out.push_str(&format!("&#x{:x};", c as u32)),
        c => out.push(c),
    }
}

/// A CSS selector list.
#[derive(Debug)]
struct Selector(Vec<Complex>);

/// Compound selectors joined by combinators, read right to left: `parts[0]` is the subject.
#[derive(Debug)]
struct Complex {
    parts: Vec<(Compound, Option<Combinator>)>,
}

#[derive(Debug, Clone, Copy)]
enum Combinator {
    Descendant,
    Child,
    Adjacent,
    Sibling,
}

#[derive(Debug, Default)]
struct Compound(Vec<Simple>);

#[derive(Debug)]
enum Simple {
    Tag(String),
    Id(String),
    Class(String),
    Attr(String, Option<(char, String)>),
    Root,
    FirstChild,
    LastChild,
    OnlyChild,
    NthChild(usize),
}

impl Selector {
    fn parse(s: &str) -> Result<Selector, FxError> {
        let error = |m: &str| FxError::Unsupported(format!("html.selector {s:?}: {m}"));
        let mut list = Vec::new();
        for group in split_top(s, ',') {
            let chars: Vec<char> = group.trim().chars().collect();
            if chars.is_empty() {
                return Err(error("an empty selector"));
            }
            let mut parts: Vec<(Compound, Option<Combinator>)> = Vec::new();
            let mut i = 0;
            let mut pending: Option<Combinator> = None;
            while i < chars.len() {
                // Combinator.
                let mut saw_space = false;
                while i < chars.len() && chars[i].is_whitespace() {
                    saw_space = true;
                    i += 1;
                }
                if i < chars.len() && matches!(chars[i], '>' | '+' | '~') {
                    pending = Some(match chars[i] {
                        '>' => Combinator::Child,
                        '+' => Combinator::Adjacent,
                        _ => Combinator::Sibling,
                    });
                    i += 1;
                    while i < chars.len() && chars[i].is_whitespace() {
                        i += 1;
                    }
                } else if saw_space && !parts.is_empty() && i < chars.len() {
                    pending = Some(Combinator::Descendant);
                }
                if i >= chars.len() {
                    break;
                }
                let (compound, next) = parse_compound(&chars, i).map_err(|m| error(&m))?;
                if compound.0.is_empty() {
                    return Err(error(&format!("unexpected {:?}", chars[i])));
                }
                if parts.is_empty() && pending.is_some() {
                    return Err(error("starts with a combinator"));
                }
                if let Some(last) = parts.last_mut() {
                    last.1 = Some(
                        pending
                            .take()
                            .ok_or_else(|| error("a missing combinator"))?,
                    );
                }
                parts.push((compound, None));
                i = next;
            }
            if pending.is_some() {
                return Err(error("ends with a combinator"));
            }
            // Right to left: the subject first, each with the combinator to the one on its left.
            let mut rtl: Vec<(Compound, Option<Combinator>)> = Vec::new();
            let mut combinators: Vec<Option<Combinator>> = parts.iter().map(|p| p.1).collect();
            combinators.insert(0, None);
            combinators.pop();
            for ((c, _), comb) in parts.into_iter().zip(combinators).rev() {
                rtl.push((c, comb));
            }
            list.push(Complex { parts: rtl });
        }
        Ok(Selector(list))
    }

    /// `memo` keeps, per (complex selector, part, element), whether it matched, so that
    /// descendant and sibling combinators do not try the same ancestors again and again.
    fn matches(&self, doc: &Doc, e: usize, memo: &mut Memo) -> bool {
        self.0
            .iter()
            .enumerate()
            .any(|(i, c)| c.matches(doc, e, 0, i, memo))
    }
}

/// Splits on `sep` outside brackets, parentheses and quotes.
fn split_top(s: &str, sep: char) -> Vec<&str> {
    let (mut out, mut depth, mut quote, mut start) = (Vec::new(), 0i32, None, 0);
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '[' | '(') => depth += 1,
            (None, ']' | ')') => depth -= 1,
            (None, c) if c == sep && depth == 0 => {
                out.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

fn ident(chars: &[char], mut i: usize) -> (String, usize) {
    let mut out = String::new();
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            out.push(chars[i + 1]);
            i += 2;
        } else if c.is_alphanumeric() || matches!(c, '-' | '_' | '|') || (c as u32) > 0x7f {
            out.push(c);
            i += 1;
        } else {
            break;
        }
    }
    (out, i)
}

fn parse_compound(chars: &[char], mut i: usize) -> Result<(Compound, usize), String> {
    let mut out = Compound::default();
    if i < chars.len() && chars[i] == '*' {
        i += 1;
        out.0.push(Simple::Tag("*".into()));
    } else {
        let (name, next) = ident(chars, i);
        if !name.is_empty() {
            out.0.push(Simple::Tag(name.replace('|', ":")));
            i = next;
        }
    }
    while i < chars.len() {
        match chars[i] {
            '#' | '.' => {
                let (name, next) = ident(chars, i + 1);
                if name.is_empty() {
                    return Err(format!("a name after {:?}", chars[i]));
                }
                out.0.push(if chars[i] == '#' {
                    Simple::Id(name)
                } else {
                    Simple::Class(name)
                });
                i = next;
            }
            '[' => {
                let end = chars[i..]
                    .iter()
                    .position(|&c| c == ']')
                    .ok_or("an unclosed [")?
                    + i;
                let body: String = chars[i + 1..end].iter().collect();
                let op = ["^=", "$=", "*=", "~=", "="]
                    .iter()
                    .find_map(|op| body.find(op).map(|at| (at, *op)));
                let simple = match op {
                    None => Simple::Attr(body.trim().to_ascii_lowercase(), None),
                    Some((at, op)) => {
                        let key = body[..at].trim().to_ascii_lowercase();
                        let mut value = body[at + op.len()..].trim().to_owned();
                        if value.len() >= 2
                            && (value.starts_with('"') && value.ends_with('"')
                                || value.starts_with('\'') && value.ends_with('\''))
                        {
                            value = value[1..value.len() - 1].to_owned();
                        }
                        let kind = op.chars().next().expect("an operator");
                        Simple::Attr(key, Some((kind, value)))
                    }
                };
                if matches!(&simple, Simple::Attr(k, _) if k.is_empty()) {
                    return Err("an attribute selector without a name".into());
                }
                out.0.push(simple);
                i = end + 1;
            }
            ':' => {
                let (name, next) = ident(chars, i + 1);
                i = next;
                let simple = match name.to_ascii_lowercase().as_str() {
                    "root" => Simple::Root,
                    "first-child" => Simple::FirstChild,
                    "last-child" => Simple::LastChild,
                    "only-child" => Simple::OnlyChild,
                    "nth-child" => {
                        if chars.get(i) != Some(&'(') {
                            return Err(":nth-child needs (n)".into());
                        }
                        let end = chars[i..]
                            .iter()
                            .position(|&c| c == ')')
                            .ok_or("an unclosed (")?
                            + i;
                        let arg: String = chars[i + 1..end].iter().collect();
                        i = end + 1;
                        let n = arg
                            .trim()
                            .parse::<usize>()
                            .ok()
                            .filter(|&n| n > 0)
                            .ok_or_else(|| format!(":nth-child({arg}): only a number"))?;
                        Simple::NthChild(n)
                    }
                    other => return Err(format!("the pseudo-class :{other}")),
                };
                out.0.push(simple);
            }
            _ => break,
        }
    }
    Ok((out, i))
}

/// What selector matching has found so far: whether (complex selector, part, element)
/// matches, and whether an ancestor or earlier sibling of the element matches the part.
#[derive(Default)]
struct Memo {
    at: HashMap<(usize, usize, usize), bool>,
    before: HashMap<(usize, usize, usize, bool), bool>,
}

impl Complex {
    fn matches(&self, doc: &Doc, e: usize, k: usize, id: usize, memo: &mut Memo) -> bool {
        if let Some(&m) = memo.at.get(&(id, k, e)) {
            return m;
        }
        let m = self.matches_here(doc, e, k, id, memo);
        memo.at.insert((id, k, e), m);
        m
    }

    /// Whether an ancestor (`siblings` false) or an earlier sibling (true) of `e` matches
    /// part `k`. Each answer is carried forward from the nearest ancestor or earlier sibling
    /// already answered, so each element is visited once per part, however long the
    /// sibling list or deep the nesting.
    fn some_before(
        &self,
        doc: &Doc,
        e: usize,
        k: usize,
        id: usize,
        siblings: bool,
        memo: &mut Memo,
    ) -> bool {
        let step = |x: usize| {
            if siblings {
                previous_element(doc, x)
            } else {
                doc.parent_element(x)
            }
        };
        let key = |x: usize| (id, k, x, siblings);
        // The chain back to an answered element (or the first one), nearest last.
        let mut chain = vec![e];
        let mut known = false;
        while let Some(&x) = chain.last() {
            if let Some(&v) = memo.before.get(&key(x)) {
                known = v;
                chain.pop();
                break;
            }
            match step(x) {
                Some(p) => chain.push(p),
                None => {
                    // Nothing before the first one.
                    memo.before.insert(key(x), false);
                    chain.pop();
                    known = false;
                    break;
                }
            }
        }
        // Fill forward: x has a match before it if its step does, or has one before it.
        let mut value = known;
        for &x in chain.iter().rev() {
            let p = step(x).expect("each element in the chain has a step back");
            value = value || self.matches(doc, p, k, id, memo);
            memo.before.insert(key(x), value);
        }
        memo.before[&key(e)]
    }

    fn matches_here(&self, doc: &Doc, e: usize, k: usize, id: usize, memo: &mut Memo) -> bool {
        let (compound, combinator) = &self.parts[k];
        if !compound.matches(doc, e) {
            return false;
        }
        let Some(combinator) = combinator else {
            return true;
        };
        match combinator {
            Combinator::Child => doc
                .parent_element(e)
                .is_some_and(|p| self.matches(doc, p, k + 1, id, memo)),
            Combinator::Descendant => self.some_before(doc, e, k + 1, id, false, memo),
            Combinator::Adjacent => {
                previous_element(doc, e).is_some_and(|p| self.matches(doc, p, k + 1, id, memo))
            }
            Combinator::Sibling => self.some_before(doc, e, k + 1, id, true, memo),
        }
    }
}

fn previous_element(doc: &Doc, e: usize) -> Option<usize> {
    let mut at = doc.prev(e);
    while let Some(p) = at {
        if doc.is_element(p) {
            return Some(p);
        }
        at = doc.prev(p);
    }
    None
}

impl Compound {
    fn matches(&self, doc: &Doc, e: usize) -> bool {
        let Some((name, _, attrs)) = doc.element(e) else {
            return false;
        };
        let node = &doc.nodes[e];
        let in_element = doc.parent_element(e).is_some();
        let count = node.parent.map_or(1, |p| doc.nodes[p].elements);
        self.0.iter().all(|s| match s {
            Simple::Tag(t) => t == "*" || t.eq_ignore_ascii_case(name),
            Simple::Id(id) => doc.attr(e, "id") == Some(id.as_str()),
            Simple::Class(c) => doc.classes(e).iter().any(|x| x.eq_ignore_ascii_case(c)),
            Simple::Attr(key, test) => {
                let Some((_, v)) = attrs.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)) else {
                    return false;
                };
                let v = v.trim().to_lowercase();
                match test {
                    None => true,
                    Some((op, want)) => {
                        let want = want.to_lowercase();
                        match op {
                            '=' => v == want,
                            '^' => v.starts_with(&want),
                            '$' => v.ends_with(&want),
                            '*' => v.contains(&want),
                            _ => v.split(is_whitespace).any(|w| w == want),
                        }
                    }
                }
            }
            Simple::Root => {
                doc.nodes[e].parent == Some(0)
                    && doc.nodes[0].children.iter().find(|&&c| doc.is_element(c)) == Some(&e)
            }
            Simple::FirstChild => in_element && node.elem_pos == 0,
            Simple::LastChild => in_element && node.elem_pos + 1 == count,
            Simple::OnlyChild => in_element && count == 1,
            Simple::NthChild(n) => in_element && node.elem_pos + 1 == *n,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors_parse_and_match() {
        let doc = Doc::parse(
            br#"<html><body><div id="a" class="x y"><p>1</p><p class="y">2</p></div><span>3</span></body></html>"#,
        )
        .unwrap();
        let names = |s: &str| -> Vec<String> {
            doc.select(&Selector::parse(s).unwrap())
                .into_iter()
                .map(|e| {
                    let mut w = Writer {
                        doc: &doc,
                        html: None,
                        text: None,
                        spent: 0,
                        max: MAX_TEXT,
                        names: HashMap::new(),
                        groups: HashMap::new(),
                        counts: HashMap::new(),
                    };
                    w.local_name(e)
                })
                .collect()
        };
        assert_eq!(names(":root"), ["html"]);
        assert_eq!(
            names("div > p"),
            ["html/body/div.x.y/p:1", "html/body/div.x.y/p.y"]
        );
        assert_eq!(names(".y"), ["html/body/div.x.y", "html/body/div.x.y/p.y"]);
        assert_eq!(
            names("#a + span, p:nth-child(2)"),
            ["html/body/div.x.y/p.y", "html/body/span"]
        );
        assert_eq!(names("[class~=x]"), ["html/body/div.x.y"]);
        assert_eq!(names("body p:first-child"), ["html/body/div.x.y/p:1"]);
        assert!(Selector::parse("p:hover").is_err());
        assert!(Selector::parse("> p").is_err());
    }

    #[test]
    fn pretty_printing_as_jsoup() {
        let doc = Doc::parse(b"<html><head>\n<title>T</title></head><body><ul><li>a</li><li>b <b>c</b></li></ul><p>x &amp; y\xC2\xA0z</p></body></html>").unwrap();
        let body = doc.select(&Selector::parse("body").unwrap())[0];
        assert_eq!(
            inner_html(&doc, body),
            "<ul>\n <li>a</li>\n <li>b <b>c</b></li>\n</ul>\n<p>x &amp; y&nbsp;z</p>"
        );
        assert_eq!(inner_text(&doc, body), "a b c x & y z");
    }

    #[test]
    fn tags_with_too_many_attributes_are_refused() {
        let attrs: String = (0..10_001).map(|i| format!(" a{i}=\"v>\"")).collect();
        let e = Doc::parse(format!("<p{attrs}>x</p>").as_bytes())
            .err()
            .unwrap();
        assert!(e.to_string().contains("10000 attributes"), "{e}");
        let ok: String = (0..1000).map(|i| format!(" a{i} b{i}=v")).collect();
        Doc::parse(format!("<p{ok}>x</p>").as_bytes()).unwrap();
        // Wherever html5ever may read the tag: after comments that end early, inside raw text
        // element names that are not raw (SVG, MathML, frameset), in an end tag, and also in
        // a script or comment, where the scan cannot tell.
        let attrs: String = (0..20_000).map(|i| format!(" a{i}")).collect();
        let big = format!("<p{attrs}>x</p>");
        for page in [
            format!("<!-->{big}"),
            format!("<!--->{big}"),
            format!("<!-- x --!>{big}"),
            format!("<svg><title>{big}</title></svg>"),
            format!("<svg><style>{big}</style></svg>"),
            format!("<svg><script>{big}</script></svg>"),
            format!("<math><textarea>{big}</textarea></math>"),
            format!("<select><title>{big}</title></select>"),
            format!("<select><style>{big}</style></select>"),
            format!("<frameset><title>{big}</title></frameset>"),
            format!("<script>x</script >{big}"),
            format!("<TITLE>x</TITLE>{big}"),
            format!("<title><!--</title>{big}"),
            format!("<p>x</p{attrs}>"),
            format!("<script>{big}</script>"),
            format!("<!-- {big} -->"),
        ] {
            let started = std::time::Instant::now();
            let Err(e) = Doc::parse(page.as_bytes()) else {
                panic!("read: {}", &page[..40]);
            };
            assert!(e.to_string().contains("10000 attributes"), "{e}");
            assert!(
                started.elapsed().as_secs_f64() < 1.0,
                "{:?}",
                started.elapsed()
            );
        }
    }

    #[test]
    fn nesting_and_markup_are_bounded() {
        let deep = format!("{}x", "<div>".repeat(600));
        let e = Doc::parse(deep.as_bytes()).err().unwrap();
        assert!(e.to_string().contains("512"), "{e}");
        // 100 nested elements around 1 KB of text: each repeats it in its innerHTML.
        let doc =
            Doc::parse(format!("{}{}", "<div>".repeat(100), "y".repeat(1000)).as_bytes()).unwrap();
        let common = crate::tree::Common {
            namespace: String::new(),
            blank_nodes: true,
            root: "http://e/r".into(),
            null_string: None,
            trim: false,
        };
        let needs = crate::options::Needs {
            all: true,
            ..Default::default()
        };
        let mut sink = Sink::new(&common, &needs);
        let root = sink.root();
        let mut w = Writer {
            doc: &doc,
            html: sink.predicate(format!("{WHATWG}innerHTML")),
            text: sink.predicate(format!("{WHATWG}innerText")),
            spent: 0,
            max: 50_000,
            names: HashMap::new(),
            groups: HashMap::new(),
            counts: HashMap::new(),
        };
        let e = w.populate(&mut sink, 1, &root).err().unwrap();
        assert!(e.to_string().contains("more than 50000 bytes"), "{e}");
        w.max = 1_000_000;
        w.spent = 0;
        w.populate(&mut sink, 1, &root).unwrap();
    }

    /// Wide parents and long selectors on deep documents stay linear: a page of 50,000 list
    /// items, each with its own class, and a selector of 12 descendant steps on 500 nested
    /// elements.
    #[test]
    fn wide_and_deep_documents_stay_fast() {
        let items: String = (0..50_000)
            .map(|i| format!("<li class=\"a u{i}\">{i}</li>"))
            .collect();
        let doc = Doc::parse(format!("<ul>{items}</ul>").as_bytes()).unwrap();
        let start = std::time::Instant::now();
        let mut w = Writer {
            doc: &doc,
            html: None,
            text: None,
            spent: 0,
            max: MAX_TEXT,
            names: HashMap::new(),
            groups: HashMap::new(),
            counts: HashMap::new(),
        };
        let li = doc.select(&Selector::parse("li").unwrap());
        assert_eq!(li.len(), 50_000);
        assert_eq!(w.local_name(li[7]), "html/body/ul/li.a.u7");
        for &e in &li {
            w.local_name(e);
        }
        // `h2 ~ p` on 50,000 paragraphs, and `div p` on them.
        let wide =
            Doc::parse(format!("<h2>t</h2>{}", "<p>x</p>".repeat(50_000)).as_bytes()).unwrap();
        assert_eq!(
            wide.select(&Selector::parse("h2 ~ p").unwrap()).len(),
            50_000
        );
        assert_eq!(wide.select(&Selector::parse("h1 ~ p").unwrap()).len(), 0);
        assert_eq!(
            wide.select(&Selector::parse("body p").unwrap()).len(),
            50_000
        );
        let deep = Doc::parse("<div>".repeat(500).as_bytes()).unwrap();
        let selector = Selector::parse(&"div ".repeat(12)).unwrap();
        assert_eq!(deep.select(&selector).len(), 500 - 11);
        assert!(start.elapsed().as_secs() < 20, "{:?}", start.elapsed());
    }
}
