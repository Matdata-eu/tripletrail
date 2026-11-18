//! pyoxigraph's RDF model, I/O, dataset and query result classes, so that `tripletrail` builds
//! them directly from the store instead of through pyoxigraph (vendored, see README.md).
#![allow(
    clippy::all,
    clippy::pedantic,
    dead_code,
    unused_imports,
    clippy::trivially_copy_pass_by_ref,
    clippy::unused_self,
    clippy::useless_conversion
)]

pub mod dataset;
pub mod errors;
pub mod io;
pub mod model;
pub mod sparql;
