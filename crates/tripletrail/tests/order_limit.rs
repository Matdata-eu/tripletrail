//! ORDER BY under a LIMIT keeps only the rows the LIMIT reads (#874): the same rows as a full
//! sort, in a query memory too small for all of them.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

fn values(engine: &Engine, query: &str) -> Result<Vec<String>, String> {
    let QueryResults::Solutions(s) = engine.query(query).map_err(|e| e.to_string())? else {
        panic!()
    };
    s.map(|row| {
        let row = row.map_err(|e| e.to_string())?;
        Ok(row.get("k").map_or_else(String::new, ToString::to_string))
    })
    .collect()
}

#[test]
fn the_first_rows_of_a_sort_hold_little_memory() {
    // Keys in a scrambled order, every tenth unbound.
    let d: String = (0..5000)
        .map(|i| {
            if i % 10 == 0 {
                format!("<urn:s{i}> <urn:q> 1 .\n")
            } else {
                format!("<urn:s{i}> <urn:p> \"v{}\" .\n", (i * 7919) % 5000)
            }
        })
        .collect();
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    for order in ["?k", "DESC(?k)", "STRLEN(?k) DESC(?k)"] {
        let all = values(
            &engine,
            &format!("SELECT ?k {{ ?s ?p ?o OPTIONAL {{ ?s <urn:p> ?v BIND(CONCAT(?v, \"x\") AS ?k) }} }} ORDER BY {order}"),
        )
        .unwrap();
        assert_eq!(all.len(), 5000);
        for (offset, limit) in [
            (0, 1),
            (0, 10),
            (5, 10),
            (495, 20),
            (4990, 20),
            (0, 6000),
            (0, 1 << 63),
            (0, usize::MAX),
        ] {
            let q = format!(
                "SELECT ?k {{ ?s ?p ?o OPTIONAL {{ ?s <urn:p> ?v BIND(CONCAT(?v, \"x\") AS ?k) }} }} ORDER BY {order} OFFSET {offset} LIMIT {limit}"
            );
            let end = (offset + limit).min(all.len());
            assert_eq!(values(&engine, &q).unwrap(), all[offset..end], "{q}");
        }
        // Projected: SELECT with an expression.
        let q = format!(
            "SELECT ?k (STRLEN(?k) AS ?n) {{ ?s <urn:p> ?v BIND(CONCAT(?v, \"x\") AS ?k) }} ORDER BY {order} LIMIT 3"
        );
        assert_eq!(values(&engine, &q).unwrap().len(), 3);
    }
    // 5,000 rows don't fit in 64 KiB, the 10 first ones do.
    let small = engine.with_query_memory(64 << 10);
    let q = "SELECT ?k { ?s <urn:p> ?v BIND(CONCAT(?v, \"x\") AS ?k) } ORDER BY ?k";
    assert!(
        values(&small, q)
            .unwrap_err()
            .contains("more memory than its limit")
    );
    let first = values(&small, &format!("{q} LIMIT 10")).unwrap();
    assert_eq!(first, values(&engine, q).unwrap()[..10]);
}
