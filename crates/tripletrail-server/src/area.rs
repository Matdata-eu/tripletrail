//! The spatial filter of the map and the OGC API's items (#33): the features with a geometry
//! in a relation to an area.
//!
//! - `area`: a geometry, as WKT (CRS84 unless it names its CRS first, as a `geo:wktLiteral`
//!   does), or the IRI of a resource whose geometry (`geo:asWKT`, or through
//!   `geo:hasGeometry`) is the area, read under the caller's view;
//! - `rel`: `intersects` (the default), `within`, `contains`, `touches`, `crosses`,
//!   `overlaps`, `disjoint` or `dwithin`, as `geof:sf…` judges it between one of the feature's
//!   geometries and the area (`disjoint`: none of its geometries meets the area);
//! - `dist`: metres, for `dwithin` (and only for it): the features with a geometry at most
//!   that far from the area, as `geof:distance` measures it.
//!
//! The candidates come from the spatial index (the area's box, widened by `dist`), and the
//! exact test runs on those only.

use crate::admin::Entry;
use crate::ogc::{Collection, run};
use crate::{HttpError, Params, View};
use geo::BoundingRect;
use std::time::Duration;
use tripletrail::geo::{Crs, parse_wkt_literal_uncached, transform, wkt_literal};
use tripletrail::oxigraph::model::{Literal, NamedNode, Term};

const GEO: &str = "http://www.opengis.net/ont/geosparql#";
const GEOF: &str = "http://www.opengis.net/def/function/geosparql/";
const WKT: &str = "http://www.opengis.net/ont/geosparql#wktLiteral";
const METRE: &str = "http://www.opengis.net/def/uom/OGC/1.0/metre";

/// The longest WKT an `area` may be, in bytes.
const MAX_WKT: usize = 200_000;
/// The longest `dist`, in metres.
const MAX_DIST: f64 = 1_000_000.0;
/// How long reading the geometry of an `area` IRI may take.
const LOOKUP: Duration = Duration::from_secs(10);

/// The filter as SPARQL over `?f`, and its part of a cache key.
pub(crate) struct AreaFilter {
    pub(crate) sparql: String,
    pub(crate) signature: String,
}

/// A spatial relation of `rel`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rel {
    Intersects,
    Within,
    Contains,
    Touches,
    Crosses,
    Overlaps,
    Disjoint,
    DWithin,
}

impl Rel {
    fn parse(text: &str) -> Result<Self, HttpError> {
        Ok(match text.trim().to_ascii_lowercase().as_str() {
            "" | "intersects" => Self::Intersects,
            "within" => Self::Within,
            "contains" => Self::Contains,
            "touches" => Self::Touches,
            "crosses" => Self::Crosses,
            "overlaps" => Self::Overlaps,
            "disjoint" => Self::Disjoint,
            "dwithin" => Self::DWithin,
            other => {
                return Err(HttpError::bad_request(format!(
                    "rel={other}: one of intersects, within, contains, touches, crosses, \
                     overlaps, disjoint, dwithin"
                )));
            }
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Intersects => "intersects",
            Self::Within => "within",
            Self::Contains => "contains",
            Self::Touches => "touches",
            Self::Crosses => "crosses",
            Self::Overlaps => "overlaps",
            Self::Disjoint => "disjoint",
            Self::DWithin => "dwithin",
        }
    }

    /// The `geof:sf…` function that tests it.
    fn function(self) -> &'static str {
        match self {
            Self::Intersects | Self::Disjoint | Self::DWithin => "sfIntersects",
            Self::Within => "sfWithin",
            Self::Contains => "sfContains",
            Self::Touches => "sfTouches",
            Self::Crosses => "sfCrosses",
            Self::Overlaps => "sfOverlaps",
        }
    }
}

/// The spatial filter of `params` on `collection`'s features, or `None` without an `area`.
/// `many`: the rest of the query keeps many features (counts over a whole class), rather
/// than a few (a tile's, a page's); it decides how `disjoint` is tested.
pub(crate) fn area_filter(
    entry: &Entry,
    view: &View,
    collection: &Collection,
    params: &Params,
    many: bool,
) -> Result<Option<AreaFilter>, HttpError> {
    let Some(area) = params.get("area").filter(|a| !a.trim().is_empty()) else {
        if params.get("rel").is_some() || params.get("dist").is_some() {
            return Err(HttpError::bad_request("rel and dist need an area"));
        }
        return Ok(None);
    };
    let rel = Rel::parse(params.get("rel").unwrap_or_default())?;
    let dist = match (rel, params.get("dist")) {
        (Rel::DWithin, Some(d)) => match d.trim().parse::<f64>() {
            Ok(d) if (0.0..=MAX_DIST).contains(&d) => d,
            _ => {
                return Err(HttpError::bad_request(format!(
                    "dist={d}: metres, from 0 to {MAX_DIST}"
                )));
            }
        },
        (Rel::DWithin, None) => return Err(HttpError::bad_request("rel=dwithin needs a dist")),
        (_, Some(_)) => return Err(HttpError::bad_request("dist goes only with rel=dwithin")),
        (_, None) => 0.0,
    };
    let geometry = area_geometry(entry, view, area.trim())?;
    let Term::Literal(literal) = wkt_literal(&geometry, &Crs::Crs84) else {
        unreachable!("a WKT literal")
    };
    let wkt = literal.value().to_owned();
    // The candidates: the area's box, widened by dist for dwithin.
    let window = match rel {
        Rel::DWithin => widened_box(&geometry, dist),
        _ => wkt.clone(),
    };
    let ways = collection.geometry.near(entry, view, "?f", &window);
    let area_term = Literal::new_typed_literal(&wkt, NamedNode::new_unchecked(WKT)).to_string();
    let test = match rel {
        Rel::DWithin => format!("<{GEOF}distance>(?areaW, {area_term}, <{METRE}>) <= {dist:?}"),
        _ => format!("<{GEOF}{}>(?areaW, {area_term})", rel.function()),
    };
    let meeting = format!(
        "SELECT DISTINCT ?f WHERE {{ {} FILTER({test}) }}",
        ways.pattern("?f", "?areaW")
    );
    // Disjoint: the features that meet the area taken away, all at once for many features,
    // or tested on each of a few (a tile's, a page's: 10 to 50 times faster there). The
    // tiles, facets and items read only features with a geometry, so one without any is
    // never kept.
    let sparql = match rel {
        Rel::Disjoint if many => format!(" MINUS {{ {meeting} }}"),
        Rel::Disjoint => format!(
            " FILTER NOT EXISTS {{ {} FILTER({test}) }}",
            ways.pattern("?f", "?areaW")
        ),
        _ => format!(" {{ {meeting} }}"),
    };
    // The area itself in the key while it is short, its hash beyond.
    let area_key = if wkt.len() <= 256 {
        wkt
    } else {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        wkt.hash(&mut h);
        format!("#{:016x}:{}", h.finish(), wkt.len())
    };
    let mut signature = format!("area={area_key}&rel={}", rel.name());
    if rel == Rel::DWithin {
        signature.push_str(&format!("&dist={dist:?}"));
    }
    Ok(Some(AreaFilter { sparql, signature }))
}

/// The area as a CRS84 geometry: WKT, or the geometry of the resource an IRI names.
fn area_geometry(entry: &Entry, view: &View, area: &str) -> Result<geo::Geometry<f64>, HttpError> {
    let is_iri = ["http://", "https://", "urn:"].iter().any(|p| {
        area.get(..p.len())
            .is_some_and(|a| a.eq_ignore_ascii_case(p))
    });
    if !is_iri {
        if area.len() > MAX_WKT {
            return Err(HttpError::bad_request(format!(
                "An area of more than {MAX_WKT} bytes of WKT"
            )));
        }
        return crs84(&Literal::new_typed_literal(area, NamedNode::new_unchecked(WKT)).into())
            .ok_or_else(|| HttpError::bad_request(format!("area: not a WKT geometry: {area}")));
    }
    let iri =
        NamedNode::new(area).map_err(|e| HttpError::bad_request(format!("area {area}: {e}")))?;
    let rows = run(
        entry,
        &format!(
            "SELECT DISTINCT ?w WHERE {{ {{ {iri} <{GEO}asWKT> ?w }} UNION \
             {{ {iri} <{GEO}hasGeometry>/<{GEO}asWKT> ?w }} }} LIMIT 100"
        ),
        Some(LOOKUP),
        view,
    )?;
    let parts: Vec<geo::Geometry<f64>> = rows
        .into_iter()
        .filter_map(|row| row.into_iter().next().flatten())
        .filter_map(|w| crs84(&w))
        .collect();
    match parts.len() {
        0 => Err(HttpError::bad_request(format!(
            "area {area}: no geometry of it to read"
        ))),
        1 => Ok(parts.into_iter().next().expect("one")),
        _ => Ok(geo::Geometry::GeometryCollection(geo::GeometryCollection(
            parts,
        ))),
    }
}

/// A WKT literal's geometry in CRS84.
fn crs84(term: &Term) -> Option<geo::Geometry<f64>> {
    let g = parse_wkt_literal_uncached(term)?;
    transform(&g.geometry, &g.crs, &Crs::Crs84)
}

/// The box of `g` widened by `metres` on every side, as CRS84 WKT: a little wider than the
/// metres, never narrower, so no feature within them is left out.
fn widened_box(g: &geo::Geometry<f64>, metres: f64) -> String {
    let Some(r) = g.bounding_rect() else {
        return "POLYGON EMPTY".to_owned();
    };
    // A degree of latitude is at least 110,574 m, and of longitude that times the cosine.
    let dy = metres / 110_000.0;
    let (y1, y2) = ((r.min().y - dy).max(-90.0), (r.max().y + dy).min(90.0));
    let widest = y1.abs().max(y2.abs());
    let dx = if widest >= 89.0 {
        360.0
    } else {
        metres / (110_000.0 * widest.to_radians().cos())
    };
    let (x1, x2) = ((r.min().x - dx).max(-180.0), (r.max().x + dx).min(180.0));
    format!("POLYGON(({x1} {y1}, {x2} {y1}, {x2} {y2}, {x1} {y2}, {x1} {y1}))")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relations_parse() {
        assert_eq!(Rel::parse("").unwrap(), Rel::Intersects);
        assert_eq!(Rel::parse(" DWithin ").unwrap(), Rel::DWithin);
        assert!(Rel::parse("near").is_err());
    }

    #[test]
    fn a_widened_box_holds_the_distance() {
        let p = geo::Geometry::Point(geo::Point::new(10.0, 60.0));
        let wkt = widened_box(&p, 1000.0);
        let g =
            crs84(&Literal::new_typed_literal(wkt, NamedNode::new_unchecked(WKT)).into()).unwrap();
        let r = g.bounding_rect().unwrap();
        // 1 km is about 0.009° of latitude and 0.018° of longitude at 60° N.
        assert!(
            r.max().y - 60.0 >= 0.009 && r.max().y - 60.0 < 0.01,
            "{r:?}"
        );
        assert!(
            r.max().x - 10.0 >= 0.0179 && r.max().x - 10.0 < 0.0185,
            "{r:?}"
        );
    }
}
