//! A `+` or `*` path joined to many rows (`?s rdf:type/rdfs:subClassOf+ ?o`) keeps the ends it
//! found per start term, so it walks the hierarchy once per class instead of once per row
//! (vendor/spareval, TRIPLETRAIL.md). The answers are those of a closure computed here: forwards
//! and backwards, with a cycle in the hierarchy, with `*`, per named graph, and after an update.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use tripletrail::Engine;

fn sorted(engine: &Engine, query: &str) -> Vec<String> {
    let q = format!("PREFIX ex: <http://example.org/> {query}");
    let QueryResults::Solutions(solutions) = engine.query(&q).unwrap() else {
        panic!("not a SELECT")
    };
    let mut v: Vec<String> = solutions
        .map(|s| {
            let s = s.unwrap();
            let mut vars: Vec<_> = s.iter().map(|(v, t)| format!("{v}={t}")).collect();
            vars.sort();
            vars.join(" ")
        })
        .collect();
    v.sort();
    v
}

/// The classes reachable from `c` in one or more steps of `sub` (`c` itself only on a cycle).
fn plus(sub: &BTreeMap<u32, Vec<u32>>, c: u32) -> BTreeSet<u32> {
    let mut seen = BTreeSet::new();
    let mut todo = sub.get(&c).cloned().unwrap_or_default();
    while let Some(x) = todo.pop() {
        if seen.insert(x) {
            todo.extend(sub.get(&x).into_iter().flatten());
        }
    }
    seen
}

fn iri(c: u32) -> String {
    format!("<http://example.org/C{c}>")
}

#[test]
fn closure_paths_joined_to_many_rows() {
    // A tree of 40 classes (C<i> below C<i/2>) with a cycle C1 -> C0 -> C1, and 3000 subjects
    // with one or two classes each.
    let mut sub: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for i in 1..40 {
        sub.entry(i).or_default().push(i / 2);
    }
    sub.entry(0).or_default().push(1);
    let mut types = Vec::new();
    for i in 0..3000u32 {
        types.push((i, (i * 7) % 40));
        if i % 3 == 0 && (i * 11) % 40 != (i * 7) % 40 {
            types.push((i, (i * 11) % 40));
        }
    }
    let mut d = String::from("@prefix ex: <http://example.org/> .\n");
    for (c, supers) in &sub {
        for s in supers {
            writeln!(d, "ex:C{c} ex:sub ex:C{s} .").unwrap();
        }
    }
    for (s, c) in &types {
        writeln!(d, "ex:s{s} a ex:C{c} .").unwrap();
    }
    // Links that no two subjects share, for a path inside EXISTS whose starts never repeat
    // (the memo gives up on them).
    for i in (0..3000).step_by(2) {
        writeln!(d, "ex:s{i} ex:link ex:l{i} . ex:l{i} ex:link ex:m{i} .").unwrap();
    }
    // In ex:g only C5 is below C4, so a path in that graph gives other ends for C5.
    d.push_str(
        "GRAPH ex:g { ex:s1 a ex:C5 . ex:s2 a ex:C5 . ex:s3 a ex:C3 . ex:C5 ex:sub ex:C4 }\n",
    );
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::TriG, d.as_bytes()).unwrap();

    check(&engine, &sub, &types);

    // Per graph: the rows of ex:g walk ex:g's hierarchy, not the default graph's.
    assert_eq!(
        sorted(
            &engine,
            "SELECT ?s ?o { GRAPH ex:g { ?s a ?c . ?c ex:sub+ ?o } }"
        ),
        [
            "?o=<http://example.org/C4> ?s=<http://example.org/s1>",
            "?o=<http://example.org/C4> ?s=<http://example.org/s2>",
        ]
    );
    assert_eq!(
        sorted(
            &engine,
            "SELECT ?g ?s ?o { GRAPH ?g { ?s a ?c . ?c ex:sub* ?o } }"
        ),
        [
            "?g=<http://example.org/g> ?o=<http://example.org/C3> ?s=<http://example.org/s3>",
            "?g=<http://example.org/g> ?o=<http://example.org/C4> ?s=<http://example.org/s1>",
            "?g=<http://example.org/g> ?o=<http://example.org/C4> ?s=<http://example.org/s2>",
            "?g=<http://example.org/g> ?o=<http://example.org/C5> ?s=<http://example.org/s1>",
            "?g=<http://example.org/g> ?o=<http://example.org/C5> ?s=<http://example.org/s2>",
        ]
    );

    // After an update: C39 moves below C3 and s0 loses a class.
    engine
        .update(
            "PREFIX ex: <http://example.org/> DELETE DATA { ex:C39 ex:sub ex:C19 . ex:s0 a ex:C0 } ;
             INSERT DATA { ex:C39 ex:sub ex:C3 }",
        )
        .unwrap();
    sub.insert(39, vec![3]);
    let types: Vec<_> = types.into_iter().filter(|t| *t != (0, 0)).collect();
    check(&engine, &sub, &types);
}

fn check(engine: &Engine, sub: &BTreeMap<u32, Vec<u32>>, types: &[(u32, u32)]) {
    // Forwards: the start of the path is bound by each `rdf:type` row.
    let mut want = Vec::new();
    for (s, c) in types {
        for o in plus(sub, *c) {
            want.push(format!("?o={} ?s=<http://example.org/s{s}>", iri(o)));
        }
    }
    want.sort();
    assert_eq!(sorted(engine, "SELECT ?s ?o { ?s a/ex:sub+ ?o }"), want);
    let n = sorted(engine, "SELECT (COUNT(*) AS ?n) { ?s a/ex:sub+ ?o }");
    assert_eq!(
        n,
        [format!(
            "?n=\"{}\"^^<http://www.w3.org/2001/XMLSchema#integer>",
            want.len()
        )]
    );
    // `*` also gives the class itself.
    let mut want = Vec::new();
    for (s, c) in types {
        let mut ends = plus(sub, *c);
        ends.insert(*c);
        for o in ends {
            want.push(format!("?o={} ?s=<http://example.org/s{s}>", iri(o)));
        }
    }
    want.sort();
    assert_eq!(
        sorted(engine, "SELECT ?s ?o { ?s a ?c . ?c ex:sub* ?o }"),
        want
    );
    // Inside EXISTS, with a start that never repeats.
    let mut want: Vec<String> = types
        .iter()
        .filter(|(s, _)| s % 2 == 0)
        .map(|(s, _)| format!("?s=<http://example.org/s{s}>"))
        .collect();
    want.sort();
    assert_eq!(
        sorted(
            engine,
            "SELECT ?s { ?s a ?c FILTER EXISTS { ?s ex:link+ ?o } }"
        ),
        want
    );
    // Backwards: the end of the path is bound by each row, the start is looked for.
    let mut want = Vec::new();
    for (s, c) in types {
        for x in sub.keys().filter(|x| plus(sub, **x).contains(c)) {
            want.push(format!("?s=<http://example.org/s{s}> ?x={}", iri(*x)));
        }
    }
    want.sort();
    assert_eq!(
        sorted(engine, "SELECT ?s ?x { ?s a ?c . ?x ex:sub+ ?c }"),
        want
    );
}
