//! Row filters (#202), checked differentially: random datasets, hidden resources and
//! predicates, and queries; a query on the filtered view must answer as a store without the
//! hidden quads.

#[path = "support/grants.rs"]
#[allow(dead_code)]
mod grants;

/// A fixed number of inputs from a seeded generator, so a failure reproduces;
/// `GRANTS_INPUTS` sets how many (400 by default).
#[test]
fn a_filtered_view_answers_as_a_store_without_the_hidden_quads() {
    let mut state: u64 = 0x5851_f42d_4c95_7f2d;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let inputs: usize = std::env::var("GRANTS_INPUTS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(400);
    for _ in 0..inputs {
        let data: Vec<u8> = (0..160).map(|_| next() as u8).collect();
        grants::check_rows(&data);
    }
}

/// Row rules are an enterprise feature (#818): without its compiler, a query under row rules
/// is refused, never answered unfiltered, and the shapes of row rules are refused too.
#[test]
fn row_rules_without_the_enterprise_compiler_are_refused() {
    use std::sync::Arc;
    use tripletrail::oxigraph::model::{Graph, NamedNode};
    use tripletrail::{Engine, QueryOptions, RowRule, RowRules, RowShapes};
    assert!(!tripletrail::row_policies_available());
    let engine = Engine::new_tripletrail();
    engine.update("INSERT DATA { <urn:a> <urn:p> 1 }").unwrap();
    let rule = RowRule::VisibleIf {
        shape: NamedNode::new_unchecked("urn:Shape").into(),
        references: true,
    };
    let options = QueryOptions {
        row_rules: Some(RowRules {
            shapes: Arc::new(RowShapes::new(Graph::new())),
            rules: vec![rule.clone()],
            agent: None,
            graphs: None,
        }),
        ..QueryOptions::default()
    };
    let error = engine
        .query_with_options("SELECT * { ?s ?p ?o }", options)
        .err()
        .expect("refused");
    assert!(
        error.to_string().contains("TripleTrail Enterprise"),
        "{error}"
    );
    assert_eq!(
        tripletrail::check_row_shapes(&Graph::new(), &[rule]),
        Err(tripletrail::NEEDS_ENTERPRISE.to_owned())
    );
    assert_eq!(tripletrail::check_row_shapes(&Graph::new(), &[]), Ok(()));
}
