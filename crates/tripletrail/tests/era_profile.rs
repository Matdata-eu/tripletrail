//! The ERA micro-topology profile respects switch navigability.

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::model::Term;
use tripletrail::oxigraph::sparql::QueryResults;

const PROFILE: &str = include_str!("../../../profiles/era-micro.rq");
const REVERSING: &str = include_str!("../../../profiles/era-micro-reversing.rq");

/// A switch at the end of trunk T with legs A and B (both attached at their origin).
/// T-A and T-B are navigable, A-B is not.
const DATA: &str = r#"
@prefix era: <http://data.europa.eu/949/> .
@prefix nav: <http://data.europa.eu/949/concepts/navigabilities/> .
@prefix ex: <http://example.com/> .
ex:T a era:LinearElement ; era:lengthOfNetLinearElement 100 .
ex:A a era:LinearElement ; era:lengthOfNetLinearElement 50 .
ex:B a era:LinearElement ; era:lengthOfNetLinearElement 70 .
ex:r1 a era:NetRelation ; era:elementA ex:T ; era:elementB ex:A ;
  era:isOnOriginOfElementA false ; era:isOnOriginOfElementB true ; era:navigability nav:Both .
ex:r2 a era:NetRelation ; era:elementA ex:T ; era:elementB ex:B ;
  era:isOnOriginOfElementA false ; era:isOnOriginOfElementB true ; era:navigability nav:Both .
ex:r3 a era:NetRelation ; era:elementA ex:A ; era:elementB ex:B ;
  era:isOnOriginOfElementA true ; era:isOnOriginOfElementB true ; era:navigability nav:None .
"#;

fn cost(e: &Engine, from: &str, to: &str) -> Option<f64> {
    cost_with(PROFILE, e, from, to)
}

fn cost_with(profile: &str, e: &Engine, from: &str, to: &str) -> Option<f64> {
    // Strip the profile's PREFIX lines into the outer query and nest its SELECT.
    let (prefixes, select): (Vec<&str>, Vec<&str>) = profile
        .lines()
        .filter(|l| !l.starts_with('#'))
        .partition(|l| l.starts_with("PREFIX"));
    let q = format!(
        r#"{}
PREFIX pgr: <http://w3id.org/tripletrail/pgr#>
SELECT ?agg WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
  [] pgr:function pgr:pgr_dijkstraCost ; pgr:start_vid "http://example.com/{from}" ;
     pgr:end_vid "http://example.com/{to}" ; pgr:agg_cost ?agg .
  {{ {} }}
}} }}"#,
        prefixes.join("\n"),
        select.join("\n")
    );
    let QueryResults::Solutions(mut s) = e.query(&q).unwrap() else {
        panic!()
    };
    s.next().map(|r| match r.unwrap().get("agg") {
        Some(Term::Literal(l)) => l.value().parse().unwrap(),
        other => panic!("{other:?}"),
    })
}

#[test]
fn switch_legs_are_not_connected() {
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    // Trunk onto either leg.
    assert_eq!(cost(&e, "T#fwd", "A#fwd"), Some(50.0));
    assert_eq!(cost(&e, "T#fwd", "B#fwd"), Some(70.0));
    // From a leg back onto the trunk.
    assert_eq!(cost(&e, "A#bwd", "T#bwd"), Some(100.0));
    // Never from one leg to the other, in either direction.
    assert_eq!(cost(&e, "A#bwd", "B#fwd"), None);
    assert_eq!(cost(&e, "B#bwd", "A#fwd"), None);
    // And no reversal on the trunk.
    assert_eq!(cost(&e, "T#fwd", "T#bwd"), None);
}

#[test]
fn reversing_profile_changes_legs_through_a_reversal() {
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    let cost = |from, to| cost_with(REVERSING, &e, from, to);
    // Unchanged where no reversal is needed.
    assert_eq!(cost("T#fwd", "A#fwd"), Some(50.0));
    // Leg A back onto the trunk (100), reverse through it (100 + 1000), out onto leg B (70).
    assert_eq!(cost("A#bwd", "B#fwd"), Some(1270.0));
    // Or reverse on A itself (50 + 1000).
    assert_eq!(cost("A#fwd", "A#bwd"), Some(1050.0));
}
