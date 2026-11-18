//! The OWL 2 RL consistency rules (#187): what makes the data contradict itself.
//!
//! Like Jena's validity report, the rules write nothing: entailment goes on with an
//! inconsistent store, and [`inconsistencies`] lists what is wrong. They read the store as
//! it is, in all graphs, so they see an entailment written into one of them: run them after
//! entailing, or they miss what the rules would derive (a type through a sub class, an
//! owl:sameAs from a functional property).

use super::{Regime, list, vocab};
use crate::{GraphSelector, Kind, Snapshot, TermId};
use oxigraph::model::{NamedNodeRef, Term};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};

const fn owl(name: &'static str) -> NamedNodeRef<'static> {
    NamedNodeRef::new_unchecked(name)
}
const NOTHING: NamedNodeRef<'_> = owl("http://www.w3.org/2002/07/owl#Nothing");
const DISJOINT_WITH: NamedNodeRef<'_> = owl("http://www.w3.org/2002/07/owl#disjointWith");
const COMPLEMENT_OF: NamedNodeRef<'_> = owl("http://www.w3.org/2002/07/owl#complementOf");
const ALL_DISJOINT_CLASSES: NamedNodeRef<'_> =
    owl("http://www.w3.org/2002/07/owl#AllDisjointClasses");
const ALL_DISJOINT_PROPERTIES: NamedNodeRef<'_> =
    owl("http://www.w3.org/2002/07/owl#AllDisjointProperties");
const MEMBERS: NamedNodeRef<'_> = owl("http://www.w3.org/2002/07/owl#members");
const DISTINCT_MEMBERS: NamedNodeRef<'_> = owl("http://www.w3.org/2002/07/owl#distinctMembers");
const IRREFLEXIVE: NamedNodeRef<'_> = owl("http://www.w3.org/2002/07/owl#IrreflexiveProperty");
const ASYMMETRIC: NamedNodeRef<'_> = owl("http://www.w3.org/2002/07/owl#AsymmetricProperty");
const PROPERTY_DISJOINT_WITH: NamedNodeRef<'_> =
    owl("http://www.w3.org/2002/07/owl#propertyDisjointWith");
const DIFFERENT_FROM: NamedNodeRef<'_> = owl("http://www.w3.org/2002/07/owl#differentFrom");
const ALL_DIFFERENT: NamedNodeRef<'_> = owl("http://www.w3.org/2002/07/owl#AllDifferent");
const SOURCE_INDIVIDUAL: NamedNodeRef<'_> = owl("http://www.w3.org/2002/07/owl#sourceIndividual");
const ASSERTION_PROPERTY: NamedNodeRef<'_> = owl("http://www.w3.org/2002/07/owl#assertionProperty");
const TARGET_INDIVIDUAL: NamedNodeRef<'_> = owl("http://www.w3.org/2002/07/owl#targetIndividual");
const TARGET_VALUE: NamedNodeRef<'_> = owl("http://www.w3.org/2002/07/owl#targetValue");

/// One contradiction: the OWL 2 RL rule (`cax-dw`, say) and the terms it is about (for
/// cax-dw: the resource and the two disjoint classes it has).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inconsistency {
    pub rule: &'static str,
    pub terms: Vec<Term>,
}

/// What [`inconsistencies`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inconsistencies {
    /// How many contradictions, each once; at most [`CAP`].
    pub count: usize,
    /// Whether the checks stopped at [`CAP`] contradictions: there are at least `count`.
    pub at_least: bool,
    /// The first of them.
    pub first: Vec<Inconsistency>,
}

/// The contradictions counted at most: each is kept (on its canonical terms) to count it
/// once, so a small file whose every resource has a hundred disjoint classes would take
/// gigabytes. Past a million the number tells nobody more.
pub const CAP: usize = 1_000_000;

/// What the store's triples, in all graphs, contradict, by the OWL 2 RL rules eq-diff1-3,
/// prp-irp, prp-asyp, prp-pdw, prp-adp, prp-npa1/2, cls-nothing2, cls-com, cls-maxc1,
/// cls-maxqc1/2, cax-dw and cax-adc: how many contradictions (up to [`CAP`]), and the first
/// `limit` of them.
/// Lists (`owl:members`, ...) are read outside graph `entailed`, as the data wrote them, not
/// in the forms owl:sameAs rewriting wrote there.
/// `cancelled` stops it between rules and every few thousand resources, with what it has.
pub fn inconsistencies(
    snapshot: &Snapshot,
    entailed: Option<TermId>,
    limit: usize,
    cancelled: &dyn Fn() -> bool,
) -> Inconsistencies {
    let full = Cell::new(false);
    let mut found = Found {
        full: &full,
        snapshot,
        same_as: snapshot.term_id(super::owl::SAME_AS.into()),
        limit,
        count: 0,
        first: Vec::new(),
        seen: HashSet::new(),
        canon: HashMap::new(),
    };
    // The checks stop as they stop when cancelled.
    check(snapshot, entailed, &mut found, &|| {
        full.get() || cancelled()
    });
    Inconsistencies {
        count: found.count,
        at_least: full.get(),
        first: found.first,
    }
}

struct Found<'a> {
    snapshot: &'a Snapshot,
    /// Set at [`CAP`] contradictions.
    full: &'a Cell<bool>,
    same_as: Option<TermId>,
    limit: usize,
    count: usize,
    first: Vec<Inconsistency>,
    /// The contradictions found, on canonical terms: entailment writes each triple again
    /// for each term the same as one of its terms, and a contradiction counts once.
    seen: HashSet<(&'static str, Vec<TermId>)>,
    canon: HashMap<TermId, TermId>,
}

impl Found<'_> {
    /// The least resource the same as `t` (owl:sameAs as entailment wrote it, closed).
    fn canon(&mut self, t: TermId) -> TermId {
        let Some(same) = self.same_as else { return t };
        if let Some(&c) = self.canon.get(&t) {
            return c;
        }
        let c = self
            .snapshot
            .quad_ids_for_pattern(Some(t), Some(same), None, GraphSelector::All)
            .map(|q| q[2])
            // `x owl:sameAs "a literal"` makes no resource a literal (eq-rep reads resources).
            .filter(|o| matches!(o.kind(), Kind::Iri | Kind::Blank))
            .fold(t, TermId::min);
        self.canon.insert(t, c);
        c
    }

    /// `terms` broke `rule`; with `unordered`, the terms at those two places in either order
    /// (the two classes of `A owl:disjointWith B`, stated either way).
    fn add(&mut self, rule: &'static str, terms: &[TermId], unordered: Option<(usize, usize)>) {
        let mut key: Vec<TermId> = terms.iter().map(|&t| self.canon(t)).collect();
        if let Some((i, j)) = unordered
            && key[j] < key[i]
        {
            key.swap(i, j);
        }
        if self.full.get() || !self.seen.insert((rule, key)) {
            return;
        }
        if self.count + 1 == CAP {
            self.full.set(true);
        }
        self.count += 1;
        if self.first.len() < self.limit {
            let terms = terms
                .iter()
                .filter_map(|&t| self.snapshot.term(t).ok())
                .collect();
            self.first.push(Inconsistency { rule, terms });
        }
    }
}

fn check(
    snapshot: &Snapshot,
    entailed: Option<TermId>,
    found: &mut Found<'_>,
    cancelled: &dyn Fn() -> bool,
) {
    let v = vocab(snapshot, Regime::OwlRl);
    let id = |n: NamedNodeRef<'_>| snapshot.term_id(n.into());
    let quads = |s: Option<TermId>, p: Option<TermId>, o: Option<TermId>| {
        snapshot.quad_ids_for_pattern(s, p, o, GraphSelector::All)
    };
    // Each (subject, object) of `p` once, over all graphs.
    let pairs = |p: Option<TermId>| -> Vec<(TermId, TermId)> {
        let Some(p) = p else { return Vec::new() };
        let mut out: Vec<(TermId, TermId)> =
            quads(None, Some(p), None).map(|q| (q[0], q[2])).collect();
        out.sort_unstable();
        out.dedup();
        out
    };
    let objects = |s: TermId, p: Option<TermId>| -> Vec<TermId> {
        let Some(p) = p else { return Vec::new() };
        let mut out: Vec<TermId> = quads(Some(s), Some(p), None).map(|q| q[2]).collect();
        out.sort_unstable();
        out.dedup();
        out
    };
    let has = |s: TermId, p: TermId, o: TermId| quads(Some(s), Some(p), Some(o)).next().is_some();
    let typed = |c: TermId| -> Vec<TermId> {
        let Some(t) = v.rdf_type else {
            return Vec::new();
        };
        let mut out: Vec<TermId> = quads(None, Some(t), Some(c)).map(|q| q[0]).collect();
        out.sort_unstable();
        out.dedup();
        out
    };
    let is_a = |x: TermId, c: TermId| v.rdf_type.is_some_and(|t| has(x, t, c));
    // The lists that `owl:members` (or `owl:distinctMembers`) of a `kind` give.
    let lists = |kind: Option<TermId>, member_ps: &[Option<TermId>]| -> Vec<Vec<TermId>> {
        let Some(kind) = kind else { return Vec::new() };
        let mut out = Vec::new();
        for x in typed(kind) {
            for &mp in member_ps {
                for head in objects(x, mp) {
                    // As the data wrote it: not the forms owl:sameAs rewrote.
                    if let Some(l) = list(snapshot, entailed, v, head) {
                        out.push(l);
                    }
                }
            }
        }
        out
    };
    // Resources of both classes, a pair of disjoint classes.
    let both = |found: &mut Found<'_>, rule: &'static str, c1: TermId, c2: TermId| {
        if c1 == c2 {
            // A class disjoint with itself: each member is inconsistent with itself.
            for (i, x) in typed(c1).into_iter().enumerate() {
                if i % 4096 == 4095 && cancelled() {
                    return;
                }
                found.add(rule, &[x, c1, c2], Some((1, 2)));
            }
            return;
        }
        let (a, b) = (typed(c1), typed(c2));
        let (small, large) = if a.len() <= b.len() {
            (&a, &b)
        } else {
            (&b, &a)
        };
        for (i, &x) in small.iter().enumerate() {
            if i % 4096 == 4095 && cancelled() {
                return;
            }
            if large.binary_search(&x).is_ok() {
                found.add(rule, &[x, c1, c2], Some((1, 2)));
            }
        }
    };

    // cls-nothing2.
    if let Some(nothing) = id(NOTHING) {
        for x in typed(nothing) {
            found.add("cls-nothing2", &[x], None);
        }
    }
    // cax-dw, cls-com.
    for (rule, p) in [
        ("cax-dw", id(DISJOINT_WITH)),
        ("cls-com", id(COMPLEMENT_OF)),
    ] {
        for (c1, c2) in pairs(p) {
            if cancelled() {
                return;
            }
            both(found, rule, c1, c2);
        }
    }
    // cax-adc.
    // Each class read once: for each resource, the places in the list of the classes it has;
    // two places, a contradiction (a class at two places too).
    for l in lists(id(ALL_DISJOINT_CLASSES), &[id(MEMBERS)]) {
        let mut at: HashMap<TermId, Vec<TermId>> = HashMap::new();
        for &c in &l {
            if cancelled() {
                return;
            }
            for x in typed(c) {
                at.entry(x).or_default().push(c);
            }
        }
        if !clashes(
            found,
            "cax-adc",
            at,
            cancelled,
            &|x, c1, c2| vec![x, c1, c2],
            (1, 2),
        ) {
            return;
        }
    }
    // prp-irp, prp-asyp.
    {
        if let Some(irp) = id(IRREFLEXIVE) {
            for p in typed(irp) {
                for (i, q) in quads(None, Some(p), None).enumerate() {
                    if i % 65_536 == 65_535 && cancelled() {
                        return;
                    }
                    if q[0] == q[2] {
                        found.add("prp-irp", &[q[0], p], None);
                    }
                }
            }
        }
        if let Some(asyp) = id(ASYMMETRIC) {
            for p in typed(asyp) {
                for (i, (x, y)) in pairs(Some(p)).into_iter().enumerate() {
                    if i % 4096 == 4095 && cancelled() {
                        return;
                    }
                    // Each pair once: x p x, or the smaller of x and y first.
                    if x <= y && has(y, p, x) {
                        found.add("prp-asyp", &[x, y, p], Some((0, 1)));
                    }
                }
            }
        }
    }
    // prp-pdw, prp-adp: a pair of resources related by both of two disjoint properties.
    let disjoint = |found: &mut Found<'_>, rule: &'static str, p1: TermId, p2: TermId| {
        let (a, b) = (pairs(Some(p1)), pairs(Some(p2)));
        let (small, large) = if a.len() <= b.len() {
            (&a, &b)
        } else {
            (&b, &a)
        };
        for (i, &(x, y)) in small.iter().enumerate() {
            if i % 4096 == 4095 && cancelled() {
                return;
            }
            if large.binary_search(&(x, y)).is_ok() {
                found.add(rule, &[x, y, p1, p2], Some((2, 3)));
            }
        }
    };
    for (p1, p2) in pairs(id(PROPERTY_DISJOINT_WITH)) {
        if cancelled() {
            return;
        }
        disjoint(found, "prp-pdw", p1, p2);
    }
    for l in lists(id(ALL_DISJOINT_PROPERTIES), &[id(MEMBERS)]) {
        let mut at: HashMap<(TermId, TermId), Vec<TermId>> = HashMap::new();
        for &p in &l {
            if cancelled() {
                return;
            }
            for xy in pairs(Some(p)) {
                at.entry(xy).or_default().push(p);
            }
        }
        if !clashes(
            found,
            "prp-adp",
            at,
            cancelled,
            &|(x, y), p1, p2| vec![x, y, p1, p2],
            (2, 3),
        ) {
            return;
        }
    }
    // eq-diff1: different and the same (a resource different from itself, too: it is the
    // same as itself).
    let same = |x: TermId, y: TermId| x == y || v.same_as.is_some_and(|s| has(x, s, y));
    // Two terms first: of the forms entailment writes for `a owl:differentFrom b` with a and
    // b the same (`a owl:differentFrom a`, ...), the one the data most likely holds.
    let mut different = pairs(id(DIFFERENT_FROM));
    different.sort_by_key(|&(x, y)| x == y);
    for (i, (x, y)) in different.into_iter().enumerate() {
        if i % 4096 == 4095 && cancelled() {
            return;
        }
        if same(x, y) {
            found.add("eq-diff1", &[x, y], Some((0, 1)));
        }
    }
    // eq-diff2/3.
    // Members grouped by their canonical term: two in a group are the same.
    for l in lists(id(ALL_DIFFERENT), &[id(MEMBERS), id(DISTINCT_MEMBERS)]) {
        let mut first: HashMap<TermId, TermId> = HashMap::new();
        for (i, &m) in l.iter().enumerate() {
            if i % 4096 == 4095 && cancelled() {
                return;
            }
            match first.entry(found.canon(m)) {
                std::collections::hash_map::Entry::Occupied(a) => {
                    found.add("eq-diff2", &[*a.get(), m], Some((0, 1)));
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(m);
                }
            }
        }
    }
    // prp-npa1/2.
    if let (Some(source), Some(property)) = (id(SOURCE_INDIVIDUAL), id(ASSERTION_PROPERTY)) {
        let mut assertions: Vec<TermId> = quads(None, Some(source), None).map(|q| q[0]).collect();
        assertions.sort_unstable();
        assertions.dedup();
        for n in assertions {
            for i1 in objects(n, Some(source)) {
                for p in objects(n, Some(property)) {
                    for (rule, target) in [
                        ("prp-npa1", id(TARGET_INDIVIDUAL)),
                        ("prp-npa2", id(TARGET_VALUE)),
                    ] {
                        for i2 in objects(n, target) {
                            if has(i1, p, i2) {
                                found.add(rule, &[i1, p, i2], None);
                            }
                        }
                    }
                }
            }
        }
    }
    // cls-maxc1, cls-maxqc1/2: a restriction to no value (of a class) with one.
    let zero = |n: TermId| super::is_cardinality(snapshot, n, 0);
    for (rule, cardinality, qualified) in [
        ("cls-maxc1", v.max_cardinality, false),
        ("cls-maxqc1", v.max_qualified_cardinality, true),
    ] {
        for (r, n) in pairs(cardinality) {
            if cancelled() {
                return;
            }
            if !zero(n) {
                continue;
            }
            let classes = if qualified {
                objects(r, v.on_class)
            } else {
                Vec::new()
            };
            let thing = v.thing;
            for p in objects(r, v.on_property) {
                for (i, x) in typed(r).into_iter().enumerate() {
                    if i % 4096 == 4095 && cancelled() {
                        return;
                    }
                    for y in objects(x, Some(p)) {
                        if !qualified {
                            found.add(rule, &[x, r, p, y], None);
                            continue;
                        }
                        for &c in &classes {
                            if Some(c) == thing {
                                found.add("cls-maxqc2", &[x, r, p, y], None);
                            } else if is_a(y, c) {
                                found.add(rule, &[x, r, p, y], None);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// For each key (a resource, a pair of resources) the list members it has, at each place
/// in the list: each two of them a contradiction. False once `cancelled`.
fn clashes<K>(
    found: &mut Found<'_>,
    rule: &'static str,
    at: HashMap<K, Vec<TermId>>,
    cancelled: &dyn Fn() -> bool,
    terms: &dyn Fn(K, TermId, TermId) -> Vec<TermId>,
    unordered: (usize, usize),
) -> bool
where
    K: Copy,
{
    for (i, (k, ms)) in at.into_iter().enumerate() {
        if i % 4096 == 4095 && cancelled() {
            return false;
        }
        // One resource with every class of a list of thousands: millions of pairs.
        for (j, &a) in ms.iter().enumerate() {
            if j % 64 == 63 && cancelled() {
                return false;
            }
            for &b in &ms[j + 1..] {
                found.add(rule, &terms(k, a, b), Some(unordered));
            }
        }
    }
    true
}
