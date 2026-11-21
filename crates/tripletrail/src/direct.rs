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

/// Solutions as rows of terms, a cell `None` where unbound.
type Rows = Vec<Vec<Option<Term>>>;

/// The answer of `query`: its variables and rows. `None` when the ids don't answer it.
pub(crate) fn answer(query: &Query, eval: &Eval) -> Option<(Arc<[Variable]>, Rows)> {
    use GraphPattern as G;
    let Query::Select {
        dataset: None,
        pattern,
        ..
    } = query
    else {
        return None;
    };
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
    // Under a slice without an order the evaluator stops after the first rows: it is faster.
    if length.is_some() && order.is_none() {
        return None;
    }
    if !crate::rows::covered(body) {
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
    let picked: Vec<usize> = match length {
        Some(l) => picked.skip(start).take(l).collect(),
        None => picked.skip(start).collect(),
    };
    let out = decode(&t, &columns, &picked, eval)?;
    Some((variables.clone().into(), out))
}

/// The rows `picked` of `t`, the columns `columns` of each as terms (an unbound cell or a
/// variable `t` doesn't have as `None`), each distinct id decoded once, under one lock of the
/// dictionary.
fn decode(t: &Table, columns: &[Option<usize>], picked: &[usize], eval: &Eval) -> Option<Rows> {
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
                cells.push(u32::MAX);
                continue;
            }
            let n = *index.entry(id).or_insert_with(|| {
                ids.push(id);
                (ids.len() - 1) as u32
            });
            cells.push(n);
        }
    }
    let mut terms: Vec<Term> = Vec::with_capacity(ids.len());
    let reader = eval.snapshot.reader();
    for (i, &id) in ids.iter().enumerate() {
        if i % 4096 == 0 && (eval.cancelled)() {
            return None;
        }
        terms.push(reader.term(id).ok()?);
    }
    drop(reader);
    if columns.is_empty() {
        return Some(vec![Vec::new(); picked.len()]);
    }
    let mut out = Vec::with_capacity(picked.len());
    for row in cells.chunks(columns.len()) {
        out.push(
            row.iter()
                .map(|&n| (n != u32::MAX).then(|| terms[n as usize].clone()))
                .collect(),
        );
    }
    Some(out)
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
