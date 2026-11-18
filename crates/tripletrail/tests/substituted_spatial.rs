//! A query with substituted variables (RDF4J `$var` bindings, the Python API) keeps the
//! GeoSPARQL rewrites: relation triples and virtual properties are inferred as without them.

use tripletrail::oxigraph::model::{Literal, NamedNode, Term};
use tripletrail::oxigraph::sparql::{QueryResults, Variable};
use tripletrail::{Engine, QueryOptions, ReadableGraphs};

const DATA: &str = r#"
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX ex: <http://e/>
INSERT DATA {
ex:area geo:hasGeometry ex:areaG . ex:areaG geo:asWKT "POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))"^^geo:wktLiteral .
ex:in geo:hasGeometry ex:inG . ex:inG geo:asWKT "POINT(5 5)"^^geo:wktLiteral .
ex:out geo:hasGeometry ex:outG . ex:outG geo:asWKT "POINT(50 5)"^^geo:wktLiteral .
}
"#;

fn rows(e: &Engine, query: &str, bind: &[(&str, Term)]) -> Vec<String> {
    rows_of(e, query, bind, "f")
}

fn rows_of(e: &Engine, query: &str, bind: &[(&str, Term)], var: &str) -> Vec<String> {
    let QueryResults::Solutions(s) = e
        .query_with_options(
            query,
            QueryOptions {
                substitutions: bind
                    .iter()
                    .map(|(v, t)| (Variable::new(*v).unwrap(), t.clone()))
                    .collect(),
                ..Default::default()
            },
        )
        .unwrap()
    else {
        panic!()
    };
    let mut out: Vec<String> = s
        .map(|s| {
            s.unwrap()
                .get(var)
                .map(ToString::to_string)
                .unwrap_or_default()
        })
        .collect();
    out.sort();
    out
}

#[test]
fn substituted_relation_triples_are_inferred() {
    let e = Engine::new_tripletrail();
    e.update(DATA).unwrap();
    let area: Term = NamedNode::new("http://e/area").unwrap().into();
    let q = "PREFIX geo: <http://www.opengis.net/ont/geosparql#>
             SELECT ?f ?a WHERE { ?f geo:sfWithin ?a }";
    assert_eq!(rows(&e, q, &[]).len(), 16);
    assert_eq!(
        rows(&e, q, &[("a", area.clone())]),
        [
            "<http://e/area>",
            "<http://e/areaG>",
            "<http://e/in>",
            "<http://e/inG>"
        ]
    );
    // The subject bound instead, and a feature that is within nothing but itself.
    let inside: Term = NamedNode::new("http://e/in").unwrap().into();
    let q = "PREFIX geo: <http://www.opengis.net/ont/geosparql#>
             SELECT ?f ?a WHERE { ?f geo:sfWithin ?a }";
    assert_eq!(
        rows_of(&e, q, &[("f", inside)], "a"),
        [
            "<http://e/area>",
            "<http://e/areaG>",
            "<http://e/in>",
            "<http://e/inG>"
        ]
    );
    let out: Term = NamedNode::new("http://e/out").unwrap().into();
    assert_eq!(
        rows(&e, q, &[("a", out)]),
        ["<http://e/out>", "<http://e/outG>"]
    );
}

#[test]
fn substituted_geometry_literals_meet_the_spatial_filters() {
    let e = Engine::new_tripletrail();
    e.update(DATA).unwrap();
    let wkt = |w: &str| -> Term {
        Literal::new_typed_literal(
            w,
            NamedNode::new("http://www.opengis.net/ont/geosparql#wktLiteral").unwrap(),
        )
        .into()
    };
    let q = "PREFIX geo: <http://www.opengis.net/ont/geosparql#>
             PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
             SELECT ?f ?box WHERE {
               ?f geo:hasGeometry/geo:asWKT ?w FILTER(geof:sfWithin(?w, ?box)) }";
    assert_eq!(
        rows(
            &e,
            q,
            &[("box", wkt("POLYGON((40 0, 60 0, 60 10, 40 10, 40 0))"))]
        ),
        ["<http://e/out>"]
    );
    assert_eq!(
        rows(&e, q, &[("box", wkt("POLYGON((1 1, 6 1, 6 6, 1 6, 1 1))"))]),
        ["<http://e/in>"]
    );
    // A virtual property of a substituted feature.
    let area: Term = NamedNode::new("http://e/area").unwrap().into();
    let q = "PREFIX geo: <http://www.opengis.net/ont/geosparql#>
             SELECT ?f ?d WHERE { ?f geo:dimension ?d }";
    assert_eq!(
        rows_of(&e, q, &[("f", area)], "d"),
        ["\"2\"^^<http://www.w3.org/2001/XMLSchema#integer>"]
    );
}

/// A caller limited to some graphs (#201) gets the relations of what it may read.
#[test]
fn substituted_relations_on_readable_graphs() {
    let e = Engine::new_tripletrail();
    e.update(
        &DATA
            .replace("INSERT DATA {", "INSERT DATA { GRAPH <http://e/g1> {")
            .replace("\n}\n", "\n} }\n"),
    )
    .unwrap();
    e.update(
        "PREFIX geo: <http://www.opengis.net/ont/geosparql#>
         INSERT DATA { GRAPH <http://e/g2> {
           <http://e/hidden> geo:hasGeometry <http://e/hiddenG> .
           <http://e/hiddenG> geo:asWKT \"POINT(6 6)\"^^geo:wktLiteral } }",
    )
    .unwrap();
    let area: Term = NamedNode::new("http://e/area").unwrap().into();
    let QueryResults::Solutions(s) = e
        .query_with_options(
            "PREFIX geo: <http://www.opengis.net/ont/geosparql#>
             SELECT ?f ?a WHERE { ?f geo:sfWithin ?a }",
            QueryOptions {
                substitutions: vec![(Variable::new("a").unwrap(), area)],
                readable: Some(ReadableGraphs {
                    default_graph: false,
                    named: vec![NamedNode::new("http://e/g1").unwrap()],
                }),
                ..Default::default()
            },
        )
        .unwrap()
    else {
        panic!()
    };
    let mut fs: Vec<String> = s
        .map(|s| s.unwrap().get("f").unwrap().to_string())
        .collect();
    fs.sort();
    assert_eq!(
        fs,
        [
            "<http://e/area>",
            "<http://e/areaG>",
            "<http://e/in>",
            "<http://e/inG>"
        ]
    );
}

/// The shortcuts that answer a whole query from the store (nearest neighbours, counts) stay
/// off for a query with substituted variables, which gets the answer of the same query with
/// the values written in it.
#[test]
fn substituted_values_answer_as_written_ones() {
    let e = Engine::new_tripletrail();
    let mut data = String::from(
        "PREFIX geo: <http://www.opengis.net/ont/geosparql#> PREFIX ex: <http://e/> INSERT DATA {",
    );
    for i in 0..40 {
        let class = if i % 3 == 0 { "A" } else { "B" };
        data.push_str(&format!(
            " ex:f{i} a ex:{class} ; geo:hasGeometry ex:g{i} . ex:g{i} geo:asWKT \"POINT({} {})\"^^geo:wktLiteral .",
            i as f64 * 0.37,
            (i * 7 % 11) as f64 * 0.53,
        ));
    }
    data.push('}');
    e.update(&data).unwrap();
    let point = "\"POINT(4.1 2.2)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>";
    let nearest = |p: &str| {
        format!(
            "PREFIX geo: <http://www.opengis.net/ont/geosparql#>
             PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
             SELECT ?f ?p WHERE {{ ?f geo:hasGeometry/geo:asWKT ?w BIND({p} AS ?p) }}
             ORDER BY (geof:distance(?w, ?p)) LIMIT 4"
        )
    };
    let bound: Term = Literal::new_typed_literal(
        "POINT(4.1 2.2)",
        NamedNode::new("http://www.opengis.net/ont/geosparql#wktLiteral").unwrap(),
    )
    .into();
    let written = rows(&e, &nearest(point), &[]);
    assert_eq!(written.len(), 4);
    let q = "PREFIX geo: <http://www.opengis.net/ont/geosparql#>
             PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
             SELECT ?f ?p WHERE { ?f geo:hasGeometry/geo:asWKT ?w }
             ORDER BY (geof:distance(?w, ?p)) LIMIT 4";
    assert_eq!(rows(&e, q, &[("p", bound)]), written);
    // A count per class, with the class bound or written.
    let class: Term = NamedNode::new("http://e/A").unwrap().into();
    let count = |c: &str| {
        format!("SELECT ?c (COUNT(*) AS ?f) WHERE {{ ?s a ?c FILTER(?c = {c}) }} GROUP BY ?c")
    };
    assert_eq!(
        rows(&e, &count("<http://e/A>"), &[]),
        ["\"14\"^^<http://www.w3.org/2001/XMLSchema#integer>"]
    );
    assert_eq!(
        rows(
            &e,
            "SELECT ?c (COUNT(*) AS ?f) WHERE { ?s a ?c } GROUP BY ?c",
            &[("c", class)]
        ),
        rows(&e, &count("<http://e/A>"), &[])
    );
}
