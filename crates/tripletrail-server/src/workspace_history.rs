//! A workspace's history (#405): the runs other clients (the Python client, curl, a notebook)
//! made against it, for the workbench to show. Queries and updates keep their text; validations
//! and inferences answered as JSON keep that answer, so the workbench shows the report, or the
//! inferred triples with their why?, as if it had run them itself. The workbench's own requests
//! (sent with `X-TripleTrail-Client: workbench`, or `ui` from the rest of the UI) are not recorded: it shows those already.
//!
//! The history is in memory only, bounded by run count and kept bytes, and lost on a restart.

use crate::Reply;
use crate::admin::rfc3339;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::Read;
use std::sync::Arc;
use std::time::SystemTime;
use tiny_http::Request;

/// Runs kept per workspace: the oldest go first.
const MAX_RUNS: usize = 100;
/// The largest answer kept with a run.
const MAX_RESULT: usize = 8 << 20;
/// Answers kept per workspace, together: the oldest answers are dropped first (their runs stay).
const MAX_KEPT: usize = 32 << 20;
/// The longest query or update text kept, in bytes.
const MAX_TEXT: usize = 64 << 10;

/// The parameters not worth showing again: the text itself, and paging.
const SKIPPED: [&str; 5] = ["query", "update", "limit", "offset", "using-graph-uri"];

pub(crate) struct Run {
    pub(crate) op: String,
    /// The pipeline step whose store it ran on.
    pub(crate) step: Option<String>,
    pub(crate) client: String,
    pub(crate) text: Option<String>,
    pub(crate) params: Vec<(String, String)>,
    pub(crate) status: u16,
    pub(crate) ms: f64,
    pub(crate) summary: Option<String>,
    pub(crate) result: Option<Arc<Vec<u8>>>,
}

#[derive(Default)]
pub(crate) struct History {
    runs: VecDeque<(u64, SystemTime, Run)>,
    next: u64,
}

impl History {
    pub(crate) fn push(&mut self, mut run: Run) {
        if let Some(text) = &mut run.text
            && text.len() > MAX_TEXT
        {
            let mut end = MAX_TEXT;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push_str("\n# … (cut)");
        }
        self.next += 1;
        self.runs.push_back((self.next, SystemTime::now(), run));
        while self.runs.len() > MAX_RUNS {
            self.runs.pop_front();
        }
        let mut kept: usize = self
            .runs
            .iter()
            .filter_map(|(_, _, r)| r.result.as_ref().map(|b| b.len()))
            .sum();
        for (_, _, r) in &mut self.runs {
            if kept <= MAX_KEPT {
                break;
            }
            if let Some(b) = r.result.take() {
                kept -= b.len();
            }
        }
    }

    pub(crate) fn clear(&mut self) {
        self.runs.clear();
    }

    /// The runs, newest first, after run `since` (all with 0).
    pub(crate) fn list(&self, since: u64) -> Value {
        let runs: Vec<Value> = self
            .runs
            .iter()
            .rev()
            .filter(|(id, _, _)| *id > since)
            .map(|(id, at, r)| describe(*id, *at, r))
            .collect();
        json!({ "runs": runs, "last": self.next })
    }

    /// One run, with its kept answer under `result`.
    pub(crate) fn get(&self, id: u64) -> Option<Value> {
        let (id, at, r) = self.runs.iter().find(|(i, _, _)| *i == id)?;
        let mut v = describe(*id, *at, r);
        if let Some(b) = &r.result {
            v["result"] = serde_json::from_slice(b).unwrap_or(Value::Null);
        }
        Some(v)
    }
}

fn describe(id: u64, at: SystemTime, r: &Run) -> Value {
    let params: serde_json::Map<String, Value> = r
        .params
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    json!({
        "id": id,
        "at": rfc3339(at),
        "op": r.op,
        "step": r.step,
        "client": r.client,
        "text": r.text,
        "params": params,
        "status": r.status,
        "ms": (r.ms * 10.0).round() / 10.0,
        "summary": r.summary,
        "result": r.result.is_some(),
    })
}

/// Who sent the request, for the history: `None` for the workbench itself. The header
/// `X-TripleTrail-Client` names a client (the Python client sends `python`); otherwise the
/// product of `User-Agent` (`curl`, `Python-urllib`), or `http`.
pub(crate) fn client(request: &Request) -> Option<String> {
    let header = |name: &'static str| {
        request
            .headers()
            .iter()
            .find(|h| h.field.equiv(name))
            .map(|h| h.value.as_str().trim().to_owned())
    };
    let named = header("X-TripleTrail-Client").filter(|v| !v.is_empty());
    if matches!(named.as_deref(), Some("workbench" | "ui")) {
        return None;
    }
    let name = named
        .or_else(|| {
            header("User-Agent").and_then(|ua| {
                let product = ua.split(['/', ' ']).next().unwrap_or("").to_owned();
                (!product.is_empty()).then_some(product)
            })
        })
        .unwrap_or_else(|| "http".to_owned());
    Some(name.chars().filter(|c| !c.is_control()).take(40).collect())
}

/// The parameters to show with a run.
pub(crate) fn shown(params: &[(String, String)]) -> Vec<(String, String)> {
    params
        .iter()
        .filter(|(k, _)| !SKIPPED.contains(&k.as_str()))
        .take(20)
        .map(|(k, v)| (k.clone(), v.chars().take(200).collect()))
        .collect()
}

/// Reads a validation's or inference's answer to keep it, when small enough and known in
/// length; gives back the reply, with the same answer, and what was read.
pub(crate) fn capture(mut reply: Reply) -> (Reply, Option<Vec<u8>>) {
    match reply.length {
        Some(n) if n <= MAX_RESULT && reply.status < 300 => {
            let mut bytes = Vec::with_capacity(n);
            if reply.body.read_to_end(&mut bytes).is_err() {
                return (Reply::empty(500), None);
            }
            reply.body = Box::new(std::io::Cursor::new(bytes.clone()));
            (reply, Some(bytes))
        }
        _ => (reply, None),
    }
}

/// A line on what a run gave, from its answer.
pub(crate) fn summary(op: &str, json: Option<&Value>, body: Option<&[u8]>) -> Option<String> {
    let plural = |n: u64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    match (op, json) {
        ("infer", Some(v)) => v["count"]
            .as_u64()
            .map(|n| plural(n, "triple", "triples") + " inferred"),
        ("validate", Some(v)) => {
            if v["conforms"] == true {
                Some("conforms".to_owned())
            } else {
                let n = v["resultCount"]
                    .as_u64()
                    .or_else(|| v["results"].as_array().map(|a| a.len() as u64))?;
                Some(plural(n, "result", "results"))
            }
        }
        ("tests", Some(v)) => Some(format!(
            "{} passed, {} failed",
            v["passed"].as_u64().unwrap_or(0),
            v["failed"].as_u64().unwrap_or(0)
        )),
        ("infer", None) => body.map(|b| {
            let n = b.split(|c| *c == b'\n').filter(|l| !l.is_empty()).count() as u64;
            plural(n, "triple", "triples") + " inferred"
        }),
        _ => None,
    }
}
