//! Reading a file source's bytes as RML-IO describes them: `rml:compression` (gzip, zip, a
//! tar archive compressed with gzip or xz), `rml:encoding` (UTF-8 or UTF-16) and
//! `rml:null` (values read as no value); and writing a logical target's file.

use crate::{LogicalTarget, RmlError};
use oxigraph::io::{JsonLdProfileSet, RdfFormat, RdfSerializer};
use oxigraph::model::vocab::xsd;
use oxigraph::model::{NamedOrBlankNode, Quad, Term};
use std::io::{Read, Write};

/// The bytes of a source after `compression` (by local name). An archive must hold one file.
pub(crate) fn decompress(bytes: Vec<u8>, compression: Option<&str>) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    match compression {
        None => return Ok(bytes),
        Some("gzip") => {
            copy_capped(flate2::read::GzDecoder::new(bytes.as_slice()), &mut out)
                .map_err(|e| format!("gzip: {e}"))?;
        }
        Some("zip") => {
            let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
                .map_err(|e| format!("zip: {e}"))?;
            let file = (0..archive.len())
                .find(|&i| archive.by_index(i).is_ok_and(|f| f.is_file()))
                .ok_or("zip: the archive holds no file")?;
            archive
                .by_index(file)
                .and_then(|f| copy_capped(f, &mut out).map_err(Into::into))
                .map_err(|e| format!("zip: {e}"))?;
        }
        Some("targz") => {
            let tar = flate2::read::GzDecoder::new(bytes.as_slice());
            first_tar_file(tar, &mut out)?;
        }
        Some("tarxz") => {
            let mut tar = Vec::new();
            lzma_rs::xz_decompress(
                &mut bytes.as_slice(),
                &mut Capped {
                    out: &mut tar,
                    left: MAX_DECOMPRESSED,
                },
            )
            .map_err(|e| format!("xz: {e}"))?;
            first_tar_file(tar.as_slice(), &mut out)?;
        }
        Some(other) => return Err(format!("compression {other} is not supported")),
    }
    Ok(out)
}

/// The most bytes a compressed source may expand to (4 GiB), so that a small archive cannot
/// fill the memory of a server.
const MAX_DECOMPRESSED: u64 = 4 << 30;

/// Appends to `out`, failing past [`MAX_DECOMPRESSED`] bytes in all.
struct Capped<'a> {
    out: &'a mut Vec<u8>,
    left: u64,
}

impl std::io::Write for Capped<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.len() as u64 > self.left {
            return Err(std::io::Error::other(format!(
                "the source expands to more than {} GiB",
                MAX_DECOMPRESSED >> 30
            )));
        }
        self.left -= buf.len() as u64;
        self.out.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn copy_capped(mut from: impl Read, out: &mut Vec<u8>) -> std::io::Result<()> {
    let left = MAX_DECOMPRESSED.saturating_sub(out.len() as u64);
    std::io::copy(&mut from, &mut Capped { out, left }).map(|_| ())
}

fn first_tar_file(tar: impl Read, out: &mut Vec<u8>) -> Result<(), String> {
    let mut archive = tar::Archive::new(tar);
    for entry in archive.entries().map_err(|e| format!("tar: {e}"))? {
        let entry = entry.map_err(|e| format!("tar: {e}"))?;
        if entry.header().entry_type().is_file() {
            copy_capped(entry, out).map_err(|e| format!("tar: {e}"))?;
            return Ok(());
        }
    }
    Err("tar: the archive holds no file".into())
}

/// The bytes as UTF-8, from `encoding` (by local name; UTF-8 when none).
pub(crate) fn decode(bytes: Vec<u8>, encoding: Option<&str>) -> Result<Vec<u8>, String> {
    let Some(encoding) = encoding else {
        return Ok(bytes);
    };
    match encoding.to_ascii_uppercase().replace('_', "-").as_str() {
        "UTF-8" | "UTF8" => Ok(bytes),
        e @ ("UTF-16" | "UTF16" | "UTF-16LE" | "UTF-16BE") => {
            if !bytes.len().is_multiple_of(2) {
                return Err("UTF-16: an odd number of bytes".into());
            }
            // A byte order mark decides; else the encoding's own order, big-endian for UTF-16.
            let (little, body) = match bytes.as_slice() {
                [0xFF, 0xFE, rest @ ..] => (true, rest),
                [0xFE, 0xFF, rest @ ..] => (false, rest),
                rest => (e.ends_with("LE"), rest),
            };
            let units = body.chunks_exact(2).map(|c| {
                if little {
                    u16::from_le_bytes([c[0], c[1]])
                } else {
                    u16::from_be_bytes([c[0], c[1]])
                }
            });
            char::decode_utf16(units)
                .collect::<Result<String, _>>()
                .map(String::into_bytes)
                .map_err(|e| format!("UTF-16: {e}"))
        }
        other => Err(format!(
            "encoding {other} is not supported (UTF-8 or UTF-16)"
        )),
    }
}

/// `value` with every string equal to one of `nulls` replaced by JSON null (no value); with
/// `drop`, removed from arrays of strings instead (the values of an XML reference).
pub(crate) fn remove_nulls(value: &mut serde_json::Value, nulls: &[String], drop: bool) {
    if nulls.is_empty() {
        return;
    }
    match value {
        serde_json::Value::String(s) if nulls.iter().any(|n| n == s) => {
            *value = serde_json::Value::Null;
        }
        serde_json::Value::Array(items) => {
            items.iter_mut().for_each(|v| remove_nulls(v, nulls, drop));
            if drop && items.iter().all(|v| v.is_string() || v.is_null()) {
                items.retain(|v| !v.is_null());
            }
        }
        serde_json::Value::Object(members) => {
            members
                .values_mut()
                .for_each(|v| remove_nulls(v, nulls, drop));
        }
        _ => {}
    }
}

/// The bytes of the file of a logical target holding `quads`: serialized as
/// `target.serialization` says, encoded and compressed. N-Triples, Turtle, N3, RDF/XML and
/// RDF/JSON hold triples: quads in named graphs are an error there.
pub fn write_target(target: &LogicalTarget, quads: &[Quad]) -> Result<Vec<u8>, RmlError> {
    let error = |e: String| RmlError::Source {
        location: target.path.clone(),
        message: e,
    };
    let format = match target.serialization.as_str() {
        "N-Quads" => Some(RdfFormat::NQuads),
        "TriG" => Some(RdfFormat::TriG),
        "N-Triples" => Some(RdfFormat::NTriples),
        "Turtle" => Some(RdfFormat::Turtle),
        "N3" => Some(RdfFormat::N3),
        "RDF_XML" => Some(RdfFormat::RdfXml),
        "JSON-LD" => Some(RdfFormat::JsonLd {
            profile: JsonLdProfileSet::empty(),
        }),
        "RDF_JSON" => None,
        s => return Err(RmlError::Unsupported(format!("the serialization {s}"))),
    };
    let datasets = format.is_none_or(|f| f.supports_datasets());
    if !datasets && let Some(q) = quads.iter().find(|q| !q.graph_name.is_default_graph()) {
        return Err(error(format!(
            "{} holds no named graphs, but {q} is in one",
            target.serialization
        )));
    }
    let text = match format {
        Some(format) => {
            let mut writer = RdfSerializer::from_format(format).for_writer(Vec::new());
            for q in quads {
                writer
                    .serialize_quad(q.as_ref())
                    .map_err(|e| error(e.to_string()))?;
            }
            writer.finish().map_err(|e| error(e.to_string()))?
        }
        None => rdf_json(quads).into_bytes(),
    };
    let bytes = match target
        .encoding
        .as_deref()
        .map(|e| e.to_ascii_uppercase().replace('_', "-"))
    {
        None => text,
        Some(e) if e == "UTF-8" || e == "UTF8" => text,
        Some(e) if e.starts_with("UTF-16") || e == "UTF16" => {
            let text = String::from_utf8(text).map_err(|e| error(e.to_string()))?;
            let big = e.ends_with("BE");
            let mut out = if big {
                vec![0xFE, 0xFF]
            } else {
                vec![0xFF, 0xFE]
            };
            for u in text.encode_utf16() {
                out.extend(if big {
                    u.to_be_bytes()
                } else {
                    u.to_le_bytes()
                });
            }
            out
        }
        Some(e) => return Err(RmlError::Unsupported(format!("the encoding {e}"))),
    };
    compress(
        bytes,
        target.compression.as_deref(),
        &entry_name(&target.path),
    )
    .map_err(error)
}

/// The name of the file inside an archive: the target's, without the compression suffix.
fn entry_name(path: &str) -> String {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    [".tar.gz", ".tar.xz", ".tgz", ".gz", ".zip", ".xz"]
        .iter()
        .find_map(|s| name.strip_suffix(s))
        .unwrap_or(name)
        .to_owned()
}

fn compress(bytes: Vec<u8>, compression: Option<&str>, name: &str) -> Result<Vec<u8>, String> {
    let tar = |bytes: &[u8]| -> Result<Vec<u8>, String> {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        let mut builder = tar::Builder::new(Vec::new());
        builder
            .append_data(&mut header, name, bytes)
            .map_err(|e| format!("tar: {e}"))?;
        builder.into_inner().map_err(|e| format!("tar: {e}"))
    };
    let gzip = |bytes: &[u8]| -> Result<Vec<u8>, String> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).map_err(|e| format!("gzip: {e}"))?;
        encoder.finish().map_err(|e| format!("gzip: {e}"))
    };
    match compression {
        None => Ok(bytes),
        Some("gzip") => gzip(&bytes),
        Some("targz") => gzip(&tar(&bytes)?),
        Some("tarxz") => {
            let tar = tar(&bytes)?;
            let mut out = Vec::new();
            lzma_rs::xz_compress(&mut tar.as_slice(), &mut out).map_err(|e| format!("xz: {e}"))?;
            Ok(out)
        }
        Some("zip") => {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            zip.start_file(
                name,
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated),
            )
            .map_err(|e| format!("zip: {e}"))?;
            zip.write_all(&bytes).map_err(|e| format!("zip: {e}"))?;
            Ok(zip.finish().map_err(|e| format!("zip: {e}"))?.into_inner())
        }
        Some(other) => Err(format!("compression {other} is not supported")),
    }
}

/// Triples as RDF/JSON (`{"s": {"p": [{"type": "uri", "value": "o"}]}}`).
fn rdf_json(quads: &[Quad]) -> String {
    let mut subjects = serde_json::Map::new();
    for q in quads {
        let s = match &q.subject {
            NamedOrBlankNode::NamedNode(n) => n.as_str().to_owned(),
            NamedOrBlankNode::BlankNode(b) => b.to_string(),
        };
        let o = match &q.object {
            Term::NamedNode(n) => serde_json::json!({"type": "uri", "value": n.as_str()}),
            Term::BlankNode(b) => serde_json::json!({"type": "bnode", "value": b.to_string()}),
            Term::Literal(l) => {
                let mut o = serde_json::json!({"type": "literal", "value": l.value()});
                if let Some(lang) = l.language() {
                    o["lang"] = lang.into();
                } else if l.datatype() != xsd::STRING {
                    o["datatype"] = l.datatype().as_str().into();
                }
                o
            }
            #[allow(unreachable_patterns)]
            other => serde_json::json!({"type": "literal", "value": other.to_string()}),
        };
        let predicates = subjects
            .entry(s)
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        if let Some(objects) = predicates
            .as_object_mut()
            .map(|p| {
                p.entry(q.predicate.as_str())
                    .or_insert_with(|| serde_json::json!([]))
            })
            .and_then(|o| o.as_array_mut())
            && !objects.contains(&o)
        {
            objects.push(o);
        }
    }
    serde_json::Value::Object(subjects).to_string()
}

/// The quads of a logical target's file, as [`write_target`] writes it: decompressed,
/// decoded and parsed as `target` says.
pub fn read_target(target: &LogicalTarget, bytes: Vec<u8>) -> Result<Vec<Quad>, RmlError> {
    let error = |e: String| RmlError::Source {
        location: target.path.clone(),
        message: e,
    };
    let bytes = decompress(bytes, target.compression.as_deref()).map_err(error)?;
    let bytes = decode(bytes, target.encoding.as_deref()).map_err(error)?;
    let format = match target.serialization.as_str() {
        "N-Quads" => RdfFormat::NQuads,
        "TriG" => RdfFormat::TriG,
        "N-Triples" => RdfFormat::NTriples,
        "Turtle" => RdfFormat::Turtle,
        "N3" => RdfFormat::N3,
        "RDF_XML" => RdfFormat::RdfXml,
        "JSON-LD" => RdfFormat::JsonLd {
            profile: JsonLdProfileSet::empty(),
        },
        "RDF_JSON" => return read_rdf_json(&bytes).map_err(error),
        s => return Err(RmlError::Unsupported(format!("the serialization {s}"))),
    };
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    oxigraph::io::RdfParser::from_format(format)
        .for_slice(bytes)
        .map(|q| q.map_err(|e| error(e.to_string())))
        .collect()
}

fn read_rdf_json(bytes: &[u8]) -> Result<Vec<Quad>, String> {
    use oxigraph::model::{BlankNode, GraphName, Literal, NamedNode};
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let node = |s: &str| -> Result<NamedOrBlankNode, String> {
        Ok(match s.strip_prefix("_:") {
            Some(b) => BlankNode::new(b).map_err(|e| e.to_string())?.into(),
            None => NamedNode::new(s).map_err(|e| e.to_string())?.into(),
        })
    };
    let mut out = Vec::new();
    for (s, predicates) in value.as_object().ok_or("RDF/JSON: not an object")? {
        let subject = node(s)?;
        for (p, objects) in predicates.as_object().ok_or("RDF/JSON: not an object")? {
            let predicate = NamedNode::new(p).map_err(|e| e.to_string())?;
            for o in objects.as_array().ok_or("RDF/JSON: not an array")? {
                let text = o["value"].as_str().ok_or("RDF/JSON: no value")?;
                let object: Term = match o["type"].as_str() {
                    Some("uri") => NamedNode::new(text).map_err(|e| e.to_string())?.into(),
                    Some("bnode") => node(text)?.into(),
                    _ => match (o["lang"].as_str(), o["datatype"].as_str()) {
                        (Some(l), _) => Literal::new_language_tagged_literal(text, l)
                            .map_err(|e| e.to_string())?,
                        (None, Some(d)) => Literal::new_typed_literal(
                            text,
                            NamedNode::new(d).map_err(|e| e.to_string())?,
                        ),
                        (None, None) => Literal::new_simple_literal(text),
                    }
                    .into(),
                };
                out.push(Quad::new(
                    subject.clone(),
                    predicate.clone(),
                    object,
                    GraphName::DefaultGraph,
                ));
            }
        }
    }
    Ok(out)
}
