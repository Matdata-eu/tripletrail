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
}

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
    for (p, o) in extra {
        lines.push(format!(
            "    {} {}",
            compact(p.as_str(), prefixes),
            term(o, prefixes)
        ));
    }
    format!("{} .", lines.join(" ;\n"))
}

/// The statements about `id` (a policy IRI), and the triples of the file outside `sg:` about
/// it.
/// A policy's statements, and its triples outside `sg:`.
/// The `sg:` terms of row rules.
const ROW_RULES: &[&str] = &[
    "visibleIf",
    "hideProperties",
    "editableIf",
    "hideReferences",
    "allowTransitivePaths",
];

type Found = (Vec<Span>, Vec<(NamedNode, Term)>);

fn find(text: &str, id: &str) -> Result<Found, String> {
    let mut directives = String::new();
    let mut found = Vec::new();
    let mut extra = Vec::new();
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
        if triples.first().is_some_and(|t| about(&t.subject)) {
            // The pages do not show row rules (#202), so they leave such a policy alone
            // rather than write it back without them.
            if let Some(t) = triples.iter().find(|t| {
                about(&t.subject)
                    && ROW_RULES
                        .iter()
                        .any(|r| t.predicate.as_str().strip_prefix(SG) == Some(r))
            }) {
                return Err(format!(
                    "Policy {id} has row-level rules (sg:{}): edit it in its file",
                    t.predicate.as_str().strip_prefix(SG).unwrap_or_default()
                ));
            }
            found.push(span);
            extra.extend(
                triples
                    .into_iter()
                    .filter(|t| about(&t.subject) && !t.predicate.as_str().starts_with(SG))
                    .filter(|t| {
                        t.predicate.as_str() != "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"
                    })
                    .map(|t| (t.predicate, t.object)),
            );
        }
    }
    Ok((found, extra))
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
    let (spans, extra) = find(text, &policy.id)?;
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
    let block = block(policy, &extra, &prefixes);
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
    let (spans, _) = find(text, id)?;
    if spans.is_empty() {
        return Err(format!("No policy {id} in the file"));
    }
    Ok(splice(text, &spans, None))
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
}
