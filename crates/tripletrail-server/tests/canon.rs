//! `tripletrail canon` and `tripletrail diff` on files and an on-disk dataset.

use std::path::Path;
use std::process::Command;

fn scratch(name: &str) -> tripletrail_testdir::TestDir {
    let dir = tripletrail_testdir::tempdir(&format!("canon-{name}"));
    std::fs::write(
        dir.join("a.nt"),
        "<http://ex/s> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/C> .\n\
         <http://ex/s> <http://ex/p> \"1\" .\n\
         <http://ex/s> <http://ex/geo> _:x .\n_:x <http://ex/wkt> \"POINT(1 2)\" .\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("same.ttl"),
        "@prefix ex: <http://ex/> .\nex:s a ex:C ; ex:p \"1\" ; ex:geo [ ex:wkt \"POINT(1 2)\" ] .\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("b.ttl"),
        "@prefix ex: <http://ex/> .\nex:s a ex:C ; ex:p \"2\" ; ex:geo [ ex:wkt \"POINT(1 2)\" ] .\n",
    )
    .unwrap();
    dir
}

fn tripletrail(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tripletrail"))
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap()
}

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

#[test]
fn isomorphic_files_and_a_dataset_have_one_digest() {
    let dir = scratch("digest");
    let a = tripletrail(&dir, &["canon", "--digest", "a.nt"]);
    assert!(a.status.success(), "{}", String::from_utf8_lossy(&a.stderr));
    let same = tripletrail(&dir, &["canon", "--digest", "same.ttl"]);
    assert_eq!(stdout(&a), stdout(&same));
    assert!(stdout(&a).starts_with("sha256:"));
    let b = tripletrail(&dir, &["canon", "--digest", "b.ttl"]);
    assert_ne!(stdout(&a), stdout(&b));

    let full = tripletrail(&dir, &["canon", "same.ttl"]);
    assert!(stdout(&full).contains("<http://ex/s> <http://ex/geo> _:c14n0 .\n"));

    let load = tripletrail(
        &dir,
        &["load", "--location", "db", "--dataset", "rel", "a.nt"],
    );
    assert!(
        load.status.success(),
        "{}",
        String::from_utf8_lossy(&load.stderr)
    );
    let ds = tripletrail(&dir, &["canon", "--digest", "db/rel"]);
    assert!(
        ds.status.success(),
        "{}",
        String::from_utf8_lossy(&ds.stderr)
    );
    assert_eq!(stdout(&a), stdout(&ds));
}

#[test]
fn a_diff_gives_a_summary_a_patch_and_the_exit_status() {
    let dir = scratch("diff");
    let same = tripletrail(&dir, &["diff", "a.nt", "same.ttl"]);
    assert_eq!(same.status.code(), Some(0));
    let out = tripletrail(
        &dir,
        &[
            "diff",
            "a.nt",
            "b.ttl",
            "--added",
            "added.nq",
            "--removed",
            "removed.nq",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(json["added"], 1);
    assert_eq!(json["removed"], 1);
    assert_eq!(json["classes"][0]["iri"], "http://ex/C");
    assert_eq!(
        std::fs::read_to_string(dir.join("added.nq")).unwrap(),
        "<http://ex/s> <http://ex/p> \"2\" .\n"
    );
    let patch = tripletrail(&dir, &["diff", "--format", "rdf-patch", "a.nt", "b.ttl"]);
    assert_eq!(
        stdout(&patch),
        "D <http://ex/s> <http://ex/p> \"1\" .\nA <http://ex/s> <http://ex/p> \"2\" .\n"
    );
}
