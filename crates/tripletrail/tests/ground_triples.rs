//! Triple patterns without variables (`:s :p :o`) check that the triple is in the graph: they
//! keep every row or none (issue #366). They used to give no row, so `FILTER EXISTS` over one
//! was always false and `NOT EXISTS` always true.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

fn rows(engine: &Engine, query: &str) -> Vec<String> {
    let query = format!("PREFIX : <http://www.example.org/>\n{query}");
    match engine.query(&query).unwrap() {
        QueryResults::Solutions(solutions) => {
            let mut rows: Vec<String> = solutions
                .map(|s| {
                    s.unwrap()
                        .iter()
                        .map(|(v, t)| format!("{}={t}", v.as_str()))
                        .collect::<Vec<_>>()
                        .join(" ")
                        .replace("<http://www.example.org/", ":")
                        .replace('>', "")
                        .replace("^^<http://www.w3.org/2001/XMLSchema#integer", "")
                })
                .collect();
            rows.sort();
            rows
        }
        QueryResults::Boolean(b) => vec![b.to_string()],
        QueryResults::Graph(_) => panic!("not a SELECT or ASK"),
    }
}

#[test]
fn triples_without_variables_keep_all_rows_or_none() {
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        b"@prefix : <http://www.example.org/> . :s :p :o, :o1, :o2 . :t :p :o1, :o2 ." as &[u8],
    )
    .unwrap();
    let both = ["s=:s p=:p", "s=:t p=:p"];
    assert_eq!(
        rows(&e, "SELECT * { ?s ?p :o2 FILTER EXISTS { :s :p :o } }"),
        both
    );
    assert!(rows(&e, "SELECT * { ?s ?p :o2 FILTER EXISTS { :s :p :zz } }").is_empty());
    assert!(rows(&e, "SELECT * { ?s ?p :o2 FILTER NOT EXISTS { :s :p :o } }").is_empty());
    assert_eq!(
        rows(&e, "SELECT * { ?s ?p :o2 FILTER NOT EXISTS { :s :p :zz } }"),
        both
    );
    assert_eq!(rows(&e, "SELECT * { :s :p :o . :t :p :o1 }"), [""]);
    assert!(rows(&e, "SELECT * { :s :p :o . :t :p :zz }").is_empty());
    assert_eq!(
        rows(&e, "SELECT * { :s :p :o . ?x :p :o1 }"),
        ["x=:s", "x=:t"]
    );
    assert!(rows(&e, "SELECT * { :s :p :zz . ?x :p :o1 }").is_empty());
    assert_eq!(
        rows(&e, "SELECT (COUNT(*) AS ?c) { :s :p :o . :t :p :o1 }"),
        ["c=\"1\""]
    );
    assert_eq!(
        rows(&e, "SELECT (COUNT(*) AS ?c) { :s :p :o . ?x :p :o1 }"),
        ["c=\"2\""]
    );
    assert_eq!(
        rows(&e, "SELECT (COUNT(*) AS ?c) { :s :p :zz . ?x :p :o1 }"),
        ["c=\"0\""]
    );
    assert_eq!(rows(&e, "ASK { :s :p :o . :t :p :o2 }"), ["true"]);
}
