//! `SERVICE <x-sparql-anything:…>` through the engine (issue #156): files join with the
//! store, CONSTRUCT lifts them, and the calls fail unless the engine enabled them.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;
use tripletrail::fx::FxConfig;

const PREFIXES: &str = "PREFIX xyz: <http://sparql.xyz/facade-x/data/>
PREFIX fx: <http://sparql.xyz/facade-x/ns/>
PREFIX fxe: <http://sparql.xyz/facade-x/engine/>
PREFIX ex: <http://example.org/>
";

/// Writes `contents` to `path` by renaming a temporary file into place: the tests run in
/// parallel and each writes the fixtures, so a plain write could let another test read a
/// file just truncated.
fn write(path: std::path::PathBuf, contents: &str) {
    let tmp = path.with_extension(format!("tmp-{:?}", std::thread::current().id()));
    std::fs::write(&tmp, contents).unwrap();
    std::fs::rename(&tmp, &path).unwrap();
}

fn dir() -> tripletrail_testdir::TestDir {
    let dir = tripletrail_testdir::tempdir("facade-x");
    write(
        dir.join("stations.json"),
        r#"[{"code":"OSL","name":"Oslo S","lines":["L1","L2"]},{"code":"LLS","name":"Lillestrøm","lines":["L1"]}]"#,
    );
    write(
        dir.join("signals.csv"),
        "property_oid,property_object_oid,name,measure1,comment\nP1,O1,Signal A,12.5,\nP2,O1,Signal A,13.0,moved\n",
    );
    dir
}

/// An engine over the stations, reading files from `dir` when given (Facade-X on).
fn engine(dir: Option<&std::path::Path>) -> Engine {
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::Turtle,
            "@prefix ex: <http://example.org/> . ex:OSL ex:km 0 . ex:LLS ex:km 21 .".as_bytes(),
        )
        .unwrap();
    if let Some(dir) = dir {
        engine.with_fx(FxConfig {
            fixed_limits: false,
            databases: None,
            base: dir.to_path_buf(),
            allowed: Some(vec![dir.to_path_buf()]),
            remote: Default::default(),
        })
    } else {
        engine
    }
}

#[test]
fn files_join_with_the_store() {
    let dir = dir();
    let QueryResults::Solutions(solutions) = engine(Some(&dir))
        .query(&format!(
            "{PREFIXES}SELECT ?name ?km WHERE {{
                SERVICE <x-sparql-anything:location=stations.json> {{ ?s xyz:code ?code ; xyz:name ?name }}
                BIND(fx:entity(ex:, ?code) AS ?station)
                ?station ex:km ?km
            }} ORDER BY ?km"
        ))
        .unwrap()
    else {
        panic!("not a SELECT")
    };
    let rows: Vec<String> = solutions
        .map(|s| {
            let s = s.unwrap();
            format!("{} {}", s.get("name").unwrap(), s.get("km").unwrap())
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            "\"Oslo S\" \"0\"^^<http://www.w3.org/2001/XMLSchema#integer>",
            "\"Lillestrøm\" \"21\"^^<http://www.w3.org/2001/XMLSchema#integer>",
        ]
    );
}

#[test]
fn construct_lifts_a_file() {
    let dir = dir();
    let QueryResults::Graph(triples) = engine(Some(&dir))
        .query(&format!(
            "{PREFIXES}CONSTRUCT {{ ?station ex:line ?line }} WHERE {{
                SERVICE <x-sparql-anything:location=stations.json> {{
                    ?s xyz:code ?code ; xyz:lines ?lines . ?lines fx:anySlot ?l
                }}
                BIND(fx:entity(ex:, ?code) AS ?station)
                BIND(fx:entity(ex:, \"line/\", ?l) AS ?line)
            }}"
        ))
        .unwrap()
    else {
        panic!("not a CONSTRUCT")
    };
    let mut lines: Vec<String> = triples.map(|t| t.unwrap().to_string()).collect();
    lines.sort();
    assert_eq!(
        lines,
        vec![
            "<http://example.org/LLS> <http://example.org/line> <http://example.org/line/L1>",
            "<http://example.org/OSL> <http://example.org/line> <http://example.org/line/L1>",
            "<http://example.org/OSL> <http://example.org/line> <http://example.org/line/L2>",
        ]
    );
}

#[test]
fn calls_fail_unless_enabled_but_functions_work() {
    let engine = engine(None);
    let failed = engine
        .query(&format!(
            "{PREFIXES}SELECT * WHERE {{ SERVICE <x-sparql-anything:location=stations.json> {{ ?s ?p ?o }} }}"
        ))
        .and_then(|r| match r {
            QueryResults::Solutions(s) => {
                s.collect::<Result<Vec<_>, _>>().map(|_| ()).map_err(Into::into)
            }
            _ => Ok(()),
        });
    assert!(failed.is_err());
    let QueryResults::Solutions(mut s) = engine
        .query(&format!(
            "{PREFIXES}SELECT (fx:entity(ex:, \"a\", 1) AS ?e) {{}}"
        ))
        .unwrap()
    else {
        panic!("not a SELECT")
    };
    assert_eq!(
        s.next().unwrap().unwrap().get("e").unwrap().to_string(),
        "<http://example.org/a1>"
    );
}

/// The shape of the rail KG lift of a CSV table: every non-empty column but a few becomes a
/// property, through a variable predicate.
#[test]
fn csv_columns_become_properties() {
    let dir = dir();
    let QueryResults::Graph(triples) = engine(Some(&dir))
        .query(&format!(
            "{PREFIXES}CONSTRUCT {{ ?version ex:of ?object ; ?property ?value }} WHERE {{
                SERVICE <x-sparql-anything:location=signals.csv,csv.headers=true> {{
                    ?row xyz:property_oid ?oid ; xyz:property_object_oid ?object_oid .
                    ?row ?column ?value .
                    FILTER(?value != \"\" && STRSTARTS(STR(?column), STR(xyz:)))
                    FILTER(?column NOT IN (xyz:property_oid, xyz:property_object_oid))
                    BIND(fx:entity(ex:, STRAFTER(STR(?column), STR(xyz:))) AS ?property)
                }}
                BIND(fx:entity(ex:, ?oid) AS ?version)
                BIND(fx:entity(ex:, ?object_oid) AS ?object)
            }}"
        ))
        .unwrap()
    else {
        panic!("not a CONSTRUCT")
    };
    let mut lines: Vec<String> = triples
        .map(|t| t.unwrap().to_string().replace("http://example.org/", ""))
        .collect();
    lines.sort();
    assert_eq!(
        lines,
        vec![
            "<P1> <measure1> \"12.5\"",
            "<P1> <name> \"Signal A\"",
            "<P1> <of> <O1>",
            "<P2> <comment> \"moved\"",
            "<P2> <measure1> \"13.0\"",
            "<P2> <name> \"Signal A\"",
            "<P2> <of> <O1>",
        ]
    );
}

fn rows(engine: &Engine, query: &str) -> (Vec<String>, Vec<String>) {
    let QueryResults::Solutions(solutions) = engine.query(&format!("{PREFIXES}{query}")).unwrap()
    else {
        panic!("not a SELECT")
    };
    let variables = solutions
        .variables()
        .iter()
        .map(|v| v.as_str().to_owned())
        .collect();
    let mut out: Vec<String> = solutions
        .map(|s| {
            s.unwrap()
                .iter()
                .map(|(v, t)| format!("{}={t}", v.as_str()))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    out.sort();
    (variables, out)
}

/// An option triple can take a value bound outside the SERVICE, as in SPARQL Anything: the
/// SERVICE then runs once per row (here JSON held in a CSV cell, with commas, `=` signs and
/// non-ASCII text in it).
#[test]
fn options_bound_outside_the_service() {
    let dir = tripletrail_testdir::tempdir("fx-outside");
    std::fs::write(
        dir.join("cells.csv"),
        "id,json\n1,\"[{\"\"t\"\":\"\"a=b, c\"\"},{\"\"t\"\":\"\"Lillestrøm\"\"}]\"\n2,\"{\"\"u\"\":1}\"\n3,\"[{\"\"t\"\":\"\"z\"\"}]\"\n",
    )
    .unwrap();
    let engine = Engine::new_tripletrail().with_fx(FxConfig {
        fixed_limits: false,
        databases: None,
        base: dir.to_path_buf(),
        allowed: Some(vec![dir.to_path_buf()]),
        remote: Default::default(),
    });
    let read = "SERVICE <x-sparql-anything:location=cells.csv,csv.headers=true> { ?row xyz:id ?id ; xyz:json ?json }";
    let inner = "fx:properties fx:content ?json ; fx:media-type \"application/json\" . ?x xyz:t ?t";
    // OPTIONAL keeps the rows the SERVICE gives nothing for.
    let optional = format!("{read} OPTIONAL {{ SERVICE <x-sparql-anything:> {{ {inner} }} }}");
    let (variables, _) = rows(&engine, &format!("SELECT * WHERE {{ {optional} }}"));
    assert!(
        variables.iter().all(|v| !v.starts_with("__fx")),
        "{variables:?}"
    );
    let (_, kept) = rows(&engine, &format!("SELECT ?id ?t WHERE {{ {optional} }}"));
    assert_eq!(
        kept,
        [
            "id=\"1\" t=\"Lillestrøm\"",
            "id=\"1\" t=\"a=b, c\"",
            "id=\"2\"",
            "id=\"3\" t=\"z\""
        ]
    );
    // A join drops them; the SERVICE may come first in the group.
    for query in [
        format!("SELECT ?id ?t WHERE {{ {read} SERVICE <x-sparql-anything:> {{ {inner} }} }}"),
        format!(
            "SELECT ?id ?t WHERE {{ {{ SERVICE <x-sparql-anything:> {{ {inner} }} }} {{ {read} }} }}"
        ),
    ] {
        let (_, joined) = rows(&engine, &query);
        assert_eq!(
            joined,
            [
                "id=\"1\" t=\"Lillestrøm\"",
                "id=\"1\" t=\"a=b, c\"",
                "id=\"3\" t=\"z\""
            ],
            "{query}"
        );
    }
    // A value from VALUES, and a location.
    let (_, located) = rows(
        &engine,
        "SELECT ?id WHERE { VALUES ?f { \"cells.csv\" } SERVICE <x-sparql-anything:csv.headers=true> { fx:properties fx:location ?f . ?r xyz:id ?id } }",
    );
    assert_eq!(located, ["id=\"1\"", "id=\"2\"", "id=\"3\""]);
}

/// `fx:root`, the root's type before SPARQL Anything 1.0, still finds the root.
#[test]
fn legacy_root_type() {
    let dir = dir();
    let engine = engine(Some(&dir));
    for query in [
        "SELECT ?code WHERE { SERVICE <x-sparql-anything:location=stations.json> { ?root a fx:root ; ?slot ?s . ?s xyz:code ?code } }",
        "SELECT ?code WHERE { SERVICE <x-sparql-anything:location=stations.json> { ?root a ?type ; ?slot ?s . ?s xyz:code ?code FILTER(?type = fx:root) } }",
    ] {
        let (_, codes) = rows(&engine, query);
        assert_eq!(codes, ["code=\"LLS\"", "code=\"OSL\""], "{query}");
    }
}

/// A GeoJSON file lifted with `geo=true` gives WKT literals that the spatial functions and
/// the spatial index answer on.
#[test]
fn geojson_lifted_with_geometry_is_found_spatially() {
    let dir = dir();
    let engine = engine(Some(&dir));
    write(
        dir.join("stations.geojson"),
        r#"{"type":"FeatureCollection","features":[
          {"type":"Feature","properties":{"code":"OSL"},"geometry":{"type":"Point","coordinates":[10.7522,59.9111]}},
          {"type":"Feature","properties":{"code":"BGO"},"geometry":{"type":"Point","coordinates":[5.3336,60.3903]}}]}"#,
    );
    engine
        .update(&format!(
            "{PREFIXES} PREFIX geo: <http://www.opengis.net/ont/geosparql#>
            INSERT {{ ?s geo:hasGeometry ?gs . ?gs geo:asWKT ?wkt }} WHERE {{
              SERVICE <x-sparql-anything:location=stations.geojson,geo=true> {{
                ?f geo:hasGeometry ?g ; xyz:properties/xyz:code ?code . ?g geo:asWKT ?wkt }}
              BIND(fx:entity(ex:, ?code) AS ?s)
              BIND(fx:entity(ex:, ?code, \"/geom\") AS ?gs) }}"
        ))
        .unwrap();
    let QueryResults::Solutions(solutions) = engine
        .query(&format!(
            "{PREFIXES} PREFIX geo: <http://www.opengis.net/ont/geosparql#>
            PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
            SELECT ?s WHERE {{ ?s geo:hasGeometry/geo:asWKT ?w .
              FILTER(geof:sfIntersects(?w, \"POLYGON((10 59, 11 59, 11 60, 10 60, 10 59))\"^^geo:wktLiteral)) }}"
        ))
        .unwrap()
    else {
        panic!()
    };
    let found: Vec<String> = solutions
        .map(|s| s.unwrap().get("s").unwrap().to_string())
        .collect();
    assert_eq!(found, ["<http://example.org/OSL>"]);
}

/// `slice=true` evaluates its pattern on a thread of its own, which needs as deep a stack as
/// the query threads: a deep pattern within the query limits answered on the query thread
/// but overflowed a default 2 MB stack and aborted the process (800 OPTIONALs do in a debug
/// build, 3,000 in a release one).
#[test]
fn sliced_patterns_get_the_query_stack() {
    let dir = tripletrail_testdir::tempdir("fx-slice-stack");
    std::fs::write(dir.join("two.csv"), "name,a\nx,1\ny,2\n").unwrap();
    let engine = Engine::new_tripletrail().with_fx(FxConfig {
        fixed_limits: false,
        databases: None,
        base: dir.to_path_buf(),
        allowed: Some(vec![dir.to_path_buf()]),
        remote: Default::default(),
    });
    let optionals: String = (0..800)
        .map(|i| format!(" OPTIONAL {{ ?r xyz:a ?a{i} }}"))
        .collect();
    let query = move |slice: bool| {
        format!(
            "{PREFIXES}SELECT ?n WHERE {{ SERVICE <x-sparql-anything:location=two.csv,csv.headers=true,slice={slice}> {{ ?r xyz:name ?n{optionals} }} }}"
        )
    };
    let (plain, sliced) = std::thread::Builder::new()
        .stack_size(tripletrail::complexity::QUERY_STACK)
        .spawn(move || {
            (
                rows(&engine, &query(false)).1,
                rows(&engine, &query(true)).1,
            )
        })
        .unwrap()
        .join()
        .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(plain.len(), 2);
    assert_eq!(sliced, plain);
}
