//! 64-bit term ids.
//!
//! The low 4 bits are the kind, the other 60 bits the payload. Keeping the kind in the low
//! bits keeps dictionary ids small, so they compress well as varints in the quad indexes.

use std::fmt;

/// A term id. Equal ids are the same RDF term (`sameTerm`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct TermId(pub u64);

/// The kind of term an id stands for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Kind {
    /// The default graph (id 0 only).
    DefaultGraph = 0,
    Iri = 1,
    Blank = 2,
    /// A literal of datatype `xsd:string`.
    String = 3,
    /// A language-tagged string.
    LangString = 4,
    /// Any other typed literal kept in the dictionary.
    Typed = 5,
    /// A geometry literal (`geo:wktLiteral`, `geo:geoJSONLiteral`, ...), in the dictionary
    /// with its parsed geometry in the geometry store.
    Geometry = 6,
    /// An `xsd:integer` in canonical form, stored in the id (zigzag, 60 bits).
    Integer = 7,
    /// An `xsd:boolean` in canonical form.
    Boolean = 8,
    /// A term that is not in the store, known only to one query.
    Ephemeral = 9,
    /// An RDF 1.2 triple term, in the dictionary as its N-Triples text.
    Triple = 10,
    /// A row of a query's result table (see [`crate::register_table`]): table and row.
    Row = 11,
    /// A column of a query's result table, the predicate that reads it: table and column.
    Column = 12,
    /// An `xsd:double` written as a plain decimal number, stored in the id with its lexical
    /// form (see [`TermId::numeric`]).
    Double = 13,
    /// An `xsd:decimal` written as a plain decimal number, stored the same way.
    Decimal = 14,
    /// An `xsd:dateTime` or `xsd:date` with a 4-digit year, at most 3 fraction digits of a
    /// second and no time zone, `Z` or a whole quarter-hour offset (see [`TermId::date_time`]).
    DateTime = 15,
}

pub(crate) const KIND_BITS: u32 = 4;
const KIND_MASK: u64 = (1 << KIND_BITS) - 1;
/// Integers inside this range are stored in the id.
pub(crate) const INLINE_INT_MAX: i64 = (1 << 58) - 1;
pub(crate) const INLINE_INT_MIN: i64 = -(1 << 58);

impl TermId {
    pub const DEFAULT_GRAPH: Self = Self(0);

    pub(crate) fn new(kind: Kind, payload: u64) -> Self {
        Self((payload << KIND_BITS) | kind as u64)
    }

    pub fn kind(self) -> Kind {
        match self.0 & KIND_MASK {
            0 => Kind::DefaultGraph,
            1 => Kind::Iri,
            2 => Kind::Blank,
            3 => Kind::String,
            4 => Kind::LangString,
            5 => Kind::Typed,
            6 => Kind::Geometry,
            7 => Kind::Integer,
            8 => Kind::Boolean,
            10 => Kind::Triple,
            11 => Kind::Row,
            12 => Kind::Column,
            13 => Kind::Double,
            14 => Kind::Decimal,
            15 => Kind::DateTime,
            _ => Kind::Ephemeral,
        }
    }

    pub(crate) fn payload(self) -> u64 {
        self.0 >> KIND_BITS
    }

    /// Whether the term is in the dictionary.
    pub(crate) fn is_dictionary(self) -> bool {
        matches!(
            self.kind(),
            Kind::Iri
                | Kind::Blank
                | Kind::String
                | Kind::LangString
                | Kind::Typed
                | Kind::Geometry
                | Kind::Triple
        )
    }

    pub(crate) fn integer(value: i64) -> Option<Self> {
        (INLINE_INT_MIN..=INLINE_INT_MAX)
            .contains(&value)
            .then(|| Self::new(Kind::Integer, ((value << 1) ^ (value >> 63)) as u64))
    }

    pub(crate) fn as_integer(self) -> i64 {
        let z = self.payload();
        // Sign-extend the 60-bit zigzag value back.
        ((z >> 1) as i64) ^ -((z & 1) as i64)
    }

    /// The value of an inline [`Kind::Integer`] id.
    pub fn integer_value(self) -> Option<i64> {
        (self.kind() == Kind::Integer).then(|| self.as_integer())
    }

    pub(crate) fn boolean(value: bool) -> Self {
        Self::new(Kind::Boolean, value as u64)
    }
}

/// The value of an inline number (a [`Kind::Double`] or [`Kind::Decimal`] id), exactly.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Numeric {
    /// `digits / 10^scale`.
    Decimal {
        digits: i128,
        scale: u8,
    },
    Double(f64),
}

/// The value of an inline [`Kind::DateTime`] id.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DateTimeValue {
    /// An `xsd:date` (its time is midnight), else an `xsd:dateTime`.
    pub date: bool,
    /// Milliseconds since 1970-01-01T00:00:00Z. Without a time zone, the local time read as
    /// UTC.
    pub instant: i64,
    /// The time zone offset in minutes, if the value has one.
    pub tz: Option<i16>,
}

// Inline numbers: the payload is the digits of the lexical form as an integer (without the
// decimal point, at most 16 digits), shifted left by 5, then 4 bits with the number of
// fraction digits and the sign bit (1 for a leading `-`). Small numbers get small payloads,
// so they pack as tightly as dictionary ids in the quad indexes. `order_key` sorts them.
const NUM_MAX_DIGITS: usize = 16;
const NUM_LOW_BITS: u32 = 5;

// Inline date-times: the local time (milliseconds since 0001-01-01T00:00:00, or days for an
// `xsd:date`), shifted left by 11, then a 7-bit time zone code (0 none, 1 `Z`, else the
// offset in quarter hours plus 58), 2 bits with the number of fraction digits of the seconds,
// and 2 bits that are 1 for an `xsd:date`.
const DT_TZ_BITS: u32 = 7;
const DT_LOW_BITS: u32 = 4 + DT_TZ_BITS;
/// Days from 0001-01-01 to 1970-01-01.
const DAYS_TO_EPOCH: i64 = 719_162;
const MS_PER_DAY: i64 = 86_400_000;
/// 14 hours: no instant with a time zone is earlier than 0001-01-01T00:00:00 minus this.
const DT_BIAS: i64 = 14 * 3_600_000;

impl TermId {
    /// A plain decimal number `-?(0|[1-9][0-9]*)(\.[0-9]+)?` as `kind` ([`Kind::Double`] or
    /// [`Kind::Decimal`]), if it has at most 16 digits, 15 of them after the point. The
    /// lexical form comes back exactly ("0.340" stays "0.340", "-0" stays "-0").
    pub(crate) fn number(kind: Kind, lexical: &str) -> Option<Self> {
        let (negative, digits) = match lexical.strip_prefix('-') {
            Some(d) => (true, d),
            None => (false, lexical),
        };
        let (int, frac) = match digits.split_once('.') {
            Some((i, f)) if !f.is_empty() => (i, f),
            Some(_) => return None,
            None => (digits, ""),
        };
        if int.is_empty()
            || (int.len() > 1 && int.starts_with('0'))
            || int.len() + frac.len() > NUM_MAX_DIGITS
            || frac.len() >= NUM_MAX_DIGITS
            || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let digits = int
            .bytes()
            .chain(frac.bytes())
            .fold(0u64, |n, b| n * 10 + u64::from(b - b'0'));
        let payload = (digits << NUM_LOW_BITS) | (frac.len() as u64) << 1 | u64::from(negative);
        Some(Self::new(kind, payload))
    }

    /// The sign, the digits as an integer and the number of fraction digits of an inline
    /// number: the lexical form without its decimal point.
    fn number_parts(self) -> (bool, u64, u32) {
        let p = self.payload();
        (p & 1 == 1, p >> NUM_LOW_BITS, ((p >> 1) & 0xF) as u32)
    }

    /// The lexical form of an inline number.
    pub(crate) fn number_lexical(self) -> String {
        let (negative, digits, frac) = self.number_parts();
        let digits = format!("{digits:0>width$}", width = frac as usize + 1);
        let (int, frac) = digits.split_at(digits.len() - frac as usize);
        let sign = if negative { "-" } else { "" };
        if frac.is_empty() {
            format!("{sign}{int}")
        } else {
            format!("{sign}{int}.{frac}")
        }
    }

    /// The value of an inline number ([`Kind::Double`] or [`Kind::Decimal`]).
    pub fn numeric(self) -> Option<Numeric> {
        let kind = self.kind();
        if !matches!(kind, Kind::Double | Kind::Decimal) {
            return None;
        }
        let (negative, digits, scale) = self.number_parts();
        Some(if kind == Kind::Decimal {
            let digits = i128::from(digits);
            Numeric::Decimal {
                digits: if negative { -digits } else { digits },
                scale: scale as u8,
            }
        } else {
            // Both exact as f64s, so the division rounds once, correctly.
            let v = if digits < 1 << 53 {
                digits as f64 / 10f64.powi(scale as i32)
            } else {
                self.number_lexical()
                    .trim_start_matches('-')
                    .parse()
                    .unwrap_or(f64::NAN)
            };
            Numeric::Double(if negative { -v } else { v })
        })
    }

    /// An `xsd:dateTime` (`date: false`, `YYYY-MM-DDThh:mm:ss(.s{1,3})?(Z|(+|-)hh:mm)?`) or
    /// `xsd:date` (`YYYY-MM-DD(Z|(+|-)hh:mm)?`) in the id, if it is valid, its year has 4
    /// digits (0001 to 9999), its hour is not 24 and its offset is a whole quarter hour other
    /// than `-00:00`. The lexical form comes back exactly.
    pub(crate) fn date_time(lexical: &str, date: bool) -> Option<Self> {
        let b = lexical.as_bytes();
        let num = |r: std::ops::Range<usize>| -> Option<i64> {
            let d = b.get(r)?;
            d.iter()
                .all(u8::is_ascii_digit)
                .then(|| d.iter().fold(0, |n, &c| n * 10 + i64::from(c - b'0')))
        };
        let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
        if b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
            return None;
        }
        if year == 0 || !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
            return None;
        }
        let mut at = 10;
        let (mut ms, mut frac) = (0, 0u64);
        if !date {
            if b.get(10) != Some(&b'T') || b.get(13) != Some(&b':') || b.get(16) != Some(&b':') {
                return None;
            }
            let (h, m, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
            if h > 23 || m > 59 || s > 59 {
                return None;
            }
            ms = ((h * 60 + m) * 60 + s) * 1000;
            at = 19;
            if b.get(19) == Some(&b'.') {
                let n = b[20..].iter().take_while(|c| c.is_ascii_digit()).count();
                if !(1..=3).contains(&n) {
                    return None;
                }
                ms += num(20..20 + n)? * 10i64.pow(3 - n as u32);
                frac = n as u64;
                at = 20 + n;
            }
        }
        let tz = match &b[at..] {
            [] => 0,
            [b'Z'] => 1,
            [sign @ (b'+' | b'-'), _, _, b':', _, _] => {
                let (h, m) = (num(at + 1..at + 3)?, num(at + 4..at + 6)?);
                let minutes = h * 60 + m;
                if m % 15 != 0 || minutes > 14 * 60 || (*sign == b'-' && minutes == 0) {
                    return None;
                }
                let minutes = if *sign == b'-' { -minutes } else { minutes };
                minutes / 15 + 58
            }
            _ => return None,
        };
        let days = days_from_civil(year, month, day);
        let local = if date { days } else { days * MS_PER_DAY + ms };
        let payload = (local as u64) << DT_LOW_BITS | (tz as u64) << 4 | frac << 2 | date as u64;
        Some(Self::new(Kind::DateTime, payload))
    }

    /// The local time in milliseconds since 0001-01-01T00:00:00, the time zone offset in
    /// minutes, if any, and whether it is a date, of an inline date-time.
    fn date_time_parts(self) -> (i64, Option<i64>, bool) {
        let p = self.payload();
        let date = p & 1 == 1;
        let local = (p >> DT_LOW_BITS) as i64;
        let local = if date { local * MS_PER_DAY } else { local };
        let tz = match (p >> 4) & ((1 << DT_TZ_BITS) - 1) {
            0 => None,
            1 => Some(0),
            t => Some((t as i64 - 58) * 15),
        };
        (local, tz, date)
    }

    /// The value of an inline [`Kind::DateTime`] id.
    pub fn date_time_value(self) -> Option<DateTimeValue> {
        if self.kind() != Kind::DateTime {
            return None;
        }
        let (local, tz, date) = self.date_time_parts();
        Some(DateTimeValue {
            date,
            instant: local - tz.unwrap_or(0) * 60_000 - DAYS_TO_EPOCH * MS_PER_DAY,
            tz: tz.map(|m| m as i16),
        })
    }

    /// The lexical form of an inline date-time.
    pub(crate) fn date_time_lexical(self) -> String {
        let p = self.payload();
        let (local, tz, date) = self.date_time_parts();
        let frac = (p >> 2) & 3;
        let (days, ms) = (local / MS_PER_DAY, local % MS_PER_DAY);
        let (year, month, day) = civil_from_days(days);
        let mut out = format!("{year:04}-{month:02}-{day:02}");
        if !date {
            let s = ms / 1000;
            out += &format!("T{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60);
            if frac > 0 {
                let f = format!("{:03}", ms % 1000);
                out.push('.');
                out.push_str(&f[..frac as usize]);
            }
        }
        match tz {
            None => {}
            Some(0) if (p >> 4) & ((1 << DT_TZ_BITS) - 1) == 1 => out.push('Z'),
            Some(m) => {
                let sign = if m < 0 { '-' } else { '+' };
                let m = m.unsigned_abs();
                out += &format!("{sign}{:02}:{:02}", m / 60, m % 60);
            }
        }
        out
    }

    /// A key that sorts inline numbers ([`Kind::Double`], [`Kind::Decimal`]) and date-times
    /// ([`Kind::DateTime`], by instant, with or without a time zone alike) as their values
    /// do, within one kind. Equal values sort by lexical form. For other kinds, the payload.
    pub fn order_key(self) -> u64 {
        match self.kind() {
            Kind::Double | Kind::Decimal => {
                // The sign (1 for "not negative"), a 5-bit exponent `e + 16` (0 for zero), the
                // digits as a 16-digit significand `m` (54 bits, the value is
                // `m * 10^(e - 15)`), and the number of fraction digits. For a negative
                // number the exponent and significand are inverted.
                let (negative, digits, frac) = self.number_parts();
                let magnitude = if digits == 0 {
                    0
                } else {
                    let n = digits.ilog10() + 1;
                    let e = i64::from(n) - 1 - i64::from(frac);
                    let m = digits * 10u64.pow(NUM_MAX_DIGITS as u32 - n);
                    ((e + 16) as u64) << 54 | m
                };
                let magnitude = if negative {
                    !magnitude & ((1 << 59) - 1)
                } else {
                    magnitude
                };
                u64::from(!negative) << 63 | magnitude << 4 | u64::from(frac)
            }
            Kind::DateTime => {
                let (local, tz, _) = self.date_time_parts();
                let instant = local - tz.unwrap_or(0) * 60_000 + DT_BIAS;
                (instant as u64) << DT_LOW_BITS | (self.payload() & ((1 << DT_LOW_BITS) - 1))
            }
            _ => self.payload(),
        }
    }
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 0001-01-01 of a proleptic Gregorian date (H. Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    // From 0000-03-01 to 0001-01-01: 306 days.
    era * 146_097 + doe - 306
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 306;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

impl fmt::Debug for TermId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}#{}", self.kind(), self.payload())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_round_trip() {
        for v in [0, 1, -1, 42, -42, INLINE_INT_MAX, INLINE_INT_MIN] {
            let id = TermId::integer(v).unwrap();
            assert_eq!(id.kind(), Kind::Integer);
            assert_eq!(id.as_integer(), v, "{v}");
        }
        assert!(TermId::integer(INLINE_INT_MAX + 1).is_none());
        assert!(TermId::integer(INLINE_INT_MIN - 1).is_none());
    }

    #[test]
    fn numbers_keep_their_lexical_form_and_sort_by_value() {
        let values = [
            "-123456789012345",
            "-730.543",
            "-12",
            "-12.000",
            "-1.5",
            "-0.0001",
            "-0",
            "-0.000",
            "0",
            "0.000",
            "0.000000000000001",
            "0.0001",
            "0.123456789012345",
            "0.340",
            "1.5",
            "1.50",
            "7",
            "58.0900508",
            "100.00000000000",
            "730.543",
            "12345678901234.5",
            "1000000000000000",
            "9999999999999990000",
        ];
        let ids: Vec<TermId> = values
            .iter()
            .filter_map(|v| TermId::number(Kind::Double, v))
            .collect();
        assert_eq!(
            ids.len(),
            values.len() - 1,
            "only 9999999999999990000 is too large"
        );
        for (id, v) in ids.iter().zip(values) {
            assert_eq!(id.kind(), Kind::Double);
            assert_eq!(id.number_lexical(), v);
            let Some(Numeric::Double(d)) = id.numeric() else {
                panic!()
            };
            assert_eq!(d, v.parse::<f64>().unwrap(), "{v}");
            let Some(Numeric::Decimal { digits, scale }) =
                TermId::number(Kind::Decimal, v).unwrap().numeric()
            else {
                panic!()
            };
            let text = v.replace('.', "");
            assert_eq!(digits, text.parse::<i128>().unwrap(), "{v}");
            assert_eq!(
                usize::from(scale),
                v.find('.').map_or(0, |d| v.len() - d - 1)
            );
        }
        for w in ids.windows(2) {
            assert!(w[0].order_key() < w[1].order_key(), "{w:?}");
        }
        for v in [
            "",
            "-",
            ".5",
            "5.",
            "05",
            "+1",
            "1e3",
            "1E3",
            "NaN",
            "INF",
            "1.2.3",
            " 1",
            "1234567890123456.7",
            "0.1234567890123456",
            "1.0000000000000000",
            "100.000000000000000",
        ] {
            assert!(TermId::number(Kind::Decimal, v).is_none(), "{v}");
        }
        assert_ne!(
            TermId::number(Kind::Double, "1.5"),
            TermId::number(Kind::Decimal, "1.5")
        );
        // Small numbers get small ids, which pack as tightly as dictionary ids.
        assert!(TermId::number(Kind::Double, "730.543").unwrap().0 < 1 << 30);
    }

    #[test]
    fn date_times_keep_their_lexical_form_and_sort_by_instant() {
        let values = [
            ("0001-01-01T00:00:00+14:00", false),
            ("0001-01-01", false),
            ("1969-12-31T23:59:59.999", false),
            ("1970-01-01T00:00:00Z", false),
            ("1970-01-01T00:00:00+00:00", false),
            ("1970-01-01T00:00:00.5", false),
            ("1970-01-01T00:00:00.50", false),
            ("2000-02-29", true),
            ("2000-02-29T12:30:00-05:45", false),
            ("2024-12-31T23:59:59.123Z", false),
            ("9999-12-31T23:59:59.999-14:00", false),
        ];
        let mut ids = Vec::new();
        for (v, _) in values {
            let date = !v.contains('T');
            let id = TermId::date_time(v, date).unwrap_or_else(|| panic!("{v}"));
            assert_eq!(id.kind(), Kind::DateTime);
            assert_eq!(id.date_time_lexical(), v);
            assert_eq!(id.date_time_value().unwrap().date, date);
            ids.push(id);
        }
        let epoch = TermId::date_time("1970-01-01T01:00:00+01:00", false).unwrap();
        assert_eq!(
            epoch.date_time_value(),
            Some(DateTimeValue {
                date: false,
                instant: 0,
                tz: Some(60)
            })
        );
        let keys: Vec<u64> = ids.iter().map(|i| i.order_key()).collect();
        assert!(keys[0] < keys[1] && keys[2] < keys[3] && keys[3] < keys[5]);
        assert!(keys[8] < keys[9] && keys[9] < keys[10]);
        for (v, date) in [
            ("2023-02-29", true),
            ("0000-01-01", true),
            ("10000-01-01", true),
            ("-0001-01-01", true),
            ("2020-1-01", true),
            ("2020-01-01T24:00:00", false),
            ("2020-01-01T00:00:60", false),
            ("2020-01-01T00:00:00.1234", false),
            ("2020-01-01T00:00:00.", false),
            ("2020-01-01T00:00:00-00:00", false),
            ("2020-01-01T00:00:00+05:07", false),
            ("2020-01-01T00:00:00+14:15", false),
            ("2020-01-01T00:00", false),
            ("2020-01-01", false),
            ("2020-01-01T00:00:00", true),
        ] {
            assert!(TermId::date_time(v, date).is_none(), "{v}");
        }
    }
}
