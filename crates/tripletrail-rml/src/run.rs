//! Running a [`Mapping`] over its sources.
//!
//! Each source is read once. Joins with conditions hash the parent's records once, on the
//! parent's join references, before any triple is made; maps without joins go straight from
//! records to quads.

use crate::RmlError;
use crate::jsonpath::Path;
use crate::mapping::{
    Collection, Encoding, Formulation, Gather, LogicalSource, Mapping, ObjectMap, Part, Quoted,
    SourceRef, TermMap, TermType, Value,
};
use oxigraph::model::vocab::{rdf, xsd};
use oxigraph::model::{
    BlankNode, GraphName, Literal, NamedNode, NamedNodeRef, NamedOrBlankNode, Quad, Term, Triple,
};
use oxiri::Iri;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use tripletrail_fx::sources::{self, CsvOptions, CsvRows};
use tripletrail_fx::{FxConfig, FxError, Options};

/// How many collections the Cartesian product strategy of a gather map may make for one
/// record: each value of each item multiplies them.
const MAX_COMBINATIONS: usize = 100_000;
/// Reads the bytes of a logical source. It decides which files a mapping may read.
pub trait Loader {
    fn load(&self, source: &LogicalSource) -> Result<Vec<u8>, RmlError>;

    /// The rows of a table or query ([`Formulation::Sql`]): on the source's
    /// [`database`](LogicalSource::database), or the loader's default database when the
    /// mapping names none (R2RML). Loaders read no databases unless they say so.
    fn table(&self, source: &LogicalSource) -> Result<Table, RmlError> {
        let _ = source;
        Err(RmlError::Unsupported("database sources".into()))
    }
}

/// The result of a table or query: its column names and rows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Table {
    pub columns: Vec<String>,
    /// A cell per column; `None` is SQL `NULL`, which makes no term.
    pub rows: Vec<Vec<Option<Cell>>>,
}

/// A value of a table: the lexical form of its natural RDF literal (R2RML 10.2) and the
/// literal's datatype (`xsd:string` for text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub lexical: String,
    pub datatype: NamedNode,
}

impl Cell {
    pub fn new(lexical: impl Into<String>, datatype: NamedNodeRef<'_>) -> Self {
        Self {
            lexical: lexical.into(),
            datatype: datatype.into_owned(),
        }
    }
}

/// A table's rows as JSON objects of their cells by column, a cell being `null` or
/// `[lexical, datatype]`: the records of an SQL source.
fn table_records(table: Table) -> serde_json::Value {
    table
        .rows
        .into_iter()
        .map(|row| {
            table
                .columns
                .iter()
                .zip(row)
                .map(|(c, cell)| {
                    (
                        c.clone(),
                        match cell {
                            None => serde_json::Value::Null,
                            Some(cell) => serde_json::json!([cell.lexical, cell.datatype.as_str()]),
                        },
                    )
                })
                .collect::<serde_json::Map<_, _>>()
                .into()
        })
        .collect::<Vec<serde_json::Value>>()
        .into()
}

/// A [`Loader`] of in-memory files, by the path written in the mapping.
#[derive(Debug, Default, Clone)]
pub struct MemoryLoader(pub HashMap<String, Vec<u8>>);

impl<K: Into<String>, V: Into<Vec<u8>>, const N: usize> From<[(K, V); N]> for MemoryLoader {
    fn from(files: [(K, V); N]) -> Self {
        Self(
            files
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }
}

impl Loader for MemoryLoader {
    fn load(&self, source: &LogicalSource) -> Result<Vec<u8>, RmlError> {
        let SourceRef::Path(p) = &source.source else {
            return Err(RmlError::Unsupported("database sources".into()));
        };
        self.0.get(p).cloned().ok_or_else(|| RmlError::Source {
            location: p.clone(),
            message: "not found".into(),
        })
    }
}

/// A [`Loader`] of files: paths (and `file:` URLs) are resolved against [`FxConfig::base`]
/// and must be inside [`FxConfig::allowed`], as Facade-X sources are. `http(s)://` URLs are
/// read when [`FxConfig::remote`] allows them.
#[derive(Debug, Clone, Default)]
pub struct FsLoader {
    pub config: FxConfig,
}

impl Loader for FsLoader {
    fn load(&self, source: &LogicalSource) -> Result<Vec<u8>, RmlError> {
        let SourceRef::Path(p) = &source.source else {
            return Err(RmlError::Unsupported("database sources".into()));
        };
        if tripletrail_fx::remote::is_url(p) {
            return tripletrail_fx::remote::read(&self.config.remote, p).map_err(|e| {
                RmlError::Source {
                    location: p.clone(),
                    message: e.to_string(),
                }
            });
        }
        let path = sources::resolve(&self.config, p).map_err(|e| RmlError::Source {
            location: p.clone(),
            message: e.to_string(),
        })?;
        std::fs::read(&path).map_err(|e| RmlError::Source {
            location: p.clone(),
            message: e.to_string(),
        })
    }
}

/// Runs `mapping`, giving its quads without duplicates, in the order first made.
pub fn run(mapping: &Mapping, loader: &dyn Loader) -> Result<Vec<Quad>, RmlError> {
    let mut seen = HashSet::new();
    let mut quads = Vec::new();
    run_with(mapping, loader, &mut |q| {
        if seen.insert(q.clone()) {
            quads.push(q);
        }
    })?;
    Ok(quads)
}

/// Runs `mapping`, giving each quad to `sink` as it is made (a quad made twice is given twice).
/// Returns the number of quads made.
pub fn run_with(
    mapping: &Mapping,
    loader: &dyn Loader,
    sink: &mut dyn FnMut(Quad),
) -> Result<usize, RmlError> {
    let plan = Plan::new(mapping, loader)?;
    plan.run(&mut |q, _| sink(q))
}

/// The quads of a run sorted by logical target (RML-IO), without duplicates.
#[derive(Debug, Default)]
pub struct TargetedQuads {
    /// The quads of term maps that name no logical target.
    pub default: Vec<Quad>,
    /// The quads of each logical target, by index in [`Mapping::targets`].
    pub targets: Vec<Vec<Quad>>,
}

/// Runs `mapping`, sending each quad to the logical targets of the term maps that made it
/// (its subject, predicate, object and graph maps, and their datatype and language maps), or
/// to the default output when none names one. [`run`] gives every quad, whatever its targets.
pub fn run_targets(mapping: &Mapping, loader: &dyn Loader) -> Result<TargetedQuads, RmlError> {
    let plan = Plan::new(mapping, loader)?;
    let mut out = TargetedQuads {
        default: Vec::new(),
        targets: vec![Vec::new(); mapping.targets.len()],
    };
    let mut seen: Vec<HashSet<Quad>> = vec![HashSet::new(); mapping.targets.len() + 1];
    plan.run(&mut |q, targets| {
        if targets.is_empty() {
            if seen[0].insert(q.clone()) {
                out.default.push(q);
            }
        } else {
            for &t in targets {
                if seen[t + 1].insert(q.clone()) {
                    out.targets[t].push(q.clone());
                }
            }
        }
    })?;
    Ok(out)
}

/// A record of a source: a JSON value, a row of a CSV file, or an object of cells by column
/// name (CSV text of a view or a spreadsheet).
#[derive(Clone, Copy)]
struct Rec<'a> {
    value: &'a serde_json::Value,
    /// A row of a CSV file, with its columns; `value` is then `null`.
    row: Option<(&'a CsvRow, &'a HashMap<String, usize>)>,
    /// Its references are keys (CSV, SQL, views), not JSONPath.
    csv: bool,
    /// A table row: its cells are `[lexical, datatype]` (see [`table_records`]).
    sql: bool,
    /// An XML record: each reference's values are an array of strings (see [`xml_records`]).
    xml: bool,
    /// A CSV row: a reference to a column the header does not name is an error.
    columns: bool,
}

/// Whether a source path names a spreadsheet, read as a CSV source (see [`sheet_records`]).
pub(crate) fn is_spreadsheet(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".xlsx", ".xlsm", ".xlsb", ".xls", ".ods"]
        .iter()
        .any(|e| lower.ends_with(e))
}

/// The rows of a spreadsheet source, as RMLMapper and Morph-KGC read them with the CSV
/// reference formulation: the sheet `rml:iterator` names (by default the first) is a table
/// whose first non-empty row names the columns. Each later non-empty row is a record of its
/// cells by column name; a cell that is empty, or equal to one of the `rml:null` values,
/// is `null`.
fn sheet_records(source: &LogicalSource, bytes: &[u8]) -> Result<serde_json::Value, FxError> {
    let mut rows = sources::sheet_cells(bytes, source.iterator.as_deref())?.into_iter();
    let Some((_, header)) = rows.next() else {
        return Ok(serde_json::Value::Array(Vec::new()));
    };
    // A column named twice is read from its first one.
    let mut columns: Vec<(u32, String)> = Vec::new();
    for (c, name) in header {
        if !columns.iter().any(|(_, n)| *n == name) {
            columns.push((c, name));
        }
    }
    let nulls = source.io.nulls.as_deref().unwrap_or_default();
    let out = rows
        .map(|(_, row)| {
            let record: serde_json::Map<_, _> = columns
                .iter()
                .map(|(c, name)| {
                    let value = match row.get(c) {
                        Some(v) if !nulls.iter().any(|n| n == v) => v.as_str().into(),
                        _ => serde_json::Value::Null,
                    };
                    (name.clone(), value)
                })
                .collect();
            record.into()
        })
        .collect();
    Ok(serde_json::Value::Array(out))
}

/// The rows of a CSV source as JSON objects of their cells by header. An empty cell is `null`:
/// it makes no term, as in RMLMapper and Morph-KGC.
fn csv_records(source: &LogicalSource, bytes: &[u8]) -> Result<serde_json::Value, FxError> {
    if let SourceRef::Path(p) = &source.source
        && is_spreadsheet(p)
    {
        return sheet_records(source, bytes);
    }
    let (tab, nulls) = csv_dialect(source);
    csv_rows_with(bytes, tab, nulls, source.io.rml_io)
}

/// The rows of CSV (or, with `tab`, TSV) text with a header row, as in [`csv_records`].
pub(crate) fn csv_rows(bytes: &[u8], tab: bool) -> Result<serde_json::Value, FxError> {
    csv_rows_with(bytes, tab, &[String::new()], false)
}

/// The rows of CSV text: cells equal to one of `nulls` are no value; with `strict`, a row
/// with more or fewer cells than the header is an error.
fn csv_rows_with(
    bytes: &[u8],
    tab: bool,
    nulls: &[String],
    strict: bool,
) -> Result<serde_json::Value, FxError> {
    let mut out = Vec::new();
    each_csv_row(bytes, tab, nulls, strict, |headers, cells| {
        let mut record = serde_json::Map::new();
        for (name, cell) in headers.iter().zip(cells) {
            if let Some(name) = name {
                record.insert(
                    name.clone(),
                    cell.map_or(serde_json::Value::Null, Into::into),
                );
            }
        }
        out.push(record.into());
        Ok(())
    })?;
    Ok(serde_json::Value::Array(out))
}

/// Calls `f` with the header and each row's cells, one per header column: `None` for a cell
/// equal to one of `nulls` or missing from a short row (outside RML-IO a missing cell is null,
/// as an empty one is). With `strict`, a row with more or fewer cells than the header is an
/// error. Gives the header.
fn each_csv_row(
    bytes: &[u8],
    tab: bool,
    nulls: &[String],
    strict: bool,
    mut f: impl FnMut(&[Option<String>], &[Option<&str>]) -> Result<(), FxError>,
) -> Result<Vec<Option<String>>, FxError> {
    let mut options = Options::default();
    options.0.insert("csv.headers".into(), "true".into());
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let mut rows = CsvRows::new(bytes, &CsvOptions::new(&options, tab)?)?;
    let headers = rows.headers.clone();
    let mut line = 1;
    while let Some(row) = rows.next_row()? {
        line += 1;
        let mut cells: Vec<Option<&str>> = Vec::with_capacity(headers.len());
        cells.extend(
            row.cells()
                .take(headers.len())
                .map(|c| c.filter(|c| !nulls.iter().any(|n| n == c))),
        );
        let given = cells.len();
        cells.resize(headers.len(), None);
        let all = row.cells().count();
        if strict && (given < headers.len() || all > headers.len()) {
            return Err(FxError::Parse(format!(
                "CSV row {line} has {all} cells, the header {}",
                headers.len()
            )));
        }
        f(&headers, &cells)?;
    }
    Ok(headers)
}

/// The rows of a CSV file source, kept compact: a file of millions of rows is held as its
/// text, not as JSON objects.
pub(crate) struct CsvTable {
    /// The column of each name (a name given twice is read from its last column).
    columns: HashMap<String, usize>,
    rows: Vec<CsvRow>,
}

/// A CSV row: its cells' text one after the other, and where each cell ends (the top bit
/// set for a null cell).
pub(crate) struct CsvRow {
    text: Box<str>,
    ends: Box<[u32]>,
}

const NULL_CELL: u32 = 1 << 31;

impl CsvRow {
    fn cell(&self, i: usize) -> Option<&str> {
        let end = *self.ends.get(i)?;
        if end & NULL_CELL != 0 {
            return None;
        }
        let start = if i == 0 {
            0
        } else {
            self.ends[i - 1] & !NULL_CELL
        };
        self.text.get(start as usize..end as usize)
    }
}

/// A CSV file source's rows (not a spreadsheet's), as [`csv_records`] reads them.
fn csv_table(source: &LogicalSource, bytes: &[u8]) -> Result<CsvTable, FxError> {
    let (tab, nulls) = csv_dialect(source);
    let mut rows = Vec::new();
    let mut text = String::new();
    let headers = each_csv_row(bytes, tab, nulls, source.io.rml_io, |_, cells| {
        text.clear();
        let mut ends = Vec::with_capacity(cells.len());
        for cell in cells {
            text.push_str(cell.unwrap_or_default());
            let end = u32::try_from(text.len())
                .ok()
                .filter(|e| e & NULL_CELL == 0)
                .ok_or_else(|| FxError::Parse("a CSV row of more than 2 GiB".into()))?;
            ends.push(if cell.is_some() { end } else { end | NULL_CELL });
        }
        rows.push(CsvRow {
            text: text.as_str().into(),
            ends: ends.into(),
        });
        Ok(())
    })?;
    let columns = headers
        .into_iter()
        .enumerate()
        .filter_map(|(i, name)| Some((name?, i)))
        .collect();
    Ok(CsvTable { columns, rows })
}

/// Whether a CSV source is tab-separated, and the cell values that are null in it.
fn csv_dialect(source: &LogicalSource) -> (bool, &[String]) {
    static LEGACY: [String; 1] = [String::new()];
    let tab = matches!(&source.source, SourceRef::Path(p)
        if p.to_ascii_lowercase().ends_with(".tsv") || p.to_ascii_lowercase().ends_with(".tab"));
    // Legacy RML reads an empty cell as no value; RML-IO only the `rml:null` values.
    let nulls = match &source.io.nulls {
        Some(n) => n.as_slice(),
        None if source.io.rml_io => &[],
        None => &LEGACY,
    };
    (tab, nulls)
}

/// The subjects of a parent triples map by the values of its join expressions.
type JoinIndex = HashMap<Vec<String>, Vec<Term>>;

/// The records of a quoted triples map, by the values of its join expressions.
type RecordIndex = HashMap<Vec<String>, Vec<usize>>;

/// A mapping with its sources read, its paths parsed and its join indexes built.
pub(crate) struct Plan<'m> {
    mapping: &'m Mapping,
    /// Per triples map, the base IRI its relative IRIs resolve against.
    bases: Vec<Option<Iri<String>>>,
    sources: HashMap<&'m LogicalSource, serde_json::Value>,
    /// The CSV file sources, read compact instead of into `sources`.
    tables: HashMap<&'m LogicalSource, CsvTable>,
    /// The records of each logical view, as arrays of objects.
    views: Vec<serde_json::Value>,
    paths: HashMap<&'m str, Path>,
    /// Per (triples map, predicate-object map, object map) with join conditions: the parent
    /// subjects by join key.
    joins: HashMap<(usize, usize, usize), JoinIndex>,
    /// Join indexes of referencing object maps in gather maps, by (gather map address, item).
    gather_joins: RefCell<HashMap<(usize, usize), JoinIndex>>,
    /// Collections and containers made by gather maps, not yet given their graphs.
    pending: RefCell<Vec<Pending>>,
    /// The triples of each quoted triples map (RML-star), per record of its source.
    quoted: RefCell<HashMap<usize, Rc<Vec<Vec<QuotedTriple>>>>>,
    /// The quoted triples maps being run, to refuse a cycle.
    quoting: RefCell<HashSet<usize>>,
    /// Per quoted map with join conditions, by (triples map, predicate-object map, object
    /// map), or (triples map, `usize::MAX`, 0) for a quoted subject: the quoted map's records
    /// by join key.
    quoted_joins: RefCell<HashMap<(usize, usize, usize), RecordIndex>>,
}

/// A triple a quoted triples map made, named by its reifier, with the triples stating
/// the reifiers it uses (`_:r rdf:reifies <<( s p o )>>`, its own first).
#[derive(Clone)]
struct QuotedTriple {
    reifier: BlankNode,
    sides: Rc<[Triple]>,
}

/// The reifier of a quoted triple: a blank node given by the triple (the first 128 bits of
/// the SHA-256 of its N-Triples form), so that every quote of the same triple names the same
/// reifier.
fn reifier(t: &Triple) -> BlankNode {
    use sha2::Digest;
    // SHA-256 rather than the standard hasher, which may change between Rust releases:
    // the same mapping lifts to the same reifiers with any build.
    let digest = sha2::Sha256::digest(t.to_string().as_bytes());
    let mut id = [0; 16];
    id.copy_from_slice(&digest[..16]);
    BlankNode::new_from_unique_id(u128::from_be_bytes(id))
}

/// Where the quads of [`Plan::run_map`] go: each with the index of the record that made it
/// (none for collections), whether it states a reifier, and its logical targets.
type Sink<'s> = dyn FnMut(Option<usize>, bool, Quad, &[usize]) + 's;

/// A collection or container a gather map made: its head, kind and members.
struct Pending {
    head: Term,
    kind: Collection,
    members: Vec<Term>,
    /// Named by the gather map's value: the members of every record naming it are gathered.
    named: bool,
    targets: Vec<usize>,
}

/// A collection or container to write: head, graph, kind, members and logical targets.
type Made = (Term, GraphName, Collection, Vec<Term>, Vec<usize>);

/// Collections and containers per (head, graph), in the order made.
#[derive(Default)]
struct Collections {
    index: HashMap<(Term, GraphName), usize>,
    list: Vec<Made>,
}

impl Collections {
    fn add(&mut self, p: &Pending, graph: &GraphName) {
        if p.named
            && let Some(&i) = self.index.get(&(p.head.clone(), graph.clone()))
        {
            self.list[i].3.extend(p.members.iter().cloned());
            return;
        }
        if p.named {
            self.index
                .insert((p.head.clone(), graph.clone()), self.list.len());
        }
        self.list.push((
            p.head.clone(),
            graph.clone(),
            p.kind,
            p.members.clone(),
            p.targets.clone(),
        ));
    }
}

const RDF_NS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

/// A named blank node head in a named graph: its own node per graph, as RML-CC asks.
fn in_graph(t: &Term, g: &GraphName, heads: &HashSet<BlankNode>) -> Term {
    match (t, g) {
        (Term::BlankNode(b), GraphName::NamedNode(n)) if heads.contains(b) => {
            let mut label = format!("{}_g", b.as_str());
            for byte in n.as_str().bytes() {
                if byte.is_ascii_alphanumeric() {
                    label.push(byte as char);
                } else {
                    label.push_str(&format!("_{byte:02x}"));
                }
            }
            BlankNode::new_unchecked(label).into()
        }
        _ => t.clone(),
    }
}

fn is_default_graph(n: NamedNodeRef<'_>) -> bool {
    [
        "http://www.w3.org/ns/r2rml#",
        "http://semweb.mmlab.be/ns/rml#",
        "http://w3id.org/rml/",
    ]
    .iter()
    .any(|ns| n.as_str().strip_prefix(ns) == Some("defaultGraph"))
}

/// The term maps a term map uses: datatype and language maps, condition, function inputs.
fn nested(map: &TermMap) -> impl Iterator<Item = &TermMap> {
    map.children()
}

/// The text a join compares: an IRI's or a literal's.
fn join_text(t: &Term) -> Option<String> {
    match t {
        Term::NamedNode(n) => Some(n.as_str().to_owned()),
        Term::Literal(l) => Some(l.value().to_owned()),
        _ => None,
    }
}

impl<'m> Plan<'m> {
    pub(crate) fn new(mapping: &'m Mapping, loader: &dyn Loader) -> Result<Self, RmlError> {
        let parse_base = |b: &String| {
            Iri::parse(b.clone())
                .map_err(|e| RmlError::Mapping(format!("invalid base IRI {b}: {e}")))
        };
        let default = mapping.base_iri.as_ref().map(parse_base).transpose()?;
        let bases = mapping
            .triples_maps
            .iter()
            .map(|tm| match &tm.base_iri {
                Some(b) => parse_base(b).map(Some),
                None => Ok(default.clone()),
            })
            .collect::<Result<_, _>>()?;
        crate::views::check_views(mapping)?;
        let read = |source: &LogicalSource| read_source(source, loader, None);
        let views = crate::views::Builder::new(mapping, &read).build()?;
        let mut plan = Self {
            mapping,
            bases,
            views,
            sources: HashMap::new(),
            tables: HashMap::new(),
            paths: HashMap::new(),
            joins: HashMap::new(),
            gather_joins: RefCell::default(),
            pending: RefCell::default(),
            quoted: RefCell::default(),
            quoting: RefCell::default(),
            quoted_joins: RefCell::default(),
        };
        for tm in &mapping.triples_maps {
            if let SourceRef::View(v) = tm.source.source {
                let mut maps: Vec<&TermMap> = vec![&tm.subject];
                maps.extend(&tm.graphs);
                for pom in &tm.predicate_objects {
                    maps.extend(pom.predicates.iter().chain(&pom.graphs));
                    for o in &pom.objects {
                        match o {
                            ObjectMap::Term(m) => maps.push(m),
                            ObjectMap::Join { conditions, .. }
                            | ObjectMap::Quoted(Quoted { conditions, .. }) => {
                                maps.extend(conditions.iter().map(|(c, _)| c))
                            }
                        }
                    }
                }
                crate::views::check(mapping, v, maps)?;
            }
            for (parent, conditions) in quoted_and_joins(tm) {
                if let SourceRef::View(v) = mapping.triples_maps[parent].source.source {
                    crate::views::check(mapping, v, conditions.iter().map(|(_, p)| p))?;
                }
            }
            match &tm.source.formulation {
                Formulation::JsonPath
                | Formulation::Csv
                | Formulation::Sql
                | Formulation::XPath => {}
                Formulation::View => continue,
                Formulation::Other(f) => {
                    return Err(RmlError::Unsupported(format!(
                        "reference formulation <{f}>"
                    )));
                }
            }
            let csv_file = tm.source.formulation == Formulation::Csv
                && matches!(&tm.source.source, SourceRef::Path(p) if !is_spreadsheet(p));
            if csv_file {
                if !plan.tables.contains_key(&tm.source) {
                    let bytes = source_bytes(&tm.source, loader)?;
                    let table = csv_table(&tm.source, &bytes).map_err(|e| RmlError::Source {
                        location: describe(&tm.source),
                        message: e.to_string(),
                    })?;
                    plan.tables.insert(&tm.source, table);
                }
            } else if !plan.sources.contains_key(&tm.source) {
                let references = source_references(mapping, &tm.source);
                plan.sources.insert(
                    &tm.source,
                    read_source(&tm.source, loader, Some(&references))?,
                );
            }
            let json = tm.source.formulation == Formulation::JsonPath;
            if json {
                plan.path(tm.source.iterator.as_deref().unwrap_or("$"))?;
            }
            for map in std::iter::once(&tm.subject).chain(&tm.graphs) {
                plan.term_map_paths(map, json)?;
            }
            if let Some(q) = &tm.quoted_subject {
                plan.joined_paths(tm, q.map, &q.conditions, json)?;
            }
            for pom in &tm.predicate_objects {
                for map in pom.predicates.iter().chain(&pom.graphs) {
                    plan.term_map_paths(map, json)?;
                }
                for o in &pom.objects {
                    match o {
                        ObjectMap::Term(map) => plan.term_map_paths(map, json)?,
                        ObjectMap::Join { parent, conditions } => {
                            if mapping.triples_maps[*parent].quoted_subject.is_some() {
                                return Err(RmlError::Unsupported(format!(
                                    "a referencing object map to {}, whose subjects are \
                                     quoted triples (use rml:quotedTriplesMap)",
                                    mapping.triples_maps[*parent].node
                                )));
                            }
                            plan.joined_paths(tm, *parent, conditions, json)?;
                        }
                        ObjectMap::Quoted(q) => {
                            plan.joined_paths(tm, q.map, &q.conditions, json)?
                        }
                    }
                }
            }
        }
        // Join indexes, once all sources are read.
        for (t, tm) in mapping.triples_maps.iter().enumerate() {
            for (p, pom) in tm.predicate_objects.iter().enumerate() {
                for (o, object) in pom.objects.iter().enumerate() {
                    if let ObjectMap::Join { parent, conditions } = object
                        && !conditions.is_empty()
                    {
                        let maps: Vec<&TermMap> = conditions.iter().map(|(_, p)| p).collect();
                        let index = plan.join_index(*parent, &maps)?;
                        plan.joins.insert((t, p, o), index);
                    }
                }
            }
        }
        Ok(plan)
    }

    /// Parses the references of the join conditions from `tm` to `parent` (a referencing or
    /// quoted map); without conditions both must read the same logical source.
    fn joined_paths(
        &mut self,
        tm: &crate::mapping::TriplesMap,
        parent: usize,
        conditions: &'m [(TermMap, TermMap)],
        json: bool,
    ) -> Result<(), RmlError> {
        let parent_map = &self.mapping.triples_maps[parent];
        if conditions.is_empty() && parent_map.source != tm.source {
            return Err(RmlError::Mapping(format!(
                "the join of {} to {} needs a join condition: their logical sources differ",
                tm.node, parent_map.node
            )));
        }
        let parent_json = parent_map.source.formulation == Formulation::JsonPath;
        for (child, parent) in conditions {
            self.term_map_paths(child, json)?;
            self.term_map_paths(parent, parent_json)?;
        }
        Ok(())
    }

    fn path(&mut self, p: &'m str) -> Result<(), RmlError> {
        if !self.paths.contains_key(p) {
            self.paths.insert(p, Path::parse(p)?);
        }
        Ok(())
    }

    /// Parses the JSONPath references of `map` (`json`), or none (CSV columns are names).
    fn term_map_paths(&mut self, map: &'m TermMap, json: bool) -> Result<(), RmlError> {
        if !json {
            return Ok(());
        }
        match &map.value {
            Value::Reference(r) => self.path(r)?,
            Value::Template(parts) => {
                for part in parts {
                    if let Part::Reference(r) = part {
                        self.path(r)?;
                    }
                }
            }
            Value::Constant(_) | Value::Fresh | Value::Function(_) => {}
        }
        for m in nested(map) {
            self.term_map_paths(m, json)?;
        }
        Ok(())
    }

    /// The number of records of the view `v`.
    pub(crate) fn view_records(&self, v: usize) -> usize {
        self.views[v].as_array().map_or(0, Vec::len)
    }

    /// The number of records a triples map with `source` reads.
    pub(crate) fn records_of(&self, source: &LogicalSource) -> usize {
        self.records(source).len()
    }

    fn records(&self, source: &LogicalSource) -> Vec<Rec<'_>> {
        static NULL: serde_json::Value = serde_json::Value::Null;
        if let Some(table) = self.tables.get(source) {
            return table
                .rows
                .iter()
                .map(|row| Rec {
                    value: &NULL,
                    row: Some((row, &table.columns)),
                    csv: true,
                    sql: false,
                    xml: false,
                    columns: true,
                })
                .collect();
        }
        let root = match source.source {
            SourceRef::View(v) => &self.views[v],
            _ => &self.sources[source],
        };
        if matches!(
            source.formulation,
            Formulation::Csv | Formulation::View | Formulation::Sql | Formulation::XPath
        ) {
            let serde_json::Value::Array(rows) = root else {
                unreachable!("CSV records are an array")
            };
            let sql = source.formulation == Formulation::Sql;
            let xml = source.formulation == Formulation::XPath;
            let columns = source.formulation == Formulation::Csv;
            return rows
                .iter()
                .map(|value| Rec {
                    value,
                    row: None,
                    csv: true,
                    sql,
                    xml,
                    columns,
                })
                .collect();
        }
        let mut out = Vec::new();
        self.paths[source.iterator.as_deref().unwrap_or("$")].select(root, &mut out);
        out.into_iter()
            .map(|value| Rec {
                value,
                row: None,
                csv: false,
                sql: false,
                xml: false,
                columns: false,
            })
            .collect()
    }

    /// The values a reference reads from a record, with the datatype the JSON value carries.
    /// A reference to an array or object is a data error: `[*]` selects the elements.
    fn values<'r>(
        &self,
        record: Rec<'r>,
        reference: &str,
    ) -> Result<Vec<(String, NamedNodeRef<'r>)>, RmlError> {
        if record.sql {
            return match column(record.value, reference) {
                None => Err(RmlError::Mapping(format!(
                    "the logical table has no column {reference}"
                ))),
                Some(serde_json::Value::Array(cell)) => match (&cell[0], &cell[1]) {
                    (serde_json::Value::String(l), serde_json::Value::String(d)) => {
                        Ok(vec![(l.clone(), NamedNodeRef::new_unchecked(d))])
                    }
                    _ => unreachable!("cells are [lexical, datatype]"),
                },
                Some(_) => Ok(Vec::new()),
            };
        }
        if record.xml {
            let Some(serde_json::Value::Array(values)) = record.value.get(reference) else {
                return Err(RmlError::Mapping(format!(
                    "the reference {reference} was not read from the XML source"
                )));
            };
            return Ok(values
                .iter()
                .filter_map(|v| v.as_str().map(|s| (s.to_owned(), xsd::STRING)))
                .collect());
        }
        if let Some((row, columns)) = record.row {
            let Some(&i) = columns.get(reference) else {
                return Err(RmlError::Mapping(format!(
                    "the reference {reference} names no column of the source"
                )));
            };
            return Ok(row
                .cell(i)
                .map(|c| (c.to_owned(), xsd::STRING))
                .into_iter()
                .collect());
        }
        let mut selected = Vec::new();
        if record.csv {
            match record.value.get(reference) {
                Some(v) => selected.push(v),
                None if !record.columns => {}
                None => {
                    return Err(RmlError::Mapping(format!(
                        "the reference {reference} names no column of the source"
                    )));
                }
            }
        } else {
            self.paths[reference].select(record.value, &mut selected);
        }
        let mut out = Vec::with_capacity(selected.len());
        for v in selected {
            match v {
                serde_json::Value::String(s) => out.push((s.clone(), xsd::STRING)),
                serde_json::Value::Number(n) => out.push((
                    n.to_string(),
                    if n.is_f64() {
                        xsd::DOUBLE
                    } else {
                        xsd::INTEGER
                    },
                )),
                serde_json::Value::Bool(b) => out.push((b.to_string(), xsd::BOOLEAN)),
                serde_json::Value::Null => {}
                serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                    return Err(RmlError::Data(format!(
                        "the reference {reference} gives a JSON array or object, not a value \
                         (select its elements with [*])"
                    )));
                }
            }
        }
        Ok(out)
    }

    /// The terms `map` makes from `record`, appended to `out`; `base` resolves relative IRIs.
    fn terms(
        &self,
        map: &TermMap,
        record: Rec<'_>,
        base: Option<&Iri<String>>,
        out: &mut Vec<Term>,
    ) -> Result<(), RmlError> {
        if let Some(condition) = &map.condition {
            let mut terms = Vec::new();
            self.terms(condition, record, base, &mut terms)?;
            if !terms
                .iter()
                .filter_map(crate::functions::text)
                .any(|t| crate::functions::truthy(&t))
            {
                return Ok(());
            }
        }
        if let Some(gather) = &map.gather {
            return self.gather(map, gather, record, base, out);
        }
        self.value_terms(map, record, base, out)
    }

    /// The heads of the collections or containers a gather map makes from `record`; the
    /// collections themselves wait in `pending` for the graphs of the triples using them.
    fn gather(
        &self,
        map: &TermMap,
        gather: &Gather,
        record: Rec<'_>,
        base: Option<&Iri<String>>,
        out: &mut Vec<Term>,
    ) -> Result<(), RmlError> {
        let mut values: Vec<Vec<Term>> = Vec::with_capacity(gather.items.len());
        for (i, item) in gather.items.iter().enumerate() {
            let mut terms = Vec::new();
            match item {
                ObjectMap::Term(m) => self.terms(m, record, base, &mut terms)?,
                ObjectMap::Quoted(_) => {
                    return Err(RmlError::Unsupported(
                        "quoted triples maps in gather maps".into(),
                    ));
                }
                ObjectMap::Join { parent, conditions } if conditions.is_empty() => {
                    let parent_map = &self.mapping.triples_maps[*parent];
                    self.terms(
                        &parent_map.subject,
                        record,
                        self.bases[*parent].as_ref(),
                        &mut terms,
                    )?;
                }
                ObjectMap::Join { parent, conditions } => {
                    let key = (std::ptr::from_ref(map) as usize, i);
                    if !self.gather_joins.borrow().contains_key(&key) {
                        let maps: Vec<&TermMap> = conditions.iter().map(|(_, p)| p).collect();
                        let index = self.join_index(*parent, &maps)?;
                        self.gather_joins.borrow_mut().insert(key, index);
                    }
                    let child: Vec<&TermMap> = conditions.iter().map(|(c, _)| c).collect();
                    let joins = self.gather_joins.borrow();
                    for k in self.keys(record, &child, base)? {
                        if let Some(found) = joins[&key].get(&k) {
                            terms.extend(found.iter().cloned());
                        }
                    }
                }
            }
            values.push(terms);
        }
        let combinations = if gather.cartesian {
            let count = values
                .iter()
                .try_fold(1usize, |n, v| n.checked_mul(v.len()))
                .filter(|&n| n <= MAX_COMBINATIONS);
            if count.is_none() {
                return Err(RmlError::Data(format!(
                    "the Cartesian product of a gather map makes more than \
                     {MAX_COMBINATIONS} collections for one record"
                )));
            }
            let mut combinations: Vec<Vec<Term>> = vec![Vec::new()];
            for v in &values {
                combinations = combinations
                    .iter()
                    .flat_map(|c| {
                        v.iter().map(move |t| {
                            let mut c = c.clone();
                            c.push(t.clone());
                            c
                        })
                    })
                    .collect();
            }
            combinations
        } else {
            vec![values.concat()]
        };
        let named = !matches!(map.value, Value::Fresh);
        for members in combinations {
            if members.is_empty() {
                if !gather.allow_empty {
                    continue;
                }
                if gather.kind == Collection::List {
                    out.push(rdf::NIL.into_owned().into());
                    continue;
                }
            }
            let mut heads = Vec::new();
            if named {
                self.value_terms(map, record, base, &mut heads)?;
            } else {
                heads.push(BlankNode::default().into());
            }
            let mut pending = self.pending.borrow_mut();
            for head in heads {
                pending.push(Pending {
                    head: head.clone(),
                    kind: gather.kind,
                    members: members.clone(),
                    named,
                    targets: map.targets.clone(),
                });
                out.push(head);
            }
        }
        Ok(())
    }

    /// The terms of `map`'s value (constant, reference, template or function) for `record`.
    fn value_terms(
        &self,
        map: &TermMap,
        record: Rec<'_>,
        base: Option<&Iri<String>>,
        out: &mut Vec<Term>,
    ) -> Result<(), RmlError> {
        match &map.value {
            Value::Function(call) => {
                let mut args: HashMap<&str, Vec<String>> = HashMap::new();
                let mut terms = Vec::new();
                for (parameter, input) in &call.inputs {
                    terms.clear();
                    self.terms(input, record, base, &mut terms)?;
                    args.entry(parameter.as_str())
                        .or_default()
                        .extend(terms.iter().filter_map(crate::functions::text));
                }
                for (lexical, natural) in call.body.call(&args)? {
                    if let Some(t) = self.make(map, lexical, Some(natural), record, base)? {
                        out.push(t);
                    }
                }
                Ok(())
            }
            Value::Constant(c) => {
                out.push(c.clone());
                Ok(())
            }
            Value::Fresh => {
                out.push(BlankNode::default().into());
                Ok(())
            }
            Value::Reference(r) => {
                for (lexical, natural) in self.values(record, r)? {
                    if let Some(t) = self.make(map, lexical, Some(natural), record, base)? {
                        out.push(t);
                    }
                }
                Ok(())
            }
            Value::Template(parts) => {
                let mut texts = vec![String::new()];
                for part in parts {
                    match part {
                        Part::Text(t) => texts.iter_mut().for_each(|s| s.push_str(t)),
                        Part::Reference(r) => {
                            let values = self.values(record, r)?;
                            if values.is_empty() {
                                return Ok(());
                            }
                            let mut next = Vec::with_capacity(texts.len() * values.len());
                            for s in &texts {
                                for (v, _) in &values {
                                    let mut s = s.clone();
                                    match (map.term_type, map.encoding) {
                                        (TermType::Iri, Encoding::Iri) => iri_safe(v, &mut s, true),
                                        (TermType::Iri, Encoding::Uri) => {
                                            iri_safe(v, &mut s, false)
                                        }
                                        _ => s.push_str(v),
                                    }
                                    next.push(s);
                                }
                            }
                            texts = next;
                        }
                    }
                }
                for s in texts {
                    if let Some(t) = self.make(map, s, None, record, base)? {
                        out.push(t);
                    }
                }
                Ok(())
            }
        }
    }

    /// The term of `map` with this text; an invalid IRI or language tag is a data error.
    fn make(
        &self,
        map: &TermMap,
        lexical: String,
        natural: Option<NamedNodeRef<'_>>,
        record: Rec<'_>,
        base: Option<&Iri<String>>,
    ) -> Result<Option<Term>, RmlError> {
        Ok(Some(match map.term_type {
            TermType::Iri => {
                let iri = match Iri::parse(lexical.as_str()) {
                    Ok(_) => lexical,
                    Err(_) => match base.map(|b| b.resolve(&lexical)) {
                        Some(Ok(iri)) => iri.into_inner(),
                        _ => {
                            return Err(RmlError::Data(format!("{lexical} is not a valid IRI")));
                        }
                    },
                };
                if map.encoding == Encoding::Unsafe {
                    NamedNode::new_unchecked(iri).into()
                } else {
                    NamedNode::new(iri)
                        .map_err(|e| RmlError::Data(e.to_string()))?
                        .into()
                }
            }
            TermType::BlankNode => {
                let mut label = String::from("b");
                for b in lexical.bytes() {
                    if b.is_ascii_alphanumeric() {
                        label.push(b as char);
                    } else {
                        label.push_str(&format!("_{b:02x}"));
                    }
                }
                BlankNode::new_unchecked(label).into()
            }
            TermType::Literal => {
                let language = match &map.language_map {
                    Some(m) => match self.first(m, record, base)? {
                        Some(Term::Literal(l)) => Some(l.value().to_owned()),
                        _ => None,
                    },
                    None => map.language.clone(),
                };
                let datatype = match &map.datatype_map {
                    Some(m) => match self.first(m, record, base)? {
                        Some(Term::NamedNode(d)) => Some(d),
                        _ => None,
                    },
                    None => map.datatype.clone(),
                };
                if let Some(lang) = language {
                    Literal::new_language_tagged_literal(lexical, &lang).map_err(|_| {
                        RmlError::Data(format!("{lang} is not a valid language tag"))
                    })?
                } else if let Some(dt) = datatype {
                    Literal::new_typed_literal(lexical, dt)
                } else {
                    match natural {
                        Some(dt) if dt != xsd::STRING => Literal::new_typed_literal(lexical, dt),
                        _ => Literal::new_simple_literal(lexical),
                    }
                }
                .into()
            }
        }))
    }

    /// The first term `map` makes from `record`.
    fn first(
        &self,
        map: &TermMap,
        record: Rec<'_>,
        base: Option<&Iri<String>>,
    ) -> Result<Option<Term>, RmlError> {
        let mut terms = Vec::new();
        self.terms(map, record, base, &mut terms)?;
        Ok(terms.into_iter().next())
    }

    /// The graphs `maps` make, each with the logical targets of the maps that made it.
    fn graphs(
        &self,
        maps: &[&TermMap],
        record: Rec<'_>,
        base: Option<&Iri<String>>,
    ) -> Result<Vec<(GraphName, Vec<usize>)>, RmlError> {
        let mut graphs: Vec<(GraphName, Vec<usize>)> = Vec::new();
        let mut terms = Vec::new();
        for map in maps {
            terms.clear();
            self.terms(map, record, base, &mut terms)?;
            for t in terms.drain(..) {
                let g = match t {
                    Term::NamedNode(n) if is_default_graph(n.as_ref()) => GraphName::DefaultGraph,
                    Term::NamedNode(n) => n.into(),
                    _ => continue,
                };
                match graphs.iter_mut().find(|(x, _)| *x == g) {
                    Some((_, targets)) => targets.extend(&map.targets),
                    None => graphs.push((g, map.targets.clone())),
                }
            }
        }
        if graphs.is_empty() {
            graphs.push((GraphName::DefaultGraph, Vec::new()));
        }
        Ok(graphs)
    }

    /// Join keys: one text per join expression, every combination of their values.
    fn keys(
        &self,
        record: Rec<'_>,
        maps: &[&TermMap],
        base: Option<&Iri<String>>,
    ) -> Result<Vec<Vec<String>>, RmlError> {
        let mut keys = vec![Vec::new()];
        let mut terms = Vec::new();
        for map in maps {
            terms.clear();
            self.terms(map, record, base, &mut terms)?;
            let mut next = Vec::with_capacity(keys.len() * terms.len());
            for k in &keys {
                for t in terms.iter().filter_map(join_text) {
                    let mut k = k.clone();
                    k.push(t);
                    next.push(k);
                }
            }
            keys = next;
        }
        Ok(keys)
    }

    fn join_index(&self, parent: usize, maps: &[&TermMap]) -> Result<JoinIndex, RmlError> {
        let tm = &self.mapping.triples_maps[parent];
        let base = self.bases[parent].as_ref();
        let mut index = JoinIndex::new();
        let mut subjects = Vec::new();
        // A gather map as the parent's subject adds collections to `pending`; they are the
        // parent's, not those of the record being mapped, so keep them out of it.
        let saved = std::mem::take(&mut *self.pending.borrow_mut());
        let result = (|| {
            for record in self.records(&tm.source) {
                let keys = self.keys(record, maps, base)?;
                if keys.is_empty() {
                    continue;
                }
                subjects.clear();
                self.terms(&tm.subject, record, base, &mut subjects)?;
                for k in keys {
                    index.entry(k).or_default().extend(subjects.iter().cloned());
                }
            }
            Ok(())
        })();
        *self.pending.borrow_mut() = saved;
        result.map(|()| index)
    }

    /// Runs the asserted triples maps (all but RML-star's non-asserted ones).
    fn run(&self, sink: &mut dyn FnMut(Quad, &[usize])) -> Result<usize, RmlError> {
        let mut n = 0;
        for (t, tm) in self.mapping.triples_maps.iter().enumerate() {
            if tm.asserted {
                n += self.run_map(t, sink)?;
            }
        }
        Ok(n)
    }

    /// Runs the triples map `t`, giving its quads, with their logical targets, to `sink`;
    /// returns how many it made.
    pub(crate) fn run_map(
        &self,
        t: usize,
        sink: &mut dyn FnMut(Quad, &[usize]),
    ) -> Result<usize, RmlError> {
        self.run_records(t, &mut |_, _, q, targets| sink(q, targets))
    }

    /// Runs the quoted triples maps `t` uses, once each, before `t` itself.
    fn prepare_quoted(&self, t: usize) -> Result<(), RmlError> {
        let tm = &self.mapping.triples_maps[t];
        let mut quoted: Vec<((usize, usize, usize), &Quoted)> = Vec::new();
        if let Some(q) = &tm.quoted_subject {
            quoted.push(((t, usize::MAX, 0), q));
        }
        for (p, pom) in tm.predicate_objects.iter().enumerate() {
            for (o, object) in pom.objects.iter().enumerate() {
                if let ObjectMap::Quoted(q) = object {
                    quoted.push(((t, p, o), q));
                }
            }
        }
        for (key, q) in quoted {
            if !self.quoted.borrow().contains_key(&q.map) {
                if !self.quoting.borrow_mut().insert(q.map) {
                    return Err(RmlError::Mapping(format!(
                        "the quoted triples maps of {} quote themselves",
                        self.mapping.triples_maps[q.map].node
                    )));
                }
                let made = self.quote(q.map);
                self.quoting.borrow_mut().remove(&q.map);
                self.quoted.borrow_mut().insert(q.map, Rc::new(made?));
            }
            if !q.conditions.is_empty() && !self.quoted_joins.borrow().contains_key(&key) {
                let quoted = &self.mapping.triples_maps[q.map];
                let base = self.bases[q.map].as_ref();
                let maps: Vec<&TermMap> = q.conditions.iter().map(|(_, p)| p).collect();
                let mut index = RecordIndex::new();
                for (i, record) in self.records(&quoted.source).into_iter().enumerate() {
                    for k in self.keys(record, &maps, base)? {
                        index.entry(k).or_default().push(i);
                    }
                }
                self.quoted_joins.borrow_mut().insert(key, index);
            }
        }
        Ok(())
    }

    /// The triples of the triples map `m`, per record, named by their reifiers.
    fn quote(&self, m: usize) -> Result<Vec<Vec<QuotedTriple>>, RmlError> {
        let records = self.records(&self.mapping.triples_maps[m].source).len();
        let mut triples: Vec<Vec<Triple>> = vec![Vec::new(); records];
        let mut sides: Vec<Vec<Triple>> = vec![Vec::new(); records];
        self.run_records(m, &mut |record, side, q, _| {
            if let Some(r) = record.filter(|r| *r < records) {
                let t = Triple::new(q.subject, q.predicate, q.object);
                if side {
                    sides[r].push(t);
                } else if !triples[r].contains(&t) {
                    triples[r].push(t);
                }
            }
        })?;
        Ok(triples
            .into_iter()
            .zip(sides)
            .map(|(triples, sides)| {
                triples
                    .into_iter()
                    .map(|t| {
                        let r = reifier(&t);
                        // The reifiers the triple uses, and theirs in turn.
                        let mut used: Vec<Term> = vec![t.subject.clone().into(), t.object.clone()];
                        let mut stated: Vec<Triple> = Vec::new();
                        while let Some(term) = used.pop() {
                            for side in sides
                                .iter()
                                .filter(|s| Term::from(s.subject.clone()) == term)
                            {
                                if !stated.contains(side) {
                                    stated.push(side.clone());
                                    if let Term::Triple(inner) = &side.object {
                                        used.push(inner.subject.clone().into());
                                        used.push(inner.object.clone());
                                    }
                                }
                            }
                        }
                        let own = Triple::new(r.clone(), rdf::REIFIES, Term::Triple(Box::new(t)));
                        stated.insert(0, own);
                        QuotedTriple {
                            reifier: r,
                            sides: stated.into(),
                        }
                    })
                    .collect()
            })
            .collect())
    }

    /// The quoted triples `q` gives for `record` (its `index` in the source) of the triples
    /// map `t`; `key` names its join index.
    fn quoted_for(
        &self,
        key: (usize, usize, usize),
        q: &Quoted,
        index: usize,
        record: Rec<'_>,
        base: Option<&Iri<String>>,
    ) -> Result<Vec<QuotedTriple>, RmlError> {
        let made = Rc::clone(&self.quoted.borrow()[&q.map]);
        if q.conditions.is_empty() {
            return Ok(made.get(index).cloned().unwrap_or_default());
        }
        let child: Vec<&TermMap> = q.conditions.iter().map(|(c, _)| c).collect();
        let joins = self.quoted_joins.borrow();
        let mut out: Vec<QuotedTriple> = Vec::new();
        for k in self.keys(record, &child, base)? {
            for &i in joins[&key].get(&k).into_iter().flatten() {
                for qt in &made[i] {
                    if !out.iter().any(|o| o.reifier == qt.reifier) {
                        out.push(qt.clone());
                    }
                }
            }
        }
        Ok(out)
    }

    /// [`Plan::run_map`], giving each quad with its record's index and whether it states a
    /// reifier.
    fn run_records(&self, t: usize, sink: &mut Sink<'_>) -> Result<usize, RmlError> {
        self.prepare_quoted(t)?;
        let tm = &self.mapping.triples_maps[t];
        let current: std::cell::Cell<Option<usize>> = std::cell::Cell::new(None);
        // The triples stating the reifiers of the record's quoted subjects and objects.
        let mut sides: HashMap<Term, Rc<[Triple]>> = HashMap::new();
        let no_sides: HashMap<Term, Rc<[Triple]>> = HashMap::new();
        let mut n = 0;
        let mut targets: Vec<usize> = Vec::new();
        // Named blank node heads of collections, which get a node of their own per graph.
        let mut heads: HashSet<BlankNode> = HashSet::new();
        let mut collections = Collections::default();
        self.pending.borrow_mut().clear();
        let mut emit = |s: &NamedOrBlankNode,
                        p: &NamedNode,
                        o: &Term,
                        g: &GraphName,
                        parts: [&[usize]; 4],
                        heads: &HashSet<BlankNode>,
                        sides: &HashMap<Term, Rc<[Triple]>>| {
            targets.clear();
            for part in parts {
                targets.extend_from_slice(part);
            }
            targets.sort_unstable();
            targets.dedup();
            let record = current.get();
            // Only maps with quoted triples have reifiers to state.
            let quoted: &[&Term] = if sides.is_empty() {
                &[]
            } else {
                &[&Term::from(s.clone()), o]
            };
            for term in quoted {
                for side in sides.get(term).into_iter().flat_map(|s| s.iter()) {
                    sink(
                        record,
                        true,
                        Quad::new(
                            side.subject.clone(),
                            side.predicate.clone(),
                            side.object.clone(),
                            g.clone(),
                        ),
                        &targets,
                    );
                }
            }
            let s = match in_graph(&s.clone().into(), g, heads) {
                Term::NamedNode(n) => NamedOrBlankNode::from(n),
                Term::BlankNode(b) => b.into(),
                _ => s.clone(),
            };
            sink(
                record,
                false,
                Quad::new(s, p.clone(), in_graph(o, g, heads), g.clone()),
                &targets,
            );
            n += 1;
        };
        // Pending collections made since `from` belong to `graphs`.
        let settle = |from: usize,
                      graphs: &[(GraphName, Vec<usize>)],
                      heads: &mut HashSet<BlankNode>,
                      collections: &mut Collections| {
            for p in self.pending.borrow_mut().drain(from..) {
                if p.named
                    && let Term::BlankNode(b) = &p.head
                {
                    heads.insert(b.clone());
                }
                for (g, _) in graphs {
                    collections.add(&p, g);
                }
            }
        };
        let mut terms = Vec::new();
        {
            let base = self.bases[t].as_ref();
            let subject_graphs: Vec<&TermMap> = tm.graphs.iter().collect();
            for (index, record) in self.records(&tm.source).into_iter().enumerate() {
                current.set(Some(index));
                terms.clear();
                sides.clear();
                self.pending.borrow_mut().clear();
                match &tm.quoted_subject {
                    Some(q) => {
                        for qt in self.quoted_for((t, usize::MAX, 0), q, index, record, base)? {
                            let r = Term::from(qt.reifier);
                            sides.insert(r.clone(), qt.sides);
                            terms.push(r);
                        }
                    }
                    None => self.terms(&tm.subject, record, base, &mut terms)?,
                }
                let subjects: Vec<NamedOrBlankNode> = terms
                    .drain(..)
                    .filter_map(|t| match t {
                        Term::NamedNode(n) => Some(n.into()),
                        Term::BlankNode(b) => Some(b.into()),
                        _ => None,
                    })
                    .collect();
                if subjects.is_empty() {
                    continue;
                }
                let graphs = self.graphs(&subject_graphs, record, base)?;
                settle(0, &graphs, &mut heads, &mut collections);
                let subject_targets = tm.subject.targets.as_slice();
                for s in &subjects {
                    for c in &tm.classes {
                        let c = Term::from(c.clone());
                        for (g, gt) in &graphs {
                            emit(
                                s,
                                &rdf::TYPE.into_owned(),
                                &c,
                                g,
                                [subject_targets, gt, &[], &[]],
                                &heads,
                                &sides,
                            );
                        }
                    }
                }
                for (p, pom) in tm.predicate_objects.iter().enumerate() {
                    let mut predicates = Vec::new();
                    for map in &pom.predicates {
                        self.terms(map, record, base, &mut terms)?;
                        predicates.extend(terms.drain(..).filter_map(|t| match t {
                            Term::NamedNode(n) => Some((n, map.targets.as_slice())),
                            _ => None,
                        }));
                    }
                    if predicates.is_empty() {
                        continue;
                    }
                    self.pending.borrow_mut().clear();
                    let mut objects = Vec::new();
                    // Where each object map's objects end in `objects`, with its targets.
                    let mut object_targets: Vec<(usize, &[usize])> = Vec::new();
                    for (o, object) in pom.objects.iter().enumerate() {
                        match object {
                            ObjectMap::Term(map) => {
                                self.terms(map, record, base, &mut objects)?;
                                object_targets.push((objects.len(), map.targets.as_slice()));
                            }
                            ObjectMap::Join { parent, conditions } if conditions.is_empty() => {
                                let parent_map = &self.mapping.triples_maps[*parent];
                                self.terms(
                                    &parent_map.subject,
                                    record,
                                    self.bases[*parent].as_ref(),
                                    &mut objects,
                                )?;
                            }
                            ObjectMap::Join { conditions, .. } => {
                                let child: Vec<&TermMap> =
                                    conditions.iter().map(|(c, _)| c).collect();
                                let index = &self.joins[&(t, p, o)];
                                for k in self.keys(record, &child, base)? {
                                    if let Some(found) = index.get(&k) {
                                        objects.extend(found.iter().cloned());
                                    }
                                }
                            }
                            ObjectMap::Quoted(q) => {
                                for qt in self.quoted_for((t, p, o), q, index, record, base)? {
                                    let r = Term::from(qt.reifier);
                                    sides.insert(r.clone(), qt.sides);
                                    objects.push(r);
                                }
                            }
                        }
                        if !matches!(object, ObjectMap::Term(_)) {
                            object_targets.push((objects.len(), &[]));
                        }
                    }
                    if objects.is_empty() {
                        continue;
                    }
                    let graphs = if pom.graphs.is_empty() {
                        graphs.clone()
                    } else {
                        let maps: Vec<&TermMap> = tm.graphs.iter().chain(&pom.graphs).collect();
                        self.graphs(&maps, record, base)?
                    };
                    settle(0, &graphs, &mut heads, &mut collections);
                    for s in &subjects {
                        for (p, pt) in &predicates {
                            for (i, o) in objects.iter().enumerate() {
                                let ot = object_targets
                                    .iter()
                                    .find(|(end, _)| i < *end)
                                    .map_or(&[][..], |(_, t)| *t);
                                for (g, gt) in &graphs {
                                    emit(s, p, o, g, [subject_targets, pt, ot, gt], &heads, &sides);
                                }
                            }
                        }
                    }
                }
            }
        }
        // The collections and containers, once every record has added its members.
        current.set(None);
        for (head, g, kind, members, targets) in std::mem::take(&mut collections.list) {
            let head = in_graph(&head, &g, &heads);
            let mut node: NamedOrBlankNode = match head {
                Term::NamedNode(n) => n.into(),
                Term::BlankNode(b) => b.into(),
                _ => continue,
            };
            if kind == Collection::List {
                for (i, m) in members.iter().enumerate() {
                    emit(
                        &node,
                        &rdf::FIRST.into_owned(),
                        m,
                        &g,
                        [&targets, &[], &[], &[]],
                        &heads,
                        &no_sides,
                    );
                    let rest: Term = if i + 1 == members.len() {
                        rdf::NIL.into_owned().into()
                    } else {
                        BlankNode::default().into()
                    };
                    emit(
                        &node,
                        &rdf::REST.into_owned(),
                        &rest,
                        &g,
                        [&targets, &[], &[], &[]],
                        &heads,
                        &no_sides,
                    );
                    if let Term::BlankNode(b) = rest {
                        node = b.into();
                    }
                }
            } else {
                let class = match kind {
                    Collection::Bag => "Bag",
                    Collection::Seq => "Seq",
                    _ => "Alt",
                };
                let class: Term = NamedNode::new_unchecked(format!("{}{class}", RDF_NS)).into();
                emit(
                    &node,
                    &rdf::TYPE.into_owned(),
                    &class,
                    &g,
                    [&targets, &[], &[], &[]],
                    &heads,
                    &no_sides,
                );
                for (i, m) in members.iter().enumerate() {
                    let p = NamedNode::new_unchecked(format!("{}_{}", RDF_NS, i + 1));
                    emit(
                        &node,
                        &p,
                        m,
                        &g,
                        [&targets, &[], &[], &[]],
                        &heads,
                        &no_sides,
                    );
                }
            }
        }
        Ok(n)
    }
}

/// The maps `tm` joins: the parents of its referencing object maps and its quoted triples
/// maps, with their join conditions.
fn quoted_and_joins(
    tm: &crate::mapping::TriplesMap,
) -> impl Iterator<Item = (usize, &[(TermMap, TermMap)])> {
    let objects = tm.predicate_objects.iter().flat_map(|pom| {
        pom.objects.iter().filter_map(|o| match o {
            ObjectMap::Join { parent, conditions } => Some((*parent, conditions.as_slice())),
            ObjectMap::Quoted(q) => Some((q.map, q.conditions.as_slice())),
            ObjectMap::Term(_) => None,
        })
    });
    tm.quoted_subject
        .iter()
        .map(|q| (q.map, q.conditions.as_slice()))
        .chain(objects)
}

/// The cell of a table row a column reference reads: by its name, a delimited identifier
/// (`"Name"`) by the name inside the quotes, and else by the name in lower case, as
/// PostgreSQL folds undelimited identifiers (`ID` reads a column `id`).
fn column<'a>(row: &'a serde_json::Value, reference: &str) -> Option<&'a serde_json::Value> {
    let row = row.as_object()?;
    if let Some(v) = row.get(reference) {
        return Some(v);
    }
    match reference
        .strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
    {
        Some(inner) => row.get(&inner.replace("\"\"", "\"")),
        None => row.get(&reference.to_lowercase()),
    }
}

fn describe(source: &LogicalSource) -> String {
    match &source.source {
        SourceRef::Path(p) | SourceRef::Table(p) | SourceRef::Query(p) => p.clone(),
        SourceRef::View(v) => format!("logical view {v}"),
        SourceRef::Empty => "no logical source".to_owned(),
    }
}

/// Every reference read from records of `source`: by the triples maps on it, and by the
/// join conditions on their parent side.
fn source_references(mapping: &Mapping, source: &LogicalSource) -> Vec<String> {
    fn add(map: &TermMap, out: &mut Vec<String>) {
        match &map.value {
            Value::Reference(r) => out.push(r.clone()),
            Value::Template(parts) => {
                for part in parts {
                    if let Part::Reference(r) = part {
                        out.push(r.clone());
                    }
                }
            }
            Value::Constant(_) | Value::Fresh | Value::Function(_) => {}
        }
        for m in nested(map) {
            add(m, out);
        }
    }
    let mut out = Vec::new();
    for tm in &mapping.triples_maps {
        let own = tm.source == *source;
        if own {
            add(&tm.subject, &mut out);
            tm.graphs.iter().for_each(|m| add(m, &mut out));
        }
        for pom in &tm.predicate_objects {
            if own {
                for m in pom.predicates.iter().chain(&pom.graphs) {
                    add(m, &mut out);
                }
            }
            for o in &pom.objects {
                match o {
                    ObjectMap::Term(m) if own => add(m, &mut out),
                    ObjectMap::Term(_) => {}
                    ObjectMap::Join { .. } | ObjectMap::Quoted(_) => {}
                }
            }
        }
        for (parent, conditions) in quoted_and_joins(tm) {
            let parent_own = mapping.triples_maps[parent].source == *source;
            for (child, parent) in conditions {
                if own {
                    add(child, &mut out);
                }
                if parent_own {
                    add(parent, &mut out);
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The records of an XML source as objects of the values of each reference (arrays of
/// strings), from `iterator` (the root element by default).
fn xml_records(
    source: &LogicalSource,
    bytes: &[u8],
    references: &[String],
) -> Result<serde_json::Value, FxError> {
    let iterator = source.iterator.as_deref().unwrap_or("/*");
    let refs: Vec<&str> = references.iter().map(String::as_str).collect();
    let records = tripletrail_fx::sources::xml_records(bytes, iterator, &refs)?;
    Ok(serde_json::Value::Array(
        records
            .into_iter()
            .map(|values| {
                references
                    .iter()
                    .cloned()
                    .zip(values.into_iter().map(serde_json::Value::from))
                    .collect::<serde_json::Map<_, _>>()
                    .into()
            })
            .collect(),
    ))
}

/// A source's bytes, decompressed and decoded to UTF-8.
fn source_bytes(source: &LogicalSource, loader: &dyn Loader) -> Result<Vec<u8>, RmlError> {
    let source_error = |message: String| RmlError::Source {
        location: describe(source),
        message,
    };
    let bytes = loader.load(source)?;
    let bytes =
        crate::io::decompress(bytes, source.io.compression.as_deref()).map_err(source_error)?;
    crate::io::decode(bytes, source.io.encoding.as_deref()).map_err(source_error)
}

/// Reads and parses a source: a JSON document, or CSV rows, table rows or XML records as an
/// array of objects. An XML source is read for `references` only, which logical views do not
/// give yet.
fn read_source(
    source: &LogicalSource,
    loader: &dyn Loader,
    references: Option<&[String]>,
) -> Result<serde_json::Value, RmlError> {
    match &source.formulation {
        Formulation::JsonPath | Formulation::Csv => {}
        Formulation::XPath if references.is_none() => {
            return Err(RmlError::Unsupported("XML sources in logical views".into()));
        }
        Formulation::XPath => {}
        Formulation::Sql => return Ok(table_records(loader.table(source)?)),
        f => {
            return Err(RmlError::Unsupported(format!(
                "reference formulation {f:?}"
            )));
        }
    }
    let source_error = |message: String| RmlError::Source {
        location: describe(source),
        message,
    };
    if source.source == SourceRef::Empty {
        return Ok(serde_json::json!([{}]));
    }
    let bytes = source_bytes(source, loader)?;
    let nulls = source.io.nulls.as_deref().unwrap_or_default();
    if source.formulation == Formulation::Csv {
        csv_records(source, &bytes).map_err(|e| source_error(e.to_string()))
    } else if let (Formulation::XPath, Some(references)) = (&source.formulation, references) {
        let mut records =
            xml_records(source, &bytes, references).map_err(|e| source_error(e.to_string()))?;
        crate::io::remove_nulls(&mut records, nulls, true);
        Ok(records)
    } else {
        let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
        let mut value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|e| source_error(e.to_string()))?;
        crate::io::remove_nulls(&mut value, nulls, false);
        Ok(value)
    }
}

/// Appends `value` with every character other than `iunreserved` ones percent-encoded, as
/// R2RML and RML do to values put in IRI templates; without `keep_ucschar`, only the ASCII
/// `unreserved` ones are kept (`rml:URI`).
fn iri_safe(value: &str, out: &mut String, keep_ucschar: bool) {
    for c in value.chars() {
        if c.is_ascii_alphanumeric()
            || matches!(c, '-' | '.' | '_' | '~')
            || (keep_ucschar && is_ucschar(c))
        {
            out.push(c);
        } else {
            let mut buf = [0; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
}

fn is_ucschar(c: char) -> bool {
    matches!(c as u32,
        0xA0..=0xD7FF | 0xF900..=0xFDCF | 0xFDF0..=0xFFEF
        | 0x10000..=0x1FFFD | 0x20000..=0x2FFFD | 0x30000..=0x3FFFD
        | 0x40000..=0x4FFFD | 0x50000..=0x5FFFD | 0x60000..=0x6FFFD
        | 0x70000..=0x7FFFD | 0x80000..=0x8FFFD | 0x90000..=0x9FFFD
        | 0xA0000..=0xAFFFD | 0xB0000..=0xBFFFD | 0xC0000..=0xCFFFD
        | 0xD0000..=0xDFFFD | 0xE1000..=0xEFFFD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iri_safe_encodes_like_r2rml() {
        let mut s = String::new();
        iri_safe("a b/c?d#é~-._", &mut s, true);
        assert_eq!(s, "a%20b%2Fc%3Fd%23é~-._");
        s.clear();
        iri_safe("Zoë", &mut s, false);
        assert_eq!(s, "Zo%C3%AB");
    }
}
