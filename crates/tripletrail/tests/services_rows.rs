//! The built-in services under row rules (#202): their sub-selects, path search and routing
//! read only the rows the calling query sees, and answer as on a store holding only those.

use oxigraph::io::RdfFormat;
use oxigraph::model::{NamedNode, NamedOrBlankNode, Quad, Term};
use oxigraph::sparql::QueryResults;
use std::sync::Arc;
use tripletrail::{Engine, QueryOptions};

const EX: &str = "urn:x:";

/// Nodes `n0` to `n11` in a ring of `ex:link`s, each with a number and a point; two
/// extra links make shortcuts through `n3`.
fn data() -> String {
    let mut ttl = String::from(
        "@prefix ex: <urn:x:> .\n@prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..12 {
        ttl.push_str(&format!(
            "ex:n{i} ex:link ex:n{} ; ex:n {} ; geo:asWKT \"LINESTRING({i} 0, {i} 1)\"^^geo:wktLiteral .\n",
            (i + 1) % 12,
            (i * 7) % 12,
        ));
    }
    ttl.push_str("ex:n0 ex:link ex:n3 . ex:n3 ex:link ex:n6 .\n");
    ttl
}

const HIDDEN: [&str; 2] = ["n3", "n8"];

fn hidden(e: &Engine) -> Arc<tripletrail::RowFilter> {
    let resources: Vec<NamedOrBlankNode> = HIDDEN
        .iter()
        .map(|n| NamedNode::new_unchecked(format!("{EX}{n}")).into())
        .collect();
    Arc::new(e.row_filter(&resources, true, &[], &[]).unwrap())
}

/// The store without the hidden resources and what points to them.
fn visible_only() -> Engine {
    let all = Engine::new_tripletrail();
    all.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    let is_hidden = |t: &Term| HIDDEN.iter().any(|n| t.to_string() == format!("<{EX}{n}>"));
    let quads: Vec<Quad> = all
        .store()
        .quads_for_pattern(None, None, None, None)
        .map(Result::unwrap)
        .filter(|q| !is_hidden(&q.subject.clone().into()) && !is_hidden(&q.object))
        .collect();
    let e = Engine::new_tripletrail();
    e.load_quads(quads).unwrap();
    e
}

const QUERIES: &[&str] = &[
    "SELECT ?g ?part { SERVICE <http://w3id.org/tripletrail/st> { \
     [] <http://w3id.org/tripletrail/st#function> <http://w3id.org/tripletrail/st#ST_DumpPoints> ; \
        <http://w3id.org/tripletrail/st#geom> ?part . \
     { SELECT ?g ?geom { ?g geo:asWKT ?geom } } } }",
    "SELECT ?s ?n ?r { SERVICE <http://w3id.org/tripletrail/window> { \
     [] <http://w3id.org/tripletrail/window#orderBy> ?n ; <http://w3id.org/tripletrail/window#rowNumber> ?r . \
     { SELECT ?s ?n { ?s ex:n ?n } } } }",
    "SELECT ?e { SERVICE <http://w3id.org/tripletrail/path> { \
     [] <http://w3id.org/tripletrail/path#source> ex:n0 ; <http://w3id.org/tripletrail/path#end> ?e ; \
        <http://w3id.org/tripletrail/path#direction> \"both\" ; <http://w3id.org/tripletrail/path#maxLength> 4 } }",
    "SELECT ?e { SERVICE <http://w3id.org/tripletrail/path> { \
     [] <http://w3id.org/tripletrail/path#source> ex:n0 ; <http://w3id.org/tripletrail/path#end> ?e ; \
        <http://w3id.org/tripletrail/path#predicate> ex:link ; <http://w3id.org/tripletrail/path#maxLength> 5 } }",
    "SELECT ?v ?c { SERVICE <http://w3id.org/tripletrail/pgr> { \
     [] <http://w3id.org/tripletrail/pgr#function> <http://w3id.org/tripletrail/pgr#pgr_connectedComponents> ; \
        <http://w3id.org/tripletrail/pgr#node> ?v ; <http://w3id.org/tripletrail/pgr#component> ?c . \
     { SELECT ?id ?source ?target ?cost { ?source ex:link ?target \
       BIND(IRI(CONCAT(STR(?source), \"-\", STR(?target))) AS ?id) BIND(1 AS ?cost) } } } }",
    "SELECT ?node ?cost { SERVICE <http://w3id.org/tripletrail/pgr> { \
     [] <http://w3id.org/tripletrail/pgr#function> <http://w3id.org/tripletrail/pgr#pgr_dijkstra> ; \
        <http://w3id.org/tripletrail/pgr#start_vid> ex:n0 ; <http://w3id.org/tripletrail/pgr#end_vid> ex:n6 ; \
        <http://w3id.org/tripletrail/pgr#node> ?node ; <http://w3id.org/tripletrail/pgr#agg_cost> ?cost . \
     { SELECT ?id ?source ?target ?cost { ?source ex:link ?target \
       BIND(IRI(CONCAT(STR(?source), \"-\", STR(?target))) AS ?id) BIND(1 AS ?cost) } } } }",
];

fn rows(e: &Engine, query: &str, options: QueryOptions) -> Result<Vec<String>, String> {
    let query =
        format!("PREFIX ex: <{EX}> PREFIX geo: <http://www.opengis.net/ont/geosparql#> {query}");
    let QueryResults::Solutions(s) = e
        .query_with_options(&query, options)
        .map_err(|e| e.to_string())?
    else {
        unreachable!("SELECT")
    };
    let mut out = Vec::new();
    for row in s {
        let row = row.map_err(|e| e.to_string())?;
        let mut cells: Vec<String> = row.iter().map(|(v, t)| format!("{v}={t}")).collect();
        cells.sort();
        out.push(cells.join(" "));
    }
    out.sort();
    Ok(out)
}

#[test]
fn services_read_only_the_rows_the_caller_sees() {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    let rules = hidden(&e);
    let visible = visible_only();
    for union in [false, true] {
        e.set_union_default_graph(union);
        visible.set_union_default_graph(union);
        for q in QUERIES {
            let options = QueryOptions {
                rows: Some(Arc::clone(&rules)),
                ..Default::default()
            };
            let got = rows(&e, q, options).unwrap_or_else(|err| panic!("{q}: {err}"));
            let want = rows(&visible, q, QueryOptions::default()).unwrap();
            assert_eq!(got, want, "union {union}: {q}");
            assert!(
                got.iter()
                    .all(|r| HIDDEN.iter().all(|n| !r.contains(&format!("{EX}{n}>")))),
                "{q}: {got:#?}"
            );
            // Without the rules, the hidden nodes are there.
            let all = rows(&e, q, QueryOptions::default()).unwrap();
            assert_ne!(all, got, "{q}: the rules hid nothing");
        }
    }
}

/// A service other than the engine's own, and one of them inside another, stay refused.
#[test]
fn other_services_stay_refused_under_row_rules() {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    let options = || QueryOptions {
        rows: Some(hidden(&e)),
        ..Default::default()
    };
    assert!(
        rows(
            &e,
            "SELECT * { SERVICE <urn:x:elsewhere> { ?s ?p ?o } }",
            options()
        )
        .is_err()
    );
    let nested = "SELECT ?s ?r { SERVICE <http://w3id.org/tripletrail/window> { \
     [] <http://w3id.org/tripletrail/window#orderBy> ?s ; <http://w3id.org/tripletrail/window#rowNumber> ?r . \
     { SELECT ?s { SERVICE <urn:x:elsewhere> { ?s ?p ?o } } } } }";
    let err = rows(&e, nested, options()).unwrap_err();
    assert!(err.contains("under row-level access rules"), "{err}");
}
