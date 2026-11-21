//! SHACL-SPARQL constraints, constraint components and targets, and SHACL-AF rules.

use crate::tests::{ex, graph};
use crate::{Inference, Shapes, Union, Validator};
use oxigraph::model::{NamedNode, Triple};

const BRIDGES: &str = r#"
ex:r1 a ex:Road ; ex:passesUnder ex:b1 .
ex:r2 a ex:Road ; ex:passesUnder ex:b2 .
ex:r3 a ex:Road .
ex:r4 a ex:Road ; ex:passesUnder _:b4 .
ex:b1 ex:clearance 4.5 ; ex:margin 0.2 .
ex:b2 ex:clearance 4.1 ; ex:margin 0.2 .
_:b4 ex:clearance 3.0 .
ex:truck ex:height 4.0 .
"#;

fn iri(local: &str) -> String {
    format!("<http://example.com/{local}>")
}

/// The focus nodes that do not conform, sorted, with `shapes` the shapes graph. Checks that
/// validating one node at a time (without prefetching) gives the same answers.
fn failing(shapes: &str, data: &str) -> Vec<String> {
    let shapes = Shapes::compile(&graph(shapes)).unwrap();
    let data = graph(data);
    let v = Validator::new(&shapes, &data);
    let mut out: Vec<String> = v
        .validate()
        .into_iter()
        .map(|(f, _)| f.to_string())
        .collect();
    assert!(v.take_error().is_none());
    out.sort();
    out.dedup();
    let one = Validator::new(&shapes, &data);
    let mut again: Vec<String> = Vec::new();
    for &s in shapes.targeted() {
        for f in one.focus_nodes(s) {
            if !one.conforms_to(&f, s) {
                again.push(f.to_string());
            }
        }
    }
    again.sort();
    again.dedup();
    assert_eq!(out, again);
    out
}

#[test]
fn sparql_constraint() {
    // A road passing under a bridge whose clearance less its margin (0 when none is given)
    // is not above the truck's height.
    let shapes = r#"
    ex:prefixes sh:declare [ sh:prefix "ex" ; sh:namespace "http://example.com/"^^xsd:anyURI ] .
    ex:S a sh:NodeShape ; sh:targetClass ex:Road ;
        sh:sparql [ sh:prefixes ex:prefixes ; sh:select """
            SELECT $this ?bridge WHERE {
                $this ex:passesUnder ?bridge . ?bridge ex:clearance ?c .
                OPTIONAL { ?bridge ex:margin ?m }
                ex:truck ex:height ?h .
                FILTER (?c - COALESCE(?m, 0) <= ?h)
            }""" ] ."#;
    // r4 passes under a blank node: blank nodes are bound like IRIs.
    assert_eq!(failing(shapes, BRIDGES), [iri("r2"), iri("r4")]);
}

#[test]
fn sparql_constraint_without_this_projected() {
    // Projects only ?b: $this is added to the projection.
    let shapes = r#"
    ex:S a sh:NodeShape ; sh:targetClass ex:Road ;
        sh:sparql [ sh:select """
            SELECT ?b WHERE { $this <http://example.com/passesUnder> ?b }""" ] ."#;
    assert_eq!(failing(shapes, BRIDGES), [iri("r1"), iri("r2"), iri("r4")]);
    // Aggregates without grouping by $this: run per focus node.
    let shapes = r#"
    ex:S a sh:NodeShape ; sh:targetClass ex:Road ;
        sh:sparql [ sh:select """
            SELECT (COUNT(?b) AS ?n) WHERE { $this <http://example.com/passesUnder> ?b }
            HAVING (COUNT(?b) = 0)""" ] ."#;
    assert_eq!(failing(shapes, BRIDGES), [iri("r3")]);
}

#[test]
fn sparql_property_shape_and_deactivated() {
    let shapes = r#"
    ex:S a sh:NodeShape ; sh:targetClass ex:Road ;
        sh:property [ sh:path ( ex:passesUnder ex:clearance ) ;
            sh:sparql [ sh:select """
                SELECT $this ?value WHERE { $this $PATH ?value . FILTER (?value < 4) }""" ] ,
                [ sh:deactivated true ; sh:select "SELECT $this WHERE { }" ] ] ."#;
    assert_eq!(failing(shapes, BRIDGES), [iri("r4")]);
}

#[test]
fn sparql_path_projected() {
    // ERA's RINF shapes project $PATH (`SELECT $this $PATH WHERE`): it is no path there, and
    // the constraint still works on its triple patterns. Every label needs an English one.
    let shapes = r#"
    ex:S a sh:NodeShape ; sh:targetClass ex:Road ;
        sh:property [ sh:path ex:label ;
            sh:sparql [ sh:select """
                SELECT $this $PATH
                WHERE {
                    # If the property exists
                    $this $PATH ?any .
                    FILTER NOT EXISTS { $this $PATH ?v . FILTER (LANGMATCHES(LANG(?v), "en")) }
                }""" ] ] ."#;
    let data = r#"
    ex:r1 a ex:Road ; ex:label "Ring"@en, "Ringvei"@no .
    ex:r2 a ex:Road ; ex:label "Ringvei"@no .
    ex:r3 a ex:Road ."#;
    assert_eq!(failing(shapes, data), [iri("r2")]);
}

#[test]
fn sparql_current_shape_and_shapes_graph() {
    let shapes_graph = graph(
        r#"
    ex:S a sh:NodeShape ; sh:targetClass ex:Road ; ex:limit 4.2 ;
        sh:sparql [ sh:select """
            SELECT $this WHERE {
                GRAPH $shapesGraph { $currentShape <http://example.com/limit> ?limit }
                $this <http://example.com/passesUnder>/<http://example.com/clearance> ?c .
                FILTER (?c < ?limit)
            }""" ] ."#,
    );
    let shapes = Shapes::compile(&shapes_graph).unwrap();
    let data = graph(BRIDGES);
    let name = NamedNode::new_unchecked("http://example.com/shapes");
    let v = Validator::new(&shapes, &data).with_shapes_graph(&name, &shapes_graph);
    let mut out: Vec<String> = v
        .validate()
        .into_iter()
        .map(|(f, _)| f.to_string())
        .collect();
    out.sort();
    assert_eq!(out, [iri("r2"), iri("r4")]);
}

#[test]
fn sparql_constraint_components() {
    let shapes = r#"
    # ASK validator: every value at least $min above ex:base of the focus node.
    ex:AboveBase a sh:ConstraintComponent ;
        sh:parameter [ sh:path ex:min ] ;
        sh:validator [ a sh:SPARQLAskValidator ; sh:ask """
            ASK { $this <http://example.com/base> ?b . FILTER ($value - ?b >= $min) }""" ] .
    # SELECT node validator, with an optional parameter.
    ex:HasKind a sh:ConstraintComponent ;
        sh:parameter [ sh:path ex:kind ] , [ sh:path ex:other ; sh:optional true ] ;
        sh:nodeValidator [ a sh:SPARQLSelectValidator ; sh:select """
            SELECT $this WHERE {
                FILTER NOT EXISTS {
                    $this <http://example.com/kind> ?k FILTER (?k = $kind || ?k = $other)
                }
            }""" ] .
    ex:P a sh:NodeShape ; sh:targetSubjectsOf ex:base ;
        sh:property [ sh:path ex:v ; ex:min 2 ] .
    ex:K a sh:NodeShape ; sh:targetSubjectsOf ex:kind ; ex:kind ex:A ; ex:other ex:B .
    "#;
    let data = r#"
    ex:a ex:base 1 ; ex:v 3, 4 .
    ex:b ex:base 1 ; ex:v 3, 2 .
    ex:c ex:base 5 .
    ex:k1 ex:kind ex:A . ex:k2 ex:kind ex:B . ex:k3 ex:kind ex:C .
    "#;
    assert_eq!(failing(shapes, data), [iri("b"), iri("k3")]);
}

#[test]
fn sparql_targets() {
    let shapes = r#"
    ex:S a sh:NodeShape ;
        sh:target [ a sh:SPARQLTarget ; sh:select """
            SELECT ?this WHERE { ?this <http://example.com/passesUnder> ?b }""" ] ;
        sh:property [ sh:path ex:passesUnder ; sh:class ex:Bridge ] .
    ex:Under a sh:SPARQLTargetType ; sh:parameter [ sh:path ex:bridge ] ;
        sh:select "SELECT ?this WHERE { ?this <http://example.com/passesUnder> $bridge }" .
    ex:T a sh:NodeShape ; sh:target [ a ex:Under ; ex:bridge ex:b1 ] ; sh:class ex:Nothing .
    "#;
    let data = format!("{BRIDGES} ex:b1 a ex:Bridge . ex:b2 a ex:Bridge .");
    // S: r4's bridge is no ex:Bridge; T: r1 passes under b1 and is no ex:Nothing.
    assert_eq!(failing(shapes, &data), [iri("r1"), iri("r4")]);
}

#[test]
fn sparql_errors() {
    // A syntax error, $PATH in a node shape, an ASK as sh:select, an unknown target.
    for s in [
        r#"ex:S sh:targetNode ex:a ; sh:sparql [ sh:select "SELECT WHERE" ] ."#,
        r#"ex:S sh:targetNode ex:a ; sh:sparql [ sh:select "SELECT $this WHERE { $this $PATH ?v }" ] ."#,
        r#"ex:S sh:targetNode ex:a ; sh:sparql [ sh:select "ASK { }" ] ."#,
        r#"ex:S sh:targetNode ex:a ; sh:target [ ex:foo 1 ] ."#,
        // Not allowed with pre-binding.
        r#"ex:S sh:targetNode ex:a ; sh:sparql [ sh:select "SELECT $this WHERE { SERVICE <http://example.com/nowhere> { ?s ?p ?o } }" ] ."#,
        r#"ex:S sh:targetNode ex:a ; sh:sparql [ sh:select "SELECT $this WHERE { $this ?p ?o MINUS { $this a ex:C } }" ] ."#,
        r#"ex:S sh:targetNode ex:a ; sh:sparql [ sh:select "SELECT $this WHERE { VALUES ?o { 1 } $this ?p ?o }" ] ."#,
        r#"ex:S sh:targetNode ex:a ; sh:sparql [ sh:select "SELECT $this WHERE { $this ?p ?o { SELECT ?o WHERE { ?s ?q ?o } } }" ] ."#,
        r#"ex:S sh:targetNode ex:a ; sh:sparql [ sh:select "SELECT ?this WHERE { BIND (ex:a AS ?this) }" ] ."#,
        r#"ex:S sh:targetNode ex:a ; sh:sparql [ sh:select "SELECT $this WHERE { $this ?p ?o FILTER NOT EXISTS { VALUES ?o { 1 } } }" ] ."#,
    ] {
        assert!(Shapes::compile(&graph(s)).is_err(), "{s}");
    }
    // A query failing at evaluation makes its focus nodes fail and says why.
    let shapes = Shapes::compile(&graph(
        r#"ex:S sh:targetNode ex:a ; sh:sparql [ sh:select "SELECT $this WHERE { BIND (<http://example.com/unknown-function>(1) AS ?x) FILTER (?x) }" ] ."#,
    ))
    .unwrap();
    let data = graph("");
    let v = Validator::new(&shapes, &data);
    assert!(!v.conforms_to(&ex("a"), shapes.shape(&ex("S")).unwrap()));
    assert!(v.take_error().is_some());
}

// ---- SHACL-AF rules ----

#[test]
fn triple_rule_feeds_a_core_constraint() {
    let shapes = graph(
        r#"
    # The clearance of a road is that of the bridge it passes under.
    ex:Derive a sh:NodeShape ; sh:targetClass ex:Road ;
        sh:rule [ a sh:TripleRule ; sh:subject sh:this ; sh:predicate ex:clearance ;
            sh:object [ sh:path ( ex:passesUnder ex:clearance ) ] ] .
    ex:Check a sh:NodeShape ; sh:targetClass ex:Road ;
        sh:property [ sh:path ex:clearance ; sh:minInclusive 4.2 ] ."#,
    );
    let shapes = Shapes::compile(&shapes).unwrap();
    assert!(shapes.has_rules());
    assert!(shapes.reads().is_none());
    let data = graph(BRIDGES);
    let inferred = Inference::new(&shapes, &data).run().unwrap();
    assert_eq!(inferred.len(), 3);
    let union = Union(vec![&data, &inferred]);
    let v = Validator::new(&shapes, &union);
    let mut out: Vec<String> = v
        .validate()
        .into_iter()
        .map(|(f, _)| f.to_string())
        .collect();
    out.sort();
    assert_eq!(out, [iri("r2"), iri("r4")]);
}

#[test]
fn sparql_rules_order_conditions_and_fixpoint() {
    let shapes = graph(
        r#"
    ex:Person a sh:NodeShape ; sh:targetSubjectsOf ex:parent ;
        # The transitive closure, which needs several passes.
        sh:rule [ a sh:SPARQLRule ; sh:order 2 ; sh:construct """
            CONSTRUCT { $this <http://example.com/ancestor> ?z }
            WHERE { $this <http://example.com/ancestor> ?y . ?y <http://example.com/ancestor> ?z }""" ] ;
        sh:rule [ a sh:TripleRule ; sh:order 1 ; sh:subject sh:this ;
            sh:predicate ex:ancestor ; sh:object [ sh:path ex:parent ] ] ;
        # Only for royal people; a filter shape picks their royal parents.
        sh:rule [ a sh:TripleRule ; sh:condition ex:Royal ; sh:subject sh:this ;
            sh:predicate ex:royalParent ;
            sh:object [ sh:filterShape ex:Royal ; sh:nodes [ sh:path ex:parent ] ] ] ;
        sh:rule [ a sh:TripleRule ; sh:deactivated true ; sh:subject sh:this ;
            sh:predicate ex:never ; sh:object 1 ] .
    ex:Royal a sh:NodeShape ; sh:property [ sh:path ex:royal ; sh:hasValue true ] ."#,
    );
    let data = graph(
        r#"ex:a ex:parent ex:b . ex:b ex:parent ex:c . ex:c ex:parent ex:d .
        ex:a ex:royal true . ex:b ex:royal true ."#,
    );
    let shapes = Shapes::compile(&shapes).unwrap();
    let inferred = Inference::new(&shapes, &data).run().unwrap();
    let has = |s: &str, p: &str, o: &str| {
        inferred.contains(&Triple::new(
            NamedNode::new_unchecked(format!("http://example.com/{s}")),
            NamedNode::new_unchecked(format!("http://example.com/{p}")),
            ex(o),
        ))
    };
    assert!(has("a", "ancestor", "b"));
    assert!(has("a", "ancestor", "d"));
    assert!(has("b", "ancestor", "d"));
    assert!(!has("d", "ancestor", "a"));
    assert!(has("a", "royalParent", "b"));
    assert!(!has("b", "royalParent", "c"));
    // 6 ancestors (a: b c d, b: c d, c: d) and one royal parent.
    assert_eq!(inferred.len(), 7, "{inferred}");
}

#[test]
fn rule_errors() {
    for s in [
        r#"ex:S sh:targetNode ex:a ; sh:rule [ a sh:TripleRule ; sh:subject sh:this ] ."#,
        r#"ex:S sh:targetNode ex:a ; sh:rule [ sh:construct "SELECT * { }" ] ."#,
        r#"ex:S sh:targetNode ex:a ; sh:rule [ sh:subject sh:this ; sh:predicate ex:p ; sh:object [ ex:x 1 ] ] ."#,
    ] {
        assert!(Shapes::compile(&graph(s)).is_err(), "{s}");
    }
}

#[test]
fn rules_are_stratified_and_rerun_only_when_their_inputs_changed() {
    let shapes = graph(
        r#"
    ex:Person a sh:NodeShape ; sh:targetSubjectsOf ex:parent ;
        sh:rule [ a sh:SPARQLRule ; sh:order 2 ; sh:construct """
            CONSTRUCT { $this <http://example.com/ancestor> ?z }
            WHERE { $this <http://example.com/ancestor> ?y . ?y <http://example.com/ancestor> ?z }""" ] ;
        sh:rule [ a sh:TripleRule ; sh:order 1 ; sh:subject sh:this ;
            sh:predicate ex:ancestor ; sh:object [ sh:path ex:parent ] ] ;
        # Reads only ex:name: nothing it reads is ever inferred.
        sh:rule [ a sh:SPARQLRule ; sh:order 3 ; sh:construct """
            CONSTRUCT { $this <http://example.com/label> ?n }
            WHERE { $this <http://example.com/name> ?n }""" ] ;
        # A variable predicate, which its FILTER keeps to ex:ancestor: runs once, after the
        # closure.
        sh:rule [ a sh:SPARQLRule ; sh:order 4 ; sh:construct """
            CONSTRUCT { $this <http://example.com/linked> true }
            WHERE { $this ?p ?o FILTER(?p = <http://example.com/ancestor>) }""" ] ."#,
    );
    let data = graph(
        r#"ex:a ex:parent ex:b ; ex:name "a" . ex:b ex:parent ex:c . ex:c ex:parent ex:d .
        ex:d ex:parent ex:e ."#,
    );
    let shapes = Shapes::compile(&shapes).unwrap();
    let runs = std::cell::RefCell::new(Vec::new());
    let observe = |r: &crate::RuleRun<'_>| {
        if r.done {
            runs.borrow_mut().push((r.pass, r.rule.to_string(), r.new));
        }
    };
    let inferred = Inference::new(&shapes, &data)
        .with_observer(&observe)
        .run()
        .unwrap();
    // 10 ancestors, one label, four linked.
    assert_eq!(inferred.len(), 15, "{inferred}");
    let runs = runs.into_inner();
    let passes = |order: &str| -> Vec<usize> {
        let rule = shapes
            .rules
            .iter()
            .find(|r| r.order.to_string() == order)
            .map(|r| r.node.to_string())
            .unwrap();
        runs.iter()
            .filter(|(_, n, _)| *n == rule)
            .map(|(p, _, _)| *p)
            .collect()
    };
    // The parent rule reads nothing inferred; the label rule neither.
    assert_eq!(passes("1"), [1], "{runs:?}");
    assert_eq!(passes("3"), [1], "{runs:?}");
    // The closure (fed by itself) runs until a pass adds nothing; the filtered rule reads
    // only what the closure writes, so it runs once, after it.
    assert_eq!(passes("2"), [1, 2, 3], "{runs:?}");
    assert_eq!(passes("4"), [1], "{runs:?}");
    let order: Vec<&String> = runs.iter().map(|(_, n, _)| n).collect();
    let first = |o: &str| {
        let rule = shapes
            .rules
            .iter()
            .find(|r| r.order.to_string() == o)
            .unwrap();
        order
            .iter()
            .position(|n| **n == rule.node.to_string())
            .unwrap()
    };
    assert!(first("1") < first("2") && first("2") < first("3") && first("3") < first("4"));
}

#[test]
fn a_rule_before_the_rule_it_reads_in_sh_order_runs_after_it_once() {
    // ex:b is written by the rule of order 2 and read by the rule of order 1: stratified, the
    // order-1 rule runs once, after it, instead of once before and again in a second pass.
    let shapes = graph(
        r#"
    ex:S a sh:NodeShape ; sh:targetSubjectsOf ex:a ;
        sh:rule [ a sh:TripleRule ; sh:order 1 ; sh:subject sh:this ;
            sh:predicate ex:c ; sh:object [ sh:path ex:b ] ] ;
        sh:rule [ a sh:TripleRule ; sh:order 2 ; sh:subject sh:this ;
            sh:predicate ex:b ; sh:object [ sh:path ex:a ] ] ."#,
    );
    let data = graph("ex:x ex:a ex:y . ex:z ex:a ex:w .");
    let shapes = Shapes::compile(&shapes).unwrap();
    let runs = std::cell::RefCell::new(Vec::new());
    let observe = |r: &crate::RuleRun<'_>| {
        if r.done {
            runs.borrow_mut().push((r.pass, r.new));
        }
    };
    let inferred = Inference::new(&shapes, &data)
        .with_observer(&observe)
        .run()
        .unwrap();
    assert_eq!(inferred.len(), 4, "{inferred}");
    assert_eq!(runs.into_inner(), [(1, 2), (1, 2)]);
}

#[test]
fn a_default_rule_with_not_exists_runs_after_the_rule_it_reads_once() {
    // The default rule (order 1) reads ex:b in its NOT EXISTS, which the rule of order 2
    // writes: it runs once, after it, and no rule runs a second time.
    let shapes = graph(
        r#"
    ex:S a sh:NodeShape ; sh:targetSubjectsOf ex:a ;
        sh:rule [ a sh:SPARQLRule ; sh:order 1 ; sh:construct """
            CONSTRUCT { $this <http://example.com/b> "default" }
            WHERE { FILTER NOT EXISTS { $this <http://example.com/b> ?any } }""" ] ;
        sh:rule [ a sh:SPARQLRule ; sh:order 2 ; sh:construct """
            CONSTRUCT { $this <http://example.com/b> ?v }
            WHERE { $this <http://example.com/a> ?v FILTER(?v != <http://example.com/w>) }""" ] ;
        # Reads only ex:a, in an EXISTS: never affected by what is inferred.
        sh:rule [ a sh:SPARQLRule ; sh:order 3 ; sh:construct """
            CONSTRUCT { $this <http://example.com/c> true }
            WHERE { FILTER EXISTS { $this <http://example.com/a> ?v } }""" ] ."#,
    );
    let data = graph("ex:x ex:a ex:y . ex:z ex:a ex:w .");
    let shapes = Shapes::compile(&shapes).unwrap();
    let runs = std::cell::RefCell::new(Vec::new());
    let observe = |r: &crate::RuleRun<'_>| {
        if r.done {
            runs.borrow_mut().push((r.pass, r.new));
        }
    };
    let inferred = Inference::new(&shapes, &data)
        .with_observer(&observe)
        .run()
        .unwrap();
    // ex:x ex:b ex:y, ex:z ex:b "default", and ex:c for both.
    assert_eq!(inferred.len(), 4, "{inferred}");
    let mut runs = runs.into_inner();
    runs.sort();
    assert_eq!(runs, [(1, 1), (1, 1), (1, 2)]);
}

#[test]
fn a_rule_with_an_empty_where_constructs_once_per_focus_node() {
    let shapes = graph(
        r#"
    ex:S a sh:NodeShape ; sh:targetClass ex:C ;
        sh:rule [ a sh:SPARQLRule ; sh:construct """
            CONSTRUCT { $this <http://example.com/q> "2" } WHERE { }""" ] ."#,
    );
    let data = graph(r#"ex:a a ex:C . ex:b a ex:C . ex:c a ex:D ."#);
    let shapes = Shapes::compile(&shapes).unwrap();
    let inferred = Inference::new(&shapes, &data).run().unwrap();
    assert_eq!(inferred.len(), 2, "{inferred}");
}

#[test]
fn with_existing_returns_the_constructed_triples_already_in_the_data() {
    let shapes = graph(
        r#"
    ex:S a sh:NodeShape ; sh:targetClass ex:C ;
        sh:rule [ a sh:SPARQLRule ; sh:construct """
            CONSTRUCT { $this <http://example.com/p> "1" . $this <http://example.com/q> "2" }
            WHERE { $this a <http://example.com/C> }""" ] ;
        sh:rule [ a sh:TripleRule ; sh:subject sh:this ; sh:predicate rdf:type ;
            sh:object ex:C ] ."#,
    );
    let data = graph(r#"ex:a a ex:C ; ex:p "1" ."#);
    let shapes = Shapes::compile(&shapes).unwrap();
    let new = Inference::new(&shapes, &data).run().unwrap();
    assert_eq!(new.len(), 1, "{new}");
    let all = Inference::new(&shapes, &data)
        .with_existing(true)
        .run()
        .unwrap();
    // ex:q (new), ex:p and rdf:type (already in the data), once each.
    assert_eq!(all.len(), 3, "{all}");
}

#[test]
fn predicates_read_only_under_a_negation_do_not_trigger_a_rule() {
    let rule = |r#where: &str| {
        let shapes = graph(&format!(
            r#"
    ex:S a sh:NodeShape ; sh:targetNode ex:x ;
        sh:rule [ a sh:SPARQLRule ; sh:construct """
            CONSTRUCT {{ $this <http://example.com/out> true }} WHERE {{ {} }}""" ] ."#,
            r#where.replace("ex:", "http://example.com/")
        ));
        let shapes = Shapes::compile(&shapes).unwrap();
        let r = crate::rules::rule_reads(&shapes, &shapes.rules[0]).unwrap();
        let short = |s: &std::collections::BTreeSet<String>| -> Vec<String> {
            s.iter()
                .filter_map(|p| p.strip_prefix("http://example.com/"))
                .map(str::to_owned)
                .collect()
        };
        (short(&r.all), short(&r.positive))
    };
    let both = |all: &[&str], positive: &[&str]| {
        (
            all.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            positive.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
    };
    assert_eq!(
        rule("$this <ex:a> ?v FILTER NOT EXISTS { $this <ex:b> ?w }"),
        both(&["a", "b"], &["a"])
    );
    assert_eq!(
        rule("$this <ex:a> ?v MINUS { $this <ex:b> ?w }"),
        both(&["a", "b"], &["a"])
    );
    // A negation of a negation, and negations whose effect may go either way, are positive.
    assert_eq!(
        rule("FILTER NOT EXISTS { $this <ex:a> ?v FILTER NOT EXISTS { ?v <ex:b> ?w } }"),
        both(&["a", "b"], &["b"])
    );
    assert_eq!(
        rule("$this <ex:a> ?v OPTIONAL { $this <ex:b> ?w } FILTER(!BOUND(?w))"),
        both(&["a", "b"], &["a", "b"])
    );
    assert_eq!(
        rule("BIND(IF(NOT EXISTS { $this <ex:b> ?w }, 1, 2) AS ?n)"),
        both(&["b"], &["b"])
    );
    assert_eq!(
        rule("FILTER EXISTS { $this <ex:b> ?w }"),
        both(&["b"], &["b"])
    );
    // A predicate variable filtered to constants, also by an OPTIONAL's own filter.
    assert_eq!(
        rule("$this <ex:a> ?i . ?i ?p ?v FILTER(?p IN (<ex:b>, <ex:c>))"),
        both(&["a", "b", "c"], &["a", "b", "c"])
    );
    assert_eq!(
        rule("$this <ex:a> ?i OPTIONAL { ?i ?p ?v FILTER(?p IN (<ex:b>, <ex:c>)) }"),
        both(&["a", "b", "c"], &["a", "b", "c"])
    );
}

#[test]
fn types_are_matched_per_class_and_bound_predicates_are_known() {
    // ex:A instances get ex:mark; a rule typing things ex:B (a subclass of ex:C) feeds only
    // the rule on ex:C, not the one on ex:A. The BIND(IF) rule writes ex:yes or ex:no only.
    let shapes = graph(
        r#"
    ex:SA a sh:NodeShape ; sh:targetClass ex:A ;
        sh:rule [ a sh:SPARQLRule ; sh:order 3 ; sh:construct """
            CONSTRUCT { $this <http://example.com/mark> true } WHERE { }""" ] .
    ex:SC a sh:NodeShape ; sh:targetClass ex:C ;
        sh:rule [ a sh:SPARQLRule ; sh:order 2 ; sh:construct """
            CONSTRUCT { $this <http://example.com/seen> true } WHERE { }""" ] .
    ex:SX a sh:NodeShape ; sh:targetSubjectsOf ex:p ;
        sh:rule [ a sh:SPARQLRule ; sh:order 1 ; sh:construct """
            CONSTRUCT { $this a <http://example.com/B> . $this ?flag true }
            WHERE { BIND(IF(EXISTS { $this <http://example.com/p> 1 },
                <http://example.com/yes>, <http://example.com/no>) AS ?flag) }""" ] ."#,
    );
    let data = graph("ex:B rdfs:subClassOf ex:C . ex:x ex:p 1 . ex:y ex:p 2 . ex:a a ex:A .");
    let shapes = Shapes::compile(&shapes).unwrap();
    let rule = |order: &str| {
        shapes
            .rules
            .iter()
            .position(|r| r.order.to_string() == order)
            .unwrap()
    };
    let writes = crate::rules::rule_writes(&shapes.rules[rule("1")]).unwrap();
    assert_eq!(writes.len(), 3, "{writes:?}");
    let runs = std::cell::RefCell::new(Vec::new());
    let observe = |r: &crate::RuleRun<'_>| {
        if r.done {
            runs.borrow_mut()
                .push((r.group, r.recursive, r.pass, r.rule.to_string(), r.new));
        }
    };
    let inferred = Inference::new(&shapes, &data)
        .with_observer(&observe)
        .run()
        .unwrap();
    // 2 types, 2 flags, 2 seen (the new ex:B instances), 1 mark.
    assert_eq!(inferred.len(), 7, "{inferred}");
    let runs = runs.into_inner();
    // Three groups, none recursive, each rule once, the ex:C rule after the typing rule.
    assert_eq!(runs.len(), 3, "{runs:?}");
    assert!(runs.iter().all(|r| !r.1 && r.2 == 1), "{runs:?}");
    let name = |o: &str| shapes.rules[rule(o)].node.to_string();
    let at = |o: &str| runs.iter().position(|r| r.3 == name(o)).unwrap();
    assert!(at("1") < at("2"), "{runs:?}");
}

#[test]
fn copies_of_matched_triples_are_not_writes() {
    // The rule copies whatever ?o says about itself: that triple is matched, so it is already in
    // the graph. Only ex:has is a write. A copy inside OPTIONAL is matched too when one of its
    // variables is bound only there (#891); one inside UNION still counts as any.
    let shapes = graph(
        r#"
    ex:S a sh:NodeShape ; sh:targetSubjectsOf ex:has ;
        sh:rule [ a sh:SPARQLRule ; sh:order 1 ; sh:construct """
            CONSTRUCT { $this <http://example.com/has2> ?o . ?o ?p ?v }
            WHERE { $this <http://example.com/has> ?o . ?o ?p ?v }""" ] ;
        sh:rule [ a sh:SPARQLRule ; sh:order 2 ; sh:construct """
            CONSTRUCT { ?o ?p ?v }
            WHERE { $this <http://example.com/has> ?o OPTIONAL { ?o ?p ?v } }""" ] ;
        sh:rule [ a sh:SPARQLRule ; sh:order 3 ; sh:construct """
            CONSTRUCT { ?o ?p ?v }
            WHERE { { $this <http://example.com/has> ?o } UNION { ?o ?p ?v } }""" ] ;
        sh:rule [ a sh:SPARQLRule ; sh:order 4 ; sh:construct """
            CONSTRUCT { _:b ?p ?v }
            WHERE { $this ?p ?v }""" ] ;
        sh:rule [ a sh:SPARQLRule ; sh:order 5 ; sh:construct """
            CONSTRUCT { ?o <http://example.com/r> ?v }
            WHERE { $this <http://example.com/has> ?o . ?o <http://example.com/q> ?v
                OPTIONAL { ?o <http://example.com/r> ?v } }""" ] ."#,
    );
    let shapes = Shapes::compile(&shapes).unwrap();
    let writes = |order: &str| {
        let r = shapes
            .rules
            .iter()
            .find(|r| r.order.to_string() == order)
            .unwrap();
        crate::rules::rule_writes(r)
    };
    let one = writes("1").unwrap();
    assert_eq!(
        one.into_iter().collect::<Vec<_>>(),
        ["http://example.com/has2"]
    );
    // ?p and ?v are bound only in the OPTIONAL: the copy is made only when it matches.
    assert_eq!(writes("2").unwrap().len(), 0);
    assert!(writes("3").is_none());
    assert!(writes("4").is_none());
    // ?o and ?v are bound outside: with no ex:r, the OPTIONAL keeps the row and the rule writes
    // a new ex:r.
    let five = writes("5").unwrap();
    assert_eq!(
        five.into_iter().collect::<Vec<_>>(),
        ["http://example.com/r"]
    );
}

/// Semi-naive later passes (#110) infer exactly what running every rule on all the data does,
/// for rules that join, OPTIONAL, MINUS, UNION, filter, bind, use blank nodes, sub-selects
/// and conditions, and feed each other.
#[test]
fn semi_naive_passes_infer_the_same_triples() {
    let shapes = graph(
        r#"
    ex:Node a sh:NodeShape ; sh:targetSubjectsOf ex:link ;
        sh:rule [ a sh:SPARQLRule ; sh:construct """
            CONSTRUCT { $this <http://example.com/reach> ?y }
            WHERE { $this <http://example.com/link> ?y }""" ] ;
        # Transitive closure, with a blank node in the pattern.
        sh:rule [ a sh:SPARQLRule ; sh:construct """
            CONSTRUCT { $this <http://example.com/reach> ?z }
            WHERE { $this <http://example.com/reach> _:m . _:m <http://example.com/reach> ?z }""" ] ;
        # OPTIONAL with a negated BOUND: nodes that reach something without a label.
        sh:rule [ a sh:SPARQLRule ; sh:construct """
            CONSTRUCT { $this <http://example.com/unlabelledReach> ?z }
            WHERE { $this <http://example.com/reach> ?z
                    OPTIONAL { ?z <http://example.com/label> ?l } FILTER(!BOUND(?l)) }""" ] ;
        # OPTIONAL feeding a value.
        sh:rule [ a sh:SPARQLRule ; sh:construct """
            CONSTRUCT { $this <http://example.com/seesLabel> ?l }
            WHERE { $this <http://example.com/reach> ?z OPTIONAL { ?z <http://example.com/label> ?l } }""" ] ;
        # Labels propagate back along reach, so OPTIONAL sides get new matches.
        sh:rule [ a sh:SPARQLRule ; sh:construct """
            CONSTRUCT { $this <http://example.com/label> ?l }
            WHERE { $this <http://example.com/reach> ?z . ?z <http://example.com/seed> ?l
                    MINUS { $this <http://example.com/blocked> true } }""" ] ;
        # UNION, BIND and a sub-select.
        sh:rule [ a sh:SPARQLRule ; sh:construct """
            CONSTRUCT { $this <http://example.com/tag> ?t }
            WHERE { { $this <http://example.com/label> ?l BIND(CONCAT("L-", STR(?l)) AS ?t) }
                    UNION
                    { SELECT $this ?t WHERE { $this <http://example.com/reach> ?z . ?z a <http://example.com/End> BIND("end" AS ?t) } } }""" ] ;
        # A type the rules make, which feeds a rule with a condition.
        sh:rule [ a sh:SPARQLRule ; sh:construct """
            CONSTRUCT { $this a <http://example.com/Tagged> }
            WHERE { $this <http://example.com/tag> ?t }""" ] ;
        sh:rule [ a sh:SPARQLRule ; sh:condition ex:IsTagged ; sh:construct """
            CONSTRUCT { $this <http://example.com/taggedReach> ?z }
            WHERE { $this <http://example.com/reach> ?z . ?z a <http://example.com/Tagged> }""" ] ;
        # Back into reach: all of the rules above feed each other.
        sh:rule [ a sh:SPARQLRule ; sh:construct """
            CONSTRUCT { $this <http://example.com/reach> ?w }
            WHERE { $this <http://example.com/taggedReach> ?z . ?z <http://example.com/label> ?l .
                    ?w <http://example.com/seed> ?l }""" ] .
    ex:IsTagged a sh:NodeShape ; sh:class ex:Tagged ."#,
    );
    let mut data = String::new();
    // Two chains joined into a cycle, with seeds, ends and a blocked node.
    for i in 0..12 {
        data += &format!("ex:n{i} ex:link ex:n{} . ", (i + 1) % 12);
    }
    for i in 0..6 {
        data += &format!("ex:m{i} ex:link ex:m{} . ", i + 1);
    }
    data += "ex:m6 ex:link ex:n3 . ex:n7 ex:seed \"seven\" . ex:m6 a ex:End . ex:m2 ex:blocked true . ex:n5 ex:label \"five\" . ex:x ex:seed \"seven\" .";
    let shapes = Shapes::compile(&shapes).unwrap();
    let data = graph(&data);
    let sorted = |g: crate::Inferred| {
        let mut v: Vec<String> = g.iter().map(|t| t.to_string()).collect();
        v.sort();
        v
    };
    let semi = sorted(Inference::new(&shapes, &data).run().unwrap());
    let naive = sorted(
        Inference::new(&shapes, &data)
            .with_semi_naive(false)
            .run()
            .unwrap(),
    );
    assert_eq!(semi, naive);
    assert!(semi.len() > 400, "{}", semi.len());
    // With provenance: the same triples, each with its rule, focus node and solution.
    let (traced, origins) = Inference::new(&shapes, &data)
        .with_provenance(true)
        .run_traced()
        .unwrap();
    assert_eq!(origins.len(), traced.len());
    for (t, o) in traced.iter().zip(&origins) {
        let focus = o.focus.as_ref().expect("a focus node");
        let solution = o.solution.as_ref().expect("a SPARQL rule's solution");
        assert_eq!(solution.get("this"), Some(focus));
        assert!(o.pass >= 1 && o.group >= 1);
        assert!(!t.to_string().is_empty());
    }
    assert_eq!(sorted(traced), semi);
}

/// Provenance of a triple rule: its focus node, no solution; and with `with_existing`, the
/// constructed triples already in the data have one too.
#[test]
fn provenance_of_triple_rules() {
    let shapes = graph(
        r#"
    ex:S a sh:NodeShape ; sh:targetClass ex:A ;
        sh:rule [ a sh:TripleRule ; sh:subject sh:this ; sh:predicate ex:p ; sh:object ex:o ] ."#,
    );
    let shapes = Shapes::compile(&shapes).unwrap();
    let data = graph("ex:a a ex:A . ex:b a ex:A . ex:b ex:p ex:o .");
    let (inferred, origins) = Inference::new(&shapes, &data)
        .with_provenance(true)
        .with_existing(true)
        .run_traced()
        .unwrap();
    let pairs: Vec<(String, String)> = inferred
        .iter()
        .zip(&origins)
        .map(|(t, o)| (t.subject.to_string(), o.focus.as_ref().unwrap().to_string()))
        .collect();
    assert_eq!(pairs, [(iri("a"), iri("a")), (iri("b"), iri("b")),]);
    assert!(
        origins
            .iter()
            .all(|o| o.solution.is_none() && o.shape.to_string() == iri("S"))
    );
    // Without provenance, no origins.
    assert!(
        Inference::new(&shapes, &data)
            .run_traced()
            .unwrap()
            .1
            .is_empty()
    );
}

/// A pattern with a variable predicate matches every delta triple: past the limit of a
/// `VALUES` block its variant reads the delta graph (the rail KG `temporal_validity` rule), and
/// the passes still infer what running every rule on all the data does.
#[test]
fn semi_naive_passes_read_large_deltas_as_a_graph() {
    let shapes = graph(
        r#"
    ex:Thing a sh:NodeShape ; sh:targetSubjectsOf ex:begins ;
        # Copies every triple of the instant to a minted interval.
        sh:rule [ a sh:SPARQLRule ; sh:construct """
            CONSTRUCT { $this <http://example.com/validity> ?interval . ?interval <http://example.com/start> ?b . ?b ?p ?v }
            WHERE { $this <http://example.com/begins> ?b . ?b ?p ?v
                    OPTIONAL { $this <http://example.com/ends> ?e . ?e ?q ?w }
                    BIND(IRI(CONCAT(STR($this), "/interval")) AS ?interval) }""" ] ;
        # Feeds the first rule: new triples on instants after its first run.
        sh:rule [ a sh:SPARQLRule ; sh:construct """
            CONSTRUCT { ?b <http://example.com/seen> ?l }
            WHERE { $this <http://example.com/begins> ?b . $this <http://example.com/label> ?l }""" ] .
    "#,
    );
    let mut data = String::new();
    for i in 0..40 {
        data += &format!("ex:t{i} ex:begins ex:b{i} ; ex:label \"t{i}\" . ex:b{i} ex:date {i} . ");
        if i % 3 == 0 {
            data += &format!("ex:t{i} ex:ends ex:e{i} . ex:e{i} ex:date {} . ", i + 1);
        }
    }
    let shapes = Shapes::compile(&shapes).unwrap();
    let data = graph(&data);
    let sorted = |g: crate::Inferred| {
        let mut v: Vec<String> = g.iter().map(|t| t.to_string()).collect();
        v.sort();
        v
    };
    let semi = sorted(Inference::new(&shapes, &data).run().unwrap());
    let naive = sorted(
        Inference::new(&shapes, &data)
            .with_semi_naive(false)
            .run()
            .unwrap(),
    );
    assert_eq!(semi, naive);
    assert!(
        semi.iter()
            .any(|t| t.contains("/interval> <http://example.com/start>"))
    );
    assert!(semi.iter().any(|t| t.contains("seen")), "{semi:?}");
}

#[test]
fn rules_on_one_shape_see_the_focus_nodes_earlier_rules_change() {
    // Each shape's first rule adds and removes focus nodes of its own shape; the second
    // rule, which runs after it, must run on the new ones.
    let shapes = graph(
        r#"
    ex:Grow a sh:NodeShape ;
        sh:target [ a sh:SPARQLTarget ; sh:select """
            SELECT $this WHERE { $this <http://example.com/mark> ?m }""" ] ;
        sh:rule [ a sh:SPARQLRule ; sh:order 0 ; sh:construct """
            CONSTRUCT { ?o <http://example.com/mark> 1 }
            WHERE { $this <http://example.com/next> ?o }""" ] ;
        sh:rule [ a sh:SPARQLRule ; sh:order 1 ; sh:construct """
            CONSTRUCT { $this <http://example.com/grown> 1 } WHERE { }""" ] .
    ex:Shrink a sh:NodeShape ;
        sh:target [ a sh:SPARQLTarget ; sh:select """
            SELECT $this WHERE { $this <http://example.com/p> ?x
                FILTER NOT EXISTS { $this <http://example.com/stop> ?s } }""" ] ;
        sh:rule [ a sh:SPARQLRule ; sh:order 2 ; sh:construct """
            CONSTRUCT { ?o <http://example.com/stop> 1 }
            WHERE { $this <http://example.com/after> ?o }""" ] ;
        sh:rule [ a sh:SPARQLRule ; sh:order 3 ; sh:construct """
            CONSTRUCT { $this <http://example.com/kept> 1 } WHERE { }""" ] ."#,
    );
    let data = graph(
        r#"ex:a ex:mark 1 ; ex:next ex:b . ex:b ex:next ex:c .
        ex:x ex:p 1 ; ex:after ex:y . ex:y ex:p 1 ."#,
    );
    let shapes = Shapes::compile(&shapes).unwrap();
    for semi_naive in [true, false] {
        let inferred = Inference::new(&shapes, &data)
            .with_semi_naive(semi_naive)
            .run()
            .unwrap();
        let mut got: Vec<String> = inferred
            .iter()
            .filter(|t| {
                ["grown", "kept"]
                    .iter()
                    .any(|p| t.predicate.as_str().ends_with(p))
            })
            .map(|t| t.subject.to_string())
            .collect();
        got.sort();
        assert_eq!(
            got,
            [iri("a"), iri("b"), iri("c"), iri("x")],
            "semi-naive {semi_naive}: {inferred}"
        );
    }
}

/// The `sh:resultMessage`s of a shapes graph's results over `data`, sorted.
fn messages(shapes: &str, data: &str) -> Vec<String> {
    let shapes = Shapes::compile(&graph(shapes)).unwrap();
    let data = graph(data);
    let v = Validator::new(&shapes, &data);
    let mut out = Vec::new();
    v.report(&mut |r| out.extend(r.messages.iter().map(|m| m.to_string())));
    assert!(v.take_error().is_none());
    out.sort();
    out
}

/// SHACL 5.3.2 and 6.4 (#90): the `{$var}` and `{?var}` of a message are filled in from the
/// solution that made the result, and from a component's parameters; a variable without a
/// binding is left as written, and the message keeps its language tag.
#[test]
fn messages_are_filled_in_from_the_solution() {
    let shapes = r#"
    ex:NoSelfLink a sh:NodeShape ; sh:targetSubjectsOf ex:prev ;
        sh:sparql [ sh:message "{$this} links to itself" , "{?value} ({?label}) is {?nothing}"@en ;
            sh:select """
            SELECT $this ?value ?label WHERE {
                $this <http://example.com/prev> ?value . FILTER(?value = $this)
                OPTIONAL { $this <http://example.com/label> ?label } }""" ] .
    ex:AboveBase a sh:ConstraintComponent ;
        sh:parameter [ sh:path ex:min ] ;
        sh:validator [ a sh:SPARQLAskValidator ; sh:message "{$value} is not {$min} above the base of {$this}" ;
            sh:ask """ASK { $this <http://example.com/base> ?b . FILTER ($value - ?b >= $min) }""" ] .
    ex:P a sh:NodeShape ; sh:targetSubjectsOf ex:base ;
        sh:property [ sh:path ex:v ; ex:min 2 ] .
    "#;
    let data = r#"
    ex:bad ex:prev ex:bad ; ex:label "Bad" . ex:good ex:prev ex:bad .
    ex:b ex:base 1 ; ex:v 3, 2 .
    "#;
    assert_eq!(
        messages(shapes, data),
        [
            r#""2 is not 2 above the base of http://example.com/b""#,
            r#""http://example.com/bad (Bad) is {?nothing}"@en"#,
            r#""http://example.com/bad links to itself""#,
        ]
    );
}

/// Only the variables a message names are kept for it, and none for plain messages.
#[test]
fn messages_name_the_variables_they_need() {
    use crate::report::template_names;
    use oxigraph::model::{Literal, Term};
    let m = |s: &str| Term::from(Literal::new_simple_literal(s));
    assert!(template_names(&[m("self link"), m("{ not a variable }")]).is_empty());
    assert_eq!(
        template_names(&[m("{$this} has {?value} and {$this} again"), m("{?label")]),
        ["this", "value"]
    );
}

/// #91: solutions that make the same result (focus node, value, path and message) give it
/// once, as pySHACL does; solutions whose message differs give a result each.
#[test]
fn repeated_solutions_give_one_result() {
    let data = r#"
    ex:r1 a ex:Road ; ex:passesUnder ex:b1 , ex:b2 .
    ex:r2 a ex:Road ; ex:passesUnder ex:b1 .
    "#;
    let count = |shapes: &str| {
        let shapes = Shapes::compile(&graph(shapes)).unwrap();
        let data = graph(data);
        let v = Validator::new(&shapes, &data);
        let mut out = Vec::new();
        v.report(&mut |r| {
            out.push((
                r.focus_node.to_string(),
                r.messages.iter().map(|m| m.to_string()).collect::<Vec<_>>(),
            ))
        });
        assert!(v.take_error().is_none());
        out.sort();
        out
    };
    // One row per bridge, the same result for each: once per road.
    let same = count(
        r#"ex:S a sh:NodeShape ; sh:targetClass ex:Road ; sh:sparql [ sh:select """
        SELECT $this WHERE { $this <http://example.com/passesUnder> ?b }""" ] ."#,
    );
    assert_eq!(
        same.iter().map(|(f, _)| f.as_str()).collect::<Vec<_>>(),
        [iri("r1"), iri("r2")]
    );
    // A message that names the bridge: two results for r1.
    let named = count(
        r#"ex:S a sh:NodeShape ; sh:targetClass ex:Road ; sh:sparql [ sh:message "under {?b}" ;
        sh:select """SELECT $this ?b WHERE { $this <http://example.com/passesUnder> ?b }""" ] ."#,
    );
    assert_eq!(named.len(), 3, "{named:?}");
}

/// #91: with a focus graph, targets find their focus nodes there only, while the rest of the
/// data graph is still read (here by `sh:class` and a SPARQL constraint).
#[test]
fn a_focus_graph_gives_the_focus_nodes() {
    let data = graph(
        r#"
    ex:t1 a ex:Track ; ex:kind ex:Main .
    ex:t2 a ex:Track ; ex:kind ex:Unknown .
    "#,
    );
    let vocab = graph(
        r#"
    ex:Main a ex:Kind .
    ex:Track a ex:Class .
    ex:Unknown a ex:Track ; ex:kind ex:Nope .
    "#,
    );
    let shapes = Shapes::compile(&graph(
        r#"
    ex:S a sh:NodeShape ; sh:targetClass ex:Track ;
        sh:property [ sh:path ex:kind ; sh:class ex:Kind ] .
    ex:T a sh:NodeShape ; sh:targetSubjectsOf ex:kind ;
        sh:sparql [ sh:select """SELECT $this WHERE {
            $this <http://example.com/kind> ?k FILTER NOT EXISTS { ?k a <http://example.com/Kind> }
        }""" ] .
    "#,
    ))
    .unwrap();
    let all = Union(vec![&data, &vocab]);
    let failing = |v: &Validator| {
        let mut out: Vec<String> = v
            .validate()
            .into_iter()
            .map(|(f, _)| f.to_string())
            .collect();
        assert!(v.take_error().is_none());
        out.sort();
        out
    };
    // Over everything, ex:Unknown (a track in the vocabulary) is validated too: it fails both
    // shapes, as t2 does (their kinds are not Kinds).
    let v = Validator::new(&shapes, &all);
    assert_eq!(
        failing(&v),
        [iri("Unknown"), iri("Unknown"), iri("t2"), iri("t2")]
    );
    // With the data as focus graph, only t1 and t2 are focus nodes, and t1 still finds the
    // class of its kind in the vocabulary.
    let v = Validator::new(&shapes, &all).with_focus_graph(&data);
    assert_eq!(failing(&v), [iri("t2"), iri("t2")]);
    for &s in shapes.targeted() {
        let mut foci: Vec<String> = v.focus_nodes(s).iter().map(|f| f.to_string()).collect();
        foci.sort();
        foci.dedup();
        assert!(
            foci.iter().all(|f| *f == iri("t1") || *f == iri("t2")),
            "{foci:?}"
        );
    }
}
