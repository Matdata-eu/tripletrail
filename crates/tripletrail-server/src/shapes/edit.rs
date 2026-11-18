//! The shapes editor's routes (#145): `GET …/shapes/outline`, every node shape of the layers
//! with its property shapes and their reader settings, and `POST …/shapes/edit`, which
//! changes those settings in the dataset's own set, as its next version.

use super::{
    Active, Binding, SetFile, WRITES, active, current, invalid, own_files, versions, write_version,
};
use crate::admin::Entry;
use crate::{HttpError, Params, State, View};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use tripletrail::oxigraph::io::{RdfFormat, RdfParser};
use tripletrail::oxigraph::model::vocab::{rdf, rdfs, xsd};
use tripletrail::oxigraph::model::{
    Graph, Literal, NamedNode, NamedNodeRef, NamedOrBlankNode, NamedOrBlankNodeRef, Term, TermRef,
    Triple,
};
use tripletrail::shapes;

const SH: &str = "http://www.w3.org/ns/shacl#";

fn sh(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{SH}{local}"))
}
fn tts(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{}{local}", shapes::TTS))
}

/// A setting the editor changes: its name in the API, its predicate, and its kind.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Text,
    Number,
    Flag,
    Iri,
    /// Readers, by name (`map`, `describe`…), as their `tts:Reader-…` concepts.
    Readers,
    /// A list of IRIs.
    Iris,
}

const SETTINGS: [(&str, &str, Kind); 17] = [
    ("name", "http://www.w3.org/ns/shacl#name", Kind::Text),
    (
        "description",
        "http://www.w3.org/ns/shacl#description",
        Kind::Text,
    ),
    ("order", "http://www.w3.org/ns/shacl#order", Kind::Number),
    ("group", "http://www.w3.org/ns/shacl#group", Kind::Iri),
    ("hidden", "http://datashapes.org/dash#hidden", Kind::Flag),
    (
        "hiddenFrom",
        "http://w3id.org/tripletrail/shapes#hiddenFrom",
        Kind::Readers,
    ),
    (
        "onlyFor",
        "http://w3id.org/tripletrail/shapes#onlyFor",
        Kind::Readers,
    ),
    (
        "deactivated",
        "http://www.w3.org/ns/shacl#deactivated",
        Kind::Flag,
    ),
    ("viewer", "http://datashapes.org/dash#viewer", Kind::Iri),
    ("key", "http://w3id.org/tripletrail/shapes#key", Kind::Text),
    (
        "label",
        "http://www.w3.org/2000/01/rdf-schema#label",
        Kind::Text,
    ),
    (
        "profiles",
        "http://w3id.org/tripletrail/shapes#inProfile",
        Kind::Iris,
    ),
    (
        "color",
        "http://w3id.org/tripletrail/shapes#color",
        Kind::Text,
    ),
    (
        "minZoom",
        "http://w3id.org/tripletrail/shapes#minZoom",
        Kind::Number,
    ),
    (
        "drawOrder",
        "http://w3id.org/tripletrail/shapes#drawOrder",
        Kind::Number,
    ),
    (
        "facet",
        "http://w3id.org/tripletrail/shapes#facet",
        Kind::Iri,
    ),
    ("time", "http://w3id.org/tripletrail/shapes#time", Kind::Iri),
];

/// The editor shapes of the forms.
const EDITOR_SHAPES: &str = include_str!("editor.ttl");

/// `GET …/shapes/form`: the fields of the forms for a node shape (`node`) and a property shape
/// (`property`), read from the editor shapes (`editor.ttl`): each with its `setting` (of the
/// edit API), `label`, `description`, `editor` (the local name of its `dash:editor`),
/// `multiple` (no `sh:maxCount 1`), `datatype`, `pattern`, `min` and `max`, and the values to
/// choose from: `in` (`{value, label}`, from `sh:in`, or the readers for `sh:class tts:Reader`)
/// or `class` (the IRI of the class whose instances are offered, for `dash:InstancesSelectEditor`).
pub(crate) fn form() -> Result<Value, HttpError> {
    let g = parse(EDITOR_SHAPES).map_err(|e| HttpError(500, format!("editor.ttl: {e}")))?;
    let vocabulary = parse(shapes::VOCABULARY).unwrap_or_default();
    let dash = |l: &str| NamedNode::new_unchecked(format!("http://datashapes.org/dash#{l}"));
    let text = |g: &Graph, s: NamedOrBlankNodeRef<'_>, p: &NamedNode| match g
        .object_for_subject_predicate(s, p)
    {
        Some(TermRef::Literal(l)) => Some(l.value().to_owned()),
        _ => None,
    };
    let local = |iri: &str| iri.rsplit(['#', '/', ':']).next().unwrap_or(iri).to_owned();
    let mut out = Map::new();
    for (key, class) in [("node", "NodeShape"), ("property", "PropertyShape")] {
        let Some(shape) = g
            .subjects_for_predicate_object(&sh("targetClass"), &sh(class))
            .next()
        else {
            continue;
        };
        let mut fields: Vec<(f64, Value)> = Vec::new();
        for f in g
            .objects_for_subject_predicate(shape, &sh("property"))
            .filter_map(node_of)
        {
            let Some(TermRef::NamedNode(path)) = g.object_for_subject_predicate(&f, &sh("path"))
            else {
                continue;
            };
            let Some((name, _, kind)) = SETTINGS.iter().find(|(_, p, _)| *p == path.as_str())
            else {
                return Err(HttpError(
                    500,
                    format!("editor.ttl: {} is no setting of the editor", path.as_str()),
                ));
            };
            let order = text(&g, f.as_ref(), &sh("order"))
                .and_then(|o| o.parse::<f64>().ok())
                .unwrap_or(f64::MAX);
            let mut v = Map::new();
            v.insert("setting".into(), json!(name));
            v.insert("label".into(), json!(text(&g, f.as_ref(), &sh("name"))));
            if let Some(d) = text(&g, f.as_ref(), &sh("description")) {
                v.insert("description".into(), json!(d));
            }
            if let Some(TermRef::NamedNode(e)) = g.object_for_subject_predicate(&f, &dash("editor"))
            {
                v.insert("editor".into(), json!(local(e.as_str())));
            }
            v.insert(
                "multiple".into(),
                json!(text(&g, f.as_ref(), &sh("maxCount")).as_deref() != Some("1")),
            );
            if let Some(TermRef::NamedNode(d)) = g.object_for_subject_predicate(&f, &sh("datatype"))
            {
                v.insert("datatype".into(), json!(local(d.as_str())));
            }
            if let Some(p) = text(&g, f.as_ref(), &sh("pattern")) {
                v.insert("pattern".into(), json!(p));
            }
            for (k, p) in [("min", "minInclusive"), ("max", "maxInclusive")] {
                if let Some(n) = text(&g, f.as_ref(), &sh(p)).and_then(|n| n.parse::<f64>().ok()) {
                    v.insert(k.into(), json!(n));
                }
            }
            let label_of = |iri: NamedNodeRef<'_>| -> String {
                [
                    NamedNode::new_unchecked(rdfs::LABEL.as_str()),
                    skos("prefLabel"),
                ]
                .iter()
                .find_map(|p| match vocabulary.object_for_subject_predicate(iri, p) {
                    Some(TermRef::Literal(l)) => Some(l.value().to_owned()),
                    _ => None,
                })
                .unwrap_or_else(|| local(iri.as_str()))
            };
            if *kind == Kind::Readers {
                let readers: Vec<Value> = shapes::Reader::ALL
                    .iter()
                    .map(|r| json!({ "value": r.name(), "label": label_of(r.iri().as_ref()) }))
                    .collect();
                v.insert("in".into(), json!(readers));
            } else if let Some(list) = g.object_for_subject_predicate(&f, &sh("in")) {
                let mut values = Vec::new();
                let mut at = list.into_owned();
                while let Some(item) = subject_of_term(&at)
                    .and_then(|n| g.object_for_subject_predicate(&n, rdf::FIRST))
                {
                    if let TermRef::NamedNode(i) = item {
                        values.push(json!({ "value": i.as_str(), "label": label_of(i) }));
                    }
                    match subject_of_term(&at)
                        .and_then(|n| g.object_for_subject_predicate(&n, rdf::REST))
                    {
                        Some(next) => at = next.into_owned(),
                        None => break,
                    }
                }
                v.insert("in".into(), json!(values));
            } else if let Some(TermRef::NamedNode(c)) =
                g.object_for_subject_predicate(&f, &sh("class"))
            {
                v.insert("class".into(), json!(c.as_str()));
            }
            fields.push((order, Value::Object(v)));
        }
        fields.sort_by(|a, b| a.0.total_cmp(&b.0));
        out.insert(
            key.into(),
            json!(fields.into_iter().map(|(_, v)| v).collect::<Vec<_>>()),
        );
    }
    Ok(Value::Object(out))
}

fn subject_of_term(t: &Term) -> Option<NamedOrBlankNode> {
    match t {
        Term::NamedNode(n) => Some(n.clone().into()),
        Term::BlankNode(b) => Some(b.clone().into()),
        _ => None,
    }
}

fn setting(name: &str) -> Result<(NamedNode, Kind), HttpError> {
    SETTINGS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, p, k)| (NamedNode::new_unchecked(*p), *k))
        .ok_or_else(|| {
            HttpError::bad_request(format!(
                "Unknown setting {name}: the settings are {}",
                SETTINGS.map(|(n, _, _)| n).join(", ")
            ))
        })
}

/// The node shapes of a graph: typed `sh:NodeShape`, or with a target class or property shapes.
fn node_shapes(graph: &Graph) -> Vec<NamedOrBlankNode> {
    let mut out: Vec<NamedOrBlankNode> = Vec::new();
    let typed = graph
        .subjects_for_predicate_object(rdf::TYPE, &sh("NodeShape"))
        .map(NamedOrBlankNodeRef::into_owned);
    let others: Vec<NamedOrBlankNode> = [sh("targetClass"), sh("property")]
        .iter()
        .flat_map(|p| {
            graph
                .triples_for_predicate(p)
                .map(|t| t.subject.into_owned())
                .collect::<Vec<_>>()
        })
        .collect();
    // A property shape with property shapes of its own (sh:node) is not listed as a node shape.
    let properties: std::collections::HashSet<NamedOrBlankNode> = graph
        .triples_for_predicate(&sh("property"))
        .filter_map(|t| node_of(t.object))
        .collect();
    for n in typed.chain(others) {
        if !out.contains(&n) && !properties.contains(&n) {
            out.push(n);
        }
    }
    out
}

fn node_of(t: TermRef<'_>) -> Option<NamedOrBlankNode> {
    match t {
        TermRef::NamedNode(n) => Some(n.into_owned().into()),
        TermRef::BlankNode(b) => Some(b.into_owned().into()),
        _ => None,
    }
}

/// A term as the API gives it: an IRI as itself, a literal as its value.
fn term_json(t: TermRef<'_>) -> Value {
    match t {
        TermRef::NamedNode(n) => json!(n.as_str()),
        TermRef::Literal(l) => {
            let dt = l.datatype();
            if dt == xsd::BOOLEAN {
                json!(l.value() == "true")
            } else if let (true, Ok(i)) = (
                [xsd::INTEGER, xsd::INT, xsd::LONG].contains(&dt),
                l.value().parse::<i64>(),
            ) {
                json!(i)
            } else if [xsd::INTEGER, xsd::DECIMAL, xsd::DOUBLE, xsd::INT].contains(&dt) {
                l.value()
                    .parse::<f64>()
                    .map_or_else(|_| json!(l.value()), |v| json!(v))
            } else {
                json!(l.value())
            }
        }
        TermRef::BlankNode(_) => Value::Null,
        #[allow(unreachable_patterns)]
        _ => Value::Null,
    }
}

/// The settings of `subject` (a node or property shape): one value each, and the names by
/// language (`names`).
fn settings_of(graph: &Graph, subject: NamedOrBlankNodeRef<'_>) -> Map<String, Value> {
    let mut out = Map::new();
    for (name, p, kind) in SETTINGS {
        let p = NamedNode::new_unchecked(p);
        let values: Vec<TermRef<'_>> = graph.objects_for_subject_predicate(subject, &p).collect();
        if values.is_empty() {
            continue;
        }
        if kind == Kind::Readers {
            // In the readers' order, then any other IRI.
            let mut names: Vec<(usize, Value)> = values
                .iter()
                .map(|v| match v {
                    TermRef::NamedNode(n) => shapes::Reader::ALL
                        .iter()
                        .position(|r| r.iri().as_ref() == *n)
                        .map_or_else(
                            || (usize::MAX, json!(n.as_str())),
                            |i| (i, json!(shapes::Reader::ALL[i].name())),
                        ),
                    _ => (usize::MAX, Value::Null),
                })
                .collect();
            names.sort_by_key(|n| n.0);
            let names: Vec<Value> = names.into_iter().map(|(_, v)| v).collect();
            out.insert(name.to_owned(), json!(names));
        } else if kind == Kind::Iris {
            let mut iris: Vec<Value> = values.iter().map(|v| term_json(*v)).collect();
            iris.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
            out.insert(name.to_owned(), json!(iris));
        } else if kind == Kind::Text {
            let mut by_lang = Map::new();
            for v in &values {
                if let TermRef::Literal(l) = v {
                    by_lang.insert(l.language().unwrap_or("").to_owned(), json!(l.value()));
                }
            }
            let first = by_lang
                .get("")
                .or_else(|| by_lang.get("en"))
                .or_else(|| by_lang.values().next())
                .cloned()
                .unwrap_or(Value::Null);
            out.insert(name.to_owned(), first);
            if by_lang.len() > 1 || by_lang.keys().any(|k| !k.is_empty()) {
                out.insert(format!("{name}ByLanguage"), Value::Object(by_lang));
            }
        } else {
            out.insert(name.to_owned(), term_json(values[0]));
        }
    }
    out
}

/// A path as text: an IRI, or the Turtle of a complex path.
fn path_of(graph: &Graph, property: NamedOrBlankNodeRef<'_>) -> Option<String> {
    match graph.object_for_subject_predicate(property, &sh("path"))? {
        TermRef::NamedNode(n) => Some(n.as_str().to_owned()),
        _ => Some("(a complex path)".to_owned()),
    }
}

/// The node shapes of each own file, by IRI.
fn files_by_shape(files: &[SetFile]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for f in files {
        if let Ok(g) = parse(&f.text) {
            for n in node_shapes(&g) {
                if let NamedOrBlankNode::NamedNode(n) = n {
                    out.entry(n.as_str().to_owned()).or_insert(f.name.clone());
                }
            }
        }
    }
    out
}

fn parse(text: &str) -> Result<Graph, String> {
    let mut g = Graph::new();
    for q in RdfParser::from_format(RdfFormat::Turtle).for_slice(text.as_bytes()) {
        let q = q.map_err(|e| e.to_string())?;
        g.insert(&Triple::from(q));
    }
    Ok(g)
}

/// The layers together, as the readers read them before projection: generated < pack < own.
fn layered(a: &Active) -> Graph {
    let mut all = a.generated.clone();
    if let Some(pack) = &a.pack {
        all = shapes::overlay(&all, pack);
    }
    if let Some(own) = &a.own {
        all = shapes::overlay(&all, own);
    }
    all
}

fn has(graph: Option<&Graph>, node: &NamedOrBlankNode) -> bool {
    graph.is_some_and(|g| g.triples_for_subject(node).next().is_some())
}

/// `GET …/shapes/outline`.
pub(crate) fn outline(
    state: &State,
    entry: &Entry,
    folder: Option<&Path>,
    params: &Params,
) -> Result<Value, HttpError> {
    let a = active(state, entry, params, &View::default(), None)?;
    let all = layered(&a);
    let own = folder.map(own_files).transpose()?.flatten();
    let files = own
        .as_ref()
        .map(|(_, f)| files_by_shape(f))
        .unwrap_or_default();
    let classes_of = |n: &NamedOrBlankNode| -> Vec<Value> {
        all.objects_for_subject_predicate(n, &sh("targetClass"))
            .map(term_json)
            .collect()
    };
    let mut list: Vec<Value> = node_shapes(&all)
        .into_iter()
        .map(|n| {
            let layer = if has(a.own.as_ref(), &n) {
                "own"
            } else if has(a.pack.as_ref(), &n) {
                "pack"
            } else {
                "generated"
            };
            let below = match layer {
                "own" => has(a.pack.as_ref(), &n) || has(Some(&a.generated), &n),
                "pack" => has(Some(&a.generated), &n),
                _ => false,
            };
            let mut properties: Vec<Value> = all
                .objects_for_subject_predicate(&n, &sh("property"))
                .filter_map(node_of)
                .map(|p| {
                    let mut v = settings_of(&all, p.as_ref());
                    v.insert("path".into(), json!(path_of(&all, p.as_ref())));
                    // What it links to or holds, for the diagram: sh:class, else sh:datatype.
                    for (key, predicate) in [("class", "class"), ("datatype", "datatype")] {
                        if let Some(TermRef::NamedNode(t)) =
                            all.object_for_subject_predicate(&p, &sh(predicate))
                        {
                            v.insert(key.into(), json!(t.as_str()));
                        }
                    }
                    if let NamedOrBlankNode::NamedNode(iri) = &p {
                        v.insert("iri".into(), json!(iri.as_str()));
                    }
                    Value::Object(v)
                })
                .collect();
            // In the readers' order: sh:order, then the path.
            properties.sort_by(|a, b| {
                let o = |v: &Value| v["order"].as_f64().unwrap_or(f64::MAX);
                o(a).total_cmp(&o(b))
                    .then_with(|| a["path"].as_str().cmp(&b["path"].as_str()))
            });
            let mut v = settings_of(&all, n.as_ref());
            let iri = match &n {
                NamedOrBlankNode::NamedNode(i) => json!(i.as_str()),
                NamedOrBlankNode::BlankNode(_) => Value::Null,
            };
            v.insert("iri".into(), iri.clone());
            v.insert("classes".into(), json!(classes_of(&n)));
            v.insert("layer".into(), json!(layer));
            v.insert("overrides".into(), json!(below));
            // Whether it changes the shape below (`tts:refines`) rather than replacing it.
            let refines = matches!(
                all.object_for_subject_predicate(&n, &tts("refines")),
                Some(TermRef::Literal(l)) if l.value() == "true"
            );
            v.insert("refines".into(), json!(below && refines));
            if let Some(file) = iri.as_str().and_then(|i| files.get(i)) {
                v.insert("file".into(), json!(file));
            }
            if let Some(TermRef::Literal(l)) =
                all.object_for_subject_predicate(&n, &tts("instances"))
            {
                v.insert("instances".into(), json!(l.value().parse::<u64>().ok()));
            }
            v.insert("properties".into(), json!(properties));
            Value::Object(v)
        })
        .collect();
    // By label, then IRI.
    list.sort_by_key(|v| {
        (
            v["label"]
                .as_str()
                .or(v["name"].as_str())
                .unwrap_or("")
                .to_lowercase(),
            v["iri"].as_str().unwrap_or("").to_owned(),
        )
    });
    let bound = folder.and_then(Binding::read);
    Ok(json!({
        "dataset": entry.name,
        "source": a.source,
        "version": folder.and_then(current),
        "versions": folder.map(versions).unwrap_or_default(),
        "bound": bound.as_ref().map(|b| b.url.clone()),
        "editable": folder.is_some() && bound.is_none() && entry.mode != crate::admin::Mode::ReadOnly,
        "settings": SETTINGS.map(|(n, _, _)| n),
        "profiles": profiles_of(&all),
        "shapes": list,
    }))
}

/// The profiles the shapes name: those declared (`a tts:Profile`) and those a shape is in,
/// with their label and the number of node shapes in each.
fn profiles_of(all: &Graph) -> Vec<Value> {
    let in_profile = tts("inProfile");
    let mut iris: Vec<NamedNode> = all
        .subjects_for_predicate_object(rdf::TYPE, &tts("Profile"))
        .filter_map(|s| match s {
            NamedOrBlankNodeRef::NamedNode(n) => Some(n.into_owned()),
            _ => None,
        })
        .chain(
            all.triples_for_predicate(&in_profile)
                .filter_map(|t| match t.object {
                    TermRef::NamedNode(n) => Some(n.into_owned()),
                    _ => None,
                }),
        )
        .collect();
    iris.sort();
    iris.dedup();
    iris.iter()
        .map(|p| {
            let label = [
                skos("prefLabel"),
                NamedNode::new_unchecked(rdfs::LABEL.as_str()),
            ]
            .iter()
            .find_map(|l| match all.object_for_subject_predicate(p, l) {
                Some(TermRef::Literal(l)) => Some(l.value().to_owned()),
                _ => None,
            });
            let shapes = all.subjects_for_predicate_object(&in_profile, p).count();
            json!({ "iri": p.as_str(), "label": label, "shapes": shapes })
        })
        .collect()
}

fn skos(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("http://www.w3.org/2004/02/skos/core#{local}"))
}

/// One change: `{shape, path?, setting, value, lang?}`. `value: null` removes the setting.
struct Change {
    shape: NamedNode,
    path: Option<NamedNode>,
    predicate: NamedNode,
    kind: Kind,
    value: Value,
    lang: Option<String>,
}

fn change(v: &Value) -> Result<Change, HttpError> {
    let bad = |m: &str| HttpError::bad_request(format!("edit: {m}"));
    let shape = v["shape"]
        .as_str()
        .ok_or_else(|| bad("each change names its node shape as shape"))?;
    let shape = NamedNode::new(shape).map_err(|e| bad(&format!("shape {shape}: {e}")))?;
    let path = match v.get("path") {
        None | Some(Value::Null) => None,
        Some(Value::String(p)) => {
            Some(NamedNode::new(p).map_err(|e| bad(&format!("path {p}: {e}")))?)
        }
        Some(_) => return Err(bad("path is the IRI of the property")),
    };
    let name = v["setting"]
        .as_str()
        .ok_or_else(|| bad("each change names its setting"))?;
    let (predicate, kind) = setting(name)?;
    if kind == Kind::Iris && path.is_some() {
        return Err(bad(
            "profiles are set on node shapes: property shapes take their node shape's",
        ));
    }
    let lang = v["lang"]
        .as_str()
        .filter(|l| !l.is_empty())
        .map(str::to_owned);
    Ok(Change {
        shape,
        path,
        predicate,
        kind,
        value: v.get("value").cloned().unwrap_or(Value::Null),
        lang,
    })
}

/// The objects a change sets: none removes the setting.
fn objects(c: &Change) -> Result<Vec<Term>, HttpError> {
    let bad = |m: String| HttpError::bad_request(format!("edit: {m}"));
    if c.kind == Kind::Iris {
        let list = match &c.value {
            Value::Null => Vec::new(),
            Value::Array(list) => list.clone(),
            v => vec![v.clone()],
        };
        return list
            .iter()
            .map(|v| {
                v.as_str()
                    .and_then(|i| NamedNode::new(i).ok())
                    .map(Term::from)
                    .ok_or_else(|| bad(format!("{v} is not an IRI")))
            })
            .collect();
    }
    if c.kind == Kind::Readers {
        let list = match &c.value {
            Value::Null => Vec::new(),
            Value::Array(list) => list.clone(),
            v => vec![v.clone()],
        };
        return list
            .iter()
            .map(|r| {
                r.as_str()
                    .and_then(shapes::Reader::from_name)
                    .map(|r| Term::from(r.iri()))
                    .ok_or_else(|| {
                        bad(format!(
                            "{r} is not a reader: the readers are {}",
                            shapes::Reader::ALL.map(|r| r.name()).join(", ")
                        ))
                    })
            })
            .collect();
    }
    Ok(object(c)?.into_iter().collect())
}

/// The object a change sets, or `None` to remove the setting.
fn object(c: &Change) -> Result<Option<Term>, HttpError> {
    let bad = |m: String| HttpError::bad_request(format!("edit: {m}"));
    let name = c.predicate.as_str();
    Ok(match (&c.value, c.kind) {
        (Value::Null, _) => None,
        (Value::String(s), _) if s.is_empty() => None,
        (Value::String(s), Kind::Text) => Some(match &c.lang {
            Some(l) => Literal::new_language_tagged_literal(s, l)
                .map_err(|e| bad(format!("lang {l}: {e}")))?
                .into(),
            None => Literal::new_simple_literal(s).into(),
        }),
        (Value::Number(n), Kind::Number) => Some(if n.is_i64() || n.is_u64() {
            Literal::new_typed_literal(n.to_string(), xsd::INTEGER).into()
        } else {
            Literal::new_typed_literal(n.to_string(), xsd::DECIMAL).into()
        }),
        (Value::String(s), Kind::Number) => {
            s.parse::<f64>()
                .map_err(|_| bad(format!("{name} is a number, not {s}")))?;
            let dt = if s.contains(['.', 'e', 'E']) {
                xsd::DECIMAL
            } else {
                xsd::INTEGER
            };
            Some(Literal::new_typed_literal(s.trim(), dt).into())
        }
        (Value::Bool(b), Kind::Flag) => {
            // false is the default: no triple.
            b.then(|| Literal::new_typed_literal("true", xsd::BOOLEAN).into())
        }
        (Value::String(s), Kind::Iri) => Some(
            NamedNode::new(s)
                .map_err(|e| bad(format!("{name} is an IRI: {e}")))?
                .into(),
        ),
        (v, _) => return Err(bad(format!("{name} can't be {v}"))),
    })
}

/// The subject a change is about in `graph`: the node shape, or its property shape with the
/// path.
fn subject(graph: &Graph, c: &Change) -> Option<NamedOrBlankNode> {
    let node: NamedOrBlankNode = c.shape.clone().into();
    graph.triples_for_subject(&node).next()?;
    match &c.path {
        None => Some(node),
        Some(path) => graph
            .objects_for_subject_predicate(&node, &sh("property"))
            .filter_map(node_of)
            .find(|p| {
                graph.object_for_subject_predicate(p, &sh("path"))
                    == Some(TermRef::NamedNode(path.as_ref()))
            }),
    }
}

/// The layers together, read once.
fn below<'a>(
    lower: &'a mut Option<Graph>,
    state: &State,
    entry: &Entry,
    params: &Params,
) -> Result<&'a Graph, HttpError> {
    if lower.is_none() {
        let a = active(state, entry, params, &View::default(), None)?;
        *lower = Some(layered(&a));
    }
    Ok(lower.as_ref().expect("just set"))
}

/// Copies `node` and what it reaches through blank nodes and `sh:property` into `out`.
fn copy_shape(from: &Graph, out: &mut Graph, node: NamedOrBlankNodeRef<'_>) {
    let mut todo = vec![node.into_owned()];
    let mut seen = std::collections::HashSet::new();
    while let Some(s) = todo.pop() {
        if !seen.insert(s.clone()) {
            continue;
        }
        for t in from.triples_for_subject(&s) {
            out.insert(t);
            let follow = matches!(t.object, TermRef::BlankNode(_))
                || (t.predicate == sh("property").as_ref()
                    && matches!(t.object, TermRef::NamedNode(_)));
            if follow && let Some(o) = node_of(t.object) {
                todo.push(o);
            }
        }
    }
}

/// `POST …/shapes/edit` with `{"changes": [...]}`: the changes made in the own set's files
/// (a shape of a lower layer is first copied into a file of its own, which then overrides it)
/// and written as its next version. With `dry=true`, nothing is written: the answer gives the
/// files as they would be.
pub(crate) fn edit(
    state: &State,
    entry: &Entry,
    folder: &Path,
    params: &Params,
    body: &Value,
) -> Result<Value, HttpError> {
    let empty = Vec::new();
    let declared = body["profiles"].as_array().unwrap_or(&empty);
    let changes = body["changes"]
        .as_array()
        .or(Some(&empty))
        .filter(|c| !c.is_empty() || !declared.is_empty())
        .ok_or_else(|| {
            HttpError::bad_request(
                "edit takes {\"changes\": [{shape, path?, setting, value, lang?}], \"profiles\"?: [{iri, label}]}",
            )
        })?
        .iter()
        .map(change)
        .collect::<Result<Vec<_>, _>>()?;
    let dry = super::dry_run(params)?;
    let _write = WRITES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut files = own_files(folder)?.map(|(_, f)| f).unwrap_or_default();
    let mut graphs: Vec<Graph> = files
        .iter()
        .map(|f| parse(&f.text).map_err(|e| HttpError(422, format!("{}: {e}", f.name))))
        .collect::<Result<_, _>>()?;
    let mut changed = vec![false; files.len()];
    let mut lower: Option<Graph> = None;
    // Profiles declared, with their labels, in the file that declares profiles (or a new
    // `profiles.ttl`).
    for p in declared {
        let bad = |m: &str| HttpError::bad_request(format!("edit: profiles: {m}"));
        let iri = p["iri"]
            .as_str()
            .ok_or_else(|| bad("each profile has an iri"))?;
        let iri = NamedNode::new(iri).map_err(|e| bad(&format!("{iri}: {e}")))?;
        let label = p["label"].as_str().filter(|l| !l.trim().is_empty());
        let kind = Triple::new(iri.clone(), rdf::TYPE, tts("Profile"));
        let at = match graphs.iter().position(|g| g.contains(&kind)).or_else(|| {
            graphs.iter().position(|g| {
                g.subjects_for_predicate_object(rdf::TYPE, &tts("Profile"))
                    .next()
                    .is_some()
            })
        }) {
            Some(i) => i,
            None => {
                let taken: Vec<String> = files.iter().map(|f| f.name.to_lowercase()).collect();
                let mut name = "profiles.ttl".to_owned();
                let mut n = 2;
                while taken.contains(&name) {
                    name = format!("profiles_{n}.ttl");
                    n += 1;
                }
                files.push(SetFile {
                    name,
                    text: String::new(),
                });
                graphs.push(Graph::new());
                changed.push(true);
                graphs.len() - 1
            }
        };
        let g = &mut graphs[at];
        g.insert(&kind);
        if let Some(label) = label {
            let old: Vec<Triple> = g
                .triples_for_subject(&iri)
                .filter(|t| t.predicate == skos("prefLabel").as_ref())
                .map(|t| t.into_owned())
                .collect();
            for t in &old {
                g.remove(t);
            }
            g.insert(&Triple::new(
                iri.clone(),
                skos("prefLabel"),
                Literal::new_simple_literal(label.trim()),
            ));
        }
        changed[at] = true;
    }
    for c in &changes {
        let node: NamedOrBlankNode = c.shape.clone().into();
        let at = match graphs
            .iter()
            .position(|g| g.triples_for_subject(&node).next().is_some())
        {
            Some(i) => i,
            None => {
                // From the layers below: a file of its own, named as `files_of` names the
                // whole shape, holding a shape that refines it (`tts:refines`): what is
                // changed, so that the rest, and properties added to the data later, still
                // come from below.
                let below = below(&mut lower, state, entry, params)?;
                if below.triples_for_subject(&node).next().is_none() {
                    return Err(HttpError(404, format!("No node shape {}", c.shape)));
                }
                let mut whole = Graph::new();
                copy_shape(below, &mut whole, node.as_ref());
                let mut part = Graph::new();
                for t in below.triples_for_subject(&node) {
                    if [rdf::TYPE, sh("targetClass").as_ref()].contains(&t.predicate) {
                        part.insert(t);
                    }
                }
                part.insert(&Triple::new(
                    c.shape.clone(),
                    tts("refines"),
                    Literal::new_typed_literal("true", xsd::BOOLEAN),
                ));
                let taken: Vec<String> = files.iter().map(|f| f.name.to_lowercase()).collect();
                let mut name = shapes::files_of(&whole)
                    .into_iter()
                    .next()
                    .map_or_else(|| "shape.ttl".to_owned(), |f| f.name);
                let stem = name.trim_end_matches(".ttl").to_owned();
                let mut n = 2;
                while taken.contains(&name.to_lowercase()) {
                    name = format!("{stem}_{n}.ttl");
                    n += 1;
                }
                files.push(SetFile {
                    name,
                    text: String::new(),
                });
                graphs.push(part);
                changed.push(true);
                graphs.len() - 1
            }
        };
        // A refining shape takes what it changes from below: a property shape whole (it
        // replaces the one below with its path), a setting with its values below (it replaces
        // them, in every language).
        let refining = graphs[at].contains(&Triple::new(
            c.shape.clone(),
            tts("refines"),
            Literal::new_typed_literal("true", xsd::BOOLEAN),
        ));
        if refining {
            let from = below(&mut lower, state, entry, params)?;
            let g = &mut graphs[at];
            match &c.path {
                Some(_) if subject(g, c).is_none() => {
                    if let Some(p) = subject(from, c) {
                        copy_shape(from, g, p.as_ref());
                        g.insert(&Triple::new(c.shape.clone(), sh("property"), p));
                    }
                }
                None if g
                    .object_for_subject_predicate(&node, &c.predicate)
                    .is_none() =>
                {
                    let values: Vec<Triple> = from
                        .triples_for_subject(&node)
                        .filter(|t| t.predicate == c.predicate.as_ref())
                        .map(|t| t.into_owned())
                        .collect();
                    for t in &values {
                        g.insert(t);
                    }
                }
                _ => {}
            }
        }
        let g = &mut graphs[at];
        let subject = subject(g, c).ok_or_else(|| {
            HttpError(
                404,
                format!(
                    "Node shape {} has no property shape for {}",
                    c.shape,
                    c.path.as_ref().map_or("?", |p| p.as_str())
                ),
            )
        })?;
        // The values the change replaces: for a text, those of its language only.
        let old: Vec<Triple> = g
            .triples_for_subject(&subject)
            .filter(|t| t.predicate == c.predicate.as_ref())
            .filter(|t| {
                c.kind != Kind::Text
                    || matches!(t.object, TermRef::Literal(l) if l.language() == c.lang.as_deref())
            })
            .map(|t| t.into_owned())
            .collect();
        for t in &old {
            g.remove(t);
        }
        for o in objects(c)? {
            g.insert(&Triple::new(subject.clone(), c.predicate.clone(), o));
        }
        changed[at] = true;
    }
    for (i, f) in files.iter_mut().enumerate() {
        if changed[i] {
            f.text = shapes::to_turtle(&graphs[i]);
        }
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    shapes::load(&files).map_err(|e| invalid(&e))?;
    let names: Vec<&str> = files
        .iter()
        .filter(|f| {
            graphs
                .iter()
                .zip(&changed)
                .any(|(g, c)| *c && shapes::to_turtle(g) == f.text)
        })
        .map(|f| f.name.as_str())
        .collect();
    if dry {
        // With `all=true`, every file of the set as it would be: what a preview of the map
        // (`POST /{ds}/tiles/preview`) takes in place of the own set.
        let all = matches!(params.get("all"), Some("" | "true" | "1"));
        let texts: Map<String, Value> = files
            .iter()
            .filter(|f| all || names.contains(&f.name.as_str()))
            .map(|f| (f.name.clone(), json!(f.text)))
            .collect();
        return Ok(json!({ "changed": names, "files": texts }));
    }
    let version = write_version(folder, &files, None)?;
    Ok(json!({ "version": version, "changed": names }))
}
