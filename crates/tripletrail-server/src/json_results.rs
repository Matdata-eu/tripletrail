//! A streaming writer of SPARQL 1.1 (and 1.2) query results in JSON for solutions.
//!
//! The general writer (`sparesults`, through `json-event-parser`) turns each term into a list
//! of events and escapes strings one character at a time; for large results it takes about
//! twice as long as evaluating the query. This one writes each term directly and copies the
//! runs of a string that need no escaping in one piece. The output is the same document.

use std::io::{self, Write};
use tripletrail::oxigraph::model::{BaseDirection, TermRef};
use tripletrail::oxigraph::sparql::{QuerySolution, Variable};

/// Writes `{"head":{"vars":[…]},"results":{"bindings":[…]}}`, one solution at a time.
pub(crate) struct JsonSolutionsWriter<W: Write> {
    out: W,
    first: bool,
}

impl<W: Write> JsonSolutionsWriter<W> {
    pub(crate) fn start(mut out: W, variables: &[Variable]) -> io::Result<Self> {
        out.write_all(b"{\"head\":{\"vars\":[")?;
        for (i, v) in variables.iter().enumerate() {
            if i > 0 {
                out.write_all(b",")?;
            }
            write_string(&mut out, v.as_str())?;
        }
        out.write_all(b"]},\"results\":{\"bindings\":[")?;
        Ok(Self { out, first: true })
    }

    pub(crate) fn serialize(&mut self, solution: &QuerySolution) -> io::Result<()> {
        let out = &mut self.out;
        out.write_all(if self.first { b"{" } else { b",{" })?;
        self.first = false;
        for (i, (v, t)) in solution.iter().enumerate() {
            if i > 0 {
                out.write_all(b",")?;
            }
            write_string(out, v.as_str())?;
            out.write_all(b":")?;
            write_term(out, t.as_ref())?;
        }
        out.write_all(b"}")
    }

    pub(crate) fn finish(mut self) -> io::Result<W> {
        self.out.write_all(b"]}}")?;
        Ok(self.out)
    }
}

pub(crate) fn write_term(out: &mut impl Write, term: TermRef<'_>) -> io::Result<()> {
    match term {
        TermRef::NamedNode(n) => {
            out.write_all(b"{\"type\":\"uri\",\"value\":")?;
            write_string(out, n.as_str())?;
        }
        TermRef::BlankNode(b) => {
            out.write_all(b"{\"type\":\"bnode\",\"value\":")?;
            write_string(out, b.as_str())?;
        }
        TermRef::Literal(l) => {
            out.write_all(b"{\"type\":\"literal\",\"value\":")?;
            write_string(out, l.value())?;
            if let Some(language) = l.language() {
                out.write_all(b",\"xml:lang\":")?;
                write_string(out, language)?;
                if let Some(direction) = l.direction() {
                    out.write_all(match direction {
                        BaseDirection::Ltr => b",\"its:dir\":\"ltr\"",
                        BaseDirection::Rtl => b",\"its:dir\":\"rtl\"",
                    })?;
                }
            } else if l.datatype() != tripletrail::oxigraph::model::vocab::xsd::STRING {
                out.write_all(b",\"datatype\":")?;
                write_string(out, l.datatype().as_str())?;
            }
        }
        TermRef::Triple(t) => {
            out.write_all(b"{\"type\":\"triple\",\"value\":{\"subject\":")?;
            write_term(out, t.subject.as_ref().into())?;
            out.write_all(b",\"predicate\":")?;
            write_term(out, t.predicate.as_ref().into())?;
            out.write_all(b",\"object\":")?;
            write_term(out, t.object.as_ref())?;
            out.write_all(b"}")?;
        }
    }
    out.write_all(b"}")
}

/// `s` as a JSON string.
pub(crate) fn write_string(out: &mut impl Write, s: &str) -> io::Result<()> {
    out.write_all(b"\"")?;
    let bytes = s.as_bytes();
    // Most values (IRIs, numbers, words) need no escaping: one test over the whole string,
    // without an early exit so that it runs on whole vectors.
    let plain = bytes.iter().fold(true, |plain, &b| {
        plain & (b >= 0x20 && b != b'"' && b != b'\\')
    });
    if plain {
        out.write_all(bytes)?;
        return out.write_all(b"\"");
    }
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let escaped: &[u8] = match b {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0x08 => b"\\b",
            0x0c => b"\\f",
            0..0x20 => {
                out.write_all(&bytes[start..i])?;
                write!(out, "\\u{b:04X}")?;
                start = i + 1;
                continue;
            }
            _ => continue,
        };
        out.write_all(&bytes[start..i])?;
        out.write_all(escaped)?;
        start = i + 1;
    }
    out.write_all(&bytes[start..])?;
    out.write_all(b"\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tripletrail::oxigraph::model::{BlankNode, Literal, NamedNode, Term, Triple};
    use tripletrail::oxigraph::sparql::results::{QueryResultsFormat, QueryResultsSerializer};

    /// The same document as the general writer, also for strings that need escaping, unbound
    /// cells, language strings with a direction and quoted triples.
    #[test]
    fn writes_what_sparesults_writes() {
        let vars: Arc<[Variable]> = vec![
            Variable::new_unchecked("a"),
            Variable::new_unchecked("b"),
            Variable::new_unchecked("c"),
        ]
        .into();
        let iri = |s: &str| Term::from(NamedNode::new_unchecked(s));
        let rows: Vec<Vec<Option<Term>>> = vec![
            vec![
                Some(iri("http://ex.org/a?x=\"1\"")),
                Some(Literal::new_simple_literal("tab\there \"q\" \\ \u{1} é 𝄞\n").into()),
                None,
            ],
            vec![None, None, None],
            vec![
                Some(BlankNode::new_unchecked("b0").into()),
                Some(Literal::new_language_tagged_literal_unchecked("hi", "en-GB").into()),
                Some(Literal::from(42).into()),
            ],
            vec![
                Some(
                    Literal::new_directional_language_tagged_literal_unchecked(
                        "x",
                        "ar",
                        BaseDirection::Rtl,
                    )
                    .into(),
                ),
                Some(
                    Triple::new(
                        NamedNode::new_unchecked("http://ex.org/s"),
                        NamedNode::new_unchecked("http://ex.org/p"),
                        Literal::new_typed_literal(
                            "1.5",
                            tripletrail::oxigraph::model::vocab::xsd::DECIMAL,
                        ),
                    )
                    .into(),
                ),
                Some(Literal::new_simple_literal("").into()),
            ],
        ];
        let solutions: Vec<QuerySolution> = rows
            .into_iter()
            .map(|r| QuerySolution::from((Arc::clone(&vars), r)))
            .collect();
        let mut expected = Vec::new();
        let mut general = QueryResultsSerializer::from_format(QueryResultsFormat::Json)
            .serialize_solutions_to_writer(&mut expected, vars.to_vec())
            .unwrap();
        for s in &solutions {
            general.serialize(s).unwrap();
        }
        general.finish().unwrap();
        let mut ours = JsonSolutionsWriter::start(Vec::new(), &vars).unwrap();
        for s in &solutions {
            ours.serialize(s).unwrap();
        }
        let ours = ours.finish().unwrap();
        let parse = |b: &[u8]| serde_json::from_slice::<serde_json::Value>(b).unwrap();
        assert_eq!(parse(&ours), parse(&expected));
        // No solutions.
        let empty = JsonSolutionsWriter::start(Vec::new(), &vars)
            .unwrap()
            .finish()
            .unwrap();
        assert_eq!(
            parse(&empty),
            serde_json::json!({"head": {"vars": ["a", "b", "c"]}, "results": {"bindings": []}})
        );
    }
}
