//! The path search SERVICE (#234): paths over any predicates, as rows.

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;

/// a → b → c → d, a shortcut a → c, a cycle x ⇄ y, a class tree, and `ex:q` in graph g.
const DATA: &str = r#"
@prefix ex: <http://example.com/> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
ex:a ex:next ex:b , ex:c . ex:b ex:next ex:c . ex:c ex:next ex:d .
ex:x ex:next ex:y . ex:y ex:next ex:x .
ex:Signal rdfs:subClassOf ex:Asset . ex:MainSignal rdfs:subClassOf ex:Signal .
ex:Switch rdfs:subClassOf ex:Asset . ex:Asset rdfs:subClassOf ex:Thing .
GRAPH ex:g { ex:a ex:q ex:d . }
"#;

fn engine() -> Engine {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::TriG, DATA.as_bytes()).unwrap();
    e
}

/// The rows of a path SERVICE with `body`, as space-separated values in the order of the
/// projected variables (local names, numbers), in the service's order.
fn rows(e: &Engine, body: &str) -> Vec<String> {
    select(e, "*", body)
}

/// [`rows`], projecting `vars` (a sub-select's variables are in scope too).
fn select(e: &Engine, vars: &str, body: &str) -> Vec<String> {
    let q = format!(
        "PREFIX ex: <http://example.com/>
         PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
         PREFIX path: <http://w3id.org/tripletrail/path#>
         SELECT {vars} {{ SERVICE <http://w3id.org/tripletrail/path> {{ {body} }} }}"
    );
    let QueryResults::Solutions(s) = e.query(&q).unwrap() else {
        panic!()
    };
    let vars: Vec<_> = s.variables().to_vec();
    s.map(|r| {
        let r = r.unwrap();
        vars.iter()
            .map(|v| match r.get(v) {
                None => "-".to_owned(),
                Some(tripletrail::oxigraph::model::Term::Literal(l)) => l.value().to_owned(),
                Some(t) => t
                    .to_string()
                    .trim_end_matches('>')
                    .rsplit(['/', '#'])
                    .next()
                    .unwrap()
                    .to_owned(),
            })
            .collect::<Vec<_>>()
            .join(" ")
    })
    .collect()
}

fn error(e: &Engine, body: &str) -> String {
    let q = format!(
        "PREFIX ex: <http://example.com/> PREFIX path: <http://w3id.org/tripletrail/path#>
         SELECT * {{ SERVICE <http://w3id.org/tripletrail/path> {{ {body} }} }}"
    );
    match e.query(&q) {
        Err(err) => err.to_string(),
        Ok(QueryResults::Solutions(mut s)) => s.find_map(Result::err).unwrap().to_string(),
        Ok(_) => panic!(),
    }
}

#[test]
fn shortest_path_with_its_edges() {
    let e = engine();
    assert_eq!(
        rows(
            &e,
            "[] path:source ex:a ; path:target ex:d ; path:predicate ex:next ;
                path:path ?p ; path:step ?i ; path:from ?f ; path:edge ?e ; path:to ?t ; path:length ?n"
        ),
        ["1 1 a next c 2", "1 2 c next d 2"]
    );
}

#[test]
fn hop_counts_from_a_source() {
    let e = engine();
    // Each node reached, once, at its shortest distance.
    assert_eq!(
        rows(
            &e,
            "[] path:source ex:a ; path:predicate ex:next ; path:end ?n ; path:length ?d"
        ),
        ["a 0", "b 1", "c 1", "d 2"]
    );
    assert_eq!(
        rows(
            &e,
            "[] path:source ex:a ; path:predicate ex:next ; path:minLength 1 ; path:maxLength 1 ;
                path:end ?n"
        ),
        ["b", "c"]
    );
    // The depth of each class below ex:Thing: a backwards search from the target.
    assert_eq!(
        rows(
            &e,
            "[] path:target ex:Thing ; path:predicate rdfs:subClassOf ; path:minLength 1 ;
                path:start ?class ; path:length ?depth"
        ),
        ["Asset 1", "Signal 2", "Switch 2", "MainSignal 3"]
    );
}

#[test]
fn all_simple_paths_and_walks() {
    let e = engine();
    assert_eq!(
        rows(
            &e,
            "[] path:source ex:a ; path:target ex:d ; path:predicate ex:next ; path:mode 'simple' ;
                path:path ?p ; path:length ?n"
        ),
        ["1 3", "2 2"]
    );
    assert_eq!(
        rows(
            &e,
            "[] path:source ex:a ; path:target ex:d ; path:predicate ex:next ; path:mode 'simple' ;
                path:maxPaths 1 ; path:length ?n"
        ),
        ["3"]
    );
    // Walks go round the cycle; simple paths don't.
    assert_eq!(
        rows(
            &e,
            "[] path:source ex:x ; path:predicate ex:next ; path:mode 'all' ; path:maxLength 4 ;
                path:minLength 1 ; path:end ?n ; path:length ?d"
        ),
        ["y 1", "x 2", "y 3", "x 4"]
    );
    assert_eq!(
        rows(
            &e,
            "[] path:source ex:x ; path:predicate ex:next ; path:mode 'simple' ; path:minLength 1 ;
                path:end ?n"
        ),
        ["y"]
    );
}

#[test]
fn directions_graphs_and_sub_selects() {
    let e = engine();
    assert_eq!(
        rows(
            &e,
            "[] path:source ex:d ; path:predicate ex:next ; path:direction 'in' ; path:end ?n ; path:length ?l"
        ),
        ["d 0", "c 1", "a 2", "b 2"]
    );
    assert_eq!(
        rows(
            &e,
            "[] path:source ex:b ; path:predicate ex:next ; path:direction 'both' ; path:maxLength 1 ;
                path:minLength 1 ; path:end ?n"
        ),
        ["c", "a"]
    );
    // In a named graph.
    assert_eq!(
        rows(
            &e,
            "[] path:source ex:a ; path:predicate ex:q ; path:minLength 1 ; path:end ?n"
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        rows(
            &e,
            "[] path:source ex:a ; path:predicate ex:q ; path:graph ex:g ; path:minLength 1 ; path:end ?n"
        ),
        ["d"]
    );
    // Edges from a sub-select, with a label.
    assert_eq!(
        select(
            &e,
            "?i ?e",
            "{ SELECT ?s ?o ?label { ?s ex:next ?o BIND(CONCAT(STRAFTER(STR(?s), 'com/'), '-', STRAFTER(STR(?o), 'com/')) AS ?label) } }
             [] path:source ex:a ; path:target ex:d ; path:step ?i ; path:edge ?e"
        ),
        ["1 a-c", "2 c-d"]
    );
    // Without a source, from the targets backwards, the edges still in walking order.
    let mut paths: Vec<String> = select(
        &e,
        "?f ?t",
        "{ SELECT ?s ?o { ?s ex:next ?o } }
             [] path:target ex:d ; path:mode 'simple' ; path:minLength 2 ; path:maxLength 2 ;
                path:path ?p ; path:from ?f ; path:to ?t",
    )
    .chunks(2)
    .map(|edges| edges.join(", "))
    .collect();
    paths.sort();
    assert_eq!(paths, ["a c, c d", "b c, c d"]);
}

#[test]
fn mistakes_are_explained() {
    let e = engine();
    for (body, message) in [
        (
            "[] path:predicate ex:next ; path:end ?n",
            "path:source or a path:target",
        ),
        (
            "[] path:source ex:a ; path:predicate ex:next ; path:exceptPredicate ex:q ; path:end ?n",
            "path:exceptPredicate",
        ),
        (
            "[] path:source ex:a ; path:predicate ex:next",
            "at least one output",
        ),
        (
            "[] path:source ex:a ; path:predicate ex:next ; path:mode 'all' ; path:end ?n",
            "maxLength",
        ),
        (
            "[] path:source ex:a ; path:predicate ex:next ; path:mode 'longest' ; path:end ?n",
            "path:mode",
        ),
        (
            "[] path:source ex:a ; path:predicate ex:next ; path:bogus ?n",
            "unknown parameter",
        ),
        (
            "[] path:source ex:a ; path:predicate ex:next ; path:end ?n ; path:to ?n",
            "bound twice",
        ),
    ] {
        let err = error(&e, body);
        assert!(err.contains(message), "{body}: {err}");
    }
}

#[test]
fn too_many_paths_and_cancellation() {
    // Every simple path of a complete graph of 11 nodes: millions.
    let e = Engine::new_tripletrail();
    let mut nt = String::new();
    for i in 0..11 {
        for j in 0..11 {
            if i != j {
                nt.push_str(&format!("<urn:n{i}> <urn:p> <urn:n{j}> .\n"));
            }
        }
    }
    e.load(RdfFormat::NTriples, nt.as_bytes()).unwrap();
    let body =
        "[] path:source <urn:n0> ; path:predicate <urn:p> ; path:mode 'simple' ; path:end ?n";
    assert!(error(&e, body).contains("more than 1000000 edges"));
    let token = tripletrail::oxigraph::sparql::CancellationToken::new();
    token.cancel();
    let err = error(&e.with_cancellation_token(token), body);
    assert!(err.contains("cancel"), "{err}");
}

/// Without `path:predicate` or a sub-select: every triple between IRIs, read from the indexes
/// (#819), from both ends at once between one source and one target.
#[test]
fn over_every_predicate() {
    let e = engine();
    assert_eq!(
        select(
            &e,
            "?i ?f ?e ?t",
            "[] path:source ex:a ; path:target ex:d ; path:step ?i ; path:from ?f ; path:edge ?e ; path:to ?t"
        ),
        ["1 a next c", "2 c next d"]
    );
    // Either way, between classes.
    assert_eq!(
        select(
            &e,
            "?l",
            "[] path:source ex:MainSignal ; path:target ex:Switch ; path:direction 'both' ; path:length ?l"
        ),
        ["3"]
    );
    assert_eq!(
        select(
            &e,
            "?l",
            "[] path:source ex:MainSignal ; path:target ex:Switch ; path:length ?l"
        ),
        Vec::<String>::new()
    );
    // Too far for the limit, a predicate left out, and in a named graph.
    assert_eq!(
        select(
            &e,
            "?l",
            "[] path:source ex:MainSignal ; path:target ex:Switch ; path:direction 'both' ; path:maxLength 2 ; path:length ?l"
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        select(
            &e,
            "?l",
            "[] path:source ex:a ; path:target ex:d ; path:exceptPredicate ex:next ; path:length ?l"
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        select(
            &e,
            "?l",
            "[] path:source ex:a ; path:target ex:d ; path:graph ex:g ; path:length ?l"
        ),
        ["1"]
    );
    // From the source to everything it reaches, and to itself.
    assert_eq!(
        select(
            &e,
            "?n ?l",
            "[] path:source ex:b ; path:end ?n ; path:length ?l"
        ),
        ["b 0", "c 1", "d 2"]
    );
    assert_eq!(
        select(
            &e,
            "?l",
            "[] path:source ex:b ; path:target ex:b ; path:length ?l"
        ),
        ["0"]
    );
}

/// The search from both ends finds paths as short as those of a search from the source over
/// the same edges given as a sub-select, on a random graph of several predicates.
#[test]
fn both_ends_find_the_shortest_paths() {
    let mut seed = 11u64;
    let mut next = move |n: u64| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) % n
    };
    let mut data = String::from("@prefix ex: <http://example.com/> .\n");
    for _ in 0..600 {
        let (a, p, b) = (next(300), next(4), next(300));
        data.push_str(&format!("ex:n{a} ex:p{p} ex:n{b} .\n"));
    }
    // Literals and rdf:type links are no path.
    data.push_str("ex:n1 ex:label \"one\" . ex:n2 ex:label \"one\" .\n");
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    let length = |body: &str| select(&e, "?l", body).first().cloned();
    for _ in 0..60 {
        let (a, b) = (next(300), next(300));
        for direction in ["out", "both"] {
            let both_ends = length(&format!(
                "[] path:source ex:n{a} ; path:target ex:n{b} ; path:direction '{direction}' ; path:length ?l"
            ));
            let one_end = length(&format!(
                "{{ SELECT ?s ?o {{ ?s ?p ?o FILTER(isIRI(?o)) }} }}
                 [] path:source ex:n{a} ; path:target ex:n{b} ; path:direction '{direction}' ; path:length ?l"
            ));
            assert_eq!(both_ends, one_end, "n{a} to n{b}, {direction}");
            // Every step is a triple of the data, in the direction walked.
            let steps = select(
                &e,
                "?f ?e ?t",
                &format!(
                    "[] path:source ex:n{a} ; path:target ex:n{b} ; path:direction '{direction}' ;
                        path:from ?f ; path:edge ?e ; path:to ?t"
                ),
            );
            let mut at = format!("n{a}");
            for step in &steps {
                let [f, p, t] = step.split(' ').collect::<Vec<_>>()[..] else {
                    panic!("{step}")
                };
                assert_eq!(f, at);
                let there = |s: &str, o: &str| data.contains(&format!("ex:{s} ex:{p} ex:{o} ."));
                assert!(
                    there(f, t) || (direction == "both" && there(t, f)),
                    "{step}"
                );
                at = t.to_owned();
            }
            if !steps.is_empty() {
                assert_eq!(at, format!("n{b}"));
            }
        }
    }
}
