//! Parallel sweep-line join of two sets of bounding boxes.
//!
//! Both inputs are sorted by `minx` and swept left to right, each side keeping the boxes still
//! open at the sweep position (its active set): a box is tested against the other side's
//! active boxes when it opens. The sweep axis is cut into stripes holding about the same
//! number of boxes, swept in parallel. A box goes into every stripe its x range meets, and a
//! pair is reported only by the stripe holding the lower-left corner of the two boxes'
//! intersection, the larger `minx` of the two, so each pair comes out once.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// A bounding box: lower and upper corners.
pub(crate) type Bbox = ([f64; 2], [f64; 2]);

/// Boxes a stripe's sweep takes between two looks at its `stop`.
const STOP_EVERY: usize = 4096;

/// Stripes per thread: more, smaller stripes even out the work when the data is uneven.
const STRIPES_PER_THREAD: usize = 4;

/// The pairs `(i, j)` of a box `left[i]` and a box `right[j]` that meet (closed boxes: touching
/// counts), and for which `test(i, j)` holds, stripe by stripe. `None` if more than `limit`
/// pairs pass.
pub(crate) fn join(
    left: &[Bbox],
    right: &[Bbox],
    threads: usize,
    limit: usize,
    test: &(dyn Fn(usize, usize) -> bool + Sync),
) -> Option<Vec<(usize, usize)>> {
    join_until(left, right, threads, limit, test, &|| false)
}

/// [`join`] that gives up, with `None`, once `stop` holds (a cancelled query): it is checked
/// between the steps that sort the boxes, between stripes, and every [`STOP_EVERY`] boxes of a
/// stripe's sweep. A sweep over millions of boxes otherwise runs on for many seconds after
/// its query's timeout, even with a `test` that fails at once (#338).
pub(crate) fn join_until(
    left: &[Bbox],
    right: &[Bbox],
    threads: usize,
    limit: usize,
    test: &(dyn Fn(usize, usize) -> bool + Sync),
    stop: &(dyn Fn() -> bool + Sync),
) -> Option<Vec<(usize, usize)>> {
    if left.is_empty() || right.is_empty() {
        return Some(Vec::new());
    }
    if stop() {
        return None;
    }
    let threads = threads.max(1);
    let bounds = stripe_bounds(left, right, threads * STRIPES_PER_THREAD);
    let stripes = bounds.len() + 1;
    if stop() {
        return None;
    }
    let l_stripes = split(left, &bounds);
    if stop() {
        return None;
    }
    let r_stripes = split(right, &bounds);
    let next = AtomicUsize::new(0);
    let found = AtomicUsize::new(0);
    let over = AtomicBool::new(false);
    // Each worker takes the next stripe until none is left.
    let work = || {
        let mut out: Vec<(usize, Vec<(usize, usize)>)> = Vec::new();
        loop {
            let s = next.fetch_add(1, Ordering::Relaxed);
            if s >= stripes || over.load(Ordering::Relaxed) {
                return out;
            }
            if stop() {
                over.store(true, Ordering::Relaxed);
                return out;
            }
            let start = if s == 0 {
                f64::NEG_INFINITY
            } else {
                bounds[s - 1]
            };
            let mut pairs = Vec::new();
            sweep(
                left,
                right,
                &l_stripes[s],
                &r_stripes[s],
                start,
                test,
                stop,
                &mut pairs,
            );
            let n = pairs.len();
            out.push((s, pairs));
            if found.fetch_add(n, Ordering::Relaxed) + n > limit {
                over.store(true, Ordering::Relaxed);
                return out;
            }
        }
    };
    let mut parts: Vec<(usize, Vec<(usize, usize)>)> = if threads == 1 || stripes == 1 {
        work()
    } else {
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..threads.min(stripes))
                .map(|_| scope.spawn(work))
                .collect();
            workers
                .into_iter()
                .flat_map(|w| w.join().unwrap())
                .collect()
        })
    };
    if over.load(Ordering::Relaxed) {
        return None;
    }
    // Stripe order, so the result does not depend on which worker took which stripe.
    parts.sort_unstable_by_key(|(s, _)| *s);
    Some(parts.into_iter().flat_map(|(_, p)| p).collect())
}

/// `f` of each item, computed in chunks of at least `min_chunk` items on `threads` threads
/// (a smaller `min_chunk` for costlier `f`).
pub(crate) fn par_map<T: Sync, U: Send>(
    items: &[T],
    threads: usize,
    min_chunk: usize,
    f: impl Fn(&T) -> U + Sync,
) -> Vec<U> {
    let chunk = items
        .len()
        .div_ceil(threads.max(1) * 4)
        .max(min_chunk.max(1));
    if items.len() <= chunk {
        return items.iter().map(f).collect();
    }
    let f = &f;
    std::thread::scope(|scope| {
        let workers: Vec<_> = items
            .chunks(chunk)
            .map(|part| scope.spawn(move || part.iter().map(f).collect::<Vec<U>>()))
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().unwrap())
            .collect()
    })
}

/// Interior stripe boundaries: `minx` quantiles of both inputs, without repeats. Stripe `s`
/// covers `[bounds[s - 1], bounds[s])`, the first and last stripes reaching to infinity.
fn stripe_bounds(left: &[Bbox], right: &[Bbox], stripes: usize) -> Vec<f64> {
    let mut xs: Vec<f64> = left.iter().chain(right).map(|b| b.0[0]).collect();
    xs.sort_unstable_by(f64::total_cmp);
    let mut bounds: Vec<f64> = (1..stripes).map(|k| xs[k * xs.len() / stripes]).collect();
    bounds.dedup();
    // A boundary at the smallest `minx` would leave the first stripe empty.
    bounds.retain(|b| *b > xs[0]);
    bounds
}

fn stripe_of(bounds: &[f64], x: f64) -> usize {
    bounds.partition_point(|b| *b <= x)
}

/// For each stripe, the boxes meeting it, sorted by `minx`.
fn split(boxes: &[Bbox], bounds: &[f64]) -> Vec<Vec<u32>> {
    let mut order: Vec<u32> = (0..boxes.len() as u32).collect();
    order.sort_unstable_by(|&i, &j| boxes[i as usize].0[0].total_cmp(&boxes[j as usize].0[0]));
    let mut out = vec![Vec::new(); bounds.len() + 1];
    for i in order {
        let (lo, hi) = boxes[i as usize];
        // In `minx` order, so each stripe's list is sorted too.
        let first = stripe_of(bounds, lo[0]);
        for stripe in &mut out[first..=stripe_of(bounds, hi[0]).max(first)] {
            stripe.push(i);
        }
    }
    out
}

/// Open boxes of one side. A sweep starts plain: every open box in `short`, and a box that
/// opens tests all the other side's open boxes. When one side has more than [`BAND_AFTER`]
/// boxes open at once (on a grid of rail lines, a whole north-south line's elements share one
/// `x` range), both sides switch to bands of `y`: a short box (meeting at most [`MAX_BANDS`]
/// bands) is in each band it meets and in `short`, a tall one only in `tall`. A short box that
/// opens then tests the other side's boxes in the bands it meets and its tall boxes; a tall box
/// tests them all (`short` and `tall`), as the plain sweep does. Tall boxes stay out of the
/// bands, which they would fill and visit all of.
struct Active {
    bands: Vec<Vec<u32>>,
    short: Vec<u32>,
    tall: Vec<u32>,
    /// `short`'s length when last pruned.
    pruned: usize,
}

/// Bands a short box meets at most.
const MAX_BANDS: usize = 4;
/// Open boxes on one side past which the sweep switches to bands.
const BAND_AFTER: usize = 256;

/// The bands of a stripe's `y` range: `y0`, and bands per unit of `y`.
#[derive(Clone, Copy)]
struct Bands {
    y0: f64,
    scale: f64,
    n: usize,
}

impl Bands {
    fn of(&self, y: f64) -> usize {
        let b = (y - self.y0) * self.scale;
        // NaN and below the range: band 0.
        if b >= 0.0 {
            (b as usize).min(self.n - 1)
        } else {
            0
        }
    }

    /// The bands `[lo, hi]` meets, if at most [`MAX_BANDS`] (else it is tall).
    fn short(&self, lo: f64, hi: f64) -> Option<(usize, usize)> {
        let (first, last) = (self.of(lo), self.of(hi));
        let last = last.max(first);
        (last - first < MAX_BANDS).then_some((first, last))
    }
}

#[cfg(test)]
thread_local! {
    /// Band entries pushed and visited, to bound the work in tests without timing.
    static BAND_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[inline]
fn count_band_work(_n: usize) {
    #[cfg(test)]
    BAND_WORK.with(|w| w.set(w.get() + _n));
}

impl Active {
    fn new() -> Self {
        Self {
            bands: Vec::new(),
            short: Vec::new(),
            tall: Vec::new(),
            pruned: 0,
        }
    }

    /// Adds `id` (of `own`), opening at `x`.
    fn insert(&mut self, bands: Option<&Bands>, own: &[Bbox], id: u32, x: f64) {
        let (lo, hi) = own[id as usize];
        let Some(bands) = bands else {
            self.short.push(id);
            return;
        };
        match bands.short(lo[1], hi[1]) {
            Some((first, last)) => {
                count_band_work(last - first + 1);
                for band in &mut self.bands[first..=last] {
                    band.push(id);
                }
                // `short` is only scanned by tall boxes: drop its closed boxes now and then.
                if self.short.len() >= 2 * self.pruned + BAND_AFTER {
                    self.short.retain(|&o| own[o as usize].1[0] >= x);
                    self.pruned = self.short.len();
                }
                self.short.push(id);
            }
            None => self.tall.push(id),
        }
    }

    /// Moves the open boxes (`short`, at `x`) into bands.
    fn band(&mut self, bands: &Bands, own: &[Bbox], x: f64) {
        self.bands = vec![Vec::new(); bands.n];
        let open = std::mem::take(&mut self.short);
        self.pruned = 0;
        for id in open {
            if own[id as usize].1[0] >= x {
                self.insert(Some(bands), own, id, x);
            }
        }
    }
}

/// Tests the box `id` (of `own`, from `[lo, hi]`) against the open boxes `list` of the other
/// side that meet it in `y`, dropping the ones closed before `lo[0]`. `owns` says whether the
/// pair is this list's to report (the bands: the band of its lower edge).
#[allow(clippy::too_many_arguments)]
fn scan(
    list: &mut Vec<u32>,
    other: &[Bbox],
    id: u32,
    (lo, hi): Bbox,
    is_left: bool,
    report: bool,
    owns: impl Fn(f64) -> bool,
    test: &(dyn Fn(usize, usize) -> bool + Sync),
    out: &mut Vec<(usize, usize)>,
) {
    // Boxes of the other side closed before this one opens meet nothing from here on.
    list.retain(|&o| other[o as usize].1[0] >= lo[0]);
    if !report {
        return;
    }
    for &o in list.iter() {
        let (olo, ohi) = other[o as usize];
        if olo[1] <= hi[1] && lo[1] <= ohi[1] && owns(lo[1].max(olo[1])) {
            let (i, j) = if is_left {
                (id as usize, o as usize)
            } else {
                (o as usize, id as usize)
            };
            if test(i, j) {
                out.push((i, j));
            }
        }
    }
}

/// The bands for a stripe: about 64 boxes per band, up to 4096, over the `y` range of its
/// boxes. `None` if one band would do.
fn bands_of(left: &[Bbox], right: &[Bbox], l_ids: &[u32], r_ids: &[u32]) -> Option<Bands> {
    let (mut y0, mut y1) = (f64::INFINITY, f64::NEG_INFINITY);
    for (&i, own) in l_ids
        .iter()
        .map(|i| (i, left))
        .chain(r_ids.iter().map(|i| (i, right)))
    {
        let (lo, hi) = own[i as usize];
        y0 = y0.min(lo[1]);
        y1 = y1.max(hi[1]);
    }
    let n = ((l_ids.len() + r_ids.len()) / 64).clamp(1, 4096);
    (n > 1 && y1 > y0).then(|| Bands {
        y0,
        scale: n as f64 / (y1 - y0),
        n,
    })
}

/// Sweeps one stripe starting at `start`, appending the pairs it owns to `out`.
#[allow(clippy::too_many_arguments)]
fn sweep(
    left: &[Bbox],
    right: &[Bbox],
    l_ids: &[u32],
    r_ids: &[u32],
    start: f64,
    test: &(dyn Fn(usize, usize) -> bool + Sync),
    stop: &(dyn Fn() -> bool + Sync),
    out: &mut Vec<(usize, usize)>,
) {
    // Worked out when the sweep first needs them.
    let mut bands: Option<Bands> = None;
    let mut may_band = true;
    let (mut li, mut ri) = (0, 0);
    let mut l_active = Active::new();
    let mut r_active = Active::new();
    while li < l_ids.len() || ri < r_ids.len() {
        // The caller sees `stop` too, and drops what this stripe found.
        if (li + ri) % STOP_EVERY == 0 && stop() {
            return;
        }
        let take_left = ri == r_ids.len()
            || (li < l_ids.len()
                && left[l_ids[li] as usize].0[0] <= right[r_ids[ri] as usize].0[0]);
        let (id, own, other, other_active, own_active, is_left) = if take_left {
            li += 1;
            let id = l_ids[li - 1];
            (id, left, right, &mut r_active, &mut l_active, true)
        } else {
            ri += 1;
            let id = r_ids[ri - 1];
            (id, right, left, &mut l_active, &mut r_active, false)
        };
        let b = own[id as usize];
        if bands.is_none() {
            // The plain sweep, until many boxes are open at once.
            let (lo, hi) = b;
            let list = &mut other_active.short;
            list.retain(|&o| other[o as usize].1[0] >= lo[0]);
            if lo[0] >= start {
                for &o in list.iter() {
                    let (olo, ohi) = other[o as usize];
                    if olo[1] <= hi[1] && lo[1] <= ohi[1] {
                        let (i, j) = if is_left {
                            (id as usize, o as usize)
                        } else {
                            (o as usize, id as usize)
                        };
                        if test(i, j) {
                            out.push((i, j));
                        }
                    }
                }
            }
            if may_band && list.len() > BAND_AFTER {
                match bands_of(left, right, l_ids, r_ids) {
                    Some(bs) => {
                        other_active.band(&bs, other, lo[0]);
                        own_active.band(&bs, own, lo[0]);
                        bands = Some(bs);
                    }
                    None => may_band = false,
                }
            }
            own_active.insert(bands.as_ref(), own, id, lo[0]);
            continue;
        }
        // The pair's lower-left corner is at this box's `minx` (the larger of the two): this
        // stripe reports it only if that lies in the stripe. Boxes opened before the stripe
        // are only there to be met by the stripe's own.
        let report = b.0[0] >= start;
        let any = |_: f64| true;
        // Banded. Each pair is found once, when its second box opens: from the other side's tall list,
        // or from its short boxes (in the bands for a short box, all of them for a tall one).
        scan(
            &mut other_active.tall,
            other,
            id,
            b,
            is_left,
            report,
            any,
            test,
            out,
        );
        match bands
            .as_ref()
            .and_then(|bs| bs.short(b.0[1], b.1[1]).map(|r| (bs, r)))
        {
            Some((bs, (first, last))) => {
                for band in first..=last {
                    count_band_work(1 + other_active.bands[band].len());
                    // A pair is in every band both boxes meet: the band of its lower edge (the
                    // larger `miny`) reports it.
                    scan(
                        &mut other_active.bands[band],
                        other,
                        id,
                        b,
                        is_left,
                        report,
                        |y| bs.of(y) == band,
                        test,
                        out,
                    );
                }
            }
            None => {
                scan(
                    &mut other_active.short,
                    other,
                    id,
                    b,
                    is_left,
                    report,
                    any,
                    test,
                    out,
                );
            }
        }
        own_active.insert(bands.as_ref(), own, id, b.0[0]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small deterministic generator, so the test needs no dependency.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    fn boxes(rng: &mut Lcg, n: usize, size: f64) -> Vec<Bbox> {
        (0..n)
            .map(|_| {
                // Coordinates on a coarse grid, so boxes often share edges and `minx` values.
                let (x, y) = (
                    (rng.next() * 100.0).round() / 10.0,
                    (rng.next() * 100.0).round() / 10.0,
                );
                let (w, h) = (
                    (rng.next() * size * 10.0).round() / 10.0,
                    (rng.next() * size * 10.0).round() / 10.0,
                );
                ([x, y], [x + w, y + h])
            })
            .collect()
    }

    fn brute(
        left: &[Bbox],
        right: &[Bbox],
        test: impl Fn(usize, usize) -> bool,
    ) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for (i, a) in left.iter().enumerate() {
            for (j, b) in right.iter().enumerate() {
                let meet =
                    a.0[0] <= b.1[0] && b.0[0] <= a.1[0] && a.0[1] <= b.1[1] && b.0[1] <= a.1[1];
                if meet && test(i, j) {
                    out.push((i, j));
                }
            }
        }
        out
    }

    #[test]
    fn matches_all_pairs() {
        let mut rng = Lcg(7);
        for (n, m, size) in [
            (1, 1, 0.5),
            (300, 500, 0.3),
            (800, 40, 2.0),
            (400, 400, 0.0),
            (500, 500, 30.0),
            (3000, 3000, 0.4),
            (5000, 200, 5.0),
            (3000, 3000, 12.0),
        ] {
            let left = boxes(&mut rng, n, size);
            let right = boxes(&mut rng, m, size);
            let test = |i: usize, j: usize| !(i + j).is_multiple_of(3);
            let mut expected = brute(&left, &right, test);
            expected.sort_unstable();
            for threads in [1, 3, 8] {
                let mut got = join(&left, &right, threads, usize::MAX, &test).unwrap();
                got.sort_unstable();
                assert_eq!(got, expected, "{n} x {m}, size {size}, {threads} threads");
            }
        }
    }

    /// Long thin boxes (north-south and east-west lines) and points, with many open at once.
    #[test]
    fn lines_and_points() {
        let mut rng = Lcg(3);
        let mut left = Vec::new();
        for k in 0..30 {
            let x = f64::from(k) / 3.0;
            for s in 0..40 {
                let y = f64::from(s) / 4.0;
                left.push(([x, y], [x + 0.001, y + 0.25]));
                left.push(([y, x], [y + 0.25, x]));
            }
        }
        let right: Vec<Bbox> = (0..4000)
            .map(|_| {
                let (x, y) = (rng.next() * 10.0, rng.next() * 10.0);
                ([x - 0.05, y - 0.05], [x + 0.05, y + 0.05])
            })
            .collect();
        let test = |i: usize, j: usize| !(i * 7 + j).is_multiple_of(5);
        let mut expected = brute(&left, &right, test);
        expected.sort_unstable();
        for threads in [1, 4] {
            let mut got = join(&left, &right, threads, usize::MAX, &test).unwrap();
            got.sort_unstable();
            assert_eq!(got, expected, "{threads} threads");
            let mut got = join(&right, &left, threads, usize::MAX, &|i, j| test(j, i)).unwrap();
            got.sort_unstable();
            let mut swapped: Vec<_> = expected.iter().map(|&(i, j)| (j, i)).collect();
            swapped.sort_unstable();
            assert_eq!(got, swapped, "{threads} threads, swapped");
        }
    }

    /// Tall boxes (whole north-south lines, large polygons) stay out of the bands: the band
    /// work stays near one entry per short box however tall the others are.
    #[test]
    fn tall_boxes_skip_the_bands() {
        let mut rng = Lcg(5);
        let tall = |rng: &mut Lcg, n: usize| -> Vec<Bbox> {
            (0..n)
                .map(|_| {
                    let x = rng.next() * 100.0;
                    ([x, 0.0], [x + 0.01, 100.0 - rng.next()])
                })
                .collect()
        };
        let points = |rng: &mut Lcg, n: usize| -> Vec<Bbox> {
            (0..n)
                .map(|_| {
                    let (x, y) = (rng.next() * 100.0, rng.next() * 100.0);
                    ([x, y], [x, y])
                })
                .collect()
        };
        for (left, right) in [
            (tall(&mut rng, 3000), points(&mut rng, 3000)),
            (tall(&mut rng, 3000), tall(&mut rng, 3000)),
            (points(&mut rng, 3000), points(&mut rng, 3000)),
        ] {
            let test = |i: usize, j: usize| !(i + 2 * j).is_multiple_of(7);
            let mut expected = brute(&left, &right, test);
            expected.sort_unstable();
            BAND_WORK.with(|w| w.set(0));
            let mut got = join(&left, &right, 1, usize::MAX, &test).unwrap();
            let work = BAND_WORK.with(std::cell::Cell::get);
            got.sort_unstable();
            assert_eq!(got, expected);
            // Points: up to 4 band entries and visits each, and what they find there.
            assert!(work <= 16 * (left.len() + right.len()), "{work}");
        }
    }

    #[test]
    fn a_stopped_join_gives_up() {
        let mut rng = Lcg(7);
        let left = boxes(&mut rng, 20_000, 0.01);
        let right = boxes(&mut rng, 20_000, 0.01);
        let tests = AtomicUsize::new(0);
        let test = |_: usize, _: usize| {
            tests.fetch_add(1, Ordering::Relaxed);
            true
        };
        for threads in [1, 4] {
            assert_eq!(
                join_until(&left, &right, threads, usize::MAX, &test, &|| true),
                None
            );
        }
        assert_eq!(tests.load(Ordering::Relaxed), 0);
        // Stopped part way: no pairs come back, and the sweep ends early.
        let looks = AtomicUsize::new(0);
        let stop = || looks.fetch_add(1, Ordering::Relaxed) >= 6;
        assert_eq!(join_until(&left, &right, 1, usize::MAX, &test, &stop), None);
        let all = join(&left, &right, 1, usize::MAX, &|_, _| true)
            .unwrap()
            .len();
        assert!(
            tests.load(Ordering::Relaxed) < all,
            "{} of {all}",
            tests.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn self_join_and_limit() {
        let mut rng = Lcg(11);
        let all = boxes(&mut rng, 600, 1.0);
        let mut expected = brute(&all, &all, |_, _| true);
        expected.sort_unstable();
        let mut got = join(&all, &all, 4, usize::MAX, &|_, _| true).unwrap();
        got.sort_unstable();
        assert_eq!(got, expected);
        assert!(join(&all, &all, 4, expected.len(), &|_, _| true).is_some());
        assert!(join(&all, &all, 4, expected.len() / 2, &|_, _| true).is_none());
        assert_eq!(join(&all, &[], 4, 0, &|_, _| true), Some(Vec::new()));
    }
}
