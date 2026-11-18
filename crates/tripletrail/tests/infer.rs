//! SHACL-AF rule inference on the store.

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::model::{GraphNameRef, NamedNode, NamedNodeRef};
use tripletrail::oxigraph::sparql::QueryResults;

const SHAPES: &str = r#"
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.org/> .
ex:Shape a sh:NodeShape ; sh:targetClass ex:Section ;
  sh:rule [ a sh:SPARQLRule ; sh:order 1 ;
    sh:construct """
      PREFIX ex: <http://example.org/>
      CONSTRUCT { $this ex:speedClass ?c } WHERE {
        $this ex:maxSpeed ?v . BIND(IF(?v >= 160, ex:Fast, ex:Slow) AS ?c) }""" ] ;
  sh:rule [ a sh:TripleRule ; sh:order 2 ;
    sh:subject sh:this ; sh:predicate ex:checked ; sh:object true ] .
# A rule on what the first one inferred: it runs after it.
ex:FastShape a sh:NodeShape ; sh:targetSubjectsOf ex:speedClass ;
  sh:rule [ a sh:SPARQLRule ; sh:construct """
      PREFIX ex: <http://example.org/>
      CONSTRUCT { $this a ex:FastSection } WHERE { $this ex:speedClass ex:Fast }""" ] .
"#;

const DATA: &str = r#"
@prefix ex: <http://example.org/> .
ex:a a ex:Section ; ex:maxSpeed 200 .
ex:b a ex:Section ; ex:maxSpeed 100 ; ex:checked true .
"#;

fn count(engine: &Engine, query: &str) -> usize {
    let QueryResults::Solutions(s) = engine.query(query).unwrap() else {
        panic!()
    };
    s.count()
}

#[test]
fn rules_run_to_a_fixpoint_into_a_graph() {
    let engine = Engine::new_tripletrail();
    let shapes = NamedNode::new("http://example.org/shapes").unwrap();
    let inferred = NamedNodeRef::new("http://example.org/inferred").unwrap();
    engine.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    engine
        .load_into(shapes.clone(), RdfFormat::Turtle, SHAPES.as_bytes())
        .unwrap();
    let triples = engine
        .infer(
            &shapes,
            GraphNameRef::DefaultGraph,
            Some(inferred.into()),
            64,
        )
        .unwrap();
    // a: speedClass Fast, checked, a FastSection; b: speedClass Slow (checked was there).
    assert_eq!(triples.len(), 4);
    assert_eq!(
        count(
            &engine,
            "SELECT * WHERE { GRAPH <http://example.org/inferred> { ?s ?p ?o } }"
        ),
        4
    );
    assert_eq!(
        count(
            &engine,
            "SELECT * WHERE { GRAPH <http://example.org/inferred> { <http://example.org/a> a <http://example.org/FastSection> } }"
        ),
        1
    );
    // One pass is enough: the rule on inferred triples runs after the rule that infers
    // them (it reads ex:speedClass; types are matched per class, so its ex:FastSection
    // does not feed the ex:Section rule back).
    let one = engine
        .infer(&shapes, GraphNameRef::DefaultGraph, None, 1)
        .unwrap();
    assert_eq!(one.len(), 4);
}

/// Rules run on the store itself (`in_store`, #260) infer what they infer through the SHACL
/// engine's own evaluator: recursive rules, semi-naive or not, a sub-select with an
/// aggregate, blank node focus nodes (which a query cannot name, so they take the other
/// path) and existing triples.
#[test]
fn rules_in_the_store_infer_the_same_triples() {
    const SHAPES: &str = r#"
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.org/> .
ex:Node a sh:NodeShape ; sh:targetSubjectsOf ex:link ;
  sh:rule [ a sh:SPARQLRule ; sh:construct """
      PREFIX ex: <http://example.org/>
      CONSTRUCT { $this ex:reach ?y } WHERE { $this ex:link ?y }""" ] ;
  sh:rule [ a sh:SPARQLRule ; sh:construct """
      PREFIX ex: <http://example.org/>
      CONSTRUCT { $this ex:reach ?z } WHERE { $this ex:reach ?y . ?y ex:reach ?z }""" ] ;
  sh:rule [ a sh:SPARQLRule ; sh:construct """
      PREFIX ex: <http://example.org/>
      CONSTRUCT { $this ex:reachCount ?n ; ex:label ?l } WHERE {
        { SELECT $this (COUNT(?z) AS ?n) WHERE { $this ex:reach ?z } GROUP BY $this }
        OPTIONAL { $this ex:reach ?w . ?w ex:seed ?l } }""" ] ;
  sh:rule [ a sh:TripleRule ; sh:subject sh:this ; sh:predicate ex:node ; sh:object true ] .
ex:Zero a sh:NodeShape ;
  sh:target [ a sh:SPARQLTarget ; sh:select """
      PREFIX ex: <http://example.org/>
      SELECT $this WHERE { $this ex:reach ex:n0 }""" ] ;
  sh:rule [ a sh:SPARQLRule ; sh:construct """
      PREFIX ex: <http://example.org/>
      CONSTRUCT { $this ex:toZero ?n } WHERE { $this ex:reachCount ?n }""" ] ;
  sh:rule [ a sh:TripleRule ; sh:subject sh:this ; sh:predicate ex:zero ; sh:object true ] .
"#;
    let mut data = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..15 {
        data += &format!("ex:n{i} ex:link ex:n{} .\n", (i + 1) % 15);
    }
    data += "_:b ex:link ex:n3 . ex:n9 ex:seed \"nine\" . ex:n2 ex:node true .\n";
    let run = |in_store: bool, semi_naive: bool, scratch: bool| {
        let engine = Engine::new_tripletrail();
        let shapes = NamedNode::new("http://example.org/shapes").unwrap();
        // Labels kept, so the runs' blank nodes compare as text (#840).
        let keep = tripletrail::BlankNodes::Keep;
        engine
            .load_with_blank_nodes(RdfFormat::Turtle, data.as_bytes(), keep)
            .unwrap();
        let parser = tripletrail::oxigraph::io::RdfParser::from(RdfFormat::Turtle)
            .with_default_graph(shapes.clone());
        engine
            .load_with_blank_nodes(parser, SHAPES.as_bytes(), keep)
            .unwrap();
        let options = tripletrail::InferOptions {
            include_existing: true,
            semi_naive,
            in_store,
            scratch,
            ..tripletrail::InferOptions::default()
        };
        let before = engine.store().len().unwrap();
        let into = NamedNode::new("http://example.org/into").unwrap();
        let g = engine
            .infer_with(
                &shapes,
                GraphNameRef::DefaultGraph,
                scratch.then(|| into.as_ref().into()),
                &options,
                &|_| {},
            )
            .unwrap();
        if scratch {
            // Only `into` is written.
            assert_eq!(engine.store().len().unwrap(), before + g.len());
        }
        let mut v: Vec<String> = g.iter().map(|t| t.to_string()).collect();
        v.sort();
        v
    };
    let reference = run(false, true, false);
    assert!(reference.len() > 200, "{}", reference.len());
    assert!(reference.iter().any(|t| t.starts_with("_:")));
    // The SPARQL target reads inferred triples, and finds the blank node too.
    assert!(
        reference
            .iter()
            .any(|t| t.starts_with("_:") && t.contains("example.org/zero")),
        "{reference:?}"
    );
    assert_eq!(run(true, true, false), reference);
    assert_eq!(run(true, false, false), reference);
    assert_eq!(run(false, true, true), reference);
    // With provenance (#455): the same triples, in the store, semi-naive or not, each with its
    // focus node, and the solution of a SPARQL rule binding `$this` to it.
    let traced = |in_store: bool, semi_naive: bool, scratch: bool| {
        let engine = Engine::new_tripletrail();
        let shapes = NamedNode::new("http://example.org/shapes").unwrap();
        // Labels kept, so the runs' blank nodes compare as text (#840).
        let keep = tripletrail::BlankNodes::Keep;
        engine
            .load_with_blank_nodes(RdfFormat::Turtle, data.as_bytes(), keep)
            .unwrap();
        let parser = tripletrail::oxigraph::io::RdfParser::from(RdfFormat::Turtle)
            .with_default_graph(shapes.clone());
        engine
            .load_with_blank_nodes(parser, SHAPES.as_bytes(), keep)
            .unwrap();
        let options = tripletrail::InferOptions {
            include_existing: true,
            semi_naive,
            in_store,
            scratch,
            ..tripletrail::InferOptions::default()
        };
        let (g, origins) = engine
            .infer_traced(&shapes, GraphNameRef::DefaultGraph, &options, &|_| {})
            .unwrap();
        assert_eq!(g.len(), origins.len());
        let mut solutions = 0;
        for o in &origins {
            let focus = o.focus.as_ref().expect("a focus node");
            if let Some(s) = &o.solution {
                assert_eq!(s.get("this"), Some(focus));
                solutions += 1;
            }
        }
        assert!(solutions > 200, "{solutions}");
        let mut v: Vec<String> = g.iter().map(|t| t.to_string()).collect();
        v.sort();
        v
    };
    for (in_store, semi_naive, scratch) in [
        (false, false, false),
        (false, true, false),
        (true, true, false),
        (true, false, false),
        (false, true, true),
    ] {
        assert_eq!(
            traced(in_store, semi_naive, scratch),
            reference,
            "in_store {in_store}, semi_naive {semi_naive}, scratch {scratch}"
        );
    }
}

/// A rule whose `$this` is only in a sub-select that does not return it makes the same
/// triples for every focus node: in order, it runs once, not once per batch of focus nodes
/// (#520), and infers what it infers with provenance, which still runs it per focus node.
#[test]
fn rules_not_reading_the_focus_node_run_once() {
    const SHAPES: &str = r#"
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.org/> .
ex:Shape a sh:NodeShape ; sh:targetSubjectsOf ex:km ;
  sh:rule [ a sh:SPARQLRule ; sh:order 1 ; sh:construct """
      PREFIX ex: <http://example.org/>
      CONSTRUCT { ?group ex:first ?first } WHERE {
        { SELECT ?group (MIN(STR(?entity)) AS ?first) WHERE {
            ?entity ex:in ?group .
            BIND(IRI(CONCAT(STR(?entity), "/item")) AS $this)
            $this ex:km ?km } GROUP BY ?group } }""" ] .
"#;
    let mut data = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..9000 {
        data += &format!(
            "<http://example.org/e{i:05}> ex:in ex:g{} .\n<http://example.org/e{i:05}/item> ex:km {i} .\n",
            i % 3
        );
    }
    let engine = Engine::new_tripletrail();
    let shapes = NamedNode::new("http://example.org/shapes").unwrap();
    engine.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    engine
        .load_into(shapes.clone(), RdfFormat::Turtle, SHAPES.as_bytes())
        .unwrap();
    let options = tripletrail::InferOptions {
        max_passes: 1,
        include_existing: true,
        scratch: true,
        sequential: true,
        ..tripletrail::InferOptions::default()
    };
    let focus_nodes = std::cell::Cell::new(0);
    let observer = |r: &tripletrail::shacl::RuleRun<'_>| {
        if r.done {
            assert_eq!(r.focus_nodes, 9000);
        }
    };
    let g = engine
        .infer_with(
            &shapes,
            GraphNameRef::DefaultGraph,
            None,
            &options,
            &observer,
        )
        .unwrap();
    let mut v: Vec<String> = g.iter().map(|t| t.to_string()).collect();
    v.sort();
    assert_eq!(
        v,
        [
            "<http://example.org/g0> <http://example.org/first> \"http://example.org/e00000\"",
            "<http://example.org/g1> <http://example.org/first> \"http://example.org/e00001\"",
            "<http://example.org/g2> <http://example.org/first> \"http://example.org/e00002\"",
        ]
    );
    let (traced, _) = engine
        .infer_traced(&shapes, GraphNameRef::DefaultGraph, &options, &|r| {
            if r.done {
                focus_nodes.set(r.focus_nodes);
            }
        })
        .unwrap();
    assert_eq!(focus_nodes.get(), 9000);
    let mut t: Vec<String> = traced.iter().map(|t| t.to_string()).collect();
    t.sort();
    t.dedup();
    assert_eq!(t, v);
}

/// Provenance against plain inference, in the store (#455): run with
/// `cargo test --release -p tripletrail --test infer provenance_overhead -- --ignored --nocapture`.
#[test]
#[ignore]
fn provenance_overhead() {
    const SHAPES: &str = r#"
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.org/> .
ex:Section a sh:NodeShape ; sh:targetClass ex:Section ;
  sh:rule [ a sh:SPARQLRule ; sh:construct """
      PREFIX ex: <http://example.org/>
      CONSTRUCT { $this ex:speedClass ?c } WHERE {
        $this ex:maxSpeed ?v . BIND(IF(?v >= 160, ex:Fast, ex:Slow) AS ?c) }""" ] ;
  sh:rule [ a sh:SPARQLRule ; sh:construct """
      PREFIX ex: <http://example.org/>
      CONSTRUCT { $this ex:after ?y } WHERE { $this ex:next ?y }""" ] ;
  sh:rule [ a sh:SPARQLRule ; sh:construct """
      PREFIX ex: <http://example.org/>
      CONSTRUCT { $this ex:after ?z } WHERE { $this ex:after ?y . ?y ex:next ?z }""" ] ;
  sh:rule [ a sh:TripleRule ; sh:subject sh:this ; sh:predicate ex:checked ; sh:object true ] .
"#;
    // 400 lines of 25 sections: 200,000 `after` triples at the fixpoint.
    let mut data = String::from("@prefix ex: <http://example.org/> .\n");
    for line in 0..400 {
        for i in 0..25 {
            data += &format!(
                "ex:s{line}_{i} a ex:Section ; ex:maxSpeed {} .\n",
                80 + (i * 7) % 120
            );
            if i < 24 {
                data += &format!("ex:s{line}_{i} ex:next ex:s{line}_{} .\n", i + 1);
            }
        }
    }
    let engine = || {
        let engine = Engine::new_tripletrail();
        engine.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
        engine
            .load_into(
                NamedNode::new("http://example.org/shapes").unwrap(),
                RdfFormat::Turtle,
                SHAPES.as_bytes(),
            )
            .unwrap();
        engine
    };
    let shapes = NamedNode::new("http://example.org/shapes").unwrap();
    for (name, in_store, semi_naive) in [
        ("in store, semi-naive", true, true),
        ("evaluator, semi-naive", false, true),
        ("in store, naive", true, false),
    ] {
        let options = tripletrail::InferOptions {
            in_store,
            semi_naive,
            ..tripletrail::InferOptions::default()
        };
        let e = engine();
        let started = std::time::Instant::now();
        let plain = e
            .infer_with(&shapes, GraphNameRef::DefaultGraph, None, &options, &|_| {})
            .unwrap();
        let plain_time = started.elapsed();
        let e = engine();
        let started = std::time::Instant::now();
        let (traced, origins) = e
            .infer_traced(&shapes, GraphNameRef::DefaultGraph, &options, &|_| {})
            .unwrap();
        let traced_time = started.elapsed();
        assert_eq!(plain.len(), traced.len());
        assert_eq!(traced.len(), origins.len());
        println!(
            "{name}: {} triples, plain {plain_time:.2?}, provenance {traced_time:.2?} ({:.2}x)",
            plain.len(),
            traced_time.as_secs_f64() / plain_time.as_secs_f64()
        );
    }
}

/// On a union default graph, the rules read every graph but the shapes graph, in a fork of
/// the store (#520): the store itself gets only `into`.
#[test]
fn rules_on_a_union_default_graph_leave_the_shapes_out() {
    const SHAPES: &str = r#"
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.org/> .
ex:S a sh:NodeShape ; sh:targetSubjectsOf ex:p ;
  sh:rule [ a sh:SPARQLRule ; sh:order 1 ; sh:construct """
      PREFIX ex: <http://example.org/>
      CONSTRUCT { ?x ex:typed true } WHERE { ?x a ?c }""" ] ;
  sh:rule [ a sh:SPARQLRule ; sh:order 2 ; sh:construct """
      PREFIX ex: <http://example.org/>
      CONSTRUCT { $this ex:again ?t } WHERE { $this ex:typed ?t }""" ] .
"#;
    let engine = Engine::new_tripletrail();
    engine
        .load_into(
            NamedNode::new("http://example.org/g1").unwrap(),
            RdfFormat::Turtle,
            "<http://example.org/a> a <http://example.org/T> ; <http://example.org/p> 1 ."
                .as_bytes(),
        )
        .unwrap();
    engine
        .load(
            RdfFormat::Turtle,
            "<http://example.org/b> a <http://example.org/T> ; <http://example.org/p> 2 ."
                .as_bytes(),
        )
        .unwrap();
    let shapes = NamedNode::new("http://example.org/shapes").unwrap();
    engine
        .load_into(shapes.clone(), RdfFormat::Turtle, SHAPES.as_bytes())
        .unwrap();
    engine.set_union_default_graph(true);
    let before = engine.store().len().unwrap();
    let into = NamedNode::new("http://example.org/into").unwrap();
    let options = tripletrail::InferOptions {
        max_passes: 1,
        scratch: true,
        sequential: true,
        ..tripletrail::InferOptions::default()
    };
    let g = engine
        .infer_with(
            &shapes,
            GraphNameRef::DefaultGraph,
            Some(into.as_ref().into()),
            &options,
            &|_| {},
        )
        .unwrap();
    let mut v: Vec<String> = g.iter().map(|t| t.to_string()).collect();
    v.sort();
    assert_eq!(
        v,
        [
            "<http://example.org/a> <http://example.org/again> \"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>",
            "<http://example.org/a> <http://example.org/typed> \"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>",
            "<http://example.org/b> <http://example.org/again> \"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>",
            "<http://example.org/b> <http://example.org/typed> \"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>",
        ]
    );
    assert_eq!(engine.store().len().unwrap(), before + 4);
}

/// Where an ordered infer step on a union default graph spends its time (#520): run with
/// `TRIPLETRAIL_INFER_DATA=kg.ttl cargo test --release -p tripletrail --test infer scratch_cost -- --ignored --nocapture`.
#[test]
#[ignore]
fn scratch_cost() {
    let Ok(path) = std::env::var("TRIPLETRAIL_INFER_DATA") else {
        return;
    };
    let engine = Engine::new_tripletrail();
    let g = NamedNode::new("http://example.org/data").unwrap();
    let t = std::time::Instant::now();
    engine
        .load_into(
            g.clone(),
            RdfFormat::Turtle,
            std::io::BufReader::new(std::fs::File::open(path).unwrap()),
        )
        .unwrap();
    eprintln!(
        "load {:.1} s, {} quads",
        t.elapsed().as_secs_f64(),
        engine.store().len().unwrap()
    );
    engine.set_union_default_graph(true);
    let shapes = NamedNode::new("http://example.org/shapes").unwrap();
    engine
        .load_into(
            shapes.clone(),
            RdfFormat::Turtle,
            r#"@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.org/> .
ex:S a sh:NodeShape ; sh:targetNode ex:none ;
  sh:rule [ a sh:SPARQLRule ; sh:order 1 ; sh:construct "CONSTRUCT { $this <http://example.org/p> 1 } WHERE { }" ] ."#
                .as_bytes(),
        )
        .unwrap();
    let into = NamedNode::new("http://example.org/into").unwrap();
    let options = tripletrail::InferOptions {
        max_passes: 1,
        include_existing: true,
        scratch: true,
        sequential: true,
        ..tripletrail::InferOptions::default()
    };
    let t = std::time::Instant::now();
    let started = std::cell::Cell::new(None);
    engine
        .infer_with(
            &shapes,
            GraphNameRef::DefaultGraph,
            Some(into.as_ref().into()),
            &options,
            &|r| {
                if !r.done && started.get().is_none() {
                    started.set(Some(t.elapsed()));
                }
            },
        )
        .unwrap();
    eprintln!(
        "infer {:.1} s, first rule at {:.1} s",
        t.elapsed().as_secs_f64(),
        started.get().unwrap_or_default().as_secs_f64()
    );
}
