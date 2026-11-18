//! RDFC-1.0 on the W3C test suite (https://github.com/w3c/rdf-canon, `tests/`), when
//! `RDFC_TESTS` names that directory; and on a few cases of its own otherwise.

use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::Quad;
use tripletrail::canon::{CanonError, canonicalize};

fn parse(text: &str) -> Vec<Quad> {
    RdfParser::from_format(RdfFormat::NQuads)
        .for_slice(text.as_bytes())
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn w3c_rdfc10_suite() {
    let Ok(dir) = std::env::var("RDFC_TESTS") else {
        eprintln!("RDFC_TESTS not set: skipped");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let manifest = std::fs::read_to_string(dir.join("manifest.csv")).unwrap();
    let (mut passed, mut failed) = (0, Vec::new());
    for line in manifest.lines().skip(1) {
        let test = line.split(',').next().unwrap();
        // test075 hashes with SHA-384, which is not offered.
        if line.contains(",SHA384,") {
            continue;
        }
        let input = std::fs::read_to_string(dir.join(format!("rdfc10/{test}-in.nq"))).unwrap();
        let result = canonicalize(parse(&input));
        if line.contains("NegativeEvalTest") {
            match result {
                Err(CanonError::TooComplex(_)) => passed += 1,
                other => failed.push(format!("{test}: expected refusal, got {other:?}")),
            }
            continue;
        }
        let expected =
            std::fs::read_to_string(dir.join(format!("rdfc10/{test}-rdfc10.nq"))).unwrap();
        match result {
            Ok(c) if c.nquads() == expected => passed += 1,
            Ok(c) => failed.push(format!("{test}:\n{}\nexpected\n{expected}", c.nquads())),
            Err(e) => failed.push(format!("{test}: {e}")),
        }
    }
    assert!(
        failed.is_empty(),
        "{passed} passed, failed:\n{}",
        failed.join("\n")
    );
    assert!(passed > 60, "{passed}");
}

#[test]
fn isomorphic_datasets_have_the_same_digest() {
    let a = parse(
        "_:x <http://ex/p> _:y <http://ex/g> .\n_:y <http://ex/p> _:x <http://ex/g> .\n_:x <http://ex/q> \"1\" .\n",
    );
    let b = parse(
        "_:k <http://ex/q> \"1\" .\n_:j <http://ex/p> _:k <http://ex/g> .\n_:k <http://ex/p> _:j <http://ex/g> .\n_:k <http://ex/q> \"1\" .\n",
    );
    let (a, b) = (canonicalize(a).unwrap(), canonicalize(b).unwrap());
    assert_eq!(a.nquads(), b.nquads());
    assert_eq!(a.digest(), b.digest());
    assert_eq!(a.lines.len(), 3);
    let c = canonicalize(parse("_:x <http://ex/q> \"2\" .\n")).unwrap();
    assert_ne!(a.digest(), c.digest());
}

#[test]
fn a_diff_matches_blank_nodes_and_counts_changes() {
    let a = canonicalize(parse(
        "<http://ex/s> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/C> .\n\
         <http://ex/s> <http://ex/p> \"1\" <http://ex/g> .\n\
         <http://ex/s> <http://ex/geo> _:a .\n_:a <http://ex/wkt> \"POINT(1 2)\" .\n",
    ))
    .unwrap();
    let b = canonicalize(parse(
        "<http://ex/s> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/C> .\n\
         <http://ex/s> <http://ex/p> \"2\" <http://ex/g> .\n\
         <http://ex/s> <http://ex/geo> _:other .\n_:other <http://ex/wkt> \"POINT(1 2)\" .\n\
         <http://ex/t> <http://ex/p> \"3\" .\n",
    ))
    .unwrap();
    let d = tripletrail::canon::diff(&a, &b);
    assert_eq!(
        d.removed,
        ["<http://ex/s> <http://ex/p> \"1\" <http://ex/g> .\n"]
    );
    assert_eq!(
        d.added,
        [
            "<http://ex/s> <http://ex/p> \"2\" <http://ex/g> .\n",
            "<http://ex/t> <http://ex/p> \"3\" .\n"
        ]
    );
    let s = d.summary(&a, &b);
    assert_eq!((s.total.added, s.total.removed), (2, 1));
    assert_eq!(s.graphs["http://ex/g"].added, 1);
    assert_eq!(s.graphs[""].added, 1);
    assert_eq!(s.predicates["http://ex/p"].removed, 1);
    assert_eq!(s.classes["http://ex/C"].added, 1);
    assert_eq!(s.classes[""].added, 1);
}

#[test]
fn a_long_list_of_one_value_is_refused_quickly() {
    let rdf = "<http://www.w3.org/1999/02/22-rdf-syntax-ns#";
    let mut text = String::from("<http://example.com/s> <http://example.com/p> _:l0 .\n");
    let n = 5000;
    for i in 0..n {
        let next = if i + 1 < n {
            format!("_:l{}", i + 1)
        } else {
            format!("{rdf}nil>")
        };
        text.push_str(&format!(
            "_:l{i} {rdf}first> \"0\" .\n_:l{i} {rdf}rest> {next} .\n"
        ));
    }
    let start = std::time::Instant::now();
    assert!(matches!(
        canonicalize(parse(&text)),
        Err(CanonError::TooComplex(_))
    ));
    assert!(start.elapsed().as_secs() < 10);
    // A short one is fine.
    let short: String = text.lines().take(41).collect::<Vec<_>>().join("\n");
    assert!(canonicalize(parse(&short)).is_ok());
}
