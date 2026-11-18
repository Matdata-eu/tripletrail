use crate::sync::RwLockUnpoisoned;
use geo::{Coord, CoordsIter, Geometry, MapCoords};
use oxigraph::model::{Literal, NamedNodeRef, Term};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, OnceLock, RwLock};

pub use crate::solid::{Surface, SurfaceKind};

pub const WKT_LITERAL: NamedNodeRef<'static> =
    NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#wktLiteral");
pub const GEOJSON_LITERAL: NamedNodeRef<'static> =
    NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#geoJSONLiteral");
pub const GML_LITERAL: NamedNodeRef<'static> =
    NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#gmlLiteral");
pub const KML_LITERAL: NamedNodeRef<'static> =
    NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#kmlLiteral");
const CRS84: &str = "http://www.opengis.net/def/crs/OGC/1.3/CRS84";
const EPSG4326: &str = "http://www.opengis.net/def/crs/EPSG/0/4326";

/// Coordinate reference system of a geometry literal.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Crs {
    /// OGC CRS84 (longitude, latitude), the GeoSPARQL default.
    Crs84,
    /// EPSG:4326. Its axis order is latitude, longitude; coordinates are swapped to
    /// longitude, latitude on parse and back on output.
    Epsg4326,
    /// Any other CRS, kept as an IRI. Only planar operations apply to it for now.
    Other(String),
}

impl Crs {
    pub fn is_geographic(&self) -> bool {
        matches!(self, Self::Crs84 | Self::Epsg4326)
    }

    pub(crate) fn from_iri(iri: &str) -> Self {
        match iri {
            CRS84 => Self::Crs84,
            EPSG4326 | "http://www.opengis.net/def/crs/EPSG/0/4258" => Self::Epsg4326,
            _ => Self::Other(iri.to_owned()),
        }
    }
}

/// The CRS a CRS IRI denotes.
pub(crate) fn crs_from_iri(iri: &str) -> Crs {
    Crs::from_iri(iri)
}

/// A parsed geometry. Geographic coordinates are always stored as (longitude, latitude).
#[derive(Debug, Clone, PartialEq)]
pub struct Geom {
    pub geometry: Geometry<f64>,
    pub crs: Crs,
    /// The Z and M ordinates of the literal, if it has any. Computations run on the 2-D
    /// `geometry`; [`wkt_literal`] puts Z and M back on the result (see [`Zm`]).
    pub zm: Option<Arc<Zm>>,
    /// The true 3-D form of a polyhedral surface, TIN, triangle or solid (and of 3-D results
    /// such as a vertical face), which the (x, y)-keyed [`Zm`] cannot hold. `geometry` is then
    /// its footprint on the XY plane, which 2-D functions see; [`geom_literal`] writes this.
    pub surface: Option<Arc<Surface>>,
    /// The polygon's edges indexed for point-in-polygon and crossing tests, built on first use
    /// (see [`Geom::prepared`]).
    pub prepared: PreparedCell,
}

/// What is derived from a geometry and kept with it, built on first use: the
/// [`crate::fast::Prepared`] form of a polygonal geometry, and its metric area (#703). Equal to
/// any other (it is derived from the geometry), and cloned with it.
#[derive(Default, Clone)]
pub struct PreparedCell(OnceLock<Option<Arc<crate::fast::Prepared>>>, OnceLock<f64>);

impl PartialEq for PreparedCell {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl std::fmt::Debug for PreparedCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PreparedCell")
    }
}

impl Geom {
    /// Roughly the heap bytes this geometry holds besides its own struct: its coordinates,
    /// its Z and M values and its 3-D form (not the prepared form, built on use). A memory
    /// quota counts the store's parsed geometries this way (#225).
    pub fn heap_size(&self) -> usize {
        use geo::CoordsIter;
        let coords = self.geometry.coords_count();
        let zm = self.zm.as_ref().map_or(0, |z| z.values.len() * 48);
        // A 3-D form holds about the footprint's vertices again, with Z.
        let surface = self.surface.as_ref().map_or(0, |_| coords * 24);
        coords * size_of::<geo::Coord<f64>>() + zm + surface
    }

    pub fn new(geometry: Geometry<f64>, crs: Crs) -> Self {
        Self {
            geometry,
            crs,
            zm: None,
            surface: None,
            prepared: PreparedCell::default(),
        }
    }

    /// The edges of a polygonal geometry, indexed once and kept with the geometry, so a
    /// polygon tested against many points or lines is prepared once. `None` for other
    /// geometries.
    pub fn prepared(&self) -> Option<&crate::fast::Prepared> {
        self.prepared
            .0
            .get_or_init(|| crate::fast::Prepared::new(&self.geometry).map(Arc::new))
            .as_deref()
    }

    /// `geof:metricArea`: the geodesic area on the WGS 84 ellipsoid in m² for geographic
    /// coordinates, else the planar area. Computed once and kept with the geometry, so a
    /// store's geometries computed ahead on every core (see `tripletrail`'s `measures`) are not
    /// computed again row by row.
    pub fn metric_area(&self) -> f64 {
        *self.prepared.1.get_or_init(|| {
            if self.crs.is_geographic() {
                crate::functions::geodesic_area(&self.geometry)
            } else {
                use geo::Area;
                self.geometry.unsigned_area()
            }
        })
    }

    /// A 3-D geometry, with its footprint and the Z of the footprint's vertices.
    pub fn from_surface(surface: Surface, crs: Crs) -> Self {
        let geometry = surface.footprint();
        let zm = surface.has_z.then(|| {
            Arc::new(Zm::from_pairs(
                true,
                false,
                surface
                    .patches
                    .iter()
                    .flatten()
                    .flatten()
                    .map(|p| (Coord { x: p[0], y: p[1] }, (p[2], f64::NAN))),
            ))
        });
        Self {
            geometry,
            crs,
            zm,
            surface: Some(Arc::new(surface)),
            prepared: Default::default(),
        }
    }
}

/// The Z and M ordinates of a geometry, by (x, y) position.
///
/// Geometries are computed in 2-D. A result vertex gets the Z and M of the input vertex at the
/// same position, or, on an input segment, values interpolated along it (as PostGIS does for
/// `ST_LineSubstring` and `ST_LineInterpolatePoint`). A result with a vertex that matches no
/// input is written in 2-D.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Zm {
    pub has_z: bool,
    pub has_m: bool,
    values: HashMap<(u64, u64), (f64, f64)>,
}

impl Zm {
    fn key(c: Coord<f64>) -> (u64, u64) {
        // +0.0 and -0.0 are the same position.
        ((c.x + 0.0).to_bits(), (c.y + 0.0).to_bits())
    }

    /// Z and M (NaN where absent) of the vertex at `c`.
    pub fn at(&self, c: Coord<f64>) -> Option<(f64, f64)> {
        self.values.get(&Self::key(c)).copied()
    }

    /// Ordinates by position; the first value given for a position is kept.
    pub(crate) fn from_pairs(
        has_z: bool,
        has_m: bool,
        pairs: impl IntoIterator<Item = (Coord<f64>, (f64, f64))>,
    ) -> Self {
        let mut values = HashMap::new();
        for (c, v) in pairs {
            values.entry(Self::key(c)).or_insert(v);
        }
        Self {
            has_z,
            has_m,
            values,
        }
    }

    /// Ordinates of a geometry whose coordinates correspond one to one, in order, to `zm`.
    pub fn from_coords(
        geometry: &Geometry<f64>,
        has_z: bool,
        has_m: bool,
        zm: impl IntoIterator<Item = (f64, f64)>,
    ) -> Self {
        let mut values = HashMap::new();
        for (c, v) in geometry.coords_iter().zip(zm) {
            values.entry(Self::key(c)).or_insert(v);
        }
        Self {
            has_z,
            has_m,
            values,
        }
    }
}

thread_local! {
    /// Input geometries with Z or M of the function being evaluated (see [`with_zm_sources`]).
    static ZM_SOURCES: RefCell<Vec<Arc<Geom>>> = const { RefCell::new(Vec::new()) };
}

/// Runs `f` with `sources` as the geometries [`wkt_literal`] takes Z and M from.
pub(crate) fn with_zm_sources<R>(sources: Vec<Arc<Geom>>, f: impl FnOnce() -> R) -> R {
    // Restores the previous sources on drop, so a panic in `f` (caught by the function
    // table) leaves no stale sources behind for the next call on this thread.
    struct Restore(Option<Vec<Arc<Geom>>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(previous) = self.0.take() {
                let _ = ZM_SOURCES.try_with(|s| *s.borrow_mut() = previous);
            }
        }
    }
    let _restore = Restore(Some(
        ZM_SOURCES.with(|s| std::mem::replace(&mut *s.borrow_mut(), sources)),
    ));
    f()
}

/// Whether a geometry literal may have Z or M ordinates: a `Z`/`M`/`ZM` tag, or more than two
/// numbers in its first coordinate. Cheap, to skip the Z/M bookkeeping for 2-D literals.
pub(crate) fn may_have_zm(term: &Term) -> bool {
    let Term::Literal(l) = term else { return false };
    if l.datatype() != WKT_LITERAL {
        return false;
    }
    let mut text = l.value().trim_start();
    if let Some(rest) = text.strip_prefix('<') {
        match rest.find('>') {
            Some(end) => text = &rest[end + 1..],
            None => return false,
        }
    }
    let Some(open) = text.find('(') else {
        return false;
    };
    let tag = text[..open].trim_end();
    if tag.ends_with(['Z', 'M', 'z', 'm']) {
        return true;
    }
    let first = text[open..].trim_start_matches(|c: char| c == '(' || c.is_whitespace());
    let end = first.find([',', ')']).unwrap_or(first.len());
    first[..end].split_whitespace().count() > 2
}

static CACHE_ENABLED: AtomicBool = AtomicBool::new(true);
/// Shards of the parse cache, by text hash, so threads testing pairs in parallel rarely meet
/// on one lock. Keyed with [`crate::FastBuildHasher`]: hashing a whole polygon's text on every
/// lookup would cost more than many of the tests the lookup serves.
const SHARDS: usize = 64;
type Shard = RwLock<HashMap<Box<str>, Arc<Geom>, crate::FastBuildHasher>>;
static CACHE: LazyLock<Vec<Shard>> = LazyLock::new(|| {
    (0..SHARDS)
        .map(|_| RwLock::new(HashMap::default()))
        .collect()
});
const CACHE_LIMIT: usize = 2_000_000;

fn shard(value: &str) -> &'static Shard {
    use std::hash::BuildHasher;
    let h = crate::FastBuildHasher::default().hash_one(value);
    &CACHE[(h >> 58) as usize % SHARDS]
}

/// Turns the parsed-geometry cache on or off (used by benchmarks to measure parse cost).
pub fn set_cache_enabled(enabled: bool) {
    CACHE_ENABLED.store(enabled, Ordering::Relaxed);
    if !enabled {
        clear_cache();
    }
}

pub fn cache_enabled() -> bool {
    CACHE_ENABLED.load(Ordering::Relaxed)
}

pub fn clear_cache() {
    for s in CACHE.iter() {
        s.write_unpoisoned().clear();
    }
}

/// Parses a geometry literal: `geo:wktLiteral`, `geo:geoJSONLiteral`, `geo:gmlLiteral` or
/// `geo:kmlLiteral`. Returns `None` for
/// any other term or an invalid literal.
/// Whether `datatype` is one of the geometry literal datatypes the parser reads
/// (`geo:wktLiteral`, `geo:geoJSONLiteral`, `geo:gmlLiteral`, `geo:kmlLiteral`, H3).
pub fn is_geometry_datatype(datatype: &str) -> bool {
    [
        WKT_LITERAL,
        GEOJSON_LITERAL,
        GML_LITERAL,
        KML_LITERAL,
        crate::dggs::H3_LITERAL,
    ]
    .iter()
    .any(|d| d.as_str() == datatype)
}

/// Where a store keeps its geometry literals parsed: the parsed geometry of a literal it
/// holds, or `None` for a literal it does not hold (a query constant, a computed value).
pub type GeometrySource = dyn Fn(&Literal) -> Option<Arc<Geom>> + Send + Sync;

thread_local! {
    /// The source [`parse_wkt_literal`] asks first, set by [`with_geometry_source`].
    static SOURCE: std::cell::Cell<Option<*const GeometrySource>> =
        const { std::cell::Cell::new(None) };
}

/// Runs `f` with [`parse_wkt_literal`] asking `source` before parsing or using the parse
/// cache, on this thread: functions evaluated on a store then use the geometries the store
/// keeps parsed (by term id) instead of parsing the literals' text a second time.
pub fn with_geometry_source<R>(source: &GeometrySource, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<*const GeometrySource>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SOURCE.with(|s| s.set(self.0));
        }
    }
    // SAFETY (for the read in `from_source`): the pointer is only read while `f` runs, during
    // which `source` is borrowed; `Restore` puts the previous one back, even on a panic.
    let ptr: *const GeometrySource = source;
    let _restore = Restore(SOURCE.with(|s| s.replace(Some(ptr))));
    f()
}

fn from_source(literal: &Literal) -> Option<Arc<Geom>> {
    let ptr = SOURCE.with(std::cell::Cell::get)?;
    // SAFETY: set by `with_geometry_source`, whose borrow of the source outlives this call.
    unsafe { (*ptr)(literal) }
}

pub fn parse_wkt_literal(term: &Term) -> Option<Arc<Geom>> {
    if let Term::Literal(l) = term
        && let Some(g) = from_source(l)
    {
        return Some(g);
    }
    let (parse, value) = geometry_parser(term)?;
    if !cache_enabled() {
        return parse(value).map(Arc::new);
    }
    let shard = shard(value);
    if let Some(g) = shard.read_unpoisoned().get(value) {
        return Some(Arc::clone(g));
    }
    let geom = Arc::new(parse(value)?);
    let mut cache = shard.write_unpoisoned();
    if cache.len() >= CACHE_LIMIT / SHARDS {
        cache.clear();
    }
    cache.insert(value.into(), Arc::clone(&geom));
    Some(geom)
}

/// [`parse_wkt_literal`] without the parse cache, for callers that keep the parsed geometry
/// themselves (a store). The cache would hold a second copy of the literal's text.
pub fn parse_wkt_literal_uncached(term: &Term) -> Option<Arc<Geom>> {
    let (parse, value) = geometry_parser(term)?;
    parse(value).map(Arc::new)
}

type Parser = fn(&str) -> Option<Geom>;

/// The parser for a geometry literal's datatype, and the literal's lexical form.
fn geometry_parser(term: &Term) -> Option<(Parser, &str)> {
    let Term::Literal(literal) = term else {
        return None;
    };
    let parse: Parser = if literal.datatype() == WKT_LITERAL {
        parse_wkt
    } else if literal.datatype() == GEOJSON_LITERAL {
        parse_geojson
    } else if literal.datatype() == GML_LITERAL {
        crate::gml::parse_gml
    } else if literal.datatype() == KML_LITERAL {
        crate::gml::parse_kml
    } else if literal.datatype() == crate::dggs::H3_LITERAL {
        crate::dggs::parse_h3
    } else {
        return None;
    };
    Some((parse, literal.value()))
}

/// Stands for a `NaN` ordinate while the WKT parser, which does not read `nan`, runs.
const NAN_MARK: f64 = f64::MIN;

fn parse_wkt(value: &str) -> Option<Geom> {
    let value = value.trim();
    let (crs, wkt) = if let Some(rest) = value.strip_prefix('<') {
        let end = rest.find('>')?;
        (Crs::from_iri(&rest[..end]), rest[end + 1..].trim_start())
    } else {
        (Crs::Crs84, value)
    };
    // An empty WKT literal (or only a CRS) is an empty geometry (GeoSPARQL 1.0,
    // requirement 13).
    if wkt.is_empty() {
        return Some(Geom::new(
            Geometry::GeometryCollection(geo::GeometryCollection(Vec::new())),
            crs,
        ));
    }
    if let Some(surface) = crate::solid::parse_wkt(wkt) {
        let surface = surface?;
        let surface = if crs == Crs::Epsg4326 {
            surface.map(|p| [p[1], p[0], p[2]])
        } else {
            surface
        };
        return Some(Geom::from_surface(surface, crs));
    }
    // A GeoPackage export writes an unknown M (or Z) as NaN, which GEOS reads.
    let marked;
    let has_nan = wkt.bytes().any(|b| b == b'n' || b == b'N') && has_nan(wkt);
    let wkt = if has_nan {
        marked = mark_nan(wkt);
        marked.as_str()
    } else {
        wkt
    };
    let parsed: wkt::Wkt<f64> = wkt.parse().ok()?;
    let zm = ordinates(&parsed, has_nan)?;
    let geometry = Geometry::<f64>::try_from(parsed).ok()?;
    // A number too large for an f64 parses as infinity, which no WKT can write back. A signed
    // `+nan` or `-nan` reaches the WKT parser as a number and is NaN here: an X or Y that is
    // not a number makes the literal invalid, as a bare `nan` does.
    if geometry
        .coords_iter()
        .any(|c| !c.x.is_finite() || !c.y.is_finite())
        || zm
            .as_ref()
            .is_some_and(|(_, _, v)| v.iter().any(|(z, m)| z.is_infinite() || m.is_infinite()))
    {
        return None;
    }
    // The conversion from the WKT parser's types reuses their allocations, which are sized for
    // its larger coordinates (with Z and M): a line string would keep about four times the
    // memory its points need. Mapping the coordinates copies them into vectors of exact size.
    let geometry = if crs == Crs::Epsg4326 {
        swap_axes(&geometry)
    } else {
        geometry.map_coords(|c| c)
    };
    if let Some((true, false, values)) = &zm {
        // Two vertices at one (x, y) with different Z (a vertical segment or face, a point
        // above another): keep the true 3-D form, which the (x, y)-keyed Z cannot hold.
        if z_conflicts(&geometry, values) {
            let z: Vec<f64> = values.iter().map(|v| v.0).collect();
            if let Some(s) = crate::solid::from_geometry(&geometry, &z) {
                return Some(Geom::from_surface(s, crs));
            }
        }
    }
    let zm =
        zm.map(|(has_z, has_m, values)| Arc::new(Zm::from_coords(&geometry, has_z, has_m, values)));
    Some(Geom {
        geometry,
        crs,
        zm,
        surface: None,
        prepared: Default::default(),
    })
}

/// Whether two coordinates at the same (x, y) have different Z.
fn z_conflicts(geometry: &Geometry<f64>, values: &[(f64, f64)]) -> bool {
    let mut seen: HashMap<(u64, u64), u64> = HashMap::new();
    geometry.coords_iter().zip(values).any(|(c, v)| {
        let z = (v.0 + 0.0).to_bits();
        *seen.entry(Zm::key(c)).or_insert(z) != z
    })
}

fn has_nan(wkt: &str) -> bool {
    wkt.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',')
        .any(|w| w.eq_ignore_ascii_case("nan"))
}

fn mark_nan(wkt: &str) -> String {
    let mut out = String::with_capacity(wkt.len() + 32);
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if word.eq_ignore_ascii_case("nan") {
            out.push_str(&format!("{NAN_MARK:e}"));
        } else {
            out.push_str(word);
        }
        word.clear();
    };
    for c in wkt.chars() {
        if c.is_whitespace() || c == '(' || c == ')' || c == ',' {
            flush(&mut word, &mut out);
            out.push(c);
        } else {
            word.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// The Z and M of every coordinate, in order, if the geometry has any; `None` (an invalid
/// literal) if an X or Y is NaN.
#[allow(clippy::type_complexity)]
fn ordinates(w: &wkt::Wkt<f64>, has_nan: bool) -> Option<Option<(bool, bool, Vec<(f64, f64)>)>> {
    use geo_traits::{CoordTrait, GeometryTrait};
    let (has_z, has_m) = match w.dim() {
        geo_traits::Dimensions::Xyz => (true, false),
        geo_traits::Dimensions::Xym => (false, true),
        geo_traits::Dimensions::Xyzm => (true, true),
        _ => (false, false),
    };
    if !has_z && !has_m && !has_nan {
        return Some(None);
    }
    let mut all = Vec::new();
    let mut ok = true;
    let unmark = |v: f64| if v == NAN_MARK { f64::NAN } else { v };
    let mut see = |c: &wkt::types::Coord<f64>| {
        ok &= c.x() != NAN_MARK && c.y() != NAN_MARK;
        all.push((c.z.map_or(f64::NAN, unmark), c.m.map_or(f64::NAN, unmark)));
    };
    fn coords(w: &wkt::Wkt<f64>, f: &mut dyn FnMut(&wkt::types::Coord<f64>)) {
        use wkt::Wkt::*;
        match w {
            Point(p) => p.coord().into_iter().for_each(&mut *f),
            LineString(l) => l.coords().iter().for_each(&mut *f),
            Polygon(p) => p.rings().iter().flat_map(|r| r.coords()).for_each(&mut *f),
            MultiPoint(m) => m
                .points()
                .iter()
                .filter_map(|p| p.coord())
                .for_each(&mut *f),
            MultiLineString(m) => m
                .line_strings()
                .iter()
                .flat_map(|l| l.coords())
                .for_each(&mut *f),
            MultiPolygon(m) => m
                .polygons()
                .iter()
                .flat_map(|p| p.rings())
                .flat_map(|r| r.coords())
                .for_each(&mut *f),
            GeometryCollection(c) => c.geometries().iter().for_each(|g| coords(g, f)),
        }
    }
    coords(w, &mut see);
    ok.then_some(())?;
    Some((has_z || has_m).then_some((has_z, has_m, all)))
}

/// GeoJSON (RFC 7946): always CRS84; a third coordinate is ignored here.
fn parse_geojson(value: &str) -> Option<Geom> {
    let json: serde_json::Value = serde_json::from_str(value).ok()?;
    Some(Geom::new(geojson_geometry(&json)?, Crs::Crs84))
}

pub(crate) fn geojson_geometry(v: &serde_json::Value) -> Option<Geometry<f64>> {
    use geo::{LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon};
    fn coord(v: &serde_json::Value) -> Option<Coord<f64>> {
        let a = v.as_array()?;
        Some(Coord {
            x: a.first()?.as_f64()?,
            y: a.get(1)?.as_f64()?,
        })
    }
    fn line(v: &serde_json::Value) -> Option<LineString<f64>> {
        v.as_array()?
            .iter()
            .map(coord)
            .collect::<Option<Vec<_>>>()
            .map(LineString)
    }
    fn polygon(v: &serde_json::Value) -> Option<Polygon<f64>> {
        let mut rings = v.as_array()?.iter().map(line);
        let exterior = rings.next()??;
        Some(Polygon::new(exterior, rings.collect::<Option<Vec<_>>>()?))
    }
    let obj = v.as_object()?;
    match obj.get("type")?.as_str()? {
        "Feature" => geojson_geometry(obj.get("geometry")?),
        "GeometryCollection" => Some(Geometry::GeometryCollection(geo::GeometryCollection(
            obj.get("geometries")?
                .as_array()?
                .iter()
                .map(geojson_geometry)
                .collect::<Option<_>>()?,
        ))),
        kind => {
            let c = obj.get("coordinates")?;
            fn list<T>(
                c: &serde_json::Value,
                f: fn(&serde_json::Value) -> Option<T>,
            ) -> Option<Vec<T>> {
                c.as_array()?.iter().map(f).collect()
            }
            Some(match kind {
                "Point" => Geometry::Point(Point(coord(c)?)),
                "LineString" => Geometry::LineString(line(c)?),
                "Polygon" => Geometry::Polygon(polygon(c)?),
                "MultiPoint" => Geometry::MultiPoint(MultiPoint(
                    list(c, coord)?.into_iter().map(Point).collect(),
                )),
                "MultiLineString" => Geometry::MultiLineString(MultiLineString(list(c, line)?)),
                "MultiPolygon" => Geometry::MultiPolygon(MultiPolygon(list(c, polygon)?)),
                _ => return None,
            })
        }
    }
}

fn swap_axes(g: &Geometry<f64>) -> Geometry<f64> {
    g.map_coords(|Coord { x, y }| Coord { x: y, y: x })
}

/// Serialises a geometry as a `geo:wktLiteral` in the given CRS. Inside a function call
/// whose inputs have Z or M, the result gets them back where its vertices allow (see [`Zm`]).
pub fn wkt_literal(geometry: &Geometry<f64>, crs: &Crs) -> Term {
    let zm = ZM_SOURCES.with(|s| {
        let sources = s.borrow();
        if sources.is_empty() {
            None
        } else {
            recover_zm(geometry, &sources)
        }
    });
    write_wkt(geometry, crs, zm.as_ref())
}

/// Serialises a geometry with its own Z and M ordinates.
pub fn geom_literal(g: &Geom) -> Term {
    if let Some(s) = &g.surface {
        return Literal::new_typed_literal(
            with_crs(&g.crs, surface_wkt(s, &g.crs, false)),
            WKT_LITERAL,
        )
        .into();
    }
    write_wkt(&g.geometry, &g.crs, g.zm.as_deref())
}

/// The WKT of a 3-D geometry, in the axis order of `crs`. `postgis` writes a solid as PostGIS
/// prints it (a `POLYHEDRALSURFACE`), otherwise as SFCGAL's `SOLID`, which keeps the flag.
pub(crate) fn surface_wkt(s: &Surface, crs: &Crs, postgis: bool) -> String {
    if *crs == Crs::Epsg4326 {
        s.map(|p| [p[1], p[0], p[2]]).to_wkt(postgis)
    } else {
        s.to_wkt(postgis)
    }
}

fn with_crs(crs: &Crs, body: String) -> String {
    match crs {
        Crs::Crs84 => body,
        Crs::Epsg4326 => format!("<{EPSG4326}> {body}"),
        Crs::Other(iri) => format!("<{iri}> {body}"),
    }
}

fn write_wkt(geometry: &Geometry<f64>, crs: &Crs, zm: Option<&Zm>) -> Term {
    let swapped;
    let geometry = if *crs == Crs::Epsg4326 {
        swapped = swap_axes(geometry);
        &swapped
    } else {
        geometry
    };
    let body = match zm {
        // The ordinates are keyed by (lon, lat); look them up before the swap.
        Some(zm) if zm.has_z || zm.has_m => {
            let lookup = |c: Coord<f64>| {
                let c = if *crs == Crs::Epsg4326 {
                    Coord { x: c.y, y: c.x }
                } else {
                    c
                };
                zm.at(c)
            };
            match to_wkt_zm(geometry, zm.has_z, zm.has_m, &lookup) {
                Some(w) => w.to_string(),
                None => plain_wkt(geometry),
            }
        }
        _ => plain_wkt(geometry),
    };
    Literal::new_typed_literal(with_crs(crs, body), WKT_LITERAL).into()
}

/// The 2-D WKT of a geometry. The `wkt` crate panics on an empty polygon inside a
/// multipolygon and writes an empty line or ring inside its parent as `()`, which does not
/// parse; none of them has a WKT of its own there, so they are left out.
fn plain_wkt(geometry: &Geometry<f64>) -> String {
    fn has_empty_member(g: &Geometry<f64>) -> bool {
        match g {
            Geometry::MultiPolygon(m) => m
                .0
                .iter()
                .any(|p| p.exterior().0.is_empty() || p.interiors().iter().any(|r| r.0.is_empty())),
            Geometry::Polygon(p) => {
                !p.exterior().0.is_empty() && p.interiors().iter().any(|r| r.0.is_empty())
            }
            Geometry::MultiLineString(m) => m.0.iter().any(|l| l.0.is_empty()),
            Geometry::GeometryCollection(c) => c.0.iter().any(has_empty_member),
            _ => false,
        }
    }
    fn without_empty_members(g: &Geometry<f64>) -> Geometry<f64> {
        match g {
            Geometry::MultiPolygon(m) => Geometry::MultiPolygon(geo::MultiPolygon(
                m.0.iter()
                    .filter(|p| !p.exterior().0.is_empty())
                    .map(without_empty_holes)
                    .collect(),
            )),
            Geometry::Polygon(p) => Geometry::Polygon(without_empty_holes(p)),
            Geometry::MultiLineString(m) => Geometry::MultiLineString(geo::MultiLineString(
                m.0.iter().filter(|l| !l.0.is_empty()).cloned().collect(),
            )),
            Geometry::GeometryCollection(c) => Geometry::GeometryCollection(
                geo::GeometryCollection(c.0.iter().map(without_empty_members).collect()),
            ),
            g => g.clone(),
        }
    }
    fn without_empty_holes(p: &geo::Polygon<f64>) -> geo::Polygon<f64> {
        geo::Polygon::new(
            p.exterior().clone(),
            p.interiors()
                .iter()
                .filter(|r| !r.0.is_empty())
                .cloned()
                .collect(),
        )
    }
    if has_empty_member(geometry) {
        crate::wkt_out::string(&without_empty_members(geometry))
    } else {
        crate::wkt_out::string(geometry)
    }
}

/// Most result vertices off the input vertices that are looked up on input segments.
const MAX_INTERPOLATED: usize = 1024;

/// Z and M for the vertices of `geometry` from the input geometries, or `None` if some
/// vertex matches none of them.
fn recover_zm(geometry: &Geometry<f64>, sources: &[Arc<Geom>]) -> Option<Zm> {
    let first = sources.iter().find_map(|s| s.zm.as_deref())?;
    let (has_z, has_m) = (first.has_z, first.has_m);
    let mut values = HashMap::new();
    let mut misses = 0;
    for c in geometry.coords_iter() {
        let key = Zm::key(c);
        if values.contains_key(&key) {
            continue;
        }
        let found = sources
            .iter()
            .find_map(|s| s.zm.as_deref()?.at(c))
            .or_else(|| {
                misses += 1;
                if misses > MAX_INTERPOLATED {
                    return None;
                }
                sources.iter().find_map(|s| interpolate(s, c))
            })?;
        values.insert(key, found);
    }
    Some(Zm {
        has_z,
        has_m,
        values,
    })
}

/// Z and M at `c` on a segment of `source`, interpolated between its ends.
fn interpolate(source: &Geom, c: Coord<f64>) -> Option<(f64, f64)> {
    let zm = source.zm.as_deref()?;
    let scale = c.x.abs().max(c.y.abs()).max(1.0);
    let tolerance = scale * 1e-9;
    let mut lines = Vec::new();
    linestrings(&source.geometry, &mut lines);
    for (a, b) in lines
        .iter()
        .flat_map(|l| l.0.windows(2).map(|w| (w[0], w[1])))
    {
        let d = b - a;
        let len2 = d.x * d.x + d.y * d.y;
        if len2 == 0.0 {
            continue;
        }
        let t = ((c.x - a.x) * d.x + (c.y - a.y) * d.y) / len2;
        if !(-1e-12..=1.0 + 1e-12).contains(&t) {
            continue;
        }
        let p = a + d * t;
        if (p.x - c.x).abs() > tolerance || (p.y - c.y).abs() > tolerance {
            continue;
        }
        let (za, ma) = zm.at(a)?;
        let (zb, mb) = zm.at(b)?;
        return Some((za + (zb - za) * t, ma + (mb - ma) * t));
    }
    None
}

fn linestrings<'a>(
    g: &'a Geometry<f64>,
    out: &mut Vec<std::borrow::Cow<'a, geo::LineString<f64>>>,
) {
    use std::borrow::Cow;
    match g {
        Geometry::Line(l) => out.push(Cow::Owned(geo::LineString::new(vec![l.start, l.end]))),
        Geometry::LineString(l) => out.push(Cow::Borrowed(l)),
        Geometry::MultiLineString(m) => out.extend(m.0.iter().map(Cow::Borrowed)),
        Geometry::Polygon(p) => {
            out.push(Cow::Borrowed(p.exterior()));
            out.extend(p.interiors().iter().map(Cow::Borrowed));
        }
        Geometry::MultiPolygon(m) => m.0.iter().for_each(|p| {
            out.push(Cow::Borrowed(p.exterior()));
            out.extend(p.interiors().iter().map(Cow::Borrowed));
        }),
        Geometry::GeometryCollection(c) => c.0.iter().for_each(|g| linestrings(g, out)),
        Geometry::Rect(r) => out.push(Cow::Owned(r.to_polygon().exterior().clone())),
        Geometry::Triangle(t) => out.push(Cow::Owned(t.to_polygon().exterior().clone())),
        Geometry::Point(_) | Geometry::MultiPoint(_) => {}
    }
}

/// The geometry as WKT with Z and/or M from `lookup`; `None` if a vertex has none.
fn to_wkt_zm(
    g: &Geometry<f64>,
    has_z: bool,
    has_m: bool,
    lookup: &dyn Fn(Coord<f64>) -> Option<(f64, f64)>,
) -> Option<wkt::Wkt<f64>> {
    use wkt::types::{self as w, Dimension};
    let dim = match (has_z, has_m) {
        (true, true) => Dimension::XYZM,
        (true, false) => Dimension::XYZ,
        (false, true) => Dimension::XYM,
        (false, false) => Dimension::XY,
    };
    let coord = |c: Coord<f64>| -> Option<w::Coord<f64>> {
        let (z, m) = lookup(c)?;
        Some(w::Coord {
            x: c.x,
            y: c.y,
            z: has_z.then_some(z),
            m: has_m.then_some(m),
        })
    };
    let line = |l: &geo::LineString<f64>| -> Option<w::LineString<f64>> {
        Some(w::LineString::new(
            l.0.iter().map(|c| coord(*c)).collect::<Option<_>>()?,
            dim,
        ))
    };
    // An empty ring or member has no WKT inside its parent (`((()))` does not parse), and it
    // adds nothing to the geometry, so it is left out, as `plain_wkt` does.
    let polygon = |p: &geo::Polygon<f64>| -> Option<w::Polygon<f64>> {
        if p.exterior().0.is_empty() {
            return Some(w::Polygon::new(Vec::new(), dim));
        }
        let mut rings = vec![line(p.exterior())?];
        for r in p.interiors().iter().filter(|r| !r.0.is_empty()) {
            rings.push(line(r)?);
        }
        Some(w::Polygon::new(rings, dim))
    };
    Some(match g {
        Geometry::Point(p) => wkt::Wkt::Point(w::Point::new(Some(coord(p.0)?), dim)),
        Geometry::Line(l) => {
            wkt::Wkt::LineString(line(&geo::LineString::new(vec![l.start, l.end]))?)
        }
        Geometry::LineString(l) => wkt::Wkt::LineString(line(l)?),
        Geometry::Polygon(p) => wkt::Wkt::Polygon(polygon(p)?),
        Geometry::MultiPoint(m) => wkt::Wkt::MultiPoint(w::MultiPoint::new(
            m.0.iter()
                .map(|p| Some(w::Point::new(Some(coord(p.0)?), dim)))
                .collect::<Option<_>>()?,
            dim,
        )),
        Geometry::MultiLineString(m) => wkt::Wkt::MultiLineString(w::MultiLineString::new(
            m.0.iter()
                .filter(|l| !l.0.is_empty())
                .map(line)
                .collect::<Option<_>>()?,
            dim,
        )),
        Geometry::MultiPolygon(m) => wkt::Wkt::MultiPolygon(w::MultiPolygon::new(
            m.0.iter()
                .filter(|p| !p.exterior().0.is_empty())
                .map(polygon)
                .collect::<Option<_>>()?,
            dim,
        )),
        Geometry::GeometryCollection(c) => {
            wkt::Wkt::GeometryCollection(w::GeometryCollection::new(
                c.0.iter()
                    .map(|g| to_wkt_zm(g, has_z, has_m, lookup))
                    .collect::<Option<_>>()?,
                dim,
            ))
        }
        Geometry::Rect(r) => wkt::Wkt::Polygon(polygon(&r.to_polygon())?),
        Geometry::Triangle(t) => wkt::Wkt::Polygon(polygon(&t.to_polygon())?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(s: &str) -> Term {
        Literal::new_typed_literal(s, WKT_LITERAL).into()
    }

    /// Found by the `geo_literals` fuzz target: empty members wrote `((()))` or `()`.
    #[test]
    fn empty_members_write_wkt_that_parses_again() {
        for (input, written) in [
            ("MULTIPOLYGON Z(EMPTY)", "MULTIPOLYGON Z EMPTY"),
            ("MULTIPOLYGON M(EMPTY)", "MULTIPOLYGON M EMPTY"),
            ("MULTIPOLYGON ZM(EMPTY)", "MULTIPOLYGON ZM EMPTY"),
            (
                "MULTIPOLYGON Z(EMPTY, ((0 0 1, 1 0 1, 1 1 1, 0 0 1)))",
                "MULTIPOLYGON Z(((0 0 1,1 0 1,1 1 1,0 0 1)))",
            ),
            (
                "POLYGON Z((0 0 1, 1 0 1, 1 1 1, 0 0 1), EMPTY)",
                "POLYGON Z((0 0 1,1 0 1,1 1 1,0 0 1))",
            ),
            ("MULTILINESTRING Z(EMPTY)", "MULTILINESTRING Z EMPTY"),
            (
                "MULTILINESTRING Z(EMPTY, (0 0 1, 1 1 2))",
                "MULTILINESTRING Z((0 0 1,1 1 2))",
            ),
            ("MULTILINESTRING(EMPTY)", "MULTILINESTRING EMPTY"),
            (
                "MULTILINESTRING(EMPTY, (0 0, 1 1))",
                "MULTILINESTRING((0 0,1 1))",
            ),
            (
                "GEOMETRYCOLLECTION Z(MULTIPOLYGON Z(EMPTY))",
                "GEOMETRYCOLLECTION Z(MULTIPOLYGON Z EMPTY)",
            ),
        ] {
            let g = parse_wkt_literal_uncached(&lit(input)).expect(input);
            let wkt = geom_literal(&g);
            assert_eq!(wkt, lit(written), "{input}");
            assert!(
                parse_wkt_literal_uncached(&wkt).is_some(),
                "{input} wrote {wkt}"
            );
        }
    }

    #[test]
    fn parsed_lines_keep_little_spare_capacity() {
        let points: Vec<String> = (0..100).map(|i| format!("{i} {i}")).collect();
        let wkt = format!("MULTILINESTRING(({}))", points.join(", "));
        let g = parse_wkt_literal_uncached(&lit(&wkt)).unwrap();
        let Geometry::MultiLineString(m) = &g.geometry else {
            panic!()
        };
        // The WKT parser's vectors hold 3-4 times the space; the parsed ones must not.
        for l in &m.0 {
            assert!(
                l.0.capacity() < 2 * l.0.len(),
                "{} for {}",
                l.0.capacity(),
                l.0.len()
            );
        }
    }

    #[test]
    fn coordinates_beyond_f64_are_invalid() {
        for wkt in [
            "POINT(1e400 1)",
            "POINT(1 -1e400)",
            "POINT Z(1 1 1e400)",
            "LINESTRING(0 0, 1e999 1)",
            "POINT(+inf 1)",
        ] {
            assert!(parse_wkt_literal_uncached(&lit(wkt)).is_none(), "{wkt}");
        }
    }

    /// Found by the geo_literals fuzz target: a signed NaN parsed, and its WKT (`NaN`) did not.
    #[test]
    fn signed_nan_x_or_y_is_invalid() {
        for wkt in [
            "MultiPoint(0\n9,1\t9,1\t9,1\t+NAN)",
            "LineString(+NAN\t7)",
            "POINT(-nan 1)",
            "POINT(1 +NaN)",
            "POINT(1 nan)",
        ] {
            assert!(parse_wkt_literal_uncached(&lit(wkt)).is_none(), "{wkt}");
        }
        // An unknown Z stays allowed, signed or not, and writes back as WKT that parses.
        for wkt in ["POINT Z(1 2 NaN)", "POINT Z(1 2 +nan)"] {
            let g = parse_wkt_literal_uncached(&lit(wkt)).expect(wkt);
            assert!(
                parse_wkt_literal_uncached(&geom_literal(&g)).is_some(),
                "{wkt}"
            );
        }
    }

    #[test]
    fn parses_default_crs() {
        let g = parse_wkt_literal(&lit("POINT(10 59)")).unwrap();
        assert_eq!(g.crs, Crs::Crs84);
        assert_eq!(g.geometry, Geometry::Point(geo::point!(x: 10.0, y: 59.0)));
    }

    #[test]
    fn swaps_epsg4326_axes() {
        let g = parse_wkt_literal(&lit(
            "<http://www.opengis.net/def/crs/EPSG/0/4326> POINT(59 10)",
        ))
        .unwrap();
        assert_eq!(g.crs, Crs::Epsg4326);
        assert_eq!(g.geometry, Geometry::Point(geo::point!(x: 10.0, y: 59.0)));
        let out = wkt_literal(&g.geometry, &g.crs);
        assert!(matches!(&out, Term::Literal(l) if l.value().ends_with("POINT(59 10)")));
    }

    #[test]
    fn parses_geojson() {
        let t: Term = Literal::new_typed_literal(
            r#"{"type":"LineString","coordinates":[[10,59],[11,60,5]]}"#,
            GEOJSON_LITERAL,
        )
        .into();
        let g = parse_wkt_literal(&t).unwrap();
        assert_eq!(g.crs, Crs::Crs84);
        assert_eq!(
            g.geometry,
            Geometry::LineString(geo::LineString::from(vec![(10.0, 59.0), (11.0, 60.0)]))
        );
    }

    #[test]
    fn rejects_other_literals() {
        assert!(parse_wkt_literal(&Literal::from("POINT(1 2)").into()).is_none());
        assert!(parse_wkt_literal(&lit("NOT WKT")).is_none());
    }
}
