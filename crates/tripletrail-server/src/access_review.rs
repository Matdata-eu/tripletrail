//! Reviewed access changes (#207, TripleTrail Enterprise): with `"review": true` in the `auth`
//! object, a change made through `/$/access` (a policy written or removed, anonymous access
//! turned on or off, a version of the history restored, a key for an admin created) is not
//! made at once but proposed, and made when a second admin approves it. No admin can change
//! who may read and write the data alone.
//!
//! The proposals are kept in `.access-history/proposals.json` next to the configuration, the
//! last [`KEEP`] decided ones with the pending ones. A proposal is made again when it is
//! approved, onto the files as they are then: a change that no longer applies fails then and
//! stays pending, to be rejected.

use crate::access::{self, Change};
use crate::admin::rfc3339;
use crate::auth::{Auth, write_atomic};
use crate::{HttpError, Reply, State, json_reply};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// Decided proposals kept.
const KEEP: usize = 100;

static WRITING: Mutex<()> = Mutex::new(());

fn file(auth: &Auth) -> PathBuf {
    auth.config_path()
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join(".access-history")
        .join("proposals.json")
}

fn read(auth: &Auth) -> Result<Vec<Value>, HttpError> {
    match std::fs::read_to_string(file(auth)) {
        Ok(text) => serde_json::from_str::<Vec<Value>>(&text)
            .map_err(|e| HttpError(500, format!("{}: {e}", file(auth).display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(HttpError(500, format!("{}: {e}", file(auth).display()))),
    }
}

fn write(auth: &Auth, proposals: &[Value]) -> Result<(), HttpError> {
    let path = file(auth);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| HttpError(500, format!("{}: {e}", dir.display())))?;
    }
    // A proposed key spec is no secret, but the folder holds key hashes: same permissions as
    // the configuration.
    write_atomic(
        &path,
        &Value::Array(proposals.to_vec()).to_string(),
        auth.config_path(),
    )
    .map_err(|e| HttpError(500, e))
}

/// `GET /$/access/proposals`: newest first.
pub(crate) fn list(auth: &Auth) -> Value {
    let mut proposals = read(auth).unwrap_or_default();
    proposals.reverse();
    json!({ "review": auth.review(), "proposals": proposals })
}

/// Holds `change` for a second admin: 202 with the proposal.
pub(crate) fn propose(auth: &Auth, change: &Change, by: Option<&str>) -> Result<Reply, HttpError> {
    let Some(by) = by else {
        return Err(HttpError(
            403,
            "Access changes are reviewed (auth.review): a change needs a caller with an agent, \
             so that another admin can approve it"
                .to_owned(),
        ));
    };
    let _one_at_a_time = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let mut proposals = read(auth)?;
    let n = proposals
        .iter()
        .filter_map(|p| p["proposal"].as_u64())
        .max()
        .unwrap_or(0)
        + 1;
    let proposal = json!({
        "proposal": n,
        "status": "pending",
        "time": rfc3339(SystemTime::now()),
        "by": by,
        "description": change.describe(),
        "change": change.to_json(),
    });
    proposals.push(proposal.clone());
    write(auth, &proposals)?;
    Ok(json_reply(202, &proposal))
}

/// `POST /$/access/proposals/{n}/approve` or `…/reject`.
pub(crate) fn decide(
    state: &Arc<State>,
    auth: &Auth,
    n: u64,
    approve: bool,
    by: Option<&str>,
) -> Result<Reply, HttpError> {
    let _one_at_a_time = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let mut proposals = read(auth)?;
    let at = proposals
        .iter()
        .position(|p| p["proposal"].as_u64() == Some(n))
        .ok_or_else(|| HttpError(404, format!("No proposal {n}")))?;
    if proposals[at]["status"] != "pending" {
        return Err(HttpError(
            409,
            format!(
                "Proposal {n} is {} already",
                proposals[at]["status"].as_str().unwrap_or("decided")
            ),
        ));
    }
    let proposer = proposals[at]["by"].as_str().map(str::to_owned);
    let mut reply = None;
    if approve {
        let Some(by) = by else {
            return Err(HttpError(
                403,
                "Approving needs a caller with an agent".to_owned(),
            ));
        };
        if proposer.as_deref() == Some(by) {
            return Err(HttpError(
                403,
                format!("Proposal {n} is yours: another admin approves it"),
            ));
        }
        let change = Change::from_json(&proposals[at]["change"])
            .ok_or_else(|| HttpError(500, format!("Proposal {n} cannot be read")))?;
        // Made now, onto the files as they are: when it fails, it stays pending.
        reply = Some(access::apply(
            state,
            auth,
            &change,
            Some(by),
            Some((n, proposer.as_deref())),
        )?);
    }
    let p = &mut proposals[at];
    p["status"] = json!(if approve { "approved" } else { "rejected" });
    p["decidedBy"] = json!(by);
    p["decided"] = json!(rfc3339(SystemTime::now()));
    let decided = p.clone();
    // The pending ones and the last KEEP decided.
    let decided_count = proposals
        .iter()
        .filter(|p| p["status"] != "pending")
        .count();
    let mut drop = decided_count.saturating_sub(KEEP);
    proposals.retain(|p| {
        if drop > 0 && p["status"] != "pending" {
            drop -= 1;
            false
        } else {
            true
        }
    });
    write(auth, &proposals)?;
    // What the change answers (a new key is shown here, once, to the approver), with the
    // proposal.
    let mut body = match reply {
        Some((_, Value::Object(m))) => Value::Object(m),
        _ => json!({}),
    };
    body["proposal"] = decided;
    Ok(json_reply(200, &body))
}
