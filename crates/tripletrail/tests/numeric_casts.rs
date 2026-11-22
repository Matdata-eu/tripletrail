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

/// Decimal division and `AVG` as Jena (`XSDFuncOp.decimalDivide`): the exact quotient when it
/// ends, else 24 fraction digits rounded half to even (#8).
#[test]
fn decimal_division_keeps_24_fraction_digits_as_jena() {
    let engine = Engine::new_tripletrail();
    let d = |s: &str| format!("\"{s}\"^^<http://www.w3.org/2001/XMLSchema#decimal>");
    for (expression, expected) in [
        ("1.0 / 3.0", d("0.333333333333333333333333")),
        ("2.0 / 3.0", d("0.666666666666666666666667")),
        ("-2 / 3", d("-0.666666666666666666666667")),
        ("10 / 3", d("3.333333333333333333333333")),
        ("1 / 4", d("0.25")),
        ("4 / 2", d("2.0")),
        ("0 / 7", d("0.0")),
        // Exact quotients keep every digit, also past 24.
        (
            "1.0 / 1099511627776",
            d("0.0000000000009094947017729282379150390625"),
        ),
        ("0.000000000000000001 / 3", d("0.000000000000000000333333")),
        ("1.0 / 0.0", "-".into()),
        // A divisor of any size (the scaled value above 3.4e37).
        (
            "1 / 99999999999999999999.000000000000000001",
            d("0.00000000000000000001"),
        ),
        // A long quotient is still a number: arithmetic reads it at 18 fraction digits.
        ("(1.0 / 3.0) * 3", d("0.999999999999999999")),
        ("(2.0 / 3.0) + 0", d("0.666666666666666667")),
    ] {
        let r = column(&engine, &format!("SELECT ({expression} AS ?r) {{}}"));
        assert_eq!(r, [expected.as_str()], "{expression}");
    }
    let r = column(&engine, "SELECT (DATATYPE(1 / 3) AS ?r) {}");
    assert_eq!(r, ["<http://www.w3.org/2001/XMLSchema#decimal>"]);
    let r = column(&engine, "SELECT (STR(2 / 3) AS ?r) {}");
    assert_eq!(r, ["\"0.666666666666666666666667\""]);

    // AVG divides the sum by the count the same way, from VALUES and from stored data.
    let r = column(&engine, "SELECT (AVG(?v) AS ?r) { VALUES ?v { 1 1 2 } }");
    assert_eq!(r, [d("1.333333333333333333333333")]);
    let r = column(&engine, "SELECT (AVG(?v) AS ?r) { VALUES ?v { 1 2 } }");
    assert_eq!(r, [d("1.5")]);
    engine
        .load(
            RdfFormat::Turtle,
            "<urn:a> <urn:n> 1.0, 1.5, 3.0 . <urn:b> <urn:n> 1, 2 .".as_bytes(),
        )
        .unwrap();
    let r = column(
        &engine,
        "SELECT (AVG(?n) AS ?r) { ?s <urn:n> ?n } GROUP BY ?s ORDER BY ?s",
    );
    assert_eq!(r, [d("1.833333333333333333333333"), d("1.5")]);

    // Data written with more than 18 fraction digits (by Jena, say) is a number too.
    engine
        .load(
            RdfFormat::Turtle,
            "<urn:c> <urn:x> \"0.333333333333333333333333\"^^<http://www.w3.org/2001/XMLSchema#decimal> ."
                .as_bytes(),
        )
        .unwrap();
    let r = column(&engine, "SELECT (?x * 3 AS ?r) { <urn:c> <urn:x> ?x }");
    assert_eq!(r, [d("0.999999999999999999")]);
    let r = column(
        &engine,
        "SELECT (STR(?x) AS ?r) { <urn:c> <urn:x> ?x FILTER(?x > 0.3 && ?x < 0.34) }",
    );
    assert_eq!(r, ["\"0.333333333333333333333333\""]);
}

/// Decimal products as Jena (`BigDecimal.multiply`): exact, also past the 18 fraction digits the
/// evaluator computes at, where it used to give an error (no value).
#[test]
fn decimal_products_are_exact_as_jena() {
    let engine = Engine::new_tripletrail();
    let d = |s: &str| format!("\"{s}\"^^<http://www.w3.org/2001/XMLSchema#decimal>");
    for (expression, expected) in [
        ("0.5 * 0.5", d("0.25")),
        ("0.1234567891 * 0.123456789", d("0.0152415787625361999")),
        ("-0.000000001 * 0.0000000001", d("-0.0000000000000000001")),
        (
            "0.000000000000000001 * 0.000000000000000001",
            d("0.000000000000000000000000000000000001"),
        ),
        (
            "123456789.123456789 * 3.0000000001",
            d("370370367.3827160459123456789"),
        ),
        (
            "12345678901234567890.5 * 0.0000000001",
            d("1234567890.12345678905"),
        ),
        ("2 * 0.25", d("0.5")),
        // A long product is still a number: arithmetic reads it at 18 fraction digits.
        ("(0.1234567891 * 0.123456789) + 0", d("0.0152415787625362")),
    ] {
        let r = column(&engine, &format!("SELECT ({expression} AS ?r) {{}}"));
        assert_eq!(r, [expected.as_str()], "{expression}");
    }
    engine
        .load(
            RdfFormat::Turtle,
            "<urn:p> <urn:ic> 0.1234567891 ; <urn:len> 0.123456789 .".as_bytes(),
        )
        .unwrap();
    let r = column(
        &engine,
        "SELECT (?ic * ?len AS ?r) { <urn:p> <urn:ic> ?ic ; <urn:len> ?len }",
    );
    assert_eq!(r, [d("0.0152415787625361999")]);
}
