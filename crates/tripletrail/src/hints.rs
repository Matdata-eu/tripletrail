//! Query hints (#238): `#pragma tt.…` comment lines in a query or update that switch off a
//! planner step or a rewrite for that request, as Stardog's `#pragma` lines do. They are
//! comments, so other engines ignore them, and they need no change to the parser.
//!
//! - `#pragma tt.optimizer off` (or `tt.join-order fixed`): the patterns are joined in the
//!   order they are written: the optimizer and TripleTrail's planning of `EXISTS` patterns are
//!   left out.
//! - `#pragma tt.spatial-rewrite off`: GeoSPARQL filters and joins are evaluated as written,
//!   without the spatial index.
//! - `#pragma tt.range-rewrite off`: numeric range `FILTER`s without the range index.
//! - `#pragma tt.id-rewrites off`: counts and rows without the shortcuts on term ids.
//! - `#pragma tt.result-cache off`: the result cache neither answers nor keeps the query.
//! - `#pragma tt.executor ids` (or `spareval`): which executor runs the query (#11), over the
//!   server's `--executor`. `ids` answers a whole query from tables of term ids when they
//!   cover it, and leaves the rest to the evaluator.
//! - `#pragma tt.timeout 30s` (`500ms`, `2m` or seconds): the request's timeout, capped by
//!   the server's, like `?timeout=`.
//!
//! An unknown `tt.` hint is an error, so a typo does not go unnoticed; other `#pragma` lines
//! (another engine's) are ignored.
//!
//! Blazegraph's query hints, triples in `hint:` (`http://www.bigdata.com/queryHints#`) as
//! Wikidata queries write them, are taken out of the query before it runs, so they never
//! match data. `hint:optimizer "None"` (on `hint:Query`, `hint:Group`, `hint:SubQuery` or
//! `hint:Prior`) is `tt.optimizer off`; the others (`hint:runFirst`, `hint:analytic`, …) are
//! ignored.
//!
//! Virtuoso's `DEFINE name value` clauses before the prologue are taken out too:
//! `DEFINE sql:select-option "order"` is `tt.optimizer off`. An `input:` one (inference,
//! default graphs) would change the answers, so it is refused; the others are ignored. The hints apply to the thread that prepares the request:
//! the engine reads them while it rewrites and plans the query, before the results are read.

use spargebra::algebra::{AggregateExpression, Expression, GraphPattern, OrderExpression};
use spargebra::term::{NamedNodePattern, TermPattern};
use spargebra::{GraphUpdateOperation, Query, Update};
use std::cell::Cell;
use std::fmt;
use std::time::Duration;

/// Blazegraph's query hints namespace.
pub const BLAZEGRAPH: &str = "http://www.bigdata.com/queryHints#";

/// The hints of one request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hints {
    /// Join the patterns in the written order.
    pub optimizer_off: bool,
    pub spatial_rewrite_off: bool,
    pub range_rewrite_off: bool,
    pub id_rewrites_off: bool,
    pub result_cache_off: bool,
    pub timeout: Option<Duration>,
    /// `tt.executor`: `Some(true)` for `ids`, `Some(false)` for `spareval`.
    pub executor_ids: Option<bool>,
}

impl Hints {
    /// The hints of a query or update text, from its `#pragma tt.…` lines.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut hints = Self::default();
        let defined;
        let text = match without_defines(text)? {
            Some((rest, found)) => {
                hints.optimizer_off = found.optimizer_off;
                defined = rest;
                defined.as_str()
            }
            None => text,
        };
        if text.contains(BLAZEGRAPH) {
            // Parsed only for its `hint:` triples; a text that does not parse is refused later.
            let mut found = Self::default();
            if let Ok(mut q) = crate::Engine::parser(true).parse_query(text) {
                found = strip_query(&mut q);
            } else if let Ok(mut u) = crate::Engine::parser(true).parse_update(text) {
                found = strip_update(&mut u);
            }
            hints.optimizer_off |= found.optimizer_off;
        }
        if !text.contains("#pragma") {
            return Ok(hints);
        }
        for comment in comments(text) {
            let Some(rest) = comment.strip_prefix("#pragma") else {
                continue;
            };
            let mut words = rest.split_whitespace();
            let Some(name) = words.next() else { continue };
            let Some(name) = name.strip_prefix("tt.") else {
                continue;
            };
            let value = words.next().unwrap_or("");
            if words.next().is_some() {
                return Err(format!("#pragma tt.{name}: one value expected"));
            }
            let off = |what: &str| match value {
                "off" => Ok(true),
                "on" => Ok(false),
                _ => Err(format!(
                    "#pragma tt.{what}: expected `off` or `on`, not `{value}`"
                )),
            };
            match name {
                "optimizer" => hints.optimizer_off = off(name)?,
                "join-order" => {
                    hints.optimizer_off = match value {
                        "fixed" => true,
                        "auto" => false,
                        _ => {
                            return Err(format!(
                                "#pragma tt.join-order: expected `fixed` or `auto`, not `{value}`"
                            ));
                        }
                    }
                }
                "spatial-rewrite" => hints.spatial_rewrite_off = off(name)?,
                "range-rewrite" => hints.range_rewrite_off = off(name)?,
                "id-rewrites" => hints.id_rewrites_off = off(name)?,
                "result-cache" => hints.result_cache_off = off(name)?,
                "executor" => {
                    hints.executor_ids = Some(match value {
                        "ids" => true,
                        "spareval" => false,
                        _ => {
                            return Err(format!(
                                "#pragma tt.executor: expected `ids` or `spareval`, not `{value}`"
                            ));
                        }
                    })
                }
                "timeout" => hints.timeout = Some(duration(value)?),
                _ => {
                    return Err(format!(
                        "Unknown query hint #pragma tt.{name} (known: optimizer, join-order, \
                         spatial-rewrite, range-rewrite, id-rewrites, result-cache, executor, \
                         timeout)"
                    ));
                }
            }
        }
        Ok(hints)
    }

    /// Whether the request leaves the result cache alone: a hint that changes the work would
    /// otherwise be answered from (or fill the cache for) the run without it.
    pub fn skips_cache(&self) -> bool {
        self.result_cache_off
            || self.optimizer_off
            || self.spatial_rewrite_off
            || self.range_rewrite_off
            || self.id_rewrites_off
            || self.executor_ids.is_some()
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The hints in force on this thread (see [`scope`]).
    pub fn current() -> Self {
        CURRENT.with(Cell::get)
    }
}

/// The hints as `#pragma` lines, as `/explain` shows them.
impl fmt::Display for Hints {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut lines = Vec::new();
        if self.optimizer_off {
            lines.push("tt.optimizer off".to_owned());
        }
        if self.spatial_rewrite_off {
            lines.push("tt.spatial-rewrite off".to_owned());
        }
        if self.range_rewrite_off {
            lines.push("tt.range-rewrite off".to_owned());
        }
        if self.id_rewrites_off {
            lines.push("tt.id-rewrites off".to_owned());
        }
        if self.result_cache_off {
            lines.push("tt.result-cache off".to_owned());
        }
        if let Some(t) = self.timeout {
            lines.push(format!("tt.timeout {}s", t.as_secs_f64()));
        }
        if let Some(ids) = self.executor_ids {
            lines.push(format!(
                "tt.executor {}",
                if ids { "ids" } else { "spareval" }
            ));
        }
        f.write_str(&lines.join("\n"))
    }
}

/// The comments of a SPARQL text: from a `#` outside a string literal or an IRI to the end
/// of the line, so a `#pragma` line inside a `"""` literal is data, not a hint.
fn comments(text: &str) -> Vec<&str> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'#' => {
                let end = text[i..].find(['\n', '\r']).map_or(b.len(), |n| i + n);
                out.push(&text[i..end]);
                i = end;
            }
            q @ (b'"' | b'\'') => {
                let long = b[i..].starts_with(&[q, q, q]);
                i += if long { 3 } else { 1 };
                while i < b.len() {
                    if b[i] == b'\\' {
                        i += 2;
                    } else if long && b[i..].starts_with(&[q, q, q]) {
                        // A long literal may end in one or two more quotes: `"""a""""`.
                        i += 3;
                        while i < b.len() && b[i] == q {
                            i += 1;
                        }
                        break;
                    } else if !long && (b[i] == q || b[i] == b'\n') {
                        i += 1;
                        break;
                    } else {
                        i += 1;
                    }
                }
            }
            b'<' => {
                // An IRIREF, or the `<` operator when no IRI characters lead to a `>`.
                let iri = b[i + 1..]
                    .iter()
                    .position(|&c| c <= b' ' || b"<\"{}|^`\\>".contains(&c))
                    .filter(|&n| b[i + 1 + n] == b'>');
                i += iri.map_or(1, |n| n + 2);
            }
            _ => i += 1,
        }
    }
    out
}

fn duration(value: &str) -> Result<Duration, String> {
    let (number, scale) = if let Some(n) = value.strip_suffix("ms") {
        (n, 0.001)
    } else if let Some(n) = value.strip_suffix('s') {
        (n, 1.0)
    } else if let Some(n) = value.strip_suffix('m') {
        (n, 60.0)
    } else {
        (value, 1.0)
    };
    number
        .parse::<f64>()
        .ok()
        .and_then(|n| Duration::try_from_secs_f64(n * scale).ok())
        .ok_or_else(|| format!("#pragma tt.timeout: invalid duration `{value}`"))
}

/// Virtuoso's `DEFINE name value` clauses at the start of `text` blanked out (so positions in
/// error messages stay right), with the hints they set; `None` without any.
pub fn without_defines(text: &str) -> Result<Option<(String, Hints)>, String> {
    let b = text.as_bytes();
    let mut i = 0;
    let mut spans = Vec::new();
    let mut hints = Hints::default();
    loop {
        // Whitespace and comments.
        while i < b.len() {
            if b[i].is_ascii_whitespace() {
                i += 1;
            } else if b[i] == b'#' {
                i = text[i..].find('\n').map_or(b.len(), |n| i + n);
            } else {
                break;
            }
        }
        if !b.get(i..i + 7).is_some_and(|w| {
            w.eq_ignore_ascii_case(b"DEFINE ") || w.eq_ignore_ascii_case(b"DEFINE\t")
        }) {
            break;
        }
        let start = i;
        i += 7;
        let mut word = || -> Result<String, String> {
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            let from = i;
            match b.get(i) {
                Some(&q @ (b'"' | b'\'')) => {
                    i += 1;
                    let mut value = String::new();
                    loop {
                        // Up to the next quote or backslash: both ASCII, so char boundaries.
                        let start = i;
                        while i < b.len() && b[i] != q && b[i] != b'\\' {
                            i += 1;
                        }
                        value.push_str(&text[start..i]);
                        match b.get(i) {
                            None => return Err("DEFINE: unterminated value".into()),
                            Some(&c) if c == q => {
                                i += 1;
                                return Ok(value);
                            }
                            // A backslash escape: the next character as it is.
                            Some(_) => {
                                let Some(c) = text[i + 1..].chars().next() else {
                                    return Err("DEFINE: unterminated value".into());
                                };
                                value.push(c);
                                i += 1 + c.len_utf8();
                            }
                        }
                    }
                }
                Some(b'<') => {
                    while i < b.len() && b[i] != b'>' {
                        i += 1;
                    }
                    if i == b.len() {
                        return Err("DEFINE: unterminated IRI".into());
                    }
                    i += 1;
                    Ok(text[from..i].to_owned())
                }
                _ => {
                    while i < b.len() && !b[i].is_ascii_whitespace() {
                        i += 1;
                    }
                    Ok(text[from..i].to_owned())
                }
            }
        };
        let name = word()?;
        let value = word()?;
        if name.is_empty() || value.is_empty() {
            return Err("DEFINE: a name and a value expected".into());
        }
        if name.starts_with("input:") {
            return Err(format!(
                "DEFINE {name} is not supported: it would change the answers"
            ));
        }
        if name == "sql:select-option" && value.split(',').any(|o| o.trim() == "order") {
            hints.optimizer_off = true;
        }
        spans.push((start, i));
    }
    if spans.is_empty() {
        return Ok(None);
    }
    let mut out = text.to_owned();
    for (from, to) in spans {
        out.replace_range(from..to, &" ".repeat(to - from));
    }
    Ok(Some((out, hints)))
}

/// Takes Blazegraph's `hint:` triples out of a query, and gives the hints they set.
pub(crate) fn strip_query(query: &mut Query) -> Hints {
    let mut hints = Hints::default();
    match query {
        Query::Select { pattern, .. }
        | Query::Construct { pattern, .. }
        | Query::Describe { pattern, .. }
        | Query::Ask { pattern, .. } => strip(pattern, &mut hints),
    }
    hints
}

/// [`strip_query`] for the patterns of an update.
pub(crate) fn strip_update(update: &mut Update) -> Hints {
    let mut hints = Hints::default();
    for op in &mut update.operations {
        if let GraphUpdateOperation::DeleteInsert { pattern, .. } = op {
            strip(pattern, &mut hints);
        }
    }
    hints
}

fn strip(p: &mut GraphPattern, hints: &mut Hints) {
    use GraphPattern as G;
    match p {
        G::Bgp { patterns } => patterns.retain(|t| {
            fn hint(name: &str) -> Option<&str> {
                name.strip_prefix(BLAZEGRAPH)
            }
            let (TermPattern::NamedNode(scope), NamedNodePattern::NamedNode(name)) =
                (&t.subject, &t.predicate)
            else {
                return true;
            };
            let (Some(_), Some(name)) = (hint(scope.as_str()), hint(name.as_str())) else {
                return true;
            };
            if name == "optimizer"
                && matches!(&t.object, TermPattern::Literal(l) if l.value() == "None")
            {
                hints.optimizer_off = true;
            }
            false
        }),
        G::LeftJoin {
            left,
            right,
            expression,
        } => {
            strip(left, hints);
            strip(right, hints);
            if let Some(e) = expression {
                strip_expression(e, hints);
            }
        }
        G::Join { left, right }
        | G::Lateral { left, right }
        | G::Union { left, right }
        | G::Minus { left, right } => {
            strip(left, hints);
            strip(right, hints);
        }
        G::Filter { expr, inner } => {
            strip_expression(expr, hints);
            strip(inner, hints);
        }
        G::Extend {
            inner, expression, ..
        } => {
            strip_expression(expression, hints);
            strip(inner, hints);
        }
        G::OrderBy { inner, expression } => {
            for o in expression {
                match o {
                    OrderExpression::Asc(e) | OrderExpression::Desc(e) => {
                        strip_expression(e, hints)
                    }
                }
            }
            strip(inner, hints);
        }
        G::Group {
            inner, aggregates, ..
        } => {
            for (_, a) in aggregates {
                if let AggregateExpression::FunctionCall { expr, .. } = a {
                    strip_expression(expr, hints);
                }
            }
            strip(inner, hints);
        }
        G::Graph { inner, .. }
        | G::Project { inner, .. }
        | G::Distinct { inner }
        | G::Reduced { inner }
        | G::Slice { inner, .. } => strip(inner, hints),
        // Another endpoint's query: it may well be a Blazegraph that reads its hints.
        G::Service { .. } | G::Path { .. } | G::Values { .. } => {}
    }
}

/// [`strip`] in the `EXISTS` patterns of an expression.
fn strip_expression(e: &mut Expression, hints: &mut Hints) {
    use Expression as E;
    match e {
        E::Exists(p) => strip(p, hints),
        E::Or(a, b)
        | E::And(a, b)
        | E::Equal(a, b)
        | E::SameTerm(a, b)
        | E::Greater(a, b)
        | E::GreaterOrEqual(a, b)
        | E::Less(a, b)
        | E::LessOrEqual(a, b)
        | E::Add(a, b)
        | E::Subtract(a, b)
        | E::Multiply(a, b)
        | E::Divide(a, b) => {
            strip_expression(a, hints);
            strip_expression(b, hints);
        }
        E::In(a, list) => {
            strip_expression(a, hints);
            for x in list {
                strip_expression(x, hints);
            }
        }
        E::UnaryPlus(a) | E::UnaryMinus(a) | E::Not(a) => strip_expression(a, hints),
        E::If(a, b, c) => {
            strip_expression(a, hints);
            strip_expression(b, hints);
            strip_expression(c, hints);
        }
        E::Coalesce(list) | E::FunctionCall(_, list) => {
            for x in list {
                strip_expression(x, hints);
            }
        }
        E::NamedNode(_) | E::Literal(_) | E::Variable(_) | E::Bound(_) => {}
    }
}

thread_local! {
    static CURRENT: Cell<Hints> = Cell::new(Hints::default());
}

/// Runs `f` with `hints` in force on this thread, then puts back the ones before.
pub(crate) fn scope<R>(hints: Hints, f: impl FnOnce() -> R) -> R {
    struct Restore(Hints);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT.with(|c| c.set(self.0));
        }
    }
    let _restore = Restore(CURRENT.with(|c| c.replace(hints)));
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pragmas_are_read_from_comment_lines() {
        let h = Hints::parse(
            "#pragma tt.optimizer off\n  #pragma tt.timeout 1.5s\n#pragma join.order.fixed\nSELECT * { ?s ?p ?o }",
        )
        .unwrap();
        assert!(h.optimizer_off);
        assert_eq!(h.timeout, Some(Duration::from_millis(1500)));
        assert!(!h.spatial_rewrite_off);
        assert_eq!(
            Hints::parse("#pragma tt.timeout 250ms").unwrap().timeout,
            Some(Duration::from_millis(250))
        );
        assert_eq!(
            Hints::parse("#pragma tt.timeout 2m").unwrap().timeout,
            Some(Duration::from_secs(120))
        );
        assert!(
            Hints::parse("#pragma tt.join-order fixed")
                .unwrap()
                .optimizer_off
        );
        assert!(Hints::parse("SELECT * { ?s ?p ?o }").unwrap().is_empty());
        assert!(Hints::parse("#pragma tt.nope off").is_err());
        assert!(Hints::parse("#pragma tt.optimizer maybe").is_err());
        assert!(Hints::parse("#pragma tt.timeout soon").is_err());
    }

    #[test]
    fn pragmas_in_literals_and_iris_are_data() {
        let text = "PREFIX ex: <http://ex.org/#pragma>\nINSERT DATA { ex:d ex:t \"\"\"one\n#pragma tt.nope off\nthree\"\"\" , '''a\n#pragma tt.optimizer off\n''' , \"#pragma tt.x\" , <http://ex.org/a#pragma> . }\n# FILTER(?a < 3)\n  #pragma tt.timeout 2s";
        let h = Hints::parse(text).unwrap();
        assert_eq!(h.timeout, Some(Duration::from_secs(2)), "{h:?}");
        assert!(!h.optimizer_off);
        assert_eq!(
            comments("SELECT * { FILTER(?a < 3 && ?b > 1) } # x\n#pragma tt.optimizer off"),
            ["# x", "#pragma tt.optimizer off"]
        );
        assert!(
            Hints::parse("SELECT * { BIND(\"\"\"q\"\"\"\" AS ?x) }\n#pragma tt.optimizer off")
                .unwrap()
                .optimizer_off
        );
    }

    #[test]
    fn virtuoso_defines_are_blanked_out() {
        let (rest, h) = without_defines(
            "# c\nDEFINE sql:select-option \"order\"\ndefine sql:log-enable 2\nSELECT * { ?s ?p ?o }",
        )
        .unwrap()
        .unwrap();
        assert!(h.optimizer_off);
        assert_eq!(
            rest.trim_start().trim_start_matches("# c").trim_start(),
            "SELECT * { ?s ?p ?o }"
        );
        assert_eq!(rest.len(), "# c\nDEFINE sql:select-option \"order\"\ndefine sql:log-enable 2\nSELECT * { ?s ?p ?o }".len());
        assert!(without_defines("SELECT * { ?s ?p ?o }").unwrap().is_none());
        assert!(
            without_defines("PREFIX define: <x:>\nSELECT * {}")
                .unwrap()
                .is_none()
        );
        assert!(without_defines("DEFINE input:inference \"r\" SELECT * {}").is_err());
        assert!(
            Hints::parse("DEFINE sql:select-option \"order\" SELECT * {}")
                .unwrap()
                .optimizer_off
        );
        // An escaped quote is part of the value.
        let (rest, _) = without_defines("DEFINE a \"x\\\"y\" SELECT * {}")
            .unwrap()
            .unwrap();
        assert_eq!(rest.trim(), "SELECT * {}");
        // Odd endings are errors, never a panic.
        for text in [
            "DEFINE a \"é",
            "DEFINE a 'é\\",
            "DEFINE a \"\\",
            "DEFINE a <é",
            "DEFINE a",
            "DEFINE é",
            "DEFINE \"é\" é",
            "define\tx 'y",
        ] {
            let _ = without_defines(text);
            let _ = Hints::parse(text);
        }
        let all = "DEFINE a \"é\\\"ü' <x> 'ö\" SELECT";
        for end in (0..=all.len()).filter(|&n| all.is_char_boundary(n)) {
            let _ = without_defines(&all[..end]);
        }
        assert!(without_defines("DEFINE a \"é").is_err());
    }

    #[test]
    fn a_scope_puts_the_hints_before_back() {
        let h = Hints {
            range_rewrite_off: true,
            ..Hints::default()
        };
        scope(h, || {
            assert_eq!(Hints::current(), h);
            scope(Hints::default(), || assert!(Hints::current().is_empty()));
            assert_eq!(Hints::current(), h);
        });
        assert!(Hints::current().is_empty());
    }
}
