"""The remote client (`tripletrail.connect`) and the notebook magics, against a `tripletrail serve`.

The server is the binary at `TRIPLETRAIL_BIN`, or the debug build of this repository; without one,
the tests are skipped.
"""

import os
import pathlib
import re
import subprocess
import tempfile

import pytest

import tripletrail
from tripletrail import magics, remote

ROOT = pathlib.Path(__file__).resolve().parents[2]
E = "http://e/"


def _binary():
    found = os.environ.get("TRIPLETRAIL_BIN")
    if found:
        return found
    for target in [os.environ.get("CARGO_TARGET_DIR"), ROOT / "target"]:
        if target and (pathlib.Path(target) / "debug" / "tripletrail").exists():
            return str(pathlib.Path(target) / "debug" / "tripletrail")
    return None


@pytest.fixture(scope="module")
def server():
    binary = _binary()
    if binary is None:
        pytest.skip("no tripletrail binary: set TRIPLETRAIL_BIN")
    # The server keeps its workspaces in a folder under TMPDIR and removes it only on a graceful
    # stop; give it a private TMPDIR that goes away with the fixture.
    tmp = tempfile.TemporaryDirectory(prefix="tripletrail-remote-")
    process = subprocess.Popen(
        [binary, "serve", "--bind", "127.0.0.1:0"],
        stderr=subprocess.PIPE,
        text=True,
        env={**os.environ, "TMPDIR": tmp.name},
    )
    url = None
    for line in process.stderr:
        found = re.search(r"listening on (http://[^/ ]+)", line)
        if found:
            url = found.group(1)
            break
    assert url, "the server did not start"
    yield url
    process.terminate()
    process.wait(timeout=30)
    tmp.cleanup()


DATA = """@prefix e: <http://e/> .
e:x a e:Station ; e:km 3 .
e:y a e:Station .
"""
SHAPES = """@prefix sh: <http://www.w3.org/ns/shacl#> .
<http://e/S> a sh:NodeShape ; sh:targetClass <http://e/Station> ;
  sh:property [ sh:path <http://e/km> ; sh:minCount 1 ] .
"""
RULES = """@prefix sh: <http://www.w3.org/ns/shacl#> .
<http://e/R> a sh:NodeShape ; sh:targetClass <http://e/Station> ;
  sh:rule [ a sh:TripleRule ; sh:subject sh:this ; sh:predicate <http://e/seen> ; sh:object true ] .
"""


def test_workspace(server, tmp_path):
    ws = tripletrail.connect(server, workspace="remote test", create=True)
    assert remote.default is ws
    assert tripletrail.connect(server, workspace="remote test").id == ws.id
    path = tmp_path / "stations.ttl"
    path.write_text(DATA)
    ws.load(path=path)
    ws.write_file("shapes/s.ttl", SHAPES)
    ws.write_file("rules/r.ttl", RULES)
    assert sorted(ws.files()) == ["data/stations.ttl", "rules/r.ttl", "shapes/s.ttl"]
    assert ws.read_file("data/stations.ttl") == DATA

    rows = ws.query("SELECT ?s ?km WHERE { ?s a <http://e/Station> OPTIONAL { ?s <http://e/km> ?km } } ORDER BY ?s")
    assert isinstance(rows, tripletrail.QuerySolutions)
    rows = list(rows)
    assert rows[0]["s"] == tripletrail.NamedNode(E + "x")
    assert rows[0]["km"] == tripletrail.Literal("3", datatype=tripletrail.NamedNode("http://www.w3.org/2001/XMLSchema#integer"))
    assert rows[1]["km"] is None
    assert bool(ws.query("ASK { <http://e/x> ?p ?o }"))
    triples = ws.query("CONSTRUCT WHERE { ?s <http://e/km> ?o }")
    assert list(triples) == [tripletrail.Triple(tripletrail.NamedNode(E + "x"), tripletrail.NamedNode(E + "km"),
                                             tripletrail.Literal("3", datatype=tripletrail.NamedNode("http://www.w3.org/2001/XMLSchema#integer")))]
    assert b"<http://e/km>" in triples.serialize()
    assert "plan" in ws.explain("SELECT * { ?s ?p ?o }")

    report = ws.validate()
    assert not report.conforms and len(report) == 1
    result = report.results[0]
    assert result["focus_node"] == tripletrail.NamedNode(E + "y")
    assert result["result_path"] == "<http://e/km>"
    assert result["severity"] == tripletrail.NamedNode("http://www.w3.org/ns/shacl#Violation")
    assert b"ValidationReport" in report.serialize()
    assert any(t.object == tripletrail.NamedNode("http://www.w3.org/ns/shacl#ValidationReport") for t in report.triples())

    inferred = ws.infer()
    assert len(inferred) == 2 and all(t.predicate == tripletrail.NamedNode(E + "seen") for t in inferred)
    traced = ws.infer(provenance=True)
    assert traced[0][1]["kind"] == "rule"

    assert b"<http://e/km>" in ws.dump(from_graph=tripletrail.DefaultGraph())
    assert b"<http://e/km>" in ws.dump()

    with pytest.raises(NotImplementedError):
        ws.update("INSERT DATA { <a:a> <a:b> <a:c> }")
    with pytest.raises(remote.RemoteError) as error:
        ws.write_file("data/bad.ttl", "<a> <b")
    assert error.value.status == 400

    ws.write_file("updates/tag.ru", "INSERT { ?s <http://e/tag> 1 } WHERE { ?s a <http://e/Station> }")
    ws.write_file("pipeline.yaml", "steps:\n  - load: data/\n  - update: updates/tag.ru\n")
    run = ws.run_pipeline()
    assert [s["report"]["status"] for s in run["steps"]] == ["ok", "ok"]
    assert len(list(ws.query("SELECT * { ?s <http://e/tag> ?o }", step=2))) == 2
    assert len(list(ws.query("SELECT * { ?s <http://e/tag> ?o }", step=1))) == 0
    blame = ws.blame(tripletrail.NamedNode(E + "x"), tripletrail.NamedNode(E + "tag"),
                     tripletrail.Literal("1", datatype=tripletrail.NamedNode("http://www.w3.org/2001/XMLSchema#integer")))
    assert blame["events"][0]["file"] == "updates/tag.ru"

    with pytest.raises(LookupError):
        tripletrail.connect(server, workspace="no such workspace")


def test_dataset(server):
    ds = tripletrail.connect(server, dataset="ds")
    ds.update("DROP SILENT ALL")
    ds.load(DATA, "ttl")
    ds.load(SHAPES, "text/turtle", to_graph=tripletrail.NamedNode(E + "shapes"))
    assert len(list(ds.query("SELECT * { ?s a <http://e/Station> }"))) == 2
    ds.update("INSERT DATA { GRAPH <http://e/g> { <http://e/x> <http://e/p> 1 } }")
    assert b"<http://e/p>" in ds.dump(from_graph=tripletrail.NamedNode(E + "g"))
    assert b"<http://e/g>" in ds.dump()
    report = ds.validate(tripletrail.NamedNode(E + "shapes"))
    assert not report.conforms and report.results[0]["focus_node"] == tripletrail.NamedNode(E + "y")
    assert len(report.results[0]["key"]) == 32
    assert report.summary["results"] == len(report.results)
    assert report.summary["cells"][0]["samples"][0]["key"] in {r["key"] for r in report.results}
    with pytest.raises(remote.RemoteError):
        ds.query("SELECT nonsense")


def _fuseki_step(base_url, dataset, graph):
    """A step as written for Fuseki: an update to `{base_url}/{dataset}/update`."""
    import urllib.parse
    import urllib.request

    update = f"INSERT {{ GRAPH <{graph}> {{ ?s <http://e/seen> true }} }} WHERE {{ ?s a <http://e/Station> }}"
    urllib.request.urlopen(urllib.request.Request(
        f"{base_url}/{dataset}/update", data=urllib.parse.urlencode({"update": update}).encode()))


def test_python_step(server):
    ws = tripletrail.connect(server, workspace="python step", create=True)
    ws.load(DATA, "ttl", file="data/a.ttl")
    ws.write_file("pipeline.yaml", "steps:\n  - load: data/\n  - python: dim.steps:run\n    graph: http://e/py\n"
                                   "  - update: updates/tag.ru\n")
    ws.write_file("updates/tag.ru", "INSERT { ?s <http://e/tag> 1 } WHERE { GRAPH <http://e/py> { ?s ?p ?o } }")
    run = ws.run_pipeline()
    assert run["steps"][1]["report"]["status"] == "waiting"
    assert ws.step_endpoint(2) == (f"{server}/$/workspaces/{ws.id}/pipeline", "2")
    done = ws.run_step(2, _fuseki_step, graph="http://e/py")
    assert done["steps"][1]["report"]["status"] == "ok"
    run = ws.run_pipeline()
    assert run["steps"][2]["report"]["status"] == "ok"
    assert len(list(ws.query("SELECT * { ?s <http://e/tag> 1 }", step=3))) == 2


def test_history(server):
    ws = tripletrail.connect(server, workspace="history", create=True)
    ws.load(DATA, "ttl", file="data/a.ttl")
    list(ws.query("SELECT * { ?s ?p ?o }"))
    runs = ws.history()
    assert runs[0]["op"] == "query"
    assert runs[0]["client"] == "python"
    assert runs[0]["text"] == "SELECT * { ?s ?p ?o }"


def test_overlay(server):
    ds = tripletrail.connect(server, dataset="ds")
    ds.load(DATA, "ttl")
    before = len(list(ds.query("SELECT * { ?s ?p ?o }")))
    ws = tripletrail.connect(server, workspace="overlay", create=True)
    with pytest.raises(NotImplementedError):
        ws.update("INSERT DATA { <http://e/x> a <http://e/T> }")
    assert ws.overlay() is None
    quads = len(list(ds.query("SELECT * { { ?s ?p ?o } UNION { GRAPH ?g { ?s ?p ?o } } }")))
    assert ws.overlay("ds")["quads"] == quads
    ws.update("INSERT DATA { <http://e/x> a <http://e/T> }")
    assert len(list(ws.query("SELECT * { ?s ?p ?o }"))) == before + 1
    assert len(list(ds.query("SELECT * { ?s ?p ?o }"))) == before
    assert "A <http://e/x>" in ws.patch()
    assert len(ws.patch("added").splitlines()) == 1
    ws.drop_changes()
    assert len(list(ws.query("SELECT * { ?s ?p ?o }"))) == before
    ws.detach()
    assert ws.overlay() is None


class FakeShell:
    def __init__(self):
        self.user_ns = {}
        self.magics = {}

    def register_magic_function(self, function, kind, name):
        self.magics[name] = function


def test_magics(server):
    shell = FakeShell()
    tripletrail.load_ipython_extension(shell)
    assert set(shell.magics) == {"sparql", "shacl"}
    engine = tripletrail.Engine()
    engine.load(DATA, "text/turtle")
    shell.user_ns["e"] = engine
    table = shell.magics["sparql"]("e --out rows", "SELECT ?s WHERE { ?s a <http://e/Station> } ORDER BY ?s")
    assert len(table) == 2 and len(shell.user_ns["rows"]) == 2
    table = shell.magics["shacl"]("e --out report", SHAPES)
    assert len(table) == 1 and not shell.user_ns["report"].conforms
    # The shapes are gone again.
    assert not engine.contains_named_graph(tripletrail.NamedNode(magics._SHAPES))

    ws = tripletrail.connect(server, workspace="magics", create=True)
    ws.load(DATA, "ttl", file="data/a.ttl")
    table = shell.magics["sparql"]("", "SELECT ?s WHERE { ?s a <http://e/Station> }")
    assert len(table) == 2
    table = shell.magics["shacl"]("", SHAPES)
    assert len(table) == 1
    assert "shapes/notebook.ttl" in ws.files()


PIPELINE = """name: export test
union_default_graph: true
parameters:
  country: NO
steps:
  - load: data/
    graph: http://e/data
  - name: enrich
    update: updates/
  - name: rules
    infer: rules/r.ttl
    graph: http://e/step
    clear: true
    ordered: true
    all_constructed: true
  - validate: shapes/
    fail_on: never
    report: out/report.ttl
  - export: out/all.nq
  - export: out/step.nt
    graph: http://e/step
"""


def _pipeline_folder(root):
    for sub in ["data", "updates", "rules", "shapes"]:
        (root / sub).mkdir(parents=True)
    (root / "data/a.ttl").write_text(DATA)
    (root / "updates/01.ru").write_text("INSERT { ?s <http://e/country> ?_country } WHERE { ?s a <http://e/Station> }")
    (root / "rules/r.ttl").write_text(RULES)
    (root / "shapes/s.ttl").write_text(SHAPES)
    (root / "pipeline.yaml").write_text(PIPELINE)


def _sorted_lines(path):
    return sorted(path.read_text().splitlines())


def test_pipeline_export(tmp_path):
    binary = _binary()
    if binary is None:
        pytest.skip("no tripletrail binary: set TRIPLETRAIL_BIN")
    import sys

    # The script, run in process, writes what `tripletrail pipeline run` writes.
    _pipeline_folder(tmp_path / "cli")
    subprocess.run([binary, "pipeline", "run", "pipeline.yaml"], cwd=tmp_path / "cli",
                   check=True, capture_output=True)
    _pipeline_folder(tmp_path / "py")
    script = subprocess.run([binary, "pipeline", "export", "pipeline.yaml", "--to", "python"],
                            cwd=tmp_path / "py", check=True, capture_output=True, text=True).stdout
    (tmp_path / "py/run.py").write_text(script)
    subprocess.run([sys.executable, "run.py"], cwd=tmp_path / "py", check=True)
    for f in ["out/all.nq", "out/step.nt"]:
        assert _sorted_lines(tmp_path / "py" / f) == _sorted_lines(tmp_path / "cli" / f), f
    assert len(_sorted_lines(tmp_path / "py/out/step.nt")) == 2

    # The DAG's tasks, run in order as Airflow would, on a dataset on disk.
    _pipeline_folder(tmp_path / "af")
    dag = subprocess.run([binary, "pipeline", "export", "pipeline.yaml", "--to", "airflow",
                          "--location", str(tmp_path / "af-kg")],
                         cwd=tmp_path / "af", check=True, capture_output=True, text=True).stdout
    tasks = []

    class Operator:
        def __init__(self, task_id, bash_command, env=None, append_env=False):
            tasks.append((task_id, bash_command, env))

        def __rshift__(self, other):
            return other

    class Dag:
        def __init__(self, **kwargs):
            pass

        def __enter__(self):
            return self

        def __exit__(self, *args):
            pass

    import types

    fake = {
        "airflow": types.SimpleNamespace(DAG=Dag),
        "airflow.providers.standard.operators.bash": types.SimpleNamespace(BashOperator=Operator),
        "pendulum": types.SimpleNamespace(datetime=lambda *a, **k: None),
    }
    saved = {k: sys.modules.get(k) for k in fake}
    sys.modules.update(fake)
    try:
        exec(compile(dag.replace('TRIPLETRAIL = "tripletrail"', f"TRIPLETRAIL = {binary!r}"), "dag.py", "exec"), {})
    finally:
        for k, v in saved.items():
            if v is None:
                sys.modules.pop(k)
            else:
                sys.modules[k] = v
    assert [t[0] for t in tasks] == ["load_1", "enrich", "rules", "validate_4", "export_5", "export_6"]
    for task_id, command, env in tasks:
        # Airflow renders the params into the environment.
        env = dict(os.environ, **{k: "NO" for k in env})
        subprocess.run(["bash", "-c", command], cwd=tmp_path / "af", env=env, check=True, capture_output=True)
    for f in ["out/all.nq", "out/step.nt"]:
        assert _sorted_lines(tmp_path / "af" / f) == _sorted_lines(tmp_path / "cli" / f), f
