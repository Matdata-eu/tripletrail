//! Compares Dijkstra, a contraction hierarchy and a customizable one on a grid of chains
//! (crossings joined by runs of `k` vertices, like a railway micro topology).
//!
//! Usage: `cargo run --release -p tripletrail-routing --example cch_bench -- [grid] [k] [queries]`

use std::time::Instant;
use tripletrail_routing::*;

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

fn main() {
    let a: Vec<usize> = std::env::args()
        .skip(1)
        .map(|x| x.parse().unwrap())
        .collect();
    let (grid, k, queries) = (
        *a.first().unwrap_or(&60),
        *a.get(1).unwrap_or(&30),
        *a.get(2).unwrap_or(&1000),
    );
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let crossing = |i: usize, j: usize| (i * grid + j) as u32;
    let mut n = (grid * grid) as u32;
    let mut rows = Vec::new();
    let mut chain = |from: u32, to: u32, rows: &mut Vec<EdgeRow>, rng: &mut Rng| {
        let mut prev = from;
        for step in 0..=k {
            let next = if step == k {
                to
            } else {
                n += 1;
                n - 1
            };
            rows.push(EdgeRow {
                id: rows.len() as u32,
                source: prev,
                target: next,
                cost: 1.0 + rng.below(100) as f64,
                reverse_cost: Some(1.0 + rng.below(100) as f64),
            });
            prev = next;
        }
    };
    for i in 0..grid {
        for j in 0..grid {
            if j + 1 < grid {
                chain(crossing(i, j), crossing(i, j + 1), &mut rows, &mut rng);
            }
            if i + 1 < grid {
                chain(crossing(i, j), crossing(i + 1, j), &mut rows, &mut rng);
            }
        }
    }
    let g = Graph::build(n as usize, &rows, true);
    println!("{} vertices, {} arcs", g.num_vertices(), g.num_arcs());
    let t = Instant::now();
    let ch = ContractionHierarchy::build(&g);
    println!(
        "CH build: {:.2} s, {} shortcuts",
        t.elapsed().as_secs_f64(),
        ch.num_shortcuts()
    );
    let t = Instant::now();
    let mut cch = Cch::build(&g);
    println!(
        "CCH build: {:.2} s, {} arcs",
        t.elapsed().as_secs_f64(),
        cch.num_arcs()
    );
    for r in &mut rows {
        r.cost = 1.0 + rng.below(100) as f64;
    }
    let g2 = Graph::build(n as usize, &rows, true);
    let t = Instant::now();
    assert!(cch.customize(&g2));
    println!("CCH customize: {:.3} s", t.elapsed().as_secs_f64());
    let t = Instant::now();
    let ch2 = ContractionHierarchy::build(&g2);
    println!(
        "CH rebuild for new costs: {:.2} s",
        t.elapsed().as_secs_f64()
    );
    let pairs: Vec<(u32, u32)> = (0..queries)
        .map(|_| (rng.below(n as u64) as u32, rng.below(n as u64) as u32))
        .collect();
    let mut sp = SearchSpace::new();
    let t = Instant::now();
    let d: Vec<f64> = pairs
        .iter()
        .map(|&(s, e)| {
            dijkstra(&g2, &mut sp, s, &[e])[0]
                .as_ref()
                .map_or(-1.0, Path::total_cost)
        })
        .collect();
    println!(
        "Dijkstra: {:.3} ms/query",
        t.elapsed().as_secs_f64() * 1e3 / queries as f64
    );
    let mut cs = ChSpace::new();
    let t = Instant::now();
    let c: Vec<f64> = pairs
        .iter()
        .map(|&(s, e)| {
            ch_dijkstra(&ch2, &mut cs, s, &[e])[0]
                .as_ref()
                .map_or(-1.0, Path::total_cost)
        })
        .collect();
    println!(
        "CH: {:.3} ms/query",
        t.elapsed().as_secs_f64() * 1e3 / queries as f64
    );
    let mut ccs = CchSpace::new();
    let t = Instant::now();
    let cc: Vec<f64> = pairs
        .iter()
        .map(|&(s, e)| {
            cch_dijkstra(&cch, &mut ccs, s, &[e])[0]
                .as_ref()
                .map_or(-1.0, Path::total_cost)
        })
        .collect();
    println!(
        "CCH: {:.3} ms/query",
        t.elapsed().as_secs_f64() * 1e3 / queries as f64
    );
    assert_eq!(d, c);
    assert_eq!(d, cc);
}
