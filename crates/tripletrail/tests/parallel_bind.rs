//! Chains of costly `BIND`s in update `WHERE`s, computed on every core (#292), give the rows
//! the evaluator gives row by row: compared with the same `WHERE` under a `LIMIT`, which leaves
//! the chains to the evaluator.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

/// Enough rows for the chains to be split into chunks.
const SUBJECTS: usize = 9000;

fn data() -> String {
    let mut ttl = String::from(
        "@prefix ex: <http://example.org/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
    );
    for i in 0..SUBJECTS {
        let s = format!("ex:s{i}");
        ttl.push_str(&format!(
            "{s} a ex:C{} ; ex:x \"{}.{:03}\"^^xsd:decimal ; ex:y {} .\n",
            i % 3,
            500_000 + i * 7,
            i % 1000,
            6_600_000 + i * 3
        ));
        if i % 4 != 0 {
            ttl.push_str(&format!("{s} ex:z \"{}.5\"^^xsd:decimal .\n", i % 900));
        }
        if i % 5 == 0 {
            ttl.push_str(&format!("{s} ex:name \"Name {i}\"@en .\n"));
        }
        ttl.push_str(&format!(
            "{s} ex:v \"v{i}\" ; ex:q \"{}{i}\" .\n",
            if i % 2 == 0 { "w" } else { "z" }
        ));
        if i % 7 == 0 {
            // Blank nodes in the rows under a chain: VALUES can't hold them.
            ttl.push_str(&format!("[] ex:v \"b{i}\" .\n"));
        }
        if i % 11 == 0 {
            ttl.push_str(&format!("{s} a ex:Skip .\n"));
        }
    }
    ttl
}

fn rows(engine: &Engine, query: &str) -> Vec<String> {
    let mut out: Vec<String> = match engine.query(query).unwrap() {
        QueryResults::Solutions(solutions) => solutions
            .map(|s| {
                let s = s.unwrap();
                let mut cells: Vec<String> = s.iter().map(|(v, t)| format!("{v}={t}")).collect();
                cells.sort();
                cells.join(" ")
            })
            .collect(),
        QueryResults::Graph(triples) => triples.map(|t| t.unwrap().to_string()).collect(),
        QueryResults::Boolean(b) => vec![b.to_string()],
    };
    out.sort();
    out
}

const PREFIXES: &str = "PREFIX ex: <http://example.org/> \
    PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> \
    PREFIX geo: <http://www.opengis.net/ont/geosparql#> \
    PREFIX spatialf: <http://jena.apache.org/function/spatial#> ";

/// The rail KG GPS update's chain: WKT made with CONCAT, moved to WGS 84, and coordinates read
/// back with REPLACE.
const GPS_CHAIN: &str = r#"
    BIND(STRDT(CONCAT("<http://www.opengis.net/def/crs/EPSG/0/25832> POINT Z(", STR(?x), " ", STR(?y), " ", STR(?z), ")"), geo:wktLiteral) AS ?w)
    BIND(STRDT(REPLACE(STR(spatialf:transformSRS(?w, "http://www.opengis.net/def/crs/EPSG/0/4326")), "([A-Z]+) ZM", "$1 Z"), geo:wktLiteral) AS ?t)
    BIND(xsd:decimal(REPLACE(STR(?t), "^.*POINT Z\\(([0-9\\.\\-]+) .*", "$1")) AS ?lat)
    BIND(xsd:decimal(REPLACE(STR(?t), "^.*POINT Z\\([0-9\\.\\-]+ ([0-9\\.\\-]+) .*", "$1")) AS ?lon)
    BIND(IRI(CONCAT(STR(ex:), "gps_", STR(?lat), "_", STR(?lon))) AS ?iri)
"#;

fn queries() -> Vec<String> {
    let gps = format!(
        "SELECT ?s ?t ?lat ?lon ?iri WHERE {{ ?s ex:x ?x ; ex:y ?y ; ex:z ?z . FILTER NOT EXISTS {{ ?s a ex:Skip }} {GPS_CHAIN} }}"
    );
    vec![
        gps.clone(),
        // The chain's inputs too.
        format!("SELECT ?s ?x ?y ?z ?w ?t ?lat ?lon ?iri WHERE {{ ?s ex:x ?x ; ex:y ?y ; ex:z ?z . {GPS_CHAIN} }}"),
        // Two branches, one of them without Z (the expressions give errors: unbound).
        format!(
            "SELECT ?s ?lat ?iri WHERE {{ {{ ?s ex:x ?x ; ex:y ?y ; ex:z ?z . {GPS_CHAIN} }} UNION {{ ?s ex:x ?x ; ex:y ?y FILTER NOT EXISTS {{ ?s ex:z ?any }} {GPS_CHAIN} }} }}"
        ),
        // Unbound inputs from OPTIONAL, and a language string.
        r#"SELECT ?s ?a ?b WHERE { ?s ex:x ?x OPTIONAL { ?s ex:name ?n } BIND(REPLACE(?n, "Name ", "N-") AS ?a) BIND(CONCAT(?a, "/", STR(?x)) AS ?b) }"#.into(),
        // A filter on what the chain binds stays above it.
        r#"SELECT ?s ?a WHERE { ?s ex:x ?x BIND(REPLACE(STR(?x), "\\.", ",") AS ?a) FILTER(STRSTARTS(?a, "50")) }"#.into(),
        // EXISTS and BOUND in the chain.
        r#"SELECT ?s ?a ?e WHERE { ?s ex:x ?x OPTIONAL { ?s ex:z ?z } BIND(REPLACE(STR(?x), "0", "o") AS ?a) BIND(IF(EXISTS { ?s a ex:Skip }, "skip", IF(BOUND(?z), "z", "-")) AS ?e) }"#.into(),
        // Grouped and ordered over the chain.
        r#"SELECT ?k (COUNT(*) AS ?n) (MIN(?a) AS ?lo) (SAMPLE(?k) AS ?any) WHERE { ?s ex:x ?x BIND(REPLACE(STR(?x), "^(..).*$", "$1") AS ?k) BIND(REGEX(STR(?x), "5$") AS ?a) } GROUP BY ?k"#.into(),
        r#"SELECT ?s ?a WHERE { ?s ex:x ?x BIND(REPLACE(STR(?x), "^5", "five") AS ?a) } ORDER BY ?a ?s"#.into(),
        r#"SELECT DISTINCT ?a WHERE { ?s ex:x ?x BIND(REPLACE(STR(?x), "^(...).*$", "$1") AS ?a) }"#.into(),
        // Blank nodes in the chain: evaluated as usual.
        r#"SELECT ?s (isBlank(?b) AS ?blank) WHERE { ?s ex:x ?x BIND(REPLACE(STR(?x), "0", "o") AS ?a) BIND(BNODE(?a) AS ?b) }"#.into(),
        // Filters whose EXISTS reads what the chain binds, only in a filter inside it: they
        // stay above the chain (#591 review), with costly and cheap chains.
        r#"SELECT ?s ?a WHERE { ?s ex:v ?x BIND(REPLACE(?x, "v", "w") AS ?a) FILTER NOT EXISTS { ?s ex:q ?o FILTER(?o = ?a) } }"#.into(),
        r#"SELECT ?s ?a WHERE { ?s ex:v ?x BIND(REPLACE(?x, "v", "w") AS ?a) FILTER EXISTS { ?s ex:q ?o FILTER(?o = ?a) } }"#.into(),
        r#"SELECT ?s ?a WHERE { ?s ex:v ?x BIND(CONCAT("w", SUBSTR(?x, 2)) AS ?a) FILTER EXISTS { ?s ex:q ?o FILTER(?o = ?a) } }"#.into(),
        r#"SELECT ?s ?a WHERE { ?s ex:v ?x BIND(REPLACE(?x, "v", "w") AS ?a) FILTER EXISTS { ?s ex:q ?o BIND(?a AS ?b) FILTER(?o = ?b) } }"#.into(),
        // ORDER BY with an EXISTS reading a variable the projection drops.
        r#"SELECT ?s WHERE { ?s ex:v ?x BIND(REPLACE(?x, "v", "w") AS ?a) } ORDER BY (EXISTS { ?s ex:q ?a }) ?s"#.into(),
        // Blank nodes in the rows under the chain.
        r#"SELECT ?s ?a WHERE { ?s ex:v ?x BIND(REPLACE(?x, "^b", "B") AS ?a) }"#.into(),
        // Only a few rows: evaluated over the rows, without chunks.
        r#"SELECT ?s ?a WHERE { ?s ex:name ?n FILTER(?s = ex:s5 || ?s = ex:s10) BIND(REPLACE(?n, "Name", "Nom") AS ?a) }"#.into(),
    ]
}

/// Three more costly calls, one an extension function, in the chain of `q` (before its first
/// `BIND`), so that it is computed in chunks.
fn padded(q: &str) -> String {
    if q.contains("transformSRS") {
        return q.to_owned();
    }
    q.replacen(
        "BIND(",
        r#"BIND(REPLACE(REPLACE(STR(spatialf:transformSRS("POINT(1 2)"^^geo:wktLiteral, "http://www.opengis.net/def/crs/EPSG/0/4326")), "x", "x"), "/", "/") AS ?pad) BIND("#,
        1,
    )
}

/// The rows an update made from the query inserted (`_:row <urn:v:name> ?name` per projected
/// variable), each as its sorted (variable, value) pairs.
fn inserted(engine: &Engine, select: &str, graph: &str) -> Vec<String> {
    let at = select.find(" WHERE").unwrap();
    let mut vars: Vec<&str> = select[..at]
        .split(|c: char| !(c.is_alphanumeric() || c == '?' || c == '_'))
        .filter(|t| t.starts_with('?'))
        .collect();
    vars.sort_unstable();
    vars.dedup();
    let template: String = vars
        .iter()
        .map(|v| format!("_:row <urn:v:{}> {v} .", &v[1..]))
        .collect();
    let update =
        format!("{PREFIXES} INSERT {{ GRAPH <{graph}> {{ {template} }} }} WHERE {{ {select} }}");
    engine.update(&update).unwrap();
    let QueryResults::Solutions(solutions) = engine
        .query(&format!(
            "SELECT ?r ?p ?o WHERE {{ GRAPH <{graph}> {{ ?r ?p ?o }} }}"
        ))
        .unwrap()
    else {
        panic!()
    };
    let mut by_row: std::collections::HashMap<String, Vec<String>> = Default::default();
    for s in solutions {
        let s = s.unwrap();
        by_row
            .entry(s.get("r").unwrap().to_string())
            .or_default()
            .push(format!("{}={}", s.get("p").unwrap(), s.get("o").unwrap()));
    }
    let mut out: Vec<String> = by_row
        .into_values()
        .map(|mut r| {
            r.sort();
            r.join(" ")
        })
        .collect();
    out.sort();
    out
}

#[test]
fn update_wheres_give_the_rows_the_evaluator_gives() {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    let before = tripletrail::parallel_bind_stats();
    for (i, q) in queries().into_iter().enumerate() {
        let q = padded(&q);
        let got = inserted(&engine, &q, &format!("urn:got:{i}"));
        // A LIMIT above the chain leaves it to the evaluator.
        let reference = inserted(
            &engine,
            &format!("{q} LIMIT 100000000"),
            &format!("urn:ref:{i}"),
        );
        assert!(!got.is_empty(), "{q}");
        assert_eq!(got, reference, "{q}");
    }
    // The counters say what became of the chains (other tests may add to them too).
    let after = tripletrail::parallel_bind_stats();
    assert!(after.taken > before.taken, "{before:?} {after:?}");
    assert!(
        after.blank_nodes > before.blank_nodes,
        "{before:?} {after:?}"
    );
}

#[test]
fn updates_insert_what_the_evaluator_gives() {
    let ours = Engine::new_tripletrail();
    let reference = Engine::new_tripletrail();
    for e in [&ours, &reference] {
        e.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    }
    // `w` wraps a WHERE: in a subquery under a LIMIT for the reference, which leaves the
    // chains to the evaluator.
    let updates = |w: &dyn Fn(&str) -> String| {
        let flat = r#"BIND(REPLACE(STR(?x), "\\.", "_") AS ?lat) BIND(IRI(CONCAT(STR(ex:), "flat_", ?lat)) AS ?iri)"#;
        [
            format!(
                "{PREFIXES} INSERT {{ GRAPH ex:out {{ ?s ex:at ?iri . ?iri ex:lat ?lat ; ex:lon ?lon ; geo:asWKT ?t }} }} WHERE {}",
                w(&format!(
                    "{{ ?s ex:x ?x ; ex:y ?y ; ex:z ?z . FILTER NOT EXISTS {{ ?s a ex:Skip }} {GPS_CHAIN} }} UNION {{ ?s ex:x ?x ; ex:y ?y FILTER NOT EXISTS {{ ?s ex:z ?any }} {flat} }}"
                ))
            ),
            // A second operation reads what the first inserted; DELETE with INSERT.
            format!(
                "{PREFIXES} INSERT {{ GRAPH ex:out {{ ?s ex:tag ?a }} }} WHERE {} ; \
                 DELETE {{ GRAPH ex:out {{ ?s ex:tag ?a }} }} INSERT {{ GRAPH ex:out {{ ?s ex:tag2 ?b }} }} WHERE {}",
                w(r#"?s ex:x ?x BIND(REPLACE(STR(?x), "0", "o") AS ?a)"#),
                w(r#"GRAPH ex:out { ?s ex:tag ?a } BIND(REPLACE(?a, "o", "0") AS ?b)"#),
            ),
        ]
    };
    for u in updates(&|p| format!("{{ {p} }}")) {
        ours.update(&u).unwrap();
    }
    for u in updates(&|p| format!("{{ {{ SELECT * WHERE {{ {p} }} LIMIT 100000000 }} }}")) {
        reference.update(&u).unwrap();
    }
    let all = "SELECT * WHERE { GRAPH ?g { ?s ?p ?o } }";
    let got = rows(&ours, all);
    assert!(got.len() > SUBJECTS * 3, "{}", got.len());
    assert_eq!(got, rows(&reference, all));
    assert!(got.iter().any(|r| r.contains("tag2")));
    assert!(!got.iter().any(|r| r.contains("ex:tag>")));
}
