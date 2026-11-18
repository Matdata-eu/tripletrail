//! Vehicle routing with pickups and deliveries, capacities and time windows (pgr_pickDeliver,
//! pgr_pickDeliverEuclidean, pgr_vrpOneDepot).
//!
//! Construction by cheapest feasible insertion, then local search: whole routes are emptied
//! when their orders fit elsewhere, and orders are moved to the position that shortens the
//! plan most, until no move helps. Plans are compared by the number of vehicles used, then
//! by total duration (pgRouting's objective).

/// A place with a time window and a service time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stop {
    /// Index into the travel-time matrix.
    pub location: usize,
    pub open: f64,
    pub close: f64,
    pub service: f64,
}

/// An order: `demand` is picked up at `pickup` and delivered at `delivery`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Order {
    pub demand: f64,
    pub pickup: Stop,
    pub delivery: Stop,
}

/// A vehicle: it leaves `start` no earlier than its opening time and must reach `end` before
/// its closing time. Travel times are divided by `speed`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vehicle {
    pub capacity: f64,
    pub speed: f64,
    pub start: Stop,
    pub end: Stop,
}

/// What happens at a stop of a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopKind {
    Start,
    Pickup(usize),
    Delivery(usize),
    End,
}

/// A stop of a planned route, with its schedule.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlannedStop {
    pub kind: StopKind,
    pub location: usize,
    /// Load after the stop.
    pub cargo: f64,
    pub travel_time: f64,
    pub arrival: f64,
    pub wait: f64,
    pub service: f64,
    pub departure: f64,
}

/// The routes of the vehicles used (by vehicle index) and the orders no vehicle could serve.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub routes: Vec<(usize, Vec<PlannedStop>)>,
    pub unassigned: Vec<usize>,
}

/// One vehicle's sequence of order events.
type Route = Vec<StopKind>;

struct Problem<'a> {
    orders: &'a [Order],
    vehicles: &'a [Vehicle],
    travel: &'a dyn Fn(usize, usize) -> f64,
}

impl Problem<'_> {
    fn stop(&self, k: StopKind, v: usize) -> Stop {
        match k {
            StopKind::Start => self.vehicles[v].start,
            StopKind::End => self.vehicles[v].end,
            StopKind::Pickup(o) => self.orders[o].pickup,
            StopKind::Delivery(o) => self.orders[o].delivery,
        }
    }

    /// The schedule of a route, or `None` if it breaks a time window or the capacity.
    fn schedule(&self, v: usize, route: &[StopKind]) -> Option<Vec<PlannedStop>> {
        let veh = &self.vehicles[v];
        let mut out = Vec::with_capacity(route.len() + 2);
        let start = veh.start;
        let mut time = start.open + start.service;
        let mut cargo = 0.0;
        let mut at = start.location;
        out.push(PlannedStop {
            kind: StopKind::Start,
            location: at,
            cargo,
            travel_time: 0.0,
            arrival: start.open,
            wait: 0.0,
            service: start.service,
            departure: time,
        });
        for &k in route.iter().chain(std::iter::once(&StopKind::End)) {
            let s = self.stop(k, v);
            let travel = (self.travel)(at, s.location) / veh.speed;
            if !travel.is_finite() {
                return None;
            }
            let arrival = time + travel;
            if arrival > s.close + 1e-9 {
                return None;
            }
            let begin = arrival.max(s.open);
            match k {
                StopKind::Pickup(o) => cargo += self.orders[o].demand,
                StopKind::Delivery(o) => cargo -= self.orders[o].demand,
                _ => {}
            }
            if cargo > veh.capacity + 1e-9 {
                return None;
            }
            time = begin + s.service;
            at = s.location;
            out.push(PlannedStop {
                kind: k,
                location: s.location,
                cargo,
                travel_time: travel,
                arrival,
                wait: begin - arrival,
                service: s.service,
                departure: time,
            });
        }
        Some(out)
    }

    /// Duration of a route (end departure minus start), if feasible.
    fn duration(&self, v: usize, route: &[StopKind]) -> Option<f64> {
        if route.is_empty() {
            return Some(0.0);
        }
        let s = self.schedule(v, route)?;
        Some(s.last().unwrap().departure - s[0].arrival)
    }

    /// The cheapest feasible insertion of order `o` into route `v`: (added duration, route).
    fn best_insertion(&self, v: usize, route: &Route, o: usize) -> Option<(f64, Route)> {
        let base = self.duration(v, route)?;
        let mut best: Option<(f64, Route)> = None;
        let n = route.len();
        for i in 0..=n {
            for j in i..=n {
                let mut r = Vec::with_capacity(n + 2);
                r.extend_from_slice(&route[..i]);
                r.push(StopKind::Pickup(o));
                r.extend_from_slice(&route[i..j]);
                r.push(StopKind::Delivery(o));
                r.extend_from_slice(&route[j..]);
                if let Some(d) = self.duration(v, &r) {
                    let added = d - base;
                    if best.as_ref().is_none_or(|b| added < b.0) {
                        best = Some((added, r));
                    }
                }
            }
        }
        best
    }

    /// Inserts `o` where it adds least, preferring vehicles already in use.
    fn insert(&self, routes: &mut [Route], o: usize, skip: Option<usize>) -> bool {
        let mut best: Option<(bool, f64, usize, Route)> = None;
        for (v, route) in routes.iter().enumerate() {
            if Some(v) == skip {
                continue;
            }
            if let Some((added, r)) = self.best_insertion(v, route, o) {
                let new_vehicle = route.is_empty();
                let better = best
                    .as_ref()
                    .is_none_or(|b| (new_vehicle, added) < (b.0, b.1));
                if better {
                    best = Some((new_vehicle, added, v, r));
                }
            }
        }
        match best {
            Some((_, _, v, r)) => {
                routes[v] = r;
                true
            }
            None => false,
        }
    }

    fn cost(&self, routes: &[Route]) -> (usize, f64) {
        let used = routes.iter().filter(|r| !r.is_empty()).count();
        let total: f64 = routes
            .iter()
            .enumerate()
            .map(|(v, r)| self.duration(v, r).unwrap_or(f64::INFINITY))
            .sum();
        (used, total)
    }
}

fn without(route: &Route, o: usize) -> Route {
    route
        .iter()
        .copied()
        .filter(|k| !matches!(k, StopKind::Pickup(x) | StopKind::Delivery(x) if *x == o))
        .collect()
}

fn orders_of(route: &Route) -> Vec<usize> {
    route
        .iter()
        .filter_map(|k| match k {
            StopKind::Pickup(o) => Some(*o),
            _ => None,
        })
        .collect()
}

/// Plans routes for `orders` with `vehicles`; `travel(a, b)` is the travel cost between
/// locations (divided by a vehicle's speed to get its travel time), `f64::INFINITY` when
/// there is no way. `max_cycles` bounds the local search.
pub fn pick_deliver(
    orders: &[Order],
    vehicles: &[Vehicle],
    travel: &dyn Fn(usize, usize) -> f64,
    max_cycles: usize,
) -> Plan {
    let p = Problem {
        orders,
        vehicles,
        travel,
    };
    let mut routes: Vec<Route> = vec![Vec::new(); vehicles.len()];
    let mut unassigned = Vec::new();
    // Tightest delivery windows first.
    let mut order_ids: Vec<usize> = (0..orders.len()).collect();
    order_ids.sort_by(|&a, &b| {
        orders[a]
            .delivery
            .close
            .total_cmp(&orders[b].delivery.close)
            .then(orders[a].pickup.close.total_cmp(&orders[b].pickup.close))
    });
    for o in order_ids {
        if !p.insert(&mut routes, o, None) {
            unassigned.push(o);
        }
    }

    for _ in 0..max_cycles {
        if crate::cancel::stopped_now() {
            break;
        }
        let before = p.cost(&routes);
        // Empty the smallest routes when their orders fit elsewhere.
        let mut by_size: Vec<usize> = (0..routes.len())
            .filter(|&v| !routes[v].is_empty())
            .collect();
        by_size.sort_by_key(|&v| routes[v].len());
        for v in by_size {
            let mut trial = routes.clone();
            let moved = orders_of(&trial[v]);
            trial[v].clear();
            // Only when that helps: moving a route to an idle vehicle as it is changes nothing.
            if moved.iter().all(|&o| p.insert(&mut trial, o, Some(v)))
                && p.cost(&trial) < p.cost(&routes)
            {
                routes = trial;
            }
        }
        // Move single orders to their best place.
        for o in 0..orders.len() {
            let Some(v) = routes.iter().position(|r| r.contains(&StopKind::Pickup(o))) else {
                continue;
            };
            let current = p.cost(&routes);
            let mut trial = routes.clone();
            trial[v] = without(&trial[v], o);
            if p.insert(&mut trial, o, None) && p.cost(&trial) < current {
                routes = trial;
            }
        }
        // Orders left out may fit now.
        unassigned.retain(|&o| !p.insert(&mut routes, o, None));
        if p.cost(&routes) >= before {
            break;
        }
    }

    let routes = routes
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.is_empty())
        .map(|(v, r)| (v, p.schedule(v, r).expect("planned routes are feasible")))
        .collect();
    unassigned.sort_unstable();
    Plan { routes, unassigned }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stop(location: usize, open: f64, close: f64) -> Stop {
        Stop {
            location,
            open,
            close,
            service: 0.0,
        }
    }

    #[test]
    fn one_vehicle_serves_everything_in_window_order() {
        // Locations on a line: 0 depot, 1..=4 at x = 1..4.
        let travel = |a: usize, b: usize| (a as f64 - b as f64).abs();
        let orders = [
            Order {
                demand: 1.0,
                pickup: stop(1, 0.0, 100.0),
                delivery: stop(2, 0.0, 100.0),
            },
            Order {
                demand: 1.0,
                pickup: stop(3, 0.0, 100.0),
                delivery: stop(4, 0.0, 100.0),
            },
        ];
        let v = Vehicle {
            capacity: 1.0,
            speed: 1.0,
            start: stop(0, 0.0, 100.0),
            end: stop(0, 0.0, 100.0),
        };
        let plan = pick_deliver(&orders, &[v, v], &travel, 10);
        assert!(plan.unassigned.is_empty());
        assert_eq!(plan.routes.len(), 1, "{plan:?}");
        // The first vehicle, not an equal one.
        assert_eq!(plan.routes[0].0, 0);
        let r = &plan.routes[0].1;
        assert_eq!(r.last().unwrap().departure, 8.0);
        assert!(r.iter().all(|s| s.cargo <= 1.0));
    }

    #[test]
    fn time_windows_split_the_work() {
        let travel = |a: usize, b: usize| (a as f64 - b as f64).abs() * 10.0;
        let orders = [
            Order {
                demand: 1.0,
                pickup: stop(0, 0.0, 5.0),
                delivery: stop(1, 0.0, 10.0),
            },
            Order {
                demand: 1.0,
                pickup: stop(0, 0.0, 5.0),
                delivery: stop(5, 0.0, 50.0),
            },
        ];
        let v = Vehicle {
            capacity: 10.0,
            speed: 1.0,
            start: stop(0, 0.0, 1000.0),
            end: stop(0, 0.0, 1000.0),
        };
        let plan = pick_deliver(&orders, &[v, v], &travel, 10);
        assert!(plan.unassigned.is_empty());
        // Both fit one vehicle: pick both, drop at 1 (t=10), then 5 (t=50).
        assert_eq!(plan.routes.len(), 1);
        let infeasible = Order {
            demand: 1.0,
            pickup: stop(0, 0.0, 1.0),
            delivery: stop(9, 0.0, 2.0),
        };
        let plan = pick_deliver(&[infeasible], &[v], &travel, 10);
        assert_eq!(plan.unassigned, vec![0]);
    }
}
