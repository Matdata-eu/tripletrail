"""Notebook magics: `%load_ext tripletrail`, then

```
%%sparql [TARGET] [--step N] [--timeout S] [--out NAME]
SELECT ...

%%shacl [TARGET] [--data GRAPH] [--out NAME]
<shapes in Turtle>
```

`TARGET` names a variable holding an `Engine`, a workspace or a dataset from
`tripletrail.connect`; without it, the client `tripletrail.connect` returned last. Results show as a
table (a `pandas.DataFrame` when pandas is installed), and `--out NAME` also keeps them in
`NAME`. `%%shacl` validates the data against the cell's shapes: on an `Engine` in a named graph
dropped afterwards; on a workspace as its file `shapes/notebook.ttl` (an empty cell validates
with the workspace's shapes as they are); on a dataset, give the shapes graph with `--shapes`.
IPython is needed only for the magics.
"""

import html
import shlex

import tripletrail
from tripletrail import remote

_SHAPES = "urn:x-tripletrail:notebook:shapes"


def _table(columns, rows):
    """`rows` (lists of values) as a DataFrame, or as a table that notebooks render."""
    try:
        import pandas
    except ImportError:
        return _Table(columns, rows)
    return pandas.DataFrame(rows, columns=columns)


class _Table:
    def __init__(self, columns, rows):
        self.columns = columns
        self.rows = rows

    def __len__(self):
        return len(self.rows)

    def _repr_html_(self):
        head = "".join(f"<th>{html.escape(str(c))}</th>" for c in self.columns)
        body = "".join(
            "<tr>" + "".join(f"<td>{html.escape(_show(v))}</td>" for v in row) + "</tr>"
            for row in self.rows
        )
        return f"<table><thead><tr>{head}</tr></thead><tbody>{body}</tbody></table>"

    def __repr__(self):
        lines = ["\t".join(map(str, self.columns))]
        lines += ["\t".join(_show(v) for v in row) for row in self.rows]
        return "\n".join(lines)


def _show(value):
    if value is None:
        return ""
    return str(value)


def results_table(results):
    """Query results (or a validation report) as a table: one column per variable, or
    subject / predicate / object for triples, or the report's results."""
    if isinstance(results, tripletrail.QueryBoolean):
        return bool(results)
    if isinstance(results, tripletrail.QuerySolutions):
        columns = [v.value for v in results.variables]
        return _table(columns, [[row[c] for c in columns] for row in results])
    if isinstance(results, (tripletrail.ValidationReport, remote.RemoteValidationReport)):
        columns = ["focus_node", "result_path", "value", "severity",
                   "source_constraint_component", "source_shape", "message"]
        return _table(columns, [[r[c] for c in columns] for r in results.results])
    triples = list(results)
    return _table(["subject", "predicate", "object"],
                  [[t.subject, t.predicate, t.object] for t in triples])


def _parse(line, options):
    """`[TARGET] [--name value ...]`."""
    words = shlex.split(line)
    target = None
    values = {}
    i = 0
    while i < len(words):
        w = words[i]
        if w.startswith("--"):
            name = w[2:]
            if name not in options:
                raise ValueError(f"unknown option {w} (expected {', '.join('--' + o for o in options)})")
            if i + 1 >= len(words):
                raise ValueError(f"{w} needs a value")
            values[name] = words[i + 1]
            i += 2
        elif target is None:
            target = w
            i += 1
        else:
            raise ValueError(f"unexpected {w}")
    return target, values


def _target(shell, name):
    if name is None:
        if remote.default is None:
            raise ValueError("name an Engine or a client, or call tripletrail.connect first")
        return remote.default
    try:
        return shell.user_ns[name]
    except KeyError:
        raise ValueError(f"no variable {name}") from None


def sparql(shell, line, cell):
    target, options = _parse(line, ["step", "timeout", "out"])
    client = _target(shell, target)
    kwargs = {}
    if "timeout" in options:
        kwargs["timeout"] = float(options["timeout"])
    if "step" in options:
        kwargs["step"] = int(options["step"])
    results = client.query(cell, **kwargs)
    if results is None:
        return None
    table = results_table(results)
    if "out" in options:
        shell.user_ns[options["out"]] = table
    return table


def shacl(shell, line, cell):
    target, options = _parse(line, ["data", "shapes", "out"])
    client = _target(shell, target)
    data = tripletrail.NamedNode(options["data"]) if "data" in options else None
    if isinstance(client, remote.Workspace):
        if data is not None or "shapes" in options:
            raise ValueError("a workspace validates its data with its shapes/: no --data or --shapes")
        if cell.strip():
            client.write_file("shapes/notebook.ttl", cell)
        report = client.validate()
    elif isinstance(client, remote.RemoteDataset):
        if cell.strip() or "shapes" not in options:
            raise ValueError("on a dataset, give the graph of its shapes: --shapes IRI, and no cell")
        report = client.validate(tripletrail.NamedNode(options["shapes"]), data_graph=data)
    else:
        shapes = tripletrail.NamedNode(options.get("shapes", _SHAPES))
        if "shapes" not in options:
            client.load(cell, "text/turtle", to_graph=shapes)
        try:
            report = client.validate(shapes, data_graph=data)
        finally:
            if "shapes" not in options:
                client.remove_graph(shapes)
    table = results_table(report)
    if "out" in options:
        shell.user_ns[options["out"]] = report
    print(f"conforms: {report.conforms}, {len(report)} results")
    return table


def load_ipython_extension(ipython):
    def sparql_magic(line, cell):
        return sparql(ipython, line, cell)

    def shacl_magic(line, cell):
        return shacl(ipython, line, cell)

    sparql_magic.__doc__ = __doc__
    shacl_magic.__doc__ = __doc__
    ipython.register_magic_function(sparql_magic, "cell", "sparql")
    ipython.register_magic_function(shacl_magic, "cell", "shacl")
