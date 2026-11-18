//! Property functions (#232): triple patterns whose predicate is a registered IRI and that
//! produce rows, as in Jena (`?list list:member ?m`, `?part apf:strSplit (?s ";")`).
//!
//! A property function in a group is rewritten into a `LATERAL` step after the rest of the
//! group: the step is evaluated once per row of what comes before it, so its arguments see the
//! values bound there, and its rows join the group like those of any pattern. Each step is
//! written in plain algebra, which the evaluator already runs:
//!
//! - RDF lists are walked with property paths on `rdf:first` / `rdf:rest` (`list:index`
//!   counts the nodes before a member, so it is quadratic in the list's length).
//! - Containers (`tt:member`, `apf:bag` / `seq` / `alt`) match the `rdf:_n` properties.
//!   A plain `rdfs:member` triple is left alone: rewriting it would turn an index lookup
//!   into a scan of every quad.
//! - `apf:strSplit` makes its rows from a `VALUES` block of 16 numbers, filtered by the number
//!   of parts, or for longer strings from three nested blocks of 32: up to 32,768 parts.
//! - `apf:splitIRI`, `apf:concat` and `apf:assign` are scalar: `BIND`s.
//!
//! An argument list `(a b)` is parsed as blank nodes with `rdf:first` / `rdf:rest` triples; the
//! rewrite reads it back and removes those triples.

use spargebra::algebra::{
    AggregateExpression, AggregateFunction, Expression, Function, GraphPattern,
    PropertyPathExpression,
};
use spargebra::term::{
    GroundTerm, Literal, NamedNode, NamedNodePattern, TermPattern, TriplePattern, Variable,
};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const RDFS_MEMBER: &str = "http://www.w3.org/2000/01/rdf-schema#member";
/// Jena list property functions.
pub const LIST: &str = "http://jena.apache.org/ARQ/list#";
/// Jena property functions.
pub const APF: &str = "http://jena.apache.org/ARQ/property#";
/// jena-geosparql's spatial property functions.
pub const SPATIAL: &str = "http://jena.apache.org/spatial#";
const SPATIALF: &str = "http://jena.apache.org/function/spatial#";
const GEO: &str = "http://www.opengis.net/ont/geosparql#";
const GEOF: &str = "http://www.opengis.net/def/function/geosparql/";
const WGS84: &str = "http://www.w3.org/2003/01/geo/wgs84_pos#";
const CRS84: &str = "http://www.opengis.net/def/crs/OGC/1.3/CRS84";
const UOM: &str = "http://www.opengis.net/def/uom/OGC/1.0/";
const MG: &str = crate::ttfn::MG;
const SPLIT_PARTS: &str = "http://w3id.org/tripletrail/fn#splitParts";
const SPLIT_PART: &str = "http://w3id.org/tripletrail/fn#splitPart";
/// Separator the split pattern is replaced with before the parts are counted and taken.
const SEPARATOR: &str = "\u{1}";

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    ListMember,
    ListIndex,
    ListLength,
    Member,
    Container(&'static str),
    StrSplit,
    SplitIri,
    Concat,
    Assign,
    /// jena-geosparql's `spatial:` search functions (#827).
    Spatial(Spatial),
}

/// The `spatial:` property functions of jena-geosparql, with whether they take a geometry
/// (the `…Geom` forms) instead of latitude and longitude.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Spatial {
    Nearby { geom: bool },
    WithinBox { geom: bool },
    IntersectBox { geom: bool },
    Cardinal { dir: Dir, geom: bool },
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Dir {
    North,
    South,
    East,
    West,
}

fn kind(iri: &str) -> Option<Kind> {
    if let Some(n) = iri.strip_prefix(LIST) {
        return match n {
            "member" => Some(Kind::ListMember),
            "index" => Some(Kind::ListIndex),
            "length" => Some(Kind::ListLength),
            _ => None,
        };
    }
    if let Some(n) = iri.strip_prefix(SPATIAL) {
        let (n, geom) = match n.strip_suffix("Geom") {
            Some(n) => (n, true),
            None => (n, false),
        };
        let dir = |dir| Some(Kind::Spatial(Spatial::Cardinal { dir, geom }));
        return match n {
            "nearby" | "withinCircle" => Some(Kind::Spatial(Spatial::Nearby { geom })),
            "withinBox" => Some(Kind::Spatial(Spatial::WithinBox { geom })),
            "intersectBox" => Some(Kind::Spatial(Spatial::IntersectBox { geom })),
            "north" => dir(Dir::North),
            "south" => dir(Dir::South),
            "east" => dir(Dir::East),
            "west" => dir(Dir::West),
            _ => None,
        };
    }
    if let Some(n) = iri.strip_prefix(APF) {
        return match n {
            "strSplit" => Some(Kind::StrSplit),
            "splitIRI" | "splitURI" => Some(Kind::SplitIri),
            "concat" => Some(Kind::Concat),
            "assign" => Some(Kind::Assign),
            "bag" => Some(Kind::Container("Bag")),
            "seq" => Some(Kind::Container("Seq")),
            "alt" => Some(Kind::Container("Alt")),
            _ => None,
        };
    }
    match iri.strip_prefix(MG)? {
        "listMember" => Some(Kind::ListMember),
        "listIndex" => Some(Kind::ListIndex),
        "listLength" => Some(Kind::ListLength),
        "strSplit" => Some(Kind::StrSplit),
        "splitIRI" => Some(Kind::SplitIri),
        "member" => Some(Kind::Member),
        _ => None,
    }
}

/// The property function IRIs (for the service description).
pub fn property_functions() -> Vec<oxigraph::model::NamedNode> {
    let mut out: Vec<String> = ["member", "index", "length"]
        .iter()
        .map(|n| format!("{LIST}{n}"))
        .collect();
    out.extend(
        [
            "strSplit", "splitIRI", "splitURI", "concat", "assign", "bag", "seq", "alt",
        ]
        .iter()
        .map(|n| format!("{APF}{n}")),
    );
    out.extend(
        [
            "listMember",
            "listIndex",
            "listLength",
            "strSplit",
            "splitIRI",
            "member",
        ]
        .iter()
        .map(|n| format!("{MG}{n}")),
    );
    for n in [
        "nearby",
        "withinCircle",
        "withinBox",
        "intersectBox",
        "north",
        "south",
        "east",
        "west",
    ] {
        out.push(format!("{SPATIAL}{n}"));
        out.push(format!("{SPATIAL}{n}Geom"));
    }
    out.into_iter()
        .map(oxigraph::model::NamedNode::new_unchecked)
        .collect()
}

/// One call: the subject and object, each a single term or an argument list.
#[derive(Clone, Debug)]
struct Call {
    kind: Kind,
    subject: Arg,
    object: Arg,
}

#[derive(Clone, Debug)]
enum Arg {
    One(TermPattern),
    List(Vec<TermPattern>),
}

impl Arg {
    fn one(&self) -> Option<&TermPattern> {
        match self {
            Arg::One(t) => Some(t),
            Arg::List(_) => None,
        }
    }

    fn list(&self) -> Option<&[TermPattern]> {
        match self {
            Arg::List(l) => Some(l),
            Arg::One(_) => None,
        }
    }
}

fn rdf(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{RDF}{name}"))
}

/// A collection triple's index and object.
type Slot = Option<(usize, TermPattern)>;

/// Splits the property function triples (and their argument lists) off a basic graph pattern.
fn extract(patterns: Vec<TriplePattern>) -> (Vec<TriplePattern>, Vec<Call>) {
    let calls: Vec<usize> = patterns
        .iter()
        .enumerate()
        .filter(|(_, t)| matches!(&t.predicate, NamedNodePattern::NamedNode(p) if kind(p.as_str()).is_some()))
        .map(|(i, _)| i)
        .collect();
    if calls.is_empty() {
        return (patterns, Vec::new());
    }
    // The collection triples of blank nodes: node -> (first, rest), with their indexes.
    let first = rdf("first");
    let rest = rdf("rest");
    let mut cells: HashMap<String, (Slot, Slot)> = HashMap::new();
    for (i, t) in patterns.iter().enumerate() {
        let (TermPattern::BlankNode(b), NamedNodePattern::NamedNode(p)) =
            (&t.subject, &t.predicate)
        else {
            continue;
        };
        let cell = cells.entry(b.as_str().to_owned()).or_default();
        if *p == first {
            cell.0 = Some((i, t.object.clone()));
        } else if *p == rest {
            cell.1 = Some((i, t.object.clone()));
        }
    }
    let mut used: HashSet<usize> = HashSet::new();
    // The items of the list starting at `head`, and the triples that make it.
    let read_list = |head: &TermPattern, used: &mut HashSet<usize>| -> Option<Vec<TermPattern>> {
        let mut items = Vec::new();
        let mut node = head.clone();
        let mut triples = Vec::new();
        loop {
            match &node {
                TermPattern::NamedNode(n) if *n == rdf("nil") => break,
                TermPattern::BlankNode(b) => {
                    let (Some((fi, f)), Some((ri, r))) = cells.get(b.as_str())? else {
                        return None;
                    };
                    if triples.contains(fi) {
                        return None;
                    }
                    items.push(f.clone());
                    triples.extend([*fi, *ri]);
                    node = r.clone();
                }
                _ => return None,
            }
        }
        used.extend(triples);
        Some(items)
    };
    let mut out_calls = Vec::new();
    for &i in &calls {
        let t = &patterns[i];
        let NamedNodePattern::NamedNode(p) = &t.predicate else {
            continue;
        };
        let kind = kind(p.as_str()).unwrap();
        // Only the object of these takes an argument list.
        let takes_list = matches!(
            kind,
            Kind::ListIndex | Kind::StrSplit | Kind::SplitIri | Kind::Concat | Kind::Spatial(_)
        );
        let object = match &t.object {
            o @ (TermPattern::BlankNode(_) | TermPattern::NamedNode(_)) if takes_list => {
                match read_list(o, &mut used) {
                    Some(items) => Arg::List(items),
                    None => Arg::One(o.clone()),
                }
            }
            o => Arg::One(o.clone()),
        };
        let subject = Arg::One(t.subject.clone());
        used.insert(i);
        out_calls.push(Call {
            kind,
            subject,
            object,
        });
    }
    let rest = patterns
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !used.contains(i))
        .map(|(_, t)| t)
        .collect();
    (rest, out_calls)
}

static FRESH: AtomicU64 = AtomicU64::new(0);

fn fresh(name: &str) -> Variable {
    let n = FRESH.fetch_add(1, Ordering::Relaxed);
    Variable::new_unchecked(format!("__pf{n}_{name}"))
}

/// A term pattern as an expression; a blank node acts as a variable.
fn expr(t: &TermPattern) -> Expression {
    match t {
        TermPattern::Variable(v) => Expression::Variable(v.clone()),
        TermPattern::NamedNode(n) => Expression::NamedNode(n.clone()),
        TermPattern::Literal(l) => Expression::Literal(l.clone()),
        TermPattern::BlankNode(b) => Expression::Variable(blank_variable(b.as_str())),
        #[allow(unreachable_patterns)]
        _ => Expression::Variable(fresh("unsupported")),
    }
}

fn blank_variable(id: &str) -> Variable {
    Variable::new_unchecked(format!("__pfb_{id}"))
}

/// Blank nodes in pattern positions become variables (they act as such).
fn term(t: &TermPattern) -> TermPattern {
    match t {
        TermPattern::BlankNode(b) => TermPattern::Variable(blank_variable(b.as_str())),
        other => other.clone(),
    }
}

fn variables_of(args: &[&Arg]) -> Vec<Variable> {
    let mut out = Vec::new();
    for a in args {
        let items: Vec<&TermPattern> = match a {
            Arg::One(t) => vec![t],
            Arg::List(l) => l.iter().collect(),
        };
        for t in items {
            if let TermPattern::Variable(v) = term(t)
                && !out.contains(&v)
            {
                out.push(v);
            }
        }
    }
    out
}

fn bgp(patterns: Vec<TriplePattern>) -> GraphPattern {
    GraphPattern::Bgp { patterns }
}

fn triple(s: TermPattern, p: NamedNode, o: TermPattern) -> TriplePattern {
    TriplePattern {
        subject: s,
        predicate: NamedNodePattern::NamedNode(p),
        object: o,
    }
}

fn path(s: TermPattern, path: PropertyPathExpression, o: TermPattern) -> GraphPattern {
    GraphPattern::Path {
        subject: s,
        path,
        object: o,
    }
}

fn join(l: GraphPattern, r: GraphPattern) -> GraphPattern {
    GraphPattern::Join {
        left: Box::new(l),
        right: Box::new(r),
    }
}

fn lateral(l: GraphPattern, r: GraphPattern) -> GraphPattern {
    GraphPattern::Lateral {
        left: Box::new(l),
        right: Box::new(r),
    }
}

fn filter(expr: Expression, inner: GraphPattern) -> GraphPattern {
    GraphPattern::Filter {
        expr,
        inner: Box::new(inner),
    }
}

fn extend(inner: GraphPattern, variable: Variable, expression: Expression) -> GraphPattern {
    GraphPattern::Extend {
        inner: Box::new(inner),
        variable,
        expression,
    }
}

fn call(f: Function, args: Vec<Expression>) -> Expression {
    Expression::FunctionCall(f, args)
}

fn custom(iri: &str, args: Vec<Expression>) -> Expression {
    call(Function::Custom(NamedNode::new_unchecked(iri)), args)
}

fn integer(n: i64) -> Expression {
    Expression::Literal(Literal::new_typed_literal(
        n.to_string(),
        NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#integer"),
    ))
}

fn values(v: &Variable, n: i64) -> GraphPattern {
    GraphPattern::Values {
        variables: vec![v.clone()],
        bindings: (0..n)
            .map(|i| {
                vec![Some(GroundTerm::Literal(Literal::new_typed_literal(
                    i.to_string(),
                    NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#integer"),
                )))]
            })
            .collect(),
    }
}

/// `inner` with `target` bound to `value`: a variable gets it (or must already hold it), a
/// constant must equal it.
fn assign(inner: GraphPattern, target: &TermPattern, value: Expression) -> GraphPattern {
    let tmp = fresh("value");
    let inner = extend(inner, tmp.clone(), value);
    let value = Expression::Variable(tmp);
    match term(target) {
        TermPattern::Variable(v) => {
            let ok = Expression::Or(
                Box::new(Expression::Not(Box::new(Expression::Bound(v.clone())))),
                Box::new(Expression::SameTerm(
                    Box::new(Expression::Variable(v.clone())),
                    Box::new(value.clone()),
                )),
            );
            extend(filter(ok, inner), v, value)
        }
        constant => filter(
            Expression::SameTerm(Box::new(expr(&constant)), Box::new(value)),
            inner,
        ),
    }
}

fn container(s: &TermPattern, o: &TermPattern) -> GraphPattern {
    let p = fresh("p");
    filter(
        call(
            Function::Regex,
            vec![
                call(Function::Str, vec![Expression::Variable(p.clone())]),
                Expression::Literal(Literal::new_simple_literal(format!(
                    "^{}_[1-9][0-9]*$",
                    regex_escape(RDF)
                ))),
            ],
        ),
        GraphPattern::Bgp {
            patterns: vec![TriplePattern {
                subject: term(s),
                predicate: NamedNodePattern::Variable(p),
                object: term(o),
            }],
        },
    )
}

fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if ".^$*+?()[]{}|\\/".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The rows of one call, given the bindings of what comes before it.
fn step(c: &Call) -> Option<GraphPattern> {
    let rest = || PropertyPathExpression::NamedNode(rdf("rest"));
    let first = || PropertyPathExpression::NamedNode(rdf("first"));
    let unit = || bgp(Vec::new());
    Some(match c.kind {
        Kind::ListMember => {
            let s = term(c.subject.one()?);
            let o = term(c.object.one()?);
            path(
                s,
                PropertyPathExpression::Sequence(
                    Box::new(PropertyPathExpression::ZeroOrMore(Box::new(rest()))),
                    Box::new(first()),
                ),
                o,
            )
        }
        Kind::ListIndex => {
            let s = term(c.subject.one()?);
            let [index, member] = c.object.list()? else {
                return None;
            };
            let node = fresh("node");
            let node_key = node.clone();
            let mid = fresh("mid");
            let count = fresh("index");
            let members = join(
                path(
                    s.clone(),
                    PropertyPathExpression::ZeroOrMore(Box::new(rest())),
                    TermPattern::Variable(node.clone()),
                ),
                bgp(vec![triple(
                    TermPattern::Variable(node.clone()),
                    rdf("first"),
                    term(member),
                )]),
            );
            // The index of a node: the nodes from the head that come before it.
            let before = GraphPattern::Group {
                inner: Box::new(join(
                    path(
                        s,
                        PropertyPathExpression::ZeroOrMore(Box::new(rest())),
                        TermPattern::Variable(mid.clone()),
                    ),
                    path(
                        TermPattern::Variable(mid.clone()),
                        PropertyPathExpression::OneOrMore(Box::new(rest())),
                        TermPattern::Variable(node.clone()),
                    ),
                )),
                variables: Vec::new(),
                aggregates: vec![(
                    count.clone(),
                    AggregateExpression::FunctionCall {
                        name: AggregateFunction::Count,
                        expr: Expression::Variable(mid),
                        distinct: true,
                    },
                )],
            };
            // A group's rows hold only its keys and aggregates: the projection brings back the
            // bindings of the row it was evaluated for.
            let mut keep = variables_of(&[&c.subject]);
            keep.extend([node_key.clone(), count.clone()]);
            let before = GraphPattern::Project {
                inner: Box::new(before),
                variables: keep,
            };
            assign(lateral(members, before), index, Expression::Variable(count))
        }
        Kind::ListLength => {
            let s = term(c.subject.one()?);
            let node = fresh("node");
            let x = fresh("x");
            let counted = fresh("counted");
            let length = fresh("length");
            let f = fresh("f");
            // A list: a node with rdf:first, or rdf:nil.
            let nil = match &s {
                TermPattern::Variable(v) => GraphPattern::Values {
                    variables: vec![v.clone()],
                    bindings: vec![vec![Some(GroundTerm::NamedNode(rdf("nil")))]],
                },
                constant => filter(
                    Expression::SameTerm(
                        Box::new(expr(constant)),
                        Box::new(Expression::NamedNode(rdf("nil"))),
                    ),
                    unit(),
                ),
            };
            let heads = GraphPattern::Distinct {
                inner: Box::new(GraphPattern::Project {
                    inner: Box::new(GraphPattern::Union {
                        left: Box::new(bgp(vec![triple(
                            s.clone(),
                            rdf("first"),
                            TermPattern::Variable(f),
                        )])),
                        right: Box::new(nil),
                    }),
                    variables: variables_of(&[&c.subject]),
                }),
            };
            let nodes = GraphPattern::LeftJoin {
                left: Box::new(path(
                    s,
                    PropertyPathExpression::ZeroOrMore(Box::new(rest())),
                    TermPattern::Variable(node.clone()),
                )),
                right: Box::new(extend(
                    bgp(vec![triple(
                        TermPattern::Variable(node.clone()),
                        rdf("first"),
                        TermPattern::Variable(x),
                    )]),
                    counted.clone(),
                    Expression::Variable(node),
                )),
                expression: None,
            };
            let grouped = GraphPattern::Group {
                inner: Box::new(join(heads, nodes)),
                variables: variables_of(&[&c.subject]),
                aggregates: vec![(
                    length.clone(),
                    AggregateExpression::FunctionCall {
                        name: AggregateFunction::Count,
                        expr: Expression::Variable(counted),
                        distinct: true,
                    },
                )],
            };
            let mut keep = variables_of(&[&c.subject]);
            keep.push(length.clone());
            let grouped = GraphPattern::Project {
                inner: Box::new(grouped),
                variables: keep,
            };
            assign(grouped, c.object.one()?, Expression::Variable(length))
        }
        Kind::Member => {
            let (s, o) = (c.subject.one()?, c.object.one()?);
            // The asserted triples, as a path so that a second rewrite leaves them alone.
            GraphPattern::Union {
                left: Box::new(path(
                    term(s),
                    PropertyPathExpression::NamedNode(NamedNode::new_unchecked(RDFS_MEMBER)),
                    term(o),
                )),
                right: Box::new(container(s, o)),
            }
        }
        Kind::Container(class) => {
            let (s, o) = (c.subject.one()?, c.object.one()?);
            join(
                bgp(vec![triple(
                    term(s),
                    rdf("type"),
                    TermPattern::NamedNode(rdf(class)),
                )]),
                container(s, o),
            )
        }
        Kind::StrSplit => {
            let [string, pattern] = c.object.list()? else {
                return None;
            };
            let replaced = fresh("replaced");
            let parts = fresh("parts");
            let (hi, mid, lo) = (fresh("hi"), fresh("mid"), fresh("lo"));
            let v = |x: &Variable| Box::new(Expression::Variable(x.clone()));
            let mul = |x: &Variable, n: i64| Expression::Multiply(v(x), Box::new(integer(n)));
            let below = |e: Expression| Expression::Less(Box::new(e), v(&parts));
            let start = extend(
                extend(
                    unit(),
                    replaced.clone(),
                    call(
                        Function::Replace,
                        vec![
                            call(Function::Str, vec![expr(string)]),
                            expr(pattern),
                            Expression::Literal(Literal::new_simple_literal(SEPARATOR)),
                        ],
                    ),
                ),
                parts.clone(),
                custom(SPLIT_PARTS, vec![Expression::Variable(replaced.clone())]),
            );
            let hi_index = mul(&hi, 1024);
            let mid_index = Expression::Add(Box::new(hi_index.clone()), Box::new(mul(&mid, 32)));
            let index = Expression::Add(Box::new(mid_index.clone()), v(&lo));
            // Most strings have few parts: one VALUES block of 16, else the three levels.
            let i = fresh("i");
            let small = filter(
                Expression::And(
                    Box::new(Expression::LessOrEqual(v(&parts), Box::new(integer(16)))),
                    Box::new(below(Expression::Variable(i.clone()))),
                ),
                values(&i, 16),
            );
            let large = extend(
                lateral(
                    lateral(
                        lateral(
                            filter(
                                Expression::Greater(v(&parts), Box::new(integer(16))),
                                unit(),
                            ),
                            filter(below(hi_index), values(&hi, 32)),
                        ),
                        filter(below(mid_index), values(&mid, 32)),
                    ),
                    filter(below(index.clone()), values(&lo, 32)),
                ),
                i.clone(),
                index,
            );
            let rows = lateral(
                start,
                GraphPattern::Union {
                    left: Box::new(small),
                    right: Box::new(large),
                },
            );
            assign(
                rows,
                c.subject.one()?,
                custom(
                    SPLIT_PART,
                    vec![Expression::Variable(replaced), Expression::Variable(i)],
                ),
            )
        }
        Kind::SplitIri => {
            let s = c.subject.one()?;
            let [ns, local] = c.object.list()? else {
                return None;
            };
            let iri = |name: &str| format!("{}{name}", crate::xfn::AFN);
            assign(
                assign(unit(), ns, custom(&iri("namespace"), vec![expr(s)])),
                local,
                custom(&iri("localname"), vec![expr(s)]),
            )
        }
        Kind::Concat => {
            let parts = c.object.list()?;
            assign(
                unit(),
                c.subject.one()?,
                call(
                    Function::Concat,
                    parts
                        .iter()
                        .map(|p| call(Function::Str, vec![expr(p)]))
                        .collect(),
                ),
            )
        }
        Kind::Assign => {
            let (s, o) = (c.subject.one()?, c.object.one()?);
            let value = Expression::Coalesce(vec![expr(o), expr(s)]);
            assign(assign(unit(), s, value.clone()), o, value)
        }
        Kind::Spatial(f) => spatial(f, c)?,
    })
}

/// A number given as a literal, for the arguments known when the query is parsed.
fn number(t: &TermPattern) -> Option<f64> {
    match t {
        TermPattern::Literal(l) => l.value().trim().parse().ok(),
        _ => None,
    }
}

fn double(x: f64) -> Expression {
    Expression::Literal(Literal::new_typed_literal(
        format!("{x:?}"),
        NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#double"),
    ))
}

fn wkt(text: String) -> Expression {
    Expression::Literal(Literal::new_typed_literal(
        text,
        NamedNode::new_unchecked(format!("{GEO}wktLiteral")),
    ))
}

/// The point at a latitude and longitude, as `spatialf:convertLatLon` makes it.
fn lat_lon(lat: &TermPattern, lon: &TermPattern) -> Expression {
    match (number(lat), number(lon)) {
        (Some(lat), Some(lon)) => wkt(format!("POINT({lon:?} {lat:?})")),
        _ => custom(
            &format!("{SPATIALF}convertLatLon"),
            vec![expr(lat), expr(lon)],
        ),
    }
}

/// The box between two corners, as `spatialf:convertLatLonBox` makes it.
fn lat_lon_box(corners: &[TermPattern]) -> Expression {
    match corners
        .iter()
        .map(number)
        .collect::<Option<Vec<f64>>>()
        .as_deref()
    {
        Some(&[lat0, lon0, lat1, lon1]) => wkt(format!(
            "POLYGON(({lon0:?} {lat0:?}, {lon1:?} {lat0:?}, {lon1:?} {lat1:?}, {lon0:?} {lat1:?}, {lon0:?} {lat0:?}))"
        )),
        _ => custom(
            &format!("{SPATIALF}convertLatLonBox"),
            corners.iter().map(expr).collect(),
        ),
    }
}

/// A call to the geometry function `iri`, computed now when its arguments are constants (so
/// that the spatial index sees a constant geometry).
fn fold(iri: &str, args: Vec<Expression>) -> Expression {
    let constant: Option<Vec<oxigraph::model::Term>> = args
        .iter()
        .map(|a| match a {
            Expression::Literal(l) => Some(l.clone().into()),
            Expression::NamedNode(n) => Some(n.clone().into()),
            _ => None,
        })
        .collect();
    if let (Some(values), Some(f)) = (constant, tripletrail_geo::function(iri))
        && let Some(oxigraph::model::Term::Literal(l)) = f(&values)
    {
        return Expression::Literal(l);
    }
    custom(iri, args)
}

/// The bounding box (min x, min y, max x, max y) in CRS84 of a constant geometry.
fn constant_bounds(g: &Expression) -> Option<[f64; 4]> {
    let crs84 = Expression::NamedNode(NamedNode::new_unchecked(CRS84));
    let g = fold(&format!("{GEOF}transform"), vec![g.clone(), crs84]);
    let mut out = [0.0; 4];
    for (i, name) in ["minX", "minY", "maxX", "maxY"].iter().enumerate() {
        let Expression::Literal(l) = fold(&format!("{GEOF}{name}"), vec![g.clone()]) else {
            return None;
        };
        out[i] = l.value().parse().ok()?;
    }
    Some(out)
}

/// The boxes, in CRS84, of the part of the globe beyond the edge of `b` in a cardinal
/// direction, as Jena searches it: the half of the globe east or west of it, wrapped at 180°.
fn cardinal_boxes(dir: Dir, [x0, y0, x1, y1]: [f64; 4]) -> Vec<[f64; 4]> {
    match dir {
        Dir::North => vec![[-180.0, y1, 180.0, 90.0]],
        Dir::South => vec![[-180.0, -90.0, 180.0, y0]],
        Dir::East => {
            let mut out = vec![[x1, -90.0, (x1 + 180.0).min(180.0), 90.0]];
            if x1 > 0.0 {
                out.push([-180.0, -90.0, x1 - 180.0, 90.0]);
            }
            out
        }
        Dir::West => {
            let mut out = vec![[(x0 - 180.0).max(-180.0), -90.0, x0, 90.0]];
            if x0 < 0.0 {
                out.push([x0 + 180.0, -90.0, 180.0, 90.0]);
            }
            out
        }
    }
}

/// The boxes, in CRS84, that hold every point within `metres` (on Jena's sphere) of a point
/// of the box `b`, split at 180°; `None` when that is most of the globe. Jena's distance, the
/// haversine distance between two points of the geometries, is never below the shortest one,
/// so a geometry nearer than `metres` meets these boxes.
fn near_boxes([x0, y0, x1, y1]: [f64; 4], metres: f64) -> Option<Vec<[f64; 4]>> {
    // Rounding: a little more than the radius.
    let arc = metres * (1.0 + 1e-9) / tripletrail_geo::JENA_EARTH_RADIUS + 1e-12;
    let dlat = arc.to_degrees();
    if dlat >= 45.0 {
        return None;
    }
    let (lat0, lat1) = (y0 - dlat, y1 + dlat);
    if lat0 <= -90.0 || lat1 >= 90.0 {
        return Some(vec![[-180.0, lat0.max(-90.0), 180.0, lat1.min(90.0)]]);
    }
    // At latitude φ, the points within an arc δ are within asin(sin δ / cos φ) of longitude;
    // the widest is at the highest latitude of the box.
    let phi = y0.abs().max(y1.abs()).to_radians();
    let ratio = arc.sin() / phi.cos();
    if ratio >= 1.0 {
        return Some(vec![[-180.0, lat0, 180.0, lat1]]);
    }
    let dlon = ratio.asin().to_degrees();
    let (lon0, lon1) = (x0 - dlon, x1 + dlon);
    if lon1 - lon0 >= 360.0 {
        return Some(vec![[-180.0, lat0, 180.0, lat1]]);
    }
    let mut out = vec![[lon0.max(-180.0), lat0, lon1.min(180.0), lat1]];
    if lon0 < -180.0 {
        out.push([lon0 + 360.0, lat0, 180.0, lat1]);
    }
    if lon1 > 180.0 {
        out.push([-180.0, lat0, lon1 - 360.0, lat1]);
    }
    Some(out)
}

/// `geof:minX` and the like of a geometry in CRS84, the order Jena's index keeps.
fn bound(name: &str, g: Expression) -> Expression {
    let crs84 = Expression::NamedNode(NamedNode::new_unchecked(CRS84));
    custom(
        &format!("{GEOF}{name}"),
        vec![custom(&format!("{GEOF}transform"), vec![g, crs84])],
    )
}

/// A jena-geosparql `spatial:` search (#827): the features whose geometry passes its test,
/// each once. A feature's geometries are those of `geo:hasGeometry` (their
/// `geo:hasSerialization`, `geo:asWKT` or `geo:asGML`), or, for a feature without one, the
/// point of its `wgs84_pos:lat` and `wgs84_pos:long`, as Jena's spatial index reads them. The
/// tests are on the geometry itself, written with the GeoSPARQL functions the spatial index
/// answers: `geof:distance` below the radius, `geof:sfWithin` or `geof:sfIntersects` the box
/// or geometry, and for the cardinal directions the bounding box beyond the given one's edge,
/// as Jena tests them. A limit takes that many features.
fn spatial(f: Spatial, c: &Call) -> Option<GraphPattern> {
    let args: Vec<TermPattern> = match &c.object {
        Arg::List(l) => l.clone(),
        Arg::One(t) => vec![t.clone()],
    };
    let geom = match f {
        Spatial::Nearby { geom }
        | Spatial::WithinBox { geom }
        | Spatial::IntersectBox { geom }
        | Spatial::Cardinal { geom, .. } => geom,
    };
    // The geometry searched from, and the arguments after it.
    let (target, rest): (Expression, &[TermPattern]) = match (f, geom) {
        // The box of a geometry: its bounding box, as Jena searches it.
        (Spatial::WithinBox { .. } | Spatial::IntersectBox { .. }, true) => (
            fold(&format!("{GEOF}envelope"), vec![expr(args.first()?)]),
            &args[1..],
        ),
        (_, true) => (expr(args.first()?), &args[1..]),
        (Spatial::WithinBox { .. } | Spatial::IntersectBox { .. }, false) => {
            (lat_lon_box(args.get(..4)?), &args[4..])
        }
        (_, false) => (lat_lon(args.first()?, args.get(1)?), &args[2..]),
    };
    // For withinBox with a variable subject: whether the step searches for features (true
    // when the subject is not bound before the call), set first in the step.
    let searched = (matches!(f, Spatial::WithinBox { .. })
        && matches!(term(c.subject.one()?), TermPattern::Variable(_)))
    .then(|| fresh("searched"));
    let (test, limit): (Box<dyn Fn(Expression) -> Expression>, Option<&TermPattern>) = match f {
        Spatial::Nearby { .. } => {
            let radius = rest.first()?;
            let units = rest.get(1);
            (rest.len() <= 3).then_some(())?;
            let unit_text = match units {
                None => Some(format!("{UOM}kilometre")),
                Some(TermPattern::NamedNode(n)) => Some(n.as_str().to_owned()),
                Some(TermPattern::Literal(l)) => Some(l.value().to_owned()),
                Some(_) => None,
            };
            let metres = unit_text
                .as_deref()
                .and_then(tripletrail_geo::metres_in_unit);
            let target = target.clone();
            let test: Box<dyn Fn(Expression) -> Expression> = match (number(radius), metres) {
                // Jena's distance, after a test the spatial index answers: the geometry meets
                // the boxes that hold every point within the radius of the target.
                (Some(r), Some(m)) => Box::new(move |w| {
                    let metre =
                        || Expression::NamedNode(NamedNode::new_unchecked(format!("{UOM}metre")));
                    let near = constant_bounds(&target).and_then(|b| {
                        let name = format!("{GEOF}sfIntersects");
                        near_boxes(b, r * m)?
                            .into_iter()
                            .map(|[a, b, c, d]| {
                                custom(
                                    &name,
                                    vec![
                                        w.clone(),
                                        wkt(format!(
                                            "POLYGON(({a:?} {b:?}, {c:?} {b:?}, {c:?} {d:?}, {a:?} {d:?}, {a:?} {b:?}))"
                                        )),
                                    ],
                                )
                            })
                            .reduce(|a, b| Expression::Or(Box::new(a), Box::new(b)))
                    });
                    let jena = Expression::Less(
                        Box::new(custom(
                            &format!("{SPATIALF}distance"),
                            vec![w, target.clone(), metre()],
                        )),
                        Box::new(double(r * m)),
                    );
                    match near {
                        Some(near) => Expression::And(Box::new(near), Box::new(jena)),
                        None => jena,
                    }
                }),
                _ => {
                    let radius = expr(radius);
                    let units = units.map_or_else(
                        || {
                            Expression::NamedNode(NamedNode::new_unchecked(format!(
                                "{UOM}kilometre"
                            )))
                        },
                        expr,
                    );
                    Box::new(move |w| {
                        Expression::Less(
                            Box::new(custom(
                                &format!("{SPATIALF}distance"),
                                vec![w, target.clone(), units.clone()],
                            )),
                            Box::new(radius.clone()),
                        )
                    })
                }
            };
            (test, rest.get(2))
        }
        // Jena's test when it searches for features (an unbound subject): their bounding box
        // meets the box. A bound subject is within it.
        Spatial::WithinBox { .. } => {
            (rest.len() <= 1).then_some(())?;
            let target = target.clone();
            let within = format!("{GEOF}sfWithin");
            let test: Box<dyn Fn(Expression) -> Expression> = match &searched {
                None => Box::new(move |w| custom(&within, vec![w, target.clone()])),
                Some(searched) => {
                    let searched = searched.clone();
                    let meets = format!("{}ST_EnvelopesIntersect", tripletrail_geo::ST);
                    Box::new(move |w| {
                        Expression::And(
                            Box::new(custom(&meets, vec![w.clone(), target.clone()])),
                            Box::new(Expression::Or(
                                Box::new(Expression::Variable(searched.clone())),
                                Box::new(custom(&within, vec![w, target.clone()])),
                            )),
                        )
                    })
                }
            };
            (test, rest.first())
        }
        Spatial::IntersectBox { .. } => {
            (rest.len() <= 1).then_some(())?;
            let target = target.clone();
            let name = format!("{GEOF}sfIntersects");
            (
                Box::new(move |w| custom(&name, vec![w, target.clone()])),
                rest.first(),
            )
        }
        // A constant: the geometries that meet the boxes beyond its edge, which is the same as
        // their bounding boxes meeting them, and which the spatial index answers.
        Spatial::Cardinal { dir, .. } if constant_bounds(&target).is_some() => {
            (rest.len() <= 1).then_some(())?;
            let boxes: Vec<Expression> = cardinal_boxes(dir, constant_bounds(&target)?)
                .into_iter()
                .map(|[a, b, c, d]| {
                    wkt(format!(
                        "POLYGON(({a:?} {b:?}, {c:?} {b:?}, {c:?} {d:?}, {a:?} {d:?}, {a:?} {b:?}))"
                    ))
                })
                .collect();
            let name = format!("{GEOF}sfIntersects");
            let test = move |w: Expression| {
                boxes
                    .iter()
                    .map(|b| custom(&name, vec![w.clone(), b.clone()]))
                    .reduce(|a, b| Expression::Or(Box::new(a), Box::new(b)))
                    .unwrap()
            };
            (Box::new(test), rest.first())
        }
        Spatial::Cardinal { dir, .. } => {
            (rest.len() <= 1).then_some(())?;
            let target = target.clone();
            let test = move |w: Expression| {
                let of = |name: &str, g: &Expression| bound(name, g.clone());
                let ge = |a, b| Expression::GreaterOrEqual(Box::new(a), Box::new(b));
                let le = |a, b| Expression::LessOrEqual(Box::new(a), Box::new(b));
                let plus = |a, x: f64| Expression::Add(Box::new(a), Box::new(double(x)));
                let or = |a, b| Expression::Or(Box::new(a), Box::new(b));
                let and = |a, b| Expression::And(Box::new(a), Box::new(b));
                match dir {
                    Dir::North => ge(of("maxY", &w), of("maxY", &target)),
                    Dir::South => le(of("minY", &w), of("minY", &target)),
                    // Half the globe to the east of the edge, wrapped at 180°.
                    Dir::East => {
                        let x = of("maxX", &target);
                        or(
                            ge(of("maxX", &w), x.clone()),
                            and(
                                Expression::Greater(Box::new(x.clone()), Box::new(double(0.0))),
                                le(of("minX", &w), plus(x, -180.0)),
                            ),
                        )
                    }
                    Dir::West => {
                        let x = of("minX", &target);
                        or(
                            le(of("minX", &w), x.clone()),
                            and(
                                Expression::Less(Box::new(x.clone()), Box::new(double(0.0))),
                                ge(of("maxX", &w), plus(x, 180.0)),
                            ),
                        )
                    }
                }
            };
            (Box::new(test), rest.first())
        }
    };
    let s = term(c.subject.one()?);
    let iri = |base: &str, n: &str| NamedNode::new_unchecked(format!("{base}{n}"));
    let has_geometry = iri(GEO, "hasGeometry");
    let mut branches = Vec::new();
    for p in ["hasSerialization", "asWKT", "asGML"] {
        let (g, w) = (fresh("geometry"), fresh("wkt"));
        branches.push(filter(
            test(Expression::Variable(w.clone())),
            bgp(vec![
                triple(
                    s.clone(),
                    has_geometry.clone(),
                    TermPattern::Variable(g.clone()),
                ),
                triple(
                    TermPattern::Variable(g),
                    iri(GEO, p),
                    TermPattern::Variable(w),
                ),
            ]),
        ));
    }
    // A feature without geometries: its latitude and longitude.
    let (lat, lon, any) = (fresh("lat"), fresh("lon"), fresh("geometry"));
    let point = custom(
        &format!("{SPATIALF}convertLatLon"),
        vec![
            Expression::Variable(lat.clone()),
            Expression::Variable(lon.clone()),
        ],
    );
    branches.push(filter(
        Expression::And(
            Box::new(Expression::Not(Box::new(Expression::Exists(Box::new(
                bgp(vec![triple(
                    s.clone(),
                    has_geometry,
                    TermPattern::Variable(any),
                )]),
            ))))),
            Box::new(test(point)),
        ),
        bgp(vec![
            triple(s.clone(), iri(WGS84, "lat"), TermPattern::Variable(lat)),
            triple(s.clone(), iri(WGS84, "long"), TermPattern::Variable(lon)),
        ]),
    ));
    let mut found = branches.pop()?;
    while let Some(b) = branches.pop() {
        found = GraphPattern::Union {
            left: Box::new(b),
            right: Box::new(found),
        };
    }
    if let (Some(searched), TermPattern::Variable(v)) = (&searched, &s) {
        let unbound = Expression::Not(Box::new(Expression::Bound(v.clone())));
        found = lateral(extend(bgp(Vec::new()), searched.clone(), unbound), found);
    }
    let found = GraphPattern::Distinct {
        inner: Box::new(GraphPattern::Project {
            inner: Box::new(found),
            variables: variables_of(&[&c.subject, &c.object]),
        }),
    };
    Some(match limit.and_then(number) {
        Some(n) if n >= 0.0 => GraphPattern::Slice {
            inner: Box::new(found),
            start: 0,
            length: Some(n as usize),
        },
        _ => found,
    })
}

/// `base` followed by the calls, each a `LATERAL` step that sees what comes before it.
fn chain(base: GraphPattern, calls: Vec<Call>, triples: &mut Vec<TriplePattern>) -> GraphPattern {
    let mut out = base;
    for c in calls {
        match step(&c) {
            Some(step) => {
                let visible = variables_of(&[&c.subject, &c.object]);
                let step = GraphPattern::Project {
                    inner: Box::new(step),
                    variables: visible,
                };
                // Nothing before it: the step alone (it still sees an enclosing row).
                out = if matches!(&out, GraphPattern::Bgp { patterns } if patterns.is_empty()) {
                    step
                } else {
                    lateral(out, step)
                };
            }
            // Not a call this module understands (wrong argument shape): the plain triple.
            None => triples.push(back_to_triple(&c)),
        }
    }
    out
}

/// A call left as a triple pattern (its argument lists are not rebuilt: such a call only
/// fails to match, as an unknown property would).
fn back_to_triple(c: &Call) -> TriplePattern {
    let pick = |a: &Arg| match a {
        Arg::One(t) => t.clone(),
        Arg::List(_) => TermPattern::Variable(fresh("list")),
    };
    triple(
        pick(&c.subject),
        NamedNode::new_unchecked(format!("{MG}unsupportedPropertyFunctionCall")),
        pick(&c.object),
    )
}

/// Whether a pattern calls a property function in its own groups (not in sub-selects).
pub(crate) fn calls_property_function(p: &GraphPattern) -> bool {
    use GraphPattern as G;
    match p {
        G::Bgp { patterns } => patterns.iter().any(
            |t| matches!(&t.predicate, NamedNodePattern::NamedNode(n) if kind(n.as_str()).is_some()),
        ),
        G::Join { left, right }
        | G::Lateral { left, right }
        | G::Union { left, right }
        | G::LeftJoin { left, right, .. } => {
            calls_property_function(left) || calls_property_function(right)
        }
        G::Filter { inner, .. } | G::Extend { inner, .. } | G::Graph { inner, .. } => {
            calls_property_function(inner)
        }
        _ => false,
    }
}

/// Rewrites the property functions of a group whose last element is `right`, after `left`.
/// `left` is `None` for a group that is a single basic graph pattern.
pub(crate) fn group(left: Option<GraphPattern>, right: GraphPattern) -> GraphPattern {
    let GraphPattern::Bgp { patterns } = right else {
        return match left {
            Some(l) => join(l, right),
            None => right,
        };
    };
    let (mut rest, calls) = extract(patterns);
    if calls.is_empty() {
        let right = bgp(rest);
        return match left {
            Some(l) => join(l, right),
            None => right,
        };
    }
    let mut extra = Vec::new();
    let base_bgp = bgp(std::mem::take(&mut rest));
    let base = match left {
        Some(l) => join(l, base_bgp),
        None => base_bgp,
    };
    let out = chain(base, calls, &mut extra);
    if extra.is_empty() {
        out
    } else {
        join(out, bgp(extra))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(q: &str) -> GraphPattern {
        let spargebra::Query::Select { pattern, .. } =
            spargebra::SparqlParser::new().parse_query(q).unwrap()
        else {
            panic!()
        };
        pattern
    }

    #[test]
    fn reads_argument_lists_and_removes_their_triples() {
        let GraphPattern::Project { inner, .. } = parse(
            r#"PREFIX apf: <http://jena.apache.org/ARQ/property#>
            SELECT ?p WHERE { ?x <http://e/s> ?s . ?p apf:strSplit (?s ";") }"#,
        ) else {
            panic!()
        };
        let GraphPattern::Bgp { patterns } = *inner else {
            panic!()
        };
        assert_eq!(patterns.len(), 6);
        let (rest, calls) = extract(patterns);
        assert_eq!(rest.len(), 1);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].kind, Kind::StrSplit);
        assert_eq!(calls[0].object.list().unwrap().len(), 2);
    }

    #[test]
    fn rdfs_member_stays_a_triple_pattern() {
        let q = "SELECT ?s ?o WHERE { ?s <http://www.w3.org/2000/01/rdf-schema#member> ?o }";
        let parsed = spargebra::SparqlParser::new().parse_query(q).unwrap();
        assert_eq!(crate::xfn::alias_query(parsed.clone()), parsed);
    }
}
