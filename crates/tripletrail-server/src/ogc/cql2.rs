//! CQL2 (OGC API – Features Part 3, `filter`): the text and JSON encodings, read into SPARQL
//! over a collection's queryables.
//!
//! What is read: Basic CQL2 (comparisons, `AND`, `OR`, `NOT`, `IS [NOT] NULL`, booleans),
//! the advanced comparison operators (`LIKE`, `BETWEEN`, `IN`), `CASEI`, comparisons of two
//! properties, timestamps and dates, and the spatial functions (`S_INTERSECTS`, `S_EQUALS`,
//! `S_DISJOINT`, `S_TOUCHES`, `S_WITHIN`, `S_OVERLAPS`, `S_CROSSES`, `S_CONTAINS`) on any
//! geometry, `BBOX` included, and the temporal functions (`T_AFTER`, `T_BEFORE`, …, on
//! instants and `INTERVAL`s). Not read: arithmetic, arrays (`A_*`), `ACCENTI` and custom
//! functions: they are refused with `400`.
//!
//! A property may have several values: a predicate holds when some values of its properties
//! make it true (`NOT` and `IS NULL` then read as "no value makes it true" and "no value").

use super::{GEOF, OutCrs, WKT, bbox};
use crate::HttpError;
use serde_json::Value;

const XSD_DATE_TIME: &str = "http://www.w3.org/2001/XMLSchema#dateTime";
const XSD_DATE: &str = "http://www.w3.org/2001/XMLSchema#date";
const XSD_DAY_TIME_DURATION: &str = "http://www.w3.org/2001/XMLSchema#dayTimeDuration";

/// An end of a temporal operand: a time, or open (`..`) before all times or after them.
#[derive(Clone, Debug)]
enum End {
    Early,
    At(String),
    Late,
}

impl End {
    fn rank(&self) -> u8 {
        match self {
            End::Early => 0,
            End::At(_) => 1,
            End::Late => 2,
        }
    }
}

/// Where a date ends: at its last millisecond, or (for meeting) at the next midnight.
#[derive(Clone, Copy)]
enum Last {
    Inside,
    Midnight,
}

/// `a op b` for two ends; an open end compares by where it lies.
fn compare(a: &End, op: &str, b: &End) -> String {
    if let (End::At(x), End::At(y)) = (a, b) {
        return format!("({x} {op} {y})");
    }
    let (x, y) = (a.rank(), b.rank());
    let holds = match op {
        "<" => x < y,
        ">" => x > y,
        "=" => x == y && x != 1,
        _ => unreachable!("only <, > and ="),
    };
    holds.to_string()
}

/// A temporal relation of the spans `[a1, a2]` and `[b1, b2]`, as CQL2 defines them.
fn temporal(relation: &str, a1: &End, a2: &End, b1: &End, b2: &End) -> String {
    let all = |tests: &[String]| format!("({})", tests.join(" && "));
    let c = compare;
    match relation {
        "t_after" => c(a1, ">", b2),
        "t_before" => c(a2, "<", b1),
        "t_contains" => all(&[c(a1, "<", b1), c(a2, ">", b2)]),
        "t_disjoint" => format!("({} || {})", c(a2, "<", b1), c(a1, ">", b2)),
        "t_during" => all(&[c(a1, ">", b1), c(a2, "<", b2)]),
        "t_equals" => all(&[c(a1, "=", b1), c(a2, "=", b2)]),
        "t_finishedby" => all(&[c(a1, "<", b1), c(a2, "=", b2)]),
        "t_finishes" => all(&[c(a1, ">", b1), c(a2, "=", b2)]),
        "t_intersects" => format!("!({} || {})", c(a2, "<", b1), c(a1, ">", b2)),
        "t_meets" => c(a2, "=", b1),
        "t_metby" => c(a1, "=", b2),
        "t_overlappedby" => all(&[c(a1, ">", b1), c(a1, "<", b2), c(a2, ">", b2)]),
        "t_overlaps" => all(&[c(a1, "<", b1), c(a2, ">", b1), c(a2, "<", b2)]),
        "t_startedby" => all(&[c(a1, "=", b1), c(a2, ">", b2)]),
        "t_starts" => all(&[c(a1, "=", b1), c(a2, "<", b2)]),
        _ => unreachable!("a T_* function"),
    }
}
/// Bounds on a filter, so that one cannot exhaust the parser's or the query engine's stack:
/// its length, how deep it nests (parentheses, `NOT`, `AND`/`OR`, `CASEI`) and how many
/// predicates it has. Beyond them it is refused with `400`.
const MAX_LENGTH: usize = 64 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_PREDICATES: usize = 200;
/// The most values all its `IN` lists may hold together, and the most vertices a geometry
/// in it may have: larger ones make queries that run into the timeout.
const MAX_IN_VALUES: usize = 1000;
const MAX_VERTICES: usize = 1000;

/// How deep an expression nests, how many predicates it has and how many values its `IN`
/// lists hold.
fn measure(e: &Expr) -> (usize, usize, usize) {
    match e {
        Expr::And(parts) | Expr::Or(parts) => parts
            .iter()
            .map(measure)
            .fold((0, 0, 0), |(d, n, v), (pd, pn, pv)| {
                (d.max(pd + 1), n + pn, v + pv)
            }),
        Expr::Not(inner) => {
            let (d, n, v) = measure(inner);
            (d + 1, n, v)
        }
        Expr::In(_, list) => (1, 1, list.len()),
        _ => (1, 1, 0),
    }
}

/// The queryable that names a feature's geometry (its first geometry way).
pub(super) const GEOMETRY: &str = "geometry";

/// The encoding of a `filter` (`filter-lang`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Lang {
    Text,
    Json,
}

impl Lang {
    pub(super) fn of(value: Option<&str>) -> Result<Self, HttpError> {
        match value {
            None | Some("cql2-text") => Ok(Lang::Text),
            Some("cql2-json") => Ok(Lang::Json),
            Some(other) => Err(bad(format!(
                "Unsupported filter-lang {other}: cql2-text or cql2-json"
            ))),
        }
    }
}

fn bad(message: impl Into<String>) -> HttpError {
    HttpError::bad_request(message.into())
}

#[derive(Clone, Debug, PartialEq)]
enum Scalar {
    Prop(String),
    Str(String),
    /// A number, as SPARQL writes it.
    Num(String),
    Bool(bool),
    Timestamp(String),
    Date(String),
    Casei(Box<Scalar>),
}

#[derive(Clone, Debug, PartialEq)]
enum Geom {
    Prop(String),
    Wkt(String),
    Bbox(Vec<f64>),
}

#[derive(Clone, Debug, PartialEq)]
enum Expr {
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Bool(bool),
    Cmp(&'static str, Scalar, Scalar),
    Like(Scalar, Scalar),
    Between(Scalar, Scalar, Scalar),
    In(Scalar, Vec<Scalar>),
    IsNull(Scalar),
    /// A GeoSPARQL Simple Features relation (`sfIntersects`, …) of two geometries.
    Spatial(&'static str, Geom, Geom),
    /// A temporal relation (`T_AFTER`, …) of two instants or intervals.
    Temporal(&'static str, Temporal, Temporal),
}

/// An operand of a temporal function: an instant (a property's value, a timestamp or a
/// date, which covers its day) or an interval of two of them.
#[derive(Clone, Debug, PartialEq)]
enum Temporal {
    Instant(Bound),
    Interval(Bound, Bound),
}

/// One end of a temporal operand; `Open` is `..`.
#[derive(Clone, Debug, PartialEq)]
enum Bound {
    Open,
    Prop(String),
    Timestamp(String),
    Date(String),
}

const TEMPORAL: &[&str] = &[
    "t_after",
    "t_before",
    "t_contains",
    "t_disjoint",
    "t_during",
    "t_equals",
    "t_finishedby",
    "t_finishes",
    "t_intersects",
    "t_meets",
    "t_metby",
    "t_overlappedby",
    "t_overlaps",
    "t_startedby",
    "t_starts",
];

const SPATIAL: &[(&str, &str)] = &[
    ("s_intersects", "sfIntersects"),
    ("s_equals", "sfEquals"),
    ("s_disjoint", "sfDisjoint"),
    ("s_touches", "sfTouches"),
    ("s_within", "sfWithin"),
    ("s_overlaps", "sfOverlaps"),
    ("s_crosses", "sfCrosses"),
    ("s_contains", "sfContains"),
];

const COMPARISONS: &[&str] = &["=", "<>", "<", ">", "<=", ">="];

const WKT_TYPES: &[&str] = &[
    "POINT",
    "LINESTRING",
    "POLYGON",
    "MULTIPOINT",
    "MULTILINESTRING",
    "MULTIPOLYGON",
    "GEOMETRYCOLLECTION",
];

/// The kind of term a property's values are, as [`Scope::forms`] finds them: a string
/// `IN` matches such values by their text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Form {
    /// An IRI.
    Iri,
    /// A blank node: its text (`STR`) is an error, so it never matches.
    Blank,
    /// A literal of datatype `xsd:string`.
    Str,
    /// A literal with this language tag (and base direction, as `@tag--dir`).
    Lang(String),
    /// A literal of any other datatype.
    Other,
}

/// What a filter is compiled for: the pattern that picks the collection's features (as `?f`),
/// and how to find the forms of a property's values.
pub(super) struct Scope<'a> {
    /// A pattern that binds `?f` to the collection's features, such as `?f a <C> .`.
    pub(super) features: String,
    /// The forms of the values of the features on a SPARQL path, or `None` when unknown.
    pub(super) forms: &'a mut FormsOf<'a>,
}

/// How [`Scope`] finds the forms of the values on a path.
pub(super) type FormsOf<'a> = dyn FnMut(&str) -> Result<Option<Vec<Form>>, HttpError> + 'a;

/// At most this many terms in the `VALUES` of a string `IN` matched by term (each value in
/// each form): more fall back to comparing the text of every value.
const MAX_IN_TERMS: usize = 20_000;

/// The SPARQL (`FILTER`s over `?f`, the feature, and `?w`, its geometry) that keeps the
/// features `filter` keeps. `properties` are the collection's queryables as `(key, SPARQL
/// path)`, besides [`GEOMETRY`]; geometries in the filter are in `crs`.
pub(super) fn to_sparql(
    filter: &str,
    lang: Lang,
    crs: OutCrs,
    properties: &[(String, String)],
    scope: &mut Scope<'_>,
) -> Result<String, HttpError> {
    if filter.len() > MAX_LENGTH {
        return Err(bad(format!("The filter is longer than {MAX_LENGTH} bytes")));
    }
    let expr = match lang {
        Lang::Text => Text::new(filter).parse()?,
        Lang::Json => {
            let value: Value =
                serde_json::from_str(filter).map_err(|e| bad(format!("Invalid CQL2 JSON: {e}")))?;
            json_expr(&value)?
        }
    };
    let (depth, predicates, values) = measure(&expr);
    if values > MAX_IN_VALUES {
        return Err(bad(format!(
            "The filter's IN lists hold more than {MAX_IN_VALUES} values"
        )));
    }
    if depth > MAX_DEPTH {
        return Err(bad(format!(
            "The filter nests deeper than {MAX_DEPTH} levels"
        )));
    }
    if predicates > MAX_PREDICATES {
        return Err(bad(format!(
            "The filter has more than {MAX_PREDICATES} predicates"
        )));
    }
    let mut compiler = Compiler {
        properties,
        crs,
        next: 0,
    };
    let conjuncts = match expr {
        Expr::And(parts) => parts,
        other => vec![other],
    };
    let mut out = String::new();
    for part in &conjuncts {
        match compiler.string_in(part, scope)? {
            Some(join) => out.push_str(&join),
            None => out.push_str(&format!(" FILTER({})", compiler.expr(part)?)),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// CQL2 text

#[derive(Clone, Debug, PartialEq)]
enum Token {
    /// A word: a keyword or a property name.
    Word(String),
    /// A double-quoted property name.
    Quoted(String),
    Str(String),
    Num(String),
    Sym(&'static str),
}

struct Text<'a> {
    s: &'a str,
    pos: usize,
    /// How deep the parser is in nested expressions, bounded by [`MAX_DEPTH`].
    depth: usize,
}

impl<'a> Text<'a> {
    fn new(s: &'a str) -> Self {
        Text {
            s,
            pos: 0,
            depth: 0,
        }
    }

    fn parse(mut self) -> Result<Expr, HttpError> {
        let expr = self.or()?;
        match self.next()? {
            None => Ok(expr),
            Some(t) => Err(bad(format!(
                "Unexpected {} at {} in the filter",
                show(&t),
                self.pos
            ))),
        }
    }

    fn skip_space(&mut self) {
        let rest = &self.s[self.pos..];
        self.pos += rest.len() - rest.trim_start().len();
    }

    fn peek(&mut self) -> Result<Option<Token>, HttpError> {
        let at = self.pos;
        let t = self.next();
        self.pos = at;
        t
    }

    fn next(&mut self) -> Result<Option<Token>, HttpError> {
        self.skip_space();
        let rest = &self.s[self.pos..];
        let Some(c) = rest.chars().next() else {
            return Ok(None);
        };
        for sym in ["<>", "<=", ">=", "=", "<", ">", "(", ")", ","] {
            if rest.starts_with(sym) {
                self.pos += sym.len();
                return Ok(Some(Token::Sym(sym)));
            }
        }
        if c == '\'' || c == '"' {
            let mut out = String::new();
            let mut chars = rest.char_indices().skip(1).peekable();
            while let Some((i, ch)) = chars.next() {
                if ch == c {
                    if chars.peek().map(|(_, n)| *n) == Some(c) {
                        chars.next();
                        out.push(c);
                        continue;
                    }
                    self.pos += i + 1;
                    return Ok(Some(if c == '\'' {
                        Token::Str(out)
                    } else {
                        Token::Quoted(out)
                    }));
                }
                out.push(ch);
            }
            return Err(bad("An unclosed quote in the filter"));
        }
        if c.is_ascii_digit() || ((c == '-' || c == '+' || c == '.') && rest.len() > 1) {
            let end = rest
                .char_indices()
                .skip(1)
                .find(|(i, ch)| {
                    !(ch.is_ascii_digit()
                        || *ch == '.'
                        || *ch == 'e'
                        || *ch == 'E'
                        || ((*ch == '-' || *ch == '+')
                            && matches!(rest.as_bytes()[i - 1], b'e' | b'E')))
                })
                .map_or(rest.len(), |(i, _)| i);
            let text = &rest[..end];
            self.pos += end;
            return Ok(Some(Token::Num(number(text)?)));
        }
        if c.is_alphabetic() || c == '_' {
            let end = rest
                .char_indices()
                .find(|(_, ch)| !(ch.is_alphanumeric() || matches!(ch, '_' | '.' | ':')))
                .map_or(rest.len(), |(i, _)| i);
            self.pos += end;
            return Ok(Some(Token::Word(rest[..end].to_owned())));
        }
        Err(bad(format!(
            "Unexpected '{c}' at {} in the filter",
            self.pos
        )))
    }

    fn expect(&mut self, sym: &'static str) -> Result<(), HttpError> {
        match self.next()? {
            Some(Token::Sym(s)) if s == sym => Ok(()),
            other => Err(bad(format!(
                "Expected '{sym}' at {} in the filter, found {}",
                self.pos,
                other.as_ref().map_or("the end".to_owned(), show)
            ))),
        }
    }

    /// Whether the next token is the keyword `word`; takes it if so.
    fn keyword(&mut self, word: &str) -> Result<bool, HttpError> {
        let at = self.pos;
        match self.next()? {
            Some(Token::Word(w)) if w.eq_ignore_ascii_case(word) => Ok(true),
            _ => {
                self.pos = at;
                Ok(false)
            }
        }
    }

    /// Goes one level deeper, or refuses a filter that nests too deep.
    fn enter(&mut self) -> Result<(), HttpError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(bad(format!(
                "The filter nests deeper than {MAX_DEPTH} levels"
            )));
        }
        Ok(())
    }

    fn or(&mut self) -> Result<Expr, HttpError> {
        self.enter()?;
        let out = self.or_inner();
        self.depth -= 1;
        out
    }

    fn or_inner(&mut self) -> Result<Expr, HttpError> {
        let mut parts = vec![self.and()?];
        while self.keyword("OR")? {
            parts.push(self.and()?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().expect("one")
        } else {
            Expr::Or(parts)
        })
    }

    fn and(&mut self) -> Result<Expr, HttpError> {
        let mut parts = vec![self.not()?];
        while self.keyword("AND")? {
            parts.push(self.not()?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().expect("one")
        } else {
            Expr::And(parts)
        })
    }

    fn not(&mut self) -> Result<Expr, HttpError> {
        if self.keyword("NOT")? {
            self.enter()?;
            let inner = self.not();
            self.depth -= 1;
            return Ok(Expr::Not(Box::new(inner?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, HttpError> {
        if self.peek()? == Some(Token::Sym("(")) {
            self.next()?;
            let inner = self.or()?;
            self.expect(")")?;
            return Ok(inner);
        }
        if let Some(Token::Word(w)) = self.peek()? {
            let lower = w.to_ascii_lowercase();
            if let Some((_, relation)) = SPATIAL.iter().find(|(name, _)| *name == lower) {
                self.next()?;
                self.expect("(")?;
                let a = self.geometry()?;
                self.expect(",")?;
                let b = self.geometry()?;
                self.expect(")")?;
                return Ok(Expr::Spatial(relation, a, b));
            }
            if let Some(relation) = TEMPORAL.iter().find(|name| **name == lower) {
                self.next()?;
                self.expect("(")?;
                let a = self.temporal()?;
                self.expect(",")?;
                let b = self.temporal()?;
                self.expect(")")?;
                return Ok(Expr::Temporal(relation, a, b));
            }
        }
        let left = self.scalar()?;
        let negated = self.keyword("NOT")?;
        if self.keyword("LIKE")? {
            let like = Expr::Like(left, self.scalar()?);
            return Ok(negate(like, negated));
        }
        if self.keyword("BETWEEN")? {
            let low = self.scalar()?;
            if !self.keyword("AND")? {
                return Err(bad("BETWEEN needs AND"));
            }
            let high = self.scalar()?;
            return Ok(negate(Expr::Between(left, low, high), negated));
        }
        if self.keyword("IN")? {
            self.expect("(")?;
            let mut list = vec![self.scalar()?];
            while self.peek()? == Some(Token::Sym(",")) {
                self.next()?;
                list.push(self.scalar()?);
            }
            self.expect(")")?;
            return Ok(negate(Expr::In(left, list), negated));
        }
        if negated {
            return Err(bad("NOT here needs LIKE, BETWEEN or IN"));
        }
        if self.keyword("IS")? {
            let not = self.keyword("NOT")?;
            if !self.keyword("NULL")? {
                return Err(bad("IS needs NULL"));
            }
            return Ok(negate(Expr::IsNull(left), not));
        }
        if let Some(Token::Sym(op)) = self.peek()?
            && COMPARISONS.contains(&op)
        {
            self.next()?;
            let right = self.scalar()?;
            return Ok(Expr::Cmp(op, left, right));
        }
        match left {
            Scalar::Bool(b) => Ok(Expr::Bool(b)),
            // A boolean property on its own.
            Scalar::Prop(_) => Ok(Expr::Cmp("=", left, Scalar::Bool(true))),
            _ => Err(bad(format!(
                "Expected a predicate at {} in the filter",
                self.pos
            ))),
        }
    }

    fn scalar(&mut self) -> Result<Scalar, HttpError> {
        match self.next()? {
            Some(Token::Str(s)) => Ok(Scalar::Str(s)),
            Some(Token::Num(n)) => Ok(Scalar::Num(n)),
            Some(Token::Quoted(p)) => Ok(Scalar::Prop(p)),
            Some(Token::Word(w)) => {
                let upper = w.to_ascii_uppercase();
                match upper.as_str() {
                    "TRUE" => return Ok(Scalar::Bool(true)),
                    "FALSE" => return Ok(Scalar::Bool(false)),
                    _ => {}
                }
                if self.peek()? != Some(Token::Sym("(")) {
                    return Ok(Scalar::Prop(w));
                }
                self.next()?;
                let out = match upper.as_str() {
                    "TIMESTAMP" | "DATE" => match self.next()? {
                        Some(Token::Str(s)) if upper == "DATE" => Scalar::Date(date(&s)?),
                        Some(Token::Str(s)) => Scalar::Timestamp(timestamp(&s)?),
                        _ => return Err(bad(format!("{upper} needs a string"))),
                    },
                    "CASEI" => {
                        self.enter()?;
                        let inner = self.scalar();
                        self.depth -= 1;
                        Scalar::Casei(Box::new(inner?))
                    }
                    _ => return Err(unsupported(&w)),
                };
                self.expect(")")?;
                Ok(out)
            }
            other => Err(bad(format!(
                "Expected a value at {} in the filter, found {}",
                self.pos,
                other.as_ref().map_or("the end".to_owned(), show)
            ))),
        }
    }

    fn temporal(&mut self) -> Result<Temporal, HttpError> {
        if self.keyword("INTERVAL")? {
            self.expect("(")?;
            let start = self.bound()?;
            self.expect(",")?;
            let end = self.bound()?;
            self.expect(")")?;
            return Ok(Temporal::Interval(start, end));
        }
        match self.bound()? {
            Bound::Open => Err(bad("'..' is only an end of an INTERVAL")),
            other => Ok(Temporal::Instant(other)),
        }
    }

    fn bound(&mut self) -> Result<Bound, HttpError> {
        match self.scalar()? {
            Scalar::Str(s) => text_bound(&s),
            other => scalar_bound(other),
        }
    }

    fn geometry(&mut self) -> Result<Geom, HttpError> {
        match self.next()? {
            Some(Token::Quoted(p)) => Ok(Geom::Prop(p)),
            Some(Token::Word(w)) => {
                let upper = w.to_ascii_uppercase();
                if upper == "BBOX" {
                    self.expect("(")?;
                    let mut numbers = Vec::new();
                    loop {
                        match self.next()? {
                            Some(Token::Num(n)) => numbers.push(n.parse::<f64>().unwrap_or(0.0)),
                            _ => return Err(bad("BBOX needs numbers")),
                        }
                        match self.next()? {
                            Some(Token::Sym(",")) => continue,
                            Some(Token::Sym(")")) => break,
                            _ => return Err(bad("BBOX needs numbers separated by commas")),
                        }
                    }
                    return Ok(Geom::Bbox(numbers));
                }
                if WKT_TYPES.contains(&upper.as_str()) {
                    let start = self.pos - w.len();
                    self.skip_space();
                    let rest = &self.s[self.pos..];
                    let end = if rest
                        .get(..5)
                        .is_some_and(|e| e.eq_ignore_ascii_case("EMPTY"))
                    {
                        5
                    } else {
                        balanced(rest)
                            .ok_or_else(|| bad(format!("Unclosed {upper} in the filter")))?
                    };
                    self.pos += end;
                    return Ok(Geom::Wkt(self.s[start..self.pos].to_owned()));
                }
                Ok(Geom::Prop(w))
            }
            other => Err(bad(format!(
                "Expected a geometry at {} in the filter, found {}",
                self.pos,
                other.as_ref().map_or("the end".to_owned(), show)
            ))),
        }
    }
}

/// The length of the parenthesised text `s` starts with (optionally after `Z`, `M` or `ZM`).
fn balanced(s: &str) -> Option<usize> {
    let open = s.find('(')?;
    if !s[..open]
        .trim()
        .chars()
        .all(|c| c.eq_ignore_ascii_case(&'z') || c.eq_ignore_ascii_case(&'m'))
    {
        return None;
    }
    let mut depth = 0usize;
    for (i, c) in s.char_indices().skip(open) {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

fn show(t: &Token) -> String {
    match t {
        Token::Word(w) => w.clone(),
        Token::Quoted(q) => format!("\"{q}\""),
        Token::Str(s) => format!("'{s}'"),
        Token::Num(n) => n.clone(),
        Token::Sym(s) => (*s).to_owned(),
    }
}

fn negate(expr: Expr, not: bool) -> Expr {
    if not { Expr::Not(Box::new(expr)) } else { expr }
}

fn unsupported(name: &str) -> HttpError {
    bad(format!(
        "The function {name} is not supported: CQL2 here has CASEI, TIMESTAMP, DATE, INTERVAL, \
         BBOX, the S_* spatial and the T_* temporal functions"
    ))
}

/// An end of an `INTERVAL` written as a string: `..`, a date or a timestamp.
fn text_bound(s: &str) -> Result<Bound, HttpError> {
    if s == ".." {
        Ok(Bound::Open)
    } else if s.len() == 10 {
        Ok(Bound::Date(date(s)?))
    } else {
        Ok(Bound::Timestamp(timestamp(s)?))
    }
}

fn scalar_bound(s: Scalar) -> Result<Bound, HttpError> {
    match s {
        Scalar::Prop(p) => Ok(Bound::Prop(p)),
        Scalar::Timestamp(t) => Ok(Bound::Timestamp(t)),
        Scalar::Date(d) => Ok(Bound::Date(d)),
        other => Err(bad(format!(
            "Expected an instant (a property, TIMESTAMP or DATE) in a temporal function, found {other:?}"
        ))),
    }
}

fn number(text: &str) -> Result<String, HttpError> {
    match text.parse::<f64>() {
        Ok(n) if n.is_finite() => Ok(n.to_string()),
        _ => Err(bad(format!("Invalid number {text} in the filter"))),
    }
}

fn timestamp(text: &str) -> Result<String, HttpError> {
    oxsdatatypes::DateTime::from_str(text)
        .ok()
        .filter(|d| d.timezone_offset().is_some())
        .map(|_| text.to_owned())
        .ok_or_else(|| bad(format!("Invalid TIMESTAMP {text}: an RFC 3339 date-time")))
}

fn date(text: &str) -> Result<String, HttpError> {
    oxsdatatypes::Date::from_str(text)
        .ok()
        .filter(|_| text.len() == 10)
        .map(|_| text.to_owned())
        .ok_or_else(|| bad(format!("Invalid DATE {text}: YYYY-MM-DD")))
}

use std::str::FromStr;

// ---------------------------------------------------------------------------------------
// CQL2 JSON

fn json_expr(v: &Value) -> Result<Expr, HttpError> {
    match v {
        Value::Bool(b) => Ok(Expr::Bool(*b)),
        Value::Object(o) => {
            let op = o
                .get("op")
                .and_then(Value::as_str)
                .ok_or_else(|| bad(format!("Expected a CQL2 JSON predicate, found {v}")))?;
            let args = o
                .get("args")
                .and_then(Value::as_array)
                .ok_or_else(|| bad(format!("The {op} needs args")))?;
            let lower = op.to_ascii_lowercase();
            let count = |n: usize| -> Result<(), HttpError> {
                if args.len() == n {
                    Ok(())
                } else {
                    Err(bad(format!("{op} takes {n} args")))
                }
            };
            match lower.as_str() {
                "and" | "or" => {
                    if args.len() < 2 {
                        return Err(bad(format!("{op} takes at least 2 args")));
                    }
                    let parts = args.iter().map(json_expr).collect::<Result<Vec<_>, _>>()?;
                    Ok(if lower == "and" {
                        Expr::And(parts)
                    } else {
                        Expr::Or(parts)
                    })
                }
                "not" => {
                    count(1)?;
                    Ok(Expr::Not(Box::new(json_expr(&args[0])?)))
                }
                "like" => {
                    count(2)?;
                    Ok(Expr::Like(json_scalar(&args[0])?, json_scalar(&args[1])?))
                }
                "between" => {
                    count(3)?;
                    Ok(Expr::Between(
                        json_scalar(&args[0])?,
                        json_scalar(&args[1])?,
                        json_scalar(&args[2])?,
                    ))
                }
                "in" => {
                    count(2)?;
                    let list = args[1]
                        .as_array()
                        .ok_or_else(|| bad("in takes a value and a list"))?
                        .iter()
                        .map(json_scalar)
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(Expr::In(json_scalar(&args[0])?, list))
                }
                "isnull" => {
                    count(1)?;
                    Ok(Expr::IsNull(json_scalar(&args[0])?))
                }
                _ => {
                    if let Some(cmp) = COMPARISONS.iter().find(|c| **c == op) {
                        count(2)?;
                        return Ok(Expr::Cmp(
                            cmp,
                            json_scalar(&args[0])?,
                            json_scalar(&args[1])?,
                        ));
                    }
                    if let Some(relation) = TEMPORAL.iter().find(|n| **n == lower) {
                        count(2)?;
                        return Ok(Expr::Temporal(
                            relation,
                            json_temporal(&args[0])?,
                            json_temporal(&args[1])?,
                        ));
                    }
                    if let Some((_, relation)) = SPATIAL.iter().find(|(n, _)| *n == lower) {
                        count(2)?;
                        return Ok(Expr::Spatial(
                            relation,
                            json_geometry(&args[0])?,
                            json_geometry(&args[1])?,
                        ));
                    }
                    Err(unsupported(op))
                }
            }
        }
        _ => Err(bad(format!("Expected a CQL2 JSON predicate, found {v}"))),
    }
}

fn json_scalar(v: &Value) -> Result<Scalar, HttpError> {
    match v {
        Value::String(s) => Ok(Scalar::Str(s.clone())),
        Value::Number(n) => Ok(Scalar::Num(number(&n.to_string())?)),
        Value::Bool(b) => Ok(Scalar::Bool(*b)),
        Value::Object(o) => {
            if let Some(p) = o.get("property").and_then(Value::as_str) {
                return Ok(Scalar::Prop(p.to_owned()));
            }
            if let Some(t) = o.get("timestamp").and_then(Value::as_str) {
                return Ok(Scalar::Timestamp(timestamp(t)?));
            }
            if let Some(d) = o.get("date").and_then(Value::as_str) {
                return Ok(Scalar::Date(date(d)?));
            }
            match o.get("op").and_then(Value::as_str) {
                Some(op) if op.eq_ignore_ascii_case("casei") => {
                    match o.get("args").and_then(Value::as_array).map(Vec::as_slice) {
                        Some([arg]) => Ok(Scalar::Casei(Box::new(json_scalar(arg)?))),
                        _ => Err(bad("casei takes 1 arg")),
                    }
                }
                Some(op) => Err(unsupported(op)),
                None => Err(bad(format!("Expected a CQL2 JSON value, found {v}"))),
            }
        }
        _ => Err(bad(format!("Expected a CQL2 JSON value, found {v}"))),
    }
}

fn json_temporal(v: &Value) -> Result<Temporal, HttpError> {
    if let Some(ends) = v.get("interval").and_then(Value::as_array) {
        let [start, end] = ends.as_slice() else {
            return Err(bad("interval takes a start and an end"));
        };
        let bound = |b: &Value| match b {
            Value::String(s) => text_bound(s),
            other => scalar_bound(json_scalar(other)?),
        };
        return Ok(Temporal::Interval(bound(start)?, bound(end)?));
    }
    match json_scalar(v)? {
        Scalar::Str(s) => Err(bad(format!(
            "Expected an instant in a temporal function, found the string {s}"
        ))),
        other => Ok(Temporal::Instant(scalar_bound(other)?)),
    }
}

fn json_geometry(v: &Value) -> Result<Geom, HttpError> {
    if let Some(p) = v.get("property").and_then(Value::as_str) {
        return Ok(Geom::Prop(p.to_owned()));
    }
    if let Some(b) = v.get("bbox").and_then(Value::as_array) {
        let numbers: Option<Vec<f64>> = b.iter().map(Value::as_f64).collect();
        return Ok(Geom::Bbox(
            numbers.ok_or_else(|| bad("bbox takes numbers"))?,
        ));
    }
    geojson_wkt(v).map(Geom::Wkt)
}

/// A GeoJSON geometry as WKT.
fn geojson_wkt(v: &Value) -> Result<String, HttpError> {
    let invalid = || bad(format!("Invalid GeoJSON geometry {v}"));
    let kind = v.get("type").and_then(Value::as_str).ok_or_else(invalid)?;
    if kind == "GeometryCollection" {
        let parts = v
            .get("geometries")
            .and_then(Value::as_array)
            .ok_or_else(invalid)?
            .iter()
            .map(geojson_wkt)
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(format!("GEOMETRYCOLLECTION({})", parts.join(", ")));
    }
    let c = v.get("coordinates").ok_or_else(invalid)?;
    fn position(c: &Value) -> Option<String> {
        let n: Option<Vec<String>> = c
            .as_array()?
            .iter()
            .map(|x| x.as_f64().map(|f| f.to_string()))
            .collect();
        let n = n?;
        (n.len() >= 2).then(|| n.join(" "))
    }
    fn list(c: &Value, depth: usize) -> Option<String> {
        if depth == 0 {
            return position(c);
        }
        let parts: Option<Vec<String>> = c.as_array()?.iter().map(|x| list(x, depth - 1)).collect();
        Some(format!("({})", parts?.join(", ")))
    }
    let (name, depth) = match kind {
        "Point" => ("POINT", 0),
        "LineString" => ("LINESTRING", 1),
        "Polygon" => ("POLYGON", 2),
        "MultiPoint" => ("MULTIPOINT", 1),
        "MultiLineString" => ("MULTILINESTRING", 2),
        "MultiPolygon" => ("MULTIPOLYGON", 3),
        _ => return Err(invalid()),
    };
    let body = list(c, depth).ok_or_else(invalid)?;
    Ok(if depth == 0 {
        format!("{name}({body})")
    } else {
        format!("{name}{body}")
    })
}

// ---------------------------------------------------------------------------------------
// SPARQL

/// What a scalar is, for how the other side of a comparison reads a property's value.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Prop,
    Str,
    Num,
    Bool,
    Timestamp,
    Date,
    Casei,
}

struct Compiler<'a> {
    properties: &'a [(String, String)],
    crs: OutCrs,
    next: usize,
}

/// The properties one predicate reads: each bound to a variable in its `EXISTS`.
#[derive(Default)]
struct Atom {
    vars: Vec<(String, String, String)>,
}

impl Atom {
    fn wrap(self, body: String) -> String {
        if self.vars.is_empty() {
            return body;
        }
        let patterns: Vec<String> = self
            .vars
            .iter()
            .map(|(_, path, var)| format!("?f {path} {var} ."))
            .collect();
        format!("EXISTS {{ {} FILTER({body}) }}", patterns.join(" "))
    }
}

impl Compiler<'_> {
    fn path(&self, key: &str) -> Result<&str, HttpError> {
        self.properties
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, p)| p.as_str())
            .ok_or_else(|| {
                bad(format!(
                    "{key} is not a queryable of this collection (see its /queryables)"
                ))
            })
    }

    fn var(&mut self, atom: &mut Atom, key: &str) -> Result<String, HttpError> {
        if let Some((_, _, var)) = atom.vars.iter().find(|(k, _, _)| k == key) {
            return Ok(var.clone());
        }
        let path = self.path(key)?.to_owned();
        self.next += 1;
        let var = format!("?cql{}", self.next);
        atom.vars.push((key.to_owned(), path, var.clone()));
        Ok(var)
    }

    /// A conjunct `property IN ('a', 'b', …)` (or with `CASEI` on both sides) as a join with
    /// the features whose value, as text, is in a `VALUES` table: read once, not once per
    /// feature as an `EXISTS` with an `IN` of many strings would be. `None` for any other
    /// conjunct.
    ///
    /// When the forms of the property's values are known and are IRIs, blank nodes, strings
    /// or language-tagged strings, the table holds each value in each form, and the features
    /// are looked up by those terms (#497): on the rail KG, 1,000 strings that match nothing took
    /// 5.8 s when the text of every value of the path was compared. Otherwise the text of the
    /// values of the collection's features is compared.
    fn string_in(&mut self, e: &Expr, scope: &mut Scope<'_>) -> Result<Option<String>, HttpError> {
        let Expr::In(x, list) = e else {
            return Ok(None);
        };
        let (key, casei) = match x {
            Scalar::Prop(key) if key != GEOMETRY => (key, false),
            Scalar::Casei(inner) => match inner.as_ref() {
                Scalar::Prop(key) if key != GEOMETRY => (key, true),
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };
        let mut values = Vec::new();
        for item in list {
            let text = match (item, casei) {
                (Scalar::Str(s), false) => s.clone(),
                (Scalar::Casei(inner), true) => match inner.as_ref() {
                    Scalar::Str(s) => s.to_lowercase(),
                    _ => return Ok(None),
                },
                _ => return Ok(None),
            };
            values.push(serde_json::to_string(&text).expect("a string"));
        }
        if values.is_empty() {
            return Ok(None);
        }
        let path = self.path(key)?.to_owned();
        self.next += 1;
        let (v, k) = (format!("?cql{}", self.next), format!("?cqlk{}", self.next));
        if !casei
            && let Some(forms) = (scope.forms)(&path)?
            && let Some(terms) = terms_in(&values, &forms)
        {
            return Ok(Some(format!(
                " {{ SELECT DISTINCT ?f WHERE {{ VALUES {v} {{ {} }} ?f {path} {v} }} }}",
                terms.join(" ")
            )));
        }
        let features = &scope.features;
        // The text of a blank node is an error, which would leave the key unbound and so
        // matching every row of the table.
        let text = if casei {
            format!("LCASE(STR({v}))")
        } else {
            format!("STR({v})")
        };
        Ok(Some(format!(
            " {{ SELECT DISTINCT ?f WHERE {{ {features} ?f {path} {v} . FILTER(!isBLANK({v})) BIND({text} AS {k}) VALUES {k} {{ {} }} }} }}",
            values.join(" ")
        )))
    }

    fn expr(&mut self, e: &Expr) -> Result<String, HttpError> {
        let mut atom = Atom::default();
        let body = match e {
            Expr::And(parts) | Expr::Or(parts) => {
                let joiner = if matches!(e, Expr::And(_)) {
                    " && "
                } else {
                    " || "
                };
                let parts = parts
                    .iter()
                    .map(|p| self.expr(p))
                    .collect::<Result<Vec<_>, _>>()?;
                return Ok(format!("({})", parts.join(joiner)));
            }
            Expr::Not(inner) => return Ok(format!("!({})", self.expr(inner)?)),
            Expr::Bool(b) => return Ok(b.to_string()),
            Expr::IsNull(Scalar::Prop(key)) => {
                if key == GEOMETRY {
                    // Every feature of a collection has a geometry.
                    return Ok("false".to_owned());
                }
                let path = self.path(key)?;
                return Ok(format!("NOT EXISTS {{ ?f {path} ?cqln }}"));
            }
            Expr::IsNull(Scalar::Casei(inner)) => {
                return self.expr(&Expr::IsNull(inner.as_ref().clone()));
            }
            // A literal is never null.
            Expr::IsNull(_) => return Ok("false".to_owned()),
            Expr::Cmp(op, a, b) => {
                let ((a, ka), (b, kb)) = (self.scalar(&mut atom, a)?, self.scalar(&mut atom, b)?);
                let (a, b) = coerce((a, ka), (b, kb));
                // SPARQL writes CQL2's `<>` as `!=`.
                let op = if *op == "<>" { "!=" } else { op };
                format!("{a} {op} {b}")
            }
            Expr::Between(x, low, high) => {
                let x = self.scalar(&mut atom, x)?;
                let (low, high) = (self.scalar(&mut atom, low)?, self.scalar(&mut atom, high)?);
                let (x1, low) = coerce(x.clone(), low);
                let (x2, high) = coerce(x, high);
                format!("({x1} >= {low} && {x2} <= {high})")
            }
            Expr::In(_, list) if list.is_empty() => return Err(bad("IN needs at least one value")),
            Expr::In(x, list) => {
                let x = self.scalar(&mut atom, x)?;
                let mut left = None;
                let mut items = Vec::new();
                for item in list {
                    let (l, r) = coerce(x.clone(), self.scalar(&mut atom, item)?);
                    left.get_or_insert(l);
                    items.push(r);
                }
                format!("{} IN ({})", left.unwrap_or_default(), items.join(", "))
            }
            Expr::Like(x, pattern) => {
                let (pattern, insensitive) = match pattern {
                    Scalar::Str(p) => (p, false),
                    Scalar::Casei(inner) => match inner.as_ref() {
                        Scalar::Str(p) => (p, true),
                        _ => return Err(bad("LIKE needs a string pattern")),
                    },
                    _ => return Err(bad("LIKE needs a string pattern")),
                };
                let (subject, insensitive) = match x {
                    Scalar::Casei(inner) => (self.scalar(&mut atom, inner)?.0, true),
                    other => (self.scalar(&mut atom, other)?.0, insensitive),
                };
                let regex = serde_json::to_string(&like_regex(pattern)).expect("a string");
                let flags = if insensitive { ", \"i\"" } else { "" };
                format!("REGEX(STR({subject}), {regex}{flags})")
            }
            Expr::Spatial(relation, a, b) => {
                let a = self.geometry(&mut atom, a)?;
                let b = self.geometry(&mut atom, b)?;
                format!("<{GEOF}{relation}>({a}, {b})")
            }
            Expr::Temporal(relation, a, b) => {
                // Meeting is ends touching: a day ends where the next begins, at midnight, not
                // at its last millisecond (#497).
                let touching = matches!(*relation, "t_meets" | "t_metby");
                let (a1, a2) = self.span(&mut atom, a, touching)?;
                let (b1, b2) = self.span(&mut atom, b, touching)?;
                temporal(relation, &a1, &a2, &b1, &b2)
            }
        };
        Ok(atom.wrap(body))
    }

    fn scalar(&mut self, atom: &mut Atom, s: &Scalar) -> Result<(String, Kind), HttpError> {
        Ok(match s {
            Scalar::Prop(key) if key == GEOMETRY => {
                return Err(bad(
                    "The geometry can only be compared with the S_* spatial functions",
                ));
            }
            Scalar::Prop(key) => (self.var(atom, key)?, Kind::Prop),
            Scalar::Str(s) => (serde_json::to_string(s).expect("a string"), Kind::Str),
            Scalar::Num(n) => (n.clone(), Kind::Num),
            Scalar::Bool(b) => (b.to_string(), Kind::Bool),
            Scalar::Timestamp(t) => (
                format!(
                    "{}^^<{XSD_DATE_TIME}>",
                    serde_json::to_string(t).expect("a string")
                ),
                Kind::Timestamp,
            ),
            Scalar::Date(d) => (serde_json::to_string(d).expect("a string"), Kind::Date),
            Scalar::Casei(inner) => {
                let (text, _) = self.scalar(atom, inner)?;
                (format!("LCASE(STR({text}))"), Kind::Casei)
            }
        })
    }

    /// The start and end of a temporal operand, as `xsd:dateTime` expressions; with
    /// `touching`, a date ends at the next midnight (see [`Compiler::end`]).
    fn span(
        &mut self,
        atom: &mut Atom,
        t: &Temporal,
        touching: bool,
    ) -> Result<(End, End), HttpError> {
        let last = if touching {
            Last::Midnight
        } else {
            Last::Inside
        };
        Ok(match t {
            Temporal::Instant(b) => (self.end(atom, b, None)?, self.end(atom, b, Some(last))?),
            Temporal::Interval(s, e) => (self.end(atom, s, None)?, self.end(atom, e, Some(last))?),
        })
    }

    /// Where `b` begins (or with `last`, ends): a date covers its day, and a value without a
    /// timezone is read as UTC, as `datetime` reads them. A date ends at its last millisecond,
    /// or with [`Last::Midnight`] at the start of the next day.
    fn end(&mut self, atom: &mut Atom, b: &Bound, last: Option<Last>) -> Result<End, HttpError> {
        let literal = |v: String| format!("\"{v}\"^^<{XSD_DATE_TIME}>");
        let next_day = |e: String| format!("({e} + \"P1D\"^^<{XSD_DAY_TIME_DURATION}>)");
        Ok(match (b, last) {
            (Bound::Open, Some(_)) => End::Late,
            (Bound::Open, None) => End::Early,
            (Bound::Timestamp(t), _) => End::At(literal(t.clone())),
            (Bound::Date(d), Some(Last::Inside)) => End::At(literal(format!("{d}T23:59:59.999Z"))),
            (Bound::Date(d), Some(Last::Midnight)) => {
                End::At(next_day(literal(format!("{d}T00:00:00Z"))))
            }
            (Bound::Date(d), None) => End::At(literal(format!("{d}T00:00:00Z"))),
            (Bound::Prop(key), _) if key == GEOMETRY => {
                return Err(bad("The geometry is not an instant"));
            }
            (Bound::Prop(key), _) => {
                let v = self.var(atom, key)?;
                let dt = XSD_DATE_TIME;
                let utc =
                    |e: String| format!("IF(TZ({e}) = \"\", <{dt}>(CONCAT(STR({e}), \"Z\")), {e})");
                End::At(match last {
                    Some(Last::Inside) => utc(format!(
                        "IF(DATATYPE({v}) = <{XSD_DATE}>, <{dt}>(CONCAT(SUBSTR(STR({v}), 1, 10), \"T23:59:59.999\", SUBSTR(STR({v}), 11))), <{dt}>({v}))"
                    )),
                    Some(Last::Midnight) => format!(
                        "IF(DATATYPE({v}) = <{XSD_DATE}>, {}, {})",
                        next_day(utc(format!(
                            "<{dt}>(CONCAT(SUBSTR(STR({v}), 1, 10), \"T00:00:00\", SUBSTR(STR({v}), 11)))"
                        ))),
                        utc(format!("<{dt}>({v})"))
                    ),
                    None => utc(format!("<{dt}>({v})")),
                })
            }
        })
    }

    fn geometry(&mut self, atom: &mut Atom, g: &Geom) -> Result<String, HttpError> {
        match g {
            Geom::Prop(key) if key == GEOMETRY => Ok("?w".to_owned()),
            Geom::Prop(key) => self.var(atom, key),
            Geom::Bbox(numbers) => {
                let text: Vec<String> = numbers.iter().map(f64::to_string).collect();
                let polygon = bbox(&text.join(","), self.crs)?;
                Ok(format!("\"{polygon}\"^^<{WKT}>"))
            }
            Geom::Wkt(wkt) => {
                // Each vertex has one separator before it, a comma or an opening parenthesis.
                if wkt.matches([',', '(']).count() > MAX_VERTICES {
                    return Err(bad(format!(
                        "A geometry in the filter has more than {MAX_VERTICES} vertices"
                    )));
                }
                let value = match self.crs {
                    OutCrs::Crs84 => wkt.clone(),
                    crs => format!("<{}> {wkt}", crs.iri()),
                };
                let literal = tripletrail::oxigraph::model::Literal::new_typed_literal(
                    value.clone(),
                    tripletrail::oxigraph::model::NamedNode::new_unchecked(WKT),
                );
                if tripletrail::geo::parse_wkt_literal_uncached(&literal.clone().into()).is_none() {
                    return Err(bad(format!("Invalid geometry {wkt} in the filter")));
                }
                Ok(literal.to_string())
            }
        }
    }
}

/// Both sides of a comparison, a property read the way the other side's literal asks: as text
/// against a string, as a date-time against a timestamp, as its first ten characters
/// (`YYYY-MM-DD`) against a date.
fn coerce(a: (String, Kind), b: (String, Kind)) -> (String, String) {
    let read = |(text, kind): (String, Kind), other: Kind| -> String {
        if kind != Kind::Prop {
            return text;
        }
        match other {
            Kind::Str | Kind::Casei => format!("STR({text})"),
            Kind::Timestamp => format!("<{XSD_DATE_TIME}>({text})"),
            Kind::Date => format!("SUBSTR(STR({text}), 1, 10)"),
            Kind::Prop | Kind::Num | Kind::Bool => text,
        }
    };
    let (ka, kb) = (a.1, b.1);
    (read(a, kb), read(b, ka))
}

/// A `LIKE` pattern as an anchored regular expression: `%` any text, `_` one character,
/// `\` the next character as it is.
fn like_regex(pattern: &str) -> String {
    let mut out = String::from("^");
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '%' => out.push_str(".*"),
            '_' => out.push('.'),
            '\\' => {
                if let Some(next) = chars.next() {
                    push_literal(&mut out, next);
                }
            }
            other => push_literal(&mut out, other),
        }
    }
    out.push('$');
    out
}

/// The terms whose text is one of `values` (each a JSON string, which is also a SPARQL
/// string), in each of `forms`; `None` when a form can't be written as terms (a datatype other
/// than `xsd:string`, whose values the store may compare by value) or there are too many.
fn terms_in(values: &[String], forms: &[Form]) -> Option<Vec<String>> {
    if forms.contains(&Form::Other) {
        return None;
    }
    let mut terms = Vec::new();
    for value in values {
        for form in forms {
            match form {
                Form::Iri => {
                    let text: String = serde_json::from_str(value).expect("a JSON string");
                    // Only an absolute IRI can be a value whose text this is.
                    if let Ok(iri) = oxigraph::model::NamedNode::new(text) {
                        terms.push(iri.to_string());
                    }
                }
                Form::Blank | Form::Other => {}
                Form::Str => terms.push(value.clone()),
                Form::Lang(tag) => terms.push(format!("{value}@{tag}")),
            }
            if terms.len() > MAX_IN_TERMS {
                return None;
            }
        }
    }
    Some(terms)
}

fn push_literal(out: &mut String, c: char) {
    if "\\^$.|?*+()[]{}".contains(c) {
        out.push('\\');
    }
    out.push(c);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props() -> Vec<(String, String)> {
        vec![
            ("name".to_owned(), "<http://example.com/name>".to_owned()),
            (
                "tracks".to_owned(),
                "<http://example.com/tracks>".to_owned(),
            ),
        ]
    }

    fn no_forms(_: &str) -> Result<Option<Vec<Form>>, HttpError> {
        Ok(None)
    }

    fn scope<'a>(forms: &'a mut FormsOf<'a>) -> Scope<'a> {
        Scope {
            features: "?f a <http://example.com/C> .".to_owned(),
            forms,
        }
    }

    fn text(f: &str) -> Result<String, HttpError> {
        to_sparql(
            f,
            Lang::Text,
            OutCrs::Crs84,
            &props(),
            &mut scope(&mut no_forms),
        )
    }

    #[test]
    fn text_and_json_read_the_same() {
        let a = text("tracks > 1 AND name LIKE 'S%'").unwrap();
        let b = to_sparql(
            r#"{"op":"and","args":[{"op":">","args":[{"property":"tracks"},1]},
               {"op":"like","args":[{"property":"name"},"S%"]}]}"#,
            Lang::Json,
            OutCrs::Crs84,
            &props(),
            &mut scope(&mut no_forms),
        )
        .unwrap();
        assert_eq!(a, b);
        assert!(a.contains("REGEX(STR(?cql2), \"^S.*$\")"), "{a}");
    }

    #[test]
    fn string_in_matches_terms_when_the_forms_are_known() {
        let mut forms = |_: &str| {
            Ok(Some(vec![
                Form::Str,
                Form::Lang("nb".to_owned()),
                Form::Iri,
                Form::Blank,
            ]))
        };
        let a = to_sparql(
            "name IN ('a\"b', 'http://example.com/x')",
            Lang::Text,
            OutCrs::Crs84,
            &props(),
            &mut scope(&mut forms),
        )
        .unwrap();
        assert_eq!(
            a,
            " { SELECT DISTINCT ?f WHERE { VALUES ?cql1 { \"a\\\"b\" \"a\\\"b\"@nb \
             \"http://example.com/x\" \"http://example.com/x\"@nb <http://example.com/x> } \
             ?f <http://example.com/name> ?cql1 } }"
        );
        // Another datatype, or CASEI: the text of the collection's values.
        let mut other = |_: &str| Ok(Some(vec![Form::Str, Form::Other]));
        let b = to_sparql(
            "name IN ('a')",
            Lang::Text,
            OutCrs::Crs84,
            &props(),
            &mut scope(&mut other),
        )
        .unwrap();
        assert!(
            b.contains(
                "?f a <http://example.com/C> . ?f <http://example.com/name> ?cql1 . FILTER(!isBLANK(?cql1)) BIND(STR(?cql1)"
            ),
            "{b}"
        );
        let c = to_sparql(
            "CASEI(name) IN (CASEI('A'))",
            Lang::Text,
            OutCrs::Crs84,
            &props(),
            &mut scope(&mut forms),
        )
        .unwrap();
        assert!(c.contains("LCASE(STR(?cql1))"), "{c}");
    }

    #[test]
    fn predicates() {
        assert!(
            text("name = 'a''b'")
                .unwrap()
                .contains("STR(?cql1) = \"a'b\"")
        );
        assert!(text("tracks BETWEEN 1 AND 2").unwrap().contains(">= 1 &&"));
        assert!(text("name NOT IN ('a', 'b')").unwrap().contains("!("));
        assert!(text("name IS NULL").unwrap().contains("NOT EXISTS"));
        assert!(
            text("name IN ('a', 'b')")
                .unwrap()
                .contains("VALUES ?cqlk1 { \"a\" \"b\" }")
        );
        assert!(text("NOT name IN ('a')").unwrap().contains("FILTER(!("));
        assert!(
            text("CASEI(name) = CASEI('AB')")
                .unwrap()
                .contains("LCASE(STR(?cql1)) = LCASE(STR(\"AB\"))")
        );
        assert!(
            text("S_INTERSECTS(geometry, BBOX(0, 0, 1, 1))")
                .unwrap()
                .contains("sfIntersects>(?w, \"POLYGON")
        );
        assert!(
            text("s_within(geometry, POINT (1 2))")
                .unwrap()
                .contains("POINT (1 2)")
        );
        assert!(text("updated > TIMESTAMP('2020-01-01T00:00:00Z')").is_err());
        assert!(text("name = ").is_err());
        assert!(text("A_CONTAINS(name, ('a'))").is_err());
        assert!(text("name = 'a' OR (tracks < 3 AND NOT tracks = 1)").is_ok());
        // Bounds: deep nesting and many predicates are refused, not a stack overflow.
        let nested = |n: usize| format!("{}TRUE{}", "(".repeat(n), ")".repeat(n));
        assert!(text(&nested(50)).is_ok());
        assert!(text(&nested(5000)).unwrap_err().1.contains("nests deeper"));
        assert!(
            text(&format!("{}TRUE", "NOT ".repeat(1000)))
                .unwrap_err()
                .1
                .contains("nests deeper")
        );
        assert!(
            text(&vec!["TRUE"; 5000].join(" AND "))
                .unwrap_err()
                .1
                .contains("predicates")
        );
        assert!(text(&vec!["tracks = 1"; 150].join(" OR ")).is_ok());
        assert!(text(&"x".repeat(70_000)).unwrap_err().1.contains("longer"));
        let many: Vec<String> = (0..1200).map(|i| i.to_string()).collect();
        assert!(
            text(&format!("tracks IN ({})", many.join(", ")))
                .unwrap_err()
                .1
                .contains("IN lists")
        );
        let line: Vec<String> = (0..1200).map(|i| format!("{i} {i}")).collect();
        let wkt = format!("S_INTERSECTS(geometry, LINESTRING({}))", line.join(", "));
        assert!(text(&wkt).unwrap_err().1.contains("vertices"));
        let json = |f: &str| {
            to_sparql(
                f,
                Lang::Json,
                OutCrs::Crs84,
                &props(),
                &mut scope(&mut no_forms),
            )
        };
        assert!(
            json(r#"{"op":"in","args":[{"property":"name"},[]]}"#)
                .unwrap_err()
                .1
                .contains("IN needs")
        );
        let mut deep = String::from("true");
        for _ in 0..1000 {
            deep = format!(r#"{{"op":"not","args":[{deep}]}}"#);
        }
        assert!(json(&deep).is_err());
        assert_eq!(like_regex("a\\%b_c.d"), "^a%b.c\\.d$");
    }
}
