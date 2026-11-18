//! YAML: mappings and sequences are containers, mapping entries are keys, sequence items
//! are slots, as for JSON. The documents of a stream all fill the root: a mapping gives the
//! root its keys, a sequence its slots, a scalar its `rdf:_1`. Plain scalars follow the YAML
//! 1.2 core schema: integers are `xsd:int` (`xsd:long` beyond 32 bits), floats `xsd:double`
//! (written as Java writes doubles), `true`/`false` `xsd:boolean`, and nulls give no triple;
//! quoted scalars and `!!str` are strings. With `blank-nodes=false` a container is
//! `<root>/<key>` or `<root>/<n>` below its parent.
//!
//! A key given twice in one mapping is an error, as YAML 1.2 says, unless
//! `yaml.allow-duplicate-keys=true`: then the last value wins. Merge keys (`<<: *base`, a
//! YAML 1.1 feature) are not merged: `<<` is a key like any other, as in SPARQL Anything.
//!
//! An alias stands for a copy of its anchor's node, so nested aliases can make a small file
//! very large. Past `yaml.max-alias-nodes` nodes made through aliases (100,000 by default)
//! the file is refused.

use crate::FxError;
use crate::options::Options;
use crate::tree::{Sink, escape_iri, xsd_double};
use oxigraph::model::vocab::xsd;
use oxigraph::model::{NamedOrBlankNode, Term};
use std::collections::HashMap;
use yaml_rust2::Yaml;
use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser, Tag};
use yaml_rust2::scanner::{Marker, TScalarStyle};

pub(crate) fn triplify(
    bytes: &[u8],
    options: &Options,
    sink: &mut Sink<'_>,
) -> Result<(), FxError> {
    let text = std::str::from_utf8(bytes).map_err(|e| FxError::Parse(format!("YAML: {e}")))?;
    let max_alias_nodes = match options.get("yaml.max-alias-nodes") {
        None => DEFAULT_MAX_ALIAS_NODES,
        Some(n) => n.trim().parse().map_err(|_| {
            FxError::Option(format!(
                "yaml.max-alias-nodes must be a number of nodes, not {n:?}"
            ))
        })?,
    };
    let mut builder = Builder {
        duplicates: options.flag("yaml.allow-duplicate-keys", false)?,
        max_alias_nodes,
        ..Builder::default()
    };
    // The events are read one by one rather than through `Parser::load`, which recurses once
    // per level of nesting and so overflows the stack on deeply nested input.
    let mut parser = Parser::new_from_str(text);
    loop {
        let (event, mark) = parser
            .next_token()
            .map_err(|e| FxError::Parse(format!("YAML: {e}")))?;
        if event == Event::StreamEnd {
            break;
        }
        builder.on_event(event, mark);
        if builder.error.is_some() {
            break;
        }
    }
    if let Some(e) = builder.error {
        return Err(e);
    }
    let root = sink.root();
    for doc in &builder.docs {
        match doc {
            Node::Scalar(s) => {
                if let Some(o) = scalar(sink, s) {
                    sink.slot(&root, 1, o);
                }
            }
            node => members(sink, &root, node, ""),
        }
    }
    Ok(())
}

/// How many nodes aliases may make, by default.
pub(crate) const DEFAULT_MAX_ALIAS_NODES: usize = 100_000;

/// A YAML node, with its plain scalars resolved.
#[derive(Debug, Clone)]
enum Node {
    Scalar(Yaml),
    Sequence(Vec<Node>),
    /// The entries in the order of the file.
    Mapping(Vec<(String, Node)>),
}

impl Node {
    /// The number of nodes in this one, itself included.
    fn size(&self) -> usize {
        1 + match self {
            Node::Scalar(_) => 0,
            Node::Sequence(items) => items.iter().map(Node::size).sum(),
            Node::Mapping(entries) => entries.iter().map(|(_, v)| v.size()).sum(),
        }
    }
}

/// The triples of the entries or items of `node`, a container at `path`.
fn members(sink: &mut Sink<'_>, c: &NamedOrBlankNode, node: &Node, path: &str) {
    match node {
        Node::Mapping(entries) => {
            for (k, v) in entries {
                let mut segment = String::new();
                escape_iri(k, &mut segment);
                if let Some(o) = value(sink, v, path, &segment) {
                    sink.key(c, k, o);
                }
            }
        }
        Node::Sequence(items) => {
            for (i, v) in items.iter().enumerate() {
                if let Some(o) = value(sink, v, path, &(i + 1).to_string()) {
                    sink.slot(c, i + 1, o);
                }
            }
        }
        Node::Scalar(_) => {}
    }
}

/// The node for `node`, the child `segment` of the container at `parent`.
fn value(sink: &mut Sink<'_>, node: &Node, parent: &str, segment: &str) -> Option<Term> {
    match node {
        Node::Scalar(s) => scalar(sink, s),
        _ => {
            let path = if parent.is_empty() {
                segment.to_owned()
            } else {
                format!("{parent}/{segment}")
            };
            let c = sink.container(|| path.clone());
            members(sink, &c, node, &path);
            Some(c.into())
        }
    }
}

fn scalar(sink: &Sink<'_>, s: &Yaml) -> Option<Term> {
    match s {
        Yaml::String(s) => sink.string(s),
        Yaml::Integer(i) => match i32::try_from(*i) {
            Ok(i) => sink.typed(&i.to_string(), xsd::INT),
            Err(_) => sink.typed(&i.to_string(), xsd::LONG),
        },
        Yaml::Real(_) => s
            .as_f64()
            .and_then(|f| sink.typed(&xsd_double(f), xsd::DOUBLE)),
        Yaml::Boolean(b) => sink.typed(if *b { "true" } else { "false" }, xsd::BOOLEAN),
        _ => None,
    }
}

/// Builds the [`Node`]s of each document from the parser's events.
#[derive(Default)]
struct Builder {
    duplicates: bool,
    /// The nodes aliases have made so far, and how many they may.
    alias_nodes: usize,
    max_alias_nodes: usize,
    docs: Vec<Node>,
    /// Open sequences and mappings: a mapping with the key waiting for its value, the
    /// anchor, and how deeply the members so far nest.
    stack: Vec<(Node, Option<String>, usize, usize)>,
    /// The anchored nodes, with their number of nodes and how deeply they nest.
    anchors: HashMap<usize, (Node, usize, usize)>,
    error: Option<FxError>,
}

/// How deeply sequences and mappings may nest: a container's IRI names all its ancestors, so
/// the graph grows with the square of the nesting, and deeper nodes would be walked
/// recursively.
const MAX_DEPTH: usize = 512;

impl MarkedEventReceiver for Builder {
    fn on_event(&mut self, ev: Event, mark: Marker) {
        if self.error.is_some() {
            return;
        }
        let done = match ev {
            Event::Scalar(text, style, anchor, tag) => Some((resolve(text, style, tag), anchor, 0)),
            Event::SequenceStart(..) | Event::MappingStart(..) if self.stack.len() >= MAX_DEPTH => {
                self.error = Some(FxError::Parse(format!(
                    "YAML line {}: sequences and mappings nest more than {MAX_DEPTH} deep",
                    mark.line()
                )));
                None
            }
            Event::SequenceStart(anchor, _) => {
                self.stack
                    .push((Node::Sequence(Vec::new()), None, anchor, 0));
                None
            }
            Event::MappingStart(anchor, _) => {
                self.stack
                    .push((Node::Mapping(Vec::new()), None, anchor, 0));
                None
            }
            Event::SequenceEnd | Event::MappingEnd => {
                self.stack.pop().map(|(n, _, a, height)| (n, a, height + 1))
            }
            // An alias copies its anchor's node, which may nest as deeply as the limit
            // allows; inside other containers the copy may not nest deeper than it.
            Event::Alias(id) => match self.anchors.get(&id) {
                Some((_, _, height)) if self.stack.len() + height > MAX_DEPTH => {
                    self.error = Some(FxError::Parse(format!(
                        "YAML line {}: through an alias, sequences and mappings nest more \
                         than {MAX_DEPTH} deep",
                        mark.line()
                    )));
                    None
                }
                Some((node, size, height)) => {
                    self.alias_nodes = self.alias_nodes.saturating_add(*size);
                    if self.alias_nodes > self.max_alias_nodes {
                        self.error = Some(FxError::Unsupported(format!(
                            "YAML line {}: aliases make more than {} nodes \
                             (yaml.max-alias-nodes)",
                            mark.line(),
                            self.max_alias_nodes
                        )));
                        None
                    } else {
                        Some((node.clone(), 0, *height))
                    }
                }
                None => {
                    self.error = Some(FxError::Parse(format!(
                        "YAML line {}: unknown alias",
                        mark.line()
                    )));
                    None
                }
            },
            _ => None,
        };
        if let Some((node, anchor, height)) = done
            && let Err(e) = self.insert(node, anchor, height, mark)
        {
            self.error = Some(e);
        }
    }
}

impl Builder {
    /// Adds `node`, in which sequences and mappings nest `height` deep, to the open
    /// container or as a document.
    fn insert(
        &mut self,
        node: Node,
        anchor: usize,
        height: usize,
        mark: Marker,
    ) -> Result<(), FxError> {
        if anchor > 0 {
            self.anchors
                .insert(anchor, (node.clone(), node.size(), height));
        }
        let Some((parent, key, _, nested)) = self.stack.last_mut() else {
            self.docs.push(node);
            return Ok(());
        };
        *nested = (*nested).max(height);
        match parent {
            Node::Sequence(items) => items.push(node),
            Node::Mapping(entries) => match key.take() {
                None => {
                    let Node::Scalar(k) = node else {
                        return Err(FxError::Unsupported(format!(
                            "YAML line {}: a key that is a sequence or a mapping",
                            mark.line()
                        )));
                    };
                    *key = Some(key_text(&k));
                }
                Some(k) => match entries.iter_mut().find(|(e, _)| *e == k) {
                    Some(_) if !self.duplicates => {
                        return Err(FxError::Parse(format!(
                            "YAML line {}: duplicate key {k:?} (yaml.allow-duplicate-keys=true keeps the last)",
                            mark.line()
                        )));
                    }
                    Some((_, v)) => *v = node,
                    None => entries.push((k, node)),
                },
            },
            Node::Scalar(_) => unreachable!("scalars are not pushed"),
        }
        Ok(())
    }
}

/// A scalar's value: quoted and block scalars and `!!str` are strings; plain ones follow the
/// core schema.
fn resolve(text: String, style: TScalarStyle, tag: Option<Tag>) -> Node {
    let string = style != TScalarStyle::Plain
        || tag
            .as_ref()
            .is_some_and(|t| t.handle == "tag:yaml.org,2002:" && t.suffix == "str");
    Node::Scalar(if string {
        Yaml::String(text)
    } else {
        Yaml::from_str(&text)
    })
}

fn key_text(k: &Yaml) -> String {
    match k {
        Yaml::String(s) | Yaml::Real(s) => s.clone(),
        Yaml::Integer(i) => i.to_string(),
        Yaml::Boolean(b) => b.to_string(),
        _ => "null".to_owned(),
    }
}
