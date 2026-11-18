//! The Facade-X functions (`fx:entity`, `fx:cardinal`, ...), for use anywhere in a query.

use crate::options::slot_index;
use crate::tree::escape_iri;
use crate::{FX, RDF};
use oxigraph::model::{BlankNode, Literal, NamedNode, Term};
use std::sync::Arc;

/// A custom function as the SPARQL evaluators take it.
pub type Function = Arc<dyn Fn(&[Term]) -> Option<Term> + Send + Sync>;

/// Every function, by IRI.
pub fn functions() -> Vec<(NamedNode, Function)> {
    let list: Vec<(&str, Function)> = vec![
        ("entity", Arc::new(entity)),
        (
            "cardinal",
            Arc::new(|a: &[Term]| {
                one(a)
                    .and_then(slot)
                    .map(|n| Literal::from(n as i64).into())
            }),
        ),
        (
            "isContainerMembershipProperty",
            Arc::new(|a: &[Term]| one(a).map(|t| Literal::from(slot(t).is_some()).into())),
        ),
        ("before", Arc::new(|a: &[Term]| compare(a, |x, y| x < y))),
        ("after", Arc::new(|a: &[Term]| compare(a, |x, y| x > y))),
        (
            "previous",
            Arc::new(|a: &[Term]| {
                one(a)
                    .and_then(slot)
                    .and_then(|n| slot_iri(n.checked_sub(1)?))
            }),
        ),
        (
            "next",
            Arc::new(|a: &[Term]| {
                one(a)
                    .and_then(slot)
                    .and_then(|n| slot_iri(n.checked_add(1)?))
            }),
        ),
        ("forward", Arc::new(|a: &[Term]| step(a, 1))),
        ("backward", Arc::new(|a: &[Term]| step(a, -1))),
        ("literal", Arc::new(literal)),
        (
            "bnode",
            Arc::new(|a: &[Term]| one(a).map(|t| bnode(&text(t)))),
        ),
        (
            "String.startsWith",
            Arc::new(|a: &[Term]| strings2(a).map(|(x, y)| boolean(x.starts_with(&y)))),
        ),
        (
            "String.endsWith",
            Arc::new(|a: &[Term]| strings2(a).map(|(x, y)| boolean(x.ends_with(&y)))),
        ),
        (
            "String.indexOf",
            Arc::new(|a: &[Term]| {
                strings2(a).map(|(x, y)| Literal::from(index(&x, x.find(&y))).into())
            }),
        ),
        (
            "String.lastIndexOf",
            Arc::new(|a: &[Term]| {
                strings2(a).map(|(x, y)| Literal::from(index(&x, x.rfind(&y))).into())
            }),
        ),
        ("String.substring", Arc::new(substring)),
        (
            "String.toLowerCase",
            Arc::new(|a: &[Term]| string1(a, |s| s.to_lowercase())),
        ),
        (
            "String.toUpperCase",
            Arc::new(|a: &[Term]| string1(a, |s| s.to_uppercase())),
        ),
        // Java's trim removes characters up to U+0020; strip* remove Unicode white space.
        (
            "String.trim",
            Arc::new(|a: &[Term]| string1(a, |s| s.trim_matches(|c| c <= ' ').to_owned())),
        ),
        (
            "String.stripLeading",
            Arc::new(|a: &[Term]| string1(a, |s| s.trim_start().to_owned())),
        ),
        (
            "String.stripTrailing",
            Arc::new(|a: &[Term]| string1(a, |s| s.trim_end().to_owned())),
        ),
        ("String.replace", Arc::new(replace)),
        (
            "String.removeTags",
            Arc::new(|a: &[Term]| string1(a, remove_tags)),
        ),
        (
            "URLEncoder.encode",
            Arc::new(|a: &[Term]| first_string(a).map(|s| plain(url_encode(&s)))),
        ),
        (
            "URLDecoder.decode",
            Arc::new(|a: &[Term]| first_string(a).and_then(|s| url_decode(&s)).map(plain)),
        ),
    ];
    list.into_iter()
        .map(|(name, f)| (NamedNode::new_unchecked(format!("{FX}{name}")), f))
        .collect()
}

fn one(args: &[Term]) -> Option<&Term> {
    match args {
        [a] => Some(a),
        _ => None,
    }
}

fn slot(t: &Term) -> Option<u64> {
    match t {
        Term::NamedNode(n) => slot_index(n.as_str()),
        _ => None,
    }
}

fn slot_iri(n: u64) -> Option<Term> {
    (n >= 1).then(|| NamedNode::new_unchecked(format!("{RDF}_{n}")).into())
}

fn compare(args: &[Term], f: fn(u64, u64) -> bool) -> Option<Term> {
    let [a, b] = args else { return None };
    Some(boolean(match (slot(a), slot(b)) {
        (Some(a), Some(b)) => f(a, b),
        _ => false,
    }))
}

/// `fx:forward(rdf:_n, k)` and `fx:backward(rdf:_n, k)`.
fn step(args: &[Term], sign: i64) -> Option<Term> {
    let [a, k] = args else { return None };
    let n = i64::try_from(slot(a)?).ok()?;
    let Term::Literal(k) = k else { return None };
    let k: i64 = k.value().trim().parse().ok()?;
    slot_iri(u64::try_from(n.checked_add(sign.checked_mul(k)?)?).ok()?)
}

/// The text of a term for concatenation: IRIs and literals by value, slots by number.
fn text(t: &Term) -> String {
    match t {
        Term::NamedNode(n) => match slot_index(n.as_str()) {
            Some(i) => i.to_string(),
            None => n.as_str().to_owned(),
        },
        Term::BlankNode(b) => b.as_str().to_owned(),
        Term::Literal(l) => l.value().to_owned(),
        Term::Triple(t) => t.to_string(),
    }
}

/// `fx:entity(?a, ...)`: the IRI made of the arguments' text. Characters an IRI may not
/// contain are percent-encoded, so a value with a space still gives an IRI.
fn entity(args: &[Term]) -> Option<Term> {
    if args.is_empty() {
        return None;
    }
    let mut iri = String::new();
    for a in args {
        escape_iri(&text(a), &mut iri);
    }
    NamedNode::new(iri).ok().map(Into::into)
}

/// `fx:literal(?value, ?type_or_language)`.
fn literal(args: &[Term]) -> Option<Term> {
    let [value, kind] = args else { return None };
    let value = text(value);
    Some(
        match kind {
            Term::NamedNode(datatype) => Literal::new_typed_literal(value, datatype.clone()),
            Term::Literal(lang) => {
                Literal::new_language_tagged_literal(value, lang.value()).ok()?
            }
            _ => return None,
        }
        .into(),
    )
}

/// A blank node labelled by `label`, or by its hex bytes when it is not a valid label.
fn bnode(label: &str) -> Term {
    BlankNode::new(label)
        .unwrap_or_else(|_| {
            let hex: String = label.bytes().map(|b| format!("{b:02x}")).collect();
            BlankNode::new_unchecked(format!("x{hex}"))
        })
        .into()
}

fn boolean(b: bool) -> Term {
    Literal::from(b).into()
}

fn plain(s: String) -> Term {
    Literal::new_simple_literal(s).into()
}

fn string_arg(t: &Term) -> Option<String> {
    match t {
        Term::Literal(l) => Some(l.value().to_owned()),
        Term::NamedNode(n) => Some(n.as_str().to_owned()),
        _ => None,
    }
}

fn first_string(args: &[Term]) -> Option<String> {
    string_arg(args.first()?)
}

fn string1(args: &[Term], f: impl Fn(&str) -> String) -> Option<Term> {
    one(args).and_then(string_arg).map(|s| plain(f(&s)))
}

fn strings2(args: &[Term]) -> Option<(String, String)> {
    let [a, b] = args else { return None };
    Some((string_arg(a)?, string_arg(b)?))
}

/// The index in characters of a byte offset, or -1.
fn index(s: &str, byte: Option<usize>) -> i64 {
    byte.map_or(-1, |b| s[..b].chars().count() as i64)
}

fn int_arg(t: &Term) -> Option<usize> {
    let Term::Literal(l) = t else { return None };
    l.value().trim().parse().ok()
}

/// `fx:String.substring(?s, begin[, end])`, by character.
fn substring(args: &[Term]) -> Option<Term> {
    let (s, begin, end) = match args {
        [s, b] => (string_arg(s)?, int_arg(b)?, None),
        [s, b, e] => (string_arg(s)?, int_arg(b)?, Some(int_arg(e)?)),
        _ => return None,
    };
    let chars: Vec<char> = s.chars().collect();
    let end = end.unwrap_or(chars.len());
    (begin <= end && end <= chars.len()).then(|| plain(chars[begin..end].iter().collect()))
}

fn replace(args: &[Term]) -> Option<Term> {
    let [s, from, to] = args else { return None };
    Some(plain(
        string_arg(s)?.replace(&string_arg(from)?, &string_arg(to)?),
    ))
}

fn remove_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// `application/x-www-form-urlencoded`, as Java's `URLEncoder` with UTF-8.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'-' | b'*' | b'_' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn url_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = s.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: &[Term]) -> Option<Term> {
        let f = functions()
            .into_iter()
            .find(|(n, _)| n.as_str() == format!("{FX}{name}"))
            .unwrap()
            .1;
        f(args)
    }

    fn iri(s: &str) -> Term {
        NamedNode::new_unchecked(s).into()
    }

    fn lit(s: &str) -> Term {
        Literal::new_simple_literal(s).into()
    }

    #[test]
    fn entity_concatenates() {
        let r = call(
            "entity",
            &[
                iri("http://ex.org/"),
                lit("dummy-entity"),
                Literal::from(1).into(),
                iri(&format!("{RDF}_7")),
            ],
        );
        assert_eq!(r, Some(iri("http://ex.org/dummy-entity17")));
        assert_eq!(
            call("entity", &[lit("http://ex.org/a b")]),
            Some(iri("http://ex.org/a%20b"))
        );
        assert_eq!(call("entity", &[lit("no scheme")]), None);
    }

    #[test]
    fn slots() {
        let s24 = iri(&format!("{RDF}_24"));
        assert_eq!(
            call("cardinal", std::slice::from_ref(&s24)),
            Some(Literal::from(24).into())
        );
        assert_eq!(call("cardinal", &[lit("24")]), None);
        assert_eq!(
            call("isContainerMembershipProperty", std::slice::from_ref(&s24)),
            Some(boolean(true))
        );
        assert_eq!(
            call("isContainerMembershipProperty", &[Literal::from(42).into()]),
            Some(boolean(false))
        );
        assert_eq!(
            call("before", &[iri(&format!("{RDF}_1")), s24.clone()]),
            Some(boolean(true))
        );
        assert_eq!(
            call("after", &[s24.clone(), s24.clone()]),
            Some(boolean(false))
        );
        assert_eq!(call("previous", &[iri(&format!("{RDF}_1"))]), None);
        assert_eq!(
            call("next", std::slice::from_ref(&s24)),
            Some(iri(&format!("{RDF}_25")))
        );
        assert_eq!(
            call(
                "forward",
                &[iri(&format!("{RDF}_2")), Literal::from(5).into()]
            ),
            Some(iri(&format!("{RDF}_7")))
        );
        assert_eq!(
            call("backward", &[s24, Literal::from(4).into()]),
            Some(iri(&format!("{RDF}_20")))
        );
    }

    #[test]
    fn strings() {
        assert_eq!(
            call(
                "URLEncoder.encode",
                &[lit("/This is a test/"), lit("UTF-8")]
            ),
            Some(lit("%2FThis+is+a+test%2F"))
        );
        assert_eq!(
            call(
                "URLDecoder.decode",
                &[lit("%2FThis+is+a+test%2F"), lit("UTF-8")]
            ),
            Some(lit("/This is a test/"))
        );
        assert_eq!(
            call("String.indexOf", &[lit("héllo"), lit("l")]),
            Some(Literal::from(2).into())
        );
        assert_eq!(
            call(
                "String.substring",
                &[
                    lit("héllo"),
                    Literal::from(1).into(),
                    Literal::from(3).into()
                ]
            ),
            Some(lit("él"))
        );
        assert_eq!(
            call("String.removeTags", &[lit("<b>bold</b> text")]),
            Some(lit("bold text"))
        );
        assert_eq!(
            call(
                "literal",
                &[lit("1"), iri("http://www.w3.org/2001/XMLSchema#int")]
            ),
            Some(
                Literal::new_typed_literal(
                    "1",
                    NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#int")
                )
                .into()
            )
        );
        assert_eq!(
            call("literal", &[lit("hi"), lit("en")]),
            Some(Literal::new_language_tagged_literal_unchecked("hi", "en").into())
        );
    }
}
