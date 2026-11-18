"""Saving an engine as a dataset directory and opening it read-only (issue #86, #88)."""

import tripletrail as ox
import pytest
from tripletrail import Engine

DATA = """
@prefix ex: <http://example.org/> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
ex:oslo geo:hasGeometry [ geo:asWKT "POINT(10.75 59.91)"^^geo:wktLiteral ] .
ex:bergen geo:hasGeometry [ geo:asWKT "POINT(5.32 60.39)"^^geo:wktLiteral ] .
"""
NEAR = """
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
SELECT ?s WHERE { ?s geo:hasGeometry/geo:asWKT ?w
  FILTER(geof:sfWithin(?w, "POLYGON((9 59, 12 59, 12 61, 9 61, 9 59))"^^geo:wktLiteral)) }
"""


@pytest.mark.parametrize("store", ["oxigraph", "tripletrail"])
def test_save_then_open_read_only(tmp_path, store):
    engine = Engine(store=store)
    engine.load(DATA, ox.RdfFormat.TURTLE)
    target = tmp_path / "network_20261107"
    engine.save(target)

    ro = Engine.open_read_only(target)
    assert ro.read_only
    assert len(ro) == len(engine)
    assert [str(r["s"]) for r in ro.query(NEAR)] == ["<http://example.org/oslo>"]
    with pytest.raises(Exception, match="read-only"):
        ro.update("INSERT DATA { <http://example.org/a> <http://example.org/b> 1 }")

    rw = Engine(target)
    assert not rw.read_only
    assert len(rw) == len(engine)

    with pytest.raises(Exception):
        engine.save(target)
