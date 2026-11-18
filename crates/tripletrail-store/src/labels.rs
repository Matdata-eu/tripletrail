//! Blank node labels of loaded documents (#840).
//!
//! A blank node label is scoped to its document: `_:b1` in two files is two blank nodes, as
//! Jena's loaders and `riot` have it. A bulk load writes each label as a keyed hash of it,
//! with one key for the whole load: the same for every block and part that the parallel
//! N-Triples path parses, so a label that two parts of one file share is still one node.
//! The new label is 32 hex digits whatever the old one was, so a dump loaded again (a backup
//! restored, an ETL step) does not make labels longer each time.

use oxigraph::model::{
    BlankNode, GraphName, GraphNameRef, NamedOrBlankNode, NamedOrBlankNodeRef, Quad, QuadRef, Term,
    TermRef, Triple, TripleRef,
};
use std::hash::{BuildHasher, Hasher};

/// What a load does with the blank node labels of its document.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BlankNodes {
    /// The document's blank nodes are its own: `_:b1` here is not `_:b1` of another load,
    /// nor of the data already stored (Jena, rdflib).
    #[default]
    PerLoad,
    /// Labels are kept as written, so a document can refer to blank nodes already stored
    /// (pyoxigraph's `Store.load`, a dump loaded in parts).
    Keep,
}

/// The key one load hashes its labels with.
pub(crate) struct Scope {
    /// SipHash keys: random per process (std seeds `RandomState` from the OS) and new for
    /// each call.
    keys: std::collections::hash_map::RandomState,
    /// The time of the load, so loads in later runs of the same store differ too.
    salt: u128,
}

impl Scope {
    pub(crate) fn new() -> Self {
        Self {
            keys: std::collections::hash_map::RandomState::new(),
            salt: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        }
    }

    /// 128 bits of keyed hash of the label: two SipHash runs that differ in their first byte.
    fn blank(&self, b: &BlankNode) -> BlankNode {
        let half = |tag: u8| {
            let mut h = self.keys.build_hasher();
            h.write_u8(tag);
            h.write_u128(self.salt);
            h.write(b.as_str().as_bytes());
            h.finish()
        };
        BlankNode::new_unchecked(format!("{:016x}{:016x}", half(0), half(1)))
    }

    fn subject(&self, s: NamedOrBlankNodeRef<'_>) -> NamedOrBlankNode {
        match s {
            NamedOrBlankNodeRef::NamedNode(n) => n.into_owned().into(),
            NamedOrBlankNodeRef::BlankNode(b) => self.blank(&b.into_owned()).into(),
        }
    }

    fn triple(&self, t: TripleRef<'_>) -> Triple {
        Triple::new(
            self.subject(t.subject),
            t.predicate.into_owned(),
            self.term(t.object),
        )
    }

    fn term(&self, t: TermRef<'_>) -> Term {
        match t {
            TermRef::BlankNode(b) => self.blank(&b.into_owned()).into(),
            TermRef::Triple(t) => self.triple(t.as_ref()).into(),
            t => t.into_owned(),
        }
    }

    /// `q` with its labels scoped, or `None` when it has no blank node (the common case,
    /// which costs only this check).
    pub(crate) fn quad(&self, q: QuadRef<'_>) -> Option<Quad> {
        if !has_blank(q) {
            return None;
        }
        Some(Quad::new(
            self.subject(q.subject),
            q.predicate.into_owned(),
            self.term(q.object),
            match q.graph_name {
                GraphNameRef::BlankNode(b) => GraphName::BlankNode(self.blank(&b.into_owned())),
                g => g.into_owned(),
            },
        ))
    }
}

fn term_has_blank(t: TermRef<'_>) -> bool {
    match t {
        TermRef::BlankNode(_) => true,
        TermRef::Triple(t) => t.subject.is_blank_node() || term_has_blank(t.object.as_ref()),
        _ => false,
    }
}

fn has_blank(q: QuadRef<'_>) -> bool {
    q.subject.is_blank_node()
        || term_has_blank(q.object)
        || matches!(q.graph_name, GraphNameRef::BlankNode(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::model::NamedNode;

    #[test]
    fn scopes_every_blank_node_and_leaves_the_rest() {
        let scope = Scope::new();
        let p = NamedNode::new_unchecked("http://e/p");
        let b = BlankNode::new_unchecked("b1");
        let inner = Triple::new(b.clone(), p.clone(), b.clone());
        let q = Quad::new(b.clone(), p.clone(), inner, GraphName::BlankNode(b.clone()));
        let scoped = scope.quad(q.as_ref()).unwrap();
        let expected = scope.blank(&b);
        assert_ne!(expected, b);
        assert_eq!(scoped.subject, NamedOrBlankNode::from(expected.clone()));
        assert_eq!(scoped.graph_name, GraphName::BlankNode(expected.clone()));
        let Term::Triple(t) = &scoped.object else {
            panic!("{scoped}")
        };
        assert_eq!(t.subject, NamedOrBlankNode::from(expected.clone()));
        assert_eq!(t.object, Term::from(expected));
        let plain = Quad::new(p.clone(), p.clone(), p, GraphName::DefaultGraph);
        assert!(scope.quad(plain.as_ref()).is_none());
        // The same label is the same node in one load, another in the next; 32 hex digits
        // whatever the label's length, so loading a dump again does not lengthen it.
        assert_eq!(scope.blank(&b), scope.blank(&b));
        assert_ne!(Scope::new().blank(&b), scope.blank(&b));
        assert_ne!(
            scope.blank(&BlankNode::new_unchecked("b2")),
            scope.blank(&b)
        );
        let mut label = BlankNode::new_unchecked("a-much-longer-label-than-32-hex-digits-is");
        for _ in 0..3 {
            label = Scope::new().blank(&label);
            assert_eq!(label.as_str().len(), 32, "{label}");
        }
    }
}
