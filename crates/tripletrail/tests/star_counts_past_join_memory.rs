//! Counts over a star of multi-valued triple patterns whose join has far more rows than the
//! id joins may hold (#179): they come from each triple's matches per subject, so they are
//! answered without the rows, where building the rows would give up past the join memory.
//! A file of its own: the join memory is a setting of the whole process.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use std::fmt::Write;
use tripletrail::Engine;

const SUBJECTS: usize = 300;
const CLASSES: usize = 30;
const FEATURES: usize = 30;
const TAGS: usize = 10;

fn rows(engine: &Engine, query: &str) -> Vec<String> {
    let QueryResults::Solutions(solutions) = engine.query(query).unwrap() else {
        panic!("not a SELECT")
    };
    let mut v: Vec<String> = solutions
        .map(|s| {
            s.unwrap()
                .iter()
                .map(|(v, t)| format!("{v}={t}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    v.sort();
    v
}

#[test]
fn star_counts_need_no_rows_past_the_join_memory() {
    // 1 MiB: 131,072 ids, against 300 × 30 × 30 × 10 = 2.7M rows of three ids.
    tripletrail_store::set_join_memory(1 << 20);
    let mut d = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..SUBJECTS {
        write!(d, "ex:s{i} ex:in ex:k{}", i % 3).unwrap();
        for c in 0..CLASSES {
            write!(d, " ; a ex:C{c}").unwrap();
        }
        for f in 0..FEATURES {
            write!(d, " ; ex:f ex:f{f}").unwrap();
        }
        for t in 0..TAGS {
            write!(d, " ; ex:t \"t{t}\"").unwrap();
        }
        d.push_str(" .\n");
    }
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    let per_subject = CLASSES * FEATURES * TAGS;
    let prefix = "PREFIX ex: <http://example.org/> ";
    let star = "?s a ?c ; ex:f ?f ; ex:t ?t";
    let total = format!("{prefix} SELECT (COUNT(*) AS ?n) WHERE {{ {star} }}");
    assert_eq!(
        rows(&engine, &total),
        [format!(
            "?n=\"{}\"^^<http://www.w3.org/2001/XMLSchema#integer>",
            SUBJECTS * per_subject
        )]
    );
    let by_class = format!("{prefix} SELECT ?c (COUNT(*) AS ?n) WHERE {{ {star} }} GROUP BY ?c");
    let got = rows(&engine, &by_class);
    assert_eq!(got.len(), CLASSES);
    let each = SUBJECTS * FEATURES * TAGS;
    assert!(
        got.iter().all(|r| r.ends_with(&format!(
            "?n=\"{each}\"^^<http://www.w3.org/2001/XMLSchema#integer>"
        ))),
        "{got:?}"
    );
    let by_key = format!(
        "{prefix} SELECT ?k (COUNT(*) AS ?n) WHERE {{ ?s ex:in ?k ; a ?c ; ex:f ?f ; ex:t ?t }} GROUP BY ?k"
    );
    let got = rows(&engine, &by_key);
    assert_eq!(got.len(), 3);
    let each = SUBJECTS / 3 * per_subject;
    assert!(
        got.iter().all(|r| r.ends_with(&format!(
            "?n=\"{each}\"^^<http://www.w3.org/2001/XMLSchema#integer>"
        ))),
        "{got:?}"
    );
    // None of them reads the join's rows.
    for q in [&total, &by_class, &by_key] {
        let plan = engine.explain(q).unwrap();
        assert!(!plan.contains("QuadPattern"), "{q}: {plan}");
    }
}
