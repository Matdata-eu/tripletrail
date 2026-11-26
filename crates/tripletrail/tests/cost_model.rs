//! The spatial rewrite's cost model: each choice flips as the data changes, and the answers
//! stay those of a full scan.

use tripletrail::Engine;
use tripletrail::oxigraph::model::vocab::rdf;
use tripletrail::oxigraph::model::{GraphName, Literal, NamedNode, Quad, Term};
use tripletrail::oxigraph::sparql::QueryResults;
use tripletrail::spatial::Decision;

const PREFIXES: &str = r#"
PREFIX ex: <http://example.com/>
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
"#;
const EX: &str = "http://example.com/";
const GEO: &str = "http://www.opengis.net/ont/geosparql#";

fn nn(s: String) -> NamedNode {
    NamedNode::new_unchecked(s)
}

/// A feature `ex:{id}` of class `ex:{class}` with a point geometry, and a name if given.
fn feature(id: &str, class: &str, x: f64, y: f64, name: Option<&str>) -> Vec<Quad> {
    let f = nn(format!("{EX}{id}"));
    let g = nn(format!("{EX}{id}/geom"));
    let quad =
        |s: &NamedNode, p: String, o: Term| Quad::new(s.clone(), nn(p), o, GraphName::DefaultGraph);
    let mut out = vec![
        quad(
            &f,
            rdf::TYPE.as_str().into(),
            nn(format!("{EX}{class}")).into(),
        ),
        quad(&f, format!("{GEO}hasGeometry"), g.clone().into()),
        quad(
            &g,
            format!("{GEO}asWKT"),
            Literal::new_typed_literal(format!("POINT({x} {y})"), nn(format!("{GEO}wktLiteral")))
                .into(),
        ),
    ];
    if let Some(name) = name {
        out.push(quad(&f, format!("{EX}name"), Literal::from(name).into()));
    }
    out
}

/// A square zone `ex:{id}` of side `d` at `(x, y)`.
fn zone(id: &str, x: f64, y: f64, d: f64) -> Vec<Quad> {
    let f = nn(format!("{EX}{id}"));
    let g = nn(format!("{EX}{id}/geom"));
    let (x2, y2) = (x + d, y + d);
    vec![
        Quad::new(
            f.clone(),
            rdf::TYPE,
            nn(format!("{EX}Zone")),
            GraphName::DefaultGraph,
        ),
        Quad::new(
            f,
            nn(format!("{GEO}hasGeometry")),
            g.clone(),
            GraphName::DefaultGraph,
        ),
        Quad::new(
            g,
            nn(format!("{GEO}asWKT")),
            Literal::new_typed_literal(
                format!("POLYGON(({x} {y}, {x2} {y}, {x2} {y2}, {x} {y2}, {x} {y}))"),
                nn(format!("{GEO}wktLiteral")),
            ),
            GraphName::DefaultGraph,
        ),
    ]
}

/// `n` points of class `class` on a grid of `per_row` columns from `(x0, y0)`, step `step`.
fn grid(
    prefix: &str,
    class: &str,
    n: usize,
    per_row: usize,
    x0: f64,
    y0: f64,
    step: f64,
) -> Vec<Quad> {
    (0..n)
        .flat_map(|i| {
            let (x, y) = (
                x0 + (i % per_row) as f64 * step,
                y0 + (i / per_row) as f64 * step,
            );
            feature(&format!("{prefix}{i}"), class, x, y, None)
        })
        .collect()
}

fn sorted(e: &Engine, q: &str) -> Vec<String> {
    let QueryResults::Solutions(s) = e.query(&format!("{PREFIXES}{q}")).unwrap() else {
        panic!()
    };
    let mut out: Vec<String> = s
        .map(|r| {
            r.unwrap()
                .iter()
                .map(|(_, t)| t.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    out.sort();
    out
}

/// Runs `q` without the rewrite, with the rewrite but without the cost model, and with both;
/// checks the three agree, and returns the rows and the cost model's decisions.
fn check(e: &Engine, q: &str) -> (usize, Vec<Decision>) {
    e.set_spatial_rewrite(false);
    let scan = sorted(e, q);
    e.set_spatial_rewrite(true);
    let (n, decisions) = check_fixed(e, q);
    assert_eq!(scan, sorted(e, q), "the rewrite changed the result of {q}");
    (n, decisions)
}

/// [`check`] for joins too large to scan: the cost model against the fixed rules.
fn check_fixed(e: &Engine, q: &str) -> (usize, Vec<Decision>) {
    e.set_cost_model(false);
    let fixed = sorted(e, q);
    assert!(e.last_spatial_stats().decisions.is_empty());
    e.set_cost_model(true);
    let costed = sorted(e, q);
    let decisions = e.last_spatial_stats().decisions;
    assert_eq!(fixed, costed, "the cost model changed the result of {q}");
    (costed.len(), decisions)
}

fn constant(decisions: &[Decision]) -> (usize, f64, bool) {
    match decisions.first() {
        Some(Decision::Constant {
            candidates,
            rows,
            inject,
            ..
        }) => (*candidates, *rows, *inject),
        other => panic!("no constant decision: {other:?}"),
    }
}

const ALL: &str = r#""POLYGON((-1 -1, 50 -1, 50 50, -1 50, -1 -1))"^^geo:wktLiteral"#;
const CORNER: &str =
    r#""POLYGON((-0.5 -0.5, 2.5 -0.5, 2.5 2.5, -0.5 2.5, -0.5 -0.5))"^^geo:wktLiteral"#;

#[test]
fn constant_test_counts_the_rows_a_small_triple_fans_out_to() {
    // #209: 2,000 stations run by 20 operators in 4 countries. `?o ex:country ?c` has the
    // fewest matches (20), but the stations it reaches bind 2,000 geometries: the polygon's
    // ~120 candidates are injected rather than tested 2,000 times by the FILTER.
    let e = Engine::new_tripletrail();
    let mut quads = grid("s", "Station", 2000, 50, 0.0, 0.0, 1.0);
    let quad = |s: String, p: &str, o: String| {
        Quad::new(
            nn(s),
            nn(format!("{EX}{p}")),
            nn(o),
            GraphName::DefaultGraph,
        )
    };
    for i in 0..2000 {
        quads.push(quad(
            format!("{EX}s{i}"),
            "operator",
            format!("{EX}op{}", i % 20),
        ));
    }
    for j in 0..20 {
        quads.push(quad(
            format!("{EX}op{j}"),
            "country",
            format!("{EX}c{}", j % 4),
        ));
    }
    e.load_quads(quads).unwrap();
    let window =
        r#""POLYGON((-0.5 -0.5, 10.5 -0.5, 10.5 10.5, -0.5 10.5, -0.5 -0.5))"^^geo:wktLiteral"#;
    let joined = format!(
        "SELECT ?c (COUNT(*) AS ?n) WHERE {{ ?s ex:operator ?o . ?o ex:country ?c . \
         ?s geo:hasGeometry/geo:asWKT ?w FILTER(geof:sfIntersects(?w, {window})) }} GROUP BY ?c"
    );
    let (n, decisions) = check(&e, &joined);
    assert_eq!(n, 4);
    let (candidates, rows, inject) = constant(&decisions);
    assert!(candidates > 32 && rows >= 1000.0 && inject, "{decisions:?}");
    // The same answers as the sub-select that starts from the polygon.
    let nested = format!(
        "SELECT ?c (COUNT(*) AS ?n) WHERE {{ {{ SELECT ?s WHERE {{ ?s geo:hasGeometry/geo:asWKT ?w \
         FILTER(geof:sfIntersects(?w, {window})) }} }} ?s ex:operator ?o . ?o ex:country ?c }} GROUP BY ?c"
    );
    assert_eq!(sorted(&e, &joined), sorted(&e, &nested));
}

#[test]
fn constant_test_is_left_to_the_filter_when_the_rest_binds_fewer_rows() {
    for e in [Engine::new_tripletrail(), Engine::new_oxigraph().unwrap()] {
        // 400 stations in a 20 x 20 grid, one of them named.
        let mut quads = grid("s", "Station", 400, 20, 0.0, 0.0, 1.0);
        quads.extend(feature("named", "Station", 3.5, 3.5, Some("Central")));
        e.load_quads(quads).unwrap();
        let q = format!(
            "SELECT ?s WHERE {{ ?s ex:name \"Central\" ; geo:hasGeometry/geo:asWKT ?w FILTER(geof:sfIntersects(?w, {ALL})) }}"
        );
        // The polygon takes in every geometry; the name one row: no VALUES block.
        let (n, decisions) = check(&e, &q);
        assert_eq!(n, 1);
        let (candidates, rows, inject) = constant(&decisions);
        assert_eq!(
            (candidates, rows, inject),
            (401, 1.0, false),
            "{decisions:?}"
        );
        assert_eq!(e.last_spatial_stats().skipped, 1);
        assert_eq!(e.last_spatial_stats().filters, 0);
        let explained = e.explain(&format!("{PREFIXES}{q}")).unwrap();
        assert!(
            explained.contains(r#""skipped":1"#) && explained.contains("leave to FILTER ?w"),
            "{explained}"
        );

        // 2000 more stations with that name, outside the polygon: the name now binds more rows
        // than the polygon has candidates, so the candidates are injected.
        let far: Vec<Quad> = (0..2000)
            .flat_map(|i| {
                feature(
                    &format!("far{i}"),
                    "Station",
                    100.0 + i as f64 * 0.01,
                    80.0,
                    Some("Central"),
                )
            })
            .collect();
        e.load_quads(far).unwrap();
        let (n, decisions) = check(&e, &q);
        assert_eq!(n, 1);
        let (candidates, rows, inject) = constant(&decisions);
        assert_eq!(
            (candidates, rows, inject),
            (401, 2001.0, true),
            "{decisions:?}"
        );
        assert_eq!(e.last_spatial_stats().filters, 1);
    }
}

#[test]
fn constant_test_is_left_to_the_filter_when_the_window_fills_up() {
    for e in [Engine::new_tripletrail(), Engine::new_oxigraph().unwrap()] {
        e.load_quads(grid("s", "Station", 400, 20, 0.0, 0.0, 1.0))
            .unwrap();
        let q = format!(
            "SELECT ?s WHERE {{ ?s a ex:Station ; geo:hasGeometry/geo:asWKT ?w FILTER(geof:sfIntersects(?w, {CORNER})) }}"
        );
        // 9 of 400 stations in the corner: injected.
        let (n, decisions) = check(&e, &q);
        assert_eq!(n, 9);
        assert_eq!(constant(&decisions), (9, 400.0, true));
        // 400 more stations, all in the corner: half the rows are now candidates.
        e.load_quads(grid("c", "Station", 400, 20, 0.0, 0.0, 0.1))
            .unwrap();
        let (n, decisions) = check(&e, &q);
        assert_eq!(n, 409);
        let (candidates, rows, inject) = constant(&decisions);
        assert!(
            !inject && candidates > 400 && rows == 800.0,
            "{decisions:?}"
        );
    }
}

fn join(decisions: &[Decision]) -> (String, bool, usize) {
    decisions
        .iter()
        .find_map(|d| match d {
            Decision::Join {
                probe,
                by_estimate,
                evaluated,
                ..
            } => Some((probe.as_str().to_owned(), *by_estimate, *evaluated)),
            // On term ids, both sides are evaluated (cheaply) and the smaller probes.
            Decision::IdJoin { probe, .. } => Some((probe.as_str().to_owned(), false, 2)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no join decision: {decisions:?}"))
}

#[test]
fn join_probes_from_the_side_estimated_smaller() {
    for e in [Engine::new_tripletrail(), Engine::new_oxigraph().unwrap()] {
        // 400 spots, 3 zones.
        let mut quads = grid("p", "Spot", 400, 20, 0.0, 0.0, 0.05);
        for i in 0..3 {
            quads.extend(zone(&format!("z{i}"), i as f64 * 0.3, 0.1, 0.2));
        }
        e.load_quads(quads).unwrap();
        let q = "SELECT ?p ?z WHERE { ?p a ex:Spot ; geo:hasGeometry/geo:asWKT ?pw . \
                 ?z a ex:Zone ; geo:hasGeometry/geo:asWKT ?zw FILTER(geof:sfWithin(?pw, ?zw)) }";
        let ids = e.store().kind() == "tripletrail";
        let (n, decisions) = check(&e, q);
        assert!(n > 0);
        if ids {
            // On term ids both sides are evaluated, cheaply, and the zones probe.
            assert_eq!(on_ids(&decisions), Some(n), "{decisions:?}");
            assert_eq!(join(&decisions).0, "zw", "{decisions:?}");
        } else {
            // Only the zones were evaluated, and probed.
            assert_eq!(join(&decisions), ("zw".into(), true, 1), "{decisions:?}");
            assert_eq!(e.last_spatial_stats().sides_evaluated, 1);
        }
        // The fixed rules evaluate both sides.
        e.set_cost_model(false);
        sorted(&e, q);
        assert_eq!(e.last_spatial_stats().sides_evaluated, 2);
        e.set_cost_model(true);

        // 2000 more zones, away from the spots: now the spots are the small side.
        let zones: Vec<Quad> = (0..2000)
            .flat_map(|i| {
                zone(
                    &format!("far{i}"),
                    10.0 + (i % 50) as f64,
                    10.0 + (i / 50) as f64,
                    0.5,
                )
            })
            .collect();
        e.load_quads(zones).unwrap();
        let (m, decisions) = check_fixed(&e, q);
        assert_eq!(m, n);
        if ids {
            assert_eq!(join(&decisions).0, "pw", "{decisions:?}");
        } else {
            assert_eq!(join(&decisions), ("pw".into(), true, 1), "{decisions:?}");
        }

        // Comparable sides: both evaluated, as without the cost model.
        let spots = grid("q", "Spot", 1700, 50, 20.0, 20.0, 0.01);
        e.load_quads(spots).unwrap();
        let (_, decisions) = check_fixed(&e, q);
        let (_, by_estimate, evaluated) = join(&decisions);
        assert!(!by_estimate && evaluated == 2, "{decisions:?}");
    }
}

#[test]
fn constant_tests_left_to_the_filter_still_narrow_a_join() {
    for e in [Engine::new_tripletrail(), Engine::new_oxigraph().unwrap()] {
        // 40 spots among 1000 other geometries, 3 zones; the window takes in everything but
        // only some of the spots.
        let mut quads = grid("p", "Spot", 40, 20, 0.0, 0.0, 0.05);
        quads.extend(grid("o", "Other", 1000, 50, 0.0, 0.0, 0.03));
        for i in 0..3 {
            quads.extend(zone(&format!("z{i}"), i as f64 * 0.3, 0.0, 0.2));
        }
        e.load_quads(quads).unwrap();
        let window = r#""POLYGON((-1 -1, 0.42 -1, 0.42 1, -1 1, -1 -1))"^^geo:wktLiteral"#;
        let q = format!(
            "SELECT ?p ?z WHERE {{ ?p a ex:Spot ; geo:hasGeometry/geo:asWKT ?pw . \
             ?z a ex:Zone ; geo:hasGeometry/geo:asWKT ?zw \
             FILTER(geof:sfIntersects(?pw, {window}) && geof:sfWithin(?pw, ?zw)) }}"
        );
        let (n, decisions) = check(&e, &q);
        assert!(n > 0);
        assert!(!constant(&decisions).2, "{decisions:?}");
        // The zones probe; the spots, 40 of 1043 indexed geometries, are evaluated too, so
        // the other geometries in the zones are skipped before their exact test (on term ids
        // both sides are evaluated anyway).
        let expected = if e.store().kind() == "tripletrail" {
            ("zw".into(), false, 2)
        } else {
            ("zw".into(), true, 2)
        };
        assert_eq!(join(&decisions), expected, "{decisions:?}");
        // Only spots passing the window test were paired with the zones: the third zone's
        // are not, though they are within it.
        let pairs = e.last_spatial_stats().candidates;
        let zoned = |extra: &str| {
            e.set_spatial_rewrite(false);
            let n = sorted(
                &e,
                &format!(
                    "SELECT ?w ?zw WHERE {{ ?p a ex:Spot ; geo:hasGeometry/geo:asWKT ?w . ?z a ex:Zone ; geo:hasGeometry/geo:asWKT ?zw FILTER(geof:sfWithin(?w, ?zw){extra}) }}"
                ),
            )
            .len();
            e.set_spatial_rewrite(true);
            n
        };
        let in_window = zoned(&format!(" && geof:sfIntersects(?w, {window})"));
        assert_eq!(pairs, in_window);
        assert!(zoned("") > in_window);
    }
}

#[test]
fn candidates_go_first_when_the_optimizer_would_start_elsewhere() {
    for e in [Engine::new_tripletrail(), Engine::new_oxigraph().unwrap()] {
        // 6000 features; 400 in the corner window.
        let mut quads = grid("f", "Feature", 400, 20, 0.0, 0.0, 0.1);
        quads.extend(grid("o", "Feature", 5600, 80, 5.0, 5.0, 0.5));
        e.load_quads(quads).unwrap();
        let q = format!(
            "SELECT ?f WHERE {{ ?f a ex:Feature ; geo:hasGeometry/geo:asWKT ?w FILTER(geof:sfIntersects(?w, {CORNER})) }}"
        );
        // 400 candidates: Oxigraph's optimizer starts from them anyway.
        let (n, decisions) = check(&e, &q);
        assert_eq!(n, 400);
        assert_eq!(decisions.len(), 1, "{decisions:?}");
        assert_eq!(e.last_spatial_stats().pinned, 0);
        // 1600 more in the window: the optimizer would start from `?f a ex:Feature`, which
        // it guesses at 1001 rows; there are 7600.
        e.load_quads(grid("g", "Feature", 1600, 40, 0.025, 0.025, 0.05))
            .unwrap();
        let (n, decisions) = check(&e, &q);
        assert_eq!(n, 2000);
        assert!(
            matches!(
                decisions.last(),
                Some(Decision::Pin { rows: 2000, start_rows, .. }) if *start_rows == 7600.0
            ),
            "{decisions:?}"
        );
        assert_eq!(e.last_spatial_stats().pinned, 1);
    }
}

#[test]
fn estimates_follow_writes() {
    for e in [Engine::new_tripletrail(), Engine::new_oxigraph().unwrap()] {
        e.load_quads(grid("s", "Station", 300, 20, 0.0, 0.0, 1.0))
            .unwrap();
        let wkt = nn(format!("{GEO}asWKT"));
        let station: Term = nn(format!("{EX}Station")).into();
        let count = |p: Option<&NamedNode>, o: Option<&Term>| {
            e.estimate_triples(None, p, o, false).unwrap()
        };
        assert_eq!(count(Some(&wkt), None), 300);
        assert_eq!(count(Some(&rdf::TYPE.into_owned()), Some(&station)), 300);
        for q in feature("x", "Station", 0.0, 0.0, None) {
            e.insert(&q).unwrap();
        }
        assert_eq!(count(Some(&wkt), None), 301);
        assert_eq!(count(Some(&rdf::TYPE.into_owned()), Some(&station)), 301);
        // The feature's triples go; its geometry's `geo:asWKT` stays.
        e.update(&format!("{PREFIXES} DELETE WHERE {{ ex:s0 ?p ?o }}"))
            .unwrap();
        assert_eq!(count(Some(&rdf::TYPE.into_owned()), Some(&station)), 300);
        assert_eq!(count(Some(&wkt), None), 301);
        e.update(&format!(
            "{PREFIXES} DELETE WHERE {{ ex:s1 a ?c ; geo:hasGeometry ?g . ?g geo:asWKT ?w }}"
        ))
        .unwrap();
        assert_eq!(count(Some(&wkt), None), 300);
        // Writes made directly on the store are seen after `invalidate`.
        e.store()
            .remove(&feature("s2", "Station", 2.0, 0.0, None)[2])
            .unwrap();
        assert_eq!(count(Some(&wkt), None), 300);
        e.invalidate();
        assert_eq!(count(Some(&wkt), None), 299);
    }
}

/// Whether the join decision chose the sweep-line join.
/// Whether the join was answered on term ids (the tripletrail store), and how many rows the
/// table it handed to the evaluation had.
fn on_ids(decisions: &[Decision]) -> Option<usize> {
    decisions.iter().find_map(|d| match d {
        Decision::IdJoin { rows, .. } => Some(*rows),
        _ => None,
    })
}

fn swept(decisions: &[Decision]) -> bool {
    decisions
        .iter()
        .find_map(|d| match d {
            Decision::Join { sweep, .. } => Some(*sweep),
            Decision::IdJoin { .. } => Some(true),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no join decision: {decisions:?}"))
}

/// A point geometry `ex:{id}/geom` in another CRS (Web Mercator), of no feature.
fn projected(id: &str, x: f64, y: f64) -> Quad {
    Quad::new(
        nn(format!("{EX}{id}/geom")),
        nn(format!("{GEO}asWKT")),
        Literal::new_typed_literal(
            format!("<http://www.opengis.net/def/crs/EPSG/0/3857> POINT({x} {y})"),
            nn(format!("{GEO}wktLiteral")),
        ),
        GraphName::DefaultGraph,
    )
}

/// Runs `q` with the cost model, its joins answered by the index nested loop and then by the
/// sweep-line join where the cost model picks it; checks both agree, and returns the rows and
/// the decisions with the sweep.
fn check_sweep(e: &Engine, q: &str) -> (usize, Vec<Decision>) {
    e.set_sweep_join(false);
    let nested = sorted(e, q);
    assert!(!swept(&e.last_spatial_stats().decisions));
    e.set_sweep_join(true);
    let swept = sorted(e, q);
    assert_eq!(nested, swept, "the sweep changed the result of {q}");
    (swept.len(), e.last_spatial_stats().decisions)
}

#[test]
fn large_joins_are_swept_and_match_the_nested_loop() {
    for e in [Engine::new_tripletrail(), Engine::new_oxigraph().unwrap()] {
        // 2500 spots and 2500 small zones, one around each spot, near 60° N; 2500 posts about
        // 28 m from the spots; and points in another CRS at the zones' coordinates, which
        // must pair with nothing.
        let mut quads = grid("p", "Spot", 2500, 50, 10.0, 60.0, 0.001);
        quads.extend(grid("q", "Post", 2500, 50, 10.0003, 60.0002, 0.001));
        for i in 0..2500 {
            let (x, y) = (
                10.0 + (i % 50) as f64 * 0.001 - 0.0002,
                60.0 + (i / 50) as f64 * 0.001 - 0.0002,
            );
            quads.extend(zone(&format!("z{i}"), x, y, 0.0006));
            quads.push(projected(&format!("m{i}"), x, y));
            // Each spot is linked to a zone, which joins the two sides; every 50th to its own.
            quads.push(Quad::new(
                nn(format!("{EX}p{i}")),
                nn(format!("{EX}in")),
                nn(format!("{EX}z{}", (i * 51) % 2500)),
                GraphName::DefaultGraph,
            ));
        }
        e.load_quads(quads).unwrap();

        // Separate sides of comparable size: both evaluated, then swept. (Without the cost
        // model the evaluator would build their cartesian product: see below.)
        let within = "SELECT ?p ?z WHERE { ?p a ex:Spot ; geo:hasGeometry/geo:asWKT ?pw . \
                      ?z a ex:Zone ; geo:hasGeometry/geo:asWKT ?zw FILTER(geof:sfWithin(?pw, ?zw)) }";
        let (n, decisions) = check_sweep(&e, within);
        assert_eq!(n, 2500);
        assert!(swept(&decisions), "{decisions:?}");
        assert_eq!(join(&decisions).2, 2, "{decisions:?}");
        assert_eq!(e.last_spatial_stats().sweeps, 1);
        if e.store().kind() == "tripletrail" {
            // On term ids: the joined rows go to the evaluation as one table, in place of
            // both sides.
            assert_eq!(on_ids(&decisions), Some(2500), "{decisions:?}");
            assert_eq!(e.last_spatial_stats().materialized, 1);
        } else {
            // Only the pairs connect the two sides: Oxigraph's optimizer would start from
            // their cartesian product, so the pairs are pinned first. (Each row of a side is
            // in one pair: looked up from the pairs, not bound from the side's rows.)
            assert_eq!(e.last_spatial_stats().materialized, 0);
            assert!(
                matches!(
                    decisions.last(),
                    Some(Decision::Pin { rows: 2500, start_rows, .. }) if *start_rows == 2500.0 * 2500.0
                ),
                "{decisions:?}"
            );
        }

        // A distance in metres on geographic coordinates: the margin is in degrees, wider in
        // longitude at this latitude. Each spot has one post within 40 m (about 28 m away; the
        // next is 45 m away).
        let near = "SELECT ?p ?q WHERE { ?p a ex:Spot ; geo:hasGeometry/geo:asWKT ?pw . \
                    ?q a ex:Post ; geo:hasGeometry/geo:asWKT ?qw FILTER(geof:distance(?pw, ?qw) < 40) }";
        let (n, decisions) = check_sweep(&e, near);
        assert_eq!(n, 2500);
        assert!(swept(&decisions), "{decisions:?}");

        // Sides joined by `ex:in`: every indexed geometry against every other, swept; and
        // the same rows as without the cost model.
        let linked = "SELECT ?p ?z WHERE { ?p ex:in ?z ; geo:hasGeometry/geo:asWKT ?pw . \
                      ?z geo:hasGeometry/geo:asWKT ?zw FILTER(geof:sfIntersects(?pw, ?zw)) }";
        let (n, decisions) = check_sweep(&e, linked);
        assert_eq!(check_fixed(&e, linked).0, n);
        assert_eq!(n, 50);
        assert!(swept(&decisions), "{decisions:?}");
        assert_eq!(e.last_spatial_stats().sides_evaluated, 0);

        // A relation triple (Query Rewrite Extension) goes through the same join.
        let relation = "SELECT ?p ?z WHERE { ?p a ex:Spot . ?z a ex:Zone . ?p geo:sfWithin ?z }";
        let (n, decisions) = check_sweep(&e, relation);
        assert_eq!(n, 2500);
        assert!(swept(&decisions), "{decisions:?}");
    }
}

#[test]
fn small_by_large_joins_use_the_nested_loop() {
    for e in [Engine::new_tripletrail(), Engine::new_oxigraph().unwrap()] {
        let mut quads = grid("p", "Spot", 3000, 60, 0.0, 0.0, 0.01);
        for i in 0..40 {
            quads.extend(zone(&format!("z{i}"), i as f64 * 0.012, 0.1, 0.05));
        }
        e.load_quads(quads).unwrap();
        let q = "SELECT ?p ?z WHERE { ?p a ex:Spot ; geo:hasGeometry/geo:asWKT ?pw . \
                 ?z a ex:Zone ; geo:hasGeometry/geo:asWKT ?zw FILTER(geof:sfWithin(?pw, ?zw)) }";
        let (n, decisions) = check(&e, q);
        assert!(n > 0);
        assert_eq!(join(&decisions).0, "zw", "{decisions:?}");
        if e.store().kind() == "tripletrail" {
            // On term ids the sweep of both (cheaply evaluated) sides is the only strategy.
            assert_eq!(on_ids(&decisions), Some(n), "{decisions:?}");
        } else {
            assert!(!swept(&decisions), "{decisions:?}");
            assert_eq!(e.last_spatial_stats().sweeps, 0);
        }
    }
}

#[test]
fn joins_bind_a_side_from_its_rows_with_the_same_answers() {
    for e in [Engine::new_tripletrail(), Engine::new_oxigraph().unwrap()] {
        // 400 spots on a grid and 25 zones over them, 16 spots each (rows of the zone side
        // are in many pairs); spot s0 and s1 share one point (two geometry nodes, one
        // literal), spot s2 has two geometries, and every zone has a name.
        let mut quads = grid("s", "Spot", 400, 20, 10.0, 60.0, 0.001);
        quads.extend(feature("s1", "Spot", 10.0, 60.0, None));
        quads.extend(
            feature("s2b", "Spot", 10.002, 60.0, None)
                .into_iter()
                .skip(2),
        );
        quads.push(Quad::new(
            nn(format!("{EX}s2")),
            nn(format!("{GEO}hasGeometry")),
            nn(format!("{EX}s2b/geom")),
            GraphName::DefaultGraph,
        ));
        for i in 0..25 {
            let (x, y) = (10.0 + (i % 5) as f64 * 0.004, 60.0 + (i / 5) as f64 * 0.004);
            quads.extend(zone(&format!("z{i}"), x - 0.0005, y - 0.0005, 0.004));
            quads.push(Quad::new(
                nn(format!("{EX}z{i}")),
                nn(format!("{EX}name")),
                Literal::from(format!("zone {i}")),
                GraphName::DefaultGraph,
            ));
        }
        e.load_quads(quads).unwrap();
        let sides = "?s a ex:Spot ; geo:hasGeometry/geo:asWKT ?sw . \
                     ?z a ex:Zone ; ex:name ?name ; geo:hasGeometry/geo:asWKT ?zw \
                     FILTER(geof:sfWithin(?sw, ?zw))";
        for q in [
            // The zone geometry is not used further: left out of the block.
            format!("SELECT ?s ?z ?name WHERE {{ {sides} }}"),
            // Used further: kept.
            format!("SELECT ?s ?zw ?z WHERE {{ {sides} }}"),
            format!("SELECT * WHERE {{ {sides} }}"),
            format!("SELECT (COUNT(*) AS ?n) WHERE {{ {sides} }}"),
            format!("SELECT ?s ?z WHERE {{ {sides} FILTER(STRLEN(STR(?zw)) > 0) }}"),
            format!("SELECT ?z (COUNT(?s) AS ?n) WHERE {{ {sides} }} GROUP BY ?z"),
        ] {
            let (n, _) = check(&e, &q);
            assert!(n > 0, "{q}");
            assert_eq!(e.last_spatial_stats().materialized, 1, "{q}");
        }
        // Every spot is in one zone; s1 has a second point (in z0), s2 a second geometry.
        let rows = sorted(&e, &format!("SELECT ?s ?z WHERE {{ {sides} }}"));
        assert_eq!(rows.len(), 402, "{rows:?}");
    }
}

#[test]
fn joins_on_ids_answer_updates_and_aggregates_as_the_evaluator_does() {
    for e in [Engine::new_tripletrail(), Engine::new_oxigraph().unwrap()] {
        let mut quads = grid("p", "Spot", 400, 20, 0.0, 0.0, 0.05);
        for i in 0..3 {
            quads.extend(zone(&format!("z{i}"), i as f64 * 0.3, 0.1, 0.2));
        }
        e.load_quads(quads).unwrap();
        let q = "SELECT ?z (COUNT(?p) AS ?n) (SUM(geof:area(?zw)) AS ?a) WHERE { \
                 ?p a ex:Spot ; geo:hasGeometry/geo:asWKT ?pw . \
                 ?z a ex:Zone ; geo:hasGeometry/geo:asWKT ?zw FILTER(geof:sfWithin(?pw, ?zw)) } \
                 GROUP BY ?z";
        let rewritten = sorted(&e, q);
        e.set_spatial_rewrite(false);
        let scanned = sorted(&e, q);
        e.set_spatial_rewrite(true);
        assert_eq!(rewritten, scanned);
        assert!(!rewritten.is_empty());
        // The same join in an update's WHERE.
        e.update(&format!(
            "{PREFIXES}INSERT {{ ?p ex:inZone ?z }} WHERE {{ \
             ?p a ex:Spot ; geo:hasGeometry/geo:asWKT ?pw . \
             ?z a ex:Zone ; geo:hasGeometry/geo:asWKT ?zw FILTER(geof:sfWithin(?pw, ?zw)) }}"
        ))
        .unwrap();
        let inserted = sorted(&e, "SELECT ?p ?z WHERE { ?p ex:inZone ?z }");
        e.set_spatial_rewrite(false);
        let expected = sorted(
            &e,
            "SELECT ?p ?z WHERE { ?p a ex:Spot ; geo:hasGeometry/geo:asWKT ?pw . \
             ?z a ex:Zone ; geo:hasGeometry/geo:asWKT ?zw FILTER(geof:sfWithin(?pw, ?zw)) }",
        );
        e.set_spatial_rewrite(true);
        assert_eq!(inserted, expected);
        // No table outlives its query: a later query naming one finds nothing.
        assert!(sorted(&e, "SELECT * WHERE { ?r <urn:tripletrail:table:1:0> ?v }").is_empty());
    }
}

#[test]
fn values_of_classes_count_the_instances_of_those_classes() {
    // 20 classes of 250 points each, on the same 50 x 5 grid: the corner window holds 9 points
    // of each class, 180 candidates (#463).
    for e in [Engine::new_tripletrail(), Engine::new_oxigraph().unwrap()] {
        let quads: Vec<Quad> = (0..20)
            .flat_map(|c| {
                grid(
                    &format!("c{c}_"),
                    &format!("C{c}"),
                    250,
                    50,
                    c as f64 * 0.01,
                    0.0,
                    1.0,
                )
            })
            .collect();
        e.load_quads(quads).unwrap();
        let classes: String = (0..20).map(|c| format!("ex:C{c} ")).collect();
        let q = format!(
            "SELECT ?c ?f WHERE {{ VALUES ?c {{ {classes}}} ?f a ?c ; geo:hasGeometry/geo:asWKT ?w FILTER(geof:sfIntersects(?w, {CORNER})) }}"
        );
        // The rows are the instances of the 20 classes, not one per class: the 180 candidates
        // are injected.
        let (n, decisions) = check(&e, &q);
        assert_eq!(n, 180);
        let (candidates, rows, inject) = constant(&decisions);
        assert_eq!(
            (candidates, rows, inject),
            (180, 5000.0, true),
            "{decisions:?}"
        );
        // Classes without instances add nothing.
        let q = format!(
            "SELECT ?c ?f WHERE {{ VALUES ?c {{ ex:C0 ex:Missing \"text\" }} ?f a ?c ; geo:hasGeometry/geo:asWKT ?w FILTER(geof:sfIntersects(?w, {CORNER})) }}"
        );
        let (n, decisions) = check(&e, &q);
        assert_eq!(n, 9);
        let (_, rows, _) = constant(&decisions);
        assert_eq!(rows, 250.0, "{decisions:?}");
    }
}

#[test]
fn joins_of_sides_in_named_graphs_run_on_ids() {
    // Spots in one named graph and zones in another (#703), as Geographica keeps each
    // dataset in its graph; a copy of the zones, shifted, in a third graph and in the default
    // graph, which the sides must not read.
    let in_graph = |quads: Vec<Quad>, g: &str| -> Vec<Quad> {
        let g = GraphName::NamedNode(nn(format!("{EX}{g}")));
        quads
            .into_iter()
            .map(|q| Quad::new(q.subject, q.predicate, q.object, g.clone()))
            .collect()
    };
    let e = Engine::new_tripletrail();
    let mut quads = in_graph(grid("p", "Spot", 900, 30, 10.0, 60.0, 0.001), "spots");
    for i in 0..900 {
        let (x, y) = (
            10.0 + (i % 30) as f64 * 0.001 - 0.0002,
            60.0 + (i / 30) as f64 * 0.001 - 0.0002,
        );
        // Only every third spot has its zone in the zones graph.
        if i % 3 == 0 {
            quads.extend(in_graph(zone(&format!("z{i}"), x, y, 0.0006), "zones"));
        }
        quads.extend(in_graph(zone(&format!("w{i}"), x, y, 0.0006), "other"));
        quads.extend(zone(&format!("d{i}"), x, y, 0.0006));
    }
    e.load_quads(quads).unwrap();
    let q = "SELECT ?p ?z WHERE { GRAPH ex:spots { ?p geo:hasGeometry/geo:asWKT ?pw } \
             GRAPH ex:zones { ?z geo:hasGeometry/geo:asWKT ?zw } FILTER(geof:sfWithin(?pw, ?zw)) }";
    let (n, decisions) = check(&e, q);
    assert_eq!(n, 300);
    assert_eq!(on_ids(&decisions), Some(300), "{decisions:?}");
    // A graph the store does not hold: no rows.
    let none = q.replace("ex:zones", "ex:nowhere");
    let (n, _) = check(&e, &none);
    assert_eq!(n, 0);
}

/// A spatial join in a window service's sub-select (#857), rewritten to a join on term ids,
/// answers what the same join answers in the main query.
#[test]
fn joins_on_ids_in_a_service_sub_select() {
    let e = Engine::new_tripletrail();
    let mut quads = grid("p", "Spot", 3000, 60, 0.0, 0.0, 0.01);
    for i in 0..40 {
        quads.extend(zone(&format!("z{i}"), i as f64 * 0.012, 0.1, 0.05));
    }
    e.load_quads(quads).unwrap();
    let join = "?p a ex:Spot ; geo:hasGeometry/geo:asWKT ?pw . \
                ?z a ex:Zone ; geo:hasGeometry/geo:asWKT ?zw FILTER(geof:sfWithin(?pw, ?zw))";
    let main = sorted(&e, &format!("SELECT ?p ?z WHERE {{ {join} }}"));
    assert!(on_ids(&e.last_spatial_stats().decisions).is_some());
    assert!(!main.is_empty());
    let window = sorted(
        &e,
        &format!(
            "SELECT ?p ?z WHERE {{ SERVICE <http://w3id.org/tripletrail/window> {{
               [] <http://w3id.org/tripletrail/window#orderBy> ?p ;
                  <http://w3id.org/tripletrail/window#rowNumber> ?n .
               {{ SELECT ?p ?z WHERE {{ {join} }} }} }} }}"
        ),
    );
    assert!(on_ids(&e.last_spatial_stats().decisions).is_some());
    assert_eq!(window, main);
}

/// With the union default graph, the estimates count every graph, as the query reads them
/// (#89): data in named graphs only no longer looks empty, so a small window injects its
/// candidates instead of leaving every geometry to the FILTER.
#[test]
fn the_union_default_graph_estimates_every_graph() {
    let e = Engine::new_tripletrail();
    let in_graph = |quads: Vec<Quad>, g: usize| -> Vec<Quad> {
        let g = nn(format!("{EX}graph{g}"));
        quads
            .into_iter()
            .map(|q| Quad::new(q.subject, q.predicate, q.object, g.clone()))
            .collect()
    };
    e.load_quads(in_graph(grid("a", "Station", 400, 20, 0.0, 0.0, 1.0), 1))
        .unwrap();
    e.load_quads(in_graph(grid("b", "Station", 400, 20, 30.0, 0.0, 1.0), 2))
        .unwrap();
    e.set_union_default_graph(true);
    let q = r#"SELECT ?x WHERE { ?x geo:asWKT ?w .
        FILTER(geof:sfIntersects(?w, "POLYGON((2.5 2.5, 4.5 2.5, 4.5 4.5, 2.5 4.5, 2.5 2.5))"^^geo:wktLiteral)) }"#;
    let (n, decisions) = check(&e, q);
    assert_eq!(n, 4);
    let (candidates, rows, inject) = constant(&decisions);
    assert!(inject, "{decisions:?}");
    assert_eq!(rows, 800.0, "{decisions:?}");
    assert!(candidates < 20, "{decisions:?}");
    assert_eq!(e.last_spatial_stats().skipped, 0);
    // Without the union, the default graph is empty and the window answers nothing.
    e.set_union_default_graph(false);
    assert_eq!(check(&e, q).0, 0);
}
