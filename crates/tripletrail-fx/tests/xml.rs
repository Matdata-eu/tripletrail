//! XML sources, on the test files of SPARQL Anything's XML triplifier (Apache-2.0), with the
//! same expected graphs: without `xml.path` (as its StAX reader) and with it (as its
//! VTD-XML walk, quirks included).

use oxigraph::model::{Dataset, Graph};
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use tripletrail_fx::{FxConfig, FxService};

fn run(query: &str) -> Result<Vec<String>, String> {
    let fx = FxService::new(FxConfig::default(), SparqlEvaluator::new);
    let empty = Box::leak(Box::new(Dataset::new()));
    let results = fx
        .register(SparqlEvaluator::new())
        .parse_query(&format!(
            "PREFIX fxe: <http://sparql.xyz/facade-x/engine/> PREFIX xyz: <http://sparql.xyz/facade-x/data/> {query}"
        ))
        .unwrap()
        .on_queryable_dataset(&*empty)
        .execute()
        .map_err(|e| e.to_string())?;
    let QueryResults::Graph(triples) = results else {
        panic!("not a CONSTRUCT")
    };
    let g: Graph = triples
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let mut lines: Vec<String> = g
        .iter()
        .map(|t| {
            t.to_string()
                .replace("http://www.example.org/document", "doc")
                .replace("http://sparql.xyz/facade-x/data/", "xyz:")
                .replace("http://www.w3.org/1999/02/22-rdf-syntax-ns#", "rdf:")
                .replace("http://sparql.xyz/facade-x/ns/", "fx:")
        })
        .collect();
    lines.sort();
    Ok(lines)
}

/// The triples of `xml` with the option triples `options`, with IRI containers under
/// `http://www.example.org/document` (written `doc`).
fn triplify(xml: &str, options: &str) -> Vec<String> {
    try_triplify(xml, options).unwrap()
}

fn try_triplify(xml: &str, options: &str) -> Result<Vec<String>, String> {
    let content = oxigraph::model::Literal::new_simple_literal(xml);
    run(&format!(
        r#"CONSTRUCT {{ ?s ?p ?o }} WHERE {{ SERVICE <x-sparql-anything:> {{
            fxe:properties fxe:content {content} ; fxe:media-type "application/xml" ;
              fxe:root "http://www.example.org/document" ; fxe:blank-nodes "false" {options} .
            ?s ?p ?o }} }}"#
    ))
}

const LANG: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<doc xml:lang="en" title="Languages">
    <p>Inherited</p>
    <p xml:lang="it" note="nota">Sovrascritto <b>grassetto</b> fine</p>
    <p xml:lang="">No language <q xml:lang="fr">Rétabli</q></p>
    <p xml:lang="en-GB" xml:space="preserve">Colour</p>
    <p xml:lang="not a tag">Malformed</p>
    <p label="inherited attribute">Child attribute</p>
</doc>"#;

const BOOKS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<books>
    <book isbn="978-3-12-148410-0">
        <author>Gambardella, Matthew</author>
        <description>An in-depth look at creating applications
            with XML.</description>
    </book>
    <book isbn="928-3-16-148410-0">
        <title>Midnight Rain</title>
    </book>
</books>"#;

const RAILML: &str = r#"<?xml version='1.0' encoding='UTF-8'?>
<railML xmlns="https://www.railml.org/schemas/3.2">
  <switchesIS>
    <switchIS id="sw1" type="ordinarySwitch">
      <!-- a comment takes a slot number -->
      <name label="A" language="en" description="My first switch"/>
      <spotLocation id="sw1_sloc" netElementRef="ne1" applicationDirection="normal" pos="12.5">
        <linearCoordinate positioningSystemRef="lps1" measure="100.0"/>
      </spotLocation>
      <!-- this one belongs to spotLocation -->
      <external ref="x">some text</external>
    </switchIS>
  </switchesIS>
</railML>"#;

fn lines(text: &str) -> Vec<String> {
    let mut v: Vec<String> = text
        .lines()
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty())
        .collect();
    v.sort();
    v
}

/// Simple.xml: the root element is the root, typed by its name too; containers are IRIs
/// under the root by path.
#[test]
fn simple() {
    assert_eq!(
        triplify(
            "<!-- c -->\n<simple>\n    <element attr1=\"food\">Some text here</element>\n</simple>",
            ""
        ),
        lines(
            r#"
            <doc> <rdf:type> <fx:Root>
            <doc> <rdf:type> <xyz:simple>
            <doc> <rdf:_1> <doc/simple/1:element>
            <doc/simple/1:element> <rdf:type> <xyz:element>
            <doc/simple/1:element> <xyz:attr1> "food"
            <doc/simple/1:element> <rdf:_1> "Some text here"
            "#
        )
    );
}

/// LangTags.xml: text and attributes in the scope of `xml:lang` are tagged, `xml:lang=""`
/// and malformed tags remove the language, attributes of the XML namespace are not tagged.
/// Text and child elements share the slot numbers.
#[test]
fn language_tags() {
    let tagged = triplify(LANG, "");
    assert_eq!(
        tagged,
        lines(
            r#"
            <doc/doc/1:p> <rdf:_1> "Inherited"@en
            <doc/doc/1:p> <rdf:type> <xyz:p>
            <doc/doc/2:p/2:b> <rdf:_1> "grassetto"@it
            <doc/doc/2:p/2:b> <rdf:type> <xyz:b>
            <doc/doc/2:p> <http://www.w3.org/XML/1998/namespace#lang> "it"
            <doc/doc/2:p> <rdf:_1> "Sovrascritto "@it
            <doc/doc/2:p> <rdf:_2> <doc/doc/2:p/2:b>
            <doc/doc/2:p> <rdf:_3> " fine"@it
            <doc/doc/2:p> <rdf:type> <xyz:p>
            <doc/doc/2:p> <xyz:note> "nota"@it
            <doc/doc/3:p/2:q> <http://www.w3.org/XML/1998/namespace#lang> "fr"
            <doc/doc/3:p/2:q> <rdf:_1> "Rétabli"@fr
            <doc/doc/3:p/2:q> <rdf:type> <xyz:q>
            <doc/doc/3:p> <http://www.w3.org/XML/1998/namespace#lang> ""
            <doc/doc/3:p> <rdf:_1> "No language "
            <doc/doc/3:p> <rdf:_2> <doc/doc/3:p/2:q>
            <doc/doc/3:p> <rdf:type> <xyz:p>
            <doc/doc/4:p> <http://www.w3.org/XML/1998/namespace#lang> "en-GB"
            <doc/doc/4:p> <http://www.w3.org/XML/1998/namespace#space> "preserve"
            <doc/doc/4:p> <rdf:_1> "Colour"@en-gb
            <doc/doc/4:p> <rdf:type> <xyz:p>
            <doc/doc/5:p> <http://www.w3.org/XML/1998/namespace#lang> "not a tag"
            <doc/doc/5:p> <rdf:_1> "Malformed"
            <doc/doc/5:p> <rdf:type> <xyz:p>
            <doc/doc/6:p> <rdf:_1> "Child attribute"@en
            <doc/doc/6:p> <rdf:type> <xyz:p>
            <doc/doc/6:p> <xyz:label> "inherited attribute"@en
            <doc> <http://www.w3.org/XML/1998/namespace#lang> "en"
            <doc> <rdf:_1> <doc/doc/1:p>
            <doc> <rdf:_2> <doc/doc/2:p>
            <doc> <rdf:_3> <doc/doc/3:p>
            <doc> <rdf:_4> <doc/doc/4:p>
            <doc> <rdf:_5> <doc/doc/5:p>
            <doc> <rdf:_6> <doc/doc/6:p>
            <doc> <rdf:type> <fx:Root>
            <doc> <rdf:type> <xyz:doc>
            <doc> <xyz:title> "Languages"@en
            "#
        )
    );
    // LangNoTags: the same graph with plain strings.
    let plain = triplify(LANG, "; fxe:xml.lang-tags \"false\"");
    let untagged: Vec<String> = tagged
        .iter()
        .map(|l| {
            l.rsplit_once("\"@")
                .map_or(l.clone(), |(v, _)| format!("{v}\""))
        })
        .collect();
    assert_eq!(plain, lines(&untagged.join("\n")));
}

/// Namespaces, references, CDATA, comments and processing instructions without `xml.path`:
/// namespaced names are `<namespace#local>`, text around a comment is one value, CDATA is
/// text.
#[test]
fn namespaces_and_text() {
    assert_eq!(
        triplify(
            "<a xmlns:p='http://p/' xmlns:q='http://q' p:x='1' xml:lang='nb'>hei &amp; &#x41;<!--c-->du<q:b>x</q:b> <![CDATA[<y>]]><?pi v?></a>",
            ""
        ),
        lines(
            r#"
            <doc/a/2:q:b> <rdf:_1> "x"@nb
            <doc/a/2:q:b> <rdf:type> <http://q#b>
            <doc> <http://p/x> "1"@nb
            <doc> <http://www.w3.org/XML/1998/namespace#lang> "nb"
            <doc> <rdf:_1> "hei & Adu"@nb
            <doc> <rdf:_2> <doc/a/2:q:b>
            <doc> <rdf:_3> " <y>"@nb
            <doc> <rdf:type> <fx:Root>
            <doc> <rdf:type> <xyz:a>
            "#
        )
    );
    assert!(try_triplify("<a><b></a>", "").unwrap_err().contains("XML"));
    assert!(try_triplify("<p:a/>", "").unwrap_err().contains("prefix p"));
}

/// Books_1.xml with `xml.path=//book`: each book is a slot of the root, named by its
/// position and name.
#[test]
fn xpath_selects_slots_of_the_root() {
    assert_eq!(
        triplify(BOOKS, "; fxe:xml.path \"//book\""),
        lines(
            r#"
            <doc/1:book/1:author> <rdf:_1> "Gambardella, Matthew"
            <doc/1:book/1:author> <rdf:type> <xyz:author>
            <doc/1:book/2:description> <rdf:_1> "An in-depth look at creating applications\n            with XML."
            <doc/1:book/2:description> <rdf:type> <xyz:description>
            <doc/1:book> <rdf:_1> <doc/1:book/1:author>
            <doc/1:book> <rdf:_2> <doc/1:book/2:description>
            <doc/1:book> <rdf:type> <xyz:book>
            <doc/1:book> <xyz:isbn> "978-3-12-148410-0"
            <doc/2:book/1:title> <rdf:_1> "Midnight Rain"
            <doc/2:book/1:title> <rdf:type> <xyz:title>
            <doc/2:book> <rdf:_1> <doc/2:book/1:title>
            <doc/2:book> <rdf:type> <xyz:book>
            <doc/2:book> <xyz:isbn> "928-3-16-148410-0"
            <doc> <rdf:_1> <doc/1:book>
            <doc> <rdf:_2> <doc/2:book>
            <doc> <rdf:type> <fx:Root>
            "#
        )
    );
}

/// The railML shape of the TMS exports (and SPARQL Anything's MultiAttribute.xml): with
/// `xml.path`, SPARQL Anything walks VTD-XML tokens, so attributes beyond the selected
/// element's count take slot numbers (spotLocation's two extra attributes make its child
/// `rdf:_3`) and a comment after an element belongs to it and takes a slot number there.
#[test]
fn xpath_follows_the_vtd_walk() {
    assert_eq!(
        triplify(RAILML, "; fxe:xml.path \"//switchIS\""),
        lines(
            r#"
            <doc/1:switchIS/2:name> <rdf:type> <xyz:name>
            <doc/1:switchIS/2:name> <xyz:description> "My first switch"
            <doc/1:switchIS/2:name> <xyz:label> "A"
            <doc/1:switchIS/2:name> <xyz:language> "en"
            <doc/1:switchIS/3:spotLocation/3:linearCoordinate> <rdf:type> <xyz:linearCoordinate>
            <doc/1:switchIS/3:spotLocation/3:linearCoordinate> <xyz:measure> "100.0"
            <doc/1:switchIS/3:spotLocation/3:linearCoordinate> <xyz:positioningSystemRef> "lps1"
            <doc/1:switchIS/3:spotLocation> <rdf:_3> <doc/1:switchIS/3:spotLocation/3:linearCoordinate>
            <doc/1:switchIS/3:spotLocation> <rdf:type> <xyz:spotLocation>
            <doc/1:switchIS/3:spotLocation> <xyz:applicationDirection> "normal"
            <doc/1:switchIS/3:spotLocation> <xyz:id> "sw1_sloc"
            <doc/1:switchIS/3:spotLocation> <xyz:netElementRef> "ne1"
            <doc/1:switchIS/3:spotLocation> <xyz:pos> "12.5"
            <doc/1:switchIS/4:external> <rdf:_1> "some text"
            <doc/1:switchIS/4:external> <rdf:type> <xyz:external>
            <doc/1:switchIS/4:external> <xyz:ref> "x"
            <doc/1:switchIS> <rdf:_2> <doc/1:switchIS/2:name>
            <doc/1:switchIS> <rdf:_3> <doc/1:switchIS/3:spotLocation>
            <doc/1:switchIS> <rdf:_4> <doc/1:switchIS/4:external>
            <doc/1:switchIS> <rdf:type> <xyz:switchIS>
            <doc/1:switchIS> <xyz:id> "sw1"
            <doc/1:switchIS> <xyz:type> "ordinarySwitch"
            <doc> <rdf:_1> <doc/1:switchIS>
            <doc> <rdf:type> <fx:Root>
            "#
        )
    );
}

const XP: &str = "<a><b k=\"1\">one</b><b k=\"2\"><c>x</c></b><d><b k=\"3\"/></d></a>";

/// The `k` of the elements an XPath selects, or the values it puts on the root.
fn selected(xpath: &str) -> Vec<String> {
    triplify(XP, &format!("; fxe:xml.path \"{xpath}\""))
        .into_iter()
        .filter(|l| {
            (l.starts_with("<doc> ") && !l.contains("fx:Root") && !l.contains("<doc/"))
                || (l.contains("> <xyz:k>") && l.matches('/').count() == 1)
        })
        .map(|l| l.rsplit_once("> ").unwrap().1.to_owned())
        .collect()
}

#[test]
fn xpath_subset() {
    for (xpath, expected) in [
        ("//b", &["\"1\"", "\"2\"", "\"3\""][..]),
        ("//b[@k='2']", &["\"2\""]),
        ("/a/b[2]", &["\"2\""]),
        ("//b[1]", &["\"1\"", "\"3\""]),
        ("//b[last()]", &["\"2\"", "\"3\""]),
        ("//b[c='x']", &["\"2\""]),
        ("//b[c]", &["\"2\""]),
        ("//b[@k]", &["\"1\"", "\"2\"", "\"3\""]),
        ("/a/*[1]", &["\"1\""]),
        ("//c/..", &["\"2\""]),
        ("a/d/b", &["\"3\""]),
        ("/a/b/text()", &["\"one\""]),
    ] {
        let mut got = selected(xpath);
        got.sort();
        assert_eq!(got, expected, "{xpath}");
    }
    // An attribute is a key of the root.
    assert_eq!(
        triplify(XP, "; fxe:xml.path \"//b/@k\"")
            .into_iter()
            .filter(|l| l.contains("xyz:k"))
            .collect::<Vec<_>>(),
        [
            "<doc> <xyz:k> \"1\"",
            "<doc> <xyz:k> \"2\"",
            "<doc> <xyz:k> \"3\""
        ]
    );
    // `xml.path.1`, `xml.path.2`, ... in order, numbering on.
    assert_eq!(
        triplify(
            XP,
            "; <http://sparql.xyz/facade-x/engine/xml.path.2> \"//c\" ; <http://sparql.xyz/facade-x/engine/xml.path.1> \"//d\""
        )
        .into_iter()
        .filter(|l| l.starts_with("<doc> <rdf:_"))
        .collect::<Vec<_>>(),
        ["<doc> <rdf:_1> <doc/1:d>", "<doc> <rdf:_2> <doc/2:c>"]
    );
    let error = try_triplify(XP, "; fxe:xml.path \"//b[contains(., 'x')]\"").unwrap_err();
    assert!(error.contains("xml.path"), "{error}");
}

/// The format comes from the `.xml` extension.
#[test]
fn needs_and_format() {
    let dir = tripletrail_testdir::tempdir("fx-xml");
    std::fs::write(dir.join("railml.xml"), RAILML).unwrap();
    let ids = run(&format!(
        "CONSTRUCT {{ ?e <http://ex/id> ?id }} WHERE {{ SERVICE <x-sparql-anything:location={},xml.path=//switchIS> {{ ?e xyz:id ?id }} }}",
        dir.join("railml.xml").display()
    ))
    .unwrap();
    let mut ids: Vec<&str> = ids.iter().map(|l| l.rsplit_once(' ').unwrap().1).collect();
    ids.sort();
    assert_eq!(ids, ["\"sw1\"", "\"sw1_sloc\""]);
}

#[test]
fn nesting_past_the_limit_is_an_error() {
    let deep = |n: usize| format!("{}x{}", "<a>".repeat(n), "</a>".repeat(n));
    assert!(try_triplify(&deep(100), "").is_ok());
    let e = try_triplify(&deep(20_000), "").unwrap_err();
    assert!(e.contains("nest more than 512 deep"), "{e}");
}
