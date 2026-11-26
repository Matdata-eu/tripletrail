//! A database's own semantics as RDF (#278): the comments, keys and constraints of its tables
//! as an OWL ontology, SHACL shapes and, for code tables, SKOS concept schemes, named as the
//! direct mapping ([`crate::direct`]) names the tables, columns and rows with the same base, so
//! that the direct-mapped data has the ontology's classes and conforms to the shapes.
//!
//! For a table or view `T` with columns `c` and base `B`:
//!
//! - OWL: `<B T>` an `owl:Class`, `<B T#c>` an `owl:DatatypeProperty` (domain `<B T>`, range
//!   the column's datatype), `<B T#ref-c1;c2>` an `owl:ObjectProperty` per foreign key (range
//!   the referenced table's class); `rdfs:label` the name, `rdfs:comment` the `COMMENT`.
//! - SHACL: a node shape `<B schema/shapes/T>` targeting `<B T>`, with a property shape
//!   `<B schema/shapes/T#c>` per column: `sh:datatype` as the direct mapping types its values,
//!   `sh:maxCount 1`, `sh:minCount 1` for `NOT NULL`, `sh:maxLength` for `varchar(n)`, `sh:in`
//!   for an enum type, and for a `CHECK` on the column (or its domain) what it says:
//!   comparisons with constants (`sh:minInclusive`, …), `IN` lists (`sh:in`), regular
//!   expressions (`sh:pattern`) and lengths (`sh:minLength`, `sh:maxLength`). A foreign key
//!   gets `sh:class` and `sh:nodeKind sh:IRI`, a one-column unique or primary key
//!   `dash:uniqueValueForClass`. What is not translated is said in an `rdfs:comment` of the
//!   shape and in [`Artefacts::notes`].
//! - SKOS: a code table named on request becomes a concept scheme `<B schema/skos/T>`, each row
//!   a `skos:Concept` (the row's direct-mapping IRI) with its key as `skos:notation` and its
//!   label column as `skos:prefLabel`.
//!
//! Every class, property and shape has `dct:source` the table or column (`schema.table`,
//! `schema.table.column`).

use crate::direct::{DirectTable, encode};
use std::fmt::Write as _;

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
const WKT: &str = "http://www.opengis.net/ont/geosparql#wktLiteral";

/// A table or view with what the catalogue says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaTable {
    /// Its name, columns and keys, as the direct mapping reads them.
    pub table: DirectTable,
    pub view: bool,
    pub comment: Option<String>,
    /// The columns, in the order of `table.columns`.
    pub columns: Vec<SchemaColumn>,
    /// Unique constraints other than the primary key, each its columns.
    pub uniques: Vec<Vec<String>>,
    /// `CHECK` constraints.
    pub checks: Vec<Check>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SchemaColumn {
    pub name: String,
    /// The type as SQL writes it (`character varying(20)`, a domain's name).
    pub sql_type: String,
    /// The name of the type its values have (a domain's base type): `int4`, `varchar`, …
    pub base: String,
    /// `n` of `varchar(n)` and `char(n)`.
    pub max_length: Option<u32>,
    pub not_null: bool,
    pub comment: Option<String>,
    /// The labels of its enum type, in their order.
    pub enum_values: Option<Vec<String>>,
    /// The `CHECK`s of its domain, on `VALUE`.
    pub domain_checks: Vec<String>,
}

/// A `CHECK` constraint as `pg_get_constraintdef` writes it, with the columns it reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub definition: String,
    pub columns: Vec<String>,
}

/// A code table's rows, for a SKOS concept scheme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeList {
    pub table: String,
    /// The column of the labels.
    pub label: String,
    /// Each row's primary key values and label.
    pub rows: Vec<(Vec<String>, String)>,
}

/// The ontology, shapes and concept schemes, each as Turtle, and what was not translated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artefacts {
    pub ontology: String,
    pub shapes: String,
    pub skos: String,
    pub notes: Vec<String>,
}

const PREFIXES: &str = "@prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
@prefix dash: <http://datashapes.org/dash#> .\n\
@prefix skos: <http://www.w3.org/2004/02/skos/core#> .\n\
@prefix dct: <http://purl.org/dc/terms/> .\n\n";

/// The datatype the direct mapping gives a value of the type named `base`.
pub fn datatype(base: &str) -> String {
    let local = match base {
        "int2" | "int4" | "int8" | "oid" => "integer",
        "numeric" => "decimal",
        "float4" | "float8" => "double",
        "bool" => "boolean",
        "date" => "date",
        "time" => "time",
        "timestamp" | "timestamptz" => "dateTime",
        "bytea" => "hexBinary",
        "geometry" | "geography" => return WKT.to_owned(),
        _ => "string",
    };
    format!("{XSD}{local}")
}

/// The ontology, shapes and concept schemes of `tables` of the schema `schema`, with IRIs
/// under `base`.
pub fn artefacts(
    schema: &str,
    tables: &[SchemaTable],
    codes: &[CodeList],
    base: &str,
) -> Artefacts {
    let mut ontology = String::from(PREFIXES);
    let mut shapes = String::from(PREFIXES);
    let mut skos = String::from(PREFIXES);
    let mut notes = Vec::new();
    let _ = writeln!(
        ontology,
        "<{base}schema/owl> a owl:Ontology ;\n  rdfs:label {} ;\n  dct:source {} .\n",
        string(&format!("The tables of schema {schema}")),
        string(schema)
    );
    let class_of = |name: &str| format!("{base}{}", encode(name));
    for t in tables {
        let class = class_of(&t.table.name);
        let source = format!("{schema}.{}", t.table.name);
        let shape = format!("{base}schema/shapes/{}", encode(&t.table.name));
        // The class.
        let _ = write!(
            ontology,
            "<{class}> a owl:Class ;\n  rdfs:label {} ;\n  dct:source {}",
            string(&t.table.name),
            string(&source)
        );
        if let Some(c) = &t.comment {
            let _ = write!(ontology, " ;\n  rdfs:comment {}", string(c));
        }
        ontology.push_str(" .\n\n");
        // Its node shape.
        let mut node = vec![
            format!("<{shape}> a sh:NodeShape"),
            format!("  sh:targetClass <{class}>"),
            format!("  rdfs:label {}", string(&t.table.name)),
            format!("  dct:source {}", string(&source)),
        ];
        if let Some(c) = &t.comment {
            node.push(format!("  rdfs:comment {}", string(c)));
        }
        let mut properties = Vec::new();
        let unique_alone = |c: &str| {
            (t.table.primary_key.len() == 1 && t.table.primary_key[0] == c)
                || t.uniques.iter().any(|u| u.len() == 1 && u[0] == c)
        };
        for column in &t.columns {
            let property = format!("{class}#{}", encode(&column.name));
            let source = format!("{schema}.{}.{}", t.table.name, column.name);
            let dt = datatype(&column.base);
            let _ = write!(
                ontology,
                "<{property}> a owl:DatatypeProperty ;\n  rdfs:label {} ;\n  rdfs:domain <{class}> ;\n  rdfs:range <{dt}> ;\n  dct:source {}",
                string(&column.name),
                string(&source)
            );
            if let Some(c) = &column.comment {
                let _ = write!(ontology, " ;\n  rdfs:comment {}", string(c));
            }
            ontology.push_str(" .\n\n");
            let ps = format!("{shape}#{}", encode(&column.name));
            properties.push(ps.clone());
            let mut parts = vec![
                format!("<{ps}> a sh:PropertyShape"),
                format!("  sh:path <{property}>"),
                format!("  sh:name {}", string(&column.name)),
                format!("  sh:datatype <{dt}>"),
                "  sh:maxCount 1".to_owned(),
                format!("  dct:source {}", string(&source)),
            ];
            if let Some(c) = &column.comment {
                parts.push(format!("  sh:description {}", string(c)));
            }
            let mut min_count = column.not_null;
            if let Some(n) = column.max_length {
                parts.push(format!("  sh:maxLength {n}"));
            }
            if let Some(values) = &column.enum_values {
                parts.push(format!(
                    "  sh:in ( {} )",
                    values
                        .iter()
                        .map(|v| literal(v, &dt))
                        .collect::<Vec<_>>()
                        .join(" ")
                ));
            }
            if unique_alone(&column.name) {
                parts.push(format!("  dash:uniqueValueForClass <{class}>"));
            }
            // The column's CHECKs and its domain's.
            let mut checks: Vec<(String, &str)> = t
                .checks
                .iter()
                .filter(|c| c.columns.len() == 1 && c.columns[0] == column.name)
                .map(|c| (c.definition.clone(), column.name.as_str()))
                .collect();
            checks.extend(column.domain_checks.iter().map(|d| (d.clone(), "VALUE")));
            for (definition, name) in &checks {
                let (constraints, left) = translate(definition, name);
                for c in constraints {
                    match c {
                        Constraint::MinCount => min_count = true,
                        Constraint::Facet(facet, value) => {
                            parts.push(format!("  sh:{facet} {}", literal(&value, &dt)));
                        }
                        Constraint::Length(facet, n) => parts.push(format!("  sh:{facet} {n}")),
                        Constraint::In(values) => parts.push(format!(
                            "  sh:in ( {} )",
                            values
                                .iter()
                                .map(|v| literal(v, &dt))
                                .collect::<Vec<_>>()
                                .join(" ")
                        )),
                        Constraint::Pattern(re) => {
                            parts.push(format!("  sh:pattern {}", string(&re)))
                        }
                    }
                }
                for l in left {
                    let note = format!("{source}: {l} is not translated to SHACL");
                    parts.push(format!("  rdfs:comment {}", string(&note)));
                    notes.push(note);
                }
            }
            if min_count {
                parts.push("  sh:minCount 1".to_owned());
            }
            let _ = writeln!(shapes, "{} .\n", parts.join(" ;\n"));
        }
        // Foreign keys.
        for fk in &t.table.foreign_keys {
            if !tables.iter().any(|p| p.table.name == fk.table) {
                continue;
            }
            let cols: Vec<String> = fk.columns.iter().map(|c| encode(c)).collect();
            let property = format!("{class}#ref-{}", cols.join(";"));
            let source = format!("{schema}.{}.{}", t.table.name, fk.columns.join(","));
            let target = class_of(&fk.table);
            let _ = writeln!(
                ontology,
                "<{property}> a owl:ObjectProperty ;\n  rdfs:label {} ;\n  rdfs:domain <{class}> ;\n  rdfs:range <{target}> ;\n  dct:source {} .\n",
                string(&format!("{} to {}", fk.columns.join(", "), fk.table)),
                string(&source)
            );
            let ps = format!("{shape}#ref-{}", cols.join(";"));
            properties.push(ps.clone());
            let mut parts = vec![
                format!("<{ps}> a sh:PropertyShape"),
                format!("  sh:path <{property}>"),
                format!("  sh:name {}", string(&fk.table)),
                format!("  sh:class <{target}>"),
                "  sh:nodeKind sh:IRI".to_owned(),
                "  sh:maxCount 1".to_owned(),
                format!("  dct:source {}", string(&source)),
            ];
            let required = fk
                .columns
                .iter()
                .all(|c| t.columns.iter().any(|col| &col.name == c && col.not_null));
            if required {
                parts.push("  sh:minCount 1".to_owned());
            }
            let _ = writeln!(shapes, "{} .\n", parts.join(" ;\n"));
        }
        // Constraints of several columns: said, not translated.
        for u in t.uniques.iter().filter(|u| u.len() > 1) {
            let note = format!(
                "{source}: UNIQUE ({}) is not translated to SHACL",
                u.join(", ")
            );
            node.push(format!("  rdfs:comment {}", string(&note)));
            notes.push(note);
        }
        for c in t.checks.iter().filter(|c| c.columns.len() != 1) {
            let note = format!("{source}: {} is not translated to SHACL", c.definition);
            node.push(format!("  rdfs:comment {}", string(&note)));
            notes.push(note);
        }
        if !properties.is_empty() {
            node.push(format!(
                "  sh:property {}",
                properties
                    .iter()
                    .map(|p| format!("<{p}>"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let _ = writeln!(shapes, "{} .\n", node.join(" ;\n"));
    }
    // Code tables.
    for code in codes {
        let Some(t) = tables.iter().find(|t| t.table.name == code.table) else {
            continue;
        };
        let scheme = format!("{base}schema/skos/{}", encode(&code.table));
        let _ = writeln!(
            skos,
            "<{scheme}> a skos:ConceptScheme ;\n  rdfs:label {} ;\n  dct:source {} .\n",
            string(&code.table),
            string(&format!("{schema}.{}", code.table))
        );
        for (key, label) in &code.rows {
            let parts: Vec<String> = t
                .table
                .primary_key
                .iter()
                .zip(key)
                .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
                .collect();
            let concept = format!("{base}{}/{}", encode(&code.table), parts.join(";"));
            let _ = writeln!(
                skos,
                "<{concept}> a skos:Concept ;\n  skos:inScheme <{scheme}> ;\n  skos:notation {} ;\n  skos:prefLabel {} .\n",
                string(&key.join(";")),
                string(label)
            );
        }
    }
    Artefacts {
        ontology,
        shapes,
        skos,
        notes,
    }
}

/// A Turtle string literal.
fn string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `value` as a literal of the datatype `dt`.
fn literal(value: &str, dt: &str) -> String {
    if dt == format!("{XSD}string") {
        string(value)
    } else {
        format!("{}^^<{dt}>", string(value))
    }
}

/// What a `CHECK` on one column says, in SHACL terms.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Constraint {
    MinCount,
    /// `minInclusive`, `minExclusive`, `maxInclusive` or `maxExclusive`, and the value.
    Facet(&'static str, String),
    /// `minLength` or `maxLength`.
    Length(&'static str, u64),
    In(Vec<String>),
    Pattern(String),
}

/// The constraints a `CHECK (…)` on `column` (or `VALUE`, for a domain) says, and the parts
/// it does not translate. The conjuncts of an `AND` are translated one by one; anything else
/// (`OR`, functions, other columns) is left.
fn translate(definition: &str, column: &str) -> (Vec<Constraint>, Vec<String>) {
    let body = definition.trim();
    let body = body.strip_suffix(" NOT VALID").unwrap_or(body);
    let Some(body) = body.strip_prefix("CHECK") else {
        return (Vec::new(), vec![definition.to_owned()]);
    };
    let mut out = Vec::new();
    let mut left = Vec::new();
    for part in split_top(unwrap(body), " AND ") {
        match conjunct(unwrap(part), column) {
            Some(c) => out.push(c),
            None => left.push(format!("CHECK ({})", unwrap(part))),
        }
    }
    (out, left)
}

fn conjunct(s: &str, column: &str) -> Option<Constraint> {
    if let Some(operand) = s.strip_suffix(" IS NOT NULL") {
        return is_column(operand, column).then_some(Constraint::MinCount);
    }
    for op in [" >= ", " <= ", " <> ", " > ", " < ", " = ", " ~ "] {
        let Some(at) = find_top(s, op) else { continue };
        let (l, r) = (s[..at].trim(), s[at + op.len()..].trim());
        let op = op.trim();
        if op == "="
            && let Some(list) = r.strip_prefix("ANY ")
        {
            if !is_column(l, column) {
                return None;
            }
            let list = strip_cast(unwrap(list));
            let items = unwrap(list).strip_prefix("ARRAY[")?.strip_suffix(']')?;
            let values: Option<Vec<String>> =
                split_top(items, ", ").into_iter().map(constant).collect();
            return values.map(Constraint::In);
        }
        if op == "~" {
            return (is_column(l, column))
                .then(|| constant(r))
                .flatten()
                .map(Constraint::Pattern);
        }
        // length(column) op n
        if let Some(n) = constant(r).and_then(|v| v.parse::<u64>().ok())
            && is_length(l, column)
        {
            return match op {
                ">=" => Some(Constraint::Length("minLength", n)),
                ">" => Some(Constraint::Length("minLength", n + 1)),
                "<=" => Some(Constraint::Length("maxLength", n)),
                "<" if n > 0 => Some(Constraint::Length("maxLength", n - 1)),
                _ => None,
            };
        }
        let (op, value) = if is_column(l, column) {
            (op, constant(r)?)
        } else if is_column(r, column) {
            let flipped = match op {
                ">=" => "<=",
                "<=" => ">=",
                ">" => "<",
                "<" => ">",
                other => other,
            };
            (flipped, constant(l)?)
        } else {
            return None;
        };
        return match op {
            ">=" => Some(Constraint::Facet("minInclusive", value)),
            ">" => Some(Constraint::Facet("minExclusive", value)),
            "<=" => Some(Constraint::Facet("maxInclusive", value)),
            "<" => Some(Constraint::Facet("maxExclusive", value)),
            "=" => Some(Constraint::In(vec![value])),
            _ => None,
        };
    }
    None
}

/// `s` without the parentheses around all of it.
fn unwrap(s: &str) -> &str {
    let mut s = s.trim();
    while s.starts_with('(') && s.ends_with(')') && closing(s, 0) == Some(s.len() - 1) {
        s = s[1..s.len() - 1].trim();
    }
    s
}

/// The index of the `)` that closes the `(` at `open`, outside quotes.
fn closing(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut quote = None;
    for (i, c) in s.char_indices().skip_while(|(i, _)| *i < open) {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '(' | '[') => depth += 1,
            (None, ')' | ']') => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Where `needle` first occurs in `s` outside parentheses, brackets and quotes.
fn find_top(s: &str, needle: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut quote = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '(' | '[') => depth += 1,
            (None, ')' | ']') => depth -= 1,
            (None, _) if depth == 0 && s[i..].starts_with(needle) => return Some(i),
            _ => {}
        }
    }
    None
}

fn split_top<'a>(s: &'a str, separator: &str) -> Vec<&'a str> {
    let mut parts = Vec::new();
    let mut rest = s;
    while let Some(at) = find_top(rest, separator) {
        parts.push(rest[..at].trim());
        rest = &rest[at + separator.len()..];
    }
    parts.push(rest.trim());
    parts
}

/// `s` without a cast at its end (`'a'::text`, `(code)::character varying`, `x::text[]`).
fn strip_cast(s: &str) -> &str {
    let s = s.trim();
    let mut cut = None;
    let mut depth = 0i32;
    let mut quote = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '(' | '[') => depth += 1,
            (None, ')' | ']') => depth -= 1,
            (None, ':') if depth == 0 && s[i..].starts_with("::") => {
                cut = Some(i);
                break;
            }
            _ => {}
        }
    }
    match cut {
        Some(i) => s[..i].trim(),
        None => s,
    }
}

/// Whether `s` is the column `column` (or `VALUE`), maybe cast or quoted.
fn is_column(s: &str, column: &str) -> bool {
    let s = unwrap(strip_cast(unwrap(s)));
    if s == column
        && (column == "VALUE"
            || s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
    {
        return true;
    }
    s.strip_prefix('"')
        .and_then(|q| q.strip_suffix('"'))
        .is_some_and(|q| q.replace("\"\"", "\"") == column)
}

/// Whether `s` is `length(column)` or `char_length(column)`.
fn is_length(s: &str, column: &str) -> bool {
    let s = unwrap(s);
    ["length(", "char_length(", "character_length("]
        .iter()
        .find_map(|f| s.strip_prefix(f))
        .and_then(|r| r.strip_suffix(')'))
        .is_some_and(|inner| is_column(inner, column))
}

/// The text of a constant: a quoted string (maybe cast), or a number.
fn constant(s: &str) -> Option<String> {
    let s = unwrap(strip_cast(unwrap(s)));
    if let Some(q) = s.strip_prefix('\'').and_then(|q| q.strip_suffix('\'')) {
        return Some(q.replace("''", "'"));
    }
    let digits = s.strip_prefix('-').unwrap_or(s);
    (!digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
        && digits.chars().filter(|c| *c == '.').count() <= 1)
        .then(|| s.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_on_one_column_become_shacl() {
        let t = |d: &str, c: &str| translate(d, c);
        assert_eq!(
            t("CHECK (((speed >= 0) AND (speed <= 350)))", "speed"),
            (
                vec![
                    Constraint::Facet("minInclusive", "0".into()),
                    Constraint::Facet("maxInclusive", "350".into())
                ],
                vec![]
            )
        );
        assert_eq!(
            t(
                "CHECK ((status = ANY (ARRAY['open'::text, 'it''s closed'::text])))",
                "status"
            ),
            (
                vec![Constraint::In(vec!["open".into(), "it's closed".into()])],
                vec![]
            )
        );
        assert_eq!(
            t(
                "CHECK (((kind)::text = ANY ((ARRAY['a'::character varying, 'b'::character varying])::text[])))",
                "kind"
            ),
            (vec![Constraint::In(vec!["a".into(), "b".into()])], vec![])
        );
        assert_eq!(
            t("CHECK (((code)::text ~ '^[A-Z]{2}[0-9]+$'::text))", "code"),
            (vec![Constraint::Pattern("^[A-Z]{2}[0-9]+$".into())], vec![])
        );
        assert_eq!(
            t("CHECK ((length((name)::text) > 0))", "name"),
            (vec![Constraint::Length("minLength", 1)], vec![])
        );
        assert_eq!(
            t("CHECK ((VALUE > '-1.5'::numeric))", "VALUE"),
            (
                vec![Constraint::Facet("minExclusive", "-1.5".into())],
                vec![]
            )
        );
        assert_eq!(
            t("CHECK ((0 < speed))", "speed"),
            (vec![Constraint::Facet("minExclusive", "0".into())], vec![])
        );
        assert_eq!(
            t("CHECK (((speed > 0) OR (speed IS NULL)))", "speed").1,
            vec!["CHECK ((speed > 0) OR (speed IS NULL))".to_owned()]
        );
        assert_eq!(
            t("CHECK ((opened >= '1830-01-01'::date))", "opened").0,
            vec![Constraint::Facet("minInclusive", "1830-01-01".into())]
        );
        assert_eq!(
            t("CHECK ((\"Max Speed\" > 0))", "Max Speed").0,
            vec![Constraint::Facet("minExclusive", "0".into())]
        );
    }

    #[test]
    fn types_have_the_direct_mappings_datatypes() {
        assert_eq!(datatype("int4"), format!("{XSD}integer"));
        assert_eq!(datatype("numeric"), format!("{XSD}decimal"));
        assert_eq!(datatype("timestamptz"), format!("{XSD}dateTime"));
        assert_eq!(datatype("varchar"), format!("{XSD}string"));
        assert_eq!(datatype("geometry"), WKT);
    }
}
