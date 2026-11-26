"""Engine.run_mapping: RML, R2RML and YARRRML mappings into the store."""

import tripletrail as ox
import pytest
from tripletrail import Engine

RML = """@prefix rr: <http://www.w3.org/ns/r2rml#> .
@prefix rml: <http://semweb.mmlab.be/ns/rml#> .
@prefix ql: <http://semweb.mmlab.be/ns/ql#> .
@prefix ex: <http://example.com/> .
<#Station> rml:logicalSource [ rml:source "stations.csv" ; rml:referenceFormulation ql:CSV ] ;
  rr:subjectMap [ rr:template "http://example.com/station/{code}" ; rr:class ex:Station ] ;
  rr:predicateObjectMap [ rr:predicate ex:name ; rr:objectMap [ rml:reference "name" ] ] .
"""

YARRRML = """prefixes:
  ex: http://example.com/
mappings:
  station:
    sources:
      - [stations.csv~csv]
    s: ex:station/$(code)
    po:
      - [a, ex:Station]
"""

EX = "http://example.com/"


@pytest.fixture
def folder(tmp_path):
    (tmp_path / "stations.csv").write_text("code,name\nGNT,Gent\nBRG,Brugge\nBRG,Brugge\n")
    (tmp_path / "stations.rml.ttl").write_text(RML)
    (tmp_path / "stations.yarrrml").write_text(YARRRML)
    return tmp_path


@pytest.mark.parametrize("store", ["oxigraph", "tripletrail"])
def test_mapping_file_into_a_graph(folder, store):
    e = Engine(store=store)
    graph = ox.NamedNode(EX + "step")
    made = e.run_mapping(path=str(folder / "stations.rml.ttl"), base_dir=str(folder), to_graph=graph)
    assert made == 6  # BRG's two triples are made twice, stored once
    assert len(e) == 4
    assert e.objects(ox.NamedNode(EX + "station/GNT"), ox.NamedNode(EX + "name"), graph) == [ox.Literal("Gent")]


def test_text_and_yarrrml_relative_to_the_current_directory(folder, monkeypatch):
    monkeypatch.chdir(folder)
    e = Engine()
    e.run_mapping(RML)
    e.run_mapping(YARRRML, yarrrml=True)
    e.run_mapping(path="stations.yarrrml")
    stations = e.subjects(ox.NamedNode("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"), ox.NamedNode(EX + "Station"))
    assert sorted(s.value for s in stations) == [EX + "station/BRG", EX + "station/GNT"]


def test_errors(folder, tmp_path_factory):
    e = Engine()
    with pytest.raises(ValueError):
        e.run_mapping("not turtle at all <")
    with pytest.raises(ValueError):
        e.run_mapping(RML, path="x.rml.ttl")
    with pytest.raises(OSError):
        e.run_mapping(RML, base_dir=str(tmp_path_factory.mktemp("empty")))
    elsewhere = tmp_path_factory.mktemp("elsewhere")
    with pytest.raises(OSError):
        e.run_mapping(RML, base_dir=str(folder), allowed=[str(elsewhere)])
    assert len(e) == 0


R2RML = """@prefix rr: <http://www.w3.org/ns/r2rml#> .
@prefix ex: <http://example.com/> .
<#Station> rr:logicalTable [ rr:tableName "tt_py_station" ] ;
  rr:subjectMap [ rr:template "http://example.com/station/{code}" ; rr:class ex:Station ] ;
  rr:predicateObjectMap [ rr:predicate ex:tracks ; rr:objectMap [ rr:column "tracks" ] ] .
"""


@pytest.fixture
def postgres():
    """A table in the PostgreSQL database of TRIPLETRAIL_TEST_POSTGRES (skipped without it)."""
    import os
    import subprocess

    url = os.environ.get("TRIPLETRAIL_TEST_POSTGRES")
    if not url:
        pytest.skip("set TRIPLETRAIL_TEST_POSTGRES to a PostgreSQL connection string")
    sql = (
        "DROP TABLE IF EXISTS tt_py_station; "
        "CREATE TABLE tt_py_station (code text PRIMARY KEY, tracks integer); "
        "INSERT INTO tt_py_station VALUES ('GNT', 12), ('BRG', 10);"
    )
    subprocess.run(["psql", url, "-q", "-c", sql], check=True)
    yield url
    subprocess.run(["psql", url, "-q", "-c", "DROP TABLE tt_py_station"], check=True)


def test_database_sources(postgres, tmp_path):
    e = Engine()
    # R2RML reads the database given.
    assert e.run_mapping(R2RML, base_dir=str(tmp_path), database=postgres) == 4
    tracks = {str(s["s"].value): s["t"].value for s in e.query(
        "SELECT ?s ?t WHERE { ?s <http://example.com/tracks> ?t }")}
    assert tracks == {EX + "station/GNT": "12", EX + "station/BRG": "10"}
    # YARRRML names its own database.
    yarrrml = f"""prefixes:
  ex: http://example.com/
sources:
  db:
    access: {postgres}
    type: postgresql
    queryFormulation: sql2008
    query: SELECT code FROM tt_py_station WHERE tracks > 10
mappings:
  big:
    sources: db
    s: ex:big/$(code)
    po:
      - [a, ex:BigStation]
"""
    assert e.run_mapping(yarrrml, yarrrml=True, base_dir=str(tmp_path)) == 1
    # Without a database, an R2RML mapping has nothing to read.
    with pytest.raises((ValueError, OSError)):
        Engine().run_mapping(R2RML, base_dir=str(tmp_path))
