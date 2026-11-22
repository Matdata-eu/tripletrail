//! `tripletrail query`, `update`, `dump` and `convert`: Jena's `arq`, `tdb2.tdbupdate`,
//! `tdb2.tdbdump` and `riot`, without a server.

use std::path::Path;
use std::process::Command;

fn scratch(name: &str) -> tripletrail_testdir::TestDir {
    let dir = tripletrail_testdir::tempdir(&format!("offline-{name}"));
    std::fs::write(
        dir.join("a.ttl"),
        "@prefix ex: <http://ex/> .\nex:a ex:name \"A\" ; ex:next ex:b .\nex:b ex:name \"B\" .\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("g.nq"),
        "<http://ex/c> <http://ex/name> \"C\" <http://ex/g> .\n",
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

/// Standard output, after checking the command succeeded.
fn ok(out: std::process::Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn query_runs_on_data_files() {
    let dir = scratch("query");
    let names = "SELECT ?n WHERE { ?s <http://ex/name> ?n } ORDER BY ?n";
    let out = ok(tripletrail(
        &dir,
        &[
            "query", "--data", "a.ttl", "--data", "g.nq", "--query", names,
        ],
    ));
    assert_eq!(out, "?n\n\"A\"\n\"B\"\n");
    let out = ok(tripletrail(
        &dir,
        &[
            "query",
            "--data",
            "a.ttl",
            "--data",
            "g.nq",
            "--union-default-graph",
            "--format",
            "csv",
            "--query",
            names,
        ],
    ));
    assert_eq!(out, "n\r\nA\r\nB\r\nC\r\n");
    std::fs::write(dir.join("q.rq"), "ASK { ?s ?p \"B\" }").unwrap();
    assert_eq!(
        ok(tripletrail(
            &dir,
            &["query", "--data", "a.ttl", "--query", "q.rq"]
        )),
        "true\n"
    );
    let out = ok(tripletrail(
        &dir,
        &[
            "query",
            "--data",
            "a.ttl",
            "--query",
            "CONSTRUCT WHERE { ?s <http://ex/next> ?o }",
            "--format",
            "nt",
        ],
    ));
    assert_eq!(out, "<http://ex/a> <http://ex/next> <http://ex/b> .\n");
    // Either a dataset on disk or files, not both, and not neither.
    assert!(
        !tripletrail(&dir, &["query", "--query", "ASK {}"])
            .status
            .success()
    );
    assert!(
        !tripletrail(
            &dir,
            &["query", "--location", "nowhere", "--query", "ASK {}"]
        )
        .status
        .success()
    );
}

#[test]
fn update_and_dump_a_dataset_on_disk() {
    let dir = scratch("update");
    ok(tripletrail(
        &dir,
        &["load", "--location", "db", "a.ttl", "g.nq"],
    ));
    ok(tripletrail(
        &dir,
        &[
            "update",
            "--location",
            "db",
            "--update",
            "DELETE WHERE { <http://ex/a> <http://ex/next> ?o }",
        ],
    ));
    let out = ok(tripletrail(&dir, &["dump", "--location", "db"]));
    let mut lines: Vec<_> = out.lines().collect();
    lines.sort_unstable();
    assert_eq!(
        lines,
        [
            "<http://ex/a> <http://ex/name> \"A\" .",
            "<http://ex/b> <http://ex/name> \"B\" .",
            "<http://ex/c> <http://ex/name> \"C\" <http://ex/g> .",
        ]
    );
    // Turtle holds one graph: refused, with nothing written, unless one is chosen.
    let refused = tripletrail(&dir, &["dump", "--location", "db", "--format", "ttl"]);
    assert!(!refused.status.success());
    assert!(refused.stdout.is_empty());
    let out = ok(tripletrail(
        &dir,
        &[
            "dump",
            "--location",
            "db",
            "--graph",
            "http://ex/g",
            "--format",
            "nt",
        ],
    ));
    assert_eq!(out, "<http://ex/c> <http://ex/name> \"C\" .\n");
    ok(tripletrail(
        &dir,
        &[
            "dump",
            "--location",
            "db",
            "--graph",
            "default",
            "--output",
            "default.nt",
        ],
    ));
    let default = std::fs::read_to_string(dir.join("default.nt")).unwrap();
    assert_eq!(default.lines().count(), 2);
}

#[test]
fn update_on_files_writes_the_dataset() {
    let dir = scratch("update-files");
    ok(tripletrail(
        &dir,
        &[
            "update",
            "--data",
            "a.ttl",
            "--update",
            "INSERT { ?s <http://ex/label> ?n } WHERE { ?s <http://ex/name> ?n }",
            "--output",
            "out.nt",
        ],
    ));
    let out = std::fs::read_to_string(dir.join("out.nt")).unwrap();
    assert_eq!(out.lines().count(), 5);
    assert!(out.contains("<http://ex/b> <http://ex/label> \"B\" ."));
    // A dataset on disk keeps its changes: there is nothing to write to.
    assert!(
        !tripletrail(
            &dir,
            &[
                "update",
                "--location",
                ".",
                "--update",
                "CLEAR ALL",
                "--output",
                "x.nq"
            ]
        )
        .status
        .success()
    );
}

#[test]
fn convert_validate_and_count() {
    let dir = scratch("convert");
    let out = ok(tripletrail(&dir, &["convert", "a.ttl", "g.nq"]));
    assert_eq!(out.lines().count(), 4);
    assert!(out.contains("<http://ex/c> <http://ex/name> \"C\" <http://ex/g> ."));
    ok(tripletrail(&dir, &["convert", "a.ttl", "--output", "a.nt"]));
    assert_eq!(
        std::fs::read_to_string(dir.join("a.nt"))
            .unwrap()
            .lines()
            .count(),
        3
    );
    let out = ok(tripletrail(&dir, &["convert", "--to", "turtle", "a.nt"]));
    assert!(
        out.contains("<http://ex/a> <http://ex/name> \"A\" ;"),
        "{out}"
    );
    // A file without an extension, with its format given.
    std::fs::copy(dir.join("a.nt"), dir.join("data")).unwrap();
    assert_eq!(
        ok(tripletrail(
            &dir,
            &["convert", "--syntax", "nt", "--count", "data"]
        )),
        "data: 3 triples\n"
    );
    std::fs::write(dir.join("bad.nt"), "<a> <b> .\n").unwrap();
    let out = tripletrail(&dir, &["convert", "--validate", "a.ttl", "bad.nt"]);
    assert!(!out.status.success());
    let out = String::from_utf8(out.stdout).unwrap();
    assert!(
        out.starts_with("a.ttl: valid, 3 triples\nbad.nt: "),
        "{out}"
    );
}

/// Blank node labels are scoped to a file, as Jena reads them: two files that both use `_:b1`
/// have two blank nodes.
#[test]
fn blank_nodes_of_different_files_stay_apart() {
    let dir = scratch("blank");
    std::fs::write(
        dir.join("one.ttl"),
        "@prefix : <http://ex/> .\n:a :p _:b1 . _:b1 :name \"file one\" .\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("two.ttl"),
        "@prefix : <http://ex/> .\n:c :p _:b1 . _:b1 :name \"file two\" .\n",
    )
    .unwrap();
    let names = "SELECT ?s ?n { ?s <http://ex/p> ?b . ?b <http://ex/name> ?n } ORDER BY ?s";
    for data in [
        &["--data", "one.ttl", "two.ttl"][..],
        &["--data", "one.ttl", "--data", "two.ttl"][..],
    ] {
        let mut args = vec!["query", "--format", "csv", "--query", names];
        args.extend_from_slice(data);
        assert_eq!(
            ok(tripletrail(&dir, &args)),
            "s,n\r\nhttp://ex/a,file one\r\nhttp://ex/c,file two\r\n"
        );
    }
    // Converted together, each file keeps its blank nodes: two of them, each with one name,
    // and the triples of one file still share theirs.
    let out = ok(tripletrail(
        &dir,
        &["convert", "--to", "nt", "one.ttl", "two.ttl"],
    ));
    let mut blanks: Vec<&str> = out
        .lines()
        .flat_map(|l| l.split(' '))
        .filter(|t| t.starts_with("_:"))
        .collect();
    blanks.sort_unstable();
    blanks.dedup();
    assert_eq!(blanks.len(), 2, "{out}");
    for b in &blanks {
        assert_eq!(out.matches(&format!("{b} ")).count(), 2, "{out}");
    }
    // One file alone keeps its labels.
    assert!(ok(tripletrail(&dir, &["convert", "--to", "nt", "one.ttl"])).contains("_:b1 "));
}

/// `convert`, `load`, `dump` and `query --data` read and write RDF/JSON (`.rj`) and TriX
/// (`.trix`), as `riot` does (#831).
#[test]
fn rdf_json_and_trix_on_the_command_line() {
    let dir = scratch("formats");
    ok(tripletrail(&dir, &["convert", "a.ttl", "--output", "a.rj"]));
    ok(tripletrail(
        &dir,
        &["convert", "a.ttl", "g.nq", "--output", "all.trix"],
    ));
    let rj: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("a.rj")).unwrap()).unwrap();
    assert_eq!(rj["http://ex/a"]["http://ex/name"][0]["value"], "A");
    ok(tripletrail(
        &dir,
        &["load", "--location", "db", "all.trix", "a.rj"],
    ));
    let out = ok(tripletrail(
        &dir,
        &["dump", "--location", "db", "--format", "nq"],
    ));
    let mut lines: Vec<_> = out.lines().collect();
    lines.sort_unstable();
    lines.dedup();
    assert_eq!(lines.len(), 4, "{out}");
    assert!(lines.contains(&"<http://ex/c> <http://ex/name> \"C\" <http://ex/g> ."));
    let names = "SELECT ?n WHERE { ?s <http://ex/name> ?n } ORDER BY ?n";
    assert_eq!(
        ok(tripletrail(
            &dir,
            &[
                "query", "--format", "csv", "--query", names, "--data", "a.rj"
            ]
        )),
        "n\r\nA\r\nB\r\n"
    );
    let trix = ok(tripletrail(
        &dir,
        &["dump", "--location", "db", "--format", "trix"],
    ));
    assert!(trix.contains("<uri>http://ex/g</uri>"), "{trix}");
    // Jena's binary RDF: written and read back the same.
    ok(tripletrail(
        &dir,
        &["convert", "a.ttl", "g.nq", "--output", "all.trdf"],
    ));
    ok(tripletrail(
        &dir,
        &["convert", "all.trdf", "--output", "all.prdf"],
    ));
    let back = ok(tripletrail(&dir, &["convert", "--to", "nq", "all.prdf"]));
    let mut back: Vec<_> = back.lines().collect();
    back.sort_unstable();
    let mut expected: Vec<_> = out.lines().collect();
    expected.sort_unstable();
    expected.dedup();
    assert_eq!(back, expected);
    // `--validate` reports a bad document as it does one in another format.
    std::fs::write(dir.join("bad.rj"), "[]").unwrap();
    let out = tripletrail(&dir, &["convert", "--validate", "bad.rj"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("not an object"));
}

#[test]
fn jena_rules_become_shacl_rules_that_infer_runs() {
    let dir = scratch("jena-rules");
    std::fs::write(
        dir.join("rules.txt"),
        "@prefix ex: <http://ex/> .\n\
         [transitive: (?a ex:next ?b), (?b ex:next ?c) -> (?a ex:next ?c)]\n\
         [label: (?x rdf:type ex:Station), (?x ex:code ?c), strConcat('ST-', ?c, ?l) -> (?x rdfs:label ?l)]\n\
         [fast: (?s ex:speed ?v), greaterThan(?v, 160), noValue(?s ex:slow 'yes') -> (?s rdf:type ex:Fast)]\n\
         [back: (?x ex:near ?y) <- (?y ex:near ?x)]\n\
         [skolem: (?a ex:p ?b) -> (?a ex:q f(?b))]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("data.ttl"),
        "@prefix ex: <http://ex/> .\n\
         ex:a ex:next ex:b . ex:b ex:next ex:c . ex:c ex:next ex:d .\n\
         ex:s1 a ex:Station ; ex:code \"OSL\" .\n\
         ex:t1 ex:speed 200 . ex:t2 ex:speed 200 ; ex:slow \"yes\" . ex:t3 ex:speed 100 .\n\
         ex:m ex:near ex:n .\n",
    )
    .unwrap();
    let out = tripletrail(&dir, &["import-jena-rules", "rules.txt"]);
    let shapes = ok(out.clone());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("4 rules translated, 1 left out"),
        "{stderr}"
    );
    assert!(stderr.contains("skolem:"), "{stderr}");
    std::fs::write(dir.join("rules.ttl"), shapes).unwrap();
    let inferred = ok(tripletrail(
        &dir,
        &["infer", "--data", "data.ttl", "--shapes", "rules.ttl"],
    ));
    let mut lines: Vec<_> = inferred.lines().collect();
    lines.sort_unstable();
    assert_eq!(
        lines,
        [
            "<http://ex/a> <http://ex/next> <http://ex/c> .",
            "<http://ex/a> <http://ex/next> <http://ex/d> .",
            "<http://ex/b> <http://ex/next> <http://ex/d> .",
            "<http://ex/n> <http://ex/near> <http://ex/m> .",
            "<http://ex/s1> <http://www.w3.org/2000/01/rdf-schema#label> \"ST-OSL\" .",
            "<http://ex/t1> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/Fast> .",
        ]
    );
    // A rule file that does not read names its line.
    std::fs::write(dir.join("bad.txt"), "[r: (?a ex:p ?b) -> (?a ex:q ?b)]").unwrap();
    let out = tripletrail(&dir, &["import-jena-rules", "bad.txt"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("line 1"));
}

/// `tripletrail load` gives each file its own blank nodes, as `tdb2.tdbloader` does (#840): two
/// files that use `_:b1`, loaded at once or one after the other, have two blank nodes.
#[test]
fn load_keeps_the_blank_nodes_of_each_file_apart() {
    let dir = scratch("load-blank");
    std::fs::write(
        dir.join("one.ttl"),
        "@prefix : <http://ex/> .\n:a :p _:b1 . _:b1 :name \"one\" .\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("two.nt"),
        "<http://ex/c> <http://ex/p> _:b1 .\n_:b1 <http://ex/name> \"two\" .\n",
    )
    .unwrap();
    let names = "SELECT ?s ?n { ?s <http://ex/p> ?b . ?b <http://ex/name> ?n } ORDER BY ?s";
    let expected = "s,n\r\nhttp://ex/a,one\r\nhttp://ex/c,two\r\n";
    let query = |db: &str| {
        ok(tripletrail(
            &dir,
            &[
                "query",
                "--location",
                db,
                "--format",
                "csv",
                "--query",
                names,
            ],
        ))
    };
    ok(tripletrail(
        &dir,
        &["load", "--location", "db1", "one.ttl", "two.nt"],
    ));
    assert_eq!(query("db1"), expected);
    ok(tripletrail(&dir, &["load", "--location", "db2", "one.ttl"]));
    ok(tripletrail(&dir, &["load", "--location", "db2", "two.nt"]));
    assert_eq!(query("db2"), expected);
}

/// A dump loaded again gets new blank nodes, and their labels keep their length (#840): a
/// backup restored or an ETL step that dumps and reloads does not lengthen them each time.
#[test]
fn loading_a_dump_again_does_not_lengthen_labels() {
    let dir = scratch("reload");
    std::fs::write(dir.join("r0.nq"), "_:b1 <http://ex/p> \"a\" .\n").unwrap();
    let mut labels = Vec::new();
    for i in 0..3 {
        let db = format!("rl{i}");
        ok(tripletrail(
            &dir,
            &["load", "--location", &db, &format!("r{i}.nq")],
        ));
        let next = format!("r{}.nq", i + 1);
        ok(tripletrail(
            &dir,
            &["dump", "--location", &db, "--output", &next],
        ));
        let text = std::fs::read_to_string(dir.join(&next)).unwrap();
        let label = text.split(' ').next().unwrap().to_owned();
        assert_eq!(label.len(), 2 + 32, "{text}");
        labels.push(label);
    }
    // Each load made a new node.
    assert_ne!(labels[0], labels[1]);
    assert_ne!(labels[1], labels[2]);
}

/// `tripletrail load` keeps WKT in binary form in a new dataset (#152), `--keep-wkt-text`
/// keeps the text, an existing dataset keeps its setting unless a flag changes it, and the
/// results show the same text either way.
#[test]
fn load_keeps_wkt_in_binary_form_in_a_new_dataset() {
    let dir = scratch("load-wkt");
    let wkt = "POINT(4.3357 50.8355)";
    std::fs::write(
        dir.join("p.nt"),
        format!(
            "<http://ex/p> <http://www.opengis.net/ont/geosparql#asWKT> \"{wkt}\"^^<http://www.opengis.net/ont/geosparql#wktLiteral> .\n"
        ),
    )
    .unwrap();
    let binary = |db: &str| dir.join(db).join("ds").join("render-wkt").is_file();
    let text = |db: &str| {
        ok(tripletrail(
            &dir,
            &[
                "query",
                "--location",
                db,
                "--format",
                "csv",
                "--query",
                "SELECT ?w { ?p <http://www.opengis.net/ont/geosparql#asWKT> ?w }",
            ],
        ))
    };
    ok(tripletrail(&dir, &["load", "--location", "on", "p.nt"]));
    assert!(binary("on"));
    ok(tripletrail(
        &dir,
        &["load", "--location", "off", "--keep-wkt-text", "p.nt"],
    ));
    assert!(!binary("off"));
    // An existing dataset keeps its setting, and a flag changes it.
    ok(tripletrail(&dir, &["load", "--location", "off", "a.ttl"]));
    assert!(!binary("off"));
    ok(tripletrail(
        &dir,
        &["load", "--location", "off", "--render-wkt", "a.ttl"],
    ));
    assert!(binary("off"));
    ok(tripletrail(
        &dir,
        &["load", "--location", "on", "--keep-wkt-text", "a.ttl"],
    ));
    assert!(!binary("on"));
    for db in ["on", "off"] {
        assert_eq!(text(db), format!("w\r\n{wkt}\r\n"), "{db}");
    }
    let both = tripletrail(
        &dir,
        &[
            "load",
            "--location",
            "x",
            "--render-wkt",
            "--keep-wkt-text",
            "p.nt",
        ],
    );
    assert!(!both.status.success());
}
