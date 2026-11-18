//! The triples inference adds, in the order they came, with hash indexes on subject,
//! predicate and object. Inserting is a few hash lookups, where an oxigraph `Graph` interns
//! every term and updates six ordered indexes: rules on large graphs insert millions.

use crate::graph::{DataGraph, Triple3, subject};
use oxigraph::model::{
    NamedNode, NamedNodeRef, NamedOrBlankNode, NamedOrBlankNodeRef, Term, TermRef, Triple,
};
use rustc_hash::FxHashMap;
use std::fmt;

/// A set of triples that keeps their insertion order ([`Inferred::as_slice`]).
#[derive(Default, Clone)]
pub struct Inferred {
    triples: Vec<Triple>,
    by_subject: FxHashMap<NamedOrBlankNode, Vec<u32>>,
    by_predicate: FxHashMap<NamedNode, Vec<u32>>,
    by_object: FxHashMap<Term, Vec<u32>>,
    /// Membership: the first triple with each hash of [`key`], and the others with the same
    /// hash (rare), so finding a triple does not scan its subject's triples.
    first: FxHashMap<u64, u32>,
    more: FxHashMap<u64, Vec<u32>>,
}

fn key(s: NamedOrBlankNodeRef<'_>, p: NamedNodeRef<'_>, o: TermRef<'_>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    (s, p, o).hash(&mut h);
    h.finish()
}

impl Inferred {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.triples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.triples.is_empty()
    }

    /// The triples in the order they were inserted.
    pub fn as_slice(&self) -> &[Triple] {
        &self.triples
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Triple> {
        self.triples.iter()
    }

    pub fn contains(&self, t: &Triple) -> bool {
        self.find(t.subject.as_ref(), t.predicate.as_ref(), t.object.as_ref())
            .1
    }

    /// The hash of `s p o`, and whether the triple is there.
    fn find(&self, s: NamedOrBlankNodeRef<'_>, p: NamedNodeRef<'_>, o: TermRef<'_>) -> (u64, bool) {
        let h = key(s, p, o);
        let is = |i: u32| {
            let t = &self.triples[i as usize];
            t.subject.as_ref() == s && t.predicate == p && t.object.as_ref() == o
        };
        let found = self
            .first
            .get(&h)
            .is_some_and(|&i| is(i) || self.more.get(&h).is_some_and(|v| v.iter().any(|&j| is(j))));
        (h, found)
    }

    /// Adds `t`; false when it is already there.
    pub fn insert(&mut self, t: &Triple) -> bool {
        let (h, found) = self.find(t.subject.as_ref(), t.predicate.as_ref(), t.object.as_ref());
        if found {
            return false;
        }
        let i = u32::try_from(self.triples.len()).expect("fewer than 2^32 inferred triples");
        match self.first.entry(h) {
            std::collections::hash_map::Entry::Occupied(_) => {
                self.more.entry(h).or_default().push(i)
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(i);
            }
        }
        push(&mut self.by_subject, &t.subject, i);
        push(&mut self.by_predicate, &t.predicate, i);
        push(&mut self.by_object, &t.object, i);
        self.triples.push(t.clone());
        true
    }

    fn at(&self, ids: Option<&Vec<u32>>) -> impl Iterator<Item = &Triple> {
        ids.into_iter()
            .flatten()
            .map(|&i| &self.triples[i as usize])
    }

    /// The triples that may match `s p o`, from the most selective index.
    fn lookup(
        &self,
        s: Option<&Term>,
        p: Option<NamedNodeRef<'_>>,
        o: Option<&Term>,
    ) -> Box<dyn Iterator<Item = &Triple> + '_> {
        if let Some(s) = s {
            return match subject(s) {
                Some(s) => Box::new(self.at(self.by_subject.get(&s.into_owned()))),
                None => Box::new(std::iter::empty()),
            };
        }
        if let Some(o) = o {
            return Box::new(self.at(self.by_object.get(o)));
        }
        match p {
            Some(p) => Box::new(self.at(self.by_predicate.get(&p.into_owned()))),
            None => Box::new(self.triples.iter()),
        }
    }
}

fn push<K: std::hash::Hash + Eq + Clone>(map: &mut FxHashMap<K, Vec<u32>>, key: &K, i: u32) {
    match map.get_mut(key) {
        Some(v) => v.push(i),
        None => {
            map.insert(key.clone(), vec![i]);
        }
    }
}

impl<'a> IntoIterator for &'a Inferred {
    type Item = &'a Triple;
    type IntoIter = std::slice::Iter<'a, Triple>;

    fn into_iter(self) -> Self::IntoIter {
        self.triples.iter()
    }
}

impl Extend<Triple> for Inferred {
    fn extend<I: IntoIterator<Item = Triple>>(&mut self, iter: I) {
        for t in iter {
            self.insert(&t);
        }
    }
}

impl FromIterator<Triple> for Inferred {
    fn from_iter<I: IntoIterator<Item = Triple>>(iter: I) -> Self {
        let mut out = Self::new();
        out.extend(iter);
        out
    }
}

impl fmt::Debug for Inferred {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(&self.triples).finish()
    }
}

/// N-Triples, one triple per line.
impl fmt::Display for Inferred {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for t in &self.triples {
            writeln!(f, "{t} .")?;
        }
        Ok(())
    }
}

impl DataGraph for Inferred {
    fn objects(&self, s: &Term, p: NamedNodeRef<'_>, out: &mut Vec<Term>) {
        out.extend(
            self.lookup(Some(s), Some(p), None)
                .filter(|t| t.predicate == p)
                .map(|t| t.object.clone()),
        );
    }

    fn subjects(&self, p: NamedNodeRef<'_>, o: &Term, out: &mut Vec<Term>) {
        out.extend(
            self.lookup(None, Some(p), Some(o))
                .filter(|t| t.predicate == p)
                .map(|t| t.subject.clone().into()),
        );
    }

    fn outgoing(&self, s: &Term, out: &mut Vec<(NamedNode, Term)>) {
        out.extend(
            self.lookup(Some(s), None, None)
                .map(|t| (t.predicate.clone(), t.object.clone())),
        );
    }

    fn with_predicate(&self, p: NamedNodeRef<'_>, out: &mut Vec<(Term, Term)>) {
        out.extend(
            self.lookup(None, Some(p), None)
                .map(|t| (t.subject.clone().into(), t.object.clone())),
        );
    }

    fn predicates(&self, out: &mut Vec<NamedNode>) {
        out.extend(self.by_predicate.keys().cloned());
    }

    fn triples(
        &self,
        s: Option<&Term>,
        p: Option<NamedNodeRef<'_>>,
        o: Option<&Term>,
        out: &mut Vec<Triple3>,
    ) {
        out.extend(
            self.lookup(s, p, o)
                .filter(|t| {
                    s.is_none_or(|s| subject(s) == Some(t.subject.as_ref()))
                        && p.is_none_or(|p| t.predicate == p)
                        && o.is_none_or(|o| t.object == *o)
                })
                .map(|t| {
                    (
                        t.subject.clone().into(),
                        t.predicate.clone(),
                        t.object.clone(),
                    )
                }),
        );
    }

    fn contains(&self, s: &Term, p: NamedNodeRef<'_>, o: &Term) -> bool {
        subject(s).is_some_and(|s| self.find(s, p, o.as_ref()).1)
    }

    fn has_object(&self, s: &Term, p: NamedNodeRef<'_>) -> bool {
        self.lookup(Some(s), None, None).any(|t| t.predicate == p)
    }

    fn has_subject(&self, p: NamedNodeRef<'_>, o: &Term) -> bool {
        self.lookup(None, None, Some(o)).any(|t| t.predicate == p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_find_what_was_inserted() {
        let n = |s: &str| NamedNode::new(format!("http://e/{s}")).unwrap();
        let t = |s: &str, p: &str, o: &str| Triple::new(n(s), n(p), n(o));
        let mut g = Inferred::new();
        assert!(g.insert(&t("a", "p", "b")));
        assert!(g.insert(&t("a", "q", "c")));
        assert!(g.insert(&t("b", "p", "c")));
        assert!(!g.insert(&t("a", "p", "b")));
        assert_eq!(g.len(), 3);
        assert_eq!(g.as_slice()[2], t("b", "p", "c"));
        let (a, p, c): (Term, _, Term) = (n("a").into(), n("p"), n("c").into());
        let mut objects = Vec::new();
        g.objects(&a, p.as_ref(), &mut objects);
        assert_eq!(objects, [Term::from(n("b"))]);
        let mut subjects = Vec::new();
        g.subjects(p.as_ref(), &c, &mut subjects);
        assert_eq!(subjects, [Term::from(n("b"))]);
        let mut all = Vec::new();
        g.triples(None, Some(p.as_ref()), None, &mut all);
        assert_eq!(all.len(), 2);
        assert!(DataGraph::contains(&g, &a, n("q").as_ref(), &c));
        assert!(!DataGraph::contains(&g, &a, p.as_ref(), &c));
        assert!(g.has_subject(n("q").as_ref(), &c));
        assert!(!g.has_object(&c, p.as_ref()));
    }
}
