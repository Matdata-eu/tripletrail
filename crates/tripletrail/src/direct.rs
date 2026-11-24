//! Whole `SELECT` queries answered from tables of term ids (#11, phase 2): when the id
//! evaluator ([`crate::idtree`]) covers the pattern, and what is around it is a projection,
//! `DISTINCT` or `REDUCED`, an `ORDER BY` of variables it orders on ids and a slice, the
//! solutions are written straight from the table, each distinct id decoded to its term once.
//! The evaluator ([`spareval`]) is not involved. Anything else (another operator, a value
//! the ids don't order, a table too large) returns `None`, and the query runs as before.
//!
//! On while the executor is `ids` (`--executor ids`, `#pragma tt.executor ids`); `spareval`,
//! the default until the oracle over the test suites is green, keeps the evaluator for all.

use crate::idtree::{Eval, Table};
use oxigraph::model::Term;
use oxigraph::sparql::Variable;
use spargebra::Query;
use spargebra::algebra::{Expression, GraphPattern, OrderExpression};
use std::collections::HashMap;
use std::sync::Arc;
use tripletrail_store::TermId;

/// The answer of `query`: its variables and rows. `None` when the ids don't answer it.
pub(crate) fn answer(query: &Query, eval: &Eval) -> Option<IdSolutions> {
    let Query::Select {
        dataset: None,
        pattern,
        ..
    } = query
    else {
        return None;
    };
    let s = select(pattern, eval, true)?;
    decode(s.variables.into(), &s.table, &s.columns, &s.rows, eval)
}

/// A sub-select with `DISTINCT`, `REDUCED`, an `ORDER BY` or a slice inside (#11), as a table
/// of its variables, its rows in order. `None` when the ids don't answer it.
pub(crate) fn sub_select(pattern: &GraphPattern, eval: &Eval) -> Option<Table> {
    let s = select(pattern, eval, false)?;
    let width = s.columns.len();
    let mut ids = Vec::with_capacity(s.rows.len() * width);
    for &r in &s.rows {
        let row = s.table.row(r);
        ids.extend(s.columns.iter().map(|c| c.map_or(TermId(0), |c| row[c])));
    }
    let maybe = s
        .columns
        .iter()
        .map(|c| c.is_none_or(|c| s.table.maybe[c]) && !s.rows.is_empty())
        .collect();
    Some(Table {
        vars: s.variables,
        ids,
        maybe,
        ones: s.rows.len(),
    })
}

/// The rows a `SELECT`'s projection, `DISTINCT`, order and slice keep, of the table of its
/// pattern.
struct Selected {
    variables: Vec<Variable>,
    table: Table,
    /// The column of `table` of each variable.
    columns: Vec<Option<usize>>,
    /// The rows of `table` kept, in order.
    rows: Vec<usize>,
}

/// `pattern` as a `SELECT`'s slice, `DISTINCT` or `REDUCED`, projection and order around a
/// pattern the ids cover. `top`: the whole query (a bare projection is taken too; inside a
/// pattern the `Project` arm of [`Eval::eval`] takes it).
fn select(pattern: &GraphPattern, eval: &Eval, top: bool) -> Option<Selected> {
    use GraphPattern as G;
    let (start, length, inner) = match pattern {
        G::Slice {
            inner,
            start,
            length,
        } => (*start, *length, &**inner),
        other => (0, None, other),
    };
    let (distinct, inner) = match inner {
        G::Distinct { inner } | G::Reduced { inner } => (true, &**inner),
        other => (false, other),
    };
    let G::Project { inner, variables } = inner else {
        return None;
    };
    let (order, body) = match &**inner {
        G::OrderBy { inner, expression } => (Some(expression), &**inner),
        other => (None, other),
    };
    if !top && !distinct && order.is_none() && length.is_none() && start == 0 {
        return None;
    }
    // Under a limit without an order the evaluator stops after the first rows (faster); in a
    // sub-select, the rows any slice without an order keeps are the evaluator's to choose.
    if (length.is_some() || (!top && start > 0)) && order.is_none() {
        return None;
    }
    // The order's keys as variables: an expression the kernels compute (`DESC(?a * ?q)`)
    // becomes a column of the body, under a name of its own (#11).
    let mut body = std::borrow::Cow::Borrowed(body);
    let mut by: Vec<(Variable, bool)> = Vec::new();
    for (i, e) in order.into_iter().flatten().enumerate() {
        let (e, descending) = match e {
            OrderExpression::Asc(e) => (e, false),
            OrderExpression::Desc(e) => (e, true),
        };
        match e {
            // An order on a computed `BIND` value: a number's id orders when it is inline, as
            // for an expression computed here (#11); other computed values (strings, made-up
            // ids) don't, so the evaluator answers those, before any work here.
            Expression::Variable(v) if computed(&body, v) => {
                if !bound_by(&body, v).is_some_and(number_computable) {
                    return None;
                }
                by.push((v.clone(), descending));
            }
            Expression::Variable(v) => by.push((v.clone(), descending)),
            e if computable(e, true) => {
                let key = Variable::new_unchecked(format!("__tripletrail_order_{i}"));
                body = std::borrow::Cow::Owned(G::Extend {
                    inner: Box::new(body.into_owned()),
                    variable: key.clone(),
                    expression: e.clone(),
                });
                by.push((key, descending));
            }
            _ => return None,
        }
    }
    if !covered(&body) {
        return None;
    }
    let t = eval.eval(&body)?;
    let rows: Vec<usize> = match order {
        Some(_) => {
            // The rows the slice keeps in order (under DISTINCT, the first row of each
            // combination of the projected variables); the slice itself is taken below.
            let keep = length.map_or(t.len(), |l| start.saturating_add(l).min(t.len()));
            let by = by.iter().map(|(v, d)| (v, *d)).collect();
            let kept = distinct.then_some(variables.as_slice());
            crate::rows::first_rows(&t, by, kept, keep, eval.snapshot)?
        }
        None => (0..t.len()).collect(),
    };
    let columns: Vec<Option<usize>> = variables.iter().map(|v| t.col(v)).collect();
    let mut seen = std::collections::HashSet::new();
    let picked = rows.into_iter().filter(|&r| {
        !distinct
            || seen.insert(
                columns
                    .iter()
                    .map(|c| c.map(|c| t.row(r)[c]))
                    .collect::<Vec<_>>(),
            )
    });
    let rows: Vec<usize> = match length {
        Some(l) => picked.skip(start).take(l).collect(),
        None => picked.skip(start).collect(),
    };
    Some(Selected {
        variables: variables.clone(),
        table: t,
        columns,
        rows,
    })
}

/// The expression a `BIND` of `p` binds `v` to, if one does.
fn bound_by<'p>(p: &'p GraphPattern, v: &Variable) -> Option<&'p Expression> {
    use GraphPattern as G;
    match p {
        G::Extend {
            inner,
            variable,
            expression,
        } => {
            if variable == v {
                Some(expression)
            } else {
                bound_by(inner, v)
            }
        }
        G::Join { left, right }
        | G::Minus { left, right }
        | G::Union { left, right }
        | G::LeftJoin { left, right, .. } => bound_by(left, v).or_else(|| bound_by(right, v)),
        G::Filter { inner, .. }
        | G::Project { inner, .. }
        | G::Graph { inner, .. }
        | G::Distinct { inner }
        | G::Reduced { inner }
        | G::OrderBy { inner, .. }
        | G::Slice { inner, .. } => bound_by(inner, v),
        _ => None,
    }
}

/// Whether `v` is bound by a `BIND` of `p` to a computed value (not a copy or a constant).
fn computed(p: &GraphPattern, v: &Variable) -> bool {
    use GraphPattern as G;
    match p {
        G::Extend {
            inner,
            variable,
            expression,
        } => {
            (variable == v
                && !matches!(
                    expression,
                    Expression::Variable(_) | Expression::NamedNode(_) | Expression::Literal(_)
                ))
                || computed(inner, v)
        }
        G::Join { left, right }
        | G::Minus { left, right }
        | G::Union { left, right }
        | G::LeftJoin { left, right, .. } => computed(left, v) || computed(right, v),
        G::Filter { inner, .. }
        | G::Project { inner, .. }
        | G::Graph { inner, .. }
        | G::Distinct { inner }
        | G::Reduced { inner }
        | G::OrderBy { inner, .. }
        | G::Slice { inner, .. } => computed(inner, v),
        _ => false,
    }
}

/// Whether the value kernels (`idvalue::compile`) may compute `e`, inside an expression.
pub(crate) fn number_computable(e: &Expression) -> bool {
    computable(e, false)
}

/// Whether the expression kernels (`idvalue::compile`) may compute `e`, by its form alone: so
/// that a `BIND` they can't compute leaves the query to the evaluator before any work on ids.
/// `top`: a constant or a copy is fine there (`BIND(ex:a AS ?k)`), not inside arithmetic.
fn computable(e: &Expression, top: bool) -> bool {
    use spargebra::algebra::Function as F;
    match e {
        Expression::Variable(_) => true,
        Expression::Literal(_) => true,
        Expression::NamedNode(_) => top,
        Expression::Add(a, b)
        | Expression::Subtract(a, b)
        | Expression::Multiply(a, b)
        | Expression::Divide(a, b) => computable(a, false) && computable(b, false),
        Expression::UnaryMinus(a) => computable(a, false),
        // Branches that are numbers: a bare variable or a string constant may be any term, and
        // then the term kernels (`idstring`) take the expression.
        Expression::If(c, a, b) => {
            crate::idvalue::is_condition(c, &|x| computable(x, false)) && branch(a) && branch(b)
        }
        Expression::Coalesce(list) => !list.is_empty() && list.iter().all(branch),
        Expression::FunctionCall(
            F::Abs
            | F::Ceil
            | F::Floor
            | F::Round
            | F::Year
            | F::Month
            | F::Day
            | F::Hours
            | F::Minutes
            | F::Seconds,
            args,
        ) => args.len() == 1 && computable(&args[0], false),
        // `STRLEN` and `xsd:integer` of a text test read a variable's string.
        Expression::FunctionCall(F::StrLen, args) => {
            args.len() == 1 && !matches!(args[0], Expression::Literal(_))
        }
        Expression::FunctionCall(F::Custom(f), args)
            if f.as_ref() == oxigraph::model::vocab::xsd::INTEGER =>
        {
            args.len() == 1
                && (crate::idtree::text_test(&args[0]).is_some() || computable(&args[0], false))
        }
        // `xsd:decimal` of a number.
        Expression::FunctionCall(F::Custom(f), args)
            if f.as_ref() == oxigraph::model::vocab::xsd::DECIMAL =>
        {
            args.len() == 1 && computable(&args[0], false)
        }
        _ => false,
    }
}

/// A branch of `IF` or `COALESCE` the value kernels compute: a number. A constant is the
/// term the branch gives as written, so it must be in the form the value kernels write
/// (`2.5`, not `2.50`); a bare variable or another constant may be any term, and then the
/// term kernels (`idstring`) take the expression.
fn branch(e: &Expression) -> bool {
    use oxigraph::model::vocab::xsd;
    match e {
        Expression::Variable(_) => false,
        Expression::Literal(l) => {
            let v = l.value();
            let d = l.datatype();
            if d == xsd::INTEGER {
                v.parse::<i64>().is_ok_and(|i| i.to_string() == v)
            } else if d == xsd::DECIMAL {
                v.parse::<oxsdatatypes::Decimal>()
                    .is_ok_and(|x| spareval::decimal_lexical(x) == v)
            } else if d == xsd::DOUBLE {
                v.parse::<oxsdatatypes::Double>()
                    .is_ok_and(|x| x.is_finite() && x.to_string() == v)
            } else {
                false
            }
        }
        e => computable(e, false),
    }
}

/// Whether the id tables may answer `p`: what [`crate::rows::covered`] covers, and `BIND`
/// over it (`idtree::Eval::extend` decides the expression).
pub(crate) fn covered(p: &GraphPattern) -> bool {
    use GraphPattern as G;
    match p {
        G::Extend {
            inner, expression, ..
        } => {
            (computable(expression, true) || crate::idstring::computable(expression))
                && covered(inner)
        }
        G::Project { inner, .. }
        | G::Graph { inner, .. }
        | G::Distinct { inner }
        | G::Reduced { inner }
        | G::OrderBy { inner, .. }
        | G::Slice { inner, .. } => covered(inner),
        G::Values { bindings, .. } => bindings
            .iter()
            .flatten()
            .flatten()
            .all(|t| !matches!(t, spargebra::term::GroundTerm::Triple(_))),
        G::Join { left, right }
        | G::Minus { left, right }
        | G::Union { left, right }
        | G::LeftJoin {
            left,
            right,
            expression: None,
        } => covered(left) && covered(right),
        G::LeftJoin {
            left,
            right,
            expression: Some(e),
        } => crate::idtree::filter_covered(e) && covered(left) && covered(right),
        G::Filter { expr, inner } => crate::idtree::filter_covered(expr) && covered(inner),
        other => crate::rows::covered(other),
    }
}

/// `answer` as the results of a query: the solutions as terms, with `answer` attached for
/// writers that write it from its ids.
pub(crate) fn results(answer: Arc<IdSolutions>) -> spareval::QueryResults<'static> {
    let variables: Arc<[Variable]> = answer.variables().into();
    spareval::QueryResults::Solutions(
        spareval::QuerySolutionIter::from_tuples(variables, Arc::clone(&answer).rows().map(Ok))
            .with_attachment(answer),
    )
}

/// The solutions of a query answered on ids: each distinct term of the answer once, and the
/// rows as numbers of those terms. Writers of results write each term once and copy it
/// where it occurs; [`IdSolutions::rows`] gives the solutions as terms for anything else.
pub struct IdSolutions {
    variables: Arc<[Variable]>,
    terms: Vec<Term>,
    /// `variables.len()` cells per row, each a number into `terms` or [`IdSolutions::UNBOUND`].
    cells: Vec<u32>,
    len: usize,
}

impl IdSolutions {
    /// A cell with no value.
    pub const UNBOUND: u32 = u32::MAX;

    pub fn variables(&self) -> &[Variable] {
        &self.variables
    }

    /// The distinct terms of the answer, numbered as the cells number them.
    pub fn terms(&self) -> &[Term] {
        &self.terms
    }

    /// The number of solutions.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Solution `i`: one cell per variable, as numbers into [`IdSolutions::terms`].
    pub fn row(&self, i: usize) -> &[u32] {
        let width = self.variables.len();
        &self.cells[i * width..(i + 1) * width]
    }

    /// The memory it takes, about.
    pub(crate) fn bytes(&self) -> usize {
        self.cells.len() * 4
            + self
                .terms
                .iter()
                .map(crate::cache::term_bytes)
                .sum::<usize>()
    }

    /// The solutions as terms, in order.
    pub fn rows(self: Arc<Self>) -> impl Iterator<Item = Vec<Option<Term>>> {
        (0..self.len).map(move |i| {
            self.row(i)
                .iter()
                .map(|&n| (n != Self::UNBOUND).then(|| self.terms[n as usize].clone()))
                .collect()
        })
    }
}

/// The rows `picked` of `t`, the columns `columns` of each (an unbound cell or a variable `t`
/// doesn't have as unbound), each distinct id decoded to its term once: in chunks on the
/// query threads, each under one lock of the dictionary.
fn decode(
    variables: Arc<[Variable]>,
    t: &Table,
    columns: &[Option<usize>],
    picked: &[usize],
    eval: &Eval,
) -> Option<IdSolutions> {
    // Each distinct id once, numbered in the order met.
    let mut index: HashMap<TermId, u32, std::hash::BuildHasherDefault<IdHasher>> =
        HashMap::default();
    let mut ids: Vec<TermId> = Vec::new();
    let mut cells: Vec<u32> = Vec::with_capacity(picked.len() * columns.len());
    for &r in picked {
        let row = t.row(r);
        for c in columns {
            let id = c.map_or(TermId(0), |c| row[c]);
            if id == TermId(0) {
                cells.push(IdSolutions::UNBOUND);
                continue;
            }
            let n = *index.entry(id).or_insert_with(|| {
                ids.push(id);
                (ids.len() - 1) as u32
            });
            cells.push(n);
        }
    }
    drop(index);
    let parts = tripletrail_store::par::chunks_sized(ids.len(), 16_384, 4_096, |range| {
        let reader = eval.snapshot.reader();
        let mut terms = Vec::with_capacity(range.len());
        for (i, &id) in ids[range].iter().enumerate() {
            if i % 4096 == 0 && (eval.cancelled)() {
                return None;
            }
            terms.push(reader.term(id).ok()?);
        }
        Some(terms)
    });
    let mut terms = Vec::with_capacity(ids.len());
    for part in parts {
        terms.extend(part?);
    }
    Some(IdSolutions {
        variables,
        terms,
        cells,
        len: picked.len(),
    })
}

/// Hashes a term id by multiplying it and folding the high half in, so that both the low
/// bits (the bucket) and the high bits (the tag) vary.
#[derive(Default)]
struct IdHasher(u64);

impl std::hash::Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 << 8 | u64::from(b)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    fn write_u64(&mut self, n: u64) {
        let h = n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = h ^ (h >> 32);
    }
}

/// A `CONSTRUCT` answered from the table of its pattern (#11), as the evaluator builds it
/// (`spareval`'s `ConstructIterator`): each solution fills the template; a triple with an
/// unbound part, a literal subject or a predicate that is not an IRI is left out; blank nodes
/// of the template are new for each solution; a triple without a blank node is given once
/// (the evaluator forgets what it gave after a million of them, and so does this).
/// `None` when the ids don't answer the pattern.
pub(crate) fn construct(query: &Query, eval: &Eval) -> Option<spareval::QueryResults<'static>> {
    use spargebra::term::{NamedNodePattern, TermPattern};
    let Query::Construct {
        template,
        dataset: None,
        pattern,
        ..
    } = query
    else {
        return None;
    };
    // Under a slice the evaluator stops after the first solutions (and picks them when there
    // is no order): left to it.
    if matches!(pattern, GraphPattern::Slice { .. }) || !covered(pattern) {
        return None;
    }
    let mut labels: Vec<spargebra::term::BlankNode> = Vec::new();
    let mut slot = |t: &TermPattern| -> Option<Slot> {
        Some(match t {
            TermPattern::Variable(v) => Slot::Variable(v.clone()),
            TermPattern::NamedNode(n) => Slot::Constant(eval.snapshot.id_of(n.clone().into())),
            TermPattern::Literal(l) => Slot::Constant(eval.snapshot.id_of(l.clone().into())),
            TermPattern::BlankNode(b) => Slot::Blank(match labels.iter().position(|x| x == b) {
                Some(i) => i,
                None => {
                    labels.push(b.clone());
                    labels.len() - 1
                }
            }),
            // Triple terms in the template: left to the evaluator.
            #[allow(unreachable_patterns)]
            _ => return None,
        })
    };
    let mut slots: Vec<[Slot; 3]> = Vec::with_capacity(template.len());
    for t in template {
        let predicate = match &t.predicate {
            NamedNodePattern::Variable(v) => Slot::Variable(v.clone()),
            NamedNodePattern::NamedNode(n) => Slot::Constant(eval.snapshot.id_of(n.clone().into())),
        };
        slots.push([slot(&t.subject)?, predicate, slot(&t.object)?]);
    }
    let table = eval.eval(pattern)?;
    if (eval.cancelled)() {
        return None;
    }
    let columns: Vec<[Cell; 3]> = slots
        .iter()
        .map(|s| {
            s.clone().map(|x| match x {
                Slot::Variable(v) => match table.col(&v) {
                    Some(c) => Cell::Column(c),
                    None => Cell::Unbound,
                },
                Slot::Constant(id) => Cell::Id(id),
                Slot::Blank(i) => Cell::Blank(i),
            })
        })
        .collect();
    Some(spareval::QueryResults::Graph(
        spareval::QueryTripleIter::new(Construct {
            snapshot: eval.snapshot.clone(),
            table,
            columns,
            blanks: labels.len(),
            row: 0,
            buffered: Vec::new(),
            emitted: std::collections::HashSet::default(),
            fresh: Vec::new(),
        }),
    ))
}

#[derive(Clone)]
enum Slot {
    Variable(Variable),
    Constant(TermId),
    Blank(usize),
}

#[derive(Clone, Copy)]
enum Cell {
    Column(usize),
    Id(TermId),
    Blank(usize),
    Unbound,
}

/// The triples of a `CONSTRUCT`, a solution at a time.
struct Construct {
    snapshot: tripletrail_store::Snapshot,
    table: Table,
    columns: Vec<[Cell; 3]>,
    blanks: usize,
    row: usize,
    /// The triples of the current solution, given last first, as the evaluator does.
    buffered: Vec<oxigraph::model::Triple>,
    emitted: std::collections::HashSet<[TermId; 3]>,
    /// The blank nodes of the current solution.
    fresh: Vec<oxigraph::model::BlankNode>,
}

impl Construct {
    /// Decoded at each use, as the evaluator does: faster than keeping the terms.
    fn term(&self, id: TermId) -> Option<Term> {
        self.snapshot.term(id).ok()
    }
}

impl Iterator for Construct {
    type Item = Result<oxigraph::model::Triple, spareval::QueryEvaluationError>;

    fn next(&mut self) -> Option<Self::Item> {
        use oxigraph::model::{BlankNode, NamedNode, NamedOrBlankNode, Triple};
        loop {
            if let Some(t) = self.buffered.pop() {
                return Some(Ok(t));
            }
            if self.row >= self.table.len() {
                return None;
            }
            let r = self.row;
            self.row += 1;
            self.fresh.clear();
            self.fresh
                .extend((0..self.blanks).map(|_| BlankNode::default()));
            for i in 0..self.columns.len() {
                let cells = self.columns[i];
                let mut ids = [TermId(0); 3];
                let mut parts: [Option<Term>; 3] = [None, None, None];
                let mut blank = false;
                for (k, cell) in cells.iter().enumerate() {
                    match *cell {
                        Cell::Column(c) => ids[k] = self.table.row(r)[c],
                        Cell::Id(id) => ids[k] = id,
                        Cell::Blank(b) => {
                            parts[k] = Some(self.fresh[b].clone().into());
                            blank = true;
                        }
                        Cell::Unbound => {}
                    }
                    if parts[k].is_none() {
                        if ids[k] == TermId(0) {
                            break;
                        }
                        // A term that doesn't decode is left out, as the evaluator leaves it.
                        parts[k] = self.term(ids[k]);
                    }
                }
                let [Some(s), Some(p), Some(o)] = parts else {
                    continue;
                };
                let Ok(subject) = NamedOrBlankNode::try_from(s) else {
                    continue;
                };
                let Ok(predicate) = NamedNode::try_from(p) else {
                    continue;
                };
                blank |= subject.is_blank_node() || o.is_blank_node() || o.is_triple();
                if !blank {
                    if !self.emitted.insert(ids) {
                        continue;
                    }
                    if self.emitted.len() > 1024 * 1024 {
                        self.emitted.clear();
                    }
                }
                self.buffered.push(Triple::new(subject, predicate, o));
            }
        }
    }
}
