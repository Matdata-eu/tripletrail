//! A `FILTER EXISTS` belongs to its group: a variable of the `EXISTS` pattern that the group
//! does not bind is free there, also when a pattern joined to the group binds it.

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;

const DATA: &str = r#"
@prefix ex: <http://example.org/> .
ex:o1 ex:qty 91 ; ex:next ex:o2 ; ex:link ex:o3 .
ex:o3 ex:qty 91 .
ex:o4 ex:qty 5 ; ex:link ex:o5 .
ex:o5 ex:qty 5 .
ex:g1 { ex:o1 ex:qty 91 ; ex:next ex:o2 . }
ex:g2 { ex:o1 ex:next ex:o3 . ex:o3 ex:qty 91 . }
"#;

fn rows(e: &Engine, query: &str) -> Vec<String> {
    let QueryResults::Solutions(solutions) = e.query(query).unwrap() else {
        panic!("not a SELECT");
    };
    let mut rows: Vec<String> = solutions
        .map(|s| {
            let s = s.unwrap();
            s.iter()
                .map(|(v, t)| format!("{v}={t}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    rows.sort();
    rows
}

#[test]
fn exists_reads_only_its_groups_variables() {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::TriG, DATA.as_bytes()).unwrap();
    let prefix = "PREFIX ex: <http://example.org/>";
    let o1 = "?o=<http://example.org/o1>";
    for (q, expected) in [
        (
            "SELECT ?o WHERE { { ?o ex:qty ?q FILTER EXISTS { ?o ex:next ?n } } ?o ex:link ?n . ?n ex:qty ?q }",
            vec![o1],
        ),
        (
            "SELECT ?o WHERE { ?o ex:link ?n . ?n ex:qty ?q { ?o ex:qty ?q FILTER EXISTS { ?o ex:next ?n } } }",
            vec![o1],
        ),
        (
            "SELECT ?o WHERE { { ?o ex:qty ?q FILTER NOT EXISTS { ?o ex:next ?n } } ?o ex:link ?n . ?n ex:qty ?q }",
            vec!["?o=<http://example.org/o4>"],
        ),
        (
            "SELECT ?o WHERE { GRAPH ex:g1 { ?o ex:qty ?q FILTER EXISTS { ?o ex:next ?n } } GRAPH ex:g2 { ?o ex:next ?n . ?n ex:qty ?q } }",
            vec![o1],
        ),
        (
            "SELECT ?o WHERE { GRAPH ex:g2 { ?o ex:next ?n . ?n ex:qty ?q } GRAPH ex:g1 { ?o ex:qty ?q FILTER EXISTS { ?o ex:next ?n } } }",
            vec![o1],
        ),
        // `?n` bound in the group: the `EXISTS` reads it.
        (
            "SELECT ?o WHERE { { ?o ex:qty ?q ; ex:link ?n FILTER EXISTS { ?o ex:next ?n } } ?n ex:qty ?q }",
            vec![],
        ),
    ] {
        for hints in [
            "",
            "#pragma tt.executor spareval\n",
            "#pragma tt.executor ids\n",
            "#pragma tt.id-rewrites off\n",
        ] {
            assert_eq!(
                rows(&e, &format!("{hints}{prefix} {q}")),
                expected,
                "{hints}{q}"
            );
        }
    }
}
