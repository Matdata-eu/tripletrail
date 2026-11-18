//! Statistical aggregates (#190): `tt:median`, `tt:percentile(?x, p)`, `tt:stddev`,
//! `tt:stddevPop`, `tt:variance`, `tt:varPop`, `tt:mode`, `tt:first`, `tt:last` and
//! `tt:topK(?x, k)`, with Jena's `agg:` names where Jena has one.
//!
//! The numeric ones follow SUM and AVG: integers and decimals are computed exactly as
//! decimals, a float or double among the values makes the result a float or double, and a
//! value that is not a number makes the aggregate unbound (an error, as for SUM). Only the
//! square root of a standard deviation is always a double. An empty group, or one value for a
//! sample variance or deviation, gives no value.
//!
//! Constant arguments (`tt:percentile(?x, 0.95)`) reach the aggregate through
//! [`ARGUMENTS`]: the parser makes the aggregate's expression a call of that function with
//! the value and the arguments, and the function packs them into one literal.

use crate::ttfn::MG;
use oxigraph::model::vocab::xsd;
use oxigraph::model::{Literal, NamedNode, Term};
use oxsdatatypes::{Decimal, Double};
use spareval::AggregateFunctionAccumulator;
use std::str::FromStr;

/// The function that packs an aggregate's value with its constant arguments.
pub const ARGUMENTS: &str = "http://w3id.org/tripletrail/fn#aggregateArguments";
/// The datatype of the packed literal: N-Triples terms, one per line.
const PACKED: &str = "http://w3id.org/tripletrail/fn#AggregateArguments";
/// Jena's aggregate namespace.
const JENA_AGG: &str = "http://jena.apache.org/ARQ/function/aggregate#";

type Factory = fn() -> Box<dyn AggregateFunctionAccumulator + Send + Sync>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Median,
    Percentile,
    Stddev,
    StddevPop,
    Variance,
    VarPop,
    Mode,
    First,
    Last,
    TopK,
}

/// The aggregates, by name.
pub fn aggregates() -> Vec<(NamedNode, Factory)> {
    fn make(kind: Kind) -> Box<dyn AggregateFunctionAccumulator + Send + Sync> {
        Box::new(Accumulator {
            kind,
            numbers: Vec::new(),
            terms: Vec::new(),
            argument: None,
            failed: false,
        })
    }
    let mg = |name: &str| NamedNode::new_unchecked(format!("{MG}{name}"));
    let jena = |name: &str| NamedNode::new_unchecked(format!("{JENA_AGG}{name}"));
    vec![
        (mg("median"), || make(Kind::Median)),
        (mg("percentile"), || make(Kind::Percentile)),
        (mg("stddev"), || make(Kind::Stddev)),
        (mg("stddevPop"), || make(Kind::StddevPop)),
        (mg("variance"), || make(Kind::Variance)),
        (mg("varPop"), || make(Kind::VarPop)),
        (mg("mode"), || make(Kind::Mode)),
        (mg("first"), || make(Kind::First)),
        (mg("last"), || make(Kind::Last)),
        (mg("topK"), || make(Kind::TopK)),
        (jena("median"), || make(Kind::Median)),
        (jena("mode"), || make(Kind::Mode)),
        (jena("stdev"), || make(Kind::Stddev)),
        (jena("stdev_samp"), || make(Kind::Stddev)),
        (jena("stdev_pop"), || make(Kind::StddevPop)),
        (jena("variance"), || make(Kind::Variance)),
        (jena("var_samp"), || make(Kind::Variance)),
        (jena("var_pop"), || make(Kind::VarPop)),
    ]
}

/// The aggregates that take constant arguments after their expression.
pub fn taking_arguments() -> [NamedNode; 2] {
    [
        NamedNode::new_unchecked(format!("{MG}percentile")),
        NamedNode::new_unchecked(format!("{MG}topK")),
    ]
}

/// [`ARGUMENTS`]: its arguments as one literal.
pub fn pack(args: &[Term]) -> Option<Term> {
    let text: Vec<String> = args.iter().map(ToString::to_string).collect();
    Some(Literal::new_typed_literal(text.join("\n"), NamedNode::new_unchecked(PACKED)).into())
}

fn unpack(term: &Term) -> Option<Vec<Term>> {
    let Term::Literal(l) = term else {
        return None;
    };
    if l.datatype().as_str() != PACKED {
        return None;
    }
    l.value()
        .split('\n')
        .map(|t| Term::from_str(t).ok())
        .collect()
}

/// A number, at its place in the numeric type promotion of SUM and AVG.
#[derive(Clone, Copy)]
pub(crate) enum Number {
    /// An integer (`true`) or a decimal.
    Exact(Decimal, bool),
    Float(f64),
    Double(f64),
}

pub(crate) fn number(term: &Term) -> Option<Number> {
    let Term::Literal(l) = term else {
        return None;
    };
    let dt = l.datatype();
    let v = l.value();
    if dt == xsd::DECIMAL {
        return Decimal::from_str(v).ok().map(|d| Number::Exact(d, false));
    }
    if dt == xsd::FLOAT {
        return f32::from_str(v.trim())
            .ok()
            .or(match v.trim() {
                "INF" => Some(f32::INFINITY),
                "-INF" => Some(f32::NEG_INFINITY),
                "NaN" => Some(f32::NAN),
                _ => None,
            })
            .map(|f| Number::Float(f64::from(f)));
    }
    if dt == xsd::DOUBLE {
        return Double::from_str(v).ok().map(|d| Number::Double(d.into()));
    }
    if is_integer(dt.as_str()) {
        return oxsdatatypes::Integer::from_str(v)
            .ok()
            .map(|i| Number::Exact(Decimal::from(i), true));
    }
    None
}

fn is_integer(dt: &str) -> bool {
    dt.strip_prefix("http://www.w3.org/2001/XMLSchema#")
        .is_some_and(|name| {
            matches!(
                name,
                "integer"
                    | "long"
                    | "int"
                    | "short"
                    | "byte"
                    | "nonNegativeInteger"
                    | "nonPositiveInteger"
                    | "positiveInteger"
                    | "negativeInteger"
                    | "unsignedLong"
                    | "unsignedInt"
                    | "unsignedShort"
                    | "unsignedByte"
            )
        })
}

/// The result type of a group: 0 exact, 2 float, 3 double.
fn level(numbers: &[Number]) -> u8 {
    numbers.iter().fold(0, |l, n| {
        l.max(match n {
            Number::Exact(..) => 0,
            Number::Float(_) => 2,
            Number::Double(_) => 3,
        })
    })
}

pub(crate) fn as_f64(n: Number) -> f64 {
    match n {
        Number::Exact(d, _) => Double::from(d).into(),
        Number::Float(f) | Number::Double(f) => f,
    }
}

/// A computed decimal, in Jena's form ("3.0"), as AVG gives one.
pub(crate) fn decimal_term(d: Decimal) -> Term {
    spareval::decimal_literal(d).into()
}

pub(crate) fn float_term(f: f64, level: u8) -> Term {
    if level == 2 {
        oxsdatatypes::Float::from(f as f32).into_term()
    } else {
        Double::from(f).into_term()
    }
}

trait IntoTerm {
    fn into_term(self) -> Term;
}

impl IntoTerm for oxsdatatypes::Float {
    fn into_term(self) -> Term {
        Literal::new_typed_literal(self.to_string(), xsd::FLOAT).into()
    }
}

impl IntoTerm for Double {
    fn into_term(self) -> Term {
        Literal::new_typed_literal(self.to_string(), xsd::DOUBLE).into()
    }
}

struct Accumulator {
    kind: Kind,
    numbers: Vec<Number>,
    terms: Vec<Term>,
    /// The constant argument (`p` of a percentile, `k` of top-k), from the first value.
    argument: Option<Term>,
    failed: bool,
}

impl AggregateFunctionAccumulator for Accumulator {
    fn accumulate(&mut self, element: Term) {
        if self.failed {
            return;
        }
        let element = match unpack(&element) {
            Some(mut packed) if !packed.is_empty() => {
                let value = packed.remove(0);
                if self.argument.is_none() {
                    self.argument = packed.into_iter().next();
                }
                value
            }
            Some(_) => {
                self.failed = true;
                return;
            }
            None => element,
        };
        match self.kind {
            Kind::Mode => self.terms.push(element),
            Kind::First => {
                if self.terms.is_empty() {
                    self.terms.push(element);
                }
            }
            Kind::Last => {
                self.terms.clear();
                self.terms.push(element);
            }
            _ => match number(&element) {
                Some(n) => self.numbers.push(n),
                None => self.failed = true,
            },
        }
    }

    fn finish(&mut self) -> Option<Term> {
        if self.failed {
            return None;
        }
        match self.kind {
            Kind::First | Kind::Last => self.terms.first().cloned(),
            Kind::Mode => mode(&self.terms),
            Kind::Median => percentile(&mut self.numbers, Decimal::from_str("0.5").ok()?),
            Kind::Percentile => {
                let p = match number(self.argument.as_ref()?)? {
                    Number::Exact(d, _) => d,
                    n => Decimal::try_from(Double::from(as_f64(n))).ok()?,
                };
                if p < Decimal::from(0) || p > Decimal::from(1) {
                    return None;
                }
                percentile(&mut self.numbers, p)
            }
            Kind::Variance => variance(&self.numbers, true).map(|v| v.0),
            Kind::VarPop => variance(&self.numbers, false).map(|v| v.0),
            Kind::Stddev | Kind::StddevPop => {
                let (_, f) = variance(&self.numbers, self.kind == Kind::Stddev)?;
                Some(Double::from(f.sqrt()).into_term())
            }
            Kind::TopK => top_k(&mut self.numbers, self.argument.as_ref()?),
        }
    }
}

/// The most frequent term; of equally frequent ones the first seen.
fn mode(terms: &[Term]) -> Option<Term> {
    let mut counts: Vec<(&Term, usize)> = Vec::new();
    let mut index = std::collections::HashMap::new();
    for t in terms {
        let i = *index.entry(t).or_insert_with(|| {
            counts.push((t, 0));
            counts.len() - 1
        });
        counts[i].1 += 1;
    }
    let max = counts.iter().map(|c| c.1).max()?;
    counts.into_iter().find(|c| c.1 == max).map(|c| c.0.clone())
}

fn to_usize(d: Decimal) -> Option<usize> {
    usize::try_from(i64::from(oxsdatatypes::Integer::try_from(d).ok()?)).ok()
}

fn sort(numbers: &mut [Number]) {
    numbers.sort_by(|a, b| match (a, b) {
        (Number::Exact(x, _), Number::Exact(y, _)) => x.cmp(y),
        _ => as_f64(*a).total_cmp(&as_f64(*b)),
    });
}

/// The continuous percentile (linear interpolation between the closest ranks, as SQL's
/// PERCENTILE_CONT).
fn percentile(numbers: &mut [Number], p: Decimal) -> Option<Term> {
    if numbers.is_empty() {
        return None;
    }
    let level = level(numbers);
    sort(numbers);
    let last = numbers.len() - 1;
    if level == 0 {
        let at = |i: usize| match numbers[i] {
            Number::Exact(d, _) => d,
            _ => unreachable!(),
        };
        let pos = p.checked_mul(Decimal::from(i64::try_from(last).ok()?))?;
        let lo = pos.checked_floor()?;
        let lo_i = to_usize(lo)?.min(last);
        let hi_i = (lo_i + 1).min(last);
        let frac = pos.checked_sub(lo)?;
        let (a, b) = (at(lo_i), at(hi_i));
        let value = a.checked_add(b.checked_sub(a)?.checked_mul(frac)?)?;
        return Some(decimal_term(value));
    }
    let pos = f64::from(Double::from(p)) * last as f64;
    let lo = (pos.floor() as usize).min(last);
    let hi = (lo + 1).min(last);
    let (a, b) = (as_f64(numbers[lo]), as_f64(numbers[hi]));
    Some(float_term(a + (b - a) * (pos - lo as f64), level))
}

/// The variance (`sample`: divided by n - 1), as a term and as a double. Integers and
/// decimals give an exact decimal, or a double when the exact computation overflows (as Jena,
/// which always gives a double).
fn variance(numbers: &[Number], sample: bool) -> Option<(Term, f64)> {
    let n = numbers.len();
    if n == 0 || (sample && n < 2) {
        return None;
    }
    let mut level = level(numbers);
    let divisor = if sample { n - 1 } else { n };
    if level == 0 {
        if let Some(v) = exact_variance(numbers, divisor) {
            return Some((decimal_term(v), Double::from(v).into()));
        }
        level = 3;
    }
    let values: Vec<f64> = numbers.iter().map(|x| as_f64(*x)).collect();
    let mean = values.iter().sum::<f64>() / n as f64;
    let v = values.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / divisor as f64;
    Some((float_term(v, level), v))
}

/// The variance of integers and decimals as a decimal; `None` on overflow.
fn exact_variance(numbers: &[Number], divisor: usize) -> Option<Decimal> {
    let values: Vec<Decimal> = numbers
        .iter()
        .map(|x| match x {
            Number::Exact(d, _) => Some(*d),
            _ => None,
        })
        .collect::<Option<_>>()?;
    let count = Decimal::from(i64::try_from(numbers.len()).ok()?);
    let mut sum = Decimal::from(0);
    for v in &values {
        sum = sum.checked_add(*v)?;
    }
    let mean = sum.checked_div(count)?;
    let mut squares = Decimal::from(0);
    for v in &values {
        let d = v.checked_sub(mean)?;
        squares = squares.checked_add(d.checked_mul(d)?)?;
    }
    squares.checked_div(Decimal::from(i64::try_from(divisor).ok()?))
}

/// The `k` largest values, largest first, separated by spaces (as GROUP_CONCAT).
fn top_k(numbers: &mut [Number], k: &Term) -> Option<Term> {
    let k = match number(k)? {
        Number::Exact(d, true) => to_usize(d)?,
        _ => return None,
    };
    sort(numbers);
    let level = level(numbers);
    let text: Vec<String> = numbers
        .iter()
        .rev()
        .take(k)
        .map(|n| match (n, level) {
            (Number::Exact(d, _), 0) => d.to_string(),
            (n, _) => match float_term(as_f64(*n), level) {
                Term::Literal(l) => l.value().to_owned(),
                _ => unreachable!(),
            },
        })
        .collect();
    Some(Literal::new_simple_literal(text.join(" ")).into())
}
