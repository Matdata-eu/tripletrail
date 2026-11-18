//! Spatial tests on parsed geometries.
//!
//! The registered functions take RDF terms and parse them through the text-keyed cache on
//! every call. A caller that already holds the parsed geometries (the spatial index, a join)
//! can call the same tests on [`Geom`]s through [`geom_predicate`], with no parsing, hashing
//! of the literal text, or result literal.

use crate::fast;
use crate::functions::{GEOF, ST, metric_distance, same_space};
use crate::literal::Geom;
use geo::BoundingRect;
use geo::line_measures::{Distance, Euclidean};
use oxigraph::model::Term;
use std::sync::Arc;

/// A test of two geometries, with the constant arguments that follow them (a unit, a
/// distance). Relations give `1.0` (true) or `0.0` (false), measures their value; `None` is
/// the SPARQL error the function would raise.
pub type GeomPredicate = Arc<dyn Fn(&Geom, &Geom, &[Term]) -> Option<f64> + Send + Sync>;

const UOM: &str = "http://www.opengis.net/def/uom/OGC/1.0/";

type Rel = fn(&geo::relate::IntersectionMatrix) -> bool;

/// Simple Features relations by `geof:` and `st:` local name.
const SF: [(&str, &str, Rel); 10] = [
    ("sfEquals", "ST_Equals", |m| m.is_equal_topo()),
    ("sfDisjoint", "ST_Disjoint", |m| m.is_disjoint()),
    ("sfIntersects", "ST_Intersects", |m| m.is_intersects()),
    ("sfTouches", "ST_Touches", |m| m.is_touches()),
    ("sfCrosses", "ST_Crosses", |m| m.is_crosses()),
    ("sfWithin", "ST_Within", |m| m.is_within()),
    ("sfContains", "ST_Contains", |m| m.is_contains()),
    ("sfOverlaps", "ST_Overlaps", |m| m.is_overlaps()),
    // geof:ehCovers/ehCoveredBy use GeoSPARQL's stricter patterns (geosparql11.rs).
    ("", "ST_Covers", |m| m.is_covers()),
    ("", "ST_CoveredBy", |m| m.is_coveredby()),
];

fn apart(a: &Geom, b: &Geom) -> bool {
    match (a.geometry.bounding_rect(), b.geometry.bounding_rect()) {
        (Some(ra), Some(rb)) => {
            ra.max().x < rb.min().x
                || rb.max().x < ra.min().x
                || ra.max().y < rb.min().y
                || rb.max().y < ra.min().y
        }
        _ => false,
    }
}

/// Whether the bounding boxes of two geometries meet; `None` for geometries in different
/// spaces, and `false` when one is empty.
pub(crate) fn envelopes_intersect(a: &Geom, b: &Geom) -> Option<bool> {
    same_space(a, b).then_some(())?;
    Some(
        a.geometry.bounding_rect().is_some()
            && b.geometry.bounding_rect().is_some()
            && !apart(a, b),
    )
}

/// A Simple Features relation (`sfIntersects`, `ST_Covers`, ...) between two geometries.
pub(crate) fn simple_features(local: &str, a: &Geom, b: &Geom) -> Option<bool> {
    let &(geof, _, rel) = SF.iter().find(|(g, s, _)| *g == local || *s == local)?;
    same_space(a, b).then_some(())?;
    let disjoint = geof == "sfDisjoint";
    // Bounding boxes first: most pairs in a FILTER are far apart.
    if apart(a, b) {
        return Some(disjoint);
    }
    // Within and covered-by need every point of the inner geometry in the outer one: a vertex
    // outside a polygon settles them without the matrix.
    let inner_outer = match local {
        "sfWithin" | "ST_Within" | "ST_CoveredBy" => Some((a, b)),
        "sfContains" | "ST_Contains" | "ST_Covers" => Some((b, a)),
        _ => None,
    };
    if let Some((inner, outer)) = inner_outer {
        if fast::vertex_outside(inner, outer) {
            return Some(false);
        }
        // A line inside a polygon, met by none of its edges: the usual answer of a spatial
        // join of lines with large areas, which `relate` builds a graph of both for.
        if fast::in_interior(inner, outer) {
            return Some(true);
        }
    }
    Some(match geof {
        "sfIntersects" => fast::intersects(a, b),
        "sfDisjoint" => !fast::intersects(a, b),
        _ => rel(&fast::relate(a, b)),
    })
}

/// An Egenhofer or RCC8 relation, as DE-9IM patterns of which any may match.
pub(crate) fn patterns(patterns: &[&str], a: &Geom, b: &Geom) -> Option<bool> {
    same_space(a, b).then_some(())?;
    if crate::geosparql11::needs_contact(patterns) && apart(a, b) {
        return Some(false);
    }
    // Relations whose first (or second) geometry lies in the other's closure: one vertex of
    // it outside a polygon settles them.
    let first_inside = |p: &str| p.as_bytes()[2] == b'F' && p.as_bytes()[5] == b'F';
    let second_inside = |p: &str| p.as_bytes()[6] == b'F' && p.as_bytes()[7] == b'F';
    if (patterns.iter().all(|p| first_inside(p)) && fast::vertex_outside(a, b))
        || (patterns.iter().all(|p| second_inside(p)) && fast::vertex_outside(b, a))
    {
        return Some(false);
    }
    let m = fast::relate(a, b);
    Some(patterns.iter().any(|p| m.matches(p).unwrap_or(false)))
}

fn num(t: &Term) -> Option<f64> {
    crate::functions::num(t)
}

fn bool_value(v: bool) -> f64 {
    if v { 1.0 } else { 0.0 }
}

/// The test of the function `iri` on parsed geometries, for the two-geometry functions the
/// spatial index answers: the topological relations (`geof:sf*`, `geof:eh*`, `geof:rcc8*`,
/// `geof:relate` excepted, and their `st:ST_*` forms) and the distances (`geof:distance`,
/// `st:ST_Distance`, `st:ST_DistanceSphere`, `st:ST_DWithin`, `st:ST_DWithinSphere`).
pub fn geom_predicate(iri: &str) -> Option<GeomPredicate> {
    if let Some(local) = iri.strip_prefix(GEOF) {
        if SF.iter().any(|(g, _, _)| !g.is_empty() && *g == local) {
            let local = local.to_owned();
            return Some(Arc::new(move |a, b, _| {
                simple_features(&local, a, b).map(bool_value)
            }));
        }
        if let Some((_, p)) = crate::geosparql11::RELATIONS
            .iter()
            .find(|(n, _)| *n == local)
        {
            let p: &'static [&'static str] = p;
            return Some(Arc::new(move |a, b, _| patterns(p, a, b).map(bool_value)));
        }
        if local == "distance" {
            return Some(Arc::new(|a, b, extra| {
                let unit = match extra.first() {
                    None => "metre",
                    Some(Term::NamedNode(n)) => n.as_str().strip_prefix(UOM)?,
                    _ => return None,
                };
                match unit {
                    "metre" => metric_distance(a, b),
                    "kilometre" => Some(metric_distance(a, b)? / 1000.0),
                    "degree" if a.crs.is_geographic() && same_space(a, b) => {
                        Some(Euclidean.distance(&a.geometry, &b.geometry))
                    }
                    _ => None,
                }
            }));
        }
        return None;
    }
    let local = iri.strip_prefix(ST)?;
    if SF.iter().any(|(_, s, _)| *s == local) {
        let local = local.to_owned();
        return Some(Arc::new(move |a, b, _| {
            simple_features(&local, a, b).map(bool_value)
        }));
    }
    if local == "ST_EnvelopesIntersect" {
        return Some(Arc::new(|a, b, _| {
            envelopes_intersect(a, b).map(bool_value)
        }));
    }
    let planar =
        |a: &Geom, b: &Geom| same_space(a, b).then(|| Euclidean.distance(&a.geometry, &b.geometry));
    Some(match local {
        "ST_DistanceSphere" => Arc::new(|a, b, _| metric_distance(a, b)),
        "ST_Distance" => Arc::new(move |a, b, _| planar(a, b)),
        "ST_DWithin" => Arc::new(move |a, b, extra| {
            let [d] = extra else { return None };
            Some(bool_value(planar(a, b)? <= num(d)?))
        }),
        "ST_DWithinSphere" => Arc::new(|a, b, extra| {
            let [d] = extra else { return None };
            Some(bool_value(metric_distance(a, b)? <= num(d)?))
        }),
        _ => return None,
    })
}
