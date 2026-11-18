//! `table=` and `sql=` sources, on a database that answers from memory.

use oxigraph::model::vocab::xsd;
use oxigraph::model::{Graph, Literal};
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use std::sync::{Arc, Mutex};
use tripletrail_fx::{DatabaseQuery, Databases, FxConfig, FxError, FxService, Rows, Select};

/// A call: the connection, the query, the columns wanted and the row cap.
type Call = (Option<String>, DatabaseQuery, Vec<&'static str>, usize);

/// Two stations, and the calls it was given.
#[derive(Debug, Default)]
struct Stations {
    calls: Mutex<Vec<Call>>,
    /// The conditions of the last call.
    conditions: Mutex<Vec<String>>,
    /// The bind-join values of the last call.
    one_of: Mutex<Vec<String>>,
}

impl Databases for Stations {
    fn rows(
        &self,
        name: Option<&str>,
        query: &DatabaseQuery,
        select: &Select<'_>,
        max: usize,
    ) -> Result<Rows, FxError> {
        let wanted = ["code", "km", "x", "y"]
            .into_iter()
            .filter(|c| (select.wanted)(c))
            .collect();
        self.calls
            .lock()
            .unwrap()
            .push((name.map(str::to_owned), query.clone(), wanted, max));
        *self.conditions.lock().unwrap() = select
            .conditions
            .iter()
            .map(|c| format!("{} {} {}", c.key, c.op.sql(), c.value))
            .collect();
        *self.one_of.lock().unwrap() = select
            .one_of
            .iter()
            .map(|o| {
                let values: Vec<String> = o.values.iter().map(ToString::to_string).collect();
                format!("{} in {}", o.key, values.join(" "))
            })
            .collect();
        Ok(Rows {
            columns: vec!["code".into(), "km".into(), "x".into(), "y".into()],
            rows: vec![
                vec![
                    Some(Literal::new_simple_literal("OSL")),
                    Some(Literal::new_typed_literal("0.0", xsd::DECIMAL)),
                    Some(Literal::new_typed_literal("1.075E1", xsd::DOUBLE)),
                    Some(Literal::new_typed_literal("5.991E1", xsd::DOUBLE)),
                ],
                vec![Some(Literal::new_simple_literal("LLS")), None, None, None],
            ],
            ..Rows::default()
        })
    }
}

/// A CONSTRUCT of everything `SERVICE <service>` gives, with `options` as `fxe:` properties.
fn construct(
    databases: Option<Arc<Stations>>,
    service: &str,
    options: &[(&str, &str)],
) -> Result<Graph, String> {
    let fx = FxService::new(
        FxConfig {
            fixed_limits: false,
            databases: databases.map(|d| d as Arc<dyn Databases>),
            ..FxConfig::default()
        },
        SparqlEvaluator::new,
    );
    let empty = Box::leak(Box::new(oxigraph::model::Dataset::new()));
    let properties: String = options
        .iter()
        .map(|(k, v)| format!("<http://sparql.xyz/facade-x/engine/properties> <http://sparql.xyz/facade-x/engine/{k}> {:?} . ", v))
        .collect();
    let query = format!(
        "CONSTRUCT {{ ?s ?p ?o }} WHERE {{ SERVICE <{service}> {{ {properties} ?s ?p ?o }} }}"
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

fn ntriples(graph: &Graph) -> Vec<String> {
    let mut lines: Vec<String> = graph.iter().map(|t| format!("{t} .")).collect();
    lines.sort();
    lines
}

#[test]
fn rows_are_containers_with_typed_values() {
    let db = Arc::new(Stations::default());
    let graph = construct(
        Some(db.clone()),
        "x-sparql-anything:table=rail.station,database=rail,blank-nodes=false,root=http://ex.org/s,geo.x=x,geo.y=y",
        &[],
    )
    .unwrap();
    let x = "http://sparql.xyz/facade-x/data/";
    let rdf = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
    let xsd = "http://www.w3.org/2001/XMLSchema#";
    let mut expected = vec![
        format!("<http://ex.org/s> <{rdf}type> <http://sparql.xyz/facade-x/ns/Root> ."),
        format!("<http://ex.org/s> <{rdf}_1> <http://ex.org/s#row1> ."),
        format!("<http://ex.org/s> <{rdf}_2> <http://ex.org/s#row2> ."),
        format!("<http://ex.org/s#row1> <{x}code> \"OSL\" ."),
        format!("<http://ex.org/s#row1> <{x}km> \"0.0\"^^<{xsd}decimal> ."),
        format!("<http://ex.org/s#row1> <{x}x> \"1.075E1\"^^<{xsd}double> ."),
        format!("<http://ex.org/s#row1> <{x}y> \"5.991E1\"^^<{xsd}double> ."),
        "<http://ex.org/s#row1> <http://www.opengis.net/ont/geosparql#asWKT> \"POINT(10.75 59.91)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral> .".to_owned(),
        format!("<http://ex.org/s#row2> <{x}code> \"LLS\" ."),
    ];
    expected.sort();
    assert_eq!(ntriples(&graph), expected);
    assert_eq!(
        db.calls.lock().unwrap().as_slice(),
        [(
            Some("rail".to_owned()),
            DatabaseQuery::Table("rail.station".into()),
            vec!["code", "km", "x", "y"],
            1_000_000
        )]
    );
}

#[test]
fn queries_are_trimmed_and_capped() {
    let db = Arc::new(Stations::default());
    construct(
        Some(db.clone()),
        "x-sparql-anything:database.max-rows=10",
        &[("sql", " SELECT code, km FROM station; ")],
    )
    .unwrap();
    assert_eq!(
        db.calls.lock().unwrap().as_slice(),
        [(
            None,
            DatabaseQuery::Sql("SELECT code, km FROM station".into()),
            vec!["code", "km", "x", "y"],
            10
        )]
    );
}

#[test]
fn bad_sources_are_refused() {
    let db = Arc::new(Stations::default());
    let e = construct(None, "x-sparql-anything:table=station", &[]).unwrap_err();
    assert!(e.contains("no database is configured"), "{e}");
    for (service, error) in [
        ("x-sparql-anything:table=a,sql=1", "not both"),
        ("x-sparql-anything:table=a;drop", "not a table name"),
        ("x-sparql-anything:table=a,location=b.csv", "not both"),
        ("x-sparql-anything:sql=;", "sql is empty"),
        (
            "x-sparql-anything:table=a,database.max-rows=0",
            "positive number",
        ),
        ("x-sparql-anything:table=a,geo.wkt=nope", "no such column"),
    ] {
        let e = construct(Some(db.clone()), service, &[]).unwrap_err();
        assert!(e.contains(error), "{service}: {e}");
    }
}

#[test]
fn only_the_columns_the_pattern_matches_are_asked_for() {
    let db = Arc::new(Stations::default());
    let fx = FxService::new(
        FxConfig {
            fixed_limits: false,
            databases: Some(db.clone() as Arc<dyn Databases>),
            ..FxConfig::default()
        },
        SparqlEvaluator::new,
    );
    let empty = Box::leak(Box::new(oxigraph::model::Dataset::new()));
    let query = "SELECT ?c WHERE { SERVICE <x-sparql-anything:table=station> { \
                 ?s <http://sparql.xyz/facade-x/data/code> ?c } }";
    let QueryResults::Solutions(solutions) = fx
        .register(SparqlEvaluator::new())
        .parse_query(query)
        .unwrap()
        .on_queryable_dataset(&*empty)
        .execute()
        .unwrap()
    else {
        panic!("not a SELECT")
    };
    assert_eq!(solutions.count(), 2);
    assert_eq!(db.calls.lock().unwrap()[0].2, ["code"]);
}

#[test]
fn conditions_go_to_the_database() {
    let db = Arc::new(Stations::default());
    let fx = FxService::new(
        FxConfig {
            fixed_limits: false,
            databases: Some(db.clone() as Arc<dyn Databases>),
            ..FxConfig::default()
        },
        SparqlEvaluator::new,
    );
    let empty = Box::leak(Box::new(oxigraph::model::Dataset::new()));
    let run = |options: &str, filter: &str| {
        let query = format!(
            "PREFIX xyz: <http://sparql.xyz/facade-x/data/> SELECT ?c WHERE {{ \
             SERVICE <x-sparql-anything:table=station{options}> {{ \
             ?s xyz:code ?c ; xyz:km ?km {filter} }} }}"
        );
        let QueryResults::Solutions(solutions) = fx
            .register(SparqlEvaluator::new())
            .parse_query(&query)
            .unwrap()
            .on_queryable_dataset(&*empty)
            .execute()
            .unwrap()
        else {
            panic!("not a SELECT")
        };
        // The pattern still checks them: the fake database ignores them.
        let codes: Vec<String> = solutions
            .map(|s| s.unwrap().get("c").unwrap().to_string())
            .collect();
        (codes, db.conditions.lock().unwrap().clone())
    };
    let (codes, conditions) = run("", "FILTER(?km >= 0 && ?c = 'OSL')");
    assert_eq!(codes, [r#""OSL""#]);
    let mut conditions = conditions;
    conditions.sort();
    assert_eq!(
        conditions,
        [
            r#"http://sparql.xyz/facade-x/data/code = "OSL""#,
            r#"http://sparql.xyz/facade-x/data/km >= "0"^^<http://www.w3.org/2001/XMLSchema#integer>"#,
        ]
    );
    // Numbered rows, dropped or trimmed values, or an OPTIONAL: no conditions.
    for options in [",blank-nodes=false", ",null-string=", ",trim-strings=true"] {
        let (codes, conditions) = run(options, "FILTER(?km >= 0)");
        assert_eq!(codes, [r#""OSL""#], "{options}");
        assert!(conditions.is_empty(), "{options}");
    }
    let (_, conditions) = run("", "OPTIONAL { ?s xyz:x ?x } FILTER(?km >= 0)");
    assert!(conditions.is_empty());
    // Simple-feature relations of a value and a constant, either way round. This evaluator
    // has no GeoSPARQL functions, so the FILTER itself fails once the rows are read.
    let query = "PREFIX xyz: <http://sparql.xyz/facade-x/data/> SELECT ?c WHERE { \
                 SERVICE <x-sparql-anything:table=station> { ?s xyz:code ?c ; xyz:km ?km \
                 FILTER(<http://www.opengis.net/def/function/geosparql/sfWithin>(\
                 \"POINT(1 2)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>, ?km)) } }";
    let _ = fx
        .register(SparqlEvaluator::new())
        .parse_query(query)
        .unwrap()
        .on_queryable_dataset(&*empty)
        .execute()
        .map(|r| {
            if let QueryResults::Solutions(s) = r {
                s.for_each(drop)
            }
        });
    assert_eq!(
        db.conditions.lock().unwrap().as_slice(),
        [
            r#"http://sparql.xyz/facade-x/data/km && "POINT(1 2)"^^<http://www.opengis.net/ont/geosparql#wktLiteral>"#
        ]
    );
}

#[test]
fn values_in_the_pattern_go_to_the_database() {
    let db = Arc::new(Stations::default());
    let fx = FxService::new(
        FxConfig {
            fixed_limits: false,
            databases: Some(db.clone() as Arc<dyn Databases>),
            ..FxConfig::default()
        },
        SparqlEvaluator::new,
    );
    let empty = Box::leak(Box::new(oxigraph::model::Dataset::new()));
    let run = |values: &str| {
        let query = format!(
            "PREFIX xyz: <http://sparql.xyz/facade-x/data/> SELECT ?c WHERE {{ \
             SERVICE <x-sparql-anything:table=station> {{ VALUES {values} \
             ?s xyz:code ?c ; xyz:km ?km }} }}"
        );
        let QueryResults::Solutions(solutions) = fx
            .register(SparqlEvaluator::new())
            .parse_query(&query)
            .unwrap()
            .on_queryable_dataset(&*empty)
            .execute()
            .unwrap()
        else {
            panic!("not a SELECT")
        };
        let codes: Vec<String> = solutions
            .map(|s| s.unwrap().get("c").unwrap().to_string())
            .collect();
        (codes, db.one_of.lock().unwrap().clone())
    };
    // Each VALUES row joins: OSL twice. The database gets each value once.
    let (codes, one_of) = run(r#"?c { "OSL" "BRG" "OSL" }"#);
    assert_eq!(codes, [r#""OSL""#, r#""OSL""#]);
    assert_eq!(
        one_of,
        [r#"http://sparql.xyz/facade-x/data/code in "OSL" "BRG""#]
    );
    // An IRI joins no value: it is left out.
    let (codes, one_of) = run(r#"?c { "OSL" <http://ex.org/x> }"#);
    assert_eq!(codes, [r#""OSL""#]);
    assert_eq!(one_of, [r#"http://sparql.xyz/facade-x/data/code in "OSL""#]);
    // An UNDEF joins any value: none are passed. It joins every row with a km: OSL.
    let (codes, one_of) = run(r#"?c { "OSL" UNDEF }"#);
    assert_eq!(codes, [r#""OSL""#, r#""OSL""#]);
    assert!(one_of.is_empty());
}
