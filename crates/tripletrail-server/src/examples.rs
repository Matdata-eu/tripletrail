//! Example queries and the default query text of the web UI's Query page (#262):
//! `--examples [DATASET=]DIR` and `--default-query [DATASET=]FILE`, served at `/{ds}/examples`.
//!
//! An example is a `.rq` file whose first line, when it is a `#` comment, is its title (else
//! the file's name). The files of a directory given without a dataset are every dataset's;
//! a dataset's own come first.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

/// The examples and default query text, read when the server starts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Examples {
    /// Every dataset's examples.
    all: Vec<Example>,
    /// A dataset's own examples, by its name.
    datasets: BTreeMap<String, Vec<Example>>,
    /// Every dataset's default query text.
    default_all: Option<String>,
    /// A dataset's own default query text, by its name.
    default: BTreeMap<String, String>,
}

/// An example query: its title and text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Example {
    pub title: String,
    pub query: String,
}

/// The most examples one directory may hold.
const MAX_FILES: usize = 200;
/// The largest query file.
const MAX_BYTES: u64 = 64 * 1024;

impl Examples {
    /// Reads `--examples` (`[DATASET=]DIR`) and `--default-query` (`[DATASET=]FILE`).
    pub fn read(examples: &[String], default_query: &[String]) -> Result<Self, String> {
        let mut out = Self::default();
        for value in examples.iter().filter(|v| !v.trim().is_empty()) {
            let (dataset, dir) = split(value);
            let found = read_dir(Path::new(dir))?;
            match dataset {
                Some(ds) => out.datasets.entry(ds.to_owned()).or_default().extend(found),
                None => out.all.extend(found),
            }
        }
        for value in default_query.iter().filter(|v| !v.trim().is_empty()) {
            let (dataset, file) = split(value);
            let text = read_query(Path::new(file), "--default-query")?;
            match dataset {
                Some(ds) => {
                    out.default.insert(ds.to_owned(), text);
                }
                None => out.default_all = Some(text),
            }
        }
        Ok(out)
    }

    /// Whether any example or default query was given.
    pub fn is_empty(&self) -> bool {
        self.all.is_empty()
            && self.datasets.is_empty()
            && self.default_all.is_none()
            && self.default.is_empty()
    }

    /// `/{ds}/examples`: the examples of the dataset called by any of `names` (the name asked
    /// and, for an alias, its target's), its own first, and its default query text.
    pub fn answer(&self, names: &[&str]) -> Value {
        let own = names.iter().find_map(|n| self.datasets.get(*n));
        let examples: Vec<Value> = own
            .into_iter()
            .flatten()
            .chain(&self.all)
            .map(|e| json!({ "title": e.title, "query": e.query }))
            .collect();
        let default = names
            .iter()
            .find_map(|n| self.default.get(*n))
            .or(self.default_all.as_ref());
        json!({ "examples": examples, "defaultQuery": default })
    }
}

/// `DATASET=PATH` or `PATH`: the dataset is a name before `=` with no path separator in it.
fn split(value: &str) -> (Option<&str>, &str) {
    let value = value.trim();
    match value.split_once('=') {
        Some((ds, path))
            if !ds.is_empty() && !ds.contains(['/', '\\']) && !path.trim().is_empty() =>
        {
            (Some(ds.trim()), path.trim())
        }
        _ => (None, value),
    }
}

/// The `.rq` files of `dir`, by file name.
fn read_dir(dir: &Path) -> Result<Vec<Example>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        format!(
            "--examples: cannot read the directory {}: {e}",
            dir.display()
        )
    })?;
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "rq"))
        .collect();
    if files.len() > MAX_FILES {
        return Err(format!(
            "--examples: {} holds {} .rq files, more than {MAX_FILES}",
            dir.display(),
            files.len()
        ));
    }
    files.sort();
    files
        .iter()
        .map(|path| {
            let query = read_query(path, "--examples")?;
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().replace(['_', '-'], " "))
                .unwrap_or_default();
            Ok(Example {
                title: title(&query).unwrap_or(stem),
                query,
            })
        })
        .collect()
}

/// A query file: UTF-8 text of at most 64 KB.
fn read_query(path: &Path, option: &str) -> Result<String, String> {
    let size = std::fs::metadata(path)
        .map_err(|e| format!("{option}: cannot read {}: {e}", path.display()))?
        .len();
    if size > MAX_BYTES {
        return Err(format!("{option}: {} is larger than 64 KB", path.display()));
    }
    let bytes = std::fs::read(path)
        .map_err(|e| format!("{option}: cannot read {}: {e}", path.display()))?;
    String::from_utf8(bytes).map_err(|_| format!("{option}: {} is not UTF-8 text", path.display()))
}

/// The title of a query: its first line, when it is a `#` comment.
fn title(query: &str) -> Option<String> {
    let first = query.trim_start_matches('\u{feff}').lines().next()?.trim();
    let text = first.strip_prefix('#')?.trim_start_matches('#').trim();
    (!text.is_empty()).then(|| text.chars().take(120).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_come_from_the_first_comment_or_the_file_name() {
        let dir = tripletrail_testdir::tempdir("examples");
        let rail = dir.join("rail");
        std::fs::create_dir_all(&rail).unwrap();
        std::fs::write(
            dir.join("01-count.rq"),
            "# Count the triples\nSELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }",
        )
        .unwrap();
        std::fs::write(
            dir.join("02_all_classes.rq"),
            "SELECT DISTINCT ?c WHERE { ?s a ?c }",
        )
        .unwrap();
        std::fs::write(dir.join("notes.txt"), "not a query").unwrap();
        std::fs::write(
            rail.join("stations.rq"),
            "## Stations\nSELECT ?s WHERE { ?s a <http://example.com/Station> }",
        )
        .unwrap();
        std::fs::write(
            dir.join("default.rq"),
            "PREFIX ex: <http://example.com/>\nSELECT * WHERE { ?s ?p ?o } LIMIT 10",
        )
        .unwrap();
        let examples = Examples::read(
            &[
                dir.display().to_string(),
                format!("rail={}", rail.display()),
            ],
            &[format!("rail={}", dir.join("default.rq").display())],
        )
        .unwrap();
        let answer = examples.answer(&["rail"]);
        let titles: Vec<&str> = answer["examples"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["title"].as_str().unwrap())
            .collect();
        assert_eq!(
            titles,
            ["Stations", "Count the triples", "02 all classes", "default"]
        );
        assert!(
            answer["defaultQuery"]
                .as_str()
                .unwrap()
                .starts_with("PREFIX ex:")
        );
        let other = examples.answer(&["kg"]);
        assert_eq!(other["examples"].as_array().unwrap().len(), 3);
        assert!(other["defaultQuery"].is_null());
        assert!(
            Examples::read(&[dir.join("missing").display().to_string()], &[])
                .unwrap_err()
                .contains("cannot read the directory")
        );
        std::fs::write(dir.join("big.rq"), vec![b'#'; 70 * 1024]).unwrap();
        assert!(
            Examples::read(&[dir.display().to_string()], &[])
                .unwrap_err()
                .contains("larger than 64 KB")
        );
    }

    #[test]
    fn a_dataset_is_a_name_before_the_equals_sign() {
        assert_eq!(split("rail=/etc/ex"), (Some("rail"), "/etc/ex"));
        assert_eq!(split("/etc/a=b/ex"), (None, "/etc/a=b/ex"));
        assert_eq!(split("/etc/ex"), (None, "/etc/ex"));
        assert_eq!(split("C:\\ex"), (None, "C:\\ex"));
    }
}
