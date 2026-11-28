//! Numeric range FILTERs answered from the value-ordered index (issue #71) give the same
//! rows as the FILTER alone, whatever the literal types, bounds and writes in between.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

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

/// The same query on an engine without the rewrite.
fn reference(data: &str, query: &str) -> Vec<String> {
    let engine = Engine::new_tripletrail();
    engine.set_spatial_rewrite(false);
    engine.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    rows(&engine, query)
}

fn data() -> String {
    let mut ttl = String::from(
        "@prefix ex: <http://example.org/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
    );
    for i in 0..3000 {
        let km = (i * 37 % 2000) as f64 / 10.0;
        // Every numeric spelling, plus values that are not numbers at all.
        let literal = match i % 7 {
            0 => format!("\"{km}\"^^xsd:decimal"),
            1 => format!("\"{km:e}\"^^xsd:double"),
            2 => format!("{}", km as i64),
            3 => format!("\"+{}\"^^xsd:int", km as i64),
            4 => format!("\"{km}\"^^xsd:float"),
            5 => format!("\"{km}\""),
            _ => format!("\"0{}\"^^xsd:integer", km as i64),
        };
        ttl.push_str(&format!(
            "ex:s{i} a ex:Signal ; ex:ref ex:lr{i} . ex:lr{i} ex:line ex:line{} ; ex:km {literal} .\n",
            i % 40
        ));
    }
    ttl.push_str("ex:x ex:km \"NaN\"^^xsd:double . ex:y ex:km \"INF\"^^xsd:double . ex:z ex:km \"-INF\"^^xsd:double .\n");
    ttl.push_str("ex:inside ex:km 100.5 .\n");
    ttl
}

const P: &str =
    "PREFIX ex: <http://example.org/>\nPREFIX xsd: <http://www.w3.org/2001/XMLSchema#>\n";

#[test]
fn range_filters_give_the_same_rows() {
    let data = data();
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    let queries = [
        "SELECT ?s ?km WHERE { ?s ex:ref ?lr . ?lr ex:km ?km . FILTER(?km >= 100 && ?km <= 102.5) }",
        "SELECT ?s WHERE { ?s ex:ref ?lr . ?lr ex:km ?km . FILTER(?km > 100.0 && ?km < 102.5) }",
        "SELECT ?s WHERE { ?s ex:ref ?lr . ?lr ex:km ?km . FILTER(100 < ?km && 101 >= ?km) }",
        "SELECT ?lr WHERE { ?lr ex:km ?km . FILTER(?km >= \"1.995e2\"^^xsd:double) }",
        "SELECT ?lr WHERE { ?lr ex:km ?km . FILTER(?km < -1) }",
        "SELECT ?lr WHERE { ?lr ex:km ?km . FILTER(?km >= 5 && ?km <= 4) }",
        "SELECT ?s WHERE { ?s a ex:Signal ; ex:ref ?lr . ?lr ex:line ex:line7 ; ex:km ?km . FILTER(?km >= 50 && ?km <= 60) }",
        "SELECT ?lr ?km WHERE { ?lr ex:km ?km . FILTER(?km >= 199 && ?km <= \"INF\"^^xsd:double && isLiteral(?km)) }",
        "SELECT (COUNT(*) AS ?n) WHERE { ?lr ex:km ?km . FILTER(?km >= 10 && ?km <= 11) }",
        "SELECT ?lr WHERE { ?lr ex:km ?km . OPTIONAL { ?lr ex:line ?l } FILTER(?km >= 10 && ?km <= 11) }",
    ];
    for q in queries {
        let q = format!("{P}{q}");
        assert_eq!(rows(&engine, &q), reference(&data, &q), "{q}");
    }
    // Writes patch the index of the predicate: the next query sees them.
    engine
        .update(&format!("{P}INSERT DATA {{ ex:new ex:km 100.75 . ex:new2 ex:km \"100.8\"^^xsd:decimal }} ; DELETE WHERE {{ ex:lr0 ex:km ?k }}"))
        .unwrap();
    let q =
        format!("{P}SELECT ?lr WHERE {{ ?lr ex:km ?km . FILTER(?km >= 100.7 && ?km <= 100.9) }}");
    let got = rows(&engine, &q);
    assert!(
        got.iter()
            .any(|r| r.contains("ex:new>") || r.ends_with("/new>")),
        "{got:?}"
    );
    assert!(got.iter().any(|r| r.ends_with("/new2>")), "{got:?}");
}

#[test]
fn explain_says_what_was_decided() {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    let narrow = engine
        .explain(&format!("{P}SELECT ?s WHERE {{ ?s ex:ref ?lr . ?lr ex:km ?km . FILTER(?km >= 100 && ?km <= 100.5) }}"))
        .unwrap();
    assert!(
        narrow.contains("range ?km in [100, 100.5]") && narrow.contains(": inject"),
        "{narrow}"
    );
    let wide = engine
        .explain(&format!("{P}SELECT ?s WHERE {{ ?s ex:ref ?lr . ?lr ex:km ?km . FILTER(?km >= 0 && ?km <= 150) }}"))
        .unwrap();
    assert!(wide.contains(": filter"), "{wide}");
    // Inside GRAPH, and with FROM, the default-graph index does not apply.
    let graph = engine
        .explain(&format!("{P}SELECT ?s WHERE {{ GRAPH ?g {{ ?s ex:km ?km }} FILTER(?km >= 100 && ?km <= 100.5) }}"))
        .unwrap();
    assert!(!graph.contains("range ?km"), "{graph}");
}

/// Dates and date-times, with and without timezones, and a few that are neither.
fn temporal_data() -> String {
    let mut ttl = String::from(
        "@prefix ex: <http://example.org/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
    );
    for i in 0..3000 {
        let day = 1 + i % 28;
        let month = 1 + (i / 28) % 12;
        let year = 2020 + i % 8;
        let hour = (i * 7) % 24;
        let literal = match i % 8 {
            0 => format!("\"{year}-{month:02}-{day:02}\"^^xsd:date"),
            1 => format!("\"{year}-{month:02}-{day:02}Z\"^^xsd:date"),
            2 => format!("\"{year}-{month:02}-{day:02}+13:00\"^^xsd:date"),
            3 => format!("\"{year}-{month:02}-{day:02}T{hour:02}:30:00\"^^xsd:dateTime"),
            4 => format!("\"{year}-{month:02}-{day:02}T{hour:02}:30:00.25Z\"^^xsd:dateTime"),
            5 => format!("\"{year}-{month:02}-{day:02}T{hour:02}:00:00-11:00\"^^xsd:dateTimeStamp"),
            6 => format!("\"{year}-{month:02}-{day:02}\""),
            _ => format!("{}", year * 10_000 + month * 100 + day),
        };
        ttl.push_str(&format!(
            "ex:p{i} a ex:Period ; ex:line ex:line{} ; ex:from {literal} .\n",
            i % 40
        ));
    }
    // Inside the ranges below only once their timezones are applied.
    ttl.push_str("ex:tz1 ex:from \"2024-03-01T20:00:00-11:00\"^^xsd:dateTime . ex:tz2 ex:from \"2024-03-03+14:00\"^^xsd:date .\n");
    ttl.push_str("ex:bad ex:from \"2024-13-40\"^^xsd:date . ex:edge ex:from \"2024-12-31T24:00:00\"^^xsd:dateTime .\n");
    ttl
}

/// #312: date and date-time ranges give the rows the FILTER alone gives, by both executors,
/// before and after writes.
#[test]
fn date_and_date_time_ranges_give_the_same_rows() {
    let data = temporal_data();
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    let queries = [
        "SELECT ?p ?d WHERE { ?p ex:from ?d . FILTER(?d >= \"2024-03-01\"^^xsd:date && ?d < \"2024-03-08\"^^xsd:date) }",
        "SELECT ?p WHERE { ?p ex:from ?d . FILTER(?d > \"2024-03-01Z\"^^xsd:date && ?d <= \"2024-03-03-05:00\"^^xsd:date) }",
        "SELECT ?p ?d WHERE { ?p ex:from ?d . FILTER(?d >= \"2025-06-01T00:00:00\"^^xsd:dateTime && ?d < \"2025-06-03T12:00:00Z\"^^xsd:dateTime) }",
        "SELECT ?p WHERE { ?p ex:from ?d . FILTER(\"2025-01-05T00:00:00+14:00\"^^xsd:dateTime <= ?d && \"2025-01-06T00:00:00-14:00\"^^xsd:dateTime > ?d) }",
        "SELECT ?p WHERE { ?p ex:from ?d . FILTER(?d >= \"2027-12-28\"^^xsd:date) }",
        "SELECT ?p WHERE { ?p ex:from ?d . FILTER(?d < \"2020-01-02T00:00:00Z\"^^xsd:dateTimeStamp) }",
        "SELECT ?p WHERE { ?p ex:from ?d . FILTER(?d >= \"2025-01-01T00:00:00\"^^xsd:dateTime && ?d <= \"2025-01-01T00:00:00\"^^xsd:dateTime) }",
        "SELECT ?p WHERE { ?p ex:from ?d . FILTER(?d >= \"2024-12-31T23:00:00\"^^xsd:dateTime && ?d <= \"2025-01-01T01:00:00\"^^xsd:dateTime) }",
        "SELECT ?p WHERE { ?p ex:from ?d . FILTER(?d >= \"2024-03-02T00:00:00Z\"^^xsd:dateTime && ?d < \"2024-03-02T08:00:00Z\"^^xsd:dateTime) }",
        "SELECT ?p WHERE { ?p ex:from ?d . FILTER(?d >= \"2024-03-02Z\"^^xsd:date && ?d < \"2024-03-02-12:00\"^^xsd:date) }",
        // A date bound and a date-time bound on one variable: no block, the FILTER alone.
        "SELECT ?p WHERE { ?p ex:from ?d . FILTER(?d >= \"2024-01-01\"^^xsd:date && ?d < \"2024-02-01T00:00:00Z\"^^xsd:dateTime) }",
        "SELECT ?p WHERE { ?p a ex:Period ; ex:line ex:line3 ; ex:from ?d . FILTER(?d >= \"2023-05-01\"^^xsd:date && ?d <= \"2023-05-20\"^^xsd:date) }",
        "SELECT (COUNT(*) AS ?n) WHERE { ?p ex:from ?d . FILTER(?d >= \"2026-02-01\"^^xsd:date && ?d <= \"2026-02-10\"^^xsd:date) }",
    ];
    let check = |engine: &Engine, data: &str| {
        for q in queries {
            for executor in ["ids", "spareval"] {
                let q = format!("#pragma tt.executor {executor}\n{P}{q}");
                let off = format!("#pragma tt.range-rewrite off\n{q}");
                let got = rows(engine, &q);
                assert_eq!(got, rows(engine, &off), "{q}");
                assert_eq!(got, reference(data, &q), "{q}");
            }
        }
    };
    check(&engine, &data);
    let narrow = engine
        .explain(&format!("{P}SELECT ?p WHERE {{ ?p ex:from ?d . FILTER(?d >= \"2024-03-01\"^^xsd:date && ?d < \"2024-03-03\"^^xsd:date) }}"))
        .unwrap();
    assert!(
        narrow.contains("(dates, seconds from 1970)") && narrow.contains(": inject"),
        "{narrow}"
    );
    // Writes patch the date and date-time columns: the next queries see them.
    let write = "INSERT DATA { ex:n1 ex:from \"2024-03-02\"^^xsd:date . ex:n2 ex:from \"2025-06-02T08:00:00+02:00\"^^xsd:dateTime } ; DELETE WHERE { ex:p0 ex:from ?d } ; DELETE WHERE { ex:p3 ex:from ?d }";
    engine.update(&format!("{P}{write}")).unwrap();
    let reference_engine = Engine::new_tripletrail();
    reference_engine
        .load(RdfFormat::Turtle, data.as_bytes())
        .unwrap();
    reference_engine.update(&format!("{P}{write}")).unwrap();
    for q in queries {
        let q = format!("{P}{q}");
        let off = format!("#pragma tt.range-rewrite off\n{q}");
        assert_eq!(rows(&engine, &q), rows(&reference_engine, &off), "{q}");
    }
    let got = rows(&engine, &format!("{P}{}", queries[0]));
    assert!(got.iter().any(|r| r.contains("/n1>")), "{got:?}");
    for (q, edge) in [(8, "/tz1>"), (9, "/tz2>")] {
        let got = rows(&engine, &format!("{P}{}", queries[q]));
        assert!(got.iter().any(|r| r.contains(edge)), "{got:?}");
    }
}
