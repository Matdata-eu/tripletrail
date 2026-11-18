//! `ST_Letters`: text as polygons, from a small built-in stroke font (A–Z, a–z, 0–9 and basic
//! punctuation) or a font given as JSON.
//!
//! The built-in glyphs are centre lines on a design grid (baseline 0, cap height 100, x-height
//! 64, descenders to -32), drawn with a stroke 14 units wide. Text starts at the origin on the
//! baseline, capitals are 100 units high, as in PostGIS.

use crate::functions::{Func, ST, func};
use crate::literal::{Crs, Geom, geom_literal};
use crate::postgis_processing::from_wkb;
use geo::algorithm::buffer::{Buffer, BufferStyle, LineCap, LineJoin};
use geo::{
    BooleanOps, BoundingRect, Coord, Geometry, LineString, MapCoords, MultiLineString,
    MultiPolygon, Polygon,
};
use oxigraph::model::Term;
use std::collections::HashMap;
use std::sync::LazyLock;

/// Glyph centre lines. Strokes are separated by `;`; a stroke is points `x,y` and arcs
/// `@cx,cy,rx,ry,from,to` (degrees, counter-clockwise when `to > from`). A stroke of one point
/// is a dot.
const GLYPHS: &[(char, &str)] = &[
    ('A', "0,0 30,100 60,0; 12,36 48,36"),
    (
        'B',
        "0,0 0,100 35,100 @35,75,20,25,90,-90 0,50; 0,50 38,50 @38,25,22,25,90,-90 0,0",
    ),
    ('C', "@33,50,33,50,50,310"),
    ('D', "0,0 0,100 25,100 @25,50,35,50,90,-90 0,0"),
    ('E', "60,100 0,100 0,0 60,0; 0,50 45,50"),
    ('F', "60,100 0,100 0,0; 0,50 45,50"),
    ('G', "@33,50,33,50,45,360 38,50"),
    ('H', "0,0 0,100; 60,0 60,100; 0,50 60,50"),
    ('I', "30,0 30,100; 12,100 48,100; 12,0 48,0"),
    ('J', "50,100 50,30 @28,30,22,30,0,-180"),
    ('K', "0,0 0,100; 58,100 0,38; 20,58 60,0"),
    ('L', "0,100 0,0 55,0"),
    ('M', "0,0 0,100 35,40 70,100 70,0"),
    ('N', "0,0 0,100 60,0 60,100"),
    ('O', "@33,50,33,50,0,360"),
    ('P', "0,0 0,100 35,100 @35,73,25,27,90,-90 0,46"),
    ('Q', "@33,50,33,50,0,360; 38,25 66,-5"),
    ('R', "0,0 0,100 35,100 @35,73,25,27,90,-90 0,46; 30,46 60,0"),
    ('S', "@30,75,28,25,20,270 @30,25,28,25,90,-160"),
    ('T', "0,100 64,100; 32,100 32,0"),
    ('U', "0,100 0,30 @30,30,30,30,180,360 60,100"),
    ('V', "0,100 30,0 60,100"),
    ('W', "0,100 18,0 37,70 56,0 74,100"),
    ('X', "0,0 60,100; 0,100 60,0"),
    ('Y', "0,100 30,50 60,100; 30,50 30,0"),
    ('Z', "0,100 60,100 0,0 60,0"),
    ('a', "@27,32,27,32,0,360; 54,64 54,0"),
    ('b', "0,100 0,0; @27,32,27,32,0,360"),
    ('c', "@28,32,28,32,45,315"),
    ('d', "@27,32,27,32,0,360; 54,100 54,0"),
    ('e', "0,32 56,32 @28,32,28,32,0,320"),
    ('f', "@38,80,14,18,40,180 24,0; 6,64 44,64"),
    ('g', "@27,32,27,32,0,360; 54,64 54,-12 @29,-12,25,20,0,-180"),
    ('h', "0,100 0,0; 0,40 @26,40,26,24,180,0 52,0"),
    ('i', "20,0 20,64; 20,88"),
    ('j', "30,64 30,-12 @14,-12,16,20,0,-180; 30,88"),
    ('k', "0,0 0,100; 50,64 0,24; 18,38 52,0"),
    ('l', "20,0 20,100"),
    (
        'm',
        "0,0 0,64; 0,44 @18,44,18,20,180,0 36,0; 36,44 @54,44,18,20,180,0 72,0",
    ),
    ('n', "0,0 0,64; 0,40 @26,40,26,24,180,0 52,0"),
    ('o', "@28,32,28,32,0,360"),
    ('p', "0,64 0,-32; @27,32,27,32,0,360"),
    ('q', "@27,32,27,32,0,360; 54,64 54,-32"),
    ('r', "0,0 0,64; 0,40 @26,40,26,24,180,60"),
    ('s', "@24,48,22,16,10,270 @24,16,22,16,90,-170"),
    ('t', "20,96 20,10 @32,10,12,10,180,300; 4,64 42,64"),
    ('u', "0,64 0,24 @26,24,26,24,180,360; 52,64 52,0"),
    ('v', "0,64 26,0 52,64"),
    ('w', "0,64 16,0 34,48 52,0 68,64"),
    ('x', "0,0 52,64; 0,64 52,0"),
    ('y', "0,64 27,0; 54,64 20,-32"),
    ('z', "0,64 52,64 0,0 52,0"),
    ('0', "@28,50,28,50,0,360"),
    ('1', "10,80 30,100 30,0; 10,0 50,0"),
    ('2', "@28,72,26,28,160,-30 0,0 56,0"),
    ('3', "@28,75,26,25,150,-90 @28,25,28,25,90,-150"),
    ('4', "40,0 40,100 0,30 58,30"),
    ('5', "54,100 6,100 4,52 @28,32,28,32,135,-150"),
    ('6', "@30,60,28,40,60,180 2,32; @29,32,27,32,0,360"),
    ('7', "0,100 56,100 20,0"),
    ('8', "@28,76,24,24,0,360; @28,26,28,26,0,360"),
    ('9', "@27,68,27,32,0,360; 54,68 54,40 @26,40,28,40,0,-120"),
    ('.', "10,4"),
    (',', "12,6 6,-14"),
    (':', "10,4; 10,50"),
    (';', "10,50; 12,6 6,-14"),
    ('!', "10,30 10,100; 10,4"),
    ('?', "@28,75,26,25,160,-70 28,30; 28,4"),
    ('\'', "10,100 10,72"),
    ('"', "6,100 6,72; 26,100 26,72"),
    ('-', "0,44 36,44"),
    ('+', "0,44 52,44; 26,18 26,70"),
    ('=', "0,30 52,30; 0,58 52,58"),
    ('(', "@40,50,30,60,120,240"),
    (')', "@0,50,30,60,60,-60"),
    ('/', "0,-10 40,100"),
    ('_', "0,-20 56,-20"),
    ('#', "10,0 22,100; 34,0 46,100; 0,34 56,34; 4,66 60,66"),
    ('*', "26,100 26,50; 4,88 48,62; 4,62 48,88"),
];

/// Design units to output units: the outline of a capital spans 0 to 100.
const SCALE: f64 = 0.86;
const HALF_STROKE: f64 = 7.0;

fn stroke(spec: &str) -> Option<Vec<Coord<f64>>> {
    let mut pts: Vec<Coord<f64>> = Vec::new();
    for tok in spec.split_whitespace() {
        let nums: Vec<f64> = tok
            .trim_start_matches('@')
            .split(',')
            .map(|v| v.parse().ok())
            .collect::<Option<_>>()?;
        if tok.starts_with('@') {
            let [cx, cy, rx, ry, a0, a1] = nums[..] else {
                return None;
            };
            let n = ((a1 - a0).abs() / 10.0).ceil().max(1.0) as usize;
            for i in 0..=n {
                let a = (a0 + (a1 - a0) * i as f64 / n as f64).to_radians();
                pts.push(Coord {
                    x: cx + rx * a.cos(),
                    y: cy + ry * a.sin(),
                });
            }
        } else {
            let [x, y] = nums[..] else { return None };
            pts.push(Coord { x, y });
        }
    }
    pts.dedup_by(|a, b| (a.x - b.x).abs() < 1e-9 && (a.y - b.y).abs() < 1e-9);
    Some(
        pts.into_iter()
            .map(|c| Coord {
                x: c.x * SCALE,
                y: HALF_STROKE + c.y * SCALE,
            })
            .collect(),
    )
}

fn square(c: Coord<f64>) -> Polygon<f64> {
    let h = HALF_STROKE;
    geo::Rect::new(
        Coord {
            x: c.x - h,
            y: c.y - h,
        },
        Coord {
            x: c.x + h,
            y: c.y + h,
        },
    )
    .to_polygon()
}

fn outline(spec: &str) -> MultiPolygon<f64> {
    let strokes: Vec<Vec<Coord<f64>>> = spec
        .split(';')
        .map(|s| stroke(s).expect("built-in glyphs parse"))
        .collect();
    let lines: Vec<LineString<f64>> = strokes
        .iter()
        .filter(|s| s.len() > 1)
        .map(|s| LineString::new(s.clone()))
        .collect();
    let style = BufferStyle::new(HALF_STROKE)
        .line_cap(LineCap::Square)
        .line_join(LineJoin::Miter(1.2));
    let mut shape = MultiLineString(lines).buffer_with_style(style);
    for dot in strokes.iter().filter(|s| s.len() == 1) {
        shape = shape.union(&MultiPolygon(vec![square(dot[0])]));
    }
    shape
}

static FONT: LazyLock<HashMap<char, MultiPolygon<f64>>> =
    LazyLock::new(|| GLYPHS.iter().map(|(c, s)| (*c, outline(s))).collect());

/// A glyph of a JSON font: hex WKB (as PostGIS's fonts), or WKT.
fn font_glyph(v: &serde_json::Value) -> Option<Geometry<f64>> {
    let s = v.as_str()?.trim();
    let term: Term = if s.bytes().all(|b| b.is_ascii_hexdigit()) {
        from_wkb(s, Some(4326))?
    } else {
        oxigraph::model::Literal::new_typed_literal(s, crate::literal::WKT_LITERAL).into()
    };
    Some(crate::functions::geom(&term)?.geometry.clone())
}

/// The polygons of `text`: each glyph is moved so that its left edge is where the previous
/// one ended plus a gap; a space advances by 40 % of the text height.
pub(crate) fn letters(text: &str, font: Option<&serde_json::Value>) -> Option<Geometry<f64>> {
    let custom: Option<HashMap<char, Geometry<f64>>> = match font {
        Some(serde_json::Value::Object(m)) => Some(
            m.iter()
                .filter_map(|(k, v)| {
                    let mut chars = k.chars();
                    let c = chars.next()?;
                    chars.next().is_none().then_some(())?;
                    Some((c, font_glyph(v)))
                })
                .map(|(c, g)| Some((c, g?)))
                .collect::<Option<_>>()?,
        ),
        Some(serde_json::Value::Null) | None => None,
        Some(_) => return None,
    };
    let glyph = |c: char| -> Option<Geometry<f64>> {
        match &custom {
            Some(f) => f.get(&c).cloned(),
            None => FONT.get(&c).cloned().map(Geometry::MultiPolygon),
        }
    };
    // Text height: 100 for the built-in font, the tallest glyph of a custom one.
    let height = match &custom {
        None => 100.0,
        Some(f) => f
            .values()
            .filter_map(|g| g.bounding_rect())
            .map(|r| r.height())
            .fold(0.0, f64::max),
    };
    let (gap, space) = (0.12 * height, 0.4 * height);
    let mut cursor = 0.0;
    let mut polys: Vec<Polygon<f64>> = Vec::new();
    for c in text.chars() {
        if c.is_whitespace() {
            cursor += space;
            continue;
        }
        let g = glyph(c)?;
        let Some(r) = g.bounding_rect() else {
            continue;
        };
        let dx = cursor - r.min().x;
        let moved = g.map_coords(|p| Coord {
            x: p.x + dx,
            y: p.y,
        });
        polys.extend(crate::postgis_processing::polygons_of(&moved));
        cursor += r.width() + gap;
    }
    Some(Geometry::MultiPolygon(MultiPolygon(polys)))
}

/// Registers the functions of this module.
pub(crate) fn add_functions(add: &mut dyn FnMut(&str, &str, Func)) {
    add(
        ST,
        "ST_Letters",
        func(|args| {
            let Some(Term::Literal(text)) = args.first() else {
                return None;
            };
            let font: Option<serde_json::Value> = match args.get(1) {
                None => None,
                Some(Term::Literal(l)) => Some(serde_json::from_str(l.value()).ok()?),
                Some(_) => return None,
            };
            let g = letters(text.value(), font.as_ref())?;
            Some(geom_literal(&Geom::new(g, Crs::Crs84)))
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{Area, Validation};

    #[test]
    fn every_glyph_is_a_valid_outline_of_the_right_height() {
        for (c, _) in GLYPHS {
            let g = &FONT[c];
            assert!(g.is_valid(), "{c}: {:?}", g.validation_errors());
            assert!(g.unsigned_area() > 0.0, "{c}");
            let r = g.bounding_rect().unwrap();
            assert!(r.min().y > -40.0 && r.max().y < 106.0, "{c}: {r:?}");
        }
        let a = FONT[&'H'].bounding_rect().unwrap();
        assert!(
            (a.min().y).abs() < 1e-9 && (a.max().y - 100.0).abs() < 1e-9,
            "{a:?}"
        );
        // Counters stay open.
        assert_eq!(FONT[&'O'].0.len(), 1);
        assert_eq!(FONT[&'O'].0[0].interiors().len(), 1);
        assert_eq!(FONT[&'B'].0[0].interiors().len(), 2);
        assert_eq!(FONT[&'i'].0.len(), 2);
    }
}
