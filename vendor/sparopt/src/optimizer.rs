use crate::algebra::{
    Expression, Function, GraphPattern, JoinAlgorithm, LeftJoinAlgorithm, MinusAlgorithm, OrderExpression,
};
use crate::type_inference::{
    VariableType, VariableTypes, infer_expression_type, infer_graph_pattern_types,
};
use oxrdf::Variable;
use oxrdf::vocab::rdf;
use spargebra::algebra::PropertyPathExpression;
use spargebra::term::{GroundTermPattern, NamedNodePattern};
use std::cmp::{max, min};

pub struct Optimizer;

impl Optimizer {
    pub fn optimize_graph_pattern(pattern: GraphPattern) -> GraphPattern {
        let pattern = Self::normalize_pattern(pattern, &VariableTypes::default());
        let pattern = Self::reorder_joins(pattern, &VariableTypes::default());
        let pattern = Self::push_filters(pattern, Vec::new(), &VariableTypes::default());
        Self::plan_exists(pattern, &VariableTypes::default())
    }

    /// tripletrail: plans the patterns of the `EXISTS` (and `NOT EXISTS`) of filters, BINDs and
    /// OPTIONAL conditions, which were left as written: each is evaluated for every row,
    /// with the row's variables bound, as the right side of a LATERAL join is. Written as
    /// `?t a ex:TenT ; ex:loc ?l . ... $this ex:loc ?l2 ...` it would scan every `ex:TenT`
    /// for every row instead of starting from `$this`.
    fn plan_exists(pattern: GraphPattern, input_types: &VariableTypes) -> GraphPattern {
        fn in_expression(expression: &mut Expression, types: &VariableTypes) {
            match expression {
                Expression::Exists(inner) => {
                    let pattern = std::mem::replace(&mut **inner, GraphPattern::empty());
                    let pattern = Optimizer::reorder_joins(pattern, types);
                    let pattern = Optimizer::push_filters(pattern, Vec::new(), types);
                    **inner = Optimizer::plan_exists(pattern, types);
                }
                Expression::Or(inner)
                | Expression::And(inner)
                | Expression::Coalesce(inner)
                | Expression::FunctionCall(_, inner) => {
                    for e in inner {
                        in_expression(e, types);
                    }
                }
                Expression::Equal(a, b)
                | Expression::SameTerm(a, b)
                | Expression::Greater(a, b)
                | Expression::GreaterOrEqual(a, b)
                | Expression::Less(a, b)
                | Expression::LessOrEqual(a, b)
                | Expression::Add(a, b)
                | Expression::Subtract(a, b)
                | Expression::Multiply(a, b)
                | Expression::Divide(a, b) => {
                    in_expression(a, types);
                    in_expression(b, types);
                }
                Expression::UnaryPlus(a) | Expression::UnaryMinus(a) | Expression::Not(a) => {
                    in_expression(a, types)
                }
                Expression::If(a, b, c) => {
                    in_expression(a, types);
                    in_expression(b, types);
                    in_expression(c, types);
                }
                Expression::NamedNode(_)
                | Expression::Literal(_)
                | Expression::Variable(_)
                | Expression::Bound(_) => {}
            }
        }
        let rec =
            |p: Box<GraphPattern>, types: &VariableTypes| Box::new(Self::plan_exists(*p, types));
        match pattern {
            GraphPattern::Filter {
                inner,
                mut expression,
            } => {
                let inner = rec(inner, input_types);
                in_expression(
                    &mut expression,
                    &infer_graph_pattern_types(&inner, input_types.clone()),
                );
                GraphPattern::Filter { inner, expression }
            }
            GraphPattern::Extend {
                inner,
                variable,
                mut expression,
            } => {
                let inner = rec(inner, input_types);
                in_expression(
                    &mut expression,
                    &infer_graph_pattern_types(&inner, input_types.clone()),
                );
                GraphPattern::Extend {
                    inner,
                    variable,
                    expression,
                }
            }
            GraphPattern::LeftJoin {
                left,
                right,
                mut expression,
                algorithm,
            } => {
                let left = rec(left, input_types);
                let right = rec(right, input_types);
                let mut types = infer_graph_pattern_types(&left, input_types.clone());
                types.intersect_with(infer_graph_pattern_types(&right, input_types.clone()));
                in_expression(&mut expression, &types);
                GraphPattern::LeftJoin {
                    left,
                    right,
                    expression,
                    algorithm,
                }
            }
            #[cfg(feature = "sep-0006")]
            GraphPattern::Lateral { left, right } => {
                // tripletrail: along the chain from its start, keeping the types of the part
                // done so far, instead of inferring the left side's types at every level (#509).
                let (first, rights) = lateral_spine(GraphPattern::Lateral { left, right });
                let mut done = Self::plan_exists(first, input_types);
                let mut types = infer_graph_pattern_types(&done, input_types.clone());
                for right in rights {
                    let right = Self::plan_exists(right, &types);
                    types = infer_graph_pattern_types(&right, types);
                    done = GraphPattern::Lateral {
                        left: Box::new(done),
                        right: Box::new(right),
                    };
                }
                done
            }
            GraphPattern::Join {
                left,
                right,
                algorithm,
            } => GraphPattern::Join {
                left: rec(left, input_types),
                right: rec(right, input_types),
                algorithm,
            },
            GraphPattern::Minus {
                left,
                right,
                algorithm,
                graph_variable,
            } => GraphPattern::Minus {
                left: rec(left, input_types),
                right: rec(right, input_types),
                algorithm,
                graph_variable,
            },
            GraphPattern::Union { inner } => GraphPattern::Union {
                inner: inner
                    .into_iter()
                    .map(|p| Self::plan_exists(p, input_types))
                    .collect(),
            },
            GraphPattern::OrderBy { inner, expression } => GraphPattern::OrderBy {
                inner: rec(inner, input_types),
                expression,
            },
            GraphPattern::Project { inner, variables } => GraphPattern::Project {
                inner: rec(inner, input_types),
                variables,
            },
            GraphPattern::Distinct { inner } => GraphPattern::Distinct {
                inner: rec(inner, input_types),
            },
            GraphPattern::Reduced { inner } => GraphPattern::Reduced {
                inner: rec(inner, input_types),
            },
            GraphPattern::Slice {
                inner,
                start,
                length,
            } => GraphPattern::Slice {
                inner: rec(inner, input_types),
                start,
                length,
            },
            GraphPattern::Group {
                inner,
                variables,
                aggregates,
            } => GraphPattern::Group {
                inner: rec(inner, input_types),
                variables,
                aggregates,
            },
            // Evaluated elsewhere, as written.
            other @ (GraphPattern::Service { .. }
            | GraphPattern::QuadPattern { .. }
            | GraphPattern::Path { .. }
            | GraphPattern::Graph { .. }
            | GraphPattern::Values { .. }) => other,
        }
    }

    /// Normalize the pattern, discarding any join ordering information
    fn normalize_pattern(pattern: GraphPattern, input_types: &VariableTypes) -> GraphPattern {
        match pattern {
            GraphPattern::QuadPattern {
                subject,
                predicate,
                object,
                graph_name,
            } => GraphPattern::QuadPattern {
                subject,
                predicate,
                object,
                graph_name,
            },
            GraphPattern::Path {
                subject,
                path,
                object,
                graph_name,
            } => GraphPattern::Path {
                subject,
                path,
                object,
                graph_name,
            },
            GraphPattern::Graph { graph_name } => GraphPattern::Graph { graph_name },
            GraphPattern::Join {
                left,
                right,
                algorithm,
            } => GraphPattern::join(
                Self::normalize_pattern(*left, input_types),
                Self::normalize_pattern(*right, input_types),
                algorithm,
            ),
            GraphPattern::LeftJoin {
                left,
                right,
                expression,
                algorithm,
            } => {
                let left = Self::normalize_pattern(*left, input_types);
                let right = Self::normalize_pattern(*right, input_types);
                let mut inner_types = infer_graph_pattern_types(&left, input_types.clone());
                inner_types.intersect_with(infer_graph_pattern_types(&right, input_types.clone()));
                GraphPattern::left_join(
                    left,
                    right,
                    Self::normalize_expression(expression, &inner_types),
                    algorithm,
                )
            }
            #[cfg(feature = "sep-0006")]
            GraphPattern::Lateral { left, right } => {
                let left = Self::normalize_pattern(*left, input_types);
                let left_types = infer_graph_pattern_types(&left, input_types.clone());
                let right = Self::normalize_pattern(*right, &left_types);
                GraphPattern::lateral(left, right)
            }
            GraphPattern::Filter { inner, expression } => {
                let inner = Self::normalize_pattern(*inner, input_types);
                let inner_types = infer_graph_pattern_types(&inner, input_types.clone());
                let expression = Self::normalize_expression(expression, &inner_types);
                let expression_type = infer_expression_type(&expression, &inner_types);
                if expression_type == VariableType::UNDEF {
                    GraphPattern::empty()
                } else {
                    GraphPattern::filter(inner, expression)
                }
            }
            GraphPattern::Union { inner } => GraphPattern::union_all(
                inner
                    .into_iter()
                    .map(|e| Self::normalize_pattern(e, input_types)),
            ),
            GraphPattern::Extend {
                inner,
                variable,
                expression,
            } => {
                let inner = Self::normalize_pattern(*inner, input_types);
                let inner_types = infer_graph_pattern_types(&inner, input_types.clone());
                let expression = Self::normalize_expression(expression, &inner_types);
                let expression_type = infer_expression_type(&expression, &inner_types);
                if expression_type == VariableType::UNDEF {
                    // TODO: valid?
                    inner
                } else {
                    GraphPattern::extend(inner, variable, expression)
                }
            }
            GraphPattern::Minus {
                left,
                right,
                algorithm,
                graph_variable,
            } => GraphPattern::minus(
                Self::normalize_pattern(*left, input_types),
                Self::normalize_pattern(*right, input_types),
                algorithm,
                graph_variable,
            ),
            GraphPattern::Values {
                variables,
                bindings,
            } => GraphPattern::values(variables, bindings),
            GraphPattern::OrderBy { inner, expression } => {
                let inner = Self::normalize_pattern(*inner, input_types);
                let inner_types = infer_graph_pattern_types(&inner, input_types.clone());
                GraphPattern::order_by(
                    inner,
                    expression
                        .into_iter()
                        .map(|e| match e {
                            OrderExpression::Asc(e) => {
                                OrderExpression::Asc(Self::normalize_expression(e, &inner_types))
                            }
                            OrderExpression::Desc(e) => {
                                OrderExpression::Desc(Self::normalize_expression(e, &inner_types))
                            }
                        })
                        .collect(),
                )
            }
            GraphPattern::Project { inner, variables } => {
                GraphPattern::project(Self::normalize_pattern(*inner, input_types), variables)
            }
            GraphPattern::Distinct { inner } => {
                GraphPattern::distinct(Self::normalize_pattern(*inner, input_types))
            }
            GraphPattern::Reduced { inner } => {
                GraphPattern::reduced(Self::normalize_pattern(*inner, input_types))
            }
            GraphPattern::Slice {
                inner,
                start,
                length,
            } => GraphPattern::slice(Self::normalize_pattern(*inner, input_types), start, length),
            GraphPattern::Group {
                inner,
                variables,
                aggregates,
            } => {
                // TODO: min, max and sample don't care about DISTINCT
                GraphPattern::group(
                    Self::normalize_pattern(*inner, input_types),
                    variables,
                    aggregates,
                )
            }
            GraphPattern::Service { .. } => {
                // We leave this problem to the remote SPARQL endpoint
                pattern
            }
        }
    }

    fn normalize_expression(expression: Expression, types: &VariableTypes) -> Expression {
        match expression {
            Expression::NamedNode(node) => node.into(),
            Expression::Literal(literal) => literal.into(),
            Expression::Variable(variable) => variable.into(),
            Expression::Or(inner) => Expression::or_all(
                inner
                    .into_iter()
                    .map(|e| Self::normalize_expression(e, types)),
            ),
            Expression::And(inner) => Expression::and_all(
                inner
                    .into_iter()
                    .map(|e| Self::normalize_expression(e, types)),
            ),
            Expression::Equal(left, right) => {
                let left = Self::normalize_expression(*left, types);
                let left_types = infer_expression_type(&left, types);
                let right = Self::normalize_expression(*right, types);
                let right_types = infer_expression_type(&right, types);
                #[allow(unused_mut, clippy::allow_attributes)]
                let mut must_use_equal = left_types.literal && right_types.literal;
                #[cfg(feature = "sparql-12")]
                {
                    must_use_equal = must_use_equal || left_types.triple && right_types.triple;
                }
                if must_use_equal {
                    Expression::equal(left, right)
                } else {
                    Expression::same_term(left, right)
                }
            }
            Expression::SameTerm(left, right) => Expression::same_term(
                Self::normalize_expression(*left, types),
                Self::normalize_expression(*right, types),
            ),
            Expression::Greater(left, right) => Expression::greater(
                Self::normalize_expression(*left, types),
                Self::normalize_expression(*right, types),
            ),
            Expression::GreaterOrEqual(left, right) => Expression::greater_or_equal(
                Self::normalize_expression(*left, types),
                Self::normalize_expression(*right, types),
            ),
            Expression::Less(left, right) => Expression::less(
                Self::normalize_expression(*left, types),
                Self::normalize_expression(*right, types),
            ),
            Expression::LessOrEqual(left, right) => Expression::less_or_equal(
                Self::normalize_expression(*left, types),
                Self::normalize_expression(*right, types),
            ),
            Expression::Add(left, right) => {
                Self::normalize_expression(*left, types) + Self::normalize_expression(*right, types)
            }
            Expression::Subtract(left, right) => {
                Self::normalize_expression(*left, types) - Self::normalize_expression(*right, types)
            }
            Expression::Multiply(left, right) => {
                Self::normalize_expression(*left, types) * Self::normalize_expression(*right, types)
            }
            Expression::Divide(left, right) => {
                Self::normalize_expression(*left, types) / Self::normalize_expression(*right, types)
            }
            Expression::UnaryPlus(inner) => {
                Expression::unary_plus(Self::normalize_expression(*inner, types))
            }
            Expression::UnaryMinus(inner) => -Self::normalize_expression(*inner, types),
            Expression::Not(inner) => !Self::normalize_expression(*inner, types),
            Expression::Exists(inner) => Expression::exists(Self::normalize_pattern(*inner, types)),
            Expression::Bound(variable) => {
                let t = types.get(&variable);
                if !t.undef {
                    true.into()
                } else if t == VariableType::UNDEF {
                    false.into()
                } else {
                    Expression::Bound(variable)
                }
            }
            Expression::If(cond, then, els) => Expression::if_cond(
                Self::normalize_expression(*cond, types),
                Self::normalize_expression(*then, types),
                Self::normalize_expression(*els, types),
            ),
            Expression::Coalesce(inners) => Expression::coalesce(
                inners
                    .into_iter()
                    .map(|e| Self::normalize_expression(e, types))
                    .collect(),
            ),
            Expression::FunctionCall(name, args) => Expression::call(
                name,
                args.into_iter()
                    .map(|e| Self::normalize_expression(e, types))
                    .collect(),
            ),
        }
    }

    fn push_filters(
        pattern: GraphPattern,
        mut filters: Vec<Expression>,
        input_types: &VariableTypes,
    ) -> GraphPattern {
        match pattern {
            GraphPattern::QuadPattern { .. }
            | GraphPattern::Path { .. }
            | GraphPattern::Graph { .. }
            | GraphPattern::Values { .. } => {
                GraphPattern::filter(pattern, Expression::and_all(filters))
            }
            GraphPattern::Join {
                left,
                right,
                algorithm,
            } => {
                // tripletrail: with no filter to place, skip the type inference. On a long
                // join chain it ran at every level, which made planning quadratic (#483).
                if filters.is_empty() {
                    return GraphPattern::join(
                        Self::push_filters(*left, Vec::new(), input_types),
                        Self::push_filters(*right, Vec::new(), input_types),
                        algorithm,
                    );
                }
                let left_types = infer_graph_pattern_types(&left, input_types.clone());
                let right_types = infer_graph_pattern_types(&right, input_types.clone());
                let mut left_filters = Vec::new();
                let mut right_filters = Vec::new();
                let mut final_filters = Vec::new();
                for filter in filters {
                    let push_left = are_all_expression_variables_bound(&filter, &left_types);
                    let push_right = are_all_expression_variables_bound(&filter, &right_types);
                    if push_left {
                        if push_right {
                            left_filters.push(filter.clone());
                            right_filters.push(filter);
                        } else {
                            left_filters.push(filter);
                        }
                    } else if push_right {
                        right_filters.push(filter);
                    } else {
                        final_filters.push(filter);
                    }
                }
                // tripletrail: a join on no key whose filter equates a variable of one side with
                // a value computed from the other (`FILTER(?oid = STRAFTER(STR(?object), ...))`)
                // computes that value on the other side, and joins on it (#531).
                let mut keyed_left = *left;
                let mut keyed_right = *right;
                let mut keyed_algorithm = algorithm;
                let JoinAlgorithm::HashBuildLeftProbeRight { keys } = &keyed_algorithm;
                if keys.is_empty() {
                    if let Some(i) = final_filters.iter().position(|f| {
                        equality_key(f, &left_types, &right_types).is_some()
                            || equality_key(f, &right_types, &left_types).is_some()
                    }) {
                        let filter = final_filters.remove(i);
                        if let Some((variable, value)) =
                            equality_key(&filter, &left_types, &right_types)
                        {
                            keyed_right = computed_key(keyed_right, variable.clone(), value);
                            keyed_algorithm = JoinAlgorithm::HashBuildLeftProbeRight {
                                keys: vec![variable],
                            };
                        } else if let Some((variable, value)) =
                            equality_key(&filter, &right_types, &left_types)
                        {
                            keyed_left = computed_key(keyed_left, variable.clone(), value);
                            keyed_algorithm = JoinAlgorithm::HashBuildLeftProbeRight {
                                keys: vec![variable],
                            };
                        }
                    }
                }
                GraphPattern::filter(
                    GraphPattern::join(
                        Self::push_filters(keyed_left, left_filters, input_types),
                        Self::push_filters(keyed_right, right_filters, input_types),
                        keyed_algorithm,
                    ),
                    Expression::and_all(final_filters),
                )
            }
            #[cfg(feature = "sep-0006")]
            GraphPattern::Lateral { left, right } => {
                // tripletrail: walks the chain `LATERAL(LATERAL(LATERAL(p0, r1), r2), ...)` from
                // its start, keeping the types of the part done so far, instead of recursing
                // into the left side and inferring its types again at every level: a chain of
                // 8,000 triple patterns took 25 s and 2.7 GB to plan (#509). Each filter
                // goes where the recursion put it: into `p0` when `p0` binds its variables,
                // else into the first `ri` after which they are bound (or the last `ri`).
                let (first, rights) = lateral_spine(GraphPattern::Lateral { left, right });
                let mut types = infer_graph_pattern_types(&first, input_types.clone());
                let (first_filters, mut pending): (Vec<_>, Vec<_>) = filters
                    .into_iter()
                    .partition(|f| are_all_expression_variables_bound(f, &types));
                let mut done = Self::push_filters(first, first_filters, input_types);
                let last = rights.len() - 1;
                for (i, right) in rights.into_iter().enumerate() {
                    let right = if pending.is_empty() {
                        let right = Self::push_filters(right, Vec::new(), &types);
                        types = infer_graph_pattern_types(&right, types);
                        right
                    } else {
                        let next_types = infer_graph_pattern_types(&right, types.clone());
                        // tripletrail: `?v = f(...)`, with `?v` bound by this right side and
                        // `f` reading the part done: `?v` is computed on the part done, and
                        // looked up in this right side (#531).
                        if let Some(at) = pending.iter().position(|f| {
                            equality_key(f, &next_types, &types)
                                .is_some_and(|(v, _)| matched_only(&right, &v))
                        }) {
                            let filter = pending.remove(at);
                            if let Some((variable, value)) =
                                equality_key(&filter, &next_types, &types)
                            {
                                types.set(variable.clone(), next_types.get(&variable));
                                done = computed_key(done, variable, value);
                            }
                        }
                        let right_filters = if i == last {
                            std::mem::take(&mut pending)
                        } else {
                            let (now, later) = pending.into_iter().partition(|f| {
                                are_all_expression_variables_bound(f, &next_types)
                            });
                            pending = later;
                            now
                        };
                        let right = Self::push_filters(right, right_filters, &types);
                        types = next_types;
                        right
                    };
                    done = if let GraphPattern::Filter {
                        inner: inner_right,
                        expression,
                    } = right
                    {
                        // We prefer to have filter out of the lateral rather than inside the right part
                        GraphPattern::filter(GraphPattern::lateral(done, *inner_right), expression)
                    } else {
                        GraphPattern::lateral(done, right)
                    };
                }
                done
            }
            GraphPattern::LeftJoin {
                left,
                right,
                expression,
                algorithm,
            } => {
                let left_types = infer_graph_pattern_types(&left, input_types.clone());
                let right_types = infer_graph_pattern_types(&right, input_types.clone());
                let mut left_filters = Vec::new();
                let mut right_filters = Vec::new();
                let mut final_filters = Vec::new();
                for filter in filters {
                    let push_left = are_all_expression_variables_bound(&filter, &left_types);
                    if push_left {
                        left_filters.push(filter);
                    } else {
                        final_filters.push(filter);
                    }
                }
                // tripletrail: each part of a conjunction on its own (#485): in
                // `OPTIONAL { ... FILTER(?a = f(?b)) FILTER(?class = :C) }` the first only
                // reads the right side and goes into it, before its parts are joined, though
                // the second reads the left side and stays in the left join.
                let conjuncts = match expression {
                    Expression::And(all) => all,
                    e => vec![e],
                };
                let mut kept = Vec::new();
                for expression in conjuncts {
                    if expression.effective_boolean_value().is_none()
                        && (are_all_expression_variables_bound(&expression, &right_types)
                            || are_no_expression_variables_bound(&expression, &left_types))
                    {
                        right_filters.push(expression);
                    } else {
                        kept.push(expression);
                    }
                }
                let expression = Expression::and_all(kept);
                GraphPattern::filter(
                    GraphPattern::left_join(
                        Self::push_filters(*left, left_filters, input_types),
                        Self::push_filters(*right, right_filters, input_types),
                        expression,
                        algorithm,
                    ),
                    Expression::and_all(final_filters),
                )
            }
            GraphPattern::Minus {
                left,
                right,
                algorithm,
                graph_variable,
            } => GraphPattern::minus(
                Self::push_filters(*left, filters, input_types),
                Self::push_filters(*right, Vec::new(), input_types),
                algorithm,
                graph_variable,
            ),
            GraphPattern::Extend {
                inner,
                expression,
                variable,
            } => {
                // TODO: handle the case where the filter overrides an expression variable (should not happen in SPARQL but allowed in the algebra)
                let mut inner_filters = Vec::new();
                let mut final_filters = Vec::new();
                for filter in filters {
                    let extend_variable_used =
                        filter.used_variables().into_iter().any(|v| *v == variable);
                    if extend_variable_used {
                        final_filters.push(filter);
                    } else {
                        inner_filters.push(filter);
                    }
                }
                GraphPattern::filter(
                    GraphPattern::extend(
                        Self::push_filters(*inner, inner_filters, input_types),
                        variable,
                        expression,
                    ),
                    Expression::and_all(final_filters),
                )
            }
            GraphPattern::Filter { inner, expression } => {
                if let Expression::And(expressions) = expression {
                    filters.extend(expressions)
                } else {
                    filters.push(expression)
                }
                Self::push_filters(*inner, filters, input_types)
            }
            GraphPattern::Union { inner } => GraphPattern::union_all(
                inner
                    .into_iter()
                    .map(|c| Self::push_filters(c, filters.clone(), input_types)),
            ),
            GraphPattern::Slice {
                inner,
                start,
                length,
            } => GraphPattern::filter(
                GraphPattern::slice(
                    Self::push_filters(*inner, Vec::new(), input_types),
                    start,
                    length,
                ),
                Expression::and_all(filters),
            ),
            GraphPattern::Distinct { inner } => {
                GraphPattern::distinct(Self::push_filters(*inner, filters, input_types))
            }
            GraphPattern::Reduced { inner } => {
                GraphPattern::reduced(Self::push_filters(*inner, filters, input_types))
            }
            GraphPattern::Project { inner, variables } => {
                let mut inner_filters = Vec::new();
                let mut final_filters = Vec::new();
                for filter in filters {
                    if filter
                        .used_variables()
                        .into_iter()
                        .all(|variable| variables.contains(variable))
                    {
                        inner_filters.push(filter);
                    } else {
                        // Variables not projected by a subquery are not in scope outside of it.
                        // Pushing such a filter would make the subquery's hidden bindings visible
                        // to the filter, in particular for correlated EXISTS expressions.
                        final_filters.push(filter);
                    }
                }
                GraphPattern::filter(
                    GraphPattern::project(
                        Self::push_filters(*inner, inner_filters, input_types),
                        variables,
                    ),
                    Expression::and_all(final_filters),
                )
            }
            GraphPattern::OrderBy { inner, expression } => {
                GraphPattern::order_by(Self::push_filters(*inner, filters, input_types), expression)
            }
            GraphPattern::Service { .. } => {
                // TODO: we can be smart and push some filters
                // But we need to check the behavior of SILENT that can transform no results into a singleton
                GraphPattern::filter(pattern, Expression::and_all(filters))
            }
            GraphPattern::Group {
                inner,
                variables,
                aggregates,
            } => GraphPattern::filter(
                GraphPattern::group(
                    Self::push_filters(*inner, Vec::new(), input_types),
                    variables,
                    aggregates,
                ),
                Expression::and_all(filters),
            ),
        }
    }

    /// tripletrail: joins the parts of a join that share no variable bound in all their rows.
    /// They are not always unrelated: a variable a BIND or an aggregate binds (in most rows)
    /// on one side can be the subject of triple patterns on the other, as in
    /// `{ SELECT ?x (SAMPLE(?y) AS ?s) ... } ?s ex:p ?o`. Joining them by a hash join on no
    /// key evaluates the triple patterns for every subject, and pairs every row with every
    /// row. Instead, the parts that can be evaluated per row with such a variable bound are
    /// planned again with it bound and joined as LATERAL joins after the parts binding it.
    /// Joins are commutative: the result is the same whatever the order.
    #[cfg(feature = "sep-0006")]
    fn join_components(
        components: Vec<GraphPattern>,
        originals: Vec<Vec<GraphPattern>>,
        input_types: &VariableTypes,
    ) -> GraphPattern {
        let types: Vec<VariableTypes> = components
            .iter()
            .map(|p| infer_graph_pattern_types(p, input_types.clone()))
            .collect();
        let linked = |a: &VariableTypes, b: &VariableTypes| {
            a.iter().any(|(variable, t)| {
                *t != VariableType::UNDEF
                    && b.get(variable) != VariableType::UNDEF
                    && input_types.get(variable).undef
            })
        };
        // tripletrail: whether `driver` binds a variable `driven` always binds: per row of
        // `driver`, `driven` is then a lookup on it. Not the other way round: a part that
        // binds the variable only in some rows (`BIND(IRI(...) AS ?c)`, an aggregate) cannot
        // be driven by it, its BIND would clash with the bound value (issue #374).
        let drives = |driver: &VariableTypes, driven: &VariableTypes| {
            driver.iter().any(|(variable, t)| {
                *t != VariableType::UNDEF
                    && {
                        let d = driven.get(variable);
                        d != VariableType::UNDEF && !d.undef
                    }
                    && input_types.get(variable).undef
            })
        };
        // A part some other part can drive, row by row.
        let dependent: Vec<bool> = (0..components.len())
            .map(|i| {
                is_fit_for_for_loop_join(&components[i], input_types, input_types)
                    && (0..components.len()).any(|j| j != i && drives(&types[j], &types[i]))
            })
            .collect();
        let mut remaining: Vec<usize> = (0..components.len()).collect();
        let mut output: Option<(GraphPattern, VariableTypes)> = None;
        // tripletrail: whether what is joined so far is one row at most (`ex:p1 ex:num ?n`, from
        // estimates): the other parts are then joined as LATERAL right sides, each evaluated
        // once, as by a hash join on no key, but with the row's variables bound, so the
        // FILTERs that compare them with the other part's (`?m < ?n + 120`) are pushed into it,
        // right after the triple pattern that binds theirs (issue #211).
        let mut one_row = false;
        // tripletrail: the estimated size and the inferred types of `output`, kept up to date as
        // it grows (#483): estimating it again for each candidate of each step walked the whole
        // chain, and inferred its types again at each LATERAL level, n⁴ steps for n parts that
        // share no variable (250 triple patterns: 27 s). The same formulas, so the same plans.
        let mut output_size = 0_usize;
        let mut output_inferred = VariableTypes::default();
        while !remaining.is_empty() {
            // tripletrail: past the timeout, the rest in the order it comes (#483).
            if crate::estimate::given_up() {
                let mut joined = output.map(|(p, _)| p);
                for i in remaining {
                    let next = components[i].clone();
                    joined = Some(match joined {
                        None => next,
                        Some(current) => GraphPattern::join(
                            current,
                            next,
                            JoinAlgorithm::HashBuildLeftProbeRight { keys: Vec::new() },
                        ),
                    });
                }
                return joined.unwrap();
            }
            // A part to evaluate per row of what is joined so far.
            if let Some((_, current_types)) = &output {
                let lateral = remaining
                    .iter()
                    .enumerate()
                    .filter(|(_, i)| one_row || linked(current_types, &types[**i]))
                    .filter_map(|(position, i)| {
                        // Planned as if the linking variables were always bound: they are in
                        // most rows (a row where one is not still gets the right result).
                        let mut driven = current_types.clone();
                        driven.assume_bound(|v| {
                            types[*i].get(v) != VariableType::UNDEF && input_types.get(v).undef
                        });
                        let replanned = Self::reorder_joins(
                            originals[*i]
                                .iter()
                                .cloned()
                                .reduce(|l, r| {
                                    GraphPattern::join(
                                        l,
                                        r,
                                        JoinAlgorithm::HashBuildLeftProbeRight { keys: Vec::new() },
                                    )
                                })
                                .unwrap(),
                            &driven,
                        );
                        is_fit_for_for_loop_join(&replanned, input_types, current_types).then(
                            || {
                                // As `estimate_lateral_cost` of `current` and `replanned`.
                                let cost = output_size.saturating_mul(
                                    estimate_graph_pattern_size(&replanned, current_types),
                                );
                                (position, replanned, cost)
                            },
                        )
                    })
                    .min_by_key(|(_, _, cost)| *cost);
                if let Some((position, replanned, _)) = lateral {
                    let i = remaining.remove(position);
                    let (current, mut current_types) = output.take().unwrap();
                    one_row =
                        one_row && estimate_graph_pattern_size(&replanned, &current_types) <= 1;
                    // As `estimate_graph_pattern_size` and `infer_graph_pattern_types` of the
                    // LATERAL join.
                    output_size = output_size
                        .saturating_mul(estimate_graph_pattern_size(&replanned, &output_inferred));
                    output_inferred = infer_graph_pattern_types(&replanned, output_inferred);
                    current_types.intersect_with(types[i].clone());
                    output = Some((GraphPattern::lateral(current, replanned), current_types));
                    continue;
                }
            }
            // Else a cartesian product with a part no other part drives, the smallest first.
            let position = (0..remaining.len())
                .min_by_key(|p| {
                    (
                        dependent[remaining[*p]],
                        estimate_graph_pattern_size(&components[remaining[*p]], input_types),
                    )
                })
                .unwrap();
            let i = remaining.remove(position);
            let next = components[i].clone();
            let size = estimate_graph_pattern_size(&next, input_types);
            one_row = crate::estimate::active() && size <= 1 && (output.is_none() || one_row);
            let next_inferred = infer_graph_pattern_types(&next, input_types.clone());
            output = Some(match output.take() {
                None => {
                    output_size = size;
                    output_inferred = next_inferred;
                    (next, types[i].clone())
                }
                Some((current, mut current_types)) => {
                    let keys = join_key_variables(&current_types, &types[i], input_types);
                    let current_size = output_size;
                    // As `estimate_graph_pattern_size` and `infer_graph_pattern_types` of the
                    // join.
                    output_size = current_size
                        .saturating_mul(size)
                        .saturating_div(1_000_usize.saturating_pow(keys.len().try_into().unwrap()));
                    output_inferred.intersect_with(next_inferred);
                    let joined = if current_size <= size {
                        GraphPattern::join(
                            current,
                            next,
                            JoinAlgorithm::HashBuildLeftProbeRight { keys },
                        )
                    } else {
                        GraphPattern::join(
                            next,
                            current,
                            JoinAlgorithm::HashBuildLeftProbeRight { keys },
                        )
                    };
                    current_types.intersect_with(types[i].clone());
                    (joined, current_types)
                }
            });
        }
        output.unwrap().0
    }

    fn reorder_joins(pattern: GraphPattern, input_types: &VariableTypes) -> GraphPattern {
        match pattern {
            GraphPattern::QuadPattern { .. }
            | GraphPattern::Path { .. }
            | GraphPattern::Values { .. }
            | GraphPattern::Graph { .. } => pattern,
            GraphPattern::Join { left, right, .. } => {
                // We flatten the join operation
                let mut to_reorder = Vec::new();
                // tripletrail: the operands as written, to plan again when one becomes the right
                // side of a LATERAL join (see below).
                #[cfg(feature = "sep-0006")]
                let mut originals = Vec::new();
                let mut todo = vec![*right, *left];
                while let Some(e) = todo.pop() {
                    if let GraphPattern::Join { left, right, .. } = e {
                        todo.push(*right);
                        todo.push(*left);
                    } else {
                        #[cfg(feature = "sep-0006")]
                        originals.push(e.clone());
                        to_reorder.push(Self::reorder_joins(e, input_types));
                    }
                }

                // We do first type inference
                let to_reorder_types = to_reorder
                    .iter()
                    .map(|p| infer_graph_pattern_types(p, input_types.clone()))
                    .collect::<Vec<_>>();
                // tripletrail: the variables each operand binds by a BIND or an aggregate, and its
                // types with those taken as bound: such a variable is bound in all rows but
                // those where its expression fails, so it can drive a lookup on the other
                // side (#484, #486). It is no hash key (a hash join needs it bound in every
                // row), so without this, `BIND(IRI(...) AS ?seq) ?l :seq ?seq ; :m ?m` was a
                // hash join on `?m` alone, or on no key.
                let to_reorder_binds = to_reorder.iter().map(bound_by_expressions).collect::<Vec<_>>();
                let to_reorder_driving = to_reorder_types
                    .iter()
                    .zip(&to_reorder_binds)
                    .map(|(t, b)| driving_types(t, b, input_types))
                    .collect::<Vec<_>>();

                // We do greedy join reordering
                let mut output_cartesian_product_joins = Vec::new();
                // tripletrail: the operands each part of the cartesian product is made of.
                #[cfg(feature = "sep-0006")]
                let mut component_ids: Vec<Vec<usize>> = Vec::new();
                let mut not_yet_reordered_ids = vec![true; to_reorder.len()];
                // tripletrail: the operands each operand can drive as a lookup, found once: the
                // choice of each component's start scanned every pair again, n³ steps for n
                // triple patterns sharing no variable (#483).
                #[cfg(feature = "sep-0006")]
                let neighbors: Vec<Vec<usize>> = if crate::estimate::active() {
                    (0..to_reorder.len())
                        .map(|i| {
                            if crate::estimate::given_up() {
                                return Vec::new();
                            }
                            (0..to_reorder.len())
                                .filter(|j| {
                                    *j != i
                                        && has_common_variables(
                                            &to_reorder_driving[i],
                                            &to_reorder_types[*j],
                                            input_types,
                                        )
                                        && is_fit_for_for_loop_join(
                                            &to_reorder[*j],
                                            input_types,
                                            &to_reorder_driving[i],
                                        )
                                })
                                .collect()
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                // We look for the next connected component to reorder and pick the smallest element
                // tripletrail: once the query's timeout passed, in the order they come (#483).
                while let Some(next_entry_id) = if crate::estimate::given_up() {
                    not_yet_reordered_ids.iter().position(|v| *v)
                } else {
                    not_yet_reordered_ids
                    .iter()
                    .enumerate()
                    .filter(|(_, v)| **v)
                    .map(|(i, _)| i)
                    .min_by_key(|i| {
                        // tripletrail: an operand planned as a chain of LATERAL joins only does
                        // lookups after its first step: start from the operand with the
                        // smallest first step, as the others are then joined with lookups too.
                        // Its whole size (a product of guesses) would rank a chain starting from
                        // 700 focus nodes after `?c rtm:refersTo ex:lps02` (every coordinate).
                        #[cfg(feature = "sep-0006")]
                        {
                            let entry = entry_size(&to_reorder[*i], input_types);
                            // tripletrail: with estimates from the data, also the size after the
                            // cheapest second step: one VALUES row (`?ps` = `ex:lps02`) whose
                            // lookup gives half the coordinates is no good start.
                            // Not once the timeout passed: n operands with n neighbours each
                            // are n² estimates (a star of 8,000 patterns, #509).
                            if crate::estimate::active() && !crate::estimate::given_up() {
                                let next = neighbors[*i]
                                    .iter()
                                    .copied()
                                    .filter(|j| not_yet_reordered_ids[*j])
                                    .map(|j| {
                                        entry.saturating_mul(estimate_graph_pattern_size(
                                            &to_reorder[j],
                                            &to_reorder_driving[*i],
                                        ))
                                    })
                                    .min();
                                next.map_or(entry, |n| n.max(entry))
                            } else {
                                entry
                            }
                        }
                        #[cfg(not(feature = "sep-0006"))]
                        {
                            estimate_graph_pattern_size(&to_reorder[*i], input_types)
                        }
                    })
                } {
                    not_yet_reordered_ids[next_entry_id] = false; // It's now done
                    #[cfg(feature = "sep-0006")]
                    component_ids.push(vec![next_entry_id]);
                    let mut output = to_reorder[next_entry_id].clone();
                    let mut output_types = to_reorder_types[next_entry_id].clone();
                    // tripletrail: the estimated size of `output`, kept up to date as it grows.
                    // Estimating it again for every candidate of every step walked the whole
                    // chain each time (and inferred types again at each of its LATERAL
                    // levels): 900 triple patterns took minutes to plan (#483). The cost
                    // functions below are those of `estimate_lateral_cost` and
                    // `estimate_join_cost` with this size in place of the walk, so the plans
                    // are the same.
                    let mut output_size = estimate_graph_pattern_size(&output, input_types);
                    // tripletrail: and its inferred types, kept up to date the same way.
                    #[cfg(feature = "sep-0006")]
                    let mut output_inferred = infer_graph_pattern_types(&output, input_types.clone());
                    // tripletrail: what `output` binds by BIND or aggregate, and its types with
                    // those taken as bound, for lookups (see `to_reorder_driving`).
                    let mut output_binds = to_reorder_binds[next_entry_id].clone();
                    let mut output_driving = to_reorder_driving[next_entry_id].clone();
                    // We look for an other child to join with that does not blow up the join cost
                    while let Some(next_id) = if crate::estimate::given_up() {
                        None
                    } else {
                        not_yet_reordered_ids
                        .iter()
                        .enumerate()
                        .filter(|(_, v)| **v)
                        .map(|(i, _)| i)
                        .filter(|i| {
                            has_common_variables(&output_types, &to_reorder_types[*i], input_types)
                                // tripletrail: or linked by a BIND variable, as a lookup only.
                                || (cfg!(feature = "sep-0006")
                                    && has_common_variables(
                                        &output_driving,
                                        &to_reorder_types[*i],
                                        input_types,
                                    )
                                    && is_fit_for_for_loop_join(
                                        &to_reorder[*i],
                                        input_types,
                                        &output_driving,
                                    ))
                        })
                        .min_by_key(|i| {
                            // Estimation of the join cost
                            if cfg!(feature = "sep-0006")
                                && is_fit_for_for_loop_join(
                                    &to_reorder[*i],
                                    input_types,
                                    &output_driving,
                                )
                            {
                                output_size.saturating_mul(estimate_graph_pattern_size(
                                    &to_reorder[*i],
                                    &output_driving,
                                ))
                            } else {
                                join_cost(
                                    output_size,
                                    &to_reorder[*i],
                                    join_key_variables(
                                        &output_types,
                                        &to_reorder_types[*i],
                                        input_types,
                                    )
                                    .len(),
                                    input_types,
                                )
                            }
                        })
                    } {
                        not_yet_reordered_ids[next_id] = false; // It's now done
                        #[cfg(feature = "sep-0006")]
                        if let Some(ids) = component_ids.last_mut() {
                            ids.push(next_id);
                        }
                        let next = to_reorder[next_id].clone();
                        #[cfg(feature = "sep-0006")]
                        {
                            output = if is_fit_for_for_loop_join(
                                &next,
                                input_types,
                                &output_driving,
                            ) {
                                // tripletrail: the right side of a LATERAL join is evaluated once
                                // per left row, with the left row's variables bound. It was
                                // planned above without them: its joins would start from a
                                // full scan for every left row. Plan it again knowing them,
                                // as for an explicit LATERAL.
                                let replanned = Self::reorder_joins(
                                    originals[next_id].clone(),
                                    &output_driving,
                                );
                                let next = if is_fit_for_for_loop_join(
                                    &replanned,
                                    input_types,
                                    &output_driving,
                                ) {
                                    replanned
                                } else {
                                    next
                                };
                                // As `estimate_graph_pattern_size` of the LATERAL join.
                                output_size = output_size.saturating_mul(
                                    estimate_graph_pattern_size(
                                        &next,
                                        &driving_types(&output_inferred, &output_binds, input_types),
                                    ),
                                );
                                // As `infer_graph_pattern_types` of the LATERAL join.
                                output_inferred = infer_graph_pattern_types(&next, output_inferred);
                                GraphPattern::lateral(output, next)
                            } else {
                                let keys = join_key_variables(
                                    &output_types,
                                    &to_reorder_types[next_id],
                                    input_types,
                                );
                                output_size = join_cost(output_size, &next, keys.len(), input_types);
                                // As `infer_graph_pattern_types` of the join.
                                output_inferred
                                    .intersect_with(infer_graph_pattern_types(&next, input_types.clone()));
                                GraphPattern::join(
                                    output,
                                    next,
                                    JoinAlgorithm::HashBuildLeftProbeRight {
                                        keys: join_key_variables(
                                            &output_types,
                                            &to_reorder_types[next_id],
                                            input_types,
                                        ),
                                    },
                                )
                            };
                        }
                        #[cfg(not(feature = "sep-0006"))]
                        {
                            let keys = join_key_variables(
                                &output_types,
                                &to_reorder_types[next_id],
                                input_types,
                            );
                            output_size = join_cost(output_size, &next, keys.len(), input_types);
                            output = GraphPattern::join(
                                output,
                                next,
                                JoinAlgorithm::HashBuildLeftProbeRight { keys },
                            );
                        }
                        output_types.intersect_with(to_reorder_types[next_id].clone());
                        output_binds.extend(to_reorder_binds[next_id].iter().cloned());
                        output_driving = driving_types(&output_types, &output_binds, input_types);
                    }
                    output_cartesian_product_joins.push(output);
                }
                // tripletrail: with variables bound by the input (the right side of a LATERAL
                // join), parts linked only through them are no cartesian product: each is a
                // lookup on the bound values. Chain them as LATERAL joins, which keeps the
                // whole pattern fit for a LATERAL join.
                #[cfg(feature = "sep-0006")]
                let chain_bound = input_types.iter().any(|(_, t)| !t.undef)
                    && output_cartesian_product_joins
                        .iter()
                        .all(|p| is_fit_for_for_loop_join(p, input_types, input_types));
                #[cfg(feature = "sep-0006")]
                if !chain_bound
                    && output_cartesian_product_joins.len() > 1
                    && !crate::estimate::given_up()
                {
                    return Self::join_components(
                        output_cartesian_product_joins,
                        component_ids
                            .into_iter()
                            .map(|ids| ids.into_iter().map(|i| originals[i].clone()).collect())
                            .collect(),
                        input_types,
                    );
                }
                output_cartesian_product_joins
                    .into_iter()
                    .reduce(|left, right| {
                        #[cfg(feature = "sep-0006")]
                        if chain_bound {
                            return GraphPattern::lateral(left, right);
                        }
                        // tripletrail: past the timeout, without walking the join built so far
                        // (#483): the keys only speed the join up, and it is not run.
                        if crate::estimate::given_up() {
                            return GraphPattern::join(
                                left,
                                right,
                                JoinAlgorithm::HashBuildLeftProbeRight { keys: Vec::new() },
                            );
                        }
                        let keys = join_key_variables(
                            &infer_graph_pattern_types(&left, input_types.clone()),
                            &infer_graph_pattern_types(&right, input_types.clone()),
                            input_types,
                        );
                        if estimate_graph_pattern_size(&left, input_types)
                            <= estimate_graph_pattern_size(&right, input_types)
                        {
                            GraphPattern::join(
                                left,
                                right,
                                JoinAlgorithm::HashBuildLeftProbeRight { keys },
                            )
                        } else {
                            GraphPattern::join(
                                right,
                                left,
                                JoinAlgorithm::HashBuildLeftProbeRight { keys },
                            )
                        }
                    })
                    .unwrap()
            }
            #[cfg(feature = "sep-0006")]
            GraphPattern::Lateral { left, right } => {
                let left_types = infer_graph_pattern_types(&left, input_types.clone());
                GraphPattern::lateral(
                    Self::reorder_joins(*left, input_types),
                    Self::reorder_joins(*right, &left_types),
                )
            }
            GraphPattern::LeftJoin {
                left,
                right,
                expression,
                ..
            } => {
                let left = Self::reorder_joins(*left, input_types);
                let left_types = infer_graph_pattern_types(&left, input_types.clone());
                #[cfg(feature = "sep-0006")]
                {
                    let initial_right_types =
                        infer_graph_pattern_types(&right, input_types.clone());
                    // tripletrail: a variable the left side binds by BIND or aggregate can drive
                    // the lookup of the right side, though it is no hash key (#484): an
                    // `OPTIONAL { ?ne prov:wasDerivedFrom ... }` after `BIND(IRI(...) AS ?ne)`
                    // was a hash left join on no key, every left row with every right row.
                    let left_driving =
                        driving_types(&left_types, &bound_by_expressions(&left), input_types);
                    if has_common_variables(&left_driving, &initial_right_types, input_types) {
                        let lateral_cost = estimate_graph_pattern_size(&left, input_types)
                            .saturating_mul(estimate_graph_pattern_size(&right, &left_driving));
                        let keys =
                            join_key_variables(&left_types, &initial_right_types, input_types);
                        let join_cost = estimate_graph_pattern_size(&left, input_types)
                            .saturating_mul(estimate_graph_pattern_size(&right, input_types))
                            .saturating_div(
                                1_000_usize.saturating_pow(keys.len().try_into().unwrap()),
                            );

                        // tripletrail: with variables bound by the input (the right side of a
                        // LATERAL join, evaluated once per row), a hash join would build its
                        // right side again for every row: look the right side up instead.
                        let per_row = input_types.iter().any(|(_, t)| !t.undef);
                        if per_row || lateral_cost <= join_cost.saturating_mul(100) {
                            let right_for_lateral =
                                Self::reorder_joins((*right).clone(), &left_driving);
                            if is_fit_for_for_loop_join(
                                &right_for_lateral,
                                input_types,
                                &left_driving,
                            ) {
                                return GraphPattern::lateral(
                                    left,
                                    GraphPattern::left_join(
                                        GraphPattern::empty_singleton(),
                                        right_for_lateral,
                                        expression,
                                        LeftJoinAlgorithm::HashBuildRightProbeLeft {
                                            keys: Vec::new(),
                                        },
                                    ),
                                );
                            }
                        }
                    }
                }
                let right = Self::reorder_joins(*right, input_types);
                let right_types = infer_graph_pattern_types(&right, input_types.clone());
                GraphPattern::left_join(
                    left,
                    right,
                    expression,
                    LeftJoinAlgorithm::HashBuildRightProbeLeft {
                        keys: join_key_variables(&left_types, &right_types, input_types),
                    },
                )
            }
            GraphPattern::Minus {
                left,
                right,
                graph_variable,
                ..
            } => {
                let left = Self::reorder_joins(*left, input_types);
                let left_types = infer_graph_pattern_types(&left, input_types.clone());
                let right = Self::reorder_joins(*right, input_types);
                let right_types = infer_graph_pattern_types(&right, input_types.clone());
                GraphPattern::minus(
                    left,
                    right,
                    MinusAlgorithm::HashBuildRightProbeLeft {
                        keys: join_key_variables(&left_types, &right_types, input_types),
                    },
                    graph_variable,
                )
            }
            GraphPattern::Extend {
                inner,
                expression,
                variable,
            } => GraphPattern::extend(
                Self::reorder_joins(*inner, input_types),
                variable,
                expression,
            ),
            GraphPattern::Filter { inner, expression } => {
                GraphPattern::filter(Self::reorder_joins(*inner, input_types), expression)
            }
            GraphPattern::Union { inner } => GraphPattern::union_all(
                inner
                    .into_iter()
                    .map(|c| Self::reorder_joins(c, input_types)),
            ),
            GraphPattern::Slice {
                inner,
                start,
                length,
            } => GraphPattern::slice(Self::reorder_joins(*inner, input_types), start, length),
            GraphPattern::Distinct { inner } => {
                GraphPattern::distinct(Self::reorder_joins(*inner, input_types))
            }
            GraphPattern::Reduced { inner } => {
                GraphPattern::reduced(Self::reorder_joins(*inner, input_types))
            }
            GraphPattern::Project { inner, variables } => {
                GraphPattern::project(Self::reorder_joins(*inner, input_types), variables)
            }
            GraphPattern::OrderBy { inner, expression } => {
                GraphPattern::order_by(Self::reorder_joins(*inner, input_types), expression)
            }
            GraphPattern::Service { .. } => {
                // We don't do join reordering inside of SERVICE calls, we don't know about cardinalities
                pattern
            }
            GraphPattern::Group {
                inner,
                variables,
                aggregates,
            } => GraphPattern::group(
                Self::reorder_joins(*inner, input_types),
                variables,
                aggregates,
            ),
        }
    }
}

/// The most rows of a `VALUES` block joined by a for loop.
const MAX_FOR_LOOP_VALUES: usize = 64;

fn is_fit_for_for_loop_join(
    pattern: &GraphPattern,
    global_input_types: &VariableTypes,
    entry_types: &VariableTypes,
) -> bool {
    // TODO: think more about it
    match pattern {
        // tripletrail: a for loop join reads all the rows of the VALUES block for each row of the
        // left side: a large block (such as the triples a rule inferred in its last pass) is hashed instead.
        GraphPattern::Values { bindings, .. } => bindings.len() <= MAX_FOR_LOOP_VALUES,
        GraphPattern::QuadPattern { .. }
        | GraphPattern::Path { .. }
        | GraphPattern::Graph { .. } => true,
        #[cfg(feature = "sep-0006")]
        GraphPattern::Lateral { left, right } => {
            is_fit_for_for_loop_join(left, global_input_types, entry_types)
                && is_fit_for_for_loop_join(right, global_input_types, entry_types)
        }
        GraphPattern::LeftJoin {
            left,
            right,
            expression,
            ..
        } => {
            if !is_fit_for_for_loop_join(left, global_input_types, entry_types) {
                return false;
            }

            // It is not ok to transform into for loop join if right binds a variable also bound by the entry part of the for loop join
            let mut left_types = infer_graph_pattern_types(left, global_input_types.clone());
            // Keep the global input types for expressions such as BIND(?input AS ?output).
            // Only check variables used by right: other global input bindings pass through unchanged.
            let right_types = infer_graph_pattern_types(right, global_input_types.clone());
            let mut has_conflicting_variable = false;
            right.lookup_used_variables(&mut |variable| {
                has_conflicting_variable |= right_types.get(variable) != VariableType::UNDEF
                    && left_types.get(variable).undef
                    && entry_types.get(variable) != VariableType::UNDEF;
            });
            if has_conflicting_variable {
                return false;
            }

            // We don't forget the final expression
            left_types.intersect_with(right_types);
            is_expression_fit_for_for_loop_join(expression, &left_types, entry_types)
        }
        GraphPattern::Union { inner } => inner
            .iter()
            .all(|i| is_fit_for_for_loop_join(i, global_input_types, entry_types)),
        GraphPattern::Filter { inner, expression } => {
            is_fit_for_for_loop_join(inner, global_input_types, entry_types)
                && is_expression_fit_for_for_loop_join(
                    expression,
                    &infer_graph_pattern_types(inner, global_input_types.clone()),
                    entry_types,
                )
        }
        GraphPattern::Extend {
            inner,
            expression,
            variable,
        } => {
            is_fit_for_for_loop_join(inner, global_input_types, entry_types)
                && entry_types.get(variable) == VariableType::UNDEF
                && is_expression_fit_for_for_loop_join(
                    expression,
                    &infer_graph_pattern_types(inner, global_input_types.clone()),
                    entry_types,
                )
        }
        GraphPattern::Join { .. }
        | GraphPattern::Minus { .. }
        | GraphPattern::Service { .. }
        | GraphPattern::OrderBy { .. }
        | GraphPattern::Distinct { .. }
        | GraphPattern::Reduced { .. }
        | GraphPattern::Slice { .. }
        | GraphPattern::Project { .. }
        | GraphPattern::Group { .. } => false,
    }
}

fn are_all_expression_variables_bound(
    expression: &Expression,
    variable_types: &VariableTypes,
) -> bool {
    expression
        .used_variables()
        .into_iter()
        .all(|v| !variable_types.get(v).undef)
}

/// tripletrail (#531): `?v = e` (or `e = ?v`, or `sameTerm`) where `?v` is bound in every row of
/// `with` and never in `other`, and `e` reads only variables bound in every row of `other`:
/// returns `?v` and `e`. With `=`, `e` must give a simple literal or an IRI, for which `=` is
/// `sameTerm`, so that the join (on terms) keeps the same rows as the filter.
fn equality_key(
    filter: &Expression,
    with: &VariableTypes,
    other: &VariableTypes,
) -> Option<(Variable, Expression)> {
    let (a, b, same_term) = match filter {
        Expression::Equal(a, b) => (a, b, false),
        Expression::SameTerm(a, b) => (a, b, true),
        _ => return None,
    };
    for (v, e) in [(a, b), (b, a)] {
        let Expression::Variable(v) = &**v else {
            continue;
        };
        if with.get(v).undef
            || other.get(v) != VariableType::UNDEF
            || e.used_variables().is_empty()
            || !are_all_expression_variables_bound(e, other)
            || !(same_term || gives_plain_string_or_iri(e))
        {
            continue;
        }
        return Some((v.clone(), (**e).clone()));
    }
    None
}

/// Whether `pattern` binds `v` only by matching it in quads (or as a graph name), never by
/// `BIND`, `VALUES`, an aggregate, a sub-select or `SERVICE`: then it can be evaluated with `v`
/// already bound, as the right side of a `LATERAL` join (#531).
fn matched_only(pattern: &GraphPattern, v: &Variable) -> bool {
    match pattern {
        GraphPattern::QuadPattern { .. } | GraphPattern::Path { .. } | GraphPattern::Graph { .. } => {
            true
        }
        GraphPattern::Join { left, right, .. }
        | GraphPattern::LeftJoin { left, right, .. }
        | GraphPattern::Minus { left, right, .. } => {
            matched_only(left, v) && matched_only(right, v)
        }
        #[cfg(feature = "sep-0006")]
        GraphPattern::Lateral { left, right } => matched_only(left, v) && matched_only(right, v),
        GraphPattern::Union { inner } => inner.iter().all(|p| matched_only(p, v)),
        GraphPattern::Filter { inner, .. }
        | GraphPattern::OrderBy { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner } => matched_only(inner, v),
        GraphPattern::Extend {
            inner, variable, ..
        } => variable != v && matched_only(inner, v),
        GraphPattern::Values { variables, .. } => !variables.contains(v),
        GraphPattern::Project { variables, .. } => !variables.contains(v),
        GraphPattern::Slice { .. } | GraphPattern::Group { .. } | GraphPattern::Service { .. } => {
            false
        }
    }
}

/// Whether the value of `e`, when it has one, is a simple literal or an IRI.
fn gives_plain_string_or_iri(e: &Expression) -> bool {
    match e {
        Expression::Literal(l) => l.language().is_none() && l.datatype() == oxrdf::vocab::xsd::STRING,
        Expression::NamedNode(_) => true,
        Expression::FunctionCall(f, args) => match f {
            Function::Str
            | Function::Iri
            | Function::EncodeForUri
            | Function::Md5
            | Function::Sha1
            | Function::Sha256
            | Function::Sha384
            | Function::Sha512
            | Function::StrUuid => true,
            Function::StrAfter
            | Function::StrBefore
            | Function::SubStr
            | Function::UCase
            | Function::LCase => args.first().is_some_and(gives_plain_string_or_iri),
            Function::Concat => args.iter().all(gives_plain_string_or_iri),
            _ => false,
        },
        _ => false,
    }
}

/// `pattern` with `?variable` bound to `value`, and only the rows where it has one.
fn computed_key(pattern: GraphPattern, variable: Variable, value: Expression) -> GraphPattern {
    GraphPattern::filter(
        GraphPattern::extend(pattern, variable.clone(), value),
        Expression::Bound(variable),
    )
}

fn are_no_expression_variables_bound(
    expression: &Expression,
    variable_types: &VariableTypes,
) -> bool {
    expression
        .used_variables()
        .into_iter()
        .all(|v| variable_types.get(v) == VariableType::UNDEF)
}

fn is_expression_fit_for_for_loop_join(
    expression: &Expression,
    input_types: &VariableTypes,
    entry_types: &VariableTypes,
) -> bool {
    match expression {
        Expression::NamedNode(_) | Expression::Literal(_) => true,
        Expression::Variable(v) | Expression::Bound(v) => {
            !input_types.get(v).undef || entry_types.get(v) == VariableType::UNDEF
        }
        Expression::Or(inner)
        | Expression::And(inner)
        | Expression::Coalesce(inner)
        | Expression::FunctionCall(_, inner) => inner
            .iter()
            .all(|e| is_expression_fit_for_for_loop_join(e, input_types, entry_types)),
        Expression::Equal(a, b)
        | Expression::SameTerm(a, b)
        | Expression::Greater(a, b)
        | Expression::GreaterOrEqual(a, b)
        | Expression::Less(a, b)
        | Expression::LessOrEqual(a, b)
        | Expression::Add(a, b)
        | Expression::Subtract(a, b)
        | Expression::Multiply(a, b)
        | Expression::Divide(a, b) => {
            is_expression_fit_for_for_loop_join(a, input_types, entry_types)
                && is_expression_fit_for_for_loop_join(b, input_types, entry_types)
        }
        Expression::UnaryPlus(e) | Expression::UnaryMinus(e) | Expression::Not(e) => {
            is_expression_fit_for_for_loop_join(e, input_types, entry_types)
        }
        Expression::If(a, b, c) => {
            is_expression_fit_for_for_loop_join(a, input_types, entry_types)
                && is_expression_fit_for_for_loop_join(b, input_types, entry_types)
                && is_expression_fit_for_for_loop_join(c, input_types, entry_types)
        }
        // tripletrail: an `EXISTS` reads the variables of its pattern from the row of the
        // pattern it filters: a variable that row may leave unbound, but the left side of the
        // LATERAL join binds, would be read from the left side's row instead. In
        // `{ ?o ex:qty ?q FILTER EXISTS { ?o ex:next ?n } } ?o ex:link ?n`, `?n` is free in
        // the filter's group.
        Expression::Exists(inner) => {
            let mut free_but_bound = false;
            inner.lookup_used_variables(&mut |v| {
                free_but_bound |=
                    input_types.get(v).undef && entry_types.get(v) != VariableType::UNDEF;
            });
            !free_but_bound && is_fit_for_for_loop_join(inner, input_types, entry_types)
        }
    }
}

fn has_common_variables(
    left: &VariableTypes,
    right: &VariableTypes,
    input_types: &VariableTypes,
) -> bool {
    // TODO: we should be smart and count as shared variables FILTER(?a = ?b)
    // tripletrail: look up the variables of the smaller side in the larger one. The join that
    // `reorder_joins` grows binds every variable so far, and was walked for each candidate at
    // each step: n³ for a join of n triple patterns (#483).
    let (left, right) = smaller_first(left, right);
    left.iter().any(|(variable, left_type)| {
        !left_type.undef && !right.get(variable).undef && input_types.get(variable).undef
    })
}

/// tripletrail: the two in the order to iterate the first and look up in the second.
fn smaller_first<'a>(
    a: &'a VariableTypes,
    b: &'a VariableTypes,
) -> (&'a VariableTypes, &'a VariableTypes) {
    if a.len() <= b.len() { (a, b) } else { (b, a) }
}

fn join_key_variables(
    left: &VariableTypes,
    right: &VariableTypes,
    input_types: &VariableTypes,
) -> Vec<Variable> {
    let (left, right) = smaller_first(left, right);
    left.iter()
        .filter(|(variable, left_type)| {
            !left_type.undef && !right.get(variable).undef && input_types.get(variable).undef
        })
        .map(|(variable, _)| variable.clone())
        .collect()
}

/// tripletrail: the size of the first step of a pattern evaluated as nested loops (LATERAL joins,
/// for-loop left joins and BINDs and FILTERs over them), or its size.
#[cfg(feature = "sep-0006")]
fn entry_size(pattern: &GraphPattern, input_types: &VariableTypes) -> usize {
    match pattern {
        GraphPattern::Lateral { left, .. } => entry_size(left, input_types),
        GraphPattern::Extend { inner, .. } | GraphPattern::Filter { inner, .. } => {
            entry_size(inner, input_types)
        }
        _ => estimate_graph_pattern_size(pattern, input_types),
    }
}

fn estimate_graph_pattern_size(pattern: &GraphPattern, input_types: &VariableTypes) -> usize {
    match pattern {
        // tripletrail: the rows of a VALUES block are known, the size of a triple pattern is a
        // guess: 1,000 for one with a constant object (`?ne rtm:isNetworkResourceOf ex:micro`),
        // which is often a whole class. Joined with VALUES (focus nodes of a SHACL rule, ids
        // given by an application), start from the VALUES block unless it is much larger.
        GraphPattern::Values { bindings, .. } => bindings.len().div_ceil(10),
        GraphPattern::QuadPattern {
            subject,
            predicate,
            object,
            ..
        } => {
            // tripletrail: an estimate from the data, when the store gave an estimator.
            if let NamedNodePattern::NamedNode(p) = predicate {
                fn slot<'t>(
                    t: &'t GroundTermPattern,
                    input_types: &VariableTypes,
                ) -> crate::estimate::Slot<'t> {
                    match t {
                        GroundTermPattern::Variable(v) if input_types.get(v).undef => {
                            crate::estimate::Slot::Free
                        }
                        GroundTermPattern::Variable(_) => crate::estimate::Slot::Bound,
                        t => crate::estimate::Slot::Constant(t),
                    }
                }
                if let Some(n) = crate::estimate::triples(
                    slot(subject, input_types),
                    p,
                    slot(object, input_types),
                ) {
                    return n;
                }
            }
            let mut size = estimate_triple_pattern_size(
                is_term_pattern_bound(subject, input_types),
                is_named_node_pattern_bound(predicate, input_types),
                is_term_pattern_bound(object, input_types),
            );
            if let NamedNodePattern::NamedNode(predicate) = predicate {
                if *predicate == rdf::TYPE {
                    size = size.saturating_add(1);
                }
            }
            // tripletrail: an object bound through a variable (a join key from the left side of a
            // LATERAL join) selects the few subjects linked to that value. A constant object
            // (`?s rtm:refersTo ex:gps01`) is often shared by a whole class: start from the
            // variable.
            if matches!(object, GroundTermPattern::Variable(_))
                && !is_term_pattern_bound(subject, input_types)
                && is_term_pattern_bound(object, input_types)
                && is_named_node_pattern_bound(predicate, input_types)
            {
                size = size / 10;
            }
            size
        }
        GraphPattern::Path {
            subject,
            path,
            object,
            ..
        } => estimate_path_size(
            is_term_pattern_bound(subject, input_types),
            path,
            is_term_pattern_bound(object, input_types),
        ),
        GraphPattern::Graph { graph_name } => {
            if is_named_node_pattern_bound(graph_name, input_types) {
                100
            } else {
                1
            }
        }
        GraphPattern::Join {
            left,
            right,
            algorithm,
        } => estimate_join_cost(left, right, algorithm, input_types),
        GraphPattern::LeftJoin {
            left,
            right,
            algorithm,
            ..
        } => match algorithm {
            LeftJoinAlgorithm::HashBuildRightProbeLeft { keys } => {
                let left_size = estimate_graph_pattern_size(left, input_types);
                max(
                    left_size,
                    left_size
                        .saturating_mul(estimate_graph_pattern_size(
                            right,
                            &infer_graph_pattern_types(right, input_types.clone()),
                        ))
                        .saturating_div(1_000_usize.saturating_pow(keys.len().try_into().unwrap())),
                )
            }
        },
        #[cfg(feature = "sep-0006")]
        GraphPattern::Lateral { left, right } => estimate_lateral_cost(
            left,
            &infer_graph_pattern_types(left, input_types.clone()),
            right,
            input_types,
        ),
        GraphPattern::Union { inner } => inner
            .iter()
            .map(|inner| estimate_graph_pattern_size(inner, input_types))
            .fold(0, usize::saturating_add),
        GraphPattern::Minus { left, .. } => estimate_graph_pattern_size(left, input_types),
        GraphPattern::Filter { inner, .. }
        | GraphPattern::Extend { inner, .. }
        | GraphPattern::OrderBy { inner, .. }
        | GraphPattern::Project { inner, .. }
        | GraphPattern::Distinct { inner, .. }
        | GraphPattern::Reduced { inner, .. }
        | GraphPattern::Group { inner, .. } => estimate_graph_pattern_size(inner, input_types),
        // tripletrail: the store's estimates are not about the inside of a SERVICE.
        GraphPattern::Service { inner, .. } => {
            crate::estimate::without(|| estimate_graph_pattern_size(inner, input_types))
        }
        GraphPattern::Slice {
            inner,
            start,
            length,
        } => {
            let inner = estimate_graph_pattern_size(inner, input_types).saturating_sub(*start);
            if let Some(length) = length {
                min(inner, *length)
            } else {
                inner
            }
        }
    }
}

fn estimate_join_cost(
    left: &GraphPattern,
    right: &GraphPattern,
    algorithm: &JoinAlgorithm,
    input_types: &VariableTypes,
) -> usize {
    match algorithm {
        JoinAlgorithm::HashBuildLeftProbeRight { keys } => {
            estimate_graph_pattern_size(left, input_types)
                .saturating_mul(estimate_graph_pattern_size(right, input_types))
                .saturating_div(1_000_usize.saturating_pow(keys.len().try_into().unwrap()))
        }
    }
}

/// tripletrail: the variables a pattern binds by `BIND` or by an aggregate (#484, #486): bound
/// in every row but those where the expression fails, unlike an `OPTIONAL`'s.
fn bound_by_expressions(pattern: &GraphPattern) -> Vec<Variable> {
    let mut found = Vec::new();
    let mut todo = vec![pattern];
    while let Some(p) = todo.pop() {
        match p {
            GraphPattern::Extend {
                inner, variable, ..
            } => {
                found.push(variable.clone());
                todo.push(inner);
            }
            GraphPattern::Group { aggregates, .. } => {
                found.extend(aggregates.iter().map(|(v, _)| v.clone()));
            }
            GraphPattern::Join { left, right, .. } => {
                todo.push(left);
                todo.push(right);
            }
            #[cfg(feature = "sep-0006")]
            GraphPattern::Lateral { left, right } => {
                todo.push(left);
                todo.push(right);
            }
            GraphPattern::LeftJoin { left, .. } | GraphPattern::Minus { left, .. } => {
                todo.push(left)
            }
            GraphPattern::Filter { inner, .. }
            | GraphPattern::Project { inner, .. }
            | GraphPattern::Distinct { inner }
            | GraphPattern::Reduced { inner }
            | GraphPattern::OrderBy { inner, .. }
            | GraphPattern::Slice { inner, .. } => todo.push(inner),
            _ => {}
        }
    }
    found
}

/// tripletrail: `types` with the variables of `binds` the input does not bind taken as bound.
fn driving_types(
    types: &VariableTypes,
    binds: &[Variable],
    input_types: &VariableTypes,
) -> VariableTypes {
    let mut driving = types.clone();
    if !binds.is_empty() {
        driving.assume_bound(|v| binds.contains(v) && input_types.get(v).undef);
    }
    driving
}

/// tripletrail: `estimate_join_cost` of a hash join on `keys` variables, with the size of its
/// left side already known.
fn join_cost(
    left_size: usize,
    right: &GraphPattern,
    keys: usize,
    input_types: &VariableTypes,
) -> usize {
    left_size
        .saturating_mul(estimate_graph_pattern_size(right, input_types))
        .saturating_div(1_000_usize.saturating_pow(keys.try_into().unwrap()))
}

fn estimate_lateral_cost(
    left: &GraphPattern,
    left_types: &VariableTypes,
    right: &GraphPattern,
    input_types: &VariableTypes,
) -> usize {
    estimate_graph_pattern_size(left, input_types)
        .saturating_mul(estimate_graph_pattern_size(right, left_types))
}

fn estimate_triple_pattern_size(
    subject_bound: bool,
    predicate_bound: bool,
    object_bound: bool,
) -> usize {
    match (subject_bound, predicate_bound, object_bound) {
        (true, true, true) => 1,
        (true, true, false) => 10,
        (true, false, true) => 2,
        (false, true, true) => 1_000,
        (true, false, false) => 100,
        (false, false, false) => 1_000_000_000,
        (false, true, false) => 1_000_000,
        (false, false, true) => 10_000,
    }
}

fn estimate_path_size(start_bound: bool, path: &PropertyPathExpression, end_bound: bool) -> usize {
    match path {
        PropertyPathExpression::NamedNode(_) => {
            estimate_triple_pattern_size(start_bound, true, end_bound)
        }
        PropertyPathExpression::Reverse(p) => estimate_path_size(end_bound, p, start_bound),
        PropertyPathExpression::Sequence(a, b) => {
            // We do a for loop join in the best direction
            min(
                estimate_path_size(start_bound, a, false)
                    .saturating_mul(estimate_path_size(true, b, end_bound)),
                estimate_path_size(start_bound, a, true)
                    .saturating_mul(estimate_path_size(false, b, end_bound)),
            )
        }
        PropertyPathExpression::Alternative(a, b) => estimate_path_size(start_bound, a, end_bound)
            .saturating_add(estimate_path_size(start_bound, b, end_bound)),
        PropertyPathExpression::ZeroOrMore(p) => {
            if start_bound && end_bound {
                1
            } else if start_bound || end_bound {
                estimate_path_size(start_bound, p, end_bound).saturating_mul(1000)
            } else {
                1_000_000_000
            }
        }
        PropertyPathExpression::OneOrMore(p) => {
            if start_bound && end_bound {
                1
            } else {
                estimate_path_size(start_bound, p, end_bound).saturating_mul(1000)
            }
        }
        PropertyPathExpression::ZeroOrOne(p) => {
            if start_bound && end_bound {
                1
            } else if start_bound || end_bound {
                estimate_path_size(start_bound, p, end_bound)
            } else {
                1_000_000_000
            }
        }
        PropertyPathExpression::NegatedPropertySet(_) => {
            estimate_triple_pattern_size(start_bound, false, end_bound)
        }
        // tripletrail: bounded paths, as their unbounded forms.
        PropertyPathExpression::Bounded { path, min, .. } => {
            if start_bound && end_bound {
                1
            } else if *min > 0 || start_bound || end_bound {
                estimate_path_size(start_bound, path, end_bound).saturating_mul(1000)
            } else {
                1_000_000_000
            }
        }
    }
}

fn is_term_pattern_bound(pattern: &GroundTermPattern, input_types: &VariableTypes) -> bool {
    match pattern {
        GroundTermPattern::NamedNode(_) | GroundTermPattern::Literal(_) => true,
        GroundTermPattern::Variable(v) => !input_types.get(v).undef,
        #[cfg(feature = "sparql-12")]
        GroundTermPattern::Triple(t) => {
            is_term_pattern_bound(&t.subject, input_types)
                && is_named_node_pattern_bound(&t.predicate, input_types)
                && is_term_pattern_bound(&t.object, input_types)
        }
    }
}

fn is_named_node_pattern_bound(pattern: &NamedNodePattern, input_types: &VariableTypes) -> bool {
    match pattern {
        NamedNodePattern::NamedNode(_) => true,
        NamedNodePattern::Variable(v) => !input_types.get(v).undef,
    }
}

// tripletrail: tests of the LATERAL planning changes.
#[cfg(all(test, feature = "sep-0006"))]
mod tripletrail_tests {
    use super::*;
    use std::collections::HashSet;

    fn plan(q: &str) -> GraphPattern {
        let spargebra::Query::Select { pattern, .. } =
            spargebra::SparqlParser::new().parse_query(q).unwrap()
        else {
            panic!("not a SELECT")
        };
        Optimizer::optimize_graph_pattern(GraphPattern::from(&pattern))
    }

    fn variables(p: &GraphPattern) -> HashSet<Variable> {
        let mut out = HashSet::new();
        p.lookup_used_variables(&mut |v| {
            out.insert(v.clone());
        });
        out
    }

    /// With estimates from the data, a one-row VALUES whose lookup matches much (one
    /// positioning system shared by half the coordinates) is not where the plan starts.
    #[test]
    fn with_estimates_values_with_a_large_fanout_does_not_start() {
        struct Counts;
        impl crate::estimate::Estimator for Counts {
            fn triples(
                &self,
                subject: crate::estimate::Slot<'_>,
                predicate: &oxrdf::NamedNode,
                object: crate::estimate::Slot<'_>,
            ) -> Option<usize> {
                use crate::estimate::Slot;
                Some(match (predicate.as_str(), subject, object) {
                    ("http://e/ps", Slot::Free, Slot::Bound) => 90_000,
                    ("http://e/ps", _, _) => 180_000,
                    ("http://e/type", Slot::Free, _) => 4_000,
                    (_, Slot::Bound, _) | (_, _, Slot::Bound) => 1,
                    _ => 200_000,
                })
            }
        }
        let q = "SELECT * WHERE { VALUES ?ps { <http://e/lps02> } ?c <http://e/ps> ?ps .
            ?t <http://e/type> <http://e/Track> . ?t <http://e/coord> ?c }";
        let first = |p: &GraphPattern| first_quad(p).map(|q| format!("{q:?}"));
        let guessed = plan(q);
        assert!(
            !first(&guessed).unwrap_or_default().contains("type"),
            "{guessed:?}"
        );
        let estimated = crate::estimate::with_estimator(std::rc::Rc::new(Counts), || plan(q));
        assert!(first(&estimated).unwrap().contains("type"), "{estimated:?}");
    }

    /// A MINUS whose sides share no variable is dropped, also in `GRAPH ?g`, where both
    /// sides would otherwise bind the graph variable (W3C test graph-minus).
    #[test]
    fn minus_without_a_shared_variable_is_dropped() {
        let has_minus = |p: &GraphPattern| format!("{p:?}").contains("Minus {");
        for q in [
            "SELECT ?a WHERE { GRAPH ?g { ?a <http://e/p> <http://e/o> MINUS { ?b <http://e/p> <http://e/o> } } }",
            "SELECT ?a WHERE { ?a <http://e/p> <http://e/o> MINUS { ?b <http://e/p> <http://e/o> } }",
        ] {
            assert!(!has_minus(&plan(q)), "{q}");
        }
        let q = "SELECT ?a WHERE { GRAPH ?g { ?a <http://e/p> <http://e/o> MINUS { ?a <http://e/q> ?b } } }";
        assert!(has_minus(&plan(q)));
    }

    /// A large VALUES block is hashed, not read again for each row of the other side.
    #[test]
    fn large_values_are_not_for_loop_joined() {
        fn values_in_lateral_right(p: &GraphPattern, right: bool) -> Option<usize> {
            match p {
                GraphPattern::Values { bindings, .. } if right => Some(bindings.len()),
                GraphPattern::Lateral { left, right } => values_in_lateral_right(left, false)
                    .or_else(|| values_in_lateral_right(right, true)),
                GraphPattern::Join { left, right, .. }
                | GraphPattern::LeftJoin { left, right, .. } => {
                    values_in_lateral_right(left, false)
                        .or_else(|| values_in_lateral_right(right, false))
                }
                GraphPattern::Filter { inner, .. }
                | GraphPattern::Extend { inner, .. }
                | GraphPattern::Project { inner, .. }
                | GraphPattern::Distinct { inner } => values_in_lateral_right(inner, right),
                _ => None,
            }
        }
        let rows = |n: usize| {
            (0..n)
                .map(|i| format!("(<http://e/b{i}> {i})"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let query = |n| {
            format!(
                "SELECT * WHERE {{ VALUES ?x {{ <http://e/a> }} ?x <http://e/p> ?y .
                 VALUES (?y ?z) {{ {} }} }}",
                rows(n)
            )
        };
        assert_eq!(values_in_lateral_right(&plan(&query(1000)), false), None);
        let small = plan(&query(8));
        assert_eq!(values_in_lateral_right(&small, false), Some(8), "{small:?}");
    }

    /// The first triple pattern evaluated in `p`.
    fn first_quad(p: &GraphPattern) -> Option<&GraphPattern> {
        match p {
            GraphPattern::QuadPattern { .. } => Some(p),
            GraphPattern::Lateral { left, .. }
            | GraphPattern::Join { left, .. }
            | GraphPattern::LeftJoin { left, .. } => first_quad(left),
            GraphPattern::Extend { inner, .. }
            | GraphPattern::Filter { inner, .. }
            | GraphPattern::Project { inner, .. } => first_quad(inner),
            _ => None,
        }
    }

    /// A hash join, or a hash left join (not the `LeftJoin` of the empty singleton that
    /// stands for a for-loop left join).
    fn has_hash_join(p: &GraphPattern) -> bool {
        match p {
            GraphPattern::Join { .. } => true,
            GraphPattern::LeftJoin { left, right, .. } => {
                !matches!(&**left, GraphPattern::Values { variables, .. } if variables.is_empty())
                    || has_hash_join(right)
            }
            GraphPattern::Lateral { left, right } => has_hash_join(left) || has_hash_join(right),
            GraphPattern::Extend { inner, .. }
            | GraphPattern::Filter { inner, .. }
            | GraphPattern::Project { inner, .. } => has_hash_join(inner),
            GraphPattern::Union { inner } => inner.iter().any(has_hash_join),
            _ => false,
        }
    }

    /// Checks that the right side of every LATERAL join (and every branch of a UNION there)
    /// starts with a triple pattern on a variable the left side binds, and holds no hash
    /// join (which would be built again for every left row).
    fn check(p: &GraphPattern) {
        match p {
            GraphPattern::Lateral { left, right } => {
                check(left);
                let bound = variables(left);
                let branches = match &**right {
                    GraphPattern::Union { inner } => inner.iter().collect(),
                    other => vec![other],
                };
                for branch in branches {
                    if let Some(quad) = first_quad(branch) {
                        assert!(
                            variables(quad).iter().any(|v| bound.contains(v)),
                            "{quad:?} does not use {bound:?}"
                        );
                    }
                    assert!(!has_hash_join(branch), "{branch:?}");
                }
                check(right);
            }
            GraphPattern::Join { left, right, .. } | GraphPattern::LeftJoin { left, right, .. } => {
                check(left);
                check(right);
            }
            GraphPattern::Extend { inner, .. }
            | GraphPattern::Filter { inner, .. }
            | GraphPattern::Project { inner, .. } => check(inner),
            GraphPattern::Union { inner } => inner.iter().for_each(check),
            _ => {}
        }
    }

    /// A hash join on no key: every row of one side with every row of the other.
    fn has_cartesian_product(p: &GraphPattern) -> bool {
        match p {
            GraphPattern::Join {
                left,
                right,
                algorithm: JoinAlgorithm::HashBuildLeftProbeRight { keys },
            } => keys.is_empty() || has_cartesian_product(left) || has_cartesian_product(right),
            GraphPattern::Lateral { left, right } | GraphPattern::LeftJoin { left, right, .. } => {
                has_cartesian_product(left) || has_cartesian_product(right)
            }
            GraphPattern::Extend { inner, .. }
            | GraphPattern::Filter { inner, .. }
            | GraphPattern::Project { inner, .. }
            | GraphPattern::Group { inner, .. } => has_cartesian_product(inner),
            GraphPattern::Union { inner } => inner.iter().any(has_cartesian_product),
            _ => false,
        }
    }

    /// BSBM explore Q5: the one-row lookups of the given product (`P1 num1 ?o1`) are joined
    /// first and the rest per their row, so the FILTER comparing `?o1` with `?s1` runs inside
    /// the chain, after the triple pattern binding `?s1`, not on every row at the end.
    #[test]
    fn one_row_parts_drive_the_rest() {
        struct Counts;
        impl crate::estimate::Estimator for Counts {
            fn triples(
                &self,
                subject: crate::estimate::Slot<'_>,
                predicate: &oxrdf::NamedNode,
                object: crate::estimate::Slot<'_>,
            ) -> Option<usize> {
                use crate::estimate::Slot;
                Some(match (predicate.as_str(), subject, object) {
                    ("http://e/feature", Slot::Free, Slot::Bound) => 40,
                    ("http://e/feature", Slot::Free, Slot::Free) => 50_000,
                    ("http://e/feature", _, _) => 20,
                    (_, Slot::Free, Slot::Free) => 3_000,
                    _ => 1,
                })
            }
        }
        let q = "SELECT ?p ?l WHERE {
            ?p <http://e/label> ?l . FILTER(<http://e/P1> != ?p)
            <http://e/P1> <http://e/feature> ?f . ?p <http://e/feature> ?f .
            <http://e/P1> <http://e/num1> ?o1 . ?p <http://e/num1> ?s1 .
            FILTER(?s1 < ?o1 + 120 && ?s1 > ?o1 - 120)
            <http://e/P1> <http://e/num2> ?o2 . ?p <http://e/num2> ?s2 .
            FILTER(?s2 < ?o2 + 170 && ?s2 > ?o2 - 170) }";
        let p = crate::estimate::with_estimator(std::rc::Rc::new(Counts), || plan(q));
        assert!(!has_cartesian_product(&p), "{p:?}");
        // The filter on ?s1 is inside the LATERAL chain (the one on ?s2 follows the chain's
        // last step, so it may be on top).
        fn top_filter_mentions(p: &GraphPattern, v: &str) -> bool {
            match p {
                GraphPattern::Filter { expression, inner } => {
                    format!("{expression:?}").contains(v) || top_filter_mentions(inner, v)
                }
                GraphPattern::Project { inner, .. } => top_filter_mentions(inner, v),
                _ => false,
            }
        }
        assert!(!top_filter_mentions(&p, "s1"), "{p:?}");
        // Without estimates (no one-row parts known), the plan is as before.
        let guessed = plan(q);
        assert!(top_filter_mentions(&guessed, "s1"), "{guessed:?}");
    }

    #[test]
    fn lookups_on_a_sample_and_a_bind() {
        let p = plan(
            "PREFIX : <http://x/> SELECT * WHERE {
                ?t a :Tunnel ; :loc ?l .
                { SELECT ?t (SAMPLE(?a) AS ?ane) WHERE { ?t :loc/:ane ?a } GROUP BY ?t }
                ?ane :on ?ne ; :begin ?b ; :end ?e .
                ?b :measure ?mb . ?e :measure ?me .
                BIND(IF(?mb < ?me, ?b, ?e) AS ?c)
                ?c :refersTo :lps02 . }",
        );
        assert!(!has_cartesian_product(&p), "{p:?}");
        check(&p);
    }

    /// The EXISTS patterns of `p`'s filters.
    fn exists_patterns(p: &GraphPattern, out: &mut Vec<GraphPattern>) {
        fn in_expression(e: &Expression, out: &mut Vec<GraphPattern>) {
            match e {
                Expression::Exists(p) => out.push((**p).clone()),
                Expression::Not(e) => in_expression(e, out),
                Expression::And(l) | Expression::Or(l) => {
                    l.iter().for_each(|e| in_expression(e, out))
                }
                _ => {}
            }
        }
        match p {
            GraphPattern::Filter { inner, expression } => {
                in_expression(expression, out);
                exists_patterns(inner, out);
            }
            GraphPattern::Lateral { left, right } | GraphPattern::Join { left, right, .. } => {
                exists_patterns(left, out);
                exists_patterns(right, out);
            }
            GraphPattern::Extend { inner, .. } | GraphPattern::Project { inner, .. } => {
                exists_patterns(inner, out)
            }
            _ => {}
        }
    }

    #[test]
    fn not_exists_starts_from_the_row() {
        let p = plan(
            "PREFIX : <http://x/> SELECT * WHERE {
                ?t a :Track .
                FILTER NOT EXISTS {
                    ?tent a :TenT ; :loc ?tl . ?tl :ane ?ta . ?ta :on ?ne ; :b ?tb .
                    ?t :loc ?l . ?l :ane ?a . ?a :on ?ne ; :b ?b .
                    FILTER(?tb <= ?b)
                } }",
        );
        let mut exists = Vec::new();
        exists_patterns(&p, &mut exists);
        assert_eq!(exists.len(), 1, "{p:?}");
        let quad = first_quad(&exists[0]).unwrap();
        assert!(
            variables(quad).contains(&Variable::new_unchecked("t")),
            "{:?}",
            exists[0]
        );
        assert!(!has_hash_join(&exists[0]), "{:?}", exists[0]);
    }

    #[test]
    fn lateral_after_bind() {
        check(&plan(
            "PREFIX : <http://x/> SELECT * WHERE {
                ?t a :Track ; :loc/:ane/:on ?ne . BIND(1 AS ?x) ?ne :begin ?b }",
        ));
    }

    #[test]
    fn lateral_union() {
        check(&plan(
            "PREFIX : <http://x/> SELECT * WHERE {
                { ?l a :S . ?l :on ?ne } UNION { ?a a :A . ?a :on ?ne . ?l :has ?a }
                ?e :spot ?l }",
        ));
    }

    #[test]
    fn lateral_with_a_constant_object() {
        check(&plan(
            "PREFIX : <http://x/> SELECT * WHERE {
                ?ne :aps ?aps . ?aps a :APS ; :ic ?ic ; :refersTo :gps01 . ?ic :coord ?g .
                BIND(1 AS ?x)
                { ?g a :G ; :x ?x1 } UNION { ?g a :G ; :y ?y } }",
        ));
    }

    #[test]
    fn lateral_with_optional() {
        check(&plan(
            "PREFIX : <http://x/> SELECT * WHERE {
                ?ls a :LS ; :loc ?ll . ?ll :ane ?ane . ?ane :on ?ne .
                { ?r :a ?ne ; :b ?op } UNION { ?r :b ?ne ; :a ?op }
                OPTIONAL { ?op :aps ?aps . ?aps a :APS ; :refersTo :gps01 }
                { ?r2 :a ?ne ; :b ?op2 } UNION { ?r2 :b ?ne ; :a ?op2 } }",
        ));
    }
}

/// tripletrail: the start and the right sides of a chain of LATERAL joins
/// `LATERAL(LATERAL(LATERAL(p0, r1), r2), ...)`, in order (#509).
#[cfg(feature = "sep-0006")]
fn lateral_spine(pattern: GraphPattern) -> (GraphPattern, Vec<GraphPattern>) {
    let mut rights = Vec::new();
    let mut current = pattern;
    while let GraphPattern::Lateral { left, right } = current {
        rights.push(*right);
        current = *left;
    }
    rights.reverse();
    (current, rights)
}
