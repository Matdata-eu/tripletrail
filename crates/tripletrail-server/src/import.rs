//! The Import page's endpoints (#20): a file sent to the server is kept in memory, never
//! written to disk, and read as Facade-X (`SERVICE <x-sparql-anything:location=upload:NAME>`)
//! by the requests that name it. Writers only, as running a mapping writes the dataset.
//!
//! - `GET /{ds}/import`: `{maxBytes}`, the largest file it takes.
//! - `POST /{ds}/import?name=FILE[&options=…][&namespace=IRI]` with the file as the body: keeps
//!   it for an hour and answers `{id, name, bytes, options, location, tree, triples, truncated,
//!   namespace, kinds, schema, query}`: the first triples of its Facade-X graph and a CONSTRUCT
//!   that lifts it in its own vocabulary under `namespace` (#275, see `import_draft`).
//! - `POST /{ds}/import/{id}/draft` with `{namespace, keys: {KIND: COLUMN or null}}`: the draft
//!   again, `{namespace, kinds, query}`, with other IRIs or key columns.
//! - `POST /{ds}/import/{id}/preview` with a CONSTRUCT as the body: its triples, as a query.
//! - `POST /{ds}/import/{id}/run?graph=IRI` with a CONSTRUCT as the body: adds its triples to
//!   the named graph, as `INSERT { GRAPH <graph> { … } } WHERE { … }`, and answers
//!   `{graph, before, after}` (the graph's triples before and after).
//! - `DELETE /{ds}/import/{id}`: forgets the file.

use crate::{Context, HttpError, Params, Reply};
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tiny_http::{Method, Request};
use tripletrail::Engine;
use tripletrail::oxigraph::model::Term;
use tripletrail::oxigraph::sparql::QueryResults;

/// How long an upload is kept after it was last used.
const LIFE: Duration = Duration::from_secs(3600);
/// Uploads kept at most; the least recently used goes first.
const MAX_UPLOADS: usize = 16;
/// The most bytes all uploads take together; the least recently used go first.
const MAX_TOTAL: usize = 256 << 20;
/// Triples of the Facade-X graph read to show and to draft the query from.
const TREE_TRIPLES: usize = 2000;
/// Triples of those shown in the answer.
const SHOWN: usize = 300;

struct Upload {
    /// The dataset it was sent to (`Entry::id`).
    dataset: u64,
    name: String,
    /// The Facade-X options it is read with (`csv.headers=true`, …), joined with commas.
    options: String,
    bytes: Arc<[u8]>,
    used: Mutex<Instant>,
}

impl Upload {
    /// `upload:NAME` with its options, as the SERVICE IRI names it.
    fn service(&self) -> String {
        let mut iri = format!("x-sparql-anything:location=upload:{}", self.name);
        if !self.options.is_empty() {
            iri.push(',');
            iri.push_str(&self.options);
        }
        iri
    }

    fn idle(&self) -> Duration {
        self.used
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .elapsed()
    }
}

/// Facade-X options that choose the source, which an upload's options may not set.
const SOURCE_KEYS: [&str; 7] = [
    "location",
    "from-archive",
    "content",
    "command",
    "table",
    "sql",
    "database",
];

/// The kept uploads by id, the most recently used last.
type Kept = VecDeque<(String, Arc<Upload>)>;

static UPLOADS: LazyLock<Mutex<Kept>> = LazyLock::new(|| Mutex::new(VecDeque::new()));

/// Answers `/{ds}/import…` (`rest` is what follows `import`).
pub(crate) fn route(
    state: &crate::State,
    request: &mut Request,
    rest: &[&str],
    params: &Params,
    context: &mut Context,
    engine: &Engine,
    read_only: bool,
) -> Result<Reply, HttpError> {
    let method = request.method().clone();
    // Running a mapping writes the dataset, and an upload takes the server's memory: writers
    // only, on every request.
    crate::may_write_as(state, request, context)?;
    // A preview's WHERE is free text and a run counts the graph: only for callers who may read
    // and write the whole dataset (routing already refuses graph grants and row rules here;
    // this keeps it so).
    if context.readable.is_some() || context.writable.is_some() || context.row_rules.is_some() {
        return Err(HttpError(
            403,
            "Importing needs access to the whole dataset: your access is limited to some \
             graphs or rows"
                .to_owned(),
        ));
    }
    if read_only {
        return Err(HttpError(405, "The dataset is read-only".to_owned()));
    }
    let entry = context.dataset.clone().expect("set by the caller");
    let timeout = context.limits.timeout(
        params
            .get("timeout")
            .map(crate::parse_timeout)
            .transpose()?,
    );
    match (method, rest) {
        (Method::Get | Method::Head, []) => {
            context.operation = Some(crate::Operation::Read);
            let body = serde_json::json!({
                "maxBytes": state.options.max_body.map_or(MAX_TOTAL, |m| m.min(MAX_TOTAL)),
                "kept": MAX_UPLOADS,
            });
            Ok(Reply::bytes(
                200,
                "application/json",
                body.to_string().into_bytes(),
            ))
        }
        (Method::Post, [id, "draft"]) => {
            context.operation = Some(crate::Operation::Read);
            let upload = find(id, entry.id)?;
            let body: serde_json::Value =
                serde_json::from_str(&crate::read_body(request, state.options.max_body)?)
                    .map_err(|e| HttpError::bad_request(format!("The body: {e}")))?;
            let namespace = match body.get("namespace").and_then(|n| n.as_str()) {
                Some(n) => checked_namespace(n)?,
                None => crate::import_draft::default_namespace(&upload.name),
            };
            let mut keys = std::collections::HashMap::new();
            if let Some(given) = body.get("keys").and_then(|k| k.as_object()) {
                for (kind, column) in given {
                    keys.insert(kind.clone(), column.as_str().map(str::to_owned));
                }
            }
            let reading = engine
                .with_upload(&upload.name, Arc::clone(&upload.bytes))
                .map_err(|e| HttpError::bad_request(e.to_string()))?;
            let (reading, _deadline) = crate::with_deadline(&reading, timeout);
            let triples = tree(&reading, &upload)?;
            let d = crate::import_draft::draft(
                &upload.name,
                &upload.service(),
                &namespace,
                &keys,
                &triples,
            );
            let body = serde_json::json!({
                "namespace": namespace,
                "kinds": d.kinds.iter().map(crate::import_draft::KindInfo::json).collect::<Vec<_>>(),
                "query": d.query,
            });
            Ok(Reply::bytes(
                200,
                "application/json",
                body.to_string().into_bytes(),
            ))
        }
        (Method::Post, []) => {
            context.operation = Some(crate::Operation::Read);
            let name = params
                .get("name")
                .ok_or_else(|| HttpError::bad_request("Give the file's name: name=FILE"))?;
            let bytes: Arc<[u8]> = crate::read_bytes(request, state.options.max_body)?.into();
            let options = match params.get("options") {
                Some(o) => o.trim().trim_matches(',').to_owned(),
                // A CSV file's first row names its columns, as in most files people drop.
                None if [".csv", ".tsv"]
                    .iter()
                    .any(|e| name.to_ascii_lowercase().ends_with(e)) =>
                {
                    "csv.headers=true".to_owned()
                }
                None => String::new(),
            };
            if options.contains(['<', '>', ' ', '{', '}']) {
                return Err(HttpError::bad_request(
                    "options: name=value pairs, joined with commas",
                ));
            }
            // The upload is the source: options that name another are refused.
            if let Some(key) = options
                .split(',')
                .filter_map(|o| o.split('=').next())
                .find(|k| SOURCE_KEYS.contains(&k.trim()))
            {
                return Err(HttpError::bad_request(format!(
                    "options: {key} names another source; the upload is the source"
                )));
            }
            let upload = Upload {
                dataset: entry.id,
                name: name.to_owned(),
                options,
                bytes,
                used: Mutex::new(Instant::now()),
            };
            let reading = engine
                .with_upload(&upload.name, Arc::clone(&upload.bytes))
                .map_err(|e| HttpError::bad_request(e.to_string()))?;
            let (reading, _deadline) = crate::with_deadline(&reading, timeout);
            let triples = tree(&reading, &upload)?;
            let namespace = match params.get("namespace") {
                Some(n) => checked_namespace(n)?,
                None => crate::import_draft::default_namespace(&upload.name),
            };
            let d = crate::import_draft::draft(
                &upload.name,
                &upload.service(),
                &namespace,
                &std::collections::HashMap::new(),
                &triples,
            );
            let id = random_id()?;
            let body = serde_json::json!({
                "id": id,
                "name": upload.name,
                "bytes": upload.bytes.len(),
                "options": upload.options,
                "location": format!("upload:{}", upload.name),
                "tree": triples
                    .iter()
                    .take(SHOWN)
                    .map(|t| format!("{} {} {} .\n", t.0, t.1, t.2))
                    .collect::<String>(),
                "triples": triples.len().min(SHOWN),
                "truncated": triples.len() > SHOWN,
                "namespace": namespace,
                "kinds": d.kinds.iter().map(crate::import_draft::KindInfo::json).collect::<Vec<_>>(),
                "schema": schema(&upload.bytes),
                "query": d.query,
            });
            keep(id, upload);
            let mut reply = Reply::bytes(201, "application/json", body.to_string().into_bytes());
            reply.headers.push(("Cache-Control", "no-store".to_owned()));
            Ok(reply)
        }
        (Method::Delete, [id]) => {
            context.operation = Some(crate::Operation::Read);
            let mut kept = UPLOADS.lock().unwrap_or_else(|e| e.into_inner());
            let before = kept.len();
            kept.retain(|(k, u)| !(k == id && u.dataset == entry.id));
            if kept.len() == before {
                return Err(gone());
            }
            Ok(Reply::empty(204))
        }
        (Method::Post, [id, step @ ("preview" | "run")]) => {
            let upload = find(id, entry.id)?;
            let query = reads_upload(
                &crate::read_body(request, state.options.max_body)?,
                &upload.name,
            );
            let reading = engine
                .with_upload(&upload.name, Arc::clone(&upload.bytes))
                .map_err(|e| HttpError::bad_request(e.to_string()))?;
            let base_iri = format!("{}/{}/import", crate::origin(state, request), entry.name);
            let parsed = spargebra::SparqlParser::new()
                .with_base_iri(&base_iri)
                .map_err(|e| HttpError(500, e.to_string()))?
                .parse_query(&query)
                .map_err(|e| HttpError::bad_request(format!("The query: {e}")))?;
            let spargebra::Query::Construct {
                template,
                pattern,
                dataset: None,
                base_iri,
            } = parsed
            else {
                return Err(HttpError::bad_request(
                    "The mapping is a CONSTRUCT query, without FROM",
                ));
            };
            if *step == "preview" {
                context.operation = Some(crate::Operation::Query);
                let budget = crate::RowBudget::of(params, context.limits.max_rows)?;
                return crate::run_query(
                    &reading,
                    &query,
                    request,
                    params,
                    timeout,
                    budget,
                    tripletrail::QueryOptions::default(),
                )
                .map_err(caller_error);
            }
            context.operation = Some(crate::Operation::Update);
            let graph = params
                .get("graph")
                .ok_or_else(|| HttpError::bad_request("Give the graph to add to: graph=IRI"))?;
            let graph = tripletrail::oxigraph::model::NamedNode::new(graph)
                .map_err(|e| HttpError::bad_request(format!("graph: {e}")))?;
            crate::update_within_quota(context, || false)?;
            let insert = template
                .into_iter()
                .map(|t| spargebra::term::QuadPattern {
                    subject: t.subject,
                    predicate: t.predicate,
                    object: t.object,
                    graph_name: spargebra::term::GraphNamePattern::NamedNode(graph.clone()),
                })
                .collect();
            let update = spargebra::Update {
                base_iri,
                operations: vec![spargebra::GraphUpdateOperation::DeleteInsert {
                    delete: Vec::new(),
                    insert,
                    using: None,
                    pattern: Box::new(pattern),
                }],
            };
            let count = |e: &Engine| -> Result<u64, HttpError> {
                let q = format!("SELECT (COUNT(*) AS ?n) {{ GRAPH {graph} {{ ?s ?p ?o }} }}");
                let QueryResults::Solutions(mut s) = e.query(&q)? else {
                    unreachable!("a SELECT")
                };
                Ok(
                    match s.next().transpose().map_err(tripletrail::Error::from)? {
                        Some(row) => match row.get("n") {
                            Some(Term::Literal(l)) => l.value().parse().unwrap_or(0),
                            _ => 0,
                        },
                        None => 0,
                    },
                )
            };
            let before = count(engine)?;
            let (reading, _deadline) = crate::with_deadline(&reading, timeout);
            reading
                .update_with_options(
                    &update.to_string(),
                    &tripletrail::UpdateOptions {
                        base_iri: None,
                        using: None,
                        readable: context.readable.clone(),
                        writable: context.writable.clone(),
                        row_rules: context.row_rules.clone(),
                    },
                )
                .map_err(|e| caller_error(e.into()))?;
            let after = count(engine)?;
            let body = serde_json::json!({
                "graph": graph.as_str(),
                "before": before,
                "after": after,
            });
            Ok(Reply::bytes(
                200,
                "application/json",
                body.to_string().into_bytes(),
            ))
        }
        (method, _) => Err(HttpError(
            if matches!(method, Method::Get | Method::Head) && !rest.is_empty() {
                405
            } else {
                404
            },
            format!("{method} is not answered here"),
        )),
    }
}

/// The query with `location=NAME` and `from-archive=NAME` of the upload's own name read from
/// the upload (`upload:NAME`), so a lift query that names the file runs on it as it is.
fn reads_upload(query: &str, name: &str) -> String {
    let mut out = query.to_owned();
    for key in ["location=", "from-archive="] {
        let plain = format!("{key}{name}");
        let mut at = 0;
        while let Some(found) = out[at..].find(&plain) {
            let start = at + found;
            let end = start + plain.len();
            let before_ok = start == 0 || matches!(out.as_bytes()[start - 1], b':' | b',' | b'<');
            let after_ok = matches!(out.as_bytes().get(end), Some(b',' | b'>' | b'&'));
            if before_ok && after_ok {
                out.replace_range(start..end, &format!("{key}upload:{name}"));
                at = end + "upload:".len();
            } else {
                at = end;
            }
        }
    }
    out
}

/// A source the mapping may not read (a file outside the allowed folders, a URL, another
/// upload) or cannot read is the caller's query at fault, not the server: 400, not 500.
fn caller_error(e: HttpError) -> HttpError {
    if e.0 >= 500 && e.1.contains("Facade-X") {
        HttpError(400, e.1)
    } else {
        e
    }
}

fn gone() -> HttpError {
    HttpError(404, "No such upload: it may have expired".to_owned())
}

/// The upload `id` of dataset `dataset`, marked as used now.
fn find(id: &str, dataset: u64) -> Result<Arc<Upload>, HttpError> {
    let mut kept = UPLOADS.lock().unwrap_or_else(|e| e.into_inner());
    kept.retain(|(_, u)| u.idle() < LIFE);
    let upload = kept
        .iter()
        .find(|(k, u)| k == id && u.dataset == dataset)
        .map(|(_, u)| Arc::clone(u))
        .ok_or_else(gone)?;
    *upload.used.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    Ok(upload)
}

/// Keeps `upload` as `id`, dropping expired uploads and then the least recently used while
/// there are too many or they take too much memory.
fn keep(id: String, upload: Upload) {
    let mut kept = UPLOADS.lock().unwrap_or_else(|e| e.into_inner());
    kept.retain(|(_, u)| u.idle() < LIFE);
    kept.push_back((id, Arc::new(upload)));
    let total = |k: &VecDeque<(String, Arc<Upload>)>| -> usize {
        k.iter().map(|(_, u)| u.bytes.len()).sum()
    };
    while kept.len() > 1 && (kept.len() > MAX_UPLOADS || total(&kept) > MAX_TOTAL) {
        let oldest = kept
            .iter()
            .enumerate()
            .take(kept.len() - 1)
            .max_by_key(|(_, (_, u))| u.idle())
            .map(|(i, _)| i)
            .expect("more than one");
        kept.remove(oldest);
    }
}

fn random_id() -> Result<String, HttpError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| HttpError(500, e.to_string()))?;
    Ok(bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    }))
}

/// The first triples of the upload's Facade-X graph.
fn tree(engine: &Engine, upload: &Upload) -> Result<Vec<(Term, Term, Term)>, HttpError> {
    let q = format!(
        "SELECT ?s ?p ?o WHERE {{ SERVICE <{}> {{ ?s ?p ?o }} }} LIMIT {TREE_TRIPLES}",
        upload.service()
    );
    let QueryResults::Solutions(solutions) = engine
        .query(&q)
        .map_err(|e| HttpError(422, format!("The file cannot be read: {e}")))?
    else {
        unreachable!("a SELECT")
    };
    let mut out = Vec::new();
    for row in solutions {
        let row = row.map_err(|e| HttpError(422, format!("The file cannot be read: {e}")))?;
        if let (Some(s), Some(p), Some(o)) = (row.get("s"), row.get("p"), row.get("o")) {
            out.push((s.clone(), p.clone(), o.clone()));
        }
    }
    Ok(out)
}

/// The namespace the caller gives: an absolute IRI that a SPARQL `PREFIX` can hold.
fn checked_namespace(n: &str) -> Result<String, HttpError> {
    let n = n.trim();
    tripletrail::oxigraph::model::NamedNode::new(n)
        .map_err(|e| HttpError::bad_request(format!("namespace: {e}")))?;
    if n.contains(['<', '>', '"', ' ', '{', '}', '|', '\\', '^', '`']) {
        return Err(HttpError::bad_request("namespace: not an IRI"));
    }
    // Names are appended to it: one that ends in neither `/` nor `#` gets a `/`.
    let mut n = n.to_owned();
    if !n.ends_with(['/', '#']) {
        n.push('/');
    }
    Ok(n)
}

/// The schema the file names, if any, for the page to offer generating an ontology and shapes
/// from it (#276): an XML Schema (`xsi:schemaLocation`, `xsi:noNamespaceSchemaLocation`), a
/// JSON Schema (`"$schema"`), or the file is an OpenAPI document. Read from its first 64 KiB.
fn schema(bytes: &[u8]) -> Option<serde_json::Value> {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(64 << 10)]);
    let quoted = |after: &str| -> Option<String> {
        let rest = after.trim_start().strip_prefix(['=', ':'])?.trim_start();
        let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let rest = &rest[1..];
        Some(rest[..rest.find(quote)?].trim().to_owned())
    };
    for key in ["noNamespaceSchemaLocation", "schemaLocation"] {
        if let Some(at) = head.find(&format!(":{key}"))
            && let Some(v) = quoted(&head[at + key.len() + 1..])
        {
            // schemaLocation is pairs of namespace and location: the location is the last.
            let location = v.split_whitespace().last().unwrap_or_default().to_owned();
            return Some(serde_json::json!({ "kind": "xsd", "location": location }));
        }
    }
    if let Some(at) = head.find("\"$schema\"")
        && let Some(v) = quoted(&head[at + 9..])
    {
        return Some(serde_json::json!({ "kind": "json-schema", "location": v }));
    }
    let start = head.trim_start_matches('\u{feff}').trim_start();
    if start.starts_with("openapi:")
        || start.starts_with("swagger:")
        || (start.starts_with('{')
            && (start[..start.len().min(200)].contains("\"openapi\"")
                || start[..start.len().min(200)].contains("\"swagger\"")))
    {
        return Some(serde_json::json!({ "kind": "openapi", "location": null }));
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_query_naming_the_file_reads_the_upload() {
        let q = "SERVICE <x-sparql-anything:location=a.csv,csv.headers=true> {} \
                 SERVICE <x-sparql-anything:location=a.csv> {} \
                 SERVICE <x-sparql-anything:from-archive=a.csv,location=x.csv> {} \
                 SERVICE <x-sparql-anything:location=a.csv.bak> {} \
                 SERVICE <x-sparql-anything:location=upload:a.csv> {}";
        assert_eq!(
            super::reads_upload(q, "a.csv"),
            "SERVICE <x-sparql-anything:location=upload:a.csv,csv.headers=true> {} \
             SERVICE <x-sparql-anything:location=upload:a.csv> {} \
             SERVICE <x-sparql-anything:from-archive=upload:a.csv,location=x.csv> {} \
             SERVICE <x-sparql-anything:location=a.csv.bak> {} \
             SERVICE <x-sparql-anything:location=upload:a.csv> {}"
        );
    }
}
