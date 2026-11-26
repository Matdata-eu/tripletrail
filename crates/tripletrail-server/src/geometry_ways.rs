//! The geometry of a node shape's instances as the readers read it (OGC API Features, the map's
//! tiles, the REST API and search): the `tts:geometry` ways with an `sh:path` in their order,
//! each for the instances with no value on the ways before it (#259), then, for the instances
//! with no value on any of them, the first way with `tts:lineBetween` (#148).
//!
//! `tts:lineBetween ( A B )` draws a straight line from the geometry at the end of path `A`
//! to the one at the end of `B`, such as a section of line between its two operational
//! points. In SPARQL it is `st:ST_MakeLine` of the two.
//!
//! A line way has no spatial index, so a read with a window (a map tile, `bbox`, a search
//! area) first narrows it: [`Ways::near`] keeps the instances whose line's box (the box of its
//! two ends) meets the window. The boxes are read once per store generation, caller's view
//! and way, and kept.

use crate::View;
use crate::admin::Entry;
use crate::ogc::{rdf_list, sparql_path, subject};
use geo::{BoundingRect, Rect};
use oxigraph::model::{Graph, Literal, NamedNode, NamedOrBlankNodeRef, Term, TermRef};
use oxigraph::sparql::QueryResults;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tripletrail::geo::{Crs, parse_wkt_literal_uncached, transform};

const TTS: &str = tripletrail::shapes::TTS;
const SH: &str = "http://www.w3.org/ns/shacl#";
const MAKE_LINE: &str = "http://w3id.org/tripletrail/st#ST_MakeLine";
const WKT: &str = "http://www.opengis.net/ont/geosparql#wktLiteral";
/// The most instances a later path way is narrowed to ([`Ways::narrowed`]).
const MAX_LEFT: usize = 1000;
/// The most instances a window keeps by their lines' boxes; past it the way is read whole.
const MAX_NEAR: usize = 10_000;
/// The longest the boxes of a way's lines may take to read.
const BOXES_TIMEOUT: Duration = Duration::from_secs(60);

/// One way to a geometry.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Way {
    /// A SPARQL property path to the geometry literal.
    Path(String),
    /// A later path way for the few instances the ways before it leave without a geometry
    /// (SPARQL terms), as [`Ways::narrowed`] found them.
    Few(String, Vec<String>),
    /// The line between the geometries at the end of two property paths, for the instances
    /// `only` names (a `VALUES` clause) when a window narrowed it.
    Line(String, String, Option<String>),
}

/// The ways a node shape gives its instances' geometry, in the order the readers try them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Ways(Vec<Way>);

impl Ways {
    /// The ways of `node`: `None` when it has none the readers can read.
    pub(crate) fn of(shapes: &Graph, node: NamedOrBlankNodeRef<'_>) -> Option<Self> {
        let iri = |ns: &str, local: &str| NamedNode::new_unchecked(format!("{ns}{local}"));
        let order = |s: NamedOrBlankNodeRef<'_>| -> i64 {
            match shapes.object_for_subject_predicate(s, &iri(SH, "order")) {
                Some(TermRef::Literal(l)) => l.value().trim().parse().unwrap_or(i64::MAX),
                _ => i64::MAX,
            }
        };
        let mut nodes: Vec<_> = shapes
            .objects_for_subject_predicate(node, &iri(TTS, "geometry"))
            .filter_map(subject)
            .collect();
        nodes.sort_by_key(|w| order(*w));
        // Every way with a path, in order: each for the instances without a value on the
        // ways before it (#259, a feature whose geometry comes from linked ones, round after
        // round).
        let paths = nodes.iter().filter_map(|w| {
            let p = shapes.object_for_subject_predicate(*w, &iri(SH, "path"))?;
            sparql_path(shapes, p).map(Way::Path)
        });
        let line = nodes.iter().find_map(|w| {
            let list = shapes.object_for_subject_predicate(*w, &iri(TTS, "lineBetween"))?;
            match rdf_list(shapes, list)?.as_slice() {
                [a, b] => Some(Way::Line(
                    sparql_path(shapes, *a)?,
                    sparql_path(shapes, *b)?,
                    None,
                )),
                _ => None,
            }
        });
        let ways: Vec<Way> = paths.chain(line).collect();
        (!ways.is_empty()).then_some(Ways(ways))
    }

    /// The path of the first way, when it is a path: the property the map draws.
    pub(crate) fn path(&self) -> Option<&str> {
        match self.0.first() {
            Some(Way::Path(p)) => Some(p),
            _ => None,
        }
    }

    /// Whether a path way follows the first ([`Ways::narrowed`] has work).
    pub(crate) fn has_later_paths(&self) -> bool {
        self.0.iter().skip(1).any(|w| matches!(w, Way::Path(_)))
    }

    /// These ways with each path way after the first narrowed to the instances of `class`
    /// that the ways before it leave without a geometry (#259), found once per collection
    /// (the collections are made again after a write): a way none of them has a value on
    /// goes, and one for at most [`MAX_LEFT`] of them reads those alone. Without that a tile
    /// would look for every instance on every later way, to keep those with a geometry on an
    /// earlier one out (RINF's running tracks, of which a handful take their geometry two links
    /// away: 34 s for a tile of a filter on their speed). A later way of more instances, or one
    /// whose instances could not be read in `timeout`, stays as it is.
    pub(crate) fn narrowed(
        &self,
        entry: &Entry,
        view: &View,
        class: &str,
        timeout: Option<Duration>,
    ) -> Ways {
        let mut ways: Vec<Way> = Vec::with_capacity(self.0.len());
        for (i, way) in self.0.iter().enumerate() {
            let Way::Path(p) = way else {
                ways.push(way.clone());
                continue;
            };
            if i == 0 {
                ways.push(way.clone());
                continue;
            }
            let mut query = format!("SELECT DISTINCT ?f WHERE {{ ?f a <{class}> . ?f {p} ?g .");
            for (j, earlier) in ways.iter().enumerate() {
                query.push_str(&format!(
                    " FILTER NOT EXISTS {{ {} }}",
                    earlier.bind("?f", &format!("?x{j}"), j)
                ));
            }
            query.push_str(&format!(" }} LIMIT {}", MAX_LEFT + 1));
            let Ok(rows) = crate::ogc::run(entry, &query, timeout, view) else {
                ways.push(way.clone());
                continue;
            };
            let left: Vec<String> = rows
                .iter()
                .filter_map(|r| match r.first() {
                    Some(Some(Term::NamedNode(n))) => Some(n.to_string()),
                    _ => None,
                })
                .collect();
            if left.len() > MAX_LEFT || left.len() < rows.len() {
                // Too many, or blank nodes a VALUES clause cannot name.
                ways.push(way.clone());
            } else if !left.is_empty() {
                ways.push(Way::Few(p.clone(), left));
            }
        }
        Ways(ways)
    }

    /// These ways for a read within `window` (WKT in CRS84) by `s`, a variable: the line way
    /// keeps the instances whose line's box meets the window, or goes when none does. It is
    /// read whole when the boxes are unknown (another store, a failed read) or too many meet
    /// the window.
    pub(crate) fn near(&self, entry: &Entry, view: &View, s: &str, window: &str) -> Ways {
        let Some(area) = parse_wkt_literal_uncached(
            &Literal::new_typed_literal(window, NamedNode::new_unchecked(WKT)).into(),
        )
        .and_then(|g| transform(&g.geometry, &g.crs, &Crs::Crs84)) else {
            return self.clone();
        };
        // The window's boxes (two across the antimeridian), compared box to box: cheap for
        // many lines, and a box too many only costs the exact test in SPARQL.
        let area: Vec<Rect<f64>> = match &area {
            geo::Geometry::MultiPolygon(m) => {
                m.0.iter().filter_map(|p| p.bounding_rect()).collect()
            }
            g => g.bounding_rect().into_iter().collect(),
        };
        let meets = |r: &Rect<f64>| {
            area.iter().any(|w| {
                r.min().x <= w.max().x
                    && w.min().x <= r.max().x
                    && r.min().y <= w.max().y
                    && w.min().y <= r.max().y
            })
        };
        let mut ways = Vec::new();
        for (i, way) in self.0.iter().enumerate() {
            let Way::Line(a, b, _) = way else {
                ways.push(way.clone());
                continue;
            };
            let Some(boxes) = boxes(entry, view, &self.0[..i], a, b) else {
                ways.push(way.clone());
                continue;
            };
            let mut near: Vec<&NamedNode> = boxes
                .iter()
                .filter(|(_, r)| meets(r))
                .map(|(f, _)| f)
                .collect();
            near.dedup();
            if near.is_empty() {
                continue;
            }
            if near.len() > MAX_NEAR || !s.starts_with('?') {
                ways.push(way.clone());
                continue;
            }
            let values: Vec<String> = near.iter().map(|f| f.to_string()).collect();
            let only = format!("VALUES {s} {{ {} }}", values.join(" "));
            ways.push(Way::Line(a.clone(), b.clone(), Some(only)));
        }
        Ways(ways)
    }

    /// These ways for the instances `values` (SPARQL terms) only, bound to `s`: a line way
    /// makes the lines of those alone (a `VALUES` outside it would not reach into it).
    pub(crate) fn only(&self, s: &str, values: &[String]) -> Ways {
        let only = format!("VALUES {s} {{ {} }}", values.join(" "));
        Ways(
            self.0
                .iter()
                .map(|w| match w {
                    Way::Line(a, b, None) => Way::Line(a.clone(), b.clone(), Some(only.clone())),
                    w => w.clone(),
                })
                .collect(),
        )
    }

    /// A graph pattern that binds `o` to each geometry of `s` (a variable or an IRI, as
    /// SPARQL text).
    pub(crate) fn pattern(&self, s: &str, o: &str) -> String {
        self.filtered(s, o, "")
    }

    /// [`Ways::pattern`] with `filter` (SPARQL on `o`, such as a `FILTER`) next to each way, so
    /// that a spatial filter stays beside the path it can use an index for.
    pub(crate) fn filtered(&self, s: &str, o: &str, filter: &str) -> String {
        let filter = if filter.is_empty() {
            String::new()
        } else {
            format!(" {filter}")
        };
        if let [Way::Path(p)] = self.0.as_slice() {
            return path_filtered(s, p, o, &filter);
        }
        if self.0.is_empty() {
            // A window none of the lines meets.
            return "{ FILTER(false) }".to_owned();
        }
        let mut branches = Vec::new();
        for (i, way) in self.0.iter().enumerate() {
            let alternatives = match way {
                Way::Path(p) if !filter.is_empty() => alternatives(p),
                _ => Vec::new(),
            };
            let (mut branch, filter) = if alternatives.len() > 1 {
                let Way::Path(p) = way else { unreachable!() };
                (path_filtered(s, p, o, &filter), String::new())
            } else {
                (way.bind(s, o, i), filter.clone())
            };
            // The instances with a geometry on an earlier way take that one.
            for (j, earlier) in self.0[..i].iter().enumerate() {
                let x = format!("{o}_{i}_{j}");
                branch.push_str(&format!(
                    " FILTER NOT EXISTS {{ {} }}",
                    earlier.bind(s, &x, j)
                ));
            }
            branches.push(format!("{{ {branch}{filter} }}"));
        }
        branches.join(" UNION ")
    }
}

/// `s p o` with `filter` beside it. A path of alternatives (`A|B|C`) becomes a `UNION` of
/// them, each with the filter: a spatial filter then uses the index for each, where beside the
/// alternatives as a whole it was tested on every instance's geometry (1.3 s for a map tile of
/// RINF Germany's tracks, 4 ms as a union, #947). The answers are the same, as a path of
/// alternatives is the union of its alternatives.
fn path_filtered(s: &str, p: &str, o: &str, filter: &str) -> String {
    let alternatives = if filter.is_empty() {
        Vec::new()
    } else {
        alternatives(p)
    };
    if alternatives.len() < 2 {
        return format!("{s} {p} {o} .{filter}");
    }
    alternatives
        .iter()
        .map(|a| format!("{{ {s} {a} {o} .{filter} }}"))
        .collect::<Vec<_>>()
        .join(" UNION ")
}

/// The alternatives of a SPARQL property path at its top level (`(A|B)/C` has one), outer
/// parentheses aside; IRIs are skipped whole, as they may hold parentheses.
fn alternatives(path: &str) -> Vec<String> {
    let mut path = path.trim();
    // Outer parentheses around the whole path.
    while path.starts_with('(') && path.ends_with(')') && closes_at_end(path) {
        path = path[1..path.len() - 1].trim();
    }
    let mut out = Vec::new();
    let (mut depth, mut start, mut iri) = (0i32, 0, false);
    for (i, c) in path.char_indices() {
        match c {
            '<' if !iri => iri = true,
            '>' if iri => iri = false,
            '(' if !iri => depth += 1,
            ')' if !iri => depth -= 1,
            '|' if !iri && depth == 0 => {
                out.push(path[start..i].trim().to_owned());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(path[start..].trim().to_owned());
    out
}

/// Whether the parenthesis opening `path` closes at its end.
fn closes_at_end(path: &str) -> bool {
    let (mut depth, mut iri) = (0i32, false);
    for (i, c) in path.char_indices() {
        match c {
            '<' if !iri => iri = true,
            '>' if iri => iri = false,
            '(' if !iri => depth += 1,
            ')' if !iri => {
                depth -= 1;
                if depth == 0 {
                    return i == path.len() - 1;
                }
            }
            _ => {}
        }
    }
    false
}

impl Way {
    /// The way as a pattern binding `o`; `i` keeps its own variables apart.
    fn bind(&self, s: &str, o: &str, i: usize) -> String {
        match self {
            Way::Path(p) => format!("{s} {p} {o} ."),
            Way::Few(p, only) if s.starts_with('?') => {
                format!("VALUES {s} {{ {} }} {s} {p} {o} .", only.join(" "))
            }
            Way::Few(p, only) if only.iter().any(|t| t == s) => format!("{s} {p} {o} ."),
            Way::Few(..) => "FILTER(false)".to_owned(),
            Way::Line(a, b, only) => {
                let (from, to) = (format!("{o}_{i}a"), format!("{o}_{i}b"));
                let only = only.as_deref().map(|v| format!("{v} ")).unwrap_or_default();
                format!(
                    "{only}{s} {a} {from} . {s} {b} {to} . BIND(<{MAKE_LINE}>({from}, {to}) AS {o})"
                )
            }
        }
    }
}

/// The instances of a line way, each with the box of its line (CRS84), sorted by instance.
type Boxes = Arc<Vec<(NamedNode, Rect<f64>)>>;
/// Dataset, caller's view, store generation and the way (its ends and the ways before it).
type BoxKey = (u64, Option<u64>, u64, String);

enum Slot {
    /// Being read in the background.
    Reading,
    /// Read (`None`: they could not be, or would take too much memory), with their size in
    /// bytes and when they were last used.
    Read(Option<Boxes>, usize, u64),
}

#[derive(Default)]
struct BoxCache {
    slots: HashMap<BoxKey, Slot>,
    /// The bytes of the boxes kept.
    bytes: usize,
    /// A clock for the least recently used.
    clock: u64,
}

impl BoxCache {
    /// Takes out the slots of `key`'s dataset, view and way at other generations than
    /// `keep` (the slots being read stay: their reader puts them back).
    fn drop_others(&mut self, key: &BoxKey, keep: u64) {
        let same = |k: &BoxKey| k.0 == key.0 && k.1 == key.1 && k.3 == key.3 && k.2 != keep;
        let mut freed = 0;
        self.slots.retain(|k, slot| match slot {
            Slot::Read(_, bytes, _) if same(k) => {
                freed += *bytes;
                false
            }
            _ => true,
        });
        self.bytes -= freed;
    }
}

/// The boxes per key.
static BOXES: Mutex<Option<BoxCache>> = Mutex::new(None);
/// The most bytes of boxes kept, over all keys; one set of more than half of it is not kept.
const MAX_BOX_BYTES: usize = 128 << 20;
/// The most slots kept, read or empty.
const MAX_BOX_SLOTS: usize = 256;

/// The boxes of the lines from `a` to `b`, for the instances with no geometry on `before`, at
/// the store's generation. `None` while they are read (in the background, once per dataset,
/// view and way at a time, so no request waits for them, and a store written to often is
/// read again only when the last read is done), or when they cannot be.
fn boxes(entry: &Entry, view: &View, before: &[Way], a: &str, b: &str) -> Option<Boxes> {
    let tripletrail::store::Dataset::TripleTrail(store) = entry.engine.store() else {
        return None;
    };
    let generation = store.generation();
    let mut query = format!("SELECT ?f ?a ?b WHERE {{ ?f {a} ?a . ?f {b} ?b .");
    for (j, earlier) in before.iter().enumerate() {
        query.push_str(&format!(
            " FILTER NOT EXISTS {{ {} }}",
            earlier.bind("?f", &format!("?x{j}"), j)
        ));
    }
    query.push_str(" }");
    let key: BoxKey = (entry.id, view.fingerprint(), generation, query.clone());
    {
        let mut guard = BOXES.lock().unwrap_or_else(|e| e.into_inner());
        let cache = guard.get_or_insert_with(BoxCache::default);
        cache.clock += 1;
        let now = cache.clock;
        match cache.slots.get_mut(&key) {
            Some(Slot::Read(boxes, _, used)) => {
                *used = now;
                return boxes.clone();
            }
            Some(Slot::Reading) => return None,
            None => {}
        }
        let reading = cache.slots.iter().any(|(k, slot)| {
            matches!(slot, Slot::Reading) && k.0 == key.0 && k.1 == key.1 && k.3 == key.3
        });
        if reading {
            return None;
        }
        cache.slots.insert(key.clone(), Slot::Reading);
    }
    let (engine, view, failed) = (entry.engine.clone(), view.clone(), key.clone());
    let spawned = std::thread::Builder::new()
        .name("tripletrail-line-boxes".into())
        .spawn(move || {
            let boxes = read_boxes(&engine, &view, &query);
            let bytes = boxes.as_ref().map_or(0, |b| {
                b.iter()
                    .map(|(f, _)| f.as_str().len() + std::mem::size_of::<(NamedNode, Rect<f64>)>())
                    .sum()
            });
            let boxes = boxes.filter(|_| bytes <= MAX_BOX_BYTES / 2);
            let bytes = if boxes.is_some() { bytes } else { 0 };
            let mut guard = BOXES.lock().unwrap_or_else(|e| e.into_inner());
            let cache = guard.get_or_insert_with(BoxCache::default);
            cache.clock += 1;
            let now = cache.clock;
            cache.drop_others(&key, key.2);
            cache.slots.insert(key, Slot::Read(boxes, bytes, now));
            cache.bytes += bytes;
            // The least recently used go until the rest fits, in bytes and in slots (an empty
            // or failed read takes no bytes, but its key holds the way's query).
            while cache.bytes > MAX_BOX_BYTES || cache.slots.len() > MAX_BOX_SLOTS {
                let Some(oldest) = cache
                    .slots
                    .iter()
                    .filter_map(|(k, s)| match s {
                        Slot::Read(_, _, used) => Some((*used, k.clone())),
                        Slot::Reading => None,
                    })
                    .min()
                    .map(|(_, k)| k)
                else {
                    break;
                };
                if let Some(Slot::Read(_, b, _)) = cache.slots.remove(&oldest) {
                    cache.bytes -= b;
                }
            }
        });
    if spawned.is_err() {
        let mut guard = BOXES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cache) = guard.as_mut() {
            cache.slots.remove(&failed);
        }
    }
    None
}

fn read_boxes(engine: &tripletrail::Engine, view: &View, query: &str) -> Option<Boxes> {
    let (engine, _deadline) = crate::with_deadline(engine, Some(BOXES_TIMEOUT));
    let Ok(QueryResults::Solutions(rows)) = engine.query_with_options(query, view.options()) else {
        return None;
    };
    let rect = |t: Option<&Term>| -> Option<Rect<f64>> {
        let g = parse_wkt_literal_uncached(t?)?;
        let r = transform(&g.geometry, &g.crs, &Crs::Crs84)?.bounding_rect()?;
        [r.min().x, r.min().y, r.max().x, r.max().y]
            .iter()
            .all(|v| v.is_finite())
            .then_some(r)
    };
    let mut out = Vec::new();
    for row in rows {
        // A failed row (a timeout) leaves the boxes unknown rather than short.
        let row = row.ok()?;
        let Some(Term::NamedNode(f)) = row.get("f") else {
            continue;
        };
        let (Some(a), Some(b)) = (rect(row.get("a")), rect(row.get("b"))) else {
            continue;
        };
        let r = Rect::new(
            (a.min().x.min(b.min().x), a.min().y.min(b.min().y)),
            (a.max().x.max(b.max().x), a.max().y.max(b.max().y)),
        );
        out.push((f.clone(), r));
    }
    out.sort_by(|x, y| x.0.as_str().cmp(y.0.as_str()));
    Some(Arc::new(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_alone_is_a_triple_pattern_and_a_line_is_the_fallback() {
        let path = Ways(vec![Way::Path("<p>".into())]);
        assert_eq!(path.pattern("?f", "?w"), "?f <p> ?w .");
        assert_eq!(
            path.filtered("?f", "?w", "FILTER(?w)"),
            "?f <p> ?w . FILTER(?w)"
        );
        let both = Ways(vec![
            Way::Path("<p>".into()),
            Way::Line("<a>/<g>".into(), "<b>/<g>".into(), None),
        ]);
        assert_eq!(
            both.pattern("?f", "?w"),
            format!(
                "{{ ?f <p> ?w . }} UNION {{ ?f <a>/<g> ?w_1a . ?f <b>/<g> ?w_1b . BIND(<{MAKE_LINE}>(?w_1a, ?w_1b) AS ?w) FILTER NOT EXISTS {{ ?f <p> ?w_1_0 . }} }}"
            )
        );
        assert_eq!(both.path(), Some("<p>"));
        // For some instances only, the line way makes their lines alone.
        let some = both.only("?f", &["<x>".to_owned(), "<y>".to_owned()]);
        assert!(
            some.pattern("?f", "?w")
                .contains("{ VALUES ?f { <x> <y> } ?f <a>/<g> ?w_1a ."),
            "{}",
            some.pattern("?f", "?w")
        );
        assert!(some.pattern("?f", "?w").starts_with("{ ?f <p> ?w . }"));
        // No way left (a window no line meets): no row.
        assert_eq!(Ways(Vec::new()).pattern("?f", "?w"), "{ FILTER(false) }");
    }

    #[test]
    fn later_path_ways_are_for_the_instances_without_a_geometry_before() {
        let shapes: Graph = {
            let ttl = "@prefix sh: <http://www.w3.org/ns/shacl#> . \
                @prefix tts: <http://w3id.org/tripletrail/shapes#> . \
                <s> tts:geometry [ sh:order 1 ; sh:path ( <q> <g> ) ] , [ sh:order 0 ; sh:path <g> ] , \
                  [ sh:order 2 ; tts:lineBetween ( <a> <b> ) ] .";
            let mut g = Graph::new();
            for q in oxigraph::io::RdfParser::from_format(oxigraph::io::RdfFormat::Turtle)
                .with_base_iri("http://x/")
                .unwrap()
                .for_slice(ttl.as_bytes())
            {
                g.insert(&oxigraph::model::Triple::from(q.unwrap()));
            }
            g
        };
        let s = NamedNode::new_unchecked("http://x/s");
        let ways = Ways::of(&shapes, s.as_ref().into()).unwrap();
        assert_eq!(
            ways,
            Ways(vec![
                Way::Path("<http://x/g>".into()),
                Way::Path("<http://x/q>/<http://x/g>".into()),
                Way::Line("<http://x/a>".into(), "<http://x/b>".into(), None),
            ])
        );
        assert!(ways.has_later_paths());
        assert_eq!(ways.path(), Some("<http://x/g>"));
        let text = ways.pattern("?f", "?w");
        assert!(
            text.contains("{ ?f <http://x/q>/<http://x/g> ?w . FILTER NOT EXISTS { ?f <http://x/g> ?w_1_0 . } }"),
            "{text}"
        );
        // Narrowed to a few instances, by name; another instance gets nothing from it.
        let few = Ways(vec![
            Way::Path("<g>".into()),
            Way::Few("<q>/<g>".into(), vec!["<x>".into()]),
        ]);
        assert!(
            few.pattern("?f", "?w")
                .contains("{ VALUES ?f { <x> } ?f <q>/<g> ?w . FILTER NOT EXISTS")
        );
        assert!(few.pattern("<x>", "?w").contains("{ <x> <q>/<g> ?w ."));
        assert!(
            few.pattern("<y>", "?w")
                .contains("{ FILTER(false) FILTER NOT EXISTS")
        );
        assert!(!few.has_later_paths());
    }

    #[test]
    fn a_filtered_path_of_alternatives_is_a_union() {
        let path = Ways(vec![Way::Path(
            "(<p>/<g>|(<q>|^(<r>))/<g>|<a(b)|c>)".into(),
        )]);
        assert_eq!(
            path.filtered("?f", "?w", "FILTER(?w)"),
            "{ ?f <p>/<g> ?w . FILTER(?w) } UNION { ?f (<q>|^(<r>))/<g> ?w . FILTER(?w) } \
             UNION { ?f <a(b)|c> ?w . FILTER(?w) }"
        );
        // Unfiltered, or one alternative, as it is.
        assert_eq!(
            path.pattern("?f", "?w"),
            "?f (<p>/<g>|(<q>|^(<r>))/<g>|<a(b)|c>) ?w ."
        );
        assert_eq!(alternatives("(<p>|<q>)/<g>"), ["(<p>|<q>)/<g>"]);
        assert_eq!(alternatives("(<p>)|(<q>)"), ["(<p>)", "(<q>)"]);
        // Beside a line way, the path's alternatives each keep the filter.
        let both = Ways(vec![
            Way::Path("<p>|<q>".into()),
            Way::Line("<a>".into(), "<b>".into(), None),
        ]);
        let text = both.filtered("?f", "?w", "FILTER(?w)");
        assert!(
            text.starts_with("{ { ?f <p> ?w . FILTER(?w) } UNION { ?f <q> ?w . FILTER(?w) } }"),
            "{text}"
        );
    }
}
