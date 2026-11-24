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
            ExpressionTerm::DecimalLiteral(parse_decimal_rounded(value)?)
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

/// tripletrail: `a * b` on decimals as Jena multiplies them (`BigDecimal.multiply`): exact. A
/// product that fits [`Decimal`] (18 fraction digits) is a decimal value; a longer one (up to 36
/// fraction digits) is the literal with all its digits, as a long quotient is (see
/// [`decimal_divide`]). `None` when the whole part of the product doesn't fit 128 bits.
pub fn decimal_multiply(a: Decimal, b: Decimal) -> Option<ExpressionTerm> {
    if let Some(p) = a.checked_mul(b) {
        return Some(ExpressionTerm::DecimalLiteral(p));
    }
    let (ra, rb) = (
        i128::from_be_bytes(a.to_be_bytes()),
        i128::from_be_bytes(b.to_be_bytes()),
    );
    let negative = (ra < 0) != (rb < 0);
    // The product of the scaled values is the product times 10^36.
    let (high, low) = wide_mul(ra.unsigned_abs(), rb.unsigned_abs());
    const E18: u128 = 1_000_000_000_000_000_000;
    let mut limbs = [
        low as u64,
        (low >> 64) as u64,
        high as u64,
        (high >> 64) as u64,
    ];
    let mut fraction = [0_u128; 2];
    for part in fraction.iter_mut().rev() {
        let mut rest = 0_u128;
        for limb in limbs.iter_mut().rev() {
            let current = (rest << 64) | u128::from(*limb);
            *limb = (current / E18) as u64;
            rest = current % E18;
        }
        *part = rest;
    }
    if limbs[2] != 0 || limbs[3] != 0 {
        return None;
    }
    let whole = (u128::from(limbs[1]) << 64) | u128::from(limbs[0]);
    let digits = format!("{:018}{:018}", fraction[0], fraction[1]);
    let digits = digits.trim_end_matches('0');
    let mut text = String::with_capacity(digits.len() + 42);
    if negative {
        text.push('-');
    }
    text.push_str(&whole.to_string());
    text.push('.');
    text.push_str(if digits.is_empty() { "0" } else { digits });
    Some(match text.parse::<Decimal>() {
        Ok(value) => ExpressionTerm::DecimalLiteral(value),
        Err(_) => ExpressionTerm::OtherTypedLiteral {
            value: text,
            datatype: oxrdf::vocab::xsd::DECIMAL.into(),
        },
    })
}

/// Fraction digits Jena keeps in a decimal division that does not end (`XSDFuncOp`,
/// `DIVIDE_PRECISION`).
const DIVIDE_FRACTION_DIGITS: usize = 24;

/// tripletrail (#8): `a / b` on decimals as Jena divides them (`XSDFuncOp.decimalDivide`): the
/// exact quotient when its decimal expansion ends, else the quotient rounded half to even to 24
/// fraction digits (`1.0 / 3.0` is `0.333333333333333333333333`, `2.0 / 3.0` ends in `7`).
/// A quotient that fits [`Decimal`] (18 fraction digits) is a decimal value; a longer one is the
/// literal with all its digits, whose value in further arithmetic is rounded to 18 fraction
/// digits (see [`parse_decimal_rounded`]). `None` when `b` is zero.
pub fn decimal_divide(a: Decimal, b: Decimal) -> Option<ExpressionTerm> {
    // Both are integers scaled by 10^18, so the quotient of the scaled values is the quotient.
    let (ra, rb) = (
        i128::from_be_bytes(a.to_be_bytes()),
        i128::from_be_bytes(b.to_be_bytes()),
    );
    if rb == 0 {
        return None;
    }
    if ra == 0 {
        return Some(ExpressionTerm::DecimalLiteral(Decimal::from(0)));
    }
    // The common case: the evaluator's own quotient (18 fraction digits, cut) is exact, which
    // `q * b == a` checked at full width shows. About 0.05 µs instead of the long division.
    if let Some(q) = a.checked_div(b) {
        let rq = i128::from_be_bytes(q.to_be_bytes());
        if (rq < 0) == ((ra < 0) != (rb < 0))
            && rq != 0
            && wide_mul(rq.unsigned_abs(), rb.unsigned_abs())
                == wide_mul(ra.unsigned_abs(), 1_000_000_000_000_000_000)
        {
            return Some(ExpressionTerm::DecimalLiteral(q));
        }
    }
    let negative = (ra < 0) != (rb < 0);
    let (mut n, mut d) = (ra.unsigned_abs(), rb.unsigned_abs());
    let g = gcd(n, d);
    n /= g;
    d /= g;
    // The expansion ends when the reduced divisor has no prime factors other than 2 and 5, after
    // as many digits as the larger of the two exponents.
    let (mut rest, mut twos, mut fives) = (d, 0_usize, 0_usize);
    while rest % 2 == 0 {
        rest /= 2;
        twos += 1;
    }
    while rest % 5 == 0 {
        rest /= 5;
        fives += 1;
    }
    let ends = rest == 1;
    let count = if ends {
        twos.max(fives)
    } else {
        DIVIDE_FRACTION_DIGITS
    };
    let mut whole = n / d;
    let mut remainder = n % d;
    let mut digits = Vec::with_capacity(count);
    for _ in 0..count {
        // 10 * remainder by ten additions, each below 2 * d <= 2^128, so any divisor fits.
        let (mut digit, mut r) = (0_u8, 0_u128);
        for _ in 0..10 {
            r += remainder;
            if r >= d {
                r -= d;
                digit += 1;
            }
        }
        digits.push(digit);
        remainder = r;
    }
    // Half to even; an expansion that does not end is never exactly half way.
    if !ends && remainder > d - remainder {
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
            whole = whole.checked_add(1)?;
        }
    }
    while digits.last() == Some(&0) {
        digits.pop();
    }
    let mut text = String::with_capacity(digits.len() + 42);
    if negative && (whole != 0 || !digits.is_empty()) {
        text.push('-');
    }
    text.push_str(&whole.to_string());
    text.push('.');
    if digits.is_empty() {
        text.push('0');
    } else {
        text.extend(digits.iter().map(|d| char::from(b'0' + d)));
    }
    Some(match text.parse::<Decimal>() {
        Ok(value) => ExpressionTerm::DecimalLiteral(value),
        Err(_) => ExpressionTerm::OtherTypedLiteral {
            value: text,
            datatype: oxrdf::vocab::xsd::DECIMAL.into(),
        },
    })
}

/// `a * b` as 256 bits, (high, low).
fn wide_mul(a: u128, b: u128) -> (u128, u128) {
    const M: u128 = u64::MAX as u128;
    let (a1, a0, b1, b0) = (a >> 64, a & M, b >> 64, b & M);
    let (lo, mid1, mid2, hi) = (a0 * b0, a0 * b1, a1 * b0, a1 * b1);
    let (mid, carry) = mid1.overflowing_add(mid2);
    let (low, c2) = lo.overflowing_add(mid << 64);
    let high = hi + (mid >> 64) + (u128::from(carry) << 64) + u128::from(c2);
    (high, low)
}

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// tripletrail (#8): the value of an `xsd:decimal` lexical form, rounded half to even to the 18
/// fraction digits of [`Decimal`] when it has more (a quotient as Jena writes it, or data written
/// by Jena). Upstream refused such a form, so the literal was no number at all.
pub(crate) fn parse_decimal_rounded(value: &str) -> Option<Decimal> {
    if let Ok(d) = value.parse() {
        return Some(d);
    }
    let (head, fraction) = value.split_once('.')?;
    let (kept, dropped) = fraction.split_at_checked(18)?;
    if !dropped.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let truncated: Decimal = format!("{head}.{kept}").parse().ok()?;
    let first = dropped.as_bytes()[0];
    let tail_zero = dropped.bytes().skip(1).all(|c| c == b'0');
    let last_odd = kept.as_bytes().last().is_some_and(|c| (c - b'0') % 2 == 1)
        || (kept.is_empty() && head.bytes().last().is_some_and(|c| (c - b'0') % 2 == 1));
    let up = first > b'5' || (first == b'5' && (!tail_zero || last_odd));
    if !up {
        return Some(truncated);
    }
    let step = Decimal::new(1, 18).ok()?;
    if head.starts_with('-') {
        truncated.checked_sub(step)
    } else {
        truncated.checked_add(step)
    }
}

/// A computed `xsd:decimal` as a literal, in Jena's form (see [`decimal_lexical`]).
pub fn decimal_literal(value: Decimal) -> Literal {
    Literal::new_typed_literal(decimal_lexical(value), oxrdf::vocab::xsd::DECIMAL)
}

#[cfg(test)]
mod decimal_divide_tests {
    use super::*;

    fn text(t: ExpressionTerm) -> String {
        match t {
            ExpressionTerm::DecimalLiteral(d) => decimal_lexical(d),
            ExpressionTerm::OtherTypedLiteral { value, .. } => value,
            _ => panic!("not a decimal"),
        }
    }

    #[test]
    fn wide_mul_is_exact() {
        assert_eq!(wide_mul(u128::MAX, u128::MAX), (u128::MAX - 1, 1));
        assert_eq!(wide_mul(1 << 64, 1 << 64), (1, 0));
        assert_eq!(wide_mul(12345, 678), (0, 12345 * 678));
    }

    /// The fast path and the long division agree, on exact and inexact quotients alike.
    #[test]
    fn fast_and_long_quotients_agree() {
        let d = |s: &str| s.parse::<Decimal>().unwrap();
        for (a, b, want) in [
            ("1", "4", "0.25"),
            ("-7.5", "2.5", "-3.0"),
            (
                "170141183460469231731.687303715884105727",
                "1",
                "170141183460469231731.687303715884105727",
            ),
            ("1", "3", "0.333333333333333333333333"),
            ("2", "-3", "-0.666666666666666666666667"),
            (
                "1",
                "1099511627776",
                "0.0000000000009094947017729282379150390625",
            ),
            ("0.000000000000000001", "2", "0.0000000000000000005"),
            ("123456789", "0.001", "123456789000.0"),
        ] {
            assert_eq!(text(decimal_divide(d(a), d(b)).unwrap()), want, "{a} / {b}");
        }
    }
}
