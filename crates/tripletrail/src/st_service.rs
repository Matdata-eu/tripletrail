//! `SERVICE <http://w3id.org/tripletrail/st>`: PostGIS's set-returning and window functions
//! (`ST_Dump`, `ST_DumpPoints`, `ST_DumpSegments`, `ST_DumpRings`, `ST_Subdivide`,
//! `ST_HexagonGrid`, `ST_SquareGrid`, `ST_ClusterDBSCAN`, `ST_ClusterKMeans`,
//! `ST_ClusterIntersectingWin`, `ST_ClusterWithinWin`, `ST_ClusterIntersecting`,
//! `ST_ClusterWithin`, `ST_IsValidDetail`, `ST_FromFlatGeobuf`) as rows, `ST_AsMVT` (a vector
//! tile with the sub-select's other columns as attributes), `ST_AsFlatGeobuf` and `ST_AsGeobuf`
//! (with them as feature properties), `rail:conflate` (source lines matched to target lines,
//! from two sub-selects), and `st:srs` / `st:srsSearch` (`postgis_srs`, `postgis_srs_all` and
//! `postgis_srs_search` over the CRSs the transforms support).
//!
//! ```sparql
//! SERVICE <http://w3id.org/tripletrail/st> {
//!   [] st:function st:ST_Dump ; st:path ?path ; st:geom ?part .
//!   { SELECT ?id ?geom WHERE { ?id geo:hasGeometry/geo:asWKT ?geom } }
//! }
//! ```
//!
//! Inputs come from a constant (`st:geom "..."^^geo:wktLiteral`) or from a sub-select projecting
//! `?geom`; every other variable of the sub-select is passed through to the rows it produces,
//! so they join back to the data. Triples with a constant object are parameters, triples with a
//! variable object bind result columns. Window functions run over all sub-select rows, or per
//! value of `?partition` if the sub-select projects one.

use oxigraph::model::{Literal, Term};
use oxigraph::sparql::{
    QueryEvaluationError, QueryResults, QuerySolution, QuerySolutionIter, ServiceHandler,
    SparqlEvaluator, Variable,
};
use oxiri::Iri;
use spargebra::algebra::GraphPattern;
use spargebra::term::{NamedNodePattern, TermPattern};
use std::collections::HashMap;
use std::sync::Arc;
use tripletrail_geo::formats;
use tripletrail_geo::sets::{self, DumpKind};
use tripletrail_geo::{mvt, rail, srs};
use tripletrail_store::Dataset;

pub const ST_SERVICE: &str = "http://w3id.org/tripletrail/st";
const ST: &str = "http://w3id.org/tripletrail/st#";
const RAIL: &str = tripletrail_geo::RAIL;

#[derive(Debug, thiserror::Error)]
pub enum StError {
    #[error("st: {0}")]
    Request(String),
    #[error("st: input query failed: {0}")]
    Input(#[source] Box<dyn std::error::Error + Send + Sync>),
}

fn bad(msg: impl Into<String>) -> StError {
    StError::Request(msg.into())
}

/// The values of one row, by column.
type Row = Vec<Option<Term>>;

/// The handler of the `st` SERVICE.
#[derive(Clone)]
pub struct StService {
    store: Dataset,
    evaluator: Arc<dyn Fn() -> SparqlEvaluator + Send + Sync>,
    /// The calling query's token: its sub-selects stop with it (a timeout).
    cancellation: Option<oxigraph::sparql::CancellationToken>,
    /// The dataset its sub-selects read: the calling query's, for a caller who reads only
    /// some graphs (#203). `None` reads the store's own default graph and named graphs.
    dataset: Option<oxigraph::sparql::QueryDataset>,
}

impl StService {
    pub fn new(
        store: Dataset,
        evaluator: impl Fn() -> SparqlEvaluator + Send + Sync + 'static,
    ) -> Self {
        Self {
            store,
            evaluator: Arc::new(evaluator),
            cancellation: None,
            dataset: None,
        }
    }

    /// The same service, its sub-selects reading `dataset` only (#203).
    #[must_use]
    pub fn with_dataset(&self, dataset: oxigraph::sparql::QueryDataset) -> Self {
        Self {
            dataset: Some(dataset),
            ..self.clone()
        }
    }

    /// A sub-select prepared on this service's dataset.
    fn prepare(&self, query: spargebra::Query) -> oxigraph::sparql::PreparedSparqlQuery {
        let mut prepared = self.evaluator().for_query(query);
        if let Some(dataset) = &self.dataset {
            *prepared.dataset_mut() = dataset.clone();
        }
        prepared
    }

    /// The same service, its sub-selects stopping once `token` is cancelled.
    #[must_use]
    pub fn with_cancellation_token(&self, token: oxigraph::sparql::CancellationToken) -> Self {
        Self {
            cancellation: Some(token),
            ..self.clone()
        }
    }

    /// An evaluator for a sub-select, with the calling query's token.
    fn evaluator(&self) -> SparqlEvaluator {
        let evaluator = (self.evaluator)();
        match &self.cancellation {
            Some(token) => evaluator.with_cancellation_token(token.clone()),
            None => evaluator,
        }
    }

    /// Rows of a sub-select: its variables and their values.
    fn rows(&self, pattern: &GraphPattern) -> Result<(Vec<Variable>, Vec<Row>), StError> {
        let query = spargebra::Query::Select {
            dataset: None,
            pattern: pattern.clone(),
            base_iri: None,
        };
        let QueryResults::Solutions(solutions) = self
            .store
            .execute(self.prepare(query))
            .map_err(|e| StError::Input(Box::new(e)))?
        else {
            return Err(bad("the input query must be a SELECT"));
        };
        let variables = solutions.variables().to_vec();
        let mut rows = Vec::new();
        for s in solutions {
            let s = s.map_err(|e| StError::Input(Box::new(e)))?;
            rows.push(variables.iter().map(|v| s.get(v).cloned()).collect());
        }
        Ok((variables, rows))
    }
}

fn collect(
    pattern: &GraphPattern,
    triples: &mut Vec<(String, TermPattern)>,
    subselects: &mut Vec<GraphPattern>,
) -> Result<(), StError> {
    match pattern {
        GraphPattern::Bgp { patterns } => {
            for t in patterns {
                let NamedNodePattern::NamedNode(p) = &t.predicate else {
                    return Err(bad("variable predicates are not allowed"));
                };
                triples.push((p.as_str().to_owned(), t.object.clone()));
            }
            Ok(())
        }
        GraphPattern::Join { left, right } => {
            collect(left, triples, subselects)?;
            collect(right, triples, subselects)
        }
        GraphPattern::Project { .. }
        | GraphPattern::Distinct { .. }
        | GraphPattern::Reduced { .. }
        | GraphPattern::Slice { .. }
        | GraphPattern::OrderBy { .. } => {
            subselects.push(pattern.clone());
            Ok(())
        }
        other => Err(bad(format!("unsupported pattern in st SERVICE: {other}"))),
    }
}

fn number(t: &Term, name: &str) -> Result<f64, StError> {
    match t {
        Term::Literal(l) => l
            .value()
            .parse()
            .map_err(|_| bad(format!("st:{name} must be numeric"))),
        _ => Err(bad(format!("st:{name} must be numeric"))),
    }
}

fn int(v: i64) -> Term {
    Literal::from(v).into()
}

/// A row of postgis_srs and postgis_srs_search.
fn srs_row(s: &srs::Srs) -> Vec<(&'static str, Term)> {
    let text = |v: &str| -> Term { Literal::new_simple_literal(v).into() };
    vec![
        ("auth_name", text(s.auth_name)),
        ("auth_srid", text(&s.auth_srid)),
        ("srname", text(&s.srname)),
        ("srtext", text(&s.srtext)),
        ("proj4text", text(&s.proj4text)),
        ("point_sw", s.point_sw()),
        ("point_ne", s.point_ne()),
        (
            "crs",
            oxigraph::model::NamedNode::new_unchecked(s.iri()).into(),
        ),
    ]
}

/// What a function produces for its inputs.
enum Output {
    /// Rows per input row: named columns.
    PerRow(Vec<Vec<Vec<(&'static str, Term)>>>),
    /// One value per input row (window functions).
    Window(&'static str, Vec<Option<Term>>),
    /// Rows not tied to an input row.
    Rows(Vec<Vec<(&'static str, Term)>>),
    /// Rows per input row whose columns vary: a column a row lacks is unbound.
    Columns(Vec<Vec<Vec<(String, Term)>>>),
}

impl ServiceHandler for StService {
    type Error = StError;

    fn handle(
        &self,
        pattern: &GraphPattern,
        _base_iri: Option<&Iri<String>>,
    ) -> Result<QuerySolutionIter<'static>, StError> {
        let mut triples = Vec::new();
        let mut subselects = Vec::new();
        collect(pattern, &mut triples, &mut subselects)?;
        let mut function = None;
        let mut params: HashMap<String, Term> = HashMap::new();
        let mut outputs: Vec<(String, Variable)> = Vec::new();
        for (predicate, object) in triples {
            let name = predicate
                .strip_prefix(ST)
                .ok_or_else(|| bad(format!("unknown predicate <{predicate}>")))?
                .to_owned();
            match object {
                TermPattern::Variable(v) => outputs.push((name, v)),
                TermPattern::NamedNode(n) if name == "function" => {
                    function = Some(match n.as_str().strip_prefix(ST) {
                        Some(f) => f.to_owned(),
                        None => format!(
                            "rail:{}",
                            n.as_str()
                                .strip_prefix(RAIL)
                                .ok_or_else(|| bad(format!("unknown function {n}")))?
                        ),
                    });
                }
                TermPattern::NamedNode(n) => {
                    params.insert(name, n.into());
                }
                TermPattern::Literal(l) => {
                    params.insert(name, l.into());
                }
                TermPattern::BlankNode(_) => {
                    return Err(bad(format!("st:{name} needs a value or a variable")));
                }
                #[allow(unreachable_patterns)]
                _ => return Err(bad(format!("unsupported value for st:{name}"))),
            }
        }
        let function = function.ok_or_else(|| bad("st:function is required"))?;
        let max_inputs = if function == "rail:conflate" { 2 } else { 1 };
        if subselects.len() > max_inputs {
            return Err(bad(format!("at most {max_inputs} input sub-select(s)")));
        }

        // Input rows: (passthrough values, geometry).
        let (input_vars, input_rows) = match subselects.first() {
            Some(p) => self.rows(p)?,
            None => (Vec::new(), Vec::new()),
        };
        let second = subselects.get(1).map(|p| self.rows(p)).transpose()?;
        let early_text = |name: &str| -> Result<Option<String>, StError> {
            match params.get(name) {
                None => Ok(None),
                Some(Term::Literal(l)) => Ok(Some(l.value().to_owned())),
                Some(_) => Err(bad(format!("st:{name} must be a literal"))),
            }
        };
        // The input column: ?geom (or the one st:geom_name names), ?data for
        // ST_FromFlatGeobuf.
        let input_name = if function == "ST_FromFlatGeobuf" {
            "data".to_owned()
        } else {
            early_text("geom_name")?.unwrap_or_else(|| "geom".to_owned())
        };
        let input_name = input_name.trim_start_matches('?').to_owned();
        let geom_col = input_vars.iter().position(|v| v.as_str() == input_name);
        let partition_col = input_vars.iter().position(|v| v.as_str() == "partition");
        let constant = if function == "ST_FromFlatGeobuf" {
            params.get("data").cloned()
        } else {
            params.get("geom").or_else(|| params.get("bounds")).cloned()
        };
        let inputs: Vec<Option<Term>> = match (&constant, subselects.is_empty()) {
            // postgis_srs reads codes, not geometries.
            (_, true) if function == "srs" => Vec::new(),
            (Some(g), true) => vec![Some(g.clone())],
            (None, true) => {
                return Err(bad(format!("give st:{input_name} or an input sub-select")));
            }
            (_, false) => {
                let col = geom_col.ok_or_else(|| {
                    bad(format!("the input sub-select must project ?{input_name}"))
                })?;
                input_rows.iter().map(|r| r[col].clone()).collect()
            }
        };
        let param = |name: &str| -> Result<Option<f64>, StError> {
            params.get(name).map(|t| number(t, name)).transpose()
        };
        let required = |name: &str| -> Result<f64, StError> {
            param(name)?.ok_or_else(|| bad(format!("st:{name} is required")))
        };
        let text = |name: &str| -> Result<Option<&str>, StError> {
            match params.get(name) {
                None => Ok(None),
                Some(Term::Literal(l)) => Ok(Some(l.value())),
                Some(_) => Err(bad(format!("st:{name} must be a literal"))),
            }
        };
        let geom_rows = |kind: DumpKind| -> Output {
            Output::PerRow(
                inputs
                    .iter()
                    .map(|g| {
                        g.as_ref()
                            .and_then(|g| sets::dump(kind, g))
                            .unwrap_or_default()
                            .into_iter()
                            .map(|(p, g)| {
                                vec![("path", Literal::new_simple_literal(p).into()), ("geom", g)]
                            })
                            .collect()
                    })
                    .collect(),
            )
        };
        // Window functions over partitions.
        let windowed_terms = |f: &dyn Fn(&[Term]) -> Vec<Option<Term>>| -> Vec<Option<Term>> {
            let mut groups: HashMap<Option<Term>, Vec<usize>> = HashMap::new();
            for i in 0..inputs.len() {
                let key = partition_col.and_then(|c| input_rows.get(i)?[c].clone());
                groups.entry(key).or_default().push(i);
            }
            let mut out = vec![None; inputs.len()];
            for members in groups.values() {
                let present: Vec<usize> = members
                    .iter()
                    .copied()
                    .filter(|&i| inputs[i].is_some())
                    .collect();
                let terms: Vec<Term> = present
                    .iter()
                    .map(|&i| inputs[i].clone().unwrap())
                    .collect();
                for (slot, v) in present.iter().zip(f(&terms)) {
                    out[*slot] = v;
                }
            }
            out
        };
        let windowed = |f: &dyn Fn(&[Term]) -> Vec<Option<i64>>| -> Vec<Option<Term>> {
            windowed_terms(&|t| f(t).into_iter().map(|v| v.map(int)).collect())
        };
        let output = match function.as_str() {
            "ST_Dump" => geom_rows(DumpKind::Parts),
            "ST_DumpPoints" => geom_rows(DumpKind::Points),
            "ST_DumpSegments" => geom_rows(DumpKind::Segments),
            "ST_DumpRings" => geom_rows(DumpKind::Rings),
            "ST_Subdivide" => {
                let max = param("max_vertices")?.unwrap_or(256.0) as usize;
                Output::PerRow(
                    inputs
                        .iter()
                        .map(|g| {
                            g.as_ref()
                                .and_then(|g| sets::subdivide(g, max))
                                .unwrap_or_default()
                                .into_iter()
                                .map(|g| vec![("geom", g)])
                                .collect()
                        })
                        .collect(),
                )
            }
            "ST_HexagonGrid" | "ST_SquareGrid" => {
                let size = required("size")?;
                let hex = function == "ST_HexagonGrid";
                Output::PerRow(
                    inputs
                        .iter()
                        .map(|g| {
                            g.as_ref()
                                .and_then(|g| sets::grid(hex, size, g))
                                .unwrap_or_default()
                                .into_iter()
                                .map(|(g, i, j)| vec![("geom", g), ("i", int(i)), ("j", int(j))])
                                .collect()
                        })
                        .collect(),
                )
            }
            "ST_ClusterDBSCAN" => {
                let eps = required("eps")?;
                let min = required("minpoints")? as usize;
                Output::Window(
                    "cluster_id",
                    windowed(&|t| sets::cluster_dbscan(t, eps, min)),
                )
            }
            "ST_ClusterKMeans" => {
                let k = required("k")? as usize;
                let r = param("max_radius")?;
                Output::Window("cluster_id", windowed(&|t| sets::cluster_kmeans(t, k, r)))
            }
            "ST_ClusterIntersectingWin" => {
                Output::Window("cluster_id", windowed(&|t| sets::cluster_within(t, None)))
            }
            "ST_ClusterWithinWin" => {
                let d = required("distance")?;
                Output::Window(
                    "cluster_id",
                    windowed(&|t| sets::cluster_within(t, Some(d))),
                )
            }
            "ST_CoverageInvalidEdges" | "ST_CoverageSimplify" | "ST_CoverageClean" => {
                let (kind, value) = match function.as_str() {
                    "ST_CoverageInvalidEdges" => (
                        sets::CoverageKind::InvalidEdges,
                        param("tolerance")?.unwrap_or(0.0),
                    ),
                    "ST_CoverageSimplify" => (sets::CoverageKind::Simplify, required("tolerance")?),
                    _ => (
                        sets::CoverageKind::Clean,
                        param("gap_width")?.unwrap_or(0.0),
                    ),
                };
                Output::Window("geom", windowed_terms(&|t| sets::coverage(kind, t, value)))
            }
            "ST_ClusterIntersecting" | "ST_ClusterWithin" => {
                let d = if function == "ST_ClusterWithin" {
                    Some(required("distance")?)
                } else {
                    None
                };
                let terms: Vec<Term> = inputs.iter().flatten().cloned().collect();
                let ids = sets::cluster_within(&terms, d);
                Output::Rows(
                    sets::collect_clusters(&terms, &ids)
                        .into_iter()
                        .map(|g| vec![("geom", g)])
                        .collect(),
                )
            }
            "ST_AsMVT" => {
                let name = text("name")?.unwrap_or("default").to_owned();
                let extent = param("extent")?.unwrap_or(4096.0);
                if !(extent >= 1.0 && extent <= f64::from(u32::MAX)) {
                    return Err(bad("st:extent must be a positive integer"));
                }
                let id_col = text("feature_id_name")?
                    .map(|n| {
                        input_vars
                            .iter()
                            .position(|v| v.as_str() == n.trim_start_matches('?'))
                            .ok_or_else(|| bad(format!("the input has no column ?{n}")))
                    })
                    .transpose()?;
                // With st:bounds, input geometries are put in tile space as by ST_AsMVTGeom.
                let bounds = params
                    .get("bounds")
                    .filter(|_| !subselects.is_empty())
                    .map(|b| {
                        tripletrail_geo::parse_wkt_literal(b)
                            .ok_or_else(|| bad("st:bounds must be a geometry"))
                    })
                    .transpose()?;
                let buffer = param("buffer")?.unwrap_or(256.0);
                let clip = match text("clip_geom")? {
                    None | Some("true" | "1") => true,
                    Some("false" | "0") => false,
                    Some(_) => return Err(bad("st:clip_geom must be a boolean")),
                };
                let mut features = Vec::new();
                for (i, g) in inputs.iter().enumerate() {
                    let Some(g) = g.as_ref().and_then(tripletrail_geo::parse_wkt_literal) else {
                        continue;
                    };
                    let geometry = match &bounds {
                        Some(b) => match mvt::as_mvt_geom(&g, b, extent, buffer, clip) {
                            Some(t) => t,
                            None => continue,
                        },
                        None => g.geometry.clone(),
                    };
                    let row = input_rows.get(i);
                    let id = id_col.and_then(|c| row?[c].as_ref()).and_then(|t| match t {
                        Term::Literal(l) => l.value().parse::<u64>().ok(),
                        _ => None,
                    });
                    let properties = row
                        .map(|r| {
                            input_vars
                                .iter()
                                .enumerate()
                                .filter(|(j, _)| Some(*j) != geom_col && Some(*j) != id_col)
                                .filter_map(|(j, v)| Some((v.as_str().to_owned(), r[j].clone()?)))
                                .collect()
                        })
                        .unwrap_or_default();
                    features.push(mvt::Feature {
                        geometry,
                        id,
                        properties,
                    });
                }
                let tile = mvt::encode_tile(&name, extent as u32, &features);
                Output::Rows(vec![vec![("mvt", mvt::tile_literal(&tile))]])
            }
            "rail:conflate" => {
                let Some((target_vars, target_rows)) = &second else {
                    return Err(bad("rail:conflate needs a source and a target sub-select"));
                };
                let col = |vars: &[Variable], name: &str| {
                    vars.iter().position(|v| v.as_str() == name).ok_or_else(|| {
                        bad(format!("each rail:conflate input must project ?{name}"))
                    })
                };
                let (sid, sgeom) = (col(&input_vars, "id")?, col(&input_vars, "geom")?);
                let (tid, tgeom) = (col(target_vars, "id")?, col(target_vars, "geom")?);
                let pairs = |rows: &[Row], id: usize, g: usize| -> Vec<(Term, Term)> {
                    rows.iter()
                        .filter_map(|r| Some((r[id].clone()?, r[g].clone()?)))
                        .collect()
                };
                let sources = pairs(&input_rows, sid, sgeom);
                let targets = pairs(target_rows, tid, tgeom);
                let tolerance = required("tolerance")?;
                let method = match text("method")? {
                    None | Some("frechet") => rail::ConflateMethod::Frechet,
                    Some("hausdorff") => rail::ConflateMethod::Hausdorff,
                    Some(m) => return Err(bad(format!("unknown st:method {m}"))),
                };
                let min_overlap = param("min_overlap")?.unwrap_or(0.0);
                let geoms = |p: &[(Term, Term)]| p.iter().map(|x| x.1.clone()).collect::<Vec<_>>();
                let matches = rail::conflate(&geoms(&sources), &geoms(&targets), tolerance, method);
                Output::Rows(
                    matches
                        .into_iter()
                        .filter(|m| m.overlap >= min_overlap)
                        .map(|m| {
                            vec![
                                ("source", sources[m.source].0.clone()),
                                ("target", targets[m.target].0.clone()),
                                ("distance", Literal::from(m.distance).into()),
                                ("overlap", Literal::from(m.overlap).into()),
                            ]
                        })
                        .collect(),
                )
            }
            "ST_AsFlatGeobuf" | "ST_AsGeobuf" => {
                let index = match text("index")? {
                    None | Some("false" | "0") => false,
                    Some("true" | "1") => true,
                    Some(_) => return Err(bad("st:index must be a boolean")),
                };
                let mut features = Vec::new();
                for (i, g) in inputs.iter().enumerate() {
                    let geom = match g {
                        Some(t) => {
                            Some(tripletrail_geo::parse_wkt_literal(t).ok_or_else(|| {
                                bad(format!("?{input_name} must hold geometries"))
                            })?)
                        }
                        None => None,
                    };
                    // The other columns become properties, as PostGIS's row argument.
                    let properties = input_rows
                        .get(i)
                        .map(|r| {
                            input_vars
                                .iter()
                                .enumerate()
                                .filter(|(j, _)| Some(*j) != geom_col)
                                .filter_map(|(j, v)| Some((v.as_str().to_owned(), r[j].clone()?)))
                                .collect()
                        })
                        .unwrap_or_default();
                    features.push(formats::Feature { geom, properties });
                }
                Output::Rows(vec![if function == "ST_AsFlatGeobuf" {
                    vec![(
                        "flatgeobuf",
                        formats::hex_literal(&formats::encode_flatgeobuf(&features, index, None)),
                    )]
                } else {
                    vec![(
                        "geobuf",
                        formats::hex_literal(&formats::encode_geobuf(&features)),
                    )]
                }])
            }
            "ST_FromFlatGeobuf" => {
                let mut per = Vec::with_capacity(inputs.len());
                for d in &inputs {
                    let Some(d) = d else {
                        per.push(Vec::new());
                        continue;
                    };
                    let bytes = formats::hex_bytes(d)
                        .ok_or_else(|| bad("st:data must be FlatGeobuf as xsd:hexBinary"))?;
                    let features = formats::decode_flatgeobuf(&bytes)
                        .ok_or_else(|| bad("st:data is not a FlatGeobuf file"))?;
                    per.push(
                        features
                            .into_iter()
                            .enumerate()
                            .map(|(fid, f)| {
                                let mut cols: Vec<(String, Term)> =
                                    vec![("fid".to_owned(), int(fid as i64))];
                                if let Some(g) = f.geom {
                                    cols.push((
                                        "geom".to_owned(),
                                        tripletrail_geo::geom_literal(&g),
                                    ));
                                }
                                for (k, v) in f.properties {
                                    if k != "fid" && k != "geom" {
                                        cols.push((k, v));
                                    }
                                }
                                cols
                            })
                            .collect(),
                    );
                }
                Output::Columns(per)
            }
            "ST_IsValidDetail" => Output::Columns(
                inputs
                    .iter()
                    .map(|g| {
                        let Some((valid, reason, location)) =
                            g.as_ref().and_then(sets::is_valid_detail)
                        else {
                            return Vec::new();
                        };
                        let mut cols = vec![("valid".to_owned(), Literal::from(valid).into())];
                        if let Some(r) = reason {
                            cols.push(("reason".to_owned(), Literal::new_simple_literal(r).into()));
                        }
                        if let Some(l) = location {
                            cols.push(("location".to_owned(), l));
                        }
                        vec![cols]
                    })
                    .collect(),
            ),
            "srs" => {
                if !subselects.is_empty() {
                    return Err(bad(
                        "st:srs takes st:code (or st:auth_name and st:auth_srid)",
                    ));
                }
                let value = |name: &str| -> Option<String> {
                    params.get(name).map(|t| match t {
                        Term::NamedNode(n) => n.as_str().to_owned(),
                        Term::Literal(l) => l.value().to_owned(),
                        other => other.to_string(),
                    })
                };
                let found: Vec<srs::Srs> =
                    match (value("code"), value("auth_name"), value("auth_srid")) {
                        (Some(c), None, None) => srs::lookup(&c).into_iter().collect(),
                        (None, auth, Some(code)) => {
                            srs::srs(auth.as_deref().unwrap_or("EPSG"), &code)
                                .into_iter()
                                .collect()
                        }
                        // postgis_srs_all, or all of one authority.
                        (None, auth, None) => srs::all()
                            .into_iter()
                            .filter(|s| {
                                auth.as_ref()
                                    .is_none_or(|a| a.eq_ignore_ascii_case(s.auth_name))
                            })
                            .collect(),
                        _ => {
                            return Err(bad(
                                "give st:code, or st:auth_name and st:auth_srid, not both",
                            ));
                        }
                    };
                Output::Rows(found.iter().map(srs_row).collect())
            }
            "srsSearch" => {
                let auth = text("auth_name")?;
                Output::PerRow(
                    inputs
                        .iter()
                        .map(|g| {
                            g.as_ref()
                                .and_then(|g| srs::search(g, auth))
                                .unwrap_or_default()
                                .iter()
                                .map(srs_row)
                                .collect()
                        })
                        .collect(),
                )
            }
            other => return Err(bad(format!("unsupported function st:{other}"))),
        };

        // Result variables: the outputs, then the passed-through input columns.
        let mut variables: Vec<Variable> = outputs.iter().map(|(_, v)| v.clone()).collect();
        let passthrough: Vec<usize> = match output {
            Output::Rows(_) => Vec::new(),
            _ => (0..input_vars.len())
                .filter(|&i| !variables.contains(&input_vars[i]))
                .collect(),
        };
        variables.extend(passthrough.iter().map(|&i| input_vars[i].clone()));
        let row = |cols: &[(&'static str, Term)],
                   input: Option<usize>|
         -> Result<Vec<Option<Term>>, StError> {
            let mut values = Vec::with_capacity(variables.len());
            for (name, _) in &outputs {
                match cols.iter().find(|(n, _)| n == name) {
                    Some((_, t)) => values.push(Some(t.clone())),
                    None => return Err(bad(format!("{function} has no column st:{name}"))),
                }
            }
            for &c in &passthrough {
                values.push(input.and_then(|i| input_rows.get(i)?[c].clone()));
            }
            Ok(values)
        };
        let mut solutions: Vec<Vec<Option<Term>>> = Vec::new();
        match &output {
            Output::PerRow(per) => {
                for (i, rows) in per.iter().enumerate() {
                    for cols in rows {
                        solutions.push(row(cols, Some(i))?);
                    }
                }
            }
            Output::Window(name, values) => {
                for (i, v) in values.iter().enumerate() {
                    let mut cols = Vec::new();
                    if let Some(v) = v {
                        cols.push((*name, v.clone()));
                    }
                    if let Some(g) = inputs[i].as_ref().filter(|_| *name != "geom") {
                        cols.push(("geom", g.clone()));
                    }
                    // A noise row (no cluster) leaves the cluster column unbound.
                    let mut values = Vec::with_capacity(variables.len());
                    for (n, _) in &outputs {
                        values.push(cols.iter().find(|(c, _)| c == n).map(|(_, t)| t.clone()));
                    }
                    for &c in &passthrough {
                        values.push(input_rows.get(i).and_then(|r| r[c].clone()));
                    }
                    solutions.push(values);
                }
            }
            Output::Rows(rows) => {
                for cols in rows {
                    solutions.push(row(cols, None)?);
                }
            }
            Output::Columns(per) => {
                for (i, rows) in per.iter().enumerate() {
                    for cols in rows {
                        let mut values = Vec::with_capacity(variables.len());
                        for (n, _) in &outputs {
                            values.push(cols.iter().find(|(c, _)| c == n).map(|(_, t)| t.clone()));
                        }
                        for &c in &passthrough {
                            values.push(input_rows.get(i).and_then(|r| r[c].clone()));
                        }
                        solutions.push(values);
                    }
                }
            }
        }
        let variables: Arc<[Variable]> = variables.into();
        let rows: Vec<QuerySolution> = solutions
            .into_iter()
            .map(|r| QuerySolution::from((Arc::clone(&variables), r)))
            .collect();
        Ok(QuerySolutionIter::new(
            variables,
            rows.into_iter().map(Ok::<_, QueryEvaluationError>),
        ))
    }
}
