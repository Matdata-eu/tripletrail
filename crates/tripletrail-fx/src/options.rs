//! The options of a Facade-X SERVICE call, and the rewrite of its pattern.
//!
//! Options come from the SERVICE IRI (`x-sparql-anything:location=data.csv,csv.headers=true`)
//! and from option triples inside the SERVICE pattern
//! (`fxe:properties fxe:location "data.csv"`). An `fxe:` triple wins over a deprecated `fx:`
//! triple, which wins over the IRI.

use crate::{FX, FXE, FxError, RDF};
use oxigraph::model::{Literal, Variable};
use spargebra::algebra::{Expression, Function, GraphPattern, PropertyPathExpression};
use spargebra::term::{NamedNodePattern, TermPattern};
use std::collections::{HashMap, HashSet};

/// The IRI scheme of a Facade-X SERVICE.
pub const SCHEME: &str = "x-sparql-anything:";

/// Prefix of the variables the rewrite adds; they are dropped from the SERVICE results.
pub(crate) const HIDDEN: &str = "__fx_";

/// Triplification options by name (`location`, `csv.headers`, ...).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options(pub HashMap<String, String>);

impl Options {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// A boolean option: `true`/`false` (any case), or `default` when absent.
    pub fn flag(&self, name: &str, default: bool) -> Result<bool, FxError> {
        match self.get(name) {
            None => Ok(default),
            Some(v) if v.eq_ignore_ascii_case("true") => Ok(true),
            Some(v) if v.eq_ignore_ascii_case("false") => Ok(false),
            Some(v) => Err(FxError::Option(format!(
                "{name} must be true or false, not {v:?}"
            ))),
        }
    }

    /// Options from a SERVICE IRI: `x-sparql-anything:` then `name=value` pairs separated by
    /// commas (`\,` and `\=` escape them), or a bare location.
    pub fn from_iri(iri: &str) -> Option<Self> {
        let rest = iri.strip_prefix(SCHEME)?;
        let mut options = HashMap::new();
        for (i, part) in split_unescaped(rest, ',').into_iter().enumerate() {
            if part.is_empty() {
                continue;
            }
            match find_unescaped(&part, '=') {
                Some(eq) => {
                    options.insert(unescape(&part[..eq]), unescape(&part[eq + 1..]));
                }
                // `x-sparql-anything:data.json` is a location only.
                None if i == 0 => {
                    options.insert("location".to_owned(), unescape(&part));
                }
                None => {
                    options.insert(unescape(&part), String::new());
                }
            }
        }
        Some(Self(options))
    }
}

/// `s` split at each `sep` not escaped by a backslash; escapes are kept.
fn split_unescaped(s: &str, sep: char) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == sep {
            parts.push(String::new());
            continue;
        }
        let last = parts.last_mut().unwrap();
        last.push(c);
        if c == '\\' {
            last.extend(chars.next());
        }
    }
    parts
}

fn find_unescaped(s: &str, c: char) -> Option<usize> {
    let mut escaped = false;
    for (i, ch) in s.char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == c {
            return Some(i);
        }
    }
    None
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && matches!(chars.peek(), Some(',' | '=')) {
            out.push(chars.next().unwrap());
        } else {
            out.push(c);
        }
    }
    out
}

/// Which triples of the generated graph the pattern can match, to build only those.
#[derive(Debug, Clone, Default)]
pub(crate) struct Needs {
    /// Some pattern has a variable or negated predicate: every triple.
    pub all: bool,
    /// Container membership properties (`rdf:_1`, ...) through `fx:anySlot`.
    pub slots: bool,
    /// The constant predicates.
    pub predicates: HashSet<String>,
    /// The pattern uses `GRAPH`: also put the triples in the named graph.
    pub named_graph: bool,
    /// Conditions every row an answer uses meets (a database source passes them on).
    pub conditions: Vec<crate::database::Condition>,
    /// A bind join's values, for a database source (see [`crate::database::one_of`]).
    pub one_of: Vec<crate::database::OneOf>,
    /// Most rows a database source need read (see [`crate::database::Select::limit`]).
    pub limit: Option<usize>,
    /// Rows the database source gave, set when it is read.
    pub rows_read: std::cell::Cell<usize>,
}

impl Needs {
    pub(crate) fn keep(&self, predicate: &str) -> bool {
        self.all || self.predicates.contains(predicate) || (self.slots && is_slot(predicate))
    }
}

/// Whether `iri` is a container membership property, `rdf:_1`, `rdf:_2`, ...
pub(crate) fn is_slot(iri: &str) -> bool {
    slot_index(iri).is_some()
}

/// `n` for `rdf:_n` (n ≥ 1).
pub(crate) fn slot_index(iri: &str) -> Option<u64> {
    let digits = iri.strip_prefix(RDF)?.strip_prefix('_')?;
    if digits.is_empty() || digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// A SERVICE call taken apart: its options, the pattern to evaluate on the generated graph,
/// and what that pattern needs.
pub(crate) struct Call {
    pub options: Options,
    pub pattern: GraphPattern,
    pub needs: Needs,
}

/// Takes the option triples out of `pattern`, rewrites `fx:anySlot` and works out which
/// triples the rest can match. `iri` is the SERVICE IRI.
pub(crate) fn prepare(iri: &str, pattern: &GraphPattern) -> Result<Call, FxError> {
    let mut options = Options::from_iri(iri)
        .ok_or_else(|| FxError::Option(format!("not a Facade-X SERVICE: <{iri}>")))?;
    // Options the query rewrite put in the IRI ([`crate::correlate`]).
    let encoded: Vec<String> = options
        .0
        .keys()
        .filter(|k| k.starts_with(crate::correlate::VALUE))
        .cloned()
        .collect();
    for key in encoded {
        let value = options.0.remove(&key).unwrap_or_default();
        options.0.insert(
            key[crate::correlate::VALUE.len()..].to_owned(),
            crate::sources::percent_decode(&value),
        );
    }
    let mut pattern = pattern.clone();
    legacy_root(&mut pattern);
    // Values of variables bound to a constant by a BIND inside the SERVICE, for option
    // triples like `BIND("data.json" AS ?f) fxe:properties fxe:location ?f`.
    let mut binds = HashMap::new();
    visit(&pattern, &mut |p| {
        if let GraphPattern::Extend {
            variable,
            expression,
            ..
        } = p
        {
            let value = match expression {
                Expression::Literal(l) => Some(l.value().to_owned()),
                Expression::NamedNode(n) => Some(n.as_str().to_owned()),
                _ => None,
            };
            if let Some(value) = value {
                binds.insert(variable.clone(), value);
            }
        }
    });
    let mut fxe_options = HashMap::new();
    let mut fx_options = HashMap::new();
    let mut error = None;
    let mut slot = 0;
    visit_mut(&mut pattern, &mut |p| {
        let GraphPattern::Bgp { patterns } = p else {
            return;
        };
        let mut slots = Vec::new();
        patterns.retain_mut(|t| {
            let options = match &t.subject {
                TermPattern::NamedNode(s)
                    if s.as_str() == "http://sparql.xyz/facade-x/engine/properties" =>
                {
                    Some((&mut fxe_options, FXE))
                }
                TermPattern::NamedNode(s)
                    if s.as_str() == "http://sparql.xyz/facade-x/ns/properties" =>
                {
                    Some((&mut fx_options, FX))
                }
                _ => None,
            };
            if let Some((options, ns)) = options {
                let name = match &t.predicate {
                    NamedNodePattern::NamedNode(n) => n.as_str().strip_prefix(ns),
                    NamedNodePattern::Variable(_) => None,
                };
                let value = match &t.object {
                    TermPattern::Literal(l) => Some(l.value().to_owned()),
                    TermPattern::NamedNode(n) => Some(n.as_str().to_owned()),
                    TermPattern::Variable(v) => binds.get(v).cloned(),
                    _ => None,
                };
                match (name, value) {
                    (Some(name), Some(value)) => {
                        options.insert(name.to_owned(), value);
                    }
                    _ if error.is_none() => {
                        error = Some(FxError::Option(format!(
                            "an option triple needs an option name in {ns} and a literal or \
                             a variable bound by a BIND inside the SERVICE: {t}"
                        )));
                    }
                    _ => {}
                }
                return false;
            }
            if let NamedNodePattern::NamedNode(n) = &t.predicate
                && n.as_str() == "http://sparql.xyz/facade-x/ns/anySlot"
            {
                let v = Variable::new_unchecked(format!("{HIDDEN}slot{slot}"));
                slot += 1;
                t.predicate = NamedNodePattern::Variable(v.clone());
                slots.push(v);
            }
            true
        });
        if !slots.is_empty() {
            let expr = slots
                .into_iter()
                .map(|v| {
                    Expression::FunctionCall(
                        Function::StrStarts,
                        vec![
                            Expression::FunctionCall(Function::Str, vec![Expression::Variable(v)]),
                            Expression::Literal(Literal::new_simple_literal(format!("{RDF}_"))),
                        ],
                    )
                })
                .reduce(|a, b| Expression::And(Box::new(a), Box::new(b)))
                .unwrap();
            let inner = std::mem::replace(p, GraphPattern::Bgp { patterns: vec![] });
            *p = GraphPattern::Filter {
                expr,
                inner: Box::new(inner),
            };
        }
    });
    if let Some(e) = error {
        return Err(e);
    }
    options.0.extend(fx_options);
    options.0.extend(fxe_options);
    let mut needs = Needs::default();
    visit(&pattern, &mut |p| match p {
        GraphPattern::Bgp { patterns } => {
            for t in patterns {
                match &t.predicate {
                    NamedNodePattern::NamedNode(n) => {
                        needs.predicates.insert(n.as_str().to_owned());
                    }
                    NamedNodePattern::Variable(v) if v.as_str().starts_with(HIDDEN) => {
                        needs.slots = true;
                    }
                    NamedNodePattern::Variable(_) => needs.all = true,
                }
                if matches!(t.subject, TermPattern::Triple(_))
                    || matches!(t.object, TermPattern::Triple(_))
                {
                    needs.all = true;
                }
            }
        }
        GraphPattern::Path { path, .. } => path_needs(path, &mut needs),
        GraphPattern::Graph { .. } => needs.named_graph = true,
        GraphPattern::Filter { expr, .. } => expr_needs(expr, &mut needs),
        GraphPattern::LeftJoin {
            expression: Some(expr),
            ..
        } => expr_needs(expr, &mut needs),
        GraphPattern::Extend { expression, .. } => expr_needs(expression, &mut needs),
        GraphPattern::OrderBy { expression, .. } => {
            for e in expression {
                let (spargebra::algebra::OrderExpression::Asc(e)
                | spargebra::algebra::OrderExpression::Desc(e)) = e;
                expr_needs(e, &mut needs);
            }
        }
        GraphPattern::Group { aggregates, .. } => {
            for (_, a) in aggregates {
                if a.to_string().contains("EXISTS") {
                    needs.all = true;
                }
            }
        }
        _ => {}
    });
    if !needs.all && !needs.slots {
        needs.conditions = crate::database::conditions(&pattern);
        needs.one_of = crate::database::one_of(&pattern);
    }
    Ok(Call {
        options,
        pattern,
        needs,
    })
}

/// `fx:root`, the type of the root before SPARQL Anything 1.0, is `fx:Root` (SPARQL Anything
/// rewrites it the same way): as the object of a triple that is not an option, and as a
/// constant in an expression.
fn legacy_root(pattern: &mut GraphPattern) {
    use spargebra::term::NamedNode;
    let legacy = format!("{FX}root");
    let root = NamedNode::new_unchecked(format!("{FX}Root"));
    let is_option = |s: &TermPattern| {
        matches!(s, TermPattern::NamedNode(n)
            if n.as_str() == format!("{FX}properties") || n.as_str() == format!("{FXE}properties"))
    };
    visit_mut(pattern, &mut |p| match p {
        GraphPattern::Bgp { patterns } => {
            for t in patterns {
                if !is_option(&t.subject)
                    && matches!(&t.object, TermPattern::NamedNode(n) if n.as_str() == legacy)
                {
                    t.object = TermPattern::NamedNode(root.clone());
                }
            }
        }
        GraphPattern::Path { object, .. } => {
            if matches!(object, TermPattern::NamedNode(n) if n.as_str() == legacy) {
                *object = TermPattern::NamedNode(root.clone());
            }
        }
        GraphPattern::Filter { expr: e, .. }
        | GraphPattern::Extend { expression: e, .. }
        | GraphPattern::LeftJoin {
            expression: Some(e),
            ..
        } => legacy_root_expr(e, &legacy, &root),
        _ => {}
    });
}

fn legacy_root_expr(e: &mut Expression, legacy: &str, root: &spargebra::term::NamedNode) {
    match e {
        Expression::NamedNode(n) if n.as_str() == legacy => *n = root.clone(),
        Expression::NamedNode(_)
        | Expression::Literal(_)
        | Expression::Variable(_)
        | Expression::Bound(_)
        | Expression::Exists(_) => {}
        Expression::Or(a, b)
        | Expression::And(a, b)
        | Expression::Equal(a, b)
        | Expression::SameTerm(a, b)
        | Expression::Greater(a, b)
        | Expression::GreaterOrEqual(a, b)
        | Expression::Less(a, b)
        | Expression::LessOrEqual(a, b)
        | Expression::Add(a, b)
        | Expression::Subtract(a, b)
        | Expression::Multiply(a, b)
        | Expression::Divide(a, b) => {
            legacy_root_expr(a, legacy, root);
            legacy_root_expr(b, legacy, root);
        }
        Expression::UnaryPlus(a) | Expression::UnaryMinus(a) | Expression::Not(a) => {
            legacy_root_expr(a, legacy, root);
        }
        Expression::In(a, list) => {
            legacy_root_expr(a, legacy, root);
            list.iter_mut()
                .for_each(|x| legacy_root_expr(x, legacy, root));
        }
        Expression::If(a, b, c) => {
            legacy_root_expr(a, legacy, root);
            legacy_root_expr(b, legacy, root);
            legacy_root_expr(c, legacy, root);
        }
        Expression::Coalesce(list) | Expression::FunctionCall(_, list) => {
            list.iter_mut()
                .for_each(|x| legacy_root_expr(x, legacy, root));
        }
    }
}

/// `EXISTS` patterns inside expressions are not visited: build everything then.
fn expr_needs(expr: &Expression, needs: &mut Needs) {
    if expr.to_string().contains("EXISTS") {
        needs.all = true;
    }
}

fn path_needs(path: &PropertyPathExpression, needs: &mut Needs) {
    match path {
        PropertyPathExpression::NamedNode(n) => {
            needs.predicates.insert(n.as_str().to_owned());
        }
        PropertyPathExpression::Reverse(p)
        | PropertyPathExpression::ZeroOrMore(p)
        | PropertyPathExpression::OneOrMore(p)
        | PropertyPathExpression::ZeroOrOne(p)
        | PropertyPathExpression::Bounded { path: p, .. } => path_needs(p, needs),
        PropertyPathExpression::Sequence(a, b) | PropertyPathExpression::Alternative(a, b) => {
            path_needs(a, needs);
            path_needs(b, needs);
        }
        PropertyPathExpression::NegatedPropertySet(_) => needs.all = true,
    }
}

/// Calls `f` on `p` and every pattern under it, except inside nested SERVICE calls (they read
/// their own source).
pub(crate) fn visit(p: &GraphPattern, f: &mut impl FnMut(&GraphPattern)) {
    f(p);
    match p {
        GraphPattern::Join { left, right }
        | GraphPattern::LeftJoin { left, right, .. }
        | GraphPattern::Lateral { left, right }
        | GraphPattern::Union { left, right }
        | GraphPattern::Minus { left, right } => {
            visit(left, f);
            visit(right, f);
        }
        GraphPattern::Filter { inner, .. }
        | GraphPattern::Graph { inner, .. }
        | GraphPattern::Extend { inner, .. }
        | GraphPattern::OrderBy { inner, .. }
        | GraphPattern::Project { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. }
        | GraphPattern::Group { inner, .. } => visit(inner, f),
        GraphPattern::Bgp { .. }
        | GraphPattern::Path { .. }
        | GraphPattern::Values { .. }
        | GraphPattern::Service { .. } => {}
    }
}

/// [`visit`], children first, with mutable access.
pub(crate) fn visit_mut(p: &mut GraphPattern, f: &mut impl FnMut(&mut GraphPattern)) {
    match p {
        GraphPattern::Join { left, right }
        | GraphPattern::LeftJoin { left, right, .. }
        | GraphPattern::Lateral { left, right }
        | GraphPattern::Union { left, right }
        | GraphPattern::Minus { left, right } => {
            visit_mut(left, f);
            visit_mut(right, f);
        }
        GraphPattern::Filter { inner, .. }
        | GraphPattern::Graph { inner, .. }
        | GraphPattern::Extend { inner, .. }
        | GraphPattern::OrderBy { inner, .. }
        | GraphPattern::Project { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. }
        | GraphPattern::Group { inner, .. } => visit_mut(inner, f),
        GraphPattern::Bgp { .. }
        | GraphPattern::Path { .. }
        | GraphPattern::Values { .. }
        | GraphPattern::Service { .. } => {}
    }
    f(p);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(pairs: &[(&str, &str)]) -> Options {
        Options(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    #[test]
    fn iri_options() {
        assert_eq!(
            Options::from_iri("x-sparql-anything:location=data/a.csv,csv.headers=true"),
            Some(opts(&[("location", "data/a.csv"), ("csv.headers", "true")]))
        );
        assert_eq!(
            Options::from_iri("x-sparql-anything:https://ex.org/a.json"),
            Some(opts(&[("location", "https://ex.org/a.json")]))
        );
        assert_eq!(
            Options::from_iri(r"x-sparql-anything:csv.delimiter=\,,content=a\=b"),
            Some(opts(&[("csv.delimiter", ","), ("content", "a=b")]))
        );
        assert_eq!(Options::from_iri("x-sparql-anything:"), Some(opts(&[])));
        assert_eq!(Options::from_iri("http://ex.org/sparql"), None);
    }

    #[test]
    fn slots() {
        assert_eq!(slot_index(&format!("{RDF}_1")), Some(1));
        assert_eq!(slot_index(&format!("{RDF}_24")), Some(24));
        assert_eq!(slot_index(&format!("{RDF}_0")), None);
        assert_eq!(slot_index(&format!("{RDF}_01")), None);
        assert_eq!(slot_index(&format!("{RDF}_")), None);
        assert_eq!(slot_index(&format!("{RDF}type")), None);
    }
}
