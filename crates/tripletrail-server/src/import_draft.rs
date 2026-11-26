//! The Import page's drafted lift (#275): the file in its own vocabulary, mechanically. The
//! draft is made from the first triples of the file's Facade-X graph:
//!
//! - one class per kind of record: the rows of a table (named after the file), the elements
//!   of one name in XML, the objects under one key in JSON or YAML, and the document itself
//!   when it has keys of its own;
//! - one property per column, key or attribute, with values typed from what they look like
//!   (`xsd:integer`, `xsd:decimal`, `xsd:double`, `xsd:boolean`, `xsd:date`,
//!   `xsd:dateTime`) when the file has only text, and as the file types them otherwise;
//! - links from a record to the records in it, under the key or element name that holds them;
//! - IRIs from a key column (`fx:entity`), chosen by the caller or found (unique, on every
//!   record, named like an id), else from a row's position, else blank nodes;
//! - `prov:wasDerivedFrom` the file on every record.
//!
//! Everything is minted in the caller's source namespace. Nothing is interpreted and nothing
//! dropped but empty values: that is the uplift's work (#283).

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use tripletrail::oxigraph::model::{Literal, Term};

const FX: &str = "http://sparql.xyz/facade-x/ns/";
const XYZ: &str = "http://sparql.xyz/facade-x/data/";
const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

/// Kinds of records a draft maps at most.
const MAX_KINDS: usize = 20;
/// Keys of one kind a draft maps at most.
const MAX_KEYS: usize = 60;

/// How the records of a kind are found in the Facade-X graph.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum How {
    /// XML elements of one name: `?n a xyz:NAME`.
    Element(String),
    /// Objects under a key (`array`: in a list under it): `?p xyz:KEY ?n`.
    Field { key: String, array: bool },
    /// The rows in the document's slots: `?root a fx:Root ; rdf:_1 ?n`.
    Row,
    /// The document itself, when it has keys.
    Root,
}

/// A literal-valued key of a kind.
struct Column {
    /// The key's IRI in the Facade-X graph.
    iri: String,
    /// The key's name (decoded), which names its property.
    name: String,
    /// The values are in a list under the key.
    array: bool,
    /// A record has more than one value.
    many: bool,
    /// The values seen, as text, when the file gives them all as plain strings.
    texts: Option<Vec<String>>,
    /// Records that have it.
    present: usize,
}

/// A link from a record of one kind to records of another.
struct Link {
    /// The property's name.
    name: String,
    via: Via,
    target: usize,
}

#[derive(PartialEq, Eq)]
enum Via {
    /// `?n xyz:KEY ?m`, or through a list under the key.
    Key { iri: String, array: bool },
    /// `?n rdf:_1 ?m` (XML child elements).
    Slot,
}

struct Kind {
    name: String,
    how: How,
    members: Vec<Term>,
    columns: Vec<Column>,
    links: Vec<Link>,
    /// Literal values in the record's slots (an XML element's text): `rdf:value`.
    text: bool,
    many_texts: bool,
    /// The column whose value names a record, if any.
    key: Option<usize>,
    /// Its one record is the document (the root element of XML).
    document: bool,
}

/// A kind as the import's answer describes it, so the page can offer its key columns.
pub(crate) struct KindInfo {
    pub name: String,
    pub class: String,
    pub records: &'static str,
    pub sampled: usize,
    pub columns: Vec<String>,
    pub key: Option<String>,
}

impl KindInfo {
    pub(crate) fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "name": self.name,
            "class": self.class,
            "records": self.records,
            "sampled": self.sampled,
            "columns": self.columns,
            "key": self.key,
        })
    }
}

pub(crate) struct Draft {
    pub query: String,
    pub kinds: Vec<KindInfo>,
}

/// `https://data.example.org/src/{stem}/`, the source namespace a file gets when none is given.
pub(crate) fn default_namespace(file: &str) -> String {
    format!("https://data.example.org/src/{}/", path_part(stem(file)))
}

/// The file's name without its extension.
pub(crate) fn stem(file: &str) -> &str {
    match file.rfind('.') {
        Some(i) if i > 0 => &file[..i],
        _ => file,
    }
}

/// The drafted CONSTRUCT for `triples` of the file `file`, read through `service`, with IRIs in
/// `namespace`. `keys` names the key column of kinds by their name (`None`: no key column,
/// absent: found).
pub(crate) fn draft(
    file: &str,
    service: &str,
    namespace: &str,
    keys: &HashMap<String, Option<String>>,
    triples: &[(Term, Term, Term)],
) -> Draft {
    let mut namespace = namespace.to_owned();
    if !namespace.ends_with(['/', '#']) {
        namespace.push('/');
    }
    let mut kinds = kinds_of(file, triples);
    for kind in &mut kinds {
        kind.key = match keys.get(&kind.name) {
            Some(Some(column)) => kind
                .columns
                .iter()
                .position(|c| !c.many && &c.name == column),
            Some(None) => None,
            None => found_key(kind),
        };
    }
    let query = write_query(file, service, &namespace, &kinds);
    let kinds = kinds
        .iter()
        .map(|k| KindInfo {
            name: k.name.clone(),
            class: local_iri(&namespace, &pascal(&k.name)),
            records: match k.how {
                _ if k.document => "document",
                How::Element(_) => "elements",
                How::Field { .. } => "objects",
                How::Row => "rows",
                How::Root => "document",
            },
            sampled: k.members.len(),
            columns: k
                .columns
                .iter()
                .filter(|c| !c.many)
                .map(|c| c.name.clone())
                .collect(),
            key: k.key.map(|i| k.columns[i].name.clone()),
        })
        .collect();
    Draft { query, kinds }
}

fn is_iri(t: &Term, iri: &str) -> bool {
    matches!(t, Term::NamedNode(n) if n.as_str() == iri)
}

fn slot(p: &Term) -> bool {
    matches!(p, Term::NamedNode(n) if n.as_str().strip_prefix(RDF).is_some_and(|r| r.starts_with('_')))
}

fn xyz_key(p: &Term) -> Option<&str> {
    match p {
        Term::NamedNode(n) => n.as_str().strip_prefix(XYZ).map(|_| n.as_str()),
        _ => None,
    }
}

fn decoded(iri: &str) -> String {
    let local = iri.strip_prefix(XYZ).unwrap_or(iri);
    crate::ogc::percent_decode(local).unwrap_or_else(|| local.to_owned())
}

/// Finds the kinds of records in the sample, the document's first.
fn kinds_of(file: &str, triples: &[(Term, Term, Term)]) -> Vec<Kind> {
    let mut out_edges: HashMap<&Term, Vec<(&Term, &Term)>> = HashMap::new();
    let mut in_edges: HashMap<&Term, Vec<(&Term, &Term)>> = HashMap::new();
    let mut order: Vec<&Term> = Vec::new();
    for (s, p, o) in triples {
        let e = out_edges.entry(s).or_default();
        if e.is_empty() {
            order.push(s);
        }
        e.push((p, o));
        if !matches!(o, Term::Literal(_)) {
            in_edges.entry(o).or_default().push((s, p));
        }
    }
    let rdf_type = format!("{RDF}type");
    let root = triples
        .iter()
        .find(|(_, p, o)| is_iri(p, &rdf_type) && is_iri(o, &format!("{FX}Root")))
        .map(|(s, _, _)| s);
    let element = |n: &Term| -> Option<String> {
        out_edges.get(n)?.iter().find_map(|(p, o)| match o {
            Term::NamedNode(t) if is_iri(p, &rdf_type) && t.as_str().starts_with(XYZ) => {
                Some(t.as_str().to_owned())
            }
            _ => None,
        })
    };
    // A list: no type, no keys, only slots (a JSON array). Not a record.
    let is_list = |n: &Term| -> bool {
        Some(n) != root
            && out_edges
                .get(n)
                .is_some_and(|e| e.iter().all(|(p, _)| slot(p)))
    };
    let has_keys = |n: &Term| -> bool {
        out_edges
            .get(n)
            .is_some_and(|e| e.iter().any(|(p, _)| xyz_key(p).is_some()))
    };
    let how_of = |n: &Term| -> Option<How> {
        if let Some(t) = element(n) {
            return Some(How::Element(t));
        }
        if Some(n) == root {
            return has_keys(n).then_some(How::Root);
        }
        if is_list(n) {
            return None;
        }
        let (parent, p) = *in_edges.get(n)?.first()?;
        if let Some(key) = xyz_key(p) {
            return Some(How::Field {
                key: key.to_owned(),
                array: false,
            });
        }
        if slot(p) {
            if Some(parent) == root {
                return Some(How::Row);
            }
            if is_list(parent) {
                let (_, gp) = *in_edges.get(parent)?.first()?;
                return xyz_key(gp).map(|key| How::Field {
                    key: key.to_owned(),
                    array: true,
                });
            }
        }
        None
    };
    let mut kinds: Vec<Kind> = Vec::new();
    let mut kind_of: HashMap<&Term, usize> = HashMap::new();
    let mut by_how: HashMap<How, usize> = HashMap::new();
    for n in order.iter().copied() {
        if matches!(n, Term::Literal(_)) {
            continue;
        }
        let Some(how) = how_of(n) else { continue };
        let index = match by_how.get(&how) {
            Some(&i) => i,
            None => {
                if kinds.len() >= MAX_KINDS {
                    continue;
                }
                let base = match &how {
                    How::Element(t) => decoded(t),
                    How::Field { key, .. } => decoded(key),
                    How::Row | How::Root => stem(file).to_owned(),
                };
                let mut name = if base.trim().is_empty() {
                    "record".to_owned()
                } else {
                    base
                };
                while kinds.iter().any(|k| k.name == name) {
                    name.push('_');
                }
                kinds.push(Kind {
                    name,
                    how: how.clone(),
                    members: Vec::new(),
                    columns: Vec::new(),
                    links: Vec::new(),
                    text: false,
                    many_texts: false,
                    key: None,
                    document: false,
                });
                by_how.insert(how, kinds.len() - 1);
                kinds.len() - 1
            }
        };
        kinds[index].members.push(n.clone());
        kinds[index].document = kinds[index].members.len() == 1 && Some(n) == root;
        kind_of.insert(n, index);
    }
    // Columns, links and text, from each record's edges.
    for k in 0..kinds.len() {
        let members = kinds[k].members.clone();
        for n in &members {
            let mut counts: HashMap<String, usize> = HashMap::new();
            let mut link_counts: HashMap<(String, usize), usize> = HashMap::new();
            let mut texts = 0;
            for &(p, o) in out_edges.get(n).map(Vec::as_slice).unwrap_or_default() {
                if let Some(key) = xyz_key(p) {
                    match o {
                        Term::Literal(l) => {
                            add_value(&mut kinds[k], key, false, l);
                            *counts.entry(key.to_owned()).or_default() += 1;
                        }
                        _ if is_list(o) => {
                            for &(_, item) in
                                out_edges.get(o).map(Vec::as_slice).unwrap_or_default()
                            {
                                match item {
                                    Term::Literal(l) => {
                                        add_value(&mut kinds[k], key, true, l);
                                    }
                                    _ => {
                                        if let Some(&t) = kind_of.get(item) {
                                            add_link(
                                                &mut kinds[k],
                                                decoded(key),
                                                Via::Key {
                                                    iri: key.to_owned(),
                                                    array: true,
                                                },
                                                t,
                                            );
                                            *link_counts.entry((key.to_owned(), t)).or_default() +=
                                                2;
                                        }
                                    }
                                }
                            }
                        }
                        _ => {
                            if let Some(&t) = kind_of.get(o) {
                                add_link(
                                    &mut kinds[k],
                                    decoded(key),
                                    Via::Key {
                                        iri: key.to_owned(),
                                        array: false,
                                    },
                                    t,
                                );
                                *link_counts.entry((key.to_owned(), t)).or_default() += 1;
                            }
                        }
                    }
                } else if slot(p) {
                    match o {
                        Term::Literal(_) => texts += 1,
                        _ => {
                            if let Some(&t) = kind_of.get(o) {
                                let name = kinds[t].name.clone();
                                add_link(&mut kinds[k], name, Via::Slot, t);
                            }
                        }
                    }
                }
            }
            for (key, c) in counts {
                if let Some(col) = kinds[k]
                    .columns
                    .iter_mut()
                    .find(|c| c.iri == key && !c.array)
                {
                    col.present += 1;
                    col.many |= c > 1;
                }
            }
            if texts > 0 {
                kinds[k].text = true;
                kinds[k].many_texts |= texts > 1;
            }
        }
    }
    kinds
}

fn add_value(kind: &mut Kind, key: &str, array: bool, l: &Literal) {
    let plain = l.language().is_none() && l.datatype().as_str() == format!("{XSD}string");
    let col = match kind
        .columns
        .iter()
        .position(|c| c.iri == key && c.array == array)
    {
        Some(i) => &mut kind.columns[i],
        None => {
            if kind.columns.len() >= MAX_KEYS {
                return;
            }
            kind.columns.push(Column {
                iri: key.to_owned(),
                name: decoded(key),
                array,
                many: array,
                texts: Some(Vec::new()),
                present: 0,
            });
            kind.columns.last_mut().expect("pushed")
        }
    };
    if !plain {
        col.texts = None;
    } else if let Some(t) = &mut col.texts {
        t.push(l.value().to_owned());
    }
}

fn add_link(kind: &mut Kind, name: String, via: Via, target: usize) {
    if !kind
        .links
        .iter()
        .any(|l| l.via == via && l.target == target)
    {
        kind.links.push(Link { name, via, target });
    }
}

/// A column that names each record: on every record sampled, a value of its own each, and
/// named as an id is (`id`, `code`, `key`, `…_id`, `…Code`); `id` itself first.
fn found_key(kind: &Kind) -> Option<usize> {
    if kind.members.len() < 2 || matches!(kind.how, How::Root) {
        return None;
    }
    let unique = |c: &Column| -> bool {
        let Some(texts) = &c.texts else {
            return false;
        };
        let set: HashSet<&String> = texts.iter().collect();
        set.len() == texts.len() && !texts.iter().any(String::is_empty)
    };
    let score = |name: &str| -> Option<u8> {
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "id" | "@id" => Some(0),
            "code" | "key" | "identifier" | "uri" | "iri" => Some(1),
            _ if lower.ends_with("id") || lower.ends_with("code") || lower.ends_with("key") => {
                Some(2)
            }
            _ => None,
        }
    };
    kind.columns
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.many && c.present == kind.members.len() && unique(c))
        .filter_map(|(i, c)| score(&c.name).map(|s| (s, i)))
        .min()
        .map(|(_, i)| i)
}

/// The datatype the values look like, when the file gives them all as text.
fn sniffed(texts: &[String]) -> Option<&'static str> {
    let values: Vec<&str> = texts
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect();
    if values.is_empty() {
        return None;
    }
    let all = |f: &dyn Fn(&str) -> bool| values.iter().all(|v| f(v));
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let integer = |s: &str| {
        let s = s.strip_prefix(['+', '-']).unwrap_or(s);
        // "007" is a code, not a number.
        digits(s) && (s.len() == 1 || !s.starts_with('0'))
    };
    let decimal = |s: &str| {
        let body = s.strip_prefix(['+', '-']).unwrap_or(s);
        match body.split_once('.') {
            Some((a, b)) => (a.is_empty() || integer(a) || a == "0") && digits(b),
            None => integer(s),
        }
    };
    let double = |s: &str| {
        let lower = s.to_ascii_lowercase();
        match lower.split_once('e') {
            Some((m, e)) => decimal(m) && integer(e.strip_prefix('+').unwrap_or(e)),
            None => decimal(s),
        }
    };
    let date = |s: &str| {
        let b = s.as_bytes();
        b.len() == 10
            && b[4] == b'-'
            && b[7] == b'-'
            && digits(&s[..4])
            && digits(&s[5..7])
            && digits(&s[8..])
    };
    // By bytes: a value with a character over byte 10 is no date-time, and no panic.
    let date_time =
        |s: &str| s.len() >= 19 && s.as_bytes()[10] == b'T' && s.get(..10).is_some_and(date);
    if all(&integer) {
        Some("integer")
    } else if all(&decimal) {
        Some("decimal")
    } else if all(&double) {
        Some("double")
    } else if all(&|s| s == "true" || s == "false") {
        Some("boolean")
    } else if all(&date) {
        Some("date")
    } else if all(&date_time) {
        Some("dateTime")
    } else {
        None
    }
}

/// `StationName` for `station name`, `station_name` and `stationName`.
fn pascal(name: &str) -> String {
    let mut out = String::new();
    for part in name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|p| !p.is_empty())
    {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert_str(0, "Record");
    }
    out
}

/// A name as a path segment of IRIs: lower case letters, digits and `-`.
fn path_part(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_owned();
    if out.is_empty() {
        "data".to_owned()
    } else {
        out
    }
}

/// `src:name` when the name is a safe local name, else the full IRI with it percent-encoded.
fn term(namespace: &str, name: &str) -> String {
    let safe = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if safe {
        format!("src:{name}")
    } else {
        format!("<{}>", local_iri(namespace, name))
    }
}

/// A name as the text of a `#` comment: on one line, so that what a file names stays a comment.
fn comment(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn local_iri(namespace: &str, name: &str) -> String {
    let mut iri = namespace.to_owned();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            iri.push(b as char);
        } else {
            let _ = write!(iri, "%{b:02X}");
        }
    }
    iri
}

/// A SPARQL variable name made of `parts`, kept apart from those already `taken`.
fn var(taken: &mut HashSet<String>, parts: &[&str]) -> String {
    let mut name: String = parts
        .join("_")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
        name.insert(0, 'v');
    }
    while !taken.insert(name.clone()) {
        name.push('_');
    }
    name
}

fn sparql_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `xyz:name` for a Facade-X key whose name is a safe local name, else `<IRI>`.
fn key_term(iri: &str) -> String {
    match iri.strip_prefix(XYZ) {
        Some(local)
            if local.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && local
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') =>
        {
            format!("xyz:{local}")
        }
        _ => format!("<{iri}>"),
    }
}

const SLOT_FILTER: &str = "STRSTARTS(STR(?{s}), STR(rdf:_))";

/// The patterns that find a record of `kind` as `?{node}` (with `?{slot}` free for its slot).
fn find(kind: &Kind, node: &str, aux: &str, out: &mut String) {
    let slot_filter = |s: &str| SLOT_FILTER.replace("{s}", s);
    match &kind.how {
        How::Element(t) => {
            let _ = writeln!(out, "      ?{node} a {} .", key_term(t));
        }
        How::Field { key, array: false } => {
            let _ = writeln!(out, "      ?{aux}_parent {k} ?{node} .", k = key_term(key));
            let _ = writeln!(out, "      FILTER(isBlank(?{node}))");
        }
        How::Field { key, array: true } => {
            let _ = writeln!(
                out,
                "      ?{aux}_parent {k} ?{aux}_list . ?{aux}_list ?{aux}_slot ?{node} .",
                k = key_term(key)
            );
            let _ = writeln!(
                out,
                "      FILTER(isBlank(?{node}) && {})",
                slot_filter(&format!("{aux}_slot"))
            );
        }
        How::Row => {
            let _ = writeln!(out, "      ?{aux}_root a fx:Root ; ?{aux}_slot ?{node} .");
            let _ = writeln!(
                out,
                "      FILTER(isBlank(?{node}) && {})",
                slot_filter(&format!("{aux}_slot"))
            );
        }
        How::Root => {
            let _ = writeln!(out, "      ?{node} a fx:Root .");
        }
    }
}

/// The BIND that names a record `?{node}` of `kind` as `?{subject}`.
fn name(kind: &Kind, node: &str, aux: &str, subject: &str, out: &mut String) {
    let path = sparql_string(&format!("{}/", path_part(&kind.name)));
    match (kind.key, &kind.how) {
        (Some(i), _) => {
            let _ = writeln!(
                out,
                "      OPTIONAL {{ ?{node} {} ?{aux}_key }}",
                key_term(&kind.columns[i].iri)
            );
            let _ = writeln!(
                out,
                "      BIND(COALESCE(fx:entity(STR(src:), {path}, ?{aux}_key), ?{node}) AS ?{subject})"
            );
        }
        (None, How::Row) => {
            let _ = writeln!(
                out,
                "      BIND(fx:entity(STR(src:), {path}, STRAFTER(STR(?{aux}_slot), STR(rdf:_))) AS ?{subject})"
            );
        }
        (None, How::Root) | (None, How::Element(_)) if kind.document || kind.how == How::Root => {
            let _ = writeln!(
                out,
                "      BIND(IRI(CONCAT(STR(src:), {})) AS ?{subject})",
                sparql_string(&path_part(&kind.name))
            );
        }
        (None, _) => {
            let _ = writeln!(out, "      BIND(?{node} AS ?{subject})");
        }
    }
}

/// The value of a column, typed, as `?{value}` from `?{node}`.
fn value(column: &Column, node: &str, raw: &str, value: &str, out: &mut String) {
    let reach = if column.array {
        format!(
            "?{node} {} ?{raw}_list . ?{raw}_list ?{raw}_slot ?{raw} . FILTER(isLiteral(?{raw}))",
            key_term(&column.iri)
        )
    } else {
        format!("?{node} {} ?{raw} .", key_term(&column.iri))
    };
    let cast = column.texts.as_deref().and_then(sniffed);
    match cast {
        Some(t) => {
            let _ = write!(
                out,
                "{reach} FILTER(STR(?{raw}) != \"\") BIND(xsd:{t}(STR(?{raw})) AS ?{value})"
            );
        }
        None if column.texts.is_some() => {
            let _ = write!(
                out,
                "{reach} FILTER(STR(?{raw}) != \"\") BIND(?{raw} AS ?{value})"
            );
        }
        None => {
            let _ = write!(out, "{reach} BIND(?{raw} AS ?{value})");
        }
    }
}

fn write_query(file: &str, service: &str, namespace: &str, kinds: &[Kind]) -> String {
    let mut taken: HashSet<String> = HashSet::new();
    let source = local_iri(namespace, file);
    let mut template = String::new();
    let mut branches: Vec<String> = Vec::new();
    let subjects: Vec<String> = kinds
        .iter()
        .map(|k| var(&mut taken, &[&path_part(&k.name).replace('-', "_")]))
        .collect();
    for (k, kind) in kinds.iter().enumerate() {
        if kind.columns.is_empty() && kind.links.is_empty() && !kind.text {
            continue;
        }
        let subject = &subjects[k];
        let node = var(&mut taken, &["_", subject]);
        let aux = node.clone();
        let _ = write!(
            template,
            "  ?{subject} a {} ;\n    prov:wasDerivedFrom <{source}>",
            term(namespace, &pascal(&kind.name))
        );
        // The main branch: the record and its single values.
        let mut main = String::new();
        let _ = writeln!(
            main,
            "      # {} ({})",
            comment(&kind.name),
            match kind.how {
                How::Element(_) => "elements",
                How::Field { .. } => "objects under a key",
                How::Row => "rows",
                How::Root => "the document",
            }
        );
        find(kind, &node, &aux, &mut main);
        name(kind, &node, &aux, subject, &mut main);
        let mut extra: Vec<String> = Vec::new();
        for column in &kind.columns {
            let v = var(&mut taken, &[subject, &column.name]);
            let _ = write!(template, " ;\n    {} ?{v}", term(namespace, &column.name));
            let raw = var(&mut taken, &["_", &v]);
            let mut pattern = String::new();
            value(column, &node, &raw, &v, &mut pattern);
            if column.many {
                let mut branch = String::new();
                find(kind, &node, &aux, &mut branch);
                name(kind, &node, &aux, subject, &mut branch);
                let _ = writeln!(branch, "      {pattern}");
                extra.push(branch);
            } else {
                let _ = writeln!(main, "      OPTIONAL {{ {pattern} }}");
            }
        }
        if kind.text {
            let v = var(&mut taken, &[subject, "text"]);
            let _ = write!(template, " ;\n    rdf:value ?{v}");
            let raw = var(&mut taken, &["_", &v]);
            let pattern = format!(
                "?{node} ?{raw}_slot ?{raw} . FILTER(isLiteral(?{raw}) && STR(?{raw}) != \"\" && {}) BIND(?{raw} AS ?{v})",
                SLOT_FILTER.replace("{s}", &format!("{raw}_slot"))
            );
            if kind.many_texts {
                let mut branch = String::new();
                find(kind, &node, &aux, &mut branch);
                name(kind, &node, &aux, subject, &mut branch);
                let _ = writeln!(branch, "      {pattern}");
                extra.push(branch);
            } else {
                let _ = writeln!(main, "      OPTIONAL {{ {pattern} }}");
            }
        }
        for link in &kind.links {
            let target = &kinds[link.target];
            let v = var(&mut taken, &[subject, &link.name]);
            let _ = write!(template, " ;\n    {} ?{v}", term(namespace, &link.name));
            let m = var(&mut taken, &["_", &v]);
            let mut branch = String::new();
            find(kind, &node, &aux, &mut branch);
            name(kind, &node, &aux, subject, &mut branch);
            match &link.via {
                Via::Key { iri, array: false } => {
                    let _ = writeln!(branch, "      ?{node} {k} ?{m} .", k = key_term(iri));
                }
                Via::Key { iri, array: true } => {
                    let _ = writeln!(
                        branch,
                        "      ?{node} {k} ?{m}_list . ?{m}_list ?{m}_slot ?{m} . FILTER({})",
                        SLOT_FILTER.replace("{s}", &format!("{m}_slot")),
                        k = key_term(iri),
                    );
                }
                Via::Slot => {
                    let _ = writeln!(
                        branch,
                        "      ?{node} ?{m}_slot ?{m} . FILTER({})",
                        SLOT_FILTER.replace("{s}", &format!("{m}_slot"))
                    );
                    if let How::Element(t) = &target.how {
                        let _ = writeln!(branch, "      ?{m} a {} .", key_term(t));
                    }
                }
            }
            let _ = writeln!(branch, "      FILTER(isBlank(?{m}))");
            name(target, &m, &m, &v, &mut branch);
            extra.push(branch);
        }
        template.push_str(" .\n");
        branches.push(main);
        branches.extend(extra);
    }
    let mut q = String::new();
    let _ = write!(
        q,
        "PREFIX fx: <{FX}>\n\
         PREFIX xyz: <{XYZ}>\n\
         PREFIX rdf: <{RDF}>\n\
         PREFIX xsd: <{XSD}>\n\
         PREFIX prov: <http://www.w3.org/ns/prov#>\n\
         PREFIX src: <{namespace}>\n\n\
         # {} in its own vocabulary: a class per kind of record, a property per key.\n\
         CONSTRUCT {{\n{template}}} WHERE {{\n  SERVICE <{service}> {{\n",
        comment(file)
    );
    if branches.is_empty() {
        q.push_str("    ?s ?p ?o FILTER(false)\n");
    }
    for (i, b) in branches.iter().enumerate() {
        q.push_str(if i == 0 { "    {\n" } else { "    UNION {\n" });
        q.push_str(b);
        q.push_str("    }\n");
    }
    q.push_str("  }\n}\n");
    q
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_typed_from_what_they_look_like() {
        let t = |v: &[&str]| sniffed(&v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        assert_eq!(t(&["1", "-20", ""]), Some("integer"));
        assert_eq!(t(&["1", "2.5", ".5"]), Some("decimal"));
        assert_eq!(t(&["1e3", "2.5"]), Some("double"));
        assert_eq!(t(&["007", "12"]), None);
        assert_eq!(t(&["true", "false"]), Some("boolean"));
        assert_eq!(t(&["1835-05-05"]), Some("date"));
        assert_eq!(t(&["1835-05-05T10:00:00Z"]), Some("dateTime"));
        assert_eq!(t(&["", " "]), None);
        assert_eq!(t(&["BE1", "2"]), None);
    }

    #[test]
    fn names_become_classes_paths_and_terms() {
        assert_eq!(pascal("stations"), "Stations");
        assert_eq!(pascal("station name"), "StationName");
        assert_eq!(pascal("2024"), "Record2024");
        assert_eq!(path_part("Speed (km/h)"), "speed-km-h");
        assert_eq!(term("https://x/", "name"), "src:name");
        assert_eq!(
            term("https://x/", "speed (km/h)"),
            "<https://x/speed%20%28km%2Fh%29>"
        );
        assert_eq!(
            default_namespace("Stations 2024.csv"),
            "https://data.example.org/src/stations-2024/"
        );
    }
}
