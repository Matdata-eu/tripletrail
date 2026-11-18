//! Dataset statistics for VoID descriptions (issue #242): totals, per graph, per predicate and
//! per class.
//!
//! On the tripletrail store they come from the counts kept with the quad indexes
//! ([`tripletrail_store::Snapshot::dataset_counts`]), plus one read of the `rdf:type` quads for
//! the classes, and are kept until the data changes. On Oxigraph's store every quad is read.

use crate::{Engine, Error};
use oxigraph::model::vocab::rdf;
use oxigraph::model::{GraphName, NamedNode, NamedOrBlankNode, Term};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tripletrail_store::{Dataset, Snapshot, TermId};

/// What [`Engine::dataset_statistics`] returns. Quads count once per graph they are in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DatasetStatistics {
    /// Quads in all graphs.
    pub triples: u64,
    pub distinct_subjects: u64,
    /// Distinct predicates (VoID's `void:properties`).
    pub distinct_predicates: u64,
    pub distinct_objects: u64,
    /// Distinct IRI subjects (VoID's `void:entities`).
    pub entities: u64,
    /// Instances (distinct subjects) per class, most instances first.
    pub classes: Vec<(NamedNode, u64)>,
    /// Per predicate, most triples first; `None` when the store keeps no per-predicate
    /// counts (more than 4,096 predicates).
    pub properties: Option<Vec<PropertyStatistics>>,
    /// Per graph, the default graph first; `None` with more than 4,096 graphs.
    pub graphs: Option<Vec<GraphStatistics>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyStatistics {
    pub property: NamedNode,
    pub triples: u64,
    pub distinct_subjects: u64,
    pub distinct_objects: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStatistics {
    /// `None` for the default graph.
    pub name: Option<NamedOrBlankNode>,
    pub triples: u64,
    pub distinct_subjects: u64,
    pub distinct_predicates: u64,
    pub distinct_objects: u64,
}

/// The statistics of the last generation they were computed for.
pub(crate) type Cache = Mutex<Option<(u64, Arc<DatasetStatistics>)>>;

impl Engine {
    /// Statistics of the whole dataset (every graph), for a VoID description. On the tripletrail
    /// store they are computed once per version of the data, from the counts kept with the
    /// indexes; while writes not yet folded into the indexes are under 1 % of them, the
    /// numbers leave those writes out.
    pub fn dataset_statistics(&self) -> Result<Arc<DatasetStatistics>, Error> {
        let Dataset::TripleTrail(store) = self.store() else {
            return Ok(Arc::new(read_all(self.store(), &|| self.is_cancelled())?));
        };
        let generation = store.generation();
        if let Some((g, s)) = &*self.statistics.lock().unwrap_or_else(|e| e.into_inner())
            && *g == generation
        {
            return Ok(Arc::clone(s));
        }
        let snapshot = store.snapshot();
        let stats = from_counts(&snapshot, &|| self.is_cancelled()).ok_or_else(cancelled_error)?;
        let stats = Arc::new(stats);
        *self.statistics.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((generation, Arc::clone(&stats)));
        Ok(stats)
    }
}

fn from_counts(snapshot: &Snapshot, cancelled: &dyn Fn() -> bool) -> Option<DatasetStatistics> {
    let counts = snapshot.dataset_counts(cancelled)?;
    let named = |id: TermId| match snapshot.term(id).ok()? {
        Term::NamedNode(n) => Some(n),
        _ => None,
    };
    let mut classes: Vec<(NamedNode, u64)> = counts
        .classes
        .iter()
        .filter_map(|c| Some((named(c.class)?, c.instances)))
        .collect();
    classes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let properties = counts.predicates.map(|ps| {
        let mut ps: Vec<PropertyStatistics> = ps
            .iter()
            .filter_map(|p| {
                Some(PropertyStatistics {
                    property: named(p.predicate)?,
                    triples: p.triples,
                    distinct_subjects: p.distinct_subjects,
                    distinct_objects: p.distinct_objects,
                })
            })
            .collect();
        sort_properties(&mut ps);
        ps
    });
    let graphs = match counts.graphs {
        None => None,
        Some(gs) => {
            let mut out = Vec::with_capacity(gs.len());
            for g in gs {
                let name = if g.graph == TermId(0) {
                    None
                } else {
                    match snapshot.term(g.graph).ok()? {
                        Term::NamedNode(n) => Some(n.into()),
                        Term::BlankNode(b) => Some(b.into()),
                        _ => continue,
                    }
                };
                out.push(GraphStatistics {
                    name,
                    triples: g.triples,
                    distinct_subjects: g.distinct_subjects,
                    distinct_predicates: g.distinct_predicates,
                    distinct_objects: g.distinct_objects,
                });
            }
            Some(out)
        }
    };
    Some(DatasetStatistics {
        triples: counts.triples,
        distinct_subjects: counts.distinct_subjects,
        distinct_predicates: counts.distinct_predicates,
        distinct_objects: counts.distinct_objects,
        entities: counts.entities,
        classes,
        properties,
        graphs,
    })
}

fn cancelled_error() -> Error {
    oxigraph::sparql::QueryEvaluationError::Cancelled.into()
}

fn sort_properties(ps: &mut [PropertyStatistics]) {
    ps.sort_by(|a, b| {
        b.triples
            .cmp(&a.triples)
            .then_with(|| a.property.cmp(&b.property))
    });
}

/// The statistics by reading every quad (Oxigraph's store).
fn read_all(store: &Dataset, cancelled: &dyn Fn() -> bool) -> Result<DatasetStatistics, Error> {
    #[derive(Default)]
    struct Sets {
        triples: u64,
        subjects: HashSet<NamedOrBlankNode>,
        predicates: HashSet<NamedNode>,
        objects: HashSet<Term>,
    }
    let mut all = Sets::default();
    let mut per_graph: HashMap<GraphName, Sets> = HashMap::new();
    let mut per_predicate: HashMap<NamedNode, (u64, HashSet<NamedOrBlankNode>, HashSet<Term>)> =
        HashMap::new();
    let mut per_class: HashMap<NamedNode, HashSet<NamedOrBlankNode>> = HashMap::new();
    for (i, quad) in store.iter().enumerate() {
        if i % 65_536 == 0 && cancelled() {
            return Err(cancelled_error());
        }
        let q = quad?;
        for sets in [&mut all, per_graph.entry(q.graph_name.clone()).or_default()] {
            sets.triples += 1;
            sets.subjects.insert(q.subject.clone());
            sets.predicates.insert(q.predicate.clone());
            sets.objects.insert(q.object.clone());
        }
        let p = per_predicate.entry(q.predicate.clone()).or_default();
        p.0 += 1;
        p.1.insert(q.subject.clone());
        p.2.insert(q.object.clone());
        if q.predicate == rdf::TYPE
            && let Term::NamedNode(class) = &q.object
        {
            per_class
                .entry(class.clone())
                .or_default()
                .insert(q.subject.clone());
        }
    }
    let mut classes: Vec<(NamedNode, u64)> = per_class
        .into_iter()
        .map(|(c, s)| (c, s.len() as u64))
        .collect();
    classes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut properties: Vec<PropertyStatistics> = per_predicate
        .into_iter()
        .map(|(property, (triples, s, o))| PropertyStatistics {
            property,
            triples,
            distinct_subjects: s.len() as u64,
            distinct_objects: o.len() as u64,
        })
        .collect();
    sort_properties(&mut properties);
    let mut graphs: Vec<GraphStatistics> = per_graph
        .into_iter()
        .map(|(g, s)| GraphStatistics {
            name: match g {
                GraphName::NamedNode(n) => Some(n.into()),
                GraphName::BlankNode(b) => Some(b.into()),
                GraphName::DefaultGraph => None,
            },
            triples: s.triples,
            distinct_subjects: s.subjects.len() as u64,
            distinct_predicates: s.predicates.len() as u64,
            distinct_objects: s.objects.len() as u64,
        })
        .collect();
    graphs.sort_by(|a, b| {
        a.name.is_some().cmp(&b.name.is_some()).then_with(|| {
            a.name
                .as_ref()
                .map(ToString::to_string)
                .cmp(&b.name.as_ref().map(ToString::to_string))
        })
    });
    Ok(DatasetStatistics {
        triples: all.triples,
        distinct_subjects: all.subjects.len() as u64,
        distinct_predicates: all.predicates.len() as u64,
        distinct_objects: all.objects.len() as u64,
        entities: all
            .subjects
            .iter()
            .filter(|s| matches!(s, NamedOrBlankNode::NamedNode(_)))
            .count() as u64,
        classes,
        properties: Some(properties),
        graphs: Some(graphs),
    })
}
