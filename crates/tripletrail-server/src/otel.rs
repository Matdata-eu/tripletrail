//! OpenTelemetry traces (#220): one span per request, with child spans for the steps of a
//! query or update, sent to an OTLP collector over HTTP with JSON (`--otlp-endpoint`, or the
//! standard `OTEL_EXPORTER_OTLP_ENDPOINT`).
//!
//! The spans are written by hand rather than with the `opentelemetry` crates: those export over
//! tokio (gRPC or hyper), and the server is synchronous. OTLP/HTTP with JSON is a stable part of
//! the protocol that every collector, Jaeger, Tempo and the vendors' agents accept on port 4318.
//!
//! A request's trace context comes from its W3C `traceparent` header when it has one (so the
//! server's spans join the caller's trace), otherwise a new trace starts. Sampling follows the
//! parent's flag, and new traces are kept at `--otlp-sample` (1.0 by default). Without an
//! endpoint nothing here runs: [`Tracer`] is `None` and a request pays one `Option` check.

use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Spans waiting to be sent, at most: past it new ones are dropped (and counted).
const MAX_QUEUED: usize = 4096;
/// Spans per export request, at most.
const BATCH: usize = 512;
/// How long a span waits, at most, before it is sent.
const FLUSH_EVERY: Duration = Duration::from_secs(2);

/// Where and how traces are sent.
#[derive(Clone, Debug, Default)]
pub struct OtlpOptions {
    /// The collector's traces URL: `…/v1/traces`.
    pub endpoint: Option<String>,
    /// Extra headers of every export request (an API key of a hosted collector).
    pub headers: Vec<(String, String)>,
    /// `service.name` of the spans.
    pub service_name: String,
    /// The share of new traces kept, from 0 to 1.
    pub sample: f64,
}

impl OtlpOptions {
    /// The traces URL from `OTEL_EXPORTER_OTLP_ENDPOINT`'s convention: a base URL gets
    /// `/v1/traces` added, a URL that already ends in it is kept.
    pub fn traces_url(base: &str) -> String {
        let base = base.trim_end_matches('/');
        if base.ends_with("/v1/traces") {
            base.to_owned()
        } else {
            format!("{base}/v1/traces")
        }
    }

    /// `OTEL_EXPORTER_OTLP_HEADERS`: `key=value` pairs separated by commas, values
    /// percent-encoded.
    pub fn parse_headers(text: &str) -> Result<Vec<(String, String)>, String> {
        text.split(',')
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(|h| {
                let (k, v) = h
                    .split_once('=')
                    .ok_or_else(|| format!("OTLP header {h:?} is not key=value"))?;
                let v = form_urlencoded::parse(
                    format!("v={}", v.trim().replace('+', "%2B")).as_bytes(),
                )
                .next()
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default();
                Ok((k.trim().to_owned(), v))
            })
            .collect()
    }
}

/// An attribute value.
#[derive(Clone, Debug, PartialEq)]
pub enum Attr {
    Str(String),
    Int(i64),
    Bool(bool),
}

impl From<&str> for Attr {
    fn from(s: &str) -> Self {
        Attr::Str(s.to_owned())
    }
}
impl From<String> for Attr {
    fn from(s: String) -> Self {
        Attr::Str(s)
    }
}
impl From<i64> for Attr {
    fn from(n: i64) -> Self {
        Attr::Int(n)
    }
}
impl From<bool> for Attr {
    fn from(b: bool) -> Self {
        Attr::Bool(b)
    }
}

/// A finished span.
#[derive(Clone, Debug)]
pub struct Span {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub parent: Option<[u8; 8]>,
    pub name: String,
    /// The request's own span (SERVER) or a step in it (INTERNAL).
    pub server: bool,
    pub start: SystemTime,
    pub end: SystemTime,
    pub attributes: Vec<(&'static str, Attr)>,
    /// An error message: the span's status is ERROR.
    pub error: Option<String>,
}

impl Span {
    fn to_json(&self) -> Value {
        let nanos = |t: SystemTime| {
            t.duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
                .to_string()
        };
        let mut span = json!({
            "traceId": hex(&self.trace_id),
            "spanId": hex(&self.span_id),
            "name": self.name,
            "kind": if self.server { 2 } else { 1 },
            "startTimeUnixNano": nanos(self.start),
            "endTimeUnixNano": nanos(self.end),
            "attributes": attributes(&self.attributes),
        });
        if let Some(p) = &self.parent {
            span["parentSpanId"] = json!(hex(p));
        }
        if let Some(message) = &self.error {
            span["status"] = json!({"code": 2, "message": message});
        }
        span
    }
}

fn attributes(attributes: &[(&str, Attr)]) -> Value {
    Value::Array(
        attributes
            .iter()
            .map(|(k, v)| {
                let value = match v {
                    Attr::Str(s) => json!({ "stringValue": s }),
                    // OTLP/JSON writes 64-bit integers as strings.
                    Attr::Int(n) => json!({ "intValue": n.to_string() }),
                    Attr::Bool(b) => json!({ "boolValue": b }),
                };
                json!({ "key": k, "value": value })
            })
            .collect(),
    )
}

/// The OTLP/JSON body of one export request.
pub fn export_body(service_name: &str, spans: &[Span]) -> Value {
    json!({
        "resourceSpans": [{
            "resource": { "attributes": attributes(&[
                ("service.name", Attr::from(service_name)),
                ("service.version", Attr::from(env!("CARGO_PKG_VERSION"))),
                ("telemetry.sdk.name", Attr::from("tripletrail")),
                ("telemetry.sdk.language", Attr::from("rust")),
            ]) },
            "scopeSpans": [{
                "scope": { "name": "tripletrail-server", "version": env!("CARGO_PKG_VERSION") },
                "spans": spans.iter().map(Span::to_json).collect::<Vec<_>>(),
            }],
        }],
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != 2 * N || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// A W3C `traceparent` header: the caller's trace, its span, and whether it is sampled.
pub fn parse_traceparent(header: &str) -> Option<([u8; 16], [u8; 8], bool)> {
    let mut parts = header.trim().split('-');
    let version = parts.next()?;
    let trace = unhex::<16>(parts.next()?)?;
    let parent = unhex::<8>(parts.next()?)?;
    let flags = unhex::<1>(parts.next()?)?;
    // Version 00 has exactly four parts; a later version may add more, read as 00.
    if version.len() != 2 || version == "ff" || (version == "00" && parts.next().is_some()) {
        return None;
    }
    // All-zero ids are invalid.
    if trace == [0; 16] || parent == [0; 8] {
        return None;
    }
    Some((trace, parent, flags[0] & 1 == 1))
}

fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    if getrandom::fill(&mut out).is_err() || out == [0; N] {
        // Never all zero (invalid); a failed random source still gives distinct ids.
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(1, |d| d.as_nanos());
        for (i, o) in out.iter_mut().enumerate() {
            *o = (n >> (8 * (i % 16))) as u8 | 1;
        }
    }
    out
}

/// The spans of one request, while it runs.
pub struct Trace {
    trace_id: [u8; 16],
    root: [u8; 8],
    parent: Option<[u8; 8]>,
    start: SystemTime,
    spans: Vec<Span>,
    pub attributes: Vec<(&'static str, Attr)>,
}

thread_local! {
    /// The trace of the request this worker thread answers, so the engine's steps deep in the
    /// call stack can add spans without a parameter on every function.
    static CURRENT: RefCell<Option<Trace>> = const { RefCell::new(None) };
}

impl Trace {
    /// The trace of a request with this `traceparent` (or none), if it is sampled.
    pub fn start(traceparent: Option<&str>, sample: f64) -> Option<Self> {
        let (trace_id, parent) = match traceparent.and_then(parse_traceparent) {
            Some((_, _, false)) => return None,
            Some((trace, parent, true)) => (trace, Some(parent)),
            None => {
                let roll = u64::from_le_bytes(random::<8>()) as f64 / u64::MAX as f64;
                if roll >= sample {
                    return None;
                }
                (random(), None)
            }
        };
        Some(Self {
            trace_id,
            root: random(),
            parent,
            start: SystemTime::now(),
            spans: Vec::new(),
            attributes: Vec::new(),
        })
    }

    /// Makes this the current thread's trace, until [`Trace::take`].
    pub fn enter(self) {
        CURRENT.with(|c| *c.borrow_mut() = Some(self));
    }

    /// The current thread's trace, which is no longer current.
    pub fn take() -> Option<Self> {
        CURRENT.with(|c| c.borrow_mut().take())
    }

    /// A step of the request, from `start` to now.
    pub fn add(&mut self, name: &str, start: SystemTime, error: Option<String>) {
        self.spans.push(Span {
            trace_id: self.trace_id,
            span_id: random(),
            parent: Some(self.root),
            name: name.to_owned(),
            server: false,
            start,
            end: SystemTime::now(),
            attributes: Vec::new(),
            error,
        });
    }

    /// The request's span, with its steps, ending now.
    pub fn finish(mut self, name: String, error: Option<String>) -> Vec<Span> {
        let root = Span {
            trace_id: self.trace_id,
            span_id: self.root,
            parent: self.parent,
            name,
            server: true,
            start: self.start,
            end: SystemTime::now(),
            attributes: std::mem::take(&mut self.attributes),
            error,
        };
        let mut spans = self.spans;
        spans.push(root);
        spans
    }

    /// The request's `traceparent` for calls it makes (a SERVICE to another endpoint).
    pub fn traceparent(&self) -> String {
        format!("00-{}-{}-01", hex(&self.trace_id), hex(&self.root))
    }
}

/// Runs `f` as a step of the current request's trace, if it has one.
pub fn step<T, E: std::fmt::Display>(name: &str, f: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
    let traced = CURRENT.with(|c| c.borrow().is_some());
    if !traced {
        return f();
    }
    let start = SystemTime::now();
    let result = f();
    let error = result.as_ref().err().map(ToString::to_string);
    CURRENT.with(|c| {
        if let Some(t) = c.borrow_mut().as_mut() {
            t.add(name, start, error);
        }
    });
    result
}

/// Sets an attribute of the current request's span, if it is traced.
pub fn attribute(key: &'static str, value: impl Into<Attr>) {
    CURRENT.with(|c| {
        if let Some(t) = c.borrow_mut().as_mut() {
            t.attributes.push((key, value.into()));
        }
    });
}

/// Sends finished spans to the collector, from a thread of its own.
pub struct Tracer {
    pub options: OtlpOptions,
    queue: Arc<(Mutex<Queue>, Condvar)>,
}

#[derive(Default)]
struct Queue {
    spans: VecDeque<Span>,
    dropped: u64,
    sent: u64,
    failed: u64,
    stopping: bool,
}

impl Tracer {
    /// Starts the exporting thread.
    pub fn start(options: OtlpOptions) -> Result<Arc<Self>, String> {
        let endpoint = options.endpoint.clone().ok_or("no OTLP endpoint")?;
        if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
            return Err(format!(
                "--otlp-endpoint must be an http(s) URL, not {endpoint:?}"
            ));
        }
        let tracer = Arc::new(Self {
            options,
            queue: Arc::default(),
        });
        let queue = Arc::clone(&tracer.queue);
        let options = tracer.options.clone();
        std::thread::Builder::new()
            .name("otlp-export".into())
            .spawn(move || export_loop(&endpoint, &options, &queue))
            .map_err(|e| e.to_string())?;
        Ok(tracer)
    }

    /// Queues spans to be sent.
    pub fn send(&self, spans: Vec<Span>) {
        let (lock, wake) = &*self.queue;
        let mut queue = lock.lock().unwrap_or_else(|e| e.into_inner());
        for span in spans {
            if queue.spans.len() >= MAX_QUEUED {
                queue.dropped += 1;
            } else {
                queue.spans.push_back(span);
            }
        }
        if queue.spans.len() >= BATCH {
            wake.notify_one();
        }
    }

    /// Spans sent, dropped because the queue was full, and lost to failed exports.
    pub fn counts(&self) -> (u64, u64, u64) {
        let queue = self.queue.0.lock().unwrap_or_else(|e| e.into_inner());
        (queue.sent, queue.dropped, queue.failed)
    }

    /// Sends what is queued and stops the thread (at shutdown), waiting at most `wait`.
    pub fn flush(&self, wait: Duration) {
        let (lock, wake) = &*self.queue;
        let mut queue = lock.lock().unwrap_or_else(|e| e.into_inner());
        queue.stopping = true;
        wake.notify_one();
        let until = std::time::Instant::now() + wait;
        while !queue.spans.is_empty() && std::time::Instant::now() < until {
            queue = wake
                .wait_timeout(queue, Duration::from_millis(50))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

fn export_loop(endpoint: &str, options: &OtlpOptions, queue: &(Mutex<Queue>, Condvar)) {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build();
    let (lock, wake) = queue;
    let mut warned = false;
    loop {
        let batch: Vec<Span> = {
            let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
            if q.spans.len() < BATCH && !q.stopping {
                q = wake
                    .wait_timeout(q, FLUSH_EVERY)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            if q.spans.is_empty() {
                if q.stopping {
                    wake.notify_all();
                    return;
                }
                continue;
            }
            let n = q.spans.len().min(BATCH);
            q.spans.drain(..n).collect()
        };
        let body = export_body(&options.service_name, &batch).to_string();
        let mut request = agent.post(endpoint).set("Content-Type", "application/json");
        for (k, v) in &options.headers {
            request = request.set(k, v);
        }
        let ok = match request.send_string(&body) {
            Ok(_) => true,
            Err(e) => {
                // Said once, not per batch: a collector that is down must not fill the log.
                if !warned {
                    eprintln!("warning: sending traces to {endpoint} failed: {e}");
                    warned = true;
                }
                false
            }
        };
        let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
        if ok {
            q.sent += batch.len() as u64;
            warned = false;
        } else {
            q.failed += batch.len() as u64;
        }
        wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_traceparent_as_w3c_writes_it() {
        // The W3C Trace Context example.
        let (trace, parent, sampled) =
            parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").unwrap();
        assert_eq!(hex(&trace), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(hex(&parent), "00f067aa0ba902b7");
        assert!(sampled);
        let (_, _, sampled) =
            parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00").unwrap();
        assert!(!sampled);
        // A later version may add fields.
        assert!(
            parse_traceparent("01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-xyz")
                .is_some()
        );
        for bad in [
            "",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e473-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
            "00-4bf92f3577b34da6a3ce929d0e0e47zz-00f067aa0ba902b7-01",
        ] {
            assert!(parse_traceparent(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_trace_joins_its_parent_or_starts_one() {
        let t = Trace::start(
            Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
            0.0,
        )
        .expect("a sampled parent is followed whatever the ratio");
        assert_eq!(hex(&t.trace_id), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert!(
            Trace::start(
                Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00"),
                1.0
            )
            .is_none()
        );
        assert!(Trace::start(None, 0.0).is_none());
        let t = Trace::start(None, 1.0).unwrap();
        assert!(t.parent.is_none() && t.trace_id != [0; 16]);
        assert!(
            t.traceparent()
                .starts_with(&format!("00-{}-", hex(&t.trace_id)))
        );
    }

    #[test]
    fn steps_are_children_of_the_request_span() {
        Trace::start(None, 1.0).unwrap().enter();
        let ok: Result<u8, String> = step("tripletrail.prepare", || Ok(1));
        assert_eq!(ok, Ok(1));
        let err: Result<u8, String> = step("tripletrail.evaluate", || Err("timeout".into()));
        assert!(err.is_err());
        attribute("http.response.status_code", 503);
        let spans = Trace::take()
            .unwrap()
            .finish("GET /ds/query".into(), Some("timeout".into()));
        // Outside a trace a step just runs.
        assert_eq!(step::<_, String>("x", || Ok(2)), Ok(2));
        assert!(Trace::take().is_none());

        let root = spans.last().unwrap();
        assert!(root.server && root.parent.is_none());
        assert_eq!(spans.len(), 3);
        for s in &spans[..2] {
            assert_eq!(s.parent, Some(root.span_id));
            assert_eq!(s.trace_id, root.trace_id);
        }
        assert_eq!(spans[1].error.as_deref(), Some("timeout"));

        let body = export_body("tripletrail", &spans);
        let span = &body["resourceSpans"][0]["scopeSpans"][0]["spans"][2];
        assert_eq!(span["kind"], 2);
        assert_eq!(span["traceId"].as_str().unwrap().len(), 32);
        assert_eq!(span["spanId"].as_str().unwrap().len(), 16);
        assert_eq!(span["status"]["code"], 2);
        assert_eq!(
            span["attributes"][0],
            json!({"key": "http.response.status_code", "value": {"intValue": "503"}})
        );
        assert_eq!(
            body["resourceSpans"][0]["resource"]["attributes"][0],
            json!({"key": "service.name", "value": {"stringValue": "tripletrail"}})
        );
        let child = &body["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(child["kind"], 1);
        assert_eq!(child["parentSpanId"], span["spanId"]);
        assert!(span.get("parentSpanId").is_none());
    }

    #[test]
    fn reads_the_standard_endpoint_and_headers() {
        assert_eq!(
            OtlpOptions::traces_url("http://collector:4318"),
            "http://collector:4318/v1/traces"
        );
        assert_eq!(
            OtlpOptions::traces_url("http://collector:4318/v1/traces/"),
            "http://collector:4318/v1/traces"
        );
        assert_eq!(
            OtlpOptions::parse_headers("api-key=a%20b+c, x-team = rail").unwrap(),
            [
                ("api-key".to_owned(), "a b+c".to_owned()),
                ("x-team".to_owned(), "rail".to_owned())
            ]
        );
        assert!(OtlpOptions::parse_headers("novalue").is_err());
    }
}
