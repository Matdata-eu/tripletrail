//! Vehicle compatibility rules, compiled with one vehicle's values into plain SHACL.
//!
//! A rule compares a property of the vehicle with a property of what it runs on (an edge, a
//! track, a road segment, ...):
//!
//! ```turtle
//! @prefix compat: <http://w3id.org/tripletrail/compat#> .
//! ex:weight a compat:Rule ;
//!     compat:vehiclePath ex:grossWeight ;        # a SHACL path from the vehicle
//!     compat:edgePath ex:maxWeight ;             # a SHACL path from the focus node
//!     compat:constraint sh:minInclusive ;        # every edge value >= the vehicle's
//!     compat:whenMissing compat:Pass .           # an edge without a value passes (default)
//! ```
//!
//! `compat:constraint` is the SHACL constraint component the vehicle's values are plugged
//! into, applied to the edge's values:
//!
//! - `sh:minInclusive`, `sh:minExclusive`: every edge value is at least (above) the vehicle's
//!   (its highest, when it has several);
//! - `sh:maxInclusive`, `sh:maxExclusive`: every edge value is at most (below) the vehicle's
//!   (its lowest);
//! - `sh:hasValue`: the edge has one of the vehicle's values;
//! - `sh:in`: every edge value is one of the vehicle's.
//!
//! `compat:whenMissing compat:Fail` makes an edge without a value fail. `compat:noneValue`
//! names a value the vehicle may have that means "also runs where the edge has none" (say,
//! not electrified, for a bi-mode train): with it, an edge without a value passes, and the
//! none value is not compared. `compat:factor` multiplies the vehicle's value before a
//! numeric comparison (units: a vehicle weight in t against limits in kg is factor 1000).
//!
//! **Ordered categories.** With `compat:rank` (one or more SHACL paths, read in the rules
//! graph from a category to a number), a range constraint compares categories by their
//! ranks: an edge value is accepted when, for every rank, it is at least (`sh:minInclusive`;
//! above, at most, below for the others) the vehicle's value. Several ranks make a partial
//! order (EN 15528 line categories: axle load and mass per metre). A vehicle value is a
//! category (its ranks are compared) or, with a single rank, a number (times
//! `compat:factor`); with several vehicle values, an edge value accepted for any of them
//! passes (a vehicle may list the categories it runs on, or only the lowest). The categories
//! are those of the rules graph with a value for every rank, and the rule compiles to the
//! list of the rejected ones (`sh:not [ sh:in (...) ]`: an unranked edge value passes like a
//! missing one) or, with `compat:whenMissing compat:Fail`, of the accepted ones (`sh:in`).
//! A vehicle value without ranks is ignored.
//!
//! **Records.** A rule with `compat:field` sub-rules compares the vehicle with the records
//! the edge path reaches (a track's load capabilities, each a line category and a speed):
//! with `compat:constraint sh:hasValue` one record passes every field, with `sh:in` every
//! record does. `compat:selectBy` sub-rules pick the records that apply (those whose speed is
//! at least the vehicle's): they are checked only together with a field, so a vehicle with a
//! speed but none of the fields' values is not checked. Each sub-rule's edge path starts at
//! the record; a sub-rule the vehicle has no value for is left out.
//!
//! A rule whose vehicle has no value is not checked. The shape is one `sh:NodeShape` with a
//! property shape per checked rule (and an `sh:or` for `sh:hasValue` rules where a missing
//! value passes).

use crate::ShaclError;
use crate::graph::DataGraph;
use crate::path::{Path, write_list};
use crate::value::number;
use crate::vocab::{rdf, sh};
use oxigraph::model::{BlankNode, Graph, Literal, NamedNode, NamedNodeRef, Term, Triple};
use std::collections::BTreeSet;

/// The compatibility vocabulary's namespace.
pub const NS: &str = "http://w3id.org/tripletrail/compat#";

macro_rules! terms {
    ($($name:ident = $local:literal,)*) => {
        $(pub const $name: NamedNodeRef<'static> =
            NamedNodeRef::new_unchecked(concat!("http://w3id.org/tripletrail/compat#", $local));)*
    };
}

terms! {
    RULE = "Rule",
    VEHICLE_PATH = "vehiclePath",
    EDGE_PATH = "edgePath",
    CONSTRAINT = "constraint",
    WHEN_MISSING = "whenMissing",
    PASS = "Pass",
    FAIL = "Fail",
    NONE_VALUE = "noneValue",
    FACTOR = "factor",
    RANK = "rank",
    FIELD = "field",
    SELECT_BY = "selectBy",
}

/// Rules compiled for one vehicle.
#[derive(Debug)]
pub struct Compiled {
    /// The shapes, as SHACL triples.
    pub graph: Graph,
    /// The node shape every focus node is checked against.
    pub shape: Term,
    /// The rules checked, and those skipped because the vehicle has no value for them.
    pub checked: usize,
    pub skipped: usize,
    /// The predicates read to compile: the rules' and the vehicle paths'.
    pub reads: BTreeSet<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    MinInclusive,
    MinExclusive,
    MaxInclusive,
    MaxExclusive,
    HasValue,
    In,
}

impl Kind {
    fn parse(t: &Term) -> Option<Self> {
        let Term::NamedNode(n) = t else { return None };
        Some(match n.as_str().strip_prefix(sh::NS)? {
            "minInclusive" => Self::MinInclusive,
            "minExclusive" => Self::MinExclusive,
            "maxInclusive" => Self::MaxInclusive,
            "maxExclusive" => Self::MaxExclusive,
            "hasValue" => Self::HasValue,
            "in" => Self::In,
            _ => return None,
        })
    }

    fn component(self) -> NamedNodeRef<'static> {
        match self {
            Self::MinInclusive => sh::MIN_INCLUSIVE,
            Self::MinExclusive => sh::MIN_EXCLUSIVE,
            Self::MaxInclusive => sh::MAX_INCLUSIVE,
            Self::MaxExclusive => sh::MAX_EXCLUSIVE,
            Self::HasValue => sh::HAS_VALUE,
            Self::In => sh::IN,
        }
    }

    /// Whether an edge rank `edge` satisfies the range constraint against a vehicle rank.
    fn admits(self, edge: f64, vehicle: f64) -> bool {
        match self {
            Self::MinInclusive => edge >= vehicle,
            Self::MinExclusive => edge > vehicle,
            Self::MaxInclusive => edge <= vehicle,
            Self::MaxExclusive => edge < vehicle,
            Self::HasValue | Self::In => false,
        }
    }
}

/// The rules of a rules graph: subjects typed `compat:Rule` or with a `compat:edgePath`, in
/// a stable order, without the sub-rules of records.
fn rule_nodes(rules: &dyn DataGraph) -> Vec<Term> {
    let mut pairs = Vec::new();
    rules.with_predicate(rdf::TYPE, &mut pairs);
    let mut nodes: Vec<Term> = pairs
        .drain(..)
        .filter(|(_, o)| o.as_ref() == RULE.into())
        .map(|(s, _)| s)
        .collect();
    rules.with_predicate(EDGE_PATH, &mut pairs);
    nodes.extend(pairs.drain(..).map(|(s, _)| s));
    rules.with_predicate(FIELD, &mut pairs);
    rules.with_predicate(SELECT_BY, &mut pairs);
    let sub: Vec<Term> = pairs.into_iter().map(|(_, o)| o).collect();
    nodes.retain(|n| !sub.contains(n));
    nodes.sort_by_key(|t| t.to_string());
    nodes.dedup();
    nodes
}

fn add(g: &mut Graph, s: &Term, p: NamedNodeRef<'_>, o: impl Into<Term>) {
    let s = match s {
        Term::NamedNode(n) => oxigraph::model::NamedOrBlankNode::from(n.clone()),
        Term::BlankNode(b) => b.clone().into(),
        _ => unreachable!("shape nodes are IRIs or blank nodes"),
    };
    g.insert(&Triple::new(s, p.into_owned(), o.into()));
}

fn blank() -> Term {
    BlankNode::default().into()
}

fn bad(rule: &Term, msg: &str) -> ShaclError {
    ShaclError::Rules(format!("{rule}: {msg}"))
}

/// Compiles the rules of `rules` with the values of `vehicle` (read from `vehicles`) into a
/// node shape, at `root` (a blank node when `None`).
pub fn compile(
    rules: &dyn DataGraph,
    vehicles: &dyn DataGraph,
    vehicle: &Term,
    root: Option<NamedNode>,
) -> Result<Compiled, ShaclError> {
    let mut c = Compiler {
        rules,
        vehicles,
        vehicle,
        g: Graph::new(),
        reads: BTreeSet::new(),
    };
    let shape: Term = root.map_or_else(blank, Term::from);
    add(&mut c.g, &shape, rdf::TYPE, sh::NODE_SHAPE.into_owned());
    let mut predicates = Vec::new();
    rules.predicates(&mut predicates);
    c.reads
        .extend(predicates.iter().map(|p| p.as_str().to_owned()));
    let (mut checked, mut skipped) = (0, 0);
    for rule in rule_nodes(rules) {
        if c.rule(&rule, &shape, 0)? {
            checked += 1;
        } else {
            skipped += 1;
        }
    }
    Ok(Compiled {
        graph: c.g,
        shape,
        checked,
        skipped,
        reads: c.reads,
    })
}

struct Compiler<'a> {
    rules: &'a dyn DataGraph,
    vehicles: &'a dyn DataGraph,
    vehicle: &'a Term,
    g: Graph,
    reads: BTreeSet<String>,
}

impl Compiler<'_> {
    /// Adds the constraints of `rule` to the node shape `shape`; whether it is checked.
    fn rule(&mut self, rule: &Term, shape: &Term, depth: usize) -> Result<bool, ShaclError> {
        if depth > 8 {
            return Err(bad(rule, "records are nested too deep"));
        }
        let rules = self.rules;
        let required = |p: NamedNodeRef<'_>| {
            rules
                .object(rule, p)
                .ok_or_else(|| bad(rule, &format!("missing {p}")))
        };
        let edge_path = Path::parse(rules, &required(EDGE_PATH)?)?;
        let kind = Kind::parse(&required(CONSTRAINT)?).ok_or_else(|| {
            bad(
                rule,
                "compat:constraint is one of sh:minInclusive, sh:minExclusive, sh:maxInclusive, sh:maxExclusive, sh:hasValue, sh:in",
            )
        })?;
        let fail_when_missing = match rules.object(rule, WHEN_MISSING) {
            None => false,
            Some(t) if t.as_ref() == PASS.into() => false,
            Some(t) if t.as_ref() == FAIL.into() => true,
            Some(_) => {
                return Err(bad(
                    rule,
                    "compat:whenMissing is compat:Pass or compat:Fail",
                ));
            }
        };
        edge_path.predicates(&mut self.reads);
        let mut fields = Vec::new();
        rules.objects(rule, FIELD, &mut fields);
        if !fields.is_empty() {
            return self.record(
                rule,
                shape,
                depth,
                &edge_path,
                kind,
                fail_when_missing,
                fields,
            );
        }

        let vehicle_path = Path::parse(rules, &required(VEHICLE_PATH)?)?;
        let mut none_values = Vec::new();
        rules.objects(rule, NONE_VALUE, &mut none_values);
        let factor = match rules.object(rule, FACTOR) {
            None => None,
            Some(f) => Some(number(&f).ok_or_else(|| bad(rule, "compat:factor is a number"))?),
        };
        let mut ranks = Vec::new();
        rules.objects(rule, RANK, &mut ranks);
        let ranks = ranks
            .iter()
            .map(|r| Path::parse(rules, r))
            .collect::<Result<Vec<_>, _>>()?;
        if !ranks.is_empty() && matches!(kind, Kind::HasValue | Kind::In) {
            return Err(bad(
                rule,
                "compat:rank goes with sh:minInclusive, sh:minExclusive, sh:maxInclusive or sh:maxExclusive",
            ));
        }
        vehicle_path.predicates(&mut self.reads);
        for r in &ranks {
            r.predicates(&mut self.reads);
        }

        let mut values = vehicle_path.values(self.vehicles, self.vehicle);
        if values.is_empty() {
            return Ok(false);
        }
        let none = values.iter().any(|v| none_values.contains(v));
        values.retain(|v| !none_values.contains(v));
        let missing_passes = none || !fail_when_missing;
        let g = &mut self.g;

        let property = |g: &mut Graph| {
            let p = blank();
            let path = edge_path.write(g);
            add(g, &p, sh::PATH, path);
            if let Term::NamedNode(_) = rule {
                add(
                    g,
                    &p,
                    NamedNodeRef::new_unchecked("http://www.w3.org/2000/01/rdf-schema#seeAlso"),
                    rule.clone(),
                );
            }
            p
        };
        if values.is_empty() {
            // Only the none value: the vehicle runs only where the edge has no value.
            let p = property(g);
            add(g, &p, sh::MAX_COUNT, Literal::from(0));
            add(g, shape, sh::PROPERTY, p);
            return Ok(true);
        }
        if !ranks.is_empty() {
            let Some((accepted, rejected)) = ranked(rules, rule, &ranks, &values, kind, factor)?
            else {
                // None of the vehicle's values is ranked.
                return Ok(false);
            };
            let p = property(g);
            if missing_passes {
                // An unranked edge value passes like a missing one.
                let q = blank();
                let list = write_list(g, rejected);
                add(g, &q, sh::IN, list);
                add(g, &p, sh::NOT, q);
            } else {
                let list = write_list(g, accepted);
                add(g, &p, sh::IN, list);
                add(g, &p, sh::MIN_COUNT, Literal::from(1));
            }
            add(g, shape, sh::PROPERTY, p);
            return Ok(true);
        }
        match kind {
            Kind::MinInclusive | Kind::MinExclusive | Kind::MaxInclusive | Kind::MaxExclusive => {
                let mut best: Option<(f64, &Term)> = None;
                for v in &values {
                    let n = number(v).ok_or_else(|| {
                        bad(rule, &format!("the vehicle's value {v} is not a number"))
                    })? * factor.unwrap_or(1.0);
                    let better = match (best, kind) {
                        (None, _) => true,
                        (Some((b, _)), Kind::MinInclusive | Kind::MinExclusive) => n > b,
                        (Some((b, _)), _) => n < b,
                    };
                    if better {
                        best = Some((n, v));
                    }
                }
                let (n, original) = best.expect("values is not empty");
                let bound: Term = match factor {
                    None => original.clone(),
                    Some(_) => Literal::from(n).into(),
                };
                let p = property(g);
                add(g, &p, kind.component(), bound);
                if !missing_passes {
                    add(g, &p, sh::MIN_COUNT, Literal::from(1));
                }
                add(g, shape, sh::PROPERTY, p);
            }
            Kind::In => {
                let p = property(g);
                let list = write_list(g, values);
                add(g, &p, sh::IN, list);
                if !missing_passes {
                    add(g, &p, sh::MIN_COUNT, Literal::from(1));
                }
                add(g, shape, sh::PROPERTY, p);
            }
            Kind::HasValue => {
                let has = property(g);
                if let [v] = values.as_slice() {
                    add(g, &has, sh::HAS_VALUE, v.clone());
                } else {
                    let q = blank();
                    let list = write_list(g, values);
                    add(g, &q, sh::IN, list);
                    add(g, &has, sh::QUALIFIED_VALUE_SHAPE, q);
                    add(g, &has, sh::QUALIFIED_MIN_COUNT, Literal::from(1));
                }
                if missing_passes {
                    let absent = property(g);
                    add(g, &absent, sh::MAX_COUNT, Literal::from(0));
                    let either = write_list(g, vec![absent, has]);
                    add(g, shape, sh::OR, either);
                } else {
                    add(g, shape, sh::PROPERTY, has);
                }
            }
        }
        Ok(true)
    }

    /// A rule with `compat:field` sub-rules, over the records `edge_path` reaches.
    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        rule: &Term,
        shape: &Term,
        depth: usize,
        edge_path: &Path,
        kind: Kind,
        fail_when_missing: bool,
        mut fields: Vec<Term>,
    ) -> Result<bool, ShaclError> {
        if !matches!(kind, Kind::HasValue | Kind::In) {
            return Err(bad(
                rule,
                "a rule with compat:field has compat:constraint sh:hasValue (one record passes) or sh:in (every record does)",
            ));
        }
        fields.sort_by_key(|t| t.to_string());
        let mut selects = Vec::new();
        self.rules.objects(rule, SELECT_BY, &mut selects);
        selects.sort_by_key(|t| t.to_string());
        // The fields, then the selections, each into a node shape for one record.
        let passes = blank();
        let mut any = false;
        for f in &fields {
            any |= self.rule(f, &passes, depth + 1)?;
        }
        if !any {
            return Ok(false);
        }
        let applies = if kind == Kind::HasValue {
            passes.clone()
        } else {
            blank()
        };
        let mut selected = false;
        for s in &selects {
            selected |= self.rule(s, &applies, depth + 1)?;
        }
        let g = &mut self.g;
        let p = blank();
        let path = edge_path.write(g);
        add(g, &p, sh::PATH, path);
        if let Term::NamedNode(_) = rule {
            add(
                g,
                &p,
                NamedNodeRef::new_unchecked("http://www.w3.org/2000/01/rdf-schema#seeAlso"),
                rule.clone(),
            );
        }
        if kind == Kind::HasValue {
            // One record passes (and applies).
            add(g, &p, sh::QUALIFIED_VALUE_SHAPE, passes);
            add(g, &p, sh::QUALIFIED_MIN_COUNT, Literal::from(1));
            if fail_when_missing {
                add(g, shape, sh::PROPERTY, p);
            } else {
                let absent = blank();
                let path = edge_path.write(g);
                add(g, &absent, sh::PATH, path);
                add(g, &absent, sh::MAX_COUNT, Literal::from(0));
                let either = write_list(g, vec![absent, p]);
                add(g, shape, sh::OR, either);
            }
        } else {
            // Every record that applies passes.
            let each = if selected {
                let not_applies = blank();
                add(g, &not_applies, sh::NOT, applies);
                let either = write_list(g, vec![not_applies, passes]);
                let each = blank();
                add(g, &each, sh::OR, either);
                each
            } else {
                passes
            };
            add(g, &p, sh::NODE, each);
            if fail_when_missing {
                add(g, &p, sh::MIN_COUNT, Literal::from(1));
            }
            add(g, shape, sh::PROPERTY, p);
        }
        Ok(true)
    }
}

/// The ranks of `node` through `ranks` (every number of each), if it has one for each.
fn ranks_of(rules: &dyn DataGraph, ranks: &[Path], node: &Term) -> Option<Vec<Vec<f64>>> {
    ranks
        .iter()
        .map(|r| {
            let n: Vec<f64> = r.values(rules, node).iter().filter_map(number).collect();
            (!n.is_empty()).then_some(n)
        })
        .collect()
}

/// Categories accepted and rejected.
type Split = (Vec<Term>, Vec<Term>);

/// The ranked categories of the rules graph accepted for the vehicle's `values`, and those
/// rejected; `None` when no vehicle value is ranked.
fn ranked(
    rules: &dyn DataGraph,
    rule: &Term,
    ranks: &[Path],
    values: &[Term],
    kind: Kind,
    factor: Option<f64>,
) -> Result<Option<Split>, ShaclError> {
    let mut wanted = Vec::new();
    for v in values {
        if let Some(n) = number(v) {
            if ranks.len() != 1 {
                return Err(bad(
                    rule,
                    &format!(
                        "the vehicle's value {v} is a number, which needs a single compat:rank"
                    ),
                ));
            }
            wanted.push(vec![vec![n * factor.unwrap_or(1.0)]]);
        } else if let Some(r) = ranks_of(rules, ranks, v) {
            wanted.push(r);
        }
    }
    if wanted.is_empty() {
        return Ok(None);
    }
    // The categories: subjects of the predicates the ranks read.
    let mut preds = BTreeSet::new();
    for r in ranks {
        r.predicates(&mut preds);
    }
    let mut pairs = Vec::new();
    for p in &preds {
        rules.with_predicate(NamedNodeRef::new_unchecked(p), &mut pairs);
    }
    let mut candidates: Vec<Term> = pairs.into_iter().map(|(s, _)| s).collect();
    candidates.sort_by_key(|t| t.to_string());
    candidates.dedup();
    let (mut accepted, mut rejected) = (Vec::new(), Vec::new());
    for c in candidates {
        let Some(have) = ranks_of(rules, ranks, &c) else {
            continue;
        };
        let ok = wanted.iter().any(|want| {
            have.iter().zip(want).all(|(edge, vehicle)| {
                edge.iter()
                    .all(|e| vehicle.iter().all(|v| kind.admits(*e, *v)))
            })
        });
        if ok {
            accepted.push(c);
        } else {
            rejected.push(c);
        }
    }
    Ok(Some((accepted, rejected)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Shapes, Validator};
    use oxigraph::io::{RdfFormat, RdfParser};

    fn graph(ttl: &str) -> Graph {
        let mut g = Graph::new();
        let ttl = format!(
            "@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://example.com/> .
             @prefix compat: <http://w3id.org/tripletrail/compat#> .\n{ttl}"
        );
        for q in RdfParser::from_format(RdfFormat::Turtle).for_slice(ttl.as_bytes()) {
            g.insert(&q.unwrap().into());
        }
        g
    }

    fn ex(local: &str) -> Term {
        NamedNode::new_unchecked(format!("http://example.com/{local}")).into()
    }

    /// Line categories ranked by axle load and mass per metre (a partial order: B2 and C2
    /// differ in both directions from each other's neighbours), a numeric axle load rule, and
    /// a record rule over load capabilities (category, speed).
    const RULES: &str = r#"
        ex:A  ex:axle 16 ; ex:metre 5.0 .   ex:B1 ex:axle 18 ; ex:metre 5.0 .
        ex:B2 ex:axle 18 ; ex:metre 6.4 .   ex:C2 ex:axle 20 ; ex:metre 6.4 .
        ex:D2 ex:axle 22.5 ; ex:metre 6.4 . ex:D4 ex:axle 22.5 ; ex:metre 8.0 .
        ex:category a compat:Rule ; compat:vehiclePath ex:category ; compat:edgePath ex:category ;
            compat:constraint sh:minInclusive ; compat:rank ex:axle, ex:metre .
        ex:strict a compat:Rule ; compat:vehiclePath ex:strictCategory ;
            compat:edgePath ex:category ; compat:constraint sh:minInclusive ;
            compat:rank ex:axle, ex:metre ; compat:whenMissing compat:Fail .
        ex:axleKg a compat:Rule ; compat:vehiclePath ex:axleKg ; compat:edgePath ex:category ;
            compat:constraint sh:minInclusive ; compat:rank ex:axle ; compat:factor 0.001 .
        ex:load a compat:Rule ; compat:edgePath ex:load ; compat:constraint sh:hasValue ;
            compat:field [ compat:vehiclePath ex:loadCategory ; compat:edgePath ex:cat ;
                           compat:constraint sh:minInclusive ; compat:rank ex:axle, ex:metre ] ;
            compat:selectBy [ compat:vehiclePath ex:speed ; compat:edgePath ex:speed ;
                              compat:constraint sh:minInclusive ] .
        ex:everyLoad a compat:Rule ; compat:edgePath ex:load ; compat:constraint sh:in ;
            compat:field [ compat:vehiclePath ex:everyCategory ; compat:edgePath ex:cat ;
                           compat:constraint sh:minInclusive ; compat:rank ex:axle, ex:metre ] ;
            compat:selectBy [ compat:vehiclePath ex:speed ; compat:edgePath ex:speed ;
                              compat:constraint sh:minInclusive ] .
    "#;

    const DATA: &str = r#"
        ex:vB1 ex:category ex:B1 .         ex:vC2 ex:category ex:C2 .
        ex:vD4 ex:category ex:D4 .         ex:vB2orD2 ex:category ex:B2, ex:D2 .
        ex:vRA ex:category ex:RA5 .        ex:sD2 ex:strictCategory ex:D2 .
        ex:kg19 ex:axleKg 19000 .
        ex:lD4 ex:loadCategory ex:D4 .     ex:lD4fast ex:loadCategory ex:D4 ; ex:speed 120 .
        ex:fast ex:speed 120 .             ex:eC2fast ex:everyCategory ex:C2 ; ex:speed 120 .
        ex:tA ex:category ex:A .   ex:tB2 ex:category ex:B2 .   ex:tC2 ex:category ex:C2 .
        ex:tD2 ex:category ex:D2 . ex:tD4 ex:category ex:D4 .   ex:tRA ex:category ex:RA5 .
        ex:tNone ex:other 1 .
        ex:tLoad ex:load [ ex:cat ex:D4 ; ex:speed 100 ] , [ ex:cat ex:C2 ; ex:speed 120 ] .
        ex:tLoadAny ex:load [ ex:cat ex:D4 ] .
    "#;

    fn conforms(vehicle: &str, edges: &[&str]) -> Vec<bool> {
        let (rules, data) = (graph(RULES), graph(DATA));
        let c = compile(&rules, &data, &ex(vehicle), None).unwrap();
        let s = Shapes::compile_with(&c.graph, std::slice::from_ref(&c.shape)).unwrap();
        let id = s.shape(&c.shape).unwrap();
        let v = Validator::new(&s, &data);
        edges.iter().map(|e| v.conforms_to(&ex(e), id)).collect()
    }

    const TRACKS: [&str; 7] = ["tA", "tB2", "tC2", "tD2", "tD4", "tRA", "tNone"];

    #[test]
    fn ordered_categories() {
        // B1: everything from B1 up in both ranks (not A), unranked and missing pass.
        assert_eq!(
            conforms("vB1", &TRACKS),
            [false, true, true, true, true, true, true]
        );
        // C2: not B2 (lower axle load) nor A; D2 and D4 are at least C2 in both ranks.
        assert_eq!(
            conforms("vC2", &TRACKS),
            [false, false, true, true, true, true, true]
        );
        // D4: only D4.
        assert_eq!(
            conforms("vD4", &TRACKS),
            [false, false, false, false, true, true, true]
        );
        // B2 or D2: any category at least one of them (C2 is above B2, not D2).
        assert_eq!(
            conforms("vB2orD2", &TRACKS),
            [false, true, true, true, true, true, true]
        );
        // An unranked vehicle category: the rule is not checked.
        let (rules, data) = (graph(RULES), graph(DATA));
        let c = compile(&rules, &data, &ex("vRA"), None).unwrap();
        assert_eq!((c.checked, c.skipped), (0, 5));
        // With compat:Fail, the accepted list: unranked and missing fail.
        assert_eq!(
            conforms("sD2", &TRACKS),
            [false, false, false, true, true, false, false]
        );
        // A number against one rank: 19 t needs 20 t (C2) and up.
        assert_eq!(
            conforms("kg19", &TRACKS),
            [false, false, true, true, true, true, true]
        );
    }

    #[test]
    fn records() {
        let tracks = ["tLoad", "tLoadAny", "tC2", "tNone"];
        // D4 at any speed: tLoad's D4 record; tLoadAny; no records passes.
        assert_eq!(conforms("lD4", &tracks), [true, true, true, true]);
        // D4 at 120: of tLoad's records only the C2 one applies at 120, so tLoad fails; a
        // record without a speed applies at any speed (tLoadAny).
        assert_eq!(conforms("lD4fast", &tracks), [false, true, true, true]);
        // A speed alone: not checked.
        let (rules, data) = (graph(RULES), graph(DATA));
        let c = compile(&rules, &data, &ex("fast"), None).unwrap();
        assert_eq!(c.checked, 0);
        // Every record at 120 or more is at least C2.
        assert_eq!(conforms("eC2fast", &tracks), [true, true, true, true]);
    }

    #[test]
    fn bad_rank_rules() {
        let rules = graph(
            "ex:r compat:vehiclePath ex:a ; compat:edgePath ex:b ; compat:constraint sh:hasValue ;
             compat:rank ex:x .",
        );
        assert!(compile(&rules, &graph("ex:v ex:a ex:c ."), &ex("v"), None).is_err());
        let rules = graph(
            "ex:r compat:vehiclePath ex:a ; compat:edgePath ex:b ; compat:constraint sh:minInclusive ;
             compat:rank ex:x, ex:y .",
        );
        assert!(compile(&rules, &graph("ex:v ex:a 3 ."), &ex("v"), None).is_err());
        let rules = graph(
            "ex:r compat:edgePath ex:b ; compat:constraint sh:minInclusive ;
             compat:field [ compat:vehiclePath ex:a ; compat:edgePath ex:c ; compat:constraint sh:in ] .",
        );
        assert!(compile(&rules, &graph("ex:v ex:a ex:c ."), &ex("v"), None).is_err());
    }
}
