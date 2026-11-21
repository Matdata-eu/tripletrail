//! Leak tests for named graph grants (#208): a caller who may read only `urn:release` asks
//! two servers the same things. One holds that graph and others (the default graph, a
//! work-in-progress graph with properties, classes, labels and geometries of its own, some on
//! the release graph's subjects); the other holds that graph only. Every answer must be the
//! same: whatever differs would tell the caller something about the graphs it may not read.
//!
//! The release graph is loaded first in both, so its terms get the same ids and the answers
//! the same order. Ports, blank node labels and timestamps are written out of the answers
//! before they are compared.
//!
//! In the enterprise build, the same requests check row rules (enterprise #9): the caller
//! reads every graph, row rules hide some resources and properties, and the second server
//! holds only what they let it see, its terms loaded first in both.

use serde_json::Value;
use std::collections::HashMap;
use std::fmt::Write as _;
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail_server::auth::{sha256, to_hex};
use tripletrail_server::{Options, Server};

const PREFIXES: &str = "@prefix geo: <http://www.opengis.net/ont/geosparql#> .
@prefix ex: <urn:x:> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
";

/// Features `tag0`… with a point, a line or a polygon each, spread over 0..10 degrees.
fn features(tag: &str, n: usize, extra: &str) -> String {
    let mut out = String::new();
    for i in 0..n {
        let x = (i * 37 % 97) as f64 / 10.0;
        let y = (i * 53 % 89) as f64 / 10.0;
        let wkt = match i % 3 {
            0 => format!("POINT({x} {y})"),
            1 => format!("LINESTRING({x} {y}, {} {})", x + 1.0, y + 0.5),
            _ => format!(
                "POLYGON(({x} {y}, {} {y}, {} {}, {x} {}, {x} {y}))",
                x + 2.0,
                x + 2.0,
                y + 2.0,
                y + 2.0
            ),
        };
        writeln!(
            out,
            "ex:{tag}{i} a ex:Feature ; ex:kind \"{tag}\" ; ex:n {i} ;{extra} \
             geo:hasGeometry ex:{tag}g{i} . ex:{tag}g{i} geo:asWKT \"{wkt}\"^^geo:wktLiteral ."
        )
        .unwrap();
    }
    out
}

/// The release graph, in both servers.
fn release() -> String {
    format!(
        "{PREFIXES}<urn:release> {{\n{}ex:rel0 geo:sfWithin ex:rel2 . ex:rel1 ex:label \"one\"@en .\n\
         ex:rel0 rdfs:label \"Release label\" . ex:rel2 rdfs:label \"Second label kind\" .\n\
         ex:rel3 ex:part [ ex:name \"blank\" ] .\n\
         ex:rel0 ex:emb \"[1, 0.2]\"^^<http://w3id.org/tripletrail/fn#vector> .\n\
         ex:rel1 ex:emb \"[0, 1]\"^^<http://w3id.org/tripletrail/fn#vector> .\n}}\n",
        features("rel", 24, "")
    )
}

/// What only the first server holds: the default graph and `urn:wip`.
fn hidden() -> String {
    format!(
        "{PREFIXES}{}<urn:wip> {{\n{}\
         ex:rel1 ex:secret \"about-rel1\" ; ex:n \"7.5\"^^xsd:decimal ; ex:label \"geheim\"@nl .\n\
         ex:relg0 geo:asWKT \"POINT(9.9 9.9)\"^^geo:wktLiteral .\n\
         ex:rel2 ex:owner ex:p1 . ex:p1 a ex:Person ; ex:name \"Secret Person\" .\n\
         ex:Feature rdfs:subClassOf ex:SecretBase ; rdfs:label \"Hidden label\" .\n\
         ex:kind rdfs:label \"Hidden kind\" .\n\
         ex:sec1 a ex:Secret ; geo:hasGeometry ex:sec1g . ex:sec1g geo:asWKT \"POINT(1 1)\"^^geo:wktLiteral .\n\
         ex:rel3 ex:part [ ex:name \"hidden blank\" ] .\n\
         ex:sec1 ex:emb \"[1, 0]\"^^<http://w3id.org/tripletrail/fn#vector> .\n\
         ex:rel1 ex:emb \"[1, 0.1]\"^^<http://w3id.org/tripletrail/fn#vector> .\n}}\n",
        features("dft", 10, ""),
        features("wip", 30, " ex:hiddenprop \"hp\" ; ex:owner ex:p1 ;"),
    )
}

struct Pair {
    a: String,
    b: String,
    /// The servers' folder, removed with the pair.
    _dir: tripletrail_testdir::TestDir,
}

/// The graph grant of the tests below: the contractor reads `urn:release` only.
const GRAPH_GRANT: &str = "@prefix sg: <https://matdata.eu/ns/sparql-guard#> .
     @prefix ex: <https://example.org/policies/> .
     ex:C a sg:Policy ; sg:group \"contractor\" ; sg:readGraph <urn:release> .";

/// Two servers whose datasets `ds` have the contractor's `policy`: the first loads the TriG
/// documents of `a` in order, the second those of `b`.
fn start(name: &str, policy: &str, union_default_graph: bool, a: &[String], b: &[String]) -> Pair {
    let dir = tripletrail_testdir::tempdir(&format!("leaks-{name}"));
    std::fs::write(dir.join("ds.ttl"), policy).unwrap();
    std::fs::write(
        dir.join("guard.json"),
        format!(
            r#"{{"auth": {{"apiKeys": [{{"sha256": "{}", "agent": "https://example.org/people/c", "groups": ["contractor"]}}]}},
                "datasets": {{"ds": {{"policies": ["ds.ttl"]}}}}}}"#,
            to_hex(&sha256(b"c"))
        ),
    )
    .unwrap();
    let serve = |data: &[String], sub: &str| {
        let engine = Engine::new_tripletrail();
        for trig in data {
            engine.load(RdfFormat::TriG, trig.as_bytes()).unwrap();
        }
        engine.set_union_default_graph(union_default_graph);
        let mut options = Options {
            threads: 2,
            auth_config: Some(dir.join("guard.json")),
            functions: functions(),
            ..Options::default()
        };
        options.workspaces.dir = Some(dir.join(sub));
        let server = Server::bind("127.0.0.1:0", [("ds".to_owned(), engine)], options).unwrap();
        let base = format!("http://{}", server.address());
        server.spawn();
        base
    };
    let (a, b) = (serve(a, "ws-a"), serve(b, "ws-b"));
    Pair { a, b, _dir: dir }
}

/// The status, the headers that tell something about the data, and the body of an answer.
type Answer = (u16, String, String);

/// The headers of `r` that depend on the data: the content type, `Link`, `X-Truncated`,
/// whether `X-Cache` is there, and the `ETag` without the time the dataset was attached
/// (its second part, which differs between any two servers).
fn head(r: &ureq::Response) -> String {
    let etag = r.header("ETag").map(|t| {
        let parts: Vec<&str> = t.split('-').collect();
        parts
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 1)
            .map(|(_, p)| *p)
            .collect::<Vec<_>>()
            .join("-")
    });
    format!(
        "type={:?} link={:?} truncated={:?} cache={} etag={etag:?}",
        r.header("Content-Type"),
        r.header("Link"),
        r.header("X-Truncated"),
        r.header("X-Cache").is_some(),
    )
}

/// The body of `r`: its text, or its bytes in hex when it is no text (a vector tile).
fn body_of(r: ureq::Response) -> String {
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut r.into_reader(), &mut bytes).unwrap();
    match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => e
            .into_bytes()
            .iter()
            .fold(String::from("bytes:"), |mut out, b| {
                let _ = write!(out, "{b:02x}");
                out
            }),
    }
}

/// An answer to a request with the contractor's key.
fn ask(base: &str, method: &str, path: &str, body: Option<(&str, &str)>) -> Answer {
    let request = ureq::request(method, &format!("{base}{path}"))
        .set("X-API-Key", "c")
        .set(
            "Accept",
            "application/sparql-results+json, application/n-quads;q=0.9, application/json;q=0.8, */*;q=0.1",
        );
    let answer = match body {
        Some((media, text)) => request.set("Content-Type", media).send_string(text),
        None => request.call(),
    };
    match answer {
        Ok(r) | Err(ureq::Error::Status(_, r)) => (r.status(), head(&r), body_of(r)),
        Err(e) => panic!("{path}: {e}"),
    }
}

/// Keys whose values differ between any two servers (times, ids of this run).
const VOLATILE: [&str; 3] = ["timeStamp", "attached", "ms"];

/// The answer without what differs between any two servers: the port, blank node labels
/// (numbered in the order they come) and timestamps.
fn normal(base: &str, body: &str) -> String {
    // The server's own address only: the port's digits may also be part of a number or of a
    // binary tile, where replacing them would make equal answers differ.
    let host = base.trim_start_matches("http://");
    let port = host.rsplit(':').next().unwrap();
    let body = body.replace(host, &host.replace(port, "PORT"));
    let mut labels: HashMap<String, String> = HashMap::new();
    let mut relabel = |label: &str| -> String {
        let n = labels.len();
        labels
            .entry(label.to_owned())
            .or_insert_with(|| format!("b{n}"))
            .clone()
    };
    if let Ok(mut json) = serde_json::from_str::<Value>(&body) {
        fn walk(v: &mut Value, relabel: &mut dyn FnMut(&str) -> String) {
            match v {
                Value::Object(o) => {
                    for k in VOLATILE {
                        o.remove(k);
                    }
                    if o.get("type").and_then(Value::as_str) == Some("bnode")
                        && let Some(Value::String(s)) = o.get_mut("value")
                    {
                        *s = relabel(s);
                    }
                    for v in o.values_mut() {
                        walk(v, relabel);
                    }
                }
                Value::Array(a) => a.iter_mut().for_each(|v| walk(v, relabel)),
                Value::String(s) if s.contains("_:") => *s = bnodes(s, relabel),
                _ => {}
            }
        }
        walk(&mut json, &mut relabel);
        return json.to_string();
    }
    bnodes(&body, &mut relabel)
}

/// `text` with each `_:label` replaced.
fn bnodes(text: &str, relabel: &mut dyn FnMut(&str) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("_:") {
        out.push_str(&rest[..at + 2]);
        let after = &rest[at + 2..];
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(after.len());
        out.push_str(&relabel(&after[..end]));
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

const Q: &str = "PREFIX geo: <http://www.opengis.net/ont/geosparql#> \
    PREFIX geof: <http://www.opengis.net/def/function/geosparql/> \
    PREFIX uom: <http://www.opengis.net/def/uom/OGC/1.0/> PREFIX ex: <urn:x:> \
    PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#> \
    PREFIX tt: <http://w3id.org/tripletrail/fn#> PREFIX text: <http://jena.apache.org/text#> ";

const QUERIES: &[&str] = &[
    "SELECT * { ?s ?p ?o }",
    "SELECT * { GRAPH ?g { ?s ?p ?o } }",
    "SELECT ?g (COUNT(*) AS ?n) { GRAPH ?g { ?s ?p ?o } } GROUP BY ?g",
    "SELECT (COUNT(*) AS ?n) { ?s ?p ?o }",
    "SELECT (COUNT(DISTINCT ?s) AS ?n) { ?s a ex:Feature }",
    "SELECT ?s { ?s ex:secret ?x }",
    "ASK { ex:p1 ?p ?o }",
    "ASK { ?s ?p ex:p1 }",
    "ASK { GRAPH <urn:wip> { ?s ?p ?o } }",
    "SELECT * FROM <urn:wip> { ?s ?p ?o }",
    "SELECT * FROM NAMED <urn:wip> { GRAPH ?g { ?s ?p ?o } }",
    "SELECT * FROM <urn:release> FROM <urn:wip> { ?s ex:label ?l }",
    "SELECT ?x { ex:rel1 ex:label ?x }",
    "SELECT ?x { ex:rel1 ex:n ?x }",
    "SELECT ?c { ex:Feature rdfs:subClassOf* ?c }",
    "SELECT ?x { ?x ex:owner* ex:p1 }",
    "SELECT ?x { ex:p1 ex:owner* ?x }",
    "SELECT ?x { ?x (ex:part/ex:name)? ?y FILTER(?x = ex:rel3) }",
    "SELECT ?n { ex:rel3 ex:part/ex:name ?n }",
    "SELECT ?s { ?s ?p \"hp\" }",
    "SELECT DISTINCT ?p { ?s ?p ?o } ORDER BY ?p",
    "SELECT DISTINCT ?c { ?s a ?c } ORDER BY ?c",
    "SELECT ?s ?o { ?s ex:n ?o } ORDER BY DESC(?o) LIMIT 3",
    "SELECT (MAX(?n) AS ?m) (SUM(?n) AS ?t) { ?s ex:n ?n }",
    "SELECT ?s { ?s ex:kind ?k FILTER NOT EXISTS { ?s ex:secret ?x } } ORDER BY ?s",
    "SELECT ?s { ?s ex:kind ?k MINUS { ?s ex:owner ?o } } ORDER BY ?s",
    "SELECT ?s ?o { ?s ex:kind ?k OPTIONAL { ?s ex:owner ?o } } ORDER BY ?s",
    "CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o FILTER(?s = ex:rel1) }",
    "DESCRIBE ex:rel1",
    "DESCRIBE ex:p1",
    "SELECT ?f { ?f geo:hasGeometry/geo:asWKT ?w \
     FILTER(geof:sfIntersects(?w, \"POLYGON((2 2, 7 2, 7 7, 2 7, 2 2))\"^^geo:wktLiteral)) } ORDER BY ?f",
    "SELECT ?g { ?g geo:asWKT ?w \
     FILTER(geof:sfIntersects(?w, \"POINT(9.9 9.9)\"^^geo:wktLiteral)) }",
    "ASK { ?g geo:asWKT ?w \
     FILTER(geof:sfIntersects(?w, \"POLYGON((0.5 0.5, 1.5 0.5, 1.5 1.5, 0.5 1.5, 0.5 0.5))\"^^geo:wktLiteral)) }",
    "SELECT ?a ?b { ?a geo:asWKT ?x . ?b geo:asWKT ?y \
     FILTER(?a != ?b && geof:sfIntersects(?x, ?y)) } ORDER BY ?a ?b",
    "SELECT ?g ?w { ?g geo:asWKT ?w } \
     ORDER BY geof:distance(?w, \"POINT(1 1)\"^^geo:wktLiteral, uom:metre) LIMIT 3",
    "SELECT (COUNT(*) AS ?n) { ?a geo:asWKT ?x . ?b geo:asWKT ?y FILTER(geof:sfIntersects(?x, ?y)) }",
    "SELECT ?f ?l { ?f geo:hasMetricLength ?l } ORDER BY ?f",
    "SELECT ?a ?b { ?a geo:sfWithin ?b } ORDER BY ?a ?b",
    "SELECT * { SERVICE <urn:x:elsewhere> { ?s ?p ?o } }",
    // The st: and window: services (#203): their sub-selects read the graphs the caller reads.
    "SELECT ?g ?part { SERVICE <http://w3id.org/tripletrail/st> { \
     [] <http://w3id.org/tripletrail/st#function> <http://w3id.org/tripletrail/st#ST_DumpPoints> ; \
        <http://w3id.org/tripletrail/st#geom> ?part . \
     { SELECT ?g ?geom { ?g geo:asWKT ?geom } } } } ORDER BY ?g STR(?part)",
    "SELECT ?s ?n ?r { SERVICE <http://w3id.org/tripletrail/window> { \
     [] <http://w3id.org/tripletrail/window#orderBy> ?n ; <http://w3id.org/tripletrail/window#rowNumber> ?r . \
     { SELECT ?s ?n { ?s ex:n ?n } } } } ORDER BY ?r",
    // The spatial rewrite in a service's sub-select (#857).
    "SELECT ?a ?b ?r { SERVICE <http://w3id.org/tripletrail/window> { \
     [] <http://w3id.org/tripletrail/window#orderBy> ?k ; <http://w3id.org/tripletrail/window#rowNumber> ?r . \
     { SELECT ?a ?b ?k { ?a geo:sfWithin ?b BIND(CONCAT(STR(?a), \" \", STR(?b)) AS ?k) } } } } ORDER BY ?r",
    "SELECT * { SERVICE <http://w3id.org/tripletrail/path> { \
     [] <http://w3id.org/tripletrail/path#source> ex:rel1 ; <http://w3id.org/tripletrail/path#end> ?e ; \
        <http://w3id.org/tripletrail/path#direction> \"both\" ; <http://w3id.org/tripletrail/path#maxLength> 3 } } ORDER BY ?e",
    // pgRouting (#203): the network is built from the graphs the caller reads.
    "SELECT ?v ?c { SERVICE <http://w3id.org/tripletrail/pgr> { \
     [] <http://w3id.org/tripletrail/pgr#function> <http://w3id.org/tripletrail/pgr#pgr_connectedComponents> ; \
        <http://w3id.org/tripletrail/pgr#node> ?v ; <http://w3id.org/tripletrail/pgr#component> ?c . \
     { SELECT ?id ?source ?target ?cost { ?source ?p ?target FILTER(isIRI(?target)) \
       BIND(IRI(CONCAT(STR(?source), \"-\", STR(?target))) AS ?id) BIND(1 AS ?cost) } } } } \
     ORDER BY ?v",
    // Full-text search (#203): the matches and their scores of the graphs the caller reads.
    "SELECT ?s ?sc ?l { ?s tt:search (\"label kind\" ?sc ?l) } ORDER BY ?s ?l",
    "SELECT ?s ?sc { (?s ?sc) text:query (rdfs:label \"label\" 5) } ORDER BY ?s",
    // Nearest vectors (#236): only those of the graphs the caller reads.
    "SELECT ?s ?sc ?v { ?s tt:nearest (ex:emb \"[1, 0]\" 2 ?sc ?v) } ORDER BY DESC(?sc) ?s",
    "SELECT ?s { ex:rel0 ex:emb ?q . ?s tt:nearest (ex:emb ?q 1) }",
    "SELECT ?s { ?s ex:emb ?v } ORDER BY DESC(tt:cosine(?v, \"[1, 0]\")) ?s LIMIT 2",
    // User-defined functions (#235): bodies that read the data are refused, one-expression
    // bodies are inlined and work.
    "SELECT (ex:get(ex:rel1, ex:secret) AS ?v) {}",
    "SELECT (ex:dget(ex:dft0, ex:kind) AS ?v) {}",
    "SELECT ?s { ?s ex:n ?n FILTER(ex:has(?s)) }",
    "SELECT ?s (ex:twice(?n) AS ?t) { ?s ex:n ?n } ORDER BY ?s",
    // An inlined expression that calls a function reading the data.
    "SELECT (ex:wrap(ex:rel1, ex:secret) AS ?v) {}",
];

/// User-defined functions both servers have: three read the data, one is an expression, and
/// one an expression calling one that reads the data.
fn functions() -> tripletrail::udf::Functions {
    let text = r#"@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <urn:x:> .
        ex:get a sh:SPARQLFunction ; sh:parameter [ sh:path ex:s ; sh:order 1 ] , [ sh:path ex:p ; sh:order 2 ] ;
            sh:select "SELECT ?o WHERE { GRAPH ?g { $s $p ?o } }" .
        ex:dget a sh:SPARQLFunction ; sh:parameter [ sh:path ex:s ; sh:order 1 ] , [ sh:path ex:p ; sh:order 2 ] ;
            sh:select "SELECT ?o WHERE { $s $p ?o }" .
        ex:has a sh:SPARQLFunction ; sh:parameter [ sh:path ex:s ] ;
            sh:ask "ASK { GRAPH ?g { $s <urn:x:secret> ?o } }" .
        ex:twice a sh:SPARQLFunction ; sh:parameter [ sh:path ex:x ] ;
            sh:select "SELECT ($x * 2 AS ?r) WHERE {}" .
        ex:wrap a sh:SPARQLFunction ; sh:parameter [ sh:path ex:s ; sh:order 1 ] , [ sh:path ex:p ; sh:order 2 ] ;
            sh:select "SELECT (<urn:x:get>($s, $p) AS ?r) WHERE {}" ."#;
    tripletrail::udf::Functions::from_triples(
        tripletrail::oxigraph::io::RdfParser::from_format(RdfFormat::Turtle)
            .for_slice(text.as_bytes())
            .map(|q| tripletrail::oxigraph::model::Triple::from(q.unwrap())),
    )
    .unwrap()
}

fn paths() -> Vec<String> {
    let encode = |s: &str| form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    let mut paths = Vec::new();
    for q in QUERIES {
        let q = encode(&format!("{Q}{q}"));
        paths.push(format!("/ds/query?query={q}"));
        paths.push(format!("/ds/explain?query={q}"));
        paths.push(format!("/repositories/ds?query={q}"));
    }
    let feature = "/ds/ogc/collections/Feature";
    paths.extend(
        [
            "/ds/data",
            "/ds/data?default",
            "/ds/data?graph=urn:release",
            "/ds/data?graph=urn:wip",
            "/ds/get",
            "/ds/describe?iri=urn:x:rel1",
            "/ds/search/similar?property=urn:x:emb&vector=%5B1,0%5D&k=3",
            "/ds/search/similar?property=urn:x:emb&iri=urn:x:rel0&k=2",
            "/ds/search/similar?property=urn:x:emb&iri=urn:x:sec1",
            "/ds/describe?iri=urn:x:rel3",
            "/ds/describe?iri=urn:x:p1",
            "/ds/describe?iri=urn:x:Feature",
            "/ds/void",
            "/ds/sparql",
            "/ds/query",
            "/.well-known/void",
            "/.well-known/dcat",
            "/$/datasets",
            "/$/datasets/ds",
            "/$/stats/ds",
            "/$/datasets/ds/profile",
            "/repositories",
            "/repositories/ds/statements",
            "/repositories/ds/statements?subj=%3Curn:x:rel1%3E",
            "/repositories/ds/statements?context=%3Curn:wip%3E",
            "/repositories/ds/statements?context=null",
            "/repositories/ds/contexts",
            "/repositories/ds/size",
            "/repositories/ds/size?context=%3Curn:wip%3E",
            "/repositories/ds/namespaces",
            "/ds/ogc",
            "/ds/ogc/api",
            "/ds/ogc/collections",
            "/ds/ogc/collections/Secret",
            "/ds/ogc/collections/Person",
            "/ds/tiles/all/0/0/0.mvt",
            "/ds/tiles/Feature/3/4/3.mvt",
            "/ds/tiles/Secret/0/0/0.mvt",
            "/ds/tiles/all/8/130/126.mvt",
            "/ds/tiles/all/5/16/15.mvt",
            "/ds/tiles/search?q=label",
            "/ds/tiles/search?q=hid",
            "/ds/search/labels?q=label",
            "/ds/search/labels?q=hid",
            "/ds/search/labels?q=secret",
            "/ds/search/labels?q=geheim",
            "/ds/search/labels?q=label&class=Feature",
            "/ds/ogc/collections?f=html",
            // Map facets (#139, #92): counts per key, of one collection or several, and
            // filtered by map area, graph and text.
            "/ds/tiles/facets/Feature",
            "/ds/tiles/facets/Secret",
            "/ds/tiles/facets/all",
            "/ds/tiles/facets?collection=Feature&collection=Secret",
            "/ds/tiles/facets/Feature?bbox=2,2,7,7",
            "/ds/tiles/facets/Feature?graph=urn:wip",
            "/ds/tiles/facets/Feature?q=label",
            "/ds/tiles/facets/Feature?kind=wip",
            "/ds/tiles/all/0/0/0.mvt?graph=urn:wip",
            "/ds/tiles/all/0/0/0.mvt?q=hid",
            // The JSON-LD REST API (#143): datasets, records, filters and one record.
            "/ds/api/v1",
            "/ds/api/openapi.json",
            "/ds/api/v1/Feature?limit=100",
            "/ds/api/v1/Feature?kind=wip",
            "/ds/api/v1/Feature?q=label",
            "/ds/api/v1/Feature/urn%3Ax%3Arel1",
            "/ds/api/v1/Feature/urn%3Ax%3Awip0",
            "/ds/api/v1/Feature/urn%3Ax%3Adft0",
            "/ds/api/v1/Secret",
            "/ds/api/v1/Person",
        ]
        .map(str::to_owned),
    );
    // The search builder (#142): classes, results and their SPARQL.
    paths.push("/ds/search/classes".to_owned());
    for m in [
        r#"{"class":"Feature","show":["kind","n","label","sfWithin","part"]}"#,
        r#"{"class":"Feature","where":[{"key":"kind","op":"is","value":"wip"}]}"#,
        r#"{"class":"Feature","where":[{"key":"kind","op":"contains","value":"rel"}],"show":["n","label"]}"#,
        r#"{"class":"Feature","where":[{"key":"label","op":"contains","value":"label"}]}"#,
        r#"{"class":"Feature","where":[{"key":"n","op":"ge","value":"0"}]}"#,
        r#"{"class":"Feature","where":[{"key":"sfWithin","op":"exists"}],"show":["sfWithin"]}"#,
        r#"{"class":"Feature","where":[{"key":"part","op":"missing"}]}"#,
        r#"{"class":"Feature","where":[{"any":[{"key":"kind","op":"is","value":"wip"},{"key":"n","op":"ge","value":"1"}]}],"show":["kind"]}"#,
        r#"{"class":"Secret"}"#,
        r#"{"class":"Person"}"#,
    ] {
        paths.push(format!("/ds/search/results?limit=1000&m={}", encode(m)));
        paths.push(format!("/ds/search/sparql?m={}", encode(m)));
        for key in ["kind", "n", "label", "sfWithin", "part"] {
            paths.push(format!(
                "/ds/search/values?limit=100&key={key}&m={}",
                encode(m)
            ));
        }
    }
    // GraphQL (#144): the schema, lists, filters, search and records by id.
    for q in [
        "{ __schema { types { name fields { name } } } }",
        "{ features(first: 100) { nodes { id kind n label hasGeometry sfWithin { id label } part { name } } pageInfo { hasNextPage endCursor } } }",
        "{ features(first: 5, after: \"2\") { edges { cursor node { id } } } }",
        "{ features(filter: {kind: \"wip\"}) { nodes { id } } }",
        "{ features(filter: {kind: \"rel\"}, first: 100) { nodes { id n } } }",
        "{ features(bbox: [2, 2, 7, 7], first: 100) { nodes { id } } }",
        "{ features(q: \"label\") { nodes { id label } } }",
        "{ feature(id: \"urn:x:rel1\") { id n label label_2 } }",
        "{ feature(id: \"urn:x:rel3\") { id part { name } } }",
        "{ feature(id: \"urn:x:wip0\") { id } }",
        "{ feature(id: \"urn:x:dft0\") { id } }",
        "{ secrets { nodes { id } } }",
        "{ persons { nodes { id } } }",
    ] {
        paths.push(format!("/ds/graphql?query={}", encode(q)));
    }
    for rest in [
        "",
        "/queryables",
        "/items?limit=100",
        "/items?limit=5&offset=3",
        "/items?bbox=2,2,7,7&limit=100",
        "/items?kind=wip",
        "/items?kind=rel&limit=100",
        "/items?hiddenprop=hp",
        "/items?owner=urn%3Ax%3Ap1",
        "/items?n=7.5",
        "/items?filter=kind%3D'rel'&limit=100",
        "/items?filter=hiddenprop%3D'hp'",
        "/items?filter=S_INTERSECTS(geometry,POINT(9.9%209.9))",
        "/items/urn%3Ax%3Arel0",
        "/items/urn%3Ax%3Arel1",
        "/items/urn%3Ax%3Awip0",
        "/items/urn%3Ax%3Adft0",
        "/items?f=html&limit=100",
        "/items?geometries=all&limit=100",
        "/items/urn%3Ax%3Arel0?geometries=all",
        "?f=html",
    ] {
        paths.push(format!("{feature}{rest}"));
    }
    paths
}

/// Asks both servers of `pair` everything, twice, and fails with the answers that differ.
fn compare(pair: &Pair) {
    let mut differ = Vec::new();
    let mut check = |what: String, a: Answer, b: Answer| {
        let (ha, hb) = (normal(&pair.a, &a.1), normal(&pair.b, &b.1));
        let (na, nb) = (normal(&pair.a, &a.2), normal(&pair.b, &b.2));
        if a.0 != b.0 || ha != hb || na != nb {
            // 400 characters from a little before the first difference.
            let from = na
                .chars()
                .zip(nb.chars())
                .take_while(|(x, y)| x == y)
                .count()
                .saturating_sub(100);
            let cut = |s: &str| s.chars().skip(from).take(400).collect::<String>();
            differ.push(format!(
                "{what}\n  with what is hidden: {} {ha} {}\n  without:             {} {hb} {}",
                a.0,
                cut(&na),
                b.0,
                cut(&nb)
            ));
        }
    };
    // Twice: the second time from the caches.
    let mut answered = 0;
    for _ in 0..2 {
        for path in paths() {
            let a = ask(&pair.a, "GET", &path, None);
            let b = ask(&pair.b, "GET", &path, None);
            answered += usize::from(a.0 == 200);
            check(path, a, b);
        }
        // A query sent in the body, as the protocol's POST forms do.
        for q in QUERIES.iter().take(12) {
            let query = format!("{Q}{q}");
            for path in ["/ds/query", "/repositories/ds"] {
                let send = |base: &str| {
                    ask(
                        base,
                        "POST",
                        path,
                        Some(("application/sparql-query", &query)),
                    )
                };
                check(format!("POST {path} {q}"), send(&pair.a), send(&pair.b));
            }
        }
        // `search/similar` with its parameters as a form, as the Search page sends them: a
        // reader of some graphs is answered as by the GET, and a hidden resource finds nothing.
        for form in [
            "property=urn:x:emb&vector=%5B1,0%5D&k=3",
            "property=urn:x:emb&iri=urn:x:rel0&k=2",
            "property=urn:x:emb&iri=urn:x:sec1",
        ] {
            let send = |base: &str| {
                ask(
                    base,
                    "POST",
                    "/ds/search/similar",
                    Some(("application/x-www-form-urlencoded", form)),
                )
            };
            let (a, b) = (send(&pair.a), send(&pair.b));
            assert_eq!(a.0, 200, "POST search/similar {form}: {}", a.2);
            if form.ends_with("sec1") {
                assert!(a.2.contains(r#""results":[]"#), "{}", a.2);
            }
            check(format!("POST search/similar {form}"), a, b);
        }
    }
    // Most requests are answered (not refused), so the comparison says something.
    assert!(answered > paths().len() * 3 / 2, "{answered} answered");
    // Workspace samples, of a class in both and of one only in the hidden graphs.
    let workspace = |base: &str| {
        let (_, _, made) = ask(
            base,
            "POST",
            "/$/workspaces",
            Some(("application/json", "{}")),
        );
        serde_json::from_str::<Value>(&made).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let (wa, wb) = (workspace(&pair.a), workspace(&pair.b));
    for classes in [
        r#"["urn:x:Feature"]"#,
        r#"["urn:x:Secret", "urn:x:Person"]"#,
    ] {
        for hops in [1, 3] {
            let body = format!(
                r#"{{"dataset": "ds", "classes": {classes}, "hops": {hops}, "perNode": 3}}"#
            );
            let sample = |base: &str, ws: &str| {
                let reply = ask(
                    base,
                    "POST",
                    &format!("/$/workspaces/{ws}/sample"),
                    Some(("application/json", &body)),
                );
                let file = ask(
                    base,
                    "GET",
                    &format!("/$/workspaces/{ws}/files/data/sample-ds.nt"),
                    None,
                );
                // The workspace's id differs between the servers; nothing else should.
                let text = reply.2.replace(ws, "WS");
                (
                    reply.0,
                    file.1,
                    format!("{}\n{}", normal(base, &text), file.2),
                )
            };
            check(
                format!("sample {classes} hops={hops}"),
                sample(&pair.a, &wa),
                sample(&pair.b, &wb),
            );
        }
    }
    let overlay = |base: &str, ws: &str| {
        ask(
            base,
            "POST",
            &format!("/$/workspaces/{ws}/overlay"),
            Some(("application/json", r#"{"dataset": "ds"}"#)),
        )
    };
    check(
        "overlay".to_owned(),
        overlay(&pair.a, &wa),
        overlay(&pair.b, &wb),
    );
    assert!(
        differ.is_empty(),
        "{} answers tell what is hidden apart:\n{}",
        differ.len(),
        differ.join("\n")
    );
}

/// The servers of the graph grant tests: with and without the hidden graphs.
fn graph_grant(union_default_graph: bool, name: &str) -> Pair {
    start(
        name,
        GRAPH_GRANT,
        union_default_graph,
        &[release(), hidden()],
        &[release()],
    )
}

#[test]
fn a_graph_grant_hides_the_other_graphs() {
    compare(&graph_grant(false, "named"));
}

#[test]
fn a_graph_grant_hides_the_other_graphs_under_a_union_default_graph() {
    compare(&graph_grant(true, "union"));
}

/// Row rules (#202, enterprise #9): the contractor reads every graph, but `sg:visibleIf` hides
/// the draft features, the people without clearance and the unapproved secrets (with the
/// references to them), and `sg:hideProperties` the secret properties of features and notes
/// everywhere. The second server has the same policy and holds only what it lets the
/// contractor see, so it hides nothing.
#[cfg(feature = "enterprise")]
const ROW_RULES: &str = "@prefix sg: <https://matdata.eu/ns/sparql-guard#> .
     @prefix sh: <http://www.w3.org/ns/shacl#> .
     @prefix pol: <https://example.org/policies/> .
     @prefix ex: <urn:x:> .
     pol:C a sg:Policy ; sg:group \"contractor\" ; sg:readAllGraphs true ;
       sg:visibleIf ex:Released, ex:Cleared, ex:Approved ; sg:hideProperties ex:NoSecrets, ex:NoNotes .
     ex:Released a sh:NodeShape ; sh:targetClass ex:Feature ;
       sh:property [ sh:path ex:status ; sh:maxCount 0 ] .
     ex:Cleared a sh:NodeShape ; sh:targetClass ex:Person ;
       sh:property [ sh:path ex:clearance ; sh:hasValue \"public\" ] .
     ex:Approved a sh:NodeShape ; sh:targetClass ex:Secret ;
       sh:property [ sh:path ex:approved ; sh:minCount 1 ] .
     ex:NoSecrets a sh:NodeShape ; sh:targetClass ex:Feature ;
       sh:property [ sh:path ex:secret ] , [ sh:path ex:hiddenprop ] .
     ex:NoNotes a sh:NodeShape ; sh:property [ sh:path ex:note ] .";

/// What the row rules let the contractor see beyond the release graph's own data: the
/// geometries of the draft features and of the secret, and a cleared person. Loaded before
/// [`row_hidden`], so that its terms get the same ids in both servers.
#[cfg(feature = "enterprise")]
fn row_visible() -> String {
    let geometries: String = features("wip", 30, "")
        .lines()
        .map(|line| format!("ex:{}\n", line.split_once(" . ex:").unwrap().1))
        .collect();
    format!(
        "{PREFIXES}<urn:release> {{\n{geometries}\
         ex:sec1g geo:asWKT \"POINT(1 1)\"^^geo:wktLiteral .\n\
         ex:p2 a ex:Person ; ex:clearance \"public\" ; ex:name \"Public Person\" .\n\
         ex:rel3 ex:owner ex:p2 .\n}}\n"
    )
}

/// What the row rules hide, in the release graph: draft features (with
/// labels, vectors and owners), secret and hidden properties of released features, notes, a
/// person without clearance (named and blank) and an unapproved secret, with references to
/// them from released features; and `urn:wip`, which holds only draft features.
#[cfg(feature = "enterprise")]
fn row_hidden() -> String {
    // The features without their geometries, which the rules do not hide.
    let drafts = |tag: &str, n: usize, extra: &str| -> String {
        features(tag, n, extra)
            .lines()
            .map(|line| format!("{} .\n", line.split_once(" . ex:").unwrap().0))
            .collect()
    };
    format!(
        "{PREFIXES}<urn:release> {{\n{}\
         ex:wip0 rdfs:label \"Hidden label kind\" .\n\
         ex:wip1 ex:emb \"[1, 0.1]\"^^<http://w3id.org/tripletrail/fn#vector> .\n\
         ex:rel1 ex:secret \"about-rel1\" ; ex:hiddenprop \"hp\" ; ex:note \"geheim\"@nl .\n\
         ex:p2 ex:note \"a note\" .\n\
         ex:rel2 ex:owner ex:p1 . ex:p1 a ex:Person ; ex:name \"Secret Person\" ; rdfs:label \"Hidden label\" .\n\
         ex:rel3 ex:owner [ a ex:Person ; ex:name \"hidden blank\" ] .\n\
         ex:sec1 a ex:Secret ; geo:hasGeometry ex:sec1g ;\
           ex:emb \"[1, 0]\"^^<http://w3id.org/tripletrail/fn#vector> .\n\
         ex:rel0 ex:part ex:sec1 .\n}}\n\
         <urn:wip> {{\n{}}}\n",
        drafts(
            "wip",
            30,
            " ex:status \"draft\" ; ex:hiddenprop \"hp\" ; ex:owner ex:p1 ;"
        ),
        drafts("drf", 5, " ex:status \"draft\" ;"),
    )
}

#[cfg(feature = "enterprise")]
#[test]
fn row_rules_hide_rows_from_every_reader() {
    compare(&start(
        "rows",
        ROW_RULES,
        false,
        &[release(), row_visible(), row_hidden()],
        &[release(), row_visible()],
    ));
}
