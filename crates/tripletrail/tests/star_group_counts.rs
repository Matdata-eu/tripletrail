//! `GROUP BY` counts over a star of triple patterns (`?s a ?c ; ex:f ?f GROUP BY ?c`) come
//! from each triple's matches per subject, without building the join's rows (#179). The
//! counts are the evaluator's, also for a key that is the centre, with constants, a blank
//! node in the pattern, quads in a named graph (not counted) and writes not yet compacted.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use std::fmt::Write;
use tripletrail::Engine;

fn sorted(engine: &Engine, query: &str) -> Vec<String> {
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
fn star_group_counts_match_the_rows() {
    let mut d = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..4000 {
        // Multi-valued properties: 1 to 3 classes, 2 to 4 features, 0 to 2 tags.
        write!(d, "ex:s{i} a ex:C{}", i % 5).unwrap();
        for k in 1..=(i % 3) {
            write!(d, ", ex:D{}", (i + k) % 4).unwrap();
        }
        for k in 0..(2 + i % 3) {
            write!(d, " ; ex:f ex:f{}", (i * 3 + k) % 41).unwrap();
        }
        for k in 0..(i % 3) {
            write!(d, " ; ex:t \"t{}\"", (i + k) % 17).unwrap();
        }
        // A rare property (far fewer matches than the others: the counts seek or walk its
        // subjects in the others, #165), sometimes with two values.
        if i % 20 == 0 {
            write!(d, " ; ex:rare ex:r{}", i % 7).unwrap();
            if i % 40 == 0 {
                write!(d, ", ex:r{}", (i + 1) % 7).unwrap();
            }
        }
        d.push_str(" .\n");
    }
    d.push_str(
        "GRAPH ex:g { ex:s1 a ex:C0 ; ex:f ex:f0 ; ex:t \"t0\" . ex:x a ex:C0 ; ex:f ex:f1 ; ex:rare ex:r0 }\n",
    );
    let queries = [
        "SELECT ?c (COUNT(*) AS ?n) WHERE { ?s a ?c ; ex:f ?f } GROUP BY ?c",
        "SELECT ?c (COUNT(*) AS ?n) (COUNT(?t) AS ?m) WHERE { ?s a ?c ; ex:f ?f ; ex:t ?t } GROUP BY ?c",
        "SELECT ?t (COUNT(?f) AS ?n) WHERE { ?s a ?c ; ex:f ?f ; ex:t ?t } GROUP BY ?t",
        "SELECT ?s (COUNT(*) AS ?n) WHERE { ?s a ?c ; ex:f ?f ; ex:t ?t } GROUP BY ?s",
        "SELECT ?f (COUNT(*) AS ?n) WHERE { ?s a ex:C2 ; ex:f ?f ; ex:t ?t } GROUP BY ?f",
        "SELECT ?f (COUNT(*) AS ?n) WHERE { ?s a [] ; ex:f ?f } GROUP BY ?f",
        "SELECT ?c (COUNT(*) AS ?n) WHERE { ?s a ?c ; ex:missing ?f } GROUP BY ?c",
        "SELECT ?r (COUNT(?s) AS ?n) WHERE { ?s ex:rare ?r ; a ?c } GROUP BY ?r",
        "SELECT ?r (COUNT(*) AS ?n) WHERE { ?s ex:rare ?r ; a ?c ; ex:f ?f } GROUP BY ?r",
        "SELECT ?c (COUNT(*) AS ?n) WHERE { ?s ex:rare ?r ; a ?c } GROUP BY ?c",
        "SELECT ?s (COUNT(*) AS ?n) WHERE { ?s ex:rare ?r ; ex:f ?f } GROUP BY ?s",
    ];
    let check = |engine: &Engine| {
        for q in queries {
            let q = format!("PREFIX ex: <http://example.org/> {q}");
            let slow = q.replacen(" } GROUP BY", " FILTER(true) } GROUP BY", 1);
            assert_ne!(q, slow);
            assert_eq!(sorted(engine, &q), sorted(engine, &slow), "{q}");
        }
    };
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::TriG, d.as_bytes()).unwrap();
    check(&engine);
    // The join's rows are not built: the plan reads no quads.
    let plan = engine
        .explain(
            "PREFIX ex: <http://example.org/> SELECT ?t (COUNT(*) AS ?n) WHERE { ?s a ?c ; ex:f ?f ; ex:t ?t } GROUP BY ?t",
        )
        .unwrap();
    assert!(!plan.contains("QuadPattern"), "{plan}");
    engine
        .update(
            "PREFIX ex: <http://example.org/> INSERT DATA { ex:new a ex:C0, ex:C9 ; ex:f ex:f0 ; ex:t \"t1\" ; ex:rare ex:r3 } ;
             DELETE DATA { ex:s0 a ex:C0 . ex:s1 ex:f ex:f3 . ex:s2 ex:t \"t2\" . ex:s20 ex:rare ex:r6 . ex:s40 ex:f ex:f0 }",
        )
        .unwrap();
    check(&engine);
}
