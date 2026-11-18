//! RDF Dataset Canonicalization (RDFC-1.0, <https://www.w3.org/TR/rdf-canon/>) with SHA-256,
//! and the difference between two datasets with blank nodes matched through their canonical
//! labels.
//!
//! [`canonicalize`] relabels the blank nodes `c14n0`, `c14n1`, … so that two isomorphic
//! datasets give the same quads, and [`Canonical::nquads`] writes them in canonical N-Quads
//! (each quad on one line, the lines sorted), whose SHA-256 is [`Canonical::digest`].
//!
//! Some datasets make the algorithm search very many ways to label their blank nodes (a
//! clique of ten blank nodes all linked to one another, say: "dataset poisoning"), or very
//! long chains of alike blank nodes (an RDF list of the same value repeated). The work for
//! each blank node, the work of the whole run (copies of the labelling state included) and the
//! depth of the search are bounded by [`Limits`]; past any of them canonicalization fails with
//! [`CanonError::TooComplex`].
//!
//! Blank nodes inside triple terms (RDF 1.2) are outside RDFC-1.0 and are refused.

use oxigraph::model::{GraphName, NamedOrBlankNode, Quad, Term};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

/// Why a dataset was not canonicalized.
#[derive(Debug, thiserror::Error)]
pub enum CanonError {
    #[error(
        "canonicalization gave up ({0}): the blank nodes are too alike to label in reasonable \
         time (a poisoned dataset, or a long chain of alike blank nodes such as an RDF list of \
         one value repeated)"
    )]
    TooComplex(String),
    #[error("RDFC-1.0 does not cover blank nodes inside triple terms: {0}")]
    TripleTerm(String),
}

/// Bounds on the work of [`canonicalize_with`].
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Steps of the search for one blank node's label among blank nodes whose neighbourhoods
    /// look alike (each permutation tried and each Hash N-Degree Quads call is a step).
    pub work: u64,
    /// Work of the whole run: the steps of all blank nodes, plus the size of every copy of
    /// the labelling state the search makes.
    pub total_work: u64,
    /// How deep Hash N-Degree Quads may recurse.
    pub depth: usize,
}

impl Default for Limits {
    /// 20,000 steps per blank node: the W3C "poison – evil" tests take under 5,000, a clique
    /// of ten blank nodes (test074) is refused after about 0.2 s. 5 million units for the
    /// whole run (a few seconds at most) and a depth of 200 (well inside a 2 MB stack).
    fn default() -> Self {
        Self {
            work: 20_000,
            total_work: 5_000_000,
            depth: 200,
        }
    }
}

/// A canonicalized dataset.
#[derive(Debug, Clone)]
pub struct Canonical {
    /// The canonical N-Quads lines, sorted, each ending with a line feed.
    pub lines: Vec<String>,
    /// The canonical label (without `_:`) of each input blank node label.
    pub labels: HashMap<String, String>,
}

impl Canonical {
    /// The canonical N-Quads document.
    pub fn nquads(&self) -> String {
        self.lines.concat()
    }

    /// The SHA-256 of the canonical N-Quads document, in lowercase hex.
    pub fn digest(&self) -> String {
        let mut hash = Sha256::new();
        for line in &self.lines {
            hash.update(line.as_bytes());
        }
        hex(&hash.finalize())
    }
}

/// Canonicalizes `quads` (duplicates count once) within the default [`Limits`].
pub fn canonicalize(quads: impl IntoIterator<Item = Quad>) -> Result<Canonical, CanonError> {
    canonicalize_with(quads, Limits::default())
}

/// Canonicalizes `quads` (duplicates count once).
pub fn canonicalize_with(
    quads: impl IntoIterator<Item = Quad>,
    limits: Limits,
) -> Result<Canonical, CanonError> {
    let quads: Vec<Quad> = quads
        .into_iter()
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let mut state = State {
        quads: &quads,
        by_node: HashMap::new(),
        canonical: Issuer::new("c14n"),
        work: 0,
        total: 0,
        depth: 0,
        limits,
    };
    // 4.4.3 step 2: the quads each blank node is in.
    for (i, quad) in quads.iter().enumerate() {
        if let Term::Triple(t) = &quad.object
            && let Some(b) = nested_blank_node(t)
        {
            return Err(CanonError::TripleTerm(format!("_:{b}")));
        }
        for label in blank_nodes(quad) {
            let list = state.by_node.entry(label.to_owned()).or_default();
            if list.last() != Some(&i) {
                list.push(i);
            }
        }
    }
    // Step 3: first-degree hashes; nodes with a unique hash get their label (steps 4).
    let mut by_hash: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut nodes: Vec<&String> = state.by_node.keys().collect();
    nodes.sort_unstable();
    for node in nodes {
        by_hash
            .entry(state.first_degree(node))
            .or_default()
            .push(node.clone());
    }
    let mut shared = Vec::new();
    for (_, nodes) in by_hash {
        if nodes.len() == 1 {
            state.canonical.issue(&nodes[0]);
        } else {
            shared.push(nodes);
        }
    }
    // Step 5: the nodes that share a first-degree hash, in hash order.
    for nodes in shared {
        let mut paths = Vec::new();
        for node in &nodes {
            if state.canonical.get(node).is_some() {
                continue;
            }
            let mut issuer = Issuer::new("b");
            issuer.issue(node);
            state.work = 0;
            paths.push(state.n_degree(node, issuer)?);
        }
        paths.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, issuer) in paths {
            for node in &issuer.order {
                state.canonical.issue(node);
            }
        }
    }
    let labels: HashMap<String, String> = state
        .canonical
        .order
        .iter()
        .map(|n| {
            (
                n.clone(),
                state.canonical.get(n).unwrap_or_default().to_owned(),
            )
        })
        .collect();
    let mut lines: Vec<String> = quads
        .iter()
        .map(|q| nquad(q, &|b| labels.get(b).map_or(b, String::as_str)))
        .collect();
    lines.sort_unstable();
    lines.dedup();
    Ok(Canonical { lines, labels })
}

struct State<'a> {
    quads: &'a [Quad],
    by_node: HashMap<String, Vec<usize>>,
    canonical: Issuer,
    work: u64,
    total: u64,
    depth: usize,
    limits: Limits,
}

impl State<'_> {
    fn step(&mut self) -> Result<(), CanonError> {
        self.work += 1;
        if self.work > self.limits.work {
            return Err(CanonError::TooComplex(format!(
                "more than {} steps on one blank node",
                self.limits.work
            )));
        }
        self.charge(1)
    }

    /// Counts `units` against the whole run's budget.
    fn charge(&mut self, units: u64) -> Result<(), CanonError> {
        self.total += units;
        if self.total > self.limits.total_work {
            return Err(CanonError::TooComplex(format!(
                "more than {} units of work in all",
                self.limits.total_work
            )));
        }
        Ok(())
    }

    /// A copy of `issuer`, charged by its size.
    fn copy(&mut self, issuer: &Issuer) -> Result<Issuer, CanonError> {
        self.charge(issuer.order.len() as u64)?;
        Ok(issuer.clone())
    }

    /// 4.6 Hash First Degree Quads.
    fn first_degree(&self, node: &str) -> String {
        let mut lines: Vec<String> = self.by_node[node]
            .iter()
            .map(|&i| nquad(&self.quads[i], &|b| if b == node { "a" } else { "z" }))
            .collect();
        lines.sort_unstable();
        sha256(lines.concat().as_bytes())
    }

    /// 4.7 Hash Related Blank Node.
    fn related(&self, related: &str, quad: &Quad, issuer: &Issuer, position: char) -> String {
        let mut input = String::new();
        input.push(position);
        if position != 'g' {
            let _ = write!(input, "<{}>", quad.predicate.as_str());
        }
        if let Some(id) = self.canonical.get(related).or_else(|| issuer.get(related)) {
            let _ = write!(input, "_:{id}");
        } else {
            input.push_str(&self.first_degree(related));
        }
        sha256(input.as_bytes())
    }

    /// 4.8 Hash N-Degree Quads: the hash of `node`'s neighbourhood, and the issuer that
    /// labelled it.
    fn n_degree(&mut self, node: &str, issuer: Issuer) -> Result<(String, Issuer), CanonError> {
        if self.depth >= self.limits.depth {
            return Err(CanonError::TooComplex(format!(
                "search deeper than {} levels",
                self.limits.depth
            )));
        }
        self.depth += 1;
        let result = self.n_degree_inner(node, issuer);
        self.depth -= 1;
        result
    }

    fn n_degree_inner(
        &mut self,
        node: &str,
        mut issuer: Issuer,
    ) -> Result<(String, Issuer), CanonError> {
        self.step()?;
        // Step 3: the related blank nodes, by the hash of how they are related.
        let mut by_hash: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for &i in &self.by_node[node] {
            let quad = &self.quads[i];
            for (component, position) in components(quad) {
                if component != node {
                    let hash = self.related(component, quad, &issuer, position);
                    by_hash.entry(hash).or_default().push(component.to_owned());
                }
            }
        }
        let mut data = String::new();
        // Step 5.
        for (hash, related) in by_hash {
            data.push_str(&hash);
            let mut chosen_path = String::new();
            let mut chosen_issuer: Option<Issuer> = None;
            let mut order: Vec<usize> = (0..related.len()).collect();
            'permutations: loop {
                self.step()?;
                let mut copy = self.copy(&issuer)?;
                let mut path = String::new();
                let mut recursion = Vec::new();
                for &k in &order {
                    let r = &related[k];
                    if let Some(id) = self.canonical.get(r) {
                        let _ = write!(path, "_:{id}");
                    } else {
                        if copy.get(r).is_none() {
                            recursion.push(r.clone());
                        }
                        let id = copy.issue(r);
                        let _ = write!(path, "_:{id}");
                    }
                    if worse(&path, &chosen_path) {
                        if next_permutation(&mut order) {
                            continue 'permutations;
                        }
                        break 'permutations;
                    }
                }
                for r in recursion {
                    let given = self.copy(&copy)?;
                    let (hash, result) = self.n_degree(&r, given)?;
                    copy = result;
                    let id = copy.issue(&r);
                    let _ = write!(path, "_:{id}<{hash}>");
                    if worse(&path, &chosen_path) {
                        if next_permutation(&mut order) {
                            continue 'permutations;
                        }
                        break 'permutations;
                    }
                }
                if chosen_path.is_empty() || path < chosen_path {
                    chosen_path = path;
                    chosen_issuer = Some(copy);
                }
                if !next_permutation(&mut order) {
                    break;
                }
            }
            data.push_str(&chosen_path);
            if let Some(chosen) = chosen_issuer {
                issuer = chosen;
            }
        }
        Ok((sha256(data.as_bytes()), issuer))
    }
}

/// Whether `path` can no longer beat `chosen` (5.4.4.3 and 5.4.5.5).
fn worse(path: &str, chosen: &str) -> bool {
    !chosen.is_empty() && path.len() >= chosen.len() && path > chosen
}

/// The next permutation of `order` in lexicographic order; false after the last.
fn next_permutation(order: &mut [usize]) -> bool {
    let Some(i) = order.windows(2).rposition(|w| w[0] < w[1]) else {
        return false;
    };
    let j = order.iter().rposition(|&x| x > order[i]).unwrap_or(i);
    order.swap(i, j);
    order[i + 1..].reverse();
    true
}

/// 4.5 Identifier Issuer.
#[derive(Clone)]
struct Issuer {
    prefix: &'static str,
    issued: HashMap<String, String>,
    order: Vec<String>,
}

impl Issuer {
    fn new(prefix: &'static str) -> Self {
        Self {
            prefix,
            issued: HashMap::new(),
            order: Vec::new(),
        }
    }

    fn get(&self, node: &str) -> Option<&str> {
        self.issued.get(node).map(String::as_str)
    }

    fn issue(&mut self, node: &str) -> String {
        if let Some(id) = self.issued.get(node) {
            return id.clone();
        }
        let id = format!("{}{}", self.prefix, self.order.len());
        self.issued.insert(node.to_owned(), id.clone());
        self.order.push(node.to_owned());
        id
    }
}

/// The blank nodes of `quad` with their position (`s`, `o` or `g`).
fn components(quad: &Quad) -> impl Iterator<Item = (&str, char)> {
    let s = match &quad.subject {
        NamedOrBlankNode::BlankNode(b) => Some((b.as_str(), 's')),
        NamedOrBlankNode::NamedNode(_) => None,
    };
    let o = match &quad.object {
        Term::BlankNode(b) => Some((b.as_str(), 'o')),
        _ => None,
    };
    let g = match &quad.graph_name {
        GraphName::BlankNode(b) => Some((b.as_str(), 'g')),
        _ => None,
    };
    [s, o, g].into_iter().flatten()
}

fn blank_nodes(quad: &Quad) -> impl Iterator<Item = &str> {
    components(quad).map(|(b, _)| b)
}

fn nested_blank_node(t: &oxigraph::model::Triple) -> Option<&str> {
    if let NamedOrBlankNode::BlankNode(b) = &t.subject {
        return Some(b.as_str());
    }
    match &t.object {
        Term::BlankNode(b) => Some(b.as_str()),
        Term::Triple(t) => nested_blank_node(t),
        _ => None,
    }
}

/// `quad` in canonical N-Quads, with a line feed, its blank node labels mapped by `label`.
pub(crate) fn nquad<'a>(quad: &'a Quad, label: &impl Fn(&'a str) -> &'a str) -> String {
    let mut out = String::new();
    subject(&mut out, &quad.subject, label);
    let _ = write!(out, " <{}> ", quad.predicate.as_str());
    term(&mut out, &quad.object, label);
    match &quad.graph_name {
        GraphName::DefaultGraph => {}
        GraphName::NamedNode(n) => {
            let _ = write!(out, " <{}>", n.as_str());
        }
        GraphName::BlankNode(b) => {
            let _ = write!(out, " _:{}", label(b.as_str()));
        }
    }
    out.push_str(" .\n");
    out
}

fn subject<'a>(out: &mut String, s: &'a NamedOrBlankNode, label: &impl Fn(&'a str) -> &'a str) {
    match s {
        NamedOrBlankNode::NamedNode(n) => {
            let _ = write!(out, "<{}>", n.as_str());
        }
        NamedOrBlankNode::BlankNode(b) => {
            let _ = write!(out, "_:{}", label(b.as_str()));
        }
    }
}

fn term<'a>(out: &mut String, t: &'a Term, label: &impl Fn(&'a str) -> &'a str) {
    match t {
        Term::NamedNode(n) => {
            let _ = write!(out, "<{}>", n.as_str());
        }
        Term::BlankNode(b) => {
            let _ = write!(out, "_:{}", label(b.as_str()));
        }
        Term::Literal(l) => {
            out.push('"');
            escape(out, l.value());
            out.push('"');
            if let Some(lang) = l.language() {
                out.push('@');
                out.push_str(&lang.to_ascii_lowercase());
                if let Some(direction) = l.direction() {
                    let _ = write!(out, "--{direction}");
                }
            } else if l.datatype() != oxigraph::model::vocab::xsd::STRING {
                let _ = write!(out, "^^<{}>", l.datatype().as_str());
            }
        }
        Term::Triple(t) => {
            out.push_str("<<( ");
            subject(out, &t.subject, label);
            let _ = write!(out, " <{}> ", t.predicate.as_str());
            term(out, &t.object, label);
            out.push_str(" )>>");
        }
    }
}

/// Escapes a literal's text as canonical N-Quads does.
fn escape(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\0'..='\u{1f}' | '\u{7f}' | '\u{fffe}' | '\u{ffff}' => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
}

/// What changed from one dataset to another: the canonical N-Quads lines only in `b`
/// (added) and only in `a` (removed), sorted. A blank node is the same in both when it gets
/// the same canonical label, which holds for isomorphic datasets; when the blank nodes differ,
/// their quads show as removed and added.
#[derive(Debug, Clone, Default)]
pub struct Diff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

/// The [`Diff`] from `a` to `b`.
pub fn diff(a: &Canonical, b: &Canonical) -> Diff {
    let (mut i, mut j) = (0, 0);
    let mut d = Diff::default();
    while i < a.lines.len() || j < b.lines.len() {
        match (a.lines.get(i), b.lines.get(j)) {
            (Some(x), Some(y)) if x == y => {
                i += 1;
                j += 1;
            }
            (Some(x), Some(y)) if x < y => {
                d.removed.push(x.clone());
                i += 1;
            }
            (Some(x), None) => {
                d.removed.push(x.clone());
                i += 1;
            }
            (_, Some(y)) => {
                d.added.push(y.clone());
                j += 1;
            }
            (None, None) => break,
        }
    }
    d
}

/// Added and removed quads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub added: usize,
    pub removed: usize,
}

/// A [`Diff`] counted per graph, per predicate and per class of the subject.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub total: Counts,
    /// By graph IRI (or `_:label`); the default graph is `""`.
    pub graphs: BTreeMap<String, Counts>,
    pub predicates: BTreeMap<String, Counts>,
    /// By the `rdf:type` of the subject in either dataset (a subject of several classes
    /// counts for each); subjects without one under `""`.
    pub classes: BTreeMap<String, Counts>,
}

impl Diff {
    /// Counts the changes; `a` and `b` are the datasets diffed, for the subjects' classes.
    pub fn summary(&self, a: &Canonical, b: &Canonical) -> Summary {
        const TYPE: &str = " <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> ";
        let mut classes: HashMap<String, Vec<String>> = HashMap::new();
        for line in a.lines.iter().chain(&b.lines) {
            if let Some((s, rest)) = line.split_once(TYPE)
                && let Some(class) = rest.strip_prefix('<').and_then(|r| r.split_once('>'))
            {
                let list = classes.entry(s.to_owned()).or_default();
                if !list.iter().any(|c| c == class.0) {
                    list.push(class.0.to_owned());
                }
            }
        }
        let mut summary = Summary::default();
        let none = vec![String::new()];
        for (lines, added) in [(&self.added, true), (&self.removed, false)] {
            for line in lines {
                let Some(quad) = parse_line(line) else {
                    continue;
                };
                let bump = |c: &mut Counts| {
                    if added {
                        c.added += 1;
                    } else {
                        c.removed += 1;
                    }
                };
                bump(&mut summary.total);
                let graph = match &quad.graph_name {
                    GraphName::DefaultGraph => String::new(),
                    GraphName::NamedNode(n) => n.as_str().to_owned(),
                    GraphName::BlankNode(b) => format!("_:{}", b.as_str()),
                };
                bump(summary.graphs.entry(graph).or_default());
                bump(
                    summary
                        .predicates
                        .entry(quad.predicate.as_str().to_owned())
                        .or_default(),
                );
                let subject = match &quad.subject {
                    NamedOrBlankNode::NamedNode(n) => format!("<{}>", n.as_str()),
                    NamedOrBlankNode::BlankNode(b) => format!("_:{}", b.as_str()),
                };
                for class in classes.get(&subject).unwrap_or(&none) {
                    bump(summary.classes.entry(class.clone()).or_default());
                }
            }
        }
        summary
    }
}

/// One canonical N-Quads line as a quad.
fn parse_line(line: &str) -> Option<Quad> {
    oxigraph::io::RdfParser::from_format(oxigraph::io::RdfFormat::NQuads)
        .for_slice(line.as_bytes())
        .next()?
        .ok()
}

fn sha256(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permutations_run_in_order() {
        let mut order = vec![0, 1, 2];
        let mut seen = vec![order.clone()];
        while next_permutation(&mut order) {
            seen.push(order.clone());
        }
        assert_eq!(seen.len(), 6);
        assert_eq!(seen[5], [2, 1, 0]);
    }

    #[test]
    fn controls_are_escaped() {
        let mut s = String::new();
        escape(&mut s, "a\u{1}\u{b}\t\"\\\u{7f}\u{fffe}é");
        assert_eq!(s, "a\\u0001\\u000B\\t\\\"\\\\\\u007F\\uFFFEé");
    }
}
