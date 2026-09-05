//! Sparse named content columns for the current draft part format.
//!
//! A content name is a physical column. Its programs and offsets are independent sections, so
//! projecting one named value never decompresses another named value's programs. Every program still
//! addresses the part-wide piece dictionary; columnar placement does not weaken content identity.

use super::idcol::put_varint;
use super::{OP_LIT, OP_PIECE};
use crate::types::{BodyOp, Content, PieceHash, Record};
use anyhow::{bail, Result};
use std::collections::{BTreeMap, HashMap};

pub(crate) const RID_DENSE: u8 = 0;
pub(crate) const RID_DELTA: u8 = 1;

pub(crate) struct BuiltColumn {
    pub name: String,
    pub occurrences: usize,
    pub dense: bool,
    pub prog: Vec<u8>,
    pub offsets: Vec<u64>,
    pub rid: Vec<u8>,
    pub identities: Vec<u8>,
}

pub(crate) struct Built {
    pub meta: Vec<u8>,
    pub cols: Vec<BuiltColumn>,
}

pub(crate) fn encode_program(
    out: &mut Vec<u8>,
    ops: &[BodyOp],
    dict_index: &HashMap<PieceHash, u32>,
) -> Result<()> {
    let emitted = ops.iter().filter(|op| !matches!(op, BodyOp::Lit(b) if b.is_empty())).count();
    put_varint(out, emitted as u64);
    for op in ops {
        match op {
            BodyOp::Lit(b) => {
                if b.is_empty() {
                    continue;
                }
                put_varint(out, (b.len() as u64) << 1 | OP_LIT);
                out.extend_from_slice(b);
            }
            BodyOp::Piece { hash, len } => {
                let idx = dict_index.get(hash).ok_or_else(|| {
                    anyhow::anyhow!("piece {hash} is outside the part's declared dictionary")
                })?;
                put_varint(out, (*idx as u64) << 1 | OP_PIECE);
                put_varint(out, *len as u64);
            }
        }
    }
    Ok(())
}

/// Build named content columns from records already arranged in part row order.
pub(crate) fn build(ordered: &[&Record], dict_index: &HashMap<PieceHash, u32>) -> Result<Built> {
    let mut universe: BTreeMap<String, Vec<(usize, &Content)>> = BTreeMap::new();
    for (row, record) in ordered.iter().enumerate() {
        crate::types::validate_contents(&record.contents)?;
        for content in &record.contents {
            universe.entry(content.name.clone()).or_default().push((row, content));
        }
    }

    let mut cols = Vec::with_capacity(universe.len());
    for (name, values) in universe {
        let dense = values.len() == ordered.len()
            && values.iter().enumerate().all(|(expected, (row, _))| expected == *row);
        let mut prog = Vec::new();
        let mut offsets = Vec::with_capacity(values.len() + 1);
        let mut rid = Vec::new();
        let mut identities = Vec::with_capacity(values.len() * 32);
        let mut previous = 0usize;
        for (occurrence, (row, content)) in values.into_iter().enumerate() {
            offsets.push(prog.len() as u64);
            encode_program(&mut prog, &content.ops, dict_index)?;
            encode_identity(&mut identities, content)?;
            if !dense {
                let delta = if occurrence == 0 { row } else { row - previous };
                if occurrence > 0 && delta == 0 {
                    bail!("content {name:?} occurs more than once on row {row}");
                }
                put_varint(&mut rid, delta as u64);
                previous = row;
            }
        }
        offsets.push(prog.len() as u64);
        cols.push(BuiltColumn {
            name,
            occurrences: offsets.len() - 1,
            dense,
            prog,
            offsets,
            rid,
            identities,
        });
    }

    let mut meta = Vec::new();
    put_varint(&mut meta, cols.len() as u64);
    for col in &cols {
        put_varint(&mut meta, col.name.len() as u64);
        meta.extend_from_slice(col.name.as_bytes());
        put_varint(&mut meta, col.occurrences as u64);
        meta.push(if col.dense { RID_DENSE } else { RID_DELTA });
    }
    Ok(Built { meta, cols })
}

pub(crate) fn encode_identity(out: &mut Vec<u8>, content: &Content) -> Result<()> {
    let identity = content.identity.ok_or_else(|| {
        anyhow::anyhow!("content {:?} has no reconstructed-byte identity", content.name)
    })?;
    out.extend_from_slice(&identity.0);
    Ok(())
}
