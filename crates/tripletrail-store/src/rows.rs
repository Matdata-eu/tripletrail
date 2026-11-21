//! Row filters (#202): a view of the store without some resources and properties, as
//! shape-level access rules (`sg:visibleIf`, `sg:hideProperties`) hide them. The filter
//! works on term ids at the quad scans, so everything a query reads (joins, `OPTIONAL`,
//! `MINUS`, `EXISTS`, paths, aggregates) sees the restricted view without the algebra being
//! touched. A view without a filter reads the store as before: the scans test one flag they
//! already tested.

use rustc_hash::FxHashSet;
use std::sync::OnceLock;

/// What a view hides, by term id.
#[derive(Clone, Debug, Default)]
pub struct RowFilter {
    /// Quads with these subjects are hidden.
    subjects: FxHashSet<u64>,
    /// Quads with these objects are hidden (references to hidden resources).
    objects: FxHashSet<u64>,
    /// Quads with these predicates are hidden.
    predicates: FxHashSet<u64>,
    /// Quads with these (subject, predicate) are hidden.
    subject_predicates: FxHashSet<(u64, u64)>,
    /// These quads are hidden: the copies of a triple in graphs after the first that holds
    /// it, for reading the union of all graphs as each triple once (#808).
    quads: FxHashSet<[u64; 4]>,
    /// When set, quads in graphs outside these (the default graph is 0) are hidden: a caller
    /// who reads some graphs only, as the union of all graphs (#204).
    graphs: Option<FxHashSet<u64>>,
    /// [`RowFilter::fingerprint`], made once (#708): a filter is shared by every query on a
    /// store version, and each would otherwise sort and hash the whole hidden set again.
    /// Every change takes it.
    fingerprint: OnceLock<u64>,
}

impl PartialEq for RowFilter {
    fn eq(&self, other: &Self) -> bool {
        self.subjects == other.subjects
            && self.objects == other.objects
            && self.predicates == other.predicates
            && self.subject_predicates == other.subject_predicates
            && self.quads == other.quads
            && self.graphs == other.graphs
    }
}

impl Eq for RowFilter {}

impl RowFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hides the quads about `resource` (as subject) and, with `references`, those that
    /// point to it (as object). A literal is never hidden: its id is never given here.
    pub fn hide_resource(&mut self, resource: crate::TermId, references: bool) {
        self.fingerprint.take();
        self.subjects.insert(resource.0);
        if references {
            self.objects.insert(resource.0);
        }
    }

    /// Hides `predicate` everywhere.
    pub fn hide_predicate(&mut self, predicate: crate::TermId) {
        self.fingerprint.take();
        self.predicates.insert(predicate.0);
    }

    /// Hides `predicate` on `subject` only.
    pub fn hide_predicate_on(&mut self, subject: crate::TermId, predicate: crate::TermId) {
        self.fingerprint.take();
        self.subject_predicates.insert((subject.0, predicate.0));
    }

    /// Hides the quad `[s, p, o, g]`.
    pub fn hide_quad(&mut self, quad: [u64; 4]) {
        self.fingerprint.take();
        self.quads.insert(quad);
    }

    /// Hides every quad outside `graphs` (the default graph is 0), on top of what else this
    /// hides. Asked again, only the graphs in both sets are shown.
    pub fn show_only_graphs(&mut self, graphs: impl IntoIterator<Item = u64>) {
        self.fingerprint.take();
        let graphs: FxHashSet<u64> = graphs.into_iter().collect();
        self.graphs = Some(match self.graphs.take() {
            Some(before) => before.intersection(&graphs).copied().collect(),
            None => graphs,
        });
    }

    /// What this and `other` hide together.
    pub fn merged(&self, other: &RowFilter) -> RowFilter {
        let mut out = self.clone();
        out.fingerprint.take();
        out.subjects.extend(&other.subjects);
        out.objects.extend(&other.objects);
        out.predicates.extend(&other.predicates);
        out.subject_predicates.extend(&other.subject_predicates);
        out.quads.extend(&other.quads);
        if let Some(graphs) = &other.graphs {
            out.show_only_graphs(graphs.iter().copied());
        }
        out
    }

    /// The quads hidden one by one ([`RowFilter::hide_quad`]), in no order.
    pub(crate) fn quad_list(&self) -> impl Iterator<Item = &[u64; 4]> {
        self.quads.iter()
    }

    /// Quads hidden one by one ([`RowFilter::hide_quad`]).
    pub fn hidden_quads(&self) -> usize {
        self.quads.len()
    }

    /// Roughly the heap bytes of the quads hidden one by one (32 bytes and a hash slot each).
    pub fn quads_heap_size(&self) -> usize {
        self.quads.capacity() * (std::mem::size_of::<[u64; 4]>() + 8)
    }

    /// Hides nothing more of `resources` (sorted): not as subjects, objects or subjects of a
    /// hidden predicate. For bringing a filter up to date, those resources judged again.
    pub fn forget(&mut self, resources: &[u64]) {
        self.fingerprint.take();
        let gone = |id: &u64| resources.binary_search(id).is_ok();
        self.subjects.retain(|id| !gone(id));
        self.objects.retain(|id| !gone(id));
        self.subject_predicates.retain(|(s, _)| !gone(s));
        if !self.quads.is_empty() {
            self.quads.retain(|q| !gone(&q[0]));
        }
    }

    /// Whether this hides `predicate` on some subject.
    pub fn hides_predicate_on_some(&self, predicate: u64) -> bool {
        self.subject_predicates.iter().any(|&(_, p)| p == predicate)
    }

    /// Whether this hides nothing.
    pub fn is_empty(&self) -> bool {
        self.subjects.is_empty()
            && self.objects.is_empty()
            && self.predicates.is_empty()
            && self.subject_predicates.is_empty()
            && self.quads.is_empty()
            && self.graphs.is_none()
    }

    /// Whether the quad `[s, p, o, g]` is hidden.
    #[inline]
    pub fn hides(&self, q: &[u64; 4]) -> bool {
        self.subjects.contains(&q[0])
            || self.objects.contains(&q[2])
            || self.predicates.contains(&q[1])
            || (!self.subject_predicates.is_empty()
                && self.subject_predicates.contains(&(q[0], q[1])))
            || (!self.quads.is_empty() && self.quads.contains(q))
            || self.graphs.as_ref().is_some_and(|g| !g.contains(&q[3]))
    }

    /// Whether every quad with this subject, predicate or object (any of them `None`) is
    /// hidden: a scan with these fixed reads nothing (#204).
    pub fn hides_all(&self, s: Option<u64>, p: Option<u64>, o: Option<u64>) -> bool {
        s.is_some_and(|s| self.subjects.contains(&s))
            || p.is_some_and(|p| self.predicates.contains(&p))
            || o.is_some_and(|o| self.objects.contains(&o))
    }

    /// Whether `resource` is hidden as a subject.
    pub fn hides_resource(&self, resource: crate::TermId) -> bool {
        self.subjects.contains(&resource.0)
    }

    /// A key for what this filter hides, for caches of what a view reads: two filters with
    /// the same key hide the same quads.
    pub fn fingerprint(&self) -> u64 {
        *self.fingerprint.get_or_init(|| self.hash_all())
    }

    fn hash_all(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for set in [&self.subjects, &self.objects, &self.predicates] {
            let mut ids: Vec<u64> = set.iter().copied().collect();
            ids.sort_unstable();
            ids.hash(&mut h);
        }
        let mut pairs: Vec<(u64, u64)> = self.subject_predicates.iter().copied().collect();
        pairs.sort_unstable();
        pairs.hash(&mut h);
        let mut quads: Vec<[u64; 4]> = self.quads.iter().copied().collect();
        quads.sort_unstable();
        quads.hash(&mut h);
        let mut graphs: Option<Vec<u64>> =
            self.graphs.as_ref().map(|g| g.iter().copied().collect());
        if let Some(g) = &mut graphs {
            g.sort_unstable();
        }
        graphs.hash(&mut h);
        h.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TermId;

    /// The fingerprint kept once made (#708) follows every change: it is that of a filter
    /// made with the same changes and never asked before, and equal filters have equal ones.
    #[test]
    fn a_kept_fingerprint_follows_every_change() {
        let changes: [&dyn Fn(&mut RowFilter); 7] = [
            &|f| f.hide_resource(TermId(1), false),
            &|f| f.hide_resource(TermId(2), true),
            &|f| f.hide_predicate(TermId(3)),
            &|f| f.hide_predicate_on(TermId(4), TermId(5)),
            &|f| f.forget(&[1]),
            &|f| f.show_only_graphs([0, 6, 7]),
            &|f| f.show_only_graphs([0, 7]),
        ];
        let mut asked = RowFilter::new();
        let mut fresh = RowFilter::new();
        let mut seen = vec![asked.fingerprint()];
        for change in changes {
            change(&mut asked);
            change(&mut fresh);
            let kept = asked.fingerprint();
            assert_eq!(kept, fresh.clone().hash_all());
            assert_eq!(kept, asked.clone().fingerprint());
            assert!(!seen.contains(&kept), "each change hides something else");
            seen.push(kept);
        }
        assert_eq!(asked, fresh);
    }

    /// A merged filter hides what either hides, and a graph restriction hides the quads of
    /// the other graphs only.
    #[test]
    fn a_merged_filter_hides_what_either_hides() {
        let mut a = RowFilter::new();
        a.hide_resource(TermId(1), false);
        a.hide_quad([2, 3, 4, 5]);
        let mut b = RowFilter::new();
        b.hide_predicate(TermId(9));
        b.show_only_graphs([0, 5]);
        let both = a.merged(&b);
        assert!(both.hides(&[1, 3, 4, 0]));
        assert!(both.hides(&[2, 3, 4, 5]));
        assert!(both.hides(&[2, 9, 4, 0]));
        assert!(both.hides(&[2, 3, 4, 6]), "graph 6 is not shown");
        assert!(!both.hides(&[2, 3, 4, 0]));
        assert!(!both.hides(&[2, 3, 8, 5]));
        assert_eq!(both, b.merged(&a));
        assert_eq!(both.fingerprint(), b.merged(&a).fingerprint());
        assert!(
            !RowFilter::new()
                .merged(&RowFilter::new())
                .hides(&[1, 2, 3, 4])
        );
    }
}
