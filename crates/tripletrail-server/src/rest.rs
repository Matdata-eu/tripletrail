//! `/{ds}/api`: a JSON-LD REST API generated from the shapes the `rest-api` reader reads
//! (#143), the generated ones when the dataset has none of its own.
//!
//! - `GET /{ds}/api/v1`: the datasets, one per node shape with a target class.
//! - `GET /{ds}/api/v1/{dataset}`: a page of its records, filtered by `?{key}=value`, `q`
//!   (text in the label) and `bbox` (CRS84, when the shape has a geometry).
//! - `GET /{ds}/api/v1/{dataset}/{id}`: one record; `id` is the percent-encoded IRI.
//! - `GET /{ds}/api/openapi.json` (and `.yaml`): an OpenAPI 3.1 definition of the same.
//!
//! A record has a key per property shape: `tts:key`, else the predicate's local name
//! without a leading `has`/`is`, in lower camel case. `sh:maxCount 1` gives one value,
//! otherwise a list; an IRI of a class that is a dataset a reference (`id`, `type`,
//! `label`); `rdf:langString` a language map; numbers and booleans JSON's own. Its
//! `@context` maps the keys back to the predicates (`@reverse` for an inverse path), so the
//! JSON is RDF. Records are written with `POST`, `PUT`, `PATCH` and `DELETE` (see
//! [`write`]).

pub(crate) mod graphql;
pub(crate) mod search;
mod write;
pub(crate) use write::{Method as WriteMethod, Request as WriteRequest, percent_encode, write};

use crate::View;
use crate::admin::Entry;
use crate::ogc::{
    local_name, number, percent_decode, property_schema, run, sparql_path, subject, value,
};
use crate::{HttpError, Params, Reply, State};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tripletrail::oxigraph::model::{
    Graph, NamedNode, NamedOrBlankNode, NamedOrBlankNodeRef, Term, TermRef,
};
use tripletrail::shapes::Reader;

const SH: &str = "http://www.w3.org/ns/shacl#";
const TTS: &str = "http://w3id.org/tripletrail/shapes#";
const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
const LANG_STRING: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#langString";
const GEOF: &str = "http://www.opengis.net/def/function/geosparql/";
const WKT: &str = "http://www.opengis.net/ont/geosparql#wktLiteral";
const JSON_LD: &str = "application/ld+json";
const JSON: &str = "application/json";
const GEOJSON: &str = "application/geo+json";
const CSV: &str = "text/csv; charset=utf-8";
const OPENAPI: &str = "application/vnd.oai.openapi+json;version=3.1";
const OPENAPI_YAML: &str = "application/vnd.oai.openapi;version=3.1";
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 10_000;
/// Values a record gives of one key at most; `truncated` says how many it has.
const MAX_VALUES: usize = 1_000;
/// Values one answer gives at most; a page that has more is refused (413).
const MAX_PAGE_VALUES: u64 = 200_000;
/// Parameters of a list of records; any other is a key to filter on.
const LIST_PARAMS: &[&str] = &["f", "limit", "offset", "q", "bbox", "timeout"];
/// Keys a record uses itself: a property shape with one of them gets a `_` after it.
/// How deep embedded objects go: a record's values, and their values.
const EMBED_DEPTH: usize = 2;
const RESERVED: &[&str] = &["id", "type", "truncated", "@context"];

/// A dataset of the API: a node shape with a target class.
#[derive(Clone)]
struct ApiSet {
    slug: String,
    /// The node shape.
    node: Term,
    class: NamedNode,
    title: String,
    description: Option<String>,
    /// `tts:labelPath`, in SPARQL.
    label: Option<String>,
    /// The geometry ways (see [`crate::geometry_ways`]): what `bbox` filters on.
    pub(crate) geometry: Option<crate::geometry_ways::Ways>,
    instances: Option<u64>,
    /// `tts:iriTemplate` (#82): how a new record's IRI is made, with `{uuid}` and `{key}`.
    iri_template: Option<String>,
    fields: Vec<Field>,
}

/// A key of a record: a property shape.
#[derive(Clone)]
struct Field {
    key: String,
    /// The path, in SPARQL.
    path: String,
    /// The predicate when the path is one, or its inverse (`true`); else the key is left
    /// out of the `@context`.
    predicate: Option<(NamedNode, bool)>,
    /// `sh:maxCount 1`.
    single: bool,
    /// `sh:minCount` of 1 or more (#82).
    required: bool,
    datatype: Option<NamedNode>,
    /// Values are IRIs (`sh:nodeKind sh:IRI` or `sh:class`).
    iri: bool,
    /// `sh:class`.
    class: Option<NamedNode>,
    /// The JSON Schema of one value.
    schema: Value,
    /// The keys of the values' node shape: blank nodes, and IRIs that are no dataset's
    /// records, are embedded objects with these keys.
    embed: Option<Arc<Vec<Field>>>,
}

impl Field {
    fn lang(&self) -> bool {
        self.datatype
            .as_ref()
            .is_some_and(|d| d.as_str() == LANG_STRING)
    }
}

type Sets = Arc<Vec<ApiSet>>;
type SetCache = HashMap<(u64, Option<u64>, &'static str), (crate::ogc::DataVersion, Sets)>;
/// The datasets per (dataset, grants' fingerprint), with the data version they were made for.
static SETS: Mutex<Option<SetCache>> = Mutex::new(None);
/// Sets of grants whose datasets are kept, over all datasets.
const SETS_KEPT: usize = 64;

/// Answers `GET /{ds}/api/{rest}`; `base` is the absolute URL of `/{ds}/api`. `view`: the
/// graphs and rows the caller may read, when not all (#203); everything is read from those.
/// `accept`: the client's `Accept` media types, best first; `f` wins over them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn route(
    state: &State,
    entry: &Entry,
    rest: &[&str],
    params: &Params,
    base: &str,
    timeout: Option<Duration>,
    view: &View,
    accept: &[String],
) -> Result<Reply, HttpError> {
    let only = |allowed: &[&str]| -> Result<(), HttpError> {
        match params
            .0
            .iter()
            // `preview` (a preview's shapes) is read for every resource, by `PreviewScope`.
            .find(|(n, _)| n != "preview" && !allowed.contains(&n.as_str()))
        {
            Some((name, _)) => Err(HttpError::bad_request(format!(
                "Unknown parameter {name}: this resource takes {}",
                allowed.join(", ")
            ))),
            None => Ok(()),
        }
    };
    // JSON-LD unless `f` asks for another.
    let format = match params.get("f") {
        None => accept
            .iter()
            .find_map(|t| match t.as_str() {
                "application/ld+json" => Some(Format::LinkedData),
                "application/json" => Some(Format::Json),
                "application/geo+json" => Some(Format::GeoJson),
                "text/csv" => Some(Format::Csv),
                _ => None,
            })
            .unwrap_or(Format::LinkedData),
        Some("jsonld") => Format::LinkedData,
        Some("json") => Format::Json,
        Some("geojson") => Format::GeoJson,
        Some("csv") => Format::Csv,
        Some(f) => {
            return Err(HttpError(
                406,
                format!("Unsupported format {f}: one of jsonld, json, geojson, csv"),
            ));
        }
    };
    let linked = format == Format::LinkedData;
    match rest {
        [] | ["v1"] => {
            only(&["f"])?;
            let sets = sets(state, entry, view)?;
            Ok(reply(false, &index(base, &sets, state.options.ui)))
        }
        ["openapi.json"] => {
            only(&[])?;
            let sets = sets(state, entry, view)?;
            Ok(Reply::bytes(
                200,
                OPENAPI,
                openapi(entry, base, &sets).to_string().into_bytes(),
            ))
        }
        ["openapi.yaml"] => {
            only(&[])?;
            let sets = sets(state, entry, view)?;
            let mut out = String::new();
            yaml_rust2::YamlEmitter::new(&mut out)
                .dump(&yaml(&openapi(entry, base, &sets)))
                .map_err(|e| HttpError(500, format!("Writing the YAML failed: {e:?}")))?;
            out.push('\n');
            Ok(Reply::bytes(200, OPENAPI_YAML, out.into_bytes()))
        }
        ["v1", slug] => {
            let sets = sets(state, entry, view)?;
            let set = find(&sets, slug)?;
            let mut allowed = LIST_PARAMS.to_vec();
            allowed.extend(set.fields.iter().map(|f| f.key.as_str()));
            only(&allowed)?;
            let mut body = list(entry, &sets, set, params, base, timeout, view, linked)?;
            match format {
                Format::LinkedData | Format::Json => Ok(reply(linked, &body)),
                Format::GeoJson => {
                    let records = take_records(&mut body);
                    let mut collection = json!({ "type": "FeatureCollection", "features": features(entry, set, records, timeout, view)? });
                    for link in ["next", "prev"] {
                        if let Some(url) = body.get(link) {
                            collection[link] = url.clone();
                        }
                    }
                    Ok(Reply::bytes(
                        200,
                        GEOJSON,
                        collection.to_string().into_bytes(),
                    ))
                }
                Format::Csv => {
                    let records = take_records(&mut body);
                    let mut reply = Reply::bytes(200, CSV, csv(set, &records).into_bytes());
                    let links: Vec<String> = ["next", "prev"]
                        .iter()
                        .filter_map(|rel| {
                            Some(format!("<{}>; rel=\"{rel}\"", body.get(*rel)?.as_str()?))
                        })
                        .collect();
                    if !links.is_empty() {
                        reply.headers.push(("Link", links.join(", ")));
                    }
                    Ok(reply)
                }
            }
        }
        // The id is the percent-encoded IRI, its slashes too: the router drops empty path
        // segments, so `http://…` unencoded would lose a slash.
        ["v1", slug, id] => {
            only(&["f", "timeout"])?;
            let sets = sets(state, entry, view)?;
            let set = find(&sets, slug)?;
            let iri = percent_decode(id)
                .and_then(|s| NamedNode::new(s).ok())
                .ok_or_else(|| HttpError(404, format!("No record {id}")))?;
            let is = format!(
                "SELECT ?f WHERE {{ VALUES ?f {{ {iri} }} ?f a <{}> }}",
                set.class.as_str()
            );
            if run(entry, &is, timeout, view)?.is_empty() {
                return Err(HttpError(404, format!("No record {id} in {slug}")));
            }
            let mut records = records(entry, &sets, set, &[iri], timeout, view)?;
            match format {
                Format::GeoJson => {
                    let feature = features(entry, set, records, timeout, view)?
                        .pop()
                        .expect("one");
                    return Ok(Reply::bytes(200, GEOJSON, feature.to_string().into_bytes()));
                }
                Format::Csv => {
                    return Ok(Reply::bytes(200, CSV, csv(set, &records).into_bytes()));
                }
                _ => {}
            }
            let mut record = records.pop().expect("one");
            let tag = record_tag(&record);
            if linked && let Value::Object(map) = &mut record {
                let mut out = Map::new();
                out.insert("@context".into(), context(&sets, set));
                out.append(map);
                record = Value::Object(out);
            }
            let mut reply = reply(linked, &record);
            // A strong tag is of one representation: plain JSON gets its own.
            reply
                .headers
                .push(("ETag", if linked { tag } else { plain_tag(&tag) }));
            Ok(reply)
        }
        _ => Err(HttpError(404, "Not found".to_owned())),
    }
}

/// A record's `ETag`: from its keys' values, so it changes when any of them does, and a
/// write with `If-Match` is refused once the record changed since it was read.
fn record_tag(record: &Value) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(record.to_string().as_bytes());
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("\"{hex}\"")
}

/// The `ETag` of a record as plain JSON (`f=json`), from its JSON-LD one.
fn plain_tag(tag: &str) -> String {
    format!("{}-json\"", tag.trim_end_matches('"'))
}

/// What an answer is written as (`f`).
#[derive(Clone, Copy, PartialEq)]
enum Format {
    LinkedData,
    Json,
    GeoJson,
    Csv,
}

fn take_records(body: &mut Value) -> Vec<Value> {
    match body.get_mut("records").map(Value::take) {
        Some(Value::Array(records)) => records,
        _ => Vec::new(),
    }
}

/// The records as GeoJSON features: the geometry is the value of the dataset's geometry
/// path (several as a collection), the rest of the record its properties.
fn features(
    entry: &Entry,
    set: &ApiSet,
    records: Vec<Value>,
    timeout: Option<Duration>,
    view: &View,
) -> Result<Vec<Value>, HttpError> {
    let Some(ways) = &set.geometry else {
        return Err(HttpError::bad_request(format!(
            "{} has no geometry: its shape has no tts:geometry",
            set.slug
        )));
    };
    let ids: Vec<String> = records
        .iter()
        .filter_map(|r| Some(format!("<{}>", r.get("id")?.as_str()?)))
        .collect();
    let mut found: HashMap<String, Vec<String>> = HashMap::new();
    if !ids.is_empty() {
        let query = format!(
            "SELECT DISTINCT ?f (<{GEOF}asGeoJSON>(?w) AS ?g) WHERE {{ VALUES ?f {{ {} }} {} }}",
            ids.join(" "),
            ways.only("?f", &ids).pattern("?f", "?w")
        );
        for row in run(entry, &query, timeout, view)? {
            if let (Some(Term::NamedNode(f)), Some(Term::Literal(g))) = (&row[0], &row[1]) {
                found
                    .entry(f.as_str().to_owned())
                    .or_default()
                    .push(g.value().to_owned());
            }
        }
    }
    Ok(records
        .into_iter()
        .map(|mut record| {
            let id = record
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let mut texts = found.remove(&id).unwrap_or_default();
            texts.sort();
            let mut parts: Vec<Value> = texts
                .iter()
                .filter_map(|t| serde_json::from_str(t).ok())
                .collect();
            let geometry = match parts.len() {
                0 => Value::Null,
                1 => parts.pop().expect("one"),
                _ => json!({ "type": "GeometryCollection", "geometries": parts }),
            };
            if let Value::Object(map) = &mut record {
                map.remove("id");
            }
            json!({ "type": "Feature", "id": id, "geometry": geometry, "properties": record })
        })
        .collect())
}

/// The records as CSV: a column per key, `id` first. Several values are joined with ` | `,
/// a reference is its IRI, and a language string is `text@lang`.
fn csv(set: &ApiSet, records: &[Value]) -> String {
    fn text(value: &Value, lang: bool) -> Vec<String> {
        match value {
            Value::Null => Vec::new(),
            Value::String(s) => vec![s.clone()],
            Value::Array(values) => values.iter().flat_map(|v| text(v, lang)).collect(),
            Value::Object(map) if map.contains_key("id") => text(&map["id"], false),
            // An embedded blank node: its keys, as JSON.
            Value::Object(map) if !lang => vec![Value::Object(map.clone()).to_string()],
            Value::Object(map) => map
                .iter()
                .flat_map(|(lang, v)| {
                    text(v, false).into_iter().map(move |t| {
                        if lang == "@none" {
                            t
                        } else {
                            format!("{t}@{lang}")
                        }
                    })
                })
                .collect(),
            other => vec![other.to_string()],
        }
    }
    fn cell(out: &mut String, value: &str) {
        // A spreadsheet would run a cell that starts like a formula: such text gets a `'`
        // before it, numbers stay as they are.
        let formula =
            value.starts_with(['=', '+', '-', '@', '\t', '\r']) && value.parse::<f64>().is_err();
        let quoted;
        let value = if formula {
            quoted = format!("'{value}");
            quoted.as_str()
        } else {
            value
        };
        if value.contains([',', '"', '\n', '\r']) {
            out.push('"');
            out.push_str(&value.replace('"', "\"\""));
            out.push('"');
        } else {
            out.push_str(value);
        }
    }
    let mut columns: Vec<&str> = vec!["id", "type"];
    columns.extend(set.fields.iter().map(|f| f.key.as_str()));
    let mut out = String::new();
    for (i, column) in columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        cell(&mut out, column);
    }
    out.push_str("\r\n");
    for record in records {
        for (i, column) in columns.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let value = record
                .get(*column)
                .map(|v| text(v, i >= 2 && set.fields[i - 2].lang()))
                .unwrap_or_default()
                .join(" | ");
            cell(&mut out, &value);
        }
        out.push_str("\r\n");
    }
    out
}

fn reply(linked: bool, body: &Value) -> Reply {
    Reply::bytes(
        200,
        if linked { JSON_LD } else { JSON },
        body.to_string().into_bytes(),
    )
}

fn find<'a>(sets: &'a [ApiSet], slug: &str) -> Result<&'a ApiSet, HttpError> {
    sets.iter()
        .find(|s| s.slug == slug)
        .ok_or_else(|| HttpError(404, format!("No dataset {slug}")))
}

/// `/{ds}/api/v1`: the datasets.
fn index(base: &str, sets: &[ApiSet], ui: bool) -> Value {
    let datasets: Vec<Value> = sets
        .iter()
        .map(|s| {
            let mut d = json!({
                "id": s.slug,
                "title": s.title,
                "class": s.class.as_str(),
                "href": format!("{base}/v1/{}", s.slug),
            });
            if let Some(description) = &s.description {
                d["description"] = description.as_str().into();
            }
            if let Some(n) = s.instances {
                d["count"] = n.into();
            }
            d
        })
        .collect();
    let mut out = json!({ "openapi": format!("{base}/openapi.json"), "datasets": datasets });
    if let Some((origin, ds)) = base.strip_suffix("/api").and_then(|b| b.rsplit_once('/')) {
        out["graphql"] = format!("{origin}/{ds}/graphql").into();
        // The docs page and GraphQL playground of the web UI, when the server has one.
        if ui {
            out["docs"] = format!("{origin}/$/ui/api/{ds}").into();
            out["playground"] = format!("{origin}/$/ui/graphql/{ds}").into();
        }
    }
    out
}

thread_local! {
    /// The shape files of a preview (`POST /{ds}/tiles/preview`) that this thread's request
    /// reads in place of the dataset's own, set for the request by [`PreviewScope`].
    static PREVIEW: std::cell::RefCell<Option<Arc<Vec<tripletrail::shapes::SetFile>>>> =
        const { std::cell::RefCell::new(None) };
}

/// While it lives, the REST API, search and GraphQL of this thread's request read a preview's
/// shapes (`preview=<id>` on the request): made with [`PreviewScope::of`], ended when dropped.
pub(crate) struct PreviewScope(());

impl PreviewScope {
    /// The scope of `params`' `preview=` for `entry`, if it has one; an unknown or expired id
    /// is a 404.
    pub(crate) fn of(entry: &Entry, params: &Params) -> Result<Option<Self>, HttpError> {
        let Some(id) = params.get("preview") else {
            return Ok(None);
        };
        let files = crate::tiles::preview_files(entry, id)?;
        PREVIEW.with(|p| *p.borrow_mut() = Some(Arc::new(files)));
        Ok(Some(Self(())))
    }
}

impl Drop for PreviewScope {
    fn drop(&mut self) {
        PREVIEW.with(|p| *p.borrow_mut() = None);
    }
}

/// The datasets the `rest-api` reader reads, that `view` sees; kept until the data or the
/// shapes change.
fn sets(state: &State, entry: &Entry, view: &View) -> Result<Sets, HttpError> {
    sets_for(state, entry, view, Reader::RestApi)
}

/// The datasets of `reader`'s projection of the shapes (the REST API's, or GraphQL's).
fn sets_for(state: &State, entry: &Entry, view: &View, reader: Reader) -> Result<Sets, HttpError> {
    // A preview's shapes, made each time: a preview is looked at, not served at volume.
    if let Some(files) = PREVIEW.with(|p| p.borrow().clone()) {
        let shapes = crate::shapes::projected_with(state, entry, reader, view, &files)?;
        return Ok(Arc::new(make_sets(&shapes)));
    }
    let version = crate::ogc::data_version(state, entry);
    let key = (entry.id, view.fingerprint(), reader.name());
    if let Some(version) = version
        && let Some((made_for, sets)) = SETS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|c| c.get(&key))
        && *made_for == version
    {
        return Ok(Arc::clone(sets));
    }
    let shapes = crate::shapes::projected(state, entry, reader, view)?;
    let sets = Arc::new(make_sets(&shapes));
    if let Some(version) = version {
        let mut cache = SETS.lock().unwrap_or_else(|e| e.into_inner());
        let cache = cache.get_or_insert_with(HashMap::new);
        if cache.len() >= SETS_KEPT {
            cache.clear();
        }
        cache.insert(key, (version, Arc::clone(&sets)));
    }
    Ok(sets)
}

/// The datasets of a projected shape graph: one per node shape with a target class, in slug
/// order; a slug taken twice keeps the first.
fn make_sets(shapes: &Graph) -> Vec<ApiSet> {
    let sh = |local: &str| NamedNode::new_unchecked(format!("{SH}{local}"));
    let tts = |local: &str| NamedNode::new_unchecked(format!("{TTS}{local}"));
    let text = |s: NamedOrBlankNodeRef<'_>, p: &NamedNode| -> Option<String> {
        match shapes.object_for_subject_predicate(s, p)? {
            TermRef::Literal(l) => Some(l.value().to_owned()),
            _ => None,
        }
    };
    let target = sh("targetClass");
    let mut nodes: Vec<_> = shapes
        .triples_for_predicate(&target)
        .map(|t| t.subject.into_owned())
        .collect();
    nodes.sort_by_key(|n| n.to_string());
    nodes.dedup();
    // The node shape of each target class: the first in IRI order.
    let mut by_class: HashMap<NamedNode, NamedOrBlankNode> = HashMap::new();
    for node in &nodes {
        if let Some(TermRef::NamedNode(class)) =
            shapes.object_for_subject_predicate(node.as_ref(), &target)
        {
            by_class
                .entry(class.into_owned())
                .or_insert_with(|| node.clone());
        }
    }
    let mut memo = Embeds::new();
    let mut out: Vec<ApiSet> = Vec::new();
    let mut taken = HashSet::new();
    for node in nodes {
        let node = node.as_ref();
        let Some(TermRef::NamedNode(class)) = shapes.object_for_subject_predicate(node, &target)
        else {
            continue;
        };
        let slug = text(node, &tts("key")).unwrap_or_else(|| kebab(local_name(class.as_str())));
        if slug.is_empty()
            || !slug
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
            || !taken.insert(slug.clone())
        {
            continue;
        }
        let geometry = crate::geometry_ways::Ways::of(shapes, node);
        let label = shapes
            .object_for_subject_predicate(node, &tts("labelPath"))
            .and_then(|p| sparql_path(shapes, p));
        let fields = fields_of(shapes, &by_class, &mut memo, node, 0);
        out.push(ApiSet {
            node: node.into_owned().into(),
            title: text(node, &sh("name")).unwrap_or_else(|| slug.clone()),
            description: text(node, &sh("description")),
            slug,
            class: class.into_owned(),
            label,
            geometry,
            instances: text(node, &tts("instances")).and_then(|n| n.trim().parse().ok()),
            iri_template: text(node, &tts("iriTemplate")),
            fields,
        });
    }
    out.sort_by(|a, b| a.slug.cmp(&b.slug));
    out
}

/// The embedded keys of node shapes, by shape and depth.
type Embeds = HashMap<(NamedOrBlankNode, usize), Option<Arc<Vec<Field>>>>;

/// The keys of a node shape's records, one per property shape (in `sh:order`). Below
/// [`EMBED_DEPTH`], a key whose values have a node shape (`sh:node`, or the shape of its
/// `sh:class`) gets that shape's keys, for the values it embeds.
fn fields_of(
    shapes: &Graph,
    by_class: &HashMap<NamedNode, NamedOrBlankNode>,
    memo: &mut Embeds,
    node: NamedOrBlankNodeRef<'_>,
    depth: usize,
) -> Vec<Field> {
    let sh = |local: &str| NamedNode::new_unchecked(format!("{SH}{local}"));
    let tts = |local: &str| NamedNode::new_unchecked(format!("{TTS}{local}"));
    let text = |s: NamedOrBlankNodeRef<'_>, p: &NamedNode| -> Option<String> {
        match shapes.object_for_subject_predicate(s, p)? {
            TermRef::Literal(l) => Some(l.value().to_owned()),
            _ => None,
        }
    };
    let order = |s: NamedOrBlankNodeRef<'_>| -> i64 {
        text(s, &sh("order"))
            .and_then(|o| o.trim().parse().ok())
            .unwrap_or(i64::MAX)
    };
    let mut properties: Vec<_> = shapes
        .objects_for_subject_predicate(node, &sh("property"))
        .filter_map(subject)
        .collect();
    properties.sort_by_key(|p| (order(*p), p.to_string()));
    let mut keys: HashSet<String> = RESERVED.iter().map(|k| (*k).to_owned()).collect();
    let mut fields = Vec::new();
    for p in properties {
        let Some(path_term) = shapes.object_for_subject_predicate(p, &sh("path")) else {
            continue;
        };
        let Some(path) = sparql_path(shapes, path_term) else {
            continue;
        };
        let predicate = match path_term {
            TermRef::NamedNode(n) => Some((n.into_owned(), false)),
            TermRef::BlankNode(b) => {
                match shapes.object_for_subject_predicate(b, &sh("inversePath")) {
                    Some(TermRef::NamedNode(n)) => Some((n.into_owned(), true)),
                    _ => None,
                }
            }
            _ => None,
        };
        let mut key = text(p, &tts("key")).unwrap_or_else(|| match &predicate {
            Some((n, _)) => camel(local_name(n.as_str())),
            None => format!("p{}", fields.len()),
        });
        while RESERVED.contains(&key.as_str()) || LIST_PARAMS.contains(&key.as_str()) {
            key.push('_');
        }
        if !keys.insert(key.clone()) {
            continue;
        }
        let named = |p2: &str| match shapes.object_for_subject_predicate(p, &sh(p2)) {
            Some(TermRef::NamedNode(n)) => Some(n.into_owned()),
            _ => None,
        };
        let class = named("class");
        let iri = class.is_some() || named("nodeKind").is_some_and(|k| k == sh("IRI"));
        // The shape of the values: `sh:node` when it has keys, else their class's.
        let value_shape = shapes
            .objects_for_subject_predicate(p, &sh("node"))
            .filter_map(subject)
            .find(|s| {
                shapes
                    .object_for_subject_predicate(*s, &sh("property"))
                    .is_some()
            })
            .map(NamedOrBlankNodeRef::into_owned)
            .or_else(|| {
                // IRIs of a class with a node shape are references to its records.
                let iris_only = named("nodeKind").is_some_and(|k| k == sh("IRI"));
                if iris_only {
                    return None;
                }
                class.as_ref().and_then(|c| by_class.get(c)).cloned()
            });
        let embed = match value_shape {
            Some(s) if depth < EMBED_DEPTH => {
                let known = memo.get(&(s.clone(), depth + 1)).cloned();
                match known {
                    Some(known) => known,
                    None => {
                        let nested = fields_of(shapes, by_class, memo, s.as_ref(), depth + 1);
                        let nested = (!nested.is_empty()).then(|| Arc::new(nested));
                        memo.insert((s, depth + 1), nested.clone());
                        nested
                    }
                }
            }
            _ => None,
        };
        fields.push(Field {
            schema: property_schema(shapes, p, &key),
            key,
            path,
            predicate,
            single: text(p, &sh("maxCount")).is_some_and(|n| n.trim() == "1"),
            required: text(p, &sh("minCount"))
                .and_then(|n| n.trim().parse::<u64>().ok())
                .is_some_and(|n| n >= 1),
            datatype: named("datatype"),
            iri,
            class,
            embed,
        });
    }
    fields
}

/// A regular expression that finds `text` regardless of case and of the accents of Latin
/// letters, as the full-text index folds them: `Köln` → `k[oòóôõöøōŏő]ln`.
fn accent_free(text: &str) -> String {
    use tripletrail::text::fold;
    let mut out = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        let base = fold(c);
        let alike: Vec<char> = std::iter::once(base)
            .chain(
                ('\u{c0}'..='\u{17f}')
                    .filter(|a| a.is_lowercase() && *a != base && fold(*a) == base),
            )
            .collect();
        if alike.len() > 1 {
            out.push('[');
            out.extend(alike);
            out.push(']');
        } else {
            // Only the meta characters: in the regex engine `\<` and `\>` are word boundaries.
            if "\\.+*?()|[]{}^$".contains(c) {
                out.push('\\');
            }
            out.push(c);
        }
    }
    out
}

/// `OperationalPoint` → `operational-point`.
fn kebab(name: &str) -> String {
    let mut out = String::new();
    let mut previous_lower = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            if c.is_ascii_uppercase() && previous_lower {
                out.push('-');
            }
            previous_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
            previous_lower = false;
        }
    }
    out.trim_end_matches('-').to_owned()
}

/// `hasTrackGauge` → `trackGauge`, `is-part-of` → `partOf`, `Name` → `name`.
fn camel(name: &str) -> String {
    let words: Vec<String> = kebab(name)
        .split('-')
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect();
    let words = match words.first().map(String::as_str) {
        Some("has" | "is") if words.len() > 1 => &words[1..],
        _ => &words[..],
    };
    let mut out = String::new();
    for (i, w) in words.iter().enumerate() {
        if i == 0 {
            out.push_str(w);
        } else {
            let mut chars = w.chars();
            if let Some(c) = chars.next() {
                out.push(c.to_ascii_uppercase());
                out.push_str(chars.as_str());
            }
        }
    }
    if out.is_empty() { name.to_owned() } else { out }
}

/// The JSON-LD `@context` of a dataset's records.
fn context(sets: &[ApiSet], set: &ApiSet) -> Value {
    let mut ctx = Map::new();
    ctx.insert("@version".into(), 1.1.into());
    ctx.insert("id".into(), "@id".into());
    ctx.insert("type".into(), json!({ "@id": "@type", "@type": "@id" }));
    key_terms(sets, &set.fields, &mut ctx);
    Value::Object(ctx)
}

/// The `@context` terms of `fields`; an embedding key's has those of its objects' keys.
fn key_terms(sets: &[ApiSet], fields: &[Field], ctx: &mut Map<String, Value>) {
    for field in fields {
        let Some((predicate, inverse)) = &field.predicate else {
            continue;
        };
        let mut term = Map::new();
        term.insert(
            if *inverse { "@reverse" } else { "@id" }.into(),
            predicate.as_str().into(),
        );
        let mut scoped = Map::new();
        if let Some(keys) = &field.embed {
            key_terms(sets, keys, &mut scoped);
        }
        if field.lang() {
            term.insert("@container".into(), "@language".into());
        } else if field.iri {
            term.insert("@type".into(), "@id".into());
            if reference(sets, field).is_some() {
                // A reference's label is for reading, not a statement (and so is an embedded
                // object's `label` beside it).
                scoped.insert("label".into(), Value::Null);
            }
        } else if let Some(d) = &field.datatype {
            // JSON's own numbers and booleans, and plain strings, need no type.
            let native = ["string", "boolean", "integer", "double"]
                .iter()
                .any(|t| d.as_str() == format!("{XSD}{t}"));
            if !native {
                term.insert("@type".into(), d.as_str().into());
            }
        }
        if !scoped.is_empty() {
            term.insert("@context".into(), Value::Object(scoped));
        }
        ctx.insert(field.key.clone(), Value::Object(term));
    }
}

/// The dataset an IRI field refers to: the one of its `sh:class`.
fn reference<'a>(sets: &'a [ApiSet], field: &Field) -> Option<&'a ApiSet> {
    let class = field.class.as_ref()?;
    sets.iter().find(|s| &s.class == class)
}

/// `…/v1/{dataset}`: a page of records, in the store's order.
#[allow(clippy::too_many_arguments)]
fn list(
    entry: &Entry,
    sets: &[ApiSet],
    set: &ApiSet,
    params: &Params,
    base: &str,
    timeout: Option<Duration>,
    view: &View,
    linked: bool,
) -> Result<Value, HttpError> {
    let limit = number(params, "limit", DEFAULT_LIMIT, 1)?.min(MAX_LIMIT);
    let offset = number(params, "offset", 0, 0)?;
    let mut filter = String::new();
    for (i, (name, wanted)) in params.0.iter().enumerate() {
        let Some(field) = set.fields.iter().find(|f| &f.key == name) else {
            continue;
        };
        let text = serde_json::to_string(wanted).expect("a string");
        filter.push_str(&format!(
            " ?f {} ?q{i} . FILTER(STR(?q{i}) = {text})",
            field.path
        ));
    }
    if let Some(q) = params.get("q").filter(|q| !q.trim().is_empty()) {
        let Some(label) = &set.label else {
            return Err(HttpError::bad_request(format!(
                "{} has no label to search: its shape has no tts:labelPath",
                set.slug
            )));
        };
        let pattern = serde_json::to_string(&accent_free(q.trim())).expect("a string");
        filter.push_str(&format!(
            " ?f {label} ?label . FILTER(REGEX(STR(?label), {pattern}, \"i\"))"
        ));
    }
    if let Some(window) = params.get("bbox") {
        let Some(geometry) = &set.geometry else {
            return Err(HttpError::bad_request(format!(
                "{} has no geometry to filter on",
                set.slug
            )));
        };
        let wkt = crate::ogc::crs84_bbox(window)?;
        filter.push_str(&format!(
            " {}",
            geometry.near(entry, view, "?f", &wkt).filtered(
                "?f",
                "?w",
                &format!("FILTER(<{GEOF}sfIntersects>(?w, \"{wkt}\"^^<{WKT}>))")
            )
        ));
    }
    let query = format!(
        "SELECT DISTINCT ?f WHERE {{ ?f a <{}> . FILTER(isIRI(?f)) {filter} }} LIMIT {} OFFSET {offset}",
        set.class.as_str(),
        limit + 1,
    );
    let rows = run(entry, &query, timeout, view)?;
    let more = rows.len() > limit;
    let order: Vec<NamedNode> = rows
        .into_iter()
        .take(limit)
        .filter_map(|row| match row.into_iter().next().flatten() {
            Some(Term::NamedNode(f)) => Some(f),
            _ => None,
        })
        .collect();
    let records = records(entry, sets, set, &order, timeout, view)?;
    let url = format!("{base}/v1/{}", set.slug);
    let page = |offset: usize| {
        let mut query = form_urlencoded::Serializer::new(String::new());
        for (name, value) in &params.0 {
            if name != "offset" {
                query.append_pair(name, value);
            }
        }
        if offset > 0 {
            query.append_pair("offset", &offset.to_string());
        }
        let query = query.finish();
        if query.is_empty() {
            url.clone()
        } else {
            format!("{url}?{query}")
        }
    };
    let mut body = Map::new();
    if linked {
        body.insert("@context".into(), context(sets, set));
        body.insert("@graph".into(), Value::Array(records));
    } else {
        body.insert("records".into(), Value::Array(records));
    }
    // Not in the `@context`: links, not statements.
    if more {
        body.insert("next".into(), page(offset + limit).into());
    }
    if offset > 0 {
        body.insert("prev".into(), page(offset.saturating_sub(limit)).into());
    }
    Ok(Value::Object(body))
}

/// The records of `order`, in that order.
fn records(
    entry: &Entry,
    sets: &[ApiSet],
    set: &ApiSet,
    order: &[NamedNode],
    timeout: Option<Duration>,
    view: &View,
) -> Result<Vec<Value>, HttpError> {
    let mut found: HashMap<(NamedNode, usize), Vec<Term>> = HashMap::new();
    // How many values a key has, where it has more than are given.
    let mut cut: HashMap<(NamedNode, usize), u64> = HashMap::new();
    if !order.is_empty() && !set.fields.is_empty() {
        let union = |fields: &[(usize, Vec<&NamedNode>)]| -> String {
            fields
                .iter()
                .map(|(i, records)| {
                    let records: Vec<String> = records.iter().map(|f| f.to_string()).collect();
                    format!(
                        "{{ VALUES ?f {{ {} }} ?f {} ?v BIND({i} AS ?k) }}",
                        records.join(" "),
                        set.fields[*i].path
                    )
                })
                .collect::<Vec<_>>()
                .join(" UNION ")
        };
        // First how many values each key has, so that no record's key brings more than
        // MAX_VALUES values into memory, and no page more than MAX_PAGE_VALUES.
        let all: Vec<(usize, Vec<&NamedNode>)> = (0..set.fields.len())
            .map(|i| (i, order.iter().collect()))
            .collect();
        let counts = format!(
            "SELECT ?f ?k (COUNT(*) AS ?n) WHERE {{ {} }} GROUP BY ?f ?k",
            union(&all)
        );
        let mut big: Vec<(NamedNode, usize, u64)> = Vec::new();
        let mut total: u64 = 0;
        for row in run(entry, &counts, timeout, view)? {
            let (Some(Term::NamedNode(f)), Some(Term::Literal(k)), Some(Term::Literal(n))) =
                (&row[0], &row[1], &row[2])
            else {
                continue;
            };
            let (Ok(k), Ok(n)) = (k.value().parse::<usize>(), n.value().parse::<u64>()) else {
                continue;
            };
            total += n.min(MAX_VALUES as u64);
            if n > MAX_VALUES as u64 {
                big.push((f.clone(), k, n));
            }
        }
        if total > MAX_PAGE_VALUES {
            return Err(HttpError(
                413,
                format!(
                    "These records have {total} values: more than {MAX_PAGE_VALUES} in one answer. Ask for fewer (limit)."
                ),
            ));
        }
        let small: Vec<(usize, Vec<&NamedNode>)> = (0..set.fields.len())
            .map(|i| {
                let records = order
                    .iter()
                    .filter(|f| !big.iter().any(|(b, k, _)| b == *f && *k == i))
                    .collect();
                (i, records)
            })
            .filter(|(_, records): &(usize, Vec<&NamedNode>)| !records.is_empty())
            .collect();
        let mut read = |query: &str| -> Result<(), HttpError> {
            for row in run(entry, query, timeout, view)? {
                let (Some(Term::NamedNode(f)), Some(Term::Literal(k)), Some(v)) =
                    (&row[0], &row[1], &row[2])
                else {
                    continue;
                };
                if let Ok(k) = k.value().parse::<usize>() {
                    found.entry((f.clone(), k)).or_default().push(v.clone());
                }
            }
            Ok(())
        };
        if !small.is_empty() {
            read(&format!("SELECT ?f ?k ?v WHERE {{ {} }}", union(&small)))?;
        }
        // A key with too many values gives the first MAX_VALUES, in SPARQL's order.
        for (f, k, n) in big {
            read(&format!(
                "SELECT ?f ?k ?v WHERE {{ VALUES ?f {{ {f} }} ?f {} ?v BIND({k} AS ?k) }} ORDER BY ?v LIMIT {MAX_VALUES}",
                set.fields[k].path
            ))?;
            cut.insert((f, k), n);
        }
    }
    // The values of the embedded objects, level by level.
    let mut nested: HashMap<usize, Nested> = HashMap::new();
    let mut budget = MAX_PAGE_VALUES;
    for (k, field) in set.fields.iter().enumerate() {
        let Some(keys) = &field.embed else { continue };
        let parents: Vec<&NamedNode> = order
            .iter()
            .filter(|f| found.contains_key(&((*f).clone(), k)))
            .collect();
        if parents.is_empty() {
            continue;
        }
        let records: Vec<String> = parents.iter().map(|f| f.to_string()).collect();
        let start = format!("VALUES ?f {{ {} }}", records.join(" "));
        let step = (field.path.clone(), embeds(sets, field));
        let level = embedded(
            entry,
            sets,
            &start,
            &[step],
            keys,
            &mut budget,
            timeout,
            view,
        )?;
        nested.insert(k, level);
    }
    let mut pairs: Vec<(&Field, &[Term])> = found
        .iter()
        .map(|((_, k), terms)| (&set.fields[*k], terms.as_slice()))
        .collect();
    for (k, level) in &nested {
        if let Some(keys) = &set.fields[*k].embed {
            level.pairs(keys, &mut pairs);
        }
    }
    let labels = labels(entry, sets, &pairs, timeout, view)?;
    Ok(order
        .iter()
        .map(|f| {
            let mut record = Map::new();
            record.insert("id".into(), f.as_str().into());
            record.insert("type".into(), set.class.as_str().into());
            let mut truncated = Map::new();
            for (k, field) in set.fields.iter().enumerate() {
                let Some(mut terms) = found.remove(&(f.clone(), k)) else {
                    continue;
                };
                // In the values' own order: the order rows come in follows the store's term
                // ids, which other graphs' data moves (#203).
                terms.sort_by_cached_key(Term::to_string);
                terms.dedup();
                if let Some(v) = field_value(sets, field, &terms, &labels, nested.get(&k)) {
                    record.insert(field.key.clone(), v);
                }
                if let Some(n) = cut.get(&(f.clone(), k)) {
                    truncated.insert(field.key.clone(), (*n).into());
                }
            }
            // Not in the `@context`: how many values the keys cut short have.
            if !truncated.is_empty() {
                record.insert("truncated".into(), Value::Object(truncated));
            }
            Value::Object(record)
        })
        .collect())
}

/// The labels of the records that the values of `pairs` refer to, by their dataset's label
/// path.
fn labels(
    entry: &Entry,
    sets: &[ApiSet],
    pairs: &[(&Field, &[Term])],
    timeout: Option<Duration>,
    view: &View,
) -> Result<HashMap<NamedNode, String>, HttpError> {
    let mut wanted: BTreeMap<String, HashSet<NamedNode>> = BTreeMap::new();
    for (field, terms) in pairs {
        let Some(path) = reference(sets, field).and_then(|t| t.label.clone()) else {
            continue;
        };
        let iris = wanted.entry(path).or_default();
        iris.extend(terms.iter().filter_map(|t| match t {
            Term::NamedNode(n) => Some(n.clone()),
            _ => None,
        }));
    }
    let mut out: HashMap<NamedNode, String> = HashMap::new();
    for (path, iris) in wanted {
        let mut iris: Vec<_> = iris.into_iter().map(|n| n.to_string()).collect();
        iris.sort();
        for chunk in iris.chunks(1000) {
            let query = format!(
                "SELECT ?r ?l WHERE {{ VALUES ?r {{ {} }} ?r {path} ?l }}",
                chunk.join(" ")
            );
            for row in run(entry, &query, timeout, view)? {
                if let (Some(Term::NamedNode(r)), Some(Term::Literal(l))) = (&row[0], &row[1]) {
                    // The first label in text order, whatever order rows come in.
                    let l = l.value();
                    match out.get_mut(r) {
                        Some(kept) if kept.as_str() <= l => {}
                        Some(kept) => *kept = l.to_owned(),
                        None => {
                            out.insert(r.clone(), l.to_owned());
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

/// A field's JSON value from its terms (sorted, not empty). `nested`: the values of the
/// objects it embeds.
fn field_value(
    sets: &[ApiSet],
    field: &Field,
    terms: &[Term],
    labels: &HashMap<NamedNode, String>,
    nested: Option<&Nested>,
) -> Option<Value> {
    if field.lang() {
        let mut map: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for t in terms {
            if let Term::Literal(l) = t {
                map.entry(l.language().unwrap_or("@none").to_owned())
                    .or_default()
                    .push(l.value().into());
            }
        }
        let map: Map<String, Value> = map
            .into_iter()
            .map(|(lang, mut v)| {
                let v = if v.len() == 1 {
                    v.pop().expect("one")
                } else {
                    Value::Array(v)
                };
                (lang, v)
            })
            .collect();
        return (!map.is_empty()).then_some(Value::Object(map));
    }
    let target = if field.iri {
        reference(sets, field)
    } else {
        None
    };
    let mut values: Vec<Value> = Vec::new();
    for t in terms {
        let embedded = match t {
            Term::BlankNode(_) => true,
            Term::NamedNode(_) => target.is_none(),
            _ => false,
        };
        if let (true, Some(keys)) = (embedded, &field.embed) {
            let mut object = Map::new();
            if let Term::NamedNode(n) = t {
                object.insert("id".into(), n.as_str().into());
            }
            let types: Vec<Value> = nested
                .and_then(|level| level.values.get(&(t.clone(), keys.len())))
                .into_iter()
                .flatten()
                .filter_map(|c| match c {
                    Term::NamedNode(c) => Some(c.as_str().into()),
                    _ => None,
                })
                .collect();
            match types.len() {
                0 => {}
                1 => {
                    object.insert("type".into(), types[0].clone());
                }
                _ => {
                    object.insert("type".into(), Value::Array(types));
                }
            }
            for (j, key) in keys.iter().enumerate() {
                let Some(level) = nested else { break };
                let Some(inner) = level.values.get(&(t.clone(), j)) else {
                    continue;
                };
                if let Some(v) = field_value(sets, key, inner, labels, level.children.get(&j)) {
                    object.insert(key.key.clone(), v);
                }
            }
            values.push(Value::Object(object));
            continue;
        }
        values.extend(match (t, target) {
            (Term::NamedNode(n), Some(target)) => {
                let mut r = json!({ "id": n.as_str(), "type": target.class.as_str() });
                if let Some(label) = labels.get(n) {
                    r["label"] = label.as_str().into();
                }
                Some(r)
            }
            _ => value(t),
        });
    }
    match values.len() {
        0 => None,
        1 if field.single => values.into_iter().next(),
        _ => Some(Value::Array(values)),
    }
}

/// The values of embedded objects: by object and key (an index into the embedding field's
/// keys, or its number of keys for the objects' types), sorted; and those of the objects
/// they embed, by key.
#[derive(Default)]
struct Nested {
    values: HashMap<(Term, usize), Vec<Term>>,
    children: HashMap<usize, Nested>,
}

impl Nested {
    /// Every key with its values, at this level and below.
    fn pairs<'a>(&'a self, keys: &'a [Field], out: &mut Vec<(&'a Field, &'a [Term])>) {
        for ((_, j), terms) in &self.values {
            // Past the keys: the objects' types.
            if let Some(key) = keys.get(*j) {
                out.push((key, terms.as_slice()));
            }
        }
        for (j, child) in &self.children {
            if let Some(inner) = &keys[*j].embed {
                child.pairs(inner, out);
            }
        }
    }
}

/// The filter on the values of a field that it embeds: blank nodes, and IRIs too when they
/// are no dataset's records.
fn embeds(sets: &[ApiSet], field: &Field) -> &'static str {
    if field.iri && reference(sets, field).is_some() {
        "FILTER(isBlank(?b))"
    } else {
        "FILTER(!isLiteral(?b))"
    }
}

/// The values of the objects reached from `start` (a pattern that binds `?f`) by `steps`
/// (a path each, with the filter on what it reaches, as `?b`): of their `keys`, and of the
/// objects these embed in turn. `budget` is how many more values may be read.
#[allow(clippy::too_many_arguments)]
fn embedded(
    entry: &Entry,
    sets: &[ApiSet],
    start: &str,
    steps: &[(String, &'static str)],
    keys: &[Field],
    budget: &mut u64,
    timeout: Option<Duration>,
    view: &View,
) -> Result<Nested, HttpError> {
    let mut chain = String::new();
    let mut from = "?f".to_owned();
    for (i, (path, filter)) in steps.iter().enumerate() {
        let to = format!("?b{i}");
        chain.push_str(&format!(
            " {from} {path} {to} . {}",
            filter.replace("?b", &to)
        ));
        from = to;
    }
    // The objects' types come as key `keys.len()`.
    let mut branches: Vec<String> = keys
        .iter()
        .enumerate()
        .map(|(j, key)| format!("{{ {from} {} ?v BIND({j} AS ?j) }}", key.path))
        .collect();
    branches.push(format!("{{ {from} a ?v BIND({} AS ?j) }}", keys.len()));
    let query = format!(
        "SELECT DISTINCT {from} ?j ?v WHERE {{ {start}{chain} {} }} LIMIT {}",
        branches.join(" UNION "),
        *budget + 1
    );
    let mut level = Nested::default();
    let rows = run(entry, &query, timeout, view)?;
    if rows.len() as u64 > *budget {
        return Err(HttpError(
            413,
            format!(
                "These records' embedded objects have more than {MAX_PAGE_VALUES} values in one answer. Ask for fewer (limit)."
            ),
        ));
    }
    *budget -= rows.len() as u64;
    for row in rows {
        let (Some(b), Some(Term::Literal(j)), Some(v)) = (&row[0], &row[1], &row[2]) else {
            continue;
        };
        if let Ok(j) = j.value().parse::<usize>() {
            level
                .values
                .entry((b.clone(), j))
                .or_default()
                .push(v.clone());
        }
    }
    for terms in level.values.values_mut() {
        terms.sort_by_cached_key(Term::to_string);
        terms.dedup();
    }
    for (j, key) in keys.iter().enumerate() {
        let Some(inner) = &key.embed else { continue };
        if !level.values.keys().any(|(_, k)| *k == j) {
            continue;
        }
        let mut deeper = steps.to_vec();
        deeper.push((key.path.clone(), embeds(sets, key)));
        let child = embedded(entry, sets, start, &deeper, inner, budget, timeout, view)?;
        level.children.insert(j, child);
    }
    Ok(level)
}

/// The JSON Schema of a key's value (a list where it may have many).
fn key_schema(sets: &[ApiSet], field: &Field) -> Value {
    let reference = field.iri && reference(sets, field).is_some();
    let one = if field.lang() {
        json!({ "type": "object", "description": "Text by language tag",
                "additionalProperties": { "oneOf": [ { "type": "string" }, { "type": "array", "items": { "type": "string" } } ] } })
    } else if let Some(keys) = &field.embed {
        let mut properties = Map::new();
        properties.insert("id".into(), json!({ "type": "string", "format": "uri" }));
        properties.insert("type".into(), json!({ "type": "string", "format": "uri" }));
        for key in keys.iter() {
            properties.insert(key.key.clone(), key_schema(sets, key));
        }
        let object = json!({
            "type": "object",
            "description": "An embedded object: a blank node, or a resource that is no dataset's record",
            "properties": properties,
        });
        if reference {
            json!({ "oneOf": [ { "$ref": "#/components/schemas/Reference" }, object ] })
        } else {
            object
        }
    } else if reference {
        json!({ "$ref": "#/components/schemas/Reference" })
    } else {
        field.schema.clone()
    };
    if field.single || field.lang() {
        one
    } else {
        json!({ "type": "array", "items": one })
    }
}

/// JSON as YAML, keys in the same order.
fn yaml(value: &Value) -> yaml_rust2::Yaml {
    use yaml_rust2::Yaml;
    match value {
        Value::Null => Yaml::Null,
        Value::Bool(b) => Yaml::Boolean(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => Yaml::Integer(i),
            None => Yaml::Real(n.to_string()),
        },
        Value::String(s) => Yaml::String(s.clone()),
        Value::Array(values) => Yaml::Array(values.iter().map(yaml).collect()),
        Value::Object(map) => Yaml::Hash(
            map.iter()
                .map(|(k, v)| (Yaml::String(k.clone()), yaml(v)))
                .collect(),
        ),
    }
}

/// An OpenAPI 3.1 definition of the API, from the same datasets.
fn openapi(entry: &Entry, base: &str, sets: &[ApiSet]) -> Value {
    let param = |name: &str, place: &str, schema: Value, description: &str| json!({ "name": name, "in": place, "required": place == "path", "schema": schema, "description": description });
    let error = json!({ "description": "An error, as text", "content": { "text/plain": { "schema": { "type": "string" } } } });
    let mut paths = Map::new();
    let mut schemas = Map::new();
    schemas.insert(
        "Reference".into(),
        json!({
            "type": "object",
            "description": "A record of another dataset",
            "required": ["id", "type"],
            "properties": {
                "id": { "type": "string", "format": "uri" },
                "type": { "type": "string", "format": "uri" },
                "label": { "type": "string" },
            },
        }),
    );
    paths.insert(
        "/v1".into(),
        json!({ "get": {
            "operationId": "datasets",
            "summary": "The datasets",
            "responses": { "200": { "description": "The datasets", "content": { JSON: { "schema": { "type": "object" } } } } },
        }}),
    );
    let format = param(
        "f",
        "query",
        json!({ "type": "string", "enum": ["jsonld", "json", "geojson", "csv"] }),
        "jsonld (the default); json, without @context; geojson, for datasets with a geometry; csv",
    );
    for set in sets {
        let name = schema_name(&set.slug);
        let mut properties = Map::new();
        properties.insert("id".into(), json!({ "type": "string", "format": "uri" }));
        properties.insert("type".into(), json!({ "type": "string", "format": "uri" }));
        for field in &set.fields {
            properties.insert(field.key.clone(), key_schema(sets, field));
        }
        let mut record = json!({
            "type": "object",
            "title": set.title,
            "required": ["id", "type"],
            "properties": properties,
        });
        if let Some(d) = &set.description {
            record["description"] = d.as_str().into();
        }
        schemas.insert(name.clone(), record);
        let mut list_params = vec![
            format.clone(),
            param(
                "limit",
                "query",
                json!({ "type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": DEFAULT_LIMIT }),
                "Records per page",
            ),
            param(
                "offset",
                "query",
                json!({ "type": "integer", "minimum": 0, "default": 0 }),
                "Records to skip",
            ),
        ];
        if set.label.is_some() {
            list_params.push(param(
                "q",
                "query",
                json!({ "type": "string" }),
                "Text the label contains, regardless of case",
            ));
        }
        if set.geometry.is_some() {
            list_params.push(param(
                "bbox",
                "query",
                json!({ "type": "array", "items": { "type": "number" }, "minItems": 4, "maxItems": 6 }),
                "Records whose geometry meets this box (CRS84): min lon, min lat, max lon, max lat",
            ));
        }
        for field in &set.fields {
            list_params.push(param(
                &field.key,
                "query",
                json!({ "type": "string" }),
                "Records with this value (as text; an IRI for references)",
            ));
        }
        let page = json!({
            "type": "object",
            "properties": {
                "@context": { "type": "object" },
                "@graph": { "type": "array", "items": { "$ref": format!("#/components/schemas/{name}") } },
                "records": { "type": "array", "items": { "$ref": format!("#/components/schemas/{name}") }, "description": "With f=json, instead of @graph" },
                "next": { "type": "string", "format": "uri" },
                "prev": { "type": "string", "format": "uri" },
            },
        });
        let if_match = param(
            "If-Match",
            "header",
            json!({ "type": "string" }),
            "The ETag the record was read with: the write is refused (412) when it changed since",
        );
        let record_ref = json!({ "$ref": format!("#/components/schemas/{name}") });
        let body = json!({ "required": true, "content": { JSON: { "schema": record_ref }, JSON_LD: { "schema": record_ref } } });
        let written = json!({ JSON_LD: { "schema": record_ref } });
        let problem = json!({ "description": "The record does not conform to its shape, or a key cannot be written", "content": { "application/problem+json": { "schema": { "$ref": "#/components/schemas/Problem" } } } });
        paths.insert(
            format!("/v1/{}", set.slug),
            json!({
                "get": {
                    "operationId": format!("list-{}", set.slug),
                    "summary": format!("{}: records", set.title),
                    "parameters": list_params,
                    "responses": {
                        "200": { "description": "A page of records", "content": { JSON_LD: { "schema": page }, JSON: { "schema": page } } },
                        "400": error,
                    },
                },
                "post": {
                    "operationId": format!("create-{}", set.slug),
                    "summary": format!("{}: a new record, its IRI in id", set.title),
                    "parameters": [ param("graph", "query", json!({ "type": "string", "format": "uri" }), "The named graph to write it to, when the default graph is the union of all graphs") ],
                    "requestBody": body,
                    "responses": {
                        "201": { "description": "The record as written", "content": written },
                        "409": error,
                        "422": problem,
                    },
                },
            }),
        );
        paths.insert(
            format!("/v1/{}/{{id}}", set.slug),
            json!({ "get": {
                "operationId": format!("get-{}", set.slug),
                "summary": format!("{}: one record", set.title),
                "parameters": [
                    param("id", "path", json!({ "type": "string" }), "The record's IRI, percent-encoded"),
                    format,
                ],
                "responses": {
                    "200": { "description": "The record", "content": {
                        JSON_LD: { "schema": { "$ref": format!("#/components/schemas/{name}") } },
                        JSON: { "schema": { "$ref": format!("#/components/schemas/{name}") } },
                    } },
                    "404": error,
                },
            },
            "put": {
                "operationId": format!("replace-{}", set.slug),
                "summary": format!("{}: replace the record's keys (a key left out loses its values), or make the record", set.title),
                "parameters": [ param("id", "path", json!({ "type": "string" }), "The record's IRI, percent-encoded"), if_match.clone() ],
                "requestBody": body,
                "responses": {
                    "200": { "description": "The record as written", "content": written },
                    "201": { "description": "The record, made", "content": written },
                    "412": error,
                    "422": problem,
                },
            },
            "patch": {
                "operationId": format!("change-{}", set.slug),
                "summary": format!("{}: change the keys in the body (null removes a key's values)", set.title),
                "parameters": [ param("id", "path", json!({ "type": "string" }), "The record's IRI, percent-encoded"), if_match.clone() ],
                "requestBody": { "required": true, "content": { "application/merge-patch+json": { "schema": { "type": "object" } }, JSON: { "schema": { "type": "object" } } } },
                "responses": {
                    "200": { "description": "The record as written", "content": written },
                    "404": error,
                    "412": error,
                    "422": problem,
                },
            },
            "delete": {
                "operationId": format!("delete-{}", set.slug),
                "summary": format!("{}: remove the record's type and keys", set.title),
                "parameters": [ param("id", "path", json!({ "type": "string" }), "The record's IRI, percent-encoded"), if_match.clone() ],
                "responses": { "204": { "description": "Removed" }, "404": error, "412": error },
            }}),
        );
    }
    schemas.insert(
        "Problem".into(),
        json!({
            "type": "object",
            "description": "Why a write was refused (RFC 9457)",
            "properties": {
                "title": { "type": "string" },
                "status": { "type": "integer" },
                "errors": { "type": "array", "items": { "type": "object", "properties": {
                    "key": { "type": "string" },
                    "value": {},
                    "constraint": { "type": "string", "description": "The SHACL constraint, as sh:minCount" },
                    "message": { "type": "string" },
                    "shape": { "type": "string" },
                } } },
            },
        }),
    );
    let mut tags = vec![json!({ "name": "Datasets", "description": "The datasets of the API" })];
    for set in sets {
        let mut tag = json!({ "name": set.title });
        if let Some(d) = &set.description {
            tag["description"] = d.as_str().into();
        }
        tags.push(tag);
    }
    // A tag per dataset, and a description of each operation (as linters ask).
    for (path, item) in paths.iter_mut() {
        let set = path
            .strip_prefix("/v1/")
            .map(|p| p.split('/').next().unwrap_or(p));
        let tag = set
            .and_then(|slug| sets.iter().find(|s| s.slug == slug))
            .map_or("Datasets".to_owned(), |s| s.title.clone());
        let one = path.ends_with("{id}");
        let Value::Object(item) = item else { continue };
        for (method, op) in item.iter_mut() {
            let description = match (method.as_str(), set.is_some(), one) {
                ("get", false, _) => {
                    "The datasets of the API, one per node shape with a target class, with their URLs."
                }
                ("get", true, false) => {
                    "A page of the dataset's records, in the store's order, filtered by the parameters. The answer links the next and previous pages."
                }
                ("post", ..) => {
                    "Makes a record, with its IRI as id. It is validated against the dataset's own shapes first."
                }
                ("get", true, true) => "One record, with its ETag.",
                ("put", ..) => {
                    "Replaces the record's writable keys, validated against the dataset's own shapes first. A key the body leaves out loses its values."
                }
                ("patch", ..) => {
                    "Changes the keys in the body (JSON Merge Patch), validated against the dataset's own shapes first."
                }
                ("delete", ..) => "Removes the record's type and the values of its writable keys.",
                _ => continue,
            };
            if let Value::Object(op) = op {
                op.insert("description".into(), description.into());
                op.insert("tags".into(), json!([tag]));
            }
        }
    }
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": format!("{}: REST API", entry.name),
            "version": "1",
            "description": "Generated from the dataset's shapes (the rest-api reader).",
            "contact": { "name": entry.name, "url": base },
        },
        "servers": [ { "url": base } ],
        "tags": tags,
        "paths": paths,
        "components": { "schemas": schemas },
    })
}

/// A schema name OpenAPI accepts: `operational-point` → `OperationalPoint`.
fn schema_name(slug: &str) -> String {
    let mut out = String::new();
    let mut upper = true;
    for c in slug.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(if upper { c.to_ascii_uppercase() } else { c });
            upper = false;
        } else {
            upper = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{camel, kebab, schema_name};

    #[test]
    fn names() {
        assert_eq!(kebab("OperationalPoint"), "operational-point");
        assert_eq!(kebab("SoL_Track"), "so-l-track");
        assert_eq!(camel("hasTrackGauge"), "trackGauge");
        assert_eq!(camel("isPartOf"), "partOf");
        assert_eq!(camel("Name"), "name");
        assert_eq!(camel("has"), "has");
        assert_eq!(camel("opName"), "opName");
        assert_eq!(schema_name("operational-point"), "OperationalPoint");
        assert_eq!(super::accent_free("<->"), "<->");
        let pattern = super::accent_free("Köln (Hbf)");
        assert!(pattern.starts_with("[kķ][oòóôõö"), "{pattern}");
        assert!(pattern.ends_with(" \\([hĥħ]bf\\)"), "{pattern}");
    }
}
