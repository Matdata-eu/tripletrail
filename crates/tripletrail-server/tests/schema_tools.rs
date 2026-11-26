//! `tripletrail schema xsd|jsonschema|openapi|asn1` (#276): a schema made into an ontology,
//! shapes and concept schemes by the optional Python tools, run in a subprocess.
//!
//! The tests with a stand-in tool always run. Those with the real tools run when the tools
//! are on the `PATH` (`pip install xsd2rdf jsonschema2rdf openapi2rdf asn2rdf`) and are
//! skipped otherwise; CI installs them in one job.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;

const NS: &str = "https://data.example.org/stations/";

fn tripletrail(args: &[&str], env: &[(&str, &str)]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_tripletrail"))
        .args(args)
        .envs(env.iter().copied())
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn ask(file: &Path, query: &str) -> bool {
    let engine = Engine::new_tripletrail();
    engine
        .load(RdfFormat::Turtle, std::fs::read(file).unwrap().as_slice())
        .unwrap();
    match engine.query(query).unwrap() {
        QueryResults::Boolean(b) => b,
        _ => unreachable!("an ASK"),
    }
}

/// A stand-in for the tools: writes what they write, says its arguments, and fails or hangs
/// when asked to.
const FAKE: &str = r#"#!/bin/sh
echo "$@" > "$FAKE_ARGS"
case "$FAKE_MODE" in
  fail) echo "Error: the schema has no root element" >&2; exit 1 ;;
  hang) exec sleep 30 ;;
esac
file="$2"; out="$4"; ns="$6"
base=$(basename "$file")
cat > "$out/$base.owl.ttl" <<EOF
<${ns}Station> a <http://www.w3.org/2002/07/owl#Class> .
EOF
cat > "$out/$base.shape.ttl" <<EOF
<${ns}shapes/Station> a <http://www.w3.org/ns/shacl#NodeShape> ;
  <http://www.w3.org/ns/shacl#property> _:b0 .
_:b0 <http://www.w3.org/ns/shacl#path> <${ns}name> .
EOF
cat > "$out/$base.skos.Kind.ttl" <<EOF
<${ns}concepts/Kind> a <http://www.w3.org/2004/02/skos/core#ConceptScheme> .
_:b0 <http://www.w3.org/2004/02/skos/core#prefLabel> "main" .
EOF
cat > "$out/$base.provenance.ttl" <<EOF
<${ns}shapes/Station> <http://purl.org/dc/terms/source> "station" .
EOF
echo "Warning: xs:any is not translated"
"#;

#[test]
fn a_tool_runs_in_a_folder_of_its_own_and_its_files_become_four() {
    let dir = tripletrail_testdir::tempdir("schema-tools");
    let tool = dir.join("fake2rdf");
    std::fs::write(&tool, FAKE).unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    let schema = dir.join("station.xsd");
    std::fs::write(
        &schema,
        "<xs:schema xmlns:xs='http://www.w3.org/2001/XMLSchema'/>",
    )
    .unwrap();
    let args_file = dir.join("args.txt");
    let out = dir.join("schema");
    let run = |extra: &[&str], mode: &str| {
        let mut args = vec![
            "schema",
            "xsd",
            schema.to_str().unwrap(),
            "--namespace",
            NS,
            "--tool-path",
            tool.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        tripletrail(
            &args,
            &[
                ("FAKE_ARGS", args_file.to_str().unwrap()),
                ("FAKE_MODE", mode),
            ],
        )
    };

    let (ok, _, err) = run(&["--date", "2026-11-26"], "");
    assert!(ok, "{err}");
    let said = std::fs::read_to_string(&args_file).unwrap();
    // Remote imports are not downloaded unless allowed; provenance is asked for.
    assert!(said.starts_with("convert "), "{said}");
    for flag in [
        "--namespace https://data.example.org/stations/",
        "--source-provenance",
        "--strict",
        "--no-download",
        "--date 2026-11-26",
    ] {
        assert!(said.contains(flag), "{flag}: {said}");
    }
    assert!(err.contains("Warning: xs:any is not translated"), "{err}");
    assert!(
        err.contains("1 ontology, 3 shape, 2 SKOS (1 schemes) and 1 provenance triples"),
        "{err}"
    );
    assert!(ask(
        &out.join("ontology.ttl"),
        &format!("ASK {{ <{NS}Station> a <http://www.w3.org/2002/07/owl#Class> }}")
    ));
    assert!(ask(
        &out.join("skos.ttl"),
        &format!(
            "ASK {{ <{NS}concepts/Kind> a <http://www.w3.org/2004/02/skos/core#ConceptScheme> }}"
        )
    ));
    assert!(ask(
        &out.join("provenance.ttl"),
        "ASK { ?s <http://purl.org/dc/terms/source> \"station\" }"
    ));
    // The blank nodes of one file are not those of another.
    assert!(ask(
        &out.join("shapes.ttl"),
        "ASK { ?b <http://www.w3.org/ns/shacl#path> ?p FILTER NOT EXISTS { ?b <http://www.w3.org/2004/02/skos/core#prefLabel> ?l } }"
    ));

    // Again: nothing changed, and that is said.
    let (ok, _, err) = run(&["--allow-download"], "");
    assert!(ok, "{err}");
    assert!(
        err.contains("ontology.ttl: 0 triples added, 0 removed"),
        "{err}"
    );
    assert!(
        !std::fs::read_to_string(&args_file)
            .unwrap()
            .contains("--no-download")
    );

    // A failure says what the tool said; a tool that hangs is stopped.
    let (ok, _, err) = run(&[], "fail");
    assert!(
        !ok && err.contains("the schema has no root element"),
        "{err}"
    );
    let (ok, _, err) = run(&["--timeout", "1"], "hang");
    assert!(!ok && err.contains("did not finish in 1s"), "{err}");

    // A missing tool says how to install it.
    let (ok, _, err) = tripletrail(
        &[
            "schema",
            "asn1",
            schema.to_str().unwrap(),
            "--namespace",
            NS,
            "--tool-path",
            dir.join("nothing").to_str().unwrap(),
        ],
        &[],
    );
    assert!(!ok && err.contains("pip install asn2rdf"), "{err}");
}

const XSD: &str = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" elementFormDefault="qualified">
  <xs:element name="station">
    <xs:complexType>
      <xs:sequence>
        <xs:element name="name" type="xs:string"/>
        <xs:element name="kind" type="kind"/>
        <xs:element name="platforms" type="xs:positiveInteger" minOccurs="0"/>
      </xs:sequence>
      <xs:attribute name="id" type="xs:string" use="required"/>
    </xs:complexType>
  </xs:element>
  <xs:simpleType name="kind">
    <xs:restriction base="xs:string">
      <xs:enumeration value="main"/>
      <xs:enumeration value="halt"/>
    </xs:restriction>
  </xs:simpleType>
</xs:schema>
"#;

const JSON_SCHEMA: &str = r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "Station",
  "type": "object",
  "properties": {
    "name": {"type": "string"},
    "kind": {"type": "string", "enum": ["main", "halt"]},
    "platforms": {"type": "integer", "minimum": 1}
  },
  "required": ["name", "kind"]
}
"#;

const OPENAPI: &str = r#"openapi: 3.0.3
info: {title: Stations, version: "1.0"}
paths: {}
components:
  schemas:
    Station:
      type: object
      required: [name, kind]
      properties:
        name: {type: string}
        kind: {type: string, enum: [main, halt]}
        platforms: {type: integer, minimum: 1}
"#;

const ASN1: &str = "Stations DEFINITIONS AUTOMATIC TAGS ::= BEGIN
Station ::= SEQUENCE {
  name UTF8String,
  kind Kind,
  platforms INTEGER (1..99) OPTIONAL
}
Kind ::= ENUMERATED { main, halt }
END
";

fn installed(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn the_tools_give_an_ontology_shapes_and_code_lists_that_load() {
    for (tool, program, file, text) in [
        ("xsd", "xsd2rdf", "station.xsd", XSD),
        (
            "jsonschema",
            "jsonschema2rdf",
            "station.schema.json",
            JSON_SCHEMA,
        ),
        ("openapi", "openapi2rdf", "stations.yaml", OPENAPI),
        ("asn1", "asn2rdf", "stations.asn", ASN1),
    ] {
        if !installed(program) {
            // CI installs them, and says so: there a missing tool fails.
            assert!(
                std::env::var_os("TRIPLETRAIL_SCHEMA_TOOLS_REQUIRED").is_none(),
                "{program} is not installed"
            );
            eprintln!("skipped: {program} is not installed");
            continue;
        }
        let dir = tripletrail_testdir::tempdir("schema-tools-real");
        let schema = dir.join(file);
        std::fs::write(&schema, text).unwrap();
        let out = dir.join("schema");
        // --strict in every run: the shapes pass SHACL-SHACL or the tool fails.
        let (ok, _, err) = tripletrail(
            &[
                "schema",
                tool,
                schema.to_str().unwrap(),
                "--namespace",
                NS,
                "--out",
                out.to_str().unwrap(),
            ],
            &[],
        );
        assert!(ok, "{tool}: {err}");
        assert!(
            ask(
                &out.join("ontology.ttl"),
                "ASK { ?c a <http://www.w3.org/2002/07/owl#Class> }"
            ),
            "{tool}: no class"
        );
        assert!(
            ask(
                &out.join("shapes.ttl"),
                "ASK { ?s a <http://www.w3.org/ns/shacl#NodeShape> ; <http://www.w3.org/ns/shacl#property> ?p }"
            ),
            "{tool}: no node shape"
        );
        assert!(
            ask(
                &out.join("skos.ttl"),
                "ASK { ?c <http://www.w3.org/2004/02/skos/core#inScheme> ?s }"
            ),
            "{tool}: no concept for the enumeration"
        );
        assert!(
            ask(
                &out.join("provenance.ttl"),
                "ASK { ?s <http://purl.org/dc/terms/source> ?o }"
            ),
            "{tool}: no provenance"
        );
    }
}

/// The Import page's way (#276): a server started with `--schema-tools` runs the tool found on
/// its `PATH` on an uploaded schema, answers what it made and, asked to, writes it to the
/// source's schema graphs; without the option it refuses.
#[test]
fn an_uploaded_schema_becomes_the_sources_schema_graphs() {
    let dir = tripletrail_testdir::tempdir("schema-tools-server");
    let bin = dir.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let tool = bin.join("xsd2rdf");
    std::fs::write(&tool, FAKE).unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    // The stand-in and the system's own programs only: not the real tools, where CI has them.
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let args_file = dir.join("args.txt");
    let serve = |extra: &[&str]| {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let child = Command::new(env!("CARGO_BIN_EXE_tripletrail"))
            .args([
                "serve",
                "--bind",
                &format!("127.0.0.1:{port}"),
                "--dataset",
                "ds",
                "--no-workspaces",
            ])
            .args(extra)
            .env("PATH", &path)
            .env("FAKE_ARGS", &args_file)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let base = format!("http://127.0.0.1:{port}");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while ureq::get(&format!("{base}/$/ready")).call().is_err() {
            assert!(std::time::Instant::now() < deadline, "not ready");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        (Server(child), base)
    };
    let post = |url: &str, body: &str| match ureq::post(url).send_string(body) {
        Ok(r) => (r.status(), r.into_string().unwrap()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap()),
        Err(e) => panic!("{e}"),
    };
    let xsd = "<xs:schema xmlns:xs='http://www.w3.org/2001/XMLSchema'/>";

    // Off by default.
    let (child, base) = serve(&[]);
    let (code, body) = post(&format!("{base}/ds/import?name=stations.xsd"), xsd);
    assert_eq!(code, 201, "{body}");
    let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (code, body) = post(&format!("{base}/ds/import/{id}/schema"), "");
    assert_eq!(code, 403, "{body}");
    assert!(body.contains("--schema-tools"), "{body}");
    drop(child);

    let (child, base) = serve(&["--schema-tools"]);
    let limits: serde_json::Value = serde_json::from_str(
        &ureq::get(&format!("{base}/ds/import"))
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(limits["schemaTools"], true);
    let (_, body) = post(&format!("{base}/ds/import?name=stations.xsd"), xsd);
    let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // A look first: what it makes, nothing written. The kind comes from the file's name.
    let (code, body) = post(&format!("{base}/ds/import/{id}/schema"), "{}");
    assert_eq!(code, 200, "{body}");
    let made: serde_json::Value = serde_json::from_str(&body).unwrap();
    let ns = "https://data.example.org/src/stations/";
    assert_eq!(made["kind"], "xsd");
    assert_eq!(made["namespace"], ns);
    assert_eq!(made["counts"]["shapes"], 3);
    assert_eq!(made["schemes"], serde_json::json!(["Kind"]));
    assert_eq!(
        made["warnings"],
        serde_json::json!(["xs:any is not translated"])
    );
    assert!(made["graphs"].is_null());
    let said = std::fs::read_to_string(&args_file).unwrap();
    assert!(said.contains("--no-download"), "{said}");
    let count = |graph: &str| -> String {
        ureq::get(&format!("{base}/ds/query"))
            .query(
                "query",
                &format!("SELECT (COUNT(*) AS ?n) {{ GRAPH <{graph}> {{ ?s ?p ?o }} }}"),
            )
            .set("Accept", "text/csv")
            .call()
            .unwrap()
            .into_string()
            .unwrap()
            .lines()
            .nth(1)
            .unwrap()
            .to_owned()
    };
    assert_eq!(count(&format!("{ns}schema/owl")), "0");
    // Written, twice: the graphs hold what the schema says, not two copies.
    for _ in 0..2 {
        let (code, body) = post(
            &format!("{base}/ds/import/{id}/schema"),
            r#"{"write": true}"#,
        );
        assert_eq!(code, 200, "{body}");
    }
    for (part, n) in [
        ("owl", "1"),
        ("shapes", "3"),
        ("skos", "2"),
        ("provenance", "1"),
    ] {
        assert_eq!(count(&format!("{ns}schema/{part}")), n, "{part}");
    }
    // An ASN.1 module uploads as text, and its tool is the one asked for.
    let (code, body) = post(
        &format!("{base}/ds/import?name=stations.asn"),
        "Stations DEFINITIONS ::= BEGIN END\n",
    );
    assert_eq!(code, 201, "{body}");
    // A file that is no schema, and a tool that is not there.
    let (_, body) = post(&format!("{base}/ds/import?name=a.csv"), "id\n1\n");
    let csv = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (code, body) = post(&format!("{base}/ds/import/{csv}/schema"), "{}");
    assert_eq!(code, 400, "{body}");
    let (code, body) = post(
        &format!("{base}/ds/import/{csv}/schema"),
        r#"{"kind": "asn1"}"#,
    );
    assert_eq!(code, 501, "{body}");
    assert!(body.contains("pip install asn2rdf"), "{body}");
    drop(child);
}

/// A server stopped when dropped, also when an assertion fails.
struct Server(std::process::Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
