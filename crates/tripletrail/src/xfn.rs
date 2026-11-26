//! The XPath and Jena function libraries (`fn:`, `math:`, `afn:`) and the integer casts that
//! Jena accepts (`xsd:int`, `xsd:long`, ...), so queries written for Jena, Fuseki and SPARQL
//! Anything run unchanged (#231).
//!
//! Functions that a SPARQL built-in already computes (`fn:matches` is `REGEX`, `fn:upper-case`
//! is `UCASE`, ...) are rewritten to that built-in before evaluation ([`alias_query`],
//! [`alias_update`]): they get its exact semantics and its optimisations, such as a regular
//! expression compiled once. The others are custom functions ([`functions`]). A call whose
//! arity the built-in does not take (`fn:round` with a precision) stays a custom call.

use oxigraph::model::vocab::{rdf, xsd};
use oxigraph::model::{Literal, NamedNode, Term};
use oxsdatatypes::{
    Date, DateTime, DayTimeDuration, Decimal, Double, Duration, Float, Integer, Time,
    TimezoneOffset,
};
use spargebra::algebra::{
    AggregateExpression, Expression, Function, GraphPattern, OrderExpression,
};
use spargebra::{GraphUpdateOperation, Query, Update};
use std::str::FromStr;
use std::sync::Arc;

/// XPath functions: `http://www.w3.org/2005/xpath-functions#`.
pub const FN: &str = "http://www.w3.org/2005/xpath-functions#";
/// XPath math functions: `http://www.w3.org/2005/xpath-functions/math#`.
pub const MATH: &str = "http://www.w3.org/2005/xpath-functions/math#";
/// Jena ARQ functions: `http://jena.apache.org/ARQ/function#`.
pub const AFN: &str = "http://jena.apache.org/ARQ/function#";
/// The old Jena ARQ namespace, which Jena still accepts.
const AFN_OLD: &str = "http://jena.hpl.hp.com/ARQ/function#";

/// A custom function: SPARQL arguments in, value out (`None` is a SPARQL error).
pub type Func = Arc<dyn Fn(&[Term]) -> Option<Term> + Send + Sync>;

// ---------------------------------------------------------------------------------------------
// Rewriting calls to SPARQL built-ins
// ---------------------------------------------------------------------------------------------

/// The built-in a call of `iri` with `arity` arguments is rewritten to.
fn builtin(iri: &str, args: &mut Vec<Expression>) -> Option<Expression> {
    use Function as F;
    let n = args.len();
    let call = |f: Function, args: &mut Vec<Expression>| {
        Some(Expression::FunctionCall(f, std::mem::take(args)))
    };
    if let Some(name) = iri.strip_prefix(FN) {
        return match (name, n) {
            ("concat", _) => call(F::Concat, args),
            ("substring", 2 | 3) => call(F::SubStr, args),
            ("string-length", 1) => call(F::StrLen, args),
            ("upper-case", 1) => call(F::UCase, args),
            ("lower-case", 1) => call(F::LCase, args),
            ("contains", 2) => call(F::Contains, args),
            ("starts-with", 2) => call(F::StrStarts, args),
            ("ends-with", 2) => call(F::StrEnds, args),
            ("substring-before", 2) => call(F::StrBefore, args),
            ("substring-after", 2) => call(F::StrAfter, args),
            ("matches", 2 | 3) => call(F::Regex, args),
            ("replace", 3 | 4) => call(F::Replace, args),
            ("abs", 1) => call(F::Abs, args),
            ("ceiling", 1) => call(F::Ceil, args),
            ("floor", 1) => call(F::Floor, args),
            ("round", 1) => call(F::Round, args),
            ("encode-for-uri", 1) => call(F::EncodeForUri, args),
            ("string", 1) => call(F::Str, args),
            ("current-dateTime", 0) => call(F::Now, args),
            (
                "year-from-dateTime" | "year-from-date" | "years-from-dateTime" | "years-from-date",
                1,
            ) => call(F::Year, args),
            (
                "month-from-dateTime"
                | "month-from-date"
                | "months-from-dateTime"
                | "months-from-date",
                1,
            ) => call(F::Month, args),
            (
                "day-from-dateTime" | "day-from-date" | "days-from-dateTime" | "days-from-date",
                1,
            ) => call(F::Day, args),
            ("hours-from-dateTime" | "hours-from-time", 1) => call(F::Hours, args),
            ("minutes-from-dateTime" | "minutes-from-time", 1) => call(F::Minutes, args),
            ("seconds-from-dateTime" | "seconds-from-time", 1) => call(F::Seconds, args),
            ("timezone-from-dateTime" | "timezone-from-date" | "timezone-from-time", 1) => {
                call(F::Timezone, args)
            }
            ("not", 1) => Some(Expression::Not(Box::new(args.remove(0)))),
            ("boolean", 1) => Some(Expression::Not(Box::new(Expression::Not(Box::new(
                args.remove(0),
            ))))),
            _ => None,
        };
    }
    let name = iri
        .strip_prefix(AFN)
        .or_else(|| iri.strip_prefix(AFN_OLD))?;
    match (name, n) {
        ("now", 0) => call(F::Now, args),
        // Jena hashes the string of an IRI too.
        ("sha1sum", 1) => Some(Expression::FunctionCall(
            F::Sha1,
            vec![Expression::FunctionCall(F::Str, vec![args.remove(0)])],
        )),
        _ => None,
    }
}

/// The IRIs [`alias_query`] rewrites to SPARQL built-ins (for the service description).
pub fn builtin_aliases() -> Vec<NamedNode> {
    const FN_NAMES: &[&str] = &[
        "concat",
        "substring",
        "string-length",
        "upper-case",
        "lower-case",
        "contains",
        "starts-with",
        "ends-with",
        "substring-before",
        "substring-after",
        "matches",
        "replace",
        "abs",
        "ceiling",
        "floor",
        "round",
        "encode-for-uri",
        "string",
        "current-dateTime",
        "year-from-dateTime",
        "year-from-date",
        "years-from-dateTime",
        "years-from-date",
        "month-from-dateTime",
        "month-from-date",
        "months-from-dateTime",
        "months-from-date",
        "day-from-dateTime",
        "day-from-date",
        "days-from-dateTime",
        "days-from-date",
        "hours-from-dateTime",
        "hours-from-time",
        "minutes-from-dateTime",
        "minutes-from-time",
        "seconds-from-dateTime",
        "seconds-from-time",
        "timezone-from-dateTime",
        "timezone-from-date",
        "timezone-from-time",
        "not",
        "boolean",
    ];
    let custom: Vec<String> = functions()
        .into_iter()
        .map(|(n, _)| n.into_string())
        .collect();
    FN_NAMES
        .iter()
        .map(|n| format!("{FN}{n}"))
        .chain(["now", "sha1sum"].iter().map(|n| format!("{AFN}{n}")))
        // fn:round with a precision is also a custom function: list it once.
        .filter(|n| !custom.contains(n))
        .map(NamedNode::new_unchecked)
        .collect()
}

/// `query` with the calls of [`builtin`] functions replaced by the built-ins.
pub(crate) fn alias_query(query: Query) -> Query {
    match query {
        Query::Select {
            dataset,
            pattern,
            base_iri,
        } => Query::Select {
            dataset,
            pattern: pattern_(pattern),
            base_iri,
        },
        Query::Construct {
            template,
            dataset,
            pattern,
            base_iri,
        } => Query::Construct {
            template,
            dataset,
            pattern: pattern_(pattern),
            base_iri,
        },
        Query::Describe {
            dataset,
            pattern,
            base_iri,
        } => Query::Describe {
            dataset,
            pattern: pattern_(pattern),
            base_iri,
        },
        Query::Ask {
            dataset,
            pattern,
            base_iri,
        } => Query::Ask {
            dataset,
            pattern: pattern_(pattern),
            base_iri,
        },
    }
}

/// [`alias_query`] for the patterns of an update.
pub(crate) fn alias_update(update: Update) -> Update {
    Update {
        operations: update
            .operations
            .into_iter()
            .map(|op| match op {
                GraphUpdateOperation::DeleteInsert {
                    delete,
                    insert,
                    using,
                    pattern,
                } => GraphUpdateOperation::DeleteInsert {
                    delete,
                    insert,
                    using,
                    pattern: Box::new(pattern_(*pattern)),
                },
                other => other,
            })
            .collect(),
        base_iri: update.base_iri,
    }
}

fn pattern_(p: GraphPattern) -> GraphPattern {
    use GraphPattern as G;
    let b = |p: Box<G>| Box::new(pattern_(*p));
    match p {
        // A group's property functions see what comes before them in the group.
        G::Join { left, right } => match *right {
            right @ G::Bgp { .. } => crate::propfn::group(Some(pattern_(*left)), right),
            right => G::Join {
                left: b(left),
                right: Box::new(pattern_(right)),
            },
        },
        // An OPTIONAL calling a property function is evaluated once per row of its left side
        // (as Jena does), so the call sees the values bound there.
        G::LeftJoin {
            left,
            right,
            expression,
        } if crate::propfn::calls_property_function(&right) => G::Lateral {
            left: b(left),
            right: Box::new(G::LeftJoin {
                left: Box::new(G::Bgp {
                    patterns: Vec::new(),
                }),
                right: b(right),
                expression: expression.map(expression_),
            }),
        },
        G::LeftJoin {
            left,
            right,
            expression,
        } => G::LeftJoin {
            left: b(left),
            right: b(right),
            expression: expression.map(expression_),
        },
        G::Lateral { left, right } => G::Lateral {
            left: b(left),
            right: b(right),
        },
        G::Filter { expr, inner } => G::Filter {
            expr: expression_(expr),
            inner: b(inner),
        },
        G::Union { left, right } => G::Union {
            left: b(left),
            right: b(right),
        },
        G::Graph { name, inner } => G::Graph {
            name,
            inner: b(inner),
        },
        G::Extend {
            inner,
            variable,
            expression,
        } => G::Extend {
            inner: b(inner),
            variable,
            expression: expression_(expression),
        },
        G::Minus { left, right } => G::Minus {
            left: b(left),
            right: b(right),
        },
        G::OrderBy { inner, expression } => G::OrderBy {
            inner: b(inner),
            expression: expression
                .into_iter()
                .map(|o| match o {
                    OrderExpression::Asc(e) => OrderExpression::Asc(expression_(e)),
                    OrderExpression::Desc(e) => OrderExpression::Desc(expression_(e)),
                })
                .collect(),
        },
        G::Project { inner, variables } => G::Project {
            inner: b(inner),
            variables,
        },
        G::Distinct { inner } => G::Distinct { inner: b(inner) },
        G::Reduced { inner } => G::Reduced { inner: b(inner) },
        G::Slice {
            inner,
            start,
            length,
        } => G::Slice {
            inner: b(inner),
            start,
            length,
        },
        G::Group {
            inner,
            variables,
            aggregates,
        } => G::Group {
            inner: b(inner),
            variables,
            aggregates: aggregates
                .into_iter()
                .map(|(v, a)| {
                    let a = match a {
                        AggregateExpression::FunctionCall {
                            name,
                            expr,
                            distinct,
                        } => AggregateExpression::FunctionCall {
                            name,
                            expr: expression_(expr),
                            distinct,
                        },
                        other => other,
                    };
                    (v, a)
                })
                .collect(),
        },
        G::Service {
            name,
            inner,
            silent,
        } => G::Service {
            name,
            inner: b(inner),
            silent,
        },
        bgp @ G::Bgp { .. } => crate::propfn::group(None, bgp),
        other @ (G::Path { .. } | G::Values { .. }) => other,
    }
}

fn expression_(e: Expression) -> Expression {
    use Expression as E;
    let b = |e: Box<E>| Box::new(expression_(*e));
    match e {
        E::Or(l, r) => E::Or(b(l), b(r)),
        E::And(l, r) => E::And(b(l), b(r)),
        E::Equal(l, r) => E::Equal(b(l), b(r)),
        E::SameTerm(l, r) => E::SameTerm(b(l), b(r)),
        E::Greater(l, r) => E::Greater(b(l), b(r)),
        E::GreaterOrEqual(l, r) => E::GreaterOrEqual(b(l), b(r)),
        E::Less(l, r) => E::Less(b(l), b(r)),
        E::LessOrEqual(l, r) => E::LessOrEqual(b(l), b(r)),
        E::In(l, r) => E::In(b(l), r.into_iter().map(expression_).collect()),
        E::Add(l, r) => E::Add(b(l), b(r)),
        E::Subtract(l, r) => E::Subtract(b(l), b(r)),
        E::Multiply(l, r) => E::Multiply(b(l), b(r)),
        E::Divide(l, r) => E::Divide(b(l), b(r)),
        E::UnaryPlus(i) => E::UnaryPlus(b(i)),
        E::UnaryMinus(i) => E::UnaryMinus(b(i)),
        E::Not(i) => E::Not(b(i)),
        E::Exists(p) => E::Exists(Box::new(pattern_(*p))),
        E::If(c, t, f) => E::If(b(c), b(t), b(f)),
        E::Coalesce(v) => E::Coalesce(v.into_iter().map(expression_).collect()),
        E::FunctionCall(f, args) => {
            let mut args: Vec<E> = args.into_iter().map(expression_).collect();
            if let Function::Custom(name) = &f
                && let Some(e) = builtin(name.as_str(), &mut args)
                    .or_else(|| crate::ttfn::rewrite(name.as_str(), &mut args))
                    .or_else(|| crate::temporal::rewrite(name.as_str(), &mut args))
            {
                return e;
            }
            E::FunctionCall(f, args)
        }
        other @ (E::NamedNode(_) | E::Literal(_) | E::Variable(_) | E::Bound(_)) => other,
    }
}

// ---------------------------------------------------------------------------------------------
// Argument and value helpers
// ---------------------------------------------------------------------------------------------

/// A numeric value, in the type it came in.
#[derive(Clone, Copy, Debug)]
enum Num {
    Integer(Integer),
    Decimal(Decimal),
    Float(Float),
    Double(Double),
}

const INTEGER_TYPES: &[&str] = &[
    "http://www.w3.org/2001/XMLSchema#integer",
    "http://www.w3.org/2001/XMLSchema#long",
    "http://www.w3.org/2001/XMLSchema#int",
    "http://www.w3.org/2001/XMLSchema#short",
    "http://www.w3.org/2001/XMLSchema#byte",
    "http://www.w3.org/2001/XMLSchema#nonNegativeInteger",
    "http://www.w3.org/2001/XMLSchema#positiveInteger",
    "http://www.w3.org/2001/XMLSchema#nonPositiveInteger",
    "http://www.w3.org/2001/XMLSchema#negativeInteger",
    "http://www.w3.org/2001/XMLSchema#unsignedLong",
    "http://www.w3.org/2001/XMLSchema#unsignedInt",
    "http://www.w3.org/2001/XMLSchema#unsignedShort",
    "http://www.w3.org/2001/XMLSchema#unsignedByte",
];

fn num(t: &Term) -> Option<Num> {
    let Term::Literal(l) = t else { return None };
    let dt = l.datatype();
    let v = l.value();
    if dt == xsd::DOUBLE {
        Double::from_str(v).ok().map(Num::Double)
    } else if dt == xsd::FLOAT {
        Float::from_str(v).ok().map(Num::Float)
    } else if dt == xsd::DECIMAL {
        Decimal::from_str(v).ok().map(Num::Decimal)
    } else if INTEGER_TYPES.contains(&dt.as_str()) {
        Integer::from_str(v).ok().map(Num::Integer)
    } else {
        None
    }
}

impl Num {
    fn f64(self) -> f64 {
        match self {
            Num::Integer(i) => i64::from(i) as f64,
            Num::Decimal(d) => d.to_string().parse().unwrap_or(f64::NAN),
            Num::Float(f) => f64::from(f32::from(f)),
            Num::Double(d) => f64::from(d),
        }
    }

    /// The exact decimal value of an integer or decimal.
    fn decimal(self) -> Option<Decimal> {
        match self {
            Num::Integer(i) => Some(i.into()),
            Num::Decimal(d) => Some(d),
            _ => None,
        }
    }

    fn term(self) -> Term {
        match self {
            Num::Integer(i) => Literal::from(i).into(),
            Num::Decimal(d) => spareval::decimal_literal(d).into(),
            Num::Float(f) => Literal::from(f32::from(f)).into(),
            Num::Double(d) => Literal::from(f64::from(d)).into(),
        }
    }
}

fn f64_arg(t: &Term) -> Option<f64> {
    num(t).map(Num::f64)
}

fn double(x: f64) -> Option<Term> {
    Some(Literal::from(x).into())
}

fn integer(i: i64) -> Option<Term> {
    Some(Literal::from(i).into())
}

/// A string argument: a simple literal, `xsd:string` or a language-tagged string.
fn string(t: &Term) -> Option<&str> {
    let Term::Literal(l) = t else { return None };
    (l.datatype() == xsd::STRING || l.datatype() == rdf::LANG_STRING).then(|| l.value())
}

fn string_term(s: impl Into<String>) -> Option<Term> {
    Some(Literal::new_simple_literal(s).into())
}

/// The string of any IRI or literal (`STR`).
fn str_of(t: &Term) -> Option<&str> {
    match t {
        Term::NamedNode(n) => Some(n.as_str()),
        Term::Literal(l) => Some(l.value()),
        _ => None,
    }
}

fn int_arg(t: &Term) -> Option<i64> {
    match num(t)? {
        Num::Integer(i) => Some(i64::from(i)),
        _ => None,
    }
}

fn pow10(p: u32) -> Option<Decimal> {
    10_i64.checked_pow(p).map(Decimal::from)
}

/// `x` rounded to `precision` digits after the point; ties to even, or towards positive
/// infinity (`fn:round`).
fn round_decimal(x: Decimal, precision: i64, even: bool) -> Option<Decimal> {
    if precision > 18 {
        return Some(x);
    }
    if precision < -18 {
        return Some(Decimal::from(0));
    }
    let scale = pow10(precision.unsigned_abs() as u32)?;
    let y = if precision >= 0 {
        x.checked_mul(scale)?
    } else {
        x.checked_div(scale)?
    };
    let floor = y.checked_floor()?;
    let frac = y.checked_sub(floor)?;
    let half = Decimal::from_str("0.5").ok()?;
    let up = if frac > half {
        true
    } else if frac < half {
        false
    } else if even {
        floor.checked_rem(Decimal::from(2))? != Decimal::from(0)
    } else {
        true
    };
    let r = if up {
        floor.checked_add(Decimal::from(1))?
    } else {
        floor
    };
    if precision >= 0 {
        r.checked_div(scale)
    } else {
        r.checked_mul(scale)
    }
}

fn round_f64(x: f64, precision: i64, even: bool) -> f64 {
    if !x.is_finite() || precision > 15 {
        return x;
    }
    let scale = 10_f64.powi(precision.clamp(-308, 308) as i32);
    let y = x * scale;
    let r = if even {
        y.round_ties_even()
    } else {
        (y + 0.5).floor()
    };
    r / scale
}

/// `fn:round` / `fn:round-half-to-even` with an optional precision, in the argument's type.
fn round(args: &[Term], even: bool) -> Option<Term> {
    let (x, precision) = match args {
        [x] => (num(x)?, 0),
        [x, p] => (num(x)?, int_arg(p)?),
        _ => return None,
    };
    Some(match x {
        Num::Integer(i) => {
            if precision >= 0 {
                x.term()
            } else {
                let d = round_decimal(i.into(), precision, even)?;
                Num::Integer(Integer::try_from(d).ok()?).term()
            }
        }
        Num::Decimal(d) => Num::Decimal(round_decimal(d, precision, even)?).term(),
        Num::Float(f) => {
            Literal::from(round_f64(f64::from(f32::from(f)), precision, even) as f32).into()
        }
        Num::Double(d) => Literal::from(round_f64(f64::from(d), precision, even)).into(),
    })
}

fn compare_nums(a: Num, b: Num) -> Option<std::cmp::Ordering> {
    match (a.decimal(), b.decimal()) {
        (Some(a), Some(b)) => Some(a.cmp(&b)),
        _ => a.f64().partial_cmp(&b.f64()),
    }
}

/// `fn:min` / `fn:max` (and `afn:min` / `afn:max`) of the arguments: numbers by value,
/// strings by code point. The argument itself is returned.
fn extreme(args: &[Term], max: bool) -> Option<Term> {
    let mut best: Option<&Term> = None;
    for a in args {
        best = Some(match best {
            None => {
                num(a).map(|_| ()).or_else(|| string(a).map(|_| ()))?;
                a
            }
            Some(b) => {
                let ord = match (num(a), num(b)) {
                    (Some(x), Some(y)) => compare_nums(x, y)?,
                    (None, None) => string(a)?.cmp(string(b)?),
                    _ => return None,
                };
                let better = if max { ord.is_gt() } else { ord.is_lt() };
                if better { a } else { b }
            }
        });
    }
    best.cloned()
}

fn numeric_mod(args: &[Term]) -> Option<Term> {
    let [a, b] = args else { return None };
    let (a, b) = (num(a)?, num(b)?);
    match (a, b) {
        (Num::Integer(x), Num::Integer(y)) => Some(Num::Integer(x.checked_rem(y)?).term()),
        (Num::Double(_), _) | (_, Num::Double(_)) => double(a.f64() % b.f64()),
        (Num::Float(_), _) | (_, Num::Float(_)) => {
            Some(Literal::from((a.f64() % b.f64()) as f32).into())
        }
        _ => Some(Num::Decimal(a.decimal()?.checked_rem(b.decimal()?)?).term()),
    }
}

fn integer_divide(args: &[Term]) -> Option<Term> {
    let [a, b] = args else { return None };
    let (a, b) = (num(a)?, num(b)?);
    match (a.decimal(), b.decimal()) {
        (Some(x), Some(y)) => {
            let q = x.checked_div(y)?;
            Some(Num::Integer(Integer::try_from(q).ok()?).term())
        }
        _ => {
            let q = (a.f64() / b.f64()).trunc();
            (q.is_finite() && q.abs() < 9.2e18).then(|| Literal::from(q as i64).into())
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Strings
// ---------------------------------------------------------------------------------------------

fn normalize_space(s: &str) -> String {
    s.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

fn translate(s: &str, map: &str, trans: &str) -> String {
    let map: Vec<char> = map.chars().collect();
    let trans: Vec<char> = trans.chars().collect();
    s.chars()
        .filter_map(|c| match map.iter().position(|&m| m == c) {
            Some(i) => trans.get(i).copied(),
            None => Some(c),
        })
        .collect()
}

/// `fn:escape-html-uri` (`all` false) and `fn:iri-to-uri` (`all` true): percent-encodes the
/// UTF-8 bytes of characters outside printable ASCII, and for `iri-to-uri` also the ASCII
/// characters an URI cannot hold.
fn escape_uri(s: &str, all: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let escape = !(' '..='~').contains(&c)
            || (all
                && matches!(
                    c,
                    ' ' | '<' | '>' | '"' | '{' | '}' | '|' | '\\' | '^' | '`'
                ));
        if escape {
            let mut buf = [0; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || !c.is_ascii()
}

fn is_name_char(c: char) -> bool {
    is_name_start(c) || c.is_ascii_digit() || c == '-' || c == '.'
}

/// Splits an IRI into namespace and local name as Jena does: the local name is the longest
/// XML name (NCName) at the end.
fn split_iri(iri: &str) -> (&str, &str) {
    let mut start = iri.len();
    for (i, c) in iri.char_indices().rev() {
        if is_name_char(c) {
            start = i;
        } else {
            break;
        }
    }
    // A name starts with a letter or `_`.
    while let Some(c) = iri[start..].chars().next() {
        if is_name_start(c) {
            break;
        }
        start += c.len_utf8();
    }
    iri.split_at(start)
}

/// Java-style substring: code points `start` (from 0) to `end` (exclusive).
fn java_substring(s: &str, start: i64, end: Option<i64>) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let end = end.unwrap_or(chars.len() as i64);
    if start < 0 || end > chars.len() as i64 || start > end {
        return None;
    }
    Some(chars[start as usize..end as usize].iter().collect())
}

/// Inserts `sep` into the digits `int` at the grouping positions (counted from the right);
/// `regular` repeats the first position.
fn group_digits(int: &str, positions: &[(usize, char)], regular: bool) -> String {
    if positions.is_empty() {
        return int.to_owned();
    }
    let digits: Vec<char> = int.chars().collect();
    let mut out = Vec::with_capacity(digits.len() * 2);
    for (k, c) in digits.iter().rev().enumerate() {
        if k > 0 {
            let sep = if regular {
                let (g, sep) = positions[0];
                (k % g == 0).then_some(sep)
            } else {
                positions.iter().find(|(p, _)| *p == k).map(|(_, s)| *s)
            };
            if let Some(sep) = sep {
                out.push(sep);
            }
        }
        out.push(*c);
    }
    out.iter().rev().collect()
}

/// Grouping positions of the integer part of a picture (separators counted from the right)
/// and whether they are regular (all multiples of the first, with no gap).
fn grouping(int_picture: &str, is_digit: impl Fn(char) -> bool) -> (Vec<(usize, char)>, bool) {
    let mut positions = Vec::new();
    let mut digits = 0;
    for c in int_picture.chars().rev() {
        if is_digit(c) {
            digits += 1;
        } else {
            positions.push((digits, c));
        }
    }
    positions.retain(|(p, _)| *p > 0);
    let regular = match positions.first() {
        Some(&(g, sep)) => positions
            .iter()
            .enumerate()
            .all(|(i, &(p, s))| p == g * (i + 1) && s == sep),
        None => false,
    };
    (positions, regular)
}

/// `fn:format-number(value, picture)` with the default decimal format: digits `0`-`9`, `#`,
/// `.` and `,`, `%` and `‰`, and a negative sub-picture after `;`. Rounds half to even.
fn format_number(x: Num, picture: &str) -> Option<String> {
    let (pos, neg) = match picture.split_once(';') {
        Some((p, n)) => (p, Some(n)),
        None => (picture, None),
    };
    let mantissa = |c: char| c.is_ascii_digit() || matches!(c, '#' | '.' | ',');
    let affixes = |p: &str| -> Option<(String, String, String)> {
        let first = p.find(mantissa)?;
        let last = p.rfind(mantissa)?;
        let end = last + p[last..].chars().next()?.len_utf8();
        Some((
            p[..first].to_owned(),
            p[first..end].to_owned(),
            p[end..].to_owned(),
        ))
    };
    let (prefix, body, suffix) = affixes(pos)?;
    let percent = prefix.contains('%') || suffix.contains('%');
    let permille = prefix.contains('‰') || suffix.contains('‰');
    let (int_pic, frac_pic) = match body.split_once('.') {
        Some((i, f)) => (i, f),
        None => (body.as_str(), ""),
    };
    if frac_pic.contains('.') {
        return None;
    }
    let min_int = int_pic.chars().filter(char::is_ascii_digit).count();
    let min_frac = frac_pic.chars().filter(char::is_ascii_digit).count();
    let max_frac = frac_pic
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '#')
        .count();
    let (positions, regular) = grouping(int_pic, |c| c.is_ascii_digit() || c == '#');
    let factor = if percent {
        100
    } else if permille {
        1000
    } else {
        1
    };

    // Sign and digits, rounded half to even to `max_frac` digits.
    let (negative, digits) = match x.decimal() {
        Some(d) => {
            let d = d.checked_mul(Decimal::from(factor))?;
            let r = round_decimal(d, max_frac as i64, true)?;
            let negative = r < Decimal::from(0);
            let s = r.to_string();
            (negative, s.trim_start_matches('-').to_owned())
        }
        None => {
            let v = x.f64() * factor as f64;
            if v.is_nan() {
                return Some("NaN".to_owned());
            }
            if v.is_infinite() {
                let (p, s) = if v < 0.0 {
                    match neg.and_then(affixes) {
                        Some((p, _, s)) => (p, s),
                        None => (format!("-{prefix}"), suffix.clone()),
                    }
                } else {
                    (prefix.clone(), suffix.clone())
                };
                return Some(format!("{p}Infinity{s}"));
            }
            let s = format!("{:.*}", max_frac, v.abs());
            let negative = v < 0.0 && s.chars().any(|c| c.is_ascii_digit() && c != '0');
            (negative, s)
        }
    };
    let (int, frac) = match digits.split_once('.') {
        Some((i, f)) => (i.to_owned(), f.to_owned()),
        None => (digits, String::new()),
    };
    let mut frac = frac;
    while frac.len() < min_frac {
        frac.push('0');
    }
    while frac.len() > min_frac && frac.ends_with('0') {
        frac.pop();
    }
    let mut int = int.trim_start_matches('0').to_owned();
    while int.len() < min_int {
        int.insert(0, '0');
    }
    if int.is_empty() && frac.is_empty() {
        int.push('0');
    }
    let mut number = group_digits(&int, &positions, regular);
    if !frac.is_empty() {
        number.push('.');
        number.push_str(&frac);
    }
    let (prefix, suffix) = if negative {
        match neg.and_then(affixes) {
            Some((p, _, s)) => (p, s),
            None => (format!("-{prefix}"), suffix),
        }
    } else {
        (prefix, suffix)
    };
    Some(format!("{prefix}{number}{suffix}"))
}

fn roman(mut n: i64, upper: bool) -> String {
    const TABLE: &[(i64, &str)] = &[
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut out = String::new();
    for &(v, s) in TABLE {
        while n >= v {
            out.push_str(s);
            n -= v;
        }
    }
    if upper { out.to_uppercase() } else { out }
}

fn alphabetic(mut n: i64, upper: bool) -> String {
    let base = if upper { b'A' } else { b'a' };
    let mut out = Vec::new();
    while n > 0 {
        n -= 1;
        out.push((base + (n % 26) as u8) as char);
        n /= 26;
    }
    out.iter().rev().collect()
}

/// `fn:format-integer(value, picture)`: decimal digit pictures with grouping separators
/// (`001`, `#,##0`), `a` / `A` (alphabetic) and `i` / `I` (roman numerals). A modifier after
/// `;` is ignored.
fn format_integer(n: i64, picture: &str) -> Option<String> {
    let picture = picture.split(';').next()?;
    let sign = if n < 0 { "-" } else { "" };
    match picture {
        "a" | "A" if n > 0 => return Some(alphabetic(n, picture == "A")),
        "i" | "I" if (1..4000).contains(&n) => return Some(roman(n, picture == "I")),
        "a" | "A" | "i" | "I" => return Some(n.to_string()),
        _ => {}
    }
    let is_digit = |c: char| c.is_ascii_digit() || c == '#';
    if !picture.chars().any(|c| c.is_ascii_digit()) || picture.chars().any(|c| c.is_alphabetic()) {
        return None;
    }
    let min = picture.chars().filter(char::is_ascii_digit).count();
    let (positions, regular) = grouping(picture, is_digit);
    let mut digits = n.unsigned_abs().to_string();
    while digits.len() < min {
        digits.insert(0, '0');
    }
    Some(format!(
        "{sign}{}",
        group_digits(&digits, &positions, regular)
    ))
}

/// Java's `String.format` for `afn:sprintf`: `%s`, `%d`, `%f`, `%e`, `%x`, `%c`, `%%`, `%n`,
/// with argument indexes (`%2$s`), flags `-`, `0`, `+`, space and `,`, width and precision.
fn sprintf(format: &str, args: &[Term]) -> Option<String> {
    let mut out = String::new();
    let mut chars = format.chars().peekable();
    let mut next = 0;
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let mut spec = String::new();
        let conversion = loop {
            let c = chars.next()?;
            if c.is_ascii_alphabetic() || c == '%' {
                break c;
            }
            spec.push(c);
        };
        match conversion {
            '%' => {
                out.push('%');
                continue;
            }
            'n' => {
                out.push('\n');
                continue;
            }
            _ => {}
        }
        // [index$][flags][width][.precision]
        let (index, rest) = match spec.split_once('$') {
            Some((i, r)) => (Some(i.parse::<usize>().ok()?.checked_sub(1)?), r.to_owned()),
            None => (None, spec),
        };
        let flags: String = rest
            .chars()
            .take_while(|c| matches!(c, '-' | '0' | '+' | ' ' | ','))
            .collect();
        let rest = &rest[flags.len()..];
        let (width, precision) = match rest.split_once('.') {
            Some((w, p)) => (w, Some(p.parse::<usize>().ok()?)),
            None => (rest, None),
        };
        let width: usize = if width.is_empty() {
            0
        } else {
            width.parse().ok()?
        };
        let arg = match index {
            Some(i) => args.get(i)?,
            None => {
                next += 1;
                args.get(next - 1)?
            }
        };
        let sign = |negative: bool| {
            if negative {
                "-"
            } else if flags.contains('+') {
                "+"
            } else if flags.contains(' ') {
                " "
            } else {
                ""
            }
        };
        let (sign, body, numeric) = match conversion {
            's' | 'S' => {
                let s = match arg {
                    Term::BlankNode(b) => b.as_str(),
                    t => str_of(t)?,
                };
                let mut s: String = match precision {
                    Some(p) => s.chars().take(p).collect(),
                    None => s.to_owned(),
                };
                if conversion == 'S' {
                    s = s.to_uppercase();
                }
                ("", s, false)
            }
            'c' => {
                let c = match arg {
                    Term::Literal(l) if num(arg).is_none() => {
                        let mut it = l.value().chars();
                        let c = it.next()?;
                        it.next().is_none().then_some(c)?
                    }
                    _ => char::from_u32(u32::try_from(int_arg(arg)?).ok()?)?,
                };
                ("", c.to_string(), false)
            }
            'd' => {
                let n = int_arg(arg)?;
                let digits = n.unsigned_abs().to_string();
                let digits = if flags.contains(',') {
                    group_digits(&digits, &[(3, ',')], true)
                } else {
                    digits
                };
                (sign(n < 0), digits, true)
            }
            'x' | 'X' => {
                let n = int_arg(arg)?;
                let s = if n < 0 {
                    format!("{:x}", n as u64)
                } else {
                    format!("{n:x}")
                };
                let s = if conversion == 'X' {
                    s.to_uppercase()
                } else {
                    s
                };
                ("", s, true)
            }
            'f' => {
                let x = num(arg)?;
                let p = precision.unwrap_or(6);
                let s = match x.decimal() {
                    Some(d) => {
                        let r = round_decimal(d, p as i64, false)?.to_string();
                        let r = r.trim_start_matches('-');
                        let (i, f) = r.split_once('.').unwrap_or((r, ""));
                        let mut f = f.to_owned();
                        f.truncate(p);
                        while f.len() < p {
                            f.push('0');
                        }
                        if p == 0 {
                            i.to_owned()
                        } else {
                            format!("{i}.{f}")
                        }
                    }
                    None => format!("{:.*}", p, x.f64().abs()),
                };
                let negative = match x.decimal() {
                    Some(d) => d < Decimal::from(0),
                    None => x.f64() < 0.0,
                };
                let s = if flags.contains(',') {
                    let (i, f) = match s.split_once('.') {
                        Some((i, f)) => (i, Some(f)),
                        None => (s.as_str(), None),
                    };
                    let i = group_digits(i, &[(3, ',')], true);
                    match f {
                        Some(f) => format!("{i}.{f}"),
                        None => i,
                    }
                } else {
                    s
                };
                (sign(negative), s, true)
            }
            'e' | 'E' => {
                let x = num(arg)?.f64();
                let p = precision.unwrap_or(6);
                let s = format!("{:.*e}", p, x.abs());
                let (m, e) = s.split_once('e')?;
                let e: i32 = e.parse().ok()?;
                let s = format!("{m}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs());
                let s = if conversion == 'E' {
                    s.to_uppercase()
                } else {
                    s
                };
                (sign(x < 0.0), s, true)
            }
            _ => return None,
        };
        let len = sign.chars().count() + body.chars().count();
        let pad = width.saturating_sub(len);
        if flags.contains('-') {
            out.push_str(sign);
            out.push_str(&body);
            out.extend(std::iter::repeat_n(' ', pad));
        } else if flags.contains('0') && numeric {
            out.push_str(sign);
            out.extend(std::iter::repeat_n('0', pad));
            out.push_str(&body);
        } else {
            out.extend(std::iter::repeat_n(' ', pad));
            out.push_str(sign);
            out.push_str(&body);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// Dates, times and durations
// ---------------------------------------------------------------------------------------------

fn duration(t: &Term) -> Option<Duration> {
    let Term::Literal(l) = t else { return None };
    let dt = l.datatype();
    (dt == xsd::DURATION || dt == xsd::YEAR_MONTH_DURATION || dt == xsd::DAY_TIME_DURATION)
        .then(|| Duration::from_str(l.value()).ok())?
}

fn timezone_arg(args: &[Term]) -> Option<TimezoneOffset> {
    match args {
        [_] => Some(TimezoneOffset::UTC),
        [_, tz] => {
            let Term::Literal(l) = tz else { return None };
            if l.datatype() != xsd::DAY_TIME_DURATION {
                return None;
            }
            TimezoneOffset::try_from(DayTimeDuration::from_str(l.value()).ok()?).ok()
        }
        _ => None,
    }
}

/// `fn:adjust-dateTime-to-timezone`, `-date-`, `-time-`. Without a timezone argument, the
/// implicit timezone is UTC.
fn adjust(args: &[Term], datatype: NamedNode) -> Option<Term> {
    let tz = timezone_arg(args)?;
    let Term::Literal(l) = args.first()? else {
        return None;
    };
    if l.datatype() != datatype.as_ref() {
        return None;
    }
    let v = l.value();
    let out = if datatype.as_ref() == xsd::DATE_TIME {
        DateTime::from_str(v).ok()?.adjust(Some(tz))?.to_string()
    } else if datatype.as_ref() == xsd::DATE {
        Date::from_str(v).ok()?.adjust(Some(tz))?.to_string()
    } else {
        Time::from_str(v).ok()?.adjust(Some(tz))?.to_string()
    };
    Some(Literal::new_typed_literal(out, datatype).into())
}

/// `fn:dateTime(date, time)`.
fn date_time(args: &[Term]) -> Option<Term> {
    let [Term::Literal(d), Term::Literal(t)] = args else {
        return None;
    };
    if d.datatype() != xsd::DATE || t.datatype() != xsd::TIME {
        return None;
    }
    let date = Date::from_str(d.value()).ok()?;
    let time = Time::from_str(t.value()).ok()?;
    let tz = match (date.timezone_offset(), time.timezone_offset()) {
        (Some(a), Some(b)) if a != b => return None,
        (a, b) => a.or(b),
    };
    let second = time.second().to_string();
    let second = if time.second() < Decimal::from(10) {
        format!("0{second}")
    } else {
        second
    };
    let lexical = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{second}{}",
        date.year(),
        date.month(),
        date.day(),
        time.hour(),
        time.minute(),
        tz.map(|t| t.to_string()).unwrap_or_default()
    );
    let dt = DateTime::from_str(&lexical).ok()?;
    Some(Literal::new_typed_literal(dt.to_string(), xsd::DATE_TIME).into())
}

// ---------------------------------------------------------------------------------------------
// Casts
// ---------------------------------------------------------------------------------------------

/// A cast to an `xsd:integer` subtype, with its range check: from numbers (truncated),
/// booleans and strings.
fn integer_cast(t: &Term, min: i128, max: i128) -> Option<i128> {
    let Term::Literal(l) = t else { return None };
    let dt = l.datatype();
    let v: i128 = if let Some(n) = num(t) {
        match n {
            Num::Integer(i) => i64::from(i).into(),
            Num::Decimal(d) => i64::from(Integer::try_from(d).ok()?).into(),
            _ => {
                let x = n.f64().trunc();
                if !x.is_finite() || x.abs() > 1.8e19 {
                    return None;
                }
                x as i128
            }
        }
    } else if dt == xsd::BOOLEAN {
        match l.value() {
            "true" | "1" => 1,
            "false" | "0" => 0,
            _ => return None,
        }
    } else if dt == xsd::STRING {
        let s = l.value().trim();
        let s = s.strip_prefix('+').unwrap_or(s);
        if s.is_empty()
            || !s
                .trim_start_matches('-')
                .bytes()
                .all(|b| b.is_ascii_digit())
        {
            return None;
        }
        s.parse().ok()?
    } else {
        return None;
    };
    (min..=max).contains(&v).then_some(v)
}

/// `xsd:decimal(x)`, as SPARQL's cast but a double or a float becomes the decimal it prints as
/// (its shortest round-trip text, as Jena's `BigDecimal.valueOf`), not its binary expansion
/// cut at 18 decimals (#380): `xsd:decimal(xsd:double("0.518134715"))` is `0.518134715`, not
/// `0.518134714999999936`, so a `ROUND` after it falls the way it does on Jena.
fn decimal_cast(t: &Term) -> Option<Decimal> {
    let Term::Literal(l) = t else { return None };
    let dt = l.datatype();
    let shortest = |text: String, exact: Option<Decimal>| Decimal::from_str(&text).ok().or(exact);
    if dt == xsd::DOUBLE {
        let x = Double::from_str(l.value()).ok()?;
        let f = f64::from(x);
        if !f.is_finite() {
            return None;
        }
        shortest(format!("{f}"), Decimal::try_from(x).ok())
    } else if dt == xsd::FLOAT {
        let x = Float::from_str(l.value()).ok()?;
        let f = f32::from(x);
        if !f.is_finite() {
            return None;
        }
        shortest(format!("{f}"), Decimal::try_from(x).ok())
    } else if dt == xsd::DECIMAL || dt == xsd::STRING {
        l.value().parse().ok()
    } else if dt == xsd::INTEGER {
        Some(Integer::from_str(l.value()).ok()?.into())
    } else if dt == xsd::BOOLEAN {
        match l.value() {
            "true" | "1" => Some(1.into()),
            "false" | "0" => Some(0.into()),
            _ => None,
        }
    } else {
        None
    }
}

// ---------------------------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------------------------

fn func(f: impl Fn(&[Term]) -> Option<Term> + Send + Sync + 'static) -> Func {
    Arc::new(f)
}

fn math1(f: fn(f64) -> f64) -> Func {
    func(move |a| match a {
        [x] => double(f(f64_arg(x)?)),
        _ => None,
    })
}

fn string1(f: fn(&str) -> String) -> Func {
    func(move |a| match a {
        [s] => string_term(f(string(s)?)),
        _ => None,
    })
}

fn duration_part(part: fn(Duration) -> Term) -> Func {
    func(move |a| match a {
        [d] => Some(part(duration(d)?)),
        _ => None,
    })
}

/// Every custom function of this module, by IRI.
pub fn functions() -> Vec<(NamedNode, Func)> {
    let mut out: Vec<(String, Func)> = Vec::new();
    let mut add = |ns: &str, name: &str, f: Func| out.push((format!("{ns}{name}"), f));

    // math:
    add(
        MATH,
        "pi",
        func(|a| {
            a.is_empty()
                .then(|| Literal::from(std::f64::consts::PI).into())
        }),
    );
    add(MATH, "exp", math1(f64::exp));
    add(MATH, "exp10", math1(|x| 10_f64.powf(x)));
    add(MATH, "log", math1(f64::ln));
    add(MATH, "log10", math1(f64::log10));
    add(MATH, "sqrt", math1(f64::sqrt));
    add(MATH, "sin", math1(f64::sin));
    add(MATH, "cos", math1(f64::cos));
    add(MATH, "tan", math1(f64::tan));
    add(MATH, "asin", math1(f64::asin));
    add(MATH, "acos", math1(f64::acos));
    add(MATH, "atan", math1(f64::atan));
    add(
        MATH,
        "pow",
        func(|a| match a {
            [x, y] => double(f64_arg(x)?.powf(f64_arg(y)?)),
            _ => None,
        }),
    );
    add(
        MATH,
        "atan2",
        func(|a| match a {
            [y, x] => double(f64_arg(y)?.atan2(f64_arg(x)?)),
            _ => None,
        }),
    );

    // fn: strings
    add(FN, "normalize-space", string1(normalize_space));
    add(
        FN,
        "translate",
        func(|a| match a {
            [s, m, t] => string_term(translate(string(s)?, string(m)?, string(t)?)),
            _ => None,
        }),
    );
    add(
        FN,
        "codepoints-to-string",
        func(|a| {
            a.iter()
                .map(|c| char::from_u32(u32::try_from(int_arg(c)?).ok()?))
                .collect::<Option<String>>()
                .and_then(string_term)
        }),
    );
    add(
        FN,
        "compare",
        func(|a| match a {
            [x, y] => integer(match string(x)?.cmp(string(y)?) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }),
            _ => None,
        }),
    );
    add(FN, "escape-html-uri", string1(|s| escape_uri(s, false)));
    add(FN, "iri-to-uri", string1(|s| escape_uri(s, true)));
    add(
        FN,
        "string-join",
        // Jena's form: the strings, then the separator.
        func(|a| {
            let (sep, parts) = a.split_last()?;
            let parts = parts.iter().map(str_of).collect::<Option<Vec<_>>>()?;
            string_term(parts.join(string(sep)?))
        }),
    );

    // fn: numbers
    add(FN, "round", func(|a| round(a, false)));
    add(FN, "round-half-to-even", func(|a| round(a, true)));
    add(
        FN,
        "format-number",
        func(|a| match a {
            [x, p] => string_term(format_number(num(x)?, string(p)?)?),
            _ => None,
        }),
    );
    add(
        FN,
        "format-integer",
        func(|a| match a {
            [x, p] => string_term(format_integer(int_arg(x)?, string(p)?)?),
            _ => None,
        }),
    );
    add(FN, "min", func(|a| extreme(a, false)));
    add(FN, "max", func(|a| extreme(a, true)));
    add(FN, "numeric-mod", func(numeric_mod));
    add(FN, "numeric-integer-divide", func(integer_divide));
    add(FN, "error", func(|_| None));

    // fn: dates, times and durations
    add(
        FN,
        "years-from-duration",
        duration_part(|d| Literal::from(d.years()).into()),
    );
    add(
        FN,
        "months-from-duration",
        duration_part(|d| Literal::from(d.months()).into()),
    );
    add(
        FN,
        "days-from-duration",
        duration_part(|d| Literal::from(d.days()).into()),
    );
    add(
        FN,
        "hours-from-duration",
        duration_part(|d| Literal::from(d.hours()).into()),
    );
    add(
        FN,
        "minutes-from-duration",
        duration_part(|d| Literal::from(d.minutes()).into()),
    );
    add(
        FN,
        "seconds-from-duration",
        duration_part(|d| Literal::from(d.seconds()).into()),
    );
    add(
        FN,
        "adjust-dateTime-to-timezone",
        func(|a| adjust(a, xsd::DATE_TIME.into_owned())),
    );
    add(
        FN,
        "adjust-date-to-timezone",
        func(|a| adjust(a, xsd::DATE.into_owned())),
    );
    add(
        FN,
        "adjust-time-to-timezone",
        func(|a| adjust(a, xsd::TIME.into_owned())),
    );
    add(FN, "dateTime", func(date_time));
    add(
        FN,
        "implicit-timezone",
        func(|a| {
            a.is_empty()
                .then(|| Literal::new_typed_literal("PT0S", xsd::DAY_TIME_DURATION).into())
        }),
    );

    // afn: (both namespaces)
    let mut afn: Vec<(&str, Func)> = vec![
        (
            "localname",
            func(|a| match a {
                [Term::NamedNode(n)] => string_term(split_iri(n.as_str()).1),
                _ => None,
            }),
        ),
        (
            "namespace",
            func(|a| match a {
                [Term::NamedNode(n)] => string_term(split_iri(n.as_str()).0),
                _ => None,
            }),
        ),
        (
            "bnode",
            func(|a| match a {
                [Term::BlankNode(b)] => string_term(b.as_str()),
                _ => None,
            }),
        ),
        (
            "sprintf",
            func(|a| {
                let (format, args) = a.split_first()?;
                string_term(sprintf(str_of(format)?, args)?)
            }),
        ),
        (
            "strjoin",
            func(|a| {
                let (sep, parts) = a.split_first()?;
                let parts = parts.iter().map(str_of).collect::<Option<Vec<_>>>()?;
                string_term(parts.join(str_of(sep)?))
            }),
        ),
        ("min", func(|a| (a.len() == 2).then(|| extreme(a, false))?)),
        ("max", func(|a| (a.len() == 2).then(|| extreme(a, true))?)),
        (
            "pi",
            func(|a| {
                a.is_empty()
                    .then(|| Literal::from(std::f64::consts::PI).into())
            }),
        ),
        (
            "e",
            func(|a| {
                a.is_empty()
                    .then(|| Literal::from(std::f64::consts::E).into())
            }),
        ),
        ("sqrt", math1(f64::sqrt)),
    ];
    for name in ["substr", "substring"] {
        afn.push((
            name,
            func(|a| {
                let (s, start, end) = match a {
                    [s, start] => (s, start, None),
                    [s, start, end] => (s, start, Some(int_arg(end)?)),
                    _ => return None,
                };
                string_term(java_substring(str_of(s)?, int_arg(start)?, end)?)
            }),
        ));
    }
    for (name, f) in afn {
        add(AFN, name, Arc::clone(&f));
        add(AFN_OLD, name, f);
    }

    // xsd: casts Jena accepts and SPARQL 1.1 does not define.
    const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
    let casts: &[(&str, i128, i128)] = &[
        ("long", i64::MIN.into(), i64::MAX.into()),
        ("int", i32::MIN.into(), i32::MAX.into()),
        ("short", i16::MIN.into(), i16::MAX.into()),
        ("byte", i8::MIN.into(), i8::MAX.into()),
        ("nonNegativeInteger", 0, i64::MAX.into()),
        ("positiveInteger", 1, i64::MAX.into()),
        ("nonPositiveInteger", i64::MIN.into(), 0),
        ("negativeInteger", i64::MIN.into(), -1),
        ("unsignedLong", 0, u64::MAX.into()),
        ("unsignedInt", 0, u32::MAX.into()),
        ("unsignedShort", 0, u16::MAX.into()),
        ("unsignedByte", 0, u8::MAX.into()),
    ];
    for &(name, min, max) in casts {
        let datatype = NamedNode::new_unchecked(format!("{XSD}{name}"));
        add(
            XSD,
            name,
            func(move |a| match a {
                [x] => Some(
                    Literal::new_typed_literal(
                        integer_cast(x, min, max)?.to_string(),
                        datatype.clone(),
                    )
                    .into(),
                ),
                _ => None,
            }),
        );
    }
    // A dateTime with a timezone (#379).
    add(
        XSD,
        "dateTimeStamp",
        func(|a| match a {
            [Term::Literal(l)]
                if [xsd::STRING, xsd::DATE_TIME, xsd::DATE_TIME_STAMP].contains(&l.datatype()) =>
            {
                let value = DateTime::from_str(l.value().trim()).ok()?;
                value.timezone_offset()?;
                Some(Literal::new_typed_literal(value.to_string(), xsd::DATE_TIME_STAMP).into())
            }
            _ => None,
        }),
    );
    add(
        XSD,
        "decimal",
        func(|a| match a {
            // As Jena, which casts by lexical form when it is valid for the target type: a
            // string, an integer or a decimal keeps its text ("3" stays "3"); a double, a float
            // or a boolean gets the form of a computed decimal ("1500.0" for 1.5e3).
            [x] => {
                let d = decimal_cast(x)?;
                Some(match x {
                    Term::Literal(l)
                        if l.datatype() == xsd::STRING
                            || l.datatype() == xsd::INTEGER
                            || l.datatype() == xsd::DECIMAL =>
                    {
                        Literal::new_typed_literal(l.value(), xsd::DECIMAL).into()
                    }
                    _ => spareval::decimal_literal(d).into(),
                })
            }
            _ => None,
        }),
    );
    add(
        XSD,
        "anyURI",
        func(|a| match a {
            [Term::NamedNode(n)] => {
                Some(Literal::new_typed_literal(n.as_str(), xsd::ANY_URI).into())
            }
            [Term::Literal(l)] if l.datatype() == xsd::STRING || l.datatype() == xsd::ANY_URI => {
                Some(Literal::new_typed_literal(l.value().trim(), xsd::ANY_URI).into())
            }
            _ => None,
        }),
    );

    out.into_iter()
        .map(|(n, f)| (NamedNode::new_unchecked(n), f))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Num {
        Num::Decimal(Decimal::from_str(s).unwrap())
    }

    #[test]
    fn rounds_half_to_even_and_up() {
        let r = |s, p, even| round_decimal(Decimal::from_str(s).unwrap(), p, even).unwrap();
        assert_eq!(r("2.5", 0, true).to_string(), "2");
        assert_eq!(r("3.5", 0, true).to_string(), "4");
        assert_eq!(r("2.5", 0, false).to_string(), "3");
        assert_eq!(r("-2.5", 0, false).to_string(), "-2");
        assert_eq!(r("1.125", 2, true).to_string(), "1.12");
        assert_eq!(r("1250", -2, true).to_string(), "1200");
        assert_eq!(r("1350", -2, true).to_string(), "1400");
        assert_eq!(round_f64(2.5, 0, true), 2.0);
        assert_eq!(round_f64(3.567812e3, 2, true), 3567.81);
    }

    #[test]
    fn formats_numbers_with_pictures() {
        let f = |x: Num, p| format_number(x, p).unwrap();
        assert_eq!(f(d("12345.6"), "#,###.00"), "12,345.60");
        assert_eq!(f(d("0.125"), "0.00"), "0.12");
        assert_eq!(f(d("0.135"), "0.00"), "0.14");
        assert_eq!(f(d("-6"), "000"), "-006");
        assert_eq!(f(d("-6"), "#;(#)"), "(6)");
        assert_eq!(f(d("0.14"), "01%"), "14%");
        assert_eq!(f(d("1234567"), "#,##0"), "1,234,567");
        assert_eq!(f(d("1234.5"), "#.##"), "1234.5");
        assert_eq!(f(d("0.5"), "#"), "0");
        assert_eq!(
            f(Num::Double(Double::from(1.0e6)), "#,##0.0"),
            "1,000,000.0"
        );
        assert_eq!(f(Num::Double(Double::from(f64::NAN)), "#"), "NaN");
        assert_eq!(f(Num::Double(Double::from(-0.001)), "0.00"), "0.00");
        assert_eq!(f(Num::Double(Double::from(f64::INFINITY)), "#"), "Infinity");
    }

    #[test]
    fn formats_integers() {
        assert_eq!(format_integer(7, "001").unwrap(), "007");
        assert_eq!(format_integer(1234567, "#,##0").unwrap(), "1,234,567");
        assert_eq!(format_integer(-42, "0").unwrap(), "-42");
        assert_eq!(format_integer(28, "a").unwrap(), "ab");
        assert_eq!(format_integer(1999, "I").unwrap(), "MCMXCIX");
        assert!(format_integer(3, "w").is_none());
    }

    #[test]
    fn sprintf_follows_java() {
        let s = |f, a: &[Term]| sprintf(f, a).unwrap();
        let i = |n: i64| Term::from(Literal::from(n));
        let x = |v: f64| Term::from(Literal::from(v));
        let st = |v: &str| Term::from(Literal::new_simple_literal(v));
        assert_eq!(s("%s-%05d", &[st("km"), i(42)]), "km-00042");
        assert_eq!(
            s("%.2f|%8.3f|%-6s|", &[x(1.23456), x(-2.5), st("ab")]),
            "1.23|  -2.500|ab    |"
        );
        assert_eq!(
            s("%,d %x %X %%", &[i(1234567), i(255), i(255)]),
            "1,234,567 ff FF %"
        );
        assert_eq!(s("%2$s %1$s", &[st("a"), st("b")]), "b a");
        assert_eq!(s("%e", &[x(12345.678)]), "1.234568e+04");
        assert_eq!(
            s(
                "%+d %s",
                &[i(5), Term::from(NamedNode::new_unchecked("http://e/x"))]
            ),
            "+5 http://e/x"
        );
        assert!(sprintf("%d", &[st("x")]).is_none());
        assert!(sprintf("%s %s", &[st("x")]).is_none());
    }

    #[test]
    fn splits_iris_like_jena() {
        assert_eq!(split_iri("http://ex.org/a#b"), ("http://ex.org/a#", "b"));
        assert_eq!(
            split_iri("http://ex.org/path/name"),
            ("http://ex.org/path/", "name")
        );
        assert_eq!(split_iri("http://ex.org/1abc"), ("http://ex.org/1", "abc"));
        assert_eq!(split_iri("urn:x:y-z.1"), ("urn:x:", "y-z.1"));
        assert_eq!(split_iri("http://ex.org/"), ("http://ex.org/", ""));
    }

    #[test]
    fn casts_to_integer_subtypes() {
        let st = |v: &str| Term::from(Literal::new_simple_literal(v));
        let x = |v: f64| Term::from(Literal::from(v));
        assert_eq!(integer_cast(&st(" 42 "), 0, 255), Some(42));
        assert_eq!(integer_cast(&st("256"), 0, 255), None);
        assert_eq!(integer_cast(&x(-3.9), -128, 127), Some(-3));
        assert_eq!(integer_cast(&st("4.5"), -128, 127), None);
        assert_eq!(
            integer_cast(&Term::from(Literal::from(true)), 0, 1),
            Some(1)
        );
    }

    #[test]
    fn translates_and_escapes() {
        assert_eq!(translate("bar", "abc", "ABC"), "BAr");
        assert_eq!(translate("--aaa--", "abc-", "ABC"), "AAA");
        assert_eq!(normalize_space("  a \t b\n c "), "a b c");
        assert_eq!(escape_uri("http://e/é x", false), "http://e/%C3%A9 x");
        assert_eq!(escape_uri("http://e/é x", true), "http://e/%C3%A9%20x");
        assert_eq!(java_substring("abcdef", 1, Some(3)).unwrap(), "bc");
        assert!(java_substring("abc", 2, Some(5)).is_none());
    }
}
