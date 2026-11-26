//! Schemas to RDF with the optional Python tools (#276): `xsd2rdf`, `jsonschema2rdf`,
//! `openapi2rdf` and `asn2rdf` (EUPL-1.2, installed apart with `pip`, never linked in). Each
//! is run in a subprocess in a temporary folder, with a timeout, and what it writes
//! (`<file>.owl.ttl`, `<file>.shape.ttl`, `<file>.skos.<Scheme>.ttl`, `<file>.provenance.ttl`)
//! is read back into the same four artefacts the database schema flow (#278) gives: an
//! ontology, shapes, the concept schemes and where each comes from.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tripletrail::oxigraph::io::{RdfFormat, RdfParser, RdfSerializer};
use tripletrail::oxigraph::model::Triple;

/// One of the schema tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Xsd,
    JsonSchema,
    OpenApi,
    Asn1,
}

impl Tool {
    pub const ALL: [Tool; 4] = [Tool::Xsd, Tool::JsonSchema, Tool::OpenApi, Tool::Asn1];

    /// The tool's program, as `pip` installs it.
    pub fn program(self) -> &'static str {
        match self {
            Tool::Xsd => "xsd2rdf",
            Tool::JsonSchema => "jsonschema2rdf",
            Tool::OpenApi => "openapi2rdf",
            Tool::Asn1 => "asn2rdf",
        }
    }

    /// The name the CLI and the server use: `xsd`, `jsonschema`, `openapi` or `asn1`.
    pub fn name(self) -> &'static str {
        match self {
            Tool::Xsd => "xsd",
            Tool::JsonSchema => "jsonschema",
            Tool::OpenApi => "openapi",
            Tool::Asn1 => "asn1",
        }
    }

    pub fn from_name(name: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| t.name() == name)
    }
}

/// How a tool is run.
#[derive(Debug, Clone)]
pub struct Run {
    /// The program to run instead of the tool's name on `PATH`.
    pub program: Option<PathBuf>,
    /// The IRI the classes, properties, shapes (`<ns>shapes/`) and concepts start with.
    pub namespace: String,
    /// The date of `dct:created`, `dct:issued` and `dct:modified` (`YYYY-MM-DD`; the tool's
    /// default, today, when `None`).
    pub date: Option<String>,
    /// Let `xsd2rdf` download the schemas a schema includes or imports from `http(s)` URLs.
    /// Off: remote imports are skipped and remote includes are an error.
    pub download: bool,
    pub timeout: Duration,
}

/// What a tool made, as Turtle.
#[derive(Debug, Clone, Default)]
pub struct Artefacts {
    pub ontology: String,
    pub shapes: String,
    pub skos: String,
    pub provenance: String,
    /// How many triples each has: ontology, shapes, SKOS, provenance.
    pub counts: [usize; 4],
    /// The concept schemes' names, from the tool's file names.
    pub schemes: Vec<String>,
    /// The tool's warnings.
    pub warnings: Vec<String>,
}

/// Why a run failed.
#[derive(Debug)]
pub enum Error {
    /// The tool is not there: how to install it.
    NotInstalled(String),
    /// It ran over its time and was stopped.
    Timeout(Duration),
    /// It failed: what it said.
    Failed(String),
    Io(std::io::Error),
    Rdf(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotInstalled(s) | Error::Failed(s) | Error::Rdf(s) => f.write_str(s),
            Error::Timeout(t) => write!(
                f,
                "the schema tool did not finish in {}s and was stopped",
                t.as_secs()
            ),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// How to install a tool, said when it is missing.
pub fn install_hint(tool: Tool) -> String {
    format!(
        "{p} is not installed: it is an optional Python tool (EUPL-1.2, Python 3.12 or later). \
         Install it with `pip install {p}`, or give its path with --tool-path",
        p = tool.program()
    )
}

/// Runs `tool` on `file` and reads back what it wrote.
pub fn run(tool: Tool, file: &Path, options: &Run) -> Result<Artefacts, Error> {
    let file = std::fs::canonicalize(file)?;
    let out = tempdir()?;
    let result = run_in(tool, &file, options, &out.0);
    drop(out);
    result
}

fn run_in(tool: Tool, file: &Path, options: &Run, out: &Path) -> Result<Artefacts, Error> {
    let program = options
        .program
        .clone()
        .unwrap_or_else(|| PathBuf::from(tool.program()));
    let mut command = Command::new(&program);
    command
        .arg("convert")
        .arg(file)
        .arg("--output-dir")
        .arg(out)
        .arg("--namespace")
        .arg(&options.namespace)
        .arg("--source-provenance")
        .arg("--strict")
        .arg("--quiet")
        .current_dir(out)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(date) = &options.date {
        command.arg("--date").arg(date);
    }
    if tool == Tool::Xsd && !options.download {
        command.arg("--no-download");
    }
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::NotInstalled(install_hint(tool)));
        }
        Err(e) => return Err(e.into()),
    };
    // Read both pipes while waiting, so a talkative tool does not block on a full pipe.
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let out_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= options.timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Timeout(options.timeout));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let said = format!(
        "{}{}",
        out_reader.join().unwrap_or_default(),
        err_reader.join().unwrap_or_default()
    );
    if !status.success() {
        let tail: Vec<&str> = said.lines().rev().take(20).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        return Err(Error::Failed(format!(
            "{} failed ({status}):\n{}",
            tool.program(),
            tail.join("\n")
        )));
    }
    let base = file
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();
    let mut made = Artefacts {
        warnings: said
            .lines()
            .filter_map(|l| l.strip_prefix("Warning: "))
            .map(str::to_owned)
            .collect(),
        ..Artefacts::default()
    };
    let mut groups: [Vec<Triple>; 4] = Default::default();
    let mut files: Vec<PathBuf> = std::fs::read_dir(out)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    files.sort();
    for path in files {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(rest) = name.strip_prefix(&base).and_then(|r| r.strip_prefix('.')) else {
            continue;
        };
        let Some(kind) = rest.strip_suffix(".ttl") else {
            continue;
        };
        let group = match kind {
            "owl" => 0,
            "shape" => 1,
            "provenance" => 3,
            k => match k.strip_prefix("skos.") {
                Some(scheme) => {
                    made.schemes.push(scheme.to_owned());
                    2
                }
                None => continue,
            },
        };
        groups[group].extend(read(&path)?);
    }
    if groups[0].is_empty() && groups[1].is_empty() {
        return Err(Error::Failed(format!(
            "{} wrote no ontology or shapes for {base}:\n{said}",
            tool.program()
        )));
    }
    let [ontology, shapes, skos, provenance] = groups;
    made.counts = [ontology.len(), shapes.len(), skos.len(), provenance.len()];
    made.ontology = turtle(&ontology)?;
    made.shapes = turtle(&shapes)?;
    made.skos = turtle(&skos)?;
    made.provenance = turtle(&provenance)?;
    Ok(made)
}

/// A file's triples, its blank nodes made distinct from those of the other files.
fn read(path: &Path) -> Result<Vec<Triple>, Error> {
    let bytes = std::fs::read(path)?;
    RdfParser::from_format(RdfFormat::Turtle)
        .rename_blank_nodes()
        .for_slice(&bytes)
        .map(|q| q.map(Triple::from))
        .collect::<Result<_, _>>()
        .map_err(|e| Error::Rdf(format!("{}: {e}", path.display())))
}

const PREFIXES: [(&str, &str); 9] = [
    ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
    ("owl", "http://www.w3.org/2002/07/owl#"),
    ("xsd", "http://www.w3.org/2001/XMLSchema#"),
    ("sh", "http://www.w3.org/ns/shacl#"),
    ("skos", "http://www.w3.org/2004/02/skos/core#"),
    ("dct", "http://purl.org/dc/terms/"),
    ("prov", "http://www.w3.org/ns/prov#"),
    ("dash", "http://datashapes.org/dash#"),
];

fn turtle(triples: &[Triple]) -> Result<String, Error> {
    let mut serializer = RdfSerializer::from_format(RdfFormat::Turtle);
    for (p, iri) in PREFIXES {
        serializer = serializer
            .with_prefix(p, iri)
            .map_err(|e| Error::Rdf(e.to_string()))?;
    }
    let mut writer = serializer.for_writer(Vec::new());
    for t in triples {
        writer.serialize_triple(t)?;
    }
    String::from_utf8(writer.finish()?).map_err(|e| Error::Rdf(e.to_string()))
}

/// A temporary folder, removed when dropped.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> std::io::Result<TempDir> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!(
        "tripletrail-schema-{}-{nanos}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&dir)?;
    Ok(TempDir(dir))
}
