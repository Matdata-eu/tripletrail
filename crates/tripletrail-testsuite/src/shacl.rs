//! Runs a W3C SHACL test (`sht:Validate`) through `Engine::validate`, on a fresh engine with
//! the data and shapes graphs loaded into the store.

use crate::eval::{blank_neutral, difference, dump};
use crate::manifest::{Files, Test, closure};
use std::collections::BTreeSet;
use tripletrail::Engine;
use tripletrail::oxigraph::model::dataset::CanonicalizationAlgorithm;
use tripletrail::oxigraph::model::vocab::rdf;
use tripletrail::oxigraph::model::{
    Graph, GraphNameRef, NamedNode, NamedOrBlankNode, NamedOrBlankNodeRef, TripleRef,
};
use tripletrail::oxigraph::sparql::CancellationToken;

const SH: &str = "http://www.w3.org/ns/shacl#";

/// Validates the test's data graph against its shapes graph and compares the report with
/// the expected one, as the SHACL test suite says: the reports must be isomorphic, with the
/// blank nodes they reach (paths, blank node shapes), leaving out `sh:resultMessage` and
/// `sh:detail`.
pub fn validate(files: &Files, test: &Test, token: CancellationToken) -> Result<(), String> {
    let shapes_file = test
        .action
        .as_deref()
        .ok_or("the test has no shapes graph")?;
    let data_file = test.data.first().ok_or("the test has no data graph")?;
    let engine = Engine::new()
        .map_err(|e| e.to_string())?
        .with_cancellation_token(token);
    let load = |file: &str| -> Result<NamedNode, String> {
        let name = NamedNode::new(file).map_err(|e| format!("{file}: {e}"))?;
        engine
            .load_into(name.clone(), files.rdf_parser(file)?, files.open(file)?)
            .map_err(|e| format!("loading {file}: {e}"))?;
        Ok(name)
    };
    let data = load(data_file)?;
    let shapes = if shapes_file == data_file {
        data.clone()
    } else {
        load(shapes_file)?
    };
    // The expected reports have no `sh:detail` (the suite leaves them out), so neither do the
    // results compared: the results they lead to are then not reached from the report.
    let detail = NamedNode::new_unchecked(format!("{SH}detail"));
    let mut report = Graph::new();
    let validated = engine.validate(&shapes, GraphNameRef::NamedNode(data.as_ref()), &mut |t| {
        if t.predicate != detail {
            report.insert(&t);
        }
    });
    let expected = match (&test.report, validated) {
        // The failure must be the shapes graph's (a query the spec refuses, say), not one of
        // loading or evaluation.
        (None, Err(e)) => {
            let message = e.to_string();
            return if message.starts_with("shacl: ") && !message.starts_with("shacl: query failed")
            {
                Ok(())
            } else {
                Err(format!("failed, but not on the shapes graph: {message}"))
            };
        }
        (None, Ok(_)) => return Err("validated, but the test expects a failure".into()),
        (Some(_), Err(e)) => return Err(format!("failed: {e}")),
        (Some(expected), Ok(_)) => expected,
    };
    // The blank nodes the report reaches, described in the store (shapes, paths, data).
    let mut everything = report.clone();
    for quad in dump(&engine)?.iter() {
        everything.insert(TripleRef::new(quad.subject, quad.predicate, quad.object));
    }
    let report_type = NamedNode::new_unchecked(format!("{SH}ValidationReport"));
    let mut actual = Graph::new();
    let roots: Vec<NamedOrBlankNode> = report
        .subjects_for_predicate_object(rdf::TYPE, &report_type)
        .map(NamedOrBlankNodeRef::into_owned)
        .collect();
    for root in &roots {
        closure(&everything, root.as_ref(), &mut actual);
    }
    same_report(expected, &actual)
}

fn same_report(expected: &Graph, actual: &Graph) -> Result<(), String> {
    let message = NamedNode::new_unchecked(format!("{SH}resultMessage"));
    let strip = |g: &Graph| -> Graph {
        g.iter()
            .filter(|t| t.predicate != message.as_ref())
            .collect()
    };
    let (mut e, mut a) = (strip(expected), strip(actual));
    let (e_len, a_len) = (e.len(), a.len());
    e.canonicalize(CanonicalizationAlgorithm::Unstable);
    a.canonicalize(CanonicalizationAlgorithm::Unstable);
    if e == a {
        return Ok(());
    }
    let lines = |g: &Graph| -> BTreeSet<String> {
        g.iter().map(|t| blank_neutral(&t.to_string())).collect()
    };
    Err(difference(
        "report triples",
        e_len,
        a_len,
        &lines(&strip(expected)),
        &lines(&strip(actual)),
    ))
}
