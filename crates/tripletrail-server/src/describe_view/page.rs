//! The parts of a describe page around the rows of its shapes, laid out as rinf-describe lays
//! them out: a header with the resource's classes, title, IRI and actions (copy, share, open
//! in the map, explore, export), then tabs. The shapes give the Overview and their own tabs;
//! every resource also gets All properties, Incoming links, Graph, Data model and RDF.
//!
//! Everything is drawn on the server, so the page reads without script: the tabs are then
//! sections one under the other. With the web UI, `/$/ui/describe.js` turns them into tabs and
//! makes the copy, share and format buttons work.

use super::{
    Ctx, Incoming, Link, PageLinks, Value, View, dash, escape, html_link, html_value, local_name,
    node_shapes, sh, viewer_for, with_superclasses,
};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use tripletrail::oxigraph::model::vocab::{rdf, rdfs};
use tripletrail::oxigraph::model::{NamedNode, NamedOrBlankNodeRef, Term, TermRef};

/// Classes in a data model at most: the resource's and their superclasses.
const MAX_MODEL_CLASSES: usize = 12;
/// Members (attributes and associations) of one class read at most.
const MAX_MODEL_MEMBERS: usize = 200;
/// Attributes drawn in a class box; the others are counted, and listed below the diagram.
const MAX_SHOWN_ATTRIBUTES: usize = 24;
/// Associations from other classes to the model's classes at most.
const MAX_MODEL_INCOMING: usize = 24;
/// Associations from other classes read at most, before they are sorted and cut.
const MAX_MODEL_INCOMING_READ: usize = 1_000;
/// Nodes on either side of the graph at most.
const MAX_GRAPH_SIDE: usize = 24;
/// Values of one property in the graph at most; the others are one "+ n more" node.
const MAX_GRAPH_PER_PROPERTY: usize = 6;
/// Literal values drawn in the graph at most: with more, the graph keeps only the links.
const MAX_GRAPH_LITERALS: usize = 12;
/// Triples of the RDF tab written into the page at most; past that, the tab offers the files.
pub(crate) const MAX_EMBEDDED_TRIPLES: usize = 5_000;
/// Bytes of Turtle written into the page at most.
pub(crate) const MAX_EMBEDDED_BYTES: usize = 1 << 20;
/// Characters of a literal in All properties and the graph at most.
const MAX_LITERAL_CHARS: usize = 1_000;

/// Well-known prefixes, for the RDF documents and the names on the page.
const KNOWN_PREFIXES: [(&str, &str); 22] = [
    ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
    ("xsd", "http://www.w3.org/2001/XMLSchema#"),
    ("owl", "http://www.w3.org/2002/07/owl#"),
    ("skos", "http://www.w3.org/2004/02/skos/core#"),
    ("geo", "http://www.opengis.net/ont/geosparql#"),
    ("sf", "http://www.opengis.net/ont/sf#"),
    ("dcterms", "http://purl.org/dc/terms/"),
    ("dc", "http://purl.org/dc/elements/1.1/"),
    ("foaf", "http://xmlns.com/foaf/0.1/"),
    ("schema", "https://schema.org/"),
    ("prov", "http://www.w3.org/ns/prov#"),
    ("dcat", "http://www.w3.org/ns/dcat#"),
    ("sh", "http://www.w3.org/ns/shacl#"),
    ("dash", "http://datashapes.org/dash#"),
    ("qudt", "http://qudt.org/schema/qudt/"),
    ("unit", "http://qudt.org/vocab/unit/"),
    ("time", "http://www.w3.org/2006/time#"),
    ("vann", "http://purl.org/vocab/vann/"),
    ("void", "http://rdfs.org/ns/void#"),
    ("wgs", "http://www.w3.org/2003/01/geo/wgs84_pos#"),
    ("era", "http://data.europa.eu/949/"),
];
/// Prefixes a document declares at most.
const MAX_PREFIXES: usize = 32;

/// The prefixes for a document of `iris`: the well-known ones of the namespaces it uses, and
/// for other namespaces used at least twice a name from their last path segment (`ex` for
/// `http://example.com/ex/`), else `ns1`, `ns2`...
pub(crate) fn prefixes<'a>(iris: impl IntoIterator<Item = &'a str>) -> Vec<(String, String)> {
    let mut uses: Vec<(&str, usize)> = Vec::new();
    for iri in iris {
        let ns = super::namespace(iri);
        if ns.is_empty() || ns.len() == iri.len() {
            continue;
        }
        match uses.iter_mut().find(|(n, _)| *n == ns) {
            Some((_, k)) => *k += 1,
            None => uses.push((ns, 1)),
        }
    }
    let mut out: Vec<(String, String)> = Vec::new();
    let taken = |out: &Vec<(String, String)>, p: &str| out.iter().any(|(q, _)| q == p);
    for (ns, k) in &uses {
        if out.len() == MAX_PREFIXES {
            break;
        }
        if let Some((p, _)) = KNOWN_PREFIXES.iter().find(|(_, n)| n == ns) {
            if !taken(&out, p) {
                out.push(((*p).to_owned(), (*ns).to_owned()));
            }
            continue;
        }
        if *k < 2 || !(ns.starts_with("http://") || ns.starts_with("https://")) {
            continue;
        }
        // The last path segment, else the host's first name (`example` for example.com).
        let rest = ns.split_once("://").map_or("", |(_, r)| r);
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        let segment = path
            .split(['/', '#'])
            .rfind(|s| !s.is_empty())
            .unwrap_or_else(|| {
                host.split('.')
                    .find(|l| *l != "www" && *l != "data")
                    .unwrap_or_default()
            })
            .to_ascii_lowercase();
        let usable = segment.len() <= 12
            && segment.starts_with(|c: char| c.is_ascii_lowercase())
            && segment.chars().all(|c| c.is_ascii_alphanumeric())
            && !KNOWN_PREFIXES.iter().any(|(p, _)| *p == segment);
        let mut name = if usable { segment } else { String::new() };
        if name.is_empty() || taken(&out, &name) {
            let mut i = 1;
            while taken(&out, &format!("ns{i}")) {
                i += 1;
            }
            name = format!("ns{i}");
        }
        out.push((name, (*ns).to_owned()));
    }
    out
}

/// `prefix:local` for an IRI under a well-known namespace, else its local name.
fn short(iri: &str) -> String {
    let ns = super::namespace(iri);
    match KNOWN_PREFIXES.iter().find(|(_, n)| *n == ns) {
        Some((p, _)) => format!("{p}:{}", &iri[ns.len()..]),
        None => local_name(iri).to_owned(),
    }
}

/// An attribute or association of a class in the data model, from a property shape.
pub(crate) struct Member {
    property: NamedNode,
    /// `sh:name` of the shape.
    label: Option<String>,
    /// The datatype (local name), `IRI`, `enumeration`...; empty when the shape says nothing.
    kind: String,
    /// The class the values have (`sh:class`): an association.
    target: Option<Link>,
    min: u64,
    max: Option<u64>,
    /// Whether this resource has the property (for links in, whether one links to it).
    used: bool,
}

impl Member {
    /// UML multiplicity: `1`, `0..1`, `0..*`, `1..*`.
    fn multiplicity(&self) -> String {
        let hi = self.max.map_or("*".to_owned(), |m| m.to_string());
        if Some(self.min) == self.max {
            hi
        } else {
            format!("{}..{hi}", self.min)
        }
    }

    fn name(&self) -> &str {
        local_name(self.property.as_str())
    }
}

/// A class of the data model with what its shapes give it.
pub(crate) struct ModelBox {
    class: Link,
    /// The boxes of its direct superclasses.
    parents: Vec<usize>,
    /// How far above the resource's own classes (0 for them).
    depth: usize,
    attributes: Vec<Member>,
    associations: Vec<Member>,
}

/// The data model of a resource: its classes and their superclasses with the attributes and
/// associations of their shapes, and the associations of other classes that point to them.
#[derive(Default)]
pub(crate) struct Model {
    boxes: Vec<ModelBox>,
    /// (class it comes from, box it points at, the association).
    incoming: Vec<(Link, usize, Member)>,
}

/// The data model of a resource of `types`; `used` and `used_in` are the properties it has
/// and those that link to it.
pub(super) fn model(
    ctx: &Ctx<'_>,
    types: &[NamedNode],
    used: &HashSet<NamedNode>,
    used_in: &HashSet<NamedNode>,
) -> Model {
    let classes: Vec<NamedNode> = with_superclasses(ctx, types)
        .into_iter()
        .take(MAX_MODEL_CLASSES)
        .collect();
    let at: HashMap<&NamedNode, usize> = classes.iter().enumerate().map(|(i, c)| (c, i)).collect();
    let parents: Vec<Vec<usize>> = classes
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut ps: Vec<usize> = ctx
                .objects(c.as_ref().into(), rdfs::SUB_CLASS_OF)
                .iter()
                .filter_map(|t| match t {
                    Term::NamedNode(n) => at.get(n).copied(),
                    _ => None,
                })
                .filter(|&j| j != i)
                .collect();
            ps.dedup();
            ps
        })
        .collect();
    // A superclass sits above every class under it (cycles stop at the number of classes).
    let mut depth = vec![0_usize; classes.len()];
    for _ in 0..classes.len() {
        let mut changed = false;
        for i in 0..classes.len() {
            for &j in &parents[i] {
                if depth[j] < depth[i] + 1 && depth[i] < classes.len() {
                    depth[j] = depth[i] + 1;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let named = |p: NamedOrBlankNodeRef<'_>, k: &str| match ctx
        .shapes
        .object_for_subject_predicate(p, &sh(k))
    {
        Some(TermRef::NamedNode(n)) => Some(n.into_owned()),
        _ => None,
    };
    let count = |p: NamedOrBlankNodeRef<'_>, k: &str| {
        ctx.shape_number(p, &sh(k))
            .filter(|n| n.is_finite() && *n >= 0.0)
            .map(|n| n as u64)
    };
    // A property is drawn once, on the most specific class whose shapes have it.
    let mut seen: HashSet<NamedNode> = HashSet::new();
    let mut boxes = Vec::new();
    for (i, class) in classes.iter().enumerate() {
        let (mut attributes, mut associations) = (Vec::new(), Vec::new());
        for n in node_shapes(ctx, std::slice::from_ref(class)) {
            // In `sh:order`, then by path: the graph's own order differs from store to store.
            let mut props: Vec<NamedOrBlankNodeRef<'_>> = ctx
                .shapes
                .objects_for_subject_predicate(n.as_ref(), &sh("property"))
                .filter_map(|p| match p {
                    TermRef::NamedNode(n) => Some(n.into()),
                    TermRef::BlankNode(b) => Some(b.into()),
                    _ => None,
                })
                .collect();
            props.sort_by_cached_key(|p| {
                (
                    ctx.shape_number(*p, &sh("order"))
                        .filter(|o| o.is_finite())
                        .map_or(i64::MAX, |o| (o * 1000.0) as i64),
                    named(*p, "path")
                        .map(|n| n.into_string())
                        .unwrap_or_default(),
                )
            });
            for p in props {
                if ctx.shape_true(p, &sh("deactivated")) {
                    continue;
                }
                let Some(path) = named(p, "path") else {
                    continue;
                };
                if attributes.len() + associations.len() >= MAX_MODEL_MEMBERS
                    || !seen.insert(path.clone())
                {
                    continue;
                }
                let mut m = Member {
                    used: used.contains(&path),
                    property: path,
                    label: ctx.shape_text(p, &sh("name")),
                    kind: String::new(),
                    target: None,
                    min: count(p, "minCount").unwrap_or(0),
                    max: count(p, "maxCount"),
                };
                if let Some(c) = named(p, "class") {
                    m.target = Some(ctx.link(&c));
                    associations.push(m);
                    continue;
                }
                m.kind = if let Some(d) = named(p, "datatype") {
                    local_name(d.as_str()).to_owned()
                } else if ctx
                    .shapes
                    .object_for_subject_predicate(p, &sh("in"))
                    .is_some()
                {
                    "enumeration".to_owned()
                } else {
                    match named(p, "nodeKind")
                        .as_ref()
                        .map(|k| local_name(k.as_str()))
                    {
                        Some("IRI") => "IRI",
                        Some("Literal") => "literal",
                        Some("BlankNode") => "node",
                        Some("BlankNodeOrIRI") => "IRI or node",
                        _ => "",
                    }
                    .to_owned()
                };
                attributes.push(m);
            }
        }
        boxes.push(ModelBox {
            class: ctx.link(class),
            parents: parents[i].clone(),
            depth: depth[i],
            attributes,
            associations,
        });
    }
    // What points at the model's classes: property shapes with `sh:class` one of them, of the
    // node shapes of other classes.
    let mut incoming = Vec::new();
    let mut pairs: HashSet<(NamedNode, NamedNode)> = HashSet::new();
    'classes: for (b, class) in classes.iter().enumerate() {
        let shapes: Vec<NamedOrBlankNodeRef<'_>> = ctx
            .shapes
            .subjects_for_predicate_object(&sh("class"), TermRef::from(class.as_ref()))
            .collect();
        for ps in shapes {
            if ctx.shape_true(ps, &sh("deactivated")) {
                continue;
            }
            let Some(path) = named(ps, "path") else {
                continue;
            };
            let owners: Vec<NamedOrBlankNodeRef<'_>> = ctx
                .shapes
                .subjects_for_predicate_object(&sh("property"), TermRef::from(ps))
                .collect();
            for ns in owners {
                let mut sources: Vec<NamedNode> = [sh("targetClass"), dash("applicableToClass")]
                    .iter()
                    .flat_map(|k| ctx.shapes.objects_for_subject_predicate(ns, k))
                    .filter_map(|t| match t {
                        TermRef::NamedNode(n) => Some(n.into_owned()),
                        _ => None,
                    })
                    .collect();
                if let (true, NamedOrBlankNodeRef::NamedNode(n)) = (sources.is_empty(), ns) {
                    sources.push(n.into_owned());
                }
                for s in sources {
                    if incoming.len() >= MAX_MODEL_INCOMING_READ {
                        break 'classes;
                    }
                    if at.contains_key(&s) || !pairs.insert((s.clone(), path.clone())) {
                        continue;
                    }
                    let m = Member {
                        used: used_in.contains(&path),
                        property: path.clone(),
                        label: ctx.shape_text(ps, &sh("name")),
                        kind: String::new(),
                        target: Some(ctx.link(class)),
                        min: count(ps, "minCount").unwrap_or(0),
                        max: count(ps, "maxCount"),
                    };
                    incoming.push((ctx.link(&s), b, m));
                }
            }
        }
    }
    // The same order on every store, whatever order the shapes were read in.
    incoming.sort_by(|(a, i, x), (b, j, y)| {
        (a.iri.as_str(), i, x.property.as_str()).cmp(&(b.iri.as_str(), j, y.property.as_str()))
    });
    incoming.truncate(MAX_MODEL_INCOMING);
    Model { boxes, incoming }
}

/// The name of a link: its label, else its local name.
fn name_of(l: &Link) -> String {
    l.label
        .clone()
        .unwrap_or_else(|| local_name(l.iri.as_str()).to_owned())
}

/// `text` cut to `n` characters, with an ellipsis.
fn cut(text: &str, n: usize) -> String {
    if text.chars().count() <= n {
        text.to_owned()
    } else {
        let mut s: String = text.chars().take(n.saturating_sub(1)).collect();
        s.push('\u{2026}');
        s
    }
}

/// About how wide `text` is in the pages' 12px font.
fn width(text: &str, px: f64) -> f64 {
    text.chars().count() as f64 * px
}

/// The map position of the resource: the centre and zoom of its geographic geometries.
fn map_position(view: &View) -> Option<(f64, f64, u32)> {
    use geo::BoundingRect;
    let mut bounds: Option<geo::Rect<f64>> = None;
    for g in view
        .geometries
        .iter()
        .filter(|g| g.geom.crs.is_geographic())
    {
        let Some(r) = g.geom.geometry.bounding_rect() else {
            continue;
        };
        if ![r.min().x, r.min().y, r.max().x, r.max().y]
            .iter()
            .all(|v| v.is_finite() && v.abs() <= 360.0)
        {
            continue;
        }
        bounds = Some(match bounds {
            None => r,
            Some(b) => geo::Rect::new(
                geo::coord! { x: b.min().x.min(r.min().x), y: b.min().y.min(r.min().y) },
                geo::coord! { x: b.max().x.max(r.max().x), y: b.max().y.max(r.max().y) },
            ),
        });
    }
    let b = bounds?;
    let c = b.center();
    let zoom = super::osm_zoom(b.width().max(b.height())).min(17);
    Some((c.y, c.x, zoom))
}

fn query_value(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// The header: classes, title, IRI and the actions.
pub(super) fn header(out: &mut String, view: &View, links: &PageLinks<'_>, title: &str) {
    let iri = view.iri.as_str();
    out.push_str("<header class=\"page-head\">");
    if !view.classes.is_empty() {
        out.push_str("<div class=\"chips\">");
        for c in &view.classes {
            let _ = write!(
                out,
                "<a class=\"chip\" href=\"{}\" title=\"{}\">{}</a>",
                escape(&(links.href)(c.iri.as_str())),
                escape(c.iri.as_str()),
                escape(&name_of(c))
            );
        }
        out.push_str("</div>");
    }
    let _ = write!(out, "<h1>{}</h1><div class=\"iri-line\">", escape(title));
    if super::web(iri) {
        let _ = write!(
            out,
            "<a class=\"iri\" href=\"{0}\" rel=\"noopener\">{0}</a>",
            escape(iri)
        );
    } else {
        let _ = write!(out, "<span class=\"iri\">{}</span>", escape(iri));
    }
    out.push_str("<span class=\"actions\">");
    // Script makes these work; without it they stay hidden.
    let _ = write!(
        out,
        "<button type=\"button\" class=\"btn\" data-copy=\"{}\" title=\"Copy the IRI\" hidden>Copy IRI</button>\
         <button type=\"button\" class=\"btn\" data-share=\"{}\" title=\"Copy a link to this page and tab\" hidden>Share</button>",
        escape(iri),
        escape(&links.page),
    );
    if let Some(ds) = links.dataset.filter(|_| links.live_map) {
        if let Some((lat, lon, zoom)) = map_position(view) {
            let _ = write!(
                out,
                "<a class=\"btn\" href=\"/$/ui/map/{}?feature={}#{zoom}/{lat:.5}/{lon:.5}\" \
                 title=\"Open the dataset's map here, with this resource selected\">Open in map</a>",
                escape(&query_value(ds)),
                escape(&query_value(iri)),
            );
        }
        let _ = write!(
            out,
            "<a class=\"btn\" href=\"/$/ui/explore/{}?iri={}\" title=\"Walk the graph from this resource\">Explore</a>",
            escape(&query_value(ds)),
            escape(&query_value(iri)),
        );
    }
    if view.own && !links.documents.is_empty() {
        out.push_str(
            "<details class=\"menu\"><summary class=\"btn\">Export</summary><div class=\"menu-items\">",
        );
        for (name, url) in &links.documents {
            let _ = write!(
                out,
                "<a href=\"{}\" download>{}</a>",
                escape(url),
                format_label(name)
            );
        }
        out.push_str("</div></details>");
    }
    out.push_str("</span></div>");
}

/// The menu name of a document format by its suffix.
fn format_label(suffix: &str) -> &str {
    match suffix {
        "ttl" => "Turtle",
        "jsonld" => "JSON-LD",
        "nt" => "N-Triples",
        "rdf" => "RDF/XML",
        other => other,
    }
}

/// A list of values by property: All properties, Incoming links.
fn triples_table(out: &mut String, links: &PageLinks<'_>, rows: &[Incoming], incoming: bool) {
    out.push_str("<div class=\"card\"><table class=\"triples\"><thead><tr>");
    out.push_str(if incoming {
        "<th>Property</th><th>Linked from</th>"
    } else {
        "<th>Property</th><th>Value</th>"
    });
    out.push_str("</tr></thead><tbody>");
    for r in rows {
        let _ = write!(
            out,
            "<tr><th scope=\"row\" title=\"{}\"><span class=\"prop\">",
            escape(r.property.iri.as_str())
        );
        if r.property.iri.as_str() == rdf::TYPE.as_str() {
            out.push_str("Type");
        } else {
            out.push_str(&escape(&name_of(&r.property)));
        }
        let _ = write!(
            out,
            "</span> <span class=\"curie\">{}</span></th><td>",
            escape(&short(r.property.iri.as_str()))
        );
        let list = r.subjects.len() > 1;
        if list {
            out.push_str("<ul class=\"plain\">");
        }
        for v in &r.subjects {
            if list {
                out.push_str("<li>");
            }
            match v {
                // A long literal (a big geometry) is cut: the RDF tab and files have it whole.
                Value::Literal(l) if l.value().chars().count() > MAX_LITERAL_CHARS => {
                    let _ = write!(
                        out,
                        "<span class=\"lit\">{}</span> <small class=\"more\">({} characters)</small>",
                        escape(&cut(l.value(), MAX_LITERAL_CHARS)),
                        l.value().chars().count()
                    );
                }
                _ => html_value(out, links, v, viewer_for(v)),
            }
            if list {
                out.push_str("</li>");
            }
        }
        if list {
            out.push_str("</ul>");
        }
        if r.count > r.subjects.len() {
            let _ = write!(
                out,
                "<div class=\"more\">{} of {}</div>",
                r.subjects.len(),
                r.count
            );
        }
        out.push_str("</td></tr>");
    }
    out.push_str("</tbody></table></div>");
}

pub(super) fn properties_panel(out: &mut String, view: &View, links: &PageLinks<'_>) {
    triples_table(out, links, &view.properties, false);
    if view.truncated {
        let _ = write!(
            out,
            "<p class=\"more\">Only the first {} triples of this resource are read.</p>",
            super::MAX_OUTGOING_SCAN
        );
    }
}

pub(super) fn incoming_panel(out: &mut String, view: &View, links: &PageLinks<'_>) {
    triples_table(out, links, &view.links_in, true);
    if view.incoming_truncated {
        let _ = write!(
            out,
            "<p class=\"more\">Only the first {} links to this resource are read.</p>",
            super::MAX_INCOMING_SCAN
        );
    }
}

/// A node of the graph.
struct Node {
    label: String,
    title: String,
    href: Option<String>,
    kind: &'static str,
    /// The property of its edge to the resource.
    property: String,
    property_iri: String,
}

/// The graph: the resource in the middle, what links to it on the left, what it links to on
/// the right (its classes as filled pills, values as square plates), one hop.
pub(super) fn graph_panel(out: &mut String, view: &View, links: &PageLinks<'_>) {
    let literals: usize = view
        .properties
        .iter()
        .map(|p| {
            p.subjects
                .iter()
                .filter(|v| matches!(v, Value::Literal(_)))
                .count()
        })
        .sum();
    let with_literals = literals <= MAX_GRAPH_LITERALS;
    let mut left: Vec<Node> = Vec::new();
    let mut right: Vec<Node> = Vec::new();
    let mut left_more = 0;
    let mut right_more = 0;
    let node_of = |v: &Value, property: &Link, kind_class: bool| -> Option<Node> {
        let (label, title, href, kind) = match v {
            Value::Iri(l) => (
                name_of(l),
                l.iri.as_str().to_owned(),
                Some((links.href)(l.iri.as_str())),
                if kind_class { "class" } else { "resource" },
            ),
            Value::Literal(l) if with_literals => (
                l.value().chars().take(80).collect(),
                cut(l.value(), 300),
                None,
                "literal",
            ),
            _ => return None,
        };
        Some(Node {
            label: cut(&label, 34),
            title,
            href,
            kind,
            property: if kind_class {
                "a".to_owned()
            } else {
                cut(local_name(property.iri.as_str()), 24)
            },
            property_iri: property.iri.as_str().to_owned(),
        })
    };
    let side = |rows: &[Incoming], nodes: &mut Vec<Node>, more: &mut usize| {
        for r in rows {
            let class = r.property.iri.as_str() == rdf::TYPE.as_str();
            let mut shown = 0;
            for v in &r.subjects {
                if nodes.len() >= MAX_GRAPH_SIDE {
                    *more += 1;
                    continue;
                }
                if shown == MAX_GRAPH_PER_PROPERTY {
                    break;
                }
                if let Some(n) = node_of(v, &r.property, class) {
                    nodes.push(n);
                    shown += 1;
                }
            }
            let drawable = r
                .subjects
                .iter()
                .filter(|v| {
                    matches!(v, Value::Iri(_)) || (with_literals && matches!(v, Value::Literal(_)))
                })
                .count();
            let rest = r.count.saturating_sub(r.subjects.len()) + drawable.saturating_sub(shown);
            if shown > 0 && rest > 0 && nodes.len() < MAX_GRAPH_SIDE {
                nodes.push(Node {
                    label: format!("+ {rest} more"),
                    title: format!("{rest} more values of {}", r.property.iri.as_str()),
                    href: None,
                    kind: "more",
                    property: cut(local_name(r.property.iri.as_str()), 24),
                    property_iri: r.property.iri.as_str().to_owned(),
                });
            } else if rest > 0 {
                *more += rest;
            }
        }
    };
    side(&view.properties, &mut right, &mut right_more);
    side(&view.links_in, &mut left, &mut left_more);

    const ROW: f64 = 34.0;
    const H: f64 = 26.0;
    const GAP: f64 = 190.0;
    let node_w = |n: &Node| (width(&n.label, 6.8) + 22.0).clamp(48.0, 250.0);
    let col_w = |ns: &[Node]| ns.iter().map(node_w).fold(0.0_f64, f64::max);
    let root_label = cut(
        &view
            .label
            .clone()
            .unwrap_or_else(|| local_name(view.iri.as_str()).to_owned()),
        40,
    );
    let root_w = (width(&root_label, 7.4) + 28.0).max(80.0);
    let lw = col_w(&left);
    let rw = col_w(&right);
    let root_x = if left.is_empty() {
        16.0
    } else {
        16.0 + lw + GAP
    };
    let right_x = root_x + root_w + GAP;
    let total_w = if right.is_empty() {
        root_x + root_w + 16.0
    } else {
        right_x + rw + 16.0
    };
    let n = left.len().max(right.len()).max(1);
    let total_h = n as f64 * ROW + 24.0;
    let root_y = total_h / 2.0 - H / 2.0;
    let y_of = |i: usize, len: usize| {
        12.0 + (total_h - 24.0 - len as f64 * ROW) / 2.0 + i as f64 * ROW + (ROW - H) / 2.0
    };
    let _ = write!(
        out,
        "<div class=\"card\"><p class=\"more\">{} linked nodes{}. Click a node to open its page.</p>\
         <div class=\"diagram\"><svg class=\"graph\" width=\"{total_w:.0}\" height=\"{total_h:.0}\" viewBox=\"0 0 {total_w:.0} {total_h:.0}\" role=\"img\" aria-label=\"Graph of the linked resources\">\
         <defs><marker id=\"mg-arrow\" viewBox=\"0 0 10 10\" refX=\"10\" refY=\"5\" markerWidth=\"7\" markerHeight=\"7\" orient=\"auto-start-reverse\"><path d=\"M0 0L10 5L0 10z\" class=\"arrow\"/></marker></defs>",
        left.len() + right.len(),
        match (with_literals, left_more + right_more) {
            (true, 0) => String::new(),
            (true, k) => format!("; {k} more are in All properties and Incoming links"),
            (false, k) => format!(
                "; the {literals} literal values{} are in All properties",
                if k > 0 {
                    format!(" and {k} more links")
                } else {
                    String::new()
                }
            ),
        }
    );
    let draw_node = |out: &mut String, n: &Node, x: f64, y: f64| {
        let w = node_w(n);
        if let Some(h) = &n.href {
            let _ = write!(out, "<a href=\"{}\">", escape(h));
        }
        let _ = write!(
            out,
            "<g class=\"node {}\"><title>{}</title><rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{H}\" rx=\"{}\"/>\
             <text x=\"{:.1}\" y=\"{:.1}\">{}</text></g>",
            n.kind,
            escape(&n.title),
            if matches!(n.kind, "literal" | "more") {
                2
            } else {
                13
            },
            x + w / 2.0,
            y + H / 2.0 + 4.0,
            escape(&n.label)
        );
        if n.href.is_some() {
            out.push_str("</a>");
        }
    };
    for (i, n) in right.iter().enumerate() {
        let y = y_of(i, right.len());
        let (x1, y1, x2, y2) = (root_x + root_w, root_y + H / 2.0, right_x, y + H / 2.0);
        let mx = (x1 + x2) / 2.0;
        let _ = write!(
            out,
            "<path class=\"edge\" d=\"M{x1:.1} {y1:.1}C{mx:.1} {y1:.1} {mx:.1} {y2:.1} {x2:.1} {y2:.1}\" marker-end=\"url(#mg-arrow)\"><title>{}</title></path>\
             <text class=\"edge-label\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\">{}</text>",
            escape(&n.property_iri),
            x2 - 10.0,
            y2 - 5.0,
            escape(&n.property)
        );
        draw_node(out, n, right_x, y);
    }
    for (i, n) in left.iter().enumerate() {
        let y = y_of(i, left.len());
        let w = node_w(n);
        let (x1, y1, x2, y2) = (16.0 + lw - (lw - w), y + H / 2.0, root_x, root_y + H / 2.0);
        let mx = (x1 + x2) / 2.0;
        let _ = write!(
            out,
            "<path class=\"edge\" d=\"M{x1:.1} {y1:.1}C{mx:.1} {y1:.1} {mx:.1} {y2:.1} {x2:.1} {y2:.1}\" marker-end=\"url(#mg-arrow)\"><title>{}</title></path>\
             <text class=\"edge-label\" x=\"{:.1}\" y=\"{:.1}\">{}</text>",
            escape(&n.property_iri),
            x1 + 10.0,
            y1 - 5.0,
            escape(&n.property)
        );
        draw_node(out, n, 16.0, y);
    }
    let _ = write!(
        out,
        "<g class=\"node root\"><title>{}</title><rect x=\"{root_x:.1}\" y=\"{root_y:.1}\" width=\"{root_w:.1}\" height=\"{H}\" rx=\"13\"/>\
         <text x=\"{:.1}\" y=\"{:.1}\">{}</text></g></svg></div></div>",
        escape(view.iri.as_str()),
        root_x + root_w / 2.0,
        root_y + H / 2.0 + 4.0,
        escape(&root_label)
    );
}

/// The data model: the resource's classes and superclasses as UML boxes (general on top) with
/// their attributes, the classes they point at on the right, the classes pointing at them on
/// the left; a filled dot is a property this resource has. Below: every attribute as a table,
/// and the model as Mermaid to copy.
pub(super) fn model_panel(out: &mut String, view: &View, links: &PageLinks<'_>) {
    let m = &view.model;
    if m.boxes.is_empty() {
        return;
    }
    const LINE: f64 = 18.0;
    const HEAD: f64 = 30.0;
    const PAD: f64 = 10.0;
    const GAP_Y: f64 = 40.0;
    const SMALL: f64 = 28.0;
    let in_model: HashMap<&str, usize> = m
        .boxes
        .iter()
        .enumerate()
        .map(|(i, b)| (b.class.iri.as_str(), i))
        .collect();
    let laid: Vec<(Vec<Line<'_>>, usize)> =
        m.boxes.iter().map(|b| lines_of(b, &in_model)).collect();
    let box_w: Vec<f64> = m
        .boxes
        .iter()
        .zip(&laid)
        .map(|(b, (lines, more))| {
            let head = width(&name_of(&b.class), 7.8) + 2.0 * PAD;
            let rows = lines
                .iter()
                .map(|l| width(&l.text, 7.2) + 2.0 * PAD + 14.0)
                .fold(0.0_f64, f64::max);
            let more = if *more > 0 {
                width("+ 999 more", 7.2) + 2.0 * PAD
            } else {
                0.0
            };
            head.max(rows).max(more).clamp(170.0, 460.0)
        })
        .collect();
    let box_h: Vec<f64> = laid
        .iter()
        .map(|(lines, more)| {
            HEAD + (lines.len() + usize::from(*more > 0)).max(1) as f64 * LINE + PAD
        })
        .collect();
    // General on top: by depth, highest first.
    let mut order: Vec<usize> = (0..m.boxes.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(m.boxes[i].depth));
    // The classes pointing in, on the left.
    let left_w = m
        .incoming
        .iter()
        .map(|(s, _, _)| width(&cut(&name_of(s), 36), 7.4) + 2.0 * PAD)
        .fold(0.0_f64, f64::max);
    let left_label = m
        .incoming
        .iter()
        .map(|(_, _, a)| width(&format!("{} [{}]", a.name(), a.multiplicity()), 6.8))
        .fold(0.0_f64, f64::max);
    let mid_x = if m.incoming.is_empty() {
        24.0
    } else {
        16.0 + left_w + (left_label + 40.0).clamp(120.0, 280.0)
    };
    let mid_w = box_w.iter().fold(0.0_f64, |a, b| a.max(*b));
    let mut box_y = vec![0.0; m.boxes.len()];
    let mut y = 16.0;
    for &i in &order {
        box_y[i] = y;
        y += box_h[i] + GAP_Y;
    }
    let mid_h = y - GAP_Y + 16.0;
    // The classes pointed at, on the right, each near the rows that point at it.
    let mut targets: Vec<(&Link, Vec<f64>)> = Vec::new();
    for (i, (lines, more)) in laid.iter().enumerate() {
        let _ = more;
        for (k, l) in lines.iter().enumerate() {
            if let Some(t) = l.out_to {
                let ly = box_y[i] + HEAD + k as f64 * LINE + LINE / 2.0;
                match targets.iter_mut().find(|(x, _)| x.iri == t.iri) {
                    Some((_, ys)) => ys.push(ly),
                    None => targets.push((t, vec![ly])),
                }
            }
        }
    }
    let wanted = |ys: &Vec<f64>| ys.iter().sum::<f64>() / ys.len() as f64 - SMALL / 2.0;
    targets.sort_by(|a, b| wanted(&a.1).total_cmp(&wanted(&b.1)));
    let mut target_y: Vec<f64> = Vec::new();
    let mut next = 16.0_f64;
    for (_, ys) in &targets {
        let ty = wanted(ys).max(next);
        target_y.push(ty);
        next = ty + SMALL + 12.0;
    }
    let right_x = mid_x + mid_w + 150.0;
    let right_w = targets
        .iter()
        .map(|(t, _)| width(&cut(&name_of(t), 36), 7.4) + 2.0 * PAD)
        .fold(0.0_f64, f64::max);
    // One box per class pointing in, a line per association out of it.
    let mut sources: Vec<(&Link, Vec<(usize, &Member)>)> = Vec::new();
    for (s, b, a) in &m.incoming {
        match sources.iter_mut().find(|(x, _)| x.iri == s.iri) {
            Some((_, v)) => v.push((*b, a)),
            None => sources.push((s, vec![(*b, a)])),
        }
    }
    let source_h = |n: usize| (n as f64 * 16.0 + 12.0).max(SMALL);
    let left_h = sources
        .iter()
        .map(|(_, v)| source_h(v.len()) + 14.0)
        .sum::<f64>()
        + 16.0;
    let total_h = mid_h.max(next + 4.0).max(left_h);
    let total_w = if targets.is_empty() {
        mid_x + mid_w + 24.0
    } else {
        right_x + right_w + 24.0
    };
    let _ = write!(
        out,
        "<div class=\"card\"><p class=\"more\">From the shapes of the resource's classes. \
         <span class=\"dot used\"></span> this resource has it, <span class=\"dot\"></span> it does not. \
         Click a class to open its page.</p>\
         <div class=\"diagram\"><svg class=\"uml\" width=\"{total_w:.0}\" height=\"{total_h:.0}\" viewBox=\"0 0 {total_w:.0} {total_h:.0}\" role=\"img\" aria-label=\"Data model\">\
         <defs><marker id=\"mg-gen\" viewBox=\"0 0 12 12\" refX=\"12\" refY=\"6\" markerWidth=\"12\" markerHeight=\"12\" orient=\"auto-start-reverse\"><path d=\"M0 0L12 6L0 12z\" class=\"gen\"/></marker>\
         <marker id=\"mg-assoc\" viewBox=\"0 0 10 10\" refX=\"10\" refY=\"5\" markerWidth=\"8\" markerHeight=\"8\" orient=\"auto-start-reverse\"><path d=\"M0 0L10 5L0 10\" class=\"assoc-head\"/></marker></defs>"
    );
    let class_link = |out: &mut String,
                      l: &Link,
                      x: f64,
                      y: f64,
                      w: f64,
                      h: f64,
                      head_y: f64,
                      class: &str| {
        let _ = write!(
            out,
            "<a href=\"{}\"><g class=\"{class}\"><title>{}</title><rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" rx=\"4\"/>\
             <text class=\"head\" x=\"{:.1}\" y=\"{head_y:.1}\">{}</text></g></a>",
            escape(&(links.href)(l.iri.as_str())),
            escape(l.iri.as_str()),
            x + w / 2.0,
            escape(&cut(&name_of(l), 40))
        );
    };
    // Generalisation: from a class to its superclass's box.
    let pos: HashMap<usize, usize> = order.iter().enumerate().map(|(k, &i)| (i, k)).collect();
    for (i, b) in m.boxes.iter().enumerate() {
        for &j in &b.parents {
            let (ci, cj) = (pos[&i], pos[&j]);
            let cx = mid_x + box_w[i].min(box_w[j]) / 2.0;
            if cj + 1 == ci {
                let _ = write!(
                    out,
                    "<path class=\"generalization\" d=\"M{cx:.1} {:.1}V{:.1}\" marker-end=\"url(#mg-gen)\"/>",
                    box_y[i],
                    box_y[j] + box_h[j]
                );
            } else {
                let (yi, yj) = (box_y[i] + HEAD / 2.0, box_y[j] + HEAD / 2.0);
                let _ = write!(
                    out,
                    "<path class=\"generalization\" d=\"M{mid_x:.1} {yi:.1}H{:.1}V{yj:.1}H{mid_x:.1}\" marker-end=\"url(#mg-gen)\"/>",
                    mid_x - 14.0
                );
            }
        }
    }
    for (i, b) in m.boxes.iter().enumerate() {
        let (x, y, w, h) = (mid_x, box_y[i], box_w[i], box_h[i]);
        let own = if b.depth == 0 { "box own" } else { "box" };
        class_link(out, &b.class, x, y, w, h, y + 20.0, own);
        let _ = write!(
            out,
            "<path class=\"sep\" d=\"M{x:.1} {:.1}H{:.1}\"/>",
            y + HEAD - 4.0,
            x + w
        );
        let (lines, more) = &laid[i];
        for (k, l) in lines.iter().enumerate() {
            let ly = y + HEAD + k as f64 * LINE + LINE / 2.0;
            let tip = [
                l.member.label.clone(),
                Some(l.member.property.as_str().to_owned()),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n");
            let _ = write!(
                out,
                "<g class=\"member{}\"><title>{}</title><circle cx=\"{:.1}\" cy=\"{ly:.1}\" r=\"3.5\" class=\"dot{}\"/>\
                 <text x=\"{:.1}\" y=\"{:.1}\">{}</text></g>",
                if l.out_to.is_some() { " assoc" } else { "" },
                escape(&tip),
                x + PAD + 3.0,
                if l.member.used { " used" } else { "" },
                x + PAD + 12.0,
                ly + 4.0,
                escape(&l.text)
            );
            if let Some(t) = l.out_to {
                let k = targets
                    .iter()
                    .position(|(x, _)| x.iri == t.iri)
                    .unwrap_or(0);
                let (x1, x2, y2) = (x + w, right_x, target_y[k] + SMALL / 2.0);
                let mx = (x1 + x2) / 2.0;
                let _ = write!(
                    out,
                    "<path class=\"association{}\" d=\"M{x1:.1} {ly:.1}C{mx:.1} {ly:.1} {mx:.1} {y2:.1} {x2:.1} {y2:.1}\" marker-end=\"url(#mg-assoc)\"/>",
                    if l.member.used { " used" } else { "" }
                );
            }
        }
        if *more > 0 {
            let ly = y + HEAD + lines.len() as f64 * LINE + LINE / 2.0;
            let _ = write!(
                out,
                "<text class=\"more-line\" x=\"{:.1}\" y=\"{:.1}\">+ {more} more</text>",
                x + PAD,
                ly + 4.0
            );
        }
    }
    for (k, (t, _)) in targets.iter().enumerate() {
        class_link(
            out,
            t,
            right_x,
            target_y[k],
            right_w,
            SMALL,
            target_y[k] + 18.0,
            "box other",
        );
    }
    let mut sy = 16.0;
    for (s, members) in &sources {
        let h = source_h(members.len());
        class_link(out, s, 16.0, sy, left_w, h, sy + h / 2.0 + 4.0, "box other");
        for (j, (b, a)) in members.iter().enumerate() {
            let step = h / (members.len() as f64 + 1.0);
            let (x1, y1, x2, y2) = (
                16.0 + left_w,
                sy + step * (j as f64 + 1.0),
                mid_x,
                box_y[*b] + HEAD / 2.0,
            );
            let mx = (x1 + x2) / 2.0;
            let _ = write!(
                out,
                "<path class=\"association{}\" d=\"M{x1:.1} {y1:.1}C{mx:.1} {y1:.1} {mx:.1} {y2:.1} {x2:.1} {y2:.1}\" marker-end=\"url(#mg-assoc)\"><title>{}</title></path>\
                 <text class=\"edge-label\" x=\"{:.1}\" y=\"{:.1}\">{} [{}]</text>",
                if a.used { " used" } else { "" },
                escape(a.property.as_str()),
                x1 + 6.0,
                y1 - 4.0,
                escape(a.name()),
                a.multiplicity()
            );
        }
        sy += h + 14.0;
    }
    out.push_str("</svg></div></div>");

    // Every attribute and association, as a table.
    out.push_str(
        "<details class=\"card\"><summary>All attributes and associations</summary>\
         <table class=\"triples\"><thead><tr><th>Class</th><th>Property</th><th>Type</th><th>Multiplicity</th><th>This resource</th></tr></thead><tbody>",
    );
    for b in &m.boxes {
        for a in b.attributes.iter().chain(&b.associations) {
            out.push_str("<tr><td>");
            html_link(out, links, &b.class);
            let _ = write!(
                out,
                "</td><td title=\"{}\">{}</td><td>",
                escape(a.property.as_str()),
                escape(
                    &a.label
                        .clone()
                        .unwrap_or_else(|| short(a.property.as_str()))
                )
            );
            match &a.target {
                Some(t) => html_link(out, links, t),
                None => out.push_str(&escape(&a.kind)),
            }
            let _ = write!(
                out,
                "</td><td>{}</td><td>{}</td></tr>",
                a.multiplicity(),
                if a.used { "has it" } else { "" }
            );
        }
    }
    out.push_str("</tbody></table></details>");

    // The model as a Mermaid class diagram.
    let mermaid = mermaid(m);
    let _ = write!(
        out,
        "<details class=\"card\"><summary>Mermaid</summary>\
         <div class=\"code-tools\"><button type=\"button\" class=\"btn\" data-copy-from=\"mg-mermaid\" hidden>Copy</button></div>\
         <pre class=\"code\"><code id=\"mg-mermaid\">{}</code></pre></details>",
        escape(&mermaid)
    );
}

/// A row of a class box; an association to a class of the model reads as an attribute of
/// that type.
struct Line<'a> {
    text: String,
    member: &'a Member,
    out_to: Option<&'a Link>,
}

/// The rows of a class box, and how many attributes are not drawn.
fn lines_of<'a>(b: &'a ModelBox, in_model: &HashMap<&str, usize>) -> (Vec<Line<'a>>, usize) {
    let mut attrs: Vec<&Member> = b.attributes.iter().collect();
    attrs.extend(b.associations.iter().filter(|a| {
        a.target
            .as_ref()
            .is_some_and(|t| in_model.contains_key(t.iri.as_str()))
    }));
    // The ones this resource has, and required ones, first.
    attrs.sort_by_key(|a| !(a.used || a.min > 0));
    let more = attrs.len().saturating_sub(MAX_SHOWN_ATTRIBUTES);
    let mut lines: Vec<Line<'_>> = attrs
        .into_iter()
        .take(MAX_SHOWN_ATTRIBUTES)
        .map(|a| {
            let kind = match &a.target {
                Some(t) => local_name(t.iri.as_str()).to_owned(),
                None if a.kind.is_empty() => String::new(),
                None => a.kind.clone(),
            };
            let text = if kind.is_empty() {
                format!("{} [{}]", a.name(), a.multiplicity())
            } else {
                format!("{}: {kind} [{}]", a.name(), a.multiplicity())
            };
            Line {
                text: cut(&text, 56),
                member: a,
                out_to: None,
            }
        })
        .collect();
    for a in b.associations.iter().filter(|a| {
        a.target
            .as_ref()
            .is_some_and(|t| !in_model.contains_key(t.iri.as_str()))
    }) {
        lines.push(Line {
            text: cut(&format!("{} [{}]", a.name(), a.multiplicity()), 56),
            member: a,
            out_to: a.target.as_ref(),
        });
    }
    (lines, more)
}

/// A Mermaid class name: the local name with only letters, digits and `_`.
fn mermaid_name(iri: &str) -> String {
    let s: String = local_name(iri)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() || s.starts_with(|c: char| c.is_ascii_digit()) {
        format!("C{s}")
    } else {
        s
    }
}

/// The model as a Mermaid `classDiagram`.
fn mermaid(m: &Model) -> String {
    let mut out = String::from("classDiagram\n");
    for b in &m.boxes {
        let name = mermaid_name(b.class.iri.as_str());
        let _ = writeln!(out, "  class {name} {{");
        for a in &b.attributes {
            let kind = a.kind.split_whitespace().collect::<Vec<_>>().join("_");
            let sep = if kind.is_empty() { "" } else { " " };
            let _ = writeln!(
                out,
                "    +{kind}{sep}{} [{}]",
                mermaid_name(a.property.as_str()),
                a.multiplicity()
            );
        }
        out.push_str("  }\n");
    }
    for b in &m.boxes {
        let name = mermaid_name(b.class.iri.as_str());
        for &p in &b.parents {
            let _ = writeln!(
                out,
                "  {} <|-- {name}",
                mermaid_name(m.boxes[p].class.iri.as_str())
            );
        }
        for a in &b.associations {
            if let Some(t) = &a.target {
                let _ = writeln!(
                    out,
                    "  {name} --> \"{}\" {} : {}",
                    a.multiplicity(),
                    mermaid_name(t.iri.as_str()),
                    mermaid_name(a.property.as_str())
                );
            }
        }
    }
    for (s, b, a) in &m.incoming {
        let _ = writeln!(
            out,
            "  {} --> \"{}\" {} : {}",
            mermaid_name(s.iri.as_str()),
            a.multiplicity(),
            mermaid_name(m.boxes[*b].class.iri.as_str()),
            mermaid_name(a.property.as_str())
        );
    }
    out
}

/// The RDF tab: the resource's description as Turtle in the page, and the other formats a
/// click away (script), each to copy or download.
pub(super) fn rdf_panel(out: &mut String, links: &PageLinks<'_>) {
    let Some(rdf) = &links.rdf else {
        return;
    };
    out.push_str("<div class=\"card\"><div class=\"code-tools\"><span class=\"seg\" role=\"group\" aria-label=\"Format\">");
    for (name, url) in &links.documents {
        let _ = write!(
            out,
            "<a class=\"seg-btn\" href=\"{}\" data-format=\"{}\"{}>{}</a>",
            escape(url),
            escape(name),
            if *name == "ttl" {
                " aria-pressed=\"true\""
            } else {
                ""
            },
            format_label(name)
        );
    }
    let turtle_url = links
        .documents
        .iter()
        .find(|(n, _)| *n == "ttl")
        .map_or(String::new(), |(_, u)| u.clone());
    let _ = write!(
        out,
        "</span><span class=\"grow\"></span><button type=\"button\" class=\"btn\" data-copy-from=\"mg-rdf\" hidden>Copy</button>\
         <a class=\"btn\" id=\"mg-rdf-download\" href=\"{}\" download>Download</a></div>",
        escape(&turtle_url)
    );
    match &rdf.turtle {
        Some(text) => {
            let _ = write!(
                out,
                "<p class=\"more\" id=\"mg-rdf-note\">The resource's description, {} triples{}.</p>\
                 <pre class=\"code rdf-src\" tabindex=\"0\"><code id=\"mg-rdf\" data-format=\"ttl\">{}</code></pre>",
                rdf.triples,
                if rdf.truncated {
                    ", cut at the server's limit"
                } else {
                    ""
                },
                escape(text)
            );
        }
        None => {
            let _ = write!(
                out,
                "<p class=\"more\" id=\"mg-rdf-note\">The description has {} triples, too big to show here: \
                 pick a format to download it.</p><pre class=\"code rdf-src\" tabindex=\"0\" hidden><code id=\"mg-rdf\"></code></pre>",
                rdf.triples
            );
        }
    }
    out.push_str("</div>");
}

/// The RDF of a page: its Turtle when small enough to embed.
pub(crate) struct PageRdf {
    pub(crate) turtle: Option<String>,
    pub(crate) triples: usize,
    pub(crate) truncated: bool,
}
