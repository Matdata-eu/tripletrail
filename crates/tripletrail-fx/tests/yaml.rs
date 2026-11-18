//! YAML sources, on the test files of SPARQL Anything's YAML triplifier (Apache-2.0, copied
//! unchanged into `tests/sparql-anything/yaml`, see the `LICENSE` and `README.md` in
//! `tests/sparql-anything`), with the same expected graphs.

use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::Graph;
use oxigraph::model::dataset::CanonicalizationAlgorithm;
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use std::path::PathBuf;
use tripletrail_fx::{FxConfig, FxService};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/sparql-anything/yaml")
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
    if !text.trim_end().ends_with('.') {
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
    assert_eq!(
        actual, expected,
        "\nactual:\n{actual}\nexpected:\n{expected}"
    );
}

#[test]
fn map_with_iri_root() {
    assert_isomorphic(
        triplify("location=Map.yaml,blank-nodes=false").unwrap(),
        expected("Map.ttl"),
    );
}

#[test]
fn nested_mapping() {
    assert_isomorphic(triplify("location=Foo.yaml").unwrap(), expected("Foo.ttl"));
}

#[test]
fn duplicate_keys_refused_by_default() {
    let e = triplify("location=DuplicateKeys.yaml").unwrap_err();
    assert!(e.contains("duplicate key"), "{e}");
}

#[test]
fn duplicate_keys_last_wins() {
    assert_isomorphic(
        triplify("location=DuplicateKeys.yaml,yaml.allow-duplicate-keys=true").unwrap(),
        expected("DuplicateKeys.ttl"),
    );
}

/// The example of SPARQL Anything's YAML documentation (`test.yaml`).
#[test]
fn scalars_lists_and_typing() {
    let expected = r#"
        @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
        @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
        @prefix fx: <http://sparql.xyz/facade-x/ns/> .
        @prefix xyz: <http://sparql.xyz/facade-x/data/> .
        [ rdf:type fx:Root;
          xyz:another-key [ rdf:_1 [ xyz:name "john"; xyz:surname "smith" ] ];
          xyz:boolean true;
          xyz:float "0.1"^^xsd:double;
          xyz:key "value";
          xyz:nested-array [ rdf:_1 [ xyz:nested-array [ rdf:_1 [ <http://sparql.xyz/facade-x/data/nested:key> "Value with spaces" ] ] ] ];
          xyz:two-values [ rdf:_1 "1"^^xsd:int; rdf:_2 "2" ]
        ] ."#;
    let expected = RdfParser::from_format(RdfFormat::Turtle)
        .for_slice(expected.as_bytes())
        .map(|q| q.map(oxigraph::model::Triple::from))
        .collect::<Result<Graph, _>>()
        .unwrap();
    assert_isomorphic(triplify("location=test.yaml").unwrap(), expected);
}

#[test]
fn iri_containers_numbers_nulls_and_anchors() {
    let content = oxigraph::model::Literal::new_simple_literal(
        "big: 3000000000\nnone: ~\nexp: 1e10\nquoted: '12'\nbase: &b {x: 1}\ncopy: *b\n---\nlist: [a, b]\n",
    );
    let fx = FxService::new(FxConfig::default(), SparqlEvaluator::new);
    let empty = Box::leak(Box::new(oxigraph::model::Dataset::new()));
    let QueryResults::Graph(triples) = fx
        .register(SparqlEvaluator::new())
        .parse_query(&format!(
            "PREFIX fxe: <http://sparql.xyz/facade-x/engine/>
            CONSTRUCT {{ ?s ?p ?o }} WHERE {{ SERVICE <x-sparql-anything:> {{
                fxe:properties fxe:content {content} ; fxe:media-type \"application/yaml\" ;
                  fxe:root \"http://ex.org/d\" ; fxe:blank-nodes \"false\" .
                ?s ?p ?o }} }}"
        ))
        .unwrap()
        .on_queryable_dataset(&*empty)
        .execute()
        .unwrap()
    else {
        panic!()
    };
    let mut lines: Vec<String> = triples
        .map(|t| {
            t.unwrap()
                .to_string()
                .replace("http://sparql.xyz/facade-x/data/", "xyz:")
                .replace("http://www.w3.org/1999/02/22-rdf-syntax-ns#", "rdf:")
                .replace("http://www.w3.org/2001/XMLSchema#", "xsd:")
        })
        .collect();
    lines.sort();
    assert_eq!(
        lines,
        [
            "<http://ex.org/d/base> <xyz:x> \"1\"^^<xsd:int>",
            "<http://ex.org/d/copy> <xyz:x> \"1\"^^<xsd:int>",
            "<http://ex.org/d/list> <rdf:_1> \"a\"",
            "<http://ex.org/d/list> <rdf:_2> \"b\"",
            "<http://ex.org/d> <rdf:type> <http://sparql.xyz/facade-x/ns/Root>",
            "<http://ex.org/d> <xyz:base> <http://ex.org/d/base>",
            "<http://ex.org/d> <xyz:big> \"3000000000\"^^<xsd:long>",
            "<http://ex.org/d> <xyz:copy> <http://ex.org/d/copy>",
            "<http://ex.org/d> <xyz:exp> \"1.0E10\"^^<xsd:double>",
            "<http://ex.org/d> <xyz:list> <http://ex.org/d/list>",
            "<http://ex.org/d> <xyz:quoted> \"12\"",
        ]
    );
}

/// The triples of YAML `text` with option triples `options`, as N-Triples lines.
fn content(text: &str, options: &str) -> Result<Vec<String>, String> {
    content_in(FxConfig::default(), text, options)
}

fn content_in(config: FxConfig, text: &str, options: &str) -> Result<Vec<String>, String> {
    let content = oxigraph::model::Literal::new_simple_literal(text);
    let fx = FxService::new(config, SparqlEvaluator::new);
    let empty = Box::leak(Box::new(oxigraph::model::Dataset::new()));
    let QueryResults::Graph(triples) = fx
        .register(SparqlEvaluator::new())
        .parse_query(&format!(
            "PREFIX fxe: <http://sparql.xyz/facade-x/engine/>
            CONSTRUCT {{ ?s ?p ?o }} WHERE {{ SERVICE <x-sparql-anything:> {{
                fxe:properties fxe:content {content} ; fxe:media-type \"application/yaml\" ;
                  fxe:root \"http://ex.org/d\" ; fxe:blank-nodes \"false\" {options} .
                ?s ?p ?o }} }}"
        ))
        .unwrap()
        .on_queryable_dataset(&*empty)
        .execute()
        .map_err(|e| e.to_string())?
    else {
        panic!()
    };
    triples
        .map(|t| t.map(|t| t.to_string()).map_err(|e| e.to_string()))
        .collect()
}

#[test]
fn alias_bombs_are_refused() {
    // Each level holds ten aliases of the one before: 10^9 nodes from 430 bytes.
    let mut text = String::from("a0: &a0 [x, x, x, x, x, x, x, x, x, x]\n");
    for level in 1..=9 {
        let prev = format!("*a{}", level - 1);
        text.push_str(&format!(
            "a{level}: &a{level} [{}]\n",
            [prev.as_str(); 10].join(", ")
        ));
    }
    let e = content(&text, "").unwrap_err();
    assert!(e.contains("yaml.max-alias-nodes"), "{e}");
    // Three levels make 11,100 nodes through aliases: under the limit, over a lower one.
    let small: String = text.lines().take(4).collect::<Vec<_>>().join("\n");
    assert!(content(&small, "").unwrap().len() > 1000);
    let e = content(&small, "; fxe:yaml.max-alias-nodes \"1000\"").unwrap_err();
    assert!(e.contains("yaml.max-alias-nodes"), "{e}");
    // A caller may raise the limit, unless the limits are fixed (as on a server).
    let raised = "; fxe:yaml.max-alias-nodes \"10000000000\"";
    assert!(content(&small, raised).is_ok());
    let fixed = || FxConfig {
        fixed_limits: true,
        ..FxConfig::default()
    };
    let e = content_in(fixed(), &small, raised).unwrap_err();
    assert!(e.contains("may not be raised above 100000"), "{e}");
    let e = content_in(fixed(), &text, "; fxe:yaml.max-alias-nodes \"1e12\"").unwrap_err();
    assert!(e.contains("yaml.max-alias-nodes"), "{e}");
    assert!(content_in(fixed(), &small, "; fxe:yaml.max-alias-nodes \"20000\"").is_ok());
}

#[test]
fn infinities_and_nan_are_xsd_doubles() {
    let lines = content("a: .inf\nb: -.inf\nc: .nan\n", "").unwrap();
    for v in ["\"INF\"", "\"-INF\"", "\"NaN\""] {
        assert!(
            lines
                .iter()
                .any(|l| l.contains(&format!("{v}^^<http://www.w3.org/2001/XMLSchema#double>"))),
            "{v} in {lines:?}"
        );
    }
}

#[test]
fn merge_keys_are_plain_keys() {
    let lines = content("base: &b {x: 1}\nmine:\n  <<: *b\n  y: 2\n", "").unwrap();
    assert!(lines.iter().any(|l| l.contains("%3C%3C")), "{lines:?}");
}

#[test]
fn nesting_past_the_limit_is_an_error() {
    let deep = |n: usize| {
        let dir = tripletrail_testdir::tempdir("fx-deep");
        let path = dir.join(format!("deep-{n}.yaml"));
        std::fs::write(&path, format!("{}x", "- ".repeat(n))).unwrap();
        let out = triplify(&format!("location={}", path.display()));
        std::fs::remove_file(&path).unwrap();
        out
    };
    assert!(deep(100).is_ok());
    let e = deep(20_000).unwrap_err();
    assert!(e.contains("nest more than 512 deep"), "{e}");
}

#[test]
fn aliases_may_not_nest_past_the_limit() {
    // An anchor 300 sequences deep, copied inside another 300 deep, nests 600 deep.
    let text = |outer: usize| {
        format!(
            "a: &a\n  {}x\nb:\n  {}*a\n",
            "- ".repeat(300),
            "- ".repeat(outer)
        )
    };
    let e = content(&text(300), "").unwrap_err();
    assert!(e.contains("through an alias"), "{e}");
    // Copied 100 deep, it nests 400 deep, which is allowed.
    assert!(content(&text(100), "").is_ok());
}
