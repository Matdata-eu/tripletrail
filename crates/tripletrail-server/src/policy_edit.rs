//! Editing sparql-guard policy files in place (#205): one `sg:Policy` is written or removed
//! and the rest of the Turtle file, comments and other policies included, stays as it was.
//!
//! The file is split into its top-level statements by a scanner that knows Turtle's IRIs,
//! strings and comments; each statement is parsed (with the directives before it) to learn its
//! subject. Writing a policy replaces the statements about it with one block, or appends the
//! block; its triples outside the `sg:` vocabulary (a label, a comment) are kept.

use crate::policy::SG;
use tripletrail::oxigraph::io::{RdfFormat, RdfParser};
use tripletrail::oxigraph::model::{NamedNode, NamedOrBlankNode, Term, Triple};

/// A policy as the access pages read and write it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicySpec {
    /// The policy's IRI.
    pub id: String,
    /// `sg:agent`: IRIs, `sg:Anyone` and `sg:AuthenticatedAgent` as full IRIs.
    pub agents: Vec<String>,
    pub groups: Vec<String>,
    pub read_all: bool,
    pub write_all: bool,
    /// `sg:readGraph`: graphs read, not written.
    pub read_graphs: Vec<String>,
    pub write_graphs: Vec<String>,
    /// The row rules (enterprise#8): `None` keeps those the file has, for a client that does
    /// not know them; `Some` writes these.
    pub rows: Option<Vec<RowSpec>>,
    /// `sg:hideReferences`, when given.
    pub hide_references: Option<bool>,
}

/// One row rule of a policy: `sg:visibleIf`, `sg:hideProperties` or `sg:editableIf` (the
/// local name, in `rule`) and the IRI of its shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowSpec {
    pub rule: String,
    pub shape: String,
}

/// The local names of the row rules a policy may have.
pub const ROW_KINDS: &[&str] = &["visibleIf", "hideProperties", "editableIf"];

impl PolicySpec {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "agents": self.agents,
            "groups": self.groups,
            "readAllGraphs": self.read_all && !self.write_all,
            "writeAllGraphs": self.write_all,
            "readGraphs": self.read_graphs,
            "writeGraphs": self.write_graphs,
            "rowRules": self.rows.as_ref().map(|rows| rows
                .iter()
                .map(|r| serde_json::json!({ "rule": format!("sg:{}", r.rule), "shape": r.shape }))
                .collect::<Vec<_>>()).unwrap_or_default(),
            "hideReferences": self.hide_references,
        })
    }

    pub fn from_json(v: &serde_json::Value) -> Result<Self, String> {
        let strings = |key: &str| -> Result<Vec<String>, String> {
            match v.get(key) {
                None | Some(serde_json::Value::Null) => Ok(Vec::new()),
                Some(serde_json::Value::Array(a)) => a
                    .iter()
                    .map(|s| {
                        s.as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| format!("{key} must be a list of strings"))
                    })
                    .collect(),
                Some(_) => Err(format!("{key} must be a list of strings")),
            }
        };
        let flag = |key: &str| -> Result<bool, String> {
            match v.get(key) {
                None | Some(serde_json::Value::Null) => Ok(false),
                Some(b) => b
                    .as_bool()
                    .ok_or_else(|| format!("{key} must be true or false")),
            }
        };
        let write_all = flag("writeAllGraphs")?;
        let rows = match v.get("rowRules") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::Array(a)) => Some(
                a.iter()
                    .map(|r| {
                        let rule = r["rule"].as_str().unwrap_or_default();
                        let rule = rule.strip_prefix("sg:").unwrap_or(rule);
                        let rule = rule.strip_prefix(SG).unwrap_or(rule);
                        if !ROW_KINDS.contains(&rule) {
                            return Err(format!(
                                "rowRules: rule must be sg:visibleIf, sg:hideProperties or sg:editableIf, not {}",
                                r["rule"]
                            ));
                        }
                        let shape = r["shape"]
                            .as_str()
                            .ok_or("rowRules: shape (the shape's IRI) is missing")?;
                        NamedNode::new(shape).map_err(|e| format!("rowRules: shape {shape}: {e}"))?;
                        Ok(RowSpec { rule: rule.to_owned(), shape: shape.to_owned() })
                    })
                    .collect::<Result<Vec<_>, String>>()?,
            ),
            Some(_) => return Err("rowRules must be a list".to_owned()),
        };
        let hide_references = match v.get("hideReferences") {
            None | Some(serde_json::Value::Null) => None,
            Some(b) => Some(b.as_bool().ok_or("hideReferences must be true or false")?),
        };
        Ok(Self {
            id: v
                .get("id")
                .and_then(|i| i.as_str())
                .ok_or("id (the policy's IRI) is missing")?
                .to_owned(),
            agents: strings("agents")?,
            groups: strings("groups")?,
            read_all: flag("readAllGraphs")? || write_all,
            write_all,
            read_graphs: strings("readGraphs")?,
            write_graphs: strings("writeGraphs")?,
            rows,
            hide_references,
        })
    }
}

/// One top-level statement: its text, from its first token to its final `.` (or, for a
/// SPARQL-style `PREFIX`/`BASE`, its IRI's `>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Span {
    start: usize,
    end: usize,
}

/// The top-level statements of a Turtle document. Text that ends without a final `.` is left
/// out (the parser then rejects the file anyway).
fn statements(text: &str) -> Vec<Span> {
    let b = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    let mut start: Option<usize> = None;
    // A SPARQL-style directive ends at its IRI.
    let mut sparql_directive = false;
    while i < b.len() {
        let c = b[i];
        if start.is_none() {
            if c.is_ascii_whitespace() {
                i += 1;
                continue;
            }
            if c == b'#' {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            start = Some(i);
            let word: String = text[i..]
                .chars()
                .take_while(|c| c.is_ascii_alphabetic())
                .collect::<String>()
                .to_ascii_uppercase();
            sparql_directive = (word == "PREFIX" || word == "BASE")
                && text[i + word.len()..].starts_with(|c: char| c.is_whitespace());
        }
        match c {
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'<' if b.get(i + 1) == Some(&b'<') => i += 2,
            b'<' => {
                // An IRI: up to its `>`.
                while i < b.len() && b[i] != b'>' {
                    i += 1;
                }
                i += 1;
                if sparql_directive {
                    spans.push(Span {
                        start: start.take().unwrap_or(0),
                        end: i.min(b.len()),
                    });
                }
                continue;
            }
            b'"' | b'\'' => {
                let long = b.get(i + 1) == Some(&c) && b.get(i + 2) == Some(&c);
                i += if long { 3 } else { 1 };
                while i < b.len() {
                    if b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == c
                        && (!long || (b.get(i + 1) == Some(&c) && b.get(i + 2) == Some(&c)))
                    {
                        i += if long { 3 } else { 1 };
                        break;
                    }
                    if !long && b[i] == b'\n' {
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            b'\\' => {
                // An escaped character of a local name (`ex:a\#b`).
                i += 2;
                continue;
            }
            b'.' if b
                .get(i + 1)
                .is_none_or(|n| n.is_ascii_whitespace() || *n == b'#') =>
            {
                i += 1;
                spans.push(Span {
                    start: start.take().unwrap_or(0),
                    end: i,
                });
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    spans
}

fn is_directive(statement: &str) -> bool {
    let word = statement
        .chars()
        .take_while(|c| c.is_ascii_alphabetic() || *c == '@')
        .collect::<String>()
        .to_ascii_lowercase();
    matches!(word.as_str(), "@prefix" | "@base" | "prefix" | "base")
}

/// The triples of a statement, read with the directives before it.
fn triples(directives: &str, statement: &str) -> Result<Vec<Triple>, String> {
    let text = format!("{directives}\n{statement}\n");
    RdfParser::from_format(RdfFormat::Turtle)
        .for_slice(text.as_bytes())
        .map(|q| q.map(Triple::from).map_err(|e| e.to_string()))
        .collect()
}

/// The prefixes the file declares, as (name, namespace).
fn prefixes(text: &str) -> Vec<(String, String)> {
    let mut parser = RdfParser::from_format(RdfFormat::Turtle).for_slice(text.as_bytes());
    for _ in parser.by_ref() {}
    parser
        .prefixes()
        .map(|(p, n)| (p.to_owned(), n.to_owned()))
        .collect()
}

/// An IRI as `prefix:local` when a declared prefix gives a plain local name, else `<iri>`.
fn compact(iri: &str, prefixes: &[(String, String)]) -> String {
    for (name, namespace) in prefixes {
        if let Some(local) = iri.strip_prefix(namespace.as_str())
            && !local.is_empty()
            && local
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            && local.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        {
            return format!("{name}:{local}");
        }
    }
    format!("<{iri}>")
}

fn term(t: &Term, prefixes: &[(String, String)]) -> String {
    match t {
        Term::NamedNode(n) => compact(n.as_str(), prefixes),
        other => other.to_string(),
    }
}

/// The Turtle block of a policy, with `extra` (its triples outside `sg:`) kept.
fn block(
    policy: &PolicySpec,
    rows: &[RowSpec],
    hide_references: Option<bool>,
    extra: &[(NamedNode, Term)],
    prefixes: &[(String, String)],
) -> String {
    let sg = |local: &str| compact(&format!("{SG}{local}"), prefixes);
    let mut lines = vec![format!(
        "{} a {}",
        compact(&policy.id, prefixes),
        sg("Policy")
    )];
    for a in &policy.agents {
        lines.push(format!("    {} {}", sg("agent"), compact(a, prefixes)));
    }
    for g in &policy.groups {
        lines.push(format!(
            "    {} {}",
            sg("group"),
            Term::from(tripletrail::oxigraph::model::Literal::new_simple_literal(g))
        ));
    }
    if policy.write_all {
        lines.push(format!("    {} true", sg("writeAllGraphs")));
    } else if policy.read_all {
        lines.push(format!("    {} true", sg("readAllGraphs")));
    }
    for g in &policy.read_graphs {
        if !policy.write_graphs.contains(g) {
            lines.push(format!("    {} {}", sg("readGraph"), compact(g, prefixes)));
        }
    }
    for g in &policy.write_graphs {
        lines.push(format!("    {} {}", sg("writeGraph"), compact(g, prefixes)));
    }
    for r in rows {
        lines.push(format!("    {} {}", sg(&r.rule), compact(&r.shape, prefixes)));
    }
    if let Some(h) = hide_references {
        lines.push(format!("    {} {h}", sg("hideReferences")));
    }
    for (p, o) in extra {
        lines.push(format!(
            "    {} {}",
            compact(p.as_str(), prefixes),
            term(o, prefixes)
        ));
    }
    format!("{} .", lines.join(" ;\n"))
}

/// The `sg:` terms of row rules.
const ROW_RULES: &[&str] = &[
    "visibleIf",
    "hideProperties",
    "editableIf",
    "hideReferences",
    "allowTransitivePaths",
];

/// What the file says about a policy: its statements, its triples outside `sg:`, its row
/// rules and its `sg:hideReferences`.
struct Found {
    spans: Vec<Span>,
    extra: Vec<(NamedNode, Term)>,
    rows: Vec<RowSpec>,
    hide_references: Option<bool>,
}

fn find(text: &str, id: &str) -> Result<Found, String> {
    let mut directives = String::new();
    let mut found = Found {
        spans: Vec::new(),
        extra: Vec::new(),
        rows: Vec::new(),
        hide_references: None,
    };
    for span in statements(text) {
        let statement = &text[span.start..span.end];
        if is_directive(statement) {
            directives.push_str(statement);
            directives.push('\n');
            continue;
        }
        let triples = triples(&directives, statement)?;
        let about =
            |s: &NamedOrBlankNode| matches!(s, NamedOrBlankNode::NamedNode(n) if n.as_str() == id);
        if !triples.first().is_some_and(|t| about(&t.subject)) {
            continue;
        }
        for t in triples.iter().filter(|t| about(&t.subject)) {
            let Some(local) = t.predicate.as_str().strip_prefix(SG) else {
                continue;
            };
            if !ROW_RULES.contains(&local) {
                continue;
            }
            match (&t.object, local) {
                (Term::NamedNode(shape), _) if ROW_KINDS.contains(&local) => {
                    found.rows.push(RowSpec {
                        rule: local.to_owned(),
                        shape: shape.as_str().to_owned(),
                    });
                }
                (Term::Literal(l), "hideReferences") => {
                    found.hide_references = Some(matches!(l.value(), "true" | "1"));
                }
                // A shape written in place (a blank node) is in the same statement, and the
                // pages would lose it: such a policy is left to its file (#202).
                _ => {
                    return Err(format!(
                        "Policy {id} has a row rule (sg:{local}) the pages cannot write back: edit it in its file"
                    ));
                }
            }
        }
        found.spans.push(span);
        found.extra.extend(
            triples
                .into_iter()
                .filter(|t| about(&t.subject) && !t.predicate.as_str().starts_with(SG))
                .filter(|t| {
                    t.predicate.as_str() != "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"
                })
                .map(|t| (t.predicate, t.object)),
        );
    }
    Ok(found)
}

/// Removes the statements in `spans` (with the line break after each), and puts `with` where
/// the first was.
fn splice(text: &str, spans: &[Span], with: Option<&str>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (i, span) in spans.iter().enumerate() {
        out.push_str(&text[at..span.start]);
        at = span.end;
        match (i, with) {
            (0, Some(w)) => out.push_str(w),
            _ => {
                // Drop the rest of the line when it is empty or a comment on the statement.
                let line = text[at..].split('\n').next().unwrap_or("");
                if line.trim_start().starts_with('#') || line.trim().is_empty() {
                    at += line.len();
                }
                if text[at..].starts_with('\n') {
                    at += 1;
                } else if text[at..].starts_with("\r\n") {
                    at += 2;
                }
            }
        }
    }
    out.push_str(&text[at..]);
    out
}

/// `text` with `policy` written: replacing the statements about it, or appended.
pub fn write(text: &str, policy: &PolicySpec) -> Result<String, String> {
    NamedNode::new(&policy.id).map_err(|e| format!("Policy id {}: {e}", policy.id))?;
    for iri in policy
        .agents
        .iter()
        .chain(&policy.read_graphs)
        .chain(&policy.write_graphs)
    {
        NamedNode::new(iri).map_err(|e| format!("{iri}: {e}"))?;
    }
    if policy.agents.is_empty() && policy.groups.is_empty() {
        return Err("A policy needs an agent or a group".to_owned());
    }
    let found = find(text, &policy.id)?;
    let rows = policy.rows.as_ref().unwrap_or(&found.rows);
    let hide_references = if policy.rows.is_some() {
        policy.hide_references
    } else {
        policy.hide_references.or(found.hide_references)
    };
    let spans = found.spans;
    let mut text = text.to_owned();
    let mut prefixes = prefixes(&text);
    // The sg: prefix, when the file has none for the vocabulary and the name is free.
    if !prefixes.iter().any(|(_, n)| n == SG) && !prefixes.iter().any(|(p, _)| p == "sg") {
        // After the comment lines that open the file.
        let at = text
            .split_inclusive('\n')
            .take_while(|l| l.trim_start().starts_with('#'))
            .map(str::len)
            .sum();
        text.insert_str(at, &format!("@prefix sg: <{SG}> .\n"));
        prefixes.push(("sg".to_owned(), SG.to_owned()));
        return write(&text, policy);
    }
    let block = block(policy, rows, hide_references, &found.extra, &prefixes);
    Ok(if spans.is_empty() {
        let sep = if text.is_empty() || text.ends_with("\n\n") {
            ""
        } else if text.ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        format!("{text}{sep}{block}\n")
    } else {
        splice(&text, &spans, Some(&block))
    })
}

/// `text` without the statements about the policy `id`.
pub fn remove(text: &str, id: &str) -> Result<String, String> {
    let spans = find(text, id)?.spans;
    if spans.is_empty() {
        return Err(format!("No policy {id} in the file"));
    }
    Ok(splice(text, &spans, None))
}

const SHACL: &str = "http://www.w3.org/ns/shacl#";
const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

/// The directives of `text` (its `@prefix` and `@base` lines), for reading a piece of Turtle
/// with the file's prefixes.
fn directives(text: &str) -> String {
    statements(text)
        .into_iter()
        .map(|s| &text[s.start..s.end])
        .filter(|s| is_directive(s))
        .map(|s| format!("{s}\n"))
        .collect()
}

/// Turtle for a predicate: `a` for `rdf:type`.
fn predicate(p: &str, prefixes: &[(String, String)]) -> String {
    if p == format!("{RDF}type") {
        "a".to_owned()
    } else {
        compact(p, prefixes)
    }
}

/// Turtle for the object `o`: a blank node written in place (`[ … ]`, or `( … )` for a list),
/// with its own triples, at `depth` levels of indentation.
fn object(
    o: &Term,
    triples: &[Triple],
    prefixes: &[(String, String)],
    depth: usize,
    seen: &mut Vec<tripletrail::oxigraph::model::BlankNode>,
) -> Result<String, String> {
    let Term::BlankNode(b) = o else {
        return Ok(term(o, prefixes));
    };
    if seen.contains(b) {
        return Err("A blank node of the shape is used twice or in a cycle".to_owned());
    }
    seen.push(b.clone());
    let about: Vec<&Triple> = triples
        .iter()
        .filter(|t| matches!(&t.subject, NamedOrBlankNode::BlankNode(s) if s == b))
        .collect();
    let first = format!("{RDF}first");
    let rest = format!("{RDF}rest");
    // A list: rdf:first and rdf:rest only.
    if !about.is_empty()
        && about
            .iter()
            .all(|t| t.predicate.as_str() == first || t.predicate.as_str() == rest)
    {
        let mut items = Vec::new();
        let mut node = o.clone();
        loop {
            match &node {
                Term::NamedNode(n) if n.as_str() == format!("{RDF}nil") => break,
                Term::BlankNode(b) => {
                    let get = |p: &str| {
                        triples.iter().find(|t| {
                            matches!(&t.subject, NamedOrBlankNode::BlankNode(s) if s == b)
                                && t.predicate.as_str() == p
                        })
                    };
                    let (Some(f), Some(r)) = (get(&first), get(&rest)) else {
                        return Err("A list of the shape is not well formed".to_owned());
                    };
                    if !items.is_empty() {
                        if seen.contains(b) {
                            return Err("A list of the shape is in a cycle".to_owned());
                        }
                        seen.push(b.clone());
                    }
                    items.push(object(&f.object, triples, prefixes, depth + 1, seen)?);
                    node = r.object.clone();
                }
                _ => return Err("A list of the shape is not well formed".to_owned()),
            }
        }
        return Ok(format!("( {} )", items.join(" ")));
    }
    if about.is_empty() {
        return Ok("[]".to_owned());
    }
    let pad = "    ".repeat(depth + 1);
    let mut lines = Vec::new();
    for t in about {
        lines.push(format!(
            "{pad}{} {}",
            predicate(t.predicate.as_str(), prefixes),
            object(&t.object, triples, prefixes, depth + 1, seen)?
        ));
    }
    Ok(format!("[\n{}\n{}]", lines.join(" ;\n"), "    ".repeat(depth)))
}

/// The Turtle block of the shape `id`, from `triples`: those about it, with the blank nodes
/// they name written in place. Every triple must be about `id` or one of those blank nodes.
fn shape_block(
    id: &str,
    triples: &[Triple],
    prefixes: &[(String, String)],
) -> Result<String, String> {
    let mut lines = Vec::new();
    let mut seen = Vec::new();
    let mut typed = false;
    for t in triples
        .iter()
        .filter(|t| matches!(&t.subject, NamedOrBlankNode::NamedNode(n) if n.as_str() == id))
    {
        if t.predicate.as_str() == format!("{RDF}type") {
            typed = true;
        }
        lines.push(format!(
            "    {} {}",
            predicate(t.predicate.as_str(), prefixes),
            object(&t.object, triples, prefixes, 1, &mut seen)?
        ));
    }
    if lines.is_empty() {
        return Err(format!("The Turtle has no triples about the shape {id}"));
    }
    if !typed {
        lines.insert(0, format!("    a {}", compact(&format!("{SHACL}NodeShape"), prefixes)));
    }
    let written = triples.iter().filter(|t| match &t.subject {
        NamedOrBlankNode::NamedNode(n) => n.as_str() == id,
        NamedOrBlankNode::BlankNode(b) => seen.contains(b),
    });
    if written.count() != triples.len() {
        return Err(format!(
            "The Turtle may only describe the shape {id} and the blank nodes it uses"
        ));
    }
    let first = lines.remove(0);
    let first = first.trim_start();
    Ok(if lines.is_empty() {
        format!("{} {first} .", compact(id, prefixes))
    } else {
        format!("{} {first} ;\n{} .", compact(id, prefixes), lines.join(" ;\n"))
    })
}

/// The statements whose subject is `id`.
fn spans_about(text: &str, id: &str) -> Result<Vec<Span>, String> {
    let mut directives = String::new();
    let mut found = Vec::new();
    for span in statements(text) {
        let statement = &text[span.start..span.end];
        if is_directive(statement) {
            directives.push_str(statement);
            directives.push('\n');
            continue;
        }
        let triples = triples(&directives, statement)?;
        if triples
            .first()
            .is_some_and(|t| matches!(&t.subject, NamedOrBlankNode::NamedNode(n) if n.as_str() == id))
        {
            found.push(span);
        }
    }
    Ok(found)
}

/// `text` with the shape `id` (enterprise#8) written from `turtle`, which may use the file's
/// prefixes and declare its own: replacing the statements about it, or appended. The shape
/// is written again from its triples, with the file's prefixes (and `sh:`, added when the
/// file has no prefix for SHACL), so a page's Turtle never breaks the file around it.
pub fn write_shape(text: &str, id: &str, turtle: &str) -> Result<String, String> {
    NamedNode::new(id).map_err(|e| format!("Shape id {id}: {e}"))?;
    let mut text = text.to_owned();
    let mut prefixes = prefixes(&text);
    for (name, iri) in [("sh", SHACL), ("sg", SG)] {
        if !prefixes.iter().any(|(_, n)| n == iri) && !prefixes.iter().any(|(p, _)| p == name) {
            let at = text
                .split_inclusive('\n')
                .take_while(|l| l.trim_start().starts_with('#'))
                .map(str::len)
                .sum();
            text.insert_str(at, &format!("@prefix {name}: <{iri}> .\n"));
            prefixes.push((name.to_owned(), iri.to_owned()));
        }
    }
    let triples = triples(&directives(&text), turtle)
        .map_err(|e| format!("The shape's Turtle does not parse: {e}"))?;
    let block = shape_block(id, &triples, &prefixes)?;
    let spans = spans_about(&text, id)?;
    Ok(if spans.is_empty() {
        let sep = if text.is_empty() || text.ends_with("\n\n") {
            ""
        } else if text.ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        format!("{text}{sep}{block}\n")
    } else {
        splice(&text, &spans, Some(&block))
    })
}

/// `text` without the statements about the shape `id`.
pub fn remove_shape(text: &str, id: &str) -> Result<String, String> {
    let spans = spans_about(text, id)?;
    if spans.is_empty() {
        return Err(format!("No shape {id} in the file"));
    }
    Ok(splice(text, &spans, None))
}

/// The node shapes of a policy file's text, each as Turtle written from its triples (see
/// [`write_shape`]), with its IRI.
pub fn shapes(text: &str) -> Result<Vec<(String, String)>, String> {
    let all: Vec<Triple> = RdfParser::from_format(RdfFormat::Turtle)
        .for_slice(text.as_bytes())
        .map(|t| t.map(Triple::from).map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;
    let prefixes = prefixes(text);
    let node_shape = format!("{SHACL}NodeShape");
    let mut out = Vec::new();
    for t in &all {
        if let (NamedOrBlankNode::NamedNode(id), Term::NamedNode(class)) = (&t.subject, &t.object)
            && t.predicate.as_str() == format!("{RDF}type")
            && class.as_str() == node_shape
        {
            // Its triples and those of the blank nodes under it.
            let mut keep: Vec<Triple> = Vec::new();
            let mut todo = vec![NamedOrBlankNode::NamedNode(id.clone())];
            while let Some(s) = todo.pop() {
                for t in all.iter().filter(|t| t.subject == s) {
                    if let Term::BlankNode(b) = &t.object
                        && !keep.iter().any(|k| matches!(&k.subject, NamedOrBlankNode::BlankNode(x) if x == b))
                    {
                        todo.push(NamedOrBlankNode::BlankNode(b.clone()));
                    }
                    if !keep.contains(t) {
                        keep.push(t.clone());
                    }
                }
            }
            if let Ok(block) = shape_block(id.as_str(), &keep, &prefixes) {
                out.push((id.as_str().to_owned(), block));
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "# Network access, kept by hand and by the UI.
@prefix sg: <https://matdata.eu/ns/sparql-guard#> .
@prefix ex: <https://example.org/policies/> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
PREFIX g: <urn:assets:>

# Everyone signed in reads the release. (The dot here. is not the end.)
ex:Readers a sg:Policy ; sg:agent sg:AuthenticatedAgent ;
    sg:readGraph g:release .   # trailing comment

ex:Writers a sg:Policy ;
    rdfs:label \"Asset team; writes. \\\"all\\\"\"@en ;
    sg:group \"asset-team\" ; sg:writeAllGraphs true .
";

    fn spec(id: &str) -> PolicySpec {
        PolicySpec {
            id: id.to_owned(),
            groups: vec!["contractors".to_owned()],
            read_graphs: vec!["urn:assets:release".to_owned()],
            ..Default::default()
        }
    }

    #[test]
    fn statements_split_at_their_final_dots() {
        let file = FILE;
        let spans = statements(file);
        let texts: Vec<&str> = spans.iter().map(|s| &file[s.start..s.end]).collect();
        assert_eq!(texts.len(), 6, "{texts:#?}");
        assert!(texts[3].starts_with("PREFIX g:") && texts[3].ends_with('>'));
        assert!(texts[4].starts_with("ex:Readers") && texts[4].ends_with("g:release ."));
        assert!(texts[5].ends_with("sg:writeAllGraphs true ."));
        // An escaped `#` in a local name is not a comment.
        let escaped = "@prefix ex: <urn:x:> .\nex:a\\#b ex:p 1 .\nex:c ex:p 2 .\n";
        assert_eq!(statements(escaped).len(), 3);
    }

    #[test]
    fn writing_a_policy_keeps_the_rest_of_the_file() {
        let file = FILE;
        // A new policy is appended.
        let added = write(file, &spec("https://example.org/policies/Contractors")).unwrap();
        assert!(added.starts_with(file), "{added}");
        assert!(
            added.ends_with(
                "\nex:Contractors a sg:Policy ;\n    sg:group \"contractors\" ;\n    sg:readGraph g:release .\n"
            ),
            "{added}"
        );
        // A changed one is replaced where it was; its label stays, comments elsewhere too.
        let mut writers = spec("https://example.org/policies/Writers");
        writers.groups = vec!["asset-team".to_owned()];
        writers.read_graphs.clear();
        writers.write_graphs = vec!["urn:assets:wip".to_owned()];
        let changed = write(&added, &writers).unwrap();
        assert!(changed.contains("# trailing comment"));
        assert!(changed.contains(
            "ex:Writers a sg:Policy ;\n    sg:group \"asset-team\" ;\n    sg:writeGraph g:wip ;\n    rdfs:label \"Asset team; writes. \\\"all\\\"\"@en .\n\nex:Contractors"
        ), "{changed}");
        let policies = crate::policy::read_text(&changed, RdfFormat::Turtle).unwrap();
        assert_eq!(policies.len(), 3);
        // Removing one leaves the others as they were.
        let removed = remove(&changed, "https://example.org/policies/Contractors").unwrap();
        assert_eq!(
            crate::policy::read_text(&removed, RdfFormat::Turtle)
                .unwrap()
                .len(),
            2
        );
        assert!(removed.starts_with("# Network access"));
        // The removed policy's trailing comment goes with it.
        let commented = remove(file, "https://example.org/policies/Readers").unwrap();
        assert!(!commented.contains("# trailing comment"), "{commented}");
        assert!(commented.contains("ex:Writers a sg:Policy"));
        assert!(remove(&removed, "https://example.org/policies/Contractors").is_err());
    }

    #[test]
    fn a_file_without_the_prefix_gets_it() {
        let written = write("", &spec("urn:p:one")).unwrap();
        assert_eq!(
            written,
            "@prefix sg: <https://matdata.eu/ns/sparql-guard#> .\n\n<urn:p:one> a sg:Policy ;\n    sg:group \"contractors\" ;\n    sg:readGraph <urn:assets:release> .\n"
        );
        assert!(
            write(
                "",
                &PolicySpec {
                    id: "urn:p".to_owned(),
                    ..Default::default()
                }
            )
            .is_err()
        );
    }

    const ROWS: &str = "@prefix sg: <https://matdata.eu/ns/sparql-guard#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.org/> .

ex:Contractors a sg:Policy ; sg:group \"contractor\" ; sg:readAllGraphs true ;
    sg:visibleIf ex:OwnArea ; sg:hideReferences false .

ex:OwnArea a sh:NodeShape ; sh:targetClass ex:Project ;
    sh:property [ sh:path ex:area ; sh:hasValue sg:requestingAgent ] .
";

    #[test]
    fn row_rules_are_kept_when_a_client_leaves_them_out_and_written_when_it_gives_them() {
        let mut spec = PolicySpec {
            id: "http://example.org/Contractors".to_owned(),
            groups: vec!["contractor".to_owned()],
            read_all: true,
            ..Default::default()
        };
        let kept = write(ROWS, &spec).unwrap();
        assert!(kept.contains("sg:visibleIf ex:OwnArea"), "{kept}");
        assert!(kept.contains("sg:hideReferences false"), "{kept}");
        spec.rows = Some(vec![RowSpec {
            rule: "editableIf".to_owned(),
            shape: "http://example.org/OwnArea".to_owned(),
        }]);
        let changed = write(ROWS, &spec).unwrap();
        assert!(changed.contains("sg:editableIf ex:OwnArea"), "{changed}");
        assert!(!changed.contains("visibleIf") && !changed.contains("hideReferences"), "{changed}");
        // As JSON, and back.
        let json = spec.to_json();
        assert_eq!(json["rowRules"][0]["rule"], "sg:editableIf");
        assert_eq!(PolicySpec::from_json(&json).unwrap().rows, spec.rows);
        let bad = serde_json::json!({ "id": "urn:p", "groups": ["g"], "rowRules": [{ "rule": "sg:hideAll", "shape": "urn:s" }] });
        assert!(PolicySpec::from_json(&bad).is_err());
        // A shape written in place stays in its file.
        let inline = "@prefix sg: <https://matdata.eu/ns/sparql-guard#> .
            <urn:p> a sg:Policy ; sg:group \"g\" ; sg:visibleIf [ a <http://www.w3.org/ns/shacl#NodeShape> ] .";
        let err = write(inline, &PolicySpec { id: "urn:p".to_owned(), groups: vec!["g".to_owned()], ..Default::default() });
        assert!(err.unwrap_err().contains("edit it in its file"));
    }

    #[test]
    fn shapes_are_written_from_their_triples_and_listed() {
        let turtle = "@prefix ex: <http://example.org/> .
            ex:OwnArea a sh:NodeShape ; sh:targetClass ex:Project ;
              sh:property [ sh:path ( ex:assignedTo ex:area ) ; sh:in ( ex:north ex:east ) ] .";
        let written = write_shape(ROWS, "http://example.org/OwnArea", turtle).unwrap();
        assert_eq!(written.matches("ex:OwnArea a sh:NodeShape").count(), 1, "{written}");
        assert!(written.contains("sh:path ( ex:assignedTo ex:area )"), "{written}");
        assert!(written.contains("sh:in ( ex:north ex:east )"), "{written}");
        assert!(!written.contains("requestingAgent"), "{written}");
        assert!(written.contains("sg:visibleIf ex:OwnArea"), "{written}");
        // The file still reads, with the same policy.
        assert!(
            RdfParser::from_format(RdfFormat::Turtle)
                .for_slice(written.as_bytes())
                .all(|t| t.is_ok())
        );
        assert_eq!(find(&written, "http://example.org/Contractors").unwrap().rows.len(), 1);
        let listed = shapes(&written).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, "http://example.org/OwnArea");
        assert!(listed[0].1.contains("sh:in ( ex:north ex:east )"), "{}", listed[0].1);
        // A new shape is appended, with sh: added to a file without it.
        let fresh = write_shape(
            "@prefix ex: <http://example.org/> .\n",
            "http://example.org/Open",
            "<http://example.org/Open> <http://www.w3.org/ns/shacl#targetClass> ex:Doc .",
        )
        .unwrap();
        assert!(fresh.contains("@prefix sh: <http://www.w3.org/ns/shacl#> ."), "{fresh}");
        assert!(fresh.contains("ex:Open a sh:NodeShape ;\n    sh:targetClass ex:Doc ."), "{fresh}");
        // Triples about anything else are refused, and so is Turtle that does not parse.
        assert!(write_shape(ROWS, "http://example.org/OwnArea", "ex:Other a sh:NodeShape .").is_err());
        assert!(write_shape(ROWS, "http://example.org/OwnArea", "ex:OwnArea sh:targetClass").is_err());
        let removed = remove_shape(&written, "http://example.org/OwnArea").unwrap();
        assert!(!removed.contains("ex:OwnArea a"), "{removed}");
        assert!(remove_shape(&removed, "http://example.org/OwnArea").is_err());
    }
}
