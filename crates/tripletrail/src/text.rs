//! Full-text search (#188): an inverted index over the literals of chosen properties, ranked
//! with BM25, read in SPARQL with the property function `tt:search`:
//!
//! ```sparql
//! ?s tt:search "brussel zuid" .                    # the resources whose labels match
//! ?s tt:search ("brussel zuid" ?score) .           # with the BM25 score
//! ?s tt:search ("brussel zuid" ?score ?label 10) . # and the literal matched; the best 10
//! ```
//!
//! Jena's `text:query` is read too, on the same index, in its own form (the property and the
//! limit optional; the subject `?s` or a list `(?s ?score ?literal)`):
//!
//! ```sparql
//! (?s ?score ?label) text:query (rdfs:label "brussel zuid" 10) .
//! ```
//!
//! A document is one (subject, literal) pair of an indexed property, in any graph: a resource
//! with two matching labels gives two rows. A query is split into words like the literals; a
//! literal matches if it holds any of them (a word ending in `*` matches the words it starts),
//! and literals holding more of them, and rarer ones, score higher.
//!
//! Words are runs of letters and digits, lower-cased, with the accents of Latin letters
//! taken off (`é` is `e`; `ø`, `æ` and `å` stay letters of their own).
//!
//! The index is built on the first search, from the store, and brought up to date on the
//! first search after each change to the data: the pairs added and removed are patched in. The query's pattern is replaced, before evaluation, by a
//! table of the matches on term ids, which the rest of the query joins like any pattern.

use crate::Error;
use oxigraph::model::{Literal, NamedNode, Term, TermRef};
use spargebra::algebra::GraphPattern;
use spargebra::term::{NamedNodePattern, TermPattern, TriplePattern, Variable};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use tripletrail_store::{GraphSelector, IdRows, Snapshot, TermId};

/// `tt:search`.
pub const SEARCH: &str = "http://w3id.org/tripletrail/fn#search";
/// `tt:nearest` (#236): the resources whose vectors are nearest a query vector, rewritten
/// like the searches into a table of their ids.
pub const NEAREST: &str = "http://w3id.org/tripletrail/fn#nearest";
/// Jena's `text:query`, answered from the same index.
pub const TEXT_QUERY: &str = "http://jena.apache.org/text#query";
/// Blazegraph's namespace: `bds:search`, and its options on the same literal variable.
const BDS: &str = "http://www.bigdata.com/rdf/search#";
/// Blazegraph's `bds:search`, answered from the same index.
pub const BDS_SEARCH: &str = "http://www.bigdata.com/rdf/search#search";
/// Stardog's `textMatch`, answered from the same index.
pub const TEXT_MATCH: &str = "tag:stardog:api:property:textMatch";
/// Jena ARQ's `apf:textMatch` (LARQ's `pf:textMatch`), the form Stardog's `textMatch` took.
pub const APF_TEXT_MATCH: &str = "http://jena.hpl.hp.com/ARQ/property#textMatch";
/// Virtuoso's `?o bif:contains "words"` (Virtuoso declares `bif:` as `<bif:>`).
pub const BIF_CONTAINS: &str = "bif:contains";
/// GraphDB's connectors, each searched as `?search a <…/instance#index> ; <…#query> "words" ;
/// <…#entities> ?entity . ?entity <…#score> ?score`: their namespaces, and the prefix their
/// documentation uses, for messages.
const CONNECTORS: [(&str, &str); 4] = [
    ("http://www.ontotext.com/connectors/lucene", "luc"),
    (
        "http://www.ontotext.com/connectors/elasticsearch",
        "elastic",
    ),
    ("http://www.ontotext.com/connectors/solr", "solr"),
    (
        "http://www.ontotext.com/connectors/opensearch",
        "opensearch",
    ),
];
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
/// QLever's older form: `?t ql:contains-word "words"`.
pub const QL_CONTAINS_WORD: &str =
    "http://qlever.cs.uni-freiburg.de/builtin-functions/contains-word";
const QL_CONTAINS_ENTITY: &str =
    "http://qlever.cs.uni-freiburg.de/builtin-functions/contains-entity";
/// QLever's text search service: `SERVICE textSearch: { ?t textSearch:contains [ textSearch:word
/// "word" ; textSearch:score ?score ] }`.
pub const QLEVER_TEXT_SEARCH: &str = "https://qlever.cs.uni-freiburg.de/textSearch/";
/// Stardog's search service namespace: `SERVICE fts:textMatch { [] fts:query "words" … }`.
const FTS: &str = "tag:stardog:api:search:";
/// Stardog's `SERVICE fts:textMatch`.
pub const FTS_TEXT_MATCH: &str = "tag:stardog:api:search:textMatch";
const RDF_FIRST: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#first";
const RDF_REST: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#rest";
const RDF_NIL: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#nil";

/// The properties whose literals are indexed in every dataset; a dataset can add its own
/// ([`crate::Engine::set_text_properties`]).
pub const INDEXED: [&str; 7] = [
    "http://www.w3.org/2000/01/rdf-schema#label",
    "http://www.w3.org/2000/01/rdf-schema#comment",
    "http://www.w3.org/2004/02/skos/core#prefLabel",
    "http://www.w3.org/2004/02/skos/core#altLabel",
    "http://purl.org/dc/terms/title",
    "http://purl.org/dc/terms/description",
    "http://purl.org/dc/elements/1.1/title",
];

/// Items in a call's argument list: `text:query`'s property, query, limit, `lang:` and
/// `highlight:` (`tt:search`'s query, score, literal and limit are fewer).
const MAX_ARGS: usize = 5;

/// BM25's term-frequency saturation and length normalisation (Lucene's defaults).
const K1: f64 = 1.2;
const B: f64 = 0.75;

/// One indexed (subject, literal) pair.
#[derive(Clone, Copy, Debug)]
struct Doc {
    subject: TermId,
    literal: TermId,
    /// Its literal's words.
    words: u32,
    /// Not removed by an update since.
    live: bool,
}

/// The number of a pair whose object is not a literal: kept with the pairs, so that updates
/// see it as already known, but not a document.
const NOT_TEXT: u32 = u32::MAX;

/// The inverted index of one store at one generation.
#[derive(Clone, Debug, Default)]
pub struct TextIndex {
    /// Documents by number. Removed ones stay, not live, until the index is built again.
    docs: Vec<Doc>,
    /// The (subject, object) pairs of the indexed properties, sorted, with their document
    /// numbers ([`NOT_TEXT`] for an object that is not a literal).
    pairs: Vec<((TermId, TermId), u32)>,
    /// For each word, the live documents holding it and how often, in document order.
    postings: BTreeMap<Box<str>, Vec<(u32, u32)>>,
    /// The entries of `postings`, and the bytes of its words, for [`TextIndex::heap_size`].
    posting_entries: usize,
    word_bytes: usize,
    /// Live documents and the words they hold, for BM25's averages.
    live: usize,
    total_words: u64,
    /// The store generation the index is of.
    pub generation: u64,
    /// The properties it indexes ([`Source::properties`]).
    properties: Arc<[NamedNode]>,
}

/// A search's match: the document's subject and literal, and its score.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    pub subject: TermId,
    pub literal: TermId,
    pub score: f64,
}

/// What an index covers (#203): the quads of a snapshot (which may be a view that hides rows)
/// in some graphs, or in all.
#[derive(Clone)]
pub struct Source {
    pub snapshot: Snapshot,
    /// The graphs read, sorted (`TermId(0)` the default graph); `None` for every graph.
    pub graphs: Option<Vec<TermId>>,
    /// The properties whose literals are indexed: [`INDEXED`] and the dataset's own.
    pub properties: Arc<[NamedNode]>,
}

impl Source {
    /// Every graph of the store's current data, with the [`INDEXED`] properties.
    pub fn store(store: &tripletrail_store::Store) -> Self {
        Self {
            snapshot: store.snapshot(),
            graphs: None,
            properties: default_properties(),
        }
    }

    /// Whether the quad's graph is read.
    pub fn reads(&self, graph: TermId) -> bool {
        self.graphs
            .as_ref()
            .is_none_or(|g| g.binary_search(&graph).is_ok())
    }

    /// Whether some read graph holds the triple.
    pub fn holds(&self, s: TermId, p: TermId, o: TermId) -> bool {
        self.snapshot
            .quad_ids_for_pattern(Some(s), Some(p), Some(o), GraphSelector::All)
            .any(|q| self.reads(q[3]))
    }
}

/// [`INDEXED`] as named nodes.
pub fn default_properties() -> Arc<[NamedNode]> {
    INDEXED.map(NamedNode::new_unchecked).into()
}

/// [`INDEXED`] followed by those of `extra` it lacks, each once.
pub fn properties_with(extra: &[NamedNode]) -> Arc<[NamedNode]> {
    let mut all = default_properties().to_vec();
    for p in extra {
        if !all.contains(p) {
            all.push(p.clone());
        }
    }
    all.into()
}

/// The (subject, object) pairs of the source's indexed properties in the graphs it reads,
/// sorted, each once whatever its property and graph.
fn indexed_pairs(source: &Source) -> Vec<(TermId, TermId)> {
    let snapshot = &source.snapshot;
    let mut pairs = Vec::new();
    for p in source.properties.iter() {
        let Some(pid) = snapshot.term_id(p.as_ref().into()) else {
            continue;
        };
        pairs.extend(
            snapshot
                .quad_ids_for_pattern(None, Some(pid), None, GraphSelector::All)
                .filter(|q| source.reads(q[3]))
                .map(|[s, _, o, _]| (s, o)),
        );
    }
    pairs.sort_unstable();
    pairs.dedup();
    pairs
}

/// A literal's words with their counts, sorted.
type WordCounts = Vec<(Box<str>, u32)>;

/// The words of `literal` with their counts, sorted; `None` if it is not a literal.
fn word_counts(snapshot: &Snapshot, literal: TermId) -> Option<WordCounts> {
    let Ok(Term::Literal(l)) = snapshot.term(literal) else {
        return None;
    };
    let mut counts: HashMap<Box<str>, u32> = HashMap::new();
    let stem = l.language().and_then(Stemmer::of);
    for w in words(l.value()) {
        if let Some(stem) = &stem {
            *counts.entry(stem.key(&w).into_boxed_str()).or_default() += 1;
        }
        *counts.entry(w.into_boxed_str()).or_default() += 1;
    }
    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.sort_unstable();
    Some(counts)
}

impl TextIndex {
    /// Indexes the literals of [`INDEXED`] properties in every graph of `store`.
    pub fn build(store: &tripletrail_store::Store) -> Result<Self, Error> {
        Self::build_from(&Source::store(store), &|| Ok(()))
    }

    /// Indexes the literals of the source's properties that it shows. `stop` is asked
    /// now and then, and its error ends the build (a deadline, #203).
    pub fn build_from(
        source: &Source,
        stop: &dyn Fn() -> Result<(), Error>,
    ) -> Result<Self, Error> {
        let snapshot = &source.snapshot;
        let mut index = Self {
            generation: snapshot.generation().unwrap_or_default(),
            properties: Arc::clone(&source.properties),
            ..Self::default()
        };
        let pairs = indexed_pairs(source);
        index.pairs.reserve_exact(pairs.len());
        let mut words_of: HashMap<TermId, Option<WordCounts>> = HashMap::new();
        let mut postings: HashMap<Box<str>, Vec<(u32, u32)>> = HashMap::new();
        for (k, (subject, literal)) in pairs.into_iter().enumerate() {
            if k % 4096 == 0 {
                stop()?;
            }
            let counts = words_of
                .entry(literal)
                .or_insert_with(|| word_counts(snapshot, literal));
            let Some(counts) = counts else {
                index.pairs.push(((subject, literal), NOT_TEXT));
                continue;
            };
            let n = index.next_number()?;
            for (w, c) in counts.iter() {
                postings.entry(w.clone()).or_default().push((n, *c));
            }
            index.push(subject, literal, counts);
            index.pairs.push(((subject, literal), n));
        }
        index.posting_entries = postings.values().map(Vec::len).sum();
        index.word_bytes = postings.keys().map(|w| w.len()).sum();
        index.postings = postings.into_iter().collect();
        Ok(index)
    }

    /// Brings the index to the store's current data. The pairs added and removed since are
    /// found by comparing the indexed properties' pairs with the index's, and only their
    /// literals are read and split into words; if they are many, the index is built again.
    pub fn update(&mut self, store: &tripletrail_store::Store) -> Result<(), Error> {
        self.update_from(&Source::store(store), &|| Ok(()))
    }

    /// Brings an index made by [`TextIndex::build_from`] to what `source` shows now, as
    /// [`TextIndex::update`] does: `source` reads the same graphs, through rows hidden the
    /// same way, at a later store generation.
    pub fn update_from(
        &mut self,
        source: &Source,
        stop: &dyn Fn() -> Result<(), Error>,
    ) -> Result<(), Error> {
        let snapshot = &source.snapshot;
        let generation = snapshot.generation().unwrap_or_default();
        if self.properties != source.properties {
            *self = Self::build_from(source, stop)?;
            return Ok(());
        }
        if generation == self.generation {
            return Ok(());
        }
        let now = indexed_pairs(source);
        let mut kept = Vec::with_capacity(now.len());
        let (mut added, mut removed) = (Vec::new(), Vec::new());
        let mut old = std::mem::take(&mut self.pairs).into_iter().peekable();
        for pair in now {
            while let Some(o) = old.next_if(|o| o.0 < pair) {
                removed.push(o);
            }
            match old.next_if(|o| o.0 == pair) {
                Some(o) => kept.push(o),
                None => added.push(pair),
            }
        }
        removed.extend(old);
        // Past a quarter of the documents changed, or as many removed ones as live ones kept,
        // building it again is about as fast and gives a compact index.
        let changed = added.len() + removed.len();
        let dead = self.docs.len() - self.live + removed.len();
        if changed * 4 > self.live || dead > self.live.max(64) {
            *self = Self::build_from(source, stop)?;
            return Ok(());
        }
        for (_, n) in removed {
            if n == NOT_TEXT {
                continue;
            }
            let doc = &mut self.docs[n as usize];
            doc.live = false;
            self.live -= 1;
            self.total_words -= u64::from(doc.words);
            // The dictionary keeps the terms of removed quads.
            let literal = doc.literal;
            for (w, _) in word_counts(snapshot, literal).unwrap_or_default() {
                if let Some(list) = self.postings.get_mut(&w) {
                    if let Ok(k) = list.binary_search_by_key(&n, |e| e.0) {
                        list.remove(k);
                        self.posting_entries -= 1;
                    }
                    if list.is_empty() {
                        self.postings.remove(&w);
                        self.word_bytes -= w.len();
                    }
                }
            }
        }
        for (subject, literal) in added {
            let Some(counts) = word_counts(snapshot, literal) else {
                kept.push(((subject, literal), NOT_TEXT));
                continue;
            };
            let n = self.next_number()?;
            for (w, c) in &counts {
                self.posting_entries += 1;
                match self.postings.get_mut(w) {
                    Some(list) => list.push((n, *c)),
                    None => {
                        self.word_bytes += w.len();
                        self.postings.insert(w.clone(), vec![(n, *c)]);
                    }
                }
            }
            self.push(subject, literal, &counts);
            kept.push(((subject, literal), n));
        }
        kept.sort_unstable();
        self.pairs = kept;
        self.generation = generation;
        Ok(())
    }

    fn next_number(&self) -> Result<u32, Error> {
        u32::try_from(self.docs.len())
            .ok()
            .filter(|&n| n != NOT_TEXT)
            .ok_or_else(|| Error::Request("full-text index: more than 4 billion documents".into()))
    }

    fn push(&mut self, subject: TermId, literal: TermId, counts: &[(Box<str>, u32)]) {
        // Stems are not words of their own.
        let words: u32 = counts
            .iter()
            .filter(|(w, _)| !w.starts_with(STEM))
            .map(|(_, c)| c)
            .sum();
        self.total_words += u64::from(words);
        self.live += 1;
        self.docs.push(Doc {
            subject,
            literal,
            words,
            live: true,
        });
    }

    /// The properties whose literals it indexes.
    pub fn properties(&self) -> &Arc<[NamedNode]> {
        &self.properties
    }

    /// Roughly the heap bytes of the index: documents, pairs and postings (#225).
    pub fn heap_size(&self) -> usize {
        // A word's map entry: its key and list, and about as much again of tree node.
        let word = 2 * (size_of::<Box<str>>() + size_of::<Vec<(u32, u32)>>());
        self.docs.capacity() * size_of::<Doc>()
            + self.pairs.capacity() * size_of::<((TermId, TermId), u32)>()
            + self.postings.len() * word
            + self.word_bytes
            + self.posting_entries * size_of::<(u32, u32)>()
    }

    /// Documents indexed.
    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Documents removed by updates since the index was last built, whose slots it keeps.
    pub fn removed(&self) -> usize {
        self.docs.len() - self.live
    }

    /// The documents matching `query`, best first (ties by subject, then literal), at most
    /// `limit`. `keep` says which (subject, literal) pairs count: a bound subject or a property
    /// is filtered before the limit. The query is `tt:search`'s: any word may match.
    pub fn search(
        &self,
        query: &str,
        limit: Option<usize>,
        keep: impl Fn(TermId, TermId) -> bool,
    ) -> Vec<Hit> {
        self.search_query(&TextQuery::plain(query), limit, keep, &|_| None)
    }

    /// As [`TextIndex::search`], for a query with required and excluded words and phrases
    /// (#830). `text` gives a literal's text, to check a phrase's words follow each other.
    pub fn search_query(
        &self,
        query: &TextQuery,
        limit: Option<usize>,
        keep: impl Fn(TermId, TermId) -> bool,
        text: &dyn Fn(TermId) -> Option<String>,
    ) -> Vec<Hit> {
        let mut must: Option<HashMap<u32, f64>> = None;
        let mut should: HashMap<u32, f64> = HashMap::new();
        let mut not: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for clause in &query.clauses {
            let docs = self.clause_docs(clause, text);
            match clause.occur {
                Occur::MustNot => not.extend(docs.into_keys()),
                Occur::Should => {
                    for (d, score) in docs {
                        *should.entry(d).or_default() += score;
                    }
                }
                Occur::Must => {
                    must = Some(match must {
                        None => docs,
                        Some(mut all) => {
                            all.retain(|d, _| docs.contains_key(d));
                            for (d, score) in all.iter_mut() {
                                *score += docs[d];
                            }
                            all
                        }
                    });
                }
            }
        }
        // With required clauses, the others only add to the score; else any may match.
        let scores = match must {
            Some(mut all) => {
                for (d, score) in all.iter_mut() {
                    *score += should.get(d).copied().unwrap_or_default();
                }
                all
            }
            None => should,
        };
        let mut hits: Vec<(Doc, f64)> = scores
            .into_iter()
            .filter(|(d, _)| !not.contains(d))
            .map(|(d, score)| (self.docs[d as usize], score))
            .filter(|(doc, _)| keep(doc.subject, doc.literal))
            .collect();
        hits.sort_unstable_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then((a.0.subject, a.0.literal).cmp(&(b.0.subject, b.0.literal)))
        });
        if let Some(limit) = limit {
            hits.truncate(limit);
        }
        hits.into_iter()
            .map(|(doc, score)| Hit {
                subject: doc.subject,
                literal: doc.literal,
                score,
            })
            .collect()
    }

    /// The live documents a clause matches, with their BM25 scores: those holding its word (or
    /// a word it starts, for a prefix), or for a phrase all its words, one after the other.
    fn clause_docs(
        &self,
        clause: &Clause,
        text: &dyn Fn(TermId) -> Option<String>,
    ) -> HashMap<u32, f64> {
        let mut scores = HashMap::new();
        if let [word] = clause.words.as_slice() {
            self.word_scores(word, clause.prefix, &mut scores);
            return scores;
        }
        let mut each = clause.words.iter().map(|w| {
            let mut s = HashMap::new();
            self.word_scores(w, false, &mut s);
            s
        });
        let Some(mut all) = each.next() else {
            return scores;
        };
        for next in each {
            all.retain(|d, _| next.contains_key(d));
            for (d, score) in all.iter_mut() {
                *score += next[d];
            }
        }
        all.retain(|d, _| {
            text(self.docs[*d as usize].literal).is_some_and(|t| {
                let found: Vec<String> = match &clause.stem {
                    Some(stem) => words(&t).map(|w| stem.key(&w)).collect(),
                    None => words(&t).collect(),
                };
                found
                    .windows(clause.words.len())
                    .any(|w| w == clause.words.as_slice())
            })
        });
        all
    }

    /// Adds to `scores` the BM25 score of `word` (or of each word it starts) in each document.
    fn word_scores(&self, word: &str, prefix: bool, scores: &mut HashMap<u32, f64>) {
        let n = self.live as f64;
        let average_words = if self.live == 0 {
            0.0
        } else {
            self.total_words as f64 / n
        };
        let lists: Vec<&Vec<(u32, u32)>> = if prefix {
            self.postings
                .range::<str, _>((std::ops::Bound::Included(word), std::ops::Bound::Unbounded))
                .take_while(|(w, _)| w.starts_with(word))
                .map(|(_, l)| l)
                .collect()
        } else {
            self.postings.get(word).into_iter().collect()
        };
        for list in lists {
            let df = list.len() as f64;
            let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
            for &(doc, tf) in list {
                let tf = f64::from(tf);
                let length = f64::from(self.docs[doc as usize].words);
                let norm = K1 * (1.0 - B + B * length / average_words.max(1.0));
                *scores.entry(doc).or_default() += idf * tf * (K1 + 1.0) / (tf + norm);
            }
        }
    }
}

/// How a clause of a query counts (Lucene's occurs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Occur {
    /// It may match, and adds to the score.
    Should,
    /// It must match (`+word`, `AND`).
    Must,
    /// It must not match (`-word`, `NOT`).
    MustNot,
}

/// A word (with `prefix`, the words it starts) or a phrase of words that follow each other.
#[derive(Clone, Debug, PartialEq)]
pub struct Clause {
    pub occur: Occur,
    pub words: Vec<String>,
    pub prefix: bool,
    /// The language its words are stems in (as [`Stemmer::key`]s), with `lang:` (#830).
    pub stem: Option<Stemmer>,
}

/// A parsed full-text query.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextQuery {
    pub clauses: Vec<Clause>,
}

impl TextQuery {
    /// `tt:search`'s query: any of its words may match; a word ending in `*` matches the
    /// words it starts.
    pub fn plain(query: &str) -> Self {
        let mut clauses = Vec::new();
        for token in query.split_whitespace() {
            let (stem, prefix) = match token.strip_suffix('*') {
                Some(stem) => (stem, true),
                None => (token, false),
            };
            for word in words(stem) {
                clauses.push(Clause {
                    occur: Occur::Should,
                    words: vec![word],
                    prefix,
                    stem: None,
                });
            }
        }
        Self { clauses }
    }

    /// The words that make a literal match, for highlighting: (word, prefix).
    /// In `language`'s stems (#830): its words, other than those a word starts, as the
    /// language's stems, matched against the stems of tagged literals in the index.
    pub fn stemmed(mut self, stem: Stemmer) -> Self {
        for c in &mut self.clauses {
            if !c.prefix {
                c.words = c.words.iter().map(|w| stem.key(w)).collect();
                c.stem = Some(stem);
            }
        }
        self
    }

    fn matching_words(&self) -> Vec<(&str, bool)> {
        self.clauses
            .iter()
            .filter(|c| c.occur != Occur::MustNot)
            .flat_map(|c| c.words.iter().map(move |w| (w.as_str(), c.prefix)))
            .collect()
    }
}

/// The words of `text`: runs of letters and digits, lower-cased, Latin accents taken off.
pub fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut out = String::with_capacity(w.len());
            for c in w.chars().flat_map(char::to_lowercase) {
                out.push(fold(c));
            }
            out
        })
}

/// What starts the index entry of a stem, so that no word is one.
const STEM: char = '\u{1}';

/// Lucene's language analyzers, as Jena's multilingual index has them (#830): the Snowball
/// stemmer of a language tag's primary subtag. A literal tagged with one is indexed with its
/// words' stems too, and a `lang:` search in that language matches them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stemmer {
    code: &'static str,
    algorithm: rust_stemmers::Algorithm,
}

impl Stemmer {
    /// The stemmer of a language tag (`en`, `nl-BE`, `nb`), if Snowball has one.
    pub fn of(tag: &str) -> Option<Self> {
        use rust_stemmers::Algorithm as A;
        let primary = tag.split('-').next()?.to_ascii_lowercase();
        let (code, algorithm) = match primary.as_str() {
            "ar" => ("ar", A::Arabic),
            "da" => ("da", A::Danish),
            "de" => ("de", A::German),
            "el" => ("el", A::Greek),
            "en" => ("en", A::English),
            "es" => ("es", A::Spanish),
            "fi" => ("fi", A::Finnish),
            "fr" => ("fr", A::French),
            "hu" => ("hu", A::Hungarian),
            "it" => ("it", A::Italian),
            "nl" => ("nl", A::Dutch),
            "no" | "nb" | "nn" => ("no", A::Norwegian),
            "pt" => ("pt", A::Portuguese),
            "ro" => ("ro", A::Romanian),
            "ru" => ("ru", A::Russian),
            "sv" => ("sv", A::Swedish),
            "ta" => ("ta", A::Tamil),
            "tr" => ("tr", A::Turkish),
            _ => return None,
        };
        Some(Self { code, algorithm })
    }

    /// The stemmer whose [`Stemmer::key`] `key` is.
    fn of_key(key: &str) -> Option<Self> {
        let rest = key.strip_prefix(STEM)?;
        Self::of(rest.split(STEM).next()?)
    }

    /// The index entry of `word`'s stem: the language and the stem, apart from any word.
    pub fn key(&self, word: &str) -> String {
        let stem = rust_stemmers::Stemmer::create(self.algorithm)
            .stem(word)
            .into_owned();
        format!("{STEM}{}{STEM}{stem}", self.code)
    }
}

/// A Latin letter without its accent (`é` → `e`); other characters as they are.
pub fn fold(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'ā' | 'ă' | 'ą' => 'a',
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => 'c',
        'ď' | 'đ' => 'd',
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => 'g',
        'ĥ' | 'ħ' => 'h',
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => 'i',
        'ĵ' => 'j',
        'ķ' => 'k',
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => 'l',
        'ñ' | 'ń' | 'ņ' | 'ň' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ō' | 'ŏ' | 'ő' => 'o',
        'ŕ' | 'ŗ' | 'ř' => 'r',
        'ś' | 'ŝ' | 'ş' | 'š' => 's',
        'ţ' | 'ť' | 'ŧ' => 't',
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => 'u',
        'ŵ' => 'w',
        'ý' | 'ÿ' | 'ŷ' => 'y',
        'ź' | 'ż' | 'ž' => 'z',
        c => c,
    }
}

/// One `tt:search` call: where its rows go and what it searches for.
#[derive(Clone, Debug)]
struct Call {
    /// `tt:search` or `text:query`, for messages.
    function: &'static str,
    subject: TermPattern,
    /// `text:query`'s property: only its literals match.
    property: Option<oxigraph::model::NamedNode>,
    query: TextQuery,
    /// The variable whose values are the query strings, for a query string bound by the rest
    /// of the query (#188): searched once per value.
    query_var: Option<Variable>,
    score: Option<Variable>,
    literal: Option<Variable>,
    limit: Option<usize>,
    /// `text:query`'s `lang:xx`: only literals in that language (lower case).
    lang: Option<String>,
    /// `text:query`'s `highlight:`: the literal given with its matched words marked.
    highlight: Option<Highlight>,
    /// One row per literal matched, whatever resources have it (`bds:search`, `textMatch`):
    /// its best score, and the limit counts literals.
    per_literal: bool,
    /// `bds:rank`: the place of the literal among the matches, from 1.
    rank: Option<Variable>,
    /// `bds:minRank`: the matches before that place are left out.
    skip: usize,
    /// `bds:matchAllTerms`: every word must match.
    all_terms: bool,
    /// `bds:prefixMatch`: every word matches the words it starts.
    prefix_terms: bool,
    /// `tt:nearest`'s property and query vector (empty while it is read from `query_var`).
    nearest: Option<(oxigraph::model::NamedNode, Vec<f64>)>,
}

impl Call {
    /// A call of `function` with nothing asked for yet.
    fn new(function: &'static str, subject: TermPattern) -> Self {
        Self {
            function,
            subject,
            property: None,
            query: TextQuery::default(),
            query_var: None,
            score: None,
            literal: None,
            limit: None,
            lang: None,
            highlight: None,
            per_literal: false,
            rank: None,
            skip: 0,
            all_terms: false,
            prefix_terms: false,
            nearest: None,
        }
    }

    /// A query string, in the syntax of the call's engine (Lucene's for `text:query` and the
    /// `textMatch`es, Virtuoso's for `bif:contains`), with its options.
    fn parse(&self, query: &str) -> Result<TextQuery, String> {
        let mut parsed = match self.function {
            "text:query" | "textMatch" | "apf:textMatch" | "fts:textMatch" | "luc:query"
            | "elastic:query" | "solr:query" | "opensearch:query" => lucene(query)?,
            "bif:contains" => virtuoso(query)?,
            "ql:contains-word" | "textSearch:word" => qlever_words(query)?,
            _ => TextQuery::plain(query),
        };
        for clause in &mut parsed.clauses {
            if self.all_terms && clause.occur == Occur::Should {
                clause.occur = Occur::Must;
            }
            clause.prefix |= self.prefix_terms;
        }
        Ok(parsed)
    }
}

/// Jena's highlighting: the markers put around matched words, and the fragments of a long
/// literal shown.
#[derive(Clone, Debug, PartialEq)]
pub struct Highlight {
    pub start: String,
    pub end: String,
    /// Most fragments shown of a literal longer than `size` characters.
    pub fragments: usize,
    pub size: usize,
}

impl Default for Highlight {
    fn default() -> Self {
        Self {
            start: "↦".to_owned(),
            end: "↤".to_owned(),
            fragments: 3,
            size: 128,
        }
    }
}

impl Highlight {
    /// `highlight:` options as Jena writes them: `s:start | e:end | z:fragments | f:size`.
    fn parse(options: &str) -> Result<Self, String> {
        let mut h = Self::default();
        for option in options.split('|').map(str::trim).filter(|o| !o.is_empty()) {
            let (key, value) = option
                .split_once(':')
                .ok_or_else(|| format!("highlight option \"{option}\" is not key:value"))?;
            let number = || {
                value
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| format!("highlight option {key}: needs a number"))
            };
            match key.trim() {
                "s" => h.start = value.to_owned(),
                "e" => h.end = value.to_owned(),
                "z" => h.fragments = number()?,
                "f" => h.size = number()?,
                other => {
                    return Err(format!(
                        "highlight option {other}: is not supported; s:, e:, z: and f: are"
                    ));
                }
            }
        }
        Ok(h)
    }

    /// `text` with the words that `query` matches between the markers; a text longer than
    /// `size` characters as at most `fragments` pieces around its matches, joined with " ...".
    pub fn apply(&self, text: &str, query: &[(&str, bool)]) -> String {
        // The matched words' byte ranges.
        let mut found = Vec::new();
        let mut at = None;
        for (i, c) in text
            .char_indices()
            .chain(std::iter::once((text.len(), ' ')))
        {
            match (c.is_alphanumeric(), at) {
                (true, None) => at = Some(i),
                (false, Some(from)) => {
                    let word: String = words(&text[from..i]).collect();
                    if query.iter().any(|(w, prefix)| {
                        if *prefix {
                            word.starts_with(w)
                        } else if let Some(stem) = Stemmer::of_key(w) {
                            stem.key(&word) == *w
                        } else {
                            word == *w
                        }
                    }) {
                        found.push((from, i));
                    }
                    at = None;
                }
                _ => {}
            }
        }
        let mark = |from: usize, to: usize| {
            let mut out = String::new();
            let mut last = from;
            for &(a, b) in found.iter().filter(|(a, b)| *a >= from && *b <= to) {
                out.push_str(&text[last..a]);
                out.push_str(&self.start);
                out.push_str(&text[a..b]);
                out.push_str(&self.end);
                last = b;
            }
            out.push_str(&text[last..to]);
            out
        };
        if text.chars().count() <= self.size || found.is_empty() {
            return mark(0, text.len());
        }
        // Pieces of `size` characters starting a little before a match, not overlapping.
        let boundary = |mut i: usize| {
            while !text.is_char_boundary(i) {
                i -= 1;
            }
            i
        };
        let mut pieces: Vec<(usize, usize)> = Vec::new();
        for &(a, _) in &found {
            if pieces.len() == self.fragments {
                break;
            }
            if pieces.last().is_some_and(|&(_, end)| a < end) {
                continue;
            }
            let before = text[..a]
                .chars()
                .rev()
                .take(self.size / 4)
                .map(char::len_utf8)
                .sum::<usize>();
            let from = a - before;
            let to = boundary((from + self.size * 4).min(text.len()));
            let to = from
                + text[from..to]
                    .chars()
                    .take(self.size)
                    .map(char::len_utf8)
                    .sum::<usize>();
            pieces.push((from, to.max(boundary(a))));
        }
        pieces
            .into_iter()
            .map(|(a, b)| mark(a, b))
            .collect::<Vec<_>>()
            .join(" ...")
    }
}

/// Whether `query` calls `tt:search` anywhere.
pub fn query_calls_search(query: &spargebra::Query) -> bool {
    use spargebra::Query as Q;
    match query {
        Q::Select { pattern, .. }
        | Q::Construct { pattern, .. }
        | Q::Describe { pattern, .. }
        | Q::Ask { pattern, .. } => calls_search(pattern),
    }
}

/// Whether `pattern` calls `tt:search` anywhere, also in `EXISTS` expressions.
pub fn calls_search(pattern: &GraphPattern) -> bool {
    use GraphPattern as G;
    match pattern {
        G::Bgp { patterns } => patterns.iter().any(|t| is_search(t) || is_option(t)),
        G::Path { .. } | G::Values { .. } => false,
        G::Join { left, right }
        | G::Lateral { left, right }
        | G::Union { left, right }
        | G::Minus { left, right } => calls_search(left) || calls_search(right),
        G::LeftJoin {
            left,
            right,
            expression,
        } => {
            calls_search(left)
                || calls_search(right)
                || expression.as_ref().is_some_and(expression_calls_search)
        }
        G::Filter { expr, inner } => expression_calls_search(expr) || calls_search(inner),
        G::Extend {
            inner, expression, ..
        } => expression_calls_search(expression) || calls_search(inner),
        G::OrderBy { inner, expression } => {
            calls_search(inner)
                || expression.iter().any(|o| match o {
                    spargebra::algebra::OrderExpression::Asc(e)
                    | spargebra::algebra::OrderExpression::Desc(e) => expression_calls_search(e),
                })
        }
        G::Group {
            inner, aggregates, ..
        } => {
            calls_search(inner)
                || aggregates.iter().any(|(_, a)| match a {
                    spargebra::algebra::AggregateExpression::FunctionCall { expr, .. } => {
                        expression_calls_search(expr)
                    }
                    spargebra::algebra::AggregateExpression::CountSolutions { .. } => false,
                })
        }
        G::Graph { inner, .. }
        | G::Project { inner, .. }
        | G::Distinct { inner }
        | G::Reduced { inner }
        | G::Slice { inner, .. } => calls_search(inner),
        G::Service { name, inner, .. } => is_fts(name) || is_qlever(name) || calls_search(inner),
    }
}

fn expression_calls_search(e: &spargebra::algebra::Expression) -> bool {
    use spargebra::algebra::Expression as E;
    match e {
        E::Exists(p) => calls_search(p),
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
        | E::Divide(a, b) => expression_calls_search(a) || expression_calls_search(b),
        E::In(a, list) => expression_calls_search(a) || list.iter().any(expression_calls_search),
        E::UnaryPlus(a) | E::UnaryMinus(a) | E::Not(a) => expression_calls_search(a),
        E::If(a, b, c) => {
            expression_calls_search(a) || expression_calls_search(b) || expression_calls_search(c)
        }
        E::Coalesce(list) | E::FunctionCall(_, list) => list.iter().any(expression_calls_search),
        E::NamedNode(_) | E::Literal(_) | E::Variable(_) | E::Bound(_) => false,
    }
}

fn is_search(t: &TriplePattern) -> bool {
    matches!(&t.predicate, NamedNodePattern::NamedNode(n)
        if [SEARCH, TEXT_QUERY, BDS_SEARCH, TEXT_MATCH, APF_TEXT_MATCH, BIF_CONTAINS,
            QL_CONTAINS_WORD, QL_CONTAINS_ENTITY, NEAREST]
            .contains(&n.as_str()))
        || connector_option(t).is_some_and(|(_, name)| name == "query")
}

/// The GraphDB connector of `t` and the option it gives (`query`, `entities`, `score`, …;
/// `type` for `?search a <…/instance#index>`), if it is one.
fn connector_option(t: &TriplePattern) -> Option<(&'static str, &str)> {
    let NamedNodePattern::NamedNode(p) = &t.predicate else {
        return None;
    };
    CONNECTORS.iter().find_map(|&(ns, prefix)| {
        if p.as_str() == RDF_TYPE {
            match &t.object {
                TermPattern::NamedNode(o)
                    if o.as_str().strip_prefix(ns)?.starts_with("/instance#") =>
                {
                    Some((prefix, "type"))
                }
                _ => None,
            }
        } else {
            Some((prefix, p.as_str().strip_prefix(ns)?.strip_prefix('#')?))
        }
    })
}

/// An option of another engine's search on its own triple: Blazegraph's or a GraphDB
/// connector's.
fn is_option(t: &TriplePattern) -> bool {
    is_bds_option(t) || connector_option(t).is_some()
}

/// A Blazegraph option of a `bds:search`: `?o bds:relevance ?score` and the like.
fn is_bds_option(t: &TriplePattern) -> bool {
    matches!(&t.predicate, NamedNodePattern::NamedNode(n)
        if n.as_str().starts_with(BDS) && n.as_str() != BDS_SEARCH)
}

/// What the rewrite needs: the index, how to turn terms into ids and back, and how to hand
/// a table of ids to the evaluation.
pub(crate) trait SearchContext {
    fn index(&self) -> Result<std::sync::Arc<TextIndex>, Error>;
    /// The vectors of `property` (#236), for `tt:nearest`.
    fn vectors(
        &self,
        property: &oxigraph::model::NamedNode,
    ) -> Result<std::sync::Arc<crate::vector::VectorIndex>, Error>;
    /// The properties the index covers, for `text:query`'s property argument.
    fn properties(&self) -> Arc<[NamedNode]>;
    fn term_id(&self, term: TermRef<'_>) -> Option<TermId>;
    /// Whether the store holds the triple in some graph.
    fn holds(&self, s: TermId, p: TermId, o: TermId) -> bool;
    fn table(&self, variables: &[Variable], rows: IdRows) -> Option<GraphPattern>;
    /// The literal with this id, for phrases, `lang:` and highlighting.
    fn literal(&self, id: TermId) -> Option<Literal>;
    /// The distinct values `var` takes in `pattern`'s solutions (an unbound one left out), in
    /// what the caller sees: on `dataset`, the query's own, unless the caller's replaces it.
    fn values(
        &self,
        pattern: &GraphPattern,
        var: &Variable,
        dataset: Option<&spargebra::algebra::QueryDataset>,
    ) -> Result<Vec<oxigraph::model::Term>, Error>;
}

/// `query` with its `tt:search` calls replaced by tables of their matches.
pub(crate) fn rewrite(
    query: spargebra::Query,
    ctx: &impl SearchContext,
) -> Result<spargebra::Query, Error> {
    let rewritten = rewrite_query(query, ctx)?;
    if query_calls_search(&rewritten) {
        return Err(Error::Request(
            "tt:search, text:query and tt:nearest are not supported inside GRAPH, SERVICE or FILTER (NOT) \
             EXISTS: they search every graph, as a pattern of the query's own groups"
                .into(),
        ));
    }
    Ok(rewritten)
}

/// The WHERE of an update operation with its `tt:search` calls replaced, as [`rewrite`]
/// does a query's; `dataset` is its `USING`.
pub(crate) fn rewrite_where(
    pattern: GraphPattern,
    dataset: Option<&spargebra::algebra::QueryDataset>,
    ctx: &impl SearchContext,
) -> Result<GraphPattern, Error> {
    let rewritten = Rewrite::new(ctx, dataset).pattern(pattern, &[])?;
    if calls_search(&rewritten) {
        return Err(Error::Request(
            "tt:search, text:query and tt:nearest are not supported inside GRAPH, SERVICE or FILTER (NOT) \
             EXISTS: they search every graph, as a pattern of the query's own groups"
                .into(),
        ));
    }
    Ok(rewritten)
}

fn rewrite_query(
    query: spargebra::Query,
    ctx: &impl SearchContext,
) -> Result<spargebra::Query, Error> {
    use spargebra::Query as Q;
    Ok(match query {
        Q::Select {
            dataset,
            pattern,
            base_iri,
        } => Q::Select {
            pattern: Rewrite::new(ctx, dataset.as_ref()).pattern(pattern, &[])?,
            dataset,
            base_iri,
        },
        Q::Construct {
            template,
            dataset,
            pattern,
            base_iri,
        } => Q::Construct {
            template,
            pattern: Rewrite::new(ctx, dataset.as_ref()).pattern(pattern, &[])?,
            dataset,
            base_iri,
        },
        Q::Describe {
            dataset,
            pattern,
            base_iri,
        } => Q::Describe {
            pattern: Rewrite::new(ctx, dataset.as_ref()).pattern(pattern, &[])?,
            dataset,
            base_iri,
        },
        Q::Ask {
            dataset,
            pattern,
            base_iri,
        } => Q::Ask {
            pattern: Rewrite::new(ctx, dataset.as_ref()).pattern(pattern, &[])?,
            dataset,
            base_iri,
        },
    })
}

/// The most different query strings a call with a query string from a variable searches.
const MAX_QUERY_STRINGS: usize = 1000;

/// The rewrite of one query's pattern, with what a call whose query string is a variable
/// needs: the patterns joined with the one being rewritten (its context, without their own
/// calls), which bind the variable, and the query's own dataset to read them in.
struct Rewrite<'c, C> {
    ctx: &'c C,
    dataset: Option<&'c spargebra::algebra::QueryDataset>,
}

impl<'c, C: SearchContext> Rewrite<'c, C> {
    fn new(ctx: &'c C, dataset: Option<&'c spargebra::algebra::QueryDataset>) -> Self {
        Self { ctx, dataset }
    }

    /// `outer` is what `p` is joined with (empty unless `p` has a call with a variable query
    /// string, see [`lateral`]).
    fn pattern(&self, p: GraphPattern, outer: &[GraphPattern]) -> Result<GraphPattern, Error> {
        use GraphPattern as G;
        let with = |more: &GraphPattern, side: &GraphPattern| -> Vec<GraphPattern> {
            if !lateral(side) {
                return Vec::new();
            }
            let mut o = outer.to_vec();
            o.push(self.strip(more.clone()));
            o
        };
        let r = |b: Box<GraphPattern>, o: &[GraphPattern]| self.pattern(*b, o).map(Box::new);
        Ok(match p {
            G::Bgp { patterns } if patterns.iter().any(|t| is_search(t) || is_option(t)) => {
                self.bgp(patterns, outer)?
            }
            // Stardog's search service: its matches, as a call's.
            G::Service { name, inner, .. } if is_fts(&name) => {
                let call = fts_call(&inner)?;
                let index = self.ctx.index()?;
                match &call.query_var {
                    None => table(&call, &index, self.ctx)?,
                    Some(var) => self.lateral_table(&call, var, &index, outer, &[])?,
                }
            }
            // QLever's: a call per text variable, joined.
            G::Service { name, inner, .. } if is_qlever(&name) => {
                let index = self.ctx.index()?;
                let mut out = GraphPattern::Bgp {
                    patterns: Vec::new(),
                };
                for call in qlever_calls(&inner)? {
                    out = GraphPattern::Join {
                        left: Box::new(out),
                        right: Box::new(table(&call, &index, self.ctx)?),
                    };
                }
                out
            }
            // Each side is joined with the other.
            G::Join { left, right } => {
                let lo = with(&right, &left);
                let ro = with(&left, &right);
                G::Join {
                    left: r(left, &lo)?,
                    right: r(right, &ro)?,
                }
            }
            // The right side sees the left's rows, as Jena evaluates OPTIONAL and LATERAL.
            G::Lateral { left, right } => {
                let ro = with(&left, &right);
                G::Lateral {
                    left: r(left, outer)?,
                    right: r(right, &ro)?,
                }
            }
            G::LeftJoin {
                left,
                right,
                expression,
            } => {
                let ro = with(&left, &right);
                G::LeftJoin {
                    left: r(left, outer)?,
                    right: r(right, &ro)?,
                    expression,
                }
            }
            G::Union { left, right } => G::Union {
                left: r(left, outer)?,
                right: r(right, outer)?,
            },
            G::Minus { left, right } => G::Minus {
                left: r(left, outer)?,
                right: r(right, outer)?,
            },
            G::Filter { expr, inner } => G::Filter {
                expr,
                inner: r(inner, outer)?,
            },
            G::Extend {
                inner,
                variable,
                expression,
            } => G::Extend {
                inner: r(inner, outer)?,
                variable,
                expression,
            },
            // Left as they are, so the query is refused (see `rewrite`).
            G::Graph { name, inner } => G::Graph { name, inner },
            G::OrderBy { inner, expression } => G::OrderBy {
                inner: r(inner, outer)?,
                expression,
            },
            // A sub-select's variables are its own: nothing outside binds them.
            G::Project { inner, variables } => G::Project {
                inner: r(inner, &[])?,
                variables,
            },
            G::Distinct { inner } => G::Distinct {
                inner: r(inner, outer)?,
            },
            G::Reduced { inner } => G::Reduced {
                inner: r(inner, outer)?,
            },
            G::Slice {
                inner,
                start,
                length,
            } => G::Slice {
                inner: r(inner, outer)?,
                start,
                length,
            },
            G::Group {
                inner,
                variables,
                aggregates,
            } => G::Group {
                inner: r(inner, &[])?,
                variables,
                aggregates,
            },
            other => other,
        })
    }

    /// `p` without its calls (and their argument lists), to read the values of a query
    /// variable from: what binds it, at least as many rows as the query has.
    fn strip(&self, p: GraphPattern) -> GraphPattern {
        use GraphPattern as G;
        let properties = self.ctx.properties();
        let s = |b: Box<GraphPattern>| Box::new(self.strip(*b));
        match p {
            G::Service { name, .. } if is_fts(&name) || is_qlever(&name) => G::Bgp {
                patterns: Vec::new(),
            },
            G::Bgp { patterns } if patterns.iter().any(is_search) => {
                let kept: Vec<TriplePattern> =
                    patterns.iter().filter(|t| !is_search(t)).cloned().collect();
                G::Bgp {
                    patterns: calls(patterns, &properties).map_or(kept, |(_, rest)| rest),
                }
            }
            G::Join { left, right } => G::Join {
                left: s(left),
                right: s(right),
            },
            G::Lateral { left, right } => G::Lateral {
                left: s(left),
                right: s(right),
            },
            G::LeftJoin {
                left,
                right,
                expression,
            } => G::LeftJoin {
                left: s(left),
                right: s(right),
                expression,
            },
            G::Union { left, right } => G::Union {
                left: s(left),
                right: s(right),
            },
            G::Minus { left, right } => G::Minus {
                left: s(left),
                right: s(right),
            },
            G::Filter { expr, inner } => G::Filter {
                expr,
                inner: s(inner),
            },
            G::Extend {
                inner,
                variable,
                expression,
            } => G::Extend {
                inner: s(inner),
                variable,
                expression,
            },
            G::OrderBy { inner, expression } => G::OrderBy {
                inner: s(inner),
                expression,
            },
            G::Project { inner, variables } => G::Project {
                inner: s(inner),
                variables,
            },
            G::Distinct { inner } => G::Distinct { inner: s(inner) },
            G::Reduced { inner } => G::Reduced { inner: s(inner) },
            G::Slice {
                inner,
                start,
                length,
            } => G::Slice {
                inner: s(inner),
                start,
                length,
            },
            G::Group {
                inner,
                variables,
                aggregates,
            } => G::Group {
                inner: s(inner),
                variables,
                aggregates,
            },
            other => other,
        }
    }

    /// A basic graph pattern with `tt:search` calls: the other triples, joined with a table
    /// of each call's matches.
    fn bgp(
        &self,
        patterns: Vec<TriplePattern>,
        outer: &[GraphPattern],
    ) -> Result<GraphPattern, Error> {
        let (calls, rest) = calls(patterns, &self.ctx.properties())?;
        let mut out = GraphPattern::Bgp {
            patterns: rest.clone(),
        };
        // Only built when a call searches text, not for `tt:nearest` alone.
        let index = if calls.iter().any(|c| c.nearest.is_none()) {
            self.ctx.index()?
        } else {
            Arc::default()
        };
        for call in calls {
            let table = match &call.query_var {
                None => table(&call, &index, self.ctx)?,
                Some(var) => self.lateral_table(&call, var, &index, outer, &rest)?,
            };
            out = GraphPattern::Join {
                left: Box::new(out),
                right: Box::new(table),
            };
        }
        Ok(out)
    }

    /// The matches of a call whose query string is the variable `var`, as Jena reads it: one
    /// search per string `var` takes in what the call is joined with (`outer` and the other
    /// triples of its group), each limited on its own, with `var` bound to its string.
    fn lateral_table(
        &self,
        call: &Call,
        var: &Variable,
        index: &TextIndex,
        outer: &[GraphPattern],
        rest: &[TriplePattern],
    ) -> Result<GraphPattern, Error> {
        let function = call.function;
        // Only what ?q's values depend on: the patterns linked to ?q by variables other than
        // the call's own. A pattern linked only through the call's results (its subject,
        // score or literal) does not narrow the strings, and read without the call would
        // multiply them by its rows.
        let own: HashSet<String> = term_name(&call.subject)
            .into_iter()
            .chain(
                [&call.score, &call.literal]
                    .into_iter()
                    .flatten()
                    .map(|v| format!("?{}", v.as_str())),
            )
            .collect();
        let mut linked: HashSet<String> = HashSet::from([format!("?{}", var.as_str())]);
        let mut parts: Vec<(Vec<String>, GraphPattern, bool)> = rest
            .iter()
            .map(|t| {
                let names = [&t.subject, &t.object]
                    .into_iter()
                    .filter_map(term_name)
                    .chain(match &t.predicate {
                        NamedNodePattern::Variable(v) => Some(format!("?{}", v.as_str())),
                        NamedNodePattern::NamedNode(_) => None,
                    })
                    .collect();
                let p = GraphPattern::Bgp {
                    patterns: vec![t.clone()],
                };
                (names, p, false)
            })
            .chain(outer.iter().map(|p| {
                let mut names = Vec::new();
                p.on_in_scope_variable(|v| names.push(format!("?{}", v.as_str())));
                (names, p.clone(), false)
            }))
            .collect();
        loop {
            let mut grew = false;
            for (names, _, kept) in &mut parts {
                if !*kept && names.iter().any(|n| linked.contains(n)) {
                    *kept = true;
                    grew = true;
                    linked.extend(names.iter().filter(|n| !own.contains(*n)).cloned());
                }
            }
            if !grew {
                break;
            }
        }
        let mut kept = parts.into_iter().filter(|(_, _, k)| *k).map(|(_, p, _)| p);
        let Some(first) = kept.next() else {
            return Err(Error::Request(format!(
                "{function}: nothing binds the query string ?{}; bind it before the search, \
                 as with VALUES or BIND",
                var.as_str()
            )));
        };
        let pattern = kept.fold(first, |left, right| GraphPattern::Join {
            left: Box::new(left),
            right: Box::new(right),
        });
        let values = self.ctx.values(&pattern, var, self.dataset)?;
        let strings: Vec<Literal> = values
            .into_iter()
            .filter_map(|t| match t {
                oxigraph::model::Term::Literal(l) => Some(l),
                _ => None,
            })
            // A query vector that is not one finds nothing, as a string with no words.
            .filter(|l| {
                call.nearest.is_none()
                    || crate::vector::parse(&oxigraph::model::Term::Literal(l.clone())).is_some()
            })
            .collect();
        if strings.len() > MAX_QUERY_STRINGS {
            return Err(Error::Request(format!(
                "{function}: ?{} takes {} query strings, more than the {MAX_QUERY_STRINGS} one \
                 query searches",
                var.as_str(),
                strings.len()
            )));
        }
        let mut arms = Vec::with_capacity(strings.len());
        for string in strings {
            let mut one = call.clone();
            one.query_var = None;
            match &mut one.nearest {
                Some((_, vector)) => {
                    *vector = crate::vector::parse(&oxigraph::model::Term::Literal(string.clone()))
                        .unwrap_or_default();
                }
                None => {
                    one.query = one
                        .parse(string.value())
                        .map_err(|m| Error::Request(format!("{function}: {m}")))?;
                }
            }
            arms.push(GraphPattern::Join {
                left: Box::new(GraphPattern::Values {
                    variables: vec![var.clone()],
                    bindings: vec![vec![Some(spargebra::term::GroundTerm::Literal(string))]],
                }),
                right: Box::new(table(&one, index, self.ctx)?),
            });
        }
        Ok(union_of(arms, var))
    }
}

/// How a variable or blank node of a triple pattern is told apart: `?name` or `_:id`.
fn term_name(t: &TermPattern) -> Option<String> {
    match t {
        TermPattern::Variable(v) => Some(format!("?{}", v.as_str())),
        TermPattern::BlankNode(b) => Some(format!("_:{}", b.as_str())),
        _ => None,
    }
}

/// The union of `arms`, balanced so that a long one does not nest deeply; no rows for none.
fn union_of(mut arms: Vec<GraphPattern>, var: &Variable) -> GraphPattern {
    if arms.len() <= 1 {
        return arms.pop().unwrap_or_else(|| GraphPattern::Values {
            variables: vec![var.clone()],
            bindings: Vec::new(),
        });
    }
    let right = arms.split_off(arms.len() / 2);
    GraphPattern::Union {
        left: Box::new(union_of(arms, var)),
        right: Box::new(union_of(right, var)),
    }
}

/// Whether `p` has a search call that reads the rest of its query: one outside a sub-select
/// (whose variables are its own). Any call counts: the rewrite then only keeps a little more.
fn lateral(p: &GraphPattern) -> bool {
    use GraphPattern as G;
    match p {
        G::Bgp { patterns } => patterns.iter().any(is_search),
        G::Service { name, .. } => is_fts(name) || is_qlever(name),
        G::Join { left, right }
        | G::Lateral { left, right }
        | G::LeftJoin { left, right, .. }
        | G::Union { left, right }
        | G::Minus { left, right } => lateral(left) || lateral(right),
        G::Filter { inner, .. }
        | G::Extend { inner, .. }
        | G::OrderBy { inner, .. }
        | G::Distinct { inner }
        | G::Reduced { inner }
        | G::Slice { inner, .. } => lateral(inner),
        _ => false,
    }
}

/// The `tt:search` and `text:query` calls among `patterns`, and the triples left (without
/// the calls' lists).
fn calls(
    patterns: Vec<TriplePattern>,
    properties: &[NamedNode],
) -> Result<(Vec<Call>, Vec<TriplePattern>), Error> {
    let iri = |t: &TriplePattern, i: &str| matches!(&t.predicate, NamedNodePattern::NamedNode(n) if n.as_str() == i);
    // rdf:first / rdf:rest of blank nodes: the argument lists.
    let mut first: HashMap<String, TermPattern> = HashMap::new();
    let mut rest: HashMap<String, TermPattern> = HashMap::new();
    for t in &patterns {
        if let TermPattern::BlankNode(b) = &t.subject {
            if iri(t, RDF_FIRST) {
                first.insert(b.as_str().to_owned(), t.object.clone());
            } else if iri(t, RDF_REST) {
                rest.insert(b.as_str().to_owned(), t.object.clone());
            }
        }
    }
    let mut used = std::collections::HashSet::new();
    // The items of the list `term` heads, or `term` alone.
    let mut items = |term: &TermPattern, bad: &dyn Fn(&str) -> Error| {
        let TermPattern::BlankNode(head) = term else {
            return Ok(vec![term.clone()]);
        };
        if !first.contains_key(head.as_str()) {
            return Ok(vec![term.clone()]);
        }
        let mut args = Vec::new();
        let mut node = head.as_str().to_owned();
        let mut seen = std::collections::HashSet::new();
        loop {
            let (Some(f), Some(r)) = (first.get(&node), rest.get(&node)) else {
                return Err(bad("its argument list is not a list"));
            };
            // A list that loops back, or longer than any call takes.
            if !seen.insert(node.clone()) || args.len() >= MAX_ARGS {
                return Err(bad("its argument list is not a list of at most 5 items"));
            }
            used.insert(node.clone());
            args.push(f.clone());
            match r {
                TermPattern::NamedNode(n) if n.as_str() == RDF_NIL => break,
                TermPattern::BlankNode(b) => node = b.as_str().to_owned(),
                _ => return Err(bad("its argument list is not a list")),
            }
        }
        Ok(args)
    };
    let mut calls = Vec::new();
    for t in patterns.iter().filter(|t| is_search(t)) {
        let jena = iri(t, TEXT_QUERY);
        let function = if jena {
            "text:query"
        } else if iri(t, BDS_SEARCH) {
            "bds:search"
        } else if iri(t, TEXT_MATCH) {
            "textMatch"
        } else if iri(t, APF_TEXT_MATCH) {
            "apf:textMatch"
        } else if iri(t, BIF_CONTAINS) {
            "bif:contains"
        } else if iri(t, QL_CONTAINS_WORD) {
            "ql:contains-word"
        } else if iri(t, QL_CONTAINS_ENTITY) {
            "ql:contains-entity"
        } else if let Some((prefix, _)) = connector_option(t) {
            match prefix {
                "luc" => "luc:query",
                "elastic" => "elastic:query",
                "solr" => "solr:query",
                _ => "opensearch:query",
            }
        } else if iri(t, NEAREST) {
            "tt:nearest"
        } else {
            "tt:search"
        };
        let bad = |m: &str| Error::Request(format!("{function}: {m}"));
        let mut subject = items(&t.subject, &bad)?.into_iter();
        let mut args = items(&t.object, &bad)?.into_iter().peekable();
        let mut call = Call::new(
            function,
            subject.next().unwrap_or_else(|| t.subject.clone()),
        );
        if function == "tt:nearest" {
            nearest(&mut call, subject, args, &bad)?;
        } else if function == "bds:search" {
            bds(&mut call, t, &patterns, &bad)?;
        } else if function == "textMatch" || function == "apf:textMatch" {
            text_match(&mut call, subject, args, &bad)?;
        } else if function == "bif:contains" || function == "ql:contains-word" {
            bif_contains(&mut call, subject, t, &bad)?;
        } else if function == "ql:contains-entity" {
            return Err(bad(
                "is not supported: the text records here are the literals of the data, which \
                 mention no entities; find the resource with a triple that has the literal",
            ));
        } else if function.ends_with(":query") && function != "text:query" {
            connector(&mut call, t, &patterns, &bad)?;
        } else {
            if jena {
                // (?s ?score ?literal) text:query (property "query" limit)
                if let Some(TermPattern::NamedNode(p)) = args.peek() {
                    if !properties.contains(p) {
                        return Err(bad(&format!(
                            "<{}> is not an indexed property; the indexed ones are {} (a dataset \
                         adds its own with text-properties)",
                            p.as_str(),
                            properties
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join(", ")
                        )));
                    }
                    call.property = Some(p.clone());
                    args.next();
                }
                for (i, s) in subject.enumerate() {
                    match s {
                        TermPattern::Variable(v) if i == 0 => call.score = Some(v),
                        TermPattern::Variable(v) if i == 1 => call.literal = Some(v),
                        _ => {
                            return Err(bad(
                                "the subject list is (?s ?score ?literal); ?graph and ?prop are not \
                             supported",
                            ));
                        }
                    }
                }
            } else if subject.next().is_some() {
                return Err(bad("the subject must be a single term"));
            }
            call.query = match args.next() {
                Some(TermPattern::Literal(l)) if jena => lucene(l.value()).map_err(|m| bad(&m))?,
                Some(TermPattern::Literal(l)) => TextQuery::plain(l.value()),
                Some(TermPattern::Variable(v)) => {
                    call.query_var = Some(v);
                    TextQuery::default()
                }
                _ => return Err(bad("the first argument must be the query string")),
            };
            for a in args {
                match a {
                    TermPattern::Variable(v) if !jena && call.score.is_none() => {
                        call.score = Some(v)
                    }
                    TermPattern::Variable(v) if !jena && call.literal.is_none() => {
                        call.literal = Some(v);
                    }
                    TermPattern::Literal(l)
                        if call.limit.is_none() && l.value().parse::<usize>().is_ok() =>
                    {
                        call.limit = l.value().parse().ok();
                    }
                    // Jena's "no limit".
                    TermPattern::Literal(l)
                        if jena && call.limit.is_none() && l.value() == "-1" => {}
                    TermPattern::Literal(l) if jena && l.value().starts_with("lang:") => {
                        let lang = l.value()["lang:".len()..].trim().to_ascii_lowercase();
                        if lang.is_empty() || call.lang.is_some() {
                            return Err(bad("lang: takes one language tag, as \"lang:en\""));
                        }
                        call.lang = Some(lang);
                    }
                    TermPattern::Literal(l) if jena && l.value().starts_with("highlight:") => {
                        call.highlight = Some(
                            Highlight::parse(&l.value()["highlight:".len()..])
                                .map_err(|m| bad(&m))?,
                        );
                    }
                    _ if jena => {
                        return Err(bad(
                            "expected (property \"query\" limit), the property and limit optional",
                        ));
                    }
                    _ => {
                        return Err(bad(
                            "expected (\"query\" ?score ?literal limit), each after the query optional",
                        ));
                    }
                }
            }
        }
        distinct_variables(&call, &bad)?;
        calls.push(call);
    }
    if let Some(o) = patterns.iter().find(|o| {
        is_bds_option(o)
            && !patterns
                .iter()
                .any(|t| iri(t, BDS_SEARCH) && t.subject == o.subject)
    }) {
        return Err(Error::Request(format!(
            "{} is a bds:search option, but nothing searches with bds:search on {}",
            o.predicate, o.subject
        )));
    }
    // A connector's options: on a node that searches, or the score of what it finds.
    if let Some(o) = patterns.iter().find(|o| {
        connector_option(o).is_some_and(|(_, name)| {
            !patterns.iter().any(|t| {
                connector_option(t).is_some_and(|(_, n)| n == "query") && t.subject == o.subject
            }) && !(name == "score" && calls.iter().any(|c| c.subject == o.subject))
        })
    }) {
        return Err(Error::Request(format!(
            "{} is an option of a GraphDB connector's search, but nothing searches with its \
             query on {}",
            o.predicate, o.subject
        )));
    }
    let left = patterns
        .into_iter()
        .filter(|t| {
            !is_search(t)
                && !is_option(t)
                && !matches!(&t.subject, TermPattern::BlankNode(b)
                    if used.contains(b.as_str()) && (iri(t, RDF_FIRST) || iri(t, RDF_REST)))
        })
        .collect();
    Ok((calls, left))
}

/// The subject, score, literal, query and rank of `call` are different variables.
fn distinct_variables(call: &Call, bad: &dyn Fn(&str) -> Error) -> Result<(), Error> {
    let mut names: Vec<&str> = Vec::new();
    if let TermPattern::Variable(v) = &call.subject {
        names.push(v.as_str());
    }
    for v in [&call.score, &call.literal, &call.query_var, &call.rank]
        .into_iter()
        .flatten()
    {
        if names.contains(&v.as_str()) {
            return Err(bad(&format!(
                "?{} is given twice: the subject, score, literal and query each need a \
                 variable of their own",
                v.as_str()
            )));
        }
        names.push(v.as_str());
    }
    Ok(())
}

/// `?s tt:nearest (property query k ?score ?vector)` (#236): the `k` resources whose vector
/// of `property` is nearest the query vector (a vector literal, a string in the same form, or
/// a variable bound by the rest of the query), with their cosine similarity and the vector
/// literal. `k` and what follows it are optional; without `k`, every vector is a match.
fn nearest(
    call: &mut Call,
    mut subject: impl Iterator<Item = TermPattern>,
    args: impl Iterator<Item = TermPattern>,
    bad: &dyn Fn(&str) -> Error,
) -> Result<(), Error> {
    if subject.next().is_some() {
        return Err(bad("the subject must be a single term"));
    }
    let usage = "expected (property query k ?score ?vector), k and what follows it optional";
    let mut args = args.peekable();
    let Some(TermPattern::NamedNode(property)) = args.next() else {
        return Err(bad(usage));
    };
    let vector = match args.next() {
        Some(TermPattern::Literal(l)) => crate::vector::parse(&oxigraph::model::Term::Literal(l))
            .ok_or_else(|| {
            bad("the query must be a vector: a JSON array of numbers, as \"[0.1, 0.2]\"")
        })?,
        Some(TermPattern::Variable(v)) => {
            call.query_var = Some(v);
            Vec::new()
        }
        _ => return Err(bad(usage)),
    };
    if let Some(TermPattern::Literal(l)) = args.peek() {
        call.limit = Some(
            l.value()
                .parse()
                .map_err(|_| bad("k must be a whole number"))?,
        );
        args.next();
    }
    for a in args {
        match a {
            TermPattern::Variable(v) if call.score.is_none() => call.score = Some(v),
            TermPattern::Variable(v) if call.literal.is_none() => call.literal = Some(v),
            _ => return Err(bad(usage)),
        }
    }
    call.nearest = Some((property, vector));
    Ok(())
}

/// The rows of a `tt:nearest` call, best first.
fn nearest_table(
    call: &Call,
    property: &oxigraph::model::NamedNode,
    vector: &[f64],
    ctx: &impl SearchContext,
) -> Result<GraphPattern, Error> {
    let function = call.function;
    let (subject_var, wanted) = match &call.subject {
        TermPattern::Variable(v) => (Some(v.clone()), None),
        TermPattern::BlankNode(_) => (None, None),
        TermPattern::NamedNode(n) => (None, Some(ctx.term_id(n.as_ref().into()))),
        _ => {
            return Err(Error::Request(format!(
                "{function}: the subject must be a resource"
            )));
        }
    };
    let index = ctx.vectors(property)?;
    // The best k first, then the subject asked for among them: a constant subject asks
    // whether it is among the best k.
    let mut hits = index.nearest(vector, call.limit);
    if let Some(w) = wanted {
        hits.retain(|h| w == Some(h.subject));
    }
    let mut variables = Vec::new();
    variables.extend(subject_var.clone());
    variables.extend(call.score.clone());
    variables.extend(call.literal.clone());
    if variables.is_empty() {
        return Ok(GraphPattern::Values {
            variables: Vec::new(),
            bindings: if hits.is_empty() {
                Vec::new()
            } else {
                vec![Vec::new()]
            },
        });
    }
    let mut ids = Vec::with_capacity(hits.len() * variables.len());
    for hit in &hits {
        if subject_var.is_some() {
            ids.push(hit.subject);
        }
        if call.score.is_some() {
            let score = Literal::new_typed_literal(
                format!("{:.6}", hit.score),
                oxigraph::model::vocab::xsd::DOUBLE,
            );
            ids.push(
                ctx.term_id(score.as_ref().into()).ok_or_else(|| {
                    Error::Request(format!("{function}: a score is not a term id"))
                })?,
            );
        }
        if call.literal.is_some() {
            ids.push(hit.literal);
        }
    }
    if ids.is_empty() {
        return Ok(GraphPattern::Values {
            variables,
            bindings: Vec::new(),
        });
    }
    let width = variables.len();
    ctx.table(&variables, IdRows { width, ids })
        .ok_or_else(|| Error::Request(format!("{function}: too many matches for one table")))
}

/// A call that matches literals, not resources (`bds:search`, `textMatch`): its subject is
/// the variable of the literal matched.
fn literal_subject(call: &mut Call, bad: &dyn Fn(&str) -> Error) -> Result<(), Error> {
    let subject = std::mem::replace(
        &mut call.subject,
        TermPattern::BlankNode(spargebra::term::BlankNode::default()),
    );
    let TermPattern::Variable(literal) = subject else {
        return Err(bad("the subject must be a variable: the literal matched"));
    };
    call.literal = Some(literal);
    call.per_literal = true;
    Ok(())
}

/// Blazegraph's `?o bds:search "words"`, with its options on `?o` (#188).
fn bds(
    call: &mut Call,
    t: &TriplePattern,
    patterns: &[TriplePattern],
    bad: &dyn Fn(&str) -> Error,
) -> Result<(), Error> {
    literal_subject(call, bad)?;
    let mut query = None;
    match &t.object {
        TermPattern::Literal(l) => query = Some(l.value().to_owned()),
        TermPattern::Variable(v) => call.query_var = Some(v.clone()),
        _ => return Err(bad("the object must be the query string")),
    }
    for o in patterns
        .iter()
        .filter(|o| is_bds_option(o) && o.subject == t.subject)
    {
        let NamedNodePattern::NamedNode(p) = &o.predicate else {
            continue;
        };
        let name = &p.as_str()[BDS.len()..];
        let value = || match &o.object {
            TermPattern::Literal(l) => Ok(l.value().to_owned()),
            _ => Err(bad(&format!("bds:{name} takes a constant"))),
        };
        let variable = || match &o.object {
            TermPattern::Variable(v) => Ok(v.clone()),
            _ => Err(bad(&format!("bds:{name} takes a variable"))),
        };
        let number = |v: String| {
            v.parse::<usize>()
                .map_err(|_| bad(&format!("bds:{name} takes a whole number")))
        };
        let flag = |v: String| match v.as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(bad(&format!("bds:{name} takes true or false"))),
        };
        match name {
            "relevance" => call.score = Some(variable()?),
            "rank" => call.rank = Some(variable()?),
            "maxRank" => call.limit = Some(number(value()?)?),
            "minRank" => call.skip = number(value()?)?.saturating_sub(1),
            "matchAllTerms" => call.all_terms = flag(value()?)?,
            "prefixMatch" => call.prefix_terms = flag(value()?)?,
            // The query's own timeout governs.
            "searchTimeout" => {}
            "minRelevance" | "maxRelevance" => {
                return Err(bad(&format!(
                    "bds:{name} is not supported: bds:relevance is a BM25 score here, not \
                     Blazegraph's from 0 to 1; use bds:maxRank, or FILTER on the score"
                )));
            }
            _ => {
                return Err(bad(&format!(
                    "bds:{name} is not supported; bds:relevance, rank, maxRank, minRank, \
                     matchAllTerms, prefixMatch and searchTimeout are"
                )));
            }
        }
    }
    if let Some(q) = query {
        call.query = call.parse(&q).map_err(|m| bad(&m))?;
    }
    Ok(())
}

/// Stardog's `?literal textMatch ("query" limit)`, or `(?literal ?score) textMatch …` (#188).
fn text_match(
    call: &mut Call,
    subject: impl Iterator<Item = TermPattern>,
    mut args: impl Iterator<Item = TermPattern>,
    bad: &dyn Fn(&str) -> Error,
) -> Result<(), Error> {
    literal_subject(call, bad)?;
    for (i, s) in subject.enumerate() {
        match s {
            TermPattern::Variable(v) if i == 0 => call.score = Some(v),
            _ => return Err(bad("the subject is ?literal or (?literal ?score)")),
        }
    }
    match args.next() {
        Some(TermPattern::Literal(l)) => call.query = call.parse(l.value()).map_err(|m| bad(&m))?,
        Some(TermPattern::Variable(v)) => call.query_var = Some(v),
        _ => return Err(bad("the first argument must be the query string")),
    }
    for a in args {
        match a {
            TermPattern::Literal(l)
                if call.limit.is_none() && l.value().parse::<usize>().is_ok() =>
            {
                call.limit = l.value().parse().ok();
            }
            TermPattern::Literal(l) if l.value().parse::<i64>().is_ok() => {
                return Err(bad("the limit must be a whole number of 0 or more"));
            }
            TermPattern::Literal(l) if l.value().parse::<f64>().is_ok() => {
                return Err(bad(
                    "a score threshold is not supported: scores are BM25 scores here, not \
                     Lucene's; give a limit, or FILTER on the score",
                ));
            }
            _ => return Err(bad("expected (\"query\" limit), the limit optional")),
        }
    }
    Ok(())
}

/// Virtuoso's `?o bif:contains "words"`: `?o` is the literal matched, the string in
/// Virtuoso's syntax. Virtuoso's `OPTION (score ?sc)` is not SPARQL, so no score is given.
fn bif_contains(
    call: &mut Call,
    mut subject: impl Iterator<Item = TermPattern>,
    t: &TriplePattern,
    bad: &dyn Fn(&str) -> Error,
) -> Result<(), Error> {
    if subject.next().is_some() {
        return Err(bad(
            "the subject must be a single variable: the literal matched",
        ));
    }
    literal_subject(call, bad)?;
    match &t.object {
        TermPattern::Literal(l) => call.query = call.parse(l.value()).map_err(|m| bad(&m))?,
        TermPattern::Variable(v) => call.query_var = Some(v.clone()),
        _ => return Err(bad("the object must be the query string")),
    }
    Ok(())
}

/// A GraphDB connector's search: `?search a <…/instance#index> ; luc:query "words" ;
/// luc:entities ?entity ; luc:limit n ; luc:offset n . ?entity luc:score ?score`, the query
/// in Lucene's syntax. The entities are the resources whose indexed literals match: one index
/// answers whatever index is named.
fn connector(
    call: &mut Call,
    t: &TriplePattern,
    patterns: &[TriplePattern],
    bad: &dyn Fn(&str) -> Error,
) -> Result<(), Error> {
    let prefix = call.function.trim_end_matches(":query");
    let mut entities = None;
    let mut query = None;
    let mut seen: Vec<&str> = Vec::new();
    for o in patterns.iter().filter(|o| o.subject == t.subject) {
        let Some((_, name)) = connector_option(o) else {
            continue;
        };
        if name != "type" && seen.contains(&name) {
            return Err(bad(&format!("{prefix}:{name} is given twice")));
        }
        seen.push(name);
        let number = || match &o.object {
            TermPattern::Literal(l) => l.value().parse::<usize>().map_err(|_| {
                bad(&format!(
                    "{prefix}:{name} takes a whole number of 0 or more"
                ))
            }),
            _ => Err(bad(&format!("{prefix}:{name} takes a whole number"))),
        };
        match name {
            "type" => {}
            "query" => match &o.object {
                TermPattern::Literal(l) => query = Some(l.value().to_owned()),
                TermPattern::Variable(v) => call.query_var = Some(v.clone()),
                _ => return Err(bad("the query must be a string, or a variable")),
            },
            "entities" => match &o.object {
                TermPattern::Variable(v) => entities = Some(v.clone()),
                _ => return Err(bad(&format!("{prefix}:entities takes a variable"))),
            },
            "limit" => call.limit = Some(number()?),
            "offset" => call.skip = number()?,
            _ => {
                return Err(bad(&format!(
                    "{prefix}:{name} is not supported; {prefix}:query, entities, limit and \
                     offset are, with {prefix}:score on the entity"
                )));
            }
        }
    }
    let Some(entity) = entities else {
        return Err(bad(&format!(
            "{prefix}:entities ?entity is needed: the variable of the resources found"
        )));
    };
    for o in patterns.iter().filter(|o| {
        o.subject == TermPattern::Variable(entity.clone())
            && connector_option(o).is_some_and(|(_, n)| n == "score")
    }) {
        match &o.object {
            TermPattern::Variable(v) if call.score.is_none() => call.score = Some(v.clone()),
            TermPattern::Variable(_) => return Err(bad(&format!("{prefix}:score is given twice"))),
            _ => return Err(bad(&format!("{prefix}:score takes a variable"))),
        }
    }
    call.subject = TermPattern::Variable(entity);
    call.limit = call.limit.map(|l| l.saturating_add(call.skip));
    if let Some(q) = query {
        call.query = call.parse(&q).map_err(|m| bad(&m))?;
    }
    Ok(())
}

fn is_qlever(name: &NamedNodePattern) -> bool {
    matches!(name, NamedNodePattern::NamedNode(n) if n.as_str() == QLEVER_TEXT_SEARCH)
}

/// QLever's `SERVICE textSearch: { ?t textSearch:contains [ textSearch:word "word*" ;
/// textSearch:score ?score ] }`: a call per text variable, whose records are the literals
/// matched, each of its words required. Entity searches are refused: the literals mention no
/// entities.
fn qlever_calls(inner: &GraphPattern) -> Result<Vec<Call>, Error> {
    let bad = |m: &str| Error::Request(format!("textSearch: {m}"));
    let GraphPattern::Bgp { patterns } = inner else {
        return Err(bad("the service's pattern must be triples"));
    };
    let option = |t: &TriplePattern| match &t.predicate {
        NamedNodePattern::NamedNode(p) => p
            .as_str()
            .strip_prefix(QLEVER_TEXT_SEARCH)
            .map(str::to_owned),
        NamedNodePattern::Variable(_) => None,
    };
    let mut calls: Vec<Call> = Vec::new();
    let mut configs: Vec<TermPattern> = Vec::new();
    for t in patterns {
        if option(t).as_deref() != Some("contains") {
            continue;
        }
        let TermPattern::Variable(text) = &t.subject else {
            return Err(bad(
                "the subject of textSearch:contains must be a variable: the text",
            ));
        };
        if configs.contains(&t.object) {
            return Err(bad("a configuration is linked to several text variables"));
        }
        configs.push(t.object.clone());
        let (mut word, mut score) = (None, None);
        let mut seen: Vec<String> = Vec::new();
        for o in patterns.iter().filter(|o| o.subject == t.object) {
            let Some(name) = option(o) else {
                return Err(bad(&format!("{} is not a textSearch: option", o.predicate)));
            };
            if seen.contains(&name) {
                return Err(bad(&format!(
                    "textSearch:{name} is given twice in a configuration"
                )));
            }
            seen.push(name.clone());
            match (name.as_str(), &o.object) {
                ("word", TermPattern::Literal(l)) if !l.value().trim().is_empty() => {
                    word = Some(l.value().to_owned());
                }
                ("word", _) => return Err(bad("textSearch:word takes a word, not empty")),
                ("score", TermPattern::Variable(v)) => score = Some(v.clone()),
                ("score", _) => return Err(bad("textSearch:score takes a variable")),
                ("entity", _) => {
                    return Err(bad(
                        "textSearch:entity is not supported: the text records here are the \
                         literals of the data, which mention no entities; find the resource \
                         with a triple that has the literal",
                    ));
                }
                (other, _) => {
                    return Err(bad(&format!(
                        "textSearch:{other} is not supported; textSearch:word and score are"
                    )));
                }
            }
        }
        let Some(word) = word else {
            return Err(bad("a configuration needs textSearch:word"));
        };
        let mut call = match calls.iter().position(|c| c.literal.as_ref() == Some(text)) {
            Some(i) => calls.remove(i),
            None => {
                let mut call = Call::new(
                    "textSearch:word",
                    TermPattern::BlankNode(spargebra::term::BlankNode::default()),
                );
                call.literal = Some(text.clone());
                call.per_literal = true;
                call
            }
        };
        call.query
            .clauses
            .extend(call.parse(&word).map_err(|m| bad(&m))?.clauses);
        if let Some(score) = score {
            if call.score.is_some() {
                return Err(bad(
                    "one textSearch:score per text variable: the score here is that of all its \
                     words together",
                ));
            }
            call.score = Some(score);
        }
        calls.push(call);
    }
    if calls.is_empty() {
        return Err(bad("textSearch:contains is needed"));
    }
    if let Some(t) = patterns
        .iter()
        .find(|t| option(t).as_deref() != Some("contains") && !configs.contains(&t.subject))
    {
        return Err(bad(&format!(
            "{t} is not on a configuration of textSearch:contains"
        )));
    }
    for call in &calls {
        distinct_variables(call, &bad)?;
    }
    Ok(calls)
}

/// QLever's words: each required, a trailing `*` matching the words it starts.
fn qlever_words(query: &str) -> Result<TextQuery, String> {
    let mut clauses = Vec::new();
    for token in query.split_whitespace() {
        let (stem, prefix) = match token.strip_suffix('*') {
            Some(stem) => (stem, true),
            None => (token, false),
        };
        if stem.contains('*') || (prefix && stem.is_empty()) {
            return Err(format!(
                "a wildcard other than at the end of a word (\"{token}\")"
            ));
        }
        for word in words(stem) {
            clauses.push(Clause {
                occur: Occur::Must,
                words: vec![word],
                prefix,
                stem: None,
            });
        }
    }
    if clauses.is_empty() {
        return Err("the query has no words".into());
    }
    Ok(TextQuery { clauses })
}

/// Stardog's `SERVICE fts:textMatch { [] fts:query "words" ; fts:result ?literal ;
/// fts:score ?score ; fts:limit n ; fts:offset n }`: the literals matched, as `textMatch`
/// gives them, the query string in Lucene's syntax.
fn fts_call(inner: &GraphPattern) -> Result<Call, Error> {
    let bad = |m: &str| Error::Request(format!("fts:textMatch: {m}"));
    let GraphPattern::Bgp { patterns } = inner else {
        return Err(bad(
            "the service's pattern must be fts: options of one node, as [] fts:query \"words\" ; \
             fts:result ?literal",
        ));
    };
    let mut call = Call::new(
        "fts:textMatch",
        TermPattern::BlankNode(spargebra::term::BlankNode::default()),
    );
    call.per_literal = true;
    let mut query = None;
    let mut seen: Vec<&str> = Vec::new();
    for t in patterns {
        let name = match &t.predicate {
            NamedNodePattern::NamedNode(p) if p.as_str().starts_with(FTS) => {
                &p.as_str()[FTS.len()..]
            }
            p => return Err(bad(&format!("{p} is not an fts: option"))),
        };
        if t.subject != patterns[0].subject {
            return Err(bad("the options must all be on one node"));
        }
        if seen.contains(&name) {
            return Err(bad(&format!("fts:{name} is given twice")));
        }
        seen.push(name);
        let variable = || match &t.object {
            TermPattern::Variable(v) => Ok(v.clone()),
            _ => Err(bad(&format!("fts:{name} takes a variable"))),
        };
        let number = || match &t.object {
            TermPattern::Literal(l) => l
                .value()
                .parse::<usize>()
                .map_err(|_| bad(&format!("fts:{name} takes a whole number of 0 or more"))),
            _ => Err(bad(&format!("fts:{name} takes a whole number"))),
        };
        match name {
            "query" => match &t.object {
                TermPattern::Literal(l) => query = Some(l.value().to_owned()),
                TermPattern::Variable(v) => call.query_var = Some(v.clone()),
                _ => return Err(bad("fts:query takes the query string, or a variable")),
            },
            "result" => call.literal = Some(variable()?),
            "score" => call.score = Some(variable()?),
            "limit" => call.limit = Some(number()?),
            "offset" => call.skip = number()?,
            "threshold" => {
                return Err(bad(
                    "fts:threshold is not supported: scores are BM25 scores here, not \
                     Lucene's; give fts:limit, or FILTER on the score",
                ));
            }
            _ => {
                return Err(bad(&format!(
                    "fts:{name} is not supported; fts:query, result, score, limit and offset are"
                )));
            }
        }
    }
    if call.literal.is_none() {
        return Err(bad(
            "fts:result ?literal is needed: the variable of the literal matched",
        ));
    }
    // The limit counts from the offset; a call's limit is a place, as bds:maxRank.
    call.limit = call.limit.map(|l| l.saturating_add(call.skip));
    match query {
        Some(q) => call.query = call.parse(&q).map_err(|m| bad(&m))?,
        None if call.query_var.is_none() => return Err(bad("fts:query is needed")),
        None => {}
    }
    distinct_variables(&call, &bad)?;
    Ok(call)
}

fn is_fts(name: &NamedNodePattern) -> bool {
    matches!(name, NamedNodePattern::NamedNode(n) if n.as_str() == FTS_TEXT_MATCH)
}

/// A `bif:contains` string in Virtuoso's text expression syntax: words and phrases joined by
/// `AND`, `OR` or `AND NOT` (in any case; words side by side are AND'ed), a phrase in double
/// or single quotes, and a quoted word ending in `*` matching the words it starts. Refused
/// rather than read differently: `NEAR`, parentheses, `AND` and `OR` in one expression
/// (they need parentheses), `NOT` other than after `AND`, and `*` other than ending a word.
fn virtuoso(query: &str) -> Result<TextQuery, String> {
    #[derive(PartialEq)]
    enum Token {
        Words(String, bool),
        And,
        Or,
        Not,
    }
    let mut tokens = Vec::new();
    let mut chars = query.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        if c == '(' || c == ')' {
            return Err("parentheses in the query are not supported".into());
        }
        if c == '"' || c == '\'' {
            chars.next();
            let mut phrase = String::new();
            loop {
                match chars.next() {
                    Some(d) if d == c => break,
                    Some(d) => phrase.push(d),
                    None => return Err(format!("a phrase is not closed ({c}{phrase})")),
                }
            }
            tokens.push(Token::Words(phrase, true));
            continue;
        }
        let mut word = String::new();
        while let Some(&d) = chars.peek() {
            if d.is_whitespace() || matches!(d, '"' | '\'' | '(' | ')') {
                break;
            }
            word.push(d);
            chars.next();
        }
        tokens.push(match word.to_ascii_lowercase().as_str() {
            "and" => Token::And,
            "or" => Token::Or,
            "not" => Token::Not,
            "near" => return Err("NEAR in the query is not supported".into()),
            _ => Token::Words(word, false),
        });
    }
    let mut clauses: Vec<Clause> = Vec::new();
    let (mut and, mut or) = (false, false);
    let mut tokens = tokens.into_iter().peekable();
    let mut first = true;
    loop {
        // What joins the next word or phrase to those before.
        let occur = if first {
            Occur::Must
        } else {
            match tokens.peek() {
                None => break,
                // Side by side: AND.
                Some(Token::Words(..)) => {
                    and = true;
                    Occur::Must
                }
                Some(Token::And) => {
                    tokens.next();
                    and = true;
                    if tokens.next_if(|t| *t == Token::Not).is_some() {
                        Occur::MustNot
                    } else {
                        Occur::Must
                    }
                }
                Some(Token::Or) => {
                    tokens.next();
                    or = true;
                    Occur::Should
                }
                Some(Token::Not) => {
                    return Err("NOT must follow AND, as in \"rail AND NOT tram\"".into());
                }
            }
        };
        match tokens.next() {
            Some(Token::Words(text, quoted)) => {
                clauses.extend(virtuoso_operand(&text, quoted, occur)?);
            }
            None if first => return Err("the query has no words".into()),
            Some(Token::Not) => {
                return Err("NOT must follow AND, as in \"rail AND NOT tram\"".into());
            }
            _ if first => return Err("the query starts with AND, OR or NOT".into()),
            _ => return Err("AND or OR with no word or phrase after it".into()),
        }
        first = false;
    }
    if and && or {
        return Err("AND and OR in one query need parentheses, which are not supported".into());
    }
    if or {
        // Any may match: the first operand too.
        for c in &mut clauses {
            c.occur = Occur::Should;
        }
    }
    Ok(TextQuery { clauses })
}

/// The clauses of one word or phrase of a `bif:contains` string.
fn virtuoso_operand(text: &str, quoted: bool, occur: Occur) -> Result<Vec<Clause>, String> {
    let (stem, prefix) = match text.strip_suffix('*') {
        Some(stem) => (stem, true),
        None => (text, false),
    };
    if stem.contains('*') {
        return Err(format!(
            "a wildcard other than at the end of a word (\"{text}\")"
        ));
    }
    let found: Vec<String> = words(stem).collect();
    if prefix && found.len() != 1 {
        return Err(format!(
            "a wildcard in a phrase is not supported (\"{text}\")"
        ));
    }
    Ok(if quoted && found.len() > 1 {
        vec![Clause {
            occur,
            words: found,
            prefix: false,
            stem: None,
        }]
    } else {
        found
            .into_iter()
            .map(|word| Clause {
                occur,
                words: vec![word],
                prefix,
                stem: None,
            })
            .collect()
    })
}

/// A `text:query` string in Lucene's syntax (#830). Words are OR'ed, as by Lucene's default;
/// `+word` and `AND` make words required, `-word`, `!word` and `NOT` exclude them, `OR` and
/// `||` are the default, `"several words"` is a phrase (the words one after the other), and a
/// trailing `*` matches the words a word starts. Syntax that would be read with another meaning
/// is refused rather than searched for differently: fields, `?`, `~`, `^`, groups, ranges,
/// escapes, `*` elsewhere than at the end of a word, and a phrase with `*`.
fn lucene(query: &str) -> Result<TextQuery, String> {
    // The tokens: runs between spaces, a quoted phrase (with its sign) as one.
    let mut tokens: Vec<String> = Vec::new();
    let mut chars = query.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut token = String::new();
        let mut quoted = false;
        while let Some(&c) = chars.peek() {
            if c == '"' {
                quoted = !quoted;
            } else if c.is_whitespace() && !quoted {
                break;
            }
            token.push(c);
            chars.next();
        }
        if quoted {
            return Err(format!("a phrase is not closed (\"{token}\")"));
        }
        tokens.push(token);
    }
    let mut clauses: Vec<Clause> = Vec::new();
    let (mut and, mut not) = (false, false);
    for token in &tokens {
        let refuse = |what: &str| {
            Err(format!(
                "{what} in the query is not supported (\"{token}\")"
            ))
        };
        match token.as_str() {
            "AND" | "&&" => {
                // The clause before is required too, unless it is excluded.
                match clauses.last_mut() {
                    Some(c) if c.occur == Occur::Should => c.occur = Occur::Must,
                    Some(_) => {}
                    None => return Err("AND with nothing before it".into()),
                }
                and = true;
                continue;
            }
            "OR" | "||" => continue,
            "NOT" | "!" => {
                not = true;
                continue;
            }
            _ => {}
        }
        let (mut occur, body) = match token.chars().next() {
            Some('+') => (Occur::Must, &token[1..]),
            Some('-' | '!') => (Occur::MustNot, &token[1..]),
            _ => (Occur::Should, token.as_str()),
        };
        if not {
            occur = Occur::MustNot;
        } else if and && occur == Occur::Should {
            occur = Occur::Must;
        }
        (and, not) = (false, false);
        if let Some(inner) = body.strip_prefix('"') {
            let Some(inner) = inner.strip_suffix('"') else {
                return refuse("a phrase");
            };
            if inner.contains(['"', '*']) {
                return refuse("a wildcard in a phrase");
            }
            let found: Vec<String> = words(inner).collect();
            if !found.is_empty() {
                clauses.push(Clause {
                    occur,
                    words: found,
                    prefix: false,
                    stem: None,
                });
            }
            continue;
        }
        if body.contains('"') {
            return refuse("a phrase");
        }
        if body.contains(':') {
            return refuse("a field");
        }
        if body.contains(['?', '~', '^', '(', ')', '[', ']', '{', '}', '\\']) {
            return refuse("this Lucene syntax");
        }
        let (stem, prefix) = match body.strip_suffix('*') {
            Some(stem) => (stem, true),
            None => (body, false),
        };
        if stem.contains('*') || (prefix && stem.is_empty()) {
            return refuse("a wildcard other than at the end");
        }
        for word in words(stem) {
            clauses.push(Clause {
                occur,
                words: vec![word],
                prefix,
                stem: None,
            });
        }
    }
    if and || not {
        return Err(format!(
            "{} with nothing after it",
            if and { "AND" } else { "NOT" }
        ));
    }
    Ok(TextQuery { clauses })
}

/// The rows of one call: subject, then score and literal if asked for.
/// Numbers the variables that join a call's highlighted literals to its rows.
static HIGHLIGHTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn table(call: &Call, index: &TextIndex, ctx: &impl SearchContext) -> Result<GraphPattern, Error> {
    if let Some((property, vector)) = &call.nearest {
        return nearest_table(call, property, vector, ctx);
    }
    let function = call.function;
    // A constant subject keeps its own matches, before the limit; a blank node is a variable
    // no one reads.
    let (subject_var, wanted) = match &call.subject {
        TermPattern::Variable(v) => (Some(v.clone()), None),
        TermPattern::BlankNode(_) => (None, None),
        TermPattern::NamedNode(n) => (None, Some(ctx.term_id(n.as_ref().into()))),
        _ => {
            return Err(Error::Request(format!(
                "{function}: the subject must be a resource"
            )));
        }
    };
    // With a property, only its literals: one known to no triple has none.
    let property = call
        .property
        .as_ref()
        .map(|p| ctx.term_id(p.as_ref().into()));
    let text = |id: TermId| ctx.literal(id).map(|l| l.value().to_owned());
    // In a language with a stemmer, its words match their other forms (#830).
    let stemmed;
    let query = match call.lang.as_deref().and_then(Stemmer::of) {
        Some(stem) => {
            stemmed = call.query.clone().stemmed(stem);
            &stemmed
        }
        None => &call.query,
    };
    // One row per literal: the limit counts literals, after the best score of each is kept.
    let after = call.per_literal || call.skip > 0;
    let limit = if after { None } else { call.limit };
    let hits = index.search_query(
        query,
        limit,
        |s, l| {
            wanted.is_none_or(|w| w == Some(s))
                && property.is_none_or(|p| p.is_some_and(|p| ctx.holds(s, p, l)))
                && call.lang.as_ref().is_none_or(|lang| {
                    ctx.literal(l)
                        .and_then(|l| l.language().map(str::to_ascii_lowercase))
                        .is_some_and(|tag| tag == *lang || tag.starts_with(&format!("{lang}-")))
                })
        },
        &text,
    );
    let hits: Vec<Hit> = if !after {
        hits
    } else {
        let mut seen = HashSet::new();
        hits.into_iter()
            .filter(|h| !call.per_literal || seen.insert(h.literal))
            .skip(call.skip)
            // bds:maxRank is a place, as bds:minRank: the places up to it.
            .take(
                call.limit
                    .map_or(usize::MAX, |l| l.saturating_sub(call.skip)),
            )
            .collect()
    };
    // Highlighted literals are new terms: the rows carry the literal matched, in a variable
    // of their own, joined to a table of it and its highlighted form.
    let highlight = match (&call.highlight, &call.literal) {
        (Some(h), Some(v)) => Some((
            h,
            v.clone(),
            Variable::new_unchecked(format!(
                "mg_text_literal_{}",
                HIGHLIGHTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )),
        )),
        _ => None,
    };
    let mut variables = Vec::new();
    variables.extend(subject_var.clone());
    variables.extend(call.score.clone());
    match &highlight {
        Some((_, _, hidden)) => variables.push(hidden.clone()),
        None => variables.extend(call.literal.clone()),
    }
    variables.extend(call.rank.clone());
    let mut marked: Vec<Vec<Option<spargebra::term::GroundTerm>>> = Vec::new();
    if let Some((h, _, _)) = &highlight {
        let query = query.matching_words();
        let mut seen = std::collections::HashSet::new();
        for hit in &hits {
            if !seen.insert(hit.literal) {
                continue;
            }
            let Some(l) = ctx.literal(hit.literal) else {
                continue;
            };
            let shown = h.apply(l.value(), &query);
            let shown = match l.language() {
                Some(lang) => Literal::new_language_tagged_literal_unchecked(shown, lang),
                None => Literal::new_simple_literal(shown),
            };
            marked.push(vec![
                Some(spargebra::term::GroundTerm::Literal(l)),
                Some(spargebra::term::GroundTerm::Literal(shown)),
            ]);
        }
    }
    let mut ids = Vec::with_capacity(hits.len() * variables.len());
    let mut matched = 0usize;
    for (place, hit) in hits.into_iter().enumerate() {
        let score = Literal::new_typed_literal(
            format!("{:.6}", hit.score),
            oxigraph::model::vocab::xsd::DOUBLE,
        );
        let Some(score_id) = ctx.term_id(score.as_ref().into()) else {
            return Err(Error::Request(format!(
                "{function}: a score is not a term id"
            )));
        };
        let mut row = Vec::new();
        if subject_var.is_some() {
            row.push(hit.subject);
        }
        if call.score.is_some() {
            row.push(score_id);
        }
        if call.literal.is_some() {
            row.push(hit.literal);
        }
        if call.rank.is_some() {
            let rank = Literal::new_typed_literal(
                (call.skip + place + 1).to_string(),
                oxigraph::model::vocab::xsd::INTEGER,
            );
            let Some(rank_id) = ctx.term_id(rank.as_ref().into()) else {
                return Err(Error::Request(format!(
                    "{function}: a rank is not a term id"
                )));
            };
            row.push(rank_id);
        }
        ids.extend(row);
        matched += 1;
    }
    if variables.is_empty() {
        // Nothing to bind: one empty row if anything matched, none otherwise.
        return Ok(GraphPattern::Values {
            variables: Vec::new(),
            bindings: if matched == 0 {
                Vec::new()
            } else {
                vec![Vec::new()]
            },
        });
    }
    let width = variables.len();
    if ids.is_empty() {
        return Ok(GraphPattern::Values {
            variables,
            bindings: Vec::new(),
        });
    }
    let rows = ctx
        .table(&variables, IdRows { width, ids })
        .ok_or_else(|| Error::Request(format!("{function}: too many matches for one table")))?;
    Ok(match highlight {
        Some((_, literal, hidden)) => GraphPattern::Join {
            left: Box::new(rows),
            right: Box::new(GraphPattern::Values {
                variables: vec![hidden, literal],
                bindings: marked,
            }),
        },
        None => rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_are_lower_cased_runs_without_latin_accents() {
        let w: Vec<String> = words("Brussel-Zuid, Trasé 12B; Ålesund").collect();
        assert_eq!(w, ["brussel", "zuid", "trase", "12b", "ålesund"]);
    }

    /// What the rewrite reads the strings of a query variable from (#188): the patterns
    /// that bind it, not those joined to the search only through its results.
    #[test]
    fn query_strings_are_read_from_what_binds_them_only() {
        struct Ctx {
            index: Arc<TextIndex>,
            read: std::sync::Mutex<Vec<String>>,
        }
        impl SearchContext for Ctx {
            fn index(&self) -> Result<Arc<TextIndex>, Error> {
                Ok(Arc::clone(&self.index))
            }
            fn vectors(&self, _: &NamedNode) -> Result<Arc<crate::vector::VectorIndex>, Error> {
                Err(Error::Request("no vectors".into()))
            }
            fn properties(&self) -> Arc<[NamedNode]> {
                default_properties()
            }
            fn term_id(&self, _: TermRef<'_>) -> Option<TermId> {
                None
            }
            fn holds(&self, _: TermId, _: TermId, _: TermId) -> bool {
                false
            }
            fn table(&self, _: &[Variable], _: IdRows) -> Option<GraphPattern> {
                None
            }
            fn literal(&self, _: TermId) -> Option<Literal> {
                None
            }
            fn values(
                &self,
                pattern: &GraphPattern,
                _: &Variable,
                _: Option<&spargebra::algebra::QueryDataset>,
            ) -> Result<Vec<oxigraph::model::Term>, Error> {
                self.read.lock().unwrap().push(pattern.to_string());
                Ok(Vec::new())
            }
        }
        let store = tripletrail_store::Store::new();
        let ctx = Ctx {
            index: Arc::new(TextIndex::build_from(&Source::store(&store), &|| Ok(())).unwrap()),
            read: std::sync::Mutex::new(Vec::new()),
        };
        let query = spargebra::SparqlParser::new()
            .parse_query(
                "PREFIX ex: <http://e/> PREFIX tt: <http://w3id.org/tripletrail/fn#>
                 SELECT * WHERE {
                   ?doc ex:title ?q ; ex:by ?who . ?who ex:name ?n .
                   ?s tt:search (?q ?score ?lit) . ?s a ex:Station ; ex:near ?x .
                   ?x ex:code ?code . ?lit ex:lang ?lang
                   OPTIONAL { ?s ex:km ?km }
                 }",
            )
            .unwrap();
        rewrite(query, &ctx).unwrap();
        let read = ctx.read.lock().unwrap();
        assert_eq!(read.len(), 1);
        for wanted in ["ex:title", "ex:by", "ex:name"] {
            let iri = wanted.replace("ex:", "http://e/");
            assert!(read[0].contains(&iri), "{wanted}: {}", read[0]);
        }
        for unwanted in ["Station", "near", "code", "lang", "km"] {
            assert!(!read[0].contains(unwanted), "{unwanted}: {}", read[0]);
        }
    }

    /// A build, and a patch that builds again, ask `stop` as they go and end with its error
    /// (a deadline, #203); asked nothing, they index everything.
    #[test]
    fn a_build_stops_when_asked() {
        let store = tripletrail_store::Store::new();
        let mut nt = String::new();
        for i in 0..10_000 {
            nt += &format!(
                "<http://e/s{i}> <http://www.w3.org/2000/01/rdf-schema#label> \"label {i}\" .\n"
            );
        }
        store
            .bulk_load(oxigraph::io::RdfFormat::NTriples, nt.as_bytes())
            .unwrap();
        let source = Source::store(&store);
        let asked = std::cell::Cell::new(0);
        let count = || {
            asked.set(asked.get() + 1);
            Ok(())
        };
        let index = TextIndex::build_from(&source, &count).unwrap();
        assert_eq!(index.len(), 10_000);
        assert!(asked.get() >= 3, "{}", asked.get());
        let stop = || Err(Error::Request("stopped".into()));
        assert!(TextIndex::build_from(&source, &stop).is_err());
        // A patch of many changes builds again, and stops too.
        let mut patched = TextIndex::build_from(&source, &|| Ok(())).unwrap();
        let mut more = String::new();
        for i in 10_000..20_000 {
            more += &format!(
                "<http://e/s{i}> <http://www.w3.org/2000/01/rdf-schema#label> \"label {i}\" .\n"
            );
        }
        store
            .bulk_load(oxigraph::io::RdfFormat::NTriples, more.as_bytes())
            .unwrap();
        assert!(patched.update_from(&Source::store(&store), &stop).is_err());
    }
}
