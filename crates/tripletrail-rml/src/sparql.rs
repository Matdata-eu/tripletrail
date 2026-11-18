//! A mapping as one Facade-X CONSTRUCT query (`tripletrail lift --as-sparql`): each triples map
//! becomes a `SERVICE <x-sparql-anything:>` over its source, its references triple patterns
//! on the record, its term maps `BIND` expressions, and its triples the query's template. The
//! query gives the triples the mapping gives, so it doubles as a translator from RML to
//! Facade-X.
//!
//! What can be written so: CSV and TSV files, and JSON files whose iterator and references are
//! paths of keys, `[*]` and `[n]` (`$.stations[*]`, `name`, `tracks[*].id`); constant,
//! reference, template and blank node term maps, with classes, datatypes and language tags
//! (constant or per record); and referencing object maps, with or without join conditions.
//! Anything else (XML, databases, spreadsheets, functions, collections, logical views,
//! logical targets, conditions, named graphs, compressed or UTF-16 files) is refused, naming
//! what cannot be written.
//!
//! The query differs from the mapping where SPARQL can only do less: a value that is no valid
//! IRI leaves the term out (the mapping fails), and a JSON number keeps its digits as written
//! in the file (the mapping writes doubles as Rust does, `1e3` as `1000.0`).

use crate::RmlError;
use crate::mapping::{
    Encoding, Formulation, LogicalSource, Mapping, ObjectMap, Part, SourceRef, TermMap, TermType,
    TriplesMap, Value,
};
use oxigraph::model::Term;
use std::fmt::Write as _;

const XYZ: &str = "http://sparql.xyz/facade-x/data/";

/// `mapping` as one Facade-X CONSTRUCT query.
pub fn as_sparql(mapping: &Mapping) -> Result<String, RmlError> {
    if !mapping.views.is_empty() {
        return Err(refused("logical views (RML-LV)"));
    }
    if !mapping.targets.is_empty() {
        return Err(refused("logical targets (RML-IO)"));
    }
    let mut bases = mapping
        .triples_maps
        .iter()
        .map(|m| m.base_iri.as_ref().or(mapping.base_iri.as_ref()));
    let base = bases.next().flatten();
    if bases.any(|b| b != base) {
        return Err(refused("triples maps with different base IRIs"));
    }
    let mut out = Query::default();
    for (i, map) in mapping.triples_maps.iter().enumerate() {
        let block = out.triples_map(mapping, i, map).map_err(|what| {
            RmlError::Mapping(format!("{} cannot be written as SPARQL: {what}", map.node))
        })?;
        out.blocks.push(block);
    }
    let mut text = String::new();
    if let Some(base) = base {
        let _ = writeln!(text, "BASE <{base}>");
    }
    text.push_str(
        "PREFIX fx:  <http://sparql.xyz/facade-x/ns/>\n\
         PREFIX fxe: <http://sparql.xyz/facade-x/engine/>\n\
         PREFIX tt:  <http://w3id.org/tripletrail/fn#>\n\
         PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>\n\
         CONSTRUCT {\n",
    );
    for t in &out.template {
        let _ = writeln!(text, "  {t} .");
    }
    text.push_str("}\nWHERE {\n");
    for (i, block) in out.blocks.iter().enumerate() {
        if i > 0 {
            text.push_str("  UNION\n");
        }
        text.push_str("  {\n");
        for line in block {
            let _ = writeln!(text, "    {line}");
        }
        text.push_str("  }\n");
    }
    text.push_str("}\n");
    Ok(text)
}

fn refused(what: &str) -> RmlError {
    RmlError::Mapping(format!("the mapping cannot be written as SPARQL: {what}"))
}

#[derive(Default)]
struct Query {
    template: Vec<String>,
    blocks: Vec<Vec<String>>,
    /// Variables made so far, to keep names unique.
    next: usize,
}

/// A term of the template: a constant, or a variable bound in the block.
enum Slot {
    Const(String),
    Var(String),
}

impl Slot {
    fn text(&self) -> &str {
        match self {
            Slot::Const(t) | Slot::Var(t) => t,
        }
    }
}

/// A source read in a block: its `SERVICE`, its record variable and the variables of the
/// references read on the record.
struct Scope<'a> {
    source: &'a LogicalSource,
    root: String,
    record: String,
    /// The patterns from the root to the records.
    records: Vec<String>,
    /// Reference → variable, with the patterns that bind it.
    refs: Vec<(String, String, Vec<String>)>,
    /// References that must have a value (join keys of a parent).
    required: Vec<String>,
}

impl Query {
    fn var(&mut self, hint: &str) -> String {
        self.next += 1;
        format!("?{hint}{}", self.next)
    }

    fn triples_map(
        &mut self,
        mapping: &Mapping,
        index: usize,
        map: &TriplesMap,
    ) -> Result<Vec<String>, String> {
        let mut lines = Vec::new();
        let mut scope = self.scope(&map.source)?;
        let mut binds = Vec::new();
        check_graphs(&map.graphs)?;
        if map.quoted_subject.is_some() || !map.asserted {
            return Err("quoted triples maps (RML-star)".into());
        }
        let subject = self.term(&map.subject, &mut scope, &mut binds, index)?;
        for class in &map.classes {
            self.template
                .push(format!("{} a <{}>", subject.text(), class.as_str()));
        }
        // Joins come after the record's own values: their keys are made from them.
        let mut joins = Vec::new();
        for pom in &map.predicate_objects {
            check_graphs(&pom.graphs)?;
            let mut predicates = Vec::new();
            for p in &pom.predicates {
                predicates.push(self.term(p, &mut scope, &mut binds, index)?);
            }
            let mut objects = Vec::new();
            for o in &pom.objects {
                match o {
                    ObjectMap::Term(t) => {
                        objects.push(self.term(t, &mut scope, &mut binds, index)?)
                    }
                    ObjectMap::Quoted(_) => return Err("quoted triples maps (RML-star)".into()),
                    ObjectMap::Join { parent, conditions } => {
                        let parent_map = &mapping.triples_maps[*parent];
                        if conditions.is_empty() {
                            if parent_map.source != map.source {
                                return Err(format!(
                                    "a join without conditions to {}, which reads another \
                                     source",
                                    parent_map.node
                                ));
                            }
                            objects.push(self.term(
                                &parent_map.subject,
                                &mut scope,
                                &mut binds,
                                *parent,
                            )?);
                        } else {
                            let (object, join) =
                                self.join(parent_map, *parent, conditions, &mut scope, &mut binds)?;
                            objects.push(object);
                            joins.push(join);
                        }
                    }
                }
            }
            for p in &predicates {
                for o in &objects {
                    self.template
                        .push(format!("{} {} {}", subject.text(), p.text(), o.text()));
                }
            }
        }
        lines.extend(service(&scope));
        lines.extend(binds);
        for join in joins {
            lines.extend(join);
        }
        Ok(lines)
    }

    /// An `OPTIONAL` reading the parent's source, joined on the conditions' values as text,
    /// that binds the parent's subject.
    fn join(
        &mut self,
        parent: &TriplesMap,
        parent_index: usize,
        conditions: &[(TermMap, TermMap)],
        child: &mut Scope<'_>,
        binds: &mut Vec<String>,
    ) -> Result<(Slot, Vec<String>), String> {
        let mut scope = self.scope(&parent.source)?;
        let mut inner = Vec::new();
        for (c, p) in conditions {
            let key = self.var("key");
            // A child without a value matches nothing: a fresh blank node equals no text.
            let c = self.text(c, child)?;
            binds.push(format!("BIND(COALESCE({c}, BNODE()) AS {key})"));
            let p = self.text(p, &mut scope)?;
            inner.push(format!("BIND({p} AS {key})"));
        }
        scope.required = scope.refs.iter().map(|(r, _, _)| r.clone()).collect();
        let subject = self.term(&parent.subject, &mut scope, &mut inner, parent_index)?;
        let mut lines = vec!["OPTIONAL {".to_owned()];
        lines.extend(service(&scope).into_iter().map(|l| format!("  {l}")));
        lines.extend(inner.into_iter().map(|l| format!("  {l}")));
        lines.push("}".to_owned());
        Ok((subject, lines))
    }

    fn scope<'a>(&mut self, source: &'a LogicalSource) -> Result<Scope<'a>, String> {
        check_source(source)?;
        let root = self.var("root");
        let record = self.var("record");
        let steps = match source.formulation {
            Formulation::Csv => vec![Step::Any],
            _ => json_path(source.iterator.as_deref().unwrap_or("$"), true)?,
        };
        let records = self.chain(&root, &steps, &record);
        Ok(Scope {
            source,
            root,
            record,
            records,
            refs: Vec::new(),
            required: Vec::new(),
        })
    }

    /// The variable holding the values of `reference` on the scope's record.
    fn reference(&mut self, scope: &mut Scope<'_>, reference: &str) -> Result<String, String> {
        if let Some((_, v, _)) = scope.refs.iter().find(|(r, _, _)| r == reference) {
            return Ok(v.clone());
        }
        let steps = match scope.source.formulation {
            Formulation::Csv => vec![Step::Key(reference.to_owned())],
            _ => json_path(reference, false)?,
        };
        let var = self.var("v");
        let patterns = self.chain(&scope.record, &steps, &var);
        scope
            .refs
            .push((reference.to_owned(), var.clone(), patterns));
        Ok(var)
    }

    /// Triple patterns from `from` along `steps` to `to`.
    fn chain(&mut self, from: &str, steps: &[Step], to: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut at = from.to_owned();
        for (i, step) in steps.iter().enumerate() {
            let next = if i + 1 == steps.len() {
                to.to_owned()
            } else {
                self.var("n")
            };
            out.push(format!("{at} {} {next} .", step.predicate()));
            at = next;
        }
        if steps.is_empty() {
            out.push(format!("BIND({from} AS {to})"));
        }
        out
    }

    /// An expression of the text `map` gives (for a join condition): `STR` of its value.
    fn text(&mut self, map: &TermMap, scope: &mut Scope<'_>) -> Result<String, String> {
        check_term_map(map)?;
        Ok(match &map.value {
            Value::Constant(t) => string(&term_text(t)),
            Value::Reference(r) => format!("STR({})", self.reference(scope, r)?),
            Value::Template(parts) => self.template_text(parts, map, scope)?,
            Value::Fresh | Value::Function(_) => {
                return Err("a join condition of a blank node or a function".into());
            }
        })
    }

    /// `CONCAT` of a template's parts, their values encoded as the term map says.
    fn template_text(
        &mut self,
        parts: &[Part],
        map: &TermMap,
        scope: &mut Scope<'_>,
    ) -> Result<String, String> {
        let mut args = Vec::new();
        for part in parts {
            match part {
                Part::Text(t) => args.push(string(t)),
                Part::Reference(r) => {
                    let v = format!("STR({})", self.reference(scope, r)?);
                    args.push(match (map.term_type, map.encoding) {
                        (TermType::Iri, Encoding::Iri) => format!("tt:encodeForIri({v})"),
                        (TermType::Iri, Encoding::Uri) => format!("ENCODE_FOR_URI({v})"),
                        _ => v,
                    });
                }
            }
        }
        Ok(match args.len() {
            0 => string(""),
            1 if matches!(parts[0], Part::Reference(_)) => args.remove(0),
            _ => format!("CONCAT({})", args.join(", ")),
        })
    }

    /// The term `map` makes, as a constant or a variable bound by a line added to `binds`.
    /// `index` is the triples map the term map belongs to (for blank nodes per record).
    fn term(
        &mut self,
        map: &TermMap,
        scope: &mut Scope<'_>,
        binds: &mut Vec<String>,
        index: usize,
    ) -> Result<Slot, String> {
        check_term_map(map)?;
        let expr = match &map.value {
            Value::Constant(t) => return Ok(Slot::Const(t.to_string())),
            Value::Function(_) => return Err("functions (RML-FNML)".into()),
            Value::Fresh => format!("fx:bnode(CONCAT(STR({}), \"#{index}\"))", scope.record),
            Value::Reference(r) => {
                let v = self.reference(scope, r)?;
                match map.term_type {
                    TermType::Iri => format!("IRI(STR({v}))"),
                    TermType::BlankNode => format!("fx:bnode(STR({v}))"),
                    TermType::Literal => {
                        let natural = match scope.source.formulation {
                            // A JSON number is an integer, or a double with a fraction or an
                            // exponent, as the mapping reads it; other values are as they are.
                            Formulation::JsonPath => Some(format!(
                                "IF(isNUMERIC({v}), STRDT(STR({v}), IF(CONTAINS(LCASE(STR({v})), \
                                 \"e\") || CONTAINS(STR({v}), \".\"), xsd:double, xsd:integer)), \
                                 {v})"
                            )),
                            _ => None,
                        };
                        self.literal(map, format!("STR({v})"), natural, scope)?
                    }
                }
            }
            Value::Template(parts) => {
                let text = self.template_text(parts, map, scope)?;
                match map.term_type {
                    TermType::Iri => format!("IRI({text})"),
                    TermType::BlankNode => format!("fx:bnode({text})"),
                    TermType::Literal => self.literal(map, text, None, scope)?,
                }
            }
        };
        let var = self.var("t");
        binds.push(format!("BIND({expr} AS {var})"));
        Ok(Slot::Var(var))
    }

    /// A literal of the text `lexical`, with the map's language or datatype, else `natural`
    /// (the value read, for its own datatype), else a plain string.
    fn literal(
        &mut self,
        map: &TermMap,
        lexical: String,
        natural: Option<String>,
        scope: &mut Scope<'_>,
    ) -> Result<String, String> {
        if let Some(lang) = &map.language {
            return Ok(format!("STRLANG({lexical}, {})", string(lang)));
        }
        if let Some(m) = &map.language_map {
            let tag = self.text(m, scope)?;
            return Ok(format!("STRLANG({lexical}, {tag})"));
        }
        if let Some(dt) = &map.datatype {
            return Ok(format!("STRDT({lexical}, <{}>)", dt.as_str()));
        }
        if let Some(m) = &map.datatype_map {
            let dt = self.text(m, scope)?;
            return Ok(format!("STRDT({lexical}, IRI({dt}))"));
        }
        Ok(natural.unwrap_or(lexical))
    }
}

/// The lines of a scope's `SERVICE`: the source's options, its records and the patterns of
/// its references (each `OPTIONAL` unless required).
fn service(scope: &Scope<'_>) -> Vec<String> {
    let source = scope.source;
    let SourceRef::Path(path) = &source.source else {
        unreachable!("checked by check_source")
    };
    let mut lines = vec!["SERVICE <x-sparql-anything:> {".to_owned()];
    let mut props = vec![
        format!("fxe:location {}", string(path)),
        "fxe:blank-nodes \"false\"".to_owned(),
    ];
    if let Formulation::Csv = source.formulation {
        {
            props.push("fxe:csv.headers \"true\"".into());
            let lower = path.to_ascii_lowercase();
            if lower.ends_with(".tsv") || lower.ends_with(".tab") {
                props.push("fxe:csv.delimiter \"\\t\"".into());
            }
            // Legacy RML reads an empty cell as no value; RML-IO only its `rml:null`.
            let null = match &source.io.nulls {
                Some(n) => n.first().cloned(),
                None if source.io.rml_io => None,
                None => Some(String::new()),
            };
            if let Some(null) = null {
                props.push(format!("fxe:csv.null-string {}", string(&null)));
            }
        }
    }
    lines.push(format!("  fxe:properties {} .", props.join(" ; ")));
    lines.push(format!("  {} a fx:Root .", scope.root));
    for l in &scope.records {
        lines.push(format!("  {l}"));
    }
    for (r, _, patterns) in &scope.refs {
        let body = patterns.join(" ");
        if scope.required.contains(r) {
            lines.push(format!("  {body}"));
        } else {
            lines.push(format!("  OPTIONAL {{ {body} }}"));
        }
    }
    lines.push("}".to_owned());
    lines
}

fn check_source(source: &LogicalSource) -> Result<(), String> {
    let path = match &source.source {
        SourceRef::Path(p) => p,
        SourceRef::Table(_) | SourceRef::Query(_) => return Err("database sources".into()),
        SourceRef::View(_) => return Err("logical views (RML-LV)".into()),
        SourceRef::Empty => return Err("a triples map without a logical source".into()),
    };
    if crate::run::is_spreadsheet(&path.to_ascii_lowercase()) {
        return Err("spreadsheet sources".into());
    }
    if source.io.compression.is_some() {
        return Err("compressed sources".into());
    }
    if source
        .io
        .encoding
        .as_deref()
        .is_some_and(|e| !e.eq_ignore_ascii_case("UTF-8"))
    {
        return Err("sources not in UTF-8".into());
    }
    if source.io.nulls.as_ref().is_some_and(|n| n.len() > 1) {
        return Err("more than one rml:null value".into());
    }
    match &source.formulation {
        Formulation::Csv => Ok(()),
        Formulation::JsonPath => {
            json_path(source.iterator.as_deref().unwrap_or("$"), true).map(|_| ())
        }
        Formulation::XPath => Err("XML sources".into()),
        f => Err(format!("{f:?} sources")),
    }
}

fn check_graphs(graphs: &[TermMap]) -> Result<(), String> {
    for g in graphs {
        match &g.value {
            Value::Constant(Term::NamedNode(n))
                if n.as_str().rsplit(['#', '/']).next() == Some("defaultGraph") => {}
            _ => return Err("graph maps (CONSTRUCT makes triples of the default graph)".into()),
        }
    }
    Ok(())
}

fn check_term_map(map: &TermMap) -> Result<(), String> {
    if map.condition.is_some() {
        return Err("conditions".into());
    }
    if map.gather.is_some() {
        return Err("collections and containers (RML-CC)".into());
    }
    if !map.targets.is_empty() {
        return Err("logical targets (RML-IO)".into());
    }
    Ok(())
}

/// A step of a JSONPath: a key, every member, or an array element (from 0).
#[derive(Debug, PartialEq)]
enum Step {
    Key(String),
    Any,
    Index(usize),
}

impl Step {
    fn predicate(&self) -> String {
        match self {
            Step::Key(k) => {
                let mut iri = String::from(XYZ);
                escape_iri(k, &mut iri);
                format!("<{iri}>")
            }
            Step::Any => "fx:anySlot".into(),
            Step::Index(i) => format!("<http://www.w3.org/1999/02/22-rdf-syntax-ns#_{}>", i + 1),
        }
    }
}

/// The steps of a JSONPath of keys, `[*]`, `.*` and `[n]`. An iterator starts with `$`; a
/// reference may (`$.name`) or may not (`name`).
fn json_path(path: &str, iterator: bool) -> Result<Vec<Step>, String> {
    let unsupported = || format!("the JSONPath {path:?} (only keys, [*] and [n])");
    let p = path.trim();
    let mut rest = match p.strip_prefix('$') {
        Some(r) => r,
        None if iterator => return Err(unsupported()),
        None => {
            // A bare reference: its first key has no dot.
            return json_steps(&format!(".{p}")).ok_or_else(unsupported);
        }
    };
    if rest.starts_with("..") {
        return Err(unsupported());
    }
    rest = rest.trim();
    json_steps(rest).ok_or_else(unsupported)
}

fn json_steps(mut s: &str) -> Option<Vec<Step>> {
    let mut steps = Vec::new();
    while !s.is_empty() {
        if let Some(r) = s.strip_prefix(".*") {
            steps.push(Step::Any);
            s = r;
        } else if let Some(r) = s.strip_prefix('.') {
            if r.starts_with('.') {
                return None;
            }
            let end = r.find(['.', '[']).unwrap_or(r.len());
            if end == 0 {
                return None;
            }
            steps.push(Step::Key(r[..end].to_owned()));
            s = &r[end..];
        } else if let Some(r) = s.strip_prefix("[*]") {
            steps.push(Step::Any);
            s = r;
        } else {
            let r = s.strip_prefix('[')?;
            let end = r.find(']')?;
            let inner = r[..end].trim();
            s = &r[end + 1..];
            if let Ok(n) = inner.parse::<usize>() {
                steps.push(Step::Index(n));
            } else {
                let k = inner
                    .strip_prefix('\'')
                    .and_then(|k| k.strip_suffix('\''))
                    .or_else(|| inner.strip_prefix('"').and_then(|k| k.strip_suffix('"')))?;
                steps.push(Step::Key(k.to_owned()));
            }
        }
    }
    Some(steps)
}

/// As Facade-X writes keys in IRIs: space, `<>"{}|\^`, the backtick and control characters
/// percent-encoded.
fn escape_iri(s: &str, out: &mut String) {
    for c in s.chars() {
        if c <= ' '
            || matches!(
                c,
                '<' | '>' | '"' | '{' | '}' | '|' | '\\' | '^' | '`' | '\u{7f}'
            )
        {
            let mut buf = [0; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                let _ = write!(out, "%{b:02X}");
            }
        } else {
            out.push(c);
        }
    }
}

/// The text of a constant term, as a join compares it.
fn term_text(t: &Term) -> String {
    match t {
        Term::NamedNode(n) => n.as_str().to_owned(),
        Term::Literal(l) => l.value().to_owned(),
        other => other.to_string(),
    }
}

/// A SPARQL string literal.
fn string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_paths() {
        assert_eq!(
            json_path("$.stations[*]", true),
            Ok(vec![Step::Key("stations".into()), Step::Any])
        );
        assert_eq!(json_path("$", true), Ok(vec![]));
        assert_eq!(
            json_path("tracks[0].id", false),
            Ok(vec![
                Step::Key("tracks".into()),
                Step::Index(0),
                Step::Key("id".into())
            ])
        );
        assert_eq!(
            json_path("$['a b']", false),
            Ok(vec![Step::Key("a b".into())])
        );
        assert!(json_path("$..x", true).is_err());
        assert!(json_path("$.a[?(@.b)]", true).is_err());
        assert!(json_path("name", true).is_err());
    }

    #[test]
    fn a_csv_mapping_becomes_one_query() {
        let mapping = Mapping::parse_turtle(
            r#"@prefix rr: <http://www.w3.org/ns/r2rml#> .
            @prefix rml: <http://semweb.mmlab.be/ns/rml#> .
            @prefix ql: <http://semweb.mmlab.be/ns/ql#> .
            <#S> rml:logicalSource [ rml:source "stations.csv" ; rml:referenceFormulation ql:CSV ] ;
              rr:subjectMap [ rr:template "http://ex.org/station/{code}" ; rr:class <http://ex.org/Station> ] ;
              rr:predicateObjectMap [ rr:predicate <http://ex.org/name> ; rr:objectMap [ rml:reference "name" ; rr:language "nl" ] ] ."#,
            Some("http://ex.org/"),
        )
        .unwrap();
        let q = as_sparql(&mapping).unwrap();
        assert!(q.contains("fxe:location \"stations.csv\""), "{q}");
        assert!(q.contains("tt:encodeForIri(STR("), "{q}");
        assert!(q.contains("a <http://ex.org/Station>"), "{q}");
        assert!(q.contains("STRLANG(STR("), "{q}");
        assert!(q.contains("fxe:csv.null-string \"\""), "{q}");
    }
}
