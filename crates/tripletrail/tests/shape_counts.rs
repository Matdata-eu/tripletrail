//! Shape statistics of a validation (`--stats`): checks, distinct focus nodes and the distinct
//! focus nodes of each shape's results (#164).

use oxigraph::io::RdfFormat;
use oxigraph::model::{GraphNameRef, NamedNode, Term};
use tripletrail::Engine;

const DATA: &str = r#"
@prefix ex: <http://example.com/> .
ex:a a ex:Car , ex:Vehicle ; ex:owner ex:p .
ex:b a ex:Car ; ex:owner ex:p , ex:q .
ex:c a ex:Car , ex:Vehicle .
ex:p ex:name "p" .
"#;

// ex:a and ex:c are selected by both targets of ex:Car; ex:p is the owner of two cars, so
// ex:Person checks it twice; ex:q has no name.
const SHAPES: &str = r#"
@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://example.com/> .
ex:Car a sh:NodeShape ; sh:targetClass ex:Car , ex:Vehicle ;
  sh:property ex:Owner .
ex:Owner sh:path ex:owner ; sh:minCount 1 ; sh:node ex:Person .
ex:Person a sh:NodeShape ; sh:property ex:Name .
ex:Name sh:path ex:name ; sh:minCount 1 .
"#;

#[test]
fn shapes_count_their_distinct_and_failing_focus_nodes() {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    let shapes = NamedNode::new("http://example.com/shapes").unwrap();
    e.load_into(shapes.clone(), RdfFormat::Turtle, SHAPES.as_bytes())
        .unwrap();
    let mut results = 0;
    let counts = e
        .validate_counted(&shapes, GraphNameRef::DefaultGraph, true, &mut |_| {
            results += 1
        })
        .unwrap();
    let of = |name: &str| {
        let shape: Term = NamedNode::new(format!("http://example.com/{name}"))
            .unwrap()
            .into();
        let c = counts.iter().find(|c| c.shape == shape).unwrap();
        (c.focus_nodes, c.distinct_focus_nodes, c.failing_focus_nodes)
    };
    // ex:a, ex:b and ex:c, ex:a and ex:c selected by both targets but checked once. The
    // results are ex:Owner's: ex:c has no owner, ex:b's owner ex:q fails ex:Person.
    assert_eq!(of("Car"), (3, 3, 0));
    assert_eq!(of("Owner"), (3, 3, 2));
    // ex:p twice (owner of ex:a and ex:b), ex:q once; ex:q's missing name is a detail of
    // ex:b's result, counted for ex:Name.
    assert_eq!(of("Person"), (3, 2, 0));
    assert_eq!(of("Name"), (3, 2, 1));
    assert!(results > 0);

    // Without statistics there are no counts.
    let none = e
        .validate_counted(&shapes, GraphNameRef::DefaultGraph, false, &mut |_| {})
        .unwrap();
    assert!(none.is_empty());
}
