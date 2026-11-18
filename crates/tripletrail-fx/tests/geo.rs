//! TripleTrail's geometry extensions (`geo=true` on GeoJSON, `geo.wkt` / `geo.x` / `geo.y` on
//! CSV): off by default, so SPARQL Anything queries give the same graph.

use oxigraph::model::{Dataset, Literal};
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use tripletrail_fx::{FxConfig, FxService};

/// The rows of `?s ?p ?o` (shortened) for `content` with `options` (option triples).
fn select(content: &str, options: &str, pattern: &str) -> Vec<String> {
    let content = Literal::new_simple_literal(content);
    let fx = FxService::new(FxConfig::default(), SparqlEvaluator::new);
    let empty = Box::leak(Box::new(Dataset::new()));
    let QueryResults::Solutions(solutions) = fx
        .register(SparqlEvaluator::new())
        .parse_query(&format!(
            "PREFIX fxe: <http://sparql.xyz/facade-x/engine/>
            PREFIX geo: <http://www.opengis.net/ont/geosparql#>
            PREFIX xyz: <http://sparql.xyz/facade-x/data/>
            SELECT * WHERE {{ SERVICE <x-sparql-anything:> {{
                fxe:properties fxe:content {content} ; fxe:root \"http://ex.org/d\" ;
                  fxe:blank-nodes \"false\" {options} .
                {pattern} }} }}"
        ))
        .unwrap()
        .on_queryable_dataset(&*empty)
        .execute()
        .unwrap()
    else {
        panic!()
    };
    let mut rows: Vec<String> = solutions
        .map(|s| {
            let s = s.unwrap();
            s.iter()
                .map(|(v, t)| format!("{v}={t}"))
                .collect::<Vec<_>>()
                .join(" ")
                .replace("http://www.opengis.net/ont/geosparql#", "geo:")
        })
        .collect();
    rows.sort();
    rows
}

const GEOJSON: &str = r#"{"type":"FeatureCollection","features":[
  {"type":"Feature","properties":{"name":"Oslo S"},"geometry":{"type":"Point","coordinates":[10.7522,59.9111]}},
  {"type":"Feature","properties":{"name":"area"},"geometry":{"type":"Polygon","coordinates":[[[10,59],[11,59],[11,60.00],[10,59]]]}},
  {"type":"Feature","properties":{"name":"both"},"geometry":{"type":"GeometryCollection","geometries":[
    {"type":"Point","coordinates":[1,2,3]},{"type":"LineString","coordinates":[[0,0],[1,1]]}]}},
  {"type":"Feature","properties":{"name":"none"},"geometry":null}
]}"#;

#[test]
fn geojson_features_get_wkt() {
    let rows = select(
        GEOJSON,
        r#"; fxe:media-type "application/geo+json" ; fxe:geo "true""#,
        "?f geo:hasGeometry ?g . ?g geo:asWKT ?wkt . ?f xyz:properties/xyz:name ?name",
    );
    assert_eq!(rows.len(), 3, "{rows:#?}");
    for (w, name) in [
        ("POINT(10.7522 59.9111)", "Oslo S"),
        ("POLYGON((10 59, 11 59, 11 60.00, 10 59))", "area"),
        (
            "GEOMETRYCOLLECTION(POINT Z(1 2 3), LINESTRING(0 0, 1 1))",
            "both",
        ),
    ] {
        let tail = format!("?wkt=\"{w}\"^^<geo:wktLiteral> ?name=\"{name}\"");
        assert!(
            rows.iter().any(|r| r.ends_with(&tail)),
            "{tail} not in {rows:#?}"
        );
    }
}

#[test]
fn geojson_crs() {
    let rows = select(
        r#"{"type":"Point","coordinates":[600000,6640000]}"#,
        r#"; fxe:media-type "application/json" ; fxe:geo "true" ; fxe:geo.crs "EPSG:25833""#,
        "?g geo:asWKT ?wkt",
    );
    assert_eq!(
        rows,
        [
            "?g=<http://ex.org/d> ?wkt=\"<http://www.opengis.net/def/crs/EPSG/0/25833> POINT(600000 6640000)\"^^<geo:wktLiteral>"
        ]
    );
}

#[test]
fn off_by_default() {
    let rows = select(
        GEOJSON,
        r#"; fxe:media-type "application/geo+json""#,
        "{ ?g geo:asWKT ?wkt } UNION { ?f geo:hasGeometry ?g }",
    );
    assert!(rows.is_empty(), "{rows:#?}");
}

#[test]
fn csv_coordinate_columns() {
    let rows = select(
        "id,east,north,h\na,600000.5,6640000,12\nb,,,\nc,x,1,\n",
        r#"; fxe:media-type "text/csv" ; fxe:csv.headers "true" ; fxe:geo.x "east" ; fxe:geo.y "north" ; fxe:geo.z "4" ; fxe:geo.crs "EPSG:25833""#,
        "?r xyz:id ?id ; geo:asWKT ?wkt",
    );
    assert_eq!(
        rows,
        [
            "?r=<http://ex.org/d#row1> ?id=\"a\" ?wkt=\"<http://www.opengis.net/def/crs/EPSG/0/25833> POINT Z(600000.5 6640000 12)\"^^<geo:wktLiteral>"
        ]
    );
}

#[test]
fn csv_wkt_column() {
    let rows = select(
        "POINT(1 2);a\n;b\n",
        r#"; fxe:media-type "text/csv" ; fxe:csv.delimiter ";" ; fxe:geo.wkt "1""#,
        "?r geo:asWKT ?wkt",
    );
    assert_eq!(
        rows,
        ["?r=<http://ex.org/d#row1> ?wkt=\"POINT(1 2)\"^^<geo:wktLiteral>"]
    );
}

#[test]
fn csv_unknown_column_is_an_error() {
    let content = Literal::new_simple_literal("a,b\n1,2\n");
    let fx = FxService::new(FxConfig::default(), SparqlEvaluator::new);
    let empty = Box::leak(Box::new(Dataset::new()));
    let result = fx
        .register(SparqlEvaluator::new())
        .parse_query(&format!(
            "PREFIX fxe: <http://sparql.xyz/facade-x/engine/>
            SELECT * WHERE {{ SERVICE <x-sparql-anything:> {{
                fxe:properties fxe:content {content} ; fxe:media-type \"text/csv\" ;
                  fxe:csv.headers \"true\" ; fxe:geo.x \"lon\" ; fxe:geo.y \"b\" .
                ?s ?p ?o }} }}"
        ))
        .unwrap()
        .on_queryable_dataset(&*empty)
        .execute()
        .and_then(|r| match r {
            QueryResults::Solutions(s) => s.collect::<Result<Vec<_>, _>>().map(|_| ()),
            _ => Ok(()),
        });
    let e = result.unwrap_err().to_string();
    assert!(e.contains("no such column"), "{e}");
}
