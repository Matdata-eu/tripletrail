//! Times for time-dependent routing: plain numbers, or `xsd:dateTime` literals counted in
//! seconds since 1970-01-01T00:00:00Z.

use oxigraph::model::{Literal, NamedNode, Term};

const DATE_TIME: &str = "http://www.w3.org/2001/XMLSchema#dateTime";
const DATE_TIME_STAMP: &str = "http://www.w3.org/2001/XMLSchema#dateTimeStamp";

/// The time a term gives, and whether it was a date-time.
pub(crate) fn seconds(t: &Term) -> Option<(f64, bool)> {
    let Term::Literal(l) = t else { return None };
    let dt = l.datatype().as_str();
    if dt == DATE_TIME || dt == DATE_TIME_STAMP {
        return parse_date_time(l.value()).map(|s| (s, true));
    }
    l.value().trim().parse().ok().map(|s| (s, false))
}

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `YYYY-MM-DDThh:mm:ss[.fff][Z|(+|-)hh:mm]`; without a zone the time is taken as UTC.
fn parse_date_time(s: &str) -> Option<f64> {
    let s = s.trim();
    let (date, rest) = s.split_once('T')?;
    let (neg, date) = match date.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, date),
    };
    let mut parts = date.splitn(3, '-');
    let y: i64 = parts.next()?.parse().ok()?;
    let y = if neg { -y } else { y };
    let m: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;
    let (time, offset) = if let Some(t) = rest.strip_suffix('Z') {
        (t, 0)
    } else if rest.len() > 6 && matches!(rest.as_bytes()[rest.len() - 6], b'+' | b'-') {
        let (t, z) = rest.split_at(rest.len() - 6);
        let sign = if z.starts_with('-') { -1 } else { 1 };
        let (h, mi) = z[1..].split_once(':')?;
        (
            t,
            sign * (h.parse::<i64>().ok()? * 3600 + mi.parse::<i64>().ok()? * 60),
        )
    } else {
        (rest, 0)
    };
    let mut hms = time.splitn(3, ':');
    let h: i64 = hms.next()?.parse().ok()?;
    let mi: i64 = hms.next()?.parse().ok()?;
    let sec: f64 = hms.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let whole = days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60 - offset;
    Some(whole as f64 + sec)
}

/// A time as a literal: an `xsd:dateTime` in UTC when `date_time`, else a number.
pub(crate) fn literal(seconds: f64, date_time: bool) -> Term {
    if !date_time || !seconds.is_finite() {
        return Literal::from(seconds).into();
    }
    let whole = seconds.floor();
    let days = (whole / 86_400.0).floor() as i64;
    let in_day = seconds - days as f64 * 86_400.0;
    let (y, m, d) = civil_from_days(days);
    let h = (in_day / 3600.0).floor();
    let mi = ((in_day - h * 3600.0) / 60.0).floor();
    let s = in_day - h * 3600.0 - mi * 60.0;
    let sec = if s.fract().abs() < 1e-9 {
        format!("{:02}", s.round() as i64)
    } else {
        let t = format!("{s:06.3}");
        t.trim_end_matches('0').to_owned()
    };
    Literal::new_typed_literal(
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{sec}Z",
            h as i64, mi as i64
        ),
        NamedNode::new_unchecked(DATE_TIME),
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> Term {
        Literal::new_typed_literal(s, NamedNode::new_unchecked(DATE_TIME)).into()
    }

    #[test]
    fn date_times() {
        assert_eq!(seconds(&dt("1970-01-01T00:00:00Z")), Some((0.0, true)));
        assert_eq!(
            seconds(&dt("2024-03-01T12:30:00+01:00")),
            Some((1_709_292_600.0, true))
        );
        assert_eq!(seconds(&Literal::from(90).into()), Some((90.0, false)));
        assert_eq!(
            literal(1_709_292_600.0 + 90.5, true),
            dt("2024-03-01T11:31:30.5Z")
        );
        assert_eq!(literal(0.0, true), dt("1970-01-01T00:00:00Z"));
        let t = seconds(&dt("2000-02-29T23:59:59Z")).unwrap().0;
        assert_eq!(literal(t, true), dt("2000-02-29T23:59:59Z"));
    }
}
