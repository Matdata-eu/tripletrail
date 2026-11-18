//! Counts kept per segment, so `COUNT` queries over a whole triple pattern are answered
//! without scanning: quads, distinct values of each key prefix, quads and distinct values per
//! term kind of the first column, and the same per value of the first column when it has few
//! values (the graph in GSPO / GPOS / GOSP, the predicate in POSG).
//!
//! They are computed while a segment is built from sorted quads and kept in the checkpoint as
//! `stats.bin` (a cache: without it, they are computed again on first use). While the
//! permutation has a delta of uncompacted writes, [`SegStats::with_delta`] adjusts them for
//! it (issue #213).

use crate::segment::Key;
use rayon::prelude::*;

/// Term kinds are the low 4 bits of an id.
pub(crate) const KINDS: usize = 16;
/// First-column values beyond this many are not counted one by one.
pub(crate) const MAX_GROUPS: usize = 4096;

#[inline]
fn kind(id: u64) -> usize {
    (id & 0xf) as usize
}

/// Quads and distinct values per term kind of a column.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct KindCounts {
    pub(crate) rows: [u64; KINDS],
    pub(crate) distinct: [u64; KINDS],
}

/// The quads that share one value of the first column.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Group {
    pub(crate) key: u64,
    pub(crate) rows: u64,
    /// Distinct values of the second column.
    pub(crate) distinct1: u64,
    /// Kinds of the second column.
    pub(crate) kinds1: KindCounts,
    /// The sum of the squares of the quads per value of the second column. With `rows`, the
    /// mean number of quads one value of the second column has, weighted by how often values
    /// come up (as a join key bound from the data does): `square1 / rows`. In POSG that is
    /// the planner's fanout of an object, in PSOG that of a subject (#210).
    pub(crate) square1: u64,
}

impl Group {
    /// `square1 / rows`, rounded up.
    pub(crate) fn fanout1(&self) -> u64 {
        self.square1.div_ceil(self.rows.max(1))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SegStats {
    pub(crate) rows: u64,
    /// `distinct[i]`: distinct prefixes of `i + 1` columns.
    pub(crate) distinct: [u64; 4],
    /// Kinds of the first column.
    pub(crate) kinds0: KindCounts,
    /// Per value of the first column, in order; `None` when there are more than
    /// [`MAX_GROUPS`].
    pub(crate) groups: Option<Vec<Group>>,
}

impl SegStats {
    /// The group of first-column value `key`: `Some(None)` when the segment has none (the
    /// counts are 0), `None` when groups are not kept.
    pub(crate) fn group(&self, key: u64) -> Option<Option<&Group>> {
        let groups = self.groups.as_ref()?;
        Some(
            groups
                .binary_search_by_key(&key, |g| g.key)
                .ok()
                .map(|i| &groups[i]),
        )
    }
}

/// Builds [`SegStats`] from quads in sorted order, without duplicates.
#[derive(Default)]
pub(crate) struct StatsBuilder {
    stats: SegStats,
    groups: Vec<Group>,
    too_many: bool,
    prev: Option<Key>,
    /// Quads so far with the current two-column prefix.
    run1: u64,
}

impl StatsBuilder {
    #[inline]
    pub(crate) fn push(&mut self, q: &Key) {
        // The first column that differs from the quad before.
        let first = match &self.prev {
            None => 0,
            Some(p) => (0..4).find(|&i| q[i] != p[i]).unwrap_or(4),
        };
        self.prev = Some(*q);
        let s = &mut self.stats;
        s.rows += 1;
        for d in &mut s.distinct[first.min(4)..] {
            *d += 1;
        }
        let k0 = kind(q[0]);
        s.kinds0.rows[k0] += 1;
        if first == 0 {
            s.kinds0.distinct[k0] += 1;
            if !self.too_many {
                if self.groups.len() == MAX_GROUPS {
                    self.too_many = true;
                    self.groups = Vec::new();
                } else {
                    self.groups.push(Group {
                        key: q[0],
                        ..Group::default()
                    });
                }
            }
        }
        if !self.too_many
            && let Some(g) = self.groups.last_mut()
        {
            let k1 = kind(q[1]);
            g.rows += 1;
            g.kinds1.rows[k1] += 1;
            if first <= 1 {
                g.distinct1 += 1;
                g.kinds1.distinct[k1] += 1;
                self.run1 = 0;
            }
            // (n + 1)² = n² + 2n + 1
            g.square1 += 2 * self.run1 + 1;
            self.run1 += 1;
        }
    }

    pub(crate) fn finish(mut self) -> SegStats {
        self.stats.groups = (!self.too_many).then_some(self.groups);
        self.stats
    }
}

impl SegStats {
    pub(crate) fn of(quads: impl IntoIterator<Item = Key>) -> Self {
        let mut b = StatsBuilder::default();
        for q in quads {
            b.push(&q);
        }
        b.finish()
    }

    /// As little-endian `u64`s: rows, distinct (4), kinds0 (32), then the number of groups
    /// (`u64::MAX`: not kept) and per group key, rows, distinct1, square1, kinds1 (32).
    pub(crate) fn encode(&self, out: &mut Vec<u64>) {
        fn kinds(out: &mut Vec<u64>, k: &KindCounts) {
            out.extend_from_slice(&k.rows);
            out.extend_from_slice(&k.distinct);
        }
        out.push(self.rows);
        out.extend_from_slice(&self.distinct);
        kinds(out, &self.kinds0);
        match &self.groups {
            None => out.push(u64::MAX),
            Some(groups) => {
                out.push(groups.len() as u64);
                for g in groups {
                    out.extend_from_slice(&[g.key, g.rows, g.distinct1, g.square1]);
                    kinds(out, &g.kinds1);
                }
            }
        }
    }

    /// Reads what [`SegStats::encode`] wrote, advancing `at`; `None` if it is cut short.
    pub(crate) fn decode(v: &[u64], at: &mut usize) -> Option<Self> {
        fn take<'a>(v: &'a [u64], at: &mut usize, n: usize) -> Option<&'a [u64]> {
            let s = v.get(*at..at.checked_add(n)?)?;
            *at += n;
            Some(s)
        }
        fn kinds(v: &[u64], at: &mut usize) -> Option<KindCounts> {
            Some(KindCounts {
                rows: take(v, at, KINDS)?.try_into().ok()?,
                distinct: take(v, at, KINDS)?.try_into().ok()?,
            })
        }
        let rows = take(v, at, 1)?[0];
        let distinct = take(v, at, 4)?.try_into().ok()?;
        let kinds0 = kinds(v, at)?;
        let n = take(v, at, 1)?[0];
        let groups = if n == u64::MAX {
            None
        } else {
            let n = usize::try_from(n).ok().filter(|n| *n <= MAX_GROUPS)?;
            let mut groups = Vec::with_capacity(n);
            for _ in 0..n {
                let h = take(v, at, 4)?;
                groups.push(Group {
                    key: h[0],
                    rows: h[1],
                    distinct1: h[2],
                    square1: h[3],
                    kinds1: kinds(v, at)?,
                });
            }
            Some(groups)
        };
        Some(Self {
            rows,
            distinct,
            kinds0,
            groups,
        })
    }
}

#[inline]
fn add(x: &mut u64, d: i64) {
    *x = x.wrapping_add_signed(d);
}

impl SegStats {
    /// These counts (those of `segment`) with a delta applied: `delta` holds the delta's
    /// keys in order, `1` for a quad added (not in `segment`) and `-1` for one removed (in
    /// `segment`). Exact, except that groups not kept stay not kept. Costs a range count of
    /// `segment` per distinct prefix of one, two and three columns in the delta, so a
    /// handful of block reads per delta quad, not a scan.
    pub(crate) fn with_delta(
        &self,
        count: &(dyn Fn(Key, Key) -> usize + Sync),
        delta: &[(Key, i64)],
    ) -> SegStats {
        let mut s = self.clone();
        let mut groups = s.groups.take();
        for (k, d) in delta {
            add(&mut s.rows, *d);
            add(&mut s.distinct[3], *d);
            add(&mut s.kinds0.rows[kind(k[0])], *d);
        }
        let group = |groups: &mut Vec<Group>, key: u64| -> usize {
            match groups.binary_search_by_key(&key, |g| g.key) {
                Ok(i) => i,
                Err(i) => {
                    groups.insert(
                        i,
                        Group {
                            key,
                            ..Group::default()
                        },
                    );
                    i
                }
            }
        };
        // Whether the segment had the prefix of the level before, by delta position: a prefix
        // it lacked has no longer prefixes in it either, so they need no seek (most of a
        // large insert of new terms).
        let mut had = vec![true; delta.len()];
        for len in 1..=3 {
            // The range counts of a level first, in parallel: on a store larger than memory
            // each can be a read from disk, and a fold of 70k changes takes ~140k of them per
            // permutation (#437).
            let runs: Vec<(usize, &[(Key, i64)])> = delta
                .chunk_by(|a, b| a.0[..len] == b.0[..len])
                .scan(0, |at, run| {
                    let start = *at;
                    *at += run.len();
                    Some((start, run))
                })
                .collect();
            let olds: Vec<i64> = runs
                .par_iter()
                .with_min_len(64)
                .map(|&(at, run)| {
                    if had[at] {
                        let k = run[0].0;
                        let (mut low, mut high) = ([0; 4], [u64::MAX; 4]);
                        low[..len].copy_from_slice(&k[..len]);
                        high[..len].copy_from_slice(&k[..len]);
                        count(low, high) as i64
                    } else {
                        0
                    }
                })
                .collect();
            for (&(at, run), old) in runs.iter().zip(olds) {
                let k = run[0].0;
                let net: i64 = run.iter().map(|(_, d)| d).sum();
                had[at..at + run.len()].fill(old > 0);
                let new = old + net;
                // A prefix that comes or goes.
                let change = i64::from(new > 0) - i64::from(old > 0);
                add(&mut s.distinct[len - 1], change);
                match (len, groups.as_mut()) {
                    (1, groups) => {
                        add(&mut s.kinds0.distinct[kind(k[0])], change);
                        if let Some(groups) = groups {
                            let i = group(groups, k[0]);
                            add(&mut groups[i].rows, net);
                        }
                    }
                    (2, Some(groups)) => {
                        let i = group(groups, k[0]);
                        let g = &mut groups[i];
                        add(&mut g.distinct1, change);
                        add(&mut g.kinds1.distinct[kind(k[1])], change);
                        add(&mut g.kinds1.rows[kind(k[1])], net);
                        add(&mut g.square1, new * new - old * old);
                    }
                    _ => {}
                }
            }
        }
        s.groups = groups
            .map(|mut g| {
                g.retain(|g| g.rows > 0);
                g
            })
            .filter(|g| g.len() <= MAX_GROUPS);
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::Segment;
    use std::sync::Arc;

    fn brute(quads: &[Key]) -> SegStats {
        use std::collections::{BTreeMap, BTreeSet};
        let mut s = SegStats {
            rows: quads.len() as u64,
            ..SegStats::default()
        };
        for i in 0..4 {
            s.distinct[i] = quads
                .iter()
                .map(|q| q[..=i].to_vec())
                .collect::<BTreeSet<_>>()
                .len() as u64;
        }
        let firsts: BTreeSet<u64> = quads.iter().map(|q| q[0]).collect();
        for q in quads {
            s.kinds0.rows[kind(q[0])] += 1;
        }
        for f in &firsts {
            s.kinds0.distinct[kind(*f)] += 1;
        }
        let mut groups: BTreeMap<u64, (Group, BTreeSet<u64>)> = BTreeMap::new();
        for q in quads {
            let (g, seen) = groups.entry(q[0]).or_insert_with(|| {
                (
                    Group {
                        key: q[0],
                        ..Group::default()
                    },
                    BTreeSet::new(),
                )
            });
            g.rows += 1;
            g.kinds1.rows[kind(q[1])] += 1;
            let same = quads
                .iter()
                .filter(|r| r[0] == q[0] && r[1] == q[1])
                .count() as u64;
            // Each quad of a value adds that value's count: Σ count² in all.
            g.square1 += same;
            if seen.insert(q[1]) {
                g.distinct1 += 1;
                g.kinds1.distinct[kind(q[1])] += 1;
            }
        }
        s.groups =
            (groups.len() <= MAX_GROUPS).then(|| groups.into_values().map(|g| g.0).collect());
        s
    }

    #[test]
    fn counts_match_a_brute_force_count() {
        let mut quads: Vec<Key> = Vec::new();
        let mut x: u64 = 7;
        for _ in 0..5000 {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            quads.push([(x >> 60) % 5, (x >> 50) % 23, (x >> 40) % 97, (x >> 33) % 3]);
        }
        quads.sort_unstable();
        quads.dedup();
        let s = SegStats::of(quads.iter().copied());
        assert_eq!(s, brute(&quads));
        let mut v = Vec::new();
        s.encode(&mut v);
        let mut at = 0;
        assert_eq!(SegStats::decode(&v, &mut at), Some(s));
        assert_eq!(at, v.len());
        assert_eq!(SegStats::decode(&v[..v.len() - 1], &mut 0), None);

        // Too many first-column values: no groups.
        let many: Vec<Key> = (0..MAX_GROUPS as u64 + 1).map(|i| [i, 0, 0, 0]).collect();
        let s = SegStats::of(many.iter().copied());
        assert_eq!(s.groups, None);
        assert_eq!(s, brute(&many));
        assert_eq!(
            SegStats::of([]),
            SegStats {
                groups: Some(Vec::new()),
                ..SegStats::default()
            }
        );
    }

    #[test]
    fn counts_with_a_delta_match_the_counts_of_the_result() {
        let mut x: u64 = 11;
        let mut next = |m: u64| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 33) % m
        };
        for round in 0..40 {
            let (a, b, c) = (2 + round % 6, 3 + round % 17, 2 + round % 29);
            let mut base: Vec<Key> = (0..(round * 37) % 900)
                .map(|_| [next(a), next(b), next(c), next(3)])
                .collect();
            base.sort_unstable();
            base.dedup();
            let segment = Arc::new(Segment::from_sorted(base.iter().copied()));
            let mut after: std::collections::BTreeSet<Key> = base.iter().copied().collect();
            let mut delta = std::collections::BTreeMap::new();
            for _ in 0..(round * 13) % 200 {
                let q = [next(a + 1), next(b), next(c), next(3)];
                if after.remove(&q) {
                    if delta.remove(&q).is_none() {
                        delta.insert(q, -1);
                    }
                } else {
                    after.insert(q);
                    if delta.remove(&q).is_none() {
                        delta.insert(q, 1);
                    }
                }
            }
            let delta: Vec<(Key, i64)> = delta.into_iter().collect();
            let got = segment
                .stats()
                .with_delta(&|l, h| segment.count_range(l, h), &delta);
            assert_eq!(got, SegStats::of(after.iter().copied()), "round {round}");
        }
        // Groups not kept stay not kept.
        let many: Vec<Key> = (0..MAX_GROUPS as u64 + 1).map(|i| [i, 0, 0, 0]).collect();
        let segment = Arc::new(Segment::from_sorted(many.iter().copied()));
        let got = segment
            .stats()
            .with_delta(&|l, h| segment.count_range(l, h), &[([0, 0, 0, 0], -1)]);
        let mut want = SegStats::of(many[1..].iter().copied());
        want.groups = None;
        assert_eq!(got, want);
    }
}
