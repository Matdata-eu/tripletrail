//! pgr_pickDeliver, pgr_pickDeliverEuclidean and pgr_vrpOneDepot: the rows of the orders,
//! vehicles and cost matrix sub-selects turned into the routing core's [`Order`]s and
//! [`Vehicle`]s, and its plan into pgRouting's result rows.

use crate::network::number;
use crate::request::{PgrFunction, Request};
use crate::service::PgrError;
use crate::tables::{Row, list_item, ranks};
use oxigraph::model::{Literal, Term};
use oxigraph::sparql::Variable;
use std::collections::HashMap;
use tripletrail_routing::{Order, Plan, Stop, StopKind, Vehicle, pick_deliver};

/// The result columns of pgr_pickDeliver and pgr_pickDeliverEuclidean.
pub(crate) const PICK_DELIVER_COLUMNS: &[&str] = &[
    "seq",
    "vehicle_seq",
    "vehicle_id",
    "stop_seq",
    "stop_type",
    "stop_id",
    "order_id",
    "cargo",
    "travel_time",
    "arrival_time",
    "wait_time",
    "service_time",
    "departure_time",
];

/// pgr_vrpOneDepot's columns (`oid`, `opos`, `vid`, `tarrival`, `tdepart`), then those of
/// pgr_pickDeliver.
pub(crate) const ONE_DEPOT_COLUMNS: &[&str] = &[
    "oid",
    "opos",
    "vid",
    "tarrival",
    "tdepart",
    "seq",
    "vehicle_seq",
    "vehicle_id",
    "stop_seq",
    "stop_type",
    "stop_id",
    "order_id",
    "cargo",
    "travel_time",
    "arrival_time",
    "wait_time",
    "service_time",
    "departure_time",
];

/// The variables and rows of an evaluated sub-select.
pub(crate) type Table = (Vec<Variable>, Vec<Vec<Option<Term>>>);

/// Column access on a sub-select's rows, with pgRouting's names in errors.
struct Columns<'a> {
    what: &'a str,
    variables: &'a [Variable],
}

impl Columns<'_> {
    fn index(&self, name: &str) -> Option<usize> {
        self.variables.iter().position(|v| v.as_str() == name)
    }

    fn term(&self, row: &[Option<Term>], name: &str) -> Result<Option<Term>, PgrError> {
        Ok(self.index(name).and_then(|i| row[i].clone()))
    }

    fn required_term(&self, row: &[Option<Term>], name: &str) -> Result<Term, PgrError> {
        self.term(row, name)?.ok_or_else(|| {
            PgrError::Request(format!("the {} sub-select must bind ?{name}", self.what))
        })
    }

    /// A numeric column: `Ok(None)` when it is not projected or unbound.
    fn number(&self, row: &[Option<Term>], name: &str) -> Result<Option<f64>, PgrError> {
        match self.term(row, name)? {
            None => Ok(None),
            Some(t) => number(&t)
                .filter(|v| v.is_finite())
                .map(Some)
                .ok_or_else(|| {
                    PgrError::Request(format!(
                        "?{name} of the {} sub-select must be numeric, not {t}",
                        self.what
                    ))
                }),
        }
    }

    fn required(&self, row: &[Option<Term>], name: &str) -> Result<f64, PgrError> {
        self.number(row, name)?.ok_or_else(|| {
            PgrError::Request(format!("the {} sub-select must bind ?{name}", self.what))
        })
    }
}

/// Where stops are and how long travel between them takes.
#[derive(Default)]
struct Locations {
    /// What to report as `stop_id`, by location index: the node of the cost matrix.
    labels: Vec<Option<Term>>,
    index: HashMap<Term, usize>,
    /// Coordinates by location index (Euclidean travel), or empty.
    points: Vec<(f64, f64)>,
    /// Directed costs between nodes, `n * n`.
    matrix: Vec<f64>,
}

impl Locations {
    fn node(&mut self, t: Term) -> usize {
        let n = self.labels.len();
        *self.index.entry(t.clone()).or_insert_with(|| {
            self.labels.push(Some(t));
            n
        })
    }

    fn point(&mut self, x: f64, y: f64, label: Option<Term>) -> usize {
        self.points.push((x, y));
        self.labels.push(label);
        self.points.len() - 1
    }

    /// Fills the cost matrix from `?start_vid ?end_vid ?agg_cost` (or pgr_vrpOneDepot's
    /// `?src_id ?dest_id ?traveltime|?cost`) rows. A pair given in one direction only is used
    /// both ways; a pair not given at all cannot be travelled.
    fn fill_matrix(&mut self, (variables, rows): &Table) -> Result<(), PgrError> {
        let c = Columns {
            what: "cost matrix",
            variables,
        };
        let (from, to, cost) = if c.index("start_vid").is_some() {
            ("start_vid", "end_vid", "agg_cost")
        } else if c.index("traveltime").is_some() {
            ("src_id", "dest_id", "traveltime")
        } else {
            ("src_id", "dest_id", "cost")
        };
        let n = self.labels.len();
        let mut given = vec![f64::INFINITY; n * n];
        for row in rows {
            let (Some(a), Some(b), Some(v)) =
                (c.term(row, from)?, c.term(row, to)?, c.number(row, cost)?)
            else {
                continue;
            };
            // Nodes no stop uses do not matter.
            let (Some(&a), Some(&b)) = (self.index.get(&a), self.index.get(&b)) else {
                continue;
            };
            if v < 0.0 {
                return Err(PgrError::Request(format!(
                    "?{cost} of the cost matrix must not be negative"
                )));
            }
            given[a * n + b] = given[a * n + b].min(v);
        }
        self.matrix = given.clone();
        for a in 0..n {
            for b in 0..n {
                let i = a * n + b;
                if a == b {
                    self.matrix[i] = 0.0;
                } else if given[i].is_infinite() {
                    self.matrix[i] = given[b * n + a];
                }
            }
        }
        Ok(())
    }

    fn travel(&self, a: usize, b: usize) -> f64 {
        if self.points.is_empty() {
            self.matrix[a * self.labels.len() + b]
        } else {
            let (p, q) = (self.points[a], self.points[b]);
            (p.0 - q.0).hypot(p.1 - q.1)
        }
    }
}

/// A stop's place: a node of the cost matrix, or coordinates.
fn place(
    c: &Columns,
    row: &[Option<Term>],
    locations: &mut Locations,
    euclidean: bool,
    node: &str,
    x: &str,
    y: &str,
) -> Result<usize, PgrError> {
    Ok(if euclidean {
        locations.point(c.required(row, x)?, c.required(row, y)?, None)
    } else {
        locations.node(c.required_term(row, node)?)
    })
}

fn window(
    c: &Columns,
    location: usize,
    open: f64,
    close: f64,
    service: Option<f64>,
    what: &str,
) -> Result<Stop, PgrError> {
    let service = service.unwrap_or(0.0);
    if close < open || service < 0.0 {
        return Err(PgrError::Request(format!(
            "the {} sub-select has a {what} window closing before it opens, or a negative service time",
            c.what
        )));
    }
    Ok(Stop {
        location,
        open,
        close,
        service,
    })
}

/// A row and its id.
type Keyed<'a> = (Term, &'a [Option<Term>]);

/// Rows sorted in pgRouting's id order on column `id`, so that ties do not depend on the
/// order the store returns them in.
fn by_id<'a>(
    c: &Columns,
    rows: &'a [Vec<Option<Term>>],
    id: &str,
) -> Result<Vec<Keyed<'a>>, PgrError> {
    let ids = rows
        .iter()
        .map(|r| c.required_term(r, id))
        .collect::<Result<Vec<_>, _>>()?;
    let mut out: Vec<Keyed> = Vec::with_capacity(rows.len());
    for i in ranks(&ids) {
        out.push((ids[i as usize].clone(), &rows[i as usize]));
    }
    if out.windows(2).any(|w| w[0].0 == w[1].0) {
        return Err(PgrError::Request(format!(
            "the {} sub-select has the same ?{id} twice",
            c.what
        )));
    }
    Ok(out)
}

/// What the solver needs, and the terms to report.
struct Instance {
    orders: Vec<Order>,
    order_ids: Vec<Term>,
    vehicles: Vec<Vehicle>,
    vehicle_ids: Vec<Term>,
    locations: Locations,
}

fn pick_deliver_instance(
    request: &Request,
    orders: &Table,
    vehicles: &Table,
) -> Result<Instance, PgrError> {
    let euclidean = request.function == PgrFunction::PickDeliverEuclidean;
    let mut locations = Locations::default();
    let c = Columns {
        what: "orders",
        variables: &orders.0,
    };
    let mut out_orders = Vec::new();
    let mut order_ids = Vec::new();
    for (id, row) in by_id(&c, &orders.1, "id")? {
        let demand = c.required(row, "demand")?;
        if demand < 0.0 {
            return Err(PgrError::Request(format!(
                "order {id} has a negative ?demand"
            )));
        }
        let p = place(
            &c,
            row,
            &mut locations,
            euclidean,
            "p_node_id",
            "p_x",
            "p_y",
        )?;
        let d = place(
            &c,
            row,
            &mut locations,
            euclidean,
            "d_node_id",
            "d_x",
            "d_y",
        )?;
        let pickup = window(
            &c,
            p,
            c.required(row, "p_open")?,
            c.required(row, "p_close")?,
            c.number(row, "p_service")?,
            "pickup",
        )?;
        let delivery = window(
            &c,
            d,
            c.required(row, "d_open")?,
            c.required(row, "d_close")?,
            c.number(row, "d_service")?,
            "delivery",
        )?;
        out_orders.push(Order {
            demand,
            pickup,
            delivery,
        });
        order_ids.push(id);
    }

    let c = Columns {
        what: "vehicles",
        variables: &vehicles.0,
    };
    let mut out_vehicles = Vec::new();
    let mut vehicle_ids = Vec::new();
    for (id, row) in by_id(&c, &vehicles.1, "id")? {
        let capacity = c.required(row, "capacity")?;
        let speed = c.number(row, "speed")?.unwrap_or(1.0);
        let copies = c.number(row, "number")?.unwrap_or(1.0);
        if capacity <= 0.0 || speed <= 0.0 || copies < 1.0 {
            return Err(PgrError::Request(format!(
                "vehicle {id} needs a positive ?capacity and ?speed, and ?number of at least 1"
            )));
        }
        let s = place(
            &c,
            row,
            &mut locations,
            euclidean,
            "start_node_id",
            "start_x",
            "start_y",
        )?;
        let has_end = if euclidean {
            c.term(row, "end_x")?.is_some() || c.term(row, "end_y")?.is_some()
        } else {
            c.term(row, "end_node_id")?.is_some()
        };
        let e = if has_end {
            place(
                &c,
                row,
                &mut locations,
                euclidean,
                "end_node_id",
                "end_x",
                "end_y",
            )?
        } else {
            s
        };
        let (open, close) = (
            c.required(row, "start_open")?,
            c.required(row, "start_close")?,
        );
        let service = c.number(row, "start_service")?;
        let start = window(&c, s, open, close, service, "start")?;
        let end = window(
            &c,
            e,
            c.number(row, "end_open")?.unwrap_or(open),
            c.number(row, "end_close")?.unwrap_or(close),
            c.number(row, "end_service")?.or(service),
            "end",
        )?;
        for _ in 0..copies as usize {
            out_vehicles.push(Vehicle {
                capacity,
                speed,
                start,
                end,
            });
            vehicle_ids.push(id.clone());
        }
    }
    Ok(Instance {
        orders: out_orders,
        order_ids,
        vehicles: out_vehicles,
        vehicle_ids,
        locations,
    })
}

/// pgr_vrpOneDepot: the order rows (`?id ?x ?y ?order_unit ?open_time ?close_time
/// ?service_time`) include the depot, from which every other one is delivered; every vehicle
/// (`?vehicle_id ?capacity`) leaves from and returns to the depot. Without a cost matrix the
/// travel times are the distances between the `?x ?y` coordinates.
fn one_depot_instance(
    request: &Request,
    orders: &Table,
    vehicles: &Table,
    euclidean: bool,
) -> Result<Instance, PgrError> {
    let depot_id = request.depot.as_ref().expect("checked by Request::parse");
    let mut locations = Locations::default();
    let c = Columns {
        what: "orders",
        variables: &orders.0,
    };
    let rows = by_id(&c, &orders.1, "id")?;
    let stop = |locations: &mut Locations, id: &Term, row: &[Option<Term>], what: &str| {
        let at = if euclidean {
            locations.point(
                c.required(row, "x")?,
                c.required(row, "y")?,
                Some(id.clone()),
            )
        } else {
            locations.node(id.clone())
        };
        window(
            &c,
            at,
            c.required(row, "open_time")?,
            c.required(row, "close_time")?,
            c.number(row, "service_time")?,
            what,
        )
    };
    let (_, depot_row) = rows.iter().find(|(id, _)| id == depot_id).ok_or_else(|| {
        PgrError::Request(format!(
            "pgr:depot_id {depot_id} is not an ?id of the orders sub-select"
        ))
    })?;
    let depot = stop(&mut locations, depot_id, depot_row, "depot")?;
    let mut out_orders = Vec::new();
    let mut order_ids = Vec::new();
    for (id, row) in &rows {
        if id == depot_id {
            continue;
        }
        let demand = c
            .number(row, "order_unit")?
            .or(c.number(row, "demand")?)
            .ok_or_else(|| PgrError::Request(format!("order {id} has no ?order_unit")))?;
        if demand < 0.0 {
            return Err(PgrError::Request(format!(
                "order {id} has a negative ?order_unit"
            )));
        }
        let delivery = stop(&mut locations, id, row, "delivery")?;
        out_orders.push(Order {
            demand,
            pickup: depot,
            delivery,
        });
        order_ids.push(id.clone());
    }

    let c = Columns {
        what: "vehicles",
        variables: &vehicles.0,
    };
    let id_column = if c.index("vehicle_id").is_some() {
        "vehicle_id"
    } else {
        "id"
    };
    let mut out_vehicles = Vec::new();
    let mut vehicle_ids = Vec::new();
    for (id, row) in by_id(&c, &vehicles.1, id_column)? {
        let capacity = c.required(row, "capacity")?;
        if capacity <= 0.0 {
            return Err(PgrError::Request(format!(
                "vehicle {id} needs a positive ?capacity"
            )));
        }
        out_vehicles.push(Vehicle {
            capacity,
            speed: 1.0,
            start: depot,
            end: depot,
        });
        vehicle_ids.push(id);
    }
    Ok(Instance {
        orders: out_orders,
        order_ids,
        vehicles: out_vehicles,
        vehicle_ids,
        locations,
    })
}

fn int(i: i64) -> Option<Term> {
    Some(Literal::from(i).into())
}

fn float(f: f64) -> Option<Term> {
    Some(Literal::from(f).into())
}

/// Plans the orders and returns pgRouting's rows: per vehicle used its start, pickups,
/// deliveries and end, then a summary row (`vehicle_seq` -2, `stop_type` -1). Columns in the
/// order of [`PICK_DELIVER_COLUMNS`], or [`ONE_DEPOT_COLUMNS`] for pgr_vrpOneDepot.
pub(crate) fn vrp_table(
    request: &Request,
    orders: &Table,
    vehicles: &Table,
    matrix: Option<&Table>,
) -> Result<Vec<Row>, PgrError> {
    let mut instance = if request.function == PgrFunction::VrpOneDepot {
        one_depot_instance(request, orders, vehicles, matrix.is_none())?
    } else {
        pick_deliver_instance(request, orders, vehicles)?
    };
    if let Some(m) = matrix {
        instance.locations.fill_matrix(m)?;
    }
    let factor = request.factor;
    let locations = &instance.locations;
    let travel = |a: usize, b: usize| locations.travel(a, b) * factor;
    let plan = pick_deliver(
        &instance.orders,
        &instance.vehicles,
        &travel,
        request.cycles,
    );
    if !plan.unassigned.is_empty() {
        return Err(PgrError::Request(format!(
            "{}: no vehicle can serve order(s) {} within the capacities and time windows",
            match request.function {
                PgrFunction::VrpOneDepot => "pgr_vrpOneDepot",
                PgrFunction::PickDeliverEuclidean => "pgr_pickDeliverEuclidean",
                _ => "pgr_pickDeliver",
            },
            plan.unassigned
                .iter()
                .map(|&o| list_item(&instance.order_ids[o]))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(rows(request, &instance, &plan))
}

fn rows(request: &Request, instance: &Instance, plan: &Plan) -> Vec<Row> {
    let one_depot = request.function == PgrFunction::VrpOneDepot;
    let mut out = Vec::new();
    let (mut travel, mut wait, mut service, mut total) = (0.0, 0.0, 0.0, 0.0);
    for (vehicle_seq, (v, route)) in plan.routes.iter().enumerate() {
        let vehicle_id = &instance.vehicle_ids[*v];
        for (stop_seq, s) in route.iter().enumerate() {
            let (stop_type, order) = match s.kind {
                StopKind::Start => (1, None),
                StopKind::Pickup(o) => (2, Some(o)),
                StopKind::Delivery(o) => (3, Some(o)),
                StopKind::End => (6, None),
            };
            let order_id = match order {
                Some(o) => Some(instance.order_ids[o].clone()),
                None => int(-1),
            };
            let stop_id = instance.locations.labels[s.location].clone();
            travel += s.travel_time;
            wait += s.wait;
            service += s.service;
            let mut row = Vec::with_capacity(ONE_DEPOT_COLUMNS.len());
            if one_depot {
                // pgr_vrpOneDepot marks depot visits with order -1.
                row.extend([
                    order_id.clone(),
                    int(stop_seq as i64 + 1),
                    Some(vehicle_id.clone()),
                    float(s.arrival),
                    float(s.departure),
                ]);
            }
            row.extend([
                int(out.len() as i64 + 1),
                int(vehicle_seq as i64 + 1),
                Some(vehicle_id.clone()),
                int(stop_seq as i64 + 1),
                int(stop_type),
                stop_id,
                order_id,
                float(s.cargo),
                float(s.travel_time),
                float(s.arrival),
                float(s.wait),
                float(s.service),
                float(s.departure),
            ]);
            out.push(row);
        }
        total += route.last().unwrap().departure - route[0].arrival;
    }
    // The summary: no capacity (`vehicle_id`) or time window (`stop_seq`) violations, and
    // the totals.
    let mut row = Vec::with_capacity(ONE_DEPOT_COLUMNS.len());
    if one_depot {
        row.extend([int(-1), int(0), int(0), float(-1.0), float(total)]);
    }
    row.extend([
        int(out.len() as i64 + 1),
        int(-2),
        int(0),
        int(0),
        int(-1),
        int(-1),
        int(-1),
        float(-1.0),
        float(travel),
        float(-1.0),
        float(wait),
        float(service),
        float(total),
    ]);
    out.push(row);
    out
}
