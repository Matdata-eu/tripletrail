//! tripletrail (#11): an expression compiled once with the evaluator's own code, for the id
//! executor, which calls it once per distinct tuple of values.

use crate::dataset::{ExpressionTerm, QueryableDataset};
use crate::expression::{
    CustomFunctionRegistry, ExpressionEvaluatorContext, build_expression_evaluator,
};
use oxiri::Iri;
use oxrdf::{Literal, NamedNode, Term, Variable};
use oxsdatatypes::DateTime;
use sparopt::algebra::GraphPattern;
use std::sync::{Arc, LazyLock};

struct Context<'a, 'v, D> {
    dataset: &'a D,
    variables: &'v [Variable],
}

/// No custom functions: the caller only compiles built-ins.
static NO_FUNCTIONS: LazyLock<CustomFunctionRegistry> = LazyLock::new(Default::default);

impl<'a, D: QueryableDataset<'a>> ExpressionEvaluatorContext<'a> for Context<'a, '_, D> {
    type Term = D::InternalTerm;
    type Tuple = Vec<Option<D::InternalTerm>>;
    type Error = ();

    fn build_variable_lookup(
        &mut self,
        variable: &Variable,
    ) -> impl Fn(&Vec<Option<D::InternalTerm>>) -> Option<D::InternalTerm> + 'a {
        let i = self.variables.iter().position(|v| v == variable);
        move |tuple| tuple.get(i?)?.clone()
    }

    fn build_is_variable_bound(
        &mut self,
        variable: &Variable,
    ) -> impl Fn(&Vec<Option<D::InternalTerm>>) -> bool + 'a {
        let i = self.variables.iter().position(|v| v == variable);
        move |tuple| i.is_some_and(|i| tuple.get(i).is_some_and(Option::is_some))
    }

    fn build_exists(
        &mut self,
        _: &GraphPattern,
    ) -> Result<impl Fn(&Vec<Option<D::InternalTerm>>) -> bool + 'a, ()> {
        Err::<fn(&Vec<Option<D::InternalTerm>>) -> bool, _>(())
    }

    fn internalize_named_node(&mut self, term: &NamedNode) -> Result<D::InternalTerm, ()> {
        self.dataset
            .internalize_term(term.clone().into())
            .map_err(|_| ())
    }

    fn internalize_literal(&mut self, term: &Literal) -> Result<D::InternalTerm, ()> {
        self.dataset
            .internalize_term(term.clone().into())
            .map_err(|_| ())
    }

    fn build_internalize_expression_term(
        &mut self,
    ) -> impl Fn(ExpressionTerm) -> Option<D::InternalTerm> + 'a {
        let dataset = self.dataset;
        move |t| dataset.internalize_expression_term(t).ok()
    }

    fn build_externalize_expression_term(
        &mut self,
    ) -> impl Fn(D::InternalTerm) -> Option<ExpressionTerm> + 'a {
        let dataset = self.dataset;
        move |t| dataset.externalize_expression_term(t).ok()
    }

    fn build_externalize_written_term(
        &mut self,
    ) -> impl Fn(D::InternalTerm) -> Option<ExpressionTerm> + 'a {
        let dataset = self.dataset;
        move |t| {
            Some(match dataset.externalize_term(t).ok()? {
                Term::Literal(literal) => ExpressionTerm::from_literal_as_written(literal),
                term => term.into(),
            })
        }
    }

    fn now(&mut self) -> DateTime {
        crate::eval::local_now()
    }

    fn base_iri(&mut self) -> Option<Arc<Iri<String>>> {
        None
    }

    fn custom_functions(&mut self) -> &CustomFunctionRegistry {
        &NO_FUNCTIONS
    }
}

/// tripletrail (#11): `expression` compiled as the evaluator compiles it over `dataset`, as a
/// function of the values of `variables` (`None` unbound), to the term it gives (`None` an
/// error). The caller leaves out what depends on more than the values: `NOW`, `RAND`, `UUID`,
/// `STRUUID`, `BNODE`, `EXISTS`, a relative `IRI` and custom functions (there are none here).
/// `None` if it does not compile.
pub fn expression_kernel<'a, D: QueryableDataset<'a>>(
    dataset: &'a D,
    expression: &spargebra::algebra::Expression,
    variables: &[Variable],
) -> Option<impl Fn(&Vec<Option<D::InternalTerm>>) -> Option<Term> + 'a> {
    let expression = sparopt::algebra::Expression::from_sparql(expression);
    let eval = build_expression_evaluator(&expression, &mut Context { dataset, variables }).ok()?;
    Some(move |tuple: &Vec<Option<D::InternalTerm>>| eval(tuple).map(Term::from))
}
