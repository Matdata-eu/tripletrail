//! Runs the examples of the user guide (`docs/guide/*.md`), so the guide stays true to what
//! `tripletrail` does.
//!
//! A fenced code block is run when an HTML comment right before it says how (blank lines
//! between the two are allowed); blocks without one are only shown. The comments are invisible
//! on GitHub:
//!
//! - `<!-- test: file PATH -->`: the block is the content of `PATH` in the chapter's folder.
//!   When the sample data (`docs/guide/data/`, copied into the folder first) has that file, the
//!   block must be the same as it.
//! - `<!-- test: run -->`: a bash script (`set -euo pipefail`), run in the chapter's folder with
//!   `tripletrail` on the `PATH`. `<!-- test: run fails -->` expects it to exit with an error.
//! - `<!-- test: serve -->`: a command that starts a server (`tripletrail serve …`). It runs in the
//!   background until the next `serve` or the end of the chapter; the harness waits for
//!   `/$/ready`.
//! - `<!-- test: query [DATASET] -->` and `<!-- test: update [DATASET] -->`: a SPARQL query or
//!   update sent to the running server (dataset `ds` by default). A query's output is its CSV
//!   results.
//! - `<!-- test: output [contains|unordered] -->`: the output of the block run last, exactly
//!   (lines with their trailing spaces trimmed), `contains`: each line of the block is in the
//!   output, `unordered`: the same lines in any order.
//!
//! Every chapter runs in a folder of its own, on its own server port: `7878` in the blocks is
//! replaced with that port. `GUIDE_CHAPTER=04` runs only the chapters whose file name contains
//! it. Without `bash` and `curl` the test is skipped, except under CI.

use std::collections::BTreeSet;
use std::io::Read as _;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn guide_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/guide")
}

#[derive(Debug)]
struct Block {
    /// The directive, without `test:`.
    directive: String,
    /// Line of the opening fence, from 1.
    line: usize,
    text: String,
}

/// The fenced blocks of a chapter that carry a `test:` directive.
fn blocks(markdown: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut pending: Option<String> = None;
    let mut lines = markdown.lines().enumerate();
    while let Some((n, line)) = lines.next() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed
            .strip_prefix("<!--")
            .and_then(|r| r.strip_suffix("-->"))
            .map(str::trim)
            .and_then(|r| r.strip_prefix("test:"))
        {
            pending = Some(rest.trim().to_string());
            continue;
        }
        let fence: String = trimmed
            .chars()
            .take_while(|c| *c == '`' || *c == '~')
            .collect();
        if fence.len() >= 3 && fence.chars().all(|c| c == fence.chars().next().unwrap()) {
            let indent = line.len() - line.trim_start().len();
            let mut text = String::new();
            for (_, inner) in lines.by_ref() {
                if inner.trim() == fence {
                    break;
                }
                text.push_str(inner.get(indent..).unwrap_or(inner.trim_start()));
                text.push('\n');
            }
            if let Some(directive) = pending.take() {
                out.push(Block {
                    directive,
                    line: n + 1,
                    text,
                });
            }
            continue;
        }
        if !trimmed.is_empty() {
            pending = None;
        }
    }
    out
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn have(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn normalize(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text
        .replace("\r\n", "\n")
        .lines()
        .map(|l| l.trim_end().to_string())
        .collect();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    while lines.first().is_some_and(String::is_empty) {
        lines.remove(0);
    }
    lines
}

struct Chapter {
    name: String,
    dir: PathBuf,
    bin: PathBuf,
    port: u16,
    server: Option<Child>,
    output: Option<String>,
}

impl Chapter {
    fn new(name: &str, root: &Path) -> Self {
        let dir = root.join(name.trim_end_matches(".md"));
        let _ = std::fs::remove_dir_all(&dir);
        let data = guide_dir().join("data");
        if data.is_dir() {
            copy_dir(&data, &dir);
        } else {
            std::fs::create_dir_all(&dir).unwrap();
        }
        // `tripletrail` on the PATH, outside the chapter's folder.
        let bin = root.join(format!("bin-{}", name.trim_end_matches(".md")));
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("tripletrail");
        let _ = std::fs::remove_file(&exe);
        #[cfg(unix)]
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_tripletrail"), &exe).unwrap();
        // The commands' temporary folder, outside the chapter's folder (whose files the guide
        // lists) but inside the test's: a server killed at the end of a chapter cannot remove
        // the temporary folders it made (its workspaces, say), and these go with the test's.
        std::fs::create_dir_all(root.join(format!("tmp-{}", name.trim_end_matches(".md"))))
            .unwrap();
        Chapter {
            name: name.to_string(),
            dir,
            bin,
            port: free_port(),
            server: None,
            output: None,
        }
    }

    fn here(&self, block: &Block) -> String {
        format!("{}:{}", self.name, block.line)
    }

    fn with_port(&self, text: &str) -> String {
        text.replace(":7878", &format!(":{}", self.port))
    }

    fn command(&self, script: &str) -> Command {
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new("bash");
        // The guide shows the defaults: no TRIPLETRAIL_* setting of the test's own environment
        // (CI runs the tests again with TRIPLETRAIL_STORE=oxigraph) reaches the commands.
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("TRIPLETRAIL_") {
                cmd.env_remove(name);
            }
        }
        cmd.arg("-c")
            .arg(format!("set -euo pipefail\n{}", self.with_port(script)))
            .current_dir(&self.dir)
            .env("PATH", path)
            .env(
                "TMPDIR",
                self.bin
                    .with_file_name(format!("tmp-{}", self.name.trim_end_matches(".md"))),
            )
            .env("TRIPLETRAIL_BIND", format!("127.0.0.1:{}", self.port));
        cmd
    }

    fn run(&mut self, block: &Block, should_fail: bool) {
        let mut child = self
            .command(&block.text)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(300);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("{}: the block ran longer than 300 s", self.here(block));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut stdout = String::new();
        let mut stderr = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut stdout)
            .unwrap();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        if status.success() == should_fail {
            panic!(
                "{}: the block {} ({status})\n--- stdout\n{stdout}\n--- stderr\n{stderr}\n--- server log\n{}",
                self.here(block),
                if should_fail {
                    "should fail but succeeded"
                } else {
                    "failed"
                },
                self.server_log()
            );
        }
        self.output = Some(stdout + &stderr);
    }

    fn server_log(&self) -> String {
        std::fs::read_to_string(self.dir.join(".server.log")).unwrap_or_default()
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.server.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn serve(&mut self, block: &Block) {
        self.stop();
        let log = std::fs::File::create(self.dir.join(".server.log")).unwrap();
        let script = format!("exec {}", block.text.trim().trim_end_matches('&'));
        let child = self
            .command(&script)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        self.server = Some(child);
        let url = format!("http://127.0.0.1:{}/$/ready", self.port);
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let ready = ureq::get(&url).timeout(Duration::from_secs(5)).call();
            if ready.is_ok_and(|r| r.status() == 200) {
                break;
            }
            if let Some(status) = self.server.as_mut().unwrap().try_wait().unwrap() {
                panic!(
                    "{}: the server stopped ({status})\n{}",
                    self.here(block),
                    self.server_log()
                );
            }
            if Instant::now() > deadline {
                panic!(
                    "{}: the server was not ready after 120 s\n{}",
                    self.here(block),
                    self.server_log()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.output = None;
    }

    fn sparql(&mut self, block: &Block, dataset: &str, update: bool) {
        assert!(
            self.server.is_some(),
            "{}: no server runs (add a `test: serve` block first)",
            self.here(block)
        );
        let base = format!("http://127.0.0.1:{}/{dataset}", self.port);
        // A query may name the server itself (a SERVICE to another of its datasets).
        let text = self.with_port(&block.text);
        let result = if update {
            ureq::post(&format!("{base}/update"))
                .set("Content-Type", "application/sparql-update")
                .send_string(&text)
        } else {
            ureq::post(&format!("{base}/query"))
                .set("Content-Type", "application/sparql-query")
                .set("Accept", "text/csv")
                .send_string(&text)
        };
        match result {
            Ok(r) => self.output = Some(r.into_string().unwrap()),
            Err(ureq::Error::Status(code, r)) => panic!(
                "{}: the server answered {code}: {}",
                self.here(block),
                r.into_string().unwrap_or_default()
            ),
            Err(e) => panic!("{}: {e}", self.here(block)),
        }
    }

    fn file(&mut self, block: &Block, path: &str) {
        let target = self.dir.join(path);
        if let Some(sample) = Some(guide_dir().join("data").join(path)).filter(|p| p.is_file()) {
            let sample = std::fs::read_to_string(sample).unwrap();
            assert_eq!(
                normalize(&sample),
                normalize(&block.text),
                "{}: the block shows {path}, but docs/guide/data/{path} differs",
                self.here(block)
            );
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(target, &block.text).unwrap();
    }

    fn expect(&self, block: &Block, mode: &str) {
        let Some(output) = &self.output else {
            panic!(
                "{}: an output block with nothing run before it",
                self.here(block)
            );
        };
        let got = normalize(output);
        let want = normalize(&block.text);
        let ok = match mode {
            "" => got == want,
            "unordered" => {
                let mut g = got.clone();
                let mut w = want.clone();
                g.sort();
                w.sort();
                g == w
            }
            "contains" => want
                .iter()
                .all(|w| got.iter().any(|g| g.contains(w.as_str()))),
            other => panic!("{}: unknown output mode {other:?}", self.here(block)),
        };
        assert!(
            ok,
            "{}: the output differs from the guide ({})\n--- guide\n{}\n--- got\n{}",
            self.here(block),
            if mode.is_empty() { "exact" } else { mode },
            want.join("\n"),
            got.join("\n")
        );
    }

    fn block(&mut self, block: &Block) {
        let mut words = block.directive.split_whitespace();
        let mut kind = words.next().unwrap_or_default();
        // `enterprise run`, `enterprise output`…: an example of an enterprise feature (#818),
        // run only by the enterprise build's tests.
        // `open output`…: what only the open build answers, such as its edition.
        if kind == "enterprise" || kind == "open" {
            if cfg!(feature = "enterprise") != (kind == "enterprise") {
                return;
            }
            kind = words.next().unwrap_or_default();
        }
        let arg = words.collect::<Vec<_>>().join(" ");
        match kind {
            "file" => self.file(block, &arg),
            "run" => self.run(block, arg == "fails"),
            "serve" => self.serve(block),
            "query" => self.sparql(block, if arg.is_empty() { "ds" } else { &arg }, false),
            "update" => self.sparql(block, if arg.is_empty() { "ds" } else { &arg }, true),
            "output" => self.expect(block, &arg),
            other => panic!("{}: unknown test directive {other:?}", self.here(block)),
        }
    }
}

impl Drop for Chapter {
    fn drop(&mut self) {
        self.stop();
    }
}

#[test]
fn the_guides_examples_work() {
    if !cfg!(unix) || !have("bash") || !have("curl") {
        assert!(
            std::env::var_os("CI").is_none(),
            "the guide's examples need bash and curl"
        );
        eprintln!("skipped: the guide's examples need bash and curl");
        return;
    }
    let only = std::env::var("GUIDE_CHAPTER").ok();
    let root = tripletrail_testdir::tempdir("guide");
    let mut chapters: Vec<String> = std::fs::read_dir(guide_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".md"))
        .filter(|n| only.as_ref().is_none_or(|o| n.contains(o.as_str())))
        .collect();
    chapters.sort();
    let mut ran = BTreeSet::new();
    for name in &chapters {
        let markdown = std::fs::read_to_string(guide_dir().join(name)).unwrap();
        let blocks = blocks(&markdown);
        if blocks.is_empty() {
            continue;
        }
        let mut chapter = Chapter::new(name, &root);
        for block in &blocks {
            chapter.block(block);
        }
        ran.insert(name.clone());
    }
    assert!(
        only.is_some() || !ran.is_empty(),
        "no chapter of the guide has tests"
    );
}

#[test]
fn directives_are_read_from_the_comment_before_a_block() {
    let md = "Text\n\n<!-- test: run -->\n```sh\necho hi\n```\n\n```sh\nnot run\n```\n\n\
              <!-- a plain comment -->\n```sh\nnot run either\n```\n\n<!-- test: output -->\n\n\
              ```text\nhi\n```\n\n1. A step:\n\n   <!-- test: file a.ttl -->\n   ```turtle\n   <a> <b> <c> .\n   ```\n";
    let found = blocks(md);
    let summary: Vec<(&str, &str)> = found
        .iter()
        .map(|b| (b.directive.as_str(), b.text.as_str()))
        .collect();
    assert_eq!(
        summary,
        [
            ("run", "echo hi\n"),
            ("output", "hi\n"),
            ("file a.ttl", "<a> <b> <c> .\n")
        ]
    );
}
