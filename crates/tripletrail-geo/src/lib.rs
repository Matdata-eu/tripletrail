//! GeoSPARQL 1.1 (`geof:`) and PostGIS-style (`st:ST_*`) functions, registered on Oxigraph's
//! [`SparqlEvaluator`] through its custom function hook.
//!
//! Geometries are `geo:wktLiteral`s. Parsed geometries are cached by literal value so a
//! literal is parsed once per process rather than once per function call; this stands in for
//! the pre-parsed geometry store of the architecture until the own storage layer exists.

mod crs;
mod csg;
mod dggs;
mod fast;
pub mod formats;
mod functions;
mod geosparql11;
mod gml;
mod hash;
mod hull3d;
mod jena;
mod letters;
mod literal;
pub mod mvt;
mod national;
mod pipeline;
mod postgis;
mod postgis_3d;
mod postgis_p2;
mod postgis_processing;
mod postgis_shapes;
mod postgis_zm;
mod predicate;
pub mod rail;
mod setops;
pub mod sets;
mod skeleton;
mod solid;
pub mod srs;
pub mod sync;
mod wkt_out;
/// Cooperative cancellation, shared with the routing algorithms.
pub use tripletrail_routing::cancel;

pub use crs::{crs_arg, is_supported as is_supported_crs, transform, transform_geom};
pub use dggs::H3_LITERAL;
pub use functions::{
    Func, FunctionTable, GEOF, ST, function, function_table, register, register_with,
    register_with_source,
};
pub use geosparql11::aggregate_functions;
pub use hash::{FastBuildHasher, SampledHasher};
pub use jena::{JENA_EARTH_RADIUS, metres_in_unit};
pub use literal::{
    Crs, GEOJSON_LITERAL, GML_LITERAL, Geom, GeometrySource, KML_LITERAL, PreparedCell, Surface,
    SurfaceKind, WKT_LITERAL, Zm, cache_enabled, clear_cache, geom_literal, is_geometry_datatype,
    parse_wkt_literal, parse_wkt_literal_uncached, set_cache_enabled, with_geometry_source,
    wkt_literal,
};
pub use postgis_processing::{wkb, wkb_literal};
pub use predicate::{GeomPredicate, geom_predicate};
pub use rail::RAIL;
