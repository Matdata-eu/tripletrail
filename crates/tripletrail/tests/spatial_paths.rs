//! Spatial filters over geometries reached by property paths (#48): the parser joins
//! `?x (a|b/c)/geo:asWKT ?p` as a path and a triple through a blank node, and the spatial
//! rewrite must keep the two together.

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;

const DATA: &str = r#"
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
@prefix ex: <https://example.org/> .
ex:region geo:hasGeometry ex:rg . ex:rg geo:asWKT "POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))"^^geo:wktLiteral .
ex:a a ex:P ; geo:hasGeometry ex:ga . ex:ga geo:asWKT "POINT(1 1)"^^geo:wktLiteral .
ex:b a ex:P ; ex:ref ex:rb . ex:rb geo:hasGeometry ex:gb . ex:gb geo:asWKT "POINT(2 2)"^^geo:wktLiteral .
ex:c a ex:P ; ex:ref ex:rc . ex:rc geo:hasGeometry ex:gc . ex:gc geo:asWKT "POINT(20 20)"^^geo:wktLiteral .
ex:d a ex:P ; ex:part ex:d1 . ex:d1 ex:part ex:d2 . ex:d2 geo:hasGeometry ex:gd . ex:gd geo:asWKT "POINT(3 3)"^^geo:wktLiteral .
ex:e a ex:P ; ex:part ex:e1 . ex:e1 geo:hasGeometry ex:ge . ex:ge geo:asWKT "POINT(30 3)"^^geo:wktLiteral .
"#;

fn rows(e: &Engine, q: &str) -> Vec<String> {
    let QueryResults::Solutions(s) = e.query(q).unwrap() else {
        panic!()
    };
    let mut out: Vec<String> = s
        .map(|r| {
            r.unwrap()
                .iter()
                .map(|(v, t)| format!("{v}={t}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    out.sort();
    out
}

#[test]
fn spatial_filters_keep_paths_joined_through_blank_nodes() {
    let tt = Engine::new_tripletrail();
    tt.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    let ox = Engine::new_oxigraph().unwrap();
    ox.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    let prefixes = "PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
        PREFIX geo: <http://www.opengis.net/ont/geosparql#>
        PREFIX ex: <https://example.org/>
        PREFIX uom: <http://www.opengis.net/def/uom/OGC/1.0/>";
    let paths = [
        "(geo:hasGeometry|ex:ref/geo:hasGeometry)/geo:asWKT",
        "ex:ref?/geo:hasGeometry/geo:asWKT",
        "ex:part*/geo:hasGeometry/geo:asWKT",
        "ex:part+/geo:hasGeometry/geo:asWKT",
        "(ex:ref|ex:part)/geo:hasGeometry/geo:asWKT",
        "ex:ref/geo:hasGeometry/geo:asWKT",
        "geo:hasGeometry/geo:asWKT",
    ];
    let tests = [
        "FILTER(geof:sfWithin(?p, ?w))",
        "FILTER(geof:sfIntersects(?w, ?p))",
        "FILTER(!geof:sfDisjoint(?p, ?w))",
        "FILTER(geof:sfWithin(?p, ?w) && ?x != ex:a)",
        "FILTER(geof:distance(?p, ?w, uom:metre) < 1)",
    ];
    for path in paths {
        for test in tests {
            let q = format!(
                "{prefixes} SELECT ?x WHERE {{ ex:region geo:hasGeometry/geo:asWKT ?w . \
                 ?x a ex:P ; {path} ?p {test} }}"
            );
            assert_eq!(rows(&tt, &q), rows(&ox, &q), "{path} {test}");
        }
        // The relation the other way round: the region found from the point's geometry.
        let q = format!(
            "{prefixes} SELECT ?x ?r WHERE {{ ?x a ex:P ; {path} ?p . \
             ?r geo:hasGeometry/geo:asWKT ?w FILTER(geof:sfContains(?w, ?p) && ?r != ?x) }}"
        );
        assert_eq!(rows(&tt, &q), rows(&ox, &q), "{path} contains");
    }
    // The issue's query: a and b, once each.
    let q = format!(
        "{prefixes} SELECT ?x WHERE {{ ex:region geo:hasGeometry/geo:asWKT ?w . \
         ?x a ex:P ; {} ?p FILTER(geof:sfWithin(?p, ?w)) }}",
        paths[0]
    );
    assert_eq!(
        rows(&tt, &q),
        ["?x=<https://example.org/a>", "?x=<https://example.org/b>"]
    );
}

/// The relation as a property (`?g geo:sfWithin ?rg`) after a path ending in a blank node:
/// the same rows as with the path's end written as a variable. (The evaluator alone infers
/// no relation triples, so the written-out form is the reference.)
#[test]
fn relation_triples_keep_paths_joined_through_blank_nodes() {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    let prefixes = "PREFIX geo: <http://www.opengis.net/ont/geosparql#>
        PREFIX ex: <https://example.org/>";
    for (path, written) in [
        (
            "(ex:ref|ex:part)/geo:hasGeometry",
            "(ex:ref|ex:part) ?m . ?m geo:hasGeometry",
        ),
        ("ex:ref?/geo:hasGeometry", "ex:ref? ?m . ?m geo:hasGeometry"),
        (
            "ex:part*/geo:hasGeometry",
            "ex:part* ?m . ?m geo:hasGeometry",
        ),
        (
            "ex:part+/geo:hasGeometry",
            "ex:part+ ?m . ?m geo:hasGeometry",
        ),
        (
            "(ex:part/ex:part?)?/geo:hasGeometry",
            "(ex:part/ex:part?)? ?m . ?m geo:hasGeometry",
        ),
    ] {
        for relation in ["geo:sfWithin", "geo:sfIntersects", "geo:ehInside"] {
            let q = |p: &str| {
                format!(
                    "{prefixes} SELECT ?x ?g WHERE {{ ex:region geo:hasGeometry ?rg . \
                     ?x a ex:P ; {p} ?g . ?g {relation} ?rg }}"
                )
            };
            let want = rows(&e, &q(written));
            assert!(!want.is_empty(), "{written} {relation}");
            assert_eq!(rows(&e, &q(path)), want, "{path} {relation}");
        }
        // The blank node written as a variable for the rewrite is not returned.
        let star = format!(
            "{prefixes} SELECT * WHERE {{ ex:region geo:hasGeometry ?rg . \
             ?x a ex:P ; {path} ?g . ?g geo:sfWithin ?rg }}"
        );
        assert!(
            rows(&e, &star).iter().all(|r| !r.contains("__tt")),
            "{path}"
        );
    }
}
