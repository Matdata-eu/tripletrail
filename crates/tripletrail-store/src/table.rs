//! Result tables a query hands to its own evaluation: rows of term ids computed outside the
//! evaluator (a spatial join on ids), read back through virtual triples
//! `_:row <urn:tripletrail:table:N:C> ?var` that the snapshot answers from the table, so the
//! rows are neither decoded to terms nor looked up again.
//!
//! A table is registered on the thread that rewrites the query and taken by every snapshot
//! made on that thread afterwards (the query's evaluation), until the outermost
//! [`table_scope`] ends.

use crate::bgp::IdRows;
use crate::id::{Kind, TermId};
use oxigraph::model::NamedNode;
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

const PREFIX: &str = "urn:tripletrail:table:";
/// Rows per table, as row ids carry the row in 40 bits.
const ROW_BITS: u32 = 40;
pub(crate) type Tables = Arc<[(u32, Arc<IdRows>)]>;
/// `(row, value)` pairs of a column: one looked up, or a whole column.
pub(crate) type ColumnQuads = rayon::iter::Either<
    std::option::IntoIter<(TermId, TermId)>,
    std::vec::IntoIter<(TermId, TermId)>,
>;

thread_local! {
    static PENDING: RefCell<Vec<(u32, Arc<IdRows>)>> = const { RefCell::new(Vec::new()) };
    static DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// The number of the next table, over all threads (#7): two queries evaluated at the same
/// time never name the same table, so a view holding another query's tables can't answer a
/// column of its own query from them. Numbered per thread before, every thread's first table
/// was table 1. Wraps after 2^32 tables, skipping 0.
static NEXT: AtomicU32 = AtomicU32::new(1);

/// Runs `f` (a query's rewrite and evaluation); the tables registered in it are dropped when
/// the outermost scope on this thread ends (snapshots taken in it keep theirs).
pub fn table_scope<R>(f: impl FnOnce() -> R) -> R {
    struct Exit;
    impl Drop for Exit {
        fn drop(&mut self) {
            let depth = DEPTH.with(|d| {
                d.set(d.get() - 1);
                d.get()
            });
            if depth == 0 {
                PENDING.with_borrow_mut(Vec::clear);
            }
        }
    }
    DEPTH.with(|d| d.set(d.get() + 1));
    let _exit = Exit;
    f()
}

/// Whether `iri` is a column of a table of rows ([`register_table`]): its values are the
/// table's, not triples of the store.
pub fn is_table_column(iri: &str) -> bool {
    iri.starts_with(PREFIX)
}

/// Registers `rows` for the snapshots taken next on this thread, and returns the predicates
/// that read its columns, one per column. `None` outside a [`table_scope`] or for a table too
/// large to address.
pub fn register_table(rows: IdRows) -> Option<Vec<NamedNode>> {
    if DEPTH.with(Cell::get) == 0 || rows.len() as u64 >= 1 << ROW_BITS || rows.width > 0xffff {
        return None;
    }
    let mut n = NEXT.fetch_add(1, Ordering::Relaxed);
    if n == 0 {
        n = NEXT.fetch_add(1, Ordering::Relaxed);
    }
    let width = rows.width;
    PENDING.with_borrow_mut(|p| p.push((n, Arc::new(rows))));
    Some(
        (0..width)
            .map(|c| NamedNode::new_unchecked(format!("{PREFIX}{n}:{c}")))
            .collect(),
    )
}

pub(crate) fn pending() -> Tables {
    PENDING.with_borrow(|p| p.iter().cloned().collect())
}

/// The table and column a predicate IRI reads, if it is one of `tables`.
pub(crate) fn column(tables: &Tables, iri: &str) -> Option<TermId> {
    let rest = iri.strip_prefix(PREFIX)?;
    let (n, c) = rest.split_once(':')?;
    let (n, c): (u32, u64) = (n.parse().ok()?, c.parse().ok()?);
    let (_, t) = tables.iter().find(|(m, _)| *m == n)?;
    (c < t.width as u64).then(|| TermId::new(Kind::Column, (u64::from(n) << 16) | c))
}

fn table(tables: &Tables, n: u32) -> Option<&Arc<IdRows>> {
    tables.iter().find(|(m, _)| *m == n).map(|(_, t)| t)
}

/// The rows of a column predicate (`Kind::Column`) as `(row, value)`, for `subject` (a row
/// id) and `object` bound or not.
pub(crate) fn quads(
    tables: &Tables,
    predicate: TermId,
    subject: Option<TermId>,
    object: Option<TermId>,
) -> ColumnQuads {
    use rayon::iter::Either::{Left, Right};
    let (n, c) = (
        (predicate.payload() >> 16) as u32,
        (predicate.payload() & 0xffff) as usize,
    );
    let Some(t) = table(tables, n) else {
        return Left(None.into_iter());
    };
    let row_id = |r: usize| TermId::new(Kind::Row, (u64::from(n) << ROW_BITS) | r as u64);
    let value = |r: usize| t.ids[r * t.width + c];
    // Id 0 (the default graph, never a value) is an unbound cell: the row has no triple.
    let wanted = |v: TermId| v != TermId(0) && object.is_none_or(|o| o == v);
    match subject {
        // One row looked up (the evaluator's nested loops): no allocation.
        Some(s) => {
            if s.kind() != Kind::Row || (s.payload() >> ROW_BITS) as u32 != n {
                return Left(None.into_iter());
            }
            let r = (s.payload() & ((1 << ROW_BITS) - 1)) as usize;
            Left(
                (r < t.len() && wanted(value(r)))
                    .then(|| (s, value(r)))
                    .into_iter(),
            )
        }
        None => Right(
            (0..t.len())
                .filter(|&r| wanted(value(r)))
                .map(|r| (row_id(r), value(r)))
                .collect::<Vec<_>>()
                .into_iter(),
        ),
    }
}

/// The rows of a pattern made only of one table's columns on one row variable, each column's
/// value in a variable of its own (`_:row <…:N:0> ?a . _:row <…:N:2> ?b`), as rows of `width`
/// columns (the row variable gets the row's id), or their number: a row whose cell is unbound
/// for one of the columns has no triple for it, so it is not a solution. `None` for other
/// patterns.
pub(crate) fn whole_rows(
    tables: &Tables,
    triples: &[crate::bgp::IdTriple],
    width: usize,
    count: bool,
) -> Option<Result<IdRows, u64>> {
    let (n, t, row_var, cols) = shape(tables, triples, width)?;
    let bound = |r: usize| {
        cols.iter()
            .all(|&(c, _)| t.ids[r * t.width + c] != TermId(0))
    };
    if count {
        return Some(Err((0..t.len()).filter(|&r| bound(r)).count() as u64));
    }
    let mut ids = Vec::with_capacity(t.len() * width);
    for r in (0..t.len()).filter(|&r| bound(r)) {
        let at = ids.len();
        ids.resize(at + width, TermId(0));
        ids[at + row_var] = TermId::new(Kind::Row, (u64::from(n) << ROW_BITS) | r as u64);
        for &(c, v) in &cols {
            ids[at + v] = t.ids[r * t.width + c];
        }
    }
    Some(Ok(IdRows { width, ids }))
}

/// Whether [`whole_rows`] answers this pattern.
pub(crate) fn is_whole_rows(
    tables: &Tables,
    triples: &[crate::bgp::IdTriple],
    width: usize,
) -> bool {
    shape(tables, triples, width).is_some()
}

/// The table, row variable and `(column, variable)` per triple of a [`whole_rows`] pattern.
#[allow(clippy::type_complexity)]
fn shape<'a>(
    tables: &'a Tables,
    triples: &[crate::bgp::IdTriple],
    width: usize,
) -> Option<(u32, &'a Arc<IdRows>, usize, Vec<(usize, usize)>)> {
    use crate::bgp::IdSlot;
    let first = triples.first()?;
    let IdSlot::Var(row_var) = first.s else {
        return None;
    };
    let IdSlot::Const(p0) = first.p else {
        return None;
    };
    if p0.kind() != Kind::Column {
        return None;
    }
    let n = (p0.payload() >> 16) as u32;
    let t = table(tables, n)?;
    let mut cols: Vec<(usize, usize)> = Vec::with_capacity(triples.len());
    for tr in triples {
        let (IdSlot::Var(s), IdSlot::Const(p), IdSlot::Var(o)) = (tr.s, tr.p, tr.o) else {
            return None;
        };
        if s != row_var
            || o == row_var
            || p.kind() != Kind::Column
            || (p.payload() >> 16) as u32 != n
            || cols.iter().any(|&(_, v)| v == o)
            || o >= width
        {
            return None;
        }
        cols.push(((p.payload() & 0xffff) as usize, o));
    }
    (row_var < width).then_some((n, t, row_var, cols))
}

/// How many quads [`quads`] gives for a column predicate, the subject bound or not.
pub(crate) fn estimate(tables: &Tables, predicate: TermId, subject_bound: bool) -> usize {
    if subject_bound {
        return 1;
    }
    table(tables, (predicate.payload() >> 16) as u32).map_or(0, |t| t.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> IdRows {
        IdRows {
            width: 1,
            ids: vec![TermId::integer(7).expect("small")],
        }
    }

    /// Two queries on two threads at the same time (two map facets, #7) get tables of their
    /// own numbers, and one's view reads no column of the other's.
    #[test]
    fn concurrent_queries_never_share_a_table_number() {
        let barrier = std::sync::Barrier::new(2);
        let names: Vec<(String, Tables)> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        table_scope(|| {
                            barrier.wait();
                            let p = register_table(rows()).expect("in a scope");
                            barrier.wait();
                            (p[0].as_str().to_owned(), pending())
                        })
                    })
                })
                .collect();
            threads.into_iter().map(|t| t.join().unwrap()).collect()
        });
        let (a, b) = (&names[0], &names[1]);
        assert_ne!(a.0, b.0);
        assert!(column(&a.1, &a.0).is_some() && column(&b.1, &b.0).is_some());
        assert!(column(&a.1, &b.0).is_none() && column(&b.1, &a.0).is_none());
    }
}
