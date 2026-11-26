//! Shape sets as files, their checks, and layers: a higher set overrides a lower one's node
//! shapes by IRI.

use super::{Reader, TTS, VOCABULARY, node_shapes, sh, subject_of, to_turtle, tts};
use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::dataset::CanonicalizationAlgorithm;
use oxigraph::model::vocab::{rdf, xsd};
use oxigraph::model::{
    Graph, NamedNode, NamedOrBlankNode, NamedOrBlankNodeRef, TermRef, TripleRef,
};
use spargebra::algebra::{AggregateExpression, Expression, GraphPattern, OrderExpression};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use tripletrail_spatial::BUILT_IN_SERVICES;

/// A file of a shape set: its name (`signal.ttl`) and its Turtle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetFile {
    pub name: String,
    pub text: String,
}

/// Copies the triples of `subject` and of the blank nodes it reaches.
fn copy_all(
    from: &Graph,
    out: &mut Graph,
    subject: NamedOrBlankNodeRef<'_>,
    copied: &mut HashSet<NamedOrBlankNode>,
) {
    if !copied.insert(subject.into_owned()) {
        return;
    }
    for t in from.triples_for_subject(subject) {
        out.insert(t);
        if let TermRef::BlankNode(b) = t.object {
            copy_all(from, out, b.into(), copied);
        }
    }
}

/// `all` (the layers overlaid) with the generated node shapes hidden from the readers that a
/// higher set names with `tts:hidesGenerated` (#92): a pack or the dataset's own set that
/// shows only its own classes on the map. A generated shape that a higher set overrides or
/// refines (by its IRI, or a refining shape for its class) is the higher set's and stays.
pub fn hide_generated(all: &Graph, generated: &Graph, uppers: &[&Graph]) -> Graph {
    let hides = tts("hidesGenerated");
    let mut readers: Vec<NamedNode> = uppers
        .iter()
        .flat_map(|g| g.triples_for_predicate(&hides))
        .filter_map(|t| match t.object {
            TermRef::NamedNode(n) => Some(n.into_owned()),
            _ => None,
        })
        .collect();
    readers.sort_unstable();
    readers.dedup();
    if readers.is_empty() {
        return all.clone();
    }
    let (target, refines) = (sh("targetClass"), tts("refines"));
    let refined: HashSet<NamedNode> = uppers
        .iter()
        .flat_map(|g| {
            node_shapes(g)
                .into_iter()
                .filter(|n| {
                    g.object_for_subject_predicate(n, &refines)
                        .is_some_and(|v| matches!(v, TermRef::Literal(l) if l.value() == "true"))
                })
                .flat_map(|n| {
                    g.objects_for_subject_predicate(&n, &target)
                        .filter_map(|c| match c {
                            TermRef::NamedNode(c) => Some(c.into_owned()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                })
        })
        .collect();
    let mut out = all.clone();
    let hidden = tts("hiddenFrom");
    for node in node_shapes(generated) {
        let touched = uppers
            .iter()
            .any(|g| g.triples_for_subject(&node).next().is_some())
            || generated
                .objects_for_subject_predicate(&node, &target)
                .any(|c| matches!(c, TermRef::NamedNode(c) if refined.contains(&c.into_owned())));
        // Gone when a higher set replaced it by class.
        if touched || all.triples_for_subject(&node).next().is_none() {
            continue;
        }
        for reader in &readers {
            out.insert(TripleRef::new(&node, &hidden, reader));
        }
    }
    out
}

/// The key of the data-quality facet [`add_quality`] gives.
pub const QUALITY_KEY: &str = "quality";

/// `all` with a data-quality facet on each node shape, when a higher set names the named
/// graph that holds a stored SHACL validation report with `tts:qualityReport` (#92): key
/// [`QUALITY_KEY`], computed on the map (`tts:select`) as the worst severity of the report's
/// results about the feature, `violation`, `warning` or `info`, else `ok`. Its counts, filters
/// and colours work as any facet's, and its key is the same on every collection, so a map of
/// several shares it. A node shape that has a facet of that key keeps its own.
pub fn add_quality(all: &Graph, uppers: &[&Graph]) -> Graph {
    let report = tts("qualityReport");
    let graph = uppers
        .iter()
        .flat_map(|g| g.triples_for_predicate(&report))
        .find_map(|t| match t.object {
            TermRef::NamedNode(n) => Some(n.into_owned()),
            _ => None,
        });
    let Some(graph) = graph else {
        return all.clone();
    };
    let shacl = "http://www.w3.org/ns/shacl#";
    let exists = |severity: &str| {
        format!(
            "EXISTS {{ GRAPH {graph} {{ ?r <{shacl}focusNode> $this ; <{shacl}resultSeverity> <{shacl}{severity}> }} }}"
        )
    };
    let select = format!(
        "BIND(IF({}, \"violation\", IF({}, \"warning\", IF({}, \"info\", \"ok\"))) AS ?value)",
        exists("Violation"),
        exists("Warning"),
        exists("Info")
    );
    let (property, key, target) = (sh("property"), tts("key"), sh("targetClass"));
    let mut out = all.clone();
    for node in node_shapes(all) {
        if all.object_for_subject_predicate(&node, &target).is_none() {
            continue;
        }
        let has = all
            .objects_for_subject_predicate(&node, &property)
            .filter_map(subject_of)
            .any(|p| {
                matches!(all.object_for_subject_predicate(p, &key),
                    Some(TermRef::Literal(l)) if l.value() == QUALITY_KEY)
            });
        if has {
            continue;
        }
        let p = oxigraph::model::BlankNode::default();
        out.insert(TripleRef::new(&node, &property, &p));
        // A path of its own, which no data has: the value is computed.
        let path = NamedNode::new_unchecked("urn:tripletrail:quality");
        out.insert(TripleRef::new(&p, &sh("path"), &path));
        let text = |v: &str| oxigraph::model::Literal::new_simple_literal(v);
        out.insert(TripleRef::new(&p, &key, &text(QUALITY_KEY)));
        out.insert(TripleRef::new(&p, &sh("name"), &text("Data quality")));
        out.insert(TripleRef::new(&p, &tts("facet"), &tts("Facet-Values")));
        out.insert(TripleRef::new(&p, &tts("onlyFor"), &Reader::Map.iri()));
        out.insert(TripleRef::new(&p, &tts("select"), &text(&select)));
    }
    out
}

/// `all` with the facets of parts rolled up to their parent (#248): a node shape with
/// `tts:rollsUpTo [ sh:path P ; sh:class C ]` gives each node shape that targets `C` its
/// facets (`tts:facet`, read through a path) that the parent lacks by key, read through the
/// inverse of `P`, for the map only. A parent then matches a filter on such a facet when one
/// of its parts matches, counts once per value of its parts, and is coloured by the highest of
/// its parts' numbers, as RINF Atlas rolls a section of line up from its running tracks. A
/// facet computed by `tts:select`, or hidden from the map, stays with the part.
pub fn add_rollups(all: &Graph) -> Graph {
    let rolls = tts("rollsUpTo");
    let ups: Vec<(NamedOrBlankNode, NamedOrBlankNode)> = all
        .triples_for_predicate(&rolls)
        .filter_map(|t| Some((t.subject.into_owned(), subject_of(t.object)?.into_owned())))
        .collect();
    if ups.is_empty() {
        return all.clone();
    }
    let (property, path, class, target, key) = (
        sh("property"),
        sh("path"),
        sh("class"),
        sh("targetClass"),
        tts("key"),
    );
    let map = Reader::Map.iri();
    let (hidden, only, facet, select) = (
        tts("hiddenFrom"),
        tts("onlyFor"),
        tts("facet"),
        tts("select"),
    );
    // A property shape's key, as the readers take it: `tts:key`, else its path's local name.
    let key_of = |p: NamedOrBlankNodeRef<'_>| -> Option<String> {
        match all.object_for_subject_predicate(p, &key) {
            Some(TermRef::Literal(l)) => Some(l.value().to_owned()),
            _ => match all.object_for_subject_predicate(p, &path)? {
                TermRef::NamedNode(n) => Some(super::local_name(n.as_str()).to_owned()),
                _ => None,
            },
        }
    };
    let for_map = |p: NamedOrBlankNodeRef<'_>| {
        !all.objects_for_subject_predicate(p, &hidden)
            .any(|r| r == map.as_ref().into())
            && all
                .objects_for_subject_predicate(p, &only)
                .all(|r| r == map.as_ref().into())
    };
    let mut out = all.clone();
    for (part, up) in ups {
        let (Some(up_path), Some(TermRef::NamedNode(parent))) = (
            all.object_for_subject_predicate(&up, &path),
            all.object_for_subject_predicate(&up, &class),
        ) else {
            continue;
        };
        let parents: Vec<NamedOrBlankNode> = all
            .subjects_for_predicate_object(&target, parent)
            .map(NamedOrBlankNodeRef::into_owned)
            .filter(|n| *n != part)
            .collect();
        let mut facets: Vec<(String, NamedOrBlankNodeRef<'_>)> = all
            .objects_for_subject_predicate(&part, &property)
            .filter_map(subject_of)
            .filter(|p| {
                all.object_for_subject_predicate(*p, &facet).is_some()
                    && all.object_for_subject_predicate(*p, &select).is_none()
                    && all.object_for_subject_predicate(*p, &path).is_some()
                    && for_map(*p)
            })
            .filter_map(|p| Some((key_of(p)?, p)))
            .collect();
        facets.sort_by(|a, b| a.0.cmp(&b.0));
        for node in parents {
            let mut has: HashSet<String> = out
                .objects_for_subject_predicate(&node, &property)
                .filter_map(subject_of)
                .filter_map(|p| match out.object_for_subject_predicate(p, &key) {
                    Some(TermRef::Literal(l)) => Some(l.value().to_owned()),
                    _ => match out.object_for_subject_predicate(p, &path)? {
                        TermRef::NamedNode(n) => Some(super::local_name(n.as_str()).to_owned()),
                        _ => None,
                    },
                })
                .collect();
            for (k, p) in &facets {
                if !has.insert(k.clone()) {
                    continue;
                }
                let Some(part_path) = all.object_for_subject_predicate(*p, &path) else {
                    continue;
                };
                let rolled = oxigraph::model::BlankNode::default();
                out.insert(TripleRef::new(&node, &property, &rolled));
                // ( [ sh:inversePath P ] part_path ): from the parent to its parts' values.
                let inverse = oxigraph::model::BlankNode::default();
                out.insert(TripleRef::new(&inverse, &sh("inversePath"), up_path));
                let (first, second) = (
                    oxigraph::model::BlankNode::default(),
                    oxigraph::model::BlankNode::default(),
                );
                out.insert(TripleRef::new(&first, rdf::FIRST, &inverse));
                out.insert(TripleRef::new(&first, rdf::REST, &second));
                out.insert(TripleRef::new(&second, rdf::FIRST, part_path));
                out.insert(TripleRef::new(&second, rdf::REST, rdf::NIL));
                out.insert(TripleRef::new(&rolled, &path, &first));
                let text = oxigraph::model::Literal::new_simple_literal;
                out.insert(TripleRef::new(&rolled, &key, &text(k.as_str())));
                out.insert(TripleRef::new(&rolled, &tts("onlyFor"), &map));
                out.insert(TripleRef::new(&rolled, &tts("rolledUpFrom"), &part));
                for copied in [
                    facet.clone(),
                    sh("name"),
                    sh("datatype"),
                    sh("order"),
                    tts("colorClasses"),
                    NamedNode::new_unchecked("http://qudt.org/schema/qudt/hasUnit"),
                ] {
                    if let Some(v) = all.object_for_subject_predicate(*p, &copied) {
                        out.insert(TripleRef::new(&rolled, &copied, v));
                    }
                }
            }
        }
    }
    out
}

/// How a node shape of a lower set is met by an upper set (see [`overlay`]).
struct Meeting {
    /// The lower node shape.
    node: NamedOrBlankNode,
    /// Its target classes.
    classes: Vec<NamedNode>,
    /// The upper set says something about its IRI.
    by_iri: bool,
    /// Its every class is the target of a reader shape of the upper set.
    by_class: bool,
    /// The upper shape that refines it (`tts:refines`).
    refiner: Option<NamedOrBlankNode>,
    /// With `by_class`, the upper shapes for its classes.
    replacing: Vec<NamedOrBlankNode>,
}

fn meetings(lower: &Graph, upper: &Graph) -> Vec<Meeting> {
    let (target, only) = (sh("targetClass"), tts("onlyFor"));
    let refines = tts("refines");
    let is_refining = |node: NamedOrBlankNodeRef<'_>| {
        upper
            .object_for_subject_predicate(node, &refines)
            .is_some_and(|v| matches!(v, TermRef::Literal(l) if l.value() == "true"))
    };
    // The classes `upper` has a shape for, and those shapes. A refining shape (`tts:refines`)
    // replaces none: it changes the lower shape of its IRI, or else those of its classes.
    let mut targeted: HashMap<NamedNode, Vec<NamedOrBlankNode>> = HashMap::new();
    let mut refined: HashMap<NamedNode, NamedOrBlankNode> = HashMap::new();
    for node in node_shapes(upper) {
        if is_refining(node.as_ref()) {
            if lower.triples_for_subject(&node).next().is_none() {
                for class in upper.objects_for_subject_predicate(&node, &target) {
                    if let TermRef::NamedNode(class) = class {
                        refined
                            .entry(class.into_owned())
                            .or_insert_with(|| node.clone());
                    }
                }
            }
            continue;
        }
        // Only a shape written for the readers: one with an `tts:` annotation, and for all of
        // them. A plain SHACL constraint shape for a class adds to its generated shape.
        let for_readers = upper
            .triples_for_subject(&node)
            .any(|t| t.predicate.as_str().starts_with(TTS));
        if !for_readers || upper.object_for_subject_predicate(&node, &only).is_some() {
            continue;
        }
        for class in upper.objects_for_subject_predicate(&node, &target) {
            if let TermRef::NamedNode(class) = class {
                targeted
                    .entry(class.into_owned())
                    .or_default()
                    .push(node.clone());
            }
        }
    }
    let mut out = Vec::new();
    for node in node_shapes(lower) {
        if !matches!(node, NamedOrBlankNode::NamedNode(_)) {
            continue;
        }
        let classes: Vec<NamedNode> = lower
            .objects_for_subject_predicate(&node, &target)
            .filter_map(|c| match c {
                TermRef::NamedNode(c) => Some(c.into_owned()),
                _ => None,
            })
            .collect();
        let by_iri = upper.triples_for_subject(&node).next().is_some();
        let by_class = !classes.is_empty() && classes.iter().all(|c| targeted.contains_key(c));
        // The shape that refines this one: of its IRI, or else one for its class that has no
        // lower shape of its own IRI.
        let refiner = if by_iri {
            is_refining(node.as_ref()).then(|| node.clone())
        } else if by_class {
            None
        } else {
            classes.iter().find_map(|c| refined.get(c).cloned())
        };
        if !(by_iri || by_class || refiner.is_some()) {
            continue;
        }
        let mut replacing: Vec<NamedOrBlankNode> = Vec::new();
        if by_class {
            for c in &classes {
                for heir in &targeted[c] {
                    if !replacing.contains(heir) {
                        replacing.push(heir.clone());
                    }
                }
            }
        }
        out.push(Meeting {
            node,
            classes,
            by_iri,
            by_class,
            refiner,
            replacing,
        });
    }
    out
}

/// `upper` on top of `lower`: a node shape of `lower` that `upper` says anything about is
/// replaced, with its property shapes, by `upper`'s, and so is one whose every target class
/// is the target of a node shape of `upper` written for the readers (with an `tts:`
/// annotation, and no `tts:onlyFor`), whatever its IRI; the rest of `lower` stays. A replacing shape without `tts:instances` takes the
/// replaced one's, so that readers still know how many instances the class has.
///
/// A node shape of `upper` with `tts:refines true` changes the node shape of the same IRI in
/// `lower` instead: its settings replace those `lower` gives (by predicate), its property
/// shapes replace those of `lower` with the same `sh:path`, and the rest of `lower`'s shape
/// stays, so properties added to the data later still show.
pub fn overlay(lower: &Graph, upper: &Graph) -> Graph {
    let (property, instances) = (sh("property"), tts("instances"));
    let path = sh("path");
    let mut dropped: HashSet<NamedOrBlankNode> = HashSet::new();
    let mut counts = Vec::new();
    // What a refining shape keeps of a shape below: (the refining shape, the lower shape's
    // (predicate, object) pairs, its property shapes).
    #[allow(clippy::type_complexity)]
    let mut kept: Vec<(
        NamedOrBlankNode,
        Vec<(NamedNode, oxigraph::model::Term)>,
        Vec<NamedOrBlankNode>,
    )> = Vec::new();
    for Meeting {
        node,
        classes,
        by_iri,
        by_class,
        refiner,
        replacing,
    } in meetings(lower, upper)
    {
        if let Some(n) = lower.object_for_subject_predicate(&node, &instances) {
            let mut heirs: Vec<NamedOrBlankNode> = Vec::new();
            if by_iri {
                heirs.push(node.clone());
            }
            if by_class && classes.len() == 1 {
                heirs.extend(replacing.iter().cloned());
            }
            if let Some(r) = &refiner
                && !by_iri
            {
                heirs.push(r.clone());
            }
            for heir in heirs {
                counts.push((heir, n.into_owned()));
            }
        }
        if let Some(r) = refiner {
            let set: HashSet<NamedNode> = upper
                .triples_for_subject(&r)
                .map(|t| t.predicate.into_owned())
                .collect();
            let paths: HashSet<oxigraph::model::Term> = upper
                .objects_for_subject_predicate(&r, &property)
                .filter_map(subject_of)
                .filter_map(|p| upper.object_for_subject_predicate(p, &path))
                .map(TermRef::into_owned)
                .collect();
            let triples = lower
                .triples_for_subject(&node)
                .filter(|t| {
                    // The count goes to its heir as for a replaced shape.
                    t.predicate != property.as_ref()
                        && t.predicate != instances.as_ref()
                        && !set.contains(&t.predicate.into_owned())
                })
                .map(|t| (t.predicate.into_owned(), t.object.into_owned()))
                .collect();
            let properties = lower
                .objects_for_subject_predicate(&node, &property)
                .filter_map(subject_of)
                .filter(|p| {
                    upper.triples_for_subject(*p).next().is_none()
                        && lower
                            .object_for_subject_predicate(*p, &path)
                            .is_none_or(|q| !paths.contains(&q.into_owned()))
                })
                .map(NamedOrBlankNodeRef::into_owned)
                .collect();
            kept.push((r, triples, properties));
        }
        for p in lower.objects_for_subject_predicate(&node, &property) {
            if let Some(p) = subject_of(p) {
                dropped.insert(p.into_owned());
            }
        }
        dropped.insert(node);
    }
    let mut out = upper.clone();
    for (heir, n) in counts {
        if out
            .object_for_subject_predicate(&heir, &instances)
            .is_none()
        {
            out.insert(TripleRef::new(&heir, &instances, &n));
        }
    }
    let mut copied = HashSet::new();
    for (node, triples, properties) in kept {
        for (p, o) in &triples {
            out.insert(TripleRef::new(&node, p, o));
            if let TermRef::BlankNode(b) = o.as_ref() {
                copy_all(lower, &mut out, b.into(), &mut copied);
            }
        }
        for p in properties {
            out.insert(TripleRef::new(&node, &property, &p));
            copy_all(lower, &mut out, p.as_ref(), &mut copied);
        }
    }
    for t in lower.iter() {
        if matches!(t.subject, NamedOrBlankNodeRef::NamedNode(_))
            && !dropped.contains(&t.subject.into_owned())
        {
            copy_all(lower, &mut out, t.subject, &mut copied);
        }
    }
    out
}

/// How a shape of a higher set meets a pack shape: by its IRI (`iri`), by refining it
/// (`refines`, by IRI or by class), or by replacing it for its classes (`class`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Override {
    /// The shape of the higher set, as in Turtle (`<iri>`, or "a blank node").
    pub shape: String,
    /// The pack's node shape it meets.
    pub pack_shape: NamedNode,
    pub how: &'static str,
}

/// What installing a pack release would do (#148): see [`upgrade`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Upgrade {
    /// Named node shapes of the new release that the installed one does not have.
    pub added: Vec<NamedNode>,
    /// Named node shapes of the installed release that the new one drops.
    pub removed: Vec<NamedNode>,
    /// Named node shapes of both whose description differs.
    pub changed: Vec<NamedNode>,
    /// The higher set's shapes that meet a shape of the new release, and so go on changing it.
    pub overrides: Vec<Override>,
    /// The higher set's shapes that met a shape of the installed release and meet none of the
    /// new one: after the upgrade they stand alone (`pack_shape` is the installed one's).
    pub orphaned: Vec<Override>,
    /// Node shapes of the new release with a target class that has no instances in the data:
    /// (shape, class).
    pub no_data: Vec<(NamedNode, NamedNode)>,
}

fn overrides(pack: &Graph, own: &Graph) -> Vec<Override> {
    let mut out = Vec::new();
    for m in meetings(pack, own) {
        let NamedOrBlankNode::NamedNode(pack_shape) = &m.node else {
            continue;
        };
        let mut push = |shape: &NamedOrBlankNode, how: &'static str| {
            let o = Override {
                shape: name(shape.as_ref()),
                pack_shape: pack_shape.clone(),
                how,
            };
            if !out.contains(&o) {
                out.push(o);
            }
        };
        match &m.refiner {
            Some(r) => push(r, "refines"),
            None if m.by_iri => push(&m.node, "iri"),
            None => {}
        }
        if m.by_class && !m.by_iri {
            for heir in &m.replacing {
                push(heir, "class");
            }
        }
    }
    out.sort_by(|a, b| (&a.shape, a.pack_shape.as_str()).cmp(&(&b.shape, b.pack_shape.as_str())));
    out
}

/// The named node shapes of a set, with their descriptions (the shape's triples and those of
/// the blank nodes it reaches), canonical so that two can be compared.
fn descriptions(set: &Graph) -> HashMap<NamedNode, Graph> {
    node_shapes(set)
        .into_iter()
        .filter_map(|node| match node {
            NamedOrBlankNode::NamedNode(n) => {
                let mut description = Graph::new();
                copy_all(
                    set,
                    &mut description,
                    n.as_ref().into(),
                    &mut HashSet::new(),
                );
                // The properties too when they are named: they belong to the shape.
                for p in set.objects_for_subject_predicate(&n, &sh("property")) {
                    if let TermRef::NamedNode(p) = p {
                        copy_all(set, &mut description, p.into(), &mut HashSet::new());
                    }
                }
                description.canonicalize(CanonicalizationAlgorithm::Unstable);
                Some((n, description))
            }
            NamedOrBlankNode::BlankNode(_) => None,
        })
        .collect()
}

/// What installing the pack release `pack` would do to a dataset that has `installed` (none
/// for a first install) under its own set `own`: the shapes the release adds, drops and
/// changes, the own shapes that go on changing a pack shape and those that would stand alone,
/// and the release's shapes for classes of which `has_instances` finds none in the data.
/// Nothing is kept; the sets are as [`load`] gives them.
pub fn upgrade(
    installed: Option<&Graph>,
    pack: &Graph,
    own: &Graph,
    has_instances: &dyn Fn(&NamedNode) -> bool,
) -> Upgrade {
    let new = descriptions(pack);
    let old = installed.map(descriptions).unwrap_or_default();
    let sorted = |mut v: Vec<NamedNode>| {
        v.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        v
    };
    let added = sorted(
        new.keys()
            .filter(|n| !old.contains_key(*n))
            .cloned()
            .collect(),
    );
    let removed = sorted(
        old.keys()
            .filter(|n| !new.contains_key(*n))
            .cloned()
            .collect(),
    );
    let changed = sorted(
        new.iter()
            .filter(|(n, d)| old.get(*n).is_some_and(|o| o != *d))
            .map(|(n, _)| n.clone())
            .collect(),
    );
    let overrides = overrides(pack, own);
    let orphaned = match installed {
        Some(installed) => self::overrides(installed, own)
            .into_iter()
            .filter(|o| !overrides.iter().any(|n| n.shape == o.shape))
            .collect(),
        None => Vec::new(),
    };
    let target = sh("targetClass");
    let mut no_data = Vec::new();
    let mut asked: HashMap<NamedNode, bool> = HashMap::new();
    for node in new.keys() {
        for class in pack.objects_for_subject_predicate(node, &target) {
            if let TermRef::NamedNode(class) = class {
                let class = class.into_owned();
                let found = *asked
                    .entry(class.clone())
                    .or_insert_with(|| has_instances(&class));
                if !found {
                    no_data.push((node.clone(), class));
                }
            }
        }
    }
    no_data.sort_by(|a, b| (a.0.as_str(), a.1.as_str()).cmp(&(b.0.as_str(), b.1.as_str())));
    Upgrade {
        added,
        removed,
        changed,
        overrides,
        orphaned,
        no_data,
    }
}

/// The `tts:` terms the vocabulary defines.
fn defined() -> &'static HashSet<String> {
    static DEFINED: OnceLock<HashSet<String>> = OnceLock::new();
    DEFINED.get_or_init(|| {
        RdfParser::from_format(RdfFormat::Turtle)
            .for_slice(VOCABULARY.as_bytes())
            .filter_map(Result::ok)
            .filter_map(|q| match q.subject {
                NamedOrBlankNode::NamedNode(n) if n.as_str().starts_with(TTS) => {
                    Some(n.into_string())
                }
                _ => None,
            })
            .collect()
    })
}

fn name(node: NamedOrBlankNodeRef<'_>) -> String {
    match node {
        NamedOrBlankNodeRef::NamedNode(n) => format!("<{}>", n.as_str()),
        NamedOrBlankNodeRef::BlankNode(_) => "a blank node".to_owned(),
    }
}

/// A term as in Turtle, short.
fn short(term: TermRef<'_>) -> String {
    match term {
        TermRef::NamedNode(n) => match n.as_str().strip_prefix(TTS) {
            Some(local) => format!("tts:{local}"),
            None => format!("<{}>", n.as_str()),
        },
        TermRef::Literal(l) if l.datatype() == xsd::INTEGER || l.datatype() == xsd::BOOLEAN => {
            l.value().to_owned()
        }
        TermRef::Literal(l) => format!("{:?}", l.value()),
        other => other.to_string(),
    }
}

/// What is wrong with a shape set, if anything: `tts:` terms the vocabulary does not define,
/// readers that are none, malformed colours and zooms, keys used twice, property shapes
/// without exactly one `sh:path`, targets that are not IRIs, `tts:select` patterns that do
/// not parse, bind no `?value` or call a service other than TripleTrail's own.
pub fn check(graph: &Graph) -> Vec<String> {
    let mut errors = Vec::new();
    let defined = defined();
    let readers: HashSet<String> = Reader::ALL.iter().map(|r| r.iri().into_string()).collect();
    let select = tts("select");
    let hides = tts("hidesGenerated");
    let rolls = tts("rollsUpTo");
    let (hidden, only, colour, zoom, target, path) = (
        tts("hiddenFrom"),
        tts("onlyFor"),
        tts("color"),
        tts("minZoom"),
        sh("targetClass"),
        sh("path"),
    );
    for t in graph.iter() {
        let p = t.predicate.as_str();
        if p.starts_with(TTS) && !defined.contains(p) {
            errors.push(format!(
                "{}: tts:{} is not in the vocabulary",
                name(t.subject),
                &p[TTS.len()..]
            ));
        }
        if let TermRef::NamedNode(o) = t.object
            && o.as_str().starts_with(TTS)
            && !defined.contains(o.as_str())
        {
            errors.push(format!(
                "{}: tts:{} is not in the vocabulary",
                name(t.subject),
                &o.as_str()[TTS.len()..]
            ));
        }
        if (t.predicate == hidden.as_ref()
            || t.predicate == only.as_ref()
            || t.predicate == hides.as_ref())
            && !matches!(t.object, TermRef::NamedNode(o) if readers.contains(o.as_str()))
        {
            errors.push(format!(
                "{}: {} is not a reader",
                name(t.subject),
                short(t.object)
            ));
        }
        if t.predicate == colour.as_ref() {
            let ok = matches!(t.object, TermRef::Literal(l)
                if l.value().len() == 7
                    && l.value().starts_with('#')
                    && l.value()[1..].chars().all(|c| c.is_ascii_hexdigit()));
            if !ok {
                errors.push(format!(
                    "{}: tts:color {} is not #rrggbb",
                    name(t.subject),
                    short(t.object)
                ));
            }
        }
        if t.predicate == zoom.as_ref() {
            let ok = matches!(t.object, TermRef::Literal(l)
                if l.datatype() == xsd::INTEGER
                    && l.value().parse::<u8>().is_ok_and(|z| z <= 24));
            if !ok {
                errors.push(format!(
                    "{}: tts:minZoom {} is not an integer from 0 to 24",
                    name(t.subject),
                    short(t.object)
                ));
            }
        }
        if t.predicate == select.as_ref() {
            let problem = match t.object {
                TermRef::Literal(l) => check_select(l.value()).err(),
                other => Some(format!("{} is not a string", short(other))),
            };
            if let Some(problem) = problem {
                // A property shape is mostly a blank node: named by its node shape and key.
                let owner = graph
                    .subjects_for_predicate_object(&sh("property"), t.subject)
                    .next();
                let key = match graph.object_for_subject_predicate(t.subject, &tts("key")) {
                    Some(TermRef::Literal(k)) => format!(" \"{}\"", k.value()),
                    _ => String::new(),
                };
                errors.push(match owner {
                    Some(node) => format!("{}: the facet{key}'s tts:select {problem}", name(node)),
                    None => format!("{}: tts:select {problem}", name(t.subject)),
                });
            }
        }
        if t.predicate == rolls.as_ref() {
            let ok = subject_of(t.object).is_some_and(|up| {
                graph.objects_for_subject_predicate(up, &path).count() == 1
                    && matches!(
                        graph.object_for_subject_predicate(up, &sh("class")),
                        Some(TermRef::NamedNode(_))
                    )
            });
            if !ok {
                errors.push(format!(
                    "{}: tts:rollsUpTo {} is not a node with one sh:path and an sh:class IRI",
                    name(t.subject),
                    short(t.object)
                ));
            }
        }
        if t.predicate == target.as_ref() && !matches!(t.object, TermRef::NamedNode(_)) {
            errors.push(format!(
                "{}: sh:targetClass {} is not an IRI",
                name(t.subject),
                short(t.object)
            ));
        }
    }
    let key = tts("key");
    let key_of = |s: NamedOrBlankNodeRef<'_>| -> Option<String> {
        graph
            .object_for_subject_predicate(s, &key)
            .map(|k| match k {
                TermRef::Literal(l) => l.value().to_owned(),
                other => other.to_string(),
            })
    };
    let mut node_keys: HashMap<String, String> = HashMap::new();
    let property = sh("property");
    let viewer = NamedNode::new_unchecked("http://datashapes.org/dash#viewer");
    for node in node_shapes(graph) {
        if let Some(k) = key_of(node.as_ref())
            && let Some(other) = node_keys.insert(k.clone(), name(node.as_ref()))
        {
            errors.push(format!(
                "{}: tts:key \"{k}\" is also the key of {other}",
                name(node.as_ref())
            ));
        }
        let mut property_keys = HashSet::new();
        for p in graph.objects_for_subject_predicate(&node, &property) {
            let Some(p) = subject_of(p) else {
                errors.push(format!(
                    "{}: sh:property {} is not a shape",
                    name(node.as_ref()),
                    short(p)
                ));
                continue;
            };
            let paths = graph.objects_for_subject_predicate(p, &path).count();
            // Without a path: a row that only turns off another's key, or a widget that
            // reads no path of its own (rinf-describe's shapes have both).
            let pathless = paths == 0
                && (graph
                    .object_for_subject_predicate(p, &sh("deactivated"))
                    .is_some()
                    || graph.object_for_subject_predicate(p, &viewer).is_some());
            if paths != 1 && !pathless {
                errors.push(format!(
                    "{}: a property shape has {paths} sh:path, not one",
                    name(node.as_ref())
                ));
            }
            if let Some(k) = key_of(p)
                && !property_keys.insert(k.clone())
            {
                errors.push(format!(
                    "{}: two property shapes have tts:key \"{k}\"",
                    name(node.as_ref())
                ));
            }
        }
    }
    // A label pattern that is no regular expression would strip nothing, quietly (#260).
    for t in graph.triples_for_predicate(&tts("labelStrip")) {
        match t.object {
            TermRef::Literal(l) => {
                if let Err(e) = regex::Regex::new(l.value()) {
                    errors.push(format!(
                        "{}: tts:labelStrip is not a regular expression: {e}",
                        name(t.subject)
                    ));
                }
            }
            _ => errors.push(format!(
                "{}: tts:labelStrip is not a string",
                name(t.subject)
            )),
        }
    }
    errors.sort();
    errors.dedup();
    errors
}

/// A computed facet's pattern (`tts:select`) as the readers inline it after `?f a <class> .`,
/// where `?f` is bound (so `OPTIONAL` in it keeps the features without a value): `$this` (or
/// `?this`) as `f`, `?value` as `v` and every other variable `?x` as `?x_` followed by `v`'s
/// name, so that it meets no variable outside it. IRIs, strings and comments are left as
/// they are. An error if its braces do not balance, so that it cannot close the query's
/// group early.
pub fn inline_select(pattern: &str, f: &str, v: &str) -> Result<String, String> {
    let suffix = v.trim_start_matches(['?', '$']);
    let mut out = String::with_capacity(pattern.len() + 16);
    let mut chars = pattern.chars().peekable();
    let mut last: Option<char> = None;
    let mut depth = 0usize;
    while let Some(c) = chars.next() {
        match c {
            // An IRI, to its end, unless it is a comparison (`?a < ?b`, `?s<200`): after a
            // term (a name, a number, a closing parenthesis) or holding a space, it is one.
            '<' if !last.is_some_and(|l| l.is_alphanumeric() || "_)\"'".contains(l)) => {
                let rest: String = chars
                    .clone()
                    .take_while(|&d| d != '>' && d != '\n')
                    .collect();
                let iri = !rest.starts_with('=')
                    && !rest.contains(|d: char| d.is_whitespace() || "<\"{}|^`\\".contains(d))
                    && chars.clone().nth(rest.chars().count()) == Some('>');
                out.push('<');
                if iri {
                    for _ in 0..=rest.chars().count() {
                        out.push(chars.next().expect("counted"));
                    }
                    last = Some('>');
                    continue;
                }
            }
            '"' | '\'' => {
                out.push(c);
                let mut escaped = false;
                for d in chars.by_ref() {
                    out.push(d);
                    if escaped {
                        escaped = false;
                    } else if d == '\\' {
                        escaped = true;
                    } else if d == c {
                        break;
                    }
                }
            }
            '#' => {
                for d in chars.by_ref() {
                    if d == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '{' => {
                depth += 1;
                out.push(c);
            }
            '}' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or("closes a brace it did not open")?;
                out.push(c);
            }
            '?' | '$'
                if chars
                    .peek()
                    .is_some_and(|d| d.is_alphanumeric() || *d == '_') =>
            {
                let mut name = String::new();
                while let Some(&d) = chars.peek() {
                    if d.is_alphanumeric() || d == '_' {
                        name.push(d);
                        chars.next();
                    } else {
                        break;
                    }
                }
                match name.as_str() {
                    "this" => out.push_str(f),
                    "value" => out.push_str(v),
                    _ => {
                        out.push('?');
                        out.push_str(&name);
                        out.push('_');
                        out.push_str(suffix);
                    }
                }
                last = Some('a');
                continue;
            }
            _ => out.push(c),
        }
        last = Some(c);
    }
    if depth > 0 {
        return Err("opens a brace it does not close".to_owned());
    }
    Ok(format!(
        " {} .",
        out.trim().trim_end_matches('.').trim_end()
    ))
}

/// What is wrong with a computed facet's pattern (`tts:select`), if anything: inlined as the
/// readers do ([`inline_select`]), it must parse, bind `?value`, and call no service but
/// TripleTrail's own, which read the store (a map tile must not wait on another endpoint).
pub fn check_select(pattern: &str) -> Result<(), String> {
    let inlined = inline_select(pattern, "?f", "?v")?;
    let query = spargebra::SparqlParser::new()
        .parse_query(&format!(
            "SELECT * WHERE {{ ?f a <urn:tripletrail:class> .{inlined}\n}}"
        ))
        .map_err(|e| format!("does not parse: {e}"))?;
    let spargebra::Query::Select { pattern, .. } = query else {
        unreachable!("a SELECT query");
    };
    let mut scope = HashSet::new();
    pattern.on_in_scope_variable(|v| {
        scope.insert(v.as_str().to_owned());
    });
    if !scope.contains("v") {
        return Err("binds no ?value".to_owned());
    }
    let mut services = Vec::new();
    select_services(&pattern, &mut services);
    match services
        .into_iter()
        .find(|s| !BUILT_IN_SERVICES.contains(&s.trim_start_matches('<').trim_end_matches('>')))
    {
        Some(other) => Err(format!(
            "calls SERVICE {other}, not one of TripleTrail's own"
        )),
        None => Ok(()),
    }
}

/// The names of the services a pattern calls, in its sub-patterns and `EXISTS` too.
fn select_services(p: &GraphPattern, out: &mut Vec<String>) {
    use GraphPattern as G;
    match p {
        G::Bgp { .. } | G::Path { .. } | G::Values { .. } => {}
        G::Join { left, right }
        | G::Lateral { left, right }
        | G::Union { left, right }
        | G::Minus { left, right } => {
            select_services(left, out);
            select_services(right, out);
        }
        G::LeftJoin {
            left,
            right,
            expression,
        } => {
            select_services(left, out);
            select_services(right, out);
            if let Some(e) = expression {
                expression_services(e, out);
            }
        }
        G::Filter { expr, inner } => {
            expression_services(expr, out);
            select_services(inner, out);
        }
        G::Extend {
            inner, expression, ..
        } => {
            expression_services(expression, out);
            select_services(inner, out);
        }
        G::OrderBy { inner, expression } => {
            for e in expression {
                match e {
                    OrderExpression::Asc(e) | OrderExpression::Desc(e) => {
                        expression_services(e, out)
                    }
                }
            }
            select_services(inner, out);
        }
        G::Group {
            inner, aggregates, ..
        } => {
            for (_, a) in aggregates {
                if let AggregateExpression::FunctionCall { expr, .. } = a {
                    expression_services(expr, out);
                }
            }
            select_services(inner, out);
        }
        G::Graph { inner, .. }
        | G::Project { inner, .. }
        | G::Distinct { inner }
        | G::Reduced { inner }
        | G::Slice { inner, .. } => select_services(inner, out),
        G::Service { name, inner, .. } => {
            out.push(name.to_string());
            select_services(inner, out);
        }
    }
}

fn expression_services(e: &Expression, out: &mut Vec<String>) {
    use Expression as E;
    match e {
        E::Exists(p) => select_services(p, out),
        E::NamedNode(_) | E::Literal(_) | E::Variable(_) | E::Bound(_) => {}
        E::Or(a, b)
        | E::And(a, b)
        | E::Equal(a, b)
        | E::SameTerm(a, b)
        | E::Greater(a, b)
        | E::GreaterOrEqual(a, b)
        | E::Less(a, b)
        | E::LessOrEqual(a, b)
        | E::Add(a, b)
        | E::Subtract(a, b)
        | E::Multiply(a, b)
        | E::Divide(a, b) => {
            expression_services(a, out);
            expression_services(b, out);
        }
        E::UnaryPlus(a) | E::UnaryMinus(a) | E::Not(a) => expression_services(a, out),
        E::In(a, list) => {
            expression_services(a, out);
            list.iter().for_each(|e| expression_services(e, out));
        }
        E::If(a, b, c) => {
            expression_services(a, out);
            expression_services(b, out);
            expression_services(c, out);
        }
        E::Coalesce(list) | E::FunctionCall(_, list) => {
            list.iter().for_each(|e| expression_services(e, out))
        }
    }
}

/// A shape set from its files, checked: the errors otherwise, each naming its file or shape.
pub fn load(files: &[SetFile]) -> Result<Graph, Vec<String>> {
    let mut graph = Graph::new();
    let mut errors = Vec::new();
    for file in files {
        // Blank node labels are each file's own.
        let parser = RdfParser::from_format(RdfFormat::Turtle).rename_blank_nodes();
        for quad in parser.for_slice(file.text.as_bytes()) {
            match quad {
                Ok(q) => {
                    graph.insert(&q.into());
                }
                Err(e) => {
                    errors.push(format!("{}: {e}", file.name));
                    break;
                }
            }
        }
    }
    errors.extend(check(&graph));
    if errors.is_empty() {
        Ok(graph)
    } else {
        Err(errors)
    }
}

/// A file name from a key: letters, digits, `_` and `-`.
fn file_name(key: &str, taken: &mut HashSet<String>) -> String {
    let base: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let base = if base.is_empty() {
        "shape".to_owned()
    } else {
        base
    };
    let mut name = format!("{base}.ttl");
    let mut n = 2;
    while !taken.insert(name.to_ascii_lowercase()) {
        name = format!("{base}_{n}.ttl");
        n += 1;
    }
    name
}

/// A shape set as files: one per named node shape (named by its `tts:key`), with its
/// property shapes, and `_set.ttl` with the rest (the class hierarchy, the set itself).
pub fn files_of(graph: &Graph) -> Vec<SetFile> {
    let property = sh("property");
    let key = tts("key");
    let mut taken = HashSet::new();
    taken.insert("_set.ttl".to_owned());
    let mut placed: HashSet<NamedOrBlankNode> = HashSet::new();
    let mut files = Vec::new();
    let mut nodes: Vec<NamedOrBlankNode> = node_shapes(graph)
        .into_iter()
        .filter(|n| matches!(n, NamedOrBlankNode::NamedNode(_)))
        .collect();
    nodes.sort_by_key(ToString::to_string);
    for node in nodes {
        let mut part = Graph::new();
        copy_all(graph, &mut part, node.as_ref(), &mut placed);
        for p in graph.objects_for_subject_predicate(&node, &property) {
            if let Some(p) = subject_of(p) {
                copy_all(graph, &mut part, p, &mut placed);
            }
        }
        let stem = match graph.object_for_subject_predicate(&node, &key) {
            Some(TermRef::Literal(l)) => l.value().to_owned(),
            _ => match &node {
                NamedOrBlankNode::NamedNode(n) => n
                    .as_str()
                    .rsplit(['#', '/', ':'])
                    .next()
                    .unwrap_or("shape")
                    .to_owned(),
                NamedOrBlankNode::BlankNode(_) => "shape".to_owned(),
            },
        };
        files.push(SetFile {
            name: file_name(&stem, &mut taken),
            text: to_turtle(&part),
        });
    }
    // Each map view in a file of its own, `view-{key}`.
    let mut views: Vec<NamedOrBlankNode> = graph
        .subjects_for_predicate_object(rdf::TYPE, &tts("MapView"))
        .filter(|v| matches!(v, NamedOrBlankNodeRef::NamedNode(_)))
        .map(NamedOrBlankNodeRef::into_owned)
        .collect();
    views.sort_by_key(ToString::to_string);
    for view in views {
        if placed.contains(&view) {
            continue;
        }
        let mut part = Graph::new();
        copy_all(graph, &mut part, view.as_ref(), &mut placed);
        let stem = match graph.object_for_subject_predicate(&view, &key) {
            Some(TermRef::Literal(l)) => l.value().to_owned(),
            _ => match &view {
                NamedOrBlankNode::NamedNode(n) => n
                    .as_str()
                    .rsplit(['#', '/', ':'])
                    .next()
                    .unwrap_or("view")
                    .to_owned(),
                NamedOrBlankNode::BlankNode(_) => "view".to_owned(),
            },
        };
        files.push(SetFile {
            name: file_name(&format!("view-{stem}"), &mut taken),
            text: to_turtle(&part),
        });
    }
    let mut rest = Graph::new();
    for t in graph.iter() {
        if matches!(t.subject, NamedOrBlankNodeRef::NamedNode(_))
            && !placed.contains(&t.subject.into_owned())
        {
            copy_all(graph, &mut rest, t.subject, &mut placed);
        }
    }
    if !rest.is_empty() {
        files.push(SetFile {
            name: "_set.ttl".to_owned(),
            text: to_turtle(&rest),
        });
    }
    files
}
