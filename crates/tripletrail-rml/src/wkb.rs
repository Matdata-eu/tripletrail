//! PostGIS geometries, which PostgreSQL gives as hex EWKB text, as GeoSPARQL WKT literals.

use std::fmt::Write as _;

/// Collections may nest this deep.
const MAX_DEPTH: usize = 32;

/// The WKT literal text of a hex (E)WKB geometry: with its SRID's CRS IRI first (none for
/// SRID 0 and 4326, which PostGIS stores longitude first, as CRS84), or `None` when it is not
/// WKB of a point, line string, polygon, their multi forms or a collection of them.
pub(crate) fn wkt_of_hex(hex: &str) -> Option<String> {
    let bytes = decode_hex(hex)?;
    let mut reader = Reader {
        bytes: &bytes,
        at: 0,
        little: true,
        // TripleTrail reads EPSG:4258 (ETRS89) latitude first, as EPSG defines it, while
        // PostGIS stores longitude first.
        swap: top_srid(&bytes) == Some(4258),
    };
    let mut wkt = String::new();
    let srid = reader.geometry(&mut wkt, 0, true)?;
    if reader.at != bytes.len() {
        return None;
    }
    Some(match srid {
        Some(srid) if srid != 0 && srid != 4326 => {
            format!("<http://www.opengis.net/def/crs/EPSG/0/{srid}> {wkt}")
        }
        _ => wkt,
    })
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    let hex = hex.as_bytes();
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    hex.chunks(2)
        .map(|p| Some(digit(p[0])? << 4 | digit(p[1])?))
        .collect()
}

/// The SRID in the header of the outer geometry.
fn top_srid(bytes: &[u8]) -> Option<u32> {
    let little = match bytes.first()? {
        0 => false,
        1 => true,
        _ => return None,
    };
    let word = |at: usize| -> Option<u32> {
        let b: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    };
    (word(1)? & 0x2000_0000 != 0).then(|| word(5)).flatten()
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
    little: bool,
    /// Write the first two coordinates the other way round.
    swap: bool,
}

#[derive(Clone, Copy)]
struct Dims {
    z: bool,
    m: bool,
}

impl Dims {
    fn count(self) -> usize {
        2 + usize::from(self.z) + usize::from(self.m)
    }

    fn tag(self) -> &'static str {
        match (self.z, self.m) {
            (false, false) => "",
            (true, false) => " Z",
            (false, true) => " M",
            (true, true) => " ZM",
        }
    }
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let b = self.bytes.get(self.at..self.at + N)?.try_into().ok()?;
        self.at += N;
        Some(b)
    }

    fn u32(&mut self) -> Option<u32> {
        let b = self.take::<4>()?;
        Some(if self.little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    fn f64(&mut self) -> Option<f64> {
        let b = self.take::<8>()?;
        Some(if self.little {
            f64::from_le_bytes(b)
        } else {
            f64::from_be_bytes(b)
        })
    }

    /// A count of items of at least `size` bytes each, no more than the bytes left hold.
    fn count(&mut self, size: usize) -> Option<usize> {
        let n = self.u32()? as usize;
        (n.checked_mul(size)? <= self.bytes.len() - self.at).then_some(n)
    }

    /// One geometry written to `out`, with the SRID of its header (read only at the top).
    fn geometry(&mut self, out: &mut String, depth: usize, top: bool) -> Option<Option<u32>> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.little = match self.take::<1>()?[0] {
            0 => false,
            1 => true,
            _ => return None,
        };
        let code = self.u32()?;
        let srid = if code & 0x2000_0000 != 0 {
            let srid = self.u32()?;
            top.then_some(srid)
        } else {
            None
        };
        // EWKB flags, or ISO codes (1001 Point Z, 2001 Point M, 3001 Point ZM).
        let iso = (code & 0x0FFF_FFFF) / 1000;
        let kind = (code & 0x0FFF_FFFF) % 1000;
        let dims = Dims {
            z: code & 0x8000_0000 != 0 || iso == 1 || iso == 3,
            m: code & 0x4000_0000 != 0 || iso == 2 || iso == 3,
        };
        if iso > 3 {
            return None;
        }
        let name = match kind {
            1 => "POINT",
            2 => "LINESTRING",
            3 => "POLYGON",
            4 => "MULTIPOINT",
            5 => "MULTILINESTRING",
            6 => "MULTIPOLYGON",
            7 => "GEOMETRYCOLLECTION",
            _ => return None,
        };
        out.push_str(name);
        out.push_str(dims.tag());
        let point = dims.count() * 8;
        match kind {
            1 => {
                let coords = self.coords(dims)?;
                if coords.iter().all(|c| c.is_nan()) {
                    out.push_str(" EMPTY");
                } else {
                    out.push('(');
                    write_coords(out, &coords);
                    out.push(')');
                }
            }
            2 => self.points(out, dims)?,
            3 => {
                let rings = self.count(4)?;
                self.list(out, rings, |r, out| r.points(out, dims))?;
            }
            4..=7 => {
                // Each member: a header of at least 5 bytes, and for points a coordinate.
                let n = self.count(if kind == 4 { 5 + point } else { 9 })?;
                let little = self.little;
                self.list(out, n, |r, out| {
                    let mut member = String::new();
                    r.geometry(&mut member, depth + 1, false)?;
                    r.little = little;
                    // Members are written without their type, except in a collection.
                    let body = match (kind, member.find('(')) {
                        (7, _) => member.as_str(),
                        (_, Some(i)) => &member[i..],
                        (_, None) => "EMPTY",
                    };
                    out.push_str(body);
                    Some(())
                })?;
            }
            _ => unreachable!("matched above"),
        }
        Some(srid)
    }

    fn coords(&mut self, dims: Dims) -> Option<Vec<f64>> {
        let mut c: Vec<f64> = (0..dims.count())
            .map(|_| self.f64())
            .collect::<Option<_>>()?;
        if self.swap {
            c.swap(0, 1);
        }
        Some(c)
    }

    /// A list of points: `(x y,x y)` or ` EMPTY`.
    fn points(&mut self, out: &mut String, dims: Dims) -> Option<()> {
        let n = self.count(dims.count() * 8)?;
        if n == 0 {
            out.push_str(" EMPTY");
            return Some(());
        }
        out.push('(');
        for i in 0..n {
            if i > 0 {
                out.push(',');
            }
            let coords = self.coords(dims)?;
            write_coords(out, &coords);
        }
        out.push(')');
        Some(())
    }

    /// `(a,b,c)` of `n` items each written by `item`, or ` EMPTY`.
    fn list(
        &mut self,
        out: &mut String,
        n: usize,
        mut item: impl FnMut(&mut Self, &mut String) -> Option<()>,
    ) -> Option<()> {
        if n == 0 {
            out.push_str(" EMPTY");
            return Some(());
        }
        out.push('(');
        for i in 0..n {
            if i > 0 {
                out.push(',');
            }
            let mut part = String::new();
            item(self, &mut part)?;
            // A ring or line string is written ` EMPTY` or `(…)`: inside a list, without
            // the space.
            out.push_str(part.trim_start());
        }
        out.push(')');
        Some(())
    }
}

fn write_coords(out: &mut String, coords: &[f64]) {
    for (i, c) in coords.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let _ = write!(out, "{c}");
    }
}

#[cfg(test)]
mod tests {
    use super::wkt_of_hex;

    #[test]
    fn postgis_geometries() {
        // SELECT ST_GeomFromText('POINT(10.5 59.9)', 4326)
        assert_eq!(
            wkt_of_hex("0101000020E610000000000000000025403333333333F34D40").as_deref(),
            Some("POINT(10.5 59.9)")
        );
        // SELECT 'SRID=25833;LINESTRING(1 2,3 4)'::geometry
        assert_eq!(
            wkt_of_hex(
                "0102000020E964000002000000000000000000F03F000000000000004000000000000008400000000000001040"
            )
            .as_deref(),
            Some("<http://www.opengis.net/def/crs/EPSG/0/25833> LINESTRING(1 2,3 4)")
        );
        // SELECT 'POINT Z(1 2 3)'::geometry
        assert_eq!(
            wkt_of_hex("0101000080000000000000F03F00000000000000400000000000000840").as_deref(),
            Some("POINT Z(1 2 3)")
        );
        // SELECT 'POLYGON((0 0,1 0,1 1,0 0))'::geometry
        assert_eq!(
            wkt_of_hex(
                "0103000000010000000400000000000000000000000000000000000000000000000000F03F0000000000000000000000000000F03F000000000000F03F00000000000000000000000000000000"
            )
            .as_deref(),
            Some("POLYGON((0 0,1 0,1 1,0 0))")
        );
        // SELECT 'MULTIPOINT((1 2),(3 4))'::geometry
        assert_eq!(
            wkt_of_hex(
                "0104000000020000000101000000000000000000F03F0000000000000040010100000000000000000008400000000000001040"
            )
            .as_deref(),
            Some("MULTIPOINT((1 2),(3 4))")
        );
        // SELECT 'GEOMETRYCOLLECTION(POINT(1 2),LINESTRING EMPTY)'::geometry
        assert_eq!(
            wkt_of_hex(
                "0107000000020000000101000000000000000000F03F0000000000000040010200000000000000"
            )
            .as_deref(),
            Some("GEOMETRYCOLLECTION(POINT(1 2),LINESTRING EMPTY)")
        );
        // SRID 4258 is written latitude first.
        assert_eq!(
            wkt_of_hex("0101000020A2100000000000000000F03F0000000000000040").as_deref(),
            Some("<http://www.opengis.net/def/crs/EPSG/0/4258> POINT(2 1)")
        );
        assert_eq!(
            wkt_of_hex("0101000000000000000000F87F000000000000F87F").as_deref(),
            Some("POINT EMPTY")
        );
    }

    #[test]
    fn bad_input_is_refused() {
        for bad in [
            "",
            "zz",
            "01",
            // A point cut short.
            "0101000000000000000000F03F",
            // Trailing bytes.
            "0101000000000000000000F03F000000000000004000",
            // A circular string (8).
            "0108000000000000",
            // A line string claiming 2^32-1 points.
            "0102000000FFFFFFFF",
        ] {
            assert_eq!(wkt_of_hex(bad), None, "{bad}");
        }
        // Collections nested deeper than the limit.
        let mut hex = String::new();
        for _ in 0..40 {
            hex.push_str("010700000001000000");
        }
        hex.push_str("0101000000000000000000F03F0000000000000040");
        assert_eq!(wkt_of_hex(&hex), None);
    }
}
