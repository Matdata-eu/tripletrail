//! Routing on graphs built from the input: contraction hierarchies (static and
//! customizable) find the same shortest-path costs as Dijkstra. Costs are small integers, so
//! the sums are exact whatever order they are added in. The rest of the input, read as a
//! saved routing graph, must give a graph or `None`, never a panic.
#![no_main]

use libfuzzer_sys::fuzz_target;
use tripletrail_routing::{
    Cch, CchSpace, ChSpace, ContractionHierarchy, EdgeRow, Graph, InternedGraph, Path, SearchSpace,
    cch_dijkstra, ch_dijkstra, dijkstra,
};

fuzz_target!(|data: &[u8]| {
    let Some((&header, rest)) = data.split_first() else {
        return;
    };
    if header & 0x80 != 0 {
        let _ = InternedGraph::<String>::read(rest, |s| Some(s.to_owned()));
        return;
    }
    let n = 2 + u32::from(header % 24);
    let directed = header & 0x40 != 0;
    let rows: Vec<EdgeRow> = rest
        .chunks_exact(4)
        .take(96)
        .enumerate()
        .map(|(i, c)| EdgeRow {
            id: i as u32,
            source: u32::from(c[0]) % n,
            target: u32::from(c[1]) % n,
            cost: if c[2] >= 240 {
                -1.0
            } else {
                f64::from(c[2] % 20)
            },
            reverse_cost: match c[3] % 4 {
                0 => None,
                1 => Some(-1.0),
                v => Some(f64::from(v * 3 + c[3] / 16)),
            },
        })
        .collect();
    let g = Graph::build(n as usize, &rows, directed);
    let ch = ContractionHierarchy::build(&g);
    let mut cch = Cch::build(&g);
    assert!(cch.customize(&g));
    let (mut space, mut ch_space, mut cch_space) =
        (SearchSpace::new(), ChSpace::new(), CchSpace::new());
    let targets: Vec<u32> = (0..n).collect();
    for s in 0..n {
        let expected: Vec<Option<f64>> = dijkstra(&g, &mut space, s, &targets)
            .iter()
            .map(|p| p.as_ref().map(Path::total_cost))
            .collect();
        let by_ch: Vec<Option<f64>> = ch_dijkstra(&ch, &mut ch_space, s, &targets)
            .iter()
            .map(|p| p.as_ref().map(Path::total_cost))
            .collect();
        let by_cch: Vec<Option<f64>> = cch_dijkstra(&cch, &mut cch_space, s, &targets)
            .iter()
            .map(|p| p.as_ref().map(Path::total_cost))
            .collect();
        assert_eq!(expected, by_ch, "CH from {s}: {rows:?}");
        assert_eq!(expected, by_cch, "CCH from {s}: {rows:?}");
    }
});
