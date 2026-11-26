//! `POST /{ds}/mcp` (#153): a Model Context Protocol endpoint, so that AI agents can query,
//! describe and search a dataset. Streamable HTTP without a stream: each JSON-RPC request
//! posted gets one JSON answer. Off unless `serve --mcp`.
//!
//! Every tool reads the dataset as the caller's own SPARQL queries do: with its graph grants
//! and row rules (the request has passed `check_access` like a query), its time budget, and at
//! most its `max-rows` (and [`MAX_ROWS`]) rows, since agents send many unplanned queries.

use crate::admin::Entry;
use crate::{Context, HttpError, Reply, State, View, may_list, with_deadline};
use serde_json::{Value, json};
use std::time::Duration;
use tripletrail::Engine;
use tripletrail::geo::sync::RwLockUnpoisoned;
use tripletrail::oxigraph::io::{RdfFormat, RdfSerializer};
use tripletrail::oxigraph::model::vocab::rdf;
use tripletrail::oxigraph::model::{Graph, NamedNode, NamedOrBlankNodeRef, TermRef};
use tripletrail::oxigraph::sparql::QueryResults;
use tripletrail::oxigraph::sparql::results::{QueryResultsFormat, QueryResultsSerializer};

/// The protocol versions this endpoint speaks, newest first.
const VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// The most rows a tool answers, below the server's own `--max-rows`.
pub(crate) const MAX_ROWS: usize = 1000;

const SH: &str = "http://www.w3.org/ns/shacl#";
const PROPERTY: &str = "http://www.w3.org/ns/shacl#property";

/// What a tool call reads: the dataset, as the caller sees it.
pub(crate) struct Call<'a> {
    pub(crate) state: &'a State,
    pub(crate) context: &'a Context,
    pub(crate) entry: &'a Entry,
    pub(crate) engine: &'a Engine,
    /// The server's origin (`https://host`), for the endpoints of other datasets.
    pub(crate) origin: String,
    /// The last query a tool ran: recorded as the request's query, in the slow-query log, the
    /// request log, the audit record and traces, as a query to `/{ds}/sparql` is.
    pub(crate) ran: std::cell::RefCell<Option<String>>,
}

/// Answers one JSON-RPC message posted to the endpoint.
pub(crate) fn answer(call: &Call<'_>, body: &[u8]) -> Result<Reply, HttpError> {
    let message: Value = match serde_json::from_slice(body) {
        Ok(m) => m,
        Err(e) => return Ok(rpc_error(Value::Null, -32700, &format!("Parse error: {e}"))),
    };
    if message.is_array() {
        return Ok(rpc_error(
            Value::Null,
            -32600,
            "Batches are not supported: post one message at a time",
        ));
    }
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        // A response or something else from the client: nothing to answer.
        return Ok(Reply::empty(202));
    };
    let Some(id) = message.get("id").cloned() else {
        // A notification (`notifications/initialized`...): accepted, never answered.
        return Ok(Reply::empty(202));
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => Ok(initialize(call, &params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => tool_call(call, &params),
        _ => Err((-32601, format!("Method not found: {method}"))),
    };
    Ok(match result {
        Ok(result) => rpc(json!({ "jsonrpc": "2.0", "id": id, "result": result })),
        Err((code, text)) => rpc_error(id, code, &text),
    })
}

fn rpc(value: Value) -> Reply {
    Reply::bytes(200, "application/json", value.to_string().into_bytes())
}

fn rpc_error(id: Value, code: i64, message: &str) -> Reply {
    rpc(json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    }))
}

fn initialize(call: &Call<'_>, params: &Value) -> Value {
    // The client's version when this endpoint speaks it, else the newest.
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = VERSIONS
        .iter()
        .find(|v| Some(**v) == asked)
        .unwrap_or(&VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "TripleTrail", "version": env!("CARGO_PKG_VERSION") },
        "instructions": format!(
            "The RDF dataset {}. Read it with SPARQL (sparql_query); see what its data looks \
             like with list_shapes and shape; find resources by their labels with search; \
             describe one resource with describe. Answers hold at most {MAX_ROWS} rows.",
            call.entry.name
        ),
    })
}

fn tools() -> Value {
    let object = |properties: Value, required: &[&str]| json!({ "type": "object", "properties": properties, "required": required });
    json!([
        {
            "name": "sparql_query",
            "description": "Runs a read-only SPARQL 1.1 query (SELECT, ASK, CONSTRUCT or \
                DESCRIBE) on the dataset. SELECT and ASK answer SPARQL JSON results, \
                CONSTRUCT and DESCRIBE Turtle. At most max_rows rows.",
            "inputSchema": object(json!({
                "query": { "type": "string", "description": "The SPARQL query" },
                "max_rows": {
                    "type": "integer", "minimum": 1, "maximum": MAX_ROWS,
                    "description": format!("The most rows to answer ({MAX_ROWS} by default)"),
                },
            }), &["query"]),
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "describe",
            "description": "The triples about one resource (SPARQL DESCRIBE), as Turtle.",
            "inputSchema": object(json!({
                "iri": { "type": "string", "description": "The resource's IRI" },
            }), &["iri"]),
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "search",
            "description": "Full-text search of the dataset's labels, titles and \
                descriptions: the best matching resources, with the literal that matched \
                and a BM25 score, as SPARQL JSON results. word* matches words that start \
                with word.",
            "inputSchema": object(json!({
                "text": { "type": "string", "description": "The words to look for" },
                "limit": {
                    "type": "integer", "minimum": 1, "maximum": MAX_ROWS,
                    "description": "The most matches (20 by default)",
                },
            }), &["text"]),
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "list_shapes",
            "description": "The SHACL node shapes of the dataset: what its data looks like. \
                Each with its IRI, label, target classes and property paths, as JSON.",
            "inputSchema": object(json!({}), &[]),
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "shape",
            "description": "One SHACL node shape of the dataset with its property shapes, as \
                Turtle.",
            "inputSchema": object(json!({
                "iri": { "type": "string", "description": "The shape's IRI, from list_shapes" },
            }), &["iri"]),
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "list_datasets",
            "description": "The datasets of this server the caller may read, with the URL of \
                each one's MCP endpoint.",
            "inputSchema": object(json!({}), &[]),
            "annotations": { "readOnlyHint": true },
        },
    ])
}

/// `tools/call`: the tool's answer, or its error as a result with `isError` (which the agent
/// sees), not as a protocol error.
fn tool_call(call: &Call<'_>, params: &Value) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((-32602, "Missing the tool's name".to_owned()))?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let text = |key: &str| -> Result<String, String> {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("Missing the argument {key}"))
    };
    let count = |key: &str, default: usize| -> Result<usize, String> {
        match args.get(key) {
            None | Some(Value::Null) => Ok(default),
            Some(v) => v
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .filter(|n| (1..=MAX_ROWS).contains(n))
                .ok_or_else(|| format!("{key} is a number from 1 to {MAX_ROWS}")),
        }
    };
    let outcome = match name {
        "sparql_query" => text("query")
            .and_then(|q| Ok((q, count("max_rows", MAX_ROWS)?)))
            .and_then(|(q, max)| query(call, &q, max)),
        "describe" => text("iri").and_then(|iri| {
            let iri = NamedNode::new(&iri).map_err(|e| format!("Invalid IRI {iri}: {e}"))?;
            query(call, &format!("DESCRIBE {iri}"), MAX_ROWS)
        }),
        "search" => text("text").and_then(|t| {
            let limit = count("limit", 20)?;
            let q = format!(
                "PREFIX tt: <http://w3id.org/tripletrail/fn#>\n\
                 SELECT ?resource ?score ?literal WHERE {{\n  \
                 ?resource tt:search ({} ?score ?literal {limit})\n\
                 }} ORDER BY DESC(?score)",
                string_literal(&t)
            );
            query(call, &q, limit)
        }),
        "list_shapes" => shapes(call).map(|g| list_shapes(&g).to_string()),
        "shape" => text("iri").and_then(|iri| {
            let iri = NamedNode::new(&iri).map_err(|e| format!("Invalid IRI {iri}: {e}"))?;
            shape(&shapes(call)?, &iri)
        }),
        "list_datasets" => Ok(list_datasets(call).to_string()),
        _ => return Err((-32602, format!("Unknown tool {name}"))),
    };
    Ok(match outcome {
        Ok(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
        Err(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": true }),
    })
}

/// `text` as a SPARQL string literal.
fn string_literal(text: &str) -> String {
    tripletrail::oxigraph::model::Literal::new_simple_literal(text).to_string()
}

/// Runs a read-only query as the caller: SPARQL JSON results for rows and booleans, Turtle
/// for graphs; at most `max` rows (and the server's `--max-rows`), saying so when cut.
fn query(call: &Call<'_>, text: &str, max: usize) -> Result<String, String> {
    // A query only: an update is refused here, before the engine sees it.
    spargebra::SparqlParser::new()
        .parse_query(text)
        .map_err(|e| format!("Not a SPARQL query (updates are not allowed): {e}"))?;
    *call.ran.borrow_mut() = Some(text.to_owned());
    let max = call.context.limits.max_rows.map_or(max, |m| m.min(max));
    let timeout: Option<Duration> = call.context.limits.timeout(None);
    let (engine, _deadline) = with_deadline(call.engine, timeout);
    let results = engine
        .query_with_options(text, View::of(call.context).options())
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut cut = false;
    match results {
        QueryResults::Boolean(value) => {
            QueryResultsSerializer::from_format(QueryResultsFormat::Json)
                .serialize_boolean_to_writer(&mut out, value)
                .map_err(|e| e.to_string())?;
        }
        QueryResults::Solutions(solutions) => {
            let variables = solutions.variables().to_vec();
            let mut writer = QueryResultsSerializer::from_format(QueryResultsFormat::Json)
                .serialize_solutions_to_writer(&mut out, variables)
                .map_err(|e| e.to_string())?;
            for (i, row) in solutions.enumerate() {
                if i == max {
                    cut = true;
                    break;
                }
                let row = row.map_err(|e| e.to_string())?;
                writer.serialize(&row).map_err(|e| e.to_string())?;
            }
            writer.finish().map_err(|e| e.to_string())?;
        }
        QueryResults::Graph(triples) => {
            let mut writer = RdfSerializer::from_format(RdfFormat::Turtle).for_writer(&mut out);
            for (i, triple) in triples.enumerate() {
                if i == max {
                    cut = true;
                    break;
                }
                let triple = triple.map_err(|e| e.to_string())?;
                writer
                    .serialize_triple(&triple)
                    .map_err(|e| e.to_string())?;
            }
            writer.finish().map_err(|e| e.to_string())?;
        }
    }
    let mut text = String::from_utf8(out).map_err(|e| e.to_string())?;
    if cut {
        text.push_str(&format!(
            "\n(Only the first {max} rows: add a LIMIT, or narrow the query.)"
        ));
    }
    Ok(text)
}

/// The dataset's shapes, as its describe pages read them, under the caller's view.
fn shapes(call: &Call<'_>) -> Result<Graph, String> {
    crate::shapes::projected(
        call.state,
        call.entry,
        tripletrail::shapes::Reader::Describe,
        &View::of(call.context),
    )
    .map_err(|e| e.1)
}

fn sh(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{SH}{local}"))
}

/// The node shapes of `graph`, each with its label, target classes and property paths.
fn list_shapes(graph: &Graph) -> Value {
    let node_shape = sh("NodeShape");
    let iris = |s: NamedOrBlankNodeRef<'_>, p: &NamedNode| -> Vec<String> {
        graph
            .objects_for_subject_predicate(s, p)
            .filter_map(|o| match o {
                TermRef::NamedNode(n) => Some(n.as_str().to_owned()),
                _ => None,
            })
            .collect()
    };
    let text = |s: NamedOrBlankNodeRef<'_>, p: &NamedNode| -> Option<String> {
        graph
            .objects_for_subject_predicate(s, p)
            .find_map(|o| match o {
                TermRef::Literal(l) => Some(l.value().to_owned()),
                _ => None,
            })
    };
    let label = NamedNode::new_unchecked("http://www.w3.org/2000/01/rdf-schema#label");
    let mut shapes: Vec<Value> = graph
        .subjects_for_predicate_object(rdf::TYPE, &node_shape)
        .filter_map(|s| match s {
            NamedOrBlankNodeRef::NamedNode(n) => Some(n),
            NamedOrBlankNodeRef::BlankNode(_) => None,
        })
        .map(|n| {
            let s = NamedOrBlankNodeRef::from(n);
            let mut paths: Vec<String> = graph
                .objects_for_subject_predicate(s, &sh("property"))
                .filter_map(|p| match p {
                    TermRef::NamedNode(p) => Some(NamedOrBlankNodeRef::from(p)),
                    TermRef::BlankNode(p) => Some(NamedOrBlankNodeRef::from(p)),
                    _ => None,
                })
                .flat_map(|p| iris(p, &sh("path")))
                .collect();
            paths.sort();
            paths.dedup();
            json!({
                "iri": n.as_str(),
                "label": text(s, &label).or_else(|| text(s, &sh("name"))),
                "targetClass": iris(s, &sh("targetClass")),
                "paths": paths,
            })
        })
        .collect();
    shapes.sort_by(|a, b| a["iri"].as_str().cmp(&b["iri"].as_str()));
    Value::Array(shapes)
}

/// The triples of the shape `iri`, of its property shapes and of the blank nodes it reaches
/// (lists), as Turtle.
fn shape(graph: &Graph, iri: &NamedNode) -> Result<String, String> {
    let start = NamedOrBlankNodeRef::from(iri.as_ref());
    if graph.triples_for_subject(start).next().is_none() {
        return Err(format!("No shape {iri}: list_shapes gives the shapes"));
    }
    let mut out = Graph::new();
    let mut todo = vec![start];
    let mut seen = std::collections::HashSet::new();
    while let Some(s) = todo.pop() {
        if !seen.insert(s) {
            continue;
        }
        for t in graph.triples_for_subject(s) {
            match t.object {
                TermRef::BlankNode(b) => todo.push(b.into()),
                // Named property shapes, as profiled shapes have.
                TermRef::NamedNode(n) if t.predicate.as_str() == PROPERTY => todo.push(n.into()),
                _ => {}
            }
            out.insert(t);
        }
    }
    let mut writer = RdfSerializer::from_format(RdfFormat::Turtle)
        .with_prefix("sh", SH)
        .map_err(|e| e.to_string())?
        .for_writer(Vec::new());
    for t in out.iter() {
        writer.serialize_triple(t).map_err(|e| e.to_string())?;
    }
    String::from_utf8(writer.finish().map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

/// The datasets the caller may read (all or some of their graphs), with their endpoints.
fn list_datasets(call: &Call<'_>) -> Value {
    let registry = call.state.registry.read_unpoisoned();
    let datasets: Vec<Value> = registry
        .datasets
        .values()
        .filter(|e| {
            may_list(call.state, call.context.caller.as_ref(), &[e.name.as_str()]).is_some()
        })
        .map(|e| {
            json!({
                "name": e.name,
                "mcp": format!("{}/{}/mcp", call.origin, e.name),
                "sparql": format!("{}/{}/sparql", call.origin, e.name),
            })
        })
        .collect();
    Value::Array(datasets)
}
