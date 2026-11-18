//! `pgr:constraint`: a SPARQL boolean expression over the extra columns of an edge set
//! (`?c_gauge = 1435 && ?c_electrified`). It is compiled once per network and evaluated once
//! per edge-set row; edges with a row that fails are left out of the search. Errors (an
//! unbound column, comparing a number with a string, ...) count as false, as in `FILTER`.

use crate::service::PgrError;
use oxigraph::model::Term;
use spargebra::algebra::{Expression, Function, GraphPattern};
use std::borrow::Cow;

/// A compiled constraint. Variables are resolved to column positions of the edge set's
/// extra columns.
#[derive(Debug, Clone)]
pub(crate) struct Constraint {
    expr: Expr,
}

#[derive(Debug, Clone)]
enum Expr {
    Const(Const),
    Column(usize),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Compare(Cmp, Box<Expr>, Box<Expr>),
    Arith(Op, Box<Expr>, Box<Expr>),
    Neg(Box<Expr>),
    Bound(usize),
    In(Box<Expr>, Vec<Expr>),
    If(Box<Expr>, Box<Expr>, Box<Expr>),
    Coalesce(Vec<Expr>),
    Call(Func, Vec<Expr>),
}

#[derive(Debug, Clone)]
enum Const {
    Num(f64),
    Bool(bool),
    Str(String),
    Iri(String),
}

#[derive(Debug, Clone, Copy)]
enum Cmp {
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
    SameTerm,
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Debug, Clone, Copy)]
enum Func {
    Str,
    LCase,
    UCase,
    Contains,
    StrStarts,
    StrEnds,
    Abs,
    StrLen,
}

/// A value during evaluation; strings borrow from the row where they can.
#[derive(Debug, Clone, PartialEq)]
enum Val<'a> {
    Num(f64),
    Bool(bool),
    Str(Cow<'a, str>),
    Iri(&'a str),
}

impl Constraint {
    /// Parses `source` as a SPARQL expression; `column` maps a variable name to its position
    /// in the row passed to [`Constraint::accepts`], or `None` if the edge set lacks it.
    pub(crate) fn parse(
        source: &str,
        column: impl Fn(&str) -> Option<usize>,
    ) -> Result<Self, PgrError> {
        let query = format!(
            "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> \
             PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> \
             SELECT * WHERE {{ FILTER({source}) }}"
        );
        let bad = |e: String| PgrError::Request(format!("pgr:constraint: {e}"));
        let pattern = match spargebra::SparqlParser::new().parse_query(&query) {
            Ok(spargebra::Query::Select { pattern, .. }) => pattern,
            Ok(_) => unreachable!(),
            Err(e) => return Err(bad(e.to_string())),
        };
        let expr = find_filter(&pattern).ok_or_else(|| bad("not an expression".into()))?;
        Ok(Self {
            expr: compile(expr, &column).map_err(bad)?,
        })
    }

    /// Whether a row passes. `row[i]` is the value of column `i`.
    pub(crate) fn accepts(&self, row: &[Option<Term>]) -> bool {
        eval(&self.expr, row).and_then(|v| ebv(&v)).unwrap_or(false)
    }
}

fn find_filter(p: &GraphPattern) -> Option<&Expression> {
    match p {
        GraphPattern::Filter { expr, .. } => Some(expr),
        GraphPattern::Project { inner, .. } => find_filter(inner),
        _ => None,
    }
}

fn compile(e: &Expression, column: &impl Fn(&str) -> Option<usize>) -> Result<Expr, String> {
    let c = |e: &Expression| compile(e, column).map(Box::new);
    let var =
        |name: &str| column(name).ok_or_else(|| format!("the edge set does not project ?{name}"));
    Ok(match e {
        Expression::Variable(v) => Expr::Column(var(v.as_str())?),
        Expression::Bound(v) => Expr::Bound(var(v.as_str())?),
        Expression::NamedNode(n) => Expr::Const(Const::Iri(n.as_str().to_owned())),
        Expression::Literal(l) => Expr::Const(match value(&Term::Literal(l.clone())) {
            Some(Val::Num(n)) => Const::Num(n),
            Some(Val::Bool(b)) => Const::Bool(b),
            _ => Const::Str(l.value().to_owned()),
        }),
        Expression::And(a, b) => Expr::And(c(a)?, c(b)?),
        Expression::Or(a, b) => Expr::Or(c(a)?, c(b)?),
        Expression::Not(a) => Expr::Not(c(a)?),
        Expression::Equal(a, b) => Expr::Compare(Cmp::Eq, c(a)?, c(b)?),
        Expression::SameTerm(a, b) => Expr::Compare(Cmp::SameTerm, c(a)?, c(b)?),
        Expression::Less(a, b) => Expr::Compare(Cmp::Lt, c(a)?, c(b)?),
        Expression::LessOrEqual(a, b) => Expr::Compare(Cmp::Le, c(a)?, c(b)?),
        Expression::Greater(a, b) => Expr::Compare(Cmp::Gt, c(a)?, c(b)?),
        Expression::GreaterOrEqual(a, b) => Expr::Compare(Cmp::Ge, c(a)?, c(b)?),
        Expression::Add(a, b) => Expr::Arith(Op::Add, c(a)?, c(b)?),
        Expression::Subtract(a, b) => Expr::Arith(Op::Sub, c(a)?, c(b)?),
        Expression::Multiply(a, b) => Expr::Arith(Op::Mul, c(a)?, c(b)?),
        Expression::Divide(a, b) => Expr::Arith(Op::Div, c(a)?, c(b)?),
        Expression::UnaryPlus(a) => compile(a, column)?,
        Expression::UnaryMinus(a) => Expr::Neg(c(a)?),
        Expression::In(a, list) => Expr::In(
            c(a)?,
            list.iter()
                .map(|e| compile(e, column))
                .collect::<Result<_, _>>()?,
        ),
        Expression::If(a, b, d) => Expr::If(c(a)?, c(b)?, c(d)?),
        Expression::Coalesce(list) => Expr::Coalesce(
            list.iter()
                .map(|e| compile(e, column))
                .collect::<Result<_, _>>()?,
        ),
        Expression::FunctionCall(f, args) => {
            let (func, arity) = match f {
                Function::Str => (Func::Str, 1),
                Function::LCase => (Func::LCase, 1),
                Function::UCase => (Func::UCase, 1),
                Function::Contains => (Func::Contains, 2),
                Function::StrStarts => (Func::StrStarts, 2),
                Function::StrEnds => (Func::StrEnds, 2),
                Function::Abs => (Func::Abs, 1),
                Function::StrLen => (Func::StrLen, 1),
                other => return Err(format!("unsupported function {other}")),
            };
            if args.len() != arity {
                return Err(format!("{f} takes {arity} argument(s)"));
            }
            Expr::Call(
                func,
                args.iter()
                    .map(|e| compile(e, column))
                    .collect::<Result<_, _>>()?,
            )
        }
        Expression::Exists(_) => return Err("EXISTS is not supported".into()),
    })
}

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

/// The value of a term: numbers and booleans by datatype, other literals as strings.
fn value(t: &Term) -> Option<Val<'_>> {
    match t {
        Term::NamedNode(n) => Some(Val::Iri(n.as_str())),
        Term::BlankNode(b) => Some(Val::Iri(b.as_str())),
        Term::Literal(l) => {
            let dt = l.datatype().as_str();
            match dt.strip_prefix(XSD) {
                Some("boolean") => match l.value() {
                    "true" | "1" => Some(Val::Bool(true)),
                    "false" | "0" => Some(Val::Bool(false)),
                    _ => None,
                },
                Some(
                    "integer" | "decimal" | "double" | "float" | "int" | "long" | "short" | "byte"
                    | "nonNegativeInteger" | "positiveInteger" | "negativeInteger"
                    | "nonPositiveInteger" | "unsignedInt" | "unsignedLong" | "unsignedShort"
                    | "unsignedByte",
                ) => l.value().trim().parse().ok().map(Val::Num),
                _ => Some(Val::Str(Cow::Borrowed(l.value()))),
            }
        }
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

fn ebv(v: &Val<'_>) -> Option<bool> {
    match v {
        Val::Bool(b) => Some(*b),
        Val::Num(n) => Some(*n != 0.0 && !n.is_nan()),
        Val::Str(s) => Some(!s.is_empty()),
        Val::Iri(_) => None,
    }
}

fn string<'a>(v: Val<'a>) -> Option<Cow<'a, str>> {
    match v {
        Val::Str(s) => Some(s),
        Val::Iri(s) => Some(Cow::Borrowed(s)),
        Val::Num(n) => Some(Cow::Owned(n.to_string())),
        Val::Bool(b) => Some(Cow::Owned(b.to_string())),
    }
}

fn compare(op: Cmp, a: &Val<'_>, b: &Val<'_>) -> Option<bool> {
    use std::cmp::Ordering;
    let order = match (a, b) {
        (Val::Num(x), Val::Num(y)) => x.partial_cmp(y),
        (Val::Bool(x), Val::Bool(y)) => Some(x.cmp(y)),
        (Val::Str(x), Val::Str(y)) => Some(x.cmp(y)),
        (Val::Iri(x), Val::Iri(y)) => match op {
            Cmp::Eq | Cmp::SameTerm => Some(x.cmp(y)),
            _ => None,
        },
        _ => {
            return match op {
                Cmp::Eq | Cmp::SameTerm => Some(false),
                _ => None,
            };
        }
    };
    let o = order?;
    Some(match op {
        Cmp::Eq | Cmp::SameTerm => o == Ordering::Equal,
        Cmp::Lt => o == Ordering::Less,
        Cmp::Le => o != Ordering::Greater,
        Cmp::Gt => o == Ordering::Greater,
        Cmp::Ge => o != Ordering::Less,
    })
}

fn eval<'a>(e: &'a Expr, row: &'a [Option<Term>]) -> Option<Val<'a>> {
    let num = |e: &'a Expr| match eval(e, row)? {
        Val::Num(n) => Some(n),
        _ => None,
    };
    let truth = |e: &'a Expr| eval(e, row).and_then(|v| ebv(&v));
    Some(match e {
        Expr::Const(Const::Num(n)) => Val::Num(*n),
        Expr::Const(Const::Bool(b)) => Val::Bool(*b),
        Expr::Const(Const::Str(s)) => Val::Str(Cow::Borrowed(s)),
        Expr::Const(Const::Iri(s)) => Val::Iri(s),
        Expr::Column(i) => value(row.get(*i)?.as_ref()?)?,
        Expr::Bound(i) => Val::Bool(row.get(*i).is_some_and(Option::is_some)),
        Expr::And(a, b) => Val::Bool(match (truth(a), truth(b)) {
            (Some(false), _) | (_, Some(false)) => false,
            (Some(true), Some(true)) => true,
            _ => return None,
        }),
        Expr::Or(a, b) => Val::Bool(match (truth(a), truth(b)) {
            (Some(true), _) | (_, Some(true)) => true,
            (Some(false), Some(false)) => false,
            _ => return None,
        }),
        Expr::Not(a) => Val::Bool(!truth(a)?),
        Expr::Compare(op, a, b) => Val::Bool(compare(*op, &eval(a, row)?, &eval(b, row)?)?),
        Expr::Arith(op, a, b) => {
            let (x, y) = (num(a)?, num(b)?);
            Val::Num(match op {
                Op::Add => x + y,
                Op::Sub => x - y,
                Op::Mul => x * y,
                Op::Div => x / y,
            })
        }
        Expr::Neg(a) => Val::Num(-num(a)?),
        Expr::In(a, list) => {
            let a = eval(a, row)?;
            let mut error = false;
            for item in list {
                match eval(item, row).and_then(|v| compare(Cmp::Eq, &a, &v)) {
                    Some(true) => return Some(Val::Bool(true)),
                    Some(false) => {}
                    None => error = true,
                }
            }
            if error {
                return None;
            }
            Val::Bool(false)
        }
        Expr::If(c, a, b) => {
            if truth(c)? {
                eval(a, row)?
            } else {
                eval(b, row)?
            }
        }
        Expr::Coalesce(list) => list.iter().find_map(|e| eval(e, row))?,
        Expr::Call(f, args) => {
            let arg = |i: usize| eval(&args[i], row);
            match f {
                Func::Str => Val::Str(string(arg(0)?)?),
                Func::LCase => Val::Str(Cow::Owned(string(arg(0)?)?.to_lowercase())),
                Func::UCase => Val::Str(Cow::Owned(string(arg(0)?)?.to_uppercase())),
                Func::StrLen => Val::Num(string(arg(0)?)?.chars().count() as f64),
                Func::Abs => Val::Num(num(&args[0])?.abs()),
                Func::Contains | Func::StrStarts | Func::StrEnds => {
                    let (Val::Str(a), Val::Str(b)) = (arg(0)?, arg(1)?) else {
                        return None;
                    };
                    Val::Bool(match f {
                        Func::Contains => a.contains(&*b),
                        Func::StrStarts => a.starts_with(&*b),
                        _ => a.ends_with(&*b),
                    })
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::model::{Literal, NamedNode};

    fn check(source: &str, row: &[Option<Term>]) -> bool {
        let names = ["c_gauge", "c_electrified", "c_kind", "c_axle"];
        Constraint::parse(source, |n| names.iter().position(|x| *x == n))
            .unwrap()
            .accepts(row)
    }

    #[test]
    fn constraints() {
        let row = [
            Some(Literal::from(1435).into()),
            Some(Literal::from(true).into()),
            Some(NamedNode::new_unchecked("http://e/main").into()),
            None,
        ];
        assert!(check("?c_gauge = 1435 && ?c_electrified", &row));
        assert!(!check("?c_gauge = 1668", &row));
        assert!(check("?c_gauge IN (1435, 1668)", &row));
        assert!(check("?c_kind = <http://e/main>", &row));
        assert!(check("?c_kind != <http://e/branch>", &row));
        assert!(check("strends(str(?c_kind), \"main\")", &row));
        // An unbound column is an error, so false, unless guarded.
        assert!(!check("?c_axle >= 22.5", &row));
        assert!(check("!bound(?c_axle) || ?c_axle >= 22.5", &row));
        assert!(check("coalesce(?c_axle, 25) >= 22.5", &row));
        assert!(check("?c_gauge * 2 > 2000", &row));
        // Comparing a number with an IRI is an error.
        assert!(!check("?c_kind > 3", &row));
        assert!(
            Constraint::parse("?c_speed > 100", |_| None).is_err(),
            "unknown columns are reported"
        );
        assert!(Constraint::parse("regex(?c_gauge, \"1\")", |_| Some(0)).is_err());
    }
}
