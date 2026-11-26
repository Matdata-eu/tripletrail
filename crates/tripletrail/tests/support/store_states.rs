// Query answers across the states of an on-disk store (#154), shared by `tests/store_states.rs`
// and the `store_states` fuzz target (which `include!`s it, so no inner doc comments here).
//
// From a byte source: a base of quads loaded in bulk, then inserts and removes in a few
// updates. The same queries run on an on-disk tripletrail store in four states of that data,
// on the tripletrail store in memory as the reference, and on Oxigraph's store:
// - "delta": the updates kept in memory beside the checkpoint (and its write-ahead log);
// - "runs": a checkpoint that compacts them into runs beside the segments;
// - "merged": the runs merged into the segments;
// - "reopened": the store opened again from its files.
// Each state reads counts, fanouts, string ranks, the numeric values and the spatial index by
// another path, so every answer must equal the reference's.

use oxigraph::io::RdfFormat;
use oxigraph::model::{GraphName, Literal, NamedNode, Quad, Term};
use oxigraph::sparql::QueryResults;
use std::path::Path;
use tripletrail::Engine;
use tripletrail::store::Dataset;

pub struct Bytes<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Bytes<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    /// The next byte, 0 once the input is used up.
    pub fn next(&mut self) -> u8 {
        let b = self.data.get(self.at).copied().unwrap_or(0);
        self.at += 1;
        b
    }

    pub fn below(&mut self, n: u8) -> u8 {
        self.next() % n
    }

    fn done(&self) -> bool {
        self.at >= self.data.len()
    }
}

const EX: &str = "http://example.com/";
const GEO: &str = "http://www.opengis.net/ont/geosparql#";
const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
const WORDS: [&str; 8] = ["Com", "Comet", "C", "Cat", "cat", "Zürich", "", "Oslo"];

fn ex(local: impl std::fmt::Display) -> NamedNode {
    NamedNode::new_unchecked(format!("{EX}{local}"))
}

/// A quad of a small vocabulary, so that writes meet the base and each other.
fn quad(b: &mut Bytes<'_>) -> Quad {
    let s = ex(format_args!("s{}", b.below(24)));
    let (p, o): (NamedNode, Term) = match b.below(9) {
        0 => (
            NamedNode::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
            ex(format_args!("C{}", b.below(3))).into(),
        ),
        1 | 2 => (
            ex(format_args!("p{}", b.below(2))),
            ex(format_args!("s{}", b.below(24))).into(),
        ),
        3 => (ex("n"), Literal::from(i64::from(b.below(61)) - 30).into()),
        4 => (
            ex("d"),
            Literal::new_typed_literal(
                format!("{}.{}", b.below(20), 1 + b.below(9)),
                NamedNode::new_unchecked(format!("{XSD}decimal")),
            )
            .into(),
        ),
        5 => (
            ex("long"),
            Literal::new_typed_literal(
                (i64::from(b.below(200)) * 1000).to_string(),
                NamedNode::new_unchecked(format!("{XSD}long")),
            )
            .into(),
        ),
        6 => {
            let w = WORDS[usize::from(b.below(8))];
            let text = format!("{w} {}", b.below(5));
            let o = match b.below(3) {
                0 => Literal::new_language_tagged_literal_unchecked(text, "nb"),
                _ => Literal::new_simple_literal(text),
            };
            (ex("label"), o.into())
        }
        7 => (
            NamedNode::new_unchecked(format!("{GEO}asWKT")),
            Literal::new_typed_literal(
                format!("POINT({} {})", b.below(10), b.below(10)),
                NamedNode::new_unchecked(format!("{GEO}wktLiteral")),
            )
            .into(),
        ),
        _ => (ex("seen"), Literal::from(b.below(2) == 0).into()),
    };
    let g = match b.below(4) {
        0 => GraphName::NamedNode(ex(format_args!("g{}", b.below(2)))),
        _ => GraphName::DefaultGraph,
    };
    Quad::new(s, p, o, g)
}

/// The base, loaded in bulk, and the updates after it: each an insert or a remove of a quad.
pub struct Plan {
    pub base: Vec<Quad>,
    pub updates: Vec<Vec<(bool, Quad)>>,
}

impl Plan {
    pub fn new(data: &[u8]) -> Self {
        let mut b = Bytes::new(data);
        // The base from a generator the first bytes seed: more than eight times the updates,
        // so that a checkpoint compacts them into runs rather than rewriting the segments.
        let mut state = u64::from_le_bytes(std::array::from_fn(|_| b.next())) | 1;
        let generated: Vec<u8> = (0..16_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let mut g = Bytes::new(&generated);
        let base = (0..2000 + 10 * usize::from(b.below(100)))
            .map(|_| quad(&mut g))
            .collect();
        let mut updates = Vec::new();
        while !b.done() && updates.len() < 8 {
            let n = 1 + usize::from(b.below(12));
            updates.push((0..n).map(|_| (b.below(3) != 0, quad(&mut b))).collect());
        }
        Self { base, updates }
    }
}

fn update_text(writes: &[(bool, Quad)]) -> String {
    let data = |insert: bool| {
        let mut text = String::new();
        for (_, q) in writes.iter().filter(|(i, _)| *i == insert) {
            match &q.graph_name {
                GraphName::NamedNode(g) => {
                    text.push_str(&format!(
                        "GRAPH {g} {{ {} {} {} }} ",
                        q.subject, q.predicate, q.object
                    ));
                }
                _ => text.push_str(&format!("{} {} {} . ", q.subject, q.predicate, q.object)),
            }
        }
        text
    };
    // The removes first, then the inserts, as one update in two operations.
    format!(
        "DELETE DATA {{ {} }} ; INSERT DATA {{ {} }}",
        data(false),
        data(true)
    )
}

const PREFIXES: &str = "PREFIX ex: <http://example.com/>
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
";

/// Queries over every part of the store a state changes: counts with and without a pattern
/// (the saved statistics), joins and fanouts, string and numeric orders and ranges, the
/// spatial index, and named graphs.
pub const QUERIES: &[&str] = &[
    "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }",
    "SELECT (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } }",
    "SELECT ?p (COUNT(*) AS ?n) WHERE { ?s ?p ?o } GROUP BY ?p",
    "SELECT ?g ?p (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } } GROUP BY ?g ?p",
    "SELECT (COUNT(*) AS ?n) WHERE { ?s ex:p0 ?o }",
    "SELECT (COUNT(*) AS ?n) WHERE { ex:s3 ?p ?o }",
    "SELECT (COUNT(DISTINCT ?s) AS ?n) (COUNT(DISTINCT ?o) AS ?m) WHERE { ?s ex:p1 ?o }",
    "SELECT ?c (COUNT(?s) AS ?n) WHERE { ?s a ?c } GROUP BY ?c",
    "SELECT ?s ?o WHERE { ?s ex:p0 ?x . ?x ex:p1 ?o }",
    "SELECT ?s (COUNT(?o) AS ?n) WHERE { ?s ex:p0 ?o } GROUP BY ?s HAVING (COUNT(?o) > 1)",
    "SELECT ?s WHERE { ?s ex:p0 ?o FILTER NOT EXISTS { ?s ex:n ?v } }",
    "SELECT ?s ?v WHERE { ?s ex:n ?v FILTER(?v > -5 && ?v <= 12) }",
    "SELECT (SUM(?v) AS ?t) (MIN(?v) AS ?lo) (MAX(?v) AS ?hi) (COUNT(?v) AS ?n) WHERE { ?s ex:n ?v }",
    "SELECT ?v WHERE { ?s ex:d ?v } ORDER BY DESC(?v) LIMIT 4",
    "SELECT ?s ?v WHERE { ?s ex:d ?v FILTER(?v >= 5.5) }",
    "SELECT (MAX(?v) AS ?hi) (MIN(?v) AS ?lo) WHERE { ?s ex:long ?v }",
    "SELECT ?v WHERE { ?s ex:long ?v } ORDER BY DESC(?v) LIMIT 3",
    "SELECT ?s WHERE { ?s ex:long ?v FILTER(?v > 100000) }",
    "SELECT ?l WHERE { ?s ex:label ?l } ORDER BY ?l LIMIT 5",
    "SELECT (MIN(?l) AS ?lo) (MAX(?l) AS ?hi) WHERE { ?s ex:label ?l }",
    "SELECT ?s ?l WHERE { ?s ex:label ?l FILTER(STRSTARTS(?l, \"Com\")) }",
    "SELECT ?s WHERE { ?s ex:label ?l FILTER(LANG(?l) = \"nb\") }",
    "SELECT ?s WHERE { ?s geo:asWKT ?w FILTER(geof:sfWithin(?w, \"POLYGON((2 2, 6 2, 6 6, 2 6, 2 2))\"^^geo:wktLiteral)) }",
    "SELECT (COUNT(*) AS ?n) WHERE { ?s geo:asWKT ?w FILTER(geof:distance(?w, \"POINT(5 5)\"^^geo:wktLiteral, <http://www.opengis.net/def/uom/OGC/1.0/degree>) < 3) }",
    "SELECT ?s ?b WHERE { ?s ex:seen ?b FILTER(?b) }",
];

/// The solutions of `query`, each row's cells sorted, the rows sorted.
fn rows(engine: &Engine, query: &str) -> Vec<String> {
    let text = format!("{PREFIXES}{query}");
    let QueryResults::Solutions(solutions) = engine
        .query(&text)
        .unwrap_or_else(|e| panic!("{query}: {e}"))
    else {
        panic!("not a SELECT: {query}")
    };
    let mut out: Vec<String> = solutions
        .map(|s| {
            let s = s.unwrap();
            let mut cells: Vec<String> = s.iter().map(|(v, t)| format!("{v}={t}")).collect();
            cells.sort();
            cells.join(" ")
        })
        .collect();
    out.sort();
    out
}

fn store(engine: &Engine) -> &tripletrail_store::Store {
    match engine.store() {
        Dataset::TripleTrail(s) => s,
        _ => unreachable!("a tripletrail store"),
    }
}

fn compare(state: &str, ours: &Engine, answers: &[Vec<String>]) {
    for (query, expected) in QUERIES.iter().zip(answers) {
        assert_eq!(&rows(ours, query), expected, "{state}: {query}");
    }
}

/// Runs `plan` on Oxigraph's store, on the tripletrail store in memory and on one in `dir`
/// (empty), and compares the answers of every state. Panics on a difference; returns whether
/// the updates were compacted into runs.
pub fn check(dir: &Path, plan: &Plan) -> bool {
    // Compactions write runs even on a store this small, as on a large one.
    tripletrail_store::Store::set_runs_from(0);
    tripletrail_store::Store::set_idle_merge_after(std::time::Duration::from_millis(1));
    let nquads: String = plan.base.iter().map(|q| format!("{q} .\n")).collect();
    let oxigraph = Engine::new_oxigraph().unwrap();
    let reference = Engine::new_tripletrail();
    for engine in [&oxigraph, &reference] {
        engine.load(RdfFormat::NQuads, nquads.as_bytes()).unwrap();
    }
    let ours = Engine::open_tripletrail(dir).unwrap();
    // The writes stay in memory until the checkpoints below.
    store(&ours).set_checkpoint_log_size(u64::MAX);
    store(&ours).set_background_merges(false);
    ours.load(RdfFormat::NQuads, nquads.as_bytes()).unwrap();
    for writes in &plan.updates {
        let update = format!("{PREFIXES}{}", update_text(writes));
        for engine in [&oxigraph, &reference, &ours] {
            engine.update(&update).unwrap();
        }
    }
    let answers: Vec<Vec<String>> = QUERIES.iter().map(|q| rows(&reference, q)).collect();
    // Oxigraph's store keeps an `xsd:long` as an `xsd:integer`; the tripletrail store keeps the
    // term as written, as Jena does.
    for (query, expected) in QUERIES.iter().zip(&answers) {
        if !query.contains("ex:long") {
            assert_eq!(&rows(&oxigraph, query), expected, "Oxigraph: {query}");
        }
    }
    compare("delta", &ours, &answers);

    store(&ours).checkpoint().unwrap();
    // None when the updates changed nothing (or undid each other).
    let runs = store(&ours).run_count() > 0;
    compare("runs", &ours, &answers);

    // Runs this small next to the segments merge once the store is idle.
    store(&ours).set_background_merges(true);
    let start = std::time::Instant::now();
    while store(&ours).run_count() > 0 {
        assert!(
            start.elapsed().as_secs() < 30,
            "runs merged into the segments"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
        store(&ours).wait_for_merges();
    }
    store(&ours).checkpoint().unwrap();
    compare("merged", &ours, &answers);

    drop(ours);
    let ours = Engine::open_tripletrail(dir).unwrap();
    compare("reopened", &ours, &answers);
    runs
}
