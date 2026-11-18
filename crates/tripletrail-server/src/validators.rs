//! Fuseki's syntax checks (#831): `/$/validate/query`, `update`, `iri`, `data` and `langtag`,
//! which the Fuseki UI and some editors call. Each takes its input as a parameter (of the URL
//! or a form body) and answers JSON in Fuseki's shape: the `input`, and `errors` (with
//! `parse-error`, `parse-error-line` and `parse-error-column` where they are known) and
//! `warnings` lists.

use crate::{HttpError, Params, Reply, json_reply};
use serde_json::{Value, json};
use tripletrail::Engine;
use tripletrail::oxigraph::io::{RdfFormat, RdfParser};
use tripletrail::oxigraph::model::{Literal, NamedNode};

/// Answers `/$/validate/{kind}`.
pub(crate) fn route(kind: &str, params: &Params) -> Result<Reply, HttpError> {
    let input = |name: &str| {
        params.get(name).ok_or_else(|| {
            HttpError::bad_request(format!(
                "/$/validate/{kind} takes the text to check as {name}="
            ))
        })
    };
    let body = match kind {
        "query" => {
            let text = input("query")?;
            text_check(text, Engine::check_query(text).map_err(|e| e.to_string()))
        }
        "update" => {
            let text = input("update")?;
            text_check(text, Engine::check_update(text).map_err(|e| e.to_string()))
        }
        "data" => {
            let text = input("data")?;
            let syntax = params.get("languageSyntax").unwrap_or("Turtle");
            let format = format_of(syntax).ok_or_else(|| {
                HttpError::bad_request(format!(
                    "languageSyntax must be Turtle, N-Triples, TriG, N-Quads, RDF/XML or JSON-LD, not {syntax}"
                ))
            })?;
            let mut checked = Ok(());
            for q in RdfParser::from_format(format).for_slice(text.as_bytes()) {
                if let Err(e) = q {
                    checked = Err(e.to_string());
                    break;
                }
            }
            text_check(text, checked)
        }
        "iri" => {
            let iris: Vec<Value> = params
                .0
                .iter()
                .filter(|(k, _)| k == "iri")
                .map(|(_, iri)| {
                    let errors: Vec<Value> = match NamedNode::new(iri) {
                        Ok(_) => Vec::new(),
                        Err(e) => vec![json!(e.to_string())],
                    };
                    json!({ "iri": iri, "error": errors, "warnings": [] })
                })
                .collect();
            if iris.is_empty() {
                return Err(HttpError::bad_request(
                    "/$/validate/iri takes the IRIs to check as iri= (one or more)",
                ));
            }
            json!({ "iris": iris })
        }
        "langtag" => {
            let tag = input("langtag")?;
            let (valid, errors) = match Literal::new_language_tagged_literal("x", tag) {
                Ok(_) => (true, Vec::new()),
                Err(e) => (false, vec![json!(e.to_string())]),
            };
            json!({ "input": tag, "valid": valid, "errors": errors, "warnings": [] })
        }
        _ => {
            return Err(HttpError(
                404,
                format!(
                    "No /$/validate/{kind}: the checks are query, update, iri, data and langtag"
                ),
            ));
        }
    };
    Ok(json_reply(200, &body))
}

/// The answer for a text that parses, or does not with the message `checked` gives.
fn text_check(text: &str, checked: Result<(), String>) -> Value {
    let errors = match checked {
        Ok(()) => Vec::new(),
        Err(message) => {
            let mut e = json!({ "parse-error": message });
            if let Some((line, column)) = position(&message) {
                e["parse-error-line"] = json!(line);
                e["parse-error-column"] = json!(column);
            }
            vec![e]
        }
    };
    json!({ "input": text, "errors": errors, "warnings": [] })
}

/// The number `s` starts with, and the rest.
fn digits(s: &str) -> Option<(u64, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    Some((s[..end].parse().ok()?, &s[end..]))
}

/// The line and column (from 1) a parser's message names, as `at 3:7` or `line 3 column 7`.
fn position(message: &str) -> Option<(u64, u64)> {
    if let Some(i) = message.find("line ") {
        let (line, rest) = digits(&message[i + 5..])?;
        let rest = rest.trim_start_matches([',', ' ']);
        let rest = rest
            .strip_prefix("column ")
            .or_else(|| rest.strip_prefix("between columns "))?;
        let (column, _) = digits(rest)?;
        return Some((line, column));
    }
    message.match_indices(" at ").find_map(|(i, _)| {
        let (line, rest) = digits(&message[i + 4..])?;
        let (column, _) = digits(rest.strip_prefix(':')?)?;
        Some((line, column))
    })
}

/// The format a Fuseki `languageSyntax` names.
fn format_of(name: &str) -> Option<RdfFormat> {
    let n = name.to_ascii_lowercase().replace([' ', '_'], "-");
    Some(match n.as_str() {
        "turtle" | "ttl" | "text/turtle" => RdfFormat::Turtle,
        "n-triples" | "ntriples" | "nt" | "application/n-triples" => RdfFormat::NTriples,
        "trig" | "application/trig" => RdfFormat::TriG,
        "n-quads" | "nquads" | "nq" | "application/n-quads" => RdfFormat::NQuads,
        "rdf/xml" | "rdfxml" | "rdf" | "application/rdf+xml" => RdfFormat::RdfXml,
        "json-ld" | "jsonld" | "application/ld+json" => RdfFormat::JsonLd {
            profile: tripletrail::oxigraph::io::JsonLdProfileSet::empty(),
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(kind: &str, params: &[(&str, &str)]) -> Value {
        let params = Params(
            params
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        );
        let mut reply = route(kind, &params).unwrap();
        let mut body = Vec::new();
        std::io::Read::read_to_end(&mut reply.body, &mut body).unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[test]
    fn syntax_is_checked_as_fuseki_does() {
        let ok = ask("query", &[("query", "SELECT * { ?s ?p ?o }")]);
        assert_eq!(ok["errors"], json!([]));
        let bad = ask("query", &[("query", "SELECT * {\n ?s ?p }")]);
        let e = &bad["errors"][0];
        assert!(e["parse-error"].is_string(), "{bad}");
        assert_eq!(e["parse-error-line"], json!(2), "{bad}");
        assert!(ask("update", &[("update", "INSERT DATA { <a> }")])["errors"][0].is_object());
        assert_eq!(
            ask(
                "update",
                &[("update", "INSERT DATA { <urn:a> <urn:b> <urn:c> }")]
            )["errors"],
            json!([])
        );
        let data = ask(
            "data",
            &[
                ("data", "<urn:a> <urn:b> ."),
                ("languageSyntax", "N-Triples"),
            ],
        );
        assert_eq!(data["errors"][0]["parse-error-line"], json!(1), "{data}");
        let iris = ask(
            "iri",
            &[("iri", "https://example.org/a"), ("iri", "not an iri")],
        );
        assert_eq!(iris["iris"][0]["error"], json!([]));
        assert_eq!(iris["iris"][1]["error"].as_array().unwrap().len(), 1);
        assert_eq!(
            ask("langtag", &[("langtag", "nl-BE")])["valid"],
            json!(true)
        );
        assert_eq!(
            ask("langtag", &[("langtag", "nl BE")])["valid"],
            json!(false)
        );
    }

    #[test]
    fn positions_are_read_from_messages() {
        assert_eq!(position("error at 3:7: expected"), Some((3, 7)));
        assert_eq!(
            position("Parser error at line 2 between columns 17 and 20: x"),
            Some((2, 17))
        );
        assert_eq!(position("no place"), None);
    }
}
