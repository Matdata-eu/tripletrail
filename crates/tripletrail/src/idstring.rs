//! `BIND`s of strings and IRIs on term ids (#11): `STR`, `CONCAT`, `IRI`, `STRLANG`, `STRDT`,
//! `UCASE`, `LCASE`, `STRLEN`, `SUBSTR` (integer constants), `STRBEFORE`, `STRAFTER`
//! and `REPLACE` (a constant pattern and flags) of columns,
//! constants and numbers the value kernels compute, with the evaluator's rules
//! (`spareval::expression`): `STR` of an IRI or a literal is a simple literal of its text,
//! of a blank node an error; `CONCAT` of string literals keeps a language all of them share;
//! `IRI` of a simple literal that is an absolute IRI without dot segments is that IRI. A value only the kernels
//! decide is a term; the column holds its id (one made up for the query when the store doesn't
//! hold it). Anything else (a term only this query knows the kernels can't read, a
//! direction-tagged string, a string the evaluator would resolve against a base) gives up, and
//! the evaluator answers the query.

use oxigraph::model::vocab::xsd;
use oxigraph::model::{BaseDirection, Literal, NamedNode, Term};
use regex::{Regex, RegexBuilder};
use spargebra::algebra::{Expression, Function};
use std::cell::RefCell;
use std::collections::HashMap;
use tripletrail_store::{Kind, Snapshot, TermId};

use crate::idtree::Table;

/// A term per row: `Some(Ok(term))`, `Some(Err(()))` an error (the cell unbound), `None` when
/// the ids don't decide it.
pub(crate) type TermKernel<'k> = Box<dyn Fn(&[TermId]) -> Option<Result<Term, ()>> + 'k>;

/// Whether `e` is a `STR`, `CONCAT` or `IRI` the kernels may compute, by its form alone.
pub(crate) fn computable(e: &Expression) -> bool {
    use Function as F;
    let arg = |e: &Expression| {
        matches!(
            e,
            Expression::Variable(_) | Expression::Literal(_) | Expression::NamedNode(_)
        ) || computable(e)
            || crate::direct::number_computable(e)
    };
    match e {
        Expression::FunctionCall(F::Str | F::Iri, args) => args.len() == 1 && arg(&args[0]),
        Expression::FunctionCall(F::Concat, args) => args.iter().all(arg),
        Expression::FunctionCall(F::StrLang | F::StrDt | F::StrBefore | F::StrAfter, args) => {
            args.len() == 2 && args.iter().all(arg)
        }
        Expression::FunctionCall(F::UCase | F::LCase | F::StrLen, args) => {
            args.len() == 1 && arg(&args[0])
        }
        // A start and length that are integer constants.
        Expression::FunctionCall(F::SubStr, args) => {
            matches!(args.len(), 2 | 3)
                && arg(&args[0])
                && args[1..].iter().all(|a| integer(a).is_some())
        }
        // A pattern and flags that are constants, compiled once.
        Expression::FunctionCall(F::Replace, args) => {
            matches!(args.len(), 3 | 4)
                && arg(&args[0])
                && arg(&args[2])
                && args[1..].iter().step_by(2).all(|a| {
                    matches!(a, Expression::Literal(_)) || (computable(a) && without_variables(a))
                })
        }
        // `IF` on a condition of numbers and dates, and `COALESCE`, of any of these terms.
        // (Of numbers only, the value kernels compute them, straight to inline ids.)
        Expression::If(c, a, b) => {
            crate::idvalue::is_condition(c, &crate::direct::number_computable)
                && arg(a)
                && arg(b)
                && !crate::direct::number_computable(e)
        }
        Expression::Coalesce(list) => {
            !list.is_empty() && list.iter().all(arg) && !crate::direct::number_computable(e)
        }
        // A condition on numbers and dates, as an `xsd:boolean`.
        e => crate::idvalue::is_condition(e, &crate::direct::number_computable),
    }
}

/// `e` compiled to a kernel over the rows of `t`, if [`computable`] and every part of it is
/// decided on ids.
pub(crate) fn compile<'k>(
    e: &Expression,
    t: &Table,
    snapshot: &'k Snapshot,
) -> Option<TermKernel<'k>> {
    use Function as F;
    Some(match e {
        Expression::Variable(v) => {
            let c = t.col(v);
            let terms: RefCell<HashMap<TermId, Term>> = RefCell::default();
            Box::new(move |row| {
                let Some(id) = c.map(|c| row[c]).filter(|id| *id != TermId(0)) else {
                    return Some(Err(()));
                };
                if matches!(id.kind(), Kind::Row | Kind::Column | Kind::DefaultGraph) {
                    return None;
                }
                if let Some(t) = terms.borrow().get(&id) {
                    return Some(Ok(t.clone()));
                }
                let term = snapshot.term(id).ok()?;
                terms.borrow_mut().insert(id, term.clone());
                Some(Ok(term))
            })
        }
        Expression::Literal(l) => {
            let term = Term::from(l.clone());
            Box::new(move |_| Some(Ok(term.clone())))
        }
        Expression::NamedNode(n) => {
            let term = Term::from(n.clone());
            Box::new(move |_| Some(Ok(term.clone())))
        }
        Expression::FunctionCall(F::Str, args) if args.len() == 1 => {
            let a = compile(&args[0], t, snapshot)?;
            Box::new(move |row| {
                Some(match a(row)? {
                    Ok(Term::NamedNode(n)) => {
                        Ok(Literal::new_simple_literal(n.into_string()).into())
                    }
                    Ok(Term::Literal(l)) => Ok(Literal::new_simple_literal(l.destruct().0).into()),
                    _ => Err(()),
                })
            })
        }
        Expression::FunctionCall(F::Iri, args) if args.len() == 1 => {
            let a = compile(&args[0], t, snapshot)?;
            Box::new(move |row| {
                Some(match a(row)? {
                    Ok(Term::NamedNode(n)) => Ok(n.into()),
                    // An absolute IRI without dot segments is itself against any base;
                    // anything else the evaluator resolves or refuses.
                    Ok(Term::Literal(l)) if l.datatype() == xsd::STRING => {
                        let s = l.value();
                        if s.contains("/.") {
                            return None;
                        }
                        Ok(NamedNode::new(s).ok()?.into())
                    }
                    _ => Err(()),
                })
            })
        }
        Expression::FunctionCall(F::Concat, args) => {
            let parts = args
                .iter()
                .map(|a| compile(a, t, snapshot))
                .collect::<Option<Vec<_>>>()?;
            Box::new(move |row| {
                let mut value = String::new();
                // `None` before the first part; then the language all parts share, if any.
                let mut language: Option<Option<String>> = None;
                for part in &parts {
                    let Ok(Term::Literal(l)) = part(row)? else {
                        return Some(Err(()));
                    };
                    let (text, datatype, tag, direction) = l.destruct();
                    // A direction-tagged string: the evaluator's.
                    if direction.is_some() {
                        return None;
                    }
                    let tag = match (datatype, tag) {
                        (None, tag) => tag,
                        (Some(d), None) if d.as_ref() == xsd::STRING => None,
                        _ => return Some(Err(())),
                    };
                    match &language {
                        Some(shared) if *shared != tag => language = Some(None),
                        Some(_) => {}
                        None => language = Some(tag),
                    }
                    value.push_str(&text);
                }
                Some(Ok(match language.flatten() {
                    Some(tag) => Literal::new_language_tagged_literal_unchecked(value, tag).into(),
                    None => Literal::new_simple_literal(value).into(),
                }))
            })
        }
        Expression::FunctionCall(F::StrLang, args) if args.len() == 2 => {
            let (a, b) = (
                compile(&args[0], t, snapshot)?,
                compile(&args[1], t, snapshot)?,
            );
            Box::new(move |row| {
                let (Some(value), Some(tag)) = (simple(a(row)?), simple(b(row)?)) else {
                    return Some(Err(()));
                };
                Some(
                    Literal::new_language_tagged_literal(value, tag)
                        .map(Term::from)
                        .map_err(|_| ()),
                )
            })
        }
        Expression::FunctionCall(F::StrDt, args) if args.len() == 2 => {
            let (a, b) = (
                compile(&args[0], t, snapshot)?,
                compile(&args[1], t, snapshot)?,
            );
            Box::new(move |row| {
                let value = simple(a(row)?);
                let Ok(Term::NamedNode(datatype)) = b(row)? else {
                    return Some(Err(()));
                };
                let Some(value) = value else {
                    return Some(Err(()));
                };
                // The store reads numbers, booleans and dates by value, so their lexical
                // forms are the evaluator's to keep.
                if datatype.as_str().starts_with(XSD) && datatype.as_ref() != xsd::STRING {
                    return None;
                }
                Some(Ok(Literal::new_typed_literal(value, datatype).into()))
            })
        }
        Expression::FunctionCall(F::Replace, args) if matches!(args.len(), 3 | 4) => {
            let (pattern, flags) = (
                folded(&args[1], t, snapshot)?,
                match args.get(3) {
                    Some(f) => Some(folded(f, t, snapshot)?),
                    None => None,
                },
            );
            let regex = compile_pattern(&pattern, flags.as_deref())?;
            let a = compile(&args[0], t, snapshot)?;
            let replacement = compile(&args[2], t, snapshot)?;
            Box::new(move |row| {
                let (text, tag) = match a(row)? {
                    Ok(Term::Literal(l)) => {
                        let (text, datatype, tag, direction) = l.destruct();
                        if direction.is_some() {
                            return None;
                        }
                        match (datatype, tag) {
                            (None, tag) => (text, tag),
                            (Some(d), None) if d.as_ref() == xsd::STRING => (text, None),
                            _ => return Some(Err(())),
                        }
                    }
                    _ => return Some(Err(())),
                };
                let Some(replacement) = simple(replacement(row)?) else {
                    return Some(Err(()));
                };
                let text = match regex.replace_all(&text, replacement.as_str()) {
                    std::borrow::Cow::Owned(replaced) => replaced,
                    std::borrow::Cow::Borrowed(_) => text,
                };
                Some(Ok(match tag {
                    Some(tag) => Literal::new_language_tagged_literal_unchecked(text, tag).into(),
                    None => Literal::new_simple_literal(text).into(),
                }))
            })
        }
        Expression::FunctionCall(F::UCase | F::LCase, args) if args.len() == 1 => {
            let upper = matches!(e, Expression::FunctionCall(F::UCase, _));
            let a = compile(&args[0], t, snapshot)?;
            Box::new(move |row| {
                Some(string_and_language(a(row)?).map(|(text, tag)| {
                    let text = if upper {
                        text.to_uppercase()
                    } else {
                        text.to_lowercase()
                    };
                    plain(text, tag)
                }))
            })
        }
        Expression::FunctionCall(F::StrLen, args) if args.len() == 1 => {
            let a = compile(&args[0], t, snapshot)?;
            Box::new(move |row| {
                Some(match string_and_language(a(row)?) {
                    Ok((text, _)) => {
                        let n = i64::try_from(text.chars().count()).map_err(|_| ());
                        n.map(|n| Literal::from(n).into())
                    }
                    Err(()) => Err(()),
                })
            })
        }
        Expression::FunctionCall(F::SubStr, args) if matches!(args.len(), 2 | 3) => {
            let start = integer(&args[1])?;
            let length = match args.get(2) {
                Some(l) => Some(integer(l)?),
                None => None,
            };
            let a = compile(&args[0], t, snapshot)?;
            Box::new(move |row| {
                Some(string_and_language(a(row)?).and_then(|(text, tag)| {
                    let part = substring(&text, start, length).ok_or(())?;
                    Ok(plain(part.to_owned(), tag))
                }))
            })
        }
        Expression::FunctionCall(F::StrBefore | F::StrAfter, args) if args.len() == 2 => {
            let before = matches!(e, Expression::FunctionCall(F::StrBefore, _));
            let (a, b) = (
                compile(&args[0], t, snapshot)?,
                compile(&args[1], t, snapshot)?,
            );
            Box::new(move |row| {
                let a = string_and_language(a(row)?);
                let b = string_and_language(b(row)?);
                let (Ok((text, tag)), Ok((needle, needle_tag))) = (a, b) else {
                    return Some(Err(()));
                };
                // The second argument's language, if any, is the first's.
                if needle_tag.is_some() && needle_tag != tag {
                    return Some(Err(()));
                }
                Some(Ok(match text.find(needle.as_str()) {
                    Some(at) if before => plain(text[..at].to_owned(), tag),
                    Some(at) => plain(text[at + needle.len()..].to_owned(), tag),
                    None => Literal::new_simple_literal("").into(),
                }))
            })
        }
        // Only the branch taken is computed, as in the evaluator.
        Expression::If(c, a, b) => {
            let (c, a, b) = (
                crate::idvalue::condition(c, t, snapshot)?,
                compile(a, t, snapshot)?,
                compile(b, t, snapshot)?,
            );
            Box::new(move |row| match c(row)? {
                Some(true) => a(row),
                Some(false) => b(row),
                None => Some(Err(())),
            })
        }
        Expression::Coalesce(list) => {
            let list = list
                .iter()
                .map(|e| compile(e, t, snapshot))
                .collect::<Option<Vec<_>>>()?;
            Box::new(move |row| {
                for k in &list {
                    if let Ok(term) = k(row)? {
                        return Some(Ok(term));
                    }
                }
                Some(Err(()))
            })
        }
        // A condition on numbers and dates: `true` or `false`, or an error.
        e if crate::idvalue::is_condition(e, &crate::direct::number_computable) => {
            let c = crate::idvalue::condition(e, t, snapshot)?;
            Box::new(move |row| {
                Some(match c(row)? {
                    Some(b) => Ok(Literal::from(b).into()),
                    None => Err(()),
                })
            })
        }
        // A number the value kernels compute, as the literal the evaluator writes.
        e => {
            let k = crate::idvalue::compile_output(e, t, snapshot)?;
            Box::new(move |row| {
                Some(match k(row)? {
                    Ok((v, form)) => Ok(crate::idvalue::literal(v, form)?.into()),
                    Err(()) => Err(()),
                })
            })
        }
    })
}

/// A constant string argument: `Some(None)` when there is none, `None` when it isn't one.
/// A constant string argument, such as a `REPLACE` pattern: a simple literal, or an
/// expression without variables whose value is one (`STR(ex:)`), computed once. `None` for
/// anything else.
fn folded(e: &Expression, t: &Table, snapshot: &Snapshot) -> Option<String> {
    if let Expression::Literal(_) = e {
        return constant(Some(e)).map(|c| c.unwrap_or_default().to_owned());
    }
    if !without_variables(e) {
        return None;
    }
    simple(compile(e, t, snapshot)?(&[])?)
}

fn without_variables(e: &Expression) -> bool {
    let mut names = std::collections::HashSet::new();
    crate::plan::expression_variables(e, &mut names).is_some() && names.is_empty()
}

fn constant(e: Option<&Expression>) -> Option<Option<&str>> {
    match e {
        None => Some(None),
        Some(Expression::Literal(l)) if l.datatype() == xsd::STRING && l.language().is_none() => {
            Some(Some(l.value()))
        }
        _ => None,
    }
}

/// A language tag with its base direction, if any.
type Language = Option<(String, Option<BaseDirection>)>;

/// A string literal's text and language (the evaluator's `to_string_and_language`), or
/// `Err(())` for any other term or an error.
fn string_and_language(t: Result<Term, ()>) -> Result<(String, Language), ()> {
    let Ok(Term::Literal(l)) = t else {
        return Err(());
    };
    match l.destruct() {
        (text, None, tag, direction) => Ok((text, tag.map(|tag| (tag, direction)))),
        (text, Some(d), None, _) if d.as_ref() == xsd::STRING => Ok((text, None)),
        _ => Err(()),
    }
}

/// A simple or language-tagged literal of `text` (the evaluator's `build_plain_literal`).
fn plain(text: String, language: Language) -> Term {
    match language {
        Some((tag, Some(direction))) => {
            Literal::new_directional_language_tagged_literal_unchecked(text, tag, direction).into()
        }
        Some((tag, None)) => Literal::new_language_tagged_literal_unchecked(text, tag).into(),
        None => Literal::new_simple_literal(text).into(),
    }
}

/// An `xsd:integer` constant, as the evaluator reads it.
fn integer(e: &Expression) -> Option<i64> {
    match e {
        Expression::Literal(l) if l.datatype() == xsd::INTEGER => l
            .value()
            .parse::<oxsdatatypes::Integer>()
            .ok()
            .map(i64::from),
        Expression::UnaryMinus(e) => integer(e)?.checked_neg(),
        _ => None,
    }
}

/// `SUBSTR` on character positions, from 1 (the evaluator's `substring`).
fn substring(source: &str, start: i64, length: Option<i64>) -> Option<&str> {
    let start = start.checked_sub(1)?;
    let end = match length {
        Some(length) => Some(start.checked_add(length)?),
        None => None,
    };
    let mut start_position = None;
    for (position, (byte_position, _)) in source.char_indices().enumerate() {
        let position = i64::try_from(position).ok()?;
        if start <= position && end.is_none_or(|end| position < end) {
            start_position.get_or_insert(byte_position);
        } else if let Some(start_position) = start_position {
            return Some(&source[start_position..byte_position]);
        }
    }
    Some(start_position.map_or("", |start_position| &source[start_position..]))
}

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

/// The text of a simple literal (an `xsd:string`); `None` for any other term or an error.
fn simple(t: Result<Term, ()>) -> Option<String> {
    match t {
        Ok(Term::Literal(l)) if l.datatype() == xsd::STRING && l.language().is_none() => {
            Some(l.destruct().0)
        }
        _ => None,
    }
}

/// A `REPLACE` pattern with its flags, as the evaluator compiles it
/// (`spareval::expression::compile_pattern`): `None` for flags or a pattern it refuses.
fn compile_pattern(pattern: &str, flags: Option<&str>) -> Option<Regex> {
    let flags = flags.unwrap_or_default();
    let pattern = if flags.contains('q') {
        regex::escape(pattern)
    } else {
        pattern.to_owned()
    };
    let mut builder = RegexBuilder::new(&pattern);
    builder.size_limit(1_000_000);
    for flag in flags.chars() {
        match flag {
            's' => {
                builder.dot_matches_new_line(true);
            }
            'm' => {
                builder.multi_line(true);
            }
            'i' => {
                builder.case_insensitive(true);
            }
            'x' => {
                builder.ignore_whitespace(true);
            }
            'q' => {}
            _ => return None,
        }
    }
    builder.build().ok()
}
