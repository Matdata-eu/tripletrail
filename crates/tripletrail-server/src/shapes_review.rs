//! Reviewed shape changes (#145, TripleTrail Enterprise): on a dataset whose shapes come from a
//! git repository (#137), the shapes editor's changes are not made on the server but proposed
//! as a pull request (a merge request on GitLab). The server commits the changed files onto
//! the commit it pulled, on a new branch, through the forge's REST API with the binding's
//! token (no git push), and opens the request against the bound branch. The repository's own
//! review decides; on merge, the push webhook pulls the new commit, as for any push.
//!
//! The proposals are kept in `.proposals.json` in the dataset's shapes folder (not copied with
//! the shapes), the last [`KEEP`] of them.

use crate::admin::{Entry, rfc3339};
use crate::shapes::{self as local, Binding};
use crate::{HttpError, Params, State};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use tripletrail::oxigraph::model::vocab::rdf;
use tripletrail::oxigraph::model::{
    Graph, NamedNode, NamedOrBlankNode, NamedOrBlankNodeRef, TermRef,
};
use tripletrail::shapes::{Reader, SetFile};

fn sh(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("http://www.w3.org/ns/shacl#{local}"))
}

fn tts(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{}{local}", tripletrail::shapes::TTS))
}

/// Most characters of a proposal's description that go into its pull request.
const DESCRIPTION: usize = 20_000;

/// Proposals kept.
const KEEP: usize = 100;
const FILE: &str = ".proposals.json";

static WRITING: Mutex<()> = Mutex::new(());

/// The token proposals use on the forge: the binding's `reviewTokenFile`, else its `tokenFile`.
fn review_token(binding: &Binding) -> Result<String, HttpError> {
    binding
        .review_token_file
        .as_deref()
        .or(binding.token_file.as_deref())
        .ok_or_else(|| {
            HttpError(
                409,
                "Proposals need the binding's reviewTokenFile (or tokenFile): a token that may \
                 push branches and open pull requests"
                    .to_owned(),
            )
        })
        .and_then(|f| {
            std::fs::read_to_string(f)
                .map(|t| t.trim().to_owned())
                .map_err(|e| HttpError(500, format!("Reading the binding's token file: {e}")))
        })
}

/// Open proposals whose pull request a refresh asks the forge about, the newest first.
const REFRESHED: usize = 20;
/// A proposal checked this recently is not asked about again.
const RECHECK_AFTER: Duration = Duration::from_secs(60);
/// Longest one question to the forge about a pull request takes.
const STATE_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest a refresh asks the forge, all questions together: past it, the rest wait.
const REFRESH_BUDGET: Duration = Duration::from_secs(30);

/// The shapes folders a refresh is running for: one at a time each.
static REFRESHING: Mutex<BTreeSet<std::path::PathBuf>> = Mutex::new(BTreeSet::new());

/// The open proposals' pull requests asked of the forge (the newest [`REFRESHED`] not
/// checked in the last [`RECHECK_AFTER`], within [`REFRESH_BUDGET`]): a merged or closed one
/// gets that `status`, each gets `checked` (when), and `statusError` when the forge could not
/// say. It spends the server's review token, so it is bounded: one refresh at a time for a
/// dataset (another caller meanwhile gets the list as it stands), each question at most
/// [`STATE_TIMEOUT`].
fn refresh(state: &State, folder: &Path) -> Result<(), HttpError> {
    licensed(state)?;
    struct Running<'a>(&'a Path);
    impl Drop for Running<'_> {
        fn drop(&mut self) {
            REFRESHING
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(self.0);
        }
    }
    if !REFRESHING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(folder.to_owned())
    {
        return Ok(());
    }
    let _running = Running(folder);
    let binding = Binding::read(folder).ok_or_else(|| {
        HttpError(
            409,
            "The shapes are not bound to a repository: there are no pull requests to check"
                .to_owned(),
        )
    })?;
    let forge = Forge::of(&binding)?;
    let token = review_token(&binding)?;
    let open: Vec<(u64, u64)> = {
        let _one_at_a_time = WRITING.lock().unwrap_or_else(|e| e.into_inner());
        read(folder)?
            .iter()
            .rev()
            .filter(|p| p["status"] == "open")
            .filter(|p| {
                !p["checked"]
                    .as_str()
                    .and_then(crate::admin::parse_time)
                    .and_then(|at| SystemTime::now().duration_since(at).ok())
                    .is_some_and(|ago| ago < RECHECK_AFTER)
            })
            .filter_map(|p| Some((p["proposal"].as_u64()?, p["number"].as_u64()?)))
            .take(REFRESHED)
            .collect()
    };
    // The forge is asked without holding the proposals: a proposal may be made meanwhile.
    let started = std::time::Instant::now();
    let states: Vec<(u64, Result<&'static str, HttpError>)> = open
        .iter()
        .take_while(|_| started.elapsed() < REFRESH_BUDGET)
        .map(|(n, number)| (*n, forge.state(&token, *number)))
        .collect();
    let at = rfc3339(SystemTime::now());
    let _one_at_a_time = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let mut proposals = read(folder)?;
    for (n, found) in states {
        let Some(p) = proposals.iter_mut().find(|p| p["proposal"] == n) else {
            continue;
        };
        if let Some(fields) = p.as_object_mut() {
            fields.remove("statusError");
        }
        p["checked"] = json!(at);
        match found {
            Ok(status) => p["status"] = json!(status),
            Err(HttpError(_, e)) => p["statusError"] = json!(e),
        }
    }
    write(folder, &proposals)
}

/// The proposals of a dataset, newest first; with `refresh`, the open ones' pull requests
/// asked of the forge first.
pub(crate) fn list(state: &State, folder: &Path, refresh_them: bool) -> Result<Value, HttpError> {
    if refresh_them {
        refresh(state, folder)?;
    }
    let mut proposals = {
        let _one_at_a_time = WRITING.lock().unwrap_or_else(|e| e.into_inner());
        let mut proposals = read(folder)?;
        // A validation left queued or running by an earlier run of the server never ends.
        let mut cut = false;
        for p in &mut proposals {
            let v = &mut p["validation"];
            if (v["status"] == "queued" || v["status"] == "running") && v["server"] != this_server()
            {
                *v = json!({
                    "status": "interrupted",
                    "error": "The server stopped before the validation was done",
                });
                cut = true;
            }
        }
        if cut {
            write(folder, &proposals)?;
        }
        proposals
    };
    proposals.reverse();
    Ok(json!({
        "licensed": licensed(state).is_ok(),
        "bound": Binding::read(folder).is_some(),
        "proposals": proposals,
    }))
}

fn read(folder: &Path) -> Result<Vec<Value>, HttpError> {
    let path = folder.join(FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| HttpError(500, format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(HttpError(500, format!("{}: {e}", path.display()))),
    }
}

/// Sets proposal `n`'s validation.
fn set_validation(folder: &Path, n: u64, validation: Value) {
    let _one_at_a_time = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(mut proposals) = read(folder) {
        if let Some(p) = proposals.iter_mut().find(|p| p["proposal"] == n) {
            p["validation"] = validation;
        }
        let _ = write(folder, &proposals);
    }
}

fn write(folder: &Path, proposals: &[Value]) -> Result<(), HttpError> {
    let tmp = folder.join(".proposals.json.writing");
    std::fs::write(&tmp, Value::Array(proposals.to_vec()).to_string())?;
    std::fs::rename(&tmp, folder.join(FILE))?;
    Ok(())
}

/// Reviewed changes need a licence that covers them.
fn licensed(state: &State) -> Result<(), HttpError> {
    if state
        .options
        .license_status()
        .allows(tripletrail_enterprise::features::REVIEW)
    {
        Ok(())
    } else {
        Err(HttpError(
            403,
            "Reviewed shape changes need a TripleTrail Enterprise licence that covers review"
                .to_owned(),
        ))
    }
}

/// `POST …/shapes/proposals` with the editor's body (`changes`, `profiles`) and a `title`
/// (and a `description`): the changed files committed on a new branch and a pull request
/// opened for them. 201 with the proposal.
#[allow(clippy::too_many_arguments)]
pub(crate) fn propose(
    state: &State,
    entry: &Entry,
    folder: &Path,
    body: &Value,
    by: Option<&str>,
    dry: bool,
    validate: bool,
    timeout: Option<Duration>,
) -> Result<Value, HttpError> {
    licensed(state)?;
    let binding = Binding::read(folder).ok_or_else(|| {
        HttpError(
            409,
            format!(
                "The shapes of {} are not bound to a repository: proposals are pull requests \
                 on it (PUT …/shapes/git); without one, change the shapes with …/shapes/edit",
                entry.name
            ),
        )
    })?;
    let title = body["title"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| HttpError::bad_request("A proposal needs a title"))?;
    if title.chars().count() > 200 {
        return Err(HttpError::bad_request(
            "A proposal's title is at most 200 characters",
        ));
    }
    let description = body["description"].as_str().unwrap_or("").trim();
    let source = local::current_version(folder)
        .and_then(|v| local::version_source(folder, v))
        .filter(|s| s["commit"].is_string())
        .ok_or_else(|| {
            HttpError(
                409,
                format!(
                    "The shapes of {} have not been pulled from {} yet: pull first",
                    entry.name, binding.url
                ),
            )
        })?;
    if !source["files"].is_object() {
        return Err(HttpError(
            409,
            format!(
                "The shapes of {} were pulled before TripleTrail recorded where each file is in \
                 the repository: pull again (POST …/shapes/pull), then propose",
                entry.name
            ),
        ));
    }
    // A proposal needs a branch to open its request against: not a tag.
    let bound_branch = branch_of(&binding.reference)?;
    let base_commit = source["commit"].as_str().unwrap_or_default().to_owned();
    // The changed files, as the editor would write them (checked as it checks them).
    let edited = crate::shapes::edit::edit(
        state,
        entry,
        folder,
        &Params(vec![
            ("dry".to_owned(), String::new()),
            ("all".to_owned(), String::new()),
        ]),
        body,
    )?;
    let changed: Vec<(String, String, bool)> = edited["changed"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|name| {
            let known = source["files"][name].as_str();
            let path = match known {
                Some(p) => p.to_owned(),
                None => {
                    let dir = binding.path.trim_matches('/');
                    if dir.is_empty() {
                        name.to_owned()
                    } else {
                        format!("{dir}/{name}")
                    }
                }
            };
            let text = edited["files"][name]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            (path, text, known.is_some())
        })
        .collect();
    if changed.is_empty() {
        return Err(HttpError::bad_request(
            "The changes change no file: nothing to propose",
        ));
    }
    // What each reader would read differently, and the profiles' node shapes.
    let all: Vec<SetFile> = edited["files"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, text)| SetFile {
            name: name.clone(),
            text: text.as_str().unwrap_or_default().to_owned(),
        })
        .collect();
    let (effects, validating_before, validating_after) = effects(state, entry, &all)?;
    if dry {
        let mut answer = json!({
            "files": changed.iter().map(|(p, text, existed)| json!({
                "path": p, "new": !existed, "text": text,
            })).collect::<Vec<_>>(),
            "effects": effects,
        });
        if validate {
            // At once, within the request's time limit, and not beside a background one.
            let _one_validation = VALIDATING.try_lock().map_err(|_| {
                HttpError(
                    503,
                    "A proposal's validation is running: try again when it is done".to_owned(),
                )
            })?;
            answer["validation"] = validation(
                &entry.engine,
                &validating_before,
                &validating_after,
                timeout,
            );
        }
        return Ok(answer);
    }
    let forge = Forge::of(&binding)?;
    let token = review_token(&binding)?;

    let _one_at_a_time = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let mut proposals = read(folder)?;
    let n = proposals
        .iter()
        .filter_map(|p| p["proposal"].as_u64())
        .max()
        .unwrap_or(0)
        + 1;
    let branch = format!("tripletrail/{}-shapes-{n}", entry.name);
    let by = by.unwrap_or("admin");
    let message = if description.is_empty() {
        title.to_owned()
    } else {
        format!("{title}\n\n{description}")
    };
    let mut request_body = String::new();
    if !description.is_empty() {
        // The proposer's own words, kept well under the forge's limit with the rest.
        request_body.extend(description.chars().take(DESCRIPTION));
        request_body.push_str("\n\n");
    }
    request_body.push_str(&format!(
        "Proposed in TripleTrail by {} on the shapes of dataset {}, onto {}.\n\nFiles:\n",
        code(by),
        code(&entry.name),
        code(&base_commit),
    ));
    for (path, _, existed) in changed.iter().take(NAMED) {
        request_body.push_str(&format!(
            "- {}{}\n",
            code(path),
            if *existed { "" } else { " (new)" }
        ));
    }
    if changed.len() > NAMED {
        request_body.push_str(&format!("- and {} more files\n", changed.len() - NAMED));
    }
    request_body.push_str(&effects_text(&effects));
    let mut proposal = json!({
        "proposal": n,
        "title": title,
        "description": description,
        "by": by,
        "at": rfc3339(SystemTime::now()),
        "forge": forge.kind.name(),
        "branch": branch,
        "base": bound_branch,
        "commit": base_commit,
        "files": changed.iter().map(|(p, _, _)| p.as_str()).collect::<Vec<_>>(),
        "effects": effects,
    });
    let opened = match &bound_branch {
        Some(b) => Ok(b.clone()),
        None => forge.default_branch(&token),
    }
    .and_then(|base| {
        proposal["base"] = json!(base);
        forge.open(
            &token,
            &Change {
                base_commit: &base_commit,
                base: &base,
                branch: &branch,
                title,
                message: &message,
                body: &request_body,
                files: &changed,
            },
        )
    });
    // A failed attempt is kept too, so that the next one takes the next number (and branch):
    // the forge may have made this one's branch before it failed.
    match &opened {
        Ok(opened) => {
            proposal["number"] = json!(opened.number);
            proposal["url"] = json!(opened.url);
            proposal["status"] = json!("open");
            if validate {
                proposal["validation"] = json!({ "status": "queued", "server": this_server() });
            }
        }
        Err(HttpError(_, message)) => {
            proposal["status"] = json!("failed");
            proposal["error"] = json!(message);
        }
    }
    proposals.push(proposal.clone());
    let old = proposals.len().saturating_sub(KEEP);
    proposals.drain(..old);
    write(folder, &proposals)?;
    drop(_one_at_a_time);
    let opened = opened?;
    // The full data validated with the shapes as they are and as proposed, in the background;
    // the result goes into the proposal and onto the pull request.
    if validate {
        let engine = entry.engine.clone();
        let folder = folder.to_owned();
        let dataset = entry.name.clone();
        let with_results = binding.review_results;
        std::thread::Builder::new()
            .name(format!("tripletrail-proposal-{n}"))
            .spawn(move || {
                // One at a time: a validation reads the whole dataset, twice.
                let _one_validation = VALIDATING.lock().unwrap_or_else(|e| e.into_inner());
                set_validation(
                    &folder,
                    n,
                    json!({ "status": "running", "server": this_server() }),
                );
                let mut result = validation(
                    &engine,
                    &validating_before,
                    &validating_after,
                    Some(VALIDATION_TIME),
                );
                let comment = validation_text(&dataset, &result, with_results);
                if let Some(number) = opened.number
                    && let Err(HttpError(_, e)) = forge.comment(&token, number, &comment)
                {
                    result["comment"] = json!(format!("not posted: {e}"));
                }
                set_validation(&folder, n, result);
            })
            .map_err(|e| HttpError(500, format!("Starting the validation: {e}")))?;
    }
    Ok(proposal)
}

/// Longest a proposal's background validation runs, both sets of shapes together.
const VALIDATION_TIME: Duration = Duration::from_secs(60 * 60);
/// New results a validation lists, at most.
const NEW_LISTED: usize = 20;
/// Distinct results a validation tells apart, at most: past it, it gives counts by shape and
/// not which results are new or gone. Each takes some 30 bytes.
const RESULTS_COMPARED: usize = 4_000_000;

/// One background validation at a time on the server: the others wait, `queued`.
static VALIDATING: Mutex<()> = Mutex::new(());

/// This run of the server, in the proposals' validations it starts: one that is still queued
/// or running under another was cut off by a restart.
fn this_server() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        format!("{}-{nanos:x}", std::process::id())
    })
}

/// A term as text: an IRI as it is, anything else as in N-Triples.
fn text_of(t: &tripletrail::oxigraph::model::Term) -> String {
    match t {
        tripletrail::oxigraph::model::Term::NamedNode(n) => n.as_str().to_owned(),
        other => other.to_string(),
    }
}

/// A SHACL path as text, in SPARQL's syntax.
fn path_text(p: &tripletrail::shacl::Path) -> String {
    use tripletrail::shacl::Path;
    let inner = |p: &Path| match p {
        Path::Predicate(_) => path_text(p),
        _ => format!("({})", path_text(p)),
    };
    match p {
        Path::Predicate(n) => n.as_str().to_owned(),
        Path::Inverse(p) => format!("^{}", inner(p)),
        Path::Sequence(list) => list.iter().map(inner).collect::<Vec<_>>().join("/"),
        Path::Alternative(list) => list.iter().map(inner).collect::<Vec<_>>().join("|"),
        Path::ZeroOrMore(p) => format!("{}*", inner(p)),
        Path::OneOrMore(p) => format!("{}+", inner(p)),
        Path::ZeroOrOne(p) => format!("{}?", inner(p)),
    }
}

/// The dataset's data validated with `before` and with `after` (the validation reader's
/// shapes as they are and as proposed), both in one read of the store so that they see the
/// same data. Results are told apart by focus node, path, value, shape, component and
/// severity, and folded as they come: a count per result (by a 64-bit hash) and per shape,
/// and the first [`NEW_LISTED`] new ones in full. Gives how many results each set gives,
/// how many are new and gone, the new ones by shape, and the first new ones; past
/// [`RESULTS_COMPARED`] distinct results, the counts by shape only (`compared: false`).
fn validation(
    engine: &tripletrail::Engine,
    before: &Graph,
    after: &Graph,
    timeout: Option<Duration>,
) -> Value {
    use std::collections::HashMap;
    use std::hash::{Hash, Hasher};
    let started = std::time::Instant::now();
    let (engine, _deadline) = crate::with_deadline(engine, timeout);
    // Per result: after's count less before's, and its shape (an index into `shapes`).
    let mut found: HashMap<u64, (i64, u32)> = HashMap::new();
    let mut shapes: Vec<String> = Vec::new();
    let mut shape_index: HashMap<String, u32> = HashMap::new();
    let mut by_shape: Vec<[u64; 3]> = Vec::new(); // before, after, new
    let (mut counts, mut new, mut compared) = ([0u64; 2], 0u64, true);
    let mut listed: Vec<Value> = Vec::new();
    // A blank node shape is told by what it says (in its set), not by its label.
    let sets = [before, after];
    let mut blank: HashMap<(usize, tripletrail::oxigraph::model::Term), String> = HashMap::new();
    let outcome = engine.validate_graphs_results(&[before, after], &mut |set, r| {
        counts[set] += 1;
        let shape = match r.source_shape {
            tripletrail::oxigraph::model::Term::BlankNode(_) => blank
                .entry((set, r.source_shape.clone()))
                .or_insert_with(|| inline(sets[set], r.source_shape.as_ref()))
                .clone(),
            other => text_of(other),
        };
        let index = match shape_index.get(&shape) {
            Some(i) => *i,
            None => {
                let i = shapes.len() as u32;
                shapes.push(shape.clone());
                shape_index.insert(shape, i);
                by_shape.push([0; 3]);
                i
            }
        };
        by_shape[index as usize][set] += 1;
        if !compared {
            return;
        }
        let fields = [
            text_of(&r.focus_node),
            r.result_path.as_deref().map(path_text).unwrap_or_default(),
            r.value.as_ref().map(text_of).unwrap_or_default(),
            shapes[index as usize].clone(),
            text_of(&r.source_constraint_component),
            text_of(r.severity),
        ];
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        fields.hash(&mut hasher);
        let key = hasher.finish();
        if found.len() >= RESULTS_COMPARED && !found.contains_key(&key) {
            compared = false;
            found = HashMap::new();
            return;
        }
        let entry = found.entry(key).or_insert((0, index));
        if set == 0 {
            entry.0 -= 1;
        } else {
            entry.0 += 1;
            // Before is done: an occurrence past before's count of it is new.
            if entry.0 > 0 {
                new += 1;
                by_shape[index as usize][2] += 1;
                if listed.len() < NEW_LISTED {
                    let [focus, path, value, shape, component, severity] = fields;
                    let mut one = json!({ "focus": focus, "shape": shape, "component": component, "severity": severity });
                    if !path.is_empty() {
                        one["path"] = json!(path);
                    }
                    if !value.is_empty() {
                        one["value"] = json!(value);
                    }
                    listed.push(one);
                }
            }
        }
    });
    let ms = started.elapsed().as_millis() as u64;
    if let Err(e) = outcome {
        return json!({ "status": "failed", "error": e.to_string(), "ms": ms });
    }
    let gone: u64 = found.values().map(|(k, _)| (-k).max(0) as u64).sum();
    let mut order: Vec<usize> = (0..shapes.len()).collect();
    let mut answer = json!({
        "status": "done",
        "before": counts[0],
        "after": counts[1],
        "compared": compared,
        "ms": ms,
    });
    if compared {
        order.retain(|i| by_shape[*i][2] > 0);
        order.sort_by(|a, b| {
            by_shape[*b][2]
                .cmp(&by_shape[*a][2])
                .then(shapes[*a].cmp(&shapes[*b]))
        });
        answer["new"] = json!(new);
        answer["gone"] = json!(gone);
        answer["newByShape"] = json!(
            order
                .iter()
                .take(NEW_LISTED)
                .map(|i| json!({ "shape": shapes[*i], "results": by_shape[*i][2] }))
                .collect::<Vec<_>>()
        );
        answer["newResults"] = json!(listed);
    } else {
        // Too many to tell apart: the shapes with more results than before.
        let more = |i: &usize| by_shape[*i][1].saturating_sub(by_shape[*i][0]);
        order.retain(|i| more(i) > 0);
        order.sort_by(|a, b| more(b).cmp(&more(a)).then(shapes[*a].cmp(&shapes[*b])));
        answer["moreByShape"] = json!(order.iter().take(NEW_LISTED)
            .map(|i| json!({ "shape": shapes[*i], "before": by_shape[*i][0], "after": by_shape[*i][1] }))
            .collect::<Vec<_>>());
    }
    answer
}

/// A validation for the pull request: a short Markdown comment. It gives counts, by shape;
/// with `with_results` (the binding's `reviewResults`), also the first new results, whose
/// focus nodes and values are data.
fn validation_text(dataset: &str, v: &Value, with_results: bool) -> String {
    if v["status"] != "done" {
        return format!(
            "TripleTrail could not validate dataset {} with the proposed shapes: {}",
            code(dataset),
            code(v["error"].as_str().unwrap_or("unknown error"))
        );
    }
    let mut out = format!(
        "TripleTrail validated all of dataset {} with the proposed shapes: {} results ({} with the \
         shapes in use)",
        code(dataset),
        v["after"],
        v["before"],
    );
    if v["compared"] == false {
        out.push_str(", too many to tell which are new.\n");
        let shapes: Vec<&Value> = v["moreByShape"].as_array().into_iter().flatten().collect();
        if !shapes.is_empty() {
            out.push_str("\nShapes with more results:\n");
            for s in shapes.iter().take(NAMED) {
                out.push_str(&format!(
                    "- {}: {} (was {})\n",
                    code(s["shape"].as_str().unwrap_or_default()),
                    s["after"],
                    s["before"]
                ));
            }
        }
        return out;
    }
    out.push_str(&format!(", {} new, {} gone.\n", v["new"], v["gone"]));
    let shapes: Vec<&Value> = v["newByShape"].as_array().into_iter().flatten().collect();
    if !shapes.is_empty() {
        out.push_str("\nNew results by shape:\n");
        for s in shapes.iter().take(NAMED) {
            out.push_str(&format!(
                "- {}: {}\n",
                code(s["shape"].as_str().unwrap_or_default()),
                s["results"]
            ));
        }
    }
    let first: Vec<&Value> = v["newResults"].as_array().into_iter().flatten().collect();
    if with_results && !first.is_empty() {
        out.push_str("\nFirst new results:\n");
        for r in first.iter().take(NAMED) {
            let field = |k: &str| r[k].as_str().map(code).unwrap_or_else(|| "-".to_owned());
            out.push_str(&format!(
                "- {} {} {} ({})\n",
                field("focus"),
                field("path"),
                field("value"),
                field("component")
            ));
        }
    }
    out
}

/// What `files` in place of the dataset's own set change for each reader (the node shapes it
/// would read, and their settings and rows, by key or path), and in each profile (the node
/// shapes in it). Readers and profiles without a change are left out.
fn effects(
    state: &State,
    entry: &Entry,
    files: &[SetFile],
) -> Result<(Value, Graph, Graph), HttpError> {
    let view = crate::View {
        graphs: None,
        rows: None,
    };
    let mut readers = serde_json::Map::new();
    let mut before_all = None;
    let mut after_all = None;
    for reader in Reader::ALL {
        let before = local::projected(state, entry, reader, &view)?;
        let after = local::projected_with(state, entry, reader, &view, files)?;
        let d = diff(&before, &after);
        if !d.is_empty() {
            readers.insert(reader.name().to_owned(), Value::Object(d));
        }
        if reader == Reader::Validation {
            before_all = Some(before);
            after_all = Some(after);
        }
    }
    // Profiles: from the validation reader's projection, which keeps every node shape that is
    // not hidden from it.
    let (before, after) = (
        before_all.unwrap_or_default(),
        after_all.unwrap_or_default(),
    );
    let in_profile = |g: &Graph| -> BTreeMap<String, BTreeSet<String>> {
        let mut by: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for t in g.triples_for_predicate(&tts("inProfile")) {
            by.entry(term(t.object))
                .or_default()
                .insert(term(t.subject.into()));
        }
        by
    };
    let (b, a) = (in_profile(&before), in_profile(&after));
    let mut profiles = serde_json::Map::new();
    for p in b.keys().chain(a.keys()).collect::<BTreeSet<_>>() {
        let empty = BTreeSet::new();
        let (old, new) = (b.get(p).unwrap_or(&empty), a.get(p).unwrap_or(&empty));
        let added: Vec<&String> = new.difference(old).collect();
        let removed: Vec<&String> = old.difference(new).collect();
        if !added.is_empty() || !removed.is_empty() {
            profiles.insert(p.clone(), json!({"added": added, "removed": removed}));
        }
    }
    Ok((
        json!({ "readers": readers, "profiles": profiles }),
        before,
        after,
    ))
}

/// A term as text, a blank node by what it holds (so that two graphs compare).
fn term(t: TermRef<'_>) -> String {
    match t {
        TermRef::NamedNode(n) => n.as_str().to_owned(),
        other => other.to_string(),
    }
}

/// A term as text, a blank node written out with its triples (sorted), within `g`.
/// A blank node already on the path is written `[ … ]`, and one term writes at most
/// [`INLINE_STEPS`] blank nodes, so blank nodes that point at each other take bounded work.
fn inline(g: &Graph, t: TermRef<'_>) -> String {
    let mut path = Vec::new();
    let mut steps = INLINE_STEPS;
    inline_within(g, t, &mut path, &mut steps)
}

/// Blank nodes one written term goes through, at most.
const INLINE_STEPS: usize = 2_000;

fn inline_within(
    g: &Graph,
    t: TermRef<'_>,
    path: &mut Vec<tripletrail::oxigraph::model::BlankNode>,
    steps: &mut usize,
) -> String {
    match t {
        TermRef::BlankNode(b) => {
            if *steps == 0 || path.iter().any(|p| p.as_ref() == b) {
                return "[ … ]".to_owned();
            }
            *steps -= 1;
            path.push(b.into_owned());
            let mut parts: Vec<String> = g
                .triples_for_subject(b)
                .map(|t| {
                    format!(
                        "{} {}",
                        t.predicate,
                        inline_within(g, t.object, path, steps)
                    )
                })
                .collect();
            path.pop();
            parts.sort();
            format!("[ {} ]", parts.join(" ; "))
        }
        other => other.to_string(),
    }
}

/// A node shape's settings (its triples but its property shapes), and its property shapes by
/// key (`tts:key`, `rd:key`, else the path), each written out.
fn signature(g: &Graph, node: NamedOrBlankNodeRef<'_>) -> (String, BTreeMap<String, String>) {
    let property = sh("property");
    let mut settings: Vec<String> = g
        .triples_for_subject(node)
        .filter(|t| t.predicate != property.as_ref())
        .map(|t| format!("{} {}", t.predicate, inline(g, t.object)))
        .collect();
    settings.sort();
    let mut rows = BTreeMap::new();
    for p in g.objects_for_subject_predicate(node, &property) {
        let p_node = match p {
            TermRef::NamedNode(n) => NamedOrBlankNodeRef::from(n),
            TermRef::BlankNode(b) => b.into(),
            _ => continue,
        };
        let key = [
            tts("key"),
            NamedNode::new_unchecked("https://rinf-describe.matdata.eu/ns#key"),
        ]
        .iter()
        .find_map(|k| match g.object_for_subject_predicate(p_node, k) {
            Some(TermRef::Literal(l)) => Some(l.value().to_owned()),
            _ => None,
        })
        .or_else(|| {
            g.object_for_subject_predicate(p_node, &sh("path"))
                .map(|path| inline(g, path))
        })
        .unwrap_or_else(|| inline(g, p));
        let mut text = inline(g, p);
        // Two rows of one key (one per class, say) compare together.
        if let Some(before) = rows.get(&key) {
            let mut both = [std::mem::take(&mut text), String::clone(before)];
            both.sort();
            text = both.join(" | ");
        }
        rows.insert(key, text);
    }
    (settings.join(" ; "), rows)
}

/// What changes from `before` to `after`: node shapes added and removed, and for those in
/// both, whether their settings changed and which rows were added, removed or changed.
fn diff(before: &Graph, after: &Graph) -> serde_json::Map<String, Value> {
    let nodes = |g: &Graph| -> BTreeMap<String, NamedOrBlankNode> {
        g.subjects_for_predicate_object(rdf::TYPE, &sh("NodeShape"))
            .filter(|n| matches!(n, NamedOrBlankNodeRef::NamedNode(_)))
            .map(|n| (term(n.into()), n.into_owned()))
            .collect()
    };
    let (b, a) = (nodes(before), nodes(after));
    let mut out = serde_json::Map::new();
    let added: Vec<&String> = a.keys().filter(|k| !b.contains_key(*k)).collect();
    let removed: Vec<&String> = b.keys().filter(|k| !a.contains_key(*k)).collect();
    let mut changed = Vec::new();
    for (k, old) in &b {
        let Some(new) = a.get(k) else { continue };
        let (os, orows) = signature(before, old.as_ref());
        let (ns, nrows) = signature(after, new.as_ref());
        let rows_added: Vec<&String> = nrows.keys().filter(|r| !orows.contains_key(*r)).collect();
        let rows_removed: Vec<&String> = orows.keys().filter(|r| !nrows.contains_key(*r)).collect();
        let rows_changed: Vec<&String> = orows
            .iter()
            .filter(|(r, text)| nrows.get(*r).is_some_and(|n| n != *text))
            .map(|(r, _)| r)
            .collect();
        if os != ns
            || !rows_added.is_empty()
            || !rows_removed.is_empty()
            || !rows_changed.is_empty()
        {
            let mut c = json!({ "shape": k });
            if os != ns {
                c["settings"] = json!(true);
            }
            for (name, list) in [
                ("rowsAdded", rows_added),
                ("rowsRemoved", rows_removed),
                ("rowsChanged", rows_changed),
            ] {
                if !list.is_empty() {
                    c[name] = json!(list);
                }
            }
            changed.push(c);
        }
    }
    for (name, list) in [("added", json!(added)), ("removed", json!(removed))] {
        if list.as_array().is_some_and(|l| !l.is_empty()) {
            out.insert(name.to_owned(), list);
        }
    }
    if !changed.is_empty() {
        out.insert("changed".to_owned(), Value::Array(changed));
    }
    out
}

/// Most characters of the "What changes" part of a pull request's description (GitHub takes
/// 65,536 for the whole description); the whole `effects` stay in the proposal.
const EFFECTS_TEXT: usize = 30_000;
/// Shapes, and rows of a shape, a line names before "and N more".
const NAMED: usize = 10;

/// `s` as inline code in Markdown: backticks and control characters dropped, cut to 200
/// characters, so that a name cannot start a heading, mention someone or link.
fn code(s: &str) -> String {
    let clean: String = s
        .chars()
        .filter(|c| *c != '`' && !c.is_control())
        .take(200)
        .collect();
    format!("`{clean}`")
}

/// The first [`NAMED`] of a list of names, as inline code, then "and N more".
fn names(v: &Value) -> String {
    let all: Vec<&str> = v
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut out: Vec<String> = all.iter().take(NAMED).map(|s| code(s)).collect();
    if all.len() > NAMED {
        out.push(format!("and {} more", all.len() - NAMED));
    }
    out.join(", ")
}

/// The effects for a pull request's description: a line per reader (readers with the same
/// effect on one line) and per profile, at most [`EFFECTS_TEXT`] characters.
fn effects_text(effects: &Value) -> String {
    let mut lines: Vec<String> = Vec::new();
    // Readers with the same effect together, in the order they come.
    let mut groups: Vec<(Vec<&str>, &Value)> = Vec::new();
    for (reader, d) in effects["readers"].as_object().into_iter().flatten() {
        match groups.iter_mut().find(|(_, other)| *other == d) {
            Some((readers, _)) => readers.push(reader),
            None => groups.push((vec![reader], d)),
        }
    }
    for (readers, d) in groups {
        let mut parts = Vec::new();
        if d["added"].is_array() {
            parts.push(format!("adds {}", names(&d["added"])));
        }
        if d["removed"].is_array() {
            parts.push(format!("drops {}", names(&d["removed"])));
        }
        let changed: Vec<&Value> = d["changed"].as_array().into_iter().flatten().collect();
        for c in changed.iter().take(NAMED) {
            let mut what = Vec::new();
            if c["settings"] == true {
                what.push("settings".to_owned());
            }
            for (key, verb) in [
                ("rowsAdded", "rows added"),
                ("rowsRemoved", "rows removed"),
                ("rowsChanged", "rows changed"),
            ] {
                if c[key].is_array() {
                    what.push(format!("{verb} {}", names(&c[key])));
                }
            }
            parts.push(format!(
                "changes {} ({})",
                code(c["shape"].as_str().unwrap_or_default()),
                what.join("; ")
            ));
        }
        if changed.len() > NAMED {
            parts.push(format!("and changes {} more shapes", changed.len() - NAMED));
        }
        lines.push(format!("- {}: {}", readers.join(", "), parts.join("; ")));
    }
    for (profile, d) in effects["profiles"].as_object().into_iter().flatten() {
        let mut parts = Vec::new();
        if d["added"].as_array().is_some_and(|a| !a.is_empty()) {
            parts.push(format!("adds {}", names(&d["added"])));
        }
        if d["removed"].as_array().is_some_and(|a| !a.is_empty()) {
            parts.push(format!("drops {}", names(&d["removed"])));
        }
        lines.push(format!("- profile {}: {}", code(profile), parts.join("; ")));
    }
    if lines.is_empty() {
        lines.push("- no reader reads anything else".to_owned());
    }
    let mut out = String::from("\nWhat changes:\n");
    for (i, line) in lines.iter().enumerate() {
        if out.len() + line.len() + 1 > EFFECTS_TEXT {
            out.push_str(&format!(
                "- and {} more lines: see the proposal's effects in TripleTrail\n",
                lines.len() - i
            ));
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The branch a binding's ref names; `None` for `HEAD` (the repository's default branch).
/// A tag (or another full ref) cannot take a pull request: refused before the forge is asked.
fn branch_of(reference: &str) -> Result<Option<String>, HttpError> {
    if reference == "HEAD" {
        Ok(None)
    } else if let Some(branch) = reference.strip_prefix("refs/heads/") {
        Ok(Some(branch.to_owned()))
    } else if reference.starts_with("refs/") {
        Err(HttpError(
            409,
            format!(
                "The shapes are bound to {reference}, not a branch: a proposal is a pull \
                 request against a branch"
            ),
        ))
    } else {
        Ok(Some(reference.to_owned()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    GitHub,
    GitLab,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::GitHub => "github",
            Kind::GitLab => "gitlab",
        }
    }
}

/// Where the binding's repository is hosted, and its REST API.
#[derive(Clone)]
struct Forge {
    kind: Kind,
    /// The API's base URL, without a trailing `/`.
    api: String,
    /// `owner/repo` (GitHub), or the project's full path (GitLab).
    project: String,
}

struct Change<'a> {
    base_commit: &'a str,
    base: &'a str,
    branch: &'a str,
    title: &'a str,
    message: &'a str,
    body: &'a str,
    /// Path in the repository, text, whether it was there.
    files: &'a [(String, String, bool)],
}

/// A link from the forge's answer, kept only when it is http(s): pages link it.
fn web_url(v: &Value) -> Option<String> {
    v.as_str()
        .filter(|u| {
            let lower = u.to_ascii_lowercase();
            lower.starts_with("https://") || lower.starts_with("http://")
        })
        .map(str::to_owned)
}

struct Opened {
    number: Option<u64>,
    url: Option<String>,
}

impl Forge {
    /// From the binding: `forge`, `api` and `project` when given, else read from its URL
    /// (`https://github.com/owner/repo`, `https://gitlab.example.org/group/repo`).
    fn of(binding: &Binding) -> Result<Self, HttpError> {
        let url = binding.url.trim_end_matches('/');
        let url = url.strip_suffix(".git").unwrap_or(url);
        let (host, path) = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .and_then(|rest| rest.split_once('/'))
            .map_or((None, None), |(h, p)| (Some(h), Some(p)));
        let unknown = || {
            HttpError(
                409,
                format!(
                    "Proposals open pull requests on GitHub or GitLab: give the binding forge \
                     (github or gitlab) for {}",
                    binding.url
                ),
            )
        };
        let kind = match binding.forge.as_deref() {
            Some("github") => Kind::GitHub,
            Some("gitlab") => Kind::GitLab,
            Some(other) => {
                return Err(HttpError::bad_request(format!(
                    "forge is github or gitlab, not {other:?}"
                )));
            }
            None => match host {
                Some("github.com") => Kind::GitHub,
                Some(h) if h.split('.').any(|part| part == "gitlab") => Kind::GitLab,
                _ => return Err(unknown()),
            },
        };
        let api = match (&binding.api, kind, host) {
            (Some(api), _, _) => api.trim_end_matches('/').to_owned(),
            (None, Kind::GitHub, Some("github.com")) => "https://api.github.com".to_owned(),
            (None, Kind::GitHub, Some(h)) => format!("https://{h}/api/v3"),
            (None, Kind::GitLab, Some(h)) => format!("https://{h}/api/v4"),
            (None, _, None) => {
                return Err(HttpError(
                    409,
                    format!("Give the binding api: the forge's API for {}", binding.url),
                ));
            }
        };
        let project = match (&binding.project, path) {
            (Some(p), _) => p.trim_matches('/').to_owned(),
            (None, Some(p)) => p.trim_matches('/').to_owned(),
            (None, None) => {
                return Err(HttpError(
                    409,
                    format!(
                        "Give the binding project: the repository on the forge for {}",
                        binding.url
                    ),
                ));
            }
        };
        if !local::safe_for_tokens(&api) {
            return Err(HttpError::bad_request(format!(
                "The forge's api is only called over https (or plain http to this machine), \
                 not {api}"
            )));
        }
        if project.is_empty()
            || project
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '?' | '#' | '%'))
        {
            return Err(HttpError::bad_request(format!(
                "Invalid project {project:?} for the forge's API"
            )));
        }
        if kind == Kind::GitHub && project.split('/').count() != 2 {
            return Err(HttpError::bad_request(format!(
                "A GitHub project is owner/repo, not {project:?}"
            )));
        }
        Ok(Self { kind, api, project })
    }

    /// The project as a path segment of GitLab's API.
    fn gitlab_id(&self) -> String {
        self.project.replace('/', "%2F")
    }

    fn call(
        &self,
        token: &str,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, HttpError> {
        self.call_within(token, method, path, body, Duration::from_secs(30))
    }

    fn call_within(
        &self,
        token: &str,
        method: &str,
        path: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<Value, HttpError> {
        let agent = ureq::AgentBuilder::new()
            .timeout(timeout)
            .redirects(0)
            .build();
        let mut request = agent
            .request(method, &format!("{}{path}", self.api))
            .set("User-Agent", "TripleTrail")
            .set("Accept", "application/json");
        request = match self.kind {
            Kind::GitHub => request
                .set("Authorization", &format!("Bearer {token}"))
                .set("X-GitHub-Api-Version", "2022-11-28"),
            Kind::GitLab => request.set("PRIVATE-TOKEN", token),
        };
        let answer = match body {
            Some(b) => request
                .set("Content-Type", "application/json")
                .send_string(&b.to_string()),
            None => request.call(),
        };
        let what = format!("{} {method} {path}", self.kind.name());
        match answer {
            Ok(r) => {
                let text = r.into_string().map_err(|e| HttpError(502, e.to_string()))?;
                serde_json::from_str(&text)
                    .map_err(|e| HttpError(502, format!("{what}: not JSON: {e}")))
            }
            Err(ureq::Error::Status(code, r)) => Err(HttpError(
                502,
                format!(
                    "{what}: {code} {}",
                    r.into_string()
                        .unwrap_or_default()
                        .chars()
                        .take(300)
                        .collect::<String>()
                ),
            )),
            Err(e) => Err(HttpError(502, format!("{what}: {e}"))),
        }
    }

    /// Comments on the pull (merge) request `number`.
    fn comment(&self, token: &str, number: u64, body: &str) -> Result<(), HttpError> {
        let path = match self.kind {
            Kind::GitHub => format!("/repos/{}/issues/{number}/comments", self.project),
            Kind::GitLab => format!(
                "/projects/{}/merge_requests/{number}/notes",
                self.gitlab_id()
            ),
        };
        self.call(token, "POST", &path, Some(&json!({ "body": body })))
            .map(|_| ())
    }

    /// Whether pull (merge) request `number` is `open`, `merged` or `closed`.
    fn state(&self, token: &str, number: u64) -> Result<&'static str, HttpError> {
        match self.kind {
            Kind::GitHub => {
                let pull = self.call_within(
                    token,
                    "GET",
                    &format!("/repos/{}/pulls/{number}", self.project),
                    None,
                    STATE_TIMEOUT,
                )?;
                Ok(if pull["merged"] == true || pull["merged_at"].is_string() {
                    "merged"
                } else if pull["state"] == "closed" {
                    "closed"
                } else {
                    "open"
                })
            }
            Kind::GitLab => {
                let request = self.call_within(
                    token,
                    "GET",
                    &format!("/projects/{}/merge_requests/{number}", self.gitlab_id()),
                    None,
                    STATE_TIMEOUT,
                )?;
                Ok(match request["state"].as_str() {
                    Some("merged") => "merged",
                    Some("closed" | "locked") => "closed",
                    _ => "open",
                })
            }
        }
    }

    fn default_branch(&self, token: &str) -> Result<String, HttpError> {
        let path = match self.kind {
            Kind::GitHub => format!("/repos/{}", self.project),
            Kind::GitLab => format!("/projects/{}", self.gitlab_id()),
        };
        self.call(token, "GET", &path, None)?["default_branch"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| HttpError(502, format!("{path}: no default_branch")))
    }

    /// Commits the files on a new branch from the base commit, and opens the request.
    fn open(&self, token: &str, change: &Change<'_>) -> Result<Opened, HttpError> {
        match self.kind {
            Kind::GitHub => {
                let repo = format!("/repos/{}", self.project);
                let commit = self.call(
                    token,
                    "GET",
                    &format!("{repo}/git/commits/{}", change.base_commit),
                    None,
                )?;
                let base_tree = commit["tree"]["sha"]
                    .as_str()
                    .ok_or_else(|| HttpError(502, format!("{repo}: commit without a tree")))?;
                let entries: Vec<Value> = change
                    .files
                    .iter()
                    .map(|(path, text, _)| {
                        json!({"path": path, "mode": "100644", "type": "blob", "content": text})
                    })
                    .collect();
                let tree = self.call(
                    token,
                    "POST",
                    &format!("{repo}/git/trees"),
                    Some(&json!({"base_tree": base_tree, "tree": entries})),
                )?;
                let tree = tree["sha"]
                    .as_str()
                    .ok_or_else(|| HttpError(502, format!("{repo}: tree without a sha")))?;
                let made = self.call(
                    token,
                    "POST",
                    &format!("{repo}/git/commits"),
                    Some(&json!({
                        "message": change.message,
                        "tree": tree,
                        "parents": [change.base_commit],
                    })),
                )?;
                let sha = made["sha"]
                    .as_str()
                    .ok_or_else(|| HttpError(502, format!("{repo}: commit without a sha")))?;
                self.call(
                    token,
                    "POST",
                    &format!("{repo}/git/refs"),
                    Some(&json!({"ref": format!("refs/heads/{}", change.branch), "sha": sha})),
                )?;
                let pull = self.call(
                    token,
                    "POST",
                    &format!("{repo}/pulls"),
                    Some(&json!({
                        "title": change.title,
                        "head": change.branch,
                        "base": change.base,
                        "body": change.body,
                    })),
                )?;
                Ok(Opened {
                    number: pull["number"].as_u64(),
                    url: web_url(&pull["html_url"]),
                })
            }
            Kind::GitLab => {
                let project = format!("/projects/{}", self.gitlab_id());
                let actions: Vec<Value> = change
                    .files
                    .iter()
                    .map(|(path, text, existed)| {
                        json!({
                            "action": if *existed { "update" } else { "create" },
                            "file_path": path,
                            "content": text,
                        })
                    })
                    .collect();
                self.call(
                    token,
                    "POST",
                    &format!("{project}/repository/commits"),
                    Some(&json!({
                        "branch": change.branch,
                        "start_sha": change.base_commit,
                        "commit_message": change.message,
                        "actions": actions,
                    })),
                )?;
                let request = self.call(
                    token,
                    "POST",
                    &format!("{project}/merge_requests"),
                    Some(&json!({
                        "source_branch": change.branch,
                        "target_branch": change.base,
                        "title": change.title,
                        "description": change.body,
                        "remove_source_branch": true,
                    })),
                )?;
                Ok(Opened {
                    number: request["iid"].as_u64(),
                    url: web_url(&request["web_url"]),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(url: &str) -> Binding {
        Binding::from_json(&json!({ "url": url })).unwrap()
    }

    #[test]
    fn the_forge_is_read_from_the_url() {
        let f = Forge::of(&binding("https://github.com/Matdata-eu/shapes.git")).unwrap();
        assert_eq!(f.kind, Kind::GitHub);
        assert_eq!(f.api, "https://api.github.com");
        assert_eq!(f.project, "Matdata-eu/shapes");
        let f = Forge::of(&binding("https://gitlab.example.org/rail/assets/shapes")).unwrap();
        assert_eq!(f.kind, Kind::GitLab);
        assert_eq!(f.api, "https://gitlab.example.org/api/v4");
        assert_eq!(f.gitlab_id(), "rail%2Fassets%2Fshapes");
        assert_eq!(
            Forge::of(&binding("https://git.example.org/x/y"))
                .err()
                .unwrap()
                .0,
            409
        );
        let b = Binding::from_json(&json!({
            "url": "https://git.example.org/x/y", "forge": "github",
            "api": "https://git.example.org/api/v3/"
        }))
        .unwrap();
        let f = Forge::of(&b).unwrap();
        assert_eq!(
            (f.kind, f.api.as_str()),
            (Kind::GitHub, "https://git.example.org/api/v3")
        );
    }

    fn graph(ttl: &str) -> Graph {
        let mut g = Graph::new();
        for t in tripletrail::oxigraph::io::RdfParser::from_format(
            tripletrail::oxigraph::io::RdfFormat::Turtle,
        )
        .for_reader(ttl.as_bytes())
        {
            g.insert(&tripletrail::oxigraph::model::Triple::from(t.unwrap()));
        }
        g
    }

    #[test]
    fn a_diff_names_the_rows_by_key_or_path() {
        let p = "@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix tts: <http://w3id.org/tripletrail/shapes#> . @prefix ex: <http://example.com/> .\n";
        let before = graph(&format!(
            "{p} ex:A a sh:NodeShape ; sh:property [ sh:path ex:x ; sh:order 1 ] , [ sh:path ex:y ; tts:key \"why\" ] .
             ex:B a sh:NodeShape ."
        ));
        let after = graph(&format!(
            "{p} ex:A a sh:NodeShape ; sh:property [ sh:path ex:x ; sh:order 2 ] , [ sh:path ex:z ] .
             ex:C a sh:NodeShape ."
        ));
        let d = Value::Object(diff(&before, &after));
        assert_eq!(d["added"], json!(["http://example.com/C"]));
        assert_eq!(d["removed"], json!(["http://example.com/B"]));
        let a = &d["changed"][0];
        assert_eq!(a["shape"], "http://example.com/A");
        assert!(a.get("settings").is_none());
        assert_eq!(a["rowsAdded"], json!(["<http://example.com/z>"]));
        assert_eq!(a["rowsRemoved"], json!(["why"]));
        assert_eq!(a["rowsChanged"], json!(["<http://example.com/x>"]));
        assert!(diff(&before, &before).is_empty());
    }

    #[test]
    fn blank_nodes_that_point_at_each_other_take_bounded_work() {
        // Shape A's row reaches 12 blank nodes that each link to all 12.
        let mut ttl = String::from(
            "@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://example.com/> .\n\
             ex:A a sh:NodeShape ; sh:property _:b0 .\n",
        );
        for i in 0..12 {
            for j in 0..12 {
                ttl.push_str(&format!("_:b{i} ex:p _:b{j} .\n"));
            }
        }
        let g = graph(&ttl);
        let started = std::time::Instant::now();
        let d = diff(&g, &graph(&ttl.replace("ex:p _:b0", "ex:q _:b0")));
        assert!(started.elapsed().as_secs() < 5, "{:?}", started.elapsed());
        assert_eq!(d["changed"][0]["shape"], "http://example.com/A");
        assert!(diff(&g, &g).is_empty());
    }

    #[test]
    fn a_pull_request_description_stays_short_and_plain() {
        // 300 shapes with 20 changed rows each, for 8 readers that differ, and 3 that agree.
        let shape = |s: usize, reader: usize| {
            json!({
                "shape": format!("http://example.com/S{s}"),
                "rowsChanged": (0..20).map(|r| format!("row-{r}-{reader}")).collect::<Vec<_>>(),
            })
        };
        let mut readers = serde_json::Map::new();
        for reader in 0..11 {
            let same = reader.min(8);
            readers.insert(
                format!("reader{reader}"),
                json!({ "changed": (0..300).map(|s| shape(s, same)).collect::<Vec<_>>() }),
            );
        }
        let text = effects_text(&json!({ "readers": readers, "profiles": {} }));
        assert!(text.len() < EFFECTS_TEXT, "{}", text.len());
        // Readers 8, 9 and 10 agree: one line for the three.
        assert_eq!(
            text.lines().filter(|l| l.starts_with("- ")).count(),
            9,
            "{text}"
        );
        assert!(
            text.lines()
                .any(|l| ["reader8", "reader9", "reader10"].iter().all(|r| l
                    .split(':')
                    .next()
                    .unwrap()
                    .contains(r))),
            "{text}"
        );
        assert!(text.contains("and changes 290 more shapes"), "{text}");
        assert!(text.contains("and 10 more"), "{text}");

        let evil = "x`\n\n# Title\n@someone [link](http://evil)";
        let text =
            effects_text(&json!({ "readers": { "map": { "added": [evil] } }, "profiles": {} }));
        assert_eq!(
            text,
            "\nWhat changes:\n- map: adds `x# Title@someone [link](http://evil)`\n"
        );
        assert_eq!(code(&"y".repeat(500)).len(), 202);
    }

    #[test]
    fn the_base_branch_is_the_bound_one() {
        assert_eq!(branch_of("main").unwrap().as_deref(), Some("main"));
        assert_eq!(branch_of("refs/heads/rel").unwrap().as_deref(), Some("rel"));
        assert_eq!(branch_of("HEAD").unwrap(), None);
        assert_eq!(branch_of("refs/tags/v2").err().unwrap().0, 409);
    }
}
