//! SERVICE <x-sparql-anything:…> calls, on the examples of the Facade-X documentation.

use oxigraph::model::{Dataset, Graph};
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use tripletrail_fx::{FxConfig, FxService};

const PREFIXES: &str = "PREFIX xyz: <http://sparql.xyz/facade-x/data/>
PREFIX fx: <http://sparql.xyz/facade-x/ns/>
PREFIX fxe: <http://sparql.xyz/facade-x/engine/>
PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>
PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
";

fn service(config: FxConfig) -> FxService {
    FxService::new(config, SparqlEvaluator::new)
}

fn run(fx: &FxService, query: &str) -> QueryResults<'static> {
    let empty = Box::leak(Box::new(Dataset::new()));
    fx.register(SparqlEvaluator::new())
        .parse_query(&format!("{PREFIXES}{query}"))
        .unwrap()
        .on_queryable_dataset(&*empty)
        .execute()
        .unwrap()
}

/// The rows as `?var=value` strings, in order.
fn rows(fx: &FxService, query: &str) -> Vec<String> {
    let QueryResults::Solutions(solutions) = run(fx, query) else {
        panic!("not a SELECT")
    };
    solutions
        .map(|s| {
            let s = s.unwrap();
            s.iter()
                .map(|(v, t)| format!("?{}={t}", v.as_str()))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

fn graph(fx: &FxService, query: &str) -> Graph {
    let QueryResults::Graph(triples) = run(fx, query) else {
        panic!("not a CONSTRUCT")
    };
    triples.map(Result::unwrap).collect()
}

/// The graph in N-Triples, sorted, with blank nodes renamed in order of appearance.
fn ntriples(g: &Graph) -> Vec<String> {
    let mut lines: Vec<String> = g.iter().map(|t| t.to_string()).collect();
    lines.sort();
    lines
}

fn fx() -> FxService {
    service(FxConfig::default())
}

#[test]
fn json_default_transformation() {
    let g = graph(
        &fx(),
        r#"CONSTRUCT { ?s ?p ?o } WHERE { SERVICE <x-sparql-anything:> {
            fxe:properties fxe:content "{\"stringArg\":\"stringValue\",\"intArg\":1,\"booleanArg\":true,\"nullArg\":null,\"arr\":[0,1]}" ;
                           fxe:media-type "application/json" .
            ?s ?p ?o } }"#,
    );
    assert_eq!(g.len(), 7, "{}", ntriples(&g).join("\n"));
    let text = ntriples(&g).join("\n");
    for expected in [
        "<http://sparql.xyz/facade-x/data/stringArg> \"stringValue\"",
        "<http://sparql.xyz/facade-x/data/intArg> \"1\"^^<http://www.w3.org/2001/XMLSchema#decimal>",
        "<http://sparql.xyz/facade-x/data/booleanArg> \"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>",
        "<http://www.w3.org/1999/02/22-rdf-syntax-ns#_2> \"1\"^^<http://www.w3.org/2001/XMLSchema#decimal>",
        "<http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://sparql.xyz/facade-x/ns/Root>",
    ] {
        assert!(text.contains(expected), "{expected} missing from\n{text}");
    }
    assert!(!text.contains("nullArg"));
}

#[test]
fn json_numbers_keep_their_numerals() {
    // #378: the numeral as written, xsd:double when it has an exponent (as SPARQL Anything).
    let g = graph(
        &fx(),
        r#"CONSTRUCT { ?s ?p ?o } WHERE { SERVICE <x-sparql-anything:> {
            fxe:properties fxe:content "[9.187028266000027, 12.50, 31.26999999999999957, 1e-7, 100]" ;
                           fxe:media-type "application/json" .
            ?s ?p ?o FILTER(isLiteral(?o)) } }"#,
    );
    let text = ntriples(&g).join("\n");
    for expected in [
        "#_1> \"9.187028266000027\"^^<http://www.w3.org/2001/XMLSchema#decimal>",
        "#_2> \"12.50\"^^<http://www.w3.org/2001/XMLSchema#decimal>",
        "#_3> \"31.26999999999999957\"^^<http://www.w3.org/2001/XMLSchema#decimal>",
        "#_4> \"1e-7\"^^<http://www.w3.org/2001/XMLSchema#double>",
        "#_5> \"100\"^^<http://www.w3.org/2001/XMLSchema#decimal>",
    ] {
        assert!(text.contains(expected), "{expected} missing from\n{text}");
    }
}

#[test]
fn iri_containers_under_the_content_root() {
    let g = graph(
        &fx(),
        r#"CONSTRUCT { ?s ?p ?o } WHERE { SERVICE <x-sparql-anything:> {
            fxe:properties fxe:content "{\"name\":\"Vincent\", \"surname\": \"Vega\", \"performer\" : {\"name\": \"John\", \"surname\": \"Travolta\"}}" ;
                           fxe:media-type "application/json" ;
                           fxe:blank-nodes false .
            ?s ?p ?o } }"#,
    );
    let root = "<http://sparql.xyz/facade-x/data/8e6a66944bcc9366cad8377556ea2302>";
    let performer = "<http://sparql.xyz/facade-x/data/8e6a66944bcc9366cad8377556ea2302/performer>";
    let mut expected = vec![
        format!("{root} <http://sparql.xyz/facade-x/data/name> \"Vincent\""),
        format!("{root} <http://sparql.xyz/facade-x/data/performer> {performer}"),
        format!("{root} <http://sparql.xyz/facade-x/data/surname> \"Vega\""),
        format!(
            "{root} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://sparql.xyz/facade-x/ns/Root>"
        ),
        format!("{performer} <http://sparql.xyz/facade-x/data/name> \"John\""),
        format!("{performer} <http://sparql.xyz/facade-x/data/surname> \"Travolta\""),
    ];
    expected.sort();
    assert_eq!(ntriples(&g), expected);
}

#[test]
fn root_option() {
    let g = graph(
        &fx(),
        r#"CONSTRUCT { ?s ?p ?o } WHERE { SERVICE <x-sparql-anything:> {
            fxe:properties fxe:content "Hello World!" ; fxe:root "http://example.org/myRoot" ; fxe:blank-nodes false .
            ?s ?p ?o } }"#,
    );
    assert_eq!(
        ntriples(&g),
        vec![
            "<http://example.org/myRoot> <http://www.w3.org/1999/02/22-rdf-syntax-ns#_1> \"Hello World!\"",
            "<http://example.org/myRoot> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://sparql.xyz/facade-x/ns/Root>",
        ]
    );
}

#[test]
fn entity_from_json_rows() {
    let r = rows(
        &fx(),
        r#"SELECT ?team ?year (fx:entity("http://example.org/", fx:URLEncoder.encode(?team, "UTF-8"), ?year) AS ?entity)
        WHERE { SERVICE <x-sparql-anything:> {
            fxe:properties fxe:content "[{\"team\":\"Golden State Warriors\", \"year\":2015, \"wins\": 67}, {\"team\":\"Golden State Warriors\", \"year\":2016, \"wins\": 73}]" ;
                           fxe:media-type "application/json" .
            ?c xyz:year ?year ; xyz:team ?team } } ORDER BY ?year"#,
    );
    assert_eq!(
        r,
        vec![
            "?team=\"Golden State Warriors\" ?year=\"2015\"^^<http://www.w3.org/2001/XMLSchema#decimal> ?entity=<http://example.org/Golden+State+Warriors2015>",
            "?team=\"Golden State Warriors\" ?year=\"2016\"^^<http://www.w3.org/2001/XMLSchema#decimal> ?entity=<http://example.org/Golden+State+Warriors2016>",
        ]
    );
}

#[test]
fn any_slot_and_cardinal() {
    let r = rows(
        &fx(),
        r#"SELECT ?slot (fx:cardinal(?p) AS ?n) WHERE { SERVICE <x-sparql-anything:> {
            fxe:properties fxe:content "[1,2,3]" ; fxe:media-type "application/json" .
            ?s fx:anySlot ?slot . ?s ?p ?slot } } ORDER BY ?n"#,
    );
    assert_eq!(
        r,
        vec![
            "?slot=\"1\"^^<http://www.w3.org/2001/XMLSchema#decimal> ?n=\"1\"^^<http://www.w3.org/2001/XMLSchema#integer>",
            "?slot=\"2\"^^<http://www.w3.org/2001/XMLSchema#decimal> ?n=\"2\"^^<http://www.w3.org/2001/XMLSchema#integer>",
            "?slot=\"3\"^^<http://www.w3.org/2001/XMLSchema#decimal> ?n=\"3\"^^<http://www.w3.org/2001/XMLSchema#integer>",
        ]
    );
}

#[test]
fn literal_from_text() {
    let r = rows(
        &fx(),
        r#"SELECT (fx:literal(?string, xsd:int) AS ?result) WHERE { SERVICE <x-sparql-anything:> {
            fxe:properties fxe:content "1" . ?s rdf:_1 ?string } }"#,
    );
    // The datatype is kept, as SPARQL Anything has it (#379).
    assert_eq!(
        r,
        vec!["?result=\"1\"^^<http://www.w3.org/2001/XMLSchema#int>"]
    );
}

#[test]
fn options_in_the_iri_and_from_bind() {
    let dir = scratch("iri");
    std::fs::write(
        dir.join("people.json"),
        r#"[{"name":"Ann"},{"name":"Bob"}]"#,
    )
    .unwrap();
    let fx = service(FxConfig {
        fixed_limits: false,
        databases: None,
        base: dir.to_path_buf(),
        allowed: Some(vec![dir.to_path_buf()]),
        remote: Default::default(),
    });
    let q = |service: &str, extra: &str| {
        rows(
            &fx,
            &format!(
                "SELECT ?name WHERE {{ SERVICE <{service}> {{ {extra} ?p xyz:name ?name }} }} ORDER BY ?name"
            ),
        )
    };
    let expected = vec!["?name=\"Ann\"", "?name=\"Bob\""];
    assert_eq!(q("x-sparql-anything:location=people.json", ""), expected);
    assert_eq!(q("x-sparql-anything:people.json", ""), expected);
    assert_eq!(
        q(
            "x-sparql-anything:",
            "BIND(\"people.json\" AS ?f) fxe:properties fxe:location ?f ."
        ),
        expected
    );
    // The deprecated fx: option triples still work.
    assert_eq!(
        q(
            "x-sparql-anything:",
            "fx:properties fx:location \"people.json\" ."
        ),
        expected
    );
}

#[test]
fn named_graph_is_the_root() {
    let r = rows(
        &fx(),
        r#"SELECT DISTINCT ?g WHERE { SERVICE <x-sparql-anything:> {
            fxe:properties fxe:content "{\"a\":1}" ; fxe:media-type "application/json" ; fxe:root "http://ex.org/r" .
            GRAPH ?g { ?s xyz:a ?o } } }"#,
    );
    assert_eq!(r, vec!["?g=<http://ex.org/r>"]);
}

#[test]
fn sources_outside_the_allowed_directories_are_refused() {
    let dir = scratch("sandbox");
    let inside = dir.join("in");
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::write(dir.join("secret.json"), "{\"a\":1}").unwrap();
    let fx = service(FxConfig {
        fixed_limits: false,
        databases: None,
        base: inside.clone(),
        allowed: Some(vec![inside]),
        remote: Default::default(),
    });
    let err = fx
        .register(SparqlEvaluator::new())
        .parse_query(&format!(
            "{PREFIXES}SELECT * WHERE {{ SERVICE <x-sparql-anything:location=../secret.json> {{ ?s ?p ?o }} }}"
        ))
        .unwrap()
        .on_queryable_dataset(&Dataset::new())
        .execute()
        .and_then(|r| match r {
            QueryResults::Solutions(s) => s.collect::<Result<Vec<_>, _>>().map(|_| ()),
            _ => Ok(()),
        })
        .unwrap_err();
    assert!(err.to_string().contains("outside"), "{err}");
}

#[test]
fn other_services_fail() {
    let err = fx()
        .register(SparqlEvaluator::new())
        .parse_query("SELECT * WHERE { SERVICE <http://ex.org/sparql> { ?s ?p ?o } }")
        .unwrap()
        .on_queryable_dataset(&Dataset::new())
        .execute()
        .and_then(|r| match r {
            QueryResults::Solutions(s) => s.collect::<Result<Vec<_>, _>>().map(|_| ()),
            _ => Ok(()),
        })
        .unwrap_err();
    assert!(err.to_string().contains("x-sparql-anything"), "{err}");
}

#[test]
fn only_needed_triples_are_built_but_answers_are_complete() {
    // A variable predicate needs everything; a constant one only its triples. Both answer
    // the same for the constant predicate.
    let content = r#"fxe:properties fxe:content "{\"a\":{\"b\":[1,{\"b\":2}]}}" ; fxe:media-type "application/json" ."#;
    let all = rows(
        &fx(),
        &format!(
            "SELECT ?o WHERE {{ SERVICE <x-sparql-anything:> {{ {content} ?s ?p ?o FILTER(?p = xyz:b) }} }} ORDER BY ?o"
        ),
    );
    let some = rows(
        &fx(),
        &format!(
            "SELECT ?o WHERE {{ SERVICE <x-sparql-anything:> {{ {content} ?s xyz:b ?o }} }} ORDER BY ?o"
        ),
    );
    let unlabel = |r: Vec<String>| -> Vec<String> {
        r.into_iter()
            .map(|s| {
                if s.starts_with("?o=_:") {
                    "?o=_:b".into()
                } else {
                    s
                }
            })
            .collect()
    };
    assert_eq!(all.len(), 2);
    assert_eq!(unlabel(all), unlabel(some));
}

fn scratch(name: &str) -> tripletrail_testdir::TestDir {
    tripletrail_testdir::tempdir(&format!("fx-{name}"))
}
