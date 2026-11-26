//! `/{ds}/tiles/{collection|all}/{z}/{x}/{y}.mvt`: Mapbox Vector Tiles of a dataset (#138), in
//! Web Mercator (EPSG:3857, the XYZ scheme of web maps). A layer per collection the `map`
//! reader reads (see [`crate::ogc::collections_for`]), named by its id; each feature carries
//! its IRI as `iri`. Tiles are cached in memory (capped in bytes) per store generation and shapes
//! version, so a write or a change of the shapes invalidates them.

use crate::View;
use crate::admin::Entry;
use crate::ogc::{Collection, Facet, collections_for, data_version, run, visible};
use crate::{HttpError, Params, Reply, State};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tripletrail::geo::mvt::{Feature, as_mvt_geom_simplified, encode_tile};
use tripletrail::geo::parse_wkt_literal;
use tripletrail::oxigraph::model::{Literal, NamedNode, Term};
use tripletrail::shapes::Reader;

const GEOF: &str = "http://www.opengis.net/def/function/geosparql/";
const WKT: &str = "http://www.opengis.net/ont/geosparql#wktLiteral";
const EPSG3857: &str = "http://www.opengis.net/def/crs/EPSG/0/3857";
/// Half the width of the Web Mercator square, in metres.
const HALF: f64 = 20_037_508.342_789_244;
const EXTENT: u32 = 4096;
/// Tile units around the tile kept, so lines and symbols cross tile edges cleanly.
const BUFFER: u32 = 64;
/// Below this zoom, lines and polygons are simplified to [`SIMPLIFY_UNITS`] tile units.
const SIMPLIFY_BELOW: u32 = 14;
const SIMPLIFY_UNITS: f64 = 1.0;
/// Below this zoom a layer of more than [`CROWDED`] features is generalised (#92):
/// [`LOW_EXTENT`] units a side instead of [`EXTENT`], and one feature per position, the one
/// drawn on top (the last), as a country or a continent then shows far more features than
/// pixels. Lines and polygons smaller than a unit are left out. A layer of fewer keeps every
/// feature.
const LOW_ZOOM_BELOW: u32 = 8;
const LOW_EXTENT: u32 = 512;
const CROWDED: usize = 1000;
/// Deepest zoom served.
const MAX_ZOOM: u32 = 24;
/// Features in one layer of one tile; past it the tile is refused, so a client zooms in.
const MAX_FEATURES: usize = 200_000;
/// Bytes of tiles kept in the cache, over all datasets; the oldest go first.
const CACHE_BYTES: usize = 256 << 20;
/// A layer tile larger than this is made again on every request rather than kept.
const CACHE_TILE_BYTES: usize = 4 << 20;
pub(crate) const MEDIA_TYPE: &str = "application/vnd.mapbox-vector-tile";

/// Dataset id, data version (store generation, shapes version), layer, z, x, y.
/// Dataset, grants' fingerprint (#203), data version, collection, z, x, y.
type Key = (u64, Option<u64>, (u64, Option<u64>), String, u32, u32, u32);
static CACHE: LazyLock<Mutex<TileCache>> =
    LazyLock::new(|| Mutex::new(TileCache::new(CACHE_BYTES, CACHE_TILE_BYTES)));

/// Layer tiles, capped in bytes, evicted oldest first.
struct TileCache {
    max_bytes: usize,
    max_tile: usize,
    bytes: usize,
    tiles: HashMap<Key, Arc<Vec<u8>>>,
    order: VecDeque<Key>,
}

impl TileCache {
    fn new(max_bytes: usize, max_tile: usize) -> Self {
        Self {
            max_bytes,
            max_tile,
            bytes: 0,
            tiles: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&self, key: &Key) -> Option<Arc<Vec<u8>>> {
        self.tiles.get(key).cloned()
    }

    fn insert(&mut self, key: Key, tile: Arc<Vec<u8>>) {
        let needed = cost(&key, &tile);
        if needed > self.max_tile || self.tiles.contains_key(&key) {
            return;
        }
        while self.bytes + needed > self.max_bytes {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(t) = self.tiles.remove(&old) {
                self.bytes -= cost(&old, &t);
            }
        }
        self.bytes += needed;
        self.order.push_back(key.clone());
        self.tiles.insert(key, tile);
    }
}

/// Bytes a kept tile is counted for besides its own and its layer name's: the rest of its key
/// and the bookkeeping. So empty tiles, and filtered ones with long names, fill the cache too.
const KEY_BYTES: usize = 96;

fn cost(key: &Key, tile: &[u8]) -> usize {
    // The key is held twice: in the map and in the eviction order.
    tile.len() + 2 * key.3.len() + KEY_BYTES
}

/// Previews kept at most; the oldest goes first.
const MAX_PREVIEWS: usize = 64;
/// How long a preview lasts after it is made.
const PREVIEW_LIFE: Duration = Duration::from_secs(3600);
/// The most text of shapes one preview takes.
const MAX_PREVIEW_BYTES: usize = 4 << 20;

/// Proposed shapes to draw the map with, before they are promoted (#146).
struct Preview {
    /// The dataset it is of (`Entry::id`).
    dataset: u64,
    made: std::time::Instant,
    files: Vec<tripletrail::shapes::SetFile>,
    /// Its collections per caller (`View::fingerprint`; None for an unlimited one).
    seen: Mutex<HashMap<Option<u64>, Arc<Vec<Collection>>>>,
}

/// The previews made, oldest first, by id.
type Previews = VecDeque<(String, Arc<Preview>)>;

static PREVIEWS: LazyLock<Mutex<Previews>> = LazyLock::new(|| Mutex::new(VecDeque::new()));

/// `POST /{ds}/tiles/preview`: keeps proposed shape files for an hour, so that the map can be
/// drawn with them in place of the dataset's own shapes (`/{ds}/tiles/preview/{id}/…`), and
/// answers `{id, style}`. The body is Turtle, or JSON `{"files": [{"name", "text"}]}`; files
/// that do not load are a 422, with what is wrong. Nothing of the dataset changes.
pub(crate) fn make_preview(
    state: &State,
    entry: &Entry,
    body: &str,
    json: bool,
    base: &str,
    view: &View,
) -> Result<Reply, HttpError> {
    use tripletrail::shapes::SetFile;
    if body.len() > MAX_PREVIEW_BYTES {
        return Err(HttpError(
            413,
            format!("A preview takes at most {MAX_PREVIEW_BYTES} bytes of shapes"),
        ));
    }
    let files = if json {
        let v: serde_json::Value = serde_json::from_str(body)
            .map_err(|e| HttpError::bad_request(format!("The body is not JSON: {e}")))?;
        let list = v
            .get("files")
            .and_then(|f| f.as_array())
            .ok_or_else(|| HttpError::bad_request("The body has no files"))?;
        list.iter()
            .map(|f| {
                let text = |k: &str| f.get(k).and_then(|t| t.as_str()).map(str::to_owned);
                Ok(SetFile {
                    name: text("name").unwrap_or_else(|| "preview.ttl".to_owned()),
                    text: text("text")
                        .ok_or_else(|| HttpError::bad_request("A file has no text"))?,
                })
            })
            .collect::<Result<Vec<_>, HttpError>>()?
    } else {
        vec![SetFile {
            name: "preview.ttl".to_owned(),
            text: body.to_owned(),
        }]
    };
    // Made now, so that errors in the files answer this request.
    let collections = crate::ogc::preview_collections(state, entry, Reader::Map, &files, view)?;
    let id = {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|e| HttpError(500, e.to_string()))?;
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    let preview = Preview {
        dataset: entry.id,
        made: std::time::Instant::now(),
        files,
        seen: Mutex::new(HashMap::from([(view.fingerprint(), collections)])),
    };
    {
        let mut kept = PREVIEWS.lock().unwrap_or_else(|e| e.into_inner());
        kept.retain(|(_, p)| p.made.elapsed() < PREVIEW_LIFE);
        while kept.len() >= MAX_PREVIEWS {
            kept.pop_front();
        }
        kept.push_back((id.clone(), Arc::new(preview)));
    }
    let body = serde_json::json!({
        "id": id,
        "style": format!("{base}/preview/{id}/style.json"),
    });
    let mut reply = Reply::bytes(201, "application/json", body.to_string().into_bytes());
    reply.headers.push(("Cache-Control", "no-store".to_owned()));
    Ok(reply)
}

/// The collections of preview `id` of `entry` for this caller.
fn preview(
    state: &State,
    entry: &Entry,
    id: &str,
    view: &View,
) -> Result<Arc<Vec<Collection>>, HttpError> {
    let found = PREVIEWS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(k, p)| k == id && p.dataset == entry.id && p.made.elapsed() < PREVIEW_LIFE)
        .map(|(_, p)| Arc::clone(p))
        .ok_or_else(|| HttpError(404, "No such preview: it may have expired".to_owned()))?;
    let fingerprint = view.fingerprint();
    if let Some(c) = found
        .seen
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&fingerprint)
    {
        return Ok(Arc::clone(c));
    }
    let made = crate::ogc::preview_collections(state, entry, Reader::Map, &found.files, view)?;
    let mut seen = found.seen.lock().unwrap_or_else(|e| e.into_inner());
    if seen.len() < 16 {
        seen.insert(fingerprint, Arc::clone(&made));
    }
    Ok(made)
}

/// The shape files of preview `id` of `entry`, for the other readers (`preview=` on a
/// describe page).
pub(crate) fn preview_files(
    entry: &Entry,
    id: &str,
) -> Result<Vec<tripletrail::shapes::SetFile>, HttpError> {
    PREVIEWS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(k, p)| k == id && p.dataset == entry.id && p.made.elapsed() < PREVIEW_LIFE)
        .map(|(_, p)| p.files.clone())
        .ok_or_else(|| HttpError(404, "No such preview: it may have expired".to_owned()))
}

/// The longest a dataset's warm-up may take ([`warm`]), whatever the timeouts.
pub(crate) const WARM_UP_LIMIT: Duration = Duration::from_secs(300);

/// What [`warm`] did.
pub(crate) struct Warmed {
    /// The map's collections.
    pub(crate) collections: usize,
    /// Those whose facets were counted, or failed (`id: error`, a facet query that timed out,
    /// say), or were left when the warm-up's time ran out.
    pub(crate) counted: usize,
    pub(crate) failed: Vec<String>,
    pub(crate) left: Vec<String>,
}

/// Makes the map's collections and the unfiltered facets of each ahead of the first visitor
/// (#92): the profile behind the collections and the first facet queries take seconds to
/// minutes on a large dataset, which the first map request would otherwise wait for. Their
/// results stay in the collection and result caches, as a request's would. Each facet query
/// gets the dataset's timeout, as a map request does, and the whole warm-up at most `within`:
/// the collections left when it is over are made by the first request that needs them.
pub(crate) fn warm(state: &State, entry: &Entry, within: Duration) -> Result<Warmed, HttpError> {
    if data_version(state, entry).is_none() {
        return Err(HttpError(
            400,
            "its store keeps no map caches (only TripleTrail's store does)".to_owned(),
        ));
    }
    let deadline = std::time::Instant::now() + within;
    let (collections, made_for) = collections_for(state, entry, Reader::Map)?;
    // The OGC API's collections are made from the same profile: cheap once it is cached.
    collections_for(state, entry, Reader::OgcFeatures)?;
    let version = made_for.filter(|v| Some(*v) == data_version(state, entry));
    let limit = crate::limits::Limits::of(state, &entry.name).timeout(None);
    let none = Params(Vec::new());
    let mut out = Warmed {
        collections: collections.len(),
        counted: 0,
        failed: Vec::new(),
        left: Vec::new(),
    };
    for c in collections.iter() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            out.left.push(c.id.clone());
            continue;
        }
        let timeout = Some(limit.map_or(left, |t| t.min(left)));
        match facet_counts(
            entry,
            c,
            &collections,
            &none,
            timeout,
            &View::default(),
            version,
        ) {
            Ok(_) => out.counted += 1,
            Err(e) => out.failed.push(format!("{}: {}", c.id, e.1)),
        }
    }
    Ok(out)
}

/// Whether `id` has the form of a preview's id (128 random bits in hex).
fn is_preview_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Answers `GET /{ds}/tiles/{rest}`; `base` is the absolute URL of `/{ds}/tiles`.
/// `/{ds}/tiles/preview/{id}/{rest}` answers the same with a preview's shapes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn route(
    state: &State,
    entry: &Entry,
    rest: &[&str],
    params: &Params,
    base: &str,
    timeout: Option<Duration>,
    view: &View,
    lang: &str,
) -> Result<Reply, HttpError> {
    let (rest, previewed, base) = match rest {
        // A preview's id is 32 hex digits, which no tile's zoom is: a collection whose id is
        // "preview" keeps its tiles at preview/{z}/{x}/{y}.
        ["preview", id, more @ ..] if is_preview_id(id) => (
            more,
            Some(preview(state, entry, id, view)?),
            format!("{base}/preview/{id}"),
        ),
        _ => (rest, None, base.to_owned()),
    };
    let base = base.as_str();
    // The collections this caller sees, and the data version they were made for (None for a
    // preview, whose tiles are not kept).
    let shown = || -> Result<(Arc<Vec<Collection>>, Option<crate::ogc::DataVersion>), HttpError> {
        match &previewed {
            Some(c) => Ok((Arc::clone(c), None)),
            None => {
                let (collections, made_for) = collections_for(state, entry, Reader::Map)?;
                let collections = visible(state, entry, Reader::Map, &collections, made_for, view)?;
                Ok((collections, made_for))
            }
        }
    };
    if rest == ["style.json"] {
        let (collections, _) = shown()?;
        let basemap = state.options.map_tiles.as_deref().map(|t| {
            (
                t,
                state.options.map_attribution.as_deref().unwrap_or_default(),
            )
        });
        let mut style = style(&entry.name, &collections, base, basemap);
        style["metadata"]["tripletrail:basemaps"] = basemaps(
            state.options.map_tiles.as_deref(),
            state.options.map_attribution.as_deref(),
            &state.options.map_basemaps,
        );
        // The data the map shows, for citing it (#37): the store's write generation, and the
        // version of the dataset's own shapes when it has some.
        if let Some((generation, shapes)) = data_version(state, entry) {
            style["metadata"]["tripletrail:dataVersion"] = match shapes {
                Some(shapes) => format!("{generation}-{shapes}"),
                None => generation.to_string(),
            }
            .into();
        }
        let mut reply = Reply::bytes(200, "application/json", style.to_string().into_bytes());
        reply.headers.push(("Cache-Control", "no-cache".to_owned()));
        return Ok(reply);
    }
    if rest == ["search"] {
        let (collections, _) = shown()?;
        let q = params.get("q").unwrap_or_default();
        let found = search(entry, &collections, q, timeout, view)?;
        let mut reply = Reply::bytes(200, "application/json", found.to_string().into_bytes());
        reply.headers.push(("Cache-Control", "no-cache".to_owned()));
        return Ok(reply);
    }
    if rest == ["facets"] {
        // Several collections in one request (#92): each one's counts, and the facets they
        // share summed over them.
        let (collections, made_for) = shown()?;
        let version = made_for.filter(|v| Some(*v) == data_version(state, entry));
        let mut chosen: Vec<&Collection> = Vec::new();
        for (k, id) in &params.0 {
            if k != "collection" {
                continue;
            }
            let c = collections
                .iter()
                .find(|c| c.id == *id)
                .ok_or_else(|| HttpError(404, format!("No collection {id}")))?;
            if !chosen.iter().any(|x| x.id == c.id) {
                chosen.push(c);
            }
        }
        if chosen.is_empty() {
            return Err(HttpError::bad_request(
                "Name the collections: collection=ID, repeated",
            ));
        }
        if chosen.len() > MAX_FACET_COLLECTIONS {
            return Err(HttpError::bad_request(format!(
                "More than {MAX_FACET_COLLECTIONS} collections"
            )));
        }
        // One deadline for the whole request, not a timeout per collection.
        let deadline = timeout.map(|t| std::time::Instant::now() + t);
        let mut each = Vec::with_capacity(chosen.len());
        for c in &chosen {
            let left = match deadline {
                Some(d) => match d.checked_duration_since(std::time::Instant::now()) {
                    Some(left) if !left.is_zero() => Some(left),
                    _ => return Err(HttpError(503, crate::TIMED_OUT.to_owned())),
                },
                None => None,
            };
            each.push(facet_counts(
                entry,
                c,
                &collections,
                params,
                left,
                view,
                version,
            )?);
        }
        label_values(
            entry,
            &mut each,
            lang,
            chosen.first().and_then(|c| c.label_strip.as_ref()),
            deadline.map(|d| d.saturating_duration_since(std::time::Instant::now())),
            view,
        )?;
        let counts = combine_facets(each);
        let mut reply = Reply::bytes(200, "application/json", counts.to_string().into_bytes());
        reply.headers.push(("Cache-Control", "no-cache".to_owned()));
        return Ok(reply);
    }
    if let ["facets", id] = rest {
        let (collections, _) = shown()?;
        let collection = collections
            .iter()
            .find(|c| c.id == *id)
            .ok_or_else(|| HttpError(404, format!("No collection {id}")))?;
        let (_, made_for) = shown()?;
        let version = made_for.filter(|v| Some(*v) == data_version(state, entry));
        let mut counts = facet_counts(
            entry,
            collection,
            &collections,
            params,
            timeout,
            view,
            version,
        )?;
        label_values(
            entry,
            std::slice::from_mut(&mut counts),
            lang,
            collection.label_strip.as_ref(),
            timeout,
            view,
        )?;
        let mut reply = Reply::bytes(200, "application/json", counts.to_string().into_bytes());
        reply.headers.push(("Cache-Control", "no-cache".to_owned()));
        return Ok(reply);
    }
    let [layer, z, x, y] = rest else {
        return Err(HttpError(
            404,
            "Tiles are at /{ds}/tiles/{collection|all}/{z}/{x}/{y}.mvt".to_owned(),
        ));
    };
    let y = y.strip_suffix(".mvt").unwrap_or(y);
    let coordinate = |s: &str| s.parse::<u32>().ok();
    let (Some(z), Some(x), Some(y)) = (coordinate(z), coordinate(x), coordinate(y)) else {
        return Err(HttpError::bad_request("z, x and y must be whole numbers"));
    };
    if z > MAX_ZOOM || u64::from(x) >= 1 << z || u64::from(y) >= 1 << z {
        return Err(HttpError(404, format!("No tile {z}/{x}/{y}")));
    }
    // While the collections are made again after a write, the older ones answer: tiles made
    // from them are not kept (nor looked up), so none outlives them, and the write still
    // shows in the next tile.
    // A caller who may read only some graphs (#203) sees the collections with features in
    // them, and tiles of those graphs, kept apart from other callers' tiles.
    let (collections, made_for) = shown()?;
    let version = made_for.filter(|v| Some(*v) == data_version(state, entry));
    let grants = view.fingerprint();
    let chosen: Vec<&Collection> = if *layer == "all" {
        collections.iter().collect()
    } else {
        let one = collections
            .iter()
            .find(|c| c.id == *layer)
            .ok_or_else(|| HttpError(404, format!("No collection {layer}")))?;
        vec![one]
    };
    let shown: Vec<&Collection> = chosen
        .into_iter()
        .filter(|c| c.min_zoom.is_none_or(|min| z >= min))
        .collect();
    // Facet filters of the collections, and with them the cache key's layer name.
    let single = *layer != "all";
    let mut filters: HashMap<&str, Filter> = HashMap::new();
    // An area of another layer is joined only within the tile.
    let window = tile_window(z, x, y);
    for c in &shown {
        let scope = scope_filter(entry, view, c, &collections, Some(&window), params, false)?;
        let filter = facet_filter(c, params, single, None, &scope)?;
        if !filter.sparql.is_empty() || filter.color.is_some() {
            filters.insert(&c.id, filter);
        }
    }
    let key = |c: &Collection| {
        let name = match filters.get(c.id.as_str()) {
            Some(f) => format!("{}?{}", c.id, f.signature),
            None => c.id.clone(),
        };
        version.map(|v| (entry.id, grants, v, name, z, x, y))
    };
    let mut tiles: HashMap<&str, Arc<Vec<u8>>> = HashMap::new();
    {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        for c in &shown {
            if let Some(tile) = key(c).and_then(|k| cache.get(&k)) {
                tiles.insert(&c.id, tile);
            }
        }
    }
    let missing: Vec<&Collection> = shown
        .iter()
        .copied()
        .filter(|c| !tiles.contains_key(c.id.as_str()))
        .collect();
    for (id, tile) in make_tiles(entry, &missing, &filters, z, x, y, timeout, view)? {
        let tile = Arc::new(tile);
        if let Some(collection) = missing.iter().find(|c| c.id == id)
            && let Some(key) = key(collection)
        {
            CACHE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key, Arc::clone(&tile));
        }
        if let Some(collection) = missing.iter().find(|c| c.id == id) {
            tiles.insert(&collection.id, tile);
        }
    }
    let mut body = Vec::new();
    for c in &shown {
        if let Some(tile) = tiles.get(c.id.as_str()) {
            body.extend_from_slice(tile);
        }
    }
    Ok(Reply::bytes(200, MEDIA_TYPE, body))
}

/// Values of one facet a filter takes at most.
const MAX_FILTER_VALUES: usize = 1_000;
/// Values of one facet counted at most (the most frequent).
const FACET_VALUES: usize = 50;
/// The most classes a map coloured by a range draws: the web UI's colours for them.
const RANGE_CLASSES: usize = 8;

const LABELS: &str =
    "<http://www.w3.org/2000/01/rdf-schema#label>|<http://www.w3.org/2004/02/skos/core#prefLabel>";
/// IRIs one query for facet values' labels names at most.
const LABEL_BATCH: usize = 500;

/// How well a label in `lang` suits a reader of `wanted` (lower is better, #260): the
/// reader's language, then its primary language (`en` for `en-GB`, either way round), then
/// English, then no language, then any other.
pub(crate) fn label_rank(lang: Option<&str>, wanted: &str) -> u8 {
    let primary = |t: &str| t.split('-').next().unwrap_or(t).to_ascii_lowercase();
    match lang {
        Some(l) if l.eq_ignore_ascii_case(wanted) => 0,
        Some(l) if primary(l) == primary(wanted) => 1,
        Some(l) if primary(l) == "en" => 2,
        None => 3,
        Some(_) => 4,
    }
}

/// Labels the IRI values of the facets of `answers` (each one collection's
/// [`facet_counts`]): the `rdfs:label` or `skos:prefLabel` that suits a reader of `lang`
/// best ([`label_rank`]), without what `strip` matches (#260). The counts are kept for the
/// data version whatever the language; the labels are looked up for each request.
fn label_values(
    entry: &Entry,
    answers: &mut [serde_json::Value],
    lang: &str,
    strip: Option<&regex::Regex>,
    timeout: Option<Duration>,
    view: &View,
) -> Result<(), HttpError> {
    let mut iris: Vec<String> = answers
        .iter()
        .filter_map(|a| a["facets"].as_array())
        .flatten()
        .filter_map(|f| f["values"].as_array())
        .flatten()
        .filter_map(|v| v["value"].as_str())
        .filter(|v| NamedNode::new(*v).is_ok())
        .map(str::to_owned)
        .collect();
    iris.sort_unstable();
    iris.dedup();
    let mut best: HashMap<String, (u8, String)> = HashMap::new();
    for batch in iris.chunks(LABEL_BATCH) {
        let values: Vec<String> = batch.iter().map(|i| format!("<{i}>")).collect();
        let rows = run(
            entry,
            &format!(
                "SELECT ?v ?l WHERE {{ VALUES ?v {{ {} }} ?v {LABELS} ?l FILTER(isLiteral(?l)) }}",
                values.join(" ")
            ),
            timeout,
            view,
        )?;
        for row in rows {
            let (Some(Term::NamedNode(v)), Some(Term::Literal(l))) = (&row[0], &row[1]) else {
                continue;
            };
            let rank = label_rank(l.language(), lang);
            let label = l.value();
            match best.get(v.as_str()) {
                // Of two labels as good, the least, so that every answer is the same.
                Some((r, known)) if (*r, known.as_str()) <= (rank, label) => {}
                _ => {
                    best.insert(v.as_str().to_owned(), (rank, label.to_owned()));
                }
            }
        }
    }
    for answer in answers {
        // `get_mut`, not indexing, which would add the keys an answer has not.
        let Some(facets) = answer.get_mut("facets").and_then(|f| f.as_array_mut()) else {
            continue;
        };
        for value in facets
            .iter_mut()
            .filter_map(|f| f.get_mut("values").and_then(|v| v.as_array_mut()))
            .flatten()
        {
            if let Some((_, label)) = value["value"].as_str().and_then(|v| best.get(v)) {
                value["label"] = crate::ogc::stripped(label, strip).into();
            }
        }
    }
    Ok(())
}

/// A collection's facet filters, as SPARQL over `?f`, and a signature of them for the cache.
#[derive(Default)]
struct Filter {
    sparql: String,
    signature: String,
    /// The pattern that binds `?v` to the values of `?f` of the facet whose value each
    /// feature carries as `value` (`color.{id}={key}`, or `color={key}` when bare), for a map
    /// coloured by it, and whether it is a range facet (its features' highest number, not
    /// their least value).
    color: Option<(String, bool)>,
    /// The key of the facet the map is coloured by, when it is one.
    color_key: Option<String>,
    /// Whether a computed facet (`tts:select`) is among the filters or is the colour: queries
    /// that read it get at most [`COMPUTED_TIMEOUT`], as the facet's own counts do.
    computed: bool,
}

/// The facet filters of `collection` in `params`: `{id}.{key}=value`, and `{key}=value` too
/// when `bare` (a tile of the collection alone, or its facets). Several values of one key are
/// alternatives; keys must all hold. Values are compared with the property's values as text,
/// as the OGC API's property filters. `tts:Facet-Text` facets take `{key}.text=…`, text a value
/// contains whatever its case. `except` leaves out one key (the facet being counted).
fn facet_filter(
    collection: &Collection,
    params: &Params,
    bare: bool,
    except: Option<&str>,
    scope: &Scope,
) -> Result<Filter, HttpError> {
    let mut out = Filter {
        signature: scope.signature.clone(),
        ..Filter::default()
    };
    let mut sign = |part: String| {
        if !out.signature.is_empty() {
            out.signature.push('&');
        }
        out.signature.push_str(&part);
    };
    let mut sparql = scope.sparql.clone();
    let mut computed = false;
    for (i, facet) in collection.facets.iter().enumerate() {
        if except == Some(facet.key.as_str()) {
            continue;
        }
        if facet.range {
            let bound = |end: &str| -> Result<Option<String>, HttpError> {
                let dotted = format!("{}.{}.{end}", collection.id, facet.key);
                let bare_name = format!("{}.{end}", facet.key);
                let Some((_, v)) = params
                    .0
                    .iter()
                    .find(|(k, _)| *k == dotted || (bare && *k == bare_name))
                else {
                    return Ok(None);
                };
                range_bound(v, facet.datatype.as_deref())
                    .map(Some)
                    .ok_or_else(|| {
                        HttpError::bad_request(format!("{bare_name}: {v} is not a bound of it"))
                    })
            };
            let (min, max) = (bound("min")?, bound("max")?);
            if min.is_none() && max.is_none() {
                continue;
            }
            let mut tests = Vec::new();
            if let Some(min) = &min {
                tests.push(format!("?facet{i} >= {min}"));
            }
            if let Some(max) = &max {
                tests.push(format!("?facet{i} <= {max}"));
            }
            sparql.push_str(&format!(
                "{} FILTER({})",
                facet.bind("?f", &format!("?facet{i}")),
                tests.join(" && ")
            ));
            computed |= facet.select.is_some();
            sign(format!(
                "{}={}..{}",
                facet.key,
                min.unwrap_or_default(),
                max.unwrap_or_default()
            ));
            continue;
        }
        if facet.text {
            // Text the value contains, ignoring case: `{key}.text=…`.
            let dotted = format!("{}.{}.text", collection.id, facet.key);
            let bare_name = format!("{}.text", facet.key);
            let Some((_, needle)) = params
                .0
                .iter()
                .find(|(k, v)| (*k == dotted || (bare && *k == bare_name)) && !v.trim().is_empty())
            else {
                continue;
            };
            let needle = serde_json::to_string(&needle.trim().to_lowercase()).expect("a string");
            sparql.push_str(&format!(
                " FILTER EXISTS {{{} FILTER(CONTAINS(LCASE(STR(?facet{i})), {needle})) }}",
                facet.bind("?f", &format!("?facet{i}"))
            ));
            computed |= facet.select.is_some();
            sign(format!("{}~{needle}", facet.key));
            continue;
        }
        let dotted = format!("{}.{}", collection.id, facet.key);
        let mut values: Vec<&str> = params
            .0
            .iter()
            .filter(|(k, _)| {
                *k == dotted || (bare && *k == facet.key && !SCOPE_KEYS.contains(&k.as_str()))
            })
            .map(|(_, v)| v.as_str())
            .collect();
        if values.is_empty() {
            continue;
        }
        values.sort_unstable();
        values.dedup();
        if values.len() > MAX_FILTER_VALUES {
            return Err(HttpError::bad_request(format!(
                "More than {MAX_FILTER_VALUES} values of {}",
                facet.key
            )));
        }
        let texts: Vec<String> = values
            .iter()
            .map(|v| serde_json::to_string(v).expect("a string"))
            .collect();
        sparql.push_str(&format!(
            "{} FILTER(STR(?facet{i}) IN ({}))",
            facet.bind("?f", &format!("?facet{i}")),
            texts.join(", ")
        ));
        computed |= facet.select.is_some();
        sign(format!("{}={}", facet.key, texts.join(",")));
    }
    out.sparql = sparql;
    out.computed = computed;
    if except.is_none() {
        let dotted = format!("color.{}", collection.id);
        let key = params
            .0
            .iter()
            .find(|(k, _)| *k == dotted || (bare && k == "color"))
            .map(|(_, v)| v.as_str());
        // A key that is no values or range facet is left out, as filters on keys that are no
        // facet are.
        let facet = key.and_then(|key| collection.facets.iter().find(|f| f.key == key && !f.text));
        if let Some(facet) = facet {
            let key = &facet.key;
            if !out.signature.is_empty() {
                out.signature.push('&');
            }
            out.signature.push_str(&format!("color={key}"));
            out.color = Some((facet.bind("?f", "?v"), facet.range));
            out.color_key = Some(key.clone());
            out.computed |= facet.select.is_some();
        }
    }
    Ok(out)
}

/// The classes a map coloured by a range facet draws, low to high, the last with no bound:
/// the shape's `tts:colorClasses` when they are 2 to [`RANGE_CLASSES`], else up to that many
/// of one round width over the values from `min` to `max`, as the web UI's map makes them.
fn color_classes(
    shape: &[(Option<f64>, Option<String>)],
    min: Option<f64>,
    max: Option<f64>,
) -> Vec<(Option<f64>, Option<String>)> {
    let last = shape.len().saturating_sub(1);
    let mut classes: Vec<(Option<f64>, Option<String>)> = shape
        .iter()
        .enumerate()
        .filter(|(i, (up_to, _))| *i == last || up_to.is_some())
        .map(|(_, k)| k.clone())
        .collect();
    if classes.len() < 2 || classes.len() > RANGE_CLASSES {
        classes.clear();
        if let (Some(min), Some(max)) = (min, max)
            && min < max
        {
            let raw = (max - min) / 5.0;
            let power = 10f64.powf(raw.log10().floor());
            let width = [1.0, 2.0, 2.5, 5.0, 10.0]
                .iter()
                .map(|m| m * power)
                .find(|w| *w >= raw)
                .unwrap_or(raw);
            let mut up = (min / width).floor() * width + width;
            while up < max && classes.len() < RANGE_CLASSES - 1 {
                // Twelve digits, as the UI rounds them: 0.1 + 0.2 is 0.3.
                let rounded = format!("{up:.11e}").parse().unwrap_or(up);
                classes.push((Some(rounded), None));
                up += width;
            }
        }
        classes.push((None, None));
    }
    if let Some(k) = classes.last_mut() {
        k.0 = None;
    }
    classes
}

/// The SELECT expressions that count the features whose highest number `?m` is in each of
/// `classes` (each up to its bound, inclusive, above the one before).
fn class_sums(classes: &[(Option<f64>, Option<String>)]) -> String {
    let bound = |i: usize| classes[i].0.map(|n| format!("{n:?}"));
    (0..classes.len())
        .map(|i| {
            let above = i
                .checked_sub(1)
                .and_then(bound)
                .map(|b| format!("?m > {b}"));
            let up_to = bound(i).map(|b| format!("?m <= {b}"));
            let test = match (above, up_to) {
                (Some(a), Some(u)) => format!("{a} && {u}"),
                (Some(t), None) | (None, Some(t)) => t,
                (None, None) => "true".to_owned(),
            };
            format!("(SUM(IF({test}, 1, 0)) AS ?class{i})")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The filters of every collection besides its facets (#92), as SPARQL over `?f` and their
/// part of the cache key's signature.
#[derive(Default)]
struct Scope {
    sparql: String,
    signature: String,
}

/// The parameters of [`scope_filter`], which a facet of the same key takes only as
/// `{collection}.{key}`.
const SCOPE_KEYS: [&str; 7] = ["bbox", "graph", "q", "area", "rel", "dist", "datetime"];
/// Words of `q` a text filter takes at most.
const SCOPE_WORDS: usize = 8;
/// Graphs a `graph` filter takes at most.
const SCOPE_GRAPHS: usize = 100;

/// The filters of `params` that every collection takes, for its tiles and facets alike (#92):
/// - `bbox=minLon,minLat,maxLon,maxLat` (CRS84): the features with a geometry that meets the
///   box, read through the spatial index, as `bbox` of the OGC API's items;
/// - `graph=IRI`, repeated for several: the features with triples in one of those graphs;
/// - `q=words`: the features with an indexed literal holding every word (full-text, as the
///   map's search box: case and Latin accents aside, the last word as a prefix);
/// - `area`, `rel` and `dist`: the features with a geometry in a relation to an area (#33,
///   [`crate::area`]).
/// - `datetime=instant` or `start/end` (`..` for an open end): the features whose `tts:time`
///   meets it, as `datetime` of the OGC API's items (#37); a collection without `tts:time`
///   keeps all of its features.
///
/// `window`: the tile's area (CRS84 WKT), which bounds the join of an `area=layer:{id}`.
/// `many`: for counts over the whole collection rather than a tile's features.
fn scope_filter(
    entry: &Entry,
    view: &View,
    collection: &Collection,
    layers: &[Collection],
    window: Option<&str>,
    params: &Params,
    many: bool,
) -> Result<Scope, HttpError> {
    let mut out = Scope::default();
    let mut sign = |part: String| {
        if !out.signature.is_empty() {
            out.signature.push('&');
        }
        out.signature.push_str(&part);
    };
    let mut sparql = String::new();
    if let Some(value) = params.get("bbox") {
        let window = crate::ogc::crs84_bbox(value)?;
        let ways = collection.geometry.near(entry, view, "?f", &window);
        sparql.push_str(&format!(
            " {{ SELECT DISTINCT ?f WHERE {{ {} FILTER(<{GEOF}sfIntersects>(?scopeW, \"{window}\"^^<{WKT}>)) }} }}",
            ways.pattern("?f", "?scopeW"),
        ));
        sign(format!("bbox={window}"));
    }
    if let Some(area) =
        crate::area::area_filter(entry, view, collection, layers, window, params, many)?
    {
        sparql.push_str(&area.sparql);
        sign(area.signature);
    }
    let mut graphs: Vec<&str> = params
        .0
        .iter()
        .filter(|(k, v)| k == "graph" && !v.is_empty())
        .map(|(_, v)| v.as_str())
        .collect();
    if !graphs.is_empty() {
        graphs.sort_unstable();
        graphs.dedup();
        if graphs.len() > SCOPE_GRAPHS {
            return Err(HttpError::bad_request(format!(
                "More than {SCOPE_GRAPHS} graphs"
            )));
        }
        let iris = graphs
            .iter()
            .map(|g| {
                NamedNode::new(*g)
                    .map(|n| n.to_string())
                    .map_err(|e| HttpError::bad_request(format!("graph {g}: {e}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        sparql.push_str(&format!(
            " FILTER EXISTS {{ VALUES ?scopeG {{ {} }} GRAPH ?scopeG {{ ?f ?scopeP ?scopeO }} }}",
            iris.join(" ")
        ));
        sign(format!("graph={}", iris.join(",")));
    }
    if let Some(datetime) = params.get("datetime").filter(|d| !d.is_empty()) {
        // Checked even for a collection without a time, so a wrong value is always a 400.
        let time = collection.time_filter(datetime)?;
        if !time.is_empty() {
            sparql.push_str(&format!(" FILTER EXISTS {{{time} }}"));
            sign(format!("datetime={datetime}"));
        }
    }
    if let Some(q) = params.get("q") {
        // Words only, as the search box takes them: the index's own syntax stays out.
        let words: Vec<String> = q
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .take(SCOPE_WORDS)
            .map(str::to_owned)
            .collect();
        if let Some((last, rest)) = words.split_last() {
            let searches: Vec<String> = rest
                .iter()
                .map(|w| format!("?f <{SEARCH}> \"{w}\" ."))
                .chain([format!("?f <{SEARCH}> \"{last}*\" .")])
                .collect();
            sparql.push_str(&format!(
                " {{ SELECT DISTINCT ?f WHERE {{ {} }} }}",
                searches.join(" ")
            ));
            sign(format!("q={}", words.join(" ")));
        }
    }
    out.sparql = sparql;
    Ok(out)
}

const SEARCH: &str = "http://w3id.org/tripletrail/fn#search";

/// The features of `collection` an `area=layer:{id}` takes (#33): its own filters
/// (`{id}.{key}=…`, never the bare ones), as SPARQL over `?f`, and their signature.
pub(crate) fn layer_selection(
    collection: &Collection,
    params: &Params,
) -> Result<(String, String), HttpError> {
    let filter = facet_filter(collection, params, false, None, &Scope::default())?;
    Ok((filter.sparql, filter.signature))
}

/// A range facet's bound as a SPARQL literal of its datatype: a number for numeric types,
/// a date or time for the others; `None` when `text` is not one.
fn range_bound(text: &str, datatype: Option<&str>) -> Option<String> {
    const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
    let text = text.trim();
    let local = datatype
        .and_then(|d| d.strip_prefix(XSD))
        .unwrap_or("decimal");
    match local {
        "date" | "dateTime" | "dateTimeStamp" | "gYear" | "gYearMonth" | "time" => {
            let ok = !text.is_empty()
                && text.len() <= 40
                && text
                    .chars()
                    .all(|c| c.is_ascii_digit() || "-:+.TZ".contains(c));
            ok.then(|| format!("\"{text}\"^^<{XSD}{local}>"))
        }
        _ => {
            let n: f64 = text.parse().ok()?;
            n.is_finite().then(|| format!("{n:?}"))
        }
    }
}

/// Results a map search gives at most, and of one class.
const SEARCH_RESULTS: usize = 20;
const SEARCH_PER_CLASS: usize = 5;
/// Candidates whose geometry a map search looks up at most.
const SEARCH_LOOKUPS: usize = 200;

/// The resources of `classes` with a label that holds every word of `q` (full-text,
/// `tt:search`; the last word as a prefix, for typing; case and Latin accents aside), each
/// once with its class and that label: those with the fewest other words first ("Paris Nord"
/// before "Paris Nord Poste 3"), then by score. From the index's best 2,000 labels.
pub(crate) fn label_matches(
    entry: &Entry,
    classes: &[NamedNode],
    q: &str,
    timeout: Option<Duration>,
    view: &View,
) -> Result<Vec<(NamedNode, NamedNode, String)>, HttpError> {
    // Words only: the search's own syntax and SPARQL's quotes stay out.
    let words: Vec<String> = q
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(8)
        .map(str::to_owned)
        .collect();
    // The index matches any word; a search wants all of them, the last as a prefix.
    let wanted: Vec<String> = tripletrail::text::words(q).take(8).collect();
    if words.is_empty() || wanted.is_empty() || classes.is_empty() {
        return Ok(Vec::new());
    }
    let text = format!("{}*", words.join(" "));
    let has_all = |label: &str| {
        let tokens: Vec<String> = tripletrail::text::words(label).collect();
        let (last, rest) = wanted.split_last().expect("not empty");
        rest.iter().all(|w| tokens.contains(w))
            && tokens.iter().any(|t| t.starts_with(last.as_str()))
    };
    let mut classes: Vec<String> = classes.iter().map(|c| c.to_string()).collect();
    classes.sort();
    classes.dedup();
    let rows = run(
        entry,
        &format!(
            // The index's best 2,000 labels, before the all-words test below: a common first
            // word may push the wanted ones out, which typing more words brings back.
            "SELECT ?f ?c ?score ?label WHERE {{ \
               ?f <http://w3id.org/tripletrail/fn#search> (\"{text}\" ?score ?label 2000) . \
               VALUES ?c {{ {} }} ?f a ?c }} \
             ORDER BY DESC(?score) STR(?f) LIMIT 2000",
            classes.join(" "),
        ),
        timeout,
        view,
    )?;
    let mut candidates: Vec<(usize, usize, NamedNode, NamedNode, String)> = Vec::new();
    let mut taken = std::collections::HashSet::new();
    for (rank, row) in rows.into_iter().enumerate() {
        let (Some(Term::NamedNode(f)), Some(Term::NamedNode(class))) = (&row[0], &row[1]) else {
            continue;
        };
        let label = match &row[3] {
            Some(Term::Literal(l)) => l.value().to_owned(),
            _ => continue,
        };
        if taken.contains(f) || !has_all(&label) {
            continue;
        }
        taken.insert(f.clone());
        let extra = tripletrail::text::words(&label)
            .count()
            .saturating_sub(wanted.len());
        candidates.push((extra, rank, f.clone(), class.clone(), label));
    }
    candidates.sort_by_key(|(extra, rank, ..)| (*extra, *rank));
    Ok(candidates
        .into_iter()
        .map(|(_, _, f, class, label)| (f, class, label))
        .collect())
}

/// `GET /{ds}/tiles/search?q=…`: the map's features whose labels match the words of `q`
/// (full-text, `tt:search`; the last word as a prefix, for typing), best first, each with
/// its collection and the box around its geometries (CRS84) to go to.
fn search(
    entry: &Entry,
    collections: &[Collection],
    q: &str,
    timeout: Option<Duration>,
    view: &View,
) -> Result<serde_json::Value, HttpError> {
    use geo::BoundingRect;
    use serde_json::json;
    use tripletrail::geo::{Crs, transform};
    if collections.is_empty() {
        return Ok(json!({ "results": [] }));
    }
    let classes: Vec<NamedNode> = collections.iter().map(|c| c.class.clone()).collect();
    let candidates = label_matches(entry, &classes, q, timeout, view)?;
    let mut results = Vec::new();
    let mut lookups = 0;
    // At most a few per class, so that a class with many similar labels (the geometries of
    // track sections, say) leaves room for the others.
    let mut per_class: HashMap<NamedNode, usize> = HashMap::new();
    for (f, class, label) in candidates {
        if results.len() == SEARCH_RESULTS {
            break;
        }
        if per_class.get(&class).copied().unwrap_or(0) == SEARCH_PER_CLASS {
            continue;
        }
        // Features without a geometry take no place, but the lookups are bounded.
        if lookups == SEARCH_LOOKUPS {
            break;
        }
        lookups += 1;
        let (f, class) = (&f, &class);
        // The first collection of its class with a geometry for it.
        for c in collections.iter().filter(|c| &c.class == class) {
            let geometries = run(
                entry,
                &format!(
                    "SELECT ?w WHERE {{ {} }} LIMIT 100",
                    c.geometry.pattern(&f.to_string(), "?w")
                ),
                timeout,
                view,
            )?;
            let mut bbox: Option<[f64; 4]> = None;
            for g in geometries {
                let Some(geom) = g[0].as_ref().and_then(parse_wkt_literal) else {
                    continue;
                };
                let Some(rect) = transform(&geom.geometry, &geom.crs, &Crs::Crs84)
                    .and_then(|g| g.bounding_rect())
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
            if let Some(bbox) = bbox {
                results.push(json!({
                    "iri": f.as_str(),
                    "label": crate::ogc::stripped(&label, c.label_strip.as_ref()),
                    "collection": c.id,
                    "bbox": bbox,
                }));
                *per_class.entry(class.clone()).or_default() += 1;
                break;
            }
        }
    }
    Ok(json!({ "results": results }))
}

/// `GET /{ds}/tiles/facets/{collection}`: the features of the collection that the facet
/// filters in `params` keep, and per `tts:Facet-Values` facet its most frequent values with
/// how many of those features have each, under the filters of the other facets (so a facet
/// shows what choosing another of its values would give). A computed facet (`tts:select`)
/// gets at most [`COMPUTED_TIMEOUT`], and its failure is remembered for the data version
/// (`version`, None for a preview or while collections are made again): the map asks again
/// on every filter change, and would otherwise wait the whole time again each time.
fn facet_counts(
    entry: &Entry,
    collection: &Collection,
    layers: &[Collection],
    params: &Params,
    timeout: Option<Duration>,
    view: &View,
    version: Option<crate::ogc::DataVersion>,
) -> Result<serde_json::Value, HttpError> {
    use serde_json::json;
    let class = collection.class.as_str();
    // Only the features the map draws (and the OGC API serves): those with a geometry.
    let mut drawn = format!(
        " FILTER EXISTS {{ {} }}",
        collection.geometry.pattern("?f", "?w")
    );
    let scope = scope_filter(entry, view, collection, layers, None, params, true)?;
    let all = facet_filter(collection, params, true, None, &scope)?;
    // The unfiltered counts are kept for the data version (#258): the warm-up counts them, and
    // the first visitor's map is answered from them whatever the result cache let go since.
    let unfiltered = version
        .filter(|_| {
            scope.signature.is_empty() && all.signature.is_empty() && all.color_key.is_none()
        })
        .map(|v| (entry.id, view.fingerprint(), v, collection.id.clone()));
    if let Some(key) = &unfiltered
        && let Some(counts) = UNFILTERED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
    {
        return Ok(counts.clone());
    }
    let number = |t: &Option<Term>| match t {
        Some(Term::Literal(l)) => l.value().parse::<u64>().unwrap_or(0),
        _ => 0,
    };
    let count = |filter: &Filter, drawn: &str| -> Result<u64, HttpError> {
        let rows = run(
            entry,
            &format!(
                "SELECT (COUNT(DISTINCT ?f) AS ?n) WHERE {{ ?f a <{class}> .{}{drawn} }}",
                filter.sparql
            ),
            capped(timeout, filter.computed),
            view,
        )?;
        Ok(rows.first().map_or(0, |r| number(&r[0])))
    };
    // The test is left out of the facet queries when every instance has a geometry, and
    // replaced by the few that have none when they are few: on a path with alternatives it
    // costs as much as the count itself, in each facet's query (#139, RINF tracks whose
    // geometry is their own or their section's; #946, 14 of RINF's 408,033 running tracks
    // without one made the first facets take 48 s). Both queries are cached across filters.
    let none = Filter::default();
    let total = count(&none, "")?;
    // Distinct: a feature typed in several graphs is one feature, as the counts have it.
    let undrawn = run(
        entry,
        &format!(
            "SELECT DISTINCT ?f WHERE {{ ?f a <{class}> FILTER NOT EXISTS {{ {} }} }} LIMIT {}",
            collection.geometry.pattern("?f", "?w"),
            UNDRAWN_NAMED + 1
        ),
        timeout,
        view,
    )?;
    let named: Option<Vec<String>> = (undrawn.len() <= UNDRAWN_NAMED)
        .then(|| {
            undrawn
                .iter()
                .map(|r| match r.first() {
                    Some(Some(Term::NamedNode(n))) => Some(format!("<{}>", n.as_str())),
                    _ => None,
                })
                .collect()
        })
        .flatten();
    let drawable = match named {
        Some(named) => {
            drawn = if named.is_empty() {
                String::new()
            } else {
                format!(" MINUS {{ VALUES ?f {{ {} }} }}", named.join(" "))
            };
            total.saturating_sub(named.len() as u64)
        }
        None => count(&none, &drawn)?,
    };
    let features = if all.sparql.is_empty() {
        drawable
    } else {
        count(&all, &drawn)?
    };
    let one = |facet: &Facet| -> Result<serde_json::Value, HttpError> {
        if facet.text {
            // Searched, not counted: its values are too many to list.
            return Ok(json!({ "key": facet.key, "title": facet.title, "text": true }));
        }
        let other = facet_filter(collection, params, true, Some(&facet.key), &scope)?;
        let timeout = capped(timeout, facet.select.is_some() || other.computed);
        let failed_key = version.filter(|_| facet.select.is_some()).map(|v| {
            (
                entry.id,
                view.fingerprint(),
                v,
                format!("{}\n{}\n{}", collection.id, facet.key, other.sparql),
            )
        });
        if let Some(key) = &failed_key
            && let Some(error) = FAILED.lock().unwrap_or_else(|e| e.into_inner()).get(key)
        {
            return Err(HttpError(error.0, error.1.clone()));
        }
        let run = |query: &str| {
            let started = std::time::Instant::now();
            let result = run(entry, query, timeout, view);
            // Only a timeout, run to its end: a memory budget or a request cancelled for
            // another reason may pass next time.
            if let (Err(e), Some(key), Some(limit)) = (&result, &failed_key, timeout)
                && e.1 == crate::TIMED_OUT
                && started.elapsed() >= limit
            {
                let mut failed = FAILED.lock().unwrap_or_else(|e| e.into_inner());
                // The dataset's older data versions are answered no more.
                failed.retain(|k, _| k.0 != key.0 || k.2 == key.2);
                if failed.len() >= FAILED_KEPT {
                    failed.clear();
                }
                failed.insert(key.clone(), HttpError(e.0, e.1.clone()));
            }
            result
        };
        if facet.range {
            // The span of the values, under the other facets' filters; numbers as numbers.
            let query = format!(
                "SELECT (MIN(?v) AS ?min) (MAX(?v) AS ?max) (COUNT(DISTINCT ?f) AS ?n) WHERE {{ \
                   ?f a <{class}> .{}{}{drawn} FILTER(isLiteral(?v)) }}",
                facet.bind("?f", "?v"),
                other.sparql,
            );
            let rows = run(&query)?;
            let bound = |t: &Option<Term>| match t {
                Some(Term::Literal(l)) => match l.value().parse::<f64>() {
                    Ok(n) if n.is_finite() => json!(n),
                    _ => json!(l.value()),
                },
                _ => serde_json::Value::Null,
            };
            let row = rows.first();
            let mut out = json!({
                "key": facet.key,
                "title": facet.title,
                "range": true,
                "min": row.map_or(serde_json::Value::Null, |r| bound(&r[0])),
                "max": row.map_or(serde_json::Value::Null, |r| bound(&r[1])),
                "count": row.map_or(0, |r| number(&r[2])),
            });
            if let Some(unit) = &facet.unit {
                out["unit"] = unit.clone().into();
            }
            if let Some(n) = facet.color_by {
                out["colorBy"] = n.into();
            }
            if all.color_key.as_deref() == Some(facet.key.as_str()) {
                // The map is coloured by it: its legend, with the features drawn in each class.
                let (min, max) = (out["min"].as_f64(), out["max"].as_f64());
                let classes = color_classes(&facet.classes, min, max);
                // Each feature in the class of its highest number, as its tiles colour it.
                let query = format!(
                    "SELECT {} WHERE {{ {{ SELECT ?f (MAX(?v) AS ?m) WHERE {{ \
                       ?f a <{class}> .{}{}{drawn} FILTER(isNumeric(?v)) }} GROUP BY ?f }} }}",
                    class_sums(&classes),
                    facet.bind("?f", "?v"),
                    all.sparql,
                );
                let rows = run(&query)?;
                let counts: Vec<u64> = (0..classes.len())
                    .map(|i| rows.first().map_or(0, |r| number(&r[i])))
                    .collect();
                out["classes"] = classes
                    .iter()
                    .zip(counts)
                    .map(|((up_to, label), n)| json!({ "upTo": up_to, "label": label, "count": n }))
                    .collect();
            } else if !facet.classes.is_empty() {
                out["classes"] = facet
                    .classes
                    .iter()
                    .map(|(up_to, label)| json!({ "upTo": up_to, "label": label }))
                    .collect();
            }
            return Ok(out);
        }
        // Ties at the cut are kept in the store's order: a second sort key costs a second on
        // RINF's 130,000 track ids.
        let query = format!(
            "SELECT ?v ?n WHERE {{ \
               {{ SELECT ?v (COUNT(DISTINCT ?f) AS ?n) WHERE {{ ?f a <{class}> .{}{}{drawn} }} \
                 GROUP BY ?v ORDER BY DESC(?n) LIMIT {} }} }} ORDER BY DESC(?n) STR(?v)",
            facet.bind("?f", "?v"),
            other.sparql,
            FACET_VALUES + 1,
        );
        let rows = run(&query)?;
        let more = rows.len() > FACET_VALUES;
        let values: Vec<serde_json::Value> = rows
            .iter()
            .take(FACET_VALUES)
            .filter_map(|r| {
                let value = match r[0].as_ref()? {
                    Term::NamedNode(n) => n.as_str().to_owned(),
                    Term::Literal(l) => l.value().to_owned(),
                    _ => return None,
                };
                // Labelled for the reader's language by `label_values` (#260).
                Some(json!({ "value": value, "label": null, "count": number(&r[1]) }))
            })
            .collect();
        let mut out = json!({
            "key": facet.key,
            "title": facet.title,
            "values": values,
            "more": more,
        });
        if let Some(n) = facet.color_by {
            out["colorBy"] = n.into();
        }
        Ok(out)
    };
    // A query per facet, on as many threads as tiles use: a collection of RINF's running
    // tracks has over 40 facets.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<Result<serde_json::Value, HttpError>>>> =
        collection.facets.iter().map(|_| Mutex::new(None)).collect();
    let workers = collection.facets.len().clamp(1, PARALLEL);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(facet) = collection.facets.get(i) else {
                        break;
                    };
                    let result = match one(facet) {
                        // A computed facet that fails (a pattern too slow for the data) leaves
                        // the others: it comes without values, and says why.
                        Err(e) if facet.select.is_some() => Ok(json!({
                            "key": facet.key,
                            "title": facet.title,
                            "values": [],
                            "more": false,
                            "error": e.1,
                        })),
                        result => result,
                    };
                    let failed = result.is_err();
                    *slots[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
                    if failed {
                        // The others stop at their next facet.
                        next.store(
                            collection.facets.len(),
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        break;
                    }
                }
            });
        }
    });
    let mut facets = Vec::with_capacity(slots.len());
    for slot in slots {
        // Empty only after another facet failed, which then returns its error.
        if let Some(result) = slot.into_inner().unwrap_or_else(|e| e.into_inner()) {
            facets.push(result?);
        }
    }
    // The same features as a SPARQL query, to open in the query page: each once, with one
    // of its geometries, so MatGUI can map them and the limit counts features.
    let query = format!(
        "SELECT ?f (SAMPLE(?g) AS ?geometry) WHERE {{\n  ?f a <{class}> .\n  {}\n {}\n}}\nGROUP BY ?f\nLIMIT 1000\n",
        collection.geometry.pattern("?f", "?g"),
        all.sparql,
    );
    let counts = json!({ "collection": collection.id, "features": features, "facets": facets, "query": query });
    // Kept only when every facet was counted: a computed facet that failed may pass next time.
    if let Some(key) = unfiltered
        && counts["facets"]
            .as_array()
            .is_some_and(|f| f.iter().all(|f| f.get("error").is_none()))
    {
        let mut kept = UNFILTERED.lock().unwrap_or_else(|e| e.into_inner());
        // The dataset's older data versions are answered no more.
        kept.retain(|k, _| k.0 != key.0 || k.2 == key.2);
        if kept.len() >= UNFILTERED_KEPT {
            kept.clear();
        }
        kept.insert(key, counts.clone());
    }
    Ok(counts)
}

/// Collections one request for facets takes at most.
const MAX_FACET_COLLECTIONS: usize = 64;

/// The facets of several collections, as `/{ds}/tiles/facets?collection=…` answers them:
/// `collections`, each one's own answer; `features`, their sum; and `shared`, the facets whose
/// key more than one of them has, summed over those: a value's counts added up (the 50 most
/// frequent kept, `more` when any collection had more: then the sums are lower bounds, as each
/// collection lists only its own 50 most frequent values), a range's lowest `min`, highest
/// `max` and its counts added up (when all its bounds are numbers, or all text such as dates,
/// and it has one unit). A feature of two of the classes is counted in each.
fn combine_facets(each: Vec<serde_json::Value>) -> serde_json::Value {
    use serde_json::{Value, json};
    let total: u64 = each.iter().filter_map(|c| c["features"].as_u64()).sum();
    // Facets by key, in the order they first come.
    let mut keys: Vec<String> = Vec::new();
    let mut by_key: HashMap<String, Vec<(&str, &Value)>> = HashMap::new();
    for c in &each {
        let collection = c["collection"].as_str().unwrap_or_default();
        for f in c["facets"].as_array().into_iter().flatten() {
            let Some(key) = f["key"].as_str() else {
                continue;
            };
            if !by_key.contains_key(key) {
                keys.push(key.to_owned());
            }
            by_key
                .entry(key.to_owned())
                .or_default()
                .push((collection, f));
        }
    }
    let mut shared = Vec::new();
    for key in keys {
        let facets = &by_key[&key];
        if facets.len() < 2 {
            continue;
        }
        let first = facets[0].1;
        let mut out = json!({
            "key": key,
            "title": first["title"],
            "collections": facets.iter().map(|(c, _)| *c).collect::<Vec<_>>(),
        });
        if facets.iter().any(|(_, f)| f["error"].is_string()) {
            out["error"] = facets
                .iter()
                .find_map(|(_, f)| f["error"].as_str())
                .unwrap_or_default()
                .into();
        }
        if facets.iter().all(|(_, f)| f["text"] == true) {
            out["text"] = true.into();
        } else if facets.iter().all(|(_, f)| f["range"] == true) {
            // One unit, or none: a span over metres and kilometres means nothing.
            let mut units: Vec<&Value> = facets.iter().map(|(_, f)| &f["unit"]).collect();
            units.dedup();
            if units.len() > 1 {
                continue;
            }
            // Bounds are numbers, or the text of dates and times (ISO, so they sort as text);
            // a collection without values has none. Of both kinds: not shared.
            let bounds: Vec<&Value> = facets
                .iter()
                .flat_map(|(_, f)| [&f["min"], &f["max"]])
                .filter(|v| !v.is_null())
                .collect();
            let (min, max) = if bounds.iter().all(|v| v.is_number()) {
                let numbers =
                    |key: &'static str| facets.iter().filter_map(move |(_, f)| f[key].as_f64());
                (
                    numbers("min").reduce(f64::min).map(Value::from),
                    numbers("max").reduce(f64::max).map(Value::from),
                )
            } else if bounds.iter().all(|v| v.is_string()) {
                let texts =
                    |key: &'static str| facets.iter().filter_map(move |(_, f)| f[key].as_str());
                (
                    texts("min").min().map(Value::from),
                    texts("max").max().map(Value::from),
                )
            } else {
                continue;
            };
            out["range"] = true.into();
            out["min"] = min.unwrap_or(Value::Null);
            out["max"] = max.unwrap_or(Value::Null);
            out["count"] = facets
                .iter()
                .filter_map(|(_, f)| f["count"].as_u64())
                .sum::<u64>()
                .into();
            if let Some(unit) = units.first().and_then(|u| u.as_str()) {
                out["unit"] = unit.into();
            }
        } else if facets.iter().all(|(_, f)| f["values"].is_array()) {
            let mut values: Vec<(String, Value, u64)> = Vec::new();
            for (_, f) in facets {
                for v in f["values"].as_array().into_iter().flatten() {
                    let value = v["value"].as_str().unwrap_or_default().to_owned();
                    let n = v["count"].as_u64().unwrap_or(0);
                    match values.iter_mut().find(|(x, _, _)| *x == value) {
                        Some(found) => {
                            found.2 += n;
                            if found.1.is_null() {
                                found.1 = v["label"].clone();
                            }
                        }
                        None => values.push((value, v["label"].clone(), n)),
                    }
                }
            }
            values.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
            let more = values.len() > FACET_VALUES || facets.iter().any(|(_, f)| f["more"] == true);
            values.truncate(FACET_VALUES);
            out["values"] = values
                .into_iter()
                .map(|(value, label, count)| json!({ "value": value, "label": label, "count": count }))
                .collect();
            out["more"] = more.into();
        } else {
            // Of different kinds in different collections: not summed.
            continue;
        }
        shared.push(out);
    }
    json!({ "collections": each, "features": total, "shared": shared })
}

/// The longest a computed facet's values are counted: on a large dataset its pattern can run
/// far longer than a plain path, and the map waits for every facet.
const COMPUTED_TIMEOUT: Duration = Duration::from_secs(10);
/// `timeout`, or at most [`COMPUTED_TIMEOUT`] for a query that reads a computed facet.
fn capped(timeout: Option<Duration>, computed: bool) -> Option<Duration> {
    if computed {
        Some(timeout.map_or(COMPUTED_TIMEOUT, |t| t.min(COMPUTED_TIMEOUT)))
    } else {
        timeout
    }
}

/// Collections' unfiltered facet counts kept, at most (#258).
const UNFILTERED_KEPT: usize = 4096;
/// Dataset, grants' fingerprint, data version and collection.
type UnfilteredKey = (u64, Option<u64>, crate::ogc::DataVersion, String);
/// The unfiltered facet counts of each collection, for the data version they were counted on.
static UNFILTERED: LazyLock<Mutex<HashMap<UnfilteredKey, serde_json::Value>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Failures of computed facets kept, at most.
const FAILED_KEPT: usize = 1024;
/// Dataset, grants' fingerprint, data version, and collection, facet and the other facets'
/// filter.
type FailedKey = (u64, Option<u64>, crate::ogc::DataVersion, String);
/// Computed facets that failed, for the data version they failed on.
static FAILED: LazyLock<Mutex<HashMap<FailedKey, HttpError>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Zoom past which a map overzooms the tiles of the deepest zoom it fetches: 4096 units over a
/// tile at zoom 18 are under 4 cm.
const STYLE_MAX_ZOOM: u32 = 18;
/// Colours of collections without an `tts:color`, in turn.
const PALETTE: [&str; 12] = [
    "#1f77b4", "#d62728", "#2ca02c", "#ff7f0e", "#9467bd", "#8c564b", "#e377c2", "#17becf",
    "#bcbd22", "#7f7f7f", "#393b79", "#637939",
];

/// A MapLibre style (version 8) of the dataset's map: its tiles as one vector source, and for
/// each collection a fill, a line and a circle layer (`{id}-fill`, `{id}-line`, `{id}-point`)
/// in the collection's colour, from its `tts:minZoom`, over the server's base map if it has
/// one. `metadata."tripletrail:collections"` lists the collections for a legend.
fn style(
    dataset: &str,
    collections: &[Collection],
    base: &str,
    basemap: Option<(&str, &str)>,
) -> serde_json::Value {
    use serde_json::json;
    let mut sources = serde_json::Map::new();
    let mut layers = Vec::new();
    match basemap {
        Some((tiles, attribution)) => {
            sources.insert(
                "basemap".into(),
                json!({
                    "type": "raster",
                    "tiles": [tiles],
                    "tileSize": 256,
                    "maxzoom": 19,
                    "attribution": attribution,
                }),
            );
            layers.push(json!({ "id": "basemap", "type": "raster", "source": "basemap" }));
        }
        None => layers.push(json!({
            "id": "background",
            "type": "background",
            "paint": { "background-color": "#f4f4f1" },
        })),
    }
    sources.insert(
        "tripletrail".into(),
        json!({
            "type": "vector",
            "tiles": [format!("{base}/all/{{z}}/{{x}}/{{y}}.mvt")],
            "minzoom": 0,
            "maxzoom": STYLE_MAX_ZOOM,
        }),
    );
    let of = |types: &[&str]| json!(["in", ["geometry-type"], ["literal", types]]);
    let mut legend = Vec::new();
    let mut spare = PALETTE.iter().cycle();
    // Areas below lines below points, so nothing hides what is smaller; within each, by
    // `tts:drawOrder` (higher on top), else the collections with more features first, below
    // the sparser ones.
    let mut order: Vec<&Collection> = collections.iter().collect();
    order.sort_by_key(|c| {
        (
            c.draw_order.unwrap_or(0),
            std::cmp::Reverse(c.instances.unwrap_or(0)),
        )
    });
    let mut fills = Vec::new();
    let mut lines = Vec::new();
    let mut points = Vec::new();
    for c in collections {
        let color = c
            .color
            .clone()
            .unwrap_or_else(|| (*spare.next().unwrap_or(&PALETTE[0])).to_owned());
        let min = c.min_zoom.unwrap_or(0);
        let layer = |kind: &str, ty: &str, filter: serde_json::Value, paint: serde_json::Value| {
            json!({
                "id": format!("{}-{kind}", c.id),
                "type": ty,
                "source": "tripletrail",
                "source-layer": c.id,
                "minzoom": min,
                "filter": filter,
                "paint": paint,
                "metadata": { "tripletrail:collection": c.id },
            })
        };
        fills.push(layer(
            "fill",
            "fill",
            of(&["Polygon", "MultiPolygon"]),
            json!({ "fill-color": color, "fill-opacity": 0.25 }),
        ));
        lines.push(layer(
            "line",
            "line",
            of(&["LineString", "MultiLineString", "Polygon", "MultiPolygon"]),
            json!({
                "line-color": color,
                "line-width": ["interpolate", ["linear"], ["zoom"], 4, 1, 14, 3],
            }),
        ));
        points.push(layer(
            "point",
            "circle",
            of(&["Point", "MultiPoint"]),
            json!({
                "circle-color": color,
                "circle-radius": ["interpolate", ["linear"], ["zoom"], 4, 2.5, 14, 6],
                "circle-stroke-color": "#ffffff",
                "circle-stroke-width": 1,
            }),
        ));
        let mut entry = json!({
            "id": c.id,
            "title": c.title,
            "class": c.class.as_str(),
            "color": color,
            "minZoom": min,
        });
        if c.has_time() {
            entry["time"] = true.into();
        }
        if let Some(description) = &c.description {
            entry["description"] = description.clone().into();
        }
        if let Some(n) = c.instances {
            entry["instances"] = n.into();
        }
        legend.push(entry);
    }
    // The legend by the shapes' `sh:order`, then by title; collections whose class the
    // profile did not find (shapes of the dataset's own for classes not in the data) last.
    let unknown =
        |c: &Collection| c.instances.is_none() && collections.iter().any(|o| o.instances.is_some());
    let mut listed: Vec<(&Collection, serde_json::Value)> =
        collections.iter().zip(legend).collect();
    listed.sort_by(|(a, _), (b, _)| {
        (
            unknown(a),
            a.order.unwrap_or(i64::MAX),
            a.title.to_lowercase(),
        )
            .cmp(&(
                unknown(b),
                b.order.unwrap_or(i64::MAX),
                b.title.to_lowercase(),
            ))
    });
    let legend: Vec<serde_json::Value> = listed.into_iter().map(|(_, e)| e).collect();
    let rank: HashMap<&str, usize> = order
        .iter()
        .enumerate()
        .map(|(i, c)| (c.id.as_str(), i))
        .collect();
    for kind in [&mut fills, &mut lines, &mut points] {
        kind.sort_by_key(|l| {
            l["metadata"]["tripletrail:collection"]
                .as_str()
                .and_then(|id| rank.get(id).copied())
                .unwrap_or(0)
        });
    }
    layers.extend(fills);
    layers.extend(lines);
    layers.extend(points);
    let mut metadata = json!({ "tripletrail:collections": legend });
    let views = map_views(collections);
    if !views.is_empty() {
        metadata["tripletrail:views"] = views.into();
    }
    // What the map page takes out of the labels it looks up itself (#260).
    if let Some(strip) = collections.first().and_then(|c| c.label_strip.as_ref()) {
        metadata["tripletrail:labelStrip"] = strip.as_str().into();
    }
    json!({
        "version": 8,
        "name": dataset,
        "metadata": metadata,
        "sources": sources,
        "layers": layers,
    })
}

/// The base maps the Map page lets viewers switch between: the server's own (`--map-tiles`,
/// key `map`) first, then each `--map-basemap` (keys `basemap-1`, `basemap-2`…), each with
/// its `key`, `title`, `tiles` (an XYZ URL template), `attribution` and `maxzoom` (the last
/// zoom of its tiles). The page adds a grey version of the first and a plain background.
fn basemaps(
    tiles: Option<&str>,
    attribution: Option<&str>,
    more: &[crate::Basemap],
) -> serde_json::Value {
    use serde_json::json;
    let mut list = Vec::new();
    if let Some(tiles) = tiles {
        let title = if tiles.contains("tile.openstreetmap.org") {
            "OpenStreetMap"
        } else {
            "Base map"
        };
        list.push(json!({
            "key": "map",
            "title": title,
            "tiles": tiles,
            "attribution": attribution.unwrap_or_default(),
            "maxzoom": crate::Basemap::DEFAULT_MAXZOOM,
        }));
    }
    for (i, b) in more.iter().enumerate() {
        list.push(json!({
            "key": format!("basemap-{}", i + 1),
            "title": b.title,
            "tiles": b.tiles,
            "attribution": b.attribution,
            "maxzoom": b.maxzoom,
        }));
    }
    list.into()
}

/// The map views of the collections (`tts:MapView`), by `sh:order` then title: for each, its
/// `key`, `title`, `description`, the `collections` it shows, and the `filters` (`{id: {key:
/// [values] | {min, max}}}`) and `colors` (`{id: key}`) it sets on them, and the `facets`
/// it lists first (`{id: [key]}`, also of collections it hides). A view shows only the
/// collections the caller sees; one that shows none is left out.
fn map_views(collections: &[Collection]) -> Vec<serde_json::Value> {
    use crate::ogc::ViewChoice;
    use serde_json::json;
    let mut views: Vec<(Option<i64>, String, serde_json::Value)> = Vec::new();
    for c in collections {
        for part in c.views.iter().filter(|p| p.shows) {
            let at = match views
                .iter()
                .position(|(_, _, v)| v["key"] == part.key.as_str())
            {
                Some(at) => at,
                None => {
                    let mut view = json!({
                        "key": part.key,
                        "title": part.title,
                        "collections": [],
                        "filters": {},
                        "colors": {},
                        "facets": {},
                    });
                    if let Some(d) = &part.description {
                        view["description"] = d.clone().into();
                    }
                    views.push((part.order, part.title.to_lowercase(), view));
                    views.len() - 1
                }
            };
            let view = &mut views[at].2;
            if let Some(list) = view["collections"].as_array_mut() {
                list.push(c.id.clone().into());
            }
            let mut filters = serde_json::Map::new();
            for (key, choice) in &part.filters {
                let value = match choice {
                    ViewChoice::Values(values) => json!(values),
                    ViewChoice::Range(min, max) => {
                        json!({ "min": min.clone().unwrap_or_default(), "max": max.clone().unwrap_or_default() })
                    }
                };
                filters.insert(key.clone(), value);
            }
            if !filters.is_empty() {
                view["filters"][c.id.as_str()] = filters.into();
            }
            if let Some(key) = &part.color {
                view["colors"][c.id.as_str()] = key.clone().into();
            }
            if !part.facets.is_empty() {
                view["facets"][c.id.as_str()] = part.facets.clone().into();
            }
        }
    }
    // A layer a view hides lists the view's facets first too, once shown by hand: of the
    // views listed, those that show a collection.
    for c in collections {
        for part in c.views.iter().filter(|p| !p.shows) {
            if let Some((_, _, view)) = views
                .iter_mut()
                .find(|(_, _, v)| v["key"] == part.key.as_str())
            {
                view["facets"][c.id.as_str()] = part.facets.clone().into();
            }
        }
    }
    views.sort_by(|a, b| (a.0.unwrap_or(i64::MAX), &a.1).cmp(&(b.0.unwrap_or(i64::MAX), &b.1)));
    views.into_iter().map(|(_, _, v)| v).collect()
}

/// The Web Mercator bounds of a tile: `(minx, miny, maxx, maxy)`, grown by `grow` of its width.
/// The area tile `z/x/y` draws, with its buffer, as CRS84 WKT.
fn tile_window(z: u32, x: u32, y: u32) -> String {
    let grow = f64::from(BUFFER) / f64::from(EXTENT);
    let (minx, miny, maxx, maxy) = bounds(z, x, y, grow);
    let (west, south) = lon_lat(minx, miny);
    let (east, north) = lon_lat(maxx, maxy);
    format!(
        "POLYGON(({west} {south}, {east} {south}, {east} {north}, {west} {north}, {west} {south}))"
    )
}

fn bounds(z: u32, x: u32, y: u32, grow: f64) -> (f64, f64, f64, f64) {
    let size = 2.0 * HALF / f64::from(1u32 << z.min(31));
    let margin = size * grow;
    let minx = -HALF + f64::from(x) * size;
    let maxy = HALF - f64::from(y) * size;
    (
        minx - margin,
        maxy - size - margin,
        minx + size + margin,
        maxy + margin,
    )
}

/// Longitude and latitude of a Web Mercator point.
fn lon_lat(x: f64, y: f64) -> (f64, f64) {
    let lon = (x / HALF * 180.0).clamp(-180.0, 180.0);
    let lat = (y / HALF * std::f64::consts::PI).sinh().atan().to_degrees();
    (lon, lat)
}

/// How detailed a WKT is: its coordinates, then its length (digits).
fn detail(wkt: &str) -> (usize, usize) {
    (wkt.matches(',').count(), wkt.len())
}

/// Layer tiles by collection id.
type Layers = Vec<(String, Vec<u8>)>;

/// Queries run at once for the layers of one tile.
const PARALLEL: usize = 4;

/// Most features without a geometry that the facet queries leave out by name rather than by
/// testing every feature's geometry path.
const UNDRAWN_NAMED: usize = 1000;

/// The layer tiles of `collections`, by collection id. Collections that read their geometry
/// through the same path share one query (with `VALUES` over their classes), so the spatial
/// index is asked once; those groups run on up to [`PARALLEL`] threads.
#[allow(clippy::too_many_arguments)]
fn make_tiles(
    entry: &Entry,
    collections: &[&Collection],
    filters: &HashMap<&str, Filter>,
    z: u32,
    x: u32,
    y: u32,
    timeout: Option<Duration>,
    view: &View,
) -> Result<Layers, HttpError> {
    let mut groups: Vec<Vec<&Collection>> = Vec::new();
    // A filtered collection is read on its own, with its filter.
    for c in collections {
        let shared = !filters.contains_key(c.id.as_str());
        match groups.iter_mut().find(|g| {
            shared && !filters.contains_key(g[0].id.as_str()) && g[0].geometry == c.geometry
        }) {
            Some(group) => group.push(c),
            None => groups.push(vec![c]),
        }
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Mutex<Vec<Result<Layers, HttpError>>> = Mutex::new(Vec::new());
    let workers = groups.len().clamp(1, PARALLEL);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(group) = groups.get(i) else { break };
                    let own = filters.get(group[0].id.as_str());
                    let filter = own.map_or("", |f| f.sparql.as_str());
                    let color = own.and_then(|f| f.color.as_ref().map(|(p, r)| (p.as_str(), *r)));
                    // Filtered or coloured by a computed facet: capped as its counts are.
                    let timeout = capped(timeout, own.is_some_and(|f| f.computed));
                    let result = group_tiles(entry, group, filter, color, z, x, y, timeout, view);
                    let failed = result.is_err();
                    results
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(result);
                    if failed {
                        // The others stop at their next group.
                        next.store(groups.len(), std::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                }
            });
        }
    });
    let mut out = Vec::new();
    for result in results.into_inner().unwrap_or_else(|e| e.into_inner()) {
        out.extend(result?);
    }
    Ok(out)
}

fn too_many(id: &str, z: u32, x: u32, y: u32) -> HttpError {
    HttpError(
        413,
        format!(
            "More than {MAX_FEATURES} features of {id} in tile {z}/{x}/{y}: zoom in, or give the \
             collection an tts:minZoom in the dataset's shapes"
        ),
    )
}

/// The layers of collections that share a geometry path, from one query.
#[allow(clippy::too_many_arguments)]
fn group_tiles(
    entry: &Entry,
    group: &[&Collection],
    filter: &str,
    color: Option<(&str, bool)>,
    z: u32,
    x: u32,
    y: u32,
    timeout: Option<Duration>,
    view: &View,
) -> Result<Layers, HttpError> {
    let window = tile_window(z, x, y);
    let mut classes: Vec<&NamedNode> = group.iter().map(|c| &c.class).collect();
    classes.sort();
    classes.dedup();
    let values: Vec<String> = classes.iter().map(|c| c.to_string()).collect();
    // The window first, so the spatial index picks the candidates; the classes are a check on
    // each. (With `VALUES ?c` the planner takes the classes for a few rows and scans them all.)
    let classes_check = if values.len() == 1 {
        format!("?f a {0} . BIND({0} AS ?c)", values[0])
    } else {
        format!("?f a ?c . FILTER(?c IN ({}))", values.join(", "))
    };
    let limit = MAX_FEATURES * classes.len();
    let query = format!(
        "SELECT ?c ?f ?w WHERE {{ {} {classes_check}{filter} }} LIMIT {}",
        group[0].geometry.near(entry, view, "?f", &window).filtered(
            "?f",
            "?w",
            &format!("FILTER(<{GEOF}sfIntersects>(?w, \"{window}\"^^<{WKT}>))")
        ),
        limit + 1,
    );
    let rows = run(entry, &query, timeout, view)?;
    if rows.len() > limit {
        return Err(too_many(&group[0].id, z, x, y));
    }
    // Per class, one geometry per feature, the most detailed, as the items do.
    let mut by_class: HashMap<NamedNode, (Vec<NamedNode>, HashMap<NamedNode, Term>)> =
        HashMap::new();
    for row in rows {
        let (Some(Term::NamedNode(c)), Some(Term::NamedNode(f)), Some(w @ Term::Literal(l))) =
            (&row[0], &row[1], &row[2])
        else {
            continue;
        };
        let (order, chosen) = by_class.entry(c.clone()).or_default();
        match chosen.get(f) {
            None => {
                order.push(f.clone());
                chosen.insert(f.clone(), w.clone());
            }
            Some(Term::Literal(old)) => {
                let (a, b) = (detail(l.value()), detail(old.value()));
                if a > b || (a == b && l.value() < old.value()) {
                    chosen.insert(f.clone(), w.clone());
                }
            }
            Some(_) => {}
        }
    }
    let (x1, y1, x2, y2) = bounds(z, x, y, 0.0);
    let tile_bounds = Term::from(Literal::new_typed_literal(
        format!("<{EPSG3857}> POLYGON(({x1} {y1}, {x2} {y1}, {x2} {y2}, {x1} {y2}, {x1} {y1}))"),
        NamedNode::new_unchecked(WKT),
    ));
    let tile_bounds = parse_wkt_literal(&tile_bounds)
        .ok_or_else(|| HttpError(500, "Could not build the tile's bounds".to_owned()))?;
    // Detail finer than this many tile units is dropped; deep zooms keep all of it.
    let tolerance = if z < SIMPLIFY_BELOW {
        SIMPLIFY_UNITS
    } else {
        0.0
    };
    let mut out = Vec::new();
    for collection in group {
        let empty = (Vec::new(), HashMap::new());
        let (order, chosen) = by_class.get(&collection.class).unwrap_or(&empty);
        if order.len() > MAX_FEATURES {
            return Err(too_many(&collection.id, z, x, y));
        }
        let low = z < LOW_ZOOM_BELOW && order.len() > CROWDED;
        let (extent, buffer) = if low {
            (LOW_EXTENT, BUFFER * LOW_EXTENT / EXTENT)
        } else {
            (EXTENT, BUFFER)
        };
        // By IRI, so a tile has the same bytes whatever order the query gave the rows in
        // (which differs between stores and runs).
        let mut order: Vec<&NamedNode> = order.iter().collect();
        order.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
        // The colouring facet's value of each feature: the least, as text, of its values.
        let mut colored: HashMap<NamedNode, String> = HashMap::new();
        if let Some((bind, range)) = color
            && !order.is_empty()
        {
            let iris: Vec<String> = order.iter().map(|f| f.to_string()).collect();
            // A range: the highest number, which the map puts in a class.
            let query = if range {
                format!(
                    "SELECT ?f (STR(MAX(?v)) AS ?value) WHERE {{ VALUES ?f {{ {} }}{bind} FILTER(isNumeric(?v)) }} GROUP BY ?f",
                    iris.join(" ")
                )
            } else {
                format!(
                    "SELECT ?f (MIN(STR(?v)) AS ?value) WHERE {{ VALUES ?f {{ {} }}{bind} }} GROUP BY ?f",
                    iris.join(" ")
                )
            };
            for row in run(entry, &query, timeout, view)? {
                if let (Some(Term::NamedNode(f)), Some(Term::Literal(v))) = (&row[0], &row[1]) {
                    colored.insert(f.clone(), v.value().to_owned());
                }
            }
        }
        let mut features = Vec::with_capacity(order.len());
        // At low zooms, the index in `features` of the feature at each position, so a later one
        // there replaces it.
        let mut at: HashMap<String, usize> = HashMap::new();
        for f in order {
            let Some(geometry) = chosen.get(f).and_then(parse_wkt_literal) else {
                continue;
            };
            let Some(geometry) = as_mvt_geom_simplified(
                &geometry,
                &tile_bounds,
                f64::from(extent),
                f64::from(buffer),
                true,
                tolerance,
            ) else {
                continue;
            };
            let replaces = if low {
                // In whole tile units already: equal shapes are drawn on the same pixels.
                let key = format!("{geometry:?}");
                match at.get(&key) {
                    Some(i) => Some(*i),
                    None => {
                        at.insert(key, features.len());
                        None
                    }
                }
            } else {
                None
            };
            let mut properties = vec![(
                "iri".to_owned(),
                Literal::new_simple_literal(f.as_str()).into(),
            )];
            if let Some(value) = colored.get(f) {
                properties.push((
                    "value".to_owned(),
                    Literal::new_simple_literal(value).into(),
                ));
            }
            let feature = Feature {
                geometry,
                id: None,
                properties,
            };
            match replaces {
                Some(i) => features[i] = feature,
                None => features.push(feature),
            }
        }
        out.push((
            collection.id.clone(),
            encode_tile(&collection.id, extent, &features),
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cache_is_capped_in_bytes() {
        let key = |n: u32| (1, None, (1, None), "layer".to_owned(), 0, n, 0);
        // Each 30-byte tile costs 30 + 2 × 5 (its layer name, held twice) + KEY_BYTES.
        let each = 40 + KEY_BYTES;
        let mut cache = TileCache::new(3 * each + 10, each + 10);
        for n in 0..3 {
            cache.insert(key(n), Arc::new(vec![0; 30]));
        }
        assert_eq!(cache.bytes, 3 * each);
        // The fourth pushes out the oldest.
        cache.insert(key(3), Arc::new(vec![0; 30]));
        assert_eq!(cache.bytes, 3 * each);
        assert!(cache.get(&key(0)).is_none());
        assert!(cache.get(&key(1)).is_some() && cache.get(&key(3)).is_some());
        // A tile over the per-tile cap is not kept.
        cache.insert(key(4), Arc::new(vec![0; 41]));
        assert!(cache.get(&key(4)).is_none());
        assert_eq!(cache.bytes, 3 * each);
        // Empty tiles with long (filtered) layer names count too, and push out others.
        let long = |n: u32| (1, None, (1, None), "x".repeat(20), 1, n, 0);
        for n in 0..3 {
            cache.insert(long(n), Arc::new(Vec::new()));
        }
        assert!(cache.get(&key(3)).is_none());
        assert_eq!(cache.bytes, 3 * each);
    }

    #[test]
    fn tile_bounds() {
        let (minx, miny, maxx, maxy) = bounds(0, 0, 0, 0.0);
        assert_eq!((minx, miny, maxx, maxy), (-HALF, -HALF, HALF, HALF));
        let (minx, _, _, maxy) = bounds(1, 1, 0, 0.0);
        assert_eq!((minx, maxy), (0.0, HALF));
        let (lon, lat) = lon_lat(HALF, HALF);
        assert_eq!(lon, 180.0);
        assert!((lat - 85.051_128_78).abs() < 1e-6);
    }
}
