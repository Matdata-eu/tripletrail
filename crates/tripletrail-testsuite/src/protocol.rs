//! The SPARQL 1.1 Protocol and Graph Store Protocol suites: their HTTP requests, replayed
//! against a `tripletrail serve` process.

use crate::manifest::{Files, HttpResponse, Test};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tripletrail::oxigraph::io::{RdfFormat, RdfParser};
use tripletrail::oxigraph::model::dataset::CanonicalizationAlgorithm;
use tripletrail::oxigraph::model::vocab::rdf;
use tripletrail::oxigraph::model::{Graph, NamedNode, NamedOrBlankNodeRef, TermRef};
use tripletrail::oxigraph::sparql::results::{
    QueryResultsFormat, QueryResultsParser, SliceQueryResultsParserOutput,
};

/// The dataset the tests run on.
const DATASET: &str = "ds";

/// A `tripletrail serve` process with one in-memory dataset, stopped when dropped.
pub struct Server {
    child: Child,
    port: u16,
    timeout: Duration,
}

impl Server {
    pub fn start(binary: &Path, timeout: Duration) -> Result<Self, String> {
        Self::start_with(binary, timeout, &[DATASET], false)
    }

    /// A server with the datasets `datasets`; `federated`: its SERVICE calls may go to
    /// itself, as a federated test's mock endpoints call each other.
    pub fn start_with(
        binary: &Path,
        timeout: Duration,
        datasets: &[&str],
        federated: bool,
    ) -> Result<Self, String> {
        // A free port: bind to port 0, read it, let it go for the server.
        let port = TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .map_err(|e| e.to_string())?
            .port();
        let mut command = Command::new(binary);
        command
            .args(["serve", "--bind", &format!("127.0.0.1:{port}")])
            .args(["--dataset", &datasets.join(","), "--no-ui"]);
        if federated {
            command.args(["--service-allow", &format!("http://127.0.0.1:{port}/")]);
        }
        let child = command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("{}: {e}", binary.display()))?;
        let server = Self {
            child,
            port,
            timeout,
        };
        let start = Instant::now();
        loop {
            if let Ok(response) = server.send("GET", "/$/ready", &[], None)
                && response.status == 200
            {
                return Ok(server);
            }
            if start.elapsed() > Duration::from_secs(30) {
                return Err("the server did not get ready in 30 s".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// One request on its own connection.
    fn send(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Option<&str>,
    ) -> Result<Response, String> {
        let mut stream =
            TcpStream::connect(("127.0.0.1", self.port)).map_err(|e| format!("connect: {e}"))?;
        stream
            .set_read_timeout(Some(self.timeout))
            .map_err(|e| e.to_string())?;
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n",
            self.port
        );
        for (name, value) in headers {
            if name.eq_ignore_ascii_case("host") || name.eq_ignore_ascii_case("content-length") {
                continue;
            }
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        let body = body.unwrap_or_default();
        if !body.is_empty() || matches!(method, "POST" | "PUT") {
            request.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        request.push_str("\r\n");
        request.push_str(body);
        stream
            .write_all(request.as_bytes())
            .map_err(|e| format!("send: {e}"))?;
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .map_err(|e| format!("read: {e}"))?;
        Response::parse(&raw)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Puts `data` (the text of the file `file`) into the default graph of `dataset`.
    pub fn put_default(&self, dataset: &str, file: &str, data: &str) -> Result<(), String> {
        let response = self.send(
            "PUT",
            &format!("/{dataset}/data?default"),
            &[("Content-Type".to_owned(), content_type(file).to_owned())],
            Some(data),
        )?;
        if !(200..300).contains(&response.status) {
            return Err(format!("loading {file}: status {}", response.status));
        }
        Ok(())
    }

    /// Empties the dataset and loads the test's graphs.
    fn reset(&self, files: &Files, test: &Test) -> Result<(), String> {
        let update = [(
            "Content-Type".to_owned(),
            "application/sparql-update".to_owned(),
        )];
        let response = self.send(
            "POST",
            &format!("/{DATASET}/update"),
            &update,
            Some("DROP ALL"),
        )?;
        if !(200..300).contains(&response.status) {
            return Err(format!("emptying the dataset: status {}", response.status));
        }
        for graph in &test.graph_data {
            let data = files.read_to_string(&graph.file)?;
            let response = self.send(
                "PUT",
                &format!("/{DATASET}/data?graph={}", encode(&graph.name)),
                &[(
                    "Content-Type".to_owned(),
                    content_type(&graph.file).to_owned(),
                )],
                Some(&data),
            )?;
            if !(200..300).contains(&response.status) {
                return Err(format!(
                    "loading {}: status {}",
                    graph.name, response.status
                ));
            }
        }
        Ok(())
    }
}

fn content_type(file: &str) -> &'static str {
    match file.rsplit('.').next() {
        Some("ttl") => "text/turtle",
        Some("rdf") => "application/rdf+xml",
        _ => "application/n-triples",
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Why a protocol test is not run, if it isn't.
pub fn untested(test: &Test) -> Option<String> {
    if test
        .requires
        .iter()
        .any(|r| r == "DirectGraphIdentification")
    {
        return Some(
            "direct graph identification (graph IRIs as server paths): not served, as in Fuseki"
                .into(),
        );
    }
    if test.requests.is_empty() && test.kind != crate::manifest::Kind::ServiceDescription {
        return Some("no HTTP requests in the test".into());
    }
    None
}

/// Runs the requests of a test in order and checks every response.
pub fn run(server: &Server, files: &Files, test: &Test) -> Result<(), String> {
    server.reset(files, test)?;
    let mut variables: Vec<(String, String)> = Vec::new();
    for (i, request) in test.requests.iter().enumerate() {
        let fill = |text: &str, encoded: bool| -> String {
            let mut text = text.to_owned();
            for (name, value) in &variables {
                let value = if encoded {
                    encode(value)
                } else {
                    value.clone()
                };
                text = text.replace(name.as_str(), &value);
            }
            text
        };
        let path = server_path(&fill(&request.path, true))?;
        let body = request.body.as_deref().map(|b| fill(b, false));
        let response = server.send(&request.method, &path, &request.headers, body.as_deref())?;
        let step = format!("request {} ({} {})", i + 1, request.method, request.path);
        check(&request.response, &response, &step, &fill)?;
        if let Some(variable) = &request.response.location {
            let location = response
                .header("location")
                .ok_or_else(|| format!("{step}: no Location header"))?;
            variables.push((variable.clone(), location.to_owned()));
        }
    }
    Ok(())
}

const SD: &str = "http://www.w3.org/ns/sparql-service-description#";

/// The SPARQL 1.1 Service Description tests (`mf:ServiceDescriptionTest`). The suite gives
/// them in prose only; as the working group's harness checks them, a `GET` of the endpoint
/// without a query (`returns-rdf`) answers RDF, which (`has-endpoint-triple`) has an
/// `sd:endpoint` triple naming the endpoint's URL, and (`conforms-to-schema`) uses the
/// vocabulary as the schema says: each `sd:Service` has an IRI `sd:endpoint` and a
/// `sd:supportedLanguage` of the three, IRI-valued properties have IRIs, the dataset
/// properties have resources, and each named graph has an IRI `sd:name`.
pub fn service_description(server: &Server, test: &Test) -> Result<(), String> {
    let path = format!("/{DATASET}/sparql");
    let url = format!("http://127.0.0.1:{}{path}", server.port);
    let response = server.send(
        "GET",
        &path,
        &[(
            "Accept".to_owned(),
            "text/turtle, application/rdf+xml;q=0.9".to_owned(),
        )],
        None,
    )?;
    if response.status != 200 {
        return Err(format!("GET {path}: status {}", response.status));
    }
    let media = response
        .header("content-type")
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    let format = RdfFormat::from_media_type(&media)
        .ok_or_else(|| format!("GET {path}: not RDF ({media:?})"))?;
    let mut graph = Graph::new();
    for quad in RdfParser::from_format(format)
        .with_base_iri(&url)
        .map_err(|e| e.to_string())?
        .for_slice(response.body.as_bytes())
    {
        let quad = quad.map_err(|e| format!("GET {path}: the {media} does not parse: {e}"))?;
        graph.insert(&quad.into());
    }
    let local = test.id.rsplit('#').next().unwrap_or_default();
    let sd = |local: &str| NamedNode::new_unchecked(format!("{SD}{local}"));
    match local {
        "returns-rdf" => {
            if graph.is_empty() {
                return Err(format!("GET {path}: no triples"));
            }
            Ok(())
        }
        "has-endpoint-triple" => {
            let endpoint = NamedNode::new_unchecked(&url);
            if graph
                .subjects_for_predicate_object(&sd("endpoint"), &endpoint)
                .next()
                .is_none()
            {
                return Err(format!("no sd:endpoint <{url}> in the description"));
            }
            Ok(())
        }
        "conforms-to-schema" => schema_errors(&graph, &sd).map_or(Ok(()), Err),
        other => Err(format!("unknown service description test {other}")),
    }
}

/// What in a service description breaks the vocabulary's schema, if anything.
fn schema_errors(graph: &Graph, sd: &dyn Fn(&str) -> NamedNode) -> Option<String> {
    let mut errors = Vec::new();
    let services: Vec<_> = graph
        .subjects_for_predicate_object(rdf::TYPE, &sd("Service"))
        .map(|s| s.into_owned())
        .collect();
    if services.is_empty() {
        errors.push("no sd:Service".to_owned());
    }
    let languages = ["SPARQL10Query", "SPARQL11Query", "SPARQL11Update"].map(sd);
    for service in &services {
        let endpoints: Vec<_> = graph
            .objects_for_subject_predicate(service, &sd("endpoint"))
            .collect();
        if endpoints.is_empty() {
            errors.push(format!("{service} has no sd:endpoint"));
        }
        let mut supported = graph
            .objects_for_subject_predicate(service, &sd("supportedLanguage"))
            .peekable();
        if supported.peek().is_none() {
            errors.push(format!("{service} has no sd:supportedLanguage"));
        }
        for language in supported {
            if !matches!(language, TermRef::NamedNode(n) if languages.iter().any(|l| l.as_ref() == n))
            {
                errors.push(format!(
                    "sd:supportedLanguage {language} is none of the three"
                ));
            }
        }
    }
    // Properties whose values are IRIs.
    for property in [
        "endpoint",
        "feature",
        "resultFormat",
        "inputFormat",
        "defaultEntailmentRegime",
        "entailmentRegime",
        "defaultSupportedEntailmentProfile",
        "supportedEntailmentProfile",
        "extensionFunction",
        "extensionAggregate",
        "languageExtension",
        "propertyFeature",
        "name",
    ] {
        for t in graph.triples_for_predicate(&sd(property)) {
            if !matches!(t.object, TermRef::NamedNode(_)) {
                errors.push(format!("sd:{property} {} is not an IRI", t.object));
            }
        }
    }
    // Properties whose values are resources (an IRI or a blank node).
    for property in [
        "defaultDataset",
        "availableGraphs",
        "namedGraph",
        "defaultGraph",
        "graph",
    ] {
        for t in graph.triples_for_predicate(&sd(property)) {
            if matches!(t.object, TermRef::Literal(_)) {
                errors.push(format!("sd:{property} {} is a literal", t.object));
            }
        }
    }
    for t in graph.triples_for_predicate(&sd("namedGraph")) {
        let named = match t.object {
            TermRef::NamedNode(n) => NamedOrBlankNodeRef::from(n),
            TermRef::BlankNode(b) => NamedOrBlankNodeRef::from(b),
            _ => continue,
        };
        if graph
            .object_for_subject_predicate(named, &sd("name"))
            .is_none()
        {
            errors.push(format!("the named graph {named} has no sd:name"));
        }
    }
    errors.sort();
    errors.dedup();
    (!errors.is_empty()).then(|| errors.join("; "))
}

/// The suites' paths on the server: `/sparql/…` is the dataset's SPARQL endpoint and
/// `/gsp…` its Graph Store endpoint.
fn server_path(path: &str) -> Result<String, String> {
    let (path, query) = match path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path, None),
    };
    let service = match path.trim_end_matches('/') {
        "/sparql" => "sparql",
        "/gsp" => "data",
        other => return Err(format!("no endpoint for the path {other}")),
    };
    Ok(match query {
        Some(query) => format!("/{DATASET}/{service}?{query}"),
        None => format!("/{DATASET}/{service}"),
    })
}

fn check(
    expected: &HttpResponse,
    actual: &Response,
    step: &str,
    fill: &dyn Fn(&str, bool) -> String,
) -> Result<(), String> {
    if !expected.statuses.is_empty()
        && !expected
            .statuses
            .iter()
            .any(|s| status_matches(s, actual.status))
    {
        return Err(format!(
            "{step}: status {}, expected {}{}",
            actual.status,
            expected.statuses.join(" or "),
            excerpt(&actual.body)
        ));
    }
    let content_type = actual
        .header("content-type")
        .map(media_type)
        .unwrap_or_default();
    for (name, value) in &expected.headers {
        let got = actual.header(name).unwrap_or_default();
        let matches = if name.eq_ignore_ascii_case("content-type") {
            media_type(got) == media_type(value)
        } else {
            got == value
        };
        if !matches {
            return Err(format!("{step}: {name} is `{got}`, expected `{value}`"));
        }
    }
    if let Some(format) = &expected.format {
        let ok = match format.as_str() {
            "boolean" | "tabular" => results_format(&content_type).is_some(),
            "RDF" => RdfFormat::from_media_type(&content_type).is_some(),
            _ => true,
        };
        if !ok {
            return Err(format!(
                "{step}: content type `{content_type}`, expected {format} results"
            ));
        }
    }
    if let Some(expected_boolean) = expected.boolean {
        let format = results_format(&content_type)
            .ok_or_else(|| format!("{step}: `{content_type}` is not a results format"))?;
        let parsed = QueryResultsParser::from_format(format)
            .for_slice(actual.body.as_bytes())
            .map_err(|e| format!("{step}: {e}"))?;
        match parsed {
            SliceQueryResultsParserOutput::Boolean(b) if b == expected_boolean => {}
            _ => {
                return Err(format!(
                    "{step}: expected the boolean {expected_boolean}{}",
                    excerpt(&actual.body)
                ));
            }
        }
    }
    if let Some(body) = &expected.body {
        let base = "http://www.example/gsp";
        let expected_type = expected
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| media_type(v))
            .unwrap_or_else(|| "text/turtle".into());
        let mut want = graph(&fill(body, false), &expected_type, base)
            .map_err(|e| format!("{step}: the expected body: {e}"))?;
        let mut got = graph(&actual.body, &content_type, base)
            .map_err(|e| format!("{step}: the body: {e}"))?;
        want.canonicalize(CanonicalizationAlgorithm::Unstable);
        got.canonicalize(CanonicalizationAlgorithm::Unstable);
        if want != got {
            return Err(format!(
                "{step}: expected {} triples, got {}{}",
                want.len(),
                got.len(),
                excerpt(&actual.body)
            ));
        }
    }
    Ok(())
}

fn graph(text: &str, media_type: &str, base: &str) -> Result<Graph, String> {
    let format = RdfFormat::from_media_type(media_type)
        .ok_or_else(|| format!("`{media_type}` is not an RDF format"))?;
    let parser = RdfParser::from_format(format)
        .with_base_iri(base)
        .map_err(|e| e.to_string())?;
    let mut graph = Graph::new();
    for quad in parser.for_slice(text.as_bytes()) {
        let quad = quad.map_err(|e| e.to_string())?;
        graph.insert(tripletrail::oxigraph::model::TripleRef::new(
            &quad.subject,
            &quad.predicate,
            &quad.object,
        ));
    }
    Ok(graph)
}

fn results_format(media_type: &str) -> Option<QueryResultsFormat> {
    QueryResultsFormat::from_media_type(media_type)
}

fn status_matches(name: &str, status: u16) -> bool {
    match name {
        "OK" => status == 200,
        "Created" => status == 201,
        "Accepted" => status == 202,
        "NoContent" => status == 204,
        "NotFound" => status == 404,
        "StatusCode2xx" => (200..300).contains(&status),
        "StatusCode3xx" => (300..400).contains(&status),
        "StatusCode4xx" => (400..500).contains(&status),
        "StatusCode5xx" => (500..600).contains(&status),
        _ => false,
    }
}

/// The media type without parameters, lower case.
fn media_type(value: &str) -> String {
    value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

fn excerpt(body: &str) -> String {
    let body: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if body.is_empty() {
        return String::new();
    }
    let cut: String = body.chars().take(120).collect();
    format!(" (body: `{cut}`)")
}

/// Percent-encodes everything but unreserved characters, for a query parameter.
pub fn encode(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Response {
    fn parse(raw: &[u8]) -> Result<Self, String> {
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or("no end of headers in the response")?;
        let head = String::from_utf8_lossy(&raw[..split]);
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .ok_or("no status line")?;
        let headers: Vec<(String, String)> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(n, v)| (n.trim().to_owned(), v.trim().to_owned()))
            .collect();
        let mut body = raw[split + 4..].to_vec();
        let chunked = headers.iter().any(|(n, v)| {
            n.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked")
        });
        if chunked {
            body = dechunk(&body)?;
        }
        Ok(Self {
            status,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        })
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn dechunk(mut raw: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let end = raw
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("bad chunk")?;
        let size = String::from_utf8_lossy(&raw[..end]);
        let size = usize::from_str_radix(size.split(';').next().unwrap_or_default().trim(), 16)
            .map_err(|e| format!("bad chunk size: {e}"))?;
        raw = &raw[end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if raw.len() < size {
            return Err("truncated chunk".into());
        }
        out.extend_from_slice(&raw[..size]);
        raw = raw.get(size + 2..).unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suite_paths_map_to_the_dataset() {
        assert_eq!(
            server_path("/sparql/?query=x").unwrap(),
            "/ds/sparql?query=x"
        );
        assert_eq!(server_path("/sparql/").unwrap(), "/ds/sparql");
        assert_eq!(server_path("/gsp?default").unwrap(), "/ds/data?default");
        assert!(server_path("/gsp/person/1.ttl").is_err());
    }

    #[test]
    fn responses_parse_with_and_without_chunks() {
        let r =
            Response::parse(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nhello").unwrap();
        assert_eq!((r.status, r.body.as_str()), (200, "hello"));
        assert_eq!(r.header("content-type"), Some("text/plain"));
        let r = Response::parse(
            b"HTTP/1.1 201 Created\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!((r.status, r.body.as_str()), (201, "abcde"));
    }

    #[test]
    fn statuses() {
        assert!(status_matches("StatusCode4xx", 415));
        assert!(status_matches("NoContent", 204));
        assert!(!status_matches("OK", 204));
    }

    #[test]
    fn encoding() {
        assert_eq!(encode("http://a/b c"), "http%3A%2F%2Fa%2Fb%20c");
    }
}
