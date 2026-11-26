//! `tripletrail`: the tripletrail [`Engine`](tripletrail::Engine) in Python.
//!
//! The module has pyoxigraph's own classes (`NamedNode`, `Literal`, `BlankNode`, `Quad`,
//! `Triple`, `Variable`, `Dataset`, the query results, `parse`, `serialize`...), vendored in
//! [`ox`], so code written against `pyoxigraph.Store` switches by changing the import, and
//! the store builds its terms natively without pyoxigraph installed.

use ox::model::{
    PyBlankNode, PyDefaultGraph, PyGraphName, PyLiteral, PyNamedNode, PyNamedOrBlankNode, PyQuad,
    PyTerm, PyTriple,
};
use pyo3::IntoPyObjectExt;
use pyo3::exceptions::{PyIOError, PyRuntimeError, PySyntaxError, PyTypeError, PyValueError};
use tripletrail::oxigraph::io::{RdfFormat, RdfParser, RdfSerializer};
use tripletrail::oxigraph::model::{
    BaseDirection, BlankNode, GraphName, Literal, NamedNode, NamedOrBlankNode, Quad, Term, Triple,
};
use tripletrail::oxigraph::sparql::{CancellationToken, QueryDataset, Variable};
use tripletrail::oxigraph::store::Store;
use tripletrail::store::QuadIter;
mod ox;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyString, PyTuple};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::Duration;

fn to_py_err(e: tripletrail::Error) -> PyErr {
    let message = e.to_string();
    match e {
        tripletrail::Error::Syntax(_) => PySyntaxError::new_err(message),
        tripletrail::Error::Loader(_) => PySyntaxError::new_err(message),
        tripletrail::Error::Storage(_) => PyIOError::new_err(message),
        _ if message.contains("cancelled") => {
            pyo3::exceptions::PyTimeoutError::new_err("The operation took longer than its timeout")
        }
        _ => PyRuntimeError::new_err(message),
    }
}

/// A mapping error as `ValueError`, a source that cannot be read as `OSError`.
fn rml_err(e: tripletrail_rml::RmlError) -> PyErr {
    match e {
        tripletrail_rml::RmlError::Source { .. } => PyIOError::new_err(e.to_string()),
        e => PyValueError::new_err(e.to_string()),
    }
}

/// A loader that refuses the databases a mapping names itself (`d2rq:Database`, YARRRML
/// `access:`) unless `allowed`, before connecting: the caller's `database` is still read.
#[cfg(feature = "postgres")]
struct OwnDatabases<L> {
    loader: L,
    allowed: bool,
}

#[cfg(feature = "postgres")]
impl<L: tripletrail_rml::Loader> tripletrail_rml::Loader for OwnDatabases<L> {
    fn load(
        &self,
        source: &tripletrail_rml::LogicalSource,
    ) -> Result<Vec<u8>, tripletrail_rml::RmlError> {
        self.loader.load(source)
    }

    fn table(
        &self,
        source: &tripletrail_rml::LogicalSource,
    ) -> Result<tripletrail_rml::Table, tripletrail_rml::RmlError> {
        if !self.allowed && source.database.is_some() {
            return Err(tripletrail_rml::RmlError::Mapping(
                "the mapping names its own database: with allowed, pass databases=True to let \
                 it connect, or give the database as database="
                    .to_owned(),
            ));
        }
        self.loader.table(source)
    }
}

fn io_err(e: impl std::fmt::Display) -> PyErr {
    PyIOError::new_err(e.to_string())
}

/// A term as a Python object of this module's classes.
fn py_term(py: Python<'_>, t: Term) -> PyResult<Bound<'_, PyAny>> {
    PyTerm::from(t).into_bound_py_any(py)
}

fn py_subject(py: Python<'_>, s: NamedOrBlankNode) -> PyResult<Bound<'_, PyAny>> {
    PyNamedOrBlankNode::from(s).into_bound_py_any(py)
}

fn class_name(o: &Bound<'_, PyAny>) -> PyResult<String> {
    Ok(o.get_type().name()?.to_string())
}

fn str_attr(o: &Bound<'_, PyAny>, name: &str) -> PyResult<String> {
    o.getattr(name)?.extract()
}

/// A named node, blank node or literal of this module's classes, checked by type only. The
/// `PyTerm` extraction tries its variants in turn, and pyo3 builds (and formats) an error for
/// each that fails: a few hundred nanoseconds per term, a third of a point lookup.
fn native_term(o: &Bound<'_, PyAny>) -> Option<Term> {
    if let Ok(n) = o.cast::<PyNamedNode>() {
        return Some(NamedNode::from(n.get().clone()).into());
    }
    if let Ok(b) = o.cast::<PyBlankNode>() {
        return Some(b.get().clone().into());
    }
    if let Ok(l) = o.cast::<PyLiteral>() {
        return Some(l.get().clone().into());
    }
    None
}

/// A term of this module (or a string, read as an IRI) as a term.
fn term_from_py(o: &Bound<'_, PyAny>) -> PyResult<Term> {
    if let Some(t) = native_term(o) {
        return Ok(t);
    }
    if let Ok(s) = o.cast::<PyString>() {
        return Ok(NamedNode::new(s.extract::<String>()?)
            .map_err(|e| PyValueError::new_err(e.to_string()))?
            .into());
    }
    // This module's own classes.
    if let Ok(t) = o.extract::<PyTerm>() {
        return Ok(t.into());
    }
    // Others with the same attributes (pyoxigraph's, for instance).
    match class_name(o)?.as_str() {
        "NamedNode" => Ok(NamedNode::new_unchecked(str_attr(o, "value")?).into()),
        "BlankNode" => Ok(BlankNode::new_unchecked(str_attr(o, "value")?).into()),
        "Literal" => {
            let value = str_attr(o, "value")?;
            let language: Option<String> = o.getattr("language")?.extract()?;
            let direction = match o.getattr("direction") {
                Ok(d) if !d.is_none() => Some(match d.str()?.to_string().to_lowercase() {
                    d if d.ends_with("rtl") => BaseDirection::Rtl,
                    _ => BaseDirection::Ltr,
                }),
                _ => None,
            };
            Ok(match (language, direction) {
                (Some(lang), Some(direction)) => {
                    Literal::new_directional_language_tagged_literal_unchecked(
                        value, lang, direction,
                    )
                }
                (Some(lang), None) => Literal::new_language_tagged_literal_unchecked(value, lang),
                (None, _) => Literal::new_typed_literal(
                    value,
                    NamedNode::new_unchecked(str_attr(&o.getattr("datatype")?, "value")?),
                ),
            }
            .into())
        }
        "Triple" => Ok(Triple::new(
            subject_from_py(&o.getattr("subject")?)?,
            predicate_from_py(&o.getattr("predicate")?)?,
            term_from_py(&o.getattr("object")?)?,
        )
        .into()),
        other => Err(PyTypeError::new_err(format!(
            "Expected an RDF term, got {other}"
        ))),
    }
}

fn graph_from_py(o: &Bound<'_, PyAny>) -> PyResult<GraphName> {
    match native_term(o) {
        Some(Term::NamedNode(n)) => return Ok(n.into()),
        Some(Term::BlankNode(b)) => return Ok(b.into()),
        _ => {}
    }
    if o.cast::<PyDefaultGraph>().is_ok() {
        return Ok(GraphName::DefaultGraph);
    }
    if let Ok(g) = o.extract::<PyGraphName>() {
        return Ok(g.into());
    }
    if class_name(o)? == "DefaultGraph" {
        return Ok(GraphName::DefaultGraph);
    }
    match term_from_py(o)? {
        Term::NamedNode(n) => Ok(n.into()),
        Term::BlankNode(b) => Ok(b.into()),
        _ => Err(PyTypeError::new_err(
            "A graph name is an IRI, a blank node or DefaultGraph",
        )),
    }
}

fn subject_from_py(o: &Bound<'_, PyAny>) -> PyResult<NamedOrBlankNode> {
    match term_from_py(o)? {
        Term::NamedNode(n) => Ok(n.into()),
        Term::BlankNode(b) => Ok(b.into()),
        _ => Err(PyTypeError::new_err("A subject is an IRI or a blank node")),
    }
}

fn predicate_from_py(o: &Bound<'_, PyAny>) -> PyResult<NamedNode> {
    match term_from_py(o)? {
        Term::NamedNode(n) => Ok(n),
        _ => Err(PyTypeError::new_err("A predicate is an IRI")),
    }
}

fn quad_from_py(o: &Bound<'_, PyAny>) -> PyResult<Quad> {
    if let Ok(q) = o.cast::<PyQuad>() {
        return Ok(q.get().clone().into());
    }
    let graph = o.getattr("graph_name").ok();
    Ok(Quad::new(
        subject_from_py(&o.getattr("subject")?)?,
        predicate_from_py(&o.getattr("predicate")?)?,
        term_from_py(&o.getattr("object")?)?,
        match graph {
            Some(g) => graph_from_py(&g)?,
            None => GraphName::DefaultGraph,
        },
    ))
}

/// A format given as a format object (with `media_type`), a media type, or a file
/// extension.
fn format_name(o: &Bound<'_, PyAny>) -> PyResult<String> {
    if let Ok(s) = o.extract::<String>() {
        return Ok(s);
    }
    str_attr(o, "media_type")
}

fn rdf_format(o: Option<&Bound<'_, PyAny>>, path: Option<&PathBuf>) -> PyResult<RdfFormat> {
    if let Some(o) = o {
        let name = format_name(o)?;
        return RdfFormat::from_media_type(&name)
            .or_else(|| RdfFormat::from_extension(&name))
            .ok_or_else(|| PyValueError::new_err(format!("Unsupported RDF format {name}")));
    }
    let extension = path
        .and_then(|p| p.extension())
        .and_then(|e| e.to_str())
        .ok_or_else(|| PyValueError::new_err("The format is needed when there is no file path"))?;
    RdfFormat::from_extension(extension)
        .ok_or_else(|| PyValueError::new_err(format!("Unknown RDF file extension {extension}")))
}

/// Writes `write`'s output to `output` (a path or a binary file object), or returns it as
/// bytes if `output` is None.
fn write_output<'py>(
    py: Python<'py>,
    output: Option<&Bound<'py, PyAny>>,
    write: impl FnOnce(&mut Vec<u8>) -> std::io::Result<()>,
) -> PyResult<Option<Bound<'py, PyBytes>>> {
    let mut buffer = Vec::new();
    write(&mut buffer).map_err(io_err)?;
    match output {
        None => Ok(Some(PyBytes::new(py, &buffer))),
        Some(o) => {
            if let Ok(path) = o.extract::<PathBuf>() {
                std::fs::File::create(path)
                    .and_then(|mut f| f.write_all(&buffer))
                    .map_err(io_err)?;
            } else {
                o.call_method1("write", (PyBytes::new(py, &buffer),))?;
            }
            Ok(None)
        }
    }
}

/// `PREFIX` and `BASE` declarations to put before a query or an update.
fn prologue(base_iri: Option<&str>, prefixes: Option<&Bound<'_, PyDict>>) -> PyResult<String> {
    let mut out = String::new();
    if let Some(base) = base_iri {
        out.push_str(&format!("BASE <{base}>\n"));
    }
    if let Some(prefixes) = prefixes {
        for (name, iri) in prefixes.iter() {
            let iri = match iri.extract::<String>() {
                Ok(s) => s,
                Err(_) => str_attr(&iri, "value")?,
            };
            out.push_str(&format!("PREFIX {}: <{iri}>\n", name.extract::<String>()?));
        }
    }
    Ok(out)
}

/// One graph name or a list of them.
fn graph_names(o: &Bound<'_, PyAny>) -> PyResult<Vec<GraphName>> {
    if o.cast::<PyString>().is_err()
        && let Ok(items) = o.try_iter()
    {
        return items.map(|g| graph_from_py(&g?)).collect();
    }
    Ok(vec![graph_from_py(o)?])
}

fn named_graph_from_py(o: &Bound<'_, PyAny>) -> PyResult<NamedOrBlankNode> {
    match graph_from_py(o)? {
        GraphName::NamedNode(n) => Ok(n.into()),
        GraphName::BlankNode(b) => Ok(b.into()),
        GraphName::DefaultGraph => Err(PyValueError::new_err("Expected a named graph")),
    }
}

/// Cancels a token after a delay unless dropped first.
struct Deadline(#[allow(dead_code)] Option<mpsc::Sender<()>>);

fn deadline(engine: &tripletrail::Engine, timeout: Option<f64>) -> (tripletrail::Engine, Deadline) {
    let Some(timeout) = timeout else {
        return (engine.clone(), Deadline(None));
    };
    let token = CancellationToken::new();
    let cancel = token.clone();
    let (tx, rx) = mpsc::channel::<()>();
    let timeout = Duration::from_secs_f64(timeout.max(0.0));
    std::thread::spawn(move || {
        if rx.recv_timeout(timeout) == Err(mpsc::RecvTimeoutError::Timeout) {
            cancel.cancel();
        }
    });
    (engine.with_cancellation_token(token), Deadline(Some(tx)))
}

/// An tripletrail store: GeoSPARQL 1.1, PostGIS-style `st:` functions, Jena's `spatialf:`
/// functions, the spatial index and the pgRouting SERVICE.
///
/// `Engine()` is in memory; `Engine(path)` opens (or creates) an on-disk store.
#[pyclass(name = "Engine", module = "tripletrail", frozen)]
struct PyEngine {
    inner: tripletrail::Engine,
}

#[pymethods]
impl PyEngine {
    /// `store` is `"tripletrail"` (the tripletrail store, in memory or in the directory `path`; the
    /// default) or `"oxigraph"` (Oxigraph's store; on disk needs the rocksdb build).
    #[new]
    #[pyo3(signature = (path = None, store = "tripletrail"))]
    fn new(path: Option<PathBuf>, store: &str) -> PyResult<Self> {
        match store {
            "oxigraph" => {}
            "tripletrail" => {
                let inner = match path {
                    None => tripletrail::Engine::new_tripletrail(),
                    Some(p) => tripletrail::Engine::open_tripletrail(p).map_err(io_err)?,
                };
                return Ok(Self { inner });
            }
            other => {
                return Err(PyValueError::new_err(format!(
                    "store must be \"oxigraph\" or \"tripletrail\", not {other:?}"
                )));
            }
        }
        let store = match path {
            None => Store::new(),
            Some(p) => open(&p),
        }
        .map_err(io_err)?;
        Ok(Self {
            inner: tripletrail::Engine::from_store(store),
        })
    }

    /// Evaluates a SPARQL query: `QuerySolutions` for SELECT, `QueryBoolean` for ASK,
    /// `QueryTriples` for CONSTRUCT and DESCRIBE. With `timeout` (seconds), evaluation stops
    /// with `TimeoutError` once it is exceeded.
    ///
    /// As in pyoxigraph: `base_iri` and `prefixes` (a dict of prefix name to IRI) are
    /// declared before the query; `use_default_graph_as_union` makes the default graph the
    /// union of the named graphs, `default_graph` (a graph name or a list of them) and
    /// `named_graphs` (a list) choose the dataset instead of `FROM` and `FROM NAMED`;
    /// `substitutions` (a dict of `Variable` to term) binds variables before evaluation.
    /// `query` may also be a `PreparedQuery` from `prepare`, parsed once (its `base_iri` and
    /// `prefixes` were given to `prepare`).
    #[pyo3(signature = (
        query, *, base_iri = None, prefixes = None, use_default_graph_as_union = false,
        default_graph = None, named_graphs = None, substitutions = None, timeout = None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn query(
        &self,
        py: Python<'_>,
        query: &Bound<'_, PyAny>,
        base_iri: Option<&str>,
        prefixes: Option<&Bound<'_, PyDict>>,
        use_default_graph_as_union: bool,
        default_graph: Option<&Bound<'_, PyAny>>,
        named_graphs: Option<&Bound<'_, PyAny>>,
        substitutions: Option<&Bound<'_, PyDict>>,
        timeout: Option<f64>,
    ) -> PyResult<Py<PyAny>> {
        let mut options = tripletrail::QueryOptions::default();
        if use_default_graph_as_union || default_graph.is_some() || named_graphs.is_some() {
            let mut dataset = QueryDataset::new();
            if use_default_graph_as_union {
                if default_graph.is_some() {
                    return Err(PyValueError::new_err(
                        "default_graph and use_default_graph_as_union can't be given together",
                    ));
                }
                dataset.set_default_graph_as_union();
            }
            if let Some(g) = default_graph {
                dataset.set_default_graph(graph_names(g)?);
            }
            if let Some(n) = named_graphs {
                dataset.set_available_named_graphs(
                    n.try_iter()?
                        .map(|g| named_graph_from_py(&g?))
                        .collect::<PyResult<_>>()?,
                );
            }
            options.dataset = Some(dataset);
        }
        if let Some(substitutions) = substitutions {
            for (variable, value) in substitutions.iter() {
                let name = match variable.extract::<String>() {
                    Ok(s) => s,
                    Err(_) => str_attr(&variable, "value")?,
                };
                let variable = Variable::new(name.trim_start_matches(['?', '$']))
                    .map_err(|e| PyValueError::new_err(e.to_string()))?;
                options
                    .substitutions
                    .push((variable, term_from_py(&value)?));
            }
        }
        let (engine, deadline) = deadline(&self.inner, timeout);
        let results = if let Ok(prepared) = query.cast::<PyPreparedQuery>() {
            if base_iri.is_some() || prefixes.is_some() {
                return Err(PyValueError::new_err(
                    "give base_iri and prefixes to prepare, not to query, for a prepared query",
                ));
            }
            engine.query_prepared(&prepared.get().inner, options)
        } else {
            let query: String = query.extract()?;
            let query = prologue(base_iri, prefixes)? + &query;
            engine.query_with_options(&query, options)
        }
        .map_err(to_py_err)?;
        let keep_alive: ox::sparql::KeepAlive = deadline
            .0
            .is_some()
            .then(|| Arc::new(deadline) as Arc<dyn std::any::Any + Send + Sync>);
        Ok(ox::sparql::query_results_to_python(py, results, keep_alive)?.unbind())
    }

    /// Parses a SPARQL query once, to run it many times with `query(prepared,
    /// substitutions=...)`: the way to pass values into a query without writing them into
    /// its text. `variables` lists the variables it selects, which `substitutions` may bind.
    #[pyo3(signature = (query, *, base_iri = None, prefixes = None))]
    fn prepare(
        &self,
        query: &str,
        base_iri: Option<&str>,
        prefixes: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<PyPreparedQuery> {
        let query = prologue(base_iri, prefixes)? + query;
        let inner = self.inner.prepare_query(&query, None).map_err(to_py_err)?;
        Ok(PyPreparedQuery { inner })
    }

    /// Runs a SPARQL update. The GIL is released meanwhile.
    /// `base_iri` and `prefixes` are declared before the update, as in pyoxigraph.
    #[pyo3(signature = (update, *, base_iri = None, prefixes = None, timeout = None))]
    fn update(
        &self,
        py: Python<'_>,
        update: &str,
        base_iri: Option<&str>,
        prefixes: Option<&Bound<'_, PyDict>>,
        timeout: Option<f64>,
    ) -> PyResult<()> {
        let (engine, _deadline) = deadline(&self.inner, timeout);
        let update = prologue(base_iri, prefixes)? + update;
        py.detach(move || engine.update(&update)).map_err(to_py_err)
    }

    /// Evaluates a query to the end and returns its plan with row counts and timings, and
    /// what the spatial rewrite did, as a JSON string.
    fn explain(&self, py: Python<'_>, query: &str) -> PyResult<String> {
        let query = query.to_owned();
        let engine = self.inner.clone();
        py.detach(move || engine.explain(&query)).map_err(to_py_err)
    }

    /// Bulk-loads RDF from `input` (bytes, str or a binary file object) or from `path`.
    /// `format` is a `RdfFormat`, a media type or a file extension (from `path`
    /// if omitted). `to_graph` puts the triples in that named graph. Blank node labels are
    /// kept, as pyoxigraph's `Store.load` keeps them, so a dump can be loaded in parts;
    /// `rename_blank_nodes=True` gives the document its own blank nodes, as `tripletrail load`
    /// and Jena do.
    #[pyo3(signature = (input = None, format = None, *, path = None, base_iri = None, to_graph = None, rename_blank_nodes = false))]
    #[allow(clippy::too_many_arguments)]
    fn load(
        &self,
        py: Python<'_>,
        input: Option<&Bound<'_, PyAny>>,
        format: Option<&Bound<'_, PyAny>>,
        path: Option<PathBuf>,
        base_iri: Option<&str>,
        to_graph: Option<&Bound<'_, PyAny>>,
        rename_blank_nodes: bool,
    ) -> PyResult<()> {
        let blank_nodes = if rename_blank_nodes {
            tripletrail::BlankNodes::PerLoad
        } else {
            tripletrail::BlankNodes::Keep
        };
        let mut parser = RdfParser::from_format(rdf_format(format, path.as_ref())?);
        if let Some(base) = base_iri {
            parser = parser
                .with_base_iri(base)
                .map_err(|e| PyValueError::new_err(e.to_string()))?;
        }
        if let Some(g) = to_graph {
            parser = parser.with_default_graph(graph_from_py(g)?);
        }
        let engine = self.inner.clone();
        match (input, path) {
            (None, Some(path)) => {
                let file = std::fs::File::open(&path).map_err(io_err)?;
                py.detach(move || {
                    engine.load_with_blank_nodes(
                        parser,
                        std::io::BufReader::with_capacity(1 << 20, file),
                        blank_nodes,
                    )
                })
                .map_err(to_py_err)
            }
            (Some(input), None) => {
                let data: Vec<u8> = if let Ok(b) = input.cast::<PyBytes>() {
                    b.as_bytes().to_vec()
                } else if let Ok(s) = input.cast::<PyString>() {
                    s.extract::<String>()?.into_bytes()
                } else {
                    let read = input.call_method0("read")?;
                    if let Ok(b) = read.cast::<PyBytes>() {
                        b.as_bytes().to_vec()
                    } else {
                        read.extract::<String>()?.into_bytes()
                    }
                };
                py.detach(move || {
                    engine.load_with_blank_nodes(parser, data.as_slice(), blank_nodes)
                })
                .map_err(to_py_err)
            }
            _ => Err(PyValueError::new_err("Give either input or path")),
        }
    }

    /// The quads matching a pattern (`None` matches anything).
    #[pyo3(signature = (subject, predicate, object, graph_name = None))]
    fn quads_for_pattern<'py>(
        &self,
        py: Python<'py>,
        subject: Option<&Bound<'py, PyAny>>,
        predicate: Option<&Bound<'py, PyAny>>,
        object: Option<&Bound<'py, PyAny>>,
        graph_name: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let s = subject.map(subject_from_py).transpose()?;
        let p = predicate.map(predicate_from_py).transpose()?;
        let o = object.map(term_from_py).transpose()?;
        let g = graph_name.map(graph_from_py).transpose()?;
        quads(
            py,
            self.inner.store().quads_for_pattern(
                s.as_ref().map(Into::into),
                p.as_ref().map(Into::into),
                o.as_ref().map(Into::into),
                g.as_ref().map(Into::into),
            ),
        )
    }

    /// The objects of the quads with this subject and predicate (in graph `graph_name`, or in
    /// any graph if None), as a list of terms: one per matching quad, without
    /// building the quads. For rdflib-style `objects` / `value` lookups.
    #[pyo3(signature = (subject, predicate, graph_name = None))]
    fn objects<'py>(
        &self,
        py: Python<'py>,
        subject: &Bound<'py, PyAny>,
        predicate: &Bound<'py, PyAny>,
        graph_name: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Vec<Bound<'py, PyAny>>> {
        let s = subject_from_py(subject)?;
        let p = predicate_from_py(predicate)?;
        let g = graph_name.map(graph_from_py).transpose()?;
        self.inner
            .store()
            .quads_for_pattern(
                Some(s.as_ref()),
                Some(p.as_ref()),
                None,
                g.as_ref().map(Into::into),
            )
            .map(|q| py_term(py, q.map_err(io_err)?.object))
            .collect()
    }

    /// The subjects of the quads with this predicate and object (in graph `graph_name`, or in
    /// any graph if None), as a list of terms, one per matching quad.
    #[pyo3(signature = (predicate, object, graph_name = None))]
    fn subjects<'py>(
        &self,
        py: Python<'py>,
        predicate: &Bound<'py, PyAny>,
        object: &Bound<'py, PyAny>,
        graph_name: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Vec<Bound<'py, PyAny>>> {
        let p = predicate_from_py(predicate)?;
        let o = term_from_py(object)?;
        let g = graph_name.map(graph_from_py).transpose()?;
        self.inner
            .store()
            .quads_for_pattern(
                None,
                Some(p.as_ref()),
                Some(o.as_ref()),
                g.as_ref().map(Into::into),
            )
            .map(|q| py_subject(py, q.map_err(io_err)?.subject))
            .collect()
    }

    /// The `(predicate, object)` pairs of the quads with this subject (in graph `graph_name`,
    /// or in any graph if None), as a list of tuples of terms, one per quad.
    #[pyo3(signature = (subject, graph_name = None))]
    fn predicate_objects<'py>(
        &self,
        py: Python<'py>,
        subject: &Bound<'py, PyAny>,
        graph_name: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Vec<Bound<'py, PyTuple>>> {
        let s = subject_from_py(subject)?;
        let g = graph_name.map(graph_from_py).transpose()?;
        self.inner
            .store()
            .quads_for_pattern(Some(s.as_ref()), None, None, g.as_ref().map(Into::into))
            .map(|q| {
                let q = q.map_err(io_err)?;
                PyTuple::new(
                    py,
                    [
                        PyNamedNode::from(q.predicate).into_bound_py_any(py)?,
                        py_term(py, q.object)?,
                    ],
                )
            })
            .collect()
    }

    /// Adds a quad.
    fn add(&self, quad: &Bound<'_, PyAny>) -> PyResult<()> {
        self.inner.insert(&quad_from_py(quad)?).map_err(to_py_err)
    }

    /// Adds quads, in bulk, with the GIL released while they are written.
    fn extend(&self, py: Python<'_>, quads: &Bound<'_, PyAny>) -> PyResult<()> {
        let quads = quads
            .try_iter()?
            .map(|q| quad_from_py(&q?))
            .collect::<PyResult<Vec<_>>>()?;
        let engine = self.inner.clone();
        py.detach(move || engine.load_quads(quads))
            .map_err(to_py_err)
    }

    /// Removes a quad.
    fn remove(&self, quad: &Bound<'_, PyAny>) -> PyResult<()> {
        self.inner.remove(&quad_from_py(quad)?).map_err(to_py_err)
    }

    /// The same as `load`, under pyoxigraph's bulk-loading name.
    #[pyo3(signature = (input = None, format = None, *, path = None, base_iri = None, to_graph = None, rename_blank_nodes = false))]
    #[allow(clippy::too_many_arguments)]
    fn bulk_load(
        &self,
        py: Python<'_>,
        input: Option<&Bound<'_, PyAny>>,
        format: Option<&Bound<'_, PyAny>>,
        path: Option<PathBuf>,
        base_iri: Option<&str>,
        to_graph: Option<&Bound<'_, PyAny>>,
        rename_blank_nodes: bool,
    ) -> PyResult<()> {
        self.load(
            py,
            input,
            format,
            path,
            base_iri,
            to_graph,
            rename_blank_nodes,
        )
    }

    /// Runs an RML, R2RML or YARRRML mapping over files and adds the quads it makes to the
    /// store, in batches as they are made; returns how many were made (a quad made twice
    /// counts twice, and is stored once). The mapping is `mapping` (Turtle text, or YARRRML
    /// with `yarrrml=True`; its relative IRIs resolve against `base_dir`) or the file `path` (YARRRML by its `.yarrrml`, `.yml` or `.yaml`
    /// name). Relative source paths resolve against `base_dir`, by default the current
    /// directory, as `tripletrail lift` does; with `allowed` (a list of directories) only files
    /// inside them are read. Quads in the default graph go to `to_graph` when given; a
    /// mapping's own graph maps are kept. PostgreSQL tables and queries are read as sources
    /// (#21): from `database` (a connection string,
    /// `postgresql://user:password@host:port/name`), as `tripletrail lift --database` does, or
    /// from the `d2rq:Database` a mapping names itself. With `allowed`, a mapping may not name
    /// its own database (it would connect to any host it names) unless `databases` is true.
    /// Logical targets are refused: use `tripletrail lift` for them.
    #[pyo3(signature = (mapping = None, *, path = None, yarrrml = false, base_dir = None, allowed = None, to_graph = None, database = None, databases = None))]
    #[allow(clippy::too_many_arguments)]
    fn run_mapping(
        &self,
        py: Python<'_>,
        mapping: Option<String>,
        path: Option<PathBuf>,
        yarrrml: bool,
        base_dir: Option<PathBuf>,
        allowed: Option<Vec<PathBuf>>,
        to_graph: Option<&Bound<'_, PyAny>>,
        database: Option<String>,
        databases: Option<bool>,
    ) -> PyResult<usize> {
        // A mapping run inside `allowed` reaches no host it names itself, unless asked for.
        let own_databases = databases.unwrap_or(allowed.is_none());
        let to_graph = to_graph.map(graph_from_py).transpose()?;
        let base = match base_dir {
            Some(b) => b,
            None => std::env::current_dir().map_err(io_err)?,
        };
        let (text, base_iri, yarrrml) = match (mapping, path) {
            // Relative IRIs of a mapping given as text (`<#Map>`) resolve against `base`.
            (Some(text), None) => {
                let dir = std::path::absolute(&base).map_err(io_err)?;
                (text, Some(format!("file://{}/", dir.display())), yarrrml)
            }
            (None, Some(path)) => {
                let text = std::fs::read_to_string(&path).map_err(io_err)?;
                let name = path.to_string_lossy().to_ascii_lowercase();
                let yaml = yarrrml
                    || [".yarrrml", ".yml", ".yaml"]
                        .iter()
                        .any(|e| name.ends_with(e));
                let absolute = std::path::absolute(&path).map_err(io_err)?;
                (text, Some(format!("file://{}", absolute.display())), yaml)
            }
            _ => {
                return Err(PyValueError::new_err(
                    "give the mapping as text or as path, not both",
                ));
            }
        };
        let engine = self.inner.clone();
        py.detach(move || -> PyResult<usize> {
            let parsed = if yarrrml {
                tripletrail_rml::Mapping::parse_yarrrml(&text, base_iri.as_deref())
            } else {
                tripletrail_rml::Mapping::parse_turtle(&text, base_iri.as_deref())
            }
            .map_err(rml_err)?;
            if !parsed.targets.is_empty() {
                return Err(PyValueError::new_err(
                    "the mapping has logical targets: run it with `tripletrail lift`",
                ));
            }
            let files = tripletrail_rml::FsLoader {
                config: tripletrail::fx::FxConfig {
                    base,
                    allowed,
                    ..Default::default()
                },
            };
            #[cfg(feature = "postgres")]
            let loader = OwnDatabases {
                loader: tripletrail_rml::DatabaseLoader::new(files, database),
                allowed: own_databases,
            };
            #[cfg(not(feature = "postgres"))]
            let loader = {
                let _ = own_databases;
                if database.is_some() {
                    return Err(PyValueError::new_err(
                        "this build reads no databases: build it with the postgres feature",
                    ));
                }
                files
            };
            const BATCH: usize = 1 << 20;
            let mut batch = Vec::with_capacity(BATCH);
            let mut failed = None;
            let made = tripletrail_rml::run_with(&parsed, &loader, &mut |mut q| {
                if failed.is_some() {
                    return;
                }
                if let (Some(g), true) = (&to_graph, q.graph_name.is_default_graph()) {
                    q.graph_name = g.clone();
                }
                batch.push(q);
                if batch.len() == BATCH
                    && let Err(e) = engine.load_quads(std::mem::take(&mut batch))
                {
                    failed = Some(e);
                }
            })
            .map_err(rml_err)?;
            if let Some(e) = failed {
                return Err(to_py_err(e));
            }
            engine.load_quads(batch).map_err(to_py_err)?;
            Ok(made)
        })
    }

    /// The same as `extend`, under pyoxigraph's bulk-loading name.
    fn bulk_extend(&self, py: Python<'_>, quads: &Bound<'_, PyAny>) -> PyResult<()> {
        self.extend(py, quads)
    }

    /// The store in RDFC-1.0 canonical N-Quads (blank nodes `_:c14n0`, `_:c14n1`, …, lines
    /// sorted), as bytes: the same for isomorphic datasets.
    fn canonicalize<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let canonical = canonical(&self.inner)?;
        Ok(PyBytes::new(py, canonical.nquads().as_bytes()))
    }

    /// A new engine on the same store that can also call the user-defined functions
    /// (SHACL-AF `sh:SPARQLFunction`s) of `input` (bytes or str) or of the file `path`, by
    /// their IRIs, in queries, updates, rules and constraints. `format` as for `load`.
    #[pyo3(signature = (input = None, format = None, *, path = None))]
    fn with_functions(
        &self,
        input: Option<&Bound<'_, PyAny>>,
        format: Option<&Bound<'_, PyAny>>,
        path: Option<PathBuf>,
    ) -> PyResult<Self> {
        let format = rdf_format(format, path.as_ref())?;
        let data: Vec<u8> = match (input, &path) {
            (None, Some(path)) => std::fs::read(path).map_err(io_err)?,
            (Some(input), None) => {
                if let Ok(b) = input.cast::<PyBytes>() {
                    b.as_bytes().to_vec()
                } else {
                    input.extract::<String>()?.into_bytes()
                }
            }
            _ => return Err(PyValueError::new_err("give either input or path")),
        };
        let mut triples = Vec::new();
        for quad in RdfParser::from_format(format).for_slice(&data) {
            let q = quad.map_err(|e| PyValueError::new_err(e.to_string()))?;
            triples.push(Triple::new(q.subject, q.predicate, q.object));
        }
        let inner = self
            .inner
            .with_functions_from(triples)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        Ok(Self { inner })
    }

    /// The SHA-256 (lowercase hex) of the store's RDFC-1.0 canonical N-Quads.
    fn canonical_digest(&self) -> PyResult<String> {
        Ok(canonical(&self.inner)?.digest())
    }

    /// Writes the store (`from_graph`: only that graph, as triples) to `output` (a path or a
    /// binary file object), or returns it as bytes. `format` defaults to N-Quads, or
    /// N-Triples with `from_graph`. A format of triples (Turtle, N-Triples, RDF/XML) without
    /// `from_graph` writes the default graph when the store has no named graph, and is an
    /// error otherwise.
    #[pyo3(signature = (output = None, format = None, *, from_graph = None, base_iri = None, prefixes = None))]
    fn dump<'py>(
        &self,
        py: Python<'py>,
        output: Option<&Bound<'py, PyAny>>,
        format: Option<&Bound<'py, PyAny>>,
        from_graph: Option<&Bound<'py, PyAny>>,
        base_iri: Option<&str>,
        prefixes: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Option<Bound<'py, PyBytes>>> {
        let mut graph = from_graph.map(graph_from_py).transpose()?;
        let format = match format {
            Some(f) => rdf_format(Some(f), None)?,
            None if graph.is_some() => RdfFormat::NTriples,
            None => RdfFormat::NQuads,
        };
        if graph.is_none() && !format.supports_datasets() {
            // A dataset with only a default graph is written as its triples (#381); with named
            // graphs, which of them is for the caller to say.
            let named = self.inner.store().named_graphs().map_err(io_err)?;
            if !named.is_empty() {
                let mut shown: Vec<String> =
                    named.iter().take(3).map(ToString::to_string).collect();
                if named.len() > 3 {
                    shown.push(format!("and {} more", named.len() - 3));
                }
                return Err(PyValueError::new_err(format!(
                    "{} only holds triples and the store has named graphs ({}): give from_graph \
                     (DefaultGraph() for the default graph, or a graph name)",
                    format.name(),
                    shown.join(", ")
                )));
            }
            graph = Some(GraphName::DefaultGraph);
        }
        let mut serializer = RdfSerializer::from_format(format);
        if let Some(base) = base_iri {
            serializer = serializer
                .with_base_iri(base)
                .map_err(|e| PyValueError::new_err(e.to_string()))?;
        }
        if let Some(prefixes) = prefixes {
            for (name, iri) in prefixes.iter() {
                let iri = match iri.extract::<String>() {
                    Ok(s) => s,
                    Err(_) => str_attr(&iri, "value")?,
                };
                serializer = serializer
                    .with_prefix(name.extract::<String>()?, iri)
                    .map_err(|e| PyValueError::new_err(e.to_string()))?;
            }
        }
        let store = self.inner.store();
        write_output(py, output, |w| {
            let mut writer = serializer.for_writer(w);
            for q in store.quads_for_pattern(None, None, None, graph.as_ref().map(Into::into)) {
                let q = q.map_err(std::io::Error::other)?;
                if graph.is_some() {
                    writer.serialize_triple(tripletrail::oxigraph::model::QuadRef::from(&q))?;
                } else {
                    writer.serialize_quad(&q)?;
                }
            }
            writer.finish().map(|_| ())
        })
    }

    /// The named graphs (`NamedNode`s and `BlankNode`s).
    fn named_graphs<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyAny>>> {
        self.inner
            .store()
            .named_graphs()
            .map_err(io_err)?
            .into_iter()
            .map(|g| py_subject(py, g))
            .collect()
    }

    /// Whether the store has this named graph.
    fn contains_named_graph(&self, graph_name: &Bound<'_, PyAny>) -> PyResult<bool> {
        self.inner
            .store()
            .contains_named_graph(&named_graph_from_py(graph_name)?)
            .map_err(io_err)
    }

    /// Declares a named graph. With the tripletrail store a graph exists while it has quads, so
    /// an empty graph is not listed by `named_graphs`.
    fn add_graph(&self, graph_name: &Bound<'_, PyAny>) -> PyResult<()> {
        let graph = match graph_from_py(graph_name)? {
            GraphName::DefaultGraph => return Ok(()),
            GraphName::NamedNode(n) => NamedOrBlankNode::from(n),
            GraphName::BlankNode(b) => b.into(),
        };
        self.inner
            .store()
            .insert_named_graph(&graph)
            .map_err(io_err)
    }

    /// Removes the quads of a graph (the default graph too); the graph stays declared.
    fn clear_graph(&self, py: Python<'_>, graph_name: &Bound<'_, PyAny>) -> PyResult<()> {
        match graph_from_py(graph_name)? {
            GraphName::DefaultGraph => self.run_update(py, "CLEAR SILENT DEFAULT".into()),
            GraphName::NamedNode(n) => self.run_update(py, format!("CLEAR SILENT GRAPH {n}")),
            g @ GraphName::BlankNode(_) => self.remove_graph_quads(&g),
        }
    }

    /// Removes a named graph and its quads.
    fn remove_graph(&self, py: Python<'_>, graph_name: &Bound<'_, PyAny>) -> PyResult<()> {
        match named_graph_from_py(graph_name)? {
            NamedOrBlankNode::NamedNode(n) => self.run_update(py, format!("DROP SILENT GRAPH {n}")),
            NamedOrBlankNode::BlankNode(b) => {
                self.remove_graph_quads(&GraphName::BlankNode(b.clone()))?;
                self.inner
                    .store()
                    .remove_named_graph(&NamedOrBlankNode::BlankNode(b))
                    .map_err(io_err)
            }
        }
    }

    /// Removes every quad and named graph.
    fn clear(&self, py: Python<'_>) -> PyResult<()> {
        self.run_update(py, "DROP SILENT ALL".into())
    }

    /// Validates a graph of the store (`data_graph`, the default graph if omitted) against
    /// the SHACL shapes of the named graph `shapes_graph`, and returns a `ValidationReport`.
    /// With `report_graph`, the W3C validation report is also written to that named graph of
    /// the store. The GIL is released meanwhile.
    #[pyo3(signature = (shapes_graph, *, data_graph = None, report_graph = None, statistics = false))]
    fn validate(
        &self,
        py: Python<'_>,
        shapes_graph: &Bound<'_, PyAny>,
        data_graph: Option<&Bound<'_, PyAny>>,
        report_graph: Option<&Bound<'_, PyAny>>,
        statistics: bool,
    ) -> PyResult<PyValidationReport> {
        let shapes = match graph_from_py(shapes_graph)? {
            GraphName::NamedNode(n) => n,
            _ => return Err(PyValueError::new_err("shapes_graph must be a NamedNode")),
        };
        let data = data_graph
            .map(graph_from_py)
            .transpose()?
            .unwrap_or_default();
        let into = report_graph.map(graph_from_py).transpose()?;
        let engine = self.inner.clone();
        py.detach(move || {
            let mut rows = Vec::new();
            let mut triples = Vec::new();
            let mut writer = tripletrail::shacl::ReportWriter::new();
            if statistics {
                writer = writer.with_cells();
            }
            let counts = engine.validate_counted(&shapes, data.as_ref(), statistics, &mut |r| {
                rows.push(ResultRow {
                    focus_node: r.focus_node.clone(),
                    result_path: r.result_path.as_ref().map(|p| p.to_sparql()),
                    value: r.value.clone(),
                    source_shape: r.source_shape.clone(),
                    shape_iri: r.source_shape_iri.as_str().to_owned(),
                    source_constraint_component: r.source_constraint_component.clone(),
                    severity: r.severity.clone(),
                    messages: r.messages.to_vec(),
                    key: format!("{:032x}", r.key()),
                });
                writer.result(&r, &mut |t| triples.push(t));
            })?;
            let mut summary = None;
            if statistics {
                writer.statistics(&counts, &mut |t| triples.push(t));
                summary = writer.take_summary();
                if let Some(s) = &summary {
                    writer.summary(s, &mut |t| triples.push(t));
                }
            }
            let conforms = writer.finish(&mut |t| triples.push(t));
            if let Some(g) = into {
                engine.load_quads(triples.iter().map(|t| t.clone().in_graph(g.clone())))?;
            }
            Ok(PyValidationReport {
                conforms,
                rows,
                triples,
                counts,
                summary,
            })
        })
        .map_err(to_py_err)
    }

    /// Writes the results of a SPARQL SELECT as a GeoParquet file, to `output` (a path or a
    /// binary file object), or returns its bytes. Geometry literal columns become WKB geometry
    /// columns with their CRS; the first one gets a `bbox` covering column. `projjson` is the
    /// PROJJSON to write for a CRS other than CRS84 and EPSG:4326. The GIL is released meanwhile.
    #[cfg(feature = "geoparquet")]
    #[pyo3(signature = (query, output = None, *, row_group_size = 65536, projjson = None))]
    fn export_geoparquet<'py>(
        &self,
        py: Python<'py>,
        query: &str,
        output: Option<&Bound<'py, PyAny>>,
        row_group_size: usize,
        projjson: Option<String>,
    ) -> PyResult<Option<Bound<'py, PyBytes>>> {
        let engine = self.inner.clone();
        let query = query.to_owned();
        let options = tripletrail::geoparquet::ExportOptions {
            row_group_size,
            projjson,
            ..Default::default()
        };
        let file = py
            .detach(move || {
                let mut file = Vec::new();
                engine
                    .export_geoparquet(&query, &mut file, &options)
                    .map(|_| file)
            })
            .map_err(to_py_err)?;
        write_output(py, output, |buffer| {
            buffer.extend(file);
            Ok(())
        })
    }

    /// Loads a GeoParquet file (a path, bytes, or a binary file object) as triples: each row
    /// gets a subject from the `subject` IRI template (`{column}` placeholders, percent-encoded,
    /// or `{+column}` as is; a blank node without one), a `predicate_prefix + column` triple per
    /// value, and each geometry column as `geo:hasGeometry` / `geo:asWKT`. `bbox` (min x, min y,
    /// max x, max y) keeps only the rows whose geometry intersects it. Returns a dict of
    /// counts: rows, triples, skipped_rows, invalid_geometries.
    #[cfg(feature = "geoparquet")]
    #[pyo3(signature = (input, *, subject = None, predicate_prefix = None, class_ = None, to_graph = None, bbox = None))]
    #[allow(clippy::too_many_arguments)]
    fn load_geoparquet<'py>(
        &self,
        py: Python<'py>,
        input: &Bound<'py, PyAny>,
        subject: Option<String>,
        predicate_prefix: Option<String>,
        class_: Option<&Bound<'py, PyAny>>,
        to_graph: Option<&Bound<'py, PyAny>>,
        bbox: Option<(f64, f64, f64, f64)>,
    ) -> PyResult<Bound<'py, PyDict>> {
        let data: Vec<u8> = if let Ok(b) = input.cast::<PyBytes>() {
            b.as_bytes().to_vec()
        } else if let Ok(path) = input.extract::<PathBuf>() {
            std::fs::read(path).map_err(io_err)?
        } else {
            let read = input.call_method0("read")?;
            read.cast::<PyBytes>()
                .map_err(|_| PyTypeError::new_err("input must be a path, bytes or a binary file"))?
                .as_bytes()
                .to_vec()
        };
        let class = match class_ {
            None => None,
            Some(c) => match term_from_py(c)? {
                Term::NamedNode(n) => Some(n),
                _ => return Err(PyValueError::new_err("class_ must be a NamedNode")),
            },
        };
        let graph = match to_graph.map(graph_from_py).transpose()? {
            None | Some(GraphName::DefaultGraph) => None,
            Some(GraphName::NamedNode(n)) => Some(n),
            Some(_) => return Err(PyValueError::new_err("to_graph must be a NamedNode")),
        };
        let mut mapping = tripletrail::geoparquet::ImportMapping {
            subject,
            class,
            graph,
            bbox: bbox.map(|(a, b, c, d)| [a, b, c, d]),
            ..Default::default()
        };
        if let Some(p) = predicate_prefix {
            mapping.predicate_prefix = p;
        }
        let engine = self.inner.clone();
        let stats = py
            .detach(move || {
                engine.load_geoparquet(tripletrail::geoparquet::Bytes::from(data), &mapping)
            })
            .map_err(to_py_err)?;
        let d = PyDict::new(py);
        d.set_item("rows", stats.rows)?;
        d.set_item("triples", stats.triples)?;
        d.set_item("skipped_rows", stats.skipped_rows)?;
        d.set_item("invalid_geometries", stats.invalid_geometries)?;
        Ok(d)
    }

    /// Runs the SHACL-AF rules (`sh:SPARQLRule`, `sh:TripleRule`) of the shapes in the named
    /// graph `shapes_graph` on `data_graph` (the default graph if None), in `sh:order`, pass
    /// after pass until they infer nothing new (at most `max_iterations`). With `into` (a
    /// graph name), the inferred triples are written there and their number is returned;
    /// otherwise they are returned as a list of `Triple`. Only triples not already
    /// in the data are inferred; with `include_existing=True`, every triple the rules
    /// construct is (as TopBraid's shaclinfer does). With `ordered=True`, each rule runs once,
    /// in `sh:order` and then by rule IRI, each seeing what the rules before it inferred: one
    /// pass, as a list of SPARQL INSERTs run in that order. The GIL is released meanwhile.
    #[pyo3(signature = (shapes_graph, *, data_graph = None, into = None, max_iterations = 64, include_existing = false, ordered = false))]
    fn infer<'py>(
        &self,
        py: Python<'py>,
        shapes_graph: &Bound<'py, PyAny>,
        data_graph: Option<&Bound<'py, PyAny>>,
        into: Option<&Bound<'py, PyAny>>,
        max_iterations: usize,
        include_existing: bool,
        ordered: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let shapes = match graph_from_py(shapes_graph)? {
            GraphName::NamedNode(n) => n,
            _ => return Err(PyValueError::new_err("shapes_graph must be a NamedNode")),
        };
        let data = data_graph
            .map(graph_from_py)
            .transpose()?
            .unwrap_or_default();
        let into = into.map(graph_from_py).transpose()?;
        let engine = self.inner.clone();
        let written = into.is_some();
        let inferred = py
            .detach(move || {
                let options = tripletrail::InferOptions {
                    max_passes: if ordered { 1 } else { max_iterations },
                    include_existing,
                    sequential: ordered,
                    ..Default::default()
                };
                engine.infer_with(
                    &shapes,
                    data.as_ref(),
                    into.as_ref().map(|g| g.as_ref()),
                    &options,
                    &|_| {},
                )
            })
            .map_err(to_py_err)?;
        if written {
            return Ok(inferred.len().into_pyobject(py)?.into_any());
        }
        let triples: Vec<Bound<'py, PyAny>> = inferred
            .iter()
            .map(|t| PyTriple::from(t.clone()).into_bound_py_any(py))
            .collect::<PyResult<_>>()?;
        Ok(pyo3::types::PyList::new(py, triples)?.into_any())
    }

    /// Writes the data, as it is now, to a new dataset directory `path` that
    /// `Engine(path)`, `Engine.open_read_only(path)` and `tripletrail serve` open without loading:
    /// a checkpoint of the tripletrail store with the parsed geometries. It is written under a
    /// temporary name and renamed when complete, so `path` never holds a half-written
    /// dataset; `path` must not exist or be an empty directory. The GIL is released meanwhile.
    fn save(&self, py: Python<'_>, path: PathBuf) -> PyResult<()> {
        let engine = self.inner.clone();
        py.detach(move || engine.save(path)).map_err(to_py_err)
    }

    /// Opens the dataset directory `path` (written by `save`, `tripletrail load` or
    /// `Engine(path)`) for reading only: its files are memory-mapped and never changed, and
    /// updates raise an error.
    #[staticmethod]
    fn open_read_only(path: PathBuf) -> PyResult<Self> {
        Ok(Self {
            inner: tripletrail::Engine::open_read_only(path).map_err(to_py_err)?,
        })
    }

    /// Whether the default graph of queries and updates is the union of all graphs (False by
    /// default), as Fuseki's `unionDefaultGraph`. A query with FROM, a query given
    /// `default_graph` or `named_graphs`, and an update with USING keep their own dataset;
    /// inserts without GRAPH still go to the real default graph. Unlike
    /// `use_default_graph_as_union`, it applies to updates too and keeps the spatial rewrite.
    #[getter]
    fn get_union_default_graph(&self) -> bool {
        self.inner.union_default_graph()
    }

    #[setter]
    fn set_union_default_graph(&self, union: bool) {
        self.inner.set_union_default_graph(union);
    }

    /// Whether the engine was opened with `open_read_only`.
    #[getter]
    fn read_only(&self) -> bool {
        self.inner.is_read_only()
    }

    /// Keeps the results of repeated SELECT and ASK queries, up to `bytes` of memory (0, the
    /// default, turns it off), until the next write. Only on the tripletrail store.
    fn set_result_cache(&self, bytes: usize) {
        self.inner.set_result_cache(bytes);
    }

    /// The result cache's `hits`, `misses`, `entries`, `bytes` and `budget`, as a dict.
    fn result_cache_stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let s = self.inner.result_cache_stats();
        let d = PyDict::new(py);
        d.set_item("hits", s.hits)?;
        d.set_item("misses", s.misses)?;
        d.set_item("entries", s.entries)?;
        d.set_item("bytes", s.bytes)?;
        d.set_item("budget", s.budget)?;
        Ok(d)
    }

    /// Drops the spatial index and routing graphs, to be rebuilt on next use.
    fn invalidate(&self) {
        self.inner.invalidate();
    }

    fn __len__(&self) -> PyResult<usize> {
        self.inner.store().len().map_err(io_err)
    }

    fn __iter__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        quads(py, self.inner.store().iter())
    }
}

impl PyEngine {
    fn run_update(&self, py: Python<'_>, update: String) -> PyResult<()> {
        let engine = self.inner.clone();
        py.detach(move || engine.update(&update)).map_err(to_py_err)
    }

    fn remove_graph_quads(&self, graph: &GraphName) -> PyResult<()> {
        let quads = self
            .inner
            .store()
            .quads_for_pattern(None, None, None, Some(graph.as_ref()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(io_err)?;
        for q in &quads {
            self.inner.remove(q).map_err(to_py_err)?;
        }
        Ok(())
    }
}

#[cfg(feature = "rocksdb")]
fn open(path: &std::path::Path) -> Result<Store, tripletrail::oxigraph::store::StorageError> {
    Store::open(path)
}

#[cfg(not(feature = "rocksdb"))]
fn open(_: &std::path::Path) -> Result<Store, tripletrail::oxigraph::store::StorageError> {
    Err(tripletrail::oxigraph::store::StorageError::Other(
        "built without on-disk storage (the rocksdb feature)".into(),
    ))
}

/// The quads of a scan, as `Quad`s, read as Python asks for them.
#[pyclass(name = "QuadIter", module = "tripletrail", frozen)]
struct PyQuadIter {
    inner: std::sync::Mutex<QuadIter>,
}

fn quads(py: Python<'_>, inner: QuadIter) -> PyResult<Bound<'_, PyAny>> {
    Bound::new(
        py,
        PyQuadIter {
            inner: std::sync::Mutex::new(inner),
        },
    )
    .map(Bound::into_any)
}

#[pymethods]
impl PyQuadIter {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&self) -> PyResult<Option<PyQuad>> {
        let next = self.inner.lock().unwrap_or_else(|e| e.into_inner()).next();
        next.map(|q| q.map(PyQuad::from).map_err(io_err))
            .transpose()
    }
}

struct ResultRow {
    focus_node: Term,
    result_path: Option<String>,
    value: Option<Term>,
    source_shape: Term,
    shape_iri: String,
    source_constraint_component: Term,
    severity: Term,
    messages: Vec<Term>,
    key: String,
}

/// A query parsed once by `Engine.prepare`, run by `Engine.query`.
#[pyclass(name = "PreparedQuery", module = "tripletrail", frozen)]
struct PyPreparedQuery {
    inner: tripletrail::PreparedQuery,
}

#[pymethods]
impl PyPreparedQuery {
    /// The variables the query selects, which `substitutions` may bind.
    #[getter]
    fn variables(&self) -> Vec<ox::model::PyVariable> {
        self.inner
            .variables()
            .into_iter()
            .map(ox::model::PyVariable::from)
            .collect()
    }
}

/// The outcome of `Engine.validate`: `conforms`, the `results` as rows, and the W3C
/// validation report (`serialize`).
#[pyclass(name = "ValidationReport", module = "tripletrail", frozen)]
struct PyValidationReport {
    conforms: bool,
    rows: Vec<ResultRow>,
    triples: Vec<Triple>,
    counts: Vec<tripletrail::shacl::ShapeCount>,
    summary: Option<tripletrail::shacl::Summary>,
}

#[pymethods]
impl PyValidationReport {
    /// Whether the data conforms: no results at all, whatever their severity.
    #[getter]
    fn conforms(&self) -> bool {
        self.conforms
    }

    /// One dict per result, ready for `pandas.DataFrame`: `focus_node`, `result_path` (as a
    /// SPARQL path, or None), `value`, `source_shape`, `source_constraint_component`,
    /// `severity` (terms), `message` (the first message, or None; `messages`
    /// holds them all), `shape_iri` (the source shape's stable IRI) and `key` (the finding key,
    /// 32 hexadecimal digits, the same for the same result in every run).
    #[getter]
    fn results<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let term = |t: &Term| py_term(py, t.clone());
        self.rows
            .iter()
            .map(|r| {
                let d = PyDict::new(py);
                d.set_item("focus_node", term(&r.focus_node)?)?;
                d.set_item("result_path", &r.result_path)?;
                d.set_item("value", r.value.as_ref().map(term).transpose()?)?;
                d.set_item("source_shape", term(&r.source_shape)?)?;
                d.set_item("shape_iri", &r.shape_iri)?;
                d.set_item(
                    "source_constraint_component",
                    term(&r.source_constraint_component)?,
                )?;
                d.set_item("severity", term(&r.severity)?)?;
                d.set_item("message", r.messages.first().map(term).transpose()?)?;
                d.set_item(
                    "messages",
                    r.messages.iter().map(term).collect::<PyResult<Vec<_>>>()?,
                )?;
                d.set_item("key", &r.key)?;
                Ok(d)
            })
            .collect()
    }

    /// With `Engine.validate(..., statistics=True)`, one dict per shape checked (conforming or
    /// not, and every targeted shape), ready for `pandas.DataFrame`: `shape` (a
    /// term), `shape_iri` (its IRI, for a blank node shape the stable IRI TripleTrail names it,
    /// the same in every run), `property` (whether it has `sh:path`), `focus_nodes` and `value_nodes` (checks,
    /// a node reached from two parents counting twice), `distinct_focus_nodes` and
    /// `failing_focus_nodes` (distinct focus nodes of its results). Empty otherwise.
    #[getter]
    fn shape_counts<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyDict>>> {
        self.counts
            .iter()
            .map(|c| {
                let d = PyDict::new(py);
                d.set_item("shape", py_term(py, c.shape.clone())?)?;
                d.set_item("shape_iri", c.iri.as_str())?;
                d.set_item("property", c.property)?;
                d.set_item("focus_nodes", c.focus_nodes)?;
                d.set_item("value_nodes", c.value_nodes)?;
                d.set_item("distinct_focus_nodes", c.distinct_focus_nodes)?;
                d.set_item("failing_focus_nodes", c.failing_focus_nodes)?;
                Ok(d)
            })
            .collect()
    }

    /// With `Engine.validate(..., statistics=True)`, the run in figures, made while the results
    /// were found: `results`, `failing_focus_nodes` (distinct focus nodes with a result),
    /// `by_severity` (severity IRI to results) and `cells`, one dict per rule, severity and
    /// constraint component with results, the most first (`shape_iri`, `severity`, `component`,
    /// `results`, `failing_focus_nodes`, and `samples`: up to 3 dicts with `focus_node`, `value`
    /// and `key`). None otherwise.
    #[getter]
    fn summary<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyDict>>> {
        let Some(s) = &self.summary else {
            return Ok(None);
        };
        let d = PyDict::new(py);
        d.set_item("results", s.results)?;
        d.set_item("failing_focus_nodes", s.failing_focus_nodes)?;
        let by = PyDict::new(py);
        for (severity, n) in s.by_severity() {
            let key = match &severity {
                Term::NamedNode(n) => n.as_str().to_owned(),
                other => other.to_string(),
            };
            by.set_item(key, n)?;
        }
        d.set_item("by_severity", by)?;
        let cells = s
            .cells
            .iter()
            .map(|c| {
                let cell = PyDict::new(py);
                cell.set_item("shape_iri", c.shape_iri.as_str())?;
                cell.set_item("severity", py_term(py, c.severity.clone())?)?;
                cell.set_item("component", py_term(py, c.component.clone())?)?;
                cell.set_item("results", c.results)?;
                cell.set_item("failing_focus_nodes", c.focus_nodes)?;
                let samples = c
                    .samples
                    .iter()
                    .map(|x| {
                        let one = PyDict::new(py);
                        one.set_item("focus_node", py_term(py, x.focus_node.clone())?)?;
                        one.set_item(
                            "value",
                            x.value.clone().map(|v| py_term(py, v)).transpose()?,
                        )?;
                        one.set_item("key", format!("{:032x}", x.key))?;
                        Ok(one)
                    })
                    .collect::<PyResult<Vec<_>>>()?;
                cell.set_item("samples", samples)?;
                Ok(cell)
            })
            .collect::<PyResult<Vec<_>>>()?;
        d.set_item("cells", cells)?;
        Ok(Some(d))
    }

    fn __len__(&self) -> usize {
        self.rows.len()
    }

    /// The report's triples, as `Triple`s.
    fn triples<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyAny>>> {
        self.triples
            .iter()
            .map(|t| PyTriple::from(t.clone()).into_bound_py_any(py))
            .collect()
    }

    /// Writes the W3C validation report (`format`: a `RdfFormat`, media type or
    /// extension; Turtle by default) to `output`, or returns it as bytes.
    #[pyo3(signature = (output = None, format = None))]
    fn serialize<'py>(
        &self,
        py: Python<'py>,
        output: Option<&Bound<'py, PyAny>>,
        format: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Option<Bound<'py, PyBytes>>> {
        let format = match format {
            Some(f) => rdf_format(Some(f), None)?,
            None => RdfFormat::Turtle,
        };
        let triples = &self.triples;
        write_output(py, output, |w| {
            let mut serializer = RdfSerializer::from_format(format)
                .with_prefix("sh", "http://www.w3.org/ns/shacl#")
                .map_err(std::io::Error::other)?
                .for_writer(w);
            for t in triples {
                serializer.serialize_triple(t)?;
            }
            serializer.finish().map(|_| ())
        })
    }

    fn __repr__(&self) -> String {
        format!(
            "<ValidationReport conforms={} results={}>",
            self.conforms,
            self.rows.len()
        )
    }
}

/// The store's quads, canonicalized (RDFC-1.0).
fn canonical(engine: &tripletrail::Engine) -> PyResult<tripletrail::canon::Canonical> {
    let quads = engine
        .store()
        .iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(io_err)?;
    tripletrail::canon::canonicalize(quads).map_err(|e| PyValueError::new_err(e.to_string()))
}

/// What changed from engine `a` to engine `b` (two releases): a dict with `added` and
/// `removed` (canonical N-Quads lines, blank nodes matched through their RDFC-1.0 labels),
/// the `sha256` of each side, and counts per `graphs`, `predicates` and `classes` (of the
/// subject), each a dict of IRI to `(added, removed)`.
#[pyfunction]
fn diff<'py>(py: Python<'py>, a: &PyEngine, b: &PyEngine) -> PyResult<Bound<'py, PyDict>> {
    let (a, b) = (canonical(&a.inner)?, canonical(&b.inner)?);
    let d = tripletrail::canon::diff(&a, &b);
    let summary = d.summary(&a, &b);
    let out = PyDict::new(py);
    out.set_item("added", &d.added)?;
    out.set_item("removed", &d.removed)?;
    out.set_item("sha256", (a.digest(), b.digest()))?;
    for (key, counts) in [
        ("graphs", &summary.graphs),
        ("predicates", &summary.predicates),
        ("classes", &summary.classes),
    ] {
        let dict = PyDict::new(py);
        for (iri, c) in counts {
            dict.set_item(iri, (c.added, c.removed))?;
        }
        out.set_item(key, dict)?;
    }
    Ok(out)
}

#[pymodule]
#[pyo3(name = "_tripletrail")]
fn tripletrail_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyEngine>()?;
    m.add_class::<PyQuadIter>()?;
    m.add_class::<PyValidationReport>()?;
    m.add_class::<PyPreparedQuery>()?;
    // pyoxigraph's classes and functions (see `ox`).
    m.add_class::<ox::model::PyNamedNode>()?;
    m.add_class::<ox::model::PyBlankNode>()?;
    m.add_class::<ox::model::PyLiteral>()?;
    m.add_class::<ox::model::PyDefaultGraph>()?;
    m.add_class::<ox::model::PyTriple>()?;
    m.add_class::<ox::model::PyQuad>()?;
    m.add_class::<ox::model::PyVariable>()?;
    m.add_class::<ox::model::PyBaseDirection>()?;
    m.add_class::<ox::io::PyRdfFormat>()?;
    m.add_class::<ox::io::PyQuadParser>()?;
    m.add_class::<ox::dataset::PyDataset>()?;
    m.add_class::<ox::dataset::PyCanonicalizationAlgorithm>()?;
    m.add_class::<ox::sparql::PyQuerySolutions>()?;
    m.add_class::<ox::sparql::PyQuerySolution>()?;
    m.add_class::<ox::sparql::PyQueryTriples>()?;
    m.add_class::<ox::sparql::PyQueryBoolean>()?;
    m.add_class::<ox::sparql::PyQueryResultsFormat>()?;
    m.add_function(wrap_pyfunction!(diff, m)?)?;
    m.add_function(wrap_pyfunction!(ox::io::parse, m)?)?;
    m.add_function(wrap_pyfunction!(ox::io::serialize, m)?)?;
    m.add_function(wrap_pyfunction!(ox::sparql::parse_query_results, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
