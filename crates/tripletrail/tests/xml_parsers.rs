//! The RDF/XML and SPARQL XML results parsers (vendored oxrdfxml and sparesults on
//! quick-xml 0.41, #133): entity references in text, and inputs that took quadratic time or
//! unbounded memory with quick-xml 0.37 (RUSTSEC-2026-0194, RUSTSEC-2026-0195).

use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::{Literal, Term, Triple};
use oxigraph::sparql::results::{
    QueryResultsFormat, QueryResultsParser, ReaderQueryResultsParserOutput,
};
use std::time::{Duration, Instant};

fn rdf_xml(input: &str) -> Result<Vec<Triple>, String> {
    RdfParser::from_format(RdfFormat::RdfXml)
        .for_reader(input.as_bytes())
        .map(|q| q.map(Triple::from).map_err(|e| e.to_string()))
        .collect()
}

fn object(input: &str) -> Term {
    let triples = rdf_xml(input).unwrap();
    assert_eq!(triples.len(), 1, "{triples:?}");
    triples[0].object.clone()
}

const HEAD: &str = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:ex="http://example.com/">"#;

#[test]
fn entity_references_in_text_are_part_of_the_literal() {
    let input = format!(
        "{HEAD}<rdf:Description rdf:about=\"http://example.com/s\"><ex:p>  a &amp; b &#x41;&#66; &lt;c&gt;  </ex:p></rdf:Description></rdf:RDF>"
    );
    assert_eq!(
        object(&input),
        Literal::new_simple_literal("  a & b AB <c>  ").into()
    );
}

#[test]
fn text_with_cdata_and_references_is_one_literal() {
    let input = format!(
        "{HEAD}<rdf:Description rdf:about=\"http://example.com/s\"><ex:p>x<![CDATA[<y>]]>&amp;z</ex:p></rdf:Description></rdf:RDF>"
    );
    assert_eq!(object(&input), Literal::new_simple_literal("x<y>&z").into());
}

#[test]
fn doctype_entities_resolve_in_text_and_attributes() {
    let input = r#"<?xml version="1.0"?>
<!DOCTYPE rdf:RDF [<!ENTITY ex "http://example.com/"> <!ENTITY name "Al &amp; Bo">]>
<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:ex="http://example.com/">
  <rdf:Description rdf:about="&ex;s"><ex:p>&name;!</ex:p></rdf:Description>
</rdf:RDF>"#;
    let triples = rdf_xml(input).unwrap();
    assert_eq!(triples.len(), 1);
    assert_eq!(triples[0].subject.to_string(), "<http://example.com/s>");
    assert_eq!(
        triples[0].object,
        Literal::new_simple_literal("Al & Bo!").into()
    );
}

#[test]
fn an_unknown_entity_is_an_error() {
    let input = format!(
        "{HEAD}<rdf:Description rdf:about=\"http://example.com/s\"><ex:p>&nope;</ex:p></rdf:Description></rdf:RDF>"
    );
    assert!(rdf_xml(&input).is_err());
}

#[test]
fn nested_entities_are_capped() {
    // Each level is ten copies of the one before: 10^9 bytes at the last level.
    let mut doctype = String::from("<!ENTITY e0 \"lollollol\">");
    for i in 1..=9 {
        let prev = format!("&e{};", i - 1);
        doctype.push_str(&format!("<!ENTITY e{i} \"{}\">", prev.repeat(10)));
    }
    let input = format!(
        "<?xml version=\"1.0\"?><!DOCTYPE rdf:RDF [{doctype}]>{HEAD}<rdf:Description rdf:about=\"http://example.com/s\"><ex:p>&e9;</ex:p></rdf:Description></rdf:RDF>"
    );
    let started = Instant::now();
    let error = rdf_xml(&input).unwrap_err();
    assert!(error.contains("longer than"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn many_attributes_parse_in_linear_time() {
    // RUSTSEC-2026-0194: checking a start tag for duplicate attribute names was quadratic.
    let attributes: String = (0..40_000).map(|i| format!(" ex:a{i}=\"{i}\"")).collect();
    let input = format!(
        "{HEAD}<rdf:Description rdf:about=\"http://example.com/s\"{attributes}/></rdf:RDF>"
    );
    let started = Instant::now();
    assert_eq!(rdf_xml(&input).unwrap().len(), 40_000);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}

fn xml_results(input: &str) -> Vec<Vec<Option<Term>>> {
    let ReaderQueryResultsParserOutput::Solutions(solutions) =
        QueryResultsParser::from_format(QueryResultsFormat::Xml)
            .for_reader(input.as_bytes())
            .unwrap()
    else {
        panic!("solutions expected")
    };
    solutions
        .map(|s| {
            let s = s.unwrap();
            s.values().to_vec()
        })
        .collect()
}

#[test]
fn xml_results_keep_entity_references_and_trim_whitespace() {
    let input = r#"<?xml version="1.0"?>
<sparql xmlns="http://www.w3.org/2005/sparql-results#">
  <head><variable name="a"/><variable name="b"/><variable name="c"/></head>
  <results>
    <result>
      <binding name="a"><literal>
        a &amp; b &lt;c&gt;
      </literal></binding>
      <binding name="b"><uri>http://example.com/?x=1&amp;y=2</uri></binding>
      <binding name="c"><literal>&#32;kept </literal></binding>
    </result>
  </results>
</sparql>"#;
    assert_eq!(
        xml_results(input),
        [[
            Some(Literal::new_simple_literal("a & b <c>").into()),
            Some(
                oxigraph::model::NamedNode::new("http://example.com/?x=1&y=2")
                    .unwrap()
                    .into()
            ),
            Some(Literal::new_simple_literal(" kept").into()),
        ]]
    );
}

#[test]
fn xml_boolean_results_parse() {
    let input = r#"<?xml version="1.0"?>
<sparql xmlns="http://www.w3.org/2005/sparql-results#"><head/>
  <boolean> true </boolean>
</sparql>"#;
    let output = QueryResultsParser::from_format(QueryResultsFormat::Xml)
        .for_reader(input.as_bytes())
        .unwrap();
    assert!(matches!(
        output,
        ReaderQueryResultsParserOutput::Boolean(true)
    ));
}
