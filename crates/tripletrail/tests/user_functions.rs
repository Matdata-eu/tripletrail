//! User-defined functions (#235): SHACL-AF `sh:SPARQLFunction`s called from queries, updates,
//! SHACL-SPARQL constraints and rules.

use tripletrail::Engine;
use tripletrail::oxigraph::io::{RdfFormat, RdfParser};
use tripletrail::oxigraph::model::{GraphNameRef, NamedNode, Term, Triple};
use tripletrail::oxigraph::sparql::QueryResults;
use tripletrail::udf::Functions;

const FUNCTIONS: &str = r#"
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
@prefix ex: <http://example.com/> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .

ex:prefixes sh:declare [ sh:prefix "ex" ; sh:namespace "http://example.com/"^^xsd:anyURI ] ,
    [ sh:prefix "rdfs" ; sh:namespace "http://www.w3.org/2000/01/rdf-schema#"^^xsd:anyURI ] .

# One expression: inlined.
ex:kmToM a sh:SPARQLFunction ;
    sh:parameter [ sh:path ex:km ; sh:datatype xsd:decimal ] ;
    sh:returnType xsd:decimal ;
    sh:select "SELECT ($km * 1000 AS ?m) WHERE {}" .

# Two parameters in sh:order, and a call of another function: inlined too.
ex:between a sh:SPARQLFunction ;
    sh:parameter [ sh:path ex:to ; sh:order 2 ] , [ sh:path ex:from ; sh:order 1 ] ;
    sh:select "SELECT (ABS(<http://example.com/kmToM>($to - $from)) AS ?d) WHERE {}" .

# Inlined, the call sees an unbound argument as unbound.
ex:given a sh:SPARQLFunction ;
    sh:parameter [ sh:path ex:v ] ;
    sh:select "SELECT (BOUND($v) AS ?b) WHERE {}" .

# Reads the data.
ex:labelOf a sh:SPARQLFunction ;
    sh:prefixes ex:prefixes ;
    sh:parameter [ sh:path ex:node ] ;
    sh:select "SELECT ?l WHERE { $node rdfs:label ?l } ORDER BY ?l" .

ex:isStation a sh:SPARQLFunction ;
    sh:prefixes ex:prefixes ;
    sh:parameter [ sh:path ex:node ] ;
    sh:ask "ASK { $node a ex:Station }" .

# One expression that calls a function reading the data.
ex:quotedLabel a sh:SPARQLFunction ;
    sh:parameter [ sh:path ex:node ] ;
    sh:select "SELECT (CONCAT('«', <http://example.com/labelOf>($node), '»') AS ?q) WHERE {}" .

# An optional parameter.
ex:code a sh:SPARQLFunction ;
    sh:parameter [ sh:path ex:value ; sh:order 1 ] , [ sh:path ex:prefix ; sh:order 2 ; sh:optional true ] ;
    sh:select "SELECT (CONCAT(COALESCE($prefix, 'X'), '-', STR($value)) AS ?c) WHERE {}" .
"#;

const DATA: &str = r#"
@prefix ex: <http://example.com/> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
ex:oslo a ex:Station ; rdfs:label "Oslo S" ; ex:km 0.5 .
ex:lillestrom a ex:Station ; rdfs:label "Lillestrøm" ; ex:km 21.25 .
ex:signal ex:km 3 .
"#;

fn triples(turtle: &str) -> Vec<Triple> {
    RdfParser::from_format(RdfFormat::Turtle)
        .for_slice(turtle.as_bytes())
        .map(|q| Triple::from(q.unwrap()))
        .collect()
}

fn engine() -> Engine {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    e.with_functions_from(triples(FUNCTIONS)).unwrap()
}

/// The values of `var` in the rows of `q`, sorted ("" when unbound).
fn column(e: &Engine, q: &str, var: &str) -> Vec<String> {
    let q = format!("PREFIX ex: <http://example.com/>\n{q}");
    let QueryResults::Solutions(s) = e.query(&q).unwrap() else {
        panic!()
    };
    let mut out: Vec<String> = s
        .map(|r| {
            r.unwrap().get(var).map_or(String::new(), |t| match t {
                Term::Literal(l) => l.value().to_owned(),
                t => t.to_string(),
            })
        })
        .collect();
    out.sort();
    out
}

#[test]
fn functions_are_read_with_their_parameters() {
    let f = Functions::from_triples(triples(FUNCTIONS)).unwrap();
    assert_eq!(f.len(), 7);
    let between = f
        .get(&NamedNode::new("http://example.com/between").unwrap())
        .unwrap();
    let names: Vec<&str> = between.parameters.iter().map(|(v, _)| v.as_str()).collect();
    assert_eq!(names, ["from", "to"]);
}

#[test]
fn expression_functions_are_inlined_and_give_the_same_values() {
    let e = engine();
    assert_eq!(
        column(
            &e,
            "SELECT (ex:kmToM(?km) AS ?m) WHERE { ?s ex:km ?km }",
            "m"
        ),
        ["21250.0", "3000", "500.0"]
    );
    assert_eq!(
        column(
            &e,
            "SELECT (ex:between(?a, ?b) AS ?d) WHERE { ex:oslo ex:km ?a . ex:lillestrom ex:km ?b }",
            "d"
        ),
        ["20750.0"]
    );
    // Inlined, the call sees an unbound argument (a call by name would get no value).
    assert_eq!(
        column(
            &e,
            "SELECT (ex:given(?nothing) AS ?b) (ex:given(1) AS ?c) {}",
            "b"
        ),
        ["false"]
    );
    // In a FILTER and an ORDER BY too.
    assert_eq!(
        column(
            &e,
            "SELECT ?s WHERE { ?s ex:km ?km FILTER(ex:kmToM(?km) > 1000) } ORDER BY ex:kmToM(?km)",
            "s"
        ),
        [
            "<http://example.com/lillestrom>",
            "<http://example.com/signal>"
        ]
    );
}

#[test]
fn query_functions_read_the_data() {
    let e = engine();
    assert_eq!(
        column(
            &e,
            "SELECT (ex:labelOf(?s) AS ?l) WHERE { ?s ex:km ?km }",
            "l"
        ),
        ["", "Lillestrøm", "Oslo S"]
    );
    assert_eq!(
        column(
            &e,
            "SELECT ?s WHERE { ?s ex:km ?km FILTER(ex:isStation(?s)) }",
            "s"
        ),
        [
            "<http://example.com/lillestrom>",
            "<http://example.com/oslo>"
        ]
    );
    // The function sees writes made after it was loaded.
    e.update("PREFIX ex: <http://example.com/> INSERT DATA { ex:signal a ex:Station }")
        .unwrap();
    assert_eq!(
        column(
            &e,
            "SELECT ?s WHERE { ?s ex:km ?km FILTER(ex:isStation(?s)) }",
            "s"
        )
        .len(),
        3
    );
}

#[test]
fn optional_parameters_and_wrong_argument_counts() {
    let e = engine();
    assert_eq!(
        column(
            &e,
            "SELECT (ex:code(7, 'N') AS ?a) (ex:code(7) AS ?b) (ex:kmToM(1, 2) AS ?c) {}",
            "a"
        ),
        ["N-7"]
    );
    assert_eq!(column(&e, "SELECT (ex:code(7) AS ?b) {}", "b"), ["X-7"]);
    assert_eq!(column(&e, "SELECT (ex:kmToM(1, 2) AS ?c) {}", "c"), [""]);
    assert_eq!(column(&e, "SELECT (ex:kmToM() AS ?c) {}", "c"), [""]);
}

#[test]
fn functions_work_in_updates() {
    let e = engine();
    e.update(
        "PREFIX ex: <http://example.com/>
         INSERT { ?s ex:m ?m ; ex:name ?l } WHERE { ?s ex:km ?km BIND(ex:kmToM(?km) AS ?m)
                                                   BIND(ex:labelOf(?s) AS ?l) }",
    )
    .unwrap();
    assert_eq!(
        column(&e, "SELECT ?m WHERE { ?s ex:m ?m }", "m"),
        ["21250.0", "3000", "500.0"]
    );
    assert_eq!(
        column(&e, "SELECT ?n WHERE { ?s ex:name ?n }", "n").len(),
        2
    );
}

#[test]
fn recursion_and_bad_declarations_are_refused() {
    let looped = r#"
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.com/> .
ex:a a sh:SPARQLFunction ; sh:parameter [ sh:path ex:x ] ;
    sh:select "SELECT (<http://example.com/b>($x) AS ?r) WHERE {}" .
ex:b a sh:SPARQLFunction ; sh:parameter [ sh:path ex:x ] ;
    sh:select "SELECT (<http://example.com/a>($x) AS ?r) WHERE {}" .
"#;
    let err = Functions::from_triples(triples(looped)).unwrap_err();
    assert!(err.to_string().contains("may not call itself"), "{err}");
    for bad in [
        r#"ex:f a sh:SPARQLFunction ."#,
        r#"ex:f a sh:SPARQLFunction ; sh:select "SELECT WHERE" ."#,
        r#"ex:f a sh:SPARQLFunction ; sh:ask "SELECT ?x {}" ."#,
        r#"ex:f a sh:SPARQLFunction ; sh:parameter [ sh:path ex:x ; sh:optional true ] , [ sh:path ex:y ] ; sh:select "SELECT ($x AS ?r) {}" ."#,
    ] {
        let text = format!(
            "@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://example.com/> . {bad}"
        );
        assert!(Functions::from_triples(triples(&text)).is_err(), "{bad}");
    }
}

#[test]
fn constraints_and_rules_call_the_functions_of_their_shapes_graph() {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    let shapes = NamedNode::new("http://example.com/shapes").unwrap();
    let shapes_text = format!(
        r#"{FUNCTIONS}
ex:Far a sh:NodeShape ; sh:targetSubjectsOf ex:km ;
    sh:sparql [ sh:prefixes ex:prefixes ; sh:select """
        SELECT $this WHERE {{ $this ex:km ?km FILTER(<http://example.com/kmToM>(?km) > 10000) }}""" ] .
ex:Named a sh:NodeShape ; sh:targetSubjectsOf ex:km ;
    sh:rule [ a sh:SPARQLRule ; sh:prefixes ex:prefixes ; sh:construct """
        CONSTRUCT {{ $this ex:display ?l }} WHERE {{ BIND(ex:labelOf($this) AS ?l) }}""" ] .
"#
    );
    e.load_into(shapes.clone(), RdfFormat::Turtle, shapes_text.as_bytes())
        .unwrap();
    let mut report = Vec::new();
    let conforms = e
        .validate(&shapes, GraphNameRef::DefaultGraph, &mut |t| report.push(t))
        .unwrap();
    assert!(!conforms);
    let focus: Vec<String> = report
        .iter()
        .filter(|t| t.predicate.as_str() == "http://www.w3.org/ns/shacl#focusNode")
        .map(|t| t.object.to_string())
        .collect();
    assert_eq!(focus, ["<http://example.com/lillestrom>"]);
    let inferred = e
        .infer(&shapes, GraphNameRef::DefaultGraph, None, 4)
        .unwrap();
    let mut labels: Vec<String> = inferred.iter().map(|t| t.object.to_string()).collect();
    labels.sort();
    assert_eq!(labels, ["\"Lillestrøm\"", "\"Oslo S\""]);
    // The functions belong to that run only.
    assert!(e.functions().is_empty());
}

#[test]
fn callers_with_access_to_some_graphs_only_get_expression_functions() {
    let e = engine();
    let options = || tripletrail::QueryOptions {
        readable: Some(tripletrail::ReadableGraphs {
            default_graph: true,
            named: Vec::new(),
        }),
        ..Default::default()
    };
    // A body that reads the data would read graphs the caller may not.
    let err = e
        .query_with_options(
            "PREFIX ex: <http://example.com/> SELECT (ex:labelOf(ex:oslo) AS ?l) {}",
            options(),
        )
        .err()
        .unwrap();
    assert!(err.to_string().contains("User-defined functions"), "{err}");
    // So would one inlined into the call of another.
    let q = "PREFIX ex: <http://example.com/> SELECT (ex:quotedLabel(ex:oslo) AS ?l) {}";
    let err = e.query_with_options(q, options()).err().unwrap();
    assert!(err.to_string().contains("User-defined functions"), "{err}");
    assert_eq!(column(&e, q, "l"), ["«Oslo S»"]);
    // An inlined one is an expression like any other.
    let QueryResults::Solutions(mut s) = e
        .query_with_options(
            "PREFIX ex: <http://example.com/> SELECT (ex:kmToM(2) AS ?m) {}",
            options(),
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        s.next().unwrap().unwrap().get("m").unwrap().to_string(),
        "\"2000\"^^<http://www.w3.org/2001/XMLSchema#integer>"
    );
}

#[test]
fn a_cancelled_body_fails_the_query() {
    let token = tripletrail::oxigraph::sparql::CancellationToken::new();
    let e = engine().with_cancellation_token(token.clone());
    token.cancel();
    let result = e
        .query("PREFIX ex: <http://example.com/> SELECT (ex:labelOf(ex:oslo) AS ?l) {}")
        .and_then(|r| match r {
            QueryResults::Solutions(s) => s
                .collect::<Result<Vec<_>, _>>()
                .map(|_| ())
                .map_err(Into::into),
            _ => Ok(()),
        });
    assert!(
        result.is_err(),
        "the query should end with the cancellation"
    );
}
