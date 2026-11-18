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
/// The predicate of `<p>+`, `^<p>+` (also `(^<p>)+`) and `<p>*` forms, whether it is
/// followed backwards, and whether a zero-length path counts.
pub(crate) fn closure_path(
    path: &PropertyPathExpression,
) -> Option<(&spargebra::term::NamedNode, bool, bool)> {
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
    match inner {
        P::NamedNode(n) => Some((n, outer_reverse, zero)),
        P::Reverse(p) => match &**p {
            P::NamedNode(n) => Some((n, !outer_reverse, zero)),
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
            } => self.path(subject, path, object, None)?,
            _ => return None,
        };
        (t.ids.len() <= max_ids()).then_some(t)
    }

    /// `?s <p>+ ?o`, `?s ^<p>+ ?o`, and with a constant end `<p>*` too (#11): the pairs of
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
        let (predicate, reverse, zero) = closure_path(path)?;
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
        if zero && starts.is_none() {
            // A zero-length path between two free variables: every term of the graph.
            return None;
        }
        let ids = match starts {
            Some(starts) if (starts.len() as f64) * LEVEL_COST < self.edge_estimate(predicate) => {
                self.closure_by_levels(predicate, forward, zero, &starts, from.is_err())?
            }
            starts => self.closure_of_all(predicate, forward, zero, starts, from.is_err())?,
        };
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

    fn edge_estimate(&self, predicate: &spargebra::term::NamedNode) -> f64 {
        match self.snapshot.term_id(predicate.as_ref().into()) {
            Some(p) => self.snapshot.estimate_ids(None, Some(p), None, self.graph) as f64,
            None => 0.0,
        }
    }

    /// The pattern of an edge followed from `__path_a` to `__path_b`.
    fn edge(
        predicate: &spargebra::term::NamedNode,
        forward: bool,
    ) -> (spargebra::term::TriplePattern, Variable, Variable) {
        let (a, b) = (
            Variable::new_unchecked("__path_a"),
            Variable::new_unchecked("__path_b"),
        );
        let (s, o) = if forward { (&a, &b) } else { (&b, &a) };
        let t = spargebra::term::TriplePattern {
            subject: s.clone().into(),
            predicate: NamedNodePattern::NamedNode(predicate.clone()),
            object: o.clone().into(),
        };
        (t, a, b)
    }

    /// The closure from a few starts: the edges of each level's nodes looked up by their
    /// values. Rows are (start, end), or the ends alone without `pairs`.
    fn closure_by_levels(
        &self,
        predicate: &spargebra::term::NamedNode,
        forward: bool,
        zero: bool,
        starts: &[TermId],
        pairs: bool,
    ) -> Option<Vec<TermId>> {
        let (pattern, a, b) = Self::edge(predicate, forward);
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
            let level = Table {
                vars: vec![a.clone()],
                ones: nodes.len(),
                ids: nodes,
                maybe: vec![false],
            };
            let edges = self.bgp_from(std::slice::from_ref(&pattern), Some(&level))?;
            let (ca, cb) = (edges.col(&a)?, edges.col(&b)?);
            let mut next_of: HashMap<TermId, Vec<TermId>> = HashMap::new();
            for r in 0..edges.len() {
                let row = edges.row(r);
                next_of.entry(row[ca]).or_default().push(row[cb]);
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
        Some(ids)
    }

    /// The closure from `starts`, or from every node with an edge, over all the predicate's
    /// edges read once.
    fn closure_of_all(
        &self,
        predicate: &spargebra::term::NamedNode,
        forward: bool,
        zero: bool,
        starts: Option<Vec<TermId>>,
        pairs_out: bool,
    ) -> Option<Vec<TermId>> {
        let (pattern, a, b) = Self::edge(predicate, forward);
        let edges = self.bgp(std::slice::from_ref(&pattern))?;
        let (ca, cb) = (edges.col(&a)?, edges.col(&b)?);
        let mut pairs: Vec<(TermId, TermId)> = (0..edges.len())
            .map(|r| {
                let row = edges.row(r);
                (row[ca], row[cb])
            })
            .collect();
        drop(edges);
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
        let limit = max_ids();
        let mut ids: Vec<TermId> = Vec::new();
        let mut seen: HashMap<TermId, usize> = HashMap::new();
        let mut stack: Vec<TermId> = Vec::new();
        for (i, &start) in starts.iter().enumerate() {
            if i % 4096 == 0 && (self.cancelled)() {
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
            } => {
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
                let parts = tripletrail_store::par::chunks(t.len(), |range| {
                    let kernel = self.kernel(expr, &t, range.clone())?;
                    let mut part = empty_like(&t);
                    for r in range {
                        let row = t.row(r);
                        // An error filters the row out, as false does.
                        if kernel(row)? == Some(true) {
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
                let (a, b) = (self.operand(a, t)?, self.operand(b, t)?);
                Box::new(move |row| {
                    let (x, y) = match (a.get(row), b.get(row)) {
                        (Some(x), Some(y)) => (x, y),
                        _ => return Some(None),
                    };
                    if x == y {
                        return Some(Some(true));
                    }
                    // Two different triple terms are equal when their parts are, and a
                    // literal part compares by value (`<<( :a :b 1 )>> = <<( :a :b 1.0 )>>`):
                    // the ids don't decide it.
                    if x.kind() == Kind::Triple && y.kind() == Kind::Triple {
                        return None;
                    }
                    match (is_literal(x), is_literal(y)) {
                        // Different literals may be equal values (1 and 1.0): not on ids,
                        // except for kinds whose values are their ids, and inline numbers
                        // and dates, compared by value.
                        (true, true) if same_value_kind(x, y) => Some(Some(false)),
                        (true, true) => {
                            let (x, y) = (crate::idvalue::of_id(x)?, crate::idvalue::of_id(y)?);
                            Some(crate::idvalue::compare(x, y)?.map(|o| o.is_eq()))
                        }
                        _ => Some(Some(false)),
                    }
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
                        _ => false,
                    }))
                })
            }
            E::FunctionCall(
                Function::Regex | Function::StrStarts | Function::Contains | Function::StrEnds,
                _,
            ) if string_prefix(expr).is_none() => {
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
            _ => return None,
        })
    }
    /// The rows of `t` with (`keep`) or without a solution of `p` compatible with them.
    fn exists(&self, t: &Table, p: &GraphPattern, keep: bool) -> Option<Table> {
        // Substituting a row's values into a basic graph pattern is a join with it.
        let GraphPattern::Bgp { .. } = p else {
            return None;
        };
        let inner = self.right(t, p)?;
        let keys = shared(t, &inner)?;
        semi(t, &inner, &keys, keep, self.cancelled)
    }
}

impl<'a> Eval<'a> {
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

    /// The literal of a dictionary id (`None`: it can't be decoded).
    fn literal(&self, id: TermId) -> Option<oxigraph::model::Literal> {
        let mut terms = self.terms.borrow_mut();
        let term = match terms.entry(id) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => e.insert(self.snapshot.term(id).ok()?),
        };
        match term {
            oxigraph::model::Term::Literal(l) => Some(l.clone()),
            _ => None,
        }
    }

    /// `LANG(?v)`: `Some(None)` an error (unbound, not a literal).
    fn language(&self, id: Option<TermId>) -> Option<Option<String>> {
        let Some(id) = id.filter(|id| *id != UNBOUND) else {
            return Some(None);
        };
        Some(match id.kind() {
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
/// `+ - *` and the number and date functions it knows.
pub(crate) fn value_expression(e: &Expression) -> bool {
    use Expression as E;
    match e {
        E::Variable(_) | E::Literal(_) => true,
        E::Add(a, b) | E::Subtract(a, b) | E::Multiply(a, b) => {
            value_expression(a) && value_expression(b)
        }
        E::UnaryMinus(a) => value_expression(a),
        E::FunctionCall(
            Function::Abs
            | Function::Ceil
            | Function::Floor
            | Function::Round
            | Function::Year
            | Function::Month
            | Function::Day
            | Function::Hours
            | Function::Minutes,
            args,
        ) => matches!(args.as_slice(), [a] if value_expression(a)),
        // The length of a string, and a text test cast to 0 or 1 (#165).
        E::FunctionCall(Function::StrLen, args) => {
            matches!(args.as_slice(), [a] if crate::idvalue::text_part(a).is_some())
        }
        E::FunctionCall(Function::Custom(f), args)
            if f.as_ref() == oxigraph::model::vocab::xsd::INTEGER =>
        {
            matches!(args.as_slice(), [a] if text_test(a).is_some())
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
    use Expression as E;
    let var = |a: &[E]| matches!(a, [E::Variable(_)]);
    let operand = |e: &E| matches!(e, E::Variable(_) | E::NamedNode(_) | E::Literal(_));
    match e {
        E::Exists(p) => matches!(**p, GraphPattern::Bgp { .. }),
        E::Not(e) => filter_covered(e),
        E::And(a, b) => filter_covered(a) && filter_covered(b),
        // An EXISTS is a filter of its own, not a value inside `||`.
        E::Or(a, b) => !has_exists(a) && !has_exists(b) && filter_covered(a) && filter_covered(b),
        E::Bound(_) => true,
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
        ) => string_prefix(e).is_some() || text_test(e).is_some(),
        _ => false,
    }
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

/// `STRSTARTS`, `STRENDS` or `CONTAINS` of a variable and a simple literal, or `REGEX` of a
/// variable with a pattern and flags that are simple literals: the variable and the test of
/// its text. Each is an error for a value that is not a string literal (simple, `xsd:string`
/// or language-tagged), and true or false otherwise: the second argument has no language tag,
/// so it is compatible with every string. The regular expression is built as the evaluator
/// builds it (XPath flags `s`, `m`, `i`, `x` and `q`); `None` for one it doesn't build.
pub(crate) fn text_test(e: &Expression) -> Option<(&Variable, TextTest)> {
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
        (Function::StrStarts, [E::Variable(v), x]) => {
            Some((v, TextTest::Starts(simple(x)?.to_owned())))
        }
        (Function::StrEnds, [E::Variable(v), x]) => {
            Some((v, TextTest::Ends(simple(x)?.to_owned())))
        }
        (Function::Contains, [E::Variable(v), x]) => {
            Some((v, TextTest::Contains(simple(x)?.to_owned())))
        }
        (Function::Regex, [E::Variable(v), p, rest @ ..]) if rest.len() <= 1 => {
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

fn join(a: &Table, b: &Table, cancelled: &(dyn Fn() -> bool + Sync)) -> Option<Table> {
    let keys = shared(a, b)?;
    let (vars, maybe, extra) = combined(a, b);
    let index = KeyIndex::new(b, keys.iter().map(|k| k.1).collect());
    let probe_cols: Vec<usize> = keys.iter().map(|k| k.0).collect();
    probe(a, vars, maybe, cancelled, |row, out| {
        for m in index.matches(row, &probe_cols) {
            if !out.room() {
                return;
            }
            out.ids.extend_from_slice(row);
            out.ids.extend(extra.iter().map(|&j| b.row(m)[j]));
            out.ones += 1;
        }
    })
}

fn left_join(a: &Table, b: &Table, cancelled: &(dyn Fn() -> bool + Sync)) -> Option<Table> {
    let keys = shared(a, b)?;
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
            out.ids.extend_from_slice(row);
            out.ids.extend(extra.iter().map(|&j| b.row(m)[j]));
            out.ones += 1;
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
    let keys = shared(a, b)?;
    // No shared variable: nothing is removed.
    if keys.is_empty() {
        let mut out = empty_like(a);
        out.ids = a.ids.clone();
        out.ones = a.len();
        return Some(out);
    }
    semi(a, b, &keys, false, cancelled)
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
    // The rows of each group, in row order, one after the other.
    let groups = index.len();
    let mut starts = vec![0u32; groups + 1];
    for &g in &of {
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
    Some(Groups {
        width,
        keys: index.keys,
        starts,
        rows,
    })
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
        let head = self
            .heads
            .entry(hash_ids(key.iter().copied()))
            .or_insert(END);
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
    /// The order of IRIs and strings, fetched on first use.
    ranks: Option<std::sync::Arc<tripletrail_store::StringRanks>>,
}

impl<'a> Pick<'a> {
    pub(crate) fn new(snapshot: &'a Snapshot) -> Self {
        Self {
            snapshot,
            terms: HashMap::new(),
            ranks: None,
        }
    }

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
    /// strings, language strings, integers and booleans, and literals of different kinds; and
    /// for the ties Jena breaks by text (#517): language strings with different tags, equal
    /// integers written differently.
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
            best = Some(match best {
                None => {
                    self.order_key(id)?;
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
            let ranks = self
                .ranks
                .get_or_insert_with(|| self.snapshot.string_ranks());
            if let (Some(x), Some(y)) = (ranks.rank(a), ranks.rank(b)) {
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
