//! User-defined functions: SHACL-AF `sh:SPARQLFunction`s (#235,
//! <https://www.w3.org/TR/shacl-af/#SPARQLFunction>).
//!
//! A function is declared as data, with `sh:parameter`s and a `sh:select` or `sh:ask` body:
//!
//! ```turtle
//! ex:kmToM a sh:SPARQLFunction ;
//!     sh:parameter [ sh:path ex:km ; sh:datatype xsd:decimal ] ;
//!     sh:returnType xsd:decimal ;
//!     sh:select "SELECT ($km * 1000 AS ?m) WHERE {}" .
//! ```
//!
//! and is then called by its IRI in any query, update, rule or constraint: `ex:kmToM(?km)`.
//! A parameter's variable is the local name of its `sh:path`; parameters are in the order of
//! their `sh:order`, else of their local names. A call runs the body with the parameters
//! bound to the arguments and gives the first row's value of the first selected variable, or
//! the `sh:ask` body's boolean. The body reads the store's default graph (or the union of
//! all graphs when the engine reads that as its default graph), not the calling query's
//! `FROM`.
//!
//! A body that is a single expression, `SELECT (expr AS ?r) WHERE {}` using no other
//! variables than the parameters, is inlined into the calling query, so a call costs
//! nothing more than writing the expression out. Functions may call one another but not
//! themselves, directly or through others.

use oxigraph::model::vocab::{rdf, xsd};
use oxigraph::model::{Graph, Literal, NamedNode, NamedOrBlankNodeRef, Term, TermRef, Triple};
use spargebra::Query;
use spargebra::algebra::{
    AggregateExpression, Expression, Function as F, GraphPattern, OrderExpression,
};
use spargebra::term::Variable;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

const SH: &str = "http://www.w3.org/ns/shacl#";

fn sh(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{SH}{name}"))
}

/// Why the functions of a graph were not loaded.
#[derive(Debug, thiserror::Error)]
#[error("sh:SPARQLFunction {function}: {message}")]
pub struct FunctionError {
    pub function: String,
    pub message: String,
}

/// One `sh:SPARQLFunction`.
#[derive(Debug, Clone)]
pub struct Function {
    pub iri: NamedNode,
    /// The parameters' variables, in call order, and whether each is optional.
    pub parameters: Vec<(Variable, bool)>,
    /// The body, planned at each call (with the arguments in it).
    pub(crate) body: Query,
    /// `None` for an `sh:ask` body; else the variable whose value the call gives.
    pub(crate) result: Option<Variable>,
    /// The body's expression when the function can be inlined.
    pub(crate) inline: Option<Expression>,
}

impl Function {
    fn required(&self) -> usize {
        self.parameters
            .iter()
            .filter(|(_, optional)| !optional)
            .count()
    }
}

/// The user-defined functions of an engine (see [`crate::Engine::with_functions`]).
#[derive(Debug, Clone, Default)]
pub struct Functions {
    map: HashMap<NamedNode, Arc<Function>>,
}

impl Functions {
    /// The `sh:SPARQLFunction`s of `triples` (a shapes graph, say).
    pub fn from_triples(triples: impl IntoIterator<Item = Triple>) -> Result<Self, FunctionError> {
        let graph: Graph = triples.into_iter().collect();
        let mut map = HashMap::new();
        let function = sh("SPARQLFunction");
        for subject in graph.subjects_for_predicate_object(rdf::TYPE, &function) {
            let NamedOrBlankNodeRef::NamedNode(iri) = subject else {
                return Err(FunctionError {
                    function: subject.to_string(),
                    message: "a function needs an IRI to be called by".to_owned(),
                });
            };
            let f = parse(&graph, iri.into_owned()).map_err(|message| FunctionError {
                function: iri.to_string(),
                message,
            })?;
            map.insert(f.iri.clone(), Arc::new(f));
        }
        let functions = Self { map };
        functions.check_recursion()?;
        Ok(functions)
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// The functions, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<Function>> {
        self.map.values()
    }

    pub fn get(&self, iri: &NamedNode) -> Option<&Arc<Function>> {
        self.map.get(iri)
    }

    /// These functions and `other`'s (`other`'s win where both define one).
    pub fn merged(&self, other: &Self) -> Result<Self, FunctionError> {
        let mut map = self.map.clone();
        map.extend(other.map.iter().map(|(k, v)| (k.clone(), Arc::clone(v))));
        let functions = Self { map };
        functions.check_recursion()?;
        Ok(functions)
    }

    /// Refuses a function that calls itself, directly or through others.
    fn check_recursion(&self) -> Result<(), FunctionError> {
        let calls: HashMap<&NamedNode, BTreeSet<NamedNode>> = self
            .map
            .iter()
            .map(|(k, f)| {
                let mut called = BTreeSet::new();
                query_calls(&f.body, &mut |n| {
                    if self.map.contains_key(n) {
                        called.insert(n.clone());
                    }
                });
                (k, called)
            })
            .collect();
        for start in calls.keys() {
            let mut seen = HashSet::new();
            let mut stack: Vec<&NamedNode> = calls[start].iter().collect();
            while let Some(n) = stack.pop() {
                if n == *start {
                    return Err(FunctionError {
                        function: start.to_string(),
                        message: "a function may not call itself, directly or through others"
                            .to_owned(),
                    });
                }
                if seen.insert(n) {
                    stack.extend(calls.get(n).into_iter().flatten());
                }
            }
        }
        Ok(())
    }

    /// Replaces the calls of inlinable functions in `query` by their expressions. Returns
    /// whether `query` still calls a function that is not inlined.
    pub(crate) fn inline(&self, query: &mut Query) -> bool {
        if self.map.is_empty() {
            return false;
        }
        let mut remaining = false;
        let mut on = |e: &mut Expression| self.inline_expression(e, &mut remaining);
        match query {
            Query::Select { pattern, .. }
            | Query::Ask { pattern, .. }
            | Query::Describe { pattern, .. } => pattern_expressions(pattern, &mut on),
            Query::Construct { pattern, .. } => pattern_expressions(pattern, &mut on),
        }
        remaining
    }

    /// [`Functions::inline`] for the `WHERE` clauses of an update. Returns whether it still
    /// calls a function that is not inlined.
    pub(crate) fn inline_update(&self, update: &mut spargebra::Update) -> bool {
        if self.map.is_empty() {
            return false;
        }
        let mut remaining = false;
        for op in &mut update.operations {
            if let spargebra::GraphUpdateOperation::DeleteInsert { pattern, .. } = op {
                pattern_expressions(pattern, &mut |e| self.inline_expression(e, &mut remaining));
            }
        }
        remaining
    }

    fn inline_expression(&self, e: &mut Expression, remaining: &mut bool) {
        // Inside out: the arguments first, then the call.
        expression_children(e, &mut |c| self.inline_expression(c, remaining));
        if let Expression::FunctionCall(F::Custom(name), args) = e
            && let Some(f) = self.map.get(name)
        {
            match f.inline.as_ref().and_then(|body| inlined(f, body, args)) {
                Some(new) => {
                    *e = new;
                    // The body may call other functions (recursion is refused, so this ends).
                    self.inline_expression(e, remaining);
                }
                None => *remaining = true,
            }
        }
    }
}

/// `body` with the parameters replaced by `args`, unless that would change what the call
/// means: missing arguments, or an argument with a random part (`RAND()`, `BNODE()`, …) used
/// more than once.
fn inlined(f: &Function, body: &Expression, args: &[Expression]) -> Option<Expression> {
    if args.len() != f.parameters.len() {
        return None;
    }
    let mut uses: HashMap<&Variable, usize> = HashMap::new();
    let mut e = body.clone();
    count_variables(body, &mut |v| *uses.entry(v).or_default() += 1);
    let mut bound = HashSet::new();
    visit_ref(body, &mut |e| {
        if let Expression::Bound(v) = e {
            bound.insert(v.clone());
        }
    });
    for ((param, _), arg) in f.parameters.iter().zip(args) {
        if uses.get(param).copied().unwrap_or(0) > 1 && !repeatable(arg) {
            return None;
        }
        if bound.contains(param) && !matches!(arg, Expression::Variable(_)) {
            return None;
        }
    }
    let by_name: HashMap<&Variable, &Expression> =
        f.parameters.iter().map(|(v, _)| v).zip(args).collect();
    substitute(&mut e, &by_name);
    Some(e)
}

fn count_variables<'a>(e: &'a Expression, f: &mut impl FnMut(&'a Variable)) {
    match e {
        Expression::Variable(v) | Expression::Bound(v) => f(v),
        _ => expression_children_ref(e, &mut |c| count_variables(c, f)),
    }
}

/// Whether evaluating `e` twice gives the same value.
fn repeatable(e: &Expression) -> bool {
    let mut ok = true;
    visit_ref(e, &mut |e| {
        if let Expression::FunctionCall(f, _) = e
            && matches!(f, F::Rand | F::BNode | F::Uuid | F::StrUuid | F::Custom(_))
        {
            ok = false;
        }
    });
    ok
}

fn visit_ref(e: &Expression, f: &mut impl FnMut(&Expression)) {
    f(e);
    expression_children_ref(e, &mut |c| visit_ref(c, f));
}

fn substitute(e: &mut Expression, args: &HashMap<&Variable, &Expression>) {
    match e {
        Expression::Variable(v) => {
            if let Some(a) = args.get(v) {
                *e = (*a).clone();
            }
        }
        // `inlined` only lets through BOUND($p) of a variable argument.
        Expression::Bound(v) => {
            if let Some(Expression::Variable(w)) = args.get(v) {
                *e = Expression::Bound(w.clone());
            }
        }
        _ => expression_children(e, &mut |c| substitute(c, args)),
    }
}

/// Reads one function from `graph`.
fn parse(graph: &Graph, iri: NamedNode) -> Result<Function, String> {
    let one = |s: NamedOrBlankNodeRef<'_>, p: &NamedNode| {
        graph
            .object_for_subject_predicate(s, p)
            .map(TermRef::into_owned)
    };
    let subject = NamedOrBlankNodeRef::NamedNode(iri.as_ref());
    // Parameters.
    let mut parameters = Vec::new();
    for p in graph.objects_for_subject_predicate(subject, &sh("parameter")) {
        let node = match p {
            TermRef::NamedNode(n) => NamedOrBlankNodeRef::NamedNode(n),
            TermRef::BlankNode(b) => NamedOrBlankNodeRef::BlankNode(b),
            _ => return Err("sh:parameter must be a node".to_owned()),
        };
        let Some(Term::NamedNode(path)) = one(node, &sh("path")) else {
            return Err("each sh:parameter needs an IRI as its sh:path".to_owned());
        };
        let name = local_name(path.as_str());
        let variable = Variable::new(name)
            .map_err(|_| format!("the local name of {path} is not a variable name"))?;
        let order = match one(node, &sh("order")) {
            Some(Term::Literal(l)) => Some(
                l.value()
                    .parse::<f64>()
                    .map_err(|_| format!("sh:order {l} is not a number"))?,
            ),
            None => None,
            Some(t) => return Err(format!("sh:order {t} is not a number")),
        };
        let optional = matches!(
            one(node, &sh("optional")),
            Some(Term::Literal(l)) if l.value() == "true" || l.value() == "1"
        );
        parameters.push((order, variable, optional));
    }
    parameters.sort_by(|a, b| {
        let key = |o: Option<f64>| o.unwrap_or(f64::INFINITY);
        key(a.0)
            .total_cmp(&key(b.0))
            .then_with(|| a.1.as_str().cmp(b.1.as_str()))
    });
    let mut seen = HashSet::new();
    for (_, v, _) in &parameters {
        if !seen.insert(v.as_str()) {
            return Err(format!("two parameters are called ${}", v.as_str()));
        }
    }
    // A required parameter after an optional one could never be given alone.
    if parameters.windows(2).any(|w| w[0].2 && !w[1].2) {
        return Err("the optional parameters must come after the required ones".to_owned());
    }
    let parameters: Vec<(Variable, bool)> =
        parameters.into_iter().map(|(_, v, o)| (v, o)).collect();
    // Body.
    let select = one(subject, &sh("select"));
    let ask = one(subject, &sh("ask"));
    let (text, is_ask) = match (select, ask) {
        (Some(Term::Literal(l)), None) => (l.value().to_owned(), false),
        (None, Some(Term::Literal(l))) => (l.value().to_owned(), true),
        (None, None) => return Err("needs a sh:select or a sh:ask query".to_owned()),
        (Some(_), Some(_)) => return Err("has both sh:select and sh:ask".to_owned()),
        _ => return Err("sh:select and sh:ask must be strings".to_owned()),
    };
    let mut prologue = String::new();
    for prefixes in graph.objects_for_subject_predicate(subject, &sh("prefixes")) {
        declarations(graph, prefixes, &mut prologue, &mut HashSet::new())?;
    }
    // `$x` and `?x` are the same variable in SPARQL.
    let source = format!("{prologue}{text}");
    let parsed = crate::Engine::parse_query_with_base(&source, None).map_err(|e| format!("{e}"))?;
    let result = match &parsed {
        Query::Select { pattern, .. } if !is_ask => Some(
            first_projected(pattern)
                .ok_or_else(|| "the sh:select query must select a variable".to_owned())?,
        ),
        Query::Ask { .. } if is_ask => None,
        _ if is_ask => return Err("sh:ask must hold an ASK query".to_owned()),
        _ => return Err("sh:select must hold a SELECT query".to_owned()),
    };
    let inline = result
        .as_ref()
        .and_then(|r| single_expression(&parsed, r))
        .filter(|e| {
            let mut ok = true;
            count_variables(e, &mut |v| ok &= parameters.iter().any(|(p, _)| p == v));
            let mut no_exists = true;
            visit_ref(e, &mut |e| no_exists &= !matches!(e, Expression::Exists(_)));
            ok && no_exists
        });
    Ok(Function {
        iri,
        parameters,
        body: parsed,
        result,
        inline,
    })
}

/// `PREFIX` lines for the `sh:declare`s of `node` and the nodes it `owl:imports`.
fn declarations(
    graph: &Graph,
    node: TermRef<'_>,
    out: &mut String,
    seen: &mut HashSet<Term>,
) -> Result<(), String> {
    if !seen.insert(node.into_owned()) {
        return Ok(());
    }
    let node = match node {
        TermRef::NamedNode(n) => NamedOrBlankNodeRef::NamedNode(n),
        TermRef::BlankNode(b) => NamedOrBlankNodeRef::BlankNode(b),
        _ => return Err("sh:prefixes must be a node".to_owned()),
    };
    for d in graph.objects_for_subject_predicate(node, &sh("declare")) {
        let d = match d {
            TermRef::NamedNode(n) => NamedOrBlankNodeRef::NamedNode(n),
            TermRef::BlankNode(b) => NamedOrBlankNodeRef::BlankNode(b),
            _ => continue,
        };
        let prefix = graph.object_for_subject_predicate(d, &sh("prefix"));
        let namespace = graph.object_for_subject_predicate(d, &sh("namespace"));
        if let (Some(TermRef::Literal(p)), Some(TermRef::Literal(ns))) = (prefix, namespace) {
            out.push_str(&format!("PREFIX {}: <{}>\n", p.value(), ns.value()));
        } else {
            return Err("each sh:declare needs sh:prefix and sh:namespace strings".to_owned());
        }
    }
    let imports = NamedNode::new_unchecked("http://www.w3.org/2002/07/owl#imports");
    let imported: Vec<Term> = graph
        .objects_for_subject_predicate(node, &imports)
        .map(TermRef::into_owned)
        .collect();
    for i in &imported {
        declarations(graph, i.as_ref(), out, seen)?;
    }
    Ok(())
}

fn local_name(iri: &str) -> &str {
    let cut = iri.rfind(['#', '/', ':']).map_or(0, |i| i + 1);
    &iri[cut..]
}

fn first_projected(pattern: &GraphPattern) -> Option<Variable> {
    match pattern {
        GraphPattern::Project { variables, .. } => variables.first().cloned(),
        GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. }
        | GraphPattern::OrderBy { inner, .. } => first_projected(inner),
        _ => None,
    }
}

/// The expression of `SELECT (expr AS ?r) WHERE {}`.
fn single_expression(query: &Query, result: &Variable) -> Option<Expression> {
    let Query::Select {
        pattern: GraphPattern::Project { inner, variables },
        dataset: None,
        ..
    } = query
    else {
        return None;
    };
    if variables.as_slice() != std::slice::from_ref(result) {
        return None;
    }
    let GraphPattern::Extend {
        inner,
        variable,
        expression,
    } = inner.as_ref()
    else {
        return None;
    };
    match inner.as_ref() {
        GraphPattern::Bgp { patterns } if patterns.is_empty() && variable == result => {
            Some(expression.clone())
        }
        _ => None,
    }
}

/// Runs `f`'s body with `args` on `engine`.
pub(crate) fn call(engine: &crate::Engine, f: &Function, args: &[Term]) -> Option<Term> {
    if args.len() < f.required() || args.len() > f.parameters.len() {
        return None;
    }
    let variables = f.parameters[..args.len()]
        .iter()
        .map(|(v, _)| v.clone())
        .collect();
    let body = tripletrail_shacl::prebind(f.body.clone(), variables, args.to_vec());
    let r = engine
        .store()
        .execute(engine.prepare(crate::plan::plan_query(body)));
    match r.ok()? {
        oxigraph::sparql::QueryResults::Solutions(mut rows) => {
            let row = rows.next()?.ok()?;
            row.get(f.result.as_ref()?).cloned()
        }
        oxigraph::sparql::QueryResults::Boolean(b) => {
            Some(Literal::new_typed_literal(b.to_string(), xsd::BOOLEAN).into())
        }
        oxigraph::sparql::QueryResults::Graph(_) => None,
    }
}

/// Calls `f` on the IRI of each custom function `query` calls.
fn query_calls(query: &Query, f: &mut impl FnMut(&NamedNode)) {
    let mut on = |e: &mut Expression| {
        visit_ref(e, &mut |e| {
            if let Expression::FunctionCall(F::Custom(n), _) = e {
                f(n);
            }
        });
    };
    let mut pattern = match query {
        Query::Select { pattern, .. }
        | Query::Ask { pattern, .. }
        | Query::Describe { pattern, .. }
        | Query::Construct { pattern, .. } => pattern.clone(),
    };
    pattern_expressions(&mut pattern, &mut on);
}

/// Calls `f` on each top-level expression of `p` (and those of `EXISTS` patterns within).
pub(crate) fn pattern_expressions(p: &mut GraphPattern, f: &mut impl FnMut(&mut Expression)) {
    use GraphPattern as G;
    fn exprs(e: &mut Expression, f: &mut dyn FnMut(&mut Expression)) {
        exists_patterns(e, &mut |p| pattern_expressions_dyn(p, f));
        f(e);
    }
    match p {
        G::Bgp { .. } | G::Path { .. } | G::Values { .. } => {}
        G::Join { left, right }
        | G::Lateral { left, right }
        | G::Union { left, right }
        | G::Minus { left, right } => {
            pattern_expressions(left, f);
            pattern_expressions(right, f);
        }
        G::LeftJoin {
            left,
            right,
            expression,
        } => {
            pattern_expressions(left, f);
            pattern_expressions(right, f);
            if let Some(e) = expression {
                exprs(e, f);
            }
        }
        G::Filter { expr, inner } => {
            pattern_expressions(inner, f);
            exprs(expr, f);
        }
        G::Extend {
            inner, expression, ..
        } => {
            pattern_expressions(inner, f);
            exprs(expression, f);
        }
        G::OrderBy { inner, expression } => {
            pattern_expressions(inner, f);
            for o in expression {
                match o {
                    OrderExpression::Asc(e) | OrderExpression::Desc(e) => exprs(e, f),
                }
            }
        }
        G::Group {
            inner, aggregates, ..
        } => {
            pattern_expressions(inner, f);
            for (_, a) in aggregates {
                if let AggregateExpression::FunctionCall { expr, .. } = a {
                    exprs(expr, f);
                }
            }
        }
        G::Graph { inner, .. }
        | G::Project { inner, .. }
        | G::Distinct { inner }
        | G::Reduced { inner }
        | G::Slice { inner, .. }
        | G::Service { inner, .. } => pattern_expressions(inner, f),
    }
}

fn pattern_expressions_dyn(p: &mut GraphPattern, f: &mut dyn FnMut(&mut Expression)) {
    pattern_expressions(p, &mut |e| f(e));
}

/// Calls `f` on the patterns of the `EXISTS` within `e`.
fn exists_patterns(e: &mut Expression, f: &mut dyn FnMut(&mut GraphPattern)) {
    if let Expression::Exists(p) = e {
        f(p);
    } else {
        expression_children(e, &mut |c| exists_patterns(c, f));
    }
}

fn expression_children(e: &mut Expression, f: &mut dyn FnMut(&mut Expression)) {
    use Expression as E;
    match e {
        E::NamedNode(_) | E::Literal(_) | E::Variable(_) | E::Bound(_) | E::Exists(_) => {}
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
            f(a);
            f(b);
        }
        E::UnaryPlus(a) | E::UnaryMinus(a) | E::Not(a) => f(a),
        E::In(a, list) => {
            f(a);
            list.iter_mut().for_each(f);
        }
        E::If(a, b, c) => {
            f(a);
            f(b);
            f(c);
        }
        E::Coalesce(list) | E::FunctionCall(_, list) => list.iter_mut().for_each(f),
    }
}

fn expression_children_ref<'a>(e: &'a Expression, f: &mut dyn FnMut(&'a Expression)) {
    use Expression as E;
    match e {
        E::NamedNode(_) | E::Literal(_) | E::Variable(_) | E::Bound(_) | E::Exists(_) => {}
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
            f(a);
            f(b);
        }
        E::UnaryPlus(a) | E::UnaryMinus(a) | E::Not(a) => f(a),
        E::In(a, list) => {
            f(a);
            list.iter().for_each(f);
        }
        E::If(a, b, c) => {
            f(a);
            f(b);
            f(c);
        }
        E::Coalesce(list) | E::FunctionCall(_, list) => list.iter().for_each(f),
    }
}
