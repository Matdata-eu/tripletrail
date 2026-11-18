//! YARRRML: the YAML syntax for RML mappings. A document is translated into the RML rules it
//! stands for (in the R2RML and legacy RML vocabularies, as the YARRRML parser writes them),
//! which are then read like any mapping document.
//!
//! Supported: `prefixes` (with the RDFa initial context predefined), `base`, `sources` (at the
//! root, by name, or in a mapping; long and `[access~formulation, iterator]` forms),
//! `mappings` with subjects (templates, or blank nodes when absent), predicate-object pairs in
//! their long and shortcut forms (`~iri`, `~lang`, datatypes, languages, `a`), graphs, and
//! links to other mappings with `equal` conditions (joins). Functions, conditions other than
//! joins and inverse predicates are reported as not supported; database sources
//! (`type: postgresql`, with `credentials` and a `query` or `table`) become `d2rq:Database`s;
//! targets are
//! ignored (`tripletrail lift` decides where triples go).

use crate::RmlError;
use oxigraph::model::vocab::rdf;
use oxigraph::model::{BlankNode, Graph, Literal, NamedNode, NamedOrBlankNode, Term, Triple};
use std::collections::HashMap;
use yaml_rust2::{Yaml, YamlLoader};

const RR: &str = "http://www.w3.org/ns/r2rml#";
const RML: &str = "http://semweb.mmlab.be/ns/rml#";
const D2RQ: &str = "http://www.wiwiss.fu-berlin.de/suhl/bizer/D2RQ/0.1#";
const QL: &str = "http://semweb.mmlab.be/ns/ql#";

/// The RDFa initial context, which every YARRRML document starts with, plus the RML ones.
const PREFIXES: &[(&str, &str)] = &[
    ("as", "https://www.w3.org/ns/activitystreams#"),
    ("cc", "http://creativecommons.org/ns#"),
    ("cnt", "http://www.w3.org/2008/content#"),
    ("ctag", "http://commontag.org/ns#"),
    ("dc", "http://purl.org/dc/terms/"),
    ("dc11", "http://purl.org/dc/elements/1.1/"),
    ("dcat", "http://www.w3.org/ns/dcat#"),
    ("dcterms", "http://purl.org/dc/terms/"),
    ("dqv", "http://www.w3.org/ns/dqv#"),
    ("earl", "http://www.w3.org/ns/earl#"),
    ("foaf", "http://xmlns.com/foaf/0.1/"),
    ("gr", "http://purl.org/goodrelations/v1#"),
    ("grddl", "http://www.w3.org/2003/g/data-view#"),
    ("ical", "http://www.w3.org/2002/12/cal/icaltzd#"),
    ("ldp", "http://www.w3.org/ns/ldp#"),
    ("ma", "http://www.w3.org/ns/ma-ont#"),
    ("oa", "http://www.w3.org/ns/oa#"),
    ("og", "http://ogp.me/ns#"),
    ("org", "http://www.w3.org/ns/org#"),
    ("owl", "http://www.w3.org/2002/07/owl#"),
    ("prov", "http://www.w3.org/ns/prov#"),
    ("qb", "http://purl.org/linked-data/cube#"),
    ("ql", QL),
    ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    ("rdfa", "http://www.w3.org/ns/rdfa#"),
    ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
    ("rev", "http://purl.org/stuff/rev#"),
    ("rif", "http://www.w3.org/2007/rif#"),
    ("rml", RML),
    ("rr", RR),
    ("schema", "http://schema.org/"),
    ("sd", "http://www.w3.org/ns/sparql-service-description#"),
    ("sioc", "http://rdfs.org/sioc/ns#"),
    ("skos", "http://www.w3.org/2004/02/skos/core#"),
    ("skosxl", "http://www.w3.org/2008/05/skos-xl#"),
    ("sosa", "http://www.w3.org/ns/sosa/"),
    ("ssn", "http://www.w3.org/ns/ssn/"),
    ("time", "http://www.w3.org/2006/time#"),
    ("v", "http://rdf.data-vocabulary.org/#"),
    ("vcard", "http://www.w3.org/2006/vcard/ns#"),
    ("void", "http://rdfs.org/ns/void#"),
    ("wdr", "http://www.w3.org/2007/05/powder#"),
    ("wdrs", "http://www.w3.org/2007/05/powder-s#"),
    ("xhv", "http://www.w3.org/1999/xhtml/vocab#"),
    ("xml", "http://www.w3.org/XML/1998/namespace"),
    ("xsd", "http://www.w3.org/2001/XMLSchema#"),
];

fn error(message: impl Into<String>) -> RmlError {
    RmlError::Mapping(format!("YARRRML: {}", message.into()))
}

fn unsupported(what: &str) -> RmlError {
    RmlError::Unsupported(format!("{what} in YARRRML"))
}

/// The RML rules a YARRRML document stands for, and its base IRI (`base`, else `base_iri`).
pub(crate) fn to_rml(
    text: &str,
    base_iri: Option<&str>,
) -> Result<(Graph, Option<String>), RmlError> {
    let docs = YamlLoader::load_from_str(text).map_err(|e| error(e.to_string()))?;
    let doc = match docs.as_slice() {
        [doc] => doc,
        [] => return Err(error("the document is empty")),
        _ => return Err(error("the file holds several YAML documents")),
    };
    let Yaml::Hash(root) = doc else {
        return Err(error("the document is not a collection of keys"));
    };
    let mut prefixes: HashMap<String, String> = PREFIXES
        .iter()
        .map(|(p, n)| ((*p).to_owned(), (*n).to_owned()))
        .collect();
    let mut base = base_iri.map(str::to_owned);
    let mut sources = HashMap::new();
    let mut mappings = None;
    for (key, value) in root {
        match text_of(key)?.as_str() {
            "prefixes" => {
                for (p, n) in hash(value, "prefixes")? {
                    prefixes.insert(text_of(p)?, text_of(n)?);
                }
            }
            "base" => base = Some(text_of(value)?),
            "sources" => {
                for (name, source) in hash(value, "sources")? {
                    sources.insert(text_of(name)?, source.clone());
                }
            }
            "mappings" | "mapping" | "m" => mappings = Some(hash(value, "mappings")?),
            "authors" | "targets" | "external" | "externals" => {}
            k => return Err(error(format!("unknown key {k}"))),
        }
    }
    let mappings = mappings.ok_or_else(|| error("the document has no mappings"))?;
    let mut g = Generator {
        prefixes,
        base: base.clone(),
        sources,
        graph: Graph::new(),
        maps: HashMap::new(),
        joins: Vec::new(),
    };
    for (name, body) in mappings {
        g.mapping(&text_of(name)?, body)?;
    }
    g.links()?;
    Ok((g.graph, base))
}

fn text_of(y: &Yaml) -> Result<String, RmlError> {
    match y {
        Yaml::String(s) | Yaml::Real(s) => Ok(s.clone()),
        Yaml::Integer(i) => Ok(i.to_string()),
        Yaml::Boolean(b) => Ok(b.to_string()),
        y => Err(error(format!("expected text, found {y:?}"))),
    }
}

fn hash<'y>(y: &'y Yaml, what: &str) -> Result<&'y yaml_rust2::yaml::Hash, RmlError> {
    match y {
        Yaml::Hash(h) => Ok(h),
        _ => Err(error(format!("{what} must be a collection of keys"))),
    }
}

/// The `value` of a collection that also has other keys (`targets`), else `y`.
fn value_of(y: &Yaml) -> &Yaml {
    match y {
        Yaml::Hash(h) => get(h, &["value", "v"]).unwrap_or(y),
        y => y,
    }
}

/// A value or a list of values, as a list.
fn many(y: &Yaml) -> Vec<&Yaml> {
    match y {
        Yaml::Array(a) => a.iter().collect(),
        Yaml::Null | Yaml::BadValue => Vec::new(),
        y => vec![y],
    }
}

/// The first of several keys (aliases) present in `h`.
fn get<'y>(h: &'y yaml_rust2::yaml::Hash, keys: &[&str]) -> Option<&'y Yaml> {
    keys.iter()
        .find_map(|k| h.get(&Yaml::String((*k).to_owned())))
}

#[derive(Default, Clone)]
struct Source {
    access: String,
    formulation: Option<String>,
    iterator: Option<String>,
    /// A database source (`type: postgresql`): `access` is its address.
    database: Option<DatabaseSource>,
}

#[derive(Debug, Clone, Default)]
struct DatabaseSource {
    driver: String,
    username: Option<String>,
    password: Option<String>,
    query: Option<String>,
    table: Option<String>,
}

/// A term to make: a YARRRML template (`$(ref)` references), how to make it, and for objects
/// its datatype or language.
#[derive(Default, Clone)]
struct Value {
    value: Option<String>,
    kind: Option<&'static str>,
    datatype: Option<String>,
    language: Option<String>,
}

/// A link to another mapping: its subjects as objects, with join conditions.
struct Link {
    pom: BlankNode,
    mapping: String,
    /// (child, parent) templates.
    conditions: Vec<(String, String)>,
    /// The triples map making the link, for error messages.
    from: String,
}

struct Generator {
    prefixes: HashMap<String, String>,
    base: Option<String>,
    sources: HashMap<String, Yaml>,
    graph: Graph,
    /// The triples maps made for each mapping.
    maps: HashMap<String, Vec<NamedOrBlankNode>>,
    joins: Vec<Link>,
}

impl Generator {
    fn add(&mut self, s: impl Into<NamedOrBlankNode>, p: &str, o: impl Into<Term>) {
        self.graph.insert(&Triple::new(
            s.into(),
            NamedNode::new_unchecked(p),
            o.into(),
        ));
    }

    /// `prefix:local` with a known prefix as an IRI; anything else as it is.
    fn expand(&self, s: &str) -> String {
        if s.contains("://") {
            return s.to_owned();
        }
        match s.split_once(':') {
            Some((p, rest)) if !rest.starts_with(' ') => match self.prefixes.get(p) {
                Some(ns) => format!("{ns}{rest}"),
                None => s.to_owned(),
            },
            _ => s.to_owned(),
        }
    }

    /// `value` with a known prefix at its start expanded (not one with an escaped colon).
    fn expand_leading(&self, value: &str) -> String {
        if value.contains("://") {
            return value.to_owned();
        }
        match value.split_once(':') {
            Some((p, rest)) if !rest.starts_with(' ') && !p.contains(['\\', '$', '(']) => {
                match self.prefixes.get(p) {
                    // Escaped, so the namespace is text in a template.
                    Some(ns) => format!("{}{rest}", escape(ns)),
                    None => value.to_owned(),
                }
            }
            _ => value.to_owned(),
        }
    }

    fn iri(&self, s: &str) -> Result<NamedNode, RmlError> {
        let expanded = self.expand(s);
        if expanded == "a" {
            return Ok(rdf::TYPE.into_owned());
        }
        NamedNode::new(&expanded).map_err(|e| error(format!("{s} is not an IRI: {e}")))
    }

    fn source(&self, y: &Yaml) -> Result<Source, RmlError> {
        match y {
            Yaml::String(s) if self.sources.contains_key(s) => self.source(&self.sources[s]),
            Yaml::String(s) => Ok(access(s)),
            Yaml::Array(a) => {
                let mut source = access(&text_of(
                    a.first().ok_or_else(|| error("an empty source"))?,
                )?);
                source.iterator = a.get(1).map(text_of).transpose()?;
                Ok(source)
            }
            Yaml::Hash(h) => {
                let access = get(h, &["access"]).ok_or_else(|| error("a source has no access"))?;
                let text = |k: &str| get(h, &[k]).map(text_of).transpose();
                let database = match text("type")? {
                    None => None,
                    Some(t) => {
                        let driver = match t.to_ascii_lowercase().as_str() {
                            "postgresql" | "postgres" => "org.postgresql.Driver",
                            "mysql" => "com.mysql.cj.jdbc.Driver",
                            "sqlserver" => "com.microsoft.sqlserver.jdbc.SQLServerDriver",
                            "oracledb" | "oracle" => "oracle.jdbc.driver.OracleDriver",
                            _ => return Err(unsupported(&format!("{t} sources"))),
                        };
                        let credentials = get(h, &["credentials"]);
                        let credential = |k: &str| match credentials {
                            Some(Yaml::Hash(c)) => get(c, &[k]).map(text_of).transpose(),
                            _ => Ok(None),
                        };
                        Some(DatabaseSource {
                            driver: driver.to_owned(),
                            username: credential("username")?,
                            password: credential("password")?,
                            query: text("query")?.map(|q| q.trim().to_owned()),
                            table: text("table")?,
                        })
                    }
                };
                Ok(Source {
                    access: text_of(access)?,
                    formulation: text("referenceFormulation")?,
                    iterator: text("iterator")?,
                    database,
                })
            }
            y => Err(error(format!("not a source: {y:?}"))),
        }
    }

    fn logical_source(&mut self, s: &Source) -> Result<BlankNode, RmlError> {
        let ls = BlankNode::default();
        if let Some(d) = &s.database {
            let db = BlankNode::default();
            self.add(
                db.clone(),
                &format!("{D2RQ}jdbcDSN"),
                Literal::from(s.access.as_str()),
            );
            self.add(
                db.clone(),
                &format!("{D2RQ}jdbcDriver"),
                Literal::from(d.driver.as_str()),
            );
            for (k, v) in [("username", &d.username), ("password", &d.password)] {
                if let Some(v) = v {
                    self.add(db.clone(), &format!("{D2RQ}{k}"), Literal::from(v.as_str()));
                }
            }
            self.add(ls.clone(), &format!("{RML}source"), db);
            match (&d.query, &d.table) {
                (Some(q), _) => self.add(
                    ls.clone(),
                    &format!("{RML}query"),
                    Literal::from(q.as_str()),
                ),
                (None, Some(t)) => self.add(
                    ls.clone(),
                    &format!("{RR}tableName"),
                    Literal::from(t.as_str()),
                ),
                (None, None) => {
                    return Err(error(format!(
                        "the database source {} needs a query or a table",
                        s.access
                    )));
                }
            }
            return Ok(ls);
        }
        self.add(
            ls.clone(),
            &format!("{RML}source"),
            Literal::from(s.access.as_str()),
        );
        if let Some(f) = &s.formulation {
            let f = match f.to_ascii_lowercase().as_str() {
                "csv" => "CSV",
                "jsonpath" => "JSONPath",
                "xpath" => "XPath",
                _ => return Err(error(format!("unknown reference formulation {f}"))),
            };
            self.add(
                ls.clone(),
                &format!("{RML}referenceFormulation"),
                NamedNode::new_unchecked(format!("{QL}{f}")),
            );
        }
        if let Some(i) = &s.iterator {
            self.add(
                ls.clone(),
                &format!("{RML}iterator"),
                Literal::from(i.as_str()),
            );
        }
        Ok(ls)
    }

    fn mapping(&mut self, name: &str, body: &Yaml) -> Result<(), RmlError> {
        let body = hash(body, &format!("mapping {name}"))?;
        let mut sources = Vec::new();
        let mut subjects = Vec::new();
        let mut pos = Vec::new();
        let mut graphs = Vec::new();
        for (key, value) in body {
            match text_of(key)?.as_str() {
                "sources" | "source" => {
                    // A list of sources, or one source in its short array form.
                    let list = match value {
                        Yaml::Array(a) if a.iter().all(|v| matches!(v, Yaml::String(_)))
                            && a.len() <= 2
                            && a.first().is_some_and(|v| {
                                matches!(v, Yaml::String(s) if s.contains('~') && !self.sources.contains_key(s))
                            }) =>
                        {
                            vec![value]
                        }
                        v => many(v),
                    };
                    for s in list {
                        sources.push(self.source(s)?);
                    }
                }
                "subjects" | "subject" | "s" => {
                    for s in many(value) {
                        subjects.push(self.value(s)?);
                    }
                }
                "predicateobjects" | "predicateobject" | "po" => pos.extend(many(value)),
                "graphs" | "graph" | "g" => graphs.extend(many(value)),
                "targets" => {}
                "condition" | "conditions" => {
                    return Err(unsupported("conditions on mappings (RML-FNML)"));
                }
                k => return Err(error(format!("unknown key {k} in mapping {name}"))),
            }
        }
        if sources.is_empty() {
            return Err(error(format!("mapping {name} has no sources")));
        }
        if subjects.is_empty() {
            // No subject: a blank node per record.
            subjects.push(Value {
                kind: Some("blank"),
                ..Value::default()
            });
        }
        let mut nodes = Vec::new();
        let count = sources.len() * subjects.len();
        for source in &sources {
            for subject in &subjects {
                let node: NamedOrBlankNode = match &self.base {
                    Some(b) if count == 1 => NamedNode::new(format!("{b}#{name}"))
                        .map_or_else(|_| BlankNode::default().into(), NamedOrBlankNode::from),
                    Some(b) => NamedNode::new(format!("{b}#{name}_{}", nodes.len()))
                        .map_or_else(|_| BlankNode::default().into(), NamedOrBlankNode::from),
                    None => BlankNode::default().into(),
                };
                self.add(
                    node.clone(),
                    rdf::TYPE.as_str(),
                    NamedNode::new_unchecked(format!("{RR}TriplesMap")),
                );
                let ls = self.logical_source(source)?;
                self.add(node.clone(), &format!("{RML}logicalSource"), ls);
                let sm = self.term_map(subject, "IRI")?;
                for g in &graphs {
                    let g = self.value(g)?;
                    let gm = self.term_map(&g, "IRI")?;
                    self.add(sm.clone(), &format!("{RR}graphMap"), gm);
                }
                self.add(node.clone(), &format!("{RR}subjectMap"), sm);
                for po in &pos {
                    self.predicate_object(&node, po, name)?;
                }
                nodes.push(node);
            }
        }
        self.maps.insert(name.to_owned(), nodes);
        Ok(())
    }

    /// A subject, object or graph: a template string with shortcuts (`~iri`, `~lang`), or a
    /// collection with `value`, `type`, `datatype`, `language`.
    fn value(&self, y: &Yaml) -> Result<Value, RmlError> {
        match y {
            Yaml::Hash(h) => {
                if get(h, &["function", "fn", "f", "condition"]).is_some() {
                    return Err(unsupported("functions (RML-FNML)"));
                }
                let mut v = match get(h, &["value", "v"]) {
                    Some(v) => self.value(v)?,
                    None => Value::default(),
                };
                if let Some(t) = get(h, &["type"]) {
                    v.kind = Some(match text_of(t)?.as_str() {
                        "iri" => "iri",
                        "literal" => "literal",
                        "blank" | "blanknode" => "blank",
                        t => return Err(error(format!("unknown term type {t}"))),
                    });
                }
                if let Some(d) = get(h, &["datatype"]) {
                    v.datatype = Some(text_of(d)?);
                }
                if let Some(l) = get(h, &["language"]) {
                    v.language = Some(text_of(value_of(l))?);
                }
                Ok(v)
            }
            y => {
                let text = text_of(y)?;
                let (value, kind) = match text.rsplit_once('~') {
                    Some((v, "iri")) => (v.to_owned(), Some("iri")),
                    Some((v, "literal")) => (v.to_owned(), Some("literal")),
                    Some((v, "blanknode")) => (v.to_owned(), Some("blank")),
                    _ => (text, None),
                };
                Ok(Value {
                    value: Some(value),
                    kind,
                    ..Value::default()
                })
            }
        }
    }

    /// The term map making `v`; `default` is the term type when `v` names none.
    fn term_map(&mut self, v: &Value, default: &str) -> Result<BlankNode, RmlError> {
        let map = BlankNode::default();
        let kind = match v.kind {
            Some("iri") => "IRI",
            Some("blank") => "BlankNode",
            Some(_) => "Literal",
            None if v.datatype.is_some() || v.language.is_some() => "Literal",
            None => default,
        };
        if let Some(value) = &v.value {
            let value = &self.expand_leading(value);
            let parts = template(value)?;
            match parts.as_slice() {
                [Part::Reference(r)] => self.add(
                    map.clone(),
                    &format!("{RML}reference"),
                    Literal::from(r.as_str()),
                ),
                _ if parts.iter().all(|p| matches!(p, Part::Text(_))) => {
                    let constant: String = parts
                        .iter()
                        .map(|p| match p {
                            Part::Text(t) | Part::Reference(t) => t.as_str(),
                        })
                        .collect();
                    let term: Term = if kind == "IRI" {
                        NamedNode::new(&constant)
                            .map_err(|e| error(format!("{constant} is not an IRI: {e}")))?
                            .into()
                    } else {
                        Literal::from(constant).into()
                    };
                    self.add(map.clone(), &format!("{RR}constant"), term);
                }
                _ => {
                    let mut rml = String::new();
                    for p in &parts {
                        match p {
                            Part::Text(t) => {
                                for c in t.chars() {
                                    if matches!(c, '{' | '}' | '\\') {
                                        rml.push('\\');
                                    }
                                    rml.push(c);
                                }
                            }
                            Part::Reference(r) => {
                                rml.push('{');
                                for c in r.chars() {
                                    if matches!(c, '{' | '}' | '\\') {
                                        rml.push('\\');
                                    }
                                    rml.push(c);
                                }
                                rml.push('}');
                            }
                        }
                    }
                    self.add(map.clone(), &format!("{RR}template"), Literal::from(rml));
                }
            }
        }
        self.add(
            map.clone(),
            &format!("{RR}termType"),
            NamedNode::new_unchecked(format!("{RR}{kind}")),
        );
        let datatype = v.datatype.as_ref().filter(|d| {
            // rdf:langString with a language is the language.
            !(v.language.is_some() && self.expand(d) == rdf::LANG_STRING.as_str())
        });
        if let Some(d) = datatype {
            let d = self.iri(d)?;
            self.add(map.clone(), &format!("{RR}datatype"), d);
        } else if let Some(l) = &v.language {
            let parts = template(l)?;
            if parts.iter().all(|p| matches!(p, Part::Text(_))) {
                self.add(
                    map.clone(),
                    &format!("{RR}language"),
                    Literal::from(l.as_str()),
                );
            } else {
                let lm = self.term_map(
                    &Value {
                        value: Some(l.clone()),
                        kind: Some("literal"),
                        ..Value::default()
                    },
                    "Literal",
                )?;
                self.add(map.clone(), &format!("{RML}languageMap"), lm);
            }
        }
        Ok(map)
    }

    fn predicate_object(
        &mut self,
        node: &NamedOrBlankNode,
        po: &Yaml,
        mapping: &str,
    ) -> Result<(), RmlError> {
        let (predicates, objects, graphs): (Vec<&Yaml>, Vec<Value>, Vec<&Yaml>);
        let mut links = Vec::new();
        match po {
            Yaml::Array(a) => {
                let (p, o) = match a.as_slice() {
                    [p, o] | [p, o, _] => (p, o),
                    _ => {
                        return Err(error(format!(
                            "a predicate-object pair in {mapping} needs a predicate and an object"
                        )));
                    }
                };
                predicates = many(p);
                let mut values = Vec::new();
                for o in many(o) {
                    let mut v = self.value(o)?;
                    if let Some(extra) = a.get(2) {
                        extra_of(&mut v, &text_of(extra)?);
                    }
                    values.push(v);
                }
                objects = values;
                graphs = Vec::new();
            }
            Yaml::Hash(h) => {
                if get(h, &["inversepredicates", "inversepredicate", "i"]).is_some() {
                    return Err(unsupported("inverse predicates"));
                }
                if get(h, &["condition", "conditions"]).is_some() {
                    return Err(unsupported("conditions (RML-FNML)"));
                }
                predicates = get(h, &["predicates", "predicate", "p"])
                    .map(many)
                    .unwrap_or_default();
                let mut values = Vec::new();
                for o in get(h, &["objects", "object", "o"])
                    .map(many)
                    .unwrap_or_default()
                {
                    match o {
                        Yaml::Hash(oh) if get(oh, &["mapping"]).is_some() => {
                            links.push(self.link(oh, mapping)?);
                        }
                        // [value, datatype or language].
                        Yaml::Array(pair) => {
                            let mut v =
                                self.value(pair.first().ok_or_else(|| error("an empty object"))?)?;
                            if let Some(extra) = pair.get(1) {
                                extra_of(&mut v, &text_of(extra)?);
                            }
                            values.push(v);
                        }
                        o => values.push(self.value(o)?),
                    }
                }
                // Keys of the predicate-object pair that apply to its objects.
                for v in &mut values {
                    if let Some(t) = get(h, &["type"]) {
                        let t = text_of(t)?;
                        v.kind = v.kind.or(match t.as_str() {
                            "iri" => Some("iri"),
                            "blank" | "blanknode" => Some("blank"),
                            _ => Some("literal"),
                        });
                    }
                    if let Some(d) = get(h, &["datatype"]) {
                        v.datatype = v.datatype.take().or(Some(text_of(d)?));
                    }
                    if let Some(l) = get(h, &["language"]) {
                        v.language = v.language.take().or(Some(text_of(l)?));
                    }
                }
                objects = values;
                graphs = get(h, &["graphs", "graph", "g"])
                    .map(many)
                    .unwrap_or_default();
            }
            y => return Err(error(format!("not a predicate-object pair: {y:?}"))),
        }
        if predicates.is_empty() || (objects.is_empty() && links.is_empty()) {
            return Err(error(format!(
                "a predicate-object pair in {mapping} needs a predicate and an object"
            )));
        }
        let pom = BlankNode::default();
        self.add(
            node.clone(),
            &format!("{RR}predicateObjectMap"),
            pom.clone(),
        );
        let mut typing = false;
        for p in predicates {
            let p = text_of(value_of(p))?;
            let v = self.value(&Yaml::String(p.clone()))?;
            let iri = self.iri(&p);
            typing |= iri.as_ref().is_ok_and(|i| i.as_ref() == rdf::TYPE);
            let pm = match iri {
                // A constant predicate: rr:predicate.
                Ok(i) if !p.contains("$(") => {
                    self.add(pom.clone(), &format!("{RR}predicate"), i);
                    continue;
                }
                _ => self.term_map(&v, "IRI")?,
            };
            self.add(pom.clone(), &format!("{RR}predicateMap"), pm);
        }
        for o in &objects {
            // The objects of rdf:type are classes: IRIs.
            let default = if typing { "IRI" } else { "Literal" };
            let om = self.term_map(o, default)?;
            self.add(pom.clone(), &format!("{RR}objectMap"), om);
        }
        for g in graphs {
            let g = self.value(g)?;
            let gm = self.term_map(&g, "IRI")?;
            self.add(pom.clone(), &format!("{RR}graphMap"), gm);
        }
        for mut link in links {
            link.pom = pom.clone();
            self.joins.push(link);
        }
        Ok(())
    }

    /// `{mapping: name, condition: {function: equal, parameters: [[str1, $(a), s], ...]}}`.
    fn link(&self, h: &yaml_rust2::yaml::Hash, from: &str) -> Result<Link, RmlError> {
        let mapping = text_of(get(h, &["mapping"]).expect("checked"))?;
        let mut conditions = Vec::new();
        for c in get(h, &["condition", "conditions"])
            .map(many)
            .unwrap_or_default()
        {
            let c = hash(c, "a condition")?;
            let function = get(c, &["function", "fn", "f"])
                .map(text_of)
                .transpose()?
                .unwrap_or_default();
            if !matches!(
                function.as_str(),
                "equal"
                    | "idlab-fn:equal"
                    | "grel:equal"
                    | "https://w3id.org/imec/idlab/function#equal"
            ) {
                return Err(unsupported(&format!(
                    "the function {function} in a link condition (only equal is)"
                )));
            }
            let (mut child, mut parent) = (None, None);
            for (i, p) in get(c, &["parameters", "pms"])
                .map(many)
                .unwrap_or_default()
                .into_iter()
                .enumerate()
            {
                let (value, side) = match p {
                    Yaml::Array(a) => {
                        let value = text_of(
                            a.get(1)
                                .ok_or_else(|| error("a parameter without a value"))?,
                        )?;
                        let side = a.get(2).map(text_of).transpose()?;
                        (value, side)
                    }
                    Yaml::Hash(ph) => (
                        text_of(
                            get(ph, &["value", "v"])
                                .ok_or_else(|| error("a parameter without a value"))?,
                        )?,
                        get(ph, &["from"]).map(text_of).transpose()?,
                    ),
                    p => return Err(error(format!("not a parameter: {p:?}"))),
                };
                // str1 is the subject's (child) side, str2 the object's (parent), unless said.
                let side = side.unwrap_or_else(|| if i == 0 { "s".into() } else { "o".into() });
                match side.as_str() {
                    "s" => child = Some(value),
                    "o" => parent = Some(value),
                    s => return Err(error(format!("unknown parameter side {s}"))),
                }
            }
            match (child, parent) {
                (Some(c), Some(p)) => conditions.push((c, p)),
                _ => {
                    return Err(error(format!(
                        "a link condition in {from} needs a value from each side"
                    )));
                }
            }
        }
        Ok(Link {
            pom: BlankNode::default(),
            mapping,
            conditions,
            from: from.to_owned(),
        })
    }

    /// Referencing object maps, once every mapping has its triples maps.
    fn links(&mut self) -> Result<(), RmlError> {
        let joins = std::mem::take(&mut self.joins);
        for link in joins {
            let parents = self.maps.get(&link.mapping).cloned().ok_or_else(|| {
                error(format!(
                    "{} links to the unknown mapping {}",
                    link.from, link.mapping
                ))
            })?;
            for parent in parents {
                let om = BlankNode::default();
                self.add(link.pom.clone(), &format!("{RR}objectMap"), om.clone());
                self.add(om.clone(), &format!("{RR}parentTriplesMap"), parent);
                for (child, parent) in &link.conditions {
                    let jc = BlankNode::default();
                    self.add(om.clone(), &format!("{RR}joinCondition"), jc.clone());
                    for (side, value) in [("child", child), ("parent", parent)] {
                        let parts = template(value)?;
                        match parts.as_slice() {
                            [Part::Reference(r)] => {
                                self.add(
                                    jc.clone(),
                                    &format!("{RR}{side}"),
                                    Literal::from(r.as_str()),
                                );
                            }
                            _ => {
                                let map = self.term_map(
                                    &Value {
                                        value: Some(value.clone()),
                                        kind: Some("literal"),
                                        ..Value::default()
                                    },
                                    "Literal",
                                )?;
                                self.add(jc.clone(), &format!("{RML}{side}Map"), map);
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// `s` with the characters templates give a meaning (`$`, `\\`) escaped.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '$' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `xsd:integer` (a datatype) or `en~lang` (a language) given after an object.
fn extra_of(v: &mut Value, extra: &str) {
    match extra.strip_suffix("~lang") {
        Some(l) => v.language = Some(l.to_owned()),
        None => v.datatype = Some(extra.to_owned()),
    }
}

/// `data.csv~csv`: the access and the reference formulation.
fn access(s: &str) -> Source {
    match s.rsplit_once('~') {
        Some((a, f)) => Source {
            access: a.to_owned(),
            formulation: Some(f.to_owned()),
            ..Source::default()
        },
        None => Source {
            access: s.to_owned(),
            ..Source::default()
        },
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Part {
    Text(String),
    Reference(String),
}

/// Splits a YARRRML template into text and `$(reference)` parts; `\` escapes the next
/// character.
fn template(t: &str) -> Result<Vec<Part>, RmlError> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(e) = chars.next() {
                    text.push(e);
                }
            }
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                if !text.is_empty() {
                    parts.push(Part::Text(std::mem::take(&mut text)));
                }
                // Up to the first `)` not escaped: `$((a\\))` is the reference `(a)`.
                let mut reference = String::new();
                loop {
                    match chars.next() {
                        None => return Err(error(format!("unclosed $( in {t}"))),
                        Some('\\') => {
                            if let Some(e) = chars.next() {
                                reference.push(e);
                            }
                        }
                        Some(')') => break,
                        Some(c) => reference.push(c),
                    }
                }
                parts.push(Part::Reference(reference));
            }
            c => text.push(c),
        }
    }
    if !text.is_empty() {
        parts.push(Part::Text(text));
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_split_into_text_and_references() {
        assert_eq!(
            template(r"ex:$(id)/a\$(b)/$((x\))").unwrap(),
            vec![
                Part::Text("ex:".into()),
                Part::Reference("id".into()),
                Part::Text("/a$(b)/".into()),
                Part::Reference("(x)".into()),
            ]
        );
        assert!(template("$(a").is_err());
    }
}
