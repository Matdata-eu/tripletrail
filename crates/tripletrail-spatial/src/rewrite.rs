use crate::cost::{self, Estimator};
use crate::index::{IndexedPredicates, SpatialIndex};
use crate::sweep;
use geo::BoundingRect;
use oxigraph::model::{Literal, Term};
use oxigraph::sparql::Variable;
use rstar::AABB;
use spargebra::Query;
use spargebra::algebra::{Expression, Function, GraphPattern};
use spargebra::term::{GroundTerm, NamedNodePattern, TermPattern, TriplePattern};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tripletrail_geo::{
    Crs, FastBuildHasher, GEOF, Geom, GeomPredicate, ST, geom_predicate, parse_wkt_literal,
};
use tripletrail_store::{IdRows, TermId};

const GEO: &str = "http://www.opengis.net/ont/geosparql#";
const AS_WKT: &str = "http://www.opengis.net/ont/geosparql#asWKT";
const UOM: &str = "http://www.opengis.net/def/uom/OGC/1.0/";
/// Fewest metres in a degree of latitude, so margins derived from it never fall short.
const METRES_PER_DEGREE: f64 = 110_000.0;
/// A join whose sides cannot be evaluated separately is answered from all pairs of indexed
/// geometries. Probing the R-tree with each of them (the fixed rules) gives up beyond this many
/// candidate pairs and leaves the join to the evaluator.
const MAX_GLOBAL_PAIRS: usize = 2_000_000;
/// The sweep-line join answers such a join in time, so only memory bounds it: it gives up
/// beyond this many result pairs, each a row of the `VALUES` block (a block of 3.4 million
/// pairs of short lines took 3.4 GB at peak, evaluation included).
const MAX_SWEEP_PAIRS: usize = 5_000_000;
/// Term ids a join on ids hands to the evaluation at most (8 bytes each): beyond, the join
/// is done the usual way.
const MAX_TABLE_IDS: usize = 128 << 20;

/// GeoSPARQL properties computed from a spatial object's geometry when the data does not
/// assert them (local name, then the `geof:` function that computes the value).
const VIRTUAL: [(&str, &str); 13] = [
    ("hasMetricLength", "metricLength"),
    ("hasLength", "length"),
    ("hasMetricArea", "metricArea"),
    ("hasArea", "area"),
    ("hasMetricPerimeterLength", "metricPerimeter"),
    ("hasPerimeterLength", "perimeter"),
    ("hasMetricSize", "metricSize"),
    ("hasSize", "size"),
    ("dimension", "dimension"),
    ("coordinateDimension", "coordinateDimension"),
    ("spatialDimension", "spatialDimension"),
    ("isEmpty", "isEmpty"),
    ("isSimple", "isSimple"),
];

/// GeoSPARQL relations that only hold between geometries that touch, by local name. These
/// can be answered from the index (disjointness relations cannot).
const RELATIONS: [&str; 23] = [
    "sfEquals",
    "sfIntersects",
    "sfTouches",
    "sfCrosses",
    "sfWithin",
    "sfContains",
    "sfOverlaps",
    "ehEquals",
    "ehMeet",
    "ehOverlap",
    "ehCovers",
    "ehCoveredBy",
    "ehInside",
    "ehContains",
    "rcc8eq",
    "rcc8ec",
    "rcc8po",
    "rcc8tppi",
    "rcc8tpp",
    "rcc8ntpp",
    "rcc8ntppi",
    // Listed for the triple forms; their functions are never indexable.
    "sfDisjoint",
    "ehDisjoint",
];
const NOT_INDEXABLE: [&str; 3] = ["sfDisjoint", "ehDisjoint", "rcc8dc"];
const ST_RELATIONS: [&str; 10] = [
    "ST_Equals",
    "ST_Intersects",
    "ST_Touches",
    "ST_Crosses",
    "ST_Within",
    "ST_Contains",
    "ST_Overlaps",
    "ST_Covers",
    "ST_CoveredBy",
    "ST_EnvelopesIntersect",
];

/// The rows of a join side on term ids (see [`SpatialContext::side_ids`]).
pub struct IdSide {
    /// The side's variables, one per column of `rows`.
    pub variables: Vec<Variable>,
    pub rows: IdRows,
    /// The column of the geometry variable.
    pub geometry: usize,
    /// The distinct geometries of that column with their bounding boxes, sorted by id
    /// (values that are not geometries, or empty ones, left out: they meet nothing).
    pub geometries: Vec<SideGeometry>,
    /// Parses a geometry of the side, by id. The join parses only the geometries whose
    /// boxes meet one of the other side (#754): reading every polygon of a large side, the
    /// landuse areas of Europe say, takes more memory and time than the store has.
    pub load: Arc<dyn Fn(TermId) -> Option<Arc<Geom>> + Send + Sync>,
    /// Parses a geometry of the side without keeping it (the store's cache is left as it
    /// is). The join reads the larger side so, once per geometry and in id order (#2).
    pub read: Arc<dyn Fn(TermId) -> Option<Arc<Geom>> + Send + Sync>,
}

/// A geometry of an [`IdSide`]: its id, CRS and bounding box (west, south, east, north).
pub struct SideGeometry {
    pub id: TermId,
    pub crs: Crs,
    pub bbox: [f64; 4],
}

/// A geometry of a join side kept for the sweep: its place in the side's `geometries`, and
/// its parsed geometry once a test needs it.
struct Lazy<'s> {
    at: u32,
    geometry: &'s SideGeometry,
    parsed: std::sync::OnceLock<Option<Arc<Geom>>>,
}

impl Lazy<'_> {
    fn geom(&self, load: &(dyn Fn(TermId) -> Option<Arc<Geom>> + Send + Sync)) -> Option<&Geom> {
        self.parsed
            .get_or_init(|| load(self.geometry.id))
            .as_deref()
    }
}

/// What the rewrite needs from the store.
pub trait SpatialContext: Sync {
    /// The spatial index; called at most once per query, and only if the query needs it.
    fn index(&self) -> Option<Arc<SpatialIndex>>;
    /// The predicates whose geometries the index holds (or will hold once built), when the
    /// context knows them without the index: a join on term ids then doesn't wait for the
    /// index (#754), which takes minutes to build over hundreds of millions of geometries.
    fn indexed_predicates(&self) -> Option<IndexedPredicates> {
        None
    }
    /// Whether any triple has this predicate (asserted relations must still be found).
    fn has_predicate(&self, iri: &str) -> bool;
    /// The distinct values `var` takes in `pattern`, evaluated on the default graph, or `None`
    /// if that fails.
    fn values_of(&self, pattern: &GraphPattern, var: &Variable) -> Option<Vec<Term>>;
    /// Every solution of `pattern` (not made distinct) over `variables`, evaluated on the
    /// default graph, or `None` if that fails or the context does not offer it (the default).
    /// A join then binds its sides from these rows instead of looking each pair up again.
    fn rows_of(
        &self,
        pattern: &GraphPattern,
        variables: &[Variable],
    ) -> Option<Vec<Vec<Option<Term>>>> {
        let _ = (pattern, variables);
        None
    }
    /// The rows of a join side on term ids, evaluated on the default graph: `None` (the
    /// default) if the context does not offer it or `pattern` is not a plain basic graph
    /// pattern. A join with both sides given so is computed on ids and handed to the
    /// evaluation as a table (see [`Self::table`]).
    fn side_ids(&self, pattern: &GraphPattern, var: &Variable) -> Option<IdSide> {
        let _ = (pattern, var);
        None
    }
    /// A pattern that reads `rows` (one column per variable) in the evaluation of the query
    /// being rewritten, or `None` (the default) if the context does not offer it.
    fn table(&self, variables: &[Variable], rows: IdRows) -> Option<GraphPattern> {
        let _ = (variables, rows);
        None
    }
    /// About how many triples match `pattern` (its variables and blank nodes match anything),
    /// in the default graph or, with `all_graphs`, in any graph. Must be cheap: it is asked
    /// while planning. `None` (the default) turns the cost model off: the rewrite then makes
    /// its choices by fixed rules.
    fn estimate(&self, pattern: &TriplePattern, all_graphs: bool) -> Option<f64> {
        let _ = (pattern, all_graphs);
        None
    }
    /// About how many rows the join of `triples` gives, following each join through the
    /// matches per value of the triple it reaches (`?v ex:country ?c . ?o ex:vendor ?v` has a
    /// row per offer, not per vendor). Must be cheap. `None` (the default): the cost model
    /// takes the smallest triple's matches.
    fn estimate_join(&self, triples: &[TriplePattern], all_graphs: bool) -> Option<f64> {
        let _ = (triples, all_graphs);
        None
    }
    /// Whether the cost model may answer a join of two large sides with the sweep-line join
    /// (the default) rather than by probing the index once per geometry of one side.
    fn sweep_join(&self) -> bool {
        true
    }
    /// The parsed geometry of a geometry literal. The default parses it (through the parse
    /// cache); a store that keeps its geometries parsed answers from them.
    fn geometry(&self, literal: &Literal) -> Option<Arc<Geom>> {
        parse_wkt_literal(&Term::Literal(literal.clone()))
    }
    /// Whether the query was cancelled (a timeout): the rewrite then stops looking for
    /// candidates and leaves the rest of the query as it is, for the evaluation to fail.
    /// Checked between candidate tests, so it must be cheap.
    fn cancelled(&self) -> bool {
        false
    }
    /// How many objects of `predicate` are not geometry literals, if known (cached by the
    /// store side; `None`, the default, if not). The nearest-neighbour path takes geometries
    /// bound through a predicate other than `geo:asWKT` only when this is `Some(0)`: other
    /// values have an error distance and sort first, but are not in the index.
    fn non_geometry_objects(&self, predicate: &str) -> Option<usize> {
        let _ = predicate;
        None
    }
}

/// A choice the cost model made, with the estimates it was made from (see
/// [`RewriteStats::decisions`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// A test of `var` against a constant geometry: its candidates were put in a `VALUES`
    /// block (`inject`), or left to the FILTER because the index would return more of them
    /// than the rest of the pattern has rows for `var`.
    Constant {
        var: Variable,
        /// Estimated index candidates (bounding boxes meeting the test's envelope).
        candidates: usize,
        /// Estimated rows of the operands that bind `var`.
        rows: f64,
        inject: bool,
    },
    /// A test between two variables: the geometries `probe` takes were looked up in the
    /// index. With `by_estimate` the probe side was chosen from the estimates and only it was
    /// evaluated; otherwise both sides were evaluated and compared.
    Join {
        a: Variable,
        b: Variable,
        /// Estimated (or, after a constant test, exact) geometries of each side.
        rows_a: Option<f64>,
        rows_b: Option<f64>,
        probe: Variable,
        by_estimate: bool,
        /// Sides evaluated to find the probe geometries.
        evaluated: usize,
        /// The pairs came from a sweep-line join of both sides rather than from probing the
        /// index with each geometry of `probe` (both sides were large).
        sweep: bool,
    },
    /// A test between two variables answered on term ids (the tripletrail store): both sides
    /// evaluated on ids, their geometries swept with the margin on `probe` (the side with fewer
    /// geometries), and the joined rows handed to the evaluation as a table of `rows` rows.
    IdJoin {
        a: Variable,
        b: Variable,
        rows_a: usize,
        rows_b: usize,
        probe: Variable,
        pairs: usize,
        rows: usize,
    },
    /// A candidate block placed before the triples binding its variables with `LATERAL`, so
    /// they are looked up per candidate: the evaluator's own guess would have started from
    /// `start`, estimated at `start_rows`, rather than from the block's `rows`. When the
    /// triples fall into groups only the block connects, `start_rows` is the estimate of the
    /// groups' cartesian product, which the evaluator would build.
    Pin {
        variables: Vec<Variable>,
        rows: usize,
        start: String,
        start_rows: f64,
    },
}

impl std::fmt::Display for Decision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Constant {
                var,
                candidates,
                rows,
                inject,
            } => write!(
                f,
                "{} {var}: ~{candidates} candidates vs ~{rows:.0} rows",
                if *inject {
                    "inject VALUES for"
                } else {
                    "leave to FILTER"
                }
            ),
            Self::IdJoin {
                a,
                b,
                rows_a,
                rows_b,
                probe,
                pairs,
                rows,
            } => write!(
                f,
                "join {a} ({rows_a} rows) x {b} ({rows_b} rows) on term ids: sweep-line join, \
                 margin on {probe}, {pairs} pairs, a table of {rows} rows"
            ),
            Self::Join {
                a,
                b,
                rows_a,
                rows_b,
                probe,
                by_estimate,
                evaluated,
                sweep,
            } => {
                let show = |r: &Option<f64>| r.map_or("?".to_owned(), |r| format!("~{r:.0}"));
                write!(
                    f,
                    "join {a} ({}) x {b} ({}): {} {probe} {}, {evaluated} side(s) evaluated",
                    show(rows_a),
                    show(rows_b),
                    if *sweep {
                        "sweep-line join, margin on"
                    } else {
                        "index nested loop, probe"
                    },
                    if *by_estimate {
                        "by estimate"
                    } else {
                        "by evaluation"
                    }
                )
            }
            Self::Pin {
                variables,
                rows,
                start,
                start_rows,
            } => {
                let vars: Vec<String> = variables.iter().map(ToString::to_string).collect();
                write!(
                    f,
                    "pin VALUES ({}) of {rows} rows before {start} (~{start_rows:.0} rows)",
                    vars.join(" ")
                )
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RewriteStats {
    /// FILTERs against a constant geometry answered through the index.
    pub filters: usize,
    /// Spatial joins between two variables answered through the index.
    pub joins: usize,
    /// GeoSPARQL relation triples (`?a geo:sfIntersects ?b`) rewritten to geometry tests.
    pub triples: usize,
    /// Virtual property triples (`?g geo:hasMetricLength ?l`) computed from geometries.
    pub virtual_properties: usize,
    /// Geometries (or pairs, for joins) put in the generated VALUES blocks.
    pub candidates: usize,
    /// Time spent in index lookups, side evaluation and exact tests.
    pub lookup: Duration,
    /// Constant tests the cost model left to the FILTER (see [`Decision::Constant`]).
    pub skipped: usize,
    /// Join sides evaluated to find the geometries to probe the index with.
    pub sides_evaluated: usize,
    /// Joins answered by the sweep-line join (see [`Decision::Join`]).
    pub sweeps: usize,
    /// Of `lookup`, the time spent finding the pairs of the joins once their sides were known
    /// (by probing the index or by sweeping).
    pub pairing: Duration,
    /// Candidate blocks placed before the triples binding their variables
    /// (see [`Decision::Pin`]).
    pub pinned: usize,
    /// What the cost model decided, in order. Empty when the context gives no estimates.
    pub decisions: Vec<Decision>,
    /// FILTER conjuncts answered exactly by their `VALUES` block and dropped from the FILTER,
    /// so the evaluator does not test them again on every row.
    pub answered: usize,
    /// Joins whose block binds every variable of both sides from their evaluated rows, so
    /// the sides' triples were dropped from the query.
    pub materialized: usize,
}

/// How far to grow a geometry's bounding box before probing the index.
#[derive(Clone, Copy)]
enum Margin {
    None,
    /// In the units of the coordinates.
    Planar(f64),
    /// In metres; converted to degrees for geographic coordinates.
    Metres(f64),
}

impl Margin {
    fn envelope(self, geom: &Geom) -> Option<AABB<[f64; 2]>> {
        let rect = geom.geometry.bounding_rect()?;
        Some(self.around(
            &geom.crs,
            [rect.min().x, rect.min().y, rect.max().x, rect.max().y],
        ))
    }

    /// The box `bbox` (west, south, east, north) in `crs`, grown by the margin.
    fn around(self, crs: &Crs, bbox: [f64; 4]) -> AABB<[f64; 2]> {
        let [x0, y0, x1, y1] = bbox;
        let (dx, dy) = match self {
            Margin::None => (0.0, 0.0),
            Margin::Planar(d) => (d, d),
            Margin::Metres(d) if crs.is_geographic() => {
                let dy = d / METRES_PER_DEGREE;
                let lat = y0.abs().max(y1.abs()) + dy;
                let dx = if lat >= 89.0 {
                    360.0
                } else {
                    d / (METRES_PER_DEGREE * lat.to_radians().cos())
                };
                (dx, dy)
            }
            Margin::Metres(d) => (d, d),
        };
        AABB::from_corners([x0 - dx, y0 - dy], [x1 + dx, y1 + dy])
    }
}

#[derive(Clone, Copy)]
enum Cmp {
    True,
    Less(f64),
    LessOrEqual(f64),
}

/// A geometry argument of the tested function: a variable, or a constant parsed once.
#[derive(Clone)]
enum Slot {
    Var(Variable),
    Const(Term, Option<Arc<Geom>>),
}

/// Sets of literals, hashed on a sample of their text (geometry literals can be long).
pub(crate) type LiteralSet = HashSet<Literal, FastBuildHasher>;

/// An indexable spatial test: a function of one or two store geometries.
struct Plan {
    /// The test on parsed geometries.
    pred: GeomPredicate,
    /// The two geometry arguments.
    args: [Slot; 2],
    /// The constant arguments after them (a unit, a distance).
    extra: Vec<Term>,
    margin: Margin,
    cmp: Cmp,
    /// The FILTER conjunct this plan tests, dropped from the FILTER when the candidates the
    /// plan puts in a `VALUES` block were all tested exactly.
    conjunct: Expression,
}

enum Shape {
    /// `var` against a constant geometry.
    Constant { var: Variable, constant: Term },
    /// Two variables.
    Join { a: Variable, b: Variable },
}

impl Plan {
    fn shape(&self) -> Option<Shape> {
        match (&self.args[0], &self.args[1]) {
            (Slot::Var(v), Slot::Const(c, _)) | (Slot::Const(c, _), Slot::Var(v)) => {
                Some(Shape::Constant {
                    var: v.clone(),
                    constant: c.clone(),
                })
            }
            (Slot::Var(a), Slot::Var(b)) if a != b => Some(Shape::Join {
                a: a.clone(),
                b: b.clone(),
            }),
            _ => None,
        }
    }

    /// Whether the test holds with each variable bound to the geometry `bind` gives it.
    fn accepts<'g>(&'g self, bind: &dyn Fn(&Variable) -> &'g Geom) -> bool {
        let geometry = |s: &'g Slot| match s {
            Slot::Var(v) => Some(bind(v)),
            Slot::Const(_, g) => g.as_deref(),
        };
        let (Some(a), Some(b)) = (geometry(&self.args[0]), geometry(&self.args[1])) else {
            return false;
        };
        let Some(v) = (self.pred)(a, b, &self.extra) else {
            return false;
        };
        match self.cmp {
            Cmp::True => v == 1.0,
            Cmp::Less(d) => v < d,
            Cmp::LessOrEqual(d) => v <= d,
        }
    }

    /// [`Plan::accepts`] with every variable bound to `g` (a test against a constant).
    fn accepts_one(&self, g: &Geom) -> bool {
        self.accepts(&|_| g)
    }
}

fn literal_number(e: &Expression) -> Option<f64> {
    match e {
        Expression::Literal(l) => l.value().parse().ok(),
        _ => None,
    }
}

/// The two geometry arguments (variables or literals), then the constant extras.
fn slots(args: &[Expression]) -> Option<([Slot; 2], Vec<Term>)> {
    let geometry = |a: &Expression| match a {
        Expression::Variable(v) => Some(Slot::Var(v.clone())),
        Expression::Literal(l) => {
            let t = Term::Literal(l.clone());
            let g = parse_wkt_literal(&t);
            Some(Slot::Const(t, g))
        }
        _ => None,
    };
    let [a, b, rest @ ..] = args else {
        return None;
    };
    let extra = rest
        .iter()
        .map(|a| match a {
            Expression::Literal(l) => Some(Term::Literal(l.clone())),
            Expression::NamedNode(n) => Some(n.clone().into()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some(([geometry(a)?, geometry(b)?], extra))
}

fn custom(f: &Function) -> Option<&str> {
    match f {
        Function::Custom(n) => Some(n.as_str()),
        _ => None,
    }
}

fn is_indexable_relation(iri: &str) -> bool {
    iri.strip_prefix(GEOF)
        .is_some_and(|n| RELATIONS.contains(&n) && !NOT_INDEXABLE.contains(&n))
        || iri
            .strip_prefix(ST)
            .is_some_and(|n| ST_RELATIONS.contains(&n))
}

/// Indexable conjuncts of a FILTER expression.
fn plans(expr: &Expression, out: &mut Vec<Plan>) {
    match expr {
        Expression::And(a, b) => {
            plans(a, out);
            plans(b, out);
        }
        Expression::FunctionCall(f, args) => {
            let Some(iri) = custom(f) else { return };
            if is_indexable_relation(iri) && args.len() == 2 {
                if let (Some((args, extra)), Some(pred)) = (slots(args), geom_predicate(iri)) {
                    out.push(Plan {
                        pred,
                        args,
                        extra,
                        margin: Margin::None,
                        cmp: Cmp::True,
                        conjunct: expr.clone(),
                    });
                }
                return;
            }
            let within: fn(f64) -> Margin = match iri.strip_prefix(ST) {
                Some("ST_DWithin") => Margin::Planar,
                Some("ST_DWithinSphere") => Margin::Metres,
                _ => return,
            };
            if args.len() != 3 {
                return;
            }
            if let (Some(d), Some((args, extra)), Some(pred)) =
                (literal_number(&args[2]), slots(args), geom_predicate(iri))
            {
                out.push(Plan {
                    pred,
                    args,
                    extra,
                    margin: within(d),
                    cmp: Cmp::True,
                    conjunct: expr.clone(),
                });
            }
        }
        Expression::Less(a, b) => out.extend(compared(distance_plan(a, b, Cmp::Less), expr)),
        Expression::LessOrEqual(a, b) => {
            out.extend(compared(distance_plan(a, b, Cmp::LessOrEqual), expr))
        }
        Expression::Greater(a, b) => out.extend(compared(distance_plan(b, a, Cmp::Less), expr)),
        Expression::GreaterOrEqual(a, b) => {
            out.extend(compared(distance_plan(b, a, Cmp::LessOrEqual), expr))
        }
        _ => {}
    }
}

/// A distance plan with the comparison `expr` as its conjunct.
fn compared(plan: Option<Plan>, expr: &Expression) -> Option<Plan> {
    plan.map(|p| Plan {
        conjunct: expr.clone(),
        ..p
    })
}

/// `expr` without the conjuncts in `answered`, or `None` if nothing is left.
fn without_conjuncts(expr: Expression, answered: &[&Expression]) -> Option<Expression> {
    match expr {
        Expression::And(a, b) => match (
            without_conjuncts(*a, answered),
            without_conjuncts(*b, answered),
        ) {
            (Some(a), Some(b)) => Some(Expression::And(Box::new(a), Box::new(b))),
            (a, b) => a.or(b),
        },
        e if answered.contains(&&e) => None,
        e => Some(e),
    }
}

/// `distance(a, b [, unit]) < d`
fn distance_plan(call: &Expression, bound: &Expression, cmp: fn(f64) -> Cmp) -> Option<Plan> {
    let Expression::FunctionCall(f, args) = call else {
        return None;
    };
    let iri = custom(f)?;
    let d = literal_number(bound)?;
    let margin = if iri == format!("{GEOF}distance") {
        let unit = match args.get(2) {
            None => "metre",
            Some(Expression::NamedNode(n)) => n.as_str().strip_prefix(UOM)?,
            _ => return None,
        };
        match unit {
            "metre" => Margin::Metres(d),
            "kilometre" => Margin::Metres(d * 1000.0),
            "degree" => Margin::Planar(d),
            _ => return None,
        }
    } else if iri == format!("{ST}ST_Distance") {
        Margin::Planar(d)
    } else if iri == format!("{ST}ST_DistanceSphere") {
        Margin::Metres(d)
    } else {
        return None;
    };
    let (args, extra) = slots(args)?;
    Some(Plan {
        pred: geom_predicate(iri)?,
        args,
        extra,
        margin,
        cmp: cmp(d),
        // Set by `plans` to the comparison; a nearest-neighbour plan has none.
        conjunct: call.clone(),
    })
}

/// Whether every value of `var` in `pattern` is the object of a `geo:asWKT` triple, so the
/// index holds all of them (the nearest-neighbour rewrite needs every value, see
/// [`SpatialIndex::outside`]).
/// Whether every value of `var` in `pattern` is the object of a triple whose predicate the
/// index covers, so every geometry literal among them is in the index. The values that are
/// not geometry literals are an error for every spatial function, so a test of `var` rejects
/// them and a `VALUES` block of indexed literals loses none of its rows.
fn binds_geometry(pattern: &GraphPattern, var: &Variable, predicates: &IndexedPredicates) -> bool {
    binds_through(
        pattern,
        var,
        &|p| match p {
            NamedNodePattern::NamedNode(p) => predicates.covers(p.as_str()),
            NamedNodePattern::Variable(_) => predicates.covers_all(),
        },
        true,
    )
}

/// Whether every value of `var` in `pattern` is the object of a triple whose predicate is
/// `covered`. With `limited`, also through a subselect's `ORDER BY`, `LIMIT` and `REDUCED`,
/// which keep some of its rows (#359): a FILTER or join evaluates such a side once, but the
/// nearest-neighbour rounds would evaluate it again each round, so they leave it alone.
pub fn binds_through(
    pattern: &GraphPattern,
    var: &Variable,
    covered: &dyn Fn(&NamedNodePattern) -> bool,
    limited: bool,
) -> bool {
    let rec = |p: &GraphPattern| binds_through(p, var, covered, limited);
    match pattern {
        GraphPattern::Bgp { patterns } => patterns.iter().any(|t| {
            covered(&t.predicate) && matches!(&t.object, TermPattern::Variable(v) if v == var)
        }),
        GraphPattern::Join { left, right } => rec(left) || rec(right),
        GraphPattern::Union { left, right } => rec(left) && rec(right),
        GraphPattern::Filter { inner, .. }
        | GraphPattern::Graph { inner, .. }
        | GraphPattern::Minus { left: inner, .. }
        | GraphPattern::Distinct { inner } => rec(inner),
        GraphPattern::Reduced { inner }
        | GraphPattern::OrderBy { inner, .. }
        | GraphPattern::Slice { inner, .. } => limited && rec(inner),
        GraphPattern::Project { inner, variables } => variables.contains(var) && rec(inner),
        GraphPattern::Extend {
            inner, variable, ..
        } => variable != var && rec(inner),
        _ => false,
    }
}

/// Adds the variables `e` reads to `out`; false when `e` holds an `EXISTS` (whose pattern may
/// read any variable of the row).
fn expression_variables(e: &Expression, out: &mut HashSet<Variable>) -> bool {
    match e {
        Expression::Variable(v) | Expression::Bound(v) => {
            out.insert(v.clone());
            true
        }
        Expression::NamedNode(_) | Expression::Literal(_) => true,
        Expression::Exists(_) => false,
        Expression::FunctionCall(_, args) | Expression::Coalesce(args) => {
            args.iter().all(|a| expression_variables(a, out))
        }
        Expression::In(a, list) => {
            expression_variables(a, out) && list.iter().all(|l| expression_variables(l, out))
        }
        Expression::Or(a, b)
        | Expression::And(a, b)
        | Expression::Equal(a, b)
        | Expression::SameTerm(a, b)
        | Expression::Greater(a, b)
        | Expression::GreaterOrEqual(a, b)
        | Expression::Less(a, b)
        | Expression::LessOrEqual(a, b)
        | Expression::Add(a, b)
        | Expression::Subtract(a, b)
        | Expression::Multiply(a, b)
        | Expression::Divide(a, b) => expression_variables(a, out) && expression_variables(b, out),
        Expression::UnaryPlus(a) | Expression::UnaryMinus(a) | Expression::Not(a) => {
            expression_variables(a, out)
        }
        Expression::If(a, b, c) => {
            expression_variables(a, out)
                && expression_variables(b, out)
                && expression_variables(c, out)
        }
    }
}

/// Whether a test reading `used` can go below `p`: through `BIND`s of other variables, `p`
/// is a left join whose left side binds `used` in every row.
fn below_optional(p: &GraphPattern, used: &HashSet<Variable>) -> bool {
    match p {
        GraphPattern::LeftJoin { left, .. } => certain(left).is_superset(used),
        GraphPattern::Extend {
            inner, variable, ..
        } => !used.contains(variable) && below_optional(inner, used),
        _ => false,
    }
}

/// The conjuncts of `e` (`a && b && c` as `[a, b, c]`).
fn split_and(e: Expression, out: &mut Vec<Expression>) {
    match e {
        Expression::And(a, b) => {
            split_and(*a, out);
            split_and(*b, out);
        }
        e => out.push(e),
    }
}

/// The variables bound in every row of `p`. Under-approximates.
fn certain(p: &GraphPattern) -> HashSet<Variable> {
    use GraphPattern as G;
    let term = |t: &TermPattern, out: &mut HashSet<Variable>| {
        if let TermPattern::Variable(v) = t {
            out.insert(v.clone());
        }
    };
    match p {
        G::Bgp { patterns } => {
            let mut out = HashSet::new();
            for t in patterns {
                term(&t.subject, &mut out);
                if let NamedNodePattern::Variable(v) = &t.predicate {
                    out.insert(v.clone());
                }
                term(&t.object, &mut out);
            }
            out
        }
        G::Path {
            subject, object, ..
        } => {
            let mut out = HashSet::new();
            term(subject, &mut out);
            term(object, &mut out);
            out
        }
        G::Join { left, right } | G::Lateral { left, right } => {
            let mut out = certain(left);
            out.extend(certain(right));
            out
        }
        G::LeftJoin { left, .. } | G::Minus { left, .. } => certain(left),
        G::Union { left, right } => {
            let right = certain(right);
            certain(left)
                .into_iter()
                .filter(|v| right.contains(v))
                .collect()
        }
        G::Graph { name, inner } => {
            let mut out = certain(inner);
            if let NamedNodePattern::Variable(v) = name {
                out.insert(v.clone());
            }
            out
        }
        G::Values {
            variables,
            bindings,
        } => variables
            .iter()
            .enumerate()
            .filter(|(i, _)| bindings.iter().all(|row| row[*i].is_some()))
            .map(|(_, v)| v.clone())
            .collect(),
        G::Project { inner, variables }
        | G::Group {
            inner, variables, ..
        } => {
            let inner = certain(inner);
            variables
                .iter()
                .filter(|v| inner.contains(*v))
                .cloned()
                .collect()
        }
        G::Filter { inner, .. }
        | G::Extend { inner, .. }
        | G::OrderBy { inner, .. }
        | G::Distinct { inner }
        | G::Reduced { inner }
        | G::Slice { inner, .. } => certain(inner),
        G::Service { .. } => HashSet::new(),
    }
}

fn in_scope(p: &GraphPattern) -> HashSet<Variable> {
    let mut out = HashSet::new();
    p.on_in_scope_variable(|v| {
        out.insert(v.clone());
    });
    out
}

/// The join operands of `p` connected to `var` through shared variables or blank nodes (the
/// triples in one BGP), and every name they reach.
fn component(p: &GraphPattern, var: &Variable) -> (Option<GraphPattern>, HashSet<cost::Name>) {
    let mut parts = Vec::new();
    cost::join_parts(p, &mut parts);
    let (taken, reached) = cost::connected(&parts, &HashSet::from([cost::var_name(var)]));
    let taken = parts
        .into_iter()
        .zip(taken)
        .filter(|(_, t)| *t)
        .map(|(p, _)| p)
        .collect();
    (cost::rejoin(taken), reached)
}

/// `p` without the join operands of `remove` (each taken out once).
fn without_parts(p: GraphPattern, remove: &[GraphPattern]) -> GraphPattern {
    let mut parts = Vec::new();
    cost::join_parts(&p, &mut parts);
    let mut gone = Vec::new();
    for r in remove {
        cost::join_parts(r, &mut gone);
    }
    parts.retain(|part| match gone.iter().position(|g| g == part) {
        Some(i) => {
            gone.swap_remove(i);
            false
        }
        None => true,
    });
    cost::rejoin(parts).unwrap_or_else(|| bgp(Vec::new()))
}

/// The join operands of `p` connected to `var`, or `None` if `other` is connected to it too
/// (then the two sides cannot be evaluated separately).
fn side(p: &GraphPattern, var: &Variable, other: &Variable) -> Option<GraphPattern> {
    let (side, reached) = component(p, var);
    if reached.contains(&cost::var_name(other)) {
        return None;
    }
    side
}

/// Joins `first` (VALUES blocks) with the join operands of `inner`, left-deep, each next
/// operand chosen to share a variable with what is already joined. Evaluation then starts
/// from the candidates and every later join is keyed, instead of first building the
/// cartesian product of unconnected operands (such as the two geometry lookups of
/// `?a geo:sfIntersects ?b`) and only then meeting the candidates.
fn join_after(first: Vec<GraphPattern>, inner: GraphPattern) -> GraphPattern {
    fn operands(p: GraphPattern, out: &mut Vec<GraphPattern>) {
        match p {
            GraphPattern::Join { left, right } => {
                operands(*left, out);
                operands(*right, out);
            }
            GraphPattern::Bgp { patterns } if patterns.is_empty() => {}
            other => out.push(other),
        }
    }
    let mut rest = Vec::new();
    operands(inner, &mut rest);
    let mut scope = HashSet::new();
    let mut acc: Option<GraphPattern> = None;
    let mut push = |p: GraphPattern, scope: &mut HashSet<Variable>| {
        scope.extend(in_scope(&p));
        acc = Some(match acc.take() {
            None => p,
            Some(a) => join(a, p),
        });
    };
    for p in first {
        push(p, &mut scope);
    }
    while !rest.is_empty() {
        let i = rest
            .iter()
            .position(|p| in_scope(p).iter().any(|v| scope.contains(v)))
            .unwrap_or(0);
        let p = rest.remove(i);
        push(p, &mut scope);
    }
    acc.unwrap_or_else(|| bgp(Vec::new()))
}

fn bgp(patterns: Vec<TriplePattern>) -> GraphPattern {
    GraphPattern::Bgp { patterns }
}

/// The virtual property of a triple and the function computing it, if it has one.
fn virtual_function(t: &TriplePattern) -> Option<(&'static str, &'static str)> {
    let NamedNodePattern::NamedNode(p) = &t.predicate else {
        return None;
    };
    let name = p.as_str().strip_prefix(GEO)?;
    VIRTUAL.iter().copied().find(|(n, _)| *n == name)
}

fn triple(s: TermPattern, p: &str, o: TermPattern) -> TriplePattern {
    TriplePattern {
        subject: s,
        predicate: NamedNodePattern::NamedNode(oxigraph::model::NamedNode::new_unchecked(p)),
        object: o,
    }
}

fn union(left: GraphPattern, right: GraphPattern) -> GraphPattern {
    GraphPattern::Union {
        left: Box::new(left),
        right: Box::new(right),
    }
}

fn join(left: GraphPattern, right: GraphPattern) -> GraphPattern {
    match (&left, &right) {
        (GraphPattern::Bgp { patterns }, _) if patterns.is_empty() => right,
        (_, GraphPattern::Bgp { patterns }) if patterns.is_empty() => left,
        _ => GraphPattern::Join {
            left: Box::new(left),
            right: Box::new(right),
        },
    }
}

/// What identifies a geometry in a join: its literal, or its term id in the store.
trait IndexKey {
    /// The key of an index entry, for joins against every indexed geometry (literals only).
    fn of_entry<'a>(index: &'a SpatialIndex, e: &'a crate::index::Entry) -> Option<&'a Self>;
}

impl IndexKey for Literal {
    fn of_entry<'a>(index: &'a SpatialIndex, e: &'a crate::index::Entry) -> Option<&'a Self> {
        Some(index.literal(e))
    }
}

impl IndexKey for u32 {
    fn of_entry<'a>(_: &'a SpatialIndex, _: &'a crate::index::Entry) -> Option<&'a Self> {
        None
    }
}

/// A side's rows grouped by geometry (by place in its sorted `geometries`), as one array of
/// row numbers with each geometry's run in it.
struct RowsByGeometry {
    starts: Vec<u32>,
    rows: Vec<u32>,
}

impl RowsByGeometry {
    fn new(side: &IdSide, threads: usize) -> Self {
        let n = side.geometries.len();
        let width = side.rows.width;
        let row_numbers: Vec<u32> = (0..side.rows.len() as u32).collect();
        // `geometries` is sorted by id: each row's geometry by binary search (u32::MAX for
        // a value that is not a geometry).
        let of_row = sweep::par_map(&row_numbers, threads, 4096, |&r| {
            let id = side.rows.ids[r as usize * width + side.geometry];
            side.geometries
                .binary_search_by_key(&id, |g| g.id)
                .map_or(u32::MAX, |i| i as u32)
        });
        let mut starts = vec![0u32; n + 1];
        for &g in &of_row {
            if g != u32::MAX {
                starts[g as usize + 1] += 1;
            }
        }
        for i in 0..n {
            starts[i + 1] += starts[i];
        }
        let mut next = starts.clone();
        let mut rows = vec![0u32; starts[n] as usize];
        for (r, &g) in of_row.iter().enumerate() {
            if g != u32::MAX {
                rows[next[g as usize] as usize] = r as u32;
                next[g as usize] += 1;
            }
        }
        Self { starts, rows }
    }

    fn of(&self, geometry: u32) -> &[u32] {
        let g = geometry as usize;
        &self.rows[self.starts[g] as usize..self.starts[g + 1] as usize]
    }
}

impl IndexKey for TermId {
    fn of_entry<'a>(_: &'a SpatialIndex, _: &'a crate::index::Entry) -> Option<&'a Self> {
        None
    }
}

/// The parsed geometries of literals, on every core; those that are not geometries, or are
/// empty, are dropped: they are not in the index and meet nothing.
fn parsed<'a>(
    ctx: &dyn SpatialContext,
    literals: impl Iterator<Item = &'a Literal>,
) -> Vec<(Literal, Arc<Geom>)> {
    let literals: Vec<&Literal> = literals.collect();
    let threads = tripletrail_store::par::cores();
    sweep::par_map(&literals, threads, 1024, |l| {
        let g = ctx.geometry(l)?;
        g.geometry.bounding_rect()?;
        Some(((*l).clone(), g))
    })
    .into_iter()
    .flatten()
    .collect()
}

/// The pairs of a `probe` geometry and an `other` one (every indexed geometry when `None`)
/// whose bounding boxes meet once the probe's is grown by `margin`, and that pass `accepts`,
/// from a sweep-line join in each coordinate space (the index's trees: geographic, or one per
/// other CRS). `None` beyond `limit` pairs.
/// Above this many bytes of literals, pairs are written per geometry (see [`pairs_pattern`]).
const MAX_PAIRS_BYTES: usize = 64 << 20;

/// The pairs of geometries passing a two-variable test, as `VALUES (?a ?b) { ... }`. When a
/// geometry is in many pairs and is large (a country polygon containing 30,000 tracks), every
/// row would hold its own copy of its literal: gigabytes for 400 kB polygons. They are then
/// written once per geometry instead, as the union over the geometries of the side whose
/// literals are larger of `VALUES ?a { g } VALUES ?b { its matches }`: the same solutions.
fn pairs_pattern(
    (probe_var, other_var): (&Variable, &Variable),
    pairs: &[(&Literal, &Literal)],
) -> GraphPattern {
    let bytes = |l: &Literal| l.value().len() + 16;
    let flat: usize = pairs.iter().map(|(p, o)| bytes(p) + bytes(o)).sum();
    // Bytes when grouped on the probe side, and when grouped on the other side.
    let grouped = |flip: bool| {
        let mut seen: HashSet<&Literal> = HashSet::new();
        pairs
            .iter()
            .map(|&(p, o)| {
                let (k, v) = if flip { (o, p) } else { (p, o) };
                bytes(v) + if seen.insert(k) { bytes(k) } else { 0 }
            })
            .sum::<usize>()
    };
    let (by_probe, by_other) = (grouped(false), grouped(true));
    if flat <= MAX_PAIRS_BYTES || flat <= by_probe.min(by_other) * 2 {
        return GraphPattern::Values {
            variables: vec![probe_var.clone(), other_var.clone()],
            bindings: pairs
                .iter()
                .map(|(p, o)| {
                    vec![
                        Some(GroundTerm::Literal((*p).clone())),
                        Some(GroundTerm::Literal((*o).clone())),
                    ]
                })
                .collect(),
        };
    }
    let (key_var, value_var, flip) = if by_probe <= by_other {
        (probe_var, other_var, false)
    } else {
        (other_var, probe_var, true)
    };
    // The matches of each key geometry, in the order the keys come.
    let mut groups: Vec<(&Literal, Vec<&Literal>)> = Vec::new();
    let mut position: HashMap<&Literal, usize> = HashMap::new();
    for &(p, o) in pairs {
        let (k, v) = if flip { (o, p) } else { (p, o) };
        let i = *position.entry(k).or_insert_with(|| {
            groups.push((k, Vec::new()));
            groups.len() - 1
        });
        groups[i].1.push(v);
    }
    let one = |var: &Variable, values: &mut dyn Iterator<Item = &Literal>| GraphPattern::Values {
        variables: vec![var.clone()],
        bindings: values
            .map(|l| vec![Some(GroundTerm::Literal(l.clone()))])
            .collect(),
    };
    let mut branches: Vec<GraphPattern> = groups
        .into_iter()
        .map(|(k, vs)| GraphPattern::Join {
            left: Box::new(one(key_var, &mut std::iter::once(k))),
            right: Box::new(one(value_var, &mut vs.into_iter())),
        })
        .collect();
    // A balanced union: no deep recursion over thousands of branches.
    while branches.len() > 1 {
        let mut next = Vec::with_capacity(branches.len().div_ceil(2));
        let mut it = branches.into_iter();
        while let Some(left) = it.next() {
            next.push(match it.next() {
                Some(right) => GraphPattern::Union {
                    left: Box::new(left),
                    right: Box::new(right),
                },
                None => left,
            });
        }
        branches = next;
    }
    branches.pop().unwrap_or(GraphPattern::Values {
        variables: vec![probe_var.clone(), other_var.clone()],
        bindings: Vec::new(),
    })
}

/// How a join refines its candidate pairs (see [`sweep_lazy`]).
struct Refine<'a> {
    /// Parses a geometry of the probe side, kept for the next pairs and queries.
    load_probe: &'a (dyn Fn(TermId) -> Option<Arc<Geom>> + Send + Sync),
    /// Parses a geometry of the other side, not kept.
    read_other: &'a (dyn Fn(TermId) -> Option<Arc<Geom>> + Send + Sync),
    /// The join's test, probe geometry first.
    test: &'a (dyn Fn(&Geom, &Geom) -> bool + Sync),
    cancelled: &'a (dyn Fn() -> bool + Sync),
}

/// Candidate pairs a join collects at most before testing them (16 bytes each while
/// sweeping, 8 after): beyond, it tests pairs as the sweep finds them, keeping every parsed
/// geometry until the join ends.
const MAX_CANDIDATES: usize = 64 << 20;

/// The pairs `(probe at, other at)` of geometries whose boxes meet (the probe's grown by
/// `margin`) and that pass the test, in that order.
///
/// The sweep only reads boxes and the probe side's geometries (the smaller side): a pair is
/// a candidate when a part of the probe (a point, or a segment of a line) comes near the
/// other's box, which a long line's box alone overstates. The candidates are then tested
/// grouped by the other side's geometry, in id order, on every core: each of those is read
/// once, in about the order the store keeps them, and not kept (#2). Reading them as the
/// sweep met them read the landuse polygons of OSM Europe at random and kept every one of
/// them in memory, which left too little for the page cache.
fn sweep_lazy(
    margin: Margin,
    probe: &[Lazy<'_>],
    other: &[Lazy<'_>],
    threads: usize,
    accepts: &(dyn Fn(&Lazy<'_>, &Lazy<'_>) -> bool + Sync),
    refine: &Refine<'_>,
) -> Option<Vec<(u32, u32)>> {
    type Side<'a, 's> = (Vec<&'a Lazy<'s>>, Vec<sweep::Bbox>);
    let space = |crs: &Crs| match crs {
        Crs::Other(iri) => Some(iri.clone()),
        _ => None,
    };
    let corners = |b: AABB<[f64; 2]>| (b.lower(), b.upper());
    let mut spaces: BTreeMap<Option<String>, (Side<'_, '_>, Side<'_, '_>)> = BTreeMap::new();
    for l in probe {
        let g = l.geometry;
        let (side, _) = spaces.entry(space(&g.crs)).or_default();
        side.0.push(l);
        side.1.push(corners(margin.around(&g.crs, g.bbox)));
    }
    for l in other {
        let g = l.geometry;
        if let Some((_, side)) = spaces.get_mut(&space(&g.crs)) {
            side.0.push(l);
            side.1.push(corners(Margin::None.around(&g.crs, g.bbox)));
        }
    }
    let mut out = Vec::new();
    for ((pl, pb), (ol, ob)) in spaces.into_values() {
        let near = |i: usize, j: usize| {
            if (refine.cancelled)() {
                return false;
            }
            let p = pl[i];
            p.geom(refine.load_probe)
                .is_some_and(|g| reaches(margin, &p.geometry.crs, &g.geometry, &ob[j]))
        };
        let Some(candidates) =
            sweep::join_until(&pb, &ob, threads, MAX_CANDIDATES, &near, refine.cancelled)
        else {
            if (refine.cancelled)() {
                return None;
            }
            // Too many candidates to hold: each pair is tested as the sweep finds it.
            let found = sweep::join_until(
                &pb,
                &ob,
                threads,
                usize::MAX,
                &|i, j| accepts(pl[i], ol[j]),
                refine.cancelled,
            )?;
            out.extend(found.into_iter().map(|(i, j)| (pl[i].at, ol[j].at)));
            continue;
        };
        // By the other side's geometry, in id order (`other` is sorted by id).
        let mut candidates: Vec<(u32, u32)> = candidates
            .into_iter()
            .map(|(i, j)| (j as u32, i as u32))
            .collect();
        candidates.sort_unstable();
        // Runs of one geometry of the other side, handed out in chunks of about even size.
        let chunk = candidates.len().div_ceil(threads.max(1) * 16).max(1024);
        let mut cuts = vec![0];
        while let Some(&last) = cuts.last() {
            let mut end = (last + chunk).min(candidates.len());
            while end < candidates.len() && candidates[end].0 == candidates[end - 1].0 {
                end += 1;
            }
            if end == last {
                break;
            }
            cuts.push(end);
        }
        let next = std::sync::atomic::AtomicUsize::new(0);
        let work = || {
            let mut found = Vec::new();
            loop {
                let c = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if c + 1 >= cuts.len() || (refine.cancelled)() {
                    return found;
                }
                let part = &candidates[cuts[c]..cuts[c + 1]];
                for run in part.chunk_by(|x, y| x.0 == y.0) {
                    let o = ol[run[0].0 as usize];
                    let Some(other_geom) = (refine.read_other)(o.geometry.id) else {
                        continue;
                    };
                    for &(_, i) in run {
                        let p = pl[i as usize];
                        if let Some(g) = p.geom(refine.load_probe)
                            && (refine.test)(g, &other_geom)
                        {
                            found.push((p.at, o.at));
                        }
                    }
                }
            }
        };
        let parts: Vec<Vec<(u32, u32)>> = if threads <= 1 || cuts.len() <= 2 {
            vec![work()]
        } else {
            std::thread::scope(|scope| {
                let workers: Vec<_> = (0..threads.min(cuts.len() - 1))
                    .map(|_| scope.spawn(work))
                    .collect();
                workers.into_iter().map(|w| w.join().unwrap()).collect()
            })
        };
        out.extend(parts.into_iter().flatten());
    }
    // In the same order whichever worker tested which pair.
    out.sort_unstable();
    Some(out)
}

/// Whether a part of `g` (a point, or a segment of a line) comes within `margin` of the box
/// `other`. A geometry with an area (or a part of one) may meet the box anywhere inside, so
/// it always does when its own box does.
fn reaches(margin: Margin, crs: &Crs, g: &geo::Geometry<f64>, other: &sweep::Bbox) -> bool {
    use geo::Geometry as G;
    let meets = |x0: f64, y0: f64, x1: f64, y1: f64| {
        let b = margin.around(crs, [x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)]);
        let (lo, hi) = (b.lower(), b.upper());
        lo[0] <= other.1[0] && other.0[0] <= hi[0] && lo[1] <= other.1[1] && other.0[1] <= hi[1]
    };
    let line = |ls: &geo::LineString<f64>| match ls.0.as_slice() {
        [p] => meets(p.x, p.y, p.x, p.y),
        _ => ls
            .lines()
            .any(|l| meets(l.start.x, l.start.y, l.end.x, l.end.y)),
    };
    match g {
        G::Point(p) => meets(p.x(), p.y(), p.x(), p.y()),
        G::MultiPoint(m) => m.iter().any(|p| meets(p.x(), p.y(), p.x(), p.y())),
        G::Line(l) => meets(l.start.x, l.start.y, l.end.x, l.end.y),
        G::LineString(ls) => line(ls),
        G::MultiLineString(m) => m.iter().any(line),
        G::GeometryCollection(c) => c.iter().any(|g| reaches(margin, crs, g, other)),
        _ => true,
    }
}

fn sweep_pairs<'a, K: IndexKey + Sync>(
    index: &'a SpatialIndex,
    margin: Margin,
    probe: &'a [(K, Arc<Geom>)],
    other: Option<&'a [(K, Arc<Geom>)]>,
    limit: usize,
    accepts: &(dyn Fn(&Geom, &Geom) -> bool + Sync),
) -> Option<Vec<(&'a K, &'a K)>> {
    type Side<'a, K> = (Vec<(&'a K, &'a Arc<Geom>)>, Vec<sweep::Bbox>);
    let threads = tripletrail_store::par::cores();
    let space = |crs: &Crs| match crs {
        Crs::Other(iri) => Some(iri.clone()),
        _ => None,
    };
    let boxed = |g: &Geom, margin: Margin| {
        let envelope = margin.envelope(g)?;
        Some((envelope.lower(), envelope.upper()))
    };
    let mut spaces: BTreeMap<Option<String>, (Crs, Side<'a, K>, Side<'a, K>)> = BTreeMap::new();
    let probe_boxes = sweep::par_map(probe, threads, 1024, |(_, g)| boxed(g, margin));
    for ((l, g), b) in probe.iter().zip(probe_boxes) {
        let Some(bbox) = b else { continue };
        let (_, side, _) = spaces
            .entry(space(&g.crs))
            .or_insert_with(|| (g.crs.clone(), Default::default(), Default::default()));
        side.0.push((l, g));
        side.1.push(bbox);
    }
    match other {
        Some(items) => {
            for (l, g) in items {
                let Some(bbox) = boxed(g, Margin::None) else {
                    continue;
                };
                if let Some((_, _, side)) = spaces.get_mut(&space(&g.crs)) {
                    side.0.push((l, g));
                    side.1.push(bbox);
                }
            }
        }
        None => {
            for (crs, _, side) in spaces.values_mut() {
                for e in index.entries_in(crs) {
                    let Some(key) = K::of_entry(index, e) else {
                        continue;
                    };
                    side.0.push((key, index.geom(e)));
                    side.1.push(e.bbox());
                }
            }
        }
    }
    let mut out = Vec::new();
    for (_, (pl, pb), (ol, ob)) in spaces.into_values() {
        let found = sweep::join(&pb, &ob, threads, limit - out.len(), &|i, j| {
            accepts(pl[i].1, ol[j].1)
        })?;
        out.extend(found.into_iter().map(|(i, j)| (pl[i].0, ol[j].0)));
    }
    Some(out)
}

struct Rewriter<'a> {
    ctx: &'a dyn SpatialContext,
    loaded: Option<Option<Arc<SpatialIndex>>>,
    stats: RewriteStats,
    /// Inside `GRAPH`, or the query has a dataset: side evaluation on the default graph would
    /// not see the same data, so only rewrites that do not evaluate anything apply.
    scoped: usize,
    fresh: usize,
    estimator: Estimator<'a>,
    /// The rows of the join sides evaluated by [`Self::side_values`], by geometry variable:
    /// the side's variables and its rows grouped by geometry literal.
    side_rows: HashMap<Variable, SideRows>,
    /// How often each variable is written in the query (see [`mentions`]); empty when not
    /// known, and then every variable counts as used.
    mentions: HashMap<String, usize>,
    /// Inside an `EXISTS`: its pattern is evaluated once per outer row with that row's values
    /// bound, so relation triples become plain geometry lookups and a test there, without the
    /// index (whose candidates would be found once per row, for the whole store).
    exists: usize,
}

/// How often each variable name is written in `query`.
fn mentions(query: &Query) -> HashMap<String, usize> {
    let text = query.to_string();
    let mut out = HashMap::new();
    let mut chars = text.char_indices().peekable();
    let mut in_string = None;
    while let Some((i, c)) = chars.next() {
        match (in_string, c) {
            (Some(q), '\\') if q != '<' => {
                chars.next();
            }
            (Some(q), c) if c == q || (q == '<' && c == '>') => in_string = None,
            (Some(_), _) => {}
            // `<` opens an IRI, unless it is the comparison (`<` or `<=`, spaced by Display).
            (None, '<') if !matches!(chars.peek(), Some((_, ' ' | '='))) => in_string = Some(c),
            (None, '"' | '\'') => in_string = Some(c),
            (None, '?' | '$') => {
                let start = i + 1;
                let mut end = start;
                while let Some(&(j, d)) = chars.peek() {
                    if d.is_alphanumeric() || d == '_' {
                        end = j + d.len_utf8();
                        chars.next();
                    } else {
                        break;
                    }
                }
                if end > start {
                    *out.entry(text[start..end].to_owned()).or_insert(0) += 1;
                }
            }
            _ => {}
        }
    }
    out
}

/// The solutions of one join side, grouped by the side's geometry.
struct SideRows {
    pattern: GraphPattern,
    rows: usize,
    variables: Vec<Variable>,
    by_geometry: HashMap<Literal, Vec<Vec<Option<GroundTerm>>>>,
}

/// Rows a materialized join block may have; past it the pairs block is used.
const MAX_MATERIALIZED_ROWS: usize = 4_000_000;
/// Estimated rows past which a join side is evaluated for its geometries only, not its rows.
const MAX_SIDE_ROWS: f64 = 100_000.0;
/// How many pairs, on average, each row of a side must be in for the block to bind that
/// side's rows: below it, looking the side up from each pair's geometry is cheaper than the
/// evaluator encoding the side's values in every row of the block.
const MIN_REUSE: usize = 4;

impl<'a> Rewriter<'a> {
    fn new(ctx: &'a dyn SpatialContext) -> Self {
        Self {
            ctx,
            loaded: None,
            stats: RewriteStats::default(),
            scoped: 0,
            fresh: 0,
            estimator: Estimator::new(ctx),
            side_rows: HashMap::new(),
            mentions: HashMap::new(),
            exists: 0,
        }
    }

    fn index(&mut self) -> Option<Arc<SpatialIndex>> {
        let ctx = self.ctx;
        Some(Arc::clone(
            self.loaded.get_or_insert_with(|| ctx.index()).as_ref()?,
        ))
    }

    fn fresh(&mut self) -> Variable {
        self.fresh += 1;
        Variable::new_unchecked(format!("_tripletrail_w{}", self.fresh))
    }

    /// Whether the context gives estimates: the cost model makes the choices.
    fn costed(&mut self) -> bool {
        let all_graphs = self.scoped > 0;
        let t = triple(
            TermPattern::Variable(Variable::new_unchecked("s")),
            AS_WKT,
            TermPattern::Variable(Variable::new_unchecked("o")),
        );
        self.estimator.triple(&t, all_graphs).is_some()
    }

    /// Estimated rows of a pattern, counting every graph inside `GRAPH` or a dataset.
    fn rows(&mut self, p: &GraphPattern) -> Option<f64> {
        let all_graphs = self.scoped > 0;
        self.estimator.rows(p, all_graphs)
    }

    /// The rows of the operands binding a variable: for triples alone, as the store follows
    /// the joins through their fanouts, where it can (#209); otherwise [`Self::rows`].
    fn bound_rows(&mut self, p: &GraphPattern) -> Option<f64> {
        let mut parts = Vec::new();
        cost::join_parts(p, &mut parts);
        let triples: Option<Vec<TriplePattern>> = parts
            .into_iter()
            .map(|part| match part {
                GraphPattern::Bgp { mut patterns } if patterns.len() == 1 => patterns.pop(),
                _ => None,
            })
            .collect();
        if let Some(triples) = triples
            && let Some(rows) = self.ctx.estimate_join(&triples, self.scoped > 0)
        {
            return Some(rows);
        }
        self.rows(p)
    }

    /// Whether a constant test of `var` is worth a `VALUES` block: the index would return
    /// few candidates next to the rows the operands binding `var` produce (otherwise the
    /// block costs more than the FILTER it saves). Always, without estimates.
    fn worth_injecting(
        &mut self,
        plan: &Plan,
        var: &Variable,
        constant: &Term,
        inner: &GraphPattern,
    ) -> bool {
        let Some(rows) = component(inner, var).0.and_then(|p| self.bound_rows(&p)) else {
            return true;
        };
        let Some(geom) = parse_wkt_literal(constant) else {
            return true;
        };
        let Some(envelope) = plan.margin.envelope(&geom) else {
            return true;
        };
        let Some(index) = self.index() else {
            return true;
        };
        let start = Instant::now();
        let candidates = index.estimate_candidates(&geom.crs, envelope);
        self.stats.lookup += start.elapsed();
        let inject =
            candidates <= cost::ALWAYS_INJECT || candidates as f64 <= cost::INJECT_RATIO * rows;
        self.stats.skipped += usize::from(!inject);
        self.stats.decisions.push(Decision::Constant {
            var: var.clone(),
            candidates,
            rows,
            inject,
        });
        inject
    }

    /// The indexed geometries passing a constant test.
    fn constant_candidates(&mut self, plan: &Plan, constant: &Term) -> Option<Vec<Literal>> {
        let geom = parse_wkt_literal(constant)?;
        let envelope = plan.margin.envelope(&geom)?;
        let index = self.index()?;
        let start = Instant::now();
        let candidates: Vec<_> = index.candidates(&geom.crs, envelope).collect();
        let threads = tripletrail_store::par::cores();
        let ctx = self.ctx;
        let found: Vec<Literal> = sweep::par_map(&candidates, threads, 256, |e| {
            (!ctx.cancelled() && plan.accepts_one(index.geom(e))).then(|| index.literal(e).clone())
        })
        .into_iter()
        .flatten()
        .collect();
        self.stats.lookup += start.elapsed();
        if ctx.cancelled() {
            return None;
        }
        self.stats.filters += 1;
        self.stats.candidates += found.len();
        Some(found)
    }

    /// The geometries a join side can take: from a constant test of the same FILTER, or by
    /// evaluating the side on its own (`side`, when it shares no variable with the other side).
    /// `Some(None)` when neither is possible; `None` when the evaluation fails.
    fn side_values(
        &mut self,
        var: &Variable,
        side: Option<&GraphPattern>,
        known: &HashMap<Variable, LiteralSet>,
    ) -> Option<Option<LiteralSet>> {
        if let Some(k) = known.get(var) {
            return Some(Some(k.clone()));
        }
        if self.scoped > 0 {
            return Some(None);
        }
        let Some(p) = side else {
            return Some(None);
        };
        self.stats.sides_evaluated += 1;
        let small = self.rows(p).is_some_and(|r| r <= MAX_SIDE_ROWS);
        if let Some(rows) = small.then(|| self.side_rows_of(p, var)).flatten() {
            let set = rows.by_geometry.keys().cloned().collect();
            self.side_rows.insert(var.clone(), rows);
            return Some(Some(set));
        }
        let values = self.ctx.values_of(p, var)?;
        Some(Some(
            values
                .into_iter()
                .filter_map(|t| match t {
                    Term::Literal(l) => Some(l),
                    _ => None,
                })
                .collect(),
        ))
    }

    /// The rows of a join side, if the context gives them and every value fits in `VALUES`
    /// (no blank nodes, `var` bound to a literal).
    fn side_rows_of(&self, pattern: &GraphPattern, var: &Variable) -> Option<SideRows> {
        let mut variables: Vec<Variable> = in_scope(pattern).into_iter().collect();
        variables.sort_by(|x, y| x.as_str().cmp(y.as_str()));
        let at = variables.iter().position(|v| v == var)?;
        let rows = self.ctx.rows_of(pattern, &variables)?;
        if rows.len() > MAX_MATERIALIZED_ROWS {
            return None;
        }
        let mut by_geometry: HashMap<Literal, Vec<Vec<Option<GroundTerm>>>> = HashMap::new();
        for row in rows {
            let Some(Term::Literal(geometry)) = row.get(at).cloned().flatten() else {
                return None;
            };
            let row = row
                .into_iter()
                .map(|t| match t {
                    None => Some(None),
                    Some(Term::NamedNode(n)) => Some(Some(GroundTerm::NamedNode(n))),
                    Some(Term::Literal(l)) => Some(Some(GroundTerm::Literal(l))),
                    Some(_) => None,
                })
                .collect::<Option<Vec<_>>>()?;
            by_geometry.entry(geometry).or_default().push(row);
        }
        Some(SideRows {
            pattern: pattern.clone(),
            rows: by_geometry.values().map(Vec::len).sum(),
            variables,
            by_geometry,
        })
    }

    /// One `VALUES` block for the pairs that binds, for each side whose rows are each in
    /// several pairs, every variable of that side from its rows: those sides need not be
    /// looked up again for each pair, and their triples are dropped (returned). A side whose
    /// rows are mostly in one pair each keeps its triples, looked up from its geometry.
    /// `None` if no side qualifies (or the block would be too large).
    fn materialized(
        &mut self,
        (probe_var, other_var): (&Variable, &Variable),
        pairs: &[(&Literal, &Literal)],
    ) -> Option<(GraphPattern, Vec<GraphPattern>)> {
        let reused = |var: &Variable| {
            self.side_rows
                .get(var)
                .filter(|r| r.rows.saturating_mul(MIN_REUSE) <= pairs.len())
        };
        let sides = [reused(probe_var), reused(other_var)];
        if sides.iter().all(Option::is_none) {
            return None;
        }
        let mut total = 0usize;
        for (a, b) in pairs {
            let count = |side: Option<&SideRows>, l: &Literal| {
                side.map_or(1, |r| r.by_geometry.get(l).map_or(0, Vec::len))
            };
            total += count(sides[0], a) * count(sides[1], b);
            if total > MAX_MATERIALIZED_ROWS {
                return None;
            }
        }
        // A geometry written only in its side's triple and in the test answered here is not
        // needed further once the triple is dropped: leaving it out saves copying (and the
        // evaluator encoding) a long literal per row.
        let needed = |v: &Variable| {
            !((v == probe_var || v == other_var) && self.mentions.get(v.as_str()) == Some(&2))
        };
        let keep: Vec<Vec<bool>> = sides
            .iter()
            .map(|s| s.map_or_else(Vec::new, |r| r.variables.iter().map(needed).collect()))
            .collect();
        let pick = |row: &[Option<GroundTerm>], keep: &[bool]| {
            row.iter()
                .zip(keep)
                .filter(|(_, k)| **k)
                .map(|(t, _)| t.clone())
                .collect::<Vec<_>>()
        };
        let rows_of = |i: usize, l: &Literal| -> Vec<Vec<Option<GroundTerm>>> {
            match sides[i] {
                Some(r) => r.by_geometry.get(l).map_or_else(Vec::new, |rows| {
                    rows.iter().map(|x| pick(x, &keep[i])).collect()
                }),
                None => vec![vec![Some(GroundTerm::Literal(l.clone()))]],
            }
        };
        let mut bindings = Vec::with_capacity(total);
        for (a, b) in pairs {
            let (ra, rb) = (rows_of(0, a), rows_of(1, b));
            for x in &ra {
                for y in &rb {
                    bindings.push(x.iter().chain(y).cloned().collect());
                }
            }
        }
        let mut variables = Vec::new();
        let mut consumed = Vec::new();
        for (i, var) in [probe_var, other_var].into_iter().enumerate() {
            match sides[i] {
                Some(r) => {
                    variables.extend(
                        r.variables
                            .iter()
                            .zip(&keep[i])
                            .filter(|(_, k)| **k)
                            .map(|(v, _)| v.clone()),
                    );
                    consumed.push(r.pattern.clone());
                }
                None => variables.push(var.clone()),
            }
        }
        self.stats.materialized += 1;
        Some((
            GraphPattern::Values {
                variables,
                bindings,
            },
            consumed,
        ))
    }

    /// `VALUES (?a ?b) { ... }` with the pairs of geometries passing a two-variable test.
    /// `left` are the constant tests of the same FILTER left to it by the cost model.
    /// [`Self::join_values`] on term ids, when the context evaluates both sides so: the
    /// sides' rows are joined through the pairs of their geometries that pass the test, and
    /// the result goes to the evaluation as a table, in place of both sides. `None` if the
    /// context does not offer it (or the result is too large to hold); `Some(None)` once
    /// the query is cancelled.
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    fn join_on_ids(
        &mut self,
        plan: &Plan,
        (a, b): (&Variable, &Variable),
        (pa, pb): (&GraphPattern, &GraphPattern),
        left: &[(&Plan, Variable)],
        start: Instant,
    ) -> Option<Option<(GraphPattern, Vec<GraphPattern>)>> {
        // Without the sweep-line join, joins probe the index (see `SpatialContext::sweep_join`).
        if self.scoped > 0 || !self.ctx.sweep_join() {
            return None;
        }
        // A side the context stopped reading because the query was cancelled (its timeout)
        // ends the rewrite here: the other ways of joining would evaluate the sides again,
        // which on large sides runs far past the timeout (#338).
        let Some(side_a) = self.ctx.side_ids(pa, a) else {
            return self.ctx.cancelled().then_some(None);
        };
        let Some(side_b) = self.ctx.side_ids(pb, b) else {
            return self.ctx.cancelled().then_some(None);
        };
        // Rows and geometries are numbered in 32 bits below.
        if [&side_a, &side_b]
            .iter()
            .any(|s| s.rows.len() >= u32::MAX as usize || s.geometries.len() >= u32::MAX as usize)
        {
            return None;
        }
        debug_assert!(
            [&side_a, &side_b]
                .iter()
                .all(|s| s.geometries.is_sorted_by_key(|g| g.id))
        );
        self.stats.sides_evaluated += 2;
        let threads = tripletrail_store::par::cores();
        // The constant tests left to the FILTER hold in every row too.
        // Geometries are keyed by their place in the side's `geometries`.
        // Geometries are parsed only for a test: a constant one here, the join's below.
        fn keep<'s>(side: &'s IdSide, tests: &[&Plan], threads: usize) -> Vec<Lazy<'s>> {
            let items: Vec<Lazy<'s>> = side
                .geometries
                .iter()
                .enumerate()
                .map(|(i, geometry)| Lazy {
                    at: i as u32,
                    geometry,
                    parsed: std::sync::OnceLock::new(),
                })
                .collect();
            if tests.is_empty() {
                return items;
            }
            let passes = sweep::par_map(&items, threads, 256, |l| {
                l.geom(&*side.load)
                    .is_some_and(|g| tests.iter().all(|p| p.accepts_one(g)))
            });
            items
                .into_iter()
                .zip(passes)
                .filter_map(|(l, ok)| ok.then_some(l))
                .collect()
        }
        let tests = |var: &Variable| -> Vec<&Plan> {
            left.iter()
                .filter(|(_, v)| v == var)
                .map(|(p, _)| *p)
                .collect()
        };
        let (geoms_a, geoms_b) = (
            keep(&side_a, &tests(a), threads),
            keep(&side_b, &tests(b), threads),
        );
        // The smaller side probes, grown by the test's margin.
        let probe_a = geoms_a.len() <= geoms_b.len();
        let (probe, other) = if probe_a {
            (&geoms_a, &geoms_b)
        } else {
            (&geoms_b, &geoms_a)
        };
        let probe_var = if probe_a { a } else { b };
        let (load_probe, load_other, read_other) = if probe_a {
            (&*side_a.load, &*side_b.load, &*side_b.read)
        } else {
            (&*side_b.load, &*side_a.load, &*side_a.read)
        };
        let ctx = self.ctx;
        let accepts = |p: &Lazy<'_>, o: &Lazy<'_>| {
            if ctx.cancelled() {
                return false;
            }
            let (Some(probe_geom), Some(other_geom)) = (p.geom(load_probe), o.geom(load_other))
            else {
                return false;
            };
            plan.accepts(&|v: &Variable| {
                if v == probe_var {
                    probe_geom
                } else {
                    other_geom
                }
            })
        };
        let pairing = Instant::now();
        let test = |probe_geom: &Geom, other_geom: &Geom| {
            plan.accepts(&|v: &Variable| {
                if v == probe_var {
                    probe_geom
                } else {
                    other_geom
                }
            })
        };
        let cancelled = || ctx.cancelled();
        let refine = Refine {
            load_probe,
            read_other,
            test: &test,
            cancelled: &cancelled,
        };
        let found = sweep_lazy(plan.margin, probe, other, threads, &accepts, &refine);
        self.stats.pairing += pairing.elapsed();
        let Some(found) = found else {
            self.stats.lookup += start.elapsed();
            return Some(None);
        };
        if ctx.cancelled() {
            self.stats.lookup += start.elapsed();
            return Some(None);
        }
        self.stats.sweeps += 1;
        self.stats.joins += 1;
        self.stats.candidates += found.len();
        // The rows of each side by geometry, then a row per pair of rows.
        let (rows_a, rows_b) = (
            RowsByGeometry::new(&side_a, threads),
            RowsByGeometry::new(&side_b, threads),
        );
        let (wa, wb) = (side_a.rows.width, side_b.rows.width);
        let width = wa + wb;
        let pair = |&(p, o): &(u32, u32)| if probe_a { (p, o) } else { (o, p) };
        let mut total = 0usize;
        for f in &found {
            let (ga, gb) = pair(f);
            total += rows_a.of(ga).len() * rows_b.of(gb).len();
        }
        if total.saturating_mul(width) > MAX_TABLE_IDS {
            self.stats.lookup += start.elapsed();
            return None;
        }
        let mut ids = Vec::with_capacity(total * width);
        for f in &found {
            let (ga, gb) = pair(f);
            let rb = rows_b.of(gb);
            for &x in rows_a.of(ga) {
                let x = x as usize;
                for &y in rb {
                    let y = y as usize;
                    ids.extend_from_slice(&side_a.rows.ids[x * wa..(x + 1) * wa]);
                    ids.extend_from_slice(&side_b.rows.ids[y * wb..(y + 1) * wb]);
                }
            }
        }
        let mut variables = side_a.variables.clone();
        variables.extend(side_b.variables.iter().cloned());
        let table = self.ctx.table(&variables, IdRows { width, ids });
        self.stats.lookup += start.elapsed();
        let table = table?;
        if self.costed() {
            self.stats.decisions.push(Decision::IdJoin {
                a: a.clone(),
                b: b.clone(),
                rows_a: side_a.rows.len(),
                rows_b: side_b.rows.len(),
                probe: probe_var.clone(),
                pairs: found.len(),
                rows: total,
            });
        }
        self.stats.materialized += 1;
        Some(Some((table, vec![pa.clone(), pb.clone()])))
    }

    fn join_values(
        &mut self,
        plan: &Plan,
        (a, b): (&Variable, &Variable),
        inner: &GraphPattern,
        known: &HashMap<Variable, LiteralSet>,
        left: &[(&Plan, Variable)],
    ) -> Option<(GraphPattern, Vec<GraphPattern>)> {
        let start = Instant::now();
        self.side_rows.clear();
        let (side_a, side_b) = (side(inner, a, b), side(inner, b, a));
        // On term ids, the join reads the sides' boxes, not the index.
        if !known.contains_key(a)
            && !known.contains_key(b)
            && let (Some(pa), Some(pb)) = (&side_a, &side_b)
            && let Some(found) = self.join_on_ids(plan, (a, b), (pa, pb), left, start)
        {
            return found;
        }
        let index = self.index()?;
        // Sizes before evaluating anything: exact after a constant test, else estimated.
        let mut size = |var: &Variable, side: &Option<GraphPattern>| match known.get(var) {
            Some(k) => Some(k.len() as f64),
            None => self.rows(side.as_ref()?),
        };
        let (rows_a, rows_b) = (size(a, &side_a), size(b, &side_b));
        // One side clearly smaller: probe from it, evaluated if need be, without evaluating
        // the other, whose candidates the final join checks anyway.
        let clear = match (rows_a, rows_b) {
            (Some(x), Some(y)) if x * cost::CLEAR_RATIO <= y => Some(true),
            (Some(x), Some(y)) if y * cost::CLEAR_RATIO <= x => Some(false),
            _ => None,
        };
        let evaluated_before = self.stats.sides_evaluated;
        let mut by_estimate = false;
        let mut chosen = None;
        if let Some(probe_a) = clear {
            let (var, side) = if probe_a { (a, &side_a) } else { (b, &side_b) };
            if let Some(set) = self.side_values(var, side.as_ref(), known)? {
                by_estimate = true;
                // The other side is not needed to probe, but if it is much smaller than the
                // index, its geometries are worth knowing: candidates outside it (other
                // classes of features in the same place) are then skipped before their exact
                // test instead of after it, in the final join.
                let (other_var, other_side, other_rows) = if probe_a {
                    (b, &side_b, rows_b)
                } else {
                    (a, &side_a, rows_a)
                };
                let narrow = other_rows
                    .is_some_and(|r| r * cost::CLEAR_RATIO <= index.stats().geometries as f64);
                let other_set = match known.get(other_var) {
                    Some(k) => Some(k.clone()),
                    None if narrow => self.side_values(other_var, other_side.as_ref(), known)?,
                    None => None,
                };
                chosen = Some(if probe_a {
                    (Some(set), other_set)
                } else {
                    (other_set, Some(set))
                });
            }
        }
        let (sa, sb) = match chosen {
            Some(c) => c,
            // A side narrowed by a constant test is usually small: probe from it without
            // evaluating the other side, whose candidates the final join checks anyway.
            None => match (known.get(a), known.get(b)) {
                (Some(k), None) => (Some(k.clone()), None),
                (None, Some(k)) => (None, Some(k.clone())),
                _ => (
                    self.side_values(a, side_a.as_ref(), known)?,
                    self.side_values(b, side_b.as_ref(), known)?,
                ),
            },
        };
        let probe_a = match (&sa, &sb) {
            (Some(va), Some(vb)) => {
                // With a clear estimate the chosen side probes; else the smaller one.
                if by_estimate {
                    clear == Some(true)
                } else {
                    va.len() <= vb.len()
                }
            }
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => true,
        };
        let (probe_set, other) = if probe_a { (sa, sb) } else { (sb, sa) };
        let (probe_var, other_var) = if probe_a { (a, b) } else { (b, a) };
        let global = probe_set.is_none();
        let limit = if global { MAX_GLOBAL_PAIRS } else { usize::MAX };
        // The geometries, parsed once (the probe side, and the other side if it is known).
        let mut probe: Vec<(Literal, Arc<Geom>)> = match &probe_set {
            Some(set) => parsed(self.ctx, set.iter()),
            // All indexed geometries on both sides.
            None => index
                .entries()
                .map(|e| (index.literal(e).clone(), Arc::clone(index.geom(e))))
                .collect(),
        };
        // The constant tests left to the FILTER hold in every row too: geometries failing them
        // need not be probed, nor matched.
        let passes = |var: &Variable, g: &Geom| {
            left.iter()
                .filter(|(_, v)| v == var)
                .all(|(p, _)| p.accepts_one(g))
        };
        let threads = tripletrail_store::par::cores();
        if left.iter().any(|(_, v)| v == probe_var) {
            let keep = sweep::par_map(&probe, threads, 256, |(_, g)| passes(probe_var, g));
            let mut keep = keep.into_iter();
            probe.retain(|_| keep.next().unwrap_or(false));
        }
        let mut other_items: Option<Vec<(Literal, Arc<Geom>)>> =
            other.as_ref().map(|o| parsed(self.ctx, o.iter()));
        if let Some(items) = other_items.as_mut()
            && left.iter().any(|(_, v)| v == other_var)
        {
            let keep = sweep::par_map(items, threads, 256, |(_, g)| passes(other_var, g));
            let mut keep = keep.into_iter();
            items.retain(|_| keep.next().unwrap_or(false));
        }
        let other_set: Option<LiteralSet> = other_items
            .as_ref()
            .map(|items| items.iter().map(|(l, _)| l.clone()).collect());
        // Without a set to check against, the candidates are tested as they come.
        let test_candidates = other.is_none() && left.iter().any(|(_, v)| v == other_var);
        // Both sides large: sweep them, else probe the index with each geometry of the
        // smaller one. Without a set, the other side is every indexed geometry.
        let other_len = other_items
            .as_ref()
            .map_or_else(|| index.stats().geometries, Vec::len);
        let costed = self.costed();
        let sweep = costed && self.ctx.sweep_join() && cost::sweep(probe.len(), other_len);
        if costed {
            self.stats.decisions.push(Decision::Join {
                a: a.clone(),
                b: b.clone(),
                rows_a,
                rows_b,
                probe: probe_var.clone(),
                by_estimate,
                evaluated: self.stats.sides_evaluated - evaluated_before,
                sweep,
            });
        }
        // The exact test of a candidate pair on the parsed geometries, after the constant
        // tests left to the FILTER if the candidate comes from the index unchecked.
        let ctx = self.ctx;
        let accepts = |probe_geom: &Geom, other_geom: &Geom| {
            // Once cancelled, every pair is rejected: the loops end fast and the block is
            // dropped below.
            if ctx.cancelled() {
                return false;
            }
            if test_candidates && !passes(other_var, other_geom) {
                return false;
            }
            plan.accepts(&|v: &Variable| {
                if v == probe_var {
                    probe_geom
                } else {
                    other_geom
                }
            })
        };
        let pairing = Instant::now();
        let pairs: Vec<(&Literal, &Literal)> = if sweep {
            let limit = if global { MAX_SWEEP_PAIRS } else { usize::MAX };
            let Some(found) = sweep_pairs(
                &index,
                plan.margin,
                &probe,
                other_items.as_deref(),
                limit,
                &accepts,
            ) else {
                self.stats.lookup += start.elapsed();
                self.stats.pairing += pairing.elapsed();
                return None;
            };
            self.stats.sweeps += 1;
            found
        } else {
            // Index nested loop: the candidates of every probe geometry, then their exact
            // tests on every core.
            let mut candidates: Vec<(usize, &crate::index::Entry)> = Vec::new();
            for (i, (_, geom)) in probe.iter().enumerate() {
                if i % 1024 == 0 && ctx.cancelled() {
                    break;
                }
                let Some(envelope) = plan.margin.envelope(geom) else {
                    continue;
                };
                for e in index.candidates(&geom.crs, envelope) {
                    if other_set
                        .as_ref()
                        .is_some_and(|o| !o.contains(index.literal(e)))
                    {
                        continue;
                    }
                    candidates.push((i, e));
                    if candidates.len() > limit {
                        self.stats.lookup += start.elapsed();
                        self.stats.pairing += pairing.elapsed();
                        return None;
                    }
                }
            }
            sweep::par_map(&candidates, threads, 64, |&(i, e)| {
                accepts(&probe[i].1, index.geom(e)).then_some((&probe[i].0, index.literal(e)))
            })
            .into_iter()
            .flatten()
            .collect()
        };
        self.stats.lookup += start.elapsed();
        self.stats.pairing += pairing.elapsed();
        if ctx.cancelled() {
            return None;
        }
        self.stats.joins += 1;
        self.stats.candidates += pairs.len();
        let found = match self.materialized((probe_var, other_var), &pairs) {
            Some(found) => found,
            None => (pairs_pattern((probe_var, other_var), &pairs), Vec::new()),
        };
        self.side_rows.clear();
        Some(found)
    }

    /// Puts a lone candidate block before the triples that bind its variables, joined to them
    /// with `LATERAL` so each candidate is looked up in turn, when the estimates say the block
    /// is smaller than the triple Oxigraph's optimizer would start from, and the optimizer
    /// would not start from the block itself (it guesses triple sizes from their constants
    /// alone, so a block of a few thousand rows loses to `?x a ex:Class`). Only plain triples
    /// are moved under the `LATERAL`, where binding the block's variables first cannot change
    /// their solutions.
    /// With `force`, the block is pinned whatever the estimates.
    fn pin(
        &mut self,
        blocks: Vec<GraphPattern>,
        inner: GraphPattern,
        force: bool,
    ) -> (Vec<GraphPattern>, GraphPattern) {
        let [
            GraphPattern::Values {
                variables,
                bindings,
            },
        ] = blocks.as_slice()
        else {
            return (blocks, inner);
        };
        let rows = bindings.len();
        let mut parts = Vec::new();
        cost::join_parts(&inner, &mut parts);
        let start: HashSet<cost::Name> = variables.iter().map(cost::var_name).collect();
        let (taken, _) = cost::connected(&parts, &start);
        let mut triples = Vec::new();
        let mut rest = Vec::new();
        for (part, taken) in parts.into_iter().zip(taken) {
            match part {
                GraphPattern::Bgp { patterns } if taken && patterns.len() == 1 => {
                    triples.extend(patterns);
                }
                _ if taken => return (blocks, inner),
                other => rest.push(other),
            }
        }
        // The optimizer starts from the triple it guesses smallest (the first of equals).
        let Some(first) = triples
            .iter()
            .min_by_key(|t| cost::optimizer_guess(t))
            .cloned()
        else {
            return (blocks, inner);
        };
        if !force && rows <= cost::optimizer_guess(&first) {
            return (blocks, inner);
        }
        let all_graphs = self.scoped > 0;
        let mut first_rows = match self.estimator.triple(&first, all_graphs) {
            Some(r) => r,
            None if force => f64::NAN,
            None => return (blocks, inner),
        };
        // Triples that only the block connects (the two sides of a join): the optimizer
        // joins each group on its own and then takes their cartesian product.
        let groups = self.unconnected_rows(&triples, all_graphs);
        if groups.len() > 1 {
            first_rows = groups.iter().product();
        }
        if !force && rows as f64 >= first_rows {
            return (blocks, inner);
        }
        self.stats.pinned += 1;
        if !first_rows.is_nan() {
            self.stats.decisions.push(Decision::Pin {
                variables: variables.clone(),
                rows,
                start: first.to_string(),
                start_rows: first_rows,
            });
        }
        let block = blocks.into_iter().next().unwrap();
        let pinned = GraphPattern::Lateral {
            left: Box::new(block),
            right: Box::new(bgp(triples)),
        };
        let rest = cost::rejoin(rest).unwrap_or_else(|| bgp(Vec::new()));
        (vec![pinned], rest)
    }

    /// Estimated rows of each group of `triples` connected through shared names (`[]` if
    /// one cannot be estimated).
    fn unconnected_rows(&mut self, triples: &[TriplePattern], all_graphs: bool) -> Vec<f64> {
        let parts: Vec<GraphPattern> = triples.iter().map(|t| bgp(vec![t.clone()])).collect();
        let mut done = vec![false; parts.len()];
        let mut out = Vec::new();
        while let Some(i) = done.iter().position(|d| !d) {
            let (group, _) = cost::connected(&parts, &cost::names(&parts[i]));
            let members: Vec<GraphPattern> = parts
                .iter()
                .zip(&group)
                .filter(|(_, g)| **g)
                .map(|(p, _)| p.clone())
                .collect();
            for (d, g) in done.iter_mut().zip(&group) {
                *d |= *g;
            }
            let Some(rows) =
                cost::rejoin(members).and_then(|p| self.estimator.rows(&p, all_graphs))
            else {
                return Vec::new();
            };
            out.push(rows);
        }
        out
    }

    fn filter(&mut self, expr: Expression, inner: GraphPattern) -> GraphPattern {
        let mut found = Vec::new();
        plans(&expr, &mut found);
        // Constant tests first: they are cheaper, and their candidates narrow the joins.
        found.sort_by_key(|p| matches!(p.shape(), Some(Shape::Join { .. })));
        let mut known: HashMap<Variable, LiteralSet> = HashMap::new();
        let mut left = Vec::new();
        let mut blocks = Vec::new();
        // The join sides a materialized block binds: dropped from `inner`.
        let mut consumed: Vec<GraphPattern> = Vec::new();
        // Conjuncts whose candidates are all in a VALUES block, each tested exactly: every
        // row the block lets through passes them, so the FILTER need not test them again.
        let mut answered: Vec<&Expression> = Vec::new();
        // No spatial test, no index, or a cancelled query: nothing to rewrite (and no index to
        // build).
        let predicates = if found.is_empty() || self.ctx.cancelled() {
            None
        } else {
            self.ctx
                .indexed_predicates()
                .or_else(|| self.index().map(|i| i.predicates().clone()))
        };
        let Some(predicates) = predicates else {
            return GraphPattern::Filter {
                expr,
                inner: Box::new(inner),
            };
        };
        let binds = |var: &Variable| binds_geometry(&inner, var, &predicates);
        for plan in &found {
            match plan.shape() {
                Some(Shape::Constant { var, constant }) if binds(&var) => {
                    if !self.worth_injecting(plan, &var, &constant, &inner) {
                        left.push((plan, var));
                        continue;
                    }
                    if let Some(found) = self.constant_candidates(plan, &constant) {
                        answered.push(&plan.conjunct);
                        let set: LiteralSet = found.into_iter().collect();
                        let set = match known.remove(&var) {
                            Some(prev) => prev.intersection(&set).cloned().collect(),
                            None => set,
                        };
                        known.insert(var, set);
                    }
                }
                Some(Shape::Join { a, b }) if binds(&a) && binds(&b) => {
                    if let Some((values, sides)) =
                        self.join_values(plan, (&a, &b), &inner, &known, &left)
                    {
                        answered.push(&plan.conjunct);
                        blocks.push(values);
                        consumed = sides;
                        // One pair block is enough; further joins are checked by the FILTER.
                        break;
                    }
                }
                _ => {}
            }
        }
        let mut known: Vec<(Variable, LiteralSet)> = known.into_iter().collect();
        known.sort_by(|x, y| x.0.as_str().cmp(y.0.as_str()));
        for (var, set) in known {
            let mut set: Vec<Literal> = set.into_iter().collect();
            set.sort_by(|x, y| x.value().cmp(y.value()));
            blocks.insert(
                0,
                GraphPattern::Values {
                    variables: vec![var],
                    bindings: set
                        .into_iter()
                        .map(|l| vec![Some(GroundTerm::Literal(l))])
                        .collect(),
                },
            );
        }
        let inner = if blocks.is_empty() {
            inner
        } else {
            // A block that binds one side from its rows has a row per pair: the other side is
            // looked up from it, never the block scanned per row of that side.
            let force = !consumed.is_empty();
            let inner = if force {
                without_parts(inner, &consumed)
            } else {
                inner
            };
            let (blocks, inner) = self.pin(blocks, inner, force);
            join_after(blocks, inner)
        };
        self.stats.answered += answered.len();
        match without_conjuncts(expr.clone(), &answered) {
            Some(expr) => GraphPattern::Filter {
                expr,
                inner: Box::new(inner),
            },
            None => inner,
        }
    }

    /// The geometry literal(s) of a feature or geometry `node`, bound to `w`.
    fn geometry_of(&mut self, node: &TermPattern, w: &Variable) -> GraphPattern {
        let wv = TermPattern::Variable(w.clone());
        let g = TermPattern::Variable(self.fresh());
        let d = TermPattern::Variable(self.fresh());
        let direct = bgp(vec![triple(node.clone(), AS_WKT, wv.clone())]);
        let default = bgp(vec![
            triple(node.clone(), &format!("{GEO}hasDefaultGeometry"), g.clone()),
            triple(g.clone(), AS_WKT, wv.clone()),
        ]);
        // Any geometry, for features without a default geometry.
        let geometries = bgp(vec![
            triple(node.clone(), &format!("{GEO}hasGeometry"), g.clone()),
            triple(g, AS_WKT, wv),
        ]);
        let defaults = bgp(vec![triple(
            node.clone(),
            &format!("{GEO}hasDefaultGeometry"),
            d,
        )]);
        // Inside EXISTS, `node` is usually bound by the outer row: a lookup is the cheaper.
        let any = if matches!(node, TermPattern::Variable(_)) && self.exists == 0 {
            // MINUS rather than FILTER NOT EXISTS: one anti-join instead of a lookup per
            // row (100x faster on large joins). They agree because `node` is on both sides.
            GraphPattern::Minus {
                left: Box::new(geometries),
                right: Box::new(defaults),
            }
        } else {
            // A constant shares no variable, so MINUS would remove nothing.
            GraphPattern::Filter {
                expr: Expression::Not(Box::new(Expression::Exists(Box::new(defaults)))),
                inner: Box::new(geometries),
            }
        };
        union(union(direct, default), any)
    }

    fn relation_call(relation: &str, a: &Variable, b: &Variable) -> Expression {
        Expression::FunctionCall(
            Function::Custom(oxigraph::model::NamedNode::new_unchecked(format!(
                "{GEOF}{relation}"
            ))),
            vec![
                Expression::Variable(a.clone()),
                Expression::Variable(b.clone()),
            ],
        )
    }

    /// A BGP with GeoSPARQL relation triples (`?a geo:sfIntersects ?b`), per the Query Rewrite
    /// Extension: a relation holds between features (through their default geometry, or any
    /// geometry if they have none) and between geometries. The other triples stay in the same
    /// group, so they restrict which geometries the spatial test (and its index lookup) sees.
    fn bgp(&mut self, patterns: Vec<TriplePattern>) -> GraphPattern {
        let (virtuals, patterns): (Vec<_>, Vec<_>) = patterns
            .into_iter()
            .partition(|t| virtual_function(t).is_some());
        let mut out = self.relation_bgp(patterns);
        for t in virtuals {
            out = join(out, self.virtual_property(&t));
        }
        out
    }

    /// A virtual property triple (`?g geo:hasMetricLength ?l`): the value is computed from the
    /// subject's geometry (its default geometry, for a feature), unless the data asserts the
    /// property for that subject, in which case the asserted value is used.
    fn virtual_property(&mut self, t: &TriplePattern) -> GraphPattern {
        let (property, function) = virtual_function(t).unwrap();
        self.stats.virtual_properties += 1;
        let w = self.fresh();
        let value = Expression::FunctionCall(
            Function::Custom(oxigraph::model::NamedNode::new_unchecked(format!(
                "{GEOF}{function}"
            ))),
            vec![Expression::Variable(w.clone())],
        );
        let geometry = self.geometry_of(&t.subject, &w);
        let computed = match &t.object {
            TermPattern::Variable(v) => GraphPattern::Extend {
                inner: Box::new(geometry),
                variable: v.clone(),
                expression: value,
            },
            constant => {
                let constant = match constant {
                    TermPattern::NamedNode(n) => Expression::NamedNode(n.clone()),
                    TermPattern::Literal(l) => Expression::Literal(l.clone()),
                    // Blank nodes and triple terms never equal a computed literal.
                    _ => return bgp(vec![t.clone()]),
                };
                GraphPattern::Filter {
                    expr: Expression::Equal(Box::new(value), Box::new(constant)),
                    inner: Box::new(geometry),
                }
            }
        };
        let mut visible: Vec<Variable> = [&t.subject, &t.object]
            .into_iter()
            .filter_map(|p| match p {
                TermPattern::Variable(v) => Some(v.clone()),
                _ => None,
            })
            .collect();
        visible.dedup();
        let computed = GraphPattern::Project {
            inner: Box::new(computed),
            variables: visible,
        };
        if !self.ctx.has_predicate(&format!("{GEO}{property}")) {
            return computed;
        }
        // Asserted values win: compute only for subjects without one.
        let any = TermPattern::Variable(self.fresh());
        let asserted = bgp(vec![triple(
            t.subject.clone(),
            &format!("{GEO}{property}"),
            any,
        )]);
        let computed = if matches!(t.subject, TermPattern::Variable(_)) {
            GraphPattern::Minus {
                left: Box::new(computed),
                right: Box::new(asserted),
            }
        } else {
            GraphPattern::Filter {
                expr: Expression::Not(Box::new(Expression::Exists(Box::new(asserted)))),
                inner: Box::new(computed),
            }
        };
        union(bgp(vec![t.clone()]), computed)
    }

    fn relation_bgp(&mut self, patterns: Vec<TriplePattern>) -> GraphPattern {
        let relation = |t: &TriplePattern| match &t.predicate {
            NamedNodePattern::NamedNode(p) => p
                .as_str()
                .strip_prefix(GEO)
                .filter(|n| RELATIONS.contains(n) || NOT_INDEXABLE.contains(n))
                .map(str::to_owned),
            NamedNodePattern::Variable(_) => None,
        };
        if !patterns.iter().any(|t| relation(t).is_some()) {
            return bgp(patterns);
        }
        let original = bgp(patterns.clone());
        let visible: Vec<Variable> = {
            let mut v: Vec<Variable> = in_scope(&original).into_iter().collect();
            v.sort_by(|a, b| a.as_str().cmp(b.as_str()));
            v
        };
        let (relations, plain): (Vec<_>, Vec<_>) =
            patterns.into_iter().partition(|t| relation(t).is_some());
        let mut inner = bgp(plain);
        let mut test: Option<Expression> = None;
        let mut asserted = false;
        for t in &relations {
            let name = relation(t).unwrap();
            asserted |= self.ctx.has_predicate(&format!("{GEO}{name}"));
            let (wa, wb) = (self.fresh(), self.fresh());
            inner = join(inner, self.geometry_of(&t.subject, &wa));
            inner = join(inner, self.geometry_of(&t.object, &wb));
            let call = Self::relation_call(&name, &wa, &wb);
            test = Some(match test {
                None => call,
                Some(prev) => Expression::And(Box::new(prev), Box::new(call)),
            });
            self.stats.triples += 1;
        }
        let inferred = GraphPattern::Filter {
            expr: test.unwrap(),
            inner: Box::new(inner),
        };
        // Asserted relation triples count too.
        let body = if asserted {
            union(original, inferred)
        } else {
            inferred
        };
        // Inside EXISTS, the row's values are bound: the lookups and the test as they are.
        if self.exists > 0 {
            return body;
        }
        // The generated FILTER is itself a spatial test; plan it like any other.
        let body = self.walk_generated(body);
        GraphPattern::Distinct {
            inner: Box::new(GraphPattern::Project {
                inner: Box::new(body),
                variables: visible,
            }),
        }
    }

    /// Walks a pattern this rewriter generated (its BGPs hold no relation triples to expand,
    /// except the original BGP kept for asserted triples, which must stay as it is).
    fn walk_generated(&mut self, p: GraphPattern) -> GraphPattern {
        match p {
            GraphPattern::Union { left, right } => GraphPattern::Union {
                left,
                right: Box::new(self.walk_generated(*right)),
            },
            GraphPattern::Filter { expr, inner } => self.filter(expr, *inner),
            other => other,
        }
    }

    /// A FILTER of a group with `OPTIONAL`s applies to the left join of the group and its
    /// optional parts. A spatial test reading only variables the left side binds in every row
    /// sees the same values before the left join (and before `BIND`s of other variables), so
    /// its conjuncts are planned there, on the join they test, rather than over an opaque left
    /// join (where they would test every row). The other conjuncts stay where they were.
    fn filter_below_optionals(&mut self, expr: Expression, inner: GraphPattern) -> GraphPattern {
        let mut conjuncts = Vec::new();
        split_and(expr.clone(), &mut conjuncts);
        let (mut spatial, mut rest) = (Vec::new(), Vec::new());
        let mut used = HashSet::new();
        for c in conjuncts {
            let mut found = Vec::new();
            plans(&c, &mut found);
            let mut vars = HashSet::new();
            if !found.is_empty() && expression_variables(&c, &mut vars) {
                used.extend(vars);
                spatial.push(c);
            } else {
                rest.push(c);
            }
        }
        let and = |cs: Vec<Expression>| {
            cs.into_iter()
                .reduce(|a, b| Expression::And(Box::new(a), Box::new(b)))
        };
        let Some(test) = and(spatial).filter(|_| below_optional(&inner, &used)) else {
            return self.filter(expr, inner);
        };
        let pushed = self.push_below_optionals(test, inner, &used);
        match and(rest) {
            Some(expr) => GraphPattern::Filter {
                expr,
                inner: Box::new(pushed),
            },
            None => pushed,
        }
    }

    /// Plans `test` (reading `used`) inside `p`, through `BIND`s of other variables and down the
    /// left side of left joins that bind `used` in every row.
    fn push_below_optionals(
        &mut self,
        test: Expression,
        p: GraphPattern,
        used: &HashSet<Variable>,
    ) -> GraphPattern {
        match p {
            GraphPattern::LeftJoin {
                left,
                right,
                expression,
            } if certain(&left).is_superset(used) => GraphPattern::LeftJoin {
                left: Box::new(self.push_below_optionals(test, *left, used)),
                right,
                expression,
            },
            GraphPattern::Extend {
                inner,
                variable,
                expression,
            } if !used.contains(&variable) && below_optional(&inner, used) => {
                GraphPattern::Extend {
                    inner: Box::new(self.push_below_optionals(test, *inner, used)),
                    variable,
                    expression,
                }
            }
            p => self.filter(test, p),
        }
    }

    /// `e` with the patterns of its `EXISTS` rewritten as any other: relation triples in
    /// `FILTER (NOT) EXISTS { ?g geo:sfWithin ?r }` are inferred too.
    fn walk_exists(&mut self, e: Expression) -> Expression {
        use Expression as E;
        let mut w = |e: Box<E>| Box::new(self.walk_exists(*e));
        match e {
            E::Exists(p) => {
                self.exists += 1;
                let p = self.walk(*p);
                self.exists -= 1;
                E::Exists(Box::new(p))
            }
            E::Or(a, b) => E::Or(w(a), w(b)),
            E::And(a, b) => E::And(w(a), w(b)),
            E::Equal(a, b) => E::Equal(w(a), w(b)),
            E::SameTerm(a, b) => E::SameTerm(w(a), w(b)),
            E::Greater(a, b) => E::Greater(w(a), w(b)),
            E::GreaterOrEqual(a, b) => E::GreaterOrEqual(w(a), w(b)),
            E::Less(a, b) => E::Less(w(a), w(b)),
            E::LessOrEqual(a, b) => E::LessOrEqual(w(a), w(b)),
            E::Add(a, b) => E::Add(w(a), w(b)),
            E::Subtract(a, b) => E::Subtract(w(a), w(b)),
            E::Multiply(a, b) => E::Multiply(w(a), w(b)),
            E::Divide(a, b) => E::Divide(w(a), w(b)),
            E::UnaryPlus(a) => E::UnaryPlus(w(a)),
            E::UnaryMinus(a) => E::UnaryMinus(w(a)),
            E::Not(a) => E::Not(w(a)),
            E::If(a, b, c) => E::If(w(a), w(b), w(c)),
            E::In(a, list) => {
                let a = w(a);
                E::In(a, list.into_iter().map(|e| self.walk_exists(e)).collect())
            }
            E::Coalesce(list) => {
                E::Coalesce(list.into_iter().map(|e| self.walk_exists(e)).collect())
            }
            E::FunctionCall(f, args) => {
                E::FunctionCall(f, args.into_iter().map(|e| self.walk_exists(e)).collect())
            }
            other => other,
        }
    }

    fn walk(&mut self, p: GraphPattern) -> GraphPattern {
        use GraphPattern as G;
        let b = |p: GraphPattern| Box::new(p);
        match p {
            G::Bgp { patterns } => self.bgp(patterns),
            G::Filter { expr, inner } if self.exists > 0 => G::Filter {
                expr: self.walk_exists(expr),
                inner: b(self.walk(*inner)),
            },
            G::Filter { expr, inner } => {
                let expr = self.walk_exists(expr);
                let inner = self.walk(*inner);
                // A FILTER applies to its whole group, BINDs included: the BINDs it does not
                // read are applied after it, so the tests see the group's join itself (and its
                // two sides apart) rather than an opaque BIND over it.
                let mut used = HashSet::new();
                let (mut inner, mut binds) = (inner, Vec::new());
                if expression_variables(&expr, &mut used) {
                    while let G::Extend {
                        inner: i,
                        variable,
                        expression,
                    } = inner
                    {
                        if used.contains(&variable) {
                            inner = G::Extend {
                                inner: i,
                                variable,
                                expression,
                            };
                            break;
                        }
                        binds.push((variable, expression));
                        inner = *i;
                    }
                }
                let mut out = self.filter_below_optionals(expr, inner);
                for (variable, expression) in binds.into_iter().rev() {
                    out = G::Extend {
                        inner: b(out),
                        variable,
                        expression,
                    };
                }
                out
            }
            G::Join { left, right } => match linked_through_paths(G::Join { left, right }) {
                // Inside EXISTS, the relation's lookups are evaluated for each row of the rest
                // of the group (LATERAL), with its values bound, rather than for every
                // geometry of the store and then joined.
                G::Join { left, right }
                    if self.exists > 0 && (has_relation(&left) != has_relation(&right)) =>
                {
                    let (rest, relation) = if has_relation(&right) {
                        (left, right)
                    } else {
                        (right, left)
                    };
                    G::Lateral {
                        left: b(self.walk(*rest)),
                        right: b(self.walk(*relation)),
                    }
                }
                G::Join { left, right } => G::Join {
                    left: b(self.walk(*left)),
                    right: b(self.walk(*right)),
                },
                other => self.walk(other),
            },
            G::LeftJoin {
                left,
                right,
                expression,
            } => G::LeftJoin {
                left: b(self.walk(*left)),
                right: b(self.walk(*right)),
                expression: expression.map(|e| self.walk_exists(e)),
            },
            G::Lateral { left, right } => G::Lateral {
                left: b(self.walk(*left)),
                right: b(self.walk(*right)),
            },
            G::Union { left, right } => G::Union {
                left: b(self.walk(*left)),
                right: b(self.walk(*right)),
            },
            G::Minus { left, right } => G::Minus {
                left: b(self.walk(*left)),
                right: b(self.walk(*right)),
            },
            G::Graph { name, inner } => {
                self.scoped += 1;
                let inner = self.walk(*inner);
                self.scoped -= 1;
                G::Graph {
                    name,
                    inner: b(inner),
                }
            }
            G::Extend {
                inner,
                variable,
                expression,
            } => G::Extend {
                inner: b(self.walk(*inner)),
                variable,
                expression: self.walk_exists(expression),
            },
            G::OrderBy { inner, expression } => G::OrderBy {
                inner: b(self.walk(*inner)),
                expression,
            },
            G::Project { inner, variables } => G::Project {
                inner: b(self.walk(*inner)),
                variables,
            },
            G::Distinct { inner } => G::Distinct {
                inner: b(self.walk(*inner)),
            },
            G::Reduced { inner } => G::Reduced {
                inner: b(self.walk(*inner)),
            },
            G::Slice {
                inner,
                start,
                length,
            } => G::Slice {
                inner: b(self.walk(*inner)),
                start,
                length,
            },
            G::Group {
                inner,
                variables,
                aggregates,
            } => G::Group {
                inner: b(self.walk(*inner)),
                variables,
                aggregates,
            },
            // TripleTrail's own services evaluate their sub-selects on this store: those are
            // rewritten as the main query is (#857). Their own triples are left as they are.
            G::Service {
                name: NamedNodePattern::NamedNode(name),
                inner,
                silent,
            } if BUILT_IN_SERVICES.contains(&name.as_str()) => G::Service {
                inner: b(self.sub_selects(*inner)),
                name: NamedNodePattern::NamedNode(name),
                silent,
            },
            // Other SERVICE blocks are handed to their handler untouched; paths and VALUES
            // have no FILTER inside.
            other => other,
        }
    }

    /// The sub-selects in a built-in service's pattern, rewritten; the rest as it is.
    fn sub_selects(&mut self, p: GraphPattern) -> GraphPattern {
        use GraphPattern as G;
        match p {
            G::Join { left, right } => G::Join {
                left: Box::new(self.sub_selects(*left)),
                right: Box::new(self.sub_selects(*right)),
            },
            G::Project { .. }
            | G::Slice { .. }
            | G::Distinct { .. }
            | G::Reduced { .. }
            | G::OrderBy { .. } => self.walk(p),
            other => other,
        }
    }
}

/// Whether `p`'s join operands hold a GeoSPARQL relation triple.
fn has_relation(p: &GraphPattern) -> bool {
    match p {
        GraphPattern::Join { left, right } => has_relation(left) || has_relation(right),
        GraphPattern::Bgp { patterns } => patterns.iter().any(|t| match &t.predicate {
            NamedNodePattern::NamedNode(p) => p
                .as_str()
                .strip_prefix(GEO)
                .is_some_and(|n| RELATIONS.contains(&n) || NOT_INDEXABLE.contains(&n)),
            NamedNodePattern::Variable(_) => false,
        }),
        _ => false,
    }
}

/// The join `p` with the blank nodes that link a property path to its siblings written as
/// variables (#48 follow-up). The parser joins `?x (a|b)/geo:hasGeometry ?g` as a path to a
/// blank node and a triple from it; a rewritten BGP projects its variables only, so a blank
/// node would no longer link the two. The variables' names start with `__tt_path_`; a
/// `SELECT` projects its own variables, so they are not returned.
fn linked_through_paths(p: GraphPattern) -> GraphPattern {
    fn ends(p: &GraphPattern, out: &mut HashSet<String>) {
        match p {
            GraphPattern::Join { left, right } => {
                ends(left, out);
                ends(right, out);
            }
            GraphPattern::Path {
                subject, object, ..
            } => {
                for t in [subject, object] {
                    if let TermPattern::BlankNode(b) = t {
                        out.insert(b.as_str().to_owned());
                    }
                }
            }
            _ => {}
        }
    }
    fn term(t: TermPattern, linked: &HashSet<String>) -> TermPattern {
        match t {
            TermPattern::BlankNode(b) if linked.contains(b.as_str()) => {
                Variable::new_unchecked(format!("__tt_path_{}", b.as_str())).into()
            }
            other => other,
        }
    }
    fn rename(p: GraphPattern, linked: &HashSet<String>) -> GraphPattern {
        match p {
            GraphPattern::Join { left, right } => GraphPattern::Join {
                left: Box::new(rename(*left, linked)),
                right: Box::new(rename(*right, linked)),
            },
            GraphPattern::Path {
                subject,
                path,
                object,
            } => GraphPattern::Path {
                subject: term(subject, linked),
                path,
                object: term(object, linked),
            },
            GraphPattern::Bgp { patterns } => GraphPattern::Bgp {
                patterns: patterns
                    .into_iter()
                    .map(|t| TriplePattern {
                        subject: term(t.subject, linked),
                        predicate: t.predicate,
                        object: term(t.object, linked),
                    })
                    .collect(),
            },
            other => other,
        }
    }
    let mut linked = HashSet::new();
    ends(&p, &mut linked);
    if linked.is_empty() {
        p
    } else {
        rename(p, &linked)
    }
}

/// The services of TripleTrail's own that read sub-selects from the store (`st:`, window,
/// routing and path search).
pub const BUILT_IN_SERVICES: [&str; 4] = [
    "http://w3id.org/tripletrail/st",
    "http://w3id.org/tripletrail/window",
    "http://w3id.org/tripletrail/pgr",
    "http://w3id.org/tripletrail/path",
];

/// Rewrites the query's spatial parts: GeoSPARQL relation triples become geometry tests, and
/// FILTERs testing store geometries against a constant geometry or against each other get a
/// `VALUES` block of candidates from the index. The FILTERs stay in place, so results do not
/// change. The index is only built if the query has an indexable test.
pub fn rewrite_query(query: Query, ctx: &dyn SpatialContext) -> (Query, RewriteStats) {
    let mut r = Rewriter::new(ctx);
    r.mentions = mentions(&query);
    let walk = |r: &mut Rewriter<'_>, dataset: &Option<spargebra::algebra::QueryDataset>, p| {
        r.scoped = usize::from(dataset.is_some());
        r.walk(p)
    };
    let query = match query {
        Query::Select {
            dataset,
            pattern,
            base_iri,
        } => Query::Select {
            pattern: walk(&mut r, &dataset, pattern),
            dataset,
            base_iri,
        },
        Query::Construct {
            template,
            dataset,
            pattern,
            base_iri,
        } => Query::Construct {
            template,
            pattern: walk(&mut r, &dataset, pattern),
            dataset,
            base_iri,
        },
        Query::Describe {
            dataset,
            pattern,
            base_iri,
        } => Query::Describe {
            pattern: walk(&mut r, &dataset, pattern),
            dataset,
            base_iri,
        },
        Query::Ask {
            dataset,
            pattern,
            base_iri,
        } => Query::Ask {
            pattern: walk(&mut r, &dataset, pattern),
            dataset,
            base_iri,
        },
    };
    (query, r.stats)
}

/// [`rewrite_query`] for the `WHERE` of a SPARQL update's `DELETE`/`INSERT` operations.
///
/// The index reflects the store before the update runs, so only the first operation is
/// rewritten: a later one may read what an earlier one of the same request wrote.
pub fn rewrite_update(
    update: spargebra::Update,
    ctx: &dyn SpatialContext,
) -> (spargebra::Update, RewriteStats) {
    use spargebra::GraphUpdateOperation;
    let mut r = Rewriter::new(ctx);
    let operations = update
        .operations
        .into_iter()
        .enumerate()
        .map(|(i, op)| match op {
            GraphUpdateOperation::DeleteInsert {
                delete,
                insert,
                using,
                pattern,
            } if i == 0 => {
                r.scoped = usize::from(using.is_some());
                GraphUpdateOperation::DeleteInsert {
                    pattern: Box::new(r.walk(*pattern)),
                    delete,
                    insert,
                    using,
                }
            }
            other => other,
        })
        .collect();
    (
        spargebra::Update {
            operations,
            base_iri: update.base_iri,
        },
        r.stats,
    )
}

/// A query asking for the geometries nearest to a constant geometry:
/// `ORDER BY geof:distance(?w, C) LIMIT k` (or `st:ST_Distance`, `st:ST_DistanceSphere`, or a
/// variable bound to one of those). PostGIS answers this shape with its `<->` index scan.
///
/// The evaluator would compute the distance to every geometry and sort. Instead,
/// [`Knn::bounded`] restricts `?w` to the geometries within `r` (found through the index),
/// plus every geometry whose distance is an error (another CRS, invalid), because SPARQL sorts
/// errors first. Once the bounded query returns `k` rows they are exactly the first `k` rows of
/// the original query, since every row it left out has a distance above `r` and sorts after
/// them. Otherwise the caller retries with a larger `r`.
#[derive(Debug, Clone)]
pub struct Knn {
    limit: usize,
    distance: Expression,
    var: Variable,
    constant: Term,
    /// The pattern under the `ORDER BY`, which binds `var`.
    pattern: GraphPattern,
}

/// A bounded nearest-neighbour query.
pub struct Bounded {
    pub query: Query,
    /// The radius took in every geometry, so the bounded query is the original one.
    pub complete: bool,
    pub stats: RewriteStats,
}

impl Knn {
    /// The `LIMIT`: the bounded query is exact once it returns this many rows.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// A radius to start from: the distance that takes in about `4 * limit` indexed
    /// geometries around the constant, whatever the rest of the pattern selects. `None` if
    /// the index cannot tell.
    pub fn first_radius(&self, ctx: &dyn SpatialContext) -> Option<f64> {
        let one = Expression::Literal(spargebra::term::Literal::new_typed_literal(
            "1",
            oxigraph::model::vocab::xsd::DOUBLE,
        ));
        let plan = distance_plan(&self.distance, &one, Cmp::LessOrEqual)?;
        let geom = parse_wkt_literal(&self.constant)?;
        let rect = geom.geometry.bounding_rect()?;
        let index = ctx.index()?;
        let d = index.nth_nearest_distance(
            &geom.crs,
            [rect.center().x, rect.center().y],
            4 * self.limit,
        )?;
        // From the centre: add the constant's own half diagonal so its far edge is covered.
        let d = d + (rect.width().hypot(rect.height())) / 2.0;
        // Degrees to metres at the scale of a degree of latitude: never less than the true
        // distance, so the radius takes in at least those geometries.
        let scale = match plan.margin {
            Margin::Metres(_) if geom.crs.is_geographic() => METRES_PER_DEGREE * 1.02,
            _ => 1.0,
        };
        Some((d * scale).max(1e-9))
    }

    /// The radius for the next round, after one with `radius` returned `rows` rows (fewer
    /// than the limit): grown by the shortfall, since the rows grow with the area.
    pub fn next_radius(&self, radius: f64, rows: usize) -> f64 {
        let rows = (rows as f64).max(0.5);
        radius * ((self.limit as f64 / rows).sqrt() * 1.3).clamp(1.5, 16.0)
    }

    /// `query` with `?w` restricted to the geometries within `radius` of the constant, plus
    /// those whose distance is an error. `None` if the index cannot answer it.
    pub fn bounded(&self, query: &Query, radius: f64, ctx: &dyn SpatialContext) -> Option<Bounded> {
        let start = Instant::now();
        let bound = Expression::Literal(spargebra::term::Literal::new_typed_literal(
            radius.to_string(),
            oxigraph::model::vocab::xsd::DOUBLE,
        ));
        let plan = distance_plan(&self.distance, &bound, Cmp::LessOrEqual)?;
        let geom = parse_wkt_literal(&self.constant)?;
        let envelope = plan.margin.envelope(&geom)?;
        let index = ctx.index()?;
        // Every value of the variable must be in the index or have an error distance there.
        let bound_by_indexed = |p: &NamedNodePattern| match p {
            NamedNodePattern::NamedNode(p) if p.as_str() == AS_WKT => index.has_every_wkt_value(),
            NamedNodePattern::NamedNode(p) => {
                index.predicates().covers(p.as_str())
                    && ctx.non_geometry_objects(p.as_str()) == Some(0)
            }
            NamedNodePattern::Variable(_) => false,
        };
        if !binds_through(&self.pattern, &self.var, &bound_by_indexed, false) {
            return None;
        }
        let mut values: Vec<Literal> = index.outside(&geom.crs);
        let mut in_space = 0usize;
        for e in index.candidates(&geom.crs, envelope) {
            if plan.accepts_one(index.geom(e)) {
                values.push(index.literal(e).clone());
                in_space += 1;
            }
        }
        let stats = RewriteStats {
            filters: 1,
            candidates: in_space,
            lookup: start.elapsed(),
            ..RewriteStats::default()
        };
        let block = GraphPattern::Values {
            variables: vec![self.var.clone()],
            bindings: values
                .into_iter()
                .map(|l| vec![Some(GroundTerm::Literal(l))])
                .collect(),
        };
        // Rows whose distance is an error stay: they sort first.
        let keep = Expression::Coalesce(vec![
            Expression::LessOrEqual(Box::new(self.distance.clone()), Box::new(bound)),
            Expression::Literal(spargebra::term::Literal::new_typed_literal(
                "true",
                oxigraph::model::vocab::xsd::BOOLEAN,
            )),
        ]);
        fn add(p: &GraphPattern, block: &GraphPattern, keep: &Expression) -> GraphPattern {
            let rec = |inner: &GraphPattern| Box::new(add(inner, block, keep));
            match p {
                GraphPattern::Slice {
                    inner,
                    start,
                    length,
                } => GraphPattern::Slice {
                    inner: rec(inner),
                    start: *start,
                    length: *length,
                },
                GraphPattern::Distinct { inner } => GraphPattern::Distinct { inner: rec(inner) },
                GraphPattern::Reduced { inner } => GraphPattern::Reduced { inner: rec(inner) },
                GraphPattern::Project { inner, variables } => GraphPattern::Project {
                    inner: rec(inner),
                    variables: variables.clone(),
                },
                GraphPattern::OrderBy { inner, expression } => GraphPattern::OrderBy {
                    inner: Box::new(GraphPattern::Filter {
                        expr: keep.clone(),
                        inner: Box::new(join_after(vec![block.clone()], (**inner).clone())),
                    }),
                    expression: expression.clone(),
                },
                other => other.clone(),
            }
        }
        let Query::Select {
            dataset,
            pattern,
            base_iri,
        } = query
        else {
            return None;
        };
        Some(Bounded {
            query: Query::Select {
                dataset: dataset.clone(),
                pattern: add(pattern, &block, &keep),
                base_iri: base_iri.clone(),
            },
            complete: in_space >= index.len_in(&geom.crs),
            stats,
        })
    }
}

/// The nearest-neighbour shape of `query`, if it has one.
pub fn knn(query: &Query) -> Option<Knn> {
    let Query::Select {
        pattern, dataset, ..
    } = query
    else {
        return None;
    };
    if dataset.is_some() {
        return None;
    }
    let GraphPattern::Slice {
        inner,
        length: Some(length),
        ..
    } = pattern
    else {
        return None;
    };
    let mut p = &**inner;
    loop {
        p = match p {
            GraphPattern::Distinct { inner }
            | GraphPattern::Reduced { inner }
            | GraphPattern::Project { inner, .. } => inner,
            _ => break,
        };
    }
    let GraphPattern::OrderBy { inner, expression } = p else {
        return None;
    };
    let Some(spargebra::algebra::OrderExpression::Asc(first)) = expression.first() else {
        return None;
    };
    // `ORDER BY ?d` with `?d` bound to the distance by BIND or a SELECT expression.
    let distance = match first {
        Expression::Variable(v) => {
            let mut q = &**inner;
            loop {
                match q {
                    GraphPattern::Extend {
                        variable,
                        expression,
                        ..
                    } if variable == v => break expression.clone(),
                    GraphPattern::Extend { inner, .. } | GraphPattern::Filter { inner, .. } => {
                        q = inner
                    }
                    _ => return None,
                }
            }
        }
        e => e.clone(),
    };
    let one = Expression::Literal(spargebra::term::Literal::new_typed_literal(
        "1",
        oxigraph::model::vocab::xsd::DOUBLE,
    ));
    match distance_plan(&distance, &one, Cmp::LessOrEqual)?.shape()? {
        // Which predicates may bind it is checked with the index, in `Knn::bounded`.
        Shape::Constant { var, constant }
            if binds_through(
                inner,
                &var,
                &|p| matches!(p, NamedNodePattern::NamedNode(_)),
                false,
            ) =>
        {
            Some(Knn {
                limit: *length,
                distance,
                var,
                constant,
                pattern: (**inner).clone(),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_reaches_a_box_only_near_its_segments() {
        let crs = Crs::Other("http://example.org/plane".to_owned());
        // An L-shaped line whose box covers (0, 0) to (10, 10).
        let line: geo::Geometry<f64> =
            geo::LineString::from(vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)]).into();
        let inside_corner: sweep::Bbox = ([1.0, 8.0], [2.0, 9.0]);
        let on_segment: sweep::Bbox = ([4.0, -1.0], [5.0, 1.0]);
        assert!(!reaches(Margin::None, &crs, &line, &inside_corner));
        assert!(reaches(Margin::None, &crs, &line, &on_segment));
        // Within the margin of the vertical segment.
        let near: sweep::Bbox = ([8.5, 4.0], [9.0, 5.0]);
        assert!(!reaches(Margin::None, &crs, &line, &near));
        assert!(reaches(Margin::Planar(1.0), &crs, &line, &near));
        // Points and one-point lines.
        let point: geo::Geometry<f64> = geo::Point::new(1.5, 8.5).into();
        assert!(reaches(Margin::None, &crs, &point, &inside_corner));
        let one: geo::Geometry<f64> = geo::LineString::from(vec![(1.5, 8.5)]).into();
        assert!(reaches(Margin::None, &crs, &one, &inside_corner));
        // A polygon may meet the box anywhere inside: kept.
        let square: geo::Geometry<f64> =
            geo::Rect::new((0.0, 0.0), (10.0, 10.0)).to_polygon().into();
        assert!(reaches(Margin::None, &crs, &square, &inside_corner));
        let collection =
            geo::Geometry::GeometryCollection(geo::GeometryCollection::from(vec![line, square]));
        assert!(reaches(Margin::None, &crs, &collection, &inside_corner));
    }

    #[test]
    fn variable_mentions_skip_iris_and_strings() {
        let q = spargebra::SparqlParser::new()
            .parse_query(
                "SELECT ?w1 WHERE { ?s <http://x/?w2> ?w1 . ?s <http://x/p> ?w2 . \
             FILTER(?w1 < ?w2 && ?w10 <= 3 && STR(?w2) != \"?w1 \\\" ?w2\") }",
            )
            .unwrap();
        let m = mentions(&q);
        assert_eq!(m.get("w1"), Some(&3), "{q}");
        assert_eq!(m.get("w2"), Some(&3), "{q}");
        assert_eq!(m.get("w10"), Some(&1), "{q}");
    }

    /// The (?a ?b) solutions of a pattern made of VALUES, joins and unions.
    fn solutions(p: &GraphPattern) -> HashSet<(String, String)> {
        fn rows(p: &GraphPattern) -> Vec<HashMap<String, String>> {
            match p {
                GraphPattern::Values {
                    variables,
                    bindings,
                } => bindings
                    .iter()
                    .map(|row| {
                        variables
                            .iter()
                            .zip(row)
                            .map(|(v, t)| match t {
                                Some(GroundTerm::Literal(l)) => {
                                    (v.as_str().to_owned(), l.value()[..1].to_owned())
                                }
                                _ => unreachable!(),
                            })
                            .collect()
                    })
                    .collect(),
                GraphPattern::Join { left, right } => {
                    let mut out = Vec::new();
                    for l in rows(left) {
                        for r in rows(right) {
                            let mut m = l.clone();
                            m.extend(r);
                            out.push(m);
                        }
                    }
                    out
                }
                GraphPattern::Union { left, right } => {
                    let mut out = rows(left);
                    out.extend(rows(right));
                    out
                }
                _ => unreachable!(),
            }
        }
        rows(p)
            .into_iter()
            .map(|m| (m["a"].clone(), m["b"].clone()))
            .collect()
    }

    #[test]
    fn pairs_of_large_geometries_are_written_once_per_geometry() {
        // Three 2 MB "polygons" (named by their first letter), each with 40 small "lines".
        let polygons: Vec<Literal> = ["x", "y", "z"]
            .iter()
            .map(|n| Literal::new_simple_literal(format!("{n}{}", " ".repeat(2 << 20))))
            .collect();
        let lines: Vec<Literal> = (0..40)
            .map(|i| Literal::new_simple_literal(format!("{}", (b'a' + i % 26) as char)))
            .collect();
        let pairs: Vec<(&Literal, &Literal)> = polygons
            .iter()
            .flat_map(|p| lines.iter().map(move |l| (l, p)))
            .collect();
        let (a, b) = (Variable::new_unchecked("a"), Variable::new_unchecked("b"));
        let pattern = pairs_pattern((&a, &b), &pairs);
        // Grouped on the polygons: one VALUES row per polygon, not one per pair.
        let polygon_rows = |p: &GraphPattern| {
            fn count(p: &GraphPattern, var: &Variable) -> usize {
                match p {
                    GraphPattern::Values {
                        variables,
                        bindings,
                    } if variables.contains(var) => bindings.len(),
                    GraphPattern::Join { left, right } | GraphPattern::Union { left, right } => {
                        count(left, var) + count(right, var)
                    }
                    _ => 0,
                }
            }
            count(p, &b)
        };
        assert_eq!(polygon_rows(&pattern), 3);
        let expected: HashSet<(String, String)> = pairs
            .iter()
            .map(|(l, p)| (l.value()[..1].to_owned(), p.value()[..1].to_owned()))
            .collect();
        assert_eq!(solutions(&pattern), expected);
    }

    #[test]
    fn small_pairs_stay_one_values_block() {
        let (x, y) = (
            Literal::new_simple_literal("x"),
            Literal::new_simple_literal("y"),
        );
        let (a, b) = (Variable::new_unchecked("a"), Variable::new_unchecked("b"));
        let pattern = pairs_pattern((&a, &b), &[(&x, &y), (&y, &x)]);
        assert!(
            matches!(&pattern, GraphPattern::Values { bindings, .. } if bindings.len() == 2),
            "{pattern:?}"
        );
    }

    /// A store whose timeout passes while it reads a join side's rows: the rewrite must end
    /// there, not evaluate the sides again for another way of joining (#338). On OSM Europe
    /// those other ways ran on for minutes past the query's timeout.
    #[test]
    fn a_join_cancelled_while_reading_a_side_evaluates_nothing_more() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        #[derive(Default)]
        struct TimesOut {
            cancelled: AtomicBool,
            sides: AtomicUsize,
            evaluated: AtomicUsize,
        }
        impl SpatialContext for TimesOut {
            fn index(&self) -> Option<Arc<SpatialIndex>> {
                self.evaluated.fetch_add(1, Ordering::Relaxed);
                None
            }
            fn indexed_predicates(&self) -> Option<IndexedPredicates> {
                Some(IndexedPredicates::default())
            }
            fn has_predicate(&self, _: &str) -> bool {
                true
            }
            fn values_of(&self, _: &GraphPattern, _: &Variable) -> Option<Vec<Term>> {
                self.evaluated.fetch_add(1, Ordering::Relaxed);
                Some(Vec::new())
            }
            fn rows_of(&self, _: &GraphPattern, _: &[Variable]) -> Option<Vec<Vec<Option<Term>>>> {
                self.evaluated.fetch_add(1, Ordering::Relaxed);
                Some(Vec::new())
            }
            fn side_ids(&self, _: &GraphPattern, _: &Variable) -> Option<IdSide> {
                // The timeout passes while the side is read: the store gives up on it.
                self.sides.fetch_add(1, Ordering::Relaxed);
                self.cancelled.store(true, Ordering::Relaxed);
                None
            }
            fn cancelled(&self) -> bool {
                self.cancelled.load(Ordering::Relaxed)
            }
        }
        let q = spargebra::SparqlParser::new()
            .parse_query(
                "PREFIX geo: <http://www.opengis.net/ont/geosparql#> \
                 PREFIX geof: <http://www.opengis.net/def/function/geosparql/> \
                 SELECT (COUNT(*) AS ?n) WHERE { \
                   ?t a <http://x/Track> ; geo:hasGeometry/geo:asWKT ?w1 . \
                   ?l a <http://x/Landuse> ; geo:hasGeometry/geo:asWKT ?w2 . \
                   FILTER(geof:sfIntersects(?w1, ?w2)) }",
            )
            .unwrap();
        let ctx = TimesOut::default();
        let (_, stats) = rewrite_query(q, &ctx);
        assert_eq!(ctx.sides.load(Ordering::Relaxed), 1);
        assert_eq!(ctx.evaluated.load(Ordering::Relaxed), 0, "{stats:?}");
        assert_eq!(stats.joins, 0);
    }
}
