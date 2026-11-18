//! RDF Patch (`application/rdf-patch`, #831): the body of Fuseki's `/{ds}/patch` service,
//! read as the SPARQL update that makes the same change.
//!
//! A patch is lines: `H` headers, `TX`/`TC`/`TA` transaction marks, `PA`/`PD` prefixes, and
//! `A`/`D` rows that add or delete a triple or a quad (subject, predicate, object, graph),
//! each ending with ` .`. The rows become `INSERT DATA` and `DELETE DATA` operations in their
//! order, the whole patch one update: it applies all at once or not at all. A transaction
//! aborted with `TA` leaves out its rows.

use tripletrail::oxigraph::io::{RdfFormat, RdfParser};
use tripletrail::oxigraph::model::NamedNode;

/// The SPARQL update that applies `patch`.
pub(crate) fn to_update(patch: &str) -> Result<String, String> {
    let mut prefixes: Vec<(String, String)> = Vec::new();
    // The operations so far, and those of the open transaction.
    let mut done: Vec<(bool, String)> = Vec::new();
    let mut open: Option<Vec<(bool, String)>> = None;
    for (n, line) in patch.lines().enumerate() {
        let at = |m: String| format!("line {}: {m}", n + 1);
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (code, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        let rest = rest.strip_suffix('.').map_or(rest, str::trim_end);
        match code {
            "H" => {}
            "TX" => {
                if open.is_some() {
                    return Err(at("TX inside a transaction".into()));
                }
                open = Some(Vec::new());
            }
            "TC" => done.extend(open.take().ok_or_else(|| at("TC without TX".into()))?),
            "TA" => {
                open.take().ok_or_else(|| at("TA without TX".into()))?;
            }
            "PA" => {
                let terms = terms(rest).map_err(at)?;
                let [name, iri] = terms.as_slice() else {
                    return Err(at("PA takes a prefix and an IRI".into()));
                };
                let name = name
                    .trim_matches(['"', '\''])
                    .trim_end_matches(':')
                    .to_owned();
                let iri_ok = iri
                    .strip_prefix('<')
                    .and_then(|i| i.strip_suffix('>'))
                    .is_some_and(|i| NamedNode::new(i).is_ok());
                if !iri_ok {
                    return Err(at(format!("PA: {iri} is not an IRI in <>")));
                }
                if !name
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
                {
                    return Err(at(format!("PA: {name} is not a prefix name")));
                }
                prefixes.retain(|(p, _)| *p != name);
                prefixes.push((name, iri.clone()));
            }
            "PD" => {
                let terms = terms(rest).map_err(at)?;
                let name = terms
                    .first()
                    .ok_or_else(|| at("PD takes a prefix".into()))?
                    .trim_matches(['"', '\''])
                    .trim_end_matches(':')
                    .to_owned();
                prefixes.retain(|(p, _)| *p != name);
            }
            "A" | "D" => {
                let terms = terms(rest).map_err(at)?;
                let add = code == "A";
                if !add && terms.iter().any(|t| t.starts_with("_:")) {
                    return Err(at(
                        "a D row with a blank node: a SPARQL update can't name a blank node to delete".into(),
                    ));
                }
                // Each term must be one term, read as Turtle reads it, so that a row says
                // nothing but its triple or quad: the update is made of the rows' text.
                check_row(&prefixes, &terms).map_err(at)?;
                let row = match terms.as_slice() {
                    [s, p, o] => format!("{s} {p} {o} ."),
                    [s, p, o, g] => format!("GRAPH {g} {{ {s} {p} {o} . }}"),
                    _ => {
                        return Err(at(format!(
                            "{code} takes a subject, a predicate, an object and optionally a graph, not {} terms",
                            terms.len()
                        )));
                    }
                };
                open.as_mut().unwrap_or(&mut done).push((add, row));
            }
            other => return Err(at(format!("unknown row {other}"))),
        }
    }
    if open.is_some() {
        return Err("the patch ends inside a transaction (TX without TC)".into());
    }
    let prologue: String = prefixes
        .iter()
        .map(|(p, iri)| format!("PREFIX {p}: {iri}\n"))
        .collect();
    let mut ops: Vec<String> = Vec::new();
    let mut i = 0;
    while i < done.len() {
        let add = done[i].0;
        let mut rows = Vec::new();
        while i < done.len() && done[i].0 == add {
            rows.push(done[i].1.as_str());
            i += 1;
        }
        let keyword = if add { "INSERT DATA" } else { "DELETE DATA" };
        ops.push(format!("{keyword} {{\n  {}\n}}", rows.join("\n  ")));
    }
    if ops.is_empty() {
        // An empty patch changes nothing; an update must still have an operation.
        return Ok("INSERT DATA {}".into());
    }
    Ok(format!("{prologue}{}", ops.join(" ;\n")))
}

/// Whether `terms` (3 or 4) are one triple or quad, as TriG reads them with `prefixes`.
fn check_row(prefixes: &[(String, String)], terms: &[String]) -> Result<(), String> {
    let mut doc: String = prefixes
        .iter()
        .map(|(p, iri)| format!("@prefix {p}: {iri} .\n"))
        .collect();
    match terms {
        [s, p, o] => doc.push_str(&format!("{s} {p} {o} .\n")),
        [s, p, o, g] => {
            if !(g.starts_with('<') || g.contains(':')) || g.starts_with("_:") {
                return Err(format!("the graph {g} is not an IRI"));
            }
            doc.push_str(&format!("GRAPH {g} {{ {s} {p} {o} . }}\n"))
        }
        _ => return Ok(()),
    }
    let mut n = 0;
    for q in RdfParser::from_format(RdfFormat::TriG).for_slice(doc.as_bytes()) {
        q.map_err(|e| format!("not a row of RDF terms ({e})"))?;
        n += 1;
    }
    if n == 1 {
        Ok(())
    } else {
        Err(format!("the row gives {n} triples, not one"))
    }
}

/// The terms of a row, as written: `<iri>`, `prefix:name`, `_:label`, a literal with its
/// language or datatype, a number or a boolean.
fn terms(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut chars = s.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let end = match c {
            '<' => {
                let end = s[start..]
                    .find('>')
                    .ok_or_else(|| format!("an IRI without > ({})", &s[start..]))?;
                start + end + 1
            }
            '"' | '\'' => {
                // The string, then @lang or ^^datatype.
                let mut i = start + 1;
                let bytes = s.as_bytes();
                loop {
                    match bytes.get(i) {
                        None => return Err(format!("a string without its closing {c}")),
                        Some(b'\\') => i += 2,
                        Some(&b) if b == c as u8 => break,
                        Some(_) => i += 1,
                    }
                }
                i += 1;
                if s[i..].starts_with('@') || s[i..].starts_with("^^") {
                    let tail = &s[i..];
                    let skip = if tail.starts_with("^^<") {
                        tail.find('>')
                            .map(|e| e + 1)
                            .ok_or("a datatype without >")?
                    } else {
                        tail.find(char::is_whitespace).unwrap_or(tail.len())
                    };
                    i += skip;
                }
                i
            }
            _ => s[start..]
                .find(char::is_whitespace)
                .map_or(s.len(), |e| start + e),
        };
        out.push(s[start..end].to_owned());
        while chars.peek().is_some_and(|&(i, _)| i < end) {
            chars.next();
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_patch_becomes_an_update() {
        let patch = "H id <uuid:1> .\nTX .\nPA \"ex\" <https://example.org/> .\nA ex:a ex:p \"x y\"@en .\nA <urn:a> <urn:p> \"1\"^^<http://www.w3.org/2001/XMLSchema#integer> <urn:g> .\nD ex:a ex:p 'old' .\nTC .\nTX .\nA ex:b ex:p 2 .\nTA .\n";
        let u = to_update(patch).unwrap();
        assert_eq!(
            u,
            "PREFIX ex: <https://example.org/>\nINSERT DATA {\n  ex:a ex:p \"x y\"@en .\n  GRAPH <urn:g> { <urn:a> <urn:p> \"1\"^^<http://www.w3.org/2001/XMLSchema#integer> . }\n} ;\nDELETE DATA {\n  ex:a ex:p 'old' .\n}"
        );
        tripletrail::Engine::check_update(&u).unwrap();
        assert_eq!(to_update("").unwrap(), "INSERT DATA {}");
    }

    #[test]
    fn mistakes_are_placed() {
        assert!(to_update("A <a> <b> .").unwrap_err().starts_with("line 1:"));
        assert!(
            to_update("TX .\nA <urn:a> <urn:b> <urn:c> .")
                .unwrap_err()
                .contains("ends inside")
        );
        assert!(
            to_update("D _:b <p> <o> .")
                .unwrap_err()
                .contains("blank node")
        );
        assert!(to_update("X <a> .").unwrap_err().contains("unknown row X"));
        // Text that is not one term can't add operations to the update.
        for row in [
            "A <urn:a> <urn:b> 1};DROP ALL;INSERT DATA{<urn:a> <urn:b> 1 .",
            "A <urn:a> <urn:b> <urn:c>}};DROP ALL;# .",
            "A <urn:a> <urn:b> \"x\"};DROP\u{20}ALL;# .",
            "A <urn:a> <urn:b> ex:c};DROP .",
            "A <urn:a> <urn:b> <urn:c> <urn:g>}};DROP .",
        ] {
            assert!(to_update(row).is_err(), "{row}");
        }
        assert!(to_update("PA \"ex\" <urn:x> } ; DROP ALL .").is_err());
        assert!(to_update("PA \"e}x\" <urn:x> .").is_err());
    }
}
