//! One test per constraint component (and the targets and paths), on small graphs.

use crate::{Shapes, Validator, compat};
use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::{Graph, NamedNode, Term};

const PREFIXES: &str = "@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.com/> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix compat: <http://w3id.org/tripletrail/compat#> .
";

pub(crate) fn graph(ttl: &str) -> Graph {
    let mut g = Graph::new();
    for q in
        RdfParser::from_format(RdfFormat::Turtle).for_slice(format!("{PREFIXES}{ttl}").as_bytes())
    {
        g.insert(&q.unwrap().into());
    }
    g
}

pub(crate) fn ex(local: &str) -> Term {
    NamedNode::new_unchecked(format!("http://example.com/{local}")).into()
}

/// Whether each of `focus` conforms to `ex:S` (ignoring targets), with `shape` the body of
/// `ex:S` and `data` the data graph.
fn conforms(shape: &str, data: &str, focus: &[&str]) -> Vec<bool> {
    let shapes_graph = graph(&format!("ex:S {shape} ."));
    let data = graph(data);
    let shapes = Shapes::compile_with(&shapes_graph, &[ex("S")]).unwrap();
    let id = shapes.shape(&ex("S")).unwrap();
    let v = Validator::new(&shapes, &data);
    focus.iter().map(|f| v.conforms_to(&ex(f), id)).collect()
}

const DATA: &str = r#"
ex:a ex:p 1, 2 ; ex:q 2 ; ex:name "alpha"@en ; ex:code "AB-12" ; a ex:Car ; ex:next ex:b .
ex:b ex:p 5 ; ex:q 5 ; ex:name "beta"@en, "bêta"@fr ; ex:code "x" ; a ex:Truck ; ex:next ex:c .
ex:c ex:p "five" ; ex:q 1 ; ex:name "gamma"@en, "gamma2"@en ; ex:link _:x ; ex:next ex:a .
ex:Truck rdfs:subClassOf ex:Vehicle . ex:Car rdfs:subClassOf ex:Vehicle .
ex:d ex:when "2024-01-01T10:00:00Z"^^xsd:dateTime ; ex:until "2024-01-02T10:00:00Z"^^xsd:dateTime .
"#;

#[test]
fn counts() {
    let s = "sh:path ex:p ; sh:minCount 2";
    assert_eq!(conforms(s, DATA, &["a", "b", "z"]), [true, false, false]);
    let s = "sh:path ex:p ; sh:maxCount 1";
    assert_eq!(conforms(s, DATA, &["a", "b", "z"]), [false, true, true]);
}

#[test]
fn value_type() {
    let s = "sh:path ex:next ; sh:class ex:Vehicle";
    // a -> b (Truck), b -> c (no type), c -> a (Car).
    assert_eq!(conforms(s, DATA, &["a", "b", "c"]), [true, false, true]);
    let s = "sh:path ex:p ; sh:datatype xsd:integer";
    assert_eq!(conforms(s, DATA, &["a", "c"]), [true, false]);
    let s = "sh:path ex:link ; sh:nodeKind sh:BlankNode";
    assert_eq!(conforms(s, DATA, &["c"]), [true]);
    let s = "sh:path ex:next ; sh:nodeKind sh:Literal";
    assert_eq!(conforms(s, DATA, &["a", "z"]), [false, true]);
    let s = "sh:nodeKind sh:IRI";
    assert_eq!(conforms(s, DATA, &["a"]), [true]);
}

#[test]
fn ranges() {
    let s = "sh:path ex:p ; sh:minInclusive 2";
    assert_eq!(conforms(s, DATA, &["a", "b", "c"]), [false, true, false]);
    let s = "sh:path ex:p ; sh:minExclusive 1";
    assert_eq!(conforms(s, DATA, &["a", "b"]), [false, true]);
    let s = "sh:path ex:p ; sh:maxInclusive 2.0";
    assert_eq!(conforms(s, DATA, &["a", "b"]), [true, false]);
    let s = "sh:path ex:p ; sh:maxExclusive 2";
    assert_eq!(conforms(s, DATA, &["a"]), [false]);
    let s = r#"sh:path ex:when ; sh:maxExclusive "2024-01-01T12:00:00Z"^^xsd:dateTime"#;
    assert_eq!(conforms(s, DATA, &["d"]), [true]);
}

#[test]
fn strings() {
    let s = "sh:path ex:code ; sh:minLength 2";
    assert_eq!(conforms(s, DATA, &["a", "b"]), [true, false]);
    let s = "sh:path ex:code ; sh:maxLength 3";
    assert_eq!(conforms(s, DATA, &["a", "b"]), [false, true]);
    let s = r#"sh:path ex:code ; sh:pattern "^[a-z]{2}-\\d+$" ; sh:flags "i""#;
    assert_eq!(conforms(s, DATA, &["a", "b"]), [true, false]);
    let s = r#"sh:path ex:name ; sh:languageIn ( "en" )"#;
    assert_eq!(conforms(s, DATA, &["a", "b"]), [true, false]);
    let s = "sh:path ex:name ; sh:uniqueLang true";
    assert_eq!(conforms(s, DATA, &["b", "c"]), [true, false]);
    // A blank node has no string.
    let s = "sh:path ex:link ; sh:minLength 0";
    assert_eq!(conforms(s, DATA, &["c"]), [false]);
}

#[test]
fn property_pairs() {
    let s = "sh:path ex:p ; sh:equals ex:q";
    assert_eq!(conforms(s, DATA, &["a", "b"]), [false, true]);
    let s = "sh:path ex:p ; sh:disjoint ex:q";
    assert_eq!(conforms(s, DATA, &["a", "b"]), [false, false]);
    let s = "sh:path ex:q ; sh:disjoint ex:code";
    assert_eq!(conforms(s, DATA, &["a"]), [true]);
    let s = "sh:path ex:p ; sh:lessThan ex:q";
    assert_eq!(conforms(s, DATA, &["a", "b"]), [false, false]);
    let s = "sh:path ex:p ; sh:lessThanOrEquals ex:q";
    assert_eq!(conforms(s, DATA, &["a", "b", "c"]), [true, true, false]);
    let s = "sh:path ex:when ; sh:lessThan ex:until";
    assert_eq!(conforms(s, DATA, &["d"]), [true]);
}

#[test]
fn logic() {
    let s = "sh:not [ sh:path ex:p ; sh:minCount 2 ]";
    assert_eq!(conforms(s, DATA, &["a", "b"]), [false, true]);
    let s = "sh:and ( [ sh:path ex:p ; sh:minCount 1 ] [ sh:path ex:q ; sh:hasValue 5 ] )";
    assert_eq!(conforms(s, DATA, &["a", "b"]), [false, true]);
    let s = "sh:or ( [ sh:path ex:q ; sh:hasValue 1 ] [ sh:path ex:q ; sh:hasValue 5 ] )";
    assert_eq!(conforms(s, DATA, &["a", "b", "c"]), [false, true, true]);
    let s = "sh:xone ( [ sh:class ex:Vehicle ] [ sh:path ex:q ; sh:hasValue 5 ] )";
    // a: Vehicle only; b: both; c: neither.
    assert_eq!(conforms(s, DATA, &["a", "b", "c"]), [true, false, false]);
}

#[test]
fn shape_based() {
    let shapes = "
        ex:Small sh:path ex:p ; sh:maxInclusive 2 .
        ex:S2 sh:property [ sh:path ex:next ; sh:node ex:HasQ5 ] .
        ex:HasQ5 sh:property [ sh:path ex:q ; sh:hasValue 5 ] .";
    let g = graph(shapes);
    let data = graph(DATA);
    let s = Shapes::compile_with(&g, &[ex("S2"), ex("Small")]).unwrap();
    let v = Validator::new(&s, &data);
    let s2 = s.shape(&ex("S2")).unwrap();
    // a -> b (q 5), b -> c (q 1).
    assert!(v.conforms_to(&ex("a"), s2));
    assert!(!v.conforms_to(&ex("b"), s2));
    let small = s.shape(&ex("Small")).unwrap();
    assert!(v.conforms_to(&ex("a"), small));
    assert!(!v.conforms_to(&ex("b"), small));

    // Qualified: at least one value is >= 2, at most one.
    let s = "sh:path ex:p ; sh:qualifiedValueShape [ sh:minInclusive 2 ] ; sh:qualifiedMinCount 1 ; sh:qualifiedMaxCount 1";
    assert_eq!(conforms(s, DATA, &["a", "b", "c"]), [true, true, false]);
    // Disjoint siblings: a value matching both qualified shapes counts for neither.
    let s = "
        sh:property [ sh:path ex:p ; sh:qualifiedValueShape [ sh:minInclusive 1 ] ;
                      sh:qualifiedMinCount 2 ; sh:qualifiedValueShapesDisjoint true ] ;
        sh:property [ sh:path ex:p ; sh:qualifiedValueShape [ sh:minInclusive 2 ] ;
                      sh:qualifiedMinCount 0 ; sh:qualifiedValueShapesDisjoint true ]";
    assert_eq!(conforms(s, DATA, &["a"]), [false]);
}

#[test]
fn closed() {
    let s = "sh:closed true ; sh:ignoredProperties ( rdf:type ex:name ex:code ex:next ) ;
             sh:property [ sh:path ex:p ] , [ sh:path ex:q ]";
    assert_eq!(conforms(s, DATA, &["a", "c"]), [true, false]);
}

#[test]
fn values() {
    let s = "sh:path ex:q ; sh:hasValue 2";
    assert_eq!(conforms(s, DATA, &["a", "b"]), [true, false]);
    let s = "sh:path ex:p ; sh:in ( 1 2 3 )";
    assert_eq!(conforms(s, DATA, &["a", "b", "z"]), [true, false, true]);
}

#[test]
fn paths() {
    // a -next-> b -next-> c -next-> a.
    let s = "sh:path ( ex:next ex:next ) ; sh:hasValue ex:c";
    assert_eq!(conforms(s, DATA, &["a", "b"]), [true, false]);
    let s = "sh:path [ sh:inversePath ex:next ] ; sh:hasValue ex:c";
    assert_eq!(conforms(s, DATA, &["a", "b"]), [true, false]);
    let s = "sh:path [ sh:alternativePath ( ex:p ex:q ) ] ; sh:minCount 3";
    assert_eq!(conforms(s, DATA, &["a", "b"]), [false, false]);
    let s = "sh:path [ sh:alternativePath ( ex:p ex:q ex:code ) ] ; sh:minCount 3";
    assert_eq!(conforms(s, DATA, &["a"]), [true]);
    let s = "sh:path [ sh:zeroOrMorePath ex:next ] ; sh:minCount 3 ; sh:maxCount 3";
    assert_eq!(conforms(s, DATA, &["a"]), [true]);
    let s = "sh:path [ sh:oneOrMorePath ex:next ] ; sh:hasValue ex:a";
    assert_eq!(conforms(s, DATA, &["a"]), [true]);
    let s = "sh:path [ sh:zeroOrOnePath ex:next ] ; sh:in ( ex:a ex:b )";
    assert_eq!(conforms(s, DATA, &["a", "b"]), [true, false]);
    let s = "sh:path ( [ sh:inversePath ex:next ] [ sh:inversePath ex:next ] ) ; sh:hasValue ex:b";
    assert_eq!(conforms(s, DATA, &["a"]), [true]);
}

#[test]
fn targets_and_full_validation() {
    let shapes = graph(
        "ex:V a sh:NodeShape ; sh:targetClass ex:Vehicle ; sh:property [ sh:path ex:q ; sh:minInclusive 2 ] .
         ex:N a sh:NodeShape ; sh:targetNode ex:c ; sh:property [ sh:path ex:q ; sh:minInclusive 2 ] .
         ex:Off a sh:NodeShape ; sh:targetSubjectsOf ex:p ; sh:deactivated true ; sh:property [ sh:path ex:q ; sh:maxCount 0 ] .
         ex:Obj a sh:NodeShape ; sh:targetObjectsOf ex:next ; sh:nodeKind sh:IRI .
         ex:Car a rdfs:Class, sh:NodeShape ; sh:property [ sh:path ex:q ; sh:hasValue 1 ] .",
    );
    let data = graph(DATA);
    let s = Shapes::compile(&shapes).unwrap();
    let v = Validator::new(&s, &data);
    // a is a Car (and a Vehicle) with q 2: the implicit class target ex:Car fails it.
    assert!(!v.conforms(&ex("a")));
    assert!(v.conforms(&ex("b")));
    assert!(!v.conforms(&ex("c")));
    assert!(v.conforms(&ex("z")));
    let mut bad: Vec<String> = v
        .validate()
        .into_iter()
        .map(|(f, id)| format!("{f} {}", s.get(id).node))
        .collect();
    bad.sort();
    assert_eq!(
        bad,
        [
            "<http://example.com/a> <http://example.com/Car>",
            "<http://example.com/c> <http://example.com/N>"
        ]
    );
    assert!(
        s.reads()
            .unwrap()
            .contains("http://www.w3.org/1999/02/22-rdf-syntax-ns#type")
    );
}

#[test]
fn errors() {
    let g = graph("ex:S sh:path ex:p ; sh:pattern \"(\" .");
    assert!(Shapes::compile_with(&g, &[ex("S")]).is_err());
    let g = graph("ex:S sh:sparql [ ] ; sh:targetNode ex:a .");
    assert!(Shapes::compile(&g).is_err());
    let g = graph("ex:T sh:targetNode ex:a .");
    assert!(Shapes::compile_with(&g, &[ex("Missing")]).is_err());
}

const RULES: &str = r#"
ex:weight a compat:Rule ; compat:vehiclePath ex:weight ; compat:edgePath ex:maxWeight ;
    compat:constraint sh:minInclusive .
ex:height a compat:Rule ; compat:vehiclePath ex:height ;
    compat:edgePath ( ex:crosses ex:clearance ) ; compat:constraint sh:minExclusive .
ex:fuel a compat:Rule ; compat:vehiclePath ex:fuel ; compat:edgePath ex:fuelAllowed ;
    compat:constraint sh:hasValue ; compat:whenMissing compat:Fail ; compat:noneValue ex:Any .
ex:zones a compat:Rule ; compat:vehiclePath ex:zone ; compat:edgePath ex:zone ;
    compat:constraint sh:in .
ex:kg a compat:Rule ; compat:vehiclePath ex:axleT ; compat:edgePath ex:maxAxleKg ;
    compat:constraint sh:minInclusive ; compat:factor 1000 .
"#;

const VEHICLES: &str = r#"
ex:truck ex:weight 30 ; ex:height 4.2 ; ex:fuel ex:Diesel ; ex:zone ex:Z1, ex:Z2 ; ex:axleT 10 .
ex:van ex:weight 3.5 ; ex:fuel ex:Diesel, ex:Any .
ex:bus ex:fuel ex:Electric, ex:Hydrogen .
"#;

const ROADS: &str = r#"
ex:r1 ex:maxWeight 40 ; ex:fuelAllowed ex:Diesel ; ex:zone ex:Z1 ; ex:maxAxleKg 11500 .
ex:r2 ex:maxWeight 7.5 ; ex:crosses ex:bridge ; ex:fuelAllowed ex:Diesel .
ex:r3 ex:crosses ex:low ; ex:zone ex:Z3 .
ex:r4 ex:fuelAllowed ex:Hydrogen ; ex:maxAxleKg 9000 .
ex:bridge ex:clearance 5.0 . ex:low ex:clearance 4.2 .
"#;

fn compat_conforms(vehicle: &str, roads: &[&str]) -> Vec<bool> {
    let rules = graph(RULES);
    let vehicles = graph(VEHICLES);
    let data = graph(ROADS);
    let c = compat::compile(&rules, &vehicles, &ex(vehicle), None).unwrap();
    let s = Shapes::compile_with(&c.graph, std::slice::from_ref(&c.shape)).unwrap();
    let id = s.shape(&c.shape).unwrap();
    let v = Validator::new(&s, &data);
    roads.iter().map(|r| v.conforms_to(&ex(r), id)).collect()
}

#[test]
fn compatibility_rules() {
    // Truck: 30 t (r2 7.5 fails), 4.2 m (r3's 4.2 m bridge is not above it), diesel and no
    // missing fuel (r3 fails), zones Z1 Z2 (r3 is Z3), axle 10 t = 10000 kg (r4 9000 fails).
    assert_eq!(
        compat_conforms("truck", &["r1", "r2", "r3", "r4"]),
        [true, false, false, false]
    );
    // Van: 3.5 t, no height or zone (not checked), diesel or anything where no fuel is given.
    assert_eq!(
        compat_conforms("van", &["r1", "r2", "r3", "r4"]),
        [true, true, true, false]
    );
    // Bus: only fuel, either of two (r4); a road without fuel fails.
    assert_eq!(
        compat_conforms("bus", &["r1", "r3", "r4"]),
        [false, false, true]
    );
    let rules = graph(RULES);
    let c = compat::compile(
        &rules,
        &graph(VEHICLES),
        &ex("bus"),
        Some(NamedNode::new_unchecked("http://example.com/BusShape")),
    )
    .unwrap();
    assert_eq!((c.checked, c.skipped), (1, 4));
    assert_eq!(c.shape, ex("BusShape"));
    assert!(c.reads.contains("http://example.com/clearance"));
}

#[test]
fn bad_rules() {
    let rules = graph(
        "ex:r compat:vehiclePath ex:a ; compat:edgePath ex:b ; compat:constraint sh:pattern .",
    );
    assert!(compat::compile(&rules, &graph(""), &ex("v"), None).is_err());
    let rules = graph("ex:r a compat:Rule ; compat:edgePath ex:b ; compat:constraint sh:in .");
    assert!(compat::compile(&rules, &graph(""), &ex("v"), None).is_err());
    let rules = graph(
        "ex:r compat:vehiclePath ex:a ; compat:edgePath ex:b ; compat:constraint sh:minInclusive .",
    );
    assert!(compat::compile(&rules, &graph("ex:v ex:a ex:notANumber ."), &ex("v"), None).is_err());
}

#[test]
fn validation_report() {
    use crate::ReportWriter;
    let shapes_graph = graph(
        r#"
        ex:Car a sh:NodeShape ; sh:targetClass ex:Car ;
            sh:property ex:Speed , ex:Name ;
            sh:sparql [ sh:select "SELECT $this WHERE { $this <http://example.com/broken> true }" ;
                        sh:message "broken"@en ] .
        ex:Speed sh:path ex:speed ; sh:maxInclusive 200 ; sh:minCount 1 ;
            sh:message "too fast"@en , "te snel"@nl .
        ex:Name sh:path ex:name ; sh:datatype xsd:string ; sh:severity sh:Warning .
        "#,
    );
    let data = graph(
        r#"
        ex:ok a ex:Car ; ex:speed 100 ; ex:name "ok" .
        ex:fast a ex:Car ; ex:speed 250, 300, 120 ; ex:name 7 .
        ex:none a ex:Car ; ex:broken true .
        "#,
    );
    let shapes = Shapes::compile(&shapes_graph).unwrap();
    let v = Validator::new(&shapes, &data);
    let mut results = Vec::new();
    v.report(&mut |r| {
        results.push((
            r.focus_node.to_string(),
            r.value.as_ref().map(Term::to_string),
            r.source_shape.to_string(),
            r.source_constraint_component.to_string(),
            r.severity.to_string(),
            r.messages.len(),
            r.result_path.as_ref().map(|p| p.to_sparql()),
        ))
    });
    results.sort();
    let sh = |l: &str| format!("<http://www.w3.org/ns/shacl#{l}>");
    let int = |i: i32| format!("\"{i}\"^^<http://www.w3.org/2001/XMLSchema#integer>");
    let ex = |l: &str| format!("<http://example.com/{l}>");
    let speed = Some("<http://example.com/speed>".to_owned());
    let name = Some("<http://example.com/name>".to_owned());
    assert_eq!(
        results,
        [
            (
                ex("fast"),
                Some(int(250)),
                ex("Speed"),
                sh("MaxInclusiveConstraintComponent"),
                sh("Violation"),
                2,
                speed.clone()
            ),
            (
                ex("fast"),
                Some(int(300)),
                ex("Speed"),
                sh("MaxInclusiveConstraintComponent"),
                sh("Violation"),
                2,
                speed.clone()
            ),
            (
                ex("fast"),
                Some(int(7)),
                ex("Name"),
                sh("DatatypeConstraintComponent"),
                sh("Warning"),
                0,
                name
            ),
            (
                ex("none"),
                None,
                ex("Speed"),
                sh("MinCountConstraintComponent"),
                sh("Violation"),
                2,
                speed
            ),
            (
                ex("none"),
                Some(ex("none")),
                ex("Car"),
                sh("SPARQLConstraintComponent"),
                sh("Violation"),
                1,
                None
            ),
        ]
    );
    // The report as triples: 5 results, sh:conforms false.
    let mut report = Graph::new();
    let mut writer = ReportWriter::new();
    let mut sink = |t: oxigraph::model::Triple| {
        report.insert(&t);
    };
    v.report(&mut |r| writer.result(&r, &mut sink));
    assert!(!writer.finish(&mut sink));
    let count = |p: &str| {
        report
            .triples_for_predicate(
                NamedNode::new_unchecked(format!("http://www.w3.org/ns/shacl#{p}")).as_ref(),
            )
            .count()
    };
    assert_eq!(count("result"), 5);
    assert_eq!(count("resultMessage"), 7);
    assert_eq!(count("conforms"), 1);
}

/// A flat summary of a result and its details, nested by indent: component, focus node,
/// value, source shape.
fn outline(r: &crate::ValidationResult<'_>, indent: usize, out: &mut Vec<String>) {
    let short = |t: &Term| {
        t.to_string()
            .replace(['<', '>'], "")
            .replace("http://www.w3.org/ns/shacl#", "sh:")
            .replace("http://example.com/", "ex:")
            .replace("ConstraintComponent", "")
    };
    out.push(format!(
        "{}{} {} {} {}",
        " ".repeat(indent),
        short(&r.source_constraint_component),
        short(&r.focus_node),
        r.value.as_ref().map(short).unwrap_or_default(),
        short(r.source_shape),
    ));
    // Details in a fixed order: the order of a shape's blank node property shapes is not.
    let mut details: Vec<Vec<String>> = r
        .details
        .iter()
        .map(|d| {
            let mut lines = Vec::new();
            outline(d, indent + 2, &mut lines);
            lines
        })
        .collect();
    details.sort();
    out.extend(details.into_iter().flatten());
}

#[test]
fn nested_shapes_give_details() {
    let shapes_graph = graph(
        r#"
        ex:Person a sh:NodeShape ; sh:targetClass ex:Person ;
            sh:property [ sh:path ex:address ; sh:node ex:Address ] ;
            sh:property [ sh:path ex:id ; sh:or ( ex:Iri ex:Number ) ] ;
            sh:property [ sh:path ex:friend ; sh:not ex:Address ] ;
            sh:property [ sh:path ex:phone ;
                          sh:qualifiedValueShape ex:Number ; sh:qualifiedMinCount 1 ] .
        ex:Address a sh:NodeShape ;
            sh:property [ sh:path ex:city ; sh:minCount 1 ] ;
            sh:property [ sh:path ex:geo ; sh:node ex:Point ] .
        ex:Point a sh:NodeShape ; sh:property [ sh:path ex:x ; sh:datatype xsd:double ] .
        ex:Iri a sh:NodeShape ; sh:nodeKind sh:IRI .
        ex:Number a sh:NodeShape ; sh:datatype xsd:integer .
        "#,
    );
    let data = graph(
        r#"
        ex:p a ex:Person ; ex:address ex:a ; ex:id "x" ; ex:friend ex:f ; ex:phone "1" .
        ex:a ex:geo ex:g .
        ex:g ex:x "east" .
        ex:f ex:city "Gent" .
        "#,
    );
    let shapes = Shapes::compile(&shapes_graph).unwrap();
    let v = Validator::new(&shapes, &data);
    let mut lines = Vec::new();
    v.report(&mut |r| {
        let mut out = Vec::new();
        outline(&r, 0, &mut out);
        // Blank node property shapes have generated labels.
        let blank = unlabel(&out.join("\n"));
        lines.push(blank);
    });
    lines.sort();
    assert_eq!(
        lines,
        [
            // sh:node: the address has no city, and its point fails in turn.
            "sh:Node ex:p ex:a _\n  sh:MinCount ex:a  _\n  sh:Node ex:a ex:g _\n    sh:Datatype ex:g \"east\" _",
            // sh:not: no details, the nested shape conforms.
            "sh:Not ex:p ex:f _",
            // sh:or: why "x" fails each member (details sorted by the outline).
            "sh:Or ex:p \"x\" _\n  sh:Datatype \"x\" \"x\" ex:Number\n  sh:NodeKind \"x\" \"x\" ex:Iri",
            // sh:qualifiedMinCount: why the value does not qualify.
            "sh:QualifiedMinCount ex:p  _\n  sh:Datatype \"1\" \"1\" ex:Number",
        ]
    );
}

/// `s` with every blank node label replaced by `_`.
fn unlabel(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("_:") {
        out.push_str(&rest[..i]);
        out.push('_');
        let end = rest[i + 2..]
            .find(|c: char| !c.is_ascii_alphanumeric())
            .map_or(rest.len(), |e| i + 2 + e);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

#[test]
fn details_are_bounded() {
    // A chain of sh:or with two failing members per level would give 2^depth details.
    let mut shapes = String::from("ex:S0 a sh:NodeShape ; sh:targetNode ex:n .\n");
    for i in 0..12 {
        shapes.push_str(&format!(
            "ex:S{i} sh:or ( ex:S{j} [ sh:property [ sh:path ex:p ; sh:node ex:S{j} ] ] ) .\n",
            j = i + 1
        ));
    }
    shapes.push_str("ex:S12 sh:hasValue ex:never .\n");
    let shapes_graph = graph(&shapes);
    let data = graph("ex:n ex:p ex:n .");
    let shapes = Shapes::compile(&shapes_graph).unwrap();
    let v = Validator::new(&shapes, &data);
    fn total(r: &crate::ValidationResult<'_>) -> usize {
        r.details.len() + r.details.iter().map(total).sum::<usize>()
    }
    let mut details = 0;
    let mut results = 0;
    v.report(&mut |r| {
        results += 1;
        details += total(&r);
        // Every level down to the first leaf says what failed.
        let mut first = &r;
        let mut depth = 0;
        while let Some(d) = first.details.first() {
            first = d;
            depth += 1;
        }
        assert!(depth >= 12, "{depth}");
    });
    assert_eq!(results, 1);
    let max = crate::validate::MAX_DETAILS;
    assert!((max..max + 50).contains(&details), "{details}");
}

/// The IRIs of the compiled shapes, sorted.
fn shape_iris(ttl: &str) -> Vec<String> {
    let shapes = Shapes::compile(&graph(ttl)).unwrap();
    let mut out: Vec<String> = (0..shapes.len())
        .map(|i| {
            shapes
                .get(crate::ShapeId(i as u32))
                .iri()
                .as_str()
                .to_owned()
        })
        .collect();
    out.sort();
    out
}

#[test]
fn blank_shapes_get_stable_iris() {
    let ttl = "ex:S a sh:NodeShape ; sh:targetClass ex:Car ;
        sh:property [ sh:path ex:p ; sh:minCount 1 ] ;
        sh:property [ sh:path ex:q ; sh:or ( [ sh:datatype xsd:integer ] [ sh:in ( 1 2 ) ] ) ] .";
    let iris = shape_iris(ttl);
    // ex:S, its two property shapes and the two shapes of the sh:or list.
    assert_eq!(iris.len(), 5);
    assert!(iris.contains(&"http://example.com/S".to_owned()));
    let named: Vec<&String> = iris
        .iter()
        .filter(|i| i.starts_with("http://example.com/S#"))
        .collect();
    assert_eq!(named.len(), 4, "{iris:?}");
    assert!(
        named
            .iter()
            .all(|i| i.len() == "http://example.com/S#".len() + 16)
    );
    // A new read (new blank node labels), with the triples and shapes in another order.
    let reordered = "ex:S sh:property [ sh:or ( [ sh:datatype xsd:integer ] [ sh:in ( 1 2 ) ] ) ;
            sh:path ex:q ] ;
        sh:property [ sh:minCount 1 ; sh:path ex:p ] ; sh:targetClass ex:Car ; a sh:NodeShape .";
    assert_eq!(shape_iris(reordered), iris);
    // A changed property shape gets a new IRI; the other keeps its own.
    let changed = ttl.replace("sh:minCount 1", "sh:minCount 2");
    let after = shape_iris(&changed);
    assert_eq!(after.iter().filter(|i| iris.contains(i)).count(), 4);
    // An IRI with a `#` gets `-`; a blank shape no IRI refers to gets a URN.
    let hashed = shape_iris(
        "<http://example.com/shapes#S> sh:targetNode ex:a ; sh:property [ sh:path ex:p ; sh:minCount 1 ] .",
    );
    assert!(
        hashed[1].starts_with("http://example.com/shapes#S-"),
        "{hashed:?}"
    );
    let anonymous = shape_iris("[] sh:targetNode ex:a ; sh:class ex:Car .");
    assert!(anonymous[0].starts_with("urn:x-tripletrail-shacl:shape:"));
}

/// The finding keys of validating `data` with `shapes`, sorted.
fn finding_keys(shapes: &str, data: &str) -> Vec<u128> {
    let shapes = Shapes::compile(&graph(shapes)).unwrap();
    let data = graph(data);
    let mut keys = Vec::new();
    Validator::new(&shapes, &data).report(&mut |r| keys.push(r.key()));
    keys.sort_unstable();
    keys
}

#[test]
fn finding_keys_are_stable() {
    let shapes = "ex:S sh:targetClass ex:Car ;
        sh:property [ sh:path ex:p ; sh:maxInclusive 1 ] ;
        sh:property [ sh:path ex:q ; sh:minCount 1 ] .";
    let data = "ex:a a ex:Car ; ex:p 2, 3 . ex:b a ex:Car ; ex:p 1 ; ex:q 1 .";
    let keys = finding_keys(shapes, data);
    // ex:a: two values over 1 and no ex:q; each result has its own key.
    assert_eq!(keys.len(), 3);
    assert!(keys.windows(2).all(|w| w[0] != w[1]));
    // A new read of the shapes (new blank node labels) gives the same keys.
    assert_eq!(finding_keys(shapes, data), keys);
    // Keys are stored to compare with later runs: they must not change between builds.
    // This one is 128-bit FNV-1a of `<http://example.com/a>`, ``, `<http://example.com/a>`,
    // `<http://example.com/S>`, `<…shacl#ClassConstraintComponent>` and `<…shacl#Violation>`,
    // each followed by the byte 0xff.
    assert_eq!(
        finding_keys("ex:S sh:targetNode ex:a ; sh:class ex:Car .", ""),
        [0x07793a455b8420e67a437da11447b4a7]
    );
    let with_message = shapes.replace("sh:minCount 1", "sh:minCount 1 ; sh:message \"no q\"");
    let changed = finding_keys(&with_message, data);
    // The message changes the ex:q shape's IRI (its triples changed), so only its key moves.
    assert_eq!(changed.iter().filter(|k| keys.contains(k)).count(), 2);
    // A severity is part of the key.
    let warning = shapes.replace(
        "sh:maxInclusive 1",
        "sh:maxInclusive 1 ; sh:severity sh:Warning",
    );
    assert_eq!(
        finding_keys(&warning, data)
            .iter()
            .filter(|k| keys.contains(k))
            .count(),
        1
    );
}
