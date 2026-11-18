//! Reads W3C test manifests (`mf:Manifest` in Turtle) into a flat list of tests.

use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::vocab::rdf;
use oxigraph::model::{
    Graph, NamedNode, NamedNodeRef, NamedOrBlankNodeRef, Term, TermRef, TripleRef,
};
use std::collections::HashSet;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;

/// The IRI the rdf-tests repository is published under. Every file of a local checkout is
/// read with the IRI it has there, so manifests, queries, data and expected results resolve
/// relative IRIs the same way.
pub const BASE: &str = "https://w3c.github.io/rdf-tests/";

const MF: &str = "http://www.w3.org/2001/sw/DataAccess/tests/test-manifest#";
const QT: &str = "http://www.w3.org/2001/sw/DataAccess/tests/test-query#";
const UT: &str = "http://www.w3.org/2009/sparql/tests/test-update#";
const HT: &str = "http://www.w3.org/2011/http#";
const RDFT: &str = "http://www.w3.org/ns/rdftest#";
const DAWGT: &str = "http://www.w3.org/2001/sw/DataAccess/tests/test-dawg#";
const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";

/// The IRIs of the SHACL test suite's files, as in the W3C SHACL implementation report
/// (`urn:x-shacl-test:/core/node/class-001`).
pub const SHACL_BASE: &str = "urn:x-shacl-test:/";

/// The SHACL test suite's directory in a checkout of w3c/data-shapes.
pub const SHACL_DIR: &str = "data-shapes-test-suite/tests";

const SHT: &str = "http://www.w3.org/ns/shacl-test#";

/// A local checkout of a test suite (w3c/rdf-tests, or the SHACL tests of w3c/data-shapes),
/// with the IRI its files are read under.
#[derive(Clone)]
pub struct Files {
    root: PathBuf,
    base: &'static str,
    /// Text replaced in every file read: the mock endpoints of a federated test (an
    /// endpoint's IRI, the URL of the server that stands in for it).
    rewrites: Vec<(String, String)>,
}

impl Files {
    /// A checkout of w3c/rdf-tests.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_base(root, BASE)
    }

    /// The files under `root` read with IRIs under `base`.
    pub fn with_base(root: impl Into<PathBuf>, base: &'static str) -> Self {
        Self {
            root: root.into(),
            base,
            rewrites: Vec::new(),
        }
    }

    /// The same files, with `from` replaced by `to` in their text.
    pub fn rewriting(&self, rewrites: Vec<(String, String)>) -> Self {
        Self {
            rewrites,
            ..self.clone()
        }
    }

    fn rewrite(&self, mut text: String) -> String {
        for (from, to) in &self.rewrites {
            text = text.replace(from.as_str(), to);
        }
        text
    }

    /// The local path of a file IRI under the suite's base IRI.
    pub fn path(&self, iri: &str) -> Result<PathBuf, String> {
        iri.strip_prefix(self.base)
            .map(|rest| self.root.join(rest))
            .ok_or_else(|| format!("{iri} is not a file of the test suite"))
    }

    pub fn is_local(&self, iri: &str) -> bool {
        iri.starts_with(self.base)
    }

    pub fn read_to_string(&self, iri: &str) -> Result<String, String> {
        let path = self.path(iri)?;
        std::fs::read_to_string(&path)
            .map(|text| self.rewrite(text))
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn open(&self, iri: &str) -> Result<Box<dyn std::io::BufRead>, String> {
        if !self.rewrites.is_empty() {
            let text = self.read_to_string(iri)?;
            return Ok(Box::new(std::io::Cursor::new(text.into_bytes())));
        }
        let path = self.path(iri)?;
        File::open(&path)
            .map(|f| Box::new(BufReader::new(f)) as Box<dyn std::io::BufRead>)
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// An RDF parser for a file IRI, from its extension, with the IRI as base.
    pub fn rdf_parser(&self, iri: &str) -> Result<RdfParser, String> {
        let extension = iri.rsplit('.').next().unwrap_or_default();
        let format = RdfFormat::from_extension(extension)
            .ok_or_else(|| format!("{iri}: unknown RDF file extension"))?;
        // Blank nodes of two files are different nodes, even with the same label.
        Ok(RdfParser::from_format(format)
            .with_base_iri(iri)
            .map_err(|e| format!("{iri}: {e}"))?
            .rename_blank_nodes())
    }

    /// Parses an RDF file into a dataset.
    pub fn dataset(&self, iri: &str) -> Result<oxigraph::model::Dataset, String> {
        let mut dataset = oxigraph::model::Dataset::new();
        for quad in self.rdf_parser(iri)?.for_reader(self.open(iri)?) {
            dataset.insert(&quad.map_err(|e| format!("{iri}: {e}"))?);
        }
        Ok(dataset)
    }

    /// Parses an RDF file into a graph (the triples of every graph of a quad format).
    pub fn graph(&self, iri: &str) -> Result<Graph, String> {
        let mut graph = Graph::new();
        for quad in self.rdf_parser(iri)?.for_reader(self.open(iri)?) {
            let quad = quad.map_err(|e| format!("{iri}: {e}"))?;
            graph.insert(TripleRef::new(&quad.subject, &quad.predicate, &quad.object));
        }
        Ok(graph)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    PositiveSyntax,
    NegativeSyntax,
    PositiveUpdateSyntax,
    NegativeUpdateSyntax,
    QueryEvaluation,
    UpdateEvaluation,
    CsvResultFormat,
    /// An RDF parser test: the file must load (`true`) or be refused (`false`).
    RdfSyntax(bool),
    /// An RDF parser test: the file loads into the dataset of the result file.
    RdfEval,
    /// N-Triples canonicalization: the file written back is the result file.
    RdfC14n,
    /// SPARQL 1.1 Protocol: HTTP requests to a server.
    Protocol,
    /// SPARQL 1.1 Graph Store Protocol: HTTP requests to a server.
    GraphStore,
    /// SPARQL 1.1 Service Description: the description a server's endpoint answers.
    ServiceDescription,
    /// An RDF semantics test (`mf:PositiveEntailmentTest` or `mf:NegativeEntailmentTest`):
    /// the action entails the result graph, or is inconsistent when the result is `false`
    /// (`true`), or does not (`false`).
    Entailment(bool),
    /// SHACL validation (`sht:Validate`): the data graph validated against the shapes graph
    /// gives the expected report, or fails (`sht:Failure`).
    ShaclValidate,
    /// A test type the runner doesn't run: its local name.
    Other(String),
}

impl Kind {
    fn from_type(iri: &str) -> Self {
        if let Some(name) = iri.strip_prefix(SHT) {
            return if name == "Validate" {
                Self::ShaclValidate
            } else {
                Self::Other(name.to_owned())
            };
        }
        if let Some(name) = iri.strip_prefix(RDFT) {
            return if name.ends_with("PositiveSyntax") {
                Self::RdfSyntax(true)
            } else if name.ends_with("NegativeSyntax") || name.ends_with("NegativeEval") {
                Self::RdfSyntax(false)
            } else if name.ends_with("Eval") {
                Self::RdfEval
            } else if name.ends_with("C14N") {
                Self::RdfC14n
            } else {
                Self::Other(name.to_owned())
            };
        }
        let name = iri.strip_prefix(MF).unwrap_or(iri);
        // The SPARQL 1.1 suites add "11" to the syntax test types.
        match name.trim_end_matches(|c: char| c.is_ascii_digit()) {
            "PositiveSyntaxTest" => Self::PositiveSyntax,
            "NegativeSyntaxTest" => Self::NegativeSyntax,
            "PositiveUpdateSyntaxTest" => Self::PositiveUpdateSyntax,
            "NegativeUpdateSyntaxTest" => Self::NegativeUpdateSyntax,
            "QueryEvaluationTest" => Self::QueryEvaluation,
            "UpdateEvaluationTest" => Self::UpdateEvaluation,
            "CSVResultFormatTest" => Self::CsvResultFormat,
            "ProtocolTest" => Self::Protocol,
            "GraphStoreProtocolTest" => Self::GraphStore,
            "ServiceDescriptionTest" => Self::ServiceDescription,
            "PositiveEntailmentTest" => Self::Entailment(true),
            "NegativeEntailmentTest" => Self::Entailment(false),
            _ => Self::Other(name.to_owned()),
        }
    }
}

/// A named graph of a test dataset: its name and the file it is read from. In query tests
/// the name is the file's IRI; in update tests it is the `rdfs:label`.
#[derive(Clone, Debug)]
pub struct GraphData {
    pub name: String,
    pub file: String,
}

#[derive(Clone, Debug)]
pub struct Test {
    pub id: String,
    pub kind: Kind,
    pub name: String,
    /// The label of the manifest that lists it.
    pub group: String,
    /// `dawgt:approval` other than approved or proposed (`NotApproved`, `Withdrawn`, …).
    pub not_approved: Option<String>,
    /// The query or update file.
    pub action: Option<String>,
    pub data: Vec<String>,
    pub graph_data: Vec<GraphData>,
    /// Set when the action names service data or only entailment regimes the runner
    /// doesn't support.
    pub unsupported: Option<String>,
    /// The mock endpoints of a federated query test (`qt:serviceData`): each endpoint's IRI
    /// and the data it holds.
    pub service_data: Vec<(String, String)>,
    /// The entailment regime the query is answered under (`sd:entailmentRegime`), by local
    /// name: `RDFS`, or `OWL-RDF-Based`, approximated by the OWL 2 RL rules. In an RDF
    /// semantics test, `mf:entailmentRegime`: `simple`, `RDF` or `RDFS`.
    pub regime: Option<String>,
    /// The datatypes an RDF semantics test recognizes (`mf:recognizedDatatypes`).
    pub recognized: Vec<String>,
    /// The expected results file (query tests).
    pub result: Option<String>,
    /// Other results files that are also right (the GeoSPARQL benchmark's alternatives).
    pub alternatives: Vec<String>,
    /// The expected dataset (update tests).
    pub result_data: Vec<String>,
    pub result_graph_data: Vec<GraphData>,
    /// `mf:resultCardinality mf:LaxCardinality`: duplicates may differ.
    pub lax: bool,
    /// `mf:requires`, by local name.
    pub requires: Vec<String>,
    /// The HTTP requests of a protocol test, in order.
    pub requests: Vec<HttpRequest>,
    /// The expected validation report of a SHACL test: the report node and its results
    /// with every blank node they reach. `None` when validation must fail (`sht:Failure`).
    pub report: Option<Graph>,
}

/// An `ht:Request` of a protocol test, with what its response must be.
#[derive(Clone, Debug, Default)]
pub struct HttpRequest {
    pub method: String,
    /// The path and query string, as in the suite (`/sparql/?…`, `/gsp?graph=…`).
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub response: HttpResponse,
}

#[derive(Clone, Debug, Default)]
pub struct HttpResponse {
    /// Acceptable statuses: `hts:` local names (`OK`, `Created`, `StatusCode2xx`, …).
    pub statuses: Vec<String>,
    pub boolean: Option<bool>,
    /// `boolean`, `tabular` or `RDF`.
    pub format: Option<String>,
    /// The template variable the `Location` header fills in later requests.
    pub location: Option<String>,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

/// Reads a manifest and the manifests it includes, depth first, in their listed order.
pub fn read(files: &Files, manifest: &str) -> Result<Vec<Test>, String> {
    let mut tests = Vec::new();
    let mut seen = HashSet::new();
    read_into(files, manifest, None, &mut tests, &mut seen)?;
    Ok(tests)
}

fn read_into(
    files: &Files,
    manifest: &str,
    parent_group: Option<&str>,
    tests: &mut Vec<Test>,
    seen: &mut HashSet<String>,
) -> Result<(), String> {
    if !seen.insert(manifest.to_owned()) {
        return Ok(());
    }
    let graph = files.graph(manifest)?;
    let node = NamedNode::new(manifest).map_err(|e| e.to_string())?;
    let mf_manifest = NamedNode::new_unchecked(format!("{MF}Manifest"));
    // Usually `<>`, but some manifests name themselves (`trs:manifest`).
    let subject = if graph.contains(TripleRef::new(&node, rdf::TYPE, &mf_manifest)) {
        NamedOrBlankNodeRef::from(node.as_ref())
    } else {
        match graph
            .subjects_for_predicate_object(rdf::TYPE, &mf_manifest)
            .next()
        {
            Some(subject) => subject,
            None => return Err(format!("{manifest}: no mf:Manifest in it")),
        }
    };
    // The SHACL suite's groups are its directories ("core/node"); its per-file manifests,
    // without a label, go in the group of the manifest that includes them.
    let directory = manifest
        .strip_prefix(SHACL_BASE)
        .and_then(|m| m.strip_suffix("/manifest.ttl"));
    let group = match (directory, string(&graph, subject, RDFS_LABEL)) {
        (Some(directory), _) => directory.to_owned(),
        (None, Some(label)) => label,
        (None, None) => parent_group.map_or_else(|| manifest.to_owned(), str::to_owned),
    };
    // A list of manifests, or (in the SHACL suite) one manifest per `mf:include`, read in
    // IRI order: a graph keeps no order.
    let mut single = Vec::new();
    for include in objects(&graph, subject, &format!("{MF}include")) {
        match include {
            TermRef::NamedNode(n) if n != rdf::NIL => single.push(n.as_str().to_owned()),
            _ => {
                for item in list(&graph, include) {
                    if let TermRef::NamedNode(m) = item {
                        read_into(files, m.as_str(), Some(&group), tests, seen)?;
                    }
                }
            }
        }
    }
    single.sort();
    for m in single {
        read_into(files, &m, Some(&group), tests, seen)?;
    }
    for entries in objects(&graph, subject, &format!("{MF}entries")) {
        for item in list(&graph, entries) {
            if let Some(test) = test(&graph, item, &group) {
                tests.push(test);
            }
        }
    }
    Ok(())
}

fn test(graph: &Graph, node: TermRef<'_>, group: &str) -> Option<Test> {
    let subject = as_subject(node)?;
    let id = match subject {
        NamedOrBlankNodeRef::NamedNode(n) => n.as_str().to_owned(),
        NamedOrBlankNodeRef::BlankNode(b) => format!("_:{}", b.as_str()),
    };
    let kind = objects(graph, subject, rdf::TYPE.as_str())
        .find_map(|t| match t {
            TermRef::NamedNode(t) => Some(Kind::from_type(t.as_str())),
            _ => None,
        })
        .unwrap_or_else(|| Kind::Other("untyped".into()));
    let not_approved = iri(graph, subject, &format!("{DAWGT}approval"))
        .or_else(|| iri(graph, subject, &format!("{RDFT}approval")))
        .map(|a| {
            a.strip_prefix(DAWGT)
                .or_else(|| a.strip_prefix(RDFT))
                .unwrap_or(&a)
                .to_owned()
        })
        .filter(|a| a != "Approved" && a != "Proposed");
    let lax = iri(graph, subject, &format!("{MF}resultCardinality"))
        .is_some_and(|c| c == format!("{MF}LaxCardinality"));
    let mut test = Test {
        id,
        kind,
        name: string(graph, subject, &format!("{MF}name")).unwrap_or_default(),
        group: group.to_owned(),
        not_approved,
        action: None,
        data: Vec::new(),
        graph_data: Vec::new(),
        unsupported: None,
        service_data: Vec::new(),
        regime: None,
        recognized: Vec::new(),
        result: None,
        alternatives: Vec::new(),
        result_data: Vec::new(),
        result_graph_data: Vec::new(),
        lax,
        requires: iris(graph, subject, &format!("{MF}requires"))
            .into_iter()
            .map(|r| r.strip_prefix(MF).unwrap_or(&r).to_owned())
            .collect(),
        requests: Vec::new(),
        report: None,
    };
    if test.kind == Kind::ShaclValidate {
        shacl_test(graph, subject, &mut test);
        return Some(test);
    }
    if matches!(test.kind, Kind::Protocol | Kind::GraphStore) {
        test.graph_data = update_graphs(graph, subject);
        if let Some(connection) = objects(graph, subject, &format!("{MF}action"))
            .next()
            .and_then(as_subject)
        {
            test.requests = objects(graph, connection, &format!("{HT}requests"))
                .flat_map(|l| list(graph, l))
                .filter_map(as_subject)
                .map(|r| request(graph, r))
                .collect();
        }
        return Some(test);
    }
    if let Kind::Entailment(_) = test.kind {
        test.regime = string(graph, subject, &format!("{MF}entailmentRegime"));
        test.recognized = objects(graph, subject, &format!("{MF}recognizedDatatypes"))
            .flat_map(|l| list(graph, l))
            .filter_map(|d| match d {
                TermRef::NamedNode(d) => Some(d.as_str().to_owned()),
                _ => None,
            })
            .collect();
    }
    let action = objects(graph, subject, &format!("{MF}action")).next();
    match action {
        // Syntax tests name the file directly.
        Some(TermRef::NamedNode(file)) => test.action = Some(file.as_str().to_owned()),
        Some(action) => {
            let action = as_subject(action)?;
            test.action = iri(graph, action, &format!("{QT}query"))
                .or_else(|| iri(graph, action, &format!("{UT}request")));
            test.data = iris(graph, action, &format!("{QT}data"));
            test.data.extend(iris(graph, action, &format!("{UT}data")));
            test.graph_data = iris(graph, action, &format!("{QT}graphData"))
                .into_iter()
                .map(|file| GraphData {
                    name: file.clone(),
                    file,
                })
                .collect();
            test.graph_data.extend(update_graphs(graph, action));
            for service in objects(graph, action, &format!("{QT}serviceData")) {
                let service = match service {
                    TermRef::NamedNode(n) => NamedOrBlankNodeRef::from(n),
                    TermRef::BlankNode(b) => NamedOrBlankNodeRef::from(b),
                    _ => continue,
                };
                let endpoint = objects(graph, service, &format!("{QT}endpoint")).next();
                let data = objects(graph, service, &format!("{QT}data")).next();
                if let (Some(TermRef::NamedNode(endpoint)), Some(TermRef::NamedNode(data))) =
                    (endpoint, data)
                {
                    test.service_data
                        .push((endpoint.as_str().to_owned(), data.as_str().to_owned()));
                }
            }
            let regimes: Vec<String> = objects(
                graph,
                action,
                "http://www.w3.org/ns/sparql-service-description#entailmentRegime",
            )
            .flat_map(|r| match r {
                TermRef::BlankNode(_) => list(graph, r),
                _ => vec![r],
            })
            .filter_map(|r| match r {
                TermRef::NamedNode(n) => Some(
                    n.as_str()
                        .strip_prefix("http://www.w3.org/ns/entailment/")
                        .unwrap_or(n.as_str())
                        .to_owned(),
                ),
                _ => None,
            })
            .collect();
            if !regimes.is_empty() {
                // The expected answers hold under each regime listed: take the one TripleTrail
                // has the most rules of.
                test.regime = ["OWL-RDF-Based", "RDFS"]
                    .into_iter()
                    .find(|r| regimes.iter().any(|x| x == r))
                    .map(str::to_owned);
                if test.regime.is_none() {
                    test.unsupported = Some(format!(
                        "entailment regime {} (TripleTrail has RDFS and OWL 2 RL)",
                        regimes.join(", ")
                    ));
                }
            }
        }
        None => {}
    }
    match objects(graph, subject, &format!("{MF}result")).next() {
        Some(TermRef::NamedNode(file)) => test.result = Some(file.as_str().to_owned()),
        Some(result) => {
            if let Some(result) = as_subject(result) {
                test.result_data = iris(graph, result, &format!("{UT}data"));
                test.result_graph_data = update_graphs(graph, result);
            }
        }
        None => {}
    }
    Some(test)
}

/// The graphs and expected report of a SHACL test.
fn shacl_test(graph: &Graph, subject: NamedOrBlankNodeRef<'_>, test: &mut Test) {
    if let Some(status) = iri(graph, subject, &format!("{MF}status")) {
        let status = status.strip_prefix(SHT).unwrap_or(&status).to_owned();
        if status != "approved" && status != "proposed" {
            test.not_approved = Some(status);
        }
    }
    if let Some(action) = objects(graph, subject, &format!("{MF}action"))
        .next()
        .and_then(as_subject)
    {
        test.action = iri(graph, action, &format!("{SHT}shapesGraph"));
        test.data = iris(graph, action, &format!("{SHT}dataGraph"));
    }
    match objects(graph, subject, &format!("{MF}result")).next() {
        Some(TermRef::NamedNode(n)) if n.as_str() == format!("{SHT}Failure") => {}
        Some(result) => {
            let mut report = Graph::new();
            if let Some(result) = as_subject(result) {
                closure(graph, result, &mut report);
            }
            test.report = Some(report);
        }
        None => test.unsupported = Some("no expected result".into()),
    }
}

/// The triples of `node`, and of every blank node they reach.
pub fn closure(graph: &Graph, node: NamedOrBlankNodeRef<'_>, into: &mut Graph) {
    let mut todo = vec![node.into_owned()];
    let mut seen = HashSet::new();
    while let Some(node) = todo.pop() {
        if !seen.insert(node.clone()) {
            continue;
        }
        for t in graph.triples_for_subject(&node) {
            into.insert(t);
            if let TermRef::BlankNode(b) = t.object {
                todo.push(b.into_owned().into());
            }
        }
    }
}

fn request(graph: &Graph, node: NamedOrBlankNodeRef<'_>) -> HttpRequest {
    let response = objects(graph, node, &format!("{HT}resp"))
        .next()
        .and_then(as_subject)
        .map(|r| HttpResponse {
            statuses: iris(graph, r, &format!("{MF}expectedStatus"))
                .into_iter()
                .map(|s| s.rsplit('#').next().unwrap_or_default().to_owned())
                .collect(),
            boolean: string(graph, r, &format!("{MF}expectedBoolean")).map(|b| b == "true"),
            format: string(graph, r, &format!("{MF}expectedFormat")),
            location: string(graph, r, &format!("{MF}expectedLocation")),
            headers: headers(graph, r),
            body: body(graph, r),
        })
        .unwrap_or_default();
    HttpRequest {
        method: string(graph, node, &format!("{HT}methodName")).unwrap_or_else(|| "GET".into()),
        path: string(graph, node, &format!("{HT}absolutePath")).unwrap_or_else(|| "/".into()),
        headers: headers(graph, node),
        body: body(graph, node),
        response,
    }
}

/// `ht:headers ( [ ht:fieldName "…" ; ht:fieldValue "…" ] … )`.
fn headers(graph: &Graph, node: NamedOrBlankNodeRef<'_>) -> Vec<(String, String)> {
    objects(graph, node, &format!("{HT}headers"))
        .flat_map(|l| list(graph, l))
        .filter_map(as_subject)
        .filter_map(|h| {
            Some((
                string(graph, h, &format!("{HT}fieldName"))?,
                string(graph, h, &format!("{HT}fieldValue"))?,
            ))
        })
        .collect()
}

/// `ht:body [ cnt:chars "…" ]`.
fn body(graph: &Graph, node: NamedOrBlankNodeRef<'_>) -> Option<String> {
    let body = objects(graph, node, &format!("{HT}body"))
        .next()
        .and_then(as_subject)?;
    string(graph, body, "http://www.w3.org/2011/content#chars")
}

/// `ut:graphData [ ut:graph <file> ; rdfs:label "name" ]`.
fn update_graphs(graph: &Graph, node: NamedOrBlankNodeRef<'_>) -> Vec<GraphData> {
    objects(graph, node, &format!("{UT}graphData"))
        .filter_map(as_subject)
        .filter_map(|g| {
            let file = iri(graph, g, &format!("{UT}graph"))?;
            let name = string(graph, g, RDFS_LABEL).unwrap_or_else(|| file.clone());
            Some(GraphData { name, file })
        })
        .collect()
}

fn as_subject(term: TermRef<'_>) -> Option<NamedOrBlankNodeRef<'_>> {
    match term {
        TermRef::NamedNode(n) => Some(n.into()),
        TermRef::BlankNode(b) => Some(b.into()),
        _ => None,
    }
}

fn objects<'a>(
    graph: &'a Graph,
    subject: NamedOrBlankNodeRef<'a>,
    predicate: &str,
) -> std::vec::IntoIter<TermRef<'a>> {
    graph
        .objects_for_subject_predicate(subject, NamedNodeRef::new_unchecked(predicate))
        .collect::<Vec<_>>()
        .into_iter()
}

fn iri(graph: &Graph, subject: NamedOrBlankNodeRef<'_>, predicate: &str) -> Option<String> {
    iris(graph, subject, predicate).into_iter().next()
}

fn iris(graph: &Graph, subject: NamedOrBlankNodeRef<'_>, predicate: &str) -> Vec<String> {
    let mut iris: Vec<String> = objects(graph, subject, predicate)
        .filter_map(|o| match o {
            TermRef::NamedNode(n) => Some(n.as_str().to_owned()),
            _ => None,
        })
        .collect();
    iris.sort();
    iris
}

fn string(graph: &Graph, subject: NamedOrBlankNodeRef<'_>, predicate: &str) -> Option<String> {
    objects(graph, subject, predicate).find_map(|o| match o {
        TermRef::Literal(l) => Some(l.value().to_owned()),
        _ => None,
    })
}

/// The members of an RDF list.
pub fn list<'a>(graph: &'a Graph, head: TermRef<'a>) -> Vec<TermRef<'a>> {
    let mut items = Vec::new();
    let mut node = head;
    let mut seen = HashSet::new();
    while node != TermRef::from(rdf::NIL) {
        let Some(subject) = as_subject(node) else {
            break;
        };
        if !seen.insert(Term::from(node)) {
            break;
        }
        if let Some(first) = graph
            .objects_for_subject_predicate(subject, rdf::FIRST)
            .next()
        {
            items.push(first);
        }
        match graph
            .objects_for_subject_predicate(subject, rdf::REST)
            .next()
        {
            Some(rest) => node = rest,
            None => break,
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_types_with_and_without_the_version_suffix() {
        assert_eq!(
            Kind::from_type(&format!("{MF}PositiveSyntaxTest11")),
            Kind::PositiveSyntax
        );
        assert_eq!(
            Kind::from_type(&format!("{MF}NegativeUpdateSyntaxTest11")),
            Kind::NegativeUpdateSyntax
        );
        assert_eq!(
            Kind::from_type(&format!("{MF}QueryEvaluationTest")),
            Kind::QueryEvaluation
        );
        assert_eq!(
            Kind::from_type(&format!("{MF}ProtocolTest")),
            Kind::Protocol
        );
        assert_eq!(
            Kind::from_type(&format!("{MF}ServiceDescriptionTest")),
            Kind::ServiceDescription
        );
        assert_eq!(
            Kind::from_type(&format!("{MF}PositiveEntailmentTest")),
            Kind::Entailment(true)
        );
        assert_eq!(
            Kind::from_type(&format!("{MF}NegativeEntailmentTest")),
            Kind::Entailment(false)
        );
    }

    #[test]
    fn rdf_test_types() {
        let kind = |name: &str| Kind::from_type(&format!("{RDFT}{name}"));
        assert_eq!(kind("TestTurtlePositiveSyntax"), Kind::RdfSyntax(true));
        assert_eq!(kind("TestTrigNegativeSyntax"), Kind::RdfSyntax(false));
        assert_eq!(kind("TestTurtleNegativeEval"), Kind::RdfSyntax(false));
        assert_eq!(kind("TestXMLEval"), Kind::RdfEval);
        assert_eq!(kind("TestNTriplesPositiveC14N"), Kind::RdfC14n);
    }

    #[test]
    fn shacl_tests() {
        assert_eq!(
            Kind::from_type(&format!("{SHT}Validate")),
            Kind::ShaclValidate
        );
        let files = Files::with_base("/x/data-shapes-test-suite/tests", SHACL_BASE);
        assert_eq!(
            files
                .path("urn:x-shacl-test:/core/node/class-001.ttl")
                .unwrap(),
            std::path::Path::new("/x/data-shapes-test-suite/tests/core/node/class-001.ttl")
        );
        assert!(!files.is_local(&format!("{BASE}sparql/x.rq")));
    }

    #[test]
    fn suite_iris_map_to_the_checkout() {
        let files = Files::new("/r");
        assert_eq!(
            files.path(&format!("{BASE}sparql/x.rq")).unwrap(),
            PathBuf::from("/r/sparql/x.rq")
        );
        assert!(files.path("http://example.org/x.rq").is_err());
    }
}
