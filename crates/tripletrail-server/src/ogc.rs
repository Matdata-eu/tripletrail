//! `/{ds}/ogc`: OGC API – Features Part 1 (Core, GeoJSON, OpenAPI 3.0) over a dataset (#138).
//! The collections are the node shapes the `ogc-features` reader reads (see [`crate::shapes`]):
//! one per node shape with a target class and a geometry way. A feature is an instance of the
//! class; its geometry is the first way's value as GeoJSON (CRS84), its properties the values
//! of the shape's property shapes, under their keys.

use crate::View;
use crate::admin::Entry;
use crate::{HttpError, Params, Reply, State};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tripletrail::oxigraph::model::vocab::{rdf, rdfs, xsd};
use tripletrail::oxigraph::model::{
    Graph, NamedNode, NamedOrBlankNode, NamedOrBlankNodeRef, Term, TermRef,
};
use tripletrail::oxigraph::sparql::QueryResults;
use tripletrail::shapes::Reader;

mod cql2;
mod html;

const SH: &str = "http://www.w3.org/ns/shacl#";
const TTS: &str = "http://w3id.org/tripletrail/shapes#";
const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
const GEOF: &str = "http://www.opengis.net/def/function/geosparql/";
const WKT: &str = "http://www.opengis.net/ont/geosparql#wktLiteral";
const CRS84: &str = "http://www.opengis.net/def/crs/OGC/1.3/CRS84";
const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 10_000;
const GEOJSON: &str = "application/geo+json";
/// JSON-FG (OGC Features and Geometries JSON, #497): GeoJSON with `time`, `place` (a geometry
/// in another CRS) and `conformsTo`.
pub(crate) const JSON_FG: &str = "application/vnd.ogc.fg+json";
const JSON_FG_CORE: &str = "http://www.opengis.net/spec/json-fg-1/1.0/conf/core";
const JSON: &str = "application/json";
const OPENAPI: &str = "application/vnd.oai.openapi+json;version=3.0";

const CONFORMANCE: &[&str] = &[
    "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/core",
    "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/geojson",
    JSON_FG_CORE,
    "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/html",
    "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/oas30",
    "http://www.opengis.net/spec/ogcapi-features-2/1.0/conf/crs",
    "http://www.opengis.net/spec/ogcapi-features-3/1.0/conf/queryables",
    "http://www.opengis.net/spec/ogcapi-features-3/1.0/conf/filter",
    "http://www.opengis.net/spec/ogcapi-features-3/1.0/conf/features-filter",
    "http://www.opengis.net/spec/cql2/1.0/conf/cql2-text",
    "http://www.opengis.net/spec/cql2/1.0/conf/cql2-json",
    "http://www.opengis.net/spec/cql2/1.0/conf/basic-cql2",
    "http://www.opengis.net/spec/cql2/1.0/conf/advanced-comparison-operators",
    "http://www.opengis.net/spec/cql2/1.0/conf/case-insensitive-comparison",
    "http://www.opengis.net/spec/cql2/1.0/conf/property-property",
    "http://www.opengis.net/spec/cql2/1.0/conf/basic-spatial-functions",
    "http://www.opengis.net/spec/cql2/1.0/conf/basic-spatial-functions-plus",
    "http://www.opengis.net/spec/cql2/1.0/conf/spatial-functions",
    "http://www.opengis.net/spec/cql2/1.0/conf/temporal-functions",
];
const EPSG4326: &str = "http://www.opengis.net/def/crs/EPSG/0/4326";
const EPSG3857: &str = "http://www.opengis.net/def/crs/EPSG/0/3857";
/// Earth radius of Web Mercator, in metres.
const MERCATOR_R: f64 = 6_378_137.0;

/// The query parameters of the items, besides the property filters.
const ITEMS_PARAMS: &[&str] = &[
    "f",
    "limit",
    "offset",
    "bbox",
    "bbox-crs",
    "crs",
    "datetime",
    "geometries",
    "timeout",
    "filter",
    "filter-lang",
    "filter-crs",
    "area",
    "rel",
    "dist",
];

/// A CRS features are given or asked for in (OGC API – Features Part 2).
#[derive(Clone, Copy, Debug, PartialEq)]
enum OutCrs {
    /// Longitude, latitude: the default.
    Crs84,
    /// Latitude, longitude, as EPSG defines its axes.
    Epsg4326,
    /// Web Mercator metres, x then y.
    WebMercator,
}

impl OutCrs {
    const ALL: [OutCrs; 3] = [OutCrs::Crs84, OutCrs::Epsg4326, OutCrs::WebMercator];

    fn iri(self) -> &'static str {
        match self {
            OutCrs::Crs84 => CRS84,
            OutCrs::Epsg4326 => EPSG4326,
            OutCrs::WebMercator => EPSG3857,
        }
    }

    /// The CRS parameter `name` (`crs`, `bbox-crs`): CRS84 when absent.
    fn of(params: &Params, name: &str) -> Result<Self, HttpError> {
        let Some(asked) = params.get(name) else {
            return Ok(OutCrs::Crs84);
        };
        let iri = asked.trim_start_matches('<').trim_end_matches('>');
        OutCrs::ALL
            .into_iter()
            .find(|c| c.iri() == iri)
            .ok_or_else(|| {
                HttpError::bad_request(format!(
                    "Unsupported {name} {asked}: one of {}",
                    OutCrs::ALL.map(OutCrs::iri).join(", ")
                ))
            })
    }

    /// A longitude and latitude in this CRS's axis order.
    fn project(self, lon: f64, lat: f64) -> (f64, f64) {
        match self {
            OutCrs::Crs84 => (lon, lat),
            OutCrs::Epsg4326 => (lat, lon),
            OutCrs::WebMercator => {
                let lat = lat.clamp(-85.051_128_779_806_59, 85.051_128_779_806_59);
                (
                    MERCATOR_R * lon.to_radians(),
                    MERCATOR_R * lat.to_radians().tan().asinh(),
                )
            }
        }
    }

    /// A point of this CRS as longitude and latitude.
    fn unproject(self, a: f64, b: f64) -> (f64, f64) {
        match self {
            OutCrs::Crs84 => (a, b),
            OutCrs::Epsg4326 => (b, a),
            OutCrs::WebMercator => (
                (a / MERCATOR_R).to_degrees(),
                (b / MERCATOR_R).sinh().atan().to_degrees(),
            ),
        }
    }
}

/// `geometry`, GeoJSON in CRS84, with its coordinates in `crs`.
fn reproject(geometry: &mut Value, crs: OutCrs) {
    fn walk(v: &mut Value, crs: OutCrs) {
        let Value::Array(items) = v else { return };
        if let [Value::Number(a), Value::Number(b), ..] = items.as_slice()
            && let (Some(a), Some(b)) = (a.as_f64(), b.as_f64())
        {
            let (x, y) = crs.project(a, b);
            if let (Some(x), Some(y)) = (
                serde_json::Number::from_f64(x),
                serde_json::Number::from_f64(y),
            ) {
                items[0] = Value::Number(x);
                items[1] = Value::Number(y);
            }
            return;
        }
        for item in items {
            walk(item, crs);
        }
    }
    if crs == OutCrs::Crs84 {
        return;
    }
    if let Some(coordinates) = geometry.get_mut("coordinates") {
        walk(coordinates, crs);
    }
    if let Some(Value::Array(parts)) = geometry.get_mut("geometries") {
        for part in parts {
            reproject(part, crs);
        }
    }
}

/// A feature collection: what it reads of the dataset.
#[derive(Clone)]
pub(crate) struct Collection {
    pub(crate) id: String,
    pub(crate) class: NamedNode,
    pub(crate) title: String,
    pub(crate) description: Option<String>,
    /// The geometry ways (see [`crate::geometry_ways`]).
    pub(crate) geometry: crate::geometry_ways::Ways,
    /// `(key, SPARQL path)` of the property shapes.
    properties: Vec<(String, String)>,
    /// Keys of the properties items can be filtered on (`?key=value`): those with an
    /// `tts:facet`.
    filters: Vec<String>,
    /// The properties with an `tts:facet`, in shape order.
    pub(crate) facets: Vec<Facet>,
    /// The JSON Schema of each property, for `/queryables`.
    schemas: HashMap<String, Value>,
    /// `tts:minZoom`: the lowest zoom a map shows the collection at.
    pub(crate) min_zoom: Option<u32>,
    /// `tts:drawOrder`: higher draws on top of the collections with the same geometry kind.
    pub(crate) draw_order: Option<i64>,
    /// The node shape's `sh:order`: where a map's legend lists the collection.
    pub(crate) order: Option<i64>,
    /// `tts:color` (`#rrggbb`): the collection's colour on a map.
    pub(crate) color: Option<String>,
    /// `tts:instances`: how many features the profile counted (generated shapes).
    pub(crate) instances: Option<u64>,
    /// The paths `tts:time` marks, which `datetime` filters on.
    time: Time,
    /// The map views (`tts:MapView`) that show the collection, with what each sets on it.
    pub(crate) views: Vec<ViewPart>,
    /// The dataset's base IRI (`--base-iri`, when it has one): the ids of the features under
    /// it are their IRIs' local part (#497). Set by [`find`].
    id_base: Option<String>,
    /// What the map takes out of the labels it shows: the `tts:labelStrip` patterns of all the
    /// shapes, one of them (#260). The same in every collection of a dataset.
    pub(crate) label_strip: Option<regex::Regex>,
}

/// A facet of a collection (a property shape with `tts:facet`): what a map counts and filters
/// its features by.
#[derive(Clone)]
pub(crate) struct Facet {
    pub(crate) key: String,
    pub(crate) title: String,
    /// The property's path, in SPARQL.
    pub(crate) path: String,
    /// `tts:Facet-Range` (numbers and dates) rather than `tts:Facet-Values`.
    pub(crate) range: bool,
    /// `tts:Facet-Text`: searched for text its values contain.
    pub(crate) text: bool,
    /// `sh:datatype`: what a range's bounds are read as.
    pub(crate) datatype: Option<String>,
    /// `qudt:hasUnit`, as a symbol (`km/h`): the unit of a range's numbers.
    pub(crate) unit: Option<String>,
    /// `tts:colorClasses`: a range's classes when a map is coloured by it, low to high, each
    /// up to a number (inclusive) but the last, with an optional label.
    pub(crate) classes: Vec<(Option<f64>, Option<String>)>,
    /// `tts:colorBy`: the facet's place among those a map's "Colour by" offers first.
    pub(crate) color_by: Option<i64>,
    /// `tts:select`: a SPARQL group pattern that gives the feature (`$this`) its value
    /// (`?value`), in place of the path: a value computed from the data, such as whether a
    /// track meets a requirement.
    pub(crate) select: Option<String>,
}

impl Facet {
    /// A pattern that binds the facet's values of `f` to `v` (both `?name`): the path as a
    /// triple pattern, or the facet's `tts:select` with its other variables renamed so that
    /// they meet nothing outside it. It goes where `f` is bound, so that an `OPTIONAL` in it
    /// keeps the features without a value.
    pub(crate) fn bind(&self, f: &str, v: &str) -> String {
        match &self.select {
            None => format!(" {f} {} {v} .", self.path),
            // The shape check refused a pattern that cannot be inlined: this one fails anyway.
            Some(select) => tripletrail::shapes::inline_select(select, f, v)
                .unwrap_or_else(|_| format!(" {select} .")),
        }
    }
}

/// What a map view (`tts:MapView`) sets on one collection it shows.
#[derive(Clone)]
pub(crate) struct ViewPart {
    /// The view's `tts:key`.
    pub(crate) key: String,
    pub(crate) title: String,
    pub(crate) description: Option<String>,
    pub(crate) order: Option<i64>,
    /// The view's filters on the collection's facets: `(facet key, choice)`.
    pub(crate) filters: Vec<(String, ViewChoice)>,
    /// The facet the view colours the collection by.
    pub(crate) color: Option<String>,
    /// The facets the view lists first in the collection's filter panel, in its order.
    pub(crate) facets: Vec<String>,
    /// Whether the view shows the collection; one it hides has only its listed facets, for
    /// when it is shown by hand.
    pub(crate) shows: bool,
}

/// A map view's filter on one facet.
#[derive(Clone)]
pub(crate) enum ViewChoice {
    /// The values kept (`sh:in`), as the map's filters take them: an IRI or a literal's text.
    Values(Vec<String>),
    /// A range facet's bounds (`sh:minInclusive`, `sh:maxInclusive`).
    Range(Option<String>, Option<String>),
}

/// The map views of `shapes` (`tts:MapView`, not `sh:deactivated`), with what each sets on
/// each collection whose class it shows: its filters, colour and listed facets on the facets
/// the collection has, the others left out.
fn add_views(shapes: &Graph, collections: &mut [Collection]) {
    let sh = |local: &str| NamedNode::new_unchecked(format!("{SH}{local}"));
    let tts = |local: &str| NamedNode::new_unchecked(format!("{TTS}{local}"));
    let text = |s: NamedOrBlankNodeRef<'_>, p: &NamedNode| -> Option<String> {
        match shapes.object_for_subject_predicate(s, p)? {
            TermRef::Literal(l) => Some(l.value().to_owned()),
            _ => None,
        }
    };
    let value = |t: TermRef<'_>| match t {
        TermRef::NamedNode(n) => Some(n.as_str().to_owned()),
        TermRef::Literal(l) => Some(l.value().to_owned()),
        _ => None,
    };
    let mut views: Vec<NamedOrBlankNode> = shapes
        .subjects_for_predicate_object(rdf::TYPE, &tts("MapView"))
        .map(NamedOrBlankNodeRef::into_owned)
        .collect();
    views.sort_by_key(ToString::to_string);
    let mut keys = HashSet::new();
    for view in &views {
        let view = view.as_ref();
        if text(view, &sh("deactivated")).as_deref() == Some("true") {
            continue;
        }
        let key = text(view, &tts("key")).unwrap_or_else(|| match view {
            NamedOrBlankNodeRef::NamedNode(n) => local_name(n.as_str()).to_owned(),
            NamedOrBlankNodeRef::BlankNode(b) => b.as_str().to_owned(),
        });
        // The first view of a key.
        if !keys.insert(key.clone()) {
            continue;
        }
        let classes: Vec<TermRef<'_>> = shapes
            .objects_for_subject_predicate(view, &tts("showsClass"))
            .collect();
        let filters: Vec<(String, NamedOrBlankNodeRef<'_>)> = shapes
            .objects_for_subject_predicate(view, &tts("filter"))
            .filter_map(|f| match f {
                TermRef::NamedNode(n) => Some(NamedOrBlankNodeRef::from(n)),
                TermRef::BlankNode(b) => Some(NamedOrBlankNodeRef::from(b)),
                _ => None,
            })
            .filter_map(|f| Some((text(f, &tts("key"))?, f)))
            .collect();
        let color = text(view, &tts("colorFacet"));
        let listed: Vec<String> = shapes
            .object_for_subject_predicate(view, &tts("listsFacets"))
            .and_then(|head| rdf_list(shapes, head))
            .unwrap_or_default()
            .into_iter()
            .filter_map(|t| match t {
                TermRef::Literal(l) => Some(l.value().to_owned()),
                _ => None,
            })
            .collect();
        for c in collections.iter_mut() {
            let facet = |k: &str| c.facets.iter().find(|f| f.key == k);
            let lists: Vec<String> = listed
                .iter()
                .filter(|k| facet(k).is_some_and(|f| !f.text))
                .cloned()
                .collect();
            if !classes.contains(&TermRef::from(c.class.as_ref())) {
                if !lists.is_empty() {
                    c.views.push(ViewPart {
                        key: key.clone(),
                        title: String::new(),
                        description: None,
                        order: None,
                        filters: Vec::new(),
                        color: None,
                        facets: lists,
                        shows: false,
                    });
                }
                continue;
            }
            let mut chosen = Vec::new();
            for (k, f) in &filters {
                let Some(facet) = facet(k) else { continue };
                let choice = if facet.range {
                    let (min, max) = (
                        shapes
                            .object_for_subject_predicate(*f, &sh("minInclusive"))
                            .and_then(value),
                        shapes
                            .object_for_subject_predicate(*f, &sh("maxInclusive"))
                            .and_then(value),
                    );
                    if min.is_none() && max.is_none() {
                        continue;
                    }
                    ViewChoice::Range(min, max)
                } else if facet.text {
                    continue;
                } else {
                    let values: Vec<String> = shapes
                        .object_for_subject_predicate(*f, &sh("in"))
                        .and_then(|head| rdf_list(shapes, head))
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(value)
                        .collect();
                    if values.is_empty() {
                        continue;
                    }
                    ViewChoice::Values(values)
                };
                chosen.push((k.clone(), choice));
            }
            c.views.push(ViewPart {
                key: key.clone(),
                title: text(view, &rdfs::LABEL.into_owned()).unwrap_or_else(|| key.clone()),
                description: text(view, &rdfs::COMMENT.into_owned()),
                order: text(view, &sh("order")).and_then(|o| o.trim().parse().ok()),
                filters: chosen,
                color: color.clone().filter(|k| facet(k).is_some_and(|f| !f.text)),
                facets: lists,
                shows: true,
            });
        }
    }
}

/// QUDT's unit IRIs.
const QUDT_UNIT: &str = "http://qudt.org/vocab/unit/";

/// The symbol of a QUDT unit (`unit:KiloM-PER-HR` is `km/h`), else its local name.
pub(crate) fn unit_symbol(iri: &str) -> String {
    let local = iri
        .strip_prefix(QUDT_UNIT)
        .unwrap_or_else(|| local_name(iri));
    let symbol = match local {
        "M" => "m",
        "KiloM" => "km",
        "MilliM" => "mm",
        "CentiM" => "cm",
        "M2" => "m²",
        "M3" => "m³",
        "SEC" => "s",
        "MIN" => "min",
        "HR" => "h",
        "DAY" => "d",
        "KiloM-PER-HR" => "km/h",
        "M-PER-SEC" => "m/s",
        "M-PER-SEC2" => "m/s²",
        "MilliM-PER-M" => "mm/m",
        "PERMITTIVITY_REL" | "PERCENT" => "%",
        "GM" => "g",
        "KiloGM" => "kg",
        "TONNE" => "t",
        "TONNE-PER-M" => "t/m",
        "N" => "N",
        "KiloN" => "kN",
        "A" => "A",
        "A-PER-M" => "A/m",
        "V" => "V",
        "KiloV" => "kV",
        "W" => "W",
        "KiloW" => "kW",
        "MegaW" => "MW",
        "HZ" => "Hz",
        "DEG" => "°",
        "DEG_C" => "°C",
        other => other,
    };
    symbol.to_owned()
}

/// SPARQL paths to a feature's time (`tts:time` on its property shapes).
#[derive(Clone, Default)]
struct Time {
    instant: Option<String>,
    start: Option<String>,
    end: Option<String>,
}

impl Time {
    fn is_empty(&self) -> bool {
        self.instant.is_none() && self.start.is_none() && self.end.is_none()
    }
}

impl Collection {
    /// Whether the shapes mark a time of the features (`tts:time`), which `datetime` filters on.
    pub(crate) fn has_time(&self) -> bool {
        !self.time.is_empty()
    }

    /// The SPARQL over `?f` that keeps the features whose time meets `datetime`, as the
    /// items' `datetime` (#37): empty when the collection has no `tts:time`.
    pub(crate) fn time_filter(&self, datetime: &str) -> Result<String, HttpError> {
        time_filter(&self.time, datetime)
    }
}

/// Answers `GET /{ds}/ogc/{rest}`; `base` is the absolute URL of `/{ds}/ogc`. The page is
/// HTML for `f=html`, or without `f` when `html_first` (the client's `Accept` names HTML
/// before JSON); JSON otherwise.
#[allow(clippy::too_many_arguments)]
pub(crate) fn route(
    state: &State,
    entry: &Entry,
    rest: &[&str],
    params: &Params,
    base: &str,
    timeout: Option<Duration>,
    html_first: bool,
    fg_first: bool,
    view: &View,
) -> Result<Reply, HttpError> {
    let as_html = match params.get("f") {
        Some(f) => f == "html",
        None => html_first,
    };
    let fg = match params.get("f") {
        Some(f) => f == "jsonfg",
        None => fg_first && !html_first,
    };
    let mut reply = answer(state, entry, rest, params, base, timeout, fg, view)?;
    if as_html {
        reply = html::page(rest, params, base, reply, &state.options.branding)?;
    }
    reply.headers.push(("Vary", "Accept".to_owned()));
    Ok(reply)
}

/// The JSON document of `GET /{ds}/ogc/{rest}`. `view`: the graphs the caller may read,
/// when not all (#203); everything is then read from those graphs only. `fg`: features in
/// JSON-FG rather than GeoJSON.
#[allow(clippy::too_many_arguments)]
fn answer(
    state: &State,
    entry: &Entry,
    rest: &[&str],
    params: &Params,
    base: &str,
    timeout: Option<Duration>,
    fg: bool,
    view: &View,
) -> Result<Reply, HttpError> {
    let format = params.get("f");
    let json_only = |allowed: &[&str]| -> Result<(), HttpError> {
        match format {
            None | Some("html") => Ok(()),
            Some(f) if allowed.contains(&f) => Ok(()),
            Some(f) => Err(HttpError(
                406,
                format!("Unsupported format {f}: one of {}", allowed.join(", ")),
            )),
        }
    };
    let only_params = |allowed: &[&str]| -> Result<(), HttpError> {
        for (name, _) in &params.0 {
            if !allowed.contains(&name.as_str()) {
                return Err(HttpError::bad_request(format!(
                    "Unknown parameter {name}: this resource takes {}",
                    allowed.join(", ")
                )));
            }
        }
        Ok(())
    };
    match rest {
        [] => {
            only_params(&["f"])?;
            json_only(&["json"])?;
            Ok(json(JSON, &landing(entry, base)))
        }
        ["conformance"] => {
            only_params(&["f"])?;
            json_only(&["json"])?;
            Ok(json(JSON, &json!({ "conformsTo": CONFORMANCE })))
        }
        ["api"] => {
            only_params(&["f"])?;
            json_only(&["json"])?;
            let collections = collections(state, entry, view)?;
            Ok(json(OPENAPI, &openapi(entry, base, &collections)))
        }
        ["collections"] => {
            only_params(&["f"])?;
            json_only(&["json"])?;
            let (collections, version) = versioned(state, entry, view)?;
            let extents = extents(entry, version, &collections, view);
            let list: Vec<Value> = collections
                .iter()
                .map(|c| describe(c, base, extents.get(&c.id)))
                .collect();
            Ok(json(
                JSON,
                &json!({
                    "links": [
                        link(&format!("{base}/collections"), "self", JSON, "The collections"),
                        link(
                            &html::with_format(&format!("{base}/collections"), "html"),
                            "alternate",
                            html::LINK_TYPE,
                            "The collections as HTML",
                        ),
                    ],
                    "collections": list,
                }),
            ))
        }
        ["collections", id] => {
            only_params(&["f"])?;
            json_only(&["json"])?;
            let collection = find(state, entry, id, view)?;
            let (collections, version) = versioned(state, entry, view)?;
            let extents = extents(entry, version, &collections, view);
            Ok(json(
                JSON,
                &describe(&collection, base, extents.get(&collection.id)),
            ))
        }
        ["collections", id, "queryables"] => {
            only_params(&["f"])?;
            json_only(&["json", "schema+json"])?;
            let collection = find(state, entry, id, view)?;
            Ok(json(SCHEMA, &queryables(&collection, base)))
        }
        ["collections", id, "items"] => {
            let collection = find(state, entry, id, view)?;
            let (layers, _) = versioned(state, entry, view)?;
            let mut allowed = ITEMS_PARAMS.to_vec();
            allowed.extend(collection.filters.iter().map(String::as_str));
            // An area of another layer takes that layer's own filters (#33).
            let layer_keys = layer_params(&layers, params);
            allowed.extend(layer_keys.iter().map(String::as_str));
            only_params(&allowed)?;
            json_only(&["json", "geojson", "jsonfg"])?;
            items(entry, &collection, &layers, params, base, timeout, fg, view)
        }
        // The id is the percent-encoded IRI; a client that leaves its slashes unencoded
        // (the OGC test suite) sends it over several segments.
        ["collections", id, "items", first, more @ ..] => {
            only_params(&["f", "crs", "geometries", "timeout"])?;
            json_only(&["json", "geojson", "jsonfg"])?;
            let collection = find(state, entry, id, view)?;
            let fid = std::iter::once(*first)
                .chain(more.iter().copied())
                .collect::<Vec<_>>()
                .join("/");
            let iri = percent_decode(&fid)
                .and_then(|s| collection.feature_iri(&s))
                .ok_or_else(|| HttpError(404, format!("No feature {fid}")))?;
            feature(entry, &collection, &iri, params, base, timeout, fg, view)
        }
        _ => Err(HttpError(404, "Not found".to_owned())),
    }
}

fn json(content_type: &str, value: &Value) -> Reply {
    Reply::bytes(200, content_type, value.to_string().into_bytes())
}

fn link(href: &str, rel: &str, kind: &str, title: &str) -> Value {
    json!({ "href": href, "rel": rel, "type": kind, "title": title })
}

fn landing(entry: &Entry, base: &str) -> Value {
    json!({
        "title": entry.name,
        "description": format!("OGC API – Features over the dataset {}", entry.name),
        "links": [
            link(base, "self", JSON, "This document"),
            link(&html::with_format(base, "html"), "alternate", html::LINK_TYPE, "This document as HTML"),
            link(&format!("{base}/api"), "service-desc", OPENAPI, "The API definition"),
            link(&format!("{base}/conformance"), "conformance", JSON, "Conformance classes"),
            link(&format!("{base}/collections"), "data", JSON, "The feature collections"),
        ],
    })
}

/// The media type of the queryables.
const SCHEMA: &str = "application/schema+json";

/// `…/queryables`: the properties a `filter` may name, as a JSON Schema.
fn queryables(collection: &Collection, base: &str) -> Value {
    let url = format!("{base}/collections/{}/queryables", collection.id);
    let mut properties = Map::new();
    properties.insert(
        cql2::GEOMETRY.to_owned(),
        json!({ "title": "Geometry", "format": "geometry-any", "x-ogc-role": "primary-geometry" }),
    );
    for (key, _) in &collection.properties {
        let schema = collection
            .schemas
            .get(key)
            .cloned()
            .unwrap_or_else(|| json!({}));
        properties.insert(key.clone(), schema);
    }
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": url,
        "type": "object",
        "title": collection.title,
        "properties": properties,
        "additionalProperties": false,
    })
}

/// The JSON Schema of a property shape: its title and the type its `sh:datatype` or
/// `sh:nodeKind` gives.
pub(crate) fn property_schema(shapes: &Graph, p: NamedOrBlankNodeRef<'_>, key: &str) -> Value {
    let sh = |local: &str| NamedNode::new_unchecked(format!("{SH}{local}"));
    let mut out = Map::new();
    let title = match shapes.object_for_subject_predicate(p, &sh("name")) {
        Some(TermRef::Literal(l)) => l.value().to_owned(),
        _ => key.to_owned(),
    };
    out.insert("title".into(), title.into());
    if let Some(TermRef::Literal(l)) = shapes.object_for_subject_predicate(p, &sh("description")) {
        out.insert("description".into(), l.value().into());
    }
    let xsd_type = match shapes.object_for_subject_predicate(p, &sh("datatype")) {
        Some(TermRef::NamedNode(n)) => n.as_str().strip_prefix(XSD).map(str::to_owned),
        _ => None,
    };
    let (kind, format) = match xsd_type.as_deref() {
        Some("boolean") => ("boolean", None),
        Some(
            "integer" | "int" | "long" | "short" | "byte" | "nonNegativeInteger"
            | "positiveInteger" | "negativeInteger" | "nonPositiveInteger" | "unsignedInt"
            | "unsignedLong" | "unsignedShort" | "unsignedByte",
        ) => ("integer", None),
        Some("decimal" | "double" | "float") => ("number", None),
        Some("dateTime" | "dateTimeStamp") => ("string", Some("date-time")),
        Some("date") => ("string", Some("date")),
        Some(_) => ("string", None),
        None => match shapes.object_for_subject_predicate(p, &sh("nodeKind")) {
            Some(TermRef::NamedNode(n)) if n.as_str() == format!("{SH}IRI") => {
                ("string", Some("uri"))
            }
            _ => ("string", None),
        },
    };
    out.insert("type".into(), kind.into());
    if let Some(format) = format {
        out.insert("format".into(), format.into());
    }
    Value::Object(out)
}

fn describe(collection: &Collection, base: &str, extent: Option<&Extent>) -> Value {
    let url = format!("{base}/collections/{}", collection.id);
    let mut out = json!({
        "id": collection.id,
        "title": collection.title,
        "itemType": "feature",
        "crs": OutCrs::ALL.map(OutCrs::iri),
        "storageCrs": CRS84,
        "links": [
            link(&url, "self", JSON, "This collection"),
            link(&html::with_format(&url, "html"), "alternate", html::LINK_TYPE, "This collection as HTML"),
            link(&format!("{url}/items"), "items", GEOJSON, "The features"),
            link(&format!("{url}/items?f=jsonfg"), "items", JSON_FG, "The features as JSON-FG"),
            link(&format!("{url}/queryables"), "http://www.opengis.net/def/rel/ogc/1.0/queryables", SCHEMA, "What the features can be filtered on"),
        ],
    });
    if let Some(description) = &collection.description {
        out["description"] = description.clone().into();
    }
    if let Some(extent) = extent {
        let mut value = Map::new();
        if let Some(bbox) = extent.bbox {
            value.insert("spatial".into(), json!({ "bbox": [bbox], "crs": CRS84 }));
        }
        if let Some((start, end)) = &extent.interval {
            value.insert(
                "temporal".into(),
                json!({
                    "interval": [[start, end]],
                    "trs": "http://www.opengis.net/def/uom/ISO-8601/0/Gregorian",
                }),
            );
        }
        if !value.is_empty() {
            out["extent"] = value.into();
        }
    }
    out
}

/// Where and when a collection's features are: the box around their geometries (in CRS84,
/// as items give them) and the span of their times (`tts:time`), RFC 3339, `None` for an open end.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Extent {
    bbox: Option<[f64; 4]>,
    interval: Option<(Option<String>, Option<String>)>,
}

/// The extents of a dataset's collections at one data version, filled in the background.
#[derive(Default)]
struct Extents {
    by_collection: Mutex<HashMap<String, Extent>>,
    done: AtomicBool,
    /// Set when a newer data version replaced this one: its fill stops at the next feature
    /// or collection, so a dataset has at most one fill that matters running.
    stale: AtomicBool,
    /// The last finished extents of an older data version, answered for the collections
    /// this fill has not reached, so a dataset under steady writes (each restarting the fill)
    /// keeps a box. Dropped once this fill is done.
    previous: Mutex<Option<Arc<Extents>>>,
}

impl Extents {
    fn map(&self) -> HashMap<String, Extent> {
        self.by_collection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

type ExtentCache = HashMap<(u64, Option<u64>), (DataVersion, Arc<Extents>)>;
/// The extents per dataset (by [`Entry::id`]) and set of grants (by their fingerprint, #203),
/// with the data version they were made for.
static EXTENTS: Mutex<Option<ExtentCache>> = Mutex::new(None);
/// How long a request waits for extents being made before answering without them.
const EXTENT_WAIT: Duration = Duration::from_millis(250);
/// The longest a collection's extent may take (it reads every geometry of the class).
const EXTENT_TIMEOUT: Duration = Duration::from_secs(120);

/// The extents of `collections` that are known: the first request for a data version starts
/// making them on a thread, and waits a moment for it, so a small dataset has them at once
/// and a large one gets them on a later request. A collection whose extent fails or times
/// out has none. Until a fill is done, collections it has not reached keep the extents of the
/// last finished fill (of an older data version), which may be out of date for a while.
/// Without a data version (a store with no generation) there are none.
///
/// `version` is the data version `collections` were made for, which after a write may be an
/// older one than the store's (see [`collections_for`]): the extents follow the collections,
/// so a fill for the new version starts with the new collections, and a class a write added
/// gets its extent then.
fn extents(
    entry: &Entry,
    version: Option<DataVersion>,
    collections: &Collections,
    view: &View,
) -> HashMap<String, Extent> {
    let Some(version) = version else {
        return HashMap::new();
    };
    let key = (entry.id, view.fingerprint());
    let (extents, started) = {
        let mut cache = EXTENTS.lock().unwrap_or_else(|e| e.into_inner());
        let cache = cache.get_or_insert_with(HashMap::new);
        match cache.get(&key) {
            Some((made_for, extents)) if *made_for == version => (Arc::clone(extents), false),
            _ => {
                let extents = Arc::new(Extents::default());
                if let Some((_, old)) = cache.insert(key, (version, Arc::clone(&extents))) {
                    old.stale.store(true, Ordering::Release);
                    let previous = if old.done.load(Ordering::Acquire) {
                        Some(old)
                    } else {
                        old.previous
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone()
                    };
                    *extents.previous.lock().unwrap_or_else(|e| e.into_inner()) = previous;
                }
                (extents, true)
            }
        }
    };
    if started {
        let engine = entry.engine.clone();
        let collections = Arc::clone(collections);
        let filling = Arc::clone(&extents);
        let view = view.clone();
        std::thread::spawn(move || {
            for collection in collections.iter() {
                if filling.stale.load(Ordering::Acquire) {
                    return;
                }
                let Some(extent) = extent_of(&engine, collection, &filling.stale, &view) else {
                    return;
                };
                filling
                    .by_collection
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(collection.id.clone(), extent);
            }
            filling.done.store(true, Ordering::Release);
            *filling.previous.lock().unwrap_or_else(|e| e.into_inner()) = None;
        });
        let until = std::time::Instant::now() + EXTENT_WAIT;
        while !extents.done.load(Ordering::Acquire) && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let previous = extents
        .previous
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let mut known = previous.map(|p| p.map()).unwrap_or_default();
    known.extend(extents.map());
    known
}

/// The extent of one collection, read from the store, or `None` when `stale` was set while
/// reading it.
fn extent_of(
    engine: &tripletrail::Engine,
    collection: &Collection,
    stale: &AtomicBool,
    view: &View,
) -> Option<Extent> {
    let rows = |query: &str| -> Option<Vec<Option<Term>>> {
        let (engine, _deadline) = crate::with_deadline(engine, Some(EXTENT_TIMEOUT));
        let QueryResults::Solutions(mut rows) = query_in(&engine, query, view).ok()? else {
            return None;
        };
        let variables: Vec<_> = rows.variables().to_vec();
        let row = rows.next()?.ok()?;
        Some(variables.iter().map(|v| row.get(v).cloned()).collect())
    };
    let class = collection.class.as_str();
    let bbox = bbox_of(engine, collection, stale, view)?;
    let time = &collection.time;
    let interval = if time.is_empty() {
        None
    } else {
        // The earliest start (or instant) and the latest end (or instant); a feature with a
        // start and no end leaves the span open. Each value is cast in a BIND: an error in an
        // aggregate's input would void the whole aggregate.
        let dt = xsd::DATE_TIME.as_str();
        // RFC 3339 wants an offset: a value without one is UTC, as `datetime` reads it.
        let text = |row: Option<Vec<Option<Term>>>| match row?.into_iter().next()? {
            Some(Term::Literal(l)) => {
                let mut value = l.value().to_owned();
                if oxsdatatypes::DateTime::from_str(&value)
                    .is_ok_and(|d| d.timezone_offset().is_none())
                {
                    value.push('Z');
                }
                Some(value)
            }
            _ => None,
        };
        let bound = |aggregate: &str, paths: &[&Option<String>]| -> Option<String> {
            let branches: Vec<String> = paths
                .iter()
                .filter_map(|p| p.as_ref())
                .map(|p| format!("{{ ?f {p} ?v }}"))
                .collect();
            if branches.is_empty() {
                return None;
            }
            text(rows(&format!(
                "SELECT ({aggregate}(?t) AS ?x) WHERE {{ ?f a <{class}> . {} \
                 BIND(<{dt}>(?v) AS ?t) FILTER(BOUND(?t)) }}",
                branches.join(" UNION ")
            )))
        };
        let first = bound("MIN", &[&time.instant, &time.start]);
        let open = match (&time.start, &time.end) {
            (Some(start), Some(end)) => {
                let (engine, _deadline) = crate::with_deadline(engine, Some(EXTENT_TIMEOUT));
                let query = format!(
                    "ASK {{ ?f a <{class}> ; {start} ?s . FILTER NOT EXISTS {{ ?f {end} ?e }} }}"
                );
                // When it cannot tell, the span is left open.
                !matches!(
                    query_in(&engine, &query, view),
                    Ok(QueryResults::Boolean(false))
                )
            }
            (Some(_), None) => true,
            _ => false,
        };
        let last = if open {
            None
        } else {
            bound("MAX", &[&time.instant, &time.end])
        };
        (first.is_some() || last.is_some()).then_some((first, last))
    };
    (!stale.load(Ordering::Acquire)).then_some(Extent { bbox, interval })
}

/// The box around a collection's geometries in CRS84: each geometry is reprojected as items
/// are (GeoJSON is CRS84), so literals in different CRSs add up; an empty geometry, one that
/// does not parse, or one in a CRS that cannot be converted is left out. The literals are
/// parsed without the geometry cache, so a fill over many features does not fill it. The
/// outer `None` is a fill stopped by `stale`; the inner one a box that cannot be told.
fn bbox_of(
    engine: &tripletrail::Engine,
    collection: &Collection,
    stale: &AtomicBool,
    view: &View,
) -> Option<Option<[f64; 4]>> {
    use geo::BoundingRect;
    use tripletrail::geo::{Crs, parse_wkt_literal_uncached, transform};
    let (engine, _deadline) = crate::with_deadline(engine, Some(EXTENT_TIMEOUT));
    let query = format!(
        "SELECT ?w WHERE {{ ?f a <{}> . {} }}",
        collection.class.as_str(),
        collection.geometry.pattern("?f", "?w")
    );
    let Ok(QueryResults::Solutions(rows)) = query_in(&engine, &query, view) else {
        return Some(None);
    };
    let mut bbox: Option<[f64; 4]> = None;
    for row in rows {
        if stale.load(Ordering::Relaxed) {
            return None;
        }
        // A failed row (a timeout) leaves the box unknown rather than too small.
        let Ok(row) = row else { return Some(None) };
        let Some(term) = row.get("w") else { continue };
        let Some(geom) = parse_wkt_literal_uncached(term) else {
            continue;
        };
        let Some(rect) =
            transform(&geom.geometry, &geom.crs, &Crs::Crs84).and_then(|g| g.bounding_rect())
        else {
            continue;
        };
        let (lo, hi) = (rect.min(), rect.max());
        if ![lo.x, lo.y, hi.x, hi.y].iter().all(|v| v.is_finite()) {
            continue;
        }
        bbox = Some(match bbox {
            None => [lo.x, lo.y, hi.x, hi.y],
            Some([a, b, c, d]) => [a.min(lo.x), b.min(lo.y), c.max(hi.x), d.max(hi.y)],
        });
    }
    Some(bbox)
}

/// The collections the `ogc-features` reader reads (that `view` sees, when given).
fn collections(
    state: &State,
    entry: &Entry,
    view: &View,
) -> Result<Arc<Vec<Collection>>, HttpError> {
    Ok(versioned(state, entry, view)?.0)
}

/// The collections the `ogc-features` reader reads (that `view` sees, when given), with the
/// data version they were made for.
fn versioned(
    state: &State,
    entry: &Entry,
    view: &View,
) -> Result<(Collections, Option<DataVersion>), HttpError> {
    let (collections, version) = collections_for(state, entry, Reader::OgcFeatures)?;
    let seen = visible(
        state,
        entry,
        Reader::OgcFeatures,
        &collections,
        version,
        view,
    )?;
    Ok((seen, version))
}

type VisibleCache = HashMap<(u64, Reader, u64), (DataVersion, Collections)>;
/// The collections a set of grants sees, by (dataset, reader, grants' fingerprint).
static VISIBLE: Mutex<Option<VisibleCache>> = Mutex::new(None);
/// Sets of grants whose collections are kept, over all datasets.
const VISIBLE_KEPT: usize = 64;

/// The collections a caller who may read only `view` graphs sees (#203): made from shapes
/// generated from those graphs only (with the dataset's own shapes over them), so their
/// properties, queryables and filters name nothing that is only in other graphs, and of
/// those the ones with a feature in the view (an own shape may name a class the view has no
/// instance of). `collections` without a view. Kept per set of grants for the data version
/// the collections were made for.
pub(crate) fn visible(
    state: &State,
    entry: &Entry,
    reader: Reader,
    collections: &Collections,
    version: Option<DataVersion>,
    view: &View,
) -> Result<Collections, HttpError> {
    let Some(fingerprint) = view.fingerprint() else {
        return Ok(Arc::clone(collections));
    };
    let key = (entry.id, reader, fingerprint);
    if let Some(version) = version
        && let Some((made_for, seen)) = VISIBLE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|c| c.get(&key))
        && *made_for == version
    {
        return Ok(Arc::clone(seen));
    }
    let seen: Collections = Arc::new(readable(
        entry,
        view,
        make_collections(state, entry, reader, view)?,
    ));
    if let Some(version) = version {
        let mut cache = VISIBLE.lock().unwrap_or_else(|e| e.into_inner());
        let cache = cache.get_or_insert_with(HashMap::new);
        if cache.len() >= VISIBLE_KEPT {
            cache.clear();
        }
        cache.insert(key, (version, Arc::clone(&seen)));
    }
    Ok(seen)
}

/// The collections with features a limited caller (`view`) may read.
fn readable(entry: &Entry, view: &View, collections: Vec<Collection>) -> Vec<Collection> {
    let options = || view.options();
    collections
        .into_iter()
        .filter(|c| {
            let query = format!(
                "ASK {{ ?f a <{}> . {} }}",
                c.class.as_str(),
                c.geometry.pattern("?f", "?w")
            );
            // When it cannot tell, the collection is left out.
            matches!(
                entry.engine.query_with_options(&query, options()),
                Ok(QueryResults::Boolean(true))
            )
        })
        .collect()
}

/// What the collections of a dataset depend on: its store generation and a stamp of its own
/// shapes' version and installed pack ([`crate::shapes::shapes_stamp`]). `None` when the store has no generation (then nothing is cached).
pub(crate) fn data_version(state: &State, entry: &Entry) -> Option<DataVersion> {
    let generation = match entry.engine.store() {
        tripletrail::store::Dataset::TripleTrail(store) => store.generation(),
        _ => return None,
    };
    let shapes = crate::shapes::folder_of(state, entry)
        .and_then(|folder| crate::shapes::shapes_stamp(&folder));
    Some((generation, shapes))
}

type Collections = Arc<Vec<Collection>>;
/// Store generation and shapes version: see [`data_version`].
pub(crate) type DataVersion = (u64, Option<u64>);
type CollectionCache = HashMap<(u64, Reader), (DataVersion, Collections)>;
/// The collections per (dataset, reader), with the data version they were made for.
static COLLECTIONS: Mutex<Option<CollectionCache>> = Mutex::new(None);

/// The collections being made again in the background, by (dataset, reader).
static REBUILDING: Mutex<Option<HashSet<(u64, Reader)>>> = Mutex::new(None);

/// The collections `reader` reads, kept until the dataset or its shapes change. After a
/// write (a new store generation, the same shapes), the collections made before it are
/// answered while [`refresh_collections`] makes new ones in the background: making them
/// profiles the data, seconds on a large dataset, which every request after every write
/// would otherwise pay. A class that a write adds or removes shows up once they are made.
/// A change of the shapes makes them again at once. If a rebuild fails or panics, the next
/// request after it starts another and the kept collections keep answering meanwhile, so its
/// error is not shown to clients (a failure without kept collections is).
///
/// Also gives the data version the collections were made for, which is older than the
/// store's while a rebuild runs: what is made from them (tiles, extents) is kept under that
/// version, never under the newer one, so nothing made from out-of-date collections outlives
/// the rebuild. `None` when the store has no generation (then nothing is kept).
pub(crate) fn collections_for(
    state: &State,
    entry: &Entry,
    reader: Reader,
) -> Result<(Collections, Option<DataVersion>), HttpError> {
    let version = data_version(state, entry);
    let key = (entry.id, reader);
    if let Some(version) = version
        && let Some((made_for, collections)) = COLLECTIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|c| c.get(&key))
        && (*made_for == version || (made_for.1 == version.1 && rebuilding(key)))
    {
        return Ok((Arc::clone(collections), Some(*made_for)));
    }
    let collections = Arc::new(make_collections(state, entry, reader, &View::default())?);
    if let Some(version) = version {
        keep_collections(key, version, &collections);
    }
    Ok((collections, version))
}

fn rebuilding(key: (u64, Reader)) -> bool {
    REBUILDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|r| r.contains(&key))
}

/// Keeps `collections` made for `version`, unless newer ones (of a later store generation
/// with the same shapes) are kept already.
fn keep_collections(key: (u64, Reader), version: DataVersion, collections: &Collections) {
    let mut cache = COLLECTIONS.lock().unwrap_or_else(|e| e.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    if cache
        .get(&key)
        .is_some_and(|(kept, _)| kept.1 == version.1 && kept.0 > version.0)
    {
        return;
    }
    cache.insert(key, (version, Arc::clone(collections)));
}

/// Starts making the collections of `reader` again on a thread when a write made the kept
/// ones out of date (same shapes, older store generation), one rebuild per dataset and
/// reader at a time; [`collections_for`] answers the kept ones meanwhile. Called before a
/// request under `/{ds}/ogc` or `/{ds}/tiles` is answered.
pub(crate) fn refresh_collections(state: &Arc<State>, entry: &Arc<Entry>, reader: Reader) {
    let Some(version) = data_version(state, entry) else {
        return;
    };
    let key = (entry.id, reader);
    let stale = COLLECTIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|c| c.get(&key))
        .is_some_and(|(made_for, _)| made_for.1 == version.1 && made_for.0 < version.0);
    if !stale
        || !REBUILDING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(HashSet::new)
            .insert(key)
    {
        return;
    }
    let (state, entry) = (Arc::clone(state), Arc::clone(entry));
    std::thread::spawn(move || {
        // Done, failed or panicked, the rebuild is over: a key left behind would answer the
        // old collections for ever.
        struct Over((u64, Reader));
        impl Drop for Over {
            fn drop(&mut self) {
                if let Some(r) = REBUILDING
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_mut()
                {
                    r.remove(&self.0);
                }
            }
        }
        let _over = Over(key);
        // The version is read before the data: a write during the rebuild leaves it out of
        // date, and the next request starts another.
        if let Some(version) = data_version(&state, &entry)
            && let Ok(collections) = make_collections(&state, &entry, reader, &View::default())
        {
            keep_collections(key, version, &Arc::new(collections));
        }
    });
}

/// The collections `reader` reads: node shapes with a target class and a geometry way. With a
/// `view`, the shapes are generated from those graphs only (#203).
fn make_collections(
    state: &State,
    entry: &Entry,
    reader: Reader,
    view: &View,
) -> Result<Vec<Collection>, HttpError> {
    let shapes = crate::shapes::projected(state, entry, reader, view)?;
    let mut collections = collections_of(&shapes)?;
    // The later geometry ways, for the instances the earlier ones leave without (#259).
    let timeout = crate::limits::Limits::of(state, &entry.name).timeout(None);
    for c in &mut collections {
        if c.geometry.has_later_paths() {
            c.geometry = c.geometry.narrowed(entry, view, c.class.as_str(), timeout);
        }
    }
    Ok(collections)
}

/// The collections `reader` would show if `files` were the dataset's own shape set: a preview
/// of proposed shapes (#146), made afresh and not kept.
/// For a limited caller (`view`), the generated shapes come from the graphs it may read and
/// only collections with features it may read are kept, as `visible` does.
pub(crate) fn preview_collections(
    state: &State,
    entry: &Entry,
    reader: Reader,
    files: &[tripletrail::shapes::SetFile],
    view: &View,
) -> Result<Collections, HttpError> {
    let shapes = crate::shapes::projected_with(state, entry, reader, view, files)?;
    let collections = collections_of(&shapes)?;
    Ok(Arc::new(if view.fingerprint().is_some() {
        readable(entry, view, collections)
    } else {
        collections
    }))
}

fn collections_of(shapes: &Graph) -> Result<Vec<Collection>, HttpError> {
    let sh = |local: &str| NamedNode::new_unchecked(format!("{SH}{local}"));
    let tts = |local: &str| NamedNode::new_unchecked(format!("{TTS}{local}"));
    let text = |s: NamedOrBlankNodeRef<'_>, p: &NamedNode| -> Option<String> {
        match shapes.object_for_subject_predicate(s, p)? {
            TermRef::Literal(l) => Some(l.value().to_owned()),
            _ => None,
        }
    };
    let order = |s: NamedOrBlankNodeRef<'_>| -> i64 {
        match shapes.object_for_subject_predicate(s, &sh("order")) {
            Some(TermRef::Literal(l)) => l.value().parse().unwrap_or(i64::MAX),
            _ => i64::MAX,
        }
    };
    let mut out = Vec::new();
    let mut taken = std::collections::HashSet::new();
    let target = sh("targetClass");
    let mut nodes: Vec<_> = shapes
        .triples_for_predicate(&target)
        .map(|t| t.subject.into_owned())
        .collect();
    nodes.sort_by_key(|n| n.to_string());
    nodes.dedup();
    for node in nodes {
        let node = node.as_ref();
        let Some(TermRef::NamedNode(class)) = shapes.object_for_subject_predicate(node, &target)
        else {
            continue;
        };
        let Some(geometry) = crate::geometry_ways::Ways::of(shapes, node) else {
            continue;
        };
        let id = text(node, &tts("key")).unwrap_or_else(|| local_name(class.as_str()).to_owned());
        if !taken.insert(id.clone())
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        {
            continue;
        }
        let drawn = geometry.path().unwrap_or_default().to_owned();
        let first_step = drawn.split('/').next().unwrap_or_default().to_owned();
        let mut shapes_of: Vec<_> = shapes
            .objects_for_subject_predicate(node, &sh("property"))
            .filter_map(subject)
            .collect();
        shapes_of.sort_by_key(|p| (order(*p), p.to_string()));
        let mut keys = std::collections::HashSet::new();
        let mut filters = Vec::new();
        let mut facets = Vec::new();
        let mut properties = Vec::new();
        let mut schemas = HashMap::new();
        let mut time = Time::default();
        for p in shapes_of {
            let Some(path_term) = shapes.object_for_subject_predicate(p, &sh("path")) else {
                continue;
            };
            let Some(path) = sparql_path(shapes, path_term) else {
                continue;
            };
            if path == drawn || path == first_step {
                continue;
            }
            if let Some(TermRef::NamedNode(role)) =
                shapes.object_for_subject_predicate(p, &tts("time"))
            {
                let slot = match role.as_str().strip_prefix(TTS) {
                    Some("Time-Instant") => Some(&mut time.instant),
                    Some("Time-Start") => Some(&mut time.start),
                    Some("Time-End") => Some(&mut time.end),
                    _ => None,
                };
                if let Some(slot) = slot.filter(|s| s.is_none()) {
                    *slot = Some(path.clone());
                }
            }
            let key = text(p, &tts("key")).unwrap_or_else(|| match path_term {
                TermRef::NamedNode(n) => local_name(n.as_str()).to_owned(),
                _ => path.clone(),
            });
            if keys.insert(key.clone()) {
                // A key that is also a parameter of the items stays a property only.
                let facet = shapes.object_for_subject_predicate(p, &tts("facet"));
                if !ITEMS_PARAMS.contains(&key.as_str())
                    && let Some(kind) = facet
                {
                    filters.push(key.clone());
                    facets.push(Facet {
                        title: text(p, &sh("name")).unwrap_or_else(|| key.clone()),
                        key: key.clone(),
                        path: path.clone(),
                        range: kind == TermRef::from(tts("Facet-Range").as_ref()),
                        text: kind == TermRef::from(tts("Facet-Text").as_ref()),
                        datatype: match shapes.object_for_subject_predicate(p, &sh("datatype")) {
                            Some(TermRef::NamedNode(d)) => Some(d.as_str().to_owned()),
                            _ => None,
                        },
                        unit: match shapes.object_for_subject_predicate(
                            p,
                            &NamedNode::new_unchecked("http://qudt.org/schema/qudt/hasUnit"),
                        ) {
                            Some(TermRef::NamedNode(u)) => Some(unit_symbol(u.as_str())),
                            _ => None,
                        },
                        classes: shapes
                            .object_for_subject_predicate(p, &tts("colorClasses"))
                            .and_then(|list| rdf_list(shapes, list))
                            .unwrap_or_default()
                            .into_iter()
                            .filter_map(subject)
                            .map(|c| {
                                (
                                    text(c, &tts("upTo")).and_then(|n| n.trim().parse().ok()),
                                    text(
                                        c,
                                        &NamedNode::new_unchecked(
                                            "http://www.w3.org/2000/01/rdf-schema#label",
                                        ),
                                    ),
                                )
                            })
                            .take(12)
                            .collect(),
                        color_by: text(p, &tts("colorBy")).and_then(|n| n.trim().parse().ok()),
                        select: text(p, &tts("select")),
                    });
                }
                // A computed facet is no property of the features.
                if text(p, &tts("select")).is_some() {
                    if let Some(at) = filters.iter().position(|k| *k == key) {
                        filters.remove(at);
                    }
                    continue;
                }
                schemas.insert(key.clone(), property_schema(shapes, p, &key));
                properties.push((key, path));
            }
        }
        out.push(Collection {
            id_base: None,
            title: text(node, &sh("name")).unwrap_or_else(|| id.clone()),
            description: text(node, &sh("description")),
            id,
            class: class.into_owned(),
            geometry,
            properties,
            filters,
            facets,
            schemas,
            min_zoom: text(node, &tts("minZoom")).and_then(|z| z.parse().ok()),
            draw_order: text(node, &tts("drawOrder")).and_then(|z| z.trim().parse().ok()),
            order: text(node, &sh("order")).and_then(|z| z.trim().parse().ok()),
            color: text(node, &tts("color")).filter(|c| {
                c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|d| d.is_ascii_hexdigit())
            }),
            instances: text(node, &tts("instances")).and_then(|n| n.parse().ok()),
            time,
            views: Vec::new(),
            label_strip: None,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    add_views(shapes, &mut out);
    let strip = label_strip(shapes);
    for c in &mut out {
        c.label_strip.clone_from(&strip);
    }
    Ok(out)
}

/// The `tts:labelStrip` patterns of `shapes`, as one (#260): those that are no regular
/// expression are left out (`shapes check` names them).
fn label_strip(shapes: &Graph) -> Option<regex::Regex> {
    let mut patterns: Vec<String> = shapes
        .triples_for_predicate(&NamedNode::new_unchecked(format!("{TTS}labelStrip")))
        .filter_map(|t| match t.object {
            TermRef::Literal(l) if regex::Regex::new(l.value()).is_ok() => {
                Some(format!("(?:{})", l.value()))
            }
            _ => None,
        })
        .collect();
    patterns.sort();
    patterns.dedup();
    if patterns.is_empty() {
        return None;
    }
    regex::Regex::new(&patterns.join("|")).ok()
}

/// `label` without what `strip` matches, trimmed; `label` itself when nothing would be left.
pub(crate) fn stripped(label: &str, strip: Option<&regex::Regex>) -> String {
    let Some(strip) = strip else {
        return label.to_owned();
    };
    let out = strip.replace_all(label, "");
    let out = out.trim();
    if out.is_empty() {
        label.to_owned()
    } else {
        out.to_owned()
    }
}

fn find(state: &State, entry: &Entry, id: &str, view: &View) -> Result<Collection, HttpError> {
    let mut collection = collections(state, entry, view)?
        .iter()
        .find(|c| c.id == id)
        .cloned()
        .ok_or_else(|| HttpError(404, format!("No collection {id}")))?;
    collection.id_base = id_base(state, entry);
    Ok(collection)
}

/// The base IRI the ids of `entry`'s features are relative to: its base IRI, when it has
/// exactly one (with several, a local part could name a feature under each).
fn id_base(state: &State, entry: &Entry) -> Option<String> {
    let mut bases = state
        .options
        .base_iris
        .iter()
        .filter(|(_, dataset)| *dataset == entry.name)
        .map(|(base, _)| base);
    match (bases.next(), bases.next()) {
        (Some(base), None) => Some(base.clone()),
        _ => None,
    }
}

impl Collection {
    /// A feature's id: its IRI's local part under the dataset's base IRI, or else the IRI.
    /// A local part that is an IRI itself stays the whole IRI, so an id reads back one way.
    fn feature_id<'a>(&self, iri: &'a NamedNode) -> &'a str {
        self.id_base
            .as_deref()
            .and_then(|base| iri.as_str().strip_prefix(base))
            .filter(|local| !local.is_empty() && NamedNode::new(*local).is_err())
            .unwrap_or(iri.as_str())
    }

    /// The feature an id names (see [`Collection::feature_id`]).
    fn feature_iri(&self, id: &str) -> Option<NamedNode> {
        NamedNode::new(id).ok().or_else(|| {
            let base = self.id_base.as_deref()?;
            NamedNode::new(format!("{base}{id}")).ok()
        })
    }
}

pub(crate) fn subject(term: TermRef<'_>) -> Option<NamedOrBlankNodeRef<'_>> {
    match term {
        TermRef::NamedNode(n) => Some(n.into()),
        TermRef::BlankNode(b) => Some(b.into()),
        _ => None,
    }
}

pub(crate) fn local_name(iri: &str) -> &str {
    iri.trim_end_matches(['/', '#'])
        .rsplit(['#', '/', ':'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(iri)
}

/// A SHACL path as a SPARQL property path; `None` for what SPARQL cannot say.
pub(crate) fn sparql_path(shapes: &Graph, path: TermRef<'_>) -> Option<String> {
    let sh = |local: &str| NamedNode::new_unchecked(format!("{SH}{local}"));
    match path {
        TermRef::NamedNode(n) => Some(format!("<{}>", n.as_str())),
        TermRef::BlankNode(b) => {
            let node: NamedOrBlankNodeRef<'_> = b.into();
            let one = |p: &str| shapes.object_for_subject_predicate(node, &sh(p));
            if let Some(inner) = one("inversePath") {
                return Some(format!("^({})", sparql_path(shapes, inner)?));
            }
            if let Some(list) = one("alternativePath") {
                let items = rdf_list(shapes, list)?;
                let parts: Option<Vec<String>> =
                    items.into_iter().map(|t| sparql_path(shapes, t)).collect();
                return Some(format!("({})", parts?.join("|")));
            }
            for (p, suffix) in [
                ("zeroOrMorePath", "*"),
                ("oneOrMorePath", "+"),
                ("zeroOrOnePath", "?"),
            ] {
                if let Some(inner) = one(p) {
                    return Some(format!("({}){suffix}", sparql_path(shapes, inner)?));
                }
            }
            let items = rdf_list(shapes, path)?;
            let parts: Option<Vec<String>> =
                items.into_iter().map(|t| sparql_path(shapes, t)).collect();
            let parts = parts?;
            (!parts.is_empty()).then(|| parts.join("/"))
        }
        _ => None,
    }
}

pub(crate) fn rdf_list<'a>(shapes: &'a Graph, head: TermRef<'a>) -> Option<Vec<TermRef<'a>>> {
    let mut out = Vec::new();
    let mut cell = head;
    while cell != TermRef::from(rdf::NIL) {
        let node = subject(cell)?;
        out.push(shapes.object_for_subject_predicate(node, rdf::FIRST)?);
        cell = shapes.object_for_subject_predicate(node, rdf::REST)?;
        if out.len() > 100 {
            return None;
        }
    }
    Some(out)
}

/// `bbox` in CRS84 (four or six numbers) as a WKT polygon, as `bbox` of `…/items` reads it.
pub(crate) fn crs84_bbox(value: &str) -> Result<String, HttpError> {
    bbox(value, OutCrs::Crs84)
}

/// `bbox`: four numbers (or six, the heights ignored) in `crs` (lower corner, upper corner,
/// in its axis order), as a CRS84 WKT polygon; one that crosses the antimeridian
/// (`minx > maxx`) as two.
fn bbox(value: &str, crs: OutCrs) -> Result<String, HttpError> {
    let numbers: Result<Vec<f64>, _> = value.split(',').map(|n| n.trim().parse::<f64>()).collect();
    let bad = || HttpError::bad_request(format!("Invalid bbox {value}: minx,miny,maxx,maxy"));
    let numbers = numbers.map_err(|_| bad())?;
    let (a1, b1, a2, b2) = match numbers.as_slice() {
        [a, b, c, d] => (*a, *b, *c, *d),
        [a, b, _, c, d, _] => (*a, *b, *c, *d),
        _ => return Err(bad()),
    };
    if numbers.iter().any(|n| !n.is_finite()) {
        return Err(bad());
    }
    let (x1, y1) = crs.unproject(a1, b1);
    let (x2, y2) = crs.unproject(a2, b2);
    if y1 > y2 || (crs == OutCrs::WebMercator && x1 > x2) {
        return Err(bad());
    }
    let ring =
        |x1: f64, x2: f64| format!("(({x1} {y1}, {x2} {y1}, {x2} {y2}, {x1} {y2}, {x1} {y1}))");
    Ok(if x1 <= x2 {
        format!("POLYGON{}", ring(x1, x2))
    } else {
        format!("MULTIPOLYGON({}, {})", ring(x1, 180.0), ring(-180.0, x2))
    })
}

pub(crate) fn number(
    params: &Params,
    name: &str,
    default: usize,
    min: usize,
) -> Result<usize, HttpError> {
    match params.get(name) {
        None => Ok(default),
        Some(v) => match v.parse::<usize>() {
            Ok(n) if n >= min => Ok(n),
            _ => Err(HttpError::bad_request(format!(
                "{name} must be a whole number from {min}"
            ))),
        },
    }
}

/// `query` on `engine`, on the `view` graphs only when given (#203).
fn query_in(
    engine: &tripletrail::Engine,
    query: &str,
    view: &View,
) -> Result<QueryResults<'static>, tripletrail::Error> {
    engine.query_with_options(query, view.options())
}

/// The forms of the values on `path` of the features `features` binds to `?f` (see
/// [`cql2::Form`]), or `None` when they come in more than a few forms.
fn value_forms(
    entry: &Entry,
    features: &str,
    path: &str,
    timeout: Option<Duration>,
    view: &View,
) -> Result<Option<Vec<cql2::Form>>, HttpError> {
    const MAX_FORMS: usize = 32;
    let query = format!(
        "SELECT DISTINCT ?d ?l ?k WHERE {{ {features} ?f {path} ?v . \
         BIND(IF(isIRI(?v), 1, IF(isBLANK(?v), 2, 0)) AS ?k) \
         BIND(DATATYPE(?v) AS ?d) BIND(LANG(?v) AS ?l) }} LIMIT {}",
        MAX_FORMS + 1
    );
    let rows = run(entry, &query, timeout, view)?;
    if rows.len() > MAX_FORMS {
        return Ok(None);
    }
    let rdf = |local: &str| format!("http://www.w3.org/1999/02/22-rdf-syntax-ns#{local}");
    let xsd_string = "http://www.w3.org/2001/XMLSchema#string";
    let text = |t: &Option<Term>| match t {
        Some(Term::NamedNode(n)) => n.as_str().to_owned(),
        Some(Term::Literal(l)) => l.value().to_owned(),
        _ => String::new(),
    };
    Ok(Some(
        rows.iter()
            .map(|row| match text(&row[2]).as_str() {
                "1" => cql2::Form::Iri,
                "2" => cql2::Form::Blank,
                _ => {
                    let datatype = text(&row[0]);
                    if datatype == xsd_string {
                        cql2::Form::Str
                    } else if datatype == rdf("langString") {
                        cql2::Form::Lang(text(&row[1]))
                    } else {
                        cql2::Form::Other
                    }
                }
            })
            .collect(),
    ))
}

pub(crate) fn run(
    entry: &Entry,
    query: &str,
    timeout: Option<Duration>,
    view: &View,
) -> Result<Vec<Vec<Option<Term>>>, HttpError> {
    let (engine, _deadline) = crate::with_deadline(&entry.engine, timeout);
    let QueryResults::Solutions(rows) = query_in(&engine, query, view)? else {
        return Err(HttpError(500, "Expected solutions".to_owned()));
    };
    let variables: Vec<_> = rows.variables().to_vec();
    let mut out = Vec::new();
    for row in rows {
        let row = row.map_err(|e| HttpError::from(tripletrail::Error::from(e)))?;
        out.push(variables.iter().map(|v| row.get(v).cloned()).collect());
    }
    Ok(out)
}

/// `…/items`: a page of features, in the store's order.
#[allow(clippy::too_many_arguments)]
/// The filters of the layer an `area=layer:{id}` names, which the items take besides their
/// own: `{id}.{key}`, `.min`, `.max` and `.text` of its facets.
fn layer_params(layers: &[Collection], params: &Params) -> Vec<String> {
    let Some(id) = params
        .get("area")
        .and_then(|a| a.trim().strip_prefix(crate::area::LAYER))
    else {
        return Vec::new();
    };
    let Some(layer) = layers.iter().find(|c| c.id == id.trim()) else {
        return Vec::new();
    };
    layer
        .facets
        .iter()
        .flat_map(|f| {
            let key = format!("{}.{}", layer.id, f.key);
            [
                format!("{key}.min"),
                format!("{key}.max"),
                format!("{key}.text"),
                key,
            ]
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn items(
    entry: &Entry,
    collection: &Collection,
    layers: &[Collection],
    params: &Params,
    base: &str,
    timeout: Option<Duration>,
    fg: bool,
    view: &View,
) -> Result<Reply, HttpError> {
    let limit = number(params, "limit", DEFAULT_LIMIT, 1)?.min(MAX_LIMIT);
    let offset = number(params, "offset", 0, 0)?;
    let bbox_crs = OutCrs::of(params, "bbox-crs")?;
    let crs = OutCrs::of(params, "crs")?;
    let window = params.get("bbox").map(|b| bbox(b, bbox_crs)).transpose()?;
    let mut filter = window
        .as_ref()
        .map(|w| format!("FILTER(<{GEOF}sfIntersects>(?w, \"{w}\"^^<{WKT}>))"))
        .unwrap_or_default();
    // Property filters: a value of the property, compared as text.
    for (i, (name, value)) in params.0.iter().enumerate() {
        if !collection.filters.contains(name) {
            continue;
        }
        let Some((_, path)) = collection.properties.iter().find(|(k, _)| k == name) else {
            continue;
        };
        let text = serde_json::to_string(value).expect("a string");
        filter.push_str(&format!(" ?f {path} ?q{i} . FILTER(STR(?q{i}) = {text})"));
    }
    if let Some(datetime) = params.get("datetime") {
        filter.push_str(&time_filter(&collection.time, datetime)?);
    }
    if let Some(area) =
        crate::area::area_filter(entry, view, collection, layers, None, params, false)?
    {
        filter.push_str(&area.sparql);
    }
    if let Some(cql) = params.get("filter") {
        let lang = cql2::Lang::of(params.get("filter-lang"))?;
        let crs = OutCrs::of(params, "filter-crs")?;
        let features = format!("?f a <{}> .", collection.class.as_str());
        let mut forms = |path: &str| value_forms(entry, &features, path, timeout, view);
        let mut scope = cql2::Scope {
            features: features.clone(),
            forms: &mut forms,
        };
        filter.push_str(&cql2::to_sparql(
            cql,
            lang,
            crs,
            &collection.properties,
            &mut scope,
        )?);
    } else if params.get("filter-lang").is_some() || params.get("filter-crs").is_some() {
        return Err(HttpError::bad_request(
            "filter-lang and filter-crs need a filter",
        ));
    }
    // A page of distinct features: one may have several values on the geometry path.
    let query = format!(
        "SELECT DISTINCT ?f WHERE {{ ?f a <{}> . {} {filter} }} LIMIT {} OFFSET {offset}",
        collection.class.as_str(),
        match &window {
            Some(w) => collection.geometry.near(entry, view, "?f", w),
            None => collection.geometry.clone(),
        }
        .pattern("?f", "?w"),
        limit + 1,
    );
    let rows = run(entry, &query, timeout, view)?;
    let more = rows.len() > limit;
    let order: Vec<NamedNode> = rows
        .into_iter()
        .take(limit)
        .filter_map(|row| match row.into_iter().next().flatten() {
            Some(Term::NamedNode(f)) => Some(f),
            _ => None,
        })
        .collect();
    let mut geometries = geometries(
        entry,
        collection,
        &order,
        all_geometries(params)?,
        timeout,
        view,
    )?;
    let mut wgs84 = (fg && crs != OutCrs::Crs84).then(|| geometries.clone());
    geometries.values_mut().for_each(|g| reproject(g, crs));
    let mut features = features(
        entry,
        collection,
        &order,
        &mut geometries,
        base,
        timeout,
        view,
    )?;
    if fg {
        json_fg(
            entry,
            collection,
            &order,
            &mut features,
            wgs84.as_mut(),
            timeout,
            view,
        )?;
    }
    let url = format!("{base}/collections/{}/items", collection.id);
    let page = |offset: usize| {
        let mut query = form_urlencoded::Serializer::new(String::new());
        for (name, value) in &params.0 {
            if name != "offset" {
                query.append_pair(name, value);
            }
        }
        if offset > 0 {
            query.append_pair("offset", &offset.to_string());
        }
        let query = query.finish();
        if query.is_empty() {
            url.clone()
        } else {
            format!("{url}?{query}")
        }
    };
    let kind = if fg { JSON_FG } else { GEOJSON };
    let mut links = vec![
        link(&page(offset), "self", kind, "This page"),
        link(
            &html::with_format(&page(offset), "html"),
            "alternate",
            html::LINK_TYPE,
            "This page as HTML",
        ),
        link(
            &format!("{base}/collections/{}", collection.id),
            "collection",
            JSON,
            "The collection",
        ),
    ];
    if more {
        links.push(link(&page(offset + limit), "next", kind, "The next page"));
    }
    if offset > 0 {
        links.push(link(
            &page(offset.saturating_sub(limit)),
            "prev",
            kind,
            "The previous page",
        ));
    }
    let mut body = json!({
        "type": "FeatureCollection",
        "numberReturned": features.len(),
        "timeStamp": timestamp(),
        "links": links,
        "features": features,
    });
    if fg {
        body["conformsTo"] = json!([JSON_FG_CORE]);
        body["featureType"] = json!(collection.id);
        if crs != OutCrs::Crs84 {
            body["coordRefSys"] = json!(crs.iri());
        }
    }
    Ok(with_crs(json(kind, &body), crs))
}

/// Makes GeoJSON `features` (of `order`, their geometries in the CRS asked for) JSON-FG: each
/// gets its `time` from the collection's `tts:time` paths, and, when `wgs84` holds the
/// geometries in CRS84 (another CRS was asked for), the geometry in that CRS as `place` and
/// the CRS84 one as `geometry`.
fn json_fg(
    entry: &Entry,
    collection: &Collection,
    order: &[NamedNode],
    features: &mut [Value],
    mut wgs84: Option<&mut HashMap<NamedNode, Value>>,
    timeout: Option<Duration>,
    view: &View,
) -> Result<(), HttpError> {
    let mut times = times(entry, collection, order, timeout, view)?;
    for (f, feature) in order.iter().zip(features.iter_mut()) {
        feature["time"] = times.remove(f).unwrap_or(Value::Null);
        match wgs84.as_deref_mut() {
            Some(wgs84) => {
                feature["place"] = feature["geometry"].take();
                feature["geometry"] = wgs84.remove(f).unwrap_or(Value::Null);
            }
            None => feature["place"] = Value::Null,
        }
    }
    Ok(())
}

/// The JSON-FG `time` of each feature of `order` that has one: its `tts:time` instant as a
/// `date` or a UTC `timestamp`, and its start and end as an `interval` (`..` for a missing
/// end). Of several values on a path, the least start, greatest end and least instant.
fn times(
    entry: &Entry,
    collection: &Collection,
    order: &[NamedNode],
    timeout: Option<Duration>,
    view: &View,
) -> Result<HashMap<NamedNode, Value>, HttpError> {
    let time = &collection.time;
    if order.is_empty() || time.is_empty() {
        return Ok(HashMap::new());
    }
    let paths = [&time.instant, &time.start, &time.end];
    let unions: Vec<String> = paths
        .iter()
        .enumerate()
        .filter_map(|(i, p)| Some(format!("{{ ?f {} ?v BIND({i} AS ?k) }}", p.as_ref()?)))
        .collect();
    let values: Vec<String> = order.iter().map(|f| f.to_string()).collect();
    let query = format!(
        "SELECT ?f ?k ?v WHERE {{ VALUES ?f {{ {} }} {} }}",
        values.join(" "),
        unions.join(" UNION ")
    );
    // Per feature: the instant, start and end, each as (sort key, JSON-FG text, is a date).
    let mut found: HashMap<NamedNode, [Option<FgInstant>; 3]> = HashMap::new();
    for row in run(entry, &query, timeout, view)? {
        let (Some(Term::NamedNode(f)), Some(Term::Literal(k)), Some(Term::Literal(v))) =
            (&row[0], &row[1], &row[2])
        else {
            continue;
        };
        let Ok(k) = k.value().parse::<usize>() else {
            continue;
        };
        let Some(instant) = fg_instant(v) else {
            continue;
        };
        let slot = &mut found.entry(f.clone()).or_default()[k.min(2)];
        // The greatest end, else the least.
        let keep = match slot {
            None => true,
            Some(old) if k == 2 => instant.0 > old.0,
            Some(old) => instant.0 < old.0,
        };
        if keep {
            *slot = Some(instant);
        }
    }
    Ok(found
        .into_iter()
        .map(|(f, [instant, start, end])| {
            let mut time = Map::new();
            if let Some((_, text, is_date)) = instant {
                time.insert(
                    (if is_date { "date" } else { "timestamp" }).to_owned(),
                    text.into(),
                );
            }
            if start.is_some() || end.is_some() {
                let end_of = |e: Option<FgInstant>| e.map_or("..".to_owned(), |e| e.1);
                time.insert("interval".to_owned(), json!([end_of(start), end_of(end)]));
            }
            (f, Value::Object(time))
        })
        .collect())
}

/// A time value for JSON-FG: (sort key, text, whether it is a date).
type FgInstant = (String, String, bool);

/// A time value as JSON-FG writes it: a date as it is (`2020-01-01`), a date-time in UTC
/// (`2020-01-01T10:00:00Z`; one without a timezone is read as UTC), with a key that sorts it
/// (the UTC date-time, a date at its start). `None` for other values.
fn fg_instant(v: &oxigraph::model::Literal) -> Option<FgInstant> {
    let text = v.value();
    if let Ok(date) = oxsdatatypes::Date::from_str(text) {
        let day = date.to_string();
        let day = day.get(..10).unwrap_or(&day).to_owned();
        return Some((format!("{day}T00:00:00Z"), day, true));
    }
    let date_time = oxsdatatypes::DateTime::from_str(text).ok()?;
    let utc = if date_time.timezone_offset().is_some() {
        date_time.adjust(Some(oxsdatatypes::TimezoneOffset::UTC))?
    } else {
        oxsdatatypes::DateTime::from_str(&format!("{text}Z")).ok()?
    };
    let text = utc.to_string();
    Some((text.clone(), text, false))
}

/// The SPARQL that keeps the features whose time meets `datetime` (an instant or an interval
/// `start/end`, either end open with `..` or empty; RFC 3339 dates or date-times, a date covering its day). Values
/// without a timezone are read as UTC. A feature
/// with an instant meets it when the instant is in the interval; one with a start and an end
/// when the two intervals overlap, a missing start or end leaving that side open. A feature
/// without any time is left out. Without `tts:time` in the shapes, `datetime` is ignored.
fn time_filter(time: &Time, datetime: &str) -> Result<String, HttpError> {
    let (low, high) = interval(datetime)?;
    if time.is_empty() {
        return Ok(String::new());
    }
    // A date value covers its day (in its own timezone, if it has one): it begins at its
    // start and ends at its end (the time goes between the date and its timezone). A value
    // without a timezone is read as UTC (else it would not compare with a bound within 14 h).
    let dt = xsd::DATE_TIME.as_str();
    let utc = |e: String| format!("IF(TZ({e}) = \"\", <{dt}>(CONCAT(STR({e}), \"Z\")), {e})");
    let begins = |v: &str| utc(format!("<{dt}>({v})"));
    let ends = |v: &str| {
        utc(format!(
            "IF(DATATYPE({v}) = <{}>, <{dt}>(CONCAT(SUBSTR(STR({v}), 1, 10), \"T23:59:59.999\", SUBSTR(STR({v}), 11))), <{dt}>({v}))",
            xsd::DATE.as_str()
        ))
    };
    let at_least = |v: &str| low.as_ref().map(|l| format!("{} >= {l}", ends(v)));
    let at_most = |v: &str| high.as_ref().map(|h| format!("{} <= {h}", begins(v)));
    let all = |tests: Vec<Option<String>>| {
        let tests: Vec<String> = tests.into_iter().flatten().collect();
        if tests.is_empty() {
            "true".to_owned()
        } else {
            tests.join(" && ")
        }
    };
    let mut out = String::new();
    let mut alternatives = Vec::new();
    if let Some(path) = &time.instant {
        out.push_str(&format!(" OPTIONAL {{ ?f {path} ?ti }}"));
        alternatives.push(format!(
            "(BOUND(?ti) && {})",
            all(vec![at_least("?ti"), at_most("?ti")])
        ));
    }
    if time.start.is_some() || time.end.is_some() {
        let mut bound = Vec::new();
        let mut tests = Vec::new();
        if let Some(path) = &time.start {
            out.push_str(&format!(" OPTIONAL {{ ?f {path} ?ts }}"));
            bound.push("BOUND(?ts)");
            // It starts before the interval ends.
            tests.push(at_most("?ts").map(|t| format!("(!BOUND(?ts) || {t})")));
        }
        if let Some(path) = &time.end {
            out.push_str(&format!(" OPTIONAL {{ ?f {path} ?te }}"));
            bound.push("BOUND(?te)");
            // It ends after the interval starts.
            tests.push(at_least("?te").map(|t| format!("(!BOUND(?te) || {t})")));
        }
        alternatives.push(format!("(({}) && {})", bound.join(" || "), all(tests)));
    }
    out.push_str(&format!(" FILTER({})", alternatives.join(" || ")));
    Ok(out)
}

/// The bounds of `datetime` as `xsd:dateTime` literals, `None` for an open end. A date
/// covers its whole day.
fn interval(datetime: &str) -> Result<(Option<String>, Option<String>), HttpError> {
    let bad = || {
        HttpError::bad_request(format!(
            "Invalid datetime {datetime}: an RFC 3339 date or date-time, or an interval start/end (.. for an open end)"
        ))
    };
    let bound = |text: &str, end: bool| -> Result<Option<String>, HttpError> {
        if text.is_empty() || text == ".." {
            return Ok(None);
        }
        // A bound without an offset (RFC 3339 asks for one) is read as UTC, like the data.
        let literal = |mut value: String| -> Result<Option<String>, HttpError> {
            let parsed = oxsdatatypes::DateTime::from_str(&value).map_err(|_| bad())?;
            if parsed.timezone_offset().is_none() {
                value.push('Z');
            }
            Ok(Some(format!("\"{value}\"^^<{}>", xsd::DATE_TIME.as_str())))
        };
        if text.contains('T') || text.contains('t') {
            literal(text.to_uppercase())
        } else if oxsdatatypes::Date::from_str(text).is_ok() {
            literal(format!(
                "{text}T{}",
                if end { "23:59:59.999Z" } else { "00:00:00Z" }
            ))
        } else {
            Err(bad())
        }
    };
    match datetime.split_once('/') {
        Some((start, end)) => {
            let low = bound(start, false)?;
            let high = bound(end, true)?;
            if low.is_none() && high.is_none() {
                return Err(bad());
            }
            Ok((low, high))
        }
        None if datetime == ".." || datetime.is_empty() => Err(bad()),
        None => Ok((bound(datetime, false)?, bound(datetime, true)?)),
    }
}

/// `…/items/{fid}`: one feature, its id the IRI.
#[allow(clippy::too_many_arguments)]
fn feature(
    entry: &Entry,
    collection: &Collection,
    iri: &NamedNode,
    params: &Params,
    base: &str,
    timeout: Option<Duration>,
    fg: bool,
    view: &View,
) -> Result<Reply, HttpError> {
    let query = format!("ASK {{ {iri} a <{}> }}", collection.class.as_str());
    let (engine, _deadline) = crate::with_deadline(&entry.engine, timeout);
    if !matches!(
        query_in(&engine, &query, view)?,
        QueryResults::Boolean(true)
    ) {
        return Err(HttpError(
            404,
            format!("No feature {} in {}", iri.as_str(), collection.id),
        ));
    }
    let crs = OutCrs::of(params, "crs")?;
    let mut geometries = geometries(
        entry,
        collection,
        std::slice::from_ref(iri),
        all_geometries(params)?,
        timeout,
        view,
    )?;
    let mut wgs84 = (fg && crs != OutCrs::Crs84).then(|| geometries.clone());
    geometries.values_mut().for_each(|g| reproject(g, crs));
    let mut features = features(
        entry,
        collection,
        std::slice::from_ref(iri),
        &mut geometries,
        base,
        timeout,
        view,
    )?;
    if fg {
        json_fg(
            entry,
            collection,
            std::slice::from_ref(iri),
            &mut features,
            wgs84.as_mut(),
            timeout,
            view,
        )?;
    }
    let mut feature = features.pop().expect("one feature");
    let kind = if fg { JSON_FG } else { GEOJSON };
    if fg {
        feature["conformsTo"] = json!([JSON_FG_CORE]);
        feature["featureType"] = json!(collection.id);
        if crs != OutCrs::Crs84 {
            feature["coordRefSys"] = json!(crs.iri());
        }
    }
    feature["links"] = json!([
        link(
            &feature_url(base, collection, iri),
            "self",
            kind,
            "This feature"
        ),
        link(
            &html::with_format(&feature_url(base, collection, iri), "html"),
            "alternate",
            html::LINK_TYPE,
            "This feature as HTML"
        ),
        link(
            &format!("{base}/collections/{}", collection.id),
            "collection",
            JSON,
            "The collection"
        ),
    ]);
    Ok(with_crs(json(kind, &feature), crs))
}

/// The `Content-Crs` header Part 2 asks for.
fn with_crs(mut reply: Reply, crs: OutCrs) -> Reply {
    reply
        .headers
        .push(("Content-Crs", format!("<{}>", crs.iri())));
    reply
}

fn feature_url(base: &str, collection: &Collection, iri: &NamedNode) -> String {
    format!(
        "{base}/collections/{}/items/{}",
        collection.id,
        percent_encode(collection.feature_id(iri))
    )
}

/// `geometries`: whether a feature with several geometry values gets them all.
fn all_geometries(params: &Params) -> Result<bool, HttpError> {
    match params.get("geometries") {
        None | Some("one") => Ok(false),
        Some("all") => Ok(true),
        Some(other) => Err(HttpError::bad_request(format!(
            "Invalid geometries {other}: one or all"
        ))),
    }
}

/// The geometry of each feature of `order`: its value on the geometry path as GeoJSON. Of
/// several distinct values, the most detailed one (most coordinates, then most digits, then
/// the first by its GeoJSON), or with `all` a `GeometryCollection` of them all, ordered by
/// their GeoJSON.
fn geometries(
    entry: &Entry,
    collection: &Collection,
    order: &[NamedNode],
    all: bool,
    timeout: Option<Duration>,
    view: &View,
) -> Result<HashMap<NamedNode, Value>, HttpError> {
    let mut found: HashMap<NamedNode, Vec<String>> = HashMap::new();
    if !order.is_empty() {
        let values: Vec<String> = order.iter().map(|f| f.to_string()).collect();
        let query = format!(
            "SELECT DISTINCT ?f (<{GEOF}asGeoJSON>(?w) AS ?g) WHERE {{ VALUES ?f {{ {} }} {} }}",
            values.join(" "),
            collection.geometry.only("?f", &values).pattern("?f", "?w"),
        );
        for row in run(entry, &query, timeout, view)? {
            if let (Some(Term::NamedNode(f)), Some(Term::Literal(g))) = (&row[0], &row[1]) {
                found
                    .entry(f.clone())
                    .or_default()
                    .push(g.value().to_owned());
            }
        }
    }
    Ok(found
        .into_iter()
        .map(|(f, mut texts)| {
            texts.sort();
            texts.dedup();
            if !all && texts.len() > 1 {
                let detail = |t: &String| (t.matches(',').count(), t.len());
                let best = texts
                    .iter()
                    .enumerate()
                    .max_by(|(i, a), (j, b)| detail(a).cmp(&detail(b)).then(j.cmp(i)))
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                texts = vec![texts.swap_remove(best)];
            }
            let mut parts: Vec<Value> = texts
                .iter()
                .filter_map(|t| serde_json::from_str(t).ok())
                .collect();
            let geometry = match parts.len() {
                0 => Value::Null,
                1 => parts.pop().expect("one"),
                _ => json!({ "type": "GeometryCollection", "geometries": parts }),
            };
            (f, geometry)
        })
        .collect())
}

/// The features `order` lists, with their properties.
fn features(
    entry: &Entry,
    collection: &Collection,
    order: &[NamedNode],
    geometries: &mut HashMap<NamedNode, Value>,
    base: &str,
    timeout: Option<Duration>,
    view: &View,
) -> Result<Vec<Value>, HttpError> {
    let mut properties: HashMap<&NamedNode, Map<String, Value>> =
        order.iter().map(|f| (f, Map::new())).collect();
    if !order.is_empty() && !collection.properties.is_empty() {
        let values: Vec<String> = order.iter().map(|f| f.to_string()).collect();
        let unions: Vec<String> = collection
            .properties
            .iter()
            .enumerate()
            .map(|(i, (_, path))| format!("{{ ?f {path} ?v BIND({i} AS ?k) }}"))
            .collect();
        let query = format!(
            "SELECT ?f ?k ?v WHERE {{ VALUES ?f {{ {} }} {} }}",
            values.join(" "),
            unions.join(" UNION ")
        );
        let mut found: HashMap<(NamedNode, usize), Vec<Value>> = HashMap::new();
        for row in run(entry, &query, timeout, view)? {
            let (Some(Term::NamedNode(f)), Some(Term::Literal(k)), Some(v)) =
                (&row[0], &row[1], &row[2])
            else {
                continue;
            };
            let Ok(k) = k.value().parse::<usize>() else {
                continue;
            };
            if let Some(v) = value(v) {
                found.entry((f.clone(), k)).or_default().push(v);
            }
        }
        for ((f, k), mut values) in found {
            if let Some(map) = properties.get_mut(&f) {
                let value = if values.len() == 1 {
                    values.pop().expect("one")
                } else {
                    // In the values' own order: the order rows come in follows the store's
                    // term ids, which other graphs' data moves (#203).
                    values.sort_by_cached_key(Value::to_string);
                    Value::Array(values)
                };
                map.insert(collection.properties[k].0.clone(), value);
            }
        }
    }
    Ok(order
        .iter()
        .map(|f| {
            let mut props = properties.remove(f).unwrap_or_default();
            // Keys in the shapes' order.
            let mut sorted = Map::new();
            for (key, _) in &collection.properties {
                if let Some(v) = props.remove(key) {
                    sorted.insert(key.clone(), v);
                }
            }
            json!({
                "type": "Feature",
                "id": collection.feature_id(f),
                "geometry": geometries.remove(f).unwrap_or(Value::Null),
                "properties": sorted,
                "links": [link(&feature_url(base, collection, f), "self", GEOJSON, "This feature")],
            })
        })
        .collect())
}

/// A property value as JSON: numbers and booleans typed, IRIs and other literals as strings.
pub(crate) fn value(term: &Term) -> Option<Value> {
    match term {
        Term::NamedNode(n) => Some(n.as_str().into()),
        Term::Literal(l) => {
            let d = l.datatype();
            let numeric = [
                xsd::INTEGER,
                xsd::INT,
                xsd::LONG,
                xsd::SHORT,
                xsd::NON_NEGATIVE_INTEGER,
                xsd::POSITIVE_INTEGER,
                xsd::UNSIGNED_INT,
                xsd::UNSIGNED_LONG,
                xsd::DECIMAL,
                xsd::DOUBLE,
                xsd::FLOAT,
            ];
            if d == xsd::BOOLEAN {
                match l.value() {
                    "true" | "1" => return Some(true.into()),
                    "false" | "0" => return Some(false.into()),
                    _ => {}
                }
            } else if numeric.contains(&d) {
                if let Ok(n) = l.value().parse::<i64>() {
                    return Some(n.into());
                }
                if let Ok(n) = l.value().parse::<f64>()
                    && let Some(n) = serde_json::Number::from_f64(n)
                {
                    return Some(Value::Number(n));
                }
            }
            Some(l.value().into())
        }
        _ => None,
    }
}

fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

/// Percent-encodes all but the unreserved characters, for an IRI in one path segment.
pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub(crate) fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A minimal OpenAPI 3.0 definition of the API, with a path per collection.
fn openapi(entry: &Entry, base: &str, collections: &[Collection]) -> Value {
    let param = |name: &str, place: &str, schema: Value, description: &str| {
        let mut p = json!({ "name": name, "in": place, "required": place == "path", "schema": schema, "description": description });
        if place == "query" {
            // As OGC API – Features defines its parameters: `bbox=1,2,3,4`, not repeated.
            p["style"] = "form".into();
            p["explode"] = false.into();
        }
        p
    };
    let ok = |kind: &str, description: &str| json!({ "200": { "description": description, "content": { kind: {} } } });
    // Features: GeoJSON, or JSON-FG for `Accept: application/vnd.ogc.fg+json` or `f=jsonfg`.
    let ok_features = |description: &str| json!({ "200": { "description": description, "content": { GEOJSON: {}, JSON_FG: {} } } });
    let mut paths = Map::new();
    paths.insert("/".into(), json!({ "get": { "summary": "Landing page", "operationId": "getLandingPage", "responses": ok(JSON, "The landing page") } }));
    paths.insert("/conformance".into(), json!({ "get": { "summary": "Conformance classes", "operationId": "getConformance", "responses": ok(JSON, "The conformance classes") } }));
    paths.insert("/api".into(), json!({ "get": { "summary": "This definition", "operationId": "getApi", "responses": ok(OPENAPI, "The API definition") } }));
    paths.insert("/collections".into(), json!({ "get": { "summary": "Feature collections", "operationId": "getCollections", "responses": ok(JSON, "The collections") } }));
    for c in collections {
        let id = &c.id;
        let mut items_params = json!([
            param(
                "limit",
                "query",
                json!({ "type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": DEFAULT_LIMIT }),
                "Features per page"
            ),
            param(
                "offset",
                "query",
                json!({ "type": "integer", "minimum": 0, "default": 0 }),
                "Features skipped"
            ),
            param(
                "bbox",
                "query",
                json!({ "type": "array", "minItems": 4, "maxItems": 6, "items": { "type": "number" } }),
                "minx,miny,maxx,maxy in CRS84"
            ),
            param(
                "datetime",
                "query",
                json!({ "type": "string" }),
                if c.time.is_empty() {
                    "Ignored: the collection's shapes mark no time (tts:time)"
                } else {
                    "An RFC 3339 instant or interval (start/end, .. for an open end) the features' time must meet"
                }
            ),
            param(
                "crs",
                "query",
                json!({ "type": "string", "format": "uri", "enum": OutCrs::ALL.map(OutCrs::iri), "default": CRS84 }),
                "The CRS of the geometries",
            ),
            param(
                "bbox-crs",
                "query",
                json!({ "type": "string", "format": "uri", "enum": OutCrs::ALL.map(OutCrs::iri), "default": CRS84 }),
                "The CRS of bbox",
            ),
            param(
                "geometries",
                "query",
                json!({ "type": "string", "enum": ["one", "all"], "default": "one" }),
                "With several geometry values, the most detailed one or a GeometryCollection of all",
            ),
            param(
                "filter",
                "query",
                json!({ "type": "string" }),
                "A CQL2 filter on the queryables (see /queryables)",
            ),
            param(
                "filter-lang",
                "query",
                json!({ "type": "string", "enum": ["cql2-text", "cql2-json"], "default": "cql2-text" }),
                "The encoding of filter",
            ),
            param(
                "filter-crs",
                "query",
                json!({ "type": "string", "format": "uri", "enum": OutCrs::ALL.map(OutCrs::iri), "default": CRS84 }),
                "The CRS of the geometries in filter",
            ),
        ]);
        paths.insert(format!("/collections/{id}"), json!({ "get": { "summary": format!("The collection {id}"), "operationId": format!("describe_{id}"), "responses": ok(JSON, "The collection") } }));
        if let Value::Array(list) = &mut items_params {
            for key in &c.filters {
                list.push(param(
                    key,
                    "query",
                    json!({ "type": "string" }),
                    "Features with this value of the property",
                ));
            }
        }
        paths.insert(format!("/collections/{id}/queryables"), json!({ "get": { "summary": format!("Queryables of {id}"), "operationId": format!("getQueryables_{id}"), "responses": ok(SCHEMA, "The queryables, as a JSON Schema") } }));
        paths.insert(format!("/collections/{id}/items"), json!({ "get": { "summary": format!("Features of {id}"), "operationId": format!("getFeatures_{id}"), "parameters": items_params, "responses": ok_features("A page of features") } }));
        paths.insert(format!("/collections/{id}/items/{{featureId}}"), json!({ "get": { "summary": format!("One feature of {id}"), "operationId": format!("getFeature_{id}"), "parameters": [param("featureId", "path", json!({ "type": "string" }), "The feature's IRI (or, with a base IRI, its local part), percent-encoded")], "responses": ok_features("The feature") } }));
    }
    // A tag per collection (and one for the rest), and a description per operation, as
    // Spectral's OpenAPI rules ask.
    let mut tags = vec![
        json!({ "name": "Capabilities", "description": "The landing page, conformance classes, this definition and the list of collections" }),
    ];
    for c in collections {
        tags.push(json!({ "name": c.id, "description": format!("The collection {}", c.id) }));
    }
    for (path, item) in paths.iter_mut() {
        let tag = path
            .strip_prefix("/collections/")
            .map(|rest| rest.split('/').next().unwrap_or(rest).to_owned())
            .unwrap_or_else(|| "Capabilities".to_owned());
        if let Some(ops) = item.as_object_mut() {
            for op in ops.values_mut() {
                let summary = op["summary"].as_str().unwrap_or_default().to_owned();
                op["tags"] = json!([tag]);
                if op.get("description").is_none() {
                    op["description"] = format!("{summary}.").into();
                }
            }
        }
    }
    json!({
        "openapi": "3.0.3",
        "info": {
            "title": format!("{} (OGC API – Features)", entry.name),
            "version": env!("CARGO_PKG_VERSION"),
            "description": "OGC API – Features (Parts 1, 2 and 3) over the dataset's classes with a geometry.",
            "contact": { "name": entry.name, "url": base },
        },
        "servers": [{ "url": base }],
        "tags": tags,
        "paths": paths,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bbox_as_wkt() {
        assert_eq!(
            bbox("1,2,3,4", OutCrs::Crs84).unwrap(),
            "POLYGON((1 2, 3 2, 3 4, 1 4, 1 2))"
        );
        assert_eq!(
            bbox("1,2,0,3,4,9", OutCrs::Crs84).unwrap(),
            "POLYGON((1 2, 3 2, 3 4, 1 4, 1 2))"
        );
        assert!(
            bbox("170,0,-170,1", OutCrs::Crs84)
                .unwrap()
                .starts_with("MULTIPOLYGON")
        );
        assert!(bbox("1,2,3", OutCrs::Crs84).is_err());
        assert!(bbox("1,4,3,2", OutCrs::Crs84).is_err());
        assert!(bbox("a,b,c,d", OutCrs::Crs84).is_err());
    }

    #[test]
    fn other_crss() {
        // EPSG:4326 boxes and points are latitude first.
        assert_eq!(
            bbox("2,1,4,3", OutCrs::Epsg4326).unwrap(),
            "POLYGON((1 2, 3 2, 3 4, 1 4, 1 2))"
        );
        let mut g = json!({ "type": "LineString", "coordinates": [[1.0, 2.0], [3.0, 4.0]] });
        reproject(&mut g, OutCrs::Epsg4326);
        assert_eq!(g["coordinates"], json!([[2.0, 1.0], [4.0, 3.0]]));
        // Web Mercator, both ways.
        let (x, y) = OutCrs::WebMercator.project(10.75, 59.91);
        assert!((x - 1_196_684.526).abs() < 0.01 && (y - 8_379_727.582).abs() < 0.01);
        let (lon, lat) = OutCrs::WebMercator.unproject(x, y);
        assert!((lon - 10.75).abs() < 1e-9 && (lat - 59.91).abs() < 1e-9);
        let mut c = json!({ "type": "GeometryCollection", "geometries": [
            { "type": "Point", "coordinates": [0.0, 0.0] } ] });
        reproject(&mut c, OutCrs::WebMercator);
        assert_eq!(c["geometries"][0]["coordinates"], json!([0.0, 0.0]));
        assert!(bbox("2000000,0,1000000,1", OutCrs::WebMercator).is_err());
    }

    #[test]
    fn percent_round_trip() {
        let iri = "http://example.com/a b#c/d?e";
        assert_eq!(percent_decode(&percent_encode(iri)).unwrap(), iri);
        assert!(percent_decode("%zz").is_none());
    }
}
