//! The vocabulary every layer shares.
//!
//! These are the *semantic* types — what a record IS, independent of how any layer stores it.
//! Storage-layer types (a piece's location in the fold, a part's dictionary indices) belong to
//! their own modules; they are encodings, not vocabulary.

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

/// One step of a record body, as produced by the lens and stored in the WAL.
///
/// The body is a FLAT program — no node graph, no prev-chains. Reconstruction is a single forward
/// pass that concatenates each op's bytes. Cross-record sharing comes from the fold (identical
/// pieces are stored once), not from sharing program structure.
#[derive(Clone, Debug, PartialEq)]
pub enum BodyOp {
    /// Bytes stored inline in the record itself — the connective tissue between pieces (JSON
    /// punctuation, short fields) that is too small to be worth folding.
    Lit(Vec<u8>),
    /// A reference to folded content, by identity.
    Piece { hash: PieceHash, len: u32 },
}

impl BodyOp {
    /// Reconstructed length of this op, without resolving anything.
    pub fn len(&self) -> u64 {
        match self {
            BodyOp::Lit(b) => b.len() as u64,
            BodyOp::Piece { len, .. } => *len as u64,
        }
    }
}

/// A record as the store takes it in and as the WAL durably records it: fully carved, so replay is
/// exact and independent of the carve logic's version. (Storing raw input instead would make replay
/// depend on the lens that happened to be compiled in — the bug class this design refuses.)
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub id: String,
    pub body: Vec<BodyOp>,
    /// Exact attribute sequence — order preserved, duplicate keys preserved. Round-tripping this
    /// faithfully is part of byte-exact reconstruction.
    pub attrs: Vec<(String, AttrValue)>,
}

impl Record {
    /// The reconstructed body length, without touching the fold.
    pub fn body_len(&self) -> u64 {
        self.body.iter().map(|o| o.len()).sum()
    }
}
