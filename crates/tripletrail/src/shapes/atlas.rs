//! Shapes of [RINF Atlas](https://rinf-atlas.matdata.eu) (`atlas:` annotations on SHACL node
//! shapes, one per class on its map) as a TripleTrail shape set in the `tts:` vocabulary (#148):
//! the start of a domain shape pack.
//!
//! | RINF Atlas | TripleTrail |
//! | --- | --- |
//! | `atlas:key`, `atlas:color`, `atlas:minZoom`, `atlas:drawOrder`, `atlas:labelPath` | the `tts:` term of the same name |
//! | `atlas:geometryPath`, `atlas:geometryFrom P` | one `tts:geometry`: its paths, and each geometry path of the set behind `P`, as the branches of an `sh:alternativePath`; then, for the features without, a way per later round behind `P` and one more link of the set per round (up to three rounds, #259) |
//! | `atlas:lineBetween ( A B )` | a second `tts:geometry` with `tts:lineBetween`: each end is every geometry path of the set behind `A` (or `B`), as an `sh:alternativePath` |
//! | `atlas:enabled false` | `sh:deactivated true` |
//! | facet `atlas:kind` `terms` or `boolean`, `range`, `text` | `tts:facet` `tts:Facet-Values`, `-Range`, `-Text` |
//! | facet `atlas:kind "link"` | a property shape without `tts:facet` |
//! | `atlas:facet`, `rdfs:label` of a facet | `tts:key`, `sh:name` |
//! | `atlas:colorClasses` with `atlas:upTo` | `tts:colorClasses` with `tts:upTo` |
//! | `atlas:shared true` | its facets added to every node shape |
//! | `atlas:order` of a node shape | `sh:order` (where the map's legend lists the class) |
//! | `atlas:unit` of a facet (a QUDT unit's local name) | `qudt:hasUnit` |
//! | `atlas:colorBy` of a facet | `tts:colorBy` (suggested first in the map's "Colour by") |
//! | `atlas:geometryFrom P` of a running track or track, with a `section-of-line` shape | `tts:rollsUpTo [ sh:path P ; sh:class ]` the section's class: the section takes its tracks' facets on the map, as RINF Atlas's `ROLLUP` |
//! | a view (`atlas:view`, in `views/`) | an `tts:MapView`: `tts:key`, `rdfs:label`, `rdfs:comment`, `sh:order` |
//! | `atlas:viewClass` of a view | `tts:showsClass`: the class of the node shape with that `atlas:key` |
//! | `atlas:filter [ atlas:facet ; sh:in ]` of a view | `tts:filter [ tts:key ; sh:in ]` |
//! | `atlas:colorBy` of a view naming a facet or check | `tts:colorFacet` |
//! | a view's facets (`sh:property [ atlas:facet ; sh:order ]`) | `tts:listsFacets ( key … )`, by their order, after its `atlas:groupBy` facet |
//! | a view's requirement checks (`atlas:check`), and their summary | facets computed from the data (`tts:select`) of the checked classes and the section class (`atlas/checks.rs`) |
//!
//! RINF Atlas takes the first branch that gives a geometry; TripleTrail's readers read every
//! branch, so a feature with a geometry on two of them has both.
//!
//! What has no TripleTrail reader is left out and named in the notes: record
//! facets, `atlas:geometry` (a rendering hint) and the like, and the vehicle compatibility rules (TripleTrail has its own, `profiles/rinf-compatibility.ttl`).

mod checks;

use super::{Reader, TTS, node_shapes, sh, tts};
use oxigraph::model::vocab::{rdf, rdfs, xsd};
use oxigraph::model::{
    BlankNode, Graph, Literal, NamedNode, NamedOrBlankNode, NamedOrBlankNodeRef, Term, TermRef,
    Triple, TripleRef,
};
use std::collections::BTreeMap;

/// The classes (by `atlas:key`) RINF Atlas counts, filters and colours by the facets of their
/// parts, with the parts' classes: its `ROLLUP` (`api/app/query.py`).
const ROLLUP: [(&str, &[&str]); 1] = [("section-of-line", &["running-track", "track"])];

const QUDT_UNIT: &str = "http://qudt.org/vocab/unit/";

fn qudt_has_unit() -> NamedNode {
    NamedNode::new_unchecked("http://qudt.org/schema/qudt/hasUnit")
}

/// Whether `name` can be the local name of a QUDT unit (`M`, `KiloM-PER-HR`, `M-PER-SEC2`).
fn is_unit_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The namespace of the RINF Atlas annotations.
pub const ATLAS: &str = "https://rinf-atlas.matdata.eu/ns#";

fn atlas(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{ATLAS}{local}"))
}

/// A set of RINF Atlas shapes as a shape set, with notes on what was left out (each with a
/// count). Node shapes get `tts:hiddenFrom` each of `hidden_from`, and so do their property
/// shapes, so that a subclass's shape for one of those readers does not inherit them either.
pub fn from_atlas(shapes: &Graph, hidden_from: &[Reader]) -> (Graph, Vec<String>) {
    let chained = with_chains(shapes);
    let shapes = &chained;
    let mut out = Graph::new();
    let mut notes: BTreeMap<String, usize> = BTreeMap::new();
    let mut note = |text: String| *notes.entry(text).or_default() += 1;
    let (shared, enabled, key, compatibility, view) = (
        atlas("shared"),
        atlas("enabled"),
        atlas("key"),
        atlas("compatibility"),
        atlas("view"),
    );
    let is_true = |node: NamedOrBlankNodeRef<'_>, p: &NamedNode| matches!(shapes.object_for_subject_predicate(node, p), Some(TermRef::Literal(l)) if l.value() == "true");
    let nodes = node_shapes(shapes);
    // The facets of the shared shapes, added to every node shape.
    let shared_facets: Vec<NamedOrBlankNode> = nodes
        .iter()
        .filter(|n| is_true(n.as_ref(), &shared))
        .flat_map(|n| properties(shapes, n.as_ref()))
        .collect();
    // Every geometry path of the set, behind which `atlas:geometryFrom` reads the linked
    // features' geometry.
    let mut all_geometries: Vec<Term> = Vec::new();
    let mut seen = Vec::new();
    for node in &nodes {
        for g in geometry_paths(shapes, node.as_ref()) {
            let text = render(shapes, g.as_ref());
            if !seen.contains(&text) {
                seen.push(text);
                all_geometries.push(g);
            }
        }
    }
    for node in &nodes {
        let node_ref = node.as_ref();
        if is_true(node_ref, &shared) {
            continue;
        }
        if is_true(node_ref, &compatibility) {
            note("the vehicle compatibility rules (atlas:compatibility)".into());
            continue;
        }
        if shapes
            .object_for_subject_predicate(node_ref, &view)
            .is_some()
        {
            map_view(shapes, &mut out, node_ref, &nodes, &mut note);
            continue;
        }
        if shapes
            .object_for_subject_predicate(node_ref, &key)
            .is_none()
        {
            note("node shapes without atlas:key".into());
            continue;
        }
        let NamedOrBlankNode::NamedNode(iri) = node else {
            note("node shapes that are blank nodes".into());
            continue;
        };
        let subject = NamedOrBlankNodeRef::from(iri.as_ref());
        out.insert(TripleRef::new(subject, rdf::TYPE, &sh("NodeShape")));
        for t in shapes.triples_for_subject(node_ref) {
            let p = t.predicate.as_str();
            if let Some(local) = p.strip_prefix(ATLAS) {
                match local {
                    "key" | "color" | "minZoom" | "drawOrder" | "labelPath" => {
                        let object = copy_term(shapes, &mut out, t.object);
                        let object = if matches!(local, "minZoom" | "drawOrder") {
                            integer(object)
                        } else {
                            object
                        };
                        out.insert(TripleRef::new(subject, &tts(local), &object));
                    }
                    "enabled" => {
                        if !is_true(node_ref, &enabled) {
                            out.insert(TripleRef::new(
                                subject,
                                &sh("deactivated"),
                                &Literal::from(true),
                            ));
                        }
                    }
                    "geometryPath" | "geometryFrom" | "lineBetween" => {}
                    local if local.starts_with(CHAIN) => {}
                    // Where the map's legend lists the class.
                    "order" => {
                        let object = integer(copy_term(shapes, &mut out, t.object));
                        out.insert(TripleRef::new(subject, &sh("order"), &object));
                    }
                    other => note(format!("atlas:{other} on node shapes")),
                }
            } else if p == rdf::TYPE.as_str() || p == sh("property").as_str() {
            } else if p.starts_with(TTS)
                || t.predicate == sh("targetClass").as_ref()
                || !p.starts_with(ATLAS)
            {
                let object = copy_term(shapes, &mut out, t.object);
                out.insert(TripleRef::new(subject, t.predicate, &object));
            }
        }
        // The geometries: the shape's own paths, then those of the features it reads them
        // from.
        let mut geometries: Vec<Way> = geometry_paths(shapes, node_ref)
            .into_iter()
            .map(|path| Way { before: None, path })
            .collect();
        for from in shapes.objects_for_subject_predicate(node_ref, &atlas("geometryFrom")) {
            for g in &all_geometries {
                geometries.push(Way {
                    before: Some(from.into_owned()),
                    path: g.clone(),
                });
            }
        }
        // The later rounds, each a way of its own for the features the rounds before gave
        // none (#259): behind the link and one more link of the set per round.
        let rounds: Vec<Vec<Way>> = (2..=ROUNDS)
            .map(|round| {
                shapes
                    .objects_for_subject_predicate(node_ref, &chain(round))
                    .flat_map(|before| {
                        all_geometries.iter().map(move |g| Way {
                            before: Some(before.into_owned()),
                            path: g.clone(),
                        })
                    })
                    .collect()
            })
            .collect();
        // RINF Atlas rolls the facets of a section's tracks up to the section (its `ROLLUP`):
        // the tracks' link to their section, as `tts:rollsUpTo` (#248).
        let own_key = match shapes.object_for_subject_predicate(node_ref, &key) {
            Some(TermRef::Literal(l)) => l.value().to_owned(),
            _ => String::new(),
        };
        for (parent_key, parts) in ROLLUP {
            if !parts.contains(&own_key.as_str()) {
                continue;
            }
            let parent_class = nodes.iter().find_map(|n| {
                matches!(shapes.object_for_subject_predicate(n, &key),
                    Some(TermRef::Literal(l)) if l.value() == parent_key)
                .then(|| shapes.object_for_subject_predicate(n, &sh("targetClass")))
                .flatten()
            });
            let link = shapes.object_for_subject_predicate(node_ref, &atlas("geometryFrom"));
            if let (Some(TermRef::NamedNode(class)), Some(link)) = (parent_class, link) {
                let up = BlankNode::default();
                let link = copy_term(shapes, &mut out, link);
                out.insert(TripleRef::new(subject, &tts("rollsUpTo"), &up));
                out.insert(TripleRef::new(&up, &sh("path"), &link));
                out.insert(TripleRef::new(&up, &sh("class"), class));
            }
        }
        // One path way per round, which the readers take in order: a path per candidate, as
        // the branches of an sh:alternativePath when there are several.
        let mut order = 0_i64;
        for ways in std::iter::once(geometries).chain(rounds) {
            if let Some(path) = one_path(shapes, &mut out, ways) {
                let way = BlankNode::default();
                out.insert(TripleRef::new(subject, &tts("geometry"), &way));
                out.insert(TripleRef::new(&way, &sh("path"), &path));
                out.insert(TripleRef::new(&way, &sh("order"), &Literal::from(order)));
                order += 1;
            }
        }
        // A line between two linked features, for the instances without a geometry of
        // their own: each end is a geometry path of the set behind the link.
        if let Some(pair) = shapes.object_for_subject_predicate(node_ref, &atlas("lineBetween")) {
            let ends: Vec<Option<Term>> = list_items(shapes, pair)
                .into_iter()
                .map(|link| {
                    let behind = all_geometries.iter().map(|g| Way {
                        before: Some(link.into_owned()),
                        path: g.clone(),
                    });
                    one_path(shapes, &mut out, behind.collect())
                })
                .collect();
            match ends.as_slice() {
                [Some(a), Some(b)] => {
                    let way = BlankNode::default();
                    let list = write_list(&mut out, vec![a.clone(), b.clone()]);
                    out.insert(TripleRef::new(subject, &tts("geometry"), &way));
                    out.insert(TripleRef::new(&way, &tts("lineBetween"), &list));
                    out.insert(TripleRef::new(&way, &sh("order"), &Literal::from(order)));
                }
                _ => note("atlas:lineBetween without two paths".into()),
            }
        }
        for h in hidden_from {
            out.insert(TripleRef::new(subject, &tts("hiddenFrom"), &h.iri()));
        }
        // The facets: the shape's own, then the shared ones it has no facet of that key for.
        let own = properties(shapes, node_ref);
        let mut keys: Vec<Term> = own
            .iter()
            .filter_map(|p| shapes.object_for_subject_predicate(p, &atlas("facet")))
            .map(TermRef::into_owned)
            .collect();
        let mut facets = own.clone();
        for p in &shared_facets {
            match shapes.object_for_subject_predicate(p, &atlas("facet")) {
                Some(k) if keys.iter().any(|o| o.as_ref() == k) => {}
                k => {
                    keys.extend(k.map(TermRef::into_owned));
                    facets.push(p.clone());
                }
            }
        }
        for p in facets {
            if let Some(facet) = facet(shapes, &mut out, p.as_ref(), &mut note) {
                out.insert(TripleRef::new(subject, &sh("property"), &facet));
            }
        }
    }
    // The node shapes' property shapes (facets, computed checks) are hidden from the same
    // readers: a subclass's shape for one of them inherits none.
    let hidden: Vec<Triple> = out
        .triples_for_predicate(&sh("property"))
        .flat_map(|t| {
            let p = t.object.into_owned();
            hidden_from.iter().filter_map(move |h| {
                Some(Triple::new(
                    NamedOrBlankNode::try_from(p.clone()).ok()?,
                    tts("hiddenFrom"),
                    h.iri(),
                ))
            })
        })
        .collect();
    for t in &hidden {
        out.insert(t);
    }
    let notes = notes
        .into_iter()
        .map(|(text, n)| format!("left out: {text} ({n})"))
        .collect();
    (out, notes)
}

/// A property shape of RINF Atlas as one of TripleTrail, `None` when it has no TripleTrail reader.
fn facet(
    shapes: &Graph,
    out: &mut Graph,
    p: NamedOrBlankNodeRef<'_>,
    note: &mut impl FnMut(String),
) -> Option<BlankNode> {
    let kind = match shapes.object_for_subject_predicate(p, &atlas("kind")) {
        Some(TermRef::Literal(l)) => l.value().to_owned(),
        _ => "terms".to_owned(),
    };
    let facet_kind = match kind.as_str() {
        "terms" | "boolean" => Some("Facet-Values"),
        "range" => Some("Facet-Range"),
        "text" => Some("Facet-Text"),
        "link" => None,
        other => {
            note(format!("{other} facets"));
            return None;
        }
    };
    let path = shapes.object_for_subject_predicate(p, &sh("path"))?;
    let shape = BlankNode::default();
    let path = copy_term(shapes, out, path);
    out.insert(TripleRef::new(&shape, &sh("path"), &path));
    if let Some(kind) = facet_kind {
        out.insert(TripleRef::new(&shape, &tts("facet"), &tts(kind)));
    }
    for t in shapes.triples_for_subject(p) {
        let predicate = t.predicate.as_str();
        if let Some(local) = predicate.strip_prefix(ATLAS) {
            match local {
                "facet" => {
                    out.insert(TripleRef::new(&shape, &tts("key"), t.object));
                }
                "colorClasses" => {
                    let classes = color_classes(shapes, out, t.object);
                    out.insert(TripleRef::new(&shape, &tts("colorClasses"), &classes));
                }
                "kind" => {}
                "colorBy" => {
                    let object = integer(copy_term(shapes, out, t.object));
                    out.insert(TripleRef::new(&shape, &tts("colorBy"), &object));
                }
                // A QUDT unit's local name ("KiloM-PER-HR"): the facet's unit.
                "unit" => match t.object {
                    TermRef::Literal(l) if is_unit_name(l.value()) => {
                        let unit = NamedNode::new_unchecked(format!("{QUDT_UNIT}{}", l.value()));
                        out.insert(TripleRef::new(&shape, &qudt_has_unit(), &unit));
                    }
                    _ => note("atlas:unit that is not a QUDT unit name".into()),
                },
                other => note(format!("atlas:{other} on facets")),
            }
        } else if t.predicate == rdfs::LABEL {
            if shapes
                .object_for_subject_predicate(p, &sh("name"))
                .is_none()
            {
                out.insert(TripleRef::new(&shape, &sh("name"), t.object));
            }
        } else if t.predicate != sh("path").as_ref() {
            let object = copy_term(shapes, out, t.object);
            out.insert(TripleRef::new(&shape, t.predicate, &object));
        }
    }
    Some(shape)
}

/// `atlas:colorClasses`, a list of `[ atlas:upTo n ; rdfs:label … ]`, in `tts:` terms.
/// A view (`atlas:view`) as an `tts:MapView`: its classes (`atlas:viewClass`, the class of
/// the node shape with that `atlas:key`), the filters it sets and the facet it colours by.
/// Its group picker, its requirement checks and the facets it lists first are left out.
fn map_view(
    shapes: &Graph,
    out: &mut Graph,
    node: NamedOrBlankNodeRef<'_>,
    nodes: &[NamedOrBlankNode],
    note: &mut impl FnMut(String),
) {
    let NamedOrBlankNodeRef::NamedNode(iri) = node else {
        note("views that are blank nodes".into());
        return;
    };
    let subject = NamedOrBlankNodeRef::from(iri);
    let class_of = |key: TermRef<'_>| -> Option<TermRef<'_>> {
        nodes
            .iter()
            .find(|n| shapes.object_for_subject_predicate(*n, &atlas("key")) == Some(key))
            .and_then(|n| shapes.object_for_subject_predicate(n, &sh("targetClass")))
    };
    // The facet keys of the set, which a view can colour by (the others are checks).
    let facet = atlas("facet");
    // The keys of the views' checks, which become facets too.
    let checks = checks::keys(shapes);
    let is_check = |key: TermRef<'_>| checks.iter().any(|k| k.as_ref() == key);
    let is_facet = |key: TermRef<'_>| {
        is_check(key)
            || shapes.triples_for_predicate(&facet).any(|t| {
                t.object == key
                    && !shapes.contains(TripleRef::new(
                        t.subject,
                        &atlas("kind"),
                        &Literal::from("record"),
                    ))
            })
    };
    // The facet keys of the classes the view shows, with the shared ones: a filter or colour
    // on another key does nothing on the map.
    let shown: Vec<&NamedOrBlankNode> = shapes
        .objects_for_subject_predicate(node, &atlas("viewClass"))
        .filter_map(|key| {
            nodes
                .iter()
                .find(|n| shapes.object_for_subject_predicate(*n, &atlas("key")) == Some(key))
        })
        .collect();
    let shown_has = |key: TermRef<'_>| {
        nodes
            .iter()
            .filter(|n| {
                shown.contains(n)
                    || matches!(shapes.object_for_subject_predicate(*n, &atlas("shared")),
                        Some(TermRef::Literal(l)) if l.value() == "true")
            })
            .flat_map(|n| properties(shapes, n.as_ref()))
            .any(|p| shapes.object_for_subject_predicate(&p, &facet) == Some(key))
            || is_check(key)
    };
    out.insert(TripleRef::new(subject, rdf::TYPE, &tts("MapView")));
    let disabled = matches!(shapes.object_for_subject_predicate(node, &atlas("enabled")),
        Some(TermRef::Literal(l)) if l.value() == "false");
    if !disabled {
        checks::write(shapes, out, node, nodes, note);
    }
    for t in shapes.triples_for_subject(node) {
        let p = t.predicate.as_str();
        match p.strip_prefix(ATLAS) {
            Some("view") => {
                let object = copy_term(shapes, out, t.object);
                out.insert(TripleRef::new(subject, &tts("key"), &object));
            }
            Some("enabled") => {
                if matches!(t.object, TermRef::Literal(l) if l.value() == "false") {
                    out.insert(TripleRef::new(
                        subject,
                        &sh("deactivated"),
                        &Literal::from(true),
                    ));
                }
            }
            Some("viewClass") => match class_of(t.object) {
                Some(class) => {
                    out.insert(TripleRef::new(subject, &tts("showsClass"), class));
                }
                None => note("atlas:viewClass naming no class of the set".into()),
            },
            Some("filter") => {
                let Some(f) = as_subject(t.object) else {
                    continue;
                };
                if let Some(key) = shapes.object_for_subject_predicate(f, &facet)
                    && !shown_has(key)
                {
                    note("view filters on a facet the view's classes lack".into());
                }
                let filter = BlankNode::default();
                for ft in shapes.triples_for_subject(f) {
                    let predicate = if ft.predicate == facet.as_ref() {
                        tts("key")
                    } else if [sh("in"), sh("minInclusive"), sh("maxInclusive")]
                        .iter()
                        .any(|s| ft.predicate == s.as_ref())
                    {
                        ft.predicate.into_owned()
                    } else {
                        note(format!(
                            "{} in view filters",
                            short_name(ft.predicate.as_str())
                        ));
                        continue;
                    };
                    let object = copy_term(shapes, out, ft.object);
                    out.insert(TripleRef::new(&filter, &predicate, &object));
                }
                out.insert(TripleRef::new(subject, &tts("filter"), &filter));
            }
            Some("colorBy") => {
                if is_facet(t.object) {
                    if !shown_has(t.object) {
                        note("atlas:colorBy of views on a facet the view's classes lack".into());
                    }
                    let object = copy_term(shapes, out, t.object);
                    out.insert(TripleRef::new(subject, &tts("colorFacet"), &object));
                } else {
                    note("atlas:colorBy of views naming no facet or check".into());
                }
            }
            // Read by `checks::write`.
            Some("checkClass" | "checkSection" | "summaryCheck" | "summaryLabel") => {}
            // The group picker's facet is listed first (below); its label is the facet's title.
            Some("groupBy" | "groupLabel") => {}
            Some(other) => note(format!("atlas:{other} on views")),
            None if t.predicate == rdfs::LABEL || t.predicate == rdfs::COMMENT => {
                let object = copy_term(shapes, out, t.object);
                out.insert(TripleRef::new(subject, t.predicate, &object));
            }
            None if t.predicate == sh("order").as_ref() => {
                let object = integer(copy_term(shapes, out, t.object));
                out.insert(TripleRef::new(subject, &sh("order"), &object));
            }
            // Checks are read by `checks::write`, listed facets below.
            None if t.predicate == sh("property").as_ref() => {}
            None => {}
        }
    }
    // The facets the view lists first, by their order, after the facet of its group picker
    // (`atlas:groupBy`, a country), which TripleTrail's map has no picker for.
    let mut listed: Vec<(i64, String, Term)> = Vec::new();
    let group = shapes
        .objects_for_subject_predicate(node, &atlas("groupBy"))
        .map(|key| (Some(i64::MIN), key));
    let properties_listed = shapes
        .objects_for_subject_predicate(node, &sh("property"))
        .filter_map(as_subject)
        .filter_map(|p| Some((None, shapes.object_for_subject_predicate(p, &facet)?, p)));
    let candidates: Vec<(Option<i64>, TermRef<'_>, Option<NamedOrBlankNodeRef<'_>>)> = group
        .map(|(o, k)| (o, k, None))
        .chain(properties_listed.map(|(o, k, p)| (o, k, Some(p))))
        .collect();
    for (fixed, key, p) in candidates {
        // Any class's: a layer the view hides, once shown, lists them first too (RINF
        // Atlas's tracks, whose parameters its section views list).
        let of_a_class = nodes
            .iter()
            .filter(|n| {
                shapes
                    .object_for_subject_predicate(*n, &atlas("view"))
                    .is_none()
            })
            .flat_map(|n| properties(shapes, n.as_ref()))
            .any(|p| shapes.object_for_subject_predicate(&p, &facet) == Some(key));
        if !of_a_class {
            note("listed facets that are no facet of the set".into());
            continue;
        }
        let order = fixed.unwrap_or_else(|| {
            match p.and_then(|p| shapes.object_for_subject_predicate(p, &sh("order"))) {
                Some(TermRef::Literal(l)) => l.value().trim().parse().unwrap_or(i64::MAX),
                _ => i64::MAX,
            }
        });
        listed.push((order, key.to_string(), key.into_owned()));
    }
    if !listed.is_empty() {
        // Blank nodes come in no fixed order: ties by key.
        listed.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        let mut seen = std::collections::HashSet::new();
        listed.retain(|(_, k, _)| seen.insert(k.clone()));
        let head = write_list(out, listed.into_iter().map(|(_, _, k)| k).collect());
        out.insert(TripleRef::new(subject, &tts("listsFacets"), &head));
    }
}

/// `atlas:x` or `sh:x` for an IRI in those namespaces, else the IRI.
fn short_name(iri: &str) -> String {
    if let Some(local) = iri.strip_prefix(ATLAS) {
        format!("atlas:{local}")
    } else if let Some(local) = iri.strip_prefix(super::SH) {
        format!("sh:{local}")
    } else {
        format!("<{iri}>")
    }
}

fn color_classes(shapes: &Graph, out: &mut Graph, head: TermRef<'_>) -> Term {
    let items: Vec<Term> = list_items(shapes, head)
        .into_iter()
        .map(|item| {
            let class = BlankNode::default();
            if let Some(item) = as_subject(item) {
                for t in shapes.triples_for_subject(item) {
                    let predicate = if t.predicate == atlas("upTo").as_ref() {
                        tts("upTo")
                    } else {
                        t.predicate.into_owned()
                    };
                    let object = if predicate == tts("upTo") {
                        decimal(t.object.into_owned())
                    } else {
                        copy_term(shapes, out, t.object)
                    };
                    out.insert(TripleRef::new(&class, &predicate, &object));
                }
            }
            class.into()
        })
        .collect();
    write_list(out, items)
}

/// The property shapes of a node shape, in their order.
fn properties(shapes: &Graph, node: NamedOrBlankNodeRef<'_>) -> Vec<NamedOrBlankNode> {
    let mut list: Vec<(i64, String, NamedOrBlankNode)> = shapes
        .objects_for_subject_predicate(node, &sh("property"))
        .filter_map(as_subject)
        .map(|p| {
            let order = match shapes.object_for_subject_predicate(p, &sh("order")) {
                Some(TermRef::Literal(l)) => {
                    l.value().parse::<f64>().map_or(i64::MAX, |v| v as i64)
                }
                _ => i64::MAX,
            };
            let key = shapes
                .object_for_subject_predicate(p, &atlas("facet"))
                .map(|k| k.to_string())
                .unwrap_or_default();
            (order, key, p.into_owned())
        })
        .collect();
    list.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    list.into_iter().map(|(_, _, p)| p).collect()
}

/// The rounds of links over which a feature reads the geometry of linked ones (#259): a
/// detector takes its track's, or its track's section's when the track has none. RINF Atlas
/// derives a geometry over up to six (`derive_geometry`); on RINF two suffice, and each round
/// is a way of its own that every map tile reads.
const ROUNDS: usize = 3;

/// The local name of the scratch predicates [`with_chains`] links a node shape to its chains
/// with, one per round; they never reach the shape set.
const CHAIN: &str = "_geometryChain";

fn chain(round: usize) -> NamedNode {
    atlas(&format!("{CHAIN}{round}"))
}

/// `shapes` with, for each `atlas:geometryFrom P` of a node shape and each later round
/// (2 to [`ROUNDS`]), the chain of links behind which that round reads the linked features'
/// geometry: `P`, then one step on any link of the set (an `sh:alternativePath` of them) per
/// round after the first. Linked to the node shape by [`chain`]`(round)`.
fn with_chains(shapes: &Graph) -> Graph {
    let mut out = shapes.clone();
    let from = atlas("geometryFrom");
    let alternative = sh("alternativePath");
    // Every link of the set, an alternative path as its branches, each once.
    let mut links: Vec<Term> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for t in shapes.triples_for_predicate(&from) {
        let branches = match as_subject(t.object)
            .and_then(|p| shapes.object_for_subject_predicate(p, &alternative))
        {
            Some(list) => list_items(shapes, list),
            None => vec![t.object],
        };
        for b in branches {
            let text = render(shapes, b);
            if !seen.contains(&text) {
                seen.push(text);
                links.push(b.into_owned());
            }
        }
    }
    let any: Term = match links.len() {
        0 => return out,
        1 => links[0].clone(),
        _ => {
            let node = BlankNode::default();
            let list = write_list(&mut out, links);
            out.insert(TripleRef::new(&node, &alternative, &list));
            node.into()
        }
    };
    let pairs: Vec<(NamedOrBlankNode, Term)> = shapes
        .triples_for_predicate(&from)
        .map(|t| (t.subject.into_owned(), t.object.into_owned()))
        .collect();
    for (node, link) in pairs {
        for round in 2..=ROUNDS {
            let mut items = vec![link.clone()];
            items.extend((1..round).map(|_| any.clone()));
            let list = write_list(&mut out, items);
            out.insert(TripleRef::new(&node, &chain(round), &list));
        }
    }
    out
}

/// `ways` as one path, written into `out`: an `sh:alternativePath` of the distinct ones when
/// there are several, `None` when there are none.
fn one_path(shapes: &Graph, out: &mut Graph, ways: Vec<Way>) -> Option<Term> {
    let mut placed: Vec<String> = Vec::new();
    let mut paths: Vec<Term> = Vec::new();
    for g in ways {
        let text = g.render(shapes);
        if !placed.contains(&text) {
            placed.push(text);
            paths.push(g.write(shapes, out));
        }
    }
    match paths.len() {
        0 => None,
        1 => paths.pop(),
        _ => {
            let alternative = BlankNode::default();
            let list = write_list(out, paths);
            out.insert(TripleRef::new(&alternative, &sh("alternativePath"), &list));
            Some(alternative.into())
        }
    }
}

/// A shape's own geometry paths: each `atlas:geometryPath`, an `sh:alternativePath` as its
/// branches in order.
fn geometry_paths(shapes: &Graph, node: NamedOrBlankNodeRef<'_>) -> Vec<Term> {
    let alternative = sh("alternativePath");
    let mut paths = Vec::new();
    for path in shapes.objects_for_subject_predicate(node, &atlas("geometryPath")) {
        match as_subject(path).and_then(|p| shapes.object_for_subject_predicate(p, &alternative)) {
            Some(branches) => paths.extend(
                list_items(shapes, branches)
                    .into_iter()
                    .map(TermRef::into_owned),
            ),
            None => paths.push(path.into_owned()),
        }
    }
    paths
}

/// A geometry path: `path`, behind the steps of `before` (an `atlas:geometryFrom`).
#[derive(Clone)]
struct Way {
    before: Option<Term>,
    path: Term,
}

impl Way {
    /// The path, written into `out`: behind `before`, as one sequence.
    fn write(&self, shapes: &Graph, out: &mut Graph) -> Term {
        let Some(before) = &self.before else {
            return copy_term(shapes, out, self.path.as_ref());
        };
        let mut items = Vec::new();
        for part in [before.as_ref(), self.path.as_ref()] {
            let members = list_items(shapes, part);
            if members.is_empty() {
                items.push(copy_term(shapes, out, part));
            } else {
                items.extend(members.into_iter().map(|m| copy_term(shapes, out, m)));
            }
        }
        write_list(out, items)
    }

    fn render(&self, shapes: &Graph) -> String {
        match &self.before {
            None => render(shapes, self.path.as_ref()),
            Some(before) => {
                let flat = |t: TermRef<'_>| {
                    let r = render(shapes, t);
                    r.strip_prefix('(')
                        .and_then(|r| r.strip_suffix(')'))
                        .map_or(r.clone(), str::to_owned)
                };
                format!("({} {})", flat(before.as_ref()), flat(self.path.as_ref()))
            }
        }
    }
}

/// A path as text, to compare paths as written (blank nodes by what they hold).
fn render(shapes: &Graph, term: TermRef<'_>) -> String {
    match term {
        TermRef::BlankNode(_) => {
            let items = list_items(shapes, term);
            if items.is_empty() {
                let mut parts: Vec<String> = as_subject(term)
                    .map(|s| {
                        shapes
                            .triples_for_subject(s)
                            .map(|t| format!("{} {}", t.predicate, render(shapes, t.object)))
                            .collect()
                    })
                    .unwrap_or_default();
                parts.sort();
                format!("[{}]", parts.join(";"))
            } else {
                format!(
                    "({})",
                    items
                        .into_iter()
                        .map(|i| render(shapes, i))
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            }
        }
        other => other.to_string(),
    }
}

/// A term of `shapes` written into `out`: blank nodes (paths, lists) as new copies, so that
/// no blank node is shared between two shapes.
fn copy_term(shapes: &Graph, out: &mut Graph, term: TermRef<'_>) -> Term {
    match term {
        TermRef::BlankNode(b) => {
            let copy = BlankNode::default();
            for t in shapes.triples_for_subject(b) {
                let object = copy_term(shapes, out, t.object);
                out.insert(TripleRef::new(&copy, t.predicate, &object));
            }
            copy.into()
        }
        other => other.into_owned(),
    }
}

fn write_list(out: &mut Graph, items: Vec<Term>) -> Term {
    let mut head: Term = rdf::NIL.into_owned().into();
    for item in items.into_iter().rev() {
        let cell = BlankNode::default();
        out.insert(TripleRef::new(&cell, rdf::FIRST, &item));
        out.insert(TripleRef::new(&cell, rdf::REST, &head));
        head = cell.into();
    }
    head
}

fn list_items<'a>(shapes: &'a Graph, head: TermRef<'a>) -> Vec<TermRef<'a>> {
    let mut items = Vec::new();
    let mut node = head;
    while let Some(cell) = as_subject(node) {
        let Some(first) = shapes.object_for_subject_predicate(cell, rdf::FIRST) else {
            break;
        };
        items.push(first);
        match shapes.object_for_subject_predicate(cell, rdf::REST) {
            Some(rest) => node = rest,
            None => break,
        }
    }
    items
}

fn as_subject(term: TermRef<'_>) -> Option<NamedOrBlankNodeRef<'_>> {
    match term {
        TermRef::NamedNode(n) => Some(n.into()),
        TermRef::BlankNode(b) => Some(b.into()),
        _ => None,
    }
}

fn integer(term: Term) -> Term {
    match &term {
        Term::Literal(l) if l.datatype() != xsd::INTEGER => {
            Literal::new_typed_literal(l.value(), xsd::INTEGER).into()
        }
        _ => term,
    }
}

fn decimal(term: Term) -> Term {
    match &term {
        Term::Literal(l) if l.datatype() == xsd::INTEGER => {
            Literal::new_typed_literal(format!("{}.0", l.value()), xsd::DECIMAL).into()
        }
        _ => term,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::io::{RdfFormat, RdfParser};

    const SHAPES: &str = r##"
PREFIX atlas: <https://rinf-atlas.matdata.eu/ns#>
PREFIX sh: <http://www.w3.org/ns/shacl#>
PREFIX era: <http://data.europa.eu/949/>
PREFIX gsp: <http://www.opengis.net/ont/geosparql#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
PREFIX ex: <https://example.org/>

ex:shared a sh:NodeShape ; atlas:shared true ;
  sh:property [ sh:path era:inCountry ; atlas:facet "country" ; atlas:kind "terms" ] ,
              [ sh:path era:opName ; atlas:facet "name" ; atlas:kind "text" ] .

ex:op a sh:NodeShape ; atlas:key "op" ; sh:targetClass era:OperationalPoint ;
  atlas:color "#d62728" ; atlas:minZoom 4 ; atlas:order 10 ;
  atlas:geometryPath [ sh:alternativePath ( ( era:netReference gsp:hasGeometry gsp:asWKT ) ( gsp:hasGeometry gsp:asWKT ) ) ] ;
  sh:property [ sh:path era:opName ; atlas:facet "name" ; rdfs:label "Name" ; atlas:kind "text" ] ,
              [ sh:path era:speed ; atlas:facet "speed" ; atlas:kind "range" ; atlas:unit "KiloM-PER-HR" ;
                atlas:colorBy 2 ;
                atlas:colorClasses ( [ atlas:upTo 40 ; rdfs:label "Slow" ] [ rdfs:label "Fast" ] ) ] ,
              [ sh:path era:track ; atlas:facet "track" ; atlas:kind "link" ] ,
              [ sh:path era:pantographs ; atlas:facet "pantographs" ; atlas:kind "record" ] .

ex:platform a sh:NodeShape ; atlas:key "platform" ; sh:targetClass era:PlatformEdge ;
  atlas:enabled false ; atlas:geometryFrom era:isPartOf .

ex:section a sh:NodeShape ; atlas:key "section" ; sh:targetClass era:SectionOfLine ;
  atlas:geometryPath ( era:netReference gsp:hasGeometry gsp:asWKT ) ;
  atlas:lineBetween ( era:opStart era:opEnd ) .

ex:rules a sh:NodeShape ; atlas:compatibility true .

ex:fast a sh:NodeShape ; atlas:view "fast" ; rdfs:label "Fast lines" ; rdfs:comment "Where trains run fast." ;
  sh:order 5 ; atlas:viewClass "op", "section", "nothing" ; atlas:groupBy "country" ;
  atlas:filter [ atlas:facet "speed" ; sh:minInclusive 100 ] , [ atlas:facet "country" ; sh:in ( era:BE era:LU ) ] ,
    [ atlas:facet "length" ; sh:minInclusive 400 ] ;
  atlas:colorBy "speed" ;
  atlas:checkClass "op" ;
  sh:property [ atlas:facet "speed" ; sh:order 3 ] , [ atlas:facet "country" ; sh:order 5 ] ,
    [ atlas:facet "length" ; sh:order 0 ] , [ atlas:facet "nope" ; sh:order 2 ] ,
    [ atlas:check "fast.ok" ; rdfs:label "Fast" ; sh:order 2 ; atlas:compareTo "speed" ; sh:minInclusive 100 ;
      atlas:appliesWhen [ atlas:facet "country" ; sh:hasValue era:BE ] ] .
"##;

    fn parse(text: &str) -> Graph {
        let mut graph = Graph::new();
        for quad in RdfParser::from_format(RdfFormat::Turtle).for_slice(text.as_bytes()) {
            graph.insert(&oxigraph::model::Triple::from(quad.unwrap()));
        }
        graph
    }

    fn ask(graph: &Graph, query: &str) -> bool {
        let store = oxigraph::store::Store::new().unwrap();
        for t in graph {
            store
                .insert(t.in_graph(oxigraph::model::GraphNameRef::DefaultGraph))
                .unwrap();
        }
        let query = format!(
            "PREFIX sh: <http://www.w3.org/ns/shacl#> PREFIX tts: <{TTS}> \
             PREFIX era: <http://data.europa.eu/949/> PREFIX gsp: <http://www.opengis.net/ont/geosparql#> \
             PREFIX ex: <https://example.org/> PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> ASK {{ {query} }}"
        );
        #[allow(deprecated)]
        let answer = store.query(query.as_str()).unwrap();
        matches!(answer, oxigraph::sparql::QueryResults::Boolean(true))
    }

    #[test]
    fn tracks_roll_up_to_their_section() {
        let shapes = r##"
PREFIX atlas: <https://rinf-atlas.matdata.eu/ns#>
PREFIX sh: <http://www.w3.org/ns/shacl#>
PREFIX era: <http://data.europa.eu/949/>
PREFIX ex: <https://example.org/>
ex:sol a sh:NodeShape ; atlas:key "section-of-line" ; sh:targetClass era:SectionOfLine .
ex:rt a sh:NodeShape ; atlas:key "running-track" ; sh:targetClass era:RunningTrack ;
  atlas:geometryFrom [ sh:alternativePath ( era:isPartOf [ sh:inversePath era:hasPart ] ) ] .
ex:op a sh:NodeShape ; atlas:key "op" ; sh:targetClass era:OperationalPoint ;
  atlas:geometryFrom era:isPartOf .
"##;
        let (set, _) = from_atlas(&parse(shapes), &[]);
        assert!(
            super::super::check(&set).is_empty(),
            "{:?}",
            super::super::check(&set)
        );
        assert!(ask(
            &set,
            "ex:rt tts:rollsUpTo [ sh:class era:SectionOfLine ; \
             sh:path [ sh:alternativePath ( era:isPartOf [ sh:inversePath era:hasPart ] ) ] ]"
        ));
        // Only the classes RINF Atlas rolls up.
        assert!(!ask(&set, "ex:op tts:rollsUpTo ?u"));
    }

    #[test]
    fn geometry_comes_from_features_linked_over_rounds() {
        let shapes = r##"
PREFIX atlas: <https://rinf-atlas.matdata.eu/ns#>
PREFIX sh: <http://www.w3.org/ns/shacl#>
PREFIX era: <http://data.europa.eu/949/>
PREFIX gsp: <http://www.opengis.net/ont/geosparql#>
PREFIX ex: <https://example.org/>
ex:sol a sh:NodeShape ; atlas:key "section-of-line" ; sh:targetClass era:SectionOfLine ;
  atlas:geometryPath ( gsp:hasGeometry gsp:asWKT ) .
ex:rt a sh:NodeShape ; atlas:key "running-track" ; sh:targetClass era:RunningTrack ;
  atlas:geometryPath ( gsp:hasGeometry gsp:asWKT ) ;
  atlas:geometryFrom [ sh:alternativePath ( era:isPartOf [ sh:inversePath era:hasPart ] ) ] .
ex:habd a sh:NodeShape ; atlas:key "habd" ; sh:targetClass era:HABD ;
  atlas:geometryFrom [ sh:inversePath era:tracksideHabd ] .
"##;
        let (set, _) = from_atlas(&parse(shapes), &[]);
        assert!(
            super::super::check(&set).is_empty(),
            "{:?}",
            super::super::check(&set)
        );
        // The detector reads the geometry behind its track; then, for the detectors without,
        // behind one more link of any kind (its track's section's), then two.
        let round = |order: i64, steps: &str| {
            format!(
                "ex:habd tts:geometry [ sh:order {order} ; sh:path ?p ] . \
                 ?p rdf:first/sh:inversePath era:tracksideHabd {steps}"
            )
        };
        assert!(
            ask(&set, &round(0, ". ?p rdf:rest/rdf:first gsp:hasGeometry")),
            "{set}"
        );
        let any = "?any rdf:rest*/rdf:first era:isPartOf . \
                   ?any rdf:rest*/rdf:first/sh:inversePath era:tracksideHabd";
        assert!(ask(
            &set,
            &format!(
                "{} ; rdf:rest/rdf:first/sh:alternativePath ?any ; \
                 rdf:rest/rdf:rest/rdf:first gsp:hasGeometry . {any}",
                round(1, "")
            )
        ));
        assert!(ask(
            &set,
            &format!(
                "{} ; rdf:rest/rdf:rest/rdf:first/sh:alternativePath ?any ; \
                 rdf:rest/rdf:rest/rdf:rest/rdf:first gsp:hasGeometry . {any}",
                round(2, "")
            )
        ));
        assert!(!ask(&set, "ex:habd tts:geometry/sh:order 3"));
        // No scratch terms in the set.
        assert!(!ask(
            &set,
            "?s ?p ?o FILTER(CONTAINS(STR(?p), \"_geometryChain\"))"
        ));
    }

    #[test]
    fn atlas_shapes_in_mgs_terms() {
        let (set, notes) = from_atlas(&parse(SHAPES), &[Reader::Describe]);
        assert!(
            super::super::check(&set).is_empty(),
            "{:?}",
            super::super::check(&set)
        );
        // Node shape settings, and the shape hidden from describe pages.
        assert!(ask(
            &set,
            "ex:op tts:key \"op\" ; tts:color \"#d62728\" ; tts:minZoom 4 ; sh:targetClass era:OperationalPoint ; tts:hiddenFrom tts:Reader-Describe"
        ));
        // Its facets too, so a subclass's describe shape does not inherit them.
        assert!(!ask(
            &set,
            "ex:op sh:property ?p FILTER NOT EXISTS { ?p tts:hiddenFrom tts:Reader-Describe }"
        ));
        // One geometry way: the alternative's branches, in order.
        assert!(ask(
            &set,
            "ex:op tts:geometry/sh:path/sh:alternativePath ?l . ?l rdf:first/rdf:first era:netReference ; rdf:rest/rdf:first/rdf:first gsp:hasGeometry"
        ));
        // Facets by kind; the shape's own facet wins over the shared one of the same key.
        assert!(ask(
            &set,
            "ex:op sh:property [ sh:path era:opName ; tts:key \"name\" ; sh:name \"Name\" ; tts:facet tts:Facet-Text ]"
        ));
        assert!(!ask(
            &set,
            "ex:op sh:property ?a, ?b . ?a tts:key \"name\" . ?b tts:key \"name\" FILTER(?a != ?b)"
        ));
        assert!(ask(
            &set,
            "ex:op sh:property [ sh:path era:inCountry ; tts:facet tts:Facet-Values ]"
        ));
        assert!(ask(
            &set,
            "ex:op sh:property [ tts:key \"speed\" ; tts:facet tts:Facet-Range ; tts:colorClasses/rdf:first/tts:upTo 40.0 ]"
        ));
        assert!(ask(
            &set,
            "ex:op sh:property ?p . ?p tts:key \"track\" FILTER NOT EXISTS { ?p tts:facet ?f }"
        ));
        assert!(!ask(&set, "?s tts:key \"pantographs\""));
        // Switched off; geometry from the linked features, behind the link.
        assert!(ask(
            &set,
            "ex:platform sh:deactivated true ; tts:geometry/sh:path/sh:alternativePath/rdf:rest/rdf:first/rdf:first era:isPartOf"
        ));
        // A section of line: its own path first, then the line between its operational
        // points, each end behind its link.
        assert!(ask(
            &set,
            "ex:section tts:geometry [ sh:order 0 ; sh:path/rdf:first era:netReference ] , \
             [ sh:order 1 ; tts:lineBetween ?l ] . \
             ?l rdf:first/sh:alternativePath/rdf:first/rdf:first era:opStart ; \
             rdf:rest/rdf:first/sh:alternativePath/rdf:first/rdf:first era:opEnd"
        ));
        assert!(
            !notes.iter().any(|n| n.contains("lineBetween")),
            "{notes:?}"
        );
        // Neither the shared shape nor the rules are node shapes of the set.
        assert!(!ask(&set, "ex:shared ?p ?o"));
        assert!(!ask(&set, "ex:rules ?p ?o"));
        assert!(
            notes.iter().any(|n| n.contains("record facets (1)")),
            "{notes:?}"
        );
        // The class's place in the legend, and a facet's unit.
        assert!(ask(&set, "ex:op sh:order 10"));
        assert!(ask(
            &set,
            "ex:op sh:property [ tts:key \"speed\" ; <http://qudt.org/schema/qudt/hasUnit> <http://qudt.org/vocab/unit/KiloM-PER-HR> ]"
        ));
        assert!(
            !notes.iter().any(|n| n.contains("atlas:order")
                || n.contains("atlas:unit")
                || n.contains("colorBy")),
            "{notes:?}"
        );
        assert!(ask(
            &set,
            "ex:op sh:property [ tts:key \"speed\" ; tts:colorBy 2 ]"
        ));
        assert!(
            notes.iter().any(|n| n.contains("compatibility")),
            "{notes:?}"
        );
        // A view: its classes, filters, colouring and listed facets (by their order; one that
        // is no facet left out); its group picker left out.
        assert!(ask(
            &set,
            "ex:fast a tts:MapView ; tts:key \"fast\" ; sh:order 5 ; \
             tts:listsFacets ( \"country\" \"speed\" ) ; \
             tts:showsClass era:OperationalPoint, era:SectionOfLine ; tts:colorFacet \"speed\" ; \
             tts:filter [ tts:key \"speed\" ; sh:minInclusive 100 ] , \
             [ tts:key \"country\" ; sh:in/rdf:rest/rdf:first era:LU ]"
        ));
        assert!(!ask(&set, "ex:fast a sh:NodeShape"));
        assert!(
            !notes.iter().any(|n| n.contains("colorBy of views")),
            "{notes:?}"
        );
        for left in [
            "atlas:viewClass naming no class of the set (1)",
            "listed facets that are no facet of the set (2)",
            "view filters on a facet the view's classes lack (1)",
        ] {
            assert!(notes.iter().any(|n| n.contains(left)), "{left}: {notes:?}");
        }
        // Its check, a facet of the operational points computed from their speed, where they
        // are in Belgium, for the map only.
        assert!(ask(
            &set,
            "ex:op sh:property [ tts:key \"fast.ok\" ; sh:name \"Fast\" ; tts:facet tts:Facet-Values ; \
             tts:onlyFor tts:Reader-Map ; tts:select ?s ] \
             FILTER(CONTAINS(?s, \"isNumeric(?x) && (?x < 100.0)\") && CONTAINS(?s, \"<http://data.europa.eu/949/BE>\"))"
        ));
        // In a file of its own.
        assert!(
            super::super::files_of(&set)
                .iter()
                .any(|f| f.name == "view-fast.ttl" && f.text.contains("MapView")),
            "{:?}",
            super::super::files_of(&set)
                .iter()
                .map(|f| &f.name)
                .collect::<Vec<_>>()
        );
    }
}
