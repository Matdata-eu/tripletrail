use tripletrail_rml::{FsLoader, Mapping, MemoryLoader, RmlError, run};

const PREFIXES: &str = r#"
@prefix rr: <http://www.w3.org/ns/r2rml#> .
@prefix rml: <http://semweb.mmlab.be/ns/rml#> .
@prefix ql: <http://semweb.mmlab.be/ns/ql#> .
@prefix rmln: <http://w3id.org/rml/> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
@prefix ex: <http://ex.org/> .
"#;

/// The quads of `mapping` over `files`, as sorted N-Quads lines.
fn lines(mapping: &str, files: MemoryLoader) -> Vec<String> {
    let mapping =
        Mapping::parse_turtle(&format!("{PREFIXES}{mapping}"), Some("http://ex.org/base/"))
            .unwrap();
    let mut out: Vec<String> = run(&mapping, &files)
        .unwrap()
        .iter()
        .map(|q| format!("{q} ."))
        .collect();
    out.sort();
    out
}

fn error(mapping: &str, files: MemoryLoader) -> RmlError {
    match Mapping::parse_turtle(&format!("{PREFIXES}{mapping}"), Some("http://ex.org/base/")) {
        Err(e) => e,
        Ok(m) => run(&m, &files).unwrap_err(),
    }
}

const STUDENTS: &str = r#"{"students": [
    {"id": 1, "name": "Venus Williams", "sports": ["Tennis", "Golf"], "score": 9.5, "active": true},
    {"id": 2, "name": "Serena", "sports": [], "active": false},
    {"id": 3, "sports": ["Chess"]}
]}"#;

#[test]
fn legacy_rml_over_json() {
    let out = lines(
        r#"
        <#S> rml:logicalSource [ rml:source "students.json" ;
                                 rml:referenceFormulation ql:JSONPath ;
                                 rml:iterator "$.students[*]" ] ;
          rr:subjectMap [ rr:template "http://ex.org/student/{id}" ; rr:class ex:Student ] ;
          rr:predicateObjectMap [ rr:predicate ex:name ; rr:objectMap [ rml:reference "name" ] ] ,
                                [ rr:predicate ex:sport ; rr:objectMap [ rml:reference "sports[*]" ] ] ,
                                [ rr:predicate ex:score ; rr:objectMap [ rml:reference "score" ] ] ,
                                [ rr:predicate ex:active ; rr:objectMap [ rml:reference "active" ] ] .
        "#,
        MemoryLoader::from([("students.json", STUDENTS)]),
    );
    assert_eq!(
        out,
        [
            r#"<http://ex.org/student/1> <http://ex.org/active> "true"^^<http://www.w3.org/2001/XMLSchema#boolean> ."#,
            r#"<http://ex.org/student/1> <http://ex.org/name> "Venus Williams" ."#,
            r#"<http://ex.org/student/1> <http://ex.org/score> "9.5"^^<http://www.w3.org/2001/XMLSchema#double> ."#,
            r#"<http://ex.org/student/1> <http://ex.org/sport> "Golf" ."#,
            r#"<http://ex.org/student/1> <http://ex.org/sport> "Tennis" ."#,
            r#"<http://ex.org/student/1> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex.org/Student> ."#,
            r#"<http://ex.org/student/2> <http://ex.org/active> "false"^^<http://www.w3.org/2001/XMLSchema#boolean> ."#,
            r#"<http://ex.org/student/2> <http://ex.org/name> "Serena" ."#,
            r#"<http://ex.org/student/2> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex.org/Student> ."#,
            r#"<http://ex.org/student/3> <http://ex.org/sport> "Chess" ."#,
            r#"<http://ex.org/student/3> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex.org/Student> ."#,
        ]
    );
}

#[test]
fn new_rml_with_a_relative_path_source_and_shortcuts() {
    let out = lines(
        r#"
        <#S> a rmln:TriplesMap ;
          rmln:logicalSource [ a rmln:LogicalSource ;
              rmln:source [ a rmln:RelativePathSource ; rmln:root rmln:MappingDirectory ;
                            rmln:path "students.json" ] ;
              rmln:referenceFormulation rmln:JSONPath ;
              rmln:iterator "$.students[*]" ] ;
          rmln:subjectMap [ rmln:template "student/{name}" ; rmln:graph ex:g ] ;
          rmln:predicateObjectMap [
              rmln:predicate ex:label ;
              rmln:objectMap [ rmln:reference "name" ; rmln:language "en" ] ] ,
            [ rmln:predicate ex:id ;
              rmln:objectMap [ rmln:reference "id" ; rmln:datatype xsd:string ] ] ,
            [ rmln:predicate ex:kind ; rmln:object "student" ;
              rmln:graphMap [ rmln:constant rmln:defaultGraph ] ] .
        "#,
        MemoryLoader::from([("students.json", STUDENTS)]),
    );
    // Relative IRIs resolve against the base; template values are IRI-safe encoded.
    assert!(out.contains(
        &r#"<http://ex.org/base/student/Venus%20Williams> <http://ex.org/label> "Venus Williams"@en <http://ex.org/g> ."#
            .to_owned()
    ));
    assert!(
        out.contains(
            &r#"<http://ex.org/base/student/Serena> <http://ex.org/id> "2" <http://ex.org/g> ."#
                .to_owned()
        )
    );
    // The predicate-object map's graph adds to the subject map's.
    assert!(out.contains(
        &r#"<http://ex.org/base/student/Serena> <http://ex.org/kind> "student" ."#.to_owned()
    ));
    assert!(out.contains(
        &r#"<http://ex.org/base/student/Serena> <http://ex.org/kind> "student" <http://ex.org/g> ."#
            .to_owned()
    ));
    // No name, no subject.
    assert!(
        !out.iter()
            .any(|l| l.contains("Chess") || l.contains("\"3\""))
    );
    assert_eq!(out.len(), 8);
}

#[test]
fn joins_on_conditions_and_on_the_same_source() {
    let people = r#"[{"id": "p1", "name": "Ada", "team": "t1", "city": {"name": "Ghent"}},
                     {"id": "p2", "name": "Bob", "team": "t2", "city": {"name": "Leuven"}},
                     {"id": "p3", "name": "Cy", "team": "t1"}]"#;
    let teams = r#"{"teams": [{"code": "t1", "label": "Red"}, {"code": "t1", "label": "Rouge"},
                              {"code": "t3", "label": "Blue"}]}"#;
    let out = lines(
        r#"
        <#People> rml:logicalSource [ rml:source "people.json" ; rml:iterator "$[*]" ;
                                      rml:referenceFormulation ql:JSONPath ] ;
          rr:subjectMap [ rr:template "http://ex.org/p/{id}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:team ;
              rr:objectMap [ rr:parentTriplesMap <#Teams> ;
                             rr:joinCondition [ rr:child "team" ; rr:parent "code" ] ] ] ,
            [ rr:predicate ex:city ; rr:objectMap [ rr:parentTriplesMap <#Cities> ] ] .
        <#Teams> rml:logicalSource [ rml:source "teams.json" ; rml:iterator "$.teams[*]" ;
                                     rml:referenceFormulation ql:JSONPath ] ;
          rr:subjectMap [ rr:template "http://ex.org/team/{label}" ] .
        <#Cities> rml:logicalSource [ rml:source "people.json" ; rml:iterator "$[*]" ;
                                      rml:referenceFormulation ql:JSONPath ] ;
          rr:subjectMap [ rr:template "{city.name}" ; rr:termType rr:BlankNode ] ;
          rr:predicateObjectMap [ rr:predicate ex:name ; rr:objectMap [ rml:reference "city.name" ] ] .
        "#,
        MemoryLoader::from([("people.json", people), ("teams.json", teams)]),
    );
    assert_eq!(
        out,
        [
            r#"<http://ex.org/p/p1> <http://ex.org/city> _:bGhent ."#,
            r#"<http://ex.org/p/p1> <http://ex.org/team> <http://ex.org/team/Red> ."#,
            r#"<http://ex.org/p/p1> <http://ex.org/team> <http://ex.org/team/Rouge> ."#,
            r#"<http://ex.org/p/p2> <http://ex.org/city> _:bLeuven ."#,
            r#"<http://ex.org/p/p3> <http://ex.org/team> <http://ex.org/team/Red> ."#,
            r#"<http://ex.org/p/p3> <http://ex.org/team> <http://ex.org/team/Rouge> ."#,
            r#"_:bGhent <http://ex.org/name> "Ghent" ."#,
            r#"_:bLeuven <http://ex.org/name> "Leuven" ."#,
        ]
    );
}

#[test]
fn templates_take_every_combination_of_multiple_values() {
    let out = lines(
        r#"
        <#S> rml:logicalSource [ rml:source "d.json" ; rml:referenceFormulation ql:JSONPath ] ;
          rr:subjectMap [ rr:template "http://ex.org/{a[*]}-{b}" ] ;
          rr:predicateObjectMap [ rr:predicateMap [ rr:template "http://ex.org/p/{a[*]}" ] ;
                                  rr:objectMap [ rr:template "{b}!" ; rr:termType rr:Literal ] ] .
        "#,
        MemoryLoader::from([("d.json", r#"{"a": ["x", "y"], "b": "z"}"#)]),
    );
    assert_eq!(
        out,
        [
            r#"<http://ex.org/x-z> <http://ex.org/p/x> "z!" ."#,
            r#"<http://ex.org/x-z> <http://ex.org/p/y> "z!" ."#,
            r#"<http://ex.org/y-z> <http://ex.org/p/x> "z!" ."#,
            r#"<http://ex.org/y-z> <http://ex.org/p/y> "z!" ."#,
        ]
    );
}

#[test]
fn mistakes_and_unsupported_parts_are_reported() {
    let json = || MemoryLoader::from([("d.json", "{}")]);
    let e = error(
        r#"<#S> rml:logicalSource [ rml:source "d.json" ] ; rr:subjectMap [ rr:template "{a" ] ."#,
        json(),
    );
    assert!(
        matches!(&e, RmlError::Mapping(m) if m.contains("unbalanced")),
        "{e}"
    );
    let e = error(
        r#"<#S> rml:logicalSource [ rml:source "d.json" ] ."#,
        json(),
    );
    assert!(matches!(e, RmlError::Mapping(m) if m.contains("no subject map")));
    let e = error(
        r#"<#S> rml:logicalSource [ rml:source "d.json" ; rml:referenceFormulation <http://ex.org/Other> ] ;
             rr:subjectMap [ rr:template "{a}" ] ."#,
        json(),
    );
    assert!(matches!(e, RmlError::Unsupported(m) if m.contains("reference formulation")));
    let e = error(
        r#"<#S> rml:logicalSource [ rml:source "missing.json" ] ; rr:subjectMap [ rr:template "{a}" ] ."#,
        json(),
    );
    assert!(matches!(e, RmlError::Source { .. }));
    let e = error(
        r#"<#S> rml:logicalSource [ rml:source "d.json" ; rml:iterator "$[?(@.a)]" ] ;
             rr:subjectMap [ rr:template "{a}" ] ."#,
        json(),
    );
    assert!(matches!(e, RmlError::Path { .. }));
    let e = error(
        r#"<#S> rml:logicalSource [ rml:source "d.json" ] ;
             rr:subjectMap [ rr:template "{a}" ; rr:termType rr:Literal ] ."#,
        json(),
    );
    assert!(matches!(e, RmlError::Mapping(_)));
    // A reference to an array is a data error: `[*]` selects its elements.
    let e = error(
        r#"<#S> rml:logicalSource [ rml:source "d.json" ] ; rr:subjectMap [ rr:template "http://ex.org/{a}" ] ."#,
        MemoryLoader::from([("d.json", r#"{"a": [1, 2]}"#)]),
    );
    assert!(matches!(&e, RmlError::Data(m) if m.contains("[*]")), "{e}");
    // So is a value that makes an invalid IRI.
    let e = error(
        r#"<#S> rml:logicalSource [ rml:source "d.json" ] ; rr:subjectMap [ rml:reference "a" ] ."#,
        MemoryLoader::from([("d.json", r#"{"a": "not an IRI"}"#)]),
    );
    assert!(matches!(&e, RmlError::Data(_)), "{e}");
    // An invalid language tag is a mapping error.
    let e = error(
        r#"<#S> rml:logicalSource [ rml:source "d.json" ] ; rr:subjectMap [ rr:template "http://ex.org/{a}" ] ;
             rr:predicateObjectMap [ rr:predicate ex:p ; rr:objectMap [ rml:reference "a" ; rr:language "a-english" ] ] ."#,
        json(),
    );
    assert!(
        matches!(&e, RmlError::Mapping(m) if m.contains("language")),
        "{e}"
    );
    // A join without conditions over different sources.
    let e = error(
        r#"<#A> rml:logicalSource [ rml:source "d.json" ] ; rr:subjectMap [ rr:template "{a}" ] ;
             rr:predicateObjectMap [ rr:predicate ex:p ; rr:objectMap [ rr:parentTriplesMap <#B> ] ] .
           <#B> rml:logicalSource [ rml:source "e.json" ] ; rr:subjectMap [ rr:template "{b}" ] ."#,
        MemoryLoader::from([("d.json", "{}"), ("e.json", "{}")]),
    );
    assert!(matches!(e, RmlError::Mapping(m) if m.contains("join condition")));
}

#[test]
fn csv_sources_and_joins_with_json() {
    let csv = "\u{feff}ID,Name,Team,Score\n1,Ada Lovelace,t1,9\n2,Bob,t9,\n";
    let teams = r#"[{"code": "t1", "label": "Red"}]"#;
    let out = lines(
        r#"
        <#P> rml:logicalSource [ rml:source "people.csv" ; rml:referenceFormulation ql:CSV ] ;
          rr:subjectMap [ rr:template "http://ex.org/p/{ID}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:name ; rr:objectMap [ rml:reference "Name" ] ] ,
            [ rr:predicate ex:score ; rr:objectMap [ rr:column "Score" ; rr:datatype xsd:integer ] ] ,
            [ rr:predicate ex:team ;
              rr:objectMap [ rr:parentTriplesMap <#T> ;
                             rr:joinCondition [ rr:child "Team" ; rr:parent "code" ] ] ] .
        <#T> rml:logicalSource [ rml:source "teams.json" ; rml:iterator "$[*]" ] ;
          rr:subjectMap [ rr:template "http://ex.org/team/{label}" ] .
        "#,
        MemoryLoader::from([("people.csv", csv), ("teams.json", teams)]),
    );
    assert_eq!(
        out,
        [
            r#"<http://ex.org/p/1> <http://ex.org/name> "Ada Lovelace" ."#,
            r#"<http://ex.org/p/1> <http://ex.org/score> "9"^^<http://www.w3.org/2001/XMLSchema#integer> ."#,
            r#"<http://ex.org/p/1> <http://ex.org/team> <http://ex.org/team/Red> ."#,
            r#"<http://ex.org/p/2> <http://ex.org/name> "Bob" ."#,
            // An empty cell makes no term; Bob's team t9 joins nothing.
        ]
    );
}

#[test]
fn files_are_read_from_the_allowed_directories_only() {
    let dir = tripletrail_testdir::tempdir("rml");
    let inside = dir.join("data");
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::write(inside.join("d.json"), r#"{"a": "x"}"#).unwrap();
    std::fs::write(dir.join("secret.json"), r#"{"a": "s"}"#).unwrap();
    let loader = FsLoader {
        config: tripletrail_fx::FxConfig {
            fixed_limits: false,
            databases: None,
            base: inside.clone(),
            allowed: Some(vec![inside.clone()]),
            remote: Default::default(),
        },
    };
    let mapping = |file: &str| {
        Mapping::parse_turtle(
            &format!(
                r#"{PREFIXES} <http://ex.org/m> rml:logicalSource [ rml:source "{file}" ] ;
                     rr:subjectMap [ rr:template "http://ex.org/{{a}}" ; rr:class ex:C ] ."#
            ),
            None,
        )
        .unwrap()
    };
    assert_eq!(run(&mapping("d.json"), &loader).unwrap().len(), 1);
    let e = run(&mapping("../secret.json"), &loader).unwrap_err();
    assert!(
        matches!(&e, RmlError::Source { message, .. } if message.contains("outside")),
        "{e}"
    );
}

const VIEWS: &str = r#"
<#people> a rmln:LogicalSource ; rmln:source [ a rmln:RelativePathSource ; rmln:path "people.json" ] ;
    rmln:referenceFormulation rmln:JSONPath ; rmln:iterator "$.people[*]" .
<#years> a rmln:LogicalSource ; rmln:source [ a rmln:RelativePathSource ; rmln:path "years.csv" ] ;
    rmln:referenceFormulation rmln:CSV .
<#peopleView> a rmln:LogicalView ; rmln:viewOn <#people> ;
    rmln:field [ a rmln:ExpressionField ; rmln:fieldName "name" ; rmln:reference "$.name" ] ;
    rmln:field [ a rmln:IterableField ; rmln:fieldName "item" ; rmln:iterator "$.items[*]" ;
        rmln:field [ a rmln:ExpressionField ; rmln:fieldName "type" ; rmln:reference "$.type" ] ] .
<#yearView> a rmln:LogicalView ; rmln:viewOn <#years> ;
    rmln:field [ a rmln:ExpressionField ; rmln:fieldName "who" ; rmln:reference "name" ] ;
    rmln:field [ a rmln:ExpressionField ; rmln:fieldName "year" ; rmln:reference "year" ] ;
    rmln:leftJoin [ rmln:parentLogicalView <#peopleView> ;
        rmln:joinCondition [ rmln:child "who" ; rmln:parent "name" ] ;
        rmln:field [ a rmln:ExpressionField ; rmln:fieldName "thing" ; rmln:reference "item.type" ] ] .
"#;

fn view_files() -> MemoryLoader {
    MemoryLoader::from([
        (
            "people.json",
            r#"{"people": [{"name": "ada", "items": [{"type": "pen"}, {"type": "cup"}]},
                           {"name": "bo", "items": []}]}"#,
        ),
        ("years.csv", "name,year\nada,1815\ncy,1900\n"),
    ])
}

#[test]
fn logical_views_with_nested_fields_index_keys_and_joins() {
    let items = format!(
        "{VIEWS}{}",
        r##"<#items> rmln:logicalSource <#peopleView> ;
            rmln:subjectMap [ rmln:template "http://ex.org/{name}/item/{item.#}" ] ;
            rmln:predicateObjectMap [ rmln:predicate ex:type ; rmln:objectMap [ rmln:reference "item.type" ] ] ;
            rmln:predicateObjectMap [ rmln:predicate ex:row ; rmln:objectMap [ rmln:reference "#" ] ] .
        <#years> rmln:logicalSource <#yearView> ;
            rmln:subjectMap [ rmln:template "http://ex.org/{who}" ] ;
            rmln:predicateObjectMap [ rmln:predicate ex:year ; rmln:objectMap [ rmln:reference "year" ] ] ;
            rmln:predicateObjectMap [ rmln:predicate ex:has ;
                rmln:objectMap [ rmln:template "{thing}-{thing.#}" ; rmln:termType rmln:Literal ] ] ."##
    );
    assert_eq!(
        lines(&items, view_files()),
        [
            r#"<http://ex.org/ada/item/0> <http://ex.org/row> "0"^^<http://www.w3.org/2001/XMLSchema#integer> ."#,
            r#"<http://ex.org/ada/item/0> <http://ex.org/type> "pen" ."#,
            r#"<http://ex.org/ada/item/1> <http://ex.org/row> "0"^^<http://www.w3.org/2001/XMLSchema#integer> ."#,
            r#"<http://ex.org/ada/item/1> <http://ex.org/type> "cup" ."#,
            r#"<http://ex.org/ada> <http://ex.org/has> "cup-1" ."#,
            r#"<http://ex.org/ada> <http://ex.org/has> "pen-0" ."#,
            r#"<http://ex.org/ada> <http://ex.org/year> "1815" ."#,
            // A left join keeps a record without a match.
            r#"<http://ex.org/cy> <http://ex.org/year> "1900" ."#,
        ]
    );
    // An inner join drops it.
    let inner = items.replace("rmln:leftJoin", "rmln:innerJoin");
    assert!(
        !lines(&inner, view_files())
            .iter()
            .any(|l| l.contains("/cy>"))
    );
}

#[test]
fn logical_view_mistakes_are_reported() {
    let map = |reference: &str| {
        format!(
            r#"{VIEWS}<#m> rmln:logicalSource <#peopleView> ;
                rmln:subjectMap [ rmln:template "http://ex.org/{{name}}" ] ;
                rmln:predicateObjectMap [ rmln:predicate ex:p ; rmln:objectMap [ rmln:reference "{reference}" ] ] ."#
        )
    };
    for (reference, message) in [
        ("nope", "is not a field"),
        ("<it>", "not values"),
        ("item", "not values"),
    ] {
        let e = error(&map(reference), view_files()).to_string();
        assert!(e.contains(message), "{reference}: {e}");
    }
    // A view on itself, and two fields with one name.
    let cycle = format!(
        r#"{PREFIXES}ex:v rmln:viewOn ex:v ; rmln:field [ rmln:fieldName "a" ; rmln:reference "a" ] ."#
    );
    let e = Mapping::parse_turtle(&cycle, None).unwrap_err();
    assert!(e.to_string().contains("itself"), "{e}");
    let twice = VIEWS.replace("\"year\" ; rmln:reference", "\"who\" ; rmln:reference");
    let e =
        Mapping::parse_turtle(&format!("{PREFIXES}{twice}"), Some("http://ex.org/")).unwrap_err();
    assert!(e.to_string().contains("two fields named who"), "{e}");
}

#[test]
fn explain_reports_views_and_maps_with_their_counts() {
    let text = format!(
        "{PREFIXES}{VIEWS}{}",
        r#"<#years> rmln:logicalSource <#yearView> ;
            rmln:subjectMap [ rmln:template "http://ex.org/{who}" ; rmln:class ex:Person ] ;
            rmln:predicateObjectMap [ rmln:predicate ex:year ;
                rmln:objectMap [ rmln:reference "year" ; rmln:datatype xsd:gYear ] ] ."#
    );
    let mapping = Mapping::parse_turtle(&text, Some("http://ex.org/base/")).unwrap();
    let report = tripletrail_rml::explain(&mapping, &view_files()).unwrap();
    let text = report.to_string();
    assert_eq!(
        report.views.iter().map(|v| v.records).collect::<Vec<_>>(),
        [3, 3]
    );
    // Duplicates count: ada has two records (one per item she joins with).
    assert_eq!((report.maps[0].records, report.maps[0].quads), (3, 6));
    for part in [
        "field item.type = reference \"$.type\" (literal)",
        "left join with <http://ex.org/base/#peopleView> on reference \"who\" (literal) = \
         reference \"name\" (literal)",
        "subject template \"http://ex.org/{who}\" (IRI)",
        "class <http://ex.org/Person>",
        "<http://ex.org/year> -> reference \"year\" (literal) ^^<http://www.w3.org/2001/XMLSchema#gYear>",
        "=> 3 records, 6 quads",
    ] {
        assert!(text.contains(part), "{part}\n{text}");
    }
}

#[test]
fn yarrrml_documents_run_like_the_rml_they_stand_for() {
    let yarrrml = r#"
prefixes:
  ex: http://ex.org/
sources:
  people: [people.json~jsonpath, "$.people[*]"]
mappings:
  person:
    sources: people
    s: ex:person/$(name)
    po:
      - [a, ex:Person]
      - [ex:name, $(name), en~lang]
      - p: ex:age
        o: {value: $(age), datatype: xsd:integer}
      - [ex:home, ex:town/$(town)~iri]
      - p: ex:lives
        o:
          - mapping: town
            condition:
              function: equal
              parameters:
                - [str1, $(town), s]
                - [str2, $(code), o]
  town:
    sources:
      - access: towns.csv
        referenceFormulation: csv
    s: ex:t/$(code)
    graphs: ex:towns
    po:
      - [rdfs:label, $(label)]
"#;
    let files = MemoryLoader::from([
        (
            "people.json",
            r#"{"people": [{"name": "ada", "age": 36, "town": "L"}]}"#,
        ),
        ("towns.csv", "code,label\nL,London\n"),
    ]);
    let mapping = Mapping::parse_yarrrml(yarrrml, Some("http://ex.org/base")).unwrap();
    let mut out: Vec<String> = run(&mapping, &files)
        .unwrap()
        .iter()
        .map(|q| format!("{q} ."))
        .collect();
    out.sort();
    assert_eq!(
        out,
        [
            r#"<http://ex.org/person/ada> <http://ex.org/age> "36"^^<http://www.w3.org/2001/XMLSchema#integer> ."#,
            r#"<http://ex.org/person/ada> <http://ex.org/home> <http://ex.org/town/L> ."#,
            r#"<http://ex.org/person/ada> <http://ex.org/lives> <http://ex.org/t/L> ."#,
            r#"<http://ex.org/person/ada> <http://ex.org/name> "ada"@en ."#,
            r#"<http://ex.org/person/ada> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex.org/Person> ."#,
            r#"<http://ex.org/t/L> <http://www.w3.org/2000/01/rdf-schema#label> "London" <http://ex.org/towns> ."#,
        ]
    );
    // Functions are not supported, and say so.
    let functions = r#"
mappings:
  m:
    sources: [[a.csv~csv]]
    s: http://ex.org/$(id)
    po:
      - p: http://ex.org/p
        o:
          function: grel:toUpperCase
          parameters: [[grel:valueParameter, $(name)]]
"#;
    assert!(matches!(
        Mapping::parse_yarrrml(functions, None),
        Err(RmlError::Unsupported(_))
    ));
    assert!(Mapping::parse_yarrrml("mappings: [", None).is_err());
}

const STATIONS_XML: &str = r#"<?xml version="1.0"?>
<network xmlns:r="https://www.railml.org/schemas/3.2">
  <r:station id="OSL"><r:name>Oslo S</r:name><r:track>1</r:track><r:track>2</r:track></r:station>
  <r:station id="LLS"><r:name>Lillestrøm</r:name><r:track>NULL</r:track></r:station>
</network>"#;

#[test]
fn xml_sources_with_xpath() {
    let out = lines(
        r#"
        <#S> rmln:logicalSource [ rmln:source [ a rmln:RelativePathSource ; rmln:path "stations.xml" ; rmln:null "NULL" ] ;
                                  rmln:referenceFormulation rmln:XPath ;
                                  rmln:iterator "/network/r:station" ] ;
          rmln:subjectMap [ rmln:template "http://ex.org/station/{@id}" ] ;
          rmln:predicateObjectMap [ rmln:predicate ex:name ; rmln:objectMap [ rmln:reference "r:name/text()" ] ] ,
                                  [ rmln:predicate ex:track ; rmln:objectMap [ rmln:reference "r:track" ] ] .
        "#,
        MemoryLoader::from([("stations.xml", STATIONS_XML)]),
    );
    assert_eq!(
        out,
        [
            r#"<http://ex.org/station/LLS> <http://ex.org/name> "Lillestrøm" ."#,
            r#"<http://ex.org/station/OSL> <http://ex.org/name> "Oslo S" ."#,
            r#"<http://ex.org/station/OSL> <http://ex.org/track> "1" ."#,
            r#"<http://ex.org/station/OSL> <http://ex.org/track> "2" ."#,
        ]
    );
}

#[test]
fn compressed_and_utf16_sources() {
    use std::io::Write;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(b"id,name\n1,Ada\n2,\n").unwrap();
    let gz = gz.finish().unwrap();
    let utf16: Vec<u8> = [0xFF, 0xFE]
        .into_iter()
        .chain("id,name\n3,Åse\n".encode_utf16().flat_map(u16::to_le_bytes))
        .collect();
    let mapping = |file: &str, extra: &str| {
        format!(
            r#"<#{file}> rmln:logicalSource [ rmln:source [ a rmln:RelativePathSource ; rmln:path "{file}" {extra} ] ;
                                    rmln:referenceFormulation rmln:CSV ] ;
              rmln:subjectMap [ rmln:template "http://ex.org/p/{{id}}" ] ;
              rmln:predicateObjectMap [ rmln:predicate ex:name ; rmln:objectMap [ rmln:reference "name" ] ] ."#
        )
    };
    let loader = || {
        MemoryLoader(
            [
                ("p.csv.gz".to_owned(), gz.clone()),
                ("p16.csv".to_owned(), utf16.clone()),
            ]
            .into(),
        )
    };
    // RML-IO: an empty cell is an empty string unless rml:null says otherwise.
    assert_eq!(
        lines(
            &mapping("p.csv.gz", "; rmln:compression rmln:gzip"),
            loader()
        ),
        [
            r#"<http://ex.org/p/1> <http://ex.org/name> "Ada" ."#,
            r#"<http://ex.org/p/2> <http://ex.org/name> "" ."#,
        ]
    );
    assert_eq!(
        lines(
            &mapping("p.csv.gz", r#"; rmln:compression rmln:gzip ; rmln:null """#),
            loader()
        ),
        [r#"<http://ex.org/p/1> <http://ex.org/name> "Ada" ."#]
    );
    assert_eq!(
        lines(&mapping("p16.csv", "; rmln:encoding rmln:UTF-16"), loader()),
        [r#"<http://ex.org/p/3> <http://ex.org/name> "Åse" ."#]
    );
    // A reference to a column the header does not name is an error.
    let e = error(
        &mapping("p16.csv", "; rmln:encoding rmln:UTF-16")
            .replace("{{id}}", "{{nope}}")
            .replace("{id}", "{nope}"),
        loader(),
    );
    assert!(e.to_string().contains("names no column"), "{e}");
}

#[test]
fn short_csv_rows_are_null_in_legacy_rml_and_an_error_in_rml_io() {
    let files =
        || MemoryLoader([("p.csv".to_owned(), b"id,name\n1,Ada\n2\n3,Bo,x\n".to_vec())].into());
    // Legacy RML: row 2 has no name, so no triple; the extra cell of row 3 is ignored.
    let legacy = r#"<#m> rml:logicalSource [ rml:source "p.csv" ; rml:referenceFormulation ql:CSV ] ;
          rr:subjectMap [ rr:template "http://ex.org/p/{id}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:name ; rr:objectMap [ rml:reference "name" ] ] ."#;
    assert_eq!(
        lines(legacy, files()),
        [
            r#"<http://ex.org/p/1> <http://ex.org/name> "Ada" ."#,
            r#"<http://ex.org/p/3> <http://ex.org/name> "Bo" ."#,
        ]
    );
    // RML-IO: a row with fewer cells than the header is an error.
    let io = r#"<#m> rmln:logicalSource [ rmln:source [ a rmln:RelativePathSource ; rmln:path "p.csv" ] ;
                                rmln:referenceFormulation rmln:CSV ] ;
          rmln:subjectMap [ rmln:template "http://ex.org/p/{id}" ] ;
          rmln:predicateObjectMap [ rmln:predicate ex:name ; rmln:objectMap [ rmln:reference "name" ] ] ."#;
    let e = error(io, files());
    assert!(e.to_string().contains("CSV row 3 has 1 cells"), "{e}");
}

const STATIONS: &str = "id,name,x,y\n1,Oslo S,597000,6643000\n2,Lillestrøm,617000,6650000\n";

#[test]
fn functions_make_values() {
    let out = lines(
        r#"
        @prefix grel: <http://users.ugent.be/~bjdmeest/function/grel.ttl#> .
        @prefix ttfx: <http://w3id.org/tripletrail/fx#> .
        <#S> rmln:logicalSource [ rmln:source [ a rmln:RelativePathSource ; rmln:path "stations.csv" ] ;
                                  rmln:referenceFormulation rmln:CSV ] ;
          rmln:subjectMap [ rmln:template "http://ex.org/station/{id}" ] ;
          rmln:predicateObjectMap [
            rmln:predicate ex:code ;
            rmln:objectMap [
              rmln:functionExecution [ rmln:function grel:toUpperCase ;
                rmln:input [ rmln:parameter grel:valueParam ; rmln:inputValueMap [ rmln:reference "name" ] ] ] ;
              rmln:return grel:stringOut ] ] ,
          [ rmln:predicate ex:geometry ;
            rmln:objectMap [
              rmln:functionExecution [ rmln:function ttfx:point ;
                rmln:input [ rmln:parameter ttfx:x ; rmln:inputValueMap [ rmln:reference "x" ] ] ,
                           [ rmln:parameter ttfx:y ; rmln:inputValueMap [ rmln:reference "y" ] ] ,
                           [ rmln:parameter ttfx:crs ; rmln:inputValue "EPSG:25833" ] ] ] ] ,
          [ rmln:predicate ex:central ;
            rmln:objectMap [ rmln:constant true ;
              rmln:condition [
                rmln:functionExecution [ rmln:function grel:string_endsWith ;
                  rmln:input [ rmln:parameter grel:valueParam ; rmln:inputValueMap [ rmln:reference "name" ] ] ,
                             [ rmln:parameter grel:string_sub ; rmln:inputValue " S" ] ] ] ] ] .
        "#,
        MemoryLoader::from([("stations.csv", STATIONS)]),
    );
    assert_eq!(
        out,
        [
            r#"<http://ex.org/station/1> <http://ex.org/central> "true"^^<http://www.w3.org/2001/XMLSchema#boolean> ."#,
            r#"<http://ex.org/station/1> <http://ex.org/code> "OSLO S" ."#,
            r#"<http://ex.org/station/1> <http://ex.org/geometry> "<http://www.opengis.net/def/crs/EPSG/0/25833> POINT(597000 6643000)"^^<http://www.opengis.net/ont/geosparql#wktLiteral> ."#,
            r#"<http://ex.org/station/2> <http://ex.org/code> "LILLESTRØM" ."#,
            r#"<http://ex.org/station/2> <http://ex.org/geometry> "<http://www.opengis.net/def/crs/EPSG/0/25833> POINT(617000 6650000)"^^<http://www.opengis.net/ont/geosparql#wktLiteral> ."#,
        ]
    );
}

#[test]
fn legacy_function_values() {
    let out = lines(
        r#"
        @prefix fnml: <http://semweb.mmlab.be/ns/fnml#> .
        @prefix fno: <https://w3id.org/function/ontology#> .
        @prefix grel: <http://users.ugent.be/~bjdmeest/function/grel.ttl#> .
        <#S> rml:logicalSource [ rml:source "stations.csv" ; rml:referenceFormulation ql:CSV ] ;
          rr:subjectMap [ rr:template "http://ex.org/station/{id}" ] ;
          rr:predicateObjectMap [ rr:predicate ex:code ;
            rr:objectMap [ fnml:functionValue [
              rr:predicateObjectMap [ rr:predicate fno:executes ; rr:objectMap [ rr:constant grel:toLowerCase ] ] ,
                                    [ rr:predicate grel:valueParam ; rr:objectMap [ rml:reference "name" ] ] ] ] ] .
        "#,
        MemoryLoader::from([("stations.csv", STATIONS)]),
    );
    assert_eq!(
        out,
        [
            r#"<http://ex.org/station/1> <http://ex.org/code> "oslo s" ."#,
            r#"<http://ex.org/station/2> <http://ex.org/code> "lillestrøm" ."#,
        ]
    );
}

#[test]
fn unknown_functions_parameters_and_outputs_are_errors() {
    let mapping = |function: &str, parameter: &str, output: &str| {
        format!(
            r#"
            @prefix grel: <http://users.ugent.be/~bjdmeest/function/grel.ttl#> .
            <#S> rmln:logicalSource [ rmln:source [ a rmln:RelativePathSource ; rmln:path "stations.csv" ] ;
                                      rmln:referenceFormulation rmln:CSV ] ;
              rmln:subjectMap [ rmln:template "http://ex.org/station/{{id}}" ] ;
              rmln:predicateObjectMap [ rmln:predicate ex:code ;
                rmln:objectMap [
                  rmln:functionExecution [ rmln:function grel:{function} ;
                    rmln:input [ rmln:parameter grel:{parameter} ; rmln:inputValueMap [ rmln:reference "name" ] ] ] ;
                  rmln:return grel:{output} ] ] .
            "#
        )
    };
    let files = || MemoryLoader::from([("stations.csv", STATIONS)]);
    let e = error(&mapping("nope", "valueParam", "stringOut"), files()).to_string();
    assert!(e.contains("unknown function"), "{e}");
    let e = error(&mapping("toUpperCase", "nope", "stringOut"), files()).to_string();
    assert!(e.contains("no parameter"), "{e}");
    let e = error(&mapping("toUpperCase", "valueParam", "nope"), files()).to_string();
    assert!(e.contains("no output"), "{e}");
}

/// A mapping whose object map is `<#o>`, described by `object`.
fn object_map(object: &str) -> String {
    format!(
        r#"
        @prefix grel: <http://users.ugent.be/~bjdmeest/function/grel.ttl#> .
        <#m> rmln:logicalSource [ rmln:source [ a rmln:RelativePathSource ; rmln:path "stations.csv" ] ;
                                  rmln:referenceFormulation rmln:CSV ] ;
            rmln:subjectMap [ rmln:template "http://ex.org/s/{{id}}" ] ;
            rmln:predicateObjectMap [ rmln:predicate ex:name ; rmln:objectMap <#o> ] .
        {object}
        "#
    )
}

#[test]
fn term_maps_made_of_themselves_are_errors() {
    let files = || MemoryLoader::from([("stations.csv", STATIONS)]);
    for object in [
        // A function input.
        r#"<#o> rmln:functionExecution [ rmln:function grel:toUpperCase ;
               rmln:input [ rmln:parameter grel:valueParam ; rmln:inputValueMap <#o> ] ] ."#,
        // A condition.
        r#"<#o> rmln:reference "name" ; rmln:condition <#o> ."#,
        // A language map.
        r#"<#o> rmln:reference "name" ; rmln:languageMap <#o> ."#,
        // A datatype map.
        r#"<#o> rmln:reference "name" ; rmln:datatypeMap <#o> ."#,
        // A cycle through a second term map.
        r#"<#o> rmln:reference "name" ; rmln:languageMap <#l> .
           <#l> rmln:reference "name" ; rmln:condition <#o> ."#,
    ] {
        let e = error(&object_map(object), files()).to_string();
        assert!(e.contains("is defined in terms of itself"), "{object}: {e}");
        assert!(
            e.contains("http://ex.org/base/#o") || e.contains("#l"),
            "{e}"
        );
    }
}

#[test]
fn deeply_nested_term_maps_are_errors() {
    let files = || MemoryLoader::from([("stations.csv", STATIONS)]);
    let call = |inner: &str| {
        format!(
            r#"[ rmln:functionExecution [ rmln:function grel:toUpperCase ;
                 rmln:input [ rmln:parameter grel:valueParam ; rmln:inputValueMap {inner} ] ] ]"#
        )
    };
    let nest = |depth: usize| {
        let mut map = r#"[ rmln:reference "name" ]"#.to_owned();
        for _ in 0..depth {
            map = call(&map);
        }
        format!(
            r#"<#o> rmln:functionExecution [ rmln:function grel:toUpperCase ;
                 rmln:input [ rmln:parameter grel:valueParam ; rmln:inputValueMap {map} ] ] ."#
        )
    };
    // Shallow nesting is fine.
    assert_eq!(lines(&object_map(&nest(10)), files()).len(), 2);
    let e = error(&object_map(&nest(100)), files()).to_string();
    assert!(e.contains("term maps deep"), "{e}");
}

#[test]
fn term_maps_shared_at_every_level_are_errors() {
    // Each map gives both inputs of a concat the next map, so 40 levels would read and
    // evaluate 2^40 term maps.
    let files = || MemoryLoader::from([("stations.csv", STATIONS)]);
    let chain = |levels: usize| {
        let mut maps =
            String::from("@prefix idlab-fn: <https://w3id.org/imec/idlab/function#> .\n");
        for i in 0..levels {
            maps += &format!(
                r#"<#o{i}> rmln:functionExecution [ rmln:function idlab-fn:concat ;
                     rmln:input [ rmln:parameter idlab-fn:_str ; rmln:inputValueMap <#o{n}> ] ,
                                [ rmln:parameter idlab-fn:_otherStr ; rmln:inputValueMap <#o{n}> ] ] .
                "#,
                n = i + 1
            );
        }
        maps += &format!("<#o{levels}> rmln:reference \"id\" .\n");
        object_map(&maps.replace("<#o0>", "<#o>"))
    };
    // Five levels: 32 copies of the id.
    let out = lines(&chain(5), files());
    assert!(
        out[0].contains(&format!("\"{}\"", "1".repeat(32))),
        "{out:?}"
    );
    let e = error(&chain(40), files()).to_string();
    assert!(e.contains("more than 100000 term maps"), "{e}");
}

#[test]
fn logical_targets_get_their_quads() {
    let mapping = Mapping::parse_turtle(
        &format!(
            r#"{PREFIXES}
            @prefix formats: <http://www.w3.org/ns/formats/> .
            <#S> rmln:logicalSource [ rmln:source [ a rmln:RelativePathSource ; rmln:path "stations.csv" ] ;
                                      rmln:referenceFormulation rmln:CSV ] ;
              rmln:subjectMap [ rmln:template "http://ex.org/station/{{id}}" ] ;
              rmln:predicateObjectMap [ rmln:predicate ex:name ; rmln:objectMap [ rmln:reference "name" ] ] ,
                [ rmln:predicate ex:x ;
                  rmln:objectMap [ rmln:reference "x" ;
                    rmln:datatypeMap [ rmln:constant xsd:integer ; rmln:logicalTarget <#T> ] ] ] .
            <#T> rmln:target [ a rmln:FilePath ; rmln:path "x.ttl.zip" ; rmln:compression rmln:zip ;
                               rmln:encoding rmln:UTF-16 ] ;
              rmln:serialization formats:Turtle ."#
        ),
        Some("http://ex.org/base/"),
    )
    .unwrap();
    let made =
        tripletrail_rml::run_targets(&mapping, &MemoryLoader::from([("stations.csv", STATIONS)]))
            .unwrap();
    assert_eq!(made.default.len(), 2);
    assert!(
        made.default
            .iter()
            .all(|q| q.predicate.as_str().ends_with("name"))
    );
    assert_eq!(made.targets[0].len(), 2);
    let target = &mapping.targets[0];
    let bytes = tripletrail_rml::write_target(target, &made.targets[0]).unwrap();
    assert_eq!(&bytes[..2], b"PK", "a zip file");
    let mut read = tripletrail_rml::read_target(target, bytes).unwrap();
    read.sort_by_key(|q| q.to_string());
    let mut wanted = made.targets[0].clone();
    wanted.sort_by_key(|q| q.to_string());
    assert_eq!(read, wanted);
    // `run` gives every quad, whatever its target.
    assert_eq!(
        run(&mapping, &MemoryLoader::from([("stations.csv", STATIONS)]))
            .unwrap()
            .len(),
        4
    );
}

#[test]
fn gather_maps_make_lists_and_containers() {
    let out = lines(
        r#"
        @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
        <#S> rmln:logicalSource [ rmln:source [ a rmln:RelativePathSource ; rmln:path "stations.csv" ] ;
                                  rmln:referenceFormulation rmln:CSV ] ;
          rmln:subjectMap [ rmln:template "http://ex.org/station/{id}" ] ;
          rmln:predicateObjectMap [ rmln:predicate ex:xy ;
            rmln:objectMap [ rmln:gather ( [ rmln:reference "x" ] [ rmln:reference "y" ] ) ;
                             rmln:gatherAs rdf:List ] ] ,
          [ rmln:predicate ex:line ;
            rmln:objectMap [ rmln:constant ex:line1 ;
                             rmln:gather ( [ rmln:reference "name" ] ) ; rmln:gatherAs rdf:Seq ] ] .
        "#,
        MemoryLoader::from([("stations.csv", STATIONS)]),
    );
    let text = out.join("\n");
    // Each station's coordinates as a two-element list.
    assert_eq!(text.matches("rdf-syntax-ns#first").count(), 4, "{text}");
    assert_eq!(text.matches("rdf-syntax-ns#nil").count(), 2, "{text}");
    // One sequence named by the constant, gathering both stations in order.
    for line in [
        r#"<http://ex.org/line1> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://www.w3.org/1999/02/22-rdf-syntax-ns#Seq> ."#,
        r#"<http://ex.org/line1> <http://www.w3.org/1999/02/22-rdf-syntax-ns#_1> "Oslo S" ."#,
        r#"<http://ex.org/line1> <http://www.w3.org/1999/02/22-rdf-syntax-ns#_2> "Lillestrøm" ."#,
        r#"<http://ex.org/station/2> <http://ex.org/line> <http://ex.org/line1> ."#,
    ] {
        assert!(out.iter().any(|l| l == line), "{line}\n{text}");
    }
}

#[test]
fn gather_maps_that_never_end_are_errors() {
    let files = || MemoryLoader::from([("stations.csv", STATIONS)]);
    let gather = |object: &str| {
        format!(
            r#"
            @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
            <#S> rmln:logicalSource [ rmln:source [ a rmln:RelativePathSource ; rmln:path "stations.csv" ] ;
                                      rmln:referenceFormulation rmln:CSV ] ;
              rmln:subjectMap [ rmln:template "http://ex.org/station/{{id}}" ] ;
              rmln:predicateObjectMap [ rmln:predicate ex:xy ; rmln:objectMap <#g> ] .
            {object}
            "#
        )
    };
    // A gather map among its own items.
    let e = error(
        &gather(r#"<#g> rmln:gather ( [ rmln:reference "x" ] <#g> ) ; rmln:gatherAs rdf:List ."#),
        files(),
    )
    .to_string();
    assert!(e.contains("is defined in terms of itself"), "{e}");
    // A gather list whose rdf:rest goes back to its start.
    let e = error(
        &gather(
            r#"<#g> rmln:gather <#c> ; rmln:gatherAs rdf:List .
               <#c> rdf:first [ rmln:reference "x" ] ; rdf:rest <#c> ."#,
        ),
        files(),
    )
    .to_string();
    assert!(e.contains("a list that never ends"), "{e}");
}

#[test]
fn cartesian_products_past_the_limit_are_errors() {
    let json = r#"{"rows": [{"id": 1, "a": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]}]}"#;
    let files = || MemoryLoader::from([("rows.json", json)]);
    let mapping = |items: usize| {
        format!(
            r#"
            @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
            <#S> rmln:logicalSource [ rmln:source [ a rmln:RelativePathSource ; rmln:path "rows.json" ] ;
                                      rmln:referenceFormulation rmln:JSONPath ; rmln:iterator "$.rows[*]" ] ;
              rmln:subjectMap [ rmln:template "http://ex.org/r/{{id}}" ] ;
              rmln:predicateObjectMap [ rmln:predicate ex:a ;
                rmln:objectMap [ rmln:gather ( {} ) ; rmln:gatherAs rdf:List ;
                                 rmln:strategy rmln:cartesianProduct ] ] .
            "#,
            r#"[ rmln:reference "a[*]" ] "#.repeat(items)
        )
    };
    // Two items of ten values: a hundred lists of two.
    let out = lines(&mapping(2), files());
    assert_eq!(
        out.iter()
            .filter(|l| l.contains("<http://ex.org/a>"))
            .count(),
        100
    );
    // Six items: a million lists, past the limit.
    let e = error(&mapping(6), files()).to_string();
    assert!(e.contains("Cartesian product"), "{e}");
}

#[test]
fn gather_joins_keep_the_parents_collections_out() {
    // The parent's subjects are gathered lists; joining to them from another gather map
    // builds the parent's lists for the join index, which must not end up in the child's
    // output as well.
    let out = lines(
        r#"
        @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
        <#P> rmln:logicalSource [ rmln:source [ a rmln:RelativePathSource ; rmln:path "stations.csv" ] ;
                                  rmln:referenceFormulation rmln:CSV ] ;
          rmln:subjectMap [ rmln:gather ( [ rmln:reference "x" ] [ rmln:reference "y" ] ) ;
                            rmln:gatherAs rdf:List ; rmln:class ex:Point ] .
        <#S> rmln:logicalSource [ rmln:source [ a rmln:RelativePathSource ; rmln:path "stations.csv" ] ;
                                  rmln:referenceFormulation rmln:CSV ] ;
          rmln:subjectMap [ rmln:template "http://ex.org/station/{id}" ] ;
          rmln:predicateObjectMap [ rmln:predicate ex:points ;
            rmln:objectMap [ rmln:gather ( [ rmln:parentTriplesMap <#P> ;
                               rmln:joinCondition [ rmln:child "id" ; rmln:parent "id" ] ] ) ;
                             rmln:gatherAs rdf:List ] ] .
        "#,
        MemoryLoader::from([("stations.csv", STATIONS)]),
    );
    let text = out.join("\n");
    // Two two-element lists from <#P>, and one one-element list per station from <#S>.
    assert_eq!(text.matches("rdf-syntax-ns#first").count(), 6, "{text}");
}

/// A spreadsheet from the Facade-X tests: Sheet1 has the header A, B, C and two rows;
/// Sheet2 three rows, the first (A1, B1, C1) read as its header.
fn workbook(name: &str) -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tripletrail-fx/tests/sparql-anything/spreadsheet")
            .join(name),
    )
    .unwrap()
}

#[test]
fn spreadsheet_sources_read_as_csv() {
    for (file, data) in [("Book1.xlsx", "book.xlsx"), ("1.xls", "book.xls")] {
        let mapping = |iterator: &str, subject: &str, object: &str| {
            format!(
                r#"
                <#S> rml:logicalSource [ rml:source "{data}" ; rml:referenceFormulation ql:CSV {iterator} ] ;
                  rr:subjectMap [ rr:template "http://ex.org/{{{subject}}}" ] ;
                  rr:predicateObjectMap [ rr:predicate ex:c ; rr:objectMap [ rml:reference "{object}" ] ] .
                "#
            )
        };
        let files = MemoryLoader::from([(data, workbook(file))]);
        // The first sheet by default, its first row the header.
        assert_eq!(
            lines(&mapping("", "A", "C"), files.clone()),
            [
                r#"<http://ex.org/A1> <http://ex.org/c> "C1" ."#,
                r#"<http://ex.org/A2> <http://ex.org/c> "C2" ."#,
            ],
            "{file}"
        );
        // rml:iterator names the sheet.
        let sheet2 = r#"; rml:iterator "Sheet2""#;
        assert_eq!(
            lines(&mapping(sheet2, "A1", "C1"), files.clone()),
            [
                r#"<http://ex.org/A11> <http://ex.org/c> "C11" ."#,
                r#"<http://ex.org/A12> <http://ex.org/c> "C12" ."#,
            ],
            "{file}"
        );
        // A column the header does not name, and a sheet the workbook does not have, are errors.
        let e = error(&mapping(sheet2, "A1", "C"), files.clone());
        assert!(e.to_string().contains("names no column"), "{file}: {e}");
        // Without a reference formulation the extension says CSV; rml:null values are no value.
        let plain = mapping("", "A", "C").replace("; rml:referenceFormulation ql:CSV", "");
        let nulls = plain.replace(
            r#"rml:source "{data}""#.replace("{data}", data).as_str(),
            &format!(
                r#"rml:source [ a rmln:RelativePathSource ; rmln:path "{data}" ; rmln:null "C2" ]"#
            ),
        );
        assert_eq!(lines(&plain, files.clone()).len(), 2, "{file}");
        assert_eq!(
            lines(&nulls, files.clone()),
            [r#"<http://ex.org/A1> <http://ex.org/c> "C1" ."#],
            "{file}"
        );
        let e = error(&mapping(r#"; rml:iterator "Nope""#, "A", "C"), files);
        assert!(e.to_string().contains("no sheet \"Nope\""), "{file}: {e}");
    }
}

const SPEEDS: &str = "track,from,to,speed,source\nT1,A,B,120,survey\nT2,B,C,80,\n";

/// RML-star: a non-asserted map's triples are quoted, as RDF 1.2 reifiers, by a subject map
/// (same record) and by an object map (joined); the reifier of a triple is the same at each
/// use, and stated next to the triples using it.
#[test]
fn quoted_triples_maps_make_reifiers() {
    let mapping = r#"
<#Speed> a rmln:NonAssertedTriplesMap ;
  rmln:logicalSource [ rmln:source [ a rmln:RelativePathSource ; rmln:path "speeds.csv" ] ;
                       rmln:referenceFormulation rmln:CSV ] ;
  rmln:subjectMap [ rmln:template "http://ex.org/{track}" ] ;
  rmln:predicateObjectMap [ rmln:predicate ex:speed ;
    rmln:objectMap [ rmln:reference "speed" ; rmln:datatype xsd:integer ] ] .
<#Source> rmln:logicalSource [ rmln:source [ a rmln:RelativePathSource ; rmln:path "speeds.csv" ] ;
                       rmln:referenceFormulation rmln:CSV ] ;
  rmln:subjectMap [ rmln:quotedTriplesMap <#Speed> ] ;
  rmln:predicateObjectMap [ rmln:predicate ex:source ; rmln:objectMap [ rmln:reference "source" ] ] .
<#Claim> rmln:logicalSource [ rmln:source [ a rmln:RelativePathSource ; rmln:path "speeds.csv" ] ;
                       rmln:referenceFormulation rmln:CSV ] ;
  rmln:subjectMap [ rmln:template "http://ex.org/claim/{track}" ] ;
  rmln:predicateObjectMap [ rmln:predicate ex:about ;
    rmln:objectMap [ rmln:quotedTriplesMap <#Speed> ;
      rmln:joinCondition [ rmln:child "to" ; rmln:parent "from" ] ] ] .
"#;
    let out = lines(mapping, MemoryLoader::from([("speeds.csv", SPEEDS)]));
    let reifies: Vec<&String> = out.iter().filter(|l| l.contains("#reifies>")).collect();
    // One reifier per quoted triple, though T2's speed is quoted twice.
    assert_eq!(reifies.len(), 2, "{out:#?}");
    let t2 = reifies
        .iter()
        .find(|l| l.contains("<http://ex.org/T2>"))
        .unwrap();
    let r2 = t2.split(' ').next().unwrap();
    // The reifier is the first 128 bits of the SHA-256 of the triple's N-Triples form, the
    // same with any build (`printf '%s' '<http://ex.org/T2> … integer>' | sha256sum`).
    assert_eq!(r2, "_:b0eb4d6988589592f11f0a8972c362ac");
    assert!(t2.ends_with(
        "<<( <http://ex.org/T2> <http://ex.org/speed> \"80\"^^<http://www.w3.org/2001/XMLSchema#integer> )>> ."
    ));
    // T1's claim is about the speed of the track starting where T1 ends: T2's.
    assert!(out.contains(&format!(
        "<http://ex.org/claim/T1> <http://ex.org/about> {r2} ."
    )));
    assert!(
        out.iter()
            .any(|l| l.ends_with("<http://ex.org/source> \"survey\" ."))
    );
    // The non-asserted triples are not written.
    assert!(
        !out.iter()
            .any(|l| l.starts_with("<http://ex.org/T1> <http://ex.org/speed>"))
    );
    assert_eq!(out.len(), 5, "{out:#?}");
}

#[test]
fn quoted_triples_maps_may_not_quote_themselves() {
    let mapping = r#"
<#A> rmln:logicalSource [ rmln:source [ a rmln:RelativePathSource ; rmln:path "speeds.csv" ] ;
                       rmln:referenceFormulation rmln:CSV ] ;
  rmln:subjectMap [ rmln:quotedTriplesMap <#B> ] ;
  rmln:predicateObjectMap [ rmln:predicate ex:p ; rmln:object ex:o ] .
<#B> rmln:logicalSource [ rmln:source [ a rmln:RelativePathSource ; rmln:path "speeds.csv" ] ;
                       rmln:referenceFormulation rmln:CSV ] ;
  rmln:subjectMap [ rmln:quotedTriplesMap <#A> ] ;
  rmln:predicateObjectMap [ rmln:predicate ex:p ; rmln:object ex:o ] .
"#;
    let e = error(mapping, MemoryLoader::from([("speeds.csv", SPEEDS)]));
    assert!(e.to_string().contains("quote themselves"), "{e}");
}
