//! The search builder's server side (#142), from the `search` reader's shapes:
//! - `GET /{ds}/search/labels?q=`: resources found by their labels, for the search box that
//!   jumps to a resource's page and the builder's value pickers; matching is the full-text
//!   index's (`tt:search`).
//! - `GET /{ds}/search/classes`: the classes to search, with the properties to search them
//!   by: what the builder offers, and the SPARQL path it writes for each.

use super::{ApiSet, Field, GEOF, WKT, reference, sets_for};
use crate::admin::Entry;
use crate::{HttpError, Params, Reply, State, View};
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use tripletrail::oxigraph::model::NamedNode;
use tripletrail::shapes::Reader;

/// Results given by default, and at most.
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;
/// Results of one class when no class is asked for, so that one class with many alike labels
/// leaves room for the others.
const PER_CLASS: usize = 5;

/// `origin` is the server's (`https://host`), `ds` the dataset's name as in the path.
pub(crate) fn labels(
    state: &State,
    entry: &Entry,
    params: &Params,
    origin: &str,
    ds: &str,
    timeout: Option<Duration>,
    view: &View,
) -> Result<Reply, HttpError> {
    let q = params
        .get("q")
        .ok_or_else(|| HttpError::bad_request("Missing the q parameter"))?;
    let limit = match params.get("limit") {
        Some(l) => l
            .parse::<usize>()
            .ok()
            .filter(|l| (1..=MAX_LIMIT).contains(l))
            .ok_or_else(|| {
                HttpError::bad_request(format!("limit is a number from 1 to {MAX_LIMIT}"))
            })?,
        None => DEFAULT_LIMIT,
    };
    let sets = sets_for(state, entry, view, Reader::Search)?;
    // A class by its dataset's id or by IRI; several with a comma.
    let chosen: Vec<&ApiSet> = match params.get("class") {
        Some(wanted) => {
            let mut chosen = Vec::new();
            for w in wanted.split(',').map(str::trim).filter(|w| !w.is_empty()) {
                let set = sets
                    .iter()
                    .find(|s| s.slug == w || s.class.as_str() == w)
                    .ok_or_else(|| HttpError(404, format!("No class {w} to search")))?;
                chosen.push(set);
            }
            chosen
        }
        None => sets.iter().collect(),
    };
    let per_class = if params.get("class").is_some() {
        limit
    } else {
        PER_CLASS
    };
    let classes: Vec<NamedNode> = chosen.iter().map(|s| s.class.clone()).collect();
    let found = crate::tiles::label_matches(entry, &classes, q, timeout, view)?;
    let encode = |s: &str| form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    let mut results = Vec::new();
    let mut counts: HashMap<NamedNode, usize> = HashMap::new();
    for (iri, class, label) in found {
        if results.len() == limit {
            break;
        }
        let n = counts.entry(class.clone()).or_default();
        if *n == per_class {
            continue;
        }
        *n += 1;
        // The first dataset of that class (a resource found through one of its classes).
        let Some(set) = chosen.iter().find(|s| s.class == class) else {
            continue;
        };
        results.push(json!({
            "iri": iri.as_str(),
            "label": label,
            "class": set.slug,
            "classTitle": set.title,
            "describe": format!("{origin}/{ds}/describe?iri={}", encode(iri.as_str())),
            "record": format!("{origin}/{ds}/api/v1/{}/{}", set.slug, encode(iri.as_str())),
        }));
    }
    let classes: Vec<_> = sets
        .iter()
        .map(|s| json!({ "id": s.slug, "title": s.title, "class": s.class.as_str() }))
        .collect();
    let body = json!({ "results": results, "classes": classes });
    Ok(Reply::bytes(
        200,
        "application/json",
        serde_json::to_vec(&body).expect("JSON"),
    ))
}

/// `GET /{ds}/search/classes`: per class its id, title, IRI, number of instances (when known),
/// label path and whether it has a geometry, and per property its key, title, SPARQL path,
/// kind (`text`, `number`, `boolean`, `date`, `reference` to another class by id, `iri`,
/// `geometry`, or `object` for embedded blank nodes) and whether it has one value at most.
pub(crate) fn classes(state: &State, entry: &Entry, view: &View) -> Result<Reply, HttpError> {
    let sets = sets_for(state, entry, view, Reader::Search)?;
    let out: Vec<_> = sets
        .iter()
        .map(|s| {
            let properties: Vec<_> = s
                .fields
                .iter()
                .map(|f| {
                    let (mut kind, target) = kind(&sets, f);
                    // The geometry way's first step: what the map draws.
                    if s.geometry
                        .as_ref()
                        .and_then(|g| g.path())
                        .is_some_and(|g| g.starts_with(&f.path))
                    {
                        kind = "geometry";
                    }
                    let mut p = json!({
                        "key": f.key,
                        "title": f.schema.get("title").and_then(|t| t.as_str()).unwrap_or(&f.key),
                        "path": f.path,
                        "kind": kind,
                        "single": f.single,
                    });
                    if let Some(t) = target {
                        p["target"] = t.into();
                    }
                    if let Some(d) = &f.datatype {
                        p["datatype"] = d.as_str().into();
                    }
                    if let Some(d) = f.schema.get("description") {
                        p["description"] = d.clone();
                    }
                    p
                })
                .collect();
            let mut c = json!({
                "id": s.slug,
                "title": s.title,
                "class": s.class.as_str(),
                "geometry": s.geometry.is_some(),
                "properties": properties,
            });
            if let Some(n) = s.instances {
                c["count"] = n.into();
            }
            if let Some(l) = &s.label {
                c["label"] = l.as_str().into();
            }
            if let Some(d) = &s.description {
                c["description"] = d.as_str().into();
            }
            c
        })
        .collect();
    Ok(Reply::bytes(
        200,
        "application/json",
        serde_json::to_vec(&json!({ "classes": out })).expect("JSON"),
    ))
}

/// How the builder offers a property, and the class id a reference points to.
fn kind(sets: &[ApiSet], f: &Field) -> (&'static str, Option<String>) {
    if f.iri {
        if let Some(target) = reference(sets, f) {
            return ("reference", Some(target.slug.clone()));
        }
        return (if f.embed.is_some() { "object" } else { "iri" }, None);
    }
    if f.embed.is_some() {
        return ("object", None);
    }
    if f.lang() {
        return ("text", None);
    }
    match (
        f.schema.get("type").and_then(|t| t.as_str()),
        f.schema.get("format").and_then(|t| t.as_str()),
    ) {
        (Some("integer" | "number"), _) => ("number", None),
        (Some("boolean"), _) => ("boolean", None),
        (_, Some("date" | "date-time")) => ("date", None),
        (_, Some("uri")) => ("iri", None),
        _ => ("text", None),
    }
}

/// Rows a search answers at most (one more is read to know there are more).
const MAX_ROWS: usize = 1_000;
const DEFAULT_ROWS: usize = 100;

/// One condition: a property, an operator and (for most operators) a value.
struct Condition {
    key: String,
    op: String,
    value: String,
    /// Alternatives, any of which may hold ("or"); then key, op and value are unused.
    any: Vec<Condition>,
}

/// A search: the class to find, the conditions its resources meet, and more properties to show.
struct Model {
    class: String,
    conditions: Vec<Condition>,
    show: Vec<String>,
    /// A map area (`minLon,minLat,maxLon,maxLat`, CRS84) the resources' geometry meets, as WKT.
    area: Option<String>,
    /// The key the rows are sorted by (`label` by default) and whether descending (#81).
    sort: Option<(String, bool)>,
    /// Words the resource's label holds, case aside (#81).
    q: Option<String>,
}

/// Operators that take no value.
const NO_VALUE: [&str; 4] = ["exists", "missing", "true", "false"];

/// The most conditions, and the most extra columns, in one search.
const MAX_PARTS: usize = 20;
/// The most alternatives in one "any" group.
const MAX_ANY: usize = 10;

fn model(params: &Params) -> Result<Model, HttpError> {
    let text = params
        .get("m")
        .ok_or_else(|| HttpError::bad_request("Missing the m parameter (the search, as JSON)"))?;
    let m: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| HttpError::bad_request(format!("m is not JSON: {e}")))?;
    let class = m
        .get("class")
        .and_then(|c| c.as_str())
        .ok_or_else(|| HttpError::bad_request("The search has no class"))?
        .to_owned();
    let text_of = |v: Option<&serde_json::Value>| match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    };
    let wheres = m
        .get("where")
        .and_then(|w| w.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default();
    let shows = m
        .get("show")
        .and_then(|s| s.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default();
    if wheres.len() > MAX_PARTS || shows.len() > MAX_PARTS {
        return Err(HttpError(
            422,
            format!("A search has at most {MAX_PARTS} conditions and {MAX_PARTS} columns"),
        ));
    }
    let one = |w: &serde_json::Value| Condition {
        key: text_of(w.get("key")),
        op: text_of(w.get("op")),
        value: text_of(w.get("value")),
        any: Vec::new(),
    };
    let mut conditions = Vec::new();
    for w in wheres {
        match w.get("any").and_then(|a| a.as_array()) {
            Some(any) => {
                if any.is_empty() || any.len() > MAX_ANY {
                    return Err(HttpError(
                        422,
                        format!("An \"any\" group has 1 to {MAX_ANY} conditions"),
                    ));
                }
                if any.iter().any(|a| a.get("any").is_some()) {
                    return Err(HttpError(422, "\"any\" groups do not nest".to_owned()));
                }
                conditions.push(Condition {
                    key: String::new(),
                    op: String::new(),
                    value: String::new(),
                    any: any.iter().map(one).collect(),
                });
            }
            None => conditions.push(one(w)),
        }
    }
    let show = shows
        .iter()
        .filter_map(|k| k.as_str().map(str::to_owned))
        .collect();
    let area = match m.get("area") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Array(a)) => Some(
            a.iter()
                .map(|n| text_of(Some(n)))
                .collect::<Vec<_>>()
                .join(","),
        ),
        Some(a) => Some(text_of(Some(a))),
    }
    .map(|a| crate::ogc::crs84_bbox(&a))
    .transpose()?;
    let sort = match m.get("sort") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(key)) => Some((key.clone(), false)),
        Some(o) => Some((
            o.get("key")
                .and_then(|k| k.as_str())
                .ok_or_else(|| HttpError(422, "sort has no key".to_owned()))?
                .to_owned(),
            o.get("desc").and_then(|d| d.as_bool()).unwrap_or(false),
        )),
    };
    let q = m
        .get("q")
        .and_then(|q| q.as_str())
        .map(|q| q.trim().to_lowercase())
        .filter(|q| !q.is_empty());
    Ok(Model {
        class,
        conditions,
        show,
        area,
        sort,
        q,
    })
}

/// A column of the answer: the variable, the property, and the variable of a reference's label.
struct Column<'a> {
    var: String,
    field: &'a Field,
    label: Option<String>,
    /// A shown property with several values: read for the rows shown by a second query
    /// (`results`), so that columns do not multiply the rows of the search.
    many: bool,
}

/// The values a column with several values shows at most per row.
const MAX_SHOWN: usize = 10;

/// The pattern of one condition on `?s`, with `v` for its value, and the property it shows
/// (none for `exists` and `missing`). `i` numbers its other variables.
fn condition<'a>(
    sets: &[ApiSet],
    set: &'a ApiSet,
    w: &Condition,
    v: &str,
    i: usize,
) -> Result<(String, Option<&'a Field>), HttpError> {
    let bad = |m: String| HttpError(422, m);
    let f = set
        .fields
        .iter()
        .find(|f| f.key == w.key)
        .ok_or_else(|| bad(format!("{} has no key {}", set.title, w.key)))?;
    let (kind, _) = kind(sets, f);
    let path = &f.path;
    let value = w.value.trim();
    if !NO_VALUE.contains(&w.op.as_str()) && value.is_empty() {
        return Err(bad(format!("Give a value for {}", w.key)));
    }
    let lower = || literal(&value.to_lowercase());
    let filter = match w.op.as_str() {
        "exists" => {
            return Ok((format!("FILTER EXISTS {{ ?s {path} ?any{i} }}"), None));
        }
        "missing" => {
            return Ok((format!("FILTER NOT EXISTS {{ ?s {path} ?any{i} }}"), None));
        }
        "contains" => format!("CONTAINS(LCASE(STR({v})), {})", lower()),
        "starts" => format!("STRSTARTS(LCASE(STR({v})), {})", lower()),
        "true" | "false" => format!("{v} = {}", w.op),
        "before" => format!("STR({v}) < {}", literal(value)),
        "after" => format!("STR({v}) > {}", literal(value)),
        "on" => format!("STRSTARTS(STR({v}), {})", literal(value)),
        "is" if matches!(kind, "reference" | "iri") => {
            let iri = NamedNode::new(value)
                .map_err(|_| bad(format!("{} is not an IRI: pick one from the list", value)))?;
            format!("{v} = {iri}")
        }
        "is" => format!("STR({v}) = {}", literal(value)),
        "between" => {
            let (a, b) = value
                .split_once("..")
                .and_then(|(a, b)| Some((number(a)?, number(b)?)))
                .ok_or_else(|| bad(format!("Give {} as two numbers, like 10..20", w.key)))?;
            format!("{v} >= {a} && {v} <= {b}")
        }
        op => {
            let sign = match op {
                "eq" => "=",
                "ne" => "!=",
                "lt" => "<",
                "le" => "<=",
                "gt" => ">",
                "ge" => ">=",
                _ => return Err(bad(format!("Unknown operator {op}"))),
            };
            let n = number(value).ok_or_else(|| bad(format!("{} needs a number", w.key)))?;
            format!("{v} {sign} {n}")
        }
    };
    Ok((format!("?s {path} {v} . FILTER({filter})"), Some(f)))
}

/// The class of `m` and the patterns of its conditions (but the `skip`th) on `?s`, with the
/// columns they show.
type Pattern<'a> = (&'a ApiSet, Vec<String>, Vec<Column<'a>>);
/// With `near`, a map area narrows a line way by its lines' boxes (see
/// [`crate::geometry_ways::Ways::near`]); the SPARQL shown to users has no `near`.
fn pattern<'a>(
    sets: &'a [ApiSet],
    m: &Model,
    skip: Option<usize>,
    near: Option<(&Entry, &View)>,
) -> Result<Pattern<'a>, HttpError> {
    let set = sets
        .iter()
        .find(|s| s.slug == m.class || s.class.as_str() == m.class)
        .ok_or_else(|| HttpError(404, format!("No class {} to search", m.class)))?;
    let mut pattern = vec![format!("?s a {} .", set.class)];
    if let Some(wkt) = &m.area {
        let geometry = set.geometry.as_ref().ok_or_else(|| {
            HttpError(
                422,
                format!("{} has no geometry to search a map area on", set.title),
            )
        })?;
        let geometry = match near {
            Some((entry, view)) => geometry.near(entry, view, "?s", wkt),
            None => geometry.clone(),
        };
        pattern.push(geometry.filtered(
            "?s",
            "?area",
            &format!("FILTER(<{GEOF}sfIntersects>(?area, \"{wkt}\"^^<{WKT}>))"),
        ));
    }
    let mut columns: Vec<Column> = Vec::new();
    for (i, w) in m.conditions.iter().enumerate() {
        if skip == Some(i) {
            continue;
        }
        if w.any.is_empty() {
            let v = format!("?w{i}");
            let (p, f) = condition(sets, set, w, &v, i)?;
            pattern.push(p);
            if let Some(field) = f {
                columns.push(Column {
                    var: v,
                    field,
                    label: None,
                    many: false,
                });
            }
        } else {
            // "or": a UNION of the alternatives; DISTINCT ?s (or GROUP BY ?s) keeps one row.
            let mut branches = Vec::new();
            for (j, a) in w.any.iter().enumerate() {
                let (p, _) = condition(sets, set, a, &format!("?w{i}_{j}"), i * 100 + j)?;
                // ?s is bound in each branch: a branch evaluates on its own, and an
                // EXISTS filter on an unbound ?s would match as soon as anything matches.
                branches.push(format!("{{ ?s a {} . {p} }}", set.class));
            }
            pattern.push(branches.join(" UNION "));
        }
    }
    Ok((set, pattern, columns))
}

/// How a search's SPARQL is asked for.
#[derive(Clone, Copy, PartialEq)]
enum Ask {
    /// Its number of resources.
    Count,
    /// Its rows, but the columns with several values, which `results` reads apart.
    Rows,
    /// One query with every column, the values of those with several joined (for the Query page).
    Whole,
}

/// The SPARQL of `m`: the rows (with `limit`, after `offset`) or their number; and the columns.
fn sparql<'a>(
    sets: &'a [ApiSet],
    m: &Model,
    limit: usize,
    offset: usize,
    ask: Ask,
    near: Option<(&Entry, &View)>,
) -> Result<(String, &'a ApiSet, Vec<Column<'a>>), HttpError> {
    let bad = |m: String| HttpError(422, m);
    let (set, mut pattern, mut columns) = pattern(sets, m, None, near)?;
    // `q`: the label holds the text, case aside (a class without a label path: its IRI).
    if let Some(q) = &m.q {
        let text = match &set.label {
            Some(label) => {
                pattern.push(format!("OPTIONAL {{ ?s {label} ?ql }}"));
                "COALESCE(STR(?ql), STR(?s))"
            }
            None => "STR(?s)",
        };
        pattern.push(format!("FILTER(CONTAINS(LCASE({text}), {}))", literal(q)));
    }
    if ask == Ask::Count {
        return Ok((
            format!(
                "SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE {{\n  {}\n}}",
                pattern.join("\n  ")
            ),
            set,
            columns,
        ));
    }
    for (i, key) in m.show.iter().enumerate() {
        let Some(f) = set.fields.iter().find(|f| &f.key == key) else {
            return Err(bad(format!("{} has no key {key}", set.title)));
        };
        if columns.iter().any(|c| c.field.key == f.key) {
            continue;
        }
        let v = format!("?c{i}");
        let many = !f.single;
        if !(many && ask == Ask::Rows) {
            pattern.push(format!("OPTIONAL {{ ?s {} {v} }}", f.path));
        }
        columns.push(Column {
            var: v,
            field: f,
            label: None,
            many,
        });
    }
    // A reference's label path, to show a label instead of its IRI; `results` reads the
    // labels afterwards, so that each label belongs to the IRI shown.
    for c in &mut columns {
        if let Some(target) = reference(sets, c.field) {
            c.label = target.label.clone();
        }
    }
    if let Some(label) = &set.label {
        pattern.push(format!("OPTIONAL {{ ?s {label} ?label }}"));
    }
    // MIN, not SAMPLE: the same data always gives the same answer.
    let mut select = vec!["?s".to_owned(), "(MIN(?label) AS ?l)".to_owned()];
    for c in &columns {
        match (c.many, ask) {
            (true, Ask::Rows) => {}
            (true, _) => select.push(format!(
                "(GROUP_CONCAT(DISTINCT STR({0}); separator=\", \") AS {0}_)",
                c.var
            )),
            _ => select.push(format!("(MIN({0}) AS {0}_)", c.var)),
        }
    }
    // By label, or by a column with one value per row (#81), then by label and IRI so that
    // pages follow each other.
    let by = match &m.sort {
        None => String::new(),
        Some((key, _)) if key == "label" => String::new(),
        Some((key, desc)) => {
            let Some(c) = columns.iter().find(|c| &c.field.key == key) else {
                return Err(bad(format!(
                    "Sort by {key}: it is no condition's or shown property"
                )));
            };
            if c.many {
                return Err(bad(format!(
                    "Sort by {key}: it may have several values (no sh:maxCount 1)"
                )));
            }
            // Numbers as numbers, other values by their text; rows without one last.
            let v = format!("{}_", c.var);
            let order = format!("COALESCE(xsd:double({v}), LCASE(STR({v})))");
            let order = order.replace("xsd:double", "<http://www.w3.org/2001/XMLSchema#double>");
            if *desc {
                format!("(!BOUND({v})) DESC({order}) ")
            } else {
                format!("(!BOUND({v})) {order} ")
            }
        }
    };
    let label_desc = matches!(&m.sort, Some((k, true)) if k == "label");
    let label_order = if label_desc {
        "DESC(LCASE(STR(?l))) ?s"
    } else {
        "LCASE(STR(?l)) ?s"
    };
    let offset = if offset > 0 {
        format!("\nOFFSET {offset}")
    } else {
        String::new()
    };
    Ok((
        format!(
            "SELECT {} WHERE {{\n  {}\n}}\nGROUP BY ?s\nORDER BY {by}{label_order}\nLIMIT {}{offset}",
            select.join(" "),
            pattern.join("\n  "),
            limit + 1
        ),
        set,
        columns,
    ))
}

fn literal(s: &str) -> String {
    tripletrail::oxigraph::model::Literal::new_simple_literal(s).to_string()
}

/// A number as SPARQL writes it, or None.
fn number(s: &str) -> Option<String> {
    let s = s.trim();
    let n: f64 = s.parse().ok()?;
    (n.is_finite() && !s.contains(['i', 'n', 'I', 'N']))
        .then(|| s.trim_start_matches('+').to_owned())
}

/// The most values `/search/values` gives.
const MAX_VALUES: usize = 100;

/// `GET /{ds}/search/values?m=&key=&at=&q=&limit=`: the values `key` takes among the
/// resources the search finds, most frequent first, with how many resources have each (and a
/// reference's label): to suggest values for a condition. `at` is the index of the condition
/// being edited, left out of the search; `q` keeps the values whose text (or label) contains it.
pub(crate) fn values(
    state: &State,
    entry: &Entry,
    params: &Params,
    timeout: Option<Duration>,
    view: &View,
) -> Result<Reply, HttpError> {
    use tripletrail::oxigraph::model::Term;
    let m = model(params)?;
    let key = params
        .get("key")
        .ok_or_else(|| HttpError::bad_request("Missing the key parameter"))?;
    let at = match params.get("at") {
        Some(a) => Some(
            a.parse::<usize>()
                .map_err(|_| HttpError::bad_request("at is the index of a condition"))?,
        ),
        None => None,
    };
    let limit = match params.get("limit") {
        Some(l) => l
            .parse::<usize>()
            .ok()
            .filter(|l| (1..=MAX_VALUES).contains(l))
            .ok_or_else(|| {
                HttpError::bad_request(format!("limit is a number from 1 to {MAX_VALUES}"))
            })?,
        None => 20,
    };
    let sets = sets_for(state, entry, view, Reader::Search)?;
    let (set, mut pattern, _) = pattern(&sets, &m, at, Some((entry, view)))?;
    let field = set
        .fields
        .iter()
        .find(|f| f.key == *key)
        .ok_or_else(|| HttpError(422, format!("{} has no key {key}", set.title)))?;
    let (kind, _) = kind(&sets, field);
    let geometry = set
        .geometry
        .as_ref()
        .and_then(|g| g.path())
        .is_some_and(|g| g.starts_with(&field.path));
    if geometry || kind == "object" {
        return Err(HttpError(422, format!("{key} has no values to list")));
    }
    pattern.push(format!("?s {} ?v .", field.path));
    let label = reference(&sets, field).and_then(|t| t.label.clone());
    if let Some(label) = &label {
        pattern.push(format!("OPTIONAL {{ ?v {label} ?vl }}"));
    }
    if let Some(q) = params.get("q").map(|q| q.trim()).filter(|q| !q.is_empty()) {
        let text = if label.is_some() {
            "COALESCE(?vl, STR(?v))"
        } else {
            "STR(?v)"
        };
        pattern.push(format!(
            "FILTER(CONTAINS(LCASE({text}), {}))",
            literal(&q.to_lowercase())
        ));
    }
    let query = format!(
        "SELECT ?v (COUNT(DISTINCT ?s) AS ?n) (MIN(?vl) AS ?l) WHERE {{\n  {}\n}}\nGROUP BY ?v\nORDER BY DESC(?n) ?v\nLIMIT {}",
        pattern.join("\n  "),
        limit + 1
    );
    let found = crate::ogc::run(entry, &query, timeout, view)?;
    let more = found.len() > limit;
    let values: Vec<_> = found
        .iter()
        .take(limit)
        .filter_map(|r| {
            let count = match &r[1] {
                Some(Term::Literal(l)) => l.value().parse::<u64>().unwrap_or(0),
                _ => 0,
            };
            let mut v = match r[0].as_ref()? {
                Term::NamedNode(n) => json!({"value": n.as_str(), "iri": true}),
                Term::Literal(l) => json!({"value": l.value()}),
                other => json!({"value": other.to_string()}),
            };
            if let Some(Term::Literal(l)) = &r[2] {
                v["label"] = l.value().into();
            }
            v["count"] = count.into();
            Some(v)
        })
        .collect();
    let body = json!({"key": key, "kind": kind, "values": values, "more": more});
    Ok(Reply::bytes(
        200,
        "application/json",
        serde_json::to_vec(&body).expect("JSON"),
    ))
}

/// `GET /{ds}/search/sparql?m=`: the SPARQL of a search, to run or edit elsewhere.
pub(crate) fn query_text(
    state: &State,
    entry: &Entry,
    params: &Params,
    view: &View,
) -> Result<Reply, HttpError> {
    let m = model(params)?;
    let sets = sets_for(state, entry, view, Reader::Search)?;
    let (query, _, _) = sparql(&sets, &m, rows(params)?, 0, Ask::Whole, None)?;
    Ok(Reply::bytes(
        200,
        "application/sparql-query; charset=utf-8",
        query.into_bytes(),
    ))
}

fn rows(params: &Params) -> Result<usize, HttpError> {
    rows_up_to(params, MAX_ROWS)
}

/// `limit`, from 1 to `max`, or [`DEFAULT_ROWS`].
fn rows_up_to(params: &Params, max: usize) -> Result<usize, HttpError> {
    match params.get("limit") {
        Some(l) => l
            .parse::<usize>()
            .ok()
            .filter(|l| (1..=max).contains(l))
            .ok_or_else(|| HttpError::bad_request(format!("limit is a number from 1 to {max}"))),
        None => Ok(DEFAULT_ROWS),
    }
}

/// The most rows a CSV of a search gives (`f=csv`, #81).
const MAX_CSV_ROWS: usize = 10_000;
/// The furthest a search pages (`offset`).
const MAX_OFFSET: usize = 1_000_000;

/// `GET /{ds}/search/results?m=`: the resources a search finds: how many, and the first
/// `limit` (100; at most 1,000) by label, each with its label and the values of the
/// conditions' and shown properties (a reference with its label), and the SPARQL it ran.
pub(crate) fn results(
    state: &State,
    entry: &Entry,
    params: &Params,
    origin: &str,
    ds: &str,
    timeout: Option<Duration>,
    view: &View,
) -> Result<Reply, HttpError> {
    use std::collections::HashMap;
    use tripletrail::oxigraph::model::{NamedNode, Term};
    let m = model(params)?;
    let csv = match params.get("f") {
        None | Some("json") => false,
        Some("csv") => true,
        Some(other) => {
            return Err(HttpError::bad_request(format!(
                "Unknown format {other}: json or csv"
            )));
        }
    };
    let limit = rows_up_to(params, if csv { MAX_CSV_ROWS } else { MAX_ROWS })?;
    let offset = match params.get("offset") {
        None => 0,
        Some(o) => o
            .parse::<usize>()
            .ok()
            .filter(|o| *o <= MAX_OFFSET)
            .ok_or_else(|| {
                HttpError::bad_request(format!("offset is a number from 0 to {MAX_OFFSET}"))
            })?,
    };
    let sets = sets_for(state, entry, view, Reader::Search)?;
    let (query, set, columns) = sparql(&sets, &m, limit, offset, Ask::Rows, Some((entry, view)))?;
    let (counting, _, _) = sparql(&sets, &m, limit, 0, Ask::Count, Some((entry, view)))?;
    let deadline = timeout.map(|t| std::time::Instant::now() + t);
    let left = || deadline.map(|d| d.saturating_duration_since(std::time::Instant::now()));
    let found = crate::ogc::run(entry, &query, left(), view)?;
    let total = match crate::ogc::run(entry, &counting, left(), view)?
        .first()
        .and_then(|r| r.first().cloned().flatten())
    {
        Some(Term::Literal(l)) => l.value().parse::<u64>().unwrap_or(0),
        _ => 0,
    };
    let encode = |s: &str| form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    let shown: Vec<(&NamedNode, &Vec<Option<Term>>)> = found
        .iter()
        .take(limit)
        .filter_map(|r| match &r[0] {
            Some(Term::NamedNode(s)) => Some((s, r)),
            _ => None,
        })
        .collect();
    // Each row's values per column: from the search's row, or for a column with several
    // values from a query of its own on the rows shown, the first MAX_SHOWN in order, with
    // how many more there are.
    let mut cells: Vec<Vec<(Vec<Term>, usize)>> = vec![Vec::new(); shown.len()];
    let mut at = 2;
    for c in &columns {
        if !c.many {
            for (row, (_, r)) in cells.iter_mut().zip(&shown) {
                row.push((r[at].clone().into_iter().collect(), 0));
            }
            at += 1;
            continue;
        }
        let mut of: HashMap<&NamedNode, (Vec<Term>, usize)> = HashMap::new();
        if !shown.is_empty() {
            // References in the order of their labels (the IRI where there is none), as the
            // rows are; other values in their own order.
            let order = match &c.label {
                Some(path) => format!(
                    "OPTIONAL {{ ?v {path} ?label }} }} GROUP BY ?s ?v ORDER BY ?s (COALESCE(MIN(STR(?label)), STR(?v))) ?v"
                ),
                None => "} GROUP BY ?s ?v ORDER BY ?s ?v".to_owned(),
            };
            let query = format!(
                "SELECT ?s ?v WHERE {{ VALUES ?s {{ {} }} ?s {} ?v {order}",
                shown
                    .iter()
                    .map(|(s, _)| s.to_string())
                    .collect::<Vec<_>>()
                    .join(" "),
                c.field.path
            );
            let iris: HashMap<&NamedNode, &NamedNode> =
                shown.iter().map(|(s, _)| (*s, *s)).collect();
            for r in crate::ogc::run(entry, &query, left(), view)? {
                if let (Some(Term::NamedNode(s)), Some(v)) = (&r[0], &r[1])
                    && let Some(s) = iris.get(s)
                {
                    let e = of.entry(s).or_default();
                    if e.0.len() < MAX_SHOWN {
                        e.0.push(v.clone());
                    } else {
                        e.1 += 1;
                    }
                }
            }
        }
        for (row, (s, _)) in cells.iter_mut().zip(&shown) {
            row.push(of.remove(s).unwrap_or_default());
        }
    }
    // The labels of the references shown, per label path: one query for each.
    let mut labels: HashMap<(&String, NamedNode), String> = HashMap::new();
    let mut paths: Vec<&String> = columns.iter().filter_map(|c| c.label.as_ref()).collect();
    paths.sort();
    paths.dedup();
    for path in paths {
        let mut iris: Vec<&NamedNode> = cells
            .iter()
            .flat_map(|row| {
                columns
                    .iter()
                    .zip(row)
                    .filter(|(c, _)| c.label.as_ref() == Some(path))
                    .flat_map(|(_, (terms, _))| terms)
                    .filter_map(|t| match t {
                        Term::NamedNode(n) => Some(n),
                        _ => None,
                    })
            })
            .collect();
        iris.sort();
        iris.dedup();
        if iris.is_empty() {
            continue;
        }
        let query = format!(
            "SELECT ?o (MIN(?label) AS ?l) WHERE {{ VALUES ?o {{ {} }} ?o {path} ?label }} GROUP BY ?o",
            iris.iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        );
        for r in crate::ogc::run(entry, &query, left(), view)? {
            if let (Some(Term::NamedNode(o)), Some(Term::Literal(l))) = (&r[0], &r[1]) {
                labels.insert((path, o.clone()), l.value().to_owned());
            }
        }
    }
    let value = |t: &Option<Term>, label: Option<&String>| match t {
        None => serde_json::Value::Null,
        Some(Term::NamedNode(n)) => {
            let mut v = json!({
                "iri": n.as_str(),
                "describe": format!("{origin}/{ds}/describe?iri={}", encode(n.as_str())),
            });
            if let Some(l) = label {
                v["label"] = l.as_str().into();
            }
            v
        }
        Some(Term::Literal(l)) => {
            let dt = l.datatype().as_str();
            let number = matches!(
                dt.strip_prefix("http://www.w3.org/2001/XMLSchema#"),
                Some("integer" | "decimal" | "double" | "float" | "int" | "long" | "short")
            );
            if number
                && let Some(n) = l
                    .value()
                    .parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
            {
                if dt.ends_with("#integer")
                    && let Ok(i) = l.value().parse::<i64>()
                {
                    return i.into();
                }
                return serde_json::Value::Number(n);
            }
            if dt.ends_with("#boolean") {
                return (l.value() == "true" || l.value() == "1").into();
            }
            l.value().into()
        }
        Some(other) => other.to_string().into(),
    };
    let more = found.len() > limit;
    let rows: Vec<_> = shown
        .iter()
        .zip(&cells)
        .map(|((s, r), row)| {
            let mut values = serde_json::Map::new();
            let mut left_out = serde_json::Map::new();
            for (c, (terms, rest)) in columns.iter().zip(row) {
                let one = |t: &Term| {
                    let label = match (&c.label, t) {
                        (Some(path), Term::NamedNode(n)) => labels.get(&(path, n.clone())),
                        _ => None,
                    };
                    value(&Some(t.clone()), label)
                };
                let v = if c.many {
                    serde_json::Value::Array(terms.iter().map(one).collect())
                } else {
                    terms.first().map(one).unwrap_or(serde_json::Value::Null)
                };
                values.insert(c.field.key.clone(), v);
                if *rest > 0 {
                    left_out.insert(c.field.key.clone(), (*rest).into());
                }
            }
            let label = match &r[1] {
                Some(Term::Literal(l)) => Some(l.value().to_owned()),
                _ => None,
            };
            let mut row = json!({
                "iri": s.as_str(),
                "label": label,
                "describe": format!("{origin}/{ds}/describe?iri={}", encode(s.as_str())),
                "values": values,
            });
            if !left_out.is_empty() {
                row["moreValues"] = left_out.into();
            }
            row
        })
        .collect();
    let columns: Vec<_> = columns
        .iter()
        .map(|c| {
            json!({
                "key": c.field.key,
                "title": c.field.schema.get("title").and_then(|t| t.as_str()).unwrap_or(&c.field.key),
                "kind": kind(&sets, c.field).0,
            })
        })
        .collect();
    if csv {
        return Ok(csv_reply(&set.slug, &columns, &rows));
    }
    let body = json!({
        "class": set.slug,
        "title": set.title,
        "count": total,
        "offset": offset,
        "more": more,
        "columns": columns,
        "rows": rows,
        "sparql": query,
    });
    Ok(Reply::bytes(
        200,
        "application/json",
        serde_json::to_vec(&body).expect("JSON"),
    ))
}

/// The rows of a search as CSV (#81): `iri`, `label`, then a column per key; a resource is its
/// IRI, several values are joined with ` | `, and a cell that a spreadsheet would run as a
/// formula gets a `'` before it, as the REST API's CSV does.
fn csv_reply(class: &str, columns: &[serde_json::Value], rows: &[serde_json::Value]) -> Reply {
    fn cell(v: &serde_json::Value) -> String {
        match v {
            serde_json::Value::Null => String::new(),
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(a) => a.iter().map(cell).collect::<Vec<_>>().join(" | "),
            serde_json::Value::Object(o) => o
                .get("iri")
                .and_then(|i| i.as_str())
                .unwrap_or_default()
                .to_owned(),
            other => other.to_string(),
        }
    }
    fn quote(text: &str) -> String {
        let risky =
            text.starts_with(['=', '+', '-', '@', '\t', '\r']) && text.parse::<f64>().is_err();
        let text = if risky {
            format!("'{text}")
        } else {
            text.to_owned()
        };
        if text.contains([',', '"', '\n', '\r']) {
            format!("\"{}\"", text.replace('"', "\"\""))
        } else {
            text
        }
    }
    let keys: Vec<&str> = columns.iter().filter_map(|c| c["key"].as_str()).collect();
    let mut out = String::new();
    let header: Vec<String> = ["iri", "label"]
        .into_iter()
        .chain(keys.iter().copied())
        .map(quote)
        .collect();
    out.push_str(&header.join(","));
    out.push_str("\r\n");
    for row in rows {
        let mut line = vec![quote(&cell(&row["iri"])), quote(&cell(&row["label"]))];
        for k in &keys {
            line.push(quote(&cell(&row["values"][*k])));
        }
        out.push_str(&line.join(","));
        out.push_str("\r\n");
    }
    let mut reply = Reply::bytes(200, "text/csv; charset=utf-8", out.into_bytes());
    reply.headers.push((
        "Content-Disposition",
        format!(
            "attachment; filename=\"{}.csv\"",
            class.replace(['"', '\\', '/'], "_")
        ),
    ));
    reply
}
