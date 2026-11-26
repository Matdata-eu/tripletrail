//! SHACL Core and SHACL-SPARQL validation, and SHACL-AF rules, built for checking many focus
//! nodes fast against a store.
//!
//! Shapes are read once from a shapes graph ([`Shapes::compile`]) into a compact form; a
//! [`Validator`] then checks focus nodes with direct lookups in the data graph (a graph of the
//! tripletrail store or of Oxigraph's store, or an in-memory graph, through [`DataGraph`]). It answers
//! whether a node conforms ([`Validator::conforms`]), or why it does not
//! ([`Validator::report`], one [`ValidationResult`] per constraint failure, written as a W3C
//! validation report by [`ReportWriter`]).
//!
//! Supported: every SHACL Core target (`sh:targetNode`, `sh:targetClass` and implicit class
//! targets, `sh:targetSubjectsOf`, `sh:targetObjectsOf`), every property path (predicate,
//! sequence, inverse, alternative, zero-or-more, one-or-more, zero-or-one) and every Core
//! constraint component: `sh:class`, `sh:datatype`, `sh:nodeKind`, `sh:minCount`,
//! `sh:maxCount`, `sh:minInclusive`, `sh:maxInclusive`, `sh:minExclusive`, `sh:maxExclusive`,
//! `sh:minLength`, `sh:maxLength`, `sh:pattern` (with `sh:flags`), `sh:languageIn`,
//! `sh:uniqueLang`, `sh:equals`, `sh:disjoint`, `sh:lessThan`, `sh:lessThanOrEquals`, `sh:not`,
//! `sh:and`, `sh:or`, `sh:xone`, `sh:node`, `sh:property`, `sh:qualifiedValueShape` (with
//! `sh:qualifiedMinCount`, `sh:qualifiedMaxCount`, `sh:qualifiedValueShapesDisjoint`),
//! `sh:closed` (with `sh:ignoredProperties`), `sh:hasValue` and `sh:in`; and `sh:deactivated`.
//!
//! SHACL-SPARQL: SPARQL-based constraints (`sh:sparql` with `sh:select`, `sh:prefixes` and
//! `sh:deactivated`; `$this`, `$PATH`, `$currentShape` and `$shapesGraph`), SPARQL-based
//! constraint components (`sh:ConstraintComponent`, `sh:parameter` with `sh:optional`,
//! `sh:validator` ASK validators, `sh:nodeValidator` and `sh:propertyValidator` SELECT
//! validators), and from SHACL Advanced Features SPARQL-based targets (`sh:target` with
//! `sh:select`, and SPARQL-based target types with parameters). Queries run on Oxigraph's
//! evaluator ([`Evaluator`]) with the data graph as default graph and the shapes graph as the
//! named graph `$shapesGraph`; [`Validator::prefetch`] checks many focus nodes with one query,
//! binding them with a `VALUES` block. Queries that break the pre-binding rules (`MINUS`,
//! `SERVICE`, `VALUES`, a nested `SELECT` without `$this`, `AS` assigning a pre-bound
//! variable) are refused when the shapes are compiled, as the SHACL spec asks.
//!
//! SHACL-AF rules (`sh:rule`: `sh:TripleRule` with node expressions, `sh:SPARQLRule`;
//! `sh:order`, `sh:condition`, `sh:deactivated`) are run by [`Inference`] to a fixpoint,
//! returning the triples they infer without writing them anywhere.
//!
//! [`compat`] compiles vehicle compatibility rules (a small vocabulary, [`compat::NS`]) with
//! one vehicle's values into plain SHACL shapes.

pub mod compat;
mod delta;
mod graph;
mod inferred;
mod path;
mod report;
mod rules;
mod shapes;
mod sparql;
mod validate;
mod value;

pub use graph::{DataGraph, Edited, StoreGraph, Triple3, Union};
pub use inferred::Inferred;
pub use path::Path;
pub use report::{FailingFocusNodes, ReportWriter, ShapeCount, ValidationResult};
pub use rules::{
    Inference, Origin, PLAN_EDGES, Plan, PlanEdge, PlanGroup, PlanRule, PlanSink, RuleRun,
};
pub use shapes::{Shape, ShapeId, Shapes};
pub use sparql::{Evaluator, prebind};
pub use validate::Validator;
pub use value::{compare, has_datatype, number};

#[derive(Debug, thiserror::Error)]
pub enum ShaclError {
    /// The shapes graph is not well-formed SHACL (or uses something not supported).
    #[error("shacl: {0}")]
    Shape(String),
    /// Compatibility rules that cannot be compiled.
    #[error("compatibility rules: {0}")]
    Rules(String),
    /// A SHACL-SPARQL query failed.
    #[error("shacl: query failed: {0}")]
    Query(String),
}

/// Vocabulary.
pub mod vocab {
    /// TripleTrail's own SHACL terms (the shape statistics of a validation report).
    pub mod ttsh {
        use oxigraph::model::NamedNodeRef;
        pub const NS: &str = "http://w3id.org/tripletrail/shacl#";
        pub const SHAPE_STATISTICS: NamedNodeRef<'static> =
            NamedNodeRef::new_unchecked("http://w3id.org/tripletrail/shacl#shapeStatistics");
        pub const SHAPE: NamedNodeRef<'static> =
            NamedNodeRef::new_unchecked("http://w3id.org/tripletrail/shacl#shape");
        pub const FOCUS_NODES: NamedNodeRef<'static> =
            NamedNodeRef::new_unchecked("http://w3id.org/tripletrail/shacl#focusNodes");
        pub const VALUE_NODES: NamedNodeRef<'static> =
            NamedNodeRef::new_unchecked("http://w3id.org/tripletrail/shacl#valueNodes");
        pub const DISTINCT_FOCUS_NODES: NamedNodeRef<'static> =
            NamedNodeRef::new_unchecked("http://w3id.org/tripletrail/shacl#distinctFocusNodes");
        pub const FAILING_FOCUS_NODES: NamedNodeRef<'static> =
            NamedNodeRef::new_unchecked("http://w3id.org/tripletrail/shacl#failingFocusNodes");
    }

    pub mod sh {
        use oxigraph::model::NamedNodeRef;
        pub const NS: &str = "http://www.w3.org/ns/shacl#";
        macro_rules! terms {
            ($($name:ident = $local:literal,)*) => {
                $(pub const $name: NamedNodeRef<'static> =
                    NamedNodeRef::new_unchecked(concat!("http://www.w3.org/ns/shacl#", $local));)*
            };
        }
        terms! {
            NODE_SHAPE = "NodeShape",
            PROPERTY_SHAPE = "PropertyShape",
            TARGET_NODE = "targetNode",
            TARGET_CLASS = "targetClass",
            TARGET_SUBJECTS_OF = "targetSubjectsOf",
            TARGET_OBJECTS_OF = "targetObjectsOf",
            PATH = "path",
            INVERSE_PATH = "inversePath",
            ALTERNATIVE_PATH = "alternativePath",
            ZERO_OR_MORE_PATH = "zeroOrMorePath",
            ONE_OR_MORE_PATH = "oneOrMorePath",
            ZERO_OR_ONE_PATH = "zeroOrOnePath",
            DEACTIVATED = "deactivated",
            CLASS = "class",
            DATATYPE = "datatype",
            NODE_KIND = "nodeKind",
            IRI = "IRI",
            BLANK_NODE = "BlankNode",
            LITERAL = "Literal",
            BLANK_NODE_OR_IRI = "BlankNodeOrIRI",
            BLANK_NODE_OR_LITERAL = "BlankNodeOrLiteral",
            IRI_OR_LITERAL = "IRIOrLiteral",
            MIN_COUNT = "minCount",
            MAX_COUNT = "maxCount",
            MIN_EXCLUSIVE = "minExclusive",
            MIN_INCLUSIVE = "minInclusive",
            MAX_EXCLUSIVE = "maxExclusive",
            MAX_INCLUSIVE = "maxInclusive",
            MIN_LENGTH = "minLength",
            MAX_LENGTH = "maxLength",
            PATTERN = "pattern",
            FLAGS = "flags",
            LANGUAGE_IN = "languageIn",
            UNIQUE_LANG = "uniqueLang",
            EQUALS = "equals",
            DISJOINT = "disjoint",
            LESS_THAN = "lessThan",
            LESS_THAN_OR_EQUALS = "lessThanOrEquals",
            NOT = "not",
            AND = "and",
            OR = "or",
            XONE = "xone",
            NODE = "node",
            PROPERTY = "property",
            QUALIFIED_VALUE_SHAPE = "qualifiedValueShape",
            QUALIFIED_MIN_COUNT = "qualifiedMinCount",
            QUALIFIED_MAX_COUNT = "qualifiedMaxCount",
            QUALIFIED_VALUE_SHAPES_DISJOINT = "qualifiedValueShapesDisjoint",
            CLOSED = "closed",
            IGNORED_PROPERTIES = "ignoredProperties",
            HAS_VALUE = "hasValue",
            IN = "in",
            SPARQL = "sparql",
            TARGET = "target",
            SELECT = "select",
            ASK = "ask",
            CONSTRUCT = "construct",
            PREFIXES = "prefixes",
            DECLARE = "declare",
            PREFIX = "prefix",
            NAMESPACE = "namespace",
            CONSTRAINT_COMPONENT = "ConstraintComponent",
            PARAMETER = "parameter",
            OPTIONAL = "optional",
            VALIDATOR = "validator",
            NODE_VALIDATOR = "nodeValidator",
            PROPERTY_VALIDATOR = "propertyValidator",
            RULE = "rule",
            TRIPLE_RULE = "TripleRule",
            SPARQL_RULE = "SPARQLRule",
            SUBJECT = "subject",
            PREDICATE = "predicate",
            OBJECT = "object",
            ORDER = "order",
            CONDITION = "condition",
            THIS = "this",
            FILTER_SHAPE = "filterShape",
            NODES = "nodes",
            INTERSECTION = "intersection",
            UNION = "union",
            MESSAGE = "message",
            SEVERITY = "severity",
            VIOLATION = "Violation",
            VALIDATION_REPORT = "ValidationReport",
            VALIDATION_RESULT = "ValidationResult",
            CONFORMS = "conforms",
            RESULT = "result",
            FOCUS_NODE = "focusNode",
            RESULT_PATH = "resultPath",
            VALUE = "value",
            SOURCE_SHAPE = "sourceShape",
            SOURCE_CONSTRAINT_COMPONENT = "sourceConstraintComponent",
            SOURCE_CONSTRAINT = "sourceConstraint",
            RESULT_SEVERITY = "resultSeverity",
            RESULT_MESSAGE = "resultMessage",
            DETAIL = "detail",
            SPARQL_CONSTRAINT_COMPONENT = "SPARQLConstraintComponent",
        }
    }

    pub mod rdf {
        use oxigraph::model::NamedNodeRef;
        pub const TYPE: NamedNodeRef<'static> =
            NamedNodeRef::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#type");
        pub const FIRST: NamedNodeRef<'static> =
            NamedNodeRef::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#first");
        pub const REST: NamedNodeRef<'static> =
            NamedNodeRef::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#rest");
        pub const NIL: NamedNodeRef<'static> =
            NamedNodeRef::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#nil");
    }

    pub mod rdfs {
        use oxigraph::model::NamedNodeRef;
        pub const CLASS: NamedNodeRef<'static> =
            NamedNodeRef::new_unchecked("http://www.w3.org/2000/01/rdf-schema#Class");
        pub const SUB_CLASS_OF: NamedNodeRef<'static> =
            NamedNodeRef::new_unchecked("http://www.w3.org/2000/01/rdf-schema#subClassOf");
        pub const COMMENT: NamedNodeRef<'static> =
            NamedNodeRef::new_unchecked("http://www.w3.org/2000/01/rdf-schema#comment");
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_sparql;
