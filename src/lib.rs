//! turndb — a content-addressed columnar store for AI traces.
//!
//! # The shape
//!
//! ```text
//! ingest ── WAL (durability) ── memtable
//!                                  │ flush
//!                                  ▼
//!                          part (immutable, columnar)
//!                                  │ merge
//!                                  ▼
//!                          larger parts
//!
//!        fold ── append-only content, shared by every part, written once, never rewritten
//! ```
//!
//! Two ideas fused. **Content addressing**: a piece of content is identified by BLAKE3 of its bytes
//! and stored exactly once, however many records reference it. **Columnar**: the typed metadata
//! around that content lives in per-field columns that compress and scan well.
//!
//! # Why the fusion is worth it
//!
//! In a conventional LSM, compaction rewrites data — merge two 1 GiB parts, write 2 GiB. That write
//! amplification is the tax every tiering strategy exists to manage.
//!
//! Here, **compaction never touches content.** Pieces live in the fold, addressed by hash, written
//! once. A part is a bundle of *references plus columns*, so merging rewrites references and columns
//! and nothing else — on trace-shaped data that is a small fraction of the bytes. Content addressing
//! is what decouples compaction cost from data volume, and that is what lets a trace store behave
//! like a database instead of a write-once archive.
//!
//! # The cardinal invariant
//!
//! **Byte-exact reconstruction.** Reading a record reproduces the original bytes exactly — including
//! attribute order and duplicate keys. Every layer below is in service of it, and no change ships
//! that breaks it.

pub mod fold;
pub mod part;
pub mod store;
pub mod types;

pub use types::{AttrValue, BodyOp, PieceHash, Record};
