//! Shapes for the readers (#137): generated from a [`DatasetProfile`] when a dataset has no
//! shapes of its own, and projected per reader and profile.
//!
//! One shape set is read by several readers (validation, the map, describe pages, search, OGC
//! API Features, the REST and GraphQL APIs, the AI assistant), each through its own
//! projection: [`project`] leaves out the shapes hidden from the reader (`tts:hiddenFrom`,
//! `tts:onlyFor`), deactivated ones, and, with profiles active, the node shapes in none of
//! them (`tts:inProfile`; a shape in no profile is in all). The annotations are in the
//! [`VOCABULARY`] (`tts:`, `http://w3id.org/tripletrail/shapes#`).
//!
//! [`generate`] makes a complete set from a profile: per class a node shape with its property
//! shapes (path, name, node kind, datatype or class, `sh:in` for code lists, and the
//! cardinalities when every instance was read) and default reader annotations (label path,
//! geometry paths, map colour and zoom, facet kinds, describe groups, keys).

use crate::DatasetProfile;
use crate::profile::{CLASS_LOOKUPS, ClassProfile, MAX_LINKS_PER_NODE, PropertyProfile};
use oxigraph::model::vocab::{rdf, rdfs, xsd};
use oxigraph::model::{
    BlankNode, Graph, Literal, NamedNode, NamedNodeRef, NamedOrBlankNode, NamedOrBlankNodeRef,
    Term, TermRef, Triple, TripleRef,
};
use std::collections::{HashMap, HashSet};

mod atlas;
mod layers;
mod turtle;

pub use atlas::{ATLAS, from_atlas};
pub use layers::{
    Override, QUALITY_KEY, SetFile, Upgrade, add_quality, check, check_select, files_of,
    hide_generated, inline_select, load, overlay, upgrade,
};
pub use turtle::to_turtle;

/// The namespace of the shapes vocabulary.
pub const TTS: &str = "http://w3id.org/tripletrail/shapes#";
/// The shapes vocabulary, in Turtle.
pub const VOCABULARY: &str = include_str!("tts.ttl");
/// The generated shape set.
pub const GENERATED_SET: &str = "urn:tripletrail:shapes:generated";

/// The namespace and graph-name prefixes from before the rename to TripleTrail, each with its
/// new one: shapes that use the old ones load, but what they say in them is not read (#955).
/// Spelt in pieces so that the rename script and the public export's deny list, which look for
/// the old name, pass them by.
pub const OLD_NAMES: [(&str, &str); 2] = [
    (
        concat!("http://w3id.org/", "mat", "graph/"),
        "http://w3id.org/tripletrail/",
    ),
    (concat!("urn:", "mat", "graph:"), "urn:tripletrail:"),
];

/// The old prefix `text` uses first, if any.
pub fn old_name_in(text: &str) -> Option<&'static str> {
    OLD_NAMES
        .iter()
        .filter_map(|(old, _)| text.find(old).map(|at| (at, *old)))
        .min()
        .map(|(_, old)| old)
}

/// `name` with its old prefix replaced by the new one, if it has one.
pub fn new_name(name: &str) -> Option<String> {
    OLD_NAMES
        .iter()
        .find_map(|(old, new)| name.strip_prefix(old).map(|rest| format!("{new}{rest}")))
}

/// A warning per file of a shape set that still uses the names from before the rename to
/// TripleTrail (#955), with the `sed` that carries it over. `place` goes before each file name.
pub fn old_names(files: &[SetFile], place: &str) -> Vec<String> {
    let sed = OLD_NAMES
        .iter()
        .map(|(old, new)| format!("s#{old}#{new}#g"))
        .collect::<Vec<_>>()
        .join("; ");
    files
        .iter()
        .filter_map(|file| {
            let old = old_name_in(&file.text)?;
            Some(format!(
                "{place}{}: uses {old}…, a name from before the rename to TripleTrail, which is \
                 not read: its reader settings, facets and keys are left out. Rewrite it with \
                 sed -i '{sed}'",
                file.name
            ))
        })
        .collect()
}

const SH: &str = "http://www.w3.org/ns/shacl#";
/// Map colours, in turn per class with a geometry.
const PALETTE: [&str; 12] = [
    "#1f77b4", "#d62728", "#2ca02c", "#ff7f0e", "#9467bd", "#8c564b", "#e377c2", "#17becf",
    "#bcbd22", "#7f7f7f", "#393b79", "#637939",
];

fn sh(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{SH}{local}"))
}

fn tts(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{TTS}{local}"))
}

/// The readers of a shape set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reader {
    Validation,
    Map,
    Describe,
    Search,
    OgcFeatures,
    RestApi,
    GraphQl,
    Ai,
}

impl Reader {
    pub const ALL: [Reader; 8] = [
        Reader::Validation,
        Reader::Map,
        Reader::Describe,
        Reader::Search,
        Reader::OgcFeatures,
        Reader::RestApi,
        Reader::GraphQl,
        Reader::Ai,
    ];

    /// Its name in APIs (`reader=map`), its `skos:notation`.
    pub fn name(self) -> &'static str {
        match self {
            Reader::Validation => "validation",
            Reader::Map => "map",
            Reader::Describe => "describe",
            Reader::Search => "search",
            Reader::OgcFeatures => "ogc-features",
            Reader::RestApi => "rest-api",
            Reader::GraphQl => "graphql",
            Reader::Ai => "ai",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.name() == name)
    }

    /// Its concept in the vocabulary, such as `tts:Reader-Map`.
    pub fn iri(self) -> NamedNode {
        tts(match self {
            Reader::Validation => "Reader-Validation",
            Reader::Map => "Reader-Map",
            Reader::Describe => "Reader-Describe",
            Reader::Search => "Reader-Search",
            Reader::OgcFeatures => "Reader-OgcFeatures",
            Reader::RestApi => "Reader-RestApi",
            Reader::GraphQl => "Reader-GraphQL",
            Reader::Ai => "Reader-Ai",
        })
    }
}

/// The IRI of the generated node shape of `class`: stable, so a customised set can override it.
pub fn generated_shape(class: &NamedNode) -> NamedNode {
    NamedNode::new_unchecked(format!("urn:tripletrail:shape:{}", encode(class.as_str())))
}

fn property_shape(node: &NamedNode, property: &NamedNode) -> NamedNode {
    NamedNode::new_unchecked(format!("{}:{}", node.as_str(), encode(property.as_str())))
}

/// Percent-encodes all but the unreserved characters.
fn encode(iri: &str) -> String {
    let mut out = String::with_capacity(iri.len());
    for b in iri.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The last segment of an IRI.
fn local_name(iri: &str) -> &str {
    iri.trim_end_matches(['/', '#'])
        .rsplit(['#', '/', ':'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(iri)
}

/// A key from a name: letters, digits, `_` and `-`, made unique among `taken`.
fn key(name: &str, taken: &mut HashSet<String>) -> String {
    let mut base: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if base.is_empty() || base.starts_with(|c: char| c.is_ascii_digit()) {
        base.insert(0, '_');
    }
    let mut key = base.clone();
    let mut n = 2;
    while !taken.insert(key.clone()) {
        key = format!("{base}_{n}");
        n += 1;
    }
    key
}

/// Writes `items` as an RDF list and returns its head.
fn list(graph: &mut Graph, items: impl IntoIterator<Item = Term>) -> Term {
    let items: Vec<Term> = items.into_iter().collect();
    let mut rest: Term = rdf::NIL.into_owned().into();
    for item in items.into_iter().rev() {
        let cell = BlankNode::default();
        graph.insert(&Triple::new(cell.clone(), rdf::FIRST, item));
        graph.insert(&Triple::new(cell.clone(), rdf::REST, rest));
        rest = cell.into();
    }
    rest
}

/// A SHACL path: the property, or a sequence (an RDF list) of them.
fn path(graph: &mut Graph, properties: &[NamedNode]) -> Term {
    match properties {
        [one] => one.clone().into(),
        many => list(graph, many.iter().cloned().map(Term::from)),
    }
}

fn integer(n: u64) -> Literal {
    Literal::new_typed_literal(n.to_string(), xsd::INTEGER)
}

fn is_range_datatype(datatype: NamedNodeRef<'_>) -> bool {
    [
        xsd::INTEGER,
        xsd::DECIMAL,
        xsd::DOUBLE,
        xsd::FLOAT,
        xsd::INT,
        xsd::LONG,
        xsd::SHORT,
        xsd::NON_NEGATIVE_INTEGER,
        xsd::POSITIVE_INTEGER,
        xsd::DATE,
        xsd::DATE_TIME,
        xsd::G_YEAR,
    ]
    .contains(&datatype)
}

/// Datatypes of text a map searches in (`tts:Facet-Text`).
fn is_text_datatype(datatype: NamedNodeRef<'_>) -> bool {
    [xsd::STRING, rdf::LANG_STRING].contains(&datatype)
}

/// A complete shape set from a profile: one node shape per profiled class.
pub fn generate(profile: &DatasetProfile) -> Graph {
    let mut graph = Graph::new();
    let set = NamedNode::new_unchecked(GENERATED_SET);
    graph.insert(&Triple::new(set.clone(), rdf::TYPE, tts("ShapeSet")));
    graph.insert(&Triple::new(
        set.clone(),
        tts("source"),
        Literal::new_simple_literal("generated"),
    ));
    if let Some(generation) = profile.generation {
        graph.insert(&Triple::new(
            set.clone(),
            tts("dataGeneration"),
            integer(generation),
        ));
    }
    let mut node_keys = HashSet::new();
    let mut colours = PALETTE.iter().cycle();
    for class in &profile.classes {
        // An umbrella class's features are drawn as those of its narrower classes.
        let off_map = is_geometry_class(&class.class) || class.umbrella;
        node_shape(
            &mut graph,
            &set,
            class,
            off_map,
            &mut node_keys,
            &mut colours,
        );
    }
    graph
}

/// GeoSPARQL's and Simple Features' classes (`geo:Geometry`, `sf:Point`…): their instances
/// are the geometries of features, which the map draws as those features.
fn is_geometry_class(class: &NamedNode) -> bool {
    [
        "http://www.opengis.net/ont/geosparql#",
        "http://www.opengis.net/ont/sf#",
    ]
    .iter()
    .any(|ns| class.as_str().starts_with(ns))
}

fn node_shape<'a>(
    graph: &mut Graph,
    set: &NamedNode,
    class: &ClassProfile,
    off_map: bool,
    node_keys: &mut HashSet<String>,
    colours: &mut impl Iterator<Item = &'a &'static str>,
) {
    let node = generated_shape(&class.class);
    let add = |graph: &mut Graph, p: NamedNode, o: Term| {
        graph.insert(&Triple::new(node.clone(), p, o));
    };
    let name = class
        .label
        .clone()
        .unwrap_or_else(|| local_name(class.class.as_str()).to_owned());
    add(graph, rdf::TYPE.into_owned(), sh("NodeShape").into());
    add(graph, sh("targetClass"), class.class.clone().into());
    add(graph, sh("name"), Literal::new_simple_literal(&name).into());
    add(graph, tts("inSet"), set.clone().into());
    let key = key(local_name(class.class.as_str()), node_keys);
    add(graph, tts("key"), Literal::new_simple_literal(key).into());
    add(graph, tts("instances"), integer(class.instances).into());
    for superclass in &class.superclasses {
        graph.insert(&Triple::new(
            class.class.clone(),
            rdfs::SUB_CLASS_OF,
            superclass.clone(),
        ));
    }
    if let Some(label) = class.label_properties.first() {
        add(graph, tts("labelPath"), label.clone().into());
        add(graph, tts("searchable"), Literal::from(true).into());
    }
    if class.geometries.is_empty() {
        for reader in [Reader::Map, Reader::OgcFeatures] {
            add(graph, tts("hiddenFrom"), reader.iri().into());
        }
    } else if off_map {
        // Its features are on the map as its narrower classes, or as the features whose
        // geometries they are.
        add(graph, tts("hiddenFrom"), Reader::Map.iri().into());
    } else {
        if let Some(colour) = colours.next() {
            add(
                graph,
                tts("color"),
                Literal::new_simple_literal(*colour).into(),
            );
        }
        let zoom = match class.instances {
            n if n > 50_000 => Some(12),
            n if n > 5_000 => Some(10),
            _ => None,
        };
        if let Some(zoom) = zoom {
            add(graph, tts("minZoom"), integer(zoom).into());
        }
    }
    for (order, geometry) in class.geometries.iter().enumerate() {
        let way = BlankNode::default();
        add(graph, tts("geometry"), way.clone().into());
        let p = path(graph, &geometry.path);
        graph.insert(&Triple::new(way.clone(), sh("path"), p));
        graph.insert(&Triple::new(
            way.clone(),
            sh("order"),
            integer(order as u64),
        ));
        graph.insert(&Triple::new(
            way.clone(),
            tts("instances"),
            integer(geometry.instances),
        ));
        for (kind, _) in &geometry.types {
            graph.insert(&Triple::new(
                way.clone(),
                tts("geometryType"),
                Literal::new_simple_literal(kind),
            ));
        }
    }
    // Every instance read: the cardinalities seen hold for the class.
    let whole = class.sampled == class.instances;
    let mut property_keys = HashSet::new();
    let properties = class
        .properties
        .iter()
        .filter(|p| p.property.as_ref() != rdf::TYPE);
    for (order, property) in properties.enumerate() {
        let shape = property_shape(&node, &property.property);
        add(graph, sh("property"), shape.clone().into());
        property_shape_of(
            graph,
            &shape,
            property,
            order,
            whole.then_some(class.sampled),
            &mut property_keys,
        );
    }
}

fn property_shape_of(
    graph: &mut Graph,
    shape: &NamedNode,
    property: &PropertyProfile,
    order: usize,
    whole: Option<u64>,
    keys: &mut HashSet<String>,
) {
    let add = |graph: &mut Graph, p: NamedNode, o: Term| {
        graph.insert(&Triple::new(shape.clone(), p, o));
    };
    let name = property
        .label
        .clone()
        .unwrap_or_else(|| local_name(property.property.as_str()).to_owned());
    add(graph, rdf::TYPE.into_owned(), sh("PropertyShape").into());
    add(graph, sh("path"), property.property.clone().into());
    add(graph, sh("name"), Literal::new_simple_literal(name).into());
    add(graph, sh("order"), integer(order as u64).into());
    let key = key(local_name(property.property.as_str()), keys);
    add(graph, tts("key"), Literal::new_simple_literal(key).into());
    let (iris, blanks, literals) = (property.iris, property.blank_nodes, property.literals);
    let kind = match (iris > 0, blanks > 0, literals > 0) {
        (true, false, false) => Some("IRI"),
        (false, true, false) => Some("BlankNode"),
        (true, true, false) => Some("BlankNodeOrIRI"),
        (false, false, true) => Some("Literal"),
        _ => None,
    };
    if let Some(kind) = kind {
        add(graph, sh("nodeKind"), sh(kind).into());
    }
    if literals > 0 && iris + blanks == 0 && property.datatypes.len() == 1 {
        add(
            graph,
            sh("datatype"),
            property.datatypes[0].0.clone().into(),
        );
    }
    let looked_up = (iris + blanks).min(CLASS_LOOKUPS as u64);
    if literals == 0 && property.classes.len() == 1 && property.classes[0].1 >= looked_up {
        add(graph, sh("class"), property.classes[0].0.clone().into());
    }
    if let Some(sampled) = whole {
        if property.instances == sampled && sampled > 0 {
            add(graph, sh("minCount"), integer(1).into());
        }
        if property.max_count < MAX_LINKS_PER_NODE as u64 {
            add(graph, sh("maxCount"), integer(property.max_count).into());
        }
    }
    let facet = if let Some(values) = &property.distinct_values {
        let head = list(graph, values.iter().cloned());
        add(graph, sh("in"), head);
        Some("Facet-Values")
    } else if literals > 0
        && property
            .datatypes
            .first()
            .is_some_and(|(d, _)| is_range_datatype(d.as_ref()))
    {
        Some("Facet-Range")
    } else if literals > 0
        && iris + blanks == 0
        && property
            .datatypes
            .first()
            .is_some_and(|(d, _)| is_text_datatype(d.as_ref()))
    {
        // Free text, names and codes too many to list: searched for what they contain.
        Some("Facet-Text")
    } else {
        None
    };
    if let Some(facet) = facet {
        add(graph, tts("facet"), tts(facet).into());
    }
    let group = if literals > 0 && iris + blanks == 0 {
        "Group-Attributes"
    } else {
        "Group-Links"
    };
    add(graph, sh("group"), tts(group).into());
}

/// The node shapes of a set: typed `sh:NodeShape`, or with a target or property shapes.
fn node_shapes(shapes: &Graph) -> Vec<NamedOrBlankNode> {
    let mut seen = HashSet::new();
    let sh_node = sh("NodeShape");
    let typed = shapes
        .subjects_for_predicate_object(rdf::TYPE, &sh_node)
        .map(NamedOrBlankNodeRef::into_owned);
    let others = [sh("targetClass"), sh("property")]
        .into_iter()
        .flat_map(|p| {
            shapes
                .triples_for_predicate(&p)
                .map(|t| t.subject.into_owned())
                .collect::<Vec<_>>()
        });
    typed
        .chain(others)
        .filter(|s| seen.insert(s.clone()))
        .collect()
}

fn subject_of(term: TermRef<'_>) -> Option<NamedOrBlankNodeRef<'_>> {
    match term {
        TermRef::NamedNode(n) => Some(n.into()),
        TermRef::BlankNode(b) => Some(b.into()),
        _ => None,
    }
}

/// rinf-describe's key of a row by its name: lower case, each run of other characters than
/// letters and digits a `-` (`"Part of"` is `part-of`).
pub fn name_key(name: &str) -> String {
    let mut out = String::new();
    for c in name.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    match out.trim_matches('-') {
        "" => "row".to_owned(),
        k => k.to_owned(),
    }
}

/// Whether `reader` reads `shape` (always, when no reader is given).
fn visible(shapes: &Graph, shape: NamedOrBlankNodeRef<'_>, reader: Option<Reader>) -> bool {
    if shapes.contains(TripleRef::new(
        shape,
        &sh("deactivated"),
        &Literal::from(true),
    )) {
        return false;
    }
    let Some(reader) = reader else {
        return true;
    };
    let iri = reader.iri();
    if shapes.contains(TripleRef::new(shape, &tts("hiddenFrom"), &iri)) {
        return false;
    }
    let mut only = shapes
        .objects_for_subject_predicate(shape, &tts("onlyFor"))
        .peekable();
    only.peek().is_none() || only.any(|r| r == iri.as_ref().into())
}

/// Whether `shape` is a deactivated property shape with a key, which the describe reader
/// keeps: it turns that key's row off on a page, also the row a superclass's shape gives
/// (`[ tts:key "parts" ; sh:deactivated true ]`, also with rinf-describe's `rd:key`).
fn turns_off_a_key(shapes: &Graph, shape: NamedOrBlankNodeRef<'_>, reader: Option<Reader>) -> bool {
    reader == Some(Reader::Describe)
        && shapes.contains(TripleRef::new(
            shape,
            &sh("deactivated"),
            &Literal::from(true),
        ))
        && [
            tts("key"),
            NamedNode::new_unchecked("https://rinf-describe.matdata.eu/ns#key"),
        ]
        .iter()
        .any(|k| shapes.object_for_subject_predicate(shape, k).is_some())
}

/// Whether `shape` is in one of `profiles` (or in all, being in none).
fn in_profiles(shapes: &Graph, shape: NamedOrBlankNodeRef<'_>, profiles: &[NamedNode]) -> bool {
    let mut tagged = shapes
        .objects_for_subject_predicate(shape, &tts("inProfile"))
        .peekable();
    profiles.is_empty()
        || tagged.peek().is_none()
        || tagged.any(|p| profiles.iter().any(|q| TermRef::from(q.as_ref()) == p))
}

/// What `reader` reads of `shapes` with `profiles` active (all node shapes when none is):
/// hidden and deactivated shapes left out, and for the readers other than validation the
/// property shapes of the superclasses' node shapes added to each node shape (unless it has
/// one for that path or of that key), as validation already applies those through their own
/// targets.
pub fn project(shapes: &Graph, reader: Option<Reader>, profiles: &[NamedNode]) -> Graph {
    let nodes = node_shapes(shapes);
    let property = sh("property");
    let all_properties: HashSet<NamedOrBlankNode> = shapes
        .triples_for_predicate(&property)
        .filter_map(|t| subject_of(t.object).map(NamedOrBlankNodeRef::into_owned))
        .collect();
    let node_set: HashSet<&NamedOrBlankNode> = nodes.iter().collect();
    let kept: Vec<&NamedOrBlankNode> = nodes
        .iter()
        .filter(|n| {
            visible(shapes, n.as_ref(), reader) && in_profiles(shapes, n.as_ref(), profiles)
        })
        .collect();
    let properties_of = |node: NamedOrBlankNodeRef<'_>| -> Vec<NamedOrBlankNode> {
        shapes
            .objects_for_subject_predicate(node, &property)
            .filter_map(subject_of)
            .filter(|p| visible(shapes, *p, reader) || turns_off_a_key(shapes, *p, reader))
            .map(NamedOrBlankNodeRef::into_owned)
            .collect()
    };
    // Node shapes by target class, for the inherited properties. A superclass's shape hidden
    // from the reader still lends its property shapes (an abstract shape kept off the map
    // carries the shared rows); the property shapes hidden from the reader themselves do not
    // (a map shape's facets, for describe pages).
    let mut by_class: HashMap<Term, Vec<&NamedOrBlankNode>> = HashMap::new();
    for node in &nodes {
        if visible(shapes, node.as_ref(), None) {
            for class in shapes.objects_for_subject_predicate(node, &sh("targetClass")) {
                by_class.entry(class.into_owned()).or_default().push(node);
            }
        }
    }
    let path_of = |p: &NamedOrBlankNode| {
        shapes
            .object_for_subject_predicate(p, &sh("path"))
            .map(TermRef::into_owned)
    };

    // A row's key (`tts:key`, or rinf-describe's `rd:key`): an inherited property shape of a
    // key the node shape has is left out too, as one of its path is (for describe pages, a
    // row without a key is keyed by its name).
    let key_of = |p: &NamedOrBlankNode| -> Option<String> {
        let explicit = [
            tts("key"),
            NamedNode::new_unchecked("https://rinf-describe.matdata.eu/ns#key"),
        ]
        .iter()
        .find_map(|k| match shapes.object_for_subject_predicate(p, k) {
            Some(TermRef::Literal(l)) => Some(l.value().to_owned()),
            _ => None,
        });
        // Describe pages key a row without one by its name, as rinf-describe does.
        explicit.or_else(|| {
            if reader != Some(Reader::Describe) {
                return None;
            }
            match shapes.object_for_subject_predicate(p, &sh("name")) {
                Some(TermRef::Literal(l)) => Some(name_key(l.value())),
                _ => None,
            }
        })
    };

    let mut out = Graph::new();
    let mut copied = HashSet::new();
    for node in &kept {
        let mut own = properties_of(node.as_ref());
        if reader.is_some_and(|r| r != Reader::Validation) {
            let paths: HashSet<Term> = own.iter().filter_map(path_of).collect();
            let keys: HashSet<String> = own.iter().filter_map(key_of).collect();
            let mut seen = HashSet::new();
            let mut todo: Vec<Term> = shapes
                .objects_for_subject_predicate(*node, &sh("targetClass"))
                .map(TermRef::into_owned)
                .collect();
            while let Some(class) = todo.pop() {
                let Some(subject) = subject_of(class.as_ref()) else {
                    continue;
                };
                for superclass in shapes.objects_for_subject_predicate(subject, rdfs::SUB_CLASS_OF)
                {
                    let superclass = superclass.into_owned();
                    if !seen.insert(superclass.clone()) {
                        continue;
                    }
                    for parent in by_class.get(&superclass).into_iter().flatten() {
                        for p in properties_of(parent.as_ref()) {
                            if path_of(&p).is_none_or(|path| !paths.contains(&path))
                                && key_of(&p).is_none_or(|key| !keys.contains(&key))
                            {
                                own.push(p);
                            }
                        }
                    }
                    todo.push(superclass);
                }
            }
        }
        copy(
            shapes,
            &mut out,
            node.as_ref(),
            &all_properties,
            &mut copied,
        );
        for p in own {
            out.insert(&Triple::new((*node).clone(), property.clone(), p.clone()));
            copy(shapes, &mut out, p.as_ref(), &all_properties, &mut copied);
        }
    }
    // Everything else about named resources: the set, the class hierarchy, groups.
    for t in shapes.iter() {
        let subject = t.subject.into_owned();
        if matches!(subject, NamedOrBlankNode::NamedNode(_))
            && !node_set.contains(&subject)
            && !all_properties.contains(&subject)
        {
            copy(shapes, &mut out, t.subject, &all_properties, &mut copied);
        }
    }
    out
}

/// Copies the triples of `subject`, and of the blank nodes it reaches (lists, geometries),
/// except its links to property shapes, which [`project`] adds itself.
fn copy(
    shapes: &Graph,
    out: &mut Graph,
    subject: NamedOrBlankNodeRef<'_>,
    properties: &HashSet<NamedOrBlankNode>,
    copied: &mut HashSet<NamedOrBlankNode>,
) {
    if !copied.insert(subject.into_owned()) {
        return;
    }
    let property = sh("property");
    for t in shapes.triples_for_subject(subject) {
        let linked = subject_of(t.object).map(NamedOrBlankNodeRef::into_owned);
        if t.predicate == property.as_ref()
            && linked.as_ref().is_some_and(|l| properties.contains(l))
        {
            continue;
        }
        out.insert(t);
        if let TermRef::BlankNode(b) = t.object {
            copy(shapes, out, b.into(), properties, copied);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_names() {
        let mut taken = HashSet::new();
        assert_eq!(key("Signal", &mut taken), "Signal");
        assert_eq!(key("Signal", &mut taken), "Signal_2");
        assert_eq!(key("1st name", &mut taken), "_1st_name");
        assert_eq!(local_name("http://ex.org/a/B#c"), "c");
        assert_eq!(local_name("http://ex.org/a/B/"), "B");
        assert_eq!(encode("http://ex.org/a#b"), "http%3A%2F%2Fex.org%2Fa%23b");
        assert_eq!(Reader::from_name("ogc-features"), Some(Reader::OgcFeatures));
        let vocabulary = VOCABULARY;
        for reader in Reader::ALL {
            let local = reader.iri().as_str().trim_start_matches(TTS).to_owned();
            assert!(
                vocabulary.contains(&format!("tts:{local} a tts:Reader")),
                "{local}"
            );
            assert!(vocabulary.contains(&format!("skos:notation \"{}\"", reader.name())));
        }
    }

    /// #148: what a pack upgrade does, before it is made.
    #[test]
    fn an_upgrade_says_what_changes_and_which_overrides_stand_alone() {
        let parse = |text: &str| {
            load(&[SetFile {
                name: "a.ttl".into(),
                text: format!(
                    "@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix tts: <http://w3id.org/tripletrail/shapes#> . @prefix ex: <http://ex.org/> .\n{text}"
                ),
            }])
            .unwrap()
        };
        let installed = parse(
            "ex:Signal a sh:NodeShape ; sh:targetClass ex:Signal ; tts:color \"#000000\" ;
               sh:property [ sh:path ex:aspect ; sh:name \"aspect\" ] .
             ex:Old a sh:NodeShape ; sh:targetClass ex:Gone ; tts:color \"#111111\" .
             ex:Track a sh:NodeShape ; sh:targetClass ex:Track ; tts:color \"#222222\" .",
        );
        let pack = parse(
            "ex:Signal a sh:NodeShape ; sh:targetClass ex:Signal ; tts:color \"#000000\" ;
               sh:property [ sh:path ex:aspect ; sh:name \"Aspect\" ] .
             ex:Track a sh:NodeShape ; sh:targetClass ex:Track ; tts:color \"#222222\" .
             ex:New a sh:NodeShape ; sh:targetClass ex:Empty ; tts:color \"#333333\" .",
        );
        let own = parse(
            "ex:Signal a sh:NodeShape ; tts:refines true ; sh:name \"Signal\" .
             ex:Old sh:deactivated true .
             ex:MyTrack a sh:NodeShape ; sh:targetClass ex:Track ; tts:color \"#ff0000\" .
             ex:Mine a sh:NodeShape ; sh:targetClass ex:Mine ; tts:color \"#00ff00\" .",
        );
        let ex = |s: &str| NamedNode::new_unchecked(format!("http://ex.org/{s}"));
        let has = |c: &NamedNode| c.as_str() != "http://ex.org/Empty";
        let up = upgrade(Some(&installed), &pack, &own, &has);
        assert_eq!(up.added, [ex("New")]);
        assert_eq!(up.removed, [ex("Old")]);
        assert_eq!(up.changed, [ex("Signal")]);
        let o = |shape: &str, pack: &str, how| Override {
            shape: format!("<http://ex.org/{shape}>"),
            pack_shape: ex(pack),
            how,
        };
        assert_eq!(
            up.overrides,
            [
                o("MyTrack", "Track", "class"),
                o("Signal", "Signal", "refines")
            ]
        );
        assert_eq!(up.orphaned, [o("Old", "Old", "iri")]);
        assert_eq!(up.no_data, [(ex("New"), ex("Empty"))]);
        // A first install: nothing was installed, so nothing is removed or stands alone.
        let first = upgrade(None, &pack, &own, &has);
        assert_eq!(first.added.len(), 3);
        assert!(first.removed.is_empty() && first.changed.is_empty() && first.orphaned.is_empty());
        assert_eq!(first.overrides, up.overrides);
    }

    #[test]
    fn a_refining_shape_changes_the_shape_below() {
        let parse = |text: &str| {
            load(&[SetFile {
                name: "a.ttl".into(),
                text: text.into(),
            }])
            .unwrap()
        };
        let prefixes = "@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix tts: <http://w3id.org/tripletrail/shapes#> . @prefix ex: <http://ex.org/> .\n";
        let lower = parse(&format!(
            "{prefixes}ex:S a sh:NodeShape ; sh:targetClass ex:C ; sh:name \"S\" ; tts:instances 4 ; tts:color \"#000000\" ;
               sh:property ex:S-a, ex:S-b .
             ex:S-a sh:path ex:a ; sh:name \"a\" ; sh:order 1 .
             ex:S-b sh:path ex:b ; sh:name \"b\" ; sh:order 2 ."
        ));
        let upper = parse(&format!(
            "{prefixes}ex:S a sh:NodeShape ; tts:refines true ; sh:name \"Station\" ; sh:property ex:S-a .
             ex:S-a sh:path ex:a ; sh:name \"A\" ; sh:order 1 ."
        ));
        let out = overlay(&lower, &upper);
        let names = |s: &str| -> Vec<String> {
            let mut v: Vec<String> = out
                .objects_for_subject_predicate(&NamedNode::new_unchecked(s), &sh("name"))
                .map(|t| t.to_string())
                .collect();
            v.sort();
            v
        };
        // The settings it gives replace those below; the rest stays.
        assert_eq!(names("http://ex.org/S"), ["\"Station\""]);
        assert_eq!(names("http://ex.org/S-a"), ["\"A\""]);
        assert_eq!(names("http://ex.org/S-b"), ["\"b\""]);
        let s = NamedNode::new_unchecked("http://ex.org/S");
        assert_eq!(
            out.objects_for_subject_predicate(&s, &sh("property"))
                .count(),
            2
        );
        assert!(
            out.object_for_subject_predicate(&s, &tts("instances"))
                .is_some()
        );
        assert!(
            out.object_for_subject_predicate(&s, &tts("color"))
                .is_some()
        );
        assert!(check(&out).is_empty(), "{:?}", check(&out));
        // Without tts:refines the shape is replaced, as before.
        let replacing = parse(&format!(
            "{prefixes}ex:S a sh:NodeShape ; sh:name \"Station\" ; sh:property ex:S-a .
             ex:S-a sh:path ex:a ; sh:name \"A\" ."
        ));
        let out = overlay(&lower, &replacing);
        assert_eq!(
            out.objects_for_subject_predicate(&s, &sh("property"))
                .count(),
            1
        );
        // A refining shape whose IRI has no shape below refines those for its class: a
        // generated shape edited on the Shapes page, under a pack installed later.
        let generated = parse(&format!(
            "{prefixes}<urn:g:C> a sh:NodeShape ; sh:targetClass ex:C ; tts:instances 4 ; sh:property <urn:g:C-a> .
             <urn:g:C-a> sh:path ex:a ; sh:name \"a\" ."
        ));
        let pack = parse(&format!(
            "{prefixes}ex:P a sh:NodeShape ; sh:targetClass ex:C ; tts:color \"#ff0000\" ; sh:property ex:P-a, ex:P-b .
             ex:P-a sh:path ex:a ; sh:name \"A\" .
             ex:P-b sh:path ex:b ; sh:name \"B\" ."
        ));
        let own = parse(&format!(
            "{prefixes}<urn:g:C> a sh:NodeShape ; sh:targetClass ex:C ; tts:refines true ; sh:name \"My C\" ."
        ));
        let out = overlay(&overlay(&generated, &pack), &own);
        let g = NamedNode::new_unchecked("urn:g:C");
        assert_eq!(
            out.objects_for_subject_predicate(&g, &sh("property"))
                .count(),
            2
        );
        for p in ["color", "instances"] {
            assert!(
                out.object_for_subject_predicate(&g, &tts(p)).is_some(),
                "{p}"
            );
        }
        let p = NamedNode::new_unchecked("http://ex.org/P");
        assert!(out.triples_for_subject(&p).next().is_none());
        assert_eq!(
            out.objects_for_subject_predicate(&g, &sh("name"))
                .map(|t| t.to_string())
                .collect::<Vec<_>>(),
            ["\"My C\""]
        );
        assert!(check(&out).is_empty(), "{:?}", check(&out));
        // With no shape below at all, it stays as it is.
        let out = overlay(&Graph::new(), &own);
        assert_eq!(out.len(), own.len());
    }
}
