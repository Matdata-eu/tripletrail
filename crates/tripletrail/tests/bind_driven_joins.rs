//! A variable made by `BIND` drives lookups on the other side of a join or `OPTIONAL`, though
//! it is no hash key (it is unbound where its expression fails), and the conditions of an
//! `OPTIONAL` that only read its own side are applied inside it (#484, #485, #486, #494).
//! Each query's answer is checked against a count made here, including rows whose `BIND`
//! fails (an invalid IRI), which are compatible with every row of the other side.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use std::fmt::Write;
use tripletrail::Engine;

const PREFIXES: &str = "PREFIX src: <http://example.org/source/>
PREFIX d: <http://example.org/data/>
PREFIX prov: <http://www.w3.org/ns/prov#>
";

fn count(e: &Engine, query: &str, var: &str) -> usize {
    let QueryResults::Solutions(mut s) = e.query(&format!("{PREFIXES}{query}")).unwrap() else {
        panic!("not a SELECT")
    };
    let row = s.next().unwrap().unwrap();
    let n = row.get(var).unwrap().to_string();
    n.split('"').nth(1).unwrap().parse().unwrap()
}

fn plan(e: &Engine, query: &str) -> String {
    e.explain(&format!("{PREFIXES}{query}")).unwrap()
}

fn load(data: &str) -> Engine {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::NTriples, data.as_bytes()).unwrap();
    e
}

/// A measure shared by three quarters of the rows, unique for the others.
fn measure(i: usize) -> String {
    if i.is_multiple_of(4) {
        i.to_string()
    } else {
        "0".into()
    }
}

#[test]
fn a_bind_variable_drives_the_lookup_not_a_weak_key() {
    // #486: turns name a sequence by id text and a measure; links have a sequence IRI and a
    // measure. The measure alone is a weak key.
    let (turns, links, sequences) = (600, 4000, 1000);
    let src = "http://example.org/source/";
    let mut nt = String::new();
    for i in 0..turns {
        let oid = if i == 7 {
            "not an iri".to_owned()
        } else {
            format!("s{}", i % sequences)
        };
        writeln!(
            nt,
            "<{src}t{i}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <{src}Turn> ."
        )
        .unwrap();
        writeln!(nt, "<{src}t{i}> <{src}to_oid> \"{oid}\" .").unwrap();
        writeln!(nt, "<{src}t{i}> <{src}to_measure> \"{}\" .", measure(i)).unwrap();
    }
    for j in 0..links {
        writeln!(
            nt,
            "<{src}l{j}> <{src}link_sequence_oid> <{src}s{}> .",
            j % sequences
        )
        .unwrap();
        writeln!(nt, "<{src}l{j}> <{src}measure_from> \"{}\" .", measure(j)).unwrap();
    }
    let e = load(&nt);
    let query = "SELECT (COUNT(*) AS ?n) WHERE {
      ?this a src:Turn ; src:to_oid ?to_oid ; src:to_measure ?to_measure .
      BIND(IRI(CONCAT(STR(src:), ?to_oid)) AS ?to_sequence)
      ?link_b src:link_sequence_oid ?to_sequence ; src:measure_from ?to_measure .
    }";
    let mut want = 0;
    for i in 0..turns {
        for j in 0..links {
            let same_sequence = i == 7 || i % sequences == j % sequences;
            want += usize::from(same_sequence && measure(i) == measure(j));
        }
    }
    assert_eq!(count(&e, query, "n"), want);
    let plan = plan(&e, query);
    assert!(!plan.contains("keys = ?to_measure"), "{plan}");
}

#[test]
fn an_optional_on_a_bind_variable_is_a_lookup() {
    // #484: 30 turnouts name a net element by id text; 2,000 net elements, each derived from a
    // link with a date.
    let src = "http://example.org/source/";
    let d = "http://example.org/data/";
    let prov = "http://www.w3.org/ns/prov#";
    let mut nt = String::new();
    for i in 0..30 {
        // Turnout 3's id makes no valid IRI; turnout 5 names a net element that does not exist.
        let id = match i {
            3 => "not an iri".to_owned(),
            5 => "missing".to_owned(),
            _ => format!("e{}", i * 7),
        };
        writeln!(
            nt,
            "<{src}m{i}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <{src}MissingTurnout> ."
        )
        .unwrap();
        writeln!(nt, "<{src}m{i}> <{src}common_branch_id> \"{id}\" .").unwrap();
    }
    let elements = 2000;
    for k in 0..elements {
        writeln!(
            nt,
            "<{d}_netelement_e{k}> <{prov}wasDerivedFrom> <{src}link{k}> ."
        )
        .unwrap();
        writeln!(nt, "<{src}link{k}> <{src}valid_from> \"2020-{k}\" .").unwrap();
    }
    let e = load(&nt);
    let query = "SELECT (COUNT(*) AS ?n) (COUNT(?common_from) AS ?c) WHERE {
      ?this a src:MissingTurnout ; src:common_branch_id ?common_id .
      BIND(IRI(CONCAT(STR(d:), \"_netelement_\", ?common_id)) AS ?common)
      OPTIONAL { ?common prov:wasDerivedFrom/src:valid_from ?common_from }
    }";
    // 28 turnouts with one match each, turnout 5 with none (kept once), turnout 3 with every
    // net element (its ?common is unbound, compatible with all).
    assert_eq!(count(&e, query, "n"), 28 + 1 + elements);
    assert_eq!(count(&e, query, "c"), 28 + elements);
    let plan = plan(&e, query);
    assert!(
        !plan.contains("LeftJoin(HashBuildRightProbeLeft, keys = ,"),
        "{plan}"
    );
}

#[test]
fn optional_conditions_on_its_own_side_apply_inside_it() {
    // #485: the condition linking the two parts of the OPTIONAL reads only its side, the
    // other one reads the left side too.
    let src = "http://example.org/source/";
    let mut nt = String::new();
    for i in 0..200 {
        writeln!(
            nt,
            "<{src}x{i}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <{src}Section> ."
        )
        .unwrap();
        writeln!(nt, "<{src}x{i}> <{src}object> \"o{i}\" .").unwrap();
    }
    for k in 0..20 {
        writeln!(nt, "<{src}k{k}> <{src}parallel> \"o{}\" .", k * 3).unwrap();
    }
    let e = load(&nt);
    let query = "SELECT (COUNT(*) AS ?n) (COUNT(?kept) AS ?k) WHERE {
      ?this a ?class . VALUES ?class { src:Section }
      OPTIONAL {
        FILTER(?class = src:Section)
        ?this src:object ?object .
        ?kept src:parallel ?parallel .
        FILTER(?parallel = ?object)
      }
    }";
    assert_eq!(count(&e, query, "n"), 200);
    assert_eq!(count(&e, query, "k"), 20);
    let plan = plan(&e, query);
    // The plan is JSON: the condition is a filter of its own, not in a left join's expression.
    let names: Vec<&str> = plan.split("\"name\":\"").skip(1).collect();
    assert!(
        names
            .iter()
            .any(|n| n.starts_with("Filter((?parallel = ?object))")),
        "{plan}"
    );
    let left_joins = names
        .iter()
        .map(|n| &n[..n.find("\",\"number").unwrap_or(n.len())])
        .filter(|n| n.contains("LeftJoin"));
    for name in left_joins {
        assert!(!name.contains("?parallel"), "{name}");
    }
}

#[test]
fn a_computed_equality_is_the_join_key() {
    // #531: the two parts of the OPTIONAL are linked by a value computed from one side
    // (`?parallel = STRAFTER(STR(?object), ...)`). They were joined on no key, every row with
    // every row, before the filter.
    let src = "http://example.org/source/";
    let mut nt = String::new();
    for i in 0..2000 {
        writeln!(
            nt,
            "<{src}x{i}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <{src}Section> ."
        )
        .unwrap();
        writeln!(nt, "<{src}x{i}> <{src}object> <{src}o{i}> .").unwrap();
    }
    // a literal object gives the same STR, a language-tagged or typed parallel is not equal
    writeln!(nt, "<{src}x0> <{src}object> \"{src}o0\" .").unwrap();
    for k in 0..20 {
        writeln!(nt, "<{src}k{k}> <{src}parallel> \"o{}\" .", k * 3).unwrap();
    }
    writeln!(nt, "<{src}k20> <{src}parallel> \"o4\"@en .").unwrap();
    writeln!(
        nt,
        "<{src}k21> <{src}parallel> \"5\"^^<http://www.w3.org/2001/XMLSchema#integer> ."
    )
    .unwrap();
    writeln!(nt, "<{src}k22> <{src}parallel> <{src}o6> .").unwrap();
    let e = load(&nt);
    for test in [
        "?parallel = STRAFTER(STR(?object), STR(src:))",
        "sameTerm(STRAFTER(STR(?object), STR(src:)), ?parallel)",
    ] {
        let query = format!(
            "SELECT (COUNT(*) AS ?n) (COUNT(?kept) AS ?k) WHERE {{
              ?this a ?class . VALUES ?class {{ src:Section }}
              OPTIONAL {{
                FILTER(?class = src:Section)
                ?this src:object ?object .
                ?kept src:parallel ?parallel .
                FILTER({test})
              }}
            }}"
        );
        // x0 matches k0 twice, through the IRI and the literal object: 2,001 rows
        assert_eq!(count(&e, &query, "n"), 2001, "{test}");
        assert_eq!(count(&e, &query, "k"), 21, "{test}");
        let plan = plan(&e, &query);
        // ?parallel is computed from ?object, and joined on (or looked up), no filter is left
        assert!(plan.contains("-> ?parallel)"), "{plan}");
        assert!(!plan.contains("Filter((?parallel ="), "{plan}");
        assert!(!plan.contains("Filter(sameTerm(STRAFTER"), "{plan}");
    }
    // a value that may not be a simple literal (here a language-tagged one) stays a filter
    let query = "SELECT (COUNT(*) AS ?n) WHERE {
      ?this src:object ?object . ?kept src:parallel ?parallel .
      FILTER(?parallel = STRLANG(STRAFTER(STR(?object), STR(src:)), \"en\"))
    }";
    assert_eq!(count(&e, query, "n"), 1);
    assert!(!plan(&e, query).contains("-> ?parallel)"));
}

#[test]
fn a_computed_equality_keys_a_hash_join() {
    // #531, as in the rail KG rule: both parts of the OPTIONAL have several patterns.
    let src = "http://example.org/source/";
    let mut nt = String::new();
    for i in 0..3000 {
        writeln!(
            nt,
            "<{src}x{i}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <{src}Section> ."
        )
        .unwrap();
        writeln!(nt, "<{src}x{i}> <{src}object> <{src}o{i}> .").unwrap();
        writeln!(nt, "<{src}x{i}> <{src}from> \"{i}\" .").unwrap();
    }
    for k in 0..55 {
        writeln!(
            nt,
            "<{src}k{k}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <{src}Section> ."
        )
        .unwrap();
        writeln!(nt, "<{src}k{k}> <{src}parallel> \"o{}\" .", k * 3).unwrap();
        writeln!(nt, "<{src}k{k}> <{src}type> \"2\" .").unwrap();
        writeln!(nt, "<{src}k{k}> <{src}from> \"{k}\" .").unwrap();
    }
    let e = load(&nt);
    let query = "SELECT (COUNT(*) AS ?n) (COUNT(?kept) AS ?k) WHERE {
      ?this a ?class . VALUES ?class { src:Section }
      OPTIONAL {
        FILTER(?class = src:Section)
        ?this src:object ?object ; src:from ?from .
        ?kept src:parallel ?parallel ; a src:Section ; src:type \"2\" ; src:from ?kept_from .
        FILTER(?parallel = STRAFTER(STR(?object), STR(src:)))
      }
    }";
    assert_eq!(count(&e, query, "n"), 3055);
    assert_eq!(count(&e, query, "k"), 55);
    let plan = plan(&e, query);
    assert!(plan.contains("-> ?parallel)"), "{plan}");
    assert!(!plan.contains("Filter((?parallel ="), "{plan}");
}
