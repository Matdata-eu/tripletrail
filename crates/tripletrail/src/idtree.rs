//! Patterns beyond a basic graph pattern evaluated on term ids, for counts (issue #167):
//! joins, `OPTIONAL`, `MINUS`, `FILTER (NOT) EXISTS` and `UNION` over basic graph patterns,
//! and `FILTER(?a = ?b)` / `sameTerm`. Each basic graph pattern is joined in the store
//! (`Snapshot::join_ids`); the operators above run on whole tables of ids with hash tables
//! on the shared variables, instead of one row at a time in the evaluator.
//!
//! Only shapes whose result on ids is certain are taken: shared variables must be bound in
//! every row of both sides (so compatibility is equality), `OPTIONAL` has no filter, and an
//! `EXISTS` pattern is a basic graph pattern. Anything else, a table too large to hold, or a
//! comparison of two different literals (`=` compares values) gives `None`, and the query is
//! evaluated as usual.

use oxigraph::sparql::Variable;
use spargebra::algebra::PropertyPathExpression;
use spargebra::algebra::{Expression, Function, GraphPattern};
use spargebra::term::{NamedNodePattern, TermPattern};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use tripletrail_store::{GraphSelector, IdSlot, IdTriple, Kind, Snapshot, TermId};

/// Ids (8 bytes each) held at most in the tables of one query: half the store's join budget
/// ([`tripletrail_store::set_join_memory`], #479), as the store's joins feed these tables.
fn max_ids() -> usize {
    tripletrail_store::join_memory() / 16
}
/// A lookup by a seed row's values costs about as much as scanning this many quads (as in
/// `tripletrail_store`'s join planning).
const LOOKUP: f64 = 40.0;
/// A key found by walking a sorted scan forward costs about as much as scanning this many
/// quads.
const WALK: f64 = 4.0;
/// The edges of one step of a path: predicates and their inverses (`ex:a`, `^ex:a`,
/// `ex:a|^ex:b`), each with whether it is followed backwards.
pub(crate) type Steps<'p> = Vec<(&'p spargebra::term::NamedNode, bool)>;

/// `p` as [`Steps`]: a predicate, its inverse, or an alternative of those.
fn steps(p: &PropertyPathExpression) -> Option<Steps<'_>> {
    let mut out = Vec::new();
    walk_alternative(p, &mut out)?;
    Some(out)
}

/// `<p>+`, `<p>*` and their inverses, where `<p>` is a predicate, its inverse or an
/// alternative of those (`(ex:a|^ex:b)*`, #11): the step's edges, whether the whole path is
/// followed backwards, and whether it matches zero-length paths.
pub(crate) fn closure_path(path: &PropertyPathExpression) -> Option<(Steps<'_>, bool, bool)> {
    use PropertyPathExpression as P;
    let (inner, zero, outer_reverse) = match path {
        P::OneOrMore(p) => (&**p, false, false),
        P::ZeroOrMore(p) => (&**p, true, false),
        P::Reverse(p) => match &**p {
            P::OneOrMore(p) => (&**p, false, true),
            P::ZeroOrMore(p) => (&**p, true, true),
            _ => return None,
        },
        _ => return None,
    };
    Some((steps(inner)?, outer_reverse, zero))
}

/// `!(p|q|…)` and `^!(…)` (`!(^p|^q)`): the predicates left out, whether the triples are
/// followed backwards. A set of both kinds is the evaluator's.
pub(crate) fn negated_path(
    path: &PropertyPathExpression,
) -> Option<(&[spargebra::term::NamedNode], bool)> {
    use PropertyPathExpression as P;
    match path {
        P::NegatedPropertySet(ps) => Some((ps, false)),
        P::Reverse(p) => match &**p {
            P::NegatedPropertySet(ps) => Some((ps, true)),
            _ => None,
        },
        _ => None,
    }
}

/// `<p>?` and `^<p>?` (or `(^<p>)?`): the predicate, whether it is followed backwards.
pub(crate) fn optional_path(
    path: &PropertyPathExpression,
) -> Option<(&spargebra::term::NamedNode, bool)> {
    use PropertyPathExpression as P;
    let (inner, outer_reverse) = match path {
        P::ZeroOrOne(p) => (&**p, false),
        P::Reverse(p) => match &**p {
            P::ZeroOrOne(p) => (&**p, true),
            _ => return None,
        },
        _ => return None,
    };
    match inner {
        P::NamedNode(n) => Some((n, outer_reverse)),
        P::Reverse(p) => match &**p {
            P::NamedNode(n) => Some((n, !outer_reverse)),
            _ => None,
        },
        _ => None,
    }
}

/// The predicates of an alternative of predicates and their inverses (`ex:a|^ex:b|ex:c`), each
/// with whether it is followed backwards: the evaluator's union of their pairs without
/// duplicates.
pub(crate) fn alternative_path(
    path: &PropertyPathExpression,
) -> Option<Vec<(&spargebra::term::NamedNode, bool)>> {
    if !matches!(path, PropertyPathExpression::Alternative(..)) {
        return None;
    }
    steps(path)
}

/// Adds the predicates of an alternative of predicates and their inverses to `out`.
fn walk_alternative<'p>(p: &'p PropertyPathExpression, out: &mut Steps<'p>) -> Option<()> {
    use PropertyPathExpression as P;
    match p {
        P::Alternative(a, b) => {
            walk_alternative(a, out)?;
            walk_alternative(b, out)
        }
        P::NamedNode(n) => {
            out.push((n, false));
            Some(())
        }
        P::Reverse(r) => match &**r {
            P::NamedNode(n) => {
                out.push((n, true));
                Some(())
            }
            _ => None,
        },
        _ => None,
    }
}

/// The operands of a tree of joins, in order.
fn joined_parts<'p>(p: &'p GraphPattern, out: &mut Vec<&'p GraphPattern>) {
    match p {
        GraphPattern::Join { left, right } => {
            joined_parts(left, out);
            joined_parts(right, out);
        }
        other => out.push(other),
    }
}

/// Whether `p` has a path, outside an `EXISTS`.
pub(crate) fn has_path(p: &GraphPattern) -> bool {
    match p {
        GraphPattern::Path { .. } => true,
        GraphPattern::Join { left, right }
        | GraphPattern::LeftJoin { left, right, .. }
        | GraphPattern::Minus { left, right }
        | GraphPattern::Union { left, right } => has_path(left) || has_path(right),
        GraphPattern::Filter { inner, .. } => has_path(inner),
        _ => false,
    }
}

/// A start searched by looking its edges up costs about as much as reading this many of
/// the predicate's edges: fewer starts than the edges over this are searched level by level.
const LEVEL_COST: f64 = 64.0;

/// The id of an unbound value: 0 is the default graph, never a subject, predicate or object.
const UNBOUND: TermId = TermId(0);

/// A hasher for pairs of ids, for the distinct inputs of a `BIND`.
#[derive(Default)]
struct PairHasher(u64);

impl std::hash::Hasher for PairHasher {
    fn finish(&self) -> u64 {
        self.0 ^ (self.0 >> 29)
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(u64::from(b));
        }
    }
    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

/// The rows whose pairs of ids a string column indexes before it decides whether they
/// repeat enough (one in ten) to compute each pair once.
const PAIR_TRIAL: usize = 65_536;

type PairHash = std::hash::BuildHasherDefault<PairHasher>;

/// Rows of ids, one column per variable.
pub(crate) struct Table {
    pub(crate) vars: Vec<Variable>,
    /// `ids[r * vars.len() + c]`.
    pub(crate) ids: Vec<TermId>,
    /// Columns that may be unbound in some rows.
    pub(crate) maybe: Vec<bool>,
    /// Rows (also for a table without columns).
    pub(crate) ones: usize,
}

impl Table {
    pub(crate) fn len(&self) -> usize {
        self.ones
    }

    fn width(&self) -> usize {
        self.vars.len()
    }

    pub(crate) fn row(&self, r: usize) -> &[TermId] {
        let w = self.width();
        &self.ids[r * w..(r + 1) * w]
    }

    pub(crate) fn col(&self, v: &Variable) -> Option<usize> {
        self.vars.iter().position(|x| x == v)
    }
}

pub(crate) struct Eval<'a> {
    pub(crate) snapshot: &'a Snapshot,
    pub(crate) cancelled: &'a (dyn Fn() -> bool + Sync),
    /// The graphs a query's default graph reads: the default graph, or every graph of a
    /// snapshot that hides the copies of triples in several graphs (the union, #808).
    pub(crate) graph: GraphSelector,
}

impl Eval<'_> {
    pub(crate) fn eval(&self, p: &GraphPattern) -> Option<Table> {
        if (self.cancelled)() {
            return None;
        }
        let t = match p {
            GraphPattern::Bgp { patterns } => self.bgp(patterns)?,
            GraphPattern::Join { .. } => {
                // The parts joined, paths last: they start from the values the others give.
                let mut parts = Vec::new();
                joined_parts(p, &mut parts);
                parts.sort_by_key(|p| matches!(p, GraphPattern::Path { .. }));
                let mut t = self.eval(parts[0])?;
                for part in &parts[1..] {
                    let r = self.right(&t, part)?;
                    t = join(&t, &r, self.cancelled)?;
                }
                t
            }
            GraphPattern::LeftJoin {
                left,
                right,
                expression: None,
            } => {
                let l = self.eval(left)?;
                let r = self.right(&l, right)?;
                left_join(&l, &r, self.cancelled)?
            }
            // `OPTIONAL { … FILTER(e) }` (#11): the filter is on the merged row, so it is the
            // right side's own when every variable it reads is either always bound on the
            // right side or never on the left (then both rows give it the same value, or
            // leave it unbound). Otherwise the evaluator takes it.
            GraphPattern::LeftJoin {
                left,
                right,
                expression: Some(e),
            } => {
                let l = self.eval(left)?;
                let r = self.right(&l, right)?;
                let mut names = std::collections::HashSet::new();
                // Not listed (an `EXISTS` inside): on the joined rows.
                let listed = crate::plan::expression_variables(e, &mut names).is_some();
                let own = listed
                    && names.iter().all(|n| {
                        let certain = r
                            .vars
                            .iter()
                            .position(|v| v.as_str() == n)
                            .is_some_and(|c| !r.maybe[c]);
                        certain || !l.vars.iter().any(|v| v.as_str() == n)
                    });
                if own {
                    let r = self.filter(e, r)?;
                    left_join(&l, &r, self.cancelled)?
                } else {
                    // The filter reads values of the left row too: it is on the joined rows,
                    // and a left row none of whose joined rows pass is kept alone.
                    self.left_join_filtered(&l, &r, e)?
                }
            }
            GraphPattern::Minus { left, right } => {
                let l = self.eval(left)?;
                let r = self.right(&l, right)?;
                minus(&l, &r, self.cancelled)?
            }
            GraphPattern::Union { left, right } => {
                let (l, r) = (self.eval(left)?, self.eval(right)?);
                union(l, r)?
            }
            GraphPattern::Filter { expr, inner } => {
                if let Some(p) = pushed_filter(expr, inner) {
                    return self.eval(&p);
                }
                // A filter on a computed `BIND` value: on the expression itself, under the
                // `BIND`, so its kernel computes the value (#11).
                if let GraphPattern::Extend {
                    inner: below,
                    variable,
                    expression,
                } = &**inner
                    && crate::executor_ids()
                    && !matches!(
                        expression,
                        Expression::Variable(_) | Expression::NamedNode(_) | Expression::Literal(_)
                    )
                    && let Some(e) = substituted(expr, variable, expression)
                {
                    return self.eval(&GraphPattern::Extend {
                        inner: Box::new(GraphPattern::Filter {
                            expr: e,
                            inner: below.clone(),
                        }),
                        variable: variable.clone(),
                        expression: expression.clone(),
                    });
                }
                if let GraphPattern::Bgp { patterns } = &**inner
                    && let Some((keep, copy, patterns)) = equal_vars(expr, patterns)
                {
                    let t = self.bgp(&patterns)?;
                    with_copy(t, &keep, copy)?
                } else {
                    let t = self.eval(inner)?;
                    self.filter(expr, t)?
                }
            }
            GraphPattern::Path {
                subject,
                path,
                object,
            } => match (alternative_path(path), negated_path(path)) {
                (Some(alternatives), _) => self.alternative(subject, &alternatives, object)?,
                (_, Some((left_out, reverse))) => {
                    self.negated(subject, left_out, reverse, object, None)?
                }
                _ => self.path(subject, path, object, None)?,
            },
            // `VALUES`, while the id executor answers whole queries (#11): each term by its
            // stored id, or one made up for the query; `UNDEF` unbound.
            GraphPattern::Values {
                variables,
                bindings,
            } if crate::executor_ids() => {
                let width = variables.len();
                let mut ids = Vec::with_capacity(bindings.len() * width);
                let mut maybe = vec![false; width];
                for row in bindings {
                    for (c, cell) in row.iter().enumerate() {
                        ids.push(match cell {
                            Some(term) => self.snapshot.id_of(term.clone().into()),
                            None => {
                                maybe[c] = true;
                                TermId(0)
                            }
                        });
                    }
                }
                Table {
                    vars: variables.clone(),
                    ids,
                    maybe,
                    ones: bindings.len(),
                }
            }
            // A sub-select's projection (the groups a count pass reads back from a table of
            // ids, #185), while the id executor answers whole queries (#11): its columns,
            // a variable it doesn't bind unbound in every row.
            // A sub-select with `DISTINCT`, `REDUCED`, an order or a slice (#11).
            GraphPattern::Slice { .. }
            | GraphPattern::Distinct { .. }
            | GraphPattern::Reduced { .. }
                if crate::executor_ids() =>
            {
                crate::direct::sub_select(p, self)?
            }
            GraphPattern::Project { inner, .. }
                if crate::executor_ids() && matches!(**inner, GraphPattern::OrderBy { .. }) =>
            {
                crate::direct::sub_select(p, self)?
            }
            GraphPattern::Project { inner, variables } if crate::executor_ids() => {
                let t = self.eval(inner)?;
                project(t, variables)
            }
            // `GRAPH`, while the id executor answers whole queries (#11): the pattern in that
            // graph, or in each graph with its name in the variable. Not over a default graph
            // that is the union of the graphs (its view hides the copies of triples).
            GraphPattern::Graph { name, inner }
                if crate::executor_ids()
                    && self.graph == GraphSelector::Default
                    && !matches!(&**inner, GraphPattern::Bgp { patterns } if patterns.is_empty()) =>
            {
                self.named(name, inner)?
            }
            // `BIND`, while the id executor answers whole queries (#11).
            GraphPattern::Extend {
                inner,
                variable,
                expression,
            } if crate::executor_ids() => {
                let t = self.eval(inner)?;
                self.extend(t, variable, expression)?
            }
            _ => return None,
        };
        (t.ids.len() <= max_ids()).then_some(t)
    }

    /// `t` with the column `variable` of `expression`'s values: a copy of a column, a constant,
    /// or a number computed on ids (`idvalue::compile`), as the term the evaluator gives; an
    /// error leaves the cell unbound. `None` when the ids don't decide every value.
    fn extend(&self, t: Table, variable: &Variable, expression: &Expression) -> Option<Table> {
        if t.col(variable).is_some() {
            return None;
        }
        let width = t.width();
        let rows = t.len();
        let mut column: Vec<TermId> = Vec::with_capacity(rows);
        let maybe = match expression {
            Expression::Variable(v) => match t.col(v) {
                Some(c) => {
                    column.extend((0..rows).map(|r| t.row(r)[c]));
                    t.maybe[c]
                }
                None => {
                    column.resize(rows, TermId(0));
                    rows > 0
                }
            },
            Expression::NamedNode(n) => {
                let id = self.snapshot.id_of(n.clone().into());
                column.resize(rows, id);
                false
            }
            Expression::Literal(l) => {
                let id = self.snapshot.id_of(l.clone().into());
                column.resize(rows, id);
                false
            }
            // Strings and IRIs (`STR`, `CONCAT`, `IRI`, `STRLANG`, `STRDT`, `REPLACE`), and what
            // the evaluator's own code computes (`idstring::evaluated`): a term a row.
            e if crate::idstring::computable(e) => self.term_column(&t, e, false, &mut column)?,
            // A number; a value the number kernels give up on (date-time arithmetic) is
            // computed with the evaluator's own code.
            e => match self.number_column(&t, e, &mut column) {
                Some(unbound) => unbound,
                None if crate::idstring::evaluated(e) => {
                    column.clear();
                    self.term_column(&t, e, true, &mut column)?
                }
                None => return None,
            },
        };
        let mut vars = t.vars;
        vars.push(variable.clone());
        let mut ids = Vec::with_capacity(rows * (width + 1));
        for (r, &id) in column.iter().enumerate() {
            ids.extend_from_slice(&t.ids[r * width..(r + 1) * width]);
            ids.push(id);
        }
        let mut flags = t.maybe;
        flags.push(maybe);
        Some(Table {
            vars,
            ids,
            maybe: flags,
            ones: rows,
        })
    }

    /// The terms of `e` for the rows of `t` in `column`, with [`crate::idstring::compile`] (or,
    /// `evaluated`, the evaluator's own code), each distinct pair of the ids it reads once;
    /// whether a cell is unbound. `None` when the kernels give up.
    fn term_column(
        &self,
        t: &Table,
        e: &Expression,
        evaluated: bool,
        column: &mut Vec<TermId>,
    ) -> Option<bool> {
        let rows = t.len();
        // An `IF` or `COALESCE`: the term it gives back, as the evaluator holds it (a number
        // keeps its text), on several cores from 8,192 rows, each chunk with its own kernel.
        let written = crate::idstring::written_branches(e, t);
        if crate::idstring::id_kernel(e, t, self.snapshot, written).is_some() {
            let parts = tripletrail_store::par::chunks_sized(rows, 8_192, 2_048, |range| {
                let kernel = crate::idstring::id_kernel(e, t, self.snapshot, written)?;
                let mut ids = Vec::with_capacity(range.len());
                for (n, r) in range.enumerate() {
                    if n % 4096 == 0 && (self.cancelled)() {
                        return None;
                    }
                    ids.push(kernel(t.row(r))?.unwrap_or(TermId(0)));
                }
                Some(ids)
            });
            if parts.iter().all(Option::is_some) {
                let start = column.len();
                column.extend(parts.into_iter().flatten().flatten());
                return Some(column[start..].contains(&TermId(0)));
            }
            if (self.cancelled)() {
                return None;
            }
        }
        let compile = |e: &Expression, t: &Table| {
            if evaluated {
                crate::idstring::evaluator_kernel(e, t, self.snapshot)
            } else {
                crate::idstring::compile(e, t, self.snapshot)
            }
        };
        // Gives up here, before any chunk, when the kernels can't compute it.
        let _ = compile(e, t)?;
        // The value depends only on the columns the expression reads: with one or
        // two, each distinct pair of ids is computed once.
        let mut names = std::collections::HashSet::new();
        crate::plan::expression_variables(e, &mut names)?;
        let cols: Vec<usize> = names
            .iter()
            .filter_map(|n| t.vars.iter().position(|v| v.as_str() == n))
            .collect();
        let key = |row: &[TermId]| match cols[..] {
            [] => Some((TermId(0), TermId(0))),
            [a] => Some((row[a], TermId(0))),
            [a, b] => Some((row[a], row[b])),
            _ => None,
        };
        // Each distinct pair computed once (the pairs met first, then their strings).
        // From 8,192 strings to compute on several cores (a string costs more than a
        // number), each chunk with its own kernel, its terms then given their ids under
        // one lock.
        // The rows to compute, and for each row of `t` its place among them. When the first
        // `PAIR_TRIAL` rows hardly repeat a pair (`STR(?s)` of every row), every row is
        // computed, without an index entry per row.
        let index_pairs = || {
            let mut index: HashMap<(TermId, TermId), u32, PairHash> = HashMap::default();
            let mut todo = Vec::new();
            let mut at = Vec::with_capacity(rows);
            for r in 0..rows {
                if r % 65_536 == 0 && (self.cancelled)() {
                    return Some(None);
                }
                if r == PAIR_TRIAL && index.len() * 10 > r * 9 {
                    return None;
                }
                let i = *index.entry(key(t.row(r))?).or_insert_with(|| {
                    todo.push(r);
                    (todo.len() - 1) as u32
                });
                at.push(i);
            }
            Some(Some((todo, Some(at))))
        };
        let pairs = if cols.len() <= 2 { index_pairs() } else { None };
        let (todo, at): (Vec<usize>, Option<Vec<u32>>) = match pairs {
            Some(Some(pairs)) => pairs,
            // Cancelled.
            Some(None) => return None,
            None => ((0..rows).collect(), None),
        };
        let parts = tripletrail_store::par::chunks_sized(todo.len(), 8_192, 2_048, |range| {
            let kernel = compile(e, t)?;
            let mut terms = Vec::new();
            // Whether each row's term is in `terms` (false: an error).
            let mut made = Vec::with_capacity(range.len());
            for (n, &r) in todo[range].iter().enumerate() {
                if n % 4096 == 0 && (self.cancelled)() {
                    return None;
                }
                match kernel(t.row(r))? {
                    Ok(term) => {
                        terms.push(term);
                        made.push(true);
                    }
                    Err(()) => made.push(false),
                }
            }
            Some((self.snapshot.prepare_ids(terms), made))
        });
        let mut computed = Vec::with_capacity(todo.len());
        for part in parts {
            let (terms, made) = part?;
            let mut ids = self.snapshot.finish_ids(terms).into_iter();
            for m in made {
                computed.push(if m { ids.next()? } else { TermId(0) });
            }
        }
        match at {
            Some(at) => column.extend(at.iter().map(|&i| computed[i as usize])),
            None => column.extend(computed),
        }
        Some(column.contains(&TermId(0)))
    }

    /// The numbers of `e` for the rows of `t` in `column`, as inline ids or literals; whether
    /// a cell is unbound. `None` when the kernels give up.
    fn number_column(
        &self,
        t: &Table,
        expression: &Expression,
        column: &mut Vec<TermId>,
    ) -> Option<bool> {
        let rows = t.len();
        let kernel = crate::idvalue::compile_output(expression, t, self.snapshot)?;
        // Each value once: the same value (in the same form) gives the same id.
        let mut ids: HashMap<((u8, i128), crate::idvalue::Form), TermId> = HashMap::new();
        let mut unbound = false;
        for r in 0..rows {
            if r % 4096 == 0 && (self.cancelled)() {
                return None;
            }
            match kernel(t.row(r))? {
                Ok((v, form)) => {
                    // An inline number is its id: no lookup, no literal.
                    if let Some(id) = crate::idvalue::inline_id(v, form) {
                        column.push(id);
                        continue;
                    }
                    let key = (value_key(v)?, form);
                    let id = match ids.get(&key) {
                        Some(&id) => id,
                        None => {
                            let id = self
                                .snapshot
                                .id_of(crate::idvalue::literal(v, form)?.into());
                            ids.insert(key, id);
                            id
                        }
                    };
                    column.push(id);
                }
                Err(()) => {
                    unbound = true;
                    column.push(TermId(0));
                }
            }
        }
        Some(unbound)
    }

    /// `subject !(p|q|…) object`: the triples whose predicate is none of `left_out`, one row
    /// per triple (backwards for `^!(…)`), as the evaluator gives them. With `seed`, the
    /// triples are looked up from the values its rows give the ends, as [`Self::bgp_from`]
    /// does.
    fn negated(
        &self,
        subject: &TermPattern,
        left_out: &[spargebra::term::NamedNode],
        reverse: bool,
        object: &TermPattern,
        seed: Option<&Table>,
    ) -> Option<Table> {
        if [subject, object]
            .iter()
            .any(|t| matches!(t, TermPattern::BlankNode(_) | TermPattern::Triple(_)))
        {
            return None;
        }
        // Two constant ends match once, however many triples join them: the evaluator's.
        if ![subject, object]
            .iter()
            .any(|t| matches!(t, TermPattern::Variable(_)))
        {
            return None;
        }
        let (s, o) = if reverse {
            (object, subject)
        } else {
            (subject, object)
        };
        // A name no query can hold (`-` is no character of a SPARQL variable name): the
        // seed's columns are the query's variables.
        let p = Variable::new_unchecked("negated-p");
        let t = self.bgp_from(
            &[spargebra::term::TriplePattern {
                subject: s.clone(),
                predicate: NamedNodePattern::Variable(p.clone()),
                object: o.clone(),
            }],
            seed,
        )?;
        // Predicates the store doesn't hold leave out no triple.
        let out: Vec<TermId> = left_out
            .iter()
            .filter_map(|n| self.snapshot.term_id(n.as_ref().into()))
            .collect();
        let c = t.col(&p)?;
        let width = t.width();
        let mut ids = Vec::with_capacity(t.ids.len());
        let mut ones = 0;
        for r in 0..t.len() {
            let row = t.row(r);
            if !out.contains(&row[c]) {
                ids.extend(
                    row.iter()
                        .enumerate()
                        .filter(|&(i, _)| i != c)
                        .map(|(_, &id)| id),
                );
                ones += 1;
            }
        }
        debug_assert_eq!(ids.len(), ones * (width - 1));
        let mut vars = t.vars;
        vars.remove(c);
        let mut maybe = t.maybe;
        maybe.remove(c);
        Some(Table {
            vars,
            ids,
            maybe,
            ones,
        })
    }

    /// `subject (p|^q|…) object`: the pairs each predicate's triples give (backwards for an
    /// inverse), without duplicates, as the evaluator gives them.
    fn alternative(
        &self,
        subject: &TermPattern,
        alternatives: &[(&spargebra::term::NamedNode, bool)],
        object: &TermPattern,
    ) -> Option<Table> {
        if [subject, object]
            .iter()
            .any(|t| matches!(t, TermPattern::BlankNode(_) | TermPattern::Triple(_)))
        {
            return None;
        }
        let mut all: Option<Table> = None;
        // Each alternative's rows are distinct; rows of two alternatives can only be equal
        // when every column's range of ids overlaps (terms of different kinds never do).
        let mut ranges: Vec<Vec<(TermId, TermId)>> = Vec::new();
        let mut overlap = false;
        for &(predicate, reverse) in alternatives {
            let (s, o) = if reverse {
                (object, subject)
            } else {
                (subject, object)
            };
            let t = self.bgp(&[spargebra::term::TriplePattern {
                subject: s.clone(),
                predicate: NamedNodePattern::NamedNode(predicate.clone()),
                object: o.clone(),
            }])?;
            // Each variable's range, in the order of the variables' names: the tables of a
            // predicate and of an inverse hold the ends in different columns.
            let mut names: Vec<(&Variable, usize)> =
                t.vars.iter().enumerate().map(|(c, v)| (v, c)).collect();
            names.sort();
            let r: Vec<(TermId, TermId)> = names
                .iter()
                .map(|&(_, c)| {
                    (0..t.len())
                        .map(|r| t.row(r)[c])
                        .fold((TermId(u64::MAX), TermId(0)), |(lo, hi), id| {
                            (lo.min(id), hi.max(id))
                        })
                })
                .collect();
            overlap |= t.len() > 0
                && ranges
                    .iter()
                    .any(|q| q.iter().zip(&r).all(|(&(a, b), &(c, d))| a <= d && c <= b));
            if t.len() > 0 {
                ranges.push(r);
            }
            all = Some(match all {
                None => t,
                Some(a) => union(a, t)?,
            });
        }
        let t = all?;
        if !overlap {
            return Some(t);
        }
        let width = t.width();
        let mut seen = std::collections::HashSet::with_capacity(t.len());
        let mut ids = Vec::with_capacity(t.ids.len());
        let mut ones = 0;
        for r in 0..t.len() {
            if seen.insert(t.row(r)) {
                ids.extend_from_slice(t.row(r));
                ones += 1;
            }
        }
        debug_assert_eq!(ids.len(), ones * width);
        Some(Table {
            vars: t.vars.clone(),
            ids,
            maybe: t.maybe.clone(),
            ones,
        })
    }

    /// `GRAPH name { inner }`: `inner` in the graph `name`, or for a variable in each graph
    /// that holds quads, its rows joined with the graph's name. `None` for a graph the store
    /// doesn't know, or more than [`MAX_GRAPHS`] graphs.
    fn named(&self, name: &NamedNodePattern, inner: &GraphPattern) -> Option<Table> {
        let within = |g: TermId| Eval {
            snapshot: self.snapshot,
            cancelled: self.cancelled,
            graph: GraphSelector::Graph(g),
        };
        let v = match name {
            NamedNodePattern::NamedNode(n) => {
                return within(self.snapshot.term_id(n.as_ref().into())?).eval(inner);
            }
            NamedNodePattern::Variable(v) => v,
        };
        let graphs = self.snapshot.graph_ids();
        if graphs.is_empty() || graphs.len() > MAX_GRAPHS {
            return None;
        }
        let mut all: Option<Table> = None;
        for g in graphs {
            let mut t = within(g).eval(inner)?;
            match t.col(v) {
                // The pattern binds the variable too: rows where it is the graph's name (an
                // unbound cell takes it).
                Some(c) => {
                    let width = t.width();
                    let mut ids = Vec::with_capacity(t.ids.len());
                    let mut ones = 0;
                    for r in 0..t.len() {
                        let row = t.row(r);
                        if row[c] == g || row[c] == UNBOUND {
                            let start = ids.len();
                            ids.extend_from_slice(row);
                            ids[start + c] = g;
                            ones += 1;
                        }
                    }
                    debug_assert_eq!(ids.len(), ones * width);
                    t.ids = ids;
                    t.ones = ones;
                    t.maybe[c] = false;
                }
                None => {
                    let width = t.width();
                    let mut ids = Vec::with_capacity(t.len() * (width + 1));
                    for r in 0..t.len() {
                        ids.extend_from_slice(t.row(r));
                        ids.push(g);
                    }
                    t.ids = ids;
                    t.vars.push(v.clone());
                    t.maybe.push(false);
                }
            }
            all = Some(match all {
                None => t,
                Some(a) => union(a, t)?,
            });
        }
        all
    }

    /// `?s <p>+ ?o`, `?s ^<p>+ ?o`, and with a constant end `<p>*` and `<p>?` too (#11): the pairs of
    /// the transitive closure of the predicate's edges, each end once per start (as SPARQL's
    /// paths give them). The starts are a constant end, else the values `seed` (the rows the
    /// path is joined with) gives one end, searched from that end, else every node with an
    /// edge. A few starts are searched by looking their edges up level by level; many, by
    /// reading all the predicate's edges once.
    fn path(
        &self,
        subject: &TermPattern,
        path: &PropertyPathExpression,
        object: &TermPattern,
        seed: Option<&Table>,
    ) -> Option<Table> {
        // `<p>?`: the starts themselves and their next nodes.
        let (steps, reverse, zero, one) = match closure_path(path) {
            Some((steps, reverse, zero)) => (steps, reverse, zero, false),
            None => {
                let (p, reverse) = optional_path(path)?;
                (vec![(p, false)], reverse, true, true)
            }
        };
        let end = |t: &TermPattern| match t {
            TermPattern::Variable(v) => Some(Err(v.clone())),
            TermPattern::NamedNode(n) => Some(Ok(self.snapshot.term_id(n.as_ref().into())?)),
            TermPattern::Literal(l) => Some(Ok(self.snapshot.term_id(l.as_ref().into())?)),
            _ => None,
        };
        // The direction the edges are followed in: from the subject, unless the subject is
        // the variable and the object the constant.
        let (mut from, mut to) = (end(subject)?, end(object)?);
        let mut forward = !reverse;
        if from.is_err() && to.is_ok() {
            std::mem::swap(&mut from, &mut to);
            forward = !forward;
        }
        let mut to = match (to, &from) {
            (Err(v), Err(u)) if v != *u => v,
            (Err(v), Ok(_)) => v,
            // The same variable at both ends, or both ends constant: the evaluator's.
            _ => return None,
        };
        // A seed that binds an end in every row gives the starts, from that end.
        let bound = |v: &Variable| {
            let t = seed?;
            let c = t.col(v).filter(|&c| !t.maybe[c])?;
            let mut values: Vec<TermId> = (0..t.len()).map(|r| t.row(r)[c]).collect();
            values.sort_unstable();
            values.dedup();
            Some(values)
        };
        let starts: Option<Vec<TermId>> = match &mut from {
            Ok(id) => Some(vec![*id]),
            Err(u) => match bound(u) {
                Some(values) => Some(values),
                None => match bound(&to) {
                    Some(values) => {
                        std::mem::swap(u, &mut to);
                        forward = !forward;
                        Some(values)
                    }
                    None => None,
                },
            },
        };
        // Which nodes a zero-length path matches in a named graph is the evaluator's to say.
        if zero && matches!(self.graph, GraphSelector::Graph(_)) {
            return None;
        }
        if zero && starts.is_none() {
            // A zero-length path between two free variables: every term of the graph.
            return None;
        }
        // A start only this query knows (a `VALUES` term the store doesn't hold) is no node
        // of the graph; whether a zero-length path matches it is the evaluator's to say.
        if zero
            && starts
                .as_ref()
                .is_some_and(|s| s.iter().any(|id| id.kind() == Kind::Ephemeral))
        {
            return None;
        }
        // The rows the closure may give: those the table it is joined into can hold (each
        // gives at least one joined row, with the seed's columns and the path's new end).
        let max_rows = max_ids() / (seed.map_or(1, |t| t.vars.len()) + 1);
        let mut ids = match &starts {
            Some(starts) if one => self.one_step(&steps, forward, starts, from.is_err())?,
            Some(starts) if (starts.len() as f64) * LEVEL_COST < self.edge_estimate(&steps) => {
                let budget = self.edge_estimate(&steps) / LEVEL_COST;
                match self.closure_by_levels(
                    &steps,
                    forward,
                    zero,
                    starts,
                    from.is_err(),
                    budget,
                )? {
                    Some(ids) => ids,
                    None => self.closure_of_all(
                        &steps,
                        forward,
                        zero,
                        Some(starts.clone()),
                        from.is_err(),
                        max_rows,
                    )?,
                }
            }
            _ => self.closure_of_all(
                &steps,
                forward,
                zero,
                starts.clone(),
                from.is_err(),
                max_rows,
            )?,
        };
        let seeded = starts.filter(|_| zero && from.is_err());
        // A value another pattern gives an end matches itself only if it is a node of the
        // graph read (a subject or object there), as in the evaluator; a constant end always
        // does. A start with another end is one; the others are looked up.
        if let Some(starts) = seeded {
            let is_node = |id: TermId| {
                self.snapshot.estimate_ids(Some(id), None, None, self.graph) > 0
                    || self.snapshot.estimate_ids(None, None, Some(id), self.graph) > 0
            };
            // The starts without another end that are no node: their own row goes.
            let mut gone: Vec<TermId> = Vec::new();
            if ids.chunks_exact(2).is_sorted_by_key(|p| p[0]) {
                // Each start's rows together (`<p>?` gives them so).
                let mut r = 0;
                while r < ids.len() {
                    let start = ids[r];
                    let mut other = false;
                    let mut e = r;
                    while e < ids.len() && ids[e] == start {
                        other |= ids[e + 1] != start;
                        e += 2;
                    }
                    if !other && !is_node(start) {
                        gone.push(start);
                    }
                    r = e;
                }
            } else {
                let mut other = vec![false; starts.len()];
                for p in ids.chunks_exact(2) {
                    if p[0] != p[1]
                        && let Ok(i) = starts.binary_search(&p[0])
                    {
                        other[i] = true;
                    }
                }
                gone.extend(
                    starts
                        .iter()
                        .zip(&other)
                        .filter(|&(&s, &o)| !o && !is_node(s))
                        .map(|(&s, _)| s),
                );
            }
            if !gone.is_empty() {
                gone.sort_unstable();
                let mut kept = Vec::with_capacity(ids.len());
                for p in ids.chunks_exact(2) {
                    if !(p[0] == p[1] && gone.binary_search(&p[0]).is_ok()) {
                        kept.extend_from_slice(p);
                    }
                }
                ids = kept;
            }
        }
        let vars = match from {
            Ok(_) => vec![to],
            Err(v) => vec![v, to],
        };
        Some(Table {
            ones: ids.len() / vars.len(),
            maybe: vec![false; vars.len()],
            vars,
            ids,
        })
    }

    fn edge_estimate(&self, steps: &Steps<'_>) -> f64 {
        steps
            .iter()
            .filter_map(|(p, _)| self.snapshot.term_id(p.as_ref().into()))
            .map(|p| self.snapshot.estimate_ids(None, Some(p), None, self.graph) as f64)
            .sum()
    }

    /// The edges of one step, followed `forward` (or backwards), looked up from `nodes` (sorted)
    /// or all read: (from, to) pairs, an edge possibly more than once, and looked-up edges
    /// may start elsewhere too. `None` past the memory limit.
    fn step_edges(
        &self,
        steps: &Steps<'_>,
        forward: bool,
        nodes: Option<&[TermId]>,
    ) -> Option<Vec<(TermId, TermId)>> {
        let mut pairs = Vec::new();
        for &(predicate, reverse) in steps {
            let (pattern, a, b) = Self::edge(predicate, forward != reverse);
            let edges = match nodes {
                Some(nodes) => {
                    let level = Table {
                        vars: vec![a.clone()],
                        ones: nodes.len(),
                        ids: nodes.to_vec(),
                        maybe: vec![false],
                    };
                    self.bgp_from(std::slice::from_ref(&pattern), Some(&level))?
                }
                None => self.bgp(std::slice::from_ref(&pattern))?,
            };
            let (ca, cb) = (edges.col(&a)?, edges.col(&b)?);
            pairs.extend((0..edges.len()).map(|r| {
                let row = edges.row(r);
                (row[ca], row[cb])
            }));
            if pairs.len().saturating_mul(4) > max_ids() {
                return None;
            }
        }
        Some(pairs)
    }

    /// The pattern of an edge followed from `path-a` to `path-b` (names no query can hold).
    fn edge(
        predicate: &spargebra::term::NamedNode,
        forward: bool,
    ) -> (spargebra::term::TriplePattern, Variable, Variable) {
        let (a, b) = (
            Variable::new_unchecked("path-a"),
            Variable::new_unchecked("path-b"),
        );
        let (s, o) = if forward { (&a, &b) } else { (&b, &a) };
        let t = spargebra::term::TriplePattern {
            subject: s.clone().into(),
            predicate: NamedNodePattern::NamedNode(predicate.clone()),
            object: o.clone().into(),
        };
        (t, a, b)
    }

    /// `<p>?` from `starts`: each start, and the nodes one edge away.
    /// Rows are (start, end), or the ends alone without `pairs`.
    fn one_step(
        &self,
        steps: &Steps<'_>,
        forward: bool,
        starts: &[TermId],
        pairs: bool,
    ) -> Option<Vec<TermId>> {
        let edges = self.step_edges(steps, forward, Some(starts))?;
        let width = usize::from(pairs) + 1;
        if (starts.len() + edges.len()).saturating_mul(width + 2) > max_ids() {
            return None;
        }
        // Each pair once: a start matches itself, and an edge may be read twice when the
        // default graph is the union of the graphs.
        // The edges read may start elsewhere too (the seed narrows the scan, it doesn't
        // filter): only those from a start.
        let mut found: Vec<(TermId, TermId)> = starts.iter().map(|&s| (s, s)).collect();
        found.extend(
            edges
                .into_iter()
                .filter(|(x, _)| starts.binary_search(x).is_ok()),
        );
        found.sort_unstable();
        found.dedup();
        let mut ids = Vec::with_capacity(found.len() * width);
        for (start, end) in found {
            if pairs {
                ids.push(start);
            }
            ids.push(end);
        }
        Some(ids)
    }

    /// The closure from a few starts: the edges of each level's nodes looked up by their
    /// values. Rows are (start, end), or the ends alone without `pairs`. `Some(None)` once
    /// more than `budget` nodes would have been looked up: reading all the edges once is then
    /// cheaper (a closure that reaches most of the graph).
    fn closure_by_levels(
        &self,
        steps: &Steps<'_>,
        forward: bool,
        zero: bool,
        starts: &[TermId],
        pairs: bool,
        budget: f64,
    ) -> Option<Option<Vec<TermId>>> {
        let mut looked_up = 0usize;
        let limit = max_ids();
        let mut ids = Vec::new();
        let mut seen: std::collections::HashSet<(TermId, TermId)> = Default::default();
        let emit = |start: TermId, end: TermId, ids: &mut Vec<TermId>| {
            if pairs {
                ids.push(start);
            }
            ids.push(end);
        };
        let mut frontier: Vec<(TermId, TermId)> = starts.iter().map(|&s| (s, s)).collect();
        if zero {
            for &s in starts {
                seen.insert((s, s));
                emit(s, s, &mut ids);
            }
        }
        // Each new pair takes its ids in the rows out, two in `seen` and two in `next`.
        let per_pair = usize::from(pairs) + 5;
        while !frontier.is_empty() {
            if (self.cancelled)() {
                return None;
            }
            let mut nodes: Vec<TermId> = frontier.iter().map(|&(_, n)| n).collect();
            nodes.sort_unstable();
            nodes.dedup();
            looked_up += nodes.len();
            if looked_up as f64 > budget {
                return Some(None);
            }
            let edges = self.step_edges(steps, forward, Some(&nodes))?;
            let mut next_of: HashMap<TermId, Vec<TermId>> = HashMap::new();
            for &(x, y) in &edges {
                next_of.entry(x).or_default().push(y);
            }
            // The most pairs this level can add, before any is made: past the limit, the
            // evaluator's (it streams them). The edges read and `next_of` hold four ids each.
            let most: usize = frontier
                .iter()
                .map(|(_, n)| next_of.get(n).map_or(0, Vec::len))
                .sum();
            let held = ids.len() + 2 * seen.len() + 4 * edges.len();
            if most.saturating_mul(per_pair).saturating_add(held) > limit {
                return None;
            }
            drop(edges);
            let mut next = Vec::new();
            for (i, (start, n)) in frontier.into_iter().enumerate() {
                if i % 4096 == 4095 && (self.cancelled)() {
                    return None;
                }
                for &m in next_of.get(&n).into_iter().flatten() {
                    if seen.insert((start, m)) {
                        emit(start, m, &mut ids);
                        next.push((start, m));
                    }
                }
            }
            frontier = next;
        }
        Some(Some(ids))
    }

    /// The closure from `starts`, or from every node with an edge, over all the predicate's
    /// edges read once. `None` past `max_rows` rows, or once the starts searched so far
    /// foretell that many (a closure reaching most of a large graph from many starts: the
    /// evaluator's, which streams its rows).
    fn closure_of_all(
        &self,
        steps: &Steps<'_>,
        forward: bool,
        zero: bool,
        starts: Option<Vec<TermId>>,
        pairs_out: bool,
        max_rows: usize,
    ) -> Option<Vec<TermId>> {
        let mut pairs = self.step_edges(steps, forward, None)?;
        pairs.sort_unstable();
        pairs.dedup();
        let next = |n: TermId| {
            let start = pairs.partition_point(|&(x, _)| x < n);
            pairs[start..]
                .iter()
                .take_while(move |&&(x, _)| x == n)
                .map(|&(_, y)| y)
        };
        let starts: Vec<TermId> = starts.unwrap_or_else(|| {
            let mut s: Vec<TermId> = pairs.iter().map(|&(x, _)| x).collect();
            s.dedup();
            s
        });
        let limit = max_ids().min(max_rows.saturating_mul(usize::from(pairs_out) + 1));
        let mut ids: Vec<TermId> = Vec::new();
        let mut seen: HashMap<TermId, usize> = HashMap::new();
        let mut stack: Vec<TermId> = Vec::new();
        for (i, &start) in starts.iter().enumerate() {
            if i % 4096 == 0 && (self.cancelled)() {
                return None;
            }
            // After a few starts, their rows on average for every start.
            if i >= 8 && (ids.len() / i).saturating_mul(starts.len()) > limit {
                return None;
            }
            let emit = |end: TermId, ids: &mut Vec<TermId>| {
                if pairs_out {
                    ids.push(start);
                }
                ids.push(end);
            };
            if zero {
                seen.insert(start, i);
                emit(start, &mut ids);
            }
            stack.clear();
            stack.push(start);
            while let Some(n) = stack.pop() {
                for m in next(n) {
                    if seen.insert(m, i) != Some(i) {
                        emit(m, &mut ids);
                        stack.push(m);
                        // A node with many edges: checked as its targets are added.
                        if ids.len() > limit {
                            return None;
                        }
                        if ids.len().is_multiple_of(65536) && (self.cancelled)() {
                            return None;
                        }
                    }
                }
            }
        }
        Some(ids)
    }

    /// The solutions of `p`, the right side of an operator over `t`'s rows: those of a basic
    /// graph pattern that share a value with a row of `t` only (a semi-join), so a few rows
    /// look its triples up by their values instead of scanning them.
    fn right(&self, t: &Table, p: &GraphPattern) -> Option<Table> {
        match p {
            GraphPattern::Bgp { patterns } => self.bgp_from(patterns, Some(t)),
            GraphPattern::Path {
                subject,
                path,
                object,
            } if let Some((left_out, reverse)) = negated_path(path) => {
                self.negated(subject, left_out, reverse, object, Some(t))
            }
            GraphPattern::Path {
                subject,
                path,
                object,
            } if alternative_path(path).is_none() => {
                let r = self.path(subject, path, object, Some(t))?;
                (r.ids.len() <= max_ids()).then_some(r)
            }
            _ => self.eval(p),
        }
    }

    /// The quads all the triple patterns of `p` match, each on its own: about what the
    /// evaluator reads for it.
    pub(crate) fn input_estimate(&self, p: &GraphPattern) -> f64 {
        if has_path(p) {
            return f64::INFINITY;
        }
        let mut total = 0.0;
        let mut add = |patterns: &[spargebra::term::TriplePattern]| {
            if let Some(compiled) = self.snapshot.compile_bgp(patterns, &[]) {
                total += compiled
                    .triples
                    .iter()
                    .map(|t| self.scan_estimate(std::slice::from_ref(t)))
                    .sum::<f64>();
            }
        };
        fn walk(p: &GraphPattern, add: &mut dyn FnMut(&[spargebra::term::TriplePattern])) {
            match p {
                GraphPattern::Bgp { patterns } => add(patterns),
                GraphPattern::Join { left, right }
                | GraphPattern::LeftJoin { left, right, .. }
                | GraphPattern::Minus { left, right }
                | GraphPattern::Union { left, right } => {
                    walk(left, add);
                    walk(right, add);
                }
                GraphPattern::Filter { expr, inner } => {
                    walk(inner, add);
                    if let Expression::Exists(e) = expr {
                        walk(e, add);
                    } else if let Expression::Not(e) = expr
                        && let Expression::Exists(e) = &**e
                    {
                        walk(e, add);
                    }
                }
                _ => {}
            }
        }
        walk(p, &mut add);
        total
    }

    /// The quads read by the cheapest scan of `triples`: no join of them reads fewer.
    fn scan_estimate(&self, triples: &[IdTriple]) -> f64 {
        let c = |s: IdSlot| match s {
            IdSlot::Const(id) => Some(id),
            IdSlot::Var(_) => None,
        };
        triples
            .iter()
            .map(|t| {
                self.snapshot
                    .estimate_ids(c(t.s), c(t.p), c(t.o), self.graph)
            })
            .min()
            .unwrap_or(0) as f64
    }

    fn bgp(&self, patterns: &[spargebra::term::TriplePattern]) -> Option<Table> {
        self.bgp_from(patterns, None)
    }

    fn bgp_from(
        &self,
        patterns: &[spargebra::term::TriplePattern],
        seed: Option<&Table>,
    ) -> Option<Table> {
        // Triples that read a result table of the spatial rewrite are answered by the snapshot
        // during evaluation, not from the quads; only one table's columns alone are read here,
        // the table whole.
        let table = reads_a_table(patterns);
        if table && seed.is_some() {
            return None;
        }
        let mut compiled = self.snapshot.compile_bgp(patterns, &[])?;
        if table
            && !self
                .snapshot
                .reads_table_whole(&compiled.triples, compiled.columns.len())
        {
            return None;
        }
        // The variables of the seed the pattern has, bound in every row of the seed: the first
        // columns of the pattern, given by the seed's distinct values.
        let mut seed_rows = None;
        if let Some(t) = seed
            && !compiled.empty
            && (t.len() as f64) * LOOKUP
                < self
                    .scan_estimate(&compiled.triples)
                    .max(self.snapshot.estimate_rows(&compiled.triples, self.graph))
        {
            let keys: Vec<(usize, Variable)> = t
                .vars
                .iter()
                .enumerate()
                .filter(|(c, v)| !t.maybe[*c] && compiled.columns.contains(&Some((*v).clone())))
                .map(|(c, v)| (c, v.clone()))
                .collect();
            if !keys.is_empty() {
                let first: Vec<Variable> = keys.iter().map(|(_, v)| v.clone()).collect();
                compiled = self.snapshot.compile_bgp(patterns, &first)?;
                let width = compiled.columns.len();
                let mut distinct: Vec<Vec<TermId>> = (0..t.len())
                    .map(|r| keys.iter().map(|&(c, _)| t.row(r)[c]).collect())
                    .collect();
                distinct.sort_unstable();
                distinct.dedup();
                let mut ids = Vec::with_capacity(distinct.len() * width);
                for k in distinct {
                    ids.extend_from_slice(&k);
                    ids.extend(std::iter::repeat_n(UNBOUND, width - k.len()));
                }
                seed_rows = Some((tripletrail_store::IdRows { width, ids }, keys.len()));
            }
        }
        let keep: Vec<usize> = (0..compiled.columns.len())
            .filter(|&c| compiled.columns[c].is_some())
            .collect();
        let vars: Vec<Variable> = keep
            .iter()
            .map(|&c| compiled.columns[c].clone().unwrap())
            .collect();
        let maybe = vec![false; vars.len()];
        if !compiled.empty && compiled.columns.is_empty() {
            // Triples without variables: one row without columns when they are all there.
            let n = self.snapshot.count_ids(
                &compiled.triples,
                0,
                self.graph,
                tripletrail_store::BgpCount::Rows,
                self.cancelled,
            )?;
            return Some(Table {
                vars,
                ids: Vec::new(),
                maybe,
                ones: usize::from(n > 0),
            });
        }
        if compiled.empty {
            return Some(Table {
                vars,
                ids: Vec::new(),
                maybe,
                ones: 0,
            });
        }
        let rows = match seed_rows {
            Some((seed, bound)) => self.snapshot.join_ids_from(
                &compiled.triples,
                seed,
                bound,
                self.graph,
                self.cancelled,
            )?,
            None => self.snapshot.join_ids(
                &compiled.triples,
                compiled.columns.len(),
                self.graph,
                self.cancelled,
            )?,
        };
        let n = rows.len();
        if n * vars.len() > max_ids() {
            return None;
        }
        let ids = if keep.len() == compiled.columns.len() {
            rows.ids
        } else {
            let mut ids = Vec::with_capacity(n * keep.len());
            for row in rows.rows() {
                ids.extend(keep.iter().map(|&c| row[c]));
            }
            ids
        };
        Some(Table {
            vars,
            ids,
            maybe,
            ones: n,
        })
    }

    fn filter(&self, expr: &Expression, t: Table) -> Option<Table> {
        match expr {
            Expression::Exists(p) => self.exists(&t, p, true),
            Expression::Not(e) if matches!(&**e, Expression::Exists(_)) => {
                let Expression::Exists(p) = &**e else {
                    unreachable!()
                };
                self.exists(&t, p, false)
            }
            Expression::And(a, b) if has_exists(a) || has_exists(b) => {
                let t = self.filter(a, t)?;
                self.filter(b, t)
            }
            _ => {
                // Large tables on several cores (#185), the chunks' rows back in order. Each
                // chunk builds its own kernel, so the terms it decodes (LANG, DATATYPE) are
                // cached without a lock.
                // A row the kernels give up on, or a filter they don't compute, is decided
                // with the evaluator's own code when the filter depends on the values alone
                // (`idstring::evaluated`).
                let evaluated = crate::idstring::evaluated(expr);
                let parts = tripletrail_store::par::chunks(t.len(), |range| {
                    let kernel = self.kernel(expr, &t, range.clone());
                    let other = if evaluated {
                        crate::idstring::evaluator_condition(expr, &t, self.snapshot)
                    } else {
                        None
                    };
                    if kernel.is_none() && other.is_none() {
                        return None;
                    }
                    let mut part = empty_like(&t);
                    for r in range {
                        let row = t.row(r);
                        let value = match kernel.as_ref().and_then(|k| k(row)) {
                            Some(value) => value,
                            None => other.as_ref()?(row)?,
                        };
                        // An error filters the row out, as false does.
                        if value == Some(true) {
                            part.push(row);
                        }
                    }
                    Some(part)
                });
                let mut parts = parts.into_iter();
                let mut out = parts.next()??;
                for part in parts {
                    let part = part?;
                    out.ids.extend_from_slice(&part.ids);
                    out.ones += part.ones;
                }
                Some(out)
            }
        }
    }

    /// `expr` as a test of a row of `t` on its ids: `Some(Some(b))` a value, `Some(None)` an
    /// error, `None` when the ids don't decide it (the whole filter then gives up).
    fn kernel<'k>(
        &'k self,
        expr: &Expression,
        t: &Table,
        rows: std::ops::Range<usize>,
    ) -> Option<Kernel<'k>> {
        use Expression as E;
        Some(match expr {
            // The effective boolean value of a column (the evaluator's
            // `effective_boolean_value`): a boolean, a number other than zero (and NaN), a
            // non-empty simple string; an error for any other term. A literal in the dictionary
            // whose value the evaluator reads (`"01"^^xsd:integer`) and the terms only this
            // query knows, other than strings and IRIs, are its to decide.
            E::Variable(v) => {
                let c = t.col(v);
                let truth = self.snapshot.term_id(
                    oxigraph::model::LiteralRef::new_typed_literal(
                        "true",
                        oxigraph::model::vocab::xsd::BOOLEAN,
                    )
                    .into(),
                );
                let empty = self
                    .snapshot
                    .term_id(oxigraph::model::LiteralRef::new_simple_literal("").into());
                let snapshot = self.snapshot;
                Box::new(move |row| {
                    let Some(id) = c.map(|c| row[c]).filter(|id| *id != UNBOUND) else {
                        return Some(None);
                    };
                    Some(match id.kind() {
                        Kind::Boolean => Some(id == truth?),
                        Kind::Integer => Some(id.integer_value()? != 0),
                        Kind::Double | Kind::Decimal => Some(match id.numeric()? {
                            tripletrail_store::Numeric::Decimal { digits, .. } => digits != 0,
                            tripletrail_store::Numeric::Double(d) => d != 0.0 && !d.is_nan(),
                        }),
                        Kind::String => Some(Some(id) != empty),
                        // A string or IRI the term kernels made for this query.
                        Kind::Ephemeral => match snapshot.term(id).ok()? {
                            oxigraph::model::Term::Literal(l)
                                if l.datatype() == oxigraph::model::vocab::xsd::STRING =>
                            {
                                Some(!l.value().is_empty())
                            }
                            oxigraph::model::Term::Literal(l) if l.language().is_none() => {
                                return None;
                            }
                            _ => None,
                        },
                        Kind::Iri
                        | Kind::Blank
                        | Kind::LangString
                        | Kind::DateTime
                        | Kind::Geometry => None,
                        _ => return None,
                    })
                })
            }
            E::Bound(v) => match t.col(v) {
                Some(c) => Box::new(move |row| Some(Some(row[c] != UNBOUND))),
                None => Box::new(|_| Some(Some(false))),
            },
            E::Not(a) => {
                let a = self.kernel(a, t, rows)?;
                Box::new(move |row| Some(a(row)?.map(|b| !b)))
            }
            // Three-valued logic: an error and false is false, an error or true is true.
            E::And(a, b) => {
                let (a, b) = (self.kernel(a, t, rows.clone())?, self.kernel(b, t, rows)?);
                Box::new(move |row| {
                    Some(match (a(row)?, b(row)?) {
                        (Some(false), _) | (_, Some(false)) => Some(false),
                        (Some(true), Some(true)) => Some(true),
                        _ => None,
                    })
                })
            }
            E::Or(a, b) => {
                let (a, b) = (self.kernel(a, t, rows.clone())?, self.kernel(b, t, rows)?);
                Box::new(move |row| {
                    Some(match (a(row)?, b(row)?) {
                        (Some(true), _) | (_, Some(true)) => Some(true),
                        (Some(false), Some(false)) => Some(false),
                        _ => None,
                    })
                })
            }
            E::SameTerm(a, b) => {
                let (a, b) = (self.operand(a, t)?, self.operand(b, t)?);
                Box::new(move |row| match (a.get(row), b.get(row)) {
                    (Some(x), Some(y)) => Some(Some(x == y)),
                    _ => Some(None),
                })
            }
            E::Greater(..) | E::GreaterOrEqual(..) | E::Less(..) | E::LessOrEqual(..) => {
                crate::idvalue::comparison(expr, t, self.snapshot)?
            }
            E::Equal(a, b)
                if value_expression(a) && value_expression(b) && (computed(a) || computed(b)) =>
            {
                crate::idvalue::comparison(expr, t, self.snapshot)?
            }
            E::Equal(a, b) => {
                if let Some(k) = self
                    .lang_equal(a, b, t, &rows)
                    .or_else(|| self.lang_equal(b, a, t, &rows))
                {
                    return Some(k);
                }
                if let Some(k) = self
                    .datatype_equal(a, b, t)
                    .or_else(|| self.datatype_equal(b, a, t))
                {
                    return Some(k);
                }
                if let Some(k) = self.str_equal(a, b, t).or_else(|| self.str_equal(b, a, t)) {
                    return Some(k);
                }
                if let Some((e, text)) = string_equal(a, b).or_else(|| string_equal(b, a)) {
                    return self.computed_string_equal(e, text, t);
                }
                let (a, b) = (self.operand(a, t)?, self.operand(b, t)?);
                let decoded = Decoded::new(self.snapshot);
                Box::new(move |row| match (a.get(row), b.get(row)) {
                    (Some(x), Some(y)) => equal_ids(x, y, &decoded),
                    _ => Some(None),
                })
            }
            // A list of constants (#63): their ids in a hash set, so a row costs one lookup
            // and not one comparison per item. What the set doesn't decide is `=` as above: a
            // term that is no literal only equals itself, and a literal is compared with the
            // literal items whose values may equal it under another id (`1` and `1.0`).
            E::In(a, list)
                if matches!(**a, E::Variable(_))
                    && !list.is_empty()
                    && list
                        .iter()
                        .all(|b| matches!(b, E::NamedNode(_) | E::Literal(_))) =>
            {
                let E::Variable(v) = &**a else { unreachable!() };
                let c = t.col(v);
                let mix = |id: TermId| id.0.wrapping_mul(0x9E37_79B9_7F4A_7C15);
                let mut set: PreMap<()> = PreMap::default();
                let mut literals = Vec::new();
                for b in list {
                    // An IRI not in the store equals no value of it; a literal without an id
                    // leaves the filter to the evaluator, as `=` does.
                    let Operand::Const(id) = self.operand(b, t)? else {
                        return None;
                    };
                    if id == ABSENT {
                        continue;
                    }
                    // Each literal once: the set says whether it was there already.
                    if set.insert(mix(id), ()).is_none() && is_literal(id) {
                        literals.push(id);
                    }
                }
                // Per kind whose values are its ids, the literal items of the other kinds: an
                // item of the same kind and another id is another value.
                let others = |k: Kind| -> Vec<TermId> {
                    literals.iter().copied().filter(|y| y.kind() != k).collect()
                };
                let (strings, integers, booleans) = (
                    others(Kind::String),
                    others(Kind::Integer),
                    others(Kind::Boolean),
                );
                let decoded = Decoded::new(self.snapshot);
                Box::new(move |row| {
                    let Some(x) = c.map(|c| row[c]).filter(|id| *id != UNBOUND) else {
                        return Some(None);
                    };
                    // An item of the same id: equal, unless the term is NaN, which equals
                    // nothing (and then no other item either).
                    if set.contains_key(&mix(x)) {
                        return Some(Some(reflexive(x, &decoded)?));
                    }
                    // A term only this query knows (a `BIND` constant the store doesn't hold,
                    // `2.5e0`) may equal an item by value: the evaluator decides.
                    if x.kind() == Kind::Ephemeral {
                        return None;
                    }
                    if !is_literal(x) {
                        // Items are IRIs and literals, never triple terms.
                        return Some(Some(false));
                    }
                    let rest = match x.kind() {
                        Kind::String => &strings,
                        Kind::Integer => &integers,
                        Kind::Boolean => &booleans,
                        _ => &literals,
                    };
                    // `a = b || a = c`: true wins over an error, an error over false.
                    let mut error = false;
                    for &y in rest {
                        match equal_ids(x, y, &decoded)? {
                            Some(true) => return Some(Some(true)),
                            Some(false) => {}
                            None => error = true,
                        }
                    }
                    Some((!error).then_some(false))
                })
            }
            // `a IN (b, c)` is `a = b || a = c` (and false for an empty list).
            E::In(a, list) => match list
                .iter()
                .map(|b| E::Equal(a.clone(), Box::new(b.clone())))
                .reduce(|x, y| E::Or(Box::new(x), Box::new(y)))
            {
                Some(e) => self.kernel(&e, t, rows)?,
                None => Box::new(|_| Some(Some(false))),
            },
            E::FunctionCall(
                f @ (Function::IsLiteral | Function::IsIri | Function::IsBlank),
                args,
            ) => {
                let [E::Variable(v)] = args.as_slice() else {
                    return None;
                };
                let c = t.col(v);
                let f = f.clone();
                Box::new(move |row| {
                    let Some(id) = c.map(|c| row[c]).filter(|id| *id != UNBOUND) else {
                        return Some(None);
                    };
                    if id.kind() == Kind::Ephemeral {
                        return None;
                    }
                    Some(Some(match f {
                        Function::IsLiteral => is_literal(id),
                        Function::IsIri => id.kind() == Kind::Iri,
                        _ => id.kind() == Kind::Blank,
                    }))
                })
            }
            E::FunctionCall(Function::IsNumeric, args) => {
                let [E::Variable(v)] = args.as_slice() else {
                    return None;
                };
                let c = t.col(v);
                let cache = Decoded::new(self.snapshot);
                Box::new(move |row| {
                    let Some(id) = c.map(|c| row[c]).filter(|id| *id != UNBOUND) else {
                        return Some(None);
                    };
                    Some(Some(match id.kind() {
                        Kind::Integer | Kind::Double | Kind::Decimal => true,
                        Kind::Typed => cache.numeric(id)?,
                        Kind::Ephemeral => return None,
                        _ => false,
                    }))
                })
            }
            E::FunctionCall(
                Function::Regex | Function::StrStarts | Function::Contains | Function::StrEnds,
                _,
            ) if string_prefix(expr).is_none() && text_test(expr).is_some() => {
                // The value of each stored string read from its dictionary key, not decoded,
                // under one lock of the dictionary per chunk that is released before any row is
                // tested, as for `LANGMATCHES` (#654). Each row's string is tested (#165): the
                // labels of a dataset are mostly distinct.
                let (v, test) = text_test(expr)?;
                let c = t.col(v);
                let snapshot = self.snapshot;
                let mix = |id: TermId| id.0.wrapping_mul(0x9E37_79B9_7F4A_7C15);
                let mut known: PreMap<bool> = PreMap::default();
                if let Some(c) = c {
                    let reader = snapshot.reader();
                    for r in rows.clone() {
                        let id = t.row(r)[c];
                        if let Some(text) = reader.string_value(id) {
                            known.insert(mix(id), test.matches(text));
                        }
                    }
                }
                Box::new(move |row| {
                    if let Some(is) = c.and_then(|c| known.get(&mix(row[c]))) {
                        return Some(Some(*is));
                    }
                    let Some(id) = c.map(|c| row[c]).filter(|id| *id != UNBOUND) else {
                        return Some(None);
                    };
                    match id.kind() {
                        Kind::String | Kind::LangString => match snapshot.term(id).ok()? {
                            oxigraph::model::Term::Literal(l) => {
                                Some(Some(test.matches(l.value())))
                            }
                            _ => None,
                        },
                        // Terms only this query knows, rows and columns: the evaluator decides.
                        Kind::Ephemeral | Kind::Row | Kind::Column | Kind::DefaultGraph => None,
                        // Not a string literal: an error.
                        _ => Some(None),
                    }
                })
            }
            // Of a string the term kernels compute (`STR(?x)`, `LCASE(?l)`): its text tested,
            // once per distinct value of the column it reads.
            E::FunctionCall(
                Function::Regex | Function::StrStarts | Function::Contains | Function::StrEnds,
                _,
            ) if text_test_on(expr).is_some_and(|(a, _)| !matches!(a, E::Variable(_))) => {
                let (a, test) = text_test_on(expr)?;
                self.computed_string_test(a, t, move |term| match term {
                    oxigraph::model::Term::Literal(l)
                        if l.datatype() == oxigraph::model::vocab::xsd::STRING
                            || l.language().is_some() =>
                    {
                        Some(Some(test.matches(l.value())))
                    }
                    // Not a string literal: an error.
                    _ => Some(None),
                })?
            }
            E::FunctionCall(Function::Regex | Function::StrStarts, _) => {
                let (v, prefix) = string_prefix(expr)?;
                let c = t.col(v);
                // The strings of the store whose text starts with the prefix are a range of
                // ranks (for language strings, of their values); strings added since the ranks
                // were built, and directional strings, are read from the dictionary.
                let ranks = self.snapshot.string_ranks();
                let range = self.snapshot.string_prefix(&ranks, Kind::String, &prefix);
                let lang_range = self
                    .snapshot
                    .string_prefix(&ranks, Kind::LangString, &prefix);
                let snapshot = self.snapshot;
                Box::new(move |row| {
                    let Some(id) = c.map(|c| row[c]).filter(|id| *id != UNBOUND) else {
                        return Some(None);
                    };
                    let text = |id: TermId| match snapshot.term(id).ok()? {
                        oxigraph::model::Term::Literal(l) => {
                            Some(Some(l.value().starts_with(prefix.as_str())))
                        }
                        _ => None,
                    };
                    match id.kind() {
                        Kind::String => match ranks.rank(id) {
                            Some(r) => Some(Some(range.contains(&r))),
                            None => text(id),
                        },
                        Kind::LangString => match ranks.rank(id) {
                            Some(r) => Some(Some(lang_range.contains(&r))),
                            None => text(id),
                        },
                        // Not a string: an error.
                        _ => Some(None),
                    }
                })
            }
            E::FunctionCall(Function::LangMatches, args) => {
                let [E::FunctionCall(Function::Lang, inner), E::Literal(range)] = args.as_slice()
                else {
                    return None;
                };
                let [E::Variable(v)] = inner.as_slice() else {
                    return None;
                };
                if !is_simple(range) {
                    return None;
                }
                let range = range.value().to_ascii_lowercase();
                let c = t.col(v);
                let cache = Decoded::new(self.snapshot);
                Box::new(move |row| {
                    let lang = match cache.language(c.map(|c| row[c]))? {
                        Some(l) => l,
                        None => return Some(None),
                    };
                    Some(Some(lang_matches(&lang, &range)))
                })
            }
            // The effective boolean value of a number the value kernels compute.
            e if !matches!(e, E::Variable(_) | E::Literal(_))
                && crate::direct::number_computable(e) =>
            {
                use crate::idvalue::Value;
                let k = crate::idvalue::compile(e, t, self.snapshot)?;
                Box::new(move |row| {
                    Some(match k(row)? {
                        Ok(Value::Integer(i)) => Some(i != 0),
                        Ok(Value::Decimal(d)) => Some(d != 0),
                        Ok(Value::Double(d)) => Some(d != 0.0 && !d.is_nan()),
                        _ => None,
                    })
                })
            }
            // The effective boolean value of a term the term kernels compute.
            e if !matches!(e, E::Variable(_)) && crate::idstring::computable(e) => {
                self.computed_string_test(e, t, effective_boolean_value)?
            }
            _ => return None,
        })
    }
    /// The rows of `t` with (`keep`) or without a solution of `p` compatible with them.
    fn exists(&self, t: &Table, p: &GraphPattern, keep: bool) -> Option<Table> {
        // Substituting a row's values into a basic graph pattern is a join with it. So it is
        // for `FILTER`s and nested `EXISTS` inside (#11), when everything they read that the
        // row binds is bound by the pattern itself: substituted or joined, they then see the
        // same values. A filter reading a value only the row gives is left to the evaluator.
        // Each filter sees only the rows of its own group, so the check is per filter (#208).
        if !filters_see_their_own(t, p) {
            // `EXISTS { r FILTER(e) }` where `e` reads a value only the row gives (`?a > ?q`):
            // `e` on the rows of `t` joined with those of `r`, as the substituted row sees
            // them (#11).
            return match p {
                GraphPattern::Filter { expr, inner } if filters_see_their_own(t, inner) => {
                    let inner = self.exists_rows(t, inner)?;
                    self.exists_filtered(t, &inner, expr, keep)
                }
                _ => None,
            };
        }
        let inner = self.exists_rows(t, p)?;
        let keys = shared(t, &inner)?;
        semi(t, &inner, &keys, keep, self.cancelled)
    }

    /// The rows of an `EXISTS` pattern ([`exists_shape`]), its triples looked up from the
    /// values `t` gives them.
    fn exists_rows(&self, t: &Table, p: &GraphPattern) -> Option<Table> {
        match p {
            GraphPattern::Bgp { .. } => self.right(t, p),
            GraphPattern::Filter { expr, inner } => {
                let inner = self.exists_rows(t, inner)?;
                self.filter(expr, inner)
            }
            _ => self.eval(p),
        }
    }
}

impl<'a> Eval<'a> {
    /// `OPTIONAL { r FILTER(e) }` after `l` where `e` reads values of `l` too (#11): `e` on the
    /// rows of `l` joined with `r`, then each row of `l` with its joined rows that pass, or
    /// alone (the columns of `r` unbound) when none does.
    fn left_join_filtered(&self, l: &Table, r: &Table, e: &Expression) -> Option<Table> {
        // `l` with its row numbers (from 1) in a column of its own, which `e` cannot read.
        let width = l.width();
        let mut numbered = Table {
            vars: l.vars.clone(),
            ids: Vec::with_capacity(l.len() * (width + 1)),
            maybe: l.maybe.clone(),
            ones: l.len(),
        };
        numbered.vars.push(Variable::new_unchecked(" row"));
        numbered.maybe.push(false);
        for i in 0..l.len() {
            numbered.ids.extend_from_slice(l.row(i));
            numbered.ids.push(TermId(i as u64 + 1));
        }
        let joined = self.filter(e, join(&numbered, r, self.cancelled)?)?;
        // The joined rows come in the order of the rows of `l`, as `left_join` gives them.
        let mut vars = l.vars.clone();
        let mut maybe = l.maybe.clone();
        for v in &joined.vars[width + 1..] {
            vars.push(v.clone());
            maybe.push(true);
        }
        let mut out = Table {
            vars,
            ids: Vec::new(),
            maybe,
            ones: 0,
        };
        let extra = joined.width() - width - 1;
        let mut next = 0;
        for i in 0..l.len() {
            if i % 4096 == 0 && (self.cancelled)() {
                return None;
            }
            let number = TermId(i as u64 + 1);
            let mut matched = false;
            while next < joined.len() && joined.row(next)[width] == number {
                let row = joined.row(next);
                out.ids.extend_from_slice(&row[..width]);
                out.ids.extend_from_slice(&row[width + 1..]);
                out.ones += 1;
                matched = true;
                next += 1;
            }
            if !matched {
                out.ids.extend_from_slice(l.row(i));
                out.ids.extend(std::iter::repeat_n(UNBOUND, extra));
                out.ones += 1;
            }
        }
        // Not in the order of `l` after all: give up rather than lose rows.
        (next == joined.len()).then_some(out)
    }

    /// The rows of `t` for which some row of `r` joined with it passes `e` (`keep`), or for
    /// which none does (`!keep`): `FILTER EXISTS { r FILTER(e) }` and its `NOT`, where `e`
    /// reads values of `t` too. Joined, `e` sees the row's values and the pattern's, as it
    /// does when the row is substituted into the pattern; a variable the row leaves unbound
    /// is the pattern's to bind, or unbound for both.
    fn exists_filtered(&self, t: &Table, r: &Table, e: &Expression, keep: bool) -> Option<Table> {
        // Without a key both are bound by, every row would pair with every row of `r`.
        if !shared(t, r).is_some_and(|keys| !keys.is_empty()) {
            return None;
        }
        let width = t.width();
        let mut numbered = Table {
            vars: t.vars.clone(),
            ids: Vec::with_capacity(t.len() * (width + 1)),
            maybe: t.maybe.clone(),
            ones: t.len(),
        };
        numbered.vars.push(Variable::new_unchecked(" row"));
        numbered.maybe.push(false);
        for i in 0..t.len() {
            numbered.ids.extend_from_slice(t.row(i));
            numbered.ids.push(TermId(i as u64 + 1));
        }
        let joined = self.filter(e, join(&numbered, r, self.cancelled)?)?;
        let number = joined.col(&Variable::new_unchecked(" row"))?;
        let mut passed = vec![false; t.len()];
        for j in 0..joined.len() {
            passed[(joined.row(j)[number].0 - 1) as usize] = true;
        }
        let mut out = empty_like(t);
        for (i, passed) in passed.into_iter().enumerate() {
            if i % 4096 == 0 && (self.cancelled)() {
                return None;
            }
            if passed == keep {
                out.push(t.row(i));
            }
        }
        Some(out)
    }

    /// A variable of `t` or a constant, as a row's id (`None`: unbound).
    fn operand(&self, e: &Expression, t: &Table) -> Option<Operand> {
        Some(match e {
            Expression::Variable(v) => match t.col(v) {
                Some(c) => Operand::Column(c),
                None => Operand::Unbound,
            },
            // An IRI not in the store equals no value of it.
            Expression::NamedNode(n) => {
                Operand::Const(self.snapshot.term_id(n.as_ref().into()).unwrap_or(ABSENT))
            }
            Expression::Literal(l) => Operand::Const(self.snapshot.term_id(l.as_ref().into())?),
            _ => return None,
        })
    }

    /// `LANG(?v) = "tag"`.
    fn lang_equal<'k>(
        &'k self,
        a: &Expression,
        b: &Expression,
        t: &Table,
        rows: &std::ops::Range<usize>,
    ) -> Option<Kernel<'k>> {
        let (Expression::FunctionCall(Function::Lang, args), Expression::Literal(tag)) = (a, b)
        else {
            return None;
        };
        let [Expression::Variable(v)] = args.as_slice() else {
            return None;
        };
        if !is_simple(tag) {
            return None;
        }
        let tag = tag.value().to_owned();
        let c = t.col(v);
        let cache = Decoded::new(self.snapshot);
        // The stored language strings of the kernel's rows (a chunk of a filter): their tags
        // read from the dictionary keys, not decoded, under one lock of the dictionary that is
        // released before any row is tested. Held while rows are tested, it would deadlock
        // with a waiting writer as soon as another kernel of the filter took it again (#654).
        let mix = |id: TermId| id.0.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut known: PreMap<bool> = PreMap::default();
        if let Some(c) = c {
            let reader = self.snapshot.reader();
            for r in rows.clone() {
                let id = t.row(r)[c];
                if let Some(is) = reader.language_is(id, &tag) {
                    known.insert(mix(id), is);
                }
            }
        }
        Some(Box::new(move |row| {
            let id = c.map(|c| row[c]);
            if let Some(is) = id.and_then(|id| known.get(&mix(id))) {
                return Some(Some(*is));
            }
            Some(cache.language(id)?.map(|l| l == tag))
        }))
    }

    /// `STR(?v) = "text"` (#657): an `xsd:string` or an IRI matches when it is that term,
    /// other literals by their lexical form, decoded once per id.
    /// `e = "text"` for a string the term kernels compute: the evaluator's `=` of a simple
    /// literal (true for the same text, false for an IRI, a blank node or a language-tagged
    /// string); a literal of another type is the evaluator's.
    fn computed_string_equal<'k>(
        &'k self,
        e: &Expression,
        text: &str,
        t: &Table,
    ) -> Option<Kernel<'k>> {
        let text = text.to_owned();
        self.computed_string_test(e, t, move |term| match term {
            oxigraph::model::Term::Literal(l)
                if l.datatype() == oxigraph::model::vocab::xsd::STRING =>
            {
                Some(Some(l.value() == text))
            }
            oxigraph::model::Term::Literal(l) if l.language().is_some() => Some(Some(false)),
            oxigraph::model::Term::Literal(_) => None,
            _ => Some(Some(false)),
        })
    }

    /// A test of the term [`crate::idstring::compile`] computes for `e` (an error is an
    /// error), once per distinct id of the one column `e` reads.
    fn computed_string_test<'k>(
        &'k self,
        e: &Expression,
        t: &Table,
        test: impl Fn(oxigraph::model::Term) -> Option<Option<bool>> + 'k,
    ) -> Option<Kernel<'k>> {
        if !crate::idstring::computable(e) {
            return None;
        }
        let kernel = crate::idstring::compile(e, t, self.snapshot)?;
        let mut names = std::collections::HashSet::new();
        crate::plan::expression_variables(e, &mut names)?;
        let cols: Vec<usize> = names
            .iter()
            .filter_map(|n| t.vars.iter().position(|v| v.as_str() == n))
            .collect();
        let column = match cols[..] {
            [c] => Some(c),
            _ => None,
        };
        let seen: std::cell::RefCell<HashMap<TermId, Option<bool>>> = Default::default();
        Some(Box::new(move |row| {
            let id = column.map(|c| row[c]);
            if let Some(is) = id.and_then(|id| seen.borrow().get(&id).copied()) {
                return Some(is);
            }
            let is = match kernel(row)? {
                Ok(term) => test(term)?,
                Err(()) => None,
            };
            if let Some(id) = id {
                seen.borrow_mut().insert(id, is);
            }
            Some(is)
        }))
    }

    fn str_equal<'k>(&'k self, a: &Expression, b: &Expression, t: &Table) -> Option<Kernel<'k>> {
        let (Expression::FunctionCall(Function::Str, args), Expression::Literal(text)) = (a, b)
        else {
            return None;
        };
        let [Expression::Variable(v)] = args.as_slice() else {
            return None;
        };
        if !is_simple(text) {
            return None;
        }
        let c = t.col(v);
        let string = self
            .snapshot
            .term_id(text.as_ref().into())
            .unwrap_or(ABSENT);
        let iri = oxigraph::model::NamedNode::new(text.value())
            .ok()
            .and_then(|n| self.snapshot.term_id(n.as_ref().into()))
            .unwrap_or(ABSENT);
        let text = text.value().to_owned();
        let cache = Decoded::new(self.snapshot);
        let seen: std::cell::RefCell<HashMap<TermId, bool>> = Default::default();
        Some(Box::new(move |row| {
            let Some(id) = c.map(|c| row[c]).filter(|id| *id != UNBOUND) else {
                return Some(None);
            };
            match id.kind() {
                Kind::String => Some(Some(id == string)),
                Kind::Iri => Some(Some(id == iri)),
                Kind::Blank => Some(None),
                Kind::LangString | Kind::Typed | Kind::Geometry | Kind::Integer | Kind::Boolean => {
                    if let Some(is) = seen.borrow().get(&id) {
                        return Some(Some(*is));
                    }
                    let is = cache.literal(id)?.value() == text;
                    seen.borrow_mut().insert(id, is);
                    Some(Some(is))
                }
                // Triple terms, query-local terms: the evaluator decides.
                _ => None,
            }
        }))
    }

    /// `DATATYPE(?v) = <iri>`.
    fn datatype_equal<'k>(
        &'k self,
        a: &Expression,
        b: &Expression,
        t: &Table,
    ) -> Option<Kernel<'k>> {
        let (Expression::FunctionCall(Function::Datatype, args), Expression::NamedNode(iri)) =
            (a, b)
        else {
            return None;
        };
        let [Expression::Variable(v)] = args.as_slice() else {
            return None;
        };
        let iri = iri.clone();
        let c = t.col(v);
        let cache = Decoded::new(self.snapshot);
        Some(Box::new(move |row| {
            Some(cache.datatype(c.map(|c| row[c]))?.map(|d| d == iri))
        }))
    }
}

/// A test of a row on its ids (see `Eval::kernel`).
pub(crate) type Kernel<'k> = Box<dyn Fn(&[TermId]) -> Option<Option<bool>> + 'k>;

/// An id no term has: an IRI constant that is not in the store.
const ABSENT: TermId = TermId(u64::MAX);

#[derive(Clone, Copy)]
enum Operand {
    Column(usize),
    Const(TermId),
    Unbound,
}

impl Operand {
    fn get(self, row: &[TermId]) -> Option<TermId> {
        match self {
            Operand::Column(c) => Some(row[c]).filter(|id| *id != UNBOUND),
            Operand::Const(id) => Some(id),
            Operand::Unbound => None,
        }
    }
}

/// Terms decoded once per id, for the tests the id's kind doesn't answer.
struct Decoded<'a> {
    snapshot: &'a Snapshot,
    terms: std::cell::RefCell<HashMap<TermId, oxigraph::model::Term>>,
}

impl<'a> Decoded<'a> {
    fn new(snapshot: &'a Snapshot) -> Self {
        Self {
            snapshot,
            terms: Default::default(),
        }
    }

    /// The term of an id (`None`: it can't be decoded).
    fn term(&self, id: TermId) -> Option<oxigraph::model::Term> {
        let mut terms = self.terms.borrow_mut();
        let term = match terms.entry(id) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => e.insert(self.snapshot.term(id).ok()?),
        };
        Some(term.clone())
    }

    /// The literal of a dictionary id (`None`: it can't be decoded).
    fn literal(&self, id: TermId) -> Option<oxigraph::model::Literal> {
        match self.term(id)? {
            oxigraph::model::Term::Literal(l) => Some(l),
            _ => None,
        }
    }

    /// `LANG(?v)`: `Some(None)` an error (unbound, not a literal).
    fn language(&self, id: Option<TermId>) -> Option<Option<String>> {
        let Some(id) = id.filter(|id| *id != UNBOUND) else {
            return Some(None);
        };
        Some(match id.kind() {
            Kind::Ephemeral => return None,
            Kind::LangString => Some(self.literal(id)?.language()?.to_owned()),
            _ if is_literal(id) => Some(String::new()),
            _ => None,
        })
    }

    /// `DATATYPE(?v)`: `Some(None)` an error (unbound, not a literal).
    fn datatype(&self, id: Option<TermId>) -> Option<Option<oxigraph::model::NamedNode>> {
        use oxigraph::model::vocab::{rdf, xsd};
        let Some(id) = id.filter(|id| *id != UNBOUND) else {
            return Some(None);
        };
        Some(match id.kind() {
            Kind::Ephemeral => return None,
            Kind::String => Some(xsd::STRING.into_owned()),
            Kind::LangString => Some(rdf::LANG_STRING.into_owned()),
            Kind::Integer => Some(xsd::INTEGER.into_owned()),
            Kind::Boolean => Some(xsd::BOOLEAN.into_owned()),
            Kind::Double => Some(xsd::DOUBLE.into_owned()),
            Kind::Decimal => Some(xsd::DECIMAL.into_owned()),
            Kind::DateTime => Some(match id.date_time_value()?.date {
                true => xsd::DATE.into_owned(),
                false => xsd::DATE_TIME.into_owned(),
            }),
            // See `numeric`: the evaluator decides the datatype of numeric literals.
            Kind::Typed | Kind::Geometry => {
                let l = self.literal(id)?;
                if is_numeric_datatype(l.datatype()) {
                    return None;
                }
                Some(l.datatype().into_owned())
            }
            _ => None,
        })
    }

    /// `isNumeric` of a typed literal (not an inlined integer or boolean); `None` (give up)
    /// for a numeric datatype. The evaluator reads such a literal as a number when its
    /// lexical form parses (ignoring the derived types' ranges: `"300"^^xsd:byte` is numeric,
    /// with datatype `xsd:integer`), and as an other literal when it doesn't
    /// (`"abc"^^xsd:integer`): that is its rule to apply.
    fn numeric(&self, id: TermId) -> Option<bool> {
        let l = self.literal(id)?;
        (!is_numeric_datatype(l.datatype())).then_some(false)
    }
}

/// XSD numeric datatypes, the derived integer types included.
pub(crate) fn is_numeric_datatype(d: oxigraph::model::NamedNodeRef<'_>) -> bool {
    d.as_str()
        .strip_prefix("http://www.w3.org/2001/XMLSchema#")
        .is_some_and(|local| {
            matches!(
                local,
                "integer"
                    | "decimal"
                    | "float"
                    | "double"
                    | "long"
                    | "int"
                    | "short"
                    | "byte"
                    | "nonNegativeInteger"
                    | "nonPositiveInteger"
                    | "negativeInteger"
                    | "positiveInteger"
                    | "unsignedLong"
                    | "unsignedInt"
                    | "unsignedShort"
                    | "unsignedByte"
            )
        })
}

/// Literals of these kinds are equal values only when their ids are equal.
/// `x = y` on two bound ids: `Some(Some(b))` a value, `Some(None)` an error, `None` when the
/// ids don't decide it.
fn equal_ids(x: TermId, y: TermId, decoded: &Decoded) -> Option<Option<bool>> {
    // An IRI the store doesn't hold equals no term it holds; two of them, the ids don't tell
    // apart. (Its id is no kind of term: never read as one.)
    if x == ABSENT || y == ABSENT {
        return (x != y).then_some(Some(false));
    }
    // The same term: equal, unless it is NaN.
    if x == y {
        return Some(Some(reflexive(x, decoded)?));
    }
    // Two different triple terms are equal when their parts are, and a literal part compares
    // by value (`<<( :a :b 1 )>> = <<( :a :b 1.0 )>>`): the ids don't decide it.
    if x.kind() == Kind::Triple && y.kind() == Kind::Triple {
        return None;
    }
    // A term only this query knows (a `BIND` constant the store doesn't hold, `2.5e0`) may
    // be a literal equal by value to another: the ids don't decide it.
    if x.kind() == Kind::Ephemeral || y.kind() == Kind::Ephemeral {
        return None;
    }
    match (is_literal(x), is_literal(y)) {
        // Different literals may be equal values (1 and 1.0): not on ids, except for kinds
        // whose values are their ids, and inline numbers and dates, compared by value.
        (true, true) if same_value_kind(x, y) => Some(Some(false)),
        (true, true) => {
            let (x, y) = (crate::idvalue::of_id(x)?, crate::idvalue::of_id(y)?);
            Some(crate::idvalue::compare(x, y)?.map(|o| o.is_eq()))
        }
        _ => Some(Some(false)),
    }
}

/// Whether a term equals itself under `=`: every term but a `NaN` double or float, which
/// equals nothing, itself included. `None` for a triple term, whose parts may hold one: the
/// ids don't decide it.
fn reflexive(id: TermId, decoded: &Decoded) -> Option<bool> {
    use oxigraph::model::vocab::xsd;
    match id.kind() {
        Kind::Double => Some(!matches!(
            id.numeric()?,
            tripletrail_store::Numeric::Double(d) if d.is_nan()
        )),
        Kind::Triple => None,
        // A literal in the dictionary or one only this query knows: a float or double in
        // any form the store keeps.
        Kind::Typed | Kind::Ephemeral => Some(match decoded.term(id)? {
            oxigraph::model::Term::Literal(l) if l.datatype() == xsd::FLOAT => !l
                .value()
                .parse::<oxsdatatypes::Float>()
                .is_ok_and(|f| f.is_nan()),
            oxigraph::model::Term::Literal(l) if l.datatype() == xsd::DOUBLE => !l
                .value()
                .parse::<oxsdatatypes::Double>()
                .is_ok_and(|f| f.is_nan()),
            _ => true,
        }),
        _ => Some(true),
    }
}

fn same_value_kind(x: TermId, y: TermId) -> bool {
    x.kind() == y.kind() && matches!(x.kind(), Kind::String | Kind::Integer | Kind::Boolean)
}

fn is_simple(l: &oxigraph::model::Literal) -> bool {
    l.language().is_none() && l.datatype() == oxigraph::model::vocab::xsd::STRING
}

/// `LANGMATCHES` (RFC 4647 basic filtering), `range` lowercase.
fn lang_matches(tag: &str, range: &str) -> bool {
    if range == "*" {
        return !tag.is_empty();
    }
    let tag = tag.to_ascii_lowercase();
    tag == range
        || tag
            .strip_prefix(range)
            .is_some_and(|rest| rest.starts_with('-'))
}

/// Whether `e` is made of what [`crate::idvalue::compile`] reads: variables, literals,
/// `+ - * /` and the number and date functions it knows.
pub(crate) fn value_expression(e: &Expression) -> bool {
    use Expression as E;
    match e {
        E::Variable(_) | E::Literal(_) => true,
        E::Add(a, b) | E::Subtract(a, b) | E::Multiply(a, b) | E::Divide(a, b) => {
            value_expression(a) && value_expression(b)
        }
        E::UnaryMinus(a) => value_expression(a),
        E::If(c, a, b) => {
            crate::idvalue::is_condition(c, &value_expression)
                && value_expression(a)
                && value_expression(b)
        }
        E::Coalesce(list) => !list.is_empty() && list.iter().all(value_expression),
        E::FunctionCall(
            Function::Abs
            | Function::Ceil
            | Function::Floor
            | Function::Round
            | Function::Year
            | Function::Month
            | Function::Day
            | Function::Hours
            | Function::Minutes
            | Function::Seconds,
            args,
        ) => matches!(args.as_slice(), [a] if value_expression(a)),
        // The length of a string, and a text test cast to 0 or 1 (#165).
        E::FunctionCall(Function::StrLen, args) => {
            matches!(args.as_slice(), [a] if crate::idvalue::text_part(a).is_some())
        }
        E::FunctionCall(Function::Custom(f), args)
            if f.as_ref() == oxigraph::model::vocab::xsd::INTEGER =>
        {
            matches!(args.as_slice(), [a] if text_test(a).is_some() || value_expression(a))
        }
        // `xsd:decimal` of a number (#11).
        E::FunctionCall(Function::Custom(f), args)
            if f.as_ref() == oxigraph::model::vocab::xsd::DECIMAL =>
        {
            matches!(args.as_slice(), [a] if value_expression(a))
        }
        _ => false,
    }
}

/// Not a plain variable or constant (those `=` compares as terms first).
fn computed(e: &Expression) -> bool {
    !matches!(
        e,
        Expression::Variable(_) | Expression::Literal(_) | Expression::NamedNode(_)
    )
}

/// The patterns of an `EXISTS` evaluated on ids: basic graph patterns, their joins, and
/// `FILTER`s over them (#11).
fn exists_shape(p: &GraphPattern) -> bool {
    match p {
        GraphPattern::Bgp { patterns } => patterns.iter().all(|t| {
            !matches!(t.subject, TermPattern::Triple(_))
                && !matches!(t.object, TermPattern::Triple(_))
        }),
        GraphPattern::Join { left, right } => exists_shape(left) && exists_shape(right),
        GraphPattern::Filter { expr, inner } => filter_covered(expr) && exists_shape(inner),
        _ => false,
    }
}

/// Whether every `FILTER` of an `EXISTS` pattern ([`exists_shape`]) reads, of the variables
/// of `t`, only those its own group certainly binds: evaluated on ids a filter sees its group's
/// rows, substituted it would see the row's values for the others.
fn filters_see_their_own(t: &Table, p: &GraphPattern) -> bool {
    match p {
        GraphPattern::Join { left, right } => {
            filters_see_their_own(t, left) && filters_see_their_own(t, right)
        }
        GraphPattern::Filter { expr, inner } => {
            let bound = crate::plan::certain(inner);
            let mut read = std::collections::HashSet::new();
            expression_reads(expr, &mut read);
            !read
                .iter()
                .any(|v| t.vars.iter().any(|c| c.as_str() == v) && !bound.contains(v))
                && filters_see_their_own(t, inner)
        }
        _ => true,
    }
}

/// The variables a filter reads, also every variable of the patterns of `EXISTS` inside it.
fn expression_reads(e: &Expression, out: &mut std::collections::HashSet<String>) {
    fn expression(e: &Expression, out: &mut std::collections::HashSet<String>) {
        match e {
            Expression::Variable(v) | Expression::Bound(v) => {
                out.insert(v.as_str().to_owned());
            }
            Expression::NamedNode(_) | Expression::Literal(_) => {}
            Expression::Exists(p) => everything(p, out),
            Expression::FunctionCall(_, args) | Expression::Coalesce(args) => {
                args.iter().for_each(|a| expression(a, out));
            }
            Expression::In(a, list) => {
                expression(a, out);
                list.iter().for_each(|l| expression(l, out));
            }
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
                expression(a, out);
                expression(b, out);
            }
            Expression::UnaryPlus(a) | Expression::UnaryMinus(a) | Expression::Not(a) => {
                expression(a, out);
            }
            Expression::If(a, b, c) => {
                expression(a, out);
                expression(b, out);
                expression(c, out);
            }
        }
    }
    // Every variable of a pattern inside an `EXISTS`: the row's values reach all of them.
    fn everything(p: &GraphPattern, out: &mut std::collections::HashSet<String>) {
        match p {
            GraphPattern::Bgp { patterns } => {
                for t in patterns {
                    for v in [&t.subject, &t.object] {
                        if let TermPattern::Variable(v) = v {
                            out.insert(v.as_str().to_owned());
                        }
                    }
                    if let NamedNodePattern::Variable(v) = &t.predicate {
                        out.insert(v.as_str().to_owned());
                    }
                }
            }
            GraphPattern::Join { left, right } => {
                everything(left, out);
                everything(right, out);
            }
            GraphPattern::Filter { expr, inner } => {
                expression(expr, out);
                everything(inner, out);
            }
            // Not an `EXISTS` shape: its filter is not covered, and nothing here is reached.
            _ => {}
        }
    }
    expression(e, out);
}

fn has_exists(e: &Expression) -> bool {
    match e {
        Expression::Exists(_) => true,
        Expression::Not(e) => has_exists(e),
        Expression::And(a, b) => has_exists(a) || has_exists(b),
        _ => false,
    }
}

/// Whether [`Eval`] evaluates `FILTER(e)` on ids (it may still give up on the data).
pub(crate) fn filter_covered(e: &Expression) -> bool {
    covered(e) || crate::idstring::evaluated(e)
}

/// [`filter_covered`] by the kernels of ids themselves.
fn covered(e: &Expression) -> bool {
    use Expression as E;
    let var = |a: &[E]| matches!(a, [E::Variable(_)]);
    let operand = |e: &E| matches!(e, E::Variable(_) | E::NamedNode(_) | E::Literal(_));
    match e {
        E::Exists(p) => exists_shape(p),
        E::Not(e) => filter_covered(e),
        E::And(a, b) => filter_covered(a) && filter_covered(b),
        // An EXISTS is a filter of its own, not a value inside `||`.
        E::Or(a, b) => !has_exists(a) && !has_exists(b) && filter_covered(a) && filter_covered(b),
        E::Bound(_) | E::Variable(_) => true,
        E::SameTerm(a, b) => operand(a) && operand(b),
        E::Equal(a, b) => {
            let lang = |x: &E, y: &E| {
                matches!(x, E::FunctionCall(Function::Lang, args) if var(args))
                    && matches!(y, E::Literal(_))
            };
            let datatype = |x: &E, y: &E| {
                matches!(x, E::FunctionCall(Function::Datatype, args) if var(args))
                    && matches!(y, E::NamedNode(_))
            };
            let str = |x: &E, y: &E| {
                matches!(x, E::FunctionCall(Function::Str, args) if var(args))
                    && matches!(y, E::Literal(l) if is_simple(l))
            };
            (operand(a) && operand(b))
                || (value_expression(a) && value_expression(b))
                || lang(a, b)
                || lang(b, a)
                || datatype(a, b)
                || datatype(b, a)
                || str(a, b)
                || str(b, a)
                || string_equal(a, b).is_some()
                || string_equal(b, a).is_some()
        }
        // `a IN (b, c)` is `a = b || a = c`.
        E::In(a, list) => list
            .iter()
            .all(|b| filter_covered(&E::Equal(a.clone(), Box::new(b.clone())))),
        E::FunctionCall(
            Function::IsLiteral | Function::IsIri | Function::IsBlank | Function::IsNumeric,
            args,
        ) => var(args),
        E::Greater(a, b) | E::GreaterOrEqual(a, b) | E::Less(a, b) | E::LessOrEqual(a, b) => {
            value_expression(a) && value_expression(b)
        }
        E::FunctionCall(Function::LangMatches, args) => matches!(
            args.as_slice(),
            [E::FunctionCall(Function::Lang, inner), E::Literal(_)] if var(inner)
        ),
        E::FunctionCall(
            Function::Regex | Function::StrStarts | Function::Contains | Function::StrEnds,
            _,
        ) => {
            string_prefix(e).is_some()
                || text_test_on(e).is_some_and(|(a, _)| {
                    matches!(a, E::Variable(_)) || crate::idstring::computable(a)
                })
        }
        e => crate::idstring::computable(e) || crate::direct::number_computable(e),
    }
}

/// The effective boolean value of a term (the evaluator's `effective_boolean_value`): `None`
/// for a literal whose value only the evaluator reads, `Some(None)` an error.
fn effective_boolean_value(term: oxigraph::model::Term) -> Option<Option<bool>> {
    use oxigraph::model::vocab::xsd;
    let oxigraph::model::Term::Literal(l) = term else {
        return Some(None);
    };
    if l.language().is_some() {
        return Some(None);
    }
    let datatype = l.datatype();
    Some(Some(if datatype == xsd::STRING {
        !l.value().is_empty()
    } else if datatype == xsd::BOOLEAN {
        match l.value() {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => return None,
        }
    } else if datatype == xsd::INTEGER {
        // Each read with its own lexical rules, as the evaluator reads it: a form the type
        // doesn't allow (`"1e3"^^xsd:integer`) is the evaluator's to decide.
        l.value().parse::<oxsdatatypes::Integer>().ok()? != oxsdatatypes::Integer::from(0)
    } else if datatype == xsd::DECIMAL {
        l.value().parse::<oxsdatatypes::Decimal>().ok()? != oxsdatatypes::Decimal::from(0)
    } else if datatype == xsd::DOUBLE {
        let v = f64::from(l.value().parse::<oxsdatatypes::Double>().ok()?);
        v != 0.0 && !v.is_nan()
    } else {
        return None;
    }))
}

/// A test of the text of a string literal.
pub(crate) enum TextTest {
    Starts(String),
    Ends(String),
    Contains(String),
    Regex(regex::Regex),
}

impl TextTest {
    pub(crate) fn matches(&self, text: &str) -> bool {
        match self {
            Self::Starts(x) => text.starts_with(x.as_str()),
            Self::Ends(x) => text.ends_with(x.as_str()),
            Self::Contains(x) => text.contains(x.as_str()),
            Self::Regex(r) => r.is_match(text),
        }
    }
}

/// `e = "text"` with `e` a string the term kernels compute (not a variable, nor `STR` of
/// one, which [`Eval::str_equal`] reads from the ids) and a simple literal: `e` and the text.
fn string_equal<'a>(e: &'a Expression, text: &'a Expression) -> Option<(&'a Expression, &'a str)> {
    match (e, text) {
        (Expression::Variable(_) | Expression::Literal(_) | Expression::NamedNode(_), _) => None,
        (Expression::FunctionCall(Function::Str, args), _)
            if matches!(args.as_slice(), [Expression::Variable(_)]) =>
        {
            None
        }
        (e, Expression::Literal(l)) if is_simple(l) && crate::idstring::computable(e) => {
            Some((e, l.value()))
        }
        _ => None,
    }
}

/// `STRSTARTS`, `STRENDS` or `CONTAINS` of a variable and a simple literal, or `REGEX` of a
/// variable with a pattern and flags that are simple literals: the variable and the test of
/// its text. Each is an error for a value that is not a string literal (simple, `xsd:string`
/// or language-tagged), and true or false otherwise: the second argument has no language tag,
/// so it is compatible with every string. The regular expression is built as the evaluator
/// builds it (XPath flags `s`, `m`, `i`, `x` and `q`); `None` for one it doesn't build.
pub(crate) fn text_test(e: &Expression) -> Option<(&Variable, TextTest)> {
    match text_test_on(e)? {
        (Expression::Variable(v), test) => Some((v, test)),
        _ => None,
    }
}

/// [`text_test`] of any first argument: the expression whose text is tested, and the test.
pub(crate) fn text_test_on(e: &Expression) -> Option<(&Expression, TextTest)> {
    use Expression as E;
    fn simple(e: &Expression) -> Option<&str> {
        match e {
            Expression::Literal(l) if is_simple(l) => Some(l.value()),
            _ => None,
        }
    }
    let E::FunctionCall(f, args) = e else {
        return None;
    };
    match (f, args.as_slice()) {
        (Function::StrStarts, [v, x]) => Some((v, TextTest::Starts(simple(x)?.to_owned()))),
        (Function::StrEnds, [v, x]) => Some((v, TextTest::Ends(simple(x)?.to_owned()))),
        (Function::Contains, [v, x]) => Some((v, TextTest::Contains(simple(x)?.to_owned()))),
        (Function::Regex, [v, p, rest @ ..]) if rest.len() <= 1 => {
            let flags = match rest {
                [f] => simple(f)?,
                _ => "",
            };
            let mut pattern = std::borrow::Cow::Borrowed(simple(p)?);
            if flags.contains('q') {
                pattern = regex::escape(&pattern).into();
            }
            let mut builder = regex::RegexBuilder::new(&pattern);
            builder.size_limit(1_000_000);
            for flag in flags.chars() {
                match flag {
                    's' => {
                        builder.dot_matches_new_line(true);
                    }
                    'm' => {
                        builder.multi_line(true);
                    }
                    'i' => {
                        builder.case_insensitive(true);
                    }
                    'x' => {
                        builder.ignore_whitespace(true);
                    }
                    'q' => {}
                    _ => return None,
                }
            }
            Some((v, TextTest::Regex(builder.build().ok()?)))
        }
        _ => None,
    }
}

/// `STRSTARTS(?v, "prefix")` or `REGEX(?v, "^prefix")` (no flags, and no special character in
/// the prefix): the variable and the prefix its string value is tested for. Both are an error
/// for a value that is not a string literal (simple or language-tagged).
fn string_prefix(e: &Expression) -> Option<(&Variable, String)> {
    use Expression as E;
    fn simple(e: &Expression) -> Option<&str> {
        match e {
            Expression::Literal(l) if is_simple(l) => Some(l.value()),
            _ => None,
        }
    }
    match e {
        E::FunctionCall(Function::StrStarts, args) => match args.as_slice() {
            [E::Variable(v), p] => Some((v, simple(p)?.to_owned())),
            _ => None,
        },
        E::FunctionCall(Function::Regex, args) => {
            let (v, pattern) = match args.as_slice() {
                [E::Variable(v), p] => (v, simple(p)?),
                [E::Variable(v), p, flags] if simple(flags)?.is_empty() => (v, simple(p)?),
                _ => return None,
            };
            let prefix = pattern.strip_prefix('^')?;
            // Characters that mean something in a pattern (and `\s`, `\d`, ...): give up.
            if prefix.chars().any(|c| ".^$*+?()[]{}|\\".contains(c)) {
                return None;
            }
            Some((v, prefix.to_owned()))
        }
        _ => None,
    }
}

/// `FILTER(e)` over `inner` as an equivalent pattern whose filters sit closer to the triples:
/// over a `UNION`, the filter of each side (a filter tests each solution on its own); a
/// conjunction with an equality of two variables (see [`equal_vars`]) as two filters, the
/// equality inside.
fn pushed_filter(e: &Expression, inner: &GraphPattern) -> Option<GraphPattern> {
    use GraphPattern as G;
    match inner {
        // A filter on the values of the triple patterns a path is joined with: on those
        // first, so fewer starts are searched (a variable of the patterns is bound in each of
        // their rows, and the joined row holds the same value).
        G::Join { left, right } => {
            let (patterns, path, path_right) = match (&**left, &**right) {
                (G::Bgp { patterns }, p @ G::Path { .. }) => (patterns, p, true),
                (p @ G::Path { .. }, G::Bgp { patterns }) => (patterns, p, false),
                _ => return None,
            };
            let mut names = std::collections::HashSet::new();
            crate::plan::expression_variables(e, &mut names)?;
            let mut bound = std::collections::HashSet::new();
            for t in patterns {
                for v in [&t.subject, &t.object] {
                    if let spargebra::term::TermPattern::Variable(v) = v {
                        bound.insert(v.as_str());
                    }
                }
                if let NamedNodePattern::Variable(v) = &t.predicate {
                    bound.insert(v.as_str());
                }
            }
            if names.is_empty() || !names.iter().all(|n| bound.contains(n.as_str())) {
                return None;
            }
            let filtered = G::Filter {
                expr: e.clone(),
                inner: Box::new(G::Bgp {
                    patterns: patterns.clone(),
                }),
            };
            let (left, right) = if path_right {
                (filtered, path.clone())
            } else {
                (path.clone(), filtered)
            };
            Some(G::Join {
                left: Box::new(left),
                right: Box::new(right),
            })
        }
        G::Union { left, right } => {
            let side = |p: &GraphPattern| G::Filter {
                expr: e.clone(),
                inner: Box::new(p.clone()),
            };
            Some(G::Union {
                left: Box::new(side(left)),
                right: Box::new(side(right)),
            })
        }
        G::Bgp { patterns } => {
            let Expression::And(a, b) = e else {
                return None;
            };
            let (eq, rest) = if equal_vars(a, patterns).is_some() {
                (a, b)
            } else if equal_vars(b, patterns).is_some() {
                (b, a)
            } else {
                return None;
            };
            Some(G::Filter {
                expr: (**rest).clone(),
                inner: Box::new(G::Filter {
                    expr: (**eq).clone(),
                    inner: Box::new(inner.clone()),
                }),
            })
        }
        _ => None,
    }
}

/// `FILTER(?a = ?b)` or `FILTER(sameTerm(?a, ?b))` over the triple patterns `patterns`, which
/// bind both variables in every solution, as the patterns with `?b` replaced by `?a`: the
/// variable kept, the one dropped (a copy of the kept one in each solution) and the patterns.
/// `=` is `sameTerm` when one of the two is a subject or predicate, an IRI or blank node,
/// which equals no other term; between two literals `=` compares values, so it stays a filter.
fn equal_vars(
    e: &Expression,
    patterns: &[spargebra::term::TriplePattern],
) -> Option<(Variable, Variable, Vec<spargebra::term::TriplePattern>)> {
    use spargebra::term::TermPattern;
    let (Expression::Equal(a, b) | Expression::SameTerm(a, b)) = e else {
        return None;
    };
    let (Expression::Variable(a), Expression::Variable(b)) = (&**a, &**b) else {
        return None;
    };
    if a == b {
        return None;
    }
    let is = |t: &TermPattern, v: &Variable| matches!(t, TermPattern::Variable(x) if x == v);
    let is_p =
        |t: &NamedNodePattern, v: &Variable| matches!(t, NamedNodePattern::Variable(x) if x == v);
    let anywhere = |v: &Variable| {
        patterns
            .iter()
            .any(|t| is(&t.subject, v) || is(&t.object, v) || is_p(&t.predicate, v))
    };
    let resource = |v: &Variable| {
        patterns
            .iter()
            .any(|t| is(&t.subject, v) || is_p(&t.predicate, v))
    };
    if !anywhere(a) || !anywhere(b) {
        return None;
    }
    let (keep, copy) = if resource(a) {
        (a, b)
    } else if resource(b) {
        (b, a)
    } else if matches!(e, Expression::SameTerm(..)) {
        (a, b)
    } else {
        return None;
    };
    let swap = |t: &TermPattern| {
        if is(t, copy) {
            TermPattern::Variable(keep.clone())
        } else {
            t.clone()
        }
    };
    let patterns = patterns
        .iter()
        .map(|t| spargebra::term::TriplePattern {
            subject: swap(&t.subject),
            predicate: if is_p(&t.predicate, copy) {
                NamedNodePattern::Variable(keep.clone())
            } else {
                t.predicate.clone()
            },
            object: swap(&t.object),
        })
        .collect();
    Some((keep.clone(), copy.clone(), patterns))
}

/// `t` with a column `copy` holding the values of its column `keep`.
fn with_copy(t: Table, keep: &Variable, copy: Variable) -> Option<Table> {
    let c = t.col(keep)?;
    let w = t.width();
    let mut ids = Vec::with_capacity(t.len() * (w + 1));
    for r in 0..t.len() {
        let row = t.row(r);
        ids.extend_from_slice(row);
        ids.push(row[c]);
    }
    let mut vars = t.vars;
    vars.push(copy);
    let mut maybe = t.maybe;
    maybe.push(maybe[c]);
    Some(Table {
        vars,
        ids,
        maybe,
        ones: t.ones,
    })
}

impl Table {
    fn push(&mut self, row: &[TermId]) {
        self.ids.extend_from_slice(row);
        self.ones += 1;
    }
}

fn empty_like(t: &Table) -> Table {
    Table {
        vars: t.vars.clone(),
        ids: Vec::new(),
        maybe: t.maybe.clone(),
        ones: 0,
    }
}

/// The columns of the variables `a` and `b` share, as (column in `a`, column in `b`), if each
/// is bound in every row of both.
fn shared(a: &Table, b: &Table) -> Option<Vec<(usize, usize)>> {
    let mut keys = Vec::new();
    for (i, v) in a.vars.iter().enumerate() {
        if let Some(j) = b.col(v) {
            if a.maybe[i] || b.maybe[j] {
                return None;
            }
            keys.push((i, j));
        }
    }
    Some(keys)
}

/// The columns of a combined row: `a`'s, then `b`'s that `a` doesn't have.
fn combined(a: &Table, b: &Table) -> (Vec<Variable>, Vec<bool>, Vec<usize>) {
    let mut vars = a.vars.clone();
    let mut maybe = a.maybe.clone();
    let mut extra = Vec::new();
    for (j, v) in b.vars.iter().enumerate() {
        if a.col(v).is_none() {
            vars.push(v.clone());
            maybe.push(b.maybe[j]);
            extra.push(j);
        }
    }
    (vars, maybe, extra)
}

/// A hash of ids, for [`KeyIndex`] and the groups of [`group_rows`] (an Fx-style fold and a
/// splitmix finish, so the low bits the table uses vary too).
#[inline]
fn hash_ids(ids: impl Iterator<Item = TermId>) -> u64 {
    let mut h: u64 = 0;
    for id in ids {
        h = (h.rotate_left(5) ^ id.0).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

/// A hasher for keys that are hashes already.
#[derive(Default)]
struct Prehashed(u64);

impl std::hash::Hasher for Prehashed {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 << 8) | u64::from(b);
        }
    }
    fn write_u64(&mut self, x: u64) {
        self.0 = x;
    }
}

type PreMap<V> = HashMap<u64, V, std::hash::BuildHasherDefault<Prehashed>>;

const END: u32 = u32::MAX;

/// The rows of a table by the values of some of its columns: per hash of those values the
/// first row, chained to the next rows with that hash in row order. No allocation per row
/// (a map of `Vec` keys took most of a hash join's time).
struct KeyIndex<'t> {
    t: &'t Table,
    cols: Vec<usize>,
    heads: PreMap<u32>,
    next: Vec<u32>,
}

impl<'t> KeyIndex<'t> {
    fn new(t: &'t Table, cols: Vec<usize>) -> Self {
        let n = t.len();
        let mut heads = PreMap::default();
        heads.reserve(n);
        let mut next = vec![END; n];
        // From the last row, so each chain runs in row order.
        for r in (0..n).rev() {
            let row = t.row(r);
            let head = heads
                .entry(hash_ids(cols.iter().map(|&c| row[c])))
                .or_insert(END);
            next[r] = *head;
            *head = r as u32;
        }
        Self {
            t,
            cols,
            heads,
            next,
        }
    }

    /// The rows whose values equal `row`'s in columns `probe`, in row order.
    fn matches<'a>(
        &'a self,
        row: &'a [TermId],
        probe: &'a [usize],
    ) -> impl Iterator<Item = usize> + 'a {
        let mut r = self
            .heads
            .get(&hash_ids(probe.iter().map(|&c| row[c])))
            .copied()
            .unwrap_or(END);
        std::iter::from_fn(move || {
            while r != END {
                let at = r as usize;
                r = self.next[at];
                let other = self.t.row(at);
                if self
                    .cols
                    .iter()
                    .zip(probe)
                    .all(|(&c, &p)| other[c] == row[p])
                {
                    return Some(at);
                }
            }
            None
        })
    }

    fn contains(&self, row: &[TermId], probe: &[usize]) -> bool {
        self.matches(row, probe).next().is_some()
    }
}

/// The rows one chunk of a probe made, for [`assemble`].
struct Part<'u> {
    ids: Vec<TermId>,
    ones: usize,
    counted: usize,
    /// The ids of all chunks.
    used: &'u AtomicUsize,
    cancelled: &'u (dyn Fn() -> bool + Sync),
    stopped: bool,
}

impl Part<'_> {
    /// Whether the ids of all chunks went past [`max_ids`].
    fn over(&mut self) -> bool {
        let new = self.ids.len() - self.counted;
        self.counted = self.ids.len();
        self.used.fetch_add(new, AtomicOrdering::Relaxed) + new > max_ids()
    }

    /// Whether another row may be added: not once the chunks went past [`max_ids`] or the
    /// query is cancelled, checked every 2^16 new ids, so one row of a cross product that
    /// matches a whole table stops there too (#467).
    fn room(&mut self) -> bool {
        if !self.stopped && self.ids.len() - self.counted > 1 << 16 {
            self.stopped = self.over() || (self.cancelled)();
        }
        !self.stopped
    }
}

/// Probes the rows of `a` chunk by chunk, on several cores for a large table
/// ([`tripletrail_store::par`]): `each` writes the output rows of one row of `a`. The chunks come
/// back in order, so the table is the one a single pass makes.
fn probe(
    a: &Table,
    vars: Vec<Variable>,
    maybe: Vec<bool>,
    cancelled: &(dyn Fn() -> bool + Sync),
    each: impl Fn(&[TermId], &mut Part) + Sync,
) -> Option<Table> {
    let used = AtomicUsize::new(0);
    let parts = tripletrail_store::par::chunks(a.len(), |range| {
        let mut part = Part {
            ids: Vec::new(),
            ones: 0,
            counted: 0,
            used: &used,
            cancelled,
            stopped: false,
        };
        for (k, r) in range.enumerate() {
            each(a.row(r), &mut part);
            // Every 256 rows, and as soon as one row's matches (a frequent key) added many.
            if part.stopped
                || (k % 256 == 255 || part.ids.len() - part.counted > 1 << 16)
                    && (part.over() || cancelled())
            {
                return None;
            }
        }
        (!part.over()).then_some(part)
    });
    let mut out = Table {
        vars,
        ids: Vec::new(),
        maybe,
        ones: 0,
    };
    let parts: Vec<Part> = parts.into_iter().collect::<Option<_>>()?;
    if parts.len() == 1 {
        let part = parts.into_iter().next()?;
        out.ids = part.ids;
        out.ones = part.ones;
        return Some(out);
    }
    out.ids.reserve(parts.iter().map(|p| p.ids.len()).sum());
    for part in parts {
        out.ids.extend_from_slice(&part.ids);
        out.ones += part.ones;
    }
    Some(out)
}

/// The columns of the variables `a` and `b` share, as (column in `a`, column in `b`): those
/// bound in every row of both, then those unbound in some rows of either. `None` when only
/// the second kind is shared and pairing every row of `a` with every row of `b` would take
/// more than [`LOOSE_PAIRS`] checks.
fn shared_loose(a: &Table, b: &Table) -> Option<(Keys, Keys)> {
    let (mut keys, mut loose) = (Vec::new(), Vec::new());
    for (i, v) in a.vars.iter().enumerate() {
        if let Some(j) = b.col(v) {
            if a.maybe[i] || b.maybe[j] {
                loose.push((i, j));
            } else {
                keys.push((i, j));
            }
        }
    }
    if keys.is_empty() && !loose.is_empty() && a.len().saturating_mul(b.len()) > LOOSE_PAIRS {
        return None;
    }
    Some((keys, loose))
}

/// Pairs of columns, (in `a`, in `b`).
type Keys = Vec<(usize, usize)>;

/// The most graphs `GRAPH ?g { … }` evaluates its pattern in, one by one.
const MAX_GRAPHS: usize = 256;

/// The most row pairs a join on variables that may be unbound checks one by one (`VALUES`
/// with `UNDEF`, a value an `OPTIONAL` may leave unbound).
const LOOSE_PAIRS: usize = 1 << 24;

/// Whether rows `x` of `a` and `y` of `b` agree on the `loose` columns: equal, or unbound on
/// one side.
fn compatible(x: &[TermId], y: &[TermId], loose: &[(usize, usize)]) -> bool {
    loose
        .iter()
        .all(|&(i, j)| x[i] == UNBOUND || y[j] == UNBOUND || x[i] == y[j])
}

/// Writes the row `x` of `a` merged with `y` of `b`: `x`, its unbound `loose` columns from
/// `y`, then `y`'s `extra` columns.
fn merged(out: &mut Part, x: &[TermId], y: &[TermId], loose: &[(usize, usize)], extra: &[usize]) {
    let start = out.ids.len();
    out.ids.extend_from_slice(x);
    for &(i, j) in loose {
        if x[i] == UNBOUND {
            out.ids[start + i] = y[j];
        }
    }
    out.ids.extend(extra.iter().map(|&j| y[j]));
    out.ones += 1;
}

fn join(a: &Table, b: &Table, cancelled: &(dyn Fn() -> bool + Sync)) -> Option<Table> {
    let (keys, loose) = shared_loose(a, b)?;
    let (vars, mut maybe, extra) = combined(a, b);
    for &(i, j) in &loose {
        maybe[i] = a.maybe[i] && b.maybe[j];
    }
    let index = KeyIndex::new(b, keys.iter().map(|k| k.1).collect());
    let probe_cols: Vec<usize> = keys.iter().map(|k| k.0).collect();
    probe(a, vars, maybe, cancelled, |row, out| {
        for m in index.matches(row, &probe_cols) {
            if !out.room() {
                return;
            }
            let other = b.row(m);
            if compatible(row, other, &loose) {
                merged(out, row, other, &loose, &extra);
            }
        }
    })
}

fn left_join(a: &Table, b: &Table, cancelled: &(dyn Fn() -> bool + Sync)) -> Option<Table> {
    let (keys, loose) = shared_loose(a, b)?;
    let (vars, mut maybe, extra) = combined(a, b);
    for m in &mut maybe[a.width()..] {
        *m = true;
    }
    let index = KeyIndex::new(b, keys.iter().map(|k| k.1).collect());
    let probe_cols: Vec<usize> = keys.iter().map(|k| k.0).collect();
    probe(a, vars, maybe, cancelled, |row, out| {
        let before = out.ones;
        for m in index.matches(row, &probe_cols) {
            if !out.room() {
                return;
            }
            let other = b.row(m);
            if compatible(row, other, &loose) {
                merged(out, row, other, &loose, &extra);
            }
        }
        if out.ones == before {
            out.ids.extend_from_slice(row);
            out.ids.extend(extra.iter().map(|_| UNBOUND));
            out.ones += 1;
        }
    })
}

/// The rows of `a` that have (`keep`) or don't have a row of `b` with the same values in the
/// columns of `keys` (`a`'s, `b`'s).
fn semi(
    a: &Table,
    b: &Table,
    keys: &[(usize, usize)],
    keep: bool,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Option<Table> {
    let index = KeyIndex::new(b, keys.iter().map(|k| k.1).collect());
    let probe_cols: Vec<usize> = keys.iter().map(|k| k.0).collect();
    probe(a, a.vars.clone(), a.maybe.clone(), cancelled, |row, out| {
        if index.contains(row, &probe_cols) == keep {
            out.ids.extend_from_slice(row);
            out.ones += 1;
        }
    })
}

fn minus(a: &Table, b: &Table, cancelled: &(dyn Fn() -> bool + Sync)) -> Option<Table> {
    let (keys, loose) = shared_loose(a, b)?;
    // No shared variable: nothing is removed.
    if keys.is_empty() && loose.is_empty() {
        let mut out = empty_like(a);
        out.ids = a.ids.clone();
        out.ones = a.len();
        return Some(out);
    }
    if loose.is_empty() {
        return semi(a, b, &keys, false, cancelled);
    }
    // A row goes when a row of `b` is compatible with it and binds a variable it binds too.
    let index = KeyIndex::new(b, keys.iter().map(|k| k.1).collect());
    let probe_cols: Vec<usize> = keys.iter().map(|k| k.0).collect();
    probe(a, a.vars.clone(), a.maybe.clone(), cancelled, |row, out| {
        let removed = index.matches(row, &probe_cols).any(|m| {
            let other = b.row(m);
            compatible(row, other, &loose)
                && (!keys.is_empty()
                    || loose
                        .iter()
                        .any(|&(i, j)| row[i] != UNBOUND && other[j] != UNBOUND))
        });
        if !removed {
            out.ids.extend_from_slice(row);
            out.ones += 1;
        }
    })
}

fn union(a: Table, b: Table) -> Option<Table> {
    let (vars, mut maybe, _) = combined(&a, &b);
    for (c, v) in vars.iter().enumerate() {
        if a.col(v).is_none() || b.col(v).is_none() {
            maybe[c] = true;
        } else if let Some(j) = b.col(v) {
            maybe[c] |= b.maybe[j];
        }
    }
    let mut out = Table {
        ids: Vec::with_capacity((a.len() + b.len()) * vars.len()),
        maybe,
        ones: 0,
        vars,
    };
    for t in [&a, &b] {
        let map: Vec<Option<usize>> = out.vars.iter().map(|v| t.col(v)).collect();
        for r in 0..t.len() {
            let row = t.row(r);
            out.ids
                .extend(map.iter().map(|c| c.map_or(UNBOUND, |c| row[c])));
            out.ones += 1;
        }
    }
    (out.ids.len() <= max_ids()).then_some(out)
}

fn is_literal(id: TermId) -> bool {
    matches!(
        id.kind(),
        Kind::String
            | Kind::LangString
            | Kind::Typed
            | Kind::Geometry
            | Kind::Integer
            | Kind::Boolean
            | Kind::Double
            | Kind::Decimal
            | Kind::DateTime
    )
}

/// `COUNT(*)`, `COUNT(?v)` (`var`, not distinct), `COUNT(DISTINCT ?v)` and
/// `COUNT(DISTINCT *)` of a table.
/// How the right side of a keyed operator ([`Eval::rows`]) counts for a left row with `l`
/// matching right rows `r` of the same key.
#[derive(Clone, Copy)]
enum Keyed {
    Join,
    Optional,
    Minus,
    Exists,
    NotExists,
}

impl Keyed {
    fn rows(self, l: u64, r: u64) -> Option<u64> {
        match self {
            Keyed::Join => l.checked_mul(r),
            Keyed::Optional => l.checked_mul(r.max(1)),
            Keyed::Minus | Keyed::NotExists => Some(if r == 0 { l } else { 0 }),
            Keyed::Exists => Some(if r == 0 { 0 } else { l }),
        }
    }
}

/// The variables a pattern binds in some of its solutions.
fn in_scope(p: &GraphPattern) -> Vec<Variable> {
    let mut vars = Vec::new();
    p.on_in_scope_variable(|v| {
        if !vars.contains(v) {
            vars.push(v.clone());
        }
    });
    vars
}

/// Triples that read a result table of the spatial rewrite are answered by the snapshot during
/// evaluation, not from the quads.
fn reads_a_table(patterns: &[spargebra::term::TriplePattern]) -> bool {
    patterns.iter().any(|t| {
        matches!(&t.predicate, NamedNodePattern::NamedNode(p) if p.as_str().starts_with("urn:tripletrail:"))
    })
}

impl Eval<'_> {
    /// The number of solutions of `p` (#185, #165), counted without building them where an
    /// operator's sides share exactly one variable: per value of that variable, from each
    /// side's counts per value (`?s rdfs:label ?l OPTIONAL { ?s a ?c }` is the sum over `?s`
    /// of its labels times its classes, or 1). `UNION` adds its sides' counts. `None` for
    /// other shapes, which [`Self::eval`] may still count from their rows.
    pub(crate) fn rows(&self, p: &GraphPattern) -> Option<u64> {
        use GraphPattern as G;
        if (self.cancelled)() {
            return None;
        }
        match p {
            G::Bgp { patterns } => {
                if reads_a_table(patterns) {
                    return None;
                }
                let compiled = self.snapshot.compile_bgp(patterns, &[])?;
                if compiled.empty {
                    return Some(0);
                }
                self.snapshot.count_ids(
                    &compiled.triples,
                    compiled.columns.len(),
                    self.graph,
                    tripletrail_store::BgpCount::Rows,
                    self.cancelled,
                )
            }
            G::Union { left, right } => self.rows(left)?.checked_add(self.rows(right)?),
            // Two basic graph patterns are one to the store's joins.
            G::Join { left, right }
                if !matches!((&**left, &**right), (G::Bgp { .. }, G::Bgp { .. })) =>
            {
                self.keyed(left, right, &in_scope(right), Keyed::Join)
            }
            G::LeftJoin {
                left,
                right,
                expression: None,
            } => self.keyed(left, right, &in_scope(right), Keyed::Optional),
            G::Minus { left, right } => self.keyed(left, right, &in_scope(right), Keyed::Minus),
            G::Filter { expr, inner } => {
                if let Some(p) = pushed_filter(expr, inner) {
                    return self.rows(&p);
                }
                if let G::Bgp { patterns } = &**inner {
                    if let Some((_, _, same)) = equal_vars(expr, patterns) {
                        return self.rows(&G::Bgp { patterns: same });
                    }
                    // `?a != ?b`: the solutions less those with the same term for both.
                    if let Expression::Not(e) = expr
                        && let Some((_, _, same)) = equal_vars(e, patterns)
                    {
                        let all = self.rows(inner)?;
                        return all.checked_sub(self.rows(&G::Bgp { patterns: same })?);
                    }
                }
                let (e, how) = match expr {
                    Expression::Exists(e) => (e, Keyed::Exists),
                    Expression::Not(n) => match &**n {
                        Expression::Exists(e) => (e, Keyed::NotExists),
                        _ => return None,
                    },
                    _ => return None,
                };
                // The pattern of an EXISTS sees every variable of the row, also those it
                // doesn't return: only a basic graph pattern, whose variables are all its own.
                let G::Bgp { patterns } = &**e else {
                    return None;
                };
                let mut vars = Vec::new();
                for t in patterns {
                    for term in [&t.subject, &t.object] {
                        if let spargebra::term::TermPattern::Variable(v) = term {
                            vars.push(v.clone());
                        }
                    }
                    if let NamedNodePattern::Variable(v) = &t.predicate {
                        vars.push(v.clone());
                    }
                }
                self.keyed(inner, e, &vars, how)
            }
            _ => None,
        }
    }

    /// The rows of `left` combined with `right` as `how` says, when the variables of `left`
    /// that `right` has (`right_vars`) are exactly one, bound in every solution of both.
    fn keyed(
        &self,
        left: &GraphPattern,
        right: &GraphPattern,
        right_vars: &[Variable],
        how: Keyed,
    ) -> Option<u64> {
        // A path is searched from the values the other side gives (`eval`), which may be few:
        // counting each side on its own would search it from every node.
        if has_path(left) || has_path(right) {
            return None;
        }
        let shared: Vec<Variable> = in_scope(left)
            .into_iter()
            .filter(|v| right_vars.contains(v))
            .collect();
        let key = match (shared.as_slice(), how) {
            ([key], _) => key,
            // Nothing in common: MINUS removes nothing.
            ([], Keyed::Minus) => return self.rows(left),
            _ => return None,
        };
        // A right side of one triple with far fewer matches than the left's one triple: its
        // keys first, then the left side's counts of those keys only (#165).
        if let (Some(nl), Some(nr)) = (self.triples_estimate(left), self.triples_estimate(right))
            && nr * WALK < nl
        {
            let r = self.key_counts(right, key)?;
            let l = self.right_key_counts(left, key, &r)?;
            let (mut total, mut covered) = (0u64, 0u64);
            let mut j = 0;
            for &(k, n) in &l {
                while j < r.len() && r[j].0 < k {
                    j += 1;
                }
                let m = if j < r.len() && r[j].0 == k {
                    r[j].1
                } else {
                    0
                };
                total = total.checked_add(how.rows(n, m)?)?;
                covered = covered.checked_add(n)?;
            }
            // The left keys the right side lacks keep their rows for MINUS, NOT EXISTS and
            // OPTIONAL.
            if how.rows(1, 0) == Some(1) {
                total = total.checked_add(self.rows(left)?.checked_sub(covered)?)?;
            }
            return Some(total);
        }
        let l = self.key_counts(left, key)?;
        let r = self.right_key_counts(right, key, &l)?;
        let mut total = 0u64;
        let mut j = 0;
        for &(k, n) in &l {
            while j < r.len() && r[j].0 < k {
                j += 1;
            }
            let m = if j < r.len() && r[j].0 == k {
                r[j].1
            } else {
                0
            };
            total = total.checked_add(how.rows(n, m)?)?;
        }
        Some(total)
    }

    /// The estimated matches of `p` when it is one triple of the default graph's quads, or a
    /// `UNION` of such triples.
    fn triples_estimate(&self, p: &GraphPattern) -> Option<f64> {
        if let GraphPattern::Union { left, right } = p {
            return Some(self.triples_estimate(left)? + self.triples_estimate(right)?);
        }
        self.one_triple_estimate(p)
    }

    /// The estimated matches of `p` when it is one triple of the default graph's quads.
    fn one_triple_estimate(&self, p: &GraphPattern) -> Option<f64> {
        let GraphPattern::Bgp { patterns } = p else {
            return None;
        };
        if reads_a_table(patterns) {
            return None;
        }
        let compiled = self.snapshot.compile_bgp(patterns, &[])?;
        let [t] = compiled.triples.as_slice() else {
            return None;
        };
        Some(if compiled.empty {
            0.0
        } else {
            self.scan_estimate(std::slice::from_ref(t))
        })
    }

    /// [`Self::key_counts`] of the right side of an operator whose left side has the keys of
    /// `left`: a triple looks those up when they are few, instead of reading all its matches.
    fn right_key_counts(
        &self,
        right: &GraphPattern,
        key: &Variable,
        left: &[(u64, u64)],
    ) -> Option<Vec<(u64, u64)>> {
        // A `UNION`: each side for those keys, added up.
        if let GraphPattern::Union { left: a, right: b } = right {
            return add_counts(
                &self.right_key_counts(a, key, left)?,
                &self.right_key_counts(b, key, left)?,
            );
        }
        if let GraphPattern::Bgp { patterns } = right
            && !reads_a_table(patterns)
            && let Some(compiled) = self.snapshot.compile_bgp(patterns, &[])
            && let [t] = compiled.triples.as_slice()
            && !compiled.empty
            && (left.len() as f64) * LOOKUP < self.scan_estimate(std::slice::from_ref(t))
        {
            let column = compiled
                .columns
                .iter()
                .position(|c| c.as_ref() == Some(key))?;
            let values: Vec<u64> = left.iter().map(|(k, _)| *k).collect();
            return self
                .snapshot
                .key_counts_of(t, column, &values, self.graph, self.cancelled);
        }
        // More keys: one sorted scan walked from key to key, while they are fewer than the
        // matches (#165).
        if let GraphPattern::Bgp { patterns } = right
            && !reads_a_table(patterns)
            && let Some(compiled) = self.snapshot.compile_bgp(patterns, &[])
            && let [t] = compiled.triples.as_slice()
            && !compiled.empty
            && (left.len() as f64) * WALK < self.scan_estimate(std::slice::from_ref(t))
            && let Some(column) = compiled
                .columns
                .iter()
                .position(|c| c.as_ref() == Some(key))
        {
            let values: Vec<u64> = left.iter().map(|(k, _)| *k).collect();
            if let Some(counts) =
                self.snapshot
                    .key_counts_walked(t, column, &values, self.graph, self.cancelled)
            {
                return counts;
            }
        }
        // Several triples: from their rows, as the store joins them (only those with the left
        // keys, when those are few). Counting a star per key reads every match of each triple,
        // which costs more than the join here, where the left side keeps few of them.
        if let GraphPattern::Bgp { patterns } = right
            && patterns.len() > 1
            && !reads_a_table(patterns)
        {
            if let Some(counts) = self.chain_counts(patterns, key, left.len()) {
                return Some(counts);
            }
            if let Some(counts) = self.star_counts(patterns, key, left) {
                return Some(counts);
            }
            let seed = Table {
                vars: vec![key.clone()],
                ids: left.iter().map(|(k, _)| TermId(*k)).collect(),
                maybe: vec![false],
                ones: left.len(),
            };
            let t = self.bgp_from(patterns, Some(&seed))?;
            return key_runs(&t, key);
        }
        self.key_counts(right, key)
    }

    /// `(value of key, solutions)` of `p`, sorted by value; `key` must be bound in every
    /// solution.
    fn key_counts(&self, p: &GraphPattern, key: &Variable) -> Option<Vec<(u64, u64)>> {
        if let GraphPattern::Union { left, right } = p {
            return add_counts(&self.key_counts(left, key)?, &self.key_counts(right, key)?);
        }
        if let GraphPattern::Bgp { patterns } = p
            && !reads_a_table(patterns)
        {
            let compiled = self.snapshot.compile_bgp(patterns, &[])?;
            if compiled.empty {
                return Some(Vec::new());
            }
            let column = compiled
                .columns
                .iter()
                .position(|c| c.as_ref() == Some(key))?;
            if let [t] = compiled.triples.as_slice() {
                return self
                    .snapshot
                    .key_counts(t, column, self.graph, self.cancelled);
            }
        }
        // From the rows. (Counting a star per key without its rows reads every match of each
        // triple: on RINF, `?s rdfs:label ?l ; a ?c` per `?s` took longer than the join.)
        key_runs(&self.eval(p)?, key)
    }

    /// [`Self::key_counts`] of a star `?key :p ?x . ?key :q ?y` for the keys of `left`: every
    /// triple has the key and shares no other variable, so the solutions of a key are the
    /// product of each triple's matches of it, each counted for those keys only (#165).
    fn star_counts(
        &self,
        patterns: &[spargebra::term::TriplePattern],
        key: &Variable,
        left: &[(u64, u64)],
    ) -> Option<Vec<(u64, u64)>> {
        use spargebra::term::TermPattern;
        let vars = |t: &spargebra::term::TriplePattern| -> Option<Vec<Variable>> {
            let mut out = Vec::new();
            for term in [&t.subject, &t.object] {
                match term {
                    TermPattern::Variable(v) => out.push(v.clone()),
                    TermPattern::NamedNode(_) | TermPattern::Literal(_) => {}
                    _ => return None,
                }
            }
            if let NamedNodePattern::Variable(v) = &t.predicate {
                out.push(v.clone());
            }
            Some(out)
        };
        let all: Vec<Vec<Variable>> = patterns.iter().map(vars).collect::<Option<_>>()?;
        for (i, vs) in all.iter().enumerate() {
            if vs.iter().filter(|v| *v == key).count() != 1 {
                return None;
            }
            let others = all
                .iter()
                .enumerate()
                .filter(|(k, _)| *k != i)
                .flat_map(|(_, o)| o.iter());
            if others.filter(|v| *v != key).any(|v| vs.contains(v)) {
                return None;
            }
        }
        let mut out: Option<Vec<(u64, u64)>> = None;
        for t in patterns {
            let one = GraphPattern::Bgp {
                patterns: vec![t.clone()],
            };
            let keys = out.as_deref().unwrap_or(left);
            let counts = self.right_key_counts(&one, key, keys)?;
            out = Some(match out {
                None => counts,
                Some(prev) => {
                    let mut next = Vec::with_capacity(prev.len().min(counts.len()));
                    let mut j = 0;
                    for (k, n) in prev {
                        while j < counts.len() && counts[j].0 < k {
                            j += 1;
                        }
                        if j < counts.len() && counts[j].0 == k {
                            next.push((k, n.checked_mul(counts[j].1)?));
                        }
                    }
                    next
                }
            });
            if out.as_ref().is_some_and(Vec::is_empty) {
                break;
            }
        }
        out
    }

    /// [`Self::key_counts`] of a chain `?key :p ?j . ?j :q ?x`: two triples that share one
    /// variable `j`, the key in the first only. The solutions per key are the sum, over the
    /// first triple's matches, of the second's matches of their `j`, which are counted once
    /// per value of `j` without building the joined rows (#165). The first triple is read
    /// whole, so `None` when it has many more matches than the `keys` the caller needs.
    fn chain_counts(
        &self,
        patterns: &[spargebra::term::TriplePattern],
        key: &Variable,
        keys: usize,
    ) -> Option<Vec<(u64, u64)>> {
        if patterns.len() != 2 {
            return None;
        }
        let compiled = self.snapshot.compile_bgp(patterns, &[])?;
        if compiled.empty {
            return Some(Vec::new());
        }
        let k = compiled
            .columns
            .iter()
            .position(|c| c.as_ref() == Some(key))?;
        let vars = |t: &IdTriple| -> Vec<usize> {
            [t.s, t.p, t.o]
                .iter()
                .filter_map(|s| match s {
                    IdSlot::Var(v) => Some(*v),
                    IdSlot::Const(_) => None,
                })
                .collect()
        };
        let [a, b] = compiled.triples.as_slice() else {
            return None;
        };
        let (a, b) = if vars(a).contains(&k) { (a, b) } else { (b, a) };
        let (va, vb) = (vars(a), vars(b));
        if !va.contains(&k) || vb.contains(&k) {
            return None;
        }
        let distinct = |v: &[usize]| v.iter().enumerate().all(|(i, x)| !v[..i].contains(x));
        let shared: Vec<usize> = va.iter().copied().filter(|v| vb.contains(v)).collect();
        let [j] = shared.as_slice() else {
            return None;
        };
        if !distinct(&va)
            || !distinct(&vb)
            || self.scan_estimate(std::slice::from_ref(a)) > keys as f64 * WALK
        {
            return None;
        }
        let rows = self.snapshot.join_ids(
            std::slice::from_ref(a),
            compiled.columns.len(),
            self.graph,
            self.cancelled,
        )?;
        let w = rows.width;
        let mut js: Vec<u64> = rows.ids.chunks_exact(w).map(|r| r[*j].0).collect();
        js.sort_unstable();
        js.dedup();
        let per_j = match self
            .snapshot
            .key_counts_walked(b, *j, &js, self.graph, self.cancelled)
        {
            Some(counts) => counts?,
            None => self
                .snapshot
                .key_counts_of(b, *j, &js, self.graph, self.cancelled)?,
        };
        let mut pairs: Vec<(u64, u64)> = Vec::with_capacity(rows.len());
        for r in rows.ids.chunks_exact(w) {
            if let Ok(i) = per_j.binary_search_by_key(&r[*j].0, |(v, _)| *v) {
                pairs.push((r[k].0, per_j[i].1));
            }
        }
        if !pairs.is_sorted_by_key(|(v, _)| *v) {
            pairs.sort_unstable_by_key(|(v, _)| *v);
        }
        let mut out: Vec<(u64, u64)> = Vec::new();
        for (v, n) in pairs {
            match out.last_mut() {
                Some((u, m)) if *u == v => *m = m.checked_add(n)?,
                _ => out.push((v, n)),
            }
        }
        Some(out)
    }
}

/// Two lists of `(value, count)` sorted by value, merged with the counts of a value added.
fn add_counts(a: &[(u64, u64)], b: &[(u64, u64)]) -> Option<Vec<(u64, u64)>> {
    let mut out = Vec::with_capacity(a.len().max(b.len()));
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        let next = match (a.get(i), b.get(j)) {
            (Some(x), Some(y)) if x.0 == y.0 => {
                i += 1;
                j += 1;
                (x.0, x.1.checked_add(y.1)?)
            }
            (Some(x), Some(y)) if x.0 < y.0 => {
                i += 1;
                *x
            }
            (Some(x), None) => {
                i += 1;
                *x
            }
            (_, Some(y)) => {
                j += 1;
                *y
            }
            (None, None) => unreachable!(),
        };
        out.push(next);
    }
    Some(out)
}

/// `(value of key, rows)` of `t`, sorted by value; `None` if `key` may be unbound.
fn key_runs(t: &Table, key: &Variable) -> Option<Vec<(u64, u64)>> {
    let c = t.col(key)?;
    if t.maybe[c] {
        return None;
    }
    let mut keys: Vec<u64> = (0..t.len()).map(|r| t.row(r)[c].0).collect();
    keys.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for k in keys {
        match out.last_mut() {
            Some((v, n)) if *v == k => *n += 1,
            _ => out.push((k, 1)),
        }
    }
    Some(out)
}

pub(crate) fn count(t: &Table, var: Option<&Variable>, distinct: bool) -> Option<u64> {
    match (var, distinct) {
        (None, false) => Some(t.len() as u64),
        (None, true) => {
            let mut rows: Vec<&[TermId]> = (0..t.len()).map(|r| t.row(r)).collect();
            rows.sort_unstable();
            rows.dedup();
            Some(rows.len() as u64)
        }
        (Some(v), d) => {
            let Some(c) = t.col(v) else {
                return Some(0);
            };
            let mut values: Vec<TermId> = (0..t.len())
                .map(|r| t.row(r)[c])
                .filter(|id| *id != UNBOUND)
                .collect();
            if d {
                values.sort_unstable();
                values.dedup();
            }
            Some(values.len() as u64)
        }
    }
}

/// Groups of `t` by the values of `keys` (a variable `t` doesn't have is unbound in every
/// row), each with the counts `wanted` asks for (as [`count`]), in no particular order. `None`
/// above `max_groups` groups.
pub(crate) fn group_counts(
    t: &Table,
    keys: &[Variable],
    wanted: &[(Option<&Variable>, bool)],
    max_groups: usize,
) -> Option<Vec<(Vec<TermId>, Vec<u64>)>> {
    let groups = group_rows(t, keys, max_groups)?;
    Some(
        (0..groups.len())
            .map(|g| {
                let rows = groups.rows(g);
                let counts = wanted
                    .iter()
                    .map(|&(v, distinct)| count_rows(t, rows, v, distinct))
                    .collect();
                (groups.key(g).to_vec(), counts)
            })
            .collect(),
    )
}

/// The rows of `t` (their numbers) per group of the values of `keys`, the groups in the order
/// they first come (one group without keys), counted on several cores for a large table (#185).
/// `None` above `max_groups` groups.
pub(crate) fn group_rows(t: &Table, keys: &[Variable], max_groups: usize) -> Option<Groups> {
    let width = keys.len();
    // Without keys, one group of all rows, also when there are none.
    if keys.is_empty() {
        return Some(Groups {
            width,
            keys: Vec::new(),
            starts: vec![0, t.len() as u32],
            rows: (0..t.len() as u32).collect(),
        });
    }
    let key_cols: Vec<Option<usize>> = keys.iter().map(|v| t.col(v)).collect();
    let key_of = |r: usize, key: &mut Vec<TermId>| {
        let row = t.row(r);
        key.clear();
        key.extend(key_cols.iter().map(|c| c.map_or(UNBOUND, |c| row[c])));
    };
    // A large table on several cores: split by key (#4).
    if t.len() >= tripletrail_store::par::PARALLEL_ROWS
        && tripletrail_store::par::query_threads() > 1
    {
        let (index, of) = partitioned_groups(t, &key_cols, max_groups)?;
        return Some(grouped(index, &of, width));
    }
    // Each chunk numbers its own groups, in the order they first come; its rows get the
    // number of their group.
    let parts = tripletrail_store::par::chunks(t.len(), |range| {
        let mut index = GroupIndex::new(width);
        let mut of = Vec::with_capacity(range.len());
        let mut key = Vec::with_capacity(width);
        for r in range {
            key_of(r, &mut key);
            of.push(index.find_or_insert(&key, max_groups)?);
        }
        Some((index, of))
    });
    let mut parts = parts.into_iter();
    let (mut index, mut of) = parts.next()??;
    // Then the groups of the later chunks are numbered on, chunk by chunk: as one pass in row
    // order would number them.
    for part in parts {
        let (local, local_of) = part?;
        let global: Vec<u32> = (0..local.len())
            .map(|g| index.find_or_insert(local.key(g as u32), max_groups))
            .collect::<Option<_>>()?;
        of.extend(local_of.into_iter().map(|g| global[g as usize]));
    }
    Some(grouped(index, &of, width))
}

/// The groups of `index`, whose rows are numbered by `of` (the group of each row).
fn grouped(index: GroupIndex, of: &[u32], width: usize) -> Groups {
    // The rows of each group, in row order, one after the other.
    let groups = index.len();
    let mut starts = vec![0u32; groups + 1];
    for &g in of {
        starts[g as usize + 1] += 1;
    }
    for g in 0..groups {
        starts[g + 1] += starts[g];
    }
    let mut next = starts.clone();
    let mut rows = vec![0u32; of.len()];
    for (r, &g) in of.iter().enumerate() {
        rows[next[g as usize] as usize] = r as u32;
        next[g as usize] += 1;
    }
    Groups {
        width,
        keys: index.keys,
        starts,
        rows,
    }
}

/// The groups of the rows of `t` by the values of columns `key_cols` on several cores (#4),
/// numbered as one pass in row order numbers them, and the group of each row: the rows split
/// by the hash of their key into parts, one group index per part, so no key is in two parts.
/// The groups of all parts are then numbered in the order of their first rows. `None` past
/// `max_groups` groups.
fn partitioned_groups(
    t: &Table,
    key_cols: &[Option<usize>],
    max_groups: usize,
) -> Option<(GroupIndex, Vec<u32>)> {
    use tripletrail_store::par;
    let width = key_cols.len();
    let n = t.len();
    let key_of = |r: usize, key: &mut Vec<TermId>| {
        let row = t.row(r);
        key.clear();
        key.extend(key_cols.iter().map(|c| c.map_or(UNBOUND, |c| row[c])));
    };
    let hashes: Vec<u64> = par::chunks(n, |range| {
        let mut key = Vec::with_capacity(width);
        range
            .map(|r| {
                key_of(r, &mut key);
                hash_ids(key.iter().copied())
            })
            .collect::<Vec<u64>>()
    })
    .concat();
    let parts = par::query_threads();
    let part_of = |h: u64| (h >> 40) as usize % parts;
    // Each part: its groups, the first row of each, and its rows with their group.
    let built = par::chunks_sized(parts, 0, 1, |range| {
        range
            .map(|p| {
                let mut index = GroupIndex::new(width);
                let mut firsts: Vec<u32> = Vec::new();
                let mut of: Vec<(u32, u32)> = Vec::new();
                let mut key = Vec::with_capacity(width);
                for (r, &h) in hashes.iter().enumerate() {
                    if part_of(h) != p {
                        continue;
                    }
                    key_of(r, &mut key);
                    let g = index.find_or_insert_hashed(&key, h, max_groups)?;
                    if g as usize == firsts.len() {
                        firsts.push(r as u32);
                    }
                    of.push((r as u32, g));
                }
                Some((index, firsts, of))
            })
            .collect::<Option<Vec<_>>>()
    });
    let built: Vec<_> = built
        .into_iter()
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    let total: usize = built.iter().map(|(index, ..)| index.len()).sum();
    if total > max_groups {
        return None;
    }
    // Every group by its first row: the order one pass gives them.
    let mut order: Vec<(u32, u32, u32)> = Vec::with_capacity(total);
    for (p, (_, firsts, _)) in built.iter().enumerate() {
        order.extend(
            firsts
                .iter()
                .enumerate()
                .map(|(g, &first)| (first, p as u32, g as u32)),
        );
    }
    par::sort_unstable(&mut order);
    let mut global: Vec<Vec<u32>> = built
        .iter()
        .map(|(index, ..)| vec![0; index.len()])
        .collect();
    let mut index = GroupIndex::new(width);
    index.keys.reserve(total * width);
    index.chain.reserve(total);
    for (number, &(_, p, g)) in order.iter().enumerate() {
        global[p as usize][g as usize] = number as u32;
        index.keys.extend_from_slice(built[p as usize].0.key(g));
        // The numbers are all `index` is read for after this (`len` and `key`).
        index.chain.push(END);
    }
    let mut of = vec![0u32; n];
    for (p, (_, _, rows)) in built.iter().enumerate() {
        for &(r, g) in rows {
            of[r as usize] = global[p][g as usize];
        }
    }
    Some((index, of))
}

/// The groups of a table, in the order they first come, without an allocation per group.
pub(crate) struct Groups {
    width: usize,
    /// The key of group `g` at `keys[g * width..]`.
    keys: Vec<TermId>,
    /// The rows of group `g` at `rows[starts[g]..starts[g + 1]]`.
    starts: Vec<u32>,
    rows: Vec<u32>,
}

impl Groups {
    pub(crate) fn len(&self) -> usize {
        self.starts.len() - 1
    }

    pub(crate) fn key(&self, g: usize) -> &[TermId] {
        &self.keys[g * self.width..(g + 1) * self.width]
    }

    pub(crate) fn rows(&self, g: usize) -> &[u32] {
        &self.rows[self.starts[g] as usize..self.starts[g + 1] as usize]
    }
}

/// Group numbers by key: per hash of a key its first group, chained to the next groups with
/// that hash.
struct GroupIndex {
    width: usize,
    heads: PreMap<u32>,
    chain: Vec<u32>,
    keys: Vec<TermId>,
}

impl GroupIndex {
    fn new(width: usize) -> Self {
        Self {
            width,
            heads: PreMap::default(),
            chain: Vec::new(),
            keys: Vec::new(),
        }
    }

    fn len(&self) -> usize {
        self.chain.len()
    }

    fn key(&self, g: u32) -> &[TermId] {
        let g = g as usize;
        &self.keys[g * self.width..(g + 1) * self.width]
    }

    /// The number of the group of `key`, a new one for a new key. `None` past `max_groups`.
    fn find_or_insert(&mut self, key: &[TermId], max_groups: usize) -> Option<u32> {
        self.find_or_insert_hashed(key, hash_ids(key.iter().copied()), max_groups)
    }

    /// [`GroupIndex::find_or_insert`] of a key whose hash is `hash`.
    fn find_or_insert_hashed(
        &mut self,
        key: &[TermId],
        hash: u64,
        max_groups: usize,
    ) -> Option<u32> {
        let head = self.heads.entry(hash).or_insert(END);
        let mut g = *head;
        while g != END {
            let at = g as usize * self.width;
            if self.keys[at..at + self.width] == *key {
                return Some(g);
            }
            g = self.chain[g as usize];
        }
        if self.chain.len() == max_groups {
            return None;
        }
        let g = self.chain.len() as u32;
        self.chain.push(*head);
        *head = g;
        self.keys.extend_from_slice(key);
        Some(g)
    }
}

/// `COUNT(*)` (`var` `None`) or `COUNT(?var)` over the rows `rows` of `t`.
pub(crate) fn count_rows(t: &Table, rows: &[u32], var: Option<&Variable>, distinct: bool) -> u64 {
    match (var.map(|v| t.col(v)), distinct) {
        (None, false) => rows.len() as u64,
        (None, true) => {
            let mut all: Vec<&[TermId]> = rows.iter().map(|&r| t.row(r as usize)).collect();
            all.sort_unstable();
            all.dedup();
            all.len() as u64
        }
        (Some(None), _) => 0,
        (Some(Some(c)), d) => {
            let mut values: Vec<TermId> = rows
                .iter()
                .map(|&r| t.row(r as usize)[c])
                .filter(|id| *id != UNBOUND)
                .collect();
            if d {
                values.sort_unstable();
                values.dedup();
            }
            values.len() as u64
        }
    }
}

/// `MIN`, `MAX` and `SAMPLE` of a variable over rows of a table, on ids.
pub(crate) struct Pick<'a> {
    snapshot: &'a Snapshot,
    terms: HashMap<TermId, oxigraph::model::Term>,
    /// The order of IRIs and strings: the snapshot's when they are built (else they are built
    /// in the background for later queries, and this compares by text until it has compared
    /// [`Pick::RANK_AFTER`] pairs, then waits for them).
    ranks: Option<std::sync::Arc<tripletrail_store::StringRanks>>,
    /// Pairs compared by their text so far, without ranks.
    by_text: usize,
    /// The numbers [`Self::extreme`] read as the evaluator does, `None` for other terms.
    numbers: HashMap<TermId, Option<spareval::ExpressionTerm>>,
}

impl<'a> Pick<'a> {
    pub(crate) fn new(snapshot: &'a Snapshot) -> Self {
        Self {
            snapshot,
            terms: HashMap::new(),
            ranks: None,
            by_text: 0,
            numbers: HashMap::new(),
        }
    }

    /// Pairs compared by their text before the ranks of all IRIs and strings are built:
    /// building them reads the whole dictionary (75 ms for 250,000 terms), more than a
    /// `MIN` over a few groups' rows costs by text (#10).
    const RANK_AFTER: usize = 1 << 18;

    /// `SAMPLE(?var)`: the first bound value (`UNBOUND` if there is none).
    pub(crate) fn sample(t: &Table, rows: &[u32], var: &Variable) -> TermId {
        let Some(c) = t.col(var) else {
            return UNBOUND;
        };
        rows.iter()
            .map(|&r| t.row(r as usize)[c])
            .find(|id| *id != UNBOUND)
            .unwrap_or(UNBOUND)
    }

    /// `MIN(?var)` (`max: false`) or `MAX(?var)` in SPARQL's order of terms, `UNBOUND` when
    /// a row has it unbound (the aggregate is an error then) or there are no rows. `None` for
    /// values this doesn't order on ids: blank nodes, quoted triples, literals other than
    /// strings, language strings, numbers, booleans and inline date-times, and literals of
    /// different kinds; and for the ties Jena breaks by text (#517): language strings with
    /// different tags, equal integers written differently. Decimals and doubles (with
    /// integers) are ordered as the evaluator orders them, ties included; date-times by
    /// instant, when they are all alike (with or without a time zone, dates or not).
    pub(crate) fn extreme(
        &mut self,
        t: &Table,
        rows: &[u32],
        var: &Variable,
        max: bool,
    ) -> Option<TermId> {
        let Some(c) = t.col(var) else {
            return Some(UNBOUND);
        };
        let mut best: Option<TermId> = None;
        for &r in rows {
            let id = t.row(r as usize)[c];
            if id == UNBOUND {
                return Some(UNBOUND);
            }
            // Inline date-times (#11), by instant: all `xsd:dateTime` or all `xsd:date`, all
            // with a time zone or all without (others aren't ordered by instant alone), and
            // two values at one instant written differently (`Z` and `+00:00`) left to the
            // evaluator, whose pick of those depends on their order.
            if id.kind() == Kind::DateTime || best.is_some_and(|b| b.kind() == Kind::DateTime) {
                let v = id.date_time_value()?;
                let Some(b) = best else {
                    best = Some(id);
                    continue;
                };
                let w = b.date_time_value()?;
                if (v.date, v.tz.is_some()) != (w.date, w.tz.is_some()) {
                    return None;
                }
                if b != id {
                    let o = v.instant.cmp(&w.instant);
                    if o.is_eq() {
                        return None;
                    }
                    if if max { o.is_gt() } else { o.is_lt() } {
                        best = Some(id);
                    }
                }
                continue;
            }
            // Decimals, doubles and floats, and integers with them, in the evaluator's own
            // order (Jena's: by value, then as written).
            if let Some(b) = best
                && b != id
                && (id.kind() != Kind::Integer || b.kind() != Kind::Integer)
                && let (Some(x), Some(y)) = (self.number(id), self.number(b))
            {
                let o = spareval::cmp_values_as_jena(&x, &y);
                let better = if max { o.is_gt() } else { o.is_lt() };
                best = Some(if better { id } else { b });
                continue;
            }
            best = Some(match best {
                None => {
                    if self.order_key(id).is_none() {
                        self.number(id)?;
                    }
                    id
                }
                Some(b) if b == id => b,
                Some(b) => {
                    let o = self.cmp(id, b)?;
                    // Jena (#517) sees language strings by tag first, and equal numbers by
                    // their text: leave those to the evaluator.
                    if o.is_eq() || (self.order_key(id) == Some(4) && self.tags_differ(id, b)?) {
                        return None;
                    }
                    let better = if max { o.is_gt() } else { o.is_lt() };
                    if better { id } else { b }
                }
            });
        }
        Some(best.unwrap_or(UNBOUND))
    }

    /// A number (an integer, decimal, double or float literal) as written, as the evaluator's
    /// `MIN` and `MAX` compare it (ties of equal values by their text, `"03"` before `3`);
    /// `None` for other terms.
    fn number(&mut self, id: TermId) -> Option<spareval::ExpressionTerm> {
        use spareval::ExpressionTerm as T;
        if !matches!(
            id.kind(),
            Kind::Integer | Kind::Decimal | Kind::Double | Kind::Typed
        ) {
            return None;
        }
        if let Some(n) = self.numbers.get(&id) {
            return n.clone();
        }
        let n = match self.snapshot.term(id).ok()? {
            oxigraph::model::Term::Literal(l) => Some(T::from_literal_as_written(l)),
            _ => None,
        }
        .filter(|t| {
            matches!(
                t.clone().value(),
                T::IntegerLiteral(_)
                    | T::DecimalLiteral(_)
                    | T::DoubleLiteral(_)
                    | T::FloatLiteral(_)
            )
        });
        self.numbers.insert(id, n.clone());
        n
    }

    /// `a` and `b` in the order of `ORDER BY` (unbound first), `None` for values this
    /// doesn't order on ids (as [`Self::extreme`]).
    pub(crate) fn order(&mut self, a: TermId, b: TermId) -> Option<std::cmp::Ordering> {
        use std::cmp::Ordering;
        Some(match (a == UNBOUND, b == UNBOUND) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ if a == b => Ordering::Equal,
            _ => self.cmp(a, b)?,
        })
    }

    /// The class of values `id` is ordered with by [`Self::order`]: IRIs (0), strings (1),
    /// integers (2), booleans (3) or language-tagged strings (4); `None` for others. Unbound
    /// values order with any class.
    pub(crate) fn order_class(&self, id: TermId) -> Option<u8> {
        self.order_key(id)
    }

    /// The rank of the kind of `id` in the order of terms (IRIs before literals), `None` for
    /// kinds this doesn't order.
    fn order_key(&self, id: TermId) -> Option<u8> {
        match id.kind() {
            Kind::Iri => Some(0),
            Kind::String => Some(1),
            Kind::Integer => Some(2),
            Kind::Boolean => Some(3),
            Kind::LangString => Some(4),
            _ => None,
        }
    }

    /// Whether two language strings have tags that differ (case aside).
    fn tags_differ(&mut self, a: TermId, b: TermId) -> Option<bool> {
        let tag = |t: &oxigraph::model::Term| match t {
            oxigraph::model::Term::Literal(l) => l.language().map(str::to_ascii_lowercase),
            _ => None,
        };
        let x = tag(self.term(a)?);
        Some(x != tag(self.term(b)?))
    }

    fn term(&mut self, id: TermId) -> Option<&oxigraph::model::Term> {
        use std::collections::hash_map::Entry;
        Some(match self.terms.entry(id) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => e.insert(self.snapshot.term(id).ok()?),
        })
    }

    /// Two different ids in SPARQL's order of terms.
    fn cmp(&mut self, a: TermId, b: TermId) -> Option<std::cmp::Ordering> {
        use oxigraph::model::Term;
        let (ka, kb) = (self.order_key(a)?, self.order_key(b)?);
        match (ka, kb) {
            (0, 0) => {}
            // An IRI comes before a literal.
            (0, _) | (_, 0) => return Some(ka.cmp(&kb)),
            // Literals of different kinds: not ordered here.
            _ if ka != kb => return None,
            _ => {}
        }
        // Integers are in their ids.
        if ka == 2
            && let (Some(x), Some(y)) = (a.integer_value(), b.integer_value())
        {
            return Some(x.cmp(&y));
        }
        // IRIs and strings by their ranks, else by their text, from the dictionary (language
        // strings by value, then language tag).
        if ka != 2 {
            if self.ranks.is_none() && self.by_text == 0 {
                self.ranks = self.snapshot.string_ranks_built();
                if self.ranks.is_none() {
                    self.snapshot.string_ranks_in_background();
                }
            }
            if self.ranks.is_none() {
                self.by_text += 1;
                if self.by_text > Self::RANK_AFTER {
                    self.ranks = Some(self.snapshot.string_ranks());
                }
            }
            if let Some(ranks) = &self.ranks
                && let (Some(x), Some(y)) = (ranks.rank(a), ranks.rank(b))
            {
                return Some(x.cmp(&y));
            }
        }
        if ka != 2
            && let Some(o) = self.snapshot.cmp_texts(a, b)
        {
            return Some(o);
        }
        self.term(a)?;
        self.term(b)?;
        Some(match (&self.terms[&a], &self.terms[&b]) {
            (Term::NamedNode(x), Term::NamedNode(y)) => x.as_str().cmp(y.as_str()),
            (Term::Literal(x), Term::Literal(y)) if ka == 2 => x
                .value()
                .parse::<i64>()
                .ok()?
                .cmp(&y.value().parse::<i64>().ok()?),
            // A language string with a base direction: not ordered here.
            (Term::Literal(x), Term::Literal(y))
                if x.direction().is_some() || y.direction().is_some() =>
            {
                return None;
            }
            // Strings by their characters, then language tag; booleans false before true.
            (Term::Literal(x), Term::Literal(y)) => {
                (x.value(), x.language()).cmp(&(y.value(), y.language()))
            }
            _ => return None,
        })
    }
}

/// A key that tells computed values apart exactly (the type and the value), for the ids of
/// `BIND`'s values; `None` for dates.
/// The columns `variables` of `t`, in that order; one `t` doesn't have is unbound.
fn project(t: Table, variables: &[Variable]) -> Table {
    let cols: Vec<Option<usize>> = variables.iter().map(|v| t.col(v)).collect();
    let width = t.width();
    let rows = t.len();
    let mut ids = Vec::with_capacity(rows * cols.len());
    for r in 0..rows {
        let row = &t.ids[r * width..(r + 1) * width];
        ids.extend(cols.iter().map(|c| c.map_or(TermId(0), |c| row[c])));
    }
    Table {
        vars: variables.to_vec(),
        ids,
        maybe: cols
            .iter()
            .map(|c| c.is_none_or(|c| t.maybe[c]) && rows > 0)
            .collect(),
        ones: rows,
    }
}

fn value_key(v: crate::idvalue::Value) -> Option<(u8, i128)> {
    use crate::idvalue::Value as V;
    Some(match v {
        V::Integer(i) => (0, i128::from(i)),
        V::Decimal(d) => (1, d),
        V::Double(d) => (2, i128::from(d.to_bits())),
        V::DateTime { .. } => return None,
    })
}

/// `e` with `variable` replaced by `by`, when `variable` occurs in it only as a value (not
/// under `BOUND` or `EXISTS`); `None` if it doesn't occur or can't be replaced.
fn substituted(e: &Expression, variable: &Variable, by: &Expression) -> Option<Expression> {
    fn walk(
        e: &mut Expression,
        variable: &Variable,
        by: &Expression,
        found: &mut bool,
        ok: &mut bool,
    ) {
        match e {
            Expression::Variable(v) if v == variable => {
                *e = by.clone();
                *found = true;
            }
            Expression::Bound(v) if v == variable => *ok = false,
            Expression::Exists(_) => *ok = false,
            _ => crate::udf::expression_children(e, &mut |c| walk(c, variable, by, found, ok)),
        }
    }
    let mut e = e.clone();
    let (mut found, mut ok) = (false, true);
    walk(&mut e, variable, by, &mut found, &mut ok);
    (found && ok).then_some(e)
}

#[cfg(test)]
mod tests {
    use super::effective_boolean_value;
    use oxigraph::model::vocab::xsd;
    use oxigraph::model::{Literal, NamedNode, Term};

    fn ebv(value: &str, datatype: oxigraph::model::NamedNodeRef<'_>) -> Option<Option<bool>> {
        effective_boolean_value(Literal::new_typed_literal(value, datatype).into())
    }

    #[test]
    fn effective_boolean_values_are_the_evaluators() {
        assert_eq!(ebv("", xsd::STRING), Some(Some(false)));
        assert_eq!(ebv("a", xsd::STRING), Some(Some(true)));
        assert_eq!(ebv("1", xsd::BOOLEAN), Some(Some(true)));
        assert_eq!(ebv("0", xsd::BOOLEAN), Some(Some(false)));
        assert_eq!(ebv("yes", xsd::BOOLEAN), None);
        assert_eq!(ebv("0", xsd::INTEGER), Some(Some(false)));
        assert_eq!(ebv("-02", xsd::INTEGER), Some(Some(true)));
        assert_eq!(ebv("0.000", xsd::DECIMAL), Some(Some(false)));
        assert_eq!(ebv("0.5", xsd::DECIMAL), Some(Some(true)));
        assert_eq!(ebv("NaN", xsd::DOUBLE), Some(Some(false)));
        assert_eq!(ebv("-0e0", xsd::DOUBLE), Some(Some(false)));
        assert_eq!(ebv("INF", xsd::DOUBLE), Some(Some(true)));
        // Forms the type doesn't allow are the evaluator's.
        assert_eq!(ebv("1e3", xsd::INTEGER), None);
        assert_eq!(ebv("1e3", xsd::DECIMAL), None);
        assert_eq!(ebv("1", xsd::INT), None);
        assert_eq!(
            effective_boolean_value(
                Literal::new_language_tagged_literal_unchecked("a", "en").into()
            ),
            Some(None)
        );
        assert_eq!(
            effective_boolean_value(Term::from(NamedNode::new_unchecked("http://e/x"))),
            Some(None)
        );
    }
}
