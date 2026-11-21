//! The count and id-row shortcuts on the view of a caller limited to some graphs or rows
//! (#204): they answer such queries, and answer as the evaluator does without them.

use oxigraph::model::{GraphName, NamedNode, Quad, Term};
use oxigraph::sparql::QueryResults;
use std::sync::Arc;
use tripletrail::{Engine, QueryOptions, ReadableGraphs};

const EX: &str = "http://example.org/";

fn ex(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{EX}{local}"))
}

/// Vendors and products in the default graph, offers spread over four named graphs, and
/// some offers in two graphs (copies the merge of the graphs reads once).
fn engine() -> Engine {
    let mut quads = Vec::new();
    let a = NamedNode::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#type");
    let mut push = |s: NamedNode, p: NamedNode, o: Term, g: GraphName| {
        quads.push(Quad::new(s, p, o, g));
    };
    for v in 0..20 {
        push(
            ex(&format!("vendor{v}")),
            a.clone(),
            ex("Vendor").into(),
            GraphName::DefaultGraph,
        );
        let country = ["BE", "NL", "NO", "FR"][v % 4];
        push(
            ex(&format!("vendor{v}")),
            ex("country"),
            oxigraph::model::Literal::new_simple_literal(country).into(),
            GraphName::DefaultGraph,
        );
    }
    for o in 0..2_000usize {
        let graphs: Vec<usize> = if o % 50 == 0 {
            vec![1 + o % 4, 1 + (o + 1) % 4]
        } else {
            vec![1 + o % 4]
        };
        for g in graphs {
            let g: GraphName = ex(&format!("graph{g}")).into();
            let s = ex(&format!("offer{o}"));
            push(s.clone(), a.clone(), ex("Offer").into(), g.clone());
            push(
                s.clone(),
                ex("vendor"),
                ex(&format!("vendor{}", (o * 7) % 20)).into(),
                g.clone(),
            );
            push(
                s,
                ex("price"),
                oxigraph::model::Literal::from(((o * 37) % 500) as i64).into(),
                g,
            );
        }
    }
    let e = Engine::new_tripletrail();
    e.load_quads(quads).unwrap();
    e
}

const QUERIES: &[&str] = &[
    "SELECT (COUNT(*) AS ?n) WHERE { ?o a ex:Offer }",
    "SELECT (COUNT(?o) AS ?n) WHERE { ?o a ex:Offer ; ex:price ?p FILTER(?p >= 100 && ?p < 120) }",
    "SELECT ?c (COUNT(?o) AS ?n) WHERE { ?o a ex:Offer ; ex:vendor ?v . ?v ex:country ?c } GROUP BY ?c",
    "SELECT (COUNT(DISTINCT ?v) AS ?n) WHERE { ?o ex:vendor ?v }",
    "SELECT ?v (COUNT(*) AS ?n) WHERE { ?o ex:vendor ?v } GROUP BY ?v",
    "SELECT ?o WHERE { VALUES ?p { 37 74 111 148 } ?o ex:price ?p }",
    "SELECT ?o WHERE { ?o a ex:Offer MINUS { ?o ex:price ?p FILTER(?p < 490) } }",
    "SELECT (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?o a ex:Offer } }",
];

fn rows(e: &Engine, query: &str, options: QueryOptions) -> Vec<String> {
    let query = format!("PREFIX ex: <{EX}>\n{query}");
    let QueryResults::Solutions(s) = e.query_with_options(&query, options).unwrap() else {
        unreachable!("SELECT")
    };
    let mut out: Vec<String> = s
        .map(|row| {
            let row = row.unwrap();
            let mut cells: Vec<String> = row.iter().map(|(v, t)| format!("{v}={t}")).collect();
            cells.sort();
            cells.join(" ")
        })
        .collect();
    out.sort();
    out
}

/// Answers each query under `options` with the shortcuts and without (`tt.id-rewrites off`),
/// asserts they agree, and returns how many parts the shortcuts answered.
fn agree(e: &Engine, options: &QueryOptions, label: &str) -> u64 {
    let before = e.view_shortcuts_taken();
    for q in QUERIES {
        let got = rows(e, q, options.clone());
        let want = rows(
            e,
            &format!("#pragma tt.id-rewrites off\n{q}"),
            options.clone(),
        );
        assert_eq!(got, want, "{label}: {q}");
    }
    e.view_shortcuts_taken() - before
}

fn readable(default_graph: bool, named: &[usize]) -> ReadableGraphs {
    ReadableGraphs {
        default_graph,
        named: named.iter().map(|g| ex(&format!("graph{g}"))).collect(),
    }
}

fn hiding(e: &Engine) -> Arc<tripletrail::RowFilter> {
    // The vendors of two countries, and what points to them: their offers keep their other
    // triples (a hidden reference), so counts of offers and of vendors differ.
    let hidden: Vec<_> = (0..20)
        .filter(|v| v % 4 < 2)
        .map(|v| ex(&format!("vendor{v}")).into())
        .collect();
    Arc::new(e.row_filter(&hidden, true, &[], &[]).unwrap())
}

#[test]
fn graph_grants_take_the_shortcuts_and_answer_as_without() {
    let e = engine();
    for union in [false, true] {
        e.set_union_default_graph(union);
        for (default_graph, named) in [
            (true, vec![1, 3]),
            (true, vec![1, 2, 3, 4]),
            (false, vec![2, 3]),
            (true, vec![2]),
        ] {
            let options = QueryOptions {
                readable: Some(readable(default_graph, &named)),
                ..Default::default()
            };
            let label = format!("union {union}, default {default_graph}, named {named:?}");
            let taken = agree(&e, &options, &label);
            assert!(taken > 0, "{label}: no shortcut taken");
        }
    }
}

#[test]
fn row_filters_take_the_shortcuts_and_answer_as_without() {
    let e = engine();
    let rows = hiding(&e);
    for union in [false, true] {
        e.set_union_default_graph(union);
        let options = QueryOptions {
            rows: Some(Arc::clone(&rows)),
            ..Default::default()
        };
        let label = format!("union {union}, rows");
        // Without the union, the default graph holds the vendors only.
        if union {
            assert!(
                agree(&e, &options, &label) > 0,
                "{label}: no shortcut taken"
            );
        }
        let both = QueryOptions {
            rows: Some(Arc::clone(&rows)),
            readable: Some(readable(true, &[1, 3, 4])),
            ..Default::default()
        };
        let label = format!("union {union}, rows and a grant");
        assert!(agree(&e, &both, &label) > 0, "{label}: no shortcut taken");
    }
}

/// The copies of an offer in two granted graphs count once, as the merge reads them, and a
/// copy in a graph not granted does not count.
#[test]
fn copies_in_several_granted_graphs_count_once() {
    let e = engine();
    e.set_union_default_graph(true);
    let count = |named: &[usize]| {
        rows(
            &e,
            "SELECT (COUNT(*) AS ?n) WHERE { ?o a ex:Offer }",
            QueryOptions {
                readable: Some(readable(true, named)),
                ..Default::default()
            },
        )
    };
    let n = |v: usize| {
        vec![format!(
            "?n=\"{v}\"^^<http://www.w3.org/2001/XMLSchema#integer>"
        )]
    };
    // Graph g holds the offers o with o % 4 == g - 1, and copies of the offers o % 50 == 0
    // with (o + 1) % 4 == g - 1: graph 2 the 20 offers 0, 100, ... of graph 1.
    assert_eq!(count(&[1, 2, 3, 4]), n(2_000));
    assert_eq!(count(&[1]), n(500));
    assert_eq!(count(&[2]), n(520));
    assert_eq!(count(&[1, 2]), n(1_000));
}
