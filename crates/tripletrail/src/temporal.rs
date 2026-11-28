//! Periods (#300): one value for every way of writing time, Allen's relations and the
//! period functions of the `tt:` namespace (`http://w3id.org/tripletrail/fn#`).
//!
//! A period is a half-open interval `[start, end)` on a timeline of whole milliseconds (UTC),
//! each side possibly open (unbounded), as PostgreSQL canonicalises discrete ranges and
//! SQL:2011 defines `PERIOD`. An instant `t` is `[t, t + 1 ms)`; finer fractions are
//! truncated. A date, a month (`xsd:gYearMonth`) and a year (`xsd:gYear`) are their whole span.
//!
//! Values without a timezone are read at an offset: UTC unless the call gives another
//! (`tt:period(x, "+01:00")`) or the engine has a default ([`crate::Engine::set_time_offset`]).
//! The functions read the engine's offset when they are called. A call is rewritten before
//! evaluation (see [`rewrite`]) so that an unbound variable given for a side of
//! `tt:period(start, end)` is passed as an open side.

use crate::xfn::Func;
use oxigraph::model::vocab::xsd;
use oxigraph::model::{Literal, NamedNode, Term};
use oxsdatatypes::{DayTimeDuration, Decimal};
use spargebra::algebra::{Expression, Function};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

const TT: &str = crate::ttfn::MG;
/// The datatype of periods: `http://w3id.org/tripletrail/fn#period`.
pub const PERIOD: &str = "http://w3id.org/tripletrail/fn#period";
/// What a rewritten call passes for an unbound side of `tt:period(start, end)`.
const OPEN: &str = "http://w3id.org/tripletrail/fn#openSide";

/// The largest year a period may name, before or after year 0.
const MAX_YEAR: i64 = 999_999;
const MS_PER_DAY: i64 = 86_400_000;
/// The offset limit of XSD: 14 hours either way.
const MAX_OFFSET: i64 = 14 * 60;

/// A half-open interval of milliseconds since 1970-01-01T00:00:00Z; `None` is an open side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Period {
    pub start: Option<i64>,
    pub end: Option<i64>,
}

impl Period {
    pub(crate) fn new(start: Option<i64>, end: Option<i64>) -> Option<Self> {
        match (start, end) {
            (Some(s), Some(e)) if s >= e => None,
            _ => Some(Self { start, end }),
        }
    }

    /// The sides as numbers, an open start before and an open end after every instant.
    fn bounds(self) -> (i64, i64) {
        (self.start.unwrap_or(i64::MIN), self.end.unwrap_or(i64::MAX))
    }

    fn is_instant(self) -> bool {
        matches!((self.start, self.end), (Some(s), Some(e)) if e - s == 1)
    }

    /// The canonical lexical form: UTC stamps, `..` for an open side.
    pub fn lexical(self) -> String {
        let side = |s: Option<i64>| s.map_or_else(|| "..".to_owned(), stamp);
        format!("{}/{}", side(self.start), side(self.end))
    }

    pub(crate) fn term(self) -> Term {
        Literal::new_typed_literal(self.lexical(), NamedNode::new_unchecked(PERIOD)).into()
    }
}

// ---------------------------------------------------------------------------------------------
// The calendar (proleptic Gregorian, year 0 is 1 BCE as in XSD 1.1)
// ---------------------------------------------------------------------------------------------

/// Days from 1970-01-01 to the date (H. Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of a day number (`civil_from_days`).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn is_leap(y: i64) -> bool {
    y.rem_euclid(4) == 0 && (y.rem_euclid(100) != 0 || y.rem_euclid(400) == 0)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// An instant as an `xsd:dateTimeStamp` lexical form in UTC, with milliseconds when it has
/// some.
pub(crate) fn stamp(ms: i64) -> String {
    let days = ms.div_euclid(MS_PER_DAY);
    let in_day = ms.rem_euclid(MS_PER_DAY);
    let (y, m, d) = civil_from_days(days);
    let (h, rest) = (in_day / 3_600_000, in_day % 3_600_000);
    let (min, rest) = (rest / 60_000, rest % 60_000);
    let (s, milli) = (rest / 1000, rest % 1000);
    let year = if y < 0 {
        format!("-{:04}", -y)
    } else {
        format!("{y:04}")
    };
    let fraction = if milli == 0 {
        String::new()
    } else {
        format!(".{milli:03}").trim_end_matches('0').to_owned()
    };
    format!("{year}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}{fraction}Z")
}

// ---------------------------------------------------------------------------------------------
// Lexical forms
// ---------------------------------------------------------------------------------------------

/// What a lexical form names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    DateTime,
    Date,
    YearMonth,
    Year,
}

/// The span a value names, `[start, end)` in local milliseconds, and its own offset.
struct Span {
    kind: Kind,
    start: i64,
    end: i64,
    offset: Option<i64>,
}

struct Cursor<'a> {
    s: &'a [u8],
    i: usize,
}

impl Cursor<'_> {
    fn done(&self) -> bool {
        self.i == self.s.len()
    }
    fn eat(&mut self, c: u8) -> bool {
        if self.s.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    /// Exactly `n` digits.
    fn digits(&mut self, n: usize) -> Option<i64> {
        let part = self.s.get(self.i..self.i + n)?;
        if !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        self.i += n;
        Some(part.iter().fold(0, |a, d| a * 10 + i64::from(d - b'0')))
    }
    /// A year: an optional `-`, four digits or more (no leading zero past four).
    fn year(&mut self) -> Option<i64> {
        let negative = self.eat(b'-');
        let from = self.i;
        while self.s.get(self.i).is_some_and(u8::is_ascii_digit) {
            self.i += 1;
        }
        let digits = &self.s[from..self.i];
        if digits.len() < 4 || (digits.len() > 4 && digits[0] == b'0') || digits.len() > 6 {
            return None;
        }
        let y = digits
            .iter()
            .fold(0i64, |a, d| a * 10 + i64::from(d - b'0'));
        (y <= MAX_YEAR).then_some(if negative { -y } else { y })
    }
    /// An optional timezone at the end: `Z` or `±hh:mm`, in minutes.
    fn timezone(&mut self) -> Option<Option<i64>> {
        if self.done() {
            return Some(None);
        }
        let offset = offset_of(std::str::from_utf8(&self.s[self.i..]).ok()?)?;
        self.i = self.s.len();
        Some(Some(offset))
    }
}

/// `Z` or `±hh:mm` (at most 14:00), in minutes east of UTC.
fn offset_of(s: &str) -> Option<i64> {
    if s == "Z" {
        return Some(0);
    }
    let b = s.as_bytes();
    if b.len() != 6 || b[3] != b':' {
        return None;
    }
    let sign = match b[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let mut c = Cursor { s: b, i: 1 };
    let h = c.digits(2)?;
    c.i += 1;
    let m = c.digits(2)?;
    let total = h * 60 + m;
    (m < 60 && total <= MAX_OFFSET).then_some(sign * total)
}

fn parse_as(s: &str, kind: Kind) -> Option<Span> {
    let mut c = Cursor {
        s: s.as_bytes(),
        i: 0,
    };
    let y = c.year()?;
    let (m, d) = match kind {
        Kind::Year => (1, 1),
        _ => {
            if !c.eat(b'-') {
                return None;
            }
            let m = c.digits(2)?;
            if !(1..=12).contains(&m) {
                return None;
            }
            if kind == Kind::YearMonth {
                (m, 1)
            } else {
                if !c.eat(b'-') {
                    return None;
                }
                let d = c.digits(2)?;
                if d < 1 || d > days_in_month(y, m) {
                    return None;
                }
                (m, d)
            }
        }
    };
    let day = days_from_civil(y, m, d) * MS_PER_DAY;
    let (start, end) = match kind {
        Kind::Year => (day, days_from_civil(y + 1, 1, 1) * MS_PER_DAY),
        Kind::YearMonth => {
            let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            (day, days_from_civil(ny, nm, 1) * MS_PER_DAY)
        }
        Kind::Date => (day, day + MS_PER_DAY),
        Kind::DateTime => {
            if !c.eat(b'T') {
                return None;
            }
            let h = c.digits(2)?;
            if !c.eat(b':') {
                return None;
            }
            let min = c.digits(2)?;
            if !c.eat(b':') {
                return None;
            }
            let sec = c.digits(2)?;
            let mut milli = 0;
            let mut fraction = false;
            if c.eat(b'.') {
                let from = c.i;
                while c.s.get(c.i).is_some_and(u8::is_ascii_digit) {
                    c.i += 1;
                }
                let digits = &c.s[from..c.i];
                if digits.is_empty() {
                    return None;
                }
                fraction = digits.iter().any(|d| *d != b'0');
                milli = digits
                    .iter()
                    .chain(std::iter::repeat(&b'0'))
                    .take(3)
                    .fold(0, |a, d| a * 10 + i64::from(d - b'0'));
            }
            if min > 59 || sec > 59 || h > 24 || (h == 24 && (min, sec, fraction) != (0, 0, false))
            {
                return None;
            }
            let t = day + ((h * 60 + min) * 60 + sec) * 1000 + milli;
            (t, t + 1)
        }
    };
    let offset = c.timezone()?;
    Some(Span {
        kind,
        start,
        end,
        offset,
    })
}

/// The span of a lexical form of any of the kinds.
fn parse_any(s: &str) -> Option<Span> {
    [Kind::DateTime, Kind::Date, Kind::YearMonth, Kind::Year]
        .into_iter()
        .find_map(|k| parse_as(s, k))
}

/// A span's sides in UTC milliseconds: its own offset, else `offset`.
fn utc(span: &Span, offset: i64) -> (i64, i64) {
    let shift = span.offset.unwrap_or(offset) * 60_000;
    (span.start - shift, span.end - shift)
}

/// A side of `start/end` or of `tt:period(start, end)`: the start of a start's span; for an
/// end, an instant itself (the end is not in the period), or the end of a date's, month's or
/// year's span (`2078-12-31` as the end includes that day).
fn side(span: &Span, offset: i64, is_end: bool) -> i64 {
    let (s, e) = utc(span, offset);
    match (is_end, span.kind) {
        (false, _) | (true, Kind::DateTime) => s,
        (true, _) => e,
    }
}

/// A period's lexical form, `start/end` with `..` (or nothing) for an open side.
fn parse_period(s: &str, offset: i64) -> Option<Period> {
    let (a, b) = s.split_once('/')?;
    let read = |text: &str, is_end: bool| -> Option<Option<i64>> {
        let text = text.trim();
        if text.is_empty() || text == ".." {
            return Some(None);
        }
        Some(Some(side(&parse_any(text)?, offset, is_end)))
    };
    Period::new(read(a, false)?, read(b, true)?)
}

/// The period of a temporal value: an `xsd:dateTime` or `xsd:dateTimeStamp` (an instant), an
/// `xsd:date`, `xsd:gYearMonth` or `xsd:gYear` (its whole span), a `tt:period`, or a string of
/// one of these.
pub fn period_of(t: &Term, offset: i64) -> Option<Period> {
    let Term::Literal(l) = t else { return None };
    let v = l.value();
    let dt = l.datatype();
    let span = |kind| {
        let span = parse_as(v, kind)?;
        let (s, e) = utc(&span, offset);
        Period::new(Some(s), Some(e))
    };
    if dt == xsd::DATE_TIME || dt == xsd::DATE_TIME_STAMP {
        if dt == xsd::DATE_TIME_STAMP && parse_as(v, Kind::DateTime)?.offset.is_none() {
            return None;
        }
        span(Kind::DateTime)
    } else if dt == xsd::DATE {
        span(Kind::Date)
    } else if dt == xsd::G_YEAR_MONTH {
        span(Kind::YearMonth)
    } else if dt == xsd::G_YEAR {
        span(Kind::Year)
    } else if dt.as_str() == PERIOD {
        parse_period(v, offset)
    } else if dt == xsd::STRING {
        let v = v.trim();
        if v.contains('/') {
            parse_period(v, offset)
        } else {
            let span = parse_any(v)?;
            let (s, e) = utc(&span, offset);
            Period::new(Some(s), Some(e))
        }
    } else {
        None
    }
}

/// A side given to `tt:period(start, end)`: `None` for an open one.
fn side_of(t: &Term, offset: i64, is_end: bool) -> Option<Option<i64>> {
    if let Term::NamedNode(n) = t
        && n.as_str() == OPEN
    {
        return Some(None);
    }
    let Term::Literal(l) = t else { return None };
    let dt = l.datatype();
    let v = l.value();
    let kind = if dt == xsd::DATE_TIME || dt == xsd::DATE_TIME_STAMP {
        Kind::DateTime
    } else if dt == xsd::DATE {
        Kind::Date
    } else if dt == xsd::G_YEAR_MONTH {
        Kind::YearMonth
    } else if dt == xsd::G_YEAR {
        Kind::Year
    } else if dt.as_str() == PERIOD || (dt == xsd::STRING && v.contains('/')) {
        // A period's own start, or its end.
        let p = period_of(t, offset)?;
        return Some(if is_end { p.end } else { p.start });
    } else if dt == xsd::STRING {
        let v = v.trim();
        if v.is_empty() || v == ".." {
            return Some(None);
        }
        return Some(Some(side(&parse_any(v)?, offset, is_end)));
    } else {
        return None;
    };
    let span = parse_as(v, kind)?;
    if dt == xsd::DATE_TIME_STAMP && span.offset.is_none() {
        return None;
    }
    Some(Some(side(&span, offset, is_end)))
}

/// A string argument that is an offset: `Z` or `±hh:mm`.
fn offset_arg(t: &Term) -> Option<i64> {
    let Term::Literal(l) = t else { return None };
    (l.datatype() == xsd::STRING).then(|| offset_of(l.value().trim()))?
}

// ---------------------------------------------------------------------------------------------
// The functions
// ---------------------------------------------------------------------------------------------

/// `tt:period(x [, offset])` and `tt:period(start, end [, offset])`.
fn period_fn(offset: i64, args: &[Term]) -> Option<Period> {
    match args {
        [x] => period_of(x, offset),
        [x, o] if offset_arg(o).is_some() && !is_open(o) => period_of(x, offset_arg(o)?),
        [s, e] => Period::new(side_of(s, offset, false)?, side_of(e, offset, true)?),
        [s, e, o] => {
            let offset = offset_arg(o)?;
            Period::new(side_of(s, offset, false)?, side_of(e, offset, true)?)
        }
        _ => None,
    }
}

fn is_open(t: &Term) -> bool {
    matches!(t, Term::NamedNode(n) if n.as_str() == OPEN)
}

/// Allen's relations, and the two checks most queries need, by name.
pub const RELATIONS: [&str; 15] = [
    "periodBefore",
    "periodAfter",
    "periodMeets",
    "periodMetBy",
    "periodOverlaps",
    "periodOverlappedBy",
    "periodStarts",
    "periodStartedBy",
    "periodDuring",
    "periodStrictlyContains",
    "periodFinishes",
    "periodFinishedBy",
    "periodEquals",
    "periodIntersects",
    "periodContains",
];

/// Whether `relation` holds from `a` to `b`.
pub fn relation(relation: &str, a: Period, b: Period) -> Option<bool> {
    let ((as_, ae), (bs, be)) = (a.bounds(), b.bounds());
    Some(match relation {
        "periodBefore" => ae < bs,
        "periodAfter" => be < as_,
        "periodMeets" => ae == bs,
        "periodMetBy" => be == as_,
        "periodOverlaps" => as_ < bs && bs < ae && ae < be,
        "periodOverlappedBy" => bs < as_ && as_ < be && be < ae,
        "periodStarts" => as_ == bs && ae < be,
        "periodStartedBy" => as_ == bs && be < ae,
        "periodDuring" => bs < as_ && ae < be,
        "periodStrictlyContains" => as_ < bs && be < ae,
        "periodFinishes" => ae == be && bs < as_,
        "periodFinishedBy" => ae == be && as_ < bs,
        "periodEquals" => as_ == bs && ae == be,
        // SQL's OVERLAPS, PostgreSQL's `&&`: an instant in common.
        "periodIntersects" => as_ < be && bs < ae,
        // PostgreSQL's `@>`: every instant of `b` is in `a`.
        "periodContains" => as_ <= bs && be <= ae,
        _ => return None,
    })
}

fn boolean(b: bool) -> Term {
    Literal::from(b).into()
}

fn stamp_term(ms: i64) -> Term {
    Literal::new_typed_literal(stamp(ms), xsd::DATE_TIME_STAMP).into()
}

/// A function of this module, given the default offset (minutes) and the arguments.
type Implementation = Arc<dyn Fn(i64, &[Term]) -> Option<Term> + Send + Sync>;

/// The functions by their name in the `tt:` namespace, each taking the default offset first.
fn implementations() -> Vec<(&'static str, Implementation)> {
    type F = Implementation;
    let one = |f: fn(Period) -> Option<Term>| -> F {
        Arc::new(move |offset, args: &[Term]| match args {
            [x] => f(period_of(x, offset)?),
            _ => None,
        })
    };
    let mut out: Vec<(&'static str, F)> = vec![
        (
            "period",
            Arc::new(|offset, args: &[Term]| Some(period_fn(offset, args)?.term())),
        ),
        ("periodStart", one(|p| Some(stamp_term(p.start?)))),
        ("periodEnd", one(|p| Some(stamp_term(p.end?)))),
        (
            "periodDuration",
            one(|p| {
                let ms = p.end?.checked_sub(p.start?)?;
                let seconds = Decimal::new(i128::from(ms), 3).ok()?;
                Some(
                    Literal::new_typed_literal(
                        DayTimeDuration::new(seconds).to_string(),
                        xsd::DAY_TIME_DURATION,
                    )
                    .into(),
                )
            }),
        ),
        ("isOpenStart", one(|p| Some(boolean(p.start.is_none())))),
        ("isOpenEnd", one(|p| Some(boolean(p.end.is_none())))),
        ("isInstant", one(|p| Some(boolean(p.is_instant())))),
    ];
    for name in RELATIONS {
        out.push((
            name,
            Arc::new(move |offset, args: &[Term]| match args {
                [a, b] => Some(boolean(relation(
                    name,
                    period_of(a, offset)?,
                    period_of(b, offset)?,
                )?)),
                _ => None,
            }),
        ));
    }
    out
}

/// Every function of this module by IRI, reading a value without a timezone at `offset`
/// (minutes east of UTC) as it is when the function is called: the engine's
/// [`crate::Engine::set_time_offset`], so every way a query reaches the evaluator (SHACL
/// constraints and rules, user-defined functions, prepared queries) reads at the same offset.
pub fn functions(offset: Arc<AtomicI64>) -> Vec<(NamedNode, Func)> {
    implementations()
        .into_iter()
        .map(|(name, f)| {
            let offset = Arc::clone(&offset);
            let func: Func = Arc::new(move |args: &[Term]| f(offset.load(Ordering::Relaxed), args));
            (NamedNode::new_unchecked(format!("{TT}{name}")), func)
        })
        .collect()
}

/// `tt:period(start, end [, offset])` with each variable given for a side passed as an open
/// side when it is unbound: `IF(BOUND(?v), ?v, tt:openSide)`. A function is not called with
/// an unbound argument.
pub(crate) fn rewrite(iri: &str, args: &mut Vec<Expression>) -> Option<Expression> {
    if iri.strip_prefix(TT)? != "period" || args.len() < 2 {
        return None;
    }
    let mut args = std::mem::take(args);
    for a in args.iter_mut().take(2) {
        if let Expression::Variable(v) = a {
            *a = Expression::If(
                Box::new(Expression::Bound(v.clone())),
                Box::new(Expression::Variable(v.clone())),
                Box::new(Expression::NamedNode(NamedNode::new_unchecked(OPEN))),
            );
        }
    }
    Some(Expression::FunctionCall(
        Function::Custom(NamedNode::new_unchecked(PERIOD)),
        args,
    ))
}

/// The public IRIs (for the service description).
pub fn names() -> Vec<NamedNode> {
    implementations()
        .into_iter()
        .map(|(n, _)| NamedNode::new_unchecked(format!("{TT}{n}")))
        .collect()
}

/// A default offset as `Z` or `±hh:mm`, in minutes, for [`crate::Engine::set_time_offset`].
pub fn parse_offset(s: &str) -> Result<i64, String> {
    offset_of(s.trim()).ok_or_else(|| format!("a time offset is Z or ±hh:mm up to 14:00, not {s}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(v: &str, dt: impl Into<NamedNode>) -> Term {
        Literal::new_typed_literal(v, dt).into()
    }
    fn p(v: &str) -> Period {
        parse_period(v, 0).unwrap()
    }

    #[test]
    fn the_calendar_round_trips() {
        for days in [
            -800_000_000i64,
            -719_528,
            -1,
            0,
            1,
            11_016,
            19_000,
            400_000_000,
        ] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(stamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(stamp(-1), "1969-12-31T23:59:59.999Z");
        assert_eq!(stamp(1_500), "1970-01-01T00:00:01.5Z");
        // Year 0 is 1 BCE; -0001 is 2 BCE.
        assert_eq!(
            stamp(days_from_civil(0, 2, 29) * MS_PER_DAY),
            "0000-02-29T00:00:00Z"
        );
        assert_eq!(
            stamp(days_from_civil(-44, 3, 15) * MS_PER_DAY),
            "-0044-03-15T00:00:00Z"
        );
    }

    #[test]
    fn values_are_their_spans() {
        let utc = |t: &Term| period_of(t, 0).unwrap().lexical();
        assert_eq!(
            utc(&lit("2026-11-26", xsd::DATE)),
            "2026-11-26T00:00:00Z/2026-11-27T00:00:00Z"
        );
        assert_eq!(
            utc(&lit("2026-11-26+01:00", xsd::DATE)),
            "2026-11-25T23:00:00Z/2026-11-26T23:00:00Z"
        );
        assert_eq!(
            utc(&lit("2024-02", xsd::G_YEAR_MONTH)),
            "2024-02-01T00:00:00Z/2024-03-01T00:00:00Z"
        );
        assert_eq!(
            utc(&lit("2024", xsd::G_YEAR)),
            "2024-01-01T00:00:00Z/2025-01-01T00:00:00Z"
        );
        assert_eq!(
            utc(&lit("2026-11-26T10:00:00.1234-02:00", xsd::DATE_TIME)),
            "2026-11-26T12:00:00.123Z/2026-11-26T12:00:00.124Z"
        );
        assert_eq!(
            utc(&lit("2026-12-31T24:00:00", xsd::DATE_TIME)),
            "2027-01-01T00:00:00Z/2027-01-01T00:00:00.001Z"
        );
        assert_eq!(
            utc(&lit(
                "2022-12-11/2078-12-31",
                NamedNode::new_unchecked(PERIOD)
            )),
            "2022-12-11T00:00:00Z/2079-01-01T00:00:00Z"
        );
        // The canonical form reads back as itself.
        let canonical = "2022-12-11T00:00:00Z/2079-01-01T00:00:00Z";
        assert_eq!(p(canonical).lexical(), canonical);
        assert_eq!(p("../2030").lexical(), "../2031-01-01T00:00:00Z");
        assert_eq!(p("2030/").lexical(), "2030-01-01T00:00:00Z/..");
        // At a default offset, values without one move; ones with one stay.
        let at = |t: &Term, o| period_of(t, o).unwrap().lexical();
        assert_eq!(
            at(&lit("2026-11-26", xsd::DATE), 60),
            "2026-11-25T23:00:00Z/2026-11-26T23:00:00Z"
        );
        assert_eq!(
            at(&lit("2026-11-26Z", xsd::DATE), 60),
            "2026-11-26T00:00:00Z/2026-11-27T00:00:00Z"
        );
    }

    #[test]
    fn bad_values_have_no_period() {
        for (v, dt) in [
            ("2023-02-29", xsd::DATE),
            ("2026-13", xsd::G_YEAR_MONTH),
            ("26", xsd::G_YEAR),
            ("2026-11-26T24:00:01", xsd::DATE_TIME),
            ("2026-11-26T10:00:00+15:00", xsd::DATE_TIME),
            ("2026-11-26T10:00:00", xsd::DATE_TIME_STAMP),
            ("02026", xsd::G_YEAR),
        ] {
            assert_eq!(period_of(&lit(v, dt), 0), None, "{v}");
        }
        assert_eq!(parse_period("2030/2020", 0), None);
        assert_eq!(parse_period("2030", 0), None);
        assert_eq!(period_of(&Literal::from(3).into(), 0), None);
    }

    #[test]
    fn relations_are_exclusive_and_exhaustive() {
        let periods = [
            "2020/2021",
            "2021/2022",
            "2020-06/2021-06",
            "2020/2020",
            "2019/2022",
            "../2021",
            "2020/..",
            "../..",
            "2020-01-01T00:00:00Z/2020-01-01T00:00:00.001Z",
        ];
        let allen = &RELATIONS[..13];
        for a in periods {
            for b in periods {
                let held: Vec<&str> = allen
                    .iter()
                    .copied()
                    .filter(|r| relation(r, p(a), p(b)).unwrap())
                    .collect();
                assert_eq!(held.len(), 1, "{a} {b}: {held:?}");
            }
        }
        assert!(p("2020-01-01T00:00:00Z/2020-01-01T00:00:00.001Z").is_instant());
    }
}
