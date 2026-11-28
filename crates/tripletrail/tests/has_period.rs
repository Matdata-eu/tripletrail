//! `?x tt:hasPeriod ?p` (#301): the period of an OWL-Time instant or interval, a DCAT period
//! of time or a schema.org event, however it is written; the same under both executors.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

const PREFIXES: &str = "PREFIX ex: <http://example.com/>
PREFIX tt: <http://w3id.org/tripletrail/fn#>
PREFIX time: <http://www.w3.org/2006/time#>
";

const DATA: &str = r#"
@prefix ex: <http://example.com/> .
@prefix time: <http://www.w3.org/2006/time#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
@prefix dcat: <http://www.w3.org/ns/dcat#> .
@prefix dct: <http://purl.org/dc/terms/> .
@prefix schema: <https://schema.org/> .

# Instants, each way of writing one.
ex:stamp a time:Instant ; time:inXSDDateTimeStamp "2022-12-11T10:30:00Z"^^xsd:dateTimeStamp .
ex:datetime time:inXSDDateTime "2022-12-11T10:30:00+01:00"^^xsd:dateTime .
ex:date time:inXSDDate "2022-12-11"^^xsd:date .
ex:month time:inXSDgYearMonth "2022-12"^^xsd:gYearMonth .
ex:year time:inXSDgYear "2022"^^xsd:gYear .
ex:described time:inDateTime [ a time:DateTimeDescription ; time:year "2022"^^xsd:gYear ;
    time:month "--12"^^xsd:gMonth ; time:day "---11"^^xsd:gDay ; time:unitType time:unitDay ] .
ex:unix time:inTimePosition [ time:hasTRS <http://dbpedia.org/resource/Unix_time> ;
    time:numericPosition 1670754600.0 ] .
ex:nominal time:inTimePosition [ time:nominalPosition "2022-12-11" ] .

# Two writings of one instant: the date and the stamp agree, so the stamp.
ex:agree time:inXSDDate "2022-12-11"^^xsd:date ;
    time:inXSDDateTimeStamp "2022-12-11T00:00:00Z"^^xsd:dateTimeStamp .
# They don't: the more precise one.
ex:disagree time:inXSDDate "2022-12-12"^^xsd:date ;
    time:inXSDDateTimeStamp "2022-12-11T00:00:00Z"^^xsd:dateTimeStamp .

# Intervals.
ex:validity a time:Interval ;
    time:hasBeginning [ time:inXSDDate "2022-12-11"^^xsd:date ] ;
    time:hasEnd [ time:inXSDDate "2078-12-31"^^xsd:date ] .
ex:openEnd time:hasBeginning [ time:inXSDDateTimeStamp "2024-01-01T00:00:00Z"^^xsd:dateTimeStamp ] .
ex:openStart time:hasEnd [ time:inXSDDateTimeStamp "2024-01-01T00:00:00Z"^^xsd:dateTimeStamp ] .
ex:backwards time:hasBeginning [ time:inXSDDate "2025-01-01"^^xsd:date ] ;
    time:hasEnd [ time:inXSDDate "2024-01-01"^^xsd:date ] .
ex:lasting time:hasBeginning [ time:inXSDDate "2024-01-31"^^xsd:date ] ;
    time:hasXSDDuration "P1M"^^xsd:duration .
ex:twoBeginnings time:hasBeginning [ time:inXSDgYear "2020"^^xsd:gYear ] ,
        [ time:inXSDgYear "2019"^^xsd:gYear ] ;
    time:hasEnd [ time:inXSDgYear "2021"^^xsd:gYear ] .

# Through time:hasTime, one period for each.
ex:event time:hasTime ex:stamp , ex:validity .

# DCAT and schema.org.
ex:dataset dct:temporal [ a dct:PeriodOfTime ; dcat:startDate "2020-01-01"^^xsd:date ;
    dcat:endDate "2020-12-31"^^xsd:date ] .
ex:fosdem a schema:Event ; schema:startDate "2027-01-30"^^xsd:date ;
    schema:endDate "2027-01-31"^^xsd:date .

# OWL-Time 2017, section 5.2's meeting: a description to the minute (its time zone is not
# read: values without an offset are read at the dataset's).
ex:meetingStart a time:Instant ; time:inDateTime [ a time:DateTimeDescription ;
    time:unitType time:unitMinute ; time:minute 30 ; time:hour 10 ; time:day "---01"^^xsd:gDay ;
    time:month "--01"^^xsd:gMonth ; time:year "2006"^^xsd:gYear ] .
ex:meeting a time:ProperInterval ; time:hasBeginning ex:meetingStart ;
    time:hasXSDDuration "PT1H30M"^^xsd:duration .

# A track whose validity is an interval (RINF's era:validity).
ex:track ex:validity ex:validity .
ex:nothing ex:label "no period" .
"#;

const EXECUTORS: [&str; 2] = [
    "#pragma tt.executor ids\n",
    "#pragma tt.executor spareval\n",
];

fn engine() -> Engine {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    e
}

/// The rows of `?x` and `?p` (lexical form of the period), sorted, the same under both
/// executors.
fn rows(e: &Engine, query: &str) -> Vec<String> {
    let run = |pragma: &str| -> Vec<String> {
        let QueryResults::Solutions(s) = e.query(&format!("{pragma}{PREFIXES}{query}")).unwrap()
        else {
            panic!("SELECT")
        };
        let mut out: Vec<String> = s
            .map(|r| {
                let r = r.unwrap();
                let x = r
                    .get("x")
                    .map(|t| {
                        t.to_string()
                            .replace("http://example.com/", "")
                            .trim_matches(['<', '>'])
                            .to_owned()
                    })
                    .unwrap_or_default();
                let p = match r.get("p") {
                    Some(oxigraph::model::Term::Literal(l)) => l.value().to_owned(),
                    Some(t) => t.to_string(),
                    None => String::new(),
                };
                format!("{x} {p}")
            })
            .collect();
        out.sort();
        out
    };
    let ids = run(EXECUTORS[0]);
    assert_eq!(ids, run(EXECUTORS[1]), "{query}");
    ids
}

fn period(e: &Engine, x: &str) -> Vec<String> {
    rows(
        e,
        &format!("SELECT ?x ?p WHERE {{ BIND(ex:{x} AS ?x) ex:{x} tt:hasPeriod ?p }}"),
    )
    .into_iter()
    .map(|r| r.split_once(' ').unwrap().1.to_owned())
    .collect()
}

#[test]
fn each_writing_of_an_instant() {
    let e = engine();
    let instant = "2022-12-11T10:30:00Z/2022-12-11T10:30:00.001Z";
    let day = "2022-12-11T00:00:00Z/2022-12-12T00:00:00Z";
    assert_eq!(period(&e, "stamp"), [instant]);
    assert_eq!(
        period(&e, "datetime"),
        ["2022-12-11T09:30:00Z/2022-12-11T09:30:00.001Z"]
    );
    assert_eq!(period(&e, "date"), [day]);
    assert_eq!(
        period(&e, "month"),
        ["2022-12-01T00:00:00Z/2023-01-01T00:00:00Z"]
    );
    assert_eq!(
        period(&e, "year"),
        ["2022-01-01T00:00:00Z/2023-01-01T00:00:00Z"]
    );
    assert_eq!(period(&e, "described"), [day]);
    assert_eq!(period(&e, "unix"), [instant]);
    assert_eq!(period(&e, "nominal"), [day]);
}

#[test]
fn writings_that_agree_or_not() {
    let e = engine();
    let stamp = "2022-12-11T00:00:00Z/2022-12-11T00:00:00.001Z";
    assert_eq!(period(&e, "agree"), [stamp]);
    assert_eq!(period(&e, "disagree"), [stamp]);
}

#[test]
fn intervals() {
    let e = engine();
    assert_eq!(
        period(&e, "validity"),
        ["2022-12-11T00:00:00Z/2079-01-01T00:00:00Z"]
    );
    assert_eq!(period(&e, "openEnd"), ["2024-01-01T00:00:00Z/.."]);
    assert_eq!(period(&e, "openStart"), ["../2024-01-01T00:00:00Z"]);
    assert!(period(&e, "backwards").is_empty());
    // 31 January plus a month is the end of February.
    assert_eq!(
        period(&e, "lasting"),
        ["2024-01-31T00:00:00Z/2024-02-29T00:00:00Z"]
    );
    assert_eq!(
        period(&e, "twoBeginnings"),
        ["2019-01-01T00:00:00Z/2022-01-01T00:00:00Z"]
    );
    assert!(period(&e, "nothing").is_empty());
}

#[test]
fn the_owl_time_meeting() {
    let e = engine();
    assert_eq!(
        period(&e, "meetingStart"),
        ["2006-01-01T10:30:00Z/2006-01-01T10:31:00Z"]
    );
    assert_eq!(
        period(&e, "meeting"),
        ["2006-01-01T10:30:00Z/2006-01-01T12:00:00Z"]
    );
}

#[test]
fn has_time_gives_one_period_each() {
    let e = engine();
    assert_eq!(
        period(&e, "event"),
        [
            "2022-12-11T00:00:00Z/2079-01-01T00:00:00Z",
            "2022-12-11T10:30:00Z/2022-12-11T10:30:00.001Z"
        ]
    );
}

#[test]
fn dcat_and_schema_org_periods() {
    let e = engine();
    assert_eq!(
        rows(
            &e,
            "PREFIX dct: <http://purl.org/dc/terms/>
            SELECT ?x ?p WHERE { ?x dct:temporal/tt:hasPeriod ?p }"
        ),
        ["dataset 2020-01-01T00:00:00Z/2021-01-01T00:00:00Z"]
    );
    assert_eq!(
        period(&e, "fosdem"),
        ["2027-01-30T00:00:00Z/2027-02-01T00:00:00Z"]
    );
}

#[test]
fn the_last_step_of_a_path_and_with_other_patterns() {
    let e = engine();
    assert_eq!(
        rows(&e, "SELECT ?x ?p WHERE { ?x ex:validity/tt:hasPeriod ?p }"),
        ["track 2022-12-11T00:00:00Z/2079-01-01T00:00:00Z"]
    );
    // With the period's functions.
    assert_eq!(
        rows(
            &e,
            "SELECT ?x ?p WHERE { ?x ex:validity ?v . ?v tt:hasPeriod ?q
              FILTER(tt:periodContains(?q, tt:period(\"2030-06-01\")))
              BIND(STR(tt:periodStart(?q)) AS ?p) }"
        ),
        ["track 2022-12-11T00:00:00Z"]
    );
    // Every entity with a period, the subject unbound.
    let all = rows(
        &e,
        "SELECT ?x ?p WHERE { ?x tt:hasPeriod ?p FILTER(isIRI(?x)) }",
    );
    assert!(all.iter().any(|r| r.starts_with("stamp ")), "{all:?}");
    assert!(all.iter().any(|r| r.starts_with("fosdem ")), "{all:?}");
    assert!(!all.iter().any(|r| r.starts_with("backwards ")), "{all:?}");
    assert!(!all.iter().any(|r| r.starts_with("track ")), "{all:?}");
}
