"""GeoParquet export and import (issue #80)."""

import io
import json

import tripletrail as ox
import pytest
from tripletrail import Engine

DATA = """
@prefix ex: <http://example.org/> . @prefix geo: <http://www.opengis.net/ont/geosparql#> .
ex:a ex:id "a" ; ex:tracks 4 ; geo:hasGeometry [ geo:asWKT "POINT(4.35 50.85)"^^geo:wktLiteral ] .
ex:b ex:id "b" ; ex:tracks 2 ; geo:hasGeometry [ geo:asWKT "LINESTRING(10 45, 11 46)"^^geo:wktLiteral ] .
"""
QUERY = """
PREFIX ex: <http://example.org/> PREFIX geo: <http://www.opengis.net/ont/geosparql#>
SELECT ?id ?tracks ?geometry WHERE { ?s ex:id ?id ; ex:tracks ?tracks ; geo:hasGeometry/geo:asWKT ?geometry } ORDER BY ?id
"""


def source():
    engine = Engine(store="tripletrail")
    engine.load(DATA.encode(), "text/turtle")
    return engine


def test_round_trip():
    data = source().export_geoparquet(QUERY)
    assert data[:4] == b"PAR1"
    target = Engine(store="tripletrail")
    stats = target.load_geoparquet(
        data, subject="http://example.org/{id}", predicate_prefix="http://example.org/"
    )
    assert stats == {"rows": 2, "triples": 10, "skipped_rows": 0, "invalid_geometries": 0}
    rows = [[s[v].value for v in ("id", "tracks", "geometry")] for s in target.query(QUERY)]
    assert rows == [["a", "4", "POINT(4.35 50.85)"], ["b", "2", "LINESTRING(10 45,11 46)"]]


def test_file_object_and_bbox(tmp_path):
    path = tmp_path / "out.parquet"
    assert source().export_geoparquet(QUERY, path, row_group_size=1) is None
    target = Engine(store="tripletrail")
    with open(path, "rb") as f:
        stats = target.load_geoparquet(
            f,
            subject="http://example.org/{id}",
            class_=ox.NamedNode("http://example.org/Feature"),
            to_graph=ox.NamedNode("http://example.org/g"),
            bbox=(4, 50, 5, 51),
        )
    assert stats["rows"] == 1 and stats["skipped_rows"] == 1
    ask = "ASK { GRAPH <http://example.org/g> { <http://example.org/a> a <http://example.org/Feature> } }"
    assert target.query(ask)


def test_pyarrow_reads_it():
    pq = pytest.importorskip("pyarrow.parquet")
    data = source().export_geoparquet(QUERY)
    table = pq.read_table(io.BytesIO(data))
    assert table.column_names == ["id", "tracks", "geometry", "bbox"]
    assert table.column("tracks").to_pylist() == [4, 2]
    geo = json.loads(pq.read_metadata(io.BytesIO(data)).metadata[b"geo"])
    assert geo["primary_column"] == "geometry"
    assert geo["columns"]["geometry"]["geometry_types"] == ["Point", "LineString"]
