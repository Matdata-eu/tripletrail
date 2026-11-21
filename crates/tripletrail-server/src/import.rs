//! The Import page's endpoints (#20): a file sent to the server is kept in memory, never
//! written to disk, and read as Facade-X (`SERVICE <x-sparql-anything:location=upload:NAME>`)
//! by the requests that name it. Writers only, as running a mapping writes the dataset.
//!
//! - `POST /{ds}/import?name=FILE[&options=…]` with the file as the body: keeps it for an hour
//!   and answers `{id, name, bytes, options, location, tree, triples, truncated, query}`: the
//!   first triples of its Facade-X graph and a CONSTRUCT that maps each key of its records.
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
/// Keys a drafted query maps at most.
const MAX_KEYS: usize = 60;

const FX: &str = "http://sparql.xyz/facade-x/ns/";
const XYZ: &str = "http://sparql.xyz/facade-x/data/";
const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

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
                "query": draft(&upload, &triples),
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
                );
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
            reading.update_with_options(
                &update.to_string(),
                &tripletrail::UpdateOptions {
                    base_iri: None,
                    using: None,
                    readable: context.readable.clone(),
                    writable: context.writable.clone(),
                    row_rules: context.row_rules.clone(),
                },
            )?;
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
            if matches!(method, Method::Get | Method::Head) {
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

/// A CONSTRUCT that maps every key of the upload's records: the nodes in the slots of one
/// container that have the most keys (the rows of a table, the objects of a JSON array), or
/// the root when none has keys. Records in the root's slots get IRIs from their position;
/// others keep their nodes.
fn draft(upload: &Upload, triples: &[(Term, Term, Term)]) -> String {
    let is = |t: &Term, iri: &str| matches!(t, Term::NamedNode(n) if n.as_str() == iri);
    let root = triples
        .iter()
        .find(|(_, p, o)| is(p, &format!("{RDF}type")) && is(o, &format!("{FX}Root")))
        .map(|(s, _, _)| s.clone());
    let keys_of = |node: &Term| -> Vec<String> {
        triples
            .iter()
            .filter(|(s, _, _)| s == node)
            .filter_map(|(_, p, _)| match p {
                Term::NamedNode(n) if n.as_str().starts_with(XYZ) => Some(n.as_str().to_owned()),
                _ => None,
            })
            .collect()
    };
    // Per container, its members that have keys.
    let mut parents: Vec<(Term, Vec<Term>)> = Vec::new();
    for (s, p, o) in triples {
        let slot = matches!(p, Term::NamedNode(n) if n.as_str().strip_prefix(RDF).is_some_and(|r| r.starts_with('_')));
        if slot && !keys_of(o).is_empty() {
            match parents.iter_mut().find(|(parent, _)| parent == s) {
                Some((_, members)) => members.push(o.clone()),
                None => parents.push((s.clone(), vec![o.clone()])),
            }
        }
    }
    let records = parents.into_iter().max_by_key(|(_, m)| m.len());
    let (in_root, members) = match &records {
        Some((parent, members)) => (Some(parent) == root.as_ref(), members.clone()),
        None => (true, root.iter().cloned().collect()),
    };
    let mut keys: Vec<String> = Vec::new();
    for m in &members {
        for k in keys_of(m) {
            if !keys.contains(&k) && keys.len() < MAX_KEYS {
                keys.push(k);
            }
        }
    }
    let mut names: Vec<String> = Vec::new();
    for k in &keys {
        let local = &k[XYZ.len()..];
        let local = crate::ogc::percent_decode(local).unwrap_or_else(|| local.to_owned());
        let mut name: String = local
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
            name.insert(0, 'k');
        }
        // Kept apart from the query's own variables.
        while names.contains(&name)
            || ["item", "record", "root", "parent", "slot"].contains(&name.as_str())
        {
            name.push('_');
        }
        names.push(name);
    }
    let mut q = String::from(
        "PREFIX fx: <http://sparql.xyz/facade-x/ns/>\n\
         PREFIX xyz: <http://sparql.xyz/facade-x/data/>\n\
         PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>\n\
         PREFIX ex: <https://example.org/import/>\n\n",
    );
    let subject = if records.is_some() && in_root {
        "?item"
    } else {
        "?record"
    };
    q.push_str("CONSTRUCT {\n");
    let _ = write!(q, "  {subject} a ex:Record");
    for n in &names {
        let _ = write!(q, " ;\n    ex:{n} ?{n}");
    }
    q.push_str(" .\n} WHERE {\n");
    let _ = writeln!(q, "  SERVICE <{}> {{", upload.service());
    match (&records, in_root) {
        (Some(_), true) => {
            q.push_str("    ?root a fx:Root ; ?slot ?record .\n");
            q.push_str("    FILTER(STRSTARTS(STR(?slot), STR(rdf:_)))\n");
        }
        (Some(_), false) => {
            q.push_str("    ?parent ?slot ?record .\n");
            q.push_str("    FILTER(STRSTARTS(STR(?slot), STR(rdf:_)))\n");
        }
        (None, _) => q.push_str("    ?record a fx:Root .\n"),
    }
    for (k, n) in keys.iter().zip(&names) {
        let _ = writeln!(q, "    OPTIONAL {{ ?record <{k}> ?{n} }}");
    }
    q.push_str("  }\n");
    if subject == "?item" {
        q.push_str(
            "  BIND(IRI(CONCAT(STR(ex:), \"record/\", STRAFTER(STR(?slot), STR(rdf:_)))) AS ?item)\n",
        );
    }
    q.push_str("}\n");
    q
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
