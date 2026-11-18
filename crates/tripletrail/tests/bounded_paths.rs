//! Bounded property paths (#234): `p{n}`, `p{n,m}`, `p{n,}` and `p{,m}`, as in Jena.

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;

/// A chain a → b → c → d → e, a shortcut a → c, and a cycle x → y → x; `ex:q` edges in graph g.
const DATA: &str = r#"
@prefix ex: <http://example.com/> .
ex:a ex:next ex:b ; ex:next ex:c .
ex:b ex:next ex:c . ex:c ex:next ex:d . ex:d ex:next ex:e .
ex:x ex:next ex:y . ex:y ex:next ex:x .
GRAPH ex:g { ex:a ex:q ex:b . ex:b ex:q ex:c . }
"#;

fn engine() -> Engine {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::TriG, DATA.as_bytes()).unwrap();
    e
}

/// The rows of `q`, as space-separated local names, sorted.
fn rows(e: &Engine, q: &str) -> Vec<String> {
    let QueryResults::Solutions(s) = e
        .query(&format!("PREFIX ex: <http://example.com/>\n{q}"))
        .unwrap()
    else {
        panic!()
    };
    let vars: Vec<_> = s.variables().to_vec();
    let mut out: Vec<String> = s
        .map(|r| {
            let r = r.unwrap();
            vars.iter()
                .map(|v| {
                    r.get(v).map_or("-".to_owned(), |t| {
                        t.to_string()
                            .trim_end_matches('>')
                            .rsplit('/')
                            .next()
                            .unwrap()
                            .to_owned()
                    })
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    out.sort();
    out
}

#[test]
fn from_a_start() {
    let e = engine();
    assert_eq!(rows(&e, "SELECT ?o { ex:a ex:next{2} ?o }"), ["c", "d"]);
    assert_eq!(
        rows(&e, "SELECT ?o { ex:a ex:next{2,3} ?o }"),
        ["c", "d", "e"]
    );
    assert_eq!(rows(&e, "SELECT ?o { ex:a ex:next{3,} ?o }"), ["d", "e"]);
    assert_eq!(
        rows(&e, "SELECT ?o { ex:a ex:next{,1} ?o }"),
        ["a", "b", "c"]
    );
    assert_eq!(rows(&e, "SELECT ?o { ex:a ex:next{0} ?o }"), ["a"]);
    // Each end once, however many walks reach it (a → c directly and through b).
    assert_eq!(
        rows(&e, "SELECT ?o { ex:a ex:next{1,2} ?o }"),
        ["b", "c", "d"]
    );
    // Around a cycle.
    assert_eq!(rows(&e, "SELECT ?o { ex:x ex:next{2} ?o }"), ["x"]);
    assert_eq!(rows(&e, "SELECT ?o { ex:x ex:next{1,5} ?o }"), ["x", "y"]);
}

#[test]
fn to_an_end_between_two_and_open() {
    let e = engine();
    assert_eq!(rows(&e, "SELECT ?s { ?s ex:next{2} ex:e }"), ["c"]);
    assert_eq!(rows(&e, "SELECT ?s { ?s ^ex:next{2} ex:a }"), ["c", "d"]);
    assert_eq!(
        rows(&e, "SELECT ?s { ?s ex:next{2,3} ex:e }"),
        ["a", "b", "c"]
    );
    let QueryResults::Boolean(b) = e
        .query("PREFIX ex: <http://example.com/> ASK { ex:a ex:next{4} ex:e }")
        .unwrap()
    else {
        panic!()
    };
    assert!(b);
    let QueryResults::Boolean(b) = e
        .query("PREFIX ex: <http://example.com/> ASK { ex:a ex:next{5} ex:e }")
        .unwrap()
    else {
        panic!()
    };
    assert!(!b);
    assert_eq!(
        rows(&e, "SELECT ?s ?o { ?s ex:next{3} ?o }"),
        ["a d", "a e", "b e", "x y", "y x"]
    );
    // In a named graph, and over all graphs.
    assert_eq!(
        rows(&e, "SELECT ?o { GRAPH ex:g { ex:a ex:q{2} ?o } }"),
        ["c"]
    );
    assert_eq!(
        rows(&e, "SELECT ?g ?s ?o { GRAPH ?g { ?s ex:q{1,2} ?o } }"),
        ["g a b", "g a c", "g b c"]
    );
    // Zero steps from a term the store does not hold, as for p*.
    assert_eq!(
        rows(&e, "SELECT ?s { ?s ex:next{0,2} ex:nowhere }"),
        ["nowhere"]
    );
}

#[test]
fn shorthands_and_mistakes() {
    let e = engine();
    // {0,} is *, {1,} is +, {0,1} is ?, {1} is the predicate itself.
    assert_eq!(
        rows(&e, "SELECT ?o { ex:a ex:next{1,} ?o }"),
        rows(&e, "SELECT ?o { ex:a ex:next+ ?o }")
    );
    assert_eq!(
        rows(&e, "SELECT ?o { ex:a ex:next{0,1} ?o }"),
        rows(&e, "SELECT ?o { ex:a ex:next? ?o }")
    );
    assert!(e.query("SELECT * { ?s <urn:p>{3,2} ?o }").is_err());
    // Sequences and groups.
    assert_eq!(
        rows(&e, "SELECT ?o { ex:a (ex:next/ex:next){2} ?o }"),
        ["e"]
    );
    // The query writes back with its bounds.
    let q = spargebra::SparqlParser::new()
        .parse_query("SELECT * { ?s <urn:p>{2,5} ?o }")
        .unwrap();
    assert!(q.to_string().contains("{2,5}"), "{q}");
}
