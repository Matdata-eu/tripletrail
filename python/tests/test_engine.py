"""tripletrail against the acceptance test of issue #21."""

import io

import tripletrail as ox
import pytest
from tripletrail import Engine

PREFIXES = """
PREFIX ex: <http://example.com/>
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
PREFIX st: <http://w3id.org/tripletrail/st#>
"""

TTL = """
@prefix ex: <http://example.com/> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
ex:track geo:hasGeometry ex:g1 . ex:g1 geo:asWKT "LINESTRING Z(0 0 10, 10 0 20)"^^geo:wktLiteral .
ex:station geo:hasGeometry ex:g2 . ex:g2 geo:asWKT "POINT(50 50)"^^geo:wktLiteral .
"""


@pytest.fixture(params=["oxigraph", "tripletrail"])
def engine(request, tmp_path):
    e = Engine(store=request.param)
    path = tmp_path / "kg.ttl"
    path.write_text(TTL)
    e.load(path=str(path))
    return e


def test_query_returns_terms(engine):
    rows = list(engine.query(PREFIXES + "SELECT ?g (st:ST_LineInterpolatePoint(?w, 0.5) AS ?p) WHERE { ?g geo:asWKT ?w FILTER(geof:dimension(?w) = 1) }"))
    assert len(rows) == 1
    row = rows[0]
    assert row["g"] == ox.NamedNode("http://example.com/g1")
    assert row[0] == row["g"] == row[ox.Variable("g")]
    point = row["p"]
    assert isinstance(point, ox.Literal)
    assert point.value == "POINT Z(5 0 15)"
    assert point.datatype == ox.NamedNode("http://www.opengis.net/ont/geosparql#wktLiteral")


def test_update_with_spatial_filter(engine):
    engine.update(PREFIXES + """INSERT { ?f ex:inBox true } WHERE {
        ?f geo:hasGeometry/geo:asWKT ?w
        FILTER(geof:sfIntersects(?w, "POLYGON((-1 -1, 11 -1, 11 1, -1 1, -1 -1))"^^geo:wktLiteral)) }""")
    found = [q.subject for q in engine.quads_for_pattern(None, ox.NamedNode("http://example.com/inBox"), None)]
    assert found == [ox.NamedNode("http://example.com/track")]


def test_ask_construct_and_serialize(engine):
    assert engine.query("ASK { ?s ?p ?o }")
    assert not engine.query("ASK { ?s <http://example.com/none> ?o }")
    triples = list(engine.query("CONSTRUCT WHERE { ?s <http://www.opengis.net/ont/geosparql#hasGeometry> ?o }"))
    assert len(triples) == 2 and all(isinstance(t, ox.Triple) for t in triples)
    csv = engine.query("SELECT ?s WHERE { ?s <http://www.opengis.net/ont/geosparql#hasGeometry> ?o } ORDER BY ?s").serialize(format=ox.QueryResultsFormat.CSV)
    assert csv.replace(b"\r", b"") == b"s\nhttp://example.com/station\nhttp://example.com/track\n"
    out = io.BytesIO()
    engine.query("SELECT * { ?s ?p ?o } LIMIT 1").serialize(out, "application/sparql-results+json")
    assert out.getvalue().startswith(b'{"head":')


def test_load_into_named_graph_bytes_and_quads(engine):
    g = ox.NamedNode("http://example.com/step")
    engine.load(b'<http://example.com/a> <http://example.com/p> "x"@en .', ox.RdfFormat.N_TRIPLES, to_graph=g)
    quads = list(engine.quads_for_pattern(None, None, None, g))
    assert quads == [ox.Quad(ox.NamedNode("http://example.com/a"), ox.NamedNode("http://example.com/p"), ox.Literal("x", language="en"), g)]
    engine.add(ox.Quad(ox.NamedNode("http://example.com/b"), ox.NamedNode("http://example.com/p"), ox.Literal(1), g))
    assert len(list(engine.quads_for_pattern(None, None, None, g))) == 2
    engine.remove(ox.Quad(ox.NamedNode("http://example.com/b"), ox.NamedNode("http://example.com/p"), ox.Literal(1), g))
    assert len(engine) == 5


def test_load_keeps_blank_node_labels_unless_asked_to_rename():
    # As pyoxigraph's Store.load: a second load can refer to `_:b1` of the first (#840).
    nt = b'<http://example.com/a> <http://example.com/p> _:b1 .'
    count = "SELECT (COUNT(DISTINCT ?b) AS ?n) { ?s ?p ?b }"
    kept = Engine()
    kept.load(nt, "nt")
    kept.load(nt, "nt")
    assert next(kept.query(count))["n"].value == "1"
    assert ox.BlankNode("b1") in {q.object for q in kept.quads_for_pattern(None, None, None)}
    renamed = Engine()
    renamed.load(nt, "nt", rename_blank_nodes=True)
    renamed.bulk_load(nt, "nt", rename_blank_nodes=True)
    assert next(renamed.query(count))["n"].value == "2"


def test_named_graph_load_is_seen_by_the_spatial_index(engine):
    q = PREFIXES + 'SELECT (COUNT(*) AS ?n) WHERE { GRAPH ?x { ?g geo:asWKT ?w } FILTER(geof:sfWithin(?w, "POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))"^^geo:wktLiteral)) }'
    assert next(engine.query(q))["n"].value == "0"
    engine.load('<http://example.com/b> <http://www.opengis.net/ont/geosparql#asWKT> "POINT(2 2)"^^<http://www.opengis.net/ont/geosparql#wktLiteral> .', "nt", to_graph=ox.NamedNode("http://example.com/step"))
    assert next(engine.query(q))["n"].value == "1"


def test_errors_and_explain(engine):
    with pytest.raises(SyntaxError):
        engine.query("SELEC")
    explanation = engine.explain("SELECT * { ?s ?p ?o }")
    assert explanation.startswith('{"rows":4,')
    chain = "\n".join(f"<http://example.com/s{i}> <http://example.com/p> <http://example.com/s{i + 1}> ." for i in range(3000))
    engine.load(chain, "nt")
    with pytest.raises(TimeoutError):
        list(engine.query("SELECT (COUNT(*) AS ?n) WHERE { ?a <http://example.com/p>* ?b }", timeout=0.2))


def test_tripletrail_store_on_disk(tmp_path):
    e = Engine(str(tmp_path / "db"), store="tripletrail")
    e.update(PREFIXES + 'INSERT DATA { ex:a ex:p "x" }')
    del e
    e = Engine(str(tmp_path / "db"), store="tripletrail")
    assert len(e) == 1
    with pytest.raises(ValueError):
        Engine(store="other")


def test_result_cache():
    e = Engine()
    e.update("INSERT DATA { <http://example.com/a> <http://example.com/p> 1 }")
    e.set_result_cache(1 << 20)
    q = "SELECT ?s WHERE { ?s <http://example.com/p> 1 }"
    assert len(list(e.query(q))) == 1
    assert len(list(e.query(q))) == 1
    assert e.result_cache_stats()["hits"] == 1
    e.update("DELETE DATA { <http://example.com/a> <http://example.com/p> 1 }")
    assert list(e.query(q)) == []


def test_prepared_query_runs_with_new_values(engine):
    q = engine.prepare(
        "SELECT ?f ?w WHERE { ?f geo:hasGeometry/geo:asWKT ?w }",
        prefixes={"geo": "http://www.opengis.net/ont/geosparql#"},
    )
    assert isinstance(q, ox.PreparedQuery)
    assert [v.value for v in q.variables] == ["f", "w"]
    station = ox.NamedNode("http://example.com/station")
    rows = engine.query(q, substitutions={ox.Variable("f"): station})
    assert [r["w"].value for r in rows] == ["POINT(50 50)"]
    assert len(list(engine.query(q))) == 2
    with pytest.raises(ValueError):
        engine.query(q, base_iri="http://example.com/")
