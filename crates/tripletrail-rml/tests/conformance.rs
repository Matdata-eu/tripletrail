//! The RML-Core, RML-IO and RML-LV test cases of the W3C KG-Construct group, run from
//! checkouts of <https://github.com/kg-construct/rml-core>,
//! <https://github.com/kg-construct/rml-io> and <https://github.com/kg-construct/rml-lv>
//! (CC BY 4.0, not copied here):
//!
//! ```sh
//! for r in rml-core rml-io rml-lv rml-fnml rml-cc rml-star; do
//!     git clone --depth 1 https://github.com/kg-construct/$r /tmp/$r; done
//! RML_CORE_TESTS=/tmp/rml-core/test-cases RML_IO_TESTS=/tmp/rml-io/test-cases \
//!     RML_LV_TESTS=/tmp/rml-lv/test-cases RML_FNML_TESTS=/tmp/rml-fnml/test-cases \
//!     RML_CC_TESTS=/tmp/rml-cc/test-cases RML_STAR_TESTS=/tmp/rml-star/test-cases \
//!     cargo test -p tripletrail-rml --test conformance -- --ignored --nocapture
//! ```
//!
//! A case passes when the mapping's output is isomorphic to `output.nq` (`default.nq` in
//! RML-IO, `output.nt` read as RDF 1.2 Turtle in RML-star), and the files of its logical targets to theirs, or, for a case without one, when
//! the mapping is rejected. Cases listed in `KNOWN` are expected to fail
//! (unsupported features); the test fails on any other failure, and on a known one that
//! passes.

use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::dataset::CanonicalizationAlgorithm;
use oxigraph::model::{Dataset, Quad};
use std::path::Path;
use tripletrail_rml::{FsLoader, Mapping, read_target, run_targets, write_target};

/// RML-Core cases that fail for a stated reason.
const CORE_KNOWN: &[(&str, &str)] = &[(
    "RMLTC0027b-JSON",
    "rml:UnsafeIRI: the expected output holds an IRI with a space, which no RDF parser reads",
)];

/// The case's "Default Base IRI" from its README, else `http://example.com/` (RML-CC:
/// `http://example.com/base/`).
fn base(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("README.md"))
        .ok()
        .and_then(|readme| {
            readme.lines().find_map(|l| {
                l.strip_prefix("**Default Base IRI**:")
                    .map(|b| b.trim().to_owned())
            })
        })
        .unwrap_or_else(|| {
            // RML-CC's READMEs name no base; its outputs use this one.
            let cc = dir
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("RMLTC-CC"));
            if cc {
                "http://example.com/base/".into()
            } else {
                "http://example.com/".into()
            }
        })
}

/// `Ok(())` when the case passes, else why not.
fn case(dir: &Path) -> Result<(), String> {
    let mapping = std::fs::read_to_string(dir.join("mapping.ttl")).map_err(|e| e.to_string())?;
    let expected = [
        dir.join("output.nq"),
        dir.join("default.nq"),
        dir.join("output.nt"),
    ]
    .into_iter()
    .find(|p| p.exists())
    .unwrap_or_else(|| dir.join("output.nq"));
    let loader = FsLoader {
        config: tripletrail_fx::FxConfig {
            fixed_limits: false,
            databases: None,
            base: dir.to_path_buf(),
            allowed: None,
            remote: Default::default(),
        },
    };
    let mapping = Mapping::parse_turtle(&mapping, Some(&base(dir)));
    let made = mapping
        .as_ref()
        .map_err(|e| e.to_string())
        .and_then(|m| run_targets(m, &loader).map_err(|e| e.to_string()));
    if !expected.exists() {
        return match made {
            Err(_) => Ok(()),
            Ok(quads) => Err(format!(
                "expected an error, made {} quads",
                quads.default.len()
            )),
        };
    }
    let made = made?;
    let mapping = mapping.map_err(|e| e.to_string())?;
    let text = std::fs::read(&expected).map_err(|e| e.to_string())?;
    // RML-star's outputs (output.nt) write quoted triples as `<< s p o >>`, which RDF 1.2
    // Turtle reads as a reifier: `_:r rdf:reifies <<( s p o )>>`, as TripleTrail makes them.
    let star = expected.extension().is_some_and(|e| e == "nt");
    let format = if star {
        RdfFormat::Turtle
    } else {
        RdfFormat::NQuads
    };
    let wanted = RdfParser::from_format(format)
        .for_slice(&text)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let (made_default, wanted) = if star {
        (one_reifier(made.default), one_reifier(wanted))
    } else {
        (made.default, wanted)
    };
    same(made_default, wanted).map_err(|e| format!("default output: {e}"))?;
    // Each target's file: what the case expects, and what TripleTrail writes (read back).
    for (target, quads) in mapping.targets.iter().zip(made.targets) {
        let path = dir.join(&target.path);
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let wanted = read_target(target, bytes).map_err(|e| e.to_string())?;
        let written = write_target(target, &quads).map_err(|e| e.to_string())?;
        let read_back = read_target(target, written).map_err(|e| e.to_string())?;
        same(read_back, wanted.clone()).map_err(|e| format!("{} as written: {e}", target.path))?;
        same(quads, wanted).map_err(|e| format!("{}: {e}", target.path))?;
    }
    Ok(())
}

/// The quads with one reifier per reified triple. RDF-star quotes a triple as a term, the
/// same at each use; RDF 1.2 Turtle reads each `<< s p o >>` as a reifier of its own, where
/// TripleTrail names the reifiers of a triple by the triple. Both are compared with each
/// reifier (a blank node with an `rdf:reifies`) renamed after the triple it reifies.
fn one_reifier(quads: Vec<Quad>) -> Vec<Quad> {
    use oxigraph::model::vocab::rdf;
    use oxigraph::model::{BlankNode, NamedOrBlankNode, Term, Triple};
    use std::collections::HashMap;
    use std::hash::{Hash, Hasher};
    type Reifies = HashMap<BlankNode, Triple>;
    let mut reifies = Reifies::new();
    for q in &quads {
        if q.predicate == rdf::REIFIES
            && let (NamedOrBlankNode::BlankNode(b), Term::Triple(t)) = (&q.subject, &q.object)
        {
            reifies.insert(b.clone(), (**t).clone());
        }
    }
    fn rename(term: Term, reifies: &Reifies, depth: usize) -> Term {
        match term {
            Term::BlankNode(b) if depth < 16 => match reifies.get(&b) {
                Some(t) => {
                    let named = rename(Term::Triple(Box::new(t.clone())), reifies, depth + 1);
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    named.to_string().hash(&mut h);
                    BlankNode::new_from_unique_id(u128::from(h.finish())).into()
                }
                None => Term::BlankNode(b),
            },
            Term::Triple(t) => {
                let subject = match rename(t.subject.clone().into(), reifies, depth) {
                    Term::BlankNode(b) => NamedOrBlankNode::from(b),
                    _ => t.subject.clone(),
                };
                let object = rename(t.object.clone(), reifies, depth);
                Term::Triple(Box::new(Triple::new(subject, t.predicate.clone(), object)))
            }
            t => t,
        }
    }
    let mut out: Vec<Quad> = Vec::new();
    for q in quads {
        let subject = match rename(q.subject.clone().into(), &reifies, 0) {
            Term::BlankNode(b) => NamedOrBlankNode::from(b),
            _ => q.subject.clone(),
        };
        let object = rename(q.object.clone(), &reifies, 0);
        let quad = Quad::new(subject, q.predicate.clone(), object, q.graph_name);
        if !out.contains(&quad) {
            out.push(quad);
        }
    }
    out
}

/// `Ok(())` when the quads are isomorphic, else some of the differences.
fn same(actual: Vec<Quad>, wanted: Vec<Quad>) -> Result<(), String> {
    let mut actual: Dataset = actual.into_iter().collect();
    let mut wanted: Dataset = wanted.into_iter().collect();
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

/// RML-LV cases that fail for a stated reason.
const LV_KNOWN: &[(&str, &str)] = &[];

/// RML-IO cases that fail for a stated reason; a name ending in `*` covers a prefix.
const IO_KNOWN: &[(&str, &str)] = &[
    (
        "RMLTTC0002q",
        "the expected file writes the JSON integer 33 with rml:datatype xsd:double as \"3.3E1\": \
         TripleTrail keeps the source's lexical form (\"33\"), as it does everywhere",
    ),
    (
        "RMLSTC0003",
        "SPARQL endpoints as sources (rml:SPARQL_Results_CSV) are not supported",
    ),
    (
        "RMLSTC0006a",
        "a d2rq:Database source without a configured database",
    ),
];

#[test]
#[ignore = "needs RML_CORE_TESTS, a checkout of the rml-core test cases"]
fn rml_core_test_cases() {
    suite("RML_CORE_TESTS", CORE_KNOWN);
}

#[test]
#[ignore = "needs RML_LV_TESTS, a checkout of the rml-lv test cases"]
fn rml_lv_test_cases() {
    suite("RML_LV_TESTS", LV_KNOWN);
}

/// RML-CC cases that fail, and why.
const CC_KNOWN: &[(&str, &str)] = &[];

#[test]
#[ignore = "needs RML_CC_TESTS, a checkout of the rml-cc test cases"]
fn rml_cc_test_cases() {
    suite("RML_CC_TESTS", CC_KNOWN);
}

/// RML-FNML cases that fail, and why.
const FNML_KNOWN: &[(&str, &str)] = &[];

#[test]
#[ignore = "needs RML_FNML_TESTS, a checkout of the rml-fnml test cases"]
fn rml_fnml_test_cases() {
    suite("RML_FNML_TESTS", FNML_KNOWN);
}

/// RML-star cases that fail, and why.
/// Their `data2.csv` starts with a Markdown line (`**Input 1**`), read as the header.
const STAR_KNOWN: &[(&str, &str)] = &[
    ("RMLSTARTC001b", STAR_DATA2),
    ("RMLSTARTC002b", STAR_DATA2),
    ("RMLSTARTC003b", STAR_DATA2),
    ("RMLSTARTC004b", STAR_DATA2),
    ("RMLSTARTC005b", STAR_DATA2),
    ("RMLSTARTC006b", STAR_DATA2),
    ("RMLSTARTC007b", STAR_DATA2),
    ("RMLSTARTC008b", STAR_DATA2),
];
const STAR_DATA2: &str = "the case's data2.csv starts with a Markdown line, read as its header";

#[test]
#[ignore = "needs RML_STAR_TESTS, a checkout of the rml-star test cases"]
fn rml_star_test_cases() {
    suite("RML_STAR_TESTS", STAR_KNOWN);
}

#[test]
#[ignore = "needs RML_IO_TESTS, a checkout of the rml-io test cases"]
fn rml_io_test_cases() {
    suite("RML_IO_TESTS", IO_KNOWN);
}

fn suite(variable: &str, known: &[(&str, &str)]) {
    let Some(root) = std::env::var_os(variable) else {
        panic!("set {variable} to the test-cases directory of the checkout");
    };
    let mut dirs: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("mapping.ttl").exists())
        .collect();
    dirs.sort();
    let (mut passed, mut unexpected) = (0, Vec::new());
    for dir in &dirs {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let known = known.iter().find(|(n, _)| match n.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => *n == name,
        });
        match (case(dir), known) {
            (Ok(()), None) => passed += 1,
            (Ok(()), Some(_)) => unexpected.push(format!("{name}: passes, remove it from KNOWN")),
            (Err(_), Some(_)) => {}
            (Err(e), None) => unexpected.push(format!("{name}: {e}")),
        }
    }
    println!(
        "{passed} of {} passed, {} known failures",
        dirs.len(),
        known.len()
    );
    assert!(unexpected.is_empty(), "{}", unexpected.join("\n"));
}

/// A mapping as text that does not depend on the names of its nodes: per triples map its
/// source, subject, classes, graphs and predicate-object maps (a join by its parent's source
/// and subject), sorted.
fn canonical(m: &Mapping) -> Vec<String> {
    use tripletrail_rml::ObjectMap;
    let mut maps: Vec<String> = m
        .triples_maps
        .iter()
        .map(|tm| {
            let mut classes: Vec<String> = tm.classes.iter().map(|c| c.to_string()).collect();
            classes.sort();
            let mut poms: Vec<String> = tm
                .predicate_objects
                .iter()
                .flat_map(|pom| {
                    let objects: Vec<String> = pom
                        .objects
                        .iter()
                        .map(|o| match o {
                            ObjectMap::Term(t) => format!("{t:?}"),
                            ObjectMap::Join { parent, conditions } => {
                                let p = &m.triples_maps[*parent];
                                format!("join {:?} {:?} on {conditions:?}", p.source, p.subject)
                            }
                            ObjectMap::Quoted(q) => {
                                let p = &m.triples_maps[q.map];
                                format!("quoted {:?} on {:?}", p.source, q.conditions)
                            }
                        })
                        .collect();
                    pom.predicates.iter().flat_map(move |p| {
                        let graphs = format!("{:?}", pom.graphs);
                        objects
                            .iter()
                            .map(move |o| format!("{p:?} -> {o} in {graphs}"))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            poms.sort();
            format!(
                "{:?}\n  {:?}\n  {classes:?}\n  {:?}\n  {}",
                tm.source,
                tm.subject,
                tm.graphs,
                poms.join("\n  ")
            )
        })
        .collect();
    maps.sort();
    maps
}

/// The YARRRML parser's test cases: each `mapping.yarrrml` must stand for the same rules as
/// the `mapping.rml.ttl` the parser makes from it. Cases whose RML uses what tripletrail-rml does
/// not support (functions) are skipped.
#[test]
#[ignore = "needs YARRRML_TESTS, a checkout of the yarrrml-parser test cases"]
fn yarrrml_parser_test_cases() {
    let Some(root) = std::env::var_os("YARRRML_TESTS") else {
        panic!("set YARRRML_TESTS to the test directory of yarrrml-parser");
    };
    let mut dirs = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(root)];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            if e.path().is_dir() {
                stack.push(e.path());
            }
        }
        if d.join("mapping.yarrrml").exists() && d.join("mapping.rml.ttl").exists() {
            dirs.push(d);
        }
    }
    dirs.sort();
    let (mut passed, mut skipped, mut failed) = (0, 0, Vec::new());
    for dir in &dirs {
        let name = dir.display().to_string();
        let base = "http://mapping.example.com/";
        let rml = std::fs::read_to_string(dir.join("mapping.rml.ttl")).unwrap();
        let expected = match Mapping::parse_turtle(&rml, Some(base)) {
            Ok(m) => m,
            Err(e) => {
                if std::env::var_os("YARRRML_VERBOSE").is_some() {
                    println!("skipped {name}: {e}");
                }
                skipped += 1;
                continue;
            }
        };
        let yarrrml = std::fs::read_to_string(dir.join("mapping.yarrrml")).unwrap();
        match Mapping::parse_yarrrml(&yarrrml, Some(base)) {
            Ok(m) if canonical(&m) == canonical(&expected) => passed += 1,
            Ok(m) => failed.push(format!(
                "{name}:\n made {:#?}\n wanted {:#?}",
                canonical(&m),
                canonical(&expected)
            )),
            // Web APIs (WoT), databases.
            Err(tripletrail_rml::RmlError::Unsupported(_)) => skipped += 1,
            Err(e) => failed.push(format!("{name}: {e}")),
        }
    }
    println!(
        "{passed} of {} passed, {skipped} skipped (unsupported RML)",
        dirs.len()
    );
    assert!(failed.is_empty(), "{}", failed.join("\n\n"));
}
