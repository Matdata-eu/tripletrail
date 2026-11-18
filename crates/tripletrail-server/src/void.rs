//! [VoID](https://www.w3.org/TR/void/) descriptions of the served datasets: at
//! `/.well-known/void` (all of them, where the VoID spec and MatGUI look) and `/{ds}/void`.
//!
//! The numbers come from [`Engine::dataset_statistics`]: the counts kept with the indexes,
//! computed once per version of the data. The class and property partitions let clients such
//! as MatGUI autocomplete classes and properties without running a `GROUP BY` over the store
//! (#242, #243).

use crate::HttpError;
use std::collections::BTreeSet;
use tripletrail::oxigraph::model::vocab::{rdf, rdfs, xsd};
use tripletrail::oxigraph::model::{
    BlankNode, GraphNameRef, Literal, NamedNode, NamedOrBlankNode, NamedOrBlankNodeRef, Term,
    Triple,
};
use tripletrail::{DatasetStatistics, Engine, ReadableGraphs};

pub(crate) const VOID: &str = "http://rdfs.org/ns/void#";
const DCTERMS: &str = "http://purl.org/dc/terms/";
const SD: &str = "http://www.w3.org/ns/sparql-service-description#";

fn void(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{VOID}{name}"))
}

fn count(n: u64) -> Term {
    Literal::new_typed_literal(n.to_string(), xsd::INTEGER).into()
}

fn t(s: &NamedOrBlankNode, p: impl Into<NamedNode>, o: impl Into<Term>) -> Triple {
    Triple::new(s.clone(), p, o)
}

/// The namespace of an IRI: up to its last `#` or `/`.
fn namespace(iri: &str) -> Option<&str> {
    let end = iri.rfind(['#', '/'])?;
    (end + 1 < iri.len()).then(|| &iri[..=end])
}

/// The IRI of dataset `name`'s VoID description node.
pub(crate) fn dataset_iri(origin: &str, name: &str) -> String {
    format!("{origin}/{name}/void#dataset")
}

/// The VoID triples of one dataset, served at `{origin}/{name}`. For a caller who may read
/// only `readable` graphs (#203), only the subsets of those graphs: the counts of the whole
/// dataset, its classes, properties and vocabularies would tell about the others.
pub(crate) fn describe(
    origin: &str,
    name: &str,
    engine: &Engine,
    readable: Option<&ReadableGraphs>,
) -> Result<Vec<Triple>, HttpError> {
    let stats = engine.dataset_statistics()?;
    Ok(triples(origin, name, &stats, readable))
}

fn triples(
    origin: &str,
    name: &str,
    stats: &DatasetStatistics,
    readable: Option<&ReadableGraphs>,
) -> Vec<Triple> {
    let mut out = Vec::new();
    let mut parts = Vec::new();
    let ds: NamedOrBlankNode = NamedNode::new_unchecked(dataset_iri(origin, name)).into();
    out.push(t(&ds, rdf::TYPE, void("Dataset")));
    out.push(t(
        &ds,
        NamedNode::new_unchecked(format!("{DCTERMS}title")),
        Literal::new_simple_literal(name),
    ));
    out.push(t(
        &ds,
        void("sparqlEndpoint"),
        NamedNode::new_unchecked(format!("{origin}/{name}/sparql")),
    ));
    out.push(t(
        &ds,
        void("dataDump"),
        NamedNode::new_unchecked(format!("{origin}/{name}/data")),
    ));
    if let Some(readable) = readable {
        subsets(&ds, stats, Some(readable), &mut out, &mut parts);
        out.extend(parts);
        return out;
    }
    out.push(t(&ds, void("triples"), count(stats.triples)));
    out.push(t(&ds, void("entities"), count(stats.entities)));
    out.push(t(
        &ds,
        void("distinctSubjects"),
        count(stats.distinct_subjects),
    ));
    out.push(t(&ds, void("properties"), count(stats.distinct_predicates)));
    out.push(t(
        &ds,
        void("distinctObjects"),
        count(stats.distinct_objects),
    ));
    out.push(t(&ds, void("classes"), count(stats.classes.len() as u64)));

    // Namespaces of the predicates and classes.
    let mut vocabularies = BTreeSet::new();
    for (class, _) in &stats.classes {
        vocabularies.extend(namespace(class.as_str()));
    }
    for p in stats.properties.iter().flatten() {
        vocabularies.extend(namespace(p.property.as_str()));
    }
    for v in vocabularies {
        out.push(t(&ds, void("vocabulary"), NamedNode::new_unchecked(v)));
    }

    for (class, instances) in &stats.classes {
        let part: NamedOrBlankNode = BlankNode::default().into();
        out.push(t(&ds, void("classPartition"), part.clone()));
        parts.push(t(&part, void("class"), class.clone()));
        parts.push(t(&part, void("entities"), count(*instances)));
    }
    for p in stats.properties.iter().flatten() {
        let part: NamedOrBlankNode = BlankNode::default().into();
        out.push(t(&ds, void("propertyPartition"), part.clone()));
        parts.push(t(&part, void("property"), p.property.clone()));
        parts.push(t(&part, void("triples"), count(p.triples)));
        parts.push(t(
            &part,
            void("distinctSubjects"),
            count(p.distinct_subjects),
        ));
        parts.push(t(&part, void("distinctObjects"), count(p.distinct_objects)));
    }
    subsets(&ds, stats, None, &mut out, &mut parts);
    out.extend(parts);
    out
}

/// One subset per graph (the `readable` ones only, when given); a named graph's carries its
/// name.
fn subsets(
    ds: &NamedOrBlankNode,
    stats: &DatasetStatistics,
    readable: Option<&ReadableGraphs>,
    out: &mut Vec<Triple>,
    parts: &mut Vec<Triple>,
) {
    for g in stats.graphs.iter().flatten() {
        let graph = match &g.name {
            None => GraphNameRef::DefaultGraph,
            Some(NamedOrBlankNode::NamedNode(n)) => GraphNameRef::NamedNode(n.as_ref()),
            Some(NamedOrBlankNode::BlankNode(b)) => GraphNameRef::BlankNode(b.as_ref()),
        };
        if readable.is_some_and(|r| !r.contains(graph)) {
            continue;
        }
        let part: NamedOrBlankNode = BlankNode::default().into();
        out.push(t(ds, void("subset"), part.clone()));
        parts.push(t(&part, rdf::TYPE, void("Dataset")));
        match g.name.as_ref().map(NamedOrBlankNode::as_ref) {
            Some(NamedOrBlankNodeRef::NamedNode(n)) => parts.push(t(
                &part,
                NamedNode::new_unchecked(format!("{SD}name")),
                n.into_owned(),
            )),
            Some(NamedOrBlankNodeRef::BlankNode(_)) => {}
            None => parts.push(t(
                &part,
                rdfs::LABEL,
                Literal::new_simple_literal("default graph"),
            )),
        }
        parts.push(t(&part, void("triples"), count(g.triples)));
        parts.push(t(
            &part,
            void("distinctSubjects"),
            count(g.distinct_subjects),
        ));
        parts.push(t(&part, void("properties"), count(g.distinct_predicates)));
        parts.push(t(&part, void("distinctObjects"), count(g.distinct_objects)));
    }
}
