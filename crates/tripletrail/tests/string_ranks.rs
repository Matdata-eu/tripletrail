//! Prefix filters (`STRSTARTS`, `REGEX "^..."`), `MIN`/`MAX` and `ORDER BY ... LIMIT` of IRIs,
//! strings and language strings on the string ranks (issue #171) give the same solutions as the evaluator: on the tripletrail store
//! they run on ids, on Oxigraph's store through the evaluator. Also after an update adds
//! strings the ranks don't cover yet.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use oxigraph::store::Store;
use tripletrail::Engine;

fn rows(engine: &Engine, query: &str) -> Vec<String> {
    let mut out = ordered_rows(engine, query);
    out.sort();
    out
}

/// The solutions in the order given (each row's cells sorted).
fn ordered_rows(engine: &Engine, query: &str) -> Vec<String> {
    let QueryResults::Solutions(solutions) = engine.query(query).unwrap() else {
        panic!("not a SELECT")
    };
    let out: Vec<String> = solutions
        .map(|s| {
            let s = s.unwrap();
            let mut cells: Vec<String> = s.iter().map(|(v, t)| format!("{v}={t}")).collect();
            cells.sort();
            cells.join(" ")
        })
        .collect();
    out
}

const WORDS: [&str; 16] = [
    "Com",
    "Comet",
    "Co",
    "C",
    "Cat",
    "cat",
    "Ça va",
    "Zürich",
    "",
    "Oslo",
    "Coal mine",
    "Côte",
    "a^b",
    "C.D",
    "Commune",
    "Drammen",
];

fn data() -> String {
    let mut ttl = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..1500_usize {
        let s = format!("ex:s{i}");
        let w = WORDS[i % WORDS.len()];
        ttl.push_str(&format!("{s} a ex:C{} .\n", i % 3));
        ttl.push_str(&format!("{s} ex:label \"{w} {}\" .\n", i % 37));
        ttl.push_str(&format!("{s} ex:name \"{w} {}\" .\n", i % 37));
        if i % 4 == 0 {
            ttl.push_str(&format!("{s} ex:label \"{w}\"@nb .\n"));
        }
        // Language strings only, with values shared between languages.
        let lang = ["en", "nb", "de", "en-GB"][i % 4];
        ttl.push_str(&format!("{s} ex:title \"{w} {}\"@{lang} .\n", i % 5));
        // IRIs, numbers and strings together.
        let mixed = match i % 5 {
            0 => format!("ex:{}", w.replace([' ', '^', '.', 'ç', 'Ç', 'ü', 'ô'], "_")),
            1 => format!("{}", i % 11),
            2 => "true".to_owned(),
            _ => format!("\"{w}\""),
        };
        ttl.push_str(&format!("{s} ex:mixed {mixed} .\n"));
        ttl.push_str(&format!("{s} ex:link ex:t{} .\n", (i * 7) % 211));
        ttl.push_str(&format!("{s} ex:num {} .\n", (i * 13) % 101));
        if i % 9 == 0 {
            ttl.push_str(&format!("{s} ex:blank [] .\n"));
        }
    }
    ttl
}

fn engines() -> (Engine, Engine) {
    let ours = Engine::new_tripletrail();
    ours.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    let reference = Engine::from_store(Store::new().unwrap());
    reference
        .load(RdfFormat::Turtle, data().as_bytes())
        .unwrap();
    (ours, reference)
}

const P: &str = "PREFIX ex: <http://example.org/> ";

fn queries() -> Vec<String> {
    let mut q = Vec::new();
    for prefix in ["C", "Co", "Com", "c", "Ç", "Zü", "", "Q", "a", "Côte"] {
        for p in ["ex:label", "ex:name", "ex:mixed", "ex:title"] {
            for test in [
                format!("REGEX(?o, \"^{prefix}\")"),
                format!("REGEX(?o, \"^{prefix}\", \"\")"),
                format!("STRSTARTS(?o, \"{prefix}\")"),
                format!("!STRSTARTS(?o, \"{prefix}\")"),
            ] {
                q.push(format!(
                    "{P}SELECT (COUNT(*) AS ?n) WHERE {{ ?s {p} ?o FILTER({test}) }}"
                ));
                q.push(format!(
                    "{P}SELECT ?s ?o WHERE {{ ?s {p} ?o FILTER({test}) }}"
                ));
            }
        }
    }
    // Patterns that are not prefixes stay with the evaluator.
    for test in [
        "REGEX(?o, \"^C.\")",
        "REGEX(?o, \"^a\\\\^\")",
        "REGEX(?o, \"^c\", \"i\")",
        "REGEX(?o, \"Com\")",
        "STRSTARTS(?o, \"C\"@en)",
        "STRSTARTS(STR(?o), \"http://example.org/C\")",
    ] {
        q.push(format!(
            "{P}SELECT (COUNT(*) AS ?n) WHERE {{ ?s ex:mixed ?o FILTER({test}) }}"
        ));
    }
    // An unbound value is an error.
    q.push(format!(
        "{P}SELECT (COUNT(*) AS ?n) WHERE {{ ?s a ?c OPTIONAL {{ ?s ex:none ?o }} FILTER(!STRSTARTS(?o, \"C\")) }}"
    ));
    for agg in ["MIN", "MAX"] {
        for p in ["ex:label", "ex:name", "ex:mixed", "ex:link", "ex:title"] {
            q.push(format!("{P}SELECT ({agg}(?o) AS ?m) WHERE {{ ?s {p} ?o }}"));
            q.push(format!(
                "{P}SELECT ?c ({agg}(?o) AS ?m) (COUNT(*) AS ?n) WHERE {{ ?s a ?c ; {p} ?o }} GROUP BY ?c"
            ));
            q.push(format!(
                "{P}SELECT ({agg}(?o) AS ?m) WHERE {{ ?s {p} ?o FILTER(isLiteral(?o)) }}"
            ));
            q.push(format!(
                "{P}SELECT ({agg}(?o) AS ?m) WHERE {{ ?s {p} ?o FILTER(isIRI(?o)) }}"
            ));
        }
    }
    q
}

/// `ORDER BY ... LIMIT` queries whose solutions are in one order only (ties broken, or only
/// the tied values projected).
fn ordered_queries() -> Vec<String> {
    let mut q = Vec::new();
    // (Blank nodes are left out: their labels differ between the stores.)
    for p in [
        "ex:label", "ex:name", "ex:mixed", "ex:link", "ex:num", "ex:title",
    ] {
        for order in ["?o", "DESC(?o)"] {
            for slice in [
                "LIMIT 10",
                "LIMIT 1",
                "OFFSET 7 LIMIT 12",
                "LIMIT 0",
                "LIMIT 5000",
            ] {
                q.push(format!(
                    "{P}SELECT ?o WHERE {{ ?s {p} ?o }} ORDER BY {order} {slice}"
                ));
                q.push(format!(
                    "{P}SELECT ?s ?o WHERE {{ ?s {p} ?o }} ORDER BY {order} ?s {slice}"
                ));
                q.push(format!(
                    "{P}SELECT DISTINCT ?o WHERE {{ ?s {p} ?o }} ORDER BY {order} {slice}"
                ));
                q.push(format!(
                    "{P}SELECT REDUCED ?o WHERE {{ ?s {p} ?o }} ORDER BY {order} {slice}"
                ));
                q.push(format!(
                    "{P}SELECT ?o ?c WHERE {{ ?s {p} ?o ; a ?c }} ORDER BY ?c {order} DESC(?s) {slice}"
                ));
                q.push(format!(
                    "{P}SELECT ?o WHERE {{ ?s {p} ?o FILTER(STRSTARTS(?o, \"Co\")) }} ORDER BY {order} {slice}"
                ));
            }
        }
    }
    // Unbound values first.
    q.push(format!(
        "{P}SELECT ?s ?o WHERE {{ ?s a ex:C0 OPTIONAL {{ ?s ex:none ?o }} }} ORDER BY ?o ?s LIMIT 5"
    ));
    q.push(format!(
        "{P}SELECT ?s ?o WHERE {{ ?s a ex:C0 OPTIONAL {{ ?s ex:blank ?b . ?s ex:name ?o }} }} ORDER BY DESC(?o) ?s LIMIT 400"
    ));
    // Not plain variables: stays with the evaluator.
    q.push(format!(
        "{P}SELECT ?o WHERE {{ ?s ex:name ?o }} ORDER BY STRLEN(?o) ?o LIMIT 9"
    ));
    q.push(format!(
        "{P}SELECT * WHERE {{ ?s ex:name ?o }} ORDER BY ?o ?s LIMIT 9"
    ));
    q.push(format!(
        "{P}SELECT DISTINCT * WHERE {{ ?s ex:name ?o }} ORDER BY ?o ?s LIMIT 9"
    ));
    q
}

fn compare(ours: &Engine, reference: &Engine) {
    for q in queries() {
        assert_eq!(rows(ours, &q), rows(reference, &q), "{q}");
    }
    for q in ordered_queries() {
        assert_eq!(ordered_rows(ours, &q), ordered_rows(reference, &q), "{q}");
    }
}

#[test]
fn prefix_filters_and_extremes_match_the_evaluator() {
    let (ours, reference) = engines();
    compare(&ours, &reference);
}

#[test]
fn strings_added_after_the_ranks_were_built() {
    let (ours, reference) = engines();
    // Build the ranks.
    rows(
        &ours,
        &format!("{P}SELECT (MIN(?o) AS ?m) WHERE {{ ?s ex:label ?o }}"),
    );
    let update = format!(
        "{P}INSERT DATA {{ ex:new1 ex:label \"A first\" ; ex:name \"A first\" , \"Cq\" ; ex:mixed \"Co-op\" , ex:Aaa . ex:new2 ex:label \"Zz last\" ; ex:mixed \"Cz\" , ex:zzz ; ex:link ex:aaa ; ex:title \"A first\"@en , \"Cq\"@nb , \"Com 1\"@aa }}"
    );
    ours.update(&update).unwrap();
    reference.update(&update).unwrap();
    compare(&ours, &reference);
}
