//! SFCGAL-style 3-D functions: polyhedral surfaces and TINs (`ST_NumPatches`, `ST_PatchN`,
//! `ST_Tesselate`), solids (`ST_MakeSolid`, `ST_IsSolid`, `ST_Volume`, `ST_Extrude`), 3-D set
//! operations (`ST_3DIntersection`, `ST_3DDifference`, `ST_3DUnion`, `ST_3DConvexHull`) and
//! `ST_StraightSkeleton`.

use crate::csg::{Csg, Op, clip_line, contains, triangles};
use crate::functions::{Func, ST, boolean, double, func, geom, integer, num};
use crate::hull3d::{Hull, convex_hull};
use crate::literal::{Crs, Geom, Surface, SurfaceKind, geom_literal};
use crate::postgis::{int, out};
use crate::postgis_processing::polygons_of;
use crate::solid::{
    P3, Patch, Ring3, add, area_vector, is_closed, norm, reversed, signed_volume, triangle_patch,
    triangulate,
};
use geo::{BooleanOps, Coord, Geometry, GeometryCollection, LineString, MultiPolygon, Orient};
use oxigraph::model::Term;

fn z_at(g: &Geom, c: Coord<f64>) -> f64 {
    match g.zm.as_ref().and_then(|z| z.at(c)) {
        Some((z, _)) if z.is_finite() => z,
        _ => 0.0,
    }
}

fn lift_line(g: &Geom, l: &LineString<f64>) -> Ring3 {
    l.0.iter().map(|&c| [c.x, c.y, z_at(g, c)]).collect()
}

fn lift_polygon(g: &Geom, p: &geo::Polygon<f64>) -> Patch {
    std::iter::once(p.exterior())
        .chain(p.interiors())
        .filter(|r| r.0.len() >= 4)
        .map(|r| lift_line(g, r))
        .collect()
}

/// The polygons, lines and points of a geometry, in 3-D.
#[derive(Default)]
struct Parts {
    polys: Vec<Patch>,
    lines: Vec<Ring3>,
    points: Vec<P3>,
}

fn parts(g: &Geom) -> Parts {
    let mut out = Parts::default();
    if let Some(s) = &g.surface {
        match s.kind {
            SurfaceKind::Point | SurfaceKind::MultiPoint => {
                out.points = s.patches.iter().flatten().flatten().copied().collect()
            }
            SurfaceKind::LineString | SurfaceKind::MultiLineString => {
                out.lines = s.patches.iter().flatten().cloned().collect()
            }
            _ => out.polys = s.patches.clone(),
        }
        return out;
    }
    fn walk(g: &Geom, geometry: &Geometry<f64>, out: &mut Parts) {
        match geometry {
            Geometry::Point(p) => out.points.push([p.x(), p.y(), z_at(g, p.0)]),
            Geometry::MultiPoint(m) => out
                .points
                .extend(m.0.iter().map(|p| [p.x(), p.y(), z_at(g, p.0)])),
            Geometry::Line(l) => out
                .lines
                .push(lift_line(g, &LineString::new(vec![l.start, l.end]))),
            Geometry::LineString(l) => out.lines.push(lift_line(g, l)),
            Geometry::MultiLineString(m) => out.lines.extend(m.0.iter().map(|l| lift_line(g, l))),
            Geometry::Polygon(_)
            | Geometry::MultiPolygon(_)
            | Geometry::Rect(_)
            | Geometry::Triangle(_) => out.polys.extend(
                polygons_of(geometry)
                    .iter()
                    .map(|p| lift_polygon(g, &p.orient(geo::orient::Direction::Default)))
                    .filter(|p| !p.is_empty()),
            ),
            Geometry::GeometryCollection(c) => c.0.iter().for_each(|x| walk(g, x, out)),
        }
    }
    walk(g, &g.geometry, &mut out);
    out
}

fn literal(s: Surface, crs: &Crs) -> Option<Term> {
    Some(geom_literal(&Geom::from_surface(s, crs.clone())))
}

fn empty(crs: &Crs) -> Option<Term> {
    out(
        Geometry::GeometryCollection(GeometryCollection(Vec::new())),
        crs,
    )
}

/// The patches of a polyhedral surface or TIN.
fn patches_of(g: &Geom) -> Option<(&Surface, &[Patch])> {
    let s = g.surface.as_deref()?;
    matches!(s.kind, SurfaceKind::PolyhedralSurface | SurfaceKind::Tin)
        .then_some((s, s.patches.as_slice()))
}

/// The closed shell swept by moving `patches` by `d`: the patches reversed at the start,
/// moved at the end, and a quadrilateral for every boundary edge; oriented outward.
fn extrude_patches(patches: &[Patch], d: P3) -> Vec<Patch> {
    use std::collections::HashSet;
    let key = |p: &P3| [p[0].to_bits(), p[1].to_bits(), p[2].to_bits()];
    let mut edges = HashSet::new();
    for r in patches.iter().flatten() {
        for w in r.windows(2) {
            edges.insert((key(&w[0]), key(&w[1])));
        }
    }
    let mut shell: Vec<Patch> = patches.iter().map(reversed).collect();
    shell.extend(patches.iter().map(|p| {
        p.iter()
            .map(|r| r.iter().map(|&q| add(q, d)).collect())
            .collect()
    }));
    for r in patches.iter().flatten() {
        for w in r.windows(2) {
            let (a, b) = (w[0], w[1]);
            if a != b && !edges.contains(&(key(&b), key(&a))) {
                shell.push(vec![vec![a, b, add(b, d), add(a, d), a]]);
            }
        }
    }
    if signed_volume(&shell) < 0.0 {
        shell = shell.iter().map(reversed).collect();
    }
    shell
}

/// PostGIS ST_Extrude: a point sweeps a line, a line a surface, a polygon a solid.
fn extrude(g: &Geom, d: P3) -> Option<Surface> {
    let p = parts(g);
    match (p.polys.is_empty(), p.lines.is_empty(), p.points.is_empty()) {
        (false, true, true) => {
            let shell = if g.surface.is_some() {
                extrude_patches(&p.polys, d)
            } else {
                // Each polygon on its own, so that each is oriented outward.
                p.polys
                    .iter()
                    .flat_map(|q| extrude_patches(std::slice::from_ref(q), d))
                    .collect()
            };
            Some(Surface {
                solid: true,
                ..Surface::new(SurfaceKind::PolyhedralSurface, shell)
            })
        }
        (true, false, true) => {
            let quads = p
                .lines
                .iter()
                .flat_map(|l| l.windows(2))
                .filter(|w| w[0] != w[1])
                .map(|w| vec![vec![w[0], w[1], add(w[1], d), add(w[0], d), w[0]]])
                .collect();
            Some(Surface::new(SurfaceKind::PolyhedralSurface, quads))
        }
        (true, true, false) => {
            let kind = if p.points.len() == 1 {
                SurfaceKind::LineString
            } else {
                SurfaceKind::MultiLineString
            };
            Some(Surface::new(
                kind,
                p.points.iter().map(|&q| vec![vec![q, add(q, d)]]).collect(),
            ))
        }
        _ => None,
    }
}

/// A 3-D result of the given dimension's parts.
fn result(polys: Vec<Patch>, lines: Vec<Ring3>, points: Vec<P3>, surface: bool) -> Option<Surface> {
    match (polys.is_empty(), lines.is_empty(), points.is_empty()) {
        (false, true, true) => Some(Surface::new(
            if surface {
                SurfaceKind::PolyhedralSurface
            } else if polys.len() == 1 {
                SurfaceKind::Polygon
            } else {
                SurfaceKind::MultiPolygon
            },
            polys,
        )),
        (true, false, true) => Some(Surface::new(
            if lines.len() == 1 {
                SurfaceKind::LineString
            } else {
                SurfaceKind::MultiLineString
            },
            lines.into_iter().map(|l| vec![l]).collect(),
        )),
        (true, true, false) => Some(Surface::new(
            if points.len() == 1 {
                SurfaceKind::Point
            } else {
                SurfaceKind::MultiPoint
            },
            points.into_iter().map(|p| vec![vec![p]]).collect(),
        )),
        _ => None,
    }
}

/// The parts of `g` inside (`inside`) or outside the solid `s`.
fn clip(g: &Geom, s: &Surface, csg: &Csg, inside: bool) -> Option<Option<Surface>> {
    let p = parts(g);
    let tris = triangles(&s.patches);
    let polys = csg.clip_surface(&p.polys, &s.patches, inside);
    let lines: Vec<Ring3> = p
        .lines
        .iter()
        .flat_map(|l| clip_line(l, &tris, csg.eps, inside))
        .collect();
    let points: Vec<P3> = p
        .points
        .into_iter()
        .filter(|&q| contains(&tris, q, csg.eps) == inside)
        .collect();
    if polys.is_empty() && lines.is_empty() && points.is_empty() {
        return Some(None);
    }
    let surface = g
        .surface
        .as_ref()
        .is_some_and(|s| matches!(s.kind, SurfaceKind::PolyhedralSurface | SurfaceKind::Tin));
    Some(Some(result(polys, lines, points, surface)?))
}

fn solid(g: &Geom) -> Option<&Surface> {
    g.surface.as_deref().filter(|s| s.solid)
}

fn scale_of(gs: &[&Geom]) -> f64 {
    gs.iter()
        .map(|g| match &g.surface {
            Some(s) => s.scale(),
            None => {
                let p = parts(g);
                p.polys
                    .iter()
                    .flatten()
                    .chain(&p.lines)
                    .flatten()
                    .chain(&p.points)
                    .flat_map(|q| q.iter())
                    .fold(1.0, |m: f64, v| m.max(v.abs()))
            }
        })
        .fold(1.0, f64::max)
}

/// SFCGAL's 3-D set operations. Closed solids go through BSP CSG; a surface, line or point
/// set is clipped by a solid; two polygonal inputs without a solid or 3-D surface are combined
/// in 2-D (their Z kept where vertices match, as in `ST_Intersection`).
fn boolean3d(a: &Geom, b: &Geom, op: Op) -> Option<Term> {
    let csg = Csg::new(scale_of(&[a, b]));
    let done = |s: Option<Surface>| match s {
        Some(s) => literal(s, &a.crs),
        None => empty(&a.crs),
    };
    match (solid(a), solid(b), op) {
        (Some(sa), Some(sb), _) => {
            let patches = csg.boolean(&sa.patches, &sb.patches, op);
            done((!patches.is_empty()).then(|| Surface {
                solid: true,
                ..Surface::new(SurfaceKind::PolyhedralSurface, patches)
            }))
        }
        (Some(s), None, Op::Intersection) => done(clip(b, s, &csg, true)?),
        (None, Some(s), Op::Intersection) => done(clip(a, s, &csg, true)?),
        (None, Some(s), Op::Difference) => done(clip(a, s, &csg, false)?),
        (Some(_), None, Op::Difference) => Some(geom_literal(a)),
        (None, None, _) if a.surface.is_none() && b.surface.is_none() => {
            let (pa, pb) = (
                MultiPolygon(polygons_of(&a.geometry)),
                MultiPolygon(polygons_of(&b.geometry)),
            );
            (!pa.0.is_empty() && !pb.0.is_empty()).then_some(())?;
            let r = match op {
                Op::Intersection => pa.intersection(&pb),
                Op::Union => pa.union(&pb),
                Op::Difference => pa.difference(&pb),
            };
            out(Geometry::MultiPolygon(r), &a.crs)
        }
        _ => None,
    }
}

pub(crate) fn add_functions(add: &mut dyn FnMut(&str, &str, Func)) {
    add(
        ST,
        "ST_NumPatches",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            integer(patches_of(&g)?.1.len() as i64)
        }),
    );
    add(
        ST,
        "ST_PatchN",
        func(|args| {
            let [a, n] = args else { return None };
            let g = geom(a)?;
            let (s, patches) = patches_of(&g)?;
            let n = usize::try_from(int(n)?).ok()?.checked_sub(1)?;
            let kind = if s.kind == SurfaceKind::Tin {
                SurfaceKind::Triangle
            } else {
                SurfaceKind::Polygon
            };
            literal(
                Surface {
                    has_z: s.has_z,
                    ..Surface::new(kind, vec![patches.get(n)?.clone()])
                },
                &g.crs,
            )
        }),
    );
    add(
        ST,
        "ST_MakeSolid",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let (s, _) = patches_of(&g)?;
            literal(
                Surface {
                    solid: true,
                    ..s.clone()
                },
                &g.crs,
            )
        }),
    );
    add(
        ST,
        "ST_IsSolid",
        func(|args| {
            let [a] = args else { return None };
            boolean(solid(&*geom(a)?).is_some())
        }),
    );
    add(
        ST,
        "ST_Volume",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            double(solid(&g).map_or(0.0, |s| signed_volume(&s.patches).abs()))
        }),
    );
    add(
        ST,
        "ST_Extrude",
        func(|args| {
            let [a, dx, dy, dz] = args else { return None };
            let g = geom(a)?;
            let d = [num(dx)?, num(dy)?, num(dz)?];
            literal(extrude(&g, d)?, &g.crs)
        }),
    );
    add(
        ST,
        "ST_3DConvexHull",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let p = parts(&g);
            let pts: Vec<P3> = p
                .polys
                .iter()
                .flatten()
                .chain(&p.lines)
                .flatten()
                .chain(&p.points)
                .copied()
                .collect();
            let s = match convex_hull(&pts) {
                Hull::Empty => return empty(&g.crs),
                Hull::Point(q) => Surface::new(SurfaceKind::Point, vec![vec![vec![q]]]),
                Hull::Segment(q, r) => {
                    Surface::new(SurfaceKind::LineString, vec![vec![vec![q, r]]])
                }
                Hull::Polygon(ring) => Surface::new(SurfaceKind::Polygon, vec![vec![ring]]),
                Hull::Polyhedron(tris) => Surface::new(
                    SurfaceKind::PolyhedralSurface,
                    tris.into_iter().map(triangle_patch).collect(),
                ),
            };
            literal(s, &g.crs)
        }),
    );
    for (name, op) in [
        ("ST_3DIntersection", Op::Intersection),
        ("ST_3DDifference", Op::Difference),
        ("ST_3DUnion", Op::Union),
    ] {
        add(
            ST,
            name,
            func(move |args| {
                let [a, b] = args else { return None };
                let (a, b) = (geom(a)?, geom(b)?);
                crate::functions::same_space(&a, &b).then_some(())?;
                boolean3d(&a, &b, op)
            }),
        );
    }
    add(
        ST,
        "ST_Tesselate",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let p = parts(&g);
            let has_z = match &g.surface {
                Some(s) => s.has_z,
                None => g.zm.as_ref().is_some_and(|z| z.has_z),
            };
            let tris: Vec<Patch> = p
                .polys
                .iter()
                .flat_map(triangulate)
                .map(triangle_patch)
                .collect();
            literal(
                Surface {
                    has_z,
                    ..Surface::new(SurfaceKind::Tin, tris)
                },
                &g.crs,
            )
        }),
    );
    add(
        ST,
        "ST_StraightSkeleton",
        func(|args| {
            let [a] = args else { return None };
            let g = geom(a)?;
            let polys = polygons_of(&g.geometry);
            (!polys.is_empty()).then_some(())?;
            out(
                Geometry::MultiLineString(crate::skeleton::skeleton_of(&polys)?),
                &g.crs,
            )
        }),
    );
}

/// The 3-D area of a surface's polygons.
pub(crate) fn surface_area(s: &Surface) -> f64 {
    if !s.is_polygonal() {
        return 0.0;
    }
    s.patches.iter().map(|p| norm(area_vector(p))).sum()
}

/// PostGIS ST_IsClosed of a 3-D geometry: a polyhedral surface or TIN when each edge is
/// shared by two patches (in opposite directions); a line when its ends meet.
pub(crate) fn surface_closed(s: &Surface) -> bool {
    match s.kind {
        SurfaceKind::PolyhedralSurface | SurfaceKind::Tin => is_closed(&s.patches),
        SurfaceKind::LineString | SurfaceKind::MultiLineString => {
            s.patches.iter().flatten().all(|l| l.first() == l.last())
        }
        _ => true,
    }
}

/// The `n`th (from 1) part of a 3-D geometry, as PostGIS ST_GeometryN: a patch of a
/// polyhedral surface or TIN, a member of a multi geometry, the geometry itself otherwise.
pub(crate) fn surface_n(s: &Surface, n: usize, crs: &Crs) -> Option<Term> {
    let i = n.checked_sub(1)?;
    let kind = match s.kind {
        SurfaceKind::Tin => SurfaceKind::Triangle,
        SurfaceKind::PolyhedralSurface | SurfaceKind::MultiPolygon => SurfaceKind::Polygon,
        SurfaceKind::MultiLineString => SurfaceKind::LineString,
        SurfaceKind::MultiPoint => SurfaceKind::Point,
        _ => {
            return (i == 0).then(|| geom_literal(&Geom::from_surface(s.clone(), crs.clone())));
        }
    };
    literal(
        Surface {
            has_z: s.has_z,
            ..Surface::new(kind, vec![s.patches.get(i)?.clone()])
        },
        crs,
    )
}
