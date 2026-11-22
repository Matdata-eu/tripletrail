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
    if !covered(body) {
        return None;
    }
    // An order on a computed `BIND` value: its ids (made up for values not inline) don't
    // order, so the evaluator answers it, before any work here.
    if let Some(expression) = order
        && expression.iter().any(|e| match e {
            OrderExpression::Asc(Expression::Variable(v))
            | OrderExpression::Desc(Expression::Variable(v)) => computed(body, v),
            _ => true,
        })
    {
        return None;
    }
    let t = eval.eval(body)?;
    let rows: Vec<usize> = match order {
        Some(expression) => {
            let by = expression
                .iter()
                .map(|e| match e {
                    OrderExpression::Asc(Expression::Variable(v)) => Some((v, false)),
                    OrderExpression::Desc(Expression::Variable(v)) => Some((v, true)),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()?;
            // All the rows in order: only the slice is kept below, after DISTINCT.
            let keep = if distinct {
                t.len()
            } else {
                length.map_or(t.len(), |l| start.saturating_add(l).min(t.len()))
            };
            crate::rows::first_rows(&t, by, None, keep, eval.snapshot)?
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
        | G::Distinct { inner }
        | G::Reduced { inner }
        | G::OrderBy { inner, .. }
        | G::Slice { inner, .. } => computed(inner, v),
        _ => false,
    }
}

/// Whether the expression kernels (`idvalue::compile`) may compute `e`, by its form alone: so
/// that a `BIND` they can't compute leaves the query to the evaluator before any work on ids.
/// `top`: a constant or a copy is fine there (`BIND(ex:a AS ?k)`), not inside arithmetic.
/// Whether the value kernels (`idvalue::compile`) may compute `e`, inside an expression.
pub(crate) fn number_computable(e: &Expression) -> bool {
    computable(e, false)
}

fn computable(e: &Expression, top: bool) -> bool {
    use spargebra::algebra::Function as F;
    match e {
        Expression::Variable(_) => true,
        Expression::Literal(_) => true,
        Expression::NamedNode(_) => top,
        Expression::Add(a, b) | Expression::Subtract(a, b) | Expression::Multiply(a, b) => {
            computable(a, false) && computable(b, false)
        }
        Expression::UnaryMinus(a) => computable(a, false),
        Expression::FunctionCall(
            F::Abs
            | F::Ceil
            | F::Floor
            | F::Round
            | F::Year
            | F::Month
            | F::Day
            | F::Hours
            | F::Minutes,
            args,
        ) => args.len() == 1 && computable(&args[0], false),
        // `STRLEN` and `xsd:integer` of a text test read a variable's string.
        Expression::FunctionCall(F::StrLen, args) => {
            args.len() == 1 && !matches!(args[0], Expression::Literal(_))
        }
        Expression::FunctionCall(F::Custom(f), args)
            if f.as_ref() == oxigraph::model::vocab::xsd::INTEGER =>
        {
            args.len() == 1 && !matches!(args[0], Expression::Literal(_))
        }
        _ => false,
    }
}

/// Whether the id tables may answer `p`: what [`crate::rows::covered`] covers, and `BIND`
/// over it (`idtree::Eval::extend` decides the expression).
fn covered(p: &GraphPattern) -> bool {
    use GraphPattern as G;
    match p {
        G::Extend {
            inner, expression, ..
        } => {
            (computable(expression, true) || crate::idstring::computable(expression))
                && covered(inner)
        }
        G::Project { inner, .. }
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
