//! Feature-independent discovery of the store's self-described field universe.
//!
//! The result preserves TurnDB's namespaces and type distinctions instead of flattening them into
//! Arrow display names. It reads only part metadata plus the writer memtable; no attribute values,
//! record layouts, or content programs are decoded.

use crate::part::{attrs, Part};
use crate::types::{AttrValue, Record};
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AttrType {
    String,
    Int,
    Float,
    Bool,
    UInt,
    Binary,
    TimestampNs,
    Null,
}

impl AttrType {
    fn from_tag(tag: u8) -> Result<AttrType> {
        match tag {
            0 => Ok(AttrType::String),
            1 => Ok(AttrType::Int),
            2 => Ok(AttrType::Float),
            3 => Ok(AttrType::Bool),
            4 => Ok(AttrType::UInt),
            5 => Ok(AttrType::Binary),
            6 => Ok(AttrType::TimestampNs),
            7 => Ok(AttrType::Null),
            other => bail!("unknown attribute type tag {other} in schema metadata"),
        }
    }

    fn of(value: &AttrValue) -> AttrType {
        match value {
            AttrValue::Str(_) => AttrType::String,
            AttrValue::Int(_) => AttrType::Int,
            AttrValue::Float(_) => AttrType::Float,
            AttrValue::Bool(_) => AttrType::Bool,
            AttrValue::UInt(_) => AttrType::UInt,
            AttrValue::Bytes(_) => AttrType::Binary,
            AttrValue::TimestampNs(_) => AttrType::TimestampNs,
            AttrValue::Null => AttrType::Null,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttributeSchema {
    pub name: String,
    /// One name may deliberately carry several scalar types. They remain distinct columns.
    pub types: Vec<AttrType>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Schema {
    pub attributes: Vec<AttributeSchema>,
    pub contents: Vec<String>,
    /// True when immutable parts contributed metadata. Such metadata is a conservative superset:
    /// a field may occur only in a physical version hidden by a newer record or tombstone. Discovery
    /// never claims per-field liveness without paying for a visibility/value walk.
    pub may_include_shadowed_fields: bool,
}

#[derive(Default)]
pub(crate) struct Builder {
    attrs: BTreeMap<String, BTreeSet<AttrType>>,
    contents: BTreeSet<String>,
    part_metadata: bool,
}

impl Builder {
    pub(crate) fn add_parts(&mut self, parts: &[Arc<Part>]) -> Result<()> {
        self.part_metadata |= !parts.is_empty();
        for part in parts {
            if part.has_columns() {
                for (name, tag, _, _) in attrs::read_meta(part)? {
                    self.attrs.entry(name).or_default().insert(AttrType::from_tag(tag)?);
                }
            }
            for content in part.content_meta()?.iter() {
                self.contents.insert(content.name.clone());
            }
        }
        Ok(())
    }

    pub(crate) fn add_record(&mut self, record: &Record) {
        for (name, value) in &record.attrs {
            self.attrs.entry(name.clone()).or_default().insert(AttrType::of(value));
        }
        self.contents.extend(record.contents.iter().map(|content| content.name.clone()));
    }

    pub(crate) fn finish(self) -> Schema {
        Schema {
            attributes: self
                .attrs
                .into_iter()
                .map(|(name, types)| AttributeSchema { name, types: types.into_iter().collect() })
                .collect(),
            contents: self.contents.into_iter().collect(),
            may_include_shadowed_fields: self.part_metadata,
        }
    }
}
