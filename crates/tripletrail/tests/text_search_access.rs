//! Full-text search under access limits (#203): a caller who may read only some graphs
//! searches what it can see, ranked as on a store holding only that. Checked
//! differentially on seeded random data, before and after writes.

use tripletrail::oxigraph::model::{GraphName, NamedNode, Quad};
use tripletrail::oxigraph::sparql::QueryResults;
use tripletrail::{Engine, QueryOptions, ReadableGraphs};

const EX: &str = "http://example.org/";
const WORDS: [&str; 6] = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"];
const GRAPHS: usize = 3;

const QUERIES: [&str; 4] = [
    "SELECT ?s ?score ?l { ?s tt:search (\"alpha gamma\" ?score ?l) }",
    "SELECT ?s ?score ?l { ?s tt:search (\"del* zeta\" ?score ?l) }",
    "SELECT ?s ?score ?l { (?s ?score ?l) text:query (rdfs:label \"beta epsilon\") }",
    // A limit: the scores of the best three (which of tied ones depends on term ids).
    "SELECT ?score { ?s tt:search (\"alpha beta gamma\" ?score ?l 3) }",
];

struct Rng(u64);

impl Rng {
    fn next(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

fn ex(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{EX}{local}"))
}

fn graph(i: usize) -> GraphName {
    if i == GRAPHS {
        GraphName::DefaultGraph
    } else {
        ex(&format!("g{i}")).into()
    }
}

fn text(rng: &mut Rng) -> String {
    let n = 1 + rng.next(4);
    (0..n)
        .map(|_| WORDS[rng.next(WORDS.len())])
        .collect::<Vec<_>>()
        .join(" ")
}

/// Resources `from..to` in random graphs: a class, a flag, labels and sometimes a comment.
fn quads(rng: &mut Rng, from: usize, to: usize) -> Vec<Quad> {
    let label = NamedNode::new_unchecked("http://www.w3.org/2000/01/rdf-schema#label");
    let comment = NamedNode::new_unchecked("http://www.w3.org/2000/01/rdf-schema#comment");
    let a = NamedNode::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#type");
    let mut out = Vec::new();
    for i in from..to {
        let s = ex(&format!("s{i}"));
        let g = graph(rng.next(GRAPHS + 1));
        let class = if rng.next(2) == 0 { "A" } else { "B" };
        out.push(Quad::new(s.clone(), a.clone(), ex(class), g.clone()));
        let ok = tripletrail::oxigraph::model::Literal::from(rng.next(3) > 0);
        out.push(Quad::new(s.clone(), ex("ok"), ok, g.clone()));
        for _ in 0..1 + rng.next(2) {
            // Some labels in another graph than the resource's.
            let g = if rng.next(4) == 0 {
                graph(rng.next(GRAPHS + 1))
            } else {
                g.clone()
            };
            let l = tripletrail::oxigraph::model::Literal::new_simple_literal(text(rng));
            out.push(Quad::new(s.clone(), label.clone(), l, g));
        }
        if rng.next(2) == 0 {
            let c = tripletrail::oxigraph::model::Literal::new_simple_literal(text(rng));
            out.push(Quad::new(s.clone(), comment.clone(), c, g.clone()));
        }
    }
    out
}

fn engine(quads: Vec<Quad>) -> Engine {
    let e = Engine::new_tripletrail();
    e.load_quads(quads).unwrap();
    e
}

/// The answer, sorted, as text.
fn answer(e: &Engine, q: &str, options: QueryOptions) -> Vec<String> {
    let q = format!(
        "PREFIX tt: <http://w3id.org/tripletrail/fn#> PREFIX text: <http://jena.apache.org/text#> \
         PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#> {q}"
    );
    let QueryResults::Solutions(s) = e.query_with_options(&q, options).unwrap() else {
        panic!("{q} is no SELECT")
    };
    let mut rows: Vec<String> = s
        .map(|r| {
            r.unwrap()
                .iter()
                .map(|(v, t)| format!("{v}={t}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    rows.sort();
    rows
}

/// Every query under `options` on `full` answers as on `only`, without limits. Returns how
/// many rows they gave, so a test can tell it compared something.
fn same(full: &Engine, only: &Engine, options: &QueryOptions, case: &str) -> usize {
    let mut n = 0;
    for q in QUERIES {
        let restricted = answer(full, q, options.clone());
        let expected = answer(only, q, QueryOptions::default());
        assert_eq!(restricted, expected, "{case}: {q}");
        n += expected.len();
    }
    n
}

#[test]
fn a_caller_with_some_graphs_searches_them_as_a_store_of_those_graphs() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut compared = 0;
    for round in 0..12 {
        let mut data = quads(&mut rng, 0, 30);
        let full = engine(data.clone());
        let default_graph = rng.next(2) == 0;
        let named: Vec<NamedNode> = (0..GRAPHS)
            .filter(|_| rng.next(2) == 0)
            .map(|i| ex(&format!("g{i}")))
            .collect();
        let readable = ReadableGraphs {
            default_graph,
            named: named.clone(),
        };
        let keep = |q: &Quad| match &q.graph_name {
            GraphName::DefaultGraph => default_graph,
            GraphName::NamedNode(n) => named.contains(n),
            GraphName::BlankNode(_) => false,
        };
        let options = QueryOptions {
            readable: Some(readable),
            ..QueryOptions::default()
        };
        let only = engine(data.iter().filter(|q| keep(q)).cloned().collect());
        compared += same(&full, &only, &options, &format!("round {round}"));
        // After a write, the caller's index is brought up to date.
        let more = quads(&mut rng, 30, 36);
        for q in &more {
            full.insert(q).unwrap();
        }
        data.extend(more);
        let only = engine(data.iter().filter(|q| keep(q)).cloned().collect());
        compared += same(
            &full,
            &only,
            &options,
            &format!("round {round}, after a write"),
        );
    }
    assert!(compared > 100, "{compared}");
}

/// After a write, searches of one view at once wait for one patch of its index and share it:
/// no full build each (#203). They all see the write.
#[test]
fn concurrent_searches_of_a_stale_view_share_one_patch() {
    let label = NamedNode::new_unchecked("http://www.w3.org/2000/01/rdf-schema#label");
    let quads: Vec<Quad> = (0..20_000)
        .map(|i| {
            Quad::new(
                ex(&format!("s{i}")),
                label.clone(),
                tripletrail::oxigraph::model::Literal::new_simple_literal(format!(
                    "{} {i}",
                    WORDS[i % WORDS.len()]
                )),
                graph(i % 2),
            )
        })
        .collect();
    let full = engine(quads);
    let options = QueryOptions {
        readable: Some(ReadableGraphs {
            default_graph: false,
            named: vec![ex("g0")],
        }),
        ..QueryOptions::default()
    };
    let q = "SELECT ?s { ?s tt:search \"fresh\" }";
    assert!(answer(&full, q, options.clone()).is_empty());
    assert_eq!(full.text_view_work(), (1, 0));
    full.insert(&Quad::new(
        ex("new"),
        label,
        tripletrail::oxigraph::model::Literal::new_simple_literal("fresh"),
        graph(0),
    ))
    .unwrap();
    let start = std::sync::Barrier::new(8);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                start.wait();
                assert_eq!(
                    answer(&full, q, options.clone()),
                    ["?s=<http://example.org/new>"]
                );
            });
        }
    });
    assert_eq!(full.text_view_work(), (1, 1));
}
