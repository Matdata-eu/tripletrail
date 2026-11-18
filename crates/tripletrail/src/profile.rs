//! The profile of a dataset (#137): what it contains, for generated shapes and the readers that
//! work from them (map, describe pages, search, OGC API Features, REST API).
//!
//! Per class: how many instances, and from a sample of them the properties they use, how
//! often, with which kinds of values (datatypes, languages, target classes, the values
//! themselves for small code lists), the label properties, and the paths by which they reach
//! a geometry (direct, `geo:hasGeometry/geo:asWKT`, or through up to [`MAX_HOPS`] links, such
//! as `era:netReference/geo:hasGeometry/geo:asWKT`) with the geometry types found.
//!
//! The class counts come from [`Engine::dataset_statistics`]; everything else is read from at
//! most [`ProfileOptions::sample`] instances per class, reading at most [`MAX_LINKS_PER_NODE`]
//! links of each node (so counts on a node with more are a lower bound), so a profile costs
//! about the same on a large dataset as on a small one. It stops when the engine's
//! cancellation token is cancelled. On the tripletrail store it is kept until the data changes.

use crate::{Engine, Error, ReadableGraphs, RowRules};
use oxigraph::model::vocab::{rdf, rdfs};
use oxigraph::model::{
    GraphNameRef, Literal, NamedNode, NamedNodeRef, NamedOrBlankNode, NamedOrBlankNodeRef, Quad,
    Term, TermRef,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tripletrail_store::{Dataset, Snapshot};

const GEO: &str = "http://www.opengis.net/ont/geosparql#";
/// Literal datatypes that hold a geometry.
const GEOMETRY_DATATYPES: [&str; 5] = [
    "wktLiteral",
    "geoJSONLiteral",
    "gmlLiteral",
    "kmlLiteral",
    "dggsLiteral",
];
/// Properties read as an entity's label, best first.
pub const LABEL_PROPERTIES: [&str; 8] = [
    "http://www.w3.org/2004/02/skos/core#prefLabel",
    "http://www.w3.org/2000/01/rdf-schema#label",
    "https://schema.org/name",
    "http://schema.org/name",
    "http://purl.org/dc/terms/title",
    "http://purl.org/dc/elements/1.1/title",
    "http://xmlns.com/foaf/0.1/name",
    "http://www.w3.org/2004/02/skos/core#altLabel",
];
/// The longest link path searched for a geometry, the geometry property included.
pub const MAX_HOPS: usize = 4;
/// Distinct values kept per property: up to this many, the values are listed (a code list).
pub const MAX_LISTED_VALUES: usize = 20;
/// Objects of one property whose classes are looked up, per class.
pub(crate) const CLASS_LOOKUPS: usize = 200;
/// Nodes visited per instance while searching for geometry paths.
const PATH_NODES: usize = 200;
/// Objects followed per property and node while searching for geometry paths.
const PATH_FANOUT: usize = 2;
/// Links read per node: a hub (a code-list or dataset node with millions of triples) costs no
/// more than this, and the counts of its properties are a lower bound.
pub const MAX_LINKS_PER_NODE: usize = 4096;
/// A literal longer than this (in bytes) among the values makes a property no code list.
const MAX_LISTED_LENGTH: usize = 256;

/// How much of the dataset a profile reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileOptions {
    /// Instances read per class.
    pub sample: usize,
    /// Of those, the instances whose links are followed to find geometry paths.
    pub path_sample: usize,
    /// Classes profiled, most instances first.
    pub max_classes: usize,
}

impl Default for ProfileOptions {
    fn default() -> Self {
        Self {
            sample: 200,
            path_sample: 20,
            max_classes: 500,
        }
    }
}

/// What [`Engine::dataset_profile`] returns.
#[derive(Clone, Debug, Default)]
pub struct DatasetProfile {
    /// The version of the data it describes (on the tripletrail store).
    pub generation: Option<u64>,
    pub options: ProfileOptions,
    /// Classes with instances, most first.
    pub classes: Vec<ClassProfile>,
    /// Classes in the dataset, beyond `max_classes` too.
    pub classes_total: usize,
}

#[derive(Clone, Debug)]
pub struct ClassProfile {
    pub class: NamedNode,
    /// The class's own label in the data, if any.
    pub label: Option<String>,
    /// `rdfs:subClassOf` of the class in the data.
    pub superclasses: Vec<NamedNode>,
    pub instances: u64,
    /// Instances read.
    pub sampled: u64,
    /// Properties of the sampled instances, the most used first.
    pub properties: Vec<PropertyProfile>,
    /// The label properties the sampled instances use ([`LABEL_PROPERTIES`] order).
    pub label_properties: Vec<NamedNode>,
    /// Paths to a geometry, the most used first.
    pub geometries: Vec<GeometryPath>,
    /// Every instance of the class, read in full and not only the sample, also has another
    /// type, and every such other type has fewer instances: an umbrella over narrower classes
    /// (RINF's `era:InfrastructureElement` over operational points, tracks and sections).
    pub umbrella: bool,
}

#[derive(Clone, Debug)]
pub struct PropertyProfile {
    pub property: NamedNode,
    /// The property's own label in the data, if any.
    pub label: Option<String>,
    /// Sampled instances with at least one value.
    pub instances: u64,
    /// Values on the sampled instances.
    pub values: u64,
    /// Fewest and most values on one sampled instance (0 when some have none).
    pub min_count: u64,
    pub max_count: u64,
    pub iris: u64,
    pub blank_nodes: u64,
    pub literals: u64,
    /// Literal datatypes, the most used first.
    pub datatypes: Vec<(NamedNode, u64)>,
    /// Language tags of language-tagged strings, the most used first.
    pub languages: Vec<(String, u64)>,
    /// Classes of the IRI and blank node values (of up to 200 looked up), the most used first.
    pub classes: Vec<(NamedNode, u64)>,
    /// Every distinct value, when there are at most [`MAX_LISTED_VALUES`], each is used twice
    /// on average and no literal is longer than 256 bytes (a code list).
    pub distinct_values: Option<Vec<Term>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeometryPath {
    /// The properties from an instance to the geometry literal.
    pub path: Vec<NamedNode>,
    pub datatype: NamedNode,
    /// Path-searched instances that reach a geometry by this path.
    pub instances: u64,
    /// Geometry types (`POINT`, `LINESTRING`…), the most found first.
    pub types: Vec<(String, u64)>,
}

/// A kept profile: the generation and options it was computed for.
type Kept = (u64, ProfileOptions, Arc<DatasetProfile>);

/// Profiles kept: the whole dataset's, and those of sets of readable graphs (#203).
#[derive(Default)]
pub(crate) struct Cache {
    all: Mutex<Option<Kept>>,
    /// By the graphs' fingerprint, at most [`VIEWS_KEPT`].
    views: Mutex<HashMap<u64, Kept>>,
}

/// Profiles of sets of readable graphs kept.
const VIEWS_KEPT: usize = 16;

impl Engine {
    /// The dataset's profile. On the tripletrail store it is kept until the data changes (or
    /// other options are asked for).
    pub fn dataset_profile(&self, options: ProfileOptions) -> Result<Arc<DatasetProfile>, Error> {
        self.dataset_profile_in(options, None)
    }

    /// The profile of the `readable` graphs only (#203), or of the whole dataset without: what
    /// a caller who may read only those graphs is told the data holds. The class counts then
    /// come from reading the `rdf:type` quads of those graphs, and every link read is one of
    /// theirs. Kept as [`Engine::dataset_profile`] is, per set of graphs.
    pub fn dataset_profile_in(
        &self,
        options: ProfileOptions,
        readable: Option<&ReadableGraphs>,
    ) -> Result<Arc<DatasetProfile>, Error> {
        self.dataset_profile_of(options, readable, None)
    }

    /// The profile of what `rows` lets a caller see (#202) of the `readable` graphs (all
    /// without): read from one view of the store without the quads the rules hide, so no
    /// hidden class, property or count shows. Kept per graphs and rules.
    pub fn dataset_profile_of(
        &self,
        options: ProfileOptions,
        readable: Option<&ReadableGraphs>,
        rows: Option<&RowRules>,
    ) -> Result<Arc<DatasetProfile>, Error> {
        let view = rows.map(|rules| self.row_view(rules)).transpose()?;
        let generation = match (&view, self.store()) {
            (Some(view), _) => view.generation(),
            (None, Dataset::TripleTrail(store)) => Some(store.generation()),
            (None, Dataset::Oxigraph(_)) => None,
        };
        let fingerprint = match (readable, rows) {
            (None, None) => None,
            (r, rows) => {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                r.map(ReadableGraphs::fingerprint).hash(&mut hasher);
                rows.map(RowRules::fingerprint).hash(&mut hasher);
                Some(hasher.finish())
            }
        };
        let kept = |k: &Kept| (Some(k.0) == generation && k.1 == options).then(|| Arc::clone(&k.2));
        if generation.is_some() {
            let hit = match fingerprint {
                None => lock(&self.profile.all).as_ref().and_then(kept),
                Some(f) => lock(&self.profile.views).get(&f).and_then(kept),
            };
            if let Some(profile) = hit {
                return Ok(profile);
            }
        }
        let classes_in = match (readable, &view) {
            (None, None) => self.dataset_statistics()?.classes.clone(),
            (readable, view) => self.classes_in(readable, view.as_ref())?,
        };
        let mut profiler = Profiler {
            store: self.store(),
            view: view.as_ref(),
            engine: self,
            readable,
            types: HashMap::new(),
            counts: classes_in.iter().cloned().collect(),
        };
        let mut classes = Vec::new();
        for (class, instances) in classes_in.iter().take(options.max_classes) {
            classes.push(profiler.class(class, *instances, options)?);
        }
        let profile = Arc::new(DatasetProfile {
            generation,
            options,
            classes,
            classes_total: classes_in.len(),
        });
        if let Some(generation) = generation {
            let entry = (generation, options, Arc::clone(&profile));
            match fingerprint {
                None => *lock(&self.profile.all) = Some(entry),
                Some(f) => {
                    let mut views = lock(&self.profile.views);
                    if views.len() >= VIEWS_KEPT && !views.contains_key(&f) {
                        views.clear();
                    }
                    views.insert(f, entry);
                }
            }
        }
        Ok(profile)
    }

    /// Instances (distinct subjects) per class in the `readable` graphs (all without), most
    /// first; from `view` when given (#202).
    fn classes_in(
        &self,
        readable: Option<&ReadableGraphs>,
        view: Option<&Snapshot>,
    ) -> Result<Vec<(NamedNode, u64)>, Error> {
        let graphs: Vec<Option<GraphNameRef<'_>>> = match readable {
            None => vec![None],
            Some(readable) => readable
                .named
                .iter()
                .map(|n| Some(GraphNameRef::NamedNode(n.as_ref())))
                .chain(
                    readable
                        .default_graph
                        .then_some(Some(GraphNameRef::DefaultGraph)),
                )
                .collect(),
        };
        let mut instances: HashMap<NamedNode, HashSet<NamedOrBlankNode>> = HashMap::new();
        for graph in graphs {
            let quads: Box<dyn Iterator<Item = Result<Quad, Error>>> = match view {
                Some(view) => Box::new(
                    view.quads_for_pattern(None, Some(rdf::TYPE), None, graph)
                        .map(Ok),
                ),
                None => Box::new(
                    self.store()
                        .quads_for_pattern(None, Some(rdf::TYPE), None, graph)
                        .map(|q| q.map_err(Error::from)),
                ),
            };
            for (i, q) in quads.enumerate() {
                if i % 4096 == 0 && self.is_cancelled() {
                    return Err(cancelled());
                }
                let q = q?;
                if let Term::NamedNode(class) = q.object {
                    instances.entry(class).or_default().insert(q.subject);
                }
            }
        }
        let mut classes: Vec<(NamedNode, u64)> = instances
            .into_iter()
            .map(|(c, s)| (c, s.len() as u64))
            .collect();
        classes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(classes)
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The `(predicate, object)` pairs of a node.
type Links = Vec<(NamedNode, Term)>;

struct Profiler<'a> {
    store: &'a Dataset,
    /// The view without what row rules hide (#202), read instead of the store when given.
    view: Option<&'a Snapshot>,
    engine: &'a Engine,
    /// The graphs read, all without.
    readable: Option<&'a ReadableGraphs>,
    /// The classes of nodes looked up so far.
    types: HashMap<Term, Vec<NamedNode>>,
    /// Instances per class in the graphs read.
    counts: HashMap<NamedNode, u64>,
}

#[derive(Default)]
struct PropertyCounts {
    instances: u64,
    values: u64,
    min: Option<u64>,
    max: u64,
    iris: u64,
    blank_nodes: u64,
    literals: u64,
    datatypes: HashMap<NamedNode, u64>,
    languages: HashMap<String, u64>,
    classes: HashMap<NamedNode, u64>,
    lookups: usize,
    distinct: Option<HashSet<Term>>,
}

#[derive(Default)]
struct PathCounts {
    instances: u64,
    types: HashMap<String, u64>,
}

fn cancelled() -> Error {
    oxigraph::sparql::QueryEvaluationError::Cancelled.into()
}

fn ranked<K: Ord + Clone>(counts: HashMap<K, u64>) -> Vec<(K, u64)> {
    let mut v: Vec<(K, u64)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

fn is_geometry(datatype: NamedNodeRef<'_>) -> bool {
    datatype
        .as_str()
        .strip_prefix(GEO)
        .is_some_and(|local| GEOMETRY_DATATYPES.contains(&local))
}

/// The geometry type of a geometry literal: `POINT`, `LINESTRING`… (empty when unknown).
pub fn geometry_type(literal: &Literal) -> String {
    let value = literal.value().trim_start();
    if literal.datatype().as_str().ends_with("#geoJSONLiteral") {
        // The first "type" member: the geometry's (or the feature's) type.
        let after = value.split_once("\"type\"").map(|(_, rest)| rest);
        let name = after
            .and_then(|rest| rest.split_once('"'))
            .and_then(|(_, rest)| rest.split_once('"'))
            .map(|(name, _)| name.to_ascii_uppercase());
        return name.unwrap_or_default();
    }
    if literal.datatype().as_str().ends_with("#wktLiteral") {
        let value = match value.strip_prefix('<') {
            Some(rest) => rest.split_once('>').map_or("", |(_, w)| w).trim_start(),
            None => value,
        };
        return value
            .chars()
            .take_while(char::is_ascii_alphabetic)
            .collect::<String>()
            .to_ascii_uppercase();
    }
    String::new()
}

impl Profiler<'_> {
    /// The quads matching a pattern in the graphs read.
    fn quads(
        &self,
        subject: Option<NamedOrBlankNodeRef<'_>>,
        predicate: Option<NamedNodeRef<'_>>,
        object: Option<TermRef<'_>>,
    ) -> impl Iterator<Item = Result<Quad, tripletrail_store::DatasetError>> + use<'_> {
        let readable = self.readable;
        let quads: Box<dyn Iterator<Item = Result<Quad, tripletrail_store::DatasetError>>> =
            match self.view {
                Some(view) => Box::new(
                    view.quads_for_pattern(subject, predicate, object, None)
                        .map(Ok),
                ),
                None => Box::new(
                    self.store
                        .quads_for_pattern(subject, predicate, object, None),
                ),
            };
        quads.filter(move |q| match (q, readable) {
            (Ok(q), Some(r)) => r.contains(q.graph_name.as_ref()),
            _ => true,
        })
    }

    fn check(&self) -> Result<(), Error> {
        if self.engine.is_cancelled() {
            Err(cancelled())
        } else {
            Ok(())
        }
    }

    /// The `(predicate, object)` pairs of a node, each once (a triple in several graphs).
    fn outgoing(&self, node: &NamedOrBlankNode) -> Result<Links, Error> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for q in self.quads(Some(node.as_ref()), None, None) {
            let q = q?;
            if seen.insert((q.predicate.clone(), q.object.clone())) {
                out.push((q.predicate, q.object));
                if out.len() >= MAX_LINKS_PER_NODE {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// A label of `iri` in the data: an untagged or English one first.
    fn label_of(&self, iri: &NamedNode) -> Result<Option<String>, Error> {
        let mut best: Option<(u8, String)> = None;
        for p in LABEL_PROPERTIES.iter().take(2) {
            let p = NamedNode::new_unchecked(*p);
            for q in self
                .quads(Some(iri.as_ref().into()), Some(p.as_ref()), None)
                .take(16)
            {
                if let Term::Literal(l) = q?.object {
                    let rank = match l.language() {
                        None => 0,
                        Some(lang) if lang.starts_with("en") => 1,
                        Some(_) => 2,
                    };
                    if best.as_ref().is_none_or(|(r, _)| rank < *r) {
                        best = Some((rank, l.value().to_owned()));
                    }
                }
            }
            if best.is_some() {
                break;
            }
        }
        Ok(best.map(|(_, l)| l))
    }

    fn types_of(&mut self, node: &Term) -> Result<Vec<NamedNode>, Error> {
        if let Some(t) = self.types.get(node) {
            return Ok(t.clone());
        }
        let subject = match node {
            Term::NamedNode(n) => NamedOrBlankNode::from(n.clone()),
            Term::BlankNode(b) => NamedOrBlankNode::from(b.clone()),
            _ => return Ok(Vec::new()),
        };
        let mut types = Vec::new();
        for q in self
            .quads(Some(subject.as_ref()), Some(rdf::TYPE), None)
            .take(8)
        {
            if let Term::NamedNode(c) = q?.object
                && !types.contains(&c)
            {
                types.push(c);
            }
        }
        if self.types.len() < 100_000 {
            self.types.insert(node.clone(), types.clone());
        }
        Ok(types)
    }

    fn class(
        &mut self,
        class: &NamedNode,
        instances: u64,
        options: ProfileOptions,
    ) -> Result<ClassProfile, Error> {
        self.check()?;
        let mut sample: Vec<NamedOrBlankNode> = Vec::new();
        let mut seen = HashSet::new();
        for q in self.quads(None, Some(rdf::TYPE), Some(TermRef::from(class.as_ref()))) {
            let s = q?.subject;
            if seen.insert(s.clone()) {
                sample.push(s);
                if sample.len() >= options.sample {
                    break;
                }
            }
        }
        let mut props: BTreeMap<NamedNode, PropertyCounts> = BTreeMap::new();
        let mut paths: BTreeMap<(Vec<NamedNode>, NamedNode), PathCounts> = BTreeMap::new();
        for (i, s) in sample.iter().enumerate() {
            if i % 32 == 0 {
                self.check()?;
            }
            let pairs = self.outgoing(s)?;
            let mut per_instance: HashMap<&NamedNode, u64> = HashMap::new();
            for (p, o) in &pairs {
                *per_instance.entry(p).or_default() += 1;
                let c = props.entry(p.clone()).or_insert_with(|| PropertyCounts {
                    distinct: Some(HashSet::new()),
                    ..PropertyCounts::default()
                });
                c.values += 1;
                match o {
                    Term::NamedNode(_) => c.iris += 1,
                    Term::BlankNode(_) => c.blank_nodes += 1,
                    Term::Literal(l) => {
                        c.literals += 1;
                        *c.datatypes.entry(l.datatype().into_owned()).or_default() += 1;
                        if let Some(lang) = l.language() {
                            *c.languages.entry(lang.to_owned()).or_default() += 1;
                        }
                    }
                    #[allow(unreachable_patterns)]
                    _ => {}
                }
                if let Some(d) = &mut c.distinct {
                    let long = matches!(o, Term::Literal(l) if l.value().len() > MAX_LISTED_LENGTH);
                    d.insert(o.clone());
                    if d.len() > MAX_LISTED_VALUES || long {
                        c.distinct = None;
                    }
                }
                let lookup = matches!(o, Term::NamedNode(_) | Term::BlankNode(_))
                    && *p != rdf::TYPE
                    && c.lookups < CLASS_LOOKUPS;
                if lookup {
                    c.lookups += 1;
                    let types = self.types_of(o)?;
                    let c = props.get_mut(p).expect("inserted above");
                    for t in types {
                        *c.classes.entry(t).or_default() += 1;
                    }
                }
            }
            for (p, n) in per_instance {
                let c = props.get_mut(p).expect("counted above");
                c.instances += 1;
                c.max = c.max.max(n);
                c.min = Some(c.min.map_or(n, |m| m.min(n)));
            }
            if i < options.path_sample {
                for (path, datatype, kind) in self.geometry_paths(s, pairs)? {
                    let c = paths.entry((path, datatype)).or_default();
                    c.instances += 1;
                    if !kind.is_empty() {
                        *c.types.entry(kind).or_default() += 1;
                    }
                }
            }
        }
        let sampled = sample.len() as u64;
        let mut properties: Vec<PropertyProfile> = Vec::with_capacity(props.len());
        for (property, c) in props {
            let label = self.label_of(&property)?;
            // A code list: few values, each used twice on average.
            let mut distinct_values = c
                .distinct
                .filter(|d| d.len() as u64 * 2 <= c.values)
                .map(|d| d.into_iter().collect::<Vec<_>>());
            if let Some(values) = &mut distinct_values {
                values.sort_by_key(ToString::to_string);
            }
            properties.push(PropertyProfile {
                label,
                instances: c.instances,
                values: c.values,
                // Some sampled instances without the property: none at least once.
                min_count: if c.instances < sampled {
                    0
                } else {
                    c.min.unwrap_or(0)
                },
                max_count: c.max,
                iris: c.iris,
                blank_nodes: c.blank_nodes,
                literals: c.literals,
                datatypes: ranked(c.datatypes),
                languages: ranked(c.languages),
                classes: ranked(c.classes),
                distinct_values,
                property,
            });
        }
        properties.sort_by(|a, b| {
            b.instances
                .cmp(&a.instances)
                .then_with(|| a.property.cmp(&b.property))
        });
        let label_properties = LABEL_PROPERTIES
            .iter()
            .filter(|l| properties.iter().any(|p| p.property.as_str() == **l))
            .map(|l| NamedNode::new_unchecked(*l))
            .collect();
        let mut geometries: Vec<GeometryPath> = paths
            .into_iter()
            .map(|((path, datatype), c)| GeometryPath {
                path,
                datatype,
                instances: c.instances,
                types: ranked(c.types),
            })
            .collect();
        geometries.sort_by(|a, b| {
            b.instances
                .cmp(&a.instances)
                .then_with(|| a.path.len().cmp(&b.path.len()))
                .then_with(|| a.path.cmp(&b.path))
        });
        let superclasses = self
            .quads(Some(class.as_ref().into()), Some(rdfs::SUB_CLASS_OF), None)
            .filter_map(|q| match q {
                Ok(q) => match q.object {
                    Term::NamedNode(n) => Some(Ok(n)),
                    _ => None,
                },
                Err(e) => Some(Err(e)),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let umbrella = self.umbrella(class, instances, &properties)?;
        Ok(ClassProfile {
            umbrella,
            class: class.clone(),
            label: self.label_of(class)?,
            superclasses,
            instances,
            sampled,
            properties,
            label_properties,
            geometries,
        })
    }

    /// Whether `class` is an umbrella ([`ClassProfile::umbrella`]). The sample tells first, as
    /// it is cheap: every sampled instance has two types or more, and the other types seen are
    /// narrower. Only then are all instances read, as the sample is the first ones in store
    /// order and can miss the instances that have only this type.
    fn umbrella(
        &self,
        class: &NamedNode,
        instances: u64,
        properties: &[PropertyProfile],
    ) -> Result<bool, Error> {
        let narrower = |c: &NamedNode| self.counts.get(c).is_some_and(|&k| k < instances);
        let Some(types) = properties.iter().find(|p| p.property == rdf::TYPE) else {
            return Ok(false);
        };
        let Some(values) = types
            .distinct_values
            .as_ref()
            .filter(|_| types.min_count >= 2)
        else {
            return Ok(false);
        };
        let sampled_ok = values.iter().all(|v| match v {
            Term::NamedNode(n) => n == class || narrower(n),
            _ => false,
        });
        if !sampled_ok {
            return Ok(false);
        }
        let mut read = 0u64;
        for q in self.quads(None, Some(rdf::TYPE), Some(TermRef::from(class.as_ref()))) {
            let subject = q?.subject;
            read += 1;
            if read.is_multiple_of(4096) {
                self.check()?;
            }
            let mut other = false;
            for t in self.quads(Some(subject.as_ref()), Some(rdf::TYPE), None) {
                match t?.object {
                    Term::NamedNode(n) if n == *class => {}
                    Term::NamedNode(n) if narrower(&n) => other = true,
                    _ => return Ok(false),
                }
            }
            if !other {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The paths from `start` (whose outgoing pairs are `first`) to geometry literals, each
    /// path once, with the datatype and type of the first geometry found by it.
    fn geometry_paths(
        &self,
        start: &NamedOrBlankNode,
        first: Links,
    ) -> Result<Vec<(Vec<NamedNode>, NamedNode, String)>, Error> {
        let mut found: BTreeMap<Vec<NamedNode>, (NamedNode, String)> = BTreeMap::new();
        let mut visited: HashSet<NamedOrBlankNode> = HashSet::from([start.clone()]);
        let mut frontier: Vec<(Vec<NamedNode>, Links)> = vec![(Vec::new(), first)];
        while let Some((path, pairs)) = frontier.pop() {
            let mut followed: HashMap<&NamedNode, usize> = HashMap::new();
            let mut next = Vec::new();
            for (p, o) in &pairs {
                let mut here = path.clone();
                here.push(p.clone());
                match o {
                    Term::Literal(l) if is_geometry(l.datatype()) => {
                        found
                            .entry(here)
                            .or_insert_with(|| (l.datatype().into_owned(), geometry_type(l)));
                    }
                    Term::NamedNode(_) | Term::BlankNode(_)
                        if here.len() < MAX_HOPS && *p != rdf::TYPE =>
                    {
                        let n = followed.entry(p).or_default();
                        if *n >= PATH_FANOUT || visited.len() >= PATH_NODES {
                            continue;
                        }
                        *n += 1;
                        let node = match o {
                            Term::NamedNode(n) => NamedOrBlankNode::from(n.clone()),
                            Term::BlankNode(b) => NamedOrBlankNode::from(b.clone()),
                            _ => unreachable!(),
                        };
                        if visited.insert(node.clone()) {
                            next.push((here, node));
                        }
                    }
                    _ => {}
                }
            }
            for (here, node) in next {
                let pairs = self.outgoing(&node)?;
                frontier.push((here, pairs));
            }
        }
        Ok(found.into_iter().map(|(p, (d, t))| (p, d, t)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_types() {
        let wkt = |s: &str| {
            Literal::new_typed_literal(s, NamedNode::new_unchecked(format!("{GEO}wktLiteral")))
        };
        assert_eq!(geometry_type(&wkt("POINT(1 2)")), "POINT");
        assert_eq!(
            geometry_type(&wkt(" LineString Z (1 2 3, 4 5 6)")),
            "LINESTRING"
        );
        assert_eq!(
            geometry_type(&wkt(
                "<http://www.opengis.net/def/crs/EPSG/0/25833> POLYGON((0 0,1 0,1 1,0 0))"
            )),
            "POLYGON"
        );
        let json = Literal::new_typed_literal(
            r#"{"type": "MultiPoint", "coordinates": [[1, 2]]}"#,
            NamedNode::new_unchecked(format!("{GEO}geoJSONLiteral")),
        );
        assert_eq!(geometry_type(&json), "MULTIPOINT");
    }
}
