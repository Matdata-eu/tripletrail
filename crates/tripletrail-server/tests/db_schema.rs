//! `tripletrail schema database` (#278): a PostgreSQL schema's comments, keys and constraints
//! as an OWL ontology, SHACL shapes and SKOS concept schemes, which the direct mapping's data
//! conforms to.
//!
//! ```sh
//! TRIPLETRAIL_TEST_POSTGRES=postgresql://postgres@localhost/postgres \
//!     cargo test -p tripletrail-server --test db_schema -- --ignored
//! ```
#![cfg(feature = "postgres")]

use std::path::Path;
use std::process::Command;
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;

const SCHEMA: &str = r#"
DROP SCHEMA IF EXISTS schema_test CASCADE;
CREATE SCHEMA schema_test;
SET search_path TO schema_test;
CREATE TYPE line_kind AS ENUM ('main', 'branch', 'freight');
CREATE DOMAIN speed AS integer CHECK (VALUE > 0 AND VALUE <= 350);
CREATE TABLE country (code char(2) PRIMARY KEY, name text NOT NULL);
COMMENT ON TABLE country IS 'Countries, a code table';
CREATE TABLE line (
  id integer PRIMARY KEY,
  name varchar(80) NOT NULL UNIQUE,
  kind line_kind NOT NULL,
  max_speed speed,
  opened date CHECK (opened >= '1830-01-01'),
  code text CHECK (code ~ '^L[0-9]+$'),
  status text CHECK (status IN ('open', 'closed')),
  country char(2) REFERENCES country (code));
COMMENT ON TABLE line IS 'Railway lines';
COMMENT ON COLUMN line.max_speed IS 'Highest speed allowed, in km/h';
CREATE TABLE section (
  line_id integer NOT NULL REFERENCES line (id),
  seq integer NOT NULL,
  km_from numeric NOT NULL,
  km_to numeric NOT NULL,
  PRIMARY KEY (line_id, seq),
  CHECK (km_to > km_from));
INSERT INTO country VALUES ('BE', 'Belgium'), ('NL', 'Netherlands');
INSERT INTO line VALUES (36, 'Brussels - Liège', 'main', 300, '1866-10-01', 'L36', 'open', 'BE'),
  (50, 'Brussels - Ghent', 'main', 160, NULL, 'L50', NULL, 'BE');
INSERT INTO section VALUES (36, 1, 0, 30.5), (36, 2, 30.5, 95.2);
"#;

const NS: &str = "https://data.example.org/db/";

fn tripletrail(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_tripletrail"))
        .args(args)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn ask(files: &[&Path], query: &str) -> bool {
    let engine = Engine::new_tripletrail();
    for f in files {
        engine
            .load(RdfFormat::Turtle, std::fs::read(f).unwrap().as_slice())
            .unwrap();
    }
    match engine.query(query).unwrap() {
        QueryResults::Boolean(b) => b,
        _ => unreachable!("an ASK"),
    }
}

#[test]
#[ignore = "needs TRIPLETRAIL_TEST_POSTGRES (a database it may add a schema to)"]
fn a_schema_gives_an_ontology_and_shapes_its_direct_mapping_conforms_to() {
    let url = std::env::var("TRIPLETRAIL_TEST_POSTGRES").expect("set TRIPLETRAIL_TEST_POSTGRES");
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    client.batch_execute(SCHEMA).unwrap();
    let dir = tripletrail_testdir::tempdir("db-schema");
    let out = dir.join("schema");
    let out_s = out.to_str().unwrap();
    let base = [
        "schema",
        "database",
        "--database",
        &url,
        "--schema",
        "schema_test",
        "--namespace",
        NS,
        "--code-table",
        "country",
        "--out",
        out_s,
    ];
    let (ok, _, err) = tripletrail(&base);
    assert!(ok, "{err}");
    // A CHECK of two columns is said, not translated.
    assert!(
        err.contains("schema_test.section: CHECK ((km_to > km_from)) is not translated"),
        "{err}"
    );
    let ontology = out.join("ontology.ttl");
    let shapes = out.join("shapes.ttl");
    let skos = out.join("skos.ttl");
    assert!(ask(
        &[&ontology],
        &format!(
            "ASK {{ <{NS}line> a <http://www.w3.org/2002/07/owl#Class> ; \
             <http://www.w3.org/2000/01/rdf-schema#comment> \"Railway lines\" . \
             <{NS}line#max_speed> <http://www.w3.org/2000/01/rdf-schema#comment> \"Highest speed allowed, in km/h\" ; \
             <http://www.w3.org/2000/01/rdf-schema#range> <http://www.w3.org/2001/XMLSchema#integer> . \
             <{NS}section#ref-line_id> <http://www.w3.org/2000/01/rdf-schema#range> <{NS}line> }}"
        )
    ));
    let sh = "http://www.w3.org/ns/shacl#";
    let s = format!("{NS}schema/shapes/line#");
    for (shape, constraint) in [
        (
            "kind",
            format!("<{sh}in> (\"main\" \"branch\" \"freight\")"),
        ),
        ("kind", format!("<{sh}minCount> 1")),
        ("name", format!("<{sh}maxLength> 80")),
        (
            "name",
            format!("<http://datashapes.org/dash#uniqueValueForClass> <{NS}line>"),
        ),
        (
            "max_speed",
            format!("<{sh}minExclusive> 0 ; <{sh}maxInclusive> 350"),
        ),
        (
            "opened",
            format!("<{sh}minInclusive> \"1830-01-01\"^^<http://www.w3.org/2001/XMLSchema#date>"),
        ),
        ("code", format!("<{sh}pattern> \"^L[0-9]+$\"")),
        ("status", format!("<{sh}in> (\"open\" \"closed\")")),
        (
            "ref-country",
            format!("<{sh}class> <{NS}country> ; <{sh}nodeKind> <{sh}IRI>"),
        ),
    ] {
        assert!(
            ask(&[&shapes], &format!("ASK {{ <{s}{shape}> {constraint} }}")),
            "{shape}: {constraint}\n{}",
            std::fs::read_to_string(&shapes).unwrap()
        );
    }
    assert!(ask(
        &[&skos],
        &format!(
            "ASK {{ <{NS}country/code=BE> a <http://www.w3.org/2004/02/skos/core#Concept> ; \
             <http://www.w3.org/2004/02/skos/core#prefLabel> \"Belgium\" ; \
             <http://www.w3.org/2004/02/skos/core#inScheme> <{NS}schema/skos/country> }}"
        )
    ));

    // The direct mapping's data conforms to the shapes.
    let data = dir.join("data.nt");
    let (ok, _, err) = tripletrail(&[
        "direct-mapping",
        "--database",
        &url,
        "--schema",
        "schema_test",
        "--base-iri",
        NS,
        "--output",
        data.to_str().unwrap(),
    ]);
    assert!(ok, "{err}");
    let (ok, report, err) = tripletrail(&[
        "validate",
        "--data",
        data.to_str().unwrap(),
        "--shapes",
        shapes.to_str().unwrap(),
    ]);
    assert!(ok, "{report}\n{err}");
    // A speed over the domain's CHECK does not.
    let bad = dir.join("bad.ttl");
    std::fs::write(
        &bad,
        format!(
            "<{NS}line/id=1> a <{NS}line> ; <{NS}line#id> 1 ; <{NS}line#name> \"x\" ; \
             <{NS}line#kind> \"main\" ; <{NS}line#max_speed> 400 .\n"
        ),
    )
    .unwrap();
    let (ok, report, _) = tripletrail(&[
        "validate",
        "--data",
        bad.to_str().unwrap(),
        "--shapes",
        shapes.to_str().unwrap(),
    ]);
    assert!(
        !ok && report.contains("MaxInclusiveConstraintComponent"),
        "{report}"
    );

    // Run again after a change: what changed is said.
    client
        .batch_execute("COMMENT ON TABLE schema_test.line IS 'Railway lines of the network'")
        .unwrap();
    let (ok, _, err) = tripletrail(&base);
    assert!(ok, "{err}");
    assert!(
        err.contains("ontology.ttl: 1 triples added, 1 removed"),
        "{err}"
    );
    assert!(
        err.contains("skos.ttl: 0 triples added, 0 removed"),
        "{err}"
    );
}
