"""An rdflib store backed by a tripletrail Engine.

```python
import rdflib
from tripletrail import Engine
from tripletrail.rdflib import TripleTrailStore

ds = rdflib.Dataset(store=TripleTrailStore(store=Engine()))
# or, through the rdflib plugin: rdflib.Dataset(store="TripleTrail")
```

It needs only rdflib (`pip install tripletrail[rdflib]`). It is adapted from oxrdflib's
`OxigraphStore` (Copyright 2020 Oxrdflib authors, BSD-3-Clause, see
`licenses/oxrdflib-LICENSE.md`), with tripletrail's own term classes.
"""

import shutil
from typing import Any, Dict, Generator, Iterable, Iterator, Mapping, Optional, Tuple, Union

from rdflib import Graph
from rdflib.graph import DATASET_DEFAULT_GRAPH_ID
from rdflib.plugins.sparql.sparql import Query, Update
from rdflib.query import Result
from rdflib.store import VALID_STORE, Store
from rdflib.term import BNode, Identifier, Literal, Node, URIRef, Variable

from tripletrail._tripletrail import (
    BlankNode,
    DefaultGraph,
    Engine,
    NamedNode,
    QueryBoolean,
    QuerySolutions,
    QueryTriples,
    Triple,
)
from tripletrail._tripletrail import Literal as MgLiteral
from tripletrail._tripletrail import Quad as MgQuad
from tripletrail._tripletrail import Variable as MgVariable

__all__ = ["TripleTrailStore", "from_tripletrail", "to_tripletrail"]

_Triple = Tuple[Node, Node, Node]
_Quad = Tuple[Node, Node, Node, Graph]
_TriplePattern = Tuple[Optional[Node], Optional[Node], Optional[Node]]


def to_tripletrail(term: Any, context: Optional[Graph] = None) -> Any:
    """An rdflib term (or triple with `context`, quad, or graph) as a tripletrail term or quad."""
    if term is None:
        return None
    if term == DATASET_DEFAULT_GRAPH_ID:
        return DefaultGraph()
    if isinstance(term, URIRef):
        return NamedNode(term)
    if isinstance(term, BNode):
        return BlankNode(term)
    if isinstance(term, Literal):
        if term.language:
            return MgLiteral(term, language=term.language)
        return MgLiteral(term, datatype=NamedNode(term.datatype) if term.datatype else None)
    if isinstance(term, Graph):
        return to_tripletrail(term.identifier)
    if isinstance(term, tuple):
        if len(term) == 3:
            return MgQuad(to_tripletrail(term[0]), to_tripletrail(term[1]), to_tripletrail(term[2]), to_tripletrail(context))
        if len(term) == 4:
            return MgQuad(to_tripletrail(term[0]), to_tripletrail(term[1]), to_tripletrail(term[2]), to_tripletrail(term[3]))
    raise ValueError(f"Unexpected rdflib term: {term!r}")


def from_tripletrail(term: Any) -> Any:
    """A tripletrail term as an rdflib term (a triple as a tuple)."""
    if term is None:
        return None
    if isinstance(term, NamedNode):
        return URIRef(term.value)
    if isinstance(term, BlankNode):
        return BNode(term.value)
    if isinstance(term, MgLiteral):
        if term.language:
            return Literal(term.value, lang=term.language)
        return Literal(term.value, datatype=URIRef(term.datatype.value))
    if isinstance(term, Triple):
        return from_tripletrail(term.subject), from_tripletrail(term.predicate), from_tripletrail(term.object)
    raise ValueError(f"Unexpected tripletrail term: {term!r}")


def _pattern(triple: _TriplePattern, context: Optional[Graph]) -> Tuple[Any, Any, Any, Any]:
    s, p, o = triple
    return to_tripletrail(s), to_tripletrail(p), to_tripletrail(o), to_tripletrail(context)


class TripleTrailStore(Store):
    """An rdflib `Store` on a `tripletrail.Engine` (a new in-memory one by default)."""

    context_aware: bool = True
    formula_aware: bool = False
    transaction_aware: bool = False
    graph_aware: bool = True

    def __init__(
        self,
        configuration: Optional[str] = None,
        identifier: Optional[Identifier] = None,
        *,
        store: Optional[Engine] = None,
    ) -> None:
        self._store = store
        self._prefix_for_namespace: Dict[URIRef, str] = {}
        self._namespace_for_prefix: Dict[str, URIRef] = {}
        super().__init__(configuration, identifier)

    def open(self, configuration: str, create: bool = False) -> Optional[int]:  # noqa: ARG002
        if self._store is not None:
            raise ValueError("The open function should be called before any RDF operation")
        self._store = Engine(configuration)
        return VALID_STORE

    def close(self, commit_pending_transaction: bool = False) -> None:  # noqa: ARG002
        self._store = None

    def destroy(self, configuration: str) -> None:
        shutil.rmtree(configuration)

    def gc(self) -> None:
        pass

    @property
    def _inner(self) -> Engine:
        if self._store is None:
            self._store = Engine()
        return self._store

    def _graph(self, graph_name: Any) -> Graph:
        if isinstance(graph_name, DefaultGraph):
            return Graph(identifier=DATASET_DEFAULT_GRAPH_ID, store=self)
        return Graph(identifier=from_tripletrail(graph_name), store=self)

    def add(self, triple: _Triple, context: Graph, quoted: bool = False) -> None:
        if quoted:
            raise ValueError("tripletrail stores are not formula aware")
        self._inner.add(to_tripletrail(triple, context))
        super().add(triple, context, quoted)

    def addN(self, quads: Iterable[_Quad]) -> None:  # noqa: N802
        quads = list(quads)
        self._inner.extend([to_tripletrail(q) for q in quads])
        for s, p, o, g in quads:
            super().add((s, p, o), g)

    def remove(self, triple: _TriplePattern, context: Optional[Graph] = None) -> None:
        for q in list(self._inner.quads_for_pattern(*_pattern(triple, context))):
            self._inner.remove(q)
        super().remove(triple, context)

    def triples(
        self, triple_pattern: _TriplePattern, context: Optional[Graph] = None
    ) -> Iterator[Tuple[_Triple, Iterator[Optional[Graph]]]]:
        try:
            pattern = _pattern(triple_pattern, context)
        except (TypeError, ValueError):
            return iter(())
        return (
            (
                (from_tripletrail(q.subject), from_tripletrail(q.predicate), from_tripletrail(q.object)),
                iter((self._graph(q.graph_name),)),
            )
            for q in self._inner.quads_for_pattern(*pattern)
        )

    def __len__(self, context: Optional[Graph] = None) -> int:
        kwargs: Dict[str, Any] = (
            {"use_default_graph_as_union": True} if context is None else {"default_graph": to_tripletrail(context)}
        )
        result = self._inner.query("SELECT (COUNT(DISTINCT TRIPLE(?s, ?p, ?o)) AS ?c) WHERE { ?s ?p ?o }", **kwargs)
        return int(next(result)[0].value)

    def contexts(self, triple: Optional[_Triple] = None) -> Generator[Graph, None, None]:
        if triple is None:
            return (self._graph(g) for g in self._inner.named_graphs())
        return (self._graph(q.graph_name) for q in self._inner.quads_for_pattern(*_pattern(triple, None)))

    def query(
        self,
        query: Union[Query, str],
        initNs: Mapping[str, Any],  # noqa: N803
        initBindings: Mapping[str, Identifier],  # noqa: N803
        queryGraph: str,  # noqa: N803
        **kwargs: object,
    ) -> Result:
        if isinstance(query, Query):
            raise NotImplementedError("Already parsed queries are not supported by the tripletrail store")
        for kwarg in kwargs:
            raise NotImplementedError(f"The parameter {kwarg} is not supported by the tripletrail store")
        result = self._inner.query(
            query,
            use_default_graph_as_union=queryGraph == "__UNION__",
            default_graph=(to_tripletrail(queryGraph) if isinstance(queryGraph, Node) else None),
            prefixes=dict(self._namespace_for_prefix, **initNs),
            substitutions={MgVariable(k): to_tripletrail(v) for k, v in initBindings.items()},
        )
        if isinstance(result, QueryBoolean):
            out = Result("ASK")
            out.askAnswer = bool(result)
        elif isinstance(result, QuerySolutions):
            out = Result("SELECT")
            out.vars = [Variable(v.value) for v in result.variables]
            out.bindings = ({v: from_tripletrail(val) for v, val in zip(out.vars, solution)} for solution in result)
        elif isinstance(result, QueryTriples):
            out = Result("CONSTRUCT")
            out.graph = Graph()
            out.graph += (from_tripletrail(t) for t in result)
        else:
            raise ValueError(f"Unexpected query result: {result}")
        return out

    def update(
        self,
        update: Union[Update, str],
        initNs: Mapping[str, Any],  # noqa: N803
        initBindings: Mapping[str, Identifier],  # noqa: N803
        queryGraph: str,  # noqa: N803
        **kwargs: object,
    ) -> None:
        if initBindings:
            raise NotImplementedError("initBindings are not supported by the tripletrail store")
        if queryGraph != DATASET_DEFAULT_GRAPH_ID:
            raise NotImplementedError(f"Only {DATASET_DEFAULT_GRAPH_ID} is supported by the tripletrail store")
        if isinstance(update, Update):
            raise NotImplementedError("Already parsed updates are not supported by the tripletrail store")
        for kwarg in kwargs:
            raise NotImplementedError(f"The parameter {kwarg} is not supported by the tripletrail store")
        self._inner.update(update, prefixes=dict(self._namespace_for_prefix, **initNs))

    def commit(self) -> None:
        pass

    def rollback(self) -> None:
        pass

    def add_graph(self, graph: Graph) -> None:
        self._inner.add_graph(to_tripletrail(graph))

    def remove_graph(self, graph: Graph) -> None:
        self._inner.remove_graph(to_tripletrail(graph))

    def bind(self, prefix: str, namespace: URIRef, override: bool = True) -> None:
        if not override and (prefix in self._namespace_for_prefix or namespace in self._prefix_for_namespace):
            return
        self._delete_from_prefix(prefix)
        self._delete_from_namespace(namespace)
        self._namespace_for_prefix[prefix] = namespace
        self._prefix_for_namespace[namespace] = prefix

    def _delete_from_prefix(self, prefix: str) -> None:
        if prefix not in self._namespace_for_prefix:
            return
        namespace = self._namespace_for_prefix.pop(prefix)
        self._delete_from_namespace(namespace)

    def _delete_from_namespace(self, namespace: str) -> None:
        if namespace not in self._prefix_for_namespace:
            return
        prefix = self._prefix_for_namespace.pop(namespace)
        self._delete_from_prefix(prefix)

    def prefix(self, namespace: URIRef) -> Optional[str]:
        return self._prefix_for_namespace.get(namespace)

    def namespace(self, prefix: str) -> Optional[URIRef]:
        return self._namespace_for_prefix.get(prefix)

    def namespaces(self) -> Iterator[Tuple[str, URIRef]]:
        yield from self._namespace_for_prefix.items()
