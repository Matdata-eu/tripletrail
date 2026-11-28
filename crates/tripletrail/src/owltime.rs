//! `?entity tt:hasPeriod ?p` (#301): the period of an OWL-Time instant or interval, a DCAT
//! `dct:PeriodOfTime` or a schema.org event, whichever way it is written.
//!
//! The property function is rewritten ([`step`]) into a pattern that reads every writing of
//! the entity it may have, and one custom aggregate ([`PARTS`]) that makes the period of them:
//!
//! - the writings of an instant: `time:inXSDDateTimeStamp`, `inXSDDateTime`, `inXSDDate`,
//!   `inXSDgYearMonth` and `inXSDgYear`; a `time:inDateTime` description (`time:year`, `month`,
//!   `day`, `hour`, `minute`, `second`, its `unitType` giving the precision); a
//!   `time:inTimePosition` with a `numericPosition` in Unix time or a `nominalPosition` written
//!   as one of the XSD forms. Several writings of one instant give their intersection; when
//!   they have nothing in common, the most precise one.
//! - an interval: from the start of its beginning's period (`time:hasBeginning`, `dcat:startDate`,
//!   `schema:startDate`) to the end of its end's (`time:hasEnd`, `dcat:endDate`,
//!   `schema:endDate`), or a beginning plus `time:hasXSDDuration`. A missing side is open; the
//!   earliest beginning and the latest end are taken; a beginning after the end gives none.
//! - `time:hasTime` leads to the instant or interval of an entity: one period for each.
//!
//! The end of an interval is the end of its end's period, except that an end written to the
//! millisecond (a date-time) is the end itself, as in `tt:period(start, end)`: an interval to
//! `2027-01-01T00:00:00Z` meets one from there.

use crate::temporal::{Period, period_of, stamp};
use oxigraph::model::vocab::xsd;
use oxigraph::model::{Literal, NamedNode, Term};
use spareval::AggregateFunctionAccumulator;
use spargebra::algebra::GraphPattern;
use spargebra::algebra::{AggregateExpression, AggregateFunction, Expression, Function};
use spargebra::term::{GroundTerm, NamedNodePattern, TermPattern, TriplePattern, Variable};
use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

const TIME: &str = "http://www.w3.org/2006/time#";
const DCAT: &str = "http://www.w3.org/ns/dcat#";
const UNIX: &str = "http://dbpedia.org/resource/Unix_time";
/// The custom aggregate the rewrite uses: the period of an entity's writings, each given as
/// `(side, node, kind, writing, value)` packed by `tt:aggregateArguments`.
pub const PARTS: &str = "http://w3id.org/tripletrail/fn#periodOfParts";

/// The writings of an instant read as one XSD value each.
const XSD_WRITINGS: [&str; 5] = [
    "inXSDDateTimeStamp",
    "inXSDDateTime",
    "inXSDDate",
    "inXSDgYearMonth",
    "inXSDgYear",
];
/// The parts of a `time:DateTimeDescription` read here.
const DESCRIPTION: [&str; 7] = [
    "year", "month", "day", "hour", "minute", "second", "unitType",
];

fn time(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{TIME}{name}"))
}

fn named(iri: &str) -> NamedNode {
    NamedNode::new_unchecked(iri)
}

fn var(v: &Variable) -> TermPattern {
    TermPattern::Variable(v.clone())
}

fn triple(s: TermPattern, p: NamedNode, o: TermPattern) -> TriplePattern {
    TriplePattern {
        subject: s,
        predicate: NamedNodePattern::NamedNode(p),
        object: o,
    }
}

fn bgp(patterns: Vec<TriplePattern>) -> GraphPattern {
    GraphPattern::Bgp { patterns }
}

fn join(left: GraphPattern, right: GraphPattern) -> GraphPattern {
    GraphPattern::Join {
        left: Box::new(left),
        right: Box::new(right),
    }
}

fn union(parts: Vec<GraphPattern>) -> GraphPattern {
    parts
        .into_iter()
        .reduce(|left, right| GraphPattern::Union {
            left: Box::new(left),
            right: Box::new(right),
        })
        .unwrap_or_else(|| bgp(Vec::new()))
}

fn extend(inner: GraphPattern, variable: &Variable, expression: Expression) -> GraphPattern {
    GraphPattern::Extend {
        inner: Box::new(inner),
        variable: variable.clone(),
        expression,
    }
}

fn values(v: &Variable, iris: impl IntoIterator<Item = NamedNode>) -> GraphPattern {
    GraphPattern::Values {
        variables: vec![v.clone()],
        bindings: iris
            .into_iter()
            .map(|n| vec![Some(GroundTerm::NamedNode(n))])
            .collect(),
    }
}

fn as_expression(t: &TermPattern) -> Expression {
    match t {
        TermPattern::Variable(v) => Expression::Variable(v.clone()),
        TermPattern::NamedNode(n) => Expression::NamedNode(n.clone()),
        TermPattern::Literal(l) => Expression::Literal(l.clone()),
        // The rewrite gives blank nodes as variables.
        _ => Expression::Literal(spargebra::term::Literal::new_simple_literal("")),
    }
}

/// The variables the rewrite of one call uses.
struct Vars {
    side: Variable,
    node: Variable,
    kind: Variable,
    writing: Variable,
    value: Variable,
}

impl Vars {
    fn new(fresh: &dyn Fn(&str) -> Variable) -> Self {
        Self {
            side: fresh("side"),
            node: fresh("node"),
            kind: fresh("kind"),
            writing: fresh("writing"),
            value: fresh("value"),
        }
    }
}

/// The writings of the instant `n`: its `kind`, `writing` (the value itself, or the node that
/// holds the parts) and `value`.
fn instant(n: &TermPattern, v: &Vars) -> GraphPattern {
    let (writing, value) = (var(&v.writing), var(&v.value));
    let by_kind = |subject: TermPattern| TriplePattern {
        subject,
        predicate: NamedNodePattern::Variable(v.kind.clone()),
        object: value.clone(),
    };
    let xsd_values = extend(
        join(
            values(&v.kind, XSD_WRITINGS.map(time)),
            bgp(vec![by_kind(n.clone())]),
        ),
        &v.writing,
        Expression::Variable(v.value.clone()),
    );
    let description = join(
        values(&v.kind, DESCRIPTION.map(time)),
        bgp(vec![
            triple(n.clone(), time("inDateTime"), writing.clone()),
            by_kind(writing.clone()),
        ]),
    );
    let position = |name: &str, trs: bool| {
        let mut patterns = vec![
            triple(n.clone(), time("inTimePosition"), writing.clone()),
            triple(writing.clone(), time(name), value.clone()),
        ];
        if trs {
            patterns.push(triple(
                writing.clone(),
                time("hasTRS"),
                TermPattern::NamedNode(named(UNIX)),
            ));
        }
        extend(bgp(patterns), &v.kind, Expression::NamedNode(time(name)))
    };
    union(vec![
        xsd_values,
        description,
        position("numericPosition", true),
        position("nominalPosition", false),
    ])
}

/// Every writing of the entity `x`, each with its `side` (`i` the entity as an instant, `b`
/// a beginning, `e` an end, `d` a duration) and the `node` it is a writing of.
fn writings(x: &TermPattern, v: &Vars, fresh: &dyn Fn(&str) -> Variable) -> GraphPattern {
    let side = |p: GraphPattern, s: &str, node: Expression| {
        extend(
            extend(p, &v.node, node),
            &v.side,
            Expression::Literal(spargebra::term::Literal::new_simple_literal(s)),
        )
    };
    let me = as_expression(x);
    let mut parts = vec![side(instant(x, v), "i", me.clone())];
    for (s, link) in [("b", "hasBeginning"), ("e", "hasEnd")] {
        let n = fresh("instant");
        parts.push(side(
            join(
                bgp(vec![triple(x.clone(), time(link), var(&n))]),
                instant(&var(&n), v),
            ),
            s,
            Expression::Variable(n),
        ));
    }
    // Dates written on the period itself: each is a writing of its side.
    for (s, names) in [
        (
            "b",
            [
                format!("{DCAT}startDate"),
                "http://schema.org/startDate".to_owned(),
                "https://schema.org/startDate".to_owned(),
            ],
        ),
        (
            "e",
            [
                format!("{DCAT}endDate"),
                "http://schema.org/endDate".to_owned(),
                "https://schema.org/endDate".to_owned(),
            ],
        ),
    ] {
        let dated = extend(
            join(
                values(&v.kind, names.iter().map(|n| named(n))),
                bgp(vec![TriplePattern {
                    subject: x.clone(),
                    predicate: NamedNodePattern::Variable(v.kind.clone()),
                    object: var(&v.value),
                }]),
            ),
            &v.writing,
            Expression::Variable(v.value.clone()),
        );
        parts.push(side(dated, s, me.clone()));
    }
    let duration = extend(
        extend(
            bgp(vec![triple(
                x.clone(),
                time("hasXSDDuration"),
                var(&v.value),
            )]),
            &v.writing,
            Expression::Variable(v.value.clone()),
        ),
        &v.kind,
        Expression::NamedNode(time("hasXSDDuration")),
    );
    parts.push(side(duration, "d", me));
    union(parts)
}

/// The pattern `subject tt:hasPeriod object` is rewritten to: the period of the subject's
/// writings, and of those of each `time:hasTime` of it, one row each that has one.
pub(crate) fn step(
    subject: &TermPattern,
    object: &TermPattern,
    fresh: &dyn Fn(&str) -> Variable,
) -> Option<GraphPattern> {
    let v = Vars::new(fresh);
    let target = fresh("time");
    let direct = writings(subject, &v, fresh);
    let via = join(
        bgp(vec![triple(subject.clone(), time("hasTime"), var(&target))]),
        writings(&var(&target), &v, fresh),
    );
    let period = fresh("period");
    let packed = Expression::FunctionCall(
        Function::Custom(named(crate::statagg::ARGUMENTS)),
        [&v.side, &v.node, &v.kind, &v.writing, &v.value]
            .into_iter()
            .map(|x| Expression::Variable(x.clone()))
            .collect(),
    );
    let mut keys = vec![target.clone()];
    if let TermPattern::Variable(s) = subject {
        keys.insert(0, s.clone());
    }
    let grouped = GraphPattern::Group {
        inner: Box::new(GraphPattern::Union {
            left: Box::new(direct),
            right: Box::new(via),
        }),
        variables: keys,
        aggregates: vec![(
            period.clone(),
            AggregateExpression::FunctionCall {
                name: AggregateFunction::Custom(named(PARTS)),
                expr: packed,
                distinct: false,
            },
        )],
    };
    let found = GraphPattern::Filter {
        expr: Expression::Bound(period.clone()),
        inner: Box::new(grouped),
    };
    Some(match object {
        TermPattern::Variable(o) => extend(found, o, Expression::Variable(period)),
        TermPattern::Literal(l) => GraphPattern::Filter {
            expr: Expression::SameTerm(
                Box::new(Expression::Variable(period)),
                Box::new(Expression::Literal(l.clone())),
            ),
            inner: Box::new(found),
        },
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// The aggregate
// ---------------------------------------------------------------------------------------------

/// The aggregate [`PARTS`], reading values without a timezone at the engine's offset.
pub fn aggregate(
    offset: Arc<AtomicI64>,
) -> impl Fn() -> Box<dyn AggregateFunctionAccumulator + Send + Sync> + Send + Sync + 'static {
    move || {
        Box::new(Parts {
            offset: Arc::clone(&offset),
            rows: Vec::new(),
        })
    }
}

struct Parts {
    offset: Arc<AtomicI64>,
    rows: Vec<Vec<Term>>,
}

impl AggregateFunctionAccumulator for Parts {
    fn accumulate(&mut self, element: Term) {
        if let Some(row) = crate::statagg::unpack(&element)
            && row.len() == 5
        {
            self.rows.push(row);
        }
    }

    fn finish(&mut self) -> Option<Term> {
        let rows = std::mem::take(&mut self.rows);
        Some(period_of_parts(&rows, self.offset.load(Ordering::Relaxed))?.term())
    }
}

fn local(t: &Term, ns: &str) -> Option<String> {
    match t {
        Term::NamedNode(n) => n.as_str().strip_prefix(ns).map(str::to_owned),
        _ => None,
    }
}

/// The period of an entity's writings, each `[side, node, kind, writing, value]`.
pub(crate) fn period_of_parts(rows: &[Vec<Term>], offset: i64) -> Option<Period> {
    // side -> node -> writing -> (kind, value) parts
    type Writings = BTreeMap<String, Vec<(String, Term)>>;
    let mut sides: BTreeMap<&str, BTreeMap<String, Writings>> = BTreeMap::new();
    let mut durations = Vec::new();
    for row in rows {
        let [side, node, kind, writing, value] = row.as_slice() else {
            continue;
        };
        let Term::Literal(side) = side else { continue };
        let side = match side.value() {
            "i" => "i",
            "b" => "b",
            "e" => "e",
            "d" => {
                durations.push(value.clone());
                continue;
            }
            _ => continue,
        };
        let kind = match kind {
            Term::NamedNode(n) => n.as_str().to_owned(),
            _ => continue,
        };
        sides
            .entry(side)
            .or_default()
            .entry(node.to_string())
            .or_default()
            .entry(writing.to_string())
            .or_default()
            .push((kind, value.clone()));
    }
    let node_periods = |side: &str| -> Vec<Period> {
        sides.get(side).map_or_else(Vec::new, |nodes| {
            nodes
                .values()
                .filter_map(|w| instant_period(w, offset))
                .collect()
        })
    };
    let (begins, ends) = (node_periods("b"), node_periods("e"));
    if !begins.is_empty() || !ends.is_empty() || !durations.is_empty() {
        // An interval.
        let start = begins
            .iter()
            .map(|p| p.start)
            .min_by_key(|s| s.unwrap_or(i64::MIN))
            .flatten();
        let mut end = ends
            .iter()
            .map(|p| end_side(*p))
            .max_by_key(|e| e.unwrap_or(i64::MAX))
            .flatten();
        if ends.is_empty()
            && let Some(s) = start
        {
            end = durations.iter().find_map(|d| add_duration(s, d));
        }
        if begins.is_empty() && ends.is_empty() {
            return None;
        }
        return Period::new(start, end);
    }
    node_periods("i").into_iter().next()
}

/// The end an end instant gives an interval: an instant itself, else the end of its span.
fn end_side(p: Period) -> Option<i64> {
    match (p.start, p.end) {
        (Some(s), Some(e)) if e - s == 1 => Some(s),
        (_, e) => e,
    }
}

/// The period of one instant from its writings: their intersection, or the most precise one
/// when they have nothing in common.
fn instant_period(writings: &BTreeMap<String, Vec<(String, Term)>>, offset: i64) -> Option<Period> {
    let periods: Vec<Period> = writings
        .values()
        .filter_map(|parts| writing_period(parts, offset))
        .collect();
    let first = *periods.first()?;
    let both = periods.iter().skip(1).try_fold(first, |a, b| {
        let start = a.start.max(b.start);
        let end = match (a.end, b.end) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (x, None) | (None, x) => x,
        };
        Period::new(start, end)
    });
    both.or_else(|| {
        periods
            .into_iter()
            .min_by_key(|p| p.end.zip(p.start).map_or(i64::MAX, |(e, s)| e - s))
    })
}

/// The period of one writing: an XSD value, a date-time description or a time position.
fn writing_period(parts: &[(String, Term)], offset: i64) -> Option<Period> {
    if let [(kind, value)] = parts {
        let name = kind.strip_prefix(TIME).unwrap_or(kind);
        if XSD_WRITINGS.contains(&name)
            || kind.ends_with("/startDate")
            || kind.ends_with("/endDate")
            || kind.starts_with(DCAT)
        {
            return period_of(value, offset);
        }
        if name == "nominalPosition" {
            let Term::Literal(l) = value else { return None };
            return period_of(&Literal::new_simple_literal(l.value()).into(), offset);
        }
        if name == "numericPosition" {
            let Term::Literal(l) = value else { return None };
            let seconds = f64::from_str(l.value().trim()).ok()?;
            let ms = (seconds * 1000.0).floor();
            if !ms.is_finite() || ms.abs() > 1e17 {
                return None;
            }
            let ms = ms as i64;
            return Period::new(Some(ms), Some(ms + 1));
        }
    }
    description_period(parts, offset)
}

/// The digits of a description's part (`"2026"`, `"--05"`, `"---11"`, `"7"`), as a number.
fn number(t: &Term) -> Option<(i64, String)> {
    let Term::Literal(l) = t else { return None };
    let v = l.value().trim();
    let digits: String = v
        .trim_start_matches('-')
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let whole = digits.split('.').next()?;
    let negative = l.datatype() == xsd::G_YEAR && v.starts_with('-');
    let n: i64 = whole.parse().ok()?;
    Some((if negative { -n } else { n }, digits))
}

/// A `time:DateTimeDescription`: its parts down to its unit (by default the finest part it
/// has), as the span of that unit.
fn description_period(parts: &[(String, Term)], offset: i64) -> Option<Period> {
    let get = |name: &str| {
        parts
            .iter()
            .find(|(k, _)| k.strip_prefix(TIME) == Some(name))
            .map(|(_, v)| v)
    };
    let year = number(get("year")?)?.0;
    let unit = get("unitType").and_then(|u| local(u, TIME));
    let order = ["year", "month", "day", "hour", "minute", "second"];
    let finest = order.iter().rposition(|n| get(n).is_some()).unwrap_or(0);
    let depth = match unit.as_deref() {
        Some("unitYear") => 0,
        Some("unitMonth") => 1,
        Some("unitDay") => 2,
        Some("unitHour") => 3,
        Some("unitMinute") => 4,
        Some("unitSecond") => 5,
        _ => finest,
    };
    let part = |i: usize, default: i64| -> Option<i64> {
        match get(order[i]) {
            Some(t) => Some(number(t)?.0),
            None if i <= depth => None,
            None => Some(default),
        }
    };
    let y = if year < 0 {
        format!("-{:04}", -year)
    } else {
        format!("{year:04}")
    };
    let lexical = |datatype: NamedNode, text: String| -> Option<Period> {
        period_of(&Literal::new_typed_literal(text, datatype).into(), offset)
    };
    if depth == 0 {
        return lexical(xsd::G_YEAR.into_owned(), y);
    }
    let m = part(1, 1)?;
    if depth == 1 {
        return lexical(xsd::G_YEAR_MONTH.into_owned(), format!("{y}-{m:02}"));
    }
    let d = part(2, 1)?;
    if depth == 2 {
        return lexical(xsd::DATE.into_owned(), format!("{y}-{m:02}-{d:02}"));
    }
    let h = part(3, 0)?;
    let min = if depth >= 4 { part(4, 0)? } else { 0 };
    let (sec, length) = if depth >= 5 {
        let (s, text) = number(get("second")?)?;
        let fraction = text.split_once('.').map(|(_, f)| f.to_owned());
        match fraction {
            Some(f) if !f.is_empty() => (format!("{s:02}.{f}"), 1),
            _ => (format!("{s:02}"), 1000),
        }
    } else {
        ("00".to_owned(), if depth == 3 { 3_600_000 } else { 60_000 })
    };
    let at = lexical(
        xsd::DATE_TIME.into_owned(),
        format!("{y}-{m:02}-{d:02}T{h:02}:{min:02}:{sec}"),
    )?;
    let start = at.start?;
    Period::new(Some(start), Some(start + length))
}

/// `start` plus an `xsd:duration`, on the calendar (a month added to 31 January is the end of
/// February), as XSD adds durations to date-times.
fn add_duration(start: i64, d: &Term) -> Option<i64> {
    let Term::Literal(l) = d else { return None };
    let duration = oxsdatatypes::Duration::from_str(l.value()).ok()?;
    let at = oxsdatatypes::DateTime::from_str(&stamp(start)).ok()?;
    let end = at.checked_add_duration(duration)?;
    period_of(
        &Literal::new_typed_literal(end.to_string(), xsd::DATE_TIME_STAMP).into(),
        0,
    )?
    .start
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(v: &str, dt: oxigraph::model::NamedNodeRef<'_>) -> Term {
        Literal::new_typed_literal(v, dt).into()
    }

    fn row(side: &str, node: &str, kind: &str, writing: &str, value: Term) -> Vec<Term> {
        vec![
            Literal::new_simple_literal(side).into(),
            NamedNode::new_unchecked(format!("http://e/{node}")).into(),
            NamedNode::new_unchecked(format!("{TIME}{kind}")).into(),
            NamedNode::new_unchecked(format!("http://e/{writing}")).into(),
            value,
        ]
    }

    fn lexical(rows: &[Vec<Term>]) -> Option<String> {
        period_of_parts(rows, 0).map(Period::lexical)
    }

    #[test]
    fn writings_of_one_instant_intersect() {
        let date = lit("2022-12-11", xsd::DATE);
        let stamp = lit("2022-12-11T10:00:00Z", xsd::DATE_TIME_STAMP);
        assert_eq!(
            lexical(&[
                row("i", "x", "inXSDDate", "w1", date.clone()),
                row("i", "x", "inXSDDateTimeStamp", "w2", stamp),
            ])
            .unwrap(),
            "2022-12-11T10:00:00Z/2022-12-11T10:00:00.001Z"
        );
        // Nothing in common: the more precise one.
        let other = lit("2023-01-01T00:00:00Z", xsd::DATE_TIME_STAMP);
        assert_eq!(
            lexical(&[
                row("i", "x", "inXSDDate", "w1", date),
                row("i", "x", "inXSDDateTimeStamp", "w2", other),
            ])
            .unwrap(),
            "2023-01-01T00:00:00Z/2023-01-01T00:00:00.001Z"
        );
    }

    #[test]
    fn an_interval_from_its_beginning_to_its_end() {
        let b = row("b", "b", "inXSDDate", "w1", lit("2026-01-01", xsd::DATE));
        let e = row("e", "e", "inXSDDate", "w2", lit("2026-12-31", xsd::DATE));
        assert_eq!(
            lexical(&[b.clone(), e]).unwrap(),
            "2026-01-01T00:00:00Z/2027-01-01T00:00:00Z"
        );
        // An end to the millisecond is the end itself.
        let e = row(
            "e",
            "e",
            "inXSDDateTimeStamp",
            "w2",
            lit("2027-01-01T00:00:00Z", xsd::DATE_TIME_STAMP),
        );
        assert_eq!(
            lexical(&[b.clone(), e]).unwrap(),
            "2026-01-01T00:00:00Z/2027-01-01T00:00:00Z"
        );
        assert_eq!(
            lexical(std::slice::from_ref(&b)).unwrap(),
            "2026-01-01T00:00:00Z/.."
        );
        // A beginning after the end.
        let early = row("e", "e", "inXSDDate", "w2", lit("2025-06-01", xsd::DATE));
        assert_eq!(lexical(&[b.clone(), early]), None);
        // A duration.
        let d = vec![
            Literal::new_simple_literal("d").into(),
            NamedNode::new_unchecked("http://e/x").into(),
            NamedNode::new_unchecked(format!("{TIME}hasXSDDuration")).into(),
            lit("P1M", xsd::DURATION),
            lit("P1M", xsd::DURATION),
        ];
        assert_eq!(
            lexical(&[b, d]).unwrap(),
            "2026-01-01T00:00:00Z/2026-02-01T00:00:00Z"
        );
    }

    #[test]
    fn descriptions_and_positions() {
        let parts = |items: &[(&str, Term)]| -> Vec<Vec<Term>> {
            items
                .iter()
                .map(|(k, v)| row("i", "x", k, "d", v.clone()))
                .collect()
        };
        let year = lit("2026", xsd::G_YEAR);
        let month = lit("--03", xsd::G_MONTH);
        let day = lit("---09", xsd::G_DAY);
        assert_eq!(
            lexical(&parts(&[("year", year.clone()), ("month", month.clone())])).unwrap(),
            "2026-03-01T00:00:00Z/2026-04-01T00:00:00Z"
        );
        let hour = lit("7", xsd::NON_NEGATIVE_INTEGER);
        assert_eq!(
            lexical(&parts(&[
                ("year", year.clone()),
                ("month", month.clone()),
                ("day", day.clone()),
                ("hour", hour),
            ]))
            .unwrap(),
            "2026-03-09T07:00:00Z/2026-03-09T08:00:00Z"
        );
        let unit: Term = NamedNode::new_unchecked(format!("{TIME}unitYear")).into();
        assert_eq!(
            lexical(&parts(&[
                ("year", year),
                ("month", month),
                ("unitType", unit)
            ]))
            .unwrap(),
            "2026-01-01T00:00:00Z/2027-01-01T00:00:00Z"
        );
        let unix = row(
            "i",
            "x",
            "numericPosition",
            "p",
            lit("1767225600", xsd::DECIMAL),
        );
        assert_eq!(
            lexical(&[unix]).unwrap(),
            "2026-01-01T00:00:00Z/2026-01-01T00:00:00.001Z"
        );
    }
}
