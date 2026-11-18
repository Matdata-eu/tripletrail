//! Why an entailed triple is there (#407): a derivation tree, from the rule that gives the
//! triple down to asserted triples.
//!
//! The entailment itself runs on compiled plans and closures that keep no trace of which
//! triples made which (that is what makes it fast), so the derivation is found afterwards,
//! goal-directed, on the closure (the asserted triples with what [`super::entail`]
//! derived): for a triple, each rule instance whose conclusion it is and whose premises all
//! hold is a candidate, and the first candidate whose premises can be derived in turn wins.
//! The search deepens step by step, so the tree it finds is a shallow one, and it never uses
//! a triple to derive itself.
//!
//! The rules are the regime's, named as Jena names the RDFS ones (`rdfs9`) and as the OWL 2
//! RL profile names its rules (`cax-sco`): under [`Regime::OwlRl`] the OWL names are used for
//! the RDFS rules too (`rdfs9` is `cax-sco`).

use super::{Regime, Vocab, vocab};
use crate::{GraphSelector, Snapshot, TermId};
use oxigraph::model::Term;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};

/// How a triple is derived: by `rule` from `premises`, or asserted (`rule` is `None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derivation {
    pub triple: [TermId; 3],
    pub rule: Option<&'static str>,
    pub premises: Vec<Derivation>,
}

/// The closure of a store under a regime, indexed to find derivations.
pub struct Explainer {
    v: Vocab,
    regime: Regime,
    asserted: HashSet<[TermId; 3]>,
    all: HashSet<[TermId; 3]>,
    sp: HashMap<(TermId, TermId), Vec<TermId>>,
    po: HashMap<(TermId, TermId), Vec<TermId>>,
    /// cls-maxc2 and cls-maxqc3/4: (R, p, C) of the restrictions to at most one value.
    max_one: Vec<(TermId, TermId, Option<TermId>, [TermId; 3])>,
    /// The index entries a search has read, and how many it may read: once over, lookups
    /// find nothing and the search gives up.
    steps: Cell<usize>,
    max_steps: Cell<usize>,
}

/// The search was cancelled (see [`Explainer::derive`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

/// Rule instances: the rule and its premises.
type Candidates = Vec<(&'static str, Vec<[TermId; 3]>)>;

/// The most candidates of one rule tried for a triple: the domain rule has one per value of
/// the property, and any one of them does.
const PER_RULE: usize = 64;

/// The most entries one index lookup reads: on a big owl:sameAs clique or a class with many
/// subclasses, the first of them.
const SCAN: usize = 256;

/// Index entries a search may read per triple of its budget.
const STEPS_PER_TRIPLE: usize = 64;

impl Explainer {
    /// The closure of the quads of `snapshot` in every graph but `skip`, with `entailed`
    /// (what [`super::entail`] derived from them under `regime`).
    pub fn new(
        snapshot: &Snapshot,
        skip: Option<TermId>,
        regime: Regime,
        entailed: &[[TermId; 3]],
    ) -> Self {
        let v = vocab(snapshot, regime);
        let asserted: HashSet<[TermId; 3]> = snapshot
            .quad_ids_for_pattern(None, None, None, GraphSelector::All)
            .filter(|q| Some(q[3]) != skip)
            .map(|q| [q[0], q[1], q[2]])
            .collect();
        let mut all = asserted.clone();
        all.extend(entailed.iter().copied());
        let mut sp: HashMap<(TermId, TermId), Vec<TermId>> = HashMap::new();
        let mut po: HashMap<(TermId, TermId), Vec<TermId>> = HashMap::new();
        for t in &all {
            sp.entry((t[0], t[1])).or_default().push(t[2]);
            po.entry((t[1], t[2])).or_default().push(t[0]);
        }
        for list in sp.values_mut().chain(po.values_mut()) {
            list.sort_unstable();
        }
        let mut explainer = Self {
            v,
            regime,
            asserted,
            all,
            sp,
            po,
            max_one: Vec::new(),
            steps: Cell::new(0),
            max_steps: Cell::new(usize::MAX),
        };
        explainer.max_one = explainer.read_max_one(snapshot);
        explainer
    }

    /// Whether `triple` holds in the closure.
    pub fn holds(&self, triple: [TermId; 3]) -> bool {
        self.all.contains(&triple)
    }

    /// A derivation of `triple`, the least deep, or `None` when it does not hold or none is
    /// found within the budget: the first `budget` triples a derivation of it may go
    /// through, and [`STEPS_PER_TRIPLE`] index entries read per triple of the budget (a
    /// derivation through a rule this explainer does not know, or through a generalized
    /// triple the entailment does not keep, is not found either). `cancelled` is asked
    /// now and then; once it says yes, the search stops with [`Cancelled`].
    ///
    /// The triples a derivation may go through are collected first, from `triple` back to
    /// the data, each with the rule instances that conclude it; then they are proved round
    /// by round from the asserted ones, so each is proved from triples proved before it.
    pub fn derive(
        &self,
        triple: [TermId; 3],
        budget: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<Derivation>, Cancelled> {
        if !self.all.contains(&triple) {
            return Ok(None);
        }
        if self.asserted.contains(&triple) {
            return Ok(Some(Derivation {
                triple,
                rule: None,
                premises: Vec::new(),
            }));
        }
        self.steps.set(0);
        self.max_steps.set(budget.saturating_mul(STEPS_PER_TRIPLE));
        // The triples a derivation of `triple` may go through, each with the rule
        // instances that conclude it.
        let mut support: HashMap<[TermId; 3], Candidates> = HashMap::new();
        let mut queue = vec![triple];
        while let Some(t) = queue.pop() {
            if support.contains_key(&t) || self.asserted.contains(&t) {
                continue;
            }
            if support.len() >= budget || self.exhausted() {
                return Ok(None);
            }
            if support.len().is_multiple_of(256) && cancelled() {
                return Err(Cancelled);
            }
            let candidates = self.candidates(t);
            for (_, premises) in &candidates {
                queue.extend(premises.iter().filter(|p| !support.contains_key(*p)));
            }
            support.insert(t, candidates);
        }
        // Proved round by round from the asserted triples: a triple proved in a round has
        // a derivation of that height, the least it has, from triples proved before it. Each
        // rule instance counts its premises not proved yet; a triple proved in a round makes
        // the instances it was the last missing premise of ready for the next.
        let mut instances: Vec<([TermId; 3], usize, usize)> = Vec::new();
        let mut waiting: HashMap<[TermId; 3], Vec<usize>> = HashMap::new();
        let mut round: Vec<usize> = Vec::new();
        for (n, (t, candidates)) in support.iter().enumerate() {
            for (k, (_, premises)) in candidates.iter().enumerate() {
                let i = instances.len();
                let mut missing: Vec<[TermId; 3]> = premises
                    .iter()
                    .filter(|p| !self.asserted.contains(*p))
                    .copied()
                    .collect();
                missing.sort_unstable();
                missing.dedup();
                for p in &missing {
                    waiting.entry(*p).or_default().push(i);
                }
                instances.push((*t, k, missing.len()));
                if missing.is_empty() {
                    round.push(i);
                }
            }
            if n.is_multiple_of(1024) && cancelled() {
                return Err(Cancelled);
            }
        }
        let mut proved: HashMap<[TermId; 3], (&'static str, Vec<[TermId; 3]>)> = HashMap::new();
        while !proved.contains_key(&triple) {
            if round.is_empty() {
                return Ok(None);
            }
            if cancelled() {
                return Err(Cancelled);
            }
            // The triples this round proves, each by its first ready instance.
            let mut now: Vec<[TermId; 3]> = Vec::new();
            round.sort_unstable();
            for i in round.drain(..) {
                let (t, k, _) = instances[i];
                if proved.contains_key(&t) {
                    continue;
                }
                let (rule, premises) = &support[&t][k];
                proved.insert(t, (*rule, premises.clone()));
                now.push(t);
            }
            for t in now {
                for &i in waiting.get(&t).into_iter().flatten() {
                    let missing = &mut instances[i].2;
                    *missing -= 1;
                    if *missing == 0 && !proved.contains_key(&instances[i].0) {
                        round.push(i);
                    }
                }
            }
        }
        fn tree(
            t: [TermId; 3],
            proved: &HashMap<[TermId; 3], (&'static str, Vec<[TermId; 3]>)>,
        ) -> Derivation {
            match proved.get(&t) {
                Some((rule, premises)) => Derivation {
                    triple: t,
                    rule: Some(rule),
                    premises: premises.iter().map(|&p| tree(p, proved)).collect(),
                },
                None => Derivation {
                    triple: t,
                    rule: None,
                    premises: Vec::new(),
                },
            }
        }
        Ok(Some(tree(triple, &proved)))
    }

    /// Whether the search has read all the index entries it may.
    fn exhausted(&self) -> bool {
        self.steps.get() > self.max_steps.get()
    }

    /// At most [`SCAN`] entries of a lookup, counted against the search's steps.
    fn counted<'a>(&self, found: Option<&'a Vec<TermId>>) -> &'a [TermId] {
        if self.exhausted() {
            return &[];
        }
        let found = found.map_or(&[][..], |v| &v[..v.len().min(SCAN)]);
        self.steps.set(self.steps.get() + found.len() + 1);
        found
    }

    /// The objects of `s p` (the first [`SCAN`]).
    fn objects(&self, s: TermId, p: Option<TermId>) -> &[TermId] {
        self.counted(p.and_then(|p| self.sp.get(&(s, p))))
    }

    /// The subjects of `p o` (the first [`SCAN`]).
    fn subjects(&self, p: Option<TermId>, o: TermId) -> &[TermId] {
        self.counted(p.and_then(|p| self.po.get(&(p, o))))
    }

    /// The members of the RDF list at `head`.
    fn list(&self, head: TermId) -> Option<Vec<TermId>> {
        let (first, rest) = (self.v.first?, self.v.rest?);
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut node = head;
        while Some(node) != self.v.nil {
            if !seen.insert(node) {
                return None;
            }
            out.push(*self.objects(node, Some(first)).first()?);
            node = *self.objects(node, Some(rest)).first()?;
        }
        Some(out)
    }

    /// The restrictions to at most one value (of a class): `owl:maxCardinality 1`, or
    /// `owl:maxQualifiedCardinality 1` with `owl:onClass`.
    fn read_max_one(
        &self,
        snapshot: &Snapshot,
    ) -> Vec<(TermId, TermId, Option<TermId>, [TermId; 3])> {
        let one = |id: TermId| matches!(snapshot.term(id), Ok(Term::Literal(l)) if l.value().trim() == "1");
        let mut out = Vec::new();
        for (card, qualified) in [
            (self.v.max_cardinality, false),
            (self.v.max_qualified_cardinality, true),
        ] {
            let Some(card) = card else { continue };
            for (&(r, p), values) in &self.sp {
                if p != card || !values.iter().any(|&n| one(n)) {
                    continue;
                }
                let n = *values.iter().find(|&&n| one(n)).expect("checked");
                for &q in self.objects(r, self.v.on_property) {
                    if qualified {
                        for &c in self.objects(r, self.v.on_class) {
                            let c = Some(c).filter(|&c| Some(c) != self.v.thing);
                            out.push((r, q, c, [r, card, n]));
                        }
                    } else {
                        out.push((r, q, None, [r, card, n]));
                    }
                }
            }
        }
        out
    }

    /// The rule's name under the regime: Jena's RDFS name, or the OWL 2 RL one.
    fn name(&self, rdfs: &'static str, owl: &'static str) -> &'static str {
        match self.regime {
            Regime::Rdfs => rdfs,
            Regime::OwlRl => owl,
        }
    }

    /// The rule instances that conclude `t`, each with premises that all hold.
    fn candidates(&self, t: [TermId; 3]) -> Candidates {
        let [s, p, o] = t;
        let v = &self.v;
        let mut out: Candidates = Vec::new();
        let mut add = |rule: &'static str, premises: Vec<[TermId; 3]>| {
            if !premises.contains(&t) && out.iter().filter(|(r, _)| *r == rule).count() < PER_RULE {
                out.push((rule, premises));
            }
        };
        let is = |p: Option<TermId>| p.is_some() && p == Some(t[1]);
        // Rules on any property.
        if let Some(sub) = v.sub_property {
            for &q in self.subjects(Some(sub), p) {
                if q != p && self.all.contains(&[s, q, o]) {
                    add(self.name("rdfs7", "prp-spo1"), vec![[q, sub, p], [s, q, o]]);
                }
            }
        }
        if let Some(eq) = v.equivalent_property {
            for &q in self.subjects(Some(eq), p) {
                if q != p && self.all.contains(&[s, q, o]) {
                    add("prp-eqp1", vec![[q, eq, p], [s, q, o]]);
                }
            }
            for &q in self.objects(p, Some(eq)) {
                if q != p && self.all.contains(&[s, q, o]) {
                    add("prp-eqp2", vec![[p, eq, q], [s, q, o]]);
                }
            }
        }
        if let Some(inv) = v.inverse_of {
            for &q in self.subjects(Some(inv), p) {
                if self.all.contains(&[o, q, s]) {
                    add("prp-inv1", vec![[q, inv, p], [o, q, s]]);
                }
            }
            for &q in self.objects(p, Some(inv)) {
                if self.all.contains(&[o, q, s]) {
                    add("prp-inv2", vec![[p, inv, q], [o, q, s]]);
                }
            }
        }
        if let (Some(ty), Some(sym)) = (v.rdf_type, v.symmetric)
            && self.all.contains(&[p, ty, sym])
            && self.all.contains(&[o, p, s])
        {
            add("prp-symp", vec![[p, ty, sym], [o, p, s]]);
        }
        if let (Some(ty), Some(tr)) = (v.rdf_type, v.transitive)
            && self.all.contains(&[p, ty, tr])
        {
            for &m in self.objects(s, Some(p)) {
                if m != o && m != s && self.all.contains(&[m, p, o]) {
                    add("prp-trp", vec![[p, ty, tr], [s, p, m], [m, p, o]]);
                }
            }
        }
        if let Some(same) = v.same_as {
            for &x in self.subjects(Some(same), s) {
                if x != s && self.all.contains(&[x, p, o]) {
                    add("eq-rep-s", vec![[x, same, s], [x, p, o]]);
                }
            }
            for &x in self.subjects(Some(same), p) {
                if x != p && self.all.contains(&[s, x, o]) {
                    add("eq-rep-p", vec![[x, same, p], [s, x, o]]);
                }
            }
            for &x in self.subjects(Some(same), o) {
                if x != o && self.all.contains(&[s, p, x]) {
                    add("eq-rep-o", vec![[x, same, o], [s, p, x]]);
                }
            }
        }
        if let Some(chain) = v.property_chain_axiom {
            for &head in self.objects(p, Some(chain)) {
                if let Some(steps) = self.list(head)
                    && let Some(path) = self.path(s, o, &steps)
                {
                    let mut premises = vec![[p, chain, head]];
                    premises.extend(path);
                    add("prp-spo2", premises);
                }
            }
        }
        if let (Some(hv), Some(on), Some(ty)) = (v.has_value, v.on_property, v.rdf_type) {
            for &r in self.subjects(Some(hv), o) {
                if self.all.contains(&[r, on, p]) && self.all.contains(&[s, ty, r]) {
                    add("cls-hv2", vec![[r, hv, o], [r, on, p], [s, ty, r]]);
                }
            }
        }
        if is(v.rdf_type) {
            self.typed(t, &mut add);
        }
        if is(v.sub_class) {
            self.sub_class(t, &mut add);
        }
        if is(v.sub_property) {
            let sub = p;
            for &m in self.objects(s, Some(sub)) {
                if m != s && m != o && self.all.contains(&[m, sub, o]) {
                    add(
                        self.name("rdfs5", "scm-spo"),
                        vec![[s, sub, m], [m, sub, o]],
                    );
                }
            }
            if let Some(eq) = v.equivalent_property {
                if self.all.contains(&[s, eq, o]) {
                    add("scm-eqp1", vec![[s, eq, o]]);
                }
                if self.all.contains(&[o, eq, s]) {
                    add("scm-eqp1", vec![[o, eq, s]]);
                }
            }
            self.declared_property(t, &mut add);
        }
        if is(v.equivalent_class) {
            if let Some(sub) = v.sub_class
                && self.all.contains(&[s, sub, o])
                && self.all.contains(&[o, sub, s])
            {
                add("scm-eqc2", vec![[s, sub, o], [o, sub, s]]);
            }
            if let (Some(ty), Some(class)) = (v.rdf_type, v.class)
                && s == o
            {
                if self.all.contains(&[s, ty, class]) {
                    add("scm-cls", vec![[s, ty, class]]);
                } else if Some(s) == v.thing || Some(s) == v.nothing {
                    add("scm-cls", Vec::new());
                }
            }
        }
        if is(v.equivalent_property) {
            if let Some(sub) = v.sub_property
                && self.all.contains(&[s, sub, o])
                && self.all.contains(&[o, sub, s])
            {
                add("scm-eqp2", vec![[s, sub, o], [o, sub, s]]);
            }
            self.declared_property(t, &mut add);
        }
        for (scoped, dom1, dom2) in [
            (v.domain, "scm-dom1", "scm-dom2"),
            (v.range, "scm-rng1", "scm-rng2"),
        ] {
            if !is(scoped) || self.regime != Regime::OwlRl {
                continue;
            }
            if let Some(sub) = v.sub_class {
                for &c in self.objects(s, Some(p)) {
                    if c != o && self.all.contains(&[c, sub, o]) {
                        add(dom1, vec![[s, p, c], [c, sub, o]]);
                    }
                }
            }
            if let Some(sub) = v.sub_property {
                for &q in self.objects(s, Some(sub)) {
                    if q != s && self.all.contains(&[q, p, o]) {
                        add(dom2, vec![[q, p, o], [s, sub, q]]);
                    }
                }
            }
        }
        if is(v.same_as) {
            self.same(t, &mut add);
        }
        out
    }

    /// The rules that conclude `s rdf:type o`.
    fn typed(&self, t: [TermId; 3], add: &mut impl FnMut(&'static str, Vec<[TermId; 3]>)) {
        let [s, ty, o] = t;
        let v = &self.v;
        if let Some(sub) = v.sub_class {
            for &d in self.subjects(Some(sub), o) {
                if d != o && self.all.contains(&[s, ty, d]) {
                    add(self.name("rdfs9", "cax-sco"), vec![[d, sub, o], [s, ty, d]]);
                }
            }
        }
        if let Some(eq) = v.equivalent_class {
            for &d in self.subjects(Some(eq), o) {
                if d != o && self.all.contains(&[s, ty, d]) {
                    add("cax-eqc1", vec![[d, eq, o], [s, ty, d]]);
                }
            }
            for &d in self.objects(o, Some(eq)) {
                if d != o && self.all.contains(&[s, ty, d]) {
                    add("cax-eqc2", vec![[o, eq, d], [s, ty, d]]);
                }
            }
        }
        if let Some(domain) = v.domain {
            for &q in self.subjects(Some(domain), o) {
                for &y in self.objects(s, Some(q)).iter().take(PER_RULE) {
                    add(
                        self.name("rdfs2", "prp-dom"),
                        vec![[q, domain, o], [s, q, y]],
                    );
                }
            }
        }
        if let Some(range) = v.range {
            for &q in self.subjects(Some(range), o) {
                for &x in self.subjects(Some(q), s).iter().take(PER_RULE) {
                    add(
                        self.name("rdfs3", "prp-rng"),
                        vec![[q, range, o], [x, q, s]],
                    );
                }
            }
        }
        if let (Some(hv), Some(on)) = (v.has_value, v.on_property) {
            for &value in self.objects(o, Some(hv)) {
                for &q in self.objects(o, Some(on)) {
                    if self.all.contains(&[s, q, value]) {
                        add("cls-hv1", vec![[o, hv, value], [o, on, q], [s, q, value]]);
                    }
                }
            }
        }
        if let (Some(svf), Some(on)) = (v.some_values_from, v.on_property) {
            for &c in self.objects(o, Some(svf)) {
                for &q in self.objects(o, Some(on)) {
                    for &y in self.objects(s, Some(q)) {
                        if Some(c) == v.thing {
                            add("cls-svf2", vec![[o, svf, c], [o, on, q], [s, q, y]]);
                        } else if self.all.contains(&[y, ty, c]) {
                            add(
                                "cls-svf1",
                                vec![[o, svf, c], [o, on, q], [s, q, y], [y, ty, c]],
                            );
                        }
                    }
                }
            }
        }
        if let (Some(avf), Some(on)) = (v.all_values_from, v.on_property) {
            for &r in self.subjects(Some(avf), o) {
                for &q in self.objects(r, Some(on)) {
                    for &x in self.subjects(Some(q), s) {
                        if self.all.contains(&[x, ty, r]) {
                            add(
                                "cls-avf",
                                vec![[r, avf, o], [r, on, q], [x, ty, r], [x, q, s]],
                            );
                        }
                    }
                }
            }
        }
        if let Some(int) = v.intersection_of {
            for &head in self.objects(o, Some(int)) {
                if let Some(members) = self.list(head)
                    && members.iter().all(|&c| self.all.contains(&[s, ty, c]))
                {
                    let mut premises = vec![[o, int, head]];
                    premises.extend(members.iter().map(|&c| [s, ty, c]));
                    add("cls-int1", premises);
                }
            }
            for &c in self.objects(s, Some(ty)) {
                for &head in self.objects(c, Some(int)) {
                    if self.list(head).is_some_and(|m| m.contains(&o)) {
                        add("cls-int2", vec![[c, int, head], [s, ty, c]]);
                    }
                }
            }
        }
        if let Some(uni) = v.union_of {
            for &head in self.objects(o, Some(uni)) {
                for c in self.list(head).unwrap_or_default() {
                    if self.all.contains(&[s, ty, c]) {
                        add("cls-uni", vec![[o, uni, head], [s, ty, c]]);
                    }
                }
            }
        }
        if let Some(one_of) = v.one_of {
            for &head in self.objects(o, Some(one_of)) {
                if self.list(head).is_some_and(|m| m.contains(&s)) {
                    add("cls-oo", vec![[o, one_of, head]]);
                }
            }
        }
    }

    /// The rules that conclude `s rdfs:subClassOf o`.
    fn sub_class(&self, t: [TermId; 3], add: &mut impl FnMut(&'static str, Vec<[TermId; 3]>)) {
        let [s, sub, o] = t;
        let v = &self.v;
        for &m in self.objects(s, Some(sub)) {
            if m != s && m != o && self.all.contains(&[m, sub, o]) {
                add(
                    self.name("rdfs11", "scm-sco"),
                    vec![[s, sub, m], [m, sub, o]],
                );
            }
        }
        if let Some(eq) = v.equivalent_class {
            if self.all.contains(&[s, eq, o]) {
                add("scm-eqc1", vec![[s, eq, o]]);
            }
            if self.all.contains(&[o, eq, s]) {
                add("scm-eqc1", vec![[o, eq, s]]);
            }
        }
        if let (Some(ty), Some(class)) = (v.rdf_type, v.class) {
            if (s == o || Some(o) == v.thing) && self.all.contains(&[s, ty, class]) {
                add("scm-cls", vec![[s, ty, class]]);
            }
            // owl:Thing and owl:Nothing are classes without being declared.
            let builtin = |c: TermId| Some(c) == v.thing || Some(c) == v.nothing;
            if builtin(s) && (s == o || Some(o) == v.thing || Some(s) == v.nothing) {
                add("scm-cls", Vec::new());
            }
            if Some(s) == v.nothing && self.all.contains(&[o, ty, class]) {
                add("scm-cls", vec![[o, ty, class]]);
            }
        }
        if let Some(int) = v.intersection_of {
            for &head in self.objects(s, Some(int)) {
                if self.list(head).is_some_and(|m| m.contains(&o)) {
                    add("scm-int", vec![[s, int, head]]);
                }
            }
        }
        if let Some(uni) = v.union_of {
            for &head in self.objects(o, Some(uni)) {
                if self.list(head).is_some_and(|m| m.contains(&s)) {
                    add("scm-uni", vec![[o, uni, head]]);
                }
            }
        }
        let (Some(on), Some(sub_p)) = (v.on_property, v.sub_property) else {
            return;
        };
        if let Some(hv) = v.has_value {
            for &i in self.objects(s, Some(hv)) {
                if !self.all.contains(&[o, hv, i]) {
                    continue;
                }
                for &p1 in self.objects(s, Some(on)) {
                    for &p2 in self.objects(o, Some(on)) {
                        if self.all.contains(&[p1, sub_p, p2]) {
                            add(
                                "scm-hv",
                                vec![
                                    [s, hv, i],
                                    [s, on, p1],
                                    [o, hv, i],
                                    [o, on, p2],
                                    [p1, sub_p, p2],
                                ],
                            );
                        }
                    }
                }
            }
        }
        for (restriction, one, two, flipped) in [
            (v.some_values_from, "scm-svf1", "scm-svf2", false),
            (v.all_values_from, "scm-avf1", "scm-avf2", true),
        ] {
            let Some(r) = restriction else { continue };
            for &y1 in self.objects(s, Some(r)) {
                for &y2 in self.objects(o, Some(r)) {
                    for &p1 in self.objects(s, Some(on)) {
                        for &p2 in self.objects(o, Some(on)) {
                            if p1 == p2 && self.all.contains(&[y1, sub, y2]) {
                                add(
                                    one,
                                    vec![
                                        [s, r, y1],
                                        [s, on, p1],
                                        [o, r, y2],
                                        [o, on, p1],
                                        [y1, sub, y2],
                                    ],
                                );
                            }
                            // svf2: p1 under p2 makes s under o; avf2: p2 under p1 does.
                            let (lower, upper) = if flipped { (p2, p1) } else { (p1, p2) };
                            if y1 == y2 && self.all.contains(&[lower, sub_p, upper]) {
                                add(
                                    two,
                                    vec![
                                        [s, r, y1],
                                        [s, on, p1],
                                        [o, r, y1],
                                        [o, on, p2],
                                        [lower, sub_p, upper],
                                    ],
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// scm-op and scm-dp: a declared property is its own sub property and equivalent.
    fn declared_property(
        &self,
        t: [TermId; 3],
        add: &mut impl FnMut(&'static str, Vec<[TermId; 3]>),
    ) {
        let [s, _, o] = t;
        let Some(ty) = self.v.rdf_type else { return };
        if s != o {
            return;
        }
        for (kind, rule) in [
            (self.v.object_property, "scm-op"),
            (self.v.datatype_property, "scm-dp"),
        ] {
            if let Some(kind) = kind
                && self.all.contains(&[s, ty, kind])
            {
                add(rule, vec![[s, ty, kind]]);
            }
        }
    }

    /// The rules that conclude `s owl:sameAs o`.
    fn same(&self, t: [TermId; 3], add: &mut impl FnMut(&'static str, Vec<[TermId; 3]>)) {
        let [s, same, o] = t;
        let v = &self.v;
        if self.all.contains(&[o, same, s]) {
            add("eq-sym", vec![[o, same, s]]);
        }
        for &m in self.objects(s, Some(same)) {
            if m != s && m != o && self.all.contains(&[m, same, o]) {
                add("eq-trans", vec![[s, same, m], [m, same, o]]);
            }
        }
        let Some(ty) = v.rdf_type else { return };
        if let Some(fp) = v.functional {
            for &q in self.subjects(Some(ty), fp) {
                for &x in self.subjects(Some(q), s) {
                    if self.all.contains(&[x, q, o]) {
                        add("prp-fp", vec![[q, ty, fp], [x, q, s], [x, q, o]]);
                    }
                }
            }
        }
        if let Some(ifp) = v.inverse_functional {
            for &q in self.subjects(Some(ty), ifp) {
                for &y in self.objects(s, Some(q)) {
                    if self.all.contains(&[o, q, y]) {
                        add("prp-ifp", vec![[q, ty, ifp], [s, q, y], [o, q, y]]);
                    }
                }
            }
        }
        for &(r, q, c, card) in &self.max_one {
            for &x in self.subjects(Some(q), s) {
                if !self.all.contains(&[x, q, o]) || !self.all.contains(&[x, ty, r]) {
                    continue;
                }
                let on = v.on_property.expect("a restriction has one");
                let mut premises = vec![card, [r, on, q], [x, ty, r], [x, q, s], [x, q, o]];
                match (c, v.on_class) {
                    (None, _) if v.max_cardinality == Some(card[1]) => add("cls-maxc2", premises),
                    (None, _) => add("cls-maxqc4", premises),
                    (Some(c), Some(on_class)) => {
                        if self.all.contains(&[s, ty, c]) && self.all.contains(&[o, ty, c]) {
                            premises.extend([[r, on_class, c], [s, ty, c], [o, ty, c]]);
                            add("cls-maxqc3", premises);
                        }
                    }
                    (Some(_), None) => {}
                }
            }
        }
        if let Some(key) = v.has_key {
            for &c in self.objects(s, Some(ty)) {
                if !self.all.contains(&[o, ty, c]) {
                    continue;
                }
                for &head in self.objects(c, Some(key)) {
                    let Some(keys) = self.list(head) else {
                        continue;
                    };
                    let mut premises = vec![[c, key, head], [s, ty, c], [o, ty, c]];
                    let shared = keys.iter().all(|&k| {
                        match self
                            .objects(s, Some(k))
                            .iter()
                            .find(|&&z| self.all.contains(&[o, k, z]))
                        {
                            Some(&z) => {
                                premises.extend([[s, k, z], [o, k, z]]);
                                true
                            }
                            None => false,
                        }
                    });
                    if shared {
                        add("prp-key", premises);
                    }
                }
            }
        }
    }

    /// A path `s p1 x1 … pn o` in the closure.
    fn path(&self, s: TermId, o: TermId, steps: &[TermId]) -> Option<Vec<[TermId; 3]>> {
        let (&last, init) = steps.split_last()?;
        if init.is_empty() {
            return self.all.contains(&[s, last, o]).then(|| vec![[s, last, o]]);
        }
        for &x in self.objects(s, Some(init[0])).iter().take(64) {
            if let Some(mut rest) = self.path(x, o, &steps[1..]) {
                rest.insert(0, [s, init[0], x]);
                return Some(rest);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::Regime;
    use super::{Cancelled, Derivation, Explainer};
    use crate::{Snapshot, Store};
    use oxigraph::io::RdfFormat;
    use oxigraph::model::NamedNodeRef;

    const PREFIXES: &str = "@prefix : <urn:> .\n\
        @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .\n\
        @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
        @prefix owl: <http://www.w3.org/2002/07/owl#> .\n";

    fn store(ttl: &str) -> Store {
        let store = Store::new();
        store
            .bulk_load(RdfFormat::Turtle, format!("{PREFIXES}{ttl}").as_bytes())
            .unwrap();
        store
    }

    /// The store's entailment written to a graph, as the server does (writing it adds the
    /// terms of the vocabulary the store lacked): the snapshot, the graph and its triples.
    pub(super) fn entailed(
        store: &Store,
        regime: Regime,
    ) -> (Snapshot, Option<crate::TermId>, Vec<[crate::TermId; 3]>) {
        let g = NamedNodeRef::new_unchecked("urn:entailed");
        store
            .transaction(|t| Ok::<_, crate::StoreError>(t.entail(g, regime)))
            .unwrap();
        let snapshot = store.snapshot();
        let graph = snapshot.term_id(g.into());
        let triples = graph.map_or_else(Vec::new, |g| {
            snapshot
                .quad_ids_for_pattern(None, None, None, crate::GraphSelector::Graph(g))
                .map(|q| [q[0], q[1], q[2]])
                .collect()
        });
        (snapshot, graph, triples)
    }

    fn id(snapshot: &Snapshot, iri: &str) -> crate::TermId {
        let iri = iri
            .replace("rdf:", "http://www.w3.org/1999/02/22-rdf-syntax-ns#")
            .replace("rdfs:", "http://www.w3.org/2000/01/rdf-schema#")
            .replace("owl:", "http://www.w3.org/2002/07/owl#");
        let iri = if iri.contains(':') {
            iri
        } else {
            format!("urn:{iri}")
        };
        snapshot
            .term_id(NamedNodeRef::new_unchecked(&iri).into())
            .unwrap_or_else(|| panic!("{iri} not in the store"))
    }

    /// The derivation as `rule(premise, …)` with triples as `s p o`, local names only.
    fn show(snapshot: &Snapshot, d: &Derivation) -> String {
        let name = |t| {
            let term = snapshot.term(t).unwrap().to_string();
            let term = term.trim_matches(|c| c == '<' || c == '>');
            term.rsplit(['#', ':']).next().unwrap().to_owned()
        };
        let triple = d.triple.map(name).join(" ");
        match d.rule {
            None => triple,
            Some(rule) => format!(
                "{rule}[{triple}]({})",
                d.premises
                    .iter()
                    .map(|p| show(snapshot, p))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn explain(ttl: &str, regime: Regime, triple: [&str; 3]) -> String {
        let store = store(ttl);
        let (snapshot, graph, entailed) = entailed(&store, regime);
        let explainer = Explainer::new(&snapshot, graph, regime, &entailed);
        let t = triple.map(|x| id(&snapshot, x));
        assert!(explainer.holds(t), "{triple:?} is not entailed");
        // Every entailed triple has a derivation.
        for &e in &entailed {
            assert!(
                explainer.derive(e, 100_000, &|| false).unwrap().is_some(),
                "no derivation of {e:?}"
            );
        }
        show(
            &snapshot,
            &explainer
                .derive(t, 100_000, &|| false)
                .unwrap()
                .expect("a derivation"),
        )
    }

    #[test]
    fn rdfs_derivations_go_down_to_asserted_triples() {
        let ttl = ":A rdfs:subClassOf :B . :B rdfs:subClassOf :C .\n\
                   :p rdfs:domain :A . :x :p :y .\n\
                   # rdf:type in the store, for the rules to write it.\n:z a :Other .";
        assert_eq!(
            explain(ttl, Regime::Rdfs, ["x", "rdf:type", "C"]),
            "rdfs9[x type C](rdfs11[A subClassOf C](A subClassOf B, B subClassOf C), rdfs2[x type A](p domain A, x p y))"
        );
        // Under OWL 2 RL, the OWL names, and the shallowest tree.
        assert_eq!(
            explain(ttl, Regime::OwlRl, ["A", "rdfs:subClassOf", "C"]),
            "scm-sco[A subClassOf C](A subClassOf B, B subClassOf C)"
        );
    }

    #[test]
    fn owl_rl_derivations() {
        // A transitive property over an inverse one.
        let ttl = ":partOf a owl:TransitiveProperty ; owl:inverseOf :hasPart .\n\
                   :a :hasPart :b . :b :hasPart :c .";
        assert_eq!(
            explain(ttl, Regime::OwlRl, ["c", "partOf", "a"]),
            "prp-trp[c partOf a](partOf type TransitiveProperty, prp-inv2[c partOf b](partOf inverseOf hasPart, b hasPart c), prp-inv2[b partOf a](partOf inverseOf hasPart, a hasPart b))"
        );
        // owl:sameAs from a functional property, then its triples copied.
        let ttl = ":mother a owl:FunctionalProperty . :x :mother :m1 , :m2 . :m1 :name \"Ann\" .\n\
                   :k1 owl:sameAs :k2 .";
        assert_eq!(
            explain(ttl, Regime::OwlRl, ["m1", "owl:sameAs", "m2"]),
            "prp-fp[m1 sameAs m2](mother type FunctionalProperty, x mother m1, x mother m2)"
        );
        // A value restriction.
        let ttl = ":Red owl:onProperty :colour ; owl:hasValue :red .\n\
                   :Car rdfs:subClassOf :Red . :c a :Car .";
        assert_eq!(
            explain(ttl, Regime::OwlRl, ["c", "colour", "red"]),
            "cls-hv2[c colour red](Red hasValue red, Red onProperty colour, cax-sco[c type Red](Car subClassOf Red, c type Car))"
        );
    }

    #[test]
    fn a_search_stops_when_cancelled_or_out_of_budget() {
        let mut ttl = String::new();
        for i in 0..60 {
            ttl.push_str(&format!(":n{i} owl:sameAs :n{} . ", i + 1));
        }
        ttl.push_str(":n0 :p :v .");
        let store = store(&ttl);
        let (snapshot, graph, entailed) = entailed(&store, Regime::OwlRl);
        let explainer = Explainer::new(&snapshot, graph, Regime::OwlRl, &entailed);
        let t = ["n60", "p", "v"].map(|x| id(&snapshot, x));
        assert!(explainer.holds(t));
        assert_eq!(explainer.derive(t, 100_000, &|| true), Err(Cancelled));
        assert_eq!(explainer.derive(t, 3, &|| false), Ok(None));
        let started = std::time::Instant::now();
        assert!(explainer.derive(t, 100_000, &|| false).unwrap().is_some());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn asserted_and_unknown_triples() {
        let store = store(":A rdfs:subClassOf :B . :x a :A .");
        let (snapshot, graph, entailed) = entailed(&store, Regime::Rdfs);
        let explainer = Explainer::new(&snapshot, graph, Regime::Rdfs, &entailed);
        let asserted = ["x", "rdf:type", "A"].map(|x| id(&snapshot, x));
        let d = explainer.derive(asserted, 10, &|| false).unwrap().unwrap();
        assert_eq!(d.rule, None);
        let absent = ["A", "rdf:type", "x"].map(|x| id(&snapshot, x));
        assert!(explainer.derive(absent, 10, &|| false).unwrap().is_none());
    }
}

/// Every entailed triple of each N-Quads file of `$EXPLAIN_FUZZ_DIR` has a derivation.
#[cfg(test)]
#[test]
#[ignore = "reads files of $EXPLAIN_FUZZ_DIR"]
fn every_entailed_triple_of_the_files_has_a_derivation() {
    let dir = std::env::var("EXPLAIN_FUZZ_DIR").expect("EXPLAIN_FUZZ_DIR");
    let mut missing = 0;
    let mut total = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let store = crate::Store::new();
        store
            .bulk_load(
                oxigraph::io::RdfFormat::NQuads,
                std::fs::File::open(&path).unwrap(),
            )
            .unwrap();
        let (snapshot, graph, entailed) = tests::entailed(&store, Regime::OwlRl);
        if entailed.len() > 3_000 {
            eprintln!("{}: {} triples, skipped", path.display(), entailed.len());
            continue;
        }
        let explainer = Explainer::new(&snapshot, graph, Regime::OwlRl, &entailed);
        let started = std::time::Instant::now();
        let before = missing;
        for &t in &entailed {
            total += 1;
            if explainer.derive(t, 10_000, &|| false).unwrap().is_none() {
                missing += 1;
                let [s, p, o] = t.map(|x| snapshot.term(x).unwrap().to_string());
                if missing - before <= 3 {
                    eprintln!("{}: {s} {p} {o}", path.display());
                }
            }
        }
        eprintln!(
            "{}: {} triples, {} missing, {:?}",
            path.display(),
            entailed.len(),
            missing - before,
            started.elapsed()
        );
    }
    eprintln!("{missing} of {total} without a derivation");
    assert_eq!(missing, 0);
}
