//! Linked Data publishing (#141): a dataset's own IRIs dereference to their description.
//!
//! With `--base-iri DATASET=PREFIX`, a request whose URL (rebuilt from `Host` or
//! `X-Forwarded-Host` / `X-Forwarded-Proto`) starts with `PREFIX` and is not one of the
//! server's own paths is answered with the description of the resource with that IRI. Any
//! IRI can also be described at `/{ds}/describe?iri=`.
//!
//! - The format comes from a suffix (`.ttl`, `.nt`, `.rdf`, `.jsonld`, `.html`), else
//!   `?format=`, else `Accept`; Turtle by default, and a small HTML page for browsers.
//! - The description is the resource's concise bounded description over all graphs: its
//!   triples, and those of the blank nodes they reach. For an IRI without a `#`, the
//!   resources `IRI#...` are described too, so a hash-namespace vocabulary is one document
//!   (W3C "Best Practice Recipes for Publishing RDF Vocabularies", recipe 3).
//! - `Vary: Accept`, `Link` headers to the other formats, an `ETag` from the store's write
//!   generation (from the description, for a caller who reads only some graphs), and
//!   `304 Not Modified` on a matching `If-None-Match`.
//! - With `--see-other`, a request for the resource itself is answered `303 See Other` with
//!   the document in the negotiated format (recipe 5).

use crate::{HttpError, Params, Reply, accepted, request_header};
use std::collections::{HashSet, VecDeque};
use std::time::SystemTime;
use tiny_http::Request;
use tripletrail::oxigraph::io::{JsonLdProfileSet, RdfFormat};
use tripletrail::oxigraph::model::{NamedNode, NamedOrBlankNode, Quad, Term, Triple};
use tripletrail::oxigraph::sparql::QueryResults;
use tripletrail::store::Dataset;
use tripletrail::{Engine, ReadableGraphs};

/// Most triples one description holds: past it the description is cut short (and says so
/// in an `X-Truncated` header), so a resource with millions of triples can't exhaust memory.
const MAX_TRIPLES: usize = 100_000;

/// What a document is served as.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Doc {
    Rdf(RdfFormat),
    Html,
    /// The view model of the describe page, as JSON (`format=view`, #140).
    View,
}

const JSON_LD: RdfFormat = RdfFormat::JsonLd {
    profile: JsonLdProfileSet::empty(),
};

/// The document formats, with their suffix and `format=` name, in the order of the `Link`
/// header.
const DOCS: [(&str, Doc); 5] = [
    ("ttl", Doc::Rdf(RdfFormat::Turtle)),
    ("jsonld", Doc::Rdf(JSON_LD)),
    ("nt", Doc::Rdf(RdfFormat::NTriples)),
    ("rdf", Doc::Rdf(RdfFormat::RdfXml)),
    ("html", Doc::Html),
];

impl Doc {
    fn media_type(self) -> &'static str {
        match self {
            Doc::Rdf(f) => f.media_type(),
            Doc::Html => "text/html; charset=utf-8",
            Doc::View => "application/json",
        }
    }

    fn suffix(self) -> &'static str {
        if self == Doc::View {
            return "view";
        }
        DOCS.iter()
            .find(|(_, d)| *d == self)
            .map_or("ttl", |(s, _)| s)
    }

    fn from_name(name: &str) -> Option<Self> {
        let name = name.to_ascii_lowercase();
        let by_name = match name.as_str() {
            "turtle" => Some(Doc::Rdf(RdfFormat::Turtle)),
            "ntriples" | "n-triples" => Some(Doc::Rdf(RdfFormat::NTriples)),
            "xml" | "rdfxml" | "rdf/xml" => Some(Doc::Rdf(RdfFormat::RdfXml)),
            "json-ld" | "json" => Some(Doc::Rdf(JSON_LD)),
            "view" => Some(Doc::View),
            _ => None,
        };
        by_name
            .or_else(|| DOCS.iter().find(|(s, _)| *s == name).map(|(_, d)| *d))
            .or_else(|| media_type_doc(&name))
    }
}

fn media_type_doc(media: &str) -> Option<Doc> {
    if matches!(media, "text/html" | "application/xhtml+xml") {
        return Some(Doc::Html);
    }
    let format = RdfFormat::from_media_type(media)?;
    // Only triple formats: a description has no graphs.
    (!format.supports_datasets() || matches!(format, RdfFormat::JsonLd { .. }))
        .then(|| Doc::Rdf(normalize(format)))
}

/// One JSON-LD variant, whatever profile `Accept` named.
fn normalize(format: RdfFormat) -> RdfFormat {
    match format {
        RdfFormat::JsonLd { .. } => JSON_LD,
        f => f,
    }
}

/// The document format: `format`/`output` parameter, else `Accept`, else Turtle.
pub(crate) fn negotiate(request: &Request, params: &Params) -> Result<Doc, HttpError> {
    if let Some(f) = params.get("format").or_else(|| params.get("output")) {
        return Doc::from_name(f).ok_or_else(|| HttpError(406, format!("Unsupported format {f}")));
    }
    let types = accepted(request);
    if types.is_empty() {
        return Ok(Doc::Rdf(RdfFormat::Turtle));
    }
    for t in &types {
        if let Some(d) = media_type_doc(t) {
            return Ok(d);
        }
        if matches!(t.as_str(), "*/*" | "text/*" | "application/*") {
            return Ok(Doc::Rdf(RdfFormat::Turtle));
        }
    }
    Err(HttpError(
        406,
        format!("No supported format in {}", types.join(", ")),
    ))
}

/// `iri` without a document suffix (`.ttl`, ...), and the format that suffix names.
pub(crate) fn split_suffix(iri: &str) -> Option<(&str, Doc)> {
    let (stem, suffix) = iri.rsplit_once('.')?;
    if stem.ends_with('/') || suffix.contains(['/', '#']) {
        return None;
    }
    DOCS.iter()
        .find(|(s, _)| *s == suffix)
        .map(|(_, d)| (stem, *d))
}

/// The concise bounded description of `iri` over all graphs (the `readable` ones only, when
/// given: #203), with the resources `iri#...` when `iri` has no fragment. The flag says
/// whether it was cut at [`MAX_TRIPLES`].
///
/// With `rows` (a view without what row rules hide, #202) it reads that view only.
pub(crate) fn describe(
    engine: &Engine,
    iri: &NamedNode,
    readable: Option<&ReadableGraphs>,
    rows: Option<&tripletrail::store::Snapshot>,
) -> Result<(Vec<Triple>, bool), HttpError> {
    let store = engine.store();
    let mut subjects: Vec<NamedOrBlankNode> = vec![iri.clone().into()];
    if !iri.as_str().contains('#') {
        match rows {
            Some(view) => subjects.extend(
                view.subjects_with_prefix(&format!("{}#", iri.as_str()), MAX_TRIPLES)
                    .into_iter()
                    .map(NamedOrBlankNode::from),
            ),
            None => subjects.extend(hash_resources(engine, iri.as_str())?),
        }
    }
    let mut seen: HashSet<NamedOrBlankNode> = subjects.iter().cloned().collect();
    let mut queue: VecDeque<NamedOrBlankNode> = subjects.into();
    let mut triples = Vec::new();
    let mut unique = HashSet::new();
    while let Some(subject) = queue.pop_front() {
        let quads: Box<dyn Iterator<Item = Result<Quad, HttpError>>> = match rows {
            Some(view) => Box::new(
                view.quads_for_pattern(Some(subject.as_ref()), None, None, None)
                    .map(Ok),
            ),
            None => Box::new(
                quads_of(store, &subject).map(|q| q.map_err(|e| HttpError(500, e.to_string()))),
            ),
        };
        for quad in quads {
            let quad = quad?;
            if !readable.is_none_or(|r| r.contains(quad.graph_name.as_ref())) {
                continue;
            }
            if let Term::BlankNode(b) = &quad.object {
                let node: NamedOrBlankNode = b.clone().into();
                if seen.insert(node.clone()) {
                    queue.push_back(node);
                }
            }
            let triple = Triple::from(quad);
            if unique.insert(triple.clone()) {
                triples.push(triple);
                if triples.len() >= MAX_TRIPLES {
                    return Ok((triples, true));
                }
            }
        }
    }
    Ok((triples, false))
}

fn quads_of(store: &Dataset, subject: &NamedOrBlankNode) -> tripletrail::store::QuadIter {
    store.quads_for_pattern(Some(subject.as_ref()), None, None, None)
}

/// The subjects `iri#...` in any graph: from the dictionary on the tripletrail store, else by a
/// query (a scan of the quads).
fn hash_resources(engine: &Engine, iri: &str) -> Result<Vec<NamedOrBlankNode>, HttpError> {
    if let Dataset::TripleTrail(store) = engine.store() {
        return Ok(store
            .snapshot()
            .subjects_with_prefix(&format!("{iri}#"), MAX_TRIPLES)
            .into_iter()
            .map(Into::into)
            .collect());
    }
    let prefix = format!("{iri}#").replace('\\', "\\\\").replace('"', "\\\"");
    let query = format!(
        "SELECT DISTINCT ?s WHERE {{ \
           {{ ?s ?p ?o FILTER(STRSTARTS(STR(?s), \"{prefix}\")) }} \
           UNION {{ GRAPH ?g {{ ?s ?p ?o }} FILTER(STRSTARTS(STR(?s), \"{prefix}\")) }} \
         }} LIMIT {MAX_TRIPLES}"
    );
    let QueryResults::Solutions(rows) = engine.query(&query)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for row in rows {
        let row = row.map_err(|e| HttpError(500, e.to_string()))?;
        if let Some(Term::NamedNode(n)) = row.get("s") {
            out.push(n.clone().into());
        }
    }
    Ok(out)
}

/// How the document of a resource links to its other formats.
pub(crate) enum Links {
    /// `{iri}.{suffix}`: the resource's URL under a base IRI.
    Suffix(String),
    /// `{path}?iri=...&format={suffix}`: the describe service.
    Query(String),
}

impl Links {
    /// The URL of the resource's page.
    fn page(&self) -> String {
        match self {
            Links::Suffix(iri) => iri.clone(),
            Links::Query(base) => base.clone(),
        }
    }

    fn url(&self, doc: Doc) -> String {
        match self {
            Links::Suffix(iri) => format!("{iri}.{}", doc.suffix()),
            Links::Query(base) => format!("{base}&format={}", doc.suffix()),
        }
    }
}

/// What a dereference answers about one resource.
pub(crate) struct Answer<'a> {
    pub(crate) engine: &'a Engine,
    pub(crate) iri: NamedNode,
    pub(crate) doc: Doc,
    pub(crate) links: Links,
    /// When the dataset was attached: in the `ETag`, so a restart does not reuse one.
    pub(crate) attached: SystemTime,
    /// How a link to another resource is written in the HTML page.
    pub(crate) href: &'a dyn Fn(&str) -> String,
    /// The graphs the caller may read, when not all (#201): the description keeps to them,
    /// and a resource with nothing in them is not found (404), as one with nothing at all.
    pub(crate) readable: Option<&'a ReadableGraphs>,
    /// A view without what row rules hide (#202), when they apply to the caller: the
    /// description keeps to it; the HTML page and its view model are not offered yet.
    pub(crate) rows: Option<&'a tripletrail::store::Snapshot>,
    /// The shapes the describe reader reads (#140), for the HTML page and the view model:
    /// asked for only when one of those is answered.
    pub(crate) shapes: &'a dyn Fn() -> Result<tripletrail::oxigraph::model::Graph, HttpError>,
    /// Whether the web UI is served: the HTML page's map then loads MapLibre from it.
    pub(crate) ui: bool,
    /// The raster tiles under that map and their attribution (`--map-tiles`), if any.
    pub(crate) basemap: Option<(&'a str, &'a str)>,
    /// The server's name, favicon and footer links (#262), for the HTML page.
    pub(crate) brand: &'a crate::branding::Branding,
    /// The dataset's name, for the page's links to its map and Explore page.
    pub(crate) dataset: &'a str,
}

/// The description of `answer.iri`, or `304`/`404`.
pub(crate) fn answer(request: &Request, answer: &Answer<'_>) -> Result<Reply, HttpError> {
    // A caller who reads only some graphs (#203) gets a tag of the description itself: the
    // store's write generation would tell it that other graphs changed.
    // Under row rules too (#202): what they hide changes with every write.
    let described = if answer.readable.is_some() || answer.rows.is_some() {
        Some(describe(
            answer.engine,
            &answer.iri,
            answer.readable,
            answer.rows,
        )?)
    } else {
        None
    };
    // The page also depends on the shapes, which are not in the store: no tag.
    let etag = match answer.doc {
        Doc::Rdf(_) => etag(answer, described.as_ref().map(|(t, _)| t.as_slice())),
        Doc::Html | Doc::View => None,
    };
    let mut headers = vec![("Vary", "Accept".to_owned())];
    let links: Vec<String> = DOCS
        .iter()
        .filter(|(_, d)| *d != answer.doc)
        .map(|(_, d)| {
            format!(
                "<{}>; rel=\"alternate\"; type=\"{}\"",
                answer.links.url(*d),
                d.media_type().split(';').next().unwrap_or_default()
            )
        })
        .collect();
    headers.push(("Link", links.join(", ")));
    if let Some(tag) = &etag {
        headers.push(("ETag", tag.clone()));
        if request_header(request, "If-None-Match")
            .is_some_and(|v| v.split(',').any(|t| t.trim() == tag || t.trim() == "*"))
        {
            let mut reply = Reply::empty(304);
            reply.headers = headers;
            return Ok(reply);
        }
    }
    if matches!(answer.doc, Doc::Html | Doc::View) {
        return page(request, answer, headers);
    }
    let (triples, truncated) = match described {
        Some(d) => d,
        None => describe(answer.engine, &answer.iri, answer.readable, answer.rows)?,
    };
    if triples.is_empty() {
        let mut reply = Reply::bytes(
            404,
            "text/plain; charset=utf-8",
            format!("Nothing is known about <{}>", answer.iri.as_str()).into_bytes(),
        );
        reply.headers.push(("Vary", "Accept".to_owned()));
        return Ok(reply);
    }
    if truncated {
        headers.push(("X-Truncated", MAX_TRIPLES.to_string()));
    }
    let body = match answer.doc {
        Doc::Rdf(format) => document(&triples, format)?,
        Doc::Html | Doc::View => unreachable!("answered by page"),
    };
    let mut reply = Reply::bytes(200, answer.doc.media_type(), body);
    reply.headers = headers;
    Ok(reply)
}

/// The describe page of `answer.iri` (#140), or its view model.
fn page(
    request: &Request,
    answer: &Answer<'_>,
    headers: Vec<(&'static str, String)>,
) -> Result<Reply, HttpError> {
    let shapes = (answer.shapes)()?;
    let lang = crate::first_language(request).unwrap_or_else(|| "en".to_owned());
    let Some(view) = crate::describe_view::build(
        answer.engine,
        &shapes,
        &answer.iri,
        answer.readable,
        answer.rows,
        &lang,
    )?
    else {
        let mut reply = Reply::bytes(
            404,
            "text/plain; charset=utf-8",
            format!("Nothing is known about <{}>", answer.iri.as_str()).into_bytes(),
        );
        reply.headers.push(("Vary", "Accept".to_owned()));
        return Ok(reply);
    };
    let body = match answer.doc {
        Doc::View => crate::describe_view::to_json(&view)
            .to_string()
            .into_bytes(),
        _ => {
            // The RDF tab shows the description as Turtle when it is small enough.
            let rdf = if view.own {
                let (triples, truncated) =
                    describe(answer.engine, &answer.iri, answer.readable, answer.rows)?;
                let turtle = if triples.len() <= crate::describe_view::MAX_EMBEDDED_TRIPLES {
                    Some(
                        String::from_utf8(document(&triples, RdfFormat::Turtle)?)
                            .unwrap_or_default(),
                    )
                    .filter(|t| t.len() <= crate::describe_view::MAX_EMBEDDED_BYTES)
                } else {
                    None
                };
                Some(crate::describe_view::PageRdf {
                    turtle,
                    triples: triples.len(),
                    truncated,
                })
            } else {
                None
            };
            let links = crate::describe_view::PageLinks {
                href: answer.href,
                documents: DOCS
                    .iter()
                    .filter(|(_, d)| *d != Doc::Html)
                    .map(|(s, d)| (*s, answer.links.url(*d)))
                    .collect(),
                live_map: answer.ui,
                basemap: answer.basemap,
                dataset: Some(answer.dataset),
                page: answer.links.page(),
                rdf,
                brand: answer.brand,
            };
            crate::describe_view::html(&view, &links).into_bytes()
        }
    };
    let mut reply = Reply::bytes(200, answer.doc.media_type(), body);
    reply.headers = headers;
    for (name, value) in &mut reply.headers {
        if *name == "Vary" {
            value.push_str(", Accept-Language");
        }
    }
    Ok(reply)
}

/// A description as a document: Turtle with prefixes for the namespaces it uses. Blank nodes
/// are named `b0`, `b1`... in the order they come, so the same description is the same text
/// (the store's own names differ with what else it holds).
fn document(triples: &[Triple], format: RdfFormat) -> Result<Vec<u8>, HttpError> {
    use tripletrail::oxigraph::model::BlankNode;
    let mut names: std::collections::HashMap<BlankNode, BlankNode> =
        std::collections::HashMap::new();
    let mut rename = |b: &BlankNode| {
        let n = names.len();
        names
            .entry(b.clone())
            .or_insert_with(|| BlankNode::new_unchecked(format!("b{n}")))
            .clone()
    };
    let triples: Vec<Triple> = triples
        .iter()
        .map(|t| {
            let subject = match &t.subject {
                NamedOrBlankNode::BlankNode(b) => rename(b).into(),
                s => s.clone(),
            };
            let object = match &t.object {
                Term::BlankNode(b) => rename(b).into(),
                o => o.clone(),
            };
            Triple::new(subject, t.predicate.clone(), object)
        })
        .collect();
    let triples = triples.as_slice();
    if format != RdfFormat::Turtle {
        return crate::service_description::serialize(triples, format);
    }
    let iris = triples.iter().flat_map(|t| {
        let s = match &t.subject {
            NamedOrBlankNode::NamedNode(n) => Some(n.as_str()),
            _ => None,
        };
        let o = match &t.object {
            Term::NamedNode(n) => Some(n.as_str()),
            Term::Literal(l) => Some(l.datatype().as_str()),
            _ => None,
        };
        [s, Some(t.predicate.as_str()), o].into_iter().flatten()
    });
    let mut serializer = tripletrail::oxigraph::io::RdfSerializer::from_format(format);
    for (prefix, iri) in crate::describe_view::prefixes(iris) {
        serializer = serializer
            .with_prefix(prefix, iri)
            .map_err(|e| HttpError(500, e.to_string()))?;
    }
    let mut writer = serializer.for_writer(Vec::new());
    for triple in triples {
        writer.serialize_triple(triple)?;
    }
    Ok(writer.finish()?)
}

/// A weak validator: the same data, attachment and format give the same tag. `None` for
/// a store without a write generation (Oxigraph's). With `described` (a caller who reads only
/// some graphs), the data is that description, hashed, not the store's write generation.
fn etag(answer: &Answer<'_>, described: Option<&[Triple]>) -> Option<String> {
    let Dataset::TripleTrail(store) = answer.engine.store() else {
        return None;
    };
    let version = match described {
        None => format!("{:x}", store.generation()),
        Some(triples) => {
            use std::hash::{Hash, Hasher};
            // Blank node labels are the store's own (they differ with what else it holds),
            // so they are left out of the hash.
            let blank = |t: String, is_blank: bool| if is_blank { "[]".to_owned() } else { t };
            let mut lines: Vec<String> = triples
                .iter()
                .map(|t| {
                    format!(
                        "{} {} {}",
                        blank(t.subject.to_string(), t.subject.is_blank_node()),
                        t.predicate,
                        blank(t.object.to_string(), t.object.is_blank_node())
                    )
                })
                .collect();
            lines.sort_unstable();
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            lines.hash(&mut hasher);
            format!("d{:x}", hasher.finish())
        }
    };
    let attached = answer
        .attached
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros());
    // Callers with other grants see other documents, so the tag tells them apart.
    let view = answer
        .readable
        .map_or(String::new(), |r| format!("-{:x}", r.fingerprint()));
    // And the resource: a tag of one resource does not validate another's (or a 404).
    let resource = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        answer.iri.as_str().hash(&mut hasher);
        hasher.finish()
    };
    Some(format!(
        "W/\"{version}-{attached:x}{view}-{resource:x}-{}\"",
        answer.doc.suffix()
    ))
}

/// `303 See Other` to the document of `iri` in the negotiated format.
pub(crate) fn see_other(iri: &str, doc: Doc) -> Reply {
    let mut reply = Reply::empty(303);
    reply
        .headers
        .push(("Location", format!("{iri}.{}", doc.suffix())));
    reply.headers.push(("Vary", "Accept".to_owned()));
    reply
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffixes() {
        assert_eq!(
            split_suffix("http://e/op/BE1.ttl"),
            Some(("http://e/op/BE1", Doc::Rdf(RdfFormat::Turtle)))
        );
        assert_eq!(
            split_suffix("http://e/a.jsonld"),
            Some(("http://e/a", Doc::Rdf(JSON_LD)))
        );
        assert_eq!(split_suffix("http://e/a.b/c"), None);
        assert_eq!(split_suffix("http://e/v1.2"), None);
        assert_eq!(split_suffix("http://e/.ttl"), None);
    }
}
