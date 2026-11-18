//! Discrete global grid literals (GeoSPARQL 1.1 `geof:asDGGS`), using H3 cells.
//!
//! The literal lists the cells covering a geometry, in the AusPIX `CELLLIST` style:
//! `"CELLLIST ((8a1f... 8a1f...))"^^<http://w3id.org/tripletrail/dggs#h3Literal>`. These literals are
//! geometries too: every `geof:` and `st:` function accepts them, as the outline of their cells.

use crate::functions::{Func, GEOF, ST, func, geom};
use crate::literal::{Crs, Geom};
use geo::{Coord, CoordsIter, Geometry, Line, MultiPolygon};
use h3o::geom::{ContainmentMode, PlotterBuilder, SolventBuilder, TilerBuilder};
use h3o::{CellIndex, LatLng, Resolution};
use oxigraph::model::{Literal, NamedNodeRef, Term};

/// Datatype of H3 cell lists.
pub const H3_LITERAL: NamedNodeRef<'static> =
    NamedNodeRef::new_unchecked("http://w3id.org/tripletrail/dggs#h3Literal");

/// Resolution used when `geof:asDGGS` is given none: about 0.1 km² cells, 200 m across.
const DEFAULT_RESOLUTION: u8 = 9;

/// Most cells a cell list may hold. Covering a large area at a fine resolution would
/// otherwise run for minutes and build a literal of hundreds of megabytes (a 4° square at the
/// default resolution is about two million cells), so such a call is an error instead.
const MAX_CELLS: f64 = 100_000.0;

/// About how many cells of `resolution` cover the polygons of `g` (in lon/lat degrees), at
/// most. Holes are ignored.
fn estimated_cells(g: &Geometry<f64>, resolution: Resolution) -> f64 {
    fn shells(g: &Geometry<f64>) -> f64 {
        let shell = |p: &geo::Polygon<f64>| {
            crate::functions::geodesic_area(&Geometry::Polygon(geo::Polygon::new(
                p.exterior().clone(),
                vec![],
            )))
        };
        match g {
            Geometry::Polygon(p) => shell(p),
            Geometry::MultiPolygon(m) => m.0.iter().map(shell).sum(),
            Geometry::Rect(_) | Geometry::Triangle(_) => crate::functions::geodesic_area(g),
            Geometry::GeometryCollection(c) => c.0.iter().map(shells).sum(),
            _ => 0.0,
        }
    }
    shells(g) / 1e6 / resolution.area_km2()
}

/// The H3 cells covering a geometry given in longitude, latitude degrees, sorted.
pub(crate) fn cells(g: &Geometry<f64>, resolution: Resolution) -> Option<Vec<CellIndex>> {
    let mut out = Vec::new();
    let point = |c: Coord<f64>| LatLng::new(c.y, c.x).ok().map(|ll| ll.to_cell(resolution));
    fn walk(
        g: &Geometry<f64>,
        resolution: Resolution,
        out: &mut Vec<CellIndex>,
        point: &dyn Fn(Coord<f64>) -> Option<CellIndex>,
    ) -> Option<()> {
        match g {
            Geometry::Point(p) => out.push(point(p.0)?),
            Geometry::MultiPoint(m) => {
                for p in m {
                    out.push(point(p.0)?);
                }
            }
            Geometry::Line(_) | Geometry::LineString(_) | Geometry::MultiLineString(_) => {
                let mut plotter = PlotterBuilder::new(resolution).build();
                let lines: Vec<Line<f64>> = match g {
                    Geometry::Line(l) => vec![*l],
                    Geometry::LineString(l) => l.lines().collect(),
                    Geometry::MultiLineString(m) => m.iter().flat_map(|l| l.lines()).collect(),
                    _ => unreachable!(),
                };
                for l in lines {
                    if l.start == l.end {
                        out.push(point(l.start)?);
                    } else {
                        plotter.add(l).ok()?;
                    }
                }
                for c in plotter.plot() {
                    out.push(c.ok()?);
                }
            }
            Geometry::Polygon(_)
            | Geometry::MultiPolygon(_)
            | Geometry::Rect(_)
            | Geometry::Triangle(_) => {
                let polygons: Vec<geo::Polygon<f64>> = match g {
                    Geometry::Polygon(p) => vec![p.clone()],
                    Geometry::MultiPolygon(m) => m.0.clone(),
                    Geometry::Rect(r) => vec![r.to_polygon()],
                    Geometry::Triangle(t) => vec![t.to_polygon()],
                    _ => unreachable!(),
                };
                let mut tiler = TilerBuilder::new(resolution)
                    .containment_mode(ContainmentMode::Covers)
                    .build();
                for p in polygons {
                    tiler.add(p).ok()?;
                }
                out.extend(tiler.into_coverage());
            }
            Geometry::GeometryCollection(c) => {
                for g in c {
                    walk(g, resolution, out, point)?;
                }
            }
        }
        Some(())
    }
    walk(g, resolution, &mut out, &point)?;
    out.sort_unstable();
    out.dedup();
    Some(out)
}

/// The `CELLLIST` lexical form of a cell set.
fn cell_list(cells: &[CellIndex]) -> String {
    let list: Vec<String> = cells.iter().map(ToString::to_string).collect();
    format!("CELLLIST (({}))", list.join(" "))
}

/// Parses an H3 cell list: `CELLLIST ((a b c))`, or the cell indexes alone.
pub(crate) fn parse_h3(value: &str) -> Option<Geom> {
    let body = value.trim();
    let body = match body.strip_prefix("CELLLIST") {
        Some(rest) => rest.trim().strip_prefix("((")?.strip_suffix("))")?,
        None => body,
    };
    let mut cells = body
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<CellIndex>().ok())
        .collect::<Option<Vec<_>>>()?;
    cells.sort_unstable();
    cells.dedup();
    let finest = cells.iter().map(|c| c.resolution()).max()?;
    let outline: MultiPolygon<f64> = SolventBuilder::new()
        .enable_heterogeneous_support(finest)
        .build()
        .dissolve(cells)
        .ok()?;
    Some(Geom::new(Geometry::MultiPolygon(outline), Crs::Crs84))
}

pub(crate) fn add_functions(add: &mut dyn FnMut(&str, &str, Func)) {
    // geof:asDGGS(geometry, datatype [, resolution]); also st:ST_H3Cells(geometry [, resolution]).
    let as_dggs = |geometry: &Term, resolution: Option<&Term>| -> Option<Term> {
        let g = geom(geometry)?;
        let lonlat = if g.crs.is_geographic() {
            g.geometry.clone()
        } else {
            crate::crs::transform(&g.geometry, &g.crs, &Crs::Crs84)?
        };
        let resolution = match resolution {
            None => DEFAULT_RESOLUTION,
            Some(Term::Literal(l)) => l.value().parse().ok()?,
            Some(_) => return None,
        };
        let resolution = Resolution::try_from(resolution).ok()?;
        lonlat.coords_iter().next()?;
        // A latitude past a pole has no geodesic area (NaN), which no estimate can bound: the
        // tiler would then cover most of the globe.
        if lonlat
            .coords_iter()
            .any(|c| !c.x.is_finite() || !(-90.0..=90.0).contains(&c.y))
        {
            return None;
        }
        let estimate = estimated_cells(&lonlat, resolution);
        if estimate.is_nan() || estimate > MAX_CELLS {
            return None;
        }
        let cells = cells(&lonlat, resolution)?;
        Some(Literal::new_typed_literal(cell_list(&cells), H3_LITERAL).into())
    };
    add(
        GEOF,
        "asDGGS",
        func(move |args| match args {
            [g, Term::NamedNode(dt)] if dt.as_ref() == H3_LITERAL => as_dggs(g, None),
            [g, Term::NamedNode(dt), r] if dt.as_ref() == H3_LITERAL => as_dggs(g, Some(r)),
            _ => None,
        }),
    );
    add(
        ST,
        "ST_H3Cells",
        func(move |args| match args {
            [g] => as_dggs(g, None),
            [g, r] => as_dggs(g, Some(r)),
            _ => None,
        }),
    );
}
