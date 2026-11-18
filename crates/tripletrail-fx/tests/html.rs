//! HTML sources, on the test files of SPARQL Anything's HTML triplifier (Apache-2.0, copied
//! unchanged into `tests/sparql-anything/html`, see the `LICENSE` and `README.md` in
//! `tests/sparql-anything`), with the same expected graphs.

use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::Graph;
use oxigraph::model::dataset::CanonicalizationAlgorithm;
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use std::path::PathBuf;
use tripletrail_fx::{FxConfig, FxService};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/sparql-anything/html")
}

fn triplify(options: &str) -> Result<Graph, String> {
    let fx = FxService::new(
        FxConfig {
            fixed_limits: false,
            databases: None,
            base: dir(),
            allowed: None,
            remote: Default::default(),
        },
        SparqlEvaluator::new,
    );
    let empty = Box::leak(Box::new(oxigraph::model::Dataset::new()));
    let query = format!(
        "CONSTRUCT {{ ?s ?p ?o }} WHERE {{ SERVICE <x-sparql-anything:root=http://www.example.org/document,{options}> {{ ?s ?p ?o }} }}"
    );
    let QueryResults::Graph(triples) = fx
        .register(SparqlEvaluator::new())
        .parse_query(&query)
        .unwrap()
        .on_queryable_dataset(&*empty)
        .execute()
        .map_err(|e| e.to_string())?
    else {
        panic!("not a CONSTRUCT")
    };
    triples.collect::<Result<_, _>>().map_err(|e| e.to_string())
}

fn expected(file: &str) -> Graph {
    let mut text = std::fs::read_to_string(dir().join(file)).unwrap();
    // Some of the files end their last statement without the final `.`.
    let statements = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    if !statements.trim_end().ends_with('.') {
        text.push_str(" .");
    }
    RdfParser::from_format(RdfFormat::Turtle)
        .for_slice(text.as_bytes())
        .map(|q| q.map(oxigraph::model::Triple::from))
        .collect::<Result<Graph, _>>()
        .unwrap()
}

fn assert_isomorphic(mut actual: Graph, mut expected: Graph) {
    actual.canonicalize(CanonicalizationAlgorithm::Unstable);
    expected.canonicalize(CanonicalizationAlgorithm::Unstable);
    assert!(
        actual == expected,
        "\nactual:\n{actual}\nexpected:\n{expected}"
    );
}

#[test]
fn html1() {
    assert_isomorphic(
        triplify("location=HTML1.html,blank-nodes=false").unwrap(),
        expected("HTML1.ttl"),
    );
}

/// `html.parser=xml` on SPARQL Anything's XML test file (INSPIRE GML).
#[test]
fn xml_parser() {
    assert_isomorphic(
        triplify("location=XML.xml,triplifier=io.github.sparqlanything.html.HTMLTriplifier,html.parser=xml").unwrap(),
        expected("XML.ttl"),
    );
}

#[test]
fn html2() {
    assert_isomorphic(
        triplify("location=HTML2.html,blank-nodes=false").unwrap(),
        expected("HTML2.ttl"),
    );
}

/// Prefixed names without a declared namespace (#544). SPARQL Anything writes the `|` of the
/// element `uid:` into its IRI, which no IRI may hold; TripleTrail percent-encodes it.
#[test]
fn dirty1() {
    let text = std::fs::read_to_string(dir().join("DIRTY1.ttl")).unwrap();
    let expected = RdfParser::from_format(RdfFormat::Turtle)
        .for_slice(text.replace("\\u007C", "%7C").as_bytes())
        .map(|q| q.map(oxigraph::model::Triple::from))
        .collect::<Result<Graph, _>>()
        .unwrap();
    assert_isomorphic(
        triplify("location=DIRTY1.html,blank-nodes=false").unwrap(),
        expected,
    );
}

#[test]
fn base_with_blank_nodes() {
    assert_isomorphic(
        triplify("location=Base.html").unwrap(),
        expected("Base.ttl"),
    );
}

/// The values of `?v` for a pattern on an HTML source, or the error.
fn values(options: &str, pattern: &str) -> Result<Vec<String>, String> {
    let fx = FxService::new(
        FxConfig {
            fixed_limits: false,
            databases: None,
            base: dir(),
            allowed: None,
            remote: Default::default(),
        },
        SparqlEvaluator::new,
    );
    let empty = Box::leak(Box::new(oxigraph::model::Dataset::new()));
    let query = format!(
        "PREFIX whatwg: <https://html.spec.whatwg.org/#>
         PREFIX xhtml: <http://www.w3.org/1999/xhtml#>
         SELECT ?v WHERE {{ SERVICE <x-sparql-anything:{options}> {{ {pattern} }} }} ORDER BY ?v"
    );
    let QueryResults::Solutions(solutions) = fx
        .register(SparqlEvaluator::new())
        .parse_query(&query)
        .unwrap()
        .on_queryable_dataset(&*empty)
        .execute()
        .map_err(|e| e.to_string())?
    else {
        panic!("not a SELECT")
    };
    let mut out = Vec::new();
    for s in solutions {
        let s = s.map_err(|e| e.to_string())?;
        out.extend(s.get("v").map(ToString::to_string));
    }
    Ok(out)
}

/// `fx:properties` triples setting `name` to `value` (full IRIs: prefixed names with dots do
/// not parse).
fn props(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| {
            format!(
                "<http://sparql.xyz/facade-x/engine/properties> <http://sparql.xyz/facade-x/engine/{k}> {v:?} . "
            )
        })
        .collect()
}

/// SPARQL Anything's example: the text of the elements of class `paragraph`.
#[test]
fn selector_picks_elements() {
    assert_eq!(
        values(
            "location=HTML1.html,html.selector=.paragraph",
            "?s whatwg:innerText ?v"
        )
        .unwrap(),
        [r#""Hello world""#]
    );
    let page = "<ul><li>a</li><li class=x>b</li></ul><p>c</p>";
    // Several elements: each is a slot of the root.
    let several = props(&[("content", page), ("html.selector", "li, p")]);
    assert_eq!(
        values(
            "media-type=text/html",
            &format!("{several} ?r a <http://sparql.xyz/facade-x/ns/Root> ; ?slot ?e . ?e whatwg:innerText ?v")
        )
        .unwrap(),
        [r#""a""#, r#""b""#, r#""c""#]
    );
    let one = props(&[("content", page), ("html.selector", "ul > li[class=x]")]);
    assert_eq!(
        values(
            "media-type=text/html",
            &format!("{one} ?s whatwg:innerText ?v")
        )
        .unwrap(),
        [r#""b""#]
    );
}

#[test]
fn unsupported_options_are_refused() {
    for (option, value, says) in [
        ("html.metadata", "true", "html.metadata"),
        ("html.browser", "chromium", "html.browser"),
        ("html.parser", "lxml", "html.parser=lxml"),
        ("html.selector", "p:hover", ":hover"),
    ] {
        let e = values(
            "location=HTML1.html",
            &format!("{} ?s ?p ?v", props(&[(option, value)])),
        )
        .unwrap_err();
        assert!(e.contains(says), "{option}: {e}");
    }
}

/// `blank-nodes=false`: every container is an IRI.
#[test]
fn no_blank_nodes() {
    let graph = triplify("location=BN.html,blank-nodes=false").unwrap();
    assert!(graph.len() > 10);
    for t in &graph {
        assert!(
            !t.subject.is_blank_node() && !t.object.is_blank_node(),
            "{t}"
        );
    }
}
