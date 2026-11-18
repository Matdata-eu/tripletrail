//! Limits on the size of a query, checked before it is planned (#483).
//!
//! The parser, the optimizer, the planner and the evaluator are recursive, and the trees they
//! walk get as deep as the query is nested or long: 3,000 triple patterns, `UNION`s or
//! `FILTER`s become a chain of 3,000 joins, unions or filters. A query deep enough for a
//! thread's stack aborts the whole process, which no `catch_unwind` can stop. Two checks
//! refuse such a query with an error (a 400 from the server):
//!
//! - [`check_nesting`] looks at the text, before the parser recurses into it: brackets nested
//!   more than [`MAX_NESTING`] deep, or a longer run of `!`, `-` and `+` signs.
//! - [`check_query`] and [`check_update`] count the nodes of the parsed algebra, without
//!   recursing: graph patterns, triple patterns, path steps and expression nodes. More than
//!   [`MAX_NODES`] is refused. The values of `VALUES`, `IN` lists of constants and the data of
//!   `INSERT DATA` do not count: those are lists that every step reads in a loop.
//!
//! The limits leave room on the stack of the threads the server and the command line run
//! queries on ([`QUERY_STACK`]); on such a stack 30,000 nodes of each shape still answered
//! when this was written. The default stack of a thread (2 MB) holds about 1,000 levels.
//!
//! A program that uses [`crate::Engine`] as a library (or through Python) and runs queries
//! on its own threads should give them a stack of [`QUERY_STACK`]
//! (`std::thread::Builder::stack_size`): on a 2 MB stack, a query of 3,000 triple patterns,
//! well within the limits, still aborts the process.

use crate::Error;
use spargebra::algebra::{
    AggregateExpression, Expression, GraphPattern, OrderExpression, PropertyPathExpression,
};
use spargebra::{GraphUpdateOperation, Query, Update};

/// The deepest nesting of brackets, and the longest run of prefix signs, a query may have.
pub const MAX_NESTING: usize = 1_000;

/// The most algebra nodes a query or update may have.
pub const MAX_NODES: usize = 10_000;

/// The stack size of a thread that parses, plans or evaluates queries: 256 MB of address
/// space, of which only what a query uses is ever touched.
pub const QUERY_STACK: usize = 256 << 20;

// Facade-X `slice=true` evaluates its pattern on a thread of its own: as deep a stack.
const _: () = assert!(tripletrail_fx::SLICE_STACK >= QUERY_STACK);

/// Refuses a query or update text whose brackets nest deeper than [`MAX_NESTING`], or with a
/// longer run of `!`, `-` or `+` signs (each a level of the parser). Strings, IRIs and
/// comments are skipped.
pub fn check_nesting(text: &str) -> Result<(), Error> {
    let b = text.as_bytes();
    let (mut depth, mut deepest, mut signs, mut most_signs) = (0usize, 0usize, 0usize, 0usize);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'(' | b'[' | b'{' => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'"' | b'\'' => {
                let long = b[i..].starts_with(&[c, c, c]);
                i += if long { 3 } else { 1 };
                while i < b.len() {
                    if b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == c && (!long || b[i..].starts_with(&[c, c, c])) {
                        i += if long { 2 } else { 0 };
                        break;
                    }
                    if !long && b[i] == b'\n' {
                        break;
                    }
                    i += 1;
                }
            }
            // An IRI: `<` followed by no space up to `>`. A `<` that is a comparison is
            // followed by a space or `=`, or reaches a space before any `>`.
            b'<' => {
                if let Some(end) = b[i + 1..]
                    .iter()
                    .position(|&x| x == b'>' || x.is_ascii_whitespace() || x == b'<')
                    .filter(|&n| b[i + 1 + n] == b'>')
                {
                    i += end + 1;
                }
            }
            _ => {}
        }
        if matches!(c, b'!' | b'-' | b'+') && b.get(i + 1) != Some(&b'=') {
            signs += 1;
            most_signs = most_signs.max(signs);
        } else if !c.is_ascii_whitespace() && !matches!(c, b'(') {
            signs = 0;
        }
        i += 1;
    }
    if deepest > MAX_NESTING || most_signs > MAX_NESTING {
        return Err(Error::Request(format!(
            "the query nests {} levels deep, at most {MAX_NESTING} are allowed",
            deepest.max(most_signs)
        )));
    }
    Ok(())
}

/// Refuses a query of more than [`MAX_NODES`] algebra nodes.
pub fn check_query(query: &Query) -> Result<(), Error> {
    let pattern = match query {
        Query::Select { pattern, .. }
        | Query::Construct { pattern, .. }
        | Query::Describe { pattern, .. }
        | Query::Ask { pattern, .. } => pattern,
    };
    limit(Nodes::default().pattern(pattern).count())
}

/// Refuses an update whose patterns have more than [`MAX_NODES`] algebra nodes in all.
pub fn check_update(update: &Update) -> Result<(), Error> {
    let mut n = 0;
    for op in &update.operations {
        if let GraphUpdateOperation::DeleteInsert {
            delete,
            insert,
            pattern,
            ..
        } = op
        {
            n += delete.len() + insert.len() + Nodes::default().pattern(pattern).count();
        }
    }
    limit(n)
}

fn limit(n: usize) -> Result<(), Error> {
    if n > MAX_NODES {
        return Err(Error::Request(format!(
            "the query is too large: {n} patterns and expressions, at most {MAX_NODES} are \
             allowed"
        )));
    }
    Ok(())
}

/// Counts nodes with a stack of its own instead of recursion.
#[derive(Default)]
struct Nodes<'a> {
    todo: Vec<Node<'a>>,
}

enum Node<'a> {
    Pattern(&'a GraphPattern),
    Expression(&'a Expression),
    Path(&'a PropertyPathExpression),
}

impl<'a> Nodes<'a> {
    fn pattern(mut self, p: &'a GraphPattern) -> Self {
        self.todo.push(Node::Pattern(p));
        self
    }

    fn count(mut self) -> usize {
        let mut n = 0;
        while let Some(node) = self.todo.pop() {
            n += 1;
            // Stop early: a query far over the limit need not be counted to the end.
            if n > MAX_NODES {
                return n;
            }
            match node {
                Node::Pattern(p) => self.children(p, &mut n),
                Node::Expression(e) => self.expression(e),
                Node::Path(p) => match p {
                    PropertyPathExpression::NamedNode(_)
                    | PropertyPathExpression::NegatedPropertySet(_) => {}
                    PropertyPathExpression::Reverse(a)
                    | PropertyPathExpression::ZeroOrMore(a)
                    | PropertyPathExpression::OneOrMore(a)
                    | PropertyPathExpression::ZeroOrOne(a)
                    | PropertyPathExpression::Bounded { path: a, .. } => {
                        self.todo.push(Node::Path(a))
                    }
                    PropertyPathExpression::Sequence(a, b)
                    | PropertyPathExpression::Alternative(a, b) => {
                        self.todo.push(Node::Path(a));
                        self.todo.push(Node::Path(b));
                    }
                },
            }
        }
        n
    }

    fn children(&mut self, p: &'a GraphPattern, n: &mut usize) {
        let todo = &mut self.todo;
        match p {
            // Each triple pattern becomes a join of its own.
            GraphPattern::Bgp { patterns } => *n += patterns.len(),
            GraphPattern::Path { path, .. } => todo.push(Node::Path(path)),
            GraphPattern::Join { left, right }
            | GraphPattern::Union { left, right }
            | GraphPattern::Minus { left, right } => {
                todo.push(Node::Pattern(left));
                todo.push(Node::Pattern(right));
            }
            GraphPattern::LeftJoin {
                left,
                right,
                expression,
            } => {
                todo.push(Node::Pattern(left));
                todo.push(Node::Pattern(right));
                if let Some(e) = expression {
                    todo.push(Node::Expression(e));
                }
            }
            GraphPattern::Filter { expr, inner } => {
                todo.push(Node::Expression(expr));
                todo.push(Node::Pattern(inner));
            }
            GraphPattern::Extend {
                inner, expression, ..
            } => {
                todo.push(Node::Expression(expression));
                todo.push(Node::Pattern(inner));
            }
            GraphPattern::OrderBy { inner, expression } => {
                todo.push(Node::Pattern(inner));
                for e in expression {
                    let (OrderExpression::Asc(e) | OrderExpression::Desc(e)) = e;
                    todo.push(Node::Expression(e));
                }
            }
            GraphPattern::Group {
                inner, aggregates, ..
            } => {
                todo.push(Node::Pattern(inner));
                for (_, a) in aggregates {
                    if let AggregateExpression::FunctionCall { expr, .. } = a {
                        todo.push(Node::Expression(expr));
                    }
                }
            }
            GraphPattern::Graph { inner, .. }
            | GraphPattern::Project { inner, .. }
            | GraphPattern::Distinct { inner }
            | GraphPattern::Reduced { inner }
            | GraphPattern::Slice { inner, .. }
            | GraphPattern::Service { inner, .. } => todo.push(Node::Pattern(inner)),
            GraphPattern::Values { .. } => {}
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }

    fn expression(&mut self, e: &'a Expression) {
        let todo = &mut self.todo;
        match e {
            Expression::NamedNode(_)
            | Expression::Literal(_)
            | Expression::Variable(_)
            | Expression::Bound(_) => {}
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
            | Expression::Divide(a, b) => {
                todo.push(Node::Expression(a));
                todo.push(Node::Expression(b));
            }
            Expression::UnaryPlus(a) | Expression::UnaryMinus(a) | Expression::Not(a) => {
                todo.push(Node::Expression(a))
            }
            Expression::In(a, list) => {
                todo.push(Node::Expression(a));
                // A list of constants is read in a loop; anything else is an expression.
                todo.extend(
                    list.iter()
                        .filter(|e| !matches!(e, Expression::NamedNode(_) | Expression::Literal(_)))
                        .map(Node::Expression),
                );
            }
            Expression::Exists(p) => todo.push(Node::Pattern(p)),
            Expression::If(a, b, c) => {
                todo.push(Node::Expression(a));
                todo.push(Node::Expression(b));
                todo.push(Node::Expression(c));
            }
            Expression::Coalesce(args) | Expression::FunctionCall(_, args) => {
                todo.extend(args.iter().map(Node::Expression))
            }
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nodes(q: &str) -> usize {
        let q = spargebra::SparqlParser::new().parse_query(q).unwrap();
        let Query::Select { pattern, .. } = &q else {
            panic!()
        };
        Nodes::default().pattern(pattern).count()
    }

    #[test]
    fn nesting_skips_strings_iris_and_comments() {
        let deep = |n| format!("ASK {{ FILTER({}true{}) }}", "(".repeat(n), ")".repeat(n));
        assert!(check_nesting(&deep(900)).is_ok());
        assert!(check_nesting(&deep(1001)).is_err());
        let signs = |n| format!("ASK {{ FILTER({}true) }}", "! ".repeat(n));
        assert!(check_nesting(&signs(900)).is_ok());
        assert!(check_nesting(&signs(1001)).is_err());
        // `-` in a negative number list and `!=` are no levels.
        let list = format!("ASK {{ FILTER(?x IN ({})) }}", vec!["-1"; 5000].join(", "));
        assert!(check_nesting(&list).is_ok());
        let ne = format!("ASK {{ FILTER({}) }}", vec!["?x != 1"; 2000].join(" && "));
        assert!(check_nesting(&ne).is_ok());
        let quoted = "(".repeat(5000);
        for text in [
            format!("ASK {{ FILTER(?x = \"{quoted}\") }}"),
            format!("ASK {{ FILTER(?x = '''{quoted}\n''') }}"),
            format!("ASK {{ ?s ?p <urn:{quoted}> }}"),
            format!("# {quoted}\nASK {{}}"),
        ] {
            assert!(check_nesting(&text).is_ok(), "{}", &text[..40]);
        }
        // A comparison is no IRI: the bracket after it still counts.
        assert!(check_nesting(&format!("ASK {{ FILTER(?a < ?b && {}) }}", deep(1001))).is_err());
    }

    #[test]
    fn nodes_count_patterns_and_expressions_but_not_values() {
        // The projection, the BGP and its two triple patterns.
        assert_eq!(nodes("SELECT * { ?s ?p ?o . ?o ?q ?r }"), 4);
        assert_eq!(nodes("SELECT * { ?s ?p ?o FILTER(?o > 1 && ?o < 9) }"), 11);
        let values = (0..20_000)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(nodes(&format!("SELECT * {{ VALUES ?x {{ {values} }} }}")) < 5);
        let list = values.replace(' ', ",");
        assert!(nodes(&format!("SELECT * {{ ?s ?p ?x FILTER(?x IN ({list})) }}")) < 10);
        let union = vec!["{ ?s ?p ?o }"; 6000].join(" UNION ");
        assert!(nodes(&format!("SELECT * {{ {union} }}")) > MAX_NODES);
    }
}
