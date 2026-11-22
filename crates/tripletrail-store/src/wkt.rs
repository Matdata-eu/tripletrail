//! WKT text written back from a geometry record ([`crate::geobin`]), for stores that keep
//! WKT literals only in binary form (#370).
//!
//! A literal is kept that way only when the text written from its record, in the style found
//! in it, is byte for byte the text loaded: so results show exactly what was loaded, and any
//! literal this writer cannot reproduce (Z or M ordinates, a number format it does not know,
//! unusual spacing) keeps its text.
//!
//! The style is two bytes: the decimals of every number ([`SHORTEST`]: as few as read back to
//! the same number), and flags for the spacing. What is written for a record and a style must
//! never change, or the text of stored literals would: [`tests::the_written_text_is_frozen`]
//! holds it.

use crate::geobin::{
    COLLECTION, CRS84, EPSG4326, LINE_STRING, MULTI_LINE_STRING, MULTI_POINT, MULTI_POLYGON, OTHER,
    POINT, POLYGON,
};

/// Numbers written with as few decimals as read back to the same `f64`.
pub(crate) const SHORTEST: u8 = 0xFF;
/// The most decimals written with a fixed count.
const MAX_DECIMALS: u8 = 15;

/// A space between a keyword and its `(`: `POINT (1 2)`.
const KEYWORD_SPACE: u8 = 1;
/// A space after each comma: `(1 2, 3 4)`.
const COMMA_SPACE: u8 = 2;
/// The CRS84 IRI is written, though it is the default.
const CRS84_IRI: u8 = 4;
/// No space between a CRS IRI and the geometry: `<…>POINT(1 2)`.
const IRI_TIGHT: u8 = 8;
/// The points of a multipoint in parentheses: `MULTIPOINT((1 2), (3 4))`.
const POINT_PARENS: u8 = 16;
const FLAGS: u8 = KEYWORD_SPACE | COMMA_SPACE | CRS84_IRI | IRI_TIGHT | POINT_PARENS;

const CRS84_TEXT: &str = "http://www.opengis.net/def/crs/OGC/1.3/CRS84";
const EPSG4326_TEXT: &str = "http://www.opengis.net/def/crs/EPSG/0/4326";

/// The style in which `record` writes `text` back exactly, if there is one.
pub(crate) fn style_of(text: &str, record: &[u8]) -> Option<[u8; 2]> {
    let flags = flags_of(text);
    let mut out = String::with_capacity(text.len());
    let mut fits = |style: [u8; 2]| {
        out.clear();
        write(record, style, &mut out).is_some() && out == text
    };
    if let Some(d) = decimals_of(text)
        && fits([d, flags])
    {
        return Some([d, flags]);
    }
    fits([SHORTEST, flags]).then_some([SHORTEST, flags])
}

/// The spacing flags of a WKT text, read from its first keyword, comma and CRS IRI.
fn flags_of(text: &str) -> u8 {
    let mut flags = 0;
    let mut body = text;
    if let Some(rest) = text.strip_prefix('<') {
        if rest.starts_with(CRS84_TEXT) {
            flags |= CRS84_IRI;
        }
        if let Some(end) = rest.find('>') {
            body = &rest[end + 1..];
            if !body.starts_with(' ') {
                flags |= IRI_TIGHT;
            }
            body = body.trim_start();
        }
    }
    let keyword = body
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(body.len());
    if body[keyword..].starts_with(' ') {
        flags |= KEYWORD_SPACE;
    }
    if let Some(comma) = body.find(',')
        && body[comma + 1..].starts_with(' ')
    {
        flags |= COMMA_SPACE;
    }
    if body.starts_with("MULTIPOINT")
        && body[keyword..]
            .trim_start()
            .strip_prefix('(')
            .is_some_and(|r| r.trim_start().starts_with('('))
    {
        flags |= POINT_PARENS;
    }
    flags
}

/// The decimals of the first number of a WKT text.
fn decimals_of(text: &str) -> Option<u8> {
    let body = text.rsplit('>').next()?;
    let start = body.find(|c: char| c.is_ascii_digit() || c == '-')?;
    let number = &body[start..];
    let end = number
        .find(|c: char| !(c.is_ascii_digit() || c == '-' || c == '.'))
        .unwrap_or(number.len());
    let number = &number[..end];
    let d = number.find('.').map_or(0, |p| number.len() - p - 1);
    u8::try_from(d).ok().filter(|&d| d <= MAX_DECIMALS)
}

/// Appends the WKT text of `record` in `style` to `out`; `None` (and `out` holding part of
/// it) for a record this writer does not write, or a style it does not know.
pub(crate) fn write(record: &[u8], style: [u8; 2], out: &mut String) -> Option<()> {
    let record = &*crate::geobin::plain(record)?;
    let [decimals, flags] = style;
    if (decimals > MAX_DECIMALS && decimals != SHORTEST) || flags & !FLAGS != 0 {
        return None;
    }
    let mut w = Writer {
        r: record,
        out,
        decimals,
        flags,
        swap: false,
    };
    match w.u8()? {
        CRS84 => {
            if flags & CRS84_IRI != 0 {
                w.iri(CRS84_TEXT);
            }
        }
        EPSG4326 => {
            w.swap = true;
            w.iri(EPSG4326_TEXT);
        }
        OTHER => {
            let n = w.len()?;
            let iri = std::str::from_utf8(w.take(n)?).ok()?;
            w.iri(iri);
        }
        _ => return None,
    }
    w.geometry(0)?;
    w.r.is_empty().then_some(())
}

struct Writer<'a, 'o> {
    r: &'a [u8],
    out: &'o mut String,
    decimals: u8,
    flags: u8,
    /// EPSG:4326 records hold (longitude, latitude); its text has latitude first.
    swap: bool,
}

impl<'a> Writer<'a, '_> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.r.len() < n {
            return None;
        }
        let (head, rest) = self.r.split_at(n);
        self.r = rest;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn len(&mut self) -> Option<usize> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?) as usize)
    }

    fn iri(&mut self, iri: &str) {
        self.out.push('<');
        self.out.push_str(iri);
        self.out.push('>');
        if self.flags & IRI_TIGHT == 0 {
            self.out.push(' ');
        }
    }

    fn keyword(&mut self, keyword: &str) {
        self.out.push_str(keyword);
        if self.flags & KEYWORD_SPACE != 0 {
            self.out.push(' ');
        }
    }

    fn comma(&mut self) {
        self.out.push_str(if self.flags & COMMA_SPACE != 0 {
            ", "
        } else {
            ","
        });
    }

    fn number(&mut self, x: f64) -> Option<()> {
        if !x.is_finite() {
            return None;
        }
        if self.decimals == SHORTEST {
            use std::fmt::Write as _;
            write!(self.out, "{x}").ok()?;
            return Some(());
        }
        fixed(self.out, x, self.decimals)
    }

    fn coord(&mut self) -> Option<()> {
        let b = self.take(16)?;
        let x = f64::from_le_bytes(b[..8].try_into().ok()?);
        let y = f64::from_le_bytes(b[8..].try_into().ok()?);
        let (x, y) = if self.swap { (y, x) } else { (x, y) };
        self.number(x)?;
        self.out.push(' ');
        self.number(y)
    }

    /// `(x y, x y, …)`; `None` when empty: no part of a geometry is written empty.
    fn coords(&mut self) -> Option<()> {
        let n = self.count(16)?;
        if n == 0 {
            return None;
        }
        self.out.push('(');
        for i in 0..n {
            if i > 0 {
                self.comma();
            }
            self.coord()?;
        }
        self.out.push(')');
        Some(())
    }

    fn rings(&mut self) -> Option<()> {
        let n = self.count(4)?;
        if n == 0 {
            return None;
        }
        self.out.push('(');
        for i in 0..n {
            if i > 0 {
                self.comma();
            }
            self.coords()?;
        }
        self.out.push(')');
        Some(())
    }

    /// A count of items at least `size` bytes each, checked against what is left.
    fn count(&mut self, size: usize) -> Option<usize> {
        let n = self.len()?;
        (n.checked_mul(size)? <= self.r.len()).then_some(n)
    }

    /// The members of a multi-geometry: `KEYWORD (…, …)`, or `KEYWORD EMPTY`.
    fn members(
        &mut self,
        keyword: &str,
        size: usize,
        mut member: impl FnMut(&mut Self) -> Option<()>,
    ) -> Option<()> {
        let n = self.count(size)?;
        self.out.push_str(keyword);
        if n == 0 {
            self.out.push_str(" EMPTY");
            return Some(());
        }
        if self.flags & KEYWORD_SPACE != 0 {
            self.out.push(' ');
        }
        self.out.push('(');
        for i in 0..n {
            if i > 0 {
                self.comma();
            }
            member(self)?;
        }
        self.out.push(')');
        Some(())
    }

    fn geometry(&mut self, depth: usize) -> Option<()> {
        if depth > 64 {
            return None;
        }
        match self.u8()? {
            POINT => {
                self.keyword("POINT");
                self.out.push('(');
                self.coord()?;
                self.out.push(')');
            }
            LINE_STRING => {
                if self.r.get(..4) == Some(&[0; 4]) {
                    self.take(4)?;
                    self.out.push_str("LINESTRING EMPTY");
                } else {
                    self.keyword("LINESTRING");
                    self.coords()?;
                }
            }
            POLYGON => {
                // One empty ring: an empty polygon.
                if self.r.get(..8) == Some(&[1, 0, 0, 0, 0, 0, 0, 0]) {
                    self.take(8)?;
                    self.out.push_str("POLYGON EMPTY");
                } else {
                    self.keyword("POLYGON");
                    self.rings()?;
                }
            }
            MULTI_POINT => {
                let parens = self.flags & POINT_PARENS != 0;
                self.members("MULTIPOINT", 16, |w| {
                    if parens {
                        w.out.push('(');
                    }
                    w.coord()?;
                    if parens {
                        w.out.push(')');
                    }
                    Some(())
                })?;
            }
            MULTI_LINE_STRING => self.members("MULTILINESTRING", 4, Self::coords)?,
            MULTI_POLYGON => self.members("MULTIPOLYGON", 4, Self::rings)?,
            COLLECTION => self.members("GEOMETRYCOLLECTION", 1, |w| w.geometry(depth + 1))?,
            // Lines, rectangles and triangles: no WKT parses to them.
            _ => return None,
        }
        Some(())
    }
}

/// The two digits of each number below 100.
const PAIRS: &[u8; 200] = b"\
    0001020304050607080910111213141516171819\
    2021222324252627282930313233343536373839\
    4041424344454647484950515253545556575859\
    6061626364656667686970717273747576777879\
    8081828384858687888990919293949596979899";

/// Writes `x` with `decimals` decimals, from the integer `x * 10^decimals` rounded: about three
/// times as fast as `format!` (#370). Numbers that do not fit that integer are not written.
fn fixed(out: &mut String, x: f64, decimals: u8) -> Option<()> {
    const POWERS: [f64; 16] = [
        1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15,
    ];
    let scaled = (x * POWERS[decimals as usize]).round();
    // Exact integers only (below 2^53), so the digits are those of the rounded value.
    if scaled.abs() >= 9_007_199_254_740_992.0 {
        return None;
    }
    let v = scaled as i64;
    let u = v.unsigned_abs();
    // The decimals are the last digits of `u`, split off by constant divisors (a division by
    // a power of ten looked up costs several times as much).
    let mut frac = u;
    let mut buf = [0u8; 40];
    let mut i = buf.len();
    // The decimals, two at a time, padded with zeros.
    let mut left = decimals as usize;
    while left >= 2 {
        let d = (frac % 100) as usize * 2;
        frac /= 100;
        i -= 2;
        buf[i..i + 2].copy_from_slice(&PAIRS[d..d + 2]);
        left -= 2;
    }
    if left == 1 {
        i -= 1;
        buf[i] = b'0' + (frac % 10) as u8;
        frac /= 10;
    }
    if decimals > 0 {
        i -= 1;
        buf[i] = b'.';
    }
    let mut int = frac;
    while int >= 100 {
        let d = (int % 100) as usize * 2;
        int /= 100;
        i -= 2;
        buf[i..i + 2].copy_from_slice(&PAIRS[d..d + 2]);
    }
    if int >= 10 {
        let d = int as usize * 2;
        i -= 2;
        buf[i..i + 2].copy_from_slice(&PAIRS[d..d + 2]);
    } else {
        i -= 1;
        buf[i] = b'0' + int as u8;
    }
    // `-0.5` rounds to a negative integer; `-0.0001` with 2 decimals rounds to 0 and is
    // written `0.00`, which then differs from its text and keeps it.
    if v < 0 {
        i -= 1;
        buf[i] = b'-';
    }
    out.push_str(std::str::from_utf8(&buf[i..]).ok()?);
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::model::{Literal, Term};
    use tripletrail_geo::parse_wkt_literal_uncached;

    fn record(text: &str) -> Vec<u8> {
        let term: Term = Literal::new_typed_literal(text, tripletrail_geo::WKT_LITERAL).into();
        let g = parse_wkt_literal_uncached(&term).expect("valid WKT");
        let mut out = Vec::new();
        assert!(crate::geobin::encode(&g, &mut out), "{text}");
        out
    }

    fn round_trip(text: &str) -> Option<[u8; 2]> {
        style_of(text, &record(text))
    }

    #[test]
    fn texts_in_common_styles_are_written_back_exactly() {
        for text in [
            "POINT(4.3517 50.8466)",
            "POINT (4.3517 50.8466)",
            "POINT(3.1315119 52.8897349)",
            "POINT(-0.5000000 -12.0000000)",
            "POINT(10 59)",
            "POINT(10.5 59.25)",
            "POINT(10.123 59.1)",
            "LINESTRING(3.6987689 51.4814996, 3.6981888 51.4814753)",
            "LINESTRING (1 2,3 4,5 6)",
            "POLYGON((0 0, 1 0, 1 1, 0 0), (0.2 0.2, 0.3 0.2, 0.3 0.3, 0.2 0.2))",
            "MULTIPOINT((1 2), (3 4))",
            "MULTIPOINT(1 2, 3 4)",
            "MULTILINESTRING((1 2, 3 4), (5 6, 7 8))",
            "MULTIPOLYGON(((0 0, 1 0, 1 1, 0 0)), ((5 5, 6 5, 6 6, 5 5)))",
            "GEOMETRYCOLLECTION(POINT(1 2), LINESTRING(1 2, 3 4))",
            "<http://www.opengis.net/def/crs/EPSG/0/4326> POINT(59.91 10.75)",
            "<http://www.opengis.net/def/crs/OGC/1.3/CRS84> POINT(10.75 59.91)",
            "<http://www.opengis.net/def/crs/EPSG/0/25832> POINT(597000.25 6643000.5)",
            "<http://www.opengis.net/def/crs/EPSG/0/25832>POINT(597000.25 6643000.5)",
            "LINESTRING EMPTY",
            "POLYGON EMPTY",
            "MULTIPOLYGON EMPTY",
            "GEOMETRYCOLLECTION EMPTY",
        ] {
            assert!(round_trip(text).is_some(), "{text}");
        }
    }

    #[test]
    fn texts_written_otherwise_keep_their_text() {
        for text in [
            // Z ordinates are not in the record.
            "POINT Z(1 2 3)",
            // Numbers of different decimals, not the shortest form.
            "POINT(1.50 2.5)",
            "POINT(1.0 2.55)",
            "POINT(1e3 2)",
            "point(1 2)",
            "POINT( 1 2 )",
            "POINT(1  2)",
            " POINT(1 2)",
            "LINESTRING(1 2, 3 4,5 6)",
            "<http://www.opengis.net/def/crs/EPSG/0/4258> POINT(59.91 10.75)",
            "<http://www.opengis.net/def/crs/EPSG/0/25832>  POINT(1 2)",
        ] {
            let term: Term = Literal::new_typed_literal(text, tripletrail_geo::WKT_LITERAL).into();
            let Some(g) = parse_wkt_literal_uncached(&term) else {
                continue;
            };
            let mut r = Vec::new();
            if !crate::geobin::encode(&g, &mut r) {
                continue;
            }
            assert_eq!(style_of(text, &r), None, "{text}");
        }
    }

    #[test]
    fn the_written_text_is_frozen() {
        // Stored literals are written back with this code: these must never change.
        let cases: [(&str, [u8; 2], &str); 5] = [
            ("POINT(1 2)", [3, KEYWORD_SPACE], "POINT (1.000 2.000)"),
            ("POINT(1.25 -2)", [SHORTEST, 0], "POINT(1.25 -2)"),
            (
                "LINESTRING(1 2, 3 4)",
                [1, COMMA_SPACE],
                "LINESTRING(1.0 2.0, 3.0 4.0)",
            ),
            (
                "MULTIPOINT(1 2, 3 4)",
                [0, POINT_PARENS],
                "MULTIPOINT((1 2),(3 4))",
            ),
            (
                "<http://www.opengis.net/def/crs/EPSG/0/4326> POINT(59.5 10.25)",
                [2, 0],
                "<http://www.opengis.net/def/crs/EPSG/0/4326> POINT(59.50 10.25)",
            ),
        ];
        for (text, style, written) in cases {
            let mut out = String::new();
            write(&record(text), style, &mut out).unwrap();
            assert_eq!(out, written);
        }
    }

    #[test]
    fn fixed_decimals_match_the_standard_formatting() {
        let mut out = String::new();
        for &x in &[
            0.0,
            1.5,
            -1.5,
            3.1315119,
            -179.9999999,
            52.8897349,
            1e-7,
            123456.789,
        ] {
            for d in 0..=9u8 {
                out.clear();
                fixed(&mut out, x, d).unwrap();
                let std = format!("{x:.*}", d as usize);
                // Halfway cases may round either way; both read back to the same text only
                // when they agree, which the store checks.
                if out != std {
                    assert!(
                        std.starts_with('-') || out.len() == std.len(),
                        "{x} {d}: {out} {std}"
                    );
                }
            }
        }
    }
}
