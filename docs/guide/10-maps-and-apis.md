# 10. Maps and APIs

Not everyone who needs your data writes SPARQL. Out of the box, TripleTrail serves every dataset
as an interactive map, a readable page per resource, a search page, a graph explorer, OGC API Features and vector tiles for GIS
tools, and a JSON REST API and GraphQL for developers. They all work from shapes TripleTrail generates from
the data, so there is nothing to configure; you can refine them later.

Start the server with the sample network:

<!-- test: serve -->
```sh
tripletrail serve --load network.ttl
```

## The Map page

<http://localhost:7878/$/ui/map/ds> draws the dataset on OpenStreetMap, a layer per class with
a geometry: here Station and Section.

- **Filters**: each layer has facets for its properties with counts (electrified, maximum
  speed), and a range for numbers (length).
- **Colour by** draws a layer in a colour per value of a facet, with a key. A facet's
  `tts:colorBy` (a number, lowest first) puts it at the top of that list.
- **Click** a feature to open its describe page beside the map.
- **Search** finds features by the words of their names.
- **Labels** are in your browser's language when the data has one in it, else in English,
  else without a language. A pack can take text out of every label the map shows with
  `tts:labelStrip`, a regular expression: the RINF Atlas import below takes out RINF's
  validity period, so "Brussel-West (from 2022-12-11 until 2078-12-31)" shows as
  "Brussel-West".
- **Views**, when the shapes name some, show one theme at a time: see
  [Name views of the map](#name-views-of-the-map).
- **Layers**: the eye beside a layer shows or hides it (**Show all** and **Hide all** do every
  layer). A layer drawn only from a closer zoom is greyed out until then, with a magnifier
  that zooms in to it. The funnel opens the layer's filters and shows how many are on.
- **Zoom to all the data**, under the zoom buttons, fits the map to every layer; the button
  under it picks the **base map**: OpenStreetMap, the same in grey (so coloured features stand
  out), none, and any the server adds. The last button goes fullscreen, with the layer list.
- **Tools**, the ruler under the base map button, draws an area that every layer is filtered
  by, measures lengths and areas, and shows the coordinates under the pointer: see
  [Draw an area, measure and read coordinates](#draw-an-area-measure-and-read-coordinates).
- The feature open beside the map is outlined in navy.
- The page's link keeps the view, the filters, hidden layers, the base map, the area and the
  feature open beside the map (`feature=IRI`), so it can be shared: **Share** in the top bar
  copies it.

The map fetches its background from OpenStreetMap, which then learns where viewers look. For
private data, or heavy use, start the server with `--map-tiles` naming your own tile server,
or `--map-tiles none`. `--map-basemap` adds base maps to the menu, each as a title, an XYZ tile
URL and its credit, and, when the provider's tiles stop before zoom 19, the last zoom it has
(here 17). Past that zoom the map enlarges the tiles of zoom 17 instead of showing blank ones:

<!-- test: serve -->
```sh
tripletrail serve --load network.ttl \
  --map-basemap 'Aerial|https://tiles.example.org/aerial/{z}/{x}/{y}.jpg|© Example Aerial|17'
```

The map's style lists the base maps the menu offers, and the page adds the grey one and none:

<!-- test: run -->
```sh
curl -s http://localhost:7878/ds/tiles/style.json | grep -o '"title":"[^"]*"' | head -2
```

<!-- test: output -->
```text
"title":"OpenStreetMap"
"title":"Aerial"
```

Each comes with the last zoom of its tiles:

<!-- test: run -->
```sh
curl -s http://localhost:7878/ds/tiles/style.json | grep -o '"maxzoom":1[0-9],"tiles"' | head -2
```

<!-- test: output -->
```text
"maxzoom":19,"tiles"
"maxzoom":17,"tiles"
```

On a large dataset the map's first request waits for TripleTrail to read the data's classes
and count every layer's facets: on all of RINF's countries, close to a minute. So once the
datasets are open, the server makes each one's map in the background, and its log says when
it is ready. It also builds the index of the search box. The counts of the panel without
filters stay until the next write. It gives
each dataset at most five minutes (and each query the dataset's
timeout); what is left then waits for the first visitor. Before the map it sorts the
dataset's IRIs and strings once, for queries with `MIN`, `MAX` or `ORDER BY` on them (the
log line `ds: string ranks of N terms`). A load, an update or new shapes make the counts
out of date, so the server makes the map again once the dataset has kept still for half a
minute after one, and logs `ds: after a change, map ready: …`; a load of many batches is
made again once, after its last batch. `--no-warm-up` leaves it all to the first visitor:

<!-- test: run -->
```sh
tripletrail serve --load network.ttl --bind 127.0.0.1:7879 2> serve.log &
for i in $(seq 100); do grep -q 'map ready' serve.log && break; sleep 0.1; done
kill $!
grep -o 'ds: map ready: [0-9]* collections' serve.log
```

<!-- test: output -->
```text
ds: map ready: 2 collections
```

## Describe pages

Every resource has a page at `/ds/describe?iri=…`. Open
<http://localhost:7878/ds/describe?iri=https://example.org/rail%23LV> for Leuven.

- **The header** names the resource's classes, its label and its IRI, with buttons: **Copy
  IRI**, **Share** (copies a link to the page and the tab you are on), **Open in map** (the
  dataset's map at the resource, with it selected; only for a resource with a geometry),
  **Explore** (walk the graph from it, see [The Explore page](#the-explore-page)) and
  **Export** (the description as Turtle, JSON-LD, N-Triples or RDF/XML).
- **Overview** has the map of its geometries and its properties, grouped as the dataset's
  shapes say ([Make it yours: shapes](#make-it-yours-shapes)). Shapes can add tabs of their own.
- **All properties** lists every triple of the resource, **Incoming links** every link to it.
- **Graph** draws its neighbourhood: what links to it on the left, its classes, links and
  values on the right. Click a node to open its page.
- **Data model** draws its classes and their superclasses as UML class boxes, from the
  shapes: their attributes with type and multiplicity (a filled dot when this resource has
  it), the classes they point at and the classes that point at them. Below it, every attribute
  as a table, and the model as a Mermaid class diagram to copy.
- **RDF** shows the description as Turtle, or JSON-LD, N-Triples or RDF/XML at a click, to read,
  copy or download.

The tab is in the address (`&tab=graph`), so a link opens the same view. Without the web UI
(`--no-ui`), or without script, the page shows the same sections one under the other.

The page lists its tabs for a program too:

<!-- test: run -->
```sh
curl -s -G http://localhost:7878/ds/describe -H 'Accept: text/html' \
  --data-urlencode 'iri=https://example.org/rail#LV' | grep -o 'data-label="[^"]*"'
```

<!-- test: output -->
```text
data-label="Overview"
data-label="All properties"
data-label="Incoming links"
data-label="Graph"
data-label="Data model"
data-label="RDF"
```

The same URL gives RDF to a program that asks for it:

<!-- test: run -->
```sh
curl -s -G http://localhost:7878/ds/describe -H 'Accept: application/n-triples' \
  --data-urlencode 'iri=https://example.org/rail#LV'
```

<!-- test: output unordered -->
```text
<https://example.org/rail#LV> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://example.org/rail#Station> .
<https://example.org/rail#LV> <http://www.w3.org/2000/01/rdf-schema#label> "Leuven" .
<https://example.org/rail#LV> <https://example.org/rail#code> "LV" .
<https://example.org/rail#LV> <http://www.opengis.net/ont/geosparql#hasGeometry> _:b0 .
_:b0 <http://www.opengis.net/ont/geosparql#asWKT> "POINT(4.7158 50.8813)"^^<http://www.opengis.net/ont/geosparql#wktLiteral> .
```

Blank nodes are named `b0`, `b1`… in the order they come, so the same description is the same
text. Turtle declares prefixes for the namespaces it uses (`rail:LV a rail:Station`). With
`--base-iri ds=https://data.example.org/`, IRIs under that prefix are served at their own URLs
as Linked Data: see [Linked Data](../server.md#linked-data).

To find a resource by name, type it in the search box of the web UI's top bar (or press `/`;
in a small window or on a phone, click the magnifier),
and pick a result to open its describe page. The same search answers programs, with the last
word as a prefix so that it works while typing:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/ds/search/labels?q=leu' | grep -o '"iri":"[^"]*"'
```

<!-- test: output -->
```text
"iri":"https://example.org/rail#LV"
```

## The Search page

<http://localhost:7878/$/ui/search/ds> builds a question from the dataset's classes and
properties, without SPARQL: **Find** a class, then add conditions such as "where maxSpeed >
120 and from is Brussels-South". Each property offers the operators that fit it: comparisons
and ranges for numbers and dates, contains and starts with for text, yes or no for booleans,
and a list to pick from for links to other resources. A value box suggests the values the
property takes among the resources the other conditions find, each with how many have it.
**or** next to a condition adds an alternative: "where from is Brussels-South *or* maxSpeed =
300". **+ Column** shows more properties in the results, each with all its values, such as every
station a line runs through, and every result links to its describe page.

- **Search this area**, in a layer's **Filter** panel on the Map page, opens the Search page
  on that class within the area the map shows: "Find Station in the map area …". The × next
  to the area searches everywhere again.
- **Copy link** gives a link that opens the same search.
- **Open in Query** puts the SPARQL of the search in a new tab of the Query page, to refine
  it there.

Programs can run the same search. It is JSON in the `m` parameter: the class, the conditions
(`where`) and extra columns (`show`). This finds the fast sections leaving Brussels-South:

<!-- test: run -->
```sh
curl -s -G http://localhost:7878/ds/search/results --data-urlencode 'm={
  "class": "Section",
  "where": [{"key": "maxSpeed", "op": "gt", "value": 120},
            {"key": "from", "op": "is", "value": "https://example.org/rail#BXS"}],
  "show": ["lengthKm"]}' | grep -o '"iri":"[^"]*#S[0-9]*"'
```

<!-- test: output -->
```text
"iri":"https://example.org/rail#S08"
"iri":"https://example.org/rail#S09"
```

An `area`, west, south, east and north in longitude and latitude, keeps the resources whose
geometry meets it. These are the stations in central Brussels:

<!-- test: run -->
```sh
curl -s -G http://localhost:7878/ds/search/results \
  --data-urlencode 'm={"class": "Station", "area": [4.30, 50.82, 4.40, 50.87]}' | grep -o '"label":"[^"]*"'
```

<!-- test: output -->
```text
"label":"Brussels-Central"
"label":"Brussels-North"
"label":"Brussels-South"
```

The suggestions come from `/ds/search/values`: here the stations sections leave from, among
those whose name holds "brus", most used first:

<!-- test: run -->
```sh
curl -s -G http://localhost:7878/ds/search/values --data-urlencode 'm={"class": "Section"}' \
  --data-urlencode key=from --data-urlencode q=brus | grep -o '"count":[0-9]*,[^}]*"label":"[^"]*"'
```

<!-- test: output -->
```text
"count":3,"iri":true,"label":"Brussels-South"
"count":2,"iri":true,"label":"Brussels-North"
"count":1,"iri":true,"label":"Brussels-Central"
```

`/ds/search/classes` lists the classes and their properties with the kind of each, and
`/ds/search/sparql` gives the SPARQL of a search. See [search](../server.md#search-by-name).

### Find similar

When the dataset has embeddings (`tt:vector` values, see [vectors](03-data.md)), the Search
page also has a **Find similar** card: type a text, or paste a resource's IRI, pick the
property holding the vectors, and get the ten nearest resources with how similar each is,
each linking to its describe page and to **explore**.

A text has to become a vector first, with the same embedding model that made the dataset's
vectors. Your browser does that, with the AI service you set up for the Workbench's AI tab
(**Key and model…**): it sends the text to that service's `/embeddings`, with your key, and
gives TripleTrail only the numbers it gets back. The key never reaches the server. Name the
embedding model next to the service (`text-embedding-3-small` for OpenAI, `mistral-embed` for
Mistral, `nomic-embed-text` for Ollama are filled in); the browser remembers it per service.
Anthropic has no embeddings, so pick another service for this. An IRI needs no AI service:
the resource's own vector is used.

The card asks [`/{dataset}/search/similar`](../vector-search.md#retrieval-for-graphrag), as
an application would.

## The Records page

<http://localhost:7878/$/ui/records/ds> browses the dataset's records like a database tool
lists its tables. On the left are the classes with how many records each has. Pick one, and
the middle shows its records in a grid, a column per property:

- Click a column's header to sort by it. Each header has a filter box: text that a value
  contains, a number with `>`, `<=` or a range such as `20..60`, yes or no for a boolean.
- **Label contains…** keeps the records whose label holds the text.
- The grid pages through the records 50 at a time, and the server does the sorting, the
  filtering and the paging, so a class of a million records stays quick.
- **CSV** downloads the filtered records (up to 10,000, read with your token like the grid), and **Open as SPARQL** puts the
  grid's query on the Query page.

Click a record and the right shows it as a form, drawn from the shapes: their groups in their
order, each property with its viewer (`dash:viewer`), and the resources that link to it under
**Referenced by**. **Describe**, **Map**, **Explore** and **Open as SPARQL** open the record in
the other tools. The link holds the class and the record, so it can be shared. A property
kept from the forms with `tts:hiddenFrom tts:Reader-Forms` (see
[below](#keep-a-property-from-the-record-forms)) does not show in the form.

The grid uses the search above with three more parts: `sort` (a key, or `{"key": …, "desc":
true}`; the label by default), `q` (the label contains it, case aside) and the `offset`
parameter. These are the longest sections after the first:

<!-- test: run -->
```sh
curl -s -G http://localhost:7878/ds/search/results --data-urlencode 'm={
  "class": "Section", "show": ["lengthKm"], "sort": {"key": "lengthKm", "desc": true}}' \
  --data-urlencode limit=3 --data-urlencode offset=1 | grep -o '"iri":"[^"]*#S[0-9]*"'
```

<!-- test: output -->
```text
"iri":"https://example.org/rail#S11"
"iri":"https://example.org/rail#S12"
"iri":"https://example.org/rail#S08"
```

`f=csv` gives the same rows as CSV, the IRI and the label first (with Windows line ends,
which `tr` takes out here):

<!-- test: run -->
```sh
curl -s -G http://localhost:7878/ds/search/results \
  --data-urlencode 'm={"class": "Station", "q": "brussels"}' --data-urlencode f=csv | tr -d '\r'
```

<!-- test: output -->
```text
iri,label
https://example.org/rail#BXC,Brussels-Central
https://example.org/rail#BXN,Brussels-North
https://example.org/rail#BXS,Brussels-South
```

The form is the describe view read for the forms reader:
`/ds/describe?iri=…&format=view&reader=forms`.

### Edit a record

When you may write the dataset (on a server with tokens, see
[Access control](12-access-control.md)), the grid has **+ New** and a record has **Edit**. Both open the record as a form of
the keys you can write: a box per value, drawn by its editor (a checkbox for a boolean, a date
picker for a date, a number box for a number), a row per language for text with a language,
and **+ Add** for a key that takes several values. A reference to another record is found by
its name: type a few letters and pick it from the list. A key the shapes require is marked
with `*`.

- Each change is checked by the server as you make it, against the record's shape, and what it
  refuses shows under its field, so the form is wrong before it is saved, never after.
- **Save** sends only the keys you changed. If someone saved the record since you opened it, a
  dialog shows their values beside yours: **Keep mine** saves yours over theirs (the keys you
  did not change keep theirs), **Take theirs** opens the record as it is now.
- A new record takes the IRI you give it, else one from the shape's `tts:iriTemplate`, else a
  `urn:uuid:` IRI.
- **Delete…** first says how many links from other resources would be left pointing at the
  record (the links you can see: those you may not read are not counted).
- A key whose path is more than one property, an embedded object and the geometry are shown but
  not edited here.

The page does it with the REST API's writes [below](#the-rest-api) (with `?dry=true` for the
checks). Which classes and keys you may write is in `/ds/search/classes`: per class `writable`
and `rest` (where its records are written), per key `writable` and `required`:

<!-- test: run -->
```sh
curl -s http://localhost:7878/ds/search/classes | grep -o '"rest":"[^"]*","title":"[^"]*","writable":true'
```

<!-- test: output -->
```text
"rest":"Section","title":"Section","writable":true
"rest":"Station","title":"Station","writable":true
```

## The Explore page

<http://localhost:7878/$/ui/explore/ds?iri=https://example.org/rail%23BXC> draws the graph
around Brussels-Central: the station and the two sections that link to it. Click a node to open
its links. Click S01 and then Brussels-South, and the graph grows to the sections leaving
Brussels-South and the stations at their other ends.

- A node with many links of one kind shows the first 8 and a box such as `has (9,950 more)`;
  click it for 50 more.
- The panel beside the graph shows the selected node's values and links, with **Collapse**,
  **Pin** (or drag the node), **Hide**, **Open page** for its describe page, and **Start here**.
- **Filter** hides links by predicate and nodes by class, and **Fit** brings everything into view.
- The page's link holds what is open and hidden, so it can be shared. The Map page's detail
  panel and the Search page's results have an **Explore** link to it.

**CONSTRUCT** saves `explore.rq`, the query that gives the links shown, to use as a starting
point elsewhere. For the start above it is:

<!-- test: file explore.rq -->
```sparql
# The links shown on the Explore page of ds, from https://example.org/rail#BXC
CONSTRUCT { ?s ?p ?o }
WHERE {
  VALUES (?s ?p ?o) {
    (<https://example.org/rail#S02> <https://example.org/rail#from> <https://example.org/rail#BXC>)
    (<https://example.org/rail#S01> <https://example.org/rail#to> <https://example.org/rail#BXC>)
  }
  ?s ?p ?o
}
```

<!-- test: run -->
```sh
curl -s http://localhost:7878/ds/sparql -H 'Accept: application/n-triples' \
  -H 'Content-Type: application/sparql-query' --data-binary @explore.rq
```

<!-- test: output unordered -->
```text
<https://example.org/rail#S02> <https://example.org/rail#from> <https://example.org/rail#BXC> .
<https://example.org/rail#S01> <https://example.org/rail#to> <https://example.org/rail#BXC> .
```

**SVG** saves the picture.

**Map** shows a map beside the graph. Every node with a GeoSPARQL geometry is drawn on it:
here the stations as points and the sections as lines, in the colours of the graph. Selecting a
node selects it on the map, and clicking a feature on the map selects its node. The map
follows what the graph shows until you move it. The link keeps the map open.

To see how two resources are connected, select one and click **Path from here**. Then select
another node, or type an IRI and click **Find**. The page adds the shortest path between them,
up to 8 links followed in either direction, and draws it highlighted. The link keeps the path,
and **Clear paths** removes it. The page runs TripleTrail's path search
([chapter 5](05-routing.md)) over every link between IRIs except `rdf:type` and hidden links,
searching from both ends at once, so it stays quick on a large dataset.
From Brussels-North to Ottignies:

<!-- test: query -->
```sparql
PREFIX ex:   <https://example.org/rail#>
PREFIX rdf:  <http://www.w3.org/1999/02/22-rdf-syntax-ns#>
PREFIX path: <http://w3id.org/tripletrail/path#>
SELECT ?step ?from ?edge ?to WHERE {
  SERVICE <http://w3id.org/tripletrail/path> {
    [] path:source ex:BXN ; path:target ex:OTN ; path:direction "both" ; path:maxLength 8 ;
       path:exceptPredicate rdf:type ;
       path:step ?step ; path:from ?from ; path:edge ?edge ; path:to ?to .
  }
} ORDER BY ?step
```

<!-- test: output -->
```text
step,from,edge,to
1,https://example.org/rail#BXN,https://example.org/rail#to,https://example.org/rail#S02
2,https://example.org/rail#S02,https://example.org/rail#from,https://example.org/rail#BXC
3,https://example.org/rail#BXC,https://example.org/rail#to,https://example.org/rail#S01
4,https://example.org/rail#S01,https://example.org/rail#from,https://example.org/rail#BXS
5,https://example.org/rail#BXS,https://example.org/rail#from,https://example.org/rail#S09
6,https://example.org/rail#S09,https://example.org/rail#to,https://example.org/rail#OTN
```

## OGC API Features: data for GIS tools

`/ds/ogc` serves the dataset as [OGC API – Features](https://ogcapi.ogc.org/features/), which
QGIS, ArcGIS, Leaflet and OpenLayers read without SPARQL. In QGIS: *Layer → Add Layer → Add
WFS / OGC API – Features Layer*, a new connection with the URL `http://localhost:7878/ds/ogc`,
then pick a collection.

Each class with a geometry is a collection:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/ds/ogc/collections?f=json' | grep -o '"id":"[A-Za-z]*"'
```

<!-- test: output unordered -->
```text
"id":"Section"
"id":"Station"
```

Its items are GeoJSON features, with the class's properties. `bbox` keeps those in an area,
and `filter` takes a [CQL2](https://docs.ogc.org/is/21-065r2/21-065r2.html) expression over
the properties:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/ds/ogc/collections/Station/items?bbox=4.2,50.75,4.8,51.05&f=json' \
  | grep -o '"label":"[^"]*"'
```

<!-- test: output unordered -->
```text
"label":"Brussels-Central"
"label":"Brussels-North"
"label":"Brussels-South"
"label":"Leuven"
"label":"Mechelen"
```

Each feature has one geometry: of the alternatives its shape's geometry path lists, the first
that gives it one (a track's own line before that of the section it is part of), with
longitudes and latitudes to 7 decimals, about a centimetre. `geometries=all` gives all of
them as a `GeometryCollection`.

<!-- test: run -->
```sh
curl -s -G http://localhost:7878/ds/ogc/collections/Section/items -d f=json \
  --data-urlencode 'filter=maxSpeed > 150 AND electrified = true' | grep -o '"line":"[^"]*"'
```

<!-- test: output unordered -->
```text
"line":"25"
"line":"25"
"line":"36"
"line":"2"
"line":"50A"
"line":"161"
"line":"161"
```

`IN` keeps the features with one of a list of values. A list of strings matches a value by its
text, whether the value is a plain string, a string with a language tag, or an IRI, so a list
of names works however the names were written:

<!-- test: run -->
```sh
curl -s -G http://localhost:7878/ds/ogc/collections/Station/items -d f=json \
  --data-urlencode "filter=label IN ('Leuven', 'Namur', 'Atlantis')" | grep -o '"label":"[^"]*"'
```

<!-- test: output unordered -->
```text
"label":"Leuven"
"label":"Namur"
```

GIS tools that read [JSON-FG](https://docs.ogc.org/is/21-045r1/21-045r1.html) (GeoJSON with
time and other CRSs) ask for it with `Accept: application/vnd.ogc.fg+json`, or add `f=jsonfg`:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/ds/ogc/collections/Station/items?f=jsonfg&limit=1' \
  | grep -o '"conformsTo":[^]]*]'
```

<!-- test: output -->
```text
"conformsTo":["http://www.opengis.net/spec/json-fg-1/1.0/conf/core"]
```

Each feature's `id` is its IRI. A dataset started with one `--base-iri` gives the features
under it short ids instead: their IRI's part after the base, so `https://data.example.org/s1`
is `s1` and is at `…/items/s1`.

In a browser, the same URLs are HTML pages you can click through, starting from
<http://localhost:7878/ds/ogc>.

## Vector tiles

`/ds/tiles/all/{z}/{x}/{y}.mvt` serves Mapbox Vector Tiles, a layer per class, for MapLibre,
OpenLayers or QGIS's *Vector Tiles* source. `/ds/tiles/style.json` is a ready-made MapLibre
style of the dataset, the one the Map page draws:

<!-- test: run -->
```sh
curl -s -o tile.mvt -w '%{http_code} %{content_type}\n' http://localhost:7878/ds/tiles/all/8/131/86.mvt
```

<!-- test: output -->
```text
200 application/vnd.mapbox-vector-tile
```

Tiles of a whole country or continent stay small. Below zoom 8, a layer with more than 1,000
features in a tile is drawn coarser (512 units a side instead of 4,096), and one feature
remains where several fall on the same position. Each feature names its resource in the
attribute `iri`; in such a coarser layer it has a number instead, the feature's id, and
`/ds/tiles/iris/{collection}?id=…` gives the IRIs of numbers (the Map page asks it when you
click such a feature). The small network here keeps every feature, with `iri`, but the
lookup works on any number; `97728579416` is the number of `ex:LV`:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/ds/tiles/iris/Station?id=97728579416'
```

<!-- test: output -->
```text
{"97728579416":["https://example.org/rail#LV"]}
```

A client that asks for gzip gets the tiles gzipped, as it does the map's facets and searches
and the OGC API's answers; a browser always asks. Answers under 1 KB, as this network's tiles
are, go as they are:

<!-- test: run -->
```sh
curl -s -D - -o /dev/null -H 'Accept-Encoding: gzip' http://localhost:7878/ds/ogc/collections | tr -d '\r' | grep -i '^content-encoding'
```

<!-- test: output -->
```text
Content-Encoding: gzip
```

### Narrow facets and tiles to an area, a graph or a text

`/ds/tiles/facets/{collection}` counts what the Map page's filters keep. Three more filters
work on the facets and on the tiles alike, for an application that draws its own map:

- `bbox=minLon,minLat,maxLon,maxLat` keeps the features whose geometry meets the box ("search
  in this map area");
- `graph=IRI`, repeated for several, keeps the features with triples in those named graphs
  (one source of data on the map; the dataset needs a union default graph to draw them);
- `q=words` keeps the features whose indexed text holds every word, as the map's search box
  matches: the last word as a prefix, case and accents aside.

The stations around Brussels, then those named Brussels, then Brussels-North alone:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/ds/tiles/facets/Station?bbox=4.3,50.8,4.4,50.9' | grep -o '"features":[0-9]*'
curl -s 'http://localhost:7878/ds/tiles/facets/Station?q=brussels' | grep -o '"features":[0-9]*'
curl -s 'http://localhost:7878/ds/tiles/facets/Station?q=brussels%20no' | grep -o '"features":[0-9]*'
```

<!-- test: output -->
```text
"features":3
"features":3
"features":1
```

They combine with each other and with facet filters, and a tile asked with them
(`/ds/tiles/all/8/131/86.mvt?q=brussels`) draws only those features.

### Filter by a spatial relation to an area

`area` goes further than a box: it keeps the features with a geometry in a relation to any
area, drawn or taken from a feature. The OGC API's items take the same parameters.

- `area`: WKT in longitude and latitude (or in the CRS it names first, as a `geo:wktLiteral`
  does), or the IRI of a resource with a geometry, such as a station or a region;
- `rel`: how a feature's geometry relates to the area. It is `intersects` by default, or
  `within`, `contains`, `touches`, `crosses`, `overlaps`, `disjoint` (none of its geometries
  meets the area) or `dwithin`;
- `dist`: metres, with `rel=dwithin`: the features at most that far from the area.

The stations in an area drawn around Mechelen and Antwerp, then those within 1.7 km and
within 2 km of Brussels-Central, named by its IRI:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/ds/tiles/facets/Station?area=POLYGON((4.3%2050.95,%204.6%2050.95,%204.6%2051.3,%204.3%2051.3,%204.3%2050.95))' | grep -o '"features":[0-9]*'
curl -s 'http://localhost:7878/ds/tiles/facets/Station?area=https%3A%2F%2Fexample.org%2Frail%23BXC&rel=dwithin&dist=1700' | grep -o '"features":[0-9]*'
curl -s 'http://localhost:7878/ds/tiles/facets/Station?area=https%3A%2F%2Fexample.org%2Frail%23BXC&rel=dwithin&dist=2000' | grep -o '"features":[0-9]*'
```

<!-- test: output -->
```text
"features":2
"features":2
"features":3
```

The sections that touch Brussels-North (they end there), and the stations outside the area
around Mechelen and Antwerp, as GeoJSON items:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/ds/tiles/facets/Section?area=https%3A%2F%2Fexample.org%2Frail%23BXN&rel=touches' | grep -o '"features":[0-9]*'
curl -s 'http://localhost:7878/ds/ogc/collections/Station/items?area=POLYGON((4.3%2050.95,%204.6%2050.95,%204.6%2051.3,%204.3%2051.3,%204.3%2050.95))&rel=disjoint' | grep -o '"numberReturned":[0-9]*'
```

<!-- test: output -->
```text
"features":3
"numberReturned":8
```

A bad `area`, `rel` or `dist` is a `400` that says which. The candidates come from the
spatial index, so a small area on a large dataset stays fast.

A panel that shows several layers at once asks for their facets in one request, naming each
collection: every collection's own answer comes back, with `features` their sum and `shared`
the facets they have in common (the same key), summed over them. A bare filter such as
`country=…` applies to each collection that has the facet, `Section.country=…` to one alone.
The ten stations and twelve sections:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/ds/tiles/facets?collection=Station&collection=Section' | grep -o '"features":[0-9]*'
```

<!-- test: output -->
```text
"features":10
"features":12
"features":22
```

### Draw an area, measure and read coordinates

The Map page's **Tools** button opens a panel with three parts.

**Filter by an area** draws an area, and every layer then shows only the features in it,
their filters count only those, and a layer's **OGC API**, **Copy GIS link** and **Query**
take the area along:

- **Rectangle**: two opposite corners;
- **Polygon**: its corners; click the first again, double-click or press Enter to close it;
- **Circle**: the centre, then a point on the edge;
- **Corridor**: a line, and the features within the **Distance** of it (250 m unless set).

**Keep the features whose geometry** picks the relation (`rel`): meets the area (the default),
lies within it, contains it, touches its edge, crosses it, overlaps it, lies outside it, or is
within a distance of it. **Filter by it**, beside a feature open next to the map, takes that
feature's geometry as the area, for the stations within 2 km of a station, say. **Copy WKT**
copies a drawn area, **Clear area** shows every feature again. Esc stops a drawing and
Backspace takes its last point back.

The area goes into the page's link and into the map's requests as the parameters of
[Filter by a spatial relation to an area](#filter-by-a-spatial-relation-to-an-area): a
rectangle or polygon as a `POLYGON`, a circle as its centre with `rel=dwithin` and its radius
as `dist`, a corridor as its `LINESTRING` with `rel=dwithin`, and a feature by its IRI. A
circle of 2 km round Brussels-Central, and a corridor 500 m either side of a line from
Brussels to the east:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/ds/tiles/facets/Station?area=POINT(4.3571%2050.8455)&rel=dwithin&dist=2000' | grep -o '"features":[0-9]*'
curl -s 'http://localhost:7878/ds/tiles/facets/Section?area=LINESTRING(4.35%2050.86,%204.48%2050.90)&rel=dwithin&dist=500' | grep -o '"features":[0-9]*'
```

<!-- test: output -->
```text
"features":3
"features":3
```

In a layer's **OGC API** and **Copy GIS link**, a drawn rectangle or polygon goes into the
CQL2 `filter` instead (`S_INTERSECTS`, `S_WITHIN`, … on `geometry`), so QGIS or any other
OGC API client reads it as a standard filter. "Lies outside it", a circle, a corridor, a
feature or another layer has no CQL2 form and stays `area=`: outside keeps a feature when
none of its geometries meets the area, and a CQL2 filter holds when any one of them does. The
stations within the area around Mechelen and Antwerp, either way:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/ds/ogc/collections/Station/items?filter=S_WITHIN(geometry,%20POLYGON((4.3%2050.95,%204.6%2050.95,%204.6%2051.3,%204.3%2051.3,%204.3%2050.95)))' | grep -o '"numberReturned":[0-9]*'
curl -s 'http://localhost:7878/ds/ogc/collections/Station/items?area=POLYGON((4.3%2050.95,%204.6%2050.95,%204.6%2051.3,%204.3%2051.3,%204.3%2050.95))&rel=within' | grep -o '"numberReturned":[0-9]*'
```

<!-- test: output -->
```text
"numberReturned":2
"numberReturned":2
```

**Or the features of** a layer selects by location: the features of that layer, as its own
filters keep them, are the area (`area=layer:{id}`, with the layer's filters as
`{id}.{key}=…`), and each feature is taken against every one of them but itself. The
stations at the ends of a section without electrification, and the stations within 2 km of
another station:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/ds/tiles/facets/Station?area=layer:Section&Section.electrified=false' | grep -o '"features":[0-9]*'
curl -s 'http://localhost:7878/ds/tiles/facets/Station?area=layer:Station&rel=dwithin&dist=2000' | grep -o '"features":[0-9]*'
```

<!-- test: output -->
```text
"features":2
"features":3
```

**Measure** draws a line, with its length and each leg's length and bearing ("43.0 km · 90°
E"), or an area, with its size and perimeter. Lengths and areas are on the Earth's sphere,
not on the flat map, so they hold far from the equator too. A measurement stays on the map
until the next one, or **Clear**; it does not filter anything.

**Coordinates** shows the latitude and longitude under the pointer, and **Copy a point**
copies the next point clicked as WKT (`POINT(4.3571 50.8455)`), for a query or a GIS. With an
EPSG code in **Also in EPSG** (31370 for the Belgian Lambert 72, say; the browser remembers
it), the panel also shows the point in that CRS, and copies it there as a `geo:wktLiteral`
with the CRS first. The server transforms it, as `geof:transform` does in a query:

<!-- test: run -->
```sh
curl -s http://localhost:7878/ds/sparql -H 'Accept: text/csv' --data-urlencode \
  'query=SELECT (<http://www.opengis.net/def/function/geosparql/transform>("POINT(4.3571 50.8455)"^^<http://www.opengis.net/ont/geosparql#wktLiteral>, <http://www.opengis.net/def/crs/EPSG/0/31370>) AS ?w) {}'
```

<!-- test: output -->
```text
w
<http://www.opengis.net/def/crs/EPSG/0/31370> POINT(149179.3617754487 170566.12911798153)
```

### Count the classes of a coloured map

With `color={key}` naming a range facet, as the map's **Colour by** sends it, that facet comes
with its `classes`: the colours its key shows, each with the `count` of features drawn in it
under the filters (a feature by its highest number, as its tiles colour it). They are the
shape's `tts:colorClasses`, else up to eight of one round width over the values, as the map
makes them. The sections by length, 2 to 64 km, in classes of 20 km:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/ds/tiles/facets/Section?color=lengthKm' | grep -o '"classes":\[[^]]*\]'
```

<!-- test: output -->
```text
"classes":[{"count":3,"label":null,"upTo":20.0},{"count":5,"label":null,"upTo":40.0},{"count":3,"label":null,"upTo":60.0},{"count":1,"label":null,"upTo":null}]
```

The features with no number, drawn grey, are `features` less the classes' counts. A values facet
needs nothing more: its `values` already count the features of each colour.

## The REST API

`/ds/api/v1` is a JSON API for developers: one list per class, records by id, with an OpenAPI
definition at `/ds/api/openapi.json` (or `.yaml`). Records are JSON-LD, so they are still RDF; `f=json`
gives plain JSON, `f=geojson` features and `f=csv` a table for a spreadsheet:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/ds/api/v1/Station?f=csv&q=brussels'
```

<!-- test: output unordered -->
```text
id,type,hasGeometry,label,code
https://example.org/rail#BXS,https://example.org/rail#Station,,Brussels-South,BXS
https://example.org/rail#BXC,https://example.org/rail#Station,,Brussels-Central,BXC
https://example.org/rail#BXN,https://example.org/rail#Station,,Brussels-North,BXN
```

`q` searches the labels, `?code=LV` filters on a property, `bbox` on the geometry, and
`limit` and `offset` page through the records.

To try the API in a browser, open its docs page,
[http://localhost:7878/$/ui/api/ds](http://localhost:7878/$/ui/api/ds). It shows every list
and record of the OpenAPI definition in Swagger UI, and **Try it out** sends a request and
shows the answer:

<!-- test: run -->
```sh
curl -s -o /dev/null -w '%{http_code}\n' 'http://localhost:7878/$/ui/api/ds'
```

<!-- test: output -->
```text
200
```

Tools that want the definition as a file, such as client generators and linters, get it as
JSON or YAML:

<!-- test: run -->
```sh
curl -s http://localhost:7878/ds/api/openapi.yaml | grep '^openapi'
```

<!-- test: output -->
```text
openapi: 3.1.0
```

The API writes too. `POST` adds a record, with its IRI as `id`; `PATCH` changes the keys it
names, `PUT` replaces them all and `DELETE` removes the record:

<!-- test: run -->
```sh
curl -s -X POST http://localhost:7878/ds/api/v1/Station -H 'Content-Type: application/json' \
  -d '{"id": "https://example.org/rail#HAL", "label": "Halle", "code": "HAL"}'
```

<!-- test: output contains -->
```text
"code":"HAL","id":"https://example.org/rail#HAL","label":"Halle"
```

A value the record's shape doesn't allow is refused, with the reason per key, and nothing is
written:

<!-- test: run -->
```sh
curl -s -X PATCH http://localhost:7878/ds/api/v1/Station/https%3A%2F%2Fexample.org%2Frail%23HAL \
  -H 'Content-Type: application/json' -d '{"code": 5}'
```

<!-- test: output contains -->
```text
{"errors":[{"key":"code","message":"Expects a string","value":5}],"status":422
```

The generated shapes describe the data as it is, so they only decide how values are read. Once
the dataset has shapes of its own, every write is checked against the record's shape first:
`sh:minCount`, `sh:in`, `sh:class` and the rest of SHACL, with your `sh:message`s. A record
comes with an `ETag`: send it back as `If-Match`, and the write is refused (412) if someone
changed the record since you read it. A tag that is no longer the record's is refused:

<!-- test: run -->
```sh
curl -s -o /dev/null -w '%{http_code}\n' -X PATCH \
  http://localhost:7878/ds/api/v1/Station/https%3A%2F%2Fexample.org%2Frail%23HAL \
  -H 'Content-Type: application/json' -H 'If-Match: "0000000000000000"' -d '{"label": "Hal"}'
```

<!-- test: output -->
```text
412
```

`?dry=true` runs every check of a write and writes nothing, so a form can show what is wrong
before it saves. A record without `id` gets one: from the shape's `tts:iriTemplate` (such as
`https://example.org/rail#{code}`, where `{code}` is the new record's code and `{uuid}` a
random UUID), else `urn:uuid:` and a random UUID:

<!-- test: run -->
```sh
curl -s -X POST 'http://localhost:7878/ds/api/v1/Station?dry=true' \
  -H 'Content-Type: application/json' -d '{"label": "Vilvoorde", "code": "VV"}' | grep -o '"dry":true'
```

<!-- test: output -->
```text
"dry":true
```

A dry `DELETE` also says how many links to the record would be left (`referencedBy`).

A value that is a blank node of a class with a shape, such as an address
(`ex:address [ a ex:Address ; ex:city "Gent" ]`), comes inside the record as an object with
that shape's keys: `"address": {"type": "…Address", "city": "Gent"}`. Write it the same way:
an object without an `id` becomes a new blank node, and the one it replaces goes, with
everything below it.

## GraphQL

The same lists and records answer GraphQL at `/ds/graphql`, for front ends that ask for
exactly the keys they show. Each class is a list (`stations`) and a field for one record
(`station(id: …)`); a list takes `first` and `after` to page, `filter` on keys, `q` and
`bbox`, as the REST API does:

<!-- test: run -->
```sh
curl -s http://localhost:7878/ds/graphql -H 'Content-Type: application/json' \
  -d '{"query": "{ stations(filter: {code: \"BXS\"}) { nodes { label code } } }"}'
```

<!-- test: output -->
```text
{"data":{"stations":{"nodes":[{"label":"Brussels-South","code":"BXS"}]}}}
```

A reference is an object: ask for its keys, and TripleTrail reads the records it refers to,
one query per level however many records the list has. Open
[http://localhost:7878/ds/graphql](http://localhost:7878/ds/graphql) in a browser for the
playground: the schema on the left, where **Use as query** writes a query for a list or a
record, an editor, and the answer (**Run**, or Ctrl+Enter). A request has the query timeout as a whole and reads at most 500,000 values,
so a query that follows references back and forth many levels deep gets an error rather than
a huge answer: ask for fewer records or levels.

<!-- test: run -->
```sh
curl -s -o /dev/null -D - -H 'Accept: text/html' http://localhost:7878/ds/graphql | grep -i '^location'
```

<!-- test: output contains -->
```text
/$/ui/graphql/ds
```

### Write records with mutations

Each class also has three mutations, which write as the REST API does: `createStation`,
`updateStation` (only the keys you give; `null` removes a key's values) and `deleteStation`.
A record is a `JSON` value with the REST API's keys, so a key that is not a GraphQL name
still works. A mutation is sent with `POST`, and needs the same rights as a SPARQL update:

<!-- test: run -->
```sh
curl -s http://localhost:7878/ds/graphql -H 'Content-Type: application/json' -d '{
  "query": "mutation($r: JSON!) { createStation(record: $r) { id label code } }",
  "variables": {"r": {"id": "https://example.org/rail#LUX", "label": "Luxembourg", "code": "LUX"}}}'
```

<!-- test: output -->
```text
{"data":{"createStation":{"id":"https://example.org/rail#LUX","label":"Luxembourg","code":"LUX"}}}
```

<!-- test: run -->
```sh
curl -s http://localhost:7878/ds/graphql -H 'Content-Type: application/json' \
  -d '{"query": "mutation { deleteStation(id: \"https://example.org/rail#LUX\") }"}'
```

<!-- test: output -->
```text
{"data":{"deleteStation":"https://example.org/rail#LUX"}}
```

Create and update answer the record as it now is, with the keys you ask for. Before anything is
written, the record is checked against the dataset's own shapes (see
[Make it yours](#make-it-yours-shapes)): one that breaks them changes nothing, and the error
names each key and what is wrong with it (`tracks: A station has at least one track`), with
the full violations under `extensions.violations`.

## From JavaScript

For a web page or a Node service that speaks SPARQL, the repository has a small client in
`clients/js`, with no dependencies: queries (`select`, `ask`, `construct`), updates, the
Graph Store Protocol (`graphs.get`, `put`, `post`, `delete`) and the admin API. It is on npm:
`npm install @matdata/tripletrail-client`, then import from `"@matdata/tripletrail-client"`.
This script, run from a checkout, imports it by its path instead, and lists the stations with
their codes, as plain values:

<!-- test: file stations.mjs -->
```js
import { TripleTrail, toValues } from "./clients/js/src/index.js";

const rail = new TripleTrail("http://localhost:7878", { dataset: "ds" });
const result = await rail.select(`
  PREFIX ex: <https://example.org/rail#>
  PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
  SELECT ?code ?label { ?s a ex:Station ; ex:code ?code ; rdfs:label ?label }
  ORDER BY ?code LIMIT 3`);
for (const { code, label } of toValues(result)) console.log(code, label);
```

`node stations.mjs` prints (the client's tests run this script against the sample data):

<!-- output of stations.mjs: checked by the tests in clients/js -->
```text
ANT Antwerp-Central
BXC Brussels-Central
BXN Brussels-North
```

Give `token` (sent as `Authorization: Bearer`) or `apiKey` (`X-API-Key`) to a dataset that
needs one. A refused request throws a `TripleTrailError` with the HTTP `status` and the server's
message, and `toValue` turns a term into a number, a boolean or text. A web page served from another
origin can call the server only when it was started with `--cors <origin>` (the page's
origin, such as `--cors https://app.example`). See
[the JavaScript client](../library.md#javascript-client).

## AI agents: MCP

An AI agent or assistant that speaks the [Model Context Protocol](https://modelcontextprotocol.io)
can read a dataset through tools instead of guessing SPARQL endpoints. Start the server with
`--mcp`, and each dataset answers MCP at `/ds/mcp` (streamable HTTP, JSON-RPC over `POST`):

<!-- test: serve -->
```sh
tripletrail serve --load network.ttl --mcp
```

Give your MCP client that URL, `http://localhost:7878/ds/mcp`, with an API key or token header
when the dataset needs one. It lists the tools:

<!-- test: run -->
```sh
curl -s http://localhost:7878/ds/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc": "2.0", "id": 1, "method": "tools/list"}' | grep -o '"name":"[a-z_]*"'
```

<!-- test: output -->
```text
"name":"sparql_query"
"name":"describe"
"name":"search"
"name":"list_shapes"
"name":"shape"
"name":"list_datasets"
```

`list_shapes` and `shape` tell the agent what the data looks like (the dataset's shapes, or the
ones TripleTrail generated), `search` finds resources by their labels, `describe` gives the
triples about one resource, and `sparql_query` runs a query:

<!-- test: run -->
```sh
curl -s http://localhost:7878/ds/mcp -H 'Content-Type: application/json' -d '{
  "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "sparql_query",
  "arguments": {"query": "PREFIX ex: <https://example.org/rail#> SELECT ?code { ?s ex:code ?code } ORDER BY ?code LIMIT 2"}}}' \
  | grep -o '\\"value\\":\\"[A-Z]*\\"'
```

<!-- test: output -->
```text
\"value\":\"ANT\"
\"value\":\"BXC\"
```

The tools only read: an update is refused. They keep to what the caller may read, as a query
does (the [graphs and rows](12-access-control.md) of their key or token), with the server's
query timeout. An answer holds at most 1,000 rows, and says so when there were more. The
endpoint is off unless you ask for it. See [MCP](../server.md#mcp).

## Make it yours: shapes

The map, the describe pages and both APIs read the dataset's shapes:
`/$/datasets/ds/shapes` shows the ones TripleTrail generated. Generated shapes use the data's own
names (`Section`, `maxSpeed`). A dataset can have shapes of its own, written in SHACL with a
few `tts:` annotations, to choose the classes, their names and keys, colours, which
properties are filters, the order of rows on a page and units. Develop them in the
[Workbench](09-workbench.md) and **Promote** them to the dataset, or keep them in a git
repository the server pulls from. See [shapes](../server.md#shapes).

### Shape packs

Shapes written once for a kind of data (a rail standard, your company's model) can be
installed on every dataset that holds it, as a **shape pack**. The pack goes on top of the
generated shapes, and the dataset's own shapes on top of the pack, so one dataset can still
change a class the pack describes. Packs are kept with the dataset's files, so the server
needs a `--location`:

<!-- test: run -->
```sh
tripletrail load --location db --dataset ds network.ttl
```

<!-- test: serve -->
```sh
tripletrail serve --location db --dataset ds
```

A pack has a name, a version and its Turtle files. This one draws stations in red. Its shape
`tts:refines` the generated one for stations, which keeps the geometry and facets it found;
without that, it would replace it:

<!-- test: file pack.json -->
```json
{"name": "rail", "version": "1.0",
 "files": {"Station.ttl": "@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix tts: <http://w3id.org/tripletrail/shapes#> . <https://example.org/pack/rail#Station> a sh:NodeShape ; sh:targetClass <https://example.org/rail#Station> ; tts:refines true ; tts:color \"#d62728\" ."}}
```

<!-- test: run -->
```sh
curl -s -X PUT 'http://localhost:7878/$/datasets/ds/shapes/pack' --data-binary @pack.json
echo
curl -s -D - -o /dev/null 'http://localhost:7878/$/datasets/ds/shapes' | grep -i '^tripletrail-shapes'
```

<!-- test: output -->
```text
{"files":1,"name":"rail","replaced":null,"version":"1.0"}
TripleTrail-Shapes: pack rail@1.0
```

The Map page now draws stations in red. Installing version `1.1` later replaces the pack and
keeps the dataset's own shapes, and `DELETE` on the same address removes it.

A pack can keep the generated layers off the map. The generated shapes add one for every
other class with a geometry: in RINF, the references and coordinates between the features. A
set that names `tts:Reader-Map` with `tts:hidesGenerated`, about itself (a `tts:ShapeSet`),
shows only its own classes on the map, and the OGC API and describe pages still have the
others. A generated shape that the set (or the dataset's own set) overrides or refines stays.
Version 1.1 of the pack, with that one more file, draws the stations alone, while the OGC API still serves
the sections:

<!-- test: file only-pack.json -->
```json
{"name": "rail", "version": "1.1",
 "files": {"_set.ttl": "@prefix tts: <http://w3id.org/tripletrail/shapes#> . <https://example.org/pack/rail> a tts:ShapeSet ; tts:hidesGenerated tts:Reader-Map .",
           "Station.ttl": "@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix tts: <http://w3id.org/tripletrail/shapes#> . <https://example.org/pack/rail#Station> a sh:NodeShape ; sh:targetClass <https://example.org/rail#Station> ; tts:refines true ; tts:color \"#d62728\" ."}}
```

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/ds/tiles/style.json' | grep -o '"tripletrail:collections":.*' | grep -o '"id":"[^"]*"'
curl -s -X PUT 'http://localhost:7878/$/datasets/ds/shapes/pack' --data-binary @only-pack.json > /dev/null
echo after:
curl -s 'http://localhost:7878/ds/tiles/style.json' | grep -o '"tripletrail:collections":.*' | grep -o '"id":"[^"]*"'
echo OGC API:
curl -s 'http://localhost:7878/ds/ogc/collections' | grep -o '"id":"Section"' | head -1
```

<!-- test: output -->
```text
"id":"Section"
"id":"Station"
after:
"id":"Station"
OGC API:
"id":"Section"
```

A pack usually lives in a git repository, released with tags. Give the repository and the tag
instead of the files: the pack is named after the repository and versioned by the tag (or
give `name` and `version`). Here the repository is a local folder; an `https://` URL works
the same way, with a `tokenFile` for a private one. With [access control](12-access-control.md),
installing from git is for server admins (the group `tripletrail:admin`), as it reads files and
URLs of the server; a dataset's admins install a pack by its files. Version 2.0 draws stations
in black:

<!-- test: file rail-pack/Station.ttl -->
```turtle
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix tts: <http://w3id.org/tripletrail/shapes#> .

<https://example.org/pack/rail#Station> a sh:NodeShape ;
  sh:targetClass <https://example.org/rail#Station> ;
  tts:refines true ;
  tts:color "#000000" .
```

<!-- test: run -->
```sh
git init -q rail-pack
git -C rail-pack add Station.ttl
git -C rail-pack -c user.name=me -c user.email=me@example.org commit -qm 'Rail pack'
git -C rail-pack tag v2.0
curl -s -X PUT 'http://localhost:7878/$/datasets/ds/shapes/pack' \
  -d "{\"git\": {\"url\": \"file://$PWD/rail-pack\", \"tag\": \"v2.0\"}}"
```

<!-- test: output -->
```text
{"files":1,"name":"rail-pack","replaced":"rail@1.1","version":"v2.0"}
```

### The Shapes page

The **Shapes** page (`/$/ui/shapes/ds`) is where a dataset's shapes are changed without
writing Turtle. It lists the node shapes, with a search box, and marks where each comes from:
`generated` from the data, from the `pack`, or the dataset's `own`, and `overrides` when an
own shape replaces a lower one (`refines` when it only changes it). Pick a shape to see its property shapes in a table, and change
what the readers read:

- **Name** (per language, chosen above the table), **Order**, **Group** and **Key**.
- **Hidden** (describe pages leave the row out), **Hidden from** (the readers that leave the
  property out: map, describe, search, the REST API…) and **Deactivated**. A subclass's shape
  inherits the property for the readers it is not hidden from.
- The shape's own name and key, the readers it is hidden from or only for (a shape hidden from
  the map is no layer of its own, but its properties still reach its subclasses' layers, so an
  abstract *Asset* shape can carry the owner facet of signals and switches), and its
  **Profiles**: the use cases it belongs to. A name typed under **New profile** makes one and puts the shape in
  it. A shape in no profile is in all of them, and the box above the list shows only the
  shapes of one profile.

The page's link names the shape shown (`?shape=IRI`), so **Share** in the top bar copies a
link that opens the same shape.

**Diagram** draws the shape with its properties, the shapes they link to (by `sh:class`) on
the right and the shapes that link to it on the left; pick one to show it.

**All settings…** beside the shape's settings, or a property's name in the table, opens a form
with everything the page can change: also descriptions, the map colour, the zoom it shows
from and its draw order, and a property's viewer, facet and time. The forms are built from
editor shapes the server ships, which the page reads from `…/shapes/form`:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/$/datasets/ds/shapes/form' | grep -o '"label":"Map colour"'
```

<!-- test: output -->
```text
"label":"Map colour"
```

**Preview the Turtle** shows the files a save would write, **Preview on map** opens the
Map page in a new tab drawn with the unsaved changes (for an hour, saving nothing), **Preview
a describe page** does the same for the page of one of the shape's instances, **Preview
search** and **Preview the REST API** for the Search page and the API's docs page, and **Save changes** keeps them as
the next version of the dataset's own set. A shape that is generated or from the pack gets a file
of its own that refines it (`tts:refines true`): it holds only what you changed, so the rest of
the shape, and properties the data gains later, still come from below. Every save is checked first, and
**Versions** lists the kept versions, each with a rollback. The Turtle of an own file can be
edited on the page as well, checked as you type. When the shapes come from a git repository,
the page saves nothing: change the repository, or download the changed files with **Export
changes**. With TripleTrail Enterprise, the changes can instead be proposed with **Propose…**,
which asks for a title and a description and can first show what the changes do: the server commits
them on a new branch of the repository and opens a pull request (a merge request on GitLab),
whose review decides, and the merge goes live through the push webhook. The pull request
says what each reader (the map, describe pages, search, the APIs, validation) and each
profile would read differently, and the server validates all of the data with the proposed
shapes and comments on the pull request with how many results are new or gone, by shape
(the results themselves, which are data, only when the binding allows it)
(`POST …/shapes/proposals`, see [the reference](../server.md#proposed-shape-changes-tripletrail-enterprise)).
**Proposals** lists what was proposed, with a link to each pull request, whether it is still
open, merged or closed (asked of the repository when the list opens), and how its validation went.
Proposing is for the dataset's admins and for the people or groups named in its
`shapeEditors` in the access configuration, so modellers can propose without being able to
change the server, and the pull request's reviewers approve.

The page saves through the shapes API, which a script can call too. This renames the pack's
stations in English:

<!-- test: file edit.json -->
```json
{"changes": [{"shape": "https://example.org/pack/rail#Station", "setting": "name",
              "value": "Railway station", "lang": "en"}]}
```

<!-- test: run -->
```sh
curl -s -X POST 'http://localhost:7878/$/datasets/ds/shapes/edit' \
  -H 'Content-Type: application/json' --data-binary @edit.json
echo
curl -s -D - -o /dev/null 'http://localhost:7878/$/datasets/ds/shapes' | grep -i '^tripletrail-shapes'
```

<!-- test: output -->
```text
{"changed":["Station.ttl"],"version":1}
TripleTrail-Shapes: pack rail-pack@v2.0; local; version=1
```

The file it wrote holds the new name and `tts:refines`, and nothing else of the pack's shape:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/$/datasets/ds/shapes/files/Station.ttl' | grep -c 'refines\|Railway station'
```

<!-- test: output -->
```text
2
```

A profile is made in the same request that puts a shape in it. Readers then take `profile=`
to read only that use case's shapes (and those in no profile):

<!-- test: file profile.json -->
```json
{"profiles": [{"iri": "urn:tripletrail:profile:maintenance", "label": "Maintenance"}],
 "changes": [{"shape": "https://example.org/pack/rail#Station", "setting": "profiles",
              "value": ["urn:tripletrail:profile:maintenance"]}]}
```

<!-- test: run -->
```sh
curl -s -X POST 'http://localhost:7878/$/datasets/ds/shapes/edit' \
  -H 'Content-Type: application/json' --data-binary @profile.json
echo
curl -s 'http://localhost:7878/$/datasets/ds/shapes/outline' | grep -o '"profiles":\[{[^]]*\]'
```

<!-- test: output -->
```text
{"changed":["Station.ttl","profiles.ttl"],"version":2}
"profiles":[{"iri":"urn:tripletrail:profile:maintenance","label":"Maintenance","shapes":1}]
```

### Check a pack before you upgrade

Before installing a new release of a pack, ask what it would do: the same `PUT` with
`?dry=true` installs nothing and answers a report. Release 3.0 of the rail pack renames the
station shape to `Stop` and adds one for platforms:

<!-- test: file pack-3.json -->
```json
{"name": "rail-pack", "version": "v3.0",
 "files": {"Stop.ttl": "@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix tts: <http://w3id.org/tripletrail/shapes#> . <https://example.org/pack/rail#Stop> a sh:NodeShape ; sh:targetClass <https://example.org/rail#Station> ; tts:color \"#000000\" .",
           "Platform.ttl": "@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix tts: <http://w3id.org/tripletrail/shapes#> . <https://example.org/pack/rail#Platform> a sh:NodeShape ; sh:targetClass <https://example.org/rail#Platform> ; tts:color \"#9467bd\" ."}}
```

<!-- test: run -->
```sh
curl -s -X PUT 'http://localhost:7878/$/datasets/ds/shapes/pack?dry=true' --data-binary @pack-3.json
```

<!-- test: output contains -->
```text
"added":["https://example.org/pack/rail#Platform","https://example.org/pack/rail#Stop"]
"noData":[{"class":"https://example.org/rail#Platform","shape":"https://example.org/pack/rail#Platform"}]
"orphaned":[]
"overrides":[{"how":"refines","packShape":"https://example.org/pack/rail#Stop","shape":"<https://example.org/pack/rail#Station>"}]
"removed":["https://example.org/pack/rail#Station"]
"replaces":"rail-pack@v2.0"
```

The dataset's `Station.ttl` refines the pack's station shape. It still meets the renamed
shape, by the stations' class, so `overrides` lists it on `Stop`: the stations keep the name
"Railway station". An override that would meet nothing any more is listed under `orphaned`
instead: after the upgrade it would stand alone. `noData` lists the release's shapes for
classes the data has no instances of: there are no platforms here.

The same check runs without a server, on the pack's folder, the release installed now and
the own set, with the data files. A release with platforms only would leave the station
override alone, and `--strict` then exits with status 1, which suits a CI job of the pack's
repository:

<!-- test: run -->
```sh
mkdir -p v4 own
echo '@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix tts: <http://w3id.org/tripletrail/shapes#> . <https://example.org/pack/rail#Platform> a sh:NodeShape ; sh:targetClass <https://example.org/rail#Platform> ; tts:color "#9467bd" .' > v4/Platform.ttl
curl -s 'http://localhost:7878/$/datasets/ds/shapes/files/Station.ttl' > own/Station.ttl
tripletrail shapes check --pack v4 --installed rail-pack --own own --data network.ttl --strict 2>/dev/null \
  || echo "exit $?"
```

<!-- test: output -->
```text
Shapes added: 1
  <https://example.org/pack/rail#Platform>
Shapes removed: 1
  <https://example.org/pack/rail#Station>
Shapes changed: 0
Own overrides kept: 0
Own overrides that would stand alone: 1
  <https://example.org/pack/rail#Station> on <https://example.org/pack/rail#Station> (refines)
Shapes for classes without instances: 1
  <https://example.org/pack/rail#Platform> (class <https://example.org/rail#Platform>)
exit 1
```

A server can also install a pack as it starts: `--pack DIR` puts the pack in that folder on
the first `--dataset` (or `--pack NAME=DIR` on another), unless that release is installed
already. The folder's `pack.json` names the release; without one, the pack takes the folder's
name and the version `local`:

<!-- test: run -->
```sh
echo '{"name": "rail-pack", "version": "v4.0"}' > v4/pack.json
```

<!-- test: serve -->
```sh
tripletrail serve --location db --dataset ds --pack v4
```

<!-- test: run -->
```sh
curl -s -D - -o /dev/null 'http://localhost:7878/$/datasets/ds/shapes' | grep -i '^tripletrail-shapes'
```

<!-- test: output -->
```text
TripleTrail-Shapes: pack rail-pack@v4.0; local; version=2
```

### Draw a line between two features

Some features have no geometry of their own, only links to features that have one: a section
of line runs from one station to another. A geometry way with `tts:lineBetween` draws them as
the straight line between the two linked geometries. Two stations and a connection between them:

<!-- test: file connection.ttl -->
```turtle
@prefix rail: <https://example.org/rail#> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .

rail:north a rail:Station ; geo:hasGeometry [ geo:asWKT "POINT(4.36 50.86)"^^geo:wktLiteral ] .
rail:south a rail:Station ; geo:hasGeometry [ geo:asWKT "POINT(4.34 50.83)"^^geo:wktLiteral ] .
rail:c1 a rail:Connection ; rail:from rail:north ; rail:to rail:south .
```

The connection's shape tries its own geometry first, then the line from `rail:from` to
`rail:to`:

<!-- test: file Connection.ttl -->
```turtle
@prefix rail: <https://example.org/rail#> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix tts: <http://w3id.org/tripletrail/shapes#> .

rail:ConnectionShape a sh:NodeShape ;
  sh:targetClass rail:Connection ;
  tts:key "connection" ;
  tts:geometry [ sh:order 0 ; sh:path ( geo:hasGeometry geo:asWKT ) ] ,
    [ sh:order 1 ;
      tts:lineBetween ( ( rail:from geo:hasGeometry geo:asWKT )
                        ( rail:to geo:hasGeometry geo:asWKT ) ) ] .
```

<!-- test: run -->
```sh
curl -s -X POST 'http://localhost:7878/ds/data' -H 'Content-Type: text/turtle' --data-binary @connection.ttl -o /dev/null
curl -s -X PUT 'http://localhost:7878/$/datasets/ds/shapes/files/Connection.ttl' --data-binary @Connection.ttl -o /dev/null
curl -s 'http://localhost:7878/ds/ogc/collections/connection/items?f=json' | grep -o '"geometry":{[^}]*}'
```

<!-- test: output -->
```text
"geometry":{"coordinates":[[4.36,50.86],[4.34,50.83]],"type":"LineString"}
```

The map, the vector tiles, the REST API and search draw and filter the connection by that line. A
connection with a geometry of its own keeps it.

### Name views of the map

A map view (`tts:MapView`) is a named lens on the map, picked from the **View** list above its
layers: the classes it shows (the other layers are hidden), the filters it sets, the facet it
colours by and the facets its filter panel lists first. Readers who don't know the data start from a view instead of an empty set of
filters. The fast sections, coloured by their speed:

<!-- test: file views.ttl -->
```turtle
@prefix ex: <https://example.org/rail#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix tts: <http://w3id.org/tripletrail/shapes#> .

ex:fastLines a tts:MapView ;
  tts:key "fast" ;
  rdfs:label "Fast lines" ;
  rdfs:comment "Sections where trains run at 160 km/h or more." ;
  sh:order 1 ;
  tts:showsClass ex:Section ;
  tts:filter [ tts:key "maxSpeed" ; sh:in ( 160 300 ) ] ;
  tts:colorFacet "maxSpeed" ;
  tts:listsFacets ( "maxSpeed" "electrified" ) .
```

A filter names a facet by its key, with `sh:in ( … )` for the values kept or, on a range
facet, `sh:minInclusive` and `sh:maxInclusive`. `tts:listsFacets` names the facets the
filter panel shows at the top, in that order, also on a layer the view hides once you show it.
A filter, colour or listed facet naming a facet the class doesn't have is left out for that
class. The map reads its views from its style:

<!-- test: run -->
```sh
curl -s -X PUT 'http://localhost:7878/$/datasets/ds/shapes/files/views.ttl' --data-binary @views.ttl -o /dev/null
curl -s 'http://localhost:7878/ds/tiles/style.json' \
  | grep -o -e '"colors":{[^}]*}' -e '"facets":{[^}]*}' -e '"filters":{"Section":{[^}]*}' \
    -e '"key":"fast","title":"[^"]*"'
```

<!-- test: output -->
```text
"colors":{"Section":"maxSpeed"}
"facets":{"Section":["maxSpeed","electrified"]}
"filters":{"Section":{"maxSpeed":["160","300"]}
"key":"fast","title":"Fast lines"
```

Picking the view puts its layers, filters and colours in the page's link with `view=fast`, so
the link can be shared like any other; changing a filter afterwards keeps the view's name. A
view shows only the layers a reader may see, and one that shows none of them is not listed.
`sh:deactivated true` switches a view off.

### Compute a facet from the data

A facet can take its value from a SPARQL pattern instead of a path (`tts:select`): a value
the data doesn't hold but implies, such as whether a section meets a requirement. The pattern
binds `?value` for the feature `$this`, written with full IRIs; its other variables stay inside
it. The sections, as fast or slow:

<!-- test: file speed.ttl -->
```turtle
@prefix ex: <https://example.org/rail#> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix tts: <http://w3id.org/tripletrail/shapes#> .

ex:SectionSpeedShape a sh:NodeShape ;
  sh:targetClass ex:Section ;
  tts:key "Section" ;
  tts:geometry [ sh:order 0 ; sh:path ( geo:hasGeometry geo:asWKT ) ] ;
  sh:property [
    sh:path ex:maxSpeed ;
    tts:key "speedClass" ;
    sh:name "Speed class" ;
    tts:facet tts:Facet-Values ;
    tts:onlyFor tts:Reader-Map ;
    tts:select """
      OPTIONAL { $this <https://example.org/rail#maxSpeed> ?speed }
      BIND(IF(!BOUND(?speed), "unknown", IF(?speed >= 160, "fast", "slow")) AS ?value)
    """
  ] .
```

<!-- test: run -->
```sh
curl -s -X PUT 'http://localhost:7878/$/datasets/ds/shapes/files/speed.ttl' --data-binary @speed.ttl -o /dev/null
curl -s 'http://localhost:7878/ds/tiles/facets/Section' | grep -o '"key":"speedClass"[^]]*]'
```

<!-- test: output -->
```text
"key":"speedClass","more":false,"title":"Speed class","values":[{"count":7,"label":null,"value":"fast"},{"count":5,"label":null,"value":"slow"}]
```

The map counts it, filters on it and colours by it like any other facet. The pattern runs
when the map asks, for each feature it draws or counts, so keep it to the feature and its
neighbours. Its count gets at most 10 seconds: a computed facet that fails (too slow for the
data) comes without values while the others still show; after a timeout it is not counted
again until the data or the shapes change. It is no property of the features in the OGC API, REST or
GraphQL: `tts:onlyFor tts:Reader-Map` keeps the other readers from the path it names.

The pattern is checked when the shapes are saved: it must parse, bind `?value`
and call no `SERVICE` but TripleTrail's own (a map must not wait on another endpoint). A pattern
that binds no `?value` is refused:

<!-- test: run -->
```sh
sed -e 's/ AS ?value)/ AS ?class)/' -e 's/SectionSpeedShape/WrongShape/' \
  -e 's/"Section"/"Wrong"/' speed.ttl > wrong.ttl
curl -s -X PUT 'http://localhost:7878/$/datasets/ds/shapes/files/wrong.ttl' --data-binary @wrong.ttl
```

<!-- test: output -->
```text
The shapes are not valid:
<https://example.org/rail#WrongShape>: the facet "speedClass"'s tts:select binds no ?value
```

### Keep a property from the record forms

The forms that show and edit one record read the shapes as a reader of their own,
`tts:Reader-Forms` (`forms`), so `tts:hiddenFrom` and `tts:onlyFor` work on them as on the map
or describe pages. The speed class above is only for the map, so the forms leave it out, and a
property hidden from the forms stays on the map:

<!-- test: run -->
```sh
for reader in map forms; do
  curl -s "http://localhost:7878/\$/datasets/ds/shapes?reader=$reader" | grep -q speedClass \
    && echo "$reader: speed class" || echo "$reader: no speed class"
done
```

<!-- test: output -->
```text
map: speed class
forms: no speed class
```

The forms draw each field with the editor its shape names (`dash:editor`, such as
`dash:TextAreaEditor` or `dash:EnumSelectEditor`), or the one that fits its values: a check
box for a boolean, a list for `sh:in`, a date picker for a date. The web UI's pages share one
renderer for this, `/$/ui/forms.js`; see [the reference](../server.md#shapes).

### Show data quality on the map

A SHACL validation report of the data ([chapter 6](06-validation-and-reasoning.md)), stored in
a named graph, can be a facet of every layer: the worst severity of the report's results about
each feature, `violation`, `warning` or `info`, else `ok`. The map counts it, filters on it and
colours by it like any other facet, and a panel of several layers shares it. Store the report,
then name its graph in the shapes with `tts:qualityReport`:

<!-- test: file report.ttl -->
```turtle
@prefix ex: <https://example.org/rail#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .

[] a sh:ValidationReport ; sh:conforms false ;
  sh:result [ sh:focusNode ex:S01 ; sh:resultSeverity sh:Violation ] ,
    [ sh:focusNode ex:S02 ; sh:resultSeverity sh:Warning ] .
```

<!-- test: run -->
```sh
curl -s -X PUT 'http://localhost:7878/ds/data?graph=urn:report' -H 'Content-Type: text/turtle' --data-binary @report.ttl -o /dev/null
echo '@prefix tts: <http://w3id.org/tripletrail/shapes#> . <urn:my:set> a tts:ShapeSet ; tts:qualityReport <urn:report> .' > quality.ttl
curl -s -X PUT 'http://localhost:7878/$/datasets/ds/shapes/files/_set.ttl' --data-binary @quality.ttl -o /dev/null
curl -s 'http://localhost:7878/ds/tiles/facets/Section' | grep -o '"key":"quality"[^]]*]'
```

<!-- test: output -->
```text
"key":"quality","more":false,"title":"Data quality","values":[{"count":10,"label":null,"value":"ok"},{"count":1,"label":null,"value":"violation"},{"count":1,"label":null,"value":"warning"}]
```

It is computed on the map only, as a `tts:select` facet: the OGC API and the other readers
have no such property. A class whose shape has a facet of key `quality` keeps its own.

### Filter a line by its tracks

Some properties belong to the parts of a feature: a section of line has tracks, and each
track has its own ETCS level and speed. A shape can roll its facets up to the parent with
`tts:rollsUpTo`, the path from a part to its parent and the parent's class. The map then gives
the parent the parts' facets it lacks: a section matches a filter when one of its tracks does,
counts once per value of its tracks, and is coloured by its fastest track. Tracks of two
sections:

<!-- test: file tracks.ttl -->
```turtle
@prefix rail: <https://example.org/rail#> .

rail:T1 a rail:Track ; rail:isPartOf rail:S03 ; rail:etcs "2" ; rail:trackSpeed 160 .
rail:T2 a rail:Track ; rail:isPartOf rail:S03 ; rail:etcs "1" ; rail:trackSpeed 140 .
rail:T3 a rail:Track ; rail:isPartOf rail:S05 ; rail:etcs "1" ; rail:trackSpeed 120 .
```

The tracks' shape names their section through `rail:isPartOf`:

<!-- test: file Track.ttl -->
```turtle
@prefix rail: <https://example.org/rail#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix tts: <http://w3id.org/tripletrail/shapes#> .

rail:TrackShape a sh:NodeShape ;
  sh:targetClass rail:Track ;
  tts:rollsUpTo [ sh:path rail:isPartOf ; sh:class rail:Section ] ;
  sh:property [ sh:path rail:etcs ; tts:key "etcs" ; sh:name "ETCS level" ;
                tts:facet tts:Facet-Values ] ,
    [ sh:path rail:trackSpeed ; tts:key "trackSpeed" ; tts:facet tts:Facet-Range ] .
```

<!-- test: run -->
```sh
curl -s -X POST 'http://localhost:7878/ds/data' -H 'Content-Type: text/turtle' --data-binary @tracks.ttl -o /dev/null
curl -s -X PUT 'http://localhost:7878/$/datasets/ds/shapes/files/Track.ttl' --data-binary @Track.ttl -o /dev/null
curl -s 'http://localhost:7878/ds/tiles/facets/Section' | grep -o '"key":"etcs"[^]]*]'
curl -s 'http://localhost:7878/ds/tiles/facets/Section?etcs=2' | grep -o '"features":[0-9]*' | head -1
```

<!-- test: output -->
```text
"key":"etcs","more":false,"title":"ETCS level","values":[{"count":2,"label":null,"value":"1"},{"count":1,"label":null,"value":"2"}]
"features":1
```

S03 counts for both levels, S05 for level 1, and the filter on level 2 keeps S03. A filter
scoped to the tracks (`Track.etcs=2` on a map of several layers) leaves the sections alone. The
rolled-up facets are the map's only: the OGC API's sections have no `etcs`. A facet computed
with `tts:select` stays with the parts.

### Show the features of a time

Features with a time, such as works on the network with a start and an end, can be shown for
a day or a span. The data:

<!-- test: file works.ttl -->
```turtle
@prefix ex: <https://example.org/rail#> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .

ex:W1 a ex:Works ; ex:start "2026-03-01"^^xsd:date ; ex:end "2026-04-15"^^xsd:date ;
  geo:hasGeometry [ geo:asWKT "POINT(4.3606 50.8603)"^^geo:wktLiteral ] .
ex:W2 a ex:Works ; ex:start "2026-06-01"^^xsd:date ;
  geo:hasGeometry [ geo:asWKT "POINT(4.4824 51.0177)"^^geo:wktLiteral ] .
```

and a shape that says which properties are the time, with `tts:time` (`tts:Time-Start` and
`tts:Time-End`, or `tts:Time-Instant` for a moment):

<!-- test: file works-shape.ttl -->
```turtle
@prefix ex: <https://example.org/rail#> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix tts: <http://w3id.org/tripletrail/shapes#> .

ex:WorksShape a sh:NodeShape ;
  sh:targetClass ex:Works ;
  tts:key "Works" ;
  tts:geometry [ sh:order 0 ; sh:path ( geo:hasGeometry geo:asWKT ) ] ;
  sh:property [ sh:path ex:start ; tts:key "start" ; tts:time tts:Time-Start ] ,
    [ sh:path ex:end ; tts:key "end" ; tts:time tts:Time-End ] .
```

The map's tiles and counts then take `datetime`, as the OGC API's items do: a day, or a span
`start/end` with `..` for an open end. On 1 April only the first works are on, from 1 May
none, and in July the second (it has no end):

<!-- test: run -->
```sh
curl -s -X POST 'http://localhost:7878/ds/data' -H 'Content-Type: text/turtle' --data-binary @works.ttl -o /dev/null
curl -s -X PUT 'http://localhost:7878/$/datasets/ds/shapes/files/works.ttl' --data-binary @works-shape.ttl -o /dev/null
for day in 2026-04-01 2026-05-01 2026-07-01; do
  echo "$day $(curl -s "http://localhost:7878/ds/tiles/facets/Works?datetime=$day" | grep -o '"features":[0-9]*')"
done
```

<!-- test: output -->
```text
2026-04-01 "features":1
2026-05-01 "features":0
2026-07-01 "features":1
```

On the Map page, the clock button above the zoom shows the time: two sliders over the span of
the layers' times, or a date typed, or **One day**. The map, its counts, the OGC API links and
the page's link follow it (`datetime=` in the link). Layers without a `tts:time` keep all of
their features.

### Export and cite the map

The Map page's download button makes a PNG of the map as it is: the map with a scale bar, a
north arrow turned with it, the legend of the layers shown, and under it the base map's credit,
the dataset, its data version and the time shown. Give it a title, or keep the dataset's name.

**Cite this map**, the button beside it, gives a text to paste into a report or a paper: the
dataset and its data version (the store's write generation, and the version of its own shapes
when it has some, from `metadata."tripletrail:dataVersion"` of `/{ds}/tiles/style.json`), the
day, the link that opens the same map again, the time shown, and for every layer shown its OGC
API items and the SPARQL that picks its features under the filters, so a reader can run the
same query. A later write to the dataset changes its data version: the citation says which
data the map showed.

### Start a pack from RINF Atlas shapes

If you draw your data with [RINF Atlas](https://rinf-atlas.matdata.eu), its shapes (SHACL with
`atlas:` annotations) can start a pack: `tripletrail shapes import-atlas` writes them in TripleTrail's
`tts:` terms, one file per class. A shape for operational points:

<!-- test: file atlas/operational-point.ttl -->
```turtle
PREFIX atlas: <https://rinf-atlas.matdata.eu/ns#>
PREFIX sh: <http://www.w3.org/ns/shacl#>
PREFIX era: <http://data.europa.eu/949/>
PREFIX gsp: <http://www.opengis.net/ont/geosparql#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>

<https://example.org/atlas/operational-point> a sh:NodeShape ;
  atlas:key "operational-point" ;
  sh:targetClass era:OperationalPoint ;
  atlas:color "#d62728" ;
  atlas:minZoom 0 ;
  atlas:order 10 ;
  atlas:geometryPath ( gsp:hasGeometry gsp:asWKT ) ;
  atlas:geometry "point" ;
  sh:property [ sh:path era:opName ; atlas:facet "opName" ; rdfs:label "Name" ; atlas:kind "text" ] .
```

<!-- test: run -->
```sh
tripletrail shapes import-atlas atlas --out rail-era 2>&1
grep -v '^@prefix' rail-era/operational-point.ttl
```

<!-- test: output -->
```text
wrote rail-era/operational-point.ttl
left out: atlas:geometry on node shapes (1)

<https://example.org/atlas/operational-point>
    a sh:NodeShape ;
    sh:order 10 ;
    sh:property [
        sh:name "Name" ;
        sh:path <http://data.europa.eu/949/opName> ;
        tts:facet tts:Facet-Text ;
        tts:key "opName"
    ] ;
    sh:targetClass <http://data.europa.eu/949/OperationalPoint> ;
    tts:color "#d62728" ;
    tts:geometry [
        sh:order 0 ;
        sh:path ( geo:hasGeometry geo:asWKT )
    ] ;
    tts:key "operational-point" ;
    tts:labelStrip "\\s*\\(from \\d{4}-\\d{2}-\\d{2} until \\d{4}-\\d{2}-\\d{2}\\)$" ;
    tts:minZoom 0 .
```

A class whose features take their geometry from linked ones (`atlas:geometryFrom`) reads it
as RINF Atlas derives it, round after round: a hot axle box detector takes its track's
geometry, or, when the track has none, its track's section's. Each round is a geometry way of
its own, for the features the ways before it gave none: behind the link, then behind one and
two more links of the set:

<!-- test: file atlas/hot-axle-box-detector.ttl -->
```turtle
PREFIX atlas: <https://rinf-atlas.matdata.eu/ns#>
PREFIX sh: <http://www.w3.org/ns/shacl#>
PREFIX era: <http://data.europa.eu/949/>

<https://example.org/atlas/hot-axle-box-detector> a sh:NodeShape ;
  atlas:key "hot-axle-box-detector" ;
  sh:targetClass era:HABD ;
  atlas:geometryFrom [ sh:inversePath era:tracksideHabd ] .
```

<!-- test: run -->
```sh
tripletrail shapes import-atlas atlas --out rail-era > /dev/null 2>&1
grep -c 'sh:order' rail-era/hot-axle-box-detector.ttl
```

<!-- test: output -->
```text
3
```

The class's place in RINF Atlas's list becomes its place in the map's legend (`sh:order`). What
TripleTrail has no reader for, here RINF Atlas's hint that the class is drawn as points, is left out
and counted. RINF Atlas's views, in the folder's `views/`, become map views (`view-*.ttl`), and
their requirement checks (TEN-T, ERTMS) computed facets of the classes they check: meets,
data missing or does not meet, and for a section the best result of its tracks. The folder is a
pack for `--pack` or `shapes check`.

The describe pages work the same way from [rinf-describe](https://github.com/Matdata-eu/rinf-describe)'s
`shapes/` folder, copied into the pack as they are. Its rows, widgets and summaries read as
rinf-describe reads them. A row without `rd:key` is known by its name, so a track's *Part of*
replaces the one its superclass gives. A summary over the tracks says how many have no value
(`none (2)`). The *RINF parameters* tab lists every parameter by its RINF index, from the ERA
ontology loaded with the data. On ERA's Luxembourg sample, all 237 pages show the groups and
rows that rinf-describe shows, in the same order.

## What you learnt

- The Map page, describe pages, the Search and Explore pages, OGC API Features, vector tiles and the REST API work on any
  dataset without configuration.
- The Shapes page changes names, order, groups and what each reader shows, as the dataset's
  own shapes, in versions you can roll back.
- GIS tools connect to `/ds/ogc`, web maps to `/ds/tiles`, and developers to `/ds/api/v1`,
  with its docs page at `/$/ui/api/ds`; the API also writes records, checked against the
  dataset's own shapes.
- `clients/js` is a JavaScript client for queries, updates, graphs and the admin API.
- `--mcp` serves each dataset to AI agents at `/ds/mcp`, read-only and under the caller's access.
- `/ds/graphql` answers GraphQL queries over the same records, with a playground at
  `/$/ui/graphql/ds`, and writes them with mutations checked against the shapes.
- The dataset's shapes decide what they show, and can be customised, or come from a shape
  pack shared between datasets; `?dry=true` and `tripletrail shapes check` say what a new
  release of a pack would change first.

Next: [replacing Fuseki](11-fuseki.md).
