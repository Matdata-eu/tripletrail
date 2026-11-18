//! The [SPARQL 1.1 Service Description](https://www.w3.org/TR/sparql11-service-description/)
//! of a dataset endpoint, served on a `GET` of the endpoint without a `query` or `update`
//! parameter.
//!
//! Nothing in it is written by hand: it is built at request time from what the server
//! negotiates ([`RESULT_FORMATS`], [`RDF_FORMATS`]), from the functions, aggregates and
//! SERVICE handlers the engine registers ([`tripletrail::extension_functions`], ...), and from
//! the dataset (its named graphs, read-only mode, union default graph). A new function or
//! format then shows up here without anyone having to add it.

use crate::HttpError;
use tripletrail::Engine;
use tripletrail::oxigraph::io::{JsonLdProfileSet, RdfFormat, RdfSerializer};
use tripletrail::oxigraph::model::vocab::{rdf, rdfs, xsd};
use tripletrail::oxigraph::model::{
    BlankNode, GraphNameRef, Literal, NamedNode, NamedOrBlankNode, NamedOrBlankNodeRef, Term,
    Triple,
};
use tripletrail::oxigraph::sparql::results::QueryResultsFormat;

const SD: &str = "http://www.w3.org/ns/sparql-service-description#";
const ENTAILMENT_SIMPLE: &str = "http://www.w3.org/ns/entailment/Simple";

/// The SELECT / ASK result formats the endpoint negotiates.
pub(crate) const RESULT_FORMATS: [QueryResultsFormat; 4] = [
    QueryResultsFormat::Json,
    QueryResultsFormat::Xml,
    QueryResultsFormat::Csv,
    QueryResultsFormat::Tsv,
];

/// The RDF formats the endpoint reads (Graph Store uploads, `LOAD`) and writes (CONSTRUCT
/// and DESCRIBE results, Graph Store reads).
pub(crate) const RDF_FORMATS: [RdfFormat; 7] = [
    RdfFormat::Turtle,
    RdfFormat::NTriples,
    RdfFormat::NQuads,
    RdfFormat::TriG,
    RdfFormat::RdfXml,
    RdfFormat::N3,
    RdfFormat::JsonLd {
        profile: JsonLdProfileSet::empty(),
    },
];

fn sd(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{SD}{name}"))
}

/// What the description says about one dataset endpoint.
pub(crate) struct Endpoint<'a> {
    /// The endpoint's absolute URL.
    pub(crate) url: String,
    pub(crate) engine: &'a Engine,
    /// Updates are refused: no `sd:SPARQL11Update`.
    pub(crate) read_only: bool,
    /// The IRI of the dataset's VoID description (see [`crate::void`]).
    pub(crate) void: String,
    /// The graphs the caller may read, when not all (#201): only these are listed, and the
    /// default graph (the union of these) is given without a size.
    pub(crate) readable: Option<&'a tripletrail::ReadableGraphs>,
}

/// The description's triples: the service, then its dataset, then the functions (in that
/// order, so Turtle groups each subject's triples).
pub(crate) fn describe(endpoint: &Endpoint<'_>) -> Result<Vec<Triple>, HttpError> {
    fn t(s: &NamedOrBlankNode, p: impl Into<NamedNode>, o: impl Into<Term>) -> Triple {
        Triple::new(s.clone(), p, o)
    }
    let mut out = Vec::new();
    let mut dataset_part = Vec::new();
    let mut functions = Vec::new();
    let service: NamedOrBlankNode = BlankNode::default().into();
    out.push(t(&service, rdf::TYPE, sd("Service")));
    let url = NamedNode::new(&endpoint.url)
        .map_err(|e| HttpError(500, format!("Endpoint URL {}: {e}", endpoint.url)))?;
    out.push(t(&service, sd("endpoint"), url));
    out.push(t(&service, sd("supportedLanguage"), sd("SPARQL11Query")));
    if !endpoint.read_only {
        out.push(t(&service, sd("supportedLanguage"), sd("SPARQL11Update")));
    }
    if endpoint.engine.union_default_graph() {
        out.push(t(&service, sd("feature"), sd("UnionDefaultGraph")));
    }
    // Oxigraph's store keeps empty named graphs; the tripletrail store only has graphs with quads.
    if endpoint.engine.store().kind() == "oxigraph" {
        out.push(t(&service, sd("feature"), sd("EmptyGraphs")));
    }
    out.push(t(
        &service,
        sd("defaultEntailmentRegime"),
        NamedNode::new_unchecked(ENTAILMENT_SIMPLE),
    ));
    for f in RESULT_FORMATS {
        out.push(t(
            &service,
            sd("resultFormat"),
            NamedNode::new_unchecked(f.iri()),
        ));
    }
    for f in RDF_FORMATS {
        out.push(t(
            &service,
            sd("resultFormat"),
            NamedNode::new_unchecked(f.iri()),
        ));
    }
    for f in RDF_FORMATS {
        out.push(t(
            &service,
            sd("inputFormat"),
            NamedNode::new_unchecked(f.iri()),
        ));
    }
    for (kind, class, names) in [
        (
            "extensionFunction",
            "Function",
            tripletrail::extension_functions(),
        ),
        (
            "extensionAggregate",
            "Aggregate",
            tripletrail::extension_aggregates(),
        ),
    ] {
        for name in names {
            let node: NamedOrBlankNode = name.clone().into();
            out.push(t(&service, sd(kind), name.clone()));
            functions.push(t(&node, rdf::TYPE, sd(class)));
            functions.push(t(&node, rdfs::LABEL, label(&name)));
        }
    }
    // SERVICE handlers have no Service Description term: describe each as an `sd:Service`
    // of its own, linked with `rdfs:seeAlso`, so clients can at least find the IRIs. Each
    // answers the graph pattern a SERVICE call sends it, so it supports SPARQL 1.1 Query as
    // the schema asks every `sd:Service` to say (the W3C suite's `conforms-to-schema`).
    for name in tripletrail::extension_services() {
        let node: NamedOrBlankNode = name.clone().into();
        out.push(t(&service, rdfs::SEE_ALSO, name.clone()));
        functions.push(t(&node, rdf::TYPE, sd("Service")));
        functions.push(t(&node, sd("endpoint"), name.clone()));
        functions.push(t(&node, sd("supportedLanguage"), sd("SPARQL11Query")));
        functions.push(t(&node, rdfs::LABEL, label(&name)));
    }

    // The dataset with its graphs and their sizes (as Blazegraph does), linked to its VoID.
    let stats = endpoint.engine.dataset_statistics()?;
    let triples = |name: Option<&NamedOrBlankNode>| -> Option<u64> {
        if name.is_none() && endpoint.readable.is_some() {
            return None;
        }
        if name.is_none() && endpoint.engine.union_default_graph() {
            return Some(stats.triples);
        }
        Some(
            stats
                .graphs
                .as_ref()?
                .iter()
                .find(|g| g.name.as_ref() == name)
                .map_or(0, |g| g.triples),
        )
    };
    let void_triples = NamedNode::new_unchecked(format!("{}triples", crate::void::VOID));
    let count = |n: u64| Literal::new_typed_literal(n.to_string(), xsd::INTEGER);
    let dataset: NamedOrBlankNode = BlankNode::default().into();
    out.push(t(&service, sd("defaultDataset"), dataset.clone()));
    dataset_part.push(t(&dataset, rdf::TYPE, sd("Dataset")));
    dataset_part.push(t(
        &dataset,
        rdfs::SEE_ALSO,
        NamedNode::new_unchecked(&endpoint.void),
    ));
    let default_graph: NamedOrBlankNode = BlankNode::default().into();
    dataset_part.push(t(&dataset, sd("defaultGraph"), default_graph.clone()));
    let mut graphs = Vec::new();
    graphs.push(t(&default_graph, rdf::TYPE, sd("Graph")));
    if let Some(n) = triples(None) {
        graphs.push(t(&default_graph, void_triples.clone(), count(n)));
    }
    for graph in endpoint.engine.store().named_graphs()? {
        // `sd:name` takes an IRI: graphs named by a blank node are left out.
        let NamedOrBlankNodeRef::NamedNode(name) = graph.as_ref() else {
            continue;
        };
        if endpoint
            .readable
            .is_some_and(|r| !r.contains(GraphNameRef::NamedNode(name)))
        {
            continue;
        }
        let named: NamedOrBlankNode = BlankNode::default().into();
        dataset_part.push(t(&dataset, sd("namedGraph"), named.clone()));
        graphs.push(t(&named, rdf::TYPE, sd("NamedGraph")));
        graphs.push(t(&named, sd("name"), name.into_owned()));
        if let Some(n) = triples(Some(&graph)) {
            let g: NamedOrBlankNode = BlankNode::default().into();
            graphs.push(t(&named, sd("graph"), g.clone()));
            graphs.push(t(&g, rdf::TYPE, sd("Graph")));
            graphs.push(t(&g, void_triples.clone(), count(n)));
        }
    }
    out.extend(dataset_part);
    out.extend(graphs);
    out.extend(functions);
    Ok(out)
}

/// The IRI's local name, as its `rdfs:label`.
fn label(iri: &NamedNode) -> Term {
    let name = iri
        .as_str()
        .rsplit(['#', '/'])
        .next()
        .unwrap_or(iri.as_str());
    Literal::new_simple_literal(name).into()
}

/// The description serialized as `format`.
pub(crate) fn serialize(triples: &[Triple], format: RdfFormat) -> Result<Vec<u8>, HttpError> {
    let mut serializer = RdfSerializer::from_format(format);
    for (prefix, iri) in [
        ("sd", SD),
        ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
        ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
        ("geof", tripletrail::geo::GEOF),
        ("st", tripletrail::geo::ST),
        ("rail", tripletrail::geo::RAIL),
        ("formats", "http://www.w3.org/ns/formats/"),
        ("void", crate::void::VOID),
        ("dcterms", "http://purl.org/dc/terms/"),
        ("dcat", crate::dcat::DCAT),
        ("foaf", "http://xmlns.com/foaf/0.1/"),
    ] {
        serializer = serializer
            .with_prefix(prefix, iri)
            .map_err(|e| HttpError(500, e.to_string()))?;
    }
    let mut writer = serializer.for_writer(Vec::new());
    for triple in triples {
        writer.serialize_triple(triple)?;
    }
    Ok(writer.finish()?)
}
