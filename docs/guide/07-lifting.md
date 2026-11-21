# 7. Turn files into RDF

Most data does not start as RDF: it comes as CSV exports, JSON from an API, XML, spreadsheets
or shapefiles. `tripletrail lift` turns such files into triples, in two ways you can mix:

- **SPARQL Anything queries** (the [Facade-X](https://sparql-anything.cc) model): a SPARQL
  `CONSTRUCT` reads the file as if it were RDF. Queries written for SPARQL Anything run
  unchanged.
- **Mappings** in [RML](https://rml.io), R2RML or YARRRML, the W3C community's mapping
  languages.

## A CSV file with a query

Here are three more stations, as `stations.csv`:

<!-- test: file stations.csv -->
```text
code,name,lon,lat,platforms
BRG,Bruges,3.2170,51.1972,10
CRL,Charleroi-Central,4.4386,50.4045,8
KOR,Kortrijk,3.2644,50.8243,7
```

Inside `SERVICE <x-sparql-anything:…>` the file is a graph: each row is a node, and with
`csv.headers=true` each cell is a triple whose property is `xyz:` and the column's name. The
query below reads the rows, makes an IRI for each station with `fx:entity`, and builds the
same shape of data as the sample network. Save it as `lift/stations.rq`:

<!-- test: file lift/stations.rq -->
```sparql
PREFIX ex:   <https://example.org/rail#>
PREFIX fx:   <http://sparql.xyz/facade-x/ns/>
PREFIX geo:  <http://www.opengis.net/ont/geosparql#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
PREFIX xsd:  <http://www.w3.org/2001/XMLSchema#>
PREFIX xyz:  <http://sparql.xyz/facade-x/data/>
CONSTRUCT {
  ?station a ex:Station ; rdfs:label ?name ; ex:code ?code ; ex:platforms ?platforms ;
    geo:hasGeometry ?geometry .
  ?geometry geo:asWKT ?wkt .
}
WHERE {
  SERVICE <x-sparql-anything:location=stations.csv,csv.headers=true> {
    ?row xyz:code ?code ; xyz:name ?name ; xyz:lon ?lon ; xyz:lat ?lat ; xyz:platforms ?p .
  }
  BIND(fx:entity("https://example.org/rail#", ?code) AS ?station)
  BIND(fx:entity("https://example.org/rail#", ?code, "-geometry") AS ?geometry)
  BIND(xsd:integer(?p) AS ?platforms)
  BIND(STRDT(CONCAT("POINT(", ?lon, " ", ?lat, ")"), geo:wktLiteral) AS ?wkt)
}
```

Run it. `--output` collects the triples in one N-Triples file:

<!-- test: run -->
```sh
tripletrail lift lift/ --output new-stations.nt
grep Kortrijk new-stations.nt
```

<!-- test: output contains -->
```text
1 queries, 18 triples
<https://example.org/rail#KOR> <http://www.w3.org/2000/01/rdf-schema#label> "Kortrijk" .
```

CSV cells are text, so the query converts the platforms to a number and the coordinates to a
WKT literal. The same works for JSON (`xyz:` and the key), XML, YAML, Excel and OpenDocument
spreadsheets, HTML, Markdown, shapefiles, KML, GPX and archives: see
[sources and options](../facade-x.md#sources-and-options).

## Files inside an archive

Exports often arrive packed. A query can read a file from inside a zip or tar archive, also
one compressed with gzip (`.tar.gz`) or bzip2 (`.tar.bz2`), without unpacking it first:
`from-archive` names the archive and `location` the file in it. Pack the CSV file, and save
a second query as `packed/stations.rq`:

<!-- test: run -->
```sh
tar -cjf stations.tar.bz2 stations.csv
mkdir -p packed
```

<!-- test: file packed/stations.rq -->
```sparql
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
PREFIX fx:   <http://sparql.xyz/facade-x/ns/>
PREFIX xyz:  <http://sparql.xyz/facade-x/data/>
CONSTRUCT { ?station rdfs:label ?name }
WHERE {
  SERVICE <x-sparql-anything:location=stations.csv,from-archive=stations.tar.bz2,csv.headers=true> {
    ?row xyz:code ?code ; xyz:name ?name .
  }
  BIND(fx:entity("https://example.org/rail#", ?code) AS ?station)
}
```

<!-- test: run -->
```sh
tripletrail lift packed/ --output packed.nt
grep Charleroi packed.nt
```

<!-- test: output contains -->
```text
<https://example.org/rail#CRL> <http://www.w3.org/2000/01/rdf-schema#label> "Charleroi-Central" .
```

Without `location`, the archive itself is the source, and its root lists the names of the
entries, so a nested `SERVICE` can read each one; see
[sources and options](../facade-x.md#sources-and-options).

## Data a web page embeds

Web pages often carry their own RDF, as RDFa attributes or schema.org Microdata, for search
engines. `html.metadata=true` adds those triples to the page's source, next to its Facade-X
tree, so a `CONSTRUCT` can pick them out by their own predicates. Save a page as
`page/station.html`:

<!-- test: run -->
```sh
mkdir -p page
```

<!-- test: file page/station.html -->
```html
<!DOCTYPE html>
<html>
<body>
  <div itemscope itemtype="https://schema.org/TrainStation" itemid="https://example.org/rail#CRL">
    <h1 itemprop="name">Charleroi-Central</h1>
    <meta itemprop="identifier" content="CRL">
  </div>
  <p vocab="https://schema.org/" typeof="TrainStation" resource="https://example.org/rail#NAM">
    <span property="name">Namur</span>
  </p>
</body>
</html>
```

and a query that keeps each station's schema.org name, as `page/names.rq`:

<!-- test: file page/names.rq -->
```sparql
PREFIX rdfs:   <http://www.w3.org/2000/01/rdf-schema#>
PREFIX schema: <https://schema.org/>
CONSTRUCT { ?station rdfs:label ?name }
WHERE {
  SERVICE <x-sparql-anything:location=page/station.html,html.metadata=true> {
    ?station a schema:TrainStation ; schema:name ?name .
  }
}
```

<!-- test: run -->
```sh
tripletrail lift page/names.rq --output page.nt
sort page.nt
```

<!-- test: output contains -->
```text
<https://example.org/rail#CRL> <http://www.w3.org/2000/01/rdf-schema#label> "Charleroi-Central" .
<https://example.org/rail#NAM> <http://www.w3.org/2000/01/rdf-schema#label> "Namur" .
```

As in SPARQL Anything, every embedded literal is a plain string, without its language tag or
datatype. See [sources and options](../facade-x.md#html) for the rules.

## Links and comments in a spreadsheet

A spreadsheet cell can hold more than its value: a hyperlink, or a comment someone left on
it. `station-links.xlsx` in the sample data has a `website` column whose cells link to each
station's page, and a comment on one name. With `spreadsheet.composite-values=true` every cell
becomes a node of its own: `rdf:_1` is its value, `xyz:address` and `xyz:label` its link, and
`xyz:author` and `xyz:threadedComment` its comment. Save this as `links.rq`:

<!-- test: file links.rq -->
```sparql
PREFIX ex:   <https://example.org/rail#>
PREFIX fx:   <http://sparql.xyz/facade-x/ns/>
PREFIX rdf:  <http://www.w3.org/1999/02/22-rdf-syntax-ns#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
PREFIX xyz:  <http://sparql.xyz/facade-x/data/>
CONSTRUCT { ?station ex:website ?site ; rdfs:comment ?note . }
WHERE {
  SERVICE <x-sparql-anything:location=station-links.xlsx,spreadsheet.headers=true,spreadsheet.composite-values=true> {
    ?row xyz:code/rdf:_1 ?code ; xyz:website/xyz:address ?url .
    OPTIONAL { ?row xyz:name/xyz:threadedComment ?note }
  }
  BIND(fx:entity("https://example.org/rail#", ?code) AS ?station)
  BIND(IRI(?url) AS ?site)
}
```

<!-- test: run -->
```sh
tripletrail lift links.rq --output links.nt
grep BRG links.nt
```

<!-- test: output contains -->
```text
<https://example.org/rail#BRG> <http://www.w3.org/2000/01/rdf-schema#comment> "Also spelled Brugge" .
<https://example.org/rail#BRG> <https://example.org/rail#website> <https://example.org/stations/bruges> .
```

Links and comments are read from `.xlsx` and `.xlsm` files; other spreadsheet formats give each
cell's kind and value only. See [sources and options](../facade-x.md#sources-and-options).

## A JSON file with a mapping

Mappings say the same thing as data instead of a query. YARRRML is the shortest to write.
Two operators, as `operators.json`:

<!-- test: file operators.json -->
```json
{"operators": [
  {"id": "brabant", "name": "Brabant Rail", "serves": ["BXS", "BXC", "BXN", "LV"]},
  {"id": "scheldt", "name": "Scheldt Express", "serves": ["ANT", "MEC"]}
]}
```

The mapping takes each element of `operators` as a record (JSONPath), makes its subject from
`id`, and an `ex:serves` link per value of `serves`. Save it as `lift/operators.yarrrml`, next
to the query:

<!-- test: file lift/operators.yarrrml -->
```yaml
prefixes:
  ex: https://example.org/rail#
  rdfs: http://www.w3.org/2000/01/rdf-schema#
mappings:
  operator:
    sources:
      - [operators.json~jsonpath, "$.operators[*]"]
    s: ex:operator-$(id)
    po:
      - [a, ex:Operator]
      - [rdfs:label, $(name)]
      - [ex:serves, "ex:$(serves[*])~iri"]
```

`tripletrail lift` runs every `.rq` query and every `*.rml.ttl`, `*.r2rml.ttl` and `*.yarrrml`
mapping of the folder, in parallel:

<!-- test: run -->
```sh
tripletrail lift lift/ --output lifted.nt
grep -c serves lifted.nt
```

<!-- test: output contains -->
```text
2 queries, 28 triples
6
```

## From a mapping to a query

`--as-sparql` writes a mapping as the SPARQL Anything query that gives the same triples,
instead of running it. Use it to see what a mapping does, or to move a mapping to a query
you can extend:

<!-- test: run -->
```sh
tripletrail lift lift/operators.yarrrml --as-sparql --output operators.rq
tripletrail lift operators.rq --output operators.nt
grep -c serves operators.nt
```

<!-- test: output contains -->
```text
6
```

`operators.rq` reads `operators.json` in a `SERVICE <x-sparql-anything:>` and makes each IRI
with `IRI(CONCAT(…))`. CSV and JSON sources, classes, datatypes, languages and joins between
triples maps are written; a mapping with XML or database sources, functions or named graphs is
refused, naming what cannot be written. See [R2RML and RML](../rml.md#mappings-as-sparql).

## Load the result

The lifted triples are ordinary N-Triples. Load them with the network:

<!-- test: run -->
```sh
tripletrail load --location db --dataset rail network.ttl lifted.nt
```

or upload them to a running server with
`curl -X POST http://localhost:7878/rail/data -H 'Content-Type: application/n-triples' --data-binary @lifted.nt`.

## Import a file in the web UI

The server's **Import** page (`/$/ui/import`) does the same without a command line. Drop a
file on it: the page shows the file as Facade-X triples and drafts a `CONSTRUCT` that maps
every column or key. Edit the query, **Preview** the triples it makes, and **Add to the graph**
puts them in the named graph you give. **Download query** saves the query as a lift query that
reads the file by its name, to run later with `tripletrail lift`.

The file is kept in the server's memory for an hour, never on its disk, and the query can read
only that file. Only someone who may write to the dataset can import (with `--update-token`,
that token). The page uses the server's import API, which you can call yourself. Start a
server:

<!-- test: serve -->
```sh
tripletrail serve --dataset rail
```

Send the file. The answer has the upload's `id`, the file as triples (`tree`) and the drafted
`query`:

<!-- test: run -->
```sh
curl -s -X POST 'http://localhost:7878/rail/import?name=stations.csv' --data-binary @stations.csv > upload.json
id=$(sed -n 's/.*"id":"\([0-9a-f]*\)".*/\1/p' upload.json)
curl -s -X POST "http://localhost:7878/rail/import/$id/run?graph=urn:import:stations" \
  -H 'Content-Type: application/sparql-query' --data-binary @lift/stations.rq
```

<!-- test: output -->
```text
{"after":18,"before":0,"graph":"urn:import:stations"}
```

The query reads `location=stations.csv`, and the import runs it on the uploaded file of that
name: `upload:stations.csv` is the same file, and any other location is refused. `POST
/rail/import/$id/preview` answers the triples without writing them, and `DELETE
/rail/import/$id` forgets the file before the hour is up. See
[the import API](../server.md#importing-files).

## Useful options

- `--output-dir out/` writes one file per query or mapping instead of one file.
- `--as-sparql` writes each mapping as a query instead of running it.
- `--base DIR` resolves the files the queries name against `DIR`, and `--allow DIR` keeps
  them from reading anywhere else.
- `-v name=value` fills a parameter: `?_name` in a query becomes the string `"value"`, as in
  SPARQL Anything.
- `-j 8` runs eight queries at a time.
- Mappings can say things about triples (RML-star `rml:quotedTriplesMap`, for instance where
  a speed limit comes from). TripleTrail writes each quoted triple as an RDF 1.2 reifier,
  `_:r rdf:reifies <<( s p o )>>`: see [quoted triples](../rml.md#quoted-triples-rml-star).
- Mappings can read PostgreSQL tables too (`--database`): see [R2RML and RML](../rml.md).
  Without any mapping, `tripletrail direct-mapping --database postgresql://… --output db.nt`
  turns every table into RDF as the W3C Direct Mapping does, and `--mapping` writes that
  mapping out for you to edit: see [direct mapping](../rml.md#direct-mapping).
- Text exports of databases read with the matching `csv.format`, such as `MySQL`,
  `PostgreSQLText` or `Oracle` (with `\N` for NULL): see [Facade-X](../facade-x.md).

From Python, `Engine.run_mapping` runs a mapping straight into a store, with no N-Triples
file in between:

```python
from tripletrail import Engine, NamedNode

e = Engine()
made = e.run_mapping(path="lift/operators.yarrrml",
                     to_graph=NamedNode("https://example.org/graph/operators"))
```

See [the Python package](../../python/README.md#rml-mappings).

For a lift that is one step of a larger job (lift, load, update, validate, export), use a
[pipeline](08-pipelines.md).

## What you learnt

- `tripletrail lift FOLDER` runs SPARQL Anything queries and RML, R2RML and YARRRML mappings on
  files, and writes N-Triples.
- In a query, `SERVICE <x-sparql-anything:location=FILE>` reads a file as a graph, and
  `fx:entity` makes IRIs.
- `--as-sparql` turns a mapping into the query that gives its triples.
- `from-archive` reads a file inside an archive, and `html.metadata=true` the RDFa and
  Microdata of a web page.
- The web UI's Import page turns a dropped file into RDF with a drafted query, previews it and
  adds it to a graph.

[Facade-X](../facade-x.md) and [R2RML and RML](../rml.md) have every format, option and
function.

Next: [pipelines](08-pipelines.md).
