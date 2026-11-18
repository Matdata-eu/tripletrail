//! Dataset statistics for VoID (#242): the tripletrail store's numbers, from the counts kept with
//! its indexes, equal those of reading every quad of Oxigraph's store.

use oxigraph::io::RdfFormat;
use tripletrail::{DatasetStatistics, Engine};

fn data() -> String {
    let mut ttl = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..3000 {
        let object = match i % 5 {
            0 => format!("ex:o{}", i % 50),
            1 => format!("\"label {}\"@en", i % 200),
            2 => format!("{}", i % 70),
            3 => format!("_:b{}", i % 25),
            _ => format!("\"v{}\"", i % 300),
        };
        ttl.push_str(&format!("ex:s{} ex:p{} {object} .\n", i % 400, i % 13));
        if i % 7 == 0 {
            ttl.push_str(&format!("ex:s{} a ex:C{} .\n", i % 400, i % 4));
        }
        if i % 11 == 0 {
            ttl.push_str(&format!("_:t{} a ex:C{} .\n", i % 30, i % 3));
        }
    }
    // A subject typed in two graphs counts once for its class; the quads count twice.
    ttl.push_str(
        "GRAPH ex:g1 { ex:s1 a ex:C1 . ex:s1 ex:p1 \"only in g1\" . ex:n ex:pn ex:m . }\n",
    );
    ttl.push_str("GRAPH ex:g2 { ex:s1 a ex:C1 . ex:x a ex:D . }\n");
    ttl
}

fn engines(data: &str) -> (Engine, Engine) {
    let tripletrail = Engine::new_tripletrail();
    tripletrail.load(RdfFormat::TriG, data.as_bytes()).unwrap();
    let oxigraph = Engine::new_oxigraph().unwrap();
    oxigraph.load(RdfFormat::TriG, data.as_bytes()).unwrap();
    (tripletrail, oxigraph)
}

fn same(a: &DatasetStatistics, b: &DatasetStatistics) {
    assert_eq!(a.triples, b.triples);
    assert_eq!(a.distinct_subjects, b.distinct_subjects);
    assert_eq!(a.distinct_predicates, b.distinct_predicates);
    assert_eq!(a.distinct_objects, b.distinct_objects);
    assert_eq!(a.entities, b.entities);
    assert_eq!(a.classes, b.classes);
    assert_eq!(a.properties, b.properties);
    assert_eq!(a.graphs, b.graphs);
}

#[test]
fn statistics_match_reading_every_quad() {
    let (m, o) = engines(&data());
    let stats = m.dataset_statistics().unwrap();
    same(&stats, &o.dataset_statistics().unwrap());
    assert_eq!(stats.graphs.as_ref().unwrap().len(), 3);
    assert_eq!(stats.properties.as_ref().unwrap().len(), 15);
    let c1 = stats
        .classes
        .iter()
        .find(|(c, _)| c.as_str().ends_with("C1"));
    assert!(c1.is_some_and(|(_, n)| *n > 1));
    // Kept until the data changes.
    assert!(std::sync::Arc::ptr_eq(
        &stats,
        &m.dataset_statistics().unwrap()
    ));

    // Writes over 1 % of the store: the indexes are read, so the numbers stay exact.
    let mut insert = String::from("PREFIX ex: <http://example.org/> INSERT DATA { ");
    for i in 0..200 {
        insert.push_str(&format!("ex:new{i} a ex:New ; ex:p{} ex:o{i} . ", i % 3));
    }
    insert.push_str("GRAPH ex:g3 { ex:z ex:pz 1 } }");
    for e in [&m, &o] {
        e.update(&insert).unwrap();
        e.update("PREFIX ex: <http://example.org/> DELETE WHERE { ex:s3 ?p ?v }")
            .unwrap();
    }
    let after = m.dataset_statistics().unwrap();
    assert!(after.triples > stats.triples);
    same(&after, &o.dataset_statistics().unwrap());

    let empty = Engine::new_tripletrail().dataset_statistics().unwrap();
    assert_eq!(empty.triples, 0);
    assert!(empty.classes.is_empty());
}
