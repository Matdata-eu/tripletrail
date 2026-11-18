//! Cardinality estimates for the SPARQL optimizer from the store's block index (see
//! `sparopt::estimate`): the join order follows the data, not fixed guesses.

use crate::{GraphSelector, Snapshot, TermId};
use oxigraph::model::{NamedNode, TermRef};
use rustc_hash::FxHashMap;
use spargebra::term::GroundTermPattern;
use sparopt::estimate::{Estimator, Slot};
use std::cell::RefCell;

/// Rows read to estimate how many quads one value of a bound position matches, when the
/// counts kept with the indexes don't have it.
const SAMPLE: usize = 256;

pub(crate) struct DataEstimator {
    snapshot: Snapshot,
    /// `(predicate, subject side)` -> rows per value of that side, looked up or sampled once
    /// per query.
    fanout: RefCell<FxHashMap<(u64, bool), usize>>,
}

impl DataEstimator {
    pub(crate) fn new(snapshot: Snapshot) -> Self {
        Self {
            snapshot,
            fanout: RefCell::default(),
        }
    }

    fn id(&self, t: &GroundTermPattern) -> Option<Option<TermId>> {
        let term: TermRef<'_> = match t {
            GroundTermPattern::NamedNode(n) => n.as_ref().into(),
            GroundTermPattern::Literal(l) => l.as_ref().into(),
            // Triple terms and variables: not estimated here.
            _ => return None,
        };
        Some(self.snapshot.term_id(term))
    }

    fn count(&self, s: Option<TermId>, p: TermId, o: Option<TermId>) -> usize {
        self.snapshot
            .estimate_ids(s, Some(p), o, GraphSelector::All)
    }

    /// How many quads with predicate `p` one value of the subject (`subject`) or object side
    /// matches, weighted by how often values come up (as a join key bound from the data does).
    fn fanout(&self, p: TermId, subject: bool) -> usize {
        if let Some(&n) = self.fanout.borrow().get(&(p.0, subject)) {
            return n;
        }
        // Kept with the indexes; sampled only for predicates they don't hold.
        if let Some(n) = self.snapshot.predicate_fanout(p, subject) {
            let n = usize::try_from(n).unwrap_or(usize::MAX);
            self.fanout.borrow_mut().insert((p.0, subject), n);
            return n;
        }
        let mut per_value: FxHashMap<u64, usize> = FxHashMap::default();
        let mut total = 0usize;
        let mut rows = 0usize;
        for q in self
            .snapshot
            .quad_ids_for_pattern(None, Some(p), None, GraphSelector::All)
            .take(SAMPLE)
        {
            let v = if subject { q[0] } else { q[2] };
            let n = *per_value.entry(v.0).or_insert_with(|| {
                if subject {
                    self.count(Some(v), p, None)
                } else {
                    self.count(None, p, Some(v))
                }
            });
            total += n;
            rows += 1;
        }
        let n = if rows == 0 { 0 } else { total.div_ceil(rows) };
        self.fanout.borrow_mut().insert((p.0, subject), n);
        n
    }
}

impl Estimator for DataEstimator {
    fn triples(&self, subject: Slot<'_>, predicate: &NamedNode, object: Slot<'_>) -> Option<usize> {
        let Some(p) = self.snapshot.term_id(predicate.as_ref().into()) else {
            return Some(0);
        };
        if p.kind() == crate::Kind::Column {
            // A column of a result table: one value per row.
            let bound = !matches!(subject, Slot::Free);
            return Some(self.count(bound.then_some(p), p, None));
        }
        let constant = |slot: Slot<'_>| match slot {
            Slot::Constant(t) => self.id(t),
            _ => Some(None),
        };
        let (s, o) = (constant(subject)?, constant(object)?);
        if matches!(subject, Slot::Constant(_)) && s.is_none()
            || matches!(object, Slot::Constant(_)) && o.is_none()
        {
            // A constant the store does not hold: nothing matches.
            return Some(0);
        }
        Some(match (subject, object) {
            (Slot::Bound, Slot::Bound) => 1,
            (Slot::Bound, Slot::Constant(_)) => self.fanout(p, true).min(self.count(None, p, o)),
            (Slot::Constant(_), Slot::Bound) => self.fanout(p, false).min(self.count(s, p, None)),
            (Slot::Bound, Slot::Free) => self.fanout(p, true),
            (Slot::Free, Slot::Bound) => self.fanout(p, false),
            _ => self.count(s, p, o),
        })
    }
}
