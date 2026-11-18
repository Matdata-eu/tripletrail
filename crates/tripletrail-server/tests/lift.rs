//! `tripletrail lift`: a folder of Facade-X CONSTRUCT queries over files, in parallel.

use std::path::Path;
use std::process::Command;

fn scratch(name: &str) -> tripletrail_testdir::TestDir {
    let dir = tripletrail_testdir::tempdir(&format!("lift-{name}"));
    std::fs::create_dir_all(dir.join("data")).unwrap();
    std::fs::create_dir_all(dir.join("queries/stations")).unwrap();
    std::fs::write(
        dir.join("data/stations.json"),
        r#"[{"code":"OSL","name":"Oslo S"},{"code":"LLS","name":"Lillestrøm"}]"#,
    )
    .unwrap();
    let prefixes = "PREFIX xyz: <http://sparql.xyz/facade-x/data/>\nPREFIX fx: <http://sparql.xyz/facade-x/ns/>\n";
    std::fs::write(
        dir.join("queries/stations/names.rq"),
        format!(
            "{prefixes}CONSTRUCT {{ ?st <http://ex.org/name> ?name }} WHERE {{
                SERVICE <x-sparql-anything:location=data/stations.json> {{ ?s xyz:code ?c ; xyz:name ?name }}
                BIND(fx:entity(\"http://ex.org/\", ?c) AS ?st) }}"
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("queries/codes.rq"),
        format!(
            "{prefixes}CONSTRUCT {{ ?st <http://ex.org/code> ?c }} WHERE {{
                SERVICE <x-sparql-anything:location=data/stations.json> {{ ?s xyz:code ?c }}
                BIND(fx:entity(\"http://ex.org/\", ?c) AS ?st) }}"
        ),
    )
    .unwrap();
    dir
}

fn tripletrail(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tripletrail"))
        .current_dir(dir)
        .arg("lift")
        .args(args)
        .output()
        .unwrap()
}

fn sorted_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    lines.sort();
    lines
}

#[test]
fn lifts_a_folder_into_one_file_and_per_query_files() {
    let dir = scratch("folder");
    let out = tripletrail(&dir, &["queries", "--output", "all.nt", "-j", "2"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        sorted_lines(&std::fs::read_to_string(dir.join("all.nt")).unwrap()),
        vec![
            "<http://ex.org/LLS> <http://ex.org/code> \"LLS\" .",
            "<http://ex.org/LLS> <http://ex.org/name> \"Lillestrøm\" .",
            "<http://ex.org/OSL> <http://ex.org/code> \"OSL\" .",
            "<http://ex.org/OSL> <http://ex.org/name> \"Oslo S\" .",
        ]
    );
    let out = tripletrail(&dir, &["queries", "--output-dir", "out", "--allow", "data"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("out/codes.nt"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("out/stations/names.nt"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[test]
fn a_failing_query_fails_the_run_but_not_the_others() {
    let dir = scratch("failing");
    std::fs::write(
        dir.join("queries/bad.rq"),
        "CONSTRUCT { ?s ?p ?o } WHERE { SERVICE <x-sparql-anything:location=missing.json> { ?s ?p ?o } }",
    )
    .unwrap();
    std::fs::write(
        dir.join("queries/stations/codes.yarrrml"),
        "prefixes:\n  ex: http://ex.org/\nmappings:\n  station:\n    sources: [[data/stations.json~jsonpath, \"$[*]\"]]\n    s: ex:$(code)\n    po:\n      - [ex:code, $(code)]\n",
    )
    .unwrap();
    let out = tripletrail(&dir, &["queries", "--output-dir", "out"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("failed: queries/bad.rq"), "{stderr}");
    assert!(dir.join("out/codes.nt").exists());
    // Files outside --allow are refused.
    let out = tripletrail(&dir, &["queries/codes.rq", "--allow", "queries"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("outside"));
}

#[test]
fn lifts_rml_mappings_next_to_queries() {
    let dir = scratch("rml");
    std::fs::write(
        dir.join("queries/stations/labels.rml.ttl"),
        r#"@prefix rr: <http://www.w3.org/ns/r2rml#> .
           @prefix rml: <http://semweb.mmlab.be/ns/rml#> .
           <#Stations> rml:logicalSource [ rml:source "data/stations.json" ; rml:iterator "$[*]" ] ;
             rr:subjectMap [ rr:template "http://ex.org/{code}" ] ;
             rr:predicateObjectMap [ rr:predicate <http://ex.org/label> ;
                                     rr:objectMap [ rml:reference "name" ; rr:language "no" ] ] ."#,
    )
    .unwrap();
    std::fs::write(
        dir.join("queries/stations/codes.yarrrml"),
        "prefixes:\n  ex: http://ex.org/\nmappings:\n  station:\n    sources: [[data/stations.json~jsonpath, \"$[*]\"]]\n    s: ex:$(code)\n    po:\n      - [ex:code, $(code)]\n",
    )
    .unwrap();
    let out = tripletrail(&dir, &["queries", "--output-dir", "out"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let codes = std::fs::read_to_string(dir.join("out/stations/codes.nt")).unwrap();
    assert_eq!(
        sorted_lines(&codes),
        [
            r#"<http://ex.org/LLS> <http://ex.org/code> "LLS" ."#,
            r#"<http://ex.org/OSL> <http://ex.org/code> "OSL" ."#,
        ]
    );
    let labels = std::fs::read_to_string(dir.join("out/stations/labels.rml.nt")).unwrap();
    assert_eq!(
        sorted_lines(&labels),
        [
            r#"<http://ex.org/LLS> <http://ex.org/label> "Lillestrøm"@no ."#,
            r#"<http://ex.org/OSL> <http://ex.org/label> "Oslo S"@no ."#,
        ]
    );
    // The queries ran alongside.
    assert!(dir.join("out/codes.nt").exists());
    // --explain: the plans instead of the triples.
    let out = tripletrail(&dir, &["queries", "--explain"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let plan = String::from_utf8_lossy(&out.stdout);
    for part in [
        "labels.rml.ttl",
        "source file data/stations.json (JSONPath, iterator $[*])",
        "subject template \"http://ex.org/{code}\" (IRI)",
        "=> 2 records, 2 quads",
        // The queries' plans, evaluated.
        "codes.rq",
    ] {
        assert!(plan.contains(part), "{part}\n{plan}");
    }
}

#[test]
#[cfg(feature = "postgres")]
#[ignore = "needs TRIPLETRAIL_TEST_POSTGRES, a PostgreSQL database it may write a table to"]
fn lifts_r2rml_mappings_from_postgres() {
    let url = std::env::var("TRIPLETRAIL_TEST_POSTGRES").expect("set TRIPLETRAIL_TEST_POSTGRES");
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    client
        .batch_execute(
            "DROP TABLE IF EXISTS lift_line; CREATE TABLE lift_line (nr integer, km real); \
             INSERT INTO lift_line VALUES (50, 41.5);",
        )
        .unwrap();
    let dir = scratch("r2rml");
    std::fs::write(
        dir.join("queries/lines.r2rml.ttl"),
        r#"@prefix rr: <http://www.w3.org/ns/r2rml#> .
           <http://ex.org/m#Lines> rr:logicalTable [ rr:tableName "lift_line" ] ;
             rr:subjectMap [ rr:template "http://ex.org/line/{nr}" ] ;
             rr:predicateObjectMap [ rr:predicate <http://ex.org/km> ;
                                     rr:objectMap [ rr:column "km" ] ] ."#,
    )
    .unwrap();
    // Without a database the mapping fails, with one it reads the table.
    let out = tripletrail(&dir, &["queries/lines.r2rml.ttl"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--database"));
    let out = tripletrail(&dir, &["queries/lines.r2rml.ttl", "--database", &url]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        r#"<http://ex.org/line/50> <http://ex.org/km> "4.15E1"^^<http://www.w3.org/2001/XMLSchema#double> ."#
    );
}

#[test]
#[cfg(feature = "postgres")]
#[ignore = "needs TRIPLETRAIL_TEST_POSTGRES, a PostgreSQL database it may write a table to"]
fn lifts_facade_x_tables_and_queries_from_postgres() {
    let url = std::env::var("TRIPLETRAIL_TEST_POSTGRES").expect("set TRIPLETRAIL_TEST_POSTGRES");
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    client
        .batch_execute(
            "DROP TABLE IF EXISTS fx_station; \
             CREATE TABLE fx_station (code text, km numeric, opened date, wkt text, gone boolean); \
             INSERT INTO fx_station VALUES ('OSL', 0.00, '1980-06-01', 'POINT(10.75 59.91)', false), \
                                           ('LLS', 21.50, NULL, NULL, NULL);",
        )
        .unwrap();
    let dir = scratch("fx-db");
    let prefixes = "PREFIX xyz: <http://sparql.xyz/facade-x/data/>\n\
                    PREFIX fx: <http://sparql.xyz/facade-x/ns/>\n\
                    PREFIX fxe: <http://sparql.xyz/facade-x/engine/>\n";
    std::fs::write(
        dir.join("queries/stations.rq"),
        format!(
            "{prefixes}CONSTRUCT {{ ?st ?p ?o }} WHERE {{
                SERVICE <x-sparql-anything:table=fx_station,geo.wkt=wkt,geo.crs=EPSG:4258> {{
                  ?s xyz:code ?c ; ?p ?o FILTER(?p != xyz:code && ?p != xyz:wkt) }}
                BIND(fx:entity(\"http://ex.org/\", ?c) AS ?st) }}"
        ),
    )
    .unwrap();
    // Without a database the query fails, and a query cannot write.
    let out = tripletrail(&dir, &["queries/stations.rq"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--database"));
    let out = tripletrail(&dir, &["queries/stations.rq", "--database", &url]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        sorted_lines(&String::from_utf8_lossy(&out.stdout)),
        sorted_lines(
            r#"<http://ex.org/OSL> <http://sparql.xyz/facade-x/data/km> "0.0"^^<http://www.w3.org/2001/XMLSchema#decimal> .
<http://ex.org/OSL> <http://sparql.xyz/facade-x/data/opened> "1980-06-01"^^<http://www.w3.org/2001/XMLSchema#date> .
<http://ex.org/OSL> <http://sparql.xyz/facade-x/data/gone> "false"^^<http://www.w3.org/2001/XMLSchema#boolean> .
<http://ex.org/OSL> <http://www.opengis.net/ont/geosparql#asWKT> "<http://www.opengis.net/def/crs/EPSG/0/4258> POINT(10.75 59.91)"^^<http://www.opengis.net/ont/geosparql#wktLiteral> .
<http://ex.org/LLS> <http://sparql.xyz/facade-x/data/km> "21.5"^^<http://www.w3.org/2001/XMLSchema#decimal> ."#
        )
    );

    // A query on a named connection, its parameters through fx:properties; a statement
    // that writes is refused, and so is a result over the row cap.
    let query = |sql: &str, extra: &str| {
        format!(
            "{prefixes}CONSTRUCT {{ ?s <http://ex.org/code> ?c }} WHERE {{
                SERVICE <x-sparql-anything:> {{
                  fxe:properties fxe:sql \"{sql}\" ; fxe:database \"rail\" {extra} .
                  ?s xyz:code ?c }} }}"
        )
    };
    std::fs::write(
        dir.join("queries/stations.rq"),
        query(
            "SELECT code, km, opened FROM fx_station WHERE km > 1 -- the far ones",
            "",
        ),
    )
    .unwrap();
    let named = format!("rail={url}");
    let out = tripletrail(&dir, &["queries/stations.rq", "--connection", &named]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains(r#""LLS""#));
    // Only `code` is read, except when a name is given twice and cannot be selected.
    std::fs::write(
        dir.join("queries/stations.rq"),
        query("SELECT code, km, km FROM fx_station ORDER BY code", ""),
    )
    .unwrap();
    let out = tripletrail(&dir, &["queries/stations.rq", "--connection", &named]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 2);
    for (sql, extra, error) in [
        ("DELETE FROM fx_station RETURNING code", "", "DELETE"),
        (
            "SELECT 1 AS code; DELETE FROM fx_station",
            "",
            "multiple commands",
        ),
        (
            "SELECT code FROM fx_station",
            "; fxe:database.max-rows \"1\"",
            "more than 1 rows",
        ),
    ] {
        std::fs::write(dir.join("queries/stations.rq"), query(sql, extra)).unwrap();
        let out = tripletrail(&dir, &["queries/stations.rq", "--connection", &named]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success() && stderr.contains(error),
            "{sql}: {stderr}"
        );
    }
    // FILTERs on one row go to PostgreSQL; the answers stay those of SPARQL.
    for (filter, expected) in [
        ("FILTER(?km > 1 && ?c = 'LLS')", vec!["LLS"]),
        ("FILTER(?km >= 0.0 && ?km < 1)", vec!["OSL"]),
        ("FILTER(?c = 'OSL' && ?gone = false)", vec!["OSL"]),
        ("FILTER(?c = \"LLS'; DROP TABLE fx_station; --\")", vec![]),
        ("FILTER(?km = 2.15E1)", vec!["LLS"]),
        // Constants PostgreSQL would refuse are not passed on.
        ("FILTER(?c = \"a\\u0000b\")", vec![]),
    ] {
        std::fs::write(
            dir.join("queries/stations.rq"),
            format!(
                "{prefixes}CONSTRUCT {{ ?s <http://ex.org/code> ?c }} WHERE {{
                    SERVICE <x-sparql-anything:table=fx_station> {{
                      ?s xyz:code ?c ; xyz:km ?km ; xyz:gone ?gone {filter} }} }}"
            )
            .replace(
                "; xyz:gone ?gone",
                if filter.contains("gone") {
                    "; xyz:gone ?gone"
                } else {
                    ""
                },
            ),
        )
        .unwrap();
        let out = tripletrail(&dir, &["queries/stations.rq", "--database", &url]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{filter}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut codes: Vec<&str> = stdout.lines().filter_map(|l| l.split('"').nth(1)).collect();
        codes.sort();
        assert_eq!(codes, expected, "{filter}");
    }
    let count: i64 = client
        .query_one("SELECT count(*) FROM fx_station", &[])
        .unwrap()
        .get(0);
    assert_eq!(count, 2);

    // Joined to values outside it, the SERVICE is called with them (a bind join), and the
    // database gets them as a WHERE.
    for (outer, expected) in [
        (r#"VALUES ?c { "LLS" "XYZ" }"#, vec!["LLS"]),
        (
            r#"VALUES ?c { "LLS" "OSL" "LLS" }"#,
            vec!["LLS", "LLS", "OSL"],
        ),
        // A decimal column never holds an integer or a string: nothing joins.
        ("VALUES ?km { 0 1 }", vec![]),
        // A string may be a NaN: then every row is read.
        (r#"VALUES ?km { 0 "0.0" }"#, vec![]),
        ("VALUES ?km { 21.5 }", vec!["LLS"]),
        (r#"VALUES ?c { "OSL" "a\u0000b" }"#, vec!["OSL"]),
    ] {
        std::fs::write(
            dir.join("queries/joined.rq"),
            format!(
                "{prefixes}CONSTRUCT {{ ?s <http://ex.org/code> ?c }} WHERE {{ {outer}
                    SERVICE <x-sparql-anything:table=fx_station> {{ ?s xyz:code ?c ; xyz:km ?km }} }}"
            ),
        )
        .unwrap();
        let out = tripletrail(&dir, &["queries/joined.rq", "--database", &url]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{outer}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // CONSTRUCT keeps each triple once.
        let mut codes: Vec<&str> = stdout.lines().filter_map(|l| l.split('"').nth(1)).collect();
        codes.sort();
        codes.dedup();
        let mut expected = expected;
        expected.dedup();
        assert_eq!(codes, expected, "{outer}");
    }

    // `--explain` shows each database query, with its parameters and rows.
    std::fs::write(
        dir.join("queries/joined.rq"),
        format!(
            "{prefixes}CONSTRUCT {{ ?s <http://ex.org/code> ?c }} WHERE {{ VALUES ?c {{ \"LLS\" \"XYZ\" }}
                SERVICE <x-sparql-anything:table=fx_station> {{ ?s xyz:code ?c ; xyz:km ?km }} }}"
        ),
    )
    .unwrap();
    let out = tripletrail(
        &dir,
        &["queries/joined.rq", "--database", &url, "--explain"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains(r#""databases":[{"connection":null,"sql":"SELECT \"code\", \"km\" FROM (TABLE fx_station\u000a) AS tripletrail_fx_rows WHERE \"code\" IN ($1, $2)","params":["LLS","XYZ"],"rows":1,"#),
        "{stdout}"
    );

    // A LIMIT on a query that is one SERVICE becomes SQL LIMIT; a row that gives no answer
    // (a FILTER PostgreSQL does not check) makes it ask again for twice as many.
    std::fs::write(
        dir.join("queries/limited.rq"),
        format!(
            "{prefixes}CONSTRUCT {{ ?s <http://ex.org/code> ?c }} WHERE {{
                SERVICE <x-sparql-anything:table=fx_station> {{ ?s xyz:code ?c FILTER(STRSTARTS(?c, \"L\")) }} }}
             LIMIT 1"
        ),
    )
    .unwrap();
    for explain in [false, true] {
        let mut args = vec!["queries/limited.rq", "--database", &url];
        if explain {
            args.push("--explain");
        }
        let out = tripletrail(&dir, &args);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        if explain {
            assert!(
                stdout.contains(r#"AS tripletrail_fx_rows LIMIT 1","params":[],"rows":1,"#),
                "{stdout}"
            );
            assert!(
                stdout.contains(r#"AS tripletrail_fx_rows LIMIT 2","params":[],"rows":2,"#),
                "{stdout}"
            );
        } else {
            assert_eq!(stdout.trim().lines().count(), 1, "{stdout}");
            assert!(stdout.contains(r#""LLS""#), "{stdout}");
        }
    }

    // OFFSET + LIMIT past the largest number reads every row, not none.
    std::fs::write(
        dir.join("queries/limited.rq"),
        format!(
            "{prefixes}CONSTRUCT {{ ?s <http://ex.org/code> ?c }} WHERE {{
                SERVICE <x-sparql-anything:table=fx_station> {{ ?s xyz:code ?c }} }}
             OFFSET 1 LIMIT 18446744073709551615"
        ),
    )
    .unwrap();
    let out = tripletrail(&dir, &["queries/limited.rq", "--database", &url]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim().lines().count(),
        1
    );

    // Spatial FILTERs go to PostGIS as bounding-box overlaps, where it is installed.
    if client
        .batch_execute("CREATE EXTENSION IF NOT EXISTS postgis")
        .is_err()
    {
        eprintln!("PostGIS is not installed: spatial FILTERs not tested");
        return;
    }
    client
        .batch_execute(
            "DROP TABLE IF EXISTS fx_place; \
             CREATE TABLE fx_place (code text, g geometry); \
             INSERT INTO fx_place VALUES \
               ('OSL', 'SRID=4326;POINT(10.75 59.91)'), \
               ('BRG', 'SRID=4326;POINT(5.33 60.39)'), \
               ('ETR', 'SRID=4258;POINT(10.75 59.91)'), \
               ('UTM', 'SRID=25833;POINT(262000 6650000)'), \
               ('NUL', NULL); \
             DROP TABLE IF EXISTS fx_typed; \
             CREATE TABLE fx_typed (code text, g geometry(Point, 4326)); \
             INSERT INTO fx_typed SELECT code, g FROM fx_place WHERE ST_SRID(g) = 4326; \
             CREATE INDEX ON fx_typed USING gist (g)",
        )
        .unwrap();
    let oslo = "POLYGON((10 59, 11 59, 11 60, 10 60, 10 59))";
    // A column declared with its SRID gets a plain `&&`, which the index serves.
    let typed = [
        (
            "table=fx_typed",
            format!("geof:sfIntersects(?g, \"{oslo}\"^^geo:wktLiteral)"),
            vec!["OSL"],
        ),
        (
            "table=fx_typed",
            "geof:sfIntersects(?g, \"<http://www.opengis.net/def/crs/EPSG/0/25833> POINT(1 2)\"^^geo:wktLiteral)".to_owned(),
            vec![],
        ),
    ];
    let mixed = [
        (format!("geof:sfIntersects(?g, \"{oslo}\"^^geo:wktLiteral)"), vec!["ETR", "OSL"]),
        (format!("geof:sfWithin(?g, \"{oslo}\"^^geo:wktLiteral)"), vec!["ETR", "OSL"]),
        (
            "geof:sfIntersects(?g, \"<http://www.opengis.net/def/crs/EPSG/0/4326> POINT(60.39 5.33)\"^^geo:wktLiteral)".to_owned(),
            vec!["BRG"],
        ),
        (
            "geof:sfIntersects(\"<http://www.opengis.net/def/crs/EPSG/0/4258> POLYGON((59 10, 60 10, 60 11, 59 11, 59 10))\"^^geo:wktLiteral, ?g)".to_owned(),
            vec!["ETR", "OSL"],
        ),
        (
            "geof:sfIntersects(?g, \"<http://www.opengis.net/def/crs/EPSG/0/25833> POLYGON((261000 6649000, 263000 6649000, 263000 6651000, 261000 6649000))\"^^geo:wktLiteral)".to_owned(),
            vec!["UTM"],
        ),
    ];
    for (source, filter, expected) in mixed
        .into_iter()
        .map(|(f, e)| ("table=fx_place", f, e))
        .chain(typed)
    {
        std::fs::write(
            dir.join("queries/places.rq"),
            format!(
                "{prefixes}PREFIX geo: <http://www.opengis.net/ont/geosparql#>
                 PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
                 CONSTRUCT {{ ?s <http://ex.org/code> ?c }} WHERE {{
                    SERVICE <x-sparql-anything:{source}> {{
                      ?s xyz:code ?c ; xyz:g ?g FILTER({filter}) }} }}"
            ),
        )
        .unwrap();
        let out = tripletrail(&dir, &["queries/places.rq", "--database", &url]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{filter}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut codes: Vec<&str> = stdout.lines().filter_map(|l| l.split('"').nth(1)).collect();
        codes.sort();
        assert_eq!(codes, expected, "{filter}");
    }

    // PostGIS in a schema off the search path, in a database of its own: the names are
    // qualified with the geometry type's schema.
    for statement in [
        "DROP DATABASE IF EXISTS tripletrail_fx_gis",
        "CREATE DATABASE tripletrail_fx_gis",
    ] {
        client.batch_execute(statement).unwrap();
    }
    let (base, query) = url.split_once('?').unwrap_or((&url, ""));
    let gis_url = format!(
        "{}/tripletrail_fx_gis{}{query}",
        base.rsplit_once('/').unwrap().0,
        if query.is_empty() { "" } else { "?" }
    );
    let mut gis = postgres::Client::connect(&gis_url, postgres::NoTls).unwrap();
    gis.batch_execute(
        "CREATE SCHEMA gis; CREATE EXTENSION postgis SCHEMA gis; \
         CREATE TABLE place (code text, g gis.geometry); \
         CREATE TABLE typed (code text, g gis.geometry(Point, 4326)); \
         INSERT INTO place VALUES ('A', 'SRID=4326;POINT(2 2)'), ('B', 'SRID=4326;POINT(20 20)'); \
         INSERT INTO typed SELECT * FROM place",
    )
    .unwrap();
    for table in ["place", "typed"] {
        std::fs::write(
            dir.join("queries/gis.rq"),
            format!(
                "{prefixes}PREFIX geo: <http://www.opengis.net/ont/geosparql#>
                 PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
                 CONSTRUCT {{ ?s <http://ex.org/code> ?c }} WHERE {{
                    SERVICE <x-sparql-anything:table={table}> {{ ?s xyz:code ?c ; xyz:g ?g
                      FILTER(geof:sfIntersects(?g, \"POLYGON((1 1,3 1,3 3,1 3,1 1))\"^^geo:wktLiteral)) }} }}"
            ),
        )
        .unwrap();
        let out = tripletrail(&dir, &["queries/gis.rq", "--database", &gis_url]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{table}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let codes: Vec<&str> = stdout.lines().filter_map(|l| l.split('"').nth(1)).collect();
        assert_eq!(codes, ["A"], "{table}");
    }
    drop(gis);
    client
        .batch_execute("DROP DATABASE tripletrail_fx_gis")
        .unwrap();
}

/// #377: the evaluator's CONSTRUCT stops filtering duplicates past 1,048,576 triples; lift
/// writes each triple once. Ignored: 1.65 M triples (about 20 s in release, much longer in
/// debug). `cargo test --release -p tripletrail-server --test lift -- --ignored`.
#[test]
#[ignore]
fn writes_each_triple_once_past_a_million() {
    let dir = scratch("duplicates");
    let mut csv = String::from("id,a,b,c,d,e,f,g,h\n");
    for i in 0..150_000 {
        csv.push_str(&format!("{i},a{i},b{i},c{i},d{i},e{i},f{i},g{i},h{i}\n"));
    }
    std::fs::write(dir.join("data/objects.csv"), csv).unwrap();
    std::fs::write(
        dir.join("queries/objects.rq"),
        r#"PREFIX fx: <http://sparql.xyz/facade-x/ns/>
PREFIX xyz: <http://sparql.xyz/facade-x/data/>
PREFIX ex: <http://example.org/>
CONSTRUCT { ?entity a ex:Object ; ex:key ?key ; ?p ?value ; ex:part ?part . ?part ?pp ?part_value }
WHERE {
  SERVICE <x-sparql-anything:location=data/objects.csv,csv.headers=true> {
    ?row xyz:id ?key .
    { ?row ?column ?value . FILTER(?column IN (xyz:a, xyz:b, xyz:c, xyz:d, xyz:e, xyz:f))
      BIND(fx:entity(ex:, STRAFTER(STR(?column), STR(xyz:))) AS ?p) }
    UNION
    { ?row ?column ?part_value . FILTER(?column IN (xyz:g, xyz:h))
      BIND(fx:entity(ex:, STRAFTER(STR(?column), STR(xyz:))) AS ?pp) }
  }
  BIND(fx:entity(ex:, "object_", ?key) AS ?entity)
  BIND(fx:entity(ex:, "part_", ?key) AS ?part)
}"#,
    )
    .unwrap();
    let out = tripletrail(&dir, &["queries/objects.rq", "--output", "out.nt"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::fs::read_to_string(dir.join("out.nt")).unwrap();
    let lines = text.lines().count();
    let mut distinct: Vec<&str> = text.lines().collect();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!((lines, distinct.len()), (1_650_000, 1_650_000));
}

#[test]
fn writes_logical_targets_below_the_output_dir() {
    let dir = scratch("targets");
    let mapping = |path: &str| {
        format!(
            r#"@prefix rml: <http://w3id.org/rml/> .
           @prefix formats: <http://www.w3.org/ns/formats/> .
           <#Stations> rml:logicalSource [ rml:source [ a rml:RelativePathSource ; rml:path "data/stations.json" ] ;
                                           rml:referenceFormulation rml:JSONPath ; rml:iterator "$[*]" ] ;
             rml:subjectMap [ rml:template "http://ex.org/{{code}}" ] ;
             rml:predicateObjectMap [ rml:predicate <http://ex.org/code> ; rml:objectMap [ rml:reference "code" ] ] ,
               [ rml:predicate <http://ex.org/name> ;
                 rml:objectMap [ rml:reference "name" ; rml:logicalTarget <#Names> ] ] .
           <#Names> rml:target [ a rml:FilePath ; rml:path "{path}" ; rml:compression rml:gzip ] ;
             rml:serialization formats:N-Triples ."#
        )
    };
    let queries = dir.join("queries");
    std::fs::remove_dir_all(&queries).unwrap();
    std::fs::create_dir_all(&queries).unwrap();
    std::fs::write(
        queries.join("stations.rml.ttl"),
        mapping("names/names.nt.gz"),
    )
    .unwrap();
    let out = tripletrail(&dir, &["queries", "--output-dir", "out"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The default output holds the quads with no target; the target file the others.
    let codes = std::fs::read_to_string(dir.join("out/stations.rml.nt")).unwrap();
    assert_eq!(sorted_lines(&codes).len(), 2, "{codes}");
    assert!(!codes.contains("Oslo"), "{codes}");
    let gz = std::fs::read(dir.join("out/names/names.nt.gz")).unwrap();
    assert_eq!(&gz[..2], [0x1f, 0x8b], "gzip");
    // A target may not leave the output directory.
    std::fs::write(queries.join("stations.rml.ttl"), mapping("../escape.nt.gz")).unwrap();
    let out = tripletrail(&dir, &["queries", "--output-dir", "out"]);
    assert!(!dir.join("escape.nt.gz").exists());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("relative path below"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Without --output-dir targets land in the base directory, where a file this run did
    // not write is refused rather than overwritten.
    std::fs::write(
        queries.join("stations.rml.ttl"),
        mapping("names/names.nt.gz"),
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("names")).unwrap();
    std::fs::write(dir.join("names/names.nt.gz"), "keep").unwrap();
    let out = tripletrail(&dir, &["queries", "--output", "all.nt"]);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("this run did not write"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("names/names.nt.gz")).unwrap(),
        "keep"
    );
    // --overwrite-targets replaces it, as a rerun needs.
    let out = tripletrail(
        &dir,
        &["queries", "--output", "all.nt", "--overwrite-targets"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        &std::fs::read(dir.join("names/names.nt.gz")).unwrap()[..2],
        [0x1f, 0x8b]
    );
}

#[test]
fn a_failed_mapping_writes_no_logical_targets() {
    let dir = scratch("targets-failed");
    let queries = dir.join("queries");
    std::fs::remove_dir_all(&queries).unwrap();
    std::fs::create_dir_all(&queries).unwrap();
    // The default output gets a named graph, which lift refuses; the target is not written.
    std::fs::write(
        queries.join("stations.rml.ttl"),
        r#"@prefix rml: <http://w3id.org/rml/> .
           @prefix formats: <http://www.w3.org/ns/formats/> .
           <#Stations> rml:logicalSource [ rml:source [ a rml:RelativePathSource ; rml:path "data/stations.json" ] ;
                                           rml:referenceFormulation rml:JSONPath ; rml:iterator "$[*]" ] ;
             rml:subjectMap [ rml:template "http://ex.org/{code}" ; rml:graph <http://ex.org/g> ] ;
             rml:predicateObjectMap [ rml:predicate <http://ex.org/code> ; rml:objectMap [ rml:reference "code" ] ] ,
               [ rml:predicate <http://ex.org/name> ;
                 rml:objectMap [ rml:reference "name" ; rml:logicalTarget <#Names> ] ] .
           <#Names> rml:target [ a rml:FilePath ; rml:path "names.nq" ] ;
             rml:serialization formats:N-Quads ."#,
    )
    .unwrap();
    let out = tripletrail(&dir, &["queries", "--output-dir", "out"]);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("named graph"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!dir.join("out/names.nq").exists());
}

#[test]
fn logical_targets_never_overwrite_inputs() {
    let dir = scratch("targets-inputs");
    let queries = dir.join("queries");
    std::fs::remove_dir_all(&queries).unwrap();
    std::fs::create_dir_all(&queries).unwrap();
    let mapping = |path: &str| {
        format!(
            r#"@prefix rml: <http://w3id.org/rml/> .
           @prefix formats: <http://www.w3.org/ns/formats/> .
           <#Stations> rml:logicalSource [ rml:source [ a rml:RelativePathSource ; rml:path "data/stations.json" ] ;
                                           rml:referenceFormulation rml:JSONPath ; rml:iterator "$[*]" ] ;
             rml:subjectMap [ rml:template "http://ex.org/{{code}}" ] ;
             rml:predicateObjectMap [ rml:predicate <http://ex.org/name> ;
                 rml:objectMap [ rml:reference "name" ; rml:logicalTarget <#T> ] ] .
           <#T> rml:target [ a rml:FilePath ; rml:path "{path}" ] ;
             rml:serialization formats:N-Triples ."#
        )
    };
    let before = std::fs::read(dir.join("data/stations.json")).unwrap();
    // Its own source, its own mapping file, and a file named like a pipeline: each refused,
    // even with --overwrite-targets.
    for path in [
        "data/stations.json",
        "queries/stations.rml.ttl",
        "pipeline.yaml",
    ] {
        std::fs::write(queries.join("stations.rml.ttl"), mapping(path)).unwrap();
        let out = tripletrail(
            &dir,
            &["queries", "--output", "all.nt", "--overwrite-targets"],
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("is an input of the run"),
            "{path}: {stderr}"
        );
    }
    assert_eq!(
        std::fs::read(dir.join("data/stations.json")).unwrap(),
        before
    );
    assert!(!dir.join("pipeline.yaml").exists());

    // A URL is no file.
    std::fs::write(
        queries.join("stations.rml.ttl"),
        mapping("http://127.0.0.1:9/out.nt"),
    )
    .unwrap();
    let out = tripletrail(&dir, &["queries", "--output", "all.nt"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("is a URL"), "{stderr}");
    assert!(!dir.join("http:").exists());

    // The source of another mapping of the run, named like any data file.
    std::fs::write(dir.join("data/other.json"), r#"[{"code":"X","name":"x"}]"#).unwrap();
    std::fs::write(queries.join("stations.rml.ttl"), mapping("data/other.json")).unwrap();
    std::fs::write(
        queries.join("other.rml.ttl"),
        mapping("out/other.nt").replace("data/stations.json", "data/other.json"),
    )
    .unwrap();
    let out = tripletrail(
        &dir,
        &["queries", "--output", "all.nt", "--overwrite-targets"],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("is an input of the run"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(dir.join("data/other.json")).unwrap(),
        r#"[{"code":"X","name":"x"}]"#
    );
}

/// Serves `body` at every path on a free port of 127.0.0.1, until the test ends.
fn serve(body: &'static str) -> String {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
            }
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    address
}

/// Queries and mappings read URLs, only those `--allow-url` allows.
#[test]
fn lifts_urls_on_the_allowlist() {
    let base = serve(r#"[{"code":"BRG","name":"Bergen"}]"#);
    let dir = scratch("urls");
    let queries = dir.join("queries");
    std::fs::remove_dir_all(&queries).unwrap();
    std::fs::create_dir_all(&queries).unwrap();
    std::fs::write(
        queries.join("names.rq"),
        format!(
            "PREFIX xyz: <http://sparql.xyz/facade-x/data/>
            CONSTRUCT {{ <http://ex.org/q> <http://ex.org/name> ?name }} WHERE {{
                SERVICE <x-sparql-anything:location={base}/stations.json> {{ ?s xyz:name ?name }} }}"
        ),
    )
    .unwrap();
    std::fs::write(
        queries.join("codes.rml.ttl"),
        format!(
            r#"@prefix rml: <http://w3id.org/rml/> .
            <#Stations> rml:logicalSource [ rml:source [ a rml:RelativePathSource ; rml:path "{base}/stations.json" ] ;
                                            rml:referenceFormulation rml:JSONPath ; rml:iterator "$[*]" ] ;
              rml:subjectMap [ rml:template "http://ex.org/{{code}}" ] ;
              rml:predicateObjectMap [ rml:predicate <http://ex.org/code> ; rml:objectMap [ rml:reference "code" ] ] ."#
        ),
    )
    .unwrap();
    let out = tripletrail(&dir, &["queries", "--output", "all.nt"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("reading URLs is off"), "{stderr}");
    let allow = format!("{base}/");
    let out = tripletrail(
        &dir,
        &["queries", "--output", "all.nt", "--allow-url", &allow],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let nt = std::fs::read_to_string(dir.join("all.nt")).unwrap();
    assert_eq!(
        sorted_lines(&nt),
        [
            "<http://ex.org/BRG> <http://ex.org/code> \"BRG\" .",
            "<http://ex.org/q> <http://ex.org/name> \"Bergen\" ."
        ]
    );
}
