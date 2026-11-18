//! Offline counterparts of Jena's command-line tools (#828): `tripletrail query` (`arq`,
//! `tdb2.tdbquery`), `tripletrail update` (`tdb2.tdbupdate`), `tripletrail dump` (`tdb2.tdbdump`) and
//! `tripletrail convert` (`riot`), on a dataset on disk or on files loaded in memory, without a
//! server.

use crate::{Result, StoreKind, load_file, open};
use std::io::Write;
use std::path::{Path, PathBuf};
use tripletrail::Engine;
use tripletrail::oxigraph::io::{RdfFormat, RdfParser};
use tripletrail::oxigraph::model::{BlankNode, NamedOrBlankNode, Term, Triple};
use tripletrail::oxigraph::model::{GraphName, NamedNode, Quad};
use tripletrail::oxigraph::sparql::QueryResults;
use tripletrail::oxigraph::sparql::results::{QueryResultsFormat, QueryResultsSerializer};
use tripletrail_server::rdf_formats::{Format, QuadWriter, read_file};

/// Where the data comes from: a dataset on disk, or files loaded in memory.
pub(crate) struct Source<'a> {
    pub location: Option<&'a Path>,
    pub dataset: &'a str,
    pub store: StoreKind,
    pub data: &'a [PathBuf],
    pub union_default_graph: bool,
    /// `--text-properties`: IRIs of more properties for full-text search to index.
    pub text_properties: &'a [String],
}

impl Source<'_> {
    fn engine(&self) -> Result<Engine> {
        if self.location.is_some() == !self.data.is_empty() {
            return Err("give either --location or --data files".into());
        }
        if let Some(dir) = self.location
            && !dir.join(self.dataset).is_dir()
        {
            return Err(format!("no dataset {:?} in {}", self.dataset, dir.display()).into());
        }
        let engine = open(self.location, self.dataset, self.store)?;
        let several = self.data.len() > 1;
        for (i, file) in self.data.iter().enumerate() {
            let rdf = !file
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("parquet"));
            if several && rdf {
                load_scoped(&engine, file, i)?;
            } else {
                load_file(&engine, file, None)?;
            }
        }
        engine.set_union_default_graph(self.union_default_graph);
        let properties = self
            .text_properties
            .iter()
            .map(|p| NamedNode::new(p).map_err(|e| format!("--text-properties {p:?}: {e}")))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        engine.set_text_properties(&properties);
        Ok(engine)
    }
}

/// A query or update given as text, as `@file`, or as the path of a `.rq` / `.ru` file.
pub(crate) fn text(arg: &str) -> Result<String> {
    if let Some(path) = arg.strip_prefix('@') {
        return std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}").into());
    }
    let path = Path::new(arg);
    let is_file = matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("rq" | "ru" | "sparql")
    ) && path.is_file();
    Ok(if is_file {
        std::fs::read_to_string(path)?
    } else {
        arg.to_owned()
    })
}

fn writer(output: Option<&Path>) -> Result<Box<dyn Write>> {
    Ok(match output {
        Some(path) => Box::new(std::io::BufWriter::new(std::fs::File::create(path)?)),
        None => Box::new(std::io::BufWriter::new(std::io::stdout().lock())),
    })
}

/// An RDF format by name or file extension (`ttl`, `turtle`, `nt`, `nq`, `trig`, `rdf`, `rj`,
/// `trix`, …).
fn rdf_format(name: &str) -> Result<Format> {
    if let Some(f) = Format::from_name(name).or_else(|| Format::from_extension(name)) {
        return Ok(f);
    }
    let name = name.to_ascii_lowercase();
    let ext = match name.as_str() {
        "turtle" => "ttl",
        "ntriples" | "n-triples" => "nt",
        "nquads" | "n-quads" => "nq",
        "rdfxml" | "rdf/xml" | "xml" => "rdf",
        "json-ld" => "jsonld",
        other => other,
    };
    Format::from_extension(ext)
        .or_else(|| Format::from_media_type(&name))
        .ok_or_else(|| format!("unknown RDF format {name:?}").into())
}

/// The format of a file from its extension.
fn format_of(path: &Path) -> Result<Format> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    Format::from_extension(ext)
        .ok_or_else(|| format!("unknown RDF format for {}", path.display()).into())
}

fn results_format(name: &str) -> Result<QueryResultsFormat> {
    match name.to_ascii_lowercase().as_str() {
        "json" | "srj" => Ok(QueryResultsFormat::Json),
        "xml" | "srx" => Ok(QueryResultsFormat::Xml),
        "csv" => Ok(QueryResultsFormat::Csv),
        "tsv" => Ok(QueryResultsFormat::Tsv),
        other => QueryResultsFormat::from_media_type(other).ok_or_else(|| {
            format!("unknown results format {other:?} (csv, tsv, json, xml)").into()
        }),
    }
}

/// `tripletrail query`: runs a query and writes its results, rows as TSV (or `format`) and graphs
/// as Turtle (or `format`), to `output` or standard output.
pub(crate) fn query(
    source: &Source,
    query: &str,
    format: Option<&str>,
    output: Option<&Path>,
) -> Result<()> {
    let query = text(query)?;
    let engine = source.engine()?;
    let results = engine.query(&query)?;
    // The format of the file written to, when no format is given.
    let by_extension = || output.and_then(|p| p.extension()).and_then(|e| e.to_str());
    let mut out = writer(output)?;
    match results {
        QueryResults::Boolean(value) => {
            let format = results_format(format.or_else(by_extension).unwrap_or("tsv"))?;
            QueryResultsSerializer::from_format(format)
                .serialize_boolean_to_writer(&mut out, value)?;
            if output.is_none() {
                writeln!(out)?;
            }
        }
        QueryResults::Solutions(solutions) => {
            let format = results_format(format.or_else(by_extension).unwrap_or("tsv"))?;
            let mut serializer = QueryResultsSerializer::from_format(format)
                .serialize_solutions_to_writer(&mut out, solutions.variables().to_vec())?;
            for solution in solutions {
                serializer.serialize(&solution?)?;
            }
            serializer.finish()?;
            if output.is_none() && matches!(format, QueryResultsFormat::Json) {
                writeln!(out)?;
            }
        }
        QueryResults::Graph(triples) => {
            let format = rdf_format(format.or_else(by_extension).unwrap_or("ttl"))?;
            let mut serializer = QuadWriter::new(format, &mut out);
            for triple in triples {
                serializer.serialize_triple(&triple?)?;
            }
            serializer.finish()?;
        }
    }
    out.flush()?;
    Ok(())
}

/// `tripletrail update`: runs an update on a dataset on disk, where it is kept, or on files in
/// memory, whose dataset after the update is written to `output` (N-Quads on standard output
/// if omitted).
pub(crate) fn update(
    source: &Source,
    update: &str,
    format: Option<&str>,
    output: Option<&Path>,
) -> Result<()> {
    let update = text(update)?;
    if source.location.is_some() && output.is_some() {
        return Err(
            "--output is for --data files; with --location the dataset keeps the changes".into(),
        );
    }
    let engine = source.engine()?;
    let before = engine.store().len()?;
    engine.update(&update)?;
    let after = engine.store().len()?;
    eprintln!("{before} quads before, {after} after");
    if source.location.is_none() {
        let format = match (format, output) {
            (Some(f), _) => rdf_format(f)?,
            (None, Some(path)) => format_of(path)?,
            (None, None) => Format::Rdf(RdfFormat::NQuads),
        };
        write_quads(&engine, None, format, output)?;
    }
    Ok(())
}

/// `tripletrail dump`: writes every quad of a dataset on disk, or of one graph with `graph`
/// (`default` for the default graph), as N-Quads (or `format`).
pub(crate) fn dump(
    location: &Path,
    dataset: &str,
    store: StoreKind,
    graph: Option<&str>,
    format: Option<&str>,
    output: Option<&Path>,
) -> Result<()> {
    let source = Source {
        location: Some(location),
        dataset,
        store,
        data: &[],
        union_default_graph: false,
        text_properties: &[],
    };
    let engine = source.engine()?;
    let graph = match graph {
        None => None,
        Some("default") => Some(GraphName::DefaultGraph),
        Some(iri) => Some(GraphName::NamedNode(
            NamedNode::new(iri).map_err(|e| format!("--graph {iri:?}: {e}"))?,
        )),
    };
    let format = match (format, output) {
        (Some(f), _) => rdf_format(f)?,
        (None, Some(path)) => format_of(path)?,
        (None, None) => Format::Rdf(RdfFormat::NQuads),
    };
    write_quads(&engine, graph.as_ref(), format, output)
}

/// Writes the quads of `engine` (of one graph, if given): a format of triples takes one graph,
/// written as the default graph.
fn write_quads(
    engine: &Engine,
    graph: Option<&GraphName>,
    format: Format,
    output: Option<&Path>,
) -> Result<()> {
    let store = engine.store();
    // Refused before anything is written: named graphs, in a format of one graph.
    if graph.is_none() && !format.supports_datasets() {
        for g in store.named_graphs()? {
            if store
                .quads_for_pattern(None, None, None, Some(g.as_ref().into()))
                .next()
                .is_some()
            {
                return Err(format!(
                    "{} holds one graph, and the data has named graphs: give --graph, or write N-Quads or TriG",
                    format.name()
                )
                .into());
            }
        }
    }
    let mut out = writer(output)?;
    let mut serializer = QuadWriter::new(format, &mut out);
    let mut written = 0usize;
    let quads = match graph {
        Some(g) => store.quads_for_pattern(None, None, None, Some(g.as_ref())),
        None => store.iter(),
    };
    for quad in quads {
        write_quad(&mut serializer, quad?, format, graph.is_some())?;
        written += 1;
    }
    serializer.finish()?;
    out.flush()?;
    eprintln!("{written} {}", plural(written, "quad"));
    Ok(())
}

/// One quad: in a format of triples, as a triple, which only the default graph's quads (or
/// those of the one graph asked for, `one_graph`) can be.
fn write_quad<W: Write>(
    serializer: &mut QuadWriter<W>,
    quad: Quad,
    format: Format,
    one_graph: bool,
) -> Result<()> {
    if format.supports_datasets() {
        serializer.serialize_quad(&quad)?;
    } else if quad.graph_name.is_default_graph() || one_graph {
        serializer.serialize_triple(quad.as_ref())?;
    } else {
        return Err(format!(
            "{} holds one graph, and the data has named graphs: give --graph, or write N-Quads or TriG",
            format.name()
        )
        .into());
    }
    Ok(())
}

/// What `tripletrail convert` does with the files.
pub(crate) struct Convert<'a> {
    pub inputs: &'a [PathBuf],
    /// The format of the input files, when their extensions don't say.
    pub syntax: Option<&'a str>,
    pub to: Option<&'a str>,
    pub output: Option<&'a Path>,
    /// Only parse, and report the first error of each file.
    pub validate: bool,
    /// Print how many triples or quads each file holds.
    pub count: bool,
    pub base: Option<&'a str>,
}

/// `tripletrail convert`: parses RDF files as they stream and writes them in another format
/// (N-Quads by default, or the extension of `output`), checks them (`validate`), or counts
/// their triples (`count`), as Jena's `riot`.
pub(crate) fn convert(c: &Convert) -> Result<()> {
    if c.inputs.is_empty() {
        return Err("give the RDF files to read".into());
    }
    let write = !c.validate && !c.count;
    let format = match (c.to, c.output) {
        (Some(f), _) => rdf_format(f)?,
        (None, Some(path)) => format_of(path)?,
        (None, None) => Format::Rdf(RdfFormat::NQuads),
    };
    let mut out = if write { Some(writer(c.output)?) } else { None };
    let mut serializer = out.as_mut().map(|o| QuadWriter::new(format, o));
    let mut failed = 0usize;
    for (index, input) in c.inputs.iter().enumerate() {
        let input_format = match c.syntax {
            Some(s) => rdf_format(s)?,
            None => format_of(input)?,
        };
        let parser = |f| {
            let parser = RdfParser::from_format(f);
            match c.base {
                Some(base) => parser
                    .with_base_iri(base)
                    .map_err(|e| format!("--base: {e}")),
                None => Ok(parser),
            }
        };
        let mut n = 0usize;
        let mut error = None;
        let scope = (c.inputs.len() > 1).then_some(index);
        let quads = match read_file(input_format, parser, input) {
            Ok(quads) => quads,
            // A document that can't be read at all, as a first parse error.
            Err(e) => Box::new(std::iter::once(Err(e))),
        };
        for quad in quads {
            match quad {
                Ok(quad) => {
                    n += 1;
                    if let Some(s) = serializer.as_mut() {
                        let quad = match scope {
                            Some(i) => scoped(quad, i),
                            None => quad,
                        };
                        write_quad(s, quad, format, false)?;
                    }
                }
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
        }
        match error {
            Some(e) if c.validate || c.count => {
                failed += 1;
                println!("{}: {e}", input.display());
            }
            Some(e) => return Err(format!("{}: {e}", input.display()).into()),
            None if c.count => println!("{}: {n} {}", input.display(), plural(n, "triple")),
            None if c.validate => {
                println!("{}: valid, {n} {}", input.display(), plural(n, "triple"));
            }
            None => {}
        }
    }
    if let Some(s) = serializer {
        s.finish()?;
    }
    if let Some(mut o) = out {
        o.flush()?;
    }
    if failed > 0 {
        return Err(format!("{failed} of {} files are not valid", c.inputs.len()).into());
    }
    Ok(())
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_owned()
    } else {
        format!("{word}s")
    }
}

/// Loads a file whose blank nodes are its own: those of file `i` are renamed `f{i}_<label>`,
/// so files that use the same label do not share a blank node (labels are scoped to a
/// document, as Jena reads them).
fn load_scoped(engine: &Engine, path: &Path, i: usize) -> Result<()> {
    let mut error = None;
    let quads = read_file(format_of(path)?, |f| Ok(RdfParser::from_format(f)), path)?
        .map_while(|q| q.map_err(|e| error = Some(e)).ok())
        .map(|q| scoped(q, i));
    engine.load_quads(quads)?;
    match error {
        Some(e) => Err(format!("{}: {e}", path.display()).into()),
        None => Ok(()),
    }
}

/// `quad` with each blank node label `l` written `f{i}_l`: labels of different files stay
/// apart (no `f{i}_` is the start of another), and those of one file keep their identity.
fn scoped(quad: Quad, i: usize) -> Quad {
    let node = |b: BlankNode| BlankNode::new_unchecked(format!("f{i}_{}", b.as_str()));
    let subject = |s: NamedOrBlankNode| match s {
        NamedOrBlankNode::BlankNode(b) => NamedOrBlankNode::BlankNode(node(b)),
        s => s,
    };
    fn term(t: Term, node: &dyn Fn(BlankNode) -> BlankNode) -> Term {
        match t {
            Term::BlankNode(b) => Term::BlankNode(node(b)),
            Term::Triple(t) => {
                let t = *t;
                let s = match t.subject {
                    NamedOrBlankNode::BlankNode(b) => NamedOrBlankNode::BlankNode(node(b)),
                    s => s,
                };
                Term::Triple(Box::new(Triple::new(s, t.predicate, term(t.object, node))))
            }
            t => t,
        }
    }
    Quad::new(
        subject(quad.subject),
        quad.predicate,
        term(quad.object, &node),
        match quad.graph_name {
            GraphName::BlankNode(b) => GraphName::BlankNode(node(b)),
            g => g,
        },
    )
}
