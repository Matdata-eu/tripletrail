use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tripletrail::Engine;
use tripletrail::oxigraph::io::{RdfFormat, RdfParser};
use tripletrail::oxigraph::model::{NamedNode, NamedNodeRef};
use tripletrail::oxigraph::store::Store;
use tripletrail::spatial::IndexedPredicates;
use tripletrail_server::{Mode, Options, Server, open_dataset};

use tripletrail_server::lift;

/// mimalloc rather than the system allocator (#934): with glibc, rows allocated by the thread
/// that evaluates a WHERE and freed by the thread that inserts them contend on the arena locks
/// (an INSERT of 7.4M quads took 44 s, against 12 s).
#[cfg(feature = "mimalloc")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;
mod offline;

/// The default base map of the maps: OpenStreetMap's standard tiles.
const OSM_TILES: &str = "https://tile.openstreetmap.org/{z}/{x}/{y}.png";
const OSM_ATTRIBUTION: &str =
    "&copy; <a href=\"https://www.openstreetmap.org/copyright\">OpenStreetMap</a> contributors";

/// The version, with the edition (`0.1.0 (enterprise)`) in the enterprise build (#734).
const VERSION: &str = {
    #[cfg(feature = "enterprise")]
    {
        concat!(env!("CARGO_PKG_VERSION"), " (enterprise)")
    }
    #[cfg(not(feature = "enterprise"))]
    {
        env!("CARGO_PKG_VERSION")
    }
};

#[derive(Parser)]
#[command(
    name = "tripletrail",
    version = VERSION,
    about = "TripleTrail: an RDF store with GeoSPARQL, PostGIS-style functions, routing and a Fuseki-compatible server"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serves datasets over the SPARQL 1.1 Protocol and the Graph Store Protocol, with an
    /// admin API under /$/ and a web UI at /$/ui/. An option that names an environment
    /// variable in brackets can also be set with it.
    Serve(Box<ServeArgs>),
    /// Exits 0 when the server at `--url` answers 200 on /$/ready, 1 otherwise: a health check
    /// for images without curl (the Docker image's HEALTHCHECK).
    Health {
        /// The server's address.
        #[arg(
            long,
            env = "TRIPLETRAIL_HEALTH_URL",
            default_value = "http://127.0.0.1:7878"
        )]
        url: String,
        /// When the server has TLS on (the same variable as `serve --tls-cert`), checks it over
        /// HTTPS. The certificate is not verified: the check only asks whether the server answers.
        #[arg(long, env = "TRIPLETRAIL_TLS_CERT", hide = true)]
        tls_cert: Option<PathBuf>,
    },
    /// Prints the SHA-256 hash of an API key, for the `sha256` of an `auth.apiKeys` entry in
    /// the --auth-config file. Reads the key from standard input when not given, so it stays
    /// out of the shell history.
    HashKey {
        /// The key (e.g. from `openssl rand -hex 24`).
        key: Option<String>,
    },
    /// Prints a hash of a password, for the `password` of an `auth.users` entry in the
    /// --auth-config file (users who sign in with HTTP Basic, as on Fuseki). Reads the
    /// password from standard input when not given, so it stays out of the shell history.
    HashPassword {
        /// The password.
        password: Option<String>,
    },
    /// Prints the `[users]` of a Fuseki `shiro.ini` as the `auth.users` of an --auth-config
    /// file: the users' roles become groups, and passwords written in the clear are hashed.
    ImportShiro {
        /// The shiro.ini file.
        file: PathBuf,
    },
    /// Prints the rules of a Jena rule file (GenericRuleReasoner) as SHACL-AF SPARQL rules in
    /// Turtle, for `tripletrail infer --shapes` or a dataset's shapes. Rules without a SPARQL form
    /// are left out and listed on standard error.
    ImportJenaRules {
        /// The rule file.
        file: PathBuf,
    },
    /// Bulk-loads files into an on-disk dataset.
    Load {
        /// Directory of the datasets.
        #[arg(long)]
        location: PathBuf,
        /// Storage, as for `serve`.
        #[arg(long, value_enum, default_value_t = StoreKind::TripleTrail)]
        store: StoreKind,
        /// The dataset to load into.
        #[arg(long, default_value = "ds")]
        dataset: String,
        /// Named graph for the triples (default graph if omitted).
        #[arg(long)]
        graph: Option<String>,
        #[command(flatten)]
        parquet: ParquetArgs,
        /// Memory a load may use to sort its quads (e.g. 32G; suffixes K, M, G, T). A
        /// larger load sorts them on disk and writes the indexes straight to files. No limit
        /// by default: everything is sorted in memory. Sorting on disk briefly needs about 1.3
        /// times the size of the finished store (20M triples: 1.8 GB for a 1.4 GB store); at
        /// most four index orders are sorted at once.
        #[arg(long, env = "TRIPLETRAIL_MEMORY_LIMIT", value_parser = parse_size)]
        memory_limit: Option<usize>,
        /// Keep WKT literals only in binary form when their text can be written back from
        /// it exactly, which saves about the size of their text on disk; results show the
        /// same text, but queries that output many WKT texts are slower (each is written
        /// back: about 2 µs per geometry); spatial work is unaffected. The default for a new
        /// dataset (#152); this turns it on for an existing one. Kept with the dataset
        /// (TripleTrail storage only).
        #[arg(long, conflicts_with = "keep_wkt_text")]
        render_wkt: bool,
        /// Keep the text of every WKT literal, as well as its geometry, instead of
        /// `--render-wkt`: for a new dataset, or to turn it off on an existing one (literals
        /// already in binary form stay so until a checkpoint rewrites them).
        #[arg(long)]
        keep_wkt_text: bool,
        /// Files (format from the extension: .ttl, .nt, .nq, .trig, .rdf..., and .parquet
        /// for GeoParquet).
        files: Vec<PathBuf>,
    },
    /// RDFS or OWL 2 RL entailment: the triples the rules derive (by default RDFS: rdfs2, 3,
    /// 5, 7, 9 and 11, as Jena's default RDFS reasoner; `--regime owl-rl` adds OWL 2 RL),
    /// written to a named graph of an on-disk dataset, which they replace, or from files to a
    /// file.
    Entail {
        /// Data files, loaded in memory (without `--location`).
        #[arg(long = "data")]
        data: Vec<PathBuf>,
        /// Directory of an on-disk dataset (TripleTrail storage) to entail in.
        #[arg(long)]
        location: Option<PathBuf>,
        /// The dataset (with `--location`).
        #[arg(long, default_value = "ds")]
        dataset: String,
        /// The named graph that holds the entailed triples; it is not read as data.
        #[arg(long, default_value = "urn:tripletrail:inferred")]
        into: String,
        /// Where to write the entailed triples, without `--location` (format from the
        /// extension; N-Triples on standard output if omitted).
        #[arg(long)]
        output: Option<PathBuf>,
        /// The rules: `rdfs` (Jena's default RDFS), or `owl-rl` (RDFS with the OWL 2 RL rules
        /// for equivalent classes and properties, inverse, symmetric and transitive
        /// properties, value restrictions, intersections, unions, property chains, owl:sameAs,
        /// (inverse) functional properties, a maximum cardinality of 1, keys and
        /// enumerations).
        #[arg(long, value_enum, default_value_t = EntailRegime::Rdfs)]
        regime: EntailRegime,
    },
    /// Runs the SHACL-AF rules (sh:SPARQLRule, sh:TripleRule) of shapes on data until they
    /// infer nothing new, and writes the inferred triples to a file, or to a named graph of
    /// an on-disk dataset with `--into`.
    Infer {
        /// Data files, loaded in memory into the default graph, or into `--data-graph`.
        #[arg(long = "data")]
        data: Vec<PathBuf>,
        /// Shapes files with the rules.
        #[arg(long = "shapes")]
        shapes: Vec<PathBuf>,
        /// Directory of an on-disk dataset to run on instead of files.
        #[arg(long)]
        location: Option<PathBuf>,
        /// Storage of the on-disk dataset.
        #[arg(long, value_enum, default_value_t = StoreKind::TripleTrail)]
        store: StoreKind,
        /// The dataset (with `--location`).
        #[arg(long, default_value = "ds")]
        dataset: String,
        /// The named graph holding the shapes (with `--location`).
        #[arg(long)]
        shapes_graph: Option<String>,
        /// The named graph the rules run on (the default graph if omitted).
        #[arg(long)]
        data_graph: Option<String>,
        /// The named graph of the dataset to write the inferred triples to.
        #[arg(long)]
        into: Option<String>,
        /// Passes at most over each group of rules that feed each other (rules are run in
        /// dependency order; a rule no rule of its group feeds runs once).
        #[arg(long, default_value_t = 64)]
        max_passes: usize,
        /// Also output the triples the rules construct that are already in the data, as
        /// TopBraid's shaclinfer does (by default only the new ones).
        #[arg(long)]
        all_constructed: bool,
        /// Run every rule on all the data in every pass, instead of only on what the last
        /// pass inferred can add (to compare; both infer the same triples).
        #[arg(long)]
        naive: bool,
        /// Run each rule once, in `sh:order` (then by rule IRI), each seeing what the rules
        /// before it inferred: one pass, as a list of SPARQL INSERTs run in that order would.
        #[arg(long)]
        ordered: bool,
        /// Where to write the inferred triples (format from the extension; N-Triples on
        /// standard output if neither this nor `--into` is given).
        #[arg(long)]
        output: Option<PathBuf>,
        /// Print each rule of each pass on standard error as it starts and as it finishes
        /// (its group of rules, `*` when the group runs to a fixpoint; focus nodes, new
        /// triples, time).
        #[arg(long, short)]
        verbose: bool,
        /// Also write where each inferred triple came from to this file, one JSON line per
        /// triple (`triple`, then the rule, shape, focus node, group, pass and, for a SPARQL
        /// rule, the solution). Slower: rules then run on all the data in every pass.
        #[arg(long)]
        provenance: Option<PathBuf>,
    },
    /// Runs SPARQL CONSTRUCT queries that read CSV, JSON and other files through
    /// `SERVICE <x-sparql-anything:…>` (the Facade-X model, as SPARQL Anything), in
    /// parallel, and R2RML / RML mapping documents over the same files, and writes what
    /// they make as N-Triples.
    Lift {
        /// Query and mapping files, or directories searched (recursively) for .rq and
        /// .sparql queries, .rml.ttl / .r2rml.ttl mapping documents and .yarrrml mappings.
        #[arg(required = true)]
        queries: Vec<PathBuf>,
        /// Relative file locations in the queries and mappings resolve against this
        /// directory (the current directory if omitted).
        #[arg(long)]
        base: Option<PathBuf>,
        /// Only read files under these directories (any file if none is given).
        #[arg(long)]
        allow: Vec<PathBuf>,
        /// Data files loaded in memory first, for queries that also read the store.
        #[arg(long = "data")]
        data: Vec<PathBuf>,
        /// The N-Triples file to write every query's triples to (standard output if neither
        /// this nor `--output-dir` is given).
        #[arg(long, conflicts_with = "output_dir")]
        output: Option<PathBuf>,
        /// Write each query's triples to its own .nt file here, at the query's path under
        /// the directory given.
        #[arg(long)]
        output_dir: Option<PathBuf>,
        /// Queries run at the same time.
        #[arg(long, short = 'j', default_value_t = default_threads())]
        jobs: usize,
        /// A query parameter, `name=value`, as SPARQL Anything's `-v`: `?_name` in the SPARQL
        /// queries (not mappings) becomes the literal "value" (`?_name_iri` an IRI, `?_name_number` a
        /// number; `?__name` is optional). Repeat for more parameters.
        #[arg(long = "values", short = 'v', value_name = "NAME=VALUE")]
        values: Vec<String>,
        /// Write each file's plan to standard output instead of its triples: for a mapping
        /// its views and triples maps with the records each read and the triples each made,
        /// for a query its evaluated plan with row counts and timings.
        #[arg(long, conflicts_with_all = ["output", "output_dir"])]
        explain: bool,
        /// Write each mapping (RML, R2RML, YARRRML) as the Facade-X CONSTRUCT query that gives
        /// the same triples, instead of running it: to standard output, to `--output` (one
        /// mapping) or as `.rq` files under `--output-dir`. CSV and JSON sources, term maps,
        /// classes, datatypes, languages and joins; other features are refused.
        #[arg(long, conflicts_with = "explain")]
        as_sparql: bool,
        /// Overwrite RML logical target files that exist. Without `--output-dir` they are
        /// written next to the sources, and an existing one is refused unless this is given.
        #[arg(long)]
        overwrite_targets: bool,
        #[command(flatten)]
        urls: UrlArgs,
        /// The PostgreSQL database R2RML mappings read their tables from (mappings that
        /// name a `d2rq:Database` connect to it instead), as a connection string:
        /// `postgresql://user:password@host:port/name`. TLS follows its `sslmode`: `prefer`
        /// by default, `require`, or `verify-full` (with `sslrootcert=ca.pem` for a private CA).
        #[cfg(feature = "postgres")]
        #[arg(long, env = "TRIPLETRAIL_DATABASE", value_name = "URL")]
        database: Option<String>,
        /// A named PostgreSQL connection, `NAME=URL`, which Facade-X sources pick with
        /// `database=NAME` (those without it read from `--database`). Repeat for more.
        #[cfg(feature = "postgres")]
        #[arg(long = "connection", value_name = "NAME=URL")]
        connections: Vec<String>,
    },
    /// The W3C direct mapping of a PostgreSQL database: a graph of its tables without a
    /// mapping document. Each row becomes a resource named by its primary key (a blank node
    /// without one) and typed by its table, each column with a value a property, each foreign
    /// key a link to the row it references. Writes N-Triples, or with `--mapping` the R2RML
    /// mapping that makes them, to edit and run with `tripletrail lift`.
    #[cfg(feature = "postgres")]
    DirectMapping {
        /// The database, as a connection string: `postgresql://user:password@host:port/name`.
        #[arg(long, env = "TRIPLETRAIL_DATABASE", value_name = "URL")]
        database: String,
        /// The IRI the tables' and rows' IRIs start with.
        #[arg(long, default_value = "http://example.com/base/")]
        base_iri: String,
        /// The schema whose tables and views are mapped.
        #[arg(long, default_value = "public")]
        schema: String,
        /// Map only these tables (all of the schema's if none is given). Repeat for more.
        #[arg(long = "table", value_name = "NAME")]
        tables: Vec<String>,
        /// Write the R2RML mapping instead of the triples.
        #[arg(long)]
        mapping: bool,
        /// The file to write to (standard output if omitted): N-Triples (.nt), or Turtle
        /// with `--mapping`.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Writes the results of a SPARQL SELECT as a GeoParquet file: geometry literal columns
    /// become WKB geometry columns, with their CRS and a bbox covering column.
    #[cfg(feature = "geoparquet")]
    Export {
        /// Directory of an on-disk dataset (without it, the `--data` files in memory).
        #[arg(long)]
        location: Option<PathBuf>,
        /// Storage of the on-disk dataset.
        #[arg(long, value_enum, default_value_t = StoreKind::TripleTrail)]
        store: StoreKind,
        /// The dataset (with `--location`).
        #[arg(long, default_value = "ds")]
        dataset: String,
        /// Data files to load in memory and query instead of a dataset on disk.
        #[arg(long = "data")]
        data: Vec<PathBuf>,
        /// The SELECT query, or @file to read it from a file.
        #[arg(long)]
        query: String,
        /// Rows per row group.
        #[arg(long, default_value_t = 65536)]
        row_group_size: usize,
        /// A file holding the PROJJSON of the geometries' CRS, for a CRS other than CRS84 and
        /// EPSG:4326.
        #[arg(long)]
        projjson: Option<PathBuf>,
        /// The GeoParquet file to write.
        output: PathBuf,
    },
    /// Runs the tests of a workspace (the folders of its `tests/`) or one test folder: an
    /// input, a query, update, rules or shapes, and the expected output (see docs/server.md,
    /// "Tests"). Prints pass or fail with a diff; exits with status 1 when a test fails.
    Test {
        /// A workspace folder, a `tests/` folder or one test (the current folder if omitted).
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Also write a JUnit XML report, for CI.
        #[arg(long)]
        junit: Option<PathBuf>,
        /// Seconds each test may take.
        #[arg(long, default_value_t = 300)]
        timeout: u64,
        /// Write what each test gives as its expected output (`expected.nt`, `expected.srj` or
        /// `violations.yaml`) instead of checking it: to start a test, or to accept a change.
        #[arg(long)]
        record: bool,
    },
    /// Pipelines: an ETL as ordered steps (lift, load, update, infer, reason, validate, export,
    /// publish, python) in a `pipeline.yaml` (see docs/server.md, "Pipelines").
    Pipeline {
        #[command(subcommand)]
        command: PipelineCommand,
    },
    /// Configuration files for `tripletrail serve --config` (#826).
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// TripleTrail Enterprise licences (#734).
    #[cfg(feature = "enterprise")]
    License {
        #[command(subcommand)]
        command: LicenseCommand,
    },
    /// The audit log of `serve --audit-log` (TripleTrail Enterprise, #207).
    #[cfg(feature = "enterprise")]
    Audit {
        #[command(subcommand)]
        command: AuditCommand,
    },
    /// Shape sets and packs (#148).
    Shapes {
        #[command(subcommand)]
        command: ShapesCommand,
    },
    /// Validates data against SHACL shapes and writes the W3C validation report. Data and
    /// shapes come from files (loaded in memory), or from the graphs of an on-disk dataset.
    /// Exits with status 1 when the data does not conform.
    Validate {
        /// Data files, loaded in memory into the default graph, or into `--data-graph`
        /// (format from the extension).
        #[arg(long = "data")]
        data: Vec<PathBuf>,
        /// Shapes files.
        #[arg(long = "shapes")]
        shapes: Vec<PathBuf>,
        /// Lookup files: read with the data (by `sh:class`, paths and SPARQL constraints) but
        /// not validated, such as the vocabularies and code lists the data refers to. Loaded
        /// into a graph of their own; focus nodes come from the `--data` files only.
        #[arg(long = "lookup")]
        lookup: Vec<PathBuf>,
        /// Named graphs of the on-disk dataset to read as lookups (with `--location`), as
        /// `--lookup` files are.
        #[arg(long = "lookup-graph")]
        lookup_graph: Vec<String>,
        /// Directory of an on-disk dataset to validate instead of files.
        #[arg(long)]
        location: Option<PathBuf>,
        /// Storage of the on-disk dataset.
        #[arg(long, value_enum, default_value_t = StoreKind::TripleTrail)]
        store: StoreKind,
        /// The dataset (with `--location`).
        #[arg(long, default_value = "ds")]
        dataset: String,
        /// The named graph holding the shapes (with `--location`).
        #[arg(long)]
        shapes_graph: Option<String>,
        /// The named graph to validate (the default graph if omitted).
        #[arg(long)]
        data_graph: Option<String>,
        /// Where to write the report (format from the extension; Turtle on standard output
        /// if omitted).
        #[arg(long)]
        report: Option<PathBuf>,
        /// Add to the report how many focus and value nodes each shape was checked against
        /// (`ttsh:shapeStatistics`, `ttsh:focusNodes`, `ttsh:valueNodes`), its IRI, the same in
        /// every run even for a blank node shape (`ttsh:shapeIri`), its distinct focus
        /// nodes and those of its results (`ttsh:distinctFocusNodes`,
        /// `ttsh:failingFocusNodes`), conforming shapes included; and the run's summary
        /// (`ttsh:results`) with one `ttsh:cell` per rule, severity and constraint component.
        #[arg(long)]
        stats: bool,
    },
    /// RDF Dataset Canonicalization (RDFC-1.0): writes the inputs' quads with their blank
    /// nodes relabelled `_:c14n0`, `_:c14n1`, … in canonical N-Quads, sorted, so that two
    /// isomorphic datasets give the same bytes, and prints their SHA-256 to standard error.
    Canon {
        /// Files (format from the extension) or directories of on-disk datasets
        /// (`<location>/<name>`), read together as one dataset.
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// Storage of the on-disk datasets.
        #[arg(long, value_enum, default_value_t = StoreKind::TripleTrail)]
        store: StoreKind,
        /// Where to write the canonical N-Quads (standard output if omitted).
        #[arg(long)]
        output: Option<PathBuf>,
        /// Print only the SHA-256 (as `sha256:<hex>`) to standard output.
        #[arg(long)]
        digest: bool,
    },
    /// What changed between two datasets (two releases): the quads only in the second
    /// (added) and only in the first (removed), with blank nodes matched through their
    /// RDFC-1.0 canonical labels. Prints a JSON summary per graph, per predicate and per class
    /// of the subject, or an RDF Patch. Exits with status 1 when they differ.
    Diff {
        /// The first dataset: a file or the directory of an on-disk dataset.
        a: PathBuf,
        /// The second dataset.
        b: PathBuf,
        /// Storage of the on-disk datasets.
        #[arg(long, value_enum, default_value_t = StoreKind::TripleTrail)]
        store: StoreKind,
        /// `json` (a summary with counts) or `rdf-patch` (`D` lines for the removed quads,
        /// then `A` lines for the added ones).
        #[arg(long, value_enum, default_value_t = DiffFormat::Json)]
        format: DiffFormat,
        /// Also write the added quads to this N-Quads file.
        #[arg(long)]
        added: Option<PathBuf>,
        /// Also write the removed quads to this N-Quads file.
        #[arg(long)]
        removed: Option<PathBuf>,
    },
    /// Runs a SPARQL query on a dataset on disk or on data files, without a server (as Jena's
    /// `arq` and `tdb2.tdbquery`). Rows are written as TSV and graphs as Turtle, unless
    /// `--format` or the extension of `--output` says otherwise.
    Query {
        #[command(flatten)]
        source: SourceArgs,
        /// The query: its text, @file, or the path of a `.rq` file.
        #[arg(long)]
        query: String,
        /// For rows: `tsv`, `csv`, `json` or `xml`. For graphs: `ttl`, `nt`, `nq`, `trig`,
        /// `rdf` or `jsonld`.
        #[arg(long)]
        format: Option<String>,
        /// Where to write the results (standard output if omitted).
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Runs a SPARQL update without a server (as Jena's `tdb2.tdbupdate`): on a dataset on
    /// disk, which keeps the changes, or on data files, whose dataset after the update is
    /// written out (N-Quads unless `--format` or the extension of `--output` says otherwise).
    Update {
        #[command(flatten)]
        source: SourceArgs,
        /// The update: its text, @file, or the path of a `.ru` file.
        #[arg(long)]
        update: String,
        /// With `--data`: the format the dataset is written in.
        #[arg(long)]
        format: Option<String>,
        /// With `--data`: where to write the dataset (standard output if omitted).
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Writes the quads of a dataset on disk (as Jena's `tdb2.tdbdump`), as N-Quads unless
    /// `--format` or the extension of `--output` says otherwise.
    Dump {
        /// Directory of the on-disk datasets.
        #[arg(long)]
        location: PathBuf,
        /// Storage of the on-disk dataset.
        #[arg(long, value_enum, default_value_t = StoreKind::TripleTrail)]
        store: StoreKind,
        #[arg(long, default_value = "ds")]
        dataset: String,
        /// Only this graph: its IRI, or `default` for the default graph. A format of triples
        /// (Turtle, N-Triples, RDF/XML) needs it when the dataset has named graphs.
        #[arg(long)]
        graph: Option<String>,
        /// `nq`, `trig`, `nt`, `ttl`, `rdf` or `jsonld`.
        #[arg(long)]
        format: Option<String>,
        /// Where to write the quads (standard output if omitted).
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// What a dataset on disk takes (#152): its files by kind (`dict.bin`, `geoms.bin`, the
    /// index segments, …) and, read from every key, the dictionary's bytes by kind of term
    /// (IRIs, strings, WKT text or binary, other literals), with what their datatypes and
    /// IRI namespaces take. Reads the dataset without changing it; a dataset a server has
    /// open is refused (stop the server, or run it on a copy of the folder).
    DiskUsage {
        /// Directory of the on-disk datasets.
        #[arg(long)]
        location: PathBuf,
        #[arg(long, default_value = "ds")]
        dataset: String,
        /// Prints JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Reads RDF files as a stream and writes them in another format, checks them, or counts
    /// their triples (as Jena's `riot`). Without `--to` or `--output`, writes N-Quads to
    /// standard output.
    Convert {
        /// The RDF files (format from the extension, or `--syntax`).
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// The format of the input files, when their extensions don't say.
        #[arg(long)]
        syntax: Option<String>,
        /// The format to write: `nq`, `nt`, `ttl`, `trig`, `rdf` or `jsonld`.
        #[arg(long)]
        to: Option<String>,
        /// Where to write (standard output if omitted); its extension gives the format.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Only check the files: print whether each is valid, and its first error.
        #[arg(long)]
        validate: bool,
        /// Only count the triples (or quads) of each file.
        #[arg(long)]
        count: bool,
        /// The base IRI relative IRIs are resolved against.
        #[arg(long)]
        base: Option<String>,
    },
}

/// The data `query` and `update` run on: a dataset on disk, or files loaded in memory.
#[derive(clap::Args)]
struct SourceArgs {
    /// Directory of the on-disk datasets (without it, the `--data` files in memory).
    #[arg(long)]
    location: Option<PathBuf>,
    /// Storage of the dataset.
    #[arg(long, value_enum, default_value_t = StoreKind::TripleTrail)]
    store: StoreKind,
    /// The dataset (with `--location`).
    #[arg(long, default_value = "ds")]
    dataset: String,
    /// Data files to load in memory (format from the extension), instead of `--location`:
    /// `--data a.ttl b.nq` or `--data a.ttl --data b.nq`. Blank node labels are each file's own.
    #[arg(long = "data", num_args = 1..)]
    data: Vec<PathBuf>,
    /// Make the default graph the union of all graphs.
    #[arg(long)]
    union_default_graph: bool,
    /// Properties whose literals full-text search (`tt:search`, `text:query`) indexes too, as
    /// `serve --text-properties` does for a dataset (#188): one IRI each, or a comma-separated
    /// list.
    #[arg(long = "text-properties", value_delimiter = ',', value_name = "IRI")]
    text_properties: Vec<String>,
}

impl SourceArgs {
    fn source(&self) -> offline::Source<'_> {
        offline::Source {
            location: self.location.as_deref(),
            dataset: &self.dataset,
            store: self.store,
            data: &self.data,
            union_default_graph: self.union_default_graph,
            text_properties: &self.text_properties,
        }
    }
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum DiffFormat {
    Json,
    RdfPatch,
}

/// Which URLs lift reads, and how.
#[derive(clap::Args)]
struct UrlArgs {
    /// URL prefixes Facade-X locations (`location=https://…`) and RML sources may be read
    /// from, such as `https://data.example.org/`: same scheme, host and port, and a path
    /// prefix. Redirects are followed only within them. No URL is read without one. Repeat
    /// for more.
    #[arg(long, value_name = "URL")]
    allow_url: Vec<String>,
    /// The longest one URL may take to read, in seconds.
    #[arg(long, default_value_t = 30.0)]
    url_timeout: f64,
    /// The largest body read from one URL, in bytes.
    #[arg(long, default_value_t = 256 << 20)]
    url_max_bytes: usize,
}

impl UrlArgs {
    fn remote(self) -> Result<tripletrail::fx::remote::RemoteLocations> {
        Ok(tripletrail::fx::remote::RemoteLocations {
            allow: self.allow_url,
            timeout: Duration::try_from_secs_f64(self.url_timeout)
                .map_err(|_| "--url-timeout takes a positive number of seconds")?,
            max_bytes: self.url_max_bytes,
        })
    }
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ExportKind {
    Airflow,
    Python,
}

#[cfg(feature = "enterprise")]
#[derive(Subcommand)]
enum LicenseCommand {
    /// Checks a licence and prints who it is for, what it unlocks and until when. Exits 0
    /// when it unlocks the enterprise features (valid, expiring or in its grace period), 1
    /// otherwise. Reads FILE, or `-` for standard input, or else TRIPLETRAIL_LICENSE or
    /// TRIPLETRAIL_LICENSE_FILE, as `serve` does.
    Inspect {
        file: Option<PathBuf>,
        /// Prints JSON (as GET /$/license answers) instead of text.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ShapesCommand {
    /// Checks a pack release before it is installed or upgraded to (#148), keeping nothing:
    /// its errors and those of the own set, the shapes it adds, drops and changes against the
    /// installed release, the own set's overrides that go on changing a pack shape and those
    /// that would stand alone, and with data, the release's shapes for classes the data has no
    /// instances of. Exits with status 1 when a set has errors (and with --strict, when an
    /// override would stand alone).
    Check {
        /// The folder of the pack release: its `.ttl` files.
        #[arg(long)]
        pack: PathBuf,
        /// The folder of the release installed now, to compare with.
        #[arg(long)]
        installed: Option<PathBuf>,
        /// The folder of the dataset's own set, on top of the pack.
        #[arg(long)]
        own: Option<PathBuf>,
        /// Data files, loaded in memory, to look for the classes of the pack's shapes.
        #[arg(long = "data")]
        data: Vec<PathBuf>,
        /// Directory of an on-disk dataset to look in instead of files.
        #[arg(long)]
        location: Option<PathBuf>,
        /// Storage of the on-disk dataset.
        #[arg(long, value_enum, default_value_t = StoreKind::TripleTrail)]
        store: StoreKind,
        /// The dataset (with `--location`).
        #[arg(long, default_value = "ds")]
        dataset: String,
        /// Exit with status 1 when an override would stand alone, too.
        #[arg(long)]
        strict: bool,
        /// The report as JSON, as `PUT …/shapes/pack?dry=true` answers it.
        #[arg(long)]
        json: bool,
    },
    /// Writes the shapes of RINF Atlas (a folder of SHACL node shapes with `atlas:`
    /// annotations) as a shape set in the `tts:` vocabulary, one file per node shape and per
    /// map view (the folder's `views/`), to start a shape pack from (#148). What TripleTrail has no reader for is left out and listed on
    /// standard error.
    ImportAtlas {
        /// The folder of RINF Atlas shapes: its `.ttl` files (not those of its subfolders).
        folder: PathBuf,
        /// The folder to write the shape files to; it is created if needed.
        #[arg(long)]
        out: PathBuf,
        /// A reader that leaves the shapes out (`describe`, `search`…), as `tts:hiddenFrom`:
        /// when the pack has other shapes for that reader. Repeatable.
        #[arg(long = "hidden-from")]
        hidden_from: Vec<String>,
        /// The start of each file's name, to keep them apart from the pack's other files.
        #[arg(long, default_value = "")]
        prefix: String,
    },
}

#[cfg(feature = "enterprise")]
#[derive(Subcommand)]
enum AuditCommand {
    /// Checks the hash chain of an audit log: exits 0 and prints the number of lines and the
    /// last hash when no line was removed, changed or moved, and 1 with each problem otherwise.
    /// Keep the last hash somewhere else (a ticket, another system) to also show lines
    /// removed from the end.
    Verify { file: PathBuf },
    /// Who did what, from an audit log: by agent and dataset, the changes done, refused and
    /// failed, the reads (with --audit-reads), the quads added and removed in each graph, how
    /// they signed in and from where. The chain is checked too: when it is not whole, the
    /// report says so and the command exits 1.
    Report {
        file: PathBuf,
        /// Lines at or after this time, in UTC as the log writes it (2026-11-15T10:00:00Z,
        /// or a prefix such as 2026-11-15).
        #[arg(long)]
        since: Option<String>,
        /// Lines before this time, in UTC.
        #[arg(long)]
        until: Option<String>,
        /// Only this agent (`anonymous` for lines without one).
        #[arg(long)]
        agent: Option<String>,
        /// Only this dataset.
        #[arg(long)]
        dataset: Option<String>,
        /// The report as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Translates a Fuseki configuration file (`config.ttl`, the assembler file
    /// `fuseki-server --config` reads) into a configuration file for `tripletrail serve --config`:
    /// its services become datasets, with their union default graph, RDFS or OWL reasoning
    /// and query timeout. What it can't carry over is listed at the end of the file.
    ImportFuseki {
        /// The Fuseki configuration, Turtle.
        file: PathBuf,
        /// Write the configuration here instead of to stdout.
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum PipelineCommand {
    /// Runs the steps of a `pipeline.yaml` on a dataset, in order, and writes a JSON report
    /// (steps, times, peak memory, counts, gate results). Stops at the first step that fails
    /// or whose validation gate finds results; exits with status 1 then.
    Run {
        /// The pipeline file.
        file: PathBuf,
        /// Directory of the on-disk dataset to run on (without it, an empty dataset in memory,
        /// dropped at the end: for pipelines that export what they make).
        #[arg(long)]
        location: Option<PathBuf>,
        /// Storage of the on-disk dataset.
        #[arg(long, value_enum, default_value_t = StoreKind::TripleTrail)]
        store: StoreKind,
        /// The dataset (with `--location`).
        #[arg(long, default_value = "ds")]
        dataset: String,
        /// A parameter, `name=value`, over the pipeline's `parameters`. Repeat for more.
        #[arg(long = "values", short = 'v', value_name = "NAME=VALUE")]
        values: Vec<String>,
        /// The first step to run: its number (from 1) or name.
        #[arg(long)]
        from: Option<String>,
        /// The last step to run: its number or name.
        #[arg(long)]
        to: Option<String>,
        /// Only lift files under these directories (any file if none is given).
        #[arg(long)]
        allow: Vec<PathBuf>,
        /// Where to write the JSON report (standard output if omitted).
        #[arg(long)]
        report: Option<PathBuf>,
        /// The Python interpreter of python steps.
        #[arg(long, env = "TRIPLETRAIL_PYTHON", default_value = "python3")]
        python: String,
        #[command(flatten)]
        urls: UrlArgs,
    },
    /// Writes a `pipeline.yaml` as code to run elsewhere: an Airflow DAG whose tasks run its
    /// steps with `tripletrail pipeline run`, or a Python script that runs them in process with
    /// `tripletrail.Engine`.
    Export {
        /// The pipeline file.
        file: PathBuf,
        /// `airflow` or `python`.
        #[arg(long, value_enum)]
        to: ExportKind,
        /// Airflow: one task for the whole pipeline instead of one per step.
        #[arg(long)]
        one_task: bool,
        /// The dataset directory the code runs on (a store in memory for a script without it).
        #[arg(long)]
        location: Option<String>,
        #[arg(long, default_value = "ds")]
        dataset: String,
        /// A parameter value over the pipeline's default (`name=value`).
        #[arg(long = "values", short = 'v', value_name = "NAME=VALUE")]
        values: Vec<String>,
        /// Where to write the code (standard output by default).
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
}

#[derive(clap::Args)]
struct ServeArgs {
    /// A configuration file (YAML) of these options and of the datasets one by one (see
    /// docs/server.md, "Configuration file"). Flags and environment variables win over it.
    #[arg(long, env = "TRIPLETRAIL_CONFIG", value_name = "FILE")]
    config: Option<PathBuf>,
    /// Address to listen on.
    #[arg(long, env = "TRIPLETRAIL_BIND", default_value = "127.0.0.1:7878")]
    bind: String,
    /// A dataset to serve, at /<name>/query, /<name>/update, /<name>/data... Repeat for
    /// several. With --location it is <location>/<name>; without, in memory. Without any
    /// --dataset or --attach, one dataset named ds.
    #[arg(long = "dataset", env = "TRIPLETRAIL_DATASETS", value_delimiter = ',')]
    datasets: Vec<String>,
    /// A shape pack to install on a dataset as it opens (#148): `DIR`, for the first
    /// `--dataset`, or `NAME=DIR`. The folder holds the pack's `.ttl` files and, optionally,
    /// `pack.json` with its `name`, `version` and `source` (else the folder's name, version
    /// `local`).
    /// A release already installed is left as it is.
    #[arg(long = "pack", value_name = "[NAME=]DIR")]
    packs: Vec<String>,
    /// Directory for on-disk datasets (one store per dataset, in <location>/<name>), also where
    /// datasets created with the admin API (type=disk) go. Without it, --dataset datasets are
    /// in memory.
    #[arg(long, env = "TRIPLETRAIL_LOCATION")]
    location: Option<PathBuf>,
    /// Open the --dataset datasets of --location read-only: memory-mapped, never changed;
    /// updates and uploads answer 405.
    #[arg(long, env = "TRIPLETRAIL_READ_ONLY")]
    read_only: bool,
    /// Do not make each dataset's string ranks (the order of its IRIs and strings) and map (its
    /// collections and unfiltered facet counts) in the background once the datasets are open;
    /// the first query or map request that needs them makes them instead.
    #[arg(long, env = "TRIPLETRAIL_NO_WARM_UP")]
    no_warm_up: bool,
    /// Serve a dataset directory (written by `tripletrail load`, tripletrail's Engine.save, or a
    /// snapshot) as NAME: NAME=DIR, or NAME=DIR:ro to open it read-only. Repeat for several.
    #[arg(
        long = "attach",
        env = "TRIPLETRAIL_ATTACH",
        value_delimiter = ',',
        value_name = "NAME=DIR[:ro]"
    )]
    attach: Vec<String>,
    /// A stable name for a dataset, which the admin API can later point elsewhere
    /// (POST /$/datasets/ALIAS/swap?to=...): ALIAS=DATASET. Repeat for several.
    #[arg(
        long = "alias",
        env = "TRIPLETRAIL_ALIASES",
        value_delimiter = ',',
        value_name = "ALIAS=DATASET"
    )]
    aliases: Vec<String>,
    /// Datasets (of --dataset and --attach) whose default graph, for queries and updates, is
    /// the union of all their graphs, as Fuseki's unionDefaultGraph; `*` for all of them.
    /// Inserts without GRAPH still go to the real default graph.
    #[arg(
        long = "union-default-graph",
        env = "TRIPLETRAIL_UNION_DEFAULT_GRAPH",
        value_delimiter = ',',
        value_name = "NAME"
    )]
    union_default_graph: Vec<String>,
    /// Datasets (of --dataset and --attach, `*` for all) that keep a named graph holding the
    /// RDFS entailment of their other graphs, up to date with every write:
    /// `NAME` (into urn:tripletrail:inferred) or `NAME=GRAPH`. Entailed once when the server
    /// starts. Writes into that graph itself are undone.
    #[arg(
        long,
        env = "TRIPLETRAIL_RDFS",
        value_delimiter = ',',
        value_name = "NAME[=GRAPH]"
    )]
    rdfs: Vec<String>,
    /// As --rdfs, with the OWL 2 RL rules (`tripletrail entail --regime owl-rl`). A write that
    /// only adds data is entailed from its triples; one that removes triples, or adds
    /// schema or owl:sameAs, entails the whole dataset again.
    #[arg(
        long = "owl-rl",
        env = "TRIPLETRAIL_OWL_RL",
        value_delimiter = ',',
        value_name = "NAME[=GRAPH]"
    )]
    owl_rl: Vec<String>,
    /// Properties whose literals full-text search (`tt:search`, `text:query`) indexes in a
    /// dataset, besides rdfs:label and the others it always indexes (#188): `NAME=IRI`, once
    /// per property, `*=IRI` for every dataset, also those created at run time. As the fields of
    /// a jena-text entity map. A comma separates entries, so an IRI holding one is written
    /// with `%2C`.
    #[arg(
        long = "text-properties",
        env = "TRIPLETRAIL_TEXT_PROPERTIES",
        value_delimiter = ',',
        value_name = "NAME=IRI"
    )]
    text_properties: Vec<String>,
    /// Storage: the tripletrail store (the default) or Oxigraph's store (RocksDB on disk,
    /// needs the rocksdb feature).
    #[arg(long, env = "TRIPLETRAIL_STORE", value_enum, default_value_t = StoreKind::TripleTrail)]
    store: StoreKind,
    /// Longest a query or update may run, in seconds (a request's `timeout` parameter
    /// may ask for less, not more).
    #[arg(long, env = "TRIPLETRAIL_TIMEOUT", value_parser = parse_seconds)]
    timeout: Option<Duration>,
    /// Worker threads.
    #[arg(long, env = "TRIPLETRAIL_THREADS", default_value_t = default_threads())]
    threads: usize,
    /// Threads of the pool that large joins, OPTIONAL, MINUS and EXISTS on ids run on (0: as
    /// many as cores; 1: each query on its own thread only). Queries share this pool, so
    /// concurrent queries don't start more; it is separate from the store's background work.
    #[arg(long, env = "TRIPLETRAIL_QUERY_THREADS", default_value_t = 0)]
    query_threads: usize,
    /// Allow cross-origin requests from these origins (e.g. https://example.com); repeat
    /// for several. Alone or with '*', from every origin (Access-Control-Allow-Origin: *),
    /// which any web page can then use to send updates unless --update-token is set.
    #[arg(
        long,
        env = "TRIPLETRAIL_CORS",
        num_args = 0..,
        value_delimiter = ',',
        default_missing_value = "*",
        value_name = "ORIGIN"
    )]
    cors: Option<Vec<String>>,
    /// Writes (SPARQL updates, and POST, PUT and DELETE on /<name>/data) need
    /// `Authorization: Bearer <token>`, this token or the admin token. Queries stay open.
    #[arg(long, env = "TRIPLETRAIL_UPDATE_TOKEN", hide_env_values = true)]
    update_token: Option<String>,
    /// Largest query, update or form body accepted, in bytes (larger ones get 413).
    #[arg(long, env = "TRIPLETRAIL_MAX_BODY", default_value_t = 16 << 20)]
    max_body: usize,
    /// Largest Graph Store upload accepted, in bytes (no limit if omitted). An upload is
    /// held in memory while it is checked for errors.
    #[arg(long, env = "TRIPLETRAIL_MAX_UPLOAD")]
    max_upload: Option<usize>,
    /// Most MiB one pull from a git repository (bound shapes, or a workspace) may add to its
    /// copy on disk, and may read from the commit. 0 for no limit.
    #[arg(long, env = "TRIPLETRAIL_GIT_FETCH_MB", default_value_t = 256)]
    git_fetch_mb: u64,
    /// Memory, in MiB, that the routing networks built from inline edge sub-selects may take
    /// per dataset; past it the least recently used are dropped. Named networks
    /// (pgr:network) are always kept. 0 for no limit.
    #[arg(long, env = "TRIPLETRAIL_NETWORK_CACHE_MB", default_value_t = 2048)]
    network_cache_mb: usize,
    /// Memory, in MiB, that repeated SELECT and ASK results may take per dataset, answered
    /// again without evaluating until the next write (`X-Cache: HIT`). 0 turns it off.
    #[arg(long, env = "TRIPLETRAIL_RESULT_CACHE_MB", default_value_t = 256)]
    result_cache_mb: usize,
    /// What answers a SELECT whose body runs on term ids: `ids` answers it straight from the
    /// id tables (patterns, joins, OPTIONAL, MINUS, FILTER, ORDER BY, DISTINCT, LIMIT), only
    /// decoding the terms it returns, and hands the rest to the term evaluator; `spareval`
    /// hands every query to the evaluator. A query can choose with
    /// `#pragma tt.executor ids|spareval`.
    #[arg(long, env = "TRIPLETRAIL_EXECUTOR", default_value = "ids",
          value_parser = ["ids", "spareval"])]
    executor: String,
    /// Memory a bulk load (`--load`, or a Graph Store upload) may use to sort its quads
    /// (e.g. 32G; suffixes K, M, G, T); a larger one sorts them on disk. No limit by default.
    #[arg(long, env = "TRIPLETRAIL_MEMORY_LIMIT", value_parser = parse_size)]
    memory_limit: Option<usize>,
    /// Memory the intermediate rows of one query may take (e.g. 256M; suffixes K, M, G, T):
    /// its joins on term ids, past which the query is evaluated row by row instead (slower, in
    /// little memory), and the rows each hash join holds, past which the query fails (503).
    /// This bounds what one query (a cross join, say) takes before its timeout. Lower it on a
    /// shared server with many concurrent queries; raise it for large ETL queries. Without
    /// it, id joins take up to 1G and hash joins have no limit.
    #[arg(long, env = "TRIPLETRAIL_QUERY_MEMORY", value_parser = parse_size)]
    query_memory: Option<usize>,
    /// Look the triple-pattern side of a join up once per row of the other side when the
    /// join has no keys, or keys with few distinct values, and every row binds a variable of
    /// that side (an IRI made by BIND, say), instead of building a product: a safety net for
    /// such plans. On by default: `--lookup-joins false` (or `TRIPLETRAIL_LOOKUP_JOINS=false`)
    /// turns it off. Takes true/false, 1/0, yes/no or on/off; an empty variable is not set.
    #[arg(
        long,
        env = "TRIPLETRAIL_LOOKUP_JOINS",
        default_value_t = true,
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = clap::builder::BoolishValueParser::new(),
        action = clap::ArgAction::Set
    )]
    lookup_joins: bool,
    /// Files to load into the first dataset before serving (format from the extension).
    #[arg(long = "load")]
    load: Vec<PathBuf>,
    /// Index only the geometry literals of this predicate (IRI). Repeat for several.
    /// Without it, the geometries of every predicate are indexed.
    #[arg(
        long = "index-predicate",
        env = "TRIPLETRAIL_INDEX_PREDICATES",
        value_delimiter = ','
    )]
    index_predicates: Vec<String>,
    /// Leave the geometry literals of this predicate (IRI) out of the spatial index, for
    /// large geometries no query tests spatially. Repeat for several.
    #[arg(
        long = "no-index-predicate",
        env = "TRIPLETRAIL_NO_INDEX_PREDICATES",
        value_delimiter = ','
    )]
    no_index_predicates: Vec<String>,
    /// Admin requests that change something (POST, PUT, DELETE under /$/) need
    /// `Authorization: Bearer <token>`. Without it, anyone who reaches the server can attach,
    /// detach and back up datasets.
    #[arg(long, env = "TRIPLETRAIL_ADMIN_TOKEN", hide_env_values = true)]
    admin_token: Option<String>,
    /// Where backups are written (default <location>/backups).
    #[arg(long, env = "TRIPLETRAIL_BACKUP_DIR")]
    backup_dir: Option<PathBuf>,
    /// Keep this many backups per dataset: after a backup succeeds, older backups of the
    /// same dataset are deleted (default: keep all).
    #[arg(long, env = "TRIPLETRAIL_BACKUP_KEEP", value_parser = parse_keep)]
    backup_keep: Option<usize>,
    /// Archive the write-ahead log of each on-disk dataset to <dir>/<dataset>/<timeline>/ as
    /// checkpoints retire it: with store backups, datasets restore to any point in time
    /// (until=).
    #[arg(long, env = "TRIPLETRAIL_WAL_ARCHIVE")]
    wal_archive: Option<PathBuf>,
    /// Keep this many retired write-ahead logs in each on-disk dataset's directory, so that
    /// read replicas (--follow, on other servers) that are behind when a checkpoint retires a
    /// log can still read it to its end instead of copying the whole dataset again.
    #[arg(long, env = "TRIPLETRAIL_KEEP_LOGS", default_value_t = 1)]
    keep_logs: u64,
    /// tt:nearest searches exactly up to this many vectors of one length, and past it builds a
    /// graph (HNSW) of them in the background and searches it, approximately. 0 keeps every
    /// search exact.
    #[arg(long, env = "TRIPLETRAIL_VECTOR_GRAPH_MIN", default_value_t = tripletrail::vector::GRAPH_MIN)]
    vector_graph_min: usize,
    /// Answer a write to an on-disk dataset only once this many read replicas (--follow, on
    /// other servers) hold it, so that losing the leader loses no answered write. 0 answers
    /// once the write is on the leader's disk.
    #[cfg(feature = "replication")]
    #[arg(long, env = "TRIPLETRAIL_SYNC_REPLICAS", default_value_t = 0)]
    sync_replicas: usize,
    /// How long, in seconds, a write waits for --sync-replicas replicas. It then answers
    /// anyway (the write is on the leader's disk), with TripleTrail-Replicas saying how many
    /// hold it, and the server logs a warning.
    #[cfg(feature = "replication")]
    #[arg(long, env = "TRIPLETRAIL_SYNC_TIMEOUT", default_value_t = 10.0)]
    sync_timeout: f64,
    /// Serve a read replica of a dataset of another tripletrail server: NAME=URL, URL being the
    /// leader's dataset (http://leader:7878/ds). Copies the dataset into <location>/NAME.r<n>,
    /// then applies each transaction the leader commits; serves queries, refuses writes.
    /// Repeatable (or comma-separated). More URLs for one NAME (NAME=URL1,URL2) are servers
    /// that may be promoted to the leader: the replica moves to whichever is the leader.
    #[cfg(feature = "replication")]
    #[arg(long, env = "TRIPLETRAIL_FOLLOW", value_delimiter = ',')]
    follow: Vec<String>,
    /// The leader's admin token (or an API key of its tripletrail:admin group), for --follow.
    #[cfg(feature = "replication")]
    #[arg(long, env = "TRIPLETRAIL_FOLLOW_TOKEN", hide_env_values = true)]
    follow_token: Option<String>,
    /// How long, in seconds, a read replica's read of the leader's log waits for a commit
    /// before asking again.
    #[cfg(feature = "replication")]
    #[arg(long, env = "TRIPLETRAIL_FOLLOW_WAIT", default_value_t = 10.0)]
    follow_wait: f64,
    /// RDF files of user-defined functions (SHACL-AF `sh:SPARQLFunction`s): queries and
    /// updates on every dataset can call them by their IRIs. Comma-separated in the
    /// environment variable.
    #[arg(
        long = "functions",
        env = "TRIPLETRAIL_FUNCTIONS",
        value_delimiter = ','
    )]
    functions: Vec<PathBuf>,
    /// Do not serve the web UI at /$/ui/.
    #[arg(long, env = "TRIPLETRAIL_NO_UI")]
    no_ui: bool,
    /// Serve a Model Context Protocol endpoint at /{dataset}/mcp, for AI agents: read-only
    /// tools (SPARQL, describe, search, shapes) under the same access rules and limits as
    /// queries.
    #[arg(long, env = "TRIPLETRAIL_MCP")]
    mcp: bool,
    /// Load MatGUI from this URL (a directory holding yasgui.min.js and yasgui.min.css, e.g.
    /// https://cdn.jsdelivr.net/npm/@matdata/yasgui@6.15.0/build/) instead of the copy built
    /// into the binary.
    #[arg(long, env = "TRIPLETRAIL_MATGUI_URL")]
    matgui_url: Option<String>,
    /// The raster tiles under the maps (describe pages, the Map page), as an XYZ URL template.
    /// OpenStreetMap's by default: each viewer's browser fetches them, so check the
    /// provider's usage policy, and prefer your own tile server for private data. `none`
    /// draws no base map, and the pages fetch nothing from elsewhere.
    #[arg(long, env = "TRIPLETRAIL_MAP_TILES", default_value = OSM_TILES)]
    map_tiles: String,
    /// The attribution of --map-tiles, shown on the map (HTML); OpenStreetMap's for the
    /// default tiles.
    #[arg(long, env = "TRIPLETRAIL_MAP_ATTRIBUTION")]
    map_attribution: Option<String>,
    /// Another base map the Map page lets viewers switch to, as `TITLE|URL|ATTRIBUTION`, or
    /// `TITLE|URL|ATTRIBUTION|MAXZOOM`: a title for its menu, an XYZ raster tile URL template,
    /// its credit (HTML), and the last zoom the provider has tiles for (default 19; the map
    /// enlarges that zoom's tiles beyond it). Repeat it for more; in the environment, one per
    /// line.
    #[arg(
        long = "map-basemap",
        env = "TRIPLETRAIL_MAP_BASEMAPS",
        value_delimiter = '\n',
        value_name = "TITLE|URL|ATTRIBUTION[|MAXZOOM]"
    )]
    map_basemaps: Vec<String>,
    /// The name of this server on its pages (#262): the tab title and top bar of the web UI,
    /// the REST API and GraphQL pages, the DCAT catalog's title (unless --catalog gives one)
    /// and map exports. Plain text, at most 80 characters. Default: TripleTrail.
    #[arg(long, env = "TRIPLETRAIL_SITE_NAME")]
    site_name: Option<String>,
    /// The favicon of the pages: an SVG, PNG, ICO, WebP or JPEG file of at most 256 KB, read at
    /// start-up, or an http(s) URL. Served at /$/ui/brand/favicon without credentials.
    #[arg(long, env = "TRIPLETRAIL_SITE_FAVICON", value_name = "FILE|URL")]
    site_favicon: Option<String>,
    /// The logo in the web UI's top bar, in place of TripleTrail's mark, shown 28 px high: a
    /// file or URL as for --site-favicon. Served at /$/ui/brand/logo.
    #[arg(long, env = "TRIPLETRAIL_SITE_LOGO", value_name = "FILE|URL")]
    site_logo: Option<String>,
    /// The logo for the dark theme, when --site-logo does not read on a dark background.
    #[arg(long, env = "TRIPLETRAIL_SITE_LOGO_DARK", value_name = "FILE|URL")]
    site_logo_dark: Option<String>,
    /// The accent colour of the pages, as #rrggbb: buttons, links, tabs and MatGUI's. White
    /// text must read on it (contrast at least 4.5:1); the dark theme uses lighter shades.
    #[arg(long, env = "TRIPLETRAIL_SITE_COLOR", value_name = "#RRGGBB")]
    site_color: Option<String>,
    /// Write one JSON line per request to standard output (time, method, path, dataset,
    /// status, ms, query_hash).
    #[arg(long, env = "TRIPLETRAIL_LOG_REQUESTS")]
    log_requests: bool,
    /// On SIGTERM, how many seconds running requests get to finish before the server
    /// checkpoints its datasets and exits.
    #[arg(long, env = "TRIPLETRAIL_SHUTDOWN_GRACE", default_value_t = 30.0)]
    shutdown_grace: f64,
    /// Linked Data: the IRIs under PREFIX (e.g. https://data.example.org/) dereference to
    /// their description in DATASET, with content negotiation (Turtle, JSON-LD, N-Triples,
    /// RDF/XML, HTML; also by .ttl/.jsonld/.nt/.rdf/.html suffix): DATASET=PREFIX. Repeat for
    /// several. Behind a reverse proxy, forward Host or X-Forwarded-Host/-Proto.
    #[arg(
        long = "base-iri",
        env = "TRIPLETRAIL_BASE_IRIS",
        value_delimiter = ',',
        value_name = "DATASET=PREFIX"
    )]
    base_iris: Vec<String>,
    /// Linked Data: answer a request for a resource itself (no suffix) with 303 See Other to
    /// its document in the negotiated format, instead of with the document.
    #[arg(long, env = "TRIPLETRAIL_SEE_OTHER")]
    see_other: bool,
    /// A Turtle file with the DCAT-AP catalog's titles, descriptions, publisher, contact
    /// point, licence and keywords, for `/.well-known/dcat`. Its relative IRIs resolve against
    /// the server's URL: `<#catalog>` is the catalog, `<kg/void#dataset>` the dataset kg.
    #[arg(long, env = "TRIPLETRAIL_CATALOG", value_name = "FILE")]
    catalog: Option<PathBuf>,
    /// Write each query or update that takes at least this many milliseconds to standard
    /// error, as a JSON line with its full text (off by default: query text can hold data).
    #[arg(long, env = "TRIPLETRAIL_SLOW_QUERY_MS", value_name = "MS")]
    slow_query_ms: Option<u64>,
    /// Send OpenTelemetry traces (a span per request, with its steps) to this OTLP collector,
    /// over HTTP with JSON: its base URL (`/v1/traces` is added) or the full traces URL.
    #[cfg(feature = "otel")]
    #[arg(long, env = "OTEL_EXPORTER_OTLP_ENDPOINT", value_name = "URL")]
    otlp_endpoint: Option<String>,
    /// Headers of the requests to the collector, as `key=value,key=value` (an API key).
    #[cfg(feature = "otel")]
    #[arg(
        long,
        env = "OTEL_EXPORTER_OTLP_HEADERS",
        value_name = "HEADERS",
        hide_env_values = true
    )]
    otlp_headers: Option<String>,
    /// The `service.name` of the spans.
    #[cfg(feature = "otel")]
    #[arg(
        long,
        env = "OTEL_SERVICE_NAME",
        default_value = "tripletrail",
        value_name = "NAME"
    )]
    otlp_service_name: String,
    /// The share of requests traced, from 0 to 1, for requests without a `traceparent` (one
    /// that has one follows its caller's choice).
    #[cfg(feature = "otel")]
    #[arg(
        long,
        env = "TRIPLETRAIL_OTLP_SAMPLE",
        default_value_t = 1.0,
        value_name = "RATIO"
    )]
    otlp_sample: f64,
    /// Most rows (solutions, or triples of a CONSTRUCT / DESCRIBE) a query may answer. A
    /// request may ask for less with `max-rows=`; a larger result is refused with 413, or cut
    /// to the first rows with `partial=true` (header X-Result-Incomplete).
    #[arg(long, env = "TRIPLETRAIL_MAX_ROWS", value_name = "ROWS")]
    max_rows: Option<usize>,
    /// Who may call: a sparql-guard configuration file (JSON; its `auth` object, or a bare one)
    /// with API keys (SHA-256 hashes from `tripletrail hash-key`), trusted headers from an
    /// authenticating proxy and anonymous access. Reloaded when it changes. With it, every
    /// request but /$/ping and /$/ready needs an identity, writes need the group
    /// tripletrail:writer or tripletrail:admin, and admin changes tripletrail:admin; --update-token
    /// and --admin-token keep working as keys of those groups.
    #[arg(long, env = "TRIPLETRAIL_AUTH_CONFIG")]
    auth_config: Option<PathBuf>,
    /// Turn off the workbench's workspaces (/$/workspaces).
    #[arg(long, env = "TRIPLETRAIL_NO_WORKSPACES")]
    no_workspaces: bool,
    /// Where workspaces are kept (default <location>/.workspaces, or a temporary folder).
    #[arg(long, env = "TRIPLETRAIL_WORKSPACE_DIR")]
    workspace_dir: Option<PathBuf>,
    /// A scratch workspace unused for this many hours is deleted (saved ones are kept).
    #[arg(long, env = "TRIPLETRAIL_WORKSPACE_TTL_HOURS", default_value_t = 24.0)]
    workspace_ttl_hours: f64,
    /// Most workspaces on the server, and per caller.
    #[arg(long, env = "TRIPLETRAIL_WORKSPACE_MAX", default_value_t = 100)]
    workspace_max: usize,
    #[arg(
        long,
        env = "TRIPLETRAIL_WORKSPACE_MAX_PER_CALLER",
        default_value_t = 10
    )]
    workspace_max_per_caller: usize,
    /// Most triples in one workspace's files, a sample of a dataset included.
    #[arg(
        long,
        env = "TRIPLETRAIL_WORKSPACE_MAX_TRIPLES",
        default_value_t = 1_000_000
    )]
    workspace_max_triples: usize,
    /// Most megabytes of files in one workspace.
    #[arg(long, env = "TRIPLETRAIL_WORKSPACE_MAX_MB", default_value_t = 256)]
    workspace_max_mb: u64,
    /// Most quads an overlay workspace may change (add or remove) on top of its dataset, kept
    /// in memory: further updates are refused until the changes are dropped.
    #[arg(
        long,
        env = "TRIPLETRAIL_WORKSPACE_MAX_OVERLAY_CHANGES",
        default_value_t = 10_000_000
    )]
    workspace_max_overlay_changes: usize,
    /// The TripleTrail Enterprise licence, as text (one line, `ttlic1.…`). Wins over
    /// --license-file. Without a valid licence the enterprise features are off and the open
    /// features work; the server never refuses to start over a licence.
    #[cfg(feature = "enterprise")]
    #[arg(long, env = "TRIPLETRAIL_LICENSE", hide_env_values = true)]
    license: Option<String>,
    /// A file holding the TripleTrail Enterprise licence.
    #[cfg(feature = "enterprise")]
    #[arg(long, env = "TRIPLETRAIL_LICENSE_FILE", value_name = "FILE")]
    license_file: Option<PathBuf>,
    /// The audit log (TripleTrail Enterprise, #207): a JSON line for each change and admin call
    /// (who, when, what, the outcome), appended to FILE and chained by SHA-256 hashes so that
    /// a removed or changed line shows; `tripletrail audit verify FILE` checks it.
    #[cfg(feature = "enterprise")]
    #[arg(long, env = "TRIPLETRAIL_AUDIT_LOG", value_name = "FILE")]
    audit_log: Option<PathBuf>,
    /// The audit log also gets a line for each query and other read of a dataset (who read
    /// which dataset, when, the query's hash): for data whose reading must be accounted for.
    #[cfg(feature = "enterprise")]
    #[arg(long, env = "TRIPLETRAIL_AUDIT_READS", requires = "audit_log")]
    audit_reads: bool,
    /// Also send each audit line to a syslog collector: udp://HOST[:PORT] or
    /// tcp://HOST[:PORT] (port 514 by default), as RFC 5424 messages. Lines are sent from a
    /// queue and dropped when it is full or the collector is down; the file keeps them all.
    #[cfg(feature = "enterprise")]
    #[arg(
        long,
        env = "TRIPLETRAIL_AUDIT_SYSLOG",
        value_name = "URL",
        requires = "audit_log"
    )]
    audit_syslog: Option<String>,
    /// Per-dataset limits: a JSON file with `default` and `datasets` (by name) entries of
    /// maxConcurrent, maxQueued, queueTimeout, timeout, maxTimeout, maxRows, maxUpload,
    /// queryMemory, and the quotas maxDisk and maxMemory (writes that add data get 507 while over one), and
    /// `callers` limits per agent, group or anonymous client.
    /// A request over maxConcurrent waits in a queue of maxQueued (default 0) for up to
    /// queueTimeout seconds, then gets 429. Reloaded when it changes; what it leaves out comes
    /// from --timeout, --max-rows and --max-upload.
    #[arg(long, env = "TRIPLETRAIL_LIMITS", value_name = "FILE")]
    limits: Option<PathBuf>,
    /// Serve HTTPS: the certificate chain (PEM), with --tls-key. Both files are read again
    /// when they change or on SIGHUP. A reverse proxy in front stays the recommended setup
    /// for public servers.
    #[arg(
        long,
        env = "TRIPLETRAIL_TLS_CERT",
        value_name = "FILE",
        requires = "tls_key"
    )]
    tls_cert: Option<PathBuf>,
    /// The private key (PEM: PKCS#8, PKCS#1 or SEC1) for --tls-cert.
    #[arg(
        long,
        env = "TRIPLETRAIL_TLS_KEY",
        value_name = "FILE",
        requires = "tls_cert"
    )]
    tls_key: Option<PathBuf>,
    /// Remote SPARQL endpoints that `SERVICE <...>` may call: IRI prefixes, repeated or
    /// comma-separated (e.g. https://query.wikidata.org/sparql). None by default, so remote
    /// SERVICE calls fail.
    #[cfg(feature = "remote-service")]
    #[arg(
        long = "service-allow",
        env = "TRIPLETRAIL_SERVICE_ALLOW",
        value_delimiter = ',',
        value_name = "IRI"
    )]
    service_allow: Vec<String>,
    /// Longest one remote SERVICE call may take, in seconds.
    #[cfg(feature = "remote-service")]
    #[arg(long, env = "TRIPLETRAIL_SERVICE_TIMEOUT", default_value_t = 30.0)]
    service_timeout: f64,
    /// Largest answer read from one remote SERVICE call, in bytes.
    #[cfg(feature = "remote-service")]
    #[arg(long, env = "TRIPLETRAIL_SERVICE_MAX_BYTES", default_value_t = 64 << 20)]
    service_max_bytes: usize,
    /// Reuse a remote SERVICE answer for the same endpoint and query for this many seconds
    /// (0, the default: never).
    #[cfg(feature = "remote-service")]
    #[arg(long, env = "TRIPLETRAIL_SERVICE_CACHE_SECONDS", default_value_t = 0.0)]
    service_cache_seconds: f64,
    /// A PostgreSQL database that queries may read live through Facade-X, `NAME=URL`
    /// (`SERVICE <x-sparql-anything:table=rail.station,database=NAME>`, or `sql=`).
    /// Repeatable. Any caller may run read-only queries on it: give it a role that may read
    /// only what callers may see.
    #[cfg(feature = "postgres")]
    #[arg(long = "fx-connection", value_name = "NAME=URL")]
    fx_connections: Vec<String>,
    /// Longest one Facade-X database query may run, in seconds.
    #[cfg(feature = "postgres")]
    #[arg(long = "fx-timeout", default_value_t = 30.0)]
    fx_timeout: f64,
    /// Most rows one Facade-X database query may read.
    #[cfg(feature = "postgres")]
    #[arg(long = "fx-max-rows", default_value_t = 100_000)]
    fx_max_rows: usize,
    /// Most bytes one Facade-X database query may read (as PostgreSQL text).
    #[cfg(feature = "postgres")]
    #[arg(long = "fx-max-bytes", default_value_t = 100_000_000)]
    fx_max_bytes: usize,
    /// Most PostgreSQL sessions open at once per --fx-connection; more calls wait.
    #[cfg(feature = "postgres")]
    #[arg(long = "fx-sessions", default_value_t = 4)]
    fx_sessions: usize,
    /// Reuse a Facade-X database result for the same connection and statement for this many
    /// seconds (0, the default: always read live).
    #[cfg(feature = "postgres")]
    #[arg(long = "fx-cache-seconds", default_value_t = 0.0)]
    fx_cache_seconds: f64,
    /// A named virtual graph, `IRI=SERVICE`: `GRAPH <IRI>` in a query reads the Facade-X
    /// SERVICE (`x-sparql-anything:table=rail.station,database=rail`). Repeatable.
    #[cfg(feature = "postgres")]
    #[arg(long = "fx-graph", value_name = "IRI=SERVICE")]
    fx_graphs: Vec<String>,
}

/// How `load` maps the rows of GeoParquet files to triples.
#[derive(clap::Args, Clone, Default)]
struct ParquetArgs {
    /// GeoParquet: the subject IRI of a row, with {column} placeholders (percent-encoded), e.g.
    /// http://example.org/station/{id}, or {+column} for a value as it is, e.g. {+feature} for
    /// a column of IRIs (a blank node per row if omitted).
    #[arg(long)]
    subject: Option<String>,
    /// GeoParquet: prepended to column names to make predicates.
    #[arg(long, default_value = "urn:x-tripletrail:column:")]
    predicate_prefix: String,
    /// GeoParquet: an rdf:type of every row.
    #[arg(long)]
    class: Option<String>,
    /// GeoParquet: only rows whose geometry intersects this box: minx,miny,maxx,maxy.
    #[arg(long)]
    bbox: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum StoreKind {
    Oxigraph,
    #[value(name = "tripletrail")]
    TripleTrail,
}

fn default_threads() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

/// A number of seconds: finite, not negative and small enough for a [`Duration`].
fn parse_keep(value: &str) -> std::result::Result<usize, String> {
    match value.parse::<usize>() {
        Ok(0) => Err("must be at least 1: 0 would delete the backup just written".to_owned()),
        Ok(n) => Ok(n),
        Err(e) => Err(e.to_string()),
    }
}

fn parse_seconds(value: &str) -> std::result::Result<Duration, String> {
    let seconds: f64 = value.parse().map_err(|e| format!("{e}"))?;
    Duration::try_from_secs_f64(seconds).map_err(|e| format!("{e}"))
}

/// A size in bytes: a whole number with an optional suffix K, M, G or T (powers of 1024,
/// with or without a following `B` or `iB`).
fn parse_size(value: &str) -> std::result::Result<usize, String> {
    let v = value.trim();
    let v = v
        .strip_suffix("iB")
        .or_else(|| v.strip_suffix('B'))
        .unwrap_or(v);
    let (digits, shift) = match v.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some('K') => (&v[..v.len() - 1], 10),
        Some('M') => (&v[..v.len() - 1], 20),
        Some('G') => (&v[..v.len() - 1], 30),
        Some('T') => (&v[..v.len() - 1], 40),
        _ => (v, 0),
    };
    let n: usize = digits
        .trim()
        .parse()
        .map_err(|_| format!("not a size: {value} (e.g. 32G, 512M)"))?;
    n.checked_mul(1 << shift)
        .ok_or_else(|| format!("too large: {value}"))
}

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Opens a dataset. On disk, its routing networks and contraction hierarchies are kept in
/// `<location>/<name>.networks`, so a restart does not rebuild them.
fn open(location: Option<&Path>, name: &str, kind: StoreKind) -> Result<Engine> {
    let Some(dir) = location else {
        return Ok(match kind {
            StoreKind::TripleTrail => Engine::new_tripletrail(),
            StoreKind::Oxigraph => Engine::from_store(Store::new()?),
        });
    };
    let networks = dir.join(format!("{name}.networks"));
    Ok(match kind {
        StoreKind::TripleTrail => {
            let store = tripletrail::store::Store::open(dir.join(name))?;
            Engine::with_network_dir(store, networks)?
        }
        StoreKind::Oxigraph => Engine::with_network_dir(open_on_disk(&dir.join(name))?, networks)?,
    })
}

#[cfg(feature = "rocksdb")]
fn open_on_disk(path: &Path) -> Result<Store> {
    std::fs::create_dir_all(path)?;
    Ok(Store::open(path)?)
}

#[cfg(not(feature = "rocksdb"))]
fn open_on_disk(_: &Path) -> Result<Store> {
    Err("on-disk datasets need the rocksdb feature (cargo build --features rocksdb)".into())
}

fn load_file(engine: &Engine, path: &Path, graph: Option<&str>) -> Result<()> {
    load_file_with(engine, path, graph, &ParquetArgs::default())
}

#[cfg(feature = "geoparquet")]
fn load_geoparquet(
    engine: &Engine,
    path: &Path,
    graph: Option<&str>,
    args: &ParquetArgs,
) -> Result<()> {
    use tripletrail::geoparquet::ImportMapping;
    let bbox = match &args.bbox {
        None => None,
        Some(b) => {
            let v: Vec<f64> = b
                .split(',')
                .map(|v| v.trim().parse::<f64>())
                .collect::<std::result::Result<_, _>>()?;
            Some(<[f64; 4]>::try_from(v).map_err(|_| "--bbox takes minx,miny,maxx,maxy")?)
        }
    };
    let mapping = ImportMapping {
        subject: args.subject.clone(),
        predicate_prefix: args.predicate_prefix.clone(),
        class: args.class.as_deref().map(NamedNode::new).transpose()?,
        graph: graph.map(NamedNode::new).transpose()?,
        bbox,
        geometry_columns: None,
    };
    let started = std::time::Instant::now();
    let stats = engine.load_geoparquet(std::fs::File::open(path)?, &mapping)?;
    eprintln!(
        "loaded {} in {:.1} s: {} rows, {} triples{}{}",
        path.display(),
        started.elapsed().as_secs_f64(),
        stats.rows,
        stats.triples,
        if stats.skipped_rows > 0 {
            format!(", {} rows outside the box", stats.skipped_rows)
        } else {
            String::new()
        },
        if stats.invalid_geometries > 0 {
            format!(", {} invalid geometries left out", stats.invalid_geometries)
        } else {
            String::new()
        },
    );
    Ok(())
}

#[cfg(feature = "geoparquet")]
fn export(
    engine: &Engine,
    query: &str,
    output: &Path,
    row_group_size: usize,
    projjson: Option<&Path>,
) -> Result<()> {
    use tripletrail::geoparquet::ExportOptions;
    let query = match query.strip_prefix('@') {
        Some(file) => std::fs::read_to_string(file)?,
        None => query.to_owned(),
    };
    let options = ExportOptions {
        row_group_size,
        projjson: projjson.map(std::fs::read_to_string).transpose()?,
        ..ExportOptions::default()
    };
    let started = std::time::Instant::now();
    let file = std::io::BufWriter::new(std::fs::File::create(output)?);
    let stats = engine.export_geoparquet(&query, file, &options)?;
    eprintln!(
        "wrote {} in {:.1} s: {} rows, geometry columns: {}",
        output.display(),
        started.elapsed().as_secs_f64(),
        stats.rows,
        if stats.geometry_columns.is_empty() {
            "none".to_owned()
        } else {
            stats.geometry_columns.join(", ")
        }
    );
    Ok(())
}

/// How often a load writes its progress line.
const LOAD_PROGRESS_EVERY: Duration = Duration::from_secs(60);

/// Runs `load` while another thread writes a line to standard error every
/// [`LOAD_PROGRESS_EVERY`]: the store's step and triples read (#232), and how much of the
/// file has been read. A load that ends sooner writes none.
fn with_load_progress(
    engine: &Engine,
    path: &Path,
    size: u64,
    read: &std::sync::atomic::AtomicU64,
    started: std::time::Instant,
    load: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let (done, wait) = std::sync::mpsc::channel::<()>();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                wait.recv_timeout(LOAD_PROGRESS_EVERY)
            {
                let read = read.load(std::sync::atomic::Ordering::Relaxed);
                eprintln!(
                    "{}",
                    load_progress_line(engine, path, read, size, started.elapsed())
                );
            }
        });
        let result = load();
        drop(done);
        result
    })
}

/// One progress line of a load, as `with_load_progress` writes it.
fn load_progress_line(
    engine: &Engine,
    path: &Path,
    read: u64,
    size: u64,
    elapsed: Duration,
) -> String {
    let store = match engine.store() {
        tripletrail::store::Dataset::TripleTrail(s) => {
            let p = s.load_progress();
            format!("{}, {} triples, ", p.phase, grouped(p.quads))
        }
        _ => String::new(),
    };
    let gb = |b: u64| b as f64 / 1e9;
    // The size of a pipe, or of a file whose metadata could not be read, is not known.
    let read = if size > 0 {
        format!(
            "{:.1} of {:.1} GB read ({:.0} %)",
            gb(read),
            gb(size),
            100.0 * read as f64 / size as f64
        )
    } else {
        format!("{:.1} GB read", gb(read))
    };
    format!(
        "loading {}: {store}{read}, {} s",
        path.display(),
        elapsed.as_secs()
    )
}

/// `n` with a comma between each group of three digits.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A reader that counts the bytes read through it.
struct Counted<'a, R> {
    inner: R,
    read: &'a std::sync::atomic::AtomicU64,
}

impl<R: std::io::Read> std::io::Read for Counted<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read
            .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(n)
    }
}

fn load_file_with(
    engine: &Engine,
    path: &Path,
    graph: Option<&str>,
    #[cfg_attr(not(feature = "geoparquet"), allow(unused_variables))] parquet: &ParquetArgs,
) -> Result<()> {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    if extension.eq_ignore_ascii_case("parquet") {
        #[cfg(feature = "geoparquet")]
        return load_geoparquet(engine, path, graph, parquet);
        #[cfg(not(feature = "geoparquet"))]
        return Err("GeoParquet needs the geoparquet feature".into());
    }
    let format = tripletrail_server::rdf_formats::Format::from_extension(extension)
        .ok_or_else(|| format!("unknown RDF format for {}", path.display()))?;
    let parser = |format| -> Result<RdfParser> {
        let parser = RdfParser::from_format(format);
        Ok(match graph {
            Some(g) => parser.with_default_graph(NamedNode::new(g)?),
            None => parser,
        })
    };
    let started = std::time::Instant::now();
    match format {
        tripletrail_server::rdf_formats::Format::Rdf(format) => {
            let file = std::fs::File::open(path)?;
            let size = file.metadata().map(|m| m.len()).unwrap_or(0);
            let read = std::sync::atomic::AtomicU64::new(0);
            let reader = Counted {
                inner: file,
                read: &read,
            };
            with_load_progress(engine, path, size, &read, started, || {
                engine.load(
                    parser(format)?,
                    std::io::BufReader::with_capacity(1 << 20, reader),
                )?;
                Ok(())
            })?;
        }
        // RDF/JSON and TriX (#831): read whole, and loaded as N-Quads.
        extra => {
            let bytes = std::fs::read(path)?;
            let (format, nquads) = tripletrail_server::rdf_formats::as_oxigraph(extra, &bytes)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            engine.load(parser(format)?, &nquads[..])?;
        }
    }
    eprintln!(
        "loaded {} in {:.1} s",
        path.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// The graph shapes files are loaded into for `validate`.
const SHAPES_GRAPH: &str = "urn:x-tripletrail:shapes";

/// The graph `--lookup` files are loaded into for `validate`.
const LOOKUP_GRAPH: &str = "urn:x-tripletrail:lookup";

#[allow(clippy::too_many_arguments)]
fn validate(
    data: &[PathBuf],
    shapes: &[PathBuf],
    lookup: &[PathBuf],
    lookup_graphs: &[String],
    location: Option<&Path>,
    store: StoreKind,
    dataset: &str,
    shapes_graph: Option<&str>,
    data_graph: Option<&str>,
    report: Option<&Path>,
    stats: bool,
) -> Result<bool> {
    use tripletrail::oxigraph::io::RdfSerializer;
    use tripletrail::oxigraph::model::GraphNameRef;
    let engine = open(location, dataset, store)?;
    for file in data {
        load_file(&engine, file, data_graph)?;
    }
    for file in shapes {
        load_file(&engine, file, Some(shapes_graph.unwrap_or(SHAPES_GRAPH)))?;
    }
    for file in lookup {
        load_file(&engine, file, Some(LOOKUP_GRAPH))?;
    }
    let mut lookups = lookup_graphs
        .iter()
        .map(|g| NamedNode::new(g.as_str()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if !lookup.is_empty() {
        lookups.push(NamedNode::new_unchecked(LOOKUP_GRAPH));
    }
    let shapes = NamedNode::new(shapes_graph.unwrap_or(SHAPES_GRAPH))?;
    let data = data_graph.map(NamedNode::new).transpose()?;
    let data = data
        .as_ref()
        .map_or(GraphNameRef::DefaultGraph, |g| g.as_ref().into());
    let format = match report {
        Some(path) => path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(RdfFormat::from_extension)
            .ok_or_else(|| format!("unknown RDF format for {}", path.display()))?,
        None => RdfFormat::Turtle,
    };
    let out: Box<dyn std::io::Write> = match report {
        Some(path) => Box::new(std::fs::File::create(path)?),
        None => Box::new(std::io::stdout().lock()),
    };
    let mut writer = RdfSerializer::from_format(format)
        .with_prefix("sh", "http://www.w3.org/ns/shacl#")?
        .with_prefix("ttsh", "http://w3id.org/tripletrail/shacl#")?
        .for_writer(std::io::BufWriter::new(out));
    let mut error = None;
    let mut results = 0usize;
    let started = std::time::Instant::now();
    let conforms = engine.validate_report_with(&shapes, data, &lookups, stats, &mut |t| {
        if t.predicate.as_str() == "http://www.w3.org/ns/shacl#result" {
            results += 1;
        }
        if error.is_none()
            && let Err(e) = writer.serialize_triple(&t)
        {
            error = Some(e);
        }
    })?;
    if let Some(e) = error {
        return Err(e.into());
    }
    writer.finish()?;
    eprintln!(
        "{} in {:.1} s: {results} results",
        if conforms {
            "conforms"
        } else {
            "does not conform"
        },
        started.elapsed().as_secs_f64()
    );
    Ok(conforms)
}

/// `tripletrail entail --regime`.
#[derive(Clone, Copy, clap::ValueEnum)]
enum EntailRegime {
    Rdfs,
    OwlRl,
}

fn entail(
    data: &[PathBuf],
    location: Option<&Path>,
    dataset: &str,
    into: &str,
    output: Option<&Path>,
    regime: tripletrail::store::entail::Regime,
) -> Result<()> {
    use tripletrail::oxigraph::io::RdfSerializer;
    if location.is_some() == !data.is_empty() {
        return Err("give either --location or --data files".into());
    }
    if location.is_some() && output.is_some() {
        return Err(
            "--output is for --data files; with --location the triples go to --into".into(),
        );
    }
    let into = NamedNode::new(into).map_err(|e| format!("--into {into:?}: {e}"))?;
    if let Some(dir) = location
        && !dir.join(dataset).is_dir()
    {
        return Err(format!("no dataset {dataset:?} in {}", dir.display()).into());
    }
    let engine = open(location, dataset, StoreKind::TripleTrail)?;
    for file in data {
        load_file(&engine, file, None)?;
    }
    let started = std::time::Instant::now();
    let done = engine.entail(into.as_ref(), regime)?;
    eprintln!(
        "entailed {} triples into <{}> in {:.1} s ({} added, {} removed)",
        done.triples,
        into.as_str(),
        started.elapsed().as_secs_f64(),
        done.added,
        done.removed,
    );
    if regime == tripletrail::store::entail::Regime::OwlRl {
        report_inconsistencies(&engine, into.as_ref())?;
    }
    if location.is_some() {
        return Ok(());
    }
    let format = match output {
        Some(path) => path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(RdfFormat::from_extension)
            .filter(|f| !f.supports_datasets())
            .ok_or_else(|| format!("unknown RDF triple format for {}", path.display()))?,
        None => RdfFormat::NTriples,
    };
    let out: Box<dyn std::io::Write> = match output {
        Some(path) => Box::new(std::fs::File::create(path)?),
        None => Box::new(std::io::stdout().lock()),
    };
    let mut writer = RdfSerializer::from_format(format).for_writer(std::io::BufWriter::new(out));
    for q in engine
        .store()
        .quads_for_pattern(None, None, None, Some(into.as_ref().into()))
    {
        writer.serialize_triple(&tripletrail::oxigraph::model::Triple::from(q?))?;
    }
    writer.finish()?;
    Ok(())
}

/// Prints what the OWL 2 RL consistency rules find, the first 20, to standard error. Like
/// Jena's validity report it fails nothing: the entailment is written all the same.
fn report_inconsistencies(engine: &Engine, entailed: NamedNodeRef<'_>) -> Result<()> {
    const SHOWN: usize = 20;
    let found = engine.inconsistencies(SHOWN, Some(entailed))?;
    let (count, first) = (found.count, found.first);
    if count == 0 {
        return Ok(());
    }
    let at_least = if found.at_least { "at least " } else { "" };
    eprintln!("the data contradicts the OWL 2 RL rules {at_least}{count} times:");
    for i in &first {
        let terms: Vec<String> = i.terms.iter().map(ToString::to_string).collect();
        eprintln!("  {}: {}", i.rule, terms.join(" "));
    }
    if count > first.len() {
        eprintln!("  and {at_least}{} more", count - first.len());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn infer(
    data: &[PathBuf],
    shapes: &[PathBuf],
    location: Option<&Path>,
    store: StoreKind,
    dataset: &str,
    shapes_graph: Option<&str>,
    data_graph: Option<&str>,
    into: Option<&str>,
    max_passes: usize,
    all_constructed: bool,
    naive: bool,
    ordered: bool,
    output: Option<&Path>,
    verbose: bool,
    provenance: Option<&Path>,
) -> Result<()> {
    use tripletrail::oxigraph::io::RdfSerializer;
    use tripletrail::oxigraph::model::GraphNameRef;
    let engine = open(location, dataset, store)?;
    for file in data {
        load_file(&engine, file, data_graph)?;
    }
    for file in shapes {
        load_file(&engine, file, Some(shapes_graph.unwrap_or(SHAPES_GRAPH)))?;
    }
    let shapes = NamedNode::new(shapes_graph.unwrap_or(SHAPES_GRAPH))?;
    let data = data_graph.map(NamedNode::new).transpose()?;
    let data = data
        .as_ref()
        .map_or(GraphNameRef::DefaultGraph, |g| g.as_ref().into());
    let into = into.map(NamedNode::new).transpose()?;
    let started = std::time::Instant::now();
    let observer = |run: &tripletrail::shacl::RuleRun<'_>| {
        if verbose && run.done {
            eprintln!(
                "group {}{} pass {} {:8.2} s {:>9} foci {:>9} new  {} on {}",
                run.group,
                if run.recursive { "*" } else { "" },
                run.pass,
                run.elapsed.as_secs_f64(),
                run.focus_nodes,
                run.new,
                run.rule,
                run.shape
            );
        } else if verbose {
            eprintln!(
                "group {}{} pass {} running {} on {}",
                run.group,
                if run.recursive { "*" } else { "" },
                run.pass,
                run.rule,
                run.shape
            );
        }
    };
    let options = tripletrail::InferOptions {
        max_passes,
        include_existing: all_constructed,
        semi_naive: !naive,
        // Files are loaded into a store of their own: the rules can run on it (#260). A
        // store on disk is not changed: they run on a copy.
        in_store: location.is_none(),
        scratch: location.is_some(),
        sequential: ordered,
        plan: None,
    };
    let inferred = match provenance {
        None => engine.infer_with(
            &shapes,
            data,
            into.as_ref().map(|g| g.as_ref().into()),
            &options,
            &observer,
        )?,
        Some(path) => {
            let (inferred, origins) = engine.infer_traced(&shapes, data, &options, &observer)?;
            let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
            for (t, o) in inferred.iter().zip(&origins) {
                let mut line = tripletrail_server::provenance_json(o);
                line["triple"] = serde_json::Value::String(t.to_string());
                serde_json::to_writer(&mut out, &line)?;
                std::io::Write::write_all(&mut out, b"\n")?;
            }
            std::io::Write::flush(&mut out)?;
            if let Some(graph) = &into {
                engine.load_quads(inferred.iter().map(|t| t.clone().in_graph(graph.clone())))?;
            }
            inferred
        }
    };
    eprintln!(
        "{} {} triples in {:.1} s",
        if all_constructed {
            "constructed"
        } else {
            "inferred"
        },
        inferred.len(),
        started.elapsed().as_secs_f64()
    );
    if output.is_some() || into.is_none() {
        let format = match output {
            Some(path) => path
                .extension()
                .and_then(|e| e.to_str())
                .and_then(RdfFormat::from_extension)
                .ok_or_else(|| format!("unknown RDF format for {}", path.display()))?,
            None => RdfFormat::NTriples,
        };
        let out: Box<dyn std::io::Write> = match output {
            Some(path) => Box::new(std::fs::File::create(path)?),
            None => Box::new(std::io::stdout().lock()),
        };
        let mut writer =
            RdfSerializer::from_format(format).for_writer(std::io::BufWriter::new(out));
        for t in inferred.iter() {
            writer.serialize_triple(t)?;
        }
        writer.finish()?;
    }
    Ok(())
}

/// `NAME=VALUE` (with an optional `:ro` suffix for attach).
fn split_pair<'a>(arg: &'a str, what: &str) -> Result<(&'a str, &'a str)> {
    arg.split_once('=')
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .ok_or_else(|| format!("{what} takes NAME=VALUE, not {arg:?}").into())
}

/// The `--fx-*` options of `serve`.
#[cfg(feature = "postgres")]
fn database_sources(
    connections: &[String],
    timeout: f64,
    max_rows: usize,
    max_bytes: usize,
    sessions: usize,
    cache_seconds: f64,
    graphs: &[String],
) -> Result<tripletrail_server::database_sources::DatabaseSources> {
    use tripletrail_server::database_sources::{DatabaseSource, DatabaseSources};
    if max_rows == 0 {
        return Err("--fx-max-rows takes a positive number".into());
    }
    if max_bytes == 0 {
        return Err("--fx-max-bytes takes a positive number".into());
    }
    if sessions == 0 {
        return Err("--fx-sessions takes a positive number".into());
    }
    let mut sources = DatabaseSources {
        timeout: Duration::try_from_secs_f64(timeout)
            .ok()
            .filter(|t| !t.is_zero())
            .ok_or("--fx-timeout takes a positive number of seconds")?,
        max_rows,
        max_bytes,
        sessions,
        cache_ttl: Duration::try_from_secs_f64(cache_seconds)
            .map_err(|_| "--fx-cache-seconds takes a number of seconds")?,
        ..Default::default()
    };
    for g in graphs {
        let parsed = g.split_once('=').and_then(|(iri, service)| {
            let iri = tripletrail::oxigraph::model::NamedNode::new(iri.trim()).ok()?;
            let service = tripletrail::oxigraph::model::NamedNode::new(service.trim()).ok()?;
            service
                .as_str()
                .starts_with("x-sparql-anything:")
                .then_some((iri, service))
        });
        let Some(pair) = parsed else {
            return Err(format!(
                "--fx-graph {g:?}: give IRI=SERVICE, the SERVICE an x-sparql-anything: IRI"
            )
            .into());
        };
        sources.graphs.push(pair);
    }
    for c in connections {
        let Some((name, url)) = c.split_once('=').filter(|(n, _)| !n.trim().is_empty()) else {
            return Err(format!("--fx-connection {c:?}: give NAME=URL").into());
        };
        sources.connections.insert(
            name.trim().to_owned(),
            DatabaseSource {
                url: url.to_owned(),
            },
        );
    }
    Ok(sources)
}

fn serve(args: ServeArgs) -> Result<()> {
    let ServeArgs {
        config,
        bind,
        mut datasets,
        packs,
        location,
        read_only,
        no_warm_up,
        attach,
        aliases,
        union_default_graph,
        rdfs,
        owl_rl,
        text_properties,
        store,
        timeout,
        threads,
        query_threads,
        cors,
        update_token,
        max_body,
        max_upload,
        git_fetch_mb,
        network_cache_mb,
        result_cache_mb,
        executor,
        memory_limit,
        query_memory,
        lookup_joins,
        load,
        index_predicates,
        no_index_predicates,
        admin_token,
        backup_dir,
        backup_keep,
        wal_archive,
        keep_logs,
        vector_graph_min,
        #[cfg(feature = "replication")]
        sync_replicas,
        #[cfg(feature = "replication")]
        sync_timeout,
        #[cfg(feature = "replication")]
        follow,
        #[cfg(feature = "replication")]
        follow_token,
        #[cfg(feature = "replication")]
        follow_wait,
        functions,
        no_ui,
        mcp,
        matgui_url,
        map_tiles,
        map_attribution,
        map_basemaps,
        site_name,
        site_favicon,
        site_logo,
        site_logo_dark,
        site_color,
        log_requests,
        shutdown_grace,
        base_iris,
        see_other,
        catalog,
        slow_query_ms,
        #[cfg(feature = "otel")]
        otlp_endpoint,
        #[cfg(feature = "otel")]
        otlp_headers,
        #[cfg(feature = "otel")]
        otlp_service_name,
        #[cfg(feature = "otel")]
        otlp_sample,
        max_rows,
        auth_config,
        no_workspaces,
        workspace_dir,
        workspace_ttl_hours,
        workspace_max,
        workspace_max_per_caller,
        workspace_max_triples,
        workspace_max_mb,
        workspace_max_overlay_changes,
        #[cfg(feature = "enterprise")]
        license,
        #[cfg(feature = "enterprise")]
        license_file,
        #[cfg(feature = "enterprise")]
        audit_log,
        #[cfg(feature = "enterprise")]
        audit_reads,
        #[cfg(feature = "enterprise")]
        audit_syslog,
        limits,
        tls_cert,
        tls_key,
        #[cfg(feature = "remote-service")]
        service_allow,
        #[cfg(feature = "remote-service")]
        service_timeout,
        #[cfg(feature = "remote-service")]
        service_max_bytes,
        #[cfg(feature = "remote-service")]
        service_cache_seconds,
        #[cfg(feature = "postgres")]
        fx_connections,
        #[cfg(feature = "postgres")]
        fx_timeout,
        #[cfg(feature = "postgres")]
        fx_max_rows,
        #[cfg(feature = "postgres")]
        fx_max_bytes,
        #[cfg(feature = "postgres")]
        fx_sessions,
        #[cfg(feature = "postgres")]
        fx_cache_seconds,
        #[cfg(feature = "postgres")]
        fx_graphs,
    } = args;
    #[cfg(feature = "postgres")]
    let database_sources = database_sources(
        &fx_connections,
        fx_timeout,
        fx_max_rows,
        fx_max_bytes,
        fx_sessions,
        fx_cache_seconds,
        &fx_graphs,
    )?;
    // `--cors` alone (or TRIPLETRAIL_CORS=true, as before origins could be listed) is every origin.
    let cors = cors.unwrap_or_default();
    let cors_all = cors
        .iter()
        .any(|o| matches!(o.as_str(), "*" | "true" | "1"));
    let cors_origins: Vec<String> = cors
        .into_iter()
        .filter(|o| !o.is_empty() && !matches!(o.as_str(), "*" | "true" | "1" | "false" | "0"))
        .collect();
    tripletrail::pgr::NetworkCache::set_default_budget(match network_cache_mb {
        0 => usize::MAX,
        mb => mb.saturating_mul(1 << 20),
    });
    tripletrail::Engine::set_default_result_cache(result_cache_mb.saturating_mul(1 << 20));
    tripletrail::Engine::set_default_executor_ids(executor == "ids");
    tripletrail::store::par::set_query_threads(query_threads);
    if let Some(bytes) = memory_limit {
        tripletrail::store::Store::set_memory_limit(bytes);
    }
    if let Some(bytes) = query_memory {
        tripletrail::set_query_memory(bytes);
    }
    tripletrail::set_lookup_joins(lookup_joins);
    #[cfg(feature = "replication")]
    let sync_timeout = Duration::try_from_secs_f64(sync_timeout)
        .map_err(|_| "--sync-timeout takes a positive number of seconds")?;
    #[cfg(not(feature = "replication"))]
    let (sync_replicas, sync_timeout) = (0, Duration::from_secs(10));
    #[cfg(feature = "replication")]
    let follows = {
        let wait = Duration::try_from_secs_f64(follow_wait)
            .map_err(|_| "--follow-wait takes a positive number of seconds")?;
        // `NAME=URL` starts a dataset; a bare URL after it (`kg=URL1,URL2`), or the same
        // NAME again, adds another URL of its leader.
        let mut follows: Vec<tripletrail_server::Follow> = Vec::new();
        for f in &follow {
            let bare = f.starts_with("http://") || f.starts_with("https://");
            if bare {
                follows
                    .last_mut()
                    .ok_or_else(|| {
                        format!("--follow {f:?}: give NAME=URL (http://leader:7878/ds)")
                    })?
                    .add_url(f)?;
                continue;
            }
            let parsed = tripletrail_server::Follow::parse(
                f,
                follow_token.clone().filter(|t| !t.is_empty()),
                wait,
            )?;
            match follows.iter_mut().find(|g| g.name == parsed.name) {
                Some(same) => same.add_url(parsed.url())?,
                None => follows.push(parsed),
            }
        }
        if !follows.is_empty() && location.is_none() {
            return Err("--follow copies datasets into --location: give one".into());
        }
        for f in &follows {
            if datasets.contains(&f.name)
                || attach
                    .iter()
                    .any(|a| a.split_once('=').is_some_and(|(n, _)| n == f.name))
            {
                return Err(format!(
                    "--follow {}: {} is also a --dataset or --attach",
                    f.name, f.name
                )
                .into());
            }
        }
        // Replicas promoted to leaders earlier are served from their copies (#217): a
        // --dataset of the same name would open (or create) another directory.
        if let Some(location) = &location {
            for name in tripletrail_server::promoted_replicas(location) {
                if datasets.contains(&name) {
                    return Err(format!(
                        "--dataset {name}: {name} is a read replica promoted to a leader ({}): it is served from its copy, so leave out --dataset {name}",
                        location.join(format!("{name}.replica")).display()
                    )
                    .into());
                }
            }
        }
        follows
    };
    // Replicas promoted to leaders: served without a --dataset or --follow (#217).
    #[cfg(feature = "replication")]
    let following = !follows.is_empty()
        || location
            .as_deref()
            .is_some_and(|l| !tripletrail_server::promoted_replicas(l).is_empty());
    #[cfg(not(feature = "replication"))]
    let following = false;
    if datasets.is_empty() && attach.is_empty() && !following {
        datasets.push("ds".to_owned());
    }
    if read_only && location.is_none() {
        return Err("--read-only opens datasets of --location".into());
    }
    // `--pack [NAME=]DIR` (#148): the first --dataset without a name.
    let packs: Vec<(String, PathBuf)> = packs
        .iter()
        .map(|p| match p.split_once('=') {
            Some((name, dir)) if !name.contains(['/', '\\']) => {
                Ok((name.to_owned(), PathBuf::from(dir)))
            }
            _ => datasets
                .first()
                .map(|name| (name.clone(), PathBuf::from(p)))
                .ok_or_else(|| format!("--pack {p}: name the dataset, NAME=DIR")),
        })
        .collect::<std::result::Result<_, _>>()?;
    if !packs.is_empty() && read_only {
        return Err("--pack installs into a dataset, which --read-only does not change".into());
    }
    if !load.is_empty() && (datasets.is_empty() || read_only) {
        return Err("--load loads into the first --dataset, which must be writable".into());
    }
    let union = move |name: &str| union_default_graph.iter().any(|n| n == "*" || n == name);
    use tripletrail::store::entail::Regime;
    let kept: Vec<(&str, &String, Regime)> = rdfs
        .iter()
        .map(|r| ("--rdfs", r, Regime::Rdfs))
        .chain(owl_rl.iter().map(|r| ("--owl-rl", r, Regime::OwlRl)))
        .collect();
    let mut entailed = Vec::new();
    for &(flag, r, regime) in &kept {
        let (name, graph) = r.split_once('=').unwrap_or((r, "urn:tripletrail:inferred"));
        let graph = NamedNode::new(graph).map_err(|e| format!("{flag} {r:?}: {e}"))?;
        if entailed.iter().any(|(n, _, _)| n == name) {
            return Err(
                format!("{flag} {name}: {name} is named twice in --rdfs and --owl-rl").into(),
            );
        }
        entailed.push((name.to_owned(), graph, regime));
    }
    let mut text_by_dataset: Vec<(String, NamedNode)> = Vec::new();
    for t in &text_properties {
        let (name, p) = t
            .split_once('=')
            .ok_or_else(|| format!("--text-properties {t:?}: NAME=IRI"))?;
        let p = NamedNode::new(p).map_err(|e| format!("--text-properties {t:?}: {e}"))?;
        text_by_dataset.push((name.to_owned(), p));
    }
    let entail = move |name: &str, engine: &Engine| -> Result<()> {
        let Some((_, graph, regime)) = entailed
            .iter()
            .find(|(n, _, _)| n == name)
            .or_else(|| entailed.iter().find(|(n, _, _)| n == "*"))
        else {
            return Ok(());
        };
        let started = std::time::Instant::now();
        let done = engine
            .set_entailment(Some(graph.clone()), *regime)
            .map_err(|e| format!("entailment of {name}: {e}"))?
            .expect("set");
        eprintln!(
            "{name}: {} entailment kept in <{}>: {} triples in {:.1} s",
            tripletrail::regime_name(*regime),
            graph.as_str(),
            done.triples,
            started.elapsed().as_secs_f64()
        );
        Ok(())
    };
    let mut attached = Vec::new();
    for a in &attach {
        let (name, dir) = split_pair(a, "--attach")?;
        let (dir, mode) = match dir.strip_suffix(":ro") {
            Some(d) => (d, Mode::ReadOnly),
            None => (dir.strip_suffix(":rw").unwrap_or(dir), Mode::ReadWrite),
        };
        attached.push((name.to_owned(), PathBuf::from(dir), mode));
    }
    // Each --rdfs and --owl-rl names a writable dataset (a typo would serve without the
    // entailment).
    for &(flag, r, _) in &kept {
        let name = r.split_once('=').map_or(r.as_str(), |(n, _)| n);
        let writable = (!read_only && datasets.iter().any(|d| d == name))
            || attached
                .iter()
                .any(|(n, _, mode)| n == name && *mode == Mode::ReadWrite);
        if name != "*" && !writable {
            return Err(format!(
                "{flag} {name}: no writable dataset {name} (of --dataset and --attach)"
            )
            .into());
        }
    }
    let mut alias_pairs = Vec::new();
    for a in &aliases {
        let (alias, target) = split_pair(a, "--alias")?;
        alias_pairs.push((alias.to_owned(), target.to_owned()));
    }
    let catalog_text = match &catalog {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("--catalog {}: {e}", path.display()))?;
            tripletrail_server::check_catalog(&text)
                .map_err(|e| format!("--catalog {}: {e}", path.display()))?;
            Some(text)
        }
        None => None,
    };
    let mut base_iri_pairs = Vec::new();
    for b in &base_iris {
        let (dataset, prefix) = split_pair(b, "--base-iri")?;
        base_iri_pairs.push((prefix.to_owned(), dataset.to_owned()));
    }
    let indexed = IndexedPredicates {
        only: (!index_predicates.is_empty()).then(|| index_predicates.into_iter().collect()),
        except: no_index_predicates.into_iter().collect(),
    };
    let https = tls_cert.is_some();
    let server = Server::bind(
        &bind,
        Vec::new(),
        Options {
            timeout,
            threads,
            config_file: config,
            cors: cors_all,
            cors_origins,
            update_token: update_token.filter(|t| !t.is_empty()),
            max_body: Some(max_body),
            max_upload,
            git_fetch: (git_fetch_mb > 0).then_some(git_fetch_mb << 20),
            admin_token: admin_token.filter(|t| !t.is_empty()),
            backup_dir: backup_dir.or_else(|| location.as_ref().map(|l| l.join("backups"))),
            backup_keep,
            wal_archive,
            keep_logs,
            vector_graph_min,
            sync_replicas,
            sync_timeout,
            functions: read_functions(&functions)?,
            location: location.clone(),
            indexed: indexed.clone(),
            text_properties: text_by_dataset.clone(),
            ui: !no_ui,
            mcp,
            matgui_url,
            map_attribution: map_attribution
                .or_else(|| (map_tiles == OSM_TILES).then(|| OSM_ATTRIBUTION.to_owned())),
            map_tiles: Some(map_tiles)
                .filter(|t| !t.trim().is_empty() && !t.trim().eq_ignore_ascii_case("none")),
            map_basemaps: read_basemaps(&map_basemaps)?,
            branding: tripletrail_server::branding::Branding::new(
                site_name.as_deref(),
                site_favicon.as_deref(),
                site_logo.as_deref(),
                site_logo_dark.as_deref(),
                site_color.as_deref(),
            )?,
            log_requests,
            shutdown_grace: Duration::try_from_secs_f64(shutdown_grace)
                .map_err(|_| "--shutdown-grace takes a positive number of seconds")?,
            base_iris: base_iri_pairs,
            see_other,
            catalog: catalog_text,
            slow_query: slow_query_ms.map(Duration::from_millis),
            #[cfg(feature = "otel")]
            otlp: tripletrail_server::otel::OtlpOptions {
                endpoint: otlp_endpoint
                    .filter(|e| !e.trim().is_empty())
                    .map(|e| tripletrail_server::otel::OtlpOptions::traces_url(&e)),
                headers: otlp_headers
                    .as_deref()
                    .map(tripletrail_server::otel::OtlpOptions::parse_headers)
                    .transpose()?
                    .unwrap_or_default(),
                service_name: otlp_service_name,
                sample: if (0.0..=1.0).contains(&otlp_sample) {
                    otlp_sample
                } else {
                    return Err("--otlp-sample takes a ratio from 0 to 1".into());
                },
            },
            max_rows,
            auth_config,
            limits,
            tls_cert,
            tls_key,
            #[cfg(feature = "remote-service")]
            remote_services: tripletrail::remote::RemoteConfig {
                allow: service_allow,
                timeout: Duration::try_from_secs_f64(service_timeout)
                    .map_err(|_| "--service-timeout takes a positive number of seconds")?,
                max_bytes: service_max_bytes,
                cache_ttl: Duration::try_from_secs_f64(service_cache_seconds)
                    .map_err(|_| "--service-cache-seconds takes a number of seconds")?,
            },
            #[cfg(feature = "postgres")]
            database_sources,
            workspaces: tripletrail_server::WorkspaceOptions {
                enabled: !no_workspaces,
                dir: workspace_dir,
                ttl: Duration::try_from_secs_f64(workspace_ttl_hours * 3600.0)
                    .map_err(|_| "--workspace-ttl-hours takes a positive number of hours")?,
                max_workspaces: workspace_max,
                max_per_owner: workspace_max_per_caller,
                max_triples: workspace_max_triples,
                max_file: max_body,
                max_bytes: workspace_max_mb.saturating_mul(1 << 20),
                max_overlay_changes: workspace_max_overlay_changes,
                ..Default::default()
            },
            #[cfg(feature = "enterprise")]
            license: tripletrail_enterprise::license::read(
                license.as_deref(),
                license_file.as_deref(),
            )
            .and_then(|text| tripletrail_enterprise::license::verify(&text)),
            #[cfg(feature = "enterprise")]
            audit_log,
            #[cfg(feature = "enterprise")]
            audit_reads,
            #[cfg(feature = "enterprise")]
            audit_syslog,
        },
    )?;
    #[cfg(feature = "enterprise")]
    watch_license(&server);
    let handle = server.handle();
    handle.set_starting(true);
    eprintln!(
        "listening on {}://{}/ (opening datasets; /$/ready says when they are open)",
        if https { "https" } else { "http" },
        server.address()
    );
    if !https && server.has_password_users() && !server.address().ip().is_loopback() {
        eprintln!(
            "warning: users sign in with passwords over plain HTTP: put the server behind \
             HTTPS (--tls-cert, or a proxy that ends TLS) so passwords are not sent in the clear"
        );
    }
    // Opened after binding, so liveness checks (/$/ping) answer while large datasets open.
    std::thread::spawn(move || {
        let opened = (|| -> Result<Vec<String>> {
            let mut names = Vec::new();
            let mut first = true;
            for name in &datasets {
                let engine = if read_only {
                    let dir = location.as_ref().expect("checked").join(name);
                    open_dataset(&dir, Mode::ReadOnly)?
                } else {
                    open(location.as_deref(), name, store)?
                };
                engine.set_indexed_predicates(indexed.clone());
                if first {
                    for file in &load {
                        load_file(&engine, file, None)?;
                    }
                    first = false;
                }
                engine.set_union_default_graph(union(name));
                if !read_only {
                    entail(name, &engine)?;
                }
                // The spatial index is built in the background from here on, not on the
                // first spatial query (see /$/ready).
                engine.warm_up();
                handle.attach(name, engine, location.as_ref().map(|l| l.join(name)));
                names.push(name.clone());
            }
            for (name, dir, mode) in &attached {
                let started = std::time::Instant::now();
                let engine = open_dataset(dir, *mode)?;
                engine.set_indexed_predicates(indexed.clone());
                engine.set_union_default_graph(union(name));
                if *mode == Mode::ReadWrite {
                    entail(name, &engine)?;
                }
                engine.warm_up();
                handle.attach(name, engine, Some(dir.clone()));
                eprintln!(
                    "attached {name} ({}, {}) in {:.1} s",
                    dir.display(),
                    if *mode == Mode::ReadOnly {
                        "read-only"
                    } else {
                        "read-write"
                    },
                    started.elapsed().as_secs_f64()
                );
                names.push(name.clone());
            }
            for (name, dir) in &packs {
                let done = handle
                    .install_pack(name, dir)
                    .map_err(|e| format!("--pack {}: {e}", dir.display()))?;
                eprintln!("{name}: {done}");
            }
            #[cfg(feature = "replication")]
            for f in &follows {
                let dir = location.as_ref().expect("checked");
                // Opens or copies the dataset on a thread of its own, retrying while the
                // leader cannot be reached; /$/ready answers 503 until it is served.
                tripletrail_server::follow(&handle, dir, f.clone())
                    .map_err(|e| format!("--follow {}: {e}", f.name))?;
                eprintln!("{}: opening or copying from {}", f.name, f.url());
            }
            #[cfg(feature = "replication")]
            if let Some(dir) = &location {
                let mode = if read_only {
                    Mode::ReadOnly
                } else {
                    Mode::ReadWrite
                };
                for name in tripletrail_server::promoted_replicas(dir) {
                    if follows.iter().any(|f| f.name == name) {
                        continue;
                    }
                    tripletrail_server::serve_promoted(&handle, dir, &name, mode)
                        .map_err(|e| format!("{name}: {e}"))?;
                    names.push(name);
                }
            }
            #[cfg(feature = "replication")]
            let followed = |n: &str| follows.iter().any(|f| f.name == n);
            #[cfg(not(feature = "replication"))]
            let followed = |_: &str| false;
            for (n, p) in &text_by_dataset {
                if n != "*" && !names.contains(n) && !followed(n) {
                    eprintln!(
                        "--text-properties {n}={}: no dataset {n} is served; it takes them if one of that name is created later",
                        p.as_str()
                    );
                }
            }
            for (alias, target) in &alias_pairs {
                if !names.contains(target) {
                    return Err(format!("--alias {alias}={target}: no dataset {target}").into());
                }
                handle.alias(alias, target);
            }
            Ok(names)
        })();
        match opened {
            Ok(names) => {
                handle.set_starting(false);
                eprintln!("serving datasets: {}", names.join(", "));
                // The map of each, one after the other, as the first visitor would wait for
                // it (#92): the profile and the facet queries, kept in the caches.
                if !no_warm_up {
                    let (handle, names) = (handle.clone(), names.clone());
                    std::thread::spawn(move || {
                        let mut watches = Vec::with_capacity(names.len());
                        for name in &names {
                            if let Some(done) = handle.warm_ranks(name) {
                                eprintln!("{name}: {done}");
                            }
                            // Read before the warm-up: a write during it is warmed after.
                            watches.push(handle.map_watch(name));
                            match handle.warm_map(name) {
                                Ok(done) => eprintln!("{name}: {done}"),
                                Err(e) => eprintln!("{name}: the map was not made ahead: {e}"),
                            }
                        }
                        // And again after each load or change of the shapes, once the dataset
                        // keeps still (#258): a write leaves the counts to the next visitor.
                        loop {
                            std::thread::sleep(tripletrail_server::MAP_WATCH);
                            for (name, watch) in names.iter().zip(watches.iter_mut()) {
                                match handle.rewarm_map(name, watch) {
                                    Some(Ok(done)) => eprintln!("{name}: after a change, {done}"),
                                    Some(Err(e)) => {
                                        eprintln!("{name}: the map was not made again: {e}")
                                    }
                                    None => {}
                                }
                            }
                        }
                    });
                }
                // Shapes and graphs a data directory from before the rename still holds (#955).
                for name in &names {
                    for warning in handle.old_names(name) {
                        eprintln!("warning: {name}: {warning}");
                    }
                }
            }
            Err(e) => {
                eprintln!("could not open the datasets: {e}");
                std::process::exit(1);
            }
        }
    });
    server.run_until_signal();
    Ok(())
}

fn main() -> Result<()> {
    // Queries are parsed, planned and evaluated recursively, and a thread's default stack
    // (2 MB) holds about 1,000 levels; a query deep enough aborts the process (#483). Every
    // thread that does not set a stack size of its own, and this command's main work, gets
    // `QUERY_STACK` of address space, of which only what is used is touched.
    if std::env::var_os("RUST_MIN_STACK").is_none() {
        // SAFETY: no other thread runs yet, so none reads the environment meanwhile.
        unsafe {
            std::env::set_var(
                "RUST_MIN_STACK",
                tripletrail::complexity::QUERY_STACK.to_string(),
            )
        };
    }
    // An empty TRIPLETRAIL_LOOKUP_JOINS (a compose file passing an unset variable) means not
    // set, so the default; the other commands (infer, lift, validate, ...) read it too.
    if std::env::var_os("TRIPLETRAIL_LOOKUP_JOINS").is_some_and(|v| v.is_empty()) {
        // SAFETY: no other thread runs yet, so none reads the environment meanwhile.
        unsafe { std::env::remove_var("TRIPLETRAIL_LOOKUP_JOINS") };
    }
    if let Some(on) = lookup_joins_from_env() {
        tripletrail::set_lookup_joins(on);
    }
    std::thread::Builder::new()
        .name("main".into())
        .stack_size(tripletrail::complexity::QUERY_STACK)
        .spawn(run)?
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// `TRIPLETRAIL_LOOKUP_JOINS` read as `serve --lookup-joins` reads it (true/false, 1/0, yes/no,
/// on/off): `None` when it is not set or is not one of these (`serve` then says so).
fn lookup_joins_from_env() -> Option<bool> {
    let value = std::env::var_os("TRIPLETRAIL_LOOKUP_JOINS")?;
    let value = value.to_str()?;
    clap::builder::TypedValueParser::parse_ref(
        &clap::builder::BoolishValueParser::new(),
        &clap::Command::new("tripletrail"),
        None,
        std::ffi::OsStr::new(value),
    )
    .ok()
}

/// Logs the licence's standing at start, then once a day while it needs attention (#734).
#[cfg(feature = "enterprise")]
fn watch_license(server: &Server) {
    let options = server.options().clone();
    // Row-level policies stay enforced without a licence that covers them (#818).
    if let Some(warning) = server.row_policies_warning() {
        eprintln!("warning: {warning}");
    }
    let report = move || {
        let status = options.license_status();
        match (status.warning(), &status.license) {
            (Some(w), _) => eprintln!("warning: {w}"),
            (None, Some(l)) => eprintln!(
                "TripleTrail Enterprise licence {} for {}, valid until {}",
                l.id, l.customer, l.expires
            ),
            (None, None) => {}
        }
        if let Some(l) = &status.license
            && let Some(max) = l.max_cores
        {
            let cores = std::thread::available_parallelism().map_or(1, |n| n.get() as u64);
            if cores > max {
                eprintln!(
                    "warning: this server has {cores} CPU cores; the TripleTrail Enterprise licence covers {max}"
                );
            }
        }
        status.warning().is_some()
    };
    if report() {
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(24 * 3600));
                report();
            }
        });
    }
}

#[cfg(feature = "enterprise")]
fn license_command(command: LicenseCommand) -> Result<()> {
    use tripletrail_enterprise::license::{self, Date, Standing, Status};
    let LicenseCommand::Inspect { file, json } = command;
    let text = match file.as_deref() {
        Some(path) if path == Path::new("-") => {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
            Ok(text)
        }
        Some(path) => license::read(None, Some(path)),
        None => license::read(
            std::env::var("TRIPLETRAIL_LICENSE").ok().as_deref(),
            std::env::var_os("TRIPLETRAIL_LICENSE_FILE")
                .map(PathBuf::from)
                .as_deref(),
        ),
    };
    let verified = text
        .as_deref()
        .map_err(Clone::clone)
        .and_then(license::verify);
    let status = Status::of(&verified, Date::today());
    if json {
        println!("{}", serde_json::to_string_pretty(&status.to_json())?);
    } else {
        // What an invalid licence claims, too: it helps support find the problem.
        let shown = status.license.clone().or_else(|| {
            text.ok()
                .and_then(|t| license::decode(&t).ok())
                .map(|d| d.0)
        });
        if let Some(l) = &shown {
            println!("Licence:   {}", l.id);
            println!("Customer:  {}", l.customer);
            println!("Issued:    {}", l.issued);
            println!("Expires:   {}", l.expires);
            let features: Vec<&str> = l.features.iter().map(String::as_str).collect();
            println!("Features:  {}", features.join(", "));
            if let Some(n) = l.max_nodes {
                println!("Max nodes: {n}");
            }
            if let Some(n) = l.max_cores {
                println!("Max cores: {n}");
            }
            if let Some(note) = &l.note {
                println!("Note:      {note}");
            }
            println!("Key:       {}", l.kid);
        }
        let verdict = match &status.standing {
            Standing::Valid => "valid".to_owned(),
            Standing::Expiring { days_left } => format!("valid, expiring in {days_left} days"),
            Standing::Grace { days_left } => {
                format!("expired, in its grace period ({days_left} days left)")
            }
            Standing::Lapsed => "expired: the enterprise features are off".to_owned(),
            Standing::Invalid(e) => format!("refused: {e}"),
        };
        println!("Status:    {verdict}");
    }
    if !status.active() {
        std::process::exit(1);
    }
    Ok(())
}

/// The command line, with the options of `serve --config FILE` added (#826).
fn command_line() -> Result<Vec<std::ffi::OsString>> {
    use clap::CommandFactory;
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let Some(path) = tripletrail_server::config::file_of(&argv) else {
        return Ok(argv);
    };
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("--config {}: {e}", path.display()))?;
    let cli = Cli::command();
    let serve = cli.find_subcommand("serve").expect("the serve command");
    let options: Vec<tripletrail_server::config::Opt> = serve
        .get_arguments()
        .filter_map(|a| {
            Some(tripletrail_server::config::Opt {
                long: a.get_long()?.to_owned(),
                short: a.get_short(),
                takes_value: a.get_action().takes_values(),
                env: a.get_env().map(|e| e.to_string_lossy().into_owned()),
            })
        })
        .collect();
    Ok(tripletrail_server::config::expand(&argv, &text, &options)
        .map_err(|e| format!("--config {}: {e}", path.display()))?)
}

fn run() -> Result<()> {
    match Cli::parse_from(command_line()?).command {
        #[cfg(feature = "enterprise")]
        Command::License { command } => license_command(command),
        #[cfg(feature = "enterprise")]
        Command::Audit {
            command: AuditCommand::Verify { file },
        } => {
            let report = tripletrail_enterprise::audit::verify(&file)?;
            for (line, problem) in &report.problems {
                println!("line {line}: {problem}");
            }
            println!(
                "{} lines, {} starts{}; last hash {}",
                report.lines,
                report.starts,
                if report.cut > 0 {
                    format!(", {} lines cut off by a crash", report.cut)
                } else {
                    String::new()
                },
                report.last_hash
            );
            if report.problems.is_empty() {
                println!("The chain is whole.");
                Ok(())
            } else {
                std::process::exit(1)
            }
        }
        #[cfg(feature = "enterprise")]
        Command::Audit {
            command:
                AuditCommand::Report {
                    file,
                    since,
                    until,
                    agent,
                    dataset,
                    json,
                },
        } => {
            let report = tripletrail_enterprise::audit::report(
                &file,
                &tripletrail_enterprise::audit::Filter {
                    since,
                    until,
                    agent,
                    dataset,
                },
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", audit_report_text(&report));
            }
            if report["chainWhole"] == true {
                Ok(())
            } else {
                // For a script that looks at the exit code only.
                std::process::exit(1)
            }
        }
        Command::Serve(args) => serve(*args),
        Command::HashPassword { password } => {
            let password = match password {
                Some(p) => p,
                None => {
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    line.trim_end_matches(['\r', '\n']).to_owned()
                }
            };
            if password.is_empty() {
                return Err("No password given".into());
            }
            println!("{}", tripletrail_server::password::hash(&password)?);
            Ok(())
        }
        Command::ImportShiro { file } => {
            let text =
                std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
            let users = tripletrail_server::password::import_shiro(&text)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({ "auth": { "users": users } }))?
            );
            Ok(())
        }
        Command::ImportJenaRules { file } => {
            let text =
                std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
            let t = tripletrail_server::jena_rules::translate(&text)
                .map_err(|e| format!("{}: {e}", file.display()))?;
            print!("{}", t.turtle);
            eprintln!(
                "{} rule{} translated, {} left out",
                t.translated,
                if t.translated == 1 { "" } else { "s" },
                t.skipped.len()
            );
            for (name, why) in &t.skipped {
                eprintln!("  {name}: {why}");
            }
            for i in &t.includes {
                eprintln!("not read: @include {i}");
            }
            Ok(())
        }
        Command::Config {
            command: ConfigCommand::ImportFuseki { file, output },
        } => {
            let text =
                std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
            let got = tripletrail_server::fuseki_config::import(&text)
                .map_err(|e| format!("{}: {e}", file.display()))?;
            match output {
                Some(path) => std::fs::write(&path, &got.yaml)?,
                None => print!("{}", got.yaml),
            }
            if !got.unmapped.is_empty() {
                eprintln!(
                    "{} setting(s) not carried over, listed at the end of the file",
                    got.unmapped.len()
                );
            }
            Ok(())
        }
        Command::HashKey { key } => {
            let key = match key {
                Some(k) => k,
                None => {
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    line
                }
            };
            let key = key.trim();
            if key.is_empty() {
                return Err("No key given".into());
            }
            println!(
                "{}",
                tripletrail_server::auth::to_hex(&tripletrail_server::auth::sha256(key.as_bytes()))
            );
            Ok(())
        }
        Command::Health { url, tls_cert } => {
            let ok = health(&url, tls_cert.is_some()).unwrap_or_else(|e| {
                eprintln!("{e}");
                false
            });
            std::process::exit(if ok { 0 } else { 1 });
        }
        Command::Load {
            location,
            store,
            dataset,
            graph,
            parquet,
            memory_limit,
            render_wkt,
            keep_wkt_text,
            files,
        } => {
            if let Some(bytes) = memory_limit {
                tripletrail::store::Store::set_memory_limit(bytes);
            }
            // A new dataset keeps WKT in binary form unless --keep-wkt-text (#152); an
            // existing one keeps its setting unless a flag changes it.
            let new = !location.join(&dataset).join("FORMAT").exists();
            let engine = open(Some(&location), &dataset, store)?;
            let set = if keep_wkt_text {
                Some(false)
            } else if render_wkt || new {
                Some(true)
            } else {
                None
            };
            match (engine.store(), set) {
                (tripletrail::store::Dataset::TripleTrail(s), Some(on)) => s.set_render_wkt(on)?,
                (_, None) => {}
                (_, Some(_)) if render_wkt || keep_wkt_text => {
                    return Err("--render-wkt and --keep-wkt-text need TripleTrail storage".into());
                }
                // Another storage keeps WKT as it does.
                (_, Some(_)) => {}
            }
            for file in &files {
                load_file_with(&engine, file, graph.as_deref(), &parquet)?;
            }
            Ok(())
        }
        Command::Entail {
            data,
            location,
            dataset,
            into,
            output,
            regime,
        } => entail(
            &data,
            location.as_deref(),
            &dataset,
            &into,
            output.as_deref(),
            match regime {
                EntailRegime::Rdfs => tripletrail::store::entail::Regime::Rdfs,
                EntailRegime::OwlRl => tripletrail::store::entail::Regime::OwlRl,
            },
        ),
        Command::Infer {
            data,
            shapes,
            location,
            store,
            dataset,
            shapes_graph,
            data_graph,
            into,
            max_passes,
            all_constructed,
            naive,
            ordered,
            output,
            verbose,
            provenance,
        } => {
            match &location {
                None if shapes.is_empty() => {
                    return Err("give --shapes files, or --location with --shapes-graph".into());
                }
                None if into.is_some() => {
                    return Err("--into writes to a dataset on disk: give --location".into());
                }
                Some(_) if !data.is_empty() || !shapes.is_empty() => {
                    return Err(
                        "--data and --shapes files are for running without --location; \
                         with --location, give --shapes-graph (and --data-graph)"
                            .into(),
                    );
                }
                Some(_) if shapes_graph.is_none() => {
                    return Err("give --shapes-graph with --location".into());
                }
                _ => {}
            }
            infer(
                &data,
                &shapes,
                location.as_deref(),
                store,
                &dataset,
                shapes_graph.as_deref(),
                data_graph.as_deref(),
                into.as_deref(),
                max_passes,
                all_constructed,
                naive,
                ordered,
                output.as_deref(),
                verbose,
                provenance.as_deref(),
            )
        }
        Command::Lift {
            queries,
            base,
            allow,
            data,
            output,
            output_dir,
            jobs,
            values,
            explain,
            as_sparql,
            overwrite_targets,
            urls,
            #[cfg(feature = "postgres")]
            database,
            #[cfg(feature = "postgres")]
            connections,
        } => {
            if as_sparql {
                let n = lift::as_sparql(&queries, output.as_deref(), output_dir.as_deref())?;
                if output.is_some() || output_dir.is_some() {
                    eprintln!("{n} mappings written as SPARQL");
                }
                return Ok(());
            }
            let engine = Engine::new_tripletrail();
            for file in &data {
                load_file(&engine, file, None)?;
            }
            let report = lift::lift(
                &engine,
                lift::LiftArgs {
                    queries,
                    base,
                    allow,
                    output,
                    output_dir,
                    jobs,
                    values: lift::parse_values(&values)?,
                    explain,
                    quiet: false,
                    overwrite_targets,
                    remote: urls.remote()?,
                    #[cfg(feature = "postgres")]
                    database,
                    #[cfg(feature = "postgres")]
                    connections: connections
                        .iter()
                        .map(|c| match c.split_once('=') {
                            Some((name, url)) if !name.trim().is_empty() => {
                                Ok((name.trim().to_owned(), url.to_owned()))
                            }
                            _ => Err(format!("--connection {c:?}: give NAME=URL")),
                        })
                        .collect::<std::result::Result<_, _>>()?,
                },
            )?;
            match report.failed() {
                0 => Ok(()),
                n => Err(format!("{n} of {} queries failed", report.files.len()).into()),
            }
        }
        #[cfg(feature = "postgres")]
        Command::DirectMapping {
            database,
            base_iri,
            schema,
            tables,
            mapping,
            output,
        } => {
            let text = lift::direct_mapping(&database, &base_iri, &schema, &tables)?;
            if mapping {
                match &output {
                    Some(path) => std::fs::write(path, text)?,
                    None => std::io::Write::write_all(&mut std::io::stdout(), text.as_bytes())?,
                }
                return Ok(());
            }
            // Run it as any R2RML mapping, from a folder of its own.
            let dir =
                std::env::temp_dir().join(format!("tripletrail-direct-{}", std::process::id()));
            std::fs::create_dir_all(&dir)?;
            let file = dir.join("direct.r2rml.ttl");
            std::fs::write(&file, text)?;
            let report = lift::lift(
                &Engine::new_tripletrail(),
                lift::LiftArgs {
                    queries: vec![file],
                    base: Some(dir.clone()),
                    allow: vec![dir.clone()],
                    output,
                    output_dir: None,
                    jobs: 1,
                    values: std::collections::HashMap::new(),
                    explain: false,
                    quiet: true,
                    overwrite_targets: false,
                    remote: Default::default(),
                    database: Some(database),
                    connections: std::collections::HashMap::new(),
                },
            );
            let _ = std::fs::remove_dir_all(&dir);
            let report = report?;
            match report.files.iter().find_map(|f| f.error.as_ref()) {
                None => Ok(()),
                Some(error) => Err(error.clone().into()),
            }
        }
        #[cfg(feature = "geoparquet")]
        Command::Export {
            location,
            store,
            dataset,
            data,
            query,
            row_group_size,
            projjson,
            output,
        } => {
            let engine = open(location.as_deref(), &dataset, store)?;
            for file in &data {
                load_file(&engine, file, None)?;
            }
            export(
                &engine,
                &query,
                &output,
                row_group_size,
                projjson.as_deref(),
            )
        }
        Command::Test {
            dir,
            junit,
            timeout,
            record,
        } => {
            use tripletrail_server::testing;
            let tests = testing::discover(&dir);
            if tests.is_empty() {
                return Err(format!(
                    "no tests in {}: a test is a folder with an expected.* file or a test.yaml",
                    dir.display()
                )
                .into());
            }
            let base = if dir.join("tests").is_dir() {
                dir.join("tests")
            } else {
                dir.clone()
            };
            let limits = testing::Limits {
                root: None,
                timeout: Some(Duration::from_secs(timeout)),
                record,
                max_bytes: None,
                max_input: None,
                max_rows: None,
            };
            let mut outcomes = Vec::new();
            for test in &tests {
                let outcome = testing::run(test, &base, limits);
                if record && outcome.passed {
                    println!("RECORDED {}", outcome.name);
                    outcomes.push(outcome);
                    continue;
                }
                print!("{}", outcome.report());
                outcomes.push(outcome);
            }
            let failed = outcomes.iter().filter(|o| !o.passed).count();
            println!("{} passed, {failed} failed", outcomes.len() - failed);
            if let Some(path) = junit {
                std::fs::write(&path, testing::junit(&outcomes))?;
            }
            if failed > 0 {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Pipeline {
            command:
                PipelineCommand::Run {
                    file,
                    location,
                    store,
                    dataset,
                    values,
                    from,
                    to,
                    allow,
                    report,
                    python,
                    urls,
                },
        } => {
            use tripletrail_server::pipeline;
            let pipeline = pipeline::Pipeline::read(&file)?;
            let engine = open(location.as_deref(), &dataset, store)?;
            let options = pipeline::Options {
                root: None,
                values: lift::parse_values(&values)?,
                from: from
                    .map(|s| pipeline.step_index(&s))
                    .transpose()?
                    .unwrap_or(0),
                to: to
                    .map(|s| pipeline.step_index(&s).map(|i| i + 1))
                    .transpose()?,
                allow,
                timeout: None,
                max_quads: None,
                max_output: None,
                // Lift steps spool their triples beside the dataset, not in /tmp.
                scratch: location.clone(),
                python: Some(python),
                store: location.as_ref().map(|l| l.join(&dataset)),
                remote: urls.remote()?,
            };
            let total = pipeline.steps.len();
            let result = pipeline::run(&pipeline, &engine, &options, &|i, done| match done {
                None => eprintln!(
                    "step {}/{total} {} ({})",
                    i + 1,
                    pipeline.steps[i].name,
                    pipeline.steps[i].kind.name()
                ),
                Some(r) => eprintln!(
                    "step {}/{total} {}: {} in {:.1} s, {} quads{}",
                    i + 1,
                    r.name,
                    r.status.label(),
                    r.seconds,
                    r.quads_after,
                    r.error
                        .as_ref()
                        .map(|e| format!(": {e}"))
                        .unwrap_or_default()
                ),
            });
            let json = serde_json::to_string_pretty(&result.to_json())?;
            match report {
                Some(path) => std::fs::write(path, json + "\n")?,
                None => println!("{json}"),
            }
            if !result.ok() {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Pipeline {
            command:
                PipelineCommand::Export {
                    file,
                    to,
                    one_task,
                    location,
                    dataset,
                    values,
                    output,
                },
        } => {
            use tripletrail_server::{pipeline, pipeline_export};
            let pipeline = pipeline::Pipeline::read(&file)?;
            let values = lift::parse_values(&values)?;
            let file = file.display().to_string();
            let target = pipeline_export::Target {
                file: &file,
                location: location.as_deref(),
                dataset: &dataset,
                values: &values,
            };
            let code = match to {
                ExportKind::Airflow => pipeline_export::airflow(&pipeline, &target, !one_task),
                ExportKind::Python => pipeline_export::python(&pipeline, &target),
            };
            match output {
                Some(path) => std::fs::write(path, code)?,
                None => print!("{code}"),
            }
            Ok(())
        }
        Command::Shapes {
            command:
                ShapesCommand::Check {
                    pack,
                    installed,
                    own,
                    data,
                    location,
                    store,
                    dataset,
                    strict,
                    json,
                },
        } => {
            let pack = set_files(&pack)?;
            let installed = installed.as_deref().map(set_files).transpose()?;
            let own = own.as_deref().map(set_files).transpose()?;
            if location.is_some() && !data.is_empty() {
                return Err("give --data files or --location, not both".into());
            }
            let engine = (location.is_some() || !data.is_empty())
                .then(|| -> Result<Engine> {
                    let engine = open(location.as_deref(), &dataset, store)?;
                    for file in &data {
                        load_file(&engine, file, None)?;
                    }
                    Ok(engine)
                })
                .transpose()?;
            let has = |class: &NamedNode| {
                let ask =
                    format!("ASK {{ {{ ?s a {class} }} UNION {{ GRAPH ?g {{ ?s a {class} }} }} }}");
                let engine = engine.as_ref().expect("with data");
                !matches!(
                    engine.query(&ask),
                    Ok(tripletrail::oxigraph::sparql::QueryResults::Boolean(false))
                )
            };
            let report = tripletrail_server::pack_check(
                installed.as_deref(),
                &pack,
                own.as_deref(),
                engine
                    .is_some()
                    .then_some(&has as &dyn Fn(&NamedNode) -> bool),
            );
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", pack_check_text(&report));
            }
            let orphaned = report["orphaned"].as_array().is_some_and(|o| !o.is_empty());
            if report["valid"] != true || (strict && orphaned) {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Shapes {
            command:
                ShapesCommand::ImportAtlas {
                    folder,
                    out,
                    hidden_from,
                    prefix,
                },
        } => {
            let readers = hidden_from
                .iter()
                .map(|name| {
                    tripletrail::shapes::Reader::from_name(name)
                        .ok_or_else(|| format!("--hidden-from {name}: no such reader"))
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let mut shapes = tripletrail::oxigraph::model::Graph::new();
            // RINF Atlas keeps its views in `views/`.
            let views = folder.join("views");
            let mut files = set_files(&folder)?;
            let has_ttl = |dir: &std::path::Path| {
                std::fs::read_dir(dir).is_ok_and(|mut entries| {
                    entries
                        .any(|e| e.is_ok_and(|e| e.path().extension().is_some_and(|x| x == "ttl")))
                })
            };
            if has_ttl(&views) {
                files.extend(set_files(&views)?);
            }
            for file in files {
                let parser = tripletrail::oxigraph::io::RdfParser::from_format(
                    tripletrail::oxigraph::io::RdfFormat::Turtle,
                )
                .rename_blank_nodes();
                for quad in parser.for_slice(file.text.as_bytes()) {
                    let quad = quad.map_err(|e| format!("{}: {e}", file.name))?;
                    shapes.insert(&tripletrail::oxigraph::model::Triple::from(quad));
                }
            }
            let (set, notes) = tripletrail::shapes::from_atlas(&shapes, &readers);
            let errors = tripletrail::shapes::check(&set);
            if !errors.is_empty() {
                return Err(errors.join("\n").into());
            }
            std::fs::create_dir_all(&out)?;
            let files = tripletrail::shapes::files_of(&set);
            for file in &files {
                let path = out.join(format!("{prefix}{}", file.name));
                std::fs::write(&path, &file.text)?;
                eprintln!("wrote {}", path.display());
            }
            for note in notes {
                eprintln!("{note}");
            }
            Ok(())
        }
        Command::Validate {
            data,
            shapes,
            lookup,
            lookup_graph,
            location,
            store,
            dataset,
            shapes_graph,
            data_graph,
            report,
            stats,
        } => {
            match &location {
                None if shapes.is_empty() => {
                    return Err("give --shapes files, or --location with --shapes-graph".into());
                }
                None if !lookup_graph.is_empty() => {
                    return Err("--lookup-graph is for --location; give --lookup files".into());
                }
                // Files would be loaded into the dataset on disk.
                Some(_) if !data.is_empty() || !shapes.is_empty() || !lookup.is_empty() => {
                    return Err(
                        "--data, --shapes and --lookup files are for validating without \
                         --location; with --location, give --shapes-graph (and --data-graph, \
                         --lookup-graph)"
                            .into(),
                    );
                }
                Some(_) if shapes_graph.is_none() => {
                    return Err("give --shapes-graph with --location".into());
                }
                _ => {}
            }
            let conforms = validate(
                &data,
                &shapes,
                &lookup,
                &lookup_graph,
                location.as_deref(),
                store,
                &dataset,
                shapes_graph.as_deref(),
                data_graph.as_deref(),
                report.as_deref(),
                stats,
            )?;
            if !conforms {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Query {
            source,
            query,
            format,
            output,
        } => offline::query(
            &source.source(),
            &query,
            format.as_deref(),
            output.as_deref(),
        ),
        Command::Update {
            source,
            update,
            format,
            output,
        } => offline::update(
            &source.source(),
            &update,
            format.as_deref(),
            output.as_deref(),
        ),
        Command::Dump {
            location,
            store,
            dataset,
            graph,
            format,
            output,
        } => offline::dump(
            &location,
            &dataset,
            store,
            graph.as_deref(),
            format.as_deref(),
            output.as_deref(),
        ),
        Command::DiskUsage {
            location,
            dataset,
            json,
        } => offline::disk_usage(&location, &dataset, json),
        Command::Convert {
            inputs,
            syntax,
            to,
            output,
            validate,
            count,
            base,
        } => offline::convert(&offline::Convert {
            inputs: &inputs,
            syntax: syntax.as_deref(),
            to: to.as_deref(),
            output: output.as_deref(),
            validate,
            count,
            base: base.as_deref(),
        }),
        Command::Canon {
            inputs,
            store,
            output,
            digest,
        } => {
            let canonical = read_canonical(&inputs, store)?;
            if digest {
                println!("sha256:{}", canonical.digest());
                return Ok(());
            }
            let mut out: Box<dyn std::io::Write> = match &output {
                Some(path) => Box::new(std::io::BufWriter::new(std::fs::File::create(path)?)),
                None => Box::new(std::io::BufWriter::new(std::io::stdout().lock())),
            };
            for line in &canonical.lines {
                out.write_all(line.as_bytes())?;
            }
            out.flush()?;
            eprintln!(
                "sha256:{} ({} quads)",
                canonical.digest(),
                canonical.lines.len()
            );
            Ok(())
        }
        Command::Diff {
            a,
            b,
            store,
            format,
            added,
            removed,
        } => {
            let a = read_canonical(std::slice::from_ref(&a), store)?;
            let b = read_canonical(std::slice::from_ref(&b), store)?;
            let diff = tripletrail::canon::diff(&a, &b);
            for (path, lines) in [(&added, &diff.added), (&removed, &diff.removed)] {
                if let Some(path) = path {
                    std::fs::write(path, lines.concat())?;
                }
            }
            use std::io::Write as _;
            let mut out = std::io::BufWriter::new(std::io::stdout().lock());
            match format {
                DiffFormat::Json => {
                    let summary = diff.summary(&a, &b);
                    let counts = |m: &std::collections::BTreeMap<
                        String,
                        tripletrail::canon::Counts,
                    >| {
                        m.iter()
                            .map(|(k, c)| {
                                serde_json::json!({ "iri": k, "added": c.added, "removed": c.removed })
                            })
                            .collect::<Vec<_>>()
                    };
                    let json = serde_json::json!({
                        "a": { "sha256": a.digest(), "quads": a.lines.len() },
                        "b": { "sha256": b.digest(), "quads": b.lines.len() },
                        "added": summary.total.added,
                        "removed": summary.total.removed,
                        "graphs": counts(&summary.graphs),
                        "predicates": counts(&summary.predicates),
                        "classes": counts(&summary.classes),
                    });
                    serde_json::to_writer_pretty(&mut out, &json)?;
                    writeln!(out)?;
                }
                DiffFormat::RdfPatch => {
                    for (op, lines) in [("D", &diff.removed), ("A", &diff.added)] {
                        for line in lines {
                            write!(out, "{op} {line}")?;
                        }
                    }
                }
            }
            out.flush()?;
            drop(out);
            if !diff.added.is_empty() || !diff.removed.is_empty() {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

/// The `--map-basemap` values; empty lines are skipped. The attribution is the rest of the
/// value after the URL, so it may hold a `|` itself; a last field of one or two digits is the
/// maxzoom (a longer number, such as a year, stays part of the credit).
fn read_basemaps(values: &[String]) -> Result<Vec<tripletrail_server::Basemap>> {
    let mut basemaps = Vec::new();
    for value in values.iter().map(|v| v.trim()).filter(|v| !v.is_empty()) {
        let mut parts = value.splitn(3, '|').map(str::trim);
        let (title, url, mut attribution) = (
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
        );
        if title.is_empty() || !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(format!(
                "--map-basemap takes TITLE|URL|ATTRIBUTION[|MAXZOOM] with an http(s) tile URL, \
                 not {value:?}"
            )
            .into());
        }
        let mut maxzoom = tripletrail_server::Basemap::DEFAULT_MAXZOOM;
        if let Some((credit, last)) = attribution.rsplit_once('|')
            && let last = last.trim()
            && (1..=2).contains(&last.len())
            && last.bytes().all(|b| b.is_ascii_digit())
        {
            maxzoom = match last.parse() {
                Ok(z @ 0..=24) => z,
                _ => {
                    return Err(format!(
                        "--map-basemap: MAXZOOM is a zoom from 0 to 24, not {last:?} in {value:?}"
                    )
                    .into());
                }
            };
            attribution = credit.trim();
        }
        basemaps.push(tripletrail_server::Basemap {
            title: title.to_owned(),
            tiles: url.to_owned(),
            attribution: attribution.to_owned(),
            maxzoom,
        });
    }
    Ok(basemaps)
}

/// The `sh:SPARQLFunction`s of RDF `files`.
fn read_functions(files: &[PathBuf]) -> Result<tripletrail::udf::Functions> {
    let mut triples = Vec::new();
    for file in files {
        let format = RdfFormat::from_extension(
            file.extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default(),
        )
        .ok_or_else(|| format!("{}: unknown RDF format", file.display()))?;
        let reader = std::io::BufReader::new(std::fs::File::open(file)?);
        for quad in RdfParser::from_format(format).for_reader(reader) {
            let q = quad.map_err(|e| format!("{}: {e}", file.display()))?;
            triples.push(tripletrail::oxigraph::model::Triple::new(
                q.subject,
                q.predicate,
                q.object,
            ));
        }
    }
    Ok(tripletrail::udf::Functions::from_triples(triples).map_err(|e| e.to_string())?)
}

/// The quads of `inputs` (files, or directories of on-disk datasets), canonicalized.
fn read_canonical(inputs: &[PathBuf], store: StoreKind) -> Result<tripletrail::canon::Canonical> {
    let mut quads = Vec::new();
    for input in inputs {
        let engine = if input.is_dir() {
            let (Some(location), Some(name)) = (input.parent(), input.file_name()) else {
                return Err(format!("{}: not a dataset directory", input.display()).into());
            };
            open(Some(location), &name.to_string_lossy(), store)?
        } else {
            let engine = open(None, "ds", StoreKind::TripleTrail)?;
            load_file(&engine, input, None)?;
            engine
        };
        for quad in engine.store().iter() {
            quads.push(quad?);
        }
    }
    tripletrail::canon::canonicalize(quads).map_err(|e| e.to_string().into())
}

/// GET /$/ready over HTTP/1.0, without a client library: over TLS for an `https://` URL or
/// when `tls` is set (the server serves HTTPS).
fn health(url: &str, tls: bool) -> std::result::Result<bool, String> {
    use std::io::{Read, Write};
    let (host, tls) = match url.strip_prefix("https://") {
        Some(host) => (host, true),
        None => (
            url.strip_prefix("http://")
                .ok_or("only http:// and https:// URLs")?,
            tls,
        ),
    };
    let host = host.trim_end_matches('/');
    let addr = if host.contains(':') {
        host.to_owned()
    } else {
        format!("{host}:{}", if tls { 443 } else { 80 })
    };
    let addr = addr.replace("0.0.0.0", "127.0.0.1");
    let mut stream = std::net::TcpStream::connect(&addr).map_err(|e| format!("{addr}: {e}"))?;
    let timeout = Some(std::time::Duration::from_secs(5));
    stream
        .set_read_timeout(timeout)
        .map_err(|e| e.to_string())?;
    let request = format!("GET /$/ready HTTP/1.0\r\nHost: {host}\r\n\r\n");
    let mut head = [0u8; 12];
    if tls {
        #[cfg(feature = "tls")]
        {
            let mut conn = tls_client::connect(host)?;
            let mut tls = rustls::Stream::new(&mut conn, &mut stream);
            tls.write_all(request.as_bytes())
                .map_err(|e| e.to_string())?;
            tls.read_exact(&mut head).map_err(|e| e.to_string())?;
        }
        #[cfg(not(feature = "tls"))]
        return Err("this build has no TLS support".to_owned());
    } else {
        stream
            .write_all(request.as_bytes())
            .map_err(|e| e.to_string())?;
        stream.read_exact(&mut head).map_err(|e| e.to_string())?;
    }
    if head.ends_with(b"200") {
        Ok(true)
    } else {
        Err(format!("{addr}: {}", String::from_utf8_lossy(&head)))
    }
}

/// A TLS client for `health` that accepts any certificate: it only asks a server, usually
/// this machine's, whether it is ready, and sends nothing secret.
#[cfg(feature = "tls")]
mod tls_client {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};
    use std::sync::Arc;

    #[derive(Debug)]
    struct AnyCertificate(Arc<CryptoProvider>);

    impl ServerCertVerifier for AnyCertificate {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            verify_tls12_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            verify_tls13_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    pub(super) fn connect(host: &str) -> Result<rustls::ClientConnection, String> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
            .with_no_client_auth();
        let name = host.rsplit_once(':').map_or(host, |(name, _)| name);
        let name = name.trim_start_matches('[').trim_end_matches(']');
        let name = ServerName::try_from(name.to_owned()).map_err(|e| e.to_string())?;
        rustls::ClientConnection::new(Arc::new(config), name).map_err(|e| e.to_string())
    }
}

/// The `.ttl` files of a folder, as a shape set (#148).
fn set_files(dir: &Path) -> Result<Vec<tripletrail::shapes::SetFile>> {
    Ok(tripletrail_server::folder_files(dir)?)
}

/// `tripletrail shapes check` as text.
fn pack_check_text(report: &serde_json::Value) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let list = |key: &str| report[key].as_array().cloned().unwrap_or_default();
    for e in list("errors") {
        let _ = writeln!(out, "error: {}", e.as_str().unwrap_or_default());
    }
    if report["valid"] != true {
        return out;
    }
    let iris = |out: &mut String, key: &str, title: &str| {
        let items = list(key);
        let _ = writeln!(out, "{title}: {}", items.len());
        for i in items {
            let _ = writeln!(out, "  <{}>", i.as_str().unwrap_or_default());
        }
    };
    iris(&mut out, "added", "Shapes added");
    iris(&mut out, "removed", "Shapes removed");
    iris(&mut out, "changed", "Shapes changed");
    let overrides = |out: &mut String, key: &str, title: &str| {
        let items = list(key);
        let _ = writeln!(out, "{title}: {}", items.len());
        for o in items {
            let _ = writeln!(
                out,
                "  {} on <{}> ({})",
                o["shape"].as_str().unwrap_or_default(),
                o["packShape"].as_str().unwrap_or_default(),
                o["how"].as_str().unwrap_or_default()
            );
        }
    };
    overrides(&mut out, "overrides", "Own overrides kept");
    overrides(&mut out, "orphaned", "Own overrides that would stand alone");
    match report["noData"].as_array() {
        Some(items) => {
            let _ = writeln!(out, "Shapes for classes without instances: {}", items.len());
            for i in items {
                let _ = writeln!(
                    out,
                    "  <{}> (class <{}>)",
                    i["shape"].as_str().unwrap_or_default(),
                    i["class"].as_str().unwrap_or_default()
                );
            }
        }
        None => out.push_str("Data not checked (give --data or --location)\n"),
    }
    out
}

/// `tripletrail audit report` as text.
#[cfg(feature = "enterprise")]
fn audit_report_text(report: &serde_json::Value) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let n = |v: &serde_json::Value| v.as_u64().unwrap_or(0);
    let _ = writeln!(
        out,
        "{} lines of {}, {} to {}",
        n(&report["lines"]),
        report["file"].as_str().unwrap_or(""),
        report["from"].as_str().unwrap_or("-"),
        report["to"].as_str().unwrap_or("-"),
    );
    if report["chainWhole"] != true {
        let _ = writeln!(
            out,
            "WARNING: the chain is not whole ({} problems): lines were changed, removed or \
             moved; run `tripletrail audit verify`",
            n(&report["chainProblems"])
        );
    }
    for agent in report["agents"].as_array().into_iter().flatten() {
        let list = |k: &str| {
            agent[k]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default()
        };
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "{} ({}{}{}), {} to {}",
            agent["agent"].as_str().unwrap_or(""),
            if list("auth").is_empty() {
                "-".to_owned()
            } else {
                list("auth")
            },
            if list("clients").is_empty() {
                ""
            } else {
                " from "
            },
            list("clients"),
            agent["first"].as_str().unwrap_or(""),
            agent["last"].as_str().unwrap_or(""),
        );
        for d in agent["datasets"].as_array().into_iter().flatten() {
            let mut parts = vec![format!("{} changes", n(&d["changes"]))];
            for (k, label) in [
                ("refused", "refused"),
                ("failed", "failed"),
                ("reads", "reads"),
            ] {
                if n(&d[k]) > 0 {
                    parts.push(format!("{} {label}", n(&d[k])));
                }
            }
            let _ = write!(
                out,
                "  {}: {}; +{} -{} quads",
                d["dataset"].as_str().unwrap_or(""),
                parts.join(", "),
                n(&d["added"]),
                n(&d["removed"]),
            );
            if n(&d["uncounted"]) > 0 {
                let _ = write!(out, " (and {} bulk loads, not counted)", n(&d["uncounted"]));
            }
            let _ = writeln!(out);
            if n(&d["partialGraphs"]) > 0 {
                let _ = writeln!(
                    out,
                    "    (the graphs below leave out those past the first 100 of {} changes: \
                     the totals above have them all)",
                    n(&d["partialGraphs"])
                );
            }
            for g in d["graphs"].as_array().into_iter().flatten() {
                let _ = writeln!(
                    out,
                    "    {}: +{} -{}",
                    g["graph"].as_str().unwrap_or(""),
                    n(&g["added"]),
                    n(&g["removed"]),
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        Cli, Command, grouped, load_progress_line, lookup_joins_from_env, parse_size, read_basemaps,
    };
    use clap::Parser;

    #[test]
    fn a_load_progress_line_says_the_step_the_triples_and_the_bytes_read() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(5_000_008_711), "5,000,008,711");
        let engine = tripletrail::Engine::new_tripletrail();
        let line = load_progress_line(
            &engine,
            std::path::Path::new("planet.nt"),
            232_000_000_000,
            932_000_000_000,
            std::time::Duration::from_secs(3_600),
        );
        assert_eq!(
            line,
            "loading planet.nt: idle, 0 triples, 232.0 of 932.0 GB read (25 %), 3600 s"
        );
        // A pipe: its size is not known.
        let line = load_progress_line(
            &engine,
            std::path::Path::new("/dev/stdin"),
            200_000_000,
            0,
            std::time::Duration::from_secs(60),
        );
        assert_eq!(
            line,
            "loading /dev/stdin: idle, 0 triples, 0.2 GB read, 60 s"
        );
    }

    #[test]
    fn a_basemap_may_name_its_last_zoom() {
        let one = |v: &str| {
            let b = read_basemaps(&[v.to_owned()]).unwrap().remove(0);
            (b.title, b.tiles, b.attribution, b.maxzoom)
        };
        let url = "https://tiles.example.org/{z}/{x}/{y}.png";
        let s = |x: &str| x.to_owned();
        assert_eq!(
            one(&format!("Aerial|{url}|© Example")),
            (s("Aerial"), s(url), s("© Example"), 19)
        );
        assert_eq!(
            one(&format!("Aerial | {url} | © Example | 17 ")),
            (s("Aerial"), s(url), s("© Example"), 17)
        );
        // Without a credit, and a credit that holds a `|` of its own.
        assert_eq!(one(&format!("Aerial|{url}||18")).2, "");
        assert_eq!(one(&format!("Aerial|{url}||18")).3, 18);
        assert_eq!(
            one(&format!("Aerial|{url}|© A | © B")),
            (s("Aerial"), s(url), s("© A | © B"), 19)
        );
        assert_eq!(one(&format!("Aerial|{url}|© A | © B|16")).2, "© A | © B");
        // A year at the end of a credit stays the credit's.
        assert_eq!(
            one(&format!("Aerial|{url}|© Example | 2024")),
            (s("Aerial"), s(url), s("© Example | 2024"), 19)
        );
        for bad in [
            format!("Aerial|{url}|© Example|25"),
            format!("Aerial|{url}|x|99"),
        ] {
            let e = read_basemaps(&[bad]).unwrap_err().to_string();
            assert!(e.contains("MAXZOOM is a zoom from 0 to 24"), "{e}");
        }
    }

    fn lookup_joins(args: &[&str]) -> Result<bool, clap::Error> {
        let mut argv = vec!["tripletrail", "serve"];
        argv.extend_from_slice(args);
        match Cli::try_parse_from(argv)?.command {
            Command::Serve(args) => Ok(args.lookup_joins),
            _ => unreachable!(),
        }
    }

    #[test]
    fn lookup_joins_takes_boolish_values() {
        // The only test that touches this variable, so tests running at once do not race.
        // SAFETY: no other test reads or writes TRIPLETRAIL_LOOKUP_JOINS.
        unsafe { std::env::remove_var("TRIPLETRAIL_LOOKUP_JOINS") };
        assert!(lookup_joins(&[]).unwrap());
        assert!(lookup_joins(&["--lookup-joins"]).unwrap());
        assert!(lookup_joins(&["--lookup-joins", "--timeout", "5"]).unwrap());
        for (value, on) in [
            ("true", true),
            ("false", false),
            ("1", true),
            ("0", false),
            ("yes", true),
            ("no", false),
            ("on", true),
            ("off", false),
        ] {
            assert_eq!(
                lookup_joins(&["--lookup-joins", value]).unwrap(),
                on,
                "{value}"
            );
            assert_eq!(
                lookup_joins(&[&format!("--lookup-joins={value}")]).unwrap(),
                on,
                "{value}"
            );
            unsafe { std::env::set_var("TRIPLETRAIL_LOOKUP_JOINS", value) };
            assert_eq!(lookup_joins(&[]).unwrap(), on, "env {value}");
            assert_eq!(lookup_joins_from_env(), Some(on), "env {value}");
            // The flag wins over the variable.
            assert!(lookup_joins(&["--lookup-joins"]).unwrap(), "env {value}");
        }
        unsafe { std::env::remove_var("TRIPLETRAIL_LOOKUP_JOINS") };
        assert_eq!(lookup_joins_from_env(), None);
        assert!(lookup_joins(&["--lookup-joins", "maybe"]).is_err());
    }

    #[test]
    fn sizes_take_binary_suffixes() {
        assert_eq!(parse_size("1024"), Ok(1024));
        assert_eq!(parse_size("32G"), Ok(32 << 30));
        assert_eq!(parse_size("512m"), Ok(512 << 20));
        assert_eq!(parse_size("2GiB"), Ok(2 << 30));
        assert_eq!(parse_size("4KB"), Ok(4 << 10));
        assert!(parse_size("lots").is_err());
        assert!(parse_size("-1G").is_err());
    }
}
