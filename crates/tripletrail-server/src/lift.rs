//! `tripletrail lift`: runs a folder of SPARQL CONSTRUCT queries over files through the Facade-X
//! SERVICE (`SERVICE <x-sparql-anything:…>`), and of R2RML/RML mapping documents
//! (`*.rml.ttl`, `*.r2rml.ttl`, `*.yarrrml`), in parallel, and writes what they make. Mappings
//! read tables and SQL queries from PostgreSQL: on their `d2rq:Database`, or (R2RML) on
//! `--database`. Facade-X `table=` and `sql=` sources read from `--database`, or from a
//! `--connection` they name with `database=<name>`.
//!
//! A mapping's logical targets (RML-IO `rml:logicalTarget`) are written as files: their
//! paths are relative to `--output-dir`, else the base directory, and may not leave it.
//! Without `--output-dir`, a target file this run did not write is refused, not overwritten,
//! unless `--overwrite-targets` is given (pipeline lift steps always overwrite).
//! Target files are written once the default output is. See `docs/rml.md`.

use crate::lift_out::{Limits, Output};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tripletrail::Engine;
use tripletrail::fx::FxConfig;
use tripletrail::oxigraph::io::{RdfFormat, RdfSerializer};
use tripletrail::oxigraph::model::TripleRef;
use tripletrail::oxigraph::sparql::QueryResults;
use tripletrail_rml::{FsLoader, Loader, Mapping};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub struct LiftArgs {
    pub queries: Vec<PathBuf>,
    pub base: Option<PathBuf>,
    pub allow: Vec<PathBuf>,
    pub output: Option<PathBuf>,
    pub output_dir: Option<PathBuf>,
    pub jobs: usize,
    /// Query parameters (`-v name=value`).
    pub values: HashMap<String, String>,
    /// Write each file's plan, with what it read and made, instead of its triples.
    pub explain: bool,
    /// Do not write progress to standard error.
    pub quiet: bool,
    /// Overwrite logical target files that exist, even without `output_dir` (a pipeline's
    /// own lift step, which reruns).
    pub overwrite_targets: bool,
    /// The URLs Facade-X locations may name.
    pub remote: tripletrail::fx::remote::RemoteLocations,
    /// The database of R2RML mappings, which name none.
    #[cfg(feature = "postgres")]
    pub database: Option<String>,
    /// Named connections, for Facade-X sources with `database=<name>`.
    #[cfg(feature = "postgres")]
    pub connections: HashMap<String, String>,
}

/// One query's outcome.
struct Done {
    triples: usize,
    seconds: f64,
}

/// What one query or mapping file of a lift did.
pub struct LiftFile {
    pub file: PathBuf,
    /// The triples it made (each once), or the plan's count with `explain`.
    pub triples: usize,
    pub seconds: f64,
    /// Why it failed.
    pub error: Option<String>,
}

/// What a lift did, file by file (in the order they finished).
pub struct LiftReport {
    pub files: Vec<LiftFile>,
    pub seconds: f64,
}

impl LiftReport {
    /// How many files failed.
    pub fn failed(&self) -> usize {
        self.files.iter().filter(|f| f.error.is_some()).count()
    }

    /// The triples of all files.
    pub fn triples(&self) -> usize {
        self.files.iter().map(|f| f.triples).sum()
    }
}

/// The W3C direct mapping of `database`'s tables in `schema` (all, or `tables`), as an R2RML
/// mapping with IRIs under `base_iri` (see [`tripletrail_rml::direct`]).
#[cfg(feature = "postgres")]
pub fn direct_mapping(
    database: &str,
    base_iri: &str,
    schema: &str,
    tables: &[String],
) -> Result<String> {
    tripletrail::oxigraph::model::NamedNode::new(base_iri)
        .map_err(|e| format!("--base-iri {base_iri}: {e}"))?;
    let loader = tripletrail_rml::DatabaseLoader::new(
        FsLoader {
            config: FxConfig::default(),
        },
        Some(database.to_owned()),
    );
    let found = loader.direct_tables(schema, tables)?;
    if found.is_empty() {
        return Err(format!("schema {schema} has no tables").into());
    }
    Ok(tripletrail_rml::direct::r2rml(&found, base_iri))
}

/// The ontology, SHACL shapes and SKOS concept schemes of a PostgreSQL schema's tables (#278),
/// named as [`direct_mapping`] with `namespace` as its base IRI names the data. `codes` are
/// the code tables made into concept schemes, each with its label column if given.
#[cfg(feature = "postgres")]
pub fn database_schema(
    database: &str,
    namespace: &str,
    schema: &str,
    tables: &[String],
    codes: &[(String, Option<String>)],
) -> Result<tripletrail_rml::db_schema::Artefacts> {
    tripletrail::oxigraph::model::NamedNode::new(namespace)
        .map_err(|e| format!("--namespace {namespace}: {e}"))?;
    let loader = tripletrail_rml::DatabaseLoader::new(
        FsLoader {
            config: FxConfig::default(),
        },
        Some(database.to_owned()),
    );
    let found = loader.schema_tables(schema, tables)?;
    if found.is_empty() {
        return Err(format!("schema {schema} has no tables").into());
    }
    let mut lists = Vec::new();
    for (name, label) in codes {
        let table = found
            .iter()
            .find(|t| &t.table.name == name)
            .ok_or_else(|| format!("--code-table {name}: there is no such table"))?;
        lists.push(loader.code_list(schema, table, label.as_deref())?);
    }
    Ok(tripletrail_rml::db_schema::artefacts(
        schema, &found, &lists, namespace,
    ))
}

/// Runs the query and mapping files of `args` and writes their triples. A file that fails is
/// reported in the [`LiftReport`] (the others still run); only setting up fails as a whole.
pub fn lift(engine: &Engine, args: LiftArgs) -> Result<LiftReport> {
    let mut files = Vec::new();
    for path in &args.queries {
        collect(path, path, &mut files)?;
    }
    if files.is_empty() {
        return Err(
            "no .rq or .sparql query files or .rml.ttl / .yarrrml mapping files given".into(),
        );
    }
    let base = match args.base {
        Some(base) => base,
        None => std::env::current_dir()?,
    };
    #[cfg_attr(not(feature = "postgres"), allow(unused_mut))]
    let mut config = FxConfig {
        fixed_limits: false,
        databases: None,
        base,
        allowed: (!args.allow.is_empty()).then_some(args.allow),
        remote: args.remote,
    };
    let file_loader = FsLoader {
        config: config.clone(),
    };
    #[cfg(feature = "postgres")]
    let shared = {
        let loader = std::sync::Arc::new(
            tripletrail_rml::DatabaseLoader::new(file_loader, args.database.clone())
                .with_named(args.connections.clone()),
        );
        config.databases = Some(loader.clone());
        loader
    };
    #[cfg(feature = "postgres")]
    let loader = &*shared;
    #[cfg(not(feature = "postgres"))]
    let loader = &file_loader;
    let targets_dir: PathBuf = args
        .output_dir
        .clone()
        .unwrap_or_else(|| config.base.clone());
    let inputs = run_inputs(&files, &config.base);
    let engine = engine.with_fx(config);
    if let Some(output) = &args.output
        && output
            .extension()
            .and_then(|e| e.to_str())
            .and_then(RdfFormat::from_extension)
            != Some(RdfFormat::NTriples)
    {
        return Err(format!(
            "--output {} must be an N-Triples (.nt) file: the queries' triples are \
             written to it as they finish",
            output.display()
        )
        .into());
    }
    if let Some(dir) = &args.output_dir {
        std::fs::create_dir_all(dir)?;
    }
    // One file for everything (or standard output) unless each query gets its own.
    let merged: Option<Mutex<Box<dyn Write + Send>>> = match (&args.output, &args.output_dir) {
        (_, Some(_)) => None,
        (Some(path), None) => Some(Mutex::new(Box::new(std::io::BufWriter::new(
            std::fs::File::create(path)?,
        )))),
        (None, None) => Some(Mutex::new(Box::new(std::io::BufWriter::new(
            std::io::stdout(),
        )))),
    };
    // Temporary files of large outputs: next to the output file, or in the system's.
    let spill_dir = args
        .output
        .as_deref()
        .and_then(Path::parent)
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(std::env::temp_dir, Path::to_path_buf);
    let next = AtomicUsize::new(0);
    let started = Instant::now();
    // Logical target files this run wrote; without --output-dir they land next to the
    // sources, so a file there that this run did not write is refused, not overwritten.
    let written = Mutex::new(HashSet::new());
    let keep_existing = args.output_dir.is_none() && !args.overwrite_targets;
    let done = Mutex::new(Vec::with_capacity(files.len()));
    let quiet = args.quiet;
    std::thread::scope(|scope| {
        for _ in 0..args.jobs.clamp(1, files.len()) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((file, relative)) = files.get(i) else {
                        break;
                    };
                    let name = file.display();
                    let target = args
                        .output_dir
                        .as_ref()
                        .map(|d| d.join(relative).with_extension("nt"));
                    match run(
                        &engine,
                        loader,
                        file,
                        &args.values,
                        args.explain,
                        Outputs {
                            file: target.as_deref(),
                            merged: merged.as_ref(),
                            spill_dir: &spill_dir,
                            targets_dir: &targets_dir,
                            inputs: &inputs,
                            written: &written,
                            keep_existing,
                        },
                    ) {
                        Ok(d) => {
                            if !quiet {
                                eprintln!("{:8.2} s {:>10} triples  {name}", d.seconds, d.triples);
                            }
                            if let Ok(mut done) = done.lock() {
                                done.push(LiftFile {
                                    file: file.clone(),
                                    triples: d.triples,
                                    seconds: d.seconds,
                                    error: None,
                                });
                            }
                        }
                        Err(e) => {
                            if !quiet {
                                eprintln!("failed: {name}: {e}");
                            }
                            if let Ok(mut done) = done.lock() {
                                done.push(LiftFile {
                                    file: file.clone(),
                                    triples: 0,
                                    seconds: 0.0,
                                    error: Some(e.to_string()),
                                });
                            }
                        }
                    }
                }
            });
        }
    });
    if let Some(out) = merged {
        out.into_inner()
            .map_err(|_| "output lock poisoned")?
            .flush()?;
    }
    let report = LiftReport {
        files: done.into_inner().map_err(|_| "lift report lock poisoned")?,
        seconds: started.elapsed().as_secs_f64(),
    };
    if !quiet {
        let failed = report.failed();
        eprintln!(
            "{} queries, {} triples in {:.1} s{}",
            files.len(),
            report.triples(),
            report.seconds,
            if failed > 0 {
                format!(", {failed} failed")
            } else {
                String::new()
            }
        );
    }
    Ok(report)
}

/// `tripletrail lift --as-sparql`: each mapping under `paths` as its Facade-X CONSTRUCT query,
/// to `output` (one mapping only), as `.rq` files under `output_dir` at the mappings' paths,
/// or to standard output after a `# file` comment each. Queries are left out. Gives how many
/// mappings were written.
pub fn as_sparql(
    paths: &[PathBuf],
    output: Option<&Path>,
    output_dir: Option<&Path>,
) -> Result<usize> {
    let mut files = Vec::new();
    for path in paths {
        collect(path, path, &mut files)?;
    }
    files.retain(|(f, _)| is_mapping(f));
    if files.is_empty() {
        return Err("no .rml.ttl, .r2rml.ttl or .yarrrml mapping files given".into());
    }
    if output.is_some() && files.len() > 1 {
        return Err(format!(
            "--output takes the query of one mapping, and {} were given (use --output-dir)",
            files.len()
        )
        .into());
    }
    let mut stdout = String::new();
    for (file, relative) in &files {
        let text = std::fs::read_to_string(file)?;
        let query = tripletrail_rml::as_sparql(&mapping_of(file, &text)?)
            .map_err(|e| format!("{}: {e}", file.display()))?;
        if let Some(out) = output {
            std::fs::write(out, &query)?;
        } else if let Some(dir) = output_dir {
            let name = relative.to_string_lossy();
            let stem = [
                ".rml.ttl",
                ".r2rml.ttl",
                ".yarrrml.yaml",
                ".yarrrml.yml",
                ".yarrrml",
            ]
            .iter()
            .find_map(|ext| name.strip_suffix(ext))
            .map(str::to_owned)
            .unwrap_or_else(|| relative.with_extension("").to_string_lossy().into_owned());
            let target = dir.join(format!("{stem}.rq"));
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(target, &query)?;
        } else {
            stdout.push_str(&format!("# {}\n{query}\n", file.display()));
        }
    }
    print!("{stdout}");
    Ok(files.len())
}

/// Whether `path` is an R2RML or RML mapping document rather than a query.
fn is_mapping(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    [".rml.ttl", ".r2rml.ttl", ".rml", ".r2rml"]
        .iter()
        .any(|ext| name.ends_with(ext))
        || is_yarrrml(path)
}

/// Whether `path` is a YARRRML document.
fn is_yarrrml(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    [".yarrrml", ".yarrrml.yml", ".yarrrml.yaml"]
        .iter()
        .any(|ext| name.ends_with(ext))
}

/// Where a logical target's file goes: `path` under `dir`, which it may not leave.
fn target_path(dir: &Path, path: &str) -> Result<PathBuf> {
    // `http://host/x` would otherwise be the relative path `http:/host/x`.
    if path.contains("://") {
        return Err(format!(
            "the logical target {path} is a URL: lift writes targets to files only"
        )
        .into());
    }
    let relative = Path::new(path);
    if relative.is_absolute()
        || relative.components().any(|c| {
            !matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        return Err(format!(
            "the logical target {path} must be a relative path below {}",
            dir.display()
        )
        .into());
    }
    Ok(dir.join(relative))
}

/// The files a run reads: its query and mapping files, and every source the mappings name
/// (relative to `base`), canonical. A mapping that does not parse is left out here; its run
/// reports the error.
fn run_inputs(files: &[(PathBuf, PathBuf)], base: &Path) -> HashSet<PathBuf> {
    let mut inputs = HashSet::new();
    for (file, _) in files {
        inputs.extend(file.canonicalize());
        if !is_mapping(file) {
            continue;
        }
        let Ok(mapping) = std::fs::read_to_string(file)
            .map_err(Into::into)
            .and_then(|text| mapping_of(file, &text))
        else {
            continue;
        };
        let sources = mapping
            .triples_maps
            .iter()
            .map(|t| &t.source.source)
            .chain(mapping.views.iter().map(|v| &v.on.source));
        for source in sources {
            if let tripletrail_rml::SourceRef::Path(p) = source {
                inputs.extend(
                    base.join(p.strip_prefix("file://").unwrap_or(p))
                        .canonicalize(),
                );
            }
        }
    }
    inputs
}

/// Refuses a logical target that is an input of the run: a file named like a query, mapping
/// or pipeline file, or one of `inputs` (the run's query and mapping files and the sources
/// of all its mappings). A pipeline's lift step overwrites its targets, so a mistyped
/// `rml:path` would otherwise replace a source.
fn refuse_input(target: &Path, inputs: &HashSet<PathBuf>) -> Result<()> {
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let input_like = is_mapping(target)
        || is_yarrrml(target)
        || [".rq", ".sparql", ".ru", ".yaml", ".yml"]
            .iter()
            .any(|ext| name.ends_with(ext));
    let read = target.canonicalize().is_ok_and(|t| inputs.contains(&t));
    if input_like || read {
        return Err(format!(
            "the logical target {} is an input of the run (a query, mapping, pipeline or \
             source file): it is not overwritten",
            target.display()
        )
        .into());
    }
    Ok(())
}

/// Where a run writes: its own file, or appended to the shared output; and the directory of
/// logical targets.
struct Outputs<'a> {
    file: Option<&'a Path>,
    merged: Option<&'a Mutex<Box<dyn Write + Send>>>,
    spill_dir: &'a Path,
    targets_dir: &'a Path,
    /// The files the run reads, which no logical target may overwrite.
    inputs: &'a HashSet<PathBuf>,
    /// Target files written so far in this run.
    written: &'a Mutex<HashSet<PathBuf>>,
    /// Refuse a target file that exists and that this run did not write.
    keep_existing: bool,
}

/// The error of a mapping quad in a named graph, which N-Triples cannot hold.
fn named_graph(q: &oxigraph::model::Quad) -> String {
    format!(
        "the mapping makes quads in the named graph {}: lift writes N-Triples",
        q.graph_name
    )
}

/// Runs one CONSTRUCT or mapping and writes its triples: to `target`, or appended to `merged`
/// in one piece once it is done.
fn run(
    engine: &Engine,
    loader: &(dyn Loader + Sync),
    file: &Path,
    values: &HashMap<String, String>,
    explain: bool,
    out: Outputs<'_>,
) -> Result<Done> {
    let Outputs {
        file: target,
        merged,
        spill_dir,
        targets_dir,
        inputs,
        written,
        keep_existing,
    } = out;
    let started = Instant::now();
    let text = std::fs::read_to_string(file)?;
    if explain {
        // The plan as text, after the file's name.
        let (plan, count) = if is_mapping(file) {
            let report = tripletrail_rml::explain(&mapping_of(file, &text)?, loader)?;
            (
                report.to_string(),
                report.maps.iter().map(|m| m.quads).sum(),
            )
        } else {
            (engine.explain(&bind_parameters(&text, values)?)?, 0)
        };
        if let Some(out) = merged {
            write!(
                out.lock().map_err(|_| "output lock poisoned")?,
                "== {}\n{}\n\n",
                file.display(),
                plan.trim_end()
            )?;
        }
        return Ok(Done {
            triples: count,
            seconds: started.elapsed().as_secs_f64(),
        });
    }
    // Each triple once: the evaluator's CONSTRUCT filters duplicates only until it holds
    // 1,048,576 triples (#377). In bounded memory, with temporary files next to the output
    // (#389).
    let spill = target
        .and_then(Path::parent)
        .unwrap_or(spill_dir)
        .to_path_buf();
    if let Some(dir) = target.and_then(Path::parent) {
        std::fs::create_dir_all(dir)?;
    }
    let mut output = Output::new(&spill, Limits::default());
    let mut line = Vec::new();
    let mut add = |t: TripleRef<'_>, output: &mut Output| -> Result<()> {
        line.clear();
        let mut w = RdfSerializer::from_format(RdfFormat::NTriples).for_writer(&mut line);
        w.serialize_triple(t)?;
        w.finish()?;
        output.line(&line)?;
        Ok(())
    };
    // The logical targets' files and contents, written once the default output is.
    let mut target_files: Vec<(PathBuf, Vec<u8>, usize)> = Vec::new();
    if is_mapping(file) {
        let mapping = mapping_of(file, &text)?;
        let quads = if mapping.targets.is_empty() {
            // Streamed: `output` drops duplicates in bounded memory, so the mapping's quads
            // are never all held at once.
            let mut failed: Option<Box<dyn std::error::Error + Send + Sync>> = None;
            tripletrail_rml::run_with(&mapping, loader, &mut |q| {
                if failed.is_some() {
                    return;
                }
                if !q.graph_name.is_default_graph() {
                    failed = Some(named_graph(&q).into());
                } else if let Err(e) = add(q.as_ref().into(), &mut output) {
                    failed = Some(e);
                }
            })?;
            if let Some(e) = failed {
                return Err(e);
            }
            Vec::new()
        } else {
            let made = tripletrail_rml::run_targets(&mapping, loader)?;
            for (t, quads) in mapping.targets.iter().zip(&made.targets) {
                let path = target_path(targets_dir, &t.path)?;
                refuse_input(&path, inputs)?;
                target_files.push((path, tripletrail_rml::write_target(t, quads)?, quads.len()));
            }
            made.default
        };
        for q in quads {
            if !q.graph_name.is_default_graph() {
                return Err(named_graph(&q).into());
            }
            add(q.as_ref().into(), &mut output)?;
        }
    } else {
        let query = bind_parameters(&text, values)?;
        let QueryResults::Graph(triples) = engine.query(&query)? else {
            return Err("not a CONSTRUCT or DESCRIBE query".into());
        };
        for t in triples {
            add(t?.as_ref(), &mut output)?;
        }
    }
    let count = match (target, merged) {
        (Some(target), _) => {
            let mut w = std::io::BufWriter::new(std::fs::File::create(target)?);
            let n = output.finish(&mut w)?;
            w.flush()?;
            n
        }
        (None, Some(out)) => {
            let mut out = out.lock().map_err(|_| "output lock poisoned")?;
            output.finish(&mut **out)?
        }
        (None, None) => output.finish(&mut std::io::sink())?,
    };
    let mut in_targets = 0;
    for (path, bytes, quads) in target_files {
        {
            let mut written = written.lock().map_err(|_| "target list lock poisoned")?;
            if keep_existing && !written.contains(&path) && path.exists() {
                return Err(format!(
                    "the logical target {} exists and this run did not write it: remove it, \
                     or give --output-dir",
                    path.display()
                )
                .into());
            }
            written.insert(path.clone());
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, bytes)?;
        in_targets += quads;
    }
    Ok(Done {
        triples: count + in_targets,
        seconds: started.elapsed().as_secs_f64(),
    })
}

/// The mapping document of a mapping file: relative IRIs in it resolve against the file.
fn mapping_of(file: &Path, text: &str) -> Result<Mapping> {
    let base = std::path::absolute(file)?;
    if is_yarrrml(file) {
        return Ok(Mapping::parse_yarrrml(
            text,
            Some(&format!("file://{}", base.display())),
        )?);
    }
    Ok(Mapping::parse_turtle(
        text,
        Some(&format!("file://{}", base.display())),
    )?)
}

/// The query and mapping files under `path` (itself if a file), sorted, with their path relative to
/// `root` (the directory given on the command line) for `--output-dir`.
fn collect(root: &Path, path: &Path, out: &mut Vec<(PathBuf, PathBuf)>) -> Result<()> {
    if path.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(path)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::result::Result<_, _>>()?;
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                collect(root, &entry, out)?;
            } else if matches!(
                entry.extension().and_then(|e| e.to_str()),
                Some("rq" | "sparql")
            ) || is_mapping(&entry)
            {
                let relative = entry.strip_prefix(root).unwrap_or(&entry).to_path_buf();
                out.push((entry, relative));
            }
        }
        Ok(())
    } else if path.is_file() {
        let name = PathBuf::from(path.file_name().unwrap_or_default());
        out.push((path.to_path_buf(), name));
        Ok(())
    } else {
        Err(format!("{} does not exist", path.display()).into())
    }
}

/// `name=value` pairs; a name given twice is an error.
pub fn parse_values(values: &[String]) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for v in values {
        let Some((name, value)) = v.split_once('=') else {
            return Err(format!("-v {v}: expected name=value").into());
        };
        if out.insert(name.to_owned(), value.to_owned()).is_some() {
            return Err(format!("-v {name} is given twice").into());
        }
    }
    Ok(out)
}

/// `query` with its parameters replaced by their values, as SPARQL Anything's `-v` (BASIL
/// query templates): `?_name` (or `$_name`) is the literal value, `?_name_literal` too,
/// `?_name_iri` the IRI and `?_name_number` the number as written. `?__name` is the same but
/// optional: without a value it stays a variable. Strings, IRIs and comments are left alone.
/// Without any `-v`, the query is unchanged.
pub fn bind_parameters(query: &str, values: &HashMap<String, String>) -> Result<String> {
    if values.is_empty() {
        return Ok(query.to_owned());
    }
    let chars: Vec<char> = query.chars().collect();
    let mut out = String::with_capacity(query.len());
    let mut missing = Vec::new();
    let mut i = 0;
    let name_char = |c: char| c.is_alphanumeric() || c == '_' || c == '\u{b7}';
    while i < chars.len() {
        let c = chars[i];
        match c {
            '"' | '\'' => {
                // A string, long or short.
                let long = chars.get(i + 1) == Some(&c) && chars.get(i + 2) == Some(&c);
                let quote = if long { 3 } else { 1 };
                let start = i;
                i += quote;
                while i < chars.len() {
                    if chars[i] == '\\' {
                        i += 2;
                        continue;
                    }
                    if chars[i] == c
                        && (!long || (chars.get(i + 1) == Some(&c) && chars.get(i + 2) == Some(&c)))
                    {
                        i += quote;
                        break;
                    }
                    i += 1;
                }
                out.extend(&chars[start..i.min(chars.len())]);
            }
            '#' => {
                let start = i;
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                out.extend(&chars[start..i]);
            }
            '<' => {
                // An IRI has no space before its `>`; otherwise it is the operator.
                let end = chars[i + 1..]
                    .iter()
                    .position(|&c| c == '>' || c.is_whitespace() || c == '<');
                match end {
                    Some(n) if chars[i + 1 + n] == '>' => {
                        out.extend(&chars[i..i + n + 2]);
                        i += n + 2;
                    }
                    _ => {
                        out.push(c);
                        i += 1;
                    }
                }
            }
            '?' | '$' if chars.get(i + 1) == Some(&'_') => {
                let start = i + 1;
                let mut end = start;
                while end < chars.len() && name_char(chars[end]) {
                    end += 1;
                }
                let variable: String = chars[start..end].iter().collect();
                let (optional, rest) = match variable.strip_prefix("__") {
                    Some(rest) => (true, rest),
                    None => (false, &variable[1..]),
                };
                let (name, kind) = match rest.rsplit_once('_') {
                    Some((name, kind @ ("iri" | "number" | "literal"))) => (name, kind),
                    _ => (rest, "literal"),
                };
                match values.get(name) {
                    Some(value) => {
                        out.push_str(&match kind {
                            "iri" => format!("<{value}>"),
                            "number" => value.clone(),
                            _ => string_literal(value),
                        });
                    }
                    None => {
                        if !optional && !missing.contains(&name.to_owned()) {
                            missing.push(name.to_owned());
                        }
                        out.extend(&chars[i..end]);
                    }
                }
                i = end;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    if !missing.is_empty() {
        return Err(format!("no -v value for the parameter(s) {}", missing.join(", ")).into());
    }
    Ok(out)
}

/// `value` as a SPARQL string literal.
fn string_literal(value: &str) -> String {
    tripletrail::oxigraph::model::Literal::new_simple_literal(value).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn parameters_like_sparql_anything() {
        let v = values(&[
            ("fullpath", "/data/a \"b\".xml"),
            ("n", "12"),
            ("base", "http://ex.org/"),
        ]);
        let q = "SELECT * WHERE { # ?_fullpath in a comment\n  fx:properties fx:location ?_fullpath . \
                 BIND(\"?_fullpath\" AS ?s) BIND($_n_number AS ?n) BIND(?_base_iri AS ?b) \
                 FILTER(?x<?_n_number && ?_fullpath != '?_n') OPTIONAL { ?a ?b ?__other } }";
        assert_eq!(
            bind_parameters(q, &v).unwrap(),
            "SELECT * WHERE { # ?_fullpath in a comment\n  fx:properties fx:location \"/data/a \\\"b\\\".xml\" . \
             BIND(\"?_fullpath\" AS ?s) BIND(12 AS ?n) BIND(<http://ex.org/> AS ?b) \
             FILTER(?x<12 && \"/data/a \\\"b\\\".xml\" != '?_n') OPTIONAL { ?a ?b ?__other } }"
        );
        // Without -v the query is unchanged; a mandatory parameter without a value fails.
        assert_eq!(bind_parameters(q, &HashMap::new()).unwrap(), q);
        let err = bind_parameters(q, &values(&[("n", "1"), ("base", "x:")])).unwrap_err();
        assert!(err.to_string().contains("fullpath"), "{err}");
        assert!(parse_values(&["a=1".into(), "a=2".into()]).is_err());
        assert!(parse_values(&["a".into()]).is_err());
        assert_eq!(
            parse_values(&["a=b=c".into()]).unwrap(),
            values(&[("a", "b=c")])
        );
    }
}
