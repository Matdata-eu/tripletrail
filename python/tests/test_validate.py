"""SHACL validation with a W3C report (issue #59)."""

import tripletrail as ox
import pytest
from tripletrail import Engine

SHAPES = """
@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://example.com/> .
ex:Car a sh:NodeShape ; sh:targetClass ex:Car ;
  sh:property [ sh:path ex:speed ; sh:maxInclusive 200 ; sh:severity sh:Warning ; sh:message "too fast"@en ] ;
  sh:property [ sh:path ex:name ; sh:minCount 1 ] .
"""
DATA = """
@prefix ex: <http://example.com/> .
ex:a a ex:Car ; ex:speed 250 ; ex:name "a" . ex:b a ex:Car ; ex:speed 100 .
"""
SH = "http://www.w3.org/ns/shacl#"


@pytest.mark.parametrize("store", ["oxigraph", "tripletrail"])
def test_validate(store):
    e = Engine(store=store)
    shapes = ox.NamedNode("http://example.com/shapes")
    e.load(DATA, ox.RdfFormat.TURTLE)
    e.load(SHAPES, ox.RdfFormat.TURTLE, to_graph=shapes)
    report = e.validate(shapes, report_graph=ox.NamedNode("http://example.com/report"))
    assert not report.conforms
    rows = sorted(report.results, key=lambda r: r["focus_node"].value)
    assert [(r["focus_node"].value, r["source_constraint_component"].value, r["severity"].value) for r in rows] == [
        ("http://example.com/a", SH + "MaxInclusiveConstraintComponent", SH + "Warning"),
        ("http://example.com/b", SH + "MinCountConstraintComponent", SH + "Violation"),
    ]
    assert rows[0]["value"] == ox.Literal(250)
    assert rows[0]["message"] == ox.Literal("too fast", language="en")
    assert rows[0]["result_path"] == "<http://example.com/speed>"
    assert rows[1]["value"] is None
    parsed = list(ox.parse(report.serialize(), ox.RdfFormat.TURTLE))
    assert len(parsed) == len(report.triples())
    asked = e.query(
        "ASK { GRAPH <http://example.com/report> { ?r <http://www.w3.org/ns/shacl#conforms> false } }"
    )
    assert bool(asked)


def test_shape_counts():
    e = Engine()
    shapes = ox.NamedNode("http://example.com/shapes")
    e.load(DATA, ox.RdfFormat.TURTLE)
    e.load(SHAPES + "ex:Boat a sh:NodeShape ; sh:targetClass ex:Boat .", ox.RdfFormat.TURTLE, to_graph=shapes)
    assert e.validate(shapes).shape_counts == []
    report = e.validate(shapes, statistics=True)
    counts = {c["shape"].value if isinstance(c["shape"], ox.NamedNode) else c["property"]: c for c in report.shape_counts}
    assert counts["http://example.com/Car"]["focus_nodes"] == 2
    # A targeted shape without focus nodes is counted too.
    assert counts["http://example.com/Boat"]["focus_nodes"] == 0
    props = [c for c in report.shape_counts if c["property"]]
    assert sorted((c["focus_nodes"], c["value_nodes"]) for c in props) == [(2, 1), (2, 2)]
    # Each property shape fails on one car: ex:a is too fast, ex:b has no name.
    assert sorted((c["distinct_focus_nodes"], c["failing_focus_nodes"]) for c in props) == [(2, 1), (2, 1)]
    assert (counts["http://example.com/Car"]["distinct_focus_nodes"], counts["http://example.com/Car"]["failing_focus_nodes"]) == (2, 0)
    stats = [t for t in report.triples() if t.predicate.value == "http://w3id.org/tripletrail/shacl#shapeStatistics"]
    assert len(stats) == 4 and all(isinstance(t.object, ox.NamedNode) for t in stats)
