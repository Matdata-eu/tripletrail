//! HTML pages of the OGC API – Features resources (the HTML conformance class): each one is
//! made from the JSON document of the same resource, so the two never say different things.
//! Pages carry a link to their JSON form; items and features get an SVG sketch of their
//! geometries.

use crate::{HttpError, Params, Reply};
use serde_json::Value;
use std::fmt::Write as _;

/// The media type of the pages.
pub(super) const HTML: &str = "text/html; charset=utf-8";
/// The media type of the pages in links (`type`), without parameters as clients match it.
pub(super) const LINK_TYPE: &str = "text/html";

/// Width and height of the geometry sketch, in SVG units.
const SKETCH: f64 = 640.0;

/// The page of the resource at `rest`, made from its JSON reply. `params` are the request's
/// query parameters, `base` the absolute URL of `/{ds}/ogc`.
pub(super) fn page(
    rest: &[&str],
    params: &Params,
    base: &str,
    reply: Reply,
    brand: &crate::branding::Branding,
) -> Result<Reply, HttpError> {
    let crs = reply
        .headers
        .iter()
        .find(|(name, _)| *name == "Content-Crs")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    let axes = Axes {
        swap: crs.contains("EPSG/0/4326"),
        geographic: !crs.contains("EPSG/0/3857"),
    };
    let headers = reply.headers;
    let mut body = Vec::new();
    let mut read = reply.body;
    std::io::Read::read_to_end(&mut read, &mut body)
        .map_err(|e| HttpError(500, format!("Reading the JSON document: {e}")))?;
    let doc: Value = serde_json::from_slice(&body)
        .map_err(|e| HttpError(500, format!("Reading the JSON document: {e}")))?;
    // Links in the pages keep `f=html` when the request named it (a client that does not
    // ask for HTML in `Accept`).
    let keep = params.get("f") == Some("html");
    let nav = |href: &str| {
        if keep {
            with_format(href, "html")
        } else {
            href.to_owned()
        }
    };
    let mut crumbs = vec![(base.to_owned(), "Landing page".to_owned())];
    let (title, main) = match rest {
        [] => (text(&doc["title"]), landing(&doc, &nav)),
        ["conformance"] => ("Conformance".to_owned(), conformance(&doc)),
        ["api"] => ("API definition".to_owned(), api(&doc)),
        ["collections"] => ("Collections".to_owned(), collections(&doc, &nav)),
        ["collections", _] => {
            crumbs.push((format!("{base}/collections"), "Collections".to_owned()));
            (text(&doc["title"]), collection(&doc, &nav))
        }
        ["collections", id, "queryables"] => {
            crumbs.push((format!("{base}/collections"), "Collections".to_owned()));
            crumbs.push((format!("{base}/collections/{id}"), (*id).to_owned()));
            (format!("{id}: queryables"), queryables(&doc))
        }
        ["collections", id, "items"] => {
            crumbs.push((format!("{base}/collections"), "Collections".to_owned()));
            crumbs.push((format!("{base}/collections/{id}"), (*id).to_owned()));
            (format!("{id}: features"), items(&doc, axes, &nav))
        }
        ["collections", id, "items", _, ..] => {
            crumbs.push((format!("{base}/collections"), "Collections".to_owned()));
            crumbs.push((format!("{base}/collections/{id}"), (*id).to_owned()));
            crumbs.push((
                format!("{base}/collections/{id}/items"),
                "Features".to_owned(),
            ));
            (text(&doc["id"]), feature(&doc, axes, &nav))
        }
        _ => return Err(HttpError(404, "Not found".to_owned())),
    };
    let json_url = with_format(&self_url(rest, params, base), "json");
    let mut out = String::new();
    let _ = write!(
        out,
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{}{}</title>{}<style>{STYLE}</style>\
         <link rel=\"alternate\" type=\"application/json\" href=\"{}\"></head><body><nav>",
        escape(&title),
        brand
            .name
            .as_deref()
            .map_or(String::new(), |n| format!(" · {}", escape(n))),
        brand.favicon_link(),
        escape(&json_url),
    );
    if !rest.is_empty() {
        let crumbs: Vec<String> = crumbs
            .iter()
            .map(|(href, label)| {
                format!("<a href=\"{}\">{}</a>", escape(&nav(href)), escape(label))
            })
            .collect();
        out.push_str(&crumbs.join(" / "));
    }
    let _ = writeln!(
        out,
        "</nav><main><h1>{}</h1>{main}</main><footer>This page as \
         <a href=\"{}\">JSON</a></footer>{}</body></html>",
        escape(&title),
        escape(&json_url),
        brand.footer_html(),
    );
    let mut reply = Reply::bytes(200, HTML, out.into_bytes());
    reply.headers = headers;
    Ok(reply)
}

/// The URL of the resource itself, with the request's parameters.
fn self_url(rest: &[&str], params: &Params, base: &str) -> String {
    let mut url = base.to_owned();
    for segment in rest {
        url.push('/');
        url.push_str(segment);
    }
    let mut query = form_urlencoded::Serializer::new(String::new());
    for (name, value) in &params.0 {
        query.append_pair(name, value);
    }
    let query = query.finish();
    if !query.is_empty() {
        url.push('?');
        url.push_str(&query);
    }
    url
}

/// `url` with its `f` parameter set to `format`.
pub(super) fn with_format(url: &str, format: &str) -> String {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let mut out = form_urlencoded::Serializer::new(String::new());
    for (name, value) in form_urlencoded::parse(query.as_bytes()) {
        if name != "f" {
            out.append_pair(&name, &value);
        }
    }
    out.append_pair("f", format);
    format!("{path}?{}", out.finish())
}

fn landing(doc: &Value, nav: &dyn Fn(&str) -> String) -> String {
    let mut out = format!("<p>{}</p><ul>", escape(&text(&doc["description"])));
    for link in links(doc) {
        if link.0 == "self" || link.0 == "alternate" {
            continue;
        }
        let href = if link.0 == "service-desc" {
            with_format(&link.1, "html")
        } else {
            nav(&link.1)
        };
        let _ = write!(
            out,
            "<li><a href=\"{}\">{}</a></li>",
            escape(&href),
            escape(&link.2)
        );
    }
    out.push_str("</ul>");
    out
}

fn conformance(doc: &Value) -> String {
    let mut out = String::from("<p>The conformance classes this API implements:</p><ul>");
    for class in doc["conformsTo"].as_array().into_iter().flatten() {
        let class = text(class);
        let _ = write!(out, "<li><a href=\"{0}\">{0}</a></li>", escape(&class));
    }
    out.push_str("</ul>");
    out
}

fn queryables(doc: &Value) -> String {
    let mut out = String::from(
        "<p>What a <code>filter</code> (CQL2) on the features may name:</p>\
         <table><thead><tr><th>Name</th><th>Title</th><th>Type</th></tr></thead><tbody>",
    );
    for (name, schema) in doc["properties"].as_object().into_iter().flatten() {
        let kind = match (schema["type"].as_str(), schema["format"].as_str()) {
            (Some(t), Some(f)) => format!("{t} ({f})"),
            (Some(t), None) => t.to_owned(),
            (None, Some(f)) => f.to_owned(),
            (None, None) => String::new(),
        };
        let _ = write!(
            out,
            "<tr><td><code>{}</code></td><td>{}</td><td>{}</td></tr>",
            escape(name),
            escape(&text(&schema["title"])),
            escape(&kind)
        );
    }
    out.push_str("</tbody></table>");
    out
}

fn api(doc: &Value) -> String {
    let mut out = format!(
        "<p>{}</p><table><thead><tr><th>Path</th><th>Operation</th><th>Parameters</th></tr></thead><tbody>",
        escape(&text(&doc["info"]["description"]))
    );
    for (path, item) in doc["paths"].as_object().into_iter().flatten() {
        let get = &item["get"];
        let names: Vec<String> = get["parameters"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| text(&p["name"]))
            .collect();
        let _ = write!(
            out,
            "<tr><td><code>{}</code></td><td>{}</td><td>{}</td></tr>",
            escape(path),
            escape(&text(&get["summary"])),
            escape(&names.join(", ")),
        );
    }
    out.push_str("</tbody></table>");
    out
}

fn collections(doc: &Value, nav: &dyn Fn(&str) -> String) -> String {
    let list = doc["collections"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return "<p>No collections: the dataset's shapes give no class with a geometry.</p>"
            .to_owned();
    }
    let mut out = String::from(
        "<table><thead><tr><th>Collection</th><th>Id</th><th>Description</th><th></th></tr></thead><tbody>",
    );
    for c in &list {
        let own = link_of(c, "self").unwrap_or_default();
        let items = link_of(c, "items").unwrap_or_default();
        let _ = write!(
            out,
            "<tr><td><a href=\"{}\">{}</a></td><td><code>{}</code></td><td>{}</td>\
             <td><a href=\"{}\">Features</a></td></tr>",
            escape(&nav(&own)),
            escape(&text(&c["title"])),
            escape(&text(&c["id"])),
            escape(&text(&c["description"])),
            escape(&nav(&items)),
        );
    }
    out.push_str("</tbody></table>");
    out
}

fn collection(doc: &Value, nav: &dyn Fn(&str) -> String) -> String {
    let mut out = String::new();
    if let Some(description) = doc["description"].as_str() {
        let _ = write!(out, "<p>{}</p>", escape(description));
    }
    let crs: Vec<String> = doc["crs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(text)
        .collect();
    let _ = write!(
        out,
        "<dl><dt>Id</dt><dd><code>{}</code></dd><dt>Coordinate systems</dt><dd>{}</dd>",
        escape(&text(&doc["id"])),
        escape(&crs.join(", ")),
    );
    let extent = &doc["extent"];
    if let Some(bbox) = extent["spatial"]["bbox"][0].as_array() {
        let corners: Vec<String> = bbox.iter().map(text).collect();
        let _ = write!(
            out,
            "<dt>Extent</dt><dd>{}</dd>",
            escape(&corners.join(", "))
        );
    }
    if let Some(interval) = extent["temporal"]["interval"][0].as_array() {
        let ends: Vec<String> = interval
            .iter()
            .map(|v| {
                if v.is_null() {
                    "..".to_owned()
                } else {
                    text(v)
                }
            })
            .collect();
        let _ = write!(out, "<dt>Time</dt><dd>{}</dd>", escape(&ends.join(" / ")));
    }
    out.push_str("</dl>");
    if let Some(items) = link_of(doc, "items") {
        let _ = write!(
            out,
            "<p><a href=\"{}\">Browse the features</a></p>",
            escape(&nav(&items))
        );
    }
    out
}

fn items(doc: &Value, axes: Axes, nav: &dyn Fn(&str) -> String) -> String {
    let features = doc["features"].as_array().cloned().unwrap_or_default();
    let mut out = format!("<p>{} features on this page.</p>", features.len());
    let shapes: Vec<(String, String, &Value)> = features
        .iter()
        .map(|f| (feature_href(f, nav), text(&f["id"]), &f["geometry"]))
        .collect();
    out.push_str(&sketch(&shapes, axes));
    // Columns: every property key, in the order they first appear.
    let mut keys: Vec<String> = Vec::new();
    for f in &features {
        for key in f["properties"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, _)| k)
        {
            if !keys.contains(key) {
                keys.push(key.clone());
            }
        }
    }
    if !features.is_empty() {
        out.push_str("<div class=\"wide\"><table><thead><tr><th>Feature</th>");
        for key in &keys {
            let _ = write!(out, "<th>{}</th>", escape(key));
        }
        out.push_str("</tr></thead><tbody>");
        for (f, (href, id, _)) in features.iter().zip(&shapes) {
            let _ = write!(
                out,
                "<tr><td><a href=\"{}\">{}</a></td>",
                escape(href),
                escape(short(id))
            );
            for key in &keys {
                let _ = write!(out, "<td>{}</td>", value(&f["properties"][key]));
            }
            out.push_str("</tr>");
        }
        out.push_str("</tbody></table></div>");
    }
    out.push_str("<p class=\"pager\">");
    if let Some(prev) = link_of(doc, "prev") {
        let _ = write!(
            out,
            "<a href=\"{}\">Previous page</a> ",
            escape(&nav(&prev))
        );
    }
    if let Some(next) = link_of(doc, "next") {
        let _ = write!(out, "<a href=\"{}\">Next page</a>", escape(&nav(&next)));
    }
    out.push_str("</p>");
    out
}

fn feature(doc: &Value, axes: Axes, nav: &dyn Fn(&str) -> String) -> String {
    let own = feature_href(doc, nav);
    let mut out = sketch(&[(own, text(&doc["id"]), &doc["geometry"])], axes);
    out.push_str("<table><tbody>");
    for (key, v) in doc["properties"].as_object().into_iter().flatten() {
        let _ = write!(
            out,
            "<tr><th>{}</th><td>{}</td></tr>",
            escape(key),
            value(v)
        );
    }
    out.push_str("</tbody></table>");
    out
}

fn feature_href(feature: &Value, nav: &dyn Fn(&str) -> String) -> String {
    link_of(feature, "self")
        .map(|h| nav(&h))
        .unwrap_or_default()
}

/// `(rel, href, title)` of each link of `doc`.
fn links(doc: &Value) -> Vec<(String, String, String)> {
    doc["links"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|l| (text(&l["rel"]), text(&l["href"]), text(&l["title"])))
        .collect()
}

fn link_of(doc: &Value, rel: &str) -> Option<String> {
    links(doc)
        .into_iter()
        .find(|(r, _, _)| r == rel)
        .map(|(_, href, _)| href)
}

/// The text of a JSON string, or the JSON of anything else; empty for null.
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// A property value as HTML: IRIs as links, lists one per line.
fn value(v: &Value) -> String {
    match v {
        Value::String(s) if s.starts_with("http://") || s.starts_with("https://") => {
            format!("<a href=\"{}\">{}</a>", escape(s), escape(short(s)))
        }
        Value::Array(list) => list.iter().map(value).collect::<Vec<_>>().join("<br>"),
        other => escape(&text(other)),
    }
}

/// The end of an IRI, after its last `/` or `#`, for a narrow column.
fn short(iri: &str) -> &str {
    let tail = iri.rsplit(['/', '#']).next().unwrap_or(iri);
    if tail.is_empty() { iri } else { tail }
}

/// How the coordinates of a page map to the sketch's x (east) and y (north).
#[derive(Clone, Copy)]
struct Axes {
    /// Latitude first (EPSG:4326).
    swap: bool,
    /// Degrees, not metres: x is shrunk by the cosine of the latitude, so shapes keep
    /// their proportions away from the equator.
    geographic: bool,
}

/// An SVG sketch of the geometries: each feature a link, fitted to their bounds.
fn sketch(shapes: &[(String, String, &Value)], axes: Axes) -> String {
    let mut parts: Vec<Vec<Part>> = Vec::with_capacity(shapes.len());
    let (mut min, mut max) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
    for (_, _, geometry) in shapes {
        let mut own = Vec::new();
        collect(geometry, &mut own);
        for part in &mut own {
            for p in part.points_mut() {
                if axes.swap {
                    p.swap(0, 1);
                }
                for axis in 0..2 {
                    min[axis] = min[axis].min(p[axis]);
                    max[axis] = max[axis].max(p[axis]);
                }
            }
        }
        parts.push(own);
    }
    if !min[0].is_finite() {
        return String::new();
    }
    let shrink = if axes.geographic {
        ((min[1] + max[1]) / 2.0)
            .to_radians()
            .cos()
            .clamp(0.05, 1.0)
    } else {
        1.0
    };
    min[0] *= shrink;
    max[0] *= shrink;
    let span = (max[0] - min[0]).max(max[1] - min[1]);
    let scale = if span > 0.0 { SKETCH / span } else { 1.0 };
    let width = ((max[0] - min[0]) * scale).max(1.0);
    let height = ((max[1] - min[1]) * scale).max(1.0);
    let at = |p: &[f64; 2]| ((p[0] * shrink - min[0]) * scale, (max[1] - p[1]) * scale);
    let pad = 8.0;
    let mut out = format!(
        "<svg class=\"sketch\" viewBox=\"{:.1} {:.1} {:.1} {:.1}\" role=\"img\" aria-label=\"The geometries\">",
        -pad,
        -pad,
        width + 2.0 * pad,
        height + 2.0 * pad
    );
    for ((href, id, _), own) in shapes.iter().zip(&parts) {
        let _ = write!(
            out,
            "<a href=\"{}\"><title>{}</title>",
            escape(href),
            escape(id)
        );
        for part in own {
            match part {
                Part::Point(p) => {
                    let (x, y) = at(p);
                    let _ = write!(out, "<circle cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"5\"/>");
                }
                Part::Line(points) => {
                    let _ = write!(out, "<polyline points=\"{}\"/>", path(points, &at));
                }
                Part::Polygon(rings) => {
                    let d: Vec<String> = rings
                        .iter()
                        .map(|ring| format!("M{}Z", path(ring, &at)))
                        .collect();
                    let _ = write!(out, "<path d=\"{}\"/>", d.join(""));
                }
            }
        }
        out.push_str("</a>");
    }
    out.push_str("</svg>");
    out
}

fn path(points: &[[f64; 2]], at: &dyn Fn(&[f64; 2]) -> (f64, f64)) -> String {
    points
        .iter()
        .map(|p| {
            let (x, y) = at(p);
            format!("{x:.1},{y:.1}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A drawable piece of a GeoJSON geometry.
enum Part {
    Point([f64; 2]),
    Line(Vec<[f64; 2]>),
    Polygon(Vec<Vec<[f64; 2]>>),
}

impl Part {
    fn points_mut(&mut self) -> Box<dyn Iterator<Item = &mut [f64; 2]> + '_> {
        match self {
            Part::Point(p) => Box::new(std::iter::once(p)),
            Part::Line(points) => Box::new(points.iter_mut()),
            Part::Polygon(rings) => Box::new(rings.iter_mut().flatten()),
        }
    }
}

fn collect(geometry: &Value, out: &mut Vec<Part>) {
    let coordinates = &geometry["coordinates"];
    match geometry["type"].as_str() {
        Some("Point") => out.extend(point(coordinates).map(Part::Point)),
        Some("MultiPoint") => out.extend(line(coordinates).into_iter().map(Part::Point)),
        Some("LineString") => out.push(Part::Line(line(coordinates))),
        Some("MultiLineString") => {
            out.extend(list(coordinates).iter().map(|l| Part::Line(line(l))));
        }
        Some("Polygon") => out.push(Part::Polygon(list(coordinates).iter().map(line).collect())),
        Some("MultiPolygon") => out.extend(
            list(coordinates)
                .iter()
                .map(|p| Part::Polygon(list(p).iter().map(line).collect())),
        ),
        Some("GeometryCollection") => {
            for g in list(&geometry["geometries"]) {
                collect(g, out);
            }
        }
        _ => {}
    }
}

fn list(v: &Value) -> &[Value] {
    v.as_array().map_or(&[], Vec::as_slice)
}

fn point(v: &Value) -> Option<[f64; 2]> {
    Some([v.get(0)?.as_f64()?, v.get(1)?.as_f64()?])
}

fn line(v: &Value) -> Vec<[f64; 2]> {
    list(v).iter().filter_map(point).collect()
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

const STYLE: &str = ":root{--bg:#fff;--fg:#1c1f24;--muted:#5d6570;--line:#d9dde3;--accent:#1a5fb4;--fill:rgba(26,95,180,.18)}\
@media (prefers-color-scheme:dark){:root{--bg:#16181c;--fg:#e6e8eb;--muted:#9aa3ad;--line:#2e333a;--accent:#78aeed;--fill:rgba(120,174,237,.22)}}\
body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,sans-serif}\
nav,main,footer{max-width:72rem;margin:0 auto;padding:.75rem 1rem}\
nav,footer{color:var(--muted);font-size:13px}\
a{color:var(--accent)}h1{font-size:1.5rem;margin:.25rem 0 1rem}\
table{border-collapse:collapse;width:100%;margin:1rem 0}th,td{text-align:left;vertical-align:top;padding:.35rem .6rem;border-bottom:1px solid var(--line)}\
th{font-weight:600}.wide{overflow-x:auto}dl{display:grid;grid-template-columns:max-content 1fr;gap:.25rem 1rem}dd{margin:0}\
code{font-size:13px}.sketch{display:block;width:100%;max-height:28rem;border:1px solid var(--line);border-radius:6px}\
.sketch circle,.sketch path,.sketch polyline{fill:var(--fill);stroke:var(--accent);stroke-width:1.5;vector-effect:non-scaling-stroke}\
.sketch path{fill-rule:evenodd}.sketch polyline{fill:none}.sketch a:hover *{stroke-width:3}\
.pager a{margin-right:1rem}";
