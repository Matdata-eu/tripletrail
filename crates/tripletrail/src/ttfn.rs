//! ETL helper functions in the TripleTrail namespace (`tt:` = `http://w3id.org/tripletrail/fn#`),
//! with aliases under the vendor IRIs whose meaning is the same (#233): deterministic IRIs
//! (RDFox `SKOLEM`, SPIF `buildURI`), dates with Java patterns (SPIF `parseDate`), the XPath
//! picture strings of `fn:format-dateTime`, string similarity and phonetic codes, and padding
//! and case helpers.

use crate::xfn::Func;
use oxigraph::model::vocab::xsd;
use oxigraph::model::{Literal, NamedNode, Term};
use oxsdatatypes::{Date, DateTime, Decimal, Time};
use spargebra::algebra::{Expression, Function};
use spargebra::term::Literal as AlgebraLiteral;
use std::str::FromStr;
use std::sync::Arc;

/// TripleTrail functions: `http://w3id.org/tripletrail/fn#`.
pub const MG: &str = "http://w3id.org/tripletrail/fn#";
/// TopBraid SPIF functions (as shipped in GraphDB): `http://spinrdf.org/spif#`.
pub const SPIF: &str = "http://spinrdf.org/spif#";
/// GraphDB functions: `http://www.ontotext.com/sparql/functions/`.
pub const OFN: &str = "http://www.ontotext.com/sparql/functions/";
const FN: &str = crate::xfn::FN;

/// The key of `tt:skolem`, computed by a custom function and hashed by `SHA256`.
const SKOLEM_KEY: &str = "http://w3id.org/tripletrail/fn#skolemKey";
/// The string of `tt:iri` / `spif:buildURI`, made an IRI by `IRI()` so that a relative one is
/// resolved against the query's base.
const IRI_STRING: &str = "http://w3id.org/tripletrail/fn#iriString";
const URI_STRING: &str = "http://w3id.org/tripletrail/fn#uriString";

fn custom(iri: &str, args: Vec<Expression>) -> Expression {
    Expression::FunctionCall(Function::Custom(NamedNode::new_unchecked(iri)), args)
}

/// Calls rewritten into expressions over built-ins (see [`crate::xfn`]).
pub(crate) fn rewrite(iri: &str, args: &mut Vec<Expression>) -> Option<Expression> {
    use Function as F;
    let name = iri.strip_prefix(MG);
    let spif = iri.strip_prefix(SPIF);
    match (name, spif) {
        // IRI(CONCAT(STR(prefix), SUBSTR(SHA256(key(args)), 1, 32)))
        (Some("skolem"), _) if !args.is_empty() => {
            let prefix = args.remove(0);
            let key = custom(SKOLEM_KEY, std::mem::take(args));
            let hash = Expression::FunctionCall(
                F::SubStr,
                vec![
                    Expression::FunctionCall(F::Sha256, vec![key]),
                    Expression::Literal(AlgebraLiteral::new_typed_literal("1", xsd::INTEGER)),
                    Expression::Literal(AlgebraLiteral::new_typed_literal("32", xsd::INTEGER)),
                ],
            );
            Some(Expression::FunctionCall(
                F::Iri,
                vec![Expression::FunctionCall(
                    F::Concat,
                    vec![Expression::FunctionCall(F::Str, vec![prefix]), hash],
                )],
            ))
        }
        (Some("iri"), _) if !args.is_empty() => Some(Expression::FunctionCall(
            F::Iri,
            vec![custom(IRI_STRING, std::mem::take(args))],
        )),
        (_, Some("buildURI")) if !args.is_empty() => Some(Expression::FunctionCall(
            F::Iri,
            vec![custom(URI_STRING, std::mem::take(args))],
        )),
        _ => None,
    }
}

/// The IRIs [`rewrite`] handles (for the service description).
pub fn rewritten() -> Vec<NamedNode> {
    [
        format!("{MG}skolem"),
        format!("{MG}iri"),
        format!("{SPIF}buildURI"),
    ]
    .into_iter()
    .map(NamedNode::new_unchecked)
    .collect()
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn func(f: impl Fn(&[Term]) -> Option<Term> + Send + Sync + 'static) -> Func {
    Arc::new(f)
}

fn string(t: &Term) -> Option<&str> {
    let Term::Literal(l) = t else { return None };
    (l.datatype() == xsd::STRING || l.language().is_some()).then(|| l.value())
}

/// The string of any IRI or literal.
fn str_of(t: &Term) -> Option<&str> {
    match t {
        Term::NamedNode(n) => Some(n.as_str()),
        Term::Literal(l) => Some(l.value()),
        _ => None,
    }
}

fn string_term(s: impl Into<String>) -> Option<Term> {
    Some(Literal::new_simple_literal(s).into())
}

fn int_arg(t: &Term) -> Option<i64> {
    let Term::Literal(l) = t else { return None };
    if l.datatype() == xsd::INTEGER
        || l.datatype() == xsd::INT
        || l.datatype() == xsd::LONG
        || l.datatype() == xsd::SHORT
        || l.datatype() == xsd::NON_NEGATIVE_INTEGER
        || l.datatype() == xsd::POSITIVE_INTEGER
    {
        l.value().parse().ok()
    } else {
        None
    }
}

/// `tt:skolemKey`: an unambiguous string of the arguments' RDF terms (kind, lexical form,
/// datatype, language), hashed by `tt:skolem`.
fn skolem_key(args: &[Term]) -> Option<Term> {
    let mut key = String::new();
    for a in args {
        let (kind, lexical, rest) = match a {
            Term::NamedNode(n) => ('I', n.as_str(), String::new()),
            Term::BlankNode(b) => ('B', b.as_str(), String::new()),
            Term::Literal(l) => (
                'L',
                l.value(),
                match l.language() {
                    Some(lang) => format!("@{lang}"),
                    None => format!("^{}", l.datatype().as_str()),
                },
            ),
            #[allow(unreachable_patterns)]
            _ => return None,
        };
        key.push_str(&format!("{kind}{}:{lexical}{rest};", lexical.len()));
    }
    string_term(key)
}

/// Fills `{0}`, `{1}` (or `{}` in turn) in `template` with the arguments, each through
/// `encode`. `{{` and `}}` are literal braces.
fn fill(template: &str, args: &[Term], encode: fn(&str) -> String) -> Option<String> {
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    let mut next = 0;
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut name = String::new();
                loop {
                    match chars.next()? {
                        '}' => break,
                        c => name.push(c),
                    }
                }
                // SPIF writes `{?1}` for the first argument; ours is `{0}`.
                let index = if let Some(n) = name.strip_prefix('?') {
                    n.parse::<usize>().ok()?.checked_sub(1)?
                } else if name.is_empty() {
                    next += 1;
                    next - 1
                } else {
                    name.parse().ok()?
                };
                out.push_str(&encode(str_of(args.get(index)?)?));
            }
            c => out.push(c),
        }
    }
    Some(out)
}

/// R2RML's IRI-safe encoding: every character that is not `iunreserved` (RFC 3987) as `%XX`
/// of its UTF-8 bytes, so letters outside ASCII are kept.
fn encode_for_iri(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let keep = c.is_ascii_alphanumeric()
            || matches!(c, '-' | '.' | '_' | '~')
            || matches!(u32::from(c),
                0xA0..=0xD7FF | 0xF900..=0xFDCF | 0xFDF0..=0xFFEF
                | 0x10000..=0x1FFFD | 0x20000..=0x2FFFD | 0x30000..=0x3FFFD
                | 0x40000..=0x4FFFD | 0x50000..=0x5FFFD | 0x60000..=0x6FFFD
                | 0x70000..=0x7FFFD | 0x80000..=0x8FFFD | 0x90000..=0x9FFFD
                | 0xA0000..=0xAFFFD | 0xB0000..=0xBFFFD | 0xC0000..=0xCFFFD
                | 0xD0000..=0xDFFFD | 0xE1000..=0xEFFFD);
        if keep {
            out.push(c);
        } else {
            let mut buf = [0; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

fn encode_for_uri(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------------------------

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const DAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The parts of a date, time or dateTime value.
#[derive(Clone, Debug, Default)]
struct Parts {
    year: Option<i64>,
    month: Option<i64>,
    day: Option<i64>,
    hour: Option<i64>,
    minute: Option<i64>,
    /// Whole seconds and the fraction digits.
    second: Option<i64>,
    fraction: String,
    /// Offset in minutes.
    tz: Option<i64>,
}

impl Parts {
    fn has_time(&self) -> bool {
        self.hour.is_some() || self.minute.is_some() || self.second.is_some()
    }

    fn weekday(&self) -> Option<usize> {
        let days = days_from_civil(self.year?, self.month?, self.day?);
        // 1970-01-01 was a Thursday (index 3 from Monday).
        Some((days + 3).rem_euclid(7) as usize)
    }

    fn day_of_year(&self) -> Option<i64> {
        Some(
            days_from_civil(self.year?, self.month?, self.day?) - days_from_civil(self.year?, 1, 1)
                + 1,
        )
    }
}

fn tz_minutes(tz: Option<oxsdatatypes::TimezoneOffset>) -> Option<i64> {
    let s = tz?.to_string();
    if s == "Z" {
        return Some(0);
    }
    let sign = if s.starts_with('-') { -1 } else { 1 };
    let (h, m) = s[1..].split_once(':')?;
    Some(sign * (h.parse::<i64>().ok()? * 60 + m.parse::<i64>().ok()?))
}

fn seconds_parts(s: Decimal) -> (i64, String) {
    let s = s.to_string();
    match s.split_once('.') {
        Some((w, f)) => (w.parse().unwrap_or(0), f.to_owned()),
        None => (s.parse().unwrap_or(0), String::new()),
    }
}

/// The parts of an `xsd:date`, `xsd:dateTime` or `xsd:time` literal.
fn parts(t: &Term) -> Option<Parts> {
    let Term::Literal(l) = t else { return None };
    let dt = l.datatype();
    let v = l.value();
    if dt == xsd::DATE_TIME || dt == xsd::DATE_TIME_STAMP {
        let d = DateTime::from_str(v).ok()?;
        let (second, fraction) = seconds_parts(d.second());
        Some(Parts {
            year: Some(d.year()),
            month: Some(d.month().into()),
            day: Some(d.day().into()),
            hour: Some(d.hour().into()),
            minute: Some(d.minute().into()),
            second: Some(second),
            fraction,
            tz: tz_minutes(d.timezone_offset()),
        })
    } else if dt == xsd::DATE {
        let d = Date::from_str(v).ok()?;
        Some(Parts {
            year: Some(d.year()),
            month: Some(d.month().into()),
            day: Some(d.day().into()),
            tz: tz_minutes(d.timezone_offset()),
            ..Parts::default()
        })
    } else if dt == xsd::TIME {
        let d = Time::from_str(v).ok()?;
        let (second, fraction) = seconds_parts(d.second());
        Some(Parts {
            hour: Some(d.hour().into()),
            minute: Some(d.minute().into()),
            second: Some(second),
            fraction,
            tz: tz_minutes(d.timezone_offset()),
            ..Parts::default()
        })
    } else {
        None
    }
}

fn format_offset(minutes: i64, colon: bool, z: bool) -> String {
    if minutes == 0 && z {
        return "Z".to_owned();
    }
    let sign = if minutes < 0 { '-' } else { '+' };
    let (h, m) = (minutes.abs() / 60, minutes.abs() % 60);
    if colon {
        format!("{sign}{h:02}:{m:02}")
    } else {
        format!("{sign}{h:02}{m:02}")
    }
}

/// A timezone argument: `Z`, `UTC`, `GMT`, `+01:00`, `+0100` or `+01`.
fn parse_offset(s: &str) -> Option<i64> {
    let s = s.trim();
    if matches!(s, "Z" | "UTC" | "GMT" | "z") {
        return Some(0);
    }
    let s = s
        .strip_prefix("UTC")
        .or_else(|| s.strip_prefix("GMT"))
        .unwrap_or(s);
    let sign = match s.chars().next()? {
        '+' => 1,
        '-' => -1,
        _ => return None,
    };
    let digits: String = s[1..].chars().filter(char::is_ascii_digit).collect();
    let (h, m) = match digits.len() {
        1 | 2 => (digits.parse::<i64>().ok()?, 0),
        3 | 4 => {
            let split = digits.len() - 2;
            (
                digits[..split].parse::<i64>().ok()?,
                digits[split..].parse::<i64>().ok()?,
            )
        }
        _ => return None,
    };
    (h <= 14 && m < 60).then_some(sign * (h * 60 + m))
}

/// A token of a Java date pattern.
#[derive(Debug, PartialEq)]
enum Token {
    Field(char, usize),
    Text(String),
}

fn java_tokens(pattern: &str) -> Option<Vec<Token>> {
    let mut out = Vec::new();
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            // Quoted text; '' is a quote.
            if chars.get(i + 1) == Some(&'\'') {
                out.push(Token::Text("'".into()));
                i += 2;
                continue;
            }
            let mut text = String::new();
            i += 1;
            loop {
                match chars.get(i)? {
                    '\'' if chars.get(i + 1) == Some(&'\'') => {
                        text.push('\'');
                        i += 2;
                    }
                    '\'' => {
                        i += 1;
                        break;
                    }
                    &c => {
                        text.push(c);
                        i += 1;
                    }
                }
            }
            out.push(Token::Text(text));
        } else if c.is_ascii_alphabetic() {
            let mut n = 1;
            while chars.get(i + n) == Some(&c) {
                n += 1;
            }
            if !"yuMLdHhkKmsSaEXxZzVn".contains(c) {
                return None;
            }
            out.push(Token::Field(c, n));
            i += n;
        } else {
            out.push(Token::Text(c.to_string()));
            i += 1;
        }
    }
    Some(out)
}

fn is_numeric_field(t: Option<&Token>) -> bool {
    matches!(t, Some(Token::Field(c, n)) if "yuMLdHhkKmsSn".contains(*c) && !("ML".contains(*c) && *n >= 3))
}

/// Parses `text` with a Java date pattern (`dd.MM.yyyy`, `yyyy-MM-dd HH:mm:ss`, `d MMM
/// yyyy`, `yyyyMMdd'T'HHmmssX`, ...).
fn parse_java(text: &str, pattern: &str) -> Option<Parts> {
    let tokens = java_tokens(pattern)?;
    let mut p = Parts::default();
    let mut pm: Option<bool> = None;
    let mut hour12: Option<i64> = None;
    let s: Vec<char> = text.trim().chars().collect();
    let mut i = 0;
    for (k, t) in tokens.iter().enumerate() {
        match t {
            Token::Text(lit) => {
                for c in lit.chars() {
                    if c == ' ' {
                        // A space in the pattern matches any run of white space.
                        while s.get(i).is_some_and(|c| c.is_whitespace()) {
                            i += 1;
                        }
                        continue;
                    }
                    if s.get(i)?.to_lowercase().ne(c.to_lowercase()) {
                        return None;
                    }
                    i += 1;
                }
            }
            Token::Field(c, n) => {
                let fixed = is_numeric_field(tokens.get(k + 1));
                let mut number = |max: usize| -> Option<i64> {
                    let negative = *c == 'y' && s.get(i) == Some(&'-');
                    if negative {
                        i += 1;
                    }
                    let start = i;
                    let limit = if fixed { (*n).max(1) } else { max.max(*n) };
                    while i < s.len() && i - start < limit && s[i].is_ascii_digit() {
                        i += 1;
                    }
                    if i == start || (fixed && *n > 1 && i - start != *n) {
                        return None;
                    }
                    let v: i64 = s[start..i].iter().collect::<String>().parse().ok()?;
                    Some(if negative { -v } else { v })
                };
                match c {
                    'y' | 'u' => {
                        let v = number(9)?;
                        p.year = Some(if *n == 2 { 2000 + v } else { v });
                    }
                    'M' | 'L' if *n >= 3 => {
                        let rest: String = s[i..].iter().collect::<String>().to_lowercase();
                        let (m, len) = MONTHS.iter().enumerate().find_map(|(m, name)| {
                            let name = name.to_lowercase();
                            if *n == 3 {
                                rest.starts_with(&name[..3]).then_some((m, 3))
                            } else {
                                rest.starts_with(&name).then_some((m, name.len()))
                            }
                        })?;
                        p.month = Some(m as i64 + 1);
                        i += len;
                    }
                    'M' | 'L' => p.month = Some(number(2)?),
                    'd' => p.day = Some(number(2)?),
                    'H' => p.hour = Some(number(2)?),
                    'k' => p.hour = Some(number(2)? % 24),
                    'h' | 'K' => hour12 = Some(number(2)? % 12),
                    'm' => p.minute = Some(number(2)?),
                    's' => p.second = Some(number(2)?),
                    'S' => {
                        let start = i;
                        let limit = if fixed { *n } else { 9 };
                        while i < s.len() && i - start < limit && s[i].is_ascii_digit() {
                            i += 1;
                        }
                        if i == start {
                            return None;
                        }
                        p.fraction = s[start..i].iter().collect();
                    }
                    'a' => {
                        let rest: String = s[i..].iter().take(2).collect::<String>().to_uppercase();
                        pm = Some(match rest.as_str() {
                            "AM" => false,
                            "PM" => true,
                            _ => return None,
                        });
                        i += 2;
                    }
                    'E' => {
                        let rest: String = s[i..].iter().collect::<String>().to_lowercase();
                        let len = DAYS.iter().find_map(|d| {
                            let d = d.to_lowercase();
                            if rest.starts_with(&d) {
                                Some(d.len())
                            } else {
                                rest.starts_with(&d[..3]).then_some(3)
                            }
                        })?;
                        i += len;
                    }
                    'X' | 'x' | 'Z' | 'z' | 'V' => {
                        let rest: String = s[i..].iter().collect();
                        let len = rest
                            .char_indices()
                            .find(|&(j, ch)| {
                                !(ch.is_ascii_digit()
                                    || ch == ':'
                                    || (j == 0 && matches!(ch, '+' | '-' | 'Z' | 'z'))
                                    || (ch.is_ascii_alphabetic()
                                        && rest[..j].chars().all(|c| c.is_ascii_alphabetic())))
                            })
                            .map_or(rest.len(), |(j, _)| j);
                        // `+01:00` after `UTC` or `GMT` belongs to the zone too.
                        let mut len = len;
                        if matches!(&rest[..len], "UTC" | "GMT") {
                            len += rest[len..]
                                .char_indices()
                                .find(|&(j, ch)| {
                                    !(ch.is_ascii_digit()
                                        || ch == ':'
                                        || (j == 0 && matches!(ch, '+' | '-')))
                                })
                                .map_or(rest.len() - len, |(j, _)| j);
                        }
                        p.tz = Some(parse_offset(&rest[..len])?);
                        i += rest[..len].chars().count();
                    }
                    'n' => {
                        let start = i;
                        while i < s.len() && s[i].is_ascii_digit() {
                            i += 1;
                        }
                        p.fraction = s[start..i].iter().collect();
                    }
                    _ => return None,
                }
            }
        }
    }
    if i != s.len() {
        return None;
    }
    if let Some(h) = hour12 {
        p.hour = Some(h + if pm == Some(true) { 12 } else { 0 });
    } else if let (Some(h), Some(true)) = (p.hour, pm)
        && h < 12
    {
        p.hour = Some(h + 12);
    }
    p.year?;
    Some(p)
}

/// The `xsd:date` or `xsd:dateTime` literal of parsed parts, validated.
fn to_literal(mut p: Parts, tz: Option<i64>) -> Option<Term> {
    if p.tz.is_none() {
        p.tz = tz;
    }
    let tz =
        p.tz.map(|m| format_offset(m, true, true))
            .unwrap_or_default();
    let year = p.year?;
    let year = if year < 0 {
        format!("-{:04}", -year)
    } else {
        format!("{year:04}")
    };
    let date = format!(
        "{year}-{:02}-{:02}",
        p.month.unwrap_or(1),
        p.day.unwrap_or(1)
    );
    if p.has_time() {
        let fraction = if p.fraction.is_empty() {
            String::new()
        } else {
            format!(".{}", p.fraction)
        };
        let lexical = format!(
            "{date}T{:02}:{:02}:{:02}{fraction}{tz}",
            p.hour.unwrap_or(0),
            p.minute.unwrap_or(0),
            p.second.unwrap_or(0)
        );
        let v = DateTime::from_str(&lexical).ok()?;
        Some(Literal::new_typed_literal(v.to_string(), xsd::DATE_TIME).into())
    } else {
        let v = Date::from_str(&format!("{date}{tz}")).ok()?;
        Some(Literal::new_typed_literal(v.to_string(), xsd::DATE).into())
    }
}

fn pad(v: i64, width: usize) -> String {
    if v < 0 {
        format!("-{:0width$}", -v)
    } else {
        format!("{v:0width$}")
    }
}

/// Formats date parts with a Java pattern.
fn format_java(p: &Parts, pattern: &str) -> Option<String> {
    let mut out = String::new();
    for t in java_tokens(pattern)? {
        match t {
            Token::Text(s) => out.push_str(&s),
            Token::Field(c, n) => out.push_str(&match c {
                'y' | 'u' if n == 2 => pad(p.year?.rem_euclid(100), 2),
                'y' | 'u' => pad(p.year?, n),
                'M' | 'L' if n == 3 => MONTHS[usize::try_from(p.month? - 1).ok()?][..3].to_owned(),
                'M' | 'L' if n >= 4 => MONTHS[usize::try_from(p.month? - 1).ok()?].to_owned(),
                'M' | 'L' => pad(p.month?, n),
                'd' => pad(p.day?, n),
                'D' => pad(p.day_of_year()?, n),
                'E' if n >= 4 => DAYS[p.weekday()?].to_owned(),
                'E' => DAYS[p.weekday()?][..3].to_owned(),
                'H' => pad(p.hour?, n),
                'k' => pad(if p.hour? == 0 { 24 } else { p.hour? }, n),
                'h' => pad(if p.hour? % 12 == 0 { 12 } else { p.hour? % 12 }, n),
                'K' => pad(p.hour? % 12, n),
                'a' => (if p.hour? < 12 { "AM" } else { "PM" }).to_owned(),
                'm' => pad(p.minute?, n),
                's' => pad(p.second?, n),
                'S' => {
                    let mut f: String = p.fraction.chars().take(n).collect();
                    while f.len() < n {
                        f.push('0');
                    }
                    f
                }
                'X' if n == 1 && p.tz? % 60 == 0 && p.tz? != 0 => {
                    format_offset(p.tz?, false, true)[..3].to_owned()
                }
                'X' => format_offset(p.tz?, n >= 3, true),
                'x' => format_offset(p.tz?, n >= 3, false),
                'Z' => format_offset(p.tz?, n >= 4, n >= 5),
                'z' | 'V' => {
                    if p.tz? == 0 {
                        "UTC".to_owned()
                    } else {
                        format!("UTC{}", format_offset(p.tz?, true, false))
                    }
                }
                _ => return None,
            }),
        }
    }
    Some(out)
}

/// One `[...]` component of an XPath picture string: letter, presentation, widths.
fn format_component(p: &Parts, marker: &str) -> Option<String> {
    let marker: String = marker.chars().filter(|c| !c.is_whitespace()).collect();
    let mut chars = marker.chars();
    let component = chars.next()?;
    let rest: String = chars.collect();
    let (presentation, width) = match rest.split_once(',') {
        Some((p, w)) => (p.to_owned(), Some(w.to_owned())),
        None => (rest, None),
    };
    // `,min-max`: `*` for no limit.
    let (min_width, max_width) = match &width {
        Some(w) => {
            let (a, b) = w.split_once('-').unwrap_or((w.as_str(), "*"));
            (a.parse::<usize>().ok(), b.parse::<usize>().ok())
        }
        None => (None, None),
    };
    let presentation = presentation.trim_end_matches(['o', 't', 'c']);
    let name = |s: &str| -> String {
        let s = match presentation {
            "N" => s.to_uppercase(),
            "n" => s.to_lowercase(),
            _ => s.to_owned(),
        };
        match max_width {
            Some(m) => s.chars().take(m).collect(),
            None => s,
        }
    };
    let number = |v: i64, default: &str| -> String {
        let pres = if presentation.is_empty() {
            default
        } else {
            presentation
        };
        let digits = pres
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '#')
            .count();
        let mandatory = pres.chars().filter(char::is_ascii_digit).count().max(1);
        let mut s = pad(v, mandatory.max(min_width.unwrap_or(0)));
        if let (Some(m), true) = (max_width, digits > 0) {
            // A two-digit year: the last digits.
            if s.len() > m {
                s = s[s.len() - m..].to_owned();
            }
        }
        s
    };
    let is_name = matches!(presentation, "N" | "n" | "Nn");
    Some(match component {
        'Y' => {
            let y = p.year?;
            let digits = presentation.chars().filter(char::is_ascii_digit).count();
            if digits == 2 && max_width.is_none() {
                pad(y.rem_euclid(100), 2)
            } else {
                number(y, "1")
            }
        }
        'M' if is_name => name(MONTHS[usize::try_from(p.month? - 1).ok()?]),
        'M' => number(p.month?, "1"),
        'D' => number(p.day?, "1"),
        'd' => number(p.day_of_year()?, "1"),
        'F' if is_name || presentation.is_empty() => {
            let n = DAYS[p.weekday()?];
            if presentation.is_empty() {
                name(&n.to_lowercase())
            } else {
                name(n)
            }
        }
        'F' => number(p.weekday()? as i64 + 1, "1"),
        'H' => number(p.hour?, "1"),
        'h' => number(if p.hour? % 12 == 0 { 12 } else { p.hour? % 12 }, "1"),
        'P' => {
            let s = if p.hour? < 12 { "am" } else { "pm" };
            match presentation {
                "N" => s.to_uppercase(),
                "Nn" => format!("{}m", s[..1].to_uppercase()),
                _ => s.to_owned(),
            }
        }
        'm' => number(p.minute?, "01"),
        's' => number(p.second?, "01"),
        'f' => {
            let digits = presentation
                .chars()
                .filter(char::is_ascii_digit)
                .count()
                .max(1);
            let digits = max_width.unwrap_or(digits).max(digits);
            let mut f: String = p.fraction.chars().take(digits).collect();
            let min = presentation
                .chars()
                .filter(char::is_ascii_digit)
                .count()
                .max(1);
            while f.len() < min {
                f.push('0');
            }
            f
        }
        'Z' => {
            let tz = p.tz?;
            match presentation {
                "Z" => format_offset(tz, true, true),
                "0" | "1" => {
                    let h = tz.abs() / 60;
                    let m = tz.abs() % 60;
                    let sign = if tz < 0 { '-' } else { '+' };
                    if m == 0 {
                        format!("{sign}{h}")
                    } else {
                        format!("{sign}{h}:{m:02}")
                    }
                }
                "0000" => format_offset(tz, false, false),
                _ => format_offset(tz, true, false),
            }
        }
        'z' => {
            let tz = p.tz?;
            if tz == 0 {
                "GMT".to_owned()
            } else {
                format!("GMT{}", format_offset(tz, true, false))
            }
        }
        _ => return None,
    })
}

/// `fn:format-dateTime` / `-date` / `-time`: `[Y0001]-[M01]-[D01]`, `[D] [MNn] [Y]`,
/// `[H01]:[m01]:[s01].[f001] [Z]`, `[FNn,*-3]`. `[[` and `]]` are literal brackets.
fn format_picture(p: &Parts, picture: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = picture.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '[' if chars.peek() == Some(&'[') => {
                chars.next();
                out.push('[');
            }
            ']' if chars.peek() == Some(&']') => {
                chars.next();
                out.push(']');
            }
            '[' => {
                let mut marker = String::new();
                loop {
                    match chars.next()? {
                        ']' => break,
                        c => marker.push(c),
                    }
                }
                out.push_str(&format_component(p, &marker)?);
            }
            ']' => return None,
            c => out.push(c),
        }
    }
    Some(out)
}

fn civil_days(p: &Parts) -> Option<i64> {
    Some(days_from_civil(p.year?, p.month?, p.day?))
}

/// Whole months from `a` to `b` (negative when `b` is earlier).
fn months_between(a: &Parts, b: &Parts) -> Option<i64> {
    let mut months = (b.year? * 12 + b.month?) - (a.year? * 12 + a.month?);
    if months > 0 && b.day? < a.day? {
        months -= 1;
    } else if months < 0 && b.day? > a.day? {
        months += 1;
    }
    Some(months)
}

// ---------------------------------------------------------------------------------------------
// Strings
// ---------------------------------------------------------------------------------------------

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn jaro(a: &[char], b: &[char]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let window = (a.len().max(b.len()) / 2).saturating_sub(1);
    let mut b_used = vec![false; b.len()];
    let mut a_matches = Vec::new();
    for (i, ca) in a.iter().enumerate() {
        let lo = i.saturating_sub(window);
        let hi = (i + window + 1).min(b.len());
        for j in lo..hi {
            if !b_used[j] && b[j] == *ca {
                b_used[j] = true;
                a_matches.push(*ca);
                break;
            }
        }
    }
    let m = a_matches.len();
    if m == 0 {
        return 0.0;
    }
    let b_matches: Vec<char> = b
        .iter()
        .zip(&b_used)
        .filter(|(_, u)| **u)
        .map(|(c, _)| *c)
        .collect();
    let transpositions = a_matches
        .iter()
        .zip(&b_matches)
        .filter(|(x, y)| x != y)
        .count()
        / 2;
    let m = m as f64;
    (m / a.len() as f64 + m / b.len() as f64 + (m - transpositions as f64) / m) / 3.0
}

fn jaro_winkler(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let j = jaro(&a, &b);
    let prefix = a.iter().zip(&b).take(4).take_while(|(x, y)| x == y).count();
    j + prefix as f64 * 0.1 * (1.0 - j)
}

/// Jaccard similarity of the sets of character `n`-grams.
fn ngram_similarity(a: &str, b: &str, n: usize) -> f64 {
    use std::collections::HashSet;
    let grams = |s: &str| -> HashSet<Vec<char>> {
        let c: Vec<char> = s.chars().collect();
        if c.len() < n {
            return std::iter::once(c).filter(|c| !c.is_empty()).collect();
        }
        c.windows(n).map(<[char]>::to_vec).collect()
    };
    let (x, y) = (grams(a), grams(b));
    if x.is_empty() && y.is_empty() {
        return 1.0;
    }
    x.intersection(&y).count() as f64 / x.union(&y).count() as f64
}

/// Kölner Phonetik, as Apache Commons Codec's `ColognePhonetic`.
fn cologne(s: &str) -> String {
    let letters: Vec<char> = s
        .chars()
        .flat_map(char::to_uppercase)
        .map(|c| match c {
            'Ä' => 'A',
            'Ö' => 'O',
            'Ü' => 'U',
            'ß' => 'S',
            c => c,
        })
        .filter(char::is_ascii_uppercase)
        .collect();
    let mut out = String::new();
    let mut last_code = '/';
    let mut last_char = '-';
    for (i, &c) in letters.iter().enumerate() {
        let next = letters.get(i + 1).copied().unwrap_or('_');
        let mut code = match c {
            'A' | 'E' | 'I' | 'J' | 'O' | 'U' | 'Y' => '0',
            'H' => '-',
            'B' => '1',
            'P' if next == 'H' => '3',
            'P' => '1',
            'D' | 'T' if matches!(next, 'C' | 'S' | 'Z') => '8',
            'D' | 'T' => '2',
            'F' | 'V' | 'W' => '3',
            'G' | 'K' | 'Q' => '4',
            'X' if !matches!(last_char, 'C' | 'K' | 'Q') => {
                out.push('4');
                '8'
            }
            'X' => '8',
            'S' | 'Z' => '8',
            'C' if last_code == '/' => {
                if matches!(next, 'A' | 'H' | 'K' | 'L' | 'O' | 'Q' | 'R' | 'U' | 'X') {
                    '4'
                } else {
                    '8'
                }
            }
            'C' if matches!(last_char, 'S' | 'Z') => '8',
            'C' if matches!(next, 'A' | 'H' | 'K' | 'O' | 'Q' | 'U' | 'X') => '4',
            'C' => '8',
            'L' => '5',
            'M' | 'N' => '6',
            'R' => '7',
            _ => '-',
        };
        if code != '-' && code != last_code && (code != '0' || last_code == '/') {
            out.push(code);
        }
        if code == '-' && c != 'H' {
            code = last_code;
        }
        last_code = code;
        last_char = c;
    }
    out
}

/// Words of an identifier or phrase: split at non-alphanumerics and at lower-to-upper case
/// changes.
fn words(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn capitalize(w: &str) -> String {
    let mut c = w.chars();
    match c.next() {
        Some(f) => f
            .to_uppercase()
            .chain(c.flat_map(char::to_lowercase))
            .collect(),
        None => String::new(),
    }
}

fn pad_string(s: &str, width: i64, fill: &str, left: bool) -> Option<String> {
    let len = s.chars().count() as i64;
    if width <= len {
        return Some(s.to_owned());
    }
    let fill: Vec<char> = fill.chars().collect();
    if fill.is_empty() {
        return None;
    }
    let padding: String = fill
        .iter()
        .cycle()
        .take(usize::try_from(width - len).ok()?)
        .collect();
    Some(if left {
        format!("{padding}{s}")
    } else {
        format!("{s}{padding}")
    })
}

fn split_parts(s: &str) -> Vec<&str> {
    let mut parts: Vec<&str> = s.split('\u{1}').collect();
    while parts.len() > 1 && parts.last() == Some(&"") {
        parts.pop();
    }
    parts
}

// ---------------------------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------------------------

fn double(x: f64) -> Option<Term> {
    Some(Literal::from(x).into())
}

fn string1(f: fn(&str) -> String) -> Func {
    func(move |a| match a {
        [s] => string_term(f(string(s)?)),
        _ => None,
    })
}

/// Every custom function of this module, by IRI.
pub fn functions() -> Vec<(NamedNode, Func)> {
    let mut out: Vec<(String, Func)> = Vec::new();
    let mut add = |iris: &[String], f: Func| {
        for iri in iris {
            out.push((iri.clone(), Arc::clone(&f)));
        }
    };
    let mg = |n: &str| format!("{MG}{n}");
    let spif = |n: &str| format!("{SPIF}{n}");
    let ofn = |n: &str| format!("{OFN}{n}");
    let xfn = |n: &str| format!("{FN}{n}");

    // IRIs and templates
    add(&[SKOLEM_KEY.to_owned()], func(skolem_key));
    add(
        &[IRI_STRING.to_owned()],
        func(|a| {
            let (t, args) = a.split_first()?;
            string_term(fill(str_of(t)?, args, encode_for_uri)?)
        }),
    );
    add(
        &[URI_STRING.to_owned()],
        func(|a| {
            let (t, args) = a.split_first()?;
            string_term(fill(str_of(t)?, args, str::to_owned)?)
        }),
    );
    // R2RML's IRI-safe form of a value in an IRI template (`tripletrail lift --as-sparql`).
    add(
        &[mg("encodeForIri")],
        func(|a| match a {
            [s] => string_term(encode_for_iri(str_of(s)?)),
            _ => None,
        }),
    );
    add(
        &[mg("format"), spif("buildString")],
        func(|a| {
            let (t, args) = a.split_first()?;
            string_term(fill(str_of(t)?, args, str::to_owned)?)
        }),
    );

    // The parts of `apf:strSplit` (see `propfn`): split at U+0001, trailing empty parts
    // dropped as Java's `String.split` does.
    add(
        &["http://w3id.org/tripletrail/fn#splitParts".to_owned()],
        func(|a| match a {
            [s] => {
                let n = split_parts(string(s)?).len();
                (n <= 32_768).then(|| Literal::from(n as i64).into())
            }
            _ => None,
        }),
    );
    add(
        &["http://w3id.org/tripletrail/fn#splitPart".to_owned()],
        func(|a| match a {
            [s, i] => {
                string_term(*split_parts(string(s)?).get(usize::try_from(int_arg(i)?).ok()?)?)
            }
            _ => None,
        }),
    );

    // Dates
    add(
        &[mg("parseDate"), spif("parseDate")],
        func(|a| {
            let (text, pattern, tz) = match a {
                [t, p] => (t, p, None),
                [t, p, z] => (t, p, Some(parse_offset(str_of(z)?)?)),
                _ => return None,
            };
            to_literal(parse_java(str_of(text)?, string(pattern)?)?, tz)
        }),
    );
    add(
        &[mg("formatDate"), spif("dateFormat")],
        func(|a| match a {
            [d, p] => string_term(format_java(&parts(d)?, string(p)?)?),
            _ => None,
        }),
    );
    for name in ["format-dateTime", "format-date", "format-time"] {
        add(
            &[xfn(name)],
            func(|a| match a {
                // Language, calendar and place are accepted and ignored (English, ISO).
                [d, p, ..] if a.len() == 2 || a.len() == 5 => {
                    string_term(format_picture(&parts(d)?, string(p)?)?)
                }
                _ => None,
            }),
        );
    }
    add(
        &[mg("daysBetween"), ofn("daysBetween")],
        func(|a| match a {
            [x, y] => Some(Literal::from(civil_days(&parts(y)?)? - civil_days(&parts(x)?)?).into()),
            _ => None,
        }),
    );
    add(
        &[mg("monthsBetween"), ofn("monthsBetween")],
        func(|a| match a {
            [x, y] => Some(Literal::from(months_between(&parts(x)?, &parts(y)?)?).into()),
            _ => None,
        }),
    );
    add(
        &[mg("yearsBetween"), ofn("yearsBetween")],
        func(|a| match a {
            [x, y] => Some(Literal::from(months_between(&parts(x)?, &parts(y)?)? / 12).into()),
            _ => None,
        }),
    );

    // Similarity and phonetic codes
    add(
        &[mg("levenshtein")],
        func(|a| match a {
            [x, y] => Some(Literal::from(levenshtein(string(x)?, string(y)?) as i64).into()),
            _ => None,
        }),
    );
    add(
        &[mg("jaroWinkler")],
        func(|a| match a {
            [x, y] => double(jaro_winkler(string(x)?, string(y)?)),
            _ => None,
        }),
    );
    add(
        &[mg("ngramSimilarity")],
        func(|a| {
            let (x, y, n) = match a {
                [x, y] => (x, y, 2),
                [x, y, n] => (x, y, usize::try_from(int_arg(n)?).ok().filter(|n| *n > 0)?),
                _ => return None,
            };
            double(ngram_similarity(string(x)?, string(y)?, n))
        }),
    );
    add(&[mg("colognePhonetic")], string1(cologne));

    // Padding, case and white space
    for (name, left) in [("leftPad", true), ("rightPad", false)] {
        out.push((
            mg(name),
            func(move |a| {
                let (s, w, fill) = match a {
                    [s, w] => (s, w, " "),
                    [s, w, f] => (s, w, string(f)?),
                    _ => return None,
                };
                string_term(pad_string(str_of(s)?, int_arg(w)?, fill, left)?)
            }),
        ));
    }
    out.push((
        mg("titleCase"),
        string1(|s| s.split(' ').map(capitalize).collect::<Vec<_>>().join(" ")),
    ));
    out.push((
        spif("titleCase"),
        string1(|s| s.split(' ').map(capitalize).collect::<Vec<_>>().join(" ")),
    ));
    out.push((
        mg("camelCase"),
        string1(|s| {
            words(s)
                .iter()
                .enumerate()
                .map(|(i, w)| {
                    if i == 0 {
                        w.to_lowercase()
                    } else {
                        capitalize(w)
                    }
                })
                .collect()
        }),
    ));
    out.push((
        mg("snakeCase"),
        string1(|s| {
            words(s)
                .iter()
                .map(|w| w.to_lowercase())
                .collect::<Vec<_>>()
                .join("_")
        }),
    ));
    out.push((mg("trim"), string1(|s| s.trim().to_owned())));
    out.push((
        mg("normalizeWhitespace"),
        string1(|s| s.split_whitespace().collect::<Vec<_>>().join(" ")),
    ));

    // Embedding vectors (#236)
    let measure = |f: fn(&[f64], &[f64]) -> Option<f64>| -> Func {
        func(move |a| match a {
            [x, y] => double(f(&crate::vector::parse(x)?, &crate::vector::parse(y)?)?),
            _ => None,
        })
    };
    out.push((mg("cosine"), measure(crate::vector::cosine)));
    out.push((mg("dot"), measure(crate::vector::dot)));
    out.push((mg("l2"), measure(crate::vector::l2)));

    out.into_iter()
        .map(|(n, f)| (NamedNode::new_unchecked(n), f))
        .chain(crate::temporal::functions())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_for_iris_as_r2rml() {
        assert_eq!(encode_for_iri("a b/c?d#é~-._"), "a%20b%2Fc%3Fd%23é~-._");
        assert_eq!(encode_for_iri("Lillestrøm"), "Lillestrøm");
        assert_eq!(encode_for_uri("Lillestrøm"), "Lillestr%C3%B8m");
    }

    #[test]
    fn parses_java_patterns() {
        let p = |t, pat| {
            parse_java(t, pat)
                .and_then(|p| to_literal(p, None))
                .map(|t| t.to_string())
        };
        assert_eq!(
            p("31.10.2026", "dd.MM.yyyy").unwrap(),
            r#""2026-10-31"^^<http://www.w3.org/2001/XMLSchema#date>"#
        );
        assert_eq!(
            p("2026-10-31 14:05", "yyyy-MM-dd HH:mm").unwrap(),
            r#""2026-10-31T14:05:00"^^<http://www.w3.org/2001/XMLSchema#dateTime>"#
        );
        assert_eq!(
            p("20261031T140501Z", "yyyyMMdd'T'HHmmssX").unwrap(),
            r#""2026-10-31T14:05:01Z"^^<http://www.w3.org/2001/XMLSchema#dateTime>"#
        );
        assert_eq!(
            p("5 Nov 2026 3:07 PM", "d MMM yyyy h:mm a").unwrap(),
            r#""2026-11-05T15:07:00"^^<http://www.w3.org/2001/XMLSchema#dateTime>"#
        );
        assert_eq!(
            p("Thursday, 5 November 26", "EEEE, d MMMM yy").unwrap(),
            r#""2026-11-05"^^<http://www.w3.org/2001/XMLSchema#date>"#
        );
        assert_eq!(
            p(
                "2026-10-31T14:05:00.250+01:00",
                "yyyy-MM-dd'T'HH:mm:ss.SSSXXX"
            )
            .unwrap(),
            r#""2026-10-31T14:05:00.25+01:00"^^<http://www.w3.org/2001/XMLSchema#dateTime>"#
        );
        assert!(p("31.13.2026", "dd.MM.yyyy").is_none());
        assert!(p("31.10.2026x", "dd.MM.yyyy").is_none());
        assert!(p("31-10-2026", "dd.MM.yyyy").is_none());
    }

    #[test]
    fn formats_java_patterns_and_pictures() {
        let p = parts(&Term::from(Literal::new_typed_literal(
            "2026-11-09T14:05:07.5+01:00",
            xsd::DATE_TIME,
        )))
        .unwrap();
        assert_eq!(
            format_java(&p, "dd.MM.yyyy HH:mm:ss.SSS XXX").unwrap(),
            "09.11.2026 14:05:07.500 +01:00"
        );
        assert_eq!(
            format_java(&p, "EEE d MMM yy, h a").unwrap(),
            "Mon 9 Nov 26, 2 PM"
        );
        assert_eq!(format_java(&p, "'week''s' EEEE").unwrap(), "week's Monday");
        assert_eq!(
            format_picture(&p, "[Y0001]-[M01]-[D01]T[H01]:[m01]:[s01][Z]").unwrap(),
            "2026-11-09T14:05:07+01:00"
        );
        assert_eq!(
            format_picture(&p, "[FNn], [D] [MNn] [Y]").unwrap(),
            "Monday, 9 November 2026"
        );
        assert_eq!(
            format_picture(&p, "[FNn,*-3] [D1] [MNn,*-3] [Y01] [h]:[m01] [P]").unwrap(),
            "Mon 9 Nov 26 2:05 pm"
        );
        assert_eq!(format_picture(&p, "[[[D]]]").unwrap(), "[9]");
        assert_eq!(format_picture(&p, "[d]").unwrap(), "313");
        assert_eq!(
            format_picture(&p, "[H01]:[m01]:[s01].[f001]").unwrap(),
            "14:05:07.500"
        );
    }

    #[test]
    fn fills_templates() {
        let s = |v: &str| Term::from(Literal::new_simple_literal(v));
        let args = [s("L 12"), s("a/b")];
        assert_eq!(
            fill("http://e/line/{0}/{1}", &args, encode_for_uri).unwrap(),
            "http://e/line/L%2012/a%2Fb"
        );
        assert_eq!(fill("{}-{}", &args, str::to_owned).unwrap(), "L 12-a/b");
        assert_eq!(fill("{?2}{{x}}", &args, str::to_owned).unwrap(), "a/b{x}");
        assert!(fill("{2}", &args, str::to_owned).is_none());
    }

    #[test]
    fn measures_similarity() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "abc"), 3);
        assert!((jaro_winkler("MARTHA", "MARHTA") - 0.9611).abs() < 1e-3);
        assert!((jaro_winkler("DIXON", "DICKSONX") - 0.8133).abs() < 1e-3);
        assert_eq!(jaro_winkler("abc", "abc"), 1.0);
        assert!((ngram_similarity("night", "nacht", 2) - 1.0 / 7.0).abs() < 1e-9);
    }

    #[test]
    fn cologne_phonetic_matches_commons_codec() {
        assert_eq!(cologne("Müller-Lüdenscheidt"), "65752682");
        assert_eq!(cologne("Wikipedia"), "3412");
        assert_eq!(cologne("Breschnew"), "17863");
        assert_eq!(cologne("Meyer"), cologne("Maier"));
        assert_eq!(cologne("Christoph"), "47823");
        assert_eq!(cologne("Xaver"), "4837");
    }

    #[test]
    fn splits_words_and_pads() {
        assert_eq!(words("trackSection_ID 2"), ["track", "Section", "ID", "2"]);
        assert_eq!(pad_string("7", 3, "0", true).unwrap(), "007");
        assert_eq!(pad_string("ab", 5, "-=", false).unwrap(), "ab-=-");
        assert_eq!(pad_string("abcdef", 3, "0", true).unwrap(), "abcdef");
    }
}
