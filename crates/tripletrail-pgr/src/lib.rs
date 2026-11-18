//! pgRouting functions as a SPARQL `SERVICE`.
//!
//! ```sparql
//! PREFIX pgr: <http://w3id.org/tripletrail/pgr#>
//! SELECT * WHERE {
//!   SERVICE <http://w3id.org/tripletrail/pgr> {
//!     [] pgr:function pgr:pgr_dijkstra ;
//!        pgr:start_vid <urn:v:1> ; pgr:end_vid <urn:v:9> ; pgr:directed true ;
//!        pgr:seq ?seq ; pgr:node ?node ; pgr:edge ?edge ; pgr:cost ?cost ; pgr:agg_cost ?agg_cost .
//!     # The edges_sql analogue: any sub-select projecting ?id ?source ?target ?cost [?reverse_cost]
//!     { SELECT ?id ?source ?target ?cost ?reverse_cost WHERE { ... } }
//!   }
//! }
//! ```
//!
//! Triples whose object is a constant are parameters (`pgr:start_vid`, `pgr:directed`, ...);
//! triples whose object is a variable bind result columns, named exactly as pgRouting's.
//! Instead of a sub-select, `pgr:network <iri>` names a network described in the store with
//! `<iri> pgr:edgeQuery "SELECT ..."`; its graph is built once and reused until the data changes.
//!
//! Functions beyond pgRouting (constrained, time-dependent and hierarchical routing, route
//! geometries, topology checks) are named in the [`RAIL`] namespace (`rail:route`, ...) and run
//! in the same SERVICE; their parameters and columns may be written with `pgr:` or `rail:`.

mod constraint;
mod geometry;
mod network;
mod pareto;
mod request;
mod service;
mod shapes;
mod tables;
mod time;
mod topology;
mod vrp;

pub use network::{
    BuildStats, Constrained, DEFAULT_NETWORK_BUDGET, Extras, FilterReads, Network, NetworkCache,
    NetworkInfo, predicates_read,
};
pub use request::{PgrFunction, Request};
pub use service::{PgrError, PgrService, QueryStats};

/// Vocabulary namespace for parameters and result columns.
pub const PGR: &str = "http://w3id.org/tripletrail/pgr#";
/// Namespace of the functions beyond pgRouting.
pub const RAIL: &str = "http://w3id.org/tripletrail/rail#";
/// The SERVICE IRI the handler is registered under.
pub const PGR_SERVICE: &str = "http://w3id.org/tripletrail/pgr";
