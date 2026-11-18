# Full-text search

TripleTrail keeps a full-text index of the labels, titles and descriptions in a dataset, and
answers two property functions from it: its own `tt:search` and Jena's `text:query`, so
queries written for Fuseki with `jena-text` keep working.

```sparql
PREFIX tt: <http://w3id.org/tripletrail/fn#>
SELECT ?s ?score ?label WHERE {
  ?s tt:search ("brussels central" ?score ?label 10)
} ORDER BY DESC(?score)
```

There is nothing to configure: the index is built on the first search (0.8 s for 560,000
labels), and the first search after a write adds and removes the labels that changed (about
40 ms on the same data). A search then takes milliseconds. Full-text search needs the
TripleTrail store, the default.

## What is indexed

The literals of `rdfs:label`, `rdfs:comment`, `skos:prefLabel`, `skos:altLabel`,
`dct:title`, `dct:description` and `dc:title`, in every graph, and those of the dataset's own
properties: `tripletrail serve --text-properties kg=https://example.org/rail#code` (once per
property, `*=IRI` for every dataset, including those created over the admin API later;
`TRIPLETRAIL_TEXT_PROPERTIES`), or `text-properties:` in the dataset's entry of the
[configuration file](server.md#configuration-file). A comma separates entries, so an IRI that
holds one is written with `%2C`; a name that is no dataset at start is reported on standard
error. `tripletrail query` and `tripletrail update` take `--text-properties IRI` for the dataset
they open. They play the part
of the fields of a jena-text `text:entityMap`: `text:query (ex:code "bxn")` searches one of
them, and `tt:search` searches them all. A `text:query` on a property outside the list answers
400. `Engine::set_text_properties` sets them from Rust.

Words are runs of letters and digits, lower-cased, with Latin accents taken off. `word*`
matches every word that starts with `word`. A literal matches if it has any word of the
query; more words and rarer words score higher, ranked with BM25.

## `tt:search`

`?s tt:search ("query" ?score ?literal limit)`, with everything after the query optional:

- `?s tt:search "query"` alone gives the matching resources;
- `?score` is the BM25 score (`xsd:double`);
- `?literal` is the literal that matched: there is one row per resource and literal;
- `limit` keeps the best matches only. With a constant subject, it counts that subject's
  matches.

The query string can also be a variable bound by the rest of the query, as Jena allows:
`VALUES ?q { "zuid" "noord" } ?s tt:search ?q`, a `BIND`, or a triple pattern of the same group
(or the left side of an `OPTIONAL` or `LATERAL`). Each distinct string is searched on its own,
with its own limit, and its rows have `?q` bound to it. A value that is not a literal searches
nothing; a variable that nothing before the search binds (only a later group, the other side
of a `UNION`, or nothing at all) is an error, as are more than 1000 distinct strings in one
call. The strings are read from the patterns that bind `?q` (and those linked to them), not
from patterns joined to the search only through its results, and before the group's `FILTER`s:
a filter keeps the rows it should, but does not lower that count. The same holds for `text:query (property ?q)`, each string read in Lucene's syntax.

## `text:query`

```sparql
PREFIX text: <http://jena.apache.org/text#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?s ?score WHERE { (?s ?score) text:query (rdfs:label "station*" 20) }
```

Jena's form, `(?s ?score ?literal) text:query (property "query" limit)`, with the property and
the limit optional and `?s` alone as subject allowed. A property must be one of the indexed
ones; then only its literals match, and the limit counts those. A limit of -1 is no limit.

The query string is read as Lucene reads it (#830):

- Words are OR'ed; `OR` and `||` say so.
- `+word`, and `AND` or `&&` between two words, make words required.
- `-word`, `!word` and `NOT` exclude them. A query of excluded words only matches nothing.
- `"several words"` is a phrase: the words one after the other, in that order. A phrase can
  be required or excluded too (`-"brussel zuid"`).
- A trailing `*` matches the words a word starts.

Other Lucene syntax (`field:value`, `?`, `~`, `^`, groups, ranges, escapes, `*` elsewhere than
at the end of a word, `*` in a phrase) is refused with an error rather than read differently.

Two of Jena's options can follow the query, in any order with the limit:

- `"lang:nl"` keeps the literals in that language; a tag also matches its subtags (`en`
  matches `en-GB`). In a language with a Snowball stemmer, the words then match their other
  forms too, as with Jena's multilingual index and Lucene's language analyzers: `"station"`
  with `lang:en` finds "Connecting stations"@en, and `"stasjoner"` with `lang:no` finds
  "Stasjonen"@no. The stemmers are those of Arabic (`ar`), Danish (`da`), Dutch (`nl`),
  English (`en`), Finnish (`fi`), French (`fr`), German (`de`), Greek (`el`), Hungarian
  (`hu`), Italian (`it`), Norwegian (`no`, `nb`, `nn`), Portuguese (`pt`), Romanian (`ro`),
  Russian (`ru`), Spanish (`es`), Swedish (`sv`), Tamil (`ta`) and Turkish (`tr`). A word
  with a trailing `*` is not stemmed. Without `lang:`, a word matches itself only. The index
  holds the stems of every literal tagged with one of these languages, beside its words.
- `"highlight:"` returns `?literal` with the matched words marked, as Jena does: `↦` before
  and `↤` after, and a literal longer than 128 characters cut to at most 3 fragments around
  its matches, joined with ` ...`. The options `s:` (start marker), `e:` (end marker), `z:`
  (fragments) and `f:` (fragment size) change those, separated by `|`:
  `"highlight:s:<b> | e:</b> | f:60"`. The marked literal keeps its language tag.

```sparql
PREFIX text: <http://jena.apache.org/text#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?s ?shown WHERE { (?s ?score ?shown) text:query ("\"brussel zuid\"" "lang:nl" "highlight:") }
```

Two intended differences from Jena: without a property, every indexed property is searched,
not only the index's default field (usually `rdfs:label`), so write `rdfs:label` for Jena's
usual answer; and scores are BM25 scores, which differ from Lucene's.

## Blazegraph's `bds:search` and Stardog's `textMatch`

Queries written for Blazegraph and Stardog run on the same index. Both match literals rather
than resources: the variable before them is the literal matched, with one row per literal (its
best score), and a triple such as `?s rdfs:label ?o` gives the resource. As with the others,
only the indexed properties' literals are searched.

Blazegraph's form is `?o bds:search "words"`, with its options as triples on `?o`:

| Option | |
|---|---|
| `bds:relevance ?score` | the BM25 score |
| `bds:rank ?rank` | the literal's place among the matches, from 1 |
| `bds:maxRank n` | the best `n` literals only |
| `bds:minRank n` | from the `n`th on |
| `bds:matchAllTerms true` | every word must match (by default any may) |
| `bds:prefixMatch true` | every word matches the words it starts (`word*` does so for one word) |
| `bds:searchTimeout` | accepted; the query's own timeout applies |

`bds:minRelevance` and `bds:maxRelevance` are refused: Blazegraph's relevance runs from 0 to 1,
a BM25 score does not. So are the other options (`bds:matchExact`, `bds:matchRegex`,
`bds:subjectSearch`), and an option on a variable that no `bds:search` searches with.

Stardog's form is `?literal <tag:stardog:api:property:textMatch> ("query" limit)`, or
`(?literal ?score) textMatch …`, the limit optional; the query string is read in Lucene's
syntax, as for `text:query`. A score threshold (`("query" 0.5 10)`) is refused, for the same
reason as `bds:minRelevance`. Stardog's `SERVICE fts:textMatch` form is not supported.

The query string of both can be a variable bound by the rest of the query, as above.

## In updates

Each of these works in the `WHERE` of an update too
(`INSERT { ?s ex:found true } WHERE { ?s tt:search "brussel" }`). The search reads the store as
it was before the update, so it must be in the update's first operation that changes
anything; an update whose later operation searches is refused, and its operations can be sent
as separate requests. An update by a caller limited to some graphs or rows can't search yet: it
answers 400.

## Access control

The search covers every graph, also in a query on the default graph, and it is not allowed
inside `GRAPH`, `SERVICE` and `FILTER (NOT) EXISTS`. A query with a dataset (the protocol's
`default-graph-uri` and `named-graph-uri`) searches the graphs of that dataset.

A caller whose [access](server.md#dataset-access-with-policies) is limited to some graphs or
rows searches what it can read (#203): the labels of its readable graphs, less those its row
rules hide (a hidden resource, a property hidden on it or everywhere). It searches an index of
those labels alone, so the matches, their scores (document count, word frequencies and average
length are those of the view) and the limits are what a store holding only that data would
give: no hit and no score tells anything about hidden data. Such an index is made on the first
search of a view (as long as building the full index on that much data) and brought up to date
after writes like the full one; eight views are kept. Searches of one view wait for its one
build or patch and then share it; searches of other views do not wait. Making one stops with
the query's timeout. The map search
(`/{ds}/tiles/search`) reads it too.
