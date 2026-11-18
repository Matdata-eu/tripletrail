//! The W3C direct mapping (`tripletrail_rml::direct`) on PostgreSQL, in a schema of its own:
//!
//! ```sh
//! TRIPLETRAIL_TEST_POSTGRES=postgresql://postgres@localhost/postgres \
//!     cargo test -p tripletrail-rml --features postgres --test direct -- --ignored
//! ```
#![cfg(feature = "postgres")]

use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::Dataset;
use oxigraph::model::dataset::CanonicalizationAlgorithm;
use tripletrail_rml::{DatabaseLoader, Mapping, MemoryLoader, run};

const SCHEMA: &str = r#"
DROP SCHEMA IF EXISTS direct_test CASCADE;
CREATE SCHEMA direct_test;
SET search_path TO direct_test;
-- The example of the Direct Mapping recommendation (section 2).
CREATE TABLE "Addresses" ("ID" integer PRIMARY KEY, city text, state text);
CREATE TABLE "People" ("ID" integer PRIMARY KEY, fname text, addr integer REFERENCES "Addresses" ("ID"));
INSERT INTO "Addresses" VALUES (18, 'Cambridge', 'MA');
INSERT INTO "People" VALUES (7, 'Bob', 18), (8, 'Sue', NULL);
-- No primary key: one blank node per row, duplicates too.
CREATE TABLE "Log" (note text);
INSERT INTO "Log" VALUES ('x'), ('x');
-- A composite key with names to encode, and a foreign key to a unique column (a join).
CREATE TABLE "Track Section" (line text, "km from" numeric, code text UNIQUE, PRIMARY KEY (line, "km from"));
INSERT INTO "Track Section" VALUES ('L1/A', 1.50, 'S1');
CREATE TABLE "Signal" (id integer PRIMARY KEY, section text REFERENCES "Track Section" (code));
INSERT INTO "Signal" VALUES (1, 'S1');
-- A view (no row ids): one blank node per row.
CREATE VIEW "Notes" AS SELECT note FROM "Log";
-- A partitioned table without a key: each partition counts its row ids from the start.
CREATE TABLE "Reading" (part integer, v text) PARTITION BY LIST (part);
CREATE TABLE "Reading 1" PARTITION OF "Reading" FOR VALUES IN (1);
CREATE TABLE "Reading 2" PARTITION OF "Reading" FOR VALUES IN (2);
INSERT INTO "Reading" VALUES (1, 'a'), (2, 'b');
"#;

const EXPECTED: &str = r#"
@prefix b: <http://example.com/base/> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
<http://example.com/base/People/ID=7> a b:People ; <http://example.com/base/People#ID> 7 ;
    <http://example.com/base/People#fname> "Bob" ; <http://example.com/base/People#addr> 18 ;
    <http://example.com/base/People#ref-addr> <http://example.com/base/Addresses/ID=18> .
<http://example.com/base/People/ID=8> a b:People ; <http://example.com/base/People#ID> 8 ;
    <http://example.com/base/People#fname> "Sue" .
<http://example.com/base/Addresses/ID=18> a b:Addresses ; <http://example.com/base/Addresses#ID> 18 ;
    <http://example.com/base/Addresses#city> "Cambridge" ; <http://example.com/base/Addresses#state> "MA" .
[] a b:Log ; <http://example.com/base/Log#note> "x" .
[] a b:Log ; <http://example.com/base/Log#note> "x" .
<http://example.com/base/Track%20Section/line=L1%2FA;km%20from=1.5> a <http://example.com/base/Track%20Section> ;
    <http://example.com/base/Track%20Section#line> "L1/A" ;
    <http://example.com/base/Track%20Section#km%20from> 1.5 ;
    <http://example.com/base/Track%20Section#code> "S1" .
[] a b:Notes ; <http://example.com/base/Notes#note> "x" .
[] a b:Notes ; <http://example.com/base/Notes#note> "x" .
[] a b:Reading ; <http://example.com/base/Reading#part> 1 ; <http://example.com/base/Reading#v> "a" .
[] a b:Reading ; <http://example.com/base/Reading#part> 2 ; <http://example.com/base/Reading#v> "b" .
<http://example.com/base/Signal/id=1> a b:Signal ; <http://example.com/base/Signal#id> 1 ;
    <http://example.com/base/Signal#section> "S1" ;
    <http://example.com/base/Signal#ref-section> <http://example.com/base/Track%20Section/line=L1%2FA;km%20from=1.5> .
"#;

#[test]
#[ignore = "needs TRIPLETRAIL_TEST_POSTGRES (a database it may add a schema to)"]
fn the_direct_mapping_of_a_schema() {
    let url = std::env::var("TRIPLETRAIL_TEST_POSTGRES").expect("set TRIPLETRAIL_TEST_POSTGRES");
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    client.batch_execute(SCHEMA).unwrap();
    let loader = DatabaseLoader::new(MemoryLoader::default(), Some(url.clone()));
    let tables = loader.direct_tables("direct_test", &[]).unwrap();
    let text = tripletrail_rml::direct::r2rml(&tables, "http://example.com/base/");
    let quads = run(&Mapping::parse_turtle(&text, None).unwrap(), &loader).unwrap();
    let mut actual: Dataset = quads.into_iter().collect();
    let mut wanted = Dataset::new();
    for q in RdfParser::from_format(RdfFormat::Turtle).for_slice(EXPECTED.as_bytes()) {
        wanted.insert(&q.unwrap());
    }
    actual.canonicalize(CanonicalizationAlgorithm::Unstable);
    wanted.canonicalize(CanonicalizationAlgorithm::Unstable);
    let missing: Vec<_> = wanted.iter().filter(|q| !actual.contains(*q)).collect();
    let extra: Vec<_> = actual.iter().filter(|q| !wanted.contains(*q)).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "missing {missing:#?}\nextra {extra:#?}\n{text}"
    );

    // Some tables only; an unknown one is refused.
    let some = loader
        .direct_tables("direct_test", &["People".to_owned()])
        .unwrap();
    assert_eq!(some.len(), 1);
    assert!(
        loader
            .direct_tables("direct_test", &["Nope".to_owned()])
            .is_err()
    );
    client
        .batch_execute("DROP SCHEMA direct_test CASCADE")
        .unwrap();
}
