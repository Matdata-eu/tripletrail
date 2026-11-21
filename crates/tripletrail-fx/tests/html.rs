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

/// The default graph of an N-Quads file, with SPARQL Anything's placeholder `<location>`
/// for the source's location replaced by the file's IRI, as its tests replace it.
fn expected_nq(file: &str, source: &str) -> Graph {
    let location = format!(
        "file://{}",
        dir().join(source).canonicalize().unwrap().display()
    );
    let text = std::fs::read_to_string(dir().join(file))
        .unwrap()
        .replace("<location>", &format!("<{location}>"));
    RdfParser::from_format(RdfFormat::NQuads)
        .for_slice(text.as_bytes())
        .filter_map(|q| {
            let q = q.unwrap();
            q.graph_name
                .is_default_graph()
                .then(|| oxigraph::model::Triple::from(q))
        })
        .collect()
}

/// `html.metadata=true`: the Microdata of the page joins its Facade-X triples.
#[test]
fn microdata() {
    assert_isomorphic(
        triplify("location=Microdata1.html,blank-nodes=false,html.metadata=true").unwrap(),
        expected_nq("Microdata1.nq", "Microdata1.html"),
    );
}

/// `html.metadata=true`: the RDFa of the page (`vocab`, `typeof`, `property` with text and
/// with `href`) joins its Facade-X triples.
#[test]
fn rdfa() {
    assert_isomorphic(
        triplify("location=RDFa.html,blank-nodes=false,html.metadata=true").unwrap(),
        expected_nq("RDFa.nq", "RDFa.html"),
    );
}

/// The triples `html.metadata=true` adds for a page of `tests/html-metadata`, as N-Triples
/// lines with the page's IRI written `<location>` (blank nodes keep their labels).
fn metadata(file: &str) -> Vec<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/html-metadata")
        .join(file);
    let location = format!("<file://{}>", path.canonicalize().unwrap().display());
    let graph = triplify(&format!(
        "location={},html.metadata=true,blank-nodes=false",
        path.display()
    ))
    .unwrap();
    let mut lines: Vec<String> = graph
        .iter()
        .filter(|t| {
            !t.subject
                .to_string()
                .starts_with("<http://www.example.org/document")
        })
        .map(|t| format!("{t} .").replace(&location, "<location>"))
        .collect();
    lines.sort();
    lines
}

fn has(lines: &[String], line: &str) {
    assert!(
        lines.iter().any(|l| l == line),
        "{line} not in:\n{}",
        lines.join("\n")
    );
}

/// RDFa 1.1: `vocab`, `prefix` and the initial context's prefixes, `about`, `typeof`, text,
/// `content`, `href` and `src` values, `rel` and `rev`, a hanging `rel`, `inlist`, blank
/// nodes, terms, and `<base>`, which changes the base only from where it stands.
#[test]
fn rdfa_features() {
    let lines = metadata("rdfa.html");
    let s = "<http://example.org/base/page#brussels>";
    for line in [
        format!("{s} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://schema.org/TrainStation> ."),
        format!("{s} <http://schema.org/name> \"Brussels-Central\" ."),
        format!("{s} <http://example.org/ns#code> \"BC\" ."),
        format!("{s} <http://example.org/ns#nested> \"Some bold text\" ."),
        format!("{s} <http://schema.org/containedInPlace> <http://example.org/cities/brussels> ."),
        format!("{s} <http://schema.org/image> <http://example.org/base/pic.png> ."),
        format!("<http://example.org/lines/l1> <http://example.org/ns#servedBy> {s} ."),
        "<location> <http://purl.org/dc/terms/title> \"Stations\" .".into(),
        "<http://example.org/base/page> <http://ogp.me/ns#description> \"Railway stations\" .".into(),
        "<http://example.org/base/page> <http://www.w3.org/1999/xhtml/vocab#license> <http://creativecommons.org/licenses/by/4.0/> .".into(),
        "<http://example.org/base/page> <http://www.w3.org/ns/rdfa#usesVocabulary> <http://schema.org/> .".into(),
        // `vocab=""` drops the vocabulary: `next` is the XHTML term again.
        "<http://example.org/base/page> <http://www.w3.org/1999/xhtml/vocab#next> <http://example.org/base/page#n> .".into(),
        // The RDFa 1.0 processor also runs, and takes `ex:code` for an IRI (no `xmlns:ex`).
        format!("{s} <ex:code> \"BC\" ."),
    ] {
        has(&lines, &line);
    }
    // `inlist`: one list, in document order.
    let stops: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("<http://example.org/ns#stop>"))
        .collect();
    assert_eq!(stops.len(), 1, "{stops:?}");
    let first = |v: &str| {
        lines.iter().any(|l| {
            l.ends_with(&format!(
                "<http://www.w3.org/1999/02/22-rdf-syntax-ns#first> \"{v}\" ."
            ))
        })
    };
    assert!(first("Antwerp") && first("Mechelen"));
    // A hanging `rel`: the station has one platform node, with two typed resources (with
    // `typeof` and no `about`, `property` gives the new resource, not the text).
    let platform: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with(&format!("{s} <http://example.org/ns#platform>")))
        .collect();
    assert_eq!(platform.len(), 1, "{platform:?}");
    let node = platform[0].split(' ').nth(2).unwrap();
    let numbers = lines
        .iter()
        .filter(|l| l.starts_with(&format!("{node} <http://example.org/ns#number> _:")))
        .count();
    assert_eq!(numbers, 2, "{lines:?}");
    // `_:x` names one blank node.
    let blank = lines
        .iter()
        .find(|l| l.starts_with(&format!("{s} <http://example.org/ns#blank>")))
        .unwrap();
    let x = blank.split(' ').nth(2).unwrap();
    has(
        &lines,
        &format!("{x} <http://example.org/ns#label> \"X\" ."),
    );
    // Every literal is an xsd:string.
    assert!(!lines.iter().any(|l| l.contains("^^") || l.contains("\"@")));
}

/// A page written for RDFa 1.0 (`xmlns:` prefixes): both processors give the same triples,
/// and their blank nodes share labels, so the person is written once.
#[test]
fn rdfa_with_xmlns_prefixes() {
    let lines = metadata("rdfa-1.0.html");
    let book = "<http://example.org/book>";
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.starts_with(book))
            .cloned()
            .collect::<Vec<_>>(),
        [
            format!("{book} <http://purl.org/dc/elements/1.1/creator> \"Ann\" ."),
            format!(
                "{book} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://xmlns.com/foaf/0.1/Document> ."
            ),
            format!("{book} <http://xmlns.com/foaf/0.1/maker> <http://example.org/ann> ."),
        ]
    );
    let people: Vec<&String> = lines
        .iter()
        .filter(|l| l.ends_with("<http://xmlns.com/foaf/0.1/Person> ."))
        .collect();
    assert_eq!(people.len(), 1, "{lines:?}");
}

/// Microdata: `itemid`, `itemref`, nested items, several names in one `itemprop`, an
/// absolute property IRI, the values of `a`, `meta`, `time` and `data`, text with a line
/// break between blocks, and an item with no type (in `http://schema.org/`).
#[test]
fn microdata_items() {
    let lines = metadata("microdata.html");
    let s = "<http://example.org/stations/bc>";
    for line in [
        format!("<location> <http://www.w3.org/1999/xhtml/microdata#item> {s} ."),
        format!(
            "{s} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://schema.org/TrainStation> ."
        ),
        format!("{s} <https://schema.org/name> \"Brussels-Central\" ."),
        format!("{s} <https://schema.org/name> \"BXL-C\" ."),
        format!("{s} <https://schema.org/alternateName> \"BXL-C\" ."),
        format!("{s} <https://schema.org/description> \"A station\\nin the centre.\" ."),
        format!("{s} <https://schema.org/url> <http://example.org/bc> ."),
        format!("{s} <https://schema.org/identifier> \"BC\" ."),
        format!("{s} <https://schema.org/openingDate> \"1952-10-04\" ."),
        format!("{s} <https://schema.org/platforms> \"6\" ."),
        format!("{s} <https://schema.org/telephone> \"+32 2 528 28 28\" ."),
        format!("{s} <http://example.org/code> \"BC1\" ."),
    ] {
        has(&lines, &line);
    }
    let address = lines
        .iter()
        .find(|l| l.starts_with(&format!("{s} <https://schema.org/address>")))
        .unwrap();
    let a = address.split(' ').nth(2).unwrap();
    has(
        &lines,
        &format!("{a} <https://schema.org/addressLocality> \"Brussels\" ."),
    );
    has(
        &lines,
        &format!(
            "{a} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://schema.org/PostalAddress> ."
        ),
    );
    assert!(
        lines
            .iter()
            .any(|l| l.ends_with("<http://schema.org/name> \"untyped\" ."))
    );
    // Two top-level items: the nested one is not.
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.contains("microdata#item"))
            .count(),
        2
    );
}

/// A page nested as deep as HTML may be, full of RDFa and Microdata, on a test thread's
/// small stack.
#[test]
fn deep_metadata() {
    let dir = tripletrail_testdir::tempdir("fx-html-deep");
    let mut page = String::new();
    for n in 0..500 {
        page.push_str(&format!(
            "<div itemscope itemprop=\"p\" vocab=\"http://e/\" typeof=\"T\" rel=\"r\" property=\"v{n}\">"
        ));
    }
    page.push('x');
    let path = dir.path().join("deep.html");
    std::fs::write(&path, page).unwrap();
    let graph = triplify(&format!("location={},html.metadata=true", path.display())).unwrap();
    assert!(graph.len() > 1000, "{}", graph.len());
}

/// One item whose `itemref` names 20,000 elements, each a property: read in linear time.
#[test]
fn many_itemrefs() {
    let dir = tripletrail_testdir::tempdir("fx-html-itemref");
    let n = 20_000;
    let ids: Vec<String> = (0..n).map(|i| format!("i{i}")).collect();
    let mut page = format!("<div itemscope itemref=\"{}\"></div>", ids.join(" "));
    for i in 0..n {
        page.push_str(&format!("<span id=\"i{i}\" itemprop=\"p{i}\">v</span>"));
    }
    let path = dir.path().join("refs.html");
    std::fs::write(&path, page).unwrap();
    let start = std::time::Instant::now();
    let values = values(
        &format!("location={},html.metadata=true", path.display()),
        "?s <http://schema.org/p19999> ?v",
    )
    .unwrap();
    assert_eq!(values, ["\"v\""]);
    // About 1 s in a debug build; quadratic, it took 20 s.
    assert!(start.elapsed().as_secs() < 8, "{:?}", start.elapsed());
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
