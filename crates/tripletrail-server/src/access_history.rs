//! The change history of the access files (#205): after every change made through
//! `/$/access`, the configuration file and the policy files it names are kept as a version in
//! `.access-history/` next to the configuration, with when, by whom and what. A version can be
//! shown next to the one before it, and restored. The first change first keeps the files as
//! they were found. The last [`KEEP`] versions are kept.

use crate::HttpError;
use crate::admin::rfc3339;
use crate::auth::{Auth, write_atomic};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// Versions kept: the oldest go first.
pub(crate) const KEEP: u64 = 100;

static WRITING: Mutex<()> = Mutex::new(());

fn folder(auth: &Auth) -> PathBuf {
    auth.config_path()
        .parent()
        .unwrap_or(Path::new("."))
        .join(".access-history")
}

/// The kept versions, oldest first.
fn versions(folder: &Path) -> Vec<u64> {
    let mut versions: Vec<u64> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let name = e.ok()?.file_name().into_string().ok()?;
            name.strip_prefix('v')?.strip_suffix(".json")?.parse().ok()
        })
        .collect();
    versions.sort_unstable();
    versions
}

fn read(folder: &Path, version: u64) -> Result<Value, HttpError> {
    let text = std::fs::read_to_string(folder.join(format!("v{version}.json")))
        .map_err(|_| HttpError(404, format!("No version {version} in the access history")))?;
    serde_json::from_str(&text).map_err(|e| HttpError(500, format!("version {version}: {e}")))
}

/// The files of a kept version, as (path, text).
fn files_of(version: &Value) -> Vec<(PathBuf, String)> {
    version["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| {
            Some((
                PathBuf::from(f["path"].as_str()?),
                f["text"].as_str()?.to_owned(),
            ))
        })
        .collect()
}

/// Keeps the access files as they are now, as the next version, unless they are the same as
/// the last one.
pub(crate) fn record(auth: &Auth, by: Option<&str>, change: &str) -> Result<(), HttpError> {
    let _one_at_a_time = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let folder = folder(auth);
    let files: Vec<(PathBuf, String)> = auth
        .access_files()
        .into_iter()
        .filter_map(|p| Some((p.clone(), std::fs::read_to_string(&p).ok()?)))
        .collect();
    let kept = versions(&folder);
    if let Some(last) = kept.last()
        && read(&folder, *last).is_ok_and(|v| files_of(&v) == files)
    {
        return Ok(());
    }
    let version = kept.last().map_or(1, |v| v + 1);
    std::fs::create_dir_all(&folder)
        .map_err(|e| HttpError(500, format!("{}: {e}", folder.display())))?;
    let value = json!({
        "version": version,
        "time": rfc3339(SystemTime::now()),
        "by": by,
        "change": change,
        "files": files.iter().map(|(p, t)| json!({"path": p.display().to_string(), "text": t})).collect::<Vec<_>>(),
    });
    // The versions hold key hashes, as the configuration does: they get its permissions.
    write_atomic(
        &folder.join(format!("v{version}.json")),
        &value.to_string(),
        auth.config_path(),
    )
    .map_err(|e| HttpError(500, e))?;
    for old in kept {
        if old + KEEP <= version {
            let _ = std::fs::remove_file(folder.join(format!("v{old}.json")));
        }
    }
    Ok(())
}

/// Before a change or a restore: keeps the files as they are, when no version holds them
/// yet. The first time that is the files as they were found; later, changes made to them by
/// hand (or by another tool), which a restore would otherwise undo without a trace.
pub(crate) fn start(auth: &Auth) -> Result<(), HttpError> {
    let change = if versions(&folder(auth)).is_empty() {
        "As found"
    } else {
        "Changed outside the access pages"
    };
    // `record` keeps nothing when the files are those of the last version.
    record(auth, None, change)
}

/// The versions, newest first, without their files.
pub(crate) fn list(auth: &Auth) -> Value {
    let folder = folder(auth);
    let versions: Vec<Value> = versions(&folder)
        .into_iter()
        .rev()
        .filter_map(|v| {
            let mut value = read(&folder, v).ok()?;
            let files = value
                .as_object_mut()?
                .remove("files")
                .and_then(|f| f.as_array().map(|a| a.len()));
            value["fileCount"] = json!(files);
            Some(value)
        })
        .collect();
    json!({ "versions": versions, "keep": KEEP })
}

/// One version with its files, the files of the version before it (what that change did)
/// and the files as they are now (what a restore of it would change). Key hashes in the
/// configuration are cut to the 8 hex digits the access pages show.
pub(crate) fn show(auth: &Auth, version: u64) -> Result<Value, HttpError> {
    let folder = folder(auth);
    let config = auth.config_path().display().to_string();
    let cut = |files: &Value| -> Value {
        Value::Array(
            files
                .as_array()
                .into_iter()
                .flatten()
                .map(|f| {
                    let mut f = f.clone();
                    if f["path"].as_str() == Some(config.as_str())
                        && let Some(text) = f["text"].as_str()
                    {
                        f["text"] = json!(cut_hashes(text));
                    }
                    f
                })
                .collect(),
        )
    };
    let mut value = read(&folder, version)?;
    value["files"] = cut(&value["files"]);
    let before = versions(&folder)
        .into_iter()
        .rfind(|v| *v < version)
        .and_then(|v| read(&folder, v).ok());
    value["before"] = before.map_or(
        Value::Null,
        |b| json!({ "version": b["version"], "files": cut(&b["files"]) }),
    );
    let current: Vec<Value> = auth
        .access_files()
        .into_iter()
        .filter_map(|p| {
            let text = std::fs::read_to_string(&p).ok()?;
            Some(json!({"path": p.display().to_string(), "text": text}))
        })
        .collect();
    value["current"] = cut(&Value::Array(current));
    Ok(value)
}

/// `text` (a configuration) with every 64-hex-digit `"sha256"` value cut to its first 8
/// digits, as the access pages show key ids.
fn cut_hashes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("\"sha256\"") {
        let (head, tail) = rest.split_at(at + "\"sha256\"".len());
        out.push_str(head);
        // `: "` then the digits.
        let Some(open) = tail.find('"') else {
            rest = tail;
            break;
        };
        let digits = &tail[open + 1..];
        let len = digits.chars().take_while(char::is_ascii_hexdigit).count();
        out.push_str(&tail[..=open]);
        if len == 64 {
            out.push_str(&digits[..8]);
            out.push('…');
        } else {
            out.push_str(&digits[..len]);
        }
        rest = &digits[len..];
    }
    out.push_str(rest);
    out
}

/// Puts the access files back as they were in `version` (keeping the current API keys), and
/// keeps that as a new version.
pub(crate) fn restore(
    auth: &Auth,
    version: u64,
    by: Option<&str>,
    options: &crate::Options,
    change: &str,
) -> Result<(), HttpError> {
    let value = read(&folder(auth), version)?;
    let files = files_of(&value);
    // A restore that changes row rules is a change of row-level policies (#818).
    for (path, text) in &files {
        let now = std::fs::read_to_string(path).unwrap_or_default();
        crate::row_policies::check_change(options, &now, text).map_err(|e| HttpError(403, e))?;
    }
    // A policy file with row rules that the version does not have stays on disk, but the
    // restored configuration may no longer name it, which takes its rules out of force: that
    // is a change of row-level policies too.
    for path in auth.access_files() {
        if !files.iter().any(|(p, _)| *p == path) {
            let now = std::fs::read_to_string(&path).unwrap_or_default();
            crate::row_policies::check_change(options, &now, "").map_err(|e| HttpError(403, e))?;
        }
    }
    auth.restore(&files).map_err(|e| HttpError(422, e))?;
    record(auth, by, change)
}
