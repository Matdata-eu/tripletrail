//! An HTTP server for tripletrail engines: the
//! [SPARQL 1.1 Protocol](https://www.w3.org/TR/sparql11-protocol/) and the
//! [Graph Store Protocol](https://www.w3.org/TR/sparql11-http-rdf-update/), with Fuseki's
//! URL layout so clients written for Fuseki work unchanged.
//!
//! Every dataset `{ds}` has:
//!
//! - `/{ds}/query` (`GET ?query=`, `POST` form or `application/sparql-query`),
//! - `/{ds}/update` (`POST` form or `application/sparql-update`),
//! - `/{ds}/sparql` and `/{ds}`: queries and updates,
//! - `/{ds}/data` (`GET`/`HEAD`/`PUT`/`POST`/`DELETE`, `?graph=<iri>` or `?default`; without
//!   either, the whole dataset), and `/{ds}/get` (read only),
//! - `/{ds}/explain`: evaluates a query and returns its plan with row counts and timings,
//! - `/{ds}/describe?iri=<iri>`: the description of a resource, with content negotiation.
//!
//! With [`Options::base_iris`], the datasets' own IRIs dereference too (Linked Data, see
//! `linked_data.rs`).
//!
//! With one dataset, the same paths without `/{ds}` work too. `/$/ping` and `/$/datasets` are
//! for health checks, and `/$/status` says whether each dataset's spatial index is built
//! (`"ready": true` once all are), for clients that want to wait for it.

mod access;
mod access_history;
mod access_review;
mod admin;
mod archive;
mod area;
#[cfg(feature = "enterprise")]
mod audit;
pub mod auth;
mod binary_rdf;
pub mod branding;
pub mod config;
#[cfg(feature = "postgres")]
pub mod database_sources;
mod dcat;
mod describe_view;
pub mod fuseki_config;
mod geometry_ways;
mod id_results;
mod import;
#[doc(hidden)]
pub mod jena_binary;
#[doc(hidden)]
pub mod jena_rules;
mod json_results;
pub mod lift;
mod lift_out;
mod limits;
mod linked_data;
mod mcp;
mod metrics;
mod ogc;
#[cfg(feature = "otel")]
pub mod otel;
pub mod password;
pub mod pipeline;
pub mod pipeline_export;
mod policy;
mod policy_edit;
mod profile;
mod rdf4j;
#[doc(hidden)]
pub mod rdf_formats;
mod rdf_patch;
mod reason;
#[cfg(feature = "replication")]
mod replication;
mod rest;
mod row_policies;
mod service_description;
mod shapes;
#[cfg(feature = "git")]
mod shapes_git;
#[cfg(all(feature = "enterprise", feature = "git"))]
mod shapes_review;
mod similar;
#[cfg(feature = "enterprise")]
mod sso;
pub mod testing;
mod threads;
mod tiles;
#[cfg(feature = "tls")]
mod tls;
mod ui;
mod validators;
mod void;
mod workspace;
mod workspace_history;
mod workspace_pipeline;

use admin::{Entry, Operation, Registry, Tasks};
pub use admin::{Mode, open_dataset};
#[cfg(feature = "replication")]
pub use replication::{Follow, follow, promoted_replicas, serve_promoted};
pub use shapes::{folder_files, pack_check};
use tripletrail::geo::sync::RwLockUnpoisoned;
use tripletrail::oxigraph::io::{RdfFormat, RdfParser};
use tripletrail::oxigraph::model::{GraphName, GraphNameRef, NamedNode, NamedOrBlankNode};
use tripletrail::oxigraph::sparql::results::{QueryResultsFormat, QueryResultsSerializer};
use tripletrail::oxigraph::sparql::{CancellationToken, QueryResults};
use tripletrail::spatial::IndexedPredicates;
use tripletrail::{Engine, ReadableGraphs};

/// Checks a `--catalog` file (see [`Options::catalog`]): an error says where its Turtle is
/// wrong.
pub fn check_catalog(text: &str) -> Result<(), String> {
    dcat::parse(text, "http://localhost/").map(|_| ())
}
use std::cell::RefCell;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use threads::{Deadline, run_deep, spawn_deep};
use tiny_http::{Header, Method, Request, Response, StatusCode};
pub use workspace::WorkspaceOptions;

/// Where an inferred triple came from, as JSON in the one format of the workbench's "why?"
/// panel (#401): `{"kind": "rule", "rule", "shape", "focus", "group", "pass", "bindings"}`.
pub fn provenance_json(origin: &tripletrail::shacl::Origin) -> serde_json::Value {
    reason::origin(origin)
}

/// A base map the Map page offers besides the server's own (`--map-basemap`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Basemap {
    /// The title in the Map page's menu.
    pub title: String,
    /// An XYZ raster tile URL template.
    pub tiles: String,
    /// Its credit (HTML).
    pub attribution: String,
    /// The last zoom the provider has tiles for: the map shows those tiles enlarged beyond it.
    pub maxzoom: u8,
}

impl Basemap {
    /// The zoom a base map's tiles go up to when nothing says otherwise.
    pub const DEFAULT_MAXZOOM: u8 = 19;
}

/// A running server's settings.
#[derive(Clone, Debug)]
pub struct Options {
    /// Longest a query or update may run; `None` for no limit. A request may ask for less
    /// with a `timeout` parameter in seconds, never for more.
    pub timeout: Option<Duration>,
    /// Worker threads answering requests.
    pub threads: usize,
    /// Adds `Access-Control-Allow-Origin: *` and answers CORS preflight requests.
    pub cors: bool,
    /// Without [`Options::cors`]: the origins (`https://example.com`) whose cross-origin
    /// requests are allowed; others get no CORS headers.
    pub cors_origins: Vec<String>,
    /// When set, writes (updates, and `POST`/`PUT`/`DELETE` on `/{ds}/data`) need
    /// `Authorization: Bearer <token>` (this token or the admin token). Queries stay open.
    pub update_token: Option<String>,
    /// Largest query, update or form body accepted, in bytes; `None` for no limit. Larger
    /// bodies are answered with 413.
    pub max_body: Option<usize>,
    /// Largest Graph Store upload (`PUT`/`POST` on `/data`) accepted, in bytes; `None` for
    /// no limit. An upload is held in memory while it is checked, so this bounds that too.
    pub max_upload: Option<usize>,
    /// Most bytes one pull from a git repository (shapes, #137, or a workspace, #400) may
    /// add to its copy on disk, and may read from the commit (#419); `None` for no limit.
    pub git_fetch: Option<u64>,
    /// When set, admin requests that change something (`POST`, `PUT`, `DELETE` under `/$/`)
    /// need `Authorization: Bearer <token>`.
    pub admin_token: Option<String>,
    /// Where datasets created over the admin API with `type=disk` go (`<location>/<name>`).
    pub location: Option<PathBuf>,
    /// The configuration file the server was started with (`serve --config`), for `/$/server`.
    pub config_file: Option<PathBuf>,
    /// Where `POST /$/backup/{name}` writes, and `from=` restores from.
    pub backup_dir: Option<PathBuf>,
    /// The backups of each dataset kept in `backup_dir`: after a backup succeeds, older ones
    /// of the same dataset beyond this many are deleted. `None` keeps them all.
    pub backup_keep: Option<usize>,
    /// Where the write-ahead logs of on-disk datasets are archived when a checkpoint retires
    /// them, in a directory per dataset: with store backups, they restore a dataset to any
    /// point in time (#222).
    pub wal_archive: Option<PathBuf>,
    /// How many retired write-ahead logs each on-disk dataset keeps in its directory, for
    /// read replicas to read to their end (#217); with fewer, a replica that falls behind
    /// a checkpoint copies the whole dataset again.
    pub keep_logs: u64,
    /// How many vectors of one length `tt:nearest` compares one by one before it builds a
    /// graph of them and searches that, approximately (#236); 0 keeps every search exact.
    pub vector_graph_min: usize,
    /// How many read replicas must hold a write before it answers (#217): 0 (the default)
    /// answers once the write is on the leader's disk.
    pub sync_replicas: usize,
    /// How long a write waits for `sync_replicas` replicas; it then answers anyway, with
    /// `TripleTrail-Replicas` saying how many hold it.
    pub sync_timeout: std::time::Duration,
    /// User-defined functions (`--functions`) every dataset's queries can call (#235).
    pub functions: tripletrail::udf::Functions,
    /// The predicates whose geometries datasets attached at runtime index.
    pub indexed: IndexedPredicates,
    /// `--text-properties` (#188): the properties full-text search indexes in a dataset
    /// besides the ones it always indexes, by dataset name (`*` for every dataset). Given to
    /// every dataset when it is served, those created or attached at run time too.
    pub text_properties: Vec<(String, tripletrail::oxigraph::model::NamedNode)>,
    /// Serve the web UI at `/$/ui/` (and redirect `/` to it).
    pub ui: bool,
    /// Serve a Model Context Protocol endpoint at `/{ds}/mcp` (#153), for AI agents.
    pub mcp: bool,
    /// Load MatGUI from this URL prefix (e.g. a CDN) instead of the copy built into the
    /// binary; the prefix must hold `yasgui.min.js` and `yasgui.min.css`.
    pub matgui_url: Option<String>,
    /// The XYZ raster tile URL template under the describe pages' interactive map, and its
    /// attribution; without it the map has no base map and fetches nothing from elsewhere.
    pub map_tiles: Option<String>,
    pub map_attribution: Option<String>,
    /// More base maps the Map page offers besides `map_tiles` (`--map-basemap`).
    pub map_basemaps: Vec<Basemap>,
    /// The name, favicon, logo and colour of the pages (`--site-*`, #262).
    pub branding: branding::Branding,
    /// Write one JSON line per request to standard output.
    pub log_requests: bool,
    /// On SIGTERM or Ctrl-C, how long running requests may take before the server exits
    /// (see [`Server::run_until_signal`]).
    pub shutdown_grace: Duration,
    /// Linked Data: `(prefix, dataset)` pairs. A request whose URL starts with `prefix` and
    /// is none of the server's own paths is answered with the description of the resource
    /// with that IRI, from `dataset`.
    pub base_iris: Vec<(String, String)>,
    /// The DCAT-AP catalog at `/.well-known/dcat`: a Turtle file with what the server
    /// cannot know (titles, publisher, licence, ...), its relative IRIs resolved against the
    /// server's URL (`<#catalog>`, `<{dataset}/void#dataset>`). See [`dcat`].
    pub catalog: Option<String>,
    /// Linked Data: answer a request for a resource itself with `303 See Other` to its
    /// document (`{iri}.ttl`, ...) instead of with the document.
    pub see_other: bool,
    /// Write a JSON line with the full text of each query or update that takes at least this
    /// long (body included) to standard error. Off when `None`: query text can hold data.
    pub slow_query: Option<Duration>,
    /// OpenTelemetry traces (#220): sent when an endpoint is set.
    #[cfg(feature = "otel")]
    pub otlp: otel::OtlpOptions,
    /// Most rows (solutions, or triples of a CONSTRUCT / DESCRIBE) one query may answer;
    /// `None` for no limit. A request may ask for less with `max-rows`, never for more. A
    /// larger result is refused with 413, or cut with `partial=true`.
    pub max_rows: Option<usize>,
    /// Who may call (#199): a sparql-guard configuration (or its `auth` object) with API
    /// keys, trusted proxy headers and anonymous access, reloaded when the file changes.
    /// When set, every request but `/$/ping` and `/$/ready` needs an identity; writes need
    /// the group `tripletrail:writer` or `tripletrail:admin`, admin changes `tripletrail:admin`.
    /// [`Options::update_token`] and [`Options::admin_token`] become keys of those groups.
    pub auth_config: Option<PathBuf>,
    /// Per-dataset limits (#225): a JSON file with `default` and per-dataset `datasets`
    /// entries of `maxConcurrent`, `maxQueued`, `queueTimeout`, `timeout`, `maxTimeout`,
    /// `maxRows` and `maxUpload`, read again when it changes. What it leaves out comes from
    /// the options above.
    pub limits: Option<PathBuf>,
    /// Serve HTTPS with this certificate chain and private key (PEM), read again when the
    /// files change or on SIGHUP. Both or neither.
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    /// `SERVICE <https://…>` to remote SPARQL endpoints (#192): the endpoints allowed (IRI
    /// prefixes; none by default, so remote calls fail), the time and size limits of a call,
    /// and how long answers are reused.
    #[cfg(feature = "remote-service")]
    pub remote_services: tripletrail::remote::RemoteConfig,
    /// PostgreSQL tables queries may read live through Facade-X
    /// (`SERVICE <x-sparql-anything:table=…,database=<name>>`, #216): none by default.
    #[cfg(feature = "postgres")]
    pub database_sources: database_sources::DatabaseSources,
    /// The workbench's workspaces (#400) under `/$/workspaces`: where they are kept, how long
    /// a scratch workspace lives, and their caps.
    pub workspaces: WorkspaceOptions,
    /// The TripleTrail Enterprise licence, as checked at start (#734). Its standing is worked
    /// out again on each use, as days pass.
    #[cfg(feature = "enterprise")]
    pub license: Result<
        tripletrail_enterprise::license::License,
        tripletrail_enterprise::license::LicenseError,
    >,
    /// The audit log (#207, TripleTrail Enterprise): a JSON line for each change and admin call,
    /// hash-chained.
    #[cfg(feature = "enterprise")]
    pub audit_log: Option<PathBuf>,
    /// The audit log also has a line for each query and other read of a dataset.
    #[cfg(feature = "enterprise")]
    pub audit_reads: bool,
    /// Where audit lines also go: `udp://HOST:PORT` or `tcp://HOST:PORT`, a syslog collector.
    #[cfg(feature = "enterprise")]
    pub audit_syslog: Option<String>,
}

/// The edition of this build: `open` (Apache-2.0) or `enterprise` (#734).
pub const EDITION: &str = {
    #[cfg(feature = "enterprise")]
    {
        tripletrail_enterprise::EDITION
    }
    #[cfg(not(feature = "enterprise"))]
    {
        "open"
    }
};

impl Options {
    /// The enterprise licence's standing today.
    #[cfg(feature = "enterprise")]
    pub fn license_status(&self) -> tripletrail_enterprise::license::Status {
        tripletrail_enterprise::license::Status::of(
            &self.license,
            tripletrail_enterprise::license::Date::today(),
        )
    }
}

impl Options {
    /// The `--text-properties` of the dataset `name`: its own and those given for `*`.
    pub fn text_properties_of(&self, name: &str) -> Vec<tripletrail::oxigraph::model::NamedNode> {
        let mut out: Vec<tripletrail::oxigraph::model::NamedNode> = Vec::new();
        for (n, p) in &self.text_properties {
            if (n == "*" || n == name) && !out.contains(p) {
                out.push(p.clone());
            }
        }
        out
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            timeout: None,
            threads: 4,
            config_file: None,
            cors: false,
            cors_origins: Vec::new(),
            update_token: None,
            max_body: Some(16 << 20),
            max_upload: None,
            git_fetch: Some(256 << 20),
            admin_token: None,
            location: None,
            backup_dir: None,
            backup_keep: None,
            wal_archive: None,
            keep_logs: 1,
            vector_graph_min: tripletrail::vector::GRAPH_MIN,
            sync_replicas: 0,
            sync_timeout: std::time::Duration::from_secs(10),
            functions: Default::default(),
            indexed: IndexedPredicates::default(),
            text_properties: Vec::new(),
            ui: true,
            mcp: false,
            matgui_url: None,
            map_tiles: None,
            map_attribution: None,
            map_basemaps: Vec::new(),
            branding: branding::Branding::default(),
            log_requests: false,
            shutdown_grace: Duration::from_secs(30),
            base_iris: Vec::new(),
            see_other: false,
            catalog: None,
            slow_query: None,
            #[cfg(feature = "otel")]
            otlp: otel::OtlpOptions::default(),
            max_rows: None,
            auth_config: None,
            limits: None,
            tls_cert: None,
            tls_key: None,
            #[cfg(feature = "remote-service")]
            remote_services: Default::default(),
            #[cfg(feature = "postgres")]
            database_sources: Default::default(),
            workspaces: WorkspaceOptions::default(),
            #[cfg(feature = "enterprise")]
            license: Err(tripletrail_enterprise::license::LicenseError::Missing),
            #[cfg(feature = "enterprise")]
            audit_log: None,
            #[cfg(feature = "enterprise")]
            audit_reads: false,
            #[cfg(feature = "enterprise")]
            audit_syslog: None,
        }
    }
}

/// An HTTP server over named engines.
pub struct Server {
    http: Arc<tiny_http::Server>,
    /// The public address, when it is not the HTTP server's (with TLS).
    public: Option<SocketAddr>,
    state: Arc<State>,
    threads: usize,
}

pub(crate) struct State {
    /// The audit log (#207), when one is kept.
    #[cfg(feature = "enterprise")]
    pub(crate) audit: Option<tripletrail_enterprise::audit::Log>,
    /// Where finished spans go (#220), when traces are on.
    #[cfg(feature = "otel")]
    pub(crate) tracer: Option<Arc<otel::Tracer>>,
    pub(crate) registry: RwLock<Registry>,
    pub(crate) options: Options,
    pub(crate) tasks: Tasks,
    /// Datasets are still being opened (see [`Handle::set_starting`]).
    pub(crate) starting: AtomicBool,
    /// A shutdown has begun: no new requests are taken.
    pub(crate) stopping: AtomicBool,
    /// A shutdown was asked for, by a signal or [`Handle::stop`].
    stop_requested: AtomicBool,
    /// Requests being answered, their bodies included.
    pub(crate) in_flight: AtomicUsize,
    pub(crate) started: SystemTime,
    pub(crate) auth: Option<auth::Auth>,
    pub(crate) limits: Option<limits::LimitsFile>,
    #[cfg(feature = "tls")]
    pub(crate) tls: Option<Arc<tls::Tls>>,
    pub(crate) callers: Arc<limits::CallerGate>,
    pub(crate) rdf4j: rdf4j::Rdf4j,
    pub(crate) workspaces: workspace::Workspaces,
    /// Facade-X with the database sources, shared by every dataset (one connection each).
    #[cfg(feature = "postgres")]
    pub(crate) fx: Option<tripletrail::fx::FxConfig>,
    /// The datasets this server follows from a leader, and how they are doing (#217).
    #[cfg(feature = "replication")]
    pub(crate) replicas:
        std::sync::Mutex<std::collections::BTreeMap<String, Arc<replication::Status>>>,
    /// Requests waiting for a `TripleTrail-Min-Version` on any replica, each holding a worker.
    #[cfg(feature = "replication")]
    pub(crate) replica_waits: AtomicUsize,
    /// Where the followers of this server's datasets stand (#217).
    #[cfg(feature = "replication")]
    pub(crate) acks: replication::Acks,
    /// Writes waiting for --sync-replicas on threads of their own.
    #[cfg(feature = "replication")]
    pub(crate) sync_waiters: AtomicUsize,
}

impl State {
    /// `engine` with the server's settings that belong to the engine: the remote SERVICE
    /// endpoints it may call.
    pub(crate) fn prepare(&self, name: &str, engine: Engine) -> Engine {
        engine.set_text_properties(&self.options.text_properties_of(name));
        engine.set_vector_graph_min(self.options.vector_graph_min);
        if let tripletrail::store::Dataset::TripleTrail(store) = engine.store() {
            store.set_log_retention(self.options.keep_logs);
        }
        if let Some(archive) = &self.options.wal_archive
            && let tripletrail::store::Dataset::TripleTrail(store) = engine.store()
            && store.is_persistent()
            && let Err(e) = store.set_wal_archive(Some(archive.join(name)))
        {
            eprintln!("archiving the write-ahead log of {name}: {e}");
        }
        let engine = if self.options.functions.is_empty() {
            engine
        } else {
            engine.with_functions(self.options.functions.clone())
        };
        #[cfg(feature = "postgres")]
        let engine = match &self.fx {
            Some(config) => engine
                .with_fx(config.clone())
                .with_fx_graphs(self.options.database_sources.graphs.clone()),
            None => engine,
        };
        #[cfg(feature = "remote-service")]
        if !self.options.remote_services.allow.is_empty() {
            return engine.with_remote_services(self.options.remote_services.clone());
        }
        engine
    }
}

/// Adds datasets to a running server and reports its start-up, for a server that opens its
/// datasets after it starts listening (so health checks answer meanwhile).
#[derive(Clone)]
pub struct Handle {
    state: Arc<State>,
}

impl Handle {
    /// Installs the shape pack in the folder `dir` on the dataset `name` (#148, `serve
    /// --pack`), unless that release is installed already: see [`shapes::install_folder`].
    /// Answers what was done.
    pub fn install_pack(&self, name: &str, dir: &Path) -> Result<String, String> {
        let entry = self
            .state
            .registry
            .read_unpoisoned()
            .datasets
            .get(name)
            .cloned()
            .ok_or_else(|| format!("no dataset {name}"))?;
        shapes::install_folder(&self.state, &entry, dir)
    }

    /// Builds the string ranks of the dataset `name` (the order of its IRIs and strings, which
    /// `MIN`, `MAX` and `ORDER BY` read) ahead of the first query that needs them (`serve`,
    /// unless `--no-warm-up`), for a dictionary of at most 16 million terms (128 MB). Answers
    /// what was done, `None` when nothing was.
    pub fn warm_ranks(&self, name: &str) -> Option<String> {
        const MAX_TERMS: usize = 1 << 24;
        let entry = self
            .state
            .registry
            .read_unpoisoned()
            .datasets
            .get(name)
            .cloned()?;
        let tripletrail::store::Dataset::TripleTrail(store) = entry.engine.store() else {
            return None;
        };
        let started = std::time::Instant::now();
        let terms = store.snapshot().warm_string_ranks(MAX_TERMS)?;
        Some(format!(
            "string ranks of {terms} terms in {:.2} s",
            started.elapsed().as_secs_f64()
        ))
    }

    /// Makes the map of the dataset `name` ahead of its first visitor (#92, `serve`, unless
    /// `--no-warm-up`): its collections and their unfiltered facet counts, kept in the caches
    /// requests read, each facet query under the dataset's timeout and the whole in at most
    /// five minutes. Answers what was done.
    pub fn warm_map(&self, name: &str) -> Result<String, String> {
        self.warm_map_within(name, tiles::WARM_UP_LIMIT)
    }

    /// [`Handle::warm_map`] in at most `within`.
    pub fn warm_map_within(&self, name: &str, within: Duration) -> Result<String, String> {
        let entry = self
            .state
            .registry
            .read_unpoisoned()
            .datasets
            .get(name)
            .cloned()
            .ok_or_else(|| format!("no dataset {name}"))?;
        let started = std::time::Instant::now();
        let warmed = tiles::warm(&self.state, &entry, within).map_err(|e| e.1)?;
        let mut done = format!(
            "map ready: {} collections and their facets in {:.1} s",
            warmed.collections,
            started.elapsed().as_secs_f64()
        );
        if !warmed.left.is_empty() {
            done = format!(
                "map made ahead in part: the facets of {} of {} collections in {:.1} s; \
                 out of time ({} s) for {}, which the first request makes",
                warmed.counted,
                warmed.collections,
                started.elapsed().as_secs_f64(),
                within.as_secs(),
                warmed.left.join(", ")
            );
        }
        if !warmed.failed.is_empty() {
            done.push_str(&format!(
                "; facets that failed: {}",
                warmed.failed.join("; ")
            ));
        }
        Ok(done)
    }

    /// Warnings about the names from before the rename to TripleTrail that the dataset `name`
    /// still uses (#955): in the shape files of its shapes folder, and as the names of its
    /// graphs. Each names the file or graph and how to carry it over.
    pub fn old_names(&self, name: &str) -> Vec<String> {
        let entry = self
            .state
            .registry
            .read_unpoisoned()
            .datasets
            .get(name)
            .cloned();
        entry.map_or_else(Vec::new, |e| shapes::old_names(&self.state, &e))
    }

    /// Serves `engine` as `name`. `location` is its directory, if it has one (shown by
    /// `/$/datasets`, and removed by a detach with `delete-files=true`).
    pub fn attach(&self, name: &str, engine: Engine, location: Option<PathBuf>) {
        let mode = if engine.is_read_only() {
            Mode::ReadOnly
        } else {
            Mode::ReadWrite
        };
        self.attach_as(name, engine, location, mode);
    }

    /// [`Handle::attach`], served as `mode`: [`Mode::ReadOnly`] refuses writes from clients
    /// even if the engine takes them (a read replica's, which only its leader's log writes).
    pub fn attach_as(&self, name: &str, engine: Engine, location: Option<PathBuf>, mode: Mode) {
        self.state.registry.write_unpoisoned().datasets.insert(
            name.to_owned(),
            Arc::new(Entry::new(
                name.to_owned(),
                self.state.prepare(name, engine),
                location,
                mode,
            )),
        );
    }

    /// Points the alias `alias` at the dataset `target`.
    pub fn alias(&self, alias: &str, target: &str) {
        self.state.registry.write_unpoisoned().aliases.insert(
            alias.to_owned(),
            admin::Alias {
                target: target.to_owned(),
                since: SystemTime::now(),
                previous: None,
            },
        );
    }

    /// While `true`, `/$/ready` answers 503.
    pub fn set_starting(&self, starting: bool) {
        self.state.starting.store(starting, Ordering::Relaxed);
    }

    /// Asks a server in [`Server::run_until_stopped`] or [`Server::run_until_signal`] to shut
    /// down gracefully.
    pub fn stop(&self) {
        self.state.stop_requested.store(true, Ordering::Relaxed);
    }
}

impl Server {
    /// Binds `address` (e.g. `127.0.0.1:7878`, port 0 for any) to serve `datasets` by name.
    pub fn bind(
        address: &str,
        datasets: impl IntoIterator<Item = (String, Engine)>,
        options: Options,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // Row-level policies are enforced in the enterprise build whatever the licence says
        // (#818): before the policies are read, so that files with row rules load.
        #[cfg(feature = "enterprise")]
        tripletrail_enterprise::rows::install();
        let listener = std::net::TcpListener::bind(address)?;
        set_nodelay(&listener);
        #[cfg(feature = "tls")]
        let mut tls_listener = None;
        let (http_listener, public) = match (&options.tls_cert, &options.tls_key) {
            (None, None) => (listener, None),
            (Some(_), None) | (None, Some(_)) => {
                return Err("--tls-cert and --tls-key go together".into());
            }
            #[cfg(not(feature = "tls"))]
            (Some(_), Some(_)) => {
                return Err("this build has no TLS: build with the feature tls".into());
            }
            #[cfg(feature = "tls")]
            (Some(_), Some(_)) => {
                // TLS on the public address, relayed to the HTTP server on a loopback port.
                let public = listener.local_addr()?;
                let inner = std::net::TcpListener::bind("127.0.0.1:0")?;
                set_nodelay(&inner);
                tls_listener = Some(listener);
                (inner, Some(public))
            }
        };
        let http = tiny_http::Server::from_listener(http_listener, None)?;
        let threads = options.threads.max(1);
        let auth = options
            .auth_config
            .as_deref()
            .map(|path| {
                auth::Auth::load(
                    path,
                    options.update_token.as_deref(),
                    options.admin_token.as_deref(),
                )
            })
            .transpose()?;
        let limits = options
            .limits
            .as_deref()
            .map(limits::LimitsFile::load)
            .transpose()?;
        #[cfg(feature = "tls")]
        let tls = match (&options.tls_cert, &options.tls_key, tls_listener) {
            (Some(cert), Some(key), Some(listener)) => {
                let tls = Arc::new(tls::Tls::load(cert, key)?);
                let inner = http
                    .server_addr()
                    .to_ip()
                    .expect("listening on an IP address");
                tls.serve(listener, inner);
                Some(tls)
            }
            _ => None,
        };
        let workspaces = workspace::Workspaces::open(&options)?;
        #[cfg(feature = "enterprise")]
        let audit = match &options.audit_log {
            Some(path) => {
                let ship = options
                    .audit_syslog
                    .as_deref()
                    .map(tripletrail_enterprise::syslog::Shipper::start)
                    .transpose()?;
                Some(tripletrail_enterprise::audit::Log::open_shipping(
                    path, ship,
                )?)
            }
            None => None,
        };
        #[cfg(feature = "otel")]
        let tracer = options
            .otlp
            .endpoint
            .is_some()
            .then(|| otel::Tracer::start(options.otlp.clone()))
            .transpose()?;
        #[cfg(feature = "postgres")]
        let fx =
            (!options.database_sources.is_empty()).then(|| options.database_sources.fx_config());
        let server = Self {
            http: Arc::new(http),
            public,
            state: Arc::new(State {
                #[cfg(feature = "enterprise")]
                audit,
                #[cfg(feature = "otel")]
                tracer,
                registry: RwLock::default(),
                options,
                tasks: Tasks::default(),
                starting: AtomicBool::new(false),
                stopping: AtomicBool::new(false),
                stop_requested: AtomicBool::new(false),
                in_flight: AtomicUsize::new(0),
                started: SystemTime::now(),
                auth,
                limits,
                #[cfg(feature = "tls")]
                tls,
                callers: Arc::default(),
                rdf4j: rdf4j::Rdf4j::default(),
                workspaces,
                #[cfg(feature = "postgres")]
                fx,
                #[cfg(feature = "replication")]
                replicas: std::sync::Mutex::default(),
                #[cfg(feature = "replication")]
                replica_waits: AtomicUsize::new(0),
                #[cfg(feature = "replication")]
                acks: replication::Acks::default(),
                #[cfg(feature = "replication")]
                sync_waiters: AtomicUsize::new(0),
            }),
            threads,
        };
        #[cfg(feature = "git")]
        shapes_git::watch(Arc::downgrade(&server.state));
        let handle = server.handle();
        for (name, engine) in datasets {
            handle.attach(&name, engine, None);
        }
        Ok(server)
    }

    /// A handle to add datasets while the server runs.
    pub fn handle(&self) -> Handle {
        Handle {
            state: Arc::clone(&self.state),
        }
    }

    /// The server's settings.
    pub fn options(&self) -> &Options {
        &self.state.options
    }

    /// Whether users sign in with passwords (HTTP Basic, #825).
    pub fn has_password_users(&self) -> bool {
        self.state.auth.as_ref().is_some_and(auth::Auth::has_users)
    }

    /// Why row-level policies in use cannot be changed now (#818): `None` when no policy has
    /// row rules, or they can be changed.
    pub fn row_policies_warning(&self) -> Option<String> {
        let in_use = self
            .state
            .auth
            .as_ref()
            .is_some_and(auth::Auth::has_row_rules);
        in_use
            .then(|| row_policies::editable(&self.state.options).err())
            .flatten()
    }

    /// The address the server listens on.
    pub fn address(&self) -> SocketAddr {
        self.public.unwrap_or_else(|| {
            self.http
                .server_addr()
                .to_ip()
                .expect("listening on an IP address")
        })
    }

    fn workers(&self) -> Vec<thread::JoinHandle<()>> {
        (0..self.threads)
            .map(|_| {
                let http = Arc::clone(&self.http);
                let state = Arc::clone(&self.state);
                spawn_deep(move || {
                    while !state.stopping.load(Ordering::Relaxed) {
                        match http.recv_timeout(Duration::from_millis(250)) {
                            Ok(Some(request)) => {
                                // `handle` answers a panic itself; this keeps the worker
                                // alive should sending the answer panic too.
                                let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                                    handle(&state, request)
                                }));
                            }
                            Ok(None) => {}
                            Err(_) => break,
                        }
                    }
                })
            })
            .collect()
    }

    /// Answers requests until the process ends.
    pub fn run(self) {
        for w in self.workers() {
            let _ = w.join();
        }
    }

    /// Answers requests on background threads.
    pub fn spawn(self) {
        thread::spawn(move || self.run());
    }

    /// Answers requests until SIGTERM (or Ctrl-C), then shuts down gracefully: `/$/ready`
    /// answers 503 and no new request is taken, running requests get up to
    /// [`Options::shutdown_grace`] to finish, and read-write datasets on disk are
    /// checkpointed. Returns when that is done.
    pub fn run_until_signal(self) {
        let handle = self.handle();
        if let Err(e) = ctrlc::set_handler(move || handle.stop()) {
            eprintln!("no signal handler ({e}); the server stops only when killed");
        }
        self.run_until_stopped();
    }

    /// Answers requests until [`Handle::stop`] is called, then shuts down gracefully as
    /// [`Server::run_until_signal`] does.
    pub fn run_until_stopped(self) {
        let workers = self.workers();
        while !self.state.stop_requested.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(50));
        }
        eprintln!("shutting down: finishing running requests");
        self.shutdown(workers);
    }

    /// Stops taking requests, waits for running ones (up to the grace period), and
    /// checkpoints the read-write datasets written to since their last checkpoint.
    pub fn stop(self) {
        self.shutdown(Vec::new());
    }

    fn shutdown(self, workers: Vec<thread::JoinHandle<()>>) {
        let state = &self.state;
        state.stopping.store(true, Ordering::Relaxed);
        let deadline = Instant::now() + state.options.shutdown_grace;
        while state.in_flight.load(Ordering::Relaxed) > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let left = state.in_flight.load(Ordering::Relaxed);
        if left > 0 {
            eprintln!("{left} requests still running after the grace period");
        }
        #[cfg(feature = "replication")]
        replication::stop_followers(state);
        // The last requests' spans, before the process ends.
        #[cfg(feature = "otel")]
        if let Some(tracer) = &state.tracer {
            tracer.flush(Duration::from_secs(5));
        }
        let entries: Vec<Arc<Entry>> = state
            .registry
            .read_unpoisoned()
            .datasets
            .values()
            .cloned()
            .collect();
        for entry in entries {
            if entry.mode == Mode::ReadWrite
                && let tripletrail::store::Dataset::TripleTrail(store) = entry.engine.store()
                && store.is_persistent()
                && let Err(e) = store.checkpoint_if_changed()
            {
                eprintln!("checkpoint of {}: {e}", entry.name);
            }
        }
        if left == 0 {
            for w in workers {
                let _ = w.join();
            }
        }
    }
}

/// The message of a request that ran past its timeout.
pub(crate) const TIMED_OUT: &str = "The operation took longer than its timeout";

/// An error answered as `text/plain`.
#[derive(Debug)]
pub(crate) struct HttpError(pub(crate) u16, pub(crate) String);

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.0, self.1)
    }
}

impl HttpError {
    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self(400, message.into())
    }
}

/// The file named by a store error for a file that failed its check (`StoreError::Corrupt`
/// with a file, not an unknown term id).
fn damaged_file(message: &str) -> Option<&str> {
    message
        .split_once("corrupt store file ")
        .map(|(_, f)| f.trim())
        .filter(|f| !f.starts_with("unknown term"))
}

impl HttpError {
    /// A write to a store whose files failed their check (#436): 503, naming the file. The
    /// dataset still answers queries; it takes writes again once restored from a backup.
    fn damaged(message: &str) -> Self {
        let file = damaged_file(message).unwrap_or(message);
        Self(
            503,
            format!(
                "The dataset's store file {file} is damaged (it does not match its checksum): \
                 the dataset answers queries but refuses writes until it is restored from a backup"
            ),
        )
    }
}

impl From<tripletrail::Error> for HttpError {
    fn from(e: tripletrail::Error) -> Self {
        let message = e.to_string();
        match e {
            tripletrail::Error::Syntax(_) | tripletrail::Error::Request(_) => Self(400, message),
            tripletrail::Error::Forbidden(_) => Self(403, message),
            _ if e.is_bad_service_request() => Self(400, message),
            _ if message.contains("read-only") => Self(405, "The dataset is read-only".to_owned()),
            _ if message.contains("cancelled") => Self(503, TIMED_OUT.to_owned()),
            _ if message.contains("more memory than its limit") => Self(503, message),
            _ if damaged_file(&message).is_some() => Self::damaged(&message),
            // A SERVICE the server may not call is the query's fault; a remote endpoint that
            // failed is a bad gateway.
            _ if message.contains("not an allowed remote endpoint")
                || message.contains("remote endpoints must be allowed") =>
            {
                Self(400, message)
            }
            _ if message.contains("SERVICE <") => Self(502, message),
            // CREATE of a graph that exists, CLEAR or DROP of one that does not: a bad
            // request, as Fuseki answers.
            tripletrail::Error::Update(
                tripletrail::oxigraph::sparql::UpdateEvaluationError::GraphAlreadyExists(_)
                | tripletrail::oxigraph::sparql::UpdateEvaluationError::GraphDoesNotExist(_),
            )
            | tripletrail::Error::Dataset(
                tripletrail::store::DatasetError::Update(
                    tripletrail::oxigraph::sparql::UpdateEvaluationError::GraphAlreadyExists(_)
                    | tripletrail::oxigraph::sparql::UpdateEvaluationError::GraphDoesNotExist(_),
                )
                | tripletrail::store::DatasetError::StoreUpdate(
                    tripletrail::store::UpdateError::GraphAlreadyExists(_)
                    | tripletrail::store::UpdateError::GraphDoesNotExist(_),
                ),
            ) => Self(400, message),
            tripletrail::Error::Loader(_)
            | tripletrail::Error::Dataset(tripletrail::store::DatasetError::Loader(_))
            | tripletrail::Error::Dataset(tripletrail::store::DatasetError::Store(
                tripletrail::store::StoreError::Parse(_),
            )) => Self(400, message),
            _ => Self(500, message),
        }
    }
}

impl From<tripletrail::oxigraph::store::StorageError> for HttpError {
    fn from(e: tripletrail::oxigraph::store::StorageError) -> Self {
        Self(500, e.to_string())
    }
}

impl From<tripletrail::store::DatasetError> for HttpError {
    fn from(e: tripletrail::store::DatasetError) -> Self {
        match e {
            tripletrail::store::DatasetError::Store(tripletrail::store::StoreError::ReadOnly) => {
                Self(405, "The dataset is read-only".to_owned())
            }
            tripletrail::store::DatasetError::Loader(_) => Self(400, e.to_string()),
            _ if damaged_file(&e.to_string()).is_some() => Self::damaged(&e.to_string()),
            _ => Self(500, e.to_string()),
        }
    }
}

impl From<std::io::Error> for HttpError {
    fn from(e: std::io::Error) -> Self {
        Self(400, e.to_string())
    }
}

type Body = Box<dyn Read>;

/// A body that lets its permits (the caller's turn, the turn on the dataset) go once it is
/// read to its end: at its declared length, or at the end of the reader, so before its last
/// bytes are handed on to be written.
struct ReleaseAtEnd {
    body: Body,
    /// The bytes still to come, when the length is known.
    left: Option<usize>,
    permit: Option<limits::CallerPermit>,
    turn: Option<limits::Permit>,
}

impl Read for ReleaseAtEnd {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.body.read(buf)?;
        if let Some(left) = &mut self.left {
            *left = left.saturating_sub(n);
        }
        if (n == 0 && !buf.is_empty()) || self.left == Some(0) {
            self.permit = None;
            self.turn = None;
        }
        Ok(n)
    }
}

pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) content_type: Option<String>,
    pub(crate) body: Body,
    pub(crate) length: Option<usize>,
    pub(crate) headers: Vec<(&'static str, String)>,
}

/// The body of a successful SPARQL update: Fuseki's.
const UPDATE_SUCCEEDED: &str = "<html>\n<head>\n</head>\n<body>\n<h1>Success</h1>\n<p>\nUpdate succeeded\n</p>\n</body>\n</html>\n";

impl Reply {
    pub(crate) fn empty(status: u16) -> Self {
        Self {
            status,
            content_type: None,
            body: Box::new(std::io::empty()),
            length: Some(0),
            headers: Vec::new(),
        }
    }

    pub(crate) fn bytes(status: u16, content_type: &str, data: Vec<u8>) -> Self {
        Self {
            status,
            content_type: Some(content_type.to_owned()),
            length: Some(data.len()),
            body: Box::new(std::io::Cursor::new(data)),
            headers: Vec::new(),
        }
    }

    /// A body written by `write` on another thread while it is being sent.
    pub(crate) fn stream(
        content_type: &str,
        write: impl FnOnce(&mut dyn Write) -> std::io::Result<()> + Send + 'static,
    ) -> Result<Self, HttpError> {
        let (reader, mut writer) = std::io::pipe()?;
        run_deep(move || {
            let mut buffered = std::io::BufWriter::with_capacity(64 * 1024, &mut writer);
            // An error here (a timeout, the client leaving) ends the body early.
            if write(&mut buffered).is_ok() {
                let _ = buffered.flush();
            }
        });
        Ok(Self {
            status: 200,
            content_type: Some(content_type.to_owned()),
            body: Box::new(reader),
            length: None,
            headers: Vec::new(),
        })
    }
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

/// Answers `value` as JSON.
pub(crate) fn json_reply(status: u16, value: &serde_json::Value) -> Reply {
    Reply::bytes(status, "application/json", value.to_string().into_bytes())
}

/// What a request was about, filled in while it is routed: for the statistics and the
/// request log. It holds the dataset while the response is sent, so a detach waits for it.
#[derive(Default)]
struct Context {
    /// The request's spans (#220), when it is traced.
    #[cfg(feature = "otel")]
    trace: Option<otel::Trace>,
    dataset: Option<Arc<Entry>>,
    /// `TripleTrail-Min-Version`: on a read replica, the request waits until the replica has
    /// applied the leader's log that far (read your writes, #217).
    min_version: Option<String>,
    operation: Option<Operation>,
    /// The query or update text.
    text_hash: Option<u64>,
    /// The query or update text itself, kept only for the slow query log and a workspace's
    /// history.
    text: Option<String>,
    /// Keep `text` whatever the slow query log: for a workspace's history.
    keep_text: bool,
    /// The query hints of the text (`#pragma tt.…`, #238), logged so that the queries the
    /// planner gets wrong are found.
    hints: Option<tripletrail::Hints>,
    /// Who is calling, when authentication is configured.
    caller: Option<auth::Caller>,
    /// With `X-TripleTrail-View-As`, the admin who sent the request: `caller` is then the one
    /// viewed as.
    viewer: Option<auth::Caller>,
    /// What the caller may do with the dataset, once one is chosen.
    access: Option<auth::Access>,
    /// The graphs the caller may read, when it may read only some (#201).
    readable: Option<tripletrail::ReadableGraphs>,
    /// The graphs the caller may write, when it may write only some (#201).
    writable: Option<tripletrail::ReadableGraphs>,
    /// The row rules that apply to the caller (#202), compiled with the query.
    row_rules: Option<tripletrail::RowRules>,
    /// The chosen dataset's limits.
    limits: limits::Limits,
    /// The request's turn on its dataset, held until the answer is read to its end.
    permit: Option<limits::Permit>,
    /// The caller's turn, held until the answer is read to its end.
    caller_permit: Option<limits::CallerPermit>,
    /// For a 429: when to retry, in seconds.
    retry_after: Option<u64>,
    /// The answer depends on `Accept` (the OGC API's HTML pages): errors say so too.
    vary_accept: bool,
    /// Checked by [`protocol`] once it knows the request is an update, before the caller's
    /// write access: an overlay workspace's (#406) owner check and change cap.
    update_gate: Option<UpdateGate>,
    /// The budget the request ran into, if any (#189): for `/$/metrics`.
    trip: Option<metrics::Trip>,
    /// The request reads a dataset (its `operation` is a read) but changes something else,
    /// such as a workspace: the audit log records it (#207).
    changes: bool,
    /// What the request's writes did to each graph, when the audit log counts them (#207):
    /// `None` when a write could not be counted (a bulk load).
    #[cfg(feature = "enterprise")]
    graph_changes: Option<Vec<tripletrail::GraphChanges>>,
}

/// What lets an update through to a store that is not a dataset (see [`Context::update_gate`]).
pub(crate) struct UpdateGate {
    /// Whether the caller may change the store; when it may, it writes as a writer.
    pub(crate) allowed: Result<(), HttpError>,
    /// The quads the store changed so far, at most, and how many it may change.
    pub(crate) changes: usize,
    pub(crate) max: usize,
}

/// Counts a request as running until dropped.
struct InFlight<'a>(&'a AtomicUsize);

impl<'a> InFlight<'a> {
    fn new(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Reads what is left of the body of a request that sent `Expect: 100-continue`, up to 1 MiB,
/// and says whether the connection may be kept. Such a client sends its body only once the
/// server asks for it; answered without that, the body never comes, and tiny_http would take
/// the next request on the connection as the rest of it (RDF4J's client begins transactions
/// this way). A body left unread past the limit closes the connection instead.
fn finish_expected_body(request: &mut Request) -> bool {
    if !request_header(request, "Expect").is_some_and(|e| e.eq_ignore_ascii_case("100-continue")) {
        return true;
    }
    let mut buffer = [0u8; 8192];
    let mut left = 1usize << 20;
    let reader = request.as_reader();
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return true,
            Ok(n) if n < left => left -= n,
            _ => return false,
        }
    }
}

/// Sends `response`. When the connection ends after it, because the client said so
/// (`Connection: close`, or HTTP/1.0 without keep-alive) or `close` (a body left unread), the
/// answer says `Connection: close` (#558, RFC 9112 §9.6). tiny_http closes such connections
/// but leaves that header out (it drops it from the headers it is given), and a client
/// pooling connections then sends its next request on the closed one, which fails: a POST
/// is not retried, so an update was lost.
fn respond<R: Read>(request: Request, response: Response<R>, close: bool) -> std::io::Result<()> {
    if !close && !client_closes(&request) {
        return request.respond(response);
    }
    let version = request.http_version().clone();
    let headers = request.headers().to_vec();
    let head = *request.method() == Method::Head;
    let mut writer = CloseHeader {
        inner: request.into_writer(),
        done: false,
    };
    response.raw_print(&mut writer, version, &headers, head, None)?;
    writer.flush()
}

/// Whether tiny_http closes the connection after answering `request`: as it decides.
fn client_closes(request: &Request) -> bool {
    let old = *request.http_version() == tiny_http::HTTPVersion(1, 0);
    match request_header(request, "Connection").map(str::to_ascii_lowercase) {
        Some(v) if v.contains("close") || v.contains("upgrade") => true,
        Some(v) => old && !v.contains("keep-alive"),
        None => old,
    }
}

/// A writer that adds `Connection: close` after the status line of the response written
/// through it.
struct CloseHeader<W> {
    inner: W,
    done: bool,
}

impl<W: std::io::Write> std::io::Write for CloseHeader<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.done {
            return self.inner.write(buf);
        }
        match buf.iter().position(|&b| b == b'\n') {
            // No end of the status line here: what is written holds none either.
            None => self.inner.write(buf),
            Some(i) => {
                self.inner.write_all(&buf[..=i])?;
                self.inner.write_all(b"Connection: close\r\n")?;
                self.done = true;
                Ok(i + 1)
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn handle(state: &Arc<State>, mut request: Request) {
    // With TLS, the HTTP server listens on loopback for the relay only. A connection the relay
    // has not registered (opened from this machine directly, or one whose client is no longer
    // known) is refused rather than answered as a loopback caller.
    #[cfg(feature = "tls")]
    if let Some(tls) = &state.tls
        && !request
            .remote_addr()
            .is_some_and(|r| tls.client_of(*r).is_some())
    {
        let _ = respond(
            request,
            tiny_http::Response::from_string("Use HTTPS on the server's address")
                .with_status_code(400),
            true,
        );
        return;
    }
    let _running = InFlight::new(&state.in_flight);
    let started = Instant::now();
    let mut context = Context::default();
    // A trace left on this worker (by a panic between `enter` and `take`) is never another
    // request's.
    #[cfg(feature = "otel")]
    drop(otel::Trace::take());
    #[cfg(feature = "otel")]
    if let Some(tracer) = &state.tracer
        && let Some(trace) = otel::Trace::start(
            request_header(&request, "traceparent"),
            tracer.options.sample,
        )
    {
        trace.enter();
    }
    // A panic while answering (a bug, e.g. in a geometry function) becomes a 500 for this
    // request instead of taking the worker thread down with it.
    let mut answer = || {
        panic::catch_unwind(AssertUnwindSafe(|| {
            route(state, &mut request, &mut context)
        }))
        .unwrap_or_else(|payload| Err(HttpError(500, panic_message(&*payload))))
    };
    // The audit log says what each change did to each graph (#207).
    #[cfg(feature = "enterprise")]
    let routed = if state.audit.is_some() {
        let (routed, graphs) = tripletrail::counting_changes(answer);
        context.graph_changes = graphs;
        routed
    } else {
        answer()
    };
    #[cfg(not(feature = "enterprise"))]
    let routed = answer();
    #[cfg(feature = "otel")]
    {
        context.trace = otel::Trace::take();
    }
    let reply = routed.unwrap_or_else(|HttpError(status, message)| {
        context.trip = metrics::Trip::of_error(status, &message);
        let mut reply = Reply::bytes(status, "text/plain; charset=utf-8", message.into_bytes());
        if status == 401 {
            reply.headers.push((
                "WWW-Authenticate",
                "Bearer realm=\"tripletrail\"".to_owned(),
            ));
            // With password users, clients and browsers are asked for them too, as Fuseki
            // asks (#825).
            if state.auth.as_ref().is_some_and(|a| a.has_users()) {
                reply.headers.push((
                    "WWW-Authenticate",
                    "Basic realm=\"tripletrail\", charset=\"UTF-8\"".to_owned(),
                ));
            }
        }
        if status == 429 {
            let wait = context.retry_after.unwrap_or(1);
            reply.headers.push(("Retry-After", wait.to_string()));
        }
        if context.vary_accept {
            reply.headers.push(("Vary", "Accept".to_owned()));
        }
        reply
    });
    let status = reply.status;
    let mut headers = Vec::new();
    #[cfg(feature = "replication")]
    let mut sync = None;
    // After a write, where the dataset's log stands: a client reading from a replica next
    // sends it back as `TripleTrail-Min-Version` to see its own write (#217).
    if status < 300
        && matches!(
            context.operation,
            Some(Operation::Update | Operation::Upload)
        )
        && let Some(entry) = &context.dataset
        && let tripletrail::store::Dataset::TripleTrail(store) = entry.engine.store()
        && let Some(p) = store.log_position()
    {
        headers.push(header(
            "TripleTrail-Version",
            &format!("{}:{}:{}", p.timeline, p.generation, p.length),
        ));
        // Followers waiting for a commit read the log again at once.
        #[cfg(feature = "replication")]
        state.acks.note_commit();
        // With --sync-replicas, the write answers once that many replicas hold it (below).
        #[cfg(feature = "replication")]
        if state.options.sync_replicas > 0 {
            sync = Some((
                entry.name.clone(),
                (p.timeline.clone(), p.generation, p.length),
            ));
        }
    }
    // The wait runs on a thread of its own, so that the worker is free for the replicas'
    // reads of the log that bring the news (#764); past MAX_SYNC_WAITERS, on the worker.
    #[cfg(feature = "replication")]
    if let Some((name, (timeline, generation, length))) = sync {
        // A write's answer is short: read now, so the job can move to another thread.
        let mut body = Vec::new();
        let mut reply = reply;
        let _ = reply.body.read_to_end(&mut body);
        let (content_type, extra) = (reply.content_type, reply.headers);
        let waiting = Arc::clone(state);
        let job = move || {
            let reply = Reply {
                content_type,
                headers: extra,
                ..Reply::bytes(status, "", body)
            };
            let _running = InFlight::new(&waiting.in_flight);
            let held = waiting.acks.wait(
                &name,
                (&timeline, generation, length),
                waiting.options.sync_replicas,
                waiting.options.sync_timeout,
            );
            let mut headers = headers;
            headers.push(header("TripleTrail-Replicas", &held.to_string()));
            send_reply(&waiting, request, reply, headers, context, started);
        };
        if state.sync_waiters.fetch_add(1, Ordering::SeqCst) < MAX_SYNC_WAITERS {
            let counted = Arc::clone(state);
            let spawned = thread::Builder::new()
                .name("sync-write".into())
                .spawn(move || {
                    job();
                    counted.sync_waiters.fetch_sub(1, Ordering::SeqCst);
                });
            if spawned.is_err() {
                // The answer is lost with the thread (the client sees the connection close).
                state.sync_waiters.fetch_sub(1, Ordering::SeqCst);
            }
        } else {
            state.sync_waiters.fetch_sub(1, Ordering::SeqCst);
            job();
        }
        return;
    }
    send_reply(state, request, reply, headers, context, started);
}

/// Writes deferred to threads of their own while they wait for --sync-replicas, at most.
#[cfg(feature = "replication")]
const MAX_SYNC_WAITERS: usize = 1024;

/// The `Server` header of every answer.
const SERVER: &str = concat!("TripleTrail/", env!("CARGO_PKG_VERSION"));

/// Sends `reply` (with `headers`), then records and logs the request.
fn send_reply(
    state: &Arc<State>,
    mut request: Request,
    reply: Reply,
    mut headers: Vec<tiny_http::Header>,
    #[cfg_attr(not(feature = "otel"), allow(unused_mut))] mut context: Context,
    started: Instant,
) {
    let status = reply.status;
    // A result cut at max-rows with partial=true (#189).
    let cut = reply
        .headers
        .iter()
        .any(|(name, _)| *name == "X-Result-Incomplete");
    let close = !finish_expected_body(&mut request);
    if let Some(t) = &reply.content_type {
        headers.push(header("Content-Type", t));
    }
    for (name, value) in &reply.headers {
        headers.push(header(name, value));
    }
    // Names the engine, where tiny_http would say "tiny-http (Rust)": MatGUI's Explain view
    // recognises TripleTrail by it, and asks /{ds}/explain for the plan.
    headers.push(header("Server", SERVER));
    let origin = if state.options.cors {
        Some("*")
    } else {
        request_header(&request, "Origin").filter(|o| {
            state
                .options
                .cors_origins
                .iter()
                .any(|allowed| allowed == o)
        })
    };
    if !state.options.cors && !state.options.cors_origins.is_empty() {
        headers.push(header("Vary", "Origin"));
    }
    if let Some(origin) = origin {
        headers.push(header("Access-Control-Allow-Origin", origin));
        // Headers a page on that origin may read (#223: the JavaScript client's `incomplete`).
        headers.push(header(
            "Access-Control-Expose-Headers",
            "X-Result-Incomplete, X-Cache, X-Truncated, Server",
        ));
        if *request.method() == Method::Options {
            headers.push(header(
                "Access-Control-Allow-Methods",
                "GET, HEAD, POST, PUT, DELETE, OPTIONS",
            ));
            headers.push(header(
                "Access-Control-Allow-Headers",
                "Authorization, Content-Type, Accept, X-API-Key",
            ));
        }
    }
    let head = *request.method() == Method::Head;
    #[cfg(feature = "enterprise")]
    let client = client_address(state, &request);
    #[cfg(feature = "enterprise")]
    let bytes_in = request_header(&request, "Content-Length").and_then(|n| n.parse::<u64>().ok());
    let method = request.method().to_string();
    let url = request.url().to_owned();
    let body: Body = if head {
        Box::new(std::io::empty())
    } else {
        reply.body
    };
    // The caller's turn and the turn on the dataset end once the body is read to its end,
    // before its last bytes are written: a client that sends its next request as soon as this
    // answer is in must not wait for, or be refused for, a turn this one still holds, nor see
    // it still running in the statistics.
    let length = if head { Some(0) } else { reply.length };
    if length == Some(0) {
        context.caller_permit = None;
        context.permit = None;
    }
    let body: Body = match (context.caller_permit.take(), context.permit.take()) {
        (None, None) => body,
        (permit, turn) => Box::new(ReleaseAtEnd {
            body,
            left: length,
            permit,
            turn,
        }),
    };
    let response = Response::new(StatusCode(status), headers, body, length, None);
    // Returns once the body is sent: the dataset is held until then.
    #[cfg(feature = "otel")]
    let sending = SystemTime::now();
    // Written before the answer goes out: a client that sees a change done finds it in the
    // log.
    #[cfg(feature = "enterprise")]
    if let Some(log) = &state.audit {
        let elapsed = started.elapsed();
        audit::record(
            log,
            state.options.audit_reads,
            &method,
            &url,
            status,
            elapsed,
            &context,
            client,
            bytes_in,
        );
    }
    let sent = respond(request, response, close);
    let elapsed = started.elapsed();
    #[cfg(feature = "otel")]
    if let (Some(tracer), Some(mut trace)) = (&state.tracer, context.trace.take()) {
        // Streamed answers are evaluated while they are sent: this span holds the rest of
        // the evaluation too.
        trace.add(
            "http.send",
            sending,
            sent.as_ref().err().map(ToString::to_string),
        );
        tracer.send(request_spans(trace, &method, &url, status, &context));
    }
    #[cfg(not(feature = "otel"))]
    let _ = sent;
    if let Some(entry) = &context.dataset {
        entry.stats.record(context.operation, status);
        entry.timings.record(context.operation, status, elapsed);
        if let Some(trip) = context.trip.or(cut.then_some(metrics::Trip::Partial)) {
            entry.timings.trip(trip);
        }
    }
    if let (Some(limit), Some(text)) = (state.options.slow_query, &context.text)
        && elapsed >= limit
    {
        let line = serde_json::json!({
            "time": admin::rfc3339(SystemTime::now()),
            "event": "slow_query",
            "dataset": context.dataset.as_ref().map(|e| e.name.clone()),
            "operation": context.operation.map(|o| format!("{o:?}").to_lowercase()),
            "status": status,
            "ms": (elapsed.as_secs_f64() * 1e5).round() / 100.0,
            "query_hash": context.text_hash.map(|h| format!("{h:016x}")),
            "hints": context.hints.map(|h| h.to_string()),
            "text": text,
        });
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "{line}");
    }
    if state.options.log_requests {
        let path = url.split_once('?').map_or(url.as_str(), |(p, _)| p);
        let mut line = serde_json::json!({
            "time": admin::rfc3339(SystemTime::now()),
            "method": method,
            "path": path,
            "status": status,
            "ms": (elapsed.as_secs_f64() * 1e5).round() / 100.0,
        });
        if let Some(entry) = &context.dataset {
            line["dataset"] = serde_json::json!(entry.name);
        }
        if let Some(h) = context.text_hash {
            line["query_hash"] = serde_json::json!(format!("{h:016x}"));
        }
        if let Some(h) = context.hints {
            line["hints"] = serde_json::json!(h.to_string());
        }
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{line}");
    }
}

/// The spans of a finished request: its own, named after its method and route, and its steps.
#[cfg(feature = "otel")]
fn request_spans(
    mut trace: otel::Trace,
    method: &str,
    url: &str,
    status: u16,
    context: &Context,
) -> Vec<otel::Span> {
    let path = url.split_once('?').map_or(url, |(p, _)| p);
    // A low-cardinality route: the dataset's name becomes `{dataset}`.
    let route = match &context.dataset {
        Some(entry) => {
            let prefix = format!("/{}", entry.name);
            match path.strip_prefix(&prefix) {
                Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                    format!("/{{dataset}}{rest}")
                }
                _ => path.to_owned(),
            }
        }
        None if path.starts_with("/$/") => {
            // `/$/datasets/kg` → `/$/datasets`: admin routes keep their first segment.
            let mut parts = path.splitn(4, '/');
            [parts.next(), parts.next(), parts.next()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("/")
        }
        None => path.to_owned(),
    };
    trace.attributes.extend([
        ("http.request.method", otel::Attr::from(method)),
        ("url.path", otel::Attr::from(path)),
        ("http.route", otel::Attr::from(route.as_str())),
        (
            "http.response.status_code",
            otel::Attr::Int(i64::from(status)),
        ),
    ]);
    if let Some(entry) = &context.dataset {
        trace
            .attributes
            .push(("db.namespace", otel::Attr::from(entry.name.as_str())));
    }
    if let Some(o) = context.operation {
        trace.attributes.push((
            "db.operation.name",
            otel::Attr::from(format!("{o:?}").to_lowercase()),
        ));
    }
    if let Some(h) = context.text_hash {
        trace.attributes.push((
            "tripletrail.query_hash",
            otel::Attr::from(format!("{h:016x}")),
        ));
    }
    // Server errors only: a 4xx is the caller's (OpenTelemetry's HTTP conventions).
    let error = (status >= 500).then(|| format!("HTTP {status}"));
    trace.finish(format!("{method} {route}"), error)
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    let detail = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown cause");
    format!("Internal error: {detail}")
}

/// A `timeout` parameter in seconds: a finite, non-negative number that fits a [`Duration`].
fn parse_timeout(value: &str) -> Result<Duration, HttpError> {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .and_then(|t| Duration::try_from_secs_f64(t).ok())
        .ok_or_else(|| HttpError::bad_request(format!("Invalid timeout {value}")))
}

/// Query string and form parameters.
pub(crate) struct Params(pub(crate) Vec<(String, String)>);

impl Params {
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn has(&self, name: &str) -> bool {
        self.0.iter().any(|(k, _)| k == name)
    }

    fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.0
            .iter()
            .filter(move |(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// The IRIs of a repeatable parameter, or 400 for one that isn't an IRI.
fn iri_params(params: &Params, name: &str) -> Result<Vec<NamedNode>, HttpError> {
    params
        .all(name)
        .map(|v| {
            NamedNode::new(v)
                .map_err(|e| HttpError::bad_request(format!("{name}: <{v}> is not an IRI: {e}")))
        })
        .collect()
}

/// The `boundary` parameter of a multipart content type.
fn boundary(content_type: &str) -> Option<&str> {
    content_type.split(';').skip(1).find_map(|p| {
        let (name, value) = p.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("boundary")
            .then(|| value.trim().trim_matches('"'))
    })
}

/// The parts of a `multipart/form-data` body, with the RDF format of each: its own
/// Content-Type, or else the extension of its file name.
fn multipart<'a>(
    body: &'a [u8],
    boundary: &str,
) -> Result<Vec<(rdf_formats::Format, &'a [u8])>, HttpError> {
    let delimiter = format!("--{boundary}");
    let find = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).position(|w| w == needle);
    let bad = |what: &str| HttpError::bad_request(format!("multipart/form-data: {what}"));
    let mut rest = &body[find(body, delimiter.as_bytes()).ok_or_else(|| bad("no boundary"))?..];
    let mut parts = Vec::new();
    loop {
        rest = &rest[delimiter.len()..];
        if rest.starts_with(b"--") {
            return Ok(parts);
        }
        let start = find(rest, b"\r\n\r\n").ok_or_else(|| bad("a part without headers"))?;
        let headers = String::from_utf8_lossy(&rest[..start]);
        let content = &rest[start + 4..];
        let end = find(content, format!("\r\n{delimiter}").as_bytes())
            .ok_or_else(|| bad("no closing boundary"))?;
        let mut format = None;
        for line in headers.split("\r\n") {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.trim().eq_ignore_ascii_case("content-type") {
                let media = value.split(';').next().unwrap_or_default().trim();
                format = format.or(rdf_formats::Format::from_media_type(media));
            } else if name.trim().eq_ignore_ascii_case("content-disposition") {
                let extension = value
                    .split(';')
                    .find_map(|p| p.trim().strip_prefix("filename="))
                    .and_then(|f| f.trim_matches('"').rsplit_once('.'))
                    .map(|(_, e)| e.to_owned());
                format = format.or(extension
                    .as_deref()
                    .and_then(rdf_formats::Format::from_extension));
            }
        }
        let format = format.ok_or_else(|| bad("a part of an unknown RDF format"))?;
        parts.push((format, &content[..end]));
        rest = &content[end + 2..];
    }
}

/// The `charset` parameter of a content type.
fn charset(content_type: &str) -> Option<&str> {
    content_type.split(';').skip(1).find_map(|p| {
        let (name, value) = p.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches('"'))
    })
}

/// The first language of the request's `Accept-Language`, if it names one.
pub(crate) fn first_language(request: &Request) -> Option<String> {
    request_header(request, "Accept-Language").and_then(|v| {
        v.split(',')
            .map(|t| t.split(';').next().unwrap_or_default().trim().to_owned())
            .find(|t| !t.is_empty() && t != "*")
    })
}

pub(crate) fn request_header<'a>(request: &'a Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str())
}

/// The media type of the body, without parameters, lower case.
pub(crate) fn content_type(request: &Request) -> Option<String> {
    request_header(request, "Content-Type").map(|t| {
        t.split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
    })
}

fn text_hash(text: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// The address of the connection's other end: the client, also for a request that came over
/// TLS (which reaches the HTTP server from the relay, on loopback).
pub(crate) fn remote_addr(state: &State, request: &Request) -> Option<SocketAddr> {
    let remote = *request.remote_addr()?;
    #[cfg(feature = "tls")]
    if let Some(tls) = &state.tls {
        // Never the relay's own loopback address (`handle` refuses those requests).
        return tls.client_of(remote);
    }
    #[cfg(not(feature = "tls"))]
    let _ = state;
    Some(remote)
}

/// Whether the request came over TLS to this server (not through a proxy).
fn is_https(state: &State, request: &Request) -> bool {
    #[cfg(feature = "tls")]
    if let (Some(tls), Some(remote)) = (&state.tls, request.remote_addr()) {
        return tls.client_of(*remote).is_some();
    }
    let _ = (state, request);
    false
}

/// The client's address: behind a trusted proxy of the auth configuration, the address it
/// forwarded for.
pub(crate) fn client_address(state: &State, request: &Request) -> Option<std::net::IpAddr> {
    let remote = remote_addr(state, request).map(|a| a.ip());
    match &state.auth {
        Some(auth) => auth.client(
            &|name| {
                request
                    .headers()
                    .iter()
                    .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
                    .map(|h| h.value.as_str().to_owned())
            },
            remote,
        ),
        None => remote,
    }
}

/// Makes `entry` the request's dataset, once it has a turn under the dataset's limits.
pub(crate) fn use_dataset(
    state: &State,
    context: &mut Context,
    entry: &Arc<Entry>,
) -> Result<(), HttpError> {
    let limits = limits::Limits::of(state, &entry.name);
    // Set first, so a refusal counts in the dataset's statistics.
    context.dataset = Some(Arc::clone(entry));
    context.limits = limits;
    context.permit = Some(limits::enter(entry, &limits)?);
    // After taking its turn, so a wait counts under the dataset's maxConcurrent.
    #[cfg(feature = "replication")]
    if let Some(version) = &context.min_version {
        replication::await_version(state, &entry.name, version)?;
    }
    Ok(())
}

fn route(
    state: &Arc<State>,
    request: &mut Request,
    context: &mut Context,
) -> Result<Reply, HttpError> {
    let url = request.url().to_owned();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let mut params = Params(
        form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect(),
    );
    context.min_version = request_header(request, "TripleTrail-Min-Version").map(str::to_owned);
    let method = request.method().clone();
    if method == Method::Options {
        let mut reply = Reply::empty(204);
        reply.content_type = None;
        return Ok(reply);
    }
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // Browser sign-in (#198) is how a browser gets credentials: it needs none.
    #[cfg(feature = "enterprise")]
    if let ["$", "sso", rest @ ..] = segments.as_slice() {
        return sso::route(state, request, rest, &params);
    }
    if let Some(auth) = &state.auth
        && !matches!(
            segments.as_slice(),
            ["$", "ping" | "ready"]
                | ["$", "role", _]
                | ["$", "ui", "config.json"]
                | ["$", "ui", "brand", _]
        )
    {
        let remote = remote_addr(state, request).map(|a| a.ip());
        let header = |name: &str| {
            request
                .headers()
                .iter()
                .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
                .map(|h| h.value.as_str().to_owned())
        };
        let caller = match auth.identify(&header, remote) {
            Ok(caller) => caller,
            // A browser opening the web UI without credentials signs in (#198).
            #[cfg(feature = "enterprise")]
            Err(HttpError(401, _))
                if matches!(method, Method::Get)
                    && matches!(segments.as_slice(), [] | ["$", "ui", ..])
                    && request_header(request, "Accept")
                        .is_some_and(|a| a.contains("text/html"))
                    && auth.browser_sign_in().is_some() =>
            {
                let back: String = form_urlencoded::byte_serialize(url.as_bytes()).collect();
                let mut reply = Reply::empty(302);
                reply.content_type = None;
                reply
                    .headers
                    .push(("Location", format!("/$/sso/login?return={back}")));
                return Ok(reply);
            }
            Err(e) => return Err(e),
        };
        let real = header(auth::VIEW_AS).map(|_| caller.clone());
        let caller = auth::view_as(caller, &header)?;
        // The admin behind a view-as request (#207): the audit log names them, and the
        // access pages refuse changes made under another name.
        context.viewer = real;
        // A browser session may change things only from the server's own pages (#198): a
        // page of another site cannot use it to send changes (CSRF). Its cookie is also
        // `SameSite=Lax`.
        #[cfg(feature = "enterprise")]
        if caller.method == auth::Method::Session
            && !matches!(method, Method::Get | Method::Head)
            && !auth
                .browser_sign_in()
                .is_some_and(|s| sso::same_origin(&s, request))
        {
            return Err(HttpError(
                403,
                "A change made with a browser sign-in must come from this server's own pages \
                 (Origin)"
                    .to_owned(),
            ));
        }
        // Single sign-on needs a licence that covers it (#198): without, its tokens are
        // refused, and keys, passwords and trusted headers go on working.
        #[cfg(feature = "enterprise")]
        if matches!(caller.method, auth::Method::Oidc | auth::Method::Session)
            && !state
                .options
                .license_status()
                .allows(tripletrail_enterprise::features::SSO)
        {
            return Err(auth::unauthorized(
                "Single sign-on needs a TripleTrail Enterprise licence that covers sso",
            ));
        }
        context.caller = Some(caller);
    }
    if !matches!(
        segments.as_slice(),
        ["$", "ping" | "ready"] | ["$", "role", _]
    ) {
        let caller = context.caller.as_ref();
        match limits::admit_caller(
            state,
            caller.and_then(|c| c.agent.as_deref()),
            caller.map_or(&[][..], |c| &c.groups),
            client_address(state, request),
        ) {
            Ok(permit) => context.caller_permit = permit,
            Err(limits::Refused(error, wait)) => {
                context.retry_after = Some(wait);
                return Err(error);
            }
        }
    }
    {
        let registry = state.registry.read_unpoisoned();
        let rdf4j = rdf4j::matches(&segments, |name| registry.resolve(name).is_some());
        drop(registry);
        if let Some(rest) = rdf4j {
            return rdf4j::route(state, request, rest, &mut params, context);
        }
    }
    const SERVICES: [&str; 12] = [
        "sparql", "query", "update", "data", "get", "explain", "void", "describe", "graphql",
        "shacl", "patch", "mcp",
    ];
    let (entry, service) = {
        let registry = state.registry.read_unpoisoned();
        match segments.as_slice() {
            ["$", "workspaces", rest @ ..] => {
                drop(registry);
                return workspace::route(state, request, rest, &mut params, context);
            }
            ["$", rest @ ..] => {
                drop(registry);
                return admin::route(state, request, rest, &mut params, context);
            }
            [".well-known", "void"] => {
                // A dataset of which the caller reads only some graphs is described from
                // those (#203).
                let entries: Vec<(Arc<Entry>, Option<ReadableGraphs>)> = registry
                    .datasets
                    .values()
                    .filter_map(|e| {
                        let view = may_list(state, context.caller.as_ref(), &[e.name.as_str()])?;
                        Some((Arc::clone(e), view))
                    })
                    .collect();
                drop(registry);
                return describe_datasets(state, request, &params, &method, &entries);
            }
            [".well-known", "dcat"] => {
                if !matches!(method, Method::Get | Method::Head) {
                    return Err(HttpError(405, format!("{method} is not allowed here")));
                }
                let mut all: Vec<&str> = registry
                    .datasets
                    .values()
                    .map(|e| e.name.as_str())
                    .collect();
                all.sort_unstable();
                all.dedup();
                // Listed as in `/.well-known/void`.
                let listed: Vec<&str> = all
                    .iter()
                    .copied()
                    .filter(|name| may_list(state, context.caller.as_ref(), &[name]).is_some())
                    .collect();
                let format = rdf_format(request, &params, RdfFormat::Turtle, true)?;
                let origin = origin(state, request);
                let aliases: Vec<(&str, &str)> = registry
                    .aliases
                    .iter()
                    .map(|(alias, a)| (alias.as_str(), a.target.as_str()))
                    .collect();
                let triples = dcat::describe(
                    &origin,
                    &listed,
                    &all,
                    &aliases,
                    state.options.catalog.as_deref(),
                    state.options.branding.name.as_deref(),
                )?;
                drop(registry);
                let body = service_description::serialize(&triples, format)?;
                return Ok(Reply::bytes(200, format.media_type(), body));
            }
            [] if state.options.ui
                && params.0.is_empty()
                && matches!(method, Method::Get | Method::Head) =>
            {
                let mut reply = Reply::empty(302);
                reply.headers.push(("Location", "/$/ui/".to_owned()));
                return Ok(reply);
            }
            [ds] if registry.resolve(ds).is_some() => (registry.resolve(ds).unwrap(), "sparql"),
            [ds, "ogc", ..] if registry.resolve(ds).is_some() => {
                (registry.resolve(ds).unwrap(), "ogc")
            }
            [ds, "tiles", ..] if registry.resolve(ds).is_some() => {
                (registry.resolve(ds).unwrap(), "tiles")
            }
            [ds, "api", ..] if registry.resolve(ds).is_some() => {
                (registry.resolve(ds).unwrap(), "api")
            }
            [ds, "search", ..] if registry.resolve(ds).is_some() => {
                (registry.resolve(ds).unwrap(), "search")
            }
            [ds, "import", ..] if registry.resolve(ds).is_some() => {
                (registry.resolve(ds).unwrap(), "import")
            }
            [ds, service] if SERVICES.contains(service) && registry.resolve(ds).is_some() => {
                (registry.resolve(ds).unwrap(), *service)
            }
            [service] if SERVICES.contains(service) && registry.single().is_some() => {
                (registry.single().unwrap(), *service)
            }
            _ => {
                drop(registry);
                return dereference(state, request, &params, &method, path, context);
            }
        }
    };
    let asked = segments.first().copied().unwrap_or_default();
    // `search/similar` reads as a GET also when its parameters come as a form.
    let reads = matches!(method, Method::Get | Method::Head)
        || (service == "search"
            && method == Method::Post
            && segments.get(2..) == Some(&["similar"][..]));
    // A caller that may read only some graphs gets the services that keep to them.
    let partial_ok = matches!(
        service,
        "query" | "sparql" | "update" | "patch" | "data" | "explain" | "graphql" | "mcp"
    )
        || (service == "void" && matches!(method, Method::Get | Method::Head))
        || (matches!(
            service,
            "get" | "describe" | "ogc" | "tiles" | "api" | "search"
        ) && reads)
        // REST writes go through the engine, which checks each graph written (#201).
        || service == "api";
    // Row rules apply to queries, explain, descriptions, Graph Store reads, OGC and tiles
    // (#202).
    let rows_ok = matches!(
        service,
        "query" | "sparql" | "explain" | "update" | "patch" | "graphql" | "mcp"
    ) || service == "data"
        || (matches!(
            service,
            "describe" | "get" | "ogc" | "tiles" | "api" | "search"
        ) && reads);
    check_access(
        state,
        context,
        &[asked, entry.name.as_str()],
        partial_ok,
        rows_ok,
    )?;
    let engine = entry.engine.clone();
    let read_only = entry.mode == Mode::ReadOnly;
    use_dataset(state, context, &entry)?;
    // The dataset's query memory (#189); a SPARQL request may ask for less.
    let engine = with_query_memory(engine, &context.limits, None);
    let engine = &engine;

    match service {
        "describe" => {
            if !matches!(method, Method::Get | Method::Head) {
                return Err(HttpError(405, format!("{method} is not allowed here")));
            }
            context.operation = Some(Operation::Read);
            let iri = params
                .get("iri")
                .ok_or_else(|| HttpError::bad_request("Missing the iri parameter"))?;
            let iri = NamedNode::new(iri)
                .map_err(|e| HttpError::bad_request(format!("Invalid IRI {iri}: {e}")))?;
            let doc = linked_data::negotiate(request, &params)?;
            let entry = context.dataset.clone().expect("set above");
            let rows = row_view(engine, context)?;
            // `reader=forms`: the rows the record forms show (Records, #81), with the shapes'
            // `tts:hiddenFrom`/`tts:onlyFor` for forms in place of those for describe pages.
            let reader = match params.get("reader") {
                None | Some("describe") => tripletrail::shapes::Reader::Describe,
                Some("forms") => tripletrail::shapes::Reader::Forms,
                Some(other) => {
                    return Err(HttpError::bad_request(format!(
                        "Unknown reader {other}: describe or forms"
                    )));
                }
            };
            // With `preview=`, the shapes of a preview (`POST /{ds}/tiles/preview`) in place
            // of the dataset's own, kept in the links to other resources.
            let preview = params.get("preview");
            let files = preview
                .map(|id| tiles::preview_files(&entry, id))
                .transpose()?;
            let describe_url = |iri: &str| {
                let mut query = form_urlencoded::Serializer::new(String::new());
                query.append_pair("iri", iri);
                if let Some(id) = preview {
                    query.append_pair("preview", id);
                }
                format!("{path}?{}", query.finish())
            };
            let href = |iri: &str| match preview {
                Some(_) => describe_url(iri),
                None => local_href(state, iri).unwrap_or_else(|| describe_url(iri)),
            };
            let shapes = || match &files {
                Some(files) => {
                    shapes::projected_with(state, &entry, reader, &View::of(context), files)
                }
                None => shapes::projected(state, &entry, reader, &View::of(context)),
            };
            return linked_data::answer(
                request,
                &linked_data::Answer {
                    engine,
                    links: linked_data::Links::Query(describe_url(iri.as_str())),
                    iri,
                    doc,
                    attached: entry.attached,
                    href: &href,
                    readable: context.readable.as_ref(),
                    rows: rows.as_ref(),
                    shapes: &shapes,
                    ui: state.options.ui,
                    dataset: &entry.name,
                    basemap: state.options.map_tiles.as_deref().map(|t| {
                        (
                            t,
                            state.options.map_attribution.as_deref().unwrap_or_default(),
                        )
                    }),
                },
            );
        }
        "api" => {
            let write = match method {
                Method::Get | Method::Head => None,
                Method::Post => Some(rest::WriteMethod::Create),
                Method::Put => Some(rest::WriteMethod::Replace),
                Method::Patch => Some(rest::WriteMethod::Change),
                Method::Delete => Some(rest::WriteMethod::Delete),
                _ => return Err(HttpError(405, format!("{method} is not allowed here"))),
            };
            let entry = context.dataset.clone().expect("set above");
            // With `preview=`, the shapes of a preview in place of the dataset's own; reads only.
            let _preview = match write {
                None => rest::PreviewScope::of(&entry, &params)?,
                Some(_) if params.get("preview").is_some() => {
                    return Err(HttpError::bad_request(
                        "A preview's shapes are read, not written with: leave out preview=",
                    ));
                }
                Some(_) => None,
            };
            let base = format!("{}/{}/api", origin(state, request), segments[0]);
            let timeout = context
                .limits
                .timeout(params.get("timeout").map(parse_timeout).transpose()?);
            let view = View::of(context);
            if let Some(write) = write {
                context.operation = Some(Operation::Update);
                if read_only {
                    return Err(HttpError(405, "The dataset is read-only".to_owned()));
                }
                if write == rest::WriteMethod::Delete {
                    may_write_as(state, request, context)?;
                } else {
                    may_write(state, request, context)?;
                }
                let body = read_bytes(request, state.options.max_body)?;
                let options = tripletrail::UpdateOptions {
                    base_iri: None,
                    using: None,
                    readable: context.readable.clone(),
                    writable: context.writable.clone(),
                    row_rules: context.row_rules.clone(),
                };
                return rest::write(
                    state,
                    &entry,
                    &segments[2..],
                    &params,
                    &base,
                    timeout,
                    &view,
                    &rest::WriteRequest {
                        method: write,
                        body: &body,
                        options: &options,
                        if_match: request_header(request, "If-Match"),
                    },
                );
            }
            context.operation = Some(Operation::Read);
            context.vary_accept = true;
            let accept = accepted(request);
            return rest::route(
                state,
                &entry,
                &segments[2..],
                &params,
                &base,
                timeout,
                &view,
                &accept,
            );
        }
        "mcp" => {
            if !state.options.mcp {
                return Err(HttpError(
                    404,
                    "No MCP endpoint: start the server with --mcp".to_owned(),
                ));
            }
            // Streamable HTTP without a stream: nothing to GET.
            if method != Method::Post {
                let mut reply = Reply::bytes(
                    405,
                    "text/plain; charset=utf-8",
                    b"POST JSON-RPC messages here".to_vec(),
                );
                reply.headers.push(("Allow", "POST".to_owned()));
                return Ok(reply);
            }
            // A browser page reaches the endpoint only from an origin --cors allows: MCP asks
            // servers to check Origin, against DNS rebinding. Agents send none.
            if let Some(o) = request_header(request, "Origin")
                && !state.options.cors
                && !state
                    .options
                    .cors_origins
                    .iter()
                    .any(|allowed| allowed == o)
            {
                return Err(HttpError(
                    403,
                    format!("Origin {o} may not use the MCP endpoint: see --cors"),
                ));
            }
            context.operation = Some(Operation::Read);
            let body = read_bytes(request, state.options.max_body)?;
            let entry = context.dataset.clone().expect("set above");
            let call = mcp::Call {
                state,
                context,
                entry: &entry,
                engine,
                origin: origin(state, request),
                ran: std::cell::RefCell::new(None),
            };
            let reply = mcp::answer(&call, &body);
            if let Some(text) = call.ran.into_inner() {
                context.operation = Some(Operation::Query);
                context.text_hash = Some(text_hash(&text));
                if state.options.slow_query.is_some() || context.keep_text {
                    context.text = Some(text);
                }
            }
            return reply;
        }
        "graphql" => {
            // A query by GET or POST; a mutation (#27) by POST only.
            let body = match method {
                Method::Get | Method::Head => None,
                Method::Post => Some(read_bytes(request, state.options.max_body)?),
                _ => return Err(HttpError(405, format!("{method} is not allowed here"))),
            };
            context.operation = Some(Operation::Read);
            // A browser that opens the endpoint gets the playground.
            if body.is_none()
                && !params.has("query")
                && state.options.ui
                && request_header(request, "Accept").is_some_and(|a| a.contains("text/html"))
            {
                let mut reply = Reply::empty(303);
                reply
                    .headers
                    .push(("Location", format!("/$/ui/graphql/{}", segments[0])));
                return Ok(reply);
            }
            let entry = context.dataset.clone().expect("set above");
            let base = format!("{}/{}/api", origin(state, request), segments[0]);
            let timeout = context
                .limits
                .timeout(params.get("timeout").map(parse_timeout).transpose()?);
            let kind = request_header(request, "Content-Type").unwrap_or("application/json");
            let graphql = rest::graphql::read_request(&params, body.as_deref().map(|b| (kind, b)))?;
            // A mutation writes as the REST API does: the same rights, quotas and read-only.
            let writes = match rest::graphql::mutates(&graphql) {
                None => None,
                Some(adds) => {
                    context.operation = Some(Operation::Update);
                    if body.is_none() {
                        return Err(HttpError(405, "Send a mutation with POST".to_owned()));
                    }
                    if read_only {
                        return Err(HttpError(405, "The dataset is read-only".to_owned()));
                    }
                    if adds {
                        may_write(state, request, context)?;
                    } else {
                        may_write_as(state, request, context)?;
                    }
                    Some(rest::graphql::Writes {
                        state: Arc::clone(state),
                        options: tripletrail::UpdateOptions {
                            base_iri: None,
                            using: None,
                            readable: context.readable.clone(),
                            writable: context.writable.clone(),
                            row_rules: context.row_rules.clone(),
                        },
                    })
                }
            };
            let view = View::of(context);
            return rest::graphql::answer(state, entry, graphql, &base, timeout, &view, writes);
        }
        "search" => {
            let what = match segments.get(2..) {
                Some(
                    [w @ ("labels" | "classes" | "results" | "sparql" | "values" | "similar")],
                ) => *w,
                _ => return Err(HttpError(404, "Not found".to_owned())),
            };
            let mut params = params;
            match method {
                Method::Get | Method::Head => {}
                // A long query vector (768 numbers make 17 KB) does not fit in a URL behind
                // most proxies: `similar` also takes its parameters as a form.
                Method::Post if what == "similar" => {
                    let kind = request_header(request, "Content-Type")
                        .and_then(|t| t.split(';').next())
                        .map(str::trim);
                    if !kind.is_some_and(|k| {
                        k.eq_ignore_ascii_case("application/x-www-form-urlencoded")
                    }) {
                        return Err(HttpError(
                            415,
                            "POST takes Content-Type: application/x-www-form-urlencoded".to_owned(),
                        ));
                    }
                    let body = read_body(request, state.options.max_body)?;
                    params = Params(
                        params
                            .0
                            .into_iter()
                            .chain(form_urlencoded::parse(body.as_bytes()).into_owned())
                            .collect(),
                    );
                }
                _ => return Err(HttpError(405, format!("{method} is not allowed here"))),
            }
            context.operation = Some(Operation::Read);
            let entry = context.dataset.clone().expect("set above");
            // With `preview=`, the shapes of a preview in place of the dataset's own.
            let _preview = rest::PreviewScope::of(&entry, &params)?;
            let timeout = context
                .limits
                .timeout(params.get("timeout").map(parse_timeout).transpose()?);
            let view = View::of(context);
            match what {
                "classes" => {
                    // Whether the caller may write the records (#82): the Records page edits
                    // only then.
                    let writable = !read_only
                        && view.rows.is_none()
                        && may_write_as(state, request, context).is_ok();
                    return rest::search::classes(state, &entry, &view, writable);
                }
                // Retrieval for GraphRAG (#219): the resources nearest a vector.
                "similar" => return similar::similar(&entry, &params, timeout, &view),
                "sparql" => return rest::search::query_text(state, &entry, &params, &view),
                "values" => {
                    return rest::search::values(state, &entry, &params, timeout, &view);
                }
                "results" => {
                    return rest::search::results(
                        state,
                        &entry,
                        &params,
                        &origin(state, request),
                        segments[0],
                        timeout,
                        &view,
                    );
                }
                _ => {}
            }
            return rest::search::labels(
                state,
                &entry,
                &params,
                &origin(state, request),
                segments[0],
                timeout,
                &view,
            );
        }
        "import" => {
            return import::route(
                state,
                request,
                &segments[2..],
                &params,
                context,
                engine,
                read_only,
            );
        }
        "tiles" if method == Method::Post && segments.get(2..) == Some(&["preview"][..]) => {
            // Proposed shapes to draw the map with (#146): reads, changes nothing.
            context.operation = Some(Operation::Read);
            let entry = context.dataset.clone().expect("set above");
            let json = content_type(request).as_deref() == Some("application/json");
            let body = read_body(request, state.options.max_body)?;
            let base = format!("{}/{}/tiles", origin(state, request), segments[0]);
            let view = View::of(context);
            return tiles::make_preview(state, &entry, &body, json, &base, &view);
        }
        "ogc" | "tiles" => {
            if !matches!(method, Method::Get | Method::Head) {
                return Err(HttpError(405, format!("{method} is not allowed here")));
            }
            context.operation = Some(Operation::Read);
            let entry = context.dataset.clone().expect("set above");
            let base = format!("{}/{}/ogc", origin(state, request), segments[0]);
            let timeout = context
                .limits
                .timeout(params.get("timeout").map(parse_timeout).transpose()?);
            let reader = if service == "tiles" {
                tripletrail::shapes::Reader::Map
            } else {
                tripletrail::shapes::Reader::OgcFeatures
            };
            ogc::refresh_collections(state, &entry, reader);
            let view = View::of(context);
            if service == "tiles" {
                let base = format!("{}/{}/tiles", origin(state, request), segments[0]);
                // The language of the map's labels (#260): `lang`, else the browser's first.
                let lang = params
                    .get("lang")
                    .map(str::to_owned)
                    .or_else(|| first_language(request))
                    .unwrap_or_else(|| "en".to_owned());
                return tiles::route(
                    state,
                    &entry,
                    &segments[2..],
                    &params,
                    &base,
                    timeout,
                    &view,
                    &lang,
                );
            }
            // A browser (`text/html` before any JSON type in `Accept`) gets the HTML pages.
            context.vary_accept = true;
            let html_first = accepted(request)
                .iter()
                .take_while(|t| !t.contains("json"))
                .any(|t| t == "text/html" || t == "application/xhtml+xml");
            // JSON-FG when it comes before the other JSON types.
            let fg_first = accepted(request)
                .iter()
                .find(|t| t.contains("json"))
                .is_some_and(|t| t == ogc::JSON_FG);
            return ogc::route(
                state,
                &entry,
                &segments[2..],
                &params,
                &base,
                timeout,
                html_first,
                fg_first,
                &view,
            );
        }
        "shacl" => return shacl_service(state, request, &params, &method, context, engine),
        "void" => {
            let entry = context.dataset.clone().expect("set above");
            return describe_datasets(
                state,
                request,
                &params,
                &method,
                &[(entry, context.readable.clone())],
            );
        }
        "patch" if method != Method::Post => {
            return Err(HttpError(
                405,
                "POST an RDF Patch (Content-Type: application/rdf-patch)".to_owned(),
            ));
        }
        "data" | "get" => {
            if service == "get" && !matches!(method, Method::Get | Method::Head) {
                return Err(HttpError(405, "Read-only endpoint".to_owned()));
            }
            let reading = matches!(method, Method::Get | Method::Head);
            if !reading && read_only {
                return Err(HttpError(405, "The dataset is read-only".to_owned()));
            }
            context.operation = Some(if reading {
                Operation::Read
            } else {
                Operation::Upload
            });
            if method == Method::Delete {
                // Removing a graph is how a dataset gets back under its quota.
                may_write_as(state, request, context)?;
            } else if !reading {
                may_write(state, request, context)?;
            }
            return graph_store(
                engine,
                request,
                &method,
                &params,
                context.limits.max_upload,
                context.readable.as_ref(),
                context.writable.as_ref(),
                row_view(engine, context)?,
                context.row_rules.as_ref(),
            );
        }
        _ => {}
    }

    protocol(
        state,
        request,
        &mut params,
        context,
        engine,
        service,
        path,
        read_only,
    )
}

/// The SPARQL Protocol on `engine` (a dataset's, or a workspace's scratch store): the
/// operation is in the query string, a form body or the body itself.
#[allow(clippy::too_many_arguments)]
pub(crate) fn protocol(
    state: &State,
    request: &mut Request,
    params: &mut Params,
    context: &mut Context,
    engine: &Engine,
    service: &str,
    path: &str,
    read_only: bool,
) -> Result<Reply, HttpError> {
    let method = request.method().clone();
    let mut query = None;
    let mut update = None;
    if method == Method::Post {
        // The protocol's bodies are UTF-8.
        if let Some(charset) = request_header(request, "Content-Type").and_then(charset)
            && !charset.eq_ignore_ascii_case("utf-8")
            && !charset.eq_ignore_ascii_case("utf8")
        {
            return Err(HttpError(
                415,
                format!("The body must be UTF-8, not {charset}"),
            ));
        }
        match content_type(request).as_deref() {
            // Fuseki's RDF Patch service (#831): the patch, as the update it makes.
            Some("application/rdf-patch") if service == "patch" => {
                let patch = read_body(request, state.options.max_body)?;
                update = Some(rdf_patch::to_update(&patch).map_err(|e| {
                    HttpError::bad_request(format!("The RDF Patch does not parse: {e}"))
                })?)
            }
            _ if service == "patch" => {
                return Err(HttpError(
                    415,
                    "The patch service takes an RDF Patch: Content-Type: application/rdf-patch"
                        .to_owned(),
                ));
            }
            Some("application/sparql-query") => {
                query = Some(read_body(request, state.options.max_body)?)
            }
            Some("application/sparql-update") => {
                update = Some(read_body(request, state.options.max_body)?)
            }
            Some("application/x-www-form-urlencoded") => {
                let body = read_body(request, state.options.max_body)?;
                params
                    .0
                    .extend(form_urlencoded::parse(body.as_bytes()).into_owned());
            }
            other => {
                return Err(HttpError(
                    415,
                    format!("Unsupported content type {}", other.unwrap_or("(none)")),
                ));
            }
        }
    } else if method != Method::Get && method != Method::Head {
        return Err(HttpError(405, format!("{method} is not allowed here")));
    }
    for name in ["query", "update"] {
        if params.all(name).count() > 1 {
            return Err(HttpError::bad_request(format!(
                "More than one {name} parameter"
            )));
        }
    }
    if (query.is_some() && params.has("query")) || (update.is_some() && params.has("update")) {
        return Err(HttpError::bad_request(
            "The operation is both in the body and in a parameter",
        ));
    }
    let query = query.or_else(|| params.get("query").map(str::to_owned));
    let update = update.or_else(|| params.get("update").map(str::to_owned));
    // The protocol's dataset: default-graph-uri and named-graph-uri for a query,
    // using-graph-uri and using-named-graph-uri for an update.
    let (default_graphs, named_graphs) = (
        iri_params(params, "default-graph-uri")?,
        iri_params(params, "named-graph-uri")?,
    );
    let (using_graphs, using_named_graphs) = (
        iri_params(params, "using-graph-uri")?,
        iri_params(params, "using-named-graph-uri")?,
    );
    if update.is_some() && !(default_graphs.is_empty() && named_graphs.is_empty()) {
        return Err(HttpError::bad_request(
            "default-graph-uri and named-graph-uri are for queries; updates take using-graph-uri \
             and using-named-graph-uri",
        ));
    }
    if query.is_some() && !(using_graphs.is_empty() && using_named_graphs.is_empty()) {
        return Err(HttpError::bad_request(
            "using-graph-uri and using-named-graph-uri are for updates; queries take \
             default-graph-uri and named-graph-uri",
        ));
    }
    // Relative IRIs resolve against the endpoint's URL.
    let base_iri = format!("{}{path}", origin(state, request));
    let mut query_options = if default_graphs.is_empty() && named_graphs.is_empty() {
        tripletrail::QueryOptions::default()
    } else {
        let mut dataset = tripletrail::oxigraph::sparql::QueryDataset::new();
        dataset.set_default_graph(default_graphs.into_iter().map(GraphName::from).collect());
        dataset.set_available_named_graphs(
            named_graphs
                .into_iter()
                .map(NamedOrBlankNode::from)
                .collect(),
        );
        tripletrail::QueryOptions {
            dataset: Some(dataset),
            ..Default::default()
        }
    };
    query_options.base_iri = Some(base_iri.clone());
    query_options.readable = context.readable.clone();
    query_options.row_rules = context.row_rules.clone();
    if context.row_rules.is_some() && query.is_none() && update.is_none() {
        return Err(HttpError(
            403,
            "Row-level rules limit what you see of this dataset: only SPARQL queries and updates are \
             available to you"
                .to_owned(),
        ));
    }
    let update_options = tripletrail::UpdateOptions {
        base_iri: Some(base_iri),
        using: (!(using_graphs.is_empty() && using_named_graphs.is_empty()))
            .then_some((using_graphs, using_named_graphs)),
        readable: context.readable.clone(),
        writable: context.writable.clone(),
        row_rules: context.row_rules.clone(),
    };
    // A request may ask for another timeout, up to the dataset's cap: with `?timeout=`, or
    // with `#pragma tt.timeout` in its text (#238; the parameter wins).
    context.hints = query
        .as_deref()
        .or(update.as_deref())
        .and_then(|t| tripletrail::Hints::parse(t).ok())
        .filter(|h| !h.is_empty());
    let hinted = context.hints.and_then(|h| h.timeout);
    let timeout = context.limits.timeout(
        params
            .get("timeout")
            .map(parse_timeout)
            .transpose()?
            .or(hinted),
    );
    let asked = params
        .get("query-memory")
        .map(|v| {
            match limits::bytes("query-memory", &serde_json::Value::String(v.to_owned())) {
                // 0 would mean no limit of the query's own, lifting the dataset's.
                Ok(0) => Err(HttpError::bad_request(
                    "query-memory must be at least one byte".to_owned(),
                )),
                bytes => bytes.map_err(HttpError::bad_request),
            }
        })
        .transpose()?;
    let limited;
    let engine = if asked.is_some() {
        limited = with_query_memory(engine.clone(), &context.limits, asked);
        &limited
    } else {
        engine
    };
    if let Some(text) = query.as_deref().or(update.as_deref()) {
        context.text_hash = Some(text_hash(text));
        if state.options.slow_query.is_some() || context.keep_text {
            context.text = Some(text.to_owned());
        }
    }
    match (service, query, update) {
        ("query" | "sparql" | "explain", Some(q), None) => {
            context.operation = Some(Operation::Query);
            if service == "explain" {
                let (engine, _deadline) = with_deadline(engine, timeout);
                let json = match (&context.row_rules, &context.readable) {
                    // Under row rules (#202), the row count of the query as it would run.
                    (Some(_), _) => engine.explain_restricted(&q, query_options)?,
                    (None, Some(readable)) => engine.explain_readable(&q, readable)?,
                    (None, None) => engine.explain(&q)?,
                };
                return Ok(Reply::bytes(200, "application/json", json.into_bytes()));
            }
            let budget = RowBudget::of(params, context.limits.max_rows)?;
            run_query(engine, &q, request, params, timeout, budget, query_options)
        }
        ("update" | "sparql" | "patch", None, Some(u)) => {
            if method != Method::Post {
                return Err(HttpError(405, "Updates must be POSTed".to_owned()));
            }
            context.operation = Some(Operation::Update);
            if read_only {
                return Err(HttpError(405, "The dataset is read-only".to_owned()));
            }
            if let Some(gate) = context.update_gate.take() {
                gate.allowed?;
                context.access = Some(auth::Access {
                    read: true,
                    write: true,
                    ..Default::default()
                });
                if gate.changes > gate.max {
                    return Err(HttpError(
                        413,
                        format!(
                            "The overlay holds {} changed quads, more than the {} it may hold: \
                             export them (GET …/overlay/patch) and drop them \
                             (DELETE …/overlay/changes)",
                            gate.changes, gate.max
                        ),
                    ));
                }
            }
            may_write_as(state, request, context)?;
            update_within_quota(context, || tripletrail::Engine::update_only_removes(&u))?;
            let (engine, _deadline) = with_deadline(engine, timeout);
            #[cfg(feature = "otel")]
            otel::step("sparql.update", || {
                engine.update_with_options(&u, &update_options)
            })?;
            #[cfg(not(feature = "otel"))]
            engine.update_with_options(&u, &update_options)?;
            // 200 with a page, as Fuseki answers: clients written against Fuseki check for
            // 200 (the protocol allows 204 too).
            Ok(Reply::bytes(
                200,
                "text/html; charset=utf-8",
                UPDATE_SUCCEEDED.as_bytes().to_vec(),
            ))
        }
        (_, Some(_), Some(_)) => Err(HttpError::bad_request("Both a query and an update")),
        ("query" | "sparql" | "update", None, None)
            if matches!(method, Method::Get | Method::Head) =>
        {
            let name = context
                .dataset
                .as_ref()
                .map(|e| e.name.clone())
                .unwrap_or_default();
            describe_service(
                state,
                engine,
                request,
                params,
                path,
                &name,
                read_only,
                context.readable.as_ref(),
            )
        }
        _ => Err(HttpError::bad_request(format!(
            "Missing the {} parameter",
            if service == "update" {
                "update"
            } else {
                "query"
            }
        ))),
    }
}

/// The base IRI of [`Options::base_iris`] that `url` falls under (the longest), and its
/// dataset.
fn base_iri<'a>(state: &'a State, url: &str) -> Option<&'a (String, String)> {
    state
        .options
        .base_iris
        .iter()
        .filter(|(prefix, _)| url.starts_with(prefix.as_str()))
        .max_by_key(|(prefix, _)| prefix.len())
}

/// A link to `iri` in a page: the IRI itself when this server dereferences it.
fn local_href(state: &State, iri: &str) -> Option<String> {
    base_iri(state, iri).map(|_| iri.to_owned())
}

/// A request for none of the server's own paths: a resource under a base IRI (Linked Data),
/// or 404.
fn dereference(
    state: &Arc<State>,
    request: &Request,
    params: &Params,
    method: &Method,
    path: &str,
    context: &mut Context,
) -> Result<Reply, HttpError> {
    let url = format!("{}{path}", origin(state, request));
    let Some((_, dataset)) = base_iri(state, &url) else {
        return Err(HttpError(404, format!("No dataset or service at {path}")));
    };
    if !matches!(method, Method::Get | Method::Head) {
        return Err(HttpError(405, format!("{method} is not allowed here")));
    }
    let entry = state
        .registry
        .read_unpoisoned()
        .resolve(dataset)
        .ok_or_else(|| HttpError(404, format!("No dataset {dataset}")))?;
    check_access(
        state,
        context,
        &[dataset, entry.name.as_str()],
        matches!(method, Method::Get | Method::Head),
        matches!(method, Method::Get | Method::Head),
    )?;
    let engine = entry.engine.clone();
    use_dataset(state, context, &entry)?;
    context.operation = Some(Operation::Read);
    let readable = context.readable.clone();
    let view = View::of(context);
    // Under row rules (#202), one view without what they hide, for every lookup below.
    let rows = row_view(&engine, context)?;
    // `/op/BE1.ttl` is the Turtle document of `/op/BE1`, unless `/op/BE1.ttl` is itself a
    // resource (has triples) and `/op/BE1` is not.
    let (iri, suffix) = match linked_data::split_suffix(&url) {
        Some((stem, doc)) => {
            let full = NamedNode::new(url.as_str()).ok();
            let stem_node = NamedNode::new(stem).ok();
            let known = |n: &Option<NamedNode>| {
                n.as_ref().is_some_and(|n| {
                    let in_view = |q: &tripletrail::oxigraph::model::Quad| {
                        readable
                            .as_ref()
                            .is_none_or(|r| r.contains(q.graph_name.as_ref()))
                    };
                    match &rows {
                        Some(view) => view
                            .quads_for_pattern(Some(n.as_ref().into()), None, None, None)
                            .any(|q| in_view(&q)),
                        None => engine
                            .store()
                            .quads_for_pattern(Some(n.as_ref().into()), None, None, None)
                            .filter_map(Result::ok)
                            .any(|q| in_view(&q)),
                    }
                })
            };
            if !known(&stem_node) && known(&full) {
                (url.clone(), None)
            } else {
                (stem.to_owned(), Some(doc))
            }
        }
        None => (url.clone(), None),
    };
    let doc = match suffix {
        Some(doc) => doc,
        None => linked_data::negotiate(request, params)?,
    };
    if state.options.see_other && suffix.is_none() && params.0.is_empty() {
        return Ok(linked_data::see_other(&iri, doc));
    }
    let node = NamedNode::new(iri.as_str())
        .map_err(|e| HttpError::bad_request(format!("Invalid IRI {iri}: {e}")))?;
    let describe_path = format!("/{}/describe", entry.name);
    let href = |other: &str| {
        local_href(state, other).unwrap_or_else(|| {
            let query: String = form_urlencoded::Serializer::new(String::new())
                .append_pair("iri", other)
                .finish();
            format!("{describe_path}?{query}")
        })
    };
    let shapes = || shapes::projected(state, &entry, tripletrail::shapes::Reader::Describe, &view);
    linked_data::answer(
        request,
        &linked_data::Answer {
            engine: &engine,
            iri: node,
            doc,
            links: linked_data::Links::Suffix(iri.clone()),
            attached: entry.attached,
            href: &href,
            readable: readable.as_ref(),
            rows: rows.as_ref(),
            shapes: &shapes,
            ui: state.options.ui,
            dataset: &entry.name,
            basemap: state.options.map_tiles.as_deref().map(|t| {
                (
                    t,
                    state.options.map_attribution.as_deref().unwrap_or_default(),
                )
            }),
        },
    )
}

/// The SPARQL Service Description of the endpoint at `path`, for a `GET` without a query or
/// an update. A browser (`text/html` before any RDF type in `Accept`) is sent to the UI.
/// A caller that reads some graphs only (#201) sees only those listed (#203).
#[allow(clippy::too_many_arguments)]
fn describe_service(
    state: &State,
    engine: &Engine,
    request: &Request,
    params: &Params,
    path: &str,
    name: &str,
    read_only: bool,
    readable: Option<&tripletrail::ReadableGraphs>,
) -> Result<Reply, HttpError> {
    let html_first = accepted(request)
        .iter()
        .take_while(|t| RdfFormat::from_media_type(t).is_none())
        .any(|t| t == "text/html" || t == "application/xhtml+xml");
    if html_first && state.options.ui {
        let mut reply = Reply::empty(302);
        reply.headers.push(("Location", "/$/ui/".to_owned()));
        return Ok(reply);
    }
    // Any RDF format: the description has only default-graph triples.
    let format = rdf_format(request, params, RdfFormat::Turtle, true)?;
    let origin = origin(state, request);
    let endpoint = service_description::Endpoint {
        url: format!("{origin}{path}"),
        engine,
        read_only,
        void: void::dataset_iri(&origin, name),
        readable,
    };
    let graph = service_description::describe(&endpoint)?;
    let body = service_description::serialize(&graph, format)?;
    Ok(Reply::bytes(200, format.media_type(), body))
}

/// The VoID description of `entries` (`/.well-known/void`, `/{ds}/void`), in the RDF format
/// asked for (Turtle by default).
fn describe_datasets(
    state: &State,
    request: &Request,
    params: &Params,
    method: &Method,
    entries: &[(Arc<Entry>, Option<ReadableGraphs>)],
) -> Result<Reply, HttpError> {
    if !matches!(method, Method::Get | Method::Head) {
        return Err(HttpError(405, format!("{method} is not allowed here")));
    }
    let format = rdf_format(request, params, RdfFormat::Turtle, true)?;
    let origin = origin(state, request);
    let mut triples = Vec::new();
    for (entry, readable) in entries {
        triples.extend(void::describe(
            &origin,
            &entry.name,
            &entry.engine,
            readable.as_ref(),
        )?);
    }
    let body = service_description::serialize(&triples, format)?;
    Ok(Reply::bytes(200, format.media_type(), body))
}

/// `scheme://host[:port]` the client used, from `X-Forwarded-Proto` / `X-Forwarded-Host`
/// behind a proxy, else `Host`.
pub(crate) fn origin(state: &State, request: &Request) -> String {
    let direct = if is_https(state, request) {
        "https"
    } else {
        "http"
    };
    let scheme = request_header(request, "X-Forwarded-Proto")
        .and_then(|p| p.split(',').next())
        .map_or(direct, str::trim);
    let host = request_header(request, "X-Forwarded-Host")
        .and_then(|h| h.split(',').next())
        .or_else(|| request_header(request, "Host"))
        .map_or("localhost", str::trim);
    format!("{scheme}://{host}")
}

/// Refuses the request unless its caller may read the dataset (`names`: the name asked for,
/// perhaps an alias, then the dataset's own), and records what it may do. A caller that may
/// read only some graphs (#201) passes only where `partial_ok`: the services that keep to
/// those graphs. Only there may a caller write some graphs only.
///
/// Row rules (#202) are taken only where `rows_ok`: the SPARQL query endpoint, for now.
/// Elsewhere a caller they apply to is refused.
fn check_access(
    state: &State,
    context: &mut Context,
    names: &[&str],
    partial_ok: bool,
    rows_ok: bool,
) -> Result<(), HttpError> {
    let (Some(auth), Some(caller)) = (&state.auth, &context.caller) else {
        return Ok(());
    };
    let access = auth.access(caller, names);
    let name = names.last().copied().unwrap_or_default();
    if access.partial {
        if !partial_ok {
            return Err(auth::refuse(
                caller,
                &format!(
                    "You may read only some graphs of {name}: use the SPARQL query endpoint, \
                     the Graph Store Protocol or describe"
                ),
            ));
        }
        context.readable = Some(auth.readable_graphs(caller, names));
    } else if !access.read {
        return Err(auth::refuse(
            caller,
            &format!("No policy gives you access to {name}"),
        ));
    }
    // Writes to some graphs only where they are checked graph by graph; elsewhere they are
    // refused.
    if access.partial_write && partial_ok {
        context.writable = Some(auth.writable_graphs(caller, names));
    }
    if let Some((shapes, rules)) = auth.row_rules(caller, names) {
        if !rows_ok {
            return Err(auth::refuse(
                caller,
                &format!(
                    "Row-level rules limit what you see of {name}: use the SPARQL query \
                     endpoint"
                ),
            ));
        }
        let graphs: Option<Vec<GraphName>> = context.readable.as_ref().map(|r| {
            r.default_graph
                .then_some(GraphName::DefaultGraph)
                .into_iter()
                .chain(r.named.iter().cloned().map(GraphName::NamedNode))
                .collect()
        });
        context.row_rules = Some(tripletrail::RowRules {
            shapes,
            rules,
            agent: caller.agent.clone(),
            graphs,
        });
    }
    context.access = Some(access);
    Ok(())
}

/// What a caller sees of a dataset: the graphs it may read (#203; all without) and the row
/// rules that hide rows of them (#202; none without).
#[derive(Clone, Debug, Default)]
pub(crate) struct View {
    pub(crate) graphs: Option<ReadableGraphs>,
    pub(crate) rows: Option<tripletrail::RowRules>,
}

impl View {
    /// What `context`'s caller sees.
    pub(crate) fn of(context: &Context) -> Self {
        Self {
            graphs: context.readable.clone(),
            rows: context.row_rules.clone(),
        }
    }

    /// Whether anything is hidden.
    pub(crate) fn limited(&self) -> bool {
        self.graphs.is_some() || self.rows.is_some()
    }

    /// A key for what this view shows, for anything cached from a read under it; `None`
    /// when nothing is hidden.
    pub(crate) fn fingerprint(&self) -> Option<u64> {
        if !self.limited() {
            return None;
        }
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.graphs
            .as_ref()
            .map(ReadableGraphs::fingerprint)
            .hash(&mut hasher);
        self.rows
            .as_ref()
            .map(tripletrail::RowRules::fingerprint)
            .hash(&mut hasher);
        Some(hasher.finish())
    }

    /// Query options that read this view only.
    pub(crate) fn options(&self) -> tripletrail::QueryOptions {
        tripletrail::QueryOptions {
            readable: self.graphs.clone(),
            row_rules: self.rows.clone(),
            ..Default::default()
        }
    }
}

/// Whether `caller` may read the dataset; always without authentication. Not for a caller
/// with row rules (#202): what reads through this sees the whole dataset.
pub(crate) fn may_read(state: &State, caller: Option<&auth::Caller>, names: &[&str]) -> bool {
    match (&state.auth, caller) {
        (Some(auth), Some(caller)) => {
            auth.access(caller, names).read && auth.row_rules(caller, names).is_none()
        }
        _ => true,
    }
}

/// Whether `caller` may read the dataset or some of its graphs (#201), for the listings of
/// datasets: `Some(None)` all of it, `Some(Some(graphs))` only those graphs, `None` nothing.
pub(crate) fn may_list(
    state: &State,
    caller: Option<&auth::Caller>,
    names: &[&str],
) -> Option<Option<ReadableGraphs>> {
    match (&state.auth, caller) {
        (Some(auth), Some(caller)) => {
            let access = auth.access(caller, names);
            if (access.read || access.partial) && auth.row_rules(caller, names).is_some() {
                // Row rules (#202): listed, but as a view of nothing, so no count or graph
                // shows what they hide.
                Some(Some(ReadableGraphs::default()))
            } else if access.read {
                Some(None)
            } else if access.partial {
                Some(Some(auth.readable_graphs(caller, names)))
            } else {
                None
            }
        }
        _ => Some(None),
    }
}

/// Refuses a write unless the caller may write (see [`may_write_as`]) and the dataset is
/// within its disk and memory quotas.
fn may_write(state: &State, request: &Request, context: &Context) -> Result<(), HttpError> {
    may_write_as(state, request, context)?;
    within_quota(context)
}

/// Refuses a write that may add data while the dataset is over its disk or memory quota.
pub(crate) fn within_quota(context: &Context) -> Result<(), HttpError> {
    match &context.dataset {
        Some(entry) => limits::check_quota(entry, &context.limits),
        None => Ok(()),
    }
}

/// Refuses an update that may add data while the dataset is over its disk or memory quota.
/// `only_removes` (which parses the update) runs only when the dataset is over a quota.
pub(crate) fn update_within_quota(
    context: &Context,
    only_removes: impl FnOnce() -> bool,
) -> Result<(), HttpError> {
    match &context.dataset {
        Some(entry) => limits::check_quota_unless(entry, &context.limits, only_removes),
        None => Ok(()),
    }
}

/// Refuses a write unless the caller is in a writer or admin group (with authentication
/// configured) or, without it, no update token is set or the request carries the update or
/// the admin token.
pub(crate) fn may_write_as(
    state: &State,
    request: &Request,
    context: &Context,
) -> Result<(), HttpError> {
    if let Some(caller) = &context.caller {
        // A caller that may write some graphs (#201) is let through where `writable` is set:
        // the engine and the Graph Store Protocol check each graph written.
        return if context.access.is_some_and(|a| a.write) || context.writable.is_some() {
            Ok(())
        } else {
            Err(auth::refuse(
                caller,
                "Writing here needs a policy with sg:writeAllGraphs or sg:writeGraph, or the \
                 group tripletrail:writer",
            ))
        };
    }
    let Some(token) = &state.options.update_token else {
        return Ok(());
    };
    if admin::bearer_is(request, token)
        || state
            .options
            .admin_token
            .as_deref()
            .is_some_and(|t| admin::bearer_is(request, t))
    {
        return Ok(());
    }
    Err(HttpError(
        401,
        "Writes need the update token: Authorization: Bearer <token>".to_owned(),
    ))
}

/// `POST /{ds}/shacl` (#829), Fuseki's SHACL service: validates graph `graph` (`default`,
/// the default; `union`, all graphs; or a graph's IRI) against the shapes of the body, in any
/// RDF format by its Content-Type (Turtle without one), or only node `target`, and answers the
/// W3C validation report in the format asked for (Turtle by default).
fn shacl_service(
    state: &State,
    request: &mut Request,
    params: &Params,
    method: &Method,
    context: &mut Context,
    engine: &Engine,
) -> Result<Reply, HttpError> {
    use tripletrail::oxigraph::io::RdfParser;
    use tripletrail::oxigraph::model::{Graph, GraphNameRef, NamedNode, Term};
    if *method != Method::Post {
        return Err(HttpError(
            405,
            "The SHACL service takes the shapes by POST".to_owned(),
        ));
    }
    context.operation = Some(Operation::Read);
    // curl's --data-binary without a Content-Type says form data: the shapes are Turtle then.
    let media = content_type(request)
        .filter(|m| {
            !matches!(
                m.as_str(),
                "application/x-www-form-urlencoded" | "application/octet-stream" | "text/plain"
            )
        })
        .unwrap_or_else(|| "text/turtle".to_owned());
    let format = RdfFormat::from_media_type(&media)
        .ok_or_else(|| HttpError(415, format!("Unsupported content type {media}")))?;
    let out = rdf_format(request, params, RdfFormat::Turtle, false)?;
    let iri = |name: &str, value: &str| {
        NamedNode::new(value).map_err(|e| HttpError::bad_request(format!("{name}: {e}")))
    };
    let (graph, union) = match params.get("graph") {
        None | Some("default") => (None, false),
        Some("union") => (None, true),
        Some(g) => (Some(iri("graph", g)?), false),
    };
    if let Some(g) = &graph
        && !engine.store().contains_named_graph(g.as_ref())?
    {
        return Err(HttpError(404, format!("No graph {g}")));
    }
    let focus = params
        .get("target")
        .map(|t| iri("target", t).map(Term::from))
        .transpose()?;
    let body = read_body(request, state.options.max_body)?;
    let mut shapes = Graph::new();
    for triple in RdfParser::from_format(format).for_slice(body.as_bytes()) {
        let triple = triple.map_err(|e| HttpError::bad_request(format!("The shapes: {e}")))?;
        shapes.insert(&tripletrail::oxigraph::model::Triple::from(triple));
    }
    let asked = params.get("timeout").map(parse_timeout).transpose()?;
    let (engine, _deadline) = with_deadline(engine, context.limits.timeout(asked));
    let data = graph
        .as_ref()
        .map_or(GraphNameRef::DefaultGraph, |g| g.as_ref().into());
    let mut triples = Vec::new();
    engine
        .validate_graph_report(&shapes, data, union, focus.as_ref(), &mut |t| {
            triples.push(t)
        })
        .map_err(crate::reason::shacl_error)?;
    crate::reason::rdf_reply(out, &triples)
}

pub(crate) fn read_body(request: &mut Request, limit: Option<usize>) -> Result<String, HttpError> {
    String::from_utf8(read_bytes(request, limit)?)
        .map_err(|e| HttpError::bad_request(format!("Unreadable body: {e}")))
}

/// The request body, or 413 once it is longer than `limit` bytes (checked against
/// `Content-Length` first, then while reading, for chunked bodies).
pub(crate) fn read_bytes(
    request: &mut Request,
    limit: Option<usize>,
) -> Result<Vec<u8>, HttpError> {
    let too_large = |limit: usize| {
        HttpError(
            413,
            format!("The request body is larger than the limit of {limit} bytes"),
        )
    };
    if let Some(limit) = limit
        && request.body_length().is_some_and(|n| n > limit)
    {
        return Err(too_large(limit));
    }
    let mut body = Vec::with_capacity(request.body_length().unwrap_or(0).min(1 << 24));
    let reader = request.as_reader();
    let result = match limit {
        Some(limit) => reader.take(limit as u64 + 1).read_to_end(&mut body),
        None => reader.read_to_end(&mut body),
    };
    result.map_err(|e| HttpError::bad_request(format!("Unreadable body: {e}")))?;
    if let Some(limit) = limit
        && body.len() > limit
    {
        return Err(too_large(limit));
    }
    Ok(body)
}

/// A view of the store without what the caller's row rules hide (#202), when they apply:
/// compiled within the dataset's query timeout, as a query would be.
fn row_view(
    engine: &Engine,
    context: &Context,
) -> Result<Option<tripletrail::store::Snapshot>, HttpError> {
    let Some(rules) = &context.row_rules else {
        return Ok(None);
    };
    let (engine, _deadline) = with_deadline(engine, context.limits.timeout(None));
    Ok(Some(engine.row_view(rules)?))
}

/// `engine` with the query memory of a request that asked for `asked` bytes (#189), at most
/// the dataset's.
fn with_query_memory(engine: Engine, limits: &limits::Limits, asked: Option<u64>) -> Engine {
    match limits.query_memory(asked) {
        Some(bytes) => engine.with_query_memory(usize::try_from(bytes).unwrap_or(usize::MAX)),
        None => engine,
    }
}

pub(crate) fn with_deadline(engine: &Engine, timeout: Option<Duration>) -> (Engine, Deadline) {
    let Some(timeout) = timeout else {
        return (engine.clone(), Deadline::NONE);
    };
    let token = CancellationToken::new();
    let deadline = Deadline::after(timeout, token.clone());
    (engine.with_cancellation_token(token), deadline)
}

/// Media types from an `Accept` header, best first (by `q`, then order, with CSV and TSV after
/// the others of their weight).
fn accepted(request: &Request) -> Vec<String> {
    let Some(accept) = request_header(request, "Accept") else {
        return Vec::new();
    };
    let mut types: Vec<(f32, usize, String)> = accept
        .split(',')
        .enumerate()
        .filter_map(|(i, item)| {
            let mut parts = item.split(';');
            let media = parts.next()?.trim().to_ascii_lowercase();
            let q = parts
                .filter_map(|p| p.trim().strip_prefix("q="))
                .find_map(|q| q.parse::<f32>().ok())
                .unwrap_or(1.0);
            (q > 0.0 && !media.is_empty()).then_some((q, i, media))
        })
        .collect();
    // Among equals, CSV and TSV come last: they drop datatypes and language tags, and RDF4J's
    // clients list text/csv first with JSON and XML at the same weight.
    let lossy = |m: &str| matches!(m, "text/csv" | "text/tab-separated-values");
    types.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then(lossy(&a.2).cmp(&lossy(&b.2)))
            .then(a.1.cmp(&b.1))
    });
    types.into_iter().map(|(_, _, m)| m).collect()
}

/// The results format: `format`/`output` parameter, else the `Accept` header, else JSON.
fn results_format(request: &Request, params: &Params) -> Result<QueryResultsFormat, HttpError> {
    if let Some(f) = params.get("format").or_else(|| params.get("output")) {
        return match f.to_ascii_lowercase().as_str() {
            "json" => Ok(QueryResultsFormat::Json),
            "xml" => Ok(QueryResultsFormat::Xml),
            "csv" => Ok(QueryResultsFormat::Csv),
            "tsv" => Ok(QueryResultsFormat::Tsv),
            other => QueryResultsFormat::from_media_type(other)
                .ok_or_else(|| HttpError(406, format!("Unsupported results format {f}"))),
        };
    }
    let types = accepted(request);
    if types.is_empty() {
        return Ok(QueryResultsFormat::Json);
    }
    for t in &types {
        if let Some(f) = QueryResultsFormat::from_media_type(t) {
            return Ok(f);
        }
        match t.as_str() {
            "*/*" | "application/*" | "application/json" => return Ok(QueryResultsFormat::Json),
            "text/*" | "text/plain" => return Ok(QueryResultsFormat::Tsv),
            _ => {}
        }
    }
    Err(HttpError(
        406,
        format!("No supported results format in {}", types.join(", ")),
    ))
}

/// The GeoParquet media type: `SELECT` results as a GeoParquet file.
#[cfg(feature = "geoparquet")]
const PARQUET: &str = "application/vnd.apache.parquet";

/// Whether the request asks for `SELECT` results as GeoParquet (`format=parquet`, or the
/// Parquet media type first in `Accept`).
#[cfg(feature = "geoparquet")]
fn wants_geoparquet(request: &Request, params: &Params) -> bool {
    match params.get("format").or_else(|| params.get("output")) {
        Some(f) => matches!(
            f.to_ascii_lowercase().as_str(),
            "parquet" | "geoparquet" | PARQUET
        ),
        None => accepted(request).first().is_some_and(|t| t == PARQUET),
    }
}

#[cfg(not(feature = "geoparquet"))]
fn wants_geoparquet(_: &Request, _: &Params) -> bool {
    false
}

/// SELECT results as Jena's Thrift or Protobuf (#831), when `?format=` or the first type of
/// `Accept` asks for them.
fn wants_binary_results(request: &Request, params: &Params) -> Option<jena_binary::Encoding> {
    use jena_binary::Encoding;
    let by_name = |f: &str| match f {
        "thrift" | jena_binary::RESULTS_THRIFT => Some(Encoding::Thrift),
        "protobuf" | jena_binary::RESULTS_PROTOBUF => Some(Encoding::Protobuf),
        _ => None,
    };
    match params.get("format").or_else(|| params.get("output")) {
        Some(f) => by_name(&f.to_ascii_lowercase()),
        None => accepted(request).first().and_then(|t| by_name(t)),
    }
}

/// Streams SELECT rows as Thrift or Protobuf.
struct BinaryResults<W: std::io::Write> {
    writer: jena_binary::ResultsWriter,
    variables: Vec<tripletrail::oxigraph::model::Variable>,
    out: W,
}

impl<W: std::io::Write> BinaryResults<W> {
    fn start(
        encoding: jena_binary::Encoding,
        variables: Vec<tripletrail::oxigraph::model::Variable>,
        mut out: W,
    ) -> std::io::Result<Self> {
        let mut writer = jena_binary::ResultsWriter::new(encoding, &variables);
        out.write_all(&std::mem::take(&mut writer.out))?;
        Ok(Self {
            writer,
            variables,
            out,
        })
    }

    fn serialize(
        &mut self,
        row: &tripletrail::oxigraph::sparql::QuerySolution,
    ) -> std::io::Result<()> {
        let values: Vec<_> = self.variables.iter().map(|v| row.get(v)).collect();
        self.writer.row(&values).map_err(std::io::Error::other)?;
        self.out.write_all(&std::mem::take(&mut self.writer.out))
    }

    fn finish(mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

/// An RDF format for a graph: `format`/`output` parameter, else `Accept`, else `default`.
fn rdf_format(
    request: &Request,
    params: &Params,
    default: RdfFormat,
    quads: bool,
) -> Result<RdfFormat, HttpError> {
    let pick =
        |f: RdfFormat| (quads || !f.supports_datasets() || f == RdfFormat::TriG).then_some(f);
    if let Some(f) = params.get("format").or_else(|| params.get("output")) {
        let format = match f.to_ascii_lowercase().as_str() {
            "turtle" | "ttl" => Some(RdfFormat::Turtle),
            "nt" | "ntriples" | "n-triples" => Some(RdfFormat::NTriples),
            "nq" | "nquads" | "n-quads" => Some(RdfFormat::NQuads),
            "trig" => Some(RdfFormat::TriG),
            "xml" | "rdfxml" | "rdf/xml" => Some(RdfFormat::RdfXml),
            other => RdfFormat::from_media_type(other),
        };
        return format
            .and_then(pick)
            .ok_or_else(|| HttpError(406, format!("Unsupported RDF format {f}")));
    }
    let types = accepted(request);
    if types.is_empty() {
        return Ok(default);
    }
    for t in &types {
        if let Some(f) = RdfFormat::from_media_type(t).and_then(pick) {
            return Ok(f);
        }
        if matches!(t.as_str(), "*/*" | "text/*" | "application/*") {
            return Ok(default);
        }
    }
    Err(HttpError(
        406,
        format!("No supported RDF format in {}", types.join(", ")),
    ))
}

/// [`rdf_format`], with RDF/JSON and TriX (#831) too: for the Graph Store and the results
/// of CONSTRUCT and DESCRIBE.
fn graph_format(
    request: &Request,
    params: &Params,
    default: RdfFormat,
    quads: bool,
) -> Result<rdf_formats::Format, HttpError> {
    use rdf_formats::Format;
    // TriX and the binary formats hold one graph as well as a dataset, as TriG does.
    let pick = |f: Format| {
        (quads
            || !f.supports_datasets()
            || matches!(
                f,
                Format::Rdf(RdfFormat::TriG) | Format::TriX | Format::Thrift | Format::Protobuf
            ))
        .then_some(f)
    };
    if let Some(f) = params.get("format").or_else(|| params.get("output")) {
        if let Some(extra) = Format::from_name(f).or_else(|| match Format::from_media_type(f) {
            Some(Format::Rdf(_)) | None => None,
            extra => extra,
        }) {
            return pick(extra)
                .ok_or_else(|| HttpError(406, format!("Unsupported RDF format {f}")));
        }
        return rdf_format(request, params, default, quads).map(Format::Rdf);
    }
    for t in &accepted(request) {
        match Format::from_media_type(t) {
            Some(Format::Rdf(_)) => break,
            Some(extra) => {
                if let Some(f) = pick(extra) {
                    return Ok(f);
                }
            }
            None if matches!(t.as_str(), "*/*" | "text/*" | "application/*") => break,
            None => {}
        }
    }
    rdf_format(request, params, default, quads).map(Format::Rdf)
}

/// How many rows a query may answer (#189), and what happens past that.
#[derive(Clone, Copy, Debug, Default)]
struct RowBudget {
    /// `None`: no limit, and the rows stream as they are evaluated.
    max: Option<usize>,
    /// Answer the first `max` rows (with `X-Result-Incomplete`) instead of refusing.
    partial: bool,
}

impl RowBudget {
    /// The `max-rows` parameter, capped by the server's limit, and `partial`.
    pub(crate) fn of(params: &Params, limit: Option<usize>) -> Result<Self, HttpError> {
        let asked = params
            .get("max-rows")
            .map(|v| {
                v.trim()
                    .parse::<usize>()
                    .map_err(|_| HttpError::bad_request(format!("Invalid max-rows {v}")))
            })
            .transpose()?;
        let max = match (asked, limit) {
            (Some(a), Some(l)) => Some(a.min(l)),
            (a, l) => a.or(l),
        };
        let partial = params
            .get("partial")
            .is_some_and(|v| matches!(v, "" | "true" | "1"));
        Ok(Self { max, partial })
    }

    /// Reads up to `max + 1` rows of `rows` (all of them without a budget is not done here:
    /// the caller streams). `Ok((rows, cut))`: the rows to answer, and whether some were left
    /// out (`partial`); an error when there are more and `partial` is off.
    fn take<T, E>(
        self,
        max: usize,
        rows: impl Iterator<Item = Result<T, E>>,
    ) -> Result<(Vec<T>, bool), HttpError>
    where
        tripletrail::Error: From<E>,
    {
        let mut kept = Vec::new();
        for row in rows {
            let row = row.map_err(|e| HttpError::from(tripletrail::Error::from(e)))?;
            if kept.len() == max {
                if self.partial {
                    return Ok((kept, true));
                }
                return Err(HttpError(
                    413,
                    format!(
                        "The result has more than {max} rows, the most this server answers; \
                         add a LIMIT, or ask for the first {max} with partial=true"
                    ),
                ));
            }
            kept.push(row);
        }
        Ok((kept, false))
    }
}

fn run_query(
    engine: &Engine,
    query: &str,
    request: &Request,
    params: &Params,
    timeout: Option<Duration>,
    budget: RowBudget,
    options: tripletrail::QueryOptions,
) -> Result<Reply, HttpError> {
    // Results iterate on the thread that evaluated them: this one, which sends the body.
    let geoparquet = wants_geoparquet(request, params);
    #[cfg(not(feature = "geoparquet"))]
    let _ = geoparquet;
    let results_format = results_format(request, params);
    let graph_format = graph_format(request, params, RdfFormat::Turtle, false);
    let (engine, deadline) = with_deadline(engine, timeout);
    // A caller who reads only some graphs (#203) is not told whether the cache answered: a
    // miss after a hit would tell that a write happened, in graphs it may not read too.
    let limited = options.readable.is_some();
    let prepare = || {
        engine
            .query_with_cache_status(query, options)
            .map_err(HttpError::from)
    };
    #[cfg(feature = "otel")]
    let (results, cache) = otel::step("sparql.prepare", prepare)?;
    #[cfg(not(feature = "otel"))]
    let (results, cache) = prepare()?;
    let mut cut = false;
    // The first rows are evaluated before answering (the rest while the body is sent).
    let evaluate = || -> Result<(&'static str, Body, Option<usize>), HttpError> {
        Ok(match results {
            QueryResults::Boolean(value) => {
                let format = results_format?;
                let mut data = Vec::new();
                QueryResultsSerializer::from_format(format)
                    .serialize_boolean_to_writer(&mut data, value)?;
                let length = data.len();
                (
                    format.media_type(),
                    Box::new(std::io::Cursor::new(data)),
                    Some(length),
                )
            }
            #[cfg(feature = "geoparquet")]
            QueryResults::Solutions(solutions) if geoparquet => {
                // Columns are typed from all their values: the rows are read first.
                let names: Vec<String> = solutions
                    .variables()
                    .iter()
                    .map(|v| v.as_str().to_owned())
                    .collect();
                let rows = match budget.max {
                    Some(max) => {
                        let (rows, over) = budget.take(max, solutions)?;
                        cut = over;
                        rows
                    }
                    None => solutions
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(tripletrail::Error::from)?,
                };
                let rows: Vec<_> = rows.iter().map(|s| s.values().to_vec()).collect();
                // Written in full before answering, so an error becomes the status.
                let mut file = Vec::new();
                tripletrail::geoparquet::write_geoparquet(
                    &names,
                    &rows,
                    &mut file,
                    &tripletrail::geoparquet::ExportOptions::default(),
                )?;
                let length = file.len();
                (PARQUET, Box::new(std::io::Cursor::new(file)), Some(length))
            }
            QueryResults::Solutions(mut solutions) => {
                let binary = wants_binary_results(request, params);
                // Thrift and Protobuf are not among the formats `results_format` knows.
                let format = match binary {
                    Some(_) => QueryResultsFormat::Json,
                    None => results_format?,
                };
                let variables = solutions.variables().to_vec();
                // An answer on term ids (#11) is written from its ids, each term once.
                if budget.max.is_none()
                    && binary.is_none()
                    && let Some(answer) = solutions
                        .attachment()
                        .and_then(|a| Arc::clone(a).downcast::<tripletrail::IdSolutions>().ok())
                {
                    let sink = Sink::default();
                    if let Some(writer) = id_results::IdResultsWriter::start(
                        sink.clone(),
                        Arc::clone(&answer),
                        format,
                    )? {
                        let (body, length) = Pulled::start(
                            sink,
                            (0..answer.len()).map(Ok),
                            writer,
                            |w, &i| w.row(i),
                            |w| w.finish(),
                            deadline,
                        )?;
                        return Ok((format.media_type(), body, length));
                    }
                }
                // With a budget, the rows (at most one more than it) are evaluated before
                // answering, so going over it is a status, not a cut body. Without one, the rows
                // of the first chunk are, so an error (a timeout of an aggregate, say) still
                // becomes the status.
                let head = match budget.max {
                    Some(max) => {
                        let (rows, over) = budget.take(max, &mut solutions)?;
                        cut = over;
                        rows
                    }
                    None => Vec::new(),
                };
                let rest = budget
                    .max
                    .is_none()
                    .then_some(solutions)
                    .into_iter()
                    .flatten()
                    .map(|s| s.map_err(std::io::Error::other));
                let rows = head.into_iter().map(Ok).chain(rest);
                let sink = Sink::default();
                if let Some(encoding) = binary {
                    let serializer = BinaryResults::start(encoding, variables, sink.clone())?;
                    let (body, length) = Pulled::start(
                        sink,
                        rows,
                        serializer,
                        |s, row| s.serialize(row),
                        |s| s.finish(),
                        deadline,
                    )?;
                    let media = match encoding {
                        jena_binary::Encoding::Thrift => jena_binary::RESULTS_THRIFT,
                        jena_binary::Encoding::Protobuf => jena_binary::RESULTS_PROTOBUF,
                    };
                    return Ok((media, body, length));
                }
                let (body, length) = if format == QueryResultsFormat::Json {
                    // Large results: see `json_results`.
                    let serializer =
                        json_results::JsonSolutionsWriter::start(sink.clone(), &variables)?;
                    Pulled::start(
                        sink,
                        rows,
                        serializer,
                        |s, row| s.serialize(row),
                        |s| s.finish().map(drop),
                        deadline,
                    )?
                } else {
                    let serializer = QueryResultsSerializer::from_format(format)
                        .serialize_solutions_to_writer(sink.clone(), variables)?;
                    Pulled::start(
                        sink,
                        rows,
                        serializer,
                        |s, row| s.serialize(row),
                        |s| s.finish().map(drop),
                        deadline,
                    )?
                };
                (format.media_type(), body, length)
            }
            QueryResults::Graph(mut triples) => {
                let format = graph_format?;
                let head = match budget.max {
                    Some(max) => {
                        let (rows, over) = budget.take(max, &mut triples)?;
                        cut = over;
                        rows
                    }
                    None => Vec::new(),
                };
                let rest = budget
                    .max
                    .is_none()
                    .then_some(triples)
                    .into_iter()
                    .flatten()
                    .map(|t| t.map_err(std::io::Error::other));
                let sink = Sink::default();
                let serializer = rdf_formats::QuadWriter::new(format, sink.clone());
                let (body, length) = Pulled::start(
                    sink,
                    head.into_iter().map(Ok).chain(rest),
                    serializer,
                    |s, triple| s.serialize_triple(triple),
                    |s| s.finish().map(drop),
                    deadline,
                )?;
                (format.media_type(), body, length)
            }
        })
    };
    #[cfg(feature = "otel")]
    let (content_type, body, length) = otel::step("sparql.evaluate", evaluate)?;
    #[cfg(not(feature = "otel"))]
    let (content_type, body, length) = evaluate()?;
    // Whether the result cache answered (HIT), evaluated and kept it (MISS) or does not
    // cache this query (BYPASS).
    let mut headers = Vec::new();
    if !limited {
        headers.push(("X-Cache", cache.as_str().to_owned()));
    }
    if cut {
        headers.push((
            "X-Result-Incomplete",
            format!("max-rows={}", budget.max.unwrap_or_default()),
        ));
    }
    Ok(Reply {
        status: 200,
        content_type: Some(content_type.to_owned()),
        body,
        length,
        headers,
    })
}

/// Bytes a serializer writes, for [`Pulled`] to hand out.
#[derive(Clone, Default)]
struct Sink(Rc<RefCell<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A response body serialized from query results while it is being sent, on the thread
/// sending it (#186): results iterate on the thread that evaluated them, and running the
/// query on another thread cost two thread switches and a pipe per query.
struct Pulled {
    sink: Sink,
    /// Bytes of `sink` already read.
    read: usize,
    /// Serializes the next rows into `sink`; `false` once the body is complete.
    next: Option<Box<dyn FnMut() -> std::io::Result<bool>>>,
    /// The query's timeout runs until the body is sent.
    _deadline: Deadline,
}

/// Bytes a [`Pulled`] serializes at a time.
const PULLED_CHUNK: usize = 64 * 1024;

impl Pulled {
    /// Serializes the first chunk, so that an error in it becomes the response status, and
    /// the body with its length when the whole result fits in it.
    fn start<T: 'static, S: 'static>(
        sink: Sink,
        rows: impl Iterator<Item = std::io::Result<T>> + 'static,
        serializer: S,
        mut write: impl FnMut(&mut S, &T) -> std::io::Result<()> + 'static,
        finish: impl FnOnce(S) -> std::io::Result<()> + 'static,
        deadline: Deadline,
    ) -> Result<(Body, Option<usize>), HttpError> {
        let mut state = Some((rows, serializer, finish));
        let chunk = sink.clone();
        let mut next = move || -> std::io::Result<bool> {
            let Some((rows, serializer, _)) = &mut state else {
                return Ok(false);
            };
            while chunk.0.borrow().len() < PULLED_CHUNK {
                match rows.next() {
                    Some(row) => write(serializer, &row?)?,
                    None => {
                        let (_, serializer, finish) = state.take().expect("still serializing");
                        finish(serializer)?;
                        return Ok(false);
                    }
                }
            }
            Ok(true)
        };
        if !next().map_err(io_to_http)? {
            let data = sink.0.take();
            let length = data.len();
            return Ok((Box::new(std::io::Cursor::new(data)), Some(length)));
        }
        let body = Self {
            sink,
            read: 0,
            next: Some(Box::new(next)),
            _deadline: deadline,
        };
        Ok((Box::new(body), None))
    }
}

impl Read for Pulled {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        loop {
            {
                let data = self.sink.0.borrow();
                if self.read < data.len() {
                    let n = out.len().min(data.len() - self.read);
                    out[..n].copy_from_slice(&data[self.read..self.read + n]);
                    self.read += n;
                    return Ok(n);
                }
            }
            self.sink.0.borrow_mut().clear();
            self.read = 0;
            let Some(next) = &mut self.next else {
                return Ok(0);
            };
            // An error (a timeout, say) fails the body: the patched tiny_http then sends no
            // last chunk and closes the connection (#549), so a client sees a cut result, also
            // in CSV or N-Triples where a shorter body would read as a whole one.
            match next() {
                Ok(true) => {}
                Ok(false) => self.next = None,
                Err(e) => {
                    self.next = None;
                    return Err(std::io::Error::other(e));
                }
            }
        }
    }
}

/// An error from serializing results: the query's own error when evaluating the rows failed.
fn io_to_http(e: std::io::Error) -> HttpError {
    match e.downcast::<tripletrail::oxigraph::sparql::QueryEvaluationError>() {
        Ok(e) => tripletrail::Error::from(e).into(),
        Err(e) => e.into(),
    }
}

/// The graph a Graph Store Protocol request is about: `Some(graph)`, or `None` for the
/// whole dataset.
fn target_graph(params: &Params) -> Result<Option<GraphName>, HttpError> {
    if let Some(iri) = params.get("graph") {
        if iri == "default" {
            return Ok(Some(GraphName::DefaultGraph));
        }
        let node = NamedNode::new(iri)
            .map_err(|e| HttpError::bad_request(format!("Invalid graph IRI {iri}: {e}")))?;
        return Ok(Some(node.into()));
    }
    Ok(params.has("default").then_some(GraphName::DefaultGraph))
}

/// The parts of a Graph Store Protocol body: a form upload (multipart/form-data) may hold
/// several files, each giving its format by its Content-Type or its file name.
/// The parts of an upload, each in a format Oxigraph's parsers read: RDF/JSON and TriX
/// (#831) are handed on as N-Quads.
/// An upload's part: its format and its text.
type Part<'a> = (RdfFormat, std::borrow::Cow<'a, [u8]>);

fn upload_parts<'a>(request: &Request, body: &'a [u8]) -> Result<Vec<Part<'a>>, HttpError> {
    let media =
        content_type(request).ok_or_else(|| HttpError(415, "Missing Content-Type".to_owned()))?;
    let parts = if media == "multipart/form-data" {
        let boundary = request_header(request, "Content-Type")
            .and_then(boundary)
            .ok_or_else(|| HttpError::bad_request("multipart/form-data without a boundary"))?;
        multipart(body, boundary)?
    } else {
        let format = rdf_formats::Format::from_media_type(&media)
            .ok_or_else(|| HttpError(415, format!("Unsupported content type {media}")))?;
        vec![(format, body)]
    };
    parts
        .into_iter()
        .map(|(format, part)| {
            rdf_formats::as_oxigraph(format, part).map_err(HttpError::bad_request)
        })
        .collect()
}

/// Graph Store Protocol writes under row rules (#202): they name one graph, `PUT` and
/// `DELETE` remove only the quads the caller sees of it, and all of it goes through the checks
/// of updates under row rules ([`Engine::write_graph_rows`]). A graph with nothing visible in
/// it is absent: `DELETE` answers 404, and a write to it 201.
fn graph_store_rows(
    engine: &Engine,
    request: &mut Request,
    method: &Method,
    max_upload: Option<usize>,
    target: Option<GraphName>,
    view: tripletrail::store::Snapshot,
    rules: &tripletrail::RowRules,
) -> Result<Reply, HttpError> {
    let Some(graph) = target else {
        return Err(HttpError(
            403,
            "Row-level rules limit what you see of this dataset: name one graph with ?graph= \
             or ?default"
                .to_owned(),
        ));
    };
    let visible = graph.is_default_graph()
        || view
            .quads_for_pattern(None, None, None, Some(graph.as_ref()))
            .next()
            .is_some();
    match method {
        Method::Put | Method::Post => {
            let body = read_bytes(request, max_upload)?;
            let mut quads = Vec::new();
            for (format, part) in upload_parts(request, &body)? {
                let mut parser = RdfParser::from_format(format).rename_blank_nodes();
                if let GraphName::NamedNode(n) = &graph {
                    parser = parser
                        .with_base_iri(n.as_str())
                        .map_err(|e| HttpError::bad_request(e.to_string()))?;
                }
                for quad in parser.with_default_graph(graph.clone()).for_slice(&part) {
                    let quad = quad.map_err(|e| HttpError::bad_request(e.to_string()))?;
                    if quad.graph_name != graph {
                        return Err(HttpError(
                            403,
                            format!("You may not write the graph {}", quad.graph_name),
                        ));
                    }
                    quads.push(quad);
                }
            }
            let clear = (*method == Method::Put).then(|| graph.clone());
            engine.write_graph_rows(rules, clear, quads)?;
            Ok(Reply::empty(if visible { 204 } else { 201 }))
        }
        Method::Delete => {
            if !visible {
                return Err(HttpError(404, format!("No graph {graph}")));
            }
            engine.write_graph_rows(rules, Some(graph), Vec::new())?;
            Ok(Reply::empty(204))
        }
        other => Err(HttpError(405, format!("{other} is not allowed here"))),
    }
}

/// Graph Store uploads of at most this many quads are written as one transaction, which keeps
/// the spatial index and cached counts (#702); larger ones are bulk-loaded.
const SMALL_UPLOAD: usize = 100_000;

#[allow(clippy::too_many_arguments)]
pub(crate) fn graph_store(
    engine: &Engine,
    request: &mut Request,
    method: &Method,
    params: &Params,
    max_upload: Option<usize>,
    readable: Option<&tripletrail::ReadableGraphs>,
    writable: Option<&tripletrail::ReadableGraphs>,
    rows: Option<tripletrail::store::Snapshot>,
    row_rules: Option<&tripletrail::RowRules>,
) -> Result<Reply, HttpError> {
    let target = target_graph(params)?;
    // Under row rules (#202) a caller reads a view without what they hide; a named graph with
    // nothing visible in it is absent. Writes come further down.
    let reading = matches!(method, Method::Get | Method::Head);
    let (rows, row_view) = match rows {
        Some(view) if !reading => (None, Some(view)),
        rows => (rows, None),
    };
    if let Some(view) = rows {
        let readable = readable.cloned();
        let may_read = move |g: GraphNameRef<'_>| readable.as_ref().is_none_or(|r| r.contains(g));
        return match target {
            Some(graph) => {
                let visible = may_read(graph.as_ref())
                    && (graph.is_default_graph()
                        || view
                            .quads_for_pattern(None, None, None, Some(graph.as_ref()))
                            .next()
                            .is_some());
                if !visible && !graph.is_default_graph() {
                    return Err(HttpError(404, format!("No graph {graph}")));
                }
                let format = graph_format(request, params, RdfFormat::Turtle, false)?;
                Reply::stream(format.media_type(), move |out| {
                    let mut serializer = rdf_formats::QuadWriter::new(format, out);
                    if visible {
                        for quad in view.quads_for_pattern(None, None, None, Some(graph.as_ref())) {
                            serializer.serialize_triple(quad.as_ref())?;
                        }
                    }
                    serializer.finish()?;
                    Ok(())
                })
            }
            None => {
                let format = graph_format(request, params, RdfFormat::NQuads, true)?;
                if !format.supports_datasets() {
                    return Err(HttpError(406, "The whole dataset needs a quad format (N-Quads, TriG, TriX); add ?default or ?graph=<iri> for one graph".to_owned()));
                }
                Reply::stream(format.media_type(), move |out| {
                    let mut serializer = rdf_formats::QuadWriter::new(format, out);
                    for quad in view.quads_for_pattern(None, None, None, None) {
                        if may_read(quad.graph_name.as_ref()) {
                            serializer.serialize_quad(&quad)?;
                        }
                    }
                    serializer.finish()?;
                    Ok(())
                })
            }
        };
    }
    // With write access to some graphs only (#201), a write names one of them.
    if let Some(writable) = writable
        && !matches!(method, Method::Get | Method::Head)
        && !target
            .as_ref()
            .is_some_and(|g| writable.contains(g.as_ref()))
    {
        return Err(HttpError(
            403,
            match &target {
                Some(g) => format!("You may not write the graph {g}"),
                None => "You may write only some graphs: name one with ?graph=".to_owned(),
            },
        ));
    }
    if let Some(view) = row_view {
        let rules = row_rules.ok_or_else(|| {
            HttpError(
                403,
                "Row-level rules limit what you see of this dataset".to_owned(),
            )
        })?;
        return graph_store_rows(engine, request, method, max_upload, target, view, rules);
    }
    let store = engine.store();
    // With access to some graphs only (#201), the others look empty, and named ones absent.
    let readable = readable.cloned();
    let may_read = move |g: GraphNameRef<'_>| readable.as_ref().is_none_or(|r| r.contains(g));
    let exists = |g: &GraphName| -> Result<bool, HttpError> {
        Ok(match g {
            GraphName::DefaultGraph => true,
            GraphName::NamedNode(n) => store.contains_named_graph(n)?,
            GraphName::BlankNode(b) => store.contains_named_graph(b)?,
        })
    };
    match method {
        Method::Get | Method::Head => match target {
            Some(graph) => {
                let visible = may_read(graph.as_ref());
                if !exists(&graph)? || (!visible && !graph.is_default_graph()) {
                    return Err(HttpError(404, format!("No graph {graph}")));
                }
                let format = graph_format(request, params, RdfFormat::Turtle, false)?;
                let store = store.clone();
                Reply::stream(format.media_type(), move |out| {
                    let mut serializer = rdf_formats::QuadWriter::new(format, out);
                    let quads = store
                        .quads_for_pattern(None, None, None, Some(graph.as_ref()))
                        .take_while(|_| visible);
                    for quad in quads {
                        let quad = quad.map_err(std::io::Error::other)?;
                        serializer.serialize_triple(quad.as_ref())?;
                    }
                    serializer.finish()?;
                    Ok(())
                })
            }
            None => {
                let format = graph_format(request, params, RdfFormat::NQuads, true)?;
                if !format.supports_datasets() {
                    return Err(HttpError(406, "The whole dataset needs a quad format (N-Quads, TriG, TriX); add ?default or ?graph=<iri> for one graph".to_owned()));
                }
                let store = store.clone();
                Reply::stream(format.media_type(), move |out| {
                    let mut serializer = rdf_formats::QuadWriter::new(format, out);
                    for quad in store.iter() {
                        let quad = quad.map_err(std::io::Error::other)?;
                        if may_read(quad.graph_name.as_ref()) {
                            serializer.serialize_quad(&quad)?;
                        }
                    }
                    serializer.finish()?;
                    Ok(())
                })
            }
        },
        Method::Put | Method::Post => {
            let body = read_bytes(request, max_upload)?;
            let parts = upload_parts(request, &body)?;
            let replace = *method == Method::Put;
            let created = match &target {
                Some(g) => !exists(g)?,
                None => false,
            };
            let parser = |format: RdfFormat| -> Result<RdfParser, HttpError> {
                let mut parser = RdfParser::from_format(format);
                if let Some(GraphName::NamedNode(n)) = &target {
                    parser = parser
                        .with_base_iri(n.as_str())
                        .map_err(|e| HttpError::bad_request(e.to_string()))?;
                }
                Ok(match target.clone() {
                    Some(graph) => parser.with_default_graph(graph),
                    None => parser,
                })
            };
            // The whole body is parsed before anything changes, so a body with an error
            // leaves the data as it was: PUT does not empty the graph, POST adds nothing.
            // A small body's quads are kept, and written in one transaction (#702).
            let mut small: Option<Vec<tripletrail::oxigraph::model::Quad>> = Some(Vec::new());
            for (format, part) in &parts {
                // Each part's blank nodes are its own, as on Fuseki (#840); a large body is
                // scoped the same way by `Engine::load` below.
                for quad in parser(*format)?.rename_blank_nodes().for_slice(part) {
                    let quad = quad.map_err(|e| HttpError::bad_request(e.to_string()))?;
                    // With write access to some graphs only (#201), a quad body may not name
                    // another graph than the target.
                    if writable.is_some() && Some(&quad.graph_name) != target.as_ref() {
                        return Err(HttpError(
                            403,
                            format!("You may not write the graph {}", quad.graph_name),
                        ));
                    }
                    if let Some(quads) = &mut small {
                        if quads.len() < SMALL_UPLOAD {
                            quads.push(quad);
                        } else {
                            small = None;
                        }
                    }
                }
            }
            if let Some(quads) = small {
                let replace = match (&target, replace) {
                    (_, false) => tripletrail::Replace::Nothing,
                    (Some(g), true) => tripletrail::Replace::Graph(g.as_ref()),
                    (None, true) => tripletrail::Replace::All,
                };
                engine.write_quads(replace, quads)?;
                return Ok(Reply::empty(if created { 201 } else { 204 }));
            }
            if replace {
                match &target {
                    Some(GraphName::DefaultGraph) => {
                        store.clear_graph(GraphNameRef::DefaultGraph)?
                    }
                    Some(GraphName::NamedNode(n)) => store.clear_graph(n)?,
                    Some(GraphName::BlankNode(b)) => store.clear_graph(b)?,
                    None => store.clear()?,
                }
                engine.invalidate();
            }
            for (format, part) in parts {
                engine.load(parser(format)?, &part[..])?;
            }
            Ok(Reply::empty(if created { 201 } else { 204 }))
        }
        Method::Delete => match target {
            Some(GraphName::DefaultGraph) => {
                engine.write_quads(
                    tripletrail::Replace::Graph(GraphNameRef::DefaultGraph),
                    Vec::new(),
                )?;
                Ok(Reply::empty(204))
            }
            Some(GraphName::NamedNode(n)) => {
                if !store.contains_named_graph(&n)? {
                    return Err(HttpError(404, format!("No graph <{}>", n.as_str())));
                }
                if matches!(store, tripletrail::store::Dataset::TripleTrail(_)) {
                    engine
                        .write_quads(tripletrail::Replace::Graph(n.as_ref().into()), Vec::new())?;
                } else {
                    store.remove_named_graph(&NamedOrBlankNode::from(n))?;
                    engine.invalidate();
                }
                Ok(Reply::empty(204))
            }
            _ => Err(HttpError(
                405,
                "DELETE needs ?default or ?graph=<iri>".to_owned(),
            )),
        },
        other => Err(HttpError(405, format!("{other} is not allowed here"))),
    }
}

/// Turns off Nagle's algorithm on the listening socket, which accepted connections inherit.
/// tiny_http writes a response in several pieces; with Nagle's algorithm on, every piece after
/// the first waits for the client to acknowledge the previous one, and clients delay their
/// acknowledgements by up to 40 ms on a kept-alive connection: a response of a few kilobytes
/// then took 40 ms more than the query.
#[cfg(unix)]
fn set_nodelay(listener: &std::net::TcpListener) {
    use std::os::fd::AsRawFd;
    let one: libc::c_int = 1;
    // SAFETY: a valid socket descriptor, and a pointer to an int that lives across the call.
    unsafe {
        libc::setsockopt(
            listener.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_NODELAY,
            (&raw const one).cast::<libc::c_void>(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

#[cfg(not(unix))]
fn set_nodelay(_listener: &std::net::TcpListener) {}
