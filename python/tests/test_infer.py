"""SHACL-AF rule inference (issue #61)."""

import tripletrail as ox
import pytest
from tripletrail import Engine

SHAPES = """
@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://example.org/> .
ex:Shape a sh:NodeShape ; sh:targetClass ex:Section ;
  sh:rule [ a sh:SPARQLRule ; sh:construct \"\"\"
    PREFIX ex: <http://example.org/>
    CONSTRUCT { $this ex:fast true } WHERE { $this ex:maxSpeed ?v FILTER(?v >= 160) }\"\"\" ] .
"""
DATA = """
@prefix ex: <http://example.org/> .
ex:a a ex:Section ; ex:maxSpeed 200 . ex:b a ex:Section ; ex:maxSpeed 100 .
"""


@pytest.mark.parametrize("store", ["oxigraph", "tripletrail"])
def test_infer(store):
    e = Engine(store=store)
    shapes = ox.NamedNode("http://example.org/shapes")
    e.load(DATA, ox.RdfFormat.TURTLE)
    e.load(SHAPES, ox.RdfFormat.TURTLE, to_graph=shapes)
    triples = e.infer(shapes)
    assert [str(t) for t in triples] == [
        '<http://example.org/a> <http://example.org/fast> "true"^^<http://www.w3.org/2001/XMLSchema#boolean>'
    ]
    assert len(e) == 9  # nothing written
    inferred = ox.NamedNode("http://example.org/inferred")
    assert e.infer(shapes, into=inferred) == 1
    assert e.query("ASK { GRAPH <http://example.org/inferred> { <http://example.org/a> <http://example.org/fast> true } }")


def test_infer_include_existing():
    e = Engine()
    shapes = ox.NamedNode("http://example.org/shapes")
    e.load(DATA + "ex:a ex:fast true .", ox.RdfFormat.TURTLE)
    e.load(SHAPES, ox.RdfFormat.TURTLE, to_graph=shapes)
    assert e.infer(shapes) == []
    assert len(e.infer(shapes, include_existing=True)) == 1
