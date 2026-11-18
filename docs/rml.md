# R2RML and RML mappings

`tripletrail lift` runs R2RML, RML and YARRRML mapping documents next to Facade-X queries:
files named `*.rml.ttl`, `*.r2rml.ttl` or `*.yarrrml` in the folder it is given. A pipeline's
`lift` step runs them the same way, and in Python `Engine.run_mapping` runs one straight
into a store (see [the Python package](../python/README.md#rml-mappings)).

```sh
tripletrail lift mappings/ --output all.nt            # every file's triples in one file
tripletrail lift mappings/ --output-dir out/          # one .nt file per mapping, under out/
```

The vocabularies of R2RML (`http://www.w3.org/ns/r2rml#`), legacy RML
(`http://semweb.mmlab.be/ns/rml#`, as RMLMapper uses) and the RML of the W3C KG-Construct
group (`http://w3id.org/rml/`) can be mixed in one document. Of the KG-Construct test cases,
TripleTrail passes RML-Core 75 of 76, RML-IO 70 of 73, RML-LV 41 of 41, RML-FNML 22 of 22 and
RML-CC 35 of 35 and RML-star 10 of 18 (8 have a broken data file; see [RML-star](#quoted-triples-rml-star)) (run by `crates/tripletrail-rml/tests/conformance.rs` against checkouts of the test
cases; they are not on the [conformance](conformance.md) page yet).

## Direct mapping

`tripletrail direct-mapping` gives a graph of a PostgreSQL database without a mapping document,
as the W3C [Direct Mapping](https://www.w3.org/TR/rdb-direct-mapping/) defines it:

```sh
tripletrail direct-mapping --database postgresql://reader@db/assets --base-iri https://data.example.org/ \
    --output assets.nt
tripletrail direct-mapping --database … --mapping --output assets.r2rml.ttl   # the mapping, to edit
```

Every table and view of `--schema` (`public` by default), or the `--table`s named, is mapped.
With `B` the base IRI (`http://example.com/base/` by default) and names percent-encoded:

- a row is `<B Table/key1=value1;key2=value2>` from its primary key, or a blank node without
  one (one per row, alike rows too), with `rdf:type <B Table>`;
- a column with a value gives `<B Table#column>` and the value's natural literal, as R2RML
  writes it (`xsd:integer`, `xsd:decimal`, `xsd:date`, `geo:wktLiteral` for PostGIS, …);
  NULLs give nothing;
- a foreign key gives `<B Table#ref-column1;column2>` to the row it references, unless one of
  its columns is NULL. Keys to another schema's tables are left out.

The command writes an R2RML mapping that does this and runs it as `tripletrail lift` would, so
`--mapping` gives a starting point to rename classes and properties and run with `lift`.

## Sources (RML-IO)

A logical source names a file, relative to the base directory (`--base`, else the current
directory), and is read under the same rules as Facade-X locations: only below the allowed
directories (`--allow`) when they are given. A source may also be an `http://` or
`https://` URL, read only when `--allow-url` allows it (see
[remote locations](facade-x.md#remote-locations)).

| Reference formulation | Iterator | References |
|---|---|---|
| CSV, TSV | none: one record per row | column names from the header row; a column the header lacks is an error |
| CSV, over a spreadsheet (`.xlsx`, `.xlsm`, `.xlsb`, `.xls`, `.ods`) | the sheet's name; by default the first sheet | column names from the sheet's first non-empty row |
| JSONPath | JSONPath, such as `$.stations[*]` | JSONPath relative to the record |
| XPath | XPath, such as `//station` | XPath relative to the record (`/`, `//`, names with their prefixes, `@attr`, `text()`, `*`, `.`, `..`, simple predicates) |

A source may also be written in its RML-IO form, with:

- `rml:compression`: `rml:gzip`, `rml:zip`, `rml:targz` or `rml:tarxz`.
- `rml:encoding`: UTF-8 or UTF-16.
- `rml:null`: the values that count as no value.

Without `rml:null`, an empty CSV cell is no value in legacy RML (as in RMLMapper) and an empty
string in the KG-Construct vocabulary, which also refuses a row with more or fewer cells than
the header.

A spreadsheet is read as RMLMapper and Morph-KGC read one, with the CSV reference
formulation (also chosen by its extension when the source names none). Every non-empty row
after the header is a record, and an empty cell is no value. Cells are text: numbers as Excel
shows them by default (`10`, not `10.0`), booleans `true` and `false`, dates as
`xsd:dateTime` text without a time zone (`2020-01-31T12:00:00`), times without a date as
`xsd:time` text (`13:05:00`), and formula cells as the value the file holds for them. Error
cells are empty, and a row of only empty and error cells is skipped, so it is never the
header. A workbook may hold at most 1,000,000 non-empty cells and
unpack to at most 1 GiB, as Facade-X's default spreadsheet limits.

Tables and SQL queries (R2RML logical tables, or RML sources on a `d2rq:Database`) are read
from PostgreSQL: from the source's `d2rq:Database`, or, for R2RML, from `--database`.
PostGIS `geometry` and `geography` columns are `geo:wktLiteral`s with their SRID's CRS IRI
first (none for SRID 0 and 4326, which PostGIS stores longitude first, so CRS84).

## Logical targets (RML-IO)

A term map with `rml:logicalTarget` sends the quads it makes to that target's file instead of
the default output. A target names its file (`rml:path` of an `rml:FilePath`), its
serialization (N-Quads, N-Triples, Turtle, TriG, N3, RDF/XML, JSON-LD or RDF/JSON), its
encoding (UTF-8 or UTF-16) and its compression (none, gzip, zip, tar.gz or tar.xz).

```turtle
<#Names> rml:target [ a rml:FilePath ; rml:path "names/names.nt.gz" ; rml:compression rml:gzip ] ;
  rml:serialization formats:N-Triples .
```

- Paths are relative to `--output-dir`, else to the base directory, and may not leave it: an
  absolute path, one with `..`, or a URL is an error.
- Without `--output-dir`, target files land next to your sources. A file there that this
  run did not write is refused, not overwritten: remove it first, give `--output-dir`, or
  give `--overwrite-targets` (for a rerun). A pipeline's `lift` step always overwrites its
  targets, since pipelines are rerun.
  Within one run, a later mapping naming the same target replaces it.
- A target may never be an input of the run: a query or mapping file of the run, a file
  any of the run's mappings reads, or a file named like a query, mapping or pipeline file (`.rq`, `.sparql`, `.ru`,
  `.rml.ttl`, `.yarrrml`, `.yaml`). Such a target is an error, with or without
  `--overwrite-targets`.
- Target files are written only after the mapping's default output succeeds, so a mapping
  that fails (for instance one that puts quads in a named graph, which `lift`'s N-Triples
  output cannot hold) leaves no target files behind.
- A mapping with logical targets holds all its quads in memory until they are written. A
  mapping without any streams its triples to the default output, which drops duplicates
  in bounded memory (spilling to disk when large), so send very large outputs there.
- The triple count `lift` reports includes the quads written to targets.

## Functions (RML-FNML)

A term map may give the value of a function, with `rml:functionExecution` (inputs by
`rml:inputValueMap` or `rml:inputValue`, output by `rml:return`) or legacy
`fnml:functionValue`, and may hold an `rml:condition` that must be true for it to make terms.
The functions are built in: common GREL string functions, IDLab's comparison and null
functions, and TripleTrail's geometry functions in `http://w3id.org/tripletrail/fx#`: `point`
(`x`, `y`, optional `z` and `crs`) and `wkt` (`wkt`, `crs`), which make `geo:wktLiteral`s. An
unknown function, parameter or output is a mapping error.

## Collections and containers (RML-CC)

Gather maps make `rdf:List`, `rdf:Bag`, `rdf:Seq` and `rdf:Alt` from term maps, referencing
object maps and nested gather maps, by appending or by Cartesian product.

A gather map with a value of its own (a constant, template or reference) names its
collection, and the members it gathers for that name are joined across the records **of one
triples map**, per graph. Two triples maps that name the same collection each make their own
members for it; to gather across sources, gather in one triples map, for instance over a
logical view (RML-LV) that joins them.

## Quoted triples (RML-star)

A subject or object map with `rml:quotedTriplesMap` makes, instead of a term, the triples of
another triples map: from the same record without `rml:joinCondition` (both maps must then
read the same logical source), else from the records of that map whose `rml:parent` values
equal this record's `rml:child` values, as for a referencing object map. An
`rml:NonAssertedTriplesMap` makes triples only for others to quote; its own are not written.

TripleTrail stores RDF 1.2, where a triple term can only be an object, so a quoted triple is
written as RDF 1.2 Turtle reads `<< s p o >>`: a **reifier**, a blank node that stands for
the triple, with `_:r rdf:reifies <<( s p o )>>` written next to each triple using it (in the
same graph). Unlike Turtle, which makes a new reifier at each `<< … >>`, the reifier is given
by the triple, so every quote of the same triple names the same blank node:

```turtle
<#Speed> a rml:NonAssertedTriplesMap ;
  rml:logicalSource [ rml:source [ a rml:RelativePathSource ; rml:path "speeds.csv" ] ;
                      rml:referenceFormulation rml:CSV ] ;
  rml:subjectMap [ rml:template "http://example.org/{track}" ] ;
  rml:predicateObjectMap [ rml:predicate ex:speed ; rml:objectMap [ rml:reference "speed" ] ] .
<#Source> rml:logicalSource [ rml:source [ a rml:RelativePathSource ; rml:path "speeds.csv" ] ;
                      rml:referenceFormulation rml:CSV ] ;
  rml:subjectMap [ rml:quotedTriplesMap <#Speed> ] ;
  rml:predicateObjectMap [ rml:predicate ex:source ; rml:objectMap [ rml:reference "source" ] ] .
```

gives, per row, `_:r rdf:reifies <<( <http://example.org/T1> ex:speed "120" )>>` and
`_:r ex:source "survey"`, without the speed triple itself. Quoted maps may quote quoted maps;
a map that ends up quoting itself is a mapping error. A referencing object map whose parent
has a quoted subject, and quoted maps inside gather maps, are refused, as is `--as-sparql`
for any quoted map.

Of the 18 RML-star test cases, 10 pass as published; the other 8 (`RMLSTARTC001b` to
`RMLSTARTC008b`) have a `data2.csv` whose first line is Markdown (`**Input 1**`), and pass
once it is removed. Their expected outputs are compared as RDF 1.2 Turtle, with the reifiers
of the same triple merged.

## Mappings as SPARQL

`tripletrail lift --as-sparql` writes a mapping as one Facade-X CONSTRUCT query that gives the
same triples, instead of running it, so it can be read, extended, or run by SPARQL Anything
users:

```sh
tripletrail lift stations.rml.ttl --as-sparql                       # to standard output
tripletrail lift stations.rml.ttl --as-sparql --output stations.rq  # one mapping
tripletrail lift mappings/ --as-sparql --output-dir queries/        # a .rq file per mapping
```

Each triples map becomes a `SERVICE <x-sparql-anything:>` over its source (options as
`fxe:properties`, rows and records as `fx:anySlot` steps from the `fx:Root`), each reference
an `OPTIONAL` pattern on the record, each term map a `BIND` (`IRI(CONCAT(…))`, `STRDT`,
`STRLANG`, `fx:bnode`), and the triples maps are joined with `UNION`. A referencing object
map with join conditions reads the parent's source in an `OPTIONAL` joined on the
conditions' values as text; one without conditions makes the parent's subject from the same
record.

What is written: CSV and TSV files (an empty cell is no value in legacy RML, as when the
mapping runs; one `rml:null` value); JSON files whose iterator and references are paths of
keys, `[*]`, `.*` and `[n]`; constant, reference, template and blank node term maps;
classes; datatypes and language tags, constant or per record (`rml:datatypeMap`,
`rml:languageMap`); and joins. Values in IRI templates are encoded with `tt:encodeForIri`
(R2RML's IRI-safe form, keeping letters outside ASCII), and with `ENCODE_FOR_URI` for
`rml:URI` maps, so the query runs in TripleTrail; with SPARQL Anything, replace
`tt:encodeForIri` by `ENCODE_FOR_URI` (equal for ASCII values). Everything else (XML,
database and spreadsheet sources, compressed or UTF-16 files, functions, collections and
containers, logical views, logical targets, conditions and named graph maps) is refused,
naming what cannot be written.

Where the query and the mapping differ: a value that is not a valid IRI leaves its term out
of the query's triples, where the mapping fails; and a JSON number keeps the digits written
in the file, where the mapping writes a double as Rust does (`1e3` as `1000.0`). Of the
RML-Core and RML-IO test cases, the 67 that can be written give the same triples as their
mappings.

## Memory

`lift` reads each source into memory once, whole, for all the triples maps that use it.
A CSV file is held as its rows' text, about 100 bytes a row plus the cells; JSON, XML and
spreadsheet sources as parsed documents, which take several times their size. A mapping's
triples are not held (see logical targets above for the exception). On a 4-core machine a
CSV file of 21.5 million rows (1 GB) lifts as the [RML benchmark](https://github.com/Matdata-eu/tripletrail-bench/tree/main/bench/rml)
shows.

## Limits

These keep a mistaken or hostile mapping from exhausting memory or the stack; each is a
mapping error, not a crash:

- A term map may not be defined in terms of itself, and term maps nest at most 64 deep.
- A mapping uses at most 100,000 term maps, counting each use of a shared one.
- A gather list that never ends (`rdf:rest` back to its start) is refused.
- A Cartesian gather makes at most 100,000 collections for one record.
