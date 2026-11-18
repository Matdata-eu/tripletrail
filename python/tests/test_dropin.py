"""tripletrail.Engine in place of pyoxigraph.Store (issue #55)."""

import io
import re

import tripletrail as ox
import pytest
from tripletrail import Engine

EX = "http://example.com/"
NQ = f"""
<{EX}a> <{EX}p> "1"^^<http://www.w3.org/2001/XMLSchema#integer> .
<{EX}b> <{EX}p> "2"^^<http://www.w3.org/2001/XMLSchema#integer> <{EX}g1> .
<{EX}c> <{EX}p> "3"^^<http://www.w3.org/2001/XMLSchema#integer> <{EX}g2> .
"""


@pytest.fixture(params=["oxigraph", "tripletrail"])
def engine(request):
    e = Engine(store=request.param)
    e.bulk_load(NQ.encode(), ox.RdfFormat.N_QUADS)
    return e


def total(engine, **kwargs):
    rows = list(engine.query("SELECT (SUM(?o) AS ?s) WHERE { ?x <http://example.com/p> ?o }", **kwargs))
    return int(rows[0]["s"].value)


def test_dataset_options(engine):
    g1, g2 = ox.NamedNode(EX + "g1"), ox.NamedNode(EX + "g2")
    assert total(engine) == 1
    # The union of all graphs, the default one included, as in pyoxigraph.
    assert total(engine, use_default_graph_as_union=True) == 6
    assert total(engine, default_graph=g1) == 2
    assert total(engine, default_graph=[ox.DefaultGraph(), g2]) == 4
    rows = engine.query("SELECT ?g WHERE { GRAPH ?g { ?s ?p ?o } }", named_graphs=[g2])
    assert [r["g"] for r in rows] == [g2]


def test_prefixes_and_substitutions(engine):
    rows = list(
        engine.query(
            "SELECT ?x ?o WHERE { GRAPH ?g { ?x ex:p ?o } }",
            prefixes={"ex": EX},
            substitutions={ox.Variable("o"): ox.Literal(3)},
        )
    )
    assert [r["x"] for r in rows] == [ox.NamedNode(EX + "c")]
    engine.update("INSERT DATA { ex:d ex:p 4 }", prefixes={"ex": EX})
    assert total(engine) == 5


def test_solution_get(engine):
    row = next(iter(engine.query("SELECT ?x ?y WHERE { ?x ?p ?o OPTIONAL { ?x ?q ?y FILTER(false) } }")))
    assert row.get("x") == ox.NamedNode(EX + "a")
    assert row.get("y") is None
    assert row.get("missing") is None
    assert row.get("missing", 7) == 7


def test_bulk_extend_and_graphs(engine):
    g3 = ox.NamedNode(EX + "g3")
    engine.bulk_extend([ox.Quad(ox.NamedNode(EX + "e"), ox.NamedNode(EX + "p"), ox.Literal(5), g3)])
    assert set(engine.named_graphs()) == {ox.NamedNode(EX + "g1"), ox.NamedNode(EX + "g2"), g3}
    assert engine.contains_named_graph(g3)
    engine.remove_graph(g3)
    assert not engine.contains_named_graph(g3)
    engine.clear_graph(ox.NamedNode(EX + "g1"))
    assert total(engine, use_default_graph_as_union=True) == 4
    engine.add_graph(g3)
    engine.clear()
    assert len(engine) == 0


def test_dump(engine):
    quads = list(ox.parse(engine.dump(format=ox.RdfFormat.N_QUADS), ox.RdfFormat.N_QUADS))
    assert len(quads) == 3
    out = io.BytesIO()
    engine.dump(out, ox.RdfFormat.TURTLE, from_graph=ox.NamedNode(EX + "g1"), prefixes={"ex": EX})
    assert b"ex:b" in out.getvalue()
    with pytest.raises(ValueError, match="DefaultGraph()"):
        engine.dump(format=ox.RdfFormat.TURTLE)


def test_dump_triples_without_from_graph():
    # Only a default graph: a format of triples writes it, without from_graph (#381).
    engine = Engine()
    engine.add(ox.Quad(ox.NamedNode(EX + "s"), ox.NamedNode(EX + "p"), ox.Literal("o")))
    out = engine.dump(format=ox.RdfFormat.N_TRIPLES)
    assert out == f'<{EX}s> <{EX}p> "o" .\n'.encode()
    assert b"<" in engine.dump(format=ox.RdfFormat.TURTLE)
    assert Engine().dump(format=ox.RdfFormat.TURTLE) == b""
    # A named graph too: which graph to write is the caller's choice.
    engine.add(ox.Quad(ox.NamedNode(EX + "s"), ox.NamedNode(EX + "p"), ox.Literal("o"), ox.NamedNode(EX + "g")))
    with pytest.raises(ValueError, match=EX + "g"):
        engine.dump(format=ox.RdfFormat.TURTLE)
    assert engine.dump(format=ox.RdfFormat.N_TRIPLES, from_graph=ox.DefaultGraph()) == out


def test_rdflib_store():
    rdflib = pytest.importorskip("rdflib")
    from tripletrail.rdflib import TripleTrailStore

    store = TripleTrailStore(store=Engine())
    ds = rdflib.Dataset(store=store)
    g = ds.graph(rdflib.URIRef(EX + "g"))
    g.parse(data=f"<{EX}s> <{EX}p> 1.0 .", format="turtle")
    g.add((rdflib.URIRef(EX + "s"), rdflib.URIRef(EX + "q"), rdflib.Literal("x")))
    assert len(g) == 2
    rows = list(g.query("SELECT ?o WHERE { ?s <http://example.com/q> ?o }"))
    assert [str(r[0]) for r in rows] == ["x"]
    assert rdflib.URIRef(EX + "g") in {c.identifier for c in ds.graphs()}
    assert b"<http://example.com/s>" in g.serialize(format="nt", encoding="utf-8")
    ds.remove_graph(g)
    assert len(store) == 0


def test_literals_keep_their_lexical_form():
    # Documented difference (issue #55): the tripletrail store keeps lexical forms, Oxigraph's
    # store canonicalises; comparisons are by value in both.
    data = b'<http://example.com/a> <http://example.com/v> 100.0 .'
    # A computed decimal is written as Jena writes it on the tripletrail store ("100.0"),
    # canonically on Oxigraph's ("100").
    for store, expected, computed in [("tripletrail", "100.0", "100.0"), ("oxigraph", "100", "100")]:
        e = Engine(store=store)
        e.load(data, "text/turtle")
        assert [q.object.value for q in e] == [expected]
        assert len(list(e.query("SELECT ?s WHERE { ?s ?p ?v FILTER(?v = 100) }"))) == 1
        assert [r[0].value for r in e.query("SELECT (?v + 0 AS ?x) WHERE { ?s ?p ?v }")] == [computed]


def test_quads_for_pattern_matches_pyoxigraph():
    # Every kind of term, in the default and in named graphs, over several batches.
    data = f"""
_:b1 <{EX}p> "plain" .
_:b1 <{EX}p> "tekst"@nl <{EX}g1> .
<{EX}a> <{EX}p> "right"@ar--rtl _:g .
<{EX}a> <{EX}q> "line\\nbreak \\"quoted\\" \\\\ é"^^<{EX}dt> .
<{EX}a> <{EX}r> <<( <{EX}s> <{EX}p> _:b2 )>> <{EX}g1> .
""" + "".join(f"<{EX}s{i // 7}> <{EX}p{i % 7}> \"{i}\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n" for i in range(10_000))
    pyox = pytest.importorskip("pyoxigraph")
    store = pyox.Store()
    store.load(data.encode(), pyox.RdfFormat.N_QUADS)
    for kind in ["oxigraph", "tripletrail"]:
        e = Engine(store=kind)
        e.load(data.encode(), ox.RdfFormat.N_QUADS)
        for pattern, pyox_pattern in [
            ((None, None, None), (None, None, None)),
            ((None, ox.NamedNode(EX + "p"), None), (None, pyox.NamedNode(EX + "p"), None)),
            ((ox.NamedNode(EX + "a"), None, None), (pyox.NamedNode(EX + "a"), None, None)),
        ]:
            # Blank node labels are each store's own.
            def norm(quads):
                return sorted(re.sub(r"_:\w+", "_:b", str(q)) for q in quads)
            assert norm(e.quads_for_pattern(*pattern)) == norm(store.quads_for_pattern(*pyox_pattern)), kind
            assert all(isinstance(q, ox.Quad) for q in e.quads_for_pattern(*pattern))
        assert len(list(e)) == len(store)
        # Taking a few quads does not read the rest.
        it = e.quads_for_pattern(None, None, None)
        assert isinstance(next(it), ox.Quad)


def test_lookups_and_projections():
    e = Engine()
    s, p, q = ox.NamedNode(EX + "s"), ox.NamedNode(EX + "p"), ox.NamedNode(EX + "q")
    g = ox.NamedNode(EX + "g")
    e.extend([ox.Quad(s, p, ox.Literal("1")), ox.Quad(s, p, ox.Literal("2", language="en")),
              ox.Quad(s, q, ox.NamedNode(EX + "o"), g)])
    # A lookup smaller than a batch, and one with nothing.
    assert [x.object for x in e.quads_for_pattern(s, q, None)] == [ox.NamedNode(EX + "o")]
    assert list(e.quads_for_pattern(q, q, None)) == []
    assert sorted(map(str, e.objects(s, p))) == ['"1"', '"2"@en']
    assert e.objects(s, q, ox.DefaultGraph()) == []
    assert e.objects(s, q, g) == [ox.NamedNode(EX + "o")]
    assert e.subjects(q, ox.NamedNode(EX + "o")) == [s]
    assert sorted(str(o) for _, o in e.predicate_objects(s)) == ['"1"', '"2"@en', f"<{EX}o>"]


def test_no_pyoxigraph_needed():
    import sys

    assert "pyoxigraph" not in sys.modules or ox.NamedNode is not sys.modules["pyoxigraph"].NamedNode
    assert ox.Store is Engine
    assert type(next(iter(Engine().query("SELECT (1 AS ?x) {}")))["x"]) is ox.Literal


def test_pyoxigraph_terms_are_accepted():
    pyox = pytest.importorskip("pyoxigraph")
    e = Engine()
    e.add(pyox.Quad(pyox.NamedNode(EX + "s"), pyox.NamedNode(EX + "p"), pyox.Literal("1", language="en")))
    e.extend([pyox.Quad(pyox.BlankNode("b"), pyox.NamedNode(EX + "p"), pyox.Literal(2), pyox.NamedNode(EX + "g"))])
    assert sorted(str(q) for q in e) == [
        f'<{EX}s> <{EX}p> "1"@en',
        f'_:b <{EX}p> "2"^^<http://www.w3.org/2001/XMLSchema#integer> <{EX}g>',
    ]
    assert e.objects(pyox.NamedNode(EX + "s"), pyox.NamedNode(EX + "p")) == [ox.Literal("1", language="en")]


def test_union_default_graph():
    e = Engine()
    e.update(f"INSERT DATA {{ GRAPH <{EX}a> {{ <{EX}x> <{EX}p> 1 }} GRAPH <{EX}b> {{ <{EX}y> <{EX}p> 2 }} }}")
    assert not e.union_default_graph
    e.union_default_graph = True
    assert len(list(e.query(f"SELECT * WHERE {{ ?s <{EX}p> ?o }}"))) == 2
    e.update(f"INSERT {{ ?s <{EX}q> ?o }} WHERE {{ ?s <{EX}p> ?o }}")
    assert len(list(e.quads_for_pattern(None, ox.NamedNode(EX + "q"), None, ox.DefaultGraph()))) == 2
