//! The dataset profile (#137): classes, their properties and their paths to a geometry.

use oxigraph::io::RdfFormat;
use tripletrail::{Engine, ProfileOptions};

const EX: &str = "http://example.org/";

fn data() -> String {
    let mut ttl = String::from(
        "@prefix ex: <http://example.org/> .\n\
         @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n\
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         @prefix skos: <http://www.w3.org/2004/02/skos/core#> .\n\
         ex:Signal rdfs:label \"Signal\"@en ; rdfs:subClassOf ex:Asset .\n\
         ex:status rdfs:label \"status\" .\n",
    );
    for i in 0..30 {
        // Signals: a direct geometry, a label, a code list and a link to a track.
        ttl.push_str(&format!(
            "ex:sig{i} a ex:Signal ; skos:prefLabel \"S{i}\"@en ; ex:status ex:status{} ;\n\
             \x20 ex:onTrack ex:track{} ; geo:hasGeometry [ geo:asWKT \"POINT({i} 1)\"^^geo:wktLiteral ] .\n",
            i % 3,
            i % 5
        ));
        if i % 2 == 0 {
            ttl.push_str(&format!("ex:sig{i} ex:height {} .\n", 3 + i));
        }
    }
    for i in 0..5 {
        // Tracks reach their geometry through a net reference (three hops).
        ttl.push_str(&format!(
            "ex:track{i} a ex:Track ; rdfs:label \"T{i}\" ; ex:netReference ex:ref{i} .\n\
             ex:ref{i} a ex:NetReference ; geo:hasGeometry ex:geom{i} .\n\
             ex:geom{i} geo:asWKT \"LINESTRING({i} 0, {i} 9)\"^^geo:wktLiteral .\n"
        ));
    }
    ttl
}

#[test]
fn profile_of_classes_properties_and_geometry_paths() {
    for engine in [Engine::new_tripletrail(), Engine::new_oxigraph().unwrap()] {
        engine.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
        let profile = engine.dataset_profile(ProfileOptions::default()).unwrap();
        assert_eq!(profile.classes_total, 3);
        let class = |name: &str| {
            profile
                .classes
                .iter()
                .find(|c| c.class.as_str() == format!("{EX}{name}"))
                .unwrap_or_else(|| panic!("{name} in {profile:?}"))
        };

        let signal = class("Signal");
        assert_eq!(
            profile.classes[0].class, signal.class,
            "most instances first"
        );
        assert_eq!((signal.instances, signal.sampled), (30, 30));
        assert_eq!(signal.label.as_deref(), Some("Signal"));
        assert_eq!(signal.superclasses[0].as_str(), format!("{EX}Asset"));
        assert_eq!(
            signal.label_properties[0].as_str(),
            "http://www.w3.org/2004/02/skos/core#prefLabel"
        );
        let prop = |name: &str| {
            signal
                .properties
                .iter()
                .find(|p| p.property.as_str().ends_with(name))
                .unwrap()
        };
        let status = prop("status");
        assert_eq!(status.label.as_deref(), Some("status"));
        assert_eq!((status.min_count, status.max_count), (1, 1));
        assert_eq!(status.distinct_values.as_ref().map(Vec::len), Some(3));
        let height = prop("height");
        assert_eq!((height.instances, height.min_count), (15, 0));
        assert!(height.datatypes[0].0.as_str().ends_with("#integer"));
        assert!(
            height.distinct_values.is_none(),
            "15 values: not a code list"
        );
        let track = prop("onTrack");
        assert_eq!(track.classes[0].0.as_str(), format!("{EX}Track"));
        assert_eq!(track.iris, 30);
        let label = prop("prefLabel");
        assert_eq!(label.languages[0], ("en".to_owned(), 30));
        // The direct geometry, through a blank node; and through the track too, longer.
        let geometry = &signal.geometries[0];
        assert_eq!(
            geometry
                .path
                .iter()
                .map(|p| p.as_str().rsplit(['#', '/']).next().unwrap())
                .collect::<Vec<_>>(),
            ["hasGeometry", "asWKT"]
        );
        assert_eq!(geometry.instances, 20, "path-searched instances");
        assert_eq!(geometry.types, [("POINT".to_owned(), 20)]);

        let track = class("Track");
        let path: Vec<&str> = track.geometries[0]
            .path
            .iter()
            .map(|p| p.as_str().rsplit(['#', '/']).next().unwrap())
            .collect();
        assert_eq!(path, ["netReference", "hasGeometry", "asWKT"]);
        assert_eq!(track.geometries[0].types[0].0, "LINESTRING");
        assert_eq!(
            track.label_properties[0].as_str(),
            "http://www.w3.org/2000/01/rdf-schema#label"
        );
    }
}

#[test]
fn profile_options_bound_the_work_and_the_profile_is_kept() {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    let small = ProfileOptions {
        sample: 4,
        path_sample: 2,
        max_classes: 1,
    };
    let profile = engine.dataset_profile(small).unwrap();
    assert_eq!(profile.classes.len(), 1);
    assert_eq!(profile.classes_total, 3);
    assert_eq!(profile.classes[0].sampled, 4);
    assert_eq!(profile.classes[0].geometries[0].instances, 2);
    // Kept until the data changes.
    let again = engine.dataset_profile(small).unwrap();
    assert!(std::sync::Arc::ptr_eq(&profile, &again));
    engine
        .update("INSERT DATA { <http://example.org/x> a <http://example.org/Track> }")
        .unwrap();
    let after = engine.dataset_profile(small).unwrap();
    assert!(!std::sync::Arc::ptr_eq(&profile, &after));
}

#[test]
fn a_hub_is_read_up_to_the_cap_and_a_cancelled_profile_stops() {
    // One instance with more links than are read per node: its counts are a lower bound.
    let mut ttl = String::from("@prefix ex: <http://example.org/> .\nex:hub a ex:Hub .\n");
    for i in 0..tripletrail::MAX_LINKS_PER_NODE + 100 {
        ttl.push_str(&format!("ex:hub ex:member ex:m{i} .\n"));
    }
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let profile = engine.dataset_profile(ProfileOptions::default()).unwrap();
    let member = profile.classes[0]
        .properties
        .iter()
        .find(|p| p.property.as_str().ends_with("member"))
        .unwrap();
    assert!(member.max_count < tripletrail::MAX_LINKS_PER_NODE as u64);
    assert!(member.max_count > 3000, "{}", member.max_count);

    let token = oxigraph::sparql::CancellationToken::new();
    token.cancel();
    let fresh = Engine::new_tripletrail();
    fresh.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    let cancelled = fresh.with_cancellation_token(token);
    assert!(
        cancelled
            .dataset_profile(ProfileOptions::default())
            .is_err()
    );
    // Nothing half-made was kept.
    assert!(fresh.dataset_profile(ProfileOptions::default()).is_ok());
}
