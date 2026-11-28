//! `SERVICE <https://…>`: SPARQL 1.1 Federated Query against remote endpoints (#192).
//!
//! Off unless [`crate::Engine::with_remote_services`] gives an allowlist: a query may only call
//! endpoints whose IRI starts with one of its entries. Each call sends the SERVICE pattern as a
//! `SELECT *` query (POST, `application/sparql-query`), waits at most [`RemoteConfig::timeout`],
//! reads at most [`RemoteConfig::max_bytes`] of answer, and can be cached for
//! [`RemoteConfig::cache_ttl`]. `SERVICE SILENT` and `SERVICE ?endpoint` (with the endpoint
//! bound by a `LATERAL` or a sub-query before it) are evaluated by the query engine around
//! these calls.

use oxigraph::model::Variable;
use oxigraph::sparql::QuerySolution;
use oxigraph::sparql::QuerySolutionIter;
use oxigraph::sparql::results::{
    QueryResultsFormat, QueryResultsParser, ReaderQueryResultsParserOutput,
};
use oxiri::Iri;
use spargebra::Query;
use spargebra::algebra::GraphPattern;
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Which remote endpoints a query may call, and how.
#[derive(Clone, Debug)]
pub struct RemoteConfig {
    /// The endpoints that may be called (`https://query.wikidata.org/sparql`, or
    /// `https://data-interop.era.europa.eu/` for every endpoint under it): same scheme, host
    /// and port, and a path prefix on a `/` boundary. Empty: none.
    pub allow: Vec<String>,
    /// The longest one call may take, connecting and reading included.
    pub timeout: Duration,
    /// The largest answer read from one call; a larger one is an error.
    pub max_bytes: usize,
    /// How long an answer is reused for the same endpoint and query; zero to never reuse.
    pub cache_ttl: Duration,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            allow: Vec::new(),
            timeout: Duration::from_secs(30),
            max_bytes: 64 << 20,
            cache_ttl: Duration::ZERO,
        }
    }
}

/// Answers kept in the cache at most.
const CACHE_ENTRIES: usize = 256;
/// ureq 2's connect timeout, kept with ureq 3 (which has none by default).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

type Answer = (
    Arc<[Variable]>,
    Arc<Vec<Vec<Option<oxigraph::model::Term>>>>,
);

/// Answers by (endpoint, query), with when they were fetched.
type Cache = HashMap<(String, String), (Instant, Answer)>;

/// The remote SERVICE caller (cheap to clone: the cache is shared).
#[derive(Clone)]
pub struct RemoteService {
    config: Arc<RemoteConfig>,
    agent: ureq::Agent,
    cache: Arc<Mutex<Cache>>,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct RemoteError(pub String);

impl RemoteService {
    pub fn new(config: RemoteConfig) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(config.timeout))
            .timeout_connect(Some(CONNECT_TIMEOUT))
            // A redirect could lead off the allowlist (to a host inside the network).
            .max_redirects(0)
            // An error status is read below, for the endpoint's reason.
            .http_status_as_error(false)
            // No proxy from the environment, as before ureq 3.
            .proxy(None)
            .user_agent(concat!("tripletrail/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Self {
            config: Arc::new(config),
            agent,
            cache: Arc::default(),
        }
    }

    /// Whether `endpoint` is on the allowlist: an entry with the same scheme, host and port
    /// whose path is a prefix of the endpoint's on a segment boundary. Endpoints with user
    /// info are never allowed.
    pub fn allows(&self, endpoint: &str) -> bool {
        tripletrail_fx::remote::allows(&self.config.allow, endpoint)
    }

    /// Evaluates `pattern` at `endpoint`.
    pub fn call(
        &self,
        endpoint: &str,
        pattern: &GraphPattern,
        base_iri: Option<&Iri<String>>,
    ) -> Result<QuerySolutionIter<'static>, RemoteError> {
        if !self.allows(endpoint) {
            return Err(RemoteError(format!(
                "SERVICE <{endpoint}> is not an allowed remote endpoint"
            )));
        }
        let query = Query::Select {
            dataset: None,
            pattern: pattern.clone(),
            base_iri: base_iri.cloned(),
        }
        .to_string();
        let key = (endpoint.to_owned(), query);
        let ttl = self.config.cache_ttl;
        if !ttl.is_zero() {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            match cache.get(&key) {
                Some((at, answer)) if at.elapsed() < ttl => return Ok(iter(answer.clone())),
                Some(_) => {
                    cache.remove(&key);
                }
                None => {}
            }
        }
        let answer = self.fetch(endpoint, &key.1)?;
        if !ttl.is_zero() {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if cache.len() >= CACHE_ENTRIES {
                cache.retain(|_, (at, _)| at.elapsed() < ttl);
                if cache.len() >= CACHE_ENTRIES {
                    cache.clear();
                }
            }
            cache.insert(key, (Instant::now(), answer.clone()));
        }
        Ok(iter(answer))
    }

    fn fetch(&self, endpoint: &str, query: &str) -> Result<Answer, RemoteError> {
        let fail = |e: String| RemoteError(format!("SERVICE <{endpoint}>: {e}"));
        let response = self
            .agent
            .post(endpoint)
            .header("Content-Type", "application/sparql-query")
            .header(
                "Accept",
                "application/sparql-results+json, application/sparql-results+xml;q=0.9, \
                 text/tab-separated-values;q=0.8",
            )
            .send(query)
            .map_err(|e| fail(e.to_string()))?;
        let code = response.status().as_u16();
        if code >= 400 {
            let mut text = String::new();
            let _ = response
                .into_body()
                .into_reader()
                .take(500)
                .read_to_string(&mut text);
            return Err(fail(format!("HTTP {code}: {}", text.trim())));
        }
        let media = response
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/sparql-results+xml")
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let format = QueryResultsFormat::from_media_type(&media)
            .ok_or_else(|| fail(format!("unsupported results format {media}")))?;
        let mut body = Vec::new();
        let limit = self.config.max_bytes;
        response
            .into_body()
            .into_reader()
            .take(limit as u64 + 1)
            .read_to_end(&mut body)
            .map_err(|e| fail(e.to_string()))?;
        if body.len() > limit {
            return Err(fail(format!("the answer is larger than {limit} bytes")));
        }
        match QueryResultsParser::from_format(format)
            .for_reader(body.as_slice())
            .map_err(|e| fail(e.to_string()))?
        {
            ReaderQueryResultsParserOutput::Solutions(solutions) => {
                let variables: Arc<[Variable]> = solutions.variables().into();
                let mut rows = Vec::new();
                for solution in solutions {
                    let solution = solution.map_err(|e| fail(e.to_string()))?;
                    rows.push(variables.iter().map(|v| solution.get(v).cloned()).collect());
                }
                Ok((variables, Arc::new(rows)))
            }
            ReaderQueryResultsParserOutput::Boolean(_) => {
                Err(fail("the endpoint answered a boolean".to_owned()))
            }
        }
    }
}

fn iter((variables, rows): Answer) -> QuerySolutionIter<'static> {
    let vars = Arc::clone(&variables);
    QuerySolutionIter::new(
        variables,
        (0..rows.len()).map(move |i| Ok(QuerySolution::from((Arc::clone(&vars), rows[i].clone())))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allows(entry: &str, endpoint: &str) -> bool {
        RemoteService::new(RemoteConfig {
            allow: vec![entry.to_owned()],
            ..RemoteConfig::default()
        })
        .allows(endpoint)
    }

    #[test]
    fn allowlist() {
        // Host-only entries cover that host only.
        assert!(allows("https://example.org", "https://example.org/sparql"));
        assert!(allows("https://example.org/", "https://example.org/sparql"));
        assert!(allows(
            "https://example.org",
            "https://EXAMPLE.org:443/sparql"
        ));
        assert!(!allows(
            "https://example.org",
            "https://example.org.evil.com/sparql"
        ));
        assert!(!allows(
            "https://example.org",
            "https://example.org@evil.com/sparql"
        ));
        assert!(!allows(
            "https://example.org",
            "https://user@example.org/sparql"
        ));
        assert!(!allows(
            "http://10.0.0.5:8080",
            "http://10.0.0.5:80801/sparql"
        ));
        assert!(allows(
            "http://10.0.0.5:8080",
            "http://10.0.0.5:8080/sparql"
        ));
        assert!(!allows("https://example.org", "http://example.org/sparql"));
        assert!(!allows(
            "https://example.org",
            "https://example.org:8443/sparql"
        ));
        // Paths match on segment boundaries, with or without a trailing slash.
        assert!(allows("https://example.org/kg", "https://example.org/kg"));
        assert!(allows(
            "https://example.org/kg",
            "https://example.org/kg/sparql"
        ));
        assert!(allows(
            "https://example.org/kg/",
            "https://example.org/kg/sparql"
        ));
        assert!(!allows(
            "https://example.org/kg",
            "https://example.org/kgx/sparql"
        ));
        assert!(!allows("https://example.org/kg/", "https://example.org/kg"));
        assert!(allows(
            "https://example.org/sparql",
            "https://example.org/sparql?default-graph-uri=x"
        ));
        // IPv6 and other schemes.
        assert!(allows("http://[::1]:7878", "http://[::1]:7878/kg/sparql"));
        assert!(!allows("http://[::1]:7878", "http://[::1]:7879/kg/sparql"));
        assert!(!allows("file:///etc", "file:///etc/passwd"));
        // Dot segments could leave the allowed path once the client normalises them.
        assert!(!allows(
            "https://host/public",
            "https://host/public/../private/sparql"
        ));
        assert!(!allows(
            "https://host/public",
            "https://host/public/%2E%2E/private"
        ));
        assert!(!allows(
            "https://host/public",
            "https://host/public/./sparql"
        ));
        assert!(!allows("https://host/public", "https://host/public/.%2e/x"));
        assert!(allows(
            "https://host/public",
            "https://host/public/..data/sparql"
        ));
    }
}
