//! WKT text of geometries, as the `wkt` crate writes it, with faster numbers.
//!
//! Functions that return a geometry spend half their time writing its coordinates: the `wkt`
//! crate formats each one with `Display`, whose shortest-digits search (Grisu, with Dragon as
//! fallback) dominates `geof:boundary` over 44,834 CORINE polygons. This writer finds the same
//! shortest digits with `zmij` and lays them out as `Display` does (no exponent, no `.0`), so
//! the text is byte for byte the `wkt` crate's.

use geo::{Coord, Geometry, LineString, Polygon};

/// Appends `x` as `format!("{x}")` writes it.
pub fn number(out: &mut String, x: f64) {
    if !x.is_finite() {
        out.push_str(if x.is_nan() {
            "NaN"
        } else if x > 0.0 {
            "inf"
        } else {
            "-inf"
        });
        return;
    }
    if may_tie(x) {
        use std::fmt::Write;
        let _ = write!(out, "{x}");
        return;
    }
    let mut buffer = zmij::Buffer::new();
    let s = buffer.format_finite(x);
    // Most numbers come without an exponent, as `Display` writes them but for an integer's ".0".
    if !s.as_bytes().contains(&b'e') {
        out.push_str(s.strip_suffix(".0").unwrap_or(s));
        return;
    }
    let (negative, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (mantissa, exponent) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i32>().unwrap_or(0)),
        None => (s, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    // The digits, and where the decimal point goes in them.
    let mut digits = [0u8; 40];
    let mut n = 0;
    for b in int.bytes().chain(frac.bytes()) {
        if n < digits.len() {
            digits[n] = b;
            n += 1;
        }
    }
    let mut point = int.len() as i32 + exponent;
    let mut start = 0;
    while start < n && digits[start] == b'0' {
        start += 1;
        point -= 1;
    }
    while n > start && digits[n - 1] == b'0' {
        n -= 1;
    }
    if negative {
        out.push('-');
    }
    let d = &digits[start..n];
    if d.is_empty() {
        out.push('0');
        return;
    }
    // ASCII digits only.
    let d = std::str::from_utf8(d).unwrap_or("0");
    let len = d.len() as i32;
    if point <= 0 {
        out.push_str("0.");
        for _ in 0..-point {
            out.push('0');
        }
        out.push_str(d);
    } else if point < len {
        out.push_str(&d[..point as usize]);
        out.push('.');
        out.push_str(&d[point as usize..]);
    } else {
        out.push_str(d);
        for _ in 0..point - len {
            out.push('0');
        }
    }
}

/// Whether two shortest digit strings of `x` can be equally near it, which `Display` breaks
/// upwards and `zmij` to even: only when the exact decimal value of `x` has at most 18
/// significant digits. Never for a coordinate with a fraction of 27 bits or more.
fn may_tie(x: f64) -> bool {
    let bits = x.to_bits();
    let exponent = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1 << 52) - 1);
    let (mut m, mut e) = if exponent == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1 << 52), exponent - 1075)
    };
    if m == 0 {
        return false;
    }
    let zeros = m.trailing_zeros();
    m >>= zeros;
    e += zeros as i32;
    if e >= 0 {
        // An integer: exact unless it needs more than 15 digits.
        return x.abs() >= 1e15;
    }
    // m / 2^-e = m * 5^-e / 10^-e: as many significant digits as m * 5^-e.
    if -e > 27 {
        return false;
    }
    u128::from(m) * 5u128.pow((-e) as u32) < 1_000_000_000_000_000_000
}

fn coord(out: &mut String, c: Coord<f64>) {
    number(out, c.x);
    out.push(' ');
    number(out, c.y);
}

fn sequence(out: &mut String, coords: impl IntoIterator<Item = Coord<f64>>) {
    out.push('(');
    for (i, c) in coords.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        coord(out, c);
    }
    out.push(')');
}

fn rings(out: &mut String, p: &Polygon<f64>) {
    sequence(out, p.exterior().0.iter().copied());
    for r in p.interiors() {
        out.push(',');
        sequence(out, r.0.iter().copied());
    }
}

fn line(out: &mut String, l: &LineString<f64>) {
    out.push_str("LINESTRING");
    if l.0.is_empty() {
        out.push_str(" EMPTY");
    } else {
        sequence(out, l.0.iter().copied());
    }
}

/// Appends the 2-D WKT of `g`, as `wkt::ToWkt::wkt_string` writes it.
pub fn write(out: &mut String, g: &Geometry<f64>) {
    match g {
        Geometry::Point(p) => {
            out.push_str("POINT(");
            coord(out, p.0);
            out.push(')');
        }
        Geometry::Line(l) => {
            out.push_str("LINESTRING");
            sequence(out, [l.start, l.end]);
        }
        Geometry::LineString(l) => line(out, l),
        Geometry::Polygon(p) => {
            out.push_str("POLYGON");
            if p.exterior().0.is_empty() {
                out.push_str(" EMPTY");
            } else {
                out.push('(');
                rings(out, p);
                out.push(')');
            }
        }
        Geometry::MultiPoint(m) => {
            out.push_str("MULTIPOINT");
            if m.0.is_empty() {
                out.push_str(" EMPTY");
            } else {
                out.push_str("((");
                for (i, p) in m.0.iter().enumerate() {
                    if i > 0 {
                        out.push_str("),(");
                    }
                    coord(out, p.0);
                }
                out.push_str("))");
            }
        }
        Geometry::MultiLineString(m) => {
            out.push_str("MULTILINESTRING");
            if m.0.is_empty() {
                out.push_str(" EMPTY");
            } else {
                out.push('(');
                for (i, l) in m.0.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    sequence(out, l.0.iter().copied());
                }
                out.push(')');
            }
        }
        Geometry::MultiPolygon(m) => {
            out.push_str("MULTIPOLYGON");
            if m.0.is_empty() {
                out.push_str(" EMPTY");
            } else {
                out.push_str("((");
                for (i, p) in m.0.iter().enumerate() {
                    if i > 0 {
                        out.push_str("),(");
                    }
                    rings(out, p);
                }
                out.push_str("))");
            }
        }
        Geometry::GeometryCollection(c) => {
            out.push_str("GEOMETRYCOLLECTION");
            if c.0.is_empty() {
                out.push_str(" EMPTY");
            } else {
                out.push('(');
                for (i, g) in c.0.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(out, g);
                }
                out.push(')');
            }
        }
        // As the `wkt` crate: through geo's polygon of the rectangle or triangle.
        Geometry::Rect(r) => write(out, &Geometry::Polygon(r.to_polygon())),
        Geometry::Triangle(t) => write(out, &Geometry::Polygon(t.to_polygon())),
    }
}

/// The 2-D WKT of `g`, as `wkt::ToWkt::wkt_string` writes it.
pub fn string(g: &Geometry<f64>) -> String {
    let mut out = String::with_capacity(64);
    write(&mut out, g);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use wkt::ToWkt;

    #[test]
    fn numbers_are_displays() {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let check = |x: f64| {
            let mut s = String::new();
            number(&mut s, x);
            assert_eq!(s, format!("{x}"), "{:#x}", x.to_bits());
        };
        for x in [
            0.0,
            -0.0,
            1.0,
            -1.0,
            0.1,
            0.5,
            1e-7,
            1e-300,
            5e-324,
            1e20,
            1e21,
            1e22,
            1.5e300,
            f64::MAX,
            f64::MIN,
            f64::MIN_POSITIVE,
            f64::EPSILON,
            123456789.0,
            4.35,
            60.0001,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            2f64.powi(53),
            2f64.powi(53) + 2.0,
            // Two shortest forms equally near: `Display` rounds the tie up.
            f64::from_bits(0x4317_9085_685d_83c9),
            2.5,
            0.125,
            1e15 + 0.5,
            4503599627370497.5,
            1e16 + 2.0,
            123456789012345.67,
        ] {
            check(x);
        }
        for _ in 0..1_000_000 {
            check(f64::from_bits(next()));
        }
        // Coordinates: degrees and metres with a few decimals, and computed ones.
        for _ in 0..500_000 {
            let r = next();
            let v = (r % 2_000_000_000) as f64 / 10f64.powi((r >> 40) as i32 % 12) - 90.0;
            check(v);
            check(v / 3.0);
        }
    }

    #[test]
    fn geometries_are_the_wkt_crates() {
        for w in [
            "POINT(1 2)",
            "POINT(-0.5 1e-7)",
            "LINESTRING(1 2,3.25 4)",
            "LINESTRING EMPTY",
            "POLYGON((0 0,1 0,1 1,0 0),(0.1 0.1,0.2 0.1,0.2 0.2,0.1 0.1))",
            "POLYGON EMPTY",
            "MULTIPOINT((1 2),(3 4))",
            "MULTIPOINT EMPTY",
            "MULTILINESTRING((1 2,3 4),(5 6,7 8))",
            "MULTILINESTRING EMPTY",
            "MULTIPOLYGON(((0 0,1 0,1 1,0 0)),((2 2,3 2,3 3,2 2),(2.1 2.1,2.2 2.1,2.2 2.2,2.1 2.1)))",
            "MULTIPOLYGON EMPTY",
            "GEOMETRYCOLLECTION(POINT(1 2),LINESTRING(1 2,3 4),GEOMETRYCOLLECTION EMPTY)",
            "GEOMETRYCOLLECTION EMPTY",
        ] {
            let g: Geometry<f64> = wkt::TryFromWkt::try_from_wkt_str(w).unwrap();
            assert_eq!(string(&g), g.wkt_string(), "{w}");
        }
        let r = Geometry::Rect(geo::Rect::new(
            Coord { x: 0.1, y: 2.0 },
            Coord { x: 3.0, y: 4.7 },
        ));
        assert_eq!(string(&r), r.wkt_string());
        let t = Geometry::Triangle(geo::Triangle::new(
            Coord { x: 0.0, y: 0.0 },
            Coord { x: 1.0, y: 0.0 },
            Coord { x: 0.0, y: 1.3 },
        ));
        assert_eq!(string(&t), t.wkt_string());
        let l = Geometry::Line(geo::Line::new(
            Coord { x: 0.0, y: 0.0 },
            Coord { x: 1.0, y: 1e-9 },
        ));
        assert_eq!(string(&l), l.wkt_string());
    }
}
