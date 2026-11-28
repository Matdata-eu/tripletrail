//! A packed R-tree of [`Entry`]s (#393): the entries in one array in sort-tile-recursive
//! order, and above them levels of node boxes, each node the box of [`FANOUT`] consecutive
//! items of the level below. An entry takes its 20 bytes and about one more for the nodes,
//! against 40 and about 7 for an rstar tree, whose nodes are an enum as large as a parent.
//!
//! The packed part is built once. Entries inserted afterwards go to a small rstar tree, and
//! removed ones are left out by slot until the next bulk load; once they are many, the tree
//! is packed again.

use super::Entry;
use rstar::{AABB, Envelope, RTree};
use std::collections::{BinaryHeap, HashSet};

/// Items per node.
const FANOUT: usize = 16;

/// The entries of one coordinate space, packed, with the changes since.
#[derive(Clone, Default)]
pub(crate) struct Tree {
    entries: Vec<Entry>,
    /// The node boxes, from the level just above the entries to the root (one box).
    levels: Vec<Vec<AABB<[f32; 2]>>>,
    /// Entries inserted since the packing.
    added: RTree<Entry>,
    /// Slots of packed entries removed since the packing.
    removed: HashSet<u32>,
}

fn centre(e: &Entry) -> [f32; 2] {
    let (lo, hi) = (e.envelope.lower(), e.envelope.upper());
    [lo[0] / 2.0 + hi[0] / 2.0, lo[1] / 2.0 + hi[1] / 2.0]
}

fn merged(boxes: &[AABB<[f32; 2]>]) -> AABB<[f32; 2]> {
    boxes
        .iter()
        .copied()
        .reduce(|a, b| a.merged(&b))
        .unwrap_or_else(AABB::new_empty)
}

impl Tree {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Packs `entries`: sorted by the x of their centres into vertical slices, each slice by
    /// y, so that consecutive entries lie close together.
    pub(crate) fn bulk_load(mut entries: Vec<Entry>) -> Self {
        entries.shrink_to_fit();
        let by =
            |axis: usize| move |a: &Entry, b: &Entry| centre(a)[axis].total_cmp(&centre(b)[axis]);
        entries.sort_unstable_by(by(0));
        let leaves = entries.len().div_ceil(FANOUT);
        let slices = (leaves as f64).sqrt().ceil().max(1.0) as usize;
        let per_slice = leaves.div_ceil(slices) * FANOUT;
        for slice in entries.chunks_mut(per_slice.max(1)) {
            slice.sort_unstable_by(by(1));
        }
        let mut levels: Vec<Vec<AABB<[f32; 2]>>> = Vec::new();
        let mut below: Vec<AABB<[f32; 2]>> = entries
            .chunks(FANOUT)
            .map(|c| {
                c.iter()
                    .map(|e| e.envelope)
                    .reduce(|a, b| a.merged(&b))
                    .expect("not empty")
            })
            .collect();
        while !below.is_empty() {
            let done = below.len() == 1;
            let next: Vec<_> = below.chunks(FANOUT).map(merged).collect();
            below.shrink_to_fit();
            levels.push(below);
            if done {
                break;
            }
            below = next;
        }
        Self {
            entries,
            levels,
            added: RTree::new(),
            removed: HashSet::new(),
        }
    }

    pub(crate) fn size(&self) -> usize {
        self.entries.len() - self.removed.len() + self.added.size()
    }

    /// A box holding every entry (after removals, perhaps more than their box).
    pub(crate) fn envelope(&self) -> AABB<[f32; 2]> {
        let packed = self.levels.last().map(|root| root[0]);
        match (packed, self.added.size()) {
            (Some(p), 0) => p,
            (Some(p), _) => p.merged(&self.added.root().envelope()),
            (None, _) => self.added.root().envelope(),
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.packed().chain(self.added.iter())
    }

    fn packed(&self) -> impl Iterator<Item = &Entry> {
        let removed = &self.removed;
        self.entries
            .iter()
            .filter(move |e| removed.is_empty() || !removed.contains(&e.slot))
    }

    /// The entries, for a new bulk load.
    pub(crate) fn into_entries(self) -> Vec<Entry> {
        let mut out: Vec<Entry> = self.packed().copied().collect();
        out.extend(self.added.iter().copied());
        out
    }

    pub(crate) fn insert(&mut self, e: Entry) {
        self.added.insert(e);
        self.repack_if_changed_much();
    }

    /// Removes the entry with the slot of `e`.
    pub(crate) fn remove(&mut self, e: &Entry) -> Option<Entry> {
        if let Some(e) = self.added.remove(e) {
            return Some(e);
        }
        let found = self
            .locate_in_envelope_intersecting(e.envelope)
            .find(|x| x.slot == e.slot)
            .copied()?;
        self.removed.insert(found.slot);
        self.repack_if_changed_much();
        Some(found)
    }

    /// Packs again once the changes are an eighth of the packed entries (at least 1024).
    fn repack_if_changed_much(&mut self) {
        if self.added.size() + self.removed.len() > (self.entries.len() / 8).max(1024) {
            *self = Self::bulk_load(std::mem::take(self).into_entries());
        }
    }

    /// The entries whose box meets `envelope`.
    pub(crate) fn locate_in_envelope_intersecting(
        &self,
        envelope: AABB<[f32; 2]>,
    ) -> impl Iterator<Item = &Entry> {
        let mut stack = Vec::new();
        if let Some(top) = self.levels.len().checked_sub(1) {
            stack.push((top, 0));
        }
        Window {
            tree: self,
            envelope,
            stack,
            leaf: 0..0,
        }
        .chain(self.added.locate_in_envelope_intersecting(envelope))
    }

    /// The entries by the distance of their box from `point`, nearest first, with the
    /// distance squared.
    pub(crate) fn nearest_neighbor_iter_with_distance_2(
        &self,
        point: [f32; 2],
    ) -> impl Iterator<Item = (&Entry, f32)> {
        let mut heap = BinaryHeap::new();
        if let Some(top) = self.levels.len().checked_sub(1) {
            let d = self.levels[top][0].distance_2(&point);
            heap.push(Near {
                distance: d,
                level: top + 1,
                at: 0,
            });
        }
        Merge {
            a: Nearest {
                tree: self,
                point,
                heap,
            }
            .peekable(),
            b: self
                .added
                .nearest_neighbor_iter_with_distance_2(point)
                .peekable(),
        }
    }

    /// Roughly the heap bytes of the tree.
    pub(crate) fn heap_size(&self) -> usize {
        let boxes: usize = self.levels.iter().map(|l| l.capacity()).sum();
        self.entries.capacity() * size_of::<Entry>()
            + boxes * size_of::<AABB<[f32; 2]>>()
            + self.added.size() * size_of::<rstar::RTreeNode<Entry>>() * 7 / 6
            + self.removed.capacity() * (size_of::<u32>() + 1)
    }
}

/// The items of node `at` of `level` (0: the entries).
fn children(level: usize, at: usize, len: usize) -> std::ops::Range<usize> {
    let _ = level;
    let start = at * FANOUT;
    start.min(len)..(start + FANOUT).min(len)
}

/// [`Tree::locate_in_envelope_intersecting`] over the packed entries.
struct Window<'t> {
    tree: &'t Tree,
    envelope: AABB<[f32; 2]>,
    /// Nodes still to look into: (level in `levels`, index).
    stack: Vec<(usize, usize)>,
    /// Entries of the leaf being read.
    leaf: std::ops::Range<usize>,
}

impl<'t> Iterator for Window<'t> {
    type Item = &'t Entry;

    fn next(&mut self) -> Option<&'t Entry> {
        let tree = self.tree;
        loop {
            for i in self.leaf.by_ref() {
                let e = &tree.entries[i];
                if e.envelope.intersects(&self.envelope)
                    && (tree.removed.is_empty() || !tree.removed.contains(&e.slot))
                {
                    return Some(e);
                }
            }
            let (level, at) = self.stack.pop()?;
            if !tree.levels[level][at].intersects(&self.envelope) {
                continue;
            }
            if level == 0 {
                self.leaf = children(0, at, tree.entries.len());
            } else {
                for child in children(level, at, tree.levels[level - 1].len()).rev() {
                    self.stack.push((level - 1, child));
                }
            }
        }
    }
}

/// A node (`level` ≥ 1: `levels[level - 1]`) or an entry (`level` 0) in the queue of
/// [`Nearest`], by distance.
#[derive(PartialEq)]
struct Near {
    distance: f32,
    level: usize,
    at: usize,
}

impl Eq for Near {}

impl PartialOrd for Near {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Reversed, so that the heap pops the nearest first (entries before nodes at a tie).
impl Ord for Near {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .distance
            .total_cmp(&self.distance)
            .then_with(|| other.level.cmp(&self.level))
    }
}

/// [`Tree::nearest_neighbor_iter_with_distance_2`] over the packed entries.
struct Nearest<'t> {
    tree: &'t Tree,
    point: [f32; 2],
    heap: BinaryHeap<Near>,
}

impl<'t> Iterator for Nearest<'t> {
    type Item = (&'t Entry, f32);

    fn next(&mut self) -> Option<(&'t Entry, f32)> {
        let tree = self.tree;
        while let Some(Near {
            distance,
            level,
            at,
        }) = self.heap.pop()
        {
            if level == 0 {
                let e = &tree.entries[at];
                if tree.removed.is_empty() || !tree.removed.contains(&e.slot) {
                    return Some((e, distance));
                }
                continue;
            }
            if level == 1 {
                for i in children(0, at, tree.entries.len()) {
                    let d = tree.entries[i].envelope.distance_2(&self.point);
                    self.heap.push(Near {
                        distance: d,
                        level: 0,
                        at: i,
                    });
                }
            } else {
                let below = &tree.levels[level - 2];
                for i in children(level - 1, at, below.len()) {
                    let d = below[i].distance_2(&self.point);
                    self.heap.push(Near {
                        distance: d,
                        level: level - 1,
                        at: i,
                    });
                }
            }
        }
        None
    }
}

/// Two streams nearest first, merged.
struct Merge<'t, A: Iterator<Item = (&'t Entry, f32)>, B: Iterator<Item = (&'t Entry, f32)>> {
    a: std::iter::Peekable<A>,
    b: std::iter::Peekable<B>,
}

impl<'t, A, B> Iterator for Merge<'t, A, B>
where
    A: Iterator<Item = (&'t Entry, f32)>,
    B: Iterator<Item = (&'t Entry, f32)>,
{
    type Item = (&'t Entry, f32);

    fn next(&mut self) -> Option<(&'t Entry, f32)> {
        match (self.a.peek(), self.b.peek()) {
            (Some(x), Some(y)) if y.1 < x.1 => self.b.next(),
            (Some(_), _) => self.a.next(),
            (None, _) => self.b.next(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Boxes from a small deterministic generator, some large.
    fn entries(n: usize, seed: u64) -> Vec<Entry> {
        let mut s = seed;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 11) as f32 / (1u64 << 53) as f32
        };
        (0..n)
            .map(|i| {
                let (x, y) = (next() * 100.0, next() * 100.0);
                let size = if i % 97 == 0 { 30.0 } else { 0.5 };
                let (w, h) = (next() * size, next() * size);
                Entry {
                    envelope: AABB::from_corners([x, y], [x + w, y + h]),
                    slot: i as u32,
                }
            })
            .collect()
    }

    fn slots<'a>(it: impl Iterator<Item = &'a Entry>) -> Vec<u32> {
        let mut v: Vec<u32> = it.map(|e| e.slot).collect();
        v.sort_unstable();
        v
    }

    /// Windows and nearest entries are those of an rstar tree of the same entries, after
    /// inserts and removals too, and the packed tree takes less than half its memory.
    #[test]
    fn the_packed_tree_answers_as_an_rstar_tree() {
        let all = entries(20_000, 7);
        let mut packed = Tree::bulk_load(all[..15_000].to_vec());
        let mut reference = RTree::bulk_load(all[..15_000].to_vec());
        let check = |packed: &Tree, reference: &RTree<Entry>| {
            assert_eq!(packed.size(), reference.size());
            assert_eq!(slots(packed.iter()), slots(reference.iter()));
            // After removals the packed boxes may still hold a removed entry's.
            assert!(
                packed
                    .envelope()
                    .contains_envelope(&reference.root().envelope())
            );
            for (i, w) in entries(50, 11).into_iter().enumerate() {
                let window = if i % 5 == 0 {
                    AABB::from_corners([-1.0, -1.0], [101.0, 101.0])
                } else {
                    w.envelope
                };
                assert_eq!(
                    slots(packed.locate_in_envelope_intersecting(window)),
                    slots(reference.locate_in_envelope_intersecting(window))
                );
                let p = centre(&w);
                let near: Vec<f32> = packed
                    .nearest_neighbor_iter_with_distance_2(p)
                    .take(40)
                    .map(|(_, d)| d)
                    .collect();
                let expected: Vec<f32> = reference
                    .nearest_neighbor_iter_with_distance_2(p)
                    .take(40)
                    .map(|(_, d)| d)
                    .collect();
                assert_eq!(near, expected);
            }
        };
        check(&packed, &reference);
        // Inserts and removals, under the repacking threshold and past it.
        for e in &all[15_000..15_500] {
            packed.insert(*e);
            reference.insert(*e);
        }
        for e in all[..800].iter().chain(&all[15_000..15_100]) {
            assert!(packed.remove(e).is_some());
            reference.remove(e);
        }
        assert!(packed.remove(&all[0]).is_none(), "removed once");
        check(&packed, &reference);
        for e in &all[15_500..] {
            packed.insert(*e);
            reference.insert(*e);
        }
        assert!(
            packed.added.size() <= (packed.entries.len() / 8).max(1024),
            "packed again"
        );
        check(&packed, &reference);
        // Packed afresh, under half of what the rstar tree's nodes take.
        let rstar_bytes = reference.size() * size_of::<rstar::RTreeNode<Entry>>() * 7 / 6;
        let fresh = Tree::bulk_load(packed.into_entries()).heap_size();
        assert!(fresh * 2 < rstar_bytes, "{fresh} against {rstar_bytes}");
        // Empty trees.
        let empty = Tree::new();
        assert_eq!(empty.size(), 0);
        assert_eq!(
            empty
                .locate_in_envelope_intersecting(AABB::from_point([0.0, 0.0]))
                .count(),
            0
        );
        assert_eq!(
            empty
                .nearest_neighbor_iter_with_distance_2([0.0, 0.0])
                .count(),
            0
        );
    }
}
