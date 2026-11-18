//! Database tables and queries as sources: `table=<name>` or `sql=<SELECT …>`, read from a
//! connection the command line or the embedding application names ([`Databases`]), never
//! one given in the query. The root holds one container per row (`rdf:_1`, `rdf:_2`, ...),
//! and a row a key per column with the value's natural literal (`xsd:integer`,
//! `xsd:decimal`, `xsd:date`, ...); a NULL gives no triple. With `blank-nodes=false` the
//! rows are `<root>#row1`, `<root>#row2`, ... as for CSV. Geometry columns (PostGIS) are
//! `geo:wktLiteral`s; `geo.wkt` or `geo.x`/`geo.y` name text or number columns to make
//! `geo:asWKT` from, as for CSV.

use crate::geo;
use crate::options::Options;
use crate::tree::Sink;
use crate::{FxError, XYZ};
use oxigraph::model::{BlankNode, Literal, NamedNode, NamedOrBlankNode, Variable};
use spargebra::term::GroundTerm;

/// The rows a `table=` or `sql=` source may give by default (`database.max-rows`).
pub const DEFAULT_MAX_ROWS: usize = 1_000_000;

/// The databases `table=` and `sql=` sources read from.
pub trait Databases: Send + Sync + std::fmt::Debug {
    /// The rows of `query`, run on the connection `name` (the default connection when
    /// `None`). An implementation runs it read-only, as one statement, and fails when it
    /// gives more than `max_rows` rows. It may leave out what `select` says the SERVICE
    /// pattern cannot match.
    fn rows(
        &self,
        name: Option<&str>,
        query: &Query,
        select: &Select<'_>,
        max_rows: usize,
    ) -> Result<Rows, FxError>;
}

/// Values per database query in a bind join (a `WHERE … IN`).
pub(crate) const BIND_JOIN_BATCH: usize = 1_000;

/// What the SERVICE pattern can match, so that a database reads less.
pub struct Select<'a> {
    /// Whether the pattern can match a column; the others may be left out.
    pub wanted: &'a dyn Fn(&str) -> bool,
    /// The key (predicate IRI) of a column.
    pub key: &'a dyn Fn(&str) -> String,
    /// Conditions that every row an answer uses meets: rows that fail one may be left out.
    /// The pattern still checks them, so a database may check fewer, never others.
    pub conditions: &'a [Condition],
    /// Values a column must take one of (a bind join's `VALUES`), checked as `conditions`.
    pub one_of: &'a [OneOf],
    /// Most rows wanted: any that many will do (the pattern takes a `LIMIT` of answers, each
    /// from one row). The caller asks again for more when they gave too few answers.
    pub limit: Option<usize>,
}

/// A row's value for the column with key `key` is one of `values` (RDF term equality, so
/// SPARQL `=` with each is looser and may be checked instead).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneOf {
    pub key: String,
    pub values: Vec<Literal>,
}

/// A row's value for the column with key `key` compares to `value` as `op` says (SPARQL
/// comparison).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    pub key: String,
    pub op: Op,
    pub value: Literal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
    /// The geometries intersect (`geof:sfIntersects` and the predicates that imply it,
    /// such as `sfWithin`): a database checks that their bounding boxes overlap.
    Intersects,
}

/// GeoSPARQL functions true only of geometries that intersect.
const INTERSECTING: &[&str] = &[
    "http://www.opengis.net/def/function/geosparql/sfIntersects",
    "http://www.opengis.net/def/function/geosparql/sfWithin",
    "http://www.opengis.net/def/function/geosparql/sfContains",
    "http://www.opengis.net/def/function/geosparql/sfOverlaps",
    "http://www.opengis.net/def/function/geosparql/sfTouches",
    "http://www.opengis.net/def/function/geosparql/sfCrosses",
    "http://www.opengis.net/def/function/geosparql/sfEquals",
];

impl Op {
    /// The operator with its sides swapped: `5 < ?v` is `?v > 5`.
    pub fn flip(self) -> Self {
        match self {
            Op::Eq => Op::Eq,
            Op::Lt => Op::Gt,
            Op::Le => Op::Ge,
            Op::Gt => Op::Lt,
            Op::Ge => Op::Le,
            Op::Intersects => Op::Intersects,
        }
    }

    /// As SQL writes it.
    pub fn sql(self) -> &'static str {
        match self {
            Op::Eq => "=",
            Op::Lt => "<",
            Op::Le => "<=",
            Op::Gt => ">",
            Op::Ge => ">=",
            Op::Intersects => "&&",
        }
    }
}

/// What a source asks a database for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// All rows of a table, `schema.table` or `table`, as written (quote a name with
    /// capitals or other characters as in SQL: `"Tracks"`).
    Table(String),
    /// A query, one `SELECT` (or `VALUES`, `TABLE`) statement.
    Sql(String),
}

/// A result: the column names, and per row a value per column (`None` for NULL).
#[derive(Debug, Clone, Default)]
pub struct Rows {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<Literal>>>,
    /// The statement the database ran, and its parameters, for `explain`.
    pub statement: Option<String>,
    pub params: Vec<String>,
}

/// A database query a SERVICE call made, as `explain` shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct DatabaseCall {
    /// The connection name (`database=`), or `None` for the default one.
    pub connection: Option<String>,
    /// The statement run, or the source's query when the database does not say.
    pub sql: String,
    pub params: Vec<String>,
    pub rows: usize,
    pub millis: f64,
}

thread_local! {
    static CALLS: std::cell::RefCell<Option<Vec<DatabaseCall>>> =
        const { std::cell::RefCell::new(None) };
}

/// Runs `f`, and gives the database queries the SERVICE calls it made on this thread.
pub fn record_database_calls<R>(f: impl FnOnce() -> R) -> (R, Vec<DatabaseCall>) {
    let outer = CALLS.with(|c| c.replace(Some(Vec::new())));
    let result = f();
    let calls = CALLS.with(|c| c.replace(outer)).unwrap_or_default();
    (result, calls)
}

pub(crate) fn record(call: impl FnOnce() -> DatabaseCall) {
    CALLS.with(|c| {
        if let Some(calls) = c.borrow_mut().as_mut() {
            calls.push(call());
        }
    });
}

/// Whether the options name a database source.
pub(crate) fn is_database(options: &Options) -> bool {
    options.get("table").is_some() || options.get("sql").is_some()
}

/// The query the options ask for, with the connection name.
pub(crate) fn query_of(options: &Options) -> Result<(Option<&str>, Query), FxError> {
    let query = match (options.get("table"), options.get("sql")) {
        (Some(_), Some(_)) => return Err(FxError::Option("table or sql, not both".into())),
        (Some(t), None) => {
            let t = t.trim();
            if !is_table_name(t) {
                return Err(FxError::Option(format!(
                    "table {t:?}: not a table name (write a query with sql= instead)"
                )));
            }
            Query::Table(t.to_owned())
        }
        (None, Some(q)) => {
            let q = q.trim().trim_end_matches(';').trim_end();
            if q.is_empty() {
                return Err(FxError::Option("sql is empty".into()));
            }
            Query::Sql(q.to_owned())
        }
        (None, None) => unreachable!("checked by is_database"),
    };
    for other in ["location", "content", "from-archive"] {
        if options.get(other).is_some() {
            return Err(FxError::Option(format!(
                "{other} and a table or sql source, not both"
            )));
        }
    }
    Ok((options.get("database"), query))
}

/// `name`, `schema.name`, or their parts quoted (`"Tracks"`, with `""` for a quote).
fn is_table_name(s: &str) -> bool {
    let mut rest = s;
    let mut parts = 0;
    loop {
        parts += 1;
        if let Some(quoted) = rest.strip_prefix('"') {
            // Up to the closing quote; `""` inside is a quote.
            let mut i = 0;
            let b = quoted.as_bytes();
            loop {
                match b.get(i) {
                    None => return false,
                    Some(b'"') if b.get(i + 1) == Some(&b'"') => i += 2,
                    Some(b'"') => break,
                    Some(_) => i += 1,
                }
            }
            if i == 0 {
                return false;
            }
            rest = &quoted[i + 1..];
        } else {
            let end = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
                .unwrap_or(rest.len());
            let (word, after) = rest.split_at(end);
            if word.is_empty() || word.starts_with(|c: char| c.is_ascii_digit() || c == '$') {
                return false;
            }
            rest = after;
        }
        match rest.strip_prefix('.') {
            Some(after) if parts < 3 => rest = after,
            Some(_) => return false,
            None => return rest.is_empty(),
        }
    }
}

/// The root of a database source without `root`: made from the connection and the query.
pub(crate) fn default_root(name: Option<&str>, query: &Query) -> Result<String, FxError> {
    let text = match query {
        Query::Table(t) => format!("{}\ntable\n{t}", name.unwrap_or_default()),
        Query::Sql(q) => format!("{}\nsql\n{q}", name.unwrap_or_default()),
    };
    Ok(format!("{XYZ}{}", crate::md5_hex(&text)?))
}

/// The rows' containers and values.
pub(crate) fn triplify(rows: Rows, options: &Options, sink: &mut Sink<'_>) -> Result<(), FxError> {
    let root = sink.root();
    let row_prefix = {
        let r = &sink.common.root;
        if r.ends_with('/') || r.ends_with('#') {
            format!("{r}row")
        } else {
            format!("{r}#row")
        }
    };
    let columns: Vec<Option<NamedNode>> = rows
        .columns
        .iter()
        .map(|name| sink.predicate(sink.key_iri(name)))
        .collect();
    let headers: Vec<Option<String>> = rows.columns.iter().cloned().map(Some).collect();
    let geometry = match geo::Columns::from_options(options)? {
        Some(columns) => sink
            .predicate(geo::AS_WKT.to_owned())
            .map(|p| Ok::<_, FxError>((p, columns.resolve(&headers)?, columns.crs)))
            .transpose()?,
        None => None,
    };
    let wkt_literal = NamedNode::new_unchecked(geo::WKT_LITERAL);
    for (n, row) in rows.rows.into_iter().enumerate() {
        let c: NamedOrBlankNode = if sink.common.blank_nodes {
            BlankNode::default().into()
        } else {
            NamedNode::new_unchecked(format!("{row_prefix}{}", n + 1)).into()
        };
        sink.slot(&root, n + 1, c.clone().into());
        if let Some((p, positions, crs)) = &geometry
            && let Some(wkt) = {
                // Numbers as plain decimals (`10.75`, not the canonical `1.075E1`), as CSV
                // gives them.
                // Only the cells it reads are copied.
                let mut plain: Vec<Option<String>> = vec![None; row.len()];
                for i in positions.columns() {
                    if let Some(Some(v)) = row.get(i) {
                        plain[i] = Some(plain_number(v));
                    }
                }
                positions.wkt(crs, |i| plain.get(i).and_then(Option::as_deref))
            }
        {
            sink.emit(
                &c,
                p,
                Literal::new_typed_literal(wkt, wkt_literal.clone()).into(),
            );
        }
        for (p, value) in columns.iter().zip(row) {
            let (Some(p), Some(value)) = (p, value) else {
                continue;
            };
            if let Some(o) = sink.typed(value.value(), value.datatype()) {
                sink.emit(&c, p, o);
            }
        }
    }
    Ok(())
}

/// A literal's text, with `xsd:double` and `xsd:float` written as plain decimals.
fn plain_number(value: &Literal) -> String {
    let dt = value.datatype();
    if (dt == oxigraph::model::vocab::xsd::DOUBLE || dt == oxigraph::model::vocab::xsd::FLOAT)
        && let Ok(f) = value.value().parse::<f64>()
        && f.is_finite()
    {
        return f.to_string();
    }
    value.value().to_owned()
}

/// `database.max-rows`, by default [`DEFAULT_MAX_ROWS`].
pub(crate) fn max_rows(options: &Options) -> Result<usize, FxError> {
    match options.get("database.max-rows") {
        None => Ok(DEFAULT_MAX_ROWS),
        Some(n) => n.trim().parse().ok().filter(|&n| n > 0).ok_or_else(|| {
            FxError::Option(format!("database.max-rows {n:?} is not a positive number"))
        }),
    }
}

/// A pattern about one row: optionally a bind join's `VALUES` joined to FILTERs over one
/// group of triples with the same subject variable and constant predicates. Gives the
/// `VALUES`, the FILTER expressions, and the key of each object variable.
#[expect(clippy::type_complexity)]
fn one_row(
    pattern: &spargebra::algebra::GraphPattern,
) -> Option<(
    Option<(&[Variable], &[Vec<Option<GroundTerm>>])>,
    Vec<&spargebra::algebra::Expression>,
    std::collections::HashMap<Variable, String>,
)> {
    use spargebra::algebra::GraphPattern;
    use spargebra::term::{NamedNodePattern, TermPattern};
    let mut p = pattern;
    let mut values = None;
    if let GraphPattern::Join { left, right } = p {
        match (&**left, &**right) {
            (
                GraphPattern::Values {
                    variables,
                    bindings,
                },
                other,
            )
            | (
                other,
                GraphPattern::Values {
                    variables,
                    bindings,
                },
            ) => {
                values = Some((variables.as_slice(), bindings.as_slice()));
                p = other;
            }
            _ => return None,
        }
    }
    let mut filters = Vec::new();
    while let GraphPattern::Filter { expr, inner } = p {
        filters.push(expr);
        p = inner;
    }
    let GraphPattern::Bgp { patterns } = p else {
        return None;
    };
    let mut subject = None;
    let mut keys = std::collections::HashMap::new();
    for t in patterns {
        let TermPattern::Variable(s) = &t.subject else {
            return None;
        };
        if subject.get_or_insert(s) != &s {
            return None;
        }
        let NamedNodePattern::NamedNode(key) = &t.predicate else {
            return None;
        };
        if let TermPattern::Variable(o) = &t.object
            && o != s
        {
            keys.entry(o.clone())
                .or_insert_with(|| key.as_str().to_owned());
        }
    }
    Some((values, filters, keys))
}

/// The columns a bind join's `VALUES` gives the values of. A row's values are literals, so
/// IRIs among them are left out; a column with an unbound value is left out.
pub(crate) fn one_of(pattern: &spargebra::algebra::GraphPattern) -> Vec<OneOf> {
    let Some((Some((variables, bindings)), _, keys)) = one_row(pattern) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, v) in variables.iter().enumerate() {
        let Some(key) = keys.get(v) else { continue };
        let mut values: Vec<Literal> = Vec::new();
        let all = bindings.iter().all(|row| match row.get(i) {
            Some(Some(GroundTerm::Literal(l))) => {
                if !values.contains(l) {
                    values.push(l.clone());
                }
                true
            }
            Some(Some(GroundTerm::NamedNode(_))) => true,
            _ => false,
        });
        if all {
            out.push(OneOf {
                key: key.clone(),
                values,
            });
        }
    }
    out
}

/// Whether a SERVICE pattern is about one row, without a bind join's `VALUES`: each row
/// gives at most one answer, so a `LIMIT` of answers needs at most that many rows.
pub(crate) fn per_row(pattern: &spargebra::algebra::GraphPattern) -> bool {
    one_row(pattern).is_some_and(|(values, _, _)| values.is_none())
}

/// Whether a SERVICE pattern is about one row and some of `shared` are its values, so that a
/// bind join on them can be passed to the database.
pub(crate) fn binds_on(pattern: &spargebra::algebra::GraphPattern, shared: &[Variable]) -> bool {
    one_row(pattern).is_some_and(|(values, _, keys)| {
        values.is_none() && shared.iter().any(|v| keys.contains_key(v))
    })
}

/// The conditions of a SERVICE pattern that is one group of triples about one row, with
/// FILTERs comparing their values to constants: `?r xyz:km ?km FILTER(?km > 10)`. Rows
/// failing them can give no answer, since every answer is about one row that has all the
/// triples. Anything else (OPTIONAL, UNION, BIND, several subjects, paths) gives none.
pub(crate) fn conditions(pattern: &spargebra::algebra::GraphPattern) -> Vec<Condition> {
    use spargebra::algebra::Expression;
    let Some((_, filters, keys)) = one_row(pattern) else {
        return Vec::new();
    };
    let mut conjuncts = Vec::new();
    let mut stack: Vec<&Expression> = filters;
    while let Some(e) = stack.pop() {
        match e {
            Expression::And(a, b) => {
                stack.push(a);
                stack.push(b);
            }
            e => conjuncts.push(e),
        }
    }
    let mut out = Vec::new();
    for e in conjuncts {
        let (op, a, b): (Op, &Expression, &Expression) = match e {
            Expression::Equal(a, b) => (Op::Eq, a, b),
            Expression::Less(a, b) => (Op::Lt, a, b),
            Expression::LessOrEqual(a, b) => (Op::Le, a, b),
            Expression::Greater(a, b) => (Op::Gt, a, b),
            Expression::GreaterOrEqual(a, b) => (Op::Ge, a, b),
            Expression::FunctionCall(spargebra::algebra::Function::Custom(f), args)
                if args.len() == 2 && INTERSECTING.contains(&f.as_str()) =>
            {
                (Op::Intersects, &args[0], &args[1])
            }
            _ => continue,
        };
        let (op, v, l) = match (a, b) {
            (Expression::Variable(v), Expression::Literal(l)) => (op, v, l),
            (Expression::Literal(l), Expression::Variable(v)) => (op.flip(), v, l),
            _ => continue,
        };
        if let Some(key) = keys.get(v) {
            out.push(Condition {
                key: key.clone(),
                op,
                value: l.clone(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditions_of_one_row() {
        let pattern = |q: &str| {
            let spargebra::Query::Select { pattern, .. } = spargebra::SparqlParser::new()
                .parse_query(&format!("PREFIX x: <http://x/> SELECT * {{ {q} }}"))
                .unwrap()
            else {
                unreachable!()
            };
            // Under the projection.
            let spargebra::algebra::GraphPattern::Project { inner, .. } = pattern else {
                unreachable!()
            };
            *inner
        };
        let c = conditions(&pattern(
            "?r x:km ?km ; x:code ?c FILTER(?km > 10 && 20 >= ?km) FILTER(?c = 'OSL')",
        ));
        let mut c: Vec<String> = c
            .iter()
            .map(|c| format!("{} {} {}", c.key, c.op.sql(), c.value))
            .collect();
        c.sort();
        assert_eq!(
            c,
            [
                "http://x/code = \"OSL\"",
                "http://x/km <= \"20\"^^<http://www.w3.org/2001/XMLSchema#integer>",
                "http://x/km > \"10\"^^<http://www.w3.org/2001/XMLSchema#integer>"
            ]
        );
        for none in [
            // Two rows.
            "?r x:km ?km . ?s x:code ?c FILTER(?km > 10)",
            // Optional.
            "?r x:code ?c OPTIONAL { ?r x:km ?km } FILTER(?km > 10)",
            // A variable predicate.
            "?r ?p ?km FILTER(?km > 10)",
            // Or.
            "?r x:km ?km FILTER(?km > 10 || ?km < 2)",
        ] {
            assert!(conditions(&pattern(none)).is_empty(), "{none}");
        }
    }

    #[test]
    fn table_names() {
        for good in [
            "tracks",
            "rail.tracks",
            "\"Tracks\"",
            "rail.\"Spor \"\"1\"\"\"",
            "db.rail.tracks",
            "spor_æøå",
        ] {
            assert!(is_table_name(good), "{good}");
        }
        for bad in [
            "",
            "tracks; drop table x",
            "tracks t",
            "a.b.c.d",
            "\"\"",
            "\"open",
            "1tracks",
            "rail.",
            "(select 1)",
        ] {
            assert!(!is_table_name(bad), "{bad}");
        }
    }
}
