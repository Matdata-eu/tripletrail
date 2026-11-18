"""The tripletrail engine in Python (see the README).

`tripletrail.connect(url, workspace=...)` works on a server's workbench workspace or dataset
over HTTP with the same methods and result classes (`tripletrail.remote`), and
`%load_ext tripletrail` adds the `%%sparql` and `%%shacl` notebook magics.

The module also has pyoxigraph's classes and functions (`NamedNode`, `Literal`, `Quad`,
`Dataset`, `parse`, `serialize`, ...), so `import tripletrail as ox` replaces
`import pyoxigraph as ox`; `Store` is the `Engine`.
"""

from tripletrail._tripletrail import (
    BaseDirection,
    BlankNode,
    CanonicalizationAlgorithm,
    Dataset,
    DefaultGraph,
    Engine,
    Literal,
    NamedNode,
    PreparedQuery,
    Quad,
    QuadParser,
    QueryBoolean,
    QueryResultsFormat,
    QuerySolution,
    QuerySolutions,
    QueryTriples,
    RdfFormat,
    Triple,
    ValidationReport,
    Variable,
    __version__,
    diff,
    parse,
    parse_query_results,
    serialize,
)

from tripletrail.remote import connect

Store = Engine
"""pyoxigraph's name for the store: the same class as `Engine`."""


def load_ipython_extension(ipython):
    """`%load_ext tripletrail`: the `%%sparql` and `%%shacl` cell magics (see `tripletrail.magics`)."""
    from tripletrail.magics import load_ipython_extension as load

    load(ipython)

__all__ = [
    "BaseDirection",
    "BlankNode",
    "CanonicalizationAlgorithm",
    "Dataset",
    "DefaultGraph",
    "Engine",
    "Literal",
    "NamedNode",
    "PreparedQuery",
    "Quad",
    "QuadParser",
    "QueryBoolean",
    "QueryResultsFormat",
    "QuerySolution",
    "QuerySolutions",
    "QueryTriples",
    "RdfFormat",
    "Store",
    "Triple",
    "ValidationReport",
    "Variable",
    "__version__",
    "connect",
    "parse",
    "parse_query_results",
    "serialize",
]
