# Facade-X: querying files with SPARQL

`tripletrail-fx` reads files as RDF inside a SPARQL query, following the
[Facade-X](https://w3c-facade-x.github.io/facade-x-specs/) model, so that queries written for
SPARQL Anything run unchanged (issue #156). It is a clean-room implementation: the option
names, the vocabulary and the shape of the generated graph follow the published
documentation.

```sparql
PREFIX xyz: <http://sparql.xyz/facade-x/data/>
PREFIX fx:  <http://sparql.xyz/facade-x/ns/>
PREFIX fxe: <http://sparql.xyz/facade-x/engine/>
CONSTRUCT { ?station <http://ex.org/name> ?name }
WHERE {
  SERVICE <x-sparql-anything:location=stations.json> { ?s xyz:code ?code ; xyz:name ?name }
  BIND(fx:entity("http://ex.org/station/", ?code) AS ?station)
}
```

## Turning it on

`SERVICE <x-sparql-anything:…>` calls read files, so they are off unless something turns
them on:

- [`tripletrail lift`](#tripletrail-lift) runs Facade-X queries from the command line, and a
  pipeline's `lift` step runs them the same way (see [Pipelines](server.md#pipelines)). This
  is the usual way to use them.
- In the library, `Engine::with_fx(FxConfig { base, allowed, remote })`.

The SPARQL endpoints of `tripletrail serve` never read files or URLs for a query. With
`--fx-connection` they read the PostgreSQL databases the administrator names, and inline
`content=` (see [Live database tables](server.md#live-database-tables)); without it,
Facade-X calls are refused.

Relative locations resolve against `base` (`--base` for `tripletrail lift`). With `allowed` set
(`--allow`), only files under those directories can be read (symbolic links are resolved
first). URLs are read only under the prefixes `remote` allows (`--allow-url`, see
[Remote locations](#remote-locations)). The `fx:` functions work in every engine.

## How a call runs

1. The options come from the SERVICE IRI (`x-sparql-anything:name=value,…`, `\,` and `\=`
   escape; a bare `x-sparql-anything:data.json` is a location) and from option triples in
   the pattern (`fxe:properties fxe:name "value"`, or a variable bound by a `BIND` inside the
   SERVICE). An `fxe:` triple wins over a deprecated `fx:` triple, which wins over the IRI.
   An option triple may also take a variable bound outside the SERVICE, as SPARQL Anything
   allows (`?row xyz:xml ?x OPTIONAL { SERVICE <x-sparql-anything:> { fx:properties
   fx:content ?x . … } }`): the query is rewritten so that the SERVICE runs once per row, with
   the row's value. The join or OPTIONAL that binds the variable becomes a `LATERAL`, and
   the value reaches the SERVICE percent-encoded in its IRI.
2. The pattern is rewritten: option triples are taken out and `?s fx:anySlot ?o` becomes a
   triple with a hidden predicate variable filtered to `rdf:_n`. `fx:root`, the root's type
   before SPARQL Anything 1.0, becomes `fx:Root`, as SPARQL Anything rewrites it.
3. The source's Facade-X graph is built in memory, with only the triples whose predicate the
   pattern can match (all of them when a predicate is a variable, a negated property path or
   inside `EXISTS`). The triples go in the default graph, and also in the named graph of the
   root when the pattern uses `GRAPH`.
4. The pattern is evaluated on that graph with the same functions as the outer query, and
   its rows join the rest of the query. In the engine, the graph is bulk loaded into a
   TripleTrail store of its own and the pattern gets the engine's planner (`FxService::new`
   alone uses an in-memory Oxigraph dataset, about three times slower to fill).

## Sources and options

| Format | Chosen by | Graph |
|---|---|---|
| JSON | `.json`, `.geojson`, `application/json` | objects and arrays are containers; members are `xyz:<key>`, items `rdf:_n`; strings `xsd:string`, numbers `xsd:decimal` (`xsd:double` with an exponent) with the numeral as written, as in SPARQL Anything, booleans `xsd:boolean`; nulls give no triple (`xyz:null` with `json.include-null-values=true`) |
| CSV | `.csv`, `.tsv`, `.tab`, `text/csv`, `text/tab-separated-values` | one container per data row (`rdf:_1`, `rdf:_2`, ...); a cell is `xyz:<header>` with `csv.headers=true`, else `rdf:_<column>`; cells are `xsd:string`, empty cells `""` |
| XML | `.xml`, `application/xml`, `text/xml` | without `xml.path`, the root element is the root; elements are containers typed by name, attributes are keys, child elements and text are slots in document order (see below) |
| YAML | `.yaml`, `.yml`, `application/yaml`, `text/yaml` | as JSON: mappings and sequences are containers, entries `xyz:<key>`, items `rdf:_n`; integers `xsd:int` (`xsd:long` past 32 bits), floats `xsd:double`, booleans `xsd:boolean`, other scalars `xsd:string`; nulls give no triple |
| Spreadsheet | `.xlsx`, `.xlsm`, `.xlsb`, `.xls`, `.ods`, the Excel and OpenDocument media types | one graph per sheet, `<root><sheet name>`, with its own `fx:Root` and one container per row; a cell is `xyz:<header>` with `spreadsheet.headers=true`, else `rdf:_<n>`; text `xsd:string`, numbers `xsd:double`, booleans `xsd:boolean`, dates `xsd:dateTime` text in UTC |
| Text | `.txt`, `text/plain`, `content` without a media type | the root has `rdf:_1` with the whole text |
| HTML | `.html`, `.htm`, `text/html` | parsed as browsers parse it; the element `html.selector` picks (`:root` by default) is the root, or the root holds the elements it picks as slots; each element is a container typed `xhtml:<tag>`, with its attributes as `xhtml:<name>`, its children (elements, text, comments) as slots, and `whatwg:innerHTML` and `whatwg:innerText` (see below) |
| Markdown | `.md`, `.markdown`, `text/markdown`, `text/x-markdown` | CommonMark with tables and YAML front matter: the root is also `xyz:Document`; each block and inline is a container typed `xyz:<CommonMark node>` (`xyz:Heading` with `xyz:level`, `xyz:Link` with `xyz:destination`, `xyz:TableCell` with `xyz:alignment`, ...) holding text and children in slots |
| Shapefile | `.shp` (with its `.shx` and `.dbf`) | TripleTrail's own: one container per record (`<root>#feature<n>`), attributes as keys typed by their dBase type, and `geo:hasGeometry` to `<feature>/geometry` with `geo:asWKT`; set `geo.crs` for its CRS |
| KML | `.kml`, `application/vnd.google-earth.kml+xml` | TripleTrail's own: one feature per `Placemark`, with `name`, `description` and its `ExtendedData` as keys, and its geometry as for Shapefile |
| GPX | `.gpx`, `application/gpx+xml` | TripleTrail's own: one feature per waypoint, route and track, its simple child elements as keys, and its geometry as for Shapefile |
| Archive | `.zip`, `.tar`, `.tar.gz`, `.tgz`, `application/zip`, `application/x-tar`, `application/x-compressed-tar` | the root's slots are the names of the entries, directories included, in archive order |
| Folder | a `location` that is a directory | the root's slots are the `file:` URLs of the directory and of every file and directory below it |

### HTML

HTML follows SPARQL Anything's HTML triplifier, which is built on Jsoup. `xhtml:` is
`http://www.w3.org/1999/xhtml#` and `whatwg:` is `https://html.spec.whatwg.org/#`.

- **Names.** A prefixed tag or attribute name (`og:title`) takes the namespace of an `xmlns:og`
  attribute on the element or above it. Without one, the whole name is put in `xhtml:`,
  URL-encoded (`xhtml:og%3Atitle`).
- **Text values.** Text slots, `innerHTML` and `innerText` are written as Jsoup 1.23 writes
  them:
  - `innerHTML` is pretty-printed with a one-space indent;
  - in text, runs of whitespace are collapsed, and `&`, `<`, `>` and the no-break space are
    escaped;
  - in `innerText`, whitespace is collapsed and there is a space between blocks.
- **IRIs.** With `blank-nodes=false`, the containers are IRIs that follow the element's path
  under the root, such as `<root>/html/body/div.note/p:2`. The `:<n>` suffix is added when the
  parent has more than one child with that tag and those classes.
- **Selectors.** `html.selector` supports this subset of CSS:
  - type selectors and `*`;
  - `#id`, `.class`, and `[a]`, `[a=v]`, `[a^=v]`, `[a$=v]`, `[a*=v]`, `[a~=v]`;
  - `:root`, `:first-child`, `:last-child`, `:only-child` and `:nth-child(n)`;
  - the combinators ` `, `>`, `+` and `~`;
  - comma lists.

  Anything else is refused. Inside a `SERVICE` IRI, write a comma as `\,`, or set the selector
  through `fx:properties`.
- **`html.parser=xml`** reads the file as XML: names keep their case and prefix, nothing is
  added (no `html`, `head` or `body`), CDATA is text, and `innerHTML` is written as Jsoup's XML
  mode writes it, not indented, with an empty element as `<a></a>`.
- **Not supported.** `html.metadata` (RDFa and Microdata through Any23) and `html.browser` (a
  headless browser) are refused.
- **Deviations from SPARQL Anything.**
  - The document is parsed by html5ever. html5ever cannot tell `a` from `a=""`, so an empty
    attribute is written as `a` in `innerHTML`.
  - An empty SVG or MathML element is written self-closing (`<path />`).
  - A `|` from a prefixed tag is percent-encoded in IRIs, where SPARQL Anything writes it
    as is.
  - The tree is built with `markup5ever_rcdom`, which html5ever publishes for its own tests
    and calls unofficial. It is only used to copy the tree once, so it can later be replaced by
    TripleTrail's own tree builder.
- **Limits.** A tag may have at most 10,000 attributes. The check before parsing is rough:
  a `<` followed by more than 10,000 words with no `>`, even in a script or a comment, is
  refused too. Elements may nest at most
  512 deep. The `innerHTML` and `innerText` of all
  elements may hold at most 256 MiB, since each element repeats the markup of everything
  inside it. For large pages, select fewer elements, or query without those two predicates:
  they are only computed when the query can match them.

### Remote locations

A `location` may be an `http://` or `https://` URL. Reading URLs is off unless the engine
is given URL prefixes to allow (`FxConfig::remote`; `tripletrail lift --allow-url
https://data.example.org/`, and the same option of `tripletrail pipeline run` for its lift
steps; workspace pipelines on a server read no URLs), as for `SERVICE` to remote endpoints: a query on a server must
not reach hosts inside its network. A prefix allows URLs with the same scheme, host and port
whose path starts with its path on a `/` boundary; URLs with user info or `.` and `..`
segments are never allowed. Redirects are followed, at most 5, and only to URLs on the
list. One read takes at most 30 seconds (`--url-timeout`) and 256 MiB (`--url-max-bytes`).

The format comes from the URL's extension, else from the media type the server sends
(`Content-Type`), unless `media-type` or `triplifier` names it. SPARQL Anything's HTTP
options are read: `http.method` (`GET` or `POST`), `http.payload` (the body of a POST),
`http.header.<name>`, `http.query.<name>` (added to the URL's query, percent-encoded),
`http.auth.user` and `http.auth.password` (HTTP Basic), and `http.redirect` (default
`true`). The headers and credentials are sent only to the URL's own scheme, host and port:
after a redirect to another, even an allowed one, they are dropped, as curl and browsers
do. The `http.query.*` parameters are not added again to a redirect's URL. A `Host`
header, and a payload without `http.method=POST`, are errors. An archive (`from-archive`) may be a URL too; a Shapefile may not, since its
`.shx` and `.dbf` are separate files. RML sources that name a URL are read under the same allowlist; the lift steps of a workspace
pipeline on a server read local files only.

### Databases

A source may be a PostgreSQL table or query instead of a file (a TripleTrail extension; SPARQL
Anything reads no databases): `table=<name>` (`station`, `rail.station`, or quoted parts such
as `"Station"`) or `sql=<query>` (one `SELECT`, `VALUES` or `TABLE` statement). The query
names no database itself: it reads from the database `tripletrail lift --database <url>` gives,
or from a named connection, `database=<name>`, that `--connection <name>=<url>` gives
(repeatable). A query cannot reach any other database. Pipeline lift steps read from
`TRIPLETRAIL_DATABASE`.

```sparql
PREFIX xyz: <http://sparql.xyz/facade-x/data/>
PREFIX fxe: <http://sparql.xyz/facade-x/engine/>
CONSTRUCT { ?st <http://ex.org/km> ?km } WHERE {
  SERVICE <x-sparql-anything:> {
    fxe:properties fxe:sql "SELECT code, km FROM station WHERE line = 'Dovrebanen'" ;
                   fxe:database "rail" .
    ?row xyz:code ?code ; xyz:km ?km }
  BIND(IRI(CONCAT("http://ex.org/station/", ?code)) AS ?st) }
```

A query with spaces or commas goes in `fxe:properties`, since a SERVICE IRI may not hold
spaces and its commas separate options.

The root holds a container per row (`rdf:_1`, `rdf:_2`, ...), named `<root>#row1`, ... with
`blank-nodes=false`, and each row a key per column. Values are the natural literals of R2RML,
as for RML database sources: integers `xsd:integer`, `numeric` `xsd:decimal`, `real` and
`double precision` `xsd:double`, `boolean`, `date`, `time`, `timestamp` `xsd:dateTime`,
`bytea` `xsd:hexBinary`, and other types strings. A NULL gives no triple. PostGIS `geometry`
and `geography` values are `geo:wktLiteral`s with their SRID's CRS IRI first (none for SRID
0 and 4326, which PostGIS stores longitude first, so CRS84; SRID 4258 written latitude
first, as TripleTrail reads that CRS); curved types stay hex EWKB
strings. `geo.wkt` or `geo.x`/`geo.y` add `geo:asWKT` from text or number columns, as for CSV.
Without `root`, the root is an IRI made from the connection name and the query.

The query runs read-only (a statement that writes fails), as one statement, through a
cursor, and fails once it gives more than `database.max-rows` rows (1,000,000 by default).
Only the columns the SERVICE pattern can match are read (all of them with `geo.*` options, or
when the result names a column twice). When the pattern is one group of triples about one
row with FILTERs comparing values to constants (`?r xyz:km ?km FILTER(?km > 10)`), the
comparisons PostgreSQL makes as SPARQL does are passed on as a `WHERE`: `=`, `<`, `<=`, `>`
and `>=` of integer and numeric columns with integers and decimals, of `double precision`
columns with numbers, and `=` of text and boolean columns (text as a query parameter).
`geof:sfIntersects`, `sfWithin`, `sfContains`, `sfOverlaps`, `sfTouches`, `sfCrosses` and
`sfEquals` of a `geometry` column with a WKT constant pass on a bounding-box overlap (`&&`)
for the rows whose SRID is the constant's CRS; rows with another SRID all come back. When
the column comes straight from a table and is declared with its SRID (`geometry(Point,
4326)`), the test is a plain `&&` that a GiST index on the column serves. The
FILTERs still run on what comes back. Nothing is passed on when the group holds other triples (such as `fxe:properties`), or with `blank-nodes=false`,
`null-string` or `trim-strings`, which number or change the values. Every SERVICE call runs
its query again.

A LIMIT on such a pattern becomes SQL `LIMIT`: a LIMIT (and OFFSET) inside the SERVICE, or
on a query that is just the SERVICE (`SELECT … WHERE { SERVICE <…> { … } } LIMIT 10`). Each
row gives at most one answer, so OFFSET + LIMIT rows are read; when FILTERs PostgreSQL does
not check leave too few answers, the call is made again with twice the rows, until there
are enough or the rows run out. At `database.max-rows` the query runs without a limit, so a
larger result fails as it would without one. SQL `LIMIT` without `ORDER BY` reads any rows,
as SPARQL LIMIT without ORDER BY gives any answers; with an ORDER BY, every row is read.

When such a SERVICE is joined to the rest of its group (or is an OPTIONAL's only pattern) on
a column's variable, it is a bind join, as for remote endpoints: the rest is evaluated
first, and the SERVICE is called once per 1,000 of its distinct values (at most 10,000; with
more, or with an unbound or blank value, it runs as written), each call with
`WHERE col IN (v1, v2, …)`. Values the column can never hold (an integer for a text
column or out of its range, an IRI) are left out of the list, and a list that cannot be checked by PostgreSQL (a
string for a `numeric` column, which may be `NaN`) reads every row. The answers are those of
the join.

`tripletrail lift --explain` (and `Engine::explain`) shows each database query a SERVICE call
made, after the row count: `"databases":[{"connection":null,"sql":"SELECT … WHERE \"code\"
IN ($1, $2)","params":["LLS","XYZ"],"rows":1,"ms":2.1}]`, with bind joins batched as the
query runs them.

CSV options, as SPARQL Anything: `csv.headers` (default false), `csv.headers-row` (the row
of the headers, default 1; the rows before it are data rows too), `csv.headers.sanitize`
(`true` replaces each run of characters other than letters and digits in header names with
`_`, any other value with that value), `csv.delimiter` (one character; default that of
`csv.format`, else tab for `.tsv`, `.tab` and `text/tab-separated-values`, else comma),
`csv.quote-char` (`"` by default; `false` or empty for no quoting), `csv.format` (`Default`,
`RFC4180`, `Excel`, `TDF`, `PostgreSQLCsv`, `InformixUnloadCsv`, `MySQL`, `PostgreSQLText`,
`Oracle`, `InformixUnload`), `csv.null-string` (cells
equal to it give no triple) and `csv.ignore-columns-with-no-header`. Header names are trimmed;
a repeated name gets `_1` appended (then `_2`, ...). With `blank-nodes=false` the rows are
`<root>#row1`, `<root>#row2`, ... Blank lines are skipped in every format (Commons CSV keeps
them as rows in `RFC4180` and `Excel`). The formats with a backslash escape read as Apache
Commons CSV does: `MySQL` (tab, no quoting, `\N` is null), `PostgreSQLText` (tab, `\N` is
null), `Oracle` (comma, cells trimmed, `\N` is null) and `InformixUnload` (`|`). `\t`, `\n`,
`\r`, `\b` and `\f` are those characters, an escaped delimiter, quote, backslash or line break
is that character (so a cell can hold a line break), and any other escaped character keeps
its backslash.

YAML, as SPARQL Anything: every document of a stream fills the root (a mapping gives it
keys, a sequence slots, a scalar `rdf:_1`). Plain scalars follow the YAML 1.2 core schema;
quoted and block scalars and `!!str` are strings; aliases repeat their anchor's node. With
`blank-nodes=false` containers are `<root>/<key>` and `<root>/<key>/<n>`. A key given twice
in a mapping is an error unless `yaml.allow-duplicate-keys=true`, which keeps the last value.
Merge keys (`<<: *base`, from YAML 1.1) are not merged: `<<` is a key like any other, as in
SPARQL Anything. Infinities and NaN are `"INF"`, `"-INF"` and `"NaN"` as `xsd:double`.
Aliases may make at most `yaml.max-alias-nodes` nodes in all (100,000 by default), so a
small file of nested aliases cannot grow into millions of triples.

Archives and folders, as SPARQL Anything: `archive.matches` keeps only the names a regular
expression matches in full (look-around and back-references are not supported and give an
error). `from-archive=<archive>` reads `location` from inside a zip or tar archive instead of
the file system (`archive-format` names the kind when the archive's extension does not), so
a nested SERVICE can triplify the files a listing gives:

```sparql
SERVICE <x-sparql-anything:location=data.zip,archive.matches=.*csv> {
  ?root ?slot ?file . FILTER(isLiteral(?file))
  SERVICE <x-sparql-anything:> {
    fxe:properties fxe:location ?file ; fxe:from-archive "data.zip" ; fxe:csv.headers "true" .
    ?row ?p ?o } }
```

Archives compressed with bzip2 (`.tar.bz2`, `.tbz2`) are not supported.

Spreadsheets, as SPARQL Anything (which reads only `.xls` and `.xlsx`): each sheet is a
graph named `<root><sheet name>` with its own `fx:Root` (the default graph holds all sheets).
Rows run from the sheet's first used row to its last, empty rows included, and are
`<root><sheet name>_Row_<n>` with `blank-nodes=false`. A row's cells run from its first to
its last non-empty cell, so `rdf:_1` is the row's first non-empty cell; empty cells between
give `""` (no triple with `null-string` set to the empty string). Options:
`spreadsheet.headers` (default false), `spreadsheet.headers-row` (default 1; earlier rows are
data), `spreadsheet.ignore-columns-with-no-header`, `spreadsheet.evaluate-formulas` (default
false: a formula cell gives its formula, `A2+B2`; true gives the value saved in the file,
which is what Excel computed, not a new evaluation). Headers are counted from column A and
data cells from the row's first cell, as SPARQL Anything does, so a header row and data rows
starting in different columns do not line up. Numbers are `xsd:double` written as Java does
(`1.0`, `1.5E10`); dates are `2020-01-31T00:00:00Z` as plain strings.

`spreadsheet.composite-values=true` makes each cell a container, as SPARQL Anything does:
the row links to it (by its header key, or by its slot), it is typed by the cell's kind
(`xyz:STRING`, `xyz:NUMERIC`, `xyz:BOOLEAN`, `xyz:FORMULA`, `xyz:ERROR`), and `rdf:_1` holds
its value: text, a number as `xsd:double` (a date as Excel's day number), a boolean, a
formula's text (or with `spreadsheet.evaluate-formulas` its saved value; the kind stays
`xyz:FORMULA`), nothing for an error. A cell with a hyperlink adds `xyz:address` (the URL,
or the place in the workbook such as `Sheet1!A1`) and `xyz:label` (the link's display text,
else the cell's); a cell with a comment adds `xyz:author` and `xyz:threadedComment` (its
text). With `blank-nodes=false` the cell is `<row>_<column>`, the column counted from 0 from
column A (`…_Row_1_0`). Hyperlinks and comments are read from xlsx and xlsm files only; xls,
xlsb and ods cells get their kind and value. One deviation: without headers SPARQL Anything
puts the container's name in the row's slot as a string, where TripleTrail links the container,
so its value can be reached from the row. A hyperlink over a range is matched against the cells the
sheet has, and each row and cell it reaches counts against `spreadsheet.max-cells`.

Limits, so that a small crafted workbook cannot take the server's memory: a workbook may
give at most `spreadsheet.max-cells` cells (1,000,000 by default), counting the empty
cells between a row's cells and the empty rows between used ones, since each of those
becomes a `""` value or a row container; and an xlsx, xlsm or ods file may unpack to at most
`spreadsheet.max-bytes` bytes (1 GiB by default, by the sizes its zip directory declares).
A cell takes about 300 to 400 bytes of memory while it is lifted, so the default allows
about 400 MB; raise it for larger workbooks if the machine has the memory.
xlsx and xlsb sheets are read cell by cell. An ods file is scanned before it is read, and
refused if its repeated rows and columns (`table:number-rows-repeated`,
`table:number-columns-repeated`) would spread its sheets over more than
`spreadsheet.max-cells` cells, or if its cell text, with repeated cells and runs of spaces
(`<text:s text:c="…"/>`) written out as calamine reads them, comes to more than
`spreadsheet.max-bytes`. A legacy `.xls` file is
scanned before it is read, and refused if its sheets would take more than
`spreadsheet.max-cells` cells (the cells each sheet's DIMENSIONS record reserves, the cells
it holds and the box around them), if the shared strings its cells name come to more than
`spreadsheet.max-bytes` (each cell gets its own copy), or if its compound-file structure
loops or points outside the file.
The limits are options of the query, so whoever writes the query can raise them.

### Geometry (TripleTrail extensions)

These options are TripleTrail's own; SPARQL Anything ignores them, and without them the graph
is exactly SPARQL Anything's.

- `geo=true` on a JSON or GeoJSON source: every GeoJSON geometry object (`Point`,
  `MultiPoint`, `LineString`, `MultiLineString`, `Polygon`, `MultiPolygon`,
  `GeometryCollection`) also gets `geo:asWKT` with a `geo:wktLiteral`, its coordinates
  written exactly as in the file (`POINT Z` when positions have three numbers), and every
  `Feature` gets `geo:hasGeometry` to its geometry's container. A GeoJSON file then lifts
  in a few lines instead of rebuilding WKT from coordinate arrays with `fx:cardinal`:

  ```sparql
  INSERT { ?s geo:hasGeometry ?gs . ?gs geo:asWKT ?wkt } WHERE {
    SERVICE <x-sparql-anything:location=nuts.geojson,geo=true> {
      ?f geo:hasGeometry ?g ; xyz:properties/xyz:NUTS_ID ?id . ?g geo:asWKT ?wkt }
    BIND(fx:entity(ex:, ?id) AS ?s) BIND(fx:entity(ex:, ?id, "/geom") AS ?gs) }
  ```

- `geo.wkt=<column>`, or `geo.x=<column>,geo.y=<column>` (and `geo.z=<column>`), on a CSV
  source: each row with a value there also gets `geo:asWKT` (`POINT(x y)`, `POINT Z(x y z)`
  from coordinate columns). A column is named by its header (with `csv.headers=true`) or by
  its number from 1. Rows whose cells are empty, or not numbers for `geo.x`/`geo.y`, get no
  geometry; a column that does not exist is an error.
- `geo.crs=EPSG:<code>` (or a CRS IRI): the coordinates' CRS, written at the start of each
  WKT literal (`<http://www.opengis.net/def/crs/EPSG/0/25833> POINT(…)`). Without it, WKT
  has no CRS IRI, which GeoSPARQL reads as longitude/latitude (CRS84), as GeoJSON is.

The parsers are also public, in `tripletrail_fx::sources` (`resolve`, `read`, `json`,
`CsvOptions`, `CsvRows`), for other mappings of the same sources.

XML, as SPARQL Anything: without `xml.path`, the root element is the root container (typed
`fx:Root` and by its name). An element is typed `xyz:<local name>`, or `<namespace#local
name>` in a namespace (`#` is added unless the namespace ends with `/` or `#`); attributes
are keys named the same way. Child elements and text share the slot numbers, in document
order; whitespace-only text is skipped and text around a comment is one value. Text and
attributes in the scope of `xml:lang` are language-tagged (`xml.lang-tags=false` turns
this off; `xml:lang=""` or a malformed tag removes the language). With `blank-nodes=false`
an element is `<root>/<root element>/2:child/1:grandchild`.

With `xml.path` (and `xml.path.1`, `xml.path.2`, ... after it), each node an XPath
selects becomes a slot of the root, in document order: an element is a container typed
`xyz:<name as written>` with its attributes as keys and its content as slots (`<root>/1:name`
with `blank-nodes=false`), an attribute a key of the root, a text node a slot of the root.
SPARQL Anything builds this from VTD-XML's token index, and its quirks are followed, as
the rail KG queries number entities by slot: comments and processing instructions take a slot
number without a triple; attributes beyond the selected element's count take slot numbers
in the elements under it; text and comments after an element up to the next element start
belong to it; and the first text of the selected element is also `rdf:_1` of every element
under it. Values are not language-tagged in this mode. The XPath subset: `/` and `//`
steps, names (`p:*` too), `*`, `@name`, `@*`, `text()`, `node()`, `.`, `..`, and the
predicates `[n]`, `[last()]`, `[@a]`, `[@a='v']`, `[child]` and `[child='v']`; anything else
is refused with an error.

General options: `location`, `content`, `media-type`, `triplifier` (matched on `csv`,
`json`, `xml`, `yaml`, `spreadsheet` or `xls`, `text`, `html`, `markdown` or `md`, `shapefile` or `shp`, `kml`, `gpx`, `zip`, `tar` or `archive`), `root`, `namespace`, `blank-nodes`,
`null-string`, `trim-strings`, `charset` (UTF-8 only; a byte order mark is skipped),
`slice`. Others are ignored.

`slice=true`, for CSV: the pattern runs on the root with `slice.rows` rows at a time (10,000
by default) and the answers are put together, so a large file never has all its triples in
memory at once. Patterns that look at one row at a time give the same answers as without
it; a pattern that joins rows, counts or sorts them inside the SERVICE does not (move that
outside the SERVICE). A pattern that matches only the root, such as `?root a fx:Root`,
answers once per slice; use `SELECT DISTINCT` or match a row too. SPARQL Anything runs the pattern once per row (`slice.rows=1` here),
which is many times slower; the answers differ only for patterns that join rows. Other
formats ignore it.

The root container is typed `fx:Root`. Containers are blank nodes, or with
`blank-nodes=false` IRIs under the root: `root/key/_2` for the second item of the array under
`key`. The root is the `root` option, else the location (a `file:` URL for a path), else
`http://sparql.xyz/facade-x/data/` plus the MD5 of the content.

## `tripletrail lift`

```sh
tripletrail lift dags/01-lift --output-dir out --allow data -j 8
tripletrail lift q1.rq q2.rq --output all.nt
```

Runs CONSTRUCT queries (files, or directories searched for `.rq` and `.sparql`) in parallel
and writes their triples as N-Triples: all into one file (`--output`, or standard output),
or each to its own file under `--output-dir`, at the query's path. Relative locations in the
queries resolve against `--base` (the current directory by default); `--allow` limits the
files they may read. `--data` loads files into the store first, for queries that also read
it. Each query prints its time and triple count; a failed query is reported, the others
still run, and the exit status is 1.

Each query writes each triple once, in the order it made them. A query's output stays in
memory up to 64 MB and then goes to a temporary file, next to `--output` (or the query's
output file), else in the system's temporary directory. Duplicates are found by a 128-bit
hash of each line, kept in memory for the first 14.7 million distinct triples of a query
(about 290 MB). Past that, the hashes go to sorted runs on disk, which are merged once the
query is done. Bounded this way, one query's output can be larger than memory.

`-v name=value` (repeatable) fills template parameters of the SPARQL queries (not of RML mappings), as SPARQL Anything's `-v`:
`?_name` becomes the string `"value"`, `?_name_iri` the IRI `<value>` and `?_name_number`
the number as written; `?__name` is optional and stays a variable without a value.
Parameters in strings, IRIs and comments are left alone. Without `-v` the queries run as
they are. Unlike SPARQL Anything, a name given twice is an error rather than a second run.

## Functions

`fx:entity`, `fx:literal`, `fx:bnode`, `fx:cardinal`, `fx:isContainerMembershipProperty`,
`fx:before`, `fx:after`, `fx:previous`, `fx:next`, `fx:forward`, `fx:backward`,
`fx:String.*` (`startsWith`, `endsWith`, `indexOf`, `lastIndexOf`, `substring`,
`toLowerCase`, `toUpperCase`, `trim`, `stripLeading`, `stripTrailing`, `replace`,
`removeTags`), `fx:URLEncoder.encode` and `fx:URLDecoder.decode`.

Known differences:

- `fx:entity` percent-encodes the characters an IRI may not contain (a space becomes `%20`),
  so it always gives a valid IRI; SPARQL Anything keeps them as they are.
- String positions count characters, not UTF-16 code units.
- Language tags are lower-cased (`"Colour"@en-gb`; SPARQL Anything keeps `en-GB`).
- XML: only the five predefined entities and character references are resolved (no DTD),
  gzipped files are not read, and documents must be UTF-8.
- A prefixed name with two dots in its local part (`fxe:csv.headers.sanitize`) does not
  parse (a SPARQL parser bug); write the full IRI or set the option in the SERVICE IRI.
