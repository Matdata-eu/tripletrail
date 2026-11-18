//! A reader for RDF4J's binary RDF format (`application/x-binary-rdf`, versions 1 and 2), which
//! RDF4J's `HTTPRepository` uploads statements in by default (#223).
//!
//! The document is `BRDF`, a 32-bit version (2: then the name of the string charset), then
//! records: namespace declarations, comments, value declarations (an id and a value, referred
//! to later by id) and statements (subject, predicate, object, context), up to an end record.
//! Version 1 writes ids as 32-bit integers and strings as a 32-bit length in UTF-16 code units
//! followed by UTF-16BE; version 2 writes both lengths and ids as variable-length integers (7
//! bits a byte, low bits first) and strings in the declared charset (UTF-8).

use tripletrail::oxigraph::model::{
    BlankNode, GraphName, Literal, NamedNode, NamedOrBlankNode, Quad, Term,
};

pub const MEDIA_TYPE: &str = "application/x-binary-rdf";

const NAMESPACE_DECL: u8 = 0;
const STATEMENT: u8 = 1;
const COMMENT: u8 = 2;
const VALUE_DECL: u8 = 3;
const END_OF_DATA: u8 = 127;

const NULL_VALUE: u8 = 0;
const URI_VALUE: u8 = 1;
const BNODE_VALUE: u8 = 2;
const PLAIN_LITERAL_VALUE: u8 = 3;
const LANG_LITERAL_VALUE: u8 = 4;
const DATATYPE_LITERAL_VALUE: u8 = 5;
const VALUE_REF: u8 = 6;
const TRIPLE_VALUE: u8 = 7;

/// Most values one document may declare, so a hostile id cannot make it allocate gigabytes.
const MAX_DECLARED: usize = 1 << 24;

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
    version: u32,
    declared: Vec<Option<Term>>,
}

/// The statements of a binary RDF document.
pub fn parse(data: &[u8]) -> Result<Vec<Quad>, String> {
    if data.get(..4) != Some(b"BRDF".as_slice()) {
        return Err("Not a binary RDF document".to_owned());
    }
    let mut r = Reader {
        data,
        at: 4,
        version: 0,
        declared: Vec::new(),
    };
    r.version = r.u32()?;
    match r.version {
        1 => {}
        2 => {
            let charset = r.string()?;
            if !charset.eq_ignore_ascii_case("UTF-8") {
                return Err(format!("Unsupported binary RDF charset {charset}"));
            }
        }
        v => return Err(format!("Unsupported binary RDF version {v}")),
    }
    let mut quads = Vec::new();
    loop {
        match r.byte()? {
            END_OF_DATA => return Ok(quads),
            NAMESPACE_DECL => {
                r.string()?;
                r.string()?;
            }
            COMMENT => {
                r.string()?;
            }
            VALUE_DECL => {
                let id = r.id()?;
                if id >= MAX_DECLARED {
                    return Err(format!("Value id {id} is too large"));
                }
                let value = r.value()?;
                if r.declared.len() <= id {
                    r.declared.resize(id + 1, None);
                }
                r.declared[id] = value;
            }
            STATEMENT => {
                let subject = match r.value()? {
                    Some(Term::NamedNode(n)) => NamedOrBlankNode::NamedNode(n),
                    Some(Term::BlankNode(b)) => NamedOrBlankNode::BlankNode(b),
                    other => return Err(format!("Invalid subject {other:?}")),
                };
                let predicate = match r.value()? {
                    Some(Term::NamedNode(n)) => n,
                    other => return Err(format!("Invalid predicate {other:?}")),
                };
                let object = r.value()?.ok_or("A statement without an object")?;
                let graph = match r.value()? {
                    None => GraphName::DefaultGraph,
                    Some(Term::NamedNode(n)) => GraphName::NamedNode(n),
                    Some(Term::BlankNode(b)) => GraphName::BlankNode(b),
                    Some(other) => return Err(format!("Invalid context {other}")),
                };
                quads.push(Quad::new(subject, predicate, object, graph));
            }
            other => return Err(format!("Invalid binary RDF record type {other}")),
        }
    }
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        let end = self
            .at
            .checked_add(n)
            .filter(|&e| e <= self.data.len())
            .ok_or("Truncated binary RDF document")?;
        let bytes = &self.data[self.at..end];
        self.at = end;
        Ok(bytes)
    }

    fn byte(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, String> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn var_int(&mut self) -> Result<usize, String> {
        let mut value: usize = 0;
        for shift in (0..35).step_by(7) {
            let b = self.byte()?;
            value |= usize::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err("Variable-length integer too long".to_owned())
    }

    fn id(&mut self) -> Result<usize, String> {
        if self.version == 1 {
            Ok(self.u32()? as usize)
        } else {
            self.var_int()
        }
    }

    fn string(&mut self) -> Result<String, String> {
        if self.version == 1 {
            let units = self.u32()? as usize;
            let bytes = self.take(units.checked_mul(2).ok_or("String too long")?)?;
            let utf16: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16(&utf16).map_err(|e| e.to_string())
        } else {
            let len = self.var_int()?;
            String::from_utf8(self.take(len)?.to_vec()).map_err(|e| e.to_string())
        }
    }

    fn iri(&mut self) -> Result<NamedNode, String> {
        let s = self.string()?;
        NamedNode::new(&s).map_err(|e| format!("Invalid IRI {s}: {e}"))
    }

    fn value(&mut self) -> Result<Option<Term>, String> {
        Ok(Some(match self.byte()? {
            NULL_VALUE => return Ok(None),
            VALUE_REF => {
                let id = self.id()?;
                return self
                    .declared
                    .get(id)
                    .cloned()
                    .ok_or_else(|| format!("Undeclared value {id}"));
            }
            URI_VALUE => self.iri()?.into(),
            BNODE_VALUE => {
                let id = self.string()?;
                BlankNode::new(&id)
                    .unwrap_or_else(|_| {
                        let hex: String = id.bytes().map(|b| format!("{b:02x}")).collect();
                        BlankNode::new_unchecked(format!("x{hex}"))
                    })
                    .into()
            }
            PLAIN_LITERAL_VALUE => Literal::new_simple_literal(self.string()?).into(),
            LANG_LITERAL_VALUE => {
                let value = self.string()?;
                let language = self.string()?;
                // RDF4J keeps a malformed tag (`en_us`) as it was given, and so does TripleTrail.
                Literal::new_language_tagged_literal(value.clone(), &language)
                    .unwrap_or_else(|_| {
                        Literal::new_language_tagged_literal_unchecked(
                            value,
                            language.to_ascii_lowercase(),
                        )
                    })
                    .into()
            }
            DATATYPE_LITERAL_VALUE => {
                let value = self.string()?;
                Literal::new_typed_literal(value, self.iri()?).into()
            }
            TRIPLE_VALUE => {
                return Err("Triple terms (RDF-star) are not supported in binary RDF".to_owned());
            }
            other => return Err(format!("Unknown binary RDF value type {other}")),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2_string(out: &mut Vec<u8>, s: &str) {
        let mut n = s.len();
        while n > 127 {
            out.push((n & 0x7f) as u8 | 0x80);
            n >>= 7;
        }
        out.push(n as u8);
        out.extend_from_slice(s.as_bytes());
    }

    #[test]
    fn reads_versions_one_and_two() {
        // Version 2: a declared value referred to twice, a language literal, a context.
        let mut d = b"BRDF".to_vec();
        d.extend_from_slice(&2u32.to_be_bytes());
        v2_string(&mut d, "UTF-8");
        d.push(NAMESPACE_DECL);
        v2_string(&mut d, "ex");
        v2_string(&mut d, "urn:ex:");
        d.extend_from_slice(&[VALUE_DECL, 200, 1, URI_VALUE]); // id 200, varint
        v2_string(&mut d, "urn:ex:s");
        d.extend_from_slice(&[STATEMENT, VALUE_REF, 200, 1, URI_VALUE]);
        v2_string(&mut d, "urn:ex:p");
        d.push(LANG_LITERAL_VALUE);
        v2_string(&mut d, &"é".repeat(100));
        v2_string(&mut d, "nl");
        d.push(URI_VALUE);
        v2_string(&mut d, "urn:ex:g");
        d.extend_from_slice(&[STATEMENT, VALUE_REF, 200, 1, URI_VALUE]);
        v2_string(&mut d, "urn:ex:p");
        d.push(BNODE_VALUE);
        v2_string(&mut d, "node-1");
        d.extend_from_slice(&[NULL_VALUE, END_OF_DATA]);
        let quads = parse(&d).unwrap();
        assert_eq!(quads.len(), 2);
        assert_eq!(quads[0].subject.to_string(), "<urn:ex:s>");
        assert_eq!(
            quads[0].object.to_string(),
            format!("\"{}\"@nl", "é".repeat(100))
        );
        assert_eq!(quads[0].graph_name.to_string(), "<urn:ex:g>");
        assert_eq!(quads[1].graph_name, GraphName::DefaultGraph);

        // Version 1: 32-bit ids, UTF-16 strings.
        let s1 = |out: &mut Vec<u8>, s: &str| {
            let units: Vec<u16> = s.encode_utf16().collect();
            out.extend_from_slice(&(units.len() as u32).to_be_bytes());
            for u in units {
                out.extend_from_slice(&u.to_be_bytes());
            }
        };
        let mut d = b"BRDF".to_vec();
        d.extend_from_slice(&1u32.to_be_bytes());
        d.extend_from_slice(&[STATEMENT, URI_VALUE]);
        s1(&mut d, "urn:ex:s");
        d.push(URI_VALUE);
        s1(&mut d, "urn:ex:p");
        d.push(DATATYPE_LITERAL_VALUE);
        s1(&mut d, "42");
        s1(&mut d, "http://www.w3.org/2001/XMLSchema#integer");
        d.extend_from_slice(&[NULL_VALUE, END_OF_DATA]);
        let quads = parse(&d).unwrap();
        assert_eq!(
            quads[0].object.to_string(),
            "\"42\"^^<http://www.w3.org/2001/XMLSchema#integer>"
        );

        assert!(parse(b"BRDF\0\0\0\x02").is_err());
        assert!(parse(b"nope").is_err());
    }
}
