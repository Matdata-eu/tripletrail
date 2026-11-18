//! Logical views (RML-LV): their records, made once per view before any triple.
//!
//! A view's record is a JSON object of its referenceable keys: `#` (the index of the
//! record of the source it was made from), and per field its absolute name (expression
//! fields only: an iterable field's records cannot be referenced) and `name.#`, the index of
//! the value among those the field made from its parent record. Sibling fields multiply, as
//! do the values of nested fields; a field that makes no value leaves its keys out (null).

use crate::RmlError;
use crate::jsonpath::Path;
use crate::mapping::{
    Field, FieldKind, Formulation, LogicalSource, LogicalView, Mapping, Part, SourceRef, TermMap,
    Value, absolute, absolute_names,
};
use oxigraph::model::Term;
use serde_json::{Map, Value as Json};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

type Row = Map<String, Json>;

/// The keys a view's records can be referenced by.
pub(crate) fn keys(mapping: &Mapping, view: usize) -> HashSet<String> {
    let v = &mapping.views[view];
    let mut names = Vec::new();
    collect_keys(&v.fields, "", &mut names);
    for j in &v.joins {
        collect_keys(&j.fields, "", &mut names);
    }
    names.push("#".into());
    names.into_iter().collect()
}

fn collect_keys(fields: &[Field], prefix: &str, out: &mut Vec<String>) {
    for f in fields {
        let name = absolute(prefix, &f.name);
        collect_keys(&f.fields, &name, out);
        out.push(format!("{name}.#"));
        if matches!(f.kind, FieldKind::Expression(_)) {
            out.push(name);
        }
    }
}

/// The references of an expression map (and its datatype and language maps).
pub(crate) fn references<'m>(map: &'m TermMap, out: &mut Vec<&'m str>) {
    match &map.value {
        Value::Reference(r) => out.push(r),
        Value::Template(parts) => out.extend(parts.iter().filter_map(|p| match p {
            Part::Reference(r) => Some(r.as_str()),
            Part::Text(_) => None,
        })),
        Value::Constant(_) | Value::Fresh | Value::Function(_) => {}
    }
    for m in map.children() {
        references(m, out);
    }
}

/// An error unless every reference of `maps` is a key of `view`.
pub(crate) fn check<'m>(
    mapping: &Mapping,
    view: usize,
    maps: impl IntoIterator<Item = &'m TermMap>,
) -> Result<(), RmlError> {
    let keys = keys(mapping, view);
    let mut refs = Vec::new();
    for m in maps {
        references(m, &mut refs);
    }
    match refs.into_iter().find(|r| !keys.contains(*r)) {
        None => Ok(()),
        Some(r) => {
            let v = &mapping.views[view];
            let mut names = Vec::new();
            absolute_names(&v.fields, "", &mut names);
            let why = if r == "<it>" || names.iter().any(|n| n == r) {
                "names records that are not values: only expression fields and index keys can \
                 be referenced"
            } else {
                "is not a field"
            };
            Err(RmlError::Mapping(format!(
                "the reference {r} of logical view {} {why}",
                v.node
            )))
        }
    }
}

/// Checks the references inside every view: of its fields on what it is a view on, and of
/// its joins.
pub(crate) fn check_views(mapping: &Mapping) -> Result<(), RmlError> {
    for (i, v) in mapping.views.iter().enumerate() {
        if let SourceRef::View(on) = v.on.source {
            check(mapping, on, top_expressions(&v.fields))?;
        }
        for j in &v.joins {
            check(mapping, i, j.conditions.iter().map(|(c, _)| c))?;
            check(mapping, j.parent, j.conditions.iter().map(|(_, p)| p))?;
            check(mapping, j.parent, top_expressions(&j.fields))?;
        }
    }
    Ok(())
}

/// The expressions of fields read from the parent record itself (not nested ones).
fn top_expressions(fields: &[Field]) -> impl Iterator<Item = &TermMap> {
    fields.iter().filter_map(|f| match &f.kind {
        FieldKind::Expression(m) => Some(m),
        FieldKind::Iterable { .. } => None,
    })
}

/// Makes the records of every view. `read` gives the parsed content of a logical source:
/// a JSON document, or the rows of a CSV file as an array of objects.
pub(crate) struct Builder<'m, 'r> {
    mapping: &'m Mapping,
    read: &'r dyn Fn(&'m LogicalSource) -> Result<Json, RmlError>,
    sources: HashMap<&'m LogicalSource, Json>,
    paths: RefCell<HashMap<String, Path>>,
    views: Vec<Option<Vec<Row>>>,
}

impl<'m, 'r> Builder<'m, 'r> {
    pub(crate) fn new(
        mapping: &'m Mapping,
        read: &'r dyn Fn(&'m LogicalSource) -> Result<Json, RmlError>,
    ) -> Self {
        Self {
            mapping,
            read,
            sources: HashMap::new(),
            paths: RefCell::default(),
            views: vec![None; mapping.views.len()],
        }
    }

    /// The records of every view, as JSON arrays of objects.
    pub(crate) fn build(mut self) -> Result<Vec<Json>, RmlError> {
        // Views come after the views they read: building them in order needs no recursion.
        let mapping = self.mapping;
        for (i, v) in mapping.views.iter().enumerate() {
            let rows = self.view(v)?;
            self.views[i] = Some(rows);
        }
        Ok(self
            .views
            .into_iter()
            .map(|rows| {
                Json::Array(
                    rows.unwrap_or_default()
                        .into_iter()
                        .map(Json::Object)
                        .collect(),
                )
            })
            .collect())
    }

    fn view(&mut self, v: &'m LogicalView) -> Result<Vec<Row>, RmlError> {
        // The records it is a view on, each with the formulation its expressions use.
        let (base, formulation): (Vec<Json>, Formulation) = match &v.on.source {
            SourceRef::View(i) => (
                self.views[*i]
                    .as_ref()
                    .expect("views are built in order")
                    .iter()
                    .cloned()
                    .map(Json::Object)
                    .collect(),
                Formulation::View,
            ),
            _ => {
                if !self.sources.contains_key(&v.on) {
                    let value = (self.read)(&v.on)?;
                    self.sources.insert(&v.on, value);
                }
                let root = &self.sources[&v.on];
                match v.on.formulation {
                    Formulation::Csv => (
                        root.as_array().cloned().unwrap_or_default(),
                        Formulation::Csv,
                    ),
                    Formulation::JsonPath => (
                        self.select(v.on.iterator.as_deref().unwrap_or("$"), root)?,
                        Formulation::JsonPath,
                    ),
                    ref f => {
                        return Err(RmlError::Unsupported(format!(
                            "logical views on {f:?} sources"
                        )));
                    }
                }
            }
        };
        let mut rows = Vec::new();
        for (i, record) in base.iter().enumerate() {
            let mut row = Row::new();
            row.insert("#".into(), i.into());
            rows.extend(self.expand(
                &v.fields,
                "",
                record,
                &formulation,
                &formulation,
                vec![row],
            )?);
        }
        for j in &v.joins {
            let parent = self.views[j.parent]
                .as_ref()
                .expect("views are built in order");
            let mut index: HashMap<Vec<String>, Vec<usize>> = HashMap::new();
            for (n, p) in parent.iter().enumerate() {
                let p = Json::Object(p.clone());
                for k in self.join_keys(&p, j.conditions.iter().map(|(_, p)| p))? {
                    index.entry(k).or_default().push(n);
                }
            }
            let mut joined = Vec::with_capacity(rows.len());
            for row in rows {
                let record = Json::Object(row);
                let mut matches: Vec<usize> = Vec::new();
                for k in self.join_keys(&record, j.conditions.iter().map(|(c, _)| c))? {
                    if let Some(found) = index.get(&k) {
                        matches.extend(found);
                    }
                }
                matches.sort_unstable();
                matches.dedup();
                let Json::Object(row) = record else {
                    unreachable!()
                };
                if matches.is_empty() {
                    if !j.inner {
                        joined.push(row);
                    }
                    continue;
                }
                // A join field's values (and their index) run over every matching record.
                let mut rows = vec![row];
                for f in &j.fields {
                    let FieldKind::Expression(map) = &f.kind else {
                        unreachable!("join fields are expression fields")
                    };
                    let mut values = Vec::new();
                    for &m in &matches {
                        let p = Json::Object(parent[m].clone());
                        values.extend(self.eval(map, &p, &Formulation::View)?);
                    }
                    if values.is_empty() {
                        continue;
                    }
                    rows = rows
                        .iter()
                        .flat_map(|row| {
                            values.iter().enumerate().map(|(i, v)| {
                                let mut row = row.clone();
                                row.insert(f.name.clone(), v.clone());
                                row.insert(format!("{}.#", f.name), i.into());
                                row
                            })
                        })
                        .collect();
                }
                joined.extend(rows);
            }
            rows = joined;
        }
        Ok(rows)
    }

    /// `rows` extended with `fields` made from `record`, read with `origin` (the formulation
    /// `record` came from) and, for iterable fields that declare none, `inherited`.
    fn expand(
        &self,
        fields: &[Field],
        prefix: &str,
        record: &Json,
        origin: &Formulation,
        inherited: &Formulation,
        mut rows: Vec<Row>,
    ) -> Result<Vec<Row>, RmlError> {
        for f in fields {
            let name = absolute(prefix, &f.name);
            let (records, formulation) = match &f.kind {
                FieldKind::Expression(map) => (self.eval(map, record, origin)?, inherited.clone()),
                FieldKind::Iterable {
                    formulation,
                    iterator,
                } => {
                    let formulation = formulation.clone().unwrap_or_else(|| inherited.clone());
                    let records =
                        self.iterate(record, origin, &formulation, iterator.as_deref())?;
                    (records, formulation)
                }
            };
            if records.is_empty() {
                continue;
            }
            let expression = matches!(f.kind, FieldKind::Expression(_));
            let child_origin = if expression { origin } else { &formulation };
            let mut next = Vec::with_capacity(rows.len() * records.len());
            for row in &rows {
                for (i, r) in records.iter().enumerate() {
                    let mut row = row.clone();
                    if expression {
                        row.insert(name.clone(), r.clone());
                    }
                    row.insert(format!("{name}.#"), i.into());
                    next.extend(self.expand(
                        &f.fields,
                        &name,
                        r,
                        child_origin,
                        &formulation,
                        vec![row],
                    )?);
                }
            }
            rows = next;
        }
        Ok(rows)
    }

    fn select(&self, path: &str, value: &Json) -> Result<Vec<Json>, RmlError> {
        let mut paths = self.paths.borrow_mut();
        if !paths.contains_key(path) {
            paths.insert(path.to_owned(), Path::parse(path)?);
        }
        let mut out = Vec::new();
        paths[path].select(value, &mut out);
        Ok(out.into_iter().cloned().collect())
    }

    /// The values of an expression on `record`.
    fn eval(
        &self,
        map: &TermMap,
        record: &Json,
        origin: &Formulation,
    ) -> Result<Vec<Json>, RmlError> {
        let reference = |r: &str| -> Result<Vec<Json>, RmlError> {
            let values = match origin {
                Formulation::JsonPath => self.select(r, record)?,
                _ => record.get(r).cloned().into_iter().collect(),
            };
            Ok(values.into_iter().filter(|v| !v.is_null()).collect())
        };
        match &map.value {
            Value::Reference(r) => reference(r),
            Value::Constant(Term::Literal(l)) => Ok(vec![l.value().into()]),
            Value::Constant(Term::NamedNode(n)) => Ok(vec![n.as_str().into()]),
            Value::Constant(_) | Value::Fresh => Ok(Vec::new()),
            Value::Function(_) => Err(RmlError::Unsupported(
                "functions in logical view fields and join conditions".into(),
            )),
            Value::Template(parts) => {
                let mut texts = vec![String::new()];
                for part in parts {
                    match part {
                        Part::Text(t) => texts.iter_mut().for_each(|s| s.push_str(t)),
                        Part::Reference(r) => {
                            let values: Vec<String> =
                                reference(r)?.iter().filter_map(text).collect();
                            texts = texts
                                .iter()
                                .flat_map(|s| values.iter().map(move |v| format!("{s}{v}")))
                                .collect();
                        }
                    }
                }
                Ok(texts.into_iter().map(Json::String).collect())
            }
        }
    }

    /// The records an iterable field selects from `record`.
    fn iterate(
        &self,
        record: &Json,
        origin: &Formulation,
        formulation: &Formulation,
        iterator: Option<&str>,
    ) -> Result<Vec<Json>, RmlError> {
        let data_error =
            |m: String| RmlError::Data(format!("an iterable field cannot read its records: {m}"));
        match formulation {
            Formulation::JsonPath => {
                let parsed;
                let value = match record {
                    Json::String(s) if *origin != Formulation::JsonPath => {
                        parsed = serde_json::from_str::<Json>(s)
                            .map_err(|e| data_error(e.to_string()))?;
                        &parsed
                    }
                    v => v,
                };
                self.select(iterator.unwrap_or("$"), value)
            }
            Formulation::Csv => {
                let Json::String(s) = record else {
                    return Err(data_error("CSV records need text".into()));
                };
                match crate::run::csv_rows(s.as_bytes(), false)
                    .map_err(|e| data_error(e.to_string()))?
                {
                    Json::Array(rows) => Ok(rows),
                    _ => unreachable!("CSV rows are an array"),
                }
            }
            f => Err(RmlError::Unsupported(format!(
                "iterable fields with the reference formulation {f:?}"
            ))),
        }
    }

    /// Join keys of `record`: one text per expression, every combination of their values.
    fn join_keys<'a>(
        &self,
        record: &Json,
        maps: impl Iterator<Item = &'a TermMap>,
    ) -> Result<Vec<Vec<String>>, RmlError> {
        let mut keys = vec![Vec::new()];
        for map in maps {
            let values: Vec<String> = self
                .eval(map, record, &Formulation::View)?
                .iter()
                .filter_map(text)
                .collect();
            keys = keys
                .iter()
                .flat_map(|k| {
                    values.iter().map(move |v| {
                        let mut k = k.clone();
                        k.push(v.clone());
                        k
                    })
                })
                .collect();
        }
        Ok(keys)
    }
}

/// The text of a value, as templates and joins use it.
fn text(v: &Json) -> Option<String> {
    match v {
        Json::Null => None,
        Json::String(s) => Some(s.clone()),
        v => Some(v.to_string()),
    }
}
