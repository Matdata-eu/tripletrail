//! `BIND`s of strings and IRIs on term ids (#11): `STR`, `CONCAT`, `IRI`, `STRLANG`, `STRDT`
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
use oxigraph::model::{Literal, NamedNode, Term};
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
        Expression::FunctionCall(F::StrLang | F::StrDt, args) => {
            args.len() == 2 && args.iter().all(arg)
        }
        // A pattern and flags that are constants, compiled once.
        Expression::FunctionCall(F::Replace, args) => {
            matches!(args.len(), 3 | 4)
                && arg(&args[0])
                && arg(&args[2])
                && args[1..]
                    .iter()
                    .step_by(2)
                    .all(|a| matches!(a, Expression::Literal(_)))
        }
        _ => false,
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
            let regex = compile_pattern(constant(args.get(1))??, constant(args.get(3))?)?;
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
        // A number the value kernels compute, as the literal the evaluator writes.
        e => {
            let k = crate::idvalue::compile(e, t, snapshot)?;
            Box::new(move |row| {
                Some(match k(row)? {
                    Ok(v) => Ok(crate::idvalue::literal(v)?.into()),
                    Err(()) => Err(()),
                })
            })
        }
    })
}

/// A constant string argument: `Some(None)` when there is none, `None` when it isn't one.
fn constant(e: Option<&Expression>) -> Option<Option<&str>> {
    match e {
        None => Some(None),
        Some(Expression::Literal(l)) if l.datatype() == xsd::STRING && l.language().is_none() => {
            Some(Some(l.value()))
        }
        _ => None,
    }
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
