//! Reviewed shape changes (#145, TripleTrail Enterprise): the shapes editor's changes on a set
//! bound to git become a pull request (GitHub) or merge request (GitLab), made through the
//! forge's REST API, here a mock of it.
#![cfg(all(feature = "enterprise", feature = "git"))]

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail_enterprise::license::{Date, License};
use tripletrail_server::{Options, Server};

const TOKEN: &str = "s3cret";

fn temp_dir(name: &str) -> tripletrail_testdir::TestDir {
    tripletrail_testdir::tempdir(&format!("review-{name}"))
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn licence(features: &[&str]) -> License {
    License {
        id: "2026-145".into(),
        kid: "test".into(),
        customer: "Example Rail AS".into(),
        issued: Date::today().plus_days(-10),
        expires: Date::today().plus_days(300),
        features: features.iter().map(|f| (*f).to_owned()).collect(),
        max_nodes: None,
        max_cores: None,
        note: None,
    }
}

/// One request the mock forge got: method, path, the token header, the JSON body.
type Seen = Arc<Mutex<Vec<(String, String, String, Value)>>>;

/// A mock of GitHub's and GitLab's REST API: answers what a proposal asks, and keeps the
/// requests.
fn forge() -> (String, Seen) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr().to_ip().unwrap());
    let seen: Seen = Arc::default();
    let keep = seen.clone();
    std::thread::spawn(move || {
        for mut request in server.incoming_requests() {
            let mut body = String::new();
            let _ = request.as_reader().read_to_string(&mut body);
            let header = |name: &'static str| -> String {
                request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv(name))
                    .map(|h| h.value.as_str().to_owned())
                    .unwrap_or_default()
            };
            let token = format!("{}{}", header("Authorization"), header("PRIVATE-TOKEN"));
            let method = request.method().as_str().to_owned();
            let path = request.url().to_owned();
            let body: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            // A forge that makes the branch and then refuses the pull request.
            let refused = path.ends_with("/pulls") && body["title"] == "Refused";
            let answer = match (method.as_str(), path.as_str()) {
                _ if refused => json!({"message": "Resource not accessible by integration"}),
                ("GET", p) if p.contains("/git/commits/") => json!({"tree": {"sha": "t0"}}),
                // Pull request 7 was merged; GitLab's merge request 3 is still open.
                ("GET", p) if p.ends_with("/pulls/7") => {
                    json!({"state": "closed", "merged": true})
                }
                ("GET", p) if p.ends_with("/merge_requests/3") => json!({"state": "opened"}),
                ("POST", p) if p.ends_with("/git/trees") => json!({"sha": "t1"}),
                ("POST", p) if p.ends_with("/git/commits") => json!({"sha": "c1"}),
                ("POST", p) if p.ends_with("/git/refs") => json!({"ref": "x"}),
                ("POST", p) if p.ends_with("/pulls") => {
                    json!({"number": 7, "html_url": "https://github.example/o/r/pull/7"})
                }
                ("GET", p) if p.starts_with("/projects/") => json!({"default_branch": "main"}),
                ("POST", p) if p.ends_with("/repository/commits") => json!({"id": "c1"}),
                ("POST", p) if p.ends_with("/comments") || p.ends_with("/notes") => {
                    json!({"id": 1})
                }
                ("POST", p) if p.ends_with("/merge_requests") => {
                    json!({"iid": 3, "web_url": "https://gitlab.example/g/s/r/-/merge_requests/3"})
                }
                _ => json!({"message": "Not Found"}),
            };
            keep.lock().unwrap().push((method, path, token, body));
            let code = if refused { 403 } else { 200 };
            let _ = request.respond(
                tiny_http::Response::from_string(answer.to_string()).with_status_code(code),
            );
        }
    });
    (base, seen)
}

struct Running {
    base: String,
}

impl Running {
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        let request = ureq::request(method, &format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {TOKEN}"))
            .set("Content-Type", "application/json");
        let answer = match body {
            Some(b) => request.send_string(&b.to_string()),
            None => request.call(),
        };
        match answer {
            Ok(r) => (
                r.status(),
                serde_json::from_str(&r.into_string().unwrap()).unwrap_or(Value::Null),
            ),
            Err(ureq::Error::Status(code, r)) => (code, json!(r.into_string().unwrap_or_default())),
            Err(e) => panic!("{e}"),
        }
    }
}

fn start(dir: &Path, features: &[&str]) -> Running {
    start_with(
        dir,
        features,
        "<http://example.com/t1> a <http://example.com/Thing> .",
    )
}

/// As [`start`], with `data` (Turtle) in the dataset.
fn start_with(dir: &Path, features: &[&str], data: &str) -> Running {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), engine)],
        Options {
            threads: 4,
            admin_token: Some(TOKEN.to_owned()),
            location: Some(dir.join("data")),
            license: Ok(licence(features)),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    Running { base }
}

/// A repository with the set in `shapes/` (one file in a subfolder).
fn repository(dir: &Path) -> (PathBuf, String) {
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join("shapes/rail")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(
        repo.join("shapes/rail/thing.ttl"),
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix ex: <http://example.com/> .\n\
         @prefix tts: <http://w3id.org/tripletrail/shapes#> .\n\
         ex:ThingShape a sh:NodeShape ; sh:targetClass ex:Thing ; tts:color \"#ff0000\" .\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    (repo, commit)
}

fn change() -> Value {
    json!({
        "title": "Thing in green",
        "description": "Greener map.",
        "changes": [{ "shape": "http://example.com/ThingShape", "setting": "color", "value": "#00ff00" }],
    })
}

#[test]
fn a_change_set_becomes_a_pull_request_on_github() {
    let dir = temp_dir("github");
    let (repo, commit) = repository(&dir);
    let token = dir.join("token");
    std::fs::write(&token, "ghp_test\n").unwrap();
    let (api, seen) = forge();
    let s = start(&dir, &["review"]);

    // Not bound: no proposals.
    assert_eq!(
        s.call(
            "POST",
            "/$/datasets/kg/shapes/proposals?validate=false",
            Some(&change())
        )
        .0,
        409
    );
    let binding = json!({
        "url": format!("file://{}", repo.display()),
        "ref": "main",
        "path": "shapes",
        "reviewTokenFile": token.to_str().unwrap(),
        "forge": "github",
        "api": api,
        "project": "o/r",
    });
    let (code, bound) = s.call("PUT", "/$/datasets/kg/shapes/git", Some(&binding));
    assert_eq!(code, 200, "{bound}");
    // The editor still refuses to change a bound set itself.
    assert_eq!(
        s.call("POST", "/$/datasets/kg/shapes/edit", Some(&change()))
            .0,
        409
    );
    // A proposal needs a title, and a change.
    assert_eq!(
        s.call(
            "POST",
            "/$/datasets/kg/shapes/proposals",
            Some(&json!({"changes": change()["changes"]}))
        )
        .0,
        400
    );

    // A dry run: the files and what each reader reads differently, and no forge call.
    let mut hide = change();
    hide["changes"] = json!([{ "shape": "http://example.com/ThingShape", "setting": "hiddenFrom", "value": ["map"] }]);
    let (code, dry) = s.call(
        "POST",
        "/$/datasets/kg/shapes/proposals?dry=true",
        Some(&hide),
    );
    assert_eq!(code, 200, "{dry}");
    assert_eq!(dry["files"][0]["path"], "shapes/rail/thing.ttl");
    assert_eq!(dry["files"][0]["new"], false);
    assert_eq!(
        dry["effects"]["readers"]["map"]["removed"],
        json!(["http://example.com/ThingShape"])
    );
    assert_eq!(
        dry["effects"]["readers"]["describe"]["changed"][0]["settings"],
        true
    );
    assert!(seen.lock().unwrap().is_empty());

    let (code, proposal) = s.call(
        "POST",
        "/$/datasets/kg/shapes/proposals?validate=false",
        Some(&change()),
    );
    assert_eq!(code, 201, "{proposal}");
    assert_eq!(proposal["proposal"], 1);
    assert_eq!(
        proposal["effects"]["readers"]["map"]["changed"][0]["shape"],
        "http://example.com/ThingShape"
    );
    assert_eq!(proposal["url"], "https://github.example/o/r/pull/7");
    assert_eq!(proposal["number"], 7);
    assert_eq!(proposal["branch"], "tripletrail/kg-shapes-1");
    assert_eq!(proposal["base"], "main");
    assert_eq!(proposal["commit"], commit.as_str());
    assert_eq!(proposal["files"], json!(["shapes/rail/thing.ttl"]));
    assert_eq!(proposal["by"], "admin");

    let seen = seen.lock().unwrap().clone();
    let calls: Vec<String> = seen.iter().map(|(m, p, _, _)| format!("{m} {p}")).collect();
    assert_eq!(
        calls,
        [
            format!("GET /repos/o/r/git/commits/{commit}"),
            "POST /repos/o/r/git/trees".to_owned(),
            "POST /repos/o/r/git/commits".to_owned(),
            "POST /repos/o/r/git/refs".to_owned(),
            "POST /repos/o/r/pulls".to_owned(),
        ]
    );
    assert!(seen.iter().all(|(_, _, t, _)| t == "Bearer ghp_test"));
    // The file at its place in the repository, with the change, on the pulled commit.
    let tree = &seen[1].3;
    assert_eq!(tree["base_tree"], "t0");
    assert_eq!(tree["tree"][0]["path"], "shapes/rail/thing.ttl");
    let text = tree["tree"][0]["content"].as_str().unwrap();
    assert!(
        text.contains("#00ff00") && !text.contains("#ff0000"),
        "{text}"
    );
    assert_eq!(seen[2].3["parents"], json!([commit]));
    assert_eq!(seen[2].3["message"], "Thing in green\n\nGreener map.");
    assert_eq!(seen[3].3["ref"], "refs/heads/tripletrail/kg-shapes-1");
    assert_eq!(seen[4].3["head"], "tripletrail/kg-shapes-1");
    assert_eq!(seen[4].3["base"], "main");
    assert!(
        seen[4].3["body"]
            .as_str()
            .unwrap()
            .contains("`shapes/rail/thing.ttl`")
    );
    let body = seen[4].3["body"].as_str().unwrap();
    // Every reader reads the colour: one line for them all.
    assert!(
        body.lines().any(|l| l.starts_with("- ")
            && l.contains("map")
            && l.contains("describe")
            && l.ends_with(": changes `http://example.com/ThingShape` (settings)")),
        "{body}"
    );

    // Nothing changed on the server: the repository is the master.
    let (_, listed) = s.call("GET", "/$/datasets/kg/shapes/proposals", None);
    assert_eq!(listed["proposals"][0]["url"], proposal["url"]);
    assert_eq!(listed["licensed"], true);
    let (_, git) = s.call("GET", "/$/datasets/kg/shapes/git", None);
    assert_eq!(git["last"]["version"], 1);
}

#[test]
fn a_change_set_becomes_a_merge_request_on_gitlab() {
    let dir = temp_dir("gitlab");
    let (repo, commit) = repository(&dir);
    let token = dir.join("token");
    std::fs::write(&token, "glpat-test").unwrap();
    let (api, seen) = forge();
    let s = start(&dir, &["review"]);
    let binding = json!({
        "url": format!("file://{}", repo.display()),
        "path": "shapes",
        "reviewTokenFile": token.to_str().unwrap(),
        "forge": "gitlab",
        "api": api,
        "project": "g/s/r",
    });
    assert_eq!(
        s.call("PUT", "/$/datasets/kg/shapes/git", Some(&binding)).0,
        200
    );
    let (code, proposal) = s.call(
        "POST",
        "/$/datasets/kg/shapes/proposals?validate=false",
        Some(&change()),
    );
    assert_eq!(code, 201, "{proposal}");
    assert_eq!(
        proposal["url"],
        "https://gitlab.example/g/s/r/-/merge_requests/3"
    );
    // Bound to HEAD: the request goes to the project's default branch.
    assert_eq!(proposal["base"], "main");
    let got = seen.lock().unwrap().clone();
    let calls: Vec<String> = got.iter().map(|(m, p, _, _)| format!("{m} {p}")).collect();
    assert_eq!(
        calls,
        [
            "GET /projects/g%2Fs%2Fr",
            "POST /projects/g%2Fs%2Fr/repository/commits",
            "POST /projects/g%2Fs%2Fr/merge_requests",
        ]
    );
    assert!(got.iter().all(|(_, _, t, _)| t == "glpat-test"));
    let commits = &got[1].3;
    assert_eq!(commits["start_sha"], commit.as_str());
    assert_eq!(commits["branch"], "tripletrail/kg-shapes-1");
    assert_eq!(commits["actions"][0]["action"], "update");
    assert_eq!(commits["actions"][0]["file_path"], "shapes/rail/thing.ttl");
    assert_eq!(got[2].3["target_branch"], "main");
    // Still open on GitLab ("opened"), and checked.
    let asked = || {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|(m, p, _, _)| m == "GET" && p.ends_with("/merge_requests/3"))
            .count()
    };
    let (code, listed) = s.call("GET", "/$/datasets/kg/shapes/proposals?refresh=true", None);
    assert_eq!(code, 200, "{listed}");
    assert_eq!(listed["proposals"][0]["status"], "open", "{listed}");
    assert!(listed["proposals"][0]["checked"].is_string(), "{listed}");
    assert!(
        listed["proposals"][0].get("statusError").is_none(),
        "{listed}"
    );
    assert_eq!(asked(), 1);
    // Checked a moment ago: a second refresh does not ask again.
    let (code, _) = s.call("GET", "/$/datasets/kg/shapes/proposals?refresh=true", None);
    assert_eq!(code, 200);
    assert_eq!(asked(), 1);
    // A refresh spends the server's token: not for anonymous callers.
    let anonymous = ureq::get(&format!(
        "{}/$/datasets/kg/shapes/proposals?refresh=true",
        s.base
    ))
    .call();
    assert!(
        matches!(anonymous, Err(ureq::Error::Status(401, _))),
        "{anonymous:?}"
    );
}

#[test]
fn proposals_need_a_licence_that_covers_review() {
    let dir = temp_dir("unlicensed");
    let (repo, _) = repository(&dir);
    let s = start(&dir, &["sso"]);
    let binding = json!({ "url": format!("file://{}", repo.display()), "path": "shapes" });
    assert_eq!(
        s.call("PUT", "/$/datasets/kg/shapes/git", Some(&binding)).0,
        200
    );
    let (code, text) = s.call(
        "POST",
        "/$/datasets/kg/shapes/proposals?validate=false",
        Some(&change()),
    );
    assert_eq!(code, 403, "{text}");
    let (_, listed) = s.call("GET", "/$/datasets/kg/shapes/proposals", None);
    assert_eq!(listed["licensed"], false);
    assert_eq!(
        s.call("GET", "/$/datasets/kg/shapes/proposals?refresh=true", None)
            .0,
        403
    );
}

/// A proposal that fails after its branch was made is kept as failed, and the next one takes
/// the next number and branch; a tag binding is refused before the forge is asked; a set
/// pulled before the paths were recorded gets them on the next pull.
#[test]
fn failed_proposals_tags_and_older_pulls() {
    let dir = temp_dir("failed");
    let (repo, _) = repository(&dir);
    git(&repo, &["tag", "v1"]);
    let token = dir.join("token");
    std::fs::write(&token, "ghp_test").unwrap();
    let (api, seen) = forge();
    let s = start(&dir, &["review"]);
    let bind = |reference: &str| {
        let binding = json!({
            "url": format!("file://{}", repo.display()),
            "ref": reference,
            "path": "shapes",
            "reviewTokenFile": token.to_str().unwrap(),
            "forge": "github",
            "api": api,
            "project": "o/r",
        });
        assert_eq!(
            s.call("PUT", "/$/datasets/kg/shapes/git", Some(&binding)).0,
            200
        );
    };
    bind("refs/tags/v1");
    let (code, text) = s.call(
        "POST",
        "/$/datasets/kg/shapes/proposals?validate=false",
        Some(&change()),
    );
    assert_eq!(code, 409, "{text}");
    assert!(seen.lock().unwrap().is_empty());

    bind("main");
    let mut refused = change();
    refused["title"] = json!("Refused");
    let (code, text) = s.call("POST", "/$/datasets/kg/shapes/proposals", Some(&refused));
    assert_eq!(code, 502, "{text}");
    assert!(text.to_string().contains("403"), "{text}");
    let (_, listed) = s.call("GET", "/$/datasets/kg/shapes/proposals", None);
    assert_eq!(listed["proposals"][0]["status"], "failed");
    assert_eq!(listed["proposals"][0]["branch"], "tripletrail/kg-shapes-1");
    let (code, proposal) = s.call(
        "POST",
        "/$/datasets/kg/shapes/proposals?validate=false",
        Some(&change()),
    );
    assert_eq!(code, 201, "{proposal}");
    assert_eq!(proposal["proposal"], 2);
    assert_eq!(proposal["branch"], "tripletrail/kg-shapes-2");
    assert_eq!(proposal["status"], "open");

    // A version pulled before the paths were recorded: refused until a pull records them.
    for source in walk(&dir.join("data"))
        .into_iter()
        .filter(|p| p.ends_with("source.json"))
    {
        let mut stored: Value =
            serde_json::from_str(&std::fs::read_to_string(&source).unwrap()).unwrap();
        stored.as_object_mut().unwrap().remove("files");
        std::fs::write(&source, stored.to_string()).unwrap();
    }
    let (code, text) = s.call(
        "POST",
        "/$/datasets/kg/shapes/proposals?validate=false",
        Some(&change()),
    );
    assert_eq!(code, 409, "{text}");
    let (code, pulled) = s.call("POST", "/$/datasets/kg/shapes/pull", None);
    assert_eq!((code, &pulled["changed"]), (200, &json!(false)), "{pulled}");
    let (code, proposal) = s.call(
        "POST",
        "/$/datasets/kg/shapes/proposals?validate=false",
        Some(&change()),
    );
    assert_eq!(code, 201, "{proposal}");
    assert_eq!(proposal["files"], json!(["shapes/rail/thing.ttl"]));
}

fn walk(path: &Path) -> Vec<PathBuf> {
    if path.is_dir() {
        std::fs::read_dir(path)
            .unwrap()
            .flatten()
            .flat_map(|e| walk(&e.path()))
            .collect()
    } else {
        vec![path.to_owned()]
    }
}

/// The full data validated with the proposed shapes: at once for a dry run with
/// `validate=true`, in the background for a proposal, with the result in the proposal and as
/// a comment on the pull request.
#[test]
fn a_proposal_validates_the_data_with_the_proposed_shapes() {
    let dir = temp_dir("validate");
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join("shapes")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    // A row the data would fail, switched off; and one it fails now.
    std::fs::write(
        repo.join("shapes/thing.ttl"),
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix ex: <http://example.com/> .\n\
         ex:ThingShape a sh:NodeShape ; sh:targetClass ex:Thing ;\n\
           sh:property [ sh:path ex:name ; sh:minCount 1 ; sh:deactivated true ] ,\n\
             [ sh:path ex:code ; sh:minCount 1 ] .\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    let token = dir.join("token");
    std::fs::write(&token, "ghp_test").unwrap();
    let (api, seen) = forge();
    let s = start(&dir, &["review"]);
    let binding = json!({
        "url": format!("file://{}", repo.display()),
        "ref": "main",
        "path": "shapes",
        "reviewTokenFile": token.to_str().unwrap(),
        "forge": "github",
        "api": api,
        "project": "o/r",
    });
    assert_eq!(
        s.call("PUT", "/$/datasets/kg/shapes/git", Some(&binding)).0,
        200
    );
    // Name becomes required: one new result for t1; code stays as it was.
    let change = json!({
        "title": "Names required",
        "changes": [{ "shape": "http://example.com/ThingShape", "path": "http://example.com/name",
                      "setting": "deactivated", "value": false }],
    });
    let (code, dry) = s.call(
        "POST",
        "/$/datasets/kg/shapes/proposals?dry=true&validate=true",
        Some(&change),
    );
    assert_eq!(code, 200, "{dry}");
    let v = &dry["validation"];
    assert_eq!(v["status"], "done", "{v}");
    assert_eq!((&v["before"], &v["after"]), (&json!(1), &json!(2)), "{v}");
    assert_eq!((&v["new"], &v["gone"]), (&json!(1), &json!(0)), "{v}");
    assert_eq!(v["compared"], true, "{v}");
    assert_eq!(v["newResults"][0]["focus"], "http://example.com/t1");
    assert_eq!(v["newResults"][0]["path"], "http://example.com/name");
    assert_eq!(v["newByShape"].as_array().unwrap().len(), 1, "{v}");
    // A dry run without validate=true does not validate.
    let (_, quick) = s.call(
        "POST",
        "/$/datasets/kg/shapes/proposals?dry=true",
        Some(&change),
    );
    assert!(quick.get("validation").is_none(), "{quick}");

    // The proposal's validation waits its turn, runs, and comments on the pull request.
    let comment = || {
        let (code, proposal) = s.call("POST", "/$/datasets/kg/shapes/proposals", Some(&change));
        assert_eq!(code, 201, "{proposal}");
        assert_eq!(proposal["validation"]["status"], "queued");
        let mut listed = Value::Null;
        for _ in 0..200 {
            listed = s.call("GET", "/$/datasets/kg/shapes/proposals", None).1;
            let status = &listed["proposals"][0]["validation"]["status"];
            if status != "queued" && status != "running" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let v = &listed["proposals"][0]["validation"];
        assert_eq!(v["status"], "done", "{listed}");
        assert_eq!(v["new"], 1);
        assert!(v.get("comment").is_none(), "{v}");
        let seen = seen.lock().unwrap().clone();
        let (method, path, _, body) = seen.last().unwrap();
        assert_eq!(
            (method.as_str(), path.as_str()),
            ("POST", "/repos/o/r/issues/7/comments")
        );
        body["body"].as_str().unwrap().to_owned()
    };
    let text = comment();
    assert!(
        text.starts_with(
            "TripleTrail validated all of dataset `kg` with the proposed shapes: 2 results (1 with \
             the shapes in use), 1 new, 0 gone."
        ),
        "{text}"
    );
    assert!(
        text.contains("New results by shape:\n- `[") && text.contains("minCount"),
        "{text}"
    );
    // The data stays on the server unless the binding says the forge may list it.
    assert!(!text.contains("example.com/t1"), "{text}");
    let mut listing = binding.clone();
    listing["reviewResults"] = json!(true);
    assert_eq!(
        s.call("PUT", "/$/datasets/kg/shapes/git", Some(&listing)).0,
        200
    );
    let text = comment();
    assert!(
        text.contains("- `http://example.com/t1` `http://example.com/name`"),
        "{text}"
    );
    listing["reviewResults"] = json!("yes");
    assert_eq!(
        s.call("PUT", "/$/datasets/kg/shapes/git", Some(&listing)).0,
        400
    );

    // A validation another run of the server left running is shown as interrupted.
    let file = find(&dir, ".proposals.json").expect("the proposals file");
    let mut kept: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    kept[0]["validation"] = json!({ "status": "running", "server": "1-gone" });
    std::fs::write(&file, kept.to_string()).unwrap();
    let listed = s.call("GET", "/$/datasets/kg/shapes/proposals", None).1;
    let last = listed["proposals"].as_array().unwrap().last().unwrap();
    assert_eq!(last["validation"]["status"], "interrupted", "{listed}");
}

/// The first file named `name` under `dir`.
fn find(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == name) {
            return Some(path);
        }
        if path.is_dir()
            && let Some(found) = find(&path, name)
        {
            return Some(found);
        }
    }
    None
}

/// A proposal's validation reads a snapshot without holding the store's dictionary: a write
/// with new terms answers while it runs.
#[test]
fn writes_go_on_while_a_proposal_validates() {
    let dir = temp_dir("writes");
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join("shapes")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(
        repo.join("shapes/thing.ttl"),
        "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix ex: <http://example.com/> .\n\
         ex:ThingShape a sh:NodeShape ; sh:targetClass ex:Thing ;\n\
           sh:property [ sh:path ex:name ; sh:minCount 1 ; sh:deactivated true ] ,\n\
             [ sh:path ex:code ; sh:minCount 1 ] .\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    let token = dir.join("token");
    std::fs::write(&token, "ghp_test").unwrap();
    let (api, _seen) = forge();
    // Enough things that the validation takes a while.
    let mut data = String::new();
    for i in 0..300_000 {
        data.push_str(&format!(
            "<http://example.com/n{i}> a <http://example.com/Thing> .\n"
        ));
    }
    let s = start_with(&dir, &["review"], &data);
    let update = |text: &str| {
        ureq::post(&format!("{}/kg/update", s.base))
            .set("Authorization", &format!("Bearer {TOKEN}"))
            .set("Content-Type", "application/sparql-update")
            .send_string(text)
            .unwrap()
            .status()
    };
    let binding = json!({
        "url": format!("file://{}", repo.display()), "ref": "main", "path": "shapes",
        "reviewTokenFile": token.to_str().unwrap(), "forge": "github", "api": api, "project": "o/r",
    });
    assert_eq!(
        s.call("PUT", "/$/datasets/kg/shapes/git", Some(&binding)).0,
        200
    );
    let change = json!({
        "title": "Names required",
        "changes": [{ "shape": "http://example.com/ThingShape", "path": "http://example.com/name",
                      "setting": "deactivated", "value": false }],
    });
    assert_eq!(
        s.call("POST", "/$/datasets/kg/shapes/proposals", Some(&change))
            .0,
        201
    );
    let status = || {
        s.call("GET", "/$/datasets/kg/shapes/proposals", None).1["proposals"][0]["validation"]
            ["status"]
            .clone()
    };
    while status() == "queued" {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // Well into the run.
    std::thread::sleep(std::time::Duration::from_secs(1));
    let started = std::time::Instant::now();
    assert_eq!(
        update("INSERT DATA { <urn:new:1> <urn:newp:1> \"new literal 1\" }"),
        200
    );
    let took = started.elapsed();
    let during = status();
    while status() == "running" {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(status(), "done");
    // The write went on while the validation ran, and did not wait for it.
    assert_eq!(
        during, "running",
        "the validation ended before the write: {took:?}"
    );
    assert!(took < std::time::Duration::from_secs(2), "{took:?}");
}

#[test]
fn a_refresh_asks_the_forge_whether_requests_were_merged() {
    let dir = temp_dir("refresh");
    let (repo, _) = repository(&dir);
    let token = dir.join("token");
    std::fs::write(&token, "ghp_test").unwrap();
    let (api, seen) = forge();
    let s = start(&dir, &["review"]);
    let binding = json!({
        "url": format!("file://{}", repo.display()), "ref": "main", "path": "shapes",
        "reviewTokenFile": token.to_str().unwrap(), "forge": "github", "api": api, "project": "o/r",
    });
    assert_eq!(
        s.call("PUT", "/$/datasets/kg/shapes/git", Some(&binding)).0,
        200
    );
    assert_eq!(
        s.call(
            "POST",
            "/$/datasets/kg/shapes/proposals?validate=false",
            Some(&change())
        )
        .0,
        201
    );
    // A plain list does not ask the forge.
    let asked = seen.lock().unwrap().len();
    let (_, listed) = s.call("GET", "/$/datasets/kg/shapes/proposals", None);
    assert_eq!(listed["proposals"][0]["status"], "open");
    assert_eq!(seen.lock().unwrap().len(), asked);
    // A refresh does, and keeps what it found.
    let (code, listed) = s.call("GET", "/$/datasets/kg/shapes/proposals?refresh=true", None);
    assert_eq!(code, 200, "{listed}");
    let p = &listed["proposals"][0];
    assert_eq!(p["status"], "merged", "{listed}");
    assert!(p["checked"].is_string(), "{listed}");
    assert!(p.get("statusError").is_none(), "{listed}");
    let calls = seen.lock().unwrap().clone();
    let (method, path, token, _) = calls.last().unwrap();
    assert_eq!(
        (method.as_str(), path.as_str(), token.as_str()),
        ("GET", "/repos/o/r/pulls/7", "Bearer ghp_test")
    );
    // A merged proposal is not asked again.
    let forge_calls = || seen.lock().unwrap().len();
    let asked = forge_calls();
    s.call("GET", "/$/datasets/kg/shapes/proposals?refresh=true", None);
    assert_eq!(forge_calls(), asked);
    assert_eq!(
        s.call("GET", "/$/datasets/kg/shapes/proposals?refresh=maybe", None)
            .0,
        400
    );
}

/// A refresh spends the server's token: the dataset's admins may, a reader may not, with GET
/// or HEAD alike, and the forge is not asked for the reader.
#[test]
fn only_the_datasets_admins_and_shape_editors_propose_and_refresh() {
    use tripletrail_server::auth::{sha256, to_hex};
    let dir = temp_dir("refresh-rights");
    let (repo, _) = repository(&dir);
    let token = dir.join("token");
    std::fs::write(&token, "ghp_test").unwrap();
    let (api, seen) = forge();
    let key = |k: &str, groups: &str| {
        format!(
            r#"{{"sha256": "{}", "agent": "https://example.org/people/{k}", "groups": [{groups}]}}"#,
            to_hex(&sha256(k.as_bytes()))
        )
    };
    let config = format!(
        r#"{{"auth": {{"anonymous": false, "apiKeys": [{}, {}, {}, {}, {}]}},
            "datasets": {{"kg": {{"admins": ["kg-admins"],
                "shapeEditors": ["kg-modellers", "https://example.org/people/ana"]}}}}}}"#,
        key("kgadmin", r#""kg-admins""#),
        key("root", r#""tripletrail:admin""#),
        key("reader", ""),
        key("modeller", r#""kg-modellers""#),
        key("ana", ""),
    );
    std::fs::write(dir.join("guard.json"), config).unwrap();
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::Turtle,
            "<http://example.com/t1> a <http://example.com/Thing> .".as_bytes(),
        )
        .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), engine)],
        Options {
            threads: 4,
            auth_config: Some(dir.join("guard.json")),
            location: Some(dir.join("data")),
            license: Ok(licence(&["review"])),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let call = |method: &str, path: &str, key: &str, body: Option<&Value>| -> u16 {
        let request = ureq::request(method, &format!("{base}{path}"))
            .set("X-API-Key", key)
            .set("Content-Type", "application/json");
        let answer = match body {
            Some(b) => request.send_string(&b.to_string()),
            None => request.call(),
        };
        match answer {
            Ok(r) => r.status(),
            Err(ureq::Error::Status(code, _)) => code,
            Err(e) => panic!("{e}"),
        }
    };
    let binding = json!({
        "url": format!("file://{}", repo.display()), "ref": "main", "path": "shapes",
        "reviewTokenFile": token.to_str().unwrap(), "forge": "github", "api": api, "project": "o/r",
    });
    assert_eq!(
        call("PUT", "/$/datasets/kg/shapes/git", "root", Some(&binding)),
        200
    );
    assert_eq!(
        call(
            "POST",
            "/$/datasets/kg/shapes/proposals?validate=false",
            "kgadmin",
            Some(&change())
        ),
        201
    );
    let asked = || {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|(m, p, _, _)| m == "GET" && p.ends_with("/pulls/7"))
            .count()
    };
    // A reader lists them, and may not refresh, with GET or HEAD.
    let refresh = "/$/datasets/kg/shapes/proposals?refresh=true";
    assert_eq!(
        call("GET", "/$/datasets/kg/shapes/proposals", "reader", None),
        200
    );
    assert_eq!(call("GET", refresh, "reader", None), 403);
    assert_eq!(call("HEAD", refresh, "reader", None), 403);
    assert_eq!(asked(), 0);
    // The dataset's admin may, without being a server admin.
    assert_eq!(call("GET", refresh, "kgadmin", None), 200);
    assert_eq!(asked(), 1);
    // A reader may not propose either.
    let proposals = "/$/datasets/kg/shapes/proposals?validate=false";
    assert_eq!(call("POST", proposals, "reader", Some(&change())), 403);
    // Shape editors, by group or as agents, propose (dry runs too) and refresh.
    assert_eq!(
        call(
            "POST",
            "/$/datasets/kg/shapes/proposals?dry=true",
            "modeller",
            Some(&change())
        ),
        200
    );
    assert_eq!(call("POST", proposals, "ana", Some(&change())), 201);
    assert_eq!(call("HEAD", refresh, "modeller", None), 200);
    let list: Value = ureq::get(&format!("{base}/$/datasets/kg/shapes/proposals"))
        .set("X-API-Key", "ana")
        .call()
        .unwrap()
        .into_string()
        .map(|t| serde_json::from_str(&t).unwrap())
        .unwrap();
    assert_eq!(list["proposals"][0]["by"], "https://example.org/people/ana");
    // They change nothing else: no saving, no binding.
    assert_eq!(
        call(
            "POST",
            "/$/datasets/kg/shapes/edit",
            "modeller",
            Some(&change())
        ),
        403
    );
    assert_eq!(
        call("PUT", "/$/datasets/kg/shapes/git", "ana", Some(&binding)),
        403
    );
}
