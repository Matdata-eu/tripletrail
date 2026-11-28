//! A count over a spatial join reads the pairs the spatial rewrite found as a table of ids
//! (#363): the view the query is evaluated on is taken before that rewrite (#328), and must
//! still see the table it registers, or the count goes through the evaluator, row by row.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

/// Tracks in pairs a few metres apart: each one is within 5 m of its pair only.
fn engine(pairs: usize) -> Engine {
    let mut d = String::from(
        "@prefix ex: <http://example.com/> .\n@prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..pairs {
        let x = 4.0 + (i % 100) as f64 * 0.01;
        let y = 50.0 + (i / 100) as f64 * 0.01;
        for (j, off) in [(0, 0.0), (1, 0.00004)] {
            d += &format!(
                "ex:t{i}_{j} a ex:Track ; geo:hasGeometry ex:g{i}_{j} .\nex:g{i}_{j} geo:asWKT \"LINESTRING({:.6} {y:.6}, {:.6} {:.6})\"^^geo:wktLiteral .\n",
                x + off,
                x + off + 0.001,
                y + 0.001
            );
        }
    }
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    e
}

const QUERY: &str = "PREFIX ex: <http://example.com/>
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
PREFIX uom: <http://www.opengis.net/def/uom/OGC/1.0/>
SELECT (COUNT(*) AS ?n) WHERE {
  ?t1 a ex:Track ; geo:hasGeometry/geo:asWKT ?w1 .
  ?t2 a ex:Track ; geo:hasGeometry/geo:asWKT ?w2 .
  FILTER(?t1 != ?t2 && geof:distance(?w1, ?w2, uom:metre) <= 5)
}";

fn count(e: &Engine, query: &str) -> String {
    let QueryResults::Solutions(mut s) = e.query(query).unwrap() else {
        panic!("SELECT")
    };
    s.next().unwrap().unwrap().get("n").unwrap().to_string()
}

#[test]
fn the_count_of_a_spatial_join_is_read_from_its_pairs() {
    let e = engine(300);
    // Ordered pairs: both ways round.
    let n = "\"600\"^^<http://www.w3.org/2001/XMLSchema#integer>";
    for executor in ["ids", "spareval"] {
        assert_eq!(
            count(&e, &format!("#pragma tt.executor {executor}\n{QUERY}")),
            n
        );
    }
    // Counted from the table, before the evaluator: it gets the number. Without the table,
    // it joined the table's rows back and counted them (`Aggregate(COUNT(*))`).
    let explained = e.explain(QUERY).unwrap();
    assert!(explained.contains("a table of 1200 rows"), "{explained}");
    assert!(explained.contains("StaticBindings"), "{explained}");
    assert!(!explained.contains("Aggregate(COUNT"), "{explained}");
}
