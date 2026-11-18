//! Property functions for RDF lists, containers and string splitting, with Jena's IRIs
//! (#232).

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::model::Term;
use tripletrail::oxigraph::sparql::QueryResults;

const PREFIXES: &str = r#"
PREFIX ex: <http://example.com/>
PREFIX list: <http://jena.apache.org/ARQ/list#>
PREFIX apf: <http://jena.apache.org/ARQ/property#>
PREFIX tt: <http://w3id.org/tripletrail/fn#>
PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
"#;

const DATA: &str = r#"
@prefix ex: <http://example.com/> .
@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
ex:shape ex:in ( "a" "b" "c" ) .
ex:other ex:in ( "x" ) .
ex:empty ex:in () .
ex:line ex:tracks "L1;L2;;L3;;" ; ex:id "A-1" .
ex:line2 ex:tracks "M1" .
ex:bag a rdf:Bag ; rdf:_1 ex:p ; rdf:_2 ex:q .
ex:seq a rdf:Seq ; rdf:_1 ex:r .
ex:thing rdfs:member ex:asserted .
"#;

fn engine() -> Engine {
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    e
}

fn rows(e: &Engine, q: &str) -> Vec<Vec<String>> {
    let QueryResults::Solutions(s) = e.query(&format!("{PREFIXES}{q}")).unwrap() else {
        panic!()
    };
    let vars = s.variables().to_vec();
    let mut out: Vec<Vec<String>> = s
        .map(|r| {
            let r = r.unwrap();
            vars.iter()
                .map(|v| match r.get(v) {
                    Some(Term::Literal(l)) => l.value().to_owned(),
                    Some(Term::NamedNode(n)) => n
                        .as_str()
                        .trim_start_matches("http://example.com/")
                        .to_owned(),
                    Some(other) => other.to_string(),
                    None => "-".to_owned(),
                })
                .collect()
        })
        .collect();
    out.sort();
    out
}

fn r(v: &[&[&str]]) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = v
        .iter()
        .map(|r| r.iter().map(|s| s.to_string()).collect())
        .collect();
    out.sort();
    out
}

#[test]
fn list_members_indexes_and_lengths() {
    let e = engine();
    assert_eq!(
        rows(&e, "SELECT ?s ?m WHERE { ?s ex:in ?l . ?l list:member ?m }"),
        r(&[
            &["other", "x"],
            &["shape", "a"],
            &["shape", "b"],
            &["shape", "c"]
        ])
    );
    assert_eq!(
        rows(
            &e,
            "SELECT ?i ?m WHERE { ex:shape ex:in ?l . ?l list:index (?i ?m) }"
        ),
        r(&[&["0", "a"], &["1", "b"], &["2", "c"]])
    );
    assert_eq!(
        rows(&e, "SELECT ?s ?n WHERE { ?s ex:in ?l . ?l list:length ?n }"),
        r(&[&["empty", "0"], &["other", "1"], &["shape", "3"]])
    );
    // A bound member: only its index.
    assert_eq!(
        rows(
            &e,
            r#"SELECT ?i WHERE { ex:shape ex:in ?l . ?l list:index (?i "b") }"#
        ),
        r(&[&["1"]])
    );
    // The tt: aliases.
    assert_eq!(
        rows(
            &e,
            "SELECT ?n WHERE { ex:other ex:in ?l . ?l tt:listLength ?n }"
        ),
        r(&[&["1"]])
    );
}

#[test]
fn strings_split_into_rows() {
    let e = engine();
    assert_eq!(
        rows(
            &e,
            r#"SELECT ?s ?part WHERE { ?s ex:tracks ?t . ?part apf:strSplit (?t ";") }"#
        ),
        r(&[
            &["line", ""],
            &["line", "L1"],
            &["line", "L2"],
            &["line", "L3"],
            &["line2", "M1"]
        ])
    );
    // The separator is a regular expression; the arguments may come from a BIND.
    assert_eq!(
        rows(
            &e,
            r#"SELECT ?part WHERE { ex:line ex:id ?id BIND(CONCAT(?id, "-2") AS ?x) ?part apf:strSplit (?x "-+") }"#
        ),
        r(&[&["1"], &["2"], &["A"]])
    );
    // More parts than one VALUES block holds.
    let long: Vec<String> = (0..2000).map(|i| i.to_string()).collect();
    let q = format!(
        r#"SELECT (COUNT(*) AS ?n) (MAX(xsd:integer(?p)) AS ?m) WHERE {{ ?p apf:strSplit ("{}" ",") }}"#,
        long.join(",")
    );
    assert_eq!(
        rows(
            &e,
            &format!("PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> {q}")
        ),
        r(&[&["2000", "1999"]])
    );
}

#[test]
fn containers_and_scalars() {
    let e = engine();
    assert_eq!(
        rows(&e, "SELECT ?c ?m WHERE { ?c tt:member ?m }"),
        r(&[
            &["bag", "p"],
            &["bag", "q"],
            &["seq", "r"],
            &["thing", "asserted"]
        ])
    );
    // A plain rdfs:member triple is only the asserted triples.
    assert_eq!(
        rows(&e, "SELECT ?c ?m WHERE { ?c rdfs:member ?m }"),
        r(&[&["thing", "asserted"]])
    );
    assert_eq!(
        rows(&e, "SELECT ?c ?m WHERE { ?c apf:bag ?m }"),
        r(&[&["bag", "p"], &["bag", "q"]])
    );
    assert_eq!(
        rows(
            &e,
            "SELECT ?ns ?ln WHERE { ex:line apf:splitIRI (?ns ?ln) }"
        ),
        r(&[&["http://example.com/", "line"]])
    );
    assert_eq!(
        rows(
            &e,
            r#"SELECT ?x WHERE { ex:line ex:id ?id . ?x apf:concat ("id=" ?id) }"#
        ),
        r(&[&["id=A-1"]])
    );
    assert_eq!(
        rows(&e, r#"SELECT ?x WHERE { ?x apf:assign "v" }"#),
        r(&[&["v"]])
    );
}

#[test]
fn property_functions_inside_optional_exists_and_updates() {
    let e = engine();
    assert_eq!(
        rows(
            &e,
            r#"SELECT ?s ?m WHERE { ?s ex:tracks ?t OPTIONAL { ?m apf:strSplit (?t ";") FILTER(?m = "M1") } }"#
        ),
        r(&[&["line", "-"], &["line2", "M1"]])
    );
    assert_eq!(
        rows(
            &e,
            r#"SELECT ?s WHERE { ?s ex:in ?l FILTER EXISTS { ?l list:member "b" } }"#
        ),
        r(&[&["shape"]])
    );
    e.update(&format!(
        r#"{PREFIXES} INSERT {{ ?s ex:track ?p }} WHERE {{ ?s ex:tracks ?t . ?p apf:strSplit (?t ";") FILTER(?p != "") }}"#
    ))
    .unwrap();
    assert_eq!(
        rows(&e, "SELECT ?p WHERE { ex:line ex:track ?p }"),
        r(&[&["L1"], &["L2"], &["L3"]])
    );
}
