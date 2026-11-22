//! Order-preserving ranks of string-like dictionary terms (issue #171).
//!
//! Term ids are dictionary ordinals in insertion order, which the spatial index and the
//! routing graphs rely on. So instead of renumbering the terms, [`StringRanks`] gives each IRI,
//! blank node, `xsd:string` literal and language-tagged string its position in the order of
//! its kind, the one SPARQL's evaluator sorts them in: the order of the text's bytes (code
//! points), and for language-tagged strings that of the value, then of the language tag.
//! Comparing ranks then replaces decoding the terms for `MIN`, `MAX`, `ORDER BY` and prefix
//! filters.

use crate::dict::{Dictionary, Text};
use crate::id::{Kind, TermId};
use rayon::prelude::*;
use std::ops::Range;

/// The kinds that get ranks.
const RANKED: [Kind; 4] = [Kind::Iri, Kind::Blank, Kind::String, Kind::LangString];

/// Ranks of the IRIs, blank nodes, `xsd:string` literals and language-tagged strings (without
/// a base direction) of a dictionary, as it was when they were built: terms added since have
/// none (see [`StringRanks::rank`]).
#[derive(Debug, Default)]
pub struct StringRanks {
    /// Ordinals covered: `1..=len`.
    len: u32,
    /// Per ordinal (index 0 unused), its rank within its kind; `u32::MAX` for other kinds.
    rank: Vec<u32>,
    /// Per ranked kind, the ordinals in rank order.
    sorted: [Vec<u32>; 4],
}

fn slot(kind: Kind) -> Option<usize> {
    RANKED.iter().position(|&k| k == kind)
}

/// The text a term of a ranked kind sorts by, from its dictionary key, then (for a language
/// string) its language tag; `None` for a language string with a base direction, which gets
/// no rank. The key is written in full: see [`text_key`] for a key of `dict`.
pub(crate) fn sort_key(key: &[u8]) -> Option<(&[u8], &[u8])> {
    let rest = &key[1..];
    if key[0] != Kind::LangString as u8 {
        return Some((rest, &[]));
    }
    // The language tag, `--` and a direction for a directional string, a 0, the value.
    let zero = rest.iter().position(|&b| b == 0)?;
    let (language, value) = (&rest[..zero], &rest[zero + 1..]);
    if language.windows(2).any(|w| w == b"--") {
        return None;
    }
    Some((value, language))
}

/// [`sort_key`] of a key of `dict`, which may be a compact IRI key (#152).
pub(crate) fn text_key<'a>(dict: &'a Dictionary, key: &'a [u8]) -> Option<(Text<'a>, &'a [u8])> {
    if let Some(iri) = dict.iri_text(key) {
        return Some((iri, &[]));
    }
    sort_key(key).map(|(text, tag)| (Text(text, &[]), tag))
}

impl StringRanks {
    /// Ranks every ranked term of `dict`.
    pub(crate) fn build(dict: &Dictionary) -> Self {
        let len = dict.len() as u32;
        let mut by_kind: [Vec<u32>; 4] = Default::default();
        for o in 1..=len {
            let key = dict.raw_key(o);
            if let Some(s) = RANKED.iter().position(|&k| k as u8 == key[0])
                && text_key(dict, key).is_some()
            {
                by_kind[s].push(o);
            }
        }
        let text = |o: u32| text_key(dict, dict.raw_key(o)).expect("a ranked term");
        for list in &mut by_kind {
            list.par_sort_unstable_by(|&a, &b| text(a).cmp(&text(b)));
        }
        let mut rank = vec![u32::MAX; len as usize + 1];
        for list in &by_kind {
            for (r, &o) in list.iter().enumerate() {
                rank[o as usize] = r as u32;
            }
        }
        Self {
            len,
            rank,
            sorted: by_kind,
        }
    }

    /// Ordinals covered, `1..=covered()`: the dictionary's size when these were built.
    pub(crate) fn covered(&self) -> u32 {
        self.len
    }

    /// The rank of an IRI, blank node, `xsd:string` literal or language-tagged string among
    /// the terms of its kind: ranks of one kind sort as the terms do in SPARQL (by the text's
    /// bytes, which is code point order; language strings by value, then language tag).
    /// `None` for other kinds, for language strings with a base direction, and for terms
    /// added after these ranks were built.
    pub fn rank(&self, id: TermId) -> Option<u32> {
        slot(id.kind())?;
        let o = id.payload();
        if o == 0 || o > u64::from(self.len) {
            return None;
        }
        Some(self.rank[o as usize]).filter(|&r| r != u32::MAX)
    }

    /// The number of ranked terms of `kind`.
    pub fn count(&self, kind: Kind) -> u32 {
        slot(kind).map_or(0, |s| self.sorted[s].len() as u32)
    }

    /// The term of `kind` at `rank`.
    pub fn id(&self, kind: Kind, rank: u32) -> Option<TermId> {
        let o = *self.sorted[slot(kind)?].get(rank as usize)?;
        Some(TermId::new(kind, u64::from(o)))
    }

    /// The ranks of the terms of `kind` whose text (for a language string, its value) starts
    /// with `prefix`, read from `dict` (the one these ranks were built from). See
    /// [`crate::Snapshot::string_prefix`].
    pub(crate) fn prefix(&self, dict: &Dictionary, kind: Kind, prefix: &str) -> Range<u32> {
        let Some(s) = slot(kind) else { return 0..0 };
        let list = &self.sorted[s];
        let prefix = prefix.as_bytes();
        let text = |o: u32| text_key(dict, dict.raw_key(o)).expect("a ranked term").0;
        let lo = list.partition_point(|&o| text(o).cmp_bytes(prefix).is_lt());
        let hi = lo + list[lo..].partition_point(|&o| text(o).starts_with(prefix));
        lo as u32..hi as u32
    }

    /// Bytes held.
    pub fn heap_size(&self) -> usize {
        self.rank.capacity() * 4 + self.sorted.iter().map(|l| l.capacity() * 4).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::{Encoded, encode};
    use oxigraph::model::{BaseDirection, BlankNode, Literal, NamedNode, Term};

    #[test]
    fn ranks_sort_like_the_terms_and_find_prefixes() {
        let terms: Vec<Term> = vec![
            NamedNode::new_unchecked("http://e/b").into(),
            Literal::new_simple_literal("Cz").into(),
            NamedNode::new_unchecked("http://e/a").into(),
            Literal::new_simple_literal("é").into(),
            Literal::new_simple_literal("C").into(),
            Literal::new_language_tagged_literal_unchecked("C", "en").into(),
            BlankNode::new_unchecked("x").into(),
            Literal::new_simple_literal("Ca").into(),
            Literal::new_simple_literal("B").into(),
            Literal::new_simple_literal("D").into(),
            NamedNode::new_unchecked("http://e/ab").into(),
            Literal::new_language_tagged_literal_unchecked("C", "de").into(),
            Literal::new_language_tagged_literal_unchecked("Ca", "de").into(),
            Literal::new_language_tagged_literal_unchecked("B", "fr").into(),
            Literal::new_directional_language_tagged_literal_unchecked(
                "A",
                "en",
                BaseDirection::Ltr,
            )
            .into(),
        ];
        let mut dict = Dictionary::new();
        let mut key = Vec::new();
        let ids: Vec<TermId> = terms
            .iter()
            .map(|t| {
                let Encoded::Key(kind) = encode(t.as_ref(), &mut key) else {
                    panic!()
                };
                TermId::new(kind, u64::from(dict.get_or_insert(&key).0))
            })
            .collect();
        let ranks = StringRanks::build(&dict);
        // Language strings sort by value, then language tag.
        let value = |t: &Term| match t {
            Term::NamedNode(n) => (n.as_str().to_owned(), String::new()),
            Term::BlankNode(b) => (b.as_str().to_owned(), String::new()),
            Term::Literal(l) => (
                l.value().to_owned(),
                l.language().unwrap_or_default().to_owned(),
            ),
            Term::Triple(_) => unreachable!(),
        };
        let directional = ids[14];
        for (a, ta) in ids.iter().zip(&terms) {
            for (b, tb) in ids.iter().zip(&terms) {
                if a.kind() == b.kind()
                    && slot(a.kind()).is_some()
                    && ![*a, *b].contains(&directional)
                {
                    assert_eq!(
                        ranks.rank(*a).cmp(&ranks.rank(*b)),
                        value(ta).cmp(&value(tb)),
                        "{ta} {tb}"
                    );
                }
            }
        }
        assert_eq!(
            ranks.rank(directional),
            None,
            "a directional string has no rank"
        );
        assert_eq!(ranks.count(Kind::LangString), 4);
        assert_eq!(ranks.count(Kind::String), 6);
        let c = ranks.prefix(&dict, Kind::String, "C");
        let found: Vec<Term> = c
            .map(|r| dict.term(ranks.id(Kind::String, r).unwrap().payload() as u32))
            .collect();
        assert_eq!(
            found,
            vec![terms[4].clone(), terms[7].clone(), terms[1].clone()]
        );
        assert!(ranks.prefix(&dict, Kind::String, "Z").is_empty());
        assert_eq!(ranks.prefix(&dict, Kind::String, "é"), 5..6);
        assert_eq!(ranks.prefix(&dict, Kind::Iri, "http://e/a").len(), 2);
        // "C"@de, "C"@en, "Ca"@de: by value, then language.
        let c = ranks.prefix(&dict, Kind::LangString, "C");
        let found: Vec<TermId> = c.map(|r| ranks.id(Kind::LangString, r).unwrap()).collect();
        assert_eq!(found, vec![ids[11], ids[5], ids[12]]);
        // A term added later has no rank.
        let mut later = Vec::new();
        let Encoded::Key(kind) =
            encode(Literal::new_simple_literal("A").as_ref().into(), &mut later)
        else {
            panic!()
        };
        let id = TermId::new(kind, u64::from(dict.get_or_insert(&later).0));
        assert_eq!(ranks.rank(id), None);
    }

    /// IRIs of compact keys (#152) rank as their full text: by namespace and rest together.
    #[test]
    fn compact_iris_rank_by_their_full_text() {
        let iris = [
            "http://ex.org/a/z",
            "http://ex.org/a/b/c",
            "http://ex.org/a/y",
            "http://ex.org/a/b/d",
            "http://ex.org/a/b/",
            "http://ex.org/a/",
            "http://ex.org/a#q",
            "http://ex.org/a#p",
            "http://ex.org/a/b",
        ];
        let mut dict = Dictionary::new();
        let mut key = Vec::new();
        let ids: Vec<TermId> = iris
            .iter()
            .map(|s| {
                let Encoded::Key(kind) =
                    encode(NamedNode::new_unchecked(*s).as_ref().into(), &mut key)
                else {
                    panic!()
                };
                TermId::new(kind, u64::from(dict.get_or_insert(&key).0))
            })
            .collect();
        assert!(dict.namespaces().len() >= 2, "some keys are compact");
        let ranks = StringRanks::build(&dict);
        let mut sorted: Vec<&str> = iris.to_vec();
        sorted.sort();
        for (id, s) in ids.iter().zip(&iris) {
            let r = ranks.rank(*id).unwrap();
            assert_eq!(sorted[r as usize], *s);
        }
        for prefix in [
            "http://ex.org/a/b",
            "http://ex.org/a/b/",
            "http://ex.org/a",
            "x",
        ] {
            let found = ranks.prefix(&dict, Kind::Iri, prefix).len();
            let want = iris.iter().filter(|s| s.starts_with(prefix)).count();
            assert_eq!(found, want, "{prefix}");
        }
    }
}
