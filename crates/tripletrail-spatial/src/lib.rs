//! Spatial index and spatial FILTER rewriting.
//!
//! [`SpatialIndex`] is an R-tree over every distinct `geo:asWKT` literal in the store, with the
//! parsed geometry kept next to its bounding box. [`rewrite_query`] finds FILTERs that test a
//! store geometry against a constant one (`geof:sfIntersects(?wkt, "POLYGON(...)"^^geo:wktLiteral)`,
//! `geof:distance(?wkt, ...) < 500`, ...) and adds a `VALUES ?wkt { ... }` with the geometries
//! that pass the test, found through the index. The FILTER stays in place, so results are the
//! same as without the rewrite; the evaluator just no longer calls the function on every row.

mod cost;
mod index;
mod rewrite;
mod sweep;

pub use index::{IndexStats, IndexedPredicates, SpatialIndex};
pub use rewrite::{
    BUILT_IN_SERVICES, Bounded, Decision, IdSide, Knn, RewriteStats, SideGeometry, SpatialContext,
    binds_through, knn, rewrite_query, rewrite_update,
};
