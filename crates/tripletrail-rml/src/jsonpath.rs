//! The JSONPath that RML iterators and references use: `$`, `.key`, `['key']`, `[n]`
//! (negative from the end), `[*]` and `.*`, `..key` and `..*` (recursive descent), and unions
//! of keys or indexes `['a','b']`, `[0,2]`. Filters and slices are not supported.
//!
//! A reference relative to the iterated record may leave out the `$`: `name`,
//! `address.city`, `['first name']` and `@.name` all read from the record.

use crate::RmlError;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// The members with these keys.
    Keys(Vec<String>),
    /// The elements at these indexes, negative ones counted from the end.
    Indexes(Vec<i64>),
    /// All members or elements.
    All,
    /// The members with this key, at any depth below (`..key`).
    Descend(String),
    /// Every value at any depth below (`..*`).
    DescendAll,
}

/// A parsed JSONPath.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path(pub Vec<Step>);

impl Path {
    pub fn parse(path: &str) -> Result<Self, RmlError> {
        let err = |message: &str| RmlError::Path {
            path: path.to_owned(),
            message: message.to_owned(),
        };
        let s = path.trim();
        let mut rest = if let Some(r) = s.strip_prefix('$').or_else(|| s.strip_prefix('@')) {
            r
        } else if s.starts_with('[') || s.starts_with('.') {
            s
        } else if s.is_empty() {
            return Err(err("empty path"));
        } else {
            // A relative reference: `name`, `a.b`.
            return Self::parse(&format!("$.{s}")).map_err(|_| err("invalid path"));
        };
        let mut steps = Vec::new();
        while !rest.is_empty() {
            if let Some(r) = rest.strip_prefix("..") {
                if let Some(r) = r.strip_prefix('*') {
                    steps.push(Step::DescendAll);
                    rest = r;
                } else if r.starts_with('[') {
                    let (step, r) = bracket(r).ok_or_else(|| err("invalid brackets"))?;
                    match step {
                        Step::Keys(k) if k.len() == 1 => {
                            steps.push(Step::Descend(k.into_iter().next().unwrap()))
                        }
                        Step::All => steps.push(Step::DescendAll),
                        _ => return Err(err("only one key can follow `..`")),
                    }
                    rest = r;
                } else {
                    let (name, r) = name(r);
                    if name.is_empty() {
                        return Err(err("a key or `*` must follow `..`"));
                    }
                    steps.push(Step::Descend(name.to_owned()));
                    rest = r;
                }
            } else if let Some(r) = rest.strip_prefix('.') {
                if let Some(r) = r.strip_prefix('*') {
                    steps.push(Step::All);
                    rest = r;
                } else {
                    let (name, r) = name(r);
                    if name.is_empty() {
                        return Err(err("a key or `*` must follow `.`"));
                    }
                    steps.push(Step::Keys(vec![name.to_owned()]));
                    rest = r;
                }
            } else if rest.starts_with('[') {
                if rest[1..].trim_start().starts_with('?') {
                    return Err(err("filters are not supported"));
                }
                let (step, r) = bracket(rest).ok_or_else(|| {
                    err(
                        if rest[..rest.find(']').unwrap_or(rest.len())].contains(':')
                            && !rest[1..].trim_start().starts_with(['\'', '"'])
                        {
                            "slices are not supported"
                        } else {
                            "invalid brackets"
                        },
                    )
                })?;
                steps.push(step);
                rest = r;
            } else {
                return Err(err("expected `.`, `..` or `[`"));
            }
        }
        Ok(Self(steps))
    }

    /// Appends the values the path selects in `root`, in document order.
    pub fn select<'a>(&self, root: &'a Value, out: &mut Vec<&'a Value>) {
        let mut current = vec![root];
        let mut next = Vec::new();
        for step in &self.0 {
            for v in current.drain(..) {
                apply(step, v, &mut next);
            }
            std::mem::swap(&mut current, &mut next);
        }
        out.extend(current);
    }
}

/// A key after `.`: up to the next `.` or `[`.
fn name(s: &str) -> (&str, &str) {
    let end = s.find(['.', '[']).unwrap_or(s.len());
    (&s[..end], &s[end..])
}

/// `[...]` at the start of `s`: the step and what follows it.
fn bracket(s: &str) -> Option<(Step, &str)> {
    let mut rest = s.strip_prefix('[')?.trim_start();
    if let Some(r) = rest.strip_prefix('*') {
        return Some((Step::All, r.trim_start().strip_prefix(']')?));
    }
    if rest.starts_with('\'') || rest.starts_with('"') {
        let mut keys = Vec::new();
        loop {
            let quote = rest.chars().next()?;
            if quote != '\'' && quote != '"' {
                return None;
            }
            let mut key = String::new();
            let mut chars = rest[1..].char_indices();
            let end = loop {
                let (i, c) = chars.next()?;
                match c {
                    '\\' => key.push(chars.next()?.1),
                    c if c == quote => break i + 2,
                    c => key.push(c),
                }
            };
            keys.push(key);
            rest = rest[end..].trim_start();
            if let Some(r) = rest.strip_prefix(',') {
                rest = r.trim_start();
            } else {
                return Some((Step::Keys(keys), rest.strip_prefix(']')?));
            }
        }
    }
    let end = rest.find(']')?;
    let indexes = rest[..end]
        .split(',')
        .map(|i| i.trim().parse().ok())
        .collect::<Option<Vec<i64>>>()?;
    Some((Step::Indexes(indexes), &rest[end + 1..]))
}

fn apply<'a>(step: &Step, v: &'a Value, out: &mut Vec<&'a Value>) {
    match step {
        Step::Keys(keys) => {
            if let Value::Object(o) = v {
                out.extend(keys.iter().filter_map(|k| o.get(k)));
            }
        }
        Step::Indexes(indexes) => {
            if let Value::Array(a) = v {
                for &i in indexes {
                    let i = if i < 0 { a.len() as i64 + i } else { i };
                    if let Some(e) = usize::try_from(i).ok().and_then(|i| a.get(i)) {
                        out.push(e);
                    }
                }
            }
        }
        Step::All => match v {
            Value::Object(o) => out.extend(o.values()),
            Value::Array(a) => out.extend(a),
            _ => {}
        },
        Step::Descend(key) => descend(v, &mut |c, k| {
            if k == Some(key.as_str()) {
                out.push(c)
            }
        }),
        Step::DescendAll => descend(v, &mut |c, _| out.push(c)),
    }
}

/// Calls `f` on every value below `v` (not `v` itself), in document order, with its key when
/// it is an object member.
fn descend<'a>(v: &'a Value, f: &mut impl FnMut(&'a Value, Option<&str>)) {
    match v {
        Value::Object(o) => {
            for (k, c) in o {
                f(c, Some(k));
                descend(c, f);
            }
        }
        Value::Array(a) => {
            for c in a {
                f(c, None);
                descend(c, f);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sel(path: &str, v: &Value) -> Vec<Value> {
        let mut out = Vec::new();
        Path::parse(path).unwrap().select(v, &mut out);
        out.into_iter().cloned().collect()
    }

    #[test]
    fn selects_like_jsonpath() {
        let v = json!({
            "people": [
                {"name": "Ada", "tags": ["a", "b"], "address": {"city": "Ghent"}},
                {"name": "Bob", "first name": "B", "address": {"city": "Leuven"}}
            ],
            "count": 2
        });
        assert_eq!(sel("$.people[*].name", &v), [json!("Ada"), json!("Bob")]);
        assert_eq!(sel("$['people'][1]['first name']", &v), [json!("B")]);
        assert_eq!(sel("$.people[-1].name", &v), [json!("Bob")]);
        assert_eq!(sel("$.people[0,1].address.city", &v).len(), 2);
        assert_eq!(sel("$..city", &v), [json!("Ghent"), json!("Leuven")]);
        assert_eq!(sel("$.people[0].tags.*", &v), [json!("a"), json!("b")]);
        assert_eq!(sel("$", &v), std::slice::from_ref(&v));
        // Relative references, as RML uses them on a record.
        let ada = &v["people"][0];
        assert_eq!(sel("name", ada), [json!("Ada")]);
        assert_eq!(sel("address.city", ada), [json!("Ghent")]);
        assert_eq!(sel("@.tags[1]", ada), [json!("b")]);
        assert_eq!(sel("['name']", ada), [json!("Ada")]);
        assert!(sel("missing.key", ada).is_empty());
        assert!(sel("name.deeper", ada).is_empty());
    }

    #[test]
    fn rejects_what_is_not_supported() {
        assert!(Path::parse("$.people[?(@.age > 2)]").is_err());
        assert!(Path::parse("$.people[0:2]").is_err());
        assert!(Path::parse("$.").is_err());
        assert!(Path::parse("").is_err());
        assert!(Path::parse("$['unclosed").is_err());
        // A colon inside a quoted key is a key, not a slice.
        assert_eq!(
            Path::parse("$['a:b']").unwrap(),
            Path(vec![Step::Keys(vec!["a:b".into()])])
        );
    }
}
