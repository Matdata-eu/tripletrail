//! The store gives the same SPARQL answers as Oxigraph's own store, keeps its data on disk,
//! and parses geometry literals once.

use oxigraph::io::RdfFormat;
use oxigraph::model::{GraphNameRef, Literal, NamedNodeRef, Quad, QuadRef, Term};
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tripletrail_store::{GraphSelector, Kind, Store, Verification};

const DATA: &str = r#"
@prefix ex: <http://example.com/> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .

ex:a ex:name "Alpha" ; ex:label "alfa"@nb ; ex:n 1 ; ex:x 7 ; ex:ok true ;
     ex:knows ex:b , ex:c ; ex:d 2.5 ; ex:when "2024-01-02"^^xsd:date .
ex:b ex:name "Beta" ; ex:n 2 ; ex:knows ex:c ; ex:big 123456789012345678901234567890 .
ex:c ex:name "Gamma" ; ex:n 3 ; ex:ok false ; ex:empty "" .
_:x ex:name "blank" ; ex:knows ex:a .
ex:s1 geo:asWKT "POINT(10.75 59.91)"^^geo:wktLiteral .
ex:s2 geo:asWKT "POINT(10.40 63.43)"^^geo:wktLiteral .
ex:s3 geo:asWKT "LINESTRING(10 59, 11 60)"^^geo:wktLiteral .
"#;

const NAMED: &str = r#"
<http://example.com/a> <http://example.com/n> "10"^^<http://www.w3.org/2001/XMLSchema#integer> <http://example.com/g1> .
<http://example.com/b> <http://example.com/n> "20"^^<http://www.w3.org/2001/XMLSchema#integer> <http://example.com/g1> .
<http://example.com/a> <http://example.com/n> "30"^^<http://www.w3.org/2001/XMLSchema#integer> <http://example.com/g2> .
_:g <http://example.com/n> "40"^^<http://www.w3.org/2001/XMLSchema#integer> _:graph .
"#;

const QUERIES: &[&str] = &[
    "SELECT * WHERE { ?s ?p ?o }",
    "SELECT * WHERE { GRAPH ?g { ?s ?p ?o } }",
    "SELECT * WHERE { GRAPH <http://example.com/g1> { ?s ?p ?o } }",
    "SELECT * WHERE { GRAPH <http://example.com/none> { ?s ?p ?o } }",
    "SELECT ?g WHERE { GRAPH ?g { } }",
    "PREFIX ex: <http://example.com/> SELECT ?s ?n WHERE { ?s ex:n ?n FILTER(?n > 1) }",
    "PREFIX ex: <http://example.com/> SELECT ?s ?o WHERE { ?s ex:knows ?o . ?o ex:knows ?x }",
    "PREFIX ex: <http://example.com/> SELECT ?s ?l WHERE { ?s ex:name ?n OPTIONAL { ?s ex:label ?l } }",
    "PREFIX ex: <http://example.com/> SELECT (COUNT(*) AS ?c) (SUM(?n) AS ?t) WHERE { ?s ex:n ?n }",
    "PREFIX ex: <http://example.com/> SELECT ?s WHERE { ?s ex:n 2 }",
    "PREFIX ex: <http://example.com/> SELECT ?s WHERE { ?s ex:x 7 }",
    "PREFIX ex: <http://example.com/> SELECT ?s WHERE { ?s ex:x ?x FILTER(?x = 7) }",
    "PREFIX ex: <http://example.com/> SELECT ?s WHERE { ?s ex:ok true }",
    "PREFIX ex: <http://example.com/> SELECT ?s WHERE { ?s ex:ok ?b FILTER(?b) }",
    "PREFIX ex: <http://example.com/> SELECT ?s WHERE { ?s ex:empty ?e FILTER(!?e) }",
    "PREFIX ex: <http://example.com/> SELECT ?s WHERE { ?s ex:missing ?o }",
    "PREFIX ex: <http://example.com/> SELECT ?s ?o WHERE { VALUES ?s { ex:a ex:zzz } ?s ex:n ?o }",
    "PREFIX ex: <http://example.com/> SELECT ?s ?m WHERE { ?s ex:n ?n BIND(?n * 2 AS ?m) FILTER(?m > 2) }",
    "PREFIX ex: <http://example.com/> SELECT ?n WHERE { ?s ex:name ?n } ORDER BY DESC(?n) LIMIT 2",
    "PREFIX ex: <http://example.com/> SELECT ?s WHERE { ?s ex:label \"alfa\"@nb }",
    "PREFIX ex: <http://example.com/> SELECT ?s ?b WHERE { ?s ex:big ?b }",
    "PREFIX ex: <http://example.com/> SELECT ?s ?w WHERE { ?s ex:when ?w FILTER(?w < \"2025-01-01\"^^<http://www.w3.org/2001/XMLSchema#date>) }",
    "PREFIX ex: <http://example.com/> SELECT ?s WHERE { ?s ex:n ?n FILTER NOT EXISTS { ?s ex:knows ?o } }",
    "PREFIX ex: <http://example.com/> SELECT ?n (COUNT(?o) AS ?c) WHERE { ?s ex:name ?n OPTIONAL { ?s ex:knows ?o } } GROUP BY ?n",
    "PREFIX ex: <http://example.com/> ASK { ex:a ex:knows ex:c }",
    "PREFIX ex: <http://example.com/> CONSTRUCT { ?o ex:knownBy ?s } WHERE { ?s ex:knows ?o }",
    "PREFIX ex: <http://example.com/> SELECT * FROM <http://example.com/g1> WHERE { ?s ex:n ?n }",
    "PREFIX ex: <http://example.com/> SELECT * FROM NAMED <http://example.com/g2> WHERE { GRAPH ?g { ?s ex:n ?n } }",
    "PREFIX ex: <http://example.com/> SELECT ?s ?p WHERE { ?s ?p ex:c }",
    "PREFIX ex: <http://example.com/> SELECT ?p WHERE { ex:a ?p ?o FILTER(isLiteral(?o)) }",
    "PREFIX ex: <http://example.com/> SELECT ?s WHERE { ?s ?p ?o FILTER(isBlank(?s)) }",
    "PREFIX geo: <http://www.opengis.net/ont/geosparql#> PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
     SELECT ?a ?b WHERE { ?a geo:asWKT ?wa . ?b geo:asWKT ?wb FILTER(?a != ?b && geof:sfIntersects(?wa, ?wb)) }",
    "PREFIX geo: <http://www.opengis.net/ont/geosparql#> PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
     SELECT ?a (ROUND(geof:distance(?w, \"POINT(10.75 59.91)\"^^geo:wktLiteral, <http://www.opengis.net/def/uom/OGC/1.0/metre>) / 1000) AS ?km)
     WHERE { ?a geo:asWKT ?w }",
];

/// The results as sorted text, so the two stores can be compared.
fn canonical(results: QueryResults<'_>) -> Vec<String> {
    let mut rows = match results {
        QueryResults::Solutions(solutions) => {
            let vars: Vec<_> = solutions.variables().to_vec();
            solutions
                .map(|s| {
                    let s = s.unwrap();
                    vars.iter()
                        .map(|v| match s.get(v) {
                            // Blank node labels differ between stores.
                            Some(Term::BlankNode(_)) => "_:".into(),
                            Some(t) => t.to_string(),
                            None => "-".into(),
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect::<Vec<_>>()
        }
        QueryResults::Boolean(b) => vec![b.to_string()],
        QueryResults::Graph(triples) => triples
            .map(|t| {
                // Blank node labels differ between stores.
                t.unwrap()
                    .to_string()
                    .split(' ')
                    .map(|w| if w.starts_with("_:") { "_:" } else { w })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect(),
    };
    rows.sort();
    rows
}

fn load_both() -> (oxigraph::store::Store, Store) {
    let reference = oxigraph::store::Store::new().unwrap();
    reference.load_from_slice(RdfFormat::Turtle, DATA).unwrap();
    reference.load_from_slice(RdfFormat::NQuads, NAMED).unwrap();
    let store = Store::new();
    store.bulk_load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    store
        .bulk_load(RdfFormat::NQuads, NAMED.as_bytes())
        .unwrap();
    (reference, store)
}

fn evaluator() -> SparqlEvaluator {
    tripletrail_geo::register(SparqlEvaluator::new())
}

fn assert_same_answers(reference: &oxigraph::store::Store, store: &Store) {
    for q in QUERIES {
        let expected = canonical(
            evaluator()
                .parse_query(q)
                .unwrap()
                .on_store(reference)
                .execute()
                .unwrap(),
        );
        let got = canonical(
            evaluator()
                .parse_query(q)
                .unwrap()
                .on_queryable_dataset(store.snapshot())
                .execute()
                .unwrap(),
        );
        assert_eq!(got, expected, "{q}");
    }
}

#[test]
fn same_answers_as_oxigraph() {
    let (reference, store) = load_both();
    assert_eq!(store.len(), reference.len().unwrap());
    assert_eq!(store.named_graphs().len(), 3);
    assert_same_answers(&reference, &store);
}

#[test]
fn same_answers_after_writes() {
    let (reference, store) = load_both();
    let ex = |s: &str| NamedNodeRef::new_unchecked(s).into_owned();
    let quad = |s, p, o: Term| Quad::new(ex(s), ex(p), o, GraphNameRef::DefaultGraph);
    let added = [
        quad(
            "http://example.com/d",
            "http://example.com/n",
            Literal::from(4).into(),
        ),
        quad(
            "http://example.com/d",
            "http://example.com/knows",
            ex("http://example.com/a").into(),
        ),
    ];
    let removed = quad(
        "http://example.com/b",
        "http://example.com/n",
        Literal::from(2).into(),
    );
    for q in &added {
        assert!(store.insert(q).unwrap());
        assert!(!store.insert(q).unwrap());
        reference.insert(q).unwrap();
    }
    assert!(store.remove(&removed).unwrap());
    assert!(!store.remove(&removed).unwrap());
    reference.remove(&removed).unwrap();
    assert_eq!(store.len(), reference.len().unwrap());
    assert_same_answers(&reference, &store);

    // A snapshot keeps seeing the data as it was.
    let before = store.snapshot();
    store.remove(&added[0]).unwrap();
    assert!(before.contains(&added[0]));
    assert!(!store.contains(&added[0]));
    store.insert(&added[0]).unwrap();

    // A failed transaction changes nothing.
    let result: Result<(), tripletrail_store::StoreError> = store.transaction(|t| {
        t.remove(&added[1]);
        Err(tripletrail_store::StoreError::Unsupported(
            "rollback".into(),
        ))
    });
    assert!(result.is_err());
    assert!(store.contains(&added[1]));
}

#[test]
fn literals_keep_their_lexical_form() {
    // Oxigraph stores "+7"^^xsd:integer as 7; these are different RDF terms, so the triple
    // pattern { ?s ex:x 7 } must not match "+7". Numeric comparison still does.
    let store = Store::new();
    store
        .bulk_load(
            RdfFormat::Turtle,
            r#"@prefix ex: <http://example.com/> . @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
               ex:a ex:x "+7"^^xsd:integer ; ex:e 1.5e0 ."#
                .as_bytes(),
        )
        .unwrap();
    let ask = |q: &str| match evaluator()
        .parse_query(q)
        .unwrap()
        .on_queryable_dataset(store.snapshot())
        .execute()
        .unwrap()
    {
        QueryResults::Boolean(b) => b,
        _ => unreachable!(),
    };
    let ex = "PREFIX ex: <http://example.com/> ";
    assert!(!ask(&format!("{ex} ASK {{ ex:a ex:x 7 }}")));
    assert!(ask(&format!("{ex} ASK {{ ex:a ex:x ?x FILTER(?x = 7) }}")));
    assert!(ask(&format!(
        "{ex} ASK {{ ex:a ex:e ?x FILTER(?x = 1.5) }}"
    )));
    let e = store
        .quads_for_pattern(
            None,
            Some(NamedNodeRef::new_unchecked("http://example.com/e")),
            None,
            None,
        )
        .next()
        .unwrap();
    assert_eq!(
        e.object.to_string(),
        "\"1.5e0\"^^<http://www.w3.org/2001/XMLSchema#double>"
    );
}

#[test]
fn integers_and_geometries_are_stored_by_id() {
    let (_, store) = load_both();
    store
        .bulk_load(
            RdfFormat::Turtle,
            r#"<http://example.com/a> <http://example.com/x> "+7"^^<http://www.w3.org/2001/XMLSchema#integer> ."#.as_bytes(),
        )
        .unwrap();
    let snapshot = store.snapshot();
    let one = Term::from(Literal::from(1));
    assert_eq!(
        snapshot.term_id(one.as_ref()).unwrap().kind(),
        Kind::Integer
    );
    let plus_seven = Term::from(Literal::new_typed_literal(
        "+7",
        oxigraph::model::vocab::xsd::INTEGER,
    ));
    let id = snapshot.term_id(plus_seven.as_ref()).unwrap();
    assert_eq!(
        id.kind(),
        Kind::Typed,
        "non-canonical forms keep their text"
    );
    assert_eq!(snapshot.term(id).unwrap(), plus_seven);

    let wkt = Term::from(Literal::new_typed_literal(
        "POINT(10.40 63.43)",
        tripletrail_geo::WKT_LITERAL,
    ));
    let id = snapshot.term_id(wkt.as_ref()).unwrap();
    assert_eq!(id.kind(), Kind::Geometry);
    // Geometries are parsed on first use, and then kept.
    assert_eq!(store.memory_use().geometries, 0);
    let geometry = snapshot.geometry(id).expect("a valid point");
    assert!(matches!(geometry.geometry, geo::Geometry::Point(_)));
    assert_eq!(store.memory_use().geometries, 1);
    assert!(Arc::ptr_eq(&geometry, &snapshot.geometry(id).unwrap()));
}

fn temp_dir(name: &str) -> tripletrail_testdir::TestDir {
    tripletrail_testdir::tempdir(&format!("store-{name}"))
}

#[test]
fn data_survives_a_restart() {
    let dir = temp_dir("restart");
    let ex = |s: &str| NamedNodeRef::new_unchecked(s).into_owned();
    let q1 = Quad::new(
        ex("http://example.com/z"),
        ex("http://example.com/n"),
        Literal::from(99),
        GraphNameRef::DefaultGraph,
    );
    let q2 = Quad::new(
        ex("http://example.com/z"),
        ex("http://example.com/name"),
        Literal::new_simple_literal("line one\nline \"two\""),
        ex("http://example.com/g1"),
    );
    let expected: Vec<String>;
    {
        let store = Store::open(&dir).unwrap();
        store.bulk_load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
        store
            .bulk_load(RdfFormat::NQuads, NAMED.as_bytes())
            .unwrap();
        store.insert(&q1).unwrap();
        store.insert(&q2).unwrap();
        store
            .remove(QuadRef::new(
                NamedNodeRef::new_unchecked("http://example.com/a"),
                NamedNodeRef::new_unchecked("http://example.com/n"),
                &Literal::from(1),
                GraphNameRef::DefaultGraph,
            ))
            .unwrap();
        expected = sorted(&store);
    }
    // Reopen: checkpoint plus log replay.
    let store = Store::open(&dir).unwrap();
    assert_eq!(sorted(&store), expected);
    assert!(store.contains(&q2));

    // A checkpoint folds the log in.
    store.checkpoint().unwrap();
    drop(store);
    let store = Store::open(&dir).unwrap();
    assert_eq!(sorted(&store), expected);
    drop(store);

    // A transaction cut short by a crash is dropped.
    let wal = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("wal-"))
        .unwrap()
        .path();
    std::fs::write(
        &wal,
        "+ <http://example.com/half> <http://example.com/n> <http://example.com/x> .\n+ <http://ex",
    )
    .unwrap();
    let store = Store::open(&dir).unwrap();
    assert_eq!(sorted(&store), expected);
    store
        .insert(&Quad::new(
            ex("http://example.com/after"),
            ex("http://example.com/n"),
            Literal::from(1),
            GraphNameRef::DefaultGraph,
        ))
        .unwrap();
    drop(store);
    let store = Store::open(&dir).unwrap();
    assert_eq!(store.len(), expected.len() + 1);
    drop(store);
}

fn sorted(store: &Store) -> Vec<String> {
    let mut v: Vec<String> = store.iter().map(|q| q.to_string()).collect();
    v.sort();
    v
}

#[test]
fn many_writes_compact_and_stay_correct() {
    let store = Store::new();
    let ex = |s: String| oxigraph::model::NamedNode::new_unchecked(s);
    let p = ex("http://example.com/p".into());
    let reference = oxigraph::store::Store::new().unwrap();
    // More single-quad writes than the delta holds, so compaction runs in between.
    store
        .transaction(|t| {
            for i in 0..70_000 {
                let q = Quad::new(
                    ex(format!("http://example.com/s{}", i % 1000)),
                    p.clone(),
                    Literal::from(i),
                    GraphNameRef::DefaultGraph,
                );
                t.insert(&q);
                reference.insert(&q).unwrap();
            }
            Ok::<_, tripletrail_store::StoreError>(())
        })
        .unwrap();
    for i in (0..70_000).step_by(7) {
        let q = Quad::new(
            ex(format!("http://example.com/s{}", i % 1000)),
            p.clone(),
            Literal::from(i),
            GraphNameRef::DefaultGraph,
        );
        store.remove(&q).unwrap();
        reference.remove(&q).unwrap();
    }
    assert_eq!(store.len(), reference.len().unwrap());
    let q = "SELECT ?s (COUNT(*) AS ?c) (SUM(?o) AS ?t) WHERE { ?s ?p ?o } GROUP BY ?s";
    let expected = canonical(
        evaluator()
            .parse_query(q)
            .unwrap()
            .on_store(&reference)
            .execute()
            .unwrap(),
    );
    let got = canonical(
        evaluator()
            .parse_query(q)
            .unwrap()
            .on_queryable_dataset(store.snapshot())
            .execute()
            .unwrap(),
    );
    assert_eq!(got, expected);
}

#[test]
fn updates_match_oxigraph() {
    let updates = [
        "PREFIX ex: <http://example.com/> INSERT DATA { ex:n1 ex:n 5 . _:b ex:name \"new\" . GRAPH ex:g3 { ex:n1 ex:p ex:n2 } }",
        "PREFIX ex: <http://example.com/> DELETE DATA { ex:a ex:n 1 }",
        "PREFIX ex: <http://example.com/> DELETE { ?s ex:n ?n } INSERT { ?s ex:n2 ?m ; ex:tag [ ex:of ?s ] } WHERE { ?s ex:n ?n BIND(?n * 10 AS ?m) }",
        "PREFIX ex: <http://example.com/> DELETE WHERE { ?s ex:knows ex:c }",
        "PREFIX ex: <http://example.com/> INSERT { GRAPH ex:g4 { ?s ?p ?o } } WHERE { GRAPH ex:g1 { ?s ?p ?o } }",
        "PREFIX ex: <http://example.com/> WITH ex:g2 DELETE { ?s ?p ?o } WHERE { ?s ?p ?o }",
        "PREFIX ex: <http://example.com/> CLEAR GRAPH ex:g1",
        "PREFIX ex: <http://example.com/> DROP SILENT GRAPH ex:nothing",
        "PREFIX ex: <http://example.com/> COPY ex:g4 TO ex:g5",
        "PREFIX ex: <http://example.com/> MOVE ex:g5 TO DEFAULT",
        "PREFIX ex: <http://example.com/> INSERT DATA { ex:x ex:y 1 } ; DELETE { ?s ex:y ?o } INSERT { ?s ex:z ?o } WHERE { ?s ex:y ?o }",
        "DROP NAMED",
        "CLEAR ALL",
    ];
    let (reference, store) = load_both();
    for u in updates {
        evaluator()
            .parse_update(u)
            .unwrap()
            .on_store(&reference)
            .execute()
            .unwrap();
        let parsed = spargebra::SparqlParser::new().parse_update(u).unwrap();
        store.update(&parsed, evaluator).unwrap();
        let normal = |mut v: Vec<String>| {
            v.iter_mut().for_each(|q| {
                *q = q
                    .split(' ')
                    .map(|w| if w.starts_with("_:") { "_:" } else { w })
                    .collect::<Vec<_>>()
                    .join(" ")
            });
            v.sort();
            v
        };
        let expected = normal(reference.iter().map(|q| q.unwrap().to_string()).collect());
        assert_eq!(
            normal(store.iter().map(|q| q.to_string()).collect()),
            expected,
            "{u}"
        );
    }
    let missing = spargebra::SparqlParser::new()
        .parse_update("CLEAR GRAPH <http://example.com/nothing>")
        .unwrap();
    assert!(store.update(&missing, evaluator).is_err());
}

#[test]
fn pattern_counts_match_the_quads() {
    use oxigraph::model::{GraphName, NamedNode, NamedOrBlankNodeRef};
    let store = Store::new();
    let nn = |s: String| NamedNode::new_unchecked(s);
    let quad = |i: usize| {
        Quad::new(
            nn(format!("http://example.com/s{}", i % 997)),
            nn(format!("http://example.com/p{}", i % 7)),
            Literal::from(format!("o{}", i % 113)),
            if i.is_multiple_of(5) {
                GraphName::NamedNode(nn(format!("http://example.com/g{}", i % 3)))
            } else {
                GraphName::DefaultGraph
            },
        )
    };
    store.bulk_load_quads((0..20_000).map(quad)).unwrap();
    // A delta of inserts and removals on top of the segments.
    for i in 20_000..21_000 {
        store.insert(&quad(i)).unwrap();
    }
    for i in (0..20_000).step_by(37) {
        store.remove(&quad(i)).unwrap();
    }
    let p3 = nn("http://example.com/p3".into());
    let s5 = nn("http://example.com/s5".into());
    let o7: Term = Literal::from("o7").into();
    let g1 = nn("http://example.com/g1".into());
    let unknown = nn("http://example.com/nothing".into());
    for (s, p, o) in [
        (None, None, None),
        (None, Some(p3.as_ref()), None),
        (None, Some(p3.as_ref()), Some(o7.as_ref())),
        (Some(s5.as_ref()), None, None),
        (None, None, Some(o7.as_ref())),
        (Some(s5.as_ref()), Some(p3.as_ref()), None),
        (None, Some(unknown.as_ref()), None),
    ] {
        let s = s.map(NamedOrBlankNodeRef::from);
        for g in [
            None,
            Some(GraphNameRef::DefaultGraph),
            Some(GraphNameRef::NamedNode(g1.as_ref())),
        ] {
            assert_eq!(
                store.estimate_len(s, p, o, g),
                store.quads_for_pattern(s, p, o, g).count(),
                "{s:?} {p:?} {o:?} {g:?}"
            );
        }
    }
}

#[test]
fn parsed_geometries_are_saved_with_the_checkpoint() {
    let dir = temp_dir("geoms");
    let wkt = |v: &str| {
        Literal::new_typed_literal(
            v,
            NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#wktLiteral"),
        )
    };
    let literals = [
        wkt("POLYGON((0 0, 4 0, 4 4, 0 4, 0 0))"),
        wkt("<http://www.opengis.net/def/crs/EPSG/0/4326> POINT(50.5 4.25)"),
        // Not held in binary form: parsed from its text after the restart.
        wkt("POINT Z(1 2 3)"),
    ];
    let expected: Vec<_> = {
        let store = Store::open(&dir).unwrap();
        for (i, l) in literals.iter().enumerate() {
            store
                .insert(QuadRef::new(
                    NamedNodeRef::new_unchecked(&format!("http://example.com/g{i}")),
                    NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#asWKT"),
                    l,
                    GraphNameRef::DefaultGraph,
                ))
                .unwrap();
        }
        // Only the first is parsed before the checkpoint; it parses the others to save them.
        let first = store.geometry(literals[0].as_ref().into()).unwrap();
        assert_eq!(store.memory_use().geometries, 1);
        store.checkpoint().unwrap();
        assert_eq!(store.memory_use().geometries, 1);
        assert!(Arc::ptr_eq(
            &first,
            &store.geometry(literals[0].as_ref().into()).unwrap()
        ));
        literals
            .iter()
            .map(|l| store.geometry(l.as_ref().into()).unwrap())
            .collect()
    };
    let checkpoint = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("checkpoint-"))
        .unwrap()
        .path();
    assert!(checkpoint.join("geoms.bin").exists());
    let store = Store::open(&dir).unwrap();
    for (l, g) in literals.iter().zip(&expected) {
        let back = store.geometry(l.as_ref().into()).unwrap();
        assert_eq!(back.geometry, g.geometry, "{l}");
        assert_eq!(back.crs, g.crs, "{l}");
        assert_eq!(back.zm.is_some(), g.zm.is_some(), "{l}");
    }
    // A damaged file is ignored: the literals are parsed from their text.
    drop(store);
    std::fs::write(checkpoint.join("geoms.bin"), b"OXGSGEO1garbage").unwrap();
    let store = Store::open(&dir).unwrap();
    let back = store.geometry(literals[0].as_ref().into()).unwrap();
    assert_eq!(back.geometry, expected[0].geometry);
}

#[test]
fn writes_next_to_a_live_iterator_keep_both_views() {
    let store = Store::new();
    let nt: String = (0..2000)
        .flat_map(|i| (0..3).map(move |j| format!("<http://ex/s{i}> <http://ex/p{j}> \"{i}\" .\n")))
        .collect();
    store.bulk_load(RdfFormat::NTriples, nt.as_bytes()).unwrap();
    let before = store.len();
    // Remove every quad of every subject while iterating over it, and add one per subject:
    // each iterator keeps the version it started from.
    for i in 0..2000 {
        let s = format!("http://ex/s{i}");
        let subject = NamedNodeRef::new_unchecked(&s);
        let mut seen = 0;
        for q in store.quads_for_pattern(Some(subject.into()), None, None, None) {
            assert!(store.remove(&q).unwrap());
            seen += 1;
        }
        assert_eq!(seen, 3);
        store
            .insert(QuadRef::new(
                subject,
                NamedNodeRef::new_unchecked("http://ex/new"),
                &Literal::from(i),
                GraphNameRef::DefaultGraph,
            ))
            .unwrap();
        assert_eq!(
            store
                .quads_for_pattern(Some(subject.into()), None, None, None)
                .count(),
            1
        );
    }
    assert_eq!(store.len(), before / 3);
}

fn sorted_quads(store: &Store) -> Vec<String> {
    let mut v: Vec<String> = store.iter().map(|q| q.to_string()).collect();
    v.sort();
    v
}

#[test]
fn a_saved_store_opens_read_only_and_read_write() {
    let dir = temp_dir("save");
    let target = dir.join("snapshot");
    let store = Store::new();
    store.bulk_load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    store
        .bulk_load(RdfFormat::NQuads, NAMED.as_bytes())
        .unwrap();
    // A write after the bulk load, still in the delta.
    let extra = Quad::new(
        NamedNodeRef::new_unchecked("http://example.com/z"),
        NamedNodeRef::new_unchecked("http://example.com/n"),
        Literal::from(99),
        GraphNameRef::DefaultGraph,
    );
    store.insert(&extra).unwrap();
    store.save_to(&target).unwrap();
    let expected = sorted_quads(&store);
    let wkt = Literal::new_typed_literal(
        "POINT(10.75 59.91)",
        NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#wktLiteral"),
    );

    let ro = Store::open_read_only(&target).unwrap();
    assert!(ro.is_read_only());
    assert!(ro.mapped_size() > 0);
    assert_eq!(sorted_quads(&ro), expected);
    assert!(ro.contains(&extra));
    assert!(ro.geometry(wkt.as_ref().into()).is_some());
    assert!(matches!(
        ro.insert(&extra),
        Err(tripletrail_store::StoreError::ReadOnly)
    ));
    assert!(matches!(
        ro.bulk_load(RdfFormat::NQuads, NAMED.as_bytes()),
        Err(tripletrail_store::StoreError::ReadOnly)
    ));
    // Queries run on the mapped segments.
    for query in QUERIES {
        let a = run(&ro, query);
        let b = run(&store, query);
        assert_eq!(a, b, "{query}");
    }

    // The same directory opens read-write too, and keeps writes.
    drop(ro);
    {
        let rw = Store::open(&target).unwrap();
        assert_eq!(sorted_quads(&rw), expected);
        assert!(rw.remove(&extra).unwrap());
    }
    let reopened = Store::open_read_only(&target).unwrap();
    assert!(!reopened.contains(&extra));
    assert_eq!(reopened.len(), expected.len() - 1);

    // Never over an existing store.
    assert!(store.save_to(&target).is_err());
}

#[test]
fn read_only_open_changes_nothing_on_disk() {
    let dir = temp_dir("ro-untouched");
    {
        let store = Store::open(&dir).unwrap();
        store.bulk_load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
        // Logged, not checkpointed.
        store
            .insert(QuadRef::new(
                NamedNodeRef::new_unchecked("http://example.com/q"),
                NamedNodeRef::new_unchecked("http://example.com/n"),
                &Literal::from(5),
                GraphNameRef::DefaultGraph,
            ))
            .unwrap();
    }
    let listing = |d: &std::path::Path| {
        let mut v: Vec<(String, u64)> = std::fs::read_dir(d)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    e.metadata().unwrap().len(),
                )
            })
            .collect();
        v.sort();
        v
    };
    let before = listing(&dir);
    let ro = Store::open_read_only(&dir).unwrap();
    assert_eq!(listing(&dir), before);
    assert!(ro.contains(QuadRef::new(
        NamedNodeRef::new_unchecked("http://example.com/q"),
        NamedNodeRef::new_unchecked("http://example.com/n"),
        &Literal::from(5),
        GraphNameRef::DefaultGraph,
    )));
    assert!(Store::open_read_only(dir.join("missing")).is_err());
}

fn run(store: &Store, query: &str) -> Vec<String> {
    canonical(
        evaluator()
            .parse_query(query)
            .unwrap()
            .on_queryable_dataset(store.snapshot())
            .execute()
            .unwrap(),
    )
}

/// Opens `dir` and reads everything, catching a panic: damaged files must give an error.
fn open_and_scan(dir: &std::path::Path) -> Result<usize, String> {
    std::panic::catch_unwind(|| match Store::open(dir) {
        Ok(store) => Ok(store.iter().count()),
        Err(e) => Err(e.to_string()),
    })
    .unwrap_or_else(|_| panic!("opening {} panicked", dir.display()))
}

#[test]
fn damaged_files_are_errors_not_panics() {
    let dir = temp_dir("damaged");
    let store = Store::open(&dir).unwrap();
    store.bulk_load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    store.checkpoint().unwrap();
    let len = store.len();
    drop(store);
    assert_eq!(open_and_scan(&dir), Ok(len));

    let checkpoint = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("checkpoint-"))
        .unwrap()
        .path();
    let files: Vec<PathBuf> = std::fs::read_dir(&checkpoint)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert!(files.len() > 1);
    for file in &files {
        let original = std::fs::read(file).unwrap();
        let mut damaged: Vec<Vec<u8>> = vec![
            Vec::new(),
            original[..original.len() / 2].to_vec(),
            original[..original.len() - 1].to_vec(),
            [original.as_slice(), b"x"].concat(),
        ];
        // Huge lengths in each header word (the first used to allocate exabytes).
        for word in 0..4 {
            let at = 8 + word * 8;
            if at + 8 <= original.len() {
                let mut d = original.clone();
                d[at..at + 8].copy_from_slice(&(1u64 << 62).to_le_bytes());
                damaged.push(d);
            }
        }
        // Every bit of the header flipped, and a sample of bits further on.
        let step = (original.len() / 64).max(1);
        for at in (0..original.len().min(48)).chain((48..original.len()).step_by(step)) {
            for bit in [0, 7] {
                let mut d = original.clone();
                d[at] ^= 1 << bit;
                damaged.push(d);
            }
        }
        for d in damaged {
            std::fs::write(file, &d).unwrap();
            // Either an error, or (a flip that keeps the file consistent) a full scan.
            let _ = open_and_scan(&dir);
        }
        std::fs::write(file, &original).unwrap();
    }
    assert_eq!(open_and_scan(&dir), Ok(len));

    // A damaged CURRENT must not make opening delete the checkpoint it should point to.
    let current = dir.join("CURRENT");
    let original = std::fs::read_to_string(&current).unwrap();
    let n: u64 = original.trim().parse().unwrap();
    assert!(n > 0);
    for damaged in [
        format!("{}\n", n - 1),
        format!("{}\n", n + 1),
        "0\n".to_owned(),
        String::new(),
        "x".to_owned(),
    ] {
        std::fs::write(&current, &damaged).unwrap();
        assert!(open_and_scan(&dir).is_err(), "CURRENT {damaged:?}");
    }
    std::fs::remove_file(&current).unwrap();
    assert!(open_and_scan(&dir).is_err(), "no CURRENT");
    std::fs::write(&current, original).unwrap();
    assert_eq!(open_and_scan(&dir), Ok(len));

    // Log lines that are too short or not N-Quads.
    let wal = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("wal-"))
        .unwrap()
        .path();
    for log in [
        "+\nC\n",
        "-\nC\n",
        "+ \nC\n",
        "+ junk\nC\n",
        "?\nC\n",
        "\nC\n",
    ] {
        std::fs::write(&wal, log).unwrap();
        assert!(open_and_scan(&dir).is_err(), "{log:?}");
    }
}

#[test]
fn a_small_load_into_a_large_store_is_logged_not_checkpointed() {
    let dir = temp_dir("small-load");
    let nt = |from: usize, to: usize| -> String {
        (from..to)
            .map(|i| format!("<http://example.com/s{i}> <http://example.com/p> \"{i}\" .\n"))
            .collect()
    };
    {
        let store = Store::open(&dir).unwrap();
        // The first load into an empty store writes a checkpoint.
        store
            .bulk_load(RdfFormat::NTriples, nt(0, 1000).as_bytes())
            .unwrap();
        let current = std::fs::read_to_string(dir.join("CURRENT")).unwrap();
        // A small one goes to the log; the checkpoint stays as it was.
        let added = store
            .bulk_load(RdfFormat::NTriples, nt(990, 1010).as_bytes())
            .unwrap();
        assert_eq!(added, 20);
        assert_eq!(store.len(), 1010);
        assert_eq!(
            std::fs::read_to_string(dir.join("CURRENT")).unwrap(),
            current
        );
        let log = std::fs::read_to_string(dir.join(format!("wal-{}.log", current.trim()))).unwrap();
        // Only the ten new quads are logged.
        assert_eq!(log.lines().filter(|l| l.starts_with('+')).count(), 10);
    }
    let store = Store::open(&dir).unwrap();
    assert_eq!(store.len(), 1010);
    let ex = |s: &str| NamedNodeRef::new_unchecked(s).into_owned();
    assert!(store.contains(&Quad::new(
        ex("http://example.com/s1009"),
        ex("http://example.com/p"),
        Literal::new_simple_literal("1009"),
        GraphNameRef::DefaultGraph,
    )));
}

#[test]
fn a_commit_succeeds_when_the_checkpoint_after_it_fails() {
    let dir = temp_dir("checkpoint-fails");
    let ex = |s: &str| NamedNodeRef::new_unchecked(s).into_owned();
    let quad = |o: &str| {
        Quad::new(
            ex("http://example.com/s"),
            ex("http://example.com/p"),
            ex(o),
            GraphNameRef::DefaultGraph,
        )
    };
    {
        let store = Store::open(&dir).unwrap();
        // Every commit asks for a checkpoint, and the next checkpoint's directory cannot be
        // created: a file is in its place.
        store.set_checkpoint_log_size(0);
        std::fs::write(dir.join("checkpoint-1"), b"in the way").unwrap();
        assert!(store.insert(&quad("http://example.com/a")).unwrap());
        assert!(store.insert(&quad("http://example.com/b")).unwrap());
        assert_eq!(store.len(), 2);
        std::fs::remove_file(dir.join("checkpoint-1")).unwrap();
    }
    // Both transactions were in the log.
    let store = Store::open(&dir).unwrap();
    assert!(store.contains(&quad("http://example.com/a")));
    assert!(store.contains(&quad("http://example.com/b")));
}

#[test]
fn a_panic_inside_a_transaction_does_not_break_later_writes() {
    let store = Store::new();
    let ex = |s: &str| NamedNodeRef::new_unchecked(s).into_owned();
    let quad = Quad::new(
        ex("http://example.com/s"),
        ex("http://example.com/p"),
        ex("http://example.com/o"),
        GraphNameRef::DefaultGraph,
    );
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store
            .transaction(|t| -> Result<(), tripletrail_store::StoreError> {
                t.insert(quad.as_ref());
                panic!("a bug in the caller");
            })
            .unwrap();
    }));
    assert!(panicked.is_err());
    // The panicking transaction wrote nothing, and the store takes writes again.
    assert_eq!(store.len(), 0);
    assert!(store.insert(&quad).unwrap());
    assert_eq!(store.len(), 1);
}

#[test]
fn a_damaged_checkpoint_file_is_reported_as_corrupt() {
    let dir = temp_dir("checksums");
    let expected = {
        let store = Store::open(&dir).unwrap();
        store.bulk_load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
        store.checkpoint().unwrap();
        sorted(&store)
    };
    let checkpoint = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("checkpoint-"))
        .unwrap()
        .path();
    let sums = std::fs::read_to_string(checkpoint.join("checksums")).unwrap();
    assert!(
        sums.contains("dict.bin ") && sums.contains("geoms.bin "),
        "{sums}"
    );
    let flip = |file: &str| {
        let path = checkpoint.join(file);
        let mut bytes = std::fs::read(&path).unwrap();
        let at = bytes.len() - 1;
        bytes[at] ^= 0x55;
        std::fs::write(&path, bytes).unwrap();
    };
    // Saved geometries are a cache: a damaged file is ignored and the literals parsed again.
    flip("geoms.bin");
    let store = Store::open(&dir).unwrap();
    assert_eq!(sorted(&store), expected);
    drop(store);
    // A damaged dictionary or segment byte, where no header check would notice it, is not
    // read as data.
    flip("dict.bin");
    let error = Store::open(&dir)
        .err()
        .expect("a damaged dictionary is an error");
    assert!(error.to_string().contains("dict.bin"), "{error}");
    assert!(Store::open_read_only(&dir).is_err());
    flip("dict.bin");
    assert_eq!(sorted(&Store::open(&dir).unwrap()), expected);
}

#[test]
fn a_checkpoint_without_psog_and_gpso_opens_and_scans_by_subject() {
    let dir = temp_dir("six-permutations");
    let expected = {
        let store = Store::open(&dir).unwrap();
        store.bulk_load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
        store.checkpoint().unwrap();
        sorted(&store)
    };
    // As written before the two permutations existed: no files, no checksums for them.
    let checkpoint = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("checkpoint-"))
        .unwrap()
        .path();
    for name in ["psog.seg", "gpso.seg"] {
        std::fs::remove_file(checkpoint.join(name)).unwrap();
    }
    let sums = std::fs::read_to_string(checkpoint.join("checksums")).unwrap();
    let sums: String = sums
        .lines()
        .filter(|l| !l.starts_with("psog.seg ") && !l.starts_with("gpso.seg "))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(checkpoint.join("checksums"), sums).unwrap();
    for read_only in [true, false] {
        let store = if read_only {
            Store::open_read_only(&dir).unwrap()
        } else {
            Store::open(&dir).unwrap()
        };
        assert_eq!(sorted(&store), expected);
        let snapshot = store.snapshot();
        let knows = snapshot
            .term_id(
                NamedNodeRef::new("http://example.com/knows")
                    .unwrap()
                    .into(),
            )
            .unwrap();
        let by_subject: Vec<_> = snapshot
            .quad_ids_sorted(None, Some(knows), None, GraphSelector::Default, 0)
            .collect();
        assert_eq!(by_subject.len(), 4);
        assert!(by_subject.is_sorted_by_key(|q| q[0]));
    }
    // The next checkpoint writes them.
    let store = Store::open(&dir).unwrap();
    store.checkpoint().unwrap();
    drop(store);
    let written = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("checkpoint-"))
        .any(|e| e.path().join("psog.seg").exists() && e.path().join("gpso.seg").exists());
    assert!(written);
    assert_eq!(sorted(&Store::open(&dir).unwrap()), expected);
}

#[test]
fn predicate_fanouts_come_from_the_counts() {
    let store = Store::new();
    store.bulk_load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    let snapshot = store.snapshot();
    let id = |p: &str| {
        snapshot
            .term_id(NamedNodeRef::new(p).unwrap().into())
            .unwrap()
    };
    let knows = id("http://example.com/knows");
    // Subjects a (2), b (1), _:x (1): (4 + 1 + 1) / 4 rounded up. Objects c (2), b, a.
    assert_eq!(snapshot.predicate_fanout(knows, true), Some(2));
    assert_eq!(snapshot.predicate_fanout(knows, false), Some(2));
    let name = id("http://example.com/name");
    assert_eq!(snapshot.predicate_fanout(name, true), Some(1));
    assert_eq!(snapshot.predicate_fanout(name, false), Some(1));
    // A predicate only in the delta has none: the planner samples it.
    let new = NamedNodeRef::new("http://example.com/new").unwrap();
    let a = NamedNodeRef::new("http://example.com/a").unwrap();
    store
        .insert(QuadRef::new(a, new, a, GraphNameRef::DefaultGraph))
        .unwrap();
    let snapshot = store.snapshot();
    let new = snapshot.term_id(new.into()).unwrap();
    assert_eq!(snapshot.predicate_fanout(new, true), None);
    assert_eq!(snapshot.predicate_fanout(knows, true), Some(2));
}

#[test]
fn small_writes_into_a_large_store_compact_in_place_and_survive_a_restart() {
    // A delta far smaller than the segments only rewrites the blocks it touches (#213):
    // reads, counts and the saved checkpoint must still match the quads.
    let dir = temp_dir("partial-compaction");
    let ex = |s: String| oxigraph::model::NamedNode::new_unchecked(s);
    let quad = |i: u64| {
        Quad::new(
            ex(format!("http://example.com/s{}", i % 997)),
            ex(format!("http://example.com/p{}", i % 7)),
            Literal::from(i as i64),
            if i.is_multiple_of(5) {
                oxigraph::model::GraphName::from(ex(format!("http://example.com/g{}", i % 3)))
            } else {
                oxigraph::model::GraphName::DefaultGraph
            },
        )
    };
    let mut model = std::collections::BTreeSet::new();
    let expected = {
        let store = Store::open(&dir).unwrap();
        store.bulk_load_quads((0..40_000).map(quad)).unwrap();
        model.extend((0..40_000).map(|i| quad(i).to_string()));
        let mut seed = 12345u64;
        for round in 0..12u64 {
            store
                .transaction(|t| {
                    for _ in 0..150 {
                        seed = seed
                            .wrapping_mul(6364136223846793005)
                            .wrapping_add(1442695040888963407);
                        let i = (seed >> 33) % 60_000;
                        let q = quad(i);
                        if (seed >> 20).is_multiple_of(3) {
                            t.remove(&q);
                            model.remove(&q.to_string());
                        } else {
                            t.insert(&q);
                            model.insert(q.to_string());
                        }
                    }
                    Ok::<_, tripletrail_store::StoreError>(())
                })
                .unwrap();
            // Every round folds its writes into the segments.
            store.checkpoint().unwrap();
            let got = sorted(&store);
            assert_eq!(
                got,
                model.iter().cloned().collect::<Vec<_>>(),
                "round {round}"
            );
            let p3 = ex("http://example.com/p3".into());
            let by_p = store
                .quads_for_pattern(None, Some(p3.as_ref()), None, None)
                .count();
            let model_p = model.iter().filter(|q| q.contains("/p3>")).count();
            assert_eq!(by_p, model_p, "round {round}");
            // The counts kept with the merged segments stay exact.
            let s5 = ex("http://example.com/s5".into());
            let g1 = ex("http://example.com/g1".into());
            for (s, p) in [
                (None, None),
                (None, Some(p3.as_ref())),
                (Some(s5.as_ref()), None),
            ] {
                let s = s.map(oxigraph::model::NamedOrBlankNodeRef::from);
                for g in [
                    None,
                    Some(GraphNameRef::DefaultGraph),
                    Some(g1.as_ref().into()),
                ] {
                    assert_eq!(
                        store.estimate_len(s, p, None, g),
                        store.quads_for_pattern(s, p, None, g).count(),
                        "round {round} {s:?} {p:?} {g:?}"
                    );
                }
            }
        }
        sorted(&store)
    };
    let store = Store::open(&dir).unwrap();
    assert_eq!(sorted(&store), expected);
    drop(store);
    let store = Store::open_read_only(&dir).unwrap();
    assert_eq!(sorted(&store), expected);
    drop(store);
}

#[test]
fn string_ranks_order_terms_and_are_rebuilt_as_the_dictionary_grows() {
    let store = Store::new();
    store.bulk_load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    let snap = store.snapshot();
    let ranks = snap.string_ranks();
    assert!(
        Arc::ptr_eq(&ranks, &snap.string_ranks()),
        "kept while the dictionary is unchanged"
    );
    let id = |t: Term| snap.reader().term_id(t.as_ref()).unwrap();
    let alpha = id(Literal::new_simple_literal("Alpha").into());
    let beta = id(Literal::new_simple_literal("Beta").into());
    let gamma = id(Literal::new_simple_literal("Gamma").into());
    let (a, b, c) = (
        ranks.rank(alpha).unwrap(),
        ranks.rank(beta).unwrap(),
        ranks.rank(gamma).unwrap(),
    );
    assert!(
        ranks
            .rank(id(Literal::new_simple_literal("").into()))
            .unwrap()
            < a
            && a < b
            && b < c
    );
    let names = snap.string_prefix(&ranks, Kind::String, "Be");
    assert_eq!(names.len(), 1);
    assert_eq!(ranks.id(Kind::String, names.start), Some(beta));
    let iris = snap.string_prefix(&ranks, Kind::Iri, "http://example.com/s");
    assert_eq!(iris.len(), 3, "ex:s1, ex:s2, ex:s3");
    // Language strings are ranked by value, then language.
    assert!(
        ranks
            .rank(id(Literal::new_language_tagged_literal_unchecked(
                "alfa", "nb"
            )
            .into()))
            .is_some()
    );
    // Many new terms: the next call ranks them too.
    let p = oxigraph::model::NamedNode::new_unchecked("http://example.com/p");
    let s = oxigraph::model::NamedNode::new_unchecked("http://example.com/a");
    for i in 0..100 {
        store
            .insert(&Quad::new(
                s.clone(),
                p.clone(),
                Literal::new_simple_literal(format!("new {i}")),
                oxigraph::model::GraphName::DefaultGraph,
            ))
            .unwrap();
    }
    let snap = store.snapshot();
    let again = snap.string_ranks();
    assert!(!Arc::ptr_eq(&ranks, &again));
    let new = snap
        .reader()
        .term_id(Literal::new_simple_literal("new 7").as_ref().into())
        .unwrap();
    assert_eq!(ranks.rank(new), None);
    assert!(again.rank(new).is_some());
}

#[test]
fn a_store_is_written_by_one_process_at_a_time() {
    use tripletrail_store::StoreError;
    let dir = temp_dir("lock");
    let in_use = |r: Result<Store, StoreError>| match r {
        Err(StoreError::Locked(d)) => assert_eq!(d, dir.display().to_string()),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("opened a store in use"),
    };
    let writer = Store::open(&dir).unwrap();
    // A second writer, or a reader, while it is open (#296).
    in_use(Store::open(&dir));
    in_use(Store::open_read_only(&dir));
    drop(writer);
    // Readers share it; a writer must wait for them.
    let a = Store::open_read_only(&dir).unwrap();
    let b = Store::open_read_only(&dir).unwrap();
    in_use(Store::open(&dir));
    drop((a, b));
    drop(Store::open(&dir).unwrap());
}

#[test]
fn insert_all_skips_quads_already_there_or_given_twice() {
    let store = Store::new();
    let ex = |s: &str| oxigraph::model::NamedNode::new_unchecked(format!("http://example.com/{s}"));
    let q =
        |s: &str, o: i64| Quad::new(ex(s), ex("p"), Literal::from(o), GraphNameRef::DefaultGraph);
    store.insert(&q("a", 1)).unwrap();
    let (added, gone) = store
        .transaction(|t| {
            // Removed and given again in the same transaction: back in.
            let gone = t.remove(&q("a", 1));
            let added = t.insert_all(vec![q("a", 1), q("b", 2), q("b", 2), q("c", 3)]);
            Ok::<_, tripletrail_store::StoreError>((added, gone))
        })
        .unwrap();
    assert!(gone);
    assert_eq!(added, 3);
    assert_eq!(store.len(), 3);
    let added = store
        .transaction(|t| {
            Ok::<_, tripletrail_store::StoreError>(t.insert_all(vec![q("c", 3), q("d", 4)]))
        })
        .unwrap();
    assert_eq!(added, 1);
    assert_eq!(store.len(), 4);
}

#[test]
fn checkpoints_write_only_the_new_dictionary_keys() {
    let dir = temp_dir("dict-pieces");
    let quad = |i: usize| {
        Quad::new(
            NamedNodeRef::new_unchecked(&format!("http://example.com/s{i}")).into_owned(),
            NamedNodeRef::new_unchecked("http://example.com/p").into_owned(),
            Literal::new_simple_literal(format!("value {i}")),
            GraphNameRef::DefaultGraph,
        )
    };
    let current = |dir: &Path| {
        let n = std::fs::read_to_string(dir.join("CURRENT")).unwrap();
        dir.join(format!("checkpoint-{}", n.trim()))
    };
    let mut first_dict = None;
    let mut store = None;
    for round in 0..12 {
        // Reopened halfway: the pieces are then read from the checkpoint.
        if round % 6 == 0 {
            store = Some(Store::open(&dir).unwrap());
        }
        let writer = store.as_ref().unwrap();
        for i in round * 10..round * 10 + 10 {
            writer.insert(&quad(i)).unwrap();
        }
        writer.checkpoint().unwrap();
        let cp = current(&dir);
        let pieces = (1..)
            .take_while(|n| cp.join(format!("dict-{n}.bin")).exists())
            .count();
        // The first checkpoint writes dict.bin, the next seven add a piece each, and the
        // ninth writes the dictionary whole again.
        assert_eq!(
            pieces,
            [0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3][round],
            "round {round}"
        );
        // Each piece has its saved index.
        assert!(cp.join("dict.idx").exists(), "round {round}");
        for n in 1..=pieces {
            assert!(cp.join(format!("dict-{n}.idx")).exists(), "round {round}");
        }
        let dict = std::fs::read(cp.join("dict.bin")).unwrap();
        if round == 0 || round == 8 {
            first_dict = Some(dict);
        } else {
            assert_eq!(Some(dict), first_dict, "dict.bin is kept as it was");
        }
        if round % 6 == 5 {
            store = None;
            let reader = Store::open_read_only(&dir).unwrap();
            assert_eq!(reader.len(), (round + 1) * 10);
            for i in 0..(round + 1) * 10 {
                assert!(reader.contains(&quad(i)), "round {round}, quad {i}");
            }
        }
    }
}

#[test]
fn writable_stores_map_their_checkpoint() {
    let dir = temp_dir("writable-mapped");
    let quad = |i: usize| {
        Quad::new(
            NamedNodeRef::new_unchecked(&format!("http://example.com/s{i}")).into_owned(),
            NamedNodeRef::new_unchecked("http://example.com/p").into_owned(),
            Literal::new_simple_literal(format!("value {i}")),
            GraphNameRef::DefaultGraph,
        )
    };
    {
        let store = Store::open(&dir).unwrap();
        store.bulk_load_quads((0..5000).map(quad)).unwrap();
        store.checkpoint().unwrap();
        // The checkpoint just written replaces the copies on the heap.
        assert!(store.mapped_size() > 0);
        let heap = store.memory_use();
        // Only the dictionary's hash table is left on the heap.
        assert!(heap.dictionary < 200 * 1024, "{}", heap.dictionary);
        assert_eq!(heap.quads, 0);
        // New terms after the checkpoint, and their own checkpoint.
        for i in 5000..5100 {
            store.insert(&quad(i)).unwrap();
        }
        assert!(store.contains(&quad(42)) && store.contains(&quad(5050)));
        store.checkpoint().unwrap();
        for i in 5100..5200 {
            store.insert(&quad(i)).unwrap();
        }
    }
    let store = Store::open(&dir).unwrap();
    assert!(store.mapped_size() > 0);
    assert_eq!(store.len(), 5200);
    for i in (0..5200).step_by(13) {
        assert!(store.contains(&quad(i)), "quad {i}");
    }
    store.insert(&quad(9999)).unwrap();
    assert!(store.contains(&quad(9999)));
}

#[test]
fn folds_of_a_store_on_disk_write_their_segments_to_files() {
    let dir = temp_dir("fold-files");
    let quad = |i: usize| {
        Quad::new(
            NamedNodeRef::new_unchecked(&format!("http://example.com/s{}", i / 4)).into_owned(),
            NamedNodeRef::new_unchecked(&format!("http://example.com/p{}", i % 4)).into_owned(),
            Literal::from(i as i64),
            GraphNameRef::DefaultGraph,
        )
    };
    let runs = |dir: &Path| std::fs::read_dir(dir.join("runs")).unwrap().count();
    {
        let store = Store::open(&dir).unwrap();
        store.bulk_load_quads((0..20_000).map(quad)).unwrap();
        // A transaction larger than the delta is folded into the segments at its commit.
        store
            .transaction(|t| {
                for i in 20_000..100_000 {
                    t.insert(&quad(i));
                }
                Ok::<_, tripletrail_store::StoreError>(())
            })
            .unwrap();
        assert_eq!(store.memory_use().quads, 0, "the segments are files");
        assert_eq!(runs(&dir), 8);
        assert_eq!(store.len(), 100_000);
        store.checkpoint().unwrap();
        // The checkpoint links the files, and the store then reads them from there.
        assert_eq!(runs(&dir), 0);
        assert_eq!(store.memory_use().quads, 0);
        for i in (0..100_000).step_by(997) {
            assert!(store.remove(&quad(i)).unwrap());
        }
    }
    let store = Store::open(&dir).unwrap();
    assert_eq!(store.len(), 100_000 - 101);
    assert!(store.contains(&quad(1)) && !store.contains(&quad(997)));
    drop(store);
    let reader = Store::open_read_only(&dir).unwrap();
    assert_eq!(reader.len(), 100_000 - 101);
    drop(reader);
}

/// Rewrites a format 4 segment file in format 3, as checkpoints wrote them before #218:
/// the magic number, then the sizes, the block index and the blocks.
fn segment_as_format_3(v4: &[u8]) -> Vec<u8> {
    assert_eq!(&v4[..8], b"OXGSSEG4");
    let u64_at = |at: usize| u64::from_le_bytes(v4[at..at + 8].try_into().unwrap());
    let trailer = v4.len() - 40;
    let [len, firsts, offsets, starts, bytes] = std::array::from_fn(|i| u64_at(trailer + 8 * i));
    let blocks = &v4[8..8 + bytes as usize];
    let at = (8 + bytes as usize).next_multiple_of(8);
    let index = &v4[at..at + 8 * (4 * firsts + offsets + starts) as usize];
    assert_eq!(at + index.len(), trailer);
    let mut v3 = b"OXGSSEG3".to_vec();
    for h in [len, firsts, offsets, starts, bytes] {
        v3.extend_from_slice(&h.to_le_bytes());
    }
    v3.extend_from_slice(index);
    v3.extend_from_slice(blocks);
    v3
}

#[test]
fn checkpoints_with_segments_in_format_3_open_and_are_rewritten() {
    let dir = temp_dir("segment-format-3");
    let expected = {
        let store = Store::open(&dir).unwrap();
        store.bulk_load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
        store.checkpoint().unwrap();
        sorted(&store)
    };
    let checkpoints = || -> Vec<PathBuf> {
        std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("checkpoint-"))
            .map(|e| e.path())
            .collect()
    };
    let [checkpoint] = checkpoints().try_into().unwrap();
    let sums = std::fs::read_to_string(checkpoint.join("checksums")).unwrap();
    let mut rewritten = String::new();
    for line in sums.lines() {
        let name = line.split(' ').next().unwrap();
        if !name.ends_with(".seg") {
            rewritten += &format!("{line}\n");
            continue;
        }
        let path = checkpoint.join(name);
        // Graph-first segments read the others' blocks (#152): they stay as they are.
        if std::fs::read(&path).unwrap().starts_with(b"OXGSSEG5") {
            rewritten += &format!("{line}\n");
            continue;
        }
        let v3 = segment_as_format_3(&std::fs::read(&path).unwrap());
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, &v3).unwrap();
        rewritten += &format!("{name} {} {:08x}\n", v3.len(), crc32fast::hash(&v3));
    }
    std::fs::write(checkpoint.join("checksums"), rewritten).unwrap();
    let format = |path: &std::path::Path| std::fs::read(path).unwrap()[..8].to_vec();
    assert_eq!(format(&checkpoint.join("spog.seg")), b"OXGSSEG3");
    for read_only in [true, false] {
        let store = if read_only {
            Store::open_read_only(&dir).unwrap()
        } else {
            Store::open(&dir).unwrap()
        };
        assert_eq!(sorted(&store), expected);
    }
    // The next checkpoint writes them in format 4.
    let store = Store::open(&dir).unwrap();
    store
        .insert(QuadRef::new(
            NamedNodeRef::new("http://example.com/new").unwrap(),
            NamedNodeRef::new("http://example.com/knows").unwrap(),
            NamedNodeRef::new("http://example.com/alice").unwrap(),
            GraphNameRef::DefaultGraph,
        ))
        .unwrap();
    store.checkpoint().unwrap();
    let expected = sorted(&store);
    drop(store);
    let [checkpoint] = checkpoints().try_into().unwrap();
    for e in std::fs::read_dir(&checkpoint).unwrap().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.ends_with(".seg") {
            // Without named graphs, those with the graph first read the others' blocks.
            let want: &[u8] = if name.starts_with('g') && name.len() == "gspo.seg".len() {
                b"OXGSSEG5"
            } else {
                b"OXGSSEG4"
            };
            assert_eq!(format(&e.path()), want, "{name}");
        }
    }
    assert_eq!(sorted(&Store::open(&dir).unwrap()), expected);
}

#[test]
fn compactions_write_runs_that_checkpoints_save() {
    // Runs on a small store (for every test of this process: they read the same).
    Store::set_runs_from(0);
    let dir = temp_dir("runs");
    let quad = |i: usize| {
        Quad::new(
            NamedNodeRef::new_unchecked(&format!("http://example.com/s{}", i / 4)).into_owned(),
            NamedNodeRef::new_unchecked(&format!("http://example.com/p{}", i % 4)).into_owned(),
            Literal::from(i as i64),
            GraphNameRef::DefaultGraph,
        )
    };
    let checkpoint_files = |dir: &Path| -> Vec<String> {
        let checkpoint = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("checkpoint-")
            })
            .max()
            .unwrap();
        std::fs::read_dir(checkpoint)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()
    };
    let model = Store::new();
    let expected = {
        let store = Store::open(&dir).unwrap();
        // The runs stay until a compaction folds them in.
        store.set_background_merges(false);
        for s in [&store, &model] {
            s.bulk_load_quads((0..200_000).map(quad)).unwrap();
        }
        // Compacted at the checkpoint into a run of each permutation: new quads and removed
        // ones of the segment.
        for s in [&store, &model] {
            for i in 200_000..205_000 {
                s.insert(&quad(i)).unwrap();
            }
            for i in (0..200_000).step_by(211) {
                s.remove(&quad(i)).unwrap();
            }
        }
        store.checkpoint().unwrap();
        let files = checkpoint_files(&dir);
        assert!(files.contains(&"spog.0a.seg".to_owned()), "{files:?}");
        assert!(files.contains(&"spog.0r.seg".to_owned()), "{files:?}");
        // Then changes that undo some of the run's, and more.
        for s in [&store, &model] {
            for i in (200_000..205_000).step_by(7) {
                s.remove(&quad(i)).unwrap();
            }
            for i in (0..200_000).step_by(211 * 3) {
                s.insert(&quad(i)).unwrap();
            }
            for i in 205_000..206_000 {
                s.insert(&quad(i)).unwrap();
            }
        }
        store.checkpoint().unwrap();
        assert!(checkpoint_files(&dir).contains(&"gpso.0a.seg".to_owned()));
        assert_eq!(store.len(), model.len());
        assert_eq!(sorted(&store), sorted(&model));
        sorted(&model)
    };
    for read_only in [false, true] {
        let store = if read_only {
            Store::open_read_only(&dir).unwrap()
        } else {
            Store::open(&dir).unwrap()
        };
        assert_eq!(store.len(), model.len());
        assert_eq!(sorted(&store), expected);
        assert!(store.contains(&quad(211 * 3)) && !store.contains(&quad(211)));
        assert!(store.contains(&quad(205_500)) && !store.contains(&quad(200_007)));
    }
}

#[test]
fn runs_merge_into_the_segments_in_the_background() {
    // Runs on a small store (for every test of this process: they read the same).
    Store::set_runs_from(0);
    let dir = temp_dir("background-merge");
    let quad = |i: usize| {
        Quad::new(
            NamedNodeRef::new_unchecked(&format!("http://example.com/s{}", i / 4)).into_owned(),
            NamedNodeRef::new_unchecked(&format!("http://example.com/p{}", i % 4)).into_owned(),
            Literal::from(i as i64),
            GraphNameRef::DefaultGraph,
        )
    };
    let has_runs = |dir: &Path| {
        let checkpoint = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("checkpoint-")
            })
            .max()
            .unwrap();
        std::fs::read_dir(checkpoint)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains(".0a."))
    };
    let model = Store::new();
    let store = Store::open(&dir).unwrap();
    for s in [&store, &model] {
        s.bulk_load_quads((0..200_000).map(quad)).unwrap();
    }
    // Checkpoints compact the writes into runs, which then merge in the background while
    // writes and reads go on.
    let write = |round: usize| {
        for s in [&store, &model] {
            s.transaction(|t| {
                for i in 0..5_000 {
                    t.insert(&quad(200_000 + round * 5_000 + i));
                }
                for i in (round * 1000..round * 1000 + 500).step_by(3) {
                    t.remove(&quad(i));
                }
                Ok::<_, tripletrail_store::StoreError>(())
            })
            .unwrap();
        }
    };
    for round in 0..6 {
        write(round);
        store.checkpoint().unwrap();
        if round == 0 {
            assert!(has_runs(&dir), "compacted into runs");
        }
        assert_eq!(store.len(), model.len(), "round {round}");
        assert_eq!(sorted(&store), sorted(&model), "round {round}");
    }
    store.wait_for_merges();
    store.checkpoint().unwrap();
    store.set_background_merges(false);
    assert!(!has_runs(&dir), "merged into the segments");
    assert_eq!(sorted(&store), sorted(&model));
    // A store dropped while it merges can be opened again at once.
    store.set_background_merges(true);
    write(6);
    store.checkpoint().unwrap();
    drop(store);
    let store = Store::open(&dir).unwrap();
    assert_eq!(store.len(), model.len());
    assert_eq!(sorted(&store), sorted(&model));
    drop(store);
}

#[test]
fn runs_too_small_to_merge_are_merged_once_the_store_is_idle() {
    // Runs on a small store (for every test of this process: they read the same).
    Store::set_runs_from(0);
    Store::set_idle_merge_after(std::time::Duration::from_millis(200));
    let dir = temp_dir("idle-merge");
    let quad = |i: usize| {
        Quad::new(
            NamedNodeRef::new_unchecked(&format!("http://example.com/s{}", i / 4)).into_owned(),
            NamedNodeRef::new_unchecked(&format!("http://example.com/p{}", i % 4)).into_owned(),
            Literal::from(i as i64),
            GraphNameRef::DefaultGraph,
        )
    };
    let has_runs = |dir: &Path| {
        let checkpoint = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("checkpoint-")
            })
            .max()
            .unwrap();
        std::fs::read_dir(checkpoint)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains(".0a."))
    };
    let model = Store::new();
    let store = Store::open(&dir).unwrap();
    for s in [&store, &model] {
        s.bulk_load_quads((0..200_000).map(quad)).unwrap();
    }
    // 1,000 quads into 200,000: under one in 64, so no merge right away.
    let write = |from: usize| {
        for s in [&store, &model] {
            s.transaction(|t| {
                for i in from..from + 1_000 {
                    t.insert(&quad(i));
                }
                Ok::<_, tripletrail_store::StoreError>(())
            })
            .unwrap();
        }
    };
    write(200_000);
    store.checkpoint().unwrap();
    assert!(has_runs(&dir), "compacted into runs");
    assert!(!store.is_merging());
    // Writes more often than the idle time keep the runs.
    for round in 1..5 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        write(200_000 + round * 1_000);
        store.checkpoint().unwrap();
    }
    assert!(has_runs(&dir), "no idle merge while writes come");
    // Then idle: the runs merge into the segments.
    let start = std::time::Instant::now();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
        store.wait_for_merges();
        store.checkpoint().unwrap();
        if !has_runs(&dir) {
            break;
        }
        assert!(
            start.elapsed().as_secs() < 30,
            "the runs were merged once idle"
        );
    }
    assert_eq!(store.len(), model.len());
    assert_eq!(sorted(&store), sorted(&model));
    // A write while an idle merge runs does not stop it: a store written now and then would
    // otherwise start the rewrite again and again without end.
    for round in 5..8 {
        write(200_000 + round * 1_000);
        store.checkpoint().unwrap();
        assert!(store.run_count() > 0, "compacted into runs");
        let start = std::time::Instant::now();
        while !store.is_merging() {
            std::thread::sleep(std::time::Duration::from_millis(1));
            assert!(start.elapsed().as_secs() < 30, "an idle merge starts");
        }
        write(300_000 + round * 1_000);
        store.wait_for_merges();
        assert_eq!(
            store.run_count(),
            0,
            "the idle merge finished, round {round}"
        );
    }
    assert_eq!(sorted(&store), sorted(&model));
    drop(store);
    let store = Store::open(&dir).unwrap();
    assert_eq!(sorted(&store), sorted(&model));
    drop(store);
}

#[cfg(unix)]
#[test]
fn checkpoints_link_the_segments_they_do_not_change() {
    use std::os::unix::fs::MetadataExt;
    // Runs on a small store (for every test of this process: they read the same).
    Store::set_runs_from(0);
    let dir = temp_dir("link-segments");
    let quad = |i: usize| {
        Quad::new(
            NamedNodeRef::new_unchecked(&format!("http://example.com/s{}", i / 4)).into_owned(),
            NamedNodeRef::new_unchecked(&format!("http://example.com/p{}", i % 4)).into_owned(),
            Literal::from(i as i64),
            GraphNameRef::DefaultGraph,
        )
    };
    let segment = |dir: &Path| {
        let checkpoint = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("checkpoint-")
            })
            .max()
            .unwrap();
        std::fs::metadata(checkpoint.join("spog.seg"))
            .unwrap()
            .ino()
    };
    let model = Store::new();
    for reopen in [false, true] {
        let store = Store::open(&dir).unwrap();
        // Runs stay runs: the segments are what the checkpoints keep.
        store.set_background_merges(false);
        if !reopen {
            for s in [&store, &model] {
                s.bulk_load_quads((0..50_000).map(quad)).unwrap();
            }
        }
        let before = segment(&dir);
        for (round, i) in [60_000, 60_001].into_iter().enumerate() {
            for s in [&store, &model] {
                s.insert(&quad(i + usize::from(reopen) * 10)).unwrap();
            }
            store.checkpoint().unwrap();
            // The same file, linked, rather than the segment written again.
            assert_eq!(segment(&dir), before, "reopened {reopen}, round {round}");
        }
        assert_eq!(sorted(&store), sorted(&model));
    }
    let store = Store::open(&dir).unwrap();
    assert_eq!(sorted(&store), sorted(&model));
    drop(store);
}

#[cfg(unix)]
#[test]
fn copies_of_a_store_share_no_file_with_it() {
    use std::os::unix::fs::MetadataExt;
    // Runs on a small store (for every test of this process: they read the same).
    Store::set_runs_from(0);
    let dir = temp_dir("copy-files");
    let quad = |i: usize| {
        Quad::new(
            NamedNodeRef::new_unchecked(&format!("http://example.com/s{}", i / 4)).into_owned(),
            NamedNodeRef::new_unchecked(&format!("http://example.com/p{}", i % 4)).into_owned(),
            Literal::from(i as i64),
            GraphNameRef::DefaultGraph,
        )
    };
    // The files under `dir` with their metadata. The store's background merge removes runs
    // and segments while the walk reads the folder (#51), so an entry that is gone by the time
    // its metadata is read, or a folder gone before it is listed, is left out.
    fn files(dir: &std::path::Path, out: &mut Vec<std::fs::Metadata>) {
        let gone = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if gone(&e) => return,
            Err(e) => panic!("{}: {e}", dir.display()),
        };
        for e in entries {
            let e = match e {
                Ok(e) => e,
                Err(e) if gone(&e) => continue,
                Err(e) => panic!("{}: {e}", dir.display()),
            };
            match e.metadata() {
                Ok(m) if m.is_dir() => files(&e.path(), out),
                Ok(m) => out.push(m),
                Err(err) if gone(&err) => {}
                Err(err) => panic!("{}: {err}", e.path().display()),
            }
        }
    }
    // The inodes of the files under `dir` with more than one name.
    fn linked(dir: &std::path::Path) -> std::collections::HashSet<u64> {
        let mut all = Vec::new();
        files(dir, &mut all);
        all.iter()
            .filter(|m| m.nlink() > 1)
            .map(|m| m.ino())
            .collect()
    }
    fn inodes(dir: &std::path::Path, out: &mut std::collections::HashSet<u64>) {
        let mut all = Vec::new();
        files(dir, &mut all);
        out.extend(all.iter().map(|m| m.ino()));
    }
    let model = Store::new();
    let copies = |store: &Store, label: &str| {
        let mut live = std::collections::HashSet::new();
        inodes(&dir, &mut live);
        for (i, base) in [false, true].into_iter().enumerate() {
            let copy = dir.with_extension(format!("{label}-{i}"));
            let _ = std::fs::remove_dir_all(&copy);
            if base {
                store.save_base(&copy).unwrap();
            } else {
                store.save_to(&copy).unwrap();
            }
            let mut copied = std::collections::HashSet::new();
            inodes(&copy, &mut copied);
            // A file shared with the store is a hard link: more than one name. (An inode number
            // alone is not enough: a background merge can delete a live file after `live` was
            // read, and the copy's new file reuse its number.)
            let shared: Vec<u64> = live.intersection(&copied).copied().collect();
            assert!(
                shared.iter().all(|i| !linked(&copy).contains(i)),
                "{label}, save_base {base}"
            );
            let opened = Store::open(&copy).unwrap();
            assert_eq!(sorted(&opened), sorted(&model), "{label}, save_base {base}");
            drop(opened);
            std::fs::remove_dir_all(&copy).unwrap();
        }
    };
    {
        let store = Store::open(&dir).unwrap();
        store.set_background_merges(false);
        for s in [&store, &model] {
            s.bulk_load_quads((0..50_000).map(quad)).unwrap();
        }
        // Runs a fold just wrote, in the store's files.
        store.checkpoint().unwrap();
        for s in [&store, &model] {
            s.insert(&quad(60_000)).unwrap();
        }
        store.checkpoint().unwrap();
        copies(&store, "runs");
    }
    // Segments read from a checkpoint.
    let store = Store::open(&dir).unwrap();
    copies(&store, "opened");
    // A damaged segment file is not copied under a new sum.
    let checkpoint = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("checkpoint-")
        })
        .max()
        .unwrap();
    drop(store);
    let path = checkpoint.join("gspo.seg");
    let mut bytes = std::fs::read(&path).unwrap();
    let at = bytes.len() / 2;
    bytes[at] ^= 0x55;
    std::fs::write(&path, &bytes).unwrap();
    // Opened without reading it (as a large store is): the copy finds the damage.
    Store::verify_in_background_above(0);
    let store = Store::open(&dir).unwrap();
    let copy = dir.with_extension("damaged");
    let _ = std::fs::remove_dir_all(&copy);
    let error = store.save_to(&copy).expect_err("the damage is found");
    assert!(error.to_string().contains("gspo.seg"), "{error}");
    drop(store);
    let _ = std::fs::remove_dir_all(&copy);
}

#[test]
fn bulk_loads_larger_than_the_memory_limit_sort_on_disk() {
    let dir = temp_dir("external-sort");
    let quad = |i: u64| {
        Quad::new(
            NamedNodeRef::new_unchecked(&format!("http://example.com/s{}", i % 997)).into_owned(),
            NamedNodeRef::new_unchecked(&format!("http://example.com/p{}", i % 13)).into_owned(),
            Literal::new_simple_literal(format!("v{}", i % 20_011)),
            if i.is_multiple_of(3) {
                oxigraph::model::GraphName::DefaultGraph
            } else {
                NamedNodeRef::new_unchecked(&format!("http://example.com/g{}", i % 5))
                    .into_owned()
                    .into()
            },
        )
    };
    let first: Vec<Quad> = (0..30_000u64).map(|i| quad(i * 7)).collect();
    let second: Vec<Quad> = (0..20_000u64).map(|i| quad(i * 11 + 3)).collect();
    // A new term in every other quad, so the dictionary grows past the limit too. Read as
    // N-Quads text, the known terms are found in the mapped keys first (#232), and each new
    // term comes twice in its batch: not found, then added once.
    let third: Vec<Quad> = (0..20_000u64)
        .map(|i| {
            let mut q = quad(i * 13);
            q.object = Literal::new_simple_literal(format!("new value {}", i / 2)).into();
            q
        })
        .collect();
    let third_text: String = third.iter().map(|q| format!("{q} .\n")).collect();
    let reference = Store::new();
    reference.bulk_load_quads(first.iter().cloned()).unwrap();
    reference.bulk_load_quads(second.iter().cloned()).unwrap();
    reference.bulk_load_quads(third).unwrap();
    // 64 KB: chunks of 4096 quads, so each load is sorted in several runs, and new
    // dictionary keys go to mapped files every 8 KB.
    Store::set_memory_limit(64 << 10);
    let store = Store::open(&dir).unwrap();
    store.bulk_load_quads(first).unwrap();
    store
        .bulk_load_quad_batches(second.chunks(5000).map(<[Quad]>::to_vec))
        .unwrap();
    store
        .bulk_load(RdfFormat::NQuads, third_text.as_bytes())
        .unwrap();
    Store::set_memory_limit(usize::MAX);
    let check = |store: &Store| {
        assert_eq!(store.len(), reference.len());
        assert_eq!(sorted(store), sorted(&reference));
        let p = NamedNodeRef::new_unchecked("http://example.com/p4");
        let s = NamedNodeRef::new_unchecked("http://example.com/s42");
        let o = Literal::new_simple_literal("v77");
        let g = NamedNodeRef::new_unchecked("http://example.com/g2");
        let count = |store: &Store,
                     s: Option<NamedNodeRef<'_>>,
                     p: Option<NamedNodeRef<'_>>,
                     o: Option<&Literal>,
                     g: Option<GraphNameRef<'_>>| {
            store
                .quads_for_pattern(s.map(Into::into), p, o.map(Into::into), g)
                .count()
        };
        for (s, p, o, g) in [
            (Some(s), None, None, None),
            (None, Some(p), None, None),
            (None, None, Some(&o), None),
            (None, None, None, Some(g.into())),
            (Some(s), Some(p), None, Some(g.into())),
            (None, Some(p), Some(&o), None),
            (None, None, None, Some(GraphNameRef::DefaultGraph)),
        ] {
            assert_eq!(count(store, s, p, o, g), count(&reference, s, p, o, g));
        }
    };
    check(&store);
    drop(store);
    let store = Store::open(&dir).unwrap();
    check(&store);
    drop(store);
}

#[test]
fn saved_dictionary_indexes_are_checked_and_written_again() {
    let dir = temp_dir("dict-index");
    let quad = |i: usize| {
        Quad::new(
            NamedNodeRef::new_unchecked(&format!("http://example.com/s{i}")).into_owned(),
            NamedNodeRef::new_unchecked("http://example.com/p").into_owned(),
            Literal::new_simple_literal(format!("value {i}")),
            GraphNameRef::DefaultGraph,
        )
    };
    let current = |dir: &Path| {
        let n = std::fs::read_to_string(dir.join("CURRENT")).unwrap();
        dir.join(format!("checkpoint-{}", n.trim()))
    };
    {
        let store = Store::open(&dir).unwrap();
        store.bulk_load_quads((0..5000).map(quad)).unwrap();
        for i in 5000..5010 {
            store.insert(&quad(i)).unwrap();
        }
        store.checkpoint().unwrap();
    }
    let cp = current(&dir);
    let sums = std::fs::read_to_string(cp.join("checksums")).unwrap();
    assert!(
        sums.contains("dict.idx ") && sums.contains("dict-1.idx "),
        "{sums}"
    );
    // Terms already in the dictionary are found through the saved indexes: inserting their
    // quads again adds nothing.
    let check = |store: &Store, n: usize| {
        for i in (0..n).step_by(97) {
            assert!(!store.insert(&quad(i)).unwrap(), "quad {i}");
            assert!(store.contains(&quad(i)));
        }
        assert_eq!(store.len(), n);
    };
    check(&Store::open(&dir).unwrap(), 5010);
    // A damaged index is a cache: not used, and written again by the next checkpoint.
    let path = cp.join("dict.idx");
    let mut bytes = std::fs::read(&path).unwrap();
    let at = bytes.len() / 2;
    bytes[at] ^= 0x55;
    std::fs::write(&path, bytes).unwrap();
    std::fs::remove_file(cp.join("dict-1.idx")).unwrap();
    {
        let store = Store::open(&dir).unwrap();
        check(&store, 5010);
        store.insert(&quad(5010)).unwrap();
        store.checkpoint().unwrap();
    }
    let cp = current(&dir);
    let sums = std::fs::read_to_string(cp.join("checksums")).unwrap();
    assert!(
        sums.contains("dict.idx ") && sums.contains("dict-1.idx ") && sums.contains("dict-2.idx "),
        "{sums}"
    );
    check(&Store::open(&dir).unwrap(), 5011);
    let reader = Store::open_read_only(&dir).unwrap();
    assert!((0..5011).all(|i| reader.contains(&quad(i))));
    assert!(!reader.contains(&quad(5011)));
}

#[test]
fn a_closed_store_stops_its_background_merge() {
    // Runs on a small store (for every test of this process: they read the same).
    Store::set_runs_from(0);
    let quad = |i: usize| {
        Quad::new(
            NamedNodeRef::new_unchecked(&format!("http://example.com/s{}", i / 4)).into_owned(),
            NamedNodeRef::new_unchecked(&format!("http://example.com/p{}", i % 4)).into_owned(),
            Literal::from(i as i64),
            GraphNameRef::DefaultGraph,
        )
    };
    for round in 0..2 {
        let dir = temp_dir(&format!("merge-on-close-{round}"));
        let store = Store::open(&dir).unwrap();
        store.bulk_load_quads((0..1_000_000).map(quad)).unwrap();
        // Each checkpoint writes runs, which merges then fold in the background: the store
        // is dropped while one may still run.
        for c in 0..6 {
            for i in 1_000_000 + c * 2000..1_002_000 + c * 2000 {
                store.insert(&quad(i)).unwrap();
            }
            store.checkpoint().unwrap();
        }
        drop(store);
        // Nothing writes in the directory any more, and the merge left no files.
        let left: Vec<_> = std::fs::read_dir(dir.join("runs"))
            .map(|d| d.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "round {round}: {left:?}");
    }
}

#[test]
fn geometries_are_saved_per_dictionary_piece() {
    let dir = temp_dir("geoms-pieces");
    let wkt = |i: usize| {
        Literal::new_typed_literal(
            format!("POLYGON(({i} 0, {i}.5 0, {i}.5 1, {i} 0))"),
            NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#wktLiteral"),
        )
    };
    let insert = |store: &Store, i: usize| {
        store
            .insert(QuadRef::new(
                NamedNodeRef::new_unchecked(&format!("http://example.com/g{i}")),
                NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#asWKT"),
                &wkt(i),
                GraphNameRef::DefaultGraph,
            ))
            .unwrap();
    };
    let current = |dir: &Path| {
        let n = std::fs::read_to_string(dir.join("CURRENT")).unwrap();
        dir.join(format!("checkpoint-{}", n.trim()))
    };
    {
        let store = Store::open(&dir).unwrap();
        for i in 0..50 {
            insert(&store, i);
        }
        store.checkpoint().unwrap();
        let first = std::fs::read(current(&dir).join("geoms.bin")).unwrap();
        // The next checkpoint encodes only the geometries added since, next to the new
        // dictionary piece, and keeps the first file as it was.
        for i in 50..80 {
            insert(&store, i);
        }
        store.checkpoint().unwrap();
        let cp = current(&dir);
        assert_eq!(std::fs::read(cp.join("geoms.bin")).unwrap(), first);
        assert!(cp.join("geoms-1.bin").exists());
    }
    // Reopened, every geometry's bounding box is read from the saved records: none is parsed.
    let store = Store::open(&dir).unwrap();
    for i in 0..80 {
        let id = store.term_id(wkt(i).as_ref().into()).unwrap();
        let (_, bbox) = store.geometry_bbox(id).unwrap();
        assert_eq!(
            bbox,
            Some([i as f64, 0.0, i as f64 + 0.5, 1.0]),
            "geometry {i}"
        );
    }
    assert_eq!(store.memory_use().geometries, 0);
    let g = store.geometry(wkt(70).as_ref().into()).unwrap();
    assert_eq!(
        geo::BoundingRect::bounding_rect(&g.geometry).map(|r| r.min().x),
        Some(70.0)
    );
    drop(store);
}

/// A quad `<http://example.com/{s}> <http://example.com/p> <http://example.com/o>`.
fn ex_quad(s: &str) -> Quad {
    let n = |x: &str| NamedNodeRef::new_unchecked(x).into_owned();
    Quad::new(
        n(&format!("http://example.com/{s}")),
        n("http://example.com/p"),
        n("http://example.com/o"),
        oxigraph::model::GraphName::DefaultGraph,
    )
}

#[test]
fn point_in_time_restore_from_a_base_backup_and_archived_logs() {
    let dir = temp_dir("pitr");
    let (live, archive, base) = (dir.join("live"), dir.join("archive"), dir.join("base"));
    let store = Store::open(&live).unwrap();
    store.set_wal_archive(Some(archive.clone())).unwrap();
    store.insert(&ex_quad("before")).unwrap();
    store.save_base(&base).unwrap();
    assert!(base.join("BASE").is_file());
    // After the base: one transaction in the log the base copied part of, one in the next.
    store.insert(&ex_quad("t1")).unwrap();
    store.checkpoint().unwrap();
    store.insert(&ex_quad("t2")).unwrap();
    store.checkpoint().unwrap();
    let timeline = std::fs::read_to_string(live.join("TIMELINE")).unwrap();
    let archived = archive.join(timeline.trim());
    assert!(archived.join("wal-0.log").is_file() && archived.join("wal-1.log").is_file());
    store.insert(&ex_quad("t3")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let before_drop = std::time::SystemTime::now();
    std::thread::sleep(std::time::Duration::from_millis(20));
    store.clear().unwrap();
    assert!(store.is_empty());

    // To just before the accident: everything up to t3, read from the archive and the live
    // store's current log.
    let logs = [archive.clone(), live.clone()];
    let restored = Store::restore(&base, &logs, Some(before_drop), dir.join("r1")).unwrap();
    assert_eq!(restored.transactions, 3, "{restored:?}");
    assert_eq!(restored.quads, 4);
    let r1 = Store::open(dir.join("r1")).unwrap();
    for s in ["before", "t1", "t2", "t3"] {
        assert!(r1.contains(&ex_quad(s)), "{s}");
    }
    drop(r1);
    // To the latest state: the clear too.
    let restored = Store::restore(&base, &logs, None, dir.join("r2")).unwrap();
    assert_eq!(restored.quads, 0, "{restored:?}");
    // A target that exists, or a directory that is no base backup, is refused.
    assert!(Store::restore(&base, &logs, None, dir.join("r2")).is_err());
    assert!(Store::restore(&live, &logs, None, dir.join("r3")).is_err());
    drop(store);
}

#[test]
fn a_log_that_failed_to_archive_is_archived_after_a_reopen() {
    let dir = temp_dir("pitr-reopen");
    let (live, archive) = (dir.join("live"), dir.join("archive"));
    let store = Store::open(&live).unwrap();
    store.set_wal_archive(Some(archive.clone())).unwrap();
    store.insert(&ex_quad("t1")).unwrap();
    let timeline = std::fs::read_to_string(live.join("TIMELINE")).unwrap();
    let archive_dir = archive.join(timeline.trim());
    // A directory where the log should go: archiving it fails, so it stays in the store.
    std::fs::create_dir_all(archive_dir.join("wal-0.log")).unwrap();
    store.checkpoint().unwrap();
    assert!(live.join("wal-0.log").is_file());
    drop(store);
    std::fs::remove_dir(archive_dir.join("wal-0.log")).unwrap();
    // Opening keeps it; the next checkpoint with the archive set archives it.
    let store = Store::open(&live).unwrap();
    assert!(live.join("wal-0.log").is_file());
    store.set_wal_archive(Some(archive.clone())).unwrap();
    store.insert(&ex_quad("t2")).unwrap();
    store.checkpoint().unwrap();
    assert!(archive_dir.join("wal-0.log").is_file() && archive_dir.join("wal-1.log").is_file());
    assert!(!live.join("wal-0.log").exists() && !live.join("wal-1.log").exists());
    drop(store);
}

#[test]
fn a_restore_fails_on_a_gap_in_the_logs() {
    let dir = temp_dir("pitr-gap");
    let (live, archive, base) = (dir.join("live"), dir.join("archive"), dir.join("base"));
    let store = Store::open(&live).unwrap();
    store.set_wal_archive(Some(archive.clone())).unwrap();
    store.save_base(&base).unwrap();
    for s in ["t1", "t2"] {
        store.insert(&ex_quad(s)).unwrap();
        store.checkpoint().unwrap();
    }
    let timeline = std::fs::read_to_string(live.join("TIMELINE")).unwrap();
    std::fs::remove_file(archive.join(timeline.trim()).join("wal-0.log")).unwrap();
    let logs = [archive.clone(), live.clone()];
    let err = Store::restore(&base, &logs, None, dir.join("r1")).unwrap_err();
    assert!(err.to_string().contains("gap"), "{err}");
    assert!(!dir.join("r1").exists());
    drop(store);
}

#[test]
fn incarnations_of_a_store_keep_their_logs_apart() {
    let dir = temp_dir("pitr-timelines");
    let (live, archive, base) = (dir.join("live"), dir.join("archive"), dir.join("base"));
    let store = Store::open(&live).unwrap();
    store.set_wal_archive(Some(archive.clone())).unwrap();
    store.save_base(&base).unwrap();
    store.insert(&ex_quad("old")).unwrap();
    store.checkpoint().unwrap();
    drop(store);
    // The dataset deleted and created again under the same name, with an archive of its own
    // logs of the same generations.
    std::fs::remove_dir_all(&live).unwrap();
    let store = Store::open(&live).unwrap();
    store.set_wal_archive(Some(archive.clone())).unwrap();
    store.insert(&ex_quad("new1")).unwrap();
    store.checkpoint().unwrap();
    store.insert(&ex_quad("new2")).unwrap();
    // The old base replays only its own timeline's log: neither the new archive nor the new
    // live store's log.
    let logs = [archive.clone(), live.clone()];
    let restored = Store::restore(&base, &logs, None, dir.join("r1")).unwrap();
    assert_eq!(restored.quads, 1, "{restored:?}");
    let r1 = Store::open(dir.join("r1")).unwrap();
    assert!(r1.contains(&ex_quad("old")) && !r1.contains(&ex_quad("new1")));
    // The restored store is a timeline of its own.
    assert_ne!(
        std::fs::read_to_string(dir.join("r1").join("TIMELINE")).unwrap(),
        std::fs::read_to_string(live.join("TIMELINE")).unwrap()
    );
    drop(r1);
    // A time before the base was taken is refused.
    let err = Store::restore(
        &base,
        &logs,
        Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(60)),
        dir.join("r2"),
    )
    .unwrap_err();
    assert!(err.to_string().contains("older base"), "{err}");
    drop(store);
}

#[test]
fn pruning_the_archive_keeps_the_logs_the_bases_need() {
    let dir = temp_dir("pitr-prune");
    let (live, archive) = (dir.join("live"), dir.join("archive"));
    let (base1, base2) = (dir.join("base1"), dir.join("base2"));
    let store = Store::open(&live).unwrap();
    store.set_wal_archive(Some(archive.clone())).unwrap();
    let timeline = std::fs::read_to_string(live.join("TIMELINE")).unwrap();
    let logs_dir = archive.join(timeline.trim());
    store.save_base(&base1).unwrap();
    for s in ["t1", "t2"] {
        store.insert(&ex_quad(s)).unwrap();
        store.checkpoint().unwrap();
    }
    // The second base starts in generation 2; its log is archived by the next checkpoint.
    store.save_base(&base2).unwrap();
    store.insert(&ex_quad("t3")).unwrap();
    store.checkpoint().unwrap();
    let archived = |n: u64| logs_dir.join(format!("wal-{n}.log")).is_file();
    assert!(archived(0) && archived(1) && archived(2));
    // A timeline no base belongs to (an earlier incarnation of the dataset).
    let other = archive.join("0123456789abcdef");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("wal-1.log"), "").unwrap();

    // With both bases kept, nothing of this timeline goes.
    let bases = [base1.clone(), base2.clone()];
    assert_eq!(Store::prune_wal_archive(&archive, &bases).unwrap(), 1);
    assert!(archived(0) && archived(1) && archived(2) && !other.exists());
    // With only the second, the logs before it go, and it still restores to the latest.
    assert_eq!(Store::prune_wal_archive(&archive, &bases[1..]).unwrap(), 2);
    assert!(!archived(0) && !archived(1) && archived(2));
    let logs = [archive.clone(), live.clone()];
    let restored = Store::restore(&base2, &logs, None, dir.join("r1")).unwrap();
    assert_eq!(restored.quads, 3, "{restored:?}");
    // An archive that does not exist is nothing to prune.
    assert_eq!(
        Store::prune_wal_archive(dir.join("none"), &bases).unwrap(),
        0
    );
    // Without bases (only N-Quads backups kept), the live timeline's directory goes too, and
    // the next checkpoint archives into it all the same.
    Store::prune_wal_archive(&archive, &[]).unwrap();
    assert!(!logs_dir.exists());
    store.insert(&ex_quad("t4")).unwrap();
    store.checkpoint().unwrap();
    assert!(archived(3), "the log retired after the prune is archived");
    assert!(!live.join("wal-3.log").exists());
    drop(store);
}

#[test]
fn a_restore_does_not_replay_past_a_bulk_load() {
    let dir = temp_dir("pitr-load");
    let (live, archive, base) = (dir.join("live"), dir.join("archive"), dir.join("base"));
    let store = Store::open(&live).unwrap();
    store.set_wal_archive(Some(archive.clone())).unwrap();
    store.save_base(&base).unwrap();
    store.insert(&ex_quad("t1")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let before_load = std::time::SystemTime::now();
    std::thread::sleep(std::time::Duration::from_millis(20));
    // A bulk load (more than a small one): into a checkpoint, not the log.
    let nt: String = (0..150_000)
        .map(|i| format!("<http://example.com/s{i}> <http://example.com/p> \"{i}\" .\n"))
        .collect();
    store.bulk_load(RdfFormat::NTriples, nt.as_bytes()).unwrap();
    let logs = [archive.clone(), live.clone()];
    let err = Store::restore(&base, &logs, None, dir.join("r1")).unwrap_err();
    assert!(err.to_string().contains("bulk load"), "{err}");
    assert!(!dir.join("r1").exists());
    let restored = Store::restore(&base, &logs, Some(before_load), dir.join("r2")).unwrap();
    assert_eq!(restored.quads, 1, "{restored:?}");
    drop(store);
}

#[test]
fn wkt_literals_kept_in_binary_form_read_back_exactly() {
    let wkt_type = NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#wktLiteral");
    let wkt = |v: String| Literal::new_typed_literal(v, wkt_type);
    let mut literals: Vec<Literal> = (0..400)
        .map(|i| {
            let (x, y) = (3.0 + i as f64 * 0.0012345, 51.0 + i as f64 * 0.000731);
            match i % 8 {
                0 => wkt(format!("POINT({x:.7} {y:.7})")),
                1 => wkt(format!("POINT ({x} {y})")),
                2 => wkt(format!(
                    "LINESTRING({x:.6} {y:.6}, {:.6} {:.6}, {:.6} {y:.6})",
                    x + 0.1,
                    y + 0.1,
                    x + 0.2
                )),
                3 => wkt(format!(
                    "POLYGON(({x:.3} {y:.3},{:.3} {y:.3},{:.3} {:.3},{x:.3} {y:.3}))",
                    x + 1.0,
                    x + 1.0,
                    y + 1.0
                )),
                4 => wkt(format!(
                    "<http://www.opengis.net/def/crs/EPSG/0/4326> POINT({y:.5} {x:.5})"
                )),
                5 => wkt(format!("MULTIPOINT(({x} {y}), ({y} {x}))")),
                // Kept as text: Z ordinates, and decimals that vary.
                6 => wkt(format!("POINT Z({x:.2} {y:.2} {i})")),
                _ => wkt(format!("POINT({x:.2} {y:.3}0)")),
            }
        })
        .collect();
    // Not WKT, and a string with the text of a WKT literal: both stay as they are.
    literals.push(Literal::new_typed_literal(
        r#"{"type":"Point","coordinates":[1,2]}"#,
        NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#geoJSONLiteral"),
    ));
    let insert = |store: &Store, literals: &[Literal], from: usize| {
        for (i, l) in literals.iter().enumerate() {
            store
                .insert(QuadRef::new(
                    NamedNodeRef::new_unchecked(&format!("http://example.com/g{}", from + i)),
                    NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#asWKT"),
                    l,
                    GraphNameRef::DefaultGraph,
                ))
                .unwrap();
        }
    };
    let check = |store: &Store, literals: &[Literal]| {
        for l in literals {
            let id = store.term_id(l.as_ref().into()).expect("found by its text");
            assert_eq!(store.snapshot().term(id).unwrap(), Term::from(l.clone()));
            let found: Vec<Quad> = store
                .quads_for_pattern(None, None, Some(l.as_ref().into()), None)
                .collect();
            assert_eq!(found.len(), 1, "{l}");
            assert_eq!(found[0].object, Term::from(l.clone()));
            if l.datatype() == wkt_type {
                let g = store.geometry(l.as_ref().into()).expect("a geometry");
                // From the box saved with a polygon's compact key (#754), or from its record.
                let (crs, bbox) = store.geometry_bbox(id).unwrap();
                assert_eq!(crs, g.crs);
                let rect = geo::BoundingRect::bounding_rect(&g.geometry);
                assert_eq!(
                    bbox,
                    rect.map(|r| [r.min().x, r.min().y, r.max().x, r.max().y])
                );
            }
        }
        // The text of a literal, as a string, is not a WKT literal.
        let text = Literal::new_simple_literal(literals[0].value());
        assert!(store.term_id(text.as_ref().into()).is_none());
    };
    let dict_bytes = |dir: &Path| -> u64 {
        let n = std::fs::read_to_string(dir.join("CURRENT")).unwrap();
        std::fs::read_dir(dir.join(format!("checkpoint-{}", n.trim())))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("dict"))
            .filter(|e| !e.file_name().to_string_lossy().contains(".idx"))
            .map(|e| e.metadata().unwrap().len())
            .sum()
    };
    let (half, all) = (literals.len() / 2, literals.len());
    let plain = temp_dir("wkt-plain");
    {
        let store = Store::open(&plain).unwrap();
        insert(&store, &literals, 0);
        store.checkpoint().unwrap();
    }
    let dir = temp_dir("wkt-binary");
    {
        let store = Store::open(&dir).unwrap();
        assert!(!store.render_wkt());
        store.set_render_wkt(true).unwrap();
        insert(&store, &literals[..half], 0);
        store.checkpoint().unwrap();
        check(&store, &literals[..half]);
        // The next checkpoint keeps the first piece and writes the new keys compact too.
        insert(&store, &literals[half..], half);
        check(&store, &literals);
        store.checkpoint().unwrap();
        check(&store, &literals);
    }
    assert!(
        dict_bytes(&dir) * 10 < dict_bytes(&plain) * 9,
        "{} against {}",
        dict_bytes(&dir),
        dict_bytes(&plain)
    );
    let store = Store::open(&dir).unwrap();
    assert!(store.render_wkt());
    check(&store, &literals);
    assert_eq!(store.len(), all);
    // A saved copy keeps the option; read-only opens read the same.
    let copy = temp_dir("wkt-binary-copy");
    store.save_to(&copy).unwrap();
    drop(store);
    let store = Store::open_read_only(&copy).unwrap();
    assert!(store.render_wkt());
    check(&store, &literals);
    // Off: later checkpoints write the text again; what is compact reads the same.
    drop(store);
    let store = Store::open(&dir).unwrap();
    store.set_render_wkt(false).unwrap();
    let more: Vec<Literal> = (0..10).map(|i| wkt(format!("POINT({i} 1)"))).collect();
    insert(&store, &more, all);
    store.checkpoint().unwrap();
    check(&store, &literals);
    check(&store, &more);
    drop(store);
    // Turned off, the store still finds the literals kept compact: its checkpoints say that
    // their dictionary may hold compact keys, and so do copies of it.
    let store = Store::open(&dir).unwrap();
    assert!(!store.render_wkt());
    check(&store, &literals);
    check(&store, &more);
    assert_eq!(store.len(), all + more.len());
    let off_copy = temp_dir("wkt-binary-off-copy");
    store.save_to(&off_copy).unwrap();
    drop(store);
    let store = Store::open_read_only(&off_copy).unwrap();
    check(&store, &literals);
    assert_eq!(store.len(), all + more.len());
    drop(store);
    // A store that never kept WKT in binary form has no such checkpoint.
    let current = |d: &Path| {
        let n = std::fs::read_to_string(d.join("CURRENT")).unwrap();
        d.join(format!("checkpoint-{}", n.trim()))
    };
    assert!(!current(&plain).join("compact-keys").exists());
    assert!(current(&dir).join("compact-keys").exists());
    // Pieces that may hold compact keys have a magic of their own, which a binary from before
    // them refuses (#428), v5 since their records may be packed (#152); the others keep the
    // v3 one. Both keep their IRIs' namespaces once (#152), so they are v7 and v6.
    let magics = |d: &Path| -> Vec<Vec<u8>> {
        let mut m: Vec<Vec<u8>> = std::fs::read_dir(current(d))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("dict") && n.ends_with(".bin"))
            .map(|n| std::fs::read(current(d).join(n)).unwrap()[..8].to_vec())
            .collect();
        m.sort();
        m.dedup();
        m
    };
    assert_eq!(magics(&plain), [b"OXGSDIC6".to_vec()]);
    assert!(
        magics(&dir).contains(&b"OXGSDIC7".to_vec()),
        "{:?}",
        magics(&dir)
    );
    // The magic alone tells a reader that the keys may be compact.
    std::fs::remove_file(current(&off_copy).join("compact-keys")).unwrap();
    let store = Store::open_read_only(&off_copy).unwrap();
    check(&store, &literals);
    drop(store);
    for d in [plain, dir, copy, off_copy] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// A child process started by another thread holds a copy of the store's open files until it
/// runs its program: a store closed meanwhile can still be opened again at once.
#[cfg(unix)]
#[test]
fn a_store_opens_again_while_other_threads_start_processes() {
    let dir = tripletrail_testdir::tempdir("store-spawn");
    drop(Store::open(&dir).unwrap());
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let spawner = {
        let stop = std::sync::Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = std::process::Command::new("true").status();
            }
        })
    };
    for _ in 0..300 {
        let store = Store::open(&dir);
        assert!(store.is_ok(), "{:?}", store.err());
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    spawner.join().unwrap();
}

#[test]
fn a_store_writes_no_checkpoint_until_its_files_are_checked() {
    // Every store counts as large on this thread: opening checks lengths and headers only.
    Store::verify_in_background_above(0);
    let dir = temp_dir("verify-hold");
    {
        let store = Store::open(&dir).unwrap();
        store.bulk_load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
        store.checkpoint().unwrap();
    }
    let checkpoints = || -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("checkpoint-"))
            .collect();
        names.sort();
        names
    };
    let before = checkpoints();
    let path = dir.join(before.last().unwrap()).join("dict.bin");
    let mut bytes = std::fs::read(&path).unwrap();
    let at = bytes.len() / 2;
    bytes[at] ^= 0x55;
    std::fs::write(&path, bytes).unwrap();
    // The check waits until released: what is written meanwhile happens during the check.
    struct Release;
    impl Drop for Release {
        fn drop(&mut self) {
            Store::hold_verification(false);
        }
    }
    Store::hold_verification(true);
    let release = Release;
    let store = Store::open(&dir).unwrap();
    assert_eq!(store.verification(), Verification::Checking);
    let quad = |i: usize| {
        Quad::new(
            NamedNodeRef::new_unchecked(&format!("http://example.com/new{i}")).into_owned(),
            NamedNodeRef::new_unchecked("http://example.com/p").into_owned(),
            Literal::from(i as i64),
            GraphNameRef::DefaultGraph,
        )
    };
    // Commits go into the log; neither a full log nor a checkpoint call writes a checkpoint,
    // which would sum the unchecked data anew.
    store.set_checkpoint_log_size(0);
    store.insert(&quad(1)).unwrap();
    store.insert(&quad(2)).unwrap();
    store.checkpoint().unwrap();
    assert_eq!(checkpoints(), before);
    std::thread::scope(|scope| {
        // A copy (like a bulk load, which is not in the log) waits for the check, and fails
        // with it.
        let copy = dir.with_extension("copy");
        let store = &store;
        let load = scope.spawn(move || store.save_to(&copy));
        // So does a bulk load too large for the log, and a refused one changes nothing.
        let len = store.len();
        let bulk = scope.spawn(move || store.bulk_load_quads((0..150_000).map(|i| quad(1000 + i))));
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!load.is_finished(), "the copy waits for the check");
        assert!(!bulk.is_finished(), "the bulk load waits for the check");
        drop(release);
        let error = bulk
            .join()
            .unwrap()
            .expect_err("the bulk load fails on the damage");
        assert!(error.to_string().contains("dict.bin"), "{error}");
        assert_eq!(store.len(), len, "the refused load left nothing");
        let error = load
            .join()
            .unwrap()
            .expect_err("the load fails on the damage");
        assert!(error.to_string().contains("dict.bin"), "{error}");
    });
    assert!(matches!(store.verification(), Verification::Damaged(f) if f.contains("dict.bin")));
    assert!(store.insert(&quad(3)).is_err());
    assert_eq!(
        checkpoints(),
        before,
        "the damaged data was not written again"
    );
    drop(store);
}

#[test]
fn large_stores_check_their_files_in_the_background() {
    // Every store counts as large on this thread: opening checks lengths and headers only.
    Store::verify_in_background_above(0);
    let dir = temp_dir("verify-background");
    let expected = {
        let store = Store::open(&dir).unwrap();
        store.bulk_load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
        store.checkpoint().unwrap();
        sorted(&store)
    };
    let checkpoint = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("checkpoint-"))
        .unwrap()
        .path();
    let flip = |file: &str| {
        let path = checkpoint.join(file);
        let mut bytes = std::fs::read(&path).unwrap();
        // A byte of data, not of a header, nor the last key end.
        let at = bytes.len() / 2;
        bytes[at] ^= 0x55;
        std::fs::write(&path, bytes).unwrap();
    };
    let store = Store::open(&dir).unwrap();
    store.wait_verified().unwrap();
    assert_eq!(store.verification(), Verification::Verified);
    drop(store);
    for file in ["geoms.bin", "dict.bin", "spog.seg"] {
        flip(file);
        let store = Store::open(&dir).unwrap();
        let error = store
            .wait_verified()
            .expect_err("a damaged file is reported");
        assert!(error.to_string().contains(file), "{error}");
        assert!(matches!(store.verification(), Verification::Damaged(f) if f.contains(file)));
        let insert = store.insert(QuadRef::new(
            NamedNodeRef::new_unchecked("http://example.com/s"),
            NamedNodeRef::new_unchecked("http://example.com/p"),
            NamedNodeRef::new_unchecked("http://example.com/o"),
            GraphNameRef::DefaultGraph,
        ));
        assert!(insert.is_err(), "a damaged store refuses writes");
        assert!(store.checkpoint().is_err());
        drop(store);
        let store = Store::open_read_only(&dir).unwrap();
        assert!(store.wait_verified().is_err());
        drop(store);
        flip(file);
    }
    let store = Store::open(&dir).unwrap();
    store.wait_verified().unwrap();
    assert_eq!(sorted(&store), expected);
    drop(store);
}

#[test]
fn a_fork_changes_apart_and_reports_its_changes_by_id() {
    let ex = |s: &str| oxigraph::model::NamedNode::new(format!("http://example.com/{s}")).unwrap();
    let quad = |s: &str, o: &str, g: Option<&str>| {
        Quad::new(
            ex(s),
            ex("p"),
            ex(o),
            g.map_or(oxigraph::model::GraphName::DefaultGraph, |g| ex(g).into()),
        )
    };
    let (reference, store) = load_both();
    // The fork answers as the store does.
    assert_same_answers(&reference, &store.fork());
    store.insert(&quad("a", "b", Some("g"))).unwrap();
    let before = store.len();
    let fork = store.fork();
    assert_eq!(fork.len(), before);
    // Writes on either side stay there.
    fork.insert(&quad("new", "term", None)).unwrap();
    fork.remove(&quad("a", "b", Some("g"))).unwrap();
    store.insert(&quad("only", "original", None)).unwrap();
    assert_eq!(store.len(), before + 1);
    assert_eq!(fork.len(), before);
    assert!(!store.contains(&quad("new", "term", None)));
    assert!(store.contains(&quad("a", "b", Some("g"))));
    assert!(!fork.contains(&quad("only", "original", None)));

    let base = store.fork();
    let after = base.fork();
    after.insert(&quad("x", "y", Some("g2"))).unwrap();
    after.remove(&quad("a", "b", Some("g"))).unwrap();
    after
        .bulk_load_quads((0..1000).map(|i| quad(&format!("s{i}"), "o", None)))
        .unwrap();
    let (added, removed) = after.changes_from(&base);
    assert_eq!(added.len(), 1001);
    assert_eq!(removed.len(), 1);
    let view = after.snapshot();
    let terms = |q: [tripletrail_store::TermId; 4]| {
        (view.term(q[0]).unwrap(), view.term(q[2]).unwrap(), q[3].0)
    };
    let (s, o, g) = terms(removed[0]);
    assert_eq!((s, o), (Term::from(ex("a")), Term::from(ex("b"))));
    assert_eq!(
        view.term(tripletrail_store::TermId(g)).unwrap(),
        Term::from(ex("g"))
    );
    assert!(added.iter().any(|q| terms(*q).0 == Term::from(ex("x"))));
    // Nothing changed: nothing reported.
    let (a, r) = base.fork().changes_from(&base);
    assert!(a.is_empty() && r.is_empty());
    let _ = GraphNameRef::DefaultGraph;
}

#[test]
fn an_overlay_answers_like_a_copy_and_leaves_its_base_alone() {
    let ex =
        |s: String| oxigraph::model::NamedNode::new(format!("http://example.com/{s}")).unwrap();
    let quad = |i: usize, g: Option<&str>| {
        Quad::new(
            ex(format!("s{}", i / 7)),
            ex(format!("p{}", i % 7)),
            Literal::from(i as i64),
            g.map_or(oxigraph::model::GraphName::DefaultGraph, |g| {
                ex(g.to_owned()).into()
            }),
        )
    };
    let base = Store::new();
    base.bulk_load_quads((0..20_000).map(|i| quad(i, (i % 3 == 0).then_some("g"))))
        .unwrap();
    let before = sorted(&base);
    let overlay = base.overlay();
    let copy = Store::new();
    copy.bulk_load_quads(base.iter()).unwrap();
    // Small writes, a load, removes of base quads, and a graph cleared, on both.
    for store in [&overlay, &copy] {
        for i in 0..100 {
            store.insert(&quad(50_000 + i, None)).unwrap();
        }
        store
            .bulk_load_quads((0..5_000).map(|i| quad(60_000 + i, Some("h"))))
            .unwrap();
        for i in (0..20_000).step_by(13) {
            store.remove(&quad(i, (i % 3 == 0).then_some("g"))).unwrap();
        }
        store
            .transaction(|t| -> Result<(), tripletrail_store::StoreError> {
                for i in 0..70_000 {
                    t.insert(&quad(100_000 + i, None));
                }
                Ok(())
            })
            .unwrap();
        store
            .clear_graph(oxigraph::model::NamedNodeRef::new_unchecked(
                "http://example.com/h",
            ))
            .unwrap();
    }
    assert_eq!(sorted(&overlay), sorted(&copy));
    assert_eq!(sorted(&base), before, "the base is unchanged");
    let (added, removed) = overlay.changes_from(&base);
    assert_eq!(added.len(), 70_100);
    assert_eq!(removed.len(), (0..20_000).step_by(13).count());
    assert!(overlay.unmerged_len() >= added.len() + removed.len());
    // The base changes apart from it.
    base.insert(&quad(999_999, None)).unwrap();
    assert!(!overlay.contains(&quad(999_999, None)));
}

/// `cargo test --release -p tripletrail-store --test store fork_of_a_million -- --ignored --nocapture`
#[test]
#[ignore]
fn fork_of_a_million_quads_and_one_change() {
    let ex =
        |s: String| oxigraph::model::NamedNode::new(format!("http://example.com/{s}")).unwrap();
    let store = Store::new();
    store
        .bulk_load_quads((0..1_000_000).map(|i| {
            Quad::new(
                ex(format!("s{}", i / 10)),
                ex(format!("p{}", i % 10)),
                Literal::from(i as i64),
                oxigraph::model::GraphName::DefaultGraph,
            )
        }))
        .unwrap();
    let t = std::time::Instant::now();
    let copied = Store::new();
    copied.bulk_load_quads(store.iter()).unwrap();
    println!("copy by quads: {:?}", t.elapsed());
    let t = std::time::Instant::now();
    let fork = store.fork();
    println!("fork: {:?}", t.elapsed());
    fork.insert(&Quad::new(
        ex("x".into()),
        ex("p".into()),
        Literal::from(1),
        oxigraph::model::GraphName::DefaultGraph,
    ))
    .unwrap();
    let t = std::time::Instant::now();
    let (a, r) = fork.changes_from(&store);
    println!(
        "changes: {:?} ({} added, {} removed)",
        t.elapsed(),
        a.len(),
        r.len()
    );
    assert_eq!((a.len(), r.len()), (1, 0));
}

/// `Changes::decoded_where` decodes only the changes it selects, in order, and
/// `Changes::predicates` names each predicate once (#292).
#[test]
fn changes_decode_only_what_is_selected() {
    let store = Store::new();
    store.bulk_load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    let update = spargebra::SparqlParser::new()
        .parse_update(
            "PREFIX ex: <http://example.com/> PREFIX geo: <http://www.opengis.net/ont/geosparql#>
             DELETE { ex:s1 geo:asWKT ?w } INSERT { ?s ex:copy ?w . ?s ex:n 9 }
             WHERE { ?s geo:asWKT ?w }",
        )
        .unwrap();
    let changes = store.update(&update, SparqlEvaluator::new).unwrap();
    assert_eq!(changes.len(), 7);
    let mut predicates: Vec<String> = changes
        .predicates()
        .into_iter()
        .map(|p| p.as_str().to_owned())
        .collect();
    predicates.sort();
    assert_eq!(
        predicates,
        [
            "http://example.com/copy",
            "http://example.com/n",
            "http://www.opengis.net/ont/geosparql#asWKT"
        ]
    );
    let geometries = changes.decoded_where(|_, o| o.kind() == Kind::Geometry);
    assert_eq!(geometries.len(), 4);
    assert_eq!(geometries.iter().filter(|(added, _)| !added).count(), 1);
    let all: Vec<_> = changes.iter().collect();
    let in_order: Vec<_> = all
        .into_iter()
        .filter(|(_, q)| matches!(&q.object, Term::Literal(l) if l.datatype().as_str().ends_with("wktLiteral")))
        .collect();
    assert_eq!(geometries, in_order);
    assert!(
        changes
            .decoded_where(|p, _| p.as_str().ends_with("nothing"))
            .is_empty()
    );
}

/// Past eight dictionary pieces, a checkpoint writes the later pieces again as one and keeps
/// the first, which holds most keys, as it is (#218); old checkpoints go on a thread of their
/// own. The store reads back whole.
#[cfg(unix)]
#[test]
fn many_checkpoints_keep_the_first_dictionary_piece() {
    use std::os::unix::fs::MetadataExt;
    let dir = temp_dir("dict-pieces");
    let quad = |i: u64| {
        Quad::new(
            NamedNodeRef::new_unchecked(&format!("http://example.org/s{i}")).into_owned(),
            NamedNodeRef::new_unchecked("http://example.org/p").into_owned(),
            Literal::new_simple_literal(format!("value {i}")),
            GraphNameRef::DefaultGraph,
        )
    };
    let current = |dir: &Path| {
        let n = std::fs::read_to_string(dir.join("CURRENT")).unwrap();
        dir.join(format!("checkpoint-{}", n.trim()))
    };
    {
        let store = Store::open(&dir).unwrap();
        store.bulk_load_quads((0..10_000).map(quad)).unwrap();
        store.checkpoint().unwrap();
        let first = std::fs::metadata(current(&dir).join("dict.bin"))
            .unwrap()
            .ino();
        for c in 0..20u64 {
            store
                .transaction(|t| {
                    for i in 0..100 {
                        t.insert(quad(10_000 + c * 100 + i).as_ref());
                    }
                    Ok::<_, tripletrail_store::StoreError>(())
                })
                .unwrap();
            store.checkpoint().unwrap();
            let now = current(&dir);
            assert_eq!(
                std::fs::metadata(now.join("dict.bin")).unwrap().ino(),
                first,
                "checkpoint {c} wrote the first piece again"
            );
            let pieces = std::fs::read_dir(&now)
                .unwrap()
                .flatten()
                .filter(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    n.starts_with("dict") && n.ends_with(".bin")
                })
                .count();
            assert!(pieces <= 8, "{pieces} pieces");
        }
    }
    let store = Store::open(&dir).unwrap();
    assert_eq!(store.len(), 12_000);
    for i in [0, 9_999, 10_000, 11_999] {
        assert!(store.contains(quad(i).as_ref()), "{i}");
    }
    // The old checkpoints are gone, renamed and then deleted in the background.
    let left = || {
        std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.starts_with("checkpoint-") || n.starts_with("stale-")
            })
            .count()
    };
    let start = std::time::Instant::now();
    while left() > 1 && start.elapsed() < std::time::Duration::from_secs(10) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(left(), 1);
    drop(store);
}

#[test]
fn concurrent_transactions_share_log_syncs_and_all_survive_a_restart() {
    // Group commit (#709): writers sync the log after letting go of the writer lock, and
    // checkpoints (here after every few hundred bytes of log) switch logs meanwhile.
    let dir = temp_dir("group-commit");
    let quad = |t: usize, i: usize| {
        Quad::new(
            NamedNodeRef::new_unchecked(&format!("http://example.com/t{t}")).into_owned(),
            NamedNodeRef::new_unchecked("http://example.com/n").into_owned(),
            Literal::from(i as i64),
            GraphNameRef::DefaultGraph,
        )
    };
    {
        let store = Store::open(&dir).unwrap();
        store.set_checkpoint_log_size(4_000);
        std::thread::scope(|s| {
            for t in 0..8 {
                let store = &store;
                s.spawn(move || {
                    for i in 0..150 {
                        store
                            .transaction(|txn| {
                                txn.insert(&quad(t, i));
                                if i % 3 == 2 {
                                    txn.remove(&quad(t, i - 1));
                                }
                                Ok::<_, tripletrail_store::StoreError>(())
                            })
                            .unwrap();
                    }
                });
            }
        });
        assert_eq!(store.len(), 8 * 100);
    }
    let store = Store::open(&dir).unwrap();
    let mut expected: Vec<String> = (0..8)
        .flat_map(|t| {
            (0..150)
                .filter(|i| i % 3 != 1)
                .map(move |i| quad(t, i).to_string())
        })
        .collect();
    expected.sort();
    assert_eq!(sorted(&store), expected);
    drop(store);
}

/// Read replicas (#217): a follower that reads the log from where a base starts, across a
/// checkpoint, ends up with the leader's quads; a log retired without being kept is gone.
#[test]
fn a_follower_reading_the_log_keeps_up_with_the_store() {
    use tripletrail_store::{LogEntry, LogRead};
    let dir = temp_dir("ship");
    let base = temp_dir("ship-base");
    let ex = |s: &str| NamedNodeRef::new_unchecked(s).into_owned();
    let quad = |i: usize| {
        Quad::new(
            ex(&format!("http://example.com/s{i}")),
            ex("http://example.com/p"),
            Literal::from(i as i64),
            GraphNameRef::DefaultGraph,
        )
    };
    let leader = Store::open(&dir).unwrap();
    leader.set_log_retention(1);
    leader.insert(&quad(0)).unwrap();
    leader.save_base(&base).unwrap();
    let start = leader.log_position().unwrap();
    for i in 1..20 {
        leader
            .transaction(|t| {
                t.insert(quad(i).as_ref());
                if i % 3 == 0 {
                    t.remove(quad(i - 1).as_ref());
                }
                Ok::<_, tripletrail_store::StoreError>(())
            })
            .unwrap();
        if i == 10 {
            leader.checkpoint_if_changed().unwrap();
        }
    }
    let end = leader.log_position().unwrap();
    assert_eq!(end.timeline, start.timeline);
    assert_eq!(end.generation, start.generation + 1);

    let (timeline, mut generation, mut skip) = Store::base_position(&base).unwrap().unwrap();
    assert_eq!(timeline, start.timeline);
    let target = temp_dir("ship-follower");
    Store::copy_dir(&base, &target).unwrap();
    let follower = Store::open(&target).unwrap();
    let mut offset = 0;
    let mut max = 100;
    loop {
        // Small reads, cut inside entries.
        let LogRead::Bytes {
            bytes,
            end,
            retired,
        } = leader.read_log(generation, offset, max).unwrap()
        else {
            panic!("the log of generation {generation} is kept");
        };
        let consumed = tripletrail_store::scan_log(&bytes, |entry| {
            let LogEntry::Transaction(time, changes) = entry else {
                panic!("no bulk loads here");
            };
            assert!(time.is_some());
            if skip > 0 {
                skip -= 1;
                return true;
            }
            follower
                .transaction(|t| {
                    for (insert, q) in &changes {
                        if *insert {
                            t.insert(q.as_ref());
                        } else {
                            t.remove(q.as_ref());
                        }
                    }
                    Ok::<_, tripletrail_store::StoreError>(())
                })
                .unwrap();
            true
        })
        .unwrap();
        offset += consumed;
        // A transaction longer than a read: read more at once.
        max = if consumed == 0 { max * 2 } else { 100 };
        if offset == end {
            if !retired {
                break;
            }
            generation += 1;
            offset = 0;
            skip = 0;
        }
    }
    let sorted = |s: &Store| {
        let mut q: Vec<String> = s
            .quads_for_pattern(None, None, None, None)
            .map(|q| q.to_string())
            .collect();
        q.sort();
        q
    };
    assert_eq!(sorted(&follower), sorted(&leader));
    assert_eq!(sorted(&leader).len(), 20 - 6);

    // Without retention, the next checkpoint deletes the retired log.
    leader.set_log_retention(0);
    leader.insert(&quad(100)).unwrap();
    leader.checkpoint_if_changed().unwrap();
    assert_eq!(
        leader.read_log(start.generation, 0, 100).unwrap(),
        LogRead::Gone
    );
    assert_eq!(
        leader.read_log(end.generation + 5, 0, 100).unwrap(),
        LogRead::Gone
    );
    drop((leader, follower));
    for d in [dir, base, target] {
        let _ = std::fs::remove_dir_all(d);
    }
}

fn one_quad() -> Quad {
    let ex = |s: &str| NamedNodeRef::new_unchecked(s).into_owned();
    Quad::new(
        ex("http://example.com/s"),
        ex("http://example.com/p"),
        ex("http://example.com/o"),
        GraphNameRef::DefaultGraph,
    )
}

/// A store keeps its format in a `FORMAT` file, and a store of a newer format is refused
/// with a clear message before its log is replayed or cut (#221).
#[test]
fn a_store_of_a_newer_format_is_refused_and_left_as_it_is() {
    let dir = temp_dir("newer-format");
    {
        let store = Store::open(&dir).unwrap();
        store.insert(&one_quad()).unwrap();
    }
    let format = std::fs::read_to_string(dir.join("FORMAT")).unwrap();
    assert_eq!(format.trim(), tripletrail_store::STORE_FORMAT.to_string());

    // A newer TripleTrail wrote this store, and a log entry this build cannot read.
    std::fs::write(dir.join("FORMAT"), "99\n").unwrap();
    let wal = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("wal"))
        .expect("a write-ahead log");
    let mut log = std::fs::read(&wal).unwrap();
    log.extend_from_slice(b"an entry in a format this TripleTrail does not know");
    std::fs::write(&wal, &log).unwrap();

    for error in [
        Store::open(&dir).err().expect("refused"),
        Store::open_read_only(&dir).err().expect("refused"),
    ] {
        let message = error.to_string();
        assert!(
            message.contains("format 99") && message.contains("newer TripleTrail"),
            "{message}"
        );
    }
    assert_eq!(std::fs::read(&wal).unwrap(), log, "the log was not cut");
    assert_eq!(std::fs::read_to_string(dir.join("FORMAT")).unwrap(), "99\n");
}

/// A store file with a newer version of a known magic number gets the same clear message
/// instead of "corrupt" (#221).
#[test]
fn a_store_file_of_a_newer_version_is_named_as_such() {
    let dir = temp_dir("newer-file");
    {
        let store = Store::open(&dir).unwrap();
        store.insert(&one_quad()).unwrap();
        store.checkpoint().unwrap();
    }
    let mut bumped = 0;
    for entry in std::fs::read_dir(dir.join("checkpoint-1")).unwrap() {
        let path = entry.unwrap().path();
        let Ok(mut bytes) = std::fs::read(&path) else {
            continue;
        };
        if bytes.starts_with(b"OXGSSEG") {
            bytes[7] = b'9';
            std::fs::write(&path, bytes).unwrap();
            bumped += 1;
        }
    }
    assert!(bumped > 0, "no segment file in {}", dir.display());
    let message = Store::open(&dir).err().expect("refused").to_string();
    assert!(
        message.contains("OXGSSEG9") && message.contains("newer TripleTrail"),
        "{message}"
    );
}

/// A base backup carries its format, and a newer TripleTrail's base is refused by a restore,
/// a copy and an open, before its log is read (#221).
#[test]
fn a_base_of_a_newer_format_is_refused() {
    let dir = temp_dir("newer-base");
    let store = Store::open(dir.join("live")).unwrap();
    store.insert(&one_quad()).unwrap();
    let base = dir.join("base");
    store.save_base(&base).unwrap();
    drop(store);
    assert_eq!(
        std::fs::read_to_string(base.join("FORMAT")).unwrap().trim(),
        tripletrail_store::STORE_FORMAT.to_string()
    );
    std::fs::write(base.join("FORMAT"), "2\n").unwrap();
    let refused = |e: tripletrail_store::StoreError| {
        let message = e.to_string();
        assert!(
            message.contains("format 2") && message.contains("newer TripleTrail"),
            "{message}"
        );
    };
    refused(Store::restore(&base, &[], None, dir.join("restored")).expect_err("refused"));
    assert!(!dir.join("restored").exists());
    refused(Store::copy_dir(&base, dir.join("copy")).expect_err("refused"));
    assert!(!dir.join("copy").exists());
    refused(Store::open(&base).err().expect("refused"));
    // A format past 32 bits is newer too, not damage.
    std::fs::write(base.join("FORMAT"), "99999999999\n").unwrap();
    let message = Store::open(&base).err().expect("refused").to_string();
    assert!(message.contains("newer TripleTrail"), "{message}");
}

/// The copies of triples in several graphs (#808): after one scan of the store, every write
/// brings them up to date from what it touched, and the union then shows each triple once.
#[test]
fn union_copies_stay_right_after_writes() {
    let store = Store::new();
    let iri = |i: u64| oxigraph::model::NamedNode::new(format!("http://example.com/{i}")).unwrap();
    let graph = |g: u64| match g {
        0 => oxigraph::model::GraphName::DefaultGraph,
        g => iri(1000 + g).into(),
    };
    let quad = |x: u64| {
        Quad::new(
            iri(x % 7),
            iri(100 + x / 7 % 3),
            iri(200 + x / 21 % 5),
            graph(x / 105 % 4),
        )
    };
    let check = |store: &Store, scan: bool| -> Option<()> {
        let snapshot = store.snapshot();
        let copies = snapshot.union_copies(scan, &|| false)?;
        let mut shown = Vec::new();
        let mut all = Vec::new();
        for q in snapshot.quad_ids_for_pattern(None, None, None, GraphSelector::All) {
            let q = [q[0].0, q[1].0, q[2].0, q[3].0];
            all.push([q[0], q[1], q[2]]);
            if !copies.as_ref().is_some_and(|c| c.hides(&q)) {
                shown.push([q[0], q[1], q[2]]);
            }
        }
        all.sort_unstable();
        all.dedup();
        shown.sort_unstable();
        assert_eq!(shown, all, "each triple shown once");
        Some(())
    };
    for x in (0..420).step_by(3) {
        store.insert(&quad(x)).unwrap();
    }
    // Not found without the scan of the whole store, then found with it.
    assert!(check(&store, false).is_none());
    assert!(check(&store, true).is_some());
    let mut x: u64 = 1;
    for round in 0..200 {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let q = quad((x >> 33) % 420);
        if round % 3 == 0 {
            store.remove(&q).unwrap();
        } else {
            store.insert(&q).unwrap();
        }
        // Kept up to date from the write: no scan of the store needed.
        assert!(check(&store, false).is_some(), "round {round}");
    }
    // A batch of writes in one transaction, then a clear of a graph.
    store
        .transaction(|t| {
            for x in 0..420 {
                if x % 5 == 0 {
                    t.insert(&quad(x));
                }
            }
            Ok::<_, tripletrail_store::StoreError>(())
        })
        .unwrap();
    assert!(check(&store, false).is_some());
    for q in store
        .quads_for_pattern(None, None, None, Some(graph(2).as_ref()))
        .collect::<Vec<_>>()
    {
        store.remove(&q).unwrap();
    }
    assert!(check(&store, false).is_some());
}

/// A store whose dictionary is format 4 (#370: compact WKT keys, their records plain) opens
/// with a binary that writes format 5 (#152: packed records): its literals are found by their
/// text, a literal loaded again gets no second id, and new literals are packed beside them.
#[test]
fn a_format_4_store_with_compact_keys_opens_and_grows() {
    let wkt_type = NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#wktLiteral");
    let wkt = |v: String| Literal::new_typed_literal(v, wkt_type);
    let quad = |i: usize, l: &Literal| {
        Quad::new(
            // Without a `/`: no namespace is kept, as in format 4.
            oxigraph::model::NamedNode::new_unchecked(format!("urn:example:f{i}")),
            oxigraph::model::NamedNode::new_unchecked("http://www.opengis.net/ont/geosparql#asWKT"),
            l.clone(),
            oxigraph::model::GraphName::DefaultGraph,
        )
    };
    // Full-precision doubles in the shortest style: compact, but not packed, as in format 4.
    let old: Vec<Literal> = (0..50)
        .map(|i| {
            wkt(format!(
                "POINT({} {})",
                3.0 + i as f64 / 7.0,
                51.0 + i as f64 / 11.0
            ))
        })
        .collect();
    let dir = temp_dir("wkt-format-4");
    {
        let store = Store::open(&dir).unwrap();
        store.set_render_wkt(true).unwrap();
        for (i, l) in old.iter().enumerate() {
            store.insert(quad(i, l).as_ref()).unwrap();
        }
        store.checkpoint().unwrap();
    }
    // Made format 4: its dictionary files' magic, and their checksums.
    let current = dir.join(format!(
        "checkpoint-{}",
        std::fs::read_to_string(dir.join("CURRENT")).unwrap().trim()
    ));
    let sums_path = current.join("checksums");
    let mut sums = std::fs::read_to_string(&sums_path).unwrap();
    for e in std::fs::read_dir(&current).unwrap().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("dict") && name.ends_with(".bin")) {
            continue;
        }
        let mut bytes = std::fs::read(e.path()).unwrap();
        assert_eq!(&bytes[..8], b"OXGSDIC5", "{name}");
        bytes[..8].copy_from_slice(b"OXGSDIC4");
        std::fs::write(e.path(), &bytes).unwrap();
        sums = sums
            .lines()
            .map(|line| {
                if line.split(' ').next() == Some(name.as_str()) {
                    format!("{name} {} {:08x}", bytes.len(), crc32fast::hash(&bytes))
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
    }
    std::fs::write(&sums_path, sums).unwrap();
    let new: Vec<Literal> = (0..50)
        .map(|i| {
            wkt(format!(
                "POINT({:.7} {:.7})",
                4.0 + i as f64 / 7.0,
                50.0 + i as f64 / 11.0
            ))
        })
        .collect();
    {
        let store = Store::open(&dir).unwrap();
        assert!(store.render_wkt());
        for (i, l) in old.iter().enumerate() {
            let id = store.term_id(l.as_ref().into()).expect("found by its text");
            assert_eq!(store.snapshot().term(id).unwrap(), Term::from(l.clone()));
            assert!(store.geometry(l.as_ref().into()).is_some(), "{l}");
            // Loaded again: the same id, no new quad.
            store.insert(quad(i, l).as_ref()).unwrap();
            assert_eq!(store.term_id(l.as_ref().into()), Some(id));
        }
        assert_eq!(store.len(), old.len());
        for (i, l) in new.iter().enumerate() {
            store.insert(quad(1000 + i, l).as_ref()).unwrap();
        }
        store.checkpoint().unwrap();
    }
    let store = Store::open(&dir).unwrap();
    assert_eq!(store.len(), old.len() + new.len());
    for l in old.iter().chain(&new) {
        let id = store.term_id(l.as_ref().into()).expect("found by its text");
        assert_eq!(store.snapshot().term(id).unwrap(), Term::from(l.clone()));
        assert!(store.geometry(l.as_ref().into()).is_some(), "{l}");
    }
}

/// Without named graphs, the permutations with the graph first read the blocks of the others
/// (#152): a checkpoint saves them as small files, and named graphs added (one quad at a
/// time or in bulk) and removed again give the same answers as Oxigraph throughout, before
/// and after a checkpoint and a reopen.
#[test]
fn graph_first_permutations_read_the_others_until_named_graphs_come() {
    let current = |d: &Path| {
        let n = std::fs::read_to_string(d.join("CURRENT")).unwrap();
        d.join(format!("checkpoint-{}", n.trim()))
    };
    let formats = |d: &Path| -> Vec<(String, Vec<u8>)> {
        let mut f: Vec<(String, Vec<u8>)> = ["gspo", "gpos", "gosp", "gpso"]
            .iter()
            .map(|n| {
                let bytes = std::fs::read(current(d).join(format!("{n}.seg"))).unwrap();
                (n.to_string(), bytes[..8].to_vec())
            })
            .collect();
        f.sort();
        f
    };
    let all = |magic: &[u8]| -> Vec<(String, Vec<u8>)> {
        ["gosp", "gpos", "gpso", "gspo"]
            .iter()
            .map(|n| (n.to_string(), magic.to_vec()))
            .collect()
    };
    let reference = oxigraph::store::Store::new().unwrap();
    reference.load_from_slice(RdfFormat::Turtle, DATA).unwrap();
    let named: Vec<Quad> = oxigraph::io::RdfParser::from_format(RdfFormat::NQuads)
        .for_slice(NAMED.as_bytes())
        .collect::<Result<_, _>>()
        .unwrap();
    for bulk in [false, true] {
        let dir = temp_dir(if bulk {
            "graph-first-bulk"
        } else {
            "graph-first"
        });
        let store = Store::open(&dir).unwrap();
        store.bulk_load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
        store.checkpoint().unwrap();
        assert_eq!(formats(&dir), all(b"OXGSSEG5"));
        let spog = std::fs::metadata(current(&dir).join("spog.seg"))
            .unwrap()
            .len();
        let gspo = std::fs::metadata(current(&dir).join("gspo.seg"))
            .unwrap()
            .len();
        assert!(gspo * 4 < spog, "{gspo} bytes against {spog}");
        assert_same_answers(&reference, &store);
        drop(store);
        assert_same_answers(&reference, &Store::open_read_only(&dir).unwrap());
        let store = Store::open(&dir).unwrap();
        assert_same_answers(&reference, &store);
        // Named graphs come.
        let both = oxigraph::store::Store::new().unwrap();
        both.load_from_slice(RdfFormat::Turtle, DATA).unwrap();
        both.load_from_slice(RdfFormat::NQuads, NAMED).unwrap();
        if bulk {
            store
                .bulk_load(RdfFormat::NQuads, NAMED.as_bytes())
                .unwrap();
        } else {
            for q in &named {
                store.insert(q.as_ref()).unwrap();
            }
        }
        assert_same_answers(&both, &store);
        store.checkpoint().unwrap();
        assert_same_answers(&both, &store);
        assert_eq!(formats(&dir), all(b"OXGSSEG4"));
        drop(store);
        let store = Store::open(&dir).unwrap();
        assert_same_answers(&both, &store);
        // And go again.
        for q in &named {
            assert!(store.remove(q.as_ref()).unwrap(), "{q}");
        }
        assert_same_answers(&reference, &store);
        store.checkpoint().unwrap();
        assert_same_answers(&reference, &store);
        drop(store);
        assert_same_answers(&reference, &Store::open_read_only(&dir).unwrap());
        assert_same_answers(&reference, &Store::open(&dir).unwrap());
    }
}

/// IRIs keep their namespace once (#152): a checkpoint saves the namespaces, and a store
/// read back (read-write or read-only) answers as before, ORDER BY and STRSTARTS on IRIs
/// included; a damaged namespaces file is an error, not wrong IRIs.
#[test]
fn iris_written_once_per_namespace_read_back_the_same() {
    let dir = temp_dir("namespaces");
    let (reference, _) = load_both();
    let ordered = [
        "SELECT ?s WHERE { ?s ?p ?o FILTER(isIRI(?s)) } ORDER BY ?s",
        "SELECT DISTINCT ?o WHERE { ?s ?p ?o FILTER(isIRI(?o)) } ORDER BY DESC(?o)",
        "SELECT ?s WHERE { ?s ?p ?o FILTER(STRSTARTS(STR(?s), \"http://example.com/\")) }",
        "SELECT (MIN(?s) AS ?a) (MAX(?s) AS ?b) WHERE { ?s ?p ?o FILTER(isIRI(?s)) }",
    ];
    let same = |store: &Store| {
        assert_same_answers(&reference, store);
        for q in ordered {
            let run = |r: QueryResults| match r {
                QueryResults::Solutions(s) => s
                    .map(|s| format!("{:?}", s.unwrap().values()))
                    .collect::<Vec<_>>(),
                _ => unreachable!(),
            };
            let expected = run(evaluator()
                .parse_query(q)
                .unwrap()
                .on_store(&reference)
                .execute()
                .unwrap());
            let got = run(evaluator()
                .parse_query(q)
                .unwrap()
                .on_queryable_dataset(store.snapshot())
                .execute()
                .unwrap());
            if q.contains("ORDER BY") {
                assert_eq!(got, expected, "{q}");
            } else {
                let sort = |mut v: Vec<String>| {
                    v.sort();
                    v
                };
                assert_eq!(sort(got), sort(expected), "{q}");
            }
        }
    };
    {
        let store = Store::open(&dir).unwrap();
        store.bulk_load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
        store
            .bulk_load(RdfFormat::NQuads, NAMED.as_bytes())
            .unwrap();
        same(&store);
        store.checkpoint().unwrap();
        same(&store);
    }
    let current = dir.join(format!(
        "checkpoint-{}",
        std::fs::read_to_string(dir.join("CURRENT")).unwrap().trim()
    ));
    let sums = std::fs::read_to_string(current.join("checksums")).unwrap();
    assert!(sums.contains("namespaces.bin "), "{sums}");
    let dict = std::fs::read(current.join("dict.bin")).unwrap();
    assert_eq!(&dict[..8], b"OXGSDIC6");
    same(&Store::open_read_only(&dir).unwrap());
    {
        // Reopened, it grows: new IRIs of a saved namespace refer to it.
        let store = Store::open(&dir).unwrap();
        same(&store);
        let usage = store.dictionary_usage();
        let iri = usage.iter().find(|u| u.kind == "iri").unwrap();
        assert!(iri.bytes < iri.keys * 20, "{iri:?}");
    }
    let path = current.join("namespaces.bin");
    let good = std::fs::read(&path).unwrap();
    let mut bad = good.clone();
    let at = bad.len() - 1;
    bad[at] ^= 0x55;
    std::fs::write(&path, &bad).unwrap();
    let error = Store::open(&dir).err().expect("a damaged namespaces file");
    assert!(error.to_string().contains("namespaces.bin"), "{error}");
    std::fs::write(&path, &good).unwrap();
    same(&Store::open(&dir).unwrap());
}

/// A store from before namespaces (#152), its IRIs in full, opens with a binary that keeps
/// them: its IRIs are found by their text, loading them again adds nothing, and new IRIs of the
/// same namespaces refer to a namespace in a piece of their own, beside the old one.
#[test]
fn a_store_with_iris_in_full_opens_and_grows() {
    let ex = |s: String| oxigraph::model::NamedNode::new_unchecked(s);
    let quad = |s: String, o: String| {
        Quad::new(
            ex(s),
            ex("http://example.com/vocab/next".to_owned()),
            ex(o),
            oxigraph::model::GraphName::DefaultGraph,
        )
    };
    let old: Vec<Quad> = (0..50)
        .map(|i| {
            quad(
                format!("http://example.com/way/{i}"),
                format!("http://example.com/way/{i}/geom/track"),
            )
        })
        .collect();
    let dir = temp_dir("iris-in-full");
    {
        let store = Store::open(&dir).unwrap();
        for q in &old {
            store.insert(q.as_ref()).unwrap();
        }
        store.checkpoint().unwrap();
    }
    // Written again as a format 3 piece without namespaces: every compact key in full.
    let current = dir.join(format!(
        "checkpoint-{}",
        std::fs::read_to_string(dir.join("CURRENT")).unwrap().trim()
    ));
    let u64s = |b: &[u8]| -> Vec<u64> {
        b.chunks(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect()
    };
    let ns_file = std::fs::read(current.join("namespaces.bin")).unwrap();
    let count = u64s(&ns_file[8..16])[0] as usize;
    let ns_ends = u64s(&ns_file[24..24 + 8 * (count + 1)]);
    let ns_bytes = &ns_file[24 + 8 * (count + 1)..];
    let dict = std::fs::read(current.join("dict.bin")).unwrap();
    assert_eq!(&dict[..8], b"OXGSDIC6");
    let h = u64s(&dict[8..24]);
    let (len, n) = (h[0] as usize, h[1] as usize);
    let bytes = &dict[24..24 + len];
    let at = (24 + len).next_multiple_of(8);
    let ends = u64s(&dict[at..at + 8 * n]);
    let (mut full, mut full_ends) = (Vec::new(), vec![0u64]);
    let mut compact = 0;
    for w in ends.windows(2) {
        let key = &bytes[w[0] as usize..w[1] as usize];
        if key.len() > 2 && key[0] == 1 && key[1] == 0 {
            let (mut i, mut shift, mut at) = (0usize, 0, 2);
            loop {
                let b = key[at];
                at += 1;
                i |= usize::from(b & 0x7f) << shift;
                shift += 7;
                if b < 0x80 {
                    break;
                }
            }
            full.push(1);
            full.extend_from_slice(&ns_bytes[ns_ends[i] as usize..ns_ends[i + 1] as usize]);
            full.extend_from_slice(&key[at..]);
            compact += 1;
        } else {
            full.extend_from_slice(key);
        }
        full_ends.push(full.len() as u64);
    }
    assert!(compact >= 90, "{compact} compact keys");
    let mut file = b"OXGSDIC3".to_vec();
    file.extend_from_slice(&(full.len() as u64).to_le_bytes());
    file.extend_from_slice(&(full_ends.len() as u64).to_le_bytes());
    file.extend_from_slice(&full);
    file.resize(file.len().next_multiple_of(8), 0);
    for e in &full_ends {
        file.extend_from_slice(&e.to_le_bytes());
    }
    std::fs::write(current.join("dict.bin"), &file).unwrap();
    std::fs::remove_file(current.join("namespaces.bin")).unwrap();
    // The saved index holds hashes of full keys and ordinals: still right.
    let sums: String = std::fs::read_to_string(current.join("checksums"))
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with("namespaces.bin "))
        .map(|l| {
            if l.starts_with("dict.bin ") {
                format!("dict.bin {} {:08x}\n", file.len(), crc32fast::hash(&file))
            } else {
                format!("{l}\n")
            }
        })
        .collect();
    std::fs::write(current.join("checksums"), sums).unwrap();
    let new: Vec<Quad> = (50..100)
        .map(|i| {
            quad(
                format!("http://example.com/way/{i}"),
                format!("http://example.com/way/{i}/geom/track"),
            )
        })
        .collect();
    let check = |store: &Store, quads: &[Quad]| {
        for q in quads {
            assert!(store.contains(q.as_ref()), "{q}");
            for t in [q.subject.clone().into(), q.object.clone()] {
                let id = store.term_id(t.as_ref()).expect("found by its text");
                assert_eq!(store.snapshot().term(id).unwrap(), t);
            }
        }
    };
    {
        let store = Store::open_read_only(&dir).unwrap();
        check(&store, &old);
    }
    {
        let store = Store::open(&dir).unwrap();
        check(&store, &old);
        let before = store.dictionary_usage();
        for q in &old {
            assert!(!store.insert(q.as_ref()).unwrap(), "{q} is there");
        }
        assert_eq!(store.dictionary_usage(), before, "no key is added again");
        for q in &new {
            store.insert(q.as_ref()).unwrap();
        }
        check(&store, &old);
        check(&store, &new);
        store.checkpoint().unwrap();
    }
    let current = dir.join(format!(
        "checkpoint-{}",
        std::fs::read_to_string(dir.join("CURRENT")).unwrap().trim()
    ));
    let magic = |f: &str| std::fs::read(current.join(f)).unwrap()[..8].to_vec();
    assert_eq!(magic("dict.bin"), b"OXGSDIC3");
    assert_eq!(magic("dict-1.bin"), b"OXGSDIC6");
    assert!(current.join("namespaces.bin").exists());
    let store = Store::open_read_only(&dir).unwrap();
    assert_eq!(store.len(), old.len() + new.len());
    check(&store, &old);
    check(&store, &new);
    let iri = store
        .dictionary_usage()
        .into_iter()
        .find(|u| u.kind == "iri")
        .unwrap();
    // The old IRIs stay in full beside the new ones that refer to a namespace.
    assert!(iri.namespace_bytes > 0, "{iri:?}");
}

/// A term a writer commits while a view is open is not stored for that view, which holds no
/// quad with it: every way the view gives the term an id gives it the same one made up for the
/// view (#11), so a query computing it in chunks, or meeting it as a constant, sees one term.
#[test]
fn a_term_committed_after_a_view_is_one_made_up_term_in_it() {
    let store = Store::new();
    let ex = |s: &str| oxigraph::model::NamedNode::new(format!("http://example.com/{s}")).unwrap();
    store
        .insert(QuadRef::new(
            &ex("a"),
            &ex("p"),
            &Literal::new_simple_literal("old value"),
            GraphNameRef::DefaultGraph,
        ))
        .unwrap();
    let view = store.snapshot();
    let old = Term::from(Literal::new_simple_literal("old value"));
    let new = Term::from(Literal::new_simple_literal("a value committed later"));
    // Subject and predicate are stored already: the new literal is the first key after the
    // view's last (`holds` at its bound).
    store
        .insert(QuadRef::new(
            &ex("a"),
            &ex("p"),
            new.as_ref(),
            GraphNameRef::DefaultGraph,
        ))
        .unwrap();
    let stored = view.id_of(old.clone());
    assert_ne!(stored.kind(), Kind::Ephemeral);
    assert_eq!(view.term_id(new.as_ref()), None);
    let made = view.id_of(new.clone());
    assert_eq!(made.kind(), Kind::Ephemeral);
    let ids = view.finish_ids(view.prepare_ids([new.clone(), old.clone()]));
    assert_eq!(ids, vec![made, stored]);
    assert_eq!(view.term(made).unwrap(), new);
    // A view taken after the write stores it.
    let later = store.snapshot();
    let id = later.id_of(new.clone());
    assert_ne!(id.kind(), Kind::Ephemeral);
    assert_eq!(later.term_id(new.as_ref()), Some(id));
    assert_eq!(later.finish_ids(later.prepare_ids([new])), vec![id]);
}

/// Row filters hide the triple terms whose triple they hide (enterprise #9): the triple terms
/// are found in the dictionary, also those added later, and hiding one hides the triples that
/// name it, such as `rdf:reifies`.
#[test]
fn row_filters_hide_triple_terms_of_hidden_triples() {
    use tripletrail_store::RowFilter;
    let store = Store::new();
    store
        .bulk_load(
            RdfFormat::TriG,
            br#"@prefix ex: <http://example.com/> .
            ex:a ex:name "A" ~ ex:r1 {| ex:source ex:x |} ; ex:salary 5 ~ ex:r2 {| ex:source ex:y |} .
            ex:b ex:name "B" .
            ex:n1 ex:about <<( ex:b ex:name "B" )>> .
            ex:n2 ex:about <<( ex:c ex:says <<( ex:a ex:name "A" )>> )>> ."#
                .as_slice(),
        )
        .unwrap();
    let snapshot = store.snapshot();
    let terms = snapshot.triple_terms();
    // <<(a name A)>> (twice in the data, once in the dictionary), <<(a salary 5)>>,
    // <<(b name B)>>, and the nested one.
    assert_eq!(terms.len(), 4, "{terms:?}");
    assert_eq!(terms.iter().filter(|t| t.nested).count(), 1);
    assert!(Arc::ptr_eq(&terms, &snapshot.triple_terms()), "found once");
    let id = |iri: &str| {
        snapshot
            .term_id(NamedNodeRef::new_unchecked(iri).into())
            .unwrap()
    };
    let ex = |l: &str| format!("http://example.com/{l}");
    let visible = |filter: RowFilter| {
        let view = snapshot.clone().with_rows(Some(Arc::new(filter)));
        let mut objects: Vec<String> = view
            .quads_for_pattern(None, None, None, None)
            .filter(|q| matches!(q.object, Term::Triple(_)))
            .map(|q| format!("{} {}", q.subject, q.object))
            .collect();
        objects.sort();
        objects
    };
    let all = visible(RowFilter::new());
    assert_eq!(all.len(), 4, "{all:?}");
    // ex:b hidden, a hidden property on ex:a, and nested terms hidden whole.
    let mut filter = RowFilter::new();
    filter.hide_resource(id(&ex("b")), true);
    filter.hide_predicate_on(id(&ex("a")), id(&ex("salary")));
    assert_eq!(filter.hide_triple_terms(&terms, true), 3);
    assert_eq!(
        visible(filter),
        [r#"<http://example.com/r1> <<( <http://example.com/a> <http://example.com/name> "A" )>>"#]
    );
    // Without hiding nested ones, the nested term is judged by its outer triple only.
    let mut filter = RowFilter::new();
    filter.hide_resource(id(&ex("b")), true);
    assert_eq!(filter.hide_triple_terms(&terms, false), 1);
    assert_eq!(visible(filter).len(), 3);
    // A triple term added later is found too.
    store
        .insert(QuadRef::new(
            NamedNodeRef::new_unchecked("http://example.com/n3"),
            NamedNodeRef::new_unchecked("http://example.com/about"),
            &Term::Triple(Box::new(oxigraph::model::Triple::new(
                NamedNodeRef::new_unchecked("http://example.com/b"),
                NamedNodeRef::new_unchecked("http://example.com/age"),
                Literal::from(3),
            ))),
            GraphNameRef::DefaultGraph,
        ))
        .unwrap();
    let later = store.snapshot().triple_terms();
    assert_eq!(later.len(), 5);
    assert!(
        later
            .iter()
            .any(|t| t.subject == id(&ex("b")).0 && !t.nested)
    );
}
