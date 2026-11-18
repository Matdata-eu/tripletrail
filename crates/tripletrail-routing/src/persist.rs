//! A compact binary form of routing graphs and contraction hierarchies, so a server can keep
//! them between restarts. The format is private to this crate and versioned: a file from
//! another version reads as `None` and is rebuilt.

use crate::cch::Cch;
use crate::ch::ContractionHierarchy;
use crate::graph::{EdgeRow, FxMap, Graph, InternedGraph};
use std::hash::Hash;

const MAGIC: &[u8; 8] = b"OXGRAPH1";

pub(crate) struct Writer<'a>(pub(crate) &'a mut Vec<u8>);

impl Writer<'_> {
    pub(crate) fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub(crate) fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn f64(&mut self, v: f64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn str(&mut self, v: &str) {
        self.u64(v.len() as u64);
        self.0.extend_from_slice(v.as_bytes());
    }
}

pub(crate) struct Reader<'a>(pub(crate) &'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }
    pub(crate) fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    pub(crate) fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    pub(crate) fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    pub(crate) fn f64(&mut self) -> Option<f64> {
        Some(f64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    pub(crate) fn str(&mut self) -> Option<&str> {
        let n = usize::try_from(self.u64()?).ok()?;
        std::str::from_utf8(self.take(n)?).ok()
    }
    /// A length that cannot exceed the remaining bytes (each item takes at least `min` bytes).
    pub(crate) fn len(&mut self, min: usize) -> Option<usize> {
        let n = usize::try_from(self.u64()?).ok()?;
        (n.checked_mul(min)? <= self.0.len()).then_some(n)
    }
}

/// A contraction hierarchy as it was saved with a graph (see [`InternedGraph::read_lazy`]).
#[derive(Debug, Clone)]
pub struct SavedHierarchy(Vec<u8>);

impl SavedHierarchy {
    /// The hierarchy, checked and customized for `graph` (the graph it was saved with).
    /// `None` if the saved bytes do not hold a valid hierarchy for it.
    pub fn load(&self, graph: &Graph) -> Option<Cch> {
        let mut r = Reader(&self.0);
        let h = Cch::read(&mut r, graph)?;
        r.0.is_empty().then_some(h)
    }
}

impl<K: Hash + Eq + Clone> InternedGraph<K> {
    /// Appends the graph, its `directed` flag and, if given, its customizable contraction
    /// hierarchy (the metric-independent part; it is customized again on reading).
    pub fn write(
        &self,
        out: &mut Vec<u8>,
        directed: bool,
        hierarchy: Option<&Cch>,
        key: impl Fn(&K) -> String,
    ) {
        let mut w = Writer(out);
        w.0.extend_from_slice(MAGIC);
        w.u8(u8::from(directed));
        w.u64(self.vertices.len() as u64);
        for v in &self.vertices {
            w.str(&key(v));
        }
        w.u64(self.edges.len() as u64);
        for e in &self.edges {
            w.str(&key(e));
        }
        w.u64(self.rows.len() as u64);
        for r in &self.rows {
            w.u32(r.id);
            w.u32(r.source);
            w.u32(r.target);
            w.f64(r.cost);
            w.f64(r.reverse_cost.unwrap_or(f64::NAN));
        }
        match &self.graph.coords {
            Some(c) => {
                w.u8(1);
                for &(x, y) in c {
                    w.f64(x);
                    w.f64(y);
                }
            }
            None => w.u8(0),
        }
        match hierarchy {
            Some(h) => {
                w.u8(2);
                h.write(&mut w);
            }
            None => w.u8(0),
        }
    }

    /// Reads what [`InternedGraph::write`] wrote, rebuilding the adjacency. `None` if the bytes
    /// are not a graph of this version or a key does not parse.
    #[allow(clippy::type_complexity)]
    pub fn read(
        bytes: &[u8],
        parse: impl Fn(&str) -> Option<K>,
    ) -> Option<(Self, bool, Option<Cch>)> {
        let (graph, directed, saved) = Self::read_lazy(bytes, parse)?;
        let hierarchy = match saved {
            Some(saved) => Some(saved.load(&graph.graph)?),
            None => None,
        };
        Some((graph, directed, hierarchy))
    }

    /// [`InternedGraph::read`] without reading the contraction hierarchy: it is returned as
    /// it was saved, for [`SavedHierarchy::load`] when a query needs it. Checking and
    /// customizing a large hierarchy takes seconds, and many queries (Dijkstra, A*, driving
    /// distance) do not use it.
    pub fn read_lazy(
        bytes: &[u8],
        parse: impl Fn(&str) -> Option<K>,
    ) -> Option<(Self, bool, Option<SavedHierarchy>)> {
        let mut r = Reader(bytes);
        if r.take(MAGIC.len())? != MAGIC {
            return None;
        }
        let directed = r.u8()? == 1;
        let n = r.len(8)?;
        let mut vertices = Vec::with_capacity(n);
        let mut vertex_ids = FxMap::default();
        for i in 0..n {
            let k = parse(r.str()?)?;
            vertex_ids.insert(k.clone(), i as u32);
            vertices.push(k);
        }
        let m = r.len(8)?;
        let edges = (0..m)
            .map(|_| parse(r.str()?))
            .collect::<Option<Vec<_>>>()?;
        let rows_len = r.len(28)?;
        let mut rows = Vec::with_capacity(rows_len);
        for _ in 0..rows_len {
            let (id, source, target) = (r.u32()?, r.u32()?, r.u32()?);
            let (cost, rc) = (r.f64()?, r.f64()?);
            if id as usize >= m || source as usize >= n || target as usize >= n {
                return None;
            }
            rows.push(EdgeRow {
                id,
                source,
                target,
                cost,
                reverse_cost: (!rc.is_nan()).then_some(rc),
            });
        }
        let mut graph = Graph::build(n, &rows, directed);
        if r.u8()? == 1 {
            let coords = (0..n)
                .map(|_| Some((r.f64()?, r.f64()?)))
                .collect::<Option<Vec<_>>>()?;
            graph = graph.with_coords(coords);
        }
        let hierarchy = match r.u8()? {
            // A contraction hierarchy from an earlier version: read past it, build anew.
            1 => {
                ContractionHierarchy::read(&mut r, n)?;
                r.0.is_empty().then_some(())?;
                None
            }
            // The rest of the file.
            2 => Some(SavedHierarchy(std::mem::take(&mut r.0).to_vec())),
            _ => {
                r.0.is_empty().then_some(())?;
                None
            }
        };
        Some((
            Self {
                graph,
                vertex_ids,
                vertices,
                edges,
                rows,
            },
            directed,
            hierarchy,
        ))
    }
}
