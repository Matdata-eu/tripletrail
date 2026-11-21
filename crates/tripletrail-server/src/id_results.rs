//! SPARQL JSON, CSV and TSV results written straight from an answer on term ids (#11): each
//! distinct term of the answer is written once, and each row copies the bytes of its terms.
//! The output is byte for byte what the writers of solutions give (`json_results` and
//! `sparesults`).

use crate::json_results;
use std::io::{self, Write};
use std::sync::Arc;
use tripletrail::IdSolutions;
use tripletrail::oxigraph::sparql::results::{
    QueryResultsFormat, QueryResultsSerializer, write_csv_term, write_tsv_term,
};

/// Writes the rows of an [`IdSolutions`] one at a time.
pub(crate) struct IdResultsWriter<W: Write> {
    out: W,
    answer: Arc<IdSolutions>,
    format: QueryResultsFormat,
    /// The bytes of term `i` are `bytes[ends[i - 1]..ends[i]]`.
    bytes: Vec<u8>,
    ends: Vec<usize>,
    /// JSON: `"name":` per variable.
    names: Vec<Vec<u8>>,
    first: bool,
}

impl<W: Write> IdResultsWriter<W> {
    /// A writer of `answer` in `format`, with its head written; `None` for a format it
    /// doesn't write.
    pub(crate) fn start(
        mut out: W,
        answer: Arc<IdSolutions>,
        format: QueryResultsFormat,
    ) -> io::Result<Option<Self>> {
        let variables = answer.variables().to_vec();
        match format {
            QueryResultsFormat::Json => {
                let mut head = Vec::new();
                json_results::JsonSolutionsWriter::start(&mut head, &variables)?;
                out.write_all(&head)?;
            }
            QueryResultsFormat::Csv | QueryResultsFormat::Tsv => {
                // The head is what the serializer writes before the first row.
                let mut head = Vec::new();
                QueryResultsSerializer::from_format(format)
                    .serialize_solutions_to_writer(&mut head, variables.clone())?
                    .finish()?;
                out.write_all(&head)?;
            }
            _ => return Ok(None),
        }
        let terms = answer.terms();
        // The terms' bytes, in chunks on the query threads.
        let parts = tripletrail::store::par::chunks(terms.len(), |range| {
            let mut bytes = Vec::new();
            let mut ends = Vec::with_capacity(range.len());
            let mut text = String::new();
            for term in &terms[range] {
                match format {
                    QueryResultsFormat::Json => {
                        json_results::write_term(&mut bytes, term.as_ref())
                            .expect("writes to memory");
                    }
                    QueryResultsFormat::Csv => {
                        text.clear();
                        write_csv_term(&mut text, term);
                        bytes.extend_from_slice(text.as_bytes());
                    }
                    _ => {
                        text.clear();
                        write_tsv_term(&mut text, term);
                        bytes.extend_from_slice(text.as_bytes());
                    }
                }
                ends.push(bytes.len());
            }
            (bytes, ends)
        });
        let mut bytes = Vec::with_capacity(parts.iter().map(|(b, _)| b.len()).sum());
        let mut ends = Vec::with_capacity(terms.len());
        for (b, e) in parts {
            let base = bytes.len();
            bytes.extend_from_slice(&b);
            ends.extend(e.into_iter().map(|end| base + end));
        }
        let names = variables
            .iter()
            .map(|v| {
                let mut name = Vec::new();
                json_results::write_string(&mut name, v.as_str()).expect("writes to memory");
                name.push(b':');
                name
            })
            .collect();
        Ok(Some(Self {
            out,
            answer,
            format,
            bytes,
            ends,
            names,
            first: true,
        }))
    }

    fn term(&self, n: u32) -> &[u8] {
        let n = n as usize;
        let start = if n == 0 { 0 } else { self.ends[n - 1] };
        &self.bytes[start..self.ends[n]]
    }

    /// Writes row `i`.
    pub(crate) fn row(&mut self, i: usize) -> io::Result<()> {
        let answer = Arc::clone(&self.answer);
        let cells = answer.row(i);
        let mut line = Vec::with_capacity(64);
        match self.format {
            QueryResultsFormat::Json => {
                line.extend_from_slice(if self.first { b"{" } else { b",{" });
                let mut bound = false;
                for (name, &n) in self.names.iter().zip(cells) {
                    if n == IdSolutions::UNBOUND {
                        continue;
                    }
                    if bound {
                        line.push(b',');
                    }
                    bound = true;
                    line.extend_from_slice(name);
                    line.extend_from_slice(self.term(n));
                }
                line.push(b'}');
            }
            format => {
                let (separator, end): (u8, &[u8]) = if format == QueryResultsFormat::Csv {
                    (b',', b"\r\n")
                } else {
                    (b'\t', b"\n")
                };
                for (k, &n) in cells.iter().enumerate() {
                    if k > 0 {
                        line.push(separator);
                    }
                    if n != IdSolutions::UNBOUND {
                        line.extend_from_slice(self.term(n));
                    }
                }
                line.extend_from_slice(end);
            }
        }
        self.first = false;
        self.out.write_all(&line)
    }

    pub(crate) fn finish(mut self) -> io::Result<()> {
        if self.format == QueryResultsFormat::Json {
            self.out.write_all(b"]}}")?;
        }
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tripletrail::Engine;
    use tripletrail::oxigraph::io::RdfFormat;
    use tripletrail::oxigraph::sparql::{QueryResults, QuerySolution};

    const DATA: &str = r#"
@prefix ex: <http://example.org/> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
ex:a ex:p "plain" , "with \"quotes\", a comma" , "line\nbreak\tand tab" , "x"@en ,
    "y"@en-GB--rtl , "7"^^xsd:integer , "1.50"^^xsd:decimal , "2.5e3"^^xsd:double ,
    "2026-11-21"^^xsd:date , "odd"^^ex:dt , _:b , ex:o , "ünïcödé \u0001" , "" ,
    "comma,only" , "  spaced  " , "true"^^xsd:boolean .
ex:b ex:p ex:o .
ex:c ex:q "only q" .
"#;

    /// The answer of `query` on ids, written by [`IdResultsWriter`] and by the writers of
    /// solutions, in `format`.
    fn both(engine: &Engine, query: &str, format: QueryResultsFormat) -> (Vec<u8>, Vec<u8>) {
        let query = format!("#pragma tt.executor ids\nPREFIX ex: <http://example.org/>\n{query}");
        let QueryResults::Solutions(solutions) = engine.query(&query).unwrap() else {
            panic!("not a SELECT")
        };
        let answer = Arc::clone(solutions.attachment().expect("answered on ids"))
            .downcast::<IdSolutions>()
            .unwrap();
        let mut fast = Vec::new();
        let mut writer = IdResultsWriter::start(&mut fast, Arc::clone(&answer), format)
            .unwrap()
            .unwrap();
        for i in 0..answer.len() {
            writer.row(i).unwrap();
        }
        writer.finish().unwrap();
        let variables: Arc<[_]> = answer.variables().into();
        let rows: Vec<QuerySolution> = Arc::clone(&answer)
            .rows()
            .map(|r| (Arc::clone(&variables), r).into())
            .collect();
        let mut general = Vec::new();
        if format == QueryResultsFormat::Json {
            let mut w =
                json_results::JsonSolutionsWriter::start(&mut general, answer.variables()).unwrap();
            for r in &rows {
                w.serialize(r).unwrap();
            }
            w.finish().unwrap();
        } else {
            let mut w = QueryResultsSerializer::from_format(format)
                .serialize_solutions_to_writer(&mut general, variables.to_vec())
                .unwrap();
            for r in &rows {
                w.serialize(r).unwrap();
            }
            w.finish().unwrap();
        }
        (fast, general)
    }

    #[test]
    fn written_from_ids_as_the_writers_write_solutions() {
        let engine = Engine::new_tripletrail();
        engine.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
        for query in [
            "SELECT * WHERE { ?s ex:p ?o }",
            "SELECT ?o ?s WHERE { ?s ex:p ?o }",
            "SELECT ?s ?o ?v WHERE { ?s ?p ?o OPTIONAL { ?s ex:q ?v } }",
            "SELECT ?s ?nothing WHERE { ?s ex:p ?o }",
            "SELECT DISTINCT ?s WHERE { ?s ?p ?o } ORDER BY DESC(?s)",
            "SELECT * WHERE { ?s ex:missing ?o }",
        ] {
            for format in [
                QueryResultsFormat::Json,
                QueryResultsFormat::Csv,
                QueryResultsFormat::Tsv,
            ] {
                let (fast, general) = both(&engine, query, format);
                assert_eq!(
                    String::from_utf8_lossy(&fast),
                    String::from_utf8_lossy(&general),
                    "{query} {format:?}"
                );
            }
        }
        // Other formats are left to the writers of solutions.
        let QueryResults::Solutions(solutions) = engine
            .query("#pragma tt.executor ids\nSELECT * WHERE { ?s ?p ?o }")
            .unwrap()
        else {
            panic!()
        };
        let answer = Arc::clone(solutions.attachment().unwrap())
            .downcast::<IdSolutions>()
            .unwrap();
        assert!(
            IdResultsWriter::start(Vec::new(), answer, QueryResultsFormat::Xml)
                .unwrap()
                .is_none()
        );
    }
}
