//! `=` on triple terms compares their parts by value, also on the id-level filter path
//! (W3C eval-triple-terms op-2).

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;

#[test]
fn equal_triple_terms_with_different_literals() {
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        "PREFIX : <http://example/>
        :x1 :left <<( :a :b 123 )>> ; :right <<( :a :b 123.0 )>> .
        :x2 :left <<( :a :b 123 )>> ; :right <<( :a :b 123 )>> .
        :x3 :left <<( :a :q <<( :a :b 123 )>> )>> ; :right <<( :a :q <<( :a :b 123.0 )>> )>> .
        :x4 :left <<( :a :b 123 )>> ; :right <<( :c :d 123 )>> .
        :x5 :left <<( :a :b 9 )>> ; :right <<( :a :b 123 )>> ."
            .as_bytes(),
    )
    .unwrap();
    let rows = |filter: &str| {
        let q = format!(
            "PREFIX : <http://example/> SELECT ?x {{ ?x :left ?l ; :right ?r FILTER({filter}) }}"
        );
        let QueryResults::Solutions(s) = e.query(&q).unwrap() else {
            panic!("not a SELECT")
        };
        let mut rows: Vec<String> = s
            .map(|s| s.unwrap().get("x").unwrap().to_string())
            .collect();
        rows.sort();
        rows
    };
    assert_eq!(
        rows("?l = ?r"),
        [
            "<http://example/x1>",
            "<http://example/x2>",
            "<http://example/x3>"
        ]
    );
    assert_eq!(
        rows("?l != ?r"),
        ["<http://example/x4>", "<http://example/x5>"]
    );
    assert_eq!(rows("sameTerm(?l, ?r)"), ["<http://example/x2>"]);
}
