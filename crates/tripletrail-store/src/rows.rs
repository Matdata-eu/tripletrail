//! Row filters (#202): a view of the store without some resources and properties, as
//! shape-level access rules (`sg:visibleIf`, `sg:hideProperties`) hide them. The filter
//! works on term ids at the quad scans, so everything a query reads (joins, `OPTIONAL`,
//! `MINUS`, `EXISTS`, paths, aggregates) sees the restricted view without the algebra being
//! touched. A view without a filter reads the store as before: the scans test one flag they
//! already tested.

use rustc_hash::FxHashSet;
use std::sync::{Arc, OnceLock};

/// A set of resources that many filters hide, shared rather than copied: the targets of an
/// "own records" rule (`sh:hasValue sg:requestingAgent`), which each agent's filter hides
/// except for that agent's own (enterprise #7). Made once per store generation, it keeps a
/// thousand agents' filters from each holding every target.
#[derive(Debug)]
pub struct SharedHidden {
    ids: Ids,
    len: usize,
    /// A key for the ids, made once.
    key: u64,
}

/// The ids of a [`SharedHidden`]: a bitmap over their range when they are dense enough (term
/// ids of one kind are numbered in order, so the targets of a class usually are), else a set.
#[derive(Debug, PartialEq)]
enum Ids {
    Bits { base: u64, words: Vec<u64> },
    Set(FxHashSet<u64>),
}

impl SharedHidden {
    pub fn new(ids: impl IntoIterator<Item = u64>) -> Self {
        use std::hash::{Hash, Hasher};
        let mut sorted: Vec<u64> = ids.into_iter().collect();
        sorted.sort_unstable();
        sorted.dedup();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        sorted.hash(&mut h);
        let len = sorted.len();
        let ids = match (sorted.first(), sorted.last()) {
            // At most 64 bits per id: no larger than the set's 16 bytes.
            (Some(&lo), Some(&hi)) if (hi - lo) / 64 <= len as u64 * 2 => {
                let mut words = vec![0u64; ((hi - lo) / 64 + 1) as usize];
                for id in sorted {
                    let d = id - lo;
                    words[(d / 64) as usize] |= 1 << (d % 64);
                }
                Ids::Bits { base: lo, words }
            }
            _ => Ids::Set(sorted.into_iter().collect()),
        };
        Self {
            ids,
            len,
            key: h.finish(),
        }
    }

    #[inline]
    pub fn contains(&self, id: u64) -> bool {
        match &self.ids {
            Ids::Bits { base, words } => id
                .checked_sub(*base)
                .and_then(|d| words.get((d / 64) as usize).map(|w| w >> (d % 64) & 1 == 1))
                .unwrap_or(false),
            Ids::Set(set) => set.contains(&id),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Roughly the heap bytes of the ids.
    pub fn heap_size(&self) -> usize {
        match &self.ids {
            Ids::Bits { words, .. } => words.capacity() * 8,
            Ids::Set(set) => set.capacity() * 16,
        }
    }
}

/// Whether two shared sets hold the same ids: the same set, or equal ids, never their keys
/// alone, so that two filters are equal (and share what is cached for them) only when they
/// hide the same.
fn same_ids(a: &Arc<SharedHidden>, b: &Arc<SharedHidden>) -> bool {
    Arc::ptr_eq(a, b) || (a.len == b.len && a.key == b.key && a.ids == b.ids)
}

/// A shared set hidden but for some of its resources ([`RowFilter::hide_shared`]).
#[derive(Clone, Debug)]
struct Shared {
    hidden: Arc<SharedHidden>,
    /// Sorted.
    shown: Vec<u64>,
    /// `shown` as bits aligned with the hidden bitmap, when there are more than a few: almost
    /// every quad of the targets asks, and a search of a list for each was half the scan.
    mask: Option<Vec<u64>>,
    references: bool,
}

/// Shown resources up to this many are looked for in their list; more get a bitmap.
const FEW_SHOWN: usize = 16;

impl Shared {
    fn new(hidden: Arc<SharedHidden>, mut shown: Vec<u64>, references: bool) -> Self {
        shown.retain(|&id| hidden.contains(id));
        shown.sort_unstable();
        shown.dedup();
        let mut shared = Self {
            hidden,
            shown,
            mask: None,
            references,
        };
        shared.index();
        shared
    }

    fn index(&mut self) {
        self.mask = None;
        if self.shown.len() > FEW_SHOWN
            && let Ids::Bits { base, words } = &self.hidden.ids
        {
            let mut mask = vec![0u64; words.len()];
            for &id in &self.shown {
                let d = id - base;
                mask[(d / 64) as usize] |= 1 << (d % 64);
            }
            self.mask = Some(mask);
        }
    }

    #[inline]
    fn hides(&self, id: u64) -> bool {
        if !self.hidden.contains(id) {
            return false;
        }
        match &self.mask {
            Some(mask) => {
                let Ids::Bits { base, .. } = &self.hidden.ids else {
                    unreachable!("a mask is made for a bitmap only")
                };
                let d = id - base;
                mask[(d / 64) as usize] >> (d % 64) & 1 == 0
            }
            None if self.shown.len() <= FEW_SHOWN => !self.shown.contains(&id),
            None => self.shown.binary_search(&id).is_err(),
        }
    }
}

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
    /// Shared sets hidden but for a few of their resources each ([`RowFilter::hide_shared`]).
    shared: Vec<Shared>,
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
            && self.shared.len() == other.shared.len()
            && self.shared.iter().zip(&other.shared).all(|(a, b)| {
                same_ids(&a.hidden, &b.hidden) && a.shown == b.shown && a.references == b.references
            })
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

    /// Hides the resources of `hidden` but those in `shown`, as subjects and, with
    /// `references`, as objects: what [`RowFilter::hide_resource`] of each would hide, with the
    /// set shared between filters instead of copied into each (enterprise #7).
    pub fn hide_shared(
        &mut self,
        hidden: Arc<SharedHidden>,
        shown: impl IntoIterator<Item = u64>,
        references: bool,
    ) {
        self.fingerprint.take();
        if hidden.is_empty() {
            return;
        }
        self.shared
            .push(Shared::new(hidden, shown.into_iter().collect(), references));
    }

    /// Roughly the heap bytes of this filter's own sets (shared sets not counted).
    pub fn heap_size(&self) -> usize {
        (self.subjects.capacity() + self.objects.capacity() + self.predicates.capacity()) * 16
            + self.subject_predicates.capacity() * 24
            + self.quads_heap_size()
            + self.graphs.as_ref().map_or(0, |g| g.capacity() * 16)
            + self
                .shared
                .iter()
                .map(|s| s.shown.capacity() * 8 + s.mask.as_ref().map_or(0, |m| m.capacity() * 8))
                .sum::<usize>()
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
        out.shared.extend(other.shared.iter().cloned());
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
        for shared in &mut self.shared {
            shared
                .shown
                .extend(resources.iter().filter(|&&r| shared.hidden.contains(r)));
            shared.shown.sort_unstable();
            shared.shown.dedup();
            shared.index();
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
            && self.shared.is_empty()
    }

    /// Whether the quad `[s, p, o, g]` is hidden.
    #[inline]
    pub fn hides(&self, q: &[u64; 4]) -> bool {
        // Empty sets are skipped: a lookup in one still hashes the key.
        (!self.subjects.is_empty() && self.subjects.contains(&q[0]))
            || (!self.objects.is_empty() && self.objects.contains(&q[2]))
            || (!self.predicates.is_empty() && self.predicates.contains(&q[1]))
            || (!self.subject_predicates.is_empty()
                && self.subject_predicates.contains(&(q[0], q[1])))
            || (!self.quads.is_empty() && self.quads.contains(q))
            || self.graphs.as_ref().is_some_and(|g| !g.contains(&q[3]))
            || (!self.shared.is_empty()
                && self
                    .shared
                    .iter()
                    .any(|s| s.hides(q[0]) || (s.references && s.hides(q[2]))))
    }

    /// Whether every quad with this subject, predicate or object (any of them `None`) is
    /// hidden: a scan with these fixed reads nothing (#204).
    pub fn hides_all(&self, s: Option<u64>, p: Option<u64>, o: Option<u64>) -> bool {
        s.is_some_and(|s| self.subjects.contains(&s))
            || p.is_some_and(|p| self.predicates.contains(&p))
            || o.is_some_and(|o| self.objects.contains(&o))
            || self.shared.iter().any(|sh| {
                s.is_some_and(|s| sh.hides(s)) || (sh.references && o.is_some_and(|o| sh.hides(o)))
            })
    }

    /// Whether `resource` is hidden as a subject.
    pub fn hides_resource(&self, resource: crate::TermId) -> bool {
        self.subjects.contains(&resource.0) || self.shared.iter().any(|s| s.hides(resource.0))
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
        for shared in &self.shared {
            (shared.hidden.key, &shared.shown, shared.references).hash(&mut h);
        }
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

    /// A shared set hides its resources but the shown ones, as subjects and with references
    /// as objects; forgetting a resource shows it; filters sharing the set with other shown
    /// resources differ, and the same shown resources give equal filters.
    #[test]
    fn a_shared_set_hides_all_but_the_shown() {
        let hidden = Arc::new(SharedHidden::new([1, 2, 3]));
        let mut a = RowFilter::new();
        a.hide_shared(Arc::clone(&hidden), [2], true);
        assert!(a.hides(&[1, 9, 9, 0]));
        assert!(a.hides(&[9, 9, 3, 0]), "a reference to a hidden resource");
        assert!(!a.hides(&[2, 9, 9, 0]));
        assert!(!a.hides(&[9, 9, 2, 0]));
        assert!(a.hides_all(Some(1), None, None));
        assert!(a.hides_all(None, None, Some(3)));
        assert!(!a.hides_all(Some(2), None, None));
        assert!(a.hides_resource(TermId(3)));
        assert!(!a.is_empty());
        let mut b = RowFilter::new();
        b.hide_shared(Arc::clone(&hidden), [3], true);
        assert_ne!(a, b);
        assert_ne!(a.fingerprint(), b.fingerprint());
        let mut c = RowFilter::new();
        c.hide_shared(Arc::clone(&hidden), [2], true);
        assert_eq!(a, c);
        // Another set of the same ids is equal; one of other ids is not, even with its key.
        let mut copy = RowFilter::new();
        copy.hide_shared(Arc::new(SharedHidden::new([3, 2, 1])), [2], true);
        assert_eq!(a, copy);
        let mut forged = SharedHidden::new([1, 2, 4]);
        forged.key = hidden.key;
        let mut other = RowFilter::new();
        other.hide_shared(Arc::new(forged), [2], true);
        assert_ne!(a, other);
        assert_eq!(a.fingerprint(), c.fingerprint());
        a.forget(&[1]);
        assert!(!a.hides(&[1, 9, 9, 0]));
        assert_ne!(a.fingerprint(), c.fingerprint());
        let mut plain = RowFilter::new();
        plain.hide_shared(hidden, [], false);
        assert!(plain.hides(&[2, 9, 9, 0]));
        assert!(!plain.hides(&[9, 9, 2, 0]), "references are not hidden");
        assert!(plain.merged(&RowFilter::new()).hides(&[1, 9, 9, 0]));
        // Many shown resources (a bitmap of them), dense and sparse hidden sets.
        for step in [1, 1000] {
            let hidden = Arc::new(SharedHidden::new((0..500).map(|i| 10 + i * step)));
            let mut many = RowFilter::new();
            many.hide_shared(hidden, (0..100).map(|i| 10 + i * 3 * step), false);
            for i in 0..500 {
                let id = 10 + i * step;
                assert_eq!(
                    many.hides(&[id, 9, 9, 0]),
                    i % 3 != 0 || i >= 300,
                    "{step} {i}"
                );
            }
            assert!(!many.hides(&[5, 9, 9, 0]));
            assert!(!many.hides(&[10 + 500 * step, 9, 9, 0]));
            many.forget(&[10 + step]);
            assert!(!many.hides(&[10 + step, 9, 9, 0]));
        }
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
