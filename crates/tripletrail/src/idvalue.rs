//! Numbers and dates read straight from inline term ids (#170): the values of integers,
//! plain decimals and dates the store keeps in the id (#125), and the arithmetic, functions
//! and comparisons on them that FILTERs and `SUM` use.
//!
//! Everything here follows the evaluator exactly or gives up (`None`): integer overflow,
//! doubles (their sums depend on the order of the additions), dictionary literals (`xsd:int`,
//! exponent forms), dates compared with date-times or with and without a time zone. The
//! caller then leaves the expression to the evaluator.

use oxigraph::model::{Literal, vocab::xsd};
use oxigraph::sparql::Variable;
use spargebra::algebra::{Expression, Function};
use std::cmp::Ordering;
use tripletrail_store::{Kind, Snapshot, TermId};

use crate::idtree::Table;

/// Fraction digits of the evaluator's decimals (`oxsdatatypes::Decimal`).
const SCALE: u32 = 18;
const ONE: i128 = 10_i128.pow(SCALE);
const MS_PER_DAY: i64 = 86_400_000;

/// A value computed on ids.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Value {
    Integer(i64),
    /// A decimal times 10^18, as the evaluator holds it.
    Decimal(i128),
    DateTime {
        /// `xsd:date`, else `xsd:dateTime`.
        date: bool,
        /// Milliseconds since 1970 (the local time read as UTC without a zone).
        instant: i64,
        tz: Option<i16>,
    },
}

/// Evaluates on a row: `Some(Ok(v))` a value, `Some(Err(()))` an error (unbound), `None`
/// give up.
pub(crate) type ValueKernel<'k> = Box<dyn Fn(&[TermId]) -> Option<Result<Value, ()>> + Sync + 'k>;

/// The value of an id, if the ids decide it.
pub(crate) fn of_id(id: TermId) -> Option<Value> {
    match id.kind() {
        Kind::Integer => id.integer_value().map(Value::Integer),
        Kind::Decimal => match id.numeric()? {
            tripletrail_store::Numeric::Decimal { digits, scale } => Some(Value::Decimal(
                digits.checked_mul(10_i128.pow(SCALE - u32::from(scale)))?,
            )),
            tripletrail_store::Numeric::Double(_) => None,
        },
        Kind::DateTime => {
            let v = id.date_time_value()?;
            Some(Value::DateTime {
                date: v.date,
                instant: v.instant,
                tz: v.tz,
            })
        }
        _ => None,
    }
}

/// Compiles `e` to a kernel over the rows of `t`, if every part of it is decided on ids.
pub(crate) fn compile<'k>(
    e: &Expression,
    t: &Table,
    snapshot: &'k Snapshot,
) -> Option<ValueKernel<'k>> {
    use Expression as E;
    Some(match e {
        E::Variable(v) => column(t, v),
        E::Literal(l) => {
            let value = constant(l, snapshot)?;
            Box::new(move |_| Some(Ok(value)))
        }
        E::Add(a, b) | E::Subtract(a, b) | E::Multiply(a, b) => {
            let op = match e {
                E::Add(..) => Op::Add,
                E::Subtract(..) => Op::Subtract,
                _ => Op::Multiply,
            };
            let (a, b) = (compile(a, t, snapshot)?, compile(b, t, snapshot)?);
            Box::new(move |row| {
                let (x, y) = match (a(row)?, b(row)?) {
                    (Ok(x), Ok(y)) => (x, y),
                    _ => return Some(Err(())),
                };
                arithmetic(op, x, y).map(Ok)
            })
        }
        E::UnaryMinus(a) => {
            let a = compile(a, t, snapshot)?;
            Box::new(move |row| {
                Some(Ok(match a(row)? {
                    Ok(Value::Integer(i)) => Value::Integer(i.checked_neg()?),
                    Ok(Value::Decimal(d)) => Value::Decimal(d.checked_neg()?),
                    Ok(_) => return None,
                    Err(()) => return Some(Err(())),
                }))
            })
        }
        // `STRLEN(?v)` of a string, read from its dictionary key (#165). Anything else (an
        // error for a non-string) is left to the evaluator.
        E::FunctionCall(Function::StrLen, args) => {
            let [a] = args.as_slice() else {
                return None;
            };
            let (v, part) = text_part(a)?;
            let c = t.col(v);
            Box::new(move |row| {
                let id = row[c?];
                if id == TermId(0) {
                    return Some(Err(()));
                }
                let n = string_of(snapshot, id, |s| part.length(s))?;
                Some(Ok(Value::Integer(i64::try_from(n).ok()?)))
            })
        }
        // `xsd:integer(CONTAINS(?v, "x"))` and the other text tests: 1 or 0.
        E::FunctionCall(Function::Custom(f), args) if f.as_ref() == xsd::INTEGER => {
            let [a] = args.as_slice() else { return None };
            let (v, test) = crate::idtree::text_test(a)?;
            let c = t.col(v);
            Box::new(move |row| {
                let id = row[c?];
                if id == TermId(0) {
                    return Some(Err(()));
                }
                let is = string_of(snapshot, id, |s| test.matches(s))?;
                Some(Ok(Value::Integer(i64::from(is))))
            })
        }
        E::FunctionCall(f, args) => {
            let [a] = args.as_slice() else { return None };
            let f = match f {
                Function::Abs => Unary::Abs,
                Function::Ceil => Unary::Ceil,
                Function::Floor => Unary::Floor,
                Function::Round => Unary::Round,
                Function::Year => Unary::Year,
                Function::Month => Unary::Month,
                Function::Day => Unary::Day,
                Function::Hours => Unary::Hours,
                Function::Minutes => Unary::Minutes,
                _ => return None,
            };
            let a = compile(a, t, snapshot)?;
            Box::new(move |row| match a(row)? {
                Ok(x) => unary(f, x).map(Ok),
                Err(()) => Some(Err(())),
            })
        }
        _ => return None,
    })
}

/// The part of a string's text whose length `STRLEN` takes: the text itself, or what
/// `STRBEFORE`, `STRAFTER`, `UCASE` or `LCASE` makes of it (#165).
pub(crate) enum TextPart {
    Whole,
    Before(String),
    After(String),
    Upper,
    Lower,
}

impl TextPart {
    /// The length in characters of this part of `text`.
    fn length(&self, text: &str) -> usize {
        match self {
            Self::Whole => text.chars().count(),
            // No match gives the empty string.
            Self::Before(x) => text
                .find(x.as_str())
                .map_or(0, |at| text[..at].chars().count()),
            Self::After(x) => text
                .find(x.as_str())
                .map_or(0, |at| text[at + x.len()..].chars().count()),
            Self::Upper => text.to_uppercase().chars().count(),
            Self::Lower => text.to_lowercase().chars().count(),
        }
    }
}

/// `?v`, `STRBEFORE(?v, "x")`, `STRAFTER(?v, "x")`, `UCASE(?v)` or `LCASE(?v)`: the variable
/// and the part. The second argument is a simple literal, compatible with every string, so
/// each is an error only for a value that is not a string (left to the evaluator).
pub(crate) fn text_part(e: &Expression) -> Option<(&Variable, TextPart)> {
    use Expression as E;
    let simple = |e: &Expression| match e {
        E::Literal(l) if l.language().is_none() && l.datatype() == xsd::STRING => {
            Some(l.value().to_owned())
        }
        _ => None,
    };
    Some(match e {
        E::Variable(v) => (v, TextPart::Whole),
        E::FunctionCall(f, args) => match (f, args.as_slice()) {
            (Function::StrBefore, [E::Variable(v), x]) => (v, TextPart::Before(simple(x)?)),
            (Function::StrAfter, [E::Variable(v), x]) => (v, TextPart::After(simple(x)?)),
            (Function::UCase, [E::Variable(v)]) => (v, TextPart::Upper),
            (Function::LCase, [E::Variable(v)]) => (v, TextPart::Lower),
            _ => return None,
        },
        _ => return None,
    })
}

/// `f` of the value of a string or language-tagged string; `None` for any other term.
fn string_of<T>(snapshot: &Snapshot, id: TermId, f: impl Fn(&str) -> T) -> Option<T> {
    if !matches!(id.kind(), Kind::String | Kind::LangString) {
        return None;
    }
    if let Some(s) = snapshot.reader().string_value(id) {
        return Some(f(s));
    }
    // An inline or ephemeral string.
    match snapshot.term(id).ok()? {
        oxigraph::model::Term::Literal(l) => Some(f(l.value())),
        _ => None,
    }
}

fn column<'k>(t: &Table, v: &Variable) -> ValueKernel<'k> {
    match t.col(v) {
        Some(c) => Box::new(move |row| {
            let id = row[c];
            if id == TermId(0) {
                return Some(Err(()));
            }
            of_id(id).map(Ok)
        }),
        None => Box::new(|_| Some(Err(()))),
    }
}

/// A literal constant, through the id the store gives it (inline numbers and dates only).
fn constant(l: &Literal, snapshot: &Snapshot) -> Option<Value> {
    // Only canonical-looking constants: the store inlines those with the exact lexical form.
    let d = l.datatype();
    if d != xsd::INTEGER && d != xsd::DECIMAL && d != xsd::DATE && d != xsd::DATE_TIME {
        return None;
    }
    of_id(snapshot.term_id(l.as_ref().into())?)
}

#[derive(Clone, Copy)]
enum Op {
    Add,
    Subtract,
    Multiply,
}

#[derive(Clone, Copy)]
enum Unary {
    Abs,
    Ceil,
    Floor,
    Round,
    Year,
    Month,
    Day,
    Hours,
    Minutes,
}

/// `x op y` with the evaluator's promotion: integer with integer is an integer, anything
/// with a decimal a decimal. Overflow, an inexact product and other types give up.
fn arithmetic(op: Op, x: Value, y: Value) -> Option<Value> {
    match (x, y) {
        (Value::Integer(a), Value::Integer(b)) => Some(Value::Integer(match op {
            Op::Add => a.checked_add(b)?,
            Op::Subtract => a.checked_sub(b)?,
            Op::Multiply => a.checked_mul(b)?,
        })),
        (Value::Integer(_) | Value::Decimal(_), Value::Integer(_) | Value::Decimal(_)) => {
            let (a, b) = (decimal(x)?, decimal(y)?);
            Some(Value::Decimal(match op {
                Op::Add => a.checked_add(b)?,
                Op::Subtract => a.checked_sub(b)?,
                Op::Multiply => {
                    let p = a.checked_mul(b)?;
                    if p % ONE != 0 {
                        return None;
                    }
                    p / ONE
                }
            }))
        }
        _ => None,
    }
}

fn decimal(v: Value) -> Option<i128> {
    match v {
        Value::Integer(i) => i128::from(i).checked_mul(ONE),
        Value::Decimal(d) => Some(d),
        _ => None,
    }
}

fn unary(f: Unary, x: Value) -> Option<Value> {
    match (f, x) {
        (Unary::Abs, Value::Integer(i)) => Some(Value::Integer(i.checked_abs()?)),
        (Unary::Abs, Value::Decimal(d)) => Some(Value::Decimal(d.checked_abs()?)),
        (Unary::Ceil | Unary::Floor | Unary::Round, Value::Integer(i)) => Some(Value::Integer(i)),
        (Unary::Ceil, Value::Decimal(d)) => Some(Value::Decimal(
            d.div_euclid(ONE)
                .checked_add(i128::from(d.rem_euclid(ONE) != 0))?
                .checked_mul(ONE)?,
        )),
        (Unary::Floor, Value::Decimal(d)) => {
            Some(Value::Decimal(d.div_euclid(ONE).checked_mul(ONE)?))
        }
        // Halves round up (towards positive infinity), as fn:round.
        (Unary::Round, Value::Decimal(d)) => Some(Value::Decimal(
            d.checked_add(ONE / 2)?.div_euclid(ONE).checked_mul(ONE)?,
        )),
        (Unary::Year | Unary::Month | Unary::Day, Value::DateTime { instant, tz, .. }) => {
            let local = instant.checked_add(i64::from(tz.unwrap_or(0)) * 60_000)?;
            let (y, m, d) = civil(local.div_euclid(MS_PER_DAY));
            Some(Value::Integer(match f {
                Unary::Year => y,
                Unary::Month => m,
                _ => d,
            }))
        }
        (
            Unary::Hours | Unary::Minutes,
            Value::DateTime {
                date: false,
                instant,
                tz,
            },
        ) => {
            let local = instant.checked_add(i64::from(tz.unwrap_or(0)) * 60_000)?;
            let ms = local.rem_euclid(MS_PER_DAY);
            Some(Value::Integer(match f {
                Unary::Hours => ms / 3_600_000,
                _ => ms / 60_000 % 60,
            }))
        }
        _ => None,
    }
}

/// Year, month and day of the day `days` after 1970-01-01 (proleptic Gregorian).
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// How `x` compares with `y` for `=`, `<` and the like: `Some(Some(o))` an order,
/// `Some(None)` an error (values that don't compare), `None` give up.
pub(crate) fn compare(x: Value, y: Value) -> Option<Option<Ordering>> {
    match (x, y) {
        (Value::Integer(a), Value::Integer(b)) => Some(Some(a.cmp(&b))),
        (Value::Integer(_) | Value::Decimal(_), Value::Integer(_) | Value::Decimal(_)) => {
            Some(Some(decimal(x)?.cmp(&decimal(y)?)))
        }
        (
            Value::DateTime {
                date: d1,
                instant: a,
                tz: t1,
            },
            Value::DateTime {
                date: d2,
                instant: b,
                tz: t2,
            },
        ) if d1 == d2 && t1.is_some() == t2.is_some() => Some(Some(a.cmp(&b))),
        _ => None,
    }
}

/// The running `SUM` of a group: `None` once it gives up.
#[derive(Clone, Copy)]
pub(crate) enum Sum {
    Integer(i64),
    Decimal(i128),
}

impl Default for Sum {
    fn default() -> Self {
        Sum::Integer(0)
    }
}

impl Sum {
    pub(crate) fn add(self, v: Value) -> Option<Self> {
        Some(match (self, v) {
            (Sum::Integer(a), Value::Integer(b)) => Sum::Integer(a.checked_add(b)?),
            (Sum::Integer(a), Value::Decimal(b)) => {
                Sum::Decimal(i128::from(a).checked_mul(ONE)?.checked_add(b)?)
            }
            (Sum::Decimal(a), Value::Integer(_) | Value::Decimal(_)) => {
                Sum::Decimal(a.checked_add(decimal(v)?)?)
            }
            _ => return None,
        })
    }

    /// The literal the evaluator gives for this sum.
    pub(crate) fn literal(self) -> Option<Literal> {
        Some(match self {
            Sum::Integer(i) => Literal::from(i),
            Sum::Decimal(d) => Literal::new_typed_literal(decimal_lexical(d), xsd::DECIMAL),
        })
    }
}

/// The canonical form of a decimal times 10^18, as the evaluator writes it: no exponent, no
/// trailing fraction zeros, no point for a whole number.
fn decimal_lexical(d: i128) -> String {
    let negative = d < 0;
    let magnitude = d.unsigned_abs();
    let whole = magnitude / ONE.unsigned_abs();
    let fraction = magnitude % ONE.unsigned_abs();
    let mut s = String::new();
    if negative {
        s.push('-');
    }
    s.push_str(&whole.to_string());
    if fraction != 0 {
        let digits = format!("{fraction:018}");
        s.push('.');
        s.push_str(digits.trim_end_matches('0'));
    } else {
        // Jena's form of a whole computed decimal (see `spareval::decimal_lexical`).
        s.push_str(".0");
    }
    s
}

/// A filter kernel for `a op b` on numbers and dates.
pub(crate) fn comparison<'k>(
    e: &Expression,
    t: &Table,
    snapshot: &'k Snapshot,
) -> Option<crate::idtree::Kernel<'k>> {
    use Expression as E;
    let (a, b, test): (_, _, fn(Ordering) -> bool) = match e {
        E::Equal(a, b) => (a, b, Ordering::is_eq),
        E::Greater(a, b) => (a, b, Ordering::is_gt),
        E::GreaterOrEqual(a, b) => (a, b, Ordering::is_ge),
        E::Less(a, b) => (a, b, Ordering::is_lt),
        E::LessOrEqual(a, b) => (a, b, Ordering::is_le),
        _ => return None,
    };
    let (a, b) = (compile(a, t, snapshot)?, compile(b, t, snapshot)?);
    Some(Box::new(move |row| {
        let (x, y) = match (a(row)?, b(row)?) {
            (Ok(x), Ok(y)) => (x, y),
            _ => return Some(None),
        };
        Some(compare(x, y)?.map(test))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(-1), (1969, 12, 31));
        assert_eq!(civil(11_016), (2000, 2, 29));
        assert_eq!(civil(-719_162), (1, 1, 1));
    }

    #[test]
    fn decimals_print_as_the_evaluator_does() {
        assert_eq!(decimal_lexical(0), "0.0");
        assert_eq!(decimal_lexical(5 * ONE), "5.0");
        assert_eq!(decimal_lexical(-15 * ONE / 10), "-1.5");
        assert_eq!(decimal_lexical(ONE / 100), "0.01");
    }

    #[test]
    fn rounding() {
        let d = |x: f64| Value::Decimal((x * 1e18) as i128);
        assert_eq!(unary(Unary::Round, d(2.5)), Some(d(3.0)));
        assert_eq!(unary(Unary::Round, d(-2.5)), Some(d(-2.0)));
        assert_eq!(unary(Unary::Ceil, d(-2.5)), Some(d(-2.0)));
        assert_eq!(unary(Unary::Floor, d(-2.5)), Some(d(-3.0)));
    }
}
