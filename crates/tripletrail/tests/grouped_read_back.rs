//! GROUP BY with many groups: the groups go to the evaluator as a table of ids (#185), also
//! when a key is a blank node, which a `VALUES` block can't hold.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use std::fmt::Write;
use tripletrail::Engine;

#[test]
fn many_groups_keep_their_values() {
    // 3,000 blank nodes (more groups than go into a VALUES block), each with two values.
    let mut d = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..3000 {
        writeln!(d, "_:b{i} ex:v {i}, {} ; ex:k ex:k{} .", i + 10, i % 2).unwrap();
    }
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    let query = "PREFIX ex: <http://example.org/>
        SELECT ?b ?k (COUNT(*) AS ?n) (MIN(?v) AS ?lo) (MAX(?v) AS ?hi) (SUM(?v) AS ?sum)
        { ?b ex:v ?v ; ex:k ?k } GROUP BY ?b ?k";
    let QueryResults::Solutions(solutions) = e.query(query).unwrap() else {
        panic!("not a SELECT")
    };
    let mut groups = 0;
    for s in solutions {
        let s = s.unwrap();
        let int = |v: &str| match s.get(v) {
            Some(oxigraph::model::Term::Literal(l)) => l.value().parse::<i64>().unwrap(),
            other => panic!("{v}: {other:?}"),
        };
        assert!(matches!(
            s.get("b"),
            Some(oxigraph::model::Term::BlankNode(_))
        ));
        assert_eq!(int("n"), 2);
        assert_eq!(int("hi"), int("lo") + 10);
        assert_eq!(int("sum"), 2 * int("lo") + 10);
        let k = s.get("k").unwrap().to_string();
        assert_eq!(k, format!("<http://example.org/k{}>", int("lo") % 2));
        groups += 1;
    }
    assert_eq!(groups, 3000);
}
