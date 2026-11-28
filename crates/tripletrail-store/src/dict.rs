//! The term dictionary: every term that is not stored inside its id gets a dense ordinal.
//!
//! Keys are kept back to back in one byte arena with an end-offset array, and a hash table
//! of ordinals finds a key's ordinal. That costs about 12 bytes per term on top of the
//! key itself, against several pointers per term for a `HashMap<String, u64>`.
//!
//! A checkpoint saves an index of each dictionary piece next to it (see [`SavedIndex`]),
//! which a store maps when it opens: the table on the heap then holds only the keys added
//! since, and opening hashes no keys (#218).

use crate::buf::Buf;
use crate::id::{Kind, TermId};
use hashbrown::HashTable;
use oxigraph::model::vocab::xsd;
use oxigraph::model::{BaseDirection, BlankNode, Literal, NamedNode, Term, TermRef, Triple};
use std::hash::{BuildHasher, Hasher as _};
use tripletrail_geo::{GEOJSON_LITERAL, GML_LITERAL, H3_LITERAL, KML_LITERAL, WKT_LITERAL};

/// Keys of geometry literals run to hundreds of kilobytes: hashed from a sample of their bytes.
type Hasher = tripletrail_geo::FastBuildHasher;

#[derive(Default, Clone)]
pub(crate) struct Dictionary {
    /// The keys, in pieces of consecutive ordinals: those of a checkpoint's dictionary files
    /// (mapped, or copied) and, last, the keys added since, on the heap. Ordinals start at 1.
    pieces: Vec<Piece>,
    /// The ordinals of the keys in pieces without a saved index.
    table: HashTable<u32>,
    hasher: Hasher,
    /// Whether keys may be compact (#370): only then is a key of another length than the
    /// one looked for read, to see whether it is a compact one (see [`Dictionary::key_is`]).
    compact: bool,
    /// The namespaces of compact IRI keys.
    ns: Namespaces,
}

/// Keys with consecutive ordinals from `first`.
#[derive(Clone, Default)]
pub(crate) struct Piece {
    first: u32,
    /// The keys, back to back. A key is the kind byte followed by the term's text.
    pub(crate) bytes: Buf<u8>,
    /// `ends[j]` is where key `first + j` starts and `ends[j + 1]` where it ends;
    /// `ends[0] == 0`.
    pub(crate) ends: Buf<u64>,
    /// The piece's saved index, read from a checkpoint; `None` when its keys are in the
    /// table on the heap.
    index: Option<SavedIndex>,
    /// Its keys are in a file a load spilled them to ([`Dictionary::map_tail`]), mapped
    /// without readahead (see `Spill::map_keys`).
    spilled: bool,
}

/// A saved hash index of a dictionary piece: open addressing with linear probing over
/// `slots`, each `tag << 32 | ordinal` (0 when empty), where `tag` is the low half of the key's
/// hash and the high half picks the first slot. The tag settles almost every probe without
/// reading the key, which may be on disk. A third of the slots stay empty.
#[derive(Clone, Default)]
pub(crate) struct SavedIndex(pub(crate) Buf<u64>);

/// A piece's saved index is used when the piece holds at least this fraction (1/n) of the
/// keys: there are then at most this many to probe, and usually one.
const LARGE_PIECE: usize = 8;

/// Slots of a saved index of `keys` keys.
pub(crate) fn index_slots(keys: usize) -> usize {
    keys + keys / 2 + 1
}

/// The hash of a fixed key: saved with an index, which is only used when it matches, so an
/// index saved by a build that hashed differently is rebuilt rather than misread.
pub(crate) fn index_check() -> u64 {
    hash_key(
        &Hasher::default(),
        b"\x01http://www.w3.org/2001/XMLSchema#string",
    )
}

fn first_slot(h: u64, slots: usize) -> usize {
    ((u128::from(h) * slots as u128) >> 64) as usize
}

impl SavedIndex {
    /// The ordinal in `range` whose key `eq` accepts, among those with hash `h`.
    fn find(
        &self,
        h: u64,
        range: std::ops::Range<u32>,
        mut eq: impl FnMut(u32) -> bool,
    ) -> Option<u32> {
        let slots = &self.0[..];
        let n = slots.len();
        let tag = h as u32;
        let mut i = first_slot(h, n);
        // At most every slot once: a damaged file can neither loop nor point outside.
        for _ in 0..n {
            let s = slots[i];
            if s == 0 {
                return None;
            }
            let o = s as u32;
            if (s >> 32) as u32 == tag && range.contains(&o) && eq(o) {
                return Some(o);
            }
            i += 1;
            if i == n {
                i = 0;
            }
        }
        None
    }
}

impl Piece {
    fn len(&self) -> usize {
        self.ends.len() - 1
    }

    fn ordinals(&self) -> std::ops::Range<u32> {
        self.first..self.first + self.len() as u32
    }

    fn key(&self, ordinal: u32) -> &[u8] {
        let j = (ordinal - self.first) as usize;
        &self.bytes[self.ends[j] as usize..self.ends[j + 1] as usize]
    }
}

/// The piece of `pieces` that holds `ordinal`.
fn piece_of(pieces: &[Piece], ordinal: u32) -> &Piece {
    // Usually one or two pieces: the last is checked first, then the others in turn.
    let last = pieces.last().expect("a dictionary has a piece");
    if ordinal >= last.first {
        return last;
    }
    &pieces[pieces.partition_point(|p| p.first <= ordinal) - 1]
}

/// Namespaces shorter than this are not kept: an IRI key that refers to one saves its bytes
/// less two (the 0 and the namespace's number).
const NAMESPACE_MIN: usize = 8;

/// The namespaces an IRI is looked up under, at most: its text up to each of its last `/` or
/// `#`.
const CUTS: usize = 8;

/// At most this many namespaces seen once are remembered (see [`Namespaces::seen`]).
const SEEN_MAX: usize = 1 << 20;

/// The namespaces of compact IRI keys (#152): an IRI's text up to and including its last `/`
/// or `#`, kept once, numbered from 0 in the order kept. A compact IRI key is
/// `[Kind::Iri][0]`, the number of its namespace (a varint) and the rest of the IRI; an IRI
/// written in full has no 0 byte. The namespaces only grow, so the keys of a checkpoint refer
/// to namespaces that every later checkpoint still has.
#[derive(Clone)]
pub(crate) struct Namespaces {
    bytes: Vec<u8>,
    /// `ends[i]..ends[i + 1]` is namespace `i`; `ends[0] == 0`.
    ends: Vec<u64>,
    table: HashTable<u32>,
    /// The hashes of namespaces seen once and not kept: a namespace is kept when a second IRI
    /// has it, so that an IRI alone in its namespace stays as it is rather than also writing
    /// the namespace. Not saved: after a restart, an IRI may be written in full where it would
    /// have been compact, which every lookup handles.
    seen: hashbrown::HashSet<u64, rustc_hash::FxBuildHasher>,
}

impl Default for Namespaces {
    fn default() -> Self {
        Self {
            bytes: Vec::new(),
            ends: vec![0],
            table: HashTable::new(),
            seen: Default::default(),
        }
    }
}

fn namespace_hash(ns: &[u8]) -> u64 {
    rustc_hash::FxBuildHasher.hash_one(ns)
}

impl Namespaces {
    /// Namespaces kept.
    pub(crate) fn len(&self) -> usize {
        self.ends.len() - 1
    }

    fn get(&self, i: u64) -> Option<&[u8]> {
        let i = usize::try_from(i).ok()?;
        let (&start, &end) = (self.ends.get(i)?, self.ends.get(i + 1)?);
        Some(&self.bytes[start as usize..end as usize])
    }

    fn find(&self, ns: &[u8]) -> Option<u32> {
        let (bytes, ends) = (&self.bytes, &self.ends);
        self.table
            .find(namespace_hash(ns), |&i| {
                &bytes[ends[i as usize] as usize..ends[i as usize + 1] as usize] == ns
            })
            .copied()
    }

    /// The number of namespace `ns`, not kept: kept now if an IRI had it before, else noted.
    fn note(&mut self, ns: &[u8]) -> Option<u32> {
        let h = namespace_hash(ns);
        if self.seen.len() >= SEEN_MAX {
            self.seen.clear();
        }
        if self.seen.insert(h) {
            return None;
        }
        self.seen.remove(&h);
        let i = u32::try_from(self.len()).ok()?;
        self.bytes.extend_from_slice(ns);
        self.ends.push(self.bytes.len() as u64);
        let (bytes, ends) = (&self.bytes, &self.ends);
        self.table.insert_unique(h, i, |&j| {
            namespace_hash(&bytes[ends[j as usize] as usize..ends[j as usize + 1] as usize])
        });
        Some(i)
    }

    /// The namespace and the rest of a compact IRI key; `None` for any other key, and for a
    /// compact key of an unknown namespace (a damaged one: see [`Dictionary::valid_parts`]).
    pub(crate) fn parts<'a>(&'a self, key: &'a [u8]) -> Option<(&'a [u8], &'a [u8])> {
        if key.len() < 3 || key[0] != Kind::Iri as u8 || key[1] != 0 {
            return None;
        }
        let mut at = 2;
        let i = crate::segment::get_varint(key, &mut at)?;
        Some((self.get(i)?, &key[at..]))
    }

    /// The namespaces as saved: their bytes and ends.
    pub(crate) fn saved(&self) -> (&[u8], &[u64]) {
        (&self.bytes, &self.ends)
    }

    /// Namespaces read back from [`Namespaces::saved`]; `None` unless the ends start at 0,
    /// never go back and end at the last byte, and every namespace is UTF-8.
    pub(crate) fn from_saved(bytes: Vec<u8>, ends: Vec<u64>) -> Option<Self> {
        if ends.first() != Some(&0) || ends.last() != Some(&(bytes.len() as u64)) {
            return None;
        }
        if !ends.windows(2).all(|w| {
            w[0] <= w[1] && std::str::from_utf8(&bytes[w[0] as usize..w[1] as usize]).is_ok()
        }) {
            return None;
        }
        let mut ns = Self {
            bytes,
            ends,
            ..Self::default()
        };
        let mut table = HashTable::with_capacity(ns.len());
        for i in 0..ns.len() {
            let h = namespace_hash(ns.get(i as u64).expect("in range"));
            table.insert_unique(h, i as u32, |_| unreachable!("the table has room"));
        }
        ns.table = table;
        Some(ns)
    }

    fn heap_size(&self) -> usize {
        self.bytes.capacity() + self.ends.capacity() * 8 + self.table.capacity() * 4
    }
}

/// A text in two parts, ordered as the two written one after the other: an IRI of a compact
/// key, its namespace then the rest, read without copying them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Text<'a>(pub(crate) &'a [u8], pub(crate) &'a [u8]);

impl Text<'_> {
    pub(crate) fn starts_with(&self, prefix: &[u8]) -> bool {
        if prefix.len() <= self.0.len() {
            return self.0.starts_with(prefix);
        }
        let (head, rest) = prefix.split_at(self.0.len());
        head == self.0 && self.1.starts_with(rest)
    }

    /// The order of this text and `other` written out.
    pub(crate) fn cmp_bytes(&self, other: &[u8]) -> std::cmp::Ordering {
        self.cmp(&Text(other, &[]))
    }

    pub(crate) fn to_vec(self) -> Vec<u8> {
        [self.0, self.1].concat()
    }
}

impl Ord for Text<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        // The same namespace (one slice of the namespaces): the rests decide.
        if std::ptr::eq(self.0, other.0) {
            return self.1.cmp(other.1);
        }
        let (mut a, mut b) = ((self.0, self.1), (other.0, other.1));
        loop {
            if a.0.is_empty() {
                if a.1.is_empty() {
                    break;
                }
                a = (a.1, &[]);
                continue;
            }
            if b.0.is_empty() {
                if b.1.is_empty() {
                    break;
                }
                b = (b.1, &[]);
                continue;
            }
            let n = a.0.len().min(b.0.len());
            match a.0[..n].cmp(&b.0[..n]) {
                Ordering::Equal => (a.0, b.0) = (&a.0[n..], &b.0[n..]),
                o => return o,
            }
        }
        (a.0.len() + a.1.len()).cmp(&(b.0.len() + b.1.len()))
    }
}

impl PartialOrd for Text<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Text<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for Text<'_> {}

/// The hash of `key` as written in full: a compact IRI key is hashed as its full key, so that
/// a lookup by the full key finds it.
fn hash_full(hasher: &Hasher, ns: &Namespaces, key: &[u8]) -> u64 {
    thread_local! {
        static FULL: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    match ns.parts(key) {
        Some((n, rest)) => FULL.with_borrow_mut(|full| {
            full.clear();
            full.extend_from_slice(n);
            full.extend_from_slice(rest);
            hash_parts(hasher, &key[..1], full)
        }),
        None => hash_key(hasher, key),
    }
}

/// How a term is stored: in its id, or in the dictionary under a key.
pub(crate) enum Encoded {
    Inline(TermId),
    Key(Kind),
}

pub(crate) fn is_geometry_datatype(datatype: &str) -> bool {
    datatype == WKT_LITERAL.as_str()
        || datatype == GEOJSON_LITERAL.as_str()
        || datatype == GML_LITERAL.as_str()
        || datatype == KML_LITERAL.as_str()
        || datatype == H3_LITERAL.as_str()
}

/// Writes the dictionary key of `term` into `key`, or returns its inline id.
pub(crate) fn encode(term: TermRef<'_>, key: &mut Vec<u8>) -> Encoded {
    key.clear();
    let kind = match term {
        TermRef::NamedNode(n) => {
            key.push(Kind::Iri as u8);
            key.extend_from_slice(n.as_str().as_bytes());
            Kind::Iri
        }
        TermRef::BlankNode(b) => {
            key.push(Kind::Blank as u8);
            key.extend_from_slice(b.as_str().as_bytes());
            Kind::Blank
        }
        TermRef::Literal(l) => {
            let value = l.value();
            if let Some(language) = l.language() {
                key.push(Kind::LangString as u8);
                key.extend_from_slice(language.as_bytes());
                // A language tag has no empty subtag, so `--` marks the RDF 1.2 direction.
                if let Some(direction) = l.direction() {
                    key.extend_from_slice(format!("--{direction}").as_bytes());
                }
                key.push(0);
                key.extend_from_slice(value.as_bytes());
                Kind::LangString
            } else {
                let datatype = l.datatype();
                if datatype == xsd::STRING {
                    key.push(Kind::String as u8);
                    key.extend_from_slice(value.as_bytes());
                    Kind::String
                } else {
                    if datatype == xsd::INTEGER
                        && let Ok(v) = value.parse::<i64>()
                        && let Some(id) = TermId::integer(v)
                        && canonical_integer(value)
                    {
                        return Encoded::Inline(id);
                    }
                    let inline = if datatype == xsd::DOUBLE {
                        TermId::number(Kind::Double, value)
                    } else if datatype == xsd::DECIMAL {
                        TermId::number(Kind::Decimal, value)
                    } else if datatype == xsd::DATE_TIME {
                        TermId::date_time(value, false)
                    } else if datatype == xsd::DATE {
                        TermId::date_time(value, true)
                    } else {
                        None
                    };
                    if let Some(id) = inline {
                        return Encoded::Inline(id);
                    }
                    if datatype == xsd::BOOLEAN {
                        match value {
                            "true" => return Encoded::Inline(TermId::boolean(true)),
                            "false" => return Encoded::Inline(TermId::boolean(false)),
                            _ => {}
                        }
                    }
                    let kind = if is_geometry_datatype(datatype.as_str()) {
                        Kind::Geometry
                    } else {
                        Kind::Typed
                    };
                    key.push(kind as u8);
                    key.extend_from_slice(datatype.as_str().as_bytes());
                    key.push(0);
                    key.extend_from_slice(value.as_bytes());
                    kind
                }
            }
        }
        TermRef::Triple(t) => {
            key.push(Kind::Triple as u8);
            key.extend_from_slice(t.to_string().as_bytes());
            Kind::Triple
        }
    };
    Encoded::Key(kind)
}

/// A key split into what comes before the term's text (the kind byte, and the datatype or
/// language with its separator) and the text, which is hashed in place.
pub(crate) fn split_key(key: &[u8]) -> (&[u8], &[u8]) {
    let k = key[0];
    if k == Kind::Iri as u8
        || k == Kind::Blank as u8
        || k == Kind::String as u8
        || k == Kind::Triple as u8
    {
        key.split_at(1)
    } else {
        let zero = key
            .iter()
            .position(|&c| c == 0)
            .expect("keys have a separator");
        key.split_at(zero + 1)
    }
}

fn hash_parts(hasher: &Hasher, prefix: &[u8], value: &[u8]) -> u64 {
    let mut h = hasher.build_hasher();
    h.write(prefix);
    h.write(value);
    h.finish()
}

fn hash_key(hasher: &Hasher, key: &[u8]) -> u64 {
    if let Some(h) = compact_hash(key) {
        return h;
    }
    let (prefix, value) = split_key(key);
    hash_parts(hasher, prefix, value)
}

/// The bytes before a compact key's style and record: kind, 0, and the hash of its full key.
const COMPACT_HEADER: usize = 2 + 8;

/// Whether `key` is a compact WKT key (#370): `[Kind::Geometry][0]`, the hash of the full
/// key (8 bytes, little-endian), the [`crate::wkt`] style (2 bytes) and the geometry record.
/// A full key has its datatype IRI where the 0 is, which is never empty.
pub(crate) fn is_compact(key: &[u8]) -> bool {
    key.len() > COMPACT_HEADER + 2 && key[0] == Kind::Geometry as u8 && key[1] == 0
}

fn compact_hash(key: &[u8]) -> Option<u64> {
    is_compact(key).then(|| u64::from_le_bytes(key[2..COMPACT_HEADER].try_into().unwrap()))
}

/// The geometry record of a compact key.
pub(crate) fn compact_record(key: &[u8]) -> Option<&[u8]> {
    is_compact(key).then(|| &key[COMPACT_HEADER + 2..])
}

/// The full key of a compact key: the WKT text written from its record.
fn expand(key: &[u8]) -> Vec<u8> {
    let mut full = String::with_capacity(WKT_LITERAL.as_str().len() + 2 + key.len() * 2);
    full.push(Kind::Geometry as u8 as char);
    full.push_str(WKT_LITERAL.as_str());
    full.push('\0');
    write_text(key, &mut full);
    full.into_bytes()
}

/// Appends the WKT text of the compact key `key` to `out`.
fn write_text(key: &[u8], out: &mut String) {
    let style = [key[COMPACT_HEADER], key[COMPACT_HEADER + 1]];
    crate::wkt::write(&key[COMPACT_HEADER + 2..], style, out)
        .expect("compact keys are checked when written and read");
}

/// What comes before the text in the key of a WKT literal.
fn wkt_prefix() -> Vec<u8> {
    let mut prefix = vec![Kind::Geometry as u8];
    prefix.extend_from_slice(WKT_LITERAL.as_str().as_bytes());
    prefix.push(0);
    prefix
}

/// Writes the compact form of the full key `key` (a WKT literal) to `out`, given the record of
/// its geometry: `false` (and `out` unchanged) when the text written from the record is not
/// exactly its text, which then stays as it is.
pub(crate) fn compact(key: &[u8], record: &[u8], out: &mut Vec<u8>) -> bool {
    let prefix = wkt_prefix();
    let Some(text) = key
        .strip_prefix(prefix.as_slice())
        .and_then(|t| std::str::from_utf8(t).ok())
    else {
        return false;
    };
    let Some(style) = crate::wkt::style_of(text, record) else {
        return false;
    };
    let h = hash_parts(&Hasher::default(), &prefix, text.as_bytes());
    out.extend_from_slice(&[Kind::Geometry as u8, 0]);
    out.extend_from_slice(&h.to_le_bytes());
    out.extend_from_slice(&style);
    // Packed when its coordinates are fixed-point numbers (#152), else as it is.
    let hint = (style[0] != crate::wkt::SHORTEST).then_some(style[0]);
    if !crate::geobin::pack_any(record, hint, out) {
        out.extend_from_slice(record);
    }
    true
}

/// The bytes of a typed or language key up to and including the 0 after its datatype or
/// tag.
fn prefix_len(key: &[u8]) -> usize {
    key.iter().position(|&b| b == 0).map_or(0, |i| i + 1)
}

/// Whether `value` is the canonical lexical form of an `xsd:integer` (no `+`, no leading
/// zeros, no `-0`), so storing the number loses nothing.
fn canonical_integer(value: &str) -> bool {
    let digits = value.strip_prefix('-').unwrap_or(value);
    !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" && !value.starts_with('-') || !digits.starts_with('0'))
}

/// The term an inline id stands for.
pub(crate) fn inline_term(id: TermId) -> Option<Term> {
    match id.kind() {
        Kind::Integer => {
            Some(Literal::new_typed_literal(id.as_integer().to_string(), xsd::INTEGER).into())
        }
        Kind::Boolean => Some(Literal::from(id.payload() != 0).into()),
        Kind::Double => Some(Literal::new_typed_literal(id.number_lexical(), xsd::DOUBLE).into()),
        Kind::Decimal => Some(Literal::new_typed_literal(id.number_lexical(), xsd::DECIMAL).into()),
        Kind::DateTime => {
            let date = id.payload() & 1 == 1;
            let datatype = if date { xsd::DATE } else { xsd::DATE_TIME };
            Some(Literal::new_typed_literal(id.date_time_lexical(), datatype).into())
        }
        _ => None,
    }
}

/// The term a dictionary key stands for.
pub(crate) fn decode(key: &[u8]) -> Term {
    let text = |b: &[u8]| String::from_utf8(b.to_vec()).expect("keys are UTF-8");
    let split = |b: &[u8]| {
        let zero = b
            .iter()
            .position(|&c| c == 0)
            .expect("keys have a separator");
        (text(&b[..zero]), text(&b[zero + 1..]))
    };
    let (kind, rest) = (key[0], &key[1..]);
    match kind {
        k if k == Kind::Iri as u8 => NamedNode::new_unchecked(text(rest)).into(),
        k if k == Kind::Blank as u8 => BlankNode::new_unchecked(text(rest)).into(),
        k if k == Kind::String as u8 => Literal::new_simple_literal(text(rest)).into(),
        k if k == Kind::LangString as u8 => {
            let (language, value) = split(rest);
            match language.split_once("--") {
                Some((language, "ltr")) => {
                    Literal::new_directional_language_tagged_literal_unchecked(
                        value,
                        language,
                        BaseDirection::Ltr,
                    )
                    .into()
                }
                Some((language, _)) => Literal::new_directional_language_tagged_literal_unchecked(
                    value,
                    language,
                    BaseDirection::Rtl,
                )
                .into(),
                None => Literal::new_language_tagged_literal_unchecked(value, language).into(),
            }
        }
        k if k == Kind::Triple as u8 => text(rest)
            .parse::<Triple>()
            .expect("triple keys are N-Triples")
            .into(),
        _ => {
            let (datatype, value) = split(rest);
            Literal::new_typed_literal(value, NamedNode::new_unchecked(datatype)).into()
        }
    }
}

/// What the dictionary's keys of one kind of term take (#152): see [`Dictionary::usage`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KindUsage {
    /// `iri`, `blank`, `string`, `langString`, `typed`, `wkt`, `wktBinary`, `geometry` (other
    /// geometry literals) or `triple`.
    pub kind: &'static str,
    /// Keys of this kind.
    pub keys: u64,
    /// Their bytes in `dict.bin`, kind byte, datatype or language tag included.
    pub bytes: u64,
    /// Of those, the bytes of the datatype IRI or language tag (typed and geometry
    /// literals, language strings), written again in every key.
    pub prefix_bytes: u64,
    /// For the IRIs written in full, the bytes of their namespaces (up to the last `/` or
    /// `#`): IRIs that share a namespace with an earlier one refer to it (#152).
    pub namespace_bytes: u64,
}

/// The kinds of [`KindUsage::kind`], in the order [`Dictionary::usage`] lists them.
const USAGE_KINDS: [&str; 9] = [
    "iri",
    "blank",
    "string",
    "langString",
    "typed",
    "wkt",
    "wktBinary",
    "geometry",
    "triple",
];

impl Dictionary {
    /// What the keys take, by kind of term (#152), read from every key: the kinds that have
    /// keys, in a fixed order. `dict.idx` (each key's end, 8 bytes) is not counted.
    pub(crate) fn usage(&self) -> Vec<KindUsage> {
        let mut usage: Vec<KindUsage> = USAGE_KINDS
            .iter()
            .map(|&kind| KindUsage {
                kind,
                ..KindUsage::default()
            })
            .collect();
        let wkt = wkt_prefix();
        for p in &self.pieces {
            for j in 0..p.len() {
                let key = &p.bytes[p.ends[j] as usize..p.ends[j + 1] as usize];
                let Some(&k) = key.first() else { continue };
                let (slot, prefix) = match k {
                    k if k == Kind::Iri as u8 => (0, 0),
                    k if k == Kind::Blank as u8 => (1, 0),
                    k if k == Kind::String as u8 => (2, 0),
                    k if k == Kind::LangString as u8 => (3, prefix_len(key)),
                    k if k == Kind::Typed as u8 => (4, prefix_len(key)),
                    k if k == Kind::Geometry as u8 && is_compact(key) => (6, 0),
                    k if k == Kind::Geometry as u8 && key.starts_with(&wkt) => (5, wkt.len()),
                    k if k == Kind::Geometry as u8 => (7, prefix_len(key)),
                    _ => (8, 0),
                };
                let u = &mut usage[slot];
                u.keys += 1;
                u.bytes += key.len() as u64;
                u.prefix_bytes += prefix as u64;
                if slot == 0 && self.ns.parts(key).is_none() {
                    let iri = &key[1..];
                    u.namespace_bytes += iri
                        .iter()
                        .rposition(|&b| b == b'/' || b == b'#')
                        .map_or(0, |i| i as u64 + 1);
                }
            }
        }
        usage.retain(|u| u.keys > 0);
        usage
    }

    pub(crate) fn new() -> Self {
        Self::from_pieces(Vec::new())
    }

    /// Number of keys.
    pub(crate) fn len(&self) -> usize {
        let last = self.pieces.last().expect("a dictionary has a piece");
        last.first as usize - 1 + last.len()
    }

    pub(crate) fn heap_size(&self) -> usize {
        self.pieces
            .iter()
            .map(|p| {
                p.bytes.heap_size()
                    + p.ends.heap_size()
                    + p.index.as_ref().map_or(0, |i| i.0.heap_size())
            })
            .sum::<usize>()
            + self.table.capacity() * 4
            + self.ns.heap_size()
    }

    /// The key of `ordinal`: a compact WKT key is given as its full key (see [`is_compact`]).
    #[cfg(test)]
    pub(crate) fn key(&self, ordinal: u32) -> std::borrow::Cow<'_, [u8]> {
        let key = self.raw_key(ordinal);
        if is_compact(key) {
            std::borrow::Cow::Owned(expand(key))
        } else if let Some((ns, rest)) = self.ns.parts(key) {
            std::borrow::Cow::Owned([&key[..1], ns, rest].concat())
        } else {
            std::borrow::Cow::Borrowed(key)
        }
    }

    /// The term of `ordinal`: [`decode`] of its key, without writing out a compact key's full
    /// key first.
    pub(crate) fn term(&self, ordinal: u32) -> Term {
        let key = self.raw_key(ordinal);
        if is_compact(key) {
            let mut text = String::with_capacity(key.len() * 2);
            write_text(key, &mut text);
            return Literal::new_typed_literal(text, WKT_LITERAL).into();
        }
        if let Some((ns, rest)) = self.ns.parts(key) {
            let iri = String::from_utf8(Text(ns, rest).to_vec()).expect("keys are UTF-8");
            return NamedNode::new_unchecked(iri).into();
        }
        decode(key)
    }

    /// The namespaces of compact IRI keys.
    pub(crate) fn namespaces(&self) -> &Namespaces {
        &self.ns
    }

    /// The text of an IRI key (without its kind byte), in two parts when compact.
    pub(crate) fn iri_text<'a>(&'a self, key: &'a [u8]) -> Option<Text<'a>> {
        if key.first() != Some(&(Kind::Iri as u8)) {
            return None;
        }
        Some(match self.ns.parts(key) {
            Some((ns, rest)) => Text(ns, rest),
            None => Text(&key[1..], &[]),
        })
    }

    /// The key of `ordinal` as stored: compact WKT keys as they are.
    pub(crate) fn raw_key(&self, ordinal: u32) -> &[u8] {
        piece_of(&self.pieces, ordinal).key(ordinal)
    }

    /// The geometry record of `ordinal`, if its key is compact.
    pub(crate) fn geometry_record(&self, ordinal: u32) -> Option<&[u8]> {
        compact_record(self.raw_key(ordinal))
    }

    /// Whether the key of `ordinal` is `prefix` followed by `value`.
    fn key_is(&self, ordinal: u32, prefix: &[u8], value: &[u8]) -> bool {
        let k = self.raw_key(ordinal);
        let is = |k: &[u8]| {
            k.len() == prefix.len() + value.len()
                && k.starts_with(prefix)
                && &k[prefix.len()..] == value
        };
        // The length first: the bytes of a key of another length are not read. A probe
        // compares a few keys that are not the one looked for, and in a mapped dictionary
        // larger than memory reading one is a read from disk (a load of Europe 1B took 4x as
        // long while this read every compared key, #218).
        if is(k) {
            return true;
        }
        // A compact IRI key (#152) is shorter than the IRI's full key.
        if prefix.first() == Some(&(Kind::Iri as u8))
            && k.len() < prefix.len() + value.len()
            && let Some((ns, rest)) = self.ns.parts(k)
        {
            let full = [&k[..1], ns, rest];
            return full.iter().map(|s| s.len()).sum::<usize>() == prefix.len() + value.len()
                && full
                    .iter()
                    .flat_map(|s| s.iter())
                    .eq(prefix.iter().chain(value));
        }
        // A compact key (#370) holds a WKT literal without its text: only a WKT literal can
        // be one, and only in a dictionary that may hold compact keys.
        self.compact
            && prefix.first() == Some(&(Kind::Geometry as u8))
            && is_compact(k)
            && is(&expand(k))
    }

    /// Whether the keys may be compact ones (#370): those of a checkpoint written with WKT
    /// literals in binary form, or read from one.
    pub(crate) fn may_hold_compact(&self) -> bool {
        self.compact
    }

    /// Notes that keys may be compact from now on (`on`); a dictionary that may hold compact
    /// keys keeps doing so.
    pub(crate) fn note_compact(&mut self, on: bool) {
        self.compact |= on;
    }

    fn hash(&self, key: &[u8]) -> u64 {
        hash_full(&self.hasher, &self.ns, key)
    }

    /// The key to store for the full key `key`, not found: a compact one for an IRI whose
    /// namespace is kept (or kept now), else `key` itself.
    fn stored<'a>(&mut self, key: &'a [u8], out: &'a mut Vec<u8>) -> &'a [u8] {
        if key.first() != Some(&(Kind::Iri as u8)) {
            return key;
        }
        let iri = &key[1..];
        // The namespaces it may have, longest first: its text up to each `/` or `#`, from the
        // last. An IRI under another (`…/way/1/geom/track` under `…/way/1`) then shares the
        // namespace of the shorter one (`…/way/`) rather than one of its own.
        let mut cuts = [0; CUTS];
        let mut n = 0;
        for (i, &b) in iri.iter().enumerate().rev() {
            if n == CUTS || i + 1 < NAMESPACE_MIN {
                break;
            }
            if b == b'/' || b == b'#' {
                cuts[n] = i + 1;
                n += 1;
            }
        }
        let cuts = &cuts[..n];
        // The longest kept, else the longest that an IRI had before.
        let Some((i, cut)) = cuts
            .iter()
            .find_map(|&c| Some((self.ns.find(&iri[..c])?, c)))
            .or_else(|| {
                cuts.iter()
                    .find_map(|&c| Some((self.ns.note(&iri[..c])?, c)))
            })
        else {
            return key;
        };
        out.clear();
        out.extend_from_slice(&[Kind::Iri as u8, 0]);
        crate::segment::put_varint(out, u64::from(i));
        out.extend_from_slice(&iri[cut..]);
        out
    }

    pub(crate) fn get(&self, key: &[u8]) -> Option<u32> {
        let h = self.hash(key);
        find(&self.table, &self.pieces, h, |o| self.key_is(o, key, &[]))
    }

    /// The ordinal of the key `prefix` followed by `value` (see [`split_key`]), without
    /// copying the value: geometry literals are looked up by their text, which can be long.
    pub(crate) fn get_split(&self, prefix: &[u8], value: &[u8]) -> Option<u32> {
        let h = hash_parts(&self.hasher, prefix, value);
        find(&self.table, &self.pieces, h, |o| {
            self.key_is(o, prefix, value)
        })
    }

    pub(crate) fn get_or_insert(&mut self, key: &[u8]) -> (u32, bool) {
        let h = self.hash(key);
        if let Some(o) = find(&self.table, &self.pieces, h, |o| self.key_is(o, key, &[])) {
            return (o, false);
        }
        let ordinal = u32::try_from(self.len() + 1).expect("fewer than 2^32 terms");
        let mut compact = Vec::new();
        let key = self.stored(key, &mut compact);
        let Self {
            pieces,
            table,
            hasher,
            ns,
            ..
        } = self;
        if table.len() == table.capacity() {
            // Grown here rather than by the table, which would rehash in bucket order,
            // reading the keys at random (#292).
            *table = build_table(hasher, ns, pieces, (table.capacity() * 2).max(16));
        }
        // New keys go to a piece on the heap: the pieces of a checkpoint stay as they are.
        if matches!(pieces.last(), Some(p) if p.bytes.mapped_size() > 0 || p.ends.mapped_size() > 0)
        {
            pieces.push(Piece {
                first: ordinal,
                bytes: Buf::default(),
                ends: vec![0].into(),
                index: None,
                spilled: false,
            });
        }
        let last = pieces.last_mut().expect("a dictionary has a piece");
        let bytes = last.bytes.to_mut();
        bytes.extend_from_slice(key);
        let end = bytes.len() as u64;
        last.ends.to_mut().push(end);
        table.insert_unique(h, ordinal, |&o| {
            hash_full(hasher, ns, piece_of(pieces, o).key(o))
        });
        (ordinal, true)
    }

    /// Calls `f` with each run of keys from ordinal `from + 1` on, `count` keys in all, as
    /// (bytes, ends) where the key ends count from `ends[0]`, which need not be 0.
    pub(crate) fn for_each_run(&self, from: usize, count: usize, mut f: impl FnMut(&[u8], &[u64])) {
        let (mut from, end) = (from, from + count);
        for p in &self.pieces {
            let (start, len) = (p.first as usize - 1, p.len());
            if from >= start + len || from >= end {
                continue;
            }
            let j = from - start;
            let k = (end - start).min(len);
            let ends = &p.ends[j..=k];
            f(&p.bytes[..ends[k - j] as usize], ends);
            from = start + k;
        }
    }

    /// Bytes of the keys added on the heap since the last mapped piece.
    pub(crate) fn tail_bytes(&self) -> usize {
        self.pieces
            .last()
            .filter(|p| p.bytes.mapped_size() == 0 && p.ends.mapped_size() == 0)
            .map_or(0, |p| p.bytes.len() + p.ends.len() * 8)
    }

    /// Moves the keys added on the heap since the last mapped piece to a file: `write` writes
    /// them and gives them back mapped. New keys then start another piece on the heap. So a
    /// bulk load larger than memory keeps its new keys in the page cache, not the heap (#218).
    pub(crate) fn map_tail(
        &mut self,
        write: impl FnOnce(&[u8], &[u64]) -> std::io::Result<(Buf<u8>, Buf<u64>)>,
    ) -> std::io::Result<()> {
        if self.tail_bytes() == 0 {
            return Ok(());
        }
        let last = self.pieces.last_mut().expect("a dictionary has a piece");
        if last.len() == 0 {
            return Ok(());
        }
        let (bytes, ends) = write(&last.bytes, &last.ends)?;
        debug_assert_eq!(bytes.len(), last.bytes.len());
        last.bytes = bytes;
        last.ends = ends;
        last.spilled = true;
        Ok(())
    }

    /// Runs `f`, which reads the keys in order (a checkpoint writes them), with readahead on
    /// the keys a load spilled: they are mapped without it for the load's lookups (#232),
    /// and one page at a time a pass over hundreds of GB of keys would wait on every page.
    pub(crate) fn reading_in_order<R>(&self, f: impl FnOnce() -> R) -> R {
        self.advise_spilled(true);
        let result = f();
        self.advise_spilled(false);
        result
    }

    fn advise_spilled(&self, in_order: bool) {
        for p in self.pieces.iter().filter(|p| p.spilled) {
            p.bytes.advise_in_order(in_order);
            p.ends.advise_in_order(in_order);
        }
    }

    /// Bytes of the checkpoint files this dictionary maps (0 when held on the heap).
    pub(crate) fn mapped_size(&self) -> usize {
        self.pieces
            .iter()
            .map(|p| {
                p.bytes.mapped_size()
                    + p.ends.mapped_size()
                    + p.index.as_ref().map_or(0, |i| i.0.mapped_size())
            })
            .sum()
    }

    /// Replaces the first pieces, holding the first keys, by `saved`, the same keys as read
    /// back from a checkpoint (mapped), with their saved indexes where read: the copies on
    /// the heap are let go, and so are the table's entries for the keys now indexed.
    pub(crate) fn replace_saved(&mut self, saved: Vec<SavedPiece>) {
        let mut first = 1u32;
        let mut pieces = Vec::with_capacity(saved.len() + 1);
        let keys: usize = saved.iter().map(|p| p.1.len() - 1).sum();
        // Only the indexes of large pieces are used: each costs a probe of mapped memory per
        // key looked up and not found in a newer piece, which every new key is. The keys of
        // the small pieces go to the table, which a store checks first anyway.
        let saved: Vec<SavedPiece> = saved
            .into_iter()
            .map(|(bytes, ends, index)| {
                let large = (ends.len() - 1) * LARGE_PIECE >= keys;
                (bytes, ends, index.filter(|_| large))
            })
            .collect();
        for (bytes, ends, index) in saved {
            let n = ends.len() as u32 - 1;
            pieces.push(Piece {
                first,
                bytes,
                ends,
                index,
                spilled: false,
            });
            first += n;
        }
        // Keys added after those saved (none when the caller holds the writer lock).
        if (first as usize) <= self.len() {
            let mut bytes = Vec::new();
            let mut ends = vec![0];
            self.for_each_run(
                first as usize - 1,
                self.len() + 1 - first as usize,
                |b, e| {
                    for w in e.windows(2) {
                        bytes.extend_from_slice(&b[w[0] as usize..w[1] as usize]);
                        ends.push(bytes.len() as u64);
                    }
                },
            );
            pieces.push(Piece {
                first,
                bytes: bytes.into(),
                ends: ends.into(),
                index: None,
                spilled: false,
            });
        }
        debug_assert!(pieces.last().map(|p| p.first as usize - 1 + p.len()) >= Some(self.len()));
        let indexed = |pieces: &[Piece]| -> Vec<std::ops::Range<u32>> {
            pieces
                .iter()
                .filter(|p| p.index.is_some())
                .map(Piece::ordinals)
                .collect()
        };
        let changed = indexed(&self.pieces) != indexed(&pieces);
        self.pieces = pieces;
        // The table holds the keys of the pieces without an index: built again only when the
        // keys indexed changed (the same keys, held elsewhere, need no change).
        if changed {
            self.table = build_table(&self.hasher, &self.ns, &self.pieces, 0);
        }
    }

    /// Fills `slots` (all 0, [`index_slots`] of `keys`) with the saved index of the `keys`
    /// keys after the first `from`, which [`Dictionary::replace_saved`] can then take as the
    /// index of the piece holding them. Checkpoints write it a window at a time
    /// ([`Dictionary::fill_index_window`]); this is the whole of it, for tests.
    #[cfg(test)]
    pub(crate) fn fill_index(&self, from: usize, keys: usize, slots: &mut [u64]) {
        debug_assert_eq!(slots.len(), index_slots(keys));
        let carry = self.fill_index_window(from, keys, 0, slots, &[]);
        debug_assert!(carry.is_empty(), "the whole index wraps around");
    }

    /// Fills the slots `lo..lo + window.len()` (all 0) of the saved index of the `keys` keys
    /// after the first `from`, out of its [`index_slots`]: first with `carry`, the entries an
    /// earlier window had no room left for, then with each key whose first slot is in the
    /// window, in order. Returns the entries this window has no room for, which go on in the
    /// next window (after the last, from slot 0). A window that is the whole index wraps
    /// around itself.
    ///
    /// Any order of insertion makes a valid index: each entry sits after its first slot with
    /// no empty slot between, which is all a lookup needs. A window at a time lets
    /// [`crate::persist`] build an index larger than memory on the heap, a part at a time,
    /// and write it in order, instead of in a mapped file whose pages, written at random for
    /// minutes, the system writes back again and again (60 times the index's size in a load
    /// of 400M triples under a memory cap, #232).
    pub(crate) fn fill_index_window(
        &self,
        from: usize,
        keys: usize,
        lo: usize,
        window: &mut [u64],
        carry: &[u64],
    ) -> Vec<u64> {
        use rayon::prelude::*;
        const CHUNK: usize = 1 << 20;
        let n = index_slots(keys);
        let len = window.len();
        debug_assert!(lo + len <= n);
        let whole = lo == 0 && len == n;
        let mut left = Vec::new();
        // The first empty slot from `i`, or `None` past the window's end.
        let free = |window: &[u64], mut i: usize| -> Option<usize> {
            while window[i] != 0 {
                i += 1;
                if i == len {
                    if !whole {
                        return None;
                    }
                    i = 0;
                }
            }
            Some(i)
        };
        for (k, &entry) in carry.iter().enumerate() {
            match (len > 0).then(|| free(window, 0)).flatten() {
                Some(i) => window[i] = entry,
                None => {
                    left.extend_from_slice(&carry[k..]);
                    break;
                }
            }
        }
        if len == 0 {
            return left;
        }
        let mut hashes = Vec::with_capacity(CHUNK.min(keys));
        for start in (from + 1..from + 1 + keys).step_by(CHUNK) {
            let end = (start + CHUNK).min(from + 1 + keys);
            (start..end)
                .into_par_iter()
                .map(|o| self.hash(self.raw_key(o as u32)))
                .collect_into_vec(&mut hashes);
            for (o, &h) in (start..end).zip(&hashes) {
                let first = first_slot(h, n);
                if first < lo || first >= lo + len {
                    continue;
                }
                let entry = (h & 0xFFFF_FFFF) << 32 | o as u64;
                // Once one entry runs past the window, every later one whose probe reaches
                // the end does too: kept in order for the next window.
                match free(window, first - lo) {
                    Some(i) => window[i] = entry,
                    None => left.push(entry),
                }
            }
        }
        left
    }

    /// Whether saved parts can be rebuilt into a dictionary whose keys all [`decode`]: the
    /// key ends start at 0, never go back, end at the last byte, and every key is a kind byte
    /// followed by UTF-8 text, with the separator literals need.
    /// A compact IRI key must refer to one of `ns`.
    pub(crate) fn valid_parts(bytes: &[u8], ends: &[u64], ns: &Namespaces) -> bool {
        if ends.first() != Some(&0) || ends.last() != Some(&(bytes.len() as u64)) {
            return false;
        }
        ends.windows(2).all(|w| {
            let Some(key) = usize::try_from(w[0])
                .ok()
                .zip(usize::try_from(w[1]).ok())
                .and_then(|(start, end)| bytes.get(start..end))
            else {
                return false;
            };
            if is_compact(key) {
                // The text is written from the record when read: it must be written.
                let style = [key[COMPACT_HEADER], key[COMPACT_HEADER + 1]];
                return crate::wkt::write(&key[COMPACT_HEADER + 2..], style, &mut String::new())
                    .is_some();
            }
            let Some((&kind, rest)) = key.split_first() else {
                return false;
            };
            if kind == Kind::Iri as u8 && rest.first() == Some(&0) {
                return ns
                    .parts(key)
                    .is_some_and(|(_, rest)| std::str::from_utf8(rest).is_ok());
            }
            std::str::from_utf8(rest).is_ok()
                && match kind {
                    k if k == Kind::Iri as u8 || k == Kind::Blank as u8 => true,
                    k if k == Kind::String as u8 => true,
                    // Quoted triples are decoded by parsing, so they must parse.
                    k if k == Kind::Triple as u8 => {
                        std::str::from_utf8(rest).is_ok_and(|t| t.parse::<Triple>().is_ok())
                    }
                    _ => rest.contains(&0),
                }
        })
    }

    /// Rebuilds a dictionary from saved pieces of consecutive keys, each
    /// [`Dictionary::valid_parts`].
    pub(crate) fn from_pieces(saved: Vec<SavedPiece>) -> Self {
        Self::from_saved(saved, Namespaces::default())
    }

    /// [`Dictionary::from_pieces`], with the namespaces its compact IRI keys refer to.
    pub(crate) fn from_saved(saved: Vec<SavedPiece>, ns: Namespaces) -> Self {
        let mut dict = Self {
            pieces: vec![Piece {
                first: 1,
                bytes: Buf::default(),
                ends: vec![0].into(),
                index: None,
                spilled: false,
            }],
            table: HashTable::new(),
            hasher: Hasher::default(),
            compact: false,
            ns,
        };
        if !saved.is_empty() {
            dict.replace_saved(saved);
        }
        dict.table = build_table(&dict.hasher, &dict.ns, &dict.pieces, 0);
        dict
    }
}

/// A saved dictionary piece: its key bytes, key ends and saved index, if any.
pub(crate) type SavedPiece = (Buf<u8>, Buf<u64>, Option<SavedIndex>);

/// The ordinal with hash `h` that `eq` accepts: in `table` (the keys added since the last
/// checkpoint, and those of pieces without an index), then in the pieces' saved indexes.
fn find(
    table: &HashTable<u32>,
    pieces: &[Piece],
    h: u64,
    mut eq: impl FnMut(u32) -> bool,
) -> Option<u32> {
    if let Some(&o) = table.find(h, |&o| eq(o)) {
        return Some(o);
    }
    // Oldest first: the first piece, holding the keys of the last full checkpoint, is the
    // largest.
    pieces
        .iter()
        .find_map(|p| p.index.as_ref()?.find(h, p.ordinals(), &mut eq))
}

/// A table of the ordinals of the keys in pieces without a saved index, with room for
/// `capacity` keys. The keys are hashed in parallel and in ordinal order, so their bytes are
/// read front to back.
fn build_table(
    hasher: &Hasher,
    ns: &Namespaces,
    pieces: &[Piece],
    capacity: usize,
) -> HashTable<u32> {
    use rayon::prelude::*;
    const CHUNK: usize = 1 << 20;
    let unindexed = pieces.iter().filter(|p| p.index.is_none());
    let len: usize = unindexed.clone().map(Piece::len).sum();
    let mut table = HashTable::with_capacity(capacity.max(len + 1));
    let mut hashes = Vec::with_capacity(CHUNK.min(len));
    for p in unindexed {
        // Read in order: with readahead on keys a load spilled (see
        // `Dictionary::reading_in_order`).
        if p.spilled {
            p.bytes.advise_in_order(true);
            p.ends.advise_in_order(true);
        }
        let first = p.first as usize;
        for start in (first..first + p.len()).step_by(CHUNK) {
            let end = (start + CHUNK).min(first + p.len());
            (start..end)
                .into_par_iter()
                .map(|o| hash_full(hasher, ns, p.key(o as u32)))
                .collect_into_vec(&mut hashes);
            for (o, &h) in (start..end).zip(&hashes) {
                // There is room: the table never grows here.
                table.insert_unique(h, o as u32, |_| unreachable!("the table has room"));
            }
        }
        if p.spilled {
            p.bytes.advise_in_order(false);
            p.ends.advise_in_order(false);
        }
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_by_kind() {
        let mut dict = Dictionary::new();
        let mut key = Vec::new();
        let terms: Vec<Term> = vec![
            NamedNode::new_unchecked("http://e/a").into(),
            NamedNode::new_unchecked("http://e/ns#b").into(),
            Literal::new_simple_literal("x").into(),
            Literal::new_language_tagged_literal_unchecked("y", "en").into(),
            Literal::new_typed_literal("z", NamedNode::new_unchecked("http://e/dt")).into(),
            Literal::new_typed_literal("POINT(1 2)", WKT_LITERAL).into(),
        ];
        for t in &terms {
            if let Encoded::Key(_) = encode(t.as_ref(), &mut key) {
                dict.get_or_insert(&key);
            }
        }
        let point: Term = Literal::new_typed_literal("POINT(3 4)", WKT_LITERAL).into();
        let g = tripletrail_geo::parse_wkt_literal_uncached(&point).expect("valid WKT");
        let mut record = Vec::new();
        assert!(crate::geobin::encode(&g, &mut record));
        encode(point.as_ref(), &mut key);
        let mut compact_key = Vec::new();
        assert!(compact(&key, &record, &mut compact_key));
        dict.get_or_insert(&compact_key);
        let usage = copy(&dict).usage();
        let kinds: Vec<_> = usage.iter().map(|u| (u.kind, u.keys)).collect();
        assert_eq!(
            kinds,
            [
                ("iri", 2),
                ("string", 1),
                ("langString", 1),
                ("typed", 1),
                ("wkt", 1),
                ("wktBinary", 1)
            ]
        );
        // The first IRI in full ("http://e/", its kind byte), the second referring to it.
        assert_eq!((usage[0].bytes, usage[0].namespace_bytes), (18, 9));
        assert_eq!(usage[2].prefix_bytes, 4); // the kind byte, "en" and its 0
        assert_eq!(usage[3].prefix_bytes, 13); // the kind byte, "http://e/dt" and its 0
        assert_eq!(usage[4].prefix_bytes, wkt_prefix().len() as u64);
        assert_eq!(usage.iter().map(|u| u.bytes).sum::<u64>(), {
            let mut n = 0;
            dict.for_each_run(0, dict.len(), |_, e| n += e[e.len() - 1] - e[0]);
            n
        });
    }

    /// A dictionary from the parts of `dict`, as one piece.
    fn copy(dict: &Dictionary) -> Dictionary {
        let (mut bytes, mut ends) = (Vec::new(), vec![0]);
        dict.for_each_run(0, dict.len(), |b, e| {
            for w in e.windows(2) {
                bytes.extend_from_slice(&b[w[0] as usize..w[1] as usize]);
                ends.push(bytes.len() as u64);
            }
        });
        assert!(Dictionary::valid_parts(&bytes, &ends, &dict.ns));
        Dictionary::from_saved(vec![(bytes.into(), ends.into(), None)], dict.ns.clone())
    }

    #[test]
    fn saved_indexes_find_every_key() {
        let mut dict = Dictionary::new();
        let key = |i: u32| format!("\u{1}http://example.org/{i}").into_bytes();
        for i in 0..50_000 {
            dict.get_or_insert(&key(i));
        }
        // Two pieces, each with its saved index.
        let piece = |from: usize, keys: usize| {
            let (mut bytes, mut ends) = (Vec::new(), vec![0]);
            for o in from + 1..=from + keys {
                bytes.extend_from_slice(dict.raw_key(o as u32));
                ends.push(bytes.len() as u64);
            }
            let mut slots = vec![0; index_slots(keys)];
            dict.fill_index(from, keys, &mut slots);
            (bytes.into(), ends.into(), Some(SavedIndex(slots.into())))
        };
        let mut saved = Dictionary::from_saved(
            vec![piece(0, 30_000), piece(30_000, 20_000)],
            dict.ns.clone(),
        );
        assert_eq!(saved.table.len(), 0, "no key is hashed again");
        for i in 0..50_000 {
            assert_eq!(saved.get(&key(i)), Some(i + 1));
        }
        assert_eq!(saved.get(&key(50_000)), None);
        let k = key(123);
        let (prefix, value) = split_key(&k);
        assert_eq!(saved.get_split(prefix, value), Some(124));
        // New keys go to the table; keys already saved are found, not added again.
        assert_eq!(saved.get_or_insert(&key(7)), (8, false));
        assert_eq!(saved.get_or_insert(&key(50_000)), (50_001, true));
        assert_eq!(saved.get(&key(50_000)), Some(50_001));
        assert_eq!(saved.table.len(), 1);
        // A damaged index cannot point at keys outside its piece, nor loop.
        let mut bad = piece(0, 100);
        bad.2 = Some(SavedIndex(vec![u64::MAX; index_slots(100)].into()));
        let bad = Dictionary::from_saved(vec![bad], dict.ns.clone());
        assert_eq!(bad.get(&key(5)), None);
    }

    #[test]
    fn grows_and_finds_every_key() {
        let mut dict = Dictionary::new();
        let key = |i: u32| format!("\u{1}http://example.org/{i}").into_bytes();
        for i in 0..100_000 {
            assert_eq!(dict.get_or_insert(&key(i)), (i + 1, true));
        }
        for i in (0..100_000).step_by(7) {
            assert_eq!(dict.get_or_insert(&key(i)), (i + 1, false));
            assert_eq!(dict.get(&key(i)), Some(i + 1));
        }
        assert_eq!(dict.get(&key(100_000)), None);
        let loaded = copy(&dict);
        assert_eq!(loaded.len(), 100_000);
        assert_eq!(loaded.get(&key(99_999)), Some(100_000));
    }

    #[test]
    fn canonical_integers() {
        for v in ["0", "1", "-1", "120", "-987"] {
            assert!(canonical_integer(v), "{v}");
        }
        for v in ["", "-", "+1", "01", "-0", "00", "1.0", " 1"] {
            assert!(!canonical_integer(v), "{v}");
        }
    }

    #[test]
    fn iris_keep_their_namespace_once() {
        let iri = |s: &str| {
            let mut key = Vec::new();
            encode(NamedNode::new_unchecked(s).as_ref().into(), &mut key);
            key
        };
        let iris = [
            "http://example.org/node/1",
            "http://example.org/node/2",
            "http://example.org/node/3",
            "http://example.org/way#4",
            "http://example.org/way#5",
            "http://example.org/alone/6",
            "urn:x:7",
            "x:y/8",
            "x:y/9",
        ];
        let mut dict = Dictionary::new();
        for (i, s) in iris.iter().enumerate() {
            assert_eq!(dict.get_or_insert(&iri(s)), (i as u32 + 1, true), "{s}");
        }
        // The first IRI of a namespace is written in full, the later ones refer to it, to the
        // longest namespace kept; namespaces seen once, and short ones, are not kept.
        let compact: Vec<bool> = (1..=iris.len() as u32)
            .map(|o| dict.ns.parts(dict.raw_key(o)).is_some())
            .collect();
        assert_eq!(
            compact,
            [false, true, true, true, true, true, false, false, false]
        );
        assert_eq!(dict.ns.len(), 2);
        assert_eq!(dict.raw_key(2), b"\x01\0\x002");
        let check = |dict: &Dictionary| {
            for (i, s) in iris.iter().enumerate() {
                let o = i as u32 + 1;
                assert_eq!(dict.get(&iri(s)), Some(o), "{s}");
                let k = iri(s);
                let (prefix, value) = split_key(&k);
                assert_eq!(dict.get_split(prefix, value), Some(o));
                assert_eq!(dict.term(o), Term::from(NamedNode::new_unchecked(*s)));
                assert_eq!(&*dict.key(o), k.as_slice());
                let text = dict.iri_text(dict.raw_key(o)).unwrap().to_vec();
                assert_eq!(text, s.as_bytes());
            }
            // The namespace itself, a prefix or an extension of an IRI is not one of them.
            for s in [
                "http://example.org/node/",
                "http://example.org/node/22",
                "http://example.org/node/",
            ] {
                assert_eq!(dict.get(&iri(s)), None, "{s}");
            }
        };
        check(&dict);
        // Again, from saved parts with the namespaces: the table is built from full keys.
        check(&copy(&dict));
        // Loaded again: the same ordinals, nothing added.
        let mut again = copy(&dict);
        for (i, s) in iris.iter().enumerate() {
            assert_eq!(again.get_or_insert(&iri(s)), (i as u32 + 1, false));
        }
        // A compact key of an unknown namespace is damaged.
        let (bytes, ends) = (b"\x01\0\x05x".to_vec(), vec![0, 4]);
        assert!(!Dictionary::valid_parts(&bytes, &ends, &dict.ns));
        let usage = dict.usage();
        assert_eq!(usage[0].keys, iris.len() as u64);
        // Only the IRIs written in full count their namespaces.
        let full: u64 = [24, 0, 4, 4].iter().sum();
        assert_eq!(usage[0].namespace_bytes, full);
        // Saved and read back.
        let (bytes, ends) = dict.ns.saved();
        let back = Namespaces::from_saved(bytes.to_vec(), ends.to_vec()).unwrap();
        assert_eq!(back.find(b"http://example.org/"), Some(1));
        assert_eq!(back.find(b"http://example.org/way#"), None);
        assert!(Namespaces::from_saved(b"ab".to_vec(), vec![0, 3]).is_none());
        assert!(Namespaces::from_saved(vec![0xff], vec![0, 1]).is_none());
    }

    #[test]
    fn two_part_texts_order_as_written_out() {
        let texts: [(&[u8], &[u8]); 8] = [
            (b"a/", b"z"),
            (b"a/b/", b"c"),
            (b"", b"a/b/c"),
            (b"a/b/c", b""),
            (b"a/b/", b""),
            (b"a/", b"b/cd"),
            (b"b", b""),
            (b"", b""),
        ];
        for a in texts {
            for b in texts {
                let (ta, tb) = (Text(a.0, a.1), Text(b.0, b.1));
                let (wa, wb) = (ta.to_vec(), tb.to_vec());
                assert_eq!(ta.cmp(&tb), wa.cmp(&wb), "{wa:?} {wb:?}");
                assert_eq!(ta.cmp_bytes(&wb), wa.cmp(&wb));
                assert_eq!(ta.starts_with(&wb), wa.starts_with(&wb), "{wa:?} {wb:?}");
            }
        }
    }

    #[test]
    fn terms_round_trip() {
        let terms: Vec<Term> = vec![
            NamedNode::new_unchecked("http://example.com/a").into(),
            BlankNode::new_unchecked("b0").into(),
            Literal::new_simple_literal("text").into(),
            Literal::new_language_tagged_literal_unchecked("tekst", "nb").into(),
            Literal::new_typed_literal("1.5e0", xsd::DOUBLE).into(),
            Literal::new_typed_literal("+1.5", xsd::DECIMAL).into(),
            Literal::new_typed_literal("2020-01-01T00:00:00+05:07", xsd::DATE_TIME).into(),
            Literal::new_typed_literal("POINT(1 2)", WKT_LITERAL).into(),
            Literal::new_typed_literal("+7", xsd::INTEGER).into(),
        ];
        let mut dict = Dictionary::new();
        let mut key = Vec::new();
        let mut ordinals = Vec::new();
        for t in &terms {
            let Encoded::Key(_) = encode(t.as_ref(), &mut key) else {
                panic!("{t} is not inline");
            };
            ordinals.push(dict.get_or_insert(&key).0);
        }
        for (t, &o) in terms.iter().zip(&ordinals) {
            assert_eq!(&decode(&dict.key(o)), t);
            encode(t.as_ref(), &mut key);
            assert_eq!(dict.get(&key), Some(o));
        }
        let copy = copy(&dict);
        for (t, &o) in terms.iter().zip(&ordinals) {
            encode(t.as_ref(), &mut key);
            assert_eq!(copy.get(&key), Some(o));
        }
    }

    #[test]
    fn integers_and_booleans_are_inline() {
        let mut key = Vec::new();
        for (t, expected) in [
            (Literal::new_typed_literal("42", xsd::INTEGER), Some(42)),
            (Literal::new_typed_literal("-3", xsd::INTEGER), Some(-3)),
            (Literal::new_typed_literal("042", xsd::INTEGER), None),
        ] {
            let term = Term::from(t);
            match encode(term.as_ref(), &mut key) {
                Encoded::Inline(id) => {
                    assert_eq!(Some(id.as_integer()), expected);
                    assert_eq!(inline_term(id), Some(term));
                }
                Encoded::Key(_) => assert_eq!(expected, None),
            }
        }
        let t = Term::from(Literal::from(true));
        let Encoded::Inline(id) = encode(t.as_ref(), &mut key) else {
            panic!()
        };
        assert_eq!(inline_term(id), Some(t));
        for (value, datatype, kind) in [
            ("0.340", xsd::DOUBLE, Kind::Double),
            ("-12.5", xsd::DECIMAL, Kind::Decimal),
            ("7", xsd::DOUBLE, Kind::Double),
            ("2024-05-01", xsd::DATE, Kind::DateTime),
            (
                "2024-05-01T10:00:00.25+02:00",
                xsd::DATE_TIME,
                Kind::DateTime,
            ),
        ] {
            let term = Term::from(Literal::new_typed_literal(value, datatype));
            let Encoded::Inline(id) = encode(term.as_ref(), &mut key) else {
                panic!("{term} is inline")
            };
            assert_eq!(id.kind(), kind);
            assert_eq!(inline_term(id), Some(term));
        }
    }
}
