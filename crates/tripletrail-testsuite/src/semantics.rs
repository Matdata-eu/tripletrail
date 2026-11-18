//! The RDF 1.1 semantics tests (`rdf/rdf11/rdf-mt`): does a graph entail another, or is it
//! inconsistent, under simple, RDF or RDFS entailment with a set of recognized datatypes.
//!
//! TripleTrail loads the action and, under RDFS, closes it with its RDFS rules (as `tripletrail
//! entail` does). The runner then checks the closure the way the RDF 1.1 Semantics define
//! entailment for a closed graph: the result graph, with its blank nodes as variables, maps
//! into the closure, where literals of a recognized datatype stand for their value. A graph
//! is inconsistent when it holds an ill-typed literal of a recognized datatype, or, under
//! RDFS, a literal in the range of a property whose value is not of the range's datatype.

use crate::eval::{engine, entail, load};
use crate::manifest::{Files, Kind, Test};
use oxsdatatypes::Decimal;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::str::FromStr;
use tripletrail::oxigraph::model::{GraphName, Literal, Term, Triple};
use tripletrail::oxigraph::sparql::{CancellationToken, QueryResults};

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const RDFS: &str = "http://www.w3.org/2000/01/rdf-schema#";

pub fn check(
    files: &Files,
    test: &Test,
    action: &str,
    token: CancellationToken,
) -> Result<(), String> {
    let positive = test.kind == Kind::Entailment(true);
    let regime = test.regime.as_deref().unwrap_or("simple");
    let mut recognized: BTreeSet<String> = test.recognized.iter().cloned().collect();
    if regime != "simple" {
        // RDF interpretations recognize these two in every graph.
        recognized.insert(format!("{XSD}string"));
        recognized.insert(format!("{RDF}langString"));
    }
    let closure = closure(files, action, regime, token)?;
    let inconsistent = inconsistency(&closure, &recognized, regime == "RDFS");
    let Some(result) = &test.result else {
        // `mf:result false`: the action is inconsistent.
        return match (positive, inconsistent) {
            (true, Some(_)) | (false, None) => Ok(()),
            (true, None) => Err("found consistent, but the action is inconsistent".into()),
            (false, Some(why)) => Err(format!("found inconsistent ({why}), but it is not")),
        };
    };
    let expected = files.graph(result)?;
    // An inconsistent graph entails every graph.
    let entailed = inconsistent.is_some() || entails(&closure, &expected, &recognized);
    match (positive, entailed) {
        (true, true) | (false, false) => Ok(()),
        (true, false) => Err(format!("{result} is not entailed, but it should be")),
        (false, true) => Err(format!(
            "{result} is entailed{}, but it should not be",
            inconsistent.map_or_else(String::new, |why| format!(" (inconsistent: {why})"))
        )),
    }
}

/// The action's triples, closed under TripleTrail's RDFS rules when the regime is RDFS.
fn closure(
    files: &Files,
    action: &str,
    regime: &str,
    token: CancellationToken,
) -> Result<Vec<Triple>, String> {
    let engine = engine(token)?;
    load(&engine, files, action, GraphName::DefaultGraph)?;
    match regime {
        "RDFS" => entail(&engine, "RDFS")?,
        "simple" | "RDF" => {}
        other => return Err(format!("no entailment regime {other}")),
    }
    let results = engine
        .query("CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }")
        .map_err(|e| format!("reading the closure failed: {e}"))?;
    let QueryResults::Graph(triples) = results else {
        return Err("CONSTRUCT gave no graph".into());
    };
    triples
        .map(|t| t.map_err(|e| format!("reading the closure failed: {e}")))
        .collect()
}

/// What a node of a graph stands for, for the entailment check: a literal of a recognized
/// datatype by its value, and any other term as itself (a language tag in lower case).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    Term(Term),
    Number(Decimal),
    Float(u32),
    Double(u64),
}

/// Why the closure is inconsistent, if it is.
fn inconsistency(closure: &[Triple], recognized: &BTreeSet<String>, rdfs: bool) -> Option<String> {
    for triple in closure {
        if let Term::Literal(literal) = &triple.object
            && recognized.contains(literal.datatype().as_str())
            && value(literal).is_none()
        {
            return Some(format!("ill-typed literal {literal}"));
        }
    }
    if !rdfs {
        return None;
    }
    // A literal is in the classes of the ranges of its property, and their super classes
    // (the closure holds `rdfs:subClassOf` closed, and the triples of super properties).
    let (sub_class_of, range) = (format!("{RDFS}subClassOf"), format!("{RDFS}range"));
    let objects = |subject: &Term, predicate: &str| -> Vec<Term> {
        closure
            .iter()
            .filter(|t| {
                t.predicate.as_str() == predicate && &Term::from(t.subject.clone()) == subject
            })
            .map(|t| t.object.clone())
            .collect()
    };
    for triple in closure {
        let Term::Literal(literal) = &triple.object else {
            continue;
        };
        if !recognized.contains(literal.datatype().as_str()) {
            continue;
        }
        let mut classes = objects(&triple.predicate.clone().into(), &range);
        for class in classes.clone() {
            classes.extend(objects(&class, &sub_class_of));
        }
        for class in classes {
            if let Term::NamedNode(class) = class
                && recognized.contains(class.as_str())
                && member(literal, class.as_str()) == Some(false)
            {
                return Some(format!(
                    "{literal} is in the range {class} of {}",
                    triple.predicate
                ));
            }
        }
    }
    None
}

/// Whether a literal's value is in the value space of a datatype, when both are known.
fn member(literal: &Literal, datatype: &str) -> Option<bool> {
    let own = literal.datatype().as_str().to_owned();
    let value = value(literal)?;
    let numeric = |dt: &str| {
        [
            format!("{XSD}integer"),
            format!("{XSD}decimal"),
            format!("{XSD}int"),
        ]
        .iter()
        .any(|d| d == dt)
    };
    let integral = |v: &Key| matches!(v, Key::Number(n) if n.checked_floor() == Some(*n));
    Some(
        match datatype
            .strip_prefix(XSD)
            .or_else(|| datatype.strip_prefix(RDF).map(|_| datatype))
        {
            Some("decimal") => numeric(&own),
            Some("integer") => numeric(&own) && integral(&value),
            Some("int") => {
                numeric(&own)
                    && integral(&value)
                    && matches!(&value, Key::Number(n) if in_int_range(*n))
            }
            Some("float") => matches!(value, Key::Float(_)),
            Some("double") => matches!(value, Key::Double(_)),
            Some("string") => own == format!("{XSD}string"),
            _ if datatype == format!("{RDF}langString") => own == format!("{RDF}langString"),
            _ if datatype == format!("{RDF}XMLLiteral") => own == format!("{RDF}XMLLiteral"),
            _ => return None,
        },
    )
}

/// The value of a literal of a datatype the runner knows, `None` when it is ill-typed. A
/// literal of another datatype stands for itself.
fn value(literal: &Literal) -> Option<Key> {
    let lexical = literal.value();
    let datatype = literal.datatype().as_str();
    let Some(local) = datatype.strip_prefix(XSD) else {
        if datatype == format!("{RDF}XMLLiteral") {
            return well_formed_xml(lexical).then(|| key(literal));
        }
        return Some(key(literal));
    };
    match local {
        "integer" | "int" => {
            if !is_integer(lexical) {
                return None;
            }
            let number = Decimal::from_str(lexical).ok()?;
            if local == "int" && !in_int_range(number) {
                return None;
            }
            Some(Key::Number(number))
        }
        "decimal" => is_decimal(lexical)
            .then(|| {
                Decimal::from_str(&decimal_form(lexical))
                    .ok()
                    .map(Key::Number)
            })
            .flatten(),
        "float" => float(lexical).map(|f| Key::Float((f as f32).to_bits())),
        "double" => float(lexical).map(|f| Key::Double(f.to_bits())),
        _ => Some(key(literal)),
    }
}

fn in_int_range(n: Decimal) -> bool {
    Decimal::from(i64::from(i32::MIN)) <= n && n <= Decimal::from(i64::from(i32::MAX))
}

/// A term as a key: language tags in lower case.
fn key(literal: &Literal) -> Key {
    Key::Term(match literal.language() {
        Some(tag) => Literal::new_language_tagged_literal_unchecked(
            literal.value(),
            tag.to_ascii_lowercase(),
        )
        .into(),
        None => literal.clone().into(),
    })
}

fn term_key(term: &Term, recognized: &BTreeSet<String>) -> Key {
    match term {
        Term::Literal(literal) if recognized.contains(literal.datatype().as_str()) => {
            value(literal).unwrap_or_else(|| key(literal))
        }
        Term::Literal(literal) => key(literal),
        other => Key::Term(other.clone()),
    }
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn unsigned(s: &str) -> &str {
    s.strip_prefix(['+', '-']).unwrap_or(s)
}

/// `[+-]?[0-9]+`
fn is_integer(s: &str) -> bool {
    digits(unsigned(s))
}

/// `[+-]?([0-9]+(\.[0-9]*)?|\.[0-9]+)`
fn is_decimal(s: &str) -> bool {
    match unsigned(s).split_once('.') {
        None => digits(unsigned(s)),
        Some((whole, fraction)) => {
            (digits(whole) && (fraction.is_empty() || digits(fraction)))
                || (whole.is_empty() && digits(fraction))
        }
    }
}

/// A decimal's lexical form as `Decimal` reads it: digits on both sides of the point.
fn decimal_form(s: &str) -> String {
    let (sign, rest) = s.split_at(s.len() - unsigned(s).len());
    match rest.split_once('.') {
        Some((whole, fraction)) => format!(
            "{sign}{}.{}",
            if whole.is_empty() { "0" } else { whole },
            if fraction.is_empty() { "0" } else { fraction }
        ),
        None => s.to_owned(),
    }
}

/// An `xsd:float` or `xsd:double` lexical form, read as a double (a float rounds it again).
fn float(s: &str) -> Option<f64> {
    match s {
        "INF" | "+INF" => return Some(f64::INFINITY),
        "-INF" => return Some(f64::NEG_INFINITY),
        "NaN" => return Some(f64::NAN),
        _ => {}
    }
    let (mantissa, exponent) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (s, None),
    };
    if !is_decimal(mantissa) || exponent.is_some_and(|e| !is_integer(e)) {
        return None;
    }
    f64::from_str(s).ok()
}

/// Whether an `rdf:XMLLiteral`'s lexical form is balanced XML content: every tag closed,
/// in order.
fn well_formed_xml(s: &str) -> bool {
    let mut open: Vec<&str> = Vec::new();
    let mut rest = s;
    while let Some(at) = rest.find('<') {
        let after = &rest[at + 1..];
        let Some(end) = after.find('>') else {
            return false;
        };
        let tag = &after[..end];
        rest = &after[end + 1..];
        if tag.starts_with('!') || tag.starts_with('?') {
            continue;
        }
        if let Some(name) = tag.strip_prefix('/') {
            if open.pop() != Some(name.trim()) {
                return false;
            }
        } else if !tag.ends_with('/') {
            let name = tag.split_whitespace().next().unwrap_or("");
            if name.is_empty() {
                return false;
            }
            open.push(name);
        }
    }
    open.is_empty() && !rest.contains('>')
}

/// Whether the result graph, with its blank nodes as variables, maps into the closure.
fn entails(
    closure: &[Triple],
    expected: &tripletrail::oxigraph::model::Graph,
    recognized: &BTreeSet<String>,
) -> bool {
    let facts: HashSet<[Key; 3]> = closure
        .iter()
        .map(|t| {
            [
                Key::Term(t.subject.clone().into()),
                Key::Term(t.predicate.clone().into()),
                term_key(&t.object, recognized),
            ]
        })
        .collect();
    let facts: Vec<[Key; 3]> = facts.into_iter().collect();
    matches(
        &facts,
        &pattern_rows(expected, recognized),
        0,
        &mut HashMap::new(),
    )
}

/// A node of the pattern: a variable (a blank node of the result graph) or a key.
#[derive(Clone, Debug)]
enum Node {
    Variable(String),
    Key(Key),
}

fn pattern_rows(
    expected: &tripletrail::oxigraph::model::Graph,
    recognized: &BTreeSet<String>,
) -> Vec<[Node; 3]> {
    let node = |term: Term| match term {
        Term::BlankNode(b) => Node::Variable(b.as_str().to_owned()),
        other => Node::Key(term_key(&other, recognized)),
    };
    expected
        .iter()
        .map(|t| {
            [
                node(t.subject.into_owned().into()),
                node(t.predicate.into_owned().into()),
                node(t.object.into_owned()),
            ]
        })
        .collect()
}

fn matches(
    facts: &[[Key; 3]],
    pattern: &[[Node; 3]],
    at: usize,
    bound: &mut HashMap<String, Key>,
) -> bool {
    let Some(row) = pattern.get(at) else {
        return true;
    };
    'facts: for fact in facts {
        let mut added = Vec::new();
        for (node, key) in row.iter().zip(fact) {
            match node {
                Node::Key(k) if k == key => {}
                Node::Key(_) => {
                    undo(bound, &added);
                    continue 'facts;
                }
                Node::Variable(v) => match bound.get(v) {
                    Some(k) if k == key => {}
                    Some(_) => {
                        undo(bound, &added);
                        continue 'facts;
                    }
                    None => {
                        bound.insert(v.clone(), key.clone());
                        added.push(v.clone());
                    }
                },
            }
        }
        if matches(facts, pattern, at + 1, bound) {
            return true;
        }
        undo(bound, &added);
    }
    false
}

fn undo(bound: &mut HashMap<String, Key>, added: &[String]) {
    for v in added {
        bound.remove(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tripletrail::oxigraph::model::NamedNode;

    fn typed(lexical: &str, local: &str) -> Literal {
        Literal::new_typed_literal(lexical, NamedNode::new_unchecked(format!("{XSD}{local}")))
    }

    #[test]
    fn values() {
        assert_eq!(
            value(&typed("010", "integer")),
            value(&typed("10.0", "decimal"))
        );
        assert_eq!(value(&typed("flargh", "integer")), None);
        assert_eq!(value(&typed(" 3 ", "int")), None);
        assert_eq!(value(&typed("3000000000", "int")), None);
        assert_eq!(
            value(&typed("1e400", "float")),
            value(&typed("1e401", "float"))
        );
        assert_ne!(
            value(&typed("0.0", "double")),
            value(&typed("-0.0", "double"))
        );
        assert_eq!(
            value(&typed("16777206.5", "float")),
            value(&typed("16777205.5", "float"))
        );
        assert_eq!(
            value(&typed(".5", "decimal")),
            value(&typed("0.50", "decimal"))
        );
        assert!(!well_formed_xml("<"));
        assert!(well_formed_xml("<a x='1'>b<c/></a>"));
    }
}
