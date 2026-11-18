//! The RDF4J REST API (protocol 12) over the datasets (#223), so RDF4J's `HTTPRepository`, and
//! tools written for an RDF4J server or GraphDB, work against TripleTrail with only the URL
//! changed. Each dataset is a repository:
//!
//! - `GET /rdf4j-server/protocol`, `GET /rdf4j-server/repositories`
//! - `/rdf4j-server/repositories/{id}`: queries (`GET`, `POST` form or body)
//! - `.../statements`: read (`GET` with `subj`, `pred`, `obj`, `context`), add (`POST`),
//!   replace (`PUT`), remove (`DELETE`), and SPARQL updates (`POST` with `update`)
//! - `.../contexts`, `.../size`, `.../namespaces[/{prefix}]`
//! - `.../transactions[/{id}]`: begin, then `PUT ?action=ADD|DELETE|UPDATE|QUERY|GET|SIZE|PING`
//!   and `COMMIT` or `ROLLBACK`. The changes of a transaction are kept by the server and applied
//!   at commit as one SPARQL update, so a commit is all or nothing. Reads within a transaction
//!   see the committed data, not its own pending changes.
//!
//! Uploads may also be in RDF4J's binary RDF (`application/x-binary-rdf`, see `binary_rdf`),
//! the client's default. A transaction `DELETE` reads its statements as patterns, as RDF4J's
//! server does: `sesame:wildcard` matches anything, no context means every graph and
//! `rdf4j:nil` the default graph.
//!
//! The same paths answer without the `/rdf4j-server` prefix (`/repositories/{id}`), as on
//! GraphDB, unless a dataset has that name.

use crate::admin::{Entry, Mode, Operation};
use crate::limits::Limits;
use crate::{Context, HttpError, Params, Reply, State};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiny_http::{Method, Request};
use tripletrail::Engine;
use tripletrail::geo::sync::{LockUnpoisoned, RwLockUnpoisoned};
use tripletrail::oxigraph::io::{RdfFormat, RdfParser, RdfSerializer};
use tripletrail::oxigraph::model::{
    BlankNode, GraphName, Literal, NamedNode, NamedOrBlankNode, NamedOrBlankNodeRef, Quad, Term,
    Variable,
};
use tripletrail::oxigraph::sparql::QuerySolution;
use tripletrail::oxigraph::sparql::results::QueryResultsSerializer;

/// The RDF4J REST protocol version served.
const PROTOCOL: &str = "12";
/// Transactions untouched for this long are dropped.
const TRANSACTION_IDLE: Duration = Duration::from_secs(3600);
/// The namespaces a repository starts with.
const DEFAULT_NAMESPACES: [(&str, &str); 10] = [
    ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
    ("owl", "http://www.w3.org/2002/07/owl#"),
    ("xsd", "http://www.w3.org/2001/XMLSchema#"),
    ("sh", "http://www.w3.org/ns/shacl#"),
    ("skos", "http://www.w3.org/2004/02/skos/core#"),
    ("dcterms", "http://purl.org/dc/terms/"),
    ("geo", "http://www.opengis.net/ont/geosparql#"),
    ("geof", "http://www.opengis.net/def/function/geosparql/"),
    ("sf", "http://www.opengis.net/ont/sf#"),
];
/// Where a dataset directory keeps its namespaces.
const NAMESPACES_FILE: &str = "namespaces.json";

/// Namespaces and open transactions.
#[derive(Default)]
pub(crate) struct Rdf4j {
    /// Per dataset name, once read or changed.
    namespaces: Mutex<HashMap<String, BTreeMap<String, String>>>,
    transactions: Mutex<HashMap<String, Transaction>>,
}

struct Transaction {
    dataset: String,
    /// Who began it; only they may use it.
    agent: Option<String>,
    /// SPARQL update operations, applied in order at commit.
    operations: Vec<String>,
    /// Bytes of `operations`, held to the upload limit.
    size: usize,
    touched: Instant,
}

/// Whether the path is the RDF4J API's; `rest` is then the part after `repositories` or
/// `protocol`.
pub(crate) fn matches<'a>(
    segments: &'a [&'a str],
    is_dataset: impl Fn(&str) -> bool,
) -> Option<&'a [&'a str]> {
    match segments {
        ["rdf4j-server", rest @ ..] => Some(rest),
        [first, ..] if matches!(*first, "repositories" | "protocol") && !is_dataset(first) => {
            Some(segments)
        }
        _ => None,
    }
}

/// Answers a request under the RDF4J API (`rest` starts at `repositories` or `protocol`).
pub(crate) fn route(
    state: &Arc<State>,
    request: &mut Request,
    rest: &[&str],
    params: &mut Params,
    context: &mut Context,
) -> Result<Reply, HttpError> {
    let method = request.method().clone();
    let get = matches!(method, Method::Get | Method::Head);
    match rest {
        ["protocol"] if get => Ok(text(PROTOCOL)),
        ["repositories"] if get => repositories(state, request, params, context),
        ["repositories", id, more @ ..] => {
            let id = *id;
            let entry = state
                .registry
                .read_unpoisoned()
                .resolve(id)
                .ok_or_else(|| HttpError(404, format!("Unknown repository: {id}")))?;
            // Under row rules (#202) the reads answer from a view without the hidden rows, and
            // statement writes and transactions go through the checks of updates under row
            // rules.
            let reads = reads_only(&method, more);
            let rows_ok = reads || row_writes(more);
            crate::check_access(state, context, &[id, entry.name.as_str()], true, rows_ok)?;
            // A caller who reads only some graphs (#203) gets the reads, kept to those graphs,
            // and no writes, not even graph by graph; nor does a caller with row rules who may
            // write only some graphs.
            let limited = context.readable.is_some()
                || (context.row_rules.is_some() && context.writable.is_some());
            if limited && !reads {
                let why = if context.row_rules.is_some() {
                    "Row-level rules limit what you see of"
                } else {
                    "You may read only some graphs of"
                };
                return Err(crate::auth::refuse(
                    context.caller.as_ref().expect("limited callers are known"),
                    &format!(
                        "{why} {id}: the RDF4J API answers you queries, statements, \
                         contexts, size and namespaces only"
                    ),
                ));
            }
            if limited {
                context.writable = None;
            }
            crate::use_dataset(state, context, &entry)?;
            repository(state, request, params, context, &entry, more)
        }
        _ => Err(HttpError(
            404,
            format!("No RDF4J endpoint {method} {}", rest.join("/")),
        )),
    }
}

/// The repository requests that only read: the ones a caller who reads only some graphs may
/// make.
fn reads_only(method: &Method, more: &[&str]) -> bool {
    let get = matches!(method, Method::Get | Method::Head);
    match more {
        [] => get || *method == Method::Post,
        ["statements" | "contexts" | "size" | "namespaces"] | ["namespaces", _] => get,
        _ => false,
    }
}

/// The repository requests that write statements: under row rules (#202) they are taken
/// from a caller who may write the whole dataset.
fn row_writes(more: &[&str]) -> bool {
    matches!(
        more,
        ["statements"] | ["transactions"] | ["transactions", _]
    )
}

fn repository(
    state: &Arc<State>,
    request: &mut Request,
    params: &mut Params,
    context: &mut Context,
    entry: &Arc<Entry>,
    more: &[&str],
) -> Result<Reply, HttpError> {
    let limits = context.limits;
    let method = request.method().clone();
    let get = matches!(method, Method::Get | Method::Head);
    let engine = &entry.engine;
    let read_only = entry.mode == Mode::ReadOnly;
    match more {
        [] => {
            if method == Method::Post {
                read_form(state, request, params)?;
            } else if !get {
                return Err(HttpError(
                    405,
                    "Creating and deleting repositories goes through the admin API (/$/datasets)"
                        .to_owned(),
                ));
            }
            context.operation = Some(Operation::Query);
            let query = params.get("query").map(str::to_owned).ok_or_else(|| {
                HttpError::bad_request("MALFORMED QUERY: missing the query parameter")
            })?;
            query_reply(
                state,
                request,
                params,
                entry,
                &query,
                context.readable.clone(),
                context.row_rules.clone(),
            )
        }
        ["statements"] => {
            if get {
                context.operation = Some(Operation::Read);
                let rows = crate::row_view(engine, context)?;
                return statements(engine, request, params, context.readable.clone(), rows);
            }
            if read_only {
                return Err(HttpError(405, "The repository is read-only".to_owned()));
            }
            let form = matches!(
                crate::content_type(request).as_deref(),
                Some("application/x-www-form-urlencoded" | "application/sparql-update")
            );
            if method == Method::Post && (form || params.has("update")) {
                if crate::content_type(request).as_deref() == Some("application/sparql-update") {
                    let body = crate::read_body(request, state.options.max_body)?;
                    params.0.push(("update".to_owned(), body));
                } else {
                    read_form(state, request, params)?;
                }
                context.operation = Some(Operation::Update);
                crate::may_write_as(state, request, context)?;
                let update = params.get("update").ok_or_else(|| {
                    HttpError::bad_request("MALFORMED QUERY: missing the update parameter")
                })?;
                let update = prepare_update(state, entry, params, update)?;
                crate::update_within_quota(context, || {
                    tripletrail::Engine::update_only_removes(&update)
                })?;
                run_update(&limits, params, engine, &update, context.row_rules.as_ref())?;
                return Ok(Reply::empty(204));
            }
            context.operation = Some(if method == Method::Delete {
                Operation::Update
            } else {
                Operation::Upload
            });
            if method == Method::Delete {
                crate::may_write_as(state, request, context)?;
            } else {
                crate::may_write(state, request, context)?;
            }
            match method {
                Method::Post => {
                    // Parsed in full before anything is added.
                    let quads = read_statements(&limits, request, params)?;
                    if let Some(rules) = &context.row_rules {
                        let update = insert_data(&quads);
                        run_update(&limits, params, engine, &update, Some(rules))?;
                    } else {
                        engine.load_quads(quads)?;
                    }
                    Ok(Reply::empty(204))
                }
                Method::Put => {
                    // One update: the data is replaced all at once or not at all.
                    let quads = read_statements(&limits, request, params)?;
                    // Under row rules, only what the caller sees is removed (CLEAR is not taken).
                    let cleared = if context.row_rules.is_some() {
                        pattern_delete([None, None, None], contexts(params)?)?
                    } else {
                        clear(params)?
                    };
                    let update = format!("{cleared} ;\n{}", insert_data(&quads));
                    run_update(&limits, params, engine, &update, context.row_rules.as_ref())?;
                    Ok(Reply::empty(204))
                }
                Method::Delete => {
                    let update = delete_pattern(params)?;
                    run_update(&limits, params, engine, &update, context.row_rules.as_ref())?;
                    Ok(Reply::empty(204))
                }
                other => Err(HttpError(405, format!("{other} is not allowed here"))),
            }
        }
        ["contexts"] if get => {
            context.operation = Some(Operation::Read);
            let mut graphs = engine.store().named_graphs()?;
            if let Some(readable) = &context.readable {
                graphs.retain(|g| readable.contains(g.as_ref().into()));
            }
            // Under row rules a graph with nothing visible in it is absent.
            if let Some(view) = crate::row_view(engine, context)? {
                graphs.retain(|g| {
                    let g = GraphName::from(g.clone());
                    view.quads_for_pattern(None, None, None, Some(g.as_ref()))
                        .next()
                        .is_some()
                });
            }
            let var = Variable::new_unchecked("contextID");
            let rows = graphs
                .into_iter()
                .map(|g| vec![Some(Term::from(g))])
                .collect();
            table(request, params, vec![var], rows)
        }
        ["size"] if get => {
            context.operation = Some(Operation::Read);
            let readable = context.readable.as_ref();
            if let Some(view) = crate::row_view(engine, context)? {
                // Under row rules, only the visible statements count.
                // Graph by graph when `context` names them, as without row rules.
                let n = match contexts(params)? {
                    None => view
                        .quads_for_pattern(None, None, None, None)
                        .filter(|q| readable.is_none_or(|r| r.contains(q.graph_name.as_ref())))
                        .count(),
                    Some(graphs) => graphs
                        .iter()
                        .filter(|g| readable.is_none_or(|r| r.contains(g.as_ref())))
                        .map(|g| {
                            view.quads_for_pattern(None, None, None, Some(g.as_ref()))
                                .count()
                        })
                        .sum(),
                };
                return Ok(text(&n.to_string()));
            }
            let store = engine.store();
            let n = match (contexts(params)?, readable) {
                (None, None) => store.len()?,
                (None, Some(readable)) => {
                    let mut graphs: Vec<GraphName> =
                        readable.named.iter().cloned().map(Into::into).collect();
                    if readable.default_graph {
                        graphs.push(GraphName::DefaultGraph);
                    }
                    let mut n = 0;
                    for g in &graphs {
                        n += store
                            .quads_for_pattern(None, None, None, Some(g.as_ref()))
                            .count();
                    }
                    n
                }
                (Some(mut graphs), _) => {
                    graphs.retain(|g| readable.is_none_or(|r| r.contains(g.as_ref())));
                    let mut n = 0;
                    for g in &graphs {
                        n += store
                            .quads_for_pattern(None, None, None, Some(g.as_ref()))
                            .count();
                    }
                    n
                }
            };
            Ok(text(&n.to_string()))
        }
        ["namespaces"] => {
            context.operation = Some(Operation::Read);
            match method {
                Method::Get | Method::Head => {
                    let ns = namespaces(state, entry);
                    let vars = vec![
                        Variable::new_unchecked("prefix"),
                        Variable::new_unchecked("namespace"),
                    ];
                    let rows = ns
                        .into_iter()
                        .map(|(p, n)| {
                            vec![
                                Some(Literal::new_simple_literal(p).into()),
                                Some(Literal::new_simple_literal(n).into()),
                            ]
                        })
                        .collect();
                    table(request, params, vars, rows)
                }
                Method::Delete => {
                    context.operation = Some(Operation::Update);
                    crate::may_write_as(state, request, context)?;
                    set_namespaces(state, entry, BTreeMap::new())?;
                    Ok(Reply::empty(204))
                }
                other => Err(HttpError(405, format!("{other} is not allowed here"))),
            }
        }
        ["namespaces", prefix] => {
            let prefix = (*prefix).to_owned();
            match method {
                Method::Get | Method::Head => {
                    context.operation = Some(Operation::Read);
                    namespaces(state, entry)
                        .remove(&prefix)
                        .map(|n| text(&n))
                        .ok_or_else(|| HttpError(404, format!("Undefined prefix: {prefix}")))
                }
                Method::Put => {
                    context.operation = Some(Operation::Update);
                    crate::may_write(state, request, context)?;
                    let namespace = crate::read_body(request, state.options.max_body)?
                        .trim()
                        .to_owned();
                    if NamedNode::new(&namespace).is_err() {
                        return Err(HttpError::bad_request(format!(
                            "Not a namespace IRI: {namespace}"
                        )));
                    }
                    let mut ns = namespaces(state, entry);
                    ns.insert(prefix, namespace);
                    set_namespaces(state, entry, ns)?;
                    Ok(Reply::empty(204))
                }
                Method::Delete => {
                    context.operation = Some(Operation::Update);
                    crate::may_write_as(state, request, context)?;
                    let mut ns = namespaces(state, entry);
                    ns.remove(&prefix);
                    set_namespaces(state, entry, ns)?;
                    Ok(Reply::empty(204))
                }
                other => Err(HttpError(405, format!("{other} is not allowed here"))),
            }
        }
        ["transactions"] if method == Method::Post => {
            context.operation = Some(Operation::Update);
            if read_only {
                return Err(HttpError(405, "The repository is read-only".to_owned()));
            }
            // A transaction may only remove data: the quota is checked when data is added.
            crate::may_write_as(state, request, context)?;
            // The isolation level comes as a form; every commit is serializable here.
            read_form(state, request, params)?;
            let id = transaction_id();
            let mut transactions = state.rdf4j.transactions.lock_unpoisoned();
            transactions.retain(|_, t| t.touched.elapsed() < TRANSACTION_IDLE);
            transactions.insert(
                id.clone(),
                Transaction {
                    dataset: entry.name.clone(),
                    agent: context.caller.as_ref().and_then(|c| c.agent.clone()),
                    operations: Vec::new(),
                    size: 0,
                    touched: Instant::now(),
                },
            );
            let location = format!(
                "{}/{}/transactions/{id}",
                crate::origin(state, request),
                request
                    .url()
                    .split('?')
                    .next()
                    .unwrap_or_default()
                    .trim_start_matches('/')
                    .trim_end_matches("/transactions")
            );
            let mut reply = Reply::empty(201);
            reply.headers.push(("Location", location));
            Ok(reply)
        }
        ["transactions", id] => {
            let id = (*id).to_owned();
            transaction(state, request, params, context, entry, &id)
        }
        _ => Err(HttpError(
            404,
            format!("No RDF4J endpoint {method} {}", more.join("/")),
        )),
    }
}

fn transaction(
    state: &Arc<State>,
    request: &mut Request,
    params: &mut Params,
    context: &mut Context,
    entry: &Arc<Entry>,
    id: &str,
) -> Result<Reply, HttpError> {
    let limits = context.limits;
    let method = request.method().clone();
    let agent = context.caller.as_ref().and_then(|c| c.agent.clone());
    {
        let mut transactions = state.rdf4j.transactions.lock_unpoisoned();
        let t = transactions
            .get_mut(id)
            .filter(|t| t.dataset == entry.name && t.agent == agent)
            .ok_or_else(|| HttpError(404, format!("Unknown transaction: {id}")))?;
        t.touched = Instant::now();
    }
    let action = if method == Method::Delete {
        "ROLLBACK".to_owned()
    } else {
        if method == Method::Post
            && crate::content_type(request).as_deref() == Some("application/x-www-form-urlencoded")
        {
            read_form(state, request, params)?;
        }
        params
            .get("action")
            .ok_or_else(|| HttpError::bad_request("Missing the action parameter"))?
            .to_ascii_uppercase()
    };
    // Write rights are checked on every write, not only at begin: the transaction id must not
    // carry them, and a key may expire or a policy change while the transaction is open.
    if matches!(action.as_str(), "ADD" | "DELETE" | "UPDATE" | "COMMIT") {
        context.operation = Some(Operation::Update);
        // Only additions are held to the dataset's quota, so it can be brought back under.
        crate::may_write_as(state, request, context)?;
        if entry.mode == Mode::ReadOnly {
            return Err(HttpError(405, "The repository is read-only".to_owned()));
        }
    }
    let engine = &entry.engine;
    // The queued changes of a transaction together stay within the upload limit.
    let queue = |operation: String| -> Result<(), HttpError> {
        let mut transactions = state.rdf4j.transactions.lock_unpoisoned();
        let t = transactions
            .get_mut(id)
            .ok_or_else(|| HttpError(404, format!("Unknown transaction: {id}")))?;
        t.size += operation.len();
        if let Some(limit) = limits.max_upload
            && t.size > limit
        {
            transactions.remove(id);
            return Err(HttpError(
                413,
                format!(
                    "The transaction's changes exceed the upload limit of {limit} bytes; it was rolled back"
                ),
            ));
        }
        t.operations.push(operation);
        Ok(())
    };
    match action.as_str() {
        // Protocol 12 asks a transaction to prepare its commit; it is checked at COMMIT.
        "PREPARE" => Ok(Reply::empty(200)),
        "PING" => Ok(text(&TRANSACTION_IDLE.as_millis().to_string())),
        "ADD" => {
            context.operation = Some(Operation::Upload);
            crate::within_quota(context)?;
            let quads = read_statements(&limits, request, params)?;
            queue(insert_data(&quads))?;
            Ok(Reply::empty(200))
        }
        "DELETE" => {
            context.operation = Some(Operation::Update);
            // RDF4J's client sends `remove(s, p, o, c...)` here as statements: `sesame:wildcard`
            // matches anything, no context means every graph and `rdf4j:nil` the default graph.
            let quads = read_statements(&limits, request, params)?;
            let mut updates = Vec::new();
            for quad in quads {
                let wild = |t: Term| match t {
                    Term::NamedNode(n) if n.as_str() == WILDCARD => None,
                    t => Some(t),
                };
                let graphs = match quad.graph_name {
                    GraphName::DefaultGraph => None,
                    GraphName::NamedNode(n) if n.as_str() == NIL || n.as_str() == SESAME_NIL => {
                        Some(vec![GraphName::DefaultGraph])
                    }
                    g => Some(vec![g]),
                };
                updates.push(pattern_delete(
                    [
                        wild(quad.subject.into()),
                        wild(quad.predicate.into()),
                        wild(quad.object),
                    ],
                    graphs,
                )?);
            }
            if !updates.is_empty() {
                queue(updates.join(" ;\n"))?;
            }
            Ok(Reply::empty(200))
        }
        "UPDATE" => {
            context.operation = Some(Operation::Update);
            read_form(state, request, params)?;
            let update = match params.get("update") {
                Some(u) => u.to_owned(),
                None => crate::read_body(request, state.options.max_body)?,
            };
            // Checked now, so an error shows here and not at commit.
            let update = prepare_update(state, entry, params, &update)?;
            crate::update_within_quota(context, || {
                tripletrail::Engine::update_only_removes(&update)
            })?;
            queue(update)?;
            Ok(Reply::empty(200))
        }
        "QUERY" => {
            context.operation = Some(Operation::Query);
            read_form(state, request, params)?;
            let query = match params.get("query") {
                Some(q) => q.to_owned(),
                None => crate::read_body(request, state.options.max_body)?,
            };
            // Under row rules (#202), reads in a transaction read the caller's view too.
            query_reply(
                state,
                request,
                params,
                entry,
                &query,
                None,
                context.row_rules.clone(),
            )
        }
        "GET" => {
            context.operation = Some(Operation::Read);
            let rows = crate::row_view(engine, context)?;
            statements(engine, request, params, None, rows)
        }
        "SIZE" => {
            context.operation = Some(Operation::Read);
            Ok(text(
                &match crate::row_view(engine, context)? {
                    Some(view) => view.quads_for_pattern(None, None, None, None).count(),
                    None => engine.store().len()?,
                }
                .to_string(),
            ))
        }
        "COMMIT" => {
            context.operation = Some(Operation::Update);
            // Checked again at commit, the transaction kept for a rollback if refused.
            crate::update_within_quota(context, || {
                state
                    .rdf4j
                    .transactions
                    .lock_unpoisoned()
                    .get(id)
                    .is_none_or(|t| {
                        t.operations.is_empty()
                            || tripletrail::Engine::update_only_removes(&t.operations.join(" ;\n"))
                    })
            })?;
            let t = state
                .rdf4j
                .transactions
                .lock_unpoisoned()
                .remove(id)
                .ok_or_else(|| HttpError(404, format!("Unknown transaction: {id}")))?;
            if !t.operations.is_empty() {
                run_update(
                    &limits,
                    params,
                    engine,
                    &t.operations.join(" ;\n"),
                    context.row_rules.as_ref(),
                )?;
            }
            Ok(Reply::empty(200))
        }
        "ROLLBACK" => {
            state.rdf4j.transactions.lock_unpoisoned().remove(id);
            Ok(Reply::empty(204))
        }
        other => Err(HttpError::bad_request(format!(
            "Unsupported transaction action {other}"
        ))),
    }
}

/// `GET /repositories`: every dataset the caller may read.
fn repositories(
    state: &Arc<State>,
    request: &Request,
    params: &Params,
    context: &Context,
) -> Result<Reply, HttpError> {
    let base = format!(
        "{}/rdf4j-server/repositories",
        crate::origin(state, request)
    );
    let registry = state.registry.read_unpoisoned();
    let mut names: Vec<(String, Arc<Entry>)> = registry
        .datasets
        .values()
        .map(|e| (e.name.clone(), e.clone()))
        .chain(
            registry
                .aliases
                .keys()
                .filter_map(|a| Some((a.clone(), registry.resolve(a)?))),
        )
        .collect();
    drop(registry);
    names.sort_by(|a, b| a.0.cmp(&b.0));
    let vars = ["uri", "id", "title", "readable", "writable"]
        .map(Variable::new_unchecked)
        .to_vec();
    let boolean = |b: bool| Some(Term::from(Literal::from(b)));
    let mut rows = Vec::new();
    for (name, entry) in names {
        // A repository of which the caller reads only some graphs (#203) is listed, read-only.
        let Some(view) = crate::may_list(state, context.caller.as_ref(), &[&name, &entry.name])
        else {
            continue;
        };
        let writable = entry.mode != Mode::ReadOnly
            && view.is_none()
            && match (&state.auth, &context.caller) {
                (Some(auth), Some(caller)) => auth.access(caller, &[&name, &entry.name]).write,
                _ => true,
            };
        rows.push(vec![
            Some(NamedNode::new_unchecked(format!("{base}/{name}")).into()),
            Some(Literal::new_simple_literal(&name).into()),
            Some(Literal::new_simple_literal(format!("TripleTrail dataset {name}")).into()),
            boolean(true),
            boolean(writable),
        ]);
    }
    table(request, params, vars, rows)
}

/// A query as RDF4J's server runs it: the request's `baseURI`, the well-known and the
/// repository's prefixes without declaring them, the union of all graphs as the default graph
/// (or the `default-graph-uri` and `named-graph-uri` given), and `$name` bindings.
fn query_reply(
    state: &State,
    request: &Request,
    params: &Params,
    entry: &Entry,
    query: &str,
    readable: Option<tripletrail::ReadableGraphs>,
    row_rules: Option<tripletrail::RowRules>,
) -> Result<Reply, HttpError> {
    let limits = Limits::of(state, &entry.name);
    let timeout = timeout(&limits, params)?;
    let budget = crate::RowBudget::of(params, limits.max_rows)?;
    let mut query = with_prologue(state, entry, params, query);
    // A query with its own FROM keeps it; others read every graph, as on RDF4J.
    let mut options = query_options(
        params,
        entry.engine.union_default_graph() || declares_dataset(&query),
    )?;
    options.readable = readable;
    options.row_rules = row_rules;
    loop {
        let result = crate::run_query(
            &entry.engine,
            &query,
            request,
            params,
            timeout,
            budget,
            options.clone(),
        );
        // The evaluator binds only variables the query selects; RDF4J binds any. Others are
        // written into the query in place of the variable.
        if let Err(HttpError(_, m)) = &result
            && let Some(name) = m
                .split_once("does not contains variable ?")
                .and_then(|(_, rest)| rest.split_once(' '))
                .map(|(name, _)| name.to_owned())
            && let Some(at) = options
                .substitutions
                .iter()
                .position(|(v, _)| v.as_str() == name)
        {
            let (variable, value) = options.substitutions.remove(at);
            query = replace_variable(&query, variable.as_str(), &value.to_string());
            continue;
        }
        return result.map_err(|e| match e {
            HttpError(400, m) => HttpError(400, format!("MALFORMED QUERY: {m}")),
            HttpError(500, m) => HttpError(500, format!("QUERY EVALUATION ERROR: {m}")),
            other => other,
        });
    }
}

/// The query with `?name` and `$name` replaced by `value`, outside strings, IRIs and comments.
fn replace_variable(query: &str, name: &str, value: &str) -> String {
    let chars: Vec<char> = query.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let name_char = |c: char| c.is_alphanumeric() || c == '_' || c == '\u{b7}';
    let mut out = String::with_capacity(query.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '#' | '<' | '"' | '\'' => {
                let end = skip_token(&chars, i);
                out.extend(&chars[i..end]);
                i = end;
            }
            '?' | '$'
                if chars[i + 1..].starts_with(&name)
                    && !chars.get(i + 1 + name.len()).is_some_and(|&c| name_char(c)) =>
            {
                out.push_str(value);
                i += 1 + name.len();
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Whether a query has a `FROM` (or `FROM NAMED`) clause.
fn declares_dataset(query: &str) -> bool {
    let chars: Vec<char> = query.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '#' | '<' | '"' | '\'' => i = skip_token(&chars, i),
            c if c.is_alphabetic() => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let before = start.checked_sub(1).map(|j| chars[j]);
                let after = chars.get(i).copied();
                let word: String = chars[start..i].iter().collect();
                if word.eq_ignore_ascii_case("from")
                    && !matches!(before, Some('?' | '$' | ':' | '@'))
                    && after != Some(':')
                {
                    return true;
                }
            }
            _ => i += 1,
        }
    }
    false
}

/// The end of a comment, IRI or string starting at `i` (`i + 1` for a `<` that starts no
/// IRI, a comparison).
fn skip_token(chars: &[char], i: usize) -> usize {
    let c = chars[i];
    match c {
        '#' => {
            let mut j = i;
            while j < chars.len() && chars[j] != '\n' {
                j += 1;
            }
            j
        }
        '<' => {
            let mut j = i + 1;
            while j < chars.len() && !matches!(chars[j], '>' | ' ' | '\n' | '\t' | '<') {
                j += 1;
            }
            if j < chars.len() && chars[j] == '>' {
                j + 1
            } else {
                i + 1
            }
        }
        _ => {
            let long = i + 2 < chars.len() && chars[i + 1] == c && chars[i + 2] == c;
            let mut j = i + if long { 3 } else { 1 };
            while j < chars.len() {
                if chars[j] == '\\' {
                    j += 2;
                    continue;
                }
                if chars[j] == c
                    && (!long || (j + 2 < chars.len() && chars[j + 1] == c && chars[j + 2] == c))
                {
                    return j + if long { 3 } else { 1 };
                }
                j += 1;
            }
            chars.len()
        }
    }
}

/// An update from a request, as RDF4J's server reads it: with its `baseURI` and the prefixes
/// it may use without declaring them, checked for syntax. Datasets of updates
/// (`using-graph-uri`, `remove-graph-uri`, ...) are refused rather than ignored.
fn prepare_update(
    state: &State,
    entry: &Entry,
    params: &Params,
    update: &str,
) -> Result<String, HttpError> {
    for name in [
        "using-graph-uri",
        "using-named-graph-uri",
        "remove-graph-uri",
        "insert-graph-uri",
        "default-graph-uri",
        "named-graph-uri",
    ] {
        if params.has(name) {
            return Err(HttpError::bad_request(format!(
                "MALFORMED QUERY: {name} is not supported for updates; use USING, WITH or \
                 GRAPH in the update"
            )));
        }
    }
    let update = with_prologue(state, entry, params, update);
    tripletrail::complexity::check_nesting(&update)
        .map_err(|e| HttpError::bad_request(format!("MALFORMED QUERY: {e}")))?;
    // Virtuoso's DEFINE clauses are read by the engine (#238): checked without them.
    let defined = tripletrail::without_defines(&update)
        .map_err(|e| HttpError::bad_request(format!("MALFORMED QUERY: {e}")))?;
    let _ = tripletrail::oxigraph::sparql::SparqlEvaluator::new()
        .parse_update(
            defined
                .as_ref()
                .map_or(update.as_str(), |(rest, _)| rest.as_str()),
        )
        .map_err(|e| HttpError::bad_request(format!("MALFORMED QUERY: {e}")))?;
    Ok(update)
}

/// Prefixes RDF4J's SPARQL parser knows without a declaration.
const IMPLICIT_PREFIXES: [(&str, &str); 6] = [
    ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
    ("owl", "http://www.w3.org/2002/07/owl#"),
    ("xsd", "http://www.w3.org/2001/XMLSchema#"),
    ("fn", "http://www.w3.org/2005/xpath-functions#"),
    ("sesame", "http://www.openrdf.org/schema/sesame#"),
];

/// The query or update with `BASE` from `baseURI` and `PREFIX`es for the repository's and the
/// well-known prefixes it uses without declaring.
fn with_prologue(state: &State, entry: &Entry, params: &Params, text: &str) -> String {
    let mut prologue = String::new();
    if let Some(base) = params.get("baseURI").filter(|b| !b.is_empty()) {
        let _ = writeln!(
            prologue,
            "BASE <{}>",
            base.trim_matches(|c| c == '<' || c == '>')
        );
    }
    let used = used_prefixes(text);
    if !used.is_empty() {
        let mut known: BTreeMap<String, String> = IMPLICIT_PREFIXES
            .iter()
            .map(|(p, n)| ((*p).to_owned(), (*n).to_owned()))
            .collect();
        known.extend(namespaces(state, entry));
        for prefix in used {
            if let Some(namespace) = known.get(&prefix) {
                let _ = writeln!(prologue, "PREFIX {prefix}: <{namespace}>");
            }
        }
    }
    if prologue.is_empty() {
        text.to_owned()
    } else {
        prologue + text
    }
}

/// The prefixes a query uses in prefixed names but does not declare. Strings, IRIs and
/// comments are skipped.
fn used_prefixes(text: &str) -> std::collections::BTreeSet<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut used = std::collections::BTreeSet::new();
    let mut declared = std::collections::BTreeSet::new();
    let name = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '.');
    let mut i = 0;
    let mut last_word = String::new();
    while i < chars.len() {
        let c = chars[i];
        match c {
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '<' => {
                // An IRI, or a comparison: an IRI has no whitespace before its `>`.
                let mut j = i + 1;
                while j < chars.len() && !matches!(chars[j], '>' | ' ' | '\n' | '\t' | '<') {
                    j += 1;
                }
                if j < chars.len() && chars[j] == '>' {
                    i = j;
                }
            }
            '"' | '\'' => {
                let long = i + 2 < chars.len() && chars[i + 1] == c && chars[i + 2] == c;
                i += if long { 3 } else { 1 };
                while i < chars.len() {
                    if chars[i] == '\\' {
                        i += 2;
                        continue;
                    }
                    if chars[i] == c
                        && (!long
                            || (i + 2 < chars.len() && chars[i + 1] == c && chars[i + 2] == c))
                    {
                        if long {
                            i += 2;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            c if name(c) || c == ':' => {
                let start = i;
                while i < chars.len() && (name(chars[i]) || chars[i] == ':') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let after_var = start > 0 && matches!(chars[start - 1], '?' | '$' | '@' | '_');
                if let Some(colon) = word.find(':')
                    && !after_var
                    && !word.starts_with(|c: char| c.is_ascii_digit())
                {
                    let prefix = word[..colon].to_owned();
                    if last_word.eq_ignore_ascii_case("prefix") {
                        declared.insert(prefix);
                    } else {
                        used.insert(prefix);
                    }
                }
                last_word = word;
                continue;
            }
            c if !c.is_whitespace() => last_word.clear(),
            _ => {}
        }
        i += 1;
    }
    used.retain(|p| !declared.contains(p));
    used
}

/// The dataset and bindings of a query request.
fn query_options(params: &Params, union: bool) -> Result<tripletrail::QueryOptions, HttpError> {
    use tripletrail::oxigraph::sparql::QueryDataset;
    let graphs = |name: &str| -> Result<Vec<Term>, HttpError> {
        params
            .0
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| {
                if v == "null" {
                    return Ok(Term::NamedNode(NamedNode::new_unchecked(NIL)));
                }
                let v = v.trim();
                let v = if v.starts_with('<') || v.starts_with("_:") {
                    v.to_owned()
                } else {
                    format!("<{v}>")
                };
                Term::from_str(&v).map_err(|e| {
                    HttpError::bad_request(format!("MALFORMED QUERY: {name} {v}: {e}"))
                })
            })
            .collect()
    };
    let default = graphs("default-graph-uri")?;
    let named = graphs("named-graph-uri")?;
    let mut options = tripletrail::QueryOptions::default();
    if !default.is_empty() || !named.is_empty() {
        let mut dataset = QueryDataset::new();
        dataset.set_default_graph(
            default
                .into_iter()
                .map(|t| match t {
                    Term::NamedNode(n) if n.as_str() == NIL => GraphName::DefaultGraph,
                    Term::NamedNode(n) => GraphName::NamedNode(n),
                    Term::BlankNode(b) => GraphName::BlankNode(b),
                    _ => GraphName::DefaultGraph,
                })
                .collect(),
        );
        // As in SPARQL, a dataset given only default graphs has no named graphs.
        {
            dataset.set_available_named_graphs(
                named
                    .into_iter()
                    .filter_map(|t| match t {
                        Term::NamedNode(n) => Some(NamedOrBlankNode::NamedNode(n)),
                        Term::BlankNode(b) => Some(NamedOrBlankNode::BlankNode(b)),
                        _ => None,
                    })
                    .collect(),
            );
        }
        options.dataset = Some(dataset);
    } else if !union {
        // RDF4J reads every statement as the default graph.
        let mut dataset = QueryDataset::new();
        dataset.set_default_graph_as_union();
        options.dataset = Some(dataset);
    }
    for (name, value) in &params.0 {
        if let Some(var) = name.strip_prefix('$') {
            let var = Variable::new(var)
                .map_err(|e| HttpError::bad_request(format!("MALFORMED QUERY: ${var}: {e}")))?;
            let value = Term::from_str(value).map_err(|e| {
                HttpError::bad_request(format!("MALFORMED QUERY: ${}: {e}", var.as_str()))
            })?;
            options.substitutions.push((var, value));
        }
    }
    Ok(options)
}

fn run_update(
    limits: &Limits,
    params: &Params,
    engine: &Engine,
    update: &str,
    row_rules: Option<&tripletrail::RowRules>,
) -> Result<(), HttpError> {
    let timeout = timeout(limits, params)?;
    let (engine, _deadline) = crate::with_deadline(engine, timeout);
    let options = tripletrail::UpdateOptions {
        row_rules: row_rules.cloned(),
        ..Default::default()
    };
    engine
        .update_with_options(update, &options)
        .map_err(|e| match HttpError::from(e) {
            HttpError(400, m) => HttpError(400, format!("MALFORMED QUERY: {m}")),
            other => other,
        })
}

/// The server's timeout, or a shorter one asked for with `timeout` (seconds, as in RDF4J's
/// `timeout` parameter).
fn timeout(limits: &Limits, params: &Params) -> Result<Option<Duration>, HttpError> {
    Ok(limits.timeout(
        params
            .get("timeout")
            .map(crate::parse_timeout)
            .transpose()?,
    ))
}

fn read_form(state: &State, request: &mut Request, params: &mut Params) -> Result<(), HttpError> {
    // RDF4J's client puts an empty `query` in the URL of a transaction QUERY, with the query
    // itself in the body.
    params
        .0
        .retain(|(k, v)| !(v.is_empty() && (k == "query" || k == "update")));
    match crate::content_type(request).as_deref() {
        Some("application/x-www-form-urlencoded") => {
            let body = crate::read_body(request, state.options.max_body)?;
            params
                .0
                .extend(form_urlencoded::parse(body.as_bytes()).into_owned());
        }
        Some("application/sparql-query") => {
            let body = crate::read_body(request, state.options.max_body)?;
            params.0.push(("query".to_owned(), body));
        }
        _ => {}
    }
    Ok(())
}

/// The graphs named by `context` parameters: `None` without any (every graph), `null` is the
/// default graph.
fn contexts(params: &Params) -> Result<Option<Vec<GraphName>>, HttpError> {
    let values: Vec<&str> = params
        .0
        .iter()
        .filter(|(k, _)| k == "context")
        .map(|(_, v)| v.as_str())
        .collect();
    if values.is_empty() {
        return Ok(None);
    }
    values
        .into_iter()
        .map(|v| {
            if v == "null" {
                return Ok(GraphName::DefaultGraph);
            }
            match Term::from_str(v) {
                Ok(Term::NamedNode(n)) => Ok(GraphName::NamedNode(n)),
                Ok(Term::BlankNode(b)) => Ok(GraphName::BlankNode(b)),
                _ => Err(HttpError::bad_request(format!("Invalid context: {v}"))),
            }
        })
        .collect::<Result<Vec<GraphName>, _>>()
        .map(|mut graphs| {
            // A graph named twice is read once.
            let mut seen = std::collections::HashSet::new();
            graphs.retain(|g| seen.insert(g.clone()));
            Some(graphs)
        })
}

/// A `subj`, `pred` or `obj` parameter, in N-Triples syntax.
fn term(params: &Params, name: &str) -> Result<Option<Term>, HttpError> {
    params
        .get(name)
        .filter(|v| !v.is_empty())
        .map(|v| {
            Term::from_str(v)
                .map_err(|e| HttpError::bad_request(format!("Invalid {name} {v}: {e}")))
        })
        .transpose()
}

/// `GET .../statements`: the matching statements, as a quad format when the client takes one.
/// The statements matching the `subj`, `pred`, `obj` and `context` parameters, in the
/// `readable` graphs only when given (#203).
fn statements(
    engine: &Engine,
    request: &Request,
    params: &Params,
    readable: Option<tripletrail::ReadableGraphs>,
    rows: Option<tripletrail::store::Snapshot>,
) -> Result<Reply, HttpError> {
    let subject = match term(params, "subj")? {
        None => None,
        Some(Term::NamedNode(n)) => Some(NamedOrBlankNode::NamedNode(n)),
        Some(Term::BlankNode(b)) => Some(NamedOrBlankNode::BlankNode(b)),
        Some(other) => return Err(HttpError::bad_request(format!("Invalid subj {other}"))),
    };
    let predicate = match term(params, "pred")? {
        None => None,
        Some(Term::NamedNode(n)) => Some(n),
        Some(other) => return Err(HttpError::bad_request(format!("Invalid pred {other}"))),
    };
    let object = term(params, "obj")?;
    let graphs = contexts(params)?;
    let format = crate::rdf_format(request, params, RdfFormat::NQuads, true)?;
    let store = engine.store().clone();
    Reply::stream(format.media_type(), move |out| {
        let mut serializer = RdfSerializer::from_format(format).for_writer(out);
        let graphs: Vec<Option<GraphName>> = match graphs {
            None => vec![None],
            Some(g) => g.into_iter().map(Some).collect(),
        };
        for graph in &graphs {
            let s = subject.as_ref().map(NamedOrBlankNodeRef::from);
            let p = predicate.as_ref().map(Into::into);
            let o = object.as_ref().map(Into::into);
            let g = graph.as_ref().map(GraphName::as_ref);
            // Under row rules (#202), from the view without the hidden rows.
            let quads: Box<dyn Iterator<Item = Result<Quad, std::io::Error>>> = match &rows {
                Some(view) => Box::new(view.quads_for_pattern(s, p, o, g).map(Ok)),
                None => Box::new(
                    store
                        .quads_for_pattern(s, p, o, g)
                        .map(|q| q.map_err(std::io::Error::other)),
                ),
            };
            for quad in quads {
                let quad = quad?;
                if !readable
                    .as_ref()
                    .is_none_or(|r| r.contains(quad.graph_name.as_ref()))
                {
                    continue;
                }
                if format.supports_datasets() {
                    serializer.serialize_quad(&quad)?;
                } else {
                    serializer.serialize_triple(quad.as_ref())?;
                }
            }
        }
        serializer.finish()?;
        Ok(())
    })
}

/// The statements of an RDF request body, in the graphs of the `context` parameters (each of
/// them) or their own. Blank node labels get the prefix `b{n}_`, so the bodies of one
/// transaction do not share blank nodes.
/// The statements of an upload. Its blank nodes are new ones, as RDF4J's server makes them,
/// unless the client asks to keep their ids with `preserveNodeId=true` (it does when it sends
/// statements it holds, such as a blank node read before).
fn read_statements(
    limits: &Limits,
    request: &mut Request,
    params: &Params,
) -> Result<Vec<Quad>, HttpError> {
    let media = crate::content_type(request).ok_or_else(|| {
        HttpError(
            415,
            "UNSUPPORTED FILE FORMAT: missing Content-Type".to_owned(),
        )
    })?;
    let body = crate::read_bytes(request, limits.max_upload)?;
    let format = if media == crate::binary_rdf::MEDIA_TYPE {
        None
    } else {
        Some(
            RdfFormat::from_media_type(&media)
                .ok_or_else(|| HttpError(415, format!("UNSUPPORTED FILE FORMAT: {media}")))?,
        )
    };
    let malformed = |e: String| HttpError(400, format!("MALFORMED DATA: {e}"));
    // RDF4J's client sends gzip and zip files as they are; a zip entry's extension, if it
    // names a format, wins over the Content-Type.
    let documents = crate::archive::unpack(body, limits.max_upload).map_err(malformed)?;
    let mut parsed = Vec::new();
    for (name, document) in documents {
        let format = name
            .as_deref()
            .and_then(|n| n.rsplit_once('.'))
            .and_then(|(_, ext)| RdfFormat::from_extension(ext))
            .or(format);
        match format {
            None => parsed.extend(crate::binary_rdf::parse(&document).map_err(malformed)?),
            Some(format) => {
                let mut parser = RdfParser::from_format(format);
                if let Some(base) = params.get("baseURI").filter(|b| !b.is_empty()) {
                    parser = parser
                        .with_base_iri(base.trim_matches(|c| c == '<' || c == '>'))
                        .map_err(|e| malformed(e.to_string()))?;
                }
                for quad in parser.for_slice(&document) {
                    parsed.push(quad.map_err(|e| malformed(e.to_string()))?);
                }
            }
        }
    }
    let graphs = contexts(params)?;
    let id = transaction_id();
    let prefix = (params.get("preserveNodeId") != Some("true")).then(|| &id[..16]);
    let fresh = |b: BlankNode| match prefix {
        Some(p) => BlankNode::new_unchecked(format!("{p}{}", b.as_str())),
        None => b,
    };
    let rename = |node: NamedOrBlankNode| match node {
        NamedOrBlankNode::BlankNode(b) => NamedOrBlankNode::BlankNode(fresh(b)),
        other => other,
    };
    let mut quads = Vec::new();
    for quad in parsed {
        let object = match quad.object {
            Term::BlankNode(b) => Term::BlankNode(fresh(b)),
            other => other,
        };
        let subject = rename(quad.subject);
        match &graphs {
            None => quads.push(Quad::new(subject, quad.predicate, object, quad.graph_name)),
            Some(graphs) => {
                for g in graphs {
                    quads.push(Quad::new(
                        subject.clone(),
                        quad.predicate.clone(),
                        object.clone(),
                        g.clone(),
                    ));
                }
            }
        }
    }
    Ok(quads)
}

/// `INSERT DATA` for quads, grouped by graph.
fn data(keyword: &str, quads: &[Quad]) -> String {
    let mut by_graph: BTreeMap<String, String> = BTreeMap::new();
    for q in quads {
        let key = match &q.graph_name {
            GraphName::DefaultGraph => String::new(),
            g => g.to_string(),
        };
        let _ = writeln!(
            by_graph.entry(key).or_default(),
            "{} {} {} .",
            q.subject,
            q.predicate,
            q.object
        );
    }
    let mut out = format!("{keyword} {{\n");
    for (graph, triples) in &by_graph {
        if graph.is_empty() {
            out.push_str(triples);
        } else {
            let _ = write!(out, "GRAPH {graph} {{\n{triples}}}\n");
        }
    }
    out.push('}');
    out
}

fn insert_data(quads: &[Quad]) -> String {
    data("INSERT DATA", quads)
}

const WILDCARD: &str = "http://www.openrdf.org/schema/sesame#wildcard";
const NIL: &str = "http://rdf4j.org/schema/rdf4j#nil";
const SESAME_NIL: &str = "http://www.openrdf.org/schema/sesame#nil";

/// `DELETE .../statements`: removes what matches `subj`, `pred`, `obj` in the `context`s.
fn delete_pattern(params: &Params) -> Result<String, HttpError> {
    pattern_delete(
        [
            term(params, "subj")?,
            term(params, "pred")?,
            term(params, "obj")?,
        ],
        contexts(params)?,
    )
}

/// An update that removes what matches a pattern (`None` matches anything) in the graphs
/// (`None`: in every graph).
fn pattern_delete(
    terms: [Option<Term>; 3],
    graphs: Option<Vec<GraphName>>,
) -> Result<String, HttpError> {
    let mut parts = Vec::new();
    for (term, var) in terms.into_iter().zip(["?s", "?p", "?o"]) {
        match term {
            None => parts.push(var.to_owned()),
            Some(Term::BlankNode(_)) => {
                return Err(HttpError::bad_request(
                    "A blank node cannot select statements to remove",
                ));
            }
            Some(t) => parts.push(t.to_string()),
        }
    }
    let triple = parts.join(" ");
    Ok(match graphs {
        None => {
            format!("DELETE WHERE {{ {triple} }} ;\nDELETE WHERE {{ GRAPH ?g {{ {triple} }} }}")
        }
        Some(graphs) => graphs
            .iter()
            .map(|g| match g {
                GraphName::DefaultGraph => format!("DELETE WHERE {{ {triple} }}"),
                g => format!("DELETE WHERE {{ GRAPH {g} {{ {triple} }} }}"),
            })
            .collect::<Vec<_>>()
            .join(" ;\n"),
    })
}

/// `PUT .../statements` first empties what it replaces: the `context`s, or everything.
fn clear(params: &Params) -> Result<String, HttpError> {
    Ok(match contexts(params)? {
        None => "CLEAR ALL".to_owned(),
        Some(graphs) => graphs
            .iter()
            .map(|g| match g {
                GraphName::DefaultGraph => "CLEAR DEFAULT".to_owned(),
                g => format!("CLEAR SILENT GRAPH {g}"),
            })
            .collect::<Vec<_>>()
            .join(" ;\n"),
    })
}

/// A SPARQL results table (JSON, XML, CSV or TSV, as asked).
fn table(
    request: &Request,
    params: &Params,
    vars: Vec<Variable>,
    rows: Vec<Vec<Option<Term>>>,
) -> Result<Reply, HttpError> {
    let format = crate::results_format(request, params)?;
    let mut out = Vec::new();
    let mut serializer = QueryResultsSerializer::from_format(format)
        .serialize_solutions_to_writer(&mut out, vars.clone())?;
    for row in rows {
        serializer.serialize(&QuerySolution::from((vars.clone(), row)))?;
    }
    serializer.finish()?;
    Ok(Reply::bytes(200, format.media_type(), out))
}

fn text(s: &str) -> Reply {
    Reply::bytes(200, "text/plain; charset=utf-8", s.as_bytes().to_vec())
}

fn transaction_id() -> String {
    let random = || {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        );
        h.finish()
    };
    format!("{:016x}{:016x}", random(), random())
}

fn namespaces_path(entry: &Entry) -> Option<PathBuf> {
    entry.location.as_ref().map(|l| l.join(NAMESPACES_FILE))
}

/// The dataset's namespaces: as changed over the API, or from its directory, or the defaults.
fn namespaces(state: &State, entry: &Entry) -> BTreeMap<String, String> {
    let mut all = state.rdf4j.namespaces.lock_unpoisoned();
    all.entry(entry.name.clone())
        .or_insert_with(|| {
            namespaces_path(entry)
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_else(|| {
                    DEFAULT_NAMESPACES
                        .iter()
                        .map(|(p, n)| ((*p).to_owned(), (*n).to_owned()))
                        .collect()
                })
        })
        .clone()
}

/// Keeps the namespaces, in the dataset directory too when it has a writable one.
fn set_namespaces(
    state: &State,
    entry: &Entry,
    namespaces: BTreeMap<String, String>,
) -> Result<(), HttpError> {
    if entry.mode != Mode::ReadOnly
        && let Some(path) = namespaces_path(entry)
    {
        let json =
            serde_json::to_vec_pretty(&namespaces).map_err(|e| HttpError(500, e.to_string()))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
    }
    state
        .rdf4j
        .namespaces
        .lock_unpoisoned()
        .insert(entry.name.clone(), namespaces);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_text_helpers() {
        let q = "PREFIX foaf: <http://xmlns.com/foaf/0.1/>\n# rdf:comment\nSELECT ?x WHERE { ?x rdfs:label \"a xsd:b\" ; foaf:name ?n . FILTER(?n < 3 && ?x != <urn:a:b>) } # owl:x";
        let used: Vec<_> = used_prefixes(q).into_iter().collect();
        assert_eq!(used, ["rdfs"]);
        assert!(!declares_dataset(q));
        assert!(declares_dataset("SELECT * FROM <urn:g> { ?s ?p ?o }"));
        assert!(!declares_dataset(
            "SELECT ?from { ?s ex:from ?from . FILTER(?s = \"FROM\") }"
        ));
        assert_eq!(
            replace_variable(
                "SELECT ?p { ?s ?p $name . FILTER(?name2 = \"?name\") }",
                "name",
                "\"x\""
            ),
            "SELECT ?p { ?s ?p \"x\" . FILTER(?name2 = \"?name\") }"
        );
    }

    fn params(pairs: &[(&str, &str)]) -> Params {
        Params(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    #[test]
    fn patterns_and_data_become_sparql_updates() {
        let p = params(&[
            ("subj", "<urn:s>"),
            ("context", "null"),
            ("context", "<urn:g>"),
        ]);
        assert_eq!(
            delete_pattern(&p).unwrap(),
            "DELETE WHERE { <urn:s> ?p ?o } ;\nDELETE WHERE { GRAPH <urn:g> { <urn:s> ?p ?o } }"
        );
        assert!(delete_pattern(&params(&[("obj", "_:b")])).is_err());
        assert_eq!(clear(&params(&[])).unwrap(), "CLEAR ALL");
        let q = Quad::new(
            NamedNode::new_unchecked("urn:s"),
            NamedNode::new_unchecked("urn:p"),
            Literal::new_simple_literal("a \"quoted\"\nvalue"),
            NamedNode::new_unchecked("urn:g"),
        );
        let update = insert_data(&[q]);
        assert_eq!(
            update,
            "INSERT DATA {\nGRAPH <urn:g> {\n<urn:s> <urn:p> \"a \\\"quoted\\\"\\nvalue\" .\n}\n}"
        );
        let _ = tripletrail::oxigraph::sparql::SparqlEvaluator::new()
            .parse_update(&update)
            .unwrap();
    }
}
