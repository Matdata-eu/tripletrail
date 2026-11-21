//! Numbers and dates read straight from inline term ids (#170): the values of integers,
//! plain decimals and dates the store keeps in the id (#125), and the arithmetic, functions
//! and comparisons on them that FILTERs and `SUM` use.
//!
//! Everything here follows the evaluator exactly or gives up (`None`): integer overflow,
//! doubles that are not finite, dictionary literals (`xsd:int`, exponent forms),
//! dates compared with date-times or with and without a time zone. The caller then leaves the
//! expression to the evaluator. The one difference is `SUM` and `AVG` over doubles (#62): the
//! evaluator adds them in row order, rounding after each addition, while these are summed
//! exactly and rounded once, so they don't depend on the order of the rows.

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
    /// An `xsd:double`, always finite: an inline one, or computed from one (#11).
    Double(f64),
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
        Kind::Double => match id.numeric()? {
            tripletrail_store::Numeric::Double(d) => Some(Value::Double(d)),
            tripletrail_store::Numeric::Decimal { .. } => None,
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

/// The literal the evaluator gives for a value it computed (`BIND(?a * 1.21 AS ?b)`), if
/// the ids decide it: dates and date-times are left to the evaluator.
pub(crate) fn literal(v: Value) -> Option<Literal> {
    Some(match v {
        Value::Integer(i) => Literal::from(i),
        Value::Decimal(d) => Literal::new_typed_literal(decimal_lexical(d), xsd::DECIMAL),
        Value::Double(d) => Literal::from(oxsdatatypes::Double::from(d)),
        Value::DateTime { .. } => return None,
    })
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
                    Ok(Value::Double(d)) => Value::Double(-d),
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

/// A literal constant: a number by its value, a date through the id the store gives it
/// (inline dates only).
fn constant(l: &Literal, snapshot: &Snapshot) -> Option<Value> {
    let d = l.datatype();
    if d == xsd::INTEGER {
        return l
            .value()
            .parse::<oxsdatatypes::Integer>()
            .ok()
            .map(|i| Value::Integer(i64::from(i)));
    }
    if d == xsd::DECIMAL {
        let v: oxsdatatypes::Decimal = l.value().parse().ok()?;
        return Some(Value::Decimal(i128::from_be_bytes(v.to_be_bytes())));
    }
    if d == xsd::DOUBLE {
        let v: oxsdatatypes::Double = l.value().parse().ok()?;
        let v = f64::from(v);
        return v.is_finite().then_some(Value::Double(v));
    }
    if d != xsd::INTEGER
        && d != xsd::DECIMAL
        && d != xsd::DOUBLE
        && d != xsd::DATE
        && d != xsd::DATE_TIME
    {
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
/// with a decimal a decimal, anything with a double a double. Overflow, an inexact product,
/// a double that is not finite and other types give up.
fn arithmetic(op: Op, x: Value, y: Value) -> Option<Value> {
    match (x, y) {
        (Value::Double(_), _) | (_, Value::Double(_)) => {
            let (a, b) = (double(x)?, double(y)?);
            let r = match op {
                Op::Add => a + b,
                Op::Subtract => a - b,
                Op::Multiply => a * b,
            };
            r.is_finite().then_some(Value::Double(r))
        }
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
                Op::Multiply => scaled_product(a, b)?,
            }))
        }
        _ => None,
    }
}

/// `a * b / 10^18` for decimals times 10^18, exact: the product is taken on 256 bits, so
/// large factors don't overflow it. `None` when it doesn't fit, or has more than 18 fraction
/// digits (the evaluator's would be cut).
fn scaled_product(a: i128, b: i128) -> Option<i128> {
    let negative = (a < 0) != (b < 0);
    let (x, y) = (a.unsigned_abs(), b.unsigned_abs());
    let (x0, x1) = (x as u64 as u128, (x >> 64) as u64 as u128);
    let (y0, y1) = (y as u64 as u128, (y >> 64) as u64 as u128);
    // The product in four 64-bit limbs, lowest first.
    let mut limbs = [0u64; 4];
    for (i, xi) in [x0, x1].into_iter().enumerate() {
        let mut carry = 0u128;
        for (j, yj) in [y0, y1].into_iter().enumerate() {
            let t = xi * yj + u128::from(limbs[i + j]) + carry;
            limbs[i + j] = t as u64;
            carry = t >> 64;
        }
        let mut k = i + 2;
        while carry != 0 && k < 4 {
            let t = u128::from(limbs[k]) + carry;
            limbs[k] = t as u64;
            carry = t >> 64;
            k += 1;
        }
    }
    // Divided by 10^18, from the highest limb down.
    let d = ONE as u128;
    let mut quotient = [0u64; 4];
    let mut rest = 0u128;
    for k in (0..4).rev() {
        let current = (rest << 64) | u128::from(limbs[k]);
        quotient[k] = (current / d) as u64;
        rest = current % d;
    }
    if rest != 0 || quotient[2] != 0 || quotient[3] != 0 {
        return None;
    }
    let q = (u128::from(quotient[1]) << 64) | u128::from(quotient[0]);
    let q = i128::try_from(q).ok()?;
    Some(if negative { -q } else { q })
}

/// A number as a double, converted as the evaluator converts it (`oxsdatatypes`).
fn double(v: Value) -> Option<f64> {
    Some(match v {
        Value::Integer(i) => f64::from(oxsdatatypes::Double::from(oxsdatatypes::Integer::from(i))),
        Value::Decimal(d) => f64::from(oxsdatatypes::Double::from(
            oxsdatatypes::Decimal::from_be_bytes(d.to_be_bytes()),
        )),
        Value::Double(d) => d,
        Value::DateTime { .. } => return None,
    })
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
        // Finite doubles: always ordered (`-0` equals `0`).
        (Value::Double(_), Value::Integer(_) | Value::Decimal(_) | Value::Double(_))
        | (Value::Integer(_) | Value::Decimal(_), Value::Double(_)) => {
            Some(double(x)?.partial_cmp(&double(y)?))
        }
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

/// The running `SUM` (and `AVG`) of a group: `None` once it gives up. Integers and decimals
/// add up exactly, as the evaluator does; once a double comes in, the sum is a double: the
/// exact sum of the doubles plus the total of the integers and decimals (itself rounded to a
/// double first), rounded once at the end.
#[derive(Clone, Default)]
pub(crate) struct Sum {
    exact: Exact,
    doubles: Option<Fsum>,
}

#[derive(Clone, Copy)]
enum Exact {
    Integer(i64),
    Decimal(i128),
}

impl Default for Exact {
    fn default() -> Self {
        Exact::Integer(0)
    }
}

impl Sum {
    pub(crate) fn add(&mut self, v: Value) -> Option<()> {
        self.exact = match (self.exact, v) {
            (Exact::Integer(a), Value::Integer(b)) => Exact::Integer(a.checked_add(b)?),
            (Exact::Integer(a), Value::Decimal(b)) => {
                Exact::Decimal(i128::from(a).checked_mul(ONE)?.checked_add(b)?)
            }
            (Exact::Decimal(a), Value::Integer(_) | Value::Decimal(_)) => {
                Exact::Decimal(a.checked_add(decimal(v)?)?)
            }
            (exact, Value::Double(d)) => {
                self.doubles.get_or_insert_with(Fsum::default).add(d)?;
                exact
            }
            _ => return None,
        };
        Some(())
    }

    /// The value of the sum, as a double once a double came in.
    fn total(&self) -> Option<Total> {
        Some(match (&self.doubles, self.exact) {
            (None, Exact::Integer(i)) => Total::Integer(i),
            (None, Exact::Decimal(d)) => Total::Decimal(d),
            (Some(f), exact) => {
                let mut f = f.clone();
                // The integers and decimals as one double, correctly rounded, as the
                // evaluator turns each into a double when it adds it to a double.
                let rest: f64 = match exact {
                    Exact::Integer(i) => i.to_string().parse().ok()?,
                    Exact::Decimal(d) => decimal_lexical(d).parse().ok()?,
                };
                f.add(rest)?;
                Total::Double(f.value())
            }
        })
    }

    /// The literal the evaluator gives for this sum.
    pub(crate) fn literal(&self) -> Option<Literal> {
        Some(match self.total()? {
            Total::Integer(i) => Literal::from(i),
            Total::Decimal(d) => Literal::new_typed_literal(decimal_lexical(d), xsd::DECIMAL),
            Total::Double(d) => Literal::from(oxsdatatypes::Double::from(d)),
        })
    }

    /// The literal the evaluator gives for the average of `count` values with this sum: only
    /// for doubles (an average of integers and decimals is a decimal division, left to the
    /// evaluator).
    pub(crate) fn average(&self, count: usize) -> Option<Literal> {
        if count == 0 {
            return Some(Literal::from(0_i64));
        }
        match self.total()? {
            Total::Double(d) => Some(Literal::from(oxsdatatypes::Double::from(d / count as f64))),
            _ => None,
        }
    }
}

enum Total {
    Integer(i64),
    Decimal(i128),
    Double(f64),
}

/// An exact sum of doubles, rounded once at the end (Shewchuk's partials, as Python's
/// `math.fsum`): the result doesn't depend on the order of the additions.
#[derive(Clone, Default)]
struct Fsum {
    /// Non-overlapping partial sums, smallest first; their exact sum is the sum so far.
    partials: Vec<f64>,
    /// The sum of the infinite and NaN values, which make the result one of them.
    special: Option<f64>,
}

impl Fsum {
    /// Adds `x`; `None` when an intermediate sum overflows (the evaluator's order may not).
    fn add(&mut self, mut x: f64) -> Option<()> {
        if !x.is_finite() {
            self.special = Some(self.special.map_or(x, |s| s + x));
            return Some(());
        }
        let mut i = 0;
        for j in 0..self.partials.len() {
            let mut y = self.partials[j];
            if x.abs() < y.abs() {
                std::mem::swap(&mut x, &mut y);
            }
            let hi = x + y;
            if !hi.is_finite() {
                return None;
            }
            let lo = y - (hi - x);
            if lo != 0.0 {
                self.partials[i] = lo;
                i += 1;
            }
            x = hi;
        }
        self.partials.truncate(i);
        self.partials.push(x);
        Some(())
    }

    /// The exact sum, correctly rounded (half to even).
    fn value(&self) -> f64 {
        if let Some(s) = self.special {
            return s;
        }
        let p = &self.partials;
        let mut n = p.len();
        let mut hi = 0.0;
        if n > 0 {
            n -= 1;
            hi = p[n];
            let mut lo = 0.0;
            while n > 0 {
                let x = hi;
                n -= 1;
                let y = p[n];
                hi = x + y;
                let yr = hi - x;
                lo = y - yr;
                if lo != 0.0 {
                    break;
                }
            }
            // Half-way between two doubles: round as if the partials below were added.
            if n > 0 && ((lo < 0.0 && p[n - 1] < 0.0) || (lo > 0.0 && p[n - 1] > 0.0)) {
                let y = lo * 2.0;
                let x = hi + y;
                if y == x - hi {
                    hi = x;
                }
            }
        }
        // The evaluator's sum starts at the integer 0: `0 + -0.0` is `0.0`.
        hi + 0.0
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
    fn decimal_products_are_exact() {
        let d = |s: &str| {
            let v: oxsdatatypes::Decimal = s.parse().unwrap();
            i128::from_be_bytes(v.to_be_bytes())
        };
        for (a, b) in [
            ("1057.1", "1.5"),
            ("-1057.1", "1.21"),
            ("123456789012.345", "-98765.4321"),
            ("0.000000001", "0.000000001"),
            ("99999999999999999999", "1.7"),
            ("0", "-5.5"),
        ] {
            let expected: oxsdatatypes::Decimal = a
                .parse::<oxsdatatypes::Decimal>()
                .unwrap()
                .checked_mul(b.parse::<oxsdatatypes::Decimal>().unwrap())
                .unwrap();
            assert_eq!(
                scaled_product(d(a), d(b)),
                Some(i128::from_be_bytes(expected.to_be_bytes())),
                "{a} * {b}"
            );
        }
        // More than 18 fraction digits, or too large: left to the evaluator.
        assert_eq!(scaled_product(d("0.000000001"), d("0.0000000001")), None);
        assert_eq!(
            scaled_product(d("10000000000000000000"), d("100000000000000000000")),
            None
        );
    }

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
    fn double_sums_are_exact_and_rounded_once() {
        let sum = |xs: &[f64]| {
            let mut s = Sum::default();
            for &x in xs {
                s.add(Value::Double(x)).unwrap();
            }
            s.literal().unwrap().value().to_owned()
        };
        // Left to right, 0.1 + 0.2 + 0.3 is 0.6000000000000001.
        assert_eq!(sum(&[0.1, 0.2, 0.3]), "0.6");
        assert_eq!(sum(&[0.3, 0.2, 0.1]), "0.6");
        // Left to right, 1e16 + 1 + 1 is 1e16: the ones are lost one by one.
        assert_eq!(sum(&[1e16, 1.0, 1.0]), sum(&[1.0, 1.0, 1e16]));
        assert_eq!(sum(&[1e100, 1.0, -1e100]), "1");
        assert_eq!(sum(&[-0.0]), "0");
        assert_eq!(sum(&[1.5, f64::INFINITY]), "INF");
        assert_eq!(sum(&[f64::INFINITY, f64::NEG_INFINITY]), "NaN");
        assert_eq!(sum(&[f64::NAN, 1.0]), "NaN");
        // Just above half-way between two doubles: up, where left to right gives 1.
        assert_eq!(
            sum(&[1.0, 1.1102230246251565e-16, 1e-300]),
            "1.0000000000000002"
        );
        // An overflow on the way: left to the evaluator.
        let mut s = Sum::default();
        s.add(Value::Double(f64::MAX)).unwrap();
        assert!(s.add(Value::Double(f64::MAX)).is_none());
        // Integers and decimals with doubles: a double.
        let mut s = Sum::default();
        for v in [
            Value::Integer(2),
            Value::Decimal(ONE / 2),
            Value::Double(0.25),
        ] {
            s.add(v).unwrap();
        }
        assert_eq!(s.literal().unwrap().value(), "2.75");
        assert_eq!(s.literal().unwrap().datatype(), xsd::DOUBLE);
        assert_eq!(s.average(2).unwrap().value(), "1.375");
        // An average of integers is a decimal division: not here.
        let mut s = Sum::default();
        s.add(Value::Integer(3)).unwrap();
        assert!(s.average(2).is_none());
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
