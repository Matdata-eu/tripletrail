//! Querying files as RDF with SPARQL, following the [Facade-X] model, so that queries
//! written for SPARQL Anything run unchanged:
//!
//! ```sparql
//! PREFIX xyz: <http://sparql.xyz/facade-x/data/>
//! PREFIX fx:  <http://sparql.xyz/facade-x/ns/>
//! CONSTRUCT { ?s <http://ex.org/name> ?name }
//! WHERE {
//!   SERVICE <x-sparql-anything:location=people.json> { ?p xyz:name ?name }
//!   BIND(fx:entity("http://ex.org/person/", ?name) AS ?s)
//! }
//! ```
//!
//! A SERVICE whose IRI starts with `x-sparql-anything:` reads its source (a file, or the
//! `content` option), builds the source's Facade-X graph in memory and evaluates the SERVICE
//! pattern on it. Only the triples whose predicate the pattern can match are built. The
//! `fx:` functions ([`functions`]) work anywhere in the query.
//!
//! Formats: CSV, JSON, YAML, XML, HTML, spreadsheets (xlsx, xls, ods) and plain text, and
//! database tables and queries through [`FxConfig::databases`]. Locations are local files, resolved against
//! [`FxConfig::base`] and limited to [`FxConfig::allowed`].
//!
//! [Facade-X]: https://w3c-facade-x.github.io/facade-x-specs/

mod archive;
mod correlate;
mod csv;
pub mod database;
mod features;
mod functions;
mod geo;
mod html;
mod json;
mod markdown;
pub mod options;
pub mod remote;
pub mod sources;
mod spreadsheet;
mod tree;
mod xls;
mod xlsx_extras;
mod xml;
mod yaml;

pub use correlate::correlate;
pub use database::{
    Condition, DatabaseCall, Databases, OneOf, Op, Query as DatabaseQuery, Rows, Select,
    record_database_calls,
};
pub use functions::{Function, functions};
pub use options::{Options, SCHEME};

use oxigraph::model::{NamedNode, Quad, Term, Variable};
use oxigraph::sparql::{
    DefaultServiceHandler, QueryEvaluationError, QueryResults, QuerySolution, QuerySolutionIter,
    SparqlEvaluator,
};
use oxiri::Iri;
use spargebra::algebra::GraphPattern;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tree::{Common, Sink};

/// Facade-X terms: `fx:Root`, `fx:anySlot`, the functions.
pub const FX: &str = "http://sparql.xyz/facade-x/ns/";
/// Facade-X engine vocabulary: `fxe:properties fxe:location "…"`.
pub const FXE: &str = "http://sparql.xyz/facade-x/engine/";
/// The default namespace of keys.
pub const XYZ: &str = "http://sparql.xyz/facade-x/data/";
pub(crate) const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

#[derive(Debug, thiserror::Error)]
pub enum FxError {
    #[error("Facade-X option: {0}")]
    Option(String),
    #[error("Facade-X: cannot read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("Facade-X: {0}")]
    Denied(String),
    #[error("Facade-X: {0}")]
    Unsupported(String),
    #[error("Facade-X: cannot parse {0}")]
    Parse(String),
    #[error("Facade-X: {0}")]
    Query(#[from] QueryEvaluationError),
    #[error("Facade-X: {0}")]
    Backend(Box<dyn std::error::Error + Send + Sync>),
}

/// Where sources may be read from.
#[derive(Debug, Clone)]
pub struct FxConfig {
    /// Relative locations are resolved against this directory.
    pub base: PathBuf,
    /// The directories sources may be read from (with their subdirectories); `None` allows
    /// any file. A server should always set it.
    pub allowed: Option<Vec<PathBuf>>,
    /// The URLs (`location=https://…`) sources may be read from: none by default.
    pub remote: remote::RemoteLocations,
    /// The databases `table=` and `sql=` sources read from: none by default, and then such
    /// sources fail.
    pub databases: Option<Arc<dyn database::Databases>>,
    /// Whether the size limits a source's options set (`spreadsheet.max-cells`,
    /// `spreadsheet.max-bytes`, `yaml.max-alias-nodes`) may only be lowered, not raised above
    /// their defaults. A server whose callers are not trusted should set it.
    pub fixed_limits: bool,
}

impl Default for FxConfig {
    /// The current directory, any file.
    fn default() -> Self {
        Self {
            base: std::env::current_dir().unwrap_or_default(),
            allowed: None,
            remote: remote::RemoteLocations::default(),
            databases: None,
            fixed_limits: false,
        }
    }
}

/// The size limits a source's options may set, with their defaults.
/// (`database.max-rows` is not here: the databases cap it themselves.)
const LIMITS: [(&str, u64); 3] = [
    ("spreadsheet.max-cells", spreadsheet::MAX_CELLS as u64),
    ("spreadsheet.max-bytes", spreadsheet::MAX_BYTES),
    ("yaml.max-alias-nodes", yaml::DEFAULT_MAX_ALIAS_NODES as u64),
];

/// Refuses a size limit raised above its default ([`FxConfig::fixed_limits`]). A value that
/// is not a number is left to the reader, which says what it should be.
fn fixed_limits(options: &options::Options) -> Result<(), FxError> {
    for (name, default) in LIMITS {
        if let Some(value) = options.get(name)
            && value
                .trim()
                .parse::<u128>()
                .is_ok_and(|n| n > u128::from(default))
        {
            return Err(FxError::Denied(format!(
                "{name} may not be raised above {default} on this server"
            )));
        }
    }
    Ok(())
}

/// Slices of answers waiting between a `slice=true` reader and the query that reads them.
const SLICE_QUEUE: usize = 4;

/// The stack of a `slice=true` reader thread, which evaluates the SERVICE pattern: as large
/// as the engine's query threads (`tripletrail::complexity::QUERY_STACK`, which checks that
/// this is not smaller), since a pattern within the query limits may need that much. Only
/// what is used is touched.
pub const SLICE_STACK: usize = 256 << 20;

/// What `slice=true` gives each row's triples (with the root's) to.
type EachSlice<'a> = &'a mut dyn FnMut(&[Quad]) -> Result<(), FxError>;

/// The handler of `SERVICE <x-sparql-anything:…>` calls, to register as the default service
/// handler of an evaluator. SERVICE calls to other IRIs fail.
#[derive(Clone)]
pub struct FxService {
    config: Arc<FxConfig>,
    backend: Arc<dyn Backend>,
    /// Files held in memory, read as `location=upload:NAME` ([`FxService::with_upload`]).
    uploads: sources::Uploads,
}

/// Evaluates a SERVICE pattern (as a SELECT `query`) on the quads built for it. `fx` is the
/// calling service, to register for nested calls.
pub trait Backend: Send + Sync {
    fn evaluate(
        &self,
        quads: Vec<Quad>,
        query: spargebra::Query,
        fx: &FxService,
    ) -> Result<QuerySolutionIter<'static>, FxError>;
}

/// The default [`Backend`]: an in-memory [`oxigraph::model::Dataset`] and an evaluator.
struct InMemory<F>(F);

impl<F: Fn() -> SparqlEvaluator + Send + Sync> Backend for InMemory<F> {
    fn evaluate(
        &self,
        quads: Vec<Quad>,
        query: spargebra::Query,
        fx: &FxService,
    ) -> Result<QuerySolutionIter<'static>, FxError> {
        let dataset: oxigraph::model::Dataset = quads.into_iter().collect();
        let QueryResults::Solutions(solutions) = fx
            .register((self.0)())
            .for_query(query)
            .on_queryable_dataset(&dataset)
            .execute()?
        else {
            unreachable!("a SELECT gives solutions")
        };
        let variables: Arc<[Variable]> = solutions.variables().into();
        let rows: Vec<_> = solutions.collect();
        Ok(QuerySolutionIter::new(variables, rows))
    }
}

impl FxService {
    /// `evaluator` makes the evaluator of the SERVICE pattern, with the custom functions the
    /// pattern may use; the `fx:` functions and this handler (for nested calls) are added.
    /// The pattern runs on an in-memory dataset.
    pub fn new(
        config: FxConfig,
        evaluator: impl Fn() -> SparqlEvaluator + Send + Sync + 'static,
    ) -> Self {
        Self::with_backend(config, InMemory(evaluator))
    }

    /// The SERVICE patterns run on `backend`.
    pub fn with_backend(config: FxConfig, backend: impl Backend + 'static) -> Self {
        Self {
            config: Arc::new(config),
            backend: Arc::new(backend),
            uploads: sources::Uploads::default(),
        }
    }

    /// The same service, also reading `bytes` as `location=upload:NAME` (and as
    /// `from-archive=upload:NAME`): a file sent to a server that reads no files of its own
    /// (the Import page). `name` is the file's name, whose extension says its format unless
    /// the options do; it may use letters, digits and `.-_` only.
    pub fn with_upload(&self, name: &str, bytes: Arc<[u8]>) -> Result<Self, FxError> {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        {
            return Err(FxError::Option(format!(
                "upload {name:?}: a name of letters, digits and .-_ only"
            )));
        }
        let mut uploads = (*self.uploads.0).clone();
        uploads.insert(format!("{}{name}", sources::UPLOAD), bytes);
        Ok(Self {
            uploads: sources::Uploads(Arc::new(uploads)),
            ..self.clone()
        })
    }

    /// Whether `table=` and `sql=` sources have a database to read.
    pub fn has_databases(&self) -> bool {
        self.config.databases.is_some()
    }

    /// Rows per call when `SERVICE <iri> { pattern }` is joined to local data on `shared`, or
    /// `None` when it should run as written: a bind join pays only for a database source whose
    /// query can take the joined values as a `WHERE` (one row per answer, rows not numbered,
    /// values as stored), joined on a column.
    pub fn bind_join_batch(
        &self,
        iri: &str,
        pattern: &GraphPattern,
        shared: &[Variable],
    ) -> Option<usize> {
        self.config.databases.as_ref()?;
        let call = options::prepare(iri, pattern).ok()?;
        let o = &call.options;
        (database::is_database(o)
            && !o.flag("slice", false).ok()?
            && o.flag("blank-nodes", true).ok()?
            && o.get("null-string").is_none()
            && !o.flag("trim-strings", false).ok()?
            && !call.needs.all
            && !call.needs.slots
            && database::binds_on(&call.pattern, shared))
        .then_some(database::BIND_JOIN_BATCH)
    }

    /// Whether `SERVICE <iri> { pattern }` reads a database source a `LIMIT` of its answers
    /// can be passed to (as SQL `LIMIT`): a pattern about one row, rows not numbered and
    /// values as stored (see [`Self::bind_join_batch`]).
    pub fn takes_limit(&self, iri: &str, pattern: &GraphPattern) -> bool {
        self.config.databases.is_some()
            && options::prepare(iri, pattern).is_ok_and(|call| narrow_rows(&call))
    }

    /// `evaluator` with the `fx:` functions and this handler.
    pub fn register(&self, evaluator: SparqlEvaluator) -> SparqlEvaluator {
        register_functions(evaluator).with_default_service_handler(self.clone())
    }

    fn call(
        &self,
        iri: &str,
        pattern: &GraphPattern,
        base_iri: Option<&Iri<String>>,
    ) -> Result<QuerySolutionIter<'static>, FxError> {
        let call = options::prepare(iri, pattern)?;
        if self.config.fixed_limits {
            fixed_limits(&call.options)?;
        }
        // A LIMIT of answers on a pattern about one row: SQL LIMIT, raised (doubled) while
        // rows give no answer, until enough answers or the rows run out.
        let limited = match &call.pattern {
            GraphPattern::Slice {
                inner,
                start,
                length: Some(length),
            } if self.config.databases.is_some() => {
                let inner = match &**inner {
                    GraphPattern::Project { inner, .. } => inner,
                    inner => inner,
                };
                let mut probe = options::prepare(iri, inner)?;
                probe.options = call.options.clone();
                narrow_rows(&probe).then_some((start.saturating_add(*length), *length))
            }
            _ => None,
        };
        let query = spargebra::Query::Select {
            dataset: None,
            pattern: correlate(call.pattern),
            base_iri: base_iri.cloned(),
        };
        let solutions = if call.options.flag("slice", false)? {
            self.sliced(call.options, call.needs, query)?
        } else if let Some((wanted_rows, answers)) = limited {
            let max_rows = database::max_rows(&call.options)?;
            let mut needs = call.needs;
            let mut rows = wanted_rows.max(1);
            loop {
                // At the cap, as written: more rows than it fail, as without a LIMIT.
                needs.limit = (rows < max_rows).then_some(rows);
                let quads = self.triplify(&call.options, &needs, None)?;
                let solutions = self.backend.evaluate(quads, query.clone(), self)?;
                let variables: Arc<[Variable]> = solutions.variables().into();
                let found: Vec<_> = solutions.collect();
                if found.len() >= answers || needs.limit.is_none() || needs.rows_read.get() < rows {
                    break QuerySolutionIter::new(variables, found);
                }
                rows = rows.saturating_mul(2);
            }
        } else {
            let quads = self.triplify(&call.options, &call.needs, None)?;
            self.backend.evaluate(quads, query, self)?
        };
        // The variables the rewrite added are not the caller's.
        let all: Vec<Variable> = solutions.variables().to_vec();
        let keep: Vec<usize> = (0..all.len())
            .filter(|&i| !all[i].as_str().starts_with(options::HIDDEN))
            .collect();
        let variables: Arc<[Variable]> = keep.iter().map(|&i| all[i].clone()).collect();
        let out = Arc::clone(&variables);
        Ok(QuerySolutionIter::new(
            variables,
            solutions.map(move |s| {
                let s = s?;
                let values: Vec<Option<Term>> = keep.iter().map(|&i| s.get(i).cloned()).collect();
                Ok(QuerySolution::from((Arc::clone(&out), values)))
            }),
        ))
    }

    /// `slice=true`: the pattern runs on each slice of rows with the root, on a thread of its
    /// own, and the solutions are handed over as they come (a bounded queue), so neither the
    /// source's triples nor all the answers are in memory at once.
    ///
    /// Each call starts a thread: a SERVICE run once per row of a join starts one per row.
    /// A caller that stops reading is noticed at the next hand-over, so up to the slice being
    /// evaluated and the [`SLICE_QUEUE`] queued ones are computed for nothing.
    fn sliced(
        &self,
        options: Options,
        needs: options::Needs,
        query: spargebra::Query,
    ) -> Result<QuerySolutionIter<'static>, FxError> {
        enum Message {
            Variables(Arc<[Variable]>),
            Rows(Vec<Result<QuerySolution, QueryEvaluationError>>),
            /// Sent last: without it, the thread stopped (a panic) and answers are missing.
            Done,
        }
        let (send, receive) = std::sync::mpsc::sync_channel::<Message>(SLICE_QUEUE);
        let fx = self.clone();
        std::thread::Builder::new()
            .name("fx-slice".into())
            .stack_size(SLICE_STACK)
            .spawn(move || {
                let mut announced = false;
                // Sends `solutions`; false when the receiver is gone (the caller stopped).
                let forward =
                    |solutions: QuerySolutionIter<'static>, announced: &mut bool| -> bool {
                        if !*announced {
                            *announced = true;
                            if send
                                .send(Message::Variables(solutions.variables().into()))
                                .is_err()
                            {
                                return false;
                            }
                        }
                        // A slice's answers go as one message: one hand-over per slice, not per row.
                        send.send(Message::Rows(solutions.collect())).is_ok()
                    };
                let mut stopped = false;
                let mut each = |quads: &[Quad]| -> Result<(), FxError> {
                    let solutions = fx.backend.evaluate(quads.to_vec(), query.clone(), &fx)?;
                    if !forward(solutions, &mut announced) {
                        stopped = true;
                        return Err(FxError::Option("stopped".into()));
                    }
                    Ok(())
                };
                let result = fx.triplify(&options, &needs, Some(&mut each));
                if stopped {
                    return;
                }
                let result = result.and_then(|quads| {
                    if announced {
                        return Ok(());
                    }
                    // Not sliced (a format without rows), or no rows: the whole graph.
                    let solutions = fx.backend.evaluate(quads, query.clone(), &fx)?;
                    forward(solutions, &mut announced);
                    Ok(())
                });
                if let Err(e) = result {
                    let _ = send.send(Message::Rows(vec![Err(QueryEvaluationError::Service(
                        Box::new(e),
                    ))]));
                }
                let _ = send.send(Message::Done);
            })
            .map_err(|e| FxError::Option(format!("slice: cannot start a thread: {e}")))?;
        match receive.recv() {
            Ok(Message::Variables(variables)) => {
                let mut done = false;
                let messages = std::iter::from_fn(move || {
                    if done {
                        return None;
                    }
                    match receive.recv() {
                        Ok(Message::Rows(rows)) => Some(rows),
                        Ok(Message::Done) => {
                            done = true;
                            None
                        }
                        Ok(Message::Variables(_)) => unreachable!("sent once, first"),
                        // The thread ended without saying it was done.
                        Err(_) => {
                            done = true;
                            Some(vec![Err(QueryEvaluationError::Service(Box::new(
                                FxError::Option("slice: the reading thread stopped".into()),
                            )))])
                        }
                    }
                });
                Ok(QuerySolutionIter::new(variables, messages.flatten()))
            }
            Ok(Message::Rows(mut rows)) => match rows.pop() {
                // Before the variables, only an error is sent.
                Some(Err(QueryEvaluationError::Service(e))) => match e.downcast::<FxError>() {
                    Ok(e) => Err(*e),
                    Err(e) => Err(FxError::Query(QueryEvaluationError::Service(e))),
                },
                Some(Err(e)) => Err(FxError::Query(e)),
                _ => unreachable!("rows come after the variables"),
            },
            Ok(Message::Done) => unreachable!("the variables or an error come first"),
            Err(_) => Err(FxError::Option("slice: the reading thread stopped".into())),
        }
    }

    /// The options every format shares, with the root: `root`, else the location's IRI, else
    /// one made from the content.
    fn common(
        &self,
        options: &Options,
        location: Option<(&str, &Path)>,
    ) -> Result<Common, FxError> {
        let root = match options.get("root") {
            Some(root) => root.to_owned(),
            None => match &location {
                Some((location, path)) => location_iri(location, path),
                None => format!(
                    "{XYZ}{}",
                    md5_hex(options.get("content").unwrap_or_default())?
                ),
            },
        };
        NamedNode::new(&root)
            .map_err(|e| FxError::Option(format!("root {root:?} is not an IRI: {e}")))?;
        Ok(Common {
            namespace: options.get("namespace").unwrap_or(XYZ).to_owned(),
            blank_nodes: options.flag("blank-nodes", true)?,
            root,
            null_string: options.get("null-string").map(str::to_owned),
            trim: options.flag("trim-strings", false)?,
            location: location.map(|(location, path)| location_iri(location, path)),
        })
    }

    /// The Facade-X graph of the source the options name, with the triples `needs` asks for.
    /// With `each`, a CSV source is given to it a slice of rows at a time (with the root's
    /// triples), and
    /// only the root's triples are returned; other formats ignore it.
    fn triplify(
        &self,
        options: &Options,
        needs: &options::Needs,
        each: Option<EachSlice<'_>>,
    ) -> Result<Vec<Quad>, FxError> {
        if database::is_database(options) {
            let (name, query) = database::query_of(options)?;
            let databases = self.config.databases.as_ref().ok_or_else(|| {
                FxError::Unsupported(
                    "table= and sql= sources: no database is configured (lift --database)".into(),
                )
            })?;
            let max_rows = database::max_rows(options)?;
            let mut options = options.clone();
            if options.get("root").is_none() {
                options
                    .0
                    .insert("root".into(), database::default_root(name, &query)?);
            }
            let common = self.common(&options, None)?;
            let mut sink = Sink::new(&common, needs);
            // Only the columns the pattern can match are read, unless `geo.*` names some.
            let all = options.0.keys().any(|k| k.starts_with("geo."));
            let wanted = |column: &str| all || sink.predicate(sink.key_iri(column)).is_some();
            let key = |column: &str| sink.key_iri(column);
            // Rows are numbered, and values may be trimmed or dropped: then every row is
            // read, so that the numbers and values stay the same.
            let narrow = common.blank_nodes && common.null_string.is_none() && !common.trim;
            let (conditions, one_of) = if narrow {
                (needs.conditions.as_slice(), needs.one_of.as_slice())
            } else {
                (&[][..], &[][..])
            };
            let select = database::Select {
                wanted: &wanted,
                key: &key,
                conditions,
                one_of,
                limit: needs.limit.filter(|_| narrow),
            };
            let started = std::time::Instant::now();
            let rows = databases.rows(name, &query, &select, max_rows)?;
            database::record(|| database::DatabaseCall {
                connection: name.map(str::to_owned),
                sql: rows.statement.clone().unwrap_or_else(|| match &query {
                    database::Query::Table(t) => format!("TABLE {t}"),
                    database::Query::Sql(q) => q.clone(),
                }),
                params: rows.params.clone(),
                rows: rows.rows.len(),
                millis: started.elapsed().as_secs_f64() * 1000.0,
            });
            needs.rows_read.set(rows.rows.len());
            database::triplify(rows, &options, &mut sink)?;
            return Ok(sink.quads);
        }
        if options.get("content").is_none()
            && options.get("from-archive").is_none()
            && let Some(location) = options.get("location")
            && !location.contains("://")
            && !location.starts_with(sources::UPLOAD)
        {
            let path = sources::resolve(&self.config, location)?;
            if path.is_dir() {
                let common = self.common(options, Some((location, &path)))?;
                let mut sink = Sink::new(&common, needs);
                archive::triplify_folder(&path, options, &mut sink)?;
                return Ok(sink.quads);
            }
        }
        let source = sources::read(&self.config, &self.uploads, options)?;
        let bytes = source.bytes();
        let location = source
            .location
            .as_ref()
            .map(|(l, p)| (l.as_str(), p.as_path()));
        // A URL without a known extension: the media type it was served with.
        let format = match (
            format(options, location.map(|(l, _)| l)),
            &source.media_type,
        ) {
            (Err(_), Some(media))
                if options.get("triplifier").is_none() && options.get("media-type").is_none() =>
            {
                format_of_media(media)?
            }
            (format, _) => format?,
        };
        let common = self.common(options, location)?;
        if let Format::Spreadsheet = format {
            return spreadsheet::triplify(bytes, options, &common, needs);
        }
        let mut sink = Sink::new(&common, needs);
        match format {
            Format::Csv { tab } => csv::triplify(bytes, options, &mut sink, tab, each)?,
            Format::Json => json::triplify(bytes, options, &mut sink)?,
            Format::Html => html::triplify(bytes, options, &mut sink)?,
            Format::Markdown => markdown::triplify(bytes, &mut sink)?,
            Format::Kml => features::kml(bytes, options, &mut sink)?,
            Format::Gpx => features::gpx(bytes, options, &mut sink)?,
            Format::Shapefile => match &source.location {
                // The reader opens the .shx and .dbf next to the .shp itself.
                Some((location, path))
                    if options.get("from-archive").is_none()
                        && !source.remote
                        && !location.starts_with(sources::UPLOAD) =>
                {
                    // The .shx and .dbf next to it are read under the same rules: a link
                    // there to a file outside the allowed directories is denied.
                    for ext in ["shx", "dbf"] {
                        let sibling = path.with_extension(ext);
                        if sibling.symlink_metadata().is_ok() {
                            sources::resolve(&self.config, &sibling.to_string_lossy())?;
                        }
                    }
                    features::shapefile(path, options, &mut sink)?
                }
                _ => {
                    return Err(FxError::Unsupported(
                        "a Shapefile from content, a URL or an archive (give its .shp file)".into(),
                    ));
                }
            },
            Format::Xml => xml::triplify(bytes, options, &mut sink)?,
            Format::Yaml => yaml::triplify(bytes, options, &mut sink)?,
            Format::Spreadsheet => unreachable!("triplified above"),
            Format::Archive(kind) => archive::triplify(bytes, kind, options, &mut sink)?,
            Format::Text => {
                let text =
                    std::str::from_utf8(bytes).map_err(|e| FxError::Parse(format!("text: {e}")))?;
                let root = sink.root();
                if let Some(o) = sink.string(text) {
                    sink.slot(&root, 1, o);
                }
            }
        }
        Ok(sink.quads)
    }
}

impl DefaultServiceHandler for FxService {
    type Error = FxError;

    fn handle(
        &self,
        service_name: &NamedNode,
        pattern: &GraphPattern,
        base_iri: Option<&Iri<String>>,
    ) -> Result<QuerySolutionIter<'static>, FxError> {
        if !service_name.as_str().starts_with(SCHEME) {
            return Err(FxError::Unsupported(format!(
                "SERVICE {service_name}: only {SCHEME} services can be called"
            )));
        }
        self.call(service_name.as_str(), pattern, base_iri)
    }
}

/// `evaluator` with the `fx:` functions.
pub fn register_functions(mut evaluator: SparqlEvaluator) -> SparqlEvaluator {
    for (name, f) in functions() {
        evaluator = evaluator.with_custom_function(name, move |args| f(args));
    }
    evaluator
}

enum Format {
    /// zip or tar: the names of the entries.
    Archive(archive::Kind),
    /// `tab`: tab-separated by default.
    Csv {
        tab: bool,
    },
    /// HTML, parsed as browsers do: elements as typed containers.
    Html,
    Json,
    /// CommonMark: blocks and inlines as typed containers.
    Markdown,
    /// Shapefile, KML, GPX: features with attributes and a geometry.
    Shapefile,
    Kml,
    Gpx,
    /// xlsx, xlsm, xlsb, xls, ods: one graph per sheet.
    Spreadsheet,
    Text,
    Xml,
    Yaml,
}

/// The format from `triplifier`, then `media-type`, then the location's extension; content
/// without a media type is text.
fn format(options: &Options, location: Option<&str>) -> Result<Format, FxError> {
    if let Some(t) = options.get("triplifier") {
        let t = t.to_ascii_lowercase();
        return if t.contains("csv") {
            Ok(Format::Csv {
                tab: csv::is_tab(options, location),
            })
        } else if t.contains("json") {
            Ok(Format::Json)
        } else if t.contains("html") {
            Ok(Format::Html)
        } else if t.contains("xml") {
            Ok(Format::Xml)
        } else if t.contains("yaml") {
            Ok(Format::Yaml)
        } else if t.contains("spreadsheet") || t.contains("xls") {
            Ok(Format::Spreadsheet)
        } else if t.contains("text") || t.contains("txt") {
            Ok(Format::Text)
        } else if t.contains("shapefile") || t.contains("shp") {
            Ok(Format::Shapefile)
        } else if t.contains("kml") {
            Ok(Format::Kml)
        } else if t.contains("gpx") {
            Ok(Format::Gpx)
        } else if t.contains("markdown") || t.contains("md") {
            Ok(Format::Markdown)
        } else if t.contains("zip") {
            Ok(Format::Archive(archive::Kind::Zip))
        } else if t.contains("tar") || t.contains("archive") {
            let kind = location
                .map(archive::Kind::from_name)
                .transpose()?
                .flatten()
                .unwrap_or(archive::Kind::Tar);
            Ok(Format::Archive(kind))
        } else {
            Err(FxError::Unsupported(format!("triplifier {t}")))
        };
    }
    if let Some(media) = options.get("media-type") {
        return format_of_media(media);
    }
    let Some(location) = location else {
        return Ok(Format::Text);
    };
    format_of_location(location)
}

/// The format of a media type.
fn format_of_media(media: &str) -> Result<Format, FxError> {
    {
        let media = media
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        match media.as_str() {
            "application/json" | "application/problem+json" | "application/geo+json" => {
                Ok(Format::Json)
            }
            "text/plain" => Ok(Format::Text),
            "text/html" => Ok(Format::Html),
            "text/markdown" | "text/x-markdown" => Ok(Format::Markdown),
            "application/vnd.google-earth.kml+xml" => Ok(Format::Kml),
            "application/gpx+xml" => Ok(Format::Gpx),
            "application/x-esri-shape" | "application/vnd.shp" => Ok(Format::Shapefile),
            "application/xml" | "text/xml" => Ok(Format::Xml),
            "text/csv" => Ok(Format::Csv { tab: false }),
            "text/tab-separated-values" => Ok(Format::Csv { tab: true }),
            "application/yaml" | "text/yaml" | "x-text/yaml" | "application/x-yaml" => {
                Ok(Format::Yaml)
            }
            "application/vnd.ms-excel"
            | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            | "application/vnd.ms-excel.sheet.macroenabled.12"
            | "application/vnd.ms-excel.sheet.binary.macroenabled.12"
            | "application/vnd.oasis.opendocument.spreadsheet" => Ok(Format::Spreadsheet),
            "application/zip" => Ok(Format::Archive(archive::Kind::Zip)),
            "application/x-tar" | "application/x-gtar" => Ok(Format::Archive(archive::Kind::Tar)),
            "application/x-compressed-tar" => Ok(Format::Archive(archive::Kind::TarGz)),
            "application/x-bzip-compressed-tar" => Ok(Format::Archive(archive::Kind::TarBz2)),
            _ => Err(FxError::Unsupported(format!("media type {media}"))),
        }
    }
}

/// The format of a location, by its extension.
fn format_of_location(location: &str) -> Result<Format, FxError> {
    let path = location.split(['?', '#']).next().unwrap_or(location);
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    if matches!(
        ext.as_deref(),
        Some("zip" | "tar" | "gz" | "tgz" | "bz2" | "tbz2")
    ) && let Some(kind) = archive::Kind::from_name(name)?
    {
        return Ok(Format::Archive(kind));
    }
    match ext.as_deref() {
        Some("json" | "geojson") => Ok(Format::Json),
        Some("txt") => Ok(Format::Text),
        Some("html" | "htm") => Ok(Format::Html),
        Some("md" | "markdown") => Ok(Format::Markdown),
        Some("shp") => Ok(Format::Shapefile),
        Some("kml") => Ok(Format::Kml),
        Some("gpx") => Ok(Format::Gpx),
        Some("xml") => Ok(Format::Xml),
        Some("csv") => Ok(Format::Csv { tab: false }),
        Some("tsv" | "tab") => Ok(Format::Csv { tab: true }),
        Some("yaml" | "yml") => Ok(Format::Yaml),
        Some("xls" | "xlsx" | "xlsm" | "xlsb" | "ods") => Ok(Format::Spreadsheet),
        _ => Err(FxError::Unsupported(format!(
            "cannot tell the format of {location}: set media-type"
        ))),
    }
}

/// The root IRI for a location: the location if it is an absolute IRI, else the file's
/// `file:` URL.
fn location_iri(location: &str, path: &Path) -> String {
    if location.contains(':')
        && NamedNode::new(location).is_ok()
        && !looks_like_windows_path(location)
    {
        return location.to_owned();
    }
    let absolute = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let text = absolute.to_string_lossy().replace('\\', "/");
    let mut iri = String::from(if text.starts_with('/') {
        "file://"
    } else {
        "file:///"
    });
    tree::escape_iri(&text, &mut iri);
    iri
}

fn looks_like_windows_path(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// The MD5 of `s` in hex, through SPARQL's `MD5` (no hashing dependency of our own).
fn md5_hex(s: &str) -> Result<String, FxError> {
    let literal = oxigraph::model::Literal::new_simple_literal(s);
    let query = SparqlEvaluator::new()
        .parse_query(&format!("SELECT (MD5({literal}) AS ?h) {{}}"))
        .map_err(|e| FxError::Option(e.to_string()))?;
    let empty = oxigraph::model::Dataset::new();
    let QueryResults::Solutions(mut solutions) = query.on_queryable_dataset(&empty).execute()?
    else {
        unreachable!("a SELECT gives solutions")
    };
    match solutions
        .next()
        .transpose()?
        .and_then(|s| s.get("h").cloned())
    {
        Some(Term::Literal(l)) => Ok(l.value().to_owned()),
        _ => Err(FxError::Option("MD5 failed".into())),
    }
}

/// Whether a database source's rows map one to one to what the pattern reads, so that it may
/// read fewer: a pattern about one row (no `VALUES`), not sliced, rows not numbered, values
/// as stored.
fn narrow_rows(call: &options::Call) -> bool {
    let o = &call.options;
    database::is_database(o)
        && !o.flag("slice", false).unwrap_or(true)
        && o.flag("blank-nodes", true).unwrap_or(false)
        && o.get("null-string").is_none()
        && !o.flag("trim-strings", false).unwrap_or(true)
        && !call.needs.all
        && !call.needs.slots
        && database::per_row(&call.pattern)
}
