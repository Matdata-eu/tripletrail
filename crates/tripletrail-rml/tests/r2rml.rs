//! The W3C R2RML test cases on PostgreSQL, run from a checkout of
//! <https://github.com/kg-construct/r2rml-test-cases-support> (W3C test suite licence, not
//! copied here) against a database the test may empty:
//!
//! ```sh
//! git clone --depth 1 https://github.com/kg-construct/r2rml-test-cases-support /tmp/r2rml
//! R2RML_TESTS=/tmp/r2rml TRIPLETRAIL_TEST_POSTGRES=postgresql://postgres@localhost/postgres \
//!     cargo test -p tripletrail-rml --features postgres --test r2rml -- --ignored --nocapture
//! ```
//!
//! Each case loads its database script into the `public` schema, which it drops first, and
//! runs `r2rml<x>.ttl`. It passes when the output is isomorphic to `mapped<x>.nq`, or, for a
//! case without one, when the mapping is rejected.
#![cfg(feature = "postgres")]

use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::Dataset;
use oxigraph::model::dataset::CanonicalizationAlgorithm;
use std::path::Path;
use tripletrail_rml::{DatabaseLoader, Mapping, MemoryLoader, run};

/// The tests share the database: one at a time.
static DATABASE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Cases that fail for a stated reason.
const KNOWN: &[(&str, &str)] = &[(
    "R2RMLTC0002f",
    "expects {Name} not to read the column \"Name\" (SQL folds it to NAME), while \
     R2RMLTC0011a expects {FirstName} to read \"FirstName\": names are matched as written",
)];

fn case(url: &str, root: &Path, dir: &Path, mapping: &Path) -> Result<(), String> {
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();
    // R2RMLTC0002a reads d002.
    let number: String = name["R2RMLTC".len()..].chars().take(4).collect();
    let databases = root.join("databases");
    let script = [
        format!("d{}-postgresql.sql", &number[1..]),
        format!("d{}.sql", &number[1..]),
    ]
    .into_iter()
    .map(|f| databases.join(f))
    .find(|p| p.exists())
    .ok_or("no database script")?;
    let mut client = postgres::Client::connect(url, postgres::NoTls).map_err(|e| e.to_string())?;
    client
        .batch_execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .map_err(|e| e.to_string())?;
    client
        .batch_execute(&std::fs::read_to_string(&script).map_err(|e| e.to_string())?)
        .map_err(|e| format!("{}: {e}", script.display()))?;
    drop(client);
    let suffix = mapping
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .trim_start_matches("r2rml")
        .to_owned();
    let expected = dir.join(format!("mapped{suffix}.nq"));
    let loader = DatabaseLoader::new(MemoryLoader::default(), Some(url.to_owned()));
    let text = std::fs::read_to_string(mapping).map_err(|e| e.to_string())?;
    let made = Mapping::parse_turtle(&text, Some("http://example.com/base/"))
        .and_then(|m| run(&m, &loader));
    if !expected.exists() {
        return match made {
            Err(_) => Ok(()),
            Ok(quads) => Err(format!("expected an error, made {} quads", quads.len())),
        };
    }
    let quads = made.map_err(|e| e.to_string())?;
    let mut actual: Dataset = quads.into_iter().collect();
    let mut wanted = Dataset::new();
    let text = std::fs::read(&expected).map_err(|e| e.to_string())?;
    for q in RdfParser::from_format(RdfFormat::NQuads).for_slice(&text) {
        wanted.insert(&q.map_err(|e| e.to_string())?);
    }
    actual.canonicalize(CanonicalizationAlgorithm::Unstable);
    wanted.canonicalize(CanonicalizationAlgorithm::Unstable);
    if actual == wanted {
        return Ok(());
    }
    let missing: Vec<String> = wanted
        .iter()
        .filter(|q| !actual.contains(*q))
        .take(3)
        .map(|q| q.to_string())
        .collect();
    let extra: Vec<String> = actual
        .iter()
        .filter(|q| !wanted.contains(*q))
        .take(3)
        .map(|q| q.to_string())
        .collect();
    Err(format!("missing {missing:?}, extra {extra:?}"))
}

#[test]
#[ignore = "needs R2RML_TESTS and TRIPLETRAIL_TEST_POSTGRES (a database it may empty)"]
fn r2rml_test_cases() {
    let (Some(root), Ok(url)) = (
        std::env::var_os("R2RML_TESTS"),
        std::env::var("TRIPLETRAIL_TEST_POSTGRES"),
    ) else {
        panic!("set R2RML_TESTS and TRIPLETRAIL_TEST_POSTGRES");
    };
    let _database = DATABASE.lock().unwrap_or_else(|e| e.into_inner());
    let root = Path::new(&root);
    let mut cases = Vec::new();
    for e in std::fs::read_dir(root).unwrap() {
        let dir = e.unwrap().path();
        if !dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("R2RMLTC")
        {
            continue;
        }
        for m in std::fs::read_dir(&dir).unwrap() {
            let m = m.unwrap().path();
            let file = m.file_name().unwrap().to_string_lossy().into_owned();
            // `r2rmlb-mysql.ttl`: MySQL's variant of `r2rmlb.ttl`.
            if file.starts_with("r2rml") && file.ends_with(".ttl") && !file.contains('-') {
                cases.push((dir.clone(), m));
            }
        }
    }
    cases.sort();
    let (mut passed, mut unexpected) = (0, Vec::new());
    for (dir, mapping) in &cases {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let known = KNOWN.iter().find(|(n, _)| *n == name);
        match (case(&url, root, dir, mapping), known) {
            (Ok(()), None) => passed += 1,
            (Ok(()), Some(_)) => unexpected.push(format!("{name}: passes, remove it from KNOWN")),
            (Err(_), Some(_)) => {}
            (Err(e), None) => unexpected.push(format!("{name}: {e}")),
        }
    }
    println!("{passed} of {} passed", cases.len());
    assert!(unexpected.is_empty(), "{}", unexpected.join("\n"));
}

/// A table read three ways: R2RML on the loader's default database, RML on a
/// `d2rq:Database`, and YARRRML with `type: postgresql`.
#[test]
#[ignore = "needs TRIPLETRAIL_TEST_POSTGRES (a database it may empty)"]
fn postgres_sources() {
    let url = std::env::var("TRIPLETRAIL_TEST_POSTGRES").expect("set TRIPLETRAIL_TEST_POSTGRES");
    let _database = DATABASE.lock().unwrap_or_else(|e| e.into_inner());
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    client
        .batch_execute(
            "DROP TABLE IF EXISTS station; \
             CREATE TABLE station (id integer, name text, length numeric, opened date, \
                                   electrified boolean); \
             INSERT INTO station VALUES (1, 'Gent', 412.50, '1928-05-01', true), \
                                        (2, 'Brugge', NULL, NULL, false);",
        )
        .unwrap();
    let expected = [
        r#"<http://ex.org/station/1> <http://ex.org/name> "Gent""#,
        r#"<http://ex.org/station/1> <http://ex.org/length> "412.5"^^<http://www.w3.org/2001/XMLSchema#decimal>"#,
        r#"<http://ex.org/station/1> <http://ex.org/opened> "1928-05-01"^^<http://www.w3.org/2001/XMLSchema#date>"#,
        r#"<http://ex.org/station/1> <http://ex.org/electrified> "true"^^<http://www.w3.org/2001/XMLSchema#boolean>"#,
        r#"<http://ex.org/station/2> <http://ex.org/name> "Brugge""#,
        r#"<http://ex.org/station/2> <http://ex.org/electrified> "false"^^<http://www.w3.org/2001/XMLSchema#boolean>"#,
    ];
    let check = |mapping: Mapping, default: Option<String>| {
        let loader = DatabaseLoader::new(MemoryLoader::default(), default);
        let mut made: Vec<String> = run(&mapping, &loader)
            .unwrap()
            .iter()
            .map(|q| q.to_string())
            .collect();
        made.sort();
        let mut wanted: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
        wanted.sort();
        assert_eq!(made, wanted);
    };
    let poms = r#"
        rr:subjectMap [ rr:template "http://ex.org/station/{id}" ] ;
        rr:predicateObjectMap [ rr:predicate ex:name ; rr:objectMap [ rr:column "name" ] ] ,
            [ rr:predicate ex:length ; rr:objectMap [ rr:column "length" ] ] ,
            [ rr:predicate ex:opened ; rr:objectMap [ rr:column "opened" ] ] ,
            [ rr:predicate ex:electrified ; rr:objectMap [ rr:column "electrified" ] ] ."#;
    let prefixes = r#"
        @prefix rr: <http://www.w3.org/ns/r2rml#> .
        @prefix rml: <http://semweb.mmlab.be/ns/rml#> .
        @prefix d2rq: <http://www.wiwiss.fu-berlin.de/suhl/bizer/D2RQ/0.1#> .
        @prefix ex: <http://ex.org/> ."#;
    let r2rml = format!(r#"{prefixes} <#S> rr:logicalTable [ rr:tableName "station" ] ; {poms}"#);
    check(
        Mapping::parse_turtle(&r2rml, Some("http://ex.org/m")).unwrap(),
        Some(url.clone()),
    );
    let rml = format!(
        r#"{prefixes}
        <#DB> a d2rq:Database ; d2rq:jdbcDSN "{url}" ; d2rq:jdbcDriver "org.postgresql.Driver" .
        <#S> rml:logicalSource [ rml:source <#DB> ; rml:query "SELECT * FROM station" ;
                                 rr:sqlVersion rr:SQL2008 ] ; {poms}"#
    );
    check(
        Mapping::parse_turtle(&rml, Some("http://ex.org/m")).unwrap(),
        None,
    );
    let yarrrml = format!(
        r#"
prefixes:
  ex: http://ex.org/
mappings:
  station:
    sources:
      - access: {url}
        type: postgresql
        queryFormulation: sql2008
        query: SELECT * FROM station
    s: ex:station/$(id)
    po:
      - [ex:name, $(name)]
      - [ex:length, $(length)]
      - [ex:opened, $(opened)]
      - [ex:electrified, $(electrified)]
"#
    );
    check(Mapping::parse_yarrrml(&yarrrml, None).unwrap(), None);
}

/// TLS by `sslmode`, on a server with TLS whose certificate (for `localhost`) chains to the
/// CA in the PEM file `TRIPLETRAIL_TEST_POSTGRES_CA`.
#[test]
#[ignore = "needs TRIPLETRAIL_TEST_POSTGRES on localhost with TLS and TRIPLETRAIL_TEST_POSTGRES_CA"]
fn postgres_tls() {
    let url = std::env::var("TRIPLETRAIL_TEST_POSTGRES").expect("set TRIPLETRAIL_TEST_POSTGRES");
    let ca =
        std::env::var("TRIPLETRAIL_TEST_POSTGRES_CA").expect("set TRIPLETRAIL_TEST_POSTGRES_CA");
    let mapping = Mapping::parse_turtle(
        r#"@prefix rr: <http://www.w3.org/ns/r2rml#> .
           <#Session> rr:logicalTable [ rr:sqlQuery
               "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()" ] ;
             rr:subjectMap [ rr:constant <http://ex.org/session> ] ;
             rr:predicateObjectMap [ rr:predicate <http://ex.org/ssl> ;
                                     rr:objectMap [ rr:column "ssl" ] ] ."#,
        Some("http://ex.org/m"),
    )
    .unwrap();
    let encrypted = |settings: &str| -> Result<bool, String> {
        let separator = if url.contains('?') { '&' } else { '?' };
        let loader = DatabaseLoader::new(
            MemoryLoader::default(),
            Some(format!("{url}{separator}{settings}")),
        );
        let quads = run(&mapping, &loader).map_err(|e| e.to_string())?;
        Ok(quads[0].object.to_string().contains("true"))
    };
    assert_eq!(encrypted("sslmode=disable"), Ok(false));
    assert_eq!(encrypted("sslmode=prefer"), Ok(true));
    assert_eq!(encrypted("sslmode=require"), Ok(true));
    assert_eq!(
        encrypted(&format!("sslmode=verify-full&sslrootcert={ca}")),
        Ok(true)
    );
    assert_eq!(
        encrypted(&format!("sslmode=verify-ca&sslrootcert={ca}")),
        Ok(true)
    );
    // Not signed by a Mozilla root.
    let refused = encrypted("sslmode=verify-full").unwrap_err();
    assert!(refused.contains("certificate"), "{refused}");
}
