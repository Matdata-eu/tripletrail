//! Markdown sources, on the test files of SPARQL Anything's Markdown triplifier (Apache-2.0,
//! copied unchanged into `tests/sparql-anything/markdown`, see the `LICENSE` and `README.md`
//! in `tests/sparql-anything`), with the same expected graphs.

use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::Graph;
use oxigraph::model::dataset::CanonicalizationAlgorithm;
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use std::path::PathBuf;
use tripletrail_fx::{FxConfig, FxService};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/sparql-anything/markdown")
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
fn block_quote() {
    assert_isomorphic(
        triplify("location=BlockQuote.md,blank-nodes=false").unwrap(),
        expected("BlockQuote.ttl"),
    );
}

#[test]
fn bullet_list() {
    assert_isomorphic(
        triplify("location=BulletList.md,blank-nodes=false").unwrap(),
        expected("BulletList.ttl"),
    );
}

#[test]
fn code() {
    assert_isomorphic(
        triplify("location=Code.md,blank-nodes=false").unwrap(),
        expected("Code.ttl"),
    );
}

#[test]
fn document() {
    assert_isomorphic(
        triplify("location=Document.md,blank-nodes=false").unwrap(),
        expected("Document.ttl"),
    );
}

#[test]
fn document_2() {
    assert_isomorphic(
        triplify("location=Document_2.md,blank-nodes=false").unwrap(),
        expected("Document_2.ttl"),
    );
}

#[test]
fn documentation_example() {
    assert_isomorphic(
        triplify("location=DocumentationExample.md,blank-nodes=true").unwrap(),
        expected("DocumentationExample.ttl"),
    );
}

#[test]
fn emphasis() {
    assert_isomorphic(
        triplify("location=Emphasis.md,blank-nodes=false").unwrap(),
        expected("Emphasis.ttl"),
    );
}

#[test]
fn fenced_code_block() {
    assert_isomorphic(
        triplify("location=FencedCodeBlock.md,blank-nodes=false").unwrap(),
        expected("FencedCodeBlock.ttl"),
    );
}

#[test]
fn hard_line_break() {
    assert_isomorphic(
        triplify("location=HardLineBreak.md,blank-nodes=false").unwrap(),
        expected("HardLineBreak.ttl"),
    );
}

#[test]
fn heading() {
    assert_isomorphic(
        triplify("location=Heading.md,blank-nodes=false").unwrap(),
        expected("Heading.ttl"),
    );
}

#[test]
fn html_block() {
    assert_isomorphic(
        triplify("location=HtmlBlock.md,blank-nodes=false").unwrap(),
        expected("HtmlBlock.ttl"),
    );
}

#[test]
fn html_inline() {
    assert_isomorphic(
        triplify("location=HtmlInline.md,blank-nodes=false").unwrap(),
        expected("HtmlInline.ttl"),
    );
}

#[test]
fn image() {
    assert_isomorphic(
        triplify("location=Image.md,blank-nodes=false").unwrap(),
        expected("Image.ttl"),
    );
}

#[test]
fn indented_code_block() {
    assert_isomorphic(
        triplify("location=IndentedCodeBlock.md,blank-nodes=false").unwrap(),
        expected("IndentedCodeBlock.ttl"),
    );
}

#[test]
fn link() {
    assert_isomorphic(
        triplify("location=Link.md,blank-nodes=false").unwrap(),
        expected("Link.ttl"),
    );
}

#[test]
fn link_reference_definition() {
    assert_isomorphic(
        triplify("location=LinkReferenceDefinition.md,blank-nodes=false").unwrap(),
        expected("LinkReferenceDefinition.ttl"),
    );
}

#[test]
fn link_2() {
    assert_isomorphic(
        triplify("location=Link_2.md,blank-nodes=false").unwrap(),
        expected("Link_2.ttl"),
    );
}

#[test]
fn list_item() {
    assert_isomorphic(
        triplify("location=ListItem.md,blank-nodes=false").unwrap(),
        expected("ListItem.ttl"),
    );
}

#[test]
fn ordered_list() {
    assert_isomorphic(
        triplify("location=OrderedList.md,blank-nodes=false").unwrap(),
        expected("OrderedList.ttl"),
    );
}

#[test]
fn ordered_list_2() {
    assert_isomorphic(
        triplify("location=OrderedList_2.md,blank-nodes=false").unwrap(),
        expected("OrderedList_2.ttl"),
    );
}

#[test]
fn paragraph() {
    assert_isomorphic(
        triplify("location=Paragraph.md,blank-nodes=false").unwrap(),
        expected("Paragraph.ttl"),
    );
}

#[test]
fn soft_line_break() {
    assert_isomorphic(
        triplify("location=SoftLineBreak.md,blank-nodes=false").unwrap(),
        expected("SoftLineBreak.ttl"),
    );
}

#[test]
fn strong_emphasis() {
    assert_isomorphic(
        triplify("location=StrongEmphasis.md,blank-nodes=false").unwrap(),
        expected("StrongEmphasis.ttl"),
    );
}

#[test]
fn table() {
    assert_isomorphic(
        triplify("location=Table.md,blank-nodes=false").unwrap(),
        expected("Table.ttl"),
    );
}

#[test]
fn table_2() {
    assert_isomorphic(
        triplify("location=Table_2.md,blank-nodes=false").unwrap(),
        expected("Table_2.ttl"),
    );
}

#[test]
fn text() {
    assert_isomorphic(
        triplify("location=Text.md,blank-nodes=false").unwrap(),
        expected("Text.ttl"),
    );
}

#[test]
fn thematic_break() {
    assert_isomorphic(
        triplify("location=ThematicBreak.md,blank-nodes=false").unwrap(),
        expected("ThematicBreak.ttl"),
    );
}

#[test]
fn yaml_front_matter() {
    assert_isomorphic(
        triplify("location=YamlFrontMatter.md,blank-nodes=false").unwrap(),
        expected("YamlFrontMatter.ttl"),
    );
}

#[test]
fn nesting_past_the_limit_is_an_error() {
    let deep = |n: usize| {
        let dir = tripletrail_testdir::tempdir("fx-deep");
        let path = dir.join(format!("deep-{n}.md"));
        std::fs::write(&path, format!("{}deep", "> ".repeat(n))).unwrap();
        let out = triplify(&format!("location={}", path.display()));
        std::fs::remove_file(&path).unwrap();
        out
    };
    assert!(deep(100).is_ok());
    let e = deep(20_000).unwrap_err();
    assert!(e.contains("nest more than 256 deep"), "{e}");
}
