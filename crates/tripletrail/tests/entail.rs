//! RDFS entailment (#187).

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::model::NamedNodeRef;
use tripletrail::oxigraph::sparql::QueryResults;

const INTO: NamedNodeRef<'_> = NamedNodeRef::new_unchecked("urn:tripletrail:inferred");

const DATA: &str = r#"
@prefix ex: <http://example.com/> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
ex:Station rdfs:subClassOf ex:OperationalPoint .
ex:OperationalPoint rdfs:subClassOf ex:Location .
ex:A rdfs:subClassOf ex:B . ex:B rdfs:subClassOf ex:A .
ex:mainTrack rdfs:subPropertyOf ex:track .
ex:track rdfs:subPropertyOf ex:related ; rdfs:domain ex:Location ; rdfs:range ex:Track .
ex:name rdfs:range rdfs:Literal .
ex:bxl a ex:Station ; ex:mainTrack ex:t1 ; ex:name "Brussel" .
ex:x a ex:A .
ex:alreadyTyped a ex:Station, ex:Location .
GRAPH ex:g { ex:gent ex:track "not a resource" }
"#;

fn engine() -> Engine {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::TriG, DATA.as_bytes()).unwrap();
    e
}

/// The inferred graph, as sorted N-Triples-ish lines with `ex:` shortened.
fn inferred(e: &Engine) -> Vec<String> {
    let QueryResults::Solutions(s) = e
        .query("SELECT ?s ?p ?o WHERE { GRAPH <urn:tripletrail:inferred> { ?s ?p ?o } }")
        .unwrap()
    else {
        panic!()
    };
    let short = |t: String| {
        t.replace("http://example.com/", "")
            .replace("http://www.w3.org/2000/01/rdf-schema#", "rdfs:")
            .replace("http://www.w3.org/1999/02/22-rdf-syntax-ns#", "rdf:")
            .replace("http://www.w3.org/2002/07/owl#", "owl:")
            .replace(['<', '>'], "")
    };
    let mut out: Vec<String> = s
        .map(|r| {
            let r = r.unwrap();
            ["s", "p", "o"]
                .map(|v| short(r.get(v).unwrap().to_string()))
                .join(" ")
        })
        .collect();
    out.sort();
    out
}

#[test]
fn the_rdfs_rules_derive_what_jena_default_rdfs_does() {
    let e = engine();
    let n = e.entail_rdfs(INTO).unwrap();
    let got = inferred(&e);
    assert_eq!((n.triples, n.added, n.removed), (got.len(), got.len(), 0));
    let expected = [
        // rdfs11, and on its cycle each class is its own subclass (the asserted triples
        // are not repeated).
        "A rdfs:subClassOf A",
        "B rdfs:subClassOf B",
        "Station rdfs:subClassOf Location",
        // rdfs5.
        "mainTrack rdfs:subPropertyOf related",
        // rdfs7 through two levels.
        "bxl related t1",
        "bxl track t1",
        // rdfs9.
        "bxl rdf:type Location",
        "bxl rdf:type OperationalPoint",
        "alreadyTyped rdf:type OperationalPoint",
        "x rdf:type B",
        // rdfs2 and rdfs3 through the super property; no type for a literal, in any graph.
        "gent rdf:type Location",
        "gent related \"not a resource\"",
        "t1 rdf:type Track",
    ];
    let mut expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(got, expected);
}

#[test]
fn running_again_follows_the_data() {
    let e = engine();
    e.entail_rdfs(INTO).unwrap();
    e.update(
        r#"PREFIX ex: <http://example.com/>
           DELETE DATA { ex:bxl ex:mainTrack ex:t1 } ;
           INSERT DATA { ex:ant a ex:Station }"#,
    )
    .unwrap();
    // Unchanged until run again.
    assert!(inferred(&e).contains(&"t1 rdf:type Track".to_owned()));
    let done = e.entail_rdfs(INTO).unwrap();
    // bxl track/related t1, t1 a Track out; ant a OperationalPoint/Location in.
    assert_eq!((done.added, done.removed), (2, 3));
    let got = inferred(&e);
    assert!(!got.iter().any(|t| t.contains("t1")), "{got:?}");
    assert!(got.contains(&"ant rdf:type Location".to_owned()));
    // The inferred graph is not read as data: running twice changes nothing.
    let before = got.clone();
    let done = e.entail_rdfs(INTO).unwrap();
    assert_eq!((done.added, done.removed), (0, 0));
    assert_eq!(inferred(&e), before);
    // And its triples answer queries like any graph's.
    let QueryResults::Boolean(b) = e
        .query("ASK { GRAPH ?g { <http://example.com/ant> a <http://example.com/Location> } }")
        .unwrap()
    else {
        panic!()
    };
    assert!(b);
}

#[test]
fn a_store_without_types_still_closes_its_hierarchies() {
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        r#"@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
           <urn:a> rdfs:subClassOf <urn:b> . <urn:b> rdfs:subClassOf <urn:c> ."#
            .as_bytes(),
    )
    .unwrap();
    assert_eq!(e.entail_rdfs(INTO).unwrap().triples, 1);
}

#[test]
fn derived_triples_feed_the_rules_again_until_nothing_new_comes() {
    // The axioms of rdfs.ttl that read other derived triples: a derived rdf:type triple has
    // a domain, a derived domain triple too, and a sub property of rdfs:subClassOf makes
    // derived schema.
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        r#"@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
           @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
           @prefix ex: <http://example.com/> .
           rdf:type rdfs:domain rdfs:Resource ; rdfs:range rdfs:Class .
           rdfs:domain rdfs:domain rdf:Property ; rdfs:range rdfs:Class .
           rdfs:subClassOf rdfs:domain rdfs:Class ; rdfs:range rdfs:Class .
           ex:narrower rdfs:subPropertyOf rdfs:subClassOf .
           ex:Station ex:narrower ex:Place .
           ex:Place rdfs:subClassOf ex:Thing .
           ex:name rdfs:domain ex:Named .
           ex:Named rdfs:subClassOf ex:Thing .
           ex:hasName rdfs:subPropertyOf ex:name .
           ex:bxl a ex:Station ; ex:hasName "Brussel" ."#
            .as_bytes(),
    )
    .unwrap();
    let done = e.entail_rdfs(INTO).unwrap();
    let got = inferred(&e);
    assert_eq!(done.triples, got.len());
    let expected = [
        // rdfs7: the sub property of rdfs:subClassOf makes schema, which then works.
        "Station rdfs:subClassOf Place",
        "Station rdfs:subClassOf Thing",
        "bxl rdf:type Place",
        "bxl rdf:type Thing",
        "bxl name \"Brussel\"",
        "bxl rdf:type Named",
        // Every rdf:type triple, asserted or derived, has a resource subject and a class
        // object; every subClassOf triple, asserted or derived, two classes.
        "bxl rdf:type rdfs:Resource",
        "Station rdf:type rdfs:Class",
        "Place rdf:type rdfs:Class",
        "Thing rdf:type rdfs:Class",
        "Named rdf:type rdfs:Class",
        "rdfs:Class rdf:type rdfs:Class",
        "rdfs:Resource rdf:type rdfs:Class",
        "rdf:Property rdf:type rdfs:Class",
        // Typed by the derived types themselves.
        "Station rdf:type rdfs:Resource",
        "Place rdf:type rdfs:Resource",
        "Thing rdf:type rdfs:Resource",
        "Named rdf:type rdfs:Resource",
        "rdfs:Class rdf:type rdfs:Resource",
        "rdfs:Resource rdf:type rdfs:Resource",
        "rdf:Property rdf:type rdfs:Resource",
        // The domain triples' subjects are properties.
        "rdf:type rdf:type rdf:Property",
        "rdfs:domain rdf:type rdf:Property",
        "rdfs:subClassOf rdf:type rdf:Property",
        "name rdf:type rdf:Property",
        "rdf:type rdf:type rdfs:Resource",
        "rdfs:domain rdf:type rdfs:Resource",
        "rdfs:subClassOf rdf:type rdfs:Resource",
        "name rdf:type rdfs:Resource",
    ];
    let mut expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(got, expected);
}

/// What entailing from scratch gives for the data of `e` (every graph but the inferred one).
fn from_scratch(e: &Engine) -> Vec<String> {
    let fresh = Engine::new_tripletrail();
    let quads: Vec<_> = e
        .store()
        .quads_for_pattern(None, None, None, None)
        .map(Result::unwrap)
        .filter(|q| q.graph_name != INTO.into())
        .collect();
    fresh.load_quads(quads).unwrap();
    fresh.entail_rdfs(INTO).unwrap();
    inferred(&fresh)
}

#[test]
fn a_kept_entailment_follows_every_write() {
    let e = engine();
    let first = e
        .set_rdfs_entailment(Some(INTO.into_owned()))
        .unwrap()
        .unwrap();
    assert_eq!(first.triples, inferred(&e).len());
    let ex = |s: &str| {
        format!(
            "PREFIX ex: <http://example.com/> PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#> {s}"
        )
    };
    let writes = [
        // Added data only: derived from the added triples.
        "INSERT DATA { ex:ant a ex:Station ; ex:mainTrack ex:t2 }",
        "INSERT DATA { GRAPH ex:g2 { ex:lux ex:track ex:t3 } }",
        // Asserted now what was derived: it leaves the inferred graph.
        "INSERT DATA { ex:ant a ex:Location }",
        // A removal: entailed again.
        "DELETE DATA { ex:bxl ex:mainTrack ex:t1 }",
        // Schema: entailed again.
        "INSERT DATA { ex:Location rdfs:subClassOf ex:Thing }",
        "DELETE DATA { ex:track rdfs:range ex:Track }",
        // Into the inferred graph itself: not kept.
        "INSERT DATA { GRAPH <urn:tripletrail:inferred> { ex:junk a ex:Junk } } ; INSERT DATA { ex:gent a ex:A }",
        "DELETE WHERE { ex:ant ?p ?o }",
    ];
    for w in writes {
        e.update(&ex(w)).unwrap();
        assert_eq!(inferred(&e), from_scratch(&e), "after {w}");
    }
    // Single quads and bulk loads too.
    use tripletrail::oxigraph::model::{GraphName, NamedNode, Quad};
    let n = |s: &str| NamedNode::new(format!("http://example.com/{s}")).unwrap();
    let quad = Quad::new(
        n("brg"),
        NamedNode::new("http://www.w3.org/1999/02/22-rdf-syntax-ns#type").unwrap(),
        n("Station"),
        GraphName::DefaultGraph,
    );
    e.insert(&quad).unwrap();
    assert!(inferred(&e).contains(&"brg rdf:type Location".to_owned()));
    assert_eq!(inferred(&e), from_scratch(&e));
    e.remove(&quad).unwrap();
    assert_eq!(inferred(&e), from_scratch(&e));
    e.load(
        RdfFormat::Turtle,
        "<http://example.com/hsl> a <http://example.com/Station> .".as_bytes(),
    )
    .unwrap();
    assert!(inferred(&e).contains(&"hsl rdf:type Thing".to_owned()));
    assert_eq!(inferred(&e), from_scratch(&e));
    // Stopped: writes leave the graph as it is.
    assert_eq!(e.set_rdfs_entailment(None).unwrap(), None);
    let before = inferred(&e);
    e.update(&ex("INSERT DATA { ex:new a ex:Station }"))
        .unwrap();
    assert_eq!(inferred(&e), before);
}

#[test]
fn a_kept_entailment_matches_entailing_from_scratch_after_random_writes() {
    // A small vocabulary, so that writes often meet: classes, properties, the RDFS terms
    // (also as subjects, as rdfs.ttl writes them), resources and a literal.
    let terms = [
        "<http://e/C0>",
        "<http://e/C1>",
        "<http://e/C2>",
        "<http://e/p0>",
        "<http://e/p1>",
        "<http://e/i0>",
        "<http://e/i1>",
        "<http://e/i2>",
        "<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>",
        "<http://www.w3.org/2000/01/rdf-schema#subClassOf>",
        "<http://www.w3.org/2000/01/rdf-schema#subPropertyOf>",
        "<http://www.w3.org/2000/01/rdf-schema#domain>",
        "<http://www.w3.org/2000/01/rdf-schema#range>",
    ];
    let predicates = [3, 4, 8, 8, 8, 9, 10, 11, 12];
    let graphs = ["", "<http://e/g>"];
    let mut seed: u64 = 0x5eed;
    let mut next = |n: usize| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((seed >> 33) as usize) % n
    };
    for round in 0..8 {
        let e = Engine::new_tripletrail();
        e.set_rdfs_entailment(Some(INTO.into_owned())).unwrap();
        for step in 0..40 {
            let mut quads = String::new();
            for _ in 0..1 + next(3) {
                let s = terms[next(terms.len())];
                let p = terms[predicates[next(predicates.len())]];
                let o = if next(8) == 0 {
                    "\"lit\""
                } else {
                    terms[next(terms.len())]
                };
                let g = graphs[next(graphs.len())];
                quads += &if g.is_empty() {
                    format!("{s} {p} {o} . ")
                } else {
                    format!("GRAPH {g} {{ {s} {p} {o} }} ")
                };
            }
            // Removals of data the store holds, alone or with the insert in one update.
            let asserted: Vec<String> = e
                .store()
                .quads_for_pattern(None, None, None, None)
                .map(Result::unwrap)
                .filter(|q| q.graph_name != INTO.into())
                .map(|q| {
                    let triple = format!("{} {} {}", q.subject, q.predicate, q.object);
                    if q.graph_name.is_default_graph() {
                        format!("{triple} .")
                    } else {
                        format!("GRAPH {} {{ {triple} }}", q.graph_name)
                    }
                })
                .collect();
            let mut gone = String::new();
            if !asserted.is_empty() {
                for _ in 0..next(3) {
                    gone += &format!("{} ", asserted[next(asserted.len())]);
                }
            }
            let update = match next(3) {
                0 if !gone.is_empty() => format!("DELETE DATA {{ {gone} }}"),
                1 if !gone.is_empty() => {
                    format!("DELETE DATA {{ {gone} }} ; INSERT DATA {{ {quads} }}")
                }
                _ => format!("INSERT DATA {{ {quads} }}"),
            };
            e.update(&update).unwrap();
            assert_eq!(
                inferred(&e),
                from_scratch(&e),
                "round {round}, step {step}: {update}"
            );
        }
    }
}

#[test]
fn owl_rl_adds_equivalence_inverse_symmetric_and_transitive_properties() {
    use tripletrail::store::entail::Regime;
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        r#"@prefix ex: <http://example.com/> .
           @prefix owl: <http://www.w3.org/2002/07/owl#> .
           @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
           ex:Station owl:equivalentClass ex:Halt .
           ex:partOf a owl:TransitiveProperty ; owl:inverseOf ex:hasPart .
           ex:hasPart rdfs:domain ex:Whole .
           ex:adjacent a owl:SymmetricProperty .
           ex:locatedIn owl:equivalentProperty ex:in .
           ex:bxl a ex:Halt ; ex:locatedIn ex:be .
           ex:p1 ex:partOf ex:bxl . ex:bxl ex:partOf ex:be .
           ex:p1 ex:adjacent ex:p2 .
           ex:p2 ex:adjacent "not a resource" ."#
            .as_bytes(),
    )
    .unwrap();
    // RDFS alone reads none of it but the domain, which no hasPart triple uses yet.
    assert_eq!(e.entail(INTO, Regime::Rdfs).unwrap().triples, 0);
    let done = e.entail(INTO, Regime::OwlRl).unwrap();
    let got = inferred(&e);
    assert_eq!(done.triples, got.len());
    let expected = [
        // scm-eqc1/2, scm-eqp1/2 (and on the cycle each is its own sub class/property).
        "Station rdfs:subClassOf Halt",
        "Halt rdfs:subClassOf Station",
        "Station rdfs:subClassOf Station",
        "Halt rdfs:subClassOf Halt",
        "locatedIn rdfs:subPropertyOf in",
        "in rdfs:subPropertyOf locatedIn",
        "locatedIn rdfs:subPropertyOf locatedIn",
        "in rdfs:subPropertyOf in",
        // scm-eqc2, scm-eqp2: sub classes (properties) of each other, each of itself too.
        "Halt owl:equivalentClass Station",
        "Station owl:equivalentClass Station",
        "Halt owl:equivalentClass Halt",
        "in owl:equivalentProperty locatedIn",
        "locatedIn owl:equivalentProperty locatedIn",
        "in owl:equivalentProperty in",
        // cax-eqc, prp-eqp.
        "bxl rdf:type Station",
        "bxl in be",
        // prp-trp, then prp-inv for each partOf triple, then the domain of hasPart.
        "p1 partOf be",
        "bxl hasPart p1",
        "be hasPart bxl",
        "be hasPart p1",
        "bxl rdf:type Whole",
        "be rdf:type Whole",
        // prp-symp; a literal is no subject.
        "p2 adjacent p1",
    ];
    let mut expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(got, expected);
}

#[test]
fn owl_rl_is_refused_into_the_graph_that_keeps_rdfs() {
    use tripletrail::store::entail::Regime;
    let e = engine();
    e.set_rdfs_entailment(Some(INTO.into_owned())).unwrap();
    let before = inferred(&e);
    assert!(e.entail(INTO, Regime::OwlRl).is_err());
    assert_eq!(inferred(&e), before);
    // Another graph is fine.
    let other = NamedNodeRef::new_unchecked("urn:tripletrail:owl");
    assert!(e.entail(other, Regime::OwlRl).is_ok());
}

#[test]
fn owl_rl_adds_restrictions_intersections_unions_and_property_chains() {
    use tripletrail::store::entail::Regime;
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        r#"@prefix ex: <http://example.com/> .
           @prefix owl: <http://www.w3.org/2002/07/owl#> .
           ex:Electrified owl:onProperty ex:power ; owl:hasValue ex:ac25 .
           ex:WithSignal owl:onProperty ex:has ; owl:someValuesFrom ex:Signal .
           ex:Linked owl:onProperty ex:link ; owl:someValuesFrom owl:Thing .
           ex:Line owl:onProperty ex:has ; owl:allValuesFrom ex:Part .
           ex:ElectrifiedStation owl:intersectionOf ( ex:Station ex:Electrified ) .
           ex:Node owl:unionOf ( ex:Station ex:Switch ) .
           ex:grandPart owl:propertyChainAxiom ( ex:has ex:has ) .
           ex:t1 ex:power ex:ac25 .
           ex:e1 a ex:Electrified .
           ex:s1 a ex:Station ; ex:power ex:ac25 ; ex:has ex:sig .
           ex:sig a ex:Signal .
           ex:l1 a ex:Line ; ex:has ex:s1 .
           ex:w1 ex:link "a literal" ."#
            .as_bytes(),
    )
    .unwrap();
    assert_eq!(e.entail(INTO, Regime::Rdfs).unwrap().triples, 0);
    let done = e.entail(INTO, Regime::OwlRl).unwrap();
    let got = inferred(&e);
    assert_eq!(done.triples, got.len());
    let expected = [
        // scm-int, scm-uni, then rdfs11.
        "ElectrifiedStation rdfs:subClassOf Station",
        "ElectrifiedStation rdfs:subClassOf Electrified",
        "ElectrifiedStation rdfs:subClassOf Node",
        "Station rdfs:subClassOf Node",
        "Switch rdfs:subClassOf Node",
        // cls-hv2, cls-hv1.
        "t1 rdf:type Electrified",
        "s1 rdf:type Electrified",
        "e1 power ac25",
        // cls-int1, then cls-uni through the sub class.
        "s1 rdf:type ElectrifiedStation",
        "s1 rdf:type Node",
        // cls-avf, cls-svf1, cls-svf2 (a literal value counts).
        "s1 rdf:type Part",
        "s1 rdf:type WithSignal",
        "w1 rdf:type Linked",
        // prp-spo2.
        "l1 grandPart sig",
    ];
    let mut expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(got, expected);
}

#[test]
fn owl_rl_adds_same_as_and_functional_properties() {
    use tripletrail::store::entail::Regime;
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        r#"@prefix ex: <http://example.com/> .
           @prefix owl: <http://www.w3.org/2002/07/owl#> .
           ex:code a owl:InverseFunctionalProperty .
           ex:operator a owl:FunctionalProperty .
           ex:a ex:code "BXL" . ex:b ex:code "BXL" .
           ex:a ex:operator ex:x . ex:a ex:operator ex:y .
           ex:b ex:name "Brussels" .
           ex:z owl:sameAs "not a resource" ."#
            .as_bytes(),
    )
    .unwrap();
    let done = e.entail(INTO, Regime::OwlRl).unwrap();
    let got = inferred(&e);
    assert_eq!(done.triples, got.len());
    let expected = [
        // prp-ifp, prp-fp, then eq-sym and eq-trans (each member the same as itself).
        "a owl:sameAs b",
        "b owl:sameAs a",
        "a owl:sameAs a",
        "b owl:sameAs b",
        "x owl:sameAs y",
        "y owl:sameAs x",
        "x owl:sameAs x",
        "y owl:sameAs y",
        // eq-rep-s and eq-rep-o.
        "a name \"Brussels\"",
        "b operator x",
        "b operator y",
    ];
    let mut expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    expected.sort();
    let got: Vec<String> = got
        .into_iter()
        .map(|t| t.replace("http://www.w3.org/2002/07/owl#", "owl:"))
        .collect();
    let mut got = got;
    got.sort();
    assert_eq!(got, expected);
}

#[test]
fn owl_rl_adds_max_cardinality_keys_and_enumerations() {
    use tripletrail::store::entail::Regime;
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        r#"@prefix ex: <http://example.com/> .
           @prefix owl: <http://www.w3.org/2002/07/owl#> .
           @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
           # cls-maxc2: only for members of the restriction.
           ex:OneMother owl:onProperty ex:mother ; owl:maxCardinality "1"^^xsd:nonNegativeInteger .
           ex:a a ex:OneMother ; ex:mother ex:m1 , ex:m2 .
           ex:z ex:mother ex:m3 , ex:m4 .
           # cls-maxqc3: only values of the class.
           ex:OneBoy owl:onProperty ex:child ;
               owl:maxQualifiedCardinality "1"^^xsd:nonNegativeInteger ; owl:onClass ex:Boy .
           ex:b a ex:OneBoy ; ex:child ex:c1 , ex:c2 , ex:c3 .
           ex:c1 a ex:Boy . ex:c2 a ex:Boy .
           # cls-maxqc4: owl:Thing.
           ex:OneBoss owl:onProperty ex:boss ; owl:maxQualifiedCardinality 1 ; owl:onClass owl:Thing .
           ex:d a ex:OneBoss ; ex:boss ex:e1 , ex:e2 .
           # A maximum of 2 derives nothing.
           ex:TwoPets owl:onProperty ex:pet ; owl:maxCardinality 2 .
           ex:f a ex:TwoPets ; ex:pet ex:p1 , ex:p2 .
           # prp-key: a value in common for every key property, members of the class only.
           ex:Person owl:hasKey ( ex:ssn ex:country ) .
           ex:x a ex:Person ; ex:ssn "1" ; ex:country ex:be .
           ex:y a ex:Person ; ex:ssn "1" , "2" ; ex:country ex:be .
           ex:w a ex:Person ; ex:ssn "1" ; ex:country ex:nl .
           ex:v ex:ssn "1" ; ex:country ex:be .
           # cls-oo.
           ex:Colour owl:oneOf ( ex:red ex:green ) ."#
            .as_bytes(),
    )
    .unwrap();
    let done = e.entail(INTO, Regime::OwlRl).unwrap();
    let got = inferred(&e);
    assert_eq!(done.triples, got.len());
    let mut expected: Vec<String> = Vec::new();
    for (a, b) in [("m1", "m2"), ("c1", "c2"), ("e1", "e2"), ("x", "y")] {
        for (s, o) in [(a, b), (b, a), (a, a), (b, b)] {
            expected.push(format!("{s} owl:sameAs {o}"));
        }
    }
    // eq-rep-s: x gets the value only y had (the others are asserted already); cls-oo.
    expected.extend(
        [
            "x ssn \"2\"",
            "red rdf:type Colour",
            "green rdf:type Colour",
        ]
        .map(String::from),
    );
    expected.sort();
    let mut got: Vec<String> = got
        .into_iter()
        .map(|t| {
            t.replace("http://www.w3.org/2002/07/owl#", "owl:")
                .replace("http://www.w3.org/1999/02/22-rdf-syntax-ns#", "rdf:")
        })
        .collect();
    got.sort();
    assert_eq!(got, expected);
}

#[test]
fn owl_rl_keys_with_many_values_take_time_in_the_data_not_its_combinations() {
    use std::fmt::Write;
    use tripletrail::store::entail::Regime;
    // A key of 5 properties, members with 30 values of each: 30^5 combinations a member.
    // m0 and m1 share one value of every property (m1's last), m2 shares all but one.
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n@prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
         ex:C owl:hasKey ( ex:k0 ex:k1 ex:k2 ex:k3 ex:k4 ) .\n",
    );
    for m in 0..3 {
        writeln!(ttl, "ex:m{m} a ex:C .").unwrap();
        for k in 0..5 {
            for v in 0..30 {
                let value = if v == 29 && (m < 2 || k < 4) {
                    "shared".to_string()
                } else {
                    format!("{m}-{v}")
                };
                writeln!(ttl, "ex:m{m} ex:k{k} \"{value}\" .").unwrap();
            }
        }
    }
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let started = std::time::Instant::now();
    e.entail(INTO, Regime::OwlRl).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    let same: Vec<String> = inferred(&e)
        .into_iter()
        .filter(|t| t.contains("owl:sameAs"))
        .map(|t| t.replace("http://www.w3.org/2002/07/owl#", "owl:"))
        .collect();
    assert_eq!(
        same,
        [
            "m0 owl:sameAs m0",
            "m0 owl:sameAs m1",
            "m1 owl:sameAs m0",
            "m1 owl:sameAs m1"
        ]
    );
}

#[test]
fn owl_rl_cardinalities_are_integers() {
    use tripletrail::store::entail::Regime;
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        r#"@prefix ex: <http://example.com/> .
           @prefix owl: <http://www.w3.org/2002/07/owl#> .
           @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
           ex:A owl:onProperty ex:p ; owl:maxCardinality "1" .
           ex:B owl:onProperty ex:p ; owl:maxCardinality "1"^^xsd:boolean .
           ex:C owl:onProperty ex:p ; owl:maxCardinality "1"^^xsd:int .
           ex:a a ex:A , ex:B ; ex:p ex:x1 , ex:x2 .
           ex:c a ex:C ; ex:p ex:y1 , ex:y2 ."#
            .as_bytes(),
    )
    .unwrap();
    e.entail(INTO, Regime::OwlRl).unwrap();
    let got: Vec<String> = inferred(&e)
        .into_iter()
        .map(|t| t.replace("http://www.w3.org/2002/07/owl#", "owl:"))
        .collect();
    assert_eq!(
        got,
        [
            "y1 owl:sameAs y1",
            "y1 owl:sameAs y2",
            "y2 owl:sameAs y1",
            "y2 owl:sameAs y2"
        ]
    );
}

#[test]
fn owl_rl_consistency_rules_list_contradictions_and_write_nothing() {
    use tripletrail::store::entail::Regime;
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        r#"@prefix ex: <http://example.com/> .
           @prefix owl: <http://www.w3.org/2002/07/owl#> .
           @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
           @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
           # cax-dw through a sub class.
           ex:Halt rdfs:subClassOf ex:Station .
           ex:Station owl:disjointWith ex:Track .
           # Stated both ways, still once.
           ex:Track owl:disjointWith ex:Station .
           ex:h1 a ex:Halt , ex:Track .
           # cls-com.
           ex:Open owl:complementOf ex:Closed .
           ex:l1 a ex:Open , ex:Closed .
           # cax-adc.
           [] a owl:AllDisjointClasses ; owl:members ( ex:A ex:B ex:C ) .
           ex:x a ex:A , ex:C .
           # cls-nothing2.
           ex:void a owl:Nothing .
           # prp-irp, prp-asyp (each pair once).
           ex:next a owl:IrreflexiveProperty .
           ex:n1 ex:next ex:n1 .
           ex:parent a owl:AsymmetricProperty .
           ex:a ex:parent ex:b . ex:b ex:parent ex:a .
           # prp-pdw.
           ex:north owl:propertyDisjointWith ex:south .
           ex:s1 ex:north ex:s2 ; ex:south ex:s2 .
           # prp-adp.
           [] a owl:AllDisjointProperties ; owl:members ( ex:p ex:q ) .
           ex:t1 ex:p ex:t2 ; ex:q ex:t2 .
           # eq-diff1 through a functional property, eq-diff2.
           ex:id a owl:FunctionalProperty .
           ex:k ex:id ex:m1 , ex:m2 .
           ex:m1 owl:differentFrom ex:m2 .
           [] a owl:AllDifferent ; owl:distinctMembers ( ex:m1 ex:m3 ex:m2 ) .
           # prp-npa1, prp-npa2.
           [] owl:sourceIndividual ex:s1 ; owl:assertionProperty ex:north ;
              owl:targetIndividual ex:s2 .
           [] owl:sourceIndividual ex:s1 ; owl:assertionProperty ex:label ;
              owl:targetValue "one" .
           ex:s1 ex:label "one" .
           # cls-maxc1 and cls-maxqc1; a maximum of 1 is no contradiction.
           ex:Leaf owl:onProperty ex:child ; owl:maxCardinality "0"^^xsd:nonNegativeInteger .
           ex:leaf a ex:Leaf ; ex:child ex:c1 .
           ex:NoTrack owl:onProperty ex:has ; owl:onClass ex:Track ;
              owl:maxQualifiedCardinality "0"^^xsd:nonNegativeInteger .
           ex:nt a ex:NoTrack ; ex:has ex:h1 , ex:l1 .
           ex:One owl:onProperty ex:child ; owl:maxCardinality "1"^^xsd:nonNegativeInteger .
           ex:one a ex:One ; ex:child ex:c1 ."#
            .as_bytes(),
    )
    .unwrap();
    // Before entailing, the type through the sub class and the owl:sameAs are not there.
    let before = e.inconsistencies(100, Some(INTO)).unwrap().count;
    let done = e.entail(INTO, Regime::OwlRl).unwrap();
    assert!(done.triples > 0);
    let found = e.inconsistencies(100, Some(INTO)).unwrap();
    assert!(!found.at_least);
    let (count, found) = (found.count, found.first);
    assert_eq!(count, found.len());
    let mut got: Vec<String> = found
        .iter()
        .map(|i| {
            let terms: Vec<String> = i
                .terms
                .iter()
                .map(|t| {
                    t.to_string()
                        .replace("http://example.com/", "")
                        .replace(['<', '>'], "")
                })
                .collect();
            format!("{} {}", i.rule, terms.join(" "))
        })
        .collect();
    got.sort();
    let mut expected = vec![
        "cax-adc x A C",
        "cax-dw h1 Station Track",
        "cls-com l1 Open Closed",
        "cls-maxc1 leaf Leaf child c1",
        "cls-maxqc1 nt NoTrack has h1",
        "cls-nothing2 void",
        // m1 and m2 are the same (the functional property): once each, the list as written.
        "eq-diff1 m1 m2",
        "eq-diff2 m1 m2",
        "prp-adp t1 t2 p q",
        "prp-asyp a b parent",
        "prp-irp n1 next",
        "prp-npa1 s1 north s2",
        "prp-npa2 s1 label \"one\"",
        "prp-pdw s1 s2 north south",
    ];
    expected.sort();
    assert_eq!(got, expected);
    assert!(before < count);
    // Nothing of it is written.
    assert_eq!(e.entail(INTO, Regime::OwlRl).unwrap().triples, done.triples);
}

#[test]
fn owl_rl_declared_classes_and_properties_and_closed_domains_and_ranges() {
    use tripletrail::store::entail::Regime;
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        r#"@prefix ex: <http://example.com/> .
           @prefix owl: <http://www.w3.org/2002/07/owl#> .
           @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
           ex:Station a owl:Class ; rdfs:subClassOf ex:Place .
           ex:Station rdfs:subClassOf ex:Station .
           ex:next a owl:ObjectProperty ; rdfs:subPropertyOf ex:near .
           ex:near rdfs:range ex:Place .
           ex:name a owl:DatatypeProperty ; rdfs:domain ex:Station .
           ex:bxl a ex:Station ."#
            .as_bytes(),
    )
    .unwrap();
    // Jena's RDFS rules write neither.
    let rdfs = e.entail(INTO, Regime::Rdfs).unwrap();
    assert_eq!(inferred(&e).len(), rdfs.triples);
    assert!(!inferred(&e).iter().any(|t| t.contains("owl:Thing")));
    e.entail(INTO, Regime::OwlRl).unwrap();
    let got = inferred(&e);
    for t in [
        // scm-cls, for the declared class and owl:Thing and owl:Nothing.
        "Station owl:equivalentClass Station",
        "Station rdfs:subClassOf owl:Thing",
        "owl:Nothing rdfs:subClassOf Station",
        "owl:Thing rdfs:subClassOf owl:Thing",
        "owl:Nothing rdfs:subClassOf owl:Thing",
        "bxl rdf:type owl:Thing",
        // scm-op, scm-dp.
        "next rdfs:subPropertyOf next",
        "next owl:equivalentProperty next",
        "name rdfs:subPropertyOf name",
        // scm-rng2 then scm-rng1 (Place is no declared class: no owl:Thing range), scm-dom1.
        "next rdfs:range Place",
        "name rdfs:domain Place",
        "name rdfs:domain owl:Thing",
    ] {
        assert!(got.iter().any(|g| g == t), "missing {t} in {got:#?}");
    }
    // Asserted, so not written; and an undeclared class is not its own sub class.
    for t in [
        "Station rdfs:subClassOf Station",
        "Place rdfs:subClassOf Place",
        "near rdfs:range Place",
    ] {
        assert!(!got.iter().any(|g| g == t), "{t} in {got:#?}");
    }
}

#[test]
fn owl_rl_self_equivalence_and_same_as_classes_keep_their_sub_classes() {
    use tripletrail::store::entail::Regime;
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        r#"@prefix ex: <http://example.com/> .
           @prefix owl: <http://www.w3.org/2002/07/owl#> .
           @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
           ex:C owl:equivalentClass ex:C .
           ex:p owl:equivalentProperty ex:p .
           ex:a rdfs:subPropertyOf ex:b .
           ex:a owl:sameAs ex:b ."#
            .as_bytes(),
    )
    .unwrap();
    e.entail(INTO, Regime::OwlRl).unwrap();
    let got = inferred(&e);
    // scm-eqc1, scm-eqp1 on a term equivalent to itself; and the sub property edge, a self
    // edge once a and b are one owl:sameAs class, still relates its members.
    for t in [
        "C rdfs:subClassOf C",
        "p rdfs:subPropertyOf p",
        "a rdfs:subPropertyOf a",
        "b rdfs:subPropertyOf a",
        "b rdfs:subPropertyOf b",
    ] {
        assert!(got.iter().any(|g| g == t), "missing {t} in {got:#?}");
    }
}

#[test]
fn a_kept_owl_rl_entailment_matches_entailing_from_scratch_after_random_writes() {
    use tripletrail::store::entail::Regime;
    // A fixed schema with joins (a someValuesFrom restriction, an intersection, a chain), a
    // transitive and an inverse property; the writes are mostly data, which the kept
    // entailment follows from the added triples alone, and now and then schema, owl:sameAs
    // or a removal, which entail again.
    let schema = r#"
        @prefix e: <http://e/> .
        @prefix owl: <http://www.w3.org/2002/07/owl#> .
        @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
        e:R owl:onProperty e:p0 ; owl:someValuesFrom e:C1 .
        e:C2 owl:equivalentClass e:R .
        e:I owl:intersectionOf ( e:C0 e:C1 ) .
        e:I rdfs:subClassOf e:C3 .
        e:p1 a owl:TransitiveProperty ; owl:inverseOf e:p2 .
        e:p3 owl:propertyChainAxiom ( e:p0 e:p1 ) .
        e:p2 rdfs:domain e:C0 .
    "#;
    let terms = [
        "<http://e/C0>",
        "<http://e/C1>",
        "<http://e/C2>",
        "<http://e/C3>",
        "<http://e/p0>",
        "<http://e/p1>",
        "<http://e/p2>",
        "<http://e/i0>",
        "<http://e/i1>",
        "<http://e/i2>",
        "<http://e/i3>",
        "<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>",
        "<http://www.w3.org/2000/01/rdf-schema#subClassOf>",
        "<http://www.w3.org/2002/07/owl#sameAs>",
        "<http://www.w3.org/2002/07/owl#SymmetricProperty>",
    ];
    // Mostly data properties and rdf:type; rdfs:subClassOf and owl:sameAs now and then.
    let predicates = [4, 4, 5, 5, 6, 11, 11, 11, 12, 13];
    let subjects = [7, 8, 9, 10, 7, 8, 9, 10, 0, 1, 4];
    let objects = [0, 1, 2, 3, 7, 8, 9, 10, 7, 8, 9, 10, 14];
    let mut seed: u64 = 0x0717;
    let mut next = |n: usize| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((seed >> 33) as usize) % n
    };
    let from_scratch = |e: &Engine| {
        let fresh = Engine::new_tripletrail();
        let quads: Vec<_> = e
            .store()
            .quads_for_pattern(None, None, None, None)
            .map(Result::unwrap)
            .filter(|q| q.graph_name != INTO.into())
            .collect();
        fresh.load_quads(quads).unwrap();
        fresh.entail(INTO, Regime::OwlRl).unwrap();
        inferred(&fresh)
    };
    for round in 0..8 {
        let e = Engine::new_tripletrail();
        e.load(RdfFormat::Turtle, schema.as_bytes()).unwrap();
        e.set_entailment(Some(INTO.into_owned()), Regime::OwlRl)
            .unwrap();
        assert_eq!(inferred(&e), from_scratch(&e), "round {round}, start");
        for step in 0..40 {
            let mut quads = String::new();
            for _ in 0..1 + next(3) {
                let s = terms[subjects[next(subjects.len())]];
                let p = terms[predicates[next(predicates.len())]];
                let o = terms[objects[next(objects.len())]];
                quads += &if next(4) == 0 {
                    format!("GRAPH <http://e/g> {{ {s} {p} {o} }} ")
                } else {
                    format!("{s} {p} {o} . ")
                };
            }
            let update = if next(8) == 0 {
                let asserted: Vec<String> = e
                    .store()
                    .quads_for_pattern(None, None, None, None)
                    .map(Result::unwrap)
                    .filter(|q| q.graph_name != INTO.into())
                    // DELETE DATA takes no blank nodes (the schema's lists and restriction).
                    .filter(|q| !q.to_string().contains("_:"))
                    .map(|q| {
                        let triple = format!("{} {} {}", q.subject, q.predicate, q.object);
                        if q.graph_name.is_default_graph() {
                            format!("{triple} .")
                        } else {
                            format!("GRAPH {} {{ {triple} }}", q.graph_name)
                        }
                    })
                    .collect();
                format!("DELETE DATA {{ {} }}", asserted[next(asserted.len())])
            } else {
                format!("INSERT DATA {{ {quads} }}")
            };
            e.update(&update).unwrap();
            assert_eq!(
                inferred(&e),
                from_scratch(&e),
                "round {round}, step {step}: {update}"
            );
        }
    }
}

/// Chains of someValuesFrom and allValuesFrom restrictions, each round
/// deriving one type triple (#756): the rules join only each round's new triples, so a chain
/// of 2,000 takes well under a second (it took minutes when every restriction was read
/// again in every round).
#[test]
fn owl_rl_follows_long_chains_of_restrictions() {
    use std::fmt::Write;
    use tripletrail::store::entail::Regime;
    let n = 2000;
    let mut ttl =
        String::from("@prefix : <urn:x:> . @prefix owl: <http://www.w3.org/2002/07/owl#> .\n");
    for i in 0..n {
        let j = i + 1;
        // cls-svf1 backwards: x_j a S_j gives x_i a S_i.
        writeln!(
            ttl,
            ":S{i} owl:someValuesFrom :S{j} ; owl:onProperty :p . :x{i} :p :x{j} ."
        )
        .unwrap();
        // cls-avf forwards: y_i a A_i gives y_j a A_j.
        writeln!(
            ttl,
            ":A{i} owl:allValuesFrom :A{j} ; owl:onProperty :q . :y{i} :q :y{j} ."
        )
        .unwrap();
    }
    writeln!(ttl, ":x{n} a :S{n} . :y0 a :A0 .").unwrap();
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let started = std::time::Instant::now();
    e.entail(INTO, Regime::OwlRl).unwrap();
    let elapsed = started.elapsed();
    let count = |q: &str| -> usize {
        let QueryResults::Solutions(s) = e.query(q).unwrap() else {
            panic!()
        };
        s.count()
    };
    let typed = |prefix: &str, subject: &str| {
        count(&format!(
            "SELECT * WHERE {{ GRAPH <urn:tripletrail:inferred> {{ ?s a ?c }} \
             FILTER(STRSTARTS(STR(?s), 'urn:x:{subject}') && STRSTARTS(STR(?c), 'urn:x:{prefix}')) }}"
        ))
    };
    // x_0 … x_n-1 each of their S, y_1 … y_n each of their A.
    assert_eq!(typed("S", "x"), n);
    assert_eq!(typed("A", "y"), n);
    // Generous for a debug build on a busy runner; the cubic version took minutes.
    assert!(elapsed.as_secs() < 60, "{elapsed:?}");
}
