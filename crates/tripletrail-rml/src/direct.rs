//! The W3C direct mapping of a relational database to RDF ("A Direct Mapping of Relational
//! Data to RDF", 2012): a graph from a database without a mapping document.
//!
//! The direct mapping is written as an R2RML mapping ([`r2rml`]) and run like any other, so it
//! reads tables as R2RML does and can be saved and edited. For each table `T`, with base IRI
//! `B` and names percent-encoded:
//!
//! - each row is `<B T/k1=v1;k2=v2>` from its primary key, or a blank node without one (one
//!   per row, duplicates included), typed `<B T>`;
//! - each column `c` with a value gives `<B T#c>` and the value's natural literal (an
//!   `xsd:integer`, `xsd:date`, `geo:wktLiteral`, …); NULLs give nothing;
//! - each foreign key on columns `c1, c2` gives `<B T#ref-c1;c2>` to the row it references,
//!   unless one of its columns is NULL.
//!
//! A foreign key to the referenced table's primary key builds that row's IRI from the
//! referencing row; others join the two tables.

use std::fmt::Write as _;

/// A table or view, as the direct mapping needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectTable {
    /// The table's name, as the database writes it.
    pub name: String,
    /// The schema the table is in, used to name it in SQL.
    pub schema: String,
    pub columns: Vec<String>,
    /// The primary key's columns, in key order; empty without one.
    pub primary_key: Vec<String>,
    pub foreign_keys: Vec<ForeignKey>,
    /// Whether the rows are stored (a table, partitioned table or materialized view), so
    /// that each has a row id; views and foreign tables number their rows instead.
    pub stored: bool,
}

/// A foreign key: `columns` of this table reference `referenced` columns of `table`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    pub columns: Vec<String>,
    /// The referenced table's name (in the same list of tables).
    pub table: String,
    pub referenced: Vec<String>,
}

/// The column of the row id that stands for a row without a primary key.
const ROW_ID: &str = "__tripletrail_row";

/// The direct mapping of `tables` as an R2RML mapping document (Turtle), with IRIs under
/// `base`. A foreign key to a table not in `tables` is left out.
pub fn r2rml(tables: &[DirectTable], base: &str) -> String {
    let mut out = String::from(
        "@prefix rr: <http://www.w3.org/ns/r2rml#> .\n\
         @prefix map: <urn:tripletrail:direct-mapping#> .\n\n",
    );
    let index = |name: &str| tables.iter().position(|t| t.name == name);
    for (i, t) in tables.iter().enumerate() {
        let table_iri = format!("{base}{}", encode(&t.name));
        let mut parts = vec![format!("map:t{i} a rr:TriplesMap")];
        if t.primary_key.is_empty() {
            // One blank node per row, also for rows that are alike: PostgreSQL's row id (with
            // the partition's, as each partition counts its own), or for views the row's
            // number, which only this query sees (no foreign key references a view).
            let row_id = if t.stored {
                "tableoid::text || ':' || ctid::text"
            } else {
                "(row_number() OVER ())::text"
            };
            parts.push(format!(
                "  rr:logicalTable [ rr:sqlQuery {} ]",
                string(&format!(
                    "SELECT *, {row_id} AS {} FROM {}.{}",
                    quote(ROW_ID),
                    quote(&t.schema),
                    quote(&t.name)
                ))
            ));
            parts.push(format!(
                "  rr:subjectMap [ rr:template {} ; rr:termType rr:BlankNode ; rr:class <{table_iri}> ]",
                string(&format!("{}-{{{ROW_ID}}}", template_text(&encode(&t.name))))
            ));
        } else {
            parts.push(format!(
                "  rr:logicalTable [ rr:sqlQuery {} ]",
                string(&format!(
                    "SELECT * FROM {}.{}",
                    quote(&t.schema),
                    quote(&t.name)
                ))
            ));
            parts.push(format!(
                "  rr:subjectMap [ rr:template {} ; rr:class <{table_iri}> ]",
                string(&row_template(base, &t.name, &t.primary_key, &t.primary_key))
            ));
        }
        for c in &t.columns {
            parts.push(format!(
                "  rr:predicateObjectMap [ rr:predicate <{table_iri}#{}> ; rr:objectMap [ rr:column {} ] ]",
                encode(c),
                string(c)
            ));
        }
        for fk in &t.foreign_keys {
            let Some(p) = index(&fk.table) else {
                continue;
            };
            let parent = &tables[p];
            let predicate = format!(
                "{table_iri}#ref-{}",
                fk.columns
                    .iter()
                    .map(|c| encode(c))
                    .collect::<Vec<_>>()
                    .join(";")
            );
            if !parent.primary_key.is_empty() && fk.referenced == parent.primary_key {
                // The referenced row's IRI, from this row's values.
                parts.push(format!(
                    "  rr:predicateObjectMap [ rr:predicate <{predicate}> ; rr:objectMap [ rr:template {} ] ]",
                    string(&row_template(base, &parent.name, &parent.primary_key, &fk.columns))
                ));
            } else {
                let conditions: Vec<String> = fk
                    .columns
                    .iter()
                    .zip(&fk.referenced)
                    .map(|(c, r)| format!("[ rr:child {} ; rr:parent {} ]", string(c), string(r)))
                    .collect();
                parts.push(format!(
                    "  rr:predicateObjectMap [ rr:predicate <{predicate}> ; rr:objectMap [ rr:parentTriplesMap map:t{p} ; rr:joinCondition {} ] ]",
                    conditions.join(" , ")
                ));
            }
        }
        out.push_str(&parts.join(" ;\n"));
        out.push_str(" .\n\n");
    }
    out
}

/// The template of a row's IRI: `base` `table/k1={v1};k2={v2}`, the key names of `table` with
/// the values of `columns`.
fn row_template(base: &str, table: &str, key: &[String], columns: &[String]) -> String {
    let parts: Vec<String> = key
        .iter()
        .zip(columns)
        .map(|(k, c)| format!("{}={{{}}}", template_text(&encode(k)), template_text(c)))
        .collect();
    format!(
        "{}{}/{}",
        template_text(base),
        template_text(&encode(table)),
        parts.join(";")
    )
}

/// Text of a template, its braces and backslashes escaped.
fn template_text(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('{', "\\{")
        .replace('}', "\\}")
}

/// The direct mapping's percent-encoding: every character that is not unreserved in an IRI
/// (`iunreserved` of RFC 3987) as `%XX` of its UTF-8 bytes.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let unreserved = c.is_ascii_alphanumeric()
            || matches!(c, '-' | '.' | '_' | '~')
            || (!c.is_ascii() && ucschar(c));
        if unreserved {
            out.push(c);
        } else {
            let mut buf = [0; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// Whether `c` is a `ucschar` of RFC 3987.
fn ucschar(c: char) -> bool {
    matches!(u32::from(c),
        0xA0..=0xD7FF | 0xF900..=0xFDCF | 0xFDF0..=0xFFEF
        | 0x10000..=0x1FFFD | 0x20000..=0x2FFFD | 0x30000..=0x3FFFD
        | 0x40000..=0x4FFFD | 0x50000..=0x5FFFD | 0x60000..=0x6FFFD
        | 0x70000..=0x7FFFD | 0x80000..=0x8FFFD | 0x90000..=0x9FFFD
        | 0xA0000..=0xAFFFD | 0xB0000..=0xBFFFD | 0xC0000..=0xCFFFD
        | 0xD0000..=0xDFFFD | 0xE1000..=0xEFFFD)
}

/// A SQL identifier, double-quoted.
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
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
    fn names_are_percent_encoded() {
        assert_eq!(encode("Student Name"), "Student%20Name");
        assert_eq!(encode("a/b#c;d=e"), "a%2Fb%23c%3Bd%3De");
        assert_eq!(encode("Bø"), "Bø");
        assert_eq!(encode("{x}"), "%7Bx%7D");
    }

    #[test]
    fn the_mapping_parses() {
        let tables = vec![
            DirectTable {
                name: "People".into(),
                schema: "public".into(),
                columns: vec!["ID".into(), "fname".into(), "addr".into()],
                primary_key: vec!["ID".into()],
                foreign_keys: vec![ForeignKey {
                    columns: vec!["addr".into()],
                    table: "Addresses".into(),
                    referenced: vec!["ID".into()],
                }],
                stored: true,
            },
            DirectTable {
                name: "Addresses".into(),
                schema: "public".into(),
                columns: vec!["ID".into(), "city".into()],
                primary_key: vec!["ID".into()],
                foreign_keys: Vec::new(),
                stored: true,
            },
            DirectTable {
                name: "Log".into(),
                schema: "public".into(),
                columns: vec!["note".into()],
                primary_key: Vec::new(),
                foreign_keys: vec![ForeignKey {
                    columns: vec!["note".into()],
                    table: "Addresses".into(),
                    referenced: vec!["city".into()],
                }],
                stored: false,
            },
            DirectTable {
                name: "Empty".into(),
                schema: "public".into(),
                columns: Vec::new(),
                primary_key: Vec::new(),
                foreign_keys: Vec::new(),
                stored: true,
            },
        ];
        let text = r2rml(&tables, "http://example.com/base/");
        assert!(
            text.contains("http://example.com/base/People/ID={ID}"),
            "{text}"
        );
        assert!(
            text.contains("<http://example.com/base/People#ref-addr>"),
            "{text}"
        );
        assert!(
            text.contains("http://example.com/base/Addresses/ID={addr}"),
            "{text}"
        );
        assert!(
            text.contains("rr:joinCondition [ rr:child \"note\" ; rr:parent \"city\" ]"),
            "{text}"
        );
        let mapping =
            crate::Mapping::parse_turtle(&text, None).unwrap_or_else(|e| panic!("{e}\n{text}"));
        assert_eq!(mapping.triples_maps.len(), 4, "{text}");
    }
}
