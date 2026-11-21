//! `html.metadata=true`: the RDF a page embeds, as SPARQL Anything extracts it with its copy
//! of Apache Any23. Its RDFa extractors (Semargl's RDFa 1.1 and RDFa 1.0 processors) and
//! its Microdata extractor run on the whole document, and their triples join the Facade-X
//! triples of the source.
//!
//! As in SPARQL Anything:
//! - every literal is an `xsd:string` (the lexical form only: no language tag, no datatype);
//! - the document IRI is the location's IRI, also for `about=""` and `itemid` (the `root`
//!   option does not change it); `<base href>` changes the base of RDFa only;
//! - the RDFa 1.1 and 1.0 processors both run, and their blank nodes share labels
//!   (`n0`, `n1`, ... in the order each creates them), so a blank node of one is the blank
//!   node of the other with the same number, as when SPARQL Anything gives Jena the labels;
//! - Microdata gives each item a new blank node unless it has an `itemid`, links the
//!   document to each top-level item with `http://www.w3.org/1999/xhtml/microdata#item`, and
//!   names an item's properties in the namespace of its first type (`https://schema.org/`
//!   for `https://schema.org/Movie`), else its parent's, else `http://schema.org/`.
//!
//! The document is always read as HTML (also with `html.parser=xml`) and is not fetched a
//! second time. The microformats2 h-card extractor SPARQL Anything also registers is not
//! run. Where Semargl stops on an error (an `about` that is not a valid CURIE on an element
//! without `rel`, `rev` or `property`), the rest of the document is still read.

use super::{Doc, Kind, Ns, Tag};
use crate::tree::Sink;
use oxigraph::model::{BlankNode, Literal, NamedNode, NamedOrBlankNode, Term};
use oxiri::Iri;
use regex::Regex;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::sync::LazyLock;

const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const RDFA: &str = "http://www.w3.org/ns/rdfa#";
const XHTML_VOCAB: &str = "http://www.w3.org/1999/xhtml/vocab#";
const XHTML_NS: &str = "http://www.w3.org/1999/xhtml";
const POWDER_DESCRIBED_BY: &str = "http://www.w3.org/2007/05/powder-s#describedby";
const XML_LITERAL: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#XMLLiteral";

/// Adds the triples the page embeds to `sink`.
pub(super) fn extract(doc: &Doc, document: &str, sink: &mut Sink<'_>) {
    let mut out = Out {
        sink,
        labels: HashMap::new(),
    };
    // As SPARQL Anything registers them: RDFa 1.1, RDFa 1.0, Microdata.
    for v11 in [true, false] {
        Rdfa::new(doc, document, v11, &mut out).run();
    }
    microdata(doc, document, &mut out);
}

/// A resource as the RDFa processor writes it: an IRI, or `_:label`.
type Res = String;

/// Where the triples go: IRIs that are not valid are left out, as are predicates the query
/// cannot match.
struct Out<'s, 'a> {
    sink: &'s mut Sink<'a>,
    /// Blank nodes by RDFa label, shared by both RDFa processors.
    labels: HashMap<String, BlankNode>,
}

enum Obj<'v> {
    Res(&'v str),
    Lit(&'v str),
}

impl Out<'_, '_> {
    fn resource(&mut self, r: &str) -> Option<NamedOrBlankNode> {
        match r.strip_prefix("_:") {
            Some(label) => Some(
                self.labels
                    .entry(label.to_owned())
                    .or_default()
                    .clone()
                    .into(),
            ),
            None => NamedNode::new(r).ok().map(Into::into),
        }
    }

    fn triple(&mut self, s: &str, p: &str, o: Obj<'_>) {
        let Some(subject) = self.resource(s) else {
            return;
        };
        let object: Term = match o {
            Obj::Res(o) => match self.resource(o) {
                Some(o) => o.into(),
                None => return,
            },
            Obj::Lit(v) => Literal::new_simple_literal(v).into(),
        };
        self.emit(&subject, p, object);
    }

    fn emit(&mut self, subject: &NamedOrBlankNode, p: &str, object: Term) {
        if NamedNode::new(p).is_err() {
            return;
        }
        if let Some(p) = self.sink.predicate(p.to_owned()) {
            self.sink.emit(subject, &p, object);
        }
    }
}

// ---------------------------------------------------------------------------------------
// RDFa, after Semargl's `RdfaParser` (Apache-2.0) as SPARQL Anything runs it: no vocabulary
// expansion, no processor graph, and the document read as HTML 4 (Any23 passes no doctype).

/// Semargl's whitespace.
fn is_space(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t'
            | '\r'
            | '\n'
            | '\u{c}'
            | '\u{b}'
            | '\u{1c}'
            | '\u{1d}'
            | '\u{1e}'
            | '\u{a0}'
            | '\u{2007}'
            | '\u{202f}'
    )
}

fn split(s: &str) -> impl Iterator<Item = &str> {
    s.split(is_space).filter(|t| !t.is_empty())
}

static ABS_OPAQUE_IRI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)^[a-zA-Z][a-zA-Z0-9+.-]*:[^#/][^#]*$").unwrap());
static ABS_HIER_IRI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?s)^[a-zA-Z][a-zA-Z0-9+.-]*:/{1,3}(([^/?#@]*)@)?(\[[^@/?#]+\]|([^@/?#:]+))(:([^/?#]*))?([^#?]*)?(\?([^#]*))?(#[^#]*)?$",
    )
    .unwrap()
});
static URN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)^urn:[a-zA-Z0-9][a-zA-Z0-9-]{1,31}:.+$").unwrap());
static TERM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9_-]+$").unwrap());

fn is_iri(s: &str) -> bool {
    ABS_HIER_IRI.is_match(s) || ABS_OPAQUE_IRI.is_match(s)
}

/// Semargl's `RIUtils.resolveIri`.
fn resolve_iri(base: &str, iri: &str) -> Result<String, Bad> {
    if is_iri(iri) || URN.is_match(iri) {
        return Ok(iri.to_owned());
    }
    if iri.is_empty() || iri.starts_with('?') {
        return Ok(format!("{}{iri}", base.strip_suffix('#').unwrap_or(base)));
    }
    let result = Iri::parse(base)
        .ok()
        .and_then(|b| b.resolve(iri).ok())
        .map(|r| r.into_inner())
        .unwrap_or_else(|| format!("{base}{iri}"));
    if is_iri(&result) {
        Ok(result)
    } else {
        Err(Bad::Iri)
    }
}

/// Why a value gave no IRI: a CURIE that does not resolve, or an IRI that is not valid.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Bad {
    Curie,
    Iri,
}

const INITIAL_CONTEXT: &[(&str, &str)] = &[
    ("owl", "http://www.w3.org/2002/07/owl#"),
    ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
    ("rdfa", "http://www.w3.org/ns/rdfa#"),
    ("xhv", "http://www.w3.org/1999/xhtml/vocab#"),
    ("xsd", "http://www.w3.org/2001/XMLSchema#"),
    ("grddl", "http://www.w3.org/2003/g/data-view#"),
    ("ma", "http://www.w3.org/ns/ma-ont#"),
    ("rif", "http://www.w3.org/2007/rif#"),
    ("skos", "http://www.w3.org/2004/02/skos/core#"),
    ("skosxl", "http://www.w3.org/2008/05/skos-xl#"),
    ("wdr", "http://www.w3.org/2007/05/powder#"),
    ("void", "http://rdfs.org/ns/void#"),
    ("wdrs", "http://www.w3.org/2007/05/powder-s#"),
    ("xml", "http://www.w3.org/XML/1998/namespace"),
    ("cc", "http://creativecommons.org/ns#"),
    ("ctag", "http://commontag.org/ns#"),
    ("dc", "http://purl.org/dc/terms/"),
    ("dcterms", "http://purl.org/dc/terms/"),
    ("foaf", "http://xmlns.com/foaf/0.1/"),
    ("gr", "http://purl.org/goodrelations/v1#"),
    ("ical", "http://www.w3.org/2002/12/cal/icaltzd#"),
    ("og", "http://ogp.me/ns#"),
    ("rev", "http://purl.org/stuff/rev#"),
    ("sioc", "http://rdfs.org/sioc/ns#"),
    ("v", "http://rdf.data-vocabulary.org/#"),
    ("vcard", "http://www.w3.org/2006/vcard/ns#"),
    ("schema", "http://schema.org/"),
];

fn initial(prefix: &str) -> Option<&'static str> {
    INITIAL_CONTEXT
        .iter()
        .find(|(p, _)| *p == prefix)
        .map(|(_, ns)| *ns)
}

const XHTML_TERMS: &[&str] = &[
    "alternate",
    "appendix",
    "bookmark",
    "cite",
    "chapter",
    "contents",
    "copyright",
    "first",
    "glossary",
    "help",
    "icon",
    "index",
    "itsRules",
    "last",
    "license",
    "meta",
    "next",
    "p3pv1",
    "prev",
    "previous",
    "role",
    "section",
    "stylesheet",
    "subsection",
    "start",
    "top",
    "up",
    "banner",
    "complementary",
    "contentinfo",
    "definition",
    "main",
    "navigation",
    "note",
    "search",
    "alert",
    "alertdialog",
    "application",
    "article",
    "button",
    "checkbox",
    "columnheader",
    "combobox",
    "dialog",
    "directory",
    "document",
    "form",
    "grid",
    "gridcell",
    "group",
    "heading",
    "img",
    "link",
    "list",
    "listbox",
    "listitem",
    "log",
    "marquee",
    "math",
    "menu",
    "menubar",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "option",
    "presentation",
    "progressbar",
    "radio",
    "radiogroup",
    "region",
    "row",
    "rowgroup",
    "rowheader",
    "scrollbar",
    "separator",
    "slider",
    "spinbutton",
    "status",
    "tab",
    "tablist",
    "tabpanel",
    "textbox",
    "timer",
    "toolbar",
    "tooltip",
    "tree",
    "treegrid",
    "treeitem",
];

/// What `objectLitDt` holds: how the text of the element is to be written.
#[derive(Clone, PartialEq)]
enum Dt {
    Plain,
    Typed,
    XmlLiteral,
}

/// A member of a list, or the object of `rdf:first`.
#[derive(Clone)]
enum Item {
    Res(Res),
    Lit(String),
}

type List = Rc<RefCell<Vec<Item>>>;
type ListMapping = Rc<RefCell<Vec<(String, List)>>>;

#[derive(Clone)]
enum Incomplete {
    Forward(String),
    Reverse(String),
    List(List),
}

type Mappings = Rc<RefCell<BTreeMap<String, String>>>;

/// Semargl's `EvalContext`.
struct Ctx {
    mappings: Mappings,
    subject: Option<Res>,
    object: Option<Res>,
    incomplete: Rc<RefCell<Vec<Incomplete>>>,
    object_lit: Option<String>,
    object_lit_dt: Option<Dt>,
    properties: Option<Vec<String>>,
    /// The properties were given with `inlist`.
    properties_inlist: bool,
    parsing_literal: bool,
    list_mapping: ListMapping,
    vocab: Option<String>,
}

impl Ctx {
    fn list_for(&self, iri: &str) -> List {
        let mut map = self.list_mapping.borrow_mut();
        if let Some((_, l)) = map.iter().find(|(k, _)| k == iri) {
            return l.clone();
        }
        let l = List::default();
        map.push((iri.to_owned(), l.clone()));
        l
    }
}

/// An element's attributes as the scanner passes them: `xmlns` declarations taken out, and
/// `vocab` trimmed and ended with `/` or `#`.
struct Attrs<'d>(Vec<(&'d str, String)>);

impl Attrs<'_> {
    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    fn has(&self, name: &str) -> bool {
        self.get(name).is_some()
    }
}

const HASH_VOCABS: &[&str] = &[
    "http://creativecommons.org/ns",
    "http://www.w3.org/2002/07/owl",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns",
    "http://www.w3.org/ns/rdfa",
    "http://www.w3.org/2000/01/rdf-schema",
    "http://www.w3.org/1999/xhtml/vocab",
    "http://www.w3.org/2001/XMLSchema",
    "http://microformats.org/profile/hcard",
    "http://www.w3.org/2006/vcard/ns",
    "http://ogp.me/ns",
    "http://ogp.me/ns/music",
    "http://ogp.me/ns/video",
    "http://ogp.me/ns/article",
    "http://ogp.me/ns/book",
    "http://ogp.me/ns/profile",
    "http://ogp.me/ns/website",
];

/// The keys of Semargl's `coalesce`: an attribute, or a rule.
#[derive(Clone, Copy, PartialEq)]
enum Key {
    About,
    Resource,
    Data,
    Href,
    Src,
    BaseIfHeadOrBody,
    BaseIfRootNode,
    ParentObject,
    BnodeIfTypeof,
}

struct Rdfa<'d, 'o, 's, 'a> {
    doc: &'d Doc,
    out: &'o mut Out<'s, 'a>,
    v11: bool,
    base: String,
    origin: String,
    stack: Vec<Ctx>,
    next_bnode: usize,
    bnodes: HashMap<String, Res>,
    overwrite: BTreeMap<String, String>,
    xml_string: Option<String>,
    xml_string_pred: Vec<String>,
    xml_string_subj: Option<Res>,
    /// Subjects typed `rdfa:Pattern`, with what they were given: a resource (`None` type) or
    /// a literal.
    patterns: HashMap<Res, Vec<(String, Item)>>,
    copying: Vec<(Res, Res)>,
    /// Prefixes `xmlns:` declares, innermost last (for attribute names).
    declared: Vec<Vec<String>>,
}

impl<'d, 'o, 's, 'a> Rdfa<'d, 'o, 's, 'a> {
    fn new(doc: &'d Doc, document: &str, v11: bool, out: &'o mut Out<'s, 'a>) -> Self {
        Rdfa {
            doc,
            out,
            v11,
            base: document.to_owned(),
            origin: document.to_owned(),
            stack: Vec::new(),
            next_bnode: 0,
            bnodes: HashMap::new(),
            overwrite: BTreeMap::new(),
            xml_string: None,
            xml_string_pred: Vec::new(),
            xml_string_subj: None,
            patterns: HashMap::new(),
            copying: Vec::new(),
            declared: Vec::new(),
        }
    }

    fn run(mut self) {
        let mut mappings = BTreeMap::new();
        mappings.insert(String::new(), XHTML_VOCAB.to_owned());
        self.stack.push(Ctx {
            mappings: Rc::new(RefCell::new(mappings)),
            subject: Some(self.base.clone()),
            object: None,
            incomplete: Default::default(),
            object_lit: None,
            object_lit_dt: None,
            properties: None,
            properties_inlist: false,
            parsing_literal: false,
            list_mapping: Default::default(),
            vocab: None,
        });
        for &c in &self.doc.nodes[0].children {
            self.node(c);
        }
        self.end_document();
    }

    fn node(&mut self, n: usize) {
        let doc = self.doc;
        match &doc.nodes[n].kind {
            Kind::Element { name, attrs, .. } => {
                let attrs = self.scan(attrs);
                self.start_element(name, &attrs);
                for &c in &doc.nodes[n].children {
                    self.node(c);
                }
                self.end_element(name);
                self.declared.pop();
            }
            Kind::Text(t) => self.characters(&jsoup_text(t)),
            _ => {}
        }
    }

    /// The scanner's part: `xmlns` declarations become prefix mappings, and attributes with
    /// a prefix no `xmlns:` declares are dropped.
    fn scan(&mut self, attrs: &'d [(String, String)]) -> Attrs<'d> {
        let mut declared = Vec::new();
        let mut rest = Vec::new();
        for (name, value) in attrs {
            if let Some(after) = name.strip_prefix("xmlns") {
                if after.is_empty() {
                    self.prefix_mapping("", value);
                    declared.push(String::new());
                    continue;
                } else if let Some(prefix) = after.strip_prefix(':') {
                    self.prefix_mapping(prefix, value);
                    declared.push(prefix.to_owned());
                    continue;
                }
            }
            rest.push((name.as_str(), value));
        }
        self.declared.push(declared);
        let mut out = Vec::new();
        for (name, value) in rest {
            let mut value = value.clone();
            let name = if name.eq_ignore_ascii_case("vocab") {
                value = value.trim().to_owned();
                if let Some(last) = value.chars().last()
                    && !matches!(last, '/' | '#' | ':')
                {
                    value.push(if HASH_VOCABS.contains(&value.as_str()) {
                        '#'
                    } else {
                        '/'
                    });
                }
                "vocab"
            } else {
                name
            };
            if let Some((prefix, _)) = name.split_once(':')
                && prefix != "xml"
                && !self.declared.iter().flatten().any(|p| p == prefix)
            {
                continue;
            }
            out.push((name, value));
        }
        Attrs(out)
    }

    fn prefix_mapping(&mut self, prefix: &str, uri: &str) {
        if prefix.is_empty() && uri.eq_ignore_ascii_case(XHTML_NS) {
            self.overwrite.insert(String::new(), XHTML_VOCAB.to_owned());
        } else if let Ok(iri) = resolve_iri(&self.origin, uri) {
            self.overwrite.insert(prefix.to_owned(), iri);
        }
    }

    fn bnode(&mut self, shortenable: bool) -> Res {
        let id = self.next_bnode;
        self.next_bnode += 1;
        if shortenable {
            format!("_:n{id}sbl")
        } else {
            format!("_:n{id}")
        }
    }

    fn top(&self) -> &Ctx {
        self.stack.last().expect("the initial context stays")
    }

    fn start_element(&mut self, q_name: &str, attrs: &Attrs<'_>) {
        if self.stack.len() < 4 {
            let old = self.base.clone();
            if q_name.eq_ignore_ascii_case("html")
                && let Some(version) = attrs.get("version")
                && version.to_lowercase().contains("rdfa 1.0")
            {
                self.v11 = false;
            }
            if q_name.eq_ignore_ascii_case("base")
                && let Some(href) = attrs.get("href")
            {
                self.base = href.split('#').next().unwrap_or_default().to_owned();
            }
            if self.base != old {
                for ctx in &mut self.stack {
                    if ctx.object.as_deref() == Some(old.as_str()) {
                        ctx.object = Some(self.base.clone());
                    }
                    if ctx.subject.as_deref() == Some(old.as_str()) {
                        ctx.subject = Some(self.base.clone());
                    }
                }
            }
        }
        if self.top().parsing_literal {
            let tag = self.open_tag(q_name, attrs);
            if let Some(x) = &mut self.xml_string {
                x.push_str(&tag);
            }
        }
        if self.v11
            && let Some(prefixes) = attrs.get("prefix")
        {
            let mut tokens = split(prefixes);
            while let Some(prefix) = tokens.next() {
                if prefix.chars().count() < 2 || !prefix.ends_with(':') {
                    continue;
                }
                let Some(uri) = tokens.next() else { break };
                self.prefix_mapping(&prefix[..prefix.len() - 1], uri);
            }
        }
        let mut current = self.child_context(attrs.get("vocab"));
        let skip_terms = self.v11 && attrs.has("property");
        let rels = rel_rev(attrs.get("rel"), skip_terms);
        let revs = rel_rev(attrs.get("rev"), skip_terms);
        let no_rels = rels.is_none() && revs.is_none();
        let skip = self.subject_and_object(q_name, attrs, no_rels, &mut current);
        {
            let parent = self.top();
            if self.v11
                && current.subject.is_some()
                && (current.subject != parent.object
                    || parent.subject.is_some() && parent.subject != parent.object)
            {
                current.list_mapping = Default::default();
            }
        }
        if let Some(rels) = &rels {
            self.rels(attrs, rels, &mut current);
        }
        if let Some(revs) = &revs {
            self.revs(revs, &mut current);
        }
        if current.object.is_none() && !no_rels {
            current.object = Some(self.bnode(false));
        }
        self.property(q_name, attrs, &mut current, no_rels);
        if self.v11
            && let Some(role) = attrs.get("role")
        {
            self.role(attrs.get("id"), role, &current);
        }
        if !skip {
            self.incomplete(&current);
        }
        self.push(current, skip);
    }

    fn child_context(&mut self, vocab: Option<&str>) -> Ctx {
        let overwrite = std::mem::take(&mut self.overwrite);
        let parent = self.top();
        let mappings = if overwrite.is_empty() {
            parent.mappings.clone()
        } else {
            let mut m = parent.mappings.borrow().clone();
            m.extend(overwrite);
            Rc::new(RefCell::new(m))
        };
        let mut current = Ctx {
            mappings,
            subject: None,
            object: None,
            incomplete: Default::default(),
            object_lit: None,
            object_lit_dt: None,
            properties: None,
            properties_inlist: false,
            parsing_literal: false,
            list_mapping: parent.list_mapping.clone(),
            vocab: parent.vocab.clone(),
        };
        if self.v11
            && let Some(vocab) = vocab
        {
            if vocab.is_empty() {
                current.vocab = None;
            } else {
                // Written straight to the output, as Semargl does.
                let base = self.base.clone();
                self.out
                    .triple(&base, &format!("{RDFA}usesVocabulary"), Obj::Res(vocab));
                current.vocab = Some(vocab.to_owned());
            }
        }
        current
    }

    fn resolve_bnode(&mut self, value: &str) -> Option<Res> {
        let name = if let Some(name) = value.strip_prefix("_:") {
            name
        } else if value.starts_with("[_:") && value.ends_with(']') {
            &value[3..value.len() - 1]
        } else {
            return None;
        };
        if let Some(b) = self.bnodes.get(name) {
            return Some(b.clone());
        }
        let b = self.bnode(false);
        self.bnodes.insert(name.to_owned(), b.clone());
        Some(b)
    }

    fn resolve_pred(&self, ctx: &Ctx, value: &str) -> Result<String, Bad> {
        if value.is_empty() {
            return Err(Bad::Iri);
        }
        if TERM.is_match(value) {
            if ctx.vocab.is_none() && self.v11 && value == "describedby" {
                return Ok(POWDER_DESCRIBED_BY.to_owned());
            }
            return match &ctx.vocab {
                Some(vocab) => {
                    let iri = format!("{vocab}{value}");
                    if ABS_HIER_IRI.is_match(&iri) {
                        Ok(iri)
                    } else {
                        Err(Bad::Iri)
                    }
                }
                None => XHTML_TERMS
                    .iter()
                    .find(|t| t.eq_ignore_ascii_case(value))
                    .map(|t| format!("{XHTML_VOCAB}{t}"))
                    .ok_or(Bad::Iri),
            };
        }
        self.resolve_curie_or_iri(ctx, value, true)
    }

    fn resolve_about(&mut self, ctx: &Ctx, value: &str) -> Result<String, Bad> {
        if let Some(b) = self.resolve_bnode(value) {
            return Ok(b);
        }
        self.resolve_curie_or_iri(ctx, value, false)
    }

    fn resolve_curie_or_iri(
        &self,
        ctx: &Ctx,
        curie: &str,
        ignore_rel: bool,
    ) -> Result<String, Bad> {
        if !ignore_rel && curie.is_empty() {
            return resolve_iri(&self.base, curie);
        }
        let safe = curie.len() >= 2 && curie.starts_with('[') && curie.ends_with(']');
        let curie = if safe {
            &curie[1..curie.len() - 1]
        } else {
            curie
        };
        let Some(delim) = curie.find(':') else {
            if safe || ignore_rel {
                return Err(Bad::Curie);
            }
            return resolve_iri(&self.base, curie);
        };
        let result = self.resolve_mapping(ctx, curie, delim, safe)?;
        if is_iri(&result) {
            Ok(result)
        } else {
            Err(Bad::Iri)
        }
    }

    fn resolve_mapping(
        &self,
        ctx: &Ctx,
        curie: &str,
        delim: usize,
        safe: bool,
    ) -> Result<String, Bad> {
        let local = &curie[delim + 1..];
        let prefix = &curie[..delim];
        if prefix == "_" {
            return Err(Bad::Curie);
        }
        let known = ctx.mappings.borrow().get(prefix).cloned();
        match known {
            Some(ns) => Ok(format!("{ns}{local}")),
            None => {
                if self.v11
                    && let Some(ns) = initial(prefix)
                {
                    ctx.mappings
                        .borrow_mut()
                        .insert(prefix.to_owned(), ns.to_owned());
                    let result = format!("{ns}{local}");
                    return if is_iri(&result) {
                        Ok(result)
                    } else {
                        Err(Bad::Curie)
                    };
                }
                if !safe && is_iri(curie) {
                    Ok(curie.to_owned())
                } else {
                    Err(Bad::Curie)
                }
            }
        }
    }

    /// Semargl's `coalesce`: the first key that gives a resource. `Err` when a key fails
    /// with an IRI that is not valid; a CURIE that does not resolve gives `Ok(None)`.
    fn coalesce(
        &mut self,
        q_name: &str,
        attrs: &Attrs<'_>,
        current: &Ctx,
        keys: &[Key],
    ) -> Result<Option<Res>, Bad> {
        for &key in keys {
            let attr = match key {
                Key::About => attrs.get("about"),
                Key::Resource => attrs.get("resource"),
                Key::Data => attrs.get("data"),
                Key::Href => attrs.get("href"),
                Key::Src => attrs.get("src"),
                Key::BnodeIfTypeof => attrs.get("typeof"),
                _ => None,
            };
            if let Some(value) = attr {
                match key {
                    Key::About | Key::Resource => {
                        if value == "[]" {
                            continue;
                        }
                        return match self.resolve_about(current, value) {
                            Ok(r) => Ok(Some(r)),
                            Err(Bad::Curie) => Ok(None),
                            Err(e) => Err(e),
                        };
                    }
                    Key::Href | Key::Src | Key::Data => {
                        return resolve_iri(&self.base, value).map(Some);
                    }
                    _ => return Ok(Some(self.bnode(false))),
                }
            } else if key == Key::ParentObject
                && let Some(object) = &self.top().object
            {
                return Ok(Some(object.clone()));
            } else {
                let head_or_body = q_name == "head" || q_name == "body";
                let root = self.stack.len() == 1 || attrs.has("typeof") && head_or_body;
                if head_or_body && key == Key::BaseIfHeadOrBody
                    || root && key == Key::BaseIfRootNode
                {
                    return Ok(Some(self.base.clone()));
                }
            }
        }
        Ok(None)
    }

    /// RDFa Core 1.1 steps 5 to 7 (1.0: steps 4 to 6). Whether to skip the element.
    fn subject_and_object(
        &mut self,
        q_name: &str,
        attrs: &Attrs<'_>,
        no_rels: bool,
        current: &mut Ctx,
    ) -> bool {
        use Key::*;
        let mut new_subject = None;
        let typeof_ = attrs.has("typeof");
        let found: Result<(), Bad> = (|| {
            if self.v11 {
                if no_rels {
                    if attrs.has("property")
                        && !attrs.has("content")
                        && !attrs.has("value")
                        && !attrs.has("datatype")
                    {
                        current.subject = self.coalesce(
                            q_name,
                            attrs,
                            current,
                            &[About, BaseIfRootNode, ParentObject],
                        )?;
                        if typeof_ {
                            current.object = self.coalesce(
                                q_name,
                                attrs,
                                current,
                                &[
                                    About,
                                    BaseIfRootNode,
                                    Resource,
                                    Data,
                                    Href,
                                    Src,
                                    BnodeIfTypeof,
                                ],
                            )?;
                            new_subject = current.object.clone();
                        }
                    } else {
                        current.subject = self.coalesce(
                            q_name,
                            attrs,
                            current,
                            &[
                                About,
                                Resource,
                                Data,
                                Href,
                                Src,
                                BaseIfRootNode,
                                BnodeIfTypeof,
                                ParentObject,
                            ],
                        )?;
                        if typeof_ {
                            new_subject = current.subject.clone();
                        }
                    }
                } else {
                    current.object =
                        self.coalesce(q_name, attrs, current, &[Resource, Data, Href, Src])?;
                    current.subject = self.coalesce(
                        q_name,
                        attrs,
                        current,
                        &[About, BaseIfRootNode, ParentObject],
                    )?;
                    if typeof_ {
                        if attrs.has("about") {
                            new_subject = current.subject.clone();
                        } else {
                            if current.object.is_none() {
                                current.object = Some(self.bnode(no_rels));
                            }
                            new_subject = current.object.clone();
                        }
                    }
                }
            } else {
                if no_rels {
                    current.subject = self.coalesce(
                        q_name,
                        attrs,
                        current,
                        &[
                            About,
                            Src,
                            Resource,
                            Href,
                            BaseIfHeadOrBody,
                            BnodeIfTypeof,
                            ParentObject,
                        ],
                    )?;
                } else {
                    current.subject = self.coalesce(
                        q_name,
                        attrs,
                        current,
                        &[About, Src, BaseIfHeadOrBody, BnodeIfTypeof, ParentObject],
                    )?;
                    current.object = self.coalesce(q_name, attrs, current, &[Resource, Href])?;
                }
                if typeof_ {
                    new_subject = current.subject.clone();
                }
            }
            Ok(())
        })();
        if found.is_err() && current.subject.is_none() {
            current.subject = self.top().subject.clone();
        }
        if let Some(subject) = new_subject {
            for t in split(attrs.get("typeof").unwrap_or_default()) {
                if let Ok(iri) = self.resolve_pred(current, t) {
                    self.add_res(&subject, &format!("{RDF}type"), &iri);
                }
            }
        }
        let parent_object = &self.top().object;
        no_rels
            && !attrs.has("property")
            && (current.subject.is_none() && parent_object.is_none()
                || current.subject.is_some() && current.subject == *parent_object)
    }

    fn rels(&mut self, attrs: &Attrs<'_>, rels: &[&str], current: &mut Ctx) {
        let in_list = self.v11 && attrs.has("inlist");
        for pred in rels {
            let Ok(iri) = self.resolve_pred(current, pred) else {
                continue;
            };
            if in_list {
                let list = current.list_for(&iri);
                match &current.object {
                    Some(o) => list.borrow_mut().push(Item::Res(o.clone())),
                    None => current.incomplete.borrow_mut().push(Incomplete::List(list)),
                }
            } else {
                match (&current.subject, &current.object) {
                    (Some(s), Some(o)) => {
                        let (s, o) = (s.clone(), o.clone());
                        self.add_res(&s, &iri, &o);
                    }
                    (_, Some(_)) => {}
                    (_, None) => current
                        .incomplete
                        .borrow_mut()
                        .push(Incomplete::Forward(iri)),
                }
            }
        }
    }

    fn revs(&mut self, revs: &[&str], current: &mut Ctx) {
        for pred in revs {
            let Ok(iri) = self.resolve_pred(current, pred) else {
                continue;
            };
            match (&current.object, &current.subject) {
                (Some(o), Some(s)) => {
                    let (s, o) = (s.clone(), o.clone());
                    self.add_res(&o, &iri, &s);
                }
                (Some(_), None) => {}
                (None, _) => current
                    .incomplete
                    .borrow_mut()
                    .push(Incomplete::Reverse(iri)),
            }
        }
    }

    /// RDFa Core 1.1 step 11 (1.0: step 9).
    fn property(&mut self, q_name: &str, attrs: &Attrs<'_>, current: &mut Ctx, no_rels: bool) {
        let Some(property) = attrs.get("property") else {
            current.parsing_literal = false;
            return;
        };
        let content = attrs.get("content");
        let mut datatype = attrs.get("datatype").map(str::to_owned);
        if let Some(d) = &datatype
            && !d.is_empty()
        {
            datatype = self.resolve_pred(current, d).ok();
        }
        // The value: `content` for a literal, else the resource.
        let mut resource = None;
        if let Some(d) = datatype.as_deref().filter(|d| *d != XML_LITERAL) {
            let _ = d;
            if content.is_none() {
                current.object_lit_dt = Some(Dt::Typed);
            }
        } else if content.is_some() {
        } else if datatype.is_none() && self.v11 {
            if !attrs.has("value") && no_rels {
                resource = self
                    .coalesce(
                        q_name,
                        attrs,
                        current,
                        &[Key::Resource, Key::Data, Key::Href, Key::Src],
                    )
                    .ok()
                    .flatten();
            }
            if resource.is_none() {
                if !attrs.has("about") && attrs.has("typeof") {
                    resource = current.object.clone();
                    if resource.is_none() {
                        current.object_lit_dt = Some(Dt::Plain);
                    }
                } else {
                    current.object_lit_dt = Some(Dt::Plain);
                }
            }
        } else {
            current.object_lit_dt = Some(Dt::XmlLiteral);
        }
        let in_list = attrs.has("inlist");
        for p in split(property) {
            let Ok(iri) = self.resolve_pred(current, p) else {
                continue;
            };
            if content.is_some() || resource.is_some() {
                if self.v11 && in_list {
                    let item = match content {
                        Some(c) => Item::Lit(c.to_owned()),
                        None => Item::Res(resource.clone().unwrap_or_default()),
                    };
                    current.list_for(&iri).borrow_mut().push(item);
                } else if let Some(s) = current.subject.clone() {
                    match content {
                        Some(c) => self.add_lit(&s, &iri, c),
                        None => {
                            let o = resource.clone().unwrap_or_default();
                            self.add_res(&s, &iri, &o);
                        }
                    }
                }
            } else if let Some(props) = &mut current.properties {
                props.push(iri);
            } else {
                current.properties = Some(vec![iri]);
                current.properties_inlist = self.v11 && in_list;
            }
        }
        current.parsing_literal = current.object_lit_dt == Some(Dt::XmlLiteral);
        if current.properties.is_none() {
            current.object_lit_dt = None;
            current.parsing_literal = false;
        }
    }

    fn role(&mut self, id: Option<&str>, role: &str, current: &Ctx) {
        let subject = match id {
            Some(id) => format!("{}#{id}", self.base),
            None => self.bnode(true),
        };
        for r in split(role) {
            let iri = if TERM.is_match(r) {
                Ok(format!("{XHTML_VOCAB}{r}"))
            } else {
                self.resolve_curie_or_iri(current, r, true)
            };
            if let Ok(iri) = iri {
                self.add_res(&subject, &format!("{XHTML_VOCAB}role"), &iri);
            }
        }
    }

    /// RDFa Core 1.1 step 12 (1.0: step 10).
    fn incomplete(&mut self, current: &Ctx) {
        let Some(new) = current.subject.clone() else {
            return;
        };
        let parent = self.top();
        let subject = parent.subject.clone().unwrap_or_default();
        let incomplete = parent.incomplete.borrow().clone();
        for i in incomplete {
            match i {
                Incomplete::Reverse(p) => self.add_res(&new, &p, &subject),
                Incomplete::Forward(p) => self.add_res(&subject, &p, &new),
                Incomplete::List(l) => l.borrow_mut().push(Item::Res(new.clone())),
            }
        }
    }

    fn push(&mut self, mut current: Ctx, skip: bool) {
        if current.parsing_literal {
            self.xml_string = Some(String::new());
            self.xml_string_pred = current.properties.clone().unwrap_or_default();
            self.xml_string_subj = current
                .subject
                .clone()
                .or_else(|| self.top().subject.clone());
        }
        let parent = self.top();
        if current.parsing_literal || skip {
            current.subject = parent.subject.clone();
            current.object = parent.object.clone();
            current.incomplete = parent.incomplete.clone();
            current.object_lit_dt = parent.object_lit_dt.clone();
            current.object_lit = current.object_lit_dt.as_ref().map(|_| String::new());
            current.properties = None;
        } else {
            if current.subject.is_none() {
                current.subject = parent.subject.clone();
            }
            if current.object.is_none() {
                current.object = current.subject.clone();
            }
            if current.object_lit_dt.is_some() || parent.object_lit_dt.is_some() {
                current.object_lit = Some(String::new());
            }
        }
        self.stack.push(current);
    }

    fn characters(&mut self, text: &str) {
        if let Some(x) = &mut self.xml_string {
            x.push_str(text);
        }
        if let Some(top) = self.stack.last_mut()
            && let Some(lit) = &mut top.object_lit
        {
            lit.push_str(text);
        }
    }

    fn end_element(&mut self, q_name: &str) {
        let current = self.stack.pop().expect("pushed at the start");
        if current.parsing_literal
            && let Some(content) = self.xml_string.take()
        {
            let preds = std::mem::take(&mut self.xml_string_pred);
            if let Some(s) = self.xml_string_subj.clone() {
                for p in preds {
                    self.add_lit(&s, &p, &content);
                }
            }
        }
        if let Some(x) = &mut self.xml_string {
            x.push_str("</");
            x.push_str(q_name);
            x.push('>');
        }
        if self.stack.is_empty() {
            return;
        }
        // The text of the element.
        if let Some(content) = &current.object_lit {
            {
                let parent = self.stack.last_mut().expect("not empty");
                if !parent.parsing_literal
                    && let Some(lit) = &mut parent.object_lit
                {
                    lit.push_str(content);
                }
            }
            if let Some(props) = &current.properties {
                if current.properties_inlist {
                    for p in props {
                        current
                            .list_for(p)
                            .borrow_mut()
                            .push(Item::Lit(content.clone()));
                    }
                } else if let Some(s) = current.subject.clone() {
                    for p in props {
                        self.add_lit(&s, p, content);
                    }
                }
            }
        }
        if !Rc::ptr_eq(&self.top().list_mapping, &current.list_mapping) {
            self.lists(&current);
        }
    }

    /// RDFa Core 1.1 step 14: the lists the element began.
    fn lists(&mut self, current: &Ctx) {
        let lists = std::mem::take(&mut *current.list_mapping.borrow_mut());
        let Some(subject) = current.subject.clone() else {
            return;
        };
        for (pred, list) in lists {
            let mut prev: Option<Res> = None;
            let mut start = None;
            let items = list.borrow().clone();
            for item in items {
                let child = self.bnode(false);
                match item {
                    Item::Lit(c) => self.add_lit(&child, &format!("{RDF}first"), &c),
                    Item::Res(r) => self.add_res(&child, &format!("{RDF}first"), &r),
                }
                match &prev {
                    None => start = Some(child.clone()),
                    Some(p) => {
                        let p = p.clone();
                        self.add_res(&p, &format!("{RDF}rest"), &child);
                    }
                }
                prev = Some(child);
            }
            match (start, prev) {
                (Some(start), Some(last)) => {
                    self.add_res(&last, &format!("{RDF}rest"), &format!("{RDF}nil"));
                    self.add_res(&subject, &pred, &start);
                }
                _ => self.add_res(&subject, &pred, &format!("{RDF}nil")),
            }
        }
    }

    /// The open tag of an element inside an XML literal, as Semargl writes it: the
    /// attributes, then every prefix mapping in scope.
    fn open_tag(&self, q_name: &str, attrs: &Attrs<'_>) -> String {
        let mut tag = format!("<{q_name}");
        for (k, v) in &attrs.0 {
            tag.push_str(&format!(" {k}=\"{v}\""));
        }
        for (k, v) in self.top().mappings.borrow().iter() {
            if k.is_empty() {
                tag.push_str(&format!(" xmlns=\"{v}\""));
            } else {
                tag.push_str(&format!(" xmlns:{k}=\"{v}\""));
            }
        }
        tag.push('>');
        tag
    }

    fn end_document(&mut self) {
        let copying = std::mem::take(&mut self.copying);
        for (subject, pattern) in &copying {
            if let Some(props) = self.patterns.get(pattern).cloned() {
                self.copy(subject, &props);
            }
        }
        for (_, pattern) in &copying {
            self.patterns.remove(pattern);
        }
        let patterns = std::mem::take(&mut self.patterns);
        for (pattern, props) in patterns {
            self.out.triple(
                &pattern,
                &format!("{RDF}type"),
                Obj::Res(&format!("{RDFA}Pattern")),
            );
            self.copy(&pattern, &props);
        }
    }

    fn copy(&mut self, subject: &str, props: &[(String, Item)]) {
        for (p, item) in props {
            match item {
                Item::Res(o) => self.out.triple(subject, p, Obj::Res(o)),
                Item::Lit(v) => self.out.triple(subject, p, Obj::Lit(v)),
            }
        }
    }

    fn add_res(&mut self, s: &str, p: &str, o: &str) {
        if o == format!("{RDFA}Pattern") {
            self.patterns.entry(s.to_owned()).or_default();
            return;
        }
        if p == format!("{RDFA}copy") {
            match self.patterns.get(o).cloned() {
                Some(props) => self.copy(s, &props),
                None => self.copying.push((s.to_owned(), o.to_owned())),
            }
            return;
        }
        if let Some(props) = self.patterns.get_mut(s) {
            props.push((p.to_owned(), Item::Res(o.to_owned())));
            return;
        }
        self.out.triple(s, p, Obj::Res(o));
    }

    fn add_lit(&mut self, s: &str, p: &str, v: &str) {
        if let Some(props) = self.patterns.get_mut(s) {
            props.push((p.to_owned(), Item::Lit(v.to_owned())));
            return;
        }
        self.out.triple(s, p, Obj::Lit(v));
    }
}

/// `rel` or `rev` as a list: none when the attribute is missing, and with `property` on the
/// element (`skip_terms`), only its CURIEs and IRIs, none when that leaves nothing.
fn rel_rev(value: Option<&str>, skip_terms: bool) -> Option<Vec<&str>> {
    let value = value?;
    let list: Vec<&str> = split(value)
        .filter(|p| !skip_terms || p.contains(':'))
        .collect();
    if skip_terms && list.is_empty() {
        None
    } else {
        Some(list)
    }
}

/// Jsoup's `TextNode.text()`: runs of whitespace (and the no-break space) as one space.
fn jsoup_text(t: &str) -> String {
    let mut out = String::with_capacity(t.len());
    let mut space = false;
    for c in t.chars() {
        if matches!(c, ' ' | '\t' | '\n' | '\u{c}' | '\r' | '\u{a0}') {
            if !space {
                out.push(' ');
                space = true;
            }
        } else {
            out.push(c);
            space = false;
        }
    }
    out
}

// ---------------------------------------------------------------------------------------
// Microdata, after Any23's `MicrodataParser` and `MicrodataExtractor` (Apache-2.0), with its
// defaults: not strict, and `http://schema.org/` for items without a type.

const MICRODATA_ITEM: &str = "http://www.w3.org/1999/xhtml/microdata#item";
const DEFAULT_NS: &str = "http://schema.org/";
const HCARD: &str = "http://microformats.org/profile/hcard";

const SRC_TAGS: &[&str] = &[
    "audio", "embed", "frame", "iframe", "img", "source", "track", "video", "input", "layer",
    "script", "textarea",
];
const HREF_TAGS: &[&str] = &["a", "area", "link"];

/// An item's property value.
enum Value {
    Item(usize),
    Link(String),
    Lit(String),
}

struct Microdata<'d, 'o, 's, 'a> {
    doc: &'d Doc,
    out: &'o mut Out<'s, 'a>,
    document: Option<Iri<String>>,
    document_text: String,
    /// The subject of each item, by element.
    subjects: HashMap<usize, NamedOrBlankNode>,
    /// Items being written, against loops.
    open: Vec<usize>,
    /// The first element with each `id`, for `itemref`.
    ids: HashMap<&'d str, usize>,
}

fn microdata(doc: &Doc, document: &str, out: &mut Out<'_, '_>) {
    let elements: Vec<usize> = (0..doc.nodes.len())
        .filter(|&i| doc.is_element(i))
        .collect();
    let scopes: Vec<usize> = elements
        .iter()
        .copied()
        .filter(|&e| doc.attr_exact(e, "itemscope").is_some())
        .collect();
    if scopes.is_empty() {
        return;
    }
    let is_prop = |e: usize| {
        doc.attr_exact(e, "itemprop").is_some() || doc.attr_exact(e, "itemprop-reverse").is_some()
    };
    let mut top = Vec::new();
    let mut possibles = Vec::new();
    for &s in &scopes {
        if !is_prop(s) {
            top.push(s);
        } else if !ancestors(doc, s).any(|a| doc.attr_exact(a, "itemscope").is_some()) {
            possibles.push(s);
        }
    }
    if !possibles.is_empty() {
        let refs: HashSet<&str> = scopes
            .iter()
            .flat_map(|&s| split_ws(doc.attr_exact(s, "itemref").unwrap_or_default()))
            .collect();
        for s in possibles {
            let in_ref = std::iter::once(s)
                .chain(ancestors(doc, s))
                .any(|a| doc.attr_exact(a, "id").is_some_and(|id| refs.contains(&id)));
            if !in_ref {
                top.push(s);
            }
        }
    }
    let mut ids = HashMap::new();
    for &e in &elements {
        if let Some(id) = doc.attr_exact(e, "id") {
            ids.entry(id).or_insert(e);
        }
    }
    let mut m = Microdata {
        ids,
        doc,
        out,
        document: Iri::parse(document.to_owned()).ok(),
        document_text: document.to_owned(),
        subjects: HashMap::new(),
        open: Vec::new(),
    };
    for item in top {
        let Some(subject) = m.item(item, DEFAULT_NS.to_owned()) else {
            continue;
        };
        if let Ok(d) = NamedNode::new(m.document_text.clone()) {
            m.out.emit(&d.into(), MICRODATA_ITEM, subject.into());
        }
    }
}

fn split_ws(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| c.is_whitespace())
        .filter(|t| !t.is_empty())
}

fn ancestors(doc: &Doc, e: usize) -> impl Iterator<Item = usize> + '_ {
    std::iter::successors(doc.nodes[e].parent, |&p| doc.nodes[p].parent)
}

/// The namespace of an IRI as RDF4J splits it: up to the last `#`, else `/`, else `:`.
fn namespace(iri: &str) -> &str {
    let i = iri
        .rfind('#')
        .or_else(|| iri.rfind('/'))
        .or_else(|| iri.rfind(':'))
        .map_or(0, |i| i + 1);
    &iri[..i]
}

static STARTS_WITH_HOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)^[^:/.]+(\.[^:/.]+)+(:\d+)?([/#?].*)?$").unwrap());

/// An `itemtype` value as an IRI: `http:` added to one that starts with a host.
fn item_type(t: &str) -> Option<NamedNode> {
    let t = t.trim();
    let has_scheme = t.split_once(':').is_some_and(|(s, _)| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    });
    let iri = if has_scheme {
        t.to_owned()
    } else if t.starts_with("//") {
        format!("http:{t}")
    } else if STARTS_WITH_HOST.is_match(t) {
        format!("http://{t}")
    } else {
        t.to_owned()
    };
    NamedNode::new(iri).ok()
}

impl Microdata<'_, '_, '_, '_> {
    fn resolve(&self, part: &str) -> Option<NamedNode> {
        let iri = self.document.as_ref()?.resolve(part.trim()).ok()?;
        NamedNode::new(iri.into_inner()).ok()
    }

    /// Writes the item at `e`, and gives its subject.
    fn item(&mut self, e: usize, parent_ns: String) -> Option<NamedOrBlankNode> {
        if let Some(s) = self.subjects.get(&e) {
            return Some(s.clone());
        }
        if self.open.contains(&e) {
            return None;
        }
        let doc = self.doc;
        let subject: NamedOrBlankNode = match doc.attr_exact(e, "itemid") {
            Some(id) => self
                .resolve(id)
                .map_or_else(|| BlankNode::default().into(), Into::into),
            None => BlankNode::default().into(),
        };
        self.subjects.insert(e, subject.clone());
        self.open.push(e);
        let types: Vec<NamedNode> = doc
            .attr_exact(e, "itemtype")
            .map(|t| split_ws(t).filter_map(item_type).collect())
            .unwrap_or_default();
        let ns = match types.first() {
            Some(t) if t.as_str().starts_with(HCARD) => format!("{HCARD}#"),
            Some(t) => namespace(t.as_str()).to_owned(),
            None => parent_ns,
        };
        for t in &types {
            self.out
                .emit(&subject, &format!("{RDF}type"), t.clone().into());
        }
        let mut props = self.props(e, true);
        let mut seen = HashSet::new();
        let mut known: HashSet<(usize, String, bool)> = props.iter().cloned().collect();
        for r in split_ws(doc.attr_exact(e, "itemref").unwrap_or_default()) {
            if !seen.insert(r) {
                continue;
            }
            if let Some(&target) = self.ids.get(r) {
                for p in self.props(target, false) {
                    if known.insert(p.clone()) {
                        props.push(p);
                    }
                }
            }
        }
        for (node, name, reverse) in props {
            let predicate = if NamedNode::new(name.trim()).is_ok() && name.trim().contains(':') {
                name.trim().to_owned()
            } else {
                format!("{}{}", namespace(&ns), name.trim())
            };
            let value = self.value(node);
            let object: Term = match value {
                Value::Lit(v) => {
                    if reverse {
                        continue;
                    }
                    Literal::new_simple_literal(v).into()
                }
                Value::Item(i) => match self.item(i, ns.clone()) {
                    Some(o) => o.into(),
                    None => continue,
                },
                Value::Link(l) => match self.resolve(&l) {
                    Some(o) => {
                        if predicate == "http://schema.org/additionalType" {
                            if reverse {
                                self.out.emit(
                                    &o.clone().into(),
                                    &format!("{RDF}type"),
                                    subject.clone().into(),
                                );
                            } else {
                                self.out
                                    .emit(&subject, &format!("{RDF}type"), o.clone().into());
                            }
                        }
                        o.into()
                    }
                    None => continue,
                },
            };
            if reverse {
                let o = match object {
                    Term::NamedNode(n) => NamedOrBlankNode::from(n),
                    Term::BlankNode(b) => b.into(),
                    _ => continue,
                };
                self.out.emit(&o, &predicate, subject.clone().into());
            } else {
                self.out.emit(&subject, &predicate, object);
            }
        }
        self.open.pop();
        Some(subject)
    }

    /// The properties of the item at `scope` (`skip_root`), or that `scope` gives when an
    /// `itemref` names it: (element, name, reverse), in document order.
    fn props(&self, scope: usize, skip_root: bool) -> Vec<(usize, String, bool)> {
        let doc = self.doc;
        let mut accepted = Vec::new();
        let is_prop = |e: usize| {
            doc.attr_exact(e, "itemprop").is_some()
                || doc.attr_exact(e, "itemprop-reverse").is_some()
        };
        let mut skip_children = false;
        if !skip_root {
            if is_prop(scope) {
                accepted.push(scope);
            }
            if doc.attr_exact(scope, "itemscope").is_some() {
                skip_children = true;
            }
        }
        if !skip_children {
            let mut stack: Vec<usize> = doc.nodes[scope].children.iter().rev().copied().collect();
            while let Some(n) = stack.pop() {
                if !doc.is_element(n) {
                    continue;
                }
                if is_prop(n) {
                    accepted.push(n);
                }
                if doc.attr_exact(n, "itemscope").is_some() {
                    continue;
                }
                stack.extend(doc.nodes[n].children.iter().rev().copied());
            }
        }
        let mut out = Vec::new();
        for e in accepted {
            let names = doc.attr_exact(e, "itemprop").unwrap_or_default();
            let reverse = doc.attr_exact(e, "itemprop-reverse").unwrap_or_default();
            for n in split_ws(names) {
                out.push((e, n.to_owned(), false));
            }
            for n in split_ws(reverse) {
                out.push((e, n.to_owned(), true));
            }
        }
        out
    }

    fn value(&self, e: usize) -> Value {
        let doc = self.doc;
        if doc.attr_exact(e, "itemscope").is_some() {
            return Value::Item(e);
        }
        let Some((name, _, _)) = doc.element(e) else {
            return Value::Lit(String::new());
        };
        let name = name.to_ascii_lowercase();
        let content = |attr: &str| {
            doc.attr_exact(e, "content")
                .or_else(|| doc.attr_exact(e, attr))
                .map(str::to_owned)
        };
        match name.as_str() {
            "data" | "meter" => Value::Lit(
                content("value")
                    .unwrap_or_else(|| text_content(doc, e))
                    .trim()
                    .to_owned(),
            ),
            "time" => Value::Lit(
                content("datetime")
                    .unwrap_or_else(|| text_content(doc, e))
                    .trim()
                    .to_owned(),
            ),
            n if SRC_TAGS.contains(&n) => link(content("src")),
            n if HREF_TAGS.contains(&n) => link(content("href")),
            "object" => link(content("data")),
            _ => match doc.attr_exact(e, "content") {
                Some(c) => Value::Lit(c.to_owned()),
                None => Value::Lit(formatted(doc, e)),
            },
        }
    }
}

fn link(value: Option<String>) -> Value {
    match value {
        Some(v) => Value::Link(v),
        None => Value::Lit(String::new()),
    }
}

/// All the text under `e`, as DOM's `textContent`.
fn text_content(doc: &Doc, e: usize) -> String {
    let mut out = String::new();
    let mut stack = vec![e];
    while let Some(n) = stack.pop() {
        match &doc.nodes[n].kind {
            Kind::Text(t) | Kind::Data(t) => out.push_str(t),
            _ => stack.extend(doc.nodes[n].children.iter().rev()),
        }
    }
    out
}

/// The text under `e` with a line break between blocks, as Any23 writes a property's text.
fn formatted(doc: &Doc, e: usize) -> String {
    let mut out = String::new();
    append_formatted(doc, e, &mut out, false);
    out
}

fn append_formatted(doc: &Doc, n: usize, out: &mut String, needs_newline: bool) -> bool {
    match &doc.nodes[n].kind {
        Kind::Text(t) | Kind::Data(t) => {
            if t.is_empty() {
                return needs_newline;
            }
            if needs_newline && separate_with_newline(out, t) {
                out.push('\n');
            }
            out.push_str(t);
            false
        }
        Kind::Element { name, ns, .. } => {
            let lower = name.to_ascii_lowercase();
            // Jsoup's tag of that name, whatever the namespace.
            let _ = ns;
            let this = lower == "br" || Tag::of(&lower, Ns::Html).block;
            let mut prev = needs_newline || this;
            for &c in &doc.nodes[n].children {
                prev = append_formatted(doc, c, out, prev);
            }
            prev || this
        }
        _ => needs_newline,
    }
}

fn separate_with_newline(s0: &str, s1: &str) -> bool {
    for c in s1.chars() {
        if c == '\n' || c == '\r' {
            return false;
        }
        if !c.is_whitespace() {
            break;
        }
    }
    for c in s0.chars().rev() {
        if c == '\n' || c == '\r' {
            return false;
        }
        if !c.is_whitespace() {
            return true;
        }
    }
    false
}
