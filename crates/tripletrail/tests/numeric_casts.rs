//! Casts of doubles to `xsd:integer` and `xsd:decimal` are exact, as in Jena: the rail KG uplift
//! mints IRIs from `xsd:integer(xsd:double(?s))`, and "2573.0" gave 2572 (the uplift_dimdb
//! divergence of 2026-11-11).

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

fn column(engine: &Engine, query: &str) -> Vec<String> {
    let QueryResults::Solutions(s) = engine.query(query).unwrap() else {
        panic!()
    };
    s.map(|row| row.unwrap().get("r").map_or("-".into(), |t| t.to_string()))
        .collect()
}

#[test]
fn whole_doubles_cast_to_the_same_integer() {
    let engine = Engine::new_tripletrail();
    engine
        .load(RdfFormat::Turtle, "<urn:s> <urn:v> \"6297.0\" .".as_bytes())
        .unwrap();
    let integers = column(
        &engine,
        "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
         SELECT (STR(xsd:integer(xsd:double(?s))) AS ?r) {
           VALUES ?s { \"2573.0\" \"6297.0\" \"2573\" \"1.0\" \"28.0\" \"-2573.9\" \"1e300\" \"NaN\" }
         }",
    );
    assert_eq!(
        integers,
        [
            "\"2573\"",
            "\"6297\"",
            "\"2573\"",
            "\"1\"",
            "\"28\"",
            "\"-2573\"",
            "-",
            "-"
        ]
    );
    // The same from stored data, and through xsd:float.
    let stored = column(
        &engine,
        "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
         SELECT (STR(xsd:integer(xsd:double(?s))) AS ?r) { <urn:s> <urn:v> ?s }",
    );
    assert_eq!(stored, ["\"6297\""]);
    let float = column(
        &engine,
        "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
         SELECT (STR(xsd:integer(xsd:float(\"2573.0\"))) AS ?r) {}",
    );
    assert_eq!(float, ["\"2573\""]);
}

#[test]
fn doubles_cast_to_their_shortest_decimal() {
    let engine = Engine::new_tripletrail();
    let decimals = column(
        &engine,
        "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
         SELECT (STR(xsd:decimal(xsd:double(?s))) AS ?r) {
           VALUES ?s { \"2573.0\" \"0.1\" \"526216.596\" \"-6297\" }
         }",
    );
    assert_eq!(
        decimals,
        ["\"2573.0\"", "\"0.1\"", "\"526216.596\"", "\"-6297.0\""]
    );
}

/// Computed decimals are written as Jena writes them: a whole number keeps ".0" (the rail KG
/// uplift mints net element IRIs from them, and the lifted ids end in "_0.0").
#[test]
fn computed_decimals_have_jena_lexical_forms() {
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::Turtle,
            "<urn:s> <urn:v> 0.0, 2.50 . <urn:t> <urn:v> 3 .".as_bytes(),
        )
        .unwrap();
    let computed = column(
        &engine,
        "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
         SELECT (STR(?e) AS ?r) {
           VALUES ?e { 0.0 }
         }",
    );
    assert_eq!(computed, ["\"0.0\""]);
    let sums = column(
        &engine,
        "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
         SELECT ?r { VALUES (?a ?b) { (1.5 1.5) (0.0 0.0) (2.25 1) }
                     BIND(CONCAT(\"_\", STR(?a + ?b)) AS ?r) }",
    );
    assert_eq!(sums, ["\"_3.0\"", "\"_0.0\"", "\"_3.25\""]);
    let rounded = column(
        &engine,
        "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
         SELECT (STR(ROUND(xsd:decimal(xsd:double(\"0.549603271817592\")) * 1000) / 1000) AS ?r) {}",
    );
    assert_eq!(rounded, ["\"0.55\""]);
    let sum = column(
        &engine,
        "SELECT (STR(SUM(?v)) AS ?r) { ?s <urn:v> ?v FILTER(?s = <urn:s>) }",
    );
    assert_eq!(sum, ["\"2.5\""]);
    let whole_sum = column(
        &engine,
        "SELECT (STR(SUM(?v)) AS ?r) { VALUES ?v { 1.5 2.5 } }",
    );
    assert_eq!(whole_sum, ["\"4.0\""]);
    // Stored decimals keep the form they were written in.
    let stored = column(
        &engine,
        "SELECT (STR(?v) AS ?r) { <urn:s> <urn:v> ?v } ORDER BY ?v",
    );
    assert_eq!(stored, ["\"0.0\"", "\"2.50\""]);
}

#[test]
fn decimal_casts_keep_a_string_and_write_a_number_as_jena() {
    let engine = Engine::new_tripletrail();
    let r = column(
        &engine,
        "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
         SELECT (CONCAT(STR(xsd:decimal(\"3\")), \"|\", STR(xsd:decimal(3)), \"|\", STR(xsd:decimal(\"3.0\")), \"|\", STR(1.0 * 2), \"|\", STR(AVG(?x))) AS ?r) { VALUES ?x { 1 3 } }",
    );
    assert_eq!(r, ["\"3|3|3.0|2.0|2.0\""]);
}

/// `xsd:double` keeps the text of a double too, as Jena (#517): `DISTINCT` inside `COUNT` and
/// `GROUP BY` then see the same three terms, "3.0", "3" and "3.0e0".
#[test]
fn double_casts_keep_the_text_of_a_double() {
    let engine = Engine::new_tripletrail();
    let r = column(
        &engine,
        "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
         SELECT (STR(xsd:double(3.0e0)) AS ?r) {}",
    );
    assert_eq!(r, ["\"3.0e0\""]);
    let distinct = column(
        &engine,
        "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
         SELECT (COUNT(DISTINCT xsd:double(?y)) AS ?r) { VALUES ?y { 3.0 \"3\"^^xsd:decimal 3 3.0e0 } }",
    );
    let groups = column(
        &engine,
        "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
         SELECT (COUNT(*) AS ?r) { { SELECT ?d { VALUES ?y { 3.0 \"3\"^^xsd:decimal 3 3.0e0 } BIND(xsd:double(?y) AS ?d) } GROUP BY ?d } }",
    );
    let integer = "\"3\"^^<http://www.w3.org/2001/XMLSchema#integer>";
    assert_eq!(distinct, [integer]);
    assert_eq!(groups, [integer]);
    // "3" cast from an integer or a string is the same term as a stored "3"^^xsd:double.
    for values in ["3 \"3\"^^xsd:double", "\"3\" \"3\"^^xsd:double"] {
        let one = "\"1\"^^<http://www.w3.org/2001/XMLSchema#integer>";
        let distinct = column(
            &engine,
            &format!(
                "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
                 SELECT (COUNT(DISTINCT xsd:double(?y)) AS ?r) {{ VALUES ?y {{ {values} }} }}"
            ),
        );
        assert_eq!(distinct, [one], "{values}");
    }
}

/// `MIN` and `MAX` over values of mixed kinds see them in Jena's order (#517): a plain string
/// below the numbers, and among equal numbers the term written last in text order, so `MAX`
/// of 3.0, "3"^^xsd:decimal, 3 and 3.0e0 is 3.0e0, as Fuseki answers.
#[test]
fn min_and_max_of_mixed_kinds_follow_jena() {
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::Turtle,
            "@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
             <urn:s> <urn:v> 3.0 ; <urn:w> \"3\"^^xsd:decimal ; <urn:x> 2.50 ; <urn:i> 3 ;
                     <urn:d> 3.0e0 ; <urn:str> \"3\" ."
                .as_bytes(),
        )
        .unwrap();
    let double = "\"3.0e0\"^^<http://www.w3.org/2001/XMLSchema#double>";
    let least = "\"2.50\"^^<http://www.w3.org/2001/XMLSchema#decimal>";
    for (aggregate, numbers, all) in [("MAX", double, double), ("MIN", least, "\"3\"")] {
        for pattern in ["?s ?p ?v", "?s ?p ?v FILTER(isNumeric(?v))"] {
            let expected = if pattern.contains("isNumeric") {
                numbers
            } else {
                all
            };
            let q = format!("SELECT ({aggregate}(?v) AS ?r) {{ {pattern} }}");
            assert_eq!(column(&engine, &q), [expected], "{q}");
            let grouped = format!("{q} GROUP BY ?s");
            assert_eq!(column(&engine, &grouped), [expected], "{grouped}");
        }
    }
    // Other kinds: an IRI is below every literal, a boolean above the numbers, a date above both.
    let r = column(
        &engine,
        "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
         SELECT (MAX(?v) AS ?r) { VALUES ?v { <urn:x> \"a\" 7 true \"2026-11-17\"^^xsd:date } }",
    );
    assert_eq!(
        r,
        ["\"2026-11-17\"^^<http://www.w3.org/2001/XMLSchema#date>"]
    );
    let r = column(
        &engine,
        "SELECT (MIN(?v) AS ?r) { VALUES ?v { \"b\" \"a\"@en 7 true <urn:x> } }",
    );
    assert_eq!(r, ["<urn:x>"]);
    let r = column(
        &engine,
        "SELECT (MAX(?v) AS ?r) { VALUES ?v { \"b\" \"a\"@en 7 true } }",
    );
    assert_eq!(r, ["\"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>"]);
    // Language strings by tag first, then text; also when the store answers on ids.
    engine
        .load(
            RdfFormat::Turtle,
            "<urn:t> <urn:label> \"b\"@en, \"a\"@fr . <urn:u> <urn:n> 3, \"03\"^^<http://www.w3.org/2001/XMLSchema#integer> ."
                .as_bytes(),
        )
        .unwrap();
    for (q, expected) in [
        (
            "SELECT (MAX(?l) AS ?r) { ?s <urn:label> ?l } GROUP BY ?s",
            "\"a\"@fr",
        ),
        (
            "SELECT (MIN(?l) AS ?r) { ?s <urn:label> ?l } GROUP BY ?s",
            "\"b\"@en",
        ),
        ("SELECT (MAX(?l) AS ?r) { ?s <urn:label> ?l }", "\"a\"@fr"),
        (
            "SELECT (MIN(?n) AS ?r) { ?s <urn:n> ?n } GROUP BY ?s",
            "\"03\"^^<http://www.w3.org/2001/XMLSchema#integer>",
        ),
        (
            "SELECT (MAX(?n) AS ?r) { ?s <urn:n> ?n }",
            "\"3\"^^<http://www.w3.org/2001/XMLSchema#integer>",
        ),
    ] {
        assert_eq!(column(&engine, q), [expected], "{q}");
    }
}

#[test]
fn decimal_division_keeps_24_digits_as_jena() {
    // Jena's XSDFuncOp.decimalDivide: exact when the quotient ends, otherwise 24 fraction
    // digits rounded half to even. Integers divide as decimals.
    let engine = Engine::new_tripletrail();
    let quotients = column(
        &engine,
        "SELECT (STR(?a / ?b) AS ?r) {
           VALUES (?a ?b) {
             (1.0 3.0) (2 3) (-2 3) (2 -3) (10 4) (6 3) (1 1024) (1 1099511627776)
             (0.000000000000000001 3) (1 0) (0 5)
           }
         }",
    );
    assert_eq!(
        quotients,
        [
            "\"0.333333333333333333333333\"",
            "\"0.666666666666666666666667\"",
            "\"-0.666666666666666666666667\"",
            "\"-0.666666666666666666666667\"",
            "\"2.5\"",
            "\"2.0\"",
            "\"0.0009765625\"",
            "\"0.0000000000009094947017729282379150390625\"",
            "\"0.000000000000000000333333\"",
            "-",
            "\"0.0\""
        ]
    );
    // The quotient is a decimal that compares, sorts and computes with as a number (its value
    // has 18 fraction digits), and AVG divides the same way.
    let mut uses = column(
        &engine,
        "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
         SELECT ?r {
           { BIND(STR(DATATYPE(1 / 3)) AS ?r) }
           UNION { BIND(STR(1 / 3 < 0.34 && 1 / 3 > 0.33) AS ?r) }
           UNION { BIND(STR(1 / 3 * 3) AS ?r) }
           UNION { BIND(STR(xsd:double(2 / 3)) AS ?r) }
           UNION { SELECT (STR(AVG(?x)) AS ?r) { VALUES ?x { 1 2 2 } } }
           UNION { SELECT (STR(AVG(?x)) AS ?r) { VALUES ?x { 1.5 2.5 } } }
         }",
    );
    uses.sort();
    assert_eq!(
        uses,
        [
            // A double cast keeps the text of its argument when that is a valid double.
            "\"0.666666666666666666666667\"",
            "\"0.999999999999999999\"",
            "\"1.666666666666666666666667\"",
            "\"2.0\"",
            "\"http://www.w3.org/2001/XMLSchema#decimal\"",
            "\"true\""
        ]
    );
}

#[test]
fn stored_decimals_with_more_than_18_digits_are_numbers() {
    // A stored decimal written with 24 fraction digits (as Jena writes a quotient) takes part in
    // arithmetic and comparisons, with its value rounded half to even at 18 digits.
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::Turtle,
            "<urn:a> <urn:v> 0.333333333333333333333333 .
             <urn:b> <urn:v> 0.6666666666666666665 .
             <urn:c> <urn:v> 0.6666666666666666675 ."
                .as_bytes(),
        )
        .unwrap();
    let values = column(
        &engine,
        "SELECT (STR(?v + 0) AS ?r) { ?s <urn:v> ?v } ORDER BY ?s",
    );
    assert_eq!(
        values,
        [
            "\"0.333333333333333333\"",
            "\"0.666666666666666666\"",
            "\"0.666666666666666668\""
        ]
    );
}
