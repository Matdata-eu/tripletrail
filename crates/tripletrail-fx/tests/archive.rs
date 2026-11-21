//! Archives and folders as lists of names, on SPARQL Anything's archive test files (see
//! `sparql-anything/README.md`), and files read from inside archives.

use oxigraph::model::Term;
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use std::path::PathBuf;
use tripletrail_fx::{FxConfig, FxService};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/sparql-anything/archive")
}

/// The values of `?v` the query's WHERE pattern binds, in slot order when it orders them.
fn select(pattern: &str) -> Result<Vec<String>, String> {
    select_with(pattern, "")
}

/// As [`select`], with `tail` (ORDER BY ...) after the WHERE pattern.
fn select_with(pattern: &str, tail: &str) -> Result<Vec<String>, String> {
    select_in(
        FxConfig {
            fixed_limits: false,
            databases: None,
            base: dir(),
            allowed: None,
            remote: Default::default(),
        },
        pattern,
        tail,
    )
}

/// As [`select_with`], with the sources `config` allows.
fn select_in(config: FxConfig, pattern: &str, tail: &str) -> Result<Vec<String>, String> {
    let fx = FxService::new(config, SparqlEvaluator::new);
    let empty = Box::leak(Box::new(oxigraph::model::Dataset::new()));
    let query = format!(
        "PREFIX fx: <http://sparql.xyz/facade-x/ns/>
         PREFIX fxe: <http://sparql.xyz/facade-x/engine/>
         PREFIX xyz: <http://sparql.xyz/facade-x/data/>
         PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>
         PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
         SELECT ?v WHERE {{ {pattern} }} {tail}"
    );
    let QueryResults::Solutions(solutions) = fx
        .register(SparqlEvaluator::new())
        .parse_query(&query)
        .unwrap()
        .on_queryable_dataset(&*empty)
        .execute()
        .map_err(|e| e.to_string())?
    else {
        panic!("not a SELECT")
    };
    solutions
        .map(|s| {
            let s = s.map_err(|e| e.to_string())?;
            Ok(match s.get("v") {
                Some(Term::Literal(l)) => l.value().to_owned(),
                Some(t) => t.to_string(),
                None => String::new(),
            })
        })
        .collect()
}

/// The names in the root's slots, in order; `matches`: `archive.matches`.
fn names(location: &str, matches: Option<&str>) -> Result<Vec<String>, String> {
    let option = matches
        .map(|m| format!("fxe:properties fxe:archive.matches {m:?} ."))
        .unwrap_or_default();
    select_with(
        &format!(
            "SERVICE <x-sparql-anything:location={location}> {{ {option}
                ?root a fx:Root ; ?slot ?v .
                FILTER(?slot != rdf:type) }}
             BIND(xsd:integer(STRAFTER(STR(?slot), \"#_\")) AS ?n)"
        ),
        "ORDER BY ?n",
    )
}

const ENTRIES: [&str; 5] = [
    "test/",
    "test/test.csv",
    "test/test.json",
    "test/test.xml",
    "test/test.txt",
];

#[test]
fn tar_and_tgz_list_their_entries() {
    assert_eq!(names("test.tar", None).unwrap(), ENTRIES);
    assert_eq!(names("test.tgz", None).unwrap(), ENTRIES);
}

#[test]
fn bzip2_tars_list_their_entries_and_give_their_files() {
    assert_eq!(names("test.tar.bz2", None).unwrap(), ENTRIES);
    assert_eq!(names("test.tbz2", None).unwrap(), ENTRIES);
    let makes = select(
        "SERVICE <x-sparql-anything:location=test/test.csv,from-archive=test.tar.bz2,csv.headers=true> {
            ?row xyz:Make ?v }",
    )
    .unwrap();
    assert!(makes.contains(&"Ford".to_owned()), "{makes:?}");
    // A bzip2 file that is not a tar archive is refused, as SPARQL Anything does.
    let root = tripletrail_testdir::tempdir("fx-bz2");
    std::fs::copy(dir().join("test.tbz2"), root.join("test.txt.bz2")).unwrap();
    let e = select_in(
        FxConfig {
            fixed_limits: false,
            databases: None,
            base: root.to_path_buf(),
            allowed: None,
            remote: Default::default(),
        },
        "SERVICE <x-sparql-anything:location=test.txt.bz2> { ?r rdf:_1 ?v }",
        "",
    )
    .unwrap_err();
    assert!(e.contains("bzip2"), "{e}");
}

#[test]
fn bzip2_tars_with_incompressible_blocks_and_several_streams_are_read_whole() {
    // A 150 kB random entry (as a photo or a PDF) makes bzip2 write a block that does not
    // compress, and pbzip2 and lbzip2 write several streams one after another.
    assert_eq!(
        names("incompressible.tar.bz2", None).unwrap(),
        ["photo.bin", "test.csv"]
    );
    let makes = select(
        "SERVICE <x-sparql-anything:location=test.csv,from-archive=incompressible.tar.bz2,csv.headers=true> {
            ?row xyz:Make ?v }",
    )
    .unwrap();
    assert!(makes.contains(&"Ford".to_owned()), "{makes:?}");
    assert_eq!(names("two-streams.tar.bz2", None).unwrap(), ENTRIES);
    let text = select(
        "SERVICE <x-sparql-anything:location=test/test.txt,from-archive=two-streams.tar.bz2> {
            ?r rdf:_1 ?v }",
    )
    .unwrap();
    assert_eq!(text, ["this is a test"]);
}

#[test]
fn zip_lists_its_entries() {
    assert_eq!(
        names("test.zip", None).unwrap(),
        ["test.csv", "test.json", "test.xml", "test.txt"]
    );
}

#[test]
fn matches_filters_names() {
    assert_eq!(
        names("test.tar", Some(".*(csv|txt)")).unwrap(),
        ["test/test.csv", "test/test.txt"]
    );
    let e = names("test.tar", Some(r".*(?<!\.json)")).unwrap_err();
    assert!(e.contains("archive.matches"), "{e}");
}

#[test]
fn folders_list_file_urls() {
    let base = std::fs::canonicalize(dir().join("folder")).unwrap();
    let base = format!("file://{}/", base.display());
    let mut names: Vec<String> = names("folder", None)
        .unwrap()
        .into_iter()
        .map(|n| n.replace(&base, ""))
        .collect();
    names.sort();
    assert_eq!(names, ["", "test.csv", "test.json", "test.txt", "test.xml"]);
}

#[test]
fn files_are_read_from_inside_archives() {
    // Each CSV file of the archive, triplified in a nested SERVICE.
    let makes = select(
        "SERVICE <x-sparql-anything:location=test.tar,archive.matches=.*csv> {
            ?root ?slot ?file . FILTER(isLiteral(?file))
            SERVICE <x-sparql-anything:> {
              fxe:properties fxe:location ?file ; fxe:from-archive \"test.tar\" ;
                fxe:csv.headers \"true\" .
              ?row xyz:Make ?v } }",
    )
    .unwrap();
    assert!(makes.contains(&"Ford".to_owned()), "{makes:?}");
    let text = select(
        "SERVICE <x-sparql-anything:location=test.txt,from-archive=test.zip> { ?r rdf:_1 ?v }",
    )
    .unwrap();
    assert_eq!(text, ["this is a test"]);
    let e = select(
        "SERVICE <x-sparql-anything:location=nope.txt,from-archive=test.zip> { ?r rdf:_1 ?v }",
    )
    .unwrap_err();
    assert!(e.contains("nope.txt"), "{e}");
}

#[cfg(unix)]
#[test]
fn folders_list_symbolic_links_as_themselves() {
    // base/ is allowed; outside/ is not, and base links into it.
    let root = tripletrail_testdir::tempdir("fx-links");
    let base = root.join("base");
    let outside = root.join("outside");
    std::fs::create_dir_all(base.join("sub")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.csv"), "a\n1\n").unwrap();
    std::fs::write(base.join("own.csv"), "a\n1\n").unwrap();
    std::os::unix::fs::symlink(outside.join("secret.csv"), base.join("link.csv")).unwrap();
    std::os::unix::fs::symlink(&outside, base.join("sub/linkdir")).unwrap();
    let config = || FxConfig {
        fixed_limits: false,
        databases: None,
        base: base.clone(),
        allowed: Some(vec![base.clone()]),
        remote: Default::default(),
    };
    let listed = select_in(
        config(),
        "SERVICE <x-sparql-anything:location=.> {
            ?root a fx:Root ; ?slot ?v . FILTER(?slot != rdf:type) }",
        "",
    )
    .unwrap();
    // Missing paths outside the allowed directory are denied like existing ones.
    let missing = select_in(
        config(),
        "SERVICE <x-sparql-anything:location=../outside/none.csv> { ?s ?p ?v }",
        "",
    )
    .unwrap_err();
    let existing = select_in(
        config(),
        "SERVICE <x-sparql-anything:location=../outside/secret.csv> { ?s ?p ?v }",
        "",
    )
    .unwrap_err();
    std::fs::remove_dir_all(&root).unwrap();
    assert_eq!(listed.len(), 5, "{listed:?}");
    assert!(listed.iter().all(|n| !n.contains("outside")), "{listed:?}");
    assert!(
        listed.iter().any(|n| n.ends_with("/base/link.csv")),
        "{listed:?}"
    );
    // A link to a directory is listed as a file, and not walked into.
    assert!(
        listed.iter().any(|n| n.ends_with("/base/sub/linkdir")),
        "{listed:?}"
    );
    assert!(missing.contains("outside the directories"), "{missing}");
    assert!(existing.contains("outside the directories"), "{existing}");
}
