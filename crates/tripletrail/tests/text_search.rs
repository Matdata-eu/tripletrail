//! Full-text search with `tt:search` (#188).

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;

const PREFIXES: &str = r#"
PREFIX ex: <http://example.com/>
PREFIX tt: <http://w3id.org/tripletrail/fn#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
PREFIX skos: <http://www.w3.org/2004/02/skos/core#>
"#;

const DATA: &str = r#"
@prefix ex: <http://example.com/> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix skos: <http://www.w3.org/2004/02/skos/core#> .
ex:bxl a ex:Station ; rdfs:label "Brussel-Zuid" ; skos:altLabel "Bruxelles-Midi" .
ex:bxn a ex:Station ; rdfs:label "Brussel-Noord" .
ex:gent a ex:Station ; rdfs:label "Gent-Sint-Pieters" ; rdfs:comment "Station in Gent, near the Zuid park" .
ex:sig a ex:Signal ; rdfs:label "Signal 12B" .
ex:trase a ex:Line ; rdfs:label "Trasé Oslo" .
ex:notext ex:name "Brussel-Zuid" .
GRAPH ex:g { ex:named rdfs:label "Brussel centraal" }
"#;

fn engine() -> Engine {
    // The tripletrail store whatever TRIPLETRAIL_STORE says: the index is on its term ids.
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::TriG, DATA.as_bytes()).unwrap();
    e
}

fn rows(e: &Engine, q: &str) -> Vec<Vec<String>> {
    let QueryResults::Solutions(s) = e.query(&format!("{PREFIXES}{q}")).unwrap() else {
        panic!()
    };
    let vars: Vec<_> = s.variables().to_vec();
    s.map(|r| {
        let r = r.unwrap();
        vars.iter()
            .map(|v| r.get(v).map_or("-".to_owned(), ToString::to_string))
            .collect()
    })
    .collect()
}

#[test]
fn finds_resources_by_words_of_their_labels() {
    let e = engine();
    let mut got = rows(&e, r#"SELECT ?s WHERE { ?s tt:search "brussel" }"#);
    got.sort();
    assert_eq!(
        got,
        [
            ["<http://example.com/bxl>"],
            ["<http://example.com/bxn>"],
            ["<http://example.com/named>"],
        ]
    );
    // Only indexed properties: ex:name is not one.
    assert!(!got.iter().any(|r| r[0].contains("notext")));
}

#[test]
fn more_words_matched_score_higher_and_join_with_the_rest() {
    let e = engine();
    let got = rows(
        &e,
        r#"SELECT ?s ?label WHERE {
             ?s tt:search ("brussel zuid" ?score ?label) ; a ex:Station .
           } ORDER BY DESC(?score) ?label"#,
    );
    assert_eq!(got[0], ["<http://example.com/bxl>", "\"Brussel-Zuid\""]);
    let stations: Vec<&str> = got.iter().map(|r| r[0].as_str()).collect();
    assert!(stations.contains(&"<http://example.com/bxn>"));
    assert!(stations.contains(&"<http://example.com/gent>"));
    // ex:named has no type: the join drops it.
    assert!(!stations.contains(&"<http://example.com/named>"));
}

#[test]
fn accents_case_prefixes_and_limits() {
    let e = engine();
    assert_eq!(
        rows(&e, r#"SELECT ?s WHERE { ?s tt:search "TRASE" }"#),
        [["<http://example.com/trase>"]]
    );
    assert_eq!(
        rows(&e, r#"SELECT ?s WHERE { ?s tt:search "bruxel*" }"#),
        [["<http://example.com/bxl>"]]
    );
    assert_eq!(
        rows(&e, r#"SELECT ?s WHERE { ?s tt:search "12b" }"#),
        [["<http://example.com/sig>"]]
    );
    assert_eq!(
        rows(&e, r#"SELECT ?s WHERE { ?s tt:search ("brussel" 1) }"#).len(),
        1
    );
    assert!(rows(&e, r#"SELECT ?s WHERE { ?s tt:search "nowhere" }"#).is_empty());
}

#[test]
fn a_constant_subject_keeps_its_own_matches() {
    let e = engine();
    assert_eq!(
        rows(
            &e,
            r#"SELECT ?l WHERE { ex:bxl tt:search ("brussel" ?score ?l) }"#
        ),
        [["\"Brussel-Zuid\""]]
    );
    assert_eq!(
        rows(
            &e,
            r#"SELECT ?l WHERE { ex:sig tt:search ("brussel" ?score ?l) }"#
        ),
        Vec::<Vec<String>>::new()
    );
}

#[test]
fn the_index_follows_updates() {
    let e = engine();
    assert!(rows(&e, r#"SELECT ?s WHERE { ?s tt:search "antwerpen" }"#).is_empty());
    e.update(
        r#"PREFIX ex: <http://example.com/> PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
                INSERT DATA { ex:ant rdfs:label "Antwerpen-Centraal" }"#,
    )
    .unwrap();
    assert_eq!(
        rows(&e, r#"SELECT ?s WHERE { ?s tt:search "antwerpen" }"#),
        [["<http://example.com/ant>"]]
    );
}

/// A query string from a variable is searched once per value the rest of the query gives
/// it (#188), as Jena does: each search limited on its own, ?q bound to its string.
#[test]
fn a_query_string_from_a_variable_is_searched_once_per_value() {
    let e = engine();
    let sorted = |q: &str| {
        let mut got = rows(&e, q);
        got.sort();
        got
    };
    // VALUES, before or after the call in the group.
    // One row per literal matched, as without a variable: DISTINCT for one per resource.
    for q in [
        r#"SELECT DISTINCT ?q ?s WHERE { VALUES ?q { "noord" "gent" } ?s tt:search ?q }"#,
        r#"SELECT DISTINCT ?q ?s WHERE { ?s tt:search ?q VALUES ?q { "noord" "gent" } }"#,
        r#"PREFIX text: <http://jena.apache.org/text#>
           SELECT DISTINCT ?q ?s WHERE { VALUES ?q { "noord" "gent" } ?s text:query (rdfs:label ?q) }"#,
    ] {
        assert_eq!(
            sorted(q),
            [
                [r#""gent""#, "<http://example.com/gent>"],
                [r#""noord""#, "<http://example.com/bxn>"],
            ],
            "{q}"
        );
    }
    // BIND, and a triple of the query: the label of one resource searched for others.
    assert_eq!(
        sorted(r#"SELECT ?s WHERE { BIND("trasé" AS ?q) ?s tt:search ?q }"#),
        [["<http://example.com/trase>"]]
    );
    assert_eq!(
        sorted(
            r#"SELECT ?s WHERE { ex:notext ex:name ?q . ?s tt:search (?q ?score ?l) FILTER(?s != ex:named) }"#
        ),
        // Words are OR'ed: "zuid" is in Gent's comment.
        [
            ["<http://example.com/bxl>"],
            ["<http://example.com/bxn>"],
            ["<http://example.com/gent>"]
        ]
    );
    // OPTIONAL sees its left side; the limit is per string.
    assert_eq!(
        sorted(
            r#"SELECT ?q ?s WHERE { VALUES ?q { "brussel" "signal" } OPTIONAL { ?s tt:search (?q 1) } }"#
        )
        .len(),
        2
    );
    // Lucene syntax in each string of text:query.
    assert_eq!(
        sorted(
            r#"PREFIX text: <http://jena.apache.org/text#>
               SELECT ?s WHERE { VALUES ?q { "+brussel -zuid" } ?s text:query ?q }"#
        ),
        [["<http://example.com/bxn>"], ["<http://example.com/named>"]]
    );
    // An unbound value and a value that is not a string search nothing.
    assert!(sorted(r#"SELECT ?s WHERE { VALUES ?q { UNDEF ex:bxl } ?s tt:search ?q }"#).is_empty());
}

#[test]
fn a_query_string_bound_by_nothing_is_refused() {
    let e = engine();
    for q in [
        r#"SELECT ?s WHERE { ?s tt:search ?q }"#,
        // Bound in another group of a union.
        r#"SELECT ?s WHERE { { BIND("gent" AS ?q) } UNION { ?s tt:search ?q } }"#,
        // A sub-select's own variables are its own.
        r#"SELECT ?s WHERE { BIND("gent" AS ?q) { SELECT ?s WHERE { ?s tt:search ?q } } }"#,
    ] {
        let err = refused(&e, q);
        assert!(err.contains("nothing binds"), "{q}: {err}");
    }
}

#[test]
fn query_strings_from_a_variable_are_read_in_what_the_caller_sees() {
    use tripletrail::oxigraph::model::NamedNode;
    let e = engine();
    e.update(&format!(
        r#"{PREFIXES} INSERT DATA {{ ex:ask ex:wants "noord" . GRAPH ex:g {{ ex:ask ex:wants "centraal" }} }}"#
    ))
    .unwrap();
    let q = format!(
        r#"{PREFIXES} SELECT DISTINCT ?s WHERE {{
            {{ ex:ask ex:wants ?q }} UNION {{ GRAPH ?g {{ ex:ask ex:wants ?q }} }}
            ?s tt:search ?q
        }}"#
    );
    let mut all: Vec<Vec<String>> = rows(&e, &q[PREFIXES.len()..]);
    all.sort();
    assert_eq!(
        all,
        [["<http://example.com/bxn>"], ["<http://example.com/named>"]]
    );
    let options = tripletrail::QueryOptions {
        readable: Some(tripletrail::ReadableGraphs {
            default_graph: false,
            named: vec![NamedNode::new("http://example.com/g").unwrap()],
        }),
        ..Default::default()
    };
    let QueryResults::Solutions(s) = e.query_with_options(&q, options).unwrap() else {
        panic!()
    };
    let got: Vec<String> = s
        .map(|r| r.unwrap().get("s").unwrap().to_string())
        .collect();
    assert_eq!(got, ["<http://example.com/named>"]);
}

fn refused(e: &Engine, q: &str) -> String {
    e.query(&format!(
        "{PREFIXES}PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>\n{q}"
    ))
    .err()
    .expect("refused")
    .to_string()
}

#[test]
fn argument_lists_that_loop_are_refused() {
    let e = engine();
    for q in [
        r#"SELECT ?s WHERE { ?s tt:search _:a . _:a rdf:first "brussel" ; rdf:rest _:a }"#,
        r#"SELECT ?s WHERE { ?s tt:search _:a . _:a rdf:first "brussel" ; rdf:rest _:b .
                             _:b rdf:first ?x ; rdf:rest _:a }"#,
    ] {
        assert!(refused(&e, q).contains("at most 5 items"), "{q}");
    }
}

#[test]
fn search_where_it_cannot_be_answered_is_refused() {
    let e = engine();
    for q in [
        r#"SELECT ?a WHERE { ex:bxl rdfs:label ?a FILTER EXISTS { ?s tt:search "brussel" } }"#,
        r#"SELECT ?a WHERE { ex:bxl rdfs:label ?a FILTER NOT EXISTS { ?s tt:search "brussel" } }"#,
        r#"SELECT ?s WHERE { GRAPH ?g { ?s tt:search "brussel" } }"#,
        r#"SELECT ?s WHERE { GRAPH ex:g { ?s tt:search "brussel" } }"#,
    ] {
        assert!(refused(&e, q).contains("not supported inside"), "{q}");
    }
}

#[test]
fn a_variable_given_twice_is_refused() {
    let e = engine();
    assert!(refused(&e, r#"SELECT * WHERE { ?s tt:search ("brussel" ?l ?l) }"#).contains("twice"));
    assert!(refused(&e, r#"SELECT * WHERE { ?s tt:search ("brussel" ?s) }"#).contains("twice"));
}

#[test]
fn a_bound_subject_is_filtered_before_the_limit() {
    let e = engine();
    // ex:bxn is not the best match for "brussel", but its own match is kept.
    assert_eq!(
        rows(
            &e,
            r#"SELECT ?l WHERE { ex:bxn tt:search ("brussel" ?sc ?l 1) }"#
        ),
        [["\"Brussel-Noord\""]]
    );
}

/// The engine's index, patched by updates, against one built from the data as it is now.
fn same_as_built(e: &Engine, queries: &[&str]) -> usize {
    let patched = e.text_index().unwrap();
    let tripletrail::store::Dataset::TripleTrail(store) = e.store() else {
        panic!()
    };
    let built = tripletrail::text::TextIndex::build(store).unwrap();
    assert_eq!(patched.len(), built.len());
    for q in queries {
        assert_eq!(
            patched.search(q, None, |_, _| true),
            built.search(q, None, |_, _| true),
            "{q}"
        );
    }
    patched.removed()
}

#[test]
fn updates_are_patched_into_the_index_as_a_new_build_would_have_them() {
    let e = Engine::new_tripletrail();
    let words = [
        "noord", "zuid", "oost", "west", "centraal", "haven", "brug", "dorp",
    ];
    let mut data = String::from(
        "@prefix ex: <http://example.com/> .\n@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n",
    );
    for i in 0..300 {
        data += &format!(
            "ex:s{i} rdfs:label \"Station {} {}\" ; rdfs:comment \"Halte {i}\" .\n",
            words[i % 8],
            words[(i / 8) % 8]
        );
    }
    // Not literals: kept apart from the documents.
    data += "ex:s0 rdfs:label ex:thing . GRAPH ex:g { ex:s1 rdfs:label \"Station zuid oost\" }\n";
    e.load(RdfFormat::TriG, data.as_bytes()).unwrap();
    let queries = [
        "station",
        "zuid",
        "haven brug",
        "dorp*",
        "halte 17",
        "nieuw",
        "w*",
    ];
    assert_eq!(same_as_built(&e, &queries), 0);
    for update in [
        // New labels, one a literal other subjects have.
        r#"INSERT DATA { ex:n1 rdfs:label "Nieuw Station haven" . ex:n2 rdfs:label "Station noord noord" }"#,
        // Removed and changed labels.
        r#"DELETE DATA { ex:s5 rdfs:label "Station haven noord" . ex:s6 rdfs:comment "Halte 6" }"#,
        r#"DELETE { ex:s7 rdfs:label ?l } INSERT { ex:s7 rdfs:label "Station dorp dorp" } WHERE { ex:s7 rdfs:label ?l }"#,
        // A pair in a second graph, then gone from one of them: the document stays.
        r#"INSERT DATA { GRAPH ex:g { ex:s2 rdfs:label "Station oost noord" } }"#,
        r#"DELETE DATA { ex:s2 rdfs:label "Station oost noord" }"#,
        // A non-literal removed, and a whole subject.
        r#"DELETE DATA { ex:s0 rdfs:label ex:thing }"#,
        r#"DELETE WHERE { ex:s9 ?p ?o }"#,
        // Data that is not indexed.
        r#"INSERT DATA { ex:s3 ex:name "Station zuid" }"#,
    ] {
        e.update(&format!(
            "PREFIX ex: <http://example.com/> PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#> {update}"
        ))
        .unwrap();
        same_as_built(&e, &queries);
    }
    // Patched, not built again: the removed documents' slots are still there.
    assert_eq!(same_as_built(&e, &queries), 5);
}

#[test]
fn jena_text_query_reads_the_same_index() {
    let e = engine();
    let q = |body: &str| {
        let mut r = rows(
            &e,
            &format!("PREFIX text: <http://jena.apache.org/text#>\n{body}"),
        );
        r.sort();
        r
    };
    // A plain string, as tt:search.
    assert_eq!(
        q(r#"SELECT ?s WHERE { ?s text:query "brussel" }"#),
        rows(
            &e,
            r#"SELECT ?s WHERE { ?s tt:search "brussel" } ORDER BY ?s"#
        )
    );
    // A property: only its literals. "Zuid" is in a label and in a comment.
    assert_eq!(
        q(r#"SELECT ?s WHERE { ?s text:query (rdfs:label "zuid") }"#),
        [["<http://example.com/bxl>"]]
    );
    assert_eq!(
        q(r#"SELECT ?s WHERE { ?s text:query (rdfs:comment "zuid") }"#),
        [["<http://example.com/gent>"]]
    );
    // The subject list, the limit and Lucene's OR and quotes.
    let got = q(r#"SELECT ?s ?l WHERE { (?s ?score ?l) text:query ("brussel OR \"zuid\"" 1) }"#);
    assert_eq!(got, [["<http://example.com/bxl>", "\"Brussel-Zuid\""]]);
    // A one-word phrase is the word, and -1 is no limit.
    assert_eq!(
        q(r#"SELECT ?s WHERE { ?s text:query ("\"trasé\"" -1) }"#),
        [["<http://example.com/trase>"]]
    );
    // A limit counts the property's matches only.
    assert_eq!(
        q(r#"SELECT ?s WHERE { ?s text:query (skos:altLabel "brussel bruxelles" 1) }"#),
        [["<http://example.com/bxl>"]]
    );
}

#[test]
fn jena_text_query_refuses_what_it_cannot_answer() {
    let e = engine();
    for (q, why) in [
        (
            r#"?s text:query (ex:name "brussel")"#,
            "not an indexed property",
        ),
        (r#"?s text:query "brussel AND""#, "nothing after"),
        (r#"?s text:query "NOT""#, "nothing after"),
        (r#"?s text:query "\"brussel noord""#, "not closed"),
        (r#"?s text:query "\"brus* noord\"""#, "wildcard in a phrase"),
        (r#"?s text:query "label:brussel""#, "field"),
        (r#"?s text:query "brus?el""#, "Lucene syntax"),
        (r#"?s text:query "brussel~""#, "Lucene syntax"),
        (r#"?s text:query "brussel^2 zuid""#, "Lucene syntax"),
        (r#"?s text:query "(brussel zuid)""#, "Lucene syntax"),
        (r#"?s text:query "*zuid""#, "wildcard"),
        (
            r#"?s text:query ("brussel" "highlight:x:1")"#,
            "not supported",
        ),
        (r#"?s text:query ("brussel" "lang:")"#, "language tag"),
        (r#"(?s ?sc ?l ?g) text:query "brussel""#, "?graph"),
    ] {
        let err = refused(
            &e,
            &format!("PREFIX text: <http://jena.apache.org/text#> SELECT * WHERE {{ {q} }}"),
        );
        assert!(
            err.contains("text:query") && err.contains(why),
            "{q}: {err}"
        );
    }
}

fn subjects(e: &Engine, query: &str) -> Vec<String> {
    let mut r: Vec<String> = rows(
        e,
        &format!("PREFIX text: <http://jena.apache.org/text#> SELECT ?s WHERE {{ ?s text:query {query} }}"),
    )
    .into_iter()
    .map(|r| r[0].trim_start_matches("<http://example.com/").trim_end_matches('>').to_owned())
    .collect();
    r.sort();
    r
}

/// Lucene's operators and phrases (#830): AND, NOT, +word, -word and "several words".
#[test]
fn jena_text_query_takes_lucenes_operators_and_phrases() {
    let e = engine();
    let q = |query: &str| subjects(&e, query);
    assert_eq!(q(r#""brussel AND zuid""#), ["bxl"]);
    assert_eq!(q(r#""brussel && zuid""#), ["bxl"]);
    assert_eq!(q(r#""+brussel +zuid""#), ["bxl"]);
    assert_eq!(q(r#""+brussel -zuid""#), ["bxn", "named"]);
    assert_eq!(q(r#""brussel NOT noord""#), ["bxl", "named"]);
    assert_eq!(q(r#""brussel !noord""#), ["bxl", "named"]);
    assert_eq!(q(r#""brussel OR gent""#), q(r#""brussel gent""#));
    // Only excluded words match nothing, as in Lucene.
    assert!(q(r#""NOT brussel""#).is_empty());
    // Phrases: the words one after the other, in that order.
    assert_eq!(q(r#""\"brussel noord\"""#), ["bxn"]);
    assert!(q(r#""\"noord brussel\"""#).is_empty());
    assert_eq!(q(r#""bru* -noord""#), ["bxl", "bxl", "named"]);
}

/// `lang:` keeps the literals in a language, and `highlight:` marks the words matched (#830).
#[test]
fn jena_text_query_takes_lang_and_highlight() {
    let e = Engine::new_tripletrail();
    let mut data = String::from(
        "@prefix ex: <http://example.com/> .\n@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         ex:a rdfs:label \"Brussel-Zuid\"@nl, \"Bruxelles-Midi\"@fr ; \
         rdfs:comment \"Station in Brussels, the south one\"@en-GB .\n\
         ex:b rdfs:label \"Brussel-Noord\" .\nex:long rdfs:comment \"",
    );
    data += &"Lorem ipsum dolor sit amet. ".repeat(10);
    data += "The station of Antwerpen is here. ";
    data += &"Lorem ipsum dolor sit amet. ".repeat(10);
    data += "\" .\n";
    e.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    let q = |body: &str| {
        let mut r = rows(
            &e,
            &format!("PREFIX text: <http://jena.apache.org/text#> {body}"),
        );
        r.sort();
        r
    };
    assert_eq!(
        q(r#"SELECT ?s ?l WHERE { (?s ?sc ?l) text:query ("brussel bruxelles" "lang:fr") }"#),
        [["<http://example.com/a>", "\"Bruxelles-Midi\"@fr"]]
    );
    // A tag's subtags: en matches en-GB.
    assert_eq!(
        q(r#"SELECT ?s WHERE { ?s text:query (rdfs:comment "brussels" "lang:en") }"#),
        [["<http://example.com/a>"]]
    );
    // Jena's markers by default; a literal keeps its language.
    assert_eq!(
        q(r#"SELECT ?s ?l WHERE { (?s ?sc ?l) text:query ("brussel" "highlight:") }"#),
        [
            ["<http://example.com/a>", "\"↦Brussel↤-Zuid\"@nl"],
            ["<http://example.com/b>", "\"↦Brussel↤-Noord\""],
        ]
    );
    // Options, and SELECT * shows no variable of the rewrite.
    assert_eq!(
        q(r#"SELECT * WHERE { (?s ?sc ?l) text:query (rdfs:label "+brussel +noord" "highlight:s:<b> | e:</b>") }"#)
            .into_iter()
            .map(|r| (r.len(), r.into_iter().find(|v| v.starts_with('"')).unwrap()))
            .collect::<Vec<_>>(),
        [(3, "\"<b>Brussel</b>-<b>Noord</b>\"".to_owned())]
    );
    // A long literal: a fragment around its match.
    let got = q(r#"SELECT ?l WHERE { (?s ?sc ?l) text:query ("antwerpen" "highlight:f:40") }"#);
    let shown = &got[0][0];
    assert!(shown.contains("↦Antwerpen↤"), "{shown}");
    assert!(shown.chars().count() < 60, "{shown}");
}

/// With `lang:`, the words match their other forms in that language, as with Jena's
/// multilingual index and Lucene's language analyzers (#830).
#[test]
fn jena_text_query_stems_in_the_language_asked_for() {
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        r#"@prefix ex: <http://example.com/> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
ex:en rdfs:label "Connecting stations"@en-GB .
ex:no rdfs:label "Stasjonen ved sporet"@no, "Sporene"@nb .
ex:nl rdfs:label "De stations van Brussel"@nl .
ex:de rdfs:label "Die Bahnhöfe"@de .
ex:plain rdfs:label "stations" .
"#
        .as_bytes(),
    )
    .unwrap();
    let q = |query: &str| subjects(&e, query);
    assert_eq!(q(r#"("station" "lang:en")"#), ["en"]);
    assert_eq!(q(r#"("connected station" "lang:en")"#), ["en"]);
    assert_eq!(q(r#"("stasjoner" "lang:no")"#), ["no"]);
    // nb keeps the literals tagged nb, stemmed as Norwegian.
    assert_eq!(q(r#"("spor" "lang:nb")"#), ["no"]);
    assert_eq!(q(r#"("spor" "lang:no")"#), ["no"]);
    assert_eq!(q(r#"("station" "lang:nl")"#), ["nl"]);
    assert_eq!(q(r#"("bahnhof" "lang:de")"#), ["de"]);
    // A phrase in stems, and the stems kept apart from other languages' words.
    assert_eq!(q(r#"("\"connected station\"" "lang:en")"#), ["en"]);
    assert!(q(r#"("station" "lang:fr")"#).is_empty());
    // Without lang: a word matches itself only.
    assert_eq!(q(r#""station""#), Vec::<String>::new());
    assert_eq!(q(r#""stations""#), ["en", "nl", "plain"]);
    // The whole word of another form is highlighted.
    assert_eq!(
        rows(
            &e,
            r#"PREFIX text: <http://jena.apache.org/text#>
               SELECT ?l WHERE { (?s ?sc ?l) text:query ("station" "lang:en" "highlight:s:[ | e:]") }"#
        ),
        [["\"Connecting [stations]\"@en-gb"]]
    );
    // The stems go with their literal.
    e.update(
        r#"PREFIX ex: <http://example.com/> PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
           DELETE DATA { ex:en rdfs:label "Connecting stations"@en-GB }"#,
    )
    .unwrap();
    assert!(q(r#"("station" "lang:en")"#).is_empty());
}

/// A dataset's own indexed properties (#188), as the fields of a jena-text entity map:
/// searched by `tt:search` and `text:query`, kept up to date, and in callers' views too.
#[test]
fn a_dataset_indexes_properties_of_its_own() {
    use tripletrail::oxigraph::model::NamedNode;
    let e = engine();
    let name = NamedNode::new("http://example.com/name").unwrap();
    let search = |e: &Engine| {
        let mut got: Vec<String> = rows(e, r#"SELECT ?s WHERE { ?s tt:search "brussel" }"#)
            .into_iter()
            .map(|r| r[0].clone())
            .collect();
        got.sort();
        got
    };
    assert!(!search(&e).contains(&"<http://example.com/notext>".to_owned()));
    let err = refused(
        &e,
        r#"PREFIX text: <http://jena.apache.org/text#> SELECT ?s WHERE { ?s text:query (ex:name "brussel") }"#,
    );
    assert!(err.contains("not an indexed property"), "{err}");
    assert!(err.contains("text-properties"), "{err}");

    e.set_text_properties(std::slice::from_ref(&name));
    assert_eq!(
        e.text_properties().last(),
        Some(&name),
        "the defaults first, then the dataset's own"
    );
    assert!(search(&e).contains(&"<http://example.com/notext>".to_owned()));
    // With the property named, only its literals.
    assert_eq!(subjects(&e, r#"(ex:name "brussel")"#), ["notext"]);
    assert_eq!(subjects(&e, r#"(rdfs:label "zuid")"#), ["bxl"]);
    // Kept up to date after a write, as a fresh build would have it.
    e.update(&format!(
        r#"{PREFIXES} INSERT DATA {{ ex:more ex:name "Brussel-Kapellekerk" . GRAPH ex:g {{ ex:gn ex:name "Brussel-Schuman" }} }}"#
    ))
    .unwrap();
    assert_eq!(
        subjects(&e, r#"(ex:name "brussel")"#),
        ["gn", "more", "notext"]
    );
    // A caller who reads some graphs only searches those, with the same properties.
    let options = tripletrail::QueryOptions {
        readable: Some(tripletrail::ReadableGraphs {
            default_graph: false,
            named: vec![NamedNode::new("http://example.com/g").unwrap()],
        }),
        ..Default::default()
    };
    let QueryResults::Solutions(s) = e
        .query_with_options(
            &format!(r#"{PREFIXES} SELECT ?s WHERE {{ ?s tt:search "brussel" }}"#),
            options,
        )
        .unwrap()
    else {
        panic!()
    };
    let mut in_g: Vec<String> = s
        .map(|r| r.unwrap().get("s").unwrap().to_string())
        .collect();
    in_g.sort();
    assert_eq!(
        in_g,
        ["<http://example.com/gn>", "<http://example.com/named>"]
    );

    // Back to the defaults.
    e.set_text_properties(&[]);
    assert!(!search(&e).contains(&"<http://example.com/notext>".to_owned()));
    assert!(
        refused(
            &e,
            r#"PREFIX text: <http://jena.apache.org/text#> SELECT ?s WHERE { ?s text:query (ex:name "brussel") }"#,
        )
        .contains("not an indexed property")
    );
}

const BDS: &str = "PREFIX bds: <http://www.bigdata.com/rdf/search#>\n";

/// Blazegraph's `bds:search` (#188): the literals matched, one row each, with its options.
#[test]
fn blazegraph_bds_search_reads_the_same_index() {
    let e = engine();
    let q = |body: &str| rows(&e, &format!("{BDS}{body}"));
    assert_eq!(
        q(r#"SELECT ?o ?rank WHERE { ?o bds:search "brussel" ; bds:rank ?rank } ORDER BY ?rank"#)
            .len(),
        3,
        "one row per literal, \"Brussel-Zuid\" once though two resources have it"
    );
    let ranked = q(r#"SELECT ?rank WHERE {
            ?o bds:search "brussel" ; bds:relevance ?score ; bds:rank ?rank ; bds:minRank 2
        } ORDER BY ?rank"#);
    assert_eq!(
        ranked,
        [
            [r#""2"^^<http://www.w3.org/2001/XMLSchema#integer>"#],
            [r#""3"^^<http://www.w3.org/2001/XMLSchema#integer>"#],
        ]
    );
    // bds:maxRank is a place, as bds:minRank: places 2 and 3 of the 3; none past it.
    assert_eq!(
        q(r#"SELECT ?rank WHERE {
            ?o bds:search "brussel" ; bds:rank ?rank ; bds:minRank 2 ; bds:maxRank 2
        }"#),
        [[r#""2"^^<http://www.w3.org/2001/XMLSchema#integer>"#]]
    );
    assert_eq!(
        q(r#"SELECT ?rank WHERE {
            ?o bds:search "brussel" ; bds:rank ?rank ; bds:minRank 2 ; bds:maxRank 3
        } ORDER BY ?rank"#)
        .len(),
        2
    );
    assert!(
        q(r#"SELECT ?o WHERE { ?o bds:search "brussel" ; bds:minRank 3 ; bds:maxRank 2 }"#)
            .is_empty()
    );
    assert_eq!(
        q(r#"SELECT ?o WHERE { ?o bds:search "brussel zuid" ; bds:maxRank 1 }"#),
        [[r#""Brussel-Zuid""#]]
    );
    let mut any = q(r#"SELECT ?o WHERE { ?o bds:search "brussel zuid" }"#);
    any.sort();
    assert_eq!(any.len(), 4, "{any:?}");
    assert_eq!(
        q(r#"SELECT ?o WHERE { ?o bds:search "brussel zuid" ; bds:matchAllTerms "true" }"#),
        [[r#""Brussel-Zuid""#]]
    );
    assert!(q(r#"SELECT ?o WHERE { ?o bds:search "noor" }"#).is_empty());
    assert_eq!(
        q(r#"SELECT ?o WHERE { ?o bds:search "noor" ; bds:prefixMatch true }"#),
        [[r#""Brussel-Noord""#]]
    );
    // Joined with the resources that have the literal, as in Blazegraph.
    assert_eq!(
        q(r#"SELECT ?s WHERE { ?s rdfs:label ?o . ?o bds:search "noord" }"#),
        [["<http://example.com/bxn>"]]
    );
    // A query string from a variable.
    assert_eq!(
        q(r#"SELECT ?o WHERE { VALUES ?q { "noord" } ?o bds:search ?q ; bds:matchAllTerms true }"#),
        [[r#""Brussel-Noord""#]]
    );
}

/// A FILTER on a score: the range pass (#71) left a table's columns alone, which hold no
/// triples to range over; it used to answer nothing.
#[test]
fn a_filter_on_the_score_keeps_the_rows_it_should() {
    let e = engine();
    let mut got = rows(
        &e,
        r#"SELECT ?s WHERE { ?s tt:search ("zuid" ?score) FILTER(?score > 0) }"#,
    );
    got.sort();
    assert_eq!(
        got,
        [["<http://example.com/bxl>"], ["<http://example.com/gent>"]]
    );
}

#[test]
fn stardog_text_match_reads_the_same_index() {
    let e = engine();
    let q = |body: &str| {
        let mut got = rows(
            &e,
            &format!("PREFIX fts: <tag:stardog:api:property:>\n{body}"),
        );
        got.sort();
        got
    };
    assert_eq!(
        q(r#"SELECT ?l WHERE { ?l fts:textMatch ("+brussel -zuid" 10) }"#),
        [[r#""Brussel centraal""#], [r#""Brussel-Noord""#]]
    );
    assert_eq!(
        q(r#"SELECT ?l WHERE { (?l ?score) fts:textMatch "zuid" FILTER(?score > 0) }"#),
        [
            [r#""Brussel-Zuid""#],
            [r#""Station in Gent, near the Zuid park""#]
        ]
    );
    assert_eq!(
        q(r#"SELECT ?l WHERE { ?l fts:textMatch ("brussel" 1) }"#).len(),
        1
    );
}

#[test]
fn other_engines_options_that_cannot_be_answered_are_refused() {
    let e = engine();
    for (q, why) in [
        (
            r#"?o bds:search "brussel" ; bds:minRelevance "0.5""#,
            "BM25",
        ),
        (
            r#"?o bds:search "brussel" ; bds:matchExact true"#,
            "not supported",
        ),
        (
            r#"?o bds:relevance ?score"#,
            "nothing searches with bds:search",
        ),
        (r#"ex:bxl bds:search "brussel""#, "must be a variable"),
        (
            r#"?o bds:search "brussel" ; bds:maxRank "x""#,
            "whole number",
        ),
        (
            r#"?l <tag:stardog:api:property:textMatch> ("brussel" 0.5 10)"#,
            "threshold",
        ),
        (
            r#"?l <tag:stardog:api:property:textMatch> ("brussel" -3)"#,
            "0 or more",
        ),
    ] {
        let err = refused(&e, &format!("{BDS}SELECT * WHERE {{ {q} }}"));
        assert!(err.contains(why), "{q}: {err}");
    }
}

/// The other engines' forms work in an update's WHERE too, as tt:search does.
#[test]
fn bds_search_and_mg_search_work_in_an_update() {
    let e = engine();
    e.update(&format!(
        r#"{PREFIXES}{BDS} INSERT {{ ?s ex:found "bds" }} WHERE {{ ?o bds:search "noord" . ?s rdfs:label ?o }}"#
    ))
    .unwrap();
    e.update(&format!(
        r#"{PREFIXES} INSERT {{ ?s ex:found "mg" }} WHERE {{ ?s tt:search "trasé" }}"#
    ))
    .unwrap();
    let mut got = rows(&e, "SELECT ?s ?how WHERE { ?s ex:found ?how }");
    got.sort();
    assert_eq!(
        got,
        [
            ["<http://example.com/bxn>", r#""bds""#],
            ["<http://example.com/trase>", r#""mg""#]
        ]
    );
}

/// A search reads the store as it was before the update: only in its first operation that
/// changes anything, and not yet for a caller limited to some graphs.
#[test]
fn a_search_an_update_cannot_answer_is_refused() {
    use tripletrail::oxigraph::model::NamedNode;
    let e = engine();
    let err = e
        .update(&format!(
            r#"{PREFIXES} INSERT DATA {{ ex:new rdfs:label "Brussel-West" }} ;
               INSERT {{ ?s ex:found true }} WHERE {{ ?s tt:search "west" }}"#
        ))
        .expect_err("refused")
        .to_string();
    assert!(err.contains("first operation"), "{err}");
    let g = tripletrail::ReadableGraphs {
        default_graph: false,
        named: vec![NamedNode::new("http://example.com/g").unwrap()],
    };
    let err = e
        .update_with_options(
            &format!(
                r#"{PREFIXES} INSERT {{ GRAPH ex:g {{ ?s ex:found true }} }} WHERE {{ ?s tt:search "brussel" }}"#
            ),
            &tripletrail::UpdateOptions {
                readable: Some(g.clone()),
                writable: Some(g),
                ..Default::default()
            },
        )
        .expect_err("refused")
        .to_string();
    assert!(err.contains("access rules"), "{err}");
}
