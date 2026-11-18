//! Where shapes and data are read from: a graph of the store (the tripletrail store or Oxigraph's),
//! an in-memory [`Graph`], or several of them read as one.

use oxigraph::model::{
    Graph, GraphName, GraphNameRef, NamedNode, NamedNodeRef, NamedOrBlankNodeRef, Term, TermRef,
    TripleRef,
};
use tripletrail_store::{Dataset, GraphSelector, Snapshot, SnapshotReader, TermId};

/// The lookups validation needs. Every method appends to `out`; a graph read through
/// [`DataGraph`] has set semantics, but a [`Union`] may repeat a triple found in two graphs.
pub trait DataGraph: Sync {
    /// The objects of `s p ?o`.
    fn objects(&self, s: &Term, p: NamedNodeRef<'_>, out: &mut Vec<Term>);
    /// The subjects of `?s p o`.
    fn subjects(&self, p: NamedNodeRef<'_>, o: &Term, out: &mut Vec<Term>);
    /// Every `p o` of subject `s`.
    fn outgoing(&self, s: &Term, out: &mut Vec<(NamedNode, Term)>);
    /// Every `s o` of predicate `p`.
    fn with_predicate(&self, p: NamedNodeRef<'_>, out: &mut Vec<(Term, Term)>);
    /// Every predicate used in the graph (with repeats).
    fn predicates(&self, out: &mut Vec<NamedNode>);
    /// Every triple matching `s p o` (each optional), for SPARQL over the graph.
    fn triples(
        &self,
        s: Option<&Term>,
        p: Option<NamedNodeRef<'_>>,
        o: Option<&Term>,
        out: &mut Vec<Triple3>,
    );

    /// Whether the graph has the triple `s p o`.
    fn contains(&self, s: &Term, p: NamedNodeRef<'_>, o: &Term) -> bool {
        let mut out = Vec::new();
        self.triples(Some(s), Some(p), Some(o), &mut out);
        !out.is_empty()
    }

    /// Whether `s p ?o` has a match.
    fn has_object(&self, s: &Term, p: NamedNodeRef<'_>) -> bool {
        let mut out = Vec::new();
        self.objects(s, p, &mut out);
        !out.is_empty()
    }

    /// Whether `?s p o` has a match.
    fn has_subject(&self, p: NamedNodeRef<'_>, o: &Term) -> bool {
        let mut out = Vec::new();
        self.subjects(p, o, &mut out);
        !out.is_empty()
    }

    /// The first object of `s p ?o`, if any.
    fn object(&self, s: &Term, p: NamedNodeRef<'_>) -> Option<Term> {
        let mut out = Vec::new();
        self.objects(s, p, &mut out);
        out.into_iter().next()
    }
}

/// A triple as `(subject, predicate, object)`.
pub type Triple3 = (Term, NamedNode, Term);

pub(crate) fn subject(t: &Term) -> Option<NamedOrBlankNodeRef<'_>> {
    match t {
        Term::NamedNode(n) => Some(n.into()),
        Term::BlankNode(b) => Some(b.into()),
        _ => None,
    }
}

impl DataGraph for Graph {
    fn objects(&self, s: &Term, p: NamedNodeRef<'_>, out: &mut Vec<Term>) {
        if let Some(s) = subject(s) {
            out.extend(
                self.objects_for_subject_predicate(s, p)
                    .map(TermRef::into_owned),
            );
        }
    }

    fn subjects(&self, p: NamedNodeRef<'_>, o: &Term, out: &mut Vec<Term>) {
        out.extend(
            self.subjects_for_predicate_object(p, o)
                .map(|s| s.into_owned().into()),
        );
    }

    fn outgoing(&self, s: &Term, out: &mut Vec<(NamedNode, Term)>) {
        if let Some(s) = subject(s) {
            out.extend(
                self.triples_for_subject(s)
                    .map(|t| (t.predicate.into_owned(), t.object.into_owned())),
            );
        }
    }

    fn with_predicate(&self, p: NamedNodeRef<'_>, out: &mut Vec<(Term, Term)>) {
        out.extend(
            self.triples_for_predicate(p)
                .map(|t| (t.subject.into_owned().into(), t.object.into_owned())),
        );
    }

    fn predicates(&self, out: &mut Vec<NamedNode>) {
        out.extend(self.iter().map(|t| t.predicate.into_owned()));
    }

    fn triples(
        &self,
        s: Option<&Term>,
        p: Option<NamedNodeRef<'_>>,
        o: Option<&Term>,
        out: &mut Vec<Triple3>,
    ) {
        let keep = |t: &TripleRef<'_>| {
            p.is_none_or(|p| t.predicate == p) && o.is_none_or(|o| t.object == o.as_ref())
        };
        let own = |t: TripleRef<'_>| -> Triple3 {
            (
                t.subject.into_owned().into(),
                t.predicate.into_owned(),
                t.object.into_owned(),
            )
        };
        match (s, p, o) {
            (Some(s), ..) => {
                if let Some(s) = subject(s) {
                    out.extend(self.triples_for_subject(s).filter(keep).map(own));
                }
            }
            (None, _, Some(o)) => {
                out.extend(self.triples_for_object(o).filter(keep).map(own));
            }
            (None, Some(p), None) => out.extend(self.triples_for_predicate(p).map(own)),
            (None, None, None) => out.extend(self.iter().map(own)),
        }
    }
}

/// One graph of a store: the default graph or a named graph. For the tripletrail store it reads a
/// snapshot taken when it is made, by term ids, decoding only the terms it returns.
pub struct StoreGraph {
    inner: Inner,
}

enum Inner {
    TripleTrail {
        /// Boxed: a snapshot holds the six permutations' deltas inline.
        snapshot: Box<Snapshot>,
        /// `None` when the named graph is not in the store (the graph is empty).
        graph: Option<GraphSelector>,
    },
    Oxigraph {
        store: oxigraph::store::Store,
        graph: GraphName,
    },
}

impl StoreGraph {
    pub fn new(dataset: &Dataset, graph: GraphNameRef<'_>) -> Self {
        let inner = match dataset {
            Dataset::TripleTrail(store) => {
                let snapshot = store.snapshot();
                let graph = match graph {
                    GraphNameRef::DefaultGraph => Some(GraphSelector::Default),
                    GraphNameRef::NamedNode(n) => {
                        snapshot.term_id(n.into()).map(GraphSelector::Graph)
                    }
                    GraphNameRef::BlankNode(b) => {
                        snapshot.term_id(b.into()).map(GraphSelector::Graph)
                    }
                };
                Inner::TripleTrail {
                    snapshot: Box::new(snapshot),
                    graph,
                }
            }
            Dataset::Oxigraph(store) => Inner::Oxigraph {
                store: store.clone(),
                graph: graph.into_owned(),
            },
        };
        Self { inner }
    }

    /// A graph of a snapshot of the tripletrail store, or with `None` every graph of it read as
    /// one (the default graph and every named graph; a triple in two graphs is found twice).
    /// Read it directly, not through [`StoreGraph::read`], when the caller looks terms up in
    /// the store meanwhile: each lookup then takes the dictionary's lock on its own.
    pub fn of_snapshot(snapshot: &Snapshot, graph: Option<GraphNameRef<'_>>) -> Self {
        let graph = match graph {
            None => Some(GraphSelector::All),
            Some(GraphNameRef::DefaultGraph) => Some(GraphSelector::Default),
            Some(GraphNameRef::NamedNode(n)) => {
                snapshot.term_id(n.into()).map(GraphSelector::Graph)
            }
            Some(GraphNameRef::BlankNode(b)) => {
                snapshot.term_id(b.into()).map(GraphSelector::Graph)
            }
        };
        Self {
            inner: Inner::TripleTrail {
                snapshot: Box::new(snapshot.clone()),
                graph,
            },
        }
    }

    /// Runs `f` on this graph read with the store's dictionary locked once for the whole run
    /// (the tripletrail store; see [`tripletrail_store::SnapshotReader`]) rather than at each lookup,
    /// which is faster and scales over threads. Call it once per thread for a batch of nodes.
    pub fn read<R>(&self, f: impl FnOnce(&dyn DataGraph) -> R) -> R {
        match &self.inner {
            Inner::TripleTrail { snapshot, graph } => f(&Locked {
                reader: snapshot.reader(),
                graph: *graph,
            }),
            Inner::Oxigraph { .. } => f(self),
        }
    }
}

/// A graph of the tripletrail store read with its dictionary locked.
struct Locked<'a> {
    reader: SnapshotReader<'a>,
    graph: Option<GraphSelector>,
}

/// Term id lookups of the tripletrail store, with or without the dictionary locked.
trait Ids {
    fn id(&self, t: TermRef<'_>) -> Option<TermId>;
    fn term(&self, id: TermId) -> Option<Term>;
    fn quads(
        &self,
        s: Option<TermId>,
        p: Option<TermId>,
        o: Option<TermId>,
        g: GraphSelector,
    ) -> impl Iterator<Item = [TermId; 4]>;
}

impl Ids for Snapshot {
    fn id(&self, t: TermRef<'_>) -> Option<TermId> {
        self.term_id(t)
    }
    fn term(&self, id: TermId) -> Option<Term> {
        Snapshot::term(self, id).ok()
    }
    fn quads(
        &self,
        s: Option<TermId>,
        p: Option<TermId>,
        o: Option<TermId>,
        g: GraphSelector,
    ) -> impl Iterator<Item = [TermId; 4]> {
        self.quad_ids_for_pattern(s, p, o, g)
    }
}

impl Ids for SnapshotReader<'_> {
    fn id(&self, t: TermRef<'_>) -> Option<TermId> {
        self.term_id(t)
    }
    fn term(&self, id: TermId) -> Option<Term> {
        SnapshotReader::term(self, id).ok()
    }
    fn quads(
        &self,
        s: Option<TermId>,
        p: Option<TermId>,
        o: Option<TermId>,
        g: GraphSelector,
    ) -> impl Iterator<Item = [TermId; 4]> {
        self.quad_ids_for_pattern(s, p, o, g)
    }
}

/// A match of a scan: `(s, p, o)`, with only the positions asked for decoded.
type Match = (Option<Term>, Option<NamedNode>, Option<Term>);

fn scan_ids(
    ids: &impl Ids,
    graph: Option<GraphSelector>,
    (s, p, o): (Option<&Term>, Option<NamedNodeRef<'_>>, Option<&Term>),
    decode: [bool; 3],
    f: &mut dyn FnMut(Match),
) {
    let Some(graph) = graph else { return };
    let id = |t: Option<TermRef<'_>>| match t {
        None => Some(None),
        Some(t) => ids.id(t).map(Some),
    };
    let (Some(s), Some(p), Some(o)) = (
        id(s.map(Term::as_ref)),
        id(p.map(Into::into)),
        id(o.map(Term::as_ref)),
    ) else {
        return;
    };
    for q in ids.quads(s, p, o, graph) {
        let term = |i: usize| if decode[i] { ids.term(q[i]) } else { None };
        let predicate = match term(1) {
            Some(Term::NamedNode(n)) => Some(n),
            _ => None,
        };
        f((term(0), predicate, term(2)));
    }
}

/// A graph read by scanning triple patterns; [`DataGraph`] follows.
trait Scan: Sync {
    /// Scans `s p o` (each optional) and hands every match to `f`, decoding only the
    /// positions `decode` asks for (`[s, p, o]`; the others are `None`).
    fn scan(
        &self,
        spo: (Option<&Term>, Option<NamedNodeRef<'_>>, Option<&Term>),
        decode: [bool; 3],
        f: &mut dyn FnMut(Match),
    );
}

impl Scan for StoreGraph {
    fn scan(
        &self,
        spo: (Option<&Term>, Option<NamedNodeRef<'_>>, Option<&Term>),
        decode: [bool; 3],
        f: &mut dyn FnMut(Match),
    ) {
        let (s, p, o) = spo;
        if s.is_some_and(|s| subject(s).is_none()) {
            return;
        }
        match &self.inner {
            Inner::TripleTrail { snapshot, graph } => scan_ids(&**snapshot, *graph, spo, decode, f),
            Inner::Oxigraph { store, graph } => {
                for q in store
                    .quads_for_pattern(
                        s.and_then(subject),
                        p,
                        o.map(Term::as_ref),
                        Some(graph.as_ref()),
                    )
                    .flatten()
                {
                    f((
                        decode[0].then(|| q.subject.into()),
                        decode[1].then_some(q.predicate),
                        decode[2].then_some(q.object),
                    ));
                }
            }
        }
    }
}

impl Scan for Locked<'_> {
    fn scan(
        &self,
        spo: (Option<&Term>, Option<NamedNodeRef<'_>>, Option<&Term>),
        decode: [bool; 3],
        f: &mut dyn FnMut(Match),
    ) {
        if spo.0.is_some_and(|s| subject(s).is_none()) {
            return;
        }
        scan_ids(&self.reader, self.graph, spo, decode, f);
    }
}

impl<T: Scan> DataGraph for T {
    fn objects(&self, s: &Term, p: NamedNodeRef<'_>, out: &mut Vec<Term>) {
        self.scan(
            (Some(s), Some(p), None),
            [false, false, true],
            &mut |(_, _, o)| out.extend(o),
        );
    }

    fn subjects(&self, p: NamedNodeRef<'_>, o: &Term, out: &mut Vec<Term>) {
        self.scan(
            (None, Some(p), Some(o)),
            [true, false, false],
            &mut |(s, _, _)| out.extend(s),
        );
    }

    fn outgoing(&self, s: &Term, out: &mut Vec<(NamedNode, Term)>) {
        self.scan((Some(s), None, None), [false, true, true], &mut |(
            _,
            p,
            o,
        )| {
            out.extend(p.zip(o))
        });
    }

    fn with_predicate(&self, p: NamedNodeRef<'_>, out: &mut Vec<(Term, Term)>) {
        self.scan((None, Some(p), None), [true, false, true], &mut |(
            s,
            _,
            o,
        )| {
            out.extend(s.zip(o))
        });
    }

    fn predicates(&self, out: &mut Vec<NamedNode>) {
        self.scan((None, None, None), [false, true, false], &mut |(
            _,
            p,
            _,
        )| {
            out.extend(p)
        });
    }

    fn triples(
        &self,
        s: Option<&Term>,
        p: Option<NamedNodeRef<'_>>,
        o: Option<&Term>,
        out: &mut Vec<Triple3>,
    ) {
        self.scan(
            (s, p, o),
            [s.is_none(), p.is_none(), o.is_none()],
            &mut |(ms, mp, mo)| {
                if let (Some(ts), Some(tp), Some(to)) = (
                    ms.or_else(|| s.cloned()),
                    mp.or_else(|| p.map(NamedNodeRef::into_owned)),
                    mo.or_else(|| o.cloned()),
                ) {
                    out.push((ts, tp, to));
                }
            },
        );
    }

    fn contains(&self, s: &Term, p: NamedNodeRef<'_>, o: &Term) -> bool {
        let mut found = false;
        self.scan((Some(s), Some(p), Some(o)), [false; 3], &mut |_| {
            found = true
        });
        found
    }

    fn has_object(&self, s: &Term, p: NamedNodeRef<'_>) -> bool {
        let mut found = false;
        self.scan((Some(s), Some(p), None), [false; 3], &mut |_| found = true);
        found
    }

    fn has_subject(&self, p: NamedNodeRef<'_>, o: &Term) -> bool {
        let mut found = false;
        self.scan((None, Some(p), Some(o)), [false; 3], &mut |_| found = true);
        found
    }
}

/// Several graphs read as one (their union).
pub struct Union<'a>(pub Vec<&'a dyn DataGraph>);

impl DataGraph for Union<'_> {
    fn objects(&self, s: &Term, p: NamedNodeRef<'_>, out: &mut Vec<Term>) {
        for g in &self.0 {
            g.objects(s, p, out);
        }
    }

    fn subjects(&self, p: NamedNodeRef<'_>, o: &Term, out: &mut Vec<Term>) {
        for g in &self.0 {
            g.subjects(p, o, out);
        }
    }

    fn outgoing(&self, s: &Term, out: &mut Vec<(NamedNode, Term)>) {
        for g in &self.0 {
            g.outgoing(s, out);
        }
    }

    fn with_predicate(&self, p: NamedNodeRef<'_>, out: &mut Vec<(Term, Term)>) {
        for g in &self.0 {
            g.with_predicate(p, out);
        }
    }

    fn predicates(&self, out: &mut Vec<NamedNode>) {
        for g in &self.0 {
            g.predicates(out);
        }
    }

    fn triples(
        &self,
        s: Option<&Term>,
        p: Option<NamedNodeRef<'_>>,
        o: Option<&Term>,
        out: &mut Vec<Triple3>,
    ) {
        for g in &self.0 {
            g.triples(s, p, o, out);
        }
    }

    fn contains(&self, s: &Term, p: NamedNodeRef<'_>, o: &Term) -> bool {
        self.0.iter().any(|g| g.contains(s, p, o))
    }

    fn has_object(&self, s: &Term, p: NamedNodeRef<'_>) -> bool {
        self.0.iter().any(|g| g.has_object(s, p))
    }

    fn has_subject(&self, p: NamedNodeRef<'_>, o: &Term) -> bool {
        self.0.iter().any(|g| g.has_subject(p, o))
    }
}

/// A graph as it would be after a change: `base` without the triples `removed`, with the
/// triples of `added`. For checking a change before it is written.
pub struct Edited<'a> {
    base: &'a dyn DataGraph,
    removed: std::collections::HashSet<Triple3>,
    added: Graph,
}

impl<'a> Edited<'a> {
    pub fn new(
        base: &'a dyn DataGraph,
        removed: impl IntoIterator<Item = Triple3>,
        added: Graph,
    ) -> Self {
        Self {
            base,
            removed: removed.into_iter().collect(),
            added,
        }
    }

    fn kept(&self, s: &Term, p: NamedNodeRef<'_>, o: &Term) -> bool {
        self.removed.is_empty()
            || !self
                .removed
                .contains(&(s.clone(), p.into_owned(), o.clone()))
    }
}

impl DataGraph for Edited<'_> {
    fn objects(&self, s: &Term, p: NamedNodeRef<'_>, out: &mut Vec<Term>) {
        let mut found = Vec::new();
        self.base.objects(s, p, &mut found);
        found.retain(|o| self.kept(s, p, o));
        self.added.objects(s, p, &mut found);
        found.sort_by_cached_key(Term::to_string);
        found.dedup();
        out.extend(found);
    }

    fn subjects(&self, p: NamedNodeRef<'_>, o: &Term, out: &mut Vec<Term>) {
        let mut found = Vec::new();
        self.base.subjects(p, o, &mut found);
        found.retain(|s| self.kept(s, p, o));
        self.added.subjects(p, o, &mut found);
        found.sort_by_cached_key(Term::to_string);
        found.dedup();
        out.extend(found);
    }

    fn outgoing(&self, s: &Term, out: &mut Vec<(NamedNode, Term)>) {
        let mut found = Vec::new();
        self.base.outgoing(s, &mut found);
        found.retain(|(p, o)| self.kept(s, p.as_ref(), o));
        self.added.outgoing(s, &mut found);
        out.extend(found);
    }

    fn with_predicate(&self, p: NamedNodeRef<'_>, out: &mut Vec<(Term, Term)>) {
        let mut found = Vec::new();
        self.base.with_predicate(p, &mut found);
        found.retain(|(s, o)| self.kept(s, p, o));
        self.added.with_predicate(p, &mut found);
        out.extend(found);
    }

    fn predicates(&self, out: &mut Vec<NamedNode>) {
        self.base.predicates(out);
        self.added.predicates(out);
    }

    fn triples(
        &self,
        s: Option<&Term>,
        p: Option<NamedNodeRef<'_>>,
        o: Option<&Term>,
        out: &mut Vec<Triple3>,
    ) {
        let mut found = Vec::new();
        self.base.triples(s, p, o, &mut found);
        found.retain(|t| self.removed.is_empty() || !self.removed.contains(t));
        self.added.triples(s, p, o, &mut found);
        out.extend(found);
    }
}

#[cfg(test)]
mod edited_tests {
    use super::*;
    use oxigraph::model::{Literal, Triple};

    #[test]
    fn an_edited_graph_drops_and_adds_triples() {
        let ex = |l: &str| NamedNode::new_unchecked(format!("http://example.org/{l}"));
        let mut base = Graph::new();
        base.insert(&Triple::new(ex("a"), ex("p"), Literal::from(1)));
        base.insert(&Triple::new(ex("a"), ex("p"), Literal::from(2)));
        let mut added = Graph::new();
        added.insert(&Triple::new(ex("a"), ex("p"), Literal::from(3)));
        let a: Term = ex("a").into();
        let edited = Edited::new(
            &base,
            [(a.clone(), ex("p"), Literal::from(1).into())],
            added,
        );
        let mut objects = Vec::new();
        edited.objects(&a, ex("p").as_ref(), &mut objects);
        assert_eq!(
            objects,
            vec![Term::from(Literal::from(2)), Literal::from(3).into()]
        );
        assert!(!edited.contains(&a, ex("p").as_ref(), &Literal::from(1).into()));
        let mut all = Vec::new();
        edited.triples(None, None, None, &mut all);
        assert_eq!(all.len(), 2);
    }
}
