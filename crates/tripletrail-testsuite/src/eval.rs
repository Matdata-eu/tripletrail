//! Runs one test against a fresh TripleTrail engine and compares the answer with the expected
//! one.

use crate::manifest::{Files, Kind, Test};
use std::collections::BTreeSet;
use std::sync::mpsc;
use std::time::Duration;
use tripletrail::Engine;
use tripletrail::oxigraph::model::dataset::CanonicalizationAlgorithm;
use tripletrail::oxigraph::model::vocab::xsd;
use tripletrail::oxigraph::model::{
    BlankNode, Dataset, Graph, GraphName, Literal, NamedNode, NamedOrBlankNode,
    NamedOrBlankNodeRef, Quad, Term, TermRef, Triple,
};
use tripletrail::oxigraph::sparql::results::{
    QueryResultsFormat, QueryResultsParser, QueryResultsSerializer, ReaderQueryResultsParserOutput,
};
use tripletrail::oxigraph::sparql::{CancellationToken, QueryResults};

const RS: &str = "http://www.w3.org/2001/sw/DataAccess/tests/result-set#";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Failed(String),
    /// Not run, with the reason.
    Untested(String),
}

/// Runs a test in its own thread, giving up after `timeout` (the engine is cancelled and the
/// thread left to finish on its own).
pub fn run(files: &std::sync::Arc<Files>, test: &Test, timeout: Duration) -> Outcome {
    run_with(files, test, timeout, None)
}

/// [`run`], with SERVICE calls allowed to the endpoints under `remote` (the mock endpoints
/// of a federated test).
pub fn run_with(
    files: &std::sync::Arc<Files>,
    test: &Test,
    timeout: Duration,
    remote: Option<String>,
) -> Outcome {
    if let Some(reason) = untested(test) {
        return Outcome::Untested(reason);
    }
    let token = CancellationToken::new();
    let (send, receive) = mpsc::channel();
    let (files, owned, thread_token) = (files.clone(), test.clone(), token.clone());
    let spawned = std::thread::Builder::new()
        .name("test".into())
        .stack_size(64 << 20)
        .spawn(move || {
            let outcome = match evaluate(&files, &owned, thread_token, remote) {
                Ok(()) => Outcome::Passed,
                Err(e) => Outcome::Failed(e),
            };
            let _ = send.send(outcome);
        });
    if let Err(e) = spawned {
        return Outcome::Failed(format!("could not start the test thread: {e}"));
    }
    match receive.recv_timeout(timeout) {
        Ok(outcome) => outcome,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            token.cancel();
            Outcome::Failed(format!("timed out after {} s", timeout.as_secs()))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Outcome::Failed("the test panicked".into()),
    }
}

fn untested(test: &Test) -> Option<String> {
    if let Some(approval) = &test.not_approved {
        return Some(format!("not approved ({approval})"));
    }
    if let Some(reason) = &test.unsupported {
        return Some(reason.clone());
    }
    if let Kind::Other(kind) = &test.kind {
        return Some(format!("test type {kind} is not run yet"));
    }
    None
}

fn evaluate(
    files: &Files,
    test: &Test,
    token: CancellationToken,
    remote: Option<String>,
) -> Result<(), String> {
    let action = test.action.as_deref().ok_or("the test has no action")?;
    match test.kind {
        Kind::PositiveSyntax | Kind::NegativeSyntax => {
            let text = with_base(&files.read_to_string(action)?, action);
            syntax(
                test.kind == Kind::PositiveSyntax,
                Engine::check_query(&text),
            )
        }
        Kind::PositiveUpdateSyntax | Kind::NegativeUpdateSyntax => {
            let text = with_base(&files.read_to_string(action)?, action);
            syntax(
                test.kind == Kind::PositiveUpdateSyntax,
                Engine::check_update(&text),
            )
        }
        Kind::QueryEvaluation | Kind::CsvResultFormat => query(files, test, action, token, remote),
        Kind::UpdateEvaluation => update(files, test, action, token),
        Kind::RdfSyntax(_) | Kind::RdfEval | Kind::RdfC14n => rdf(files, test, action, token),
        Kind::ShaclValidate => crate::shacl::validate(files, test, token),
        Kind::Entailment(_) => crate::semantics::check(files, test, action, token),
        Kind::Other(_) | Kind::Protocol | Kind::GraphStore | Kind::ServiceDescription => {
            unreachable!("run elsewhere or not at all")
        }
    }
}

fn syntax(positive: bool, parsed: Result<(), tripletrail::Error>) -> Result<(), String> {
    match (positive, parsed) {
        (true, Ok(())) | (false, Err(_)) => Ok(()),
        (true, Err(e)) => Err(format!("rejected: {e}")),
        (false, Ok(())) => Err("accepted, but it is not valid".into()),
    }
}

/// Sets the file's IRI as base: queries and updates in the suites use relative IRIs.
fn with_base(text: &str, iri: &str) -> String {
    format!("BASE <{iri}>\n{}", text.trim_start_matches('\u{feff}'))
}

pub(crate) fn engine(token: CancellationToken) -> Result<Engine, String> {
    Ok(Engine::new()
        .map_err(|e| e.to_string())?
        .with_cancellation_token(token))
}

pub(crate) fn load(
    engine: &Engine,
    files: &Files,
    file: &str,
    graph: impl Into<GraphName>,
) -> Result<(), String> {
    engine
        .load_into(graph, files.rdf_parser(file)?, files.open(file)?)
        .map_err(|e| format!("loading {file}: {e}"))
}

fn named(name: &str) -> Result<NamedNode, String> {
    NamedNode::new(name).map_err(|e| format!("{name}: {e}"))
}

fn query(
    files: &Files,
    test: &Test,
    action: &str,
    token: CancellationToken,
    remote: Option<String>,
) -> Result<(), String> {
    let mut engine = engine(token)?;
    if let Some(allow) = remote {
        engine = engine.with_remote_services(tripletrail::remote::RemoteConfig {
            allow: vec![allow],
            ..Default::default()
        });
    }
    for file in &test.data {
        load(&engine, files, file, GraphName::DefaultGraph)?;
    }
    let mut loaded = BTreeSet::new();
    for graph in &test.graph_data {
        load(&engine, files, &graph.file, named(&graph.name)?)?;
        loaded.insert(graph.name.clone());
    }
    let raw = files.read_to_string(action)?;
    // Graphs named in FROM and FROM NAMED are files of the suite: load them under their IRI.
    for iri in dataset_iris(&raw, action) {
        if files.is_local(&iri) && loaded.insert(iri.clone()) {
            load(&engine, files, &iri, named(&iri)?)?;
        }
    }
    if let Some(regime) = &test.regime {
        entail(&engine, regime)?;
    }
    let text = with_base(&raw, action);
    let results = engine
        .query(&text)
        .map_err(|e| format!("query failed: {e}"))?;
    let result = test
        .result
        .as_deref()
        .ok_or("the test has no expected result")?;
    if test.kind == Kind::CsvResultFormat {
        return csv(files, results, result);
    }
    let ordered = has_order_by(&raw);
    let actual = Answer::from_results(results, ordered)?;
    let expected = Answer::expected(files, result, &actual)?;
    let mismatch = match compare(expected, actual.clone(), test.lax) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    // Any of the alternatives is right too; the failure reports the main answer.
    for alternative in &test.alternatives {
        let expected = Answer::expected(files, alternative, &actual)?;
        if compare(expected, actual.clone(), test.lax).is_ok() {
            return Ok(());
        }
    }
    Err(mismatch)
}

/// Answers under an entailment regime: materializes the default graph's entailment as
/// `tripletrail entail` does, and adds it to the default graph.
pub(crate) fn entail(engine: &Engine, regime: &str) -> Result<(), String> {
    use tripletrail::store::entail::Regime;
    const INTO: &str = "urn:tripletrail:testsuite:entailed";
    let regime = match regime {
        "RDFS" => Regime::Rdfs,
        "OWL-RDF-Based" => Regime::OwlRl,
        other => return Err(format!("no entailment regime {other}")),
    };
    let into = NamedNode::new(INTO).map_err(|e| e.to_string())?;
    engine
        .entail(into.as_ref(), regime)
        .map_err(|e| format!("entailment failed: {e}"))?;
    engine
        .update(&format!(
            "INSERT {{ ?s ?p ?o }} WHERE {{ GRAPH <{INTO}> {{ ?s ?p ?o }} }}; DROP SILENT GRAPH <{INTO}>"
        ))
        .map_err(|e| format!("adding the entailment failed: {e}"))
}

/// The IRIs of a query's `FROM` and `FROM NAMED` clauses.
fn dataset_iris(text: &str, base: &str) -> Vec<String> {
    let Ok(parser) = spargebra::SparqlParser::new().with_base_iri(base) else {
        return Vec::new();
    };
    let Ok(query) = parser.parse_query(text) else {
        return Vec::new();
    };
    let dataset = match query {
        spargebra::Query::Select { dataset, .. }
        | spargebra::Query::Construct { dataset, .. }
        | spargebra::Query::Describe { dataset, .. }
        | spargebra::Query::Ask { dataset, .. } => dataset,
    };
    let Some(dataset) = dataset else {
        return Vec::new();
    };
    dataset
        .default
        .iter()
        .chain(dataset.named.iter().flatten())
        .map(|n| n.as_str().to_owned())
        .collect()
}

/// Whether the query orders its results: an `ORDER BY` outside every `{ }`. One inside a
/// sub-select orders only that sub-select's rows (W3C eval-triple-terms order-1 and order-2).
fn has_order_by(query: &str) -> bool {
    let mut depth = 0usize;
    let mut words = Vec::new();
    for line in query.lines() {
        let mut word = String::new();
        for c in line.chars().chain([' ']) {
            match c {
                '{' | '}' | ' ' | '\t' => {
                    if !word.is_empty() {
                        words.push((depth, std::mem::take(&mut word).to_ascii_uppercase()));
                    }
                    if c == '{' {
                        depth += 1;
                    } else if c == '}' {
                        depth = depth.saturating_sub(1);
                    }
                }
                _ => word.push(c),
            }
        }
    }
    words
        .windows(2)
        .any(|w| w[0].0 == 0 && w[0].1 == "ORDER" && w[1].1 == "BY")
}

fn update(
    files: &Files,
    test: &Test,
    action: &str,
    token: CancellationToken,
) -> Result<(), String> {
    let engine = engine(token)?;
    for file in &test.data {
        load(&engine, files, file, GraphName::DefaultGraph)?;
    }
    for graph in &test.graph_data {
        load(&engine, files, &graph.file, named(&graph.name)?)?;
    }
    let text = with_base(&files.read_to_string(action)?, action);
    engine
        .update(&text)
        .map_err(|e| format!("update failed: {e}"))?;

    let mut expected = Dataset::new();
    // A TriG or N-Quads result keeps its named graphs (W3C eval-triple-terms update-1).
    for file in &test.result_data {
        for q in files.dataset(file)?.iter() {
            expected.insert(q);
        }
    }
    for graph in &test.result_graph_data {
        let name = named(&graph.name)?;
        for t in files.graph(&graph.file)?.iter() {
            expected.insert(&Quad::new(
                t.subject.into_owned(),
                t.predicate.into_owned(),
                t.object.into_owned(),
                name.clone(),
            ));
        }
    }
    same_dataset(&expected, &dump(&engine)?)
}

/// Whether two datasets are the same up to blank node renaming, or how they differ.
fn same_dataset(expected: &Dataset, actual: &Dataset) -> Result<(), String> {
    let (mut e, mut a) = (expected.clone(), actual.clone());
    e.canonicalize(CanonicalizationAlgorithm::Unstable);
    a.canonicalize(CanonicalizationAlgorithm::Unstable);
    if e == a {
        return Ok(());
    }
    let lines = |d: &Dataset| -> BTreeSet<String> {
        d.iter().map(|q| blank_neutral(&q.to_string())).collect()
    };
    Err(difference(
        "quads",
        expected.len(),
        actual.len(),
        &lines(expected),
        &lines(actual),
    ))
}

/// An RDF parser test, through the engine's loader and store: what loads is read back from
/// the store.
fn rdf(files: &Files, test: &Test, action: &str, token: CancellationToken) -> Result<(), String> {
    let engine = engine(token)?;
    let loaded = engine
        .load(files.rdf_parser(action)?, files.open(action)?)
        .map_err(|e| e.to_string());
    match (&test.kind, loaded) {
        (Kind::RdfSyntax(true), Ok(())) | (Kind::RdfSyntax(false), Err(_)) => Ok(()),
        (Kind::RdfSyntax(false), Ok(())) => Err("loaded, but it is not valid".into()),
        (_, Err(e)) => Err(format!("refused: {e}")),
        (Kind::RdfEval, Ok(())) => {
            let result = test
                .result
                .as_deref()
                .ok_or("the test has no expected result")?;
            same_dataset(&files.dataset(result)?, &dump(&engine)?)
        }
        (Kind::RdfC14n, Ok(())) => {
            let result = test
                .result
                .as_deref()
                .ok_or("the test has no expected result")?;
            use tripletrail::oxigraph::io::{RdfFormat, RdfSerializer};
            let format = if action.ends_with(".nq") {
                RdfFormat::NQuads
            } else {
                RdfFormat::NTriples
            };
            let mut writer = RdfSerializer::from_format(format).for_writer(Vec::new());
            for quad in dump(&engine)?.iter() {
                writer.serialize_quad(quad).map_err(|e| e.to_string())?;
            }
            let out = writer.finish().map_err(|e| e.to_string())?;
            let actual = String::from_utf8(out).map_err(|e| e.to_string())?;
            let expected = files.read_to_string(result)?;
            // The store gives the statements back in its own order, with its own blank node
            // labels: compare the lines as sets, labels left out.
            let lines = |text: &str| -> BTreeSet<String> {
                text.lines()
                    .filter(|l| !l.is_empty())
                    .map(blank_neutral)
                    .collect()
            };
            if lines(&actual) == lines(&expected) {
                return Ok(());
            }
            Err(format!(
                "wrote {}, expected {}",
                quote(&[&actual.trim_end().to_owned()]),
                quote(&[&expected.trim_end().to_owned()])
            ))
        }
        (kind, Ok(())) => Err(format!("{kind:?} is not an RDF test")),
    }
}

/// Every quad of the engine's store.
pub(crate) fn dump(engine: &Engine) -> Result<Dataset, String> {
    let results = engine
        .query("SELECT ?s ?p ?o ?g WHERE { { ?s ?p ?o } UNION { GRAPH ?g { ?s ?p ?o } } }")
        .map_err(|e| format!("reading the store: {e}"))?;
    let QueryResults::Solutions(solutions) = results else {
        return Err("reading the store: not solutions".into());
    };
    let mut dataset = Dataset::new();
    for solution in solutions {
        let solution = solution.map_err(|e| format!("reading the store: {e}"))?;
        let subject = match solution.get("s") {
            Some(Term::NamedNode(n)) => NamedOrBlankNode::from(n.clone()),
            Some(Term::BlankNode(b)) => NamedOrBlankNode::from(b.clone()),
            _ => continue,
        };
        let Some(Term::NamedNode(predicate)) = solution.get("p") else {
            continue;
        };
        let Some(object) = solution.get("o") else {
            continue;
        };
        let graph = match solution.get("g") {
            Some(Term::NamedNode(n)) => GraphName::NamedNode(n.clone()),
            Some(Term::BlankNode(b)) => GraphName::BlankNode(b.clone()),
            _ => GraphName::DefaultGraph,
        };
        dataset.insert(&Quad::new(
            subject,
            predicate.clone(),
            object.clone(),
            graph,
        ));
    }
    Ok(dataset)
}

/// A solution: its variables and values.
type Row = Vec<(String, Term)>;

/// A query answer in a form two answers can be compared in.
#[derive(Clone)]
enum Answer {
    Boolean(bool),
    Solutions {
        variables: Vec<String>,
        rows: Vec<Row>,
        /// Whether the order of the rows counts.
        ordered: bool,
    },
    Graph(Box<Graph>),
}

impl Answer {
    fn from_results(results: QueryResults<'static>, ordered: bool) -> Result<Self, String> {
        Ok(match results {
            QueryResults::Boolean(b) => Self::Boolean(b),
            QueryResults::Solutions(solutions) => {
                let variables = solutions
                    .variables()
                    .iter()
                    .map(|v| v.as_str().to_owned())
                    .collect();
                let mut rows = Vec::new();
                for solution in solutions {
                    let solution = solution.map_err(|e| format!("query failed: {e}"))?;
                    rows.push(
                        solution
                            .iter()
                            .map(|(v, t)| (v.as_str().to_owned(), t.clone()))
                            .collect(),
                    );
                }
                Self::Solutions {
                    variables,
                    rows,
                    ordered,
                }
            }
            QueryResults::Graph(triples) => {
                let mut graph = Graph::new();
                for triple in triples {
                    graph.insert(&triple.map_err(|e| format!("query failed: {e}"))?);
                }
                Self::Graph(Box::new(graph))
            }
        })
    }

    /// Reads an expected results file. An RDF file is a graph when the query returned one,
    /// and otherwise a result set in the `rs:` vocabulary.
    fn expected(files: &Files, iri: &str, actual: &Answer) -> Result<Self, String> {
        let extension = iri.rsplit('.').next().unwrap_or_default();
        let format = match extension {
            "srx" => Some(QueryResultsFormat::Xml),
            "srj" => Some(QueryResultsFormat::Json),
            "tsv" => Some(QueryResultsFormat::Tsv),
            _ => None,
        };
        let ordered = matches!(actual, Answer::Solutions { ordered: true, .. });
        if let Some(format) = format {
            let mut text = files.read_to_string(iri)?;
            if format == QueryResultsFormat::Xml {
                text = without_cdata(&text);
            }
            let parsed = QueryResultsParser::from_format(format)
                .for_reader(text.as_bytes())
                .map_err(|e| format!("{iri}: {e}"))?;
            return Ok(match parsed {
                ReaderQueryResultsParserOutput::Boolean(b) => Self::Boolean(b),
                ReaderQueryResultsParserOutput::Solutions(solutions) => {
                    let variables = solutions
                        .variables()
                        .iter()
                        .map(|v| v.as_str().to_owned())
                        .collect();
                    let mut rows = Vec::new();
                    for solution in solutions {
                        let solution = solution.map_err(|e| format!("{iri}: {e}"))?;
                        rows.push(
                            solution
                                .iter()
                                .map(|(v, t)| (v.as_str().to_owned(), t.clone()))
                                .collect(),
                        );
                    }
                    Self::Solutions {
                        variables,
                        rows,
                        ordered,
                    }
                }
            });
        }
        let graph = files.graph(iri)?;
        if matches!(actual, Answer::Graph(_)) {
            return Ok(Self::Graph(Box::new(graph)));
        }
        result_set(&graph, ordered).ok_or_else(|| format!("{iri}: no rs:ResultSet in it"))
    }

    /// The answer as a graph, the solutions and bindings as blank nodes, so that blank nodes
    /// in the answer compare up to renaming.
    fn to_graph(&self, lax: bool) -> Graph {
        let rs = |name: &str| NamedNode::new_unchecked(format!("{RS}{name}"));
        let mut graph = Graph::new();
        match self {
            Answer::Graph(g) => {
                for t in g.iter() {
                    graph.insert(&Triple::new(
                        t.subject.into_owned(),
                        t.predicate.into_owned(),
                        by_value(t.object.into_owned()),
                    ));
                }
            }
            Answer::Boolean(b) => {
                graph.insert(&Triple::new(
                    BlankNode::default(),
                    rs("boolean"),
                    Literal::from(*b),
                ));
            }
            Answer::Solutions {
                variables, ordered, ..
            } => {
                for v in variables {
                    graph.insert(&Triple::new(
                        rs("set"),
                        rs("resultVariable"),
                        Literal::new_simple_literal(v),
                    ));
                }
                for (i, row) in self.rows(lax).into_iter().enumerate() {
                    // A row is a blank node with one triple per binding, the variable as
                    // predicate: few blank nodes keep the canonicalization fast.
                    let solution = BlankNode::default();
                    graph.insert(&Triple::new(rs("set"), rs("solution"), solution.clone()));
                    if *ordered {
                        graph.insert(&Triple::new(
                            solution.clone(),
                            rs("index"),
                            Literal::new_typed_literal((i + 1).to_string(), xsd::INTEGER),
                        ));
                    }
                    for (variable, value) in row {
                        graph.insert(&Triple::new(
                            solution.clone(),
                            NamedNode::new_unchecked(format!("urn:variable:{variable}")),
                            by_value(value.clone()),
                        ));
                    }
                }
            }
        }
        graph
    }

    /// The rows, without duplicates when the cardinality is lax.
    fn rows(&self, lax: bool) -> Vec<&Row> {
        let Answer::Solutions { rows, .. } = self else {
            return Vec::new();
        };
        let mut rows: Vec<&Row> = rows.iter().collect();
        if lax {
            let mut seen = BTreeSet::new();
            rows.retain(|r| seen.insert(row_key(r)));
        }
        rows
    }

    fn has_blank_nodes(&self) -> bool {
        let blank = |t: &Term| match t {
            Term::BlankNode(_) => true,
            Term::Triple(_) => t.to_string().contains("_:"),
            _ => false,
        };
        match self {
            Answer::Boolean(_) => false,
            Answer::Graph(g) => g
                .iter()
                .any(|t| t.subject.is_blank_node() || blank(&t.object.into_owned())),
            Answer::Solutions { rows, .. } => rows.iter().flatten().any(|(_, t)| blank(t)),
        }
    }

    /// The answer as comparable text, when it has no blank nodes (canonicalization is not
    /// needed then, and can take long on many similar rows).
    fn plain(&self, lax: bool) -> Option<Vec<String>> {
        if self.has_blank_nodes() {
            return None;
        }
        Some(match self {
            Answer::Boolean(b) => vec![b.to_string()],
            Answer::Graph(g) => {
                let mut lines: Vec<String> = g
                    .iter()
                    .map(|t| {
                        Triple::new(
                            t.subject.into_owned(),
                            t.predicate.into_owned(),
                            by_value(t.object.into_owned()),
                        )
                        .to_string()
                    })
                    .collect();
                lines.sort();
                lines
            }
            Answer::Solutions {
                variables, ordered, ..
            } => {
                let mut variables = variables.clone();
                variables.sort();
                let mut lines: Vec<String> =
                    self.rows(lax).into_iter().map(|r| row_key(r)).collect();
                if !ordered {
                    lines.sort();
                }
                lines.insert(0, format!("variables: {}", variables.join(" ")));
                lines
            }
        })
    }

    /// One line per row (or triple), blank node labels left out, for failure messages.
    fn lines(&self) -> (usize, BTreeSet<String>) {
        match self {
            Answer::Boolean(b) => (1, BTreeSet::from([b.to_string()])),
            Answer::Graph(g) => (
                g.len(),
                g.iter().map(|t| blank_neutral(&t.to_string())).collect(),
            ),
            Answer::Solutions {
                variables, rows, ..
            } => {
                let mut lines: BTreeSet<String> = rows.iter().map(|r| row_text(r)).collect();
                let mut variables = variables.clone();
                variables.sort();
                lines.insert(format!("variables: {}", variables.join(" ")));
                (rows.len(), lines)
            }
        }
    }
}

/// A row as text for comparing: numbers by value, blank node labels left out, cells sorted.
fn row_key(row: &[(String, Term)]) -> String {
    let mut cells: Vec<String> = row
        .iter()
        .map(|(v, t)| blank_neutral(&format!("?{v}={}", by_value(t.clone()))))
        .collect();
    cells.sort();
    cells.join(" ")
}

/// A row as text for failure messages: the values as they are.
fn row_text(row: &[(String, Term)]) -> String {
    let mut cells: Vec<String> = row
        .iter()
        .map(|(v, t)| blank_neutral(&format!("?{v}={t}")))
        .collect();
    cells.sort();
    cells.join(" ")
}

/// Numeric literals in their canonical form, so that results compare by value: `"1.0"` and
/// `"1"` are the same `xsd:decimal`. The expected results of the suites come from several
/// implementations, which write numbers differently; Jena's and Oxigraph's runners also
/// compare them by value.
fn by_value(term: Term) -> Term {
    use std::str::FromStr;
    let Term::Literal(literal) = &term else {
        return term;
    };
    let value = literal.value();
    let datatype = literal.datatype();
    let canonical = if datatype == xsd::INTEGER {
        oxsdatatypes::Integer::from_str(value)
            .ok()
            .map(|v| v.to_string())
    } else if datatype == xsd::DECIMAL {
        oxsdatatypes::Decimal::from_str(value)
            .ok()
            .map(|v| v.to_string())
    } else if datatype == xsd::DOUBLE {
        oxsdatatypes::Double::from_str(value)
            .ok()
            .map(|v| v.to_string())
    } else if datatype == xsd::FLOAT {
        oxsdatatypes::Float::from_str(value)
            .ok()
            .map(|v| v.to_string())
    } else if datatype.as_str() == WKT_LITERAL {
        // Geometry literals as the GeoSPARQL benchmark compares them: WKT without white
        // space and case, GML, KML and GeoJSON without white space between elements.
        Some(value.split_whitespace().collect::<String>().to_lowercase())
    } else if GEOMETRY_MARKUP.contains(&datatype.as_str()) {
        Some(without_layout(value))
    } else {
        None
    };
    match canonical {
        Some(canonical) => Literal::new_typed_literal(canonical, datatype.into_owned()).into(),
        None => term,
    }
}

/// XML with its CDATA sections written as escaped text. The GeoSPARQL benchmark's answers
/// put GML literals in CDATA, which the results parser refuses.
fn without_cdata(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<![CDATA[") {
        out.push_str(&rest[..start]);
        let body = &rest[start + 9..];
        let end = body.find("]]>").unwrap_or(body.len());
        for c in body[..end].chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                c => out.push(c),
            }
        }
        rest = body.get(end + 3..).unwrap_or_default();
    }
    out.push_str(rest);
    out
}

const WKT_LITERAL: &str = "http://www.opengis.net/ont/geosparql#wktLiteral";

const GEOMETRY_MARKUP: [&str; 3] = [
    "http://www.opengis.net/ont/geosparql#gmlLiteral",
    "http://www.opengis.net/ont/geosparql#kmlLiteral",
    "http://www.opengis.net/ont/geosparql#geoJSONLiteral",
];

/// XML or JSON without the white space between its elements or tokens (outside strings and
/// text).
fn without_layout(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut pending_space = false;
    for c in text.trim().chars() {
        if c == '"' {
            in_string = !in_string;
        }
        if !in_string && c.is_whitespace() {
            pending_space = true;
            continue;
        }
        // Keep one space between two word characters (`<gml:Point srsName=…>`), drop it
        // around punctuation (`> <`, `, `, `: `).
        if pending_space
            && out
                .chars()
                .last()
                .is_some_and(|p| p.is_alphanumeric() || p == '"')
            && (c.is_alphanumeric() || c == '-')
        {
            out.push(' ');
        }
        pending_space = false;
        out.push(c);
    }
    out
}

/// Replaces blank node labels by `_:b`.
pub(crate) fn blank_neutral(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if c == '"' {
            in_string = !in_string;
        }
        if !in_string && c == '_' && chars.peek() == Some(&':') {
            chars.next();
            while chars
                .peek()
                .is_some_and(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
            {
                chars.next();
            }
            out.push_str("_:b");
            continue;
        }
        out.push(c);
    }
    out
}

/// A result set in the `rs:` vocabulary.
fn result_set(graph: &Graph, ordered: bool) -> Option<Answer> {
    let rs = |name: &str| NamedNode::new_unchecked(format!("{RS}{name}"));
    let set_type = rs("ResultSet");
    let set = graph
        .subjects_for_predicate_object(
            tripletrail::oxigraph::model::vocab::rdf::TYPE,
            set_type.as_ref(),
        )
        .next()?;
    if let Some(TermRef::Literal(b)) = graph
        .objects_for_subject_predicate(set, rs("boolean").as_ref())
        .next()
    {
        return Some(Answer::Boolean(b.value() == "true"));
    }
    let variables = graph
        .objects_for_subject_predicate(set, rs("resultVariable").as_ref())
        .filter_map(|v| match v {
            TermRef::Literal(l) => Some(l.value().to_owned()),
            _ => None,
        })
        .collect();
    let mut rows: Vec<(Option<u64>, Row)> = Vec::new();
    let mut indexed = false;
    for solution in graph.objects_for_subject_predicate(set, rs("solution").as_ref()) {
        let solution: NamedOrBlankNodeRef<'_> = match solution {
            TermRef::BlankNode(b) => b.into(),
            TermRef::NamedNode(n) => n.into(),
            _ => continue,
        };
        let index = graph
            .objects_for_subject_predicate(solution, rs("index").as_ref())
            .find_map(|i| match i {
                TermRef::Literal(l) => l.value().parse::<u64>().ok(),
                _ => None,
            });
        indexed |= index.is_some();
        let mut row = Vec::new();
        for binding in graph.objects_for_subject_predicate(solution, rs("binding").as_ref()) {
            let binding: NamedOrBlankNodeRef<'_> = match binding {
                TermRef::BlankNode(b) => b.into(),
                TermRef::NamedNode(n) => n.into(),
                _ => continue,
            };
            let variable = graph
                .objects_for_subject_predicate(binding, rs("variable").as_ref())
                .find_map(|v| match v {
                    TermRef::Literal(l) => Some(l.value().to_owned()),
                    _ => None,
                });
            let value = graph
                .objects_for_subject_predicate(binding, rs("value").as_ref())
                .next();
            if let (Some(variable), Some(value)) = (variable, value) {
                row.push((variable, value.into_owned()));
            }
        }
        rows.push((index, row));
    }
    rows.sort_by_key(|(i, _)| *i);
    Some(Answer::Solutions {
        variables,
        rows: rows.into_iter().map(|(_, r)| r).collect(),
        // Only an indexed result set gives an order to check.
        ordered: ordered && indexed,
    })
}

fn compare(expected: Answer, mut actual: Answer, lax: bool) -> Result<(), String> {
    // An unordered expectation makes the comparison unordered on both sides.
    if let (Answer::Solutions { ordered: false, .. }, Answer::Solutions { ordered, .. }) =
        (&expected, &mut actual)
    {
        *ordered = false;
    }
    let same = match (expected.plain(lax), actual.plain(lax)) {
        (Some(e), Some(a)) => e == a,
        _ => {
            let (mut e, mut a) = (expected.to_graph(lax), actual.to_graph(lax));
            e.canonicalize(CanonicalizationAlgorithm::Unstable);
            a.canonicalize(CanonicalizationAlgorithm::Unstable);
            e == a
        }
    };
    if same {
        return Ok(());
    }
    let (expected_len, expected_lines) = expected.lines();
    let (actual_len, actual_lines) = actual.lines();
    let what = match expected {
        Answer::Boolean(_) => "boolean",
        Answer::Graph(_) => "triples",
        Answer::Solutions { .. } => "rows",
    };
    if expected_lines == actual_lines && expected_len == actual_len {
        return Err(format!(
            "same {what}, but in another order or with other blank nodes"
        ));
    }
    Err(difference(
        what,
        expected_len,
        actual_len,
        &expected_lines,
        &actual_lines,
    ))
}

pub(crate) fn difference(
    what: &str,
    expected_len: usize,
    actual_len: usize,
    expected: &BTreeSet<String>,
    actual: &BTreeSet<String>,
) -> String {
    let mut message = format!("expected {expected_len} {what}, got {actual_len}");
    let missing: Vec<&String> = expected.difference(actual).take(3).collect();
    let extra: Vec<&String> = actual.difference(expected).take(3).collect();
    if !missing.is_empty() {
        message.push_str(&format!("; missing {}", quote(&missing)));
    }
    if !extra.is_empty() {
        message.push_str(&format!("; unexpected {}", quote(&extra)));
    }
    message
}

fn quote(lines: &[&String]) -> String {
    lines
        .iter()
        .map(|l| format!("`{l}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// CSV is lossy: compare the text, rows in any order, blank node labels ignored.
fn csv(files: &Files, results: QueryResults<'static>, expected: &str) -> Result<(), String> {
    let QueryResults::Solutions(solutions) = results else {
        return Err("CSV results need a SELECT query".into());
    };
    let variables = solutions.variables().to_vec();
    let mut serializer = QueryResultsSerializer::from_format(QueryResultsFormat::Csv)
        .serialize_solutions_to_writer(Vec::new(), variables)
        .map_err(|e| e.to_string())?;
    for solution in solutions {
        let solution = solution.map_err(|e| format!("query failed: {e}"))?;
        serializer
            .serialize(solution.iter())
            .map_err(|e| e.to_string())?;
    }
    let actual = String::from_utf8(serializer.finish().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let normalize = |text: &str| -> (Option<String>, Vec<String>) {
        let mut lines = text
            .split('\n')
            .map(|l| blank_neutral(l.trim_end_matches('\r')))
            .filter(|l| !l.is_empty());
        let header = lines.next();
        let mut rows: Vec<String> = lines.collect();
        rows.sort();
        (header, rows)
    };
    let expected_text = files.read_to_string(expected)?;
    let (eh, er) = normalize(&expected_text);
    let (ah, ar) = normalize(&actual);
    if eh == ah && er == ar {
        return Ok(());
    }
    let set = |rows: &[String], header: &Option<String>| -> BTreeSet<String> {
        let mut s: BTreeSet<String> = rows.iter().cloned().collect();
        s.extend(header.iter().map(|h| format!("header: {h}")));
        s
    };
    Err(difference(
        "CSV rows",
        er.len(),
        ar.len(),
        &set(&er, &eh),
        &set(&ar, &ah),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_compare_by_value() {
        let decimal = |v: &str| Term::from(Literal::new_typed_literal(v, xsd::DECIMAL));
        assert_eq!(by_value(decimal("1.0")), by_value(decimal("1")));
        let double = |v: &str| Term::from(Literal::new_typed_literal(v, xsd::DOUBLE));
        assert_eq!(by_value(double("3.0E4")), by_value(double("30000")));
        // Ill-formed numbers and other datatypes stay as they are.
        assert_eq!(by_value(decimal("abc")), decimal("abc"));
        let string = Term::from(Literal::new_simple_literal("01"));
        assert_eq!(by_value(string.clone()), string);
    }

    #[test]
    fn blank_node_labels_are_left_out_outside_strings() {
        assert_eq!(blank_neutral(r#"?x=_:b12 ?y="_:a""#), r#"?x=_:b ?y="_:a""#);
    }

    #[test]
    fn order_by_is_found_across_lines_and_case() {
        assert!(has_order_by("SELECT * { ?s ?p ?o }\norder\n  by ?s"));
        assert!(!has_order_by("SELECT ?order { ?s ?by ?o }"));
        assert!(!has_order_by(
            "SELECT * { { SELECT ?v { ?s ?p ?v } ORDER BY ?v LIMIT 1 } }"
        ));
        assert!(has_order_by(
            "SELECT * { { SELECT ?v { ?s <http://e/#p> ?v } } }\nORDER BY ?v"
        ));
    }

    #[test]
    fn unordered_rows_compare_as_multisets() {
        let row = |v: &str| vec![("x".to_owned(), Term::from(Literal::new_simple_literal(v)))];
        let answer = |rows: Vec<Row>, ordered| Answer::Solutions {
            variables: vec!["x".into()],
            rows,
            ordered,
        };
        let a = answer(vec![row("a"), row("b")], false);
        let b = answer(vec![row("b"), row("a")], false);
        assert!(compare(a, b, false).is_ok());
        let a = answer(vec![row("a"), row("b")], true);
        let b = answer(vec![row("b"), row("a")], true);
        assert!(compare(a, b, false).is_err());
        let a = answer(vec![row("a"), row("a")], false);
        let b = answer(vec![row("a")], false);
        assert!(compare(a, b, false).is_err());
        let a = answer(vec![row("a"), row("a")], false);
        let b = answer(vec![row("a")], false);
        assert!(compare(a, b, true).is_ok());
    }

    #[test]
    fn blank_nodes_compare_up_to_renaming() {
        let row = |b: &str| vec![("x".to_owned(), Term::from(BlankNode::new_unchecked(b)))];
        let answer = |rows| Answer::Solutions {
            variables: vec!["x".into()],
            rows,
            ordered: false,
        };
        assert!(
            compare(
                answer(vec![row("a"), row("b")]),
                answer(vec![row("c"), row("d")]),
                false
            )
            .is_ok()
        );
        // One blank node twice is not two blank nodes.
        assert!(
            compare(
                answer(vec![row("a"), row("b")]),
                answer(vec![row("c"), row("c")]),
                false
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod geometry_tests {
    use super::*;

    #[test]
    fn cdata_becomes_text() {
        assert_eq!(
            without_cdata("<literal><![CDATA[<gml:Point>1 2</gml:Point>]]></literal>"),
            "<literal>&lt;gml:Point&gt;1 2&lt;/gml:Point&gt;</literal>"
        );
        assert_eq!(without_cdata("no cdata"), "no cdata");
    }

    #[test]
    fn geometry_layout() {
        assert_eq!(
            without_layout(
                "<gml:Point srsName=\"x\">\n  <gml:pos>-83.4 34.2</gml:pos>\n</gml:Point>"
            ),
            "<gml:Point srsName=\"x\"><gml:pos>-83.4 34.2</gml:pos></gml:Point>"
        );
        assert_eq!(
            without_layout("{ \"type\": \"Point\", \"coordinates\": [ -83.4, 34.2 ] }"),
            "{\"type\":\"Point\",\"coordinates\":[-83.4,34.2]}"
        );
    }
}
