//! Literal values: ordering for the range and property pair constraints, and well-formedness
//! for `sh:datatype`.

use oxigraph::model::{Literal, Term};
use oxsdatatypes::{
    Boolean, Date, DateTime, DayTimeDuration, Decimal, Double, Duration, Float, GDay, GMonth,
    GMonthDay, GYear, GYearMonth, Time, YearMonthDuration,
};
use std::cmp::Ordering;
use std::str::FromStr;

pub(crate) const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
const RDF_LANG_STRING: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#langString";

/// A literal as SPARQL's `<` sees it.
#[derive(Debug, Clone, PartialEq, PartialOrd)]
enum Value<'a> {
    Number(f64),
    String(&'a str),
    Boolean(bool),
    DateTime(DateTime),
    Date(Date),
    Time(Time),
    Duration(Duration),
}

fn integer_range(name: &str) -> Option<(i128, i128)> {
    Some(match name {
        "integer" => (i128::MIN, i128::MAX),
        "long" => (i64::MIN.into(), i64::MAX.into()),
        "int" => (i32::MIN.into(), i32::MAX.into()),
        "short" => (i16::MIN.into(), i16::MAX.into()),
        "byte" => (i8::MIN.into(), i8::MAX.into()),
        "nonNegativeInteger" => (0, i128::MAX),
        "positiveInteger" => (1, i128::MAX),
        "nonPositiveInteger" => (i128::MIN, 0),
        "negativeInteger" => (i128::MIN, -1),
        "unsignedLong" => (0, u64::MAX.into()),
        "unsignedInt" => (0, u32::MAX.into()),
        "unsignedShort" => (0, u16::MAX.into()),
        "unsignedByte" => (0, u8::MAX.into()),
        _ => return None,
    })
}

fn parse_integer(s: &str) -> Option<i128> {
    let s = s.strip_prefix('+').unwrap_or(s);
    if s.is_empty()
        || !s
            .trim_start_matches('-')
            .bytes()
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    s.parse().ok()
}

fn value(l: &Literal) -> Option<Value<'_>> {
    let dt = l.datatype().as_str();
    let name = dt.strip_prefix(XSD)?;
    let v = l.value();
    Some(match name {
        "string" => Value::String(v),
        "boolean" => Value::Boolean(bool::from(Boolean::from_str(v).ok()?)),
        "decimal" => Value::Number(Decimal::from_str(v).ok()?.to_string().parse().ok()?),
        "double" => Value::Number(f64::from(Double::from_str(v).ok()?)),
        "float" => Value::Number(f32::from(Float::from_str(v).ok()?).into()),
        "dateTime" | "dateTimeStamp" => Value::DateTime(DateTime::from_str(v).ok()?),
        "date" => Value::Date(Date::from_str(v).ok()?),
        "time" => Value::Time(Time::from_str(v).ok()?),
        "duration" => Value::Duration(Duration::from_str(v).ok()?),
        "dayTimeDuration" => Value::Duration(DayTimeDuration::from_str(v).ok()?.into()),
        "yearMonthDuration" => Value::Duration(YearMonthDuration::from_str(v).ok()?.into()),
        n => {
            let (lo, hi) = integer_range(n)?;
            let i = parse_integer(v)?;
            if i < lo || i > hi {
                return None;
            }
            Value::Number(i as f64)
        }
    })
}

/// `a` against `b` as SPARQL's `<`, `=` and `>` compare them; `None` when they cannot be
/// compared (an IRI, a number and a string, an ill-formed literal, ...).
pub fn compare(a: &Term, b: &Term) -> Option<Ordering> {
    let (Term::Literal(a), Term::Literal(b)) = (a, b) else {
        return None;
    };
    let (x, y) = (value(a)?, value(b)?);
    match (&x, &y) {
        (Value::Number(_), Value::Number(_))
        | (Value::String(_), Value::String(_))
        | (Value::Boolean(_), Value::Boolean(_))
        | (Value::DateTime(_), Value::DateTime(_))
        | (Value::Date(_), Value::Date(_))
        | (Value::Time(_), Value::Time(_))
        | (Value::Duration(_), Value::Duration(_)) => x.partial_cmp(&y),
        _ => None,
    }
}

/// The number a literal stands for, if it is numeric.
pub fn number(t: &Term) -> Option<f64> {
    match t {
        Term::Literal(l) => match value(l)? {
            Value::Number(n) => Some(n),
            _ => None,
        },
        _ => None,
    }
}

/// Whether `l` has datatype `datatype` and a lexical form valid for it (for the XSD types
/// the validator knows; any form of another datatype is taken as valid).
pub fn has_datatype(l: &Literal, datatype: &str) -> bool {
    if l.datatype().as_str() != datatype {
        return false;
    }
    if datatype == RDF_LANG_STRING {
        return l.language().is_some();
    }
    let Some(name) = datatype.strip_prefix(XSD) else {
        return true;
    };
    let v = l.value();
    match name {
        "boolean" => Boolean::from_str(v).is_ok(),
        "decimal" => Decimal::from_str(v).is_ok(),
        "double" => Double::from_str(v).is_ok(),
        "float" => Float::from_str(v).is_ok(),
        "dateTime" => DateTime::from_str(v).is_ok(),
        "dateTimeStamp" => DateTime::from_str(v).is_ok_and(|d| d.timezone().is_some()),
        "date" => Date::from_str(v).is_ok(),
        "time" => Time::from_str(v).is_ok(),
        "gYear" => GYear::from_str(v).is_ok(),
        "gYearMonth" => GYearMonth::from_str(v).is_ok(),
        "gMonth" => GMonth::from_str(v).is_ok(),
        "gMonthDay" => GMonthDay::from_str(v).is_ok(),
        "gDay" => GDay::from_str(v).is_ok(),
        "duration" => Duration::from_str(v).is_ok(),
        "dayTimeDuration" => DayTimeDuration::from_str(v).is_ok(),
        "yearMonthDuration" => YearMonthDuration::from_str(v).is_ok(),
        n => match integer_range(n) {
            Some((lo, hi)) => parse_integer(v).is_some_and(|i| i >= lo && i <= hi),
            None => true,
        },
    }
}

/// The string `sh:minLength`, `sh:maxLength` and `sh:pattern` look at: an IRI or a literal's
/// lexical form (blank nodes have none).
pub fn string_of(t: &Term) -> Option<&str> {
    match t {
        Term::NamedNode(n) => Some(n.as_str()),
        Term::Literal(l) => Some(l.value()),
        _ => None,
    }
}

/// `langMatches` with basic filtering (RFC 4647), as `sh:languageIn` uses it.
pub fn lang_matches(tag: &str, range: &str) -> bool {
    if range == "*" {
        return !tag.is_empty();
    }
    tag.len() >= range.len()
        && tag[..range.len()].eq_ignore_ascii_case(range)
        && (tag.len() == range.len() || tag.as_bytes()[range.len()] == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::model::NamedNode;

    fn typed(v: &str, dt: &str) -> Term {
        Literal::new_typed_literal(v, NamedNode::new_unchecked(format!("{XSD}{dt}"))).into()
    }

    #[test]
    fn ordering() {
        assert_eq!(
            compare(&typed("3", "integer"), &typed("3.5", "decimal")),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare(&typed("4.0E0", "double"), &typed("4", "int")),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare(&typed("b", "string"), &typed("a", "string")),
            Some(Ordering::Greater)
        );
        assert_eq!(compare(&typed("1", "integer"), &typed("a", "string")), None);
        assert_eq!(
            compare(
                &typed("2024-01-01T00:00:00Z", "dateTime"),
                &typed("2024-06-01T00:00:00Z", "dateTime")
            ),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare(&typed("x", "integer"), &typed("1", "integer")),
            None
        );
    }

    #[test]
    fn datatypes() {
        let lit = |v: &str, dt: &str| {
            Literal::new_typed_literal(v, NamedNode::new_unchecked(format!("{XSD}{dt}")))
        };
        assert!(has_datatype(
            &lit("12", "integer"),
            &format!("{XSD}integer")
        ));
        assert!(!has_datatype(
            &lit("1.5", "integer"),
            &format!("{XSD}integer")
        ));
        assert!(!has_datatype(&lit("300", "byte"), &format!("{XSD}byte")));
        assert!(!has_datatype(
            &lit("-1", "nonNegativeInteger"),
            &format!("{XSD}nonNegativeInteger")
        ));
        assert!(has_datatype(&lit("1e3", "double"), &format!("{XSD}double")));
        assert!(!has_datatype(
            &lit("2024-13-01", "date"),
            &format!("{XSD}date")
        ));
        assert!(!has_datatype(
            &lit("12", "integer"),
            &format!("{XSD}decimal")
        ));
        assert!(lang_matches("en-GB", "en"));
        assert!(!lang_matches("eng", "en"));
        assert!(lang_matches("de", "*"));
    }
}
