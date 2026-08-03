//! The vocabulary every layer shares.
//!
//! These are the *semantic* types — what a record IS, independent of how any layer stores it.
//! Storage-layer types (a piece's location in the fold, a part's dictionary indices) belong to
//! their own modules; they are encodings, not vocabulary.

use std::collections::BTreeSet;
use std::fmt;

/// A piece's content identity: BLAKE3 of its exact bytes. Identical bytes anywhere in the store —
/// across records, sessions, corpora — resolve to this same value and are stored once.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PieceHash(pub [u8; 32]);

impl PieceHash {
    pub fn of(bytes: &[u8]) -> PieceHash {
        PieceHash(blake3::hash(bytes).into())
    }
    pub fn to_hex(self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

impl fmt::Debug for PieceHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // first 8 hex chars — enough to correlate in a log, short enough to read
        write!(f, "piece:{:02x}{:02x}{:02x}{:02x}", self.0[0], self.0[1], self.0[2], self.0[3])
    }
}

impl fmt::Display for PieceHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// A named content value's byte identity: BLAKE3 of the complete reconstructed byte sequence.
///
/// This is deliberately distinct from [`PieceHash`]. A value can contain literals and any number
/// of pieces, and its identity must remain the same when carving boundaries change.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentHash(pub [u8; 32]);

impl ContentHash {
    pub fn of(bytes: &[u8]) -> ContentHash {
        ContentHash(blake3::hash(bytes).into())
    }

    pub fn to_hex(self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "content:{:02x}{:02x}{:02x}{:02x}", self.0[0], self.0[1], self.0[2], self.0[3])
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// A typed attribute value — the queryable metadata plane. Deliberately four scalars: these are what
/// you filter and aggregate on. Bulk content is not an attribute; it is folded into pieces.
///
/// Equality compares floats by **bit pattern**, not by IEEE value. In a store whose contract is
/// byte-exact round-trip, two values are the same exactly when they are stored the same — so NaN
/// equals itself and `-0.0` differs from `0.0`. Derived `PartialEq` would say the opposite of both,
/// and would silently mis-merge dictionary entries.
#[derive(Clone, Debug)]
pub enum AttrValue {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl PartialEq for AttrValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (AttrValue::Str(a), AttrValue::Str(b)) => a == b,
            (AttrValue::Int(a), AttrValue::Int(b)) => a == b,
            (AttrValue::Float(a), AttrValue::Float(b)) => a.to_bits() == b.to_bits(),
            (AttrValue::Bool(a), AttrValue::Bool(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for AttrValue {}

impl AttrValue {
    /// The column type tag. One logical column per (key, tag), so a key carrying mixed types across
    /// records yields several homogeneous columns rather than one column that can mis-decode.
    pub fn type_tag(&self) -> u8 {
        match self {
            AttrValue::Str(_) => 0,
            AttrValue::Int(_) => 1,
            AttrValue::Float(_) => 2,
            AttrValue::Bool(_) => 3,
        }
    }
}

/// The conventional name used by the compatibility body API.
pub const BODY_CONTENT: &str = "body";

/// One step of a content value, as produced by the lens and stored in the WAL.
///
/// Content is a FLAT program — no node graph, no prev-chains. Reconstruction is a single forward
/// pass that concatenates each op's bytes. Cross-record sharing comes from the fold (identical
/// pieces are stored once), not from sharing program structure.
#[derive(Clone, Debug, PartialEq)]
pub enum ContentOp {
    /// Bytes stored inline in the record itself — the connective tissue between pieces (JSON
    /// punctuation, short fields) that is too small to be worth folding.
    Lit(Vec<u8>),
    /// A reference to folded content, by identity.
    Piece { hash: PieceHash, len: u32 },
}

impl ContentOp {
    /// Reconstructed length of this op, without resolving anything.
    pub fn len(&self) -> u64 {
        match self {
            ContentOp::Lit(b) => b.len() as u64,
            ContentOp::Piece { len, .. } => *len as u64,
        }
    }

    /// Whether this op contributes no bytes. A zero-length piece is legal — an empty message part
    /// carves to one — so this is a real question, not a lint appeasement.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Pre-1.0 source compatibility for callers that construct body programs directly. New code should
/// use [`ContentOp`]; a body is now merely content named [`BODY_CONTENT`].
pub type BodyOp = ContentOp;

/// One independently projectable, content-addressed value in a record.
///
/// Names are map keys: they are non-empty and unique within a record. The storage layer canonicalises
/// them into UTF-8 byte order, so their input order carries no meaning. `ops` remains ordered because
/// concatenating it is what reconstructs the value byte-exactly.
#[derive(Clone, Debug, PartialEq)]
pub struct Content {
    pub name: String,
    pub ops: Vec<ContentOp>,
    /// Exact reconstructed-byte identity when the ingest or on-disk format carried it. Legacy
    /// records leave this unavailable rather than substituting a program or piece hash.
    pub identity: Option<ContentHash>,
}

impl Content {
    pub fn new(name: impl Into<String>, ops: Vec<ContentOp>) -> Content {
        Content { name: name.into(), ops, identity: None }
    }

    pub fn identified(
        name: impl Into<String>,
        ops: Vec<ContentOp>,
        identity: ContentHash,
    ) -> Content {
        Content { name: name.into(), ops, identity: Some(identity) }
    }

    /// Reconstructed length without touching the fold.
    pub fn len(&self) -> u64 {
        self.ops.iter().map(ContentOp::len).sum()
    }

    /// An empty value is still PRESENT. Absence is represented by no [`Content`] carrying the name.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A record as the store takes it in and as the WAL durably records it: fully carved, so replay is
/// exact and independent of the carve logic's version. (Storing raw input instead would make replay
/// depend on the lens that happened to be compiled in — the bug class this design refuses.)
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub id: String,
    /// Named content values. Names are unique; readers return them in UTF-8 byte order.
    pub contents: Vec<Content>,
    /// Exact attribute sequence — order preserved, duplicate keys preserved. Round-tripping this
    /// faithfully is part of byte-exact reconstruction.
    pub attrs: Vec<(String, AttrValue)>,
}

impl Record {
    /// Construct a record and canonicalise its content-column order.
    pub fn new(
        id: impl Into<String>,
        mut contents: Vec<Content>,
        attrs: Vec<(String, AttrValue)>,
    ) -> anyhow::Result<Record> {
        let id = id.into();
        if id.is_empty() {
            anyhow::bail!("record id must not be empty");
        }
        validate_contents(&contents)?;
        contents.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        Ok(Record { id, contents, attrs })
    }

    /// A named content value, if present.
    pub fn content(&self, name: &str) -> Option<&Content> {
        self.contents.iter().find(|c| c.name == name)
    }

    /// The compatibility body program, if content named `body` is present.
    pub fn body(&self) -> Option<&[ContentOp]> {
        self.content(BODY_CONTENT).map(|c| c.ops.as_slice())
    }

    /// The reconstructed length of a named content value, without touching the fold.
    pub fn content_len(&self, name: &str) -> Option<u64> {
        self.content(name).map(Content::len)
    }

    /// Compatibility form of [`Record::content_len`]. An absent body reports zero, matching the
    /// historical empty-program representation used by tombstone-shaped test records.
    pub fn body_len(&self) -> u64 {
        self.content_len(BODY_CONTENT).unwrap_or(0)
    }
}

/// Enforce the semantic content-map contract without changing its order.
pub fn validate_contents(contents: &[Content]) -> anyhow::Result<()> {
    let mut names = BTreeSet::new();
    for content in contents {
        if content.name.is_empty() {
            anyhow::bail!("content name must not be empty");
        }
        if !names.insert(content.name.as_str()) {
            anyhow::bail!("duplicate content name {:?}", content.name);
        }
    }
    Ok(())
}
