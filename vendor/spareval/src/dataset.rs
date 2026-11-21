#[cfg(feature = "sparql-12")]
use oxrdf::BaseDirection;
use oxrdf::{
    BlankNode, Dataset, GraphNameRef, Literal, NamedNode, NamedOrBlankNodeRef, QuadRef, Term,
    TermRef,
};
#[cfg(feature = "sparql-12")]
use oxrdf::{NamedOrBlankNode, Triple};
use oxsdatatypes::{Boolean, DateTime, Decimal, Double, Float, Integer};
#[cfg(feature = "sep-0002")]
use oxsdatatypes::{Date, DayTimeDuration, Duration, Time, YearMonthDuration};
#[cfg(feature = "calendar-ext")]
use oxsdatatypes::{GDay, GMonth, GMonthDay, GYear, GYearMonth};
use rustc_hash::FxHashSet;
use std::convert::Infallible;
use std::error::Error;
use std::hash::{Hash, Hasher};
use std::iter::empty;
use std::mem::discriminant;

/// A [RDF dataset](https://www.w3.org/TR/sparql11-query/#rdfDataset) that can be queried using SPARQL
pub trait QueryableDataset<'a>: Sized + 'a {
    /// Internal representation of an RDF term
    ///
    /// Can be just an integer that indexes into a dictionary...
    ///
    /// Equality here is the RDF term equality (SPARQL `sameTerm` function)
    type InternalTerm: Clone + Eq + Hash + 'a;

    /// Error returned by the dataset.
    type Error: Error + Send + Sync + 'static;

    /// Fetches quads according to a pattern
    ///
    /// For `graph_name`, `Some(None)` encodes the default graph, `Some(Some(_))` a named graph and `None` any named graph (but not the default one!)
    fn internal_quads_for_pattern(
        &self,
        subject: Option<&Self::InternalTerm>,
        predicate: Option<&Self::InternalTerm>,
        object: Option<&Self::InternalTerm>,
        graph_name: Option<Option<&Self::InternalTerm>>,
    ) -> impl Iterator<Item = Result<InternalQuad<Self::InternalTerm>, Self::Error>> + use<'a, Self>;

    /// Fetches the list of dataset named graphs
    fn internal_named_graphs(
        &self,
    ) -> impl Iterator<Item = Result<Self::InternalTerm, Self::Error>> + use<'a, Self> {
        let mut error = None;
        let graph_names = self
            .internal_quads_for_pattern(None, None, None, None)
            .filter_map(|r| match r {
                Ok(r) => Some(r.graph_name?),
                Err(e) => {
                    error = Some(e);
                    None
                }
            })
            .collect::<FxHashSet<_>>();

        Box::new(
            error
                .map(Err)
                .into_iter()
                .chain(graph_names.into_iter().map(Ok)),
        )
    }

    /// Returns if the dataset contains a given named graph
    fn contains_internal_graph_name(
        &self,
        graph_name: &Self::InternalTerm,
    ) -> Result<bool, Self::Error> {
        Ok(self
            .internal_quads_for_pattern(None, None, None, Some(Some(graph_name)))
            .next()
            .transpose()?
            .is_some())
    }

    /// Builds an internal term from the [`Term`] struct
    fn internalize_term(&self, term: Term) -> Result<Self::InternalTerm, Self::Error>;

    /// Builds a [`Term`] from an internal term
    fn externalize_term(&self, term: Self::InternalTerm) -> Result<Term, Self::Error>;

    // Optional methods that can be overridden for better performances

    /// Builds an [`ExpressionTerm`] from an internal term
    fn externalize_expression_term(
        &self,
        term: Self::InternalTerm,
    ) -> Result<ExpressionTerm, Self::Error> {
        Ok(self.externalize_term(term)?.into())
    }

    /// Builds an internal term from an [`ExpressionTerm`]
    fn internalize_expression_term(
        &self,
        term: ExpressionTerm,
    ) -> Result<Self::InternalTerm, Self::Error> {
        self.internalize_term(term.into())
    }

    /// Computes the term [Effective boolean value](https://www.w3.org/TR/sparql11-query/#ebv)
    fn internal_term_effective_boolean_value(
        &self,
        term: Self::InternalTerm,
    ) -> Result<Option<bool>, Self::Error> {
        Ok(self
            .externalize_expression_term(term)?
            .effective_boolean_value())
    }
}

impl<'a> QueryableDataset<'a> for &'a Dataset {
    type InternalTerm = TermCow<'a>;
    type Error = Infallible;

    fn internal_quads_for_pattern(
        &self,
        subject: Option<&TermCow<'a>>,
        predicate: Option<&TermCow<'a>>,
        object: Option<&TermCow<'a>>,
        graph_name: Option<Option<&TermCow<'a>>>,
    ) -> impl Iterator<Item = Result<InternalQuad<TermCow<'a>>, Infallible>> + use<'a> {
        #[expect(clippy::unnecessary_wraps)]
        fn quad_to_result(quad: QuadRef<'_>) -> Result<InternalQuad<TermCow<'_>>, Infallible> {
            Ok(InternalQuad {
                subject: TermRef::from(quad.subject).into(),
                predicate: TermRef::from(quad.predicate).into(),
                object: quad.object.into(),
                graph_name: match quad.graph_name {
                    GraphNameRef::NamedNode(g) => Some(TermRef::from(g).into()),
                    GraphNameRef::BlankNode(g) => Some(TermRef::from(g).into()),
                    GraphNameRef::DefaultGraph => None,
                },
            })
        }

        let graph_name = if let Some(graph_name) = graph_name {
            Some(if let Some(graph_name) = graph_name {
                match graph_name.into() {
                    TermRef::NamedNode(s) => s.into(),
                    TermRef::BlankNode(s) => s.into(),
                    TermRef::Literal(_) => {
                        let empty: Box<dyn Iterator<Item = Result<_, _>>> = Box::new(empty());
                        return empty;
                    }
                    #[cfg(feature = "sparql-12")]
                    TermRef::Triple(_) => return Box::new(empty()),
                }
            } else {
                GraphNameRef::DefaultGraph
            })
        } else {
            None
        };
        if let Some(subject) = subject {
            let subject = match subject.into() {
                TermRef::NamedNode(s) => NamedOrBlankNodeRef::from(s),
                TermRef::BlankNode(s) => s.into(),
                TermRef::Literal(_) => {
                    return Box::new(empty());
                }
                #[cfg(feature = "sparql-12")]
                TermRef::Triple(_) => return Box::new(empty()),
            };
            let predicate = predicate.cloned();
            let object = object.cloned();
            let graph_name = graph_name.map(GraphNameRef::into_owned);
            Box::new(
                self.quads_for_subject(subject)
                    .filter(move |q| {
                        predicate
                            .as_ref()
                            .is_none_or(|t| TermRef::from(t) == q.predicate.into())
                            && object.as_ref().is_none_or(|t| TermRef::from(t) == q.object)
                            && graph_name.as_ref().map_or_else(
                                || !q.graph_name.is_default_graph(),
                                |t| t.as_ref() == q.graph_name,
                            )
                    })
                    .map(quad_to_result),
            )
        } else if let Some(object) = object {
            let predicate = predicate.cloned();
            let graph_name = graph_name.map(GraphNameRef::into_owned);
            Box::new(
                self.quads_for_object(object)
                    .filter(move |q| {
                        predicate
                            .as_ref()
                            .is_none_or(|t| TermRef::from(t) == q.predicate.into())
                            && graph_name.as_ref().map_or_else(
                                || !q.graph_name.is_default_graph(),
                                |t| t.as_ref() == q.graph_name,
                            )
                    })
                    .map(quad_to_result),
            )
        } else if let Some(predicate) = predicate {
            let TermRef::NamedNode(predicate) = predicate.into() else {
                return Box::new(empty());
            };
            let graph_name = graph_name.map(GraphNameRef::into_owned);
            Box::new(
                self.quads_for_predicate(predicate)
                    .filter(move |q| {
                        graph_name.as_ref().map_or_else(
                            || !q.graph_name.is_default_graph(),
                            |t| t.as_ref() == q.graph_name,
                        )
                    })
                    .map(quad_to_result),
            )
        } else if let Some(graph_name) = graph_name {
            Box::new(self.quads_for_graph_name(graph_name).map(quad_to_result))
        } else {
            Box::new(
                self.iter()
                    .filter(|q| !q.graph_name.is_default_graph())
                    .map(quad_to_result),
            )
        }
    }

    fn internalize_term(&self, term: Term) -> Result<TermCow<'a>, Infallible> {
        Ok(term.into())
    }

    fn externalize_term(&self, term: TermCow<'a>) -> Result<Term, Infallible> {
        Ok(term.into())
    }
}

pub struct InternalQuad<T> {
    pub subject: T,
    pub predicate: T,
    pub object: T,
    /// `None` if the quad is in the default graph
    pub graph_name: Option<T>,
}

/// A term as understood by the expression evaluator
#[derive(Clone)]
pub enum ExpressionTerm {
    NamedNode(NamedNode),
    BlankNode(BlankNode),
    StringLiteral(String),
    LangStringLiteral {
        value: String,
        language: String,
    },
    #[cfg(feature = "sparql-12")]
    DirLangStringLiteral {
        value: String,
        language: String,
        direction: BaseDirection,
    },
    BooleanLiteral(Boolean),
    IntegerLiteral(Integer),
    DecimalLiteral(Decimal),
    FloatLiteral(Float),
    DoubleLiteral(Double),
    DateTimeLiteral(DateTime),
    #[cfg(feature = "sep-0002")]
    DateLiteral(Date),
    #[cfg(feature = "sep-0002")]
    TimeLiteral(Time),
    #[cfg(feature = "calendar-ext")]
    GYearLiteral(GYear),
    #[cfg(feature = "calendar-ext")]
    GYearMonthLiteral(GYearMonth),
    #[cfg(feature = "calendar-ext")]
    GMonthLiteral(GMonth),
    #[cfg(feature = "calendar-ext")]
    GMonthDayLiteral(GMonthDay),
    #[cfg(feature = "calendar-ext")]
    GDayLiteral(GDay),
    #[cfg(feature = "sep-0002")]
    DurationLiteral(Duration),
    #[cfg(feature = "sep-0002")]
    YearMonthDurationLiteral(YearMonthDuration),
    #[cfg(feature = "sep-0002")]
    DayTimeDurationLiteral(DayTimeDuration),
    OtherTypedLiteral {
        value: String,
        datatype: NamedNode,
    },
    #[cfg(feature = "sparql-12")]
    Triple(Box<ExpressionTriple>),
}

impl PartialEq for ExpressionTerm {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        discriminant(self) == discriminant(other)
            && match (self, other) {
                (Self::NamedNode(l), Self::NamedNode(r)) => l == r,
                (Self::BlankNode(l), Self::BlankNode(r)) => l == r,
                (Self::StringLiteral(l), Self::StringLiteral(r)) => l == r,
                (
                    Self::LangStringLiteral {
                        value: lv,
                        language: ll,
                    },
                    Self::LangStringLiteral {
                        value: rv,
                        language: rl,
                    },
                ) => lv == rv && ll == rl,
                #[cfg(feature = "sparql-12")]
                (
                    Self::DirLangStringLiteral {
                        value: lv,
                        language: ll,
                        direction: ld,
                    },
                    Self::DirLangStringLiteral {
                        value: rv,
                        language: rl,
                        direction: rd,
                    },
                ) => lv == rv && ll == rl && ld == rd,
                (Self::BooleanLiteral(l), Self::BooleanLiteral(r)) => l == r,
                (Self::IntegerLiteral(l), Self::IntegerLiteral(r)) => l == r,
                (Self::DecimalLiteral(l), Self::DecimalLiteral(r)) => l == r,
                (Self::FloatLiteral(l), Self::FloatLiteral(r)) => l.is_identical_with(*r),
                (Self::DoubleLiteral(l), Self::DoubleLiteral(r)) => l.is_identical_with(*r),
                (Self::DateTimeLiteral(l), Self::DateTimeLiteral(r)) => l == r,
                #[cfg(feature = "sep-0002")]
                (Self::DateLiteral(l), Self::DateLiteral(r)) => l == r,
                #[cfg(feature = "sep-0002")]
                (Self::TimeLiteral(l), Self::TimeLiteral(r)) => l == r,
                #[cfg(feature = "calendar-ext")]
                (Self::GYearMonthLiteral(l), Self::GYearMonthLiteral(r)) => l == r,
                #[cfg(feature = "calendar-ext")]
                (Self::GYearLiteral(l), Self::GYearLiteral(r)) => l == r,
                #[cfg(feature = "calendar-ext")]
                (Self::GMonthLiteral(l), Self::GMonthLiteral(r)) => l == r,
                #[cfg(feature = "calendar-ext")]
                (Self::GMonthDayLiteral(l), Self::GMonthDayLiteral(r)) => l == r,
                #[cfg(feature = "calendar-ext")]
                (Self::GDayLiteral(l), Self::GDayLiteral(r)) => l == r,
                #[cfg(feature = "sep-0002")]
                (Self::DurationLiteral(l), Self::DurationLiteral(r)) => l == r,
                #[cfg(feature = "sep-0002")]
                (Self::YearMonthDurationLiteral(l), Self::YearMonthDurationLiteral(r)) => l == r,
                #[cfg(feature = "sep-0002")]
                (Self::DayTimeDurationLiteral(l), Self::DayTimeDurationLiteral(r)) => l == r,
                (
                    Self::OtherTypedLiteral {
                        value: lv,
                        datatype: ld,
                    },
                    Self::OtherTypedLiteral {
                        value: rv,
                        datatype: rd,
                    },
                ) => lv == rv && ld == rd,
                #[cfg(feature = "sparql-12")]
                (Self::Triple(l), Self::Triple(r)) => l == r,
                (_, _) => unreachable!(),
            }
    }
}

impl Eq for ExpressionTerm {}

impl Hash for ExpressionTerm {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        discriminant(self).hash(state);
        match self {
            ExpressionTerm::NamedNode(v) => v.hash(state),
            ExpressionTerm::BlankNode(v) => v.hash(state),
            ExpressionTerm::StringLiteral(v) => v.hash(state),
            ExpressionTerm::LangStringLiteral { value, language } => (value, language).hash(state),
            #[cfg(feature = "sparql-12")]
            ExpressionTerm::DirLangStringLiteral {
                value,
                language,
                direction,
            } => (value, language, direction).hash(state),
            ExpressionTerm::BooleanLiteral(v) => v.hash(state),
            ExpressionTerm::IntegerLiteral(v) => v.hash(state),
            ExpressionTerm::DecimalLiteral(v) => v.hash(state),
            ExpressionTerm::FloatLiteral(v) => v.to_be_bytes().hash(state),
            ExpressionTerm::DoubleLiteral(v) => v.to_be_bytes().hash(state),
            ExpressionTerm::DateTimeLiteral(v) => v.hash(state),
            #[cfg(feature = "sep-0002")]
            ExpressionTerm::DateLiteral(v) => v.hash(state),
            #[cfg(feature = "sep-0002")]
            ExpressionTerm::TimeLiteral(v) => v.hash(state),
            #[cfg(feature = "calendar-ext")]
            ExpressionTerm::GYearLiteral(v) => v.hash(state),
            #[cfg(feature = "calendar-ext")]
            ExpressionTerm::GYearMonthLiteral(v) => v.hash(state),
            #[cfg(feature = "calendar-ext")]
            ExpressionTerm::GMonthLiteral(v) => v.hash(state),
            #[cfg(feature = "calendar-ext")]
            ExpressionTerm::GMonthDayLiteral(v) => v.hash(state),
            #[cfg(feature = "calendar-ext")]
            ExpressionTerm::GDayLiteral(v) => v.hash(state),
            #[cfg(feature = "sep-0002")]
            ExpressionTerm::DurationLiteral(v) => v.hash(state),
            #[cfg(feature = "sep-0002")]
            ExpressionTerm::YearMonthDurationLiteral(v) => v.hash(state),
            #[cfg(feature = "sep-0002")]
            ExpressionTerm::DayTimeDurationLiteral(v) => v.hash(state),
            ExpressionTerm::OtherTypedLiteral { value, datatype } => (value, datatype).hash(state),
            #[cfg(feature = "sparql-12")]
            ExpressionTerm::Triple(v) => v.hash(state),
        }
    }
}

impl From<Term> for ExpressionTerm {
    #[inline]
    fn from(term: Term) -> Self {
        match term {
            Term::NamedNode(t) => Self::NamedNode(t),
            Term::BlankNode(t) => Self::BlankNode(t),
            Term::Literal(t) => {
                #[cfg(feature = "sparql-12")]
                {
                    let (value, datatype, language, direction) = t.destruct();
                    if let Some(language) = language {
                        if let Some(direction) = direction {
                            Self::DirLangStringLiteral {
                                value,
                                language,
                                direction,
                            }
                        } else {
                            Self::LangStringLiteral { value, language }
                        }
                    } else if let Some(datatype) = datatype {
                        parse_typed_literal(&value, datatype.as_str())
                            .unwrap_or(Self::OtherTypedLiteral { value, datatype })
                    } else {
                        Self::StringLiteral(value)
                    }
                }
                #[cfg(not(feature = "sparql-12"))]
                {
                    let (value, datatype, language) = t.destruct();
                    if let Some(language) = language {
                        Self::LangStringLiteral { value, language }
                    } else if let Some(datatype) = datatype {
                        parse_typed_literal(&value, datatype.as_str())
                            .unwrap_or(Self::OtherTypedLiteral { value, datatype })
                    } else {
                        Self::StringLiteral(value)
                    }
                }
            }
            #[cfg(feature = "sparql-12")]
            Term::Triple(t) => Self::Triple(Box::new((*t).into())),
        }
    }
}

impl From<ExpressionTerm> for Term {
    #[inline]
    fn from(term: ExpressionTerm) -> Self {
        match term {
            ExpressionTerm::NamedNode(t) => t.into(),
            ExpressionTerm::BlankNode(t) => t.into(),
            ExpressionTerm::StringLiteral(value) => Literal::from(value).into(),
            ExpressionTerm::LangStringLiteral { value, language } => {
                Literal::new_language_tagged_literal_unchecked(value, language).into()
            }
            #[cfg(feature = "sparql-12")]
            ExpressionTerm::DirLangStringLiteral {
                value,
                language,
                direction,
            } => Literal::new_directional_language_tagged_literal_unchecked(
                value, language, direction,
            )
            .into(),
            ExpressionTerm::BooleanLiteral(value) => Literal::from(value).into(),
            ExpressionTerm::IntegerLiteral(value) => Literal::from(value).into(),
            ExpressionTerm::DecimalLiteral(value) => decimal_literal(value).into(),
            ExpressionTerm::FloatLiteral(value) => Literal::from(value).into(),
            ExpressionTerm::DoubleLiteral(value) => Literal::from(value).into(),
            ExpressionTerm::DateTimeLiteral(value) => Literal::from(value).into(),
            #[cfg(feature = "sep-0002")]
            ExpressionTerm::DateLiteral(value) => Literal::from(value).into(),
            #[cfg(feature = "sep-0002")]
            ExpressionTerm::TimeLiteral(value) => Literal::from(value).into(),
            #[cfg(feature = "calendar-ext")]
            ExpressionTerm::GYearLiteral(value) => Literal::from(value).into(),
            #[cfg(feature = "calendar-ext")]
            ExpressionTerm::GYearMonthLiteral(value) => Literal::from(value).into(),
            #[cfg(feature = "calendar-ext")]
            ExpressionTerm::GMonthLiteral(value) => Literal::from(value).into(),
            #[cfg(feature = "calendar-ext")]
            ExpressionTerm::GMonthDayLiteral(value) => Literal::from(value).into(),
            #[cfg(feature = "calendar-ext")]
            ExpressionTerm::GDayLiteral(value) => Literal::from(value).into(),
            #[cfg(feature = "sep-0002")]
            ExpressionTerm::DurationLiteral(value) => Literal::from(value).into(),
            #[cfg(feature = "sep-0002")]
            ExpressionTerm::YearMonthDurationLiteral(value) => Literal::from(value).into(),
            #[cfg(feature = "sep-0002")]
            ExpressionTerm::DayTimeDurationLiteral(value) => Literal::from(value).into(),
            ExpressionTerm::OtherTypedLiteral { value, datatype } => {
                Literal::new_typed_literal(value, datatype).into()
            }
            #[cfg(feature = "sparql-12")]
            ExpressionTerm::Triple(t) => Triple::from(*t).into(),
        }
    }
}

impl From<NamedNode> for ExpressionTerm {
    #[inline]
    fn from(term: NamedNode) -> Self {
        Self::NamedNode(term)
    }
}
impl From<bool> for ExpressionTerm {
    fn from(value: bool) -> Self {
        Self::BooleanLiteral(value.into())
    }
}

impl ExpressionTerm {
    /// tripletrail: a literal as written, with exactly its datatype and lexical form (`STRDT`, a
    /// literal in an expression). A literal whose datatype is derived from a type the
    /// evaluator computes with (`xsd:long`, `xsd:dateTimeStamp`), or whose lexical form is not
    /// the canonical one (`"01"^^xsd:integer`), stays an `OtherTypedLiteral`: the typed value
    /// would write `xsd:integer` and `"1"`. Operators and functions on values read it through
    /// [`Self::value`].
    pub fn from_literal_as_written(literal: Literal) -> Self {
        if literal.language().is_some() || literal.datatype() == oxrdf::vocab::xsd::STRING {
            return Self::from(Term::from(literal));
        }
        let Some(parsed) = parse_typed_literal(literal.value(), literal.datatype().as_str()) else {
            let (value, datatype) = (literal.value().to_owned(), literal.datatype().into_owned());
            return Self::OtherTypedLiteral { value, datatype };
        };
        if let Self::OtherTypedLiteral { .. } = parsed {
            return parsed;
        }
        if let Term::Literal(written) = Term::from(parsed.clone())
            && written.datatype() == literal.datatype()
            && written.value() == literal.value()
        {
            return parsed;
        }
        let (value, datatype) = (literal.value().to_owned(), literal.datatype().into_owned());
        Self::OtherTypedLiteral { value, datatype }
    }

    /// tripletrail: the typed value of a literal kept as written (see
    /// [`Self::from_literal_as_written`]), for operators and functions on values; other terms
    /// as they are.
    pub fn value(self) -> Self {
        match self {
            Self::OtherTypedLiteral { value, datatype } => {
                parse_typed_literal(&value, datatype.as_str())
                    .or_else(|| long_decimal(&value, &datatype))
                    .unwrap_or(Self::OtherTypedLiteral { value, datatype })
            }
            term => term,
        }
    }

    /// tripletrail: [`Self::value`] of a borrowed term, without a copy when it is already one.
    pub(crate) fn value_ref(&self) -> std::borrow::Cow<'_, Self> {
        match self {
            Self::OtherTypedLiteral { value, datatype } => {
                parse_typed_literal(value, datatype.as_str())
                    .or_else(|| long_decimal(value, datatype))
                    .map_or(std::borrow::Cow::Borrowed(self), std::borrow::Cow::Owned)
            }
            term => std::borrow::Cow::Borrowed(term),
        }
    }

    /// Computes the term [Effective boolean value](https://www.w3.org/TR/sparql11-query/#ebv)
    pub(crate) fn effective_boolean_value(&self) -> Option<bool> {
        if let Self::OtherTypedLiteral { .. } = self
            && let std::borrow::Cow::Owned(value) = self.value_ref()
        {
            return value.effective_boolean_value();
        }
        match self {
            ExpressionTerm::BooleanLiteral(value) => Some((*value).into()),
            ExpressionTerm::StringLiteral(value) => Some(!value.is_empty()),
            ExpressionTerm::FloatLiteral(value) => Some(Boolean::from(*value).into()),
            ExpressionTerm::DoubleLiteral(value) => Some(Boolean::from(*value).into()),
            ExpressionTerm::IntegerLiteral(value) => Some(Boolean::from(*value).into()),
            ExpressionTerm::DecimalLiteral(value) => Some(Boolean::from(*value).into()),
            _ => None,
        }
    }
}

/// tripletrail: the value of a `xsd:decimal` literal with more than 18 fraction digits, which
/// `parse_typed_literal` refuses (see [`rounded_decimal`]).
fn long_decimal(value: &str, datatype: &NamedNode) -> Option<ExpressionTerm> {
    (datatype.as_ref() == oxrdf::vocab::xsd::DECIMAL)
        .then(|| rounded_decimal(value))
        .flatten()
        .map(ExpressionTerm::DecimalLiteral)
}

fn parse_typed_literal(value: &str, datatype: &str) -> Option<ExpressionTerm> {
    Some(match datatype {
        "http://www.w3.org/2001/XMLSchema#boolean" => {
            ExpressionTerm::BooleanLiteral(value.parse().ok()?)
        }
        "http://www.w3.org/2001/XMLSchema#string" => ExpressionTerm::StringLiteral(value.into()),
        "http://www.w3.org/2001/XMLSchema#float" => {
            ExpressionTerm::FloatLiteral(value.parse().ok()?)
        }
        "http://www.w3.org/2001/XMLSchema#double" => {
            ExpressionTerm::DoubleLiteral(value.parse().ok()?)
        }
        "http://www.w3.org/2001/XMLSchema#decimal" => {
            ExpressionTerm::DecimalLiteral(value.parse().ok()?)
        }
        "http://www.w3.org/2001/XMLSchema#integer"
        | "http://www.w3.org/2001/XMLSchema#byte"
        | "http://www.w3.org/2001/XMLSchema#short"
        | "http://www.w3.org/2001/XMLSchema#int"
        | "http://www.w3.org/2001/XMLSchema#long"
        | "http://www.w3.org/2001/XMLSchema#unsignedByte"
        | "http://www.w3.org/2001/XMLSchema#unsignedShort"
        | "http://www.w3.org/2001/XMLSchema#unsignedInt"
        | "http://www.w3.org/2001/XMLSchema#unsignedLong"
        | "http://www.w3.org/2001/XMLSchema#positiveInteger"
        | "http://www.w3.org/2001/XMLSchema#negativeInteger"
        | "http://www.w3.org/2001/XMLSchema#nonPositiveInteger"
        | "http://www.w3.org/2001/XMLSchema#nonNegativeInteger" => {
            ExpressionTerm::IntegerLiteral(value.parse().ok()?)
        }
        "http://www.w3.org/2001/XMLSchema#dateTime"
        | "http://www.w3.org/2001/XMLSchema#dateTimeStamp" => {
            ExpressionTerm::DateTimeLiteral(value.parse().ok()?)
        }
        #[cfg(feature = "sep-0002")]
        "http://www.w3.org/2001/XMLSchema#time" => ExpressionTerm::TimeLiteral(value.parse().ok()?),
        #[cfg(feature = "sep-0002")]
        "http://www.w3.org/2001/XMLSchema#date" => ExpressionTerm::DateLiteral(value.parse().ok()?),
        #[cfg(feature = "calendar-ext")]
        "http://www.w3.org/2001/XMLSchema#gYearMonth" => {
            ExpressionTerm::GYearMonthLiteral(value.parse().ok()?)
        }
        #[cfg(feature = "calendar-ext")]
        "http://www.w3.org/2001/XMLSchema#gYear" => {
            ExpressionTerm::GYearLiteral(value.parse().ok()?)
        }
        #[cfg(feature = "calendar-ext")]
        "http://www.w3.org/2001/XMLSchema#gMonthDay" => {
            ExpressionTerm::GMonthDayLiteral(value.parse().ok()?)
        }
        #[cfg(feature = "calendar-ext")]
        "http://www.w3.org/2001/XMLSchema#gDay" => ExpressionTerm::GDayLiteral(value.parse().ok()?),
        #[cfg(feature = "calendar-ext")]
        "http://www.w3.org/2001/XMLSchema#gMonth" => {
            ExpressionTerm::GMonthLiteral(value.parse().ok()?)
        }
        #[cfg(feature = "sep-0002")]
        "http://www.w3.org/2001/XMLSchema#duration" => {
            ExpressionTerm::DurationLiteral(value.parse().ok()?)
        }
        #[cfg(feature = "sep-0002")]
        "http://www.w3.org/2001/XMLSchema#yearMonthDuration" => {
            ExpressionTerm::YearMonthDurationLiteral(value.parse().ok()?)
        }
        #[cfg(feature = "sep-0002")]
        "http://www.w3.org/2001/XMLSchema#dayTimeDuration" => {
            ExpressionTerm::DayTimeDurationLiteral(value.parse().ok()?)
        }
        _ => return None,
    })
}

#[cfg(feature = "sparql-12")]
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ExpressionTriple {
    pub subject: NamedOrBlankNode,
    pub predicate: NamedNode,
    pub object: ExpressionTerm,
}

#[cfg(feature = "sparql-12")]
impl From<ExpressionTriple> for ExpressionTerm {
    #[inline]
    fn from(triple: ExpressionTriple) -> Self {
        Self::Triple(Box::new(triple))
    }
}

#[cfg(feature = "sparql-12")]
impl From<Triple> for ExpressionTriple {
    #[inline]
    fn from(triple: Triple) -> Self {
        ExpressionTriple {
            subject: triple.subject,
            predicate: triple.predicate,
            object: triple.object.into(),
        }
    }
}

#[cfg(feature = "sparql-12")]
impl From<ExpressionTriple> for Triple {
    #[inline]
    fn from(triple: ExpressionTriple) -> Self {
        Triple {
            subject: triple.subject,
            predicate: triple.predicate,
            object: triple.object.into(),
        }
    }
}

#[cfg(feature = "sparql-12")]
impl ExpressionTriple {
    pub fn new(
        subject: ExpressionTerm,
        predicate: ExpressionTerm,
        object: ExpressionTerm,
    ) -> Option<Self> {
        if !matches!(
            subject,
            ExpressionTerm::NamedNode(_) | ExpressionTerm::BlankNode(_) | ExpressionTerm::Triple(_)
        ) {
            return None;
        }
        if !matches!(predicate, ExpressionTerm::NamedNode(_)) {
            return None;
        }
        Some(Self {
            subject: match subject {
                ExpressionTerm::NamedNode(s) => NamedOrBlankNode::NamedNode(s),
                ExpressionTerm::BlankNode(s) => NamedOrBlankNode::BlankNode(s),
                _ => return None,
            },
            predicate: if let ExpressionTerm::NamedNode(p) = predicate {
                p
            } else {
                return None;
            },
            object,
        })
    }
}

#[cfg(feature = "sparql-12")]
impl From<NamedOrBlankNode> for ExpressionTerm {
    #[inline]
    fn from(subject: NamedOrBlankNode) -> Self {
        match subject {
            NamedOrBlankNode::NamedNode(s) => Self::NamedNode(s),
            NamedOrBlankNode::BlankNode(s) => Self::BlankNode(s),
        }
    }
}

#[doc(hidden)]
#[derive(Clone)]
pub enum TermCow<'a> {
    Owned(Term),
    Borrowed(TermRef<'a>),
}

impl PartialEq for TermCow<'_> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        TermRef::from(self) == TermRef::from(other)
    }
}

impl Eq for TermCow<'_> {}

impl Hash for TermCow<'_> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        TermRef::from(self).hash(state)
    }
}

impl From<Term> for TermCow<'_> {
    fn from(value: Term) -> Self {
        Self::Owned(value)
    }
}

impl<'a> From<TermRef<'a>> for TermCow<'a> {
    fn from(value: TermRef<'a>) -> Self {
        Self::Borrowed(value)
    }
}

impl From<TermCow<'_>> for Term {
    fn from(value: TermCow<'_>) -> Self {
        match value {
            TermCow::Owned(t) => t,
            TermCow::Borrowed(t) => t.into_owned(),
        }
    }
}

impl<'a> From<&'a TermCow<'a>> for TermRef<'a> {
    fn from(value: &'a TermCow<'a>) -> Self {
        match value {
            TermCow::Owned(t) => t.as_ref(),
            TermCow::Borrowed(t) => *t,
        }
    }
}

/// The lexical form Jena gives a computed `xsd:decimal`: Oxigraph's canonical form, with `.0`
/// after a whole number ("2573.0", not "2573"). TripleTrail patch: the rail KG rules mint IRIs from
/// `STR` of computed decimals, and follow Fuseki where the forms differ.
pub fn decimal_lexical(value: Decimal) -> String {
    let mut s = value.to_string();
    if !s.contains('.') {
        s.push_str(".0");
    }
    s
}

/// Fraction digits Jena keeps when a decimal quotient does not end (`XSDFuncOp.DIVIDE_PRECISION`).
const DIVIDE_PRECISION: usize = 24;

/// tripletrail: decimal division as Jena does it (`XSDFuncOp.decimalDivide`), for `/` on
/// integers and decimals and for `AVG`. The quotient is exact when it ends; otherwise it is
/// rounded half to even at 24 fraction digits. A quotient that `Decimal` holds (at most 18
/// fraction digits) is a [`ExpressionTerm::DecimalLiteral`]; a longer one keeps its digits as a
/// `xsd:decimal` [`ExpressionTerm::OtherTypedLiteral`], whose value (see [`ExpressionTerm::value`])
/// is rounded to 18 fraction digits. `None` when the divisor is zero.
pub fn decimal_quotient(dividend: Decimal, divisor: Decimal) -> Option<ExpressionTerm> {
    let (negative, integer, digits) = quotient_digits(dividend, divisor)?;
    if digits.len() <= 18 {
        // Most quotients: one `Decimal`, built without text.
        let mut value = i128::try_from(integer).ok();
        for i in 0..18 {
            let digit = digits.get(i).copied().unwrap_or(0);
            value = value
                .and_then(|v| v.checked_mul(10))
                .and_then(|v| v.checked_add(i128::from(digit)));
        }
        if let Some(value) = value {
            let value = if negative { -value } else { value };
            return Some(ExpressionTerm::DecimalLiteral(Decimal::from_be_bytes(
                value.to_be_bytes(),
            )));
        }
    }
    let mut s = String::with_capacity(digits.len() + 42);
    if negative {
        s.push('-');
    }
    s.push_str(&integer.to_string());
    s.push('.');
    if digits.is_empty() {
        s.push('0');
    }
    s.extend(digits.iter().map(|d| char::from(b'0' + d)));
    Some(ExpressionTerm::OtherTypedLiteral {
        value: s,
        datatype: oxrdf::vocab::xsd::DECIMAL.into_owned(),
    })
}

/// The sign, integer part and fraction digits (without trailing zeros) of Jena's decimal
/// quotient (see [`decimal_quotient`]); the sign is never negative for zero.
fn quotient_digits(dividend: Decimal, divisor: Decimal) -> Option<(bool, u128, Vec<u8>)> {
    // Both are integers scaled by the same power of ten, so the quotient of the scaled
    // integers is the quotient of the decimals.
    let a = i128::from_be_bytes(dividend.to_be_bytes());
    let b = i128::from_be_bytes(divisor.to_be_bytes());
    if b == 0 {
        return None;
    }
    let negative = (a < 0) != (b < 0);
    let (a, b) = (a.unsigned_abs(), b.unsigned_abs());
    let mut integer = a / b;
    let mut rest = a % b;
    // The next digit of rest / b: rest < b <= 2^127, so a sum of two values below b fits.
    let next_digit = |rest: &mut u128| {
        let (mut digit, mut acc) = (0_u8, 0_u128);
        for _ in 0..10 {
            acc += *rest;
            if acc >= b {
                acc -= b;
                digit += 1;
            }
        }
        *rest = acc;
        digit
    };
    let mut digits = Vec::new();
    while rest != 0 && digits.len() < DIVIDE_PRECISION {
        digits.push(next_digit(&mut rest));
    }
    if rest != 0 {
        // a / b ends exactly when the part of b without the factors 2 and 5 divides a.
        let mut odd = b >> b.trailing_zeros();
        while odd % 5 == 0 {
            odd /= 5;
        }
        if a % odd == 0 {
            while rest != 0 {
                digits.push(next_digit(&mut rest));
            }
        } else if next_digit(&mut rest) >= 5 {
            // A quotient that does not end is never exactly half way, so this is half to even.
            let mut carry = true;
            for digit in digits.iter_mut().rev() {
                if *digit == 9 {
                    *digit = 0;
                } else {
                    *digit += 1;
                    carry = false;
                    break;
                }
            }
            if carry {
                integer += 1;
            }
        }
    }
    while digits.last() == Some(&0) {
        digits.pop();
    }
    Some((negative && (integer != 0 || !digits.is_empty()), integer, digits))
}

/// tripletrail: the value of a `xsd:decimal` with more fraction digits than `Decimal` holds
/// (a quotient of [`decimal_quotient`], or a stored literal), rounded half to even at 18.
fn rounded_decimal(value: &str) -> Option<Decimal> {
    const KEPT: usize = 18;
    let (int_part, fraction) = value.split_once('.')?;
    let fraction = fraction.trim_end_matches('0');
    if fraction.len() <= KEPT || !fraction.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (kept, dropped) = fraction.split_at(KEPT);
    let truncated: Decimal = format!("{int_part}.{kept}").parse().ok()?;
    let dropped = dropped.as_bytes();
    let up = dropped[0] > b'5'
        || (dropped[0] == b'5'
            && (dropped[1..].iter().any(|&c| c != b'0') || kept.as_bytes()[KEPT - 1] % 2 == 1));
    if !up {
        return Some(truncated);
    }
    let ulp = Decimal::new(1, 18).ok()?;
    if int_part.starts_with('-') {
        truncated.checked_sub(ulp)
    } else {
        truncated.checked_add(ulp)
    }
}

/// A computed `xsd:decimal` as a literal, in Jena's form (see [`decimal_lexical`]).
pub fn decimal_literal(value: Decimal) -> Literal {
    Literal::new_typed_literal(decimal_lexical(value), oxrdf::vocab::xsd::DECIMAL)
}
