//! R2RML and RML mappings: a mapping document is read into a [`Mapping`] (the plan: per
//! triples map a logical source, an iterator, term maps and joins), then [`run`] over its
//! sources, giving quads.
//!
//! ```
//! use tripletrail_rml::{Mapping, MemoryLoader, run};
//!
//! let mapping = Mapping::parse_turtle(r#"
//!     @prefix rr: <http://www.w3.org/ns/r2rml#> .
//!     @prefix rml: <http://semweb.mmlab.be/ns/rml#> .
//!     @prefix ql: <http://semweb.mmlab.be/ns/ql#> .
//!     <#People> rml:logicalSource [ rml:source "people.json" ;
//!                                   rml:referenceFormulation ql:JSONPath ;
//!                                   rml:iterator "$.people[*]" ] ;
//!       rr:subjectMap [ rr:template "http://ex.org/person/{id}" ] ;
//!       rr:predicateObjectMap [ rr:predicate <http://ex.org/name> ;
//!                               rr:objectMap [ rml:reference "name" ] ] .
//! "#, Some("file:///mappings/")).unwrap();
//! let loader = MemoryLoader::from([("people.json", r#"{"people":[{"id":1,"name":"Ada"}]}"#)]);
//! let quads = run(&mapping, &loader).unwrap();
//! assert_eq!(quads.len(), 1);
//! assert_eq!(quads[0].to_string(), r#"<http://ex.org/person/1> <http://ex.org/name> "Ada""#);
//! ```
//!
//! Vocabularies: R2RML (`http://www.w3.org/ns/r2rml#`), the RML that RMLMapper and most
//! existing mappings use (`http://semweb.mmlab.be/ns/rml#`, with `ql:` reference
//! formulations) and the RML of the W3C KG-Construct group (`http://w3id.org/rml/`), Core,
//! IO, functions (RML-FNML), collections (RML-CC) and logical views (RML-LV). They can be mixed in one document.
//! YARRRML documents are read with [`Mapping::parse_yarrrml`]: they are translated into the
//! RML they stand for.
//!
//! Sources: JSON, with a JSONPath iterator and JSONPath references ([`jsonpath`]); CSV or
//! TSV files with a header row, whose references are column names (a column the header does
//! not name is an error); and XML, with an XPath iterator and XPath references relative to
//! each record, in the XPath subset of Facade-X's `xml.path` (`/`, `//`, names with their
//! prefixes as written in the file, `@attr`, `text()`, `*`, `.`, `..` and simple predicates).
//! File sources follow RML-IO's `rml:compression` (`gzip`, `zip`, `targz`, `tarxz`),
//! `rml:encoding` (UTF-8, UTF-16) and `rml:null`. Without `rml:null`, an empty CSV cell is no
//! value in legacy RML, as in RMLMapper, and an empty string in RML-IO, which also refuses a
//! CSV row with more or fewer cells than the header. Files are read by a
//! [`Loader`], which decides where they may come from: [`FsLoader`] reads them under the same
//! rules as Facade-X sources. Tables and SQL queries (R2RML logical tables, RML sources on a
//! `d2rq:Database`) are read by [`Loader::table`]: with the `postgres` feature,
//! `DatabaseLoader` reads them from PostgreSQL.
//!
//! Functions (RML-FNML): a term map may give the value of a function, with
//! `rml:functionExecution` (inputs by `rml:inputValueMap` or `rml:inputValue`, output by
//! `rml:return`) or legacy `fnml:functionValue`, and may hold an `rml:condition` that must be
//! true for it to make terms. The functions are built in: common GREL string functions
//! (`toUpperCase`, `string_substring`, `string_replace`, `string_split`, `escape`, ...),
//! IDLab's (`equal`, `notEqual`, `isNull`, `trueCondition`, `concat`, `toUpperCaseURL`, ...)
//! and TripleTrail's geometry functions in `http://w3id.org/tripletrail/fx#`: `point` (`x`, `y`,
//! optional `z` and `crs` such as `EPSG:25833`) and `wkt` (`wkt`, `crs`), which give
//! `geo:wktLiteral`s. An unknown function, parameter or output is a mapping error. A triples
//! map without a logical source runs once, on an empty record.
//!
//! Logical targets (RML-IO `rml:logicalTarget` on any term map): [`run_targets`] sorts the
//! quads by the targets of the maps that made them, and [`write_target`] writes a target's
//! file in its serialization (N-Quads, N-Triples, Turtle, TriG, N3, RDF/XML, JSON-LD or
//! RDF/JSON), encoding and compression; [`read_target`] reads one back. [`run`] ignores
//! targets and gives every quad.
//!
//! Collections and containers (RML-CC): a term map with `rml:gather` and `rml:gatherAs`
//! makes an `rdf:List`, `rdf:Bag`, `rdf:Seq` or `rdf:Alt` of its items' values (appended, or
//! one per combination with `rml:cartesianProduct`), in the graphs of the triples using it.
//! Without a value it is a new blank node each time; with one (a template, constant or
//! reference) the members of every record naming the same collection in the same graph are
//! gathered into it. An empty one is made only with `rml:allowEmptyListAndContainer`.

#[cfg(feature = "postgres")]
mod database;
pub mod db_schema;
pub mod direct;
mod explain;
mod functions;
mod io;
pub mod jsonpath;
mod mapping;
mod run;
mod sparql;
#[cfg(feature = "postgres")]
mod tls;
mod views;
#[cfg(feature = "postgres")]
mod wkb;
mod yarrrml;

#[cfg(feature = "postgres")]
pub use database::DatabaseLoader;
pub use explain::{Explanation, MapReport, ViewReport, explain};
pub use io::{read_target, write_target};
pub use mapping::{
    Collection, Database, Encoding, Field, FieldKind, Formulation, FunctionCall, Gather,
    LogicalSource, LogicalTarget, LogicalView, Mapping, ObjectMap, Part, PredicateObjectMap,
    Quoted, SourceIo, SourceRef, TermMap, TermType, TriplesMap, Value, ViewJoin,
};
pub use run::{
    Cell, FsLoader, Loader, MemoryLoader, Table, TargetedQuads, run, run_targets, run_with,
};
pub use sparql::as_sparql;

#[derive(Debug, thiserror::Error)]
pub enum RmlError {
    /// The mapping document is not valid R2RML or RML.
    #[error("RML mapping: {0}")]
    Mapping(String),
    /// The mapping document is not valid RDF.
    #[error("RML: cannot parse the mapping document: {0}")]
    Syntax(#[from] oxigraph::io::RdfSyntaxError),
    /// A source could not be read or parsed.
    #[error("RML source {location}: {message}")]
    Source { location: String, message: String },
    /// A part of RML that is not supported yet.
    #[error("RML: {0} is not supported yet")]
    Unsupported(String),
    /// A record gave a value that cannot be used: an invalid IRI or language tag, or a
    /// reference to a JSON array or object.
    #[error("RML data error: {0}")]
    Data(String),
    /// An iterator or reference that is not valid JSONPath, or uses a part not supported.
    #[error("RML: JSONPath {path}: {message}")]
    Path { path: String, message: String },
}
