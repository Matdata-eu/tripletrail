//! The window SERVICE (#237): SQL window functions over a sub-select's rows. Expected values
//! as PostgreSQL gives them for the same rows.

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;

const DATA: &str = r#"
@prefix ex: <http://example.com/> .
ex:s1 ex:line ex:L1 ; ex:km 1.5 .
ex:s2 ex:line ex:L1 ; ex:km 3.0 .
ex:s3 ex:line ex:L1 ; ex:km 3.0 .
ex:s4 ex:line ex:L1 ; ex:km 7.25 .
ex:s5 ex:line ex:L2 ; ex:km 2 .
ex:s6 ex:line ex:L2 ; ex:km 5 .
ex:s7 ex:line ex:L2 .
"#;

const PREFIXES: &str = "PREFIX ex: <http://example.com/>
PREFIX win: <http://w3id.org/tripletrail/window#>
";

fn engine() -> Engine {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    e
}

/// Rows as `?s` (local name) to the values of `cols`, as text ("" when unbound).
fn table(e: &Engine, q: &str, cols: &[&str]) -> Vec<(String, Vec<String>)> {
    let QueryResults::Solutions(s) = e.query(&format!("{PREFIXES}{q}")).unwrap() else {
        panic!()
    };
    let mut rows: Vec<(String, Vec<String>)> = s
        .map(|r| {
            let r = r.unwrap();
            let name = r.get("s").unwrap().to_string();
            let name = name
                .trim_end_matches('>')
                .rsplit('/')
                .next()
                .unwrap()
                .to_owned();
            let values = cols
                .iter()
                .map(|c| match r.get(*c) {
                    Some(tripletrail::oxigraph::model::Term::Literal(l)) => l.value().to_owned(),
                    Some(t) => t.to_string(),
                    None => String::new(),
                })
                .collect();
            (name, values)
        })
        .collect();
    rows.sort();
    rows
}

fn expect(rows: &[(String, Vec<String>)], expected: &[(&str, &[&str])]) {
    let got: Vec<(String, Vec<String>)> = rows.to_vec();
    let want: Vec<(String, Vec<String>)> = expected
        .iter()
        .map(|(s, v)| (s.to_string(), v.iter().map(|x| x.to_string()).collect()))
        .collect();
    assert_eq!(got, want);
}

#[test]
fn ranks_and_neighbours_per_partition() {
    let e = engine();
    let rows = table(
        &e,
        "SELECT * WHERE { SERVICE <http://w3id.org/tripletrail/window> {
           { SELECT ?s ?line ?km WHERE { ?s ex:line ?line ; ex:km ?km } }
           [] win:partitionBy ?line ; win:orderBy ?km ;
              win:rowNumber ?n ; win:rank ?r ; win:denseRank ?d ;
              win:lag (?km ?prev) ; win:lead (?km 1 \"end\" ?next) ;
              win:percentRank ?pr ; win:cumeDist ?cd .
         } }",
        &["n", "r", "d", "prev", "next", "pr", "cd"],
    );
    expect(
        &rows,
        &[
            ("s1", &["1", "1", "1", "", "3.0", "0", "0.25"]),
            (
                "s2",
                &["2", "2", "2", "1.5", "3.0", "0.3333333333333333", "0.75"],
            ),
            (
                "s3",
                &["3", "2", "2", "3.0", "7.25", "0.3333333333333333", "0.75"],
            ),
            ("s4", &["4", "4", "3", "3.0", "end", "1", "1"]),
            ("s5", &["1", "1", "1", "", "5", "0", "0.5"]),
            ("s6", &["2", "2", "2", "2", "end", "1", "1"]),
        ],
    );
}

#[test]
fn running_aggregates_follow_the_default_frame_and_rows_frames() {
    let e = engine();
    // Default frame with an order: to the last peer (s2 and s3 are peers).
    let rows = table(
        &e,
        "SELECT * WHERE { SERVICE <http://w3id.org/tripletrail/window> {
           { SELECT ?s ?line ?km WHERE { ?s ex:line ?line OPTIONAL { ?s ex:km ?km } } }
           [] win:partitionBy ?line ; win:orderBy ?s ;
              win:sum (?km ?sum) ; win:count (?km ?c) ; win:max (?km ?max) ;
              win:firstValue (?km ?first) ; win:lastValue (?km ?last) .
         } }",
        &["sum", "c", "max", "first", "last"],
    );
    expect(
        &rows,
        &[
            ("s1", &["1.5", "1", "1.5", "1.5", "1.5"]),
            ("s2", &["4.5", "2", "3.0", "1.5", "3.0"]),
            ("s3", &["7.5", "3", "3.0", "1.5", "3.0"]),
            ("s4", &["14.75", "4", "7.25", "1.5", "7.25"]),
            ("s5", &["2", "1", "2", "2", "2"]),
            ("s6", &["7", "2", "5", "2", "5"]),
            // Unbound ?km is skipped, as SQL skips NULL.
            ("s7", &["7", "2", "5", "2", ""]),
        ],
    );
    // A sliding frame: the row and the one before; and the whole partition.
    let rows = table(
        &e,
        "SELECT * WHERE { SERVICE <http://w3id.org/tripletrail/window> {
           { SELECT ?s ?line ?km WHERE { ?s ex:line ?line ; ex:km ?km } }
           [] win:partitionBy ?line ; win:orderBy ?km ; win:rows (-1 0) ;
              win:avg (?km ?avg) ; win:min (?km ?min) .
         } }",
        &["avg", "min"],
    );
    expect(
        &rows,
        &[
            ("s1", &["1.5", "1.5"]),
            ("s2", &["2.25", "1.5"]),
            ("s3", &["3.0", "3.0"]),
            ("s4", &["5.125", "3.0"]),
            ("s5", &["2.0", "2"]),
            ("s6", &["3.5", "2"]),
        ],
    );
}

#[test]
fn ntile_descending_order_and_one_partition() {
    let e = engine();
    let rows = table(
        &e,
        "SELECT * WHERE { SERVICE <http://w3id.org/tripletrail/window> {
           { SELECT ?s ?km WHERE { ?s ex:km ?km } }
           [] win:orderByDesc ?km ; win:ntile (4 ?q) ; win:rowNumber ?n ;
              win:rows (\"unbounded\" \"unbounded\") ; win:lastValue (?km ?smallest) .
         } }",
        &["q", "n", "smallest"],
    );
    // 7.25 5 3.0 3.0 2 1.5: six rows in four buckets of 2, 2, 1, 1.
    let q: Vec<(String, String)> = rows
        .iter()
        .map(|(s, v)| (v[1].clone(), format!("{s} {} {}", v[0], v[2])))
        .collect();
    let mut by_n = q.clone();
    by_n.sort_by_key(|(n, _)| n.parse::<u32>().unwrap());
    let buckets: Vec<&str> = by_n
        .iter()
        .map(|(_, r)| r.split(' ').nth(1).unwrap())
        .collect();
    assert_eq!(buckets, ["1", "1", "2", "2", "3", "4"]);
    assert!(rows.iter().all(|(_, v)| v[2] == "1.5"));
}

#[test]
fn mistakes_are_explained() {
    let e = engine();
    for (q, needle) in [
        ("[] win:rowNumber ?n .", "sub-select"),
        (
            "{ SELECT ?s WHERE { ?s ex:km ?km } } [] win:orderBy ?km ; win:rowNumber ?n .",
            "not a variable of the sub-select",
        ),
        (
            "{ SELECT ?s WHERE { ?s ex:km ?km } } [] win:unknown ?n .",
            "unknown function",
        ),
        (
            "{ SELECT ?s WHERE { ?s ex:km ?km } } [] win:rowNumber ?s .",
            "bound twice",
        ),
        (
            "{ SELECT ?s WHERE { ?s ex:km ?km } } [] win:ntile (0 ?q) .",
            "positive",
        ),
    ] {
        let err = e
            .query(&format!(
                "{PREFIXES}SELECT * WHERE {{ SERVICE <http://w3id.org/tripletrail/window> {{ {q} }} }}"
            ))
            .map_err(|e| e.to_string())
            .and_then(|r| match r {
                QueryResults::Solutions(s) => s
                    .collect::<Result<Vec<_>, _>>()
                    .map(|_| ())
                    .map_err(|e| e.to_string()),
                _ => Ok(()),
            })
            .unwrap_err();
        assert!(err.contains(needle), "{q}: {err}");
    }
}

#[test]
fn moving_sums_of_doubles_add_only_the_frame() {
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        r#"@prefix ex: <http://example.com/> .
        @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
        ex:a ex:k 1 ; ex:v "1e20"^^xsd:double .
        ex:b ex:k 2 ; ex:v "1"^^xsd:double .
        ex:c ex:k 3 ; ex:v "1"^^xsd:double .
        ex:d ex:k 4 ; ex:v "INF"^^xsd:double .
        ex:e ex:k 5 ; ex:v "2"^^xsd:double .
        ex:f ex:k 6 ; ex:v "3"^^xsd:double .
        ex:g ex:k 7 ; ex:v "NaN"^^xsd:double .
        ex:h ex:k 8 ; ex:v "4"^^xsd:double .
        ex:i ex:k 9 ; ex:v "5"^^xsd:double ."#
            .as_bytes(),
    )
    .unwrap();
    let rows = table(
        &e,
        "SELECT ?s ?own ?pair WHERE { SERVICE <http://w3id.org/tripletrail/window> {
           { SELECT ?s ?k ?v WHERE { ?s ex:k ?k ; ex:v ?v } }
           [] win:orderBy ?k ; win:rows (-1 0) ; win:sum (?v ?own) ; win:avg (?v ?pair) .
         } }",
        &["own", "pair"],
    );
    expect(
        &rows,
        &[
            // The frame is the row and the one before (1e20 + 1 is 1e20 in a double).
            ("a", &["100000000000000000000", "100000000000000000000"]),
            ("b", &["100000000000000000000", "50000000000000000000"]),
            ("c", &["2", "1"]),
            ("d", &["INF", "INF"]),
            ("e", &["INF", "INF"]),
            ("f", &["5", "2.5"]),
            ("g", &["NaN", "NaN"]),
            ("h", &["NaN", "NaN"]),
            ("i", &["9", "4.5"]),
        ],
    );
}

#[test]
fn a_decimal_average_keeps_24_fraction_digits_as_the_aggregate() {
    // win:avg divides as AVG and Jena do (#8): 13.25 / 3, not 18 truncated digits.
    let e = engine();
    let rows = table(
        &e,
        "SELECT * WHERE { SERVICE <http://w3id.org/tripletrail/window> {
           { SELECT ?s ?line ?km WHERE { ?s ex:line ?line ; ex:km ?km } }
           [] win:partitionBy ?line ; win:orderBy ?km ; win:rows (-2 0) ; win:avg (?km ?avg) .
         } }",
        &["avg"],
    );
    expect(
        &rows,
        &[
            ("s1", &["1.5"]),
            ("s2", &["2.25"]),
            ("s3", &["2.5"]),
            ("s4", &["4.416666666666666666666667"]),
            ("s5", &["2.0"]),
            ("s6", &["3.5"]),
        ],
    );
}
