//! RDFS entailment on term ids (#187): the triples the RDFS rules derive from a store,
//! materialised into a graph of their own.
//!
//! The rules are those of Jena's default RDFS reasoner, which Fuseki's RDFS datasets use:
//!
//! | rule | if | then |
//! |---|---|---|
//! | rdfs2 | `p rdfs:domain c`, `x p y` | `x rdf:type c` |
//! | rdfs3 | `p rdfs:range c`, `x p y`, `y` not a literal | `y rdf:type c` |
//! | rdfs5 | `p rdfs:subPropertyOf q`, `q rdfs:subPropertyOf r` | `p rdfs:subPropertyOf r` |
//! | rdfs7 | `p rdfs:subPropertyOf q`, `x p y` | `x q y` |
//! | rdfs9 | `c rdfs:subClassOf d`, `x rdf:type c` | `x rdf:type d` |
//! | rdfs11 | `c rdfs:subClassOf d`, `d rdfs:subClassOf e` | `c rdfs:subClassOf e` |
//!
//! Left out, as in Jena's default level: the axiomatic triples, `x rdf:type rdfs:Resource`
//! (rdfs4), and the reflexive `rdfs:subClassOf` / `rdfs:subPropertyOf` (rdfs6, 8, 10, 12, 13).
//!
//! [`Regime::OwlRl`] adds a first part of OWL 2 RL: equivalent classes and properties (as sub
//! classes and properties both ways), inverse and symmetric properties (in the plans: `x p y`
//! gives `y q x`), and transitive properties (the closure of their triples, each round).
//!
//! The rules run to their fixpoint. The schema (`rdfs:subClassOf`, `rdfs:subPropertyOf`,
//! `rdfs:domain`, `rdfs:range`) is compiled first into a plan per property: its super
//! properties and the classes its subjects and objects get. Each asserted triple of a planned
//! property, and each type, is read once, on every core. A plan already holds all its
//! property's consequences, so what it derives needs no second look, except triples of
//! rdf:type or of a schema property that has a plan itself (as `rdf:type rdfs:domain c` or the
//! axioms of `rdfs.ttl` give): those are fed back to the plans until nothing new comes. A
//! derived schema triple that changes the schema (a sub property of `rdfs:subClassOf`, say)
//! compiles the plans again and reads all triples again.

use crate::{GraphSelector, Kind, Snapshot, TermId};
use oxigraph::model::NamedNodeRef;
use oxigraph::model::vocab::{rdf, rdfs};

/// The OWL terms the OWL 2 RL rules read.
mod owl {
    use oxigraph::model::NamedNodeRef;
    const fn n(iri: &'static str) -> NamedNodeRef<'static> {
        NamedNodeRef::new_unchecked(iri)
    }
    pub const EQUIVALENT_CLASS: NamedNodeRef<'_> =
        n("http://www.w3.org/2002/07/owl#equivalentClass");
    pub const EQUIVALENT_PROPERTY: NamedNodeRef<'_> =
        n("http://www.w3.org/2002/07/owl#equivalentProperty");
    pub const INVERSE_OF: NamedNodeRef<'_> = n("http://www.w3.org/2002/07/owl#inverseOf");
    pub const SYMMETRIC_PROPERTY: NamedNodeRef<'_> =
        n("http://www.w3.org/2002/07/owl#SymmetricProperty");
    pub const TRANSITIVE_PROPERTY: NamedNodeRef<'_> =
        n("http://www.w3.org/2002/07/owl#TransitiveProperty");
    pub const ON_PROPERTY: NamedNodeRef<'_> = n("http://www.w3.org/2002/07/owl#onProperty");
    pub const HAS_VALUE: NamedNodeRef<'_> = n("http://www.w3.org/2002/07/owl#hasValue");
    pub const SOME_VALUES_FROM: NamedNodeRef<'_> =
        n("http://www.w3.org/2002/07/owl#someValuesFrom");
    pub const ALL_VALUES_FROM: NamedNodeRef<'_> = n("http://www.w3.org/2002/07/owl#allValuesFrom");
    pub const INTERSECTION_OF: NamedNodeRef<'_> = n("http://www.w3.org/2002/07/owl#intersectionOf");
    pub const UNION_OF: NamedNodeRef<'_> = n("http://www.w3.org/2002/07/owl#unionOf");
    pub const PROPERTY_CHAIN_AXIOM: NamedNodeRef<'_> =
        n("http://www.w3.org/2002/07/owl#propertyChainAxiom");
    pub const THING: NamedNodeRef<'_> = n("http://www.w3.org/2002/07/owl#Thing");
    pub const NOTHING: NamedNodeRef<'_> = n("http://www.w3.org/2002/07/owl#Nothing");
    pub const CLASS: NamedNodeRef<'_> = n("http://www.w3.org/2002/07/owl#Class");
    pub const OBJECT_PROPERTY: NamedNodeRef<'_> = n("http://www.w3.org/2002/07/owl#ObjectProperty");
    pub const DATATYPE_PROPERTY: NamedNodeRef<'_> =
        n("http://www.w3.org/2002/07/owl#DatatypeProperty");
    pub const SAME_AS: NamedNodeRef<'_> = n("http://www.w3.org/2002/07/owl#sameAs");
    pub const MAX_CARDINALITY: NamedNodeRef<'_> = n("http://www.w3.org/2002/07/owl#maxCardinality");
    pub const MAX_QUALIFIED_CARDINALITY: NamedNodeRef<'_> =
        n("http://www.w3.org/2002/07/owl#maxQualifiedCardinality");
    pub const ON_CLASS: NamedNodeRef<'_> = n("http://www.w3.org/2002/07/owl#onClass");
    pub const HAS_KEY: NamedNodeRef<'_> = n("http://www.w3.org/2002/07/owl#hasKey");
    pub const ONE_OF: NamedNodeRef<'_> = n("http://www.w3.org/2002/07/owl#oneOf");
    pub const FUNCTIONAL_PROPERTY: NamedNodeRef<'_> =
        n("http://www.w3.org/2002/07/owl#FunctionalProperty");
    pub const INVERSE_FUNCTIONAL_PROPERTY: NamedNodeRef<'_> =
        n("http://www.w3.org/2002/07/owl#InverseFunctionalProperty");
}

/// Which rules run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Regime {
    /// Jena's default RDFS rules (see the module documentation).
    Rdfs,
    /// RDFS and the OWL 2 RL rules for equivalent classes and properties, inverse, symmetric
    /// and transitive properties (cax-eqc1/2, prp-eqp1/2, scm-eqc1, scm-eqp1, prp-inv1/2,
    /// prp-symp, prp-trp), value restrictions (cls-hv1/2, cls-svf1/2, cls-avf, scm-hv,
    /// scm-svf1/2, scm-avf1/2), intersections and unions (cls-int1/2, cls-uni, scm-int,
    /// scm-uni), property chains (prp-spo2), owl:sameAs (eq-sym, eq-trans, eq-rep-s/p/o),
    /// (inverse) functional properties (prp-fp, prp-ifp), a maximum cardinality of 1
    /// (cls-maxc2, cls-maxqc3/4), keys (prp-key), enumerations (cls-oo), and equivalence from
    /// sub classes and sub properties both ways (scm-eqc2, scm-eqp2). The consistency rules
    /// write nothing: [`inconsistencies`] lists what they find (#187).
    OwlRl,
}
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};

mod consistency;
pub use consistency::{CAP, Inconsistencies, Inconsistency, inconsistencies};
mod explain;
pub use explain::{Cancelled, Derivation, Explainer};

/// What an entailment run did to its graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entailed {
    /// The triples the graph holds now: what the rules derive and no other graph holds.
    pub triples: usize,
    /// The triples that came in.
    pub added: usize,
    /// The triples that left: no longer derived, or put there by something else.
    pub removed: usize,
}

/// For each node, the nodes it reaches over one or more edges, itself only on a cycle;
/// `None` once `cancelled`.
fn closure(
    edges: &[(TermId, TermId)],
    cancelled: &dyn Fn() -> bool,
) -> Option<HashMap<TermId, Vec<TermId>>> {
    let mut next: HashMap<TermId, Vec<TermId>> = HashMap::new();
    for &(a, b) in edges {
        next.entry(a).or_default().push(b);
    }
    let mut out = HashMap::with_capacity(next.len());
    for &start in next.keys() {
        // A start reads up to every edge: on a closed hierarchy of thousands of classes
        // (each with an edge to each of its super classes) that is milliseconds each.
        if cancelled() {
            return None;
        }
        let mut seen = HashSet::new();
        let mut stack: Vec<TermId> = next[&start].clone();
        while let Some(n) = stack.pop() {
            if seen.insert(n)
                && let Some(more) = next.get(&n)
            {
                stack.extend(more.iter().copied());
            }
        }
        let mut reached: Vec<TermId> = seen.into_iter().collect();
        reached.sort_unstable();
        out.insert(start, reached);
    }
    Some(out)
}

/// What a triple of one property implies: its super properties, and the classes of its
/// subject and of its object (through its own domains and ranges and those of its super
/// properties, with their super classes).
#[derive(Debug, PartialEq)]
struct Plan {
    supers: Vec<TermId>,
    domain: Vec<TermId>,
    range: Vec<TermId>,
    /// OWL 2 RL: for `x p y`, the properties `q` with `y q x` (inverses of the property and
    /// of its super properties, with their super properties).
    inverse: Vec<TermId>,
    /// OWL 2 RL, cls-hv2: (v, R): `x p v` gives `x rdf:type R` (from the property and its
    /// super properties).
    values: Vec<(TermId, TermId)>,
}

/// The ids of the RDFS terms the rules read (`None`: not in the store, so in no triple).
/// Under [`Regime::Rdfs`] the OWL ids are `None`, so no OWL rule runs.
#[derive(Clone, Copy)]
struct Vocab {
    rdf_type: Option<TermId>,
    sub_class: Option<TermId>,
    sub_property: Option<TermId>,
    domain: Option<TermId>,
    range: Option<TermId>,
    equivalent_class: Option<TermId>,
    equivalent_property: Option<TermId>,
    inverse_of: Option<TermId>,
    symmetric: Option<TermId>,
    transitive: Option<TermId>,
    on_property: Option<TermId>,
    has_value: Option<TermId>,
    some_values_from: Option<TermId>,
    all_values_from: Option<TermId>,
    intersection_of: Option<TermId>,
    union_of: Option<TermId>,
    property_chain_axiom: Option<TermId>,
    first: Option<TermId>,
    rest: Option<TermId>,
    nil: Option<TermId>,
    thing: Option<TermId>,
    nothing: Option<TermId>,
    class: Option<TermId>,
    object_property: Option<TermId>,
    datatype_property: Option<TermId>,
    same_as: Option<TermId>,
    functional: Option<TermId>,
    inverse_functional: Option<TermId>,
    max_cardinality: Option<TermId>,
    max_qualified_cardinality: Option<TermId>,
    on_class: Option<TermId>,
    has_key: Option<TermId>,
    one_of: Option<TermId>,
}

impl Vocab {
    fn is_schema(&self, p: TermId) -> bool {
        [
            self.sub_class,
            self.sub_property,
            self.domain,
            self.range,
            self.equivalent_class,
            self.equivalent_property,
            self.inverse_of,
            self.on_property,
            self.has_value,
            self.some_values_from,
            self.all_values_from,
            self.intersection_of,
            self.union_of,
            self.property_chain_axiom,
            self.max_cardinality,
            self.max_qualified_cardinality,
            self.on_class,
            self.has_key,
            self.one_of,
        ]
        .contains(&Some(p))
    }

    /// A schema triple: of a schema property, or a property typed symmetric, transitive,
    /// functional or inverse functional.
    fn is_schema_triple(&self, [_, p, o]: &[TermId; 3]) -> bool {
        self.is_schema(*p)
            || (Some(*p) == self.rdf_type
                && [
                    self.symmetric,
                    self.transitive,
                    self.functional,
                    self.inverse_functional,
                ]
                .contains(&Some(*o)))
    }

    fn owl(&self) -> bool {
        self.inverse_of.is_some()
            || self.equivalent_class.is_some()
            || self.equivalent_property.is_some()
            || self.symmetric.is_some()
            || self.transitive.is_some()
            || self.on_property.is_some()
            || self.intersection_of.is_some()
            || self.union_of.is_some()
            || self.property_chain_axiom.is_some()
            || self.same_as.is_some()
            || self.functional.is_some()
            || self.inverse_functional.is_some()
            || self.max_cardinality.is_some()
            || self.max_qualified_cardinality.is_some()
            || self.has_key.is_some()
            || self.one_of.is_some()
    }
}

/// The compiled schema.
#[derive(Debug, PartialEq)]
struct Schema {
    super_classes: HashMap<TermId, Vec<TermId>>,
    super_properties: HashMap<TermId, Vec<TermId>>,
    /// OWL 2 RL scm-cls, scm-op and scm-dp: the classes (with owl:Thing and owl:Nothing)
    /// and properties declared as such, each its own sub class (property) and equivalent
    /// (sorted). Kept out of the hierarchies, where they would derive each of their
    /// triples again.
    declared: [Vec<TermId>; 2],
    /// OWL 2 RL scm-dom1/2 and scm-rng1/2: each property's stated domains and ranges with
    /// those of its super properties and their super classes, as triples.
    scoped: Vec<[TermId; 3]>,
    plans: HashMap<TermId, Plan>,
    /// OWL 2 RL: the transitive properties (sorted).
    transitive: Vec<TermId>,
    /// OWL 2 RL, cls-hv1: for a class R, (p, v): `x rdf:type R` gives `x p v`.
    values: HashMap<TermId, Vec<(TermId, TermId)>>,
    /// OWL 2 RL: the rules that join triples.
    joins: Joins,
}

/// The OWL 2 RL rules that join two triples or more, run on all triples in each round
/// whose new triples they read.
#[derive(Debug, PartialEq, Default)]
struct Joins {
    /// cls-svf1, (R, p, C): `x p y` and `y rdf:type C` give `x rdf:type R`.
    some: Vec<(TermId, TermId, TermId)>,
    /// cls-avf, (R, p, C): `x rdf:type R` and `x p y` give `y rdf:type C`.
    all: Vec<(TermId, TermId, TermId)>,
    /// cls-int1, (C, [C1, …]): `x rdf:type Ci` for each i gives `x rdf:type C`.
    intersections: Vec<(TermId, Vec<TermId>)>,
    /// prp-spo2, (p, [p1, …]): `x0 p1 x1`, …, `xn-1 pn xn` give `x0 p xn`.
    chains: Vec<(TermId, Vec<TermId>)>,
    /// prp-fp: `x p y1` and `x p y2` give `y1 owl:sameAs y2`; prp-ifp: `x1 p y` and `x2 p y`
    /// give `x1 owl:sameAs x2` (resources, not equal, as in Jena).
    functional: Vec<TermId>,
    inverse_functional: Vec<TermId>,
    /// cls-maxc2 and cls-maxqc3/4, (R, p, C): `x rdf:type R`, `x p y1`, `x p y2` (with
    /// `y1` and `y2` of type C, if any) give `y1 owl:sameAs y2`.
    max_one: Vec<(TermId, TermId, Option<TermId>)>,
    /// prp-key, (C, [p1, …]): `x` and `y` of type C with a value in common for each pi give
    /// `x owl:sameAs y`.
    keys: Vec<(TermId, Vec<TermId>)>,
    /// cls-oo, (C, [x1, …]): each xi is of type C.
    one_of: Vec<(TermId, Vec<TermId>)>,
    /// The properties and the classes whose triples they read (sorted).
    properties: Vec<TermId>,
    classes: Vec<TermId>,
}

impl Joins {
    /// With [`Joins::properties`] and [`Joins::classes`] made from the rules.
    fn indexed(mut self) -> Self {
        let j = &mut self;
        for &(r, p, c) in &j.max_one {
            j.properties.push(p);
            j.classes.push(r);
            j.classes.extend(c);
        }
        for (c, ps) in &j.keys {
            j.classes.push(*c);
            j.properties.extend(ps.iter().copied());
        }
        for &(_, p, c) in &j.some {
            j.properties.push(p);
            j.classes.push(c);
        }
        for &(r, p, _) in &j.all {
            j.properties.push(p);
            j.classes.push(r);
        }
        for (_, cs) in &j.intersections {
            j.classes.extend(cs.iter().copied());
        }
        for (_, ps) in &j.chains {
            j.properties.extend(ps.iter().copied());
        }
        j.properties
            .extend(j.functional.iter().chain(&j.inverse_functional).copied());
        for xs in [&mut j.properties, &mut j.classes] {
            xs.sort_unstable();
            xs.dedup();
        }
        self
    }

    fn is_empty(&self) -> bool {
        self.properties.is_empty() && self.classes.is_empty() && self.one_of.is_empty()
    }

    /// Whether they read one of `triples`.
    fn read(&self, v: Vocab, triples: &[[TermId; 3]]) -> bool {
        (!self.properties.is_empty() || !self.classes.is_empty())
            && triples.iter().any(|&[_, p, o]| {
                self.properties.binary_search(&p).is_ok()
                    || (Some(p) == v.rdf_type && self.classes.binary_search(&o).is_ok())
            })
    }

    /// As [`Joins::run`], after a round that derived `delta` (also in `derived`, sorted):
    /// cls-svf1, cls-avf and cls-int1 join only the new triples with the others (semi-naive),
    /// so a new triple costs a few lookups instead of a pass over every restriction (#756);
    /// the other rules run in full when the round touched what they read. The first pass
    /// ([`Schema::pass`]) runs them all in full.
    #[allow(clippy::too_many_arguments)]
    fn run_delta(
        &self,
        snapshot: &Snapshot,
        skip: Option<TermId>,
        v: Vocab,
        derived: &[[TermId; 3]],
        delta: &[[TermId; 3]],
        out: &mut Vec<[TermId; 3]>,
        cancelled: &dyn Fn() -> bool,
    ) {
        let Some(t) = v.rdf_type else {
            return;
        };
        // Whether `x rdf:type c` is in the data or derived.
        let member = |x: TermId, c: TermId| {
            derived.binary_search(&[x, t, c]).is_ok()
                || snapshot
                    .quad_ids_for_pattern(Some(x), Some(t), Some(c), GraphSelector::All)
                    .any(|q| Some(q[3]) != skip)
        };
        // The `y` of `x p y`, in the data and derived.
        let objects = |x: TermId, p: TermId, out: &mut Vec<TermId>| {
            out.extend(
                snapshot
                    .quad_ids_for_pattern(Some(x), Some(p), None, GraphSelector::All)
                    .filter(|q| Some(q[3]) != skip)
                    .map(|q| q[2]),
            );
            let from = derived.partition_point(|d| (d[0], d[1]) < (x, p));
            out.extend(
                derived[from..]
                    .iter()
                    .take_while(|d| d[0] == x && d[1] == p)
                    .map(|d| d[2]),
            );
        };
        let resource = |t: TermId| matches!(t.kind(), Kind::Iri | Kind::Blank);
        // The restrictions and intersections by the property and class they read.
        let mut some_by_class: HashMap<TermId, Vec<(TermId, TermId)>> = HashMap::new();
        let mut some_by_property: HashMap<TermId, Vec<(TermId, TermId)>> = HashMap::new();
        for &(r, p, c) in &self.some {
            some_by_class.entry(c).or_default().push((r, p));
            some_by_property.entry(p).or_default().push((r, c));
        }
        let mut all_by_class: HashMap<TermId, Vec<(TermId, TermId)>> = HashMap::new();
        let mut all_by_property: HashMap<TermId, Vec<(TermId, TermId)>> = HashMap::new();
        for &(r, p, c) in &self.all {
            all_by_class.entry(r).or_default().push((p, c));
            all_by_property.entry(p).or_default().push((r, c));
        }
        let mut int_by_class: HashMap<TermId, Vec<usize>> = HashMap::new();
        for (i, (_, cs)) in self.intersections.iter().enumerate() {
            for &c in cs {
                int_by_class.entry(c).or_default().push(i);
            }
        }
        // cls-svf1 from a new type triple needs the subjects of `x p y` for its `y`: the
        // derived ones, by property and object, made only when a round needs them.
        let mut derived_subjects: Option<HashMap<(TermId, TermId), Vec<TermId>>> = None;
        let mut ys = Vec::new();
        for (i, &[s, p, o]) in delta.iter().enumerate() {
            if i % 4096 == 4095 && cancelled() {
                return;
            }
            if p == t {
                for &(r, q) in some_by_class.get(&o).into_iter().flatten() {
                    // cls-svf1: `x q s` and the new `s rdf:type o` give `x rdf:type r`.
                    out.extend(
                        snapshot
                            .quad_ids_for_pattern(None, Some(q), Some(s), GraphSelector::All)
                            .filter(|g| Some(g[3]) != skip)
                            .map(|g| [g[0], t, r]),
                    );
                    let index = derived_subjects.get_or_insert_with(|| {
                        let mut m: HashMap<(TermId, TermId), Vec<TermId>> = HashMap::new();
                        for d in derived {
                            if some_by_property.contains_key(&d[1]) {
                                m.entry((d[1], d[2])).or_default().push(d[0]);
                            }
                        }
                        m
                    });
                    out.extend(index.get(&(q, s)).into_iter().flatten().map(|&x| [x, t, r]));
                }
                for &(q, c) in all_by_class.get(&o).into_iter().flatten() {
                    // cls-avf: the new `s rdf:type o` and `s q y` give `y rdf:type c`.
                    ys.clear();
                    objects(s, q, &mut ys);
                    out.extend(ys.iter().filter(|&&y| resource(y)).map(|&y| [y, t, c]));
                }
                for &k in int_by_class.get(&o).into_iter().flatten() {
                    // cls-int1: the new `s rdf:type o` and `s` of every other member class.
                    let (c, cs) = &self.intersections[k];
                    if cs.iter().all(|&m| m == o || member(s, m)) {
                        out.push([s, t, *c]);
                    }
                }
            }
            for &(r, c) in some_by_property.get(&p).into_iter().flatten() {
                // cls-svf1: the new `s p o` and `o rdf:type c` give `s rdf:type r`.
                if member(o, c) {
                    out.push([s, t, r]);
                }
            }
            for &(r, c) in all_by_property.get(&p).into_iter().flatten() {
                // cls-avf: `s rdf:type r` and the new `s p o` give `o rdf:type c`.
                if resource(o) && member(s, r) {
                    out.push([o, t, c]);
                }
            }
        }
        // The other rules: in full, when the round touched them.
        let rest = Joins {
            chains: self.chains.clone(),
            functional: self.functional.clone(),
            inverse_functional: self.inverse_functional.clone(),
            max_one: self.max_one.clone(),
            keys: self.keys.clone(),
            ..Joins::default()
        }
        .indexed();
        if rest.read(v, delta) {
            rest.run(snapshot, skip, v, derived, out, cancelled);
        }
    }

    /// What they derive from the data's triples and `derived` (sorted).
    fn run(
        &self,
        snapshot: &Snapshot,
        skip: Option<TermId>,
        v: Vocab,
        derived: &[[TermId; 3]],
        out: &mut Vec<[TermId; 3]>,
        cancelled: &dyn Fn() -> bool,
    ) {
        let Some(t) = v.rdf_type else {
            return;
        };
        if self.is_empty() {
            return;
        }
        // The derived triples they read, by property and by class.
        let mut by_property: HashMap<TermId, Vec<(TermId, TermId)>> = HashMap::new();
        let mut by_class: HashMap<TermId, Vec<TermId>> = HashMap::new();
        for (i, &[s, p, o]) in derived.iter().enumerate() {
            if i % 65_536 == 65_535 && cancelled() {
                return;
            }
            if self.properties.binary_search(&p).is_ok() {
                by_property.entry(p).or_default().push((s, o));
            }
            if p == t && self.classes.binary_search(&o).is_ok() {
                by_class.entry(o).or_default().push(s);
            }
        }
        let pairs = |p: TermId| -> Vec<(TermId, TermId)> {
            let mut out: Vec<(TermId, TermId)> = snapshot
                .quad_ids_for_pattern(None, Some(p), None, GraphSelector::All)
                .filter(|q| Some(q[3]) != skip)
                .map(|q| (q[0], q[2]))
                .collect();
            out.extend(by_property.get(&p).into_iter().flatten().copied());
            out
        };
        let members = |c: TermId| -> HashSet<TermId> {
            let mut out: HashSet<TermId> = snapshot
                .quad_ids_for_pattern(None, Some(t), Some(c), GraphSelector::All)
                .filter(|q| Some(q[3]) != skip)
                .map(|q| q[0])
                .collect();
            out.extend(by_class.get(&c).into_iter().flatten().copied());
            out
        };
        let resource = |t: TermId| matches!(t.kind(), Kind::Iri | Kind::Blank);
        for &(r, p, c) in &self.some {
            if cancelled() {
                return;
            }
            let ms = members(c);
            out.extend(
                pairs(p)
                    .into_iter()
                    .filter(|(_, y)| ms.contains(y))
                    .map(|(x, _)| [x, t, r]),
            );
        }
        for &(r, p, c) in &self.all {
            if cancelled() {
                return;
            }
            let ms = members(r);
            out.extend(
                pairs(p)
                    .into_iter()
                    .filter(|&(x, y)| resource(y) && ms.contains(&x))
                    .map(|(_, y)| [y, t, c]),
            );
        }
        for (c, cs) in &self.intersections {
            let mut sets: Vec<HashSet<TermId>> = cs.iter().map(|&c| members(c)).collect();
            sets.sort_by_key(|s| s.len());
            if let Some((first, rest)) = sets.split_first() {
                out.extend(
                    first
                        .iter()
                        .filter(|x| rest.iter().all(|s| s.contains(x)))
                        .map(|&x| [x, t, *c]),
                );
            }
        }
        for (p, ps) in &self.chains {
            let Some((first, rest)) = ps.split_first() else {
                continue;
            };
            let mut reached = pairs(*first);
            for &q in rest {
                if cancelled() {
                    return;
                }
                let mut next: HashMap<TermId, Vec<TermId>> = HashMap::new();
                for (a, b) in pairs(q) {
                    next.entry(a).or_default().push(b);
                }
                // A chain over a long path joins thousands of pairs for each pair reached,
                // most of them the same: sorted and each kept once as they come, every few
                // million, so no sort is long and the duplicates take no memory.
                let mut more: Vec<(TermId, TermId)> = Vec::new();
                let mut kept = 0;
                for part in reached.chunks(1024) {
                    if cancelled() {
                        return;
                    }
                    more.extend(part.iter().flat_map(|&(x, y)| {
                        next.get(&y).into_iter().flatten().map(move |&z| (x, z))
                    }));
                    if more.len() > 2 * kept + (1 << 22) {
                        more.par_sort_unstable();
                        more.dedup();
                        kept = more.len();
                    }
                }
                more.par_sort_unstable();
                more.dedup();
                reached = more;
            }
            out.extend(reached.into_iter().map(|(x, y)| [x, *p, y]));
        }
        for (c, xs) in &self.one_of {
            out.extend(xs.iter().map(|&x| [x, t, *c]));
        }
        if let Some(same) = v.same_as {
            let mut group = |groups: Vec<Vec<TermId>>| {
                for mut xs in groups {
                    xs.sort_unstable();
                    xs.dedup();
                    for (i, &a) in xs.iter().enumerate() {
                        if i % 256 == 255 && cancelled() {
                            return;
                        }
                        out.extend(xs.iter().filter(|&&b| b != a).map(|&b| [a, same, b]));
                    }
                }
            };
            for &p in &self.functional {
                if cancelled() {
                    return;
                }
                let mut by: HashMap<TermId, Vec<TermId>> = HashMap::new();
                for (x, y) in pairs(p) {
                    if resource(y) {
                        by.entry(x).or_default().push(y);
                    }
                }
                group(by.into_values().collect());
            }
            for &p in &self.inverse_functional {
                if cancelled() {
                    return;
                }
                let mut by: HashMap<TermId, Vec<TermId>> = HashMap::new();
                for (x, y) in pairs(p) {
                    by.entry(y).or_default().push(x);
                }
                group(by.into_values().collect());
            }
            for &(r, p, c) in &self.max_one {
                if cancelled() {
                    return;
                }
                let (ms, of_class) = (members(r), c.map(members));
                let mut by: HashMap<TermId, Vec<TermId>> = HashMap::new();
                for (x, y) in pairs(p) {
                    if resource(y)
                        && ms.contains(&x)
                        && of_class.as_ref().is_none_or(|cs| cs.contains(&y))
                    {
                        by.entry(x).or_default().push(y);
                    }
                }
                group(by.into_values().collect());
            }
            for (c, ps) in &self.keys {
                if cancelled() {
                    return;
                }
                let ms = members(*c);
                // Each member's values of each key property.
                let values: Vec<HashMap<TermId, Vec<TermId>>> = ps
                    .iter()
                    .map(|&p| {
                        let mut m: HashMap<TermId, Vec<TermId>> = HashMap::new();
                        for (x, y) in pairs(p) {
                            if ms.contains(&x) {
                                m.entry(x).or_default().push(y);
                            }
                        }
                        m
                    })
                    .collect();
                // Two members with a value in common for every key property are the same. The
                // candidates are the members that share a value of one property, the one whose
                // values make the fewest pairs; each pair is then tested on the others. The
                // work is the pairs the data has for that property, not the combinations of
                // values.
                let mut values = values;
                for m in &mut values {
                    for ys in m.values_mut() {
                        ys.sort_unstable();
                        ys.dedup();
                    }
                }
                let sharing = |m: &HashMap<TermId, Vec<TermId>>| {
                    let mut by: HashMap<TermId, Vec<TermId>> = HashMap::new();
                    for (&x, ys) in m {
                        for &y in ys {
                            by.entry(y).or_default().push(x);
                        }
                    }
                    by
                };
                let cost = |m: &HashMap<TermId, Vec<TermId>>| -> usize {
                    sharing(m).values().map(|xs| xs.len() * xs.len()).sum()
                };
                let Some(narrow) = (0..values.len()).min_by_key(|&i| cost(&values[i])) else {
                    continue;
                };
                let common = |a: &[TermId], b: &[TermId]| {
                    let (mut i, mut j) = (0, 0);
                    while i < a.len() && j < b.len() {
                        match a[i].cmp(&b[j]) {
                            std::cmp::Ordering::Less => i += 1,
                            std::cmp::Ordering::Greater => j += 1,
                            std::cmp::Ordering::Equal => return true,
                        }
                    }
                    false
                };
                let mut same_pairs: HashSet<(TermId, TermId)> = HashSet::new();
                for (_, mut xs) in sharing(&values[narrow]) {
                    if xs.len() > 64 && cancelled() {
                        return;
                    }
                    xs.sort_unstable();
                    for (i, &x) in xs.iter().enumerate() {
                        if i % 256 == 255 && cancelled() {
                            return;
                        }
                        for &y in &xs[i + 1..] {
                            if same_pairs.contains(&(x, y)) {
                                continue;
                            }
                            let all = values.iter().enumerate().all(|(k, m)| {
                                k == narrow
                                    || matches!((m.get(&x), m.get(&y)), (Some(a), Some(b)) if common(a, b))
                            });
                            if all {
                                same_pairs.insert((x, y));
                            }
                        }
                    }
                }
                group(same_pairs.into_iter().map(|(x, y)| vec![x, y]).collect());
            }
        }
    }

    /// prp-fp and prp-ifp for these properties.
    fn functional(&mut self, functional: Vec<TermId>, inverse_functional: Vec<TermId>) {
        self.properties.extend(&functional);
        self.properties.extend(&inverse_functional);
        self.properties.sort_unstable();
        self.properties.dedup();
        self.functional = functional;
        self.inverse_functional = inverse_functional;
    }
}

/// OWL 2 RL owl:sameAs between resources (eq-sym, eq-trans, eq-rep-s/p/o): the classes of
/// resources the same as each other. eq-ref, which makes every term the same as itself,
/// is left out, as in Jena.
#[derive(Default)]
struct Same {
    /// For a member, its class in `classes`.
    of: HashMap<TermId, usize>,
    /// Each class's members (sorted), the classes sorted.
    classes: Vec<Vec<TermId>>,
    /// The canonical triples (each term in a class replaced by its class's first member)
    /// expanded under these classes: an expansion is closed under replacement, so no triple
    /// is expanded twice and the work stays proportional to what is written.
    expanded: HashSet<[TermId; 3]>,
    /// Whether the classes were read once.
    ready: bool,
}

impl Same {
    /// What owl:sameAs derives in a round from the data, everything derived (`all`, sorted)
    /// and what the last round derived (`delta`): with the classes changed, the classes as
    /// triples and every triple with a member rewritten; else the new triples rewritten.
    /// Whether the classes changed.
    #[allow(clippy::too_many_arguments)]
    fn round(
        &mut self,
        snapshot: &Snapshot,
        skip: Option<TermId>,
        v: Vocab,
        all: &[[TermId; 3]],
        delta: &[[TermId; 3]],
        out: &mut Vec<[TermId; 3]>,
        cancelled: &dyn Fn() -> bool,
    ) -> bool {
        let Some(same) = v.same_as else {
            return false;
        };
        // The classes change only with new owl:sameAs triples.
        if self.ready && !delta.iter().any(|t| t[1] == same) {
            if !self.classes.is_empty() {
                let mut expanded = std::mem::take(&mut self.expanded);
                // The caller asks again and stops.
                self.replace_each(&mut expanded, delta, out, cancelled);
                self.expanded = expanded;
            }
            return false;
        }
        self.ready = true;
        let resource = |t: TermId| matches!(t.kind(), Kind::Iri | Kind::Blank);
        let mut parent: HashMap<TermId, TermId> = HashMap::new();
        fn find(parent: &mut HashMap<TermId, TermId>, mut x: TermId) -> TermId {
            while let Some(&p) = parent.get(&x) {
                if p == x {
                    break;
                }
                let up = parent.get(&p).copied().unwrap_or(p);
                parent.insert(x, up);
                x = up;
            }
            x
        }
        let pairs = snapshot
            .quad_ids_for_pattern(None, Some(same), None, GraphSelector::All)
            .filter(|q| Some(q[3]) != skip)
            .map(|q| (q[0], q[2]))
            .chain(all.iter().filter(|t| t[1] == same).map(|t| (t[0], t[2])));
        for (i, (a, b)) in pairs.enumerate() {
            if i % 65_536 == 65_535 && cancelled() {
                return true;
            }
            if a != b && resource(a) && resource(b) {
                let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
                parent.entry(ra).or_insert(ra);
                if ra != rb {
                    parent.insert(rb, ra);
                }
            }
        }
        let members: Vec<TermId> = parent.keys().copied().collect();
        let mut by_root: HashMap<TermId, Vec<TermId>> = HashMap::new();
        for m in members {
            let r = find(&mut parent, m);
            by_root.entry(r).or_default().push(m);
        }
        let mut classes: Vec<Vec<TermId>> = by_root
            .into_values()
            .map(|mut c| {
                c.sort_unstable();
                c
            })
            .collect();
        classes.sort_unstable();
        if classes == self.classes {
            if !self.classes.is_empty() {
                let mut expanded = std::mem::take(&mut self.expanded);
                // The caller asks again and stops.
                self.replace_each(&mut expanded, delta, out, cancelled);
                self.expanded = expanded;
            }
            return false;
        }
        self.of = classes
            .iter()
            .enumerate()
            .flat_map(|(i, c)| c.iter().map(move |&m| (m, i)))
            .collect();
        self.classes = classes;
        let mut expanded = HashSet::new();
        // The class triples are what an owl:sameAs triple within a class expands to: it
        // needs no expanding again (when owl:sameAs itself is in no class).
        let plain = !self.of.contains_key(&same);
        for c in &self.classes {
            if plain {
                expanded.insert([c[0], same, c[0]]);
            }
            for (i, &a) in c.iter().enumerate() {
                // The caller asks again and stops: what is out by then is dropped.
                if i % 256 == 255 && cancelled() {
                    return true;
                }
                out.extend(c.iter().map(|&b| [a, same, b]));
            }
        }
        for c in &self.classes {
            for (i, &m) in c.iter().enumerate() {
                if i % 256 == 255 && cancelled() {
                    return true;
                }
                for pattern in [
                    (Some(m), None, None),
                    (None, Some(m), None),
                    (None, None, Some(m)),
                ] {
                    for q in snapshot.quad_ids_for_pattern(
                        pattern.0,
                        pattern.1,
                        pattern.2,
                        GraphSelector::All,
                    ) {
                        if Some(q[3]) != skip
                            && self.replace(&mut expanded, [q[0], q[1], q[2]], out, cancelled)
                        {
                            return true;
                        }
                    }
                }
            }
        }
        if self.replace_each(&mut expanded, all, out, cancelled) {
            return true;
        }
        self.expanded = expanded;
        true
    }

    /// The first member of `t`'s class, or `t`.
    fn canon(&self, t: TermId) -> TermId {
        self.of.get(&t).map_or(t, |&i| self.classes[i][0])
    }

    /// The class of `one`'s term, or itself; whether it is in one.
    fn class<'a>(&'a self, one: &'a [TermId; 1]) -> (&'a [TermId], bool) {
        match self.of.get(&one[0]) {
            Some(&i) => (&self.classes[i][..], true),
            None => (&one[..], false),
        }
    }

    /// [`Self::replace`] for each of `triples`; whether it stopped, `cancelled`.
    fn replace_each(
        &self,
        expanded: &mut HashSet<[TermId; 3]>,
        triples: &[[TermId; 3]],
        out: &mut Vec<[TermId; 3]>,
        cancelled: &dyn Fn() -> bool,
    ) -> bool {
        for (i, &t) in triples.iter().enumerate() {
            if (i % 65_536 == 65_535 && cancelled()) || self.replace(expanded, t, out, cancelled) {
                return true;
            }
        }
        false
    }

    /// eq-rep-s/p/o: `t` with each of its terms replaced by those the same as it, unless a
    /// triple with the same canonical form was. Whether it stopped, `cancelled` (a triple
    /// with two terms in a class of thousands is millions of triples).
    fn replace(
        &self,
        expanded: &mut HashSet<[TermId; 3]>,
        [s, p, o]: [TermId; 3],
        out: &mut Vec<[TermId; 3]>,
        cancelled: &dyn Fn() -> bool,
    ) -> bool {
        let (s1, p1, o1) = ([s], [p], [o]);
        let ((ss, a), (ps, b), (os, c)) = (self.class(&s1), self.class(&p1), self.class(&o1));
        if !(a || b || c) {
            return false;
        }
        if !expanded.insert([ss[0], ps[0], os[0]]) {
            return false;
        }
        for (i, &x) in ss.iter().enumerate() {
            if i % 256 == 255 && cancelled() {
                return true;
            }
            for &q in ps {
                out.extend(os.iter().map(|&y| [x, q, y]));
            }
        }
        false
    }
}

/// The members of the RDF list at `head` in the data (not in graph `skip`), or `None` when
/// it is no well-formed list.
fn list(snapshot: &Snapshot, skip: Option<TermId>, v: Vocab, head: TermId) -> Option<Vec<TermId>> {
    let (first, rest, nil) = (v.first?, v.rest?, v.nil);
    let one = |s: TermId, p: TermId| {
        snapshot
            .quad_ids_for_pattern(Some(s), Some(p), None, GraphSelector::All)
            .find(|q| Some(q[3]) != skip)
            .map(|q| q[2])
    };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut node = head;
    while Some(node) != nil {
        if !seen.insert(node) {
            return None;
        }
        out.push(one(node, first)?);
        node = one(node, rest)?;
    }
    Some(out)
}

/// Whether `n` is a literal of an XSD integer type with the value 1 (a cardinality).
fn is_one(snapshot: &Snapshot, n: TermId) -> bool {
    is_cardinality(snapshot, n, 1)
}

/// Whether `n` is a literal of an XSD integer type with the value `k` (a cardinality).
fn is_cardinality(snapshot: &Snapshot, n: TermId, k: u64) -> bool {
    const INTEGERS: [&str; 11] = [
        "integer",
        "nonNegativeInteger",
        "positiveInteger",
        "long",
        "int",
        "short",
        "byte",
        "unsignedLong",
        "unsignedInt",
        "unsignedShort",
        "unsignedByte",
    ];
    matches!(
        snapshot.term(n),
        Ok(oxigraph::model::Term::Literal(l))
            if l.datatype()
                .as_str()
                .strip_prefix("http://www.w3.org/2001/XMLSchema#")
                .is_some_and(|t| INTEGERS.contains(&t))
                && l.value().trim().parse::<u64>() == Ok(k)
    )
}

/// The OWL 2 RL class descriptions of the schema.
#[derive(Default)]
struct Restrictions {
    /// (R, p, v) for owl:hasValue, (R, p, C) for owl:someValuesFrom and owl:allValuesFrom.
    value: Vec<(TermId, TermId, TermId)>,
    some: Vec<(TermId, TermId, TermId)>,
    all: Vec<(TermId, TermId, TermId)>,
    /// (C, [C1, …]) for owl:intersectionOf and owl:unionOf; (p, [p1, …]) for
    /// owl:propertyChainAxiom.
    intersections: Vec<(TermId, Vec<TermId>)>,
    unions: Vec<(TermId, Vec<TermId>)>,
    chains: Vec<(TermId, Vec<TermId>)>,
    /// (R, p, C) for owl:maxCardinality 1 (C `None`) and owl:maxQualifiedCardinality 1 on
    /// class C (`None` for owl:Thing).
    max_one: Vec<(TermId, TermId, Option<TermId>)>,
    /// (C, [p1, …]) for owl:hasKey; (C, [x1, …]) for owl:oneOf.
    keys: Vec<(TermId, Vec<TermId>)>,
    one_of: Vec<(TermId, Vec<TermId>)>,
}

impl Restrictions {
    /// From `pairs` (canonical, see [`Schema::compile`]); lists from their heads in
    /// `raw_pairs`, their members made canonical with `canon`.
    fn read(
        snapshot: &Snapshot,
        skip: Option<TermId>,
        v: Vocab,
        pairs: &impl Fn(Option<TermId>) -> Vec<(TermId, TermId)>,
        raw_pairs: &impl Fn(Option<TermId>) -> Vec<(TermId, TermId)>,
        canon: &impl Fn(TermId) -> TermId,
    ) -> Self {
        let mut me = Self::default();
        if v.on_property.is_some() {
            let mut on: HashMap<TermId, Vec<TermId>> = HashMap::new();
            for (r, p) in pairs(v.on_property) {
                on.entry(r).or_default().push(p);
            }
            let with = |of: Option<TermId>, out: &mut Vec<(TermId, TermId, TermId)>| {
                for (r, c) in pairs(of) {
                    for &p in on.get(&r).into_iter().flatten() {
                        out.push((r, p, c));
                    }
                }
                out.sort_unstable();
                out.dedup();
            };
            with(v.has_value, &mut me.value);
            with(v.some_values_from, &mut me.some);
            with(v.all_values_from, &mut me.all);
            // cls-maxc2, cls-maxqc3 and cls-maxqc4: a maximum of 1 (a maximum of 0 only
            // makes data inconsistent, which is not checked).
            let mut max_one = Vec::new();
            with(v.max_cardinality, &mut max_one);
            me.max_one.extend(
                max_one
                    .into_iter()
                    .filter(|&(_, _, n)| is_one(snapshot, n))
                    .map(|(r, p, _)| (r, p, None)),
            );
            let mut qualified = Vec::new();
            with(v.max_qualified_cardinality, &mut qualified);
            let classes: HashMap<TermId, Vec<TermId>> =
                pairs(v.on_class)
                    .into_iter()
                    .fold(HashMap::new(), |mut m, (r, c)| {
                        m.entry(r).or_default().push(c);
                        m
                    });
            let thing = v.thing.map(canon);
            for (r, p, n) in qualified {
                if is_one(snapshot, n) {
                    for &c in classes.get(&r).into_iter().flatten() {
                        me.max_one
                            .push((r, p, Some(c).filter(|&c| Some(c) != thing)));
                    }
                }
            }
            me.max_one.sort_unstable();
            me.max_one.dedup();
        }
        let lists = |of: Option<TermId>| -> Vec<(TermId, Vec<TermId>)> {
            let mut out: Vec<(TermId, Vec<TermId>)> = raw_pairs(of)
                .into_iter()
                .filter_map(|(c, head)| {
                    let members = list(snapshot, skip, v, head)?;
                    Some((canon(c), members.into_iter().map(canon).collect()))
                })
                .collect();
            out.sort_unstable();
            out.dedup();
            out
        };
        me.intersections = lists(v.intersection_of);
        me.unions = lists(v.union_of);
        me.chains = lists(v.property_chain_axiom);
        me.keys = lists(v.has_key);
        me.one_of = lists(v.one_of);
        me
    }

    /// scm-int and scm-uni: (sub class, super class).
    fn class_edges(&self) -> Vec<(TermId, TermId)> {
        let term = |t: TermId| matches!(t.kind(), Kind::Iri | Kind::Blank);
        let mut out = Vec::new();
        for (c, cs) in &self.intersections {
            out.extend(cs.iter().filter(|&&d| term(d)).map(|&d| (*c, d)));
        }
        for (c, cs) in &self.unions {
            out.extend(cs.iter().filter(|&&d| term(d)).map(|&d| (d, *c)));
        }
        out
    }

    /// scm-hv, scm-svf1/2 and scm-avf1/2 under the closed hierarchies: (sub class, super
    /// class).
    ///
    /// Two restrictions in one owl:sameAs class are one canonical restriction, which is
    /// then a sub class of itself (`shared`: whether a term's class has other members).
    fn sub_classes(
        &self,
        super_classes: &HashMap<TermId, Vec<TermId>>,
        super_properties: &HashMap<TermId, Vec<TermId>>,
        shared: &impl Fn(TermId) -> bool,
    ) -> Vec<(TermId, TermId)> {
        let differ = |a: TermId, b: TermId| a != b || shared(a);
        let under = |m: &HashMap<TermId, Vec<TermId>>, a: TermId, b: TermId| {
            a == b || m.get(&a).is_some_and(|s| s.binary_search(&b).is_ok())
        };
        let mut out = Vec::new();
        for &(r1, p1, i1) in &self.value {
            for &(r2, p2, i2) in &self.value {
                if differ(r1, r2) && i1 == i2 && under(super_properties, p1, p2) {
                    out.push((r1, r2));
                }
            }
        }
        for &(r1, p1, c1) in &self.some {
            for &(r2, p2, c2) in &self.some {
                if differ(r1, r2)
                    && ((p1 == p2 && under(super_classes, c1, c2))
                        || (c1 == c2 && under(super_properties, p1, p2)))
                {
                    out.push((r1, r2));
                }
            }
        }
        for &(r1, p1, c1) in &self.all {
            for &(r2, p2, c2) in &self.all {
                if differ(r1, r2) && p1 == p2 && under(super_classes, c1, c2) {
                    out.push((r1, r2));
                }
                // scm-avf2: a restriction on a super property is the narrower one.
                if differ(r1, r2) && c1 == c2 && under(super_properties, p1, p2) {
                    out.push((r2, r1));
                }
            }
        }
        out
    }

    /// The rules that join triples.
    /// `thing`: owl:Thing's canonical term.
    fn joins(&self, thing: Option<TermId>) -> Joins {
        let j = Joins {
            some: self
                .some
                .iter()
                .filter(|s| Some(s.2) != thing)
                .copied()
                .collect(),
            all: self.all.clone(),
            intersections: self
                .intersections
                .iter()
                .filter(|(_, cs)| !cs.is_empty())
                .cloned()
                .collect(),
            chains: self
                .chains
                .iter()
                .filter(|(_, ps)| !ps.is_empty())
                .cloned()
                .collect(),
            max_one: self.max_one.clone(),
            keys: self
                .keys
                .iter()
                .filter(|(_, ps)| !ps.is_empty())
                .cloned()
                .collect(),
            one_of: self.one_of.clone(),
            ..Joins::default()
        };
        j.indexed()
    }
}

impl Schema {
    /// From the schema triples of the data and of `derived` (sorted).
    ///
    /// Each term is replaced by `canon` (its owl:sameAs class's first member): the rules
    /// then fire on the canonical form of each triple, which owl:sameAs keeps derived for
    /// every triple with a member, and not again on each of its other forms.
    fn compile(
        snapshot: &Snapshot,
        skip: Option<TermId>,
        v: Vocab,
        derived: &[[TermId; 3]],
        canon: &impl Fn(TermId) -> TermId,
        shared: &impl Fn(TermId) -> bool,
        cancelled: &dyn Fn() -> bool,
    ) -> Option<Self> {
        let raw_pairs = |p: Option<TermId>| -> Vec<(TermId, TermId)> {
            let Some(p) = p else {
                return Vec::new();
            };
            let mut out: Vec<(TermId, TermId)> = snapshot
                .quad_ids_for_pattern(None, Some(p), None, GraphSelector::All)
                .filter(|q| Some(q[3]) != skip)
                .map(|[s, _, o, _]| (s, o))
                .collect();
            out.extend(derived.iter().filter(|t| t[1] == p).map(|t| (t[0], t[2])));
            out
        };
        let pairs = |p: Option<TermId>| -> Vec<(TermId, TermId)> {
            let mut out: Vec<(TermId, TermId)> = raw_pairs(p)
                .into_iter()
                .map(|(s, o)| (canon(s), canon(o)))
                .collect();
            out.sort_unstable();
            out.dedup();
            out
        };
        // Equivalence is a sub class (property) both ways (scm-eqc1/2, scm-eqp1/2).
        let both = |sub: Option<TermId>, eq: Option<TermId>| {
            // A self edge stays: after owl:sameAs canonicalisation it may stand for edges
            // between members of one class, each a sub class of the others.
            let mut edges = pairs(sub);
            // A literal is no class or property: `c owl:equivalentClass "x"` relates nothing.
            let term = |t: TermId| matches!(t.kind(), Kind::Iri | Kind::Blank);
            for (a, b) in pairs(eq) {
                if term(a) && term(b) {
                    edges.push((a, b));
                    edges.push((b, a));
                }
            }
            edges
        };
        // The subjects of `x rdf:type c`.
        let typed = |c: Option<TermId>| -> Vec<TermId> {
            let (Some(t), Some(c)) = (v.rdf_type, c) else {
                return Vec::new();
            };
            let mut out: Vec<TermId> = snapshot
                .quad_ids_for_pattern(None, Some(t), Some(c), GraphSelector::All)
                .filter(|q| Some(q[3]) != skip)
                .map(|q| q[0])
                .collect();
            out.extend(
                derived
                    .iter()
                    .filter(|d| d[1] == t && d[2] == c)
                    .map(|d| d[0]),
            );
            for x in &mut out {
                *x = canon(*x);
            }
            out.sort_unstable();
            out.dedup();
            out
        };
        // scm-cls, scm-op, scm-dp. A declared class is a sub class of owl:Thing, and
        // owl:Nothing of it, as edges of the hierarchy; its being its own sub class and
        // equivalent is written apart (`declared`).
        let mut classes = typed(v.class);
        let thing_nothing: Vec<TermId> = [v.thing, v.nothing]
            .into_iter()
            .flatten()
            .map(canon)
            .collect();
        if !classes.is_empty() {
            classes.extend(&thing_nothing);
            classes.sort_unstable();
            classes.dedup();
        }
        let mut properties = typed(v.object_property);
        properties.extend(typed(v.datatype_property));
        properties.sort_unstable();
        properties.dedup();
        let super_properties = closure(&both(v.sub_property, v.equivalent_property), cancelled)?;
        // Each of these reads all of `derived`: tens of millions of triples, with owl:sameAs.
        if cancelled() {
            return None;
        }
        let restrictions = Restrictions::read(snapshot, skip, v, &pairs, &raw_pairs, canon);
        if cancelled() {
            return None;
        }
        let mut class_edges = both(v.sub_class, v.equivalent_class);
        class_edges.extend(restrictions.class_edges());
        if let (Some(thing), Some(nothing)) = (v.thing.map(canon), v.nothing.map(canon)) {
            for &c in &classes {
                if c != thing {
                    class_edges.push((c, thing));
                }
                if c != nothing {
                    class_edges.push((nothing, c));
                }
            }
        }
        let declared = [classes, properties];
        // scm-hv, scm-svf1/2, scm-avf1/2 read the closed hierarchies and add to them.
        let super_classes = loop {
            let super_classes = closure(&class_edges, cancelled)?;
            let more = restrictions.sub_classes(&super_classes, &super_properties, shared);
            let known: HashSet<(TermId, TermId)> = class_edges.iter().copied().collect();
            let more: Vec<(TermId, TermId)> =
                more.into_iter().filter(|e| !known.contains(e)).collect();
            if more.is_empty() {
                break super_classes;
            }
            class_edges.extend(more);
        };
        if cancelled() {
            return None;
        }
        let mut inverses: HashMap<TermId, Vec<TermId>> = HashMap::new();
        for (p, q) in pairs(v.inverse_of) {
            inverses.entry(p).or_default().push(q);
            inverses.entry(q).or_default().push(p);
        }
        for p in typed(v.symmetric) {
            inverses.entry(p).or_default().push(p);
        }
        let transitive = typed(v.transitive);
        let (functional, inverse_functional) = (typed(v.functional), typed(v.inverse_functional));
        let classes_with = |of: Option<TermId>| {
            let mut m: HashMap<TermId, Vec<TermId>> = HashMap::new();
            for (p, c) in pairs(of) {
                m.entry(p).or_default().push(c);
            }
            m
        };
        if cancelled() {
            return None;
        }
        let (mut domains, ranges) = (classes_with(v.domain), classes_with(v.range));
        // Under OWL 2 RL, before cls-svf2 adds to them: the stated domains, for scm-dom.
        let stated_domains = if v.equivalent_class.is_some() {
            domains.clone()
        } else {
            HashMap::new()
        };
        let thing = v.thing.map(canon);
        // cls-svf2: a restriction to some value of owl:Thing is a domain of its property.
        for &(r, p, c) in &restrictions.some {
            if Some(c) == thing {
                domains.entry(p).or_default().push(r);
            }
        }
        let mut valued: HashMap<TermId, Vec<(TermId, TermId)>> = HashMap::new();
        let mut values: HashMap<TermId, Vec<(TermId, TermId)>> = HashMap::new();
        for &(r, p, value) in &restrictions.value {
            valued.entry(p).or_default().push((value, r));
            values.entry(r).or_default().push((p, value));
        }
        // Classes and their super classes, sorted, each once.
        let with_supers = |cs: &mut Vec<TermId>| {
            let mut more = Vec::new();
            for c in cs.iter() {
                if let Some(s) = super_classes.get(c) {
                    more.extend(s.iter().copied());
                }
            }
            cs.extend(more);
            cs.sort_unstable();
            cs.dedup();
        };
        let mut properties: HashSet<TermId> = HashSet::new();
        properties.extend(super_properties.keys().copied());
        properties.extend(domains.keys().copied());
        properties.extend(ranges.keys().copied());
        properties.extend(inverses.keys().copied());
        properties.extend(valued.keys().copied());
        let mut plans = HashMap::new();
        let mut scoped = Vec::new();
        for p in properties {
            let supers = super_properties.get(&p).cloned().unwrap_or_default();
            let mut domain = Vec::new();
            let mut range = Vec::new();
            let mut inverse = Vec::new();
            let mut values = Vec::new();
            for q in std::iter::once(&p).chain(&supers) {
                values.extend(valued.get(q).into_iter().flatten().copied());
                domain.extend(domains.get(q).into_iter().flatten().copied());
                range.extend(ranges.get(q).into_iter().flatten().copied());
                for &r in inverses.get(q).into_iter().flatten() {
                    inverse.push(r);
                    inverse.extend(super_properties.get(&r).into_iter().flatten().copied());
                }
            }
            with_supers(&mut domain);
            with_supers(&mut range);
            // scm-dom1/2, scm-rng1/2 (OWL 2 RL only: Jena's RDFS rules write none).
            if let (Some(d), Some(rg)) = (v.domain, v.range)
                && v.equivalent_class.is_some()
            {
                let mut stated = Vec::new();
                for q in std::iter::once(&p).chain(&supers) {
                    stated.extend(stated_domains.get(q).into_iter().flatten().copied());
                }
                with_supers(&mut stated);
                scoped.extend(stated.into_iter().map(|c| [p, d, c]));
                scoped.extend(range.iter().map(|&c| [p, rg, c]));
            }
            inverse.sort_unstable();
            inverse.dedup();
            values.sort_unstable();
            values.dedup();
            plans.insert(
                p,
                Plan {
                    supers,
                    domain,
                    range,
                    inverse,
                    values,
                },
            );
        }
        let mut joins = restrictions.joins(thing);
        joins.functional(functional, inverse_functional);
        Some(Self {
            super_classes,
            super_properties,
            declared,
            scoped,
            plans,
            transitive,
            values,
            joins,
        })
    }

    /// The properties whose triples imply something.
    fn read(&self, v: Vocab) -> Vec<TermId> {
        let mut ps: Vec<TermId> = self.plans.keys().copied().collect();
        if let Some(t) = v.rdf_type
            && (!self.super_classes.is_empty() || !self.values.is_empty())
            && !self.plans.contains_key(&t)
        {
            ps.push(t);
        }
        ps
    }

    /// What one triple implies, by one step of rdfs2, 3, 7 and 9.
    fn apply(&self, v: Vocab, [x, p, y]: [TermId; 3], out: &mut Vec<[TermId; 3]>) {
        let resource = |t: TermId| matches!(t.kind(), Kind::Iri | Kind::Blank);
        if let Some(plan) = self.plans.get(&p) {
            for &q in &plan.supers {
                out.push([x, q, y]);
                // A sub property of rdf:type: the types' super classes too, so that every
                // rdf:type triple out of here is closed over them.
                if Some(q) == v.rdf_type
                    && let Some(cs) = self.super_classes.get(&y)
                {
                    out.extend(cs.iter().map(|&c| [x, q, c]));
                }
            }
            if let Some(t) = v.rdf_type {
                out.extend(plan.domain.iter().map(|&c| [x, t, c]));
                if resource(y) {
                    out.extend(plan.range.iter().map(|&c| [y, t, c]));
                }
            }
            // prp-inv1/2, prp-symp: a literal is no subject.
            if resource(y) {
                out.extend(plan.inverse.iter().map(|&q| [y, q, x]));
            }
            // cls-hv2.
            if let Some(t) = v.rdf_type {
                out.extend(
                    plan.values
                        .iter()
                        .filter(|&&(value, _)| value == y)
                        .map(|&(_, r)| [x, t, r]),
                );
            }
        }
        if Some(p) == v.rdf_type {
            if let Some(cs) = self.super_classes.get(&y) {
                out.extend(cs.iter().map(|&c| [x, p, c]));
            }
            // cls-hv1.
            if let Some(pvs) = self.values.get(&y) {
                out.extend(pvs.iter().map(|&(q, value)| [x, q, value]));
            }
        }
    }

    /// rdfs5 and rdfs11: the closed hierarchies as triples.
    ///
    /// OWL 2 RL scm-eqc2 and scm-eqp2: classes (properties) each a sub class (property) of
    /// the other are equivalent, a class on a cycle to itself too, as it is its own sub class.
    /// scm-cls, scm-op, scm-dp: declared classes and properties are their own sub class
    /// (property) and equivalent. scm-dom1/2 and scm-rng1/2: a property's domains and ranges
    /// with those of its super properties and their super classes (`scoped`).
    fn closures(&self, v: Vocab, out: &mut Vec<[TermId; 3]>) {
        out.extend(&self.scoped);
        for ((supers, p, eq), declared) in [
            (&self.super_classes, v.sub_class, v.equivalent_class),
            (
                &self.super_properties,
                v.sub_property,
                v.equivalent_property,
            ),
        ]
        .into_iter()
        .zip(&self.declared)
        {
            for &c in declared {
                out.extend(p.map(|p| [c, p, c]));
                out.extend(eq.map(|eq| [c, eq, c]));
            }
            if let Some(p) = p {
                out.extend(
                    supers
                        .iter()
                        .flat_map(|(&a, bs)| bs.iter().map(move |&b| [a, p, b])),
                );
            }
            if let Some(eq) = eq {
                let mutual = |a: TermId, b: TermId| {
                    supers
                        .get(&b)
                        .is_some_and(|cs| cs.binary_search(&a).is_ok())
                };
                out.extend(supers.iter().flat_map(|(&a, bs)| {
                    bs.iter()
                        .filter(move |&&b| mutual(a, b))
                        .map(move |&b| [a, eq, b])
                }));
            }
        }
    }

    /// What the data's triples and `derived` imply, by one step.
    fn pass(
        &self,
        snapshot: &Snapshot,
        skip: Option<TermId>,
        v: Vocab,
        derived: &[[TermId; 3]],
        cancelled: &dyn Fn() -> bool,
    ) -> Vec<[TermId; 3]> {
        let mut out: Vec<[TermId; 3]> = self
            .read(v)
            .par_iter()
            .flat_map_iter(|&p| {
                let mut out = Vec::new();
                for [x, _, y, g] in
                    snapshot.quad_ids_for_pattern(None, Some(p), None, GraphSelector::All)
                {
                    if Some(g) != skip {
                        self.apply(v, [x, p, y], &mut out);
                    }
                }
                out
            })
            .collect();
        out.extend(self.step(v, derived));
        self.closures(v, &mut out);
        self.transitive(
            snapshot,
            skip,
            derived,
            &self.transitive,
            &mut out,
            cancelled,
        );
        self.joins
            .run(snapshot, skip, v, derived, &mut out, cancelled);
        out
    }

    /// prp-trp: the closure of each of `properties` that is transitive, over its triples in
    /// the data and in `derived` (sorted).
    fn transitive(
        &self,
        snapshot: &Snapshot,
        skip: Option<TermId>,
        derived: &[[TermId; 3]],
        properties: &[TermId],
        out: &mut Vec<[TermId; 3]>,
        cancelled: &dyn Fn() -> bool,
    ) {
        for &p in properties {
            if self.transitive.binary_search(&p).is_err() {
                continue;
            }
            let mut edges: Vec<(TermId, TermId)> = snapshot
                .quad_ids_for_pattern(None, Some(p), None, GraphSelector::All)
                .filter(|q| Some(q[3]) != skip)
                .map(|q| (q[0], q[2]))
                .collect();
            edges.extend(derived.iter().filter(|d| d[1] == p).map(|d| (d[0], d[2])));
            // The caller asks again and stops.
            let Some(reached) = closure(&edges, cancelled) else {
                return;
            };
            for (a, bs) in reached {
                // A literal is no subject, so it ends a chain.
                if matches!(a.kind(), Kind::Iri | Kind::Blank) {
                    out.extend(bs.into_iter().map(|b| [a, p, b]));
                }
            }
        }
    }

    /// What derived `triples` imply, by one step, beyond what the step that derived them did.
    ///
    /// That step already gave everything a derived triple implies through the plans, unless
    /// its property is rdf:type or a schema property with a plan of its own: a triple
    /// `x q y` comes from `x p y` with `p` a sub property of `q`, whose plan holds all of
    /// `q`'s; and an rdf:type triple comes out closed over the super classes. So only
    /// triples of those few properties are read again (none, in most stores).
    fn step(&self, v: Vocab, triples: &[[TermId; 3]]) -> Vec<[TermId; 3]> {
        // Under OWL 2 RL an inverse can lead to a property whose plan the first one's lacks:
        // every derived triple is read again.
        let owl = v.owl();
        let again = |p: TermId| {
            owl || ((Some(p) == v.rdf_type || v.is_schema(p)) && self.plans.contains_key(&p))
        };
        triples
            .par_chunks(4096)
            .flat_map_iter(|chunk| {
                let mut out = Vec::new();
                for &t in chunk {
                    if again(t[1]) {
                        self.apply(v, t, &mut out);
                    }
                }
                out
            })
            .collect()
    }
}

/// What the RDFS rules derive from the quads of `snapshot` in every graph but `skip`, as
/// (subject, predicate, object) ids, sorted, each once, without the triples any graph but
/// `skip` already holds.
pub fn rdfs(snapshot: &Snapshot, skip: Option<TermId>) -> Vec<[TermId; 3]> {
    entail(snapshot, skip, Regime::Rdfs)
}

/// [`rdfs`] under a regime.
pub fn entail(snapshot: &Snapshot, skip: Option<TermId>, regime: Regime) -> Vec<[TermId; 3]> {
    entail_until(snapshot, skip, regime, &|| false).expect("never cancelled")
}

/// [`entail`], asking `cancelled` between the rules and inside the ones that join many
/// triples (#647): `None` once it says so.
pub fn entail_until(
    snapshot: &Snapshot,
    skip: Option<TermId>,
    regime: Regime,
    cancelled: &dyn Fn() -> bool,
) -> Option<Vec<[TermId; 3]>> {
    let v = vocab(snapshot, regime);
    let mut schema = Schema::compile(snapshot, skip, v, &[], &|t| t, &|_| false, cancelled)?;
    // Everything derived so far, sorted, each once.
    let mut all: Vec<[TermId; 3]> = Vec::new();
    let mut new = schema.pass(snapshot, skip, v, &[], cancelled);
    let mut same = Same::default();
    // New owl:sameAs classes: the schema compiled again on the canonical terms.
    let mut stale = same.round(snapshot, skip, v, &[], &[], &mut new, cancelled);
    loop {
        if cancelled() {
            return None;
        }
        if !sort_until(&mut new, cancelled) {
            return None;
        }
        let delta = minus(new, &all);
        if delta.is_empty() {
            break;
        }
        all = union(all, &delta);
        if cancelled() {
            return None;
        }
        let mut rewritten = Vec::new();
        stale |= same.round(snapshot, skip, v, &all, &delta, &mut rewritten, cancelled);
        if cancelled() {
            return None;
        }
        if stale || delta.iter().any(|t| v.is_schema_triple(t)) {
            stale = false;
            let compiled = Schema::compile(
                snapshot,
                skip,
                v,
                &all,
                &|t| same.canon(t),
                &|t| same.of.contains_key(&t),
                cancelled,
            )?;
            if compiled != schema {
                // A new schema: every triple read again with it.
                schema = compiled;
                new = schema.pass(snapshot, skip, v, &all, cancelled);
                new.extend(rewritten);
                continue;
            }
        }
        new = schema.step(v, &delta);
        new.extend(rewritten);
        if !schema.transitive.is_empty() {
            let mut touched: Vec<TermId> = delta.iter().map(|t| t[1]).collect();
            touched.sort_unstable();
            touched.dedup();
            schema.transitive(snapshot, skip, &all, &touched, &mut new, cancelled);
        }
        if schema.joins.read(v, &delta) {
            schema
                .joins
                .run_delta(snapshot, skip, v, &all, &delta, &mut new, cancelled);
        }
    }
    // Inside the fixpoint the rules read generalized triples (a blank node property from a
    // chain, say); only RDF triples are written.
    all.retain(rdf);
    Some(finish(snapshot, skip, all))
}

/// How a kept OWL 2 RL entailment follows a transaction that only adds triples (#187):
/// graph `into` of `snapshot` held the entailment of the other graphs before `added` were
/// written to them (the snapshot shows them). Returns what `into` gains: what the rules
/// derive from the added triples, round by round with the schema fixed, that no graph holds.
///
/// `None` (entail again) when the writes or what they lead to touch the schema (a schema
/// triple, a list cell, a class or property declaration) or owl:sameAs: an owl:sameAs
/// triple derived, or a term of a triple in an owl:sameAs class, whose triples the
/// equality rules copy to every member.
pub fn owl_rl_added(snapshot: &Snapshot, added: &[[TermId; 3]]) -> Option<Vec<[TermId; 3]>> {
    let v = vocab(snapshot, Regime::OwlRl);
    let never = || false;
    let declared = [v.class, v.object_property, v.datatype_property];
    let in_class = |t: TermId| {
        v.same_as.is_some_and(|same| {
            snapshot
                .quad_ids_for_pattern(Some(t), Some(same), None, GraphSelector::All)
                .next()
                .is_some()
                || snapshot
                    .quad_ids_for_pattern(None, Some(same), Some(t), GraphSelector::All)
                    .next()
                    .is_some()
        })
    };
    let simple = |t: &[TermId; 3]| {
        !v.is_schema_triple(t)
            && Some(t[1]) != v.same_as
            && Some(t[1]) != v.first
            && Some(t[1]) != v.rest
            && !(Some(t[1]) == v.rdf_type && declared.contains(&Some(t[2])))
            && !t.iter().any(|&x| in_class(x))
    };
    if !added.iter().all(simple) {
        return None;
    }
    let schema = Schema::compile(snapshot, None, v, &[], &|t| t, &|_| false, &never)?;
    let held = |&[s, p, o]: &[TermId; 3]| {
        snapshot
            .quad_ids_for_pattern(Some(s), Some(p), Some(o), GraphSelector::All)
            .next()
            .is_some()
    };
    let mut all: Vec<[TermId; 3]> = Vec::new();
    let mut delta: Vec<[TermId; 3]> = added.to_vec();
    delta.sort_unstable();
    delta.dedup();
    loop {
        let mut new = schema.step(v, &delta);
        if !schema.transitive.is_empty() {
            let mut touched: Vec<TermId> = delta.iter().map(|t| t[1]).collect();
            touched.sort_unstable();
            touched.dedup();
            schema.transitive(snapshot, None, &all, &touched, &mut new, &never);
        }
        if schema.joins.read(v, &delta) {
            // `all` lacks the added triples, which the snapshot holds.
            schema
                .joins
                .run_delta(snapshot, None, v, &all, &delta, &mut new, &never);
        }
        new.par_sort_unstable();
        new.dedup();
        let mut fresh = minus(new, &all);
        fresh.retain(|t| !held(t));
        if fresh.is_empty() {
            break;
        }
        if !fresh.iter().all(simple) {
            return None;
        }
        all = union(all, &fresh);
        delta = fresh;
    }
    all.retain(rdf);
    Some(all)
}

/// `triples` sorted, each once; false once `cancelled`. A round can derive tens of millions
/// of triples (owl:sameAs over a class of thousands), seconds to sort: they are sorted in
/// parts, asking between, and the sorted parts merged.
fn sort_until(triples: &mut Vec<[TermId; 3]>, cancelled: &dyn Fn() -> bool) -> bool {
    const PART: usize = 1 << 22;
    if triples.len() > 2 * PART {
        for part in triples.chunks_mut(PART) {
            if cancelled() {
                return false;
            }
            part.par_sort_unstable();
        }
        // A stable sort merges sorted runs.
        triples.par_sort();
    } else {
        triples.par_sort_unstable();
    }
    triples.dedup();
    !cancelled()
}

/// An RDF triple: a resource subject and an IRI predicate.
fn rdf(t: &[TermId; 3]) -> bool {
    matches!(t[0].kind(), Kind::Iri | Kind::Blank) && t[1].kind() == Kind::Iri
}

/// What a kept entailment changes by: triples its graph gains and loses.
#[derive(Debug, Default)]
pub struct Change {
    pub insert: Vec<[TermId; 3]>,
    pub remove: Vec<[TermId; 3]>,
}

/// How a kept entailment follows writes (#187): graph `into` of the store `snapshot` holds
/// held the entailment of the other graphs before triples `added` and `removed` were written
/// to them (the snapshot shows the writes, not yet the entailment's change). `None` when the
/// writes change the schema, or the change touches derived schema: then entail again with
/// [`rdfs`].
///
/// With the schema fixed every rule reads one triple, so a triple is entailed when a chain
/// of rules leads to it from an asserted one. Removals go by delete and rederive: what the
/// removed triples lead to in `into` is set aside, then what of it another triple still
/// leads to comes back. Additions go by what they lead to that no graph holds.
pub fn rdfs_changed(
    snapshot: &Snapshot,
    into: TermId,
    added: &[[TermId; 3]],
    removed: &[[TermId; 3]],
) -> Option<Change> {
    let v = vocab(snapshot, Regime::Rdfs);
    if added.iter().chain(removed).any(|t| v.is_schema(t[1])) {
        return None;
    }
    // With the schema the entailment holds too (derived, as by a sub property of
    // rdfs:subClassOf).
    let schema = Schema::compile(snapshot, None, v, &[], &|t| t, &|_| false, &|| false)?;
    // Whether a triple is in a graph other than `into`, and whether in `into`.
    let held = |&[s, p, o]: &[TermId; 3]| {
        let (mut asserted, mut inferred) = (false, false);
        for q in snapshot.quad_ids_for_pattern(Some(s), Some(p), Some(o), GraphSelector::All) {
            if q[3] == into {
                inferred = true;
            } else {
                asserted = true;
            }
        }
        (asserted, inferred)
    };
    // Delete: the removed triples no graph holds any more, and what they lead to in `into`.
    let mut gone: HashSet<[TermId; 3]> = HashSet::new();
    let mut frontier: Vec<[TermId; 3]> = removed.iter().filter(|t| !held(t).0).copied().collect();
    gone.extend(frontier.iter().copied());
    let mut out = Vec::new();
    while let Some(t) = frontier.pop() {
        schema.apply(v, t, &mut out);
        for o in out.drain(..) {
            if !gone.contains(&o) && held(&o) == (false, true) {
                if v.is_schema(o[1]) {
                    return None;
                }
                gone.insert(o);
                frontier.push(o);
            }
        }
    }
    // Rederive: what of it a triple outside it still leads to, and what that leads to.
    let mut back: HashSet<[TermId; 3]> = HashSet::new();
    if !gone.is_empty() {
        let inverse = Inverse::of(&schema);
        let alive = |q: [TermId; 4]| q[3] != into || !gone.contains(&[q[0], q[1], q[2]]);
        let any = |s: Option<TermId>, p: TermId, o: Option<TermId>| {
            snapshot
                .quad_ids_for_pattern(s, Some(p), o, GraphSelector::All)
                .any(alive)
        };
        let candidates: Vec<[TermId; 3]> = gone.iter().copied().collect();
        let mut queue: Vec<[TermId; 3]> = candidates
            .into_par_iter()
            .filter(|&d| inverse.leads_to(v, d, &any, snapshot, &alive))
            .collect();
        back.extend(queue.iter().copied());
        while let Some(t) = queue.pop() {
            schema.apply(v, t, &mut out);
            for o in out.drain(..) {
                if gone.contains(&o) && back.insert(o) {
                    queue.push(o);
                }
            }
        }
    }
    let lost = |t: &[TermId; 3]| gone.contains(t) && !back.contains(t);
    // Add: what the added triples lead to that no graph holds after the removals.
    let present = |t: &[TermId; 3]| {
        let (asserted, inferred) = held(t);
        asserted || back.contains(t) || (inferred && !lost(t))
    };
    let mut all: Vec<[TermId; 3]> = Vec::new();
    let mut new = Vec::new();
    for &t in added {
        schema.apply(v, t, &mut new);
    }
    loop {
        new.par_sort_unstable();
        new.dedup();
        let mut delta = minus(new, &all);
        delta.retain(|t| !present(t));
        if delta.is_empty() {
            break;
        }
        if delta.iter().any(|t| v.is_schema(t[1])) {
            return None;
        }
        all = union(all, &delta);
        new = schema.step(v, &delta);
    }
    let mut change = Change::default();
    // Back, but not in `into`: a removed triple that is still entailed.
    change
        .insert
        .extend(back.iter().filter(|t| !held(t).1).copied());
    change.insert.extend(all.iter().copied());
    change.remove.extend(
        gone.iter()
            .filter(|t| lost(t) && all.binary_search(t).is_err() && held(t).1)
            .copied(),
    );
    change.insert.retain(rdf);
    Some(change)
}

/// The schema read backwards: what leads to a triple, in one rule.
struct Inverse {
    /// For a property, its sub properties.
    sub_properties: HashMap<TermId, Vec<TermId>>,
    /// For a class, its super classes (sorted).
    super_classes: HashMap<TermId, Vec<TermId>>,
    /// For a class, the properties whose subjects get it, and whose objects.
    domain_of: HashMap<TermId, Vec<TermId>>,
    range_of: HashMap<TermId, Vec<TermId>>,
}

impl Inverse {
    fn of(schema: &Schema) -> Self {
        let mut me = Self {
            sub_properties: HashMap::new(),
            super_classes: schema.super_classes.clone(),
            domain_of: HashMap::new(),
            range_of: HashMap::new(),
        };
        for (&p, plan) in &schema.plans {
            for &q in &plan.supers {
                me.sub_properties.entry(q).or_default().push(p);
            }
            for &c in &plan.domain {
                me.domain_of.entry(c).or_default().push(p);
            }
            for &c in &plan.range {
                me.range_of.entry(c).or_default().push(p);
            }
        }
        me
    }

    /// Whether a rule leads to `[a, q, b]` from a triple `any` finds (subject, predicate,
    /// object; `None` for any) or, for rdfs9, from a quad of `snapshot` that is `alive`: the
    /// inverse of each case of [`Schema::apply`].
    fn leads_to(
        &self,
        v: Vocab,
        [a, q, b]: [TermId; 3],
        any: &impl Fn(Option<TermId>, TermId, Option<TermId>) -> bool,
        snapshot: &Snapshot,
        alive: &impl Fn([TermId; 4]) -> bool,
    ) -> bool {
        let none = Vec::new();
        let subs = |p: TermId| self.sub_properties.get(&p).unwrap_or(&none);
        if subs(q).iter().any(|&p| any(Some(a), p, Some(b))) {
            return true;
        }
        let Some(t) = v.rdf_type else {
            return false;
        };
        if q != t {
            return false;
        }
        // rdfs9, from a type of `a` (by rdf:type or a sub property of it) under `b`: the
        // subject's types are few, the sub classes of a class near the root many.
        let under = |c: TermId| {
            self.super_classes
                .get(&c)
                .is_some_and(|s| s.binary_search(&b).is_ok())
        };
        std::iter::once(t)
            .chain(subs(t).iter().copied())
            .any(|p| {
                snapshot
                    .quad_ids_for_pattern(Some(a), Some(p), None, GraphSelector::All)
                    .any(|q| under(q[2]) && alive(q))
            })
            // rdfs2 and rdfs3.
            || self
                .domain_of
                .get(&b)
                .is_some_and(|ps| ps.iter().any(|&p| any(Some(a), p, None)))
            || (matches!(a.kind(), Kind::Iri | Kind::Blank)
                && self
                    .range_of
                    .get(&b)
                    .is_some_and(|ps| ps.iter().any(|&p| any(None, p, Some(a)))))
    }
}

/// The terms the rules read.
pub const VOCABULARY: [NamedNodeRef<'static>; 13] = [
    rdf::TYPE,
    rdfs::SUB_CLASS_OF,
    rdfs::SUB_PROPERTY_OF,
    rdfs::DOMAIN,
    rdfs::RANGE,
    owl::EQUIVALENT_CLASS,
    owl::EQUIVALENT_PROPERTY,
    owl::INVERSE_OF,
    owl::SYMMETRIC_PROPERTY,
    owl::TRANSITIVE_PROPERTY,
    // Written by prp-fp and prp-ifp.
    owl::SAME_AS,
    // Written by scm-cls.
    owl::THING,
    owl::NOTHING,
];

/// The ids of the RDFS terms.
fn vocab(snapshot: &Snapshot, regime: Regime) -> Vocab {
    let id = |n: NamedNodeRef<'_>| snapshot.term_id(n.into());
    let owl = |n: NamedNodeRef<'_>| if regime == Regime::OwlRl { id(n) } else { None };
    Vocab {
        rdf_type: id(rdf::TYPE),
        sub_class: id(rdfs::SUB_CLASS_OF),
        sub_property: id(rdfs::SUB_PROPERTY_OF),
        domain: id(rdfs::DOMAIN),
        range: id(rdfs::RANGE),
        equivalent_class: owl(owl::EQUIVALENT_CLASS),
        equivalent_property: owl(owl::EQUIVALENT_PROPERTY),
        inverse_of: owl(owl::INVERSE_OF),
        symmetric: owl(owl::SYMMETRIC_PROPERTY),
        transitive: owl(owl::TRANSITIVE_PROPERTY),
        on_property: owl(owl::ON_PROPERTY),
        has_value: owl(owl::HAS_VALUE),
        some_values_from: owl(owl::SOME_VALUES_FROM),
        all_values_from: owl(owl::ALL_VALUES_FROM),
        intersection_of: owl(owl::INTERSECTION_OF),
        union_of: owl(owl::UNION_OF),
        property_chain_axiom: owl(owl::PROPERTY_CHAIN_AXIOM),
        first: owl(rdf::FIRST),
        rest: owl(rdf::REST),
        nil: owl(rdf::NIL),
        thing: owl(owl::THING),
        nothing: owl(owl::NOTHING),
        class: owl(owl::CLASS),
        object_property: owl(owl::OBJECT_PROPERTY),
        datatype_property: owl(owl::DATATYPE_PROPERTY),
        same_as: owl(owl::SAME_AS),
        functional: owl(owl::FUNCTIONAL_PROPERTY),
        inverse_functional: owl(owl::INVERSE_FUNCTIONAL_PROPERTY),
        max_cardinality: owl(owl::MAX_CARDINALITY),
        max_qualified_cardinality: owl(owl::MAX_QUALIFIED_CARDINALITY),
        on_class: owl(owl::ON_CLASS),
        has_key: owl(owl::HAS_KEY),
        one_of: owl(owl::ONE_OF),
    }
}

/// The triples of `a` that `b` lacks; both sorted.
fn minus(a: Vec<[TermId; 3]>, b: &[[TermId; 3]]) -> Vec<[TermId; 3]> {
    let mut i = 0;
    a.into_iter()
        .filter(|t| {
            while i < b.len() && b[i] < *t {
                i += 1;
            }
            b.get(i) != Some(t)
        })
        .collect()
}

/// Both, sorted; they share no triple.
fn union(a: Vec<[TermId; 3]>, b: &[[TermId; 3]]) -> Vec<[TermId; 3]> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if j == b.len() || (i < a.len() && a[i] < b[j]) {
            out.push(a[i]);
            i += 1;
        } else {
            out.push(b[j]);
            j += 1;
        }
    }
    out
}

/// Without what the data holds (`derived` sorted, each once): by a point lookup per triple,
/// or, when they are many against the data, by one pass over the data in (s, p, o) order
/// beside them.
fn finish(
    snapshot: &Snapshot,
    skip: Option<TermId>,
    mut derived: Vec<[TermId; 3]>,
) -> Vec<[TermId; 3]> {
    if derived.len().saturating_mul(8) < snapshot.len() {
        let asserted: Vec<bool> = derived
            .par_iter()
            .map(|&[s, p, o]| {
                snapshot
                    .quad_ids_for_pattern(Some(s), Some(p), Some(o), GraphSelector::All)
                    .any(|q| Some(q[3]) != skip)
            })
            .collect();
        return derived
            .into_iter()
            .zip(asserted)
            .filter_map(|(t, a)| (!a).then_some(t))
            .collect();
    }
    // Every graph's quads in SPOG order.
    let mut data = snapshot
        .quad_ids_for_pattern(None, None, None, GraphSelector::All)
        .filter(|q| Some(q[3]) != skip)
        .map(|[s, p, o, _]| [s, p, o])
        .peekable();
    derived.retain(|t| {
        while data.next_if(|d| d < t).is_some() {}
        data.peek() != Some(t)
    });
    derived
}

#[cfg(test)]
mod tests {
    use crate::{Store, StoreError};
    use oxigraph::model::vocab::{rdf, rdfs};
    use oxigraph::model::{GraphNameRef, NamedNodeRef, QuadRef};

    const G: NamedNodeRef<'_> = NamedNodeRef::new_unchecked("urn:inferred");

    fn n(s: &str) -> NamedNodeRef<'_> {
        NamedNodeRef::new_unchecked(s)
    }

    #[test]
    fn entailment_in_a_transaction_sees_and_replaces_its_own_writes() {
        let store = Store::new();
        let d = GraphNameRef::DefaultGraph;
        store
            .transaction(|t| {
                t.insert(QuadRef::new(n("urn:A"), rdfs::SUB_CLASS_OF, n("urn:B"), d));
                // A stale triple in the inferred graph, written by this transaction.
                t.insert(QuadRef::new(n("urn:old"), rdf::TYPE, n("urn:B"), G));
                t.insert(QuadRef::new(n("urn:x"), rdf::TYPE, n("urn:A"), d));
                assert_eq!(t.entail_rdfs(G).triples, 1);
                Ok::<_, StoreError>(())
            })
            .unwrap();
        let inferred: Vec<_> = store
            .quads_for_pattern(None, None, None, Some(G.into()))
            .map(|q| q.to_string())
            .collect();
        assert_eq!(
            inferred,
            ["<urn:x> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <urn:B> <urn:inferred>"]
        );
        // Removed in the store, then entailed again in one transaction: the triple leaves.
        store
            .transaction(|t| {
                t.remove(QuadRef::new(n("urn:x"), rdf::TYPE, n("urn:A"), d));
                assert_eq!(t.entail_rdfs(G).triples, 0);
                Ok::<_, StoreError>(())
            })
            .unwrap();
        assert_eq!(
            store
                .quads_for_pattern(None, None, None, Some(G.into()))
                .count(),
            0
        );
    }

    #[test]
    fn a_stopped_entailment_leaves_the_transactions_writes_as_they_were() {
        use super::Regime;
        use std::cell::Cell;
        let d = GraphNameRef::DefaultGraph;
        // Enough triples for the changes to be noted in several parts, each asking.
        let write = |t: &mut crate::Transaction<'_>| {
            t.insert(QuadRef::new(n("urn:A"), rdfs::SUB_CLASS_OF, n("urn:B"), d));
            for i in 0..200_000 {
                let x = format!("urn:x{i}");
                t.insert(QuadRef::new(n(&x), rdf::TYPE, n("urn:A"), d));
            }
            // This transaction's own write in the graph entailed into.
            t.insert(QuadRef::new(n("urn:old"), rdf::TYPE, n("urn:B"), G));
        };
        // How often an entailment that runs to the end asks.
        let asked = Cell::new(0);
        let store = Store::new();
        store
            .transaction(|t| {
                write(t);
                let count = || {
                    asked.set(asked.get() + 1);
                    false
                };
                assert!(t.entail_until(G, Regime::Rdfs, &count).is_some());
                Ok::<_, StoreError>(())
            })
            .unwrap();
        assert_eq!(
            store
                .quads_for_pattern(None, None, None, Some(G.into()))
                .count(),
            200_000
        );
        // Stopped at its last question, after every change is noted: the transaction commits
        // what it wrote itself, and no entailment.
        for last in [asked.get(), asked.get() - 1] {
            let store = Store::new();
            let calls = Cell::new(0);
            store
                .transaction(|t| {
                    write(t);
                    let stop = || {
                        calls.set(calls.get() + 1);
                        calls.get() >= last
                    };
                    assert!(t.entail_until(G, Regime::Rdfs, &stop).is_none());
                    Ok::<_, StoreError>(())
                })
                .unwrap();
            let inferred: Vec<_> = store
                .quads_for_pattern(None, None, None, Some(G.into()))
                .map(|q| q.to_string())
                .collect();
            assert_eq!(
                inferred,
                [
                    "<urn:old> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <urn:B> <urn:inferred>"
                ],
                "stopped at question {last}"
            );
            assert_eq!(store.len(), 200_002, "stopped at question {last}");
        }
    }
}
