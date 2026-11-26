//! Generated shapes and reader projections (#137).

use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::vocab::rdf;
use oxigraph::model::{Graph, NamedNode, NamedOrBlankNodeRef, Term, TermRef};
use tripletrail::shapes::{self, Reader};
use tripletrail::{Engine, ProfileOptions};

const EX: &str = "http://example.org/";
const SH: &str = "http://www.w3.org/ns/shacl#";
const TTS: &str = shapes::TTS;

fn iri(s: &str) -> NamedNode {
    NamedNode::new_unchecked(s)
}

fn parse(ttl: &str) -> Graph {
    let mut graph = Graph::new();
    for t in RdfParser::from_format(RdfFormat::Turtle).for_slice(ttl.as_bytes()) {
        graph.insert(&t.unwrap().into());
    }
    graph
}

fn data() -> String {
    let mut ttl = String::from(
        "@prefix ex: <http://example.org/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         ex:Signal rdfs:subClassOf ex:Asset .\n",
    );
    for i in 0..12 {
        ttl.push_str(&format!(
            "ex:sig{i} a ex:Signal ; rdfs:label \"S{i}\" ; ex:status ex:status{} ; ex:height {i} ;\n\
             \x20 geo:hasGeometry [ geo:asWKT \"POINT({i} 1)\"^^geo:wktLiteral ] .\n",
            i % 3
        ));
    }
    for i in 0..4 {
        ttl.push_str(&format!("ex:a{i} a ex:Asset ; ex:owner \"Owner {i}\" .\n"));
    }
    ttl
}

fn object(g: &Graph, s: &NamedNode, p: &str) -> Option<Term> {
    g.object_for_subject_predicate(s, &iri(p))
        .map(TermRef::into_owned)
}

fn property_shape(g: &Graph, node: &NamedNode, path: &str) -> Option<NamedNode> {
    g.objects_for_subject_predicate(node, &iri(&format!("{SH}property")))
        .find_map(|p| match p {
            TermRef::NamedNode(p)
                if g.object_for_subject_predicate(p, &iri(&format!("{SH}path")))
                    == Some(TermRef::from(iri(path).as_ref())) =>
            {
                Some(p.into_owned())
            }
            _ => None,
        })
}

#[test]
fn the_vocabulary_parses() {
    let vocabulary = parse(shapes::VOCABULARY);
    for reader in Reader::ALL {
        assert!(vocabulary.contains(oxigraph::model::TripleRef::new(
            &reader.iri(),
            rdf::TYPE,
            &iri(&format!("{TTS}Reader"))
        )));
    }
}

#[test]
fn generated_shapes_from_a_profile() {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    let profile = engine.dataset_profile(ProfileOptions::default()).unwrap();
    let g = shapes::generate(&profile);
    let signal = shapes::generated_shape(&iri(&format!("{EX}Signal")));
    assert_eq!(
        object(&g, &signal, &format!("{SH}targetClass")),
        Some(iri(&format!("{EX}Signal")).into())
    );
    assert_eq!(
        object(&g, &signal, &format!("{TTS}key"))
            .unwrap()
            .to_string(),
        "\"Signal\""
    );
    assert!(object(&g, &signal, &format!("{TTS}color")).is_some());
    assert_eq!(
        object(&g, &signal, &format!("{TTS}labelPath")),
        Some(iri("http://www.w3.org/2000/01/rdf-schema#label").into())
    );
    // A code list, with its cardinality (every instance was read).
    let status = property_shape(&g, &signal, &format!("{EX}status")).unwrap();
    assert!(object(&g, &status, &format!("{SH}in")).is_some());
    assert_eq!(
        object(&g, &status, &format!("{TTS}facet")),
        Some(iri(&format!("{TTS}Facet-Values")).into())
    );
    assert_eq!(
        object(&g, &status, &format!("{SH}minCount"))
            .unwrap()
            .to_string(),
        "\"1\"^^<http://www.w3.org/2001/XMLSchema#integer>"
    );
    assert_eq!(
        object(&g, &status, &format!("{SH}nodeKind")),
        Some(iri(&format!("{SH}IRI")).into())
    );
    let height = property_shape(&g, &signal, &format!("{EX}height")).unwrap();
    assert_eq!(
        object(&g, &height, &format!("{SH}datatype")),
        Some(iri("http://www.w3.org/2001/XMLSchema#integer").into())
    );
    assert_eq!(
        object(&g, &height, &format!("{TTS}facet")),
        Some(iri(&format!("{TTS}Facet-Range")).into())
    );
    // Names, too many to list: searched for text.
    let label = property_shape(&g, &signal, "http://www.w3.org/2000/01/rdf-schema#label").unwrap();
    assert_eq!(
        object(&g, &label, &format!("{TTS}facet")),
        Some(iri(&format!("{TTS}Facet-Text")).into())
    );
    // Assets have no geometry: hidden from the map.
    let asset = shapes::generated_shape(&iri(&format!("{EX}Asset")));
    assert!(object(&g, &asset, &format!("{TTS}hiddenFrom")).is_some());
    // A sample of part of the instances claims no cardinality.
    let small = engine
        .dataset_profile(ProfileOptions {
            sample: 5,
            ..ProfileOptions::default()
        })
        .unwrap();
    let g = shapes::generate(&small);
    let status = property_shape(&g, &signal, &format!("{EX}status")).unwrap();
    assert!(object(&g, &status, &format!("{SH}minCount")).is_none());
    assert!(object(&g, &status, &format!("{SH}maxCount")).is_none());
}

#[test]
fn projections_per_reader_and_profile() {
    let set = parse(
        "@prefix ex: <http://example.org/> .\n\
         @prefix sh: <http://www.w3.org/ns/shacl#> .\n\
         @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         ex:Signal rdfs:subClassOf ex:Asset .\n\
         ex:AssetShape a sh:NodeShape ; sh:targetClass ex:Asset ;\n\
           sh:property [ sh:path ex:owner ] , [ sh:path ex:status ; sh:name \"asset status\" ] .\n\
         ex:SignalShape a sh:NodeShape ; sh:targetClass ex:Signal ; tts:inProfile ex:Rinf ;\n\
           sh:property [ sh:path ex:status ; sh:in ( ex:on ex:off ) ] ,\n\
             [ sh:path ex:secret ; tts:hiddenFrom tts:Reader-Map ] ,\n\
             [ sh:path ex:mapOnly ; tts:onlyFor tts:Reader-Map ] .\n\
         ex:EtcsShape a sh:NodeShape ; sh:targetClass ex:Balise ; tts:inProfile ex:Etcs .\n\
         ex:OffShape a sh:NodeShape ; sh:targetClass ex:Old ; sh:deactivated true .\n",
    );
    let signal = iri(&format!("{EX}SignalShape"));
    let paths = |g: &Graph, node: &NamedNode| -> Vec<String> {
        let mut paths: Vec<String> = g
            .objects_for_subject_predicate(node, &iri(&format!("{SH}property")))
            .filter_map(|p| match p {
                TermRef::NamedNode(n) => Some(NamedOrBlankNodeRef::from(n)),
                TermRef::BlankNode(b) => Some(b.into()),
                _ => None,
            })
            .filter_map(|p| g.object_for_subject_predicate(p, &iri(&format!("{SH}path"))))
            .map(|p| {
                p.to_string()
                    .trim_start_matches("<http://example.org/")
                    .trim_end_matches('>')
                    .to_owned()
            })
            .collect();
        paths.sort();
        paths
    };
    let has = |g: &Graph, s: &str| {
        g.triples_for_subject(&iri(&format!("{EX}{s}")))
            .next()
            .is_some()
    };

    // The map: the hidden property out, the map-only one in, the inherited owner added, and
    // status not inherited (the signal shape has its own).
    let map = shapes::project(&set, Some(Reader::Map), &[]);
    assert_eq!(paths(&map, &signal), ["mapOnly", "owner", "status"]);
    assert!(!has(&map, "OffShape"));
    assert!(has(&map, "EtcsShape"));
    // The code list (an RDF list) comes along.
    assert!(map.iter().any(|t| t.predicate == rdf::FIRST));
    // Search: no map-only property, the hidden one in.
    let search = shapes::project(&set, Some(Reader::Search), &[]);
    assert_eq!(paths(&search, &signal), ["owner", "secret", "status"]);
    // Validation: nothing inherited (the asset shape applies to signals by its own target).
    let validation = shapes::project(&set, Some(Reader::Validation), &[]);
    assert_eq!(paths(&validation, &signal), ["secret", "status"]);
    // Profiles: untagged shapes are in all of them.
    let etcs = shapes::project(&set, Some(Reader::Map), &[iri(&format!("{EX}Etcs"))]);
    assert!(!has(&etcs, "SignalShape"));
    assert!(has(&etcs, "EtcsShape") && has(&etcs, "AssetShape"));
    assert_eq!(etcs.iter().filter(|t| t.predicate == rdf::FIRST).count(), 0);
    let rinf = shapes::project(&set, Some(Reader::Map), &[iri(&format!("{EX}Rinf"))]);
    assert!(has(&rinf, "SignalShape") && !has(&rinf, "EtcsShape"));
    // The class hierarchy stays.
    assert!(has(&rinf, "Signal"));
}

/// #140: a superclass's shape a reader does not read adds nothing under a subclass's shape,
/// and an inherited property shape of a key (or, for describe pages, a name) the subclass's
/// shape has is left out, as one of its path is.
#[test]
fn inherited_properties_follow_the_reader_and_the_keys() {
    let set = parse(
        "@prefix ex: <http://example.org/> .\n\
         @prefix sh: <http://www.w3.org/ns/shacl#> .\n\
         @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         ex:Signal rdfs:subClassOf ex:Asset .\n\
         ex:AssetMap a sh:NodeShape ; sh:targetClass ex:Asset ; tts:hiddenFrom tts:Reader-Describe ;\n\
           sh:property [ sh:path ex:colour ; tts:hiddenFrom tts:Reader-Describe ] .\n\
         ex:AssetShape a sh:NodeShape ; sh:targetClass ex:Asset ; tts:hiddenFrom tts:Reader-Map ;\n\
           sh:property [ sh:path ex:km ; tts:key \"where\" ] , [ sh:path ex:within ; sh:name \"Part of\" ] ,\n\
             [ sh:path ex:owner ] .\n\
         ex:SignalShape a sh:NodeShape ; sh:targetClass ex:Signal ;\n\
           sh:property [ sh:path ex:position ; tts:key \"where\" ] ,\n\
             [ sh:path ( ex:within ex:area ) ; sh:name \"Part of\" ] .\n",
    );
    let signal = iri(&format!("{EX}SignalShape"));
    let paths = |g: &Graph| -> Vec<String> {
        let mut paths: Vec<String> = g
            .objects_for_subject_predicate(&signal, &iri(&format!("{SH}property")))
            .filter_map(|p| match p {
                TermRef::NamedNode(n) => Some(NamedOrBlankNodeRef::from(n)),
                TermRef::BlankNode(b) => Some(b.into()),
                _ => None,
            })
            .filter_map(|p| g.object_for_subject_predicate(p, &iri(&format!("{SH}path"))))
            .map(|p| match p {
                TermRef::NamedNode(n) => n.as_str().trim_start_matches(EX).to_owned(),
                _ => "(list)".to_owned(),
            })
            .collect();
        paths.sort();
        paths
    };
    // Describe pages: no map colour (hidden from them); not km (its key is the signal's), nor
    // within (its name).
    let describe = shapes::project(&set, Some(Reader::Describe), &[]);
    assert_eq!(paths(&describe), ["(list)", "owner", "position"]);
    // The map: the map shape's colour, and the asset shape's rows, though the asset shape is no
    // layer of its own (within has another path, and the map keys no row by its name).
    let map = shapes::project(&set, Some(Reader::Map), &[]);
    assert_eq!(
        paths(&map),
        ["(list)", "colour", "owner", "position", "within"]
    );
}

#[test]
fn an_abstract_superclass_hidden_from_the_map_lends_its_rows() {
    let set = parse(
        "@prefix ex: <http://example.org/> .\n\
         @prefix sh: <http://www.w3.org/ns/shacl#> .\n\
         @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         ex:Signal rdfs:subClassOf ex:Asset .\n\
         ex:AssetShape a sh:NodeShape ; sh:targetClass ex:Asset ;\n\
           tts:hiddenFrom tts:Reader-Map , tts:Reader-OgcFeatures ;\n\
           sh:property [ sh:path ex:owner ; tts:key \"owner\" ; tts:facet tts:Facet-Values ] ,\n\
             [ sh:path ex:name ; tts:key \"name\" ] .\n\
         ex:SignalShape a sh:NodeShape ; sh:targetClass ex:Signal ;\n\
           tts:geometry [ sh:path ex:wkt ] ; sh:property [ sh:path ex:position ] .\n",
    );
    let signal = iri(&format!("{EX}SignalShape"));
    let asset = iri(&format!("{EX}AssetShape"));
    for reader in [Reader::Map, Reader::OgcFeatures] {
        let g = shapes::project(&set, Some(reader), &[]);
        // No layer or collection of assets.
        assert!(
            g.objects_for_subject_predicate(&asset, &iri(&format!("{SH}targetClass")))
                .next()
                .is_none(),
            "{reader:?}"
        );
        let mut paths: Vec<String> = g
            .objects_for_subject_predicate(&signal, &iri(&format!("{SH}property")))
            .filter_map(|p| match p {
                TermRef::NamedNode(n) => Some(NamedOrBlankNodeRef::from(n)),
                TermRef::BlankNode(b) => Some(b.into()),
                _ => None,
            })
            .filter_map(|p| g.object_for_subject_predicate(p, &iri(&format!("{SH}path"))))
            .map(|p| p.to_string())
            .collect();
        paths.sort();
        assert_eq!(
            paths,
            [
                "<http://example.org/name>",
                "<http://example.org/owner>",
                "<http://example.org/position>"
            ],
            "{reader:?}"
        );
    }
}

#[test]
fn a_generated_set_as_files_loads_back_and_overlays() {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    let profile = engine.dataset_profile(ProfileOptions::default()).unwrap();
    let generated = shapes::generate(&profile);
    assert!(
        shapes::check(&generated).is_empty(),
        "{:?}",
        shapes::check(&generated)
    );
    let files = shapes::files_of(&generated);
    let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["Asset.ttl", "Signal.ttl", "_set.ttl"]);
    let signal = &files[1].text;
    assert!(
        signal.contains("sh:in ( <http://example.org/status0>"),
        "{signal}"
    );
    assert!(signal.contains("tts:geometry [\n"), "{signal}");
    // The same text from the same data: files diff well.
    let again = shapes::files_of(&shapes::generate(&profile));
    assert_eq!(files, again);
    let loaded = shapes::load(&files).unwrap();
    assert_eq!(loaded.len(), generated.len());

    // A customised signal shape replaces the generated one; the asset shape stays.
    let custom = shapes::load(&[shapes::SetFile {
        name: "signal.ttl".to_owned(),
        text: format!(
            "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
             <{}> a sh:NodeShape ; sh:targetClass <{EX}Signal> ;\n\
             sh:property [ sh:path <{EX}status> ] .\n",
            shapes::generated_shape(&iri(&format!("{EX}Signal"))).as_str()
        ),
    }])
    .unwrap();
    let both = shapes::overlay(&generated, &custom);
    let signal = shapes::generated_shape(&iri(&format!("{EX}Signal")));
    let asset = shapes::generated_shape(&iri(&format!("{EX}Asset")));
    assert_eq!(
        both.objects_for_subject_predicate(&signal, &iri(&format!("{SH}property")))
            .count(),
        1
    );
    assert!(object(&both, &signal, &format!("{TTS}color")).is_none());
    assert!(object(&both, &asset, &format!("{TTS}key")).is_some());
    assert!(shapes::check(&both).is_empty());
    // A tts:labelStrip that is no regular expression is named (#260).
    let strips = |pattern: &str| -> Vec<String> {
        shapes::load(&[shapes::SetFile {
            name: "strip.ttl".to_owned(),
            text: format!(
                "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
                 @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
                 <urn:my:s> a sh:NodeShape ; sh:targetClass <{EX}Signal> ; tts:labelStrip {pattern:?} .\n"
            ),
        }])
        .err()
        .unwrap_or_default()
    };
    assert!(strips(shapes::VALIDITY).is_empty());
    let errors = strips("(unclosed");
    assert!(
        errors
            .iter()
            .any(|e| e.contains("tts:labelStrip is not a regular expression")),
        "{errors:?}"
    );
    // It keeps the generated shape's instance count.
    assert_eq!(
        object(&both, &signal, &format!("{TTS}instances")),
        object(&generated, &signal, &format!("{TTS}instances"))
    );

    // A shape of its own IRI for a class replaces the class's generated shape too (#139),
    // unless it is only for some readers.
    let own = |extra: &str| {
        shapes::load(&[shapes::SetFile {
            name: "asset.ttl".to_owned(),
            text: format!(
                "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
                 @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
                 <urn:my:asset> a sh:NodeShape ; sh:targetClass <{EX}Asset> ; tts:key \"assets\" {extra} ;\n\
                 sh:property [ sh:path <{EX}status> ] .\n"
            ),
        }])
        .unwrap()
    };
    let mine = iri("urn:my:asset");
    assert!(object(&generated, &asset, &format!("{TTS}instances")).is_some());
    let both = shapes::overlay(&generated, &own(""));
    assert!(both.triples_for_subject(&asset).next().is_none());
    assert_eq!(
        object(&both, &mine, &format!("{TTS}instances")),
        object(&generated, &asset, &format!("{TTS}instances"))
    );
    assert!(object(&both, &signal, &format!("{TTS}key")).is_some());
    assert!(
        shapes::check(&both).is_empty(),
        "{:?}",
        shapes::check(&both)
    );
    let both = shapes::overlay(&generated, &own("; tts:onlyFor tts:Reader-Validation"));
    assert!(object(&both, &asset, &format!("{TTS}key")).is_some());
    assert!(object(&both, &mine, &format!("{TTS}instances")).is_none());
    // Nor does a plain SHACL constraint shape, without any tts: annotation.
    let rule = shapes::load(&[shapes::SetFile {
        name: "rule.ttl".to_owned(),
        text: format!(
            "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
             <urn:my:rule> a sh:NodeShape ; sh:targetClass <{EX}Asset> ;\n\
             sh:property [ sh:path <{EX}status> ; sh:minCount 1 ] .\n"
        ),
    }])
    .unwrap();
    let both = shapes::overlay(&generated, &rule);
    assert!(object(&both, &asset, &format!("{TTS}key")).is_some());
    assert!(object(&both, &iri("urn:my:rule"), &format!("{TTS}instances")).is_none());
}

#[test]
fn checks_name_what_is_wrong() {
    let errors = shapes::load(&[
        shapes::SetFile {
            name: "a.ttl".to_owned(),
            text: "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
                   @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
                   <urn:a> a sh:NodeShape ; tts:key \"a\" ; tts:colour \"red\" ; tts:color \"red\" ;\n\
                     tts:minZoom 30 ; tts:hiddenFrom tts:Reader-Nope ;\n\
                     sh:property [ sh:name \"no path\" ] .\n\
                   <urn:b> sh:targetClass \"x\" ; tts:key \"a\" .\n"
                .to_owned(),
        },
        shapes::SetFile {
            name: "broken.ttl".to_owned(),
            text: "<urn:x> <urn:y> .".to_owned(),
        },
    ])
    .unwrap_err();
    let all = errors.join("\n");
    for expected in [
        "broken.ttl:",
        "tts:colour is not in the vocabulary",
        "tts:color \"red\" is not #rrggbb",
        "tts:minZoom 30",
        "is not a reader",
        "has 0 sh:path",
        "sh:targetClass \"x\" is not an IRI",
        "tts:key \"a\" is also the key",
    ] {
        assert!(all.contains(expected), "{expected} in {all}");
    }
}

#[test]
fn computed_facets_are_checked() {
    assert_eq!(
        shapes::check_select(
            "OPTIONAL { $this <http://example.org/speed> ?s } # the speed\n\
             BIND(IF(?s >= 160, \"fast\", \"slow\") AS ?value)"
        ),
        Ok(())
    );
    assert_eq!(
        shapes::check_select(
            "SERVICE <http://w3id.org/tripletrail/st> { ?this <http://example.org/p> ?value }"
        ),
        Ok(())
    );
    for (pattern, expected) in [
        ("$this ex:speed ?value", "does not parse"),
        ("$this <http://example.org/speed> ?s", "binds no ?value"),
        (
            "$this <http://example.org/speed> ?s . \
             FILTER EXISTS { SERVICE <http://example.org/sparql> { ?s ?p ?value } } \
             BIND(?s AS ?value)",
            "calls SERVICE <http://example.org/sparql>",
        ),
        (
            "SERVICE ?endpoint { $this <http://example.org/speed> ?value }",
            "calls SERVICE ?endpoint",
        ),
    ] {
        let error = shapes::check_select(pattern).unwrap_err();
        assert!(error.contains(expected), "{expected} in {error}");
    }
    // Inlined as the readers do: a path's IRIs stay whole, `#` and all.
    for pattern in [
        "$this <https://e.org/r#a>/<https://e.org/r#b> ?value",
        "$this ^<https://e.org/r#a> ?value",
        "$this <https://e.org/r#a>|<https://e.org/r#b> ?value",
        "$this !<https://e.org/r#a> ?value",
        "$this (<https://e.org/r#a>)* ?value",
        "$this <https://e.org/r#a> ?s FILTER(?s<200 && ?s = <https://e.org/r#c>) BIND(?s AS ?value)",
    ] {
        assert_eq!(shapes::check_select(pattern), Ok(()), "{pattern}");
    }
    let error = shapes::check_select("BIND(1 AS ?x) } VALUES ?value { 1").unwrap_err();
    assert!(error.contains("closes a brace"), "{error}");
    assert_eq!(
        shapes::inline_select(
            "$this <http://e/x?a=1> ?v . OPTIONAL { ?v <http://e/n> ?n } \
             BIND(IF(?n > 3, \"?big\", 'small?') AS ?value) # ?comment\n FILTER(?n < ?value)",
            "?f",
            "?facet2"
        ),
        Ok(" ?f <http://e/x?a=1> ?v_facet2 . OPTIONAL { ?v_facet2 <http://e/n> ?n_facet2 } \
             BIND(IF(?n_facet2 > 3, \"?big\", 'small?') AS ?facet2) \n FILTER(?n_facet2 < ?facet2) ."
            .to_owned())
    );
    assert_eq!(
        shapes::inline_select(
            "$this <https://e.org/r#a>/<https://e.org/r#b> ?s .",
            "?f",
            "?v"
        ),
        Ok(" ?f <https://e.org/r#a>/<https://e.org/r#b> ?s_v .".to_owned())
    );
    let errors = shapes::load(&[shapes::SetFile {
        name: "a.ttl".to_owned(),
        text: "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
               @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
               <urn:a> a sh:NodeShape ; sh:property [ sh:path <urn:p> ; tts:key \"k\" ; tts:select \"$this ?p\" ] .\n"
            .to_owned(),
    }])
    .unwrap_err();
    assert!(
        errors[0].starts_with("<urn:a>: the facet \"k\"'s tts:select does not parse"),
        "{errors:?}"
    );
}

#[test]
fn the_map_leaves_out_umbrella_classes_and_geometries() {
    let mut ttl = String::from(
        "@prefix ex: <http://example.org/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    // Every element is also a track or a station, which have fewer instances each.
    for i in 0..6 {
        let kind = if i < 4 { "Track" } else { "Station" };
        ttl.push_str(&format!(
            "ex:e{i} a ex:Element , ex:{kind} ; geo:hasGeometry ex:g{i} .\n\
             ex:g{i} a geo:Geometry ; geo:asWKT \"POINT({i} 1)\"^^geo:wktLiteral .\n"
        ));
    }
    // A signal is also a device, but most devices are not signals.
    for i in 0..5 {
        let types = if i < 2 {
            "ex:Device , ex:Signal"
        } else {
            "ex:Device"
        };
        ttl.push_str(&format!(
            "ex:d{i} a {types} ; geo:hasGeometry [ geo:asWKT \"POINT({i} 2)\"^^geo:wktLiteral ] .\n"
        ));
    }
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let profile = engine.dataset_profile(ProfileOptions::default()).unwrap();
    let g = shapes::generate(&profile);
    let hidden = |class: &str| -> Vec<String> {
        let node = shapes::generated_shape(&iri(class));
        g.objects_for_subject_predicate(&node, &iri(&format!("{TTS}hiddenFrom")))
            .map(|t| t.to_string())
            .collect()
    };
    let map = format!("<{}>", Reader::Map.iri().as_str());
    // On the map as tracks and stations, and as the elements their geometries belong to;
    // the OGC API still has them.
    assert_eq!(hidden(&format!("{EX}Element")), std::slice::from_ref(&map));
    assert_eq!(
        hidden("http://www.opengis.net/ont/geosparql#Geometry"),
        std::slice::from_ref(&map)
    );
    let geometry = shapes::generated_shape(&iri("http://www.opengis.net/ont/geosparql#Geometry"));
    assert!(object(&g, &geometry, &format!("{TTS}color")).is_none());
    for shown in ["Track", "Station", "Device", "Signal"] {
        assert!(hidden(&format!("{EX}{shown}")).is_empty(), "{shown}");
        let node = shapes::generated_shape(&iri(&format!("{EX}{shown}")));
        assert!(
            object(&g, &node, &format!("{TTS}color")).is_some(),
            "{shown}"
        );
    }
    // A sample of two meets only the tracks among the elements' narrower classes, and only
    // devices that are signals: the elements are still left out, the devices are not.
    let sampled = engine
        .dataset_profile(ProfileOptions {
            sample: 2,
            ..ProfileOptions::default()
        })
        .unwrap();
    let g = shapes::generate(&sampled);
    let off = |class: &str| -> Vec<String> {
        let node = shapes::generated_shape(&iri(&format!("{EX}{class}")));
        g.objects_for_subject_predicate(&node, &iri(&format!("{TTS}hiddenFrom")))
            .map(|t| t.to_string())
            .collect()
    };
    assert_eq!(off("Element"), [map]);
    assert!(off("Device").is_empty());
    assert!(off("Signal").is_empty());
}

/// #80: forms are a reader of their own, so `tts:hiddenFrom` and `tts:onlyFor` keep a shape or
/// a property from the record forms and nowhere else.
#[test]
fn forms_are_a_reader() {
    assert_eq!(Reader::from_name("forms"), Some(Reader::Forms));
    assert_eq!(
        Reader::Forms.iri().as_str(),
        "http://w3id.org/tripletrail/shapes#Reader-Forms"
    );
    let set = parse(
        "@prefix ex: <http://example.org/> .\n\
         @prefix sh: <http://www.w3.org/ns/shacl#> .\n\
         @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
         ex:SignalShape a sh:NodeShape ; sh:targetClass ex:Signal ;\n\
           sh:property [ sh:path ex:km ] , [ sh:path ex:checksum ; tts:hiddenFrom tts:Reader-Forms ] ,\n\
             [ sh:path ex:note ; tts:onlyFor tts:Reader-Forms ] .\n",
    );
    let signal = iri(&format!("{EX}SignalShape"));
    let paths = |g: &Graph| -> Vec<String> {
        let mut paths: Vec<String> = g
            .objects_for_subject_predicate(&signal, &iri(&format!("{SH}property")))
            .filter_map(|p| match p {
                TermRef::NamedNode(n) => Some(NamedOrBlankNodeRef::from(n)),
                TermRef::BlankNode(b) => Some(b.into()),
                _ => None,
            })
            .filter_map(|p| g.object_for_subject_predicate(p, &iri(&format!("{SH}path"))))
            .map(|p| match p {
                TermRef::NamedNode(n) => n.as_str().trim_start_matches(EX).to_owned(),
                _ => "(list)".to_owned(),
            })
            .collect();
        paths.sort();
        paths
    };
    let forms = shapes::project(&set, Some(Reader::Forms), &[]);
    assert_eq!(paths(&forms), ["km", "note"]);
    let describe = shapes::project(&set, Some(Reader::Describe), &[]);
    assert_eq!(paths(&describe), ["checksum", "km"]);
}
