//! Reading a mapping document into the plan. The R2RML, legacy RML and new RML vocabularies
//! are read alike: a property is looked up by its local name in each namespace, so
//! `rr:subjectMap`, `rml:subjectMap` (either RML) and mixes of them all work.

use crate::RmlError;
use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::vocab::rdf;
use oxigraph::model::{
    Graph, Literal, NamedNode, NamedNodeRef, NamedOrBlankNode, NamedOrBlankNodeRef, Term, TermRef,
};
use std::cell::RefCell;
use std::collections::HashMap;

const R2RML: &str = "http://www.w3.org/ns/r2rml#";
const RML_LEGACY: &str = "http://semweb.mmlab.be/ns/rml#";
const RML: &str = "http://w3id.org/rml/";
const NAMESPACES: [&str; 3] = [R2RML, RML_LEGACY, RML];
const CSVW_URL: &str = "http://www.w3.org/ns/csvw#url";
const D2RQ: &str = "http://www.wiwiss.fu-berlin.de/suhl/bizer/D2RQ/0.1#";

/// A mapping document: its triples maps, in a stable order.
#[derive(Debug, Clone)]
pub struct Mapping {
    pub triples_maps: Vec<TriplesMap>,
    /// The logical views (RML-LV) the triples maps and other views read, by
    /// [`SourceRef::View`] index. A view comes after the views it reads.
    pub views: Vec<LogicalView>,
    /// Relative IRIs made by references and templates are resolved against it.
    pub base_iri: Option<String>,
    /// The logical targets (RML-IO) term maps send their quads to, by index in
    /// [`TermMap::targets`].
    pub targets: Vec<LogicalTarget>,
}

/// A logical target (RML-IO): a file the quads of the term maps naming it are written to,
/// instead of the default output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalTarget {
    pub node: NamedOrBlankNode,
    /// The file, as written in the mapping (relative to the working directory).
    pub path: String,
    /// The local name of the `formats:` serialization (`N-Quads` when not given).
    pub serialization: String,
    /// `gzip`, `zip`, `targz` or `tarxz`.
    pub compression: Option<String>,
    /// `UTF-8` (the default) or `UTF-16`.
    pub encoding: Option<String>,
}

/// A triples map: for each record of its logical source, a subject, its classes and the
/// predicate-object pairs.
#[derive(Debug, Clone)]
pub struct TriplesMap {
    /// The node of the triples map in the mapping document.
    pub node: NamedOrBlankNode,
    pub source: LogicalSource,
    pub subject: TermMap,
    pub classes: Vec<NamedNode>,
    /// Graphs of the subject map: they apply to every triple of the map.
    pub graphs: Vec<TermMap>,
    pub predicate_objects: Vec<PredicateObjectMap>,
    /// `rml:baseIRI`: relative IRIs made by this map resolve against it rather than against
    /// [`Mapping::base_iri`].
    pub base_iri: Option<String>,
    /// Whether its triples are written: false for an `rml:NonAssertedTriplesMap` (RML-star),
    /// whose triples are only quoted by other maps.
    pub asserted: bool,
    /// `rml:quotedTriplesMap` on the subject map (RML-star): the subjects are the triples of
    /// that map, each named by its reifier ([`Quoted`]); `subject` is then unused.
    pub quoted_subject: Option<Quoted>,
}

/// A quoted triples map (RML-star): the triples another map (by index in
/// [`Mapping::triples_maps`]) makes from the records joining this one, as for a referencing
/// object map. TripleTrail stores RDF 1.2, where a triple term is only an object, so each quoted
/// triple is named by a reifier, a blank node `_:r` given by the triple, with
/// `_:r rdf:reifies <<( s p o )>>` written next to the triples that use it: RDF 1.2's reading
/// of `<< s p o >>` in Turtle.
#[derive(Debug, Clone)]
pub struct Quoted {
    pub map: usize,
    pub conditions: Vec<(TermMap, TermMap)>,
}

/// Where records come from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LogicalSource {
    pub source: SourceRef,
    pub formulation: Formulation,
    /// Selects the records (JSONPath, XPath); `None` for a CSV row or a table row.
    pub iterator: Option<String>,
    /// The database of a table or query, when the mapping names one (`d2rq:Database`). An
    /// R2RML mapping names none: the [`Loader`](crate::Loader) connects to its default.
    pub database: Option<Database>,
    /// How the source's bytes are read (RML-IO).
    pub io: SourceIo,
}

/// How a file source is read: RML-IO's `rml:compression`, `rml:encoding` and `rml:null`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct SourceIo {
    /// `rml:compression`, by local name: `gzip`, `zip`, `targz`, `tarxz` (`none` is `None`).
    pub compression: Option<String>,
    /// `rml:encoding`, by local name: `UTF-8` (the default) or `UTF-16`.
    pub encoding: Option<String>,
    /// `rml:null`: values read as no value. `None` when the source gives none: then an empty
    /// CSV cell is no value in legacy RML (as RMLMapper does) and an empty string in RML-IO.
    pub nulls: Option<Vec<String>>,
    /// The source is described in the RML-IO vocabulary (`http://w3id.org/rml/`), whose
    /// rules are stricter: a CSV row must have as many cells as the header.
    pub rml_io: bool,
}

/// A database a mapping connects to: a `d2rq:Database`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Database {
    /// `d2rq:jdbcDSN`: `jdbc:postgresql://host:port/name`, or a PostgreSQL connection URL.
    pub dsn: String,
    /// `d2rq:jdbcDriver`, such as `org.postgresql.Driver`.
    pub driver: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("dsn", &self.dsn)
            .field("driver", &self.driver)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SourceRef {
    /// A file, as written in the mapping (`rml:source "x.json"`, `rml:path`, `csvw:url`).
    Path(String),
    /// `rr:tableName`.
    Table(String),
    /// `rr:sqlQuery`.
    Query(String),
    /// A logical view, by index in [`Mapping::views`].
    View(usize),
    /// No logical source (a triples map of constants and functions, RML-FNML): one empty
    /// record.
    Empty,
}

/// A logical view (RML-LV): records made from another logical source or view by naming
/// fields of its records, optionally joined with other views. Its records are referenced by
/// field name: `name`, `item.type`, and the index keys `#` and `name.#`.
#[derive(Debug, Clone)]
pub struct LogicalView {
    pub node: NamedOrBlankNode,
    /// `rml:viewOn`: a logical source or (as [`SourceRef::View`]) a view.
    pub on: LogicalSource,
    pub fields: Vec<Field>,
    pub joins: Vec<ViewJoin>,
}

/// A field of a logical view: named records made from its parent's records.
#[derive(Debug, Clone)]
pub struct Field {
    /// The declared name; the absolute name is the parent field's, a dot and this one.
    pub name: String,
    pub kind: FieldKind,
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone)]
pub enum FieldKind {
    /// The values of an expression (reference, template or constant) on the parent record.
    Expression(TermMap),
    /// The records an iterator selects from the parent record, read with `formulation`
    /// (the parent's when `None`): a CSV text gives its rows, JSON text is parsed.
    Iterable {
        formulation: Option<Formulation>,
        iterator: Option<String>,
    },
}

/// `rml:leftJoin` / `rml:innerJoin`: adds fields of the records of another view that match
/// on every condition (`(child, parent)` expressions, compared as text).
#[derive(Debug, Clone)]
pub struct ViewJoin {
    pub inner: bool,
    /// The parent view, by index in [`Mapping::views`].
    pub parent: usize,
    pub conditions: Vec<(TermMap, TermMap)>,
    /// Expression fields evaluated on the parent's matching record.
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Formulation {
    JsonPath,
    Csv,
    XPath,
    Sql,
    /// The records of a logical view: references are field names.
    View,
    Other(String),
}

fn formulation(f: NamedNodeRef<'_>) -> Formulation {
    match f.as_str().rsplit(['#', '/']).next().unwrap_or_default() {
        "JSONPath" => Formulation::JsonPath,
        "CSV" => Formulation::Csv,
        "XPath" => Formulation::XPath,
        "SQL2008" | "SQL2008Table" | "SQL2008Query" => Formulation::Sql,
        _ => Formulation::Other(f.as_str().to_owned()),
    }
}

#[derive(Debug, Clone)]
pub struct PredicateObjectMap {
    pub predicates: Vec<TermMap>,
    pub objects: Vec<ObjectMap>,
    pub graphs: Vec<TermMap>,
}

#[derive(Debug, Clone)]
pub enum ObjectMap {
    Term(TermMap),
    /// A referencing object map: the subjects of another triples map (by index in
    /// [`Mapping::triples_maps`]) whose records join this one: for each `(child, parent)`
    /// condition, a value of `child` on this record equals one of `parent` on the parent's
    /// (compared as text). Without conditions both maps must share their logical source, and
    /// the parent's subject is made from the same record.
    Join {
        parent: usize,
        conditions: Vec<(TermMap, TermMap)>,
    },
    /// `rml:quotedTriplesMap` (RML-star): the triples of another map, named by their
    /// reifiers.
    Quoted(Quoted),
}

/// How to make an RDF term from a record.
#[derive(Debug, Clone)]
pub struct TermMap {
    pub value: Value,
    pub term_type: TermType,
    /// How template values are made safe for IRIs.
    pub encoding: Encoding,
    pub datatype: Option<NamedNode>,
    /// A datatype made per record (`rml:datatypeMap` other than a constant).
    pub datatype_map: Option<Box<TermMap>>,
    pub language: Option<String>,
    /// A language tag made per record (`rml:languageMap` other than a constant).
    pub language_map: Option<Box<TermMap>>,
    /// `rml:condition`: a map whose value must be `true` for this map to make terms.
    pub condition: Option<Box<TermMap>>,
    /// The logical targets (by index in [`Mapping::targets`]) of this map and of its datatype
    /// and language maps.
    pub targets: Vec<usize>,
    /// `rml:gather` (RML-CC): this map makes a collection or container of the values of these
    /// maps, named by its value ([`Value::Fresh`]: a new blank node each time).
    pub gather: Option<Box<Gather>>,
}

/// A gather map's members and the collection or container they make.
#[derive(Debug, Clone)]
pub struct Gather {
    pub items: Vec<ObjectMap>,
    pub kind: Collection,
    /// `rml:cartesianProduct`: one collection per combination of the items' values, instead
    /// of one of all their values in order (`rml:append`).
    pub cartesian: bool,
    /// `rml:allowEmptyListAndContainer`: no values make `rdf:nil` or an empty container.
    pub allow_empty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Collection {
    List,
    Bag,
    Seq,
    Alt,
}

/// How values put in IRI templates are percent-encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// IRIs (the default): every character but `iunreserved` ones, so non-ASCII letters stay
    /// as they are.
    Iri,
    /// `rml:URI`: every character but the ASCII `unreserved` ones.
    Uri,
    /// `rml:UnsafeIRI`: values are put in as they are.
    Unsafe,
}

#[derive(Debug, Clone)]
pub enum Value {
    Constant(Term),
    /// `rml:reference` or `rr:column`.
    Reference(String),
    Template(Vec<Part>),
    /// A blank node per record (a blank node term map with no value).
    Fresh,
    /// The value of a function (RML-FNML `rml:functionExecution`, legacy
    /// `fnml:functionValue`).
    Function(Box<FunctionCall>),
}

/// A function execution: the function, the maps giving its parameters, and the output used.
#[derive(Debug, Clone)]
pub struct FunctionCall {
    pub function: NamedNode,
    /// The parameters (by local name) and the maps giving their values.
    pub inputs: Vec<(String, TermMap)>,
    /// The output used (by local name).
    pub output: String,
    pub(crate) body: crate::functions::Function,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    Text(String),
    Reference(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermType {
    Iri,
    BlankNode,
    Literal,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Position {
    Subject,
    Predicate,
    Object,
    Graph,
    /// A datatype map: IRIs only.
    Datatype,
    /// A language map: literals only.
    Language,
    /// A side of a join condition: any term, compared as text.
    Join,
    /// A function input or condition: any term, literals by default.
    Input,
}

impl Mapping {
    /// Reads a mapping document written in Turtle.
    pub fn parse_turtle(turtle: &str, base_iri: Option<&str>) -> Result<Self, RmlError> {
        let mut parser = RdfParser::from_format(RdfFormat::Turtle);
        if let Some(base) = base_iri {
            parser = parser
                .with_base_iri(base)
                .map_err(|e| RmlError::Mapping(format!("invalid base IRI {base}: {e}")))?;
        }
        let mut graph = Graph::new();
        for q in parser.for_slice(turtle.as_bytes()) {
            graph.insert(q?.as_ref());
        }
        Self::from_graph(&graph, base_iri)
    }

    /// Reads a YARRRML document: the RML rules it stands for. A `base` key in it overrides
    /// `base_iri`.
    pub fn parse_yarrrml(yarrrml: &str, base_iri: Option<&str>) -> Result<Self, RmlError> {
        let (graph, base) = crate::yarrrml::to_rml(yarrrml, base_iri)?;
        Self::from_graph(&graph, base.as_deref())
    }

    /// Reads the triples maps of a mapping graph.
    pub fn from_graph(graph: &Graph, base_iri: Option<&str>) -> Result<Self, RmlError> {
        let doc = Doc {
            graph,
            views: RefCell::default(),
            view_index: RefCell::default(),
            reading: RefCell::default(),
            term_maps: RefCell::default(),
            term_map_count: std::cell::Cell::new(0),
            targets: RefCell::default(),
            index: RefCell::default(),
        };
        // Triples maps: typed as such, or with a logical source or table.
        let mut nodes: Vec<NamedOrBlankNode> = Vec::new();
        for ns in NAMESPACES {
            let class = NamedNode::new_unchecked(format!("{ns}TriplesMap"));
            nodes.extend(
                graph
                    .subjects_for_predicate_object(rdf::TYPE, &class)
                    .map(|s| s.into_owned()),
            );
            for p in ["logicalSource", "logicalTable"] {
                let p = NamedNode::new_unchecked(format!("{ns}{p}"));
                nodes.extend(
                    graph
                        .triples_for_predicate(&p)
                        .map(|t| t.subject.into_owned()),
                );
            }
        }
        nodes.sort_by_key(|n| n.to_string());
        nodes.dedup();
        let index: HashMap<NamedOrBlankNode, usize> = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), i))
            .collect();
        doc.index.replace(index.clone());
        let triples_maps = nodes
            .iter()
            .map(|n| doc.triples_map(n.as_ref(), &index))
            .collect::<Result<_, _>>()?;
        // Views no triples map reads must be valid too.
        let mut views: Vec<NamedOrBlankNode> = Vec::new();
        for ns in NAMESPACES {
            let class = NamedNode::new_unchecked(format!("{ns}LogicalView"));
            views.extend(
                graph
                    .subjects_for_predicate_object(rdf::TYPE, &class)
                    .map(|s| s.into_owned()),
            );
            let p = NamedNode::new_unchecked(format!("{ns}viewOn"));
            views.extend(
                graph
                    .triples_for_predicate(&p)
                    .map(|t| t.subject.into_owned()),
            );
        }
        views.sort_by_key(|n| n.to_string());
        for v in views {
            doc.view(v.as_ref())?;
        }
        Ok(Self {
            triples_maps,
            views: doc.views.into_inner(),
            base_iri: base_iri.map(str::to_owned),
            targets: doc.targets.into_inner(),
        })
    }
}

struct Doc<'a> {
    graph: &'a Graph,
    views: RefCell<Vec<LogicalView>>,
    view_index: RefCell<HashMap<NamedOrBlankNode, usize>>,
    /// The views being read, to find cycles.
    reading: RefCell<Vec<NamedOrBlankNode>>,
    /// The term maps being read, to find cycles and bound nesting.
    term_maps: RefCell<Vec<NamedOrBlankNode>>,
    /// How many term maps have been read: a term map used in several places is read, and
    /// later evaluated, once per use.
    term_map_count: std::cell::Cell<usize>,
    targets: RefCell<Vec<LogicalTarget>>,
    /// The triples maps by node, for referencing object maps in gather maps.
    index: RefCell<HashMap<NamedOrBlankNode, usize>>,
}

/// How deeply term maps may nest: through function inputs, conditions, datatype and
/// language maps.
const MAX_TERM_MAP_DEPTH: usize = 64;

/// How many term maps a document may read, counting each use of a shared term map. It bounds
/// the work of reading the document and of evaluating it on each record, which a chain of
/// term maps that each use the next twice would otherwise double at every level.
const MAX_TERM_MAPS: usize = 100_000;

fn node(t: TermRef<'_>) -> Option<NamedOrBlankNodeRef<'_>> {
    match t {
        TermRef::NamedNode(n) => Some(n.into()),
        TermRef::BlankNode(b) => Some(b.into()),
        _ => None,
    }
}

fn local(iri: NamedNodeRef<'_>) -> Option<&str> {
    NAMESPACES
        .iter()
        .find_map(|ns| iri.as_str().strip_prefix(ns))
}

impl<'a> Doc<'a> {
    /// The values of `local` (in any of the namespaces) on `s`.
    fn all(&self, s: NamedOrBlankNodeRef<'_>, local: &str) -> Vec<TermRef<'a>> {
        let mut out = Vec::new();
        for ns in NAMESPACES {
            let p = NamedNode::new_unchecked(format!("{ns}{local}"));
            out.extend(self.graph.objects_for_subject_predicate(s, &p));
        }
        out
    }

    /// The value of `local` on `s`; an error if it has several.
    fn one(
        &self,
        s: NamedOrBlankNodeRef<'_>,
        local: &str,
    ) -> Result<Option<TermRef<'a>>, RmlError> {
        let mut all = self.all(s, local);
        match all.len() {
            0 | 1 => Ok(all.pop()),
            _ => Err(RmlError::Mapping(format!("{s} has several {local} values"))),
        }
    }

    fn string(&self, s: NamedOrBlankNodeRef<'_>, local: &str) -> Result<Option<String>, RmlError> {
        match self.one(s, local)? {
            None => Ok(None),
            Some(TermRef::Literal(l)) => Ok(Some(l.value().to_owned())),
            Some(t) => Err(RmlError::Mapping(format!(
                "{s} {local} must be a literal, not {t}"
            ))),
        }
    }

    fn map_node(
        &self,
        s: NamedOrBlankNodeRef<'_>,
        local: &str,
    ) -> Result<Option<NamedOrBlankNodeRef<'a>>, RmlError> {
        match self.one(s, local)? {
            None => Ok(None),
            Some(t) => node(t)
                .map(Some)
                .ok_or_else(|| RmlError::Mapping(format!("{s} {local} must be a node, not {t}"))),
        }
    }

    fn triples_map(
        &self,
        n: NamedOrBlankNodeRef<'_>,
        index: &HashMap<NamedOrBlankNode, usize>,
    ) -> Result<TriplesMap, RmlError> {
        let source = self.logical_source(n)?;
        // The subject: a subject map, or the `subject` constant shortcut.
        let subject_node = self.map_node(n, "subjectMap")?;
        let quoted_subject = match subject_node {
            Some(sm) => self.quoted(sm, index)?,
            None => None,
        };
        let (subject, classes, graphs) = match (subject_node, self.one(n, "subject")?) {
            (Some(sm), None) if quoted_subject.is_some() => {
                let mut classes = Vec::new();
                for c in self.all(sm, "class") {
                    match c {
                        TermRef::NamedNode(c) => classes.push(c.into_owned()),
                        c => return Err(RmlError::Mapping(format!("class {c} is not an IRI"))),
                    }
                }
                (
                    TermMap::new(Value::Fresh, TermType::BlankNode),
                    classes,
                    self.graph_maps(sm)?,
                )
            }
            (Some(sm), None) => {
                let mut classes = Vec::new();
                for c in self.all(sm, "class") {
                    match c {
                        TermRef::NamedNode(c) => classes.push(c.into_owned()),
                        c => return Err(RmlError::Mapping(format!("class {c} is not an IRI"))),
                    }
                }
                (
                    self.term_map(sm, Position::Subject)?,
                    classes,
                    self.graph_maps(sm)?,
                )
            }
            (None, Some(c)) => (
                constant(c.into_owned(), Position::Subject)?,
                Vec::new(),
                Vec::new(),
            ),
            (None, None) => {
                return Err(RmlError::Mapping(format!(
                    "triples map {n} has no subject map"
                )));
            }
            (Some(_), Some(_)) => {
                return Err(RmlError::Mapping(format!(
                    "triples map {n} has both a subject and a subject map"
                )));
            }
        };
        let mut predicate_objects = Vec::new();
        for pom in self.all(n, "predicateObjectMap") {
            let pom = node(pom).ok_or_else(|| {
                RmlError::Mapping(format!("predicate-object map {pom} of {n} is not a node"))
            })?;
            predicate_objects.push(self.predicate_object_map(pom, index)?);
        }
        let base_iri = match self.one(n, "baseIRI")? {
            None => None,
            Some(TermRef::NamedNode(b)) => Some(b.as_str().to_owned()),
            Some(TermRef::Literal(l)) => Some(l.value().to_owned()),
            Some(t) => return Err(RmlError::Mapping(format!("base IRI {t} is not an IRI"))),
        };
        let non_asserted = NAMESPACES.iter().any(|ns| {
            self.graph.contains(oxigraph::model::TripleRef::new(
                n,
                rdf::TYPE,
                &NamedNode::new_unchecked(format!("{ns}NonAssertedTriplesMap")),
            ))
        });
        let asserted_type = NAMESPACES.iter().any(|ns| {
            self.graph.contains(oxigraph::model::TripleRef::new(
                n,
                rdf::TYPE,
                &NamedNode::new_unchecked(format!("{ns}AssertedTriplesMap")),
            ))
        });
        if non_asserted && asserted_type {
            return Err(RmlError::Mapping(format!(
                "triples map {n} is both asserted and non-asserted"
            )));
        }
        Ok(TriplesMap {
            node: n.into_owned(),
            source,
            subject,
            classes,
            graphs,
            predicate_objects,
            base_iri,
            asserted: !non_asserted,
            quoted_subject,
        })
    }

    fn logical_source(&self, n: NamedOrBlankNodeRef<'_>) -> Result<LogicalSource, RmlError> {
        if let Some(table) = self.map_node(n, "logicalTable")? {
            let source = match (
                self.string(table, "tableName")?,
                self.string(table, "sqlQuery")?,
            ) {
                (Some(t), None) => SourceRef::Table(t),
                (None, Some(q)) => SourceRef::Query(q),
                _ => {
                    return Err(RmlError::Mapping(format!(
                        "logical table {table} needs one table name or SQL query"
                    )));
                }
            };
            return Ok(LogicalSource {
                source,
                formulation: Formulation::Sql,
                iterator: None,
                database: None,
                io: SourceIo::default(),
            });
        }
        match self.map_node(n, "logicalSource")? {
            Some(ls) => self.abstract_source(ls),
            // RML-FNML's RMLFNMLTC0200: constants and functions of constants, made once.
            None => Ok(LogicalSource {
                source: SourceRef::Empty,
                formulation: Formulation::Csv,
                iterator: None,
                database: None,
                io: SourceIo::default(),
            }),
        }
    }

    /// Whether `n` has the class `local` (in any of the namespaces).
    fn is_a(&self, n: NamedOrBlankNodeRef<'_>, local: &str) -> bool {
        NAMESPACES.iter().any(|ns| {
            self.graph.contains(oxigraph::model::TripleRef::new(
                n,
                rdf::TYPE,
                &NamedNode::new_unchecked(format!("{ns}{local}")),
            ))
        })
    }

    /// A logical source or a logical view.
    fn abstract_source(&self, ls: NamedOrBlankNodeRef<'_>) -> Result<LogicalSource, RmlError> {
        if self.is_a(ls, "LogicalView") || !self.all(ls, "viewOn").is_empty() {
            return Ok(LogicalSource {
                source: SourceRef::View(self.view(ls)?),
                formulation: Formulation::View,
                iterator: None,
                database: None,
                io: SourceIo::default(),
            });
        }
        let source_node = self.one(ls, "source")?;
        if let Some(database) = source_node
            .and_then(node)
            .map(|s| self.database(s))
            .transpose()?
            .flatten()
        {
            // Legacy RML: rr:tableName or rml:query; new RML: the iterator of an
            // rml:SQL2008Table or rml:SQL2008Query formulation.
            let kind = match self.one(ls, "referenceFormulation")? {
                Some(TermRef::NamedNode(f)) => {
                    f.as_str().rsplit(['#', '/']).next().map(str::to_owned)
                }
                _ => None,
            };
            let iterator = self.string(ls, "iterator")?;
            let source = match (
                self.string(ls, "tableName")?,
                self.string(ls, "query")?.or(self.string(ls, "sqlQuery")?),
                kind.as_deref(),
                iterator,
            ) {
                (Some(t), None, _, _) | (None, None, Some("SQL2008Table"), Some(t)) => {
                    SourceRef::Table(t)
                }
                (None, Some(q), _, _) | (None, None, Some("SQL2008Query"), Some(q)) => {
                    SourceRef::Query(q)
                }
                _ => {
                    return Err(RmlError::Mapping(format!(
                        "logical source {ls} on a database needs one table name or query"
                    )));
                }
            };
            return Ok(LogicalSource {
                source,
                formulation: Formulation::Sql,
                iterator: None,
                database: Some(database),
                io: SourceIo::default(),
            });
        }
        let source = match source_node {
            Some(TermRef::Literal(l)) => SourceRef::Path(l.value().to_owned()),
            Some(t) => {
                let s = node(t).expect("not a literal");
                // New RML: rml:path (of a RelativePathSource or a FilePath); legacy: csvw:url.
                let path = self.string(s, "path")?.or_else(|| {
                    match self
                        .graph
                        .object_for_subject_predicate(s, NamedNodeRef::new_unchecked(CSVW_URL))
                    {
                        Some(TermRef::Literal(l)) => Some(l.value().to_owned()),
                        Some(TermRef::NamedNode(u)) => Some(u.as_str().to_owned()),
                        _ => None,
                    }
                });
                match path {
                    Some(p) => SourceRef::Path(p),
                    None => match t {
                        TermRef::NamedNode(iri) => SourceRef::Path(iri.as_str().to_owned()),
                        _ => return Err(RmlError::Unsupported(format!("the source {t}"))),
                    },
                }
            }
            None => {
                // R2RML-style tables in an RML logical source.
                match (self.string(ls, "tableName")?, self.string(ls, "query")?) {
                    (Some(t), _) => SourceRef::Table(t),
                    (None, Some(q)) => SourceRef::Query(q),
                    (None, None) => {
                        return Err(RmlError::Mapping(format!(
                            "logical source {ls} has no source"
                        )));
                    }
                }
            }
        };
        let formulation = match self.formulation(ls)? {
            Some(f) => f,
            None => match &source {
                SourceRef::Path(p) => {
                    let lower = p.to_ascii_lowercase();
                    if lower.ends_with(".json") {
                        Formulation::JsonPath
                    } else if lower.ends_with(".csv")
                        || lower.ends_with(".tsv")
                        || crate::run::is_spreadsheet(&lower)
                    {
                        Formulation::Csv
                    } else if lower.ends_with(".xml") {
                        Formulation::XPath
                    } else {
                        return Err(RmlError::Mapping(format!(
                            "logical source {ls} has no reference formulation"
                        )));
                    }
                }
                _ => Formulation::Sql,
            },
        };
        let io = match source_node.and_then(node) {
            Some(s) => self.source_io(ls, s)?,
            None => SourceIo::default(),
        };
        Ok(LogicalSource {
            source,
            formulation,
            iterator: self.string(ls, "iterator")?,
            database: None,
            io,
        })
    }

    /// `rml:compression`, `rml:encoding` and `rml:null` of the source `s` of `ls`.
    fn source_io(
        &self,
        ls: NamedOrBlankNodeRef<'_>,
        s: NamedOrBlankNodeRef<'_>,
    ) -> Result<SourceIo, RmlError> {
        let local = |local: &str| -> Result<Option<String>, RmlError> {
            Ok(match self.one(s, local)? {
                Some(TermRef::NamedNode(n)) => {
                    n.as_str().rsplit(['#', '/']).next().map(str::to_owned)
                }
                Some(t) => {
                    return Err(RmlError::Mapping(format!("rml:{local} {t} is not an IRI")));
                }
                None => None,
            })
        };
        let compression = local("compression")?.filter(|c| c != "none");
        let encoding = local("encoding")?;
        let nulls: Vec<String> = self
            .all(s, "null")
            .into_iter()
            .map(|t| match t {
                TermRef::Literal(l) => Ok(l.value().to_owned()),
                t => Err(RmlError::Mapping(format!("rml:null {t} is not a literal"))),
            })
            .collect::<Result<_, _>>()?;
        let rml_io = self
            .graph
            .object_for_subject_predicate(ls, NamedNodeRef::new_unchecked(&format!("{RML}source")))
            .is_some();
        Ok(SourceIo {
            compression,
            encoding,
            nulls: (!nulls.is_empty()).then_some(nulls),
            rml_io,
        })
    }

    /// The `d2rq:Database` `s` describes, if it has a `d2rq:jdbcDSN`.
    fn database(&self, s: NamedOrBlankNodeRef<'_>) -> Result<Option<Database>, RmlError> {
        let get = |local: &str| -> Result<Option<String>, RmlError> {
            let p = NamedNode::new_unchecked(format!("{D2RQ}{local}"));
            let mut values = self.graph.objects_for_subject_predicate(s, &p);
            let value = match values.next() {
                None => return Ok(None),
                Some(TermRef::Literal(l)) => l.value().to_owned(),
                Some(TermRef::NamedNode(n)) => n.as_str().to_owned(),
                Some(t) => {
                    return Err(RmlError::Mapping(format!(
                        "{s} d2rq:{local} must be a literal, not {t}"
                    )));
                }
            };
            if values.next().is_some() {
                return Err(RmlError::Mapping(format!(
                    "{s} has several d2rq:{local} values"
                )));
            }
            Ok(Some(value))
        };
        let Some(dsn) = get("jdbcDSN")? else {
            return Ok(None);
        };
        Ok(Some(Database {
            dsn,
            driver: get("jdbcDriver")?,
            username: get("username")?,
            password: get("password")?,
        }))
    }

    fn formulation(&self, n: NamedOrBlankNodeRef<'_>) -> Result<Option<Formulation>, RmlError> {
        match self.one(n, "referenceFormulation")? {
            None => Ok(None),
            Some(TermRef::NamedNode(f)) => Ok(Some(formulation(f))),
            // A formulation described by its class, such as an rml:XPathReferenceFormulation
            // with rml:namespace declarations. XPath prefixes are matched as written in the
            // document, so the declarations are not needed.
            Some(TermRef::BlankNode(b)) => {
                for (class, f) in [
                    ("XPathReferenceFormulation", Formulation::XPath),
                    ("JSONPathReferenceFormulation", Formulation::JsonPath),
                    ("CSVReferenceFormulation", Formulation::Csv),
                ] {
                    if self.is_a(b.into(), class) {
                        return Ok(Some(f));
                    }
                }
                Err(RmlError::Unsupported(format!("reference formulation {b}")))
            }
            Some(t) => Err(RmlError::Mapping(format!(
                "reference formulation {t} is not an IRI"
            ))),
        }
    }

    /// Reads the logical view `n` (and the views it reads) into [`Self::views`].
    fn view(&self, n: NamedOrBlankNodeRef<'_>) -> Result<usize, RmlError> {
        let owned = n.into_owned();
        if let Some(&i) = self.view_index.borrow().get(&owned) {
            return Ok(i);
        }
        if self.reading.borrow().contains(&owned) {
            return Err(RmlError::Mapping(format!(
                "logical view {n} is defined in terms of itself"
            )));
        }
        self.reading.borrow_mut().push(owned.clone());
        let read = self.view_parts(n);
        self.reading.borrow_mut().pop();
        let (on, fields, joins) = read?;
        let mut views = self.views.borrow_mut();
        views.push(LogicalView {
            node: owned.clone(),
            on,
            fields,
            joins,
        });
        let i = views.len() - 1;
        self.view_index.borrow_mut().insert(owned, i);
        Ok(i)
    }

    #[allow(clippy::type_complexity)]
    fn view_parts(
        &self,
        n: NamedOrBlankNodeRef<'_>,
    ) -> Result<(LogicalSource, Vec<Field>, Vec<ViewJoin>), RmlError> {
        let on = self
            .map_node(n, "viewOn")?
            .ok_or_else(|| RmlError::Mapping(format!("logical view {n} has no rml:viewOn")))?;
        let on = self.abstract_source(on)?;
        let fields = self.fields(n, &mut Vec::new())?;
        if fields.is_empty() {
            return Err(RmlError::Mapping(format!("logical view {n} has no fields")));
        }
        let mut names = Vec::new();
        absolute_names(&fields, "", &mut names);
        let mut joins = Vec::new();
        for (local, inner) in [("leftJoin", false), ("innerJoin", true)] {
            for j in self.all(n, local) {
                let j = node(j)
                    .ok_or_else(|| RmlError::Mapping(format!("join {j} of {n} is not a node")))?;
                let parent = self.map_node(j, "parentLogicalView")?.ok_or_else(|| {
                    RmlError::Mapping(format!("join {j} of {n} has no parent logical view"))
                })?;
                let parent = self.view(parent)?;
                let mut conditions = Vec::new();
                for c in self.all(j, "joinCondition") {
                    let c = node(c).ok_or_else(|| {
                        RmlError::Mapping(format!("join condition {c} is not a node"))
                    })?;
                    conditions.push((self.join_side(c, "child")?, self.join_side(c, "parent")?));
                }
                if conditions.is_empty() {
                    return Err(RmlError::Mapping(format!(
                        "join {j} of {n} has no join condition"
                    )));
                }
                let fields = self.fields(j, &mut Vec::new())?;
                if fields.is_empty() {
                    return Err(RmlError::Mapping(format!("join {j} of {n} has no fields")));
                }
                for f in &fields {
                    if !matches!(f.kind, FieldKind::Expression(_)) || !f.fields.is_empty() {
                        return Err(RmlError::Mapping(format!(
                            "the field {} of join {j} must be an expression field without \
                             fields",
                            f.name
                        )));
                    }
                    if names.contains(&f.name) {
                        return Err(RmlError::Mapping(format!(
                            "logical view {n} has two fields named {}",
                            f.name
                        )));
                    }
                    names.push(f.name.clone());
                }
                joins.push(ViewJoin {
                    inner,
                    parent,
                    conditions,
                    fields,
                });
            }
        }
        Ok((on, fields, joins))
    }

    /// The fields of `n` (a view, field or join); `parents` are the fields above, to find
    /// cycles.
    fn fields(
        &self,
        n: NamedOrBlankNodeRef<'_>,
        parents: &mut Vec<NamedOrBlankNode>,
    ) -> Result<Vec<Field>, RmlError> {
        let mut out: Vec<Field> = Vec::new();
        for f in self.all(n, "field") {
            let f = node(f).ok_or_else(|| RmlError::Mapping(format!("field {f} is not a node")))?;
            let owned = f.into_owned();
            if parents.contains(&owned) {
                return Err(RmlError::Mapping(format!("field {f} contains itself")));
            }
            let name = self
                .string(f, "fieldName")?
                .ok_or_else(|| RmlError::Mapping(format!("field {f} has no rml:fieldName")))?;
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
            {
                return Err(RmlError::Mapping(format!(
                    "field name {name:?} is not alphanumerical"
                )));
            }
            if out.iter().any(|o| o.name == name) {
                return Err(RmlError::Mapping(format!(
                    "{n} has two fields named {name}"
                )));
            }
            let iterable = self.is_a(f, "IterableField")
                || self.one(f, "iterator")?.is_some()
                || self.one(f, "referenceFormulation")?.is_some();
            let kind = if iterable {
                FieldKind::Iterable {
                    formulation: self.formulation(f)?,
                    iterator: self.string(f, "iterator")?,
                }
            } else {
                let map = self.term_map(f, Position::Join)?;
                if matches!(map.value, Value::Fresh) {
                    return Err(RmlError::Mapping(format!("field {f} has no expression")));
                }
                FieldKind::Expression(map)
            };
            parents.push(owned);
            let fields = self.fields(f, parents);
            parents.pop();
            out.push(Field {
                name,
                kind,
                fields: fields?,
            });
        }
        Ok(out)
    }

    fn graph_maps(&self, n: NamedOrBlankNodeRef<'_>) -> Result<Vec<TermMap>, RmlError> {
        let mut out = Vec::new();
        for g in self.all(n, "graph") {
            out.push(constant(g.into_owned(), Position::Graph)?);
        }
        for g in self.all(n, "graphMap") {
            let g =
                node(g).ok_or_else(|| RmlError::Mapping(format!("graph map {g} is not a node")))?;
            out.push(self.term_map(g, Position::Graph)?);
        }
        Ok(out)
    }

    fn predicate_object_map(
        &self,
        pom: NamedOrBlankNodeRef<'_>,
        index: &HashMap<NamedOrBlankNode, usize>,
    ) -> Result<PredicateObjectMap, RmlError> {
        let mut predicates = Vec::new();
        for p in self.all(pom, "predicate") {
            predicates.push(constant(p.into_owned(), Position::Predicate)?);
        }
        for p in self.all(pom, "predicateMap") {
            let p = node(p)
                .ok_or_else(|| RmlError::Mapping(format!("predicate map {p} is not a node")))?;
            predicates.push(self.term_map(p, Position::Predicate)?);
        }
        let mut objects = Vec::new();
        for o in self.all(pom, "object") {
            objects.push(ObjectMap::Term(constant(o.into_owned(), Position::Object)?));
        }
        for o in self.all(pom, "objectMap") {
            let o = node(o)
                .ok_or_else(|| RmlError::Mapping(format!("object map {o} is not a node")))?;
            objects.push(self.object_map(o, index)?);
        }
        if predicates.is_empty() || objects.is_empty() {
            return Err(RmlError::Mapping(format!(
                "predicate-object map {pom} needs a predicate and an object"
            )));
        }
        Ok(PredicateObjectMap {
            predicates,
            objects,
            graphs: self.graph_maps(pom)?,
        })
    }

    /// An object map: a term map, or a referencing object map (`rml:parentTriplesMap`).
    fn object_map(
        &self,
        o: NamedOrBlankNodeRef<'_>,
        index: &HashMap<NamedOrBlankNode, usize>,
    ) -> Result<ObjectMap, RmlError> {
        if let Some(quoted) = self.quoted(o, index)? {
            return Ok(ObjectMap::Quoted(quoted));
        }
        let Some(parent) = self.one(o, "parentTriplesMap")? else {
            return Ok(ObjectMap::Term(self.term_map(o, Position::Object)?));
        };
        let parent = node(parent)
            .and_then(|p| index.get(&p.into_owned()).copied())
            .ok_or_else(|| {
                RmlError::Mapping(format!("parent triples map {parent} is not a triples map"))
            })?;
        let mut conditions = Vec::new();
        for c in self.all(o, "joinCondition") {
            let c = node(c)
                .ok_or_else(|| RmlError::Mapping(format!("join condition {c} is not a node")))?;
            conditions.push((self.join_side(c, "child")?, self.join_side(c, "parent")?));
        }
        Ok(ObjectMap::Join { parent, conditions })
    }

    /// `rml:quotedTriplesMap` on a subject or object map `n`, with its join conditions.
    fn quoted(
        &self,
        n: NamedOrBlankNodeRef<'_>,
        index: &HashMap<NamedOrBlankNode, usize>,
    ) -> Result<Option<Quoted>, RmlError> {
        let Some(map) = self.one(n, "quotedTriplesMap")? else {
            return Ok(None);
        };
        let map = node(map)
            .and_then(|m| index.get(&m.into_owned()).copied())
            .ok_or_else(|| {
                RmlError::Mapping(format!("quoted triples map {map} is not a triples map"))
            })?;
        for other in [
            "template",
            "reference",
            "constant",
            "column",
            "parentTriplesMap",
        ] {
            if self.one(n, other)?.is_some() {
                return Err(RmlError::Mapping(format!(
                    "{n} has both rml:quotedTriplesMap and rml:{other}"
                )));
            }
        }
        let mut conditions = Vec::new();
        for c in self.all(n, "joinCondition") {
            let c = node(c)
                .ok_or_else(|| RmlError::Mapping(format!("join condition {c} is not a node")))?;
            conditions.push((self.join_side(c, "child")?, self.join_side(c, "parent")?));
        }
        Ok(Some(Quoted { map, conditions }))
    }

    /// `rml:gather` on `n`: the maps whose values make a collection or container (RML-CC).
    fn gather(&self, n: NamedOrBlankNodeRef<'_>) -> Result<Option<Gather>, RmlError> {
        let Some(list) = self.one(n, "gather")? else {
            return Ok(None);
        };
        let mut items = Vec::new();
        let mut cell = list;
        let index = self.index.borrow().clone();
        let mut cells = std::collections::HashSet::new();
        loop {
            let c = match cell {
                TermRef::NamedNode(n) if n == rdf::NIL => break,
                t => node(t).ok_or_else(|| {
                    RmlError::Mapping(format!("rml:gather of {n} is not an RDF list"))
                })?,
            };
            if !cells.insert(c) {
                return Err(RmlError::Mapping(format!(
                    "rml:gather of {n} is a list that never ends"
                )));
            }
            let item = self
                .graph
                .object_for_subject_predicate(c, rdf::FIRST)
                .and_then(node)
                .ok_or_else(|| {
                    RmlError::Mapping(format!("rml:gather of {n}: an item is not a term map"))
                })?;
            items.push(self.object_map(item, &index)?);
            cell = self
                .graph
                .object_for_subject_predicate(c, rdf::REST)
                .ok_or_else(|| RmlError::Mapping(format!("rml:gather of {n}: a broken list")))?;
        }
        let kind = match self.one(n, "gatherAs")? {
            Some(TermRef::NamedNode(k)) if k == rdf::LIST => Collection::List,
            Some(TermRef::NamedNode(k))
                if k.as_str() == concat!("http://www.w3.org/1999/02/22-rdf-syntax-ns#", "Bag") =>
            {
                Collection::Bag
            }
            Some(TermRef::NamedNode(k))
                if k.as_str() == concat!("http://www.w3.org/1999/02/22-rdf-syntax-ns#", "Seq") =>
            {
                Collection::Seq
            }
            Some(TermRef::NamedNode(k))
                if k.as_str() == concat!("http://www.w3.org/1999/02/22-rdf-syntax-ns#", "Alt") =>
            {
                Collection::Alt
            }
            other => {
                return Err(RmlError::Mapping(format!(
                    "gather map {n} needs rml:gatherAs rdf:List, rdf:Bag, rdf:Seq or rdf:Alt, \
                     not {other:?}"
                )));
            }
        };
        let cartesian = match self.one(n, "strategy")? {
            None => false,
            Some(TermRef::NamedNode(s)) if local(s) == Some("append") => false,
            Some(TermRef::NamedNode(s)) if local(s) == Some("cartesianProduct") => true,
            Some(s) => return Err(RmlError::Mapping(format!("unknown strategy {s}"))),
        };
        let allow_empty = match self.one(n, "allowEmptyListAndContainer")? {
            None => false,
            Some(TermRef::Literal(l)) if l.value() == "true" || l.value() == "1" => true,
            Some(TermRef::Literal(l)) if l.value() == "false" || l.value() == "0" => false,
            Some(t) => {
                return Err(RmlError::Mapping(format!(
                    "rml:allowEmptyListAndContainer {t} is not a boolean"
                )));
            }
        };
        Ok(Some(Gather {
            items,
            kind,
            cartesian,
            allow_empty,
        }))
    }

    /// A side of a join condition: a literal reference (`rr:child "x"`), or (new RML) an
    /// expression map (`rml:child [...]` or `rml:childMap [...]`).
    fn join_side(&self, c: NamedOrBlankNodeRef<'_>, local: &str) -> Result<TermMap, RmlError> {
        let missing = || RmlError::Mapping(format!("join condition {c} has no {local}"));
        let map = match self.one(c, local)? {
            Some(TermRef::Literal(l)) => {
                return Ok(TermMap::new(
                    Value::Reference(l.value().to_owned()),
                    TermType::Literal,
                ));
            }
            Some(t) => node(t).ok_or_else(missing)?,
            None => self
                .map_node(c, &format!("{local}Map"))?
                .ok_or_else(missing)?,
        };
        self.term_map(map, Position::Join)
    }

    /// Reads the term map `n`, refusing one that is part of itself (through a condition,
    /// function input, datatype or language map) or that nests too deeply.
    fn term_map(
        &self,
        n: NamedOrBlankNodeRef<'_>,
        position: Position,
    ) -> Result<TermMap, RmlError> {
        let owned = n.into_owned();
        {
            let reading = self.term_maps.borrow();
            if reading.contains(&owned) {
                return Err(RmlError::Mapping(format!(
                    "term map {n} is defined in terms of itself"
                )));
            }
            if reading.len() >= MAX_TERM_MAP_DEPTH {
                return Err(RmlError::Mapping(format!(
                    "term map {n} nests more than {MAX_TERM_MAP_DEPTH} term maps deep"
                )));
            }
        }
        let count = self.term_map_count.get() + 1;
        if count > MAX_TERM_MAPS {
            return Err(RmlError::Mapping(format!(
                "the mapping uses more than {MAX_TERM_MAPS} term maps, counting each use of \
                 a shared one (at {n})"
            )));
        }
        self.term_map_count.set(count);
        self.term_maps.borrow_mut().push(owned);
        let read = self.term_map_parts(n, position);
        self.term_maps.borrow_mut().pop();
        read
    }

    fn term_map_parts(
        &self,
        n: NamedOrBlankNodeRef<'_>,
        position: Position,
    ) -> Result<TermMap, RmlError> {
        let mut values = Vec::new();
        if let Some(c) = self.one(n, "constant")? {
            values.push(Value::Constant(c.into_owned()));
        }
        if let Some(r) = self.string(n, "reference")? {
            values.push(Value::Reference(r));
        }
        if let Some(r) = self.string(n, "column")? {
            values.push(Value::Reference(r));
        }
        let mut encoding = Encoding::Iri;
        if let Some(t) = self.string(n, "template")? {
            values.push(Value::Template(template(&t)?));
        }
        if let Some(call) = self.function_call(n)? {
            values.push(Value::Function(Box::new(call)));
        }
        let mut targets = self.targets_of(n)?;
        let gather = self.gather(n)?.map(Box::new);
        let condition = match self.map_node(n, "condition")? {
            Some(c) => Some(Box::new(self.term_map(c, Position::Input)?)),
            None => None,
        };
        let explicit = match self.one(n, "termType")? {
            None => None,
            Some(TermRef::NamedNode(t)) => Some(match local(t) {
                Some("UnsafeIRI") => {
                    encoding = Encoding::Unsafe;
                    TermType::Iri
                }
                Some("URI") => {
                    encoding = Encoding::Uri;
                    TermType::Iri
                }
                Some("IRI") => TermType::Iri,
                Some("BlankNode") => TermType::BlankNode,
                Some("Literal") => TermType::Literal,
                _ => return Err(RmlError::Mapping(format!("unknown term type {t}"))),
            }),
            Some(t) => return Err(RmlError::Mapping(format!("term type {t} is not an IRI"))),
        };
        let value = match values.len() {
            1 => values.pop().unwrap(),
            0 if explicit == Some(TermType::BlankNode) || gather.is_some() => Value::Fresh,
            0 => {
                return Err(RmlError::Mapping(format!(
                    "term map {n} needs a constant, reference, column, template or function"
                )));
            }
            _ => {
                return Err(RmlError::Mapping(format!(
                    "term map {n} has more than one of constant, reference, column, template \
                     and function"
                )));
            }
        };
        // A literal constant with the IRI term type (as the YARRRML parser writes
        // `[a, foaf:Person]`) is that IRI, as in RMLMapper. (With the blank node term type it
        // stays an error, as RML-Core's RMLTC0024a requires.)
        let value = match (value, explicit) {
            (Value::Constant(Term::Literal(l)), Some(TermType::Iri)) => Value::Constant(
                NamedNode::new(l.value())
                    .map_err(|e| {
                        RmlError::Mapping(format!("term map {n}: {} is not an IRI: {e}", l.value()))
                    })?
                    .into(),
            ),
            (v, _) => v,
        };
        let mut datatype_map = None;
        let datatype = match self.one(n, "datatype")? {
            Some(TermRef::NamedNode(d)) => Some(d.into_owned()),
            Some(t) => return Err(RmlError::Mapping(format!("datatype {t} is not an IRI"))),
            None => match self.map_node(n, "datatypeMap")? {
                Some(m) => {
                    let map = self.term_map(m, Position::Datatype)?;
                    targets.extend(&map.targets);
                    match map.value {
                        Value::Constant(Term::NamedNode(d)) => Some(d),
                        _ => {
                            datatype_map = Some(Box::new(map));
                            None
                        }
                    }
                }
                None => None,
            },
        };
        let mut language_map = None;
        let language = match self.string(n, "language")? {
            Some(l) => Some(l),
            None => match self.map_node(n, "languageMap")? {
                Some(m) => {
                    let map = self.term_map(m, Position::Language)?;
                    targets.extend(&map.targets);
                    match map.value {
                        Value::Constant(Term::Literal(l)) => Some(l.value().to_owned()),
                        _ => {
                            language_map = Some(Box::new(map));
                            None
                        }
                    }
                }
                None => None,
            },
        };
        // BCP 47 allows a primary language subtag of 5 to 8 letters, but none is registered:
        // `english` is a mistake, not a language.
        if let Some(l) = &language
            && (Literal::new_language_tagged_literal("", l).is_err()
                || l.split('-').next().is_some_and(|p| p.len() > 3))
        {
            return Err(RmlError::Mapping(format!(
                "term map {n}: {l} is not a valid language tag"
            )));
        }
        let typed = datatype.is_some()
            || language.is_some()
            || datatype_map.is_some()
            || language_map.is_some();
        if let Value::Constant(c) = &value {
            let mut map = constant(c.clone(), position)?;
            if explicit.is_some_and(|t| t != map.term_type) {
                return Err(RmlError::Mapping(format!(
                    "term map {n}: the term type does not match the constant {c}"
                )));
            }
            map.datatype = datatype.or(map.datatype);
            map.language = language.or(map.language);
            map.datatype_map = datatype_map;
            map.language_map = language_map;
            map.condition = condition;
            map.targets = targets;
            map.gather = gather;
            return Ok(map);
        }
        let term_type = explicit.unwrap_or(match (position, &value) {
            (_, Value::Fresh) => TermType::BlankNode,
            (Position::Object | Position::Join, Value::Reference(_))
            | (Position::Object, Value::Function(_))
            | (Position::Language | Position::Input, _) => TermType::Literal,
            (Position::Object, _) if typed => TermType::Literal,
            _ => TermType::Iri,
        });
        if typed && term_type != TermType::Literal {
            return Err(RmlError::Mapping(format!(
                "term map {n} has a datatype or language but makes no literals"
            )));
        }
        if !matches!(
            position,
            Position::Object | Position::Language | Position::Join | Position::Input
        ) && term_type == TermType::Literal
        {
            return Err(RmlError::Mapping(format!(
                "term map {n} makes literals where only IRIs or blank nodes are allowed"
            )));
        }
        if matches!(
            position,
            Position::Predicate | Position::Graph | Position::Datatype | Position::Language
        ) && term_type == TermType::BlankNode
        {
            return Err(RmlError::Mapping(format!(
                "term map {n} makes blank nodes where only IRIs are allowed"
            )));
        }
        Ok(TermMap {
            value,
            term_type,
            encoding,
            datatype,
            datatype_map,
            language,
            language_map,
            condition,
            targets,
            gather,
        })
    }

    /// The logical targets `n` names (`rml:logicalTarget`), by index in [`Mapping::targets`].
    fn targets_of(&self, n: NamedOrBlankNodeRef<'_>) -> Result<Vec<usize>, RmlError> {
        let mut out = Vec::new();
        for t in self.all(n, "logicalTarget") {
            let t = node(t)
                .ok_or_else(|| RmlError::Mapping(format!("logical target {t} is not a node")))?;
            if let Some(i) = self
                .targets
                .borrow()
                .iter()
                .position(|x| x.node.as_ref() == t)
            {
                out.push(i);
                continue;
            }
            let target = self.logical_target(t)?;
            let mut targets = self.targets.borrow_mut();
            out.push(targets.len());
            targets.push(target);
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    fn logical_target(&self, t: NamedOrBlankNodeRef<'_>) -> Result<LogicalTarget, RmlError> {
        let target = self
            .map_node(t, "target")?
            .ok_or_else(|| RmlError::Mapping(format!("logical target {t} has no rml:target")))?;
        let path = match (self.string(target, "path")?, self.one(target, "root")?) {
            (Some(p), _) => p,
            (None, _) => {
                return Err(RmlError::Unsupported(format!(
                    "logical target {t}: targets other than a file path"
                )));
            }
        };
        let local = |s: NamedOrBlankNodeRef<'_>, name: &str| -> Result<Option<String>, RmlError> {
            match self.one(s, name)? {
                Some(TermRef::NamedNode(n)) => {
                    Ok(n.as_str().rsplit(['#', '/']).next().map(str::to_owned))
                }
                Some(v) => Err(RmlError::Mapping(format!("rml:{name} {v} is not an IRI"))),
                None => Ok(None),
            }
        };
        let serialization = local(t, "serialization")?.unwrap_or_else(|| "N-Quads".into());
        if !matches!(
            serialization.as_str(),
            "N-Quads" | "N-Triples" | "Turtle" | "TriG" | "N3" | "RDF_XML" | "JSON-LD" | "RDF_JSON"
        ) {
            return Err(RmlError::Unsupported(format!(
                "logical target {t}: the serialization {serialization}"
            )));
        }
        let compression = local(target, "compression")?.filter(|c| c != "none");
        if let Some(c) = &compression
            && !matches!(c.as_str(), "gzip" | "zip" | "targz" | "tarxz")
        {
            return Err(RmlError::Unsupported(format!(
                "logical target {t}: the compression {c}"
            )));
        }
        let encoding = local(target, "encoding")?;
        Ok(LogicalTarget {
            node: t.into_owned(),
            path,
            serialization,
            compression,
            encoding,
        })
    }

    /// The function `n` executes: `rml:functionExecution` with `rml:input`s and `rml:return`
    /// (or `rml:returnMap`), or legacy `fnml:functionValue`, a triples map whose
    /// predicate-object maps give `fno:executes` and the parameters.
    fn function_call(&self, n: NamedOrBlankNodeRef<'_>) -> Result<Option<FunctionCall>, RmlError> {
        if let Some(fe) = self.map_node(n, "functionExecution")? {
            let function = match self.one(fe, "function")? {
                Some(TermRef::NamedNode(f)) => f.into_owned(),
                Some(t) => return Err(RmlError::Mapping(format!("function {t} is not an IRI"))),
                None => {
                    return Err(RmlError::Mapping(format!(
                        "function execution {fe} has no function"
                    )));
                }
            };
            let mut inputs = Vec::new();
            for i in self.all(fe, "input") {
                let i =
                    node(i).ok_or_else(|| RmlError::Mapping(format!("input {i} is not a node")))?;
                let parameter = match self.one(i, "parameter")? {
                    Some(TermRef::NamedNode(p)) => p.into_owned(),
                    _ => {
                        return Err(RmlError::Mapping(format!(
                            "input {i} needs a parameter IRI"
                        )));
                    }
                };
                let map = match (
                    self.one(i, "inputValue")?,
                    self.map_node(i, "inputValueMap")?,
                ) {
                    (Some(c), None) => constant(c.into_owned(), Position::Input)?,
                    (None, Some(m)) => self.term_map(m, Position::Input)?,
                    _ => {
                        return Err(RmlError::Mapping(format!(
                            "input {i} needs one input value or input value map"
                        )));
                    }
                };
                inputs.push((parameter, map));
            }
            let output = match (self.one(n, "return")?, self.map_node(n, "returnMap")?) {
                (Some(TermRef::NamedNode(o)), None) => Some(o.into_owned()),
                (None, Some(m)) => match self.one(m, "constant")? {
                    Some(TermRef::NamedNode(o)) => Some(o.into_owned()),
                    _ => {
                        return Err(RmlError::Unsupported(format!(
                            "return map {m} other than a constant IRI"
                        )));
                    }
                },
                (None, None) => None,
                _ => {
                    return Err(RmlError::Mapping(format!(
                        "term map {n} needs one return IRI or return map"
                    )));
                }
            };
            return function_call(function, inputs, output).map(Some);
        }
        let legacy = NamedNode::new_unchecked("http://semweb.mmlab.be/ns/fnml#functionValue");
        let fv = match self.graph.object_for_subject_predicate(n, &legacy) {
            Some(t) => node(t)
                .ok_or_else(|| RmlError::Mapping(format!("function value {t} is not a node")))?,
            None => match self.map_node(n, "functionValue")? {
                Some(fv) => fv,
                None => return Ok(None),
            },
        };
        let mut function = None;
        let mut inputs = Vec::new();
        for pom in self.all(fv, "predicateObjectMap") {
            let pom = node(pom).ok_or_else(|| {
                RmlError::Mapping(format!("predicate-object map {pom} is not a node"))
            })?;
            let pom = self.predicate_object_map_of_function(pom)?;
            for (parameter, map) in pom {
                if parameter.as_str().ends_with("ontology#executes") {
                    match map.value {
                        Value::Constant(Term::NamedNode(f)) => function = Some(f),
                        _ => {
                            return Err(RmlError::Mapping(format!(
                                "function value {fv}: fno:executes needs a constant IRI"
                            )));
                        }
                    }
                } else {
                    inputs.push((parameter, map));
                }
            }
        }
        let function = function
            .ok_or_else(|| RmlError::Mapping(format!("function value {fv} has no fno:executes")))?;
        function_call(function, inputs, None).map(Some)
    }

    /// The (predicate, object map) pairs of a predicate-object map of a legacy function value.
    fn predicate_object_map_of_function(
        &self,
        pom: NamedOrBlankNodeRef<'_>,
    ) -> Result<Vec<(NamedNode, TermMap)>, RmlError> {
        let mut predicates = Vec::new();
        for p in self.all(pom, "predicate") {
            match p {
                TermRef::NamedNode(p) => predicates.push(p.into_owned()),
                p => return Err(RmlError::Mapping(format!("predicate {p} is not an IRI"))),
            }
        }
        for p in self.all(pom, "predicateMap") {
            match node(p)
                .map(|p| self.one(p, "constant"))
                .transpose()?
                .flatten()
            {
                Some(TermRef::NamedNode(p)) => predicates.push(p.into_owned()),
                _ => {
                    return Err(RmlError::Unsupported(format!(
                        "function parameter map {p} other than a constant IRI"
                    )));
                }
            }
        }
        let mut objects = Vec::new();
        for o in self.all(pom, "object") {
            objects.push(constant(o.into_owned(), Position::Input)?);
        }
        for o in self.all(pom, "objectMap") {
            let o = node(o)
                .ok_or_else(|| RmlError::Mapping(format!("object map {o} is not a node")))?;
            objects.push(self.term_map(o, Position::Input)?);
        }
        let mut out = Vec::new();
        for p in &predicates {
            for o in &objects {
                out.push((p.clone(), o.clone()));
            }
        }
        Ok(out)
    }
}

/// The local name of a function, parameter or output IRI.
fn local_name(iri: &NamedNode) -> &str {
    iri.as_str().rsplit(['#', '/']).next().unwrap_or_default()
}

/// A call of `function`, checked against what it takes and gives.
fn function_call(
    function: NamedNode,
    inputs: Vec<(NamedNode, TermMap)>,
    output: Option<NamedNode>,
) -> Result<FunctionCall, RmlError> {
    let body = crate::functions::function(function.as_str())
        .ok_or_else(|| RmlError::Mapping(format!("unknown function {function}")))?;
    let mut named = Vec::with_capacity(inputs.len());
    for (parameter, map) in inputs {
        let name = local_name(&parameter);
        if !body.params.contains(&name) {
            return Err(RmlError::Mapping(format!(
                "function {function} has no parameter {parameter} (it takes {})",
                body.params.join(", ")
            )));
        }
        named.push((name.to_owned(), map));
    }
    let output = match output {
        None => body.outputs[0].to_owned(),
        Some(o) if body.outputs.contains(&local_name(&o)) => local_name(&o).to_owned(),
        Some(o) => {
            return Err(RmlError::Mapping(format!(
                "function {function} has no output {o}"
            )));
        }
    };
    Ok(FunctionCall {
        function,
        inputs: named,
        output,
        body,
    })
}

impl TermMap {
    /// The term maps this one uses: its datatype and language maps, its condition and the
    /// inputs of its function.
    pub fn children(&self) -> impl Iterator<Item = &TermMap> {
        let inputs: &[(String, TermMap)] = match &self.value {
            Value::Function(call) => &call.inputs,
            _ => &[],
        };
        self.datatype_map
            .as_deref()
            .into_iter()
            .chain(self.language_map.as_deref())
            .chain(self.condition.as_deref())
            .chain(inputs.iter().map(|(_, m)| m))
            .chain(self.gather.iter().flat_map(|g| {
                g.items.iter().flat_map(|i| match i {
                    ObjectMap::Term(m) => vec![m],
                    ObjectMap::Join { conditions, .. }
                    | ObjectMap::Quoted(Quoted { conditions, .. }) => {
                        conditions.iter().map(|(c, _)| c).collect()
                    }
                })
            }))
    }

    /// A term map with no datatype or language.
    pub fn new(value: Value, term_type: TermType) -> Self {
        Self {
            value,
            term_type,
            encoding: Encoding::Iri,
            datatype: None,
            datatype_map: None,
            language: None,
            language_map: None,
            condition: None,
            targets: Vec::new(),
            gather: None,
        }
    }
}

/// The absolute names of `fields` and their nested fields, with `prefix` (the parent's).
pub(crate) fn absolute_names(fields: &[Field], prefix: &str, out: &mut Vec<String>) {
    for f in fields {
        let name = absolute(prefix, &f.name);
        absolute_names(&f.fields, &name, out);
        out.push(name);
    }
}

pub(crate) fn absolute(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}.{name}")
    }
}

fn constant(c: Term, position: Position) -> Result<TermMap, RmlError> {
    // Where only IRIs can be, a plain literal that is an IRI is taken as one (as RMLMapper
    // does with the `rr:constant "http://..."` some generators write).
    let c = match c {
        Term::Literal(l)
            if matches!(
                position,
                Position::Subject | Position::Predicate | Position::Graph | Position::Datatype
            ) && l.datatype() == oxigraph::model::vocab::xsd::STRING
                && oxiri::Iri::parse(l.value()).is_ok() =>
        {
            NamedNode::new_unchecked(l.value()).into()
        }
        c => c,
    };
    let (term_type, datatype, language) = match &c {
        Term::NamedNode(_) => (TermType::Iri, None, None),
        Term::BlankNode(_) if matches!(position, Position::Subject | Position::Object) => {
            (TermType::BlankNode, None, None)
        }
        Term::Literal(l)
            if matches!(
                position,
                Position::Object | Position::Language | Position::Join | Position::Input
            ) =>
        {
            (
                TermType::Literal,
                Some(l.datatype().into_owned()),
                l.language().map(str::to_owned),
            )
        }
        _ => {
            return Err(RmlError::Mapping(format!(
                "the constant {c} is not allowed in this position"
            )));
        }
    };
    Ok(TermMap {
        datatype,
        language,
        ..TermMap::new(Value::Constant(c), term_type)
    })
}

/// Splits a template into text and `{reference}` parts; `\{`, `\}` and `\\` are literal.
pub(crate) fn template(t: &str) -> Result<Vec<Part>, RmlError> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut chars = t.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(e @ ('{' | '}' | '\\')) => text.push(e),
                _ => return Err(RmlError::Mapping(format!("bad escape in template {t}"))),
            },
            '{' => {
                if !text.is_empty() {
                    parts.push(Part::Text(std::mem::take(&mut text)));
                }
                let mut reference = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some('\\') => match chars.next() {
                            Some(e) => reference.push(e),
                            None => {
                                return Err(RmlError::Mapping(format!("bad template {t}")));
                            }
                        },
                        Some('{') | None => {
                            return Err(RmlError::Mapping(format!(
                                "unbalanced braces in template {t}"
                            )));
                        }
                        Some(c) => reference.push(c),
                    }
                }
                if reference.is_empty() {
                    return Err(RmlError::Mapping(format!(
                        "empty reference in template {t}"
                    )));
                }
                parts.push(Part::Reference(reference));
            }
            '}' => {
                return Err(RmlError::Mapping(format!(
                    "unbalanced braces in template {t}"
                )));
            }
            c => text.push(c),
        }
    }
    if !text.is_empty() {
        parts.push(Part::Text(text));
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_split_into_parts() {
        assert_eq!(
            template(r"http://ex.org/{a}/x\{y\}/{b c}").unwrap(),
            vec![
                Part::Text("http://ex.org/".into()),
                Part::Reference("a".into()),
                Part::Text("/x{y}/".into()),
                Part::Reference("b c".into()),
            ]
        );
        assert!(template("{a").is_err());
        assert!(template("a}").is_err());
        assert!(template("{}").is_err());
    }
}
