"""A client for a TripleTrail server: a workbench workspace or a dataset, over HTTP.

`tripletrail.connect(url, workspace=...)` or `tripletrail.connect(url, dataset=...)` gives an object
with the `Engine` methods that make sense remotely (`query`, `explain`, `load`, `dump`,
`validate`, `infer`, and `update` on a dataset), returning the same classes as in process:
`QuerySolutions`, `QueryBoolean`, `Triple` lists, and a report with the attributes of
`ValidationReport`. Only the standard library is used (`urllib`).
"""

import json
import os
import urllib.error
import urllib.parse
import urllib.request

from tripletrail._tripletrail import (
    BlankNode,
    DefaultGraph,
    Literal,
    NamedNode,
    Quad,
    QueryResultsFormat,
    RdfFormat,
    Triple,
    parse,
    parse_query_results,
    serialize,
)

__all__ = ["connect", "RemoteError", "RemoteDataset", "Workspace", "RemoteValidationReport"]

_SH = "http://www.w3.org/ns/shacl#"

#: The client `connect` returned last: the target of the notebook magics when they name none.
default = None


class RemoteError(RuntimeError):
    """An answer of the server other than success: `status` and the server's `message`."""

    def __init__(self, status, message, url=None):
        super().__init__(f"{status}: {message}" if message else str(status))
        self.status = status
        self.message = message
        self.url = url


def connect(url, workspace=None, *, dataset=None, token=None, create=False, timeout=None):
    """Connects to the TripleTrail server at `url` (e.g. `http://localhost:7878`).

    With `workspace` (its id or name), a `Workspace`; `create=True` makes one by that name when
    there is none. With `dataset`, a `RemoteDataset`. `token` is sent as `Authorization: Bearer`
    (the update or admin token, or an API key); `TRIPLETRAIL_TOKEN` when not given. `timeout` is
    the HTTP timeout in seconds of each request (none by default).
    """
    global default
    if (workspace is None) == (dataset is None):
        raise ValueError("give a workspace or a dataset")
    if token is None:
        token = os.environ.get("TRIPLETRAIL_TOKEN")
    http = _Http(url, token, timeout)
    if dataset is not None:
        client = RemoteDataset(http, dataset)
    else:
        client = Workspace._open(http, workspace, create)
    default = client
    return client


class _Http:
    def __init__(self, url, token, timeout):
        self.url = url.rstrip("/")
        self.token = token
        self.timeout = timeout

    def request(self, method, path, *, params=None, data=None, headers=None, form=None):
        url = self.url + path
        if params:
            url += "?" + urllib.parse.urlencode(
                [(k, v) for k, v in params.items() if v is not None], doseq=True
            )
        # Names this client in a workspace's history, which the workbench shows.
        headers = dict(headers or {})
        headers.setdefault("X-TripleTrail-Client", "python")
        if form is not None:
            data = urllib.parse.urlencode(form).encode()
            headers["Content-Type"] = "application/x-www-form-urlencoded"
        if isinstance(data, str):
            data = data.encode()
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        request = urllib.request.Request(url, data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                return response.read(), response.headers.get("Content-Type", "")
        except urllib.error.HTTPError as e:
            body = e.read().decode("utf-8", "replace").strip()
            try:
                message = json.loads(body).get("error", body)
            except (ValueError, AttributeError):
                message = body
            if e.code == 503 and ("timeout" in message.lower() or "cancelled" in message):
                raise TimeoutError(message) from None
            raise RemoteError(e.code, message, url) from None

    def json(self, method, path, *, headers=None, **kwargs):
        headers = dict(headers or {}, Accept="application/json")
        body, _ = self.request(method, path, headers=headers, **kwargs)
        return json.loads(body) if body else None


# Terms of SPARQL JSON results.
def _term(t):
    if t is None:
        return None
    kind = t["type"]
    if kind == "uri":
        return NamedNode(t["value"])
    if kind == "bnode":
        return BlankNode(t["value"])
    if kind in ("literal", "typed-literal"):
        if "xml:lang" in t:
            return Literal(t["value"], language=t["xml:lang"])
        if "datatype" in t:
            return Literal(t["value"], datatype=NamedNode(t["datatype"]))
        return Literal(t["value"])
    if kind == "triple":
        v = t["value"]
        return Triple(_term(v["subject"]), _term(v["predicate"]), _term(v["object"]))
    raise ValueError(f"unknown term type {kind}")


def _rdf_format(format, path=None):
    if format is None:
        if path is None:
            raise ValueError("give a format")
        ext = os.path.basename(str(path)).rsplit(".", 1)[-1]
        found = RdfFormat.from_extension(ext)
        if found is None:
            raise ValueError(f"no RDF format for the extension .{ext}")
        return found
    if isinstance(format, RdfFormat):
        return format
    found = RdfFormat.from_media_type(format) if "/" in format else RdfFormat.from_extension(format)
    if found is None:
        raise ValueError(f"unknown RDF format {format}")
    return found


def _read_input(input, path):
    if path is not None:
        with open(path, "rb") as f:
            return f.read()
    if isinstance(input, str):
        return input.encode()
    if isinstance(input, (bytes, bytearray)):
        return bytes(input)
    if hasattr(input, "read"):
        data = input.read()
        return data.encode() if isinstance(data, str) else data
    raise TypeError("give input (bytes, str or a file object) or path")


def _write_output(data, output):
    if output is None:
        return data
    if hasattr(output, "write"):
        output.write(data)
    else:
        with open(output, "wb") as f:
            f.write(data)
    return None


def _graph_param(graph):
    if graph is None or isinstance(graph, DefaultGraph):
        return None
    if isinstance(graph, NamedNode):
        return graph.value
    return str(graph)


class QueryTriples(list):
    """The triples of a CONSTRUCT or DESCRIBE answered by a server: a list of `Triple`."""

    def serialize(self, output=None, format=None):
        """Writes the triples (`format`: a `RdfFormat`, N-Triples by default) to `output`, or
        returns them as bytes."""
        format = _rdf_format(format) if format is not None else RdfFormat.N_TRIPLES
        return serialize(list(self), output, format)

    def __repr__(self):
        return f"<QueryTriples {len(self)} triples>"


class RemoteValidationReport:
    """A validation report answered by a server, with the attributes of `ValidationReport`:
    `conforms`, `results` (one dict per result: `focus_node`, `result_path` as a SPARQL path or
    None, `value`, `source_shape`, `source_constraint_component`, `severity`, `message`,
    `messages`, `shape_iri`, `key`), `shape_counts`, `coverage` (workspaces), `triples()` and `serialize()` (the
    W3C report, fetched when first asked for)."""

    def __init__(self, answer, fetch_report):
        self.conforms = answer["conforms"]
        self.truncated = answer.get("truncated", False)
        self.result_count = answer.get("resultCount", len(answer.get("results", [])))
        self.coverage = answer.get("coverage")
        self.results = []
        for r in answer.get("results", []):
            messages = [_term(m) for m in r.get("messages", [])]
            self.results.append(
                {
                    "focus_node": _term(r.get("focusNode")),
                    "result_path": r.get("resultPath"),
                    "value": _term(r.get("value")),
                    "source_shape": _term(r.get("sourceShape")),
                    "source_constraint_component": _term(r.get("sourceConstraintComponent")),
                    "severity": _term(r.get("severity")),
                    "message": messages[0] if messages else None,
                    "messages": messages,
                    "shape_iri": r.get("shapeIri"),
                    "key": r.get("key"),
                }
            )
        self.shape_counts = [
            {
                "shape": _term(c["shape"]),
                "shape_iri": c.get("shapeIri"),
                "property": c.get("property"),
                "focus_nodes": c.get("focusNodes"),
                "value_nodes": c.get("valueNodes"),
                "distinct_focus_nodes": c.get("distinctFocusNodes"),
                "failing_focus_nodes": c.get("failingFocusNodes"),
            }
            for c in answer.get("shapes", [])
        ]
        self._fetch_report = fetch_report
        self._triples = None

    def __len__(self):
        return self.result_count

    def triples(self):
        """The report's triples, as `Triple`s."""
        if self._triples is None:
            data = self._fetch_report()
            self._triples = [q.triple for q in parse(data, RdfFormat.N_TRIPLES)]
        return list(self._triples)

    def serialize(self, output=None, format=None):
        """Writes the W3C validation report (`format`: a `RdfFormat`, media type or extension;
        Turtle by default) to `output`, or returns it as bytes."""
        format = _rdf_format(format) if format is not None else RdfFormat.TURTLE
        return serialize(self.triples(), output, format, prefixes={"sh": _SH})

    def __repr__(self):
        return f"<ValidationReport conforms={self.conforms} results={len(self)}>"


class _Client:
    def __init__(self, http):
        self._http = http

    def _sparql_path(self, step=None):
        raise NotImplementedError

    def query(self, query, *, timeout=None, default_graph=None, named_graphs=None, step=None):
        """Runs a SPARQL query on the server: `QuerySolutions`, `QueryBoolean` or
        `QueryTriples` (a list of `Triple`), as `Engine.query`. Past `timeout` seconds,
        `TimeoutError`. `default_graph` and `named_graphs` (IRIs) set the dataset of the query
        as the protocol's `default-graph-uri` and `named-graph-uri`."""
        form = [("query", query)]
        if timeout is not None:
            form.append(("timeout", str(timeout)))
        for g in _as_list(default_graph):
            form.append(("default-graph-uri", _graph_param(g)))
        for g in _as_list(named_graphs):
            form.append(("named-graph-uri", _graph_param(g)))
        body, content_type = self._http.request(
            "POST",
            self._sparql_path(step),
            form=form,
            headers={"Accept": "application/sparql-results+json, application/n-triples;q=0.9"},
        )
        if content_type.startswith("application/n-triples") or content_type.startswith("text/plain"):
            return QueryTriples(q.triple for q in parse(body, RdfFormat.N_TRIPLES))
        return parse_query_results(body, QueryResultsFormat.JSON)

    def explain(self, query):
        """The plan of a query, with row counts and timings, as JSON text (as `Engine.explain`)."""
        body, _ = self._http.request(
            "POST", self._explain_path(), form=[("query", query)],
            headers={"Accept": "application/json"},
        )
        return body.decode()


def _as_list(value):
    if value is None:
        return []
    if isinstance(value, (list, tuple, set)):
        return list(value)
    return [value]


class RemoteDataset(_Client):
    """A dataset of a TripleTrail (or Fuseki-compatible) server: the SPARQL Protocol and the
    Graph Store Protocol."""

    def __init__(self, http, name):
        super().__init__(http)
        self.name = name
        self._base = "/" + urllib.parse.quote(name)

    def _sparql_path(self, step=None):
        if step is not None:
            raise ValueError("step= is for workspaces")
        return self._base + "/sparql"

    def _explain_path(self):
        return self._base + "/explain"

    def update(self, update, *, timeout=None):
        """Runs a SPARQL update on the dataset."""
        form = [("update", update)]
        if timeout is not None:
            form.append(("timeout", str(timeout)))
        self._http.request("POST", self._base + "/update", form=form)

    def load(self, input=None, format=None, *, path=None, base_iri=None, to_graph=None):
        """Adds RDF (bytes, str, a file object, or `path`) to the dataset, in `to_graph` or the
        default graph (Graph Store Protocol `POST`). `format`: a `RdfFormat`, media type or
        extension (from `path` if omitted)."""
        format = _rdf_format(format, path)
        data = _read_input(input, path)
        graph = _graph_param(to_graph)
        params = {"graph": graph} if graph else ({} if format.supports_datasets else {"default": ""})
        if base_iri is not None and not format.supports_datasets:
            # The server resolves against the graph; base IRIs are for the parser here.
            quads = parse(data, format, base_iri=base_iri)
            data, format = serialize((q.triple for q in quads), None, RdfFormat.N_TRIPLES), RdfFormat.N_TRIPLES
        self._http.request(
            "POST", self._base + "/data", params=params, data=data,
            headers={"Content-Type": format.media_type},
        )

    def dump(self, output=None, format=None, *, from_graph=None):
        """Writes a graph (`from_graph`, or the default graph with `DefaultGraph()`) or the whole
        dataset to `output`, or returns it as bytes. `format`: N-Triples for a graph and N-Quads
        for the dataset by default."""
        if from_graph is None:
            params, default = {}, RdfFormat.N_QUADS
        elif isinstance(from_graph, DefaultGraph):
            params, default = {"default": ""}, RdfFormat.N_TRIPLES
        else:
            params, default = {"graph": _graph_param(from_graph)}, RdfFormat.N_TRIPLES
        format = _rdf_format(format) if format is not None else default
        body, _ = self._http.request(
            "GET", self._base + "/data", params=params, headers={"Accept": format.media_type}
        )
        return _write_output(body, output)

    def validate(self, shapes_graph, *, data_graph=None, timeout=None):
        """Validates `data_graph` (the default graph if omitted) against the shapes of the
        named graph `shapes_graph`, on the server, without writing to the dataset."""
        params = {
            "shapes": _graph_param(shapes_graph),
            "graph": _graph_param(data_graph),
            "timeout": timeout,
        }
        path = "/$/datasets/" + urllib.parse.quote(self.name) + "/validate"
        answer = self._http.json("GET", path, params=dict(params, limit=1_000_000))

        def report():
            return self._http.request(
                "GET", path, params=params, headers={"Accept": "application/n-triples"}
            )[0]

        return RemoteValidationReport(answer, report)

    def infer(self, shapes_graph, *, data_graph=None, timeout=None):
        """What the SHACL-AF rules of `shapes_graph` infer from `data_graph`, as a list of
        `Triple` (the dataset is not changed)."""
        params = {
            "shapes": _graph_param(shapes_graph),
            "graph": _graph_param(data_graph),
            "timeout": timeout,
        }
        path = "/$/datasets/" + urllib.parse.quote(self.name) + "/infer"
        body, _ = self._http.request(
            "GET", path, params=params, headers={"Accept": "application/n-triples"}
        )
        return [q.triple for q in parse(body, RdfFormat.N_TRIPLES)]

    def __repr__(self):
        return f"<RemoteDataset {self.name} at {self._http.url}>"


class Workspace(_Client):
    """A workbench workspace: a folder of files (`data/`, `shapes/`, `rules/`, `queries/`,
    `updates/`, `tests/`, `pipeline.yaml`) and the scratch store built from them. Its store
    holds what its files hold, so it is changed by writing files (`load`, `write_file`), not
    by updates, unless it is an overlay of a dataset (`overlay`): then updates change it, and
    never the dataset. What is done here shows in the workbench at `url`."""

    def __init__(self, http, info):
        super().__init__(http)
        self.id = info["id"]
        self.name = info.get("name")
        self._base = "/$/workspaces/" + self.id

    @classmethod
    def _open(cls, http, workspace, create):
        listed = http.json("GET", "/$/workspaces")["workspaces"]
        found = [w for w in listed if w["id"] == workspace] or [
            w for w in listed if w.get("name") == workspace
        ]
        if len(found) > 1:
            raise ValueError(f"{len(found)} workspaces are named {workspace}: give its id")
        if found:
            return cls(http, found[0])
        if not create:
            raise LookupError(f"no workspace {workspace} on {http.url}")
        made = http.json(
            "POST", "/$/workspaces", data=json.dumps({"name": workspace}),
            headers={"Content-Type": "application/json"},
        )
        return cls(http, made)

    @property
    def url(self):
        """The workspace in the workbench."""
        return f"{self._http.url}/$/ui/workbench/{self.id}"

    def _sparql_path(self, step=None):
        if step is None:
            return self._base + "/sparql"
        return f"{self._base}/pipeline/{int(step)}/sparql"

    def _explain_path(self):
        return self._base + "/explain"

    def info(self):
        """The workspace: its files with their sizes and triples, graphs, expiry."""
        return self._http.json("GET", self._base)

    def files(self):
        """The paths of its files."""
        return [f["path"] for f in self.info()["files"]]

    def read_file(self, path):
        """A file's text."""
        return self._http.request("GET", self._base + "/files/" + _file_path(path))[0].decode()

    def write_file(self, path, text):
        """Writes a file (replacing it); an RDF file that does not parse is refused."""
        self._http.request(
            "PUT", self._base + "/files/" + _file_path(path), data=text,
            headers={"Content-Type": "text/plain; charset=utf-8"},
        )

    def delete_file(self, path):
        self._http.request("DELETE", self._base + "/files/" + _file_path(path))

    def update(self, update, *, timeout=None):
        """Runs a SPARQL update on an overlay's store (see `overlay`). A plain workspace's
        store holds what its files hold: put the update in updates/ (write_file) and run it
        as a pipeline step instead."""
        form = [("update", update)]
        if timeout is not None:
            form.append(("timeout", str(timeout)))
        try:
            self._http.request("POST", self._base + "/update", form=form)
        except RemoteError as e:
            if e.status == 405:
                raise NotImplementedError(
                    "a workspace's store holds what its files hold: make it an overlay of a "
                    "dataset (overlay), or put the update in updates/ (write_file) and run it "
                    "as a pipeline step"
                ) from e
            raise

    def overlay(self, dataset=None):
        """Makes the workspace an overlay of `dataset`: its store reads the whole dataset as
        it is now, read only, and keeps its own changes (updates, pipeline steps) apart; the
        dataset is never written. Taking a dataset again drops the changes. Without
        `dataset`, what the overlay is now (`dataset`, `at`, `quads`, `changes`), or None."""
        if dataset is None:
            return self.info().get("overlay")
        return self._http.json(
            "POST", self._base + "/overlay", data=json.dumps({"dataset": dataset}),
            headers={"Content-Type": "application/json"},
        )

    def patch(self, format="rdf-patch"):
        """The overlay's changes: an RDF Patch (`rdf-patch`), or the `added` or `removed`
        quads as N-Quads, for an explicit, reviewed apply to the dataset."""
        return self._http.request(
            "GET", self._base + "/overlay/patch", params={"format": format}
        )[0].decode()

    def drop_changes(self):
        """Drops the overlay's changes: back to the dataset as taken."""
        self._http.request("DELETE", self._base + "/overlay/changes")

    def detach(self):
        """Makes an overlay a plain workspace again (its changes go)."""
        self._http.request("DELETE", self._base + "/overlay")

    def load(self, input=None, format=None, *, path=None, file=None, base_iri=None, to_graph=None):
        """Adds RDF to the workspace as a file of `data/` (the default graph): `file` names it
        (`data/` + the name of `path`, or `data/loaded.<ext>`, by default). It replaces a file
        of that name."""
        if to_graph is not None:
            raise ValueError(
                "a workspace's data/ is its default graph: write shapes or rules with "
                "write_file('shapes/…') or ('rules/…')"
            )
        format = _rdf_format(format, path)
        data = _read_input(input, path)
        if base_iri is not None:
            data = serialize(parse(data, format, base_iri=base_iri), None, RdfFormat.N_QUADS
                             if format.supports_datasets else RdfFormat.N_TRIPLES)
            format = RdfFormat.N_QUADS if format.supports_datasets else RdfFormat.N_TRIPLES
        if file is None:
            name = os.path.basename(str(path)) if path is not None else f"loaded.{format.file_extension}"
            file = "data/" + name
        self._http.request(
            "PUT", self._base + "/files/" + _file_path(file), data=data,
            headers={"Content-Type": format.media_type},
        )

    def dump(self, output=None, format=None, *, from_graph=None, step=None):
        """Writes the scratch store (or the store after pipeline step `step`) to `output`, or
        returns it as bytes: a graph (`from_graph`, `DefaultGraph()` for the default graph) in
        N-Triples, or all graphs in N-Quads, by default."""
        if from_graph is None:
            rows = self.query(
                "SELECT ?s ?p ?o ?g WHERE { { ?s ?p ?o } UNION { GRAPH ?g { ?s ?p ?o } } }",
                step=step,
            )
            quads = [
                Quad(r["s"], r["p"], r["o"], r["g"] if r["g"] is not None else DefaultGraph())
                for r in rows
            ]
            format = _rdf_format(format) if format is not None else RdfFormat.N_QUADS
            return serialize(quads, output, format)
        if isinstance(from_graph, DefaultGraph):
            where = "{ ?s ?p ?o }"
        else:
            where = f"{{ GRAPH <{_graph_param(from_graph)}> {{ ?s ?p ?o }} }}"
        triples = self.query(f"CONSTRUCT {{ ?s ?p ?o }} WHERE {where}", step=step)
        format = _rdf_format(format) if format is not None else RdfFormat.N_TRIPLES
        return serialize(list(triples), output, format)

    def validate(self, *, rules=True, timeout=None):
        """Validates the data, plus what the rules of `rules/` infer (`rules=False` for the data
        alone), against `shapes/`: a report with the attributes of `ValidationReport`, and
        `coverage` (classes no shape targets, shapes that matched nothing)."""
        form = {"limit": "1000000"}
        if not rules:
            form["rules"] = "false"
        if timeout is not None:
            form["timeout"] = str(timeout)
        path = self._base + "/validate"
        body, _ = self._http.request("POST", path, form=form, headers={"Accept": "application/json"})

        def report():
            return self._http.request(
                "POST", path, form={k: v for k, v in form.items() if k != "limit"},
                headers={"Accept": "application/n-triples"},
            )[0]

        return RemoteValidationReport(json.loads(body), report)

    def infer(self, *, max_iterations=None, include_existing=False, shapes=False,
              provenance=False, timeout=None):
        """What the rules of `rules/` (of `shapes/` with `shapes=True`) infer from the data, as
        a list of `Triple`. With `provenance=True`, a list of `(Triple, origin)` pairs, `origin`
        a dict (`kind`, `rule`, `shape`, `focus`, `pass`, `bindings`, terms as SPARQL JSON)."""
        form = {}
        if max_iterations is not None:
            form["max_iterations"] = str(max_iterations)
        if include_existing:
            form["include_existing"] = "true"
        if shapes:
            form["shapes"] = "shapes"
        if timeout is not None:
            form["timeout"] = str(timeout)
        path = self._base + "/infer"
        if provenance:
            form.update(provenance="true", limit="10000000")
            answer = json.loads(self._http.request(
                "POST", path, form=form, headers={"Accept": "application/json"})[0])
            return [
                (Triple(_term(t["subject"]), _term(t["predicate"]), _term(t["object"])), origin)
                for t, origin in zip(answer["triples"], answer.get("provenance", []))
            ]
        body, _ = self._http.request(
            "POST", path, form=form, headers={"Accept": "application/n-triples"}
        )
        return [q.triple for q in parse(body, RdfFormat.N_TRIPLES)]

    def tests(self, names=None, *, record=False):
        """Runs the workspace's tests (all, or those named): for each its `name`, `passed`,
        `missing` and `unexpected`."""
        params = {"record": "true"} if record else None
        body = json.dumps({"tests": list(names)}) if names else ""
        return self._http.json("POST", self._base + "/tests", params=params, data=body,
                               headers={"Content-Type": "application/json"})

    def pipeline(self):
        """The steps of its `pipeline.yaml`, with what their last run did."""
        return self._http.json("GET", self._base + "/pipeline")

    def run_pipeline(self, start=None, to=None, values=None):
        """Runs steps of its `pipeline.yaml` (from the first not run, failed or changed, to the
        last, by default; a step by number or name), as the workbench's Pipeline tab does."""
        body = {k: v for k, v in {"from": start, "to": to, "values": values}.items() if v is not None}
        return self._http.json("POST", self._base + "/pipeline", data=json.dumps(body),
                               headers={"Content-Type": "application/json"})

    def step_endpoint(self, step):
        """`(base_url, dataset)` of the store a waiting python step works on, served as a
        Fuseki dataset (`{base_url}/{dataset}/sparql`, `/update`, `/data`): what code written
        as `function(base_url, dataset)` for Fuseki takes. A run of the pipeline stops at a
        python step and waits for its code; `complete_step` then takes what it left."""
        return f"{self._http.url}{self._base}/pipeline", str(int(step))

    def complete_step(self, step):
        """Takes what the code of waiting python step `step` left in its store as the step's
        result; the next `run_pipeline` goes on from the step after it."""
        return self._http.json("POST", f"{self._base}/pipeline/{int(step)}/complete")

    def run_step(self, step, function, **kwargs):
        """Runs `function(base_url, dataset, **kwargs)` against the store of waiting python step
        `step` (see `step_endpoint`) and completes the step. Code that sends no token works
        only against a server that asks for none."""
        base_url, dataset = self.step_endpoint(step)
        function(base_url, dataset, **kwargs)
        return self.complete_step(step)

    def history(self):
        """What clients other than the workbench ran against the workspace (this one too),
        newest first: each run's `op`, `client`, `text`, `status`, `ms` and `summary`. The
        workbench's History tab shows them, and opens a validation's report or an inference's
        triples with their why? (ask `infer(provenance=True)` for that)."""
        return self._http.json("GET", self._base + "/history")["runs"]

    def blame(self, subject, predicate, object):
        """Which steps of the last pipeline run added or removed the triple, and the file."""
        params = {"subject": str(subject), "predicate": str(predicate), "object": str(object)}
        return self._http.json("GET", self._base + "/pipeline/blame", params=params)

    def __repr__(self):
        return f"<Workspace {self.name or self.id} at {self._http.url}>"


def _file_path(path):
    return "/".join(urllib.parse.quote(p) for p in str(path).strip("/").split("/"))
