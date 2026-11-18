# GeoParquet

With the `geoparquet` feature (on by default in the `tripletrail` binary and the Python package),
`engine.export_geoparquet(query, writer, &options)` writes the results of a `SELECT` as a
GeoParquet 1.1 file, and `engine.load_geoparquet(reader, &mapping)` loads one as triples.

- **Export**: one column per variable, typed from its values (integers as `INT64`,
  `xsd:decimal` as `DECIMAL(38, s)`, doubles, booleans, anything else as text). A column whose
  values are all geometry literals is a WKB geometry column, with its CRS (CRS84 is the
  default and written as no `crs`; EPSG:4326 as its PROJJSON; other CRSs need
  `ExportOptions::projjson`, or get a PROJJSON holding only their `id`, which GeoPandas refuses),
  geometry types and extent in the `geo` metadata. The first geometry column gets a `bbox`
  covering column, so readers (`geopandas.read_parquet(bbox=...)`, DuckDB) skip row groups
  outside an area. A column mixing CRSs is an error.
- **Import**: each row gets a subject from an IRI template (`http://example.org/station/{id}`,
  values percent-encoded, or `{+feature}` for a column of IRIs, used as is), a
  `<predicate_prefix><column>` triple per value, an optional `rdf:type`, and each WKB geometry
  column as `geo:hasGeometry <subject/geometry>` with a `geo:Geometry` node and its
  `geo:asWKT`. A bounding box keeps only the rows whose geometry intersects it, and skips whole
  row groups by their `bbox` statistics. GeoArrow native encodings are not read yet.

```sh
tripletrail export --location /data/tripletrail --dataset kg --query @features.rq features.parquet
tripletrail load --location /data/tripletrail --dataset kg --subject '{+feature}' features.parquet
tripletrail load --location /data/tripletrail --dataset kg --subject 'https://example.org/b/{id}' \
  --predicate-prefix https://example.org/def# --class https://example.org/def#Building \
  --bbox 4.2,50.7,4.5,50.95 buildings.parquet
```

The server answers a `SELECT` as GeoParquet for `Accept: application/vnd.apache.parquet` or
`?format=parquet`. From Python: `Engine.export_geoparquet(query, output=None)` (bytes without
`output`) and `Engine.load_geoparquet(input, subject=..., predicate_prefix=..., class_=...,
to_graph=..., bbox=...)`.

On 251k RINF features (25 country graphs, points, lines and polygons), an export and re-import
gives back the same geometries, byte for byte as WKB. The GeoParquet file is 28 MB against
194 MB of N-Triples for the same triples, and loads in 1.4 s against 1.2 s: the store keeps
WKT text, so each WKB geometry is written out as text on import.
