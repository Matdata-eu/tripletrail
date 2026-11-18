//! End-to-end tests of the vehicle routing functions (pgr_pickDeliver,
//! pgr_pickDeliverEuclidean, pgr_vrpOneDepot) on small instances worked out by hand in the
//! comments.

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::model::Term;
use tripletrail::oxigraph::sparql::QueryResults;

const PREFIXES: &str = r#"
PREFIX ex: <http://example.com/>
PREFIX pgr: <http://w3id.org/tripletrail/pgr#>
"#;

// Orders stored as RDF: nodes n0..n4 lie on a line, one unit apart.
const DATA: &str = r#"
@prefix ex: <http://example.com/> .
ex:o1 a ex:Order ; ex:demand 1 ; ex:from ex:n1 ; ex:to ex:n2 ; ex:pickupOpen 0 ; ex:service 1 .
ex:o2 a ex:Order ; ex:demand 1 ; ex:from ex:n3 ; ex:to ex:n4 ; ex:pickupOpen 5 ; ex:service 0 .
ex:v1 a ex:Truck ; ex:capacity 1 ; ex:depot ex:n0 .
ex:v2 a ex:Truck ; ex:capacity 1 ; ex:depot ex:n0 .
"#;

const COLUMNS: &str = "pgr:seq ?seq ; pgr:vehicle_seq ?vehicle_seq ; pgr:vehicle_id ?vehicle_id ;
    pgr:stop_seq ?stop_seq ; pgr:stop_type ?stop_type ; pgr:stop_id ?stop_id ;
    pgr:order_id ?order_id ; pgr:cargo ?cargo ; pgr:travel_time ?travel_time ;
    pgr:arrival_time ?arrival_time ; pgr:wait_time ?wait_time ;
    pgr:service_time ?service_time ; pgr:departure_time ?departure_time";

const SELECTED: &str = "?vehicle_seq ?vehicle_id ?stop_seq ?stop_type ?stop_id ?order_id ?cargo
    ?travel_time ?arrival_time ?wait_time ?service_time ?departure_time";

fn engine() -> Engine {
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    e
}

fn query(e: &Engine, q: &str) -> Result<Vec<Vec<String>>, String> {
    let QueryResults::Solutions(s) = e
        .query(&format!("{PREFIXES}{q}"))
        .map_err(|err| err.to_string())?
    else {
        panic!()
    };
    let vars = s.variables().to_vec();
    let mut out = Vec::new();
    for r in s {
        let r = r.map_err(|err| err.to_string())?;
        out.push(
            vars.iter()
                .map(|v| match r.get(v) {
                    Some(Term::NamedNode(n)) => n
                        .as_str()
                        .trim_start_matches("http://example.com/")
                        .to_owned(),
                    Some(Term::Literal(l)) => l.value().to_owned(),
                    Some(other) => other.to_string(),
                    None => "-".to_owned(),
                })
                .collect(),
        );
    }
    Ok(out)
}

fn rows(e: &Engine, q: &str) -> Vec<Vec<String>> {
    query(e, q).unwrap_or_else(|err| panic!("{err}\n{q}"))
}

fn strs(rows: &[Vec<String>]) -> Vec<Vec<&str>> {
    rows.iter()
        .map(|r| r.iter().map(String::as_str).collect())
        .collect()
}

#[test]
fn pick_deliver_over_a_cost_matrix() {
    let e = engine();
    // The matrix gives each pair one way only (|i - j|); it is used both ways.
    // With capacity 1, the orders cannot be carried together, but one truck does both:
    //   n0 -1-> n1 (o1 picked, 1 service) -1-> n2 (t 3, delivered) -1-> n3 (t 4, waits until
    //   5 to pick o2) -1-> n4 (t 6) -4-> n0 (t 10).
    // A second truck would add a route, and fewer trucks is better.
    let q = format!(
        r#"SELECT {SELECTED} WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
        [] pgr:function pgr:pgr_pickDeliver ; {COLUMNS} .
        {{ SELECT ?id ?demand ?p_node_id ?p_open ?p_close ?p_service
                  ?d_node_id ?d_open ?d_close WHERE {{
            ?id a ex:Order ; ex:demand ?demand ; ex:from ?p_node_id ; ex:to ?d_node_id ;
                ex:pickupOpen ?p_open ; ex:service ?p_service .
            BIND(100 AS ?p_close) BIND(0 AS ?d_open) BIND(100 AS ?d_close) }} }}
        {{ SELECT ?id ?capacity ?start_node_id ?start_open ?start_close WHERE {{
            ?id a ex:Truck ; ex:capacity ?capacity ; ex:depot ?start_node_id .
            BIND(0 AS ?start_open) BIND(100 AS ?start_close) }} }}
        {{ SELECT ?start_vid ?end_vid ?agg_cost WHERE {{
            VALUES ?i {{ 0 1 2 3 4 }} VALUES ?j {{ 0 1 2 3 4 }} FILTER(?i < ?j)
            BIND(IRI(CONCAT("http://example.com/n", STR(?i))) AS ?start_vid)
            BIND(IRI(CONCAT("http://example.com/n", STR(?j))) AS ?end_vid)
            BIND(?j - ?i AS ?agg_cost) }} }}
    }} }} ORDER BY ?seq"#
    );
    let r = rows(&e, &q);
    assert_eq!(
        strs(&r),
        [
            [
                "1", "v1", "1", "1", "n0", "-1", "0", "0", "0", "0", "0", "0"
            ],
            [
                "1", "v1", "2", "2", "n1", "o1", "1", "1", "1", "0", "1", "2"
            ],
            [
                "1", "v1", "3", "3", "n2", "o1", "0", "1", "3", "0", "0", "3"
            ],
            [
                "1", "v1", "4", "2", "n3", "o2", "1", "1", "4", "1", "0", "5"
            ],
            [
                "1", "v1", "5", "3", "n4", "o2", "0", "1", "6", "0", "0", "6"
            ],
            [
                "1", "v1", "6", "6", "n0", "-1", "0", "4", "10", "0", "0", "10"
            ],
            // The summary: no violations, then total travel, wait, service and duration.
            [
                "-2", "0", "0", "-1", "-1", "-1", "-1", "8", "-1", "1", "1", "10"
            ],
        ]
    );

    // A pair missing from the matrix cannot be travelled: without n3-n4, o2 is impossible.
    let q = q.replace("FILTER(?i < ?j)", "FILTER(?i < ?j && !(?i = 3 && ?j = 4))");
    let err = query(&e, &q).unwrap_err();
    assert!(err.contains("order(s) http://example.com/o2"), "{err}");
}

const EUCLIDEAN_ORDERS: &str = r#"
{ SELECT ?id ?demand ?p_x ?p_y ?p_open ?p_close ?d_x ?d_y ?d_open ?d_close WHERE {
    VALUES (?id ?demand ?p_x ?p_y ?p_open ?p_close ?d_x ?d_y ?d_open ?d_close) {
        (1 1 0 0 0 100 10 0 0 10)
        (2 1 0 0 0 100 -10 0 0 10) } } }"#;

#[test]
fn pick_deliver_euclidean_splits_by_time_windows() {
    let e = engine();
    // Both orders are picked up at the depot and must be delivered by time 10, 10 units east
    // and 10 units west: one truck cannot do both, so each of the two copies of truck 7
    // (?number 2) takes one and is back after 20.
    let q = format!(
        r#"SELECT {SELECTED} WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
        [] pgr:function pgr:pgr_pickDeliverEuclidean ; pgr:max_cycles 5 ; pgr:initial_sol 4 ;
           {COLUMNS} .
        {EUCLIDEAN_ORDERS}
        {{ SELECT ?id ?capacity ?number ?start_x ?start_y ?start_open ?start_close WHERE {{
            VALUES (?id ?capacity ?number ?start_x ?start_y ?start_open ?start_close) {{
                (7 5 2 0 0 0 100) }} }} }}
    }} }} ORDER BY ?seq"#
    );
    let r = rows(&e, &q);
    assert_eq!(
        strs(&r),
        [
            ["1", "7", "1", "1", "-", "-1", "0", "0", "0", "0", "0", "0"],
            ["1", "7", "2", "2", "-", "1", "1", "0", "0", "0", "0", "0"],
            [
                "1", "7", "3", "3", "-", "1", "0", "10", "10", "0", "0", "10"
            ],
            [
                "1", "7", "4", "6", "-", "-1", "0", "10", "20", "0", "0", "20"
            ],
            ["2", "7", "1", "1", "-", "-1", "0", "0", "0", "0", "0", "0"],
            ["2", "7", "2", "2", "-", "2", "1", "0", "0", "0", "0", "0"],
            [
                "2", "7", "3", "3", "-", "2", "0", "10", "10", "0", "0", "10"
            ],
            [
                "2", "7", "4", "6", "-", "-1", "0", "10", "20", "0", "0", "20"
            ],
            [
                "-2", "0", "0", "-1", "-1", "-1", "-1", "40", "-1", "0", "0", "40"
            ],
        ]
    );

    // pgr:factor multiplies travel times: at twice the time the deliveries are late.
    let slow = q.replace("pgr:max_cycles 5", "pgr:factor 2");
    let err = query(&e, &slow).unwrap_err();
    assert!(err.contains("no vehicle can serve order(s) 1, 2"), "{err}");
    // A single truck cannot serve both either.
    let one = q.replace("(7 5 2 0 0 0 100)", "(7 5 1 0 0 0 100)");
    let err = query(&e, &one).unwrap_err();
    assert!(err.contains("order(s) 2"), "{err}");
    // Wrong parameters are refused.
    let err = query(&e, &q.replace("pgr:initial_sol 4", "pgr:initial_sol 9")).unwrap_err();
    assert!(err.contains("initial_sol"), "{err}");
}

#[test]
fn vrp_one_depot() {
    let e = engine();
    // Depot 1 at (0 0); customers 2 at (3 0) and 3 at (3 4), 5 units each, 1 to unload,
    // and 3 opens at 6. Truck 9 carries 10: from the depot to 2 (arrives 3, leaves 4), to 3
    // (4 further: 8, leaves 9) and back (5: 14). Visiting 3 first would wait until 6 and end
    // at 15.
    let orders = r#"{ SELECT ?id ?x ?y ?order_unit ?open_time ?close_time ?service_time WHERE {
        VALUES (?id ?x ?y ?order_unit ?open_time ?close_time ?service_time) {
            (1 0 0 0 0 100 0) (2 3 0 5 0 100 1) (3 3 4 5 6 100 1) } } }"#;
    let vehicles = r#"{ SELECT ?vehicle_id ?capacity ?case_no WHERE {
        VALUES (?vehicle_id ?capacity ?case_no) { (9 10 1) } } }"#;
    let matrix = r#"{ SELECT ?src_id ?dest_id ?cost ?distance ?traveltime WHERE {
        VALUES (?src_id ?dest_id ?traveltime) { (1 2 3) (2 3 4) (1 3 5) }
        BIND(?traveltime AS ?cost) BIND(?traveltime AS ?distance) } }"#;
    for cost in [matrix, ""] {
        // Without a cost matrix, the distances between the coordinates are the same.
        let q = format!(
            r#"SELECT ?oid ?opos ?vid ?tarrival ?tdepart ?stop_type ?stop_id WHERE {{
            SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function pgr:pgr_vrpOneDepot ; pgr:depot_id 1 ;
               pgr:oid ?oid ; pgr:opos ?opos ; pgr:vid ?vid ; pgr:tarrival ?tarrival ;
               pgr:tdepart ?tdepart ; pgr:stop_type ?stop_type ; pgr:stop_id ?stop_id ;
               pgr:seq ?seq .
            {orders} {vehicles} {cost}
        }} }} ORDER BY ?seq"#
        );
        let r = rows(&e, &q);
        // Start, the two loads at the depot, two deliveries, end, summary.
        assert_eq!(r.len(), 7, "{r:?}");
        assert_eq!(r[0], ["-1", "1", "9", "0", "0", "1", "1"]);
        assert!(
            r[1..3]
                .iter()
                .all(|s| s[5] == "2" && s[6] == "1" && s[4] == "0")
        );
        assert_eq!(
            strs(&r[3..]),
            [
                ["2", "4", "9", "3", "4", "3", "2"],
                ["3", "5", "9", "8", "9", "3", "3"],
                ["-1", "6", "9", "14", "14", "6", "1"],
                ["-1", "0", "0", "-1", "14", "-1", "-1"],
            ]
        );
    }
    // pgr:depot_id must be one of the orders.
    let q = format!(
        r#"SELECT ?oid WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function pgr:pgr_vrpOneDepot ; pgr:depot_id 5 ; pgr:oid ?oid .
            {orders} {vehicles} }} }}"#
    );
    assert!(query(&e, &q).unwrap_err().contains("depot_id"));
}
