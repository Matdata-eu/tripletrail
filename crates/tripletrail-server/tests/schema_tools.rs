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
here=$(dirname "$0")
echo "$@" > "$here/args.txt"
echo "${TT_TEST_SECRET:-none}" > "$here/token.txt"
case "$(cat "$here/mode" 2>/dev/null)" in
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
        // The mode is read from a file beside the tool: the tool's environment is cleared.
        std::fs::write(dir.join("mode"), mode).unwrap();
        tripletrail(&args, &[("TT_TEST_SECRET", "secret")])
    };

    let (ok, _, err) = run(&["--date", "2026-11-26"], "");
    assert!(ok, "{err}");
    let said = std::fs::read_to_string(&args_file).unwrap();
    // The tool does not see the server's or the caller's secrets.
    assert_eq!(
        std::fs::read_to_string(dir.join("token.txt"))
            .unwrap()
            .trim(),
        "none"
    );
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
    let args_file = bin.join("args.txt");
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
            .env("TT_TEST_SECRET", "secret")
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
    assert_eq!(
        std::fs::read_to_string(bin.join("token.txt"))
            .unwrap()
            .trim(),
        "none"
    );
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

/// A schema of stations with tracks, in a namespace: an attribute, simple elements, an
/// enumeration and a repeated element of a named type.
const LIFT_XSD: &str = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:st="http://example.org/st"
    targetNamespace="http://example.org/st" elementFormDefault="qualified">
  <xs:element name="stations">
    <xs:complexType>
      <xs:sequence>
        <xs:element name="station" maxOccurs="unbounded">
          <xs:complexType>
            <xs:sequence>
              <xs:element name="name" type="xs:string"/>
              <xs:element name="kind" type="st:kind"/>
              <xs:element name="track" type="st:trackType" minOccurs="0" maxOccurs="unbounded"/>
            </xs:sequence>
            <xs:attribute name="code" type="xs:string" use="required"/>
          </xs:complexType>
        </xs:element>
      </xs:sequence>
    </xs:complexType>
  </xs:element>
  <xs:complexType name="trackType">
    <xs:sequence>
      <xs:element name="number" type="xs:positiveInteger"/>
    </xs:sequence>
  </xs:complexType>
  <xs:simpleType name="kind">
    <xs:restriction base="xs:string">
      <xs:enumeration value="main"/>
      <xs:enumeration value="halt"/>
    </xs:restriction>
  </xs:simpleType>
</xs:schema>
"#;

const LIFT_XML: &str = r#"<stations xmlns="http://example.org/st">
  <station code="A1"><name>Alpha</name><kind>main</kind><track><number>1</number></track><track><number>2</number></track></station>
  <station code="B2"><name>Beta</name><kind>halt</kind></station>
</stations>
"#;

/// What `xsd2rdf --source-provenance` writes for `LIFT_XSD` (read from its source; the real
/// tool's run is the test after this one): node shapes per element and type, property shapes
/// with `xsd2rdf:xmlName`, and the enumeration as SKOS concepts.
const LIFT_SHAPES: &str = r#"@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
@prefix skos: <http://www.w3.org/2004/02/skos/core#> .
@prefix ex: <https://data.example.org/stations/> .
@prefix ns: <https://data.example.org/stations/shapes/NodeShape/> .
@prefix ps: <https://data.example.org/stations/shapes/PropertyShape/> .
ns:stations a sh:NodeShape ; sh:nodeKind sh:IRI ; sh:targetClass ex:Stations ; sh:property ps:stations\/station .
ps:stations\/station a sh:PropertyShape ; sh:path ex:hasStation ; sh:nodeKind sh:IRI ; sh:class ex:Station ;
  sh:node ns:stations\/station ; sh:minCount 1 .
ns:stations\/station a sh:NodeShape ; sh:nodeKind sh:IRI ; sh:targetClass ex:Station ;
  sh:property ps:station\/code , ps:station\/name , ps:station\/kind , ps:station\/track .
ps:station\/code a sh:PropertyShape ; sh:path ex:code ; sh:datatype xsd:string ; sh:minCount 1 ; sh:maxCount 1 .
ps:station\/name a sh:PropertyShape ; sh:path ex:name ; sh:datatype xsd:string ; sh:minCount 1 ; sh:maxCount 1 .
ps:station\/kind a sh:PropertyShape ; sh:path ex:kind ; sh:nodeKind sh:IRI ; sh:class skos:Concept ; sh:minCount 1 ;
  sh:sparql [ a sh:SPARQLConstraint ; sh:select """PREFIX skos: <http://www.w3.org/2004/02/skos/core#>
    SELECT $this WHERE { $this $PATH ?concept . FILTER NOT EXISTS { ?concept skos:inScheme <https://data.example.org/stations/concepts/Kind> . } }""" ] .
ps:station\/track a sh:PropertyShape ; sh:path ex:track ; sh:nodeKind sh:IRI ; sh:node ns:trackType .
ns:trackType a sh:NodeShape ; sh:nodeKind sh:IRI ; sh:targetClass ex:TrackType ; sh:property ps:trackType\/number .
ps:trackType\/number a sh:PropertyShape ; sh:path ex:number ; sh:datatype xsd:positiveInteger ; sh:minCount 1 .
"#;

const LIFT_PROVENANCE: &str = r#"@prefix dct: <http://purl.org/dc/terms/> .
@prefix x: <https://w3id.org/xsd2rdf#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
@prefix ns: <https://data.example.org/stations/shapes/NodeShape/> .
@prefix ps: <https://data.example.org/stations/shapes/PropertyShape/> .
ns:stations dct:source "st.xsd#xmlns(st=http://example.org/st)xscd(/schemaElement::st:stations)"^^xsd:anyURI .
ns:stations\/station dct:source "st.xsd#xmlns(st=http://example.org/st)xscd(/schemaElement::st:stations/type::0/model::sequence/schemaElement::st:station)"^^xsd:anyURI .
ps:stations\/station x:xmlName "station" ; x:xmlNamespace "http://example.org/st"^^xsd:anyURI ; x:xmlNodeKind "element" .
ps:station\/code x:xmlName "code" ; x:xmlNodeKind "attribute" .
ps:station\/name x:xmlName "name" ; x:xmlNamespace "http://example.org/st"^^xsd:anyURI ; x:xmlNodeKind "element" .
ps:station\/kind x:xmlName "kind" ; x:xmlNamespace "http://example.org/st"^^xsd:anyURI ; x:xmlNodeKind "element" .
ps:station\/track x:xmlName "track" ; x:xmlNamespace "http://example.org/st"^^xsd:anyURI ; x:xmlNodeKind "element" .
ps:trackType\/number x:xmlName "number" ; x:xmlNamespace "http://example.org/st"^^xsd:anyURI ; x:xmlNodeKind "element" .
"#;

const LIFT_SKOS: &str = r#"@prefix skos: <http://www.w3.org/2004/02/skos/core#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
<https://data.example.org/stations/concepts/Kind> a skos:ConceptScheme .
<https://data.example.org/stations/concepts/Kind/main> a skos:Concept ; skos:notation "main"^^xsd:token ;
  skos:inScheme <https://data.example.org/stations/concepts/Kind> .
<https://data.example.org/stations/concepts/Kind/halt> a skos:Concept ; skos:notation "halt"^^xsd:token ;
  skos:inScheme <https://data.example.org/stations/concepts/Kind> .
"#;

/// Lifts `LIFT_XML` with the query `schema lift` makes of the schema folder, and checks the
/// data against the folder's shapes, with its concept schemes to look up.
fn lift_and_validate(schema: &Path) -> (String, bool, String) {
    let dir = schema.parent().unwrap();
    let query = dir.join("lift.rq");
    let (ok, _, err) = tripletrail(
        &[
            "schema",
            "lift",
            schema.to_str().unwrap(),
            "--namespace",
            NS,
            "--output",
            query.to_str().unwrap(),
        ],
        &[],
    );
    assert!(ok, "{err}");
    std::fs::write(dir.join("stations.xml"), LIFT_XML).unwrap();
    let data = dir.join("stations.nt");
    let location = format!("location={}", dir.join("stations.xml").display());
    let (ok, _, err) = tripletrail(
        &[
            "lift",
            query.to_str().unwrap(),
            "-v",
            &location,
            "--output",
            data.to_str().unwrap(),
        ],
        &[],
    );
    assert!(ok, "{err}");
    let (conforms, report, _) = tripletrail(
        &[
            "validate",
            "--data",
            data.to_str().unwrap(),
            "--shapes",
            schema.join("shapes.ttl").to_str().unwrap(),
            "--lookup",
            schema.join("skos.ttl").to_str().unwrap(),
        ],
        &[],
    );
    (std::fs::read_to_string(&data).unwrap(), conforms, report)
}

/// #277: the lift made from the shapes gives data the shapes accept, with no hand edits.
#[test]
fn the_lift_made_from_the_shapes_gives_data_they_accept() {
    let dir = tripletrail_testdir::tempdir("schema-lift");
    let schema = dir.join("schema");
    std::fs::create_dir_all(&schema).unwrap();
    for (name, text) in [
        ("shapes.ttl", LIFT_SHAPES),
        ("provenance.ttl", LIFT_PROVENANCE),
        ("skos.ttl", LIFT_SKOS),
    ] {
        std::fs::write(schema.join(name), text).unwrap();
    }
    let (data, conforms, report) = lift_and_validate(&schema);
    assert!(conforms, "{report}\n{data}");
    let count = |q: &str| {
        let e = Engine::new_tripletrail();
        e.load(RdfFormat::NTriples, data.as_bytes()).unwrap();
        match e
            .query(&format!(
                "PREFIX ex: <{NS}> SELECT (COUNT(*) AS ?n) {{ {q} }}"
            ))
            .unwrap()
        {
            QueryResults::Solutions(mut s) => {
                s.next().unwrap().unwrap().get("n").unwrap().to_string()
            }
            _ => unreachable!(),
        }
    };
    let n = |k: u32| format!("\"{k}\"^^<http://www.w3.org/2001/XMLSchema#integer>");
    assert_eq!(count("?s a ex:Stations ; ex:hasStation ?x"), n(2));
    assert_eq!(count("?s a ex:Station ; ex:code ?c ; ex:name ?m"), n(2));
    assert_eq!(
        count(
            "?s ex:code \"A1\" ; ex:kind <https://data.example.org/stations/concepts/Kind/main> ; ex:track ?t . ?t a ex:TrackType ; ex:number ?k"
        ),
        n(2)
    );
    assert_eq!(
        count("?t ex:number \"2\"^^<http://www.w3.org/2001/XMLSchema#positiveInteger>"),
        n(1)
    );
    // Another file of the schema gives other IRIs: they hash the file's location too.
    let other = dir.join("other.xml");
    std::fs::write(&other, LIFT_XML).unwrap();
    let (ok, other_data, err) = tripletrail(
        &[
            "lift",
            dir.join("lift.rq").to_str().unwrap(),
            "-v",
            &format!("location={}", other.display()),
        ],
        &[],
    );
    assert!(ok, "{err}");
    let subjects = |d: &str| -> std::collections::BTreeSet<String> {
        d.lines()
            .filter_map(|l| l.split(' ').next())
            .map(str::to_owned)
            .collect()
    };
    let (a, b) = (subjects(&data), subjects(&other_data));
    assert_eq!(a.len(), b.len());
    assert!(a.is_disjoint(&b), "{a:?}");
    // The same file gives the same IRIs: they hash each element's place in the document.
    let sorted = |d: &str| {
        let mut lines: Vec<_> = d.lines().map(str::to_owned).collect();
        lines.sort();
        lines
    };
    let query = std::fs::read_to_string(dir.join("lift.rq")).unwrap();
    assert_eq!(sorted(&lift_and_validate(&schema).0), sorted(&data));
    assert_eq!(std::fs::read_to_string(dir.join("lift.rq")).unwrap(), query);
    // Shapes without their provenance say where to get it.
    std::fs::write(schema.join("provenance.ttl"), "").unwrap();
    let (ok, _, err) = tripletrail(
        &[
            "schema",
            "lift",
            schema.to_str().unwrap(),
            "--namespace",
            NS,
        ],
        &[],
    );
    assert!(!ok && err.contains("tripletrail schema xsd"), "{err}");
}

/// The same with the real `xsd2rdf` (CI installs it): its shapes, provenance and concept
/// schemes give a lift whose data they accept.
#[test]
fn the_lift_made_from_xsd2rdf_shapes_gives_data_they_accept() {
    if !installed("xsd2rdf") {
        assert!(
            std::env::var_os("TRIPLETRAIL_SCHEMA_TOOLS_REQUIRED").is_none(),
            "xsd2rdf is not installed"
        );
        eprintln!("skipped: xsd2rdf is not installed");
        return;
    }
    let dir = tripletrail_testdir::tempdir("schema-lift-real");
    let xsd = dir.join("stations.xsd");
    std::fs::write(&xsd, LIFT_XSD).unwrap();
    let schema = dir.join("schema");
    let (ok, _, err) = tripletrail(
        &[
            "schema",
            "xsd",
            xsd.to_str().unwrap(),
            "--namespace",
            NS,
            "--out",
            schema.to_str().unwrap(),
        ],
        &[],
    );
    assert!(ok, "{err}");
    let (data, conforms, report) = lift_and_validate(&schema);
    assert!(conforms, "{report}\n{data}");
    assert!(data.contains("\"Alpha\""), "{data}");
}
