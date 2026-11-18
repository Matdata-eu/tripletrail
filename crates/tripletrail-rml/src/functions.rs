//! The functions RML-FNML mappings can call: the GREL functions most mappings use
//! (`http://users.ugent.be/~bjdmeest/function/grel.ttl#`), IDLab's common ones
//! (`https://w3id.org/imec/idlab/function#`) and TripleTrail's geometry functions
//! (`http://w3id.org/tripletrail/fx#`). A function is known by its IRI; its parameters and
//! outputs by their local names, so the GREL parameters (`grel:valueParam`) also work for
//! IDLab functions, as RMLMapper accepts.

use crate::RmlError;
use oxigraph::model::vocab::xsd;
use oxigraph::model::{NamedNodeRef, Term};
use std::collections::HashMap;

pub(crate) const GREL: &str = "http://users.ugent.be/~bjdmeest/function/grel.ttl#";
pub(crate) const IDLAB: &str = "https://w3id.org/imec/idlab/function#";
pub(crate) const TTFX: &str = "http://w3id.org/tripletrail/fx#";

/// A value a function gives: its text and natural datatype.
pub(crate) type Output = (String, NamedNodeRef<'static>);

type Args<'a> = HashMap<&'a str, Vec<String>>;
type Body = fn(&Args<'_>) -> Result<Vec<Output>, RmlError>;

#[derive(Clone)]
pub(crate) struct Function {
    pub params: &'static [&'static str],
    pub outputs: &'static [&'static str],
    body: Body,
}

const STRING_OUT: &[&str] = &["stringOut", "output_string", "_stringOut"];
const BOOL_OUT: &[&str] = &["output_bool", "_boolOut", "boolOut"];
const NUMBER_OUT: &[&str] = &["output_number", "output_int", "_intOut"];
const ARRAY_OUT: &[&str] = &["output_array", "stringOut", "_stringOut"];

/// The function `iri` names, if it is one of those supported.
pub(crate) fn function(iri: &str) -> Option<Function> {
    let f = |params, outputs, body| {
        Some(Function {
            params,
            outputs,
            body,
        })
    };
    if let Some(name) = iri.strip_prefix(GREL) {
        return match name {
            "toUpperCase" | "toUppercase" => f(&["valueParam"], STRING_OUT, |a| {
                map1(a, "valueParam", |s| s.to_uppercase())
            }),
            "toLowerCase" | "toLowercase" => f(&["valueParam"], STRING_OUT, |a| {
                map1(a, "valueParam", |s| s.to_lowercase())
            }),
            "toTitlecase" => f(&["valueParam"], STRING_OUT, |a| {
                map1(a, "valueParam", title_case)
            }),
            "string_trim" | "trim" => f(&["valueParam"], STRING_OUT, |a| {
                map1(a, "valueParam", |s| s.trim().to_owned())
            }),
            "string_length" | "length" => f(&["valueParam"], NUMBER_OUT, |a| {
                Ok(one(a, "valueParam")
                    .map(|s| (s.chars().count().to_string(), xsd::INTEGER))
                    .into_iter()
                    .collect())
            }),
            "string_substring" => f(
                &["valueParam", "p_int_i_from", "p_int_i_opt_to"],
                STRING_OUT,
                substring,
            ),
            "string_replace" => f(
                &["valueParam", "param_find", "param_replace"],
                STRING_OUT,
                |a| {
                    let (Some(s), Some(find)) = (one(a, "valueParam"), one(a, "param_find")) else {
                        return Ok(Vec::new());
                    };
                    let with = one(a, "param_replace").unwrap_or_default();
                    Ok(vec![(s.replace(find, with), xsd::STRING)])
                },
            ),
            "string_split" => f(&["valueParam", "p_string_sep"], ARRAY_OUT, |a| {
                let (Some(s), Some(sep)) = (one(a, "valueParam"), one(a, "p_string_sep")) else {
                    return Ok(Vec::new());
                };
                Ok(s.split(sep).map(|p| (p.to_owned(), xsd::STRING)).collect())
            }),
            "array_join" => f(&["p_array_a", "p_string_sep"], STRING_OUT, |a| {
                let sep = one(a, "p_string_sep").unwrap_or_default();
                let values = a.get("p_array_a").cloned().unwrap_or_default();
                Ok(vec![(values.join(sep), xsd::STRING)])
            }),
            "escape" => f(&["valueParam", "modeParam"], STRING_OUT, escape),
            "boolean_not" => f(&["bool_b"], BOOL_OUT, |a| {
                Ok(one(a, "bool_b")
                    .map(|b| (bool_text(!truthy(b)), xsd::BOOLEAN))
                    .into_iter()
                    .collect())
            }),
            "controls_if" => f(&["bool_b", "any_true", "any_false"], STRING_OUT, |a| {
                let pick = if one(a, "bool_b").is_some_and(truthy) {
                    "any_true"
                } else {
                    "any_false"
                };
                Ok(one(a, pick)
                    .map(|v| (v.to_owned(), xsd::STRING))
                    .into_iter()
                    .collect())
            }),
            "string_toString" | "toString" => f(&["valueParam", "any_e"], STRING_OUT, |a| {
                Ok(one(a, "valueParam")
                    .or_else(|| one(a, "any_e"))
                    .map(|v| (v.to_owned(), xsd::STRING))
                    .into_iter()
                    .collect())
            }),
            _ => None,
        }
        .or_else(|| match name {
            "string_contains" => f(&["valueParam", "string_sub"], BOOL_OUT, |a| {
                test2(a, "string_sub", |s, t| s.contains(t))
            }),
            "string_startsWith" => f(&["valueParam", "string_sub"], BOOL_OUT, |a| {
                test2(a, "string_sub", |s, t| s.starts_with(t))
            }),
            "string_endsWith" => f(&["valueParam", "string_sub"], BOOL_OUT, |a| {
                test2(a, "string_sub", |s, t| s.ends_with(t))
            }),
            _ => None,
        });
    }
    if let Some(name) = iri.strip_prefix(IDLAB) {
        return match name {
            "toUpperCaseURL" => f(&["_str", "str"], STRING_OUT, |a| {
                Ok(one(a, "_str")
                    .or_else(|| one(a, "str"))
                    .map(|s| {
                        let upper = s.to_uppercase();
                        let url = if upper.starts_with("HTTP") {
                            upper
                        } else {
                            format!("HTTP://{upper}")
                        };
                        (url, xsd::STRING)
                    })
                    .into_iter()
                    .collect())
            }),
            "equal" => f(&["valueParam", "valueParam2"], BOOL_OUT, |a| {
                compare(a, |x, y| x == y)
            }),
            "notEqual" => f(&["valueParam", "valueParam2"], BOOL_OUT, |a| {
                compare(a, |x, y| x != y)
            }),
            "stringContainsOtherString" => f(&["_str", "_otherStr", "_delimiter"], BOOL_OUT, |a| {
                let (Some(s), Some(other)) = (one(a, "_str"), one(a, "_otherStr")) else {
                    return Ok(Vec::new());
                };
                let found = match one(a, "_delimiter") {
                    Some(d) if !d.is_empty() => s.split(d).any(|p| p == other),
                    _ => s.contains(other),
                };
                Ok(vec![(bool_text(found), xsd::BOOLEAN)])
            }),
            "isNull" => f(&["_str", "str"], BOOL_OUT, |a| {
                let null = one(a, "_str").or_else(|| one(a, "str")).is_none();
                Ok(vec![(bool_text(null), xsd::BOOLEAN)])
            }),
            "isNotNull" => f(&["_str", "str"], BOOL_OUT, |a| {
                let set = one(a, "_str").or_else(|| one(a, "str")).is_some();
                Ok(vec![(bool_text(set), xsd::BOOLEAN)])
            }),
            "trueCondition" => f(&["strBoolean", "str"], STRING_OUT, |a| {
                Ok(match (one(a, "strBoolean"), one(a, "str")) {
                    (Some(b), Some(s)) if truthy(b) => vec![(s.to_owned(), xsd::STRING)],
                    _ => Vec::new(),
                })
            }),
            "concat" => f(&["_str", "_otherStr", "_delimiter"], STRING_OUT, |a| {
                let (Some(s), Some(other)) = (one(a, "_str"), one(a, "_otherStr")) else {
                    return Ok(Vec::new());
                };
                let d = one(a, "_delimiter").unwrap_or_default();
                Ok(vec![(format!("{s}{d}{other}"), xsd::STRING)])
            }),
            "alwaysReturnsABC" => f(&[], STRING_OUT, |_| Ok(vec![("ABC".into(), xsd::STRING)])),
            _ => None,
        };
    }
    if let Some(name) = iri.strip_prefix(TTFX) {
        return match name {
            // A WKT point from coordinates, with an optional CRS (`EPSG:25833` or an IRI).
            "point" => f(&["x", "y", "z", "crs"], &["wkt"], |a| {
                let (Some(x), Some(y)) = (one(a, "x"), one(a, "y")) else {
                    return Ok(Vec::new());
                };
                let crs = crs_prefix(one(a, "crs"))?;
                let wkt = match one(a, "z") {
                    Some(z) => format!("{crs}POINT Z({} {} {})", x.trim(), y.trim(), z.trim()),
                    None => format!("{crs}POINT({} {})", x.trim(), y.trim()),
                };
                Ok(vec![(wkt, WKT_LITERAL)])
            }),
            // A WKT literal from WKT text, with an optional CRS.
            "wkt" => f(&["wkt", "crs"], &["wkt"], |a| {
                let Some(w) = one(a, "wkt").map(str::trim).filter(|w| !w.is_empty()) else {
                    return Ok(Vec::new());
                };
                let crs = if w.starts_with('<') {
                    String::new()
                } else {
                    crs_prefix(one(a, "crs"))?
                };
                Ok(vec![(format!("{crs}{w}"), WKT_LITERAL)])
            }),
            _ => None,
        };
    }
    None
}

const WKT_LITERAL: NamedNodeRef<'static> =
    NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#wktLiteral");

impl std::fmt::Debug for Function {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Function")
            .field("params", &self.params)
            .field("outputs", &self.outputs)
            .finish()
    }
}

impl Function {
    pub(crate) fn call(&self, args: &Args<'_>) -> Result<Vec<Output>, RmlError> {
        (self.body)(args)
    }
}

/// The text of a term, as functions read it.
pub(crate) fn text(t: &Term) -> Option<String> {
    match t {
        Term::NamedNode(n) => Some(n.as_str().to_owned()),
        Term::Literal(l) => Some(l.value().to_owned()),
        _ => None,
    }
}

fn one<'a>(a: &'a Args<'_>, name: &str) -> Option<&'a str> {
    a.get(name).and_then(|v| v.first()).map(String::as_str)
}

fn map1(a: &Args<'_>, name: &str, f: impl Fn(&str) -> String) -> Result<Vec<Output>, RmlError> {
    Ok(one(a, name)
        .map(|s| (f(s), xsd::STRING))
        .into_iter()
        .collect())
}

fn test2(
    a: &Args<'_>,
    other: &str,
    f: impl Fn(&str, &str) -> bool,
) -> Result<Vec<Output>, RmlError> {
    Ok(match (one(a, "valueParam"), one(a, other)) {
        (Some(s), Some(t)) => vec![(bool_text(f(s, t)), xsd::BOOLEAN)],
        _ => Vec::new(),
    })
}

fn compare(a: &Args<'_>, f: impl Fn(&str, &str) -> bool) -> Result<Vec<Output>, RmlError> {
    let x = one(a, "valueParam");
    let y = one(a, "valueParam2");
    Ok(match (x, y) {
        (Some(x), Some(y)) => vec![(bool_text(f(x, y)), xsd::BOOLEAN)],
        _ => Vec::new(),
    })
}

fn bool_text(b: bool) -> String {
    if b { "true" } else { "false" }.to_owned()
}

/// `true`, `1` (any case) are true.
pub(crate) fn truthy(s: &str) -> bool {
    s.trim().eq_ignore_ascii_case("true") || s.trim() == "1"
}

fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut start = true;
    for c in s.chars() {
        if c.is_whitespace() {
            start = true;
            out.push(c);
        } else if start {
            out.extend(c.to_uppercase());
            start = false;
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// GREL `substring(s, from, to)`: characters from `from` (from 0) up to `to` (exclusive, the
/// end when absent); negative positions count from the end. No value when `from` is past
/// the end.
fn substring(a: &Args<'_>) -> Result<Vec<Output>, RmlError> {
    let Some(s) = one(a, "valueParam") else {
        return Ok(Vec::new());
    };
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len() as i64;
    let int = |name: &str| -> Result<Option<i64>, RmlError> {
        one(a, name)
            .map(|v| {
                v.trim().parse::<i64>().map_err(|_| {
                    RmlError::Data(format!("grel:{name} must be an integer, not {v:?}"))
                })
            })
            .transpose()
    };
    let at = |i: i64| if i < 0 { (len + i).max(0) } else { i };
    let from = at(int("p_int_i_from")?.unwrap_or(0));
    let to = int("p_int_i_opt_to")?.map_or(len, at).min(len);
    if from > len {
        return Ok(Vec::new());
    }
    let text: String = if from < to {
        chars[from as usize..to as usize].iter().collect()
    } else {
        String::new()
    };
    Ok(vec![(text, xsd::STRING)])
}

/// GREL `escape(s, mode)` for `html`, `xml`, `csv`, `url` and `javascript`.
fn escape(a: &Args<'_>) -> Result<Vec<Output>, RmlError> {
    let (Some(s), Some(mode)) = (one(a, "valueParam"), one(a, "modeParam")) else {
        return Ok(Vec::new());
    };
    let mut out = String::with_capacity(s.len());
    match mode.to_ascii_lowercase().as_str() {
        "html" | "xml" => {
            for c in s.chars() {
                match c {
                    '&' => out.push_str("&amp;"),
                    '<' => out.push_str("&lt;"),
                    '>' => out.push_str("&gt;"),
                    '"' => out.push_str("&quot;"),
                    '\'' if mode.eq_ignore_ascii_case("xml") => out.push_str("&apos;"),
                    c => out.push(c),
                }
            }
        }
        "csv" => {
            if s.contains([',', '"', '\n', '\r']) {
                out.push('"');
                out.push_str(&s.replace('"', "\"\""));
                out.push('"');
            } else {
                out.push_str(s);
            }
        }
        "url" => {
            for b in s.bytes() {
                if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'*') {
                    out.push(b as char);
                } else if b == b' ' {
                    out.push('+');
                } else {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
        }
        "javascript" => {
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\'' => out.push_str("\\'"),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c => out.push(c),
                }
            }
        }
        m => {
            return Err(RmlError::Data(format!(
                "grel:escape mode {m:?} (html, xml, csv, url or javascript)"
            )));
        }
    }
    Ok(vec![(out, xsd::STRING)])
}

fn crs_prefix(crs: Option<&str>) -> Result<String, RmlError> {
    let Some(crs) = crs.map(str::trim).filter(|c| !c.is_empty()) else {
        return Ok(String::new());
    };
    if let Some(code) = crs.strip_prefix("EPSG:")
        && !code.is_empty()
        && code.bytes().all(|b| b.is_ascii_digit())
    {
        return Ok(format!("<http://www.opengis.net/def/crs/EPSG/0/{code}> "));
    }
    if crs.contains("://") && oxiri::Iri::parse(crs).is_ok() {
        return Ok(format!("<{crs}> "));
    }
    Err(RmlError::Data(format!(
        "CRS {crs:?}: give EPSG:<code> or a CRS IRI"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(iri: &str, args: &[(&'static str, &str)]) -> Vec<String> {
        let mut a: Args<'_> = HashMap::new();
        for (k, v) in args {
            a.entry(*k).or_default().push((*v).to_owned());
        }
        function(iri)
            .unwrap()
            .call(&a)
            .unwrap()
            .into_iter()
            .map(|(s, _)| s)
            .collect()
    }

    #[test]
    fn grel() {
        let g = |n: &str| format!("{GREL}{n}");
        assert_eq!(
            call(&g("toUpperCase"), &[("valueParam", "Venus")]),
            ["VENUS"]
        );
        assert_eq!(
            call(&g("toTitlecase"), &[("valueParam", "oslo SENTRAL")]),
            ["Oslo Sentral"]
        );
        assert_eq!(
            call(
                &g("string_substring"),
                &[
                    ("valueParam", "Venus"),
                    ("p_int_i_from", "1"),
                    ("p_int_i_opt_to", "-1")
                ]
            ),
            ["enu"]
        );
        assert_eq!(
            call(
                &g("string_substring"),
                &[("valueParam", "Venus"), ("p_int_i_from", "5")]
            ),
            [""]
        );
        assert!(
            call(
                &g("string_substring"),
                &[("valueParam", "Venus"), ("p_int_i_from", "6")]
            )
            .is_empty()
        );
        assert_eq!(
            call(
                &g("escape"),
                &[("valueParam", "A&B"), ("modeParam", "html")]
            ),
            ["A&amp;B"]
        );
        assert_eq!(
            call(
                &g("string_split"),
                &[("valueParam", "a;b"), ("p_string_sep", ";")]
            ),
            ["a", "b"]
        );
        assert_eq!(
            call(
                &g("string_contains"),
                &[("valueParam", "abc"), ("string_sub", "b")]
            ),
            ["true"]
        );
        assert!(function(&g("nope")).is_none());
    }

    #[test]
    fn idlab_and_geometry() {
        assert_eq!(
            call(
                &format!("{IDLAB}toUpperCaseURL"),
                &[("_str", "example.com")]
            ),
            ["HTTP://EXAMPLE.COM"]
        );
        assert_eq!(
            call(
                &format!("{IDLAB}equal"),
                &[("valueParam", "A"), ("valueParam2", "A")]
            ),
            ["true"]
        );
        assert_eq!(
            call(
                &format!("{TTFX}point"),
                &[("x", "600000"), ("y", "6640000"), ("crs", "EPSG:25833")]
            ),
            ["<http://www.opengis.net/def/crs/EPSG/0/25833> POINT(600000 6640000)"]
        );
    }
}
