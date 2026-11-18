//! Markdown (CommonMark, with tables and YAML front matter), as SPARQL Anything sees it: the
//! root is typed `xyz:Document`, and each block and inline node is a container typed by its
//! CommonMark node name (`xyz:Paragraph`, `xyz:Heading`, `xyz:BulletList`, `xyz:ListItem`,
//! `xyz:Link`, `xyz:TableBlock`, ...) holding its children and text in slots, in document
//! order. Headings have `xyz:level` (`xsd:int`); links and images `xyz:destination` and
//! `xyz:title`; link reference definitions `xyz:label` too. Code, HTML and front matter hold
//! their text in `rdf:_1`. With `blank-nodes=false` a container is
//! `<parent>/<node name in lower case>/<n>`, `n` counting the nodes of that kind in the
//! whole document.
//!
//! The tree is the one commonmark-java builds (which SPARQL Anything uses): a list item's
//! text is in a paragraph also when the list is tight, adjacent text is one string, and a
//! table is a `TableBlock` with a `TableHead` and a `TableBody` of `TableRow`s.

use crate::FxError;
use crate::tree::Sink;
use oxigraph::model::NamedOrBlankNode;
use oxigraph::model::vocab::xsd;
use pulldown_cmark::{
    Alignment, CodeBlockKind, Event, MetadataBlockKind, Options as Extensions, Parser, Tag, TagEnd,
};
use std::collections::HashMap;
use std::ops::Range;

/// How deeply blocks and inlines may nest. A node's path names all its ancestors, so the
/// graph grows with the square of the nesting: 40 KB of nested block quotes would make 3 GB.
const MAX_DEPTH: usize = 256;

pub(crate) fn triplify(bytes: &[u8], sink: &mut Sink<'_>) -> Result<(), FxError> {
    let text = std::str::from_utf8(bytes).map_err(|e| FxError::Parse(format!("Markdown: {e}")))?;
    let extensions = Extensions::ENABLE_TABLES | Extensions::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    let events = Parser::new_ext(text, extensions).into_offset_iter();
    // Link reference definitions are no events: they are put back at their place among the
    // document's blocks.
    let mut definitions: Vec<Definition> = events
        .reference_definitions()
        .iter()
        .map(|(_, d)| Definition {
            span: d.span.clone(),
            label: label(&text[d.span.clone()]),
            destination: d.dest.to_string(),
            title: d.title.as_ref().map(|t| t.to_string()),
        })
        .collect();
    definitions.sort_by_key(|d| d.span.start);
    definitions.reverse();
    let root = sink.root();
    let document = sink.key_iri("Document");
    sink.class(&root, document);
    let mut b = Builder {
        sink,
        stack: vec![Frame::new(root, String::new())],
        counters: HashMap::new(),
        body: false,
        alignments: Vec::new(),
        cell: 0,
    };
    for (event, range) in events {
        if b.stack.len() == 1 && matches!(event, Event::Start(_) | Event::Rule | Event::Html(_)) {
            b.definitions_before(&mut definitions, range.start);
        }
        b.event(event);
        if b.stack.len() > MAX_DEPTH {
            return Err(FxError::Parse(format!(
                "Markdown: blocks and inlines nest more than {MAX_DEPTH} deep (at byte {})",
                range.start
            )));
        }
    }
    b.definitions_before(&mut definitions, usize::MAX);
    while b.stack.len() > 1 {
        b.close();
    }
    let root = b.stack.pop().expect("the root frame");
    b.flush_frame(root);
    Ok(())
}

struct Definition {
    span: Range<usize>,
    label: String,
    destination: String,
    title: Option<String>,
}

/// The label of a link reference definition's source (`[id]: ...`), as written.
fn label(source: &str) -> String {
    let start = source.find('[').map_or(0, |i| i + 1);
    let end = source.find("]:").unwrap_or(source.len());
    source.get(start..end).unwrap_or_default().to_owned()
}

/// An open container.
struct Frame {
    node: NamedOrBlankNode,
    path: String,
    slot: usize,
    /// Text not written yet: adjacent text is one string.
    text: String,
    /// A paragraph made for the text of a tight list item.
    implicit: bool,
    /// Code, HTML or front matter: the text is the whole value.
    literal: bool,
    /// HTML or front matter: the value is without its last newline.
    trim: bool,
}

impl Frame {
    fn new(node: NamedOrBlankNode, path: String) -> Self {
        Self {
            node,
            path,
            slot: 0,
            text: String::new(),
            implicit: false,
            literal: false,
            trim: false,
        }
    }
}

struct Builder<'s, 'a> {
    sink: &'s mut Sink<'a>,
    stack: Vec<Frame>,
    /// How many nodes of each kind so far, for their paths.
    counters: HashMap<&'static str, usize>,
    /// Whether the current table's body is open.
    body: bool,
    /// The current table's column alignments, and the current row's cell.
    alignments: Vec<Alignment>,
    cell: usize,
}

impl Builder<'_, '_> {
    fn top(&mut self) -> &mut Frame {
        self.stack.last_mut().expect("the root frame")
    }

    /// Writes the pending text of the top frame in its next slot.
    fn flush(&mut self) {
        let frame = self.stack.last_mut().expect("the root frame");
        if frame.text.is_empty() || frame.literal {
            return;
        }
        let text = std::mem::take(&mut frame.text);
        frame.slot += 1;
        let (node, slot) = (frame.node.clone(), frame.slot);
        if let Some(o) = self.sink.string(&text) {
            self.sink.slot(&node, slot, o);
        }
    }

    fn flush_frame(&mut self, frame: Frame) {
        if frame.literal {
            let text = match frame.trim {
                true => frame.text.strip_suffix('\n').unwrap_or(&frame.text),
                false => &frame.text,
            };
            if let Some(o) = self.sink.string(text) {
                self.sink.slot(&frame.node, 1, o);
            }
        } else if !frame.text.is_empty()
            && let Some(o) = self.sink.string(&frame.text)
        {
            self.sink.slot(&frame.node, frame.slot + 1, o);
        }
    }

    /// Opens a container of kind `name` in the next slot of the top frame.
    fn open(&mut self, name: &'static str) -> NamedOrBlankNode {
        self.flush();
        let n = {
            let c = self.counters.entry(name).or_default();
            *c += 1;
            *c
        };
        let parent = self.top();
        parent.slot += 1;
        let (parent_node, slot) = (parent.node.clone(), parent.slot);
        let path = if parent.path.is_empty() {
            format!("{}/{n}", name.to_ascii_lowercase())
        } else {
            format!("{}/{}/{n}", parent.path, name.to_ascii_lowercase())
        };
        let node = self.sink.container(|| path.clone());
        self.sink.slot(&parent_node, slot, node.clone().into());
        let class = self.sink.key_iri(name);
        self.sink.class(&node, class);
        self.stack.push(Frame::new(node.clone(), path));
        node
    }

    fn close(&mut self) {
        if self.stack.len() > 1 {
            let frame = self.stack.pop().expect("an open frame");
            self.flush_frame(frame);
        }
    }

    /// A container holding `text` in `rdf:_1`.
    fn leaf(&mut self, name: &'static str, text: &str) {
        let node = self.open(name);
        if let Some(o) = self.sink.string(text) {
            self.sink.slot(&node, 1, o);
        }
        self.top().slot = 1;
        self.close();
    }

    fn key(&mut self, node: &NamedOrBlankNode, key: &str, value: &str) {
        if let Some(o) = self.sink.string(value) {
            self.sink.key(node, key, o);
        }
    }

    /// Closes the paragraph opened for a tight list item's text.
    fn end_implicit(&mut self) {
        if self.top().implicit {
            self.close();
        }
    }

    /// Inline content: in a tight list item, it goes in a paragraph of its own.
    fn inline(&mut self) {
        if self.stack.len() > 1 && self.at("ListItem") {
            self.open("Paragraph");
            self.top().implicit = true;
        }
    }

    /// Whether the top frame is a container of kind `name`.
    fn at(&self, name: &str) -> bool {
        let frame = self.stack.last().expect("the root frame");
        let lower = format!("/{}/", name.to_ascii_lowercase());
        let path = format!("/{}", frame.path);
        path.rsplit_once('/')
            .is_some_and(|(head, _)| format!("{head}/").ends_with(&lower))
    }

    fn definitions_before(&mut self, definitions: &mut Vec<Definition>, offset: usize) {
        while definitions.last().is_some_and(|d| d.span.start < offset) {
            let d = definitions.pop().expect("a definition");
            let node = self.open("LinkReferenceDefinition");
            self.key(&node, "label", &d.label);
            self.key(&node, "destination", &d.destination);
            if let Some(t) = &d.title {
                self.key(&node, "title", t);
            }
            self.close();
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => {
                self.inline();
                self.top().text.push_str(&t);
            }
            Event::Code(t) => {
                self.inline();
                self.leaf("Code", &t);
            }
            Event::Html(t) => {
                if self.top().literal {
                    self.top().text.push_str(&t);
                } else {
                    self.leaf("HtmlBlock", t.strip_suffix('\n').unwrap_or(&t));
                }
            }
            Event::InlineHtml(t) => {
                self.inline();
                self.leaf("HtmlInline", &t);
            }
            Event::SoftBreak => {
                self.inline();
                self.open("SoftLineBreak");
                self.close();
            }
            Event::HardBreak => {
                self.inline();
                self.open("HardLineBreak");
                self.close();
            }
            Event::Rule => {
                self.end_implicit();
                self.open("ThematicBreak");
                self.close();
            }
            // Not enabled: footnotes, task lists, math.
            Event::FootnoteReference(_)
            | Event::TaskListMarker(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        let block = !matches!(
            tag,
            Tag::Emphasis
                | Tag::Strong
                | Tag::Strikethrough
                | Tag::Superscript
                | Tag::Subscript
                | Tag::Link { .. }
                | Tag::Image { .. }
        );
        if block {
            self.end_implicit();
        } else {
            self.inline();
        }
        match tag {
            Tag::Paragraph => {
                self.open("Paragraph");
            }
            Tag::Heading { level, .. } => {
                let node = self.open("Heading");
                if let Some(o) = self.sink.typed(&(level as u8).to_string(), xsd::INT) {
                    self.sink.key(&node, "level", o);
                }
            }
            Tag::BlockQuote(_) => {
                self.open("BlockQuote");
            }
            Tag::CodeBlock(kind) => {
                self.open(match kind {
                    CodeBlockKind::Fenced(_) => "FencedCodeBlock",
                    CodeBlockKind::Indented => "IndentedCodeBlock",
                });
                self.top().literal = true;
            }
            Tag::HtmlBlock => {
                self.open("HtmlBlock");
                self.top().literal = true;
                self.top().trim = true;
            }
            Tag::MetadataBlock(MetadataBlockKind::YamlStyle | MetadataBlockKind::PlusesStyle) => {
                self.open("YamlFrontMatter");
                self.top().literal = true;
                self.top().trim = true;
            }
            Tag::List(start) => {
                self.open(if start.is_some() {
                    "OrderedList"
                } else {
                    "BulletList"
                });
            }
            Tag::Item => {
                self.open("ListItem");
            }
            Tag::Emphasis => {
                self.open("Emphasis");
            }
            Tag::Strong => {
                self.open("StrongEmphasis");
            }
            Tag::Link {
                dest_url, title, ..
            } => {
                let node = self.open("Link");
                self.key(&node, "destination", &dest_url);
                if !title.is_empty() {
                    self.key(&node, "title", &title);
                }
            }
            Tag::Image {
                dest_url, title, ..
            } => {
                let node = self.open("Image");
                self.key(&node, "destination", &dest_url);
                if !title.is_empty() {
                    self.key(&node, "title", &title);
                }
            }
            Tag::Table(alignments) => {
                self.open("TableBlock");
                self.body = false;
                self.alignments = alignments;
            }
            Tag::TableHead => {
                self.open("TableHead");
                self.open("TableRow");
                self.cell = 0;
            }
            Tag::TableRow => {
                if !self.body {
                    self.open("TableBody");
                    self.body = true;
                }
                self.open("TableRow");
                self.cell = 0;
            }
            Tag::TableCell => {
                let node = self.open("TableCell");
                let alignment = match self.alignments.get(self.cell) {
                    Some(Alignment::Left) => Some("LEFT"),
                    Some(Alignment::Right) => Some("RIGHT"),
                    Some(Alignment::Center) => Some("CENTER"),
                    _ => None,
                };
                self.cell += 1;
                if let Some(a) = alignment {
                    self.key(&node, "alignment", a);
                }
            }
            // Not enabled.
            Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Strikethrough
            | Tag::Superscript
            | Tag::Subscript => {
                self.open("CustomNode");
            }
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Item | TagEnd::List(_) | TagEnd::BlockQuote(_) => {
                self.end_implicit();
                self.close();
            }
            TagEnd::TableHead => {
                self.close();
                self.close();
            }
            TagEnd::Table => {
                if self.body {
                    self.close();
                    self.body = false;
                }
                self.close();
            }
            _ => self.close(),
        }
    }
}
