// Differential check of named graph grants (#208), shared by `tests/graph_access_diff.rs`
// and the `graph_grants` fuzz target (which `include!`s it, so no inner doc comments here).
//
// From a byte source: a small dataset (the default graph and up to four named graphs, with
// triples repeated across graphs), a grant (some of the graphs) and queries. The answer to a
// query by a caller limited to the grant must equal the answer, under the same grant, on a
// second store holding only the granted graphs: what the caller sees must not depend on the
// graphs it may not read. (The same grant on both, because a grant is the caller's dataset
// as FROM NAMED gives one: a granted graph without quads is an empty graph of it, which the
// second store could not hold; FROM NAMED of a graph not granted leaves it out.)

use oxigraph::model::{GraphName, Literal, NamedNode, Quad, Term};
use oxigraph::sparql::QueryResults;
use tripletrail::{Engine, QueryOptions, ReadableGraphs};

pub struct Bytes<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Bytes<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    /// The next byte, 0 once the input is used up.
    pub fn next(&mut self) -> u8 {
        let b = self.data.get(self.at).copied().unwrap_or(0);
        self.at += 1;
        b
    }

    pub fn below(&mut self, n: usize) -> usize {
        usize::from(self.next()) % n
    }

    pub fn exhausted(&self) -> bool {
        self.at >= self.data.len()
    }
}

const GRAPHS: usize = 4;

fn iri(prefix: &str, i: usize) -> NamedNode {
    NamedNode::new_unchecked(format!("urn:{prefix}{i}"))
}

/// The graph of index `g`: 0 is the default graph, 1.. named graphs.
fn graph(g: usize) -> GraphName {
    if g == 0 {
        GraphName::DefaultGraph
    } else {
        iri("g", g).into()
    }
}

fn object(i: usize) -> Term {
    if i < 4 {
        iri("s", i).into()
    } else {
        Literal::from((i - 4) as i64).into()
    }
}

/// A term of a pattern: a variable most of the time, else a constant.
fn node(
    b: &mut Bytes<'_>,
    vars: &[&str],
    constant: impl FnOnce(&mut Bytes<'_>) -> String,
) -> String {
    let i = b.below(vars.len() + 2);
    if i < vars.len() {
        format!("?{}", vars[i])
    } else {
        constant(b)
    }
}

fn pattern(b: &mut Bytes<'_>) -> String {
    let vars = ["a", "b", "c"];
    let s = node(b, &vars, |b| format!("<urn:s{}>", b.below(4)));
    let p = match b.below(8) {
        0 => "?p".to_owned(),
        1 => "<urn:p0>/<urn:p1>".to_owned(),
        2 => "<urn:p0>+".to_owned(),
        3 => "<urn:p1>*".to_owned(),
        4 => "^<urn:p2>".to_owned(),
        n => format!("<urn:p{}>", n % 3),
    };
    let o = node(b, &vars, |b| object(b.below(7)).to_string());
    // A path with both ends constant or a predicate variable stays a plain pattern.
    format!("{s} {p} {o} .")
}

fn group(b: &mut Bytes<'_>) -> String {
    let mut body: Vec<String> = (0..1 + b.below(3)).map(|_| pattern(b)).collect();
    match b.below(8) {
        0 => body.push(format!("OPTIONAL {{ {} }}", pattern(b))),
        1 => body.push(format!("MINUS {{ {} }}", pattern(b))),
        2 => body.push(format!("FILTER EXISTS {{ {} }}", pattern(b))),
        3 => body.push(format!("FILTER NOT EXISTS {{ {} }}", pattern(b))),
        4 => body.push(format!("{{ {} }} UNION {{ {} }}", pattern(b), pattern(b))),
        5 => body.push(format!("GRAPH ?g {{ {} }}", pattern(b))),
        _ => {}
    }
    body.join(" ")
}

/// A random query; `None` when the bytes give none.
pub fn query(b: &mut Bytes<'_>) -> String {
    let from = match b.below(6) {
        0 => format!("FROM <urn:g{}> ", 1 + b.below(GRAPHS)),
        1 => format!("FROM NAMED <urn:g{}> ", 1 + b.below(GRAPHS)),
        _ => String::new(),
    };
    let body = group(b);
    match b.below(7) {
        0 => format!("SELECT (COUNT(*) AS ?n) {from}{{ {body} }}"),
        1 => format!("SELECT DISTINCT * {from}{{ {body} }}"),
        2 => format!("ASK {from}{{ {body} }}"),
        3 => format!("CONSTRUCT {{ ?a <urn:r> ?b }} {from}{{ {body} }}"),
        4 => format!(
            "SELECT ?a (COUNT(?b) AS ?n) {from}{{ {body} }} GROUP BY ?a HAVING (COUNT(?b) > 1)"
        ),
        5 => "SELECT DISTINCT ?g { GRAPH ?g { ?s ?p ?o } }".to_owned(),
        _ => format!("SELECT * {from}{{ {body} }}"),
    }
}

/// The dataset: quads over the default graph and the named graphs.
pub fn quads(b: &mut Bytes<'_>) -> Vec<Quad> {
    let n = 4 + b.below(28);
    quads_of(b, n)
}

/// `n` random triples, some in several graphs.
fn quads_of(b: &mut Bytes<'_>, n: usize) -> Vec<Quad> {
    let mut quads: Vec<Quad> = Vec::new();
    for _ in 0..n {
        let s = iri("s", b.below(4));
        let p = iri("p", b.below(3));
        let o = object(b.below(7));
        // Often the same triple again in another graph.
        let copies = 1 + usize::from(b.next().is_multiple_of(4));
        for _ in 0..copies {
            quads.push(Quad::new(
                s.clone(),
                p.clone(),
                o.clone(),
                graph(b.below(GRAPHS + 1)),
            ));
        }
    }
    quads
}

/// A grant: whether the default graph is readable, and which named graphs (1..=GRAPHS).
pub fn grant(b: &mut Bytes<'_>) -> (bool, Vec<usize>) {
    let bits = b.next();
    let named = (1..=GRAPHS).filter(|g| bits & (1 << g) != 0).collect();
    (bits & 1 != 0, named)
}

fn answer(engine: &Engine, query: &str, options: QueryOptions) -> Result<Vec<String>, String> {
    let mut rows: Vec<String> = match engine
        .query_with_options(query, options)
        .map_err(|e| e.to_string())?
    {
        QueryResults::Boolean(b) => vec![b.to_string()],
        QueryResults::Solutions(s) => s
            .map(|row| {
                let row = row.map_err(|e| e.to_string())?;
                let mut cells: Vec<String> = row.iter().map(|(v, t)| format!("{v}={t}")).collect();
                cells.sort();
                Ok(cells.join(" "))
            })
            .collect::<Result<_, String>>()?,
        QueryResults::Graph(g) => g
            .map(|t| t.map(|t| t.to_string()).map_err(|e| e.to_string()))
            .collect::<Result<_, String>>()?,
    };
    rows.sort();
    Ok(rows)
}

/// Checks one input; panics, with what differed, when the two stores answer differently.
pub fn check(data: &[u8]) {
    let mut b = Bytes::new(data);
    let quads = quads(&mut b);
    let (default_graph, named) = grant(&mut b);
    let union = b.next() & 1 != 0;
    let limited = Engine::new_tripletrail();
    limited.load_quads(quads.clone()).unwrap();
    limited.set_union_default_graph(union);
    let granted = |g: &GraphName| match g {
        GraphName::DefaultGraph => default_graph,
        GraphName::NamedNode(n) => named.iter().any(|i| iri("g", *i) == *n),
        GraphName::BlankNode(_) => false,
    };
    let own = Engine::new_tripletrail();
    own.load_quads(quads.iter().filter(|q| granted(&q.graph_name)).cloned())
        .unwrap();
    own.set_union_default_graph(true);
    let readable = ReadableGraphs {
        default_graph,
        named: named.iter().map(|i| iri("g", *i)).collect(),
    };
    for _ in 0..4 {
        let query = query(&mut b);
        let seen = answer(
            &limited,
            &query,
            QueryOptions {
                readable: Some(readable.clone()),
                ..Default::default()
            },
        );
        let expected = answer(
            &own,
            &query,
            QueryOptions {
                readable: Some(readable.clone()),
                ..Default::default()
            },
        );
        assert_eq!(
            seen.is_ok(),
            expected.is_ok(),
            "{query}\nfull store: {seen:?}\nstore of the granted graphs: {expected:?}"
        );
        assert_eq!(
            seen.ok(),
            expected.ok(),
            "{query}\ngrant: default {default_graph}, named {named:?}, union {union}\nquads: {quads:#?}"
        );
        if b.exhausted() {
            break;
        }
    }
}

/// A graph of a write: one of the named graphs, or the default graph (empty).
fn target(b: &mut Bytes<'_>) -> String {
    match b.below(GRAPHS + 1) {
        0 => String::new(),
        g => format!("<urn:g{g}>"),
    }
}

fn triple(b: &mut Bytes<'_>) -> String {
    format!(
        "<urn:s{}> <urn:p{}> {}",
        b.below(4),
        b.below(3),
        object(b.below(7))
    )
}

/// In a template: inside `GRAPH <g> { }`, or bare (the default graph, or WITH's graph).
fn wrap(graph: &str, body: &str) -> String {
    if graph.is_empty() {
        body.to_owned()
    } else {
        format!("GRAPH {graph} {{ {body} }}")
    }
}

/// One update operation.
fn operation(b: &mut Bytes<'_>) -> String {
    let graph = target(b);
    let other = target(b);
    let name = |g: &str| {
        if g.is_empty() {
            "DEFAULT".to_owned()
        } else {
            g.to_owned()
        }
    };
    match b.below(13) {
        0 => format!("INSERT DATA {{ {} }}", wrap(&graph, &triple(b))),
        1 => format!("DELETE DATA {{ {} }}", wrap(&graph, &triple(b))),
        2 => format!(
            "INSERT {{ {} }} WHERE {{ {} }}",
            wrap(&graph, "?a <urn:r> ?b"),
            group(b)
        ),
        3 => format!(
            "DELETE {{ {} }} WHERE {{ {} }}",
            wrap(&graph, "?a ?p ?b"),
            wrap(&graph, "?a ?p ?b")
        ),
        4 => format!("DELETE WHERE {{ {} }}", wrap(&graph, &pattern(b))),
        5 if !graph.is_empty() => format!(
            "WITH {graph} DELETE {{ ?a ?p ?b }} INSERT {{ ?a <urn:r> ?b }} WHERE {{ ?a ?p ?b }}"
        ),
        6 if !other.is_empty() => format!(
            "INSERT {{ {} }} USING {other} WHERE {{ ?a ?p ?b }}",
            wrap(&graph, "?a <urn:r> ?b")
        ),
        7 => format!(
            "CLEAR {}",
            if graph.is_empty() {
                "DEFAULT".to_owned()
            } else {
                format!("GRAPH {graph}")
            }
        ),
        8 => format!(
            "DROP SILENT {}",
            if graph.is_empty() {
                "DEFAULT".to_owned()
            } else {
                format!("GRAPH {graph}")
            }
        ),
        9 => format!("ADD {} TO {}", name(&other), name(&graph)),
        10 => format!("COPY {} TO {}", name(&other), name(&graph)),
        11 => format!("MOVE {} TO {}", name(&other), name(&graph)),
        _ => ["CLEAR ALL", "CLEAR NAMED", "DROP ALL"][b.below(3)].to_owned(),
    }
}

/// Every quad of `engine`, in order, the default graph's as `DEFAULT`.
fn dump(engine: &Engine) -> Vec<String> {
    let query = "SELECT ?g ?s ?p ?o { { ?s ?p ?o } UNION { GRAPH ?g { ?s ?p ?o } } }";
    let mut quads = answer(engine, query, QueryOptions::default()).unwrap();
    quads.sort();
    quads
}

/// Updates (#208): a caller who may read some graphs and write some must change the store
/// as it changes a store holding only those graphs, refuse the same updates, and never
/// change another graph; a refused update changes nothing.
pub fn check_update(data: &[u8]) {
    let mut b = Bytes::new(data);
    let quads = quads(&mut b);
    let (read_default, read_named) = grant(&mut b);
    let (write_default, write_named) = grant(&mut b);
    let union = b.next() & 1 != 0;
    let index = |g: &GraphName| match g {
        GraphName::DefaultGraph => 0,
        GraphName::NamedNode(n) => (1..=GRAPHS).find(|i| iri("g", *i) == *n).unwrap(),
        GraphName::BlankNode(_) => unreachable!(),
    };
    let readable = ReadableGraphs {
        default_graph: read_default,
        named: read_named.iter().map(|i| iri("g", *i)).collect(),
    };
    let writable = ReadableGraphs {
        default_graph: write_default,
        named: write_named.iter().map(|i| iri("g", *i)).collect(),
    };
    let reached = |g: usize| {
        if g == 0 {
            read_default || write_default
        } else {
            read_named.contains(&g) || write_named.contains(&g)
        }
    };
    let full = Engine::new_tripletrail();
    full.load_quads(quads.clone()).unwrap();
    let own = Engine::new_tripletrail();
    own.load_quads(
        quads
            .iter()
            .filter(|q| reached(index(&q.graph_name)))
            .cloned(),
    )
    .unwrap();
    // The graphs the caller may neither read nor write, as `dump` rows give them.
    let hidden = |row: &String| {
        (0..=GRAPHS).filter(|g| !reached(*g)).any(|g| {
            if g == 0 {
                !row.contains("?g=")
            } else {
                row.contains(&format!("?g=<urn:g{g}>"))
            }
        })
    };
    let options = tripletrail::UpdateOptions {
        readable: Some(readable),
        writable: Some(writable),
        ..Default::default()
    };
    for _ in 0..3 {
        let update = (0..1 + b.below(2))
            .map(|_| operation(&mut b))
            .collect::<Vec<_>>()
            .join(" ; ");
        let before = dump(&full);
        full.set_union_default_graph(union);
        own.set_union_default_graph(union);
        let on_full = full
            .update_with_options(&update, &options)
            .map_err(|e| e.to_string());
        let on_own = own
            .update_with_options(&update, &options)
            .map_err(|e| e.to_string());
        full.set_union_default_graph(false);
        own.set_union_default_graph(false);
        let after = dump(&full);
        let context = || {
            format!(
                "{update}\nread {read_default} {read_named:?}, write {write_default} {write_named:?}, union {union}\nfull: {on_full:?}\nown: {on_own:?}\nbefore: {before:#?}"
            )
        };
        assert_eq!(on_full.is_ok(), on_own.is_ok(), "{}", context());
        if on_full.is_err() {
            assert_eq!(
                before,
                after,
                "a refused update changed the store\n{}",
                context()
            );
        }
        let kept = |rows: &[String]| {
            rows.iter()
                .filter(|r| hidden(r))
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(kept(&before), kept(&after), "{}", context());
        let seen: Vec<String> = after.iter().filter(|r| !hidden(r)).cloned().collect();
        assert_eq!(seen, dump(&own), "{}", context());
        if b.exhausted() {
            break;
        }
    }
}

/// Checks one input of row filters (#202): the answer to a query on a view hiding some
/// resources and predicates must equal the answer on a second store without the hidden
/// quads. Panics, with what differed, when they differ.
pub fn check_rows(data: &[u8]) {
    use oxigraph::model::NamedOrBlankNode;
    use std::sync::Arc;
    let mut b = Bytes::new(data);
    let quads = quads(&mut b);
    let union = b.next() & 1 != 0;
    let bits = b.next();
    let hidden: Vec<usize> = (0..4).filter(|i| bits & (1 << i) != 0).collect();
    let references = bits & 0x10 != 0;
    let bits = b.next();
    let predicates: Vec<usize> = (0..3)
        .filter(|i| bits & (1 << i) != 0 && bits & 0x80 != 0)
        .collect();
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for _ in 0..1 + b.below(2) {
        if b.next() & 1 != 0 {
            pairs.push((b.below(4), b.below(3)));
        }
    }
    let hides = |q: &Quad| {
        let subject = |i: usize| q.subject == NamedOrBlankNode::from(iri("s", i));
        hidden.iter().any(|i| subject(*i))
            || (references && hidden.iter().any(|i| q.object == Term::from(iri("s", *i))))
            || predicates.iter().any(|p| q.predicate == iri("p", *p))
            || pairs
                .iter()
                .any(|(s, p)| subject(*s) && q.predicate == iri("p", *p))
    };
    let filtered = Engine::new_tripletrail();
    filtered.load_quads(quads.clone()).unwrap();
    filtered.set_union_default_graph(union);
    let visible = Engine::new_tripletrail();
    visible
        .load_quads(quads.iter().filter(|q| !hides(q)).cloned())
        .unwrap();
    visible.set_union_default_graph(union);
    let rows = filtered
        .row_filter(
            &hidden
                .iter()
                .map(|i| iri("s", *i).into())
                .collect::<Vec<_>>(),
            references,
            &predicates.iter().map(|p| iri("p", *p)).collect::<Vec<_>>(),
            &pairs
                .iter()
                .map(|(s, p)| (iri("s", *s).into(), iri("p", *p)))
                .collect::<Vec<_>>(),
        )
        .unwrap();
    let rows = Arc::new(rows);
    for _ in 0..4 {
        let query = query(&mut b);
        let seen = answer(
            &filtered,
            &query,
            QueryOptions {
                rows: Some(Arc::clone(&rows)),
                ..Default::default()
            },
        );
        let expected = answer(&visible, &query, QueryOptions::default());
        assert_eq!(
            seen, expected,
            "{query}\nhidden {hidden:?} (references {references}), predicates {predicates:?}, pairs {pairs:?}, union {union}\nquads: {quads:#?}"
        );
        if b.exhausted() {
            break;
        }
    }
}

/// One row rule of [`check_rules`]: a target, and for `sg:visibleIf` the constraints a
/// target must meet, or for `sg:hideProperties` the properties hidden.
#[derive(Debug)]
struct Rule {
    /// `sg:hideProperties` instead of `sg:visibleIf`.
    hide: bool,
    references: bool,
    target: Option<Target>,
    /// For `sg:visibleIf`: (path, constraint); for `sg:hideProperties`: the paths alone.
    properties: Vec<(usize, Constraint)>,
}

#[derive(Debug)]
enum Target {
    Class(usize),
    Node(usize),
    SubjectsOf(usize),
    ObjectsOf(usize),
}

#[derive(Debug)]
enum Constraint {
    None,
    MinCount(usize),
    MaxCount(usize),
    HasValue(usize),
    In(usize, usize),
    MinInclusive(i64),
    Integer,
    Iri,
    Class(usize),
}

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

fn rule(b: &mut Bytes<'_>) -> Rule {
    let hide = b.below(3) == 0;
    let target = match b.below(5) {
        0 => Some(Target::Class(b.below(2))),
        1 => Some(Target::Node(b.below(4))),
        2 => Some(Target::SubjectsOf(b.below(3))),
        3 => Some(Target::ObjectsOf(b.below(3))),
        _ => None,
    };
    let properties = (0..1 + b.below(2))
        .map(|_| {
            let path = b.below(3);
            let constraint = if hide {
                Constraint::None
            } else {
                match b.below(8) {
                    0 => Constraint::MinCount(b.below(3)),
                    1 => Constraint::MaxCount(b.below(2)),
                    2 => Constraint::HasValue(b.below(7)),
                    3 => Constraint::In(b.below(7), b.below(7)),
                    4 => Constraint::MinInclusive(b.below(3) as i64),
                    5 => Constraint::Integer,
                    6 => Constraint::Iri,
                    _ => Constraint::Class(b.below(2)),
                }
            };
            (path, constraint)
        })
        .collect();
    Rule {
        hide,
        references: b.next() & 1 != 0,
        target,
        properties,
    }
}

/// The rule's shape, `<urn:shape{n}>`, in Turtle.
fn shape_turtle(n: usize, rule: &Rule) -> String {
    let mut out = format!("<urn:shape{n}> a sh:NodeShape ");
    match &rule.target {
        Some(Target::Class(c)) => out += &format!("; sh:targetClass <urn:c{c}> "),
        Some(Target::Node(s)) => out += &format!("; sh:targetNode <urn:s{s}> "),
        Some(Target::SubjectsOf(p)) => out += &format!("; sh:targetSubjectsOf <urn:p{p}> "),
        Some(Target::ObjectsOf(p)) => out += &format!("; sh:targetObjectsOf <urn:p{p}> "),
        None => {}
    }
    for (path, constraint) in &rule.properties {
        let check = match constraint {
            Constraint::None => String::new(),
            Constraint::MinCount(n) => format!("; sh:minCount {n}"),
            Constraint::MaxCount(n) => format!("; sh:maxCount {n}"),
            Constraint::HasValue(v) => format!("; sh:hasValue {}", object(*v)),
            Constraint::In(a, b) => format!("; sh:in ( {} {} )", object(*a), object(*b)),
            Constraint::MinInclusive(k) => format!("; sh:minInclusive {k}"),
            Constraint::Integer => {
                "; sh:datatype <http://www.w3.org/2001/XMLSchema#integer>".to_owned()
            }
            Constraint::Iri => "; sh:nodeKind sh:IRI".to_owned(),
            Constraint::Class(c) => format!("; sh:class <urn:c{c}>"),
        };
        out += &format!("; sh:property [ sh:path <urn:p{path}> {check} ] ");
    }
    out + ".\n"
}

/// The quads `rules` let a caller who reads every graph see, by an evaluator of its own.
fn shown_by(quads: &[Quad], rules: &[Rule]) -> Vec<Quad> {
    use std::collections::HashSet;
    let rdf_type = NamedNode::new_unchecked(RDF_TYPE);
    // Judged over every graph.
    let triples: HashSet<(Term, NamedNode, Term)> = quads
        .iter()
        .map(|q| {
            (
                q.subject.clone().into(),
                q.predicate.clone(),
                q.object.clone(),
            )
        })
        .collect();
    let values = |focus: &Term, p: &NamedNode| -> Vec<Term> {
        triples
            .iter()
            .filter(|(s, q, _)| s == focus && q == p)
            .map(|(_, _, o)| o.clone())
            .collect()
    };
    let is_a = |node: &Term, c: usize| {
        triples.contains(&(node.clone(), rdf_type.clone(), iri("c", c).into()))
    };
    let targets = |target: &Target| -> HashSet<Term> {
        match target {
            Target::Class(c) => triples
                .iter()
                .filter(|(_, p, o)| *p == rdf_type && *o == Term::from(iri("c", *c)))
                .map(|(s, _, _)| s.clone())
                .collect(),
            Target::Node(s) => [Term::from(iri("s", *s))].into(),
            Target::SubjectsOf(p) => triples
                .iter()
                .filter(|(_, q, _)| *q == iri("p", *p))
                .map(|(s, _, _)| s.clone())
                .collect(),
            Target::ObjectsOf(p) => triples
                .iter()
                .filter(|(_, q, _)| *q == iri("p", *p))
                .map(|(_, _, o)| o.clone())
                .collect(),
        }
    };
    let meets = |focus: &Term, path: usize, constraint: &Constraint| -> bool {
        let vs = values(focus, &iri("p", path));
        let integer = |v: &Term| match v {
            Term::Literal(l)
                if l.datatype().as_str() == "http://www.w3.org/2001/XMLSchema#integer" =>
            {
                l.value().parse::<i64>().ok()
            }
            _ => None,
        };
        match constraint {
            Constraint::None => true,
            Constraint::MinCount(n) => vs.len() >= *n,
            Constraint::MaxCount(n) => vs.len() <= *n,
            Constraint::HasValue(v) => vs.contains(&object(*v)),
            Constraint::In(a, c) => vs.iter().all(|v| *v == object(*a) || *v == object(*c)),
            Constraint::MinInclusive(k) => vs.iter().all(|v| integer(v).is_some_and(|i| i >= *k)),
            Constraint::Integer => vs.iter().all(|v| integer(v).is_some()),
            Constraint::Iri => vs.iter().all(|v| matches!(v, Term::NamedNode(_))),
            Constraint::Class(c) => vs.iter().all(|v| is_a(v, *c)),
        }
    };
    let mut subjects: HashSet<Term> = HashSet::new();
    let mut objects: HashSet<Term> = HashSet::new();
    let mut predicates: HashSet<NamedNode> = HashSet::new();
    let mut pairs: HashSet<(Term, NamedNode)> = HashSet::new();
    for rule in rules {
        let paths = rule.properties.iter().map(|(p, _)| iri("p", *p));
        match (&rule.target, rule.hide) {
            (None, true) => predicates.extend(paths),
            (Some(target), true) => {
                for focus in targets(target) {
                    pairs.extend(paths.clone().map(|p| (focus.clone(), p)));
                }
            }
            (target, false) => {
                // Without targets, every resource in subject position (and in object
                // position, with references). A literal target is a value, not a resource:
                // never hidden.
                let candidates: HashSet<Term> = match target {
                    Some(target) => targets(target),
                    None => triples
                        .iter()
                        .flat_map(|(s, _, o)| {
                            std::iter::once(s.clone()).chain(rule.references.then(|| o.clone()))
                        })
                        .collect(),
                };
                for focus in candidates
                    .into_iter()
                    .filter(|t| !matches!(t, Term::Literal(_)))
                {
                    if !rule.properties.iter().all(|(p, c)| meets(&focus, *p, c)) {
                        subjects.insert(focus.clone());
                        if rule.references {
                            objects.insert(focus);
                        }
                    }
                }
            }
        }
    }
    let hides = |q: &Quad| {
        let s: Term = q.subject.clone().into();
        subjects.contains(&s)
            || objects.contains(&q.object)
            || predicates.contains(&q.predicate)
            || pairs.contains(&(s, q.predicate.clone()))
    };

    quads.iter().filter(|q| !hides(q)).cloned().collect()
}

/// Checks one input of row rules (#208): a policy from the SHACL subset of sparql-guard,
/// judged by the engine, must hide what a small evaluator written here says it hides: queries
/// under the rules answer as on a store holding only the quads that evaluator shows.
pub fn check_rules(data: &[u8]) {
    use oxigraph::model::Triple;
    use std::sync::Arc;
    use tripletrail::{RowRule, RowRules, RowShapes};
    let mut b = Bytes::new(data);
    let mut quads = quads(&mut b);
    // Classes, for the class targets and `sh:class`.
    let rdf_type = NamedNode::new_unchecked(RDF_TYPE);
    for s in 0..4 {
        if b.next() & 1 != 0 {
            quads.push(Quad::new(
                iri("s", s),
                rdf_type.clone(),
                iri("c", b.below(2)),
                graph(b.below(GRAPHS + 1)),
            ));
        }
    }
    let union = b.next() & 1 != 0;
    let rules: Vec<Rule> = (0..1 + b.below(2)).map(|_| rule(&mut b)).collect();

    let mut ttl = String::from("@prefix sh: <http://www.w3.org/ns/shacl#> .\n");
    for (n, rule) in rules.iter().enumerate() {
        ttl += &shape_turtle(n, rule);
    }
    let shapes: oxigraph::model::Graph =
        oxigraph::io::RdfParser::from_format(oxigraph::io::RdfFormat::Turtle)
            .for_slice(ttl.as_bytes())
            .map(|q| Triple::from(q.unwrap()))
            .collect();
    let row_rules = RowRules {
        shapes: Arc::new(RowShapes::new(shapes)),
        rules: rules
            .iter()
            .enumerate()
            .map(|(n, rule)| {
                let shape: Term = iri("shape", n).into();
                if rule.hide {
                    RowRule::HideProperties { shape }
                } else {
                    RowRule::VisibleIf {
                        shape,
                        references: rule.references,
                    }
                }
            })
            .collect(),
        agent: None,
        graphs: None,
    };
    let full = Engine::new_tripletrail();
    full.load_quads(quads.clone()).unwrap();
    full.set_union_default_graph(union);
    let visible_of = |quads: &[Quad]| {
        let visible = Engine::new_tripletrail();
        visible.load_quads(shown_by(quads, &rules)).unwrap();
        visible.set_union_default_graph(union);
        visible
    };
    let mut visible = visible_of(&quads);
    for round in 0..6 {
        // From the third round on, a write before each query (by an admin, without rules):
        // the hidden set is brought up to date from the resources it touched (#644).
        if round >= 2 {
            let n = 1 + b.below(3);
            let mut more = quads_of(&mut b, n);
            if b.next() & 1 != 0 {
                more.push(Quad::new(
                    iri("s", b.below(4)),
                    rdf_type.clone(),
                    iri("c", b.below(2)),
                    graph(b.below(GRAPHS + 1)),
                ));
            }
            let delete = b.next() & 1 != 0 && !quads.is_empty();
            let text = |q: &Quad| match &q.graph_name {
                GraphName::NamedNode(g) => {
                    format!("GRAPH {g} {{ {} {} {} }}", q.subject, q.predicate, q.object)
                }
                _ => format!("{} {} {} .", q.subject, q.predicate, q.object),
            };
            let data: Vec<String> = if delete {
                let gone = quads[b.below(quads.len())].clone();
                quads.retain(|q| *q != gone);
                vec![text(&gone)]
            } else {
                for q in &more {
                    if !quads.contains(q) {
                        quads.push(q.clone());
                    }
                }
                more.iter().map(text).collect()
            };
            let update = format!(
                "{} DATA {{ {} }}",
                if delete { "DELETE" } else { "INSERT" },
                data.join(" ")
            );
            full.update(&update).unwrap();
            visible = visible_of(&quads);
        }
        let query = query(&mut b);
        let seen = answer(
            &full,
            &query,
            QueryOptions {
                row_rules: Some(row_rules.clone()),
                ..Default::default()
            },
        );
        let expected = answer(&visible, &query, QueryOptions::default());
        assert_eq!(
            seen, expected,
            "{query}\nshapes:\n{ttl}rules {rules:?}\nunion {union}\nquads: {quads:#?}"
        );
        if b.exhausted() {
            break;
        }
    }
}
