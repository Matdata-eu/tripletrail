//! Travelling salesperson tours (pgr_TSP, pgr_TSPeuclidean): nearest neighbour, then 2-opt
//! and or-opt moves until no move shortens the tour.

/// A symmetric cost matrix over `n` points, row-major. Missing pairs are `f64::INFINITY`.
#[derive(Debug, Clone)]
pub struct CostMatrix {
    n: usize,
    cost: Vec<f64>,
}

impl CostMatrix {
    /// A matrix from directed `(from, to, cost)` entries; when both directions are given the
    /// cheaper one is used for both, as the tour is undirected.
    pub fn new(n: usize, entries: impl IntoIterator<Item = (u32, u32, f64)>) -> Self {
        let mut cost = vec![f64::INFINITY; n * n];
        for i in 0..n {
            cost[i * n + i] = 0.0;
        }
        for (a, b, c) in entries {
            let (a, b) = (a as usize, b as usize);
            if a != b && c < cost[a * n + b] {
                cost[a * n + b] = c;
                cost[b * n + a] = c;
            }
        }
        Self { n, cost }
    }

    /// Euclidean distances between points.
    pub fn euclidean(points: &[(f64, f64)]) -> Self {
        let n = points.len();
        let mut cost = vec![0.0; n * n];
        for (i, a) in points.iter().enumerate() {
            for (j, b) in points.iter().enumerate() {
                cost[i * n + j] = (a.0 - b.0).hypot(a.1 - b.1);
            }
        }
        Self { n, cost }
    }

    #[inline]
    pub fn get(&self, a: u32, b: u32) -> f64 {
        self.cost[a as usize * self.n + b as usize]
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }
}

/// A closed tour through every point, starting and ending at `start`. With `end`, `end` is the
/// last point visited before returning to `start`. `None` if every tour needs a missing pair.
pub fn tsp(matrix: &CostMatrix, start: u32, end: Option<u32>) -> Option<Vec<u32>> {
    let n = matrix.len();
    if n == 0 {
        return None;
    }
    let end = end.filter(|&e| e != start);
    // Nearest neighbour, keeping `end` for last.
    let mut tour = Vec::with_capacity(n + 1);
    let mut visited = vec![false; n];
    visited[start as usize] = true;
    tour.push(start);
    if let Some(e) = end {
        visited[e as usize] = true;
    }
    let mut at = start;
    while let Some(next) = (0..n as u32)
        .filter(|&v| !visited[v as usize])
        .min_by(|&a, &b| matrix.get(at, a).total_cmp(&matrix.get(at, b)))
    {
        visited[next as usize] = true;
        tour.push(next);
        at = next;
    }
    if let Some(e) = end {
        tour.push(e);
    }
    tour.push(start);
    // Positions 1..=last may move; with `end` it stays at tour[n - 1].
    let last = if end.is_some() { n - 2 } else { n - 1 };
    // Missing pairs cost a huge but finite amount, so that moves can remove them.
    let d = |a: u32, b: u32| {
        let c = matrix.get(a, b);
        if c.is_finite() { c } else { 1e300 }
    };
    let mut improved = true;
    let mut rounds = 0;
    while improved && rounds < 1000 {
        if crate::cancel::stopped_now() {
            break;
        }
        improved = false;
        rounds += 1;
        // 2-opt: reverse tour[i..=j].
        for i in 1..last {
            for j in i + 1..=last {
                let (a, b, c, e) = (tour[i - 1], tour[i], tour[j], tour[j + 1]);
                let delta = d(a, c) + d(b, e) - d(a, b) - d(c, e);
                if delta < -1e-9 {
                    tour[i..=j].reverse();
                    improved = true;
                }
            }
        }
        // Or-opt: move a run of 1 to 3 points elsewhere.
        for len in 1..=3usize {
            let mut i = 1;
            while i + len - 1 <= last {
                let j = i + len - 1;
                let (p, f, l, q) = (tour[i - 1], tour[i], tour[j], tour[j + 1]);
                let removed = d(p, f) + d(l, q) - d(p, q);
                let mut best = (0.0, usize::MAX, false);
                for k in 0..=last {
                    // Insert between tour[k] and tour[k + 1], outside the run.
                    if k + 1 >= i && k <= j {
                        continue;
                    }
                    let (x, y) = (tour[k], tour[k + 1]);
                    for reversed in [false, true] {
                        let (first, second) = if reversed { (l, f) } else { (f, l) };
                        let gain = removed - (d(x, first) + d(second, y) - d(x, y));
                        if gain > best.0 + 1e-9 {
                            best = (gain, k, reversed);
                        }
                    }
                }
                if best.1 != usize::MAX {
                    let mut run: Vec<u32> = tour.drain(i..=j).collect();
                    if best.2 {
                        run.reverse();
                    }
                    let at = if best.1 < i {
                        best.1 + 1
                    } else {
                        best.1 + 1 - len
                    };
                    tour.splice(at..at, run);
                    improved = true;
                }
                i += 1;
            }
        }
    }
    tour.windows(2)
        .all(|w| matrix.get(w[0], w[1]).is_finite())
        .then_some(tour)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn length(m: &CostMatrix, tour: &[u32]) -> f64 {
        tour.windows(2).map(|w| m.get(w[0], w[1])).sum()
    }

    #[test]
    fn square_and_circle() {
        // The corners of a square, given crossed: the tour runs around the edge.
        let m = CostMatrix::euclidean(&[(0.0, 0.0), (1.0, 1.0), (1.0, 0.0), (0.0, 1.0)]);
        let tour = tsp(&m, 0, None).unwrap();
        assert_eq!(tour.first(), Some(&0));
        assert_eq!(tour.last(), Some(&0));
        assert_eq!(length(&m, &tour), 4.0);
        // Points on a circle, shuffled: the optimum is the circle's order.
        let n = 12;
        let points: Vec<(f64, f64)> = (0..n)
            .map(|i| {
                let a = ((i * 5) % n) as f64 / n as f64 * std::f64::consts::TAU;
                (a.cos(), a.sin())
            })
            .collect();
        let m = CostMatrix::euclidean(&points);
        let tour = tsp(&m, 3, Some(7)).unwrap();
        assert_eq!(tour.len(), n + 1);
        assert_eq!(tour[n - 1], 7);
        let tour = tsp(&m, 3, None).unwrap();
        let optimum = n as f64 * 2.0 * (std::f64::consts::PI / n as f64).sin();
        assert!((length(&m, &tour) - optimum).abs() < 1e-9);
    }

    #[test]
    fn missing_pairs() {
        let m = CostMatrix::new(3, [(0, 1, 1.0), (1, 2, 1.0)]);
        assert!(tsp(&m, 0, None).is_none());
        let m = CostMatrix::new(3, [(0, 1, 1.0), (1, 2, 1.0), (2, 0, 5.0), (0, 2, 3.0)]);
        assert_eq!(tsp(&m, 0, None).map(|t| length(&m, &t)), Some(5.0));
    }
}
