//! `tripletrail lift --as-sparql`: the query written for a mapping gives the mapping's triples.
//!
//! The RML-Core and RML-IO cases of the W3C KG-Construct group are run from checkouts (CC BY
//! 4.0, not copied here), each case that `--as-sparql` writes as a query compared with the
//! case's expected output:
//!
//! ```sh
//! for r in rml-core rml-io; do git clone --depth 1 https://github.com/kg-construct/$r /tmp/$r; done
//! RML_CORE_TESTS=/tmp/rml-core/test-cases RML_IO_TESTS=/tmp/rml-io/test-cases \
//!     cargo test -p tripletrail-server --test as_sparql -- --ignored --nocapture
//! ```

use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::Dataset;
use oxigraph::model::dataset::CanonicalizationAlgorithm;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tripletrail(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tripletrail"))
        .current_dir(dir)
        .arg("lift")
        .args(args)
        .output()
        .unwrap()
}

fn scratch(name: &str) -> tripletrail_testdir::TestDir {
    tripletrail_testdir::tempdir(&format!("as-sparql-{name}"))
}

fn read(path: &Path, format: RdfFormat) -> Dataset {
    let text = std::fs::read(path).unwrap();
    let mut d: Dataset = RdfParser::from_format(format)
        .for_slice(&text)
        .map(|q| q.unwrap())
        .collect();
    d.canonicalize(CanonicalizationAlgorithm::Unstable);
    d
}

/// The mapping and its query give the same triples.
fn same_triples(dir: &Path, mapping: &str) {
    let mapped = tripletrail(dir, &[mapping, "--output", "mapped.nt"]);
    assert!(
        mapped.status.success(),
        "{}",
        String::from_utf8_lossy(&mapped.stderr)
    );
    let written = tripletrail(dir, &[mapping, "--as-sparql", "--output", "query.rq"]);
    assert!(
        written.status.success(),
        "{}",
        String::from_utf8_lossy(&written.stderr)
    );
    let queried = tripletrail(dir, &["query.rq", "--output", "queried.nt"]);
    assert!(
        queried.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&queried.stderr),
        std::fs::read_to_string(dir.join("query.rq")).unwrap()
    );
    let a = read(&dir.join("mapped.nt"), RdfFormat::NTriples);
    let b = read(&dir.join("queried.nt"), RdfFormat::NTriples);
    assert!(
        a == b && !a.is_empty(),
        "mapping:\n{}\nquery:\n{}\n{}",
        std::fs::read_to_string(dir.join("mapped.nt")).unwrap(),
        std::fs::read_to_string(dir.join("queried.nt")).unwrap(),
        std::fs::read_to_string(dir.join("query.rq")).unwrap()
    );
}

#[test]
fn csv_and_json_mappings_with_joins() {
    let dir = scratch("joins");
    std::fs::write(
        dir.join("stations.csv"),
        "code,name,line,note\nGNT,Gent-Sint-Pieters,50,\nBRG,Brugge,50,coast\nLV,Leuven,36,\nNUL,,99,\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("lines.json"),
        r#"{"lines": [{"id": 50, "name": "Brussel - Gent", "km": 64.3, "electrified": true, "tags": ["main", "IC"]},
                       {"id": 36, "name": "Brussel - Leuven", "km": 30, "electrified": true, "tags": []}]}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("network.rml.ttl"),
        r#"@prefix rr: <http://www.w3.org/ns/r2rml#> .
@prefix rml: <http://semweb.mmlab.be/ns/rml#> .
@prefix ql: <http://semweb.mmlab.be/ns/ql#> .
@prefix ex: <http://ex.org/> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
<#Station> rml:logicalSource [ rml:source "stations.csv" ; rml:referenceFormulation ql:CSV ] ;
  rr:subjectMap [ rr:template "http://ex.org/station/{name}" ; rr:class ex:Station ] ;
  rr:predicateObjectMap [ rr:predicate ex:code ; rr:objectMap [ rml:reference "code" ] ] ,
    [ rr:predicate ex:label ; rr:objectMap [ rml:reference "name" ; rr:language "nl" ] ] ,
    [ rr:predicate ex:note ; rr:objectMap [ rr:template "note: {note}" ; rr:termType rr:Literal ] ] ,
    [ rr:predicate ex:self ; rr:objectMap [ rr:parentTriplesMap <#Station> ] ] ,
    [ rr:predicate ex:line ; rr:objectMap [ rr:parentTriplesMap <#Line> ;
        rr:joinCondition [ rr:child "line" ; rr:parent "id" ] ] ] ,
    [ rr:predicate ex:place ; rr:objectMap [ rr:template "{code}-place" ; rr:termType rr:BlankNode ] ] .
<#Line> rml:logicalSource [ rml:source "lines.json" ; rml:referenceFormulation ql:JSONPath ;
    rml:iterator "$.lines[*]" ] ;
  rr:subjectMap [ rr:template "http://ex.org/line/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate ex:name ; rr:objectMap [ rml:reference "name" ] ] ,
    [ rr:predicate ex:km ; rr:objectMap [ rml:reference "km" ] ] ,
    [ rr:predicate ex:length ; rr:objectMap [ rml:reference "km" ; rr:datatype xsd:decimal ] ] ,
    [ rr:predicate ex:electrified ; rr:objectMap [ rml:reference "electrified" ] ] ,
    [ rr:predicate ex:tag ; rr:objectMap [ rml:reference "tags[*]" ] ] ,
    [ rr:predicate ex:first ; rr:objectMap [ rml:reference "$.tags[0]" ] ] ,
    [ rr:predicate ex:page ; rr:objectMap [ rr:template "https://ex.org/pages/{name}" ; rr:termType rr:IRI ] ] .
"#,
    )
    .unwrap();
    same_triples(&dir, "network.rml.ttl");
}

#[test]
fn what_cannot_be_written_is_refused() {
    let dir = scratch("refused");
    std::fs::write(
        dir.join("x.rml.ttl"),
        r#"@prefix rr: <http://www.w3.org/ns/r2rml#> .
@prefix rml: <http://semweb.mmlab.be/ns/rml#> .
@prefix ql: <http://semweb.mmlab.be/ns/ql#> .
<#X> rml:logicalSource [ rml:source "x.xml" ; rml:referenceFormulation ql:XPath ; rml:iterator "/a" ] ;
  rr:subjectMap [ rr:template "http://ex.org/{@id}" ] ."#,
    )
    .unwrap();
    let out = tripletrail(&dir, &["x.rml.ttl", "--as-sparql"]);
    assert!(!out.status.success());
    let e = String::from_utf8_lossy(&out.stderr);
    assert!(
        e.contains("cannot be written as SPARQL: XML sources"),
        "{e}"
    );
}

/// The conformance cases `--as-sparql` writes as a query give the triples their mapping gives
/// (with the same base IRI, the mapping file's; the cases' own outputs are checked by
/// tripletrail-rml's conformance test).
#[test]
#[ignore = "needs RML_CORE_TESTS / RML_IO_TESTS checkouts"]
fn conformance_cases() {
    let mut written = 0;
    let mut failed = Vec::new();
    for var in ["RML_CORE_TESTS", "RML_IO_TESTS"] {
        let Ok(root) = std::env::var(var) else {
            continue;
        };
        let mut cases: Vec<PathBuf> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.join("mapping.ttl").exists())
            .collect();
        cases.sort();
        for case in cases {
            let name = case.file_name().unwrap().to_string_lossy().into_owned();
            let work = scratch("case");
            let path = |f: &str| work.join(f).to_str().unwrap().to_owned();
            std::fs::copy(case.join("mapping.ttl"), work.join("m.rml.ttl")).unwrap();
            let query = tripletrail(
                &case,
                &[&path("m.rml.ttl"), "--as-sparql", "--output", &path("q.rq")],
            );
            let mapped = tripletrail(&case, &[&path("m.rml.ttl"), "--output", &path("mapped.nt")]);
            if !query.status.success() || !mapped.status.success() {
                let _ = std::fs::remove_dir_all(&work);
                continue;
            }
            written += 1;
            let run = tripletrail(&case, &[&path("q.rq"), "--output", &path("queried.nt")]);
            if !run.status.success() {
                failed.push(format!("{name}: {}", String::from_utf8_lossy(&run.stderr)));
                continue;
            }
            let want = read(&work.join("mapped.nt"), RdfFormat::NTriples);
            let got = read(&work.join("queried.nt"), RdfFormat::NTriples);
            if got != want {
                failed.push(format!(
                    "{name}: query gives\n{}\nmapping gives\n{}\n{}",
                    std::fs::read_to_string(work.join("queried.nt")).unwrap(),
                    std::fs::read_to_string(work.join("mapped.nt")).unwrap(),
                    std::fs::read_to_string(work.join("q.rq")).unwrap()
                ));
            }
            let _ = std::fs::remove_dir_all(&work);
        }
    }
    eprintln!("{written} cases written as SPARQL, {} differ", failed.len());
    assert!(written > 0, "no cases: set RML_CORE_TESTS or RML_IO_TESTS");
    assert!(failed.is_empty(), "{}", failed.join("\n\n"));
}
