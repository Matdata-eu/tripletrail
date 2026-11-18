//! Describe pages (#140): a readable page for any resource of a dataset, from the shapes the
//! describe reader reads (#137): the dataset's own, over the shapes generated from its profile.
//!
//! [`build`] makes the view model of a resource: its label, its classes, its values in rows,
//! the rows in groups, and the links that point to it. [`html`] draws it as a page, and
//! [`to_json`] gives it to a browser that draws it itself.
//!
//! - The node shapes of the resource's classes and their superclasses apply (`sh:targetClass`,
//!   `dash:applicableToClass`, or a class that is itself a node shape): the dataset's own
//!   first, then those generated from its profile, each by class (the resource's own classes
//!   first); their property shapes give the rows, by `tts:key` (or path) once.
//! - A row has the name (`sh:name`), the group (`sh:group`, its `rdfs:label` and `sh:order`)
//!   and the order (`sh:order`) of its property shape, and a viewer (`dash:viewer`: label,
//!   literal, URI, details, value table, image or HTML viewer). `dash:hidden` and
//!   `sh:deactivated` shapes give no row. Any SHACL path works (sequence, alternative,
//!   inverse, zero-or-more, one-or-more, zero-or-one), read within a budget of triples.
//! - The resource's other properties are rows too: attributes when every value is a literal,
//!   links otherwise, grouped by namespace and sorted by property.
//! - Values are links with labels, literals with their datatype or language, or, for a blank
//!   node, its own rows.
//! - Links to the resource that no row shows are listed per property, with a sample of the
//!   resources they come from.

use crate::HttpError;
use serde_json::{Value as Json, json};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use tripletrail::oxigraph::model::vocab::{rdf, rdfs, xsd};
use tripletrail::oxigraph::model::{
    Graph, Literal, NamedNode, NamedNodeRef, NamedOrBlankNode, NamedOrBlankNodeRef, Quad, Term,
    TermRef,
};
use tripletrail::{Engine, ReadableGraphs};

const SH: &str = "http://www.w3.org/ns/shacl#";
const DASH: &str = "http://datashapes.org/dash#";
const TTS: &str = tripletrail::shapes::TTS;
const RD: &str = "https://rinf-describe.matdata.eu/ns#";

/// Values shown per row; the others are counted.
const MAX_VALUES: usize = 100;
/// Resources shown per incoming property.
const MAX_INCOMING: usize = 20;
/// Links to the resource read at most (a class IRI can have millions).
const MAX_INCOMING_SCAN: usize = 10_000;
/// Triples of the resource read at most.
const MAX_OUTGOING_SCAN: usize = 100_000;
/// Geometries on the map at most.
const MAX_GEOMETRIES: usize = 20;
/// Points drawn on the map, about: a geometry of more points is thinned.
const MAX_MAP_POINTS: usize = 50_000;
const GEO: &str = "http://www.opengis.net/ont/geosparql#";
/// Superclasses followed at most.
const MAX_CLASSES: usize = 64;
/// Rows of a blank node, and how deep blank nodes are shown.
const MAX_NESTED_ROWS: usize = 50;
const MAX_DEPTH: usize = 3;

/// The label properties tried, after the shapes' `tts:labelPath`.
const LABELS: [&str; 7] = [
    "http://www.w3.org/2000/01/rdf-schema#label",
    "http://www.w3.org/2004/02/skos/core#prefLabel",
    "http://purl.org/dc/terms/title",
    "http://purl.org/dc/elements/1.1/title",
    "http://schema.org/name",
    "https://schema.org/name",
    "http://xmlns.com/foaf/0.1/name",
];

/// The view model of a resource.
pub(crate) struct View {
    pub(crate) iri: NamedNode,
    pub(crate) label: Option<String>,
    pub(crate) classes: Vec<Link>,
    /// The key figures, shown under the title.
    pub(crate) badges: Vec<Row>,
    pub(crate) groups: Vec<Group>,
    pub(crate) incoming: Vec<Incoming>,
    /// Whether the links to the resource were not all read.
    pub(crate) incoming_truncated: bool,
    /// Whether the resource's own triples were not all read.
    pub(crate) truncated: bool,
    /// Whether the resource has triples of its own (else it is only linked to, and has no
    /// RDF document).
    pub(crate) own: bool,
    /// Whether there were more than [`MAX_GEOMETRIES`] geometries.
    pub(crate) geometries_truncated: bool,
    /// The resource's geometries: literals of its own, of its blank nodes, and of the
    /// geometries it links to with `geo:hasGeometry` / `geo:hasDefaultGeometry`.
    pub(crate) geometries: Vec<Geometry>,
    /// What the map shows around them (`rd:context` of a map row: the section of line a
    /// track is part of...), drawn muted; at most [`MAX_GEOMETRIES`].
    pub(crate) context: Vec<Geometry>,
}

/// A geometry of the resource, for the map.
pub(crate) struct Geometry {
    pub(crate) literal: Literal,
    pub(crate) geom: std::sync::Arc<tripletrail::geo::Geom>,
}

/// An IRI, with its label when it has one.
#[derive(Clone)]
pub(crate) struct Link {
    pub(crate) iri: NamedNode,
    pub(crate) label: Option<String>,
}

pub(crate) struct Group {
    /// The `sh:PropertyGroup`, or none for rows outside any group.
    pub(crate) iri: Option<NamedNode>,
    pub(crate) label: String,
    /// The tab of the group (`rd:tab`), none for the overview.
    pub(crate) tab: Option<String>,
    order: f64,
    pub(crate) rows: Vec<Row>,
}

#[derive(Clone)]
pub(crate) struct Row {
    pub(crate) property: NamedNode,
    /// The values are the resources that link to this one.
    pub(crate) inverse: bool,
    /// The path in SPARQL syntax, when it is neither a property nor the inverse of one
    /// (`property` is then its first property).
    pub(crate) path: Option<String>,
    pub(crate) label: String,
    pub(crate) viewer: &'static str,
    pub(crate) values: Vec<Value>,
    /// Values not shown.
    pub(crate) more: usize,
    /// The unit written after the values (`rd:unit`).
    pub(crate) unit: Option<String>,
    /// How the value sums up the linked resources (`rd:aggregate` over `rd:over`).
    pub(crate) aggregate: Option<String>,
    /// A key figure (`dash:KeyInfoRole`), also shown under the title.
    pub(crate) key_info: bool,
    /// The rinf-describe widget that draws the row (`dash:viewer rd:…Viewer`), and the
    /// values of the paths it reads (`rd:begin`, `rd:km`...), by name.
    pub(crate) widget: Option<String>,
    pub(crate) params: Vec<(String, Vec<Value>)>,
    /// The reference marks of a scale bar (`rd:marks "400 Passenger train; 740 Freight
    /// train"`), and the scale it draws a range on (`rd:scale "4 7"`).
    pub(crate) marks: Vec<(f64, String)>,
    pub(crate) scale: Option<(f64, f64)>,
    /// The summary rows a key figures widget shows (`rd:figures`), in its order.
    pub(crate) figures: Vec<Row>,
    /// The keys of those rows (`rd:figures`).
    figure_keys: Vec<String>,
    /// The literal settings of a widget (`rd:lengthUnit "m"`, `rd:lanes "..."`), by name.
    pub(crate) settings: Vec<(String, String)>,
    /// The other rows of the page a widget draws from (`rd:table`, `rd:markers`,
    /// `rd:posts`), by key.
    pub(crate) refs: Vec<(String, Row)>,
    /// The row's key: its `rd:key` or `tts:key`, else its path (rows of a shape only).
    pub(crate) key: Option<String>,
    /// A copy, kept for a widget, of a row that is also on the page: the view names it by
    /// key instead of repeating it.
    shown: bool,
    /// The values as the rows of a table, when the property shape has `sh:node`.
    pub(crate) table: Option<Table>,
    /// Whether the path or summary was not read to the end (within its budget of triples):
    /// a summary is then of what was read.
    pub(crate) truncated: bool,
    order: f64,
}

/// The columns of a row's values (`sh:node`), and per value its cells.
#[derive(Clone)]
pub(crate) struct Table {
    pub(crate) columns: Vec<Column>,
    /// Per value of the row (in order), per column, the values of the cell.
    pub(crate) cells: Vec<Vec<Vec<Value>>>,
    /// Whether the table's budget of triples ran out: later cells may be empty, not read.
    pub(crate) truncated: bool,
}

#[derive(Clone)]
pub(crate) struct Column {
    pub(crate) key: String,
    pub(crate) label: String,
    pub(crate) unit: Option<String>,
    /// A column read for widgets but not shown (`rd:hidden`, `dash:hidden`).
    pub(crate) hidden: bool,
}

/// Columns of a table at most, and values of a cell.
const MAX_COLUMNS: usize = 20;
const MAX_CELL_VALUES: usize = 10;

#[derive(Clone)]
pub(crate) enum Value {
    Iri(Link),
    Literal(Literal),
    /// A blank node, as its own rows (property label, value).
    Blank(Vec<(Link, Value)>),
}

pub(crate) struct Incoming {
    pub(crate) property: Link,
    pub(crate) subjects: Vec<Value>,
    /// Links read with this property.
    pub(crate) count: usize,
}

/// What the page is built from.
struct Ctx<'a> {
    engine: &'a Engine,
    readable: Option<&'a ReadableGraphs>,
    /// A view without what row rules hide (#202), read instead of the store when given.
    rows: Option<&'a tripletrail::store::Snapshot>,
    shapes: &'a Graph,
    lang: &'a str,
    labels: Vec<NamedNode>,
    cache: std::cell::RefCell<HashMap<NamedNode, Option<String>>>,
}

fn sh(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{SH}{local}"))
}
fn dash(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{DASH}{local}"))
}
/// The rinf-describe vocabulary, whose shapes load as they are.
fn rd(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{RD}{local}"))
}
fn tts(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{TTS}{local}"))
}

/// The primary subtag of a language tag (`en` of `en-GB`).
fn primary(tag: &str) -> &str {
    tag.split('-').next().unwrap_or(tag)
}

fn local_name(iri: &str) -> &str {
    let cut = iri.rfind(['#', '/', ':']).map_or(0, |i| i + 1);
    match &iri[cut..] {
        "" => iri,
        name => name,
    }
}

fn namespace(iri: &str) -> &str {
    let cut = iri.rfind(['#', '/', ':']).map_or(0, |i| i + 1);
    &iri[..cut]
}

impl Ctx<'_> {
    fn quads(
        &self,
        s: Option<NamedOrBlankNodeRef<'_>>,
        p: Option<NamedNodeRef<'_>>,
        o: Option<TermRef<'_>>,
    ) -> impl Iterator<Item = Quad> + '_ {
        let readable = self.readable;
        let quads: Box<dyn Iterator<Item = Quad> + '_> = match self.rows {
            Some(view) => Box::new(view.quads_for_pattern(s, p, o, None)),
            None => Box::new(
                self.engine
                    .store()
                    .quads_for_pattern(s, p, o, None)
                    .filter_map(Result::ok),
            ),
        };
        quads.filter(move |q| readable.is_none_or(|r| r.contains(q.graph_name.as_ref())))
    }

    /// The values of `p` on `s`, without repeats, in store order.
    fn objects(&self, s: NamedOrBlankNodeRef<'_>, p: NamedNodeRef<'_>) -> Vec<Term> {
        let mut seen = HashSet::new();
        self.quads(Some(s), Some(p), None)
            .map(|q| q.object)
            .filter(|o| seen.insert(o.clone()))
            .collect()
    }

    /// The best literal among `values`: in the page's language, else without one, else any.
    fn pick(&self, values: impl IntoIterator<Item = Term>) -> Option<String> {
        let mut best: Option<(u8, String)> = None;
        for v in values {
            let Term::Literal(l) = v else { continue };
            let rank = match l.language() {
                Some(lang) if lang.eq_ignore_ascii_case(self.lang) => 0,
                // `en-GB` and `en`, either way round.
                Some(lang) if primary(lang).eq_ignore_ascii_case(primary(self.lang)) => 1,
                None => 2,
                Some(_) => 3,
            };
            if best.as_ref().is_none_or(|(r, _)| rank < *r) {
                best = Some((rank, l.value().to_owned()));
            }
        }
        best.map(|(_, s)| s)
    }

    fn label(&self, iri: &NamedNode) -> Option<String> {
        if let Some(known) = self.cache.borrow().get(iri) {
            return known.clone();
        }
        let label = self.labels.iter().find_map(|p| {
            self.pick(
                self.quads(Some(iri.as_ref().into()), Some(p.as_ref()), None)
                    .map(|q| q.object),
            )
        });
        self.cache.borrow_mut().insert(iri.clone(), label.clone());
        label
    }

    fn link(&self, iri: &NamedNode) -> Link {
        Link {
            iri: iri.clone(),
            label: self.label(iri),
        }
    }

    fn value(&self, term: &Term, depth: usize) -> Value {
        match term {
            Term::NamedNode(n) => Value::Iri(self.link(n)),
            Term::Literal(l) => Value::Literal(l.clone()),
            Term::BlankNode(b) if depth < MAX_DEPTH => Value::Blank(
                self.quads(Some(b.as_ref().into()), None, None)
                    .take(MAX_NESTED_ROWS)
                    .map(|q| {
                        let property = self.link(&q.predicate);
                        (property, self.value(&q.object, depth + 1))
                    })
                    .collect(),
            ),
            other => Value::Literal(Literal::new_simple_literal(other.to_string())),
        }
    }

    /// A literal of a shape, in the page's language when it has several.
    fn shape_text(&self, s: NamedOrBlankNodeRef<'_>, p: &NamedNode) -> Option<String> {
        self.pick(
            self.shapes
                .objects_for_subject_predicate(s, p)
                .map(TermRef::into_owned),
        )
    }

    fn shape_number(&self, s: NamedOrBlankNodeRef<'_>, p: &NamedNode) -> Option<f64> {
        match self.shapes.object_for_subject_predicate(s, p)? {
            TermRef::Literal(l) => l.value().parse().ok(),
            _ => None,
        }
    }

    fn shape_true(&self, s: NamedOrBlankNodeRef<'_>, p: &NamedNode) -> bool {
        matches!(
            self.shapes.object_for_subject_predicate(s, p),
            Some(TermRef::Literal(l)) if l.value() == "true" || l.value() == "1"
        )
    }
}

/// A SHACL property path (`sh:path`), with inverses pushed down to the properties.
#[derive(Clone)]
enum ShPath {
    Property(NamedNode, bool),
    Sequence(Vec<ShPath>),
    Alternative(Vec<ShPath>),
    ZeroOrMore(Box<ShPath>),
    OneOrMore(Box<ShPath>),
    ZeroOrOne(Box<ShPath>),
}

/// The predicates of a path node other than a sequence.
const PATH_KINDS: [&str; 5] = [
    "inversePath",
    "alternativePath",
    "zeroOrMorePath",
    "oneOrMorePath",
    "zeroOrOnePath",
];
/// Nodes of one path at most (a path and its shared named paths, counted each time used).
const MAX_PATH_NODES: usize = 256;
/// Paths nested at most, and members of a list read at most.
const MAX_PATH_DEPTH: usize = 16;
const MAX_PATH_LIST: usize = 64;
/// Triples read at most to follow a path from the resource.
const MAX_PATH_SCAN: usize = 100_000;

/// The members of an RDF list in the shapes graph.
fn list<'a>(shapes: &'a Graph, mut node: TermRef<'a>) -> Option<Vec<TermRef<'a>>> {
    let mut out = Vec::new();
    while node != TermRef::NamedNode(rdf::NIL) {
        let n: NamedOrBlankNodeRef<'_> = match node {
            TermRef::NamedNode(n) => n.into(),
            TermRef::BlankNode(b) => b.into(),
            _ => return None,
        };
        if out.len() == MAX_PATH_LIST {
            return None;
        }
        out.push(shapes.object_for_subject_predicate(n, rdf::FIRST)?);
        node = shapes.object_for_subject_predicate(n, rdf::REST)?;
    }
    Some(out)
}

impl ShPath {
    /// The path at `node` of the shapes graph, followed backwards if `inverse`; none when it
    /// is not a well-formed path.
    /// The path at `node`, or none when it is not a well-formed path or has more than
    /// [`MAX_PATH_NODES`] nodes (shared named paths could otherwise multiply without bound).
    fn read(shapes: &Graph, node: TermRef<'_>) -> Option<Self> {
        Self::parse(shapes, node, false, 0, &mut 0)
    }

    fn parse(
        shapes: &Graph,
        node: TermRef<'_>,
        inverse: bool,
        depth: usize,
        nodes: &mut usize,
    ) -> Option<Self> {
        *nodes += 1;
        if depth > MAX_PATH_DEPTH || *nodes > MAX_PATH_NODES {
            return None;
        }
        // A named node with path triples is that path (shapes files share paths that way).
        let b: NamedOrBlankNodeRef<'_> = match node {
            TermRef::NamedNode(n)
                if !PATH_KINDS
                    .iter()
                    .any(|k| shapes.object_for_subject_predicate(n, &sh(k)).is_some())
                    && shapes.object_for_subject_predicate(n, rdf::FIRST).is_none() =>
            {
                return Some(Self::Property(n.into_owned(), inverse));
            }
            TermRef::NamedNode(n) => n.into(),
            TermRef::BlankNode(b) => b.into(),
            _ => return None,
        };
        let one = |p: &str| shapes.object_for_subject_predicate(b, &sh(p));
        if let Some(t) = one("inversePath") {
            return Self::parse(shapes, t, !inverse, depth + 1, nodes);
        }
        if let Some(t) = one("alternativePath") {
            let members = list(shapes, t)?
                .into_iter()
                .map(|m| Self::parse(shapes, m, inverse, depth + 1, nodes))
                .collect::<Option<Vec<_>>>()?;
            return (!members.is_empty()).then_some(Self::Alternative(members));
        }
        if let Some(t) = one("zeroOrMorePath") {
            return Some(Self::ZeroOrMore(Box::new(Self::parse(
                shapes,
                t,
                inverse,
                depth + 1,
                nodes,
            )?)));
        }
        if let Some(t) = one("oneOrMorePath") {
            return Some(Self::OneOrMore(Box::new(Self::parse(
                shapes,
                t,
                inverse,
                depth + 1,
                nodes,
            )?)));
        }
        if let Some(t) = one("zeroOrOnePath") {
            return Some(Self::ZeroOrOne(Box::new(Self::parse(
                shapes,
                t,
                inverse,
                depth + 1,
                nodes,
            )?)));
        }
        // A sequence: a list of two or more paths, the other way round when inverse.
        let mut members = list(shapes, node)?
            .into_iter()
            .map(|m| Self::parse(shapes, m, inverse, depth + 1, nodes))
            .collect::<Option<Vec<_>>>()?;
        if members.len() < 2 {
            return None;
        }
        if inverse {
            members.reverse();
        }
        Some(Self::Sequence(members))
    }

    /// The path in SPARQL syntax, with full IRIs, or local names when `short`.
    fn sparql(&self, short: bool) -> String {
        let join = |members: &[ShPath], sep: &str| {
            members
                .iter()
                .map(|m| m.sparql(short))
                .collect::<Vec<_>>()
                .join(sep)
        };
        match self {
            Self::Property(p, inverse) => format!(
                "{}{}",
                if *inverse { "^" } else { "" },
                if short {
                    local_name(p.as_str()).to_owned()
                } else {
                    p.to_string()
                }
            ),
            Self::Sequence(m) => format!("({})", join(m, " / ")),
            Self::Alternative(m) => format!("({})", join(m, " | ")),
            Self::ZeroOrMore(p) => format!("{}*", p.sparql(short)),
            Self::OneOrMore(p) => format!("{}+", p.sparql(short)),
            Self::ZeroOrOne(p) => format!("{}?", p.sparql(short)),
        }
    }

    /// The first property of the path.
    fn first(&self) -> &NamedNode {
        match self {
            Self::Property(p, _) => p,
            Self::Sequence(m) | Self::Alternative(m) => m[0].first(),
            Self::ZeroOrMore(p) | Self::OneOrMore(p) | Self::ZeroOrOne(p) => p.first(),
        }
    }

    /// The nodes the path reaches from `from`, without repeats, in the order found; reads
    /// at most `budget` triples more (and stops when it runs out).
    fn follow(&self, ctx: &Ctx<'_>, from: &[Term], budget: &mut usize) -> Vec<Term> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        let mut add = |out: &mut Vec<Term>, t: Term| {
            if seen.insert(t.clone()) {
                out.push(t);
            }
        };
        match self {
            Self::Property(p, inverse) => {
                for f in from {
                    if *budget == 0 {
                        break;
                    }
                    let quads: Box<dyn Iterator<Item = Quad>> = if *inverse {
                        Box::new(ctx.quads(None, Some(p.as_ref()), Some(f.as_ref())))
                    } else {
                        let s: NamedOrBlankNodeRef<'_> = match f {
                            Term::NamedNode(n) => n.as_ref().into(),
                            Term::BlankNode(b) => b.as_ref().into(),
                            _ => continue,
                        };
                        Box::new(ctx.quads(Some(s), Some(p.as_ref()), None))
                    };
                    for q in quads {
                        if *budget == 0 {
                            break;
                        }
                        *budget -= 1;
                        add(&mut out, if *inverse { q.subject.into() } else { q.object });
                    }
                }
            }
            Self::Sequence(members) => {
                let mut nodes = from.to_vec();
                for m in members {
                    nodes = m.follow(ctx, &nodes, budget);
                }
                for t in nodes {
                    add(&mut out, t);
                }
            }
            Self::Alternative(members) => {
                for m in members {
                    for t in m.follow(ctx, from, budget) {
                        add(&mut out, t);
                    }
                }
            }
            Self::ZeroOrOne(p) => {
                for t in from.iter().cloned().chain(p.follow(ctx, from, budget)) {
                    add(&mut out, t);
                }
            }
            Self::ZeroOrMore(p) | Self::OneOrMore(p) => {
                let zero = matches!(self, Self::ZeroOrMore(_));
                let mut reached: HashSet<Term> = HashSet::new();
                if zero {
                    for t in from {
                        reached.insert(t.clone());
                        add(&mut out, t.clone());
                    }
                }
                let mut frontier = from.to_vec();
                while !frontier.is_empty() && *budget > 0 {
                    let next: Vec<Term> = p
                        .follow(ctx, &frontier, budget)
                        .into_iter()
                        .filter(|t| reached.insert(t.clone()))
                        .collect();
                    for t in &next {
                        add(&mut out, t.clone());
                    }
                    frontier = next;
                }
            }
        }
        out
    }
}

/// The table of `values` by the property shapes of the node shape `node` (its columns, in
/// `sh:order`, without hidden ones); none without columns. Its cells are read within one
/// budget of triples for the table.
fn table(ctx: &Ctx<'_>, node: TermRef<'_>, values: &[Term]) -> Option<Table> {
    let node: NamedOrBlankNodeRef<'_> = match node {
        TermRef::NamedNode(n) => n.into(),
        TermRef::BlankNode(b) => b.into(),
        _ => return None,
    };
    let shapes: Vec<NamedOrBlankNodeRef<'_>> = ctx
        .shapes
        .objects_for_subject_predicate(node, &sh("property"))
        .filter_map(|t| match t {
            TermRef::NamedNode(n) => Some(n.into()),
            TermRef::BlankNode(b) => Some(b.into()),
            _ => None,
        })
        .filter(|p| !ctx.shape_true(*p, &sh("deactivated")))
        .collect();
    let mut keyed: Vec<(f64, String, NamedOrBlankNodeRef<'_>)> = shapes
        .into_iter()
        .map(|p| {
            (
                ctx.shape_number(p, &sh("order")).unwrap_or(f64::MAX),
                shape_text_key(ctx.shapes, p),
                p,
            )
        })
        .collect();
    keyed.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let shapes = keyed.into_iter().map(|(_, _, p)| p);
    let mut columns = Vec::new();
    let mut paths = Vec::new();
    // Shown and hidden columns are capped apart, so helper columns for widgets never push
    // shown ones out.
    let (mut shown, mut helpers) = (0, 0);
    for p in shapes {
        let hidden = ctx.shape_true(p, &dash("hidden")) || ctx.shape_true(p, &rd("hidden"));
        let n = if hidden { &mut helpers } else { &mut shown };
        if *n == MAX_COLUMNS {
            continue;
        }
        let Some(path) = ctx
            .shapes
            .object_for_subject_predicate(p, &sh("path"))
            .and_then(|t| ShPath::read(ctx.shapes, t))
        else {
            continue;
        };
        let key = [tts("key"), rd("key")]
            .iter()
            .find_map(|k| match ctx.shapes.object_for_subject_predicate(p, k) {
                Some(TermRef::Literal(l)) => Some(l.value().to_owned()),
                _ => None,
            })
            .unwrap_or_else(|| path.sparql(false));
        let label = ctx
            .shape_text(p, &sh("name"))
            .unwrap_or_else(|| match &path {
                ShPath::Property(n, false) => ctx
                    .label(n)
                    .unwrap_or_else(|| local_name(n.as_str()).to_owned()),
                other => other.sparql(true),
            });
        columns.push(Column {
            key,
            label,
            unit: ctx.shape_text(p, &rd("unit")),
            hidden,
        });
        if hidden {
            helpers += 1;
        } else {
            shown += 1;
        }
        paths.push(path);
    }
    if columns.is_empty() {
        return None;
    }
    let mut budget = MAX_PATH_SCAN;
    let cells = values
        .iter()
        .take(MAX_VALUES)
        .map(|v| {
            paths
                .iter()
                .map(|path| {
                    path.follow(ctx, std::slice::from_ref(v), &mut budget)
                        .iter()
                        .take(MAX_CELL_VALUES)
                        .map(|t| ctx.value(t, MAX_DEPTH))
                        .collect()
                })
                .collect()
        })
        .collect();
    Some(Table {
        columns,
        cells,
        truncated: budget == 0,
    })
}

/// The `rd:` terms of a row that are settings or structure, not paths a widget reads.
const RD_TERMS: [&str; 22] = [
    "key",
    "over",
    "class",
    "aggregate",
    "unit",
    "badge",
    "tab",
    "hidden",
    "scope",
    "limit",
    "sortBy",
    "table",
    "figures",
    "symbol",
    "layers",
    "weightUnit",
    "cacheSeconds",
    "lanes",
    "profiles",
    "marks",
    "scale",
    "indexRoot",
];
/// Values of a widget's path at most.
const MAX_PARAM_VALUES: usize = 10;

/// The paths a widget reads (`rd:begin shape:validity-begin`, `rd:km ( ... )`), followed
/// from the resource within one budget, by name; a name used more than once is numbered
/// (`facility`, `facility.1`...).
fn widget_params(
    ctx: &Ctx<'_>,
    p: NamedOrBlankNodeRef<'_>,
    iri: &NamedNode,
) -> Vec<(String, Vec<Value>)> {
    let mut found: Vec<(String, ShPath)> = ctx
        .shapes
        .triples_for_subject(p)
        .filter_map(|t| {
            let name = t.predicate.as_str().strip_prefix(RD)?;
            if RD_TERMS.contains(&name) || matches!(t.object, TermRef::Literal(_)) {
                return None;
            }
            Some((name.to_owned(), ShPath::read(ctx.shapes, t.object)?))
        })
        .collect();
    found.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.sparql(false).cmp(&b.1.sparql(false)))
    });
    let mut budget = MAX_PATH_SCAN;
    let mut out: Vec<(String, Vec<Value>)> = Vec::new();
    for (name, path) in found.into_iter().take(MAX_COLUMNS) {
        // A facility (`rd:facility`, one per boolean property) is named by its property.
        let name = match &path {
            ShPath::Property(n, false) if name == "facility" => ctx
                .label(n)
                .unwrap_or_else(|| local_name(n.as_str()).to_owned()),
            _ => name,
        };
        let n = out
            .iter()
            .filter(|(k, _)| k == &name || k.starts_with(&format!("{name}.")))
            .count();
        let key = if n == 0 { name } else { format!("{name}.{n}") };
        let values = path
            .follow(ctx, &[Term::from(iri.clone())], &mut budget)
            .iter()
            .take(MAX_PARAM_VALUES)
            .map(|t| ctx.value(t, MAX_DEPTH))
            .collect();
        out.push((key, values));
    }
    out
}

/// The ERA vocabulary (RINF), and its data status properties with what they say.
const ERA: &str = "http://data.europa.eu/949/";
const DATA_STATUS: [(&str, &str); 2] = [
    (
        "notApplicable",
        "The data states that these parameters do not apply here.",
    ),
    (
        "notYetAvailable",
        "The data states that these parameters are not published yet.",
    ),
];

/// The QUDT units of the ERA ontology (`era:unitOfMeasure`) as the symbols written after a
/// value, as rinf-describe writes them.
const UNIT_SYMBOLS: [(&str, &str); 25] = [
    ("M", "m"),
    ("KiloM", "km"),
    ("MilliM", "mm"),
    ("M2", "m\u{b2}"),
    ("KiloM-PER-HR", "km/h"),
    ("MilliM-PER-M", "\u{2030}"),
    ("M-PER-SEC2", "m/s\u{b2}"),
    ("V", "V"),
    ("A", "A"),
    ("A-PER-M", "A/m"),
    ("N", "N"),
    ("KiloN", "kN"),
    ("OHM", "\u{3a9}"),
    ("KiloGM", "kg"),
    ("TONNE", "t"),
    ("KiloJ", "kJ"),
    ("KiloW", "kW"),
    ("SEC", "s"),
    ("MIN", "min"),
    ("HZ", "Hz"),
    ("NanoFARAD", "nF"),
    ("MilliH", "mH"),
    ("DeciB", "dB"),
    ("DeciB_A", "dB(A)"),
    ("GM-PER-SEC", "g/s"),
];

/// Linked resources whose parameters the RINF parameters table also lists, at most.
const MAX_PARAMETER_LINKS: usize = 100;
/// Lines of the RINF parameters table, at most (the first by index; the table is then marked
/// truncated).
const MAX_PARAMETER_LINES: usize = 1000;

/// The sort key of a RINF index (`1.1.1.2.3`): its numbers, a part that is not one as 0.
fn index_key(ix: &str) -> Vec<u64> {
    ix.split('.').map(|x| x.parse().unwrap_or(0)).collect()
}

/// One line of the RINF parameters table.
struct ParameterLine {
    property: NamedNode,
    index: Option<String>,
    values: Vec<Term>,
    /// The property of the resource that links to the one the values are on, and that one.
    via: Option<(NamedNode, NamedNode)>,
    /// `notApplicable` or `notYetAvailable`, for a parameter the data marks so.
    status: Option<&'static str>,
}

/// rinf-describe's `rd:ParameterIndexViewer`: every RINF parameter of the resource (the
/// properties with an `era:rinfIndex` in the dataset's ERA ontology), also those of the
/// resources it links to that are not infrastructure elements themselves (its contact line
/// system, ETCS...), and those the data marks not applicable or not yet available; ordered by
/// index under the resource's index root (`rd:indexRoot`, else the root most of its
/// parameters share), each with the ontology's unit and the two innermost chapters of the
/// index (the labels of the properties with that index). As the row's values (the
/// parameters) and a table with columns `index`, `value`, `on` and `status`, and hidden
/// `unit`, `chapter` and `section` (each an index and its label).
fn parameter_index(
    ctx: &Ctx<'_>,
    resource: &NamedNode,
    outgoing: &[(NamedNode, Vec<Term>)],
    root: Option<&str>,
) -> Option<(Vec<Value>, Table)> {
    let rinf_index = NamedNode::new_unchecked(format!("{ERA}rinfIndex"));
    let infrastructure = NamedNode::new_unchecked(format!("{ERA}InfrastructureElement"));
    let mut indexes: HashMap<NamedNode, Vec<String>> = HashMap::new();
    let mut index_of = |p: &NamedNode| -> Vec<String> {
        indexes
            .entry(p.clone())
            .or_insert_with(|| {
                let mut ix: Vec<String> = ctx
                    .objects(p.as_ref().into(), rinf_index.as_ref())
                    .iter()
                    .filter_map(|t| match t {
                        Term::Literal(l) => Some(l.value().trim().to_owned()),
                        _ => None,
                    })
                    .filter(|t| !t.is_empty())
                    .collect();
                ix.sort_by_key(|i| index_key(i));
                ix.dedup();
                ix
            })
            .clone()
    };
    let mut lines: Vec<ParameterLine> = Vec::new();
    let mut given: HashSet<NamedNode> = HashSet::new();
    let mut budget = MAX_PATH_SCAN;
    let mut linked = 0;
    for (q, terms) in outgoing {
        if !index_of(q).is_empty() {
            given.insert(q.clone());
            lines.push(ParameterLine {
                property: q.clone(),
                index: None,
                values: terms.clone(),
                via: None,
                status: None,
            });
        }
        for t in terms {
            // A link to itself lists no parameter twice.
            let Term::NamedNode(x) = t else { continue };
            if x == resource {
                continue;
            }
            if linked >= MAX_PARAMETER_LINKS || budget == 0 {
                break;
            }
            if ctx
                .quads(
                    Some(x.as_ref().into()),
                    Some(rdf::TYPE),
                    Some(infrastructure.as_ref().into()),
                )
                .next()
                .is_some()
            {
                continue;
            }
            linked += 1;
            let mut found: Vec<(NamedNode, Vec<Term>)> = Vec::new();
            for quad in ctx.quads(Some(x.as_ref().into()), None, None) {
                if budget == 0 {
                    break;
                }
                budget -= 1;
                match found.iter_mut().find(|(p, _)| *p == quad.predicate) {
                    Some((_, v)) => {
                        if v.len() < MAX_VALUES && !v.contains(&quad.object) {
                            v.push(quad.object);
                        }
                    }
                    None => found.push((quad.predicate, vec![quad.object])),
                }
            }
            for (p, values) in found {
                if !index_of(&p).is_empty() {
                    given.insert(p.clone());
                    lines.push(ParameterLine {
                        property: p,
                        index: None,
                        values,
                        via: Some((q.clone(), x.clone())),
                        status: None,
                    });
                }
            }
        }
    }
    for (k, _) in DATA_STATUS {
        let marked = NamedNode::new_unchecked(format!("{ERA}{k}"));
        for (_, terms) in outgoing.iter().filter(|(q, _)| *q == marked) {
            for t in terms {
                let Term::NamedNode(p) = t else { continue };
                if !given.contains(p) && !index_of(p).is_empty() {
                    given.insert(p.clone());
                    lines.push(ParameterLine {
                        property: p.clone(),
                        index: None,
                        values: Vec::new(),
                        via: None,
                        status: Some(k),
                    });
                }
            }
        }
    }
    if lines.is_empty() {
        return None;
    }
    // The index roots: set on the row, else by how many parameters share each.
    let roots: Vec<String> = match root {
        Some(r) => r
            .split([' ', ','])
            .filter(|r| !r.is_empty())
            .map(str::to_owned)
            .collect(),
        None => {
            let mut votes: HashMap<String, usize> = HashMap::new();
            let mut counted = HashSet::new();
            for l in &lines {
                if !counted.insert(l.property.clone()) {
                    continue;
                }
                let own: HashSet<String> = index_of(&l.property)
                    .iter()
                    .map(|ix| ix.split('.').take(3).collect::<Vec<_>>().join("."))
                    .collect();
                for r in own {
                    *votes.entry(r).or_default() += 1;
                }
            }
            let mut roots: Vec<(String, usize)> = votes.into_iter().collect();
            roots.sort_by(|a, b| {
                b.1.cmp(&a.1)
                    .then_with(|| index_key(&a.0).cmp(&index_key(&b.0)))
            });
            roots.into_iter().map(|(r, _)| r).collect()
        }
    };
    for l in &mut lines {
        let ix = index_of(&l.property);
        l.index = roots
            .iter()
            .find_map(|r| ix.iter().find(|i| i.starts_with(&format!("{r}."))))
            .or(ix.first())
            .cloned();
    }
    let name = |p: &NamedNode| {
        let mut name = ctx
            .label(p)
            .unwrap_or_else(|| local_name(p.as_str()).to_owned());
        if let Some(first) = name.chars().next() {
            name = first.to_uppercase().chain(name.chars().skip(1)).collect();
        }
        name
    };
    lines.sort_by_cached_key(|l| {
        (
            l.index.as_deref().map_or(vec![99], index_key),
            name(&l.property),
        )
    });
    let cut = lines.len() > MAX_PARAMETER_LINES;
    lines.truncate(MAX_PARAMETER_LINES);
    // The chapters: the labels of the properties whose index is that of a chapter.
    let mut chapters: HashMap<String, String> = HashMap::new();
    for quad in ctx.quads(None, Some(rinf_index.as_ref()), None) {
        let (NamedOrBlankNode::NamedNode(p), Term::Literal(ix)) = (&quad.subject, &quad.object)
        else {
            continue;
        };
        let ix = ix.value().trim().trim_end_matches('.');
        if (ix.matches('.').count() <= 4 || ix.ends_with(".0"))
            && !chapters.contains_key(ix)
            && let Some(label) = ctx.label(p)
        {
            let mut c = label.chars();
            let label: String = c
                .next()
                .into_iter()
                .flat_map(char::to_uppercase)
                .chain(c)
                .collect();
            chapters.insert(ix.to_owned(), label);
        }
    }
    let unit_of_measure = NamedNode::new_unchecked(format!("{ERA}unitOfMeasure"));
    let text = |s: &str| vec![Value::Literal(Literal::new_simple_literal(s))];
    let mut values = Vec::new();
    let mut cells = Vec::new();
    for l in &lines {
        values.push(Value::Iri(Link {
            iri: l.property.clone(),
            label: Some(name(&l.property)),
        }));
        let unit = ctx
            .objects(l.property.as_ref().into(), unit_of_measure.as_ref())
            .into_iter()
            .find_map(|u| match u {
                Term::NamedNode(u) => {
                    let short = u.as_str().strip_prefix("http://qudt.org/vocab/unit/")?;
                    Some(
                        UNIT_SYMBOLS
                            .iter()
                            .find(|(k, _)| *k == short)
                            .map_or(short, |(_, s)| s)
                            .to_owned(),
                    )
                }
                _ => None,
            });
        let mut found: Vec<Vec<Value>> = Vec::new();
        if let Some(ix) = &l.index {
            let parts: Vec<&str> = ix.split('.').collect();
            for n in 3..parts.len() {
                let at = parts[..n].join(".");
                if let Some(label) = chapters.get(&at) {
                    found.push(vec![
                        Value::Literal(Literal::new_simple_literal(at)),
                        Value::Literal(Literal::new_simple_literal(label.clone())),
                    ]);
                }
            }
        }
        let section = if found.len() >= 2 { found.pop() } else { None };
        let chapter = found.pop();
        cells.push(vec![
            l.index.as_deref().map(text).unwrap_or_default(),
            l.values
                .iter()
                .take(MAX_CELL_VALUES)
                .map(|t| ctx.value(t, 0))
                .collect(),
            l.via
                .as_ref()
                .map(|(via, of)| {
                    vec![Value::Iri(Link {
                        iri: of.clone(),
                        label: Some(name(via)),
                    })]
                })
                .unwrap_or_default(),
            l.status
                .map(|s| {
                    text(if s == "notApplicable" {
                        "Not applicable"
                    } else {
                        "Not yet available"
                    })
                })
                .unwrap_or_default(),
            unit.as_deref().map(text).unwrap_or_default(),
            chapter.unwrap_or_default(),
            section.unwrap_or_default(),
        ]);
    }
    let column = |key: &str, label: &str, hidden: bool| Column {
        key: key.to_owned(),
        label: label.to_owned(),
        unit: None,
        hidden,
    };
    Some((
        values,
        Table {
            columns: vec![
                column("index", "Index", false),
                column("value", "Value", false),
                column("on", "On", false),
                column("status", "Status", false),
                column("unit", "Unit", true),
                column("chapter", "Chapter", true),
                column("section", "Section", true),
            ],
            cells,
            truncated: budget == 0 || cut,
        },
    ))
}

/// The settings that name other rows of the page a widget draws from, by key.
const REF_SETTINGS: [&str; 6] = [
    "table",
    "markers",
    "posts",
    "platforms",
    "sidings",
    "elements",
];

/// Literal `rd:` settings of a widget at most.
const MAX_SETTINGS: usize = 32;

/// The literal `rd:` settings of a widget's property shape (`rd:lengthUnit "m"`), sorted by
/// name; a repeated setting keeps its first value.
fn widget_settings(ctx: &Ctx<'_>, p: NamedOrBlankNodeRef<'_>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = ctx
        .shapes
        .triples_for_subject(p)
        .filter_map(|t| match t.object {
            TermRef::Literal(l) => Some((
                t.predicate.as_str().strip_prefix(RD)?.to_owned(),
                l.value().to_owned(),
            )),
            _ => None,
        })
        .collect();
    out.sort();
    out.dedup_by(|a, b| a.0 == b.0);
    out.truncate(MAX_SETTINGS);
    out
}

/// Reference marks of a scale bar: value and label.
type Marks = Vec<(f64, String)>;

/// The marks of a scale bar, `"400 Passenger train; 740 Freight train"`, and its scale,
/// `"4 7"`. A mark that does not start with a number is left out.
fn scale_marks(ctx: &Ctx<'_>, p: NamedOrBlankNodeRef<'_>) -> (Marks, Option<(f64, f64)>) {
    let marks = ctx
        .shape_text(p, &rd("marks"))
        .map(|m| {
            m.split(';')
                .filter_map(|mark| {
                    let mark = mark.trim();
                    let (n, label) = mark.split_once(char::is_whitespace).unwrap_or((mark, ""));
                    let n: f64 = n.parse().ok().filter(|n: &f64| n.is_finite())?;
                    Some((n, label.trim().to_owned()))
                })
                .take(MAX_PARAM_VALUES)
                .collect()
        })
        .unwrap_or_default();
    let scale = ctx.shape_text(p, &rd("scale")).and_then(|s| {
        let mut n = s.split_whitespace().map(str::parse::<f64>);
        match (n.next(), n.next()) {
            (Some(Ok(a)), Some(Ok(b))) if a.is_finite() && b.is_finite() && a < b => Some((a, b)),
            _ => None,
        }
    });
    (marks, scale)
}

/// A key of a property shape from what it says (its triples, with blank nodes followed),
/// not from its blank node label: shapes of equal `sh:order` sort the same on every server.
fn shape_text_key(shapes: &Graph, p: NamedOrBlankNodeRef<'_>) -> String {
    fn write(shapes: &Graph, s: NamedOrBlankNodeRef<'_>, depth: usize, out: &mut Vec<String>) {
        if let NamedOrBlankNodeRef::NamedNode(n) = s
            && depth > 0
        {
            out.push(n.to_string());
            return;
        }
        let mut lines: Vec<String> = Vec::new();
        for t in shapes.triples_for_subject(s) {
            let mut line = vec![t.predicate.to_string()];
            match t.object {
                TermRef::BlankNode(b) if depth < 8 => write(shapes, b.into(), depth + 1, &mut line),
                o => line.push(o.to_string()),
            }
            lines.push(line.join(" "));
        }
        lines.sort();
        out.push(lines.join(" ; "));
    }
    let mut out = Vec::new();
    write(shapes, p, 0, &mut out);
    out.join(" ")
}

/// A number of a literal, for sums and ranges.
fn number(t: &Term) -> Option<f64> {
    match t {
        Term::Literal(l) => l.value().trim().parse().ok(),
        _ => None,
    }
}

fn plain(n: f64) -> String {
    let mut s = format!("{n:.3}");
    while s.contains('.') && (s.ends_with('0') || s.ends_with('.')) {
        s.pop();
    }
    s
}

/// The summary `kind` (`count`, `distinct-count`, `range` or `sum`, as rinf-describe's
/// `rd:aggregate`) of the values of `path` on the `linked` resources, as text, read as
/// rinf-describe does: `count` counts the linked resources, a distinct count ends with
/// `none (n)` for those without a value (so linked resources none of which has a value still
/// give a row), and a range of only some of them ends with `(k of n)`. None when nothing is
/// linked, and for a range or sum when no linked resource has a value.
fn summary(
    ctx: &Ctx<'_>,
    kind: &str,
    path: &ShPath,
    linked: &[Term],
    budget: &mut usize,
) -> Option<String> {
    let mut with_value = 0usize;
    let mut counts: Vec<(Term, usize)> = Vec::new();
    let mut at: HashMap<Term, usize> = HashMap::new();
    let mut numbers: Vec<f64> = Vec::new();
    let mut sum = 0.0;
    for l in linked {
        let values = path.follow(ctx, std::slice::from_ref(l), budget);
        if values.is_empty() {
            continue;
        }
        with_value += 1;
        if let Some(n) = number(&values[0]) {
            sum += n;
        }
        for v in values {
            numbers.extend(number(&v));
            match at.get(&v) {
                Some(&i) => counts[i].1 += 1,
                None => {
                    at.insert(v.clone(), counts.len());
                    counts.push((v, 1));
                }
            }
        }
    }
    let children = linked.len();
    if children == 0 {
        return None;
    }
    Some(match kind {
        "count" => children.to_string(),
        "sum" if with_value > 0 => plain(sum),
        "range" | "sum" => {
            if numbers.is_empty() {
                return None;
            }
            let min = numbers.iter().copied().fold(f64::MAX, f64::min);
            let max = numbers.iter().copied().fold(f64::MIN, f64::max);
            let range = if min == max {
                plain(min)
            } else {
                format!("{}\u{2013}{}", plain(min), plain(max))
            };
            if with_value < children {
                format!("{range} ({with_value} of {children})")
            } else {
                range
            }
        }
        _ => {
            counts.sort_by_key(|c| std::cmp::Reverse(c.1));
            let mut parts: Vec<String> = counts
                .iter()
                .take(MAX_VALUES)
                .map(|(t, c)| {
                    let text = match t {
                        Term::NamedNode(n) => ctx
                            .label(n)
                            .unwrap_or_else(|| local_name(n.as_str()).to_owned()),
                        Term::Literal(l) => l.value().to_owned(),
                        other => other.to_string(),
                    };
                    format!("{text} ({c})")
                })
                .collect();
            if with_value < children {
                parts.push(format!("none ({})", children - with_value));
            }
            parts.join(" \u{b7} ")
        }
    })
}

/// The geometries in `term` (a value of the resource): itself, its blank node's values, or the
/// serialisations of a geometry it links to by `property`.
fn collect_geometries(
    ctx: &Ctx<'_>,
    property: &NamedNode,
    term: &Term,
    depth: usize,
    out: &mut Vec<Geometry>,
) {
    if out.len() > MAX_GEOMETRIES {
        return;
    }
    match term {
        Term::Literal(l) => {
            if let Some(geom) = tripletrail::geo::parse_wkt_literal(term) {
                out.push(Geometry {
                    literal: l.clone(),
                    geom,
                });
            }
        }
        Term::BlankNode(b) if depth < 2 => {
            for q in ctx
                .quads(Some(b.as_ref().into()), None, None)
                .take(MAX_NESTED_ROWS)
            {
                collect_geometries(ctx, &q.predicate, &q.object, depth + 1, out);
            }
        }
        Term::NamedNode(n)
            if depth < 2
                && matches!(
                    property.as_str().strip_prefix(GEO),
                    Some("hasGeometry" | "hasDefaultGeometry")
                ) =>
        {
            for q in ctx
                .quads(Some(n.as_ref().into()), None, None)
                .take(MAX_NESTED_ROWS)
            {
                if matches!(
                    q.predicate.as_str().strip_prefix(GEO),
                    Some("asWKT" | "asGML")
                ) {
                    collect_geometries(ctx, &q.predicate, &q.object, depth + 1, out);
                }
            }
        }
        _ => {}
    }
}

/// The classes of `types` and their superclasses (`rdfs:subClassOf` in the data), the types
/// first.
fn with_superclasses(ctx: &Ctx<'_>, types: &[NamedNode]) -> Vec<NamedNode> {
    // The most specific classes first: a type that is a superclass of another type (a
    // resource typed both era:OperationalPoint and era:InfrastructureElement) comes later.
    let closure = |t: &NamedNode| {
        let mut seen = HashSet::new();
        let mut todo = vec![t.clone()];
        while let Some(c) = todo.pop() {
            if seen.len() >= MAX_CLASSES || !seen.insert(c.clone()) {
                continue;
            }
            for s in ctx.objects(c.as_ref().into(), rdfs::SUB_CLASS_OF) {
                if let Term::NamedNode(s) = s {
                    todo.push(s);
                }
            }
        }
        seen.remove(t);
        seen
    };
    let above: HashSet<NamedNode> = types.iter().flat_map(closure).collect();
    let (general, specific): (Vec<&NamedNode>, Vec<&NamedNode>) =
        types.iter().partition(|t| above.contains(*t));
    let mut all: Vec<NamedNode> = Vec::new();
    let mut seen = HashSet::new();
    let mut queue: std::collections::VecDeque<NamedNode> =
        specific.into_iter().chain(general).cloned().collect();
    while let Some(c) = queue.pop_front() {
        if all.len() >= MAX_CLASSES || !seen.insert(c.clone()) {
            continue;
        }
        for s in ctx.objects(c.as_ref().into(), rdfs::SUB_CLASS_OF) {
            if let Term::NamedNode(s) = s {
                queue.push_back(s);
            }
        }
        all.push(c);
    }
    all
}

/// The node shapes that apply to `classes`, in their order, without `sh:deactivated` ones.
fn node_shapes(ctx: &Ctx<'_>, classes: &[NamedNode]) -> Vec<NamedOrBlankNode> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let node_shape = sh("NodeShape");
    for class in classes {
        let class_term = TermRef::from(class.as_ref());
        let mut found: Vec<NamedOrBlankNode> = ctx
            .shapes
            .subjects_for_predicate_object(&sh("targetClass"), class_term)
            .chain(
                ctx.shapes
                    .subjects_for_predicate_object(&dash("applicableToClass"), class_term),
            )
            .map(NamedOrBlankNodeRef::into_owned)
            .collect();
        if ctx
            .shapes
            .contains(tripletrail::oxigraph::model::TripleRef::new(
                class,
                rdf::TYPE,
                &node_shape,
            ))
        {
            found.push(class.clone().into());
        }
        found.sort_by_key(ToString::to_string);
        for s in found {
            if !ctx.shape_true(s.as_ref(), &sh("deactivated")) && seen.insert(s.clone()) {
                out.push(s);
            }
        }
    }
    // The dataset's own shapes before the generated ones, which only fill in what they leave.
    out.sort_by_key(|s| s.to_string().starts_with("<urn:tripletrail:shape:"));
    out
}

/// The viewer named by `dash:viewer`, or the default for `values`.
fn viewer(named: Option<&str>, values: &[Value]) -> &'static str {
    const VIEWERS: [&str; 9] = [
        "LabelViewer",
        "LiteralViewer",
        "URIViewer",
        "DetailsViewer",
        "ValueTableViewer",
        "ImageViewer",
        "HTMLViewer",
        "LangStringViewer",
        "HyperlinkViewer",
    ];
    if let Some(v) = named.and_then(|n| VIEWERS.iter().find(|v| **v == n)) {
        return v;
    }
    match values.first() {
        Some(Value::Blank(_)) => "DetailsViewer",
        Some(Value::Literal(_)) => "LiteralViewer",
        _ => "LabelViewer",
    }
}

/// The index of the group `group` in `groups`, added (with its label and order) when new.
fn group_of(
    groups: &mut Vec<Group>,
    ctx: &Ctx<'_>,
    group: Option<NamedNode>,
    fallback: f64,
) -> usize {
    if let Some(i) = groups.iter().position(|g| g.iri == group) {
        return i;
    }
    // The groups of the vocabulary (`tts.ttl`), which the generated shapes name.
    let known = match group.as_ref().and_then(|g| g.as_str().strip_prefix(TTS)) {
        Some("Group-Attributes") => Some(("Attributes", 0.0)),
        Some("Group-Links") => Some(("Links", 1.0)),
        Some("Group-DataStatus") => Some(("Data status", 1e6)),
        _ => None,
    };
    let (label, order) = match &group {
        Some(g) => (
            ctx.shape_text(g.as_ref().into(), &rdfs::LABEL.into_owned())
                .unwrap_or_else(|| match known {
                    Some((label, _)) => label.to_owned(),
                    None => local_name(g.as_str()).to_owned(),
                }),
            ctx.shape_number(g.as_ref().into(), &sh("order"))
                .or(known.map(|(_, order)| order))
                .unwrap_or(fallback),
        ),
        None => (String::new(), f64::NEG_INFINITY),
    };
    let tab = group
        .as_ref()
        .and_then(|g| ctx.shape_text(g.as_ref().into(), &rd("tab")));
    groups.push(Group {
        tab,
        iri: group,
        label,
        order,
        rows: Vec::new(),
    });
    groups.len() - 1
}

/// The view model of `iri`, or `None` when nothing (readable) is known about it.
pub(crate) fn build(
    engine: &Engine,
    shapes: &Graph,
    iri: &NamedNode,
    readable: Option<&ReadableGraphs>,
    rows: Option<&tripletrail::store::Snapshot>,
    lang: &str,
) -> Result<Option<View>, HttpError> {
    let mut ctx = Ctx {
        engine,
        readable,
        rows,
        shapes,
        lang,
        labels: Vec::new(),
        cache: Default::default(),
    };
    let subject: NamedOrBlankNodeRef<'_> = iri.as_ref().into();
    // The resource's own triples, by property, in store order.
    // At most MAX_VALUES values are kept per property, and the others counted; at most
    // MAX_OUTGOING_SCAN triples are read.
    let mut outgoing: Vec<(NamedNode, Vec<Term>)> = Vec::new();
    let mut distinct: Vec<HashSet<Term>> = Vec::new();
    let mut index: HashMap<NamedNode, usize> = HashMap::new();
    let mut outgoing_truncated = false;
    for (read, q) in ctx.quads(Some(subject), None, None).enumerate() {
        if read == MAX_OUTGOING_SCAN {
            outgoing_truncated = true;
            break;
        }
        let i = *index.entry(q.predicate.clone()).or_insert_with(|| {
            outgoing.push((q.predicate.clone(), Vec::new()));
            distinct.push(HashSet::new());
            outgoing.len() - 1
        });
        if distinct[i].insert(q.object.clone()) && outgoing[i].1.len() < MAX_VALUES {
            outgoing[i].1.push(q.object);
        }
    }
    let count = |i: usize| distinct[i].len();
    let mut incoming_quads: Vec<Quad> = ctx
        .quads(None, None, Some(iri.as_ref().into()))
        .take(MAX_INCOMING_SCAN + 1)
        .collect();
    let incoming_truncated = incoming_quads.len() > MAX_INCOMING_SCAN;
    incoming_quads.truncate(MAX_INCOMING_SCAN);
    if outgoing.is_empty() && incoming_quads.is_empty() {
        return Ok(None);
    }
    let types: Vec<NamedNode> = index
        .get(&rdf::TYPE.into_owned())
        .map(|&i| &outgoing[i].1)
        .into_iter()
        .flatten()
        .filter_map(|t| match t {
            Term::NamedNode(n) => Some(n.clone()),
            _ => None,
        })
        .collect();
    let classes = with_superclasses(&ctx, &types);
    let nodes = node_shapes(&ctx, &classes);
    // The label paths of the shapes, then the usual label properties.
    let mut labels = Vec::new();
    for n in &nodes {
        for p in shapes.objects_for_subject_predicate(n.as_ref(), &sh("property")) {
            let p: NamedOrBlankNodeRef<'_> = match p {
                TermRef::NamedNode(n) => n.into(),
                TermRef::BlankNode(b) => b.into(),
                _ => continue,
            };
            let title = shapes.contains(tripletrail::oxigraph::model::TripleRef::new(
                p,
                &dash("propertyRole"),
                &dash("LabelRole"),
            ));
            if let (true, Some(TermRef::NamedNode(l))) =
                (title, shapes.object_for_subject_predicate(p, &sh("path")))
            {
                let l = l.into_owned();
                if !labels.contains(&l) {
                    labels.push(l);
                }
            }
        }
        for p in shapes.objects_for_subject_predicate(n.as_ref(), &tts("labelPath")) {
            if let TermRef::NamedNode(p) = p {
                labels.push(p.into_owned());
            }
        }
    }
    for l in LABELS {
        let l = NamedNode::new_unchecked(l);
        if !labels.contains(&l) {
            labels.push(l);
        }
    }
    ctx.labels = labels;

    let mut groups: Vec<Group> = Vec::new();
    let mut badges: Vec<Row> = Vec::new();

    // The rows of the property shapes.
    let mut used: HashSet<(NamedNode, bool)> = HashSet::new();
    used.insert((rdf::TYPE.into_owned(), false));
    // The parameters the data marks as not applicable or not yet available
    // (`era:notApplicable`, `era:notYetAvailable`, rinf-describe's `rd:DataStatusViewer`):
    // one row of their own, not two rows of links.
    let status: Vec<(String, Vec<Value>, usize)> = DATA_STATUS
        .iter()
        .filter_map(|(k, _)| {
            let p = NamedNode::new_unchecked(format!("{ERA}{k}"));
            let i = outgoing.iter().position(|(q, _)| *q == p)?;
            used.insert((p, false));
            let terms = &outgoing[i].1;
            Some((
                (*k).to_owned(),
                terms
                    .iter()
                    .take(MAX_VALUES)
                    .map(|t| ctx.value(t, 0))
                    .collect(),
                count(i),
            ))
        })
        .collect();
    if !status.is_empty() {
        let more = status
            .iter()
            .map(|(_, v, n)| n.saturating_sub(v.len()))
            .sum();
        let row = Row {
            property: NamedNode::new_unchecked(format!("{ERA}{}", status[0].0)),
            inverse: false,
            label: "Data status".to_owned(),
            viewer: "WidgetViewer",
            values: Vec::new(),
            more,
            order: f64::MAX,
            path: None,
            unit: None,
            aggregate: None,
            key_info: false,
            truncated: false,
            table: None,
            widget: Some("DataStatusViewer".to_owned()),
            // The count of each, past the values listed.
            settings: status
                .iter()
                .map(|(k, _, n)| (k.clone(), n.to_string()))
                .collect(),
            params: status.into_iter().map(|(k, v, _)| (k, v)).collect(),
            marks: Vec::new(),
            scale: None,
            figures: Vec::new(),
            figure_keys: Vec::new(),
            refs: Vec::new(),
            key: Some("data-status".to_owned()),
            shown: false,
        };
        let g = group_of(&mut groups, &ctx, Some(tts("Group-DataStatus")), f64::MAX);
        groups[g].rows.push(row);
    }

    let mut keys = HashSet::new();
    // The rows widgets draw from (`rd:figures`, `rd:table`, `rd:markers`, `rd:posts`): read
    // even when hidden, and kept for the widget, by key.
    let sh_property = sh("property");
    let figure_keys: HashSet<String> = nodes
        .iter()
        .flat_map(|n| shapes.objects_for_subject_predicate(n.as_ref(), &sh_property))
        .filter_map(|t| {
            let p: NamedOrBlankNodeRef<'_> = match t {
                TermRef::NamedNode(n) => n.into(),
                TermRef::BlankNode(b) => b.into(),
                _ => return None,
            };
            let mut lists: Vec<String> = std::iter::once(&"figures")
                .chain(REF_SETTINGS.iter())
                .filter_map(|k| ctx.shape_text(p, &rd(k)))
                .collect();
            // What is positioned on an element is drawn over the page's `length` row.
            if shapes
                .object_for_subject_predicate(p, &dash("viewer"))
                .is_some_and(|v| v == TermRef::from(rd("PositionedViewer").as_ref()))
            {
                lists.push("length".to_owned());
            }
            Some(lists.join(" "))
        })
        .flat_map(|l| {
            l.split_whitespace()
                .take(MAX_COLUMNS)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect();
    let mut figures: HashMap<String, Row> = HashMap::new();
    // Properties a shape hides: in no row, and not on the map.
    let mut hidden: HashSet<NamedNode> = HashSet::new();
    // The dataset's own property shapes of every node shape first, then the generated ones
    // (which a projection can also put under an own node shape, from a superclass).
    let generated = |p: &NamedOrBlankNode| p.to_string().starts_with("<urn:tripletrail:shape:");
    for (node, generated_pass) in [false, true]
        .into_iter()
        .flat_map(|pass| nodes.iter().map(move |n| (n, pass)))
    {
        let props: Vec<NamedOrBlankNode> = shapes
            .objects_for_subject_predicate(node.as_ref(), &sh("property"))
            .filter_map(|t| match t {
                TermRef::NamedNode(n) => Some(n.into_owned().into()),
                TermRef::BlankNode(b) => Some(b.into_owned().into()),
                _ => None,
            })
            .filter(|p| generated(p) == generated_pass)
            .collect();
        // A row that turns a key off first (before an inherited row of that key), then by
        // sh:order, then by what the shape says; each key is made once.
        let mut keyed: Vec<(bool, f64, String, NamedOrBlankNode)> = props
            .into_iter()
            .map(|p| {
                (
                    !ctx.shape_true(p.as_ref(), &sh("deactivated")),
                    ctx.shape_number(p.as_ref(), &sh("order"))
                        .unwrap_or(f64::MAX),
                    shape_text_key(shapes, p.as_ref()),
                    p,
                )
            })
            .collect();
        keyed.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.total_cmp(&b.1))
                .then_with(|| a.2.cmp(&b.2))
        });
        let props = keyed.into_iter().map(|(_, _, _, p)| p);
        for p in props {
            let p = p.as_ref();
            let path = shapes
                .object_for_subject_predicate(p, &sh("path"))
                .and_then(|t| ShPath::read(shapes, t));
            let over = shapes
                .object_for_subject_predicate(p, &rd("over"))
                .and_then(|t| ShPath::read(shapes, t));
            let kind = ctx
                .shape_text(p, &rd("aggregate"))
                .unwrap_or_else(|| "distinct-count".to_owned());
            // Without a key, a row is its path (and for a summary, what it sums up and how).
            let explicit = [tts("key"), rd("key")].iter().find_map(|k| {
                match shapes.object_for_subject_predicate(p, k) {
                    Some(TermRef::Literal(l)) => Some(l.value().to_owned()),
                    _ => None,
                }
            });
            // A row of the dataset's own shapes without a key is also known by its name, as
            // rinf-describe keys it (`"Part of"` is `part-of`), so a superclass's row of that
            // key is not repeated under a subclass's own.
            let alias = if explicit.is_none() && !generated_pass {
                ctx.shape_text(p, &sh("name"))
                    .map(|n| tripletrail::shapes::name_key(&n))
            } else {
                None
            };
            let key = explicit.or_else(|| {
                // A widget without a path: by its name (rinf-describe's default key).
                let Some(path) = path.as_ref() else {
                    return shapes
                        .object_for_subject_predicate(p, &dash("viewer"))
                        .and_then(|_| ctx.shape_text(p, &sh("name")));
                };
                let path = path.sparql(false);
                Some(match &over {
                    Some(o) => format!("{path} over {} {kind}", o.sparql(false)),
                    None => path,
                })
            });
            // A row of a key a more specific shape gave (or deactivated) is not repeated.
            let Some(key) = key else { continue };
            if alias.as_ref().is_some_and(|a| keys.contains(a)) || !keys.insert(key.clone()) {
                continue;
            }
            if let Some(a) = alias {
                keys.insert(a);
            }
            // A hidden row a key figures widget shows: read, then kept for the widget.
            let figure = figure_keys.contains(key.as_str())
                && !ctx.shape_true(p, &sh("deactivated"))
                && (ctx.shape_true(p, &dash("hidden")) || ctx.shape_true(p, &rd("hidden")));
            // The widget of the row (a rinf-describe `rd:` viewer) and the paths it reads.
            let widget = match shapes.object_for_subject_predicate(p, &dash("viewer")) {
                Some(TermRef::NamedNode(v)) => v.as_str().strip_prefix(RD).map(str::to_owned),
                _ => None,
            };
            let params = if widget.is_some() {
                widget_params(&ctx, p, iri)
            } else {
                Vec::new()
            };
            let (marks, scale) = if widget.is_some() {
                scale_marks(&ctx, p)
            } else {
                (Vec::new(), None)
            };
            let settings = if widget.is_some() {
                widget_settings(&ctx, p)
            } else {
                Vec::new()
            };
            let Some(path) = path else {
                // A widget that reads no path of its own: a row when its paths have values.
                let off = ctx.shape_true(p, &sh("deactivated"))
                    || ctx.shape_true(p, &dash("hidden"))
                    || ctx.shape_true(p, &rd("hidden"));
                // The facilities of a facilities widget have no rows of their own.
                if widget.as_deref() == Some("FacilitiesViewer") && !off {
                    for t in shapes.objects_for_subject_predicate(p, &rd("facility")) {
                        if let TermRef::NamedNode(n) = t {
                            used.insert((n.into_owned(), false));
                        }
                    }
                }
                // A widget that draws other rows of the page is kept until those are read.
                let shows_figures = widget.as_deref() == Some("KeyFiguresViewer")
                    || settings
                        .iter()
                        .any(|(k, _)| REF_SETTINGS.contains(&k.as_str()));
                // The RINF parameters: every parameter of the resource, as a table.
                let parameters = if widget.as_deref() == Some("ParameterIndexViewer") && !off {
                    parameter_index(
                        &ctx,
                        iri,
                        &outgoing,
                        settings
                            .iter()
                            .find(|(k, _)| k == "indexRoot")
                            .map(|(_, v)| v.as_str()),
                    )
                } else {
                    None
                };
                if off
                    || (parameters.is_none()
                        && !shows_figures
                        && params.iter().all(|(_, v)| v.is_empty()))
                {
                    continue;
                }
                let (values, table) = match parameters {
                    Some((values, table)) => (values, Some(table)),
                    None => (Vec::new(), None),
                };
                let group = match shapes.object_for_subject_predicate(p, &sh("group")) {
                    Some(TermRef::NamedNode(g)) => Some(g.into_owned()),
                    _ => None,
                };
                let name = widget.clone().unwrap_or_else(|| "Widget".to_owned());
                let row = Row {
                    property: rd(&name),
                    inverse: false,
                    path: None,
                    label: ctx.shape_text(p, &sh("name")).unwrap_or(name),
                    viewer: "WidgetViewer",
                    values,
                    more: 0,
                    unit: ctx.shape_text(p, &rd("unit")),
                    aggregate: None,
                    key_info: false,
                    truncated: table.as_ref().is_some_and(|t| t.truncated),
                    table,
                    widget,
                    params,
                    marks,
                    scale,
                    figures: Vec::new(),
                    settings,
                    refs: Vec::new(),
                    figure_keys: ctx
                        .shape_text(p, &rd("figures"))
                        .map(|l| {
                            l.split_whitespace()
                                .take(MAX_COLUMNS)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default(),
                    order: ctx.shape_number(p, &sh("order")).unwrap_or(f64::MAX),
                    key: Some(key.clone()),
                    shown: false,
                };
                if figure_keys.contains(key.as_str()) {
                    figures.insert(
                        key,
                        Row {
                            shown: true,
                            ..row.clone()
                        },
                    );
                }
                let g = group_of(&mut groups, &ctx, group, f64::MAX);
                groups[g].rows.push(row);
                continue;
            };
            // A property or the inverse of one is read from the triples above; another path
            // is followed from the resource.
            let simple = match &path {
                ShPath::Property(n, inverse) => Some((n.clone(), *inverse)),
                _ => None,
            };
            let property = path.first().clone();
            let inverse = simple.as_ref().is_some_and(|(_, i)| *i);
            let role = |r: &str| {
                shapes.contains(tripletrail::oxigraph::model::TripleRef::new(
                    p,
                    &dash("propertyRole"),
                    &dash(r),
                ))
            };
            let group = match shapes.object_for_subject_predicate(p, &sh("group")) {
                Some(TermRef::NamedNode(g)) => Some(g.into_owned()),
                _ => None,
            };
            let key_info = role("KeyInfoRole");
            // A row of its own for the property: not a summary, nor a badge only.
            let claims = over.is_none() && !(key_info && group.is_none());
            // Rows of one property for different classes (`rd:class`: the tracks, the sidings)
            // each keep their own.
            let of_one_class = shapes
                .object_for_subject_predicate(p, &rd("class"))
                .is_some();
            // A generated row of a property a row of the dataset's own shapes shows is not
            // repeated; rows of the own shapes are told apart by their keys.
            if claims
                && generated_pass
                && !of_one_class
                && simple.as_ref().is_some_and(|s| used.contains(s))
            {
                continue;
            }
            if let Some(s) = simple.as_ref().filter(|_| claims) {
                used.insert(s.clone());
            }
            if !figure
                && (ctx.shape_true(p, &dash("hidden"))
                    || ctx.shape_true(p, &sh("deactivated"))
                    || ctx.shape_true(p, &rd("hidden"))
                    || role("LabelRole"))
            {
                if simple.is_some() && !inverse && claims {
                    hidden.insert(property.clone());
                }
                continue;
            }
            let class = match shapes.object_for_subject_predicate(p, &rd("class")) {
                Some(TermRef::NamedNode(c)) => Some(c.into_owned()),
                _ => None,
            };
            let of_class = |t: &Term| {
                let Some(c) = &class else { return true };
                let s: NamedOrBlankNodeRef<'_> = match t {
                    Term::NamedNode(n) => n.as_ref().into(),
                    Term::BlankNode(b) => b.as_ref().into(),
                    _ => return false,
                };
                ctx.quads(Some(s), Some(rdf::TYPE), Some(c.as_ref().into()))
                    .next()
                    .is_some()
            };
            let unit = ctx.shape_text(p, &rd("unit"));
            let mut aggregate = None;
            let mut truncated = false;
            let (terms, total): (Vec<Term>, usize) = if let Some(over) = &over {
                // A summary of the values of the path over the linked resources: the links and
                // the values are read within a budget each, and a summary of what was read
                // says so.
                let mut budget = MAX_PATH_SCAN;
                let linked: Vec<Term> = over
                    .follow(&ctx, &[Term::from(iri.clone())], &mut budget)
                    .into_iter()
                    .filter(|t| of_class(t))
                    .collect();
                let mut values_budget = MAX_PATH_SCAN;
                let text = summary(&ctx, &kind, &path, &linked, &mut values_budget);
                truncated = budget == 0 || values_budget == 0;
                match text {
                    Some(text) => {
                        aggregate = Some(kind.clone());
                        (vec![Literal::new_simple_literal(text).into()], 1)
                    }
                    None => (Vec::new(), 0),
                }
            } else if simple.is_none() {
                let mut budget = MAX_PATH_SCAN;
                let all: Vec<Term> = path
                    .follow(&ctx, &[Term::from(iri.clone())], &mut budget)
                    .into_iter()
                    .filter(|t| of_class(t))
                    .collect();
                truncated = budget == 0;
                let total = all.len();
                (all.into_iter().take(MAX_VALUES).collect(), total)
            } else if inverse {
                let mut seen = HashSet::new();
                let terms = incoming_quads
                    .iter()
                    .filter(|q| q.predicate == property)
                    .map(|q| Term::from(q.subject.clone()))
                    .filter(|s| seen.insert(s.clone()) && of_class(s))
                    .collect::<Vec<_>>();
                let total = terms.len();
                (terms, total)
            } else {
                let (terms, total) = index
                    .get(&property)
                    .map(|&i| (outgoing[i].1.clone(), count(i)))
                    .unwrap_or_default();
                if class.is_some() {
                    let terms: Vec<Term> = terms.into_iter().filter(|t| of_class(t)).collect();
                    let total = terms.len();
                    (terms, total)
                } else {
                    (terms, total)
                }
            };
            // A map row is kept when the context around the resource has geometries.
            if terms.is_empty()
                && (widget.as_deref() != Some("MapViewer")
                    || params.iter().all(|(k, v)| k != "context" || v.is_empty()))
            {
                continue;
            }
            let more = total.saturating_sub(terms.len().min(MAX_VALUES));
            let values: Vec<Value> = terms
                .iter()
                .take(MAX_VALUES)
                .map(|t| ctx.value(t, 0))
                .collect();
            let named = match shapes.object_for_subject_predicate(p, &dash("viewer")) {
                Some(TermRef::NamedNode(v)) => v.as_str().strip_prefix(DASH).map(str::to_owned),
                _ => None,
            };
            let label = ctx
                .shape_text(p, &sh("name"))
                .or_else(|| simple.is_some().then(|| ctx.label(&property)).flatten())
                .unwrap_or_else(|| match &simple {
                    Some(_) => local_name(property.as_str()).to_owned(),
                    None => path.sparql(true),
                });
            let row = Row {
                path: simple.is_none().then(|| path.sparql(false)),
                viewer: viewer(named.as_deref(), &values),
                order: ctx.shape_number(p, &sh("order")).unwrap_or(f64::MAX),
                property,
                inverse,
                label,
                values,
                more,
                unit,
                aggregate,
                key_info,
                truncated,
                widget,
                params,
                marks,
                scale,
                figures: Vec::new(),
                figure_keys: Vec::new(),
                settings,
                refs: Vec::new(),
                table: shapes
                    .object_for_subject_predicate(p, &sh("node"))
                    .and_then(|n| table(&ctx, n, &terms)),
                key: Some(key.clone()),
                shown: false,
            };
            if figure {
                figures.insert(key, row);
                continue;
            }
            if figure_keys.contains(key.as_str()) {
                figures.insert(
                    key,
                    Row {
                        shown: true,
                        ..row.clone()
                    },
                );
            }
            // A key figure outside any group is a badge only.
            match group {
                None if key_info => badges.push(row),
                group => {
                    if key_info {
                        badges.push(row.clone());
                    }
                    let g = group_of(&mut groups, &ctx, group, f64::MAX);
                    groups[g].rows.push(row);
                }
            }
        }
    }

    // The resource's other properties: attributes, then links, by namespace and property.
    let mut rest: Vec<usize> = (0..outgoing.len())
        .filter(|&i| !used.contains(&(outgoing[i].0.clone(), false)))
        .collect();
    rest.sort_by(|&a, &b| {
        let key = |i: usize| {
            let p = outgoing[i].0.as_str();
            (namespace(p), p)
        };
        key(a).cmp(&key(b))
    });
    for i in rest {
        let (property, terms) = &outgoing[i];
        let literals = terms.iter().all(|t| matches!(t, Term::Literal(_)));
        let group = if literals {
            "Group-Attributes"
        } else {
            "Group-Links"
        };
        let values: Vec<Value> = terms
            .iter()
            .take(MAX_VALUES)
            .map(|t| ctx.value(t, 0))
            .collect();
        let row = Row {
            property: property.clone(),
            inverse: false,
            label: ctx
                .label(property)
                .unwrap_or_else(|| local_name(property.as_str()).to_owned()),
            viewer: viewer(None, &values),
            values,
            more: count(i).saturating_sub(terms.len()),
            order: f64::MAX,
            path: None,
            unit: None,
            aggregate: None,
            key_info: false,
            truncated: false,
            table: None,
            widget: None,
            params: Vec::new(),
            marks: Vec::new(),
            scale: None,
            figures: Vec::new(),
            figure_keys: Vec::new(),
            settings: Vec::new(),
            refs: Vec::new(),
            key: None,
            shown: false,
        };
        let g = group_of(&mut groups, &ctx, Some(tts(group)), f64::MAX);
        groups[g].rows.push(row);
    }
    // Key figures widgets get their rows; one with none is not shown. Other widgets get the
    // rows they draw from.
    for g in &mut groups {
        for r in &mut g.rows {
            let mut refs: Vec<String> = r
                .settings
                .iter()
                .filter(|(k, _)| REF_SETTINGS.contains(&k.as_str()))
                .flat_map(|(_, v)| v.split_whitespace().map(str::to_owned))
                .collect();
            if r.widget.as_deref() == Some("PositionedViewer") {
                refs.push("length".to_owned());
            }
            r.refs = refs
                .into_iter()
                .filter_map(|k| Some((k.clone(), figures.get(&k)?.clone())))
                .collect();
            if r.widget.as_deref() != Some("KeyFiguresViewer") {
                continue;
            }
            r.figures = r
                .figure_keys
                .iter()
                .filter_map(|k| figures.get(k).cloned())
                .collect();
        }
        g.rows.retain(|r| {
            let draws_rows = r.path.is_none()
                && r.values.is_empty()
                && r.settings
                    .iter()
                    .any(|(k, _)| REF_SETTINGS.contains(&k.as_str()));
            (r.widget.as_deref() != Some("KeyFiguresViewer") || !r.figures.is_empty())
                && (!draws_rows
                    || !r.refs.is_empty()
                    || r.params.iter().any(|(_, v)| !v.is_empty()))
        });
    }
    groups.retain(|g| !g.rows.is_empty());
    for g in &mut groups {
        g.rows.sort_by(|a, b| a.order.total_cmp(&b.order));
    }
    groups.sort_by(|a, b| a.order.total_cmp(&b.order).then(a.label.cmp(&b.label)));
    // With shapes of the dataset's own, the generic groups are a tab after theirs.
    if nodes
        .iter()
        .any(|n| !n.to_string().starts_with("<urn:tripletrail:shape:"))
    {
        for g in &mut groups {
            let generic = g.iri.as_ref().is_some_and(|i| {
                matches!(
                    i.as_str().strip_prefix(TTS),
                    Some("Group-Attributes" | "Group-Links")
                )
            });
            if generic && g.tab.is_none() {
                g.tab = Some("Other properties".to_owned());
            }
        }
    }
    // The overview, then each tab where its first group is.
    let mut tabs: Vec<Option<String>> = vec![None];
    for g in &groups {
        if !tabs.contains(&g.tab) {
            tabs.push(g.tab.clone());
        }
    }
    groups.sort_by_key(|g| tabs.iter().position(|t| *t == g.tab));
    badges.sort_by(|a, b| a.order.total_cmp(&b.order));

    // The links to the resource no row shows.
    let mut incoming: Vec<Incoming> = Vec::new();
    for q in &incoming_quads {
        if used.contains(&(q.predicate.clone(), true)) {
            continue;
        }
        let i = match incoming.iter().position(|i| i.property.iri == q.predicate) {
            Some(i) => i,
            None => {
                incoming.push(Incoming {
                    property: ctx.link(&q.predicate),
                    subjects: Vec::new(),
                    count: 0,
                });
                incoming.len() - 1
            }
        };
        incoming[i].count += 1;
        if incoming[i].subjects.len() < MAX_INCOMING {
            incoming[i]
                .subjects
                .push(ctx.value(&q.subject.clone().into(), MAX_DEPTH));
        }
    }
    incoming.sort_by_key(|i| std::cmp::Reverse(i.count));

    // One more than shown, to know there are more; none of the hidden properties.
    let mut geometries = Vec::new();
    for (property, terms) in outgoing.iter().filter(|(p, _)| !hidden.contains(p)) {
        for t in terms {
            collect_geometries(&ctx, property, t, 0, &mut geometries);
        }
    }
    let geometries_truncated = geometries.len() > MAX_GEOMETRIES;
    geometries.truncate(MAX_GEOMETRIES);
    // The context of map rows (`rd:context`), around the resource's own geometries.
    let mut context = Vec::new();
    let context_property = rd("context");
    for r in groups.iter().flat_map(|g| &g.rows).chain(&badges) {
        for (_, values) in r.params.iter().filter(|(k, _)| k == "context") {
            for v in values {
                if let Value::Literal(l) = v {
                    collect_geometries(&ctx, &context_property, &l.clone().into(), 0, &mut context);
                }
            }
        }
    }
    context.truncate(MAX_GEOMETRIES);
    Ok(Some(View {
        geometries_truncated,
        geometries,
        context,
        label: ctx.label(iri),
        classes: types.iter().map(|t| ctx.link(t)).collect(),
        iri: iri.clone(),
        badges,
        groups,
        incoming,
        incoming_truncated,
        truncated: outgoing_truncated,
        own: !outgoing.is_empty(),
    }))
}

fn link_json(l: &Link) -> Json {
    json!({"iri": l.iri.as_str(), "label": l.label})
}

fn value_json(v: &Value) -> Json {
    match v {
        Value::Iri(l) => json!({"type": "uri", "value": l.iri.as_str(), "label": l.label}),
        Value::Literal(l) => match l.language() {
            Some(lang) => json!({"type": "literal", "value": l.value(), "xml:lang": lang}),
            None => {
                json!({"type": "literal", "value": l.value(), "datatype": l.datatype().as_str()})
            }
        },
        Value::Blank(rows) => json!({
            "type": "bnode",
            "rows": rows.iter().map(|(p, v)| json!({"property": link_json(p), "value": value_json(v)})).collect::<Vec<_>>(),
        }),
    }
}

/// The view model as JSON, for a page that draws it itself.
fn row_json(r: &Row) -> Json {
    json!({
        "property": r.property.as_str(),
        "inverse": r.inverse,
        "key": r.key,
        "path": r.path,
        "label": r.label,
        "viewer": r.viewer,
        "values": r.values.iter().map(value_json).collect::<Vec<_>>(),
        "more": r.more,
        "unit": r.unit,
        "aggregate": r.aggregate,
        "keyInfo": r.key_info,
        "truncated": r.truncated,
        "columns": r.table.as_ref().map(|t| t.columns.iter().map(|c| json!({
            "key": c.key,
            "label": c.label,
            "unit": c.unit,
            "hidden": c.hidden,
        })).collect::<Vec<_>>()),
        "widget": r.widget,
        "params": r.params.iter().map(|(k, v)| {
            (k.clone(), Json::Array(v.iter().map(value_json).collect()))
        }).collect::<serde_json::Map<String, Json>>(),
        "marks": r.marks.iter().map(|(v, l)| json!({"value": v, "label": l})).collect::<Vec<_>>(),
        "scale": r.scale.map(|(a, b)| [a, b]),
        "figures": r.figures.iter().map(row_json).collect::<Vec<_>>(),
        // A row the page also shows is named by its key, not repeated.
        "rows": r.refs.iter().map(|(k, row)| (k.clone(), if row.shown {
            json!({"key": k, "shown": true})
        } else {
            row_json(row)
        })).collect::<serde_json::Map<String, Json>>(),
        "settings": r.settings.iter().map(|(k, v)| (k.clone(), Json::from(v.as_str()))).collect::<serde_json::Map<String, Json>>(),
        "tableTruncated": r.table.as_ref().is_some_and(|t| t.truncated),
        "cells": r.table.as_ref().map(|t| t.cells.iter().map(|row| row.iter().map(|cell| {
            cell.iter().map(value_json).collect::<Vec<_>>()
        }).collect::<Vec<_>>()).collect::<Vec<_>>()),
    })
}

pub(crate) fn to_json(view: &View) -> Json {
    json!({
        "iri": view.iri.as_str(),
        "label": view.label,
        "classes": view.classes.iter().map(link_json).collect::<Vec<_>>(),
        "badges": view.badges.iter().map(row_json).collect::<Vec<_>>(),
        "groups": view.groups.iter().map(|g| json!({
            "group": g.iri.as_ref().map(NamedNode::as_str),
            "label": g.label,
            "tab": g.tab,
            "rows": g.rows.iter().map(row_json).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "incoming": view.incoming.iter().map(|i| json!({
            "property": link_json(&i.property),
            "count": i.count,
            "subjects": i.subjects.iter().map(value_json).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "incomingTruncated": view.incoming_truncated,
        "truncated": view.truncated,
        "geometriesTruncated": view.geometries_truncated,
        "geometries": view.geometries.iter().map(|g| json!({
            "value": g.literal.value(),
            "datatype": g.literal.datatype().as_str(),
        })).collect::<Vec<_>>(),
        "context": view.context.iter().map(|g| json!({
            "value": g.literal.value(),
            "datatype": g.literal.datatype().as_str(),
        })).collect::<Vec<_>>(),
    })
}

pub(crate) fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// How the page links to things: a resource's own page, and the documents of this one.
pub(crate) struct PageLinks<'a> {
    pub(crate) href: &'a dyn Fn(&str) -> String,
    /// (format name, URL) of the RDF documents of the resource.
    pub(crate) documents: Vec<(&'static str, String)>,
    /// Whether the map is made interactive with MapLibre from the web UI (`/$/ui/`).
    pub(crate) live_map: bool,
    /// The raster tiles under it and their attribution (`--map-tiles`); none: no base map.
    pub(crate) basemap: Option<(&'a str, &'a str)>,
}

fn html_link(out: &mut String, links: &PageLinks<'_>, l: &Link) {
    let text = l.label.as_deref().unwrap_or_else(|| l.iri.as_str());
    let _ = write!(
        out,
        "<a href=\"{}\" title=\"{}\">{}</a>",
        escape(&(links.href)(l.iri.as_str())),
        escape(l.iri.as_str()),
        escape(text)
    );
}

fn html_value(out: &mut String, links: &PageLinks<'_>, v: &Value, viewer: &str) {
    match v {
        Value::Iri(l) => match viewer {
            // A link to the IRI itself only for http(s): not a `javascript:` IRI of the data.
            "URIViewer" | "HyperlinkViewer" if web(l.iri.as_str()) => {
                let _ = write!(out, "<a href=\"{0}\">{0}</a>", escape(l.iri.as_str()));
            }
            "ImageViewer" if web(l.iri.as_str()) => {
                let _ = write!(
                    out,
                    "<img src=\"{0}\" alt=\"{0}\" loading=\"lazy\">",
                    escape(l.iri.as_str())
                );
            }
            _ => html_link(out, links, l),
        },
        Value::Literal(l) => {
            let _ = write!(out, "<span class=\"lit\">{}</span>", escape(l.value()));
            if let Some(lang) = l.language() {
                let _ = write!(out, " <small class=\"dt\">@{}</small>", escape(lang));
            } else if l.datatype() != xsd::STRING {
                let _ = write!(
                    out,
                    " <small class=\"dt\" title=\"{}\">{}</small>",
                    escape(l.datatype().as_str()),
                    escape(local_name(l.datatype().as_str()))
                );
            }
        }
        Value::Blank(rows) => {
            out.push_str("<table class=\"nested\">");
            for (p, v) in rows {
                out.push_str("<tr><th>");
                html_link(out, links, p);
                out.push_str("</th><td>");
                html_value(out, links, v, viewer_for(v));
                out.push_str("</td></tr>");
            }
            out.push_str("</table>");
        }
    }
}

/// What a widget reads: a validity as "from … to …", other widgets as their named values.
fn html_params(out: &mut String, links: &PageLinks<'_>, r: &Row) {
    if r.params.iter().all(|(_, v)| v.is_empty()) {
        return;
    }
    let get = |k: &str| r.params.iter().find(|(n, _)| n == k).map(|(_, v)| v);
    let list = |out: &mut String, values: &[Value]| {
        for (i, v) in values.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            html_value(out, links, v, "LabelViewer");
        }
    };
    if r.widget.as_deref() == Some("ValidityViewer") {
        out.push_str("<span class=\"validity\">");
        if let Some(b) = get("begin").filter(|b| !b.is_empty()) {
            out.push_str("from ");
            list(out, b);
        }
        if let Some(e) = get("end").filter(|e| !e.is_empty()) {
            out.push_str(" until ");
            list(out, e);
        }
        out.push_str("</span>");
        return;
    }
    out.push_str("<table class=\"nested\">");
    for (name, values) in r.params.iter().filter(|(_, v)| !v.is_empty()) {
        let _ = write!(out, "<tr><th>{}</th><td>", escape(name));
        list(out, values);
        out.push_str("</td></tr>");
    }
    out.push_str("</table>");
}

/// The values of a row as a table: each value, then its cells.
fn html_table(out: &mut String, links: &PageLinks<'_>, values: &[Value], t: &Table) {
    out.push_str("<table class=\"nested values\"><tr><th></th>");
    for c in t.columns.iter().filter(|c| !c.hidden) {
        let _ = write!(out, "<th>{}", escape(&c.label));
        if let Some(u) = &c.unit {
            let _ = write!(out, " <span class=\"unit\">({})</span>", escape(u));
        }
        out.push_str("</th>");
    }
    out.push_str("</tr>");
    for (v, cells) in values.iter().zip(&t.cells) {
        out.push_str("<tr><td>");
        html_value(out, links, v, "LabelViewer");
        out.push_str("</td>");
        for (cell, _) in cells.iter().zip(&t.columns).filter(|(_, c)| !c.hidden) {
            out.push_str("<td>");
            for (i, c) in cell.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                html_value(out, links, c, "LabelViewer");
            }
            out.push_str("</td>");
        }
        out.push_str("</tr>");
    }
    out.push_str("</table>");
}

/// The cell of a table row by column key.
fn cell<'r>(r: &'r Row, i: usize, key: &str) -> &'r [Value] {
    let Some(t) = &r.table else { return &[] };
    t.columns
        .iter()
        .position(|c| c.key == key)
        .and_then(|c| t.cells.get(i)?.get(c))
        .map_or(&[], Vec::as_slice)
}

/// The text of a value: a literal's lexical form, or an IRI.
fn text_of(v: &Value) -> Option<&str> {
    match v {
        Value::Literal(l) => Some(l.value()),
        Value::Iri(l) => Some(l.iri.as_str()),
        Value::Blank(_) => None,
    }
}

fn number_of(v: &Value) -> Option<f64> {
    match v {
        Value::Literal(l) => l
            .value()
            .trim()
            .parse()
            .ok()
            .filter(|n: &f64| n.is_finite()),
        _ => None,
    }
}

/// A number a drawing places things by (a km, an offset, a length): finite and below 10^9,
/// so that scaling it stays finite.
fn measure_of(v: &Value) -> Option<f64> {
    number_of(v).filter(|n| n.abs() < 1e9)
}

/// The name of a value to show and sort by: its label, else the end of its IRI.
fn name_of(v: &Value) -> String {
    match v {
        Value::Iri(l) => l
            .label
            .clone()
            .unwrap_or_else(|| local_name(l.iri.as_str()).to_owned()),
        Value::Literal(l) => l.value().to_owned(),
        Value::Blank(_) => String::new(),
    }
}

/// The file type of an address: the extension of its last segment ("PDF"), else "LINK".
fn file_type(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let last = path.rsplit('/').next().unwrap_or(path);
    match last.rsplit_once('.') {
        Some((stem, ext))
            if !stem.is_empty()
                && (1..=5).contains(&ext.len())
                && ext.bytes().all(|b| b.is_ascii_alphanumeric()) =>
        {
            ext.to_ascii_uppercase()
        }
        _ => "LINK".to_owned(),
    }
}

/// `rd:DocumentViewer`: each document as a card with its file type, named by its "name"
/// cell and opening its "url" cell (`sh:node` columns).
fn html_documents(out: &mut String, links: &PageLinks<'_>, r: &Row) -> bool {
    if r.table.is_none() || r.values.is_empty() {
        return false;
    }
    out.push_str("<div class=\"doc-cards\">");
    for (i, v) in r.values.iter().enumerate() {
        let url = cell(r, i, "url")
            .iter()
            .filter_map(text_of)
            .find(|u| web(u));
        let name = cell(r, i, "name")
            .iter()
            .find_map(|n| match n {
                Value::Literal(l) => Some(l.value().to_owned()),
                _ => None,
            })
            .unwrap_or_else(|| name_of(v));
        out.push_str("<div class=\"doc-card\">");
        match url {
            Some(u) => {
                let _ = write!(
                    out,
                    "<span class=\"ftype\">{}</span><a href=\"{}\" rel=\"noopener\">{}</a>",
                    escape(&file_type(u)),
                    escape(u),
                    escape(if name.is_empty() { u } else { &name })
                );
                if let Value::Iri(l) = v {
                    out.push_str(" <span class=\"doc-of\">");
                    html_link(
                        out,
                        links,
                        &Link {
                            iri: l.iri.clone(),
                            label: Some("details".to_owned()),
                        },
                    );
                    out.push_str("</span>");
                }
            }
            None => html_value(out, links, v, "LabelViewer"),
        }
        out.push_str("</div>");
    }
    out.push_str("</div>");
    true
}

/// A number for a page: plain, or in exponent form when very large or very small.
fn number_text(n: f64) -> String {
    let a = n.abs();
    if a != 0.0 && !(1e-6..1e15).contains(&a) {
        format!("{n:e}")
    } else {
        n.to_string()
    }
}

/// `rd:ScaleBarViewer`: a length as a bar against its marks, or the range `min` to `max`
/// on `rd:scale`.
fn html_scale_bar(out: &mut String, r: &Row) -> bool {
    let unit = r
        .unit
        .as_deref()
        .map(|u| format!(" {}", escape(u)))
        .unwrap_or_default();
    let param = |k: &str| {
        r.params
            .iter()
            .find(|(n, _)| n == k)
            .and_then(|(_, v)| v.iter().find_map(number_of))
    };
    if let (Some((lo, hi)), Some(min), Some(max)) = (r.scale, param("min"), param("max")) {
        let at = |n: f64| ((n - lo) / (hi - lo) * 100.0).clamp(0.0, 100.0);
        let _ = write!(
            out,
            "<div class=\"scalebar\"><div class=\"track\"><div class=\"fill\" \
             style=\"left:{:.1}%;width:{:.1}%\"></div></div><div class=\"scale-ends\">\
             <span>{}</span><span>{}</span></div><div>{} to {}{unit}</div></div>",
            at(min.min(max)),
            (at(max.max(min)) - at(min.min(max))).max(0.5),
            number_text(lo),
            number_text(hi),
            number_text(min),
            number_text(max),
        );
        return true;
    }
    let Some(value) = r.values.iter().find_map(number_of) else {
        return false;
    };
    let top = r.marks.iter().map(|m| m.0).fold(value, f64::max) * 1.1;
    if top <= 0.0 {
        return false;
    }
    let at = |n: f64| (n / top * 100.0).clamp(0.0, 100.0);
    let _ = write!(
        out,
        "<div class=\"scalebar\"><div class=\"track\"><div class=\"fill\" style=\"width:{:.1}%\"></div>",
        at(value)
    );
    for (n, label) in &r.marks {
        let _ = write!(
            out,
            "<span class=\"mark\" style=\"left:{:.1}%\" title=\"{} {}\"></span>",
            at(*n),
            number_text(*n),
            escape(label)
        );
    }
    let _ = write!(
        out,
        "</div><div><strong>{}</strong>{unit}</div>",
        number_text(value)
    );
    if !r.marks.is_empty() {
        out.push_str("<div class=\"marks\">");
        for (n, label) in &r.marks {
            let _ = write!(
                out,
                "<span class=\"mark-label{}\">{} {}{unit}</span>",
                if value >= *n { " reached" } else { "" },
                escape(label),
                number_text(*n),
            );
        }
        out.push_str("</div>");
    }
    out.push_str("</div>");
    true
}

struct Tree<'t> {
    nodes: &'t [(usize, &'t Link)],
    children: &'t [Vec<usize>],
    notations: &'t [Option<String>],
    own: &'t NamedNode,
}

impl Tree<'_> {
    fn draw(
        &self,
        out: &mut String,
        links: &PageLinks<'_>,
        n: usize,
        depth: usize,
        seen: &mut [bool],
    ) {
        seen[n] = true;
        out.push_str("<li>");
        if let Some(code) = &self.notations[n] {
            let _ = write!(out, "<span class=\"notation\">{}</span> ", escape(code));
        }
        let me = self.nodes[n].1.iri == *self.own;
        if me {
            out.push_str("<strong>");
        }
        html_link(out, links, self.nodes[n].1);
        if me {
            out.push_str("</strong>");
        }
        if depth < MAX_TREE_DEPTH && self.children[n].iter().any(|&c| !seen[c]) {
            out.push_str("<ul>");
            for &c in &self.children[n] {
                if !seen[c] {
                    self.draw(out, links, c, depth + 1, seen);
                }
            }
            out.push_str("</ul>");
        }
        out.push_str("</li>");
    }
}

/// `rd:FacilitiesViewer`: each boolean property of `rd:facility` as a tile: yes, no, or not
/// given; another value is shown as it is.
fn html_facilities(out: &mut String, links: &PageLinks<'_>, r: &Row) -> bool {
    if r.params.is_empty() {
        return false;
    }
    out.push_str("<div class=\"facilities\">");
    for (name, values) in &r.params {
        let flag = match values.as_slice() {
            [Value::Literal(l)] => match l.value() {
                "true" | "1" => Some(true),
                "false" | "0" => Some(false),
                _ => None,
            },
            _ => None,
        };
        let (class, mark) = match (flag, values.is_empty()) {
            (Some(true), _) => ("yes", "\u{2713}"),
            (Some(false), _) => ("no", "\u{2717}"),
            (None, true) => ("unknown", "?"),
            (None, false) => ("other", "\u{2022}"),
        };
        let _ = write!(
            out,
            "<span class=\"facility {class}\"><span class=\"flag\">{mark}</span> {}",
            escape(name)
        );
        if flag.is_none() && !values.is_empty() {
            out.push_str(": ");
            for (i, v) in values.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                html_value(out, links, v, "LabelViewer");
            }
        }
        out.push_str("</span>");
    }
    out.push_str("</div>");
    true
}

/// `rd:KeyFiguresViewer`: the summary rows named in `rd:figures` as tiles.
fn html_key_figures(out: &mut String, links: &PageLinks<'_>, r: &Row) -> bool {
    if r.figures.is_empty() {
        return false;
    }
    out.push_str("<div class=\"figures\">");
    for f in &r.figures {
        out.push_str("<div class=\"figure\"><div class=\"figure-value\">");
        for (i, v) in f.values.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            html_value(out, links, v, "LabelViewer");
        }
        if let Some(u) = &f.unit {
            let _ = write!(out, " <span class=\"unit\">{}</span>", escape(u));
        }
        if f.truncated {
            out.push_str("<span title=\"Of the first linked resources only\">+</span>");
        }
        let _ = write!(
            out,
            "</div><div class=\"figure-label\">{}</div></div>",
            escape(&f.label)
        );
    }
    out.push_str("</div>");
    true
}

/// A railway location, `km 53+240`: kilometres and metres after them.
fn km_text(km: f64, offset: f64) -> Option<String> {
    let total = km * 1000.0 + offset;
    if !total.is_finite() || total.abs() >= 1e12 {
        return None;
    }
    let metres = total.abs().round() as u64;
    Some(format!(
        "km {}{}+{:03}",
        if total < 0.0 && metres > 0 { "-" } else { "" },
        metres / 1000,
        metres % 1000
    ))
}

/// `rd:LrsLocationViewer`: the values with their railway location from the `km` and
/// `offset` paths (`km 53+240`).
fn html_lrs_location(out: &mut String, links: &PageLinks<'_>, r: &Row) -> bool {
    let param = |k: &str| {
        r.params
            .iter()
            .find(|(n, _)| n == k)
            .and_then(|(_, v)| v.iter().find_map(number_of))
    };
    let Some(text) = param("km").and_then(|km| km_text(km, param("offset").unwrap_or(0.0))) else {
        return false;
    };
    for (i, v) in r.values.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        html_value(out, links, v, r.viewer);
    }
    let _ = write!(
        out,
        "{}<span class=\"km\">{}</span>",
        if r.values.is_empty() { "" } else { " " },
        escape(&text)
    );
    true
}

/// The row a widget draws from by its `rd:table` key.
fn table_ref(r: &Row) -> Option<&Row> {
    let key = r
        .settings
        .iter()
        .find(|(k, _)| k == "table")?
        .1
        .split_whitespace()
        .next()?;
    r.refs.iter().find(|(k, _)| k == key).map(|(_, row)| row)
}

/// `rd:LineStopsViewer`: the operational points at the ends of the sections of the
/// `rd:table` row (columns from, to, start-km, start-offset, end-km, end-offset), in km order,
/// each once.
fn html_line_stops(out: &mut String, links: &PageLinks<'_>, r: &Row) -> bool {
    let Some(sections) = table_ref(r) else {
        return false;
    };
    let mut stops: Vec<(f64, &Value)> = Vec::new();
    let mut seen = HashSet::new();
    for i in 0..sections.values.len() {
        for (op, km, offset) in [
            ("from", "start-km", "start-offset"),
            ("to", "end-km", "end-offset"),
        ] {
            let Some(km) = cell(sections, i, km).iter().find_map(number_of) else {
                continue;
            };
            let offset = cell(sections, i, offset)
                .iter()
                .find_map(number_of)
                .unwrap_or(0.0);
            for v in cell(sections, i, op) {
                let id = match v {
                    Value::Iri(l) => l.iri.as_str().to_owned(),
                    other => name_of(other),
                };
                if seen.insert(id) {
                    stops.push((km * 1000.0 + offset, v));
                }
            }
        }
    }
    if stops.is_empty() {
        return false;
    }
    stops.sort_by(|a, b| {
        a.0.total_cmp(&b.0)
            .then_with(|| name_of(a.1).cmp(&name_of(b.1)))
    });
    out.push_str("<ol class=\"stops\">");
    for (metres, v) in stops {
        out.push_str("<li>");
        if let Some(km) = km_text(0.0, metres) {
            let _ = write!(out, "<span class=\"km\">{}</span> ", escape(&km));
        }
        html_value(out, links, v, "LabelViewer");
        out.push_str("</li>");
    }
    out.push_str("</ol>");
    if sections.more > 0 || sections.table.as_ref().is_some_and(|t| t.truncated) {
        out.push_str("<div class=\"more\">Of the first sections only: the line has more.</div>");
    }
    true
}

/// The first number of a widget's named path.
fn param_number(r: &Row, k: &str) -> Option<f64> {
    r.params
        .iter()
        .find(|(n, _)| n == k)
        .and_then(|(_, v)| v.iter().find_map(number_of))
}

fn param_value<'r>(r: &'r Row, k: &str) -> Option<&'r Value> {
    r.params
        .iter()
        .find(|(n, _)| n == k)
        .and_then(|(_, v)| v.first())
}

/// `rd:RouteDiagramViewer`: a section from its `start` to its `end` operational point, with
/// their km (`startKm`, `startOffset`, `endKm`, `endOffset`) and its `length`.
fn html_route_diagram(out: &mut String, links: &PageLinks<'_>, r: &Row) -> bool {
    let (Some(start), Some(end)) = (param_value(r, "start"), param_value(r, "end")) else {
        return false;
    };
    let km = |k: &str, o: &str| {
        param_number(r, k).and_then(|km| km_text(km, param_number(r, o).unwrap_or(0.0)))
    };
    out.push_str("<div class=\"route\"><span class=\"route-end\">");
    html_value(out, links, start, "LabelViewer");
    if let Some(k) = km("startKm", "startOffset") {
        let _ = write!(out, " <span class=\"km\">{}</span>", escape(&k));
    }
    out.push_str("</span><span class=\"route-line\">");
    if let Some(length) = param_number(r, "length") {
        let unit = r
            .unit
            .as_deref()
            .map(|u| format!(" {}", escape(u)))
            .unwrap_or_default();
        let _ = write!(out, "{}{unit}", number_text(length));
    }
    out.push_str("</span><span class=\"route-end\">");
    html_value(out, links, end, "LabelViewer");
    if let Some(k) = km("endKm", "endOffset") {
        let _ = write!(out, " <span class=\"km\">{}</span>", escape(&k));
    }
    out.push_str("</span></div>");
    true
}

/// Markers of an along-line strip drawn at most.
const MAX_MARKERS: usize = 500;

/// `rd:AlongLineViewer`: a section as a strip from its start to its end km (the `rd:table`
/// row's `startKm`... paths), with the values of the `rd:markers` rows at their km: a
/// stretch (columns `start-km`, `start-offset`, `end-km`, `end-offset`) as a band, a point
/// (`km`, `offset`) as a tick, or a short stretch (`km` and `length` in metres). What lies
/// outside the section is left out.
fn html_along_line(out: &mut String, r: &Row) -> bool {
    let Some(route) = table_ref(r) else {
        return false;
    };
    let param = |k: &str| {
        route
            .params
            .iter()
            .find(|(n, _)| n == k)
            .and_then(|(_, v)| v.iter().find_map(measure_of))
    };
    let km = |k: &str, o: &str| param(k).map(|km| km + param(o).unwrap_or(0.0) / 1000.0);
    let (Some(a), Some(b)) = (km("startKm", "startOffset"), km("endKm", "endOffset")) else {
        return false;
    };
    let (lo, hi) = (a.min(b), a.max(b));
    if !(hi - lo).is_finite() || hi - lo <= 0.0 {
        return false;
    }
    let keys: Vec<&str> = r
        .settings
        .iter()
        .find(|(k, _)| k == "markers")
        .map(|(_, v)| v.split_whitespace().collect())
        .unwrap_or_default();
    let at = |row: &Row, i: usize, k: &str, o: &str| {
        cell(row, i, k)
            .iter()
            .find_map(measure_of)
            .map(|km| km + cell(row, i, o).iter().find_map(measure_of).unwrap_or(0.0) / 1000.0)
    };
    // (lane, from, to or none for a point, value) of each marker within the section.
    let mut marks: Vec<(usize, &str, f64, Option<f64>, &Value)> = Vec::new();
    for (lane, key) in keys.iter().enumerate() {
        let Some((_, row)) = r.refs.iter().find(|(k, _)| k == key) else {
            continue;
        };
        for (i, v) in row.values.iter().enumerate() {
            let mark = if let Some(s) = at(row, i, "start-km", "start-offset") {
                let e = at(row, i, "end-km", "end-offset").unwrap_or(s);
                Some((s.min(e), Some(s.max(e))))
            } else {
                at(row, i, "km", "offset").map(|k| {
                    let len = cell(row, i, "length").iter().find_map(measure_of);
                    (k, len.filter(|l| *l > 0.0).map(|l| k + l / 1000.0))
                })
            };
            let Some((from, to)) = mark else { continue };
            if to.unwrap_or(from) < lo || from > hi || marks.len() == MAX_MARKERS {
                continue;
            }
            marks.push((
                lane,
                row.label.as_str(),
                from.max(lo),
                to.map(|t| t.min(hi)),
                v,
            ));
        }
    }
    let (w, h) = (640.0, 64.0);
    let (left, right) = (8.0, 8.0);
    let x = |k: f64| left + (k - lo) / (hi - lo) * (w - left - right);
    let _ = write!(
        out,
        "<svg class=\"along\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-label=\"The section from {} to {}\">\
         <rect class=\"strip\" x=\"{left}\" y=\"24\" width=\"{}\" height=\"8\"/>\
         <text class=\"tick\" x=\"{left}\" y=\"52\">{}</text>\
         <text class=\"tick\" x=\"{}\" y=\"52\" text-anchor=\"end\">{}</text>",
        escape(&km_text(lo, 0.0).unwrap_or_default()),
        escape(&km_text(hi, 0.0).unwrap_or_default()),
        w - left - right,
        escape(&km_text(lo, 0.0).unwrap_or_default()),
        w - right,
        escape(&km_text(hi, 0.0).unwrap_or_default()),
    );
    for (lane, label, from, to, v) in &marks {
        let title = format!(
            "{label}: {} at {}{}",
            name_of(v),
            km_text(*from, 0.0).unwrap_or_default(),
            to.and_then(|t| km_text(t, 0.0))
                .map(|t| format!(" to {t}"))
                .unwrap_or_default()
        );
        match to {
            Some(t) => {
                let _ = write!(
                    out,
                    "<rect class=\"mark lane-{}\" x=\"{:.1}\" y=\"18\" width=\"{:.1}\" height=\"20\"><title>{}</title></rect>",
                    lane % 6,
                    x(*from),
                    (x(*t) - x(*from)).max(2.0),
                    escape(&title)
                );
            }
            None => {
                let _ = write!(
                    out,
                    "<line class=\"mark lane-{}\" x1=\"{x:.1}\" x2=\"{x:.1}\" y1=\"14\" y2=\"42\"><title>{}</title></line>",
                    lane % 6,
                    escape(&title),
                    x = x(*from)
                );
            }
        }
    }
    out.push_str("</svg><div class=\"marks\">");
    for (lane, key) in keys.iter().enumerate() {
        let Some((_, row)) = r.refs.iter().find(|(k, _)| k == key) else {
            continue;
        };
        let n = marks.iter().filter(|m| m.0 == lane).count();
        if n > 0 {
            // A marker row read only in part says so.
            let partial = row.more > 0 || row.table.as_ref().is_some_and(|t| t.truncated);
            let _ = write!(
                out,
                "<span class=\"legend lane-{}\">{} ({n}{})</span>",
                lane % 6,
                escape(&row.label),
                if partial {
                    " of the first read; there are more"
                } else {
                    ""
                }
            );
        }
    }
    out.push_str("</div>");
    true
}

/// `rd:LineProfileViewer`: a line along km, one lane per column of its sections (the
/// `rd:table` row, `rd:lanes`): numbers as steps (the highest of a cell), other values as
/// coloured bands (the most common of a cell), operational points (`from`, `to`) as ticks.
/// A section is placed by its `start-km` (+ `start-offset`) and `end-km` or `length`;
/// sections without a km are left out. `rd:profiles` lanes (gradient profiles) are not drawn
/// yet.
fn html_line_profile(out: &mut String, r: &Row) -> bool {
    let Some(sections) = table_ref(r) else {
        return false;
    };
    let Some(t) = &sections.table else {
        return false;
    };
    let num = |i: usize, k: &str| cell(sections, i, k).iter().find_map(number_of);
    // (row, from km, to km) of each section with a km.
    let placed: Vec<(usize, f64, f64)> = (0..sections.values.len())
        .filter_map(|i| {
            let a = num(i, "start-km")? + num(i, "start-offset").unwrap_or(0.0) / 1000.0;
            let b = num(i, "end-km")
                .map(|e| e + num(i, "end-offset").unwrap_or(0.0) / 1000.0)
                .or_else(|| num(i, "length").map(|l| a + l))?;
            // A span that is not a finite number of km is left out.
            ((b - a).is_finite() && a != b && (b - a).abs() < 1e7).then_some((
                i,
                a.min(b),
                a.max(b),
            ))
        })
        .collect();
    if placed.is_empty() {
        return false;
    }
    let lo = placed.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
    let hi = placed.iter().map(|p| p.2).fold(f64::NEG_INFINITY, f64::max);
    let setting = |k: &str| {
        r.settings
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.split_whitespace().collect::<Vec<_>>())
            .unwrap_or_default()
    };
    let profiles = setting("profiles");
    let lanes: Vec<&Column> = setting("lanes")
        .into_iter()
        .filter(|k| !profiles.contains(k))
        .filter_map(|k| t.columns.iter().find(|c| c.key == k))
        .collect();
    let (w, lane_h, label_w, top) = (960.0, 22.0, 120.0, 26.0);
    let h = top + lane_h * lanes.len() as f64 + 20.0;
    let x = |k: f64| label_w + (k - lo) / (hi - lo).max(1e-9) * (w - label_w - 8.0);
    let _ = write!(
        out,
        "<svg class=\"profile\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-label=\"The line from {} to {}\">",
        escape(&km_text(lo, 0.0).unwrap_or_default()),
        escape(&km_text(hi, 0.0).unwrap_or_default())
    );
    // The operational points at the ends of the sections, as ticks over the lanes.
    let mut ticks: Vec<(f64, String)> = Vec::new();
    for (i, a, b) in &placed {
        for (k, col) in [(*a, "from"), (*b, "to")] {
            if let Some(v) = cell(sections, *i, col).first() {
                ticks.push((k, name_of(v)));
            }
        }
    }
    ticks.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    ticks.dedup_by(|a, b| (a.0 - b.0).abs() < 1e-9);
    for (k, name) in &ticks {
        let _ = write!(
            out,
            "<line class=\"op\" x1=\"{x:.1}\" x2=\"{x:.1}\" y1=\"{}\" y2=\"{}\"><title>{} at {}</title></line>",
            top - 6.0,
            h - 20.0,
            escape(name),
            escape(&km_text(*k, 0.0).unwrap_or_default()),
            x = x(*k)
        );
    }
    let _ = write!(
        out,
        "<text class=\"tick\" x=\"{label_w}\" y=\"{}\">{}</text><text class=\"tick\" x=\"{}\" y=\"{}\" text-anchor=\"end\">{}</text>",
        h - 6.0,
        escape(&km_text(lo, 0.0).unwrap_or_default()),
        w - 8.0,
        h - 6.0,
        escape(&km_text(hi, 0.0).unwrap_or_default())
    );
    let mut legend: Vec<(String, Vec<String>)> = Vec::new();
    for (n, col) in lanes.iter().enumerate() {
        let y = top + lane_h * n as f64;
        let ci = t.columns.iter().position(|c| c.key == col.key).unwrap_or(0);
        let values = |i: usize| {
            t.cells
                .get(i)
                .and_then(|c| c.get(ci))
                .map_or(&[][..], Vec::as_slice)
        };
        let _ = write!(
            out,
            "<text class=\"lane-label\" x=\"0\" y=\"{:.1}\">{}</text>",
            y + lane_h * 0.65,
            escape(&col.label)
        );
        let numeric = placed
            .iter()
            .flat_map(|(i, ..)| values(*i))
            .all(|v| number_of(v).is_some());
        if numeric {
            let peak = placed
                .iter()
                .filter_map(|(i, ..)| values(*i).iter().filter_map(number_of).reduce(f64::max))
                .fold(0.0, f64::max);
            for (i, a, b) in &placed {
                let Some(v) = values(*i).iter().filter_map(number_of).reduce(f64::max) else {
                    continue;
                };
                let bar = if peak > 0.0 {
                    (v / peak).clamp(0.0, 1.0) * (lane_h - 6.0)
                } else {
                    0.0
                };
                let unit = col
                    .unit
                    .as_deref()
                    .map(|u| format!(" {}", escape(u)))
                    .unwrap_or_default();
                let _ = write!(
                    out,
                    "<rect class=\"step\" x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\"><title>{}: {}{unit}</title></rect>",
                    x(*a),
                    y + lane_h - 3.0 - bar.max(1.0),
                    (x(*b) - x(*a)).max(1.0),
                    bar.max(1.0),
                    escape(&col.label),
                    number_text(v)
                );
            }
        } else {
            let mut categories: Vec<String> = Vec::new();
            for (i, a, b) in &placed {
                let mut counts: Vec<(String, usize)> = Vec::new();
                for v in values(*i) {
                    let name = name_of(v);
                    match counts.iter_mut().find(|c| c.0 == name) {
                        Some(c) => c.1 += 1,
                        None => counts.push((name, 1)),
                    }
                }
                let Some((name, _)) = counts
                    .into_iter()
                    .max_by(|p, q| p.1.cmp(&q.1).then_with(|| q.0.cmp(&p.0)))
                else {
                    continue;
                };
                let c = match categories.iter().position(|n| *n == name) {
                    Some(c) => c,
                    None => {
                        categories.push(name.clone());
                        categories.len() - 1
                    }
                };
                let _ = write!(
                    out,
                    "<rect class=\"band lane-{}\" x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\"><title>{}: {}</title></rect>",
                    c % 6,
                    x(*a),
                    y + 3.0,
                    (x(*b) - x(*a)).max(1.0),
                    lane_h - 6.0,
                    escape(&col.label),
                    escape(&name)
                );
            }
            legend.push((col.label.clone(), categories));
        }
    }
    out.push_str("</svg>");
    for (label, categories) in legend.iter().filter(|l| !l.1.is_empty()) {
        let _ = write!(out, "<div class=\"marks\">{}: ", escape(label));
        for (c, name) in categories.iter().enumerate().take(12) {
            let _ = write!(
                out,
                "<span class=\"legend lane-{}\">{}</span>",
                c % 6,
                escape(name)
            );
        }
        out.push_str("</div>");
    }
    let _ = write!(
        out,
        "<div class=\"more\">{} sections of line along {} km{}.</div>",
        placed.len(),
        number_text(((hi - lo) * 10.0).round() / 10.0),
        if placed.len() < sections.values.len() || sections.more > 0 {
            "; sections without a km, or past the first ones read, are not drawn"
        } else {
            ""
        }
    );
    true
}

/// `rd:LinePositionViewer`: where a kilometric post (the page's resource) is on its line.
/// The line as a strip over the km of its sections (the `rd:table` row) and posts (the
/// `rd:posts` row, column `km`), its operational points as stops, the other posts as ticks and
/// this one marked; then the stops on either side.
fn html_line_position(out: &mut String, links: &PageLinks<'_>, r: &Row, own: &NamedNode) -> bool {
    let posts_key = r
        .settings
        .iter()
        .find(|(k, _)| k == "posts")
        .and_then(|(_, v)| v.split_whitespace().next());
    let posts = posts_key.and_then(|k| r.refs.iter().find(|(n, _)| n == k).map(|(_, row)| row));
    let Some(posts) = posts else { return false };
    let km_of = |row: &Row, i: usize| cell(row, i, "km").iter().find_map(measure_of);
    let mut here = None;
    let mut ticks: Vec<f64> = Vec::new();
    for (i, v) in posts.values.iter().enumerate() {
        let Some(k) = km_of(posts, i) else { continue };
        if matches!(v, Value::Iri(l) if l.iri == *own) {
            here = Some(k);
        } else {
            ticks.push(k);
        }
    }
    let Some(here) = here else { return false };
    // The operational points at the ends of the sections, each once.
    let mut stops: Vec<(f64, &Value)> = Vec::new();
    if let Some(sections) = table_ref(r) {
        let num = |i: usize, k: &str| cell(sections, i, k).iter().find_map(measure_of);
        let mut seen = HashSet::new();
        for i in 0..sections.values.len() {
            for (op, km, off) in [
                ("from", "start-km", "start-offset"),
                ("to", "end-km", "end-offset"),
            ] {
                let Some(k) = num(i, km) else { continue };
                let k = k + num(i, off).unwrap_or(0.0) / 1000.0;
                for v in cell(sections, i, op) {
                    if seen.insert(name_of(v)) {
                        stops.push((k, v));
                    }
                }
            }
        }
    }
    stops.sort_by(|a, b| a.0.total_cmp(&b.0));
    let all = || {
        ticks
            .iter()
            .copied()
            .chain(stops.iter().map(|s| s.0))
            .chain([here])
    };
    let lo = all().fold(f64::INFINITY, f64::min);
    let hi = all().fold(f64::NEG_INFINITY, f64::max);
    let (w, h, left, right) = (640.0, 56.0, 8.0, 8.0);
    let x = |k: f64| left + if hi > lo { (k - lo) / (hi - lo) } else { 0.5 } * (w - left - right);
    let _ = write!(
        out,
        "<svg class=\"along\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-label=\"This post at {} on a line from {} to {}\">\
         <rect class=\"strip\" x=\"{left}\" y=\"22\" width=\"{}\" height=\"6\"/>",
        escape(&km_text(here, 0.0).unwrap_or_default()),
        escape(&km_text(lo, 0.0).unwrap_or_default()),
        escape(&km_text(hi, 0.0).unwrap_or_default()),
        w - left - right
    );
    for k in ticks.iter().take(MAX_MARKERS) {
        let _ = write!(
            out,
            "<line class=\"post\" x1=\"{x:.1}\" x2=\"{x:.1}\" y1=\"18\" y2=\"32\"/>",
            x = x(*k)
        );
    }
    for (k, v) in stops.iter().take(MAX_MARKERS) {
        let _ = write!(
            out,
            "<circle class=\"stop\" cx=\"{:.1}\" cy=\"25\" r=\"4\"><title>{} at {}</title></circle>",
            x(*k),
            escape(&name_of(v)),
            escape(&km_text(*k, 0.0).unwrap_or_default())
        );
    }
    let _ = write!(
        out,
        "<path class=\"here\" d=\"M{x:.1} 16 l-6 -10 h12 z\"><title>This post at {}</title></path>\
         <text class=\"tick\" x=\"{left}\" y=\"50\">{}</text><text class=\"tick\" x=\"{}\" y=\"50\" text-anchor=\"end\">{}</text></svg>",
        escape(&km_text(here, 0.0).unwrap_or_default()),
        escape(&km_text(lo, 0.0).unwrap_or_default()),
        w - right,
        escape(&km_text(hi, 0.0).unwrap_or_default()),
        x = x(here)
    );
    let before = stops.iter().rev().find(|s| s.0 <= here);
    let after = stops.iter().find(|s| s.0 > here);
    if before.is_some() || after.is_some() {
        out.push_str("<div class=\"marks\">");
        for (word, stop) in [("After", before), ("Before", after)] {
            let Some((k, v)) = stop else { continue };
            let _ = write!(out, "<span class=\"mark-label\">{word} ");
            html_value(out, links, v, "LabelViewer");
            let _ = write!(
                out,
                " <span class=\"km\">{}</span> ({} km away)</span>",
                escape(&km_text(*k, 0.0).unwrap_or_default()),
                number_text(((k - here).abs() * 1000.0).round() / 1000.0)
            );
        }
        out.push_str("</div>");
    }
    true
}

/// The row a widget draws from by the key in its setting `name` (`rd:platforms "..."`).
fn ref_row<'r>(r: &'r Row, name: &str) -> Option<&'r Row> {
    let key = r
        .settings
        .iter()
        .find(|(k, _)| k == name)?
        .1
        .split_whitespace()
        .next()?;
    r.refs.iter().find(|(k, _)| k == key).map(|(_, row)| row)
}

/// A link to an IRI with its own text.
fn html_link_text(out: &mut String, links: &PageLinks<'_>, l: &Link, text: &str, class: &str) {
    let _ = write!(
        out,
        "<a class=\"{class}\" href=\"{}\" title=\"{}\">{}</a>",
        escape(&(links.href)(l.iri.as_str())),
        escape(l.iri.as_str()),
        escape(text)
    );
}

/// A boolean literal (`true`, `1`, `false`, `0`).
fn bool_of(v: &Value) -> Option<bool> {
    match v {
        Value::Literal(l) => match l.value().trim() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Millimetres from a platform height ("550", "760 mm"); none for ranges and words.
fn height_mm(text: &str) -> Option<f64> {
    let t = text.trim();
    let t = t.strip_suffix("mm").unwrap_or(t).trim();
    ((2..=4).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_digit()))
        .then(|| t.parse().ok())
        .flatten()
}

/// `rd:PlatformViewer`: a platform edge side-on, its height above the rail (`height`, a
/// value or a concept labelled in mm) to scale against the TSI heights 550 and 760 mm, with
/// its usable `length` (m) and whether it is `curved`. The length is not to scale.
fn html_platform(out: &mut String, r: &Row) -> bool {
    let label = param_value(r, "height").map(name_of);
    let mm = label.as_deref().and_then(height_mm);
    let length = param_number(r, "length").filter(|n| *n > 0.0);
    let curved = param_value(r, "curved").and_then(bool_of) == Some(true);
    if mm.is_none() && length.is_none() {
        return false;
    }
    let (w, h, rail, scale) = (640.0, 230.0, 196.0, 0.2);
    let top = mm.map_or(rail - 80.0, |mm| rail - mm.min(950.0) * scale);
    let mut text = match (mm, &label) {
        (Some(mm), _) => format!("{} mm", number_text(mm)),
        (None, Some(l)) if !l.is_empty() => l.clone(),
        _ => "Height unknown".to_owned(),
    };
    if let Some(l) = length {
        let _ = write!(text, " \u{b7} {} m usable", number_text(l));
    }
    if curved {
        text.push_str(" \u{b7} curved");
    }
    let _ = write!(
        out,
        "<figure class=\"platform-sketch\"><svg class=\"sketch\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-label=\"Platform: {}\">\
         <path class=\"rail\" d=\"M20 {rail}H{}\"/>",
        escape(&text),
        w - 20.0
    );
    for (reference, name) in [(550.0, "550 mm"), (760.0, "760 mm")] {
        let y = rail - reference * scale;
        let _ = write!(
            out,
            "<path class=\"ref{}\" d=\"M20 {y:.1}H{}\"/><text class=\"tick\" x=\"{}\" y=\"{:.1}\" text-anchor=\"end\">TSI {name}</text>",
            if mm == Some(reference) { " hit" } else { "" },
            w - 20.0,
            w - 22.0,
            y - 4.0
        );
    }
    let (x0, x1) = (70.0, w - 120.0);
    let edge = if curved {
        format!("Q{} {:.1} {x1} {top:.1}", (x0 + x1) / 2.0, top + 10.0)
    } else {
        format!("L{x1} {top:.1}")
    };
    let _ = write!(
        out,
        "<path class=\"block{}\" d=\"M{x0} {rail}L{x0} {top:.1}{edge}L{x1} {rail}Z\"/>\
         <text class=\"label\" x=\"{}\" y=\"{:.1}\" text-anchor=\"middle\">{}</text></svg>\
         <figcaption class=\"more\">Side view: the height to scale against the TSI platform heights, the length not.</figcaption></figure>",
        if mm.is_none() { " unknown" } else { "" },
        (x0 + x1) / 2.0,
        (top + rail) / 2.0 + 5.0,
        escape(&text)
    );
    true
}

/// `rd:CrossSectionViewer`: a track end-on, not to scale: the rails at their `gauge`, the
/// gauging profile (`loadingGauge`) above them, the contact wire between `wireMin` and
/// `wireMax` (m), and the rail `inclination`.
fn html_cross_section(out: &mut String, r: &Row) -> bool {
    let text = |k: &str| {
        let names: Vec<String> = r
            .params
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.iter().map(name_of).filter(|n| !n.is_empty()).collect())
            .unwrap_or_default();
        (!names.is_empty()).then(|| names.join(" \u{b7} "))
    };
    let (gauge, profile, inclination) = (text("gauge"), text("loadingGauge"), text("inclination"));
    let (lo, hi) = (param_number(r, "wireMin"), param_number(r, "wireMax"));
    let wire = match (lo, hi) {
        (Some(a), Some(b)) => Some((a.min(b), a.max(b))),
        (Some(a), None) | (None, Some(a)) => Some((a, a)),
        _ => None,
    };
    if gauge.is_none() && profile.is_none() && wire.is_none() && inclination.is_none() {
        return false;
    }
    let (w, h, cx, rail) = (420.0, 300.0, 210.0, 262.0);
    let y0 = if wire.is_some() { 0.0 } else { 66.0 };
    let _ = write!(
        out,
        "<figure class=\"cross-section\"><svg class=\"sketch\" viewBox=\"0 {y0} {w} {}\" role=\"img\" aria-label=\"Cross-section of the track, not to scale\">\
         <path class=\"ballast\" d=\"M60 {}L100 {}H320L360 {}Z\"/><rect class=\"sleeper\" x=\"110\" y=\"{}\" width=\"200\" height=\"8\"/>\
         <rect class=\"rail-head\" x=\"144\" y=\"{}\" width=\"12\" height=\"12\"/><rect class=\"rail-head\" x=\"264\" y=\"{}\" width=\"12\" height=\"12\"/>",
        h - y0,
        rail + 26.0,
        rail + 6.0,
        rail + 26.0,
        rail - 2.0,
        rail - 14.0,
        rail - 14.0
    );
    if let Some(g) = &gauge {
        let _ = write!(
            out,
            "<path class=\"measure\" d=\"M150 {}H270M150 {}v8M270 {}v8\"/><text class=\"label\" x=\"{cx}\" y=\"{}\" text-anchor=\"middle\">{}</text>",
            rail - 22.0,
            rail - 26.0,
            rail - 26.0,
            rail - 28.0,
            escape(g)
        );
    }
    let _ = write!(
        out,
        "<path class=\"profile-outline{}\" d=\"M112 {}V120Q112 86 146 80H274Q308 86 308 120V{}\"/>",
        if profile.is_none() { " unknown" } else { "" },
        rail - 40.0,
        rail - 40.0
    );
    if let Some(p) = &profile {
        let _ = write!(
            out,
            "<text class=\"label strong\" x=\"{cx}\" y=\"150\" text-anchor=\"middle\">{}</text>",
            escape(p)
        );
    }
    if let Some((lo, hi)) = wire {
        let span = if lo == hi {
            format!("{} m", number_text(lo))
        } else {
            format!("{}\u{2013}{} m", number_text(lo), number_text(hi))
        };
        let _ = write!(
            out,
            "<rect class=\"wire-band\" x=\"120\" y=\"34\" width=\"180\" height=\"22\"/><path class=\"wire\" d=\"M120 45H300\"/>\
             <text class=\"tick\" x=\"312\" y=\"36\">contact wire</text><text class=\"label\" x=\"312\" y=\"50\">{}</text>",
            escape(&span)
        );
    }
    if let Some(i) = &inclination {
        let _ = write!(
            out,
            "<text class=\"tick\" x=\"300\" y=\"{}\">rails {}</text>",
            rail - 2.0,
            escape(i)
        );
    }
    out.push_str("</svg><figcaption class=\"more\">Not to scale.</figcaption></figure>");
    true
}

/// The start all names share, cut back to a `" - "` so that no word is split ("Leuven - ").
fn common_prefix(names: &[String]) -> usize {
    let Some(first) = names.first().filter(|_| names.len() > 1) else {
        return 0;
    };
    let mut p = first.as_str();
    for n in names {
        while !n.starts_with(p) {
            let mut end = p.len() - 1;
            while !p.is_char_boundary(end) {
                end -= 1;
            }
            p = &p[..end];
        }
    }
    p.rfind(" - ").map_or(0, |i| i + 3)
}

/// Tracks, platforms and sidings of a track layout at most.
const MAX_LAYOUT_ITEMS: usize = 200;

/// `rd:TrackLayoutViewer`: a schematic of an operational point: its tracks (the row, column
/// `id`) as bars in order of their id, the platform edges on them (the `rd:platforms` row,
/// columns `track`, `length`, `height`) and the sidings below (the `rd:sidings` row, column
/// `length`). Platforms and sidings are drawn to their usable length against the longest.
fn html_track_layout(out: &mut String, links: &PageLinks<'_>, r: &Row) -> bool {
    let platforms = ref_row(r, "platforms");
    let sidings = ref_row(r, "sidings");
    let n_platforms = platforms.map_or(0, |p| p.values.len());
    let n_sidings = sidings.map_or(0, |s| s.values.len());
    if r.values.is_empty() && n_sidings == 0 {
        return false;
    }
    let length = |row: &Row, i: usize| {
        cell(row, i, "length")
            .iter()
            .find_map(number_of)
            .filter(|n| *n > 0.0)
    };
    let longest = platforms
        .into_iter()
        .chain(sidings)
        .flat_map(|row| (0..row.values.len()).filter_map(move |i| length(row, i)))
        .fold(1.0, f64::max);
    let name = |row: &Row, i: usize, col: &str| {
        cell(row, i, col)
            .first()
            .map(name_of)
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| row.values.get(i).map(name_of).unwrap_or_default())
    };
    let names: Vec<String> = (0..r.values.len()).map(|i| name(r, i, "id")).collect();
    let cut = common_prefix(&names);
    let short = |n: &str| {
        let s = &n[cut.min(n.len())..];
        if s.is_empty() {
            n.to_owned()
        } else {
            s.to_owned()
        }
    };
    let mut order: Vec<usize> = (0..r.values.len()).collect();
    order.sort_by(|a, b| natural_cmp(&short(&names[*a]), &short(&names[*b])));
    out.push_str("<div class=\"track-layout\">");
    if !order.is_empty() {
        out.push_str("<ol class=\"tl-tracks\" aria-label=\"Tracks\">");
    }
    for &i in order.iter().take(MAX_LAYOUT_ITEMS) {
        out.push_str("<li class=\"tl-track\">");
        match &r.values[i] {
            Value::Iri(l) => html_link_text(out, links, l, &short(&names[i]), "tl-id"),
            _ => {
                let _ = write!(
                    out,
                    "<span class=\"tl-id\">{}</span>",
                    escape(&short(&names[i]))
                );
            }
        }
        out.push_str("<div class=\"tl-lane\"><span class=\"tl-rail\"></span>");
        if let (Some(p), Value::Iri(track)) = (platforms, &r.values[i]) {
            for j in (0..p.values.len()).take(MAX_LAYOUT_ITEMS) {
                let on = cell(p, j, "track")
                    .iter()
                    .any(|v| matches!(v, Value::Iri(l) if l.iri == track.iri));
                let Some(Value::Iri(pl)) = p.values.get(j).filter(|_| on) else {
                    continue;
                };
                let len = length(p, j);
                let height = cell(p, j, "height").first().map(name_of);
                let mut text = len.map_or_else(
                    || name(p, j, "platform"),
                    |l| format!("{} m", number_text(l)),
                );
                if let Some(h) = height.as_deref().filter(|h| !h.is_empty()) {
                    let _ = write!(text, " \u{b7} {h} mm");
                }
                let _ = write!(
                    out,
                    "<a class=\"tl-platform\" style=\"width:{:.1}%\" href=\"{}\" title=\"{}\">{}</a>",
                    (len.unwrap_or(longest * 0.3) / longest * 100.0).max(12.0),
                    escape(&(links.href)(pl.iri.as_str())),
                    escape(&name_of(&p.values[j])),
                    escape(&text)
                );
            }
        }
        out.push_str("</div></li>");
    }
    if !order.is_empty() {
        out.push_str("</ol>");
    }
    if let Some(s) = sidings.filter(|s| !s.values.is_empty()) {
        let _ = write!(
            out,
            "<div class=\"more\">{} {}</div><ul class=\"tl-sidings\">",
            s.values.len(),
            if s.values.len() == 1 {
                "siding"
            } else {
                "sidings"
            }
        );
        let mut by_length: Vec<usize> = (0..s.values.len()).collect();
        by_length.sort_by(|a, b| {
            length(s, *b)
                .unwrap_or(0.0)
                .total_cmp(&length(s, *a).unwrap_or(0.0))
        });
        for i in by_length.into_iter().take(MAX_LAYOUT_ITEMS) {
            out.push_str("<li class=\"tl-siding\">");
            html_value(out, links, &s.values[i], "LabelViewer");
            let len = length(s, i);
            let _ = write!(
                out,
                "<span class=\"tl-bar\" style=\"width:{:.1}%\"></span><span class=\"more\">{}</span></li>",
                (len.unwrap_or(0.0) / longest * 100.0).max(4.0),
                len.map_or_else(
                    || "length unknown".to_owned(),
                    |l| format!("{} m", number_text(l))
                )
            );
        }
        out.push_str("</ul>");
    }
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let _ = write!(
        out,
        "<div class=\"more\">{}, {}. Schematic: platforms and sidings are drawn to their usable \
         length against the longest; tracks are not to scale.</div></div>",
        plural(r.values.len(), "track", "tracks"),
        plural(n_platforms, "platform edge", "platform edges")
    );
    true
}

/// Two names in the order of their text, with runs of digits by their number ("2" before
/// "10").
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn parts(s: &str) -> Vec<(bool, &str)> {
        let mut out = Vec::new();
        let mut start = 0;
        let bytes = s.as_bytes();
        for i in 1..=bytes.len() {
            if i == bytes.len() || bytes[i].is_ascii_digit() != bytes[start].is_ascii_digit() {
                out.push((bytes[start].is_ascii_digit(), &s[start..i]));
                start = i;
            }
        }
        out
    }
    let (pa, pb) = (parts(a), parts(b));
    for (x, y) in pa.iter().zip(&pb) {
        let o = match (x, y) {
            ((true, x), (true, y)) => {
                let (x, y) = (x.trim_start_matches('0'), y.trim_start_matches('0'));
                x.len().cmp(&y.len()).then_with(|| x.cmp(y))
            }
            ((_, x), (_, y)) => x.to_lowercase().cmp(&y.to_lowercase()),
        };
        if o.is_ne() {
            return o;
        }
    }
    pa.len().cmp(&pb.len()).then_with(|| a.cmp(b))
}

/// The points of a geometry in longitude/latitude, at most `MAX_LIVE_POINTS`.
fn lon_lat_points(g: &tripletrail::geo::Geom) -> Vec<(f64, f64)> {
    use geo::CoordsIter;
    tripletrail::geo::transform(&g.geometry, &g.crs, &tripletrail::geo::Crs::Crs84)
        .map(|g| {
            g.coords_iter()
                .take(MAX_LIVE_POINTS)
                .map(|c| (c.x, c.y))
                .filter(|(x, y)| (-180.0..=180.0).contains(x) && (-90.0..=90.0).contains(y))
                .collect()
        })
        .unwrap_or_default()
}

/// Bearing in degrees (0 east, counter-clockwise) from `centre` to the end of a section
/// furthest from it: where the section goes, not the way it leaves the station.
fn bearing(centre: (f64, f64), section: &[(f64, f64)]) -> Option<f64> {
    let (a, b) = (section.first()?, section.last()?);
    if section.len() < 2 {
        return None;
    }
    let k = centre.1.to_radians().cos();
    let d = |p: &(f64, f64)| ((p.0 - centre.0) * k).hypot(p.1 - centre.1);
    let far = if d(a) > d(b) { a } else { b };
    (d(far) > 1e-9).then(|| {
        (far.1 - centre.1)
            .atan2((far.0 - centre.0) * k)
            .to_degrees()
    })
}

/// Angles spread so that no two are closer than `min` degrees.
fn spread(angles: &[f64], min: f64) -> Vec<f64> {
    let mut order: Vec<(f64, usize)> = angles
        .iter()
        .enumerate()
        .map(|(i, a)| (a.rem_euclid(360.0), i))
        .collect();
    order.sort_by(|x, y| x.0.total_cmp(&y.0));
    for _ in 0..8 {
        for j in 1..order.len() {
            if order[j].0 - order[j - 1].0 < min {
                order[j].0 = order[j - 1].0 + min;
            }
        }
    }
    let mut out = vec![0.0; angles.len()];
    for (a, i) in order {
        out[i] = a;
    }
    out
}

/// Many angles: evenly spaced in the order of their bearings, turned to stay closest to them.
fn evenly(angles: &[f64]) -> Vec<f64> {
    let n = angles.len();
    let step = 360.0 / n as f64;
    let mut order: Vec<(f64, usize)> = angles
        .iter()
        .enumerate()
        .map(|(i, a)| (a.rem_euclid(360.0), i))
        .collect();
    order.sort_by(|x, y| x.0.total_cmp(&y.0));
    let (mut sx, mut sy) = (0.0, 0.0);
    for (k, (a, _)) in order.iter().enumerate() {
        let d = (a - k as f64 * step).to_radians();
        sx += d.cos();
        sy += d.sin();
    }
    let turn = f64::atan2(sy, sx).to_degrees();
    let mut out = vec![0.0; n];
    for (k, (_, i)) in order.iter().enumerate() {
        out[*i] = turn + k as f64 * step;
    }
    out
}

/// Connections drawn as spokes at most; the table lists them all.
const MAX_SPOKES: usize = 60;

/// `rd:ConnectionsViewer`: the resource in the middle and the sections of the row (columns
/// `start`, `end`, `line`, `length` in km, `wkt`) as spokes, each to the operational point at
/// its other end. With the geometries of the resource and every section, a spoke points
/// where its section goes, north up; else they are evenly spread. Not to scale.
fn html_connections(
    out: &mut String,
    links: &PageLinks<'_>,
    r: &Row,
    own: &NamedNode,
    geometries: &[Geometry],
) -> bool {
    if r.values.is_empty() {
        return false;
    }
    let pts: Vec<(f64, f64)> = geometries
        .iter()
        .flat_map(|g| lon_lat_points(&g.geom))
        .collect();
    let centre = (!pts.is_empty()).then(|| {
        let n = pts.len() as f64;
        (
            pts.iter().map(|p| p.0).sum::<f64>() / n,
            pts.iter().map(|p| p.1).sum::<f64>() / n,
        )
    });
    struct Spoke<'r> {
        section: &'r Value,
        far: Option<&'r Value>,
        line: Option<&'r Value>,
        length: Option<f64>,
        angle: Option<f64>,
    }
    let is_own = |v: &Value| matches!(v, Value::Iri(l) if l.iri == *own);
    let spokes: Vec<Spoke<'_>> = r
        .values
        .iter()
        .enumerate()
        .map(|(i, section)| {
            let (start, end) = (cell(r, i, "start").first(), cell(r, i, "end").first());
            let far = match (start, end) {
                (Some(s), e) if is_own(s) => e,
                (s, Some(e)) if is_own(e) => s,
                (s, e) => e.or(s),
            };
            let angle = centre.and_then(|c| {
                cell(r, i, "wkt").iter().find_map(|v| match v {
                    Value::Literal(l) => {
                        let g = tripletrail::geo::parse_wkt_literal(&Term::Literal(l.clone()))?;
                        bearing(c, &lon_lat_points(&g))
                    }
                    _ => None,
                })
            });
            Spoke {
                section,
                far,
                line: cell(r, i, "line").first(),
                length: cell(r, i, "length").iter().find_map(measure_of),
                angle,
            }
        })
        .collect();
    let drawn = &spokes[..spokes.len().min(MAX_SPOKES)];
    let known = drawn.iter().all(|s| s.angle.is_some());
    let raw: Vec<f64> = drawn
        .iter()
        .enumerate()
        .map(|(i, s)| match s.angle {
            Some(a) if known => a,
            _ => 360.0 / drawn.len() as f64 * i as f64 + 90.0,
        })
        .collect();
    let angles = if drawn.len() > 8 {
        evenly(&raw)
    } else {
        spread(&raw, 28.0)
    };
    let (w, h, radius) = (640.0, 380.0, 130.0);
    let (cx, cy) = (w / 2.0, h / 2.0);
    let _ = write!(
        out,
        "<figure class=\"connections\"><svg class=\"sketch\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-label=\"{} connected {}\">",
        spokes.len(),
        if spokes.len() == 1 {
            "section"
        } else {
            "sections"
        }
    );
    for (s, deg) in drawn.iter().zip(&angles) {
        let a = deg.to_radians();
        let (x, y) = (cx + a.cos() * radius, cy - a.sin() * radius * 1.12);
        let far = s.far.map_or_else(|| "Unknown".to_owned(), name_of);
        let anchor = if a.cos().abs() < 0.26 {
            "middle"
        } else if a.cos() > 0.0 {
            "start"
        } else {
            "end"
        };
        let (tx, ty) = (
            x + a.cos() * 12.0,
            y - a.sin() * 12.0 + if a.sin() > 0.0 { -4.0 } else { 12.0 },
        );
        let line = s.line.map(name_of).unwrap_or_default();
        let _ = write!(out, "<g class=\"spoke\">");
        match s.section {
            Value::Iri(l) => {
                let _ = write!(
                    out,
                    "<a href=\"{}\"><title>Section of line: {}</title><line class=\"conn\" x1=\"{cx}\" y1=\"{cy}\" x2=\"{x:.1}\" y2=\"{y:.1}\"/></a>",
                    escape(&(links.href)(l.iri.as_str())),
                    escape(&name_of(s.section))
                );
            }
            _ => {
                let _ = write!(
                    out,
                    "<line class=\"conn\" x1=\"{cx}\" y1=\"{cy}\" x2=\"{x:.1}\" y2=\"{y:.1}\"/>"
                );
            }
        }
        let _ = write!(
            out,
            "<circle class=\"stop\" cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"6\"/>"
        );
        let far_text = format!(
            "<text class=\"label\" x=\"{tx:.1}\" y=\"{ty:.1}\" text-anchor=\"{anchor}\">{}</text>",
            escape(&far)
        );
        match s.far {
            Some(Value::Iri(l)) => {
                let _ = write!(
                    out,
                    "<a href=\"{}\">{far_text}</a>",
                    escape(&(links.href)(l.iri.as_str()))
                );
            }
            _ => out.push_str(&far_text),
        }
        let meta: Vec<String> = [
            (!line.is_empty()).then_some(line),
            s.length.map(|l| format!("{} km", number_text(l))),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !meta.is_empty() {
            let _ = write!(
                out,
                "<text class=\"tick\" x=\"{tx:.1}\" y=\"{:.1}\" text-anchor=\"{anchor}\">{}</text>",
                ty + 12.0,
                escape(&meta.join(" \u{b7} "))
            );
        }
        out.push_str("</g>");
    }
    let _ = write!(
        out,
        "<circle class=\"hub-ring\" cx=\"{cx}\" cy=\"{cy}\" r=\"13\"/><circle class=\"hub\" cx=\"{cx}\" cy=\"{cy}\" r=\"6\"/></svg>"
    );
    // The sections themselves, clockwise from north.
    let mut order: Vec<usize> = (0..spokes.len()).collect();
    let clockwise = |i: usize| {
        angles
            .get(i)
            .map_or(f64::MAX, |a| (90.0 - a).rem_euclid(360.0))
    };
    order.sort_by(|a, b| clockwise(*a).total_cmp(&clockwise(*b)));
    out.push_str("<table><tr><th>Section of line</th><th>Line</th><th>To</th><th>Length</th></tr>");
    for i in order {
        let s = &spokes[i];
        out.push_str("<tr><td>");
        html_value(out, links, s.section, "LabelViewer");
        out.push_str("</td><td>");
        match s.line {
            Some(v) => html_value(out, links, v, "LabelViewer"),
            None => out.push('\u{2013}'),
        }
        out.push_str("</td><td>");
        match s.far {
            Some(v) => html_value(out, links, v, "LabelViewer"),
            None => out.push('\u{2013}'),
        }
        let _ = write!(
            out,
            "</td><td>{}</td></tr>",
            s.length.map_or_else(
                || "\u{2013}".to_owned(),
                |l| format!("{} km", number_text(l))
            )
        );
    }
    let _ = write!(
        out,
        "</table><figcaption class=\"more\">{} {} of line start or end here{}{}. Not to scale.</figcaption></figure>",
        spokes.len(),
        if spokes.len() == 1 {
            "section"
        } else {
            "sections"
        },
        if known && centre.is_some() {
            "; each spoke points to where its section goes, north up"
        } else {
            ""
        },
        if spokes.len() > MAX_SPOKES {
            format!("; the first {MAX_SPOKES} are drawn")
        } else {
            String::new()
        }
    );
    true
}

/// Which way a train can pass a net relation, from its navigability concept (`.../Both`,
/// `AB`, `BA`, `None`).
#[derive(Clone, Copy, PartialEq)]
enum Way {
    Both,
    AB,
    BA,
    No,
    Unknown,
}

impl Way {
    fn of(v: Option<&Value>) -> Way {
        let Some(Value::Iri(l)) = v else {
            return Way::Unknown;
        };
        match local_name(l.iri.as_str()).to_ascii_lowercase().as_str() {
            "both" => Way::Both,
            "ab" => Way::AB,
            "ba" => Way::BA,
            "none" => Way::No,
            _ => Way::Unknown,
        }
    }

    fn class(self) -> &'static str {
        match self {
            Way::Both => "both",
            Way::AB => "ab",
            Way::BA => "ba",
            Way::No => "none",
            Way::Unknown => "unknown",
        }
    }

    fn text(self) -> &'static str {
        match self {
            Way::Both => "Navigable both ways",
            Way::AB => "Navigable from A to B",
            Way::BA => "Navigable from B to A",
            Way::No => "Not navigable",
            Way::Unknown => "Navigability not given",
        }
    }
}

/// A net relation as read: elements A and B, the way, which ends meet, its table line.
type RelationCells = (Link, Link, Way, (Option<bool>, Option<bool>), usize);

/// Elements and net relations of a topology drawn at most.
const MAX_TOPOLOGY_NODES: usize = 200;
const MAX_TOPOLOGY_EDGES: usize = 400;

/// A length in metres as `850 m` or `1.25 km`.
fn metres_text(m: f64) -> String {
    if m >= 1000.0 {
        format!("{} km", number_text((m / 10.0).round() / 100.0))
    } else {
        format!("{} m", number_text(m.round()))
    }
}

/// `rd:TopologyViewer`: the net relations of the row (columns `a`, `b`, `navigability`,
/// `origin-a`, `origin-b`) as a graph of linear elements: the page's element marked, the
/// others by how many relations away they are, arrows the way a train can pass, dashed when
/// it cannot. With the geometries of the `rd:elements` row (`wkt`, `length`), each element
/// sits where the middle of it lies, north up; else in rings around the page's element.
fn html_topology(out: &mut String, links: &PageLinks<'_>, r: &Row, own: &NamedNode) -> bool {
    struct Relation<'r> {
        a: &'r Link,
        b: &'r Link,
        way: Way,
        ends: (Option<bool>, Option<bool>),
    }
    let link = |vs: &[Value]| {
        vs.iter().find_map(|v| match v {
            Value::Iri(l) => Some(l.clone()),
            _ => None,
        })
    };
    let mut owned: Vec<RelationCells> = Vec::new();
    for i in 0..r.values.len() {
        let (Some(a), Some(b)) = (link(cell(r, i, "a")), link(cell(r, i, "b"))) else {
            continue;
        };
        let end = |k: &str| cell(r, i, k).iter().find_map(bool_of);
        owned.push((
            a,
            b,
            Way::of(cell(r, i, "navigability").first()),
            (end("origin-a"), end("origin-b")),
            i,
        ));
    }
    let rels: Vec<Relation<'_>> = owned
        .iter()
        .take(MAX_TOPOLOGY_EDGES)
        .map(|(a, b, way, ends, _)| Relation {
            a,
            b,
            way: *way,
            ends: *ends,
        })
        .collect();
    if rels.is_empty() {
        return false;
    }
    // Steps from the page's element to every element over the relations.
    let mut depth: Vec<(&Link, usize)> = Vec::new();
    let index = |depth: &[(&Link, usize)], l: &Link| depth.iter().position(|(n, _)| n.iri == l.iri);
    let start = rels.iter().flat_map(|r| [r.a, r.b]).find(|l| l.iri == *own);
    let mut front: Vec<&Link> = Vec::new();
    if let Some(s) = start {
        depth.push((s, 0));
        front.push(s);
    }
    loop {
        let mut next = Vec::new();
        for x in &front {
            for rel in &rels {
                let y = if rel.a.iri == x.iri {
                    rel.b
                } else if rel.b.iri == x.iri {
                    rel.a
                } else {
                    continue;
                };
                if index(&depth, y).is_none() && depth.len() < MAX_TOPOLOGY_NODES {
                    let d = depth[index(&depth, x).unwrap_or(0)].1 + 1;
                    depth.push((y, d));
                    next.push(y);
                }
            }
        }
        if next.is_empty() {
            // Elements not reached from the page's element: one step further than the rest.
            let far = depth.iter().map(|d| d.1).max().map_or(0, |d| d + 1);
            let Some(rest) = rels
                .iter()
                .flat_map(|r| [r.a, r.b])
                .find(|l| index(&depth, l).is_none())
                .filter(|_| depth.len() < MAX_TOPOLOGY_NODES)
            else {
                break;
            };
            depth.push((rest, far));
            front = vec![rest];
            continue;
        }
        front = next;
    }
    // Where the elements lie: the middle of each geometry of the elements row.
    let elements = ref_row(r, "elements");
    let mut geo: Vec<Option<(f64, f64)>> = vec![None; depth.len()];
    let mut lengths: Vec<Option<f64>> = vec![None; depth.len()];
    if let Some(e) = elements {
        for (i, v) in e.values.iter().enumerate() {
            let Value::Iri(l) = v else { continue };
            let Some(n) = index(&depth, l) else { continue };
            lengths[n] = cell(e, i, "length")
                .iter()
                .find_map(measure_of)
                .filter(|m| *m > 0.0);
            geo[n] = cell(e, i, "wkt").iter().find_map(|v| match v {
                Value::Literal(l) => {
                    let g = tripletrail::geo::parse_wkt_literal(&Term::Literal(l.clone()))?;
                    let pts = lon_lat_points(&g);
                    pts.get(pts.len() / 2).copied()
                }
                _ => None,
            });
        }
    }
    let (w, h, pad) = (640.0, 420.0, 60.0);
    let placed: Vec<(f64, f64)> = geo.iter().flatten().copied().collect();
    let mut pos: Vec<(f64, f64)> = vec![(w / 2.0, h / 2.0); depth.len()];
    let on_map = placed.len() >= 2;
    if on_map {
        let lat = placed.iter().map(|p| p.1).sum::<f64>() / placed.len() as f64;
        let k = lat.to_radians().cos();
        let xs = || placed.iter().map(|p| p.0 * k);
        let ys = || placed.iter().map(|p| -p.1);
        let (x0, x1) = (
            xs().fold(f64::INFINITY, f64::min),
            xs().fold(f64::NEG_INFINITY, f64::max),
        );
        let (y0, y1) = (
            ys().fold(f64::INFINITY, f64::min),
            ys().fold(f64::NEG_INFINITY, f64::max),
        );
        let span = (x1 - x0).max(y1 - y0).max(1e-12);
        let scale = (w - 2.0 * pad).min(h - 2.0 * pad) / span;
        for (n, g) in geo.iter().enumerate() {
            if let Some((lon, lat)) = g {
                pos[n] = (
                    w / 2.0 + (lon * k - (x0 + x1) / 2.0) * scale,
                    h / 2.0 + (-lat - (y0 + y1) / 2.0) * scale,
                );
            }
        }
    }
    // The others in rings by their distance, around the page's element.
    let max_depth = depth.iter().map(|d| d.1).max().unwrap_or(0).max(1);
    for d in 0..=max_depth {
        let ring: Vec<usize> = (0..depth.len())
            .filter(|&n| depth[n].1 == d && !(on_map && geo[n].is_some()))
            .collect();
        for (j, &n) in ring.iter().enumerate() {
            if d == 0 && !on_map {
                continue;
            }
            let radius = (h / 2.0 - 30.0) * d.max(1) as f64 / max_depth as f64;
            let a =
                std::f64::consts::TAU * j as f64 / ring.len() as f64 - std::f64::consts::FRAC_PI_2;
            pos[n] = (w / 2.0 + a.cos() * radius * 1.4, h / 2.0 + a.sin() * radius);
        }
    }
    let _ = write!(
        out,
        "<figure class=\"topology\"><svg class=\"sketch topo\" viewBox=\"0 0 {w} {h}\" role=\"img\" \
         aria-label=\"Topology around the element: {} elements, {} net relations\">\
         <defs><marker id=\"mg-arrow\" viewBox=\"0 0 10 10\" refX=\"10\" refY=\"5\" markerWidth=\"7\" markerHeight=\"7\" \
         orient=\"auto-start-reverse\"><path d=\"M0 0L10 5L0 10z\" fill=\"context-stroke\"/></marker></defs>",
        depth.len(),
        rels.len()
    );
    for rel in &rels {
        let (Some(a), Some(b)) = (index(&depth, rel.a), index(&depth, rel.b)) else {
            continue;
        };
        let ((x1, y1), (x2, y2)) = (pos[a], pos[b]);
        // Stop short of the boxes, so that the arrows show.
        let (dx, dy) = (x2 - x1, y2 - y1);
        let len = dx.hypot(dy).max(1e-9);
        let cut = (22.0 / len).min(0.4);
        let (ax, ay, bx, by) = (x1 + dx * cut, y1 + dy * cut, x2 - dx * cut, y2 - dy * cut);
        let end = |e: Option<bool>, n: &str| match e {
            Some(true) => format!("start of {n}"),
            Some(false) => format!("end of {n}"),
            None => n.to_owned(),
        };
        let _ = write!(
            out,
            "<path class=\"edge {}\" d=\"M{ax:.1} {ay:.1}L{bx:.1} {by:.1}\"{}{}><title>{}: {} meets {} ({}, {})</title></path>",
            rel.way.class(),
            if matches!(rel.way, Way::Both | Way::AB) {
                " marker-end=\"url(#mg-arrow)\""
            } else {
                ""
            },
            if matches!(rel.way, Way::Both | Way::BA) {
                " marker-start=\"url(#mg-arrow)\""
            } else {
                ""
            },
            rel.way.text(),
            end(rel.ends.0, "A"),
            end(rel.ends.1, "B"),
            escape(&name_of(&Value::Iri(rel.a.clone()))),
            escape(&name_of(&Value::Iri(rel.b.clone()))),
        );
    }
    for (n, (l, d)) in depth.iter().enumerate() {
        let (x, y) = pos[n];
        let mut name = name_of(&Value::Iri((*l).clone()));
        if name.chars().count() > 26 {
            let chars: Vec<char> = name.chars().collect();
            name = format!(
                "{}\u{2026}{}",
                chars[..12].iter().collect::<String>(),
                chars[chars.len() - 10..].iter().collect::<String>()
            );
        }
        let class = match d {
            0 => "self",
            1 => "near",
            _ => "far",
        };
        let _ = write!(
            out,
            "<a href=\"{}\"><title>{}</title><g class=\"node {class}\"><rect x=\"{:.1}\" y=\"{:.1}\" width=\"120\" height=\"{}\" rx=\"10\"/>\
             <text class=\"label\" x=\"{x:.1}\" y=\"{:.1}\" text-anchor=\"middle\">{}</text>",
            escape(&(links.href)(l.iri.as_str())),
            escape(l.iri.as_str()),
            x - 60.0,
            y - 14.0,
            if lengths[n].is_some() { 34 } else { 24 },
            y + 2.0,
            escape(&name)
        );
        if let Some(m) = lengths[n] {
            let _ = write!(
                out,
                "<text class=\"tick\" x=\"{x:.1}\" y=\"{:.1}\" text-anchor=\"middle\">{}</text>",
                y + 15.0,
                escape(&metres_text(m))
            );
        }
        out.push_str("</g></a>");
    }
    out.push_str("</svg><div class=\"marks\">");
    for (class, text) in [
        ("self", "This element"),
        ("near", "One relation away"),
        ("far", "Further"),
    ] {
        let _ = write!(
            out,
            "<span class=\"mark-label topo-key {class}\">{text}</span>"
        );
    }
    for way in [Way::Both, Way::AB, Way::BA, Way::No, Way::Unknown] {
        let n = rels.iter().filter(|r| r.way == way).count();
        if n > 0 {
            let _ = write!(
                out,
                "<span class=\"mark-label way {}\">{} ({n})</span>",
                way.class(),
                way.text()
            );
        }
    }
    let _ = write!(
        out,
        "</div><figcaption class=\"more\">{}{} Hover a connection for the ends that meet; click an element to open it.</figcaption></figure>",
        if on_map {
            "Elements are where the middle of them lies on the map, north up."
        } else {
            "Elements are in rings by how many relations away they are."
        },
        if owned.len() > MAX_TOPOLOGY_EDGES || depth.len() >= MAX_TOPOLOGY_NODES {
            " Not every relation is drawn."
        } else {
            ""
        }
    );
    true
}

/// Types every object has besides its own: never the one it is listed under.
const GENERIC_TYPES: [&str; 5] = [
    "http://data.europa.eu/949/InfrastructureElement",
    "http://data.europa.eu/949/Feature",
    "http://www.opengis.net/ont/geosparql#Feature",
    "http://data.europa.eu/949/Track",
    "http://data.europa.eu/949/NetElement",
];

/// `rd:PositionedViewer`: what is positioned on a linear element (the row: columns `type`,
/// `on`, `offset` in m). Points on this element are ticks at their offset from its origin, a
/// lane per type, over the element's length (the page's `length` row, else the furthest
/// offset); below, everything by type, with what runs over the element.
fn html_positioned(out: &mut String, links: &PageLinks<'_>, r: &Row, own: &NamedNode) -> bool {
    if r.values.is_empty() {
        return false;
    }
    let kind = |i: usize| {
        let types: Vec<&Link> = cell(r, i, "type")
            .iter()
            .filter_map(|v| match v {
                Value::Iri(l) => Some(l),
                _ => None,
            })
            .collect();
        types
            .iter()
            .find(|l| !GENERIC_TYPES.contains(&l.iri.as_str()))
            .or(types.first())
            .map(|l| name_of(&Value::Iri((*l).clone())))
            .unwrap_or_default()
    };
    let at = |i: usize| {
        let on = cell(r, i, "on")
            .iter()
            .any(|v| matches!(v, Value::Iri(l) if l.iri == *own));
        on.then(|| cell(r, i, "offset").iter().find_map(measure_of))
            .flatten()
    };
    let all: Vec<(usize, String, Option<f64>)> =
        (0..r.values.len()).map(|i| (i, kind(i), at(i))).collect();
    let mut kinds: Vec<String> = all.iter().map(|p| p.1.clone()).collect();
    kinds.sort_by_key(|k| k.to_lowercase());
    kinds.dedup();
    let lane_of = |k: &str| kinds.iter().position(|x| x == k).unwrap_or(0) % 6;
    let length = r
        .refs
        .iter()
        .find(|(k, _)| k == "length")
        .and_then(|(_, l)| l.values.iter().find_map(measure_of))
        .filter(|m| *m > 0.0);
    let points: Vec<&(usize, String, Option<f64>)> = all.iter().filter(|p| p.2.is_some()).collect();
    let span = points
        .iter()
        .filter_map(|p| p.2)
        .fold(length.unwrap_or(0.0), f64::max);
    out.push_str("<figure class=\"positioned\">");
    if !points.is_empty() && span.is_finite() && span > 0.0 {
        let lanes: Vec<&String> = kinds
            .iter()
            .filter(|k| points.iter().any(|p| &p.1 == *k))
            .collect();
        let (w, left, right, top, lane) = (1000.0, 14.0, 14.0, 24.0, 18.0);
        let h = top + 16.0 + lanes.len() as f64 * lane;
        let x = |m: f64| left + (m / span).clamp(0.0, 1.0) * (w - left - right);
        let _ = write!(
            out,
            "<svg class=\"along\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-label=\"{} {} along the element, 0 to {}\">\
             <rect class=\"strip\" x=\"{left}\" y=\"{}\" width=\"{}\" height=\"4\"/>\
             <text class=\"tick\" x=\"{left}\" y=\"12\">origin</text><text class=\"tick\" x=\"{}\" y=\"12\" text-anchor=\"end\">{}</text>",
            points.len(),
            if points.len() == 1 {
                "object"
            } else {
                "objects"
            },
            escape(&metres_text(span)),
            top - 2.0,
            w - left - right,
            w - right,
            escape(&metres_text(span))
        );
        let mut drawn = 0;
        for (n, k) in lanes.iter().enumerate() {
            let y0 = top + 14.0 + n as f64 * lane;
            let _ = write!(
                out,
                "<g class=\"lane-{}\"><line class=\"lane-rule\" x1=\"{left}\" x2=\"{}\" y1=\"{y0}\" y2=\"{y0}\"/>",
                lane_of(k),
                w - right
            );
            for p in points.iter().filter(|p| &&p.1 == k) {
                if drawn >= MAX_MARKERS {
                    break;
                }
                drawn += 1;
                let m = p.2.unwrap_or(0.0);
                let px = x(m);
                let href = match &r.values[p.0] {
                    Value::Iri(l) => (links.href)(l.iri.as_str()),
                    _ => String::new(),
                };
                let _ = write!(
                    out,
                    "<a href=\"{}\"><title>{}: {}, {} from the origin</title><line class=\"mark\" x1=\"{px:.1}\" x2=\"{px:.1}\" y1=\"{}\" y2=\"{y0}\"/>\
                     <circle class=\"dot\" cx=\"{px:.1}\" cy=\"{y0}\" r=\"3.5\"/></a>",
                    escape(&href),
                    escape(k),
                    escape(&name_of(&r.values[p.0])),
                    escape(&metres_text(m)),
                    top + 4.0
                );
            }
            out.push_str("</g>");
        }
        out.push_str("</svg>");
    }
    for k in &kinds {
        let mut of: Vec<&(usize, String, Option<f64>)> = all.iter().filter(|p| &p.1 == k).collect();
        of.sort_by(|a, b| {
            a.2.unwrap_or(f64::INFINITY)
                .total_cmp(&b.2.unwrap_or(f64::INFINITY))
                .then_with(|| natural_cmp(&name_of(&r.values[a.0]), &name_of(&r.values[b.0])))
        });
        let _ = write!(
            out,
            "<details class=\"lane-{}\"{}><summary><span class=\"legend\">{}</span> <span class=\"more\">{}</span></summary><ul>",
            lane_of(k),
            if of.len() <= 12 { " open" } else { "" },
            escape(if k.is_empty() { "Other" } else { k }),
            of.len()
        );
        for p in of {
            out.push_str("<li>");
            html_value(out, links, &r.values[p.0], "LabelViewer");
            match p.2 {
                Some(m) => {
                    let _ = write!(
                        out,
                        " <span class=\"km\">{}</span>",
                        escape(&metres_text(m))
                    );
                }
                None if cell(r, p.0, "offset").is_empty() => {
                    out.push_str(" <span class=\"more\">runs over this element</span>");
                }
                None => {}
            }
            out.push_str("</li>");
        }
        out.push_str("</ul></details>");
    }
    let _ = write!(
        out,
        "<figcaption class=\"more\">{}</figcaption></figure>",
        if points.is_empty() {
            "No object has a position on this element; these refer to it.".to_owned()
        } else {
            format!(
                "{} {} placed at their offset from the element's origin.",
                points.len(),
                if points.len() == 1 { "point" } else { "points" }
            )
        }
    );
    true
}

/// Days since 1970-01-01 of a date written `YYYY-MM-DD...` (an `xsd:date` or `dateTime`).
fn day_of(text: &str) -> Option<i64> {
    let t = text.trim();
    let (y, rest) = if let Some(r) = t.strip_prefix('-') {
        let (y, rest) = r.split_once('-')?;
        (-y.parse::<i64>().ok()?, rest)
    } else {
        let (y, rest) = t.split_once('-')?;
        (y.parse::<i64>().ok()?, rest)
    };
    if !(-1_000_000..=1_000_000).contains(&y) {
        return None;
    }
    let m: i64 = rest.get(0..2)?.parse().ok()?;
    let d: i64 = rest.get(3..5)?.parse().ok()?;
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days_in_month = match m {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if rest.as_bytes().get(2) != Some(&b'-')
        || !(1..=12).contains(&m)
        || !(1..=days_in_month).contains(&d)
    {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// Lines of a changes widget at most.
const MAX_CHANGES: usize = 200;

/// `rd:ChangesViewer`: parameters that apply from or until a date (the row's columns
/// `parameter`, `value`, `begin`, `end`) on a time axis from today (or the first begin) to a
/// year on (or the last end), with today marked, a line per parameter and value, by name and
/// begin.
fn html_changes(out: &mut String, links: &PageLinks<'_>, r: &Row) -> bool {
    let today = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| (d.as_secs() / 86_400) as i64);
    html_changes_at(out, links, r, today)
}

fn html_changes_at(out: &mut String, links: &PageLinks<'_>, r: &Row, today: i64) -> bool {
    let date = |i: usize, k: &str| {
        cell(r, i, k)
            .iter()
            .find_map(|v| text_of(v).and_then(|t| day_of(t).map(|d| (d, t.trim()))))
    };
    let mut lines: Vec<usize> = (0..r.values.len())
        .filter(|&i| !cell(r, i, "parameter").is_empty())
        .collect();
    if lines.is_empty() {
        return false;
    }
    let name = |i: usize| name_of(&cell(r, i, "parameter")[0]);
    lines.sort_by(|a, b| {
        name(*a)
            .to_lowercase()
            .cmp(&name(*b).to_lowercase())
            .then_with(|| {
                date(*a, "begin")
                    .map(|d| d.0)
                    .cmp(&date(*b, "begin").map(|d| d.0))
            })
    });
    // The axis holds today, a year on, and every begin and end drawn.
    let drawn = &lines[..lines.len().min(MAX_CHANGES)];
    let dates = || {
        drawn
            .iter()
            .flat_map(|&i| [date(i, "begin"), date(i, "end")])
            .flatten()
            .map(|d| d.0)
            .chain([today, today + 365])
    };
    let lo = dates().min().unwrap_or(today);
    let hi = dates().max().unwrap_or(today + 365);
    // A change without an end runs on past the last date.
    let open = drawn.iter().any(|&i| date(i, "end").is_none());
    let hi = if open {
        hi + ((hi - lo) / 10).max(30)
    } else {
        hi
    };
    let pct = |d: i64| ((d - lo) as f64 / (hi - lo).max(1) as f64 * 100.0).clamp(0.0, 100.0);
    out.push_str("<ul class=\"changes\">");
    for &i in lines.iter().take(MAX_CHANGES) {
        let (b, e) = (date(i, "begin"), date(i, "end"));
        let (from, to) = (b.map_or(lo, |d| d.0), e.map_or(hi, |d| d.0));
        // An end before its begin: drawn over the two, as given.
        let (from, to) = (from.min(to), from.max(to));
        let state = if from > today {
            "future"
        } else if to < today {
            "past"
        } else {
            "now"
        };
        let _ = write!(
            out,
            "<li class=\"change {state}\"><span class=\"change-name\">"
        );
        html_value(out, links, &cell(r, i, "parameter")[0], "LabelViewer");
        if let Some(v) = cell(r, i, "value").first() {
            out.push_str(" <span class=\"chip\">");
            html_value(out, links, v, "LabelViewer");
            out.push_str("</span>");
        }
        let _ = write!(
            out,
            "</span><span class=\"change-axis\"><span class=\"change-bar\" style=\"left:{:.2}%;width:{:.2}%\"></span>\
             <span class=\"change-today\" style=\"left:{:.2}%\" title=\"Today\"></span></span><span class=\"more\">",
            pct(from),
            (pct(to) - pct(from)).max(0.5).min(100.0 - pct(from)),
            pct(today)
        );
        let mut when = Vec::new();
        if let Some((_, t)) = b {
            when.push(format!("from {}", t.split('T').next().unwrap_or(t)));
        }
        if let Some((_, t)) = e {
            when.push(format!("until {}", t.split('T').next().unwrap_or(t)));
        }
        let _ = write!(out, "{}</span></li>", escape(&when.join(" ")));
    }
    out.push_str("</ul>");
    if lines.len() > MAX_CHANGES || r.more > 0 {
        let _ = write!(
            out,
            "<div class=\"more\">{} of the changes read are drawn; there are more.</div>",
            drawn.len()
        );
    }
    true
}

/// EU authority country codes (ISO 3166-1 alpha-3) of the RINF countries and their
/// neighbours, with their two-letter codes.
const COUNTRIES: [(&str, &str); 48] = [
    ("AUT", "AT"),
    ("BEL", "BE"),
    ("BGR", "BG"),
    ("HRV", "HR"),
    ("CYP", "CY"),
    ("CZE", "CZ"),
    ("DNK", "DK"),
    ("EST", "EE"),
    ("FIN", "FI"),
    ("FRA", "FR"),
    ("DEU", "DE"),
    ("GRC", "GR"),
    ("HUN", "HU"),
    ("IRL", "IE"),
    ("ITA", "IT"),
    ("LVA", "LV"),
    ("LTU", "LT"),
    ("LUX", "LU"),
    ("MLT", "MT"),
    ("NLD", "NL"),
    ("POL", "PL"),
    ("PRT", "PT"),
    ("ROU", "RO"),
    ("SVK", "SK"),
    ("SVN", "SI"),
    ("ESP", "ES"),
    ("SWE", "SE"),
    ("NOR", "NO"),
    ("CHE", "CH"),
    ("LIE", "LI"),
    ("ISL", "IS"),
    ("GBR", "GB"),
    ("MNE", "ME"),
    ("MKD", "MK"),
    ("SRB", "RS"),
    ("ALB", "AL"),
    ("BIH", "BA"),
    ("UKR", "UA"),
    ("MDA", "MD"),
    ("BLR", "BY"),
    ("RUS", "RU"),
    ("TUR", "TR"),
    ("AND", "AD"),
    ("MCO", "MC"),
    ("SMR", "SM"),
    ("XKX", "XK"),
    ("GEO", "GE"),
    ("ARM", "AM"),
];

/// The flag of a country by its EU authority IRI (`.../country/SWE`), as two regional
/// indicator letters; a white flag otherwise.
fn flag_of(v: &Value) -> String {
    let code = match v {
        Value::Iri(l) => l
            .iri
            .as_str()
            .rsplit_once("/country/")
            .and_then(|(_, c)| COUNTRIES.iter().find(|(a, _)| *a == c))
            .map(|(_, two)| *two),
        _ => None,
    };
    code.map_or_else(
        || "\u{1f3f3}".to_owned(),
        |c| {
            c.bytes()
                .filter_map(|b| char::from_u32(0x1f1e6 + u32::from(b - b'A')))
                .collect()
        },
    )
}

/// The way a track is normally run against its kilometres (`N`/`Normal`, `O`/`Opposite`,
/// `B`/`Both`, or the codes of track-running-directions 10, 20, 30 and orientations 00, 01).
fn direction_of(v: &Value) -> Option<&'static str> {
    let label = name_of(v).trim().to_ascii_uppercase();
    let code = match v {
        Value::Iri(l) => {
            let s = l.iri.as_str();
            let last = s.rsplit('/').next().unwrap_or("");
            let parent = s.rsplit('/').nth(1).unwrap_or("");
            let known = matches!(
                parent,
                "track-running-directions" | "TrackRunningDirections" | "orientations"
            );
            known.then_some(last)
        }
        _ => None,
    };
    match (label.as_str(), code) {
        ("N" | "NORMAL", _) | (_, Some("10" | "00")) => Some("normal"),
        ("O" | "OPPOSITE", _) | (_, Some("20" | "01")) => Some("opposite"),
        ("B" | "BOTH", _) | (_, Some("30")) => Some("both"),
        _ => None,
    }
}

/// Current, voltage and frequency of an energy supply system ("AC 25kV-50Hz", "DC 750V").
fn supply_of(label: &str) -> Option<(bool, f64, Option<f64>)> {
    let t = label.trim();
    let ac = match t.get(..2)?.to_ascii_uppercase().as_str() {
        "AC" => true,
        "DC" => false,
        _ => return None,
    };
    let rest = t[2..].trim_start();
    let digits = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    let mut volts: f64 = rest[..digits].parse().ok()?;
    let rest = rest[digits..].trim_start();
    let rest = if let Some(r) = rest.strip_prefix(['k', 'K']) {
        volts *= 1000.0;
        r
    } else {
        rest
    };
    let rest = rest.strip_prefix(['V', 'v'])?.trim_start();
    let hz = rest.strip_prefix('-').and_then(|r| {
        let r = r.trim_start();
        let n = r
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(r.len());
        r[..n].parse().ok()
    });
    Some((ac, volts, hz))
}

/// The colour OpenRailwayMap draws an energy supply system with.
fn supply_colour(s: Option<(bool, f64, Option<f64>)>) -> &'static str {
    match s {
        None => "var(--muted)",
        Some((true, _, Some(hz))) if hz < 20.0 => "#2e8b3e",
        Some((true, _, _)) => "#c4262e",
        Some((false, v, _)) if v >= 3000.0 => "#1f5fbf",
        Some((false, v, _)) if v >= 1500.0 => "#8a3fb5",
        Some(_) => "#e07a10",
    }
}

/// `rd:SymbolViewer`: an illustrative, non-dimensioned symbol beside each value (`rd:symbol`
/// `loading-gauge`, the default; `track-gauge`; `country`, its flag; `platform-height`, a
/// platform beside a rail at its height; `direction`, against the kilometres;
/// `energy-supply`, a bolt in OpenRailwayMap's colour of the system).
fn html_symbols(out: &mut String, links: &PageLinks<'_>, r: &Row) -> bool {
    if r.values.is_empty() {
        return false;
    }
    let kind = r
        .settings
        .iter()
        .find(|(k, _)| k == "symbol")
        .map_or("loading-gauge", |(_, v)| v.as_str());
    out.push_str("<div class=\"symbols\">");
    for v in &r.values {
        out.push_str("<figure class=\"symbol-tile\">");
        const SVG: &str = "<svg class=\"symbol\" viewBox=\"0 0 64 72\" aria-hidden=\"true\">";
        match kind {
            "country" => {
                let _ = write!(
                    out,
                    "<span class=\"symbol flag\" aria-hidden=\"true\">{}</span>",
                    flag_of(v)
                );
            }
            "track-gauge" => {
                let _ = write!(
                    out,
                    "{SVG}<path class=\"base\" d=\"M6 54h52\"/><path class=\"rail-head\" d=\"M14 44h6v10h-6zM44 44h6v10h-6z\"/>\
                     <path class=\"outline\" d=\"M20 30h24M20 30l5-4M20 30l5 4M44 30l-5-4M44 30l-5 4M20 24v16M44 24v16\"/></svg>"
                );
            }
            "platform-height" => {
                let mm = height_mm(&name_of(v));
                let top = 58.0 - mm.unwrap_or(400.0).min(1000.0) * 0.04;
                let _ = write!(
                    out,
                    "{SVG}<path class=\"base\" d=\"M4 62h56\"/><path class=\"rail-head\" d=\"M42 52h6v6h-6z\"/>\
                     <path class=\"outline{}\" d=\"M6 62V{top:.1}h28V62\"/></svg>",
                    if mm.is_none() { " dashed" } else { "" }
                );
            }
            "direction" => {
                let d = direction_of(v);
                let head =
                    |x: i32, dir: i32| format!("M{x} 36l{} -8M{x} 36l{} 8", -10 * dir, -10 * dir);
                let path = format!(
                    "M8 36H56{}{}",
                    if d != Some("opposite") {
                        head(56, 1)
                    } else {
                        String::new()
                    },
                    if matches!(d, Some("opposite" | "both")) {
                        head(8, -1)
                    } else {
                        String::new()
                    }
                );
                let _ = write!(
                    out,
                    "{SVG}<path class=\"base\" d=\"M6 52h52\"/><path class=\"outline{}\" d=\"{path}\"/></svg>",
                    if d.is_none() { " dashed" } else { "" }
                );
            }
            "energy-supply" => {
                let s = supply_of(&name_of(v));
                let _ = write!(
                    out,
                    "{SVG}<circle cx=\"32\" cy=\"34\" r=\"24\" style=\"fill:{}\"/><path d=\"M35 14L22 38h10l-4 20 14-26H32z\" style=\"fill:#fff\"/>\
                     <path class=\"outline\" d=\"{}\"/></svg>",
                    supply_colour(s),
                    if s.is_some_and(|s| s.0) {
                        "M18 64q4-5 8 0t8 0 8 0 8 0"
                    } else {
                        "M18 64h28"
                    }
                );
            }
            _ => {
                let _ = write!(
                    out,
                    "{SVG}<path class=\"outline\" d=\"M14 60V22c0-8 6-14 14-14h8c8 0 14 6 14 14v38\"/>\
                     <path class=\"base\" d=\"M8 60h48\"/><path class=\"rail-head\" d=\"M18 66h6M40 66h6\"/></svg>"
                );
            }
        }
        out.push_str("<figcaption>");
        html_value(out, links, v, "LabelViewer");
        out.push_str("</figcaption></figure>");
    }
    out.push_str("</div>");
    true
}

/// `rd:DataStatusViewer`: the parameters the data marks `era:notApplicable` and
/// `era:notYetAvailable`, each list folded, by name.
fn html_data_status(out: &mut String, links: &PageLinks<'_>, r: &Row) -> bool {
    let mut drawn = false;
    for (k, note) in DATA_STATUS {
        let Some((_, values)) = r.params.iter().find(|(n, v)| n == k && !v.is_empty()) else {
            continue;
        };
        let mut sorted: Vec<&Value> = values.iter().collect();
        sorted.sort_by_key(|v| name_of(v).to_lowercase());
        let _ = write!(
            out,
            "<details class=\"data-status\"><summary>{} <span class=\"more\">({})</span></summary><div class=\"more\">{note}</div><div class=\"chips\">",
            if k == "notApplicable" {
                "Not applicable"
            } else {
                "Not yet available"
            },
            r.settings
                .iter()
                .find(|(n, _)| n == k)
                .and_then(|(_, n)| n.parse().ok())
                .unwrap_or(values.len())
        );
        for v in sorted {
            out.push_str("<span class=\"chip\">");
            html_value(out, links, v, "LabelViewer");
            out.push_str("</span>");
        }
        out.push_str("</div></details>");
        drawn = true;
    }
    drawn
}

/// `rd:ParameterIndexViewer`: the RINF parameters as a table by index, under a heading line for
/// each chapter they are in.
fn html_parameters(out: &mut String, links: &PageLinks<'_>, r: &Row) -> bool {
    if r.table.is_none() || r.values.is_empty() {
        return false;
    }
    let given = (0..r.values.len())
        .filter(|&i| cell(r, i, "status").is_empty())
        .count();
    let marked = r.values.len() - given;
    let _ = write!(
        out,
        "<div class=\"more\">{given} {} given{}</div>",
        if given == 1 {
            "parameter"
        } else {
            "parameters"
        },
        if marked > 0 {
            format!(", {marked} marked not applicable or not yet available")
        } else {
            String::new()
        }
    );
    out.push_str(
        "<table class=\"nested values params\"><tr><th>Index</th><th>Parameter</th><th>Value</th></tr>",
    );
    let mut open: Vec<String> = Vec::new();
    for (i, v) in r.values.iter().enumerate() {
        let chain: Vec<(&str, &str)> = ["chapter", "section"]
            .iter()
            .filter_map(|k| match cell(r, i, k) {
                [ix, label] => Some((text_of(ix)?, text_of(label)?)),
                _ => None,
            })
            .collect();
        if chain.is_empty() {
            open.clear();
        }
        for (depth, (ix, label)) in chain.iter().enumerate() {
            if open.get(depth).is_some_and(|o| o == ix) {
                continue;
            }
            open.truncate(depth);
            open.push((*ix).to_owned());
            let _ = write!(
                out,
                "<tr class=\"param-chapter\"><td class=\"more\">{}</td><th colspan=\"2\">{}</th></tr>",
                escape(ix),
                escape(label)
            );
        }
        out.push_str("<tr><td class=\"more\">");
        if let Some(ix) = cell(r, i, "index").first().and_then(text_of) {
            out.push_str(&escape(ix));
        }
        out.push_str("</td><td>");
        html_value(out, links, v, "LabelViewer");
        out.push_str("</td><td>");
        for (n, value) in cell(r, i, "value").iter().enumerate() {
            if n > 0 {
                out.push_str(", ");
            }
            html_value(out, links, value, "LabelViewer");
        }
        if let Some(u) = cell(r, i, "unit").first().and_then(text_of) {
            let _ = write!(out, " <span class=\"unit\">{}</span>", escape(u));
        }
        if let Some(on) = cell(r, i, "on").first() {
            out.push_str(" <span class=\"more\">on ");
            html_value(out, links, on, "LabelViewer");
            out.push_str("</span>");
        }
        if let Some(status) = cell(r, i, "status").first().and_then(text_of) {
            let _ = write!(out, "<span class=\"chip\">{}</span>", escape(status));
        }
        out.push_str("</td></tr>");
    }
    out.push_str("</table>");
    true
}

/// Steps of a step chart drawn at most.
const MAX_STEPS: usize = 2_000;

/// A profile written `value(length),value(length),...`, or none when a part is not.
fn parse_steps(text: &str) -> Option<Vec<(f64, f64)>> {
    let steps: Vec<(f64, f64)> = text
        .split([',', ';'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (v, rest) = p.split_once('(')?;
            let len = rest.strip_suffix(')')?;
            let (v, len): (f64, f64) = (v.trim().parse().ok()?, len.trim().parse().ok()?);
            (v.is_finite() && len.is_finite() && len >= 0.0).then_some((v, len))
        })
        .collect::<Option<_>>()?;
    (!steps.is_empty() && steps.len() <= MAX_STEPS).then_some(steps)
}

/// `rd:StepChartViewer`: a profile (`value(length),...`, a gradient profile) as a step
/// chart along the distance, rising and falling steps apart.
fn html_step_chart(out: &mut String, r: &Row) -> bool {
    let Some(steps) = r.values.iter().find_map(|v| match v {
        Value::Literal(l) => parse_steps(l.value()),
        _ => None,
    }) else {
        return false;
    };
    let unit = r.unit.as_deref().unwrap_or("");
    let length_unit = r
        .settings
        .iter()
        .find(|(k, _)| k == "lengthUnit")
        .map_or("", |(_, v)| v.as_str());
    let total: f64 = steps.iter().map(|s| s.1).sum();
    if total <= 0.0 {
        return false;
    }
    let peak = steps.iter().map(|s| s.0.abs()).fold(1e-9, f64::max);
    let (w, h, left, right, top, bottom) = (640.0, 160.0, 44.0, 8.0, 10.0, 24.0);
    let x = |d: f64| left + d / total * (w - left - right);
    let y = |v: f64| top + (1.0 - (v + peak) / (2.0 * peak)) * (h - top - bottom);
    let _ = write!(
        out,
        "<svg class=\"step-chart\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-label=\"Profile over {} {}, from {} to {} {}\">",
        number_text(total),
        escape(length_unit),
        number_text(-peak),
        number_text(peak),
        escape(unit)
    );
    for v in [peak, 0.0, -peak] {
        let _ = write!(
            out,
            "<line class=\"axis\" x1=\"{left}\" x2=\"{x2}\" y1=\"{yv:.1}\" y2=\"{yv:.1}\"/>\
             <text class=\"tick\" x=\"{tx}\" y=\"{ty:.1}\" text-anchor=\"end\">{sign}{n}</text>",
            x2 = w - right,
            yv = y(v),
            tx = left - 4.0,
            ty = y(v) + 4.0,
            sign = if v > 0.0 { "+" } else { "" },
            n = number_text(v)
        );
    }
    let _ = write!(
        out,
        "<text class=\"tick\" x=\"{left}\" y=\"{}\">0 {}</text>\
         <text class=\"tick\" x=\"{}\" y=\"{}\" text-anchor=\"end\">{} {}</text>",
        h - 6.0,
        escape(length_unit),
        w - right,
        h - 6.0,
        number_text(total),
        escape(length_unit)
    );
    let mut at = 0.0;
    for (v, len) in &steps {
        let (x0, x1) = (x(at), x(at + len));
        let (y0, y1) = (y(*v).min(y(0.0)), y(*v).max(y(0.0)));
        let class = if *v > 0.0 {
            "up"
        } else if *v < 0.0 {
            "down"
        } else {
            "flat"
        };
        let _ = write!(
            out,
            "<rect class=\"bar {class}\" x=\"{x0:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\">\
             <title>{}{} {} over {} {}, from {} {}</title></rect>",
            if *v == 0.0 { y(0.0) - 0.75 } else { y0 },
            (x1 - x0).max(0.5),
            if *v == 0.0 { 1.5 } else { (y1 - y0).max(1.0) },
            if *v > 0.0 { "+" } else { "" },
            number_text(*v),
            escape(unit),
            number_text(*len),
            escape(length_unit),
            number_text(at),
            escape(length_unit)
        );
        at += len;
    }
    out.push_str("</svg>");
    true
}

/// Levels of a concept tree drawn at most.
const MAX_TREE_DEPTH: usize = 32;

/// `rd:ConceptTreeViewer`: the concepts of the row as a tree by their "broader" cells,
/// each with its "notation"; the page's own concept stands out.
fn html_concept_tree(out: &mut String, links: &PageLinks<'_>, r: &Row, own: &NamedNode) -> bool {
    if r.table.is_none() {
        return false;
    }
    let nodes: Vec<(usize, &Link)> = r
        .values
        .iter()
        .enumerate()
        .filter_map(|(i, v)| match v {
            Value::Iri(l) => Some((i, l)),
            _ => None,
        })
        .collect();
    if nodes.is_empty() {
        return false;
    }
    let index: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(n, (_, l))| (l.iri.as_str(), n))
        .collect();
    let notations: Vec<Option<String>> = nodes
        .iter()
        .map(|(i, _)| {
            cell(r, *i, "notation").iter().find_map(|v| match v {
                Value::Literal(l) => Some(l.value().to_owned()),
                _ => None,
            })
        })
        .collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    let mut has_parent = vec![false; nodes.len()];
    for (n, (i, l)) in nodes.iter().enumerate() {
        let parent = cell(r, *i, "broader").iter().find_map(|b| match b {
            Value::Iri(p) if p.iri != l.iri => index.get(p.iri.as_str()).copied(),
            _ => None,
        });
        if let Some(p) = parent {
            children[p].push(n);
            has_parent[n] = true;
        }
    }
    let key = |n: usize| {
        (
            notations[n].clone().unwrap_or_default(),
            name_of(&r.values[nodes[n].0]),
        )
    };
    for c in &mut children {
        c.sort_by_cached_key(|&n| key(n));
    }
    let mut roots: Vec<usize> = (0..nodes.len()).filter(|&n| !has_parent[n]).collect();
    roots.sort_by_cached_key(|&n| key(n));
    let tree = Tree {
        nodes: &nodes,
        children: &children,
        notations: &notations,
        own,
    };
    let mut seen = vec![false; nodes.len()];
    out.push_str("<ul class=\"tree\">");
    for n in roots {
        tree.draw(out, links, n, 0, &mut seen);
    }
    // Concepts only on a cycle of broader links have no root: drawn after, at the top.
    for n in 0..nodes.len() {
        if !seen[n] {
            tree.draw(out, links, n, 0, &mut seen);
        }
    }
    out.push_str("</ul>");
    let others = r.values.len() - nodes.len();
    if others > 0 {
        let _ = write!(
            out,
            "<div class=\"more\">{others} value{} not a concept (not an IRI) and not in the tree.</div>",
            if others == 1 { " is" } else { "s are" }
        );
    }
    true
}

fn web(iri: &str) -> bool {
    let lower = iri.get(..8).unwrap_or(iri).to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://")
}

fn viewer_for(v: &Value) -> &'static str {
    viewer(None, std::slice::from_ref(v))
}

const STYLE: &str = "\
:root{--fg:#1d2433;--muted:#5b6475;--line:#e3e6ec;--bg:#fff;--chip:#eef2f8;--accent:#1f5fbf}\
@media (prefers-color-scheme:dark){:root{--fg:#e6e9ef;--muted:#9aa3b2;--line:#2c3240;--bg:#151922;--chip:#222937;--accent:#7fb0ff}}\
body{font-family:system-ui,sans-serif;margin:0;color:var(--fg);background:var(--bg);line-height:1.45}\
main{max-width:64rem;margin:0 auto;padding:1.25rem 1rem 3rem}\
h1{font-size:1.5rem;margin:.2rem 0;word-break:break-word}h2{font-size:1.05rem;margin:1.6rem 0 .4rem;\
border-bottom:1px solid var(--line);padding-bottom:.25rem}\
a{color:var(--accent);text-decoration:none}a:hover{text-decoration:underline}\
.iri{color:var(--muted);font-size:.85rem;word-break:break-all}\
.chips{margin:.5rem 0}.chip{display:inline-block;background:var(--chip);border-radius:1rem;padding:.1rem .6rem;\
margin:0 .3rem .3rem 0;font-size:.85rem}\
.docs{font-size:.8rem;color:var(--muted)}.docs a{margin-right:.5rem}\
table{border-collapse:collapse;width:100%}th,td{text-align:left;vertical-align:top;padding:.3rem .5rem;\
border-bottom:1px solid var(--line)}th{width:28%;font-weight:500;color:var(--muted)}\
td{word-break:break-word}td ul{margin:0;padding-left:1.1rem}.dt{color:var(--muted)}\
.doc-cards{display:flex;flex-wrap:wrap;gap:.5rem}.doc-card{border:1px solid var(--line);border-radius:.4rem;padding:.4rem .6rem}\
.ftype{font-size:.7rem;font-weight:600;background:var(--chip);border-radius:.2rem;padding:0 .3rem;margin-right:.4rem}.doc-of{font-size:.8rem}\
.scalebar{max-width:28rem}.track{position:relative;height:.6rem;background:var(--chip);border-radius:.3rem;margin:.3rem 0}\
.fill{position:absolute;top:0;bottom:0;left:0;background:var(--accent);border-radius:.3rem}.mark{position:absolute;top:-.2rem;bottom:-.2rem;width:2px;background:var(--fg)}\
.scale-ends{display:flex;justify-content:space-between;font-size:.8rem;color:var(--muted)}.marks{font-size:.85rem;color:var(--muted)}\
.mark-label{margin-right:.8rem}.mark-label.reached{color:var(--fg)}.mark-label.reached::before{content:'\\2713  '}\
.facilities{display:flex;flex-wrap:wrap;gap:.4rem}.facility{border:1px solid var(--line);border-radius:.3rem;padding:.2rem .5rem}\
.facility.yes .flag{color:#2e8540}.facility.no .flag{color:#c0392b}.facility.unknown{color:var(--muted)}\
.figures{display:flex;flex-wrap:wrap;gap:.6rem}.figure{border:1px solid var(--line);border-radius:.4rem;padding:.4rem .8rem;min-width:7rem}\
.figure-value{font-size:1.4rem;font-weight:600}.figure-label{font-size:.8rem;color:var(--muted)}\
.live-map{width:100%;max-width:56rem;height:22rem;border:1px solid var(--line);border-radius:.4rem}\
.km{font-family:monospace;color:var(--muted)}.route{display:flex;align-items:center;gap:.6rem;flex-wrap:wrap}.route-line{flex:1;min-width:6rem;border-top:3px solid var(--accent);text-align:center;font-size:.85rem;color:var(--muted);padding-top:.2rem}\
.along{width:100%;max-width:40rem;height:auto}.along .strip{fill:var(--line)}.along .tick{font-size:10px;fill:var(--muted)}\
.along .post{stroke:var(--muted)}.along .lane-rule{stroke:var(--line)}.along .dot{fill:var(--lane)}\
.topo .edge{fill:none;stroke:var(--accent);stroke-width:2}.topo .edge.none{stroke:#c0392b;stroke-dasharray:6 5}.topo .edge.unknown{stroke:var(--muted);stroke-dasharray:2 4}\
.topo .node rect{fill:var(--bg);stroke:var(--muted);stroke-width:1.5}.topo .node.near rect{stroke:var(--accent)}.topo .node.self rect{stroke:#c0392b;stroke-width:3}.topo .node.far .label{fill:var(--muted)}\
.along .stop{fill:var(--bg);stroke:var(--fg);stroke-width:2}.along .here{fill:#c0392b}\
.lane-0{--lane:#1f77b4}.lane-1{--lane:#d62728}.lane-2{--lane:#2ca02c}.lane-3{--lane:#9467bd}.lane-4{--lane:#ff7f0e}.lane-5{--lane:#8c564b}\
.along rect.mark{fill:var(--lane);opacity:.7}.along line.mark{stroke:var(--lane);stroke-width:2}.legend{margin-right:.8rem}.legend::before{content:'';display:inline-block;width:.7rem;height:.7rem;margin-right:.3rem;background:var(--lane)}\
.profile{width:100%;height:auto}.profile .lane-label{font-size:11px;fill:var(--fg)}.profile .tick{font-size:10px;fill:var(--muted)}\
.profile .op{stroke:var(--line)}.profile .step{fill:var(--accent);opacity:.8}.profile rect.band{fill:var(--lane);opacity:.75}\
ol.stops{list-style:none;padding-left:0;margin:0}ol.stops li{padding:.1rem 0}.step-chart{width:100%;max-width:40rem;height:auto}\
.step-chart .axis{stroke:var(--line)}.step-chart .tick{font-size:10px;fill:var(--muted)}\
.step-chart .up{fill:#2e8540}.step-chart .down{fill:#c0392b}.step-chart .flat{fill:var(--muted)}\
ul.tree,ul.tree ul{list-style:none;padding-left:1rem;margin:0}ul.tree{padding-left:0}.notation{font-family:monospace;color:var(--muted)}\
table.nested{width:auto;font-size:.9rem}table.nested th{width:auto}img{max-width:20rem;max-height:14rem}\
.more{color:var(--muted);font-size:.85rem}.unit{color:var(--muted)}\
.badges{margin:.4rem 0}.badge{display:inline-block;border:1px solid var(--line);border-radius:.3rem;\
padding:.05rem .5rem;margin:0 .3rem .3rem 0;font-size:.85rem}\
h2.tab{font-size:1.2rem;border-bottom:2px solid var(--accent)}h3{font-size:1rem;margin:1.2rem 0 .3rem}\
.sketch{width:100%;max-width:40rem;height:auto}.sketch .tick{font-size:11px;fill:var(--muted)}.sketch .label{font-size:12px;fill:var(--fg)}\
.sketch .strong{font-weight:600}.sketch .rail{stroke:var(--fg);stroke-width:3}.sketch .ref{stroke:var(--muted);stroke-dasharray:4 4}\
.sketch .ref.hit{stroke:var(--accent);stroke-dasharray:none}.sketch .block{fill:var(--chip);stroke:var(--fg)}.sketch .unknown{stroke-dasharray:5 4;fill-opacity:.5}\
.sketch .ballast{fill:var(--line)}.sketch .sleeper{fill:var(--muted)}.sketch .rail-head{fill:var(--fg)}.sketch .measure{stroke:var(--muted);fill:none}\
.sketch .profile-outline{fill:none;stroke:var(--accent);stroke-width:2}.sketch .wire-band{fill:var(--chip)}.sketch .wire{stroke:#c0392b;stroke-width:2}\
.sketch .conn{stroke:var(--accent);stroke-width:4}.sketch .stop{fill:var(--bg);stroke:var(--fg);stroke-width:2}.sketch .hub{fill:var(--fg)}.sketch .hub-ring{fill:none;stroke:var(--fg);stroke-width:2}\
ol.tl-tracks{list-style:none;padding:0;margin:0}.tl-track{display:flex;align-items:center;gap:.6rem;padding:.15rem 0}.tl-id{min-width:5rem;font-size:.9rem}\
.tl-lane{position:relative;flex:1;min-height:1.4rem;display:flex;align-items:center}.tl-rail{position:absolute;left:0;right:0;top:50%;border-top:3px solid var(--fg)}\
.tl-platform{position:relative;background:var(--chip);border:1px solid var(--accent);border-radius:.2rem;font-size:.75rem;padding:0 .3rem;margin-right:.3rem;white-space:nowrap;overflow:hidden}\
ul.tl-sidings{list-style:none;padding:0;margin:0}.tl-siding{display:flex;align-items:center;gap:.5rem}.tl-bar{display:inline-block;height:.4rem;background:var(--muted);border-radius:.2rem;max-width:50%}\
ul.changes{list-style:none;padding:0;margin:0}.change{display:grid;grid-template-columns:minmax(8rem,1fr) 2fr auto;gap:.6rem;align-items:center;padding:.15rem 0}\
.change-axis{position:relative;height:.6rem;background:var(--chip);border-radius:.3rem}.change-bar{position:absolute;top:0;bottom:0;background:var(--accent);border-radius:.3rem}\
.change.past .change-bar{background:var(--muted)}.change.future .change-bar{opacity:.5}.change-today{position:absolute;top:-.25rem;bottom:-.25rem;width:2px;background:#c0392b}\
.symbols{display:flex;flex-wrap:wrap;gap:.6rem}.symbol-tile{margin:0;text-align:center;font-size:.85rem}svg.symbol{width:3.2rem;height:3.6rem;display:block;margin:0 auto}\
.symbol .outline{fill:none;stroke:var(--fg);stroke-width:2.5}.symbol .dashed{stroke-dasharray:4 3}.symbol .base{stroke:var(--muted);stroke-width:2}.symbol .rail-head{fill:var(--fg);stroke:var(--fg)}\
.symbol.flag{font-size:2.4rem;display:block;line-height:1.2}\
svg.map{width:100%;max-height:22rem;background:var(--chip);border-radius:.4rem}\
svg.map circle{fill:var(--accent)}svg.map .line{fill:none;stroke:var(--accent);stroke-width:2.5}\
svg.map .area{fill:var(--accent);fill-opacity:.25;stroke:var(--accent);stroke-width:1.5}\
svg.map .context circle{fill:var(--muted)}svg.map .context .line{stroke:var(--muted);stroke-width:2}\
svg.map .context .area{fill:var(--muted);fill-opacity:.12;stroke:var(--muted)}";

/// The map of the geometries: an SVG drawing of them (no base map, so the page needs no tiles
/// from elsewhere), and for geographic coordinates a link to OpenStreetMap at their centre.
/// Coordinates of the interactive map's GeoJSON at most, in all.
const MAX_LIVE_POINTS: usize = 20_000;

/// A geometry as GeoJSON, about every `step`th point of a line or ring (with its ends).
fn geojson(g: &geo::Geometry<f64>, step: usize) -> Option<Json> {
    use geo::Geometry as G;
    let pt = |c: geo::Coord<f64>| json!([c.x, c.y]);
    let line = |l: &geo::LineString<f64>, min: usize| -> Json {
        let n = l.0.len();
        // A short ring or line keeps `min` points at least (4 for a ring: a valid one).
        let step = step.min((n.saturating_sub(1) / (min - 1).max(1)).max(1));
        let keep = |i: usize| i.is_multiple_of(step) || i + 1 == n || n <= min;
        Json::Array(
            l.0.iter()
                .enumerate()
                .filter(|(i, _)| keep(*i))
                .map(|(_, c)| pt(*c))
                .collect(),
        )
    };
    let polygon = |p: &geo::Polygon<f64>| -> Json {
        Json::Array(
            std::iter::once(p.exterior())
                .chain(p.interiors())
                .map(|r| line(r, 8))
                .collect(),
        )
    };
    Some(match g {
        G::Point(p) => json!({"type": "Point", "coordinates": pt(p.0)}),
        G::MultiPoint(m) => {
            json!({"type": "MultiPoint", "coordinates": m.0.iter().map(|p| pt(p.0)).collect::<Vec<_>>()})
        }
        G::Line(l) => json!({"type": "LineString", "coordinates": [pt(l.start), pt(l.end)]}),
        G::LineString(l) => json!({"type": "LineString", "coordinates": line(l, 4)}),
        G::MultiLineString(m) => json!({
            "type": "MultiLineString",
            "coordinates": m.0.iter().map(|l| line(l, 4)).collect::<Vec<_>>(),
        }),
        G::Polygon(p) => json!({"type": "Polygon", "coordinates": polygon(p)}),
        G::MultiPolygon(m) => json!({
            "type": "MultiPolygon",
            "coordinates": m.0.iter().map(polygon).collect::<Vec<_>>(),
        }),
        G::Rect(r) => json!({"type": "Polygon", "coordinates": polygon(&r.to_polygon())}),
        G::Triangle(t) => json!({"type": "Polygon", "coordinates": polygon(&t.to_polygon())}),
        G::GeometryCollection(c) => json!({
            "type": "GeometryCollection",
            "geometries": c.0.iter().filter_map(|g| geojson(g, step)).collect::<Vec<_>>(),
        }),
    })
}

/// The geometries (`false`) and those of their context (`true`) as a GeoJSON feature
/// collection in longitude/latitude with its `bbox`, the context's marked `context`; none when
/// one of them cannot be put in longitude/latitude.
fn live_map_data(geometries: &[(&Geometry, bool)]) -> Option<Json> {
    use geo::BoundingRect;
    let lon_lat: Vec<geo::Geometry<f64>> = geometries
        .iter()
        .map(|(g, _)| {
            tripletrail::geo::transform(
                &g.geom.geometry,
                &g.geom.crs,
                &tripletrail::geo::Crs::Crs84,
            )
        })
        .collect::<Option<_>>()?;
    let total: usize = lon_lat.iter().map(geo::CoordsIter::coords_count).sum();
    let step = total.div_ceil(MAX_LIVE_POINTS).max(1);
    let mut bounds: Option<geo::Rect<f64>> = None;
    for g in &lon_lat {
        if let Some(r) = g.bounding_rect() {
            bounds = Some(match bounds {
                None => r,
                Some(b) => geo::Rect::new(
                    geo::coord! { x: b.min().x.min(r.min().x), y: b.min().y.min(r.min().y) },
                    geo::coord! { x: b.max().x.max(r.max().x), y: b.max().y.max(r.max().y) },
                ),
            });
        }
    }
    let b = bounds?;
    let inside = |x: f64, y: f64| (-180.0..=180.0).contains(&x) && (-90.0..=90.0).contains(&y);
    if !inside(b.min().x, b.min().y) || !inside(b.max().x, b.max().y) {
        return None;
    }
    let features: Vec<Json> = lon_lat
        .iter()
        .zip(geometries)
        .filter_map(|(g, (_, context))| Some((geojson(g, step)?, *context)))
        .map(|(g, context)| {
            json!({"type": "Feature", "properties": {"context": context}, "geometry": g})
        })
        .collect();
    Some(json!({
        "type": "FeatureCollection",
        "bbox": [b.min().x, b.min().y, b.max().x, b.max().y],
        "features": features,
    }))
}

fn map(out: &mut String, own: &[Geometry], context: &[Geometry], links: &PageLinks<'_>) {
    // The context first, so the resource's own are drawn over it.
    let all: Vec<(&Geometry, bool)> = context
        .iter()
        .map(|g| (g, true))
        .chain(own.iter().map(|g| (g, false)))
        .collect();
    let data = if links.live_map {
        live_map_data(&all)
    } else {
        None
    };
    use geo::BoundingRect;
    // One drawing, in one CRS: the first geometry's.
    let Some(first) = own.first().or(context.first()) else {
        return;
    };
    let crs = &first.geom.crs;
    let geometries: Vec<(&Geometry, bool)> = all
        .into_iter()
        .filter(|(g, _)| &g.geom.crs == crs || (g.geom.crs.is_geographic() && crs.is_geographic()))
        // Coordinates that would not scale to finite numbers are not drawn.
        .filter(|(g, _)| {
            g.geom.geometry.bounding_rect().is_some_and(|r| {
                [r.min().x, r.min().y, r.max().x, r.max().y]
                    .iter()
                    .all(|v| v.abs() < 1e12)
            })
        })
        .collect();
    let mut bounds: Option<geo::Rect<f64>> = None;
    for (g, _) in &geometries {
        if let Some(r) = g.geom.geometry.bounding_rect() {
            bounds = Some(match bounds {
                None => r,
                Some(b) => geo::Rect::new(
                    geo::coord! { x: b.min().x.min(r.min().x), y: b.min().y.min(r.min().y) },
                    geo::coord! { x: b.max().x.max(r.max().x), y: b.max().y.max(r.max().y) },
                ),
            });
        }
    }
    let Some(b) = bounds else { return };
    let geographic = geometries.iter().all(|(g, _)| g.geom.crs.is_geographic());
    // On geographic coordinates, a degree of longitude is shorter than one of latitude.
    let centre = b.center();
    let kx = if geographic {
        centre.y.to_radians().cos().max(0.01)
    } else {
        1.0
    };
    let (w, h) = (640.0_f64, 320.0_f64);
    let span_x = ((b.max().x - b.min().x) * kx).max(1e-9);
    let span_y = (b.max().y - b.min().y).max(1e-9);
    let scale = ((w - 20.0) / span_x).min((h - 20.0) / span_y);
    let ox = (w - span_x * scale) / 2.0;
    let oy = (h - span_y * scale) / 2.0;
    let point = |c: geo::Coord<f64>| {
        (
            ox + (c.x - b.min().x) * kx * scale,
            h - oy - (c.y - b.min().y) * scale,
        )
    };
    let mut svg = String::new();
    // About MAX_MAP_POINTS points in all.
    let total: usize = geometries
        .iter()
        .map(|(g, _)| geo::CoordsIter::coords_count(&g.geom.geometry))
        .sum();
    let step = total.div_ceil(MAX_MAP_POINTS).max(1);
    let mut in_context = false;
    for (g, context) in &geometries {
        if *context != in_context {
            svg.push_str(if *context {
                "<g class=\"context\">"
            } else {
                "</g>"
            });
            in_context = *context;
        }
        draw(&mut svg, &g.geom.geometry, &point, step);
    }
    if in_context {
        svg.push_str("</g>");
    }
    out.push_str("<h2>Map</h2>");
    let _ = write!(
        out,
        "<svg class=\"map\" id=\"mg-svg-map\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-label=\"Geometry\">{svg}</svg>"
    );
    // With the web UI, MapLibre makes it a map over OpenStreetMap; without script, the
    // drawing stays.
    if let Some(data) = data {
        let data = data.to_string().replace('<', "\\u003c");
        let basemap = links
            .basemap
            .map(|(tiles, attribution)| {
                format!(
                    " data-tiles=\"{}\" data-attribution=\"{}\"",
                    escape(tiles),
                    escape(attribution)
                )
            })
            .unwrap_or_default();
        let _ = write!(
            out,
            "<div class=\"live-map\" id=\"mg-live-map\"{basemap} hidden></div>\
             <script type=\"application/json\" id=\"mg-map-data\">{data}</script>\
             <script type=\"module\" src=\"/$/ui/describe-map.js\"></script>"
        );
    }
    if geographic {
        let _ = write!(
            out,
            "<div class=\"more\"><a href=\"https://www.openstreetmap.org/?mlat={lat:.6}&amp;mlon={lon:.6}#map={zoom}/{lat:.6}/{lon:.6}\">Open in OpenStreetMap</a></div>",
            lat = centre.y,
            lon = centre.x,
            zoom = osm_zoom(b.width().max(b.height()))
        );
    }
}

/// The OpenStreetMap zoom that shows `span` degrees: a tile is 360 / 2^zoom degrees wide, and
/// the window a few tiles.
fn osm_zoom(span: f64) -> u32 {
    if span <= 0.0 || !span.is_finite() {
        return 17;
    }
    (360.0 / span).log2().floor().clamp(1.0, 18.0) as u32
}

/// SVG elements for `g`: every `step`-th point of each line or ring (and its last), so a
/// geometry of many points keeps its shape on a page of bounded size, without points that fall
/// on the one before at the drawing's precision.
fn draw(
    svg: &mut String,
    g: &geo::Geometry<f64>,
    point: &dyn Fn(geo::Coord<f64>) -> (f64, f64),
    step: usize,
) {
    let path = |coords: &[geo::Coord<f64>], close: bool| {
        let mut d = String::new();
        let mut last = String::new();
        let n = coords.len();
        // A short ring or line keeps 4 points at least, so it does not vanish next to a long one.
        let step = step.min((n / 4).max(1));
        for (i, c) in coords.iter().enumerate() {
            if i % step != 0 && i + 1 != n {
                continue;
            }
            let (x, y) = point(*c);
            let at = format!("{x:.1},{y:.1}");
            if at == last {
                continue;
            }
            let _ = write!(d, "{}{at}", if d.is_empty() { "M" } else { "L" });
            last = at;
        }
        if close && !d.is_empty() {
            d.push('Z');
        }
        d
    };
    match g {
        geo::Geometry::Point(p) => {
            let (x, y) = point(p.0);
            let _ = write!(svg, "<circle cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"4\"/>");
        }
        geo::Geometry::MultiPoint(m) => {
            let mut seen = HashSet::new();
            for p in m.iter().step_by(step) {
                let (x, y) = point(p.0);
                if seen.insert(format!("{x:.1},{y:.1}")) {
                    let _ = write!(svg, "<circle cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"4\"/>");
                }
            }
        }
        geo::Geometry::Line(l) => {
            let d = path(&[l.start, l.end], false);
            let _ = write!(svg, "<path class=\"line\" d=\"{d}\"/>");
        }
        geo::Geometry::LineString(l) => {
            let d = path(&l.0, false);
            let _ = write!(svg, "<path class=\"line\" d=\"{d}\"/>");
        }
        geo::Geometry::MultiLineString(m) => {
            for l in m {
                draw(svg, &geo::Geometry::LineString(l.clone()), point, step);
            }
        }
        geo::Geometry::Polygon(p) => {
            let mut d = path(&p.exterior().0, true);
            for i in p.interiors() {
                d.push_str(&path(&i.0, true));
            }
            let _ = write!(
                svg,
                "<path class=\"area\" fill-rule=\"evenodd\" d=\"{d}\"/>"
            );
        }
        geo::Geometry::MultiPolygon(m) => {
            for p in m {
                draw(svg, &geo::Geometry::Polygon(p.clone()), point, step);
            }
        }
        geo::Geometry::Rect(r) => draw(svg, &geo::Geometry::Polygon(r.to_polygon()), point, step),
        geo::Geometry::Triangle(t) => {
            draw(svg, &geo::Geometry::Polygon(t.to_polygon()), point, step);
        }
        geo::Geometry::GeometryCollection(c) => {
            for g in c {
                draw(svg, g, point, step);
            }
        }
    }
}

/// The page of `view`.
pub(crate) fn html(view: &View, links: &PageLinks<'_>) -> String {
    let title = view
        .label
        .as_deref()
        .unwrap_or_else(|| local_name(view.iri.as_str()));
    let mut out = format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"referrer\" content=\"strict-origin\">\
         <title>{}</title><style>{STYLE}</style></head><body><main><h1>{}</h1>\
         <div class=\"iri\">{}</div>",
        escape(title),
        escape(title),
        escape(view.iri.as_str())
    );
    if !view.classes.is_empty() {
        out.push_str("<div class=\"chips\">");
        for c in &view.classes {
            out.push_str("<span class=\"chip\">");
            html_link(&mut out, links, c);
            out.push_str("</span>");
        }
        out.push_str("</div>");
    }
    if !view.badges.is_empty() {
        out.push_str("<div class=\"badges\">");
        for b in &view.badges {
            let _ = write!(out, "<span class=\"badge\">{}: ", escape(&b.label));
            for (i, v) in b.values.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                html_value(&mut out, links, v, "LabelViewer");
            }
            if let Some(u) = &b.unit {
                let _ = write!(out, " {}", escape(u));
            }
            if b.truncated {
                out.push_str(" (of the first linked resources)");
            }
            out.push_str("</span>");
        }
        out.push_str("</div>");
    }
    if view.own && !links.documents.is_empty() {
        out.push_str("<div class=\"docs\">Download: ");
        for (name, url) in &links.documents {
            let _ = write!(out, "<a href=\"{}\">{}</a>", escape(url), escape(name));
        }
        out.push_str("</div>");
    }
    if !view.geometries.is_empty() || !view.context.is_empty() {
        map(&mut out, &view.geometries, &view.context, links);
        if view.geometries_truncated {
            let _ = write!(
                out,
                "<div class=\"more\">The first {MAX_GEOMETRIES} geometries are drawn.</div>"
            );
        }
    }
    if view.truncated {
        let _ = write!(
            out,
            "<p class=\"more\">Only the first {MAX_OUTGOING_SCAN} triples of this resource are shown.</p>"
        );
    }
    let mut tab: Option<&str> = None;
    for g in &view.groups {
        // A group whose rows all draw nothing is left out, with its tab's heading when it is
        // the tab's first.
        let group_start = out.len();
        let before = tab;
        // A tab is a heading over its groups.
        if g.tab.as_deref() != tab {
            tab = g.tab.as_deref();
            if let Some(t) = tab {
                let _ = write!(out, "<h2 class=\"tab\">{}</h2>", escape(t));
            }
        }
        let h = if tab.is_some() { "h3" } else { "h2" };
        if !g.label.is_empty() {
            let _ = write!(out, "<{h}>{}</{h}>", escape(&g.label));
        }
        out.push_str("<table>");
        let mut written = 0;
        // A map row is the page's map, drawn above.
        for r in g
            .rows
            .iter()
            .filter(|r| r.widget.as_deref() != Some("MapViewer"))
        {
            let row_start = out.len();
            let _ = write!(
                out,
                "<tr><th title=\"{}\">{}</th><td>",
                escape(&r.path.clone().unwrap_or_else(|| format!(
                    "{}{}",
                    if r.inverse { "^" } else { "" },
                    r.property.as_str()
                ))),
                escape(&r.label)
            );
            // A generic widget draws the row; a widget not drawn here falls back to its
            // values, table and named paths.
            let mut known = true;
            let drawn = match r.widget.as_deref() {
                Some("DocumentViewer") => html_documents(&mut out, links, r),
                Some("ScaleBarViewer") => html_scale_bar(&mut out, r),
                Some("ConceptTreeViewer") => html_concept_tree(&mut out, links, r, &view.iri),
                Some("FacilitiesViewer") => html_facilities(&mut out, links, r),
                Some("KeyFiguresViewer") => html_key_figures(&mut out, links, r),
                Some("LrsLocationViewer") => html_lrs_location(&mut out, links, r),
                Some("StepChartViewer") => html_step_chart(&mut out, r),
                Some("LineStopsViewer") => html_line_stops(&mut out, links, r),
                Some("RouteDiagramViewer") => html_route_diagram(&mut out, links, r),
                Some("AlongLineViewer") => html_along_line(&mut out, r),
                Some("LineProfileViewer") => html_line_profile(&mut out, r),
                Some("LinePositionViewer") => html_line_position(&mut out, links, r, &view.iri),
                Some("PlatformViewer") => html_platform(&mut out, r),
                Some("CrossSectionViewer") => html_cross_section(&mut out, r),
                Some("TrackLayoutViewer") => html_track_layout(&mut out, links, r),
                Some("TopologyViewer") => html_topology(&mut out, links, r, &view.iri),
                Some("PositionedViewer") => html_positioned(&mut out, links, r, &view.iri),
                Some("ChangesViewer") => html_changes(&mut out, links, r),
                Some("DataStatusViewer") => html_data_status(&mut out, links, r),
                Some("ParameterIndexViewer") => html_parameters(&mut out, links, r),
                Some("SymbolViewer") => html_symbols(&mut out, links, r),
                Some("ConnectionsViewer") => {
                    html_connections(&mut out, links, r, &view.iri, &view.geometries)
                }
                _ => {
                    known = false;
                    false
                }
            };
            // A widget drawn here with nothing to draw (an along-the-line strip without the
            // section's kilometres), on a row with nothing else, is left out, as rinf-describe
            // does; so is a line profile or list of stops whose sections have no kilometres.
            if known
                && !drawn
                && ((r.viewer == "WidgetViewer" && r.values.is_empty() && r.table.is_none())
                    || matches!(
                        r.widget.as_deref(),
                        Some("LineProfileViewer" | "LineStopsViewer")
                    ))
            {
                out.truncate(row_start);
                continue;
            }
            written += 1;
            if let Some(t) = &r.table {
                if !drawn {
                    html_table(&mut out, links, &r.values, t);
                }
                if t.truncated {
                    out.push_str(
                        "<div class=\"more\">Not every cell was read: \
                         an empty cell further down may have values.</div>",
                    );
                }
            }
            let list = r.values.len() > 1 && r.table.is_none() && !drawn;
            if list {
                out.push_str("<ul>");
            }
            for v in r.values.iter().filter(|_| r.table.is_none() && !drawn) {
                if list {
                    out.push_str("<li>");
                }
                html_value(&mut out, links, v, r.viewer);
                if list {
                    out.push_str("</li>");
                }
            }
            if list {
                out.push_str("</ul>");
            }
            if let Some(u) = r.unit.as_ref().filter(|_| !drawn) {
                let _ = write!(out, " <span class=\"unit\">{}</span>", escape(u));
            }
            if !drawn {
                html_params(&mut out, links, r);
            }
            if r.more > 0 {
                let _ = write!(out, "<div class=\"more\">and {} more</div>", r.more);
            }
            if r.truncated {
                out.push_str(
                    "<div class=\"more\">Not every linked resource was read: \
                     this is of the first ones.</div>",
                );
            }
            out.push_str("</td></tr>");
        }
        if written == 0 {
            out.truncate(group_start);
            tab = before;
            continue;
        }
        out.push_str("</table>");
    }
    if !view.incoming.is_empty() {
        out.push_str("<h2>Referenced by</h2><table>");
        for i in &view.incoming {
            out.push_str("<tr><th>");
            html_link(&mut out, links, &i.property);
            out.push_str("</th><td><ul>");
            for s in &i.subjects {
                out.push_str("<li>");
                html_value(&mut out, links, s, "LabelViewer");
                out.push_str("</li>");
            }
            out.push_str("</ul>");
            if i.count > i.subjects.len() {
                let _ = write!(
                    out,
                    "<div class=\"more\">{} links{}</div>",
                    i.count,
                    if view.incoming_truncated {
                        " or more"
                    } else {
                        ""
                    }
                );
            }
            out.push_str("</td></tr>");
        }
        out.push_str("</table>");
    }
    out.push_str("</main></body></html>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_days_since_1970() {
        assert_eq!(day_of("1970-01-01"), Some(0));
        assert_eq!(day_of("2000-03-01T12:00:00Z"), Some(11_017));
        assert_eq!(day_of("1969-12-31"), Some(-1));
        assert_eq!(day_of("2024-13-01"), None);
        assert_eq!(day_of("today"), None);
        assert_eq!(day_of("2027-02-30"), None);
        assert_eq!(day_of("2024-02-29"), Some(19_782));
        assert_eq!(day_of("2023-02-29"), None);
    }

    #[test]
    fn energy_supplies_are_read_from_their_label() {
        assert_eq!(
            supply_of("AC 25kV-50Hz"),
            Some((true, 25_000.0, Some(50.0)))
        );
        assert_eq!(
            supply_of("AC 15 kV - 16.7 Hz"),
            Some((true, 15_000.0, Some(16.7)))
        );
        assert_eq!(supply_of("DC 750V"), Some((false, 750.0, None)));
        assert_eq!(supply_of("not electrified"), None);
        assert_eq!(supply_colour(supply_of("AC 15kV-16.7Hz")), "#2e8b3e");
        assert_eq!(supply_colour(supply_of("DC 3kV")), "#1f5fbf");
    }

    #[test]
    fn names_sort_naturally() {
        assert!(natural_cmp("Voie 2", "Voie 10").is_lt());
        assert!(natural_cmp("A", "b").is_lt());
        assert_eq!(
            common_prefix(&["Leuven - 1".into(), "Leuven - 2A".into()]),
            9
        );
        assert_eq!(common_prefix(&["Track 1".into(), "Track 2".into()]), 0);
    }
}
