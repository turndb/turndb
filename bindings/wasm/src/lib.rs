//! The WASM ABI — the only place turndb speaks in pointers and status codes.
//!
//! Kept in its own crate so the engine never carries a binding. Everything here is mechanical:
//! move bytes across the boundary, call the Rust API, move bytes back.
//!
//! # The calling convention
//!
//! WASM passes numbers, so every string and buffer crosses as a `(ptr, len)` pair into linear
//! memory the host allocated with [`tdb_alloc`]. Results come back the other way: a call writes
//! into a per-instance output buffer and returns its length, and the host reads it at
//! [`tdb_out_ptr`] before the next call overwrites it.
//!
//! Every fallible call returns `i32`: `>= 0` succeeded (and for reads, `0` means "absent" while
//! `1` means "in the output buffer"), and `-1` failed with a message at [`tdb_err_ptr`]. **The
//! error string is never discarded.** A storage engine that reports "something went wrong" is
//! useless at 3am, and the whole point of the engine's error discipline is lost if the binding
//! flattens it — so `anyhow`'s full context chain (`{:#}`) crosses the boundary intact.
//!
//! # Attributes cross as a tagged array, not an object
//!
//! turndb preserves attribute ORDER and DUPLICATE KEYS, because byte-exact reconstruction depends
//! on it. A JSON object can represent neither. So attributes cross as
//! `[[key, tag, value], ...]` with scalar tags `s`/`i`/`f`/`b`/`u`/`x`/`t`/`n` — ordered,
//! duplicate-tolerant, and
//! explicit about int-vs-float, which a bare JSON number cannot be. The JS wrapper builds this from
//! the friendlier shapes a caller actually wants to write.
//!
//! # Panics
//!
//! The crate is built with `panic = "abort"`, so a panic takes the instance down rather than
//! unwinding across the ABI (which is undefined). Every entry point is written to return an error
//! instead of panicking; the abort is the backstop, not the plan.

// A broken doc link is a documentation claim that no longer resolves, and this crate has
// shipped three of them. Denying it makes the build refuse rather than leaving it for a
// sweep — which matters because the one instance caught today was caught by comparing
// `cargo doc` counts, not by anyone remembering the rule.
//
// Reaches only links written with `[`Item`]` syntax: 83 such lines against 302 carrying any
// backticked identifier. The remainder is a separate problem and is not closed by this.
#![deny(rustdoc::broken_intra_doc_links)]

use std::cell::RefCell;
use std::path::Path;
use turndb::fold::FoldCfg;
use turndb::read_limits::ReadLimits;
use turndb::scan::{
    Compare, ContentMode, ContentSelect, Direction, Predicate, ScanPage, ScanRequest,
};
use turndb::store::{Store, StoreOptions, WriteLimits};
use turndb::types::AttrValue;

thread_local! {
    /// Open stores by handle. A slot is `None` once closed, so a stale handle errors rather than
    /// addressing whatever was opened next.
    static STORES: RefCell<Vec<Option<Store>>> = const { RefCell::new(Vec::new()) };
    static OUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static ERR: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

// ── Memory and result plumbing ──────────────────────────────────────────────

/// Allocate `len` bytes for the host to write into. Pair with [`tdb_free`].
#[no_mangle]
pub extern "C" fn tdb_alloc(len: u32) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(len as usize);
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// Release a buffer from [`tdb_alloc`]. `len` must be the length it was allocated with.
///
/// # Safety
/// `ptr` must come from [`tdb_alloc`] with the same `len`, and must not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn tdb_free(ptr: *mut u8, len: u32) {
    if !ptr.is_null() {
        drop(Vec::from_raw_parts(ptr, 0, len as usize));
    }
}

#[no_mangle]
pub extern "C" fn tdb_out_ptr() -> *const u8 {
    OUT.with(|o| o.borrow().as_ptr())
}

#[no_mangle]
pub extern "C" fn tdb_out_len() -> u32 {
    OUT.with(|o| o.borrow().len() as u32)
}

#[no_mangle]
pub extern "C" fn tdb_err_ptr() -> *const u8 {
    ERR.with(|e| e.borrow().as_ptr())
}

#[no_mangle]
pub extern "C" fn tdb_err_len() -> u32 {
    ERR.with(|e| e.borrow().len() as u32)
}

fn set_out(bytes: &[u8]) {
    OUT.with(|o| {
        let mut o = o.borrow_mut();
        o.clear();
        o.extend_from_slice(bytes);
    });
}

/// Record an error and return the failure code. `{:#}` keeps anyhow's whole context chain, which
/// is the difference between "open failed" and "open failed: read MANIFEST: no such file".
fn fail(e: impl std::fmt::Display) -> i32 {
    ERR.with(|slot| {
        let mut slot = slot.borrow_mut();
        slot.clear();
        slot.extend_from_slice(format!("{e:#}").as_bytes());
    });
    -1
}

fn clear_err() {
    ERR.with(|e| e.borrow_mut().clear());
}

/// # Safety
/// `ptr`/`len` must describe initialised memory valid for the call.
unsafe fn slice<'a>(ptr: *const u8, len: u32) -> &'a [u8] {
    if ptr.is_null() || len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(ptr, len as usize)
    }
}

/// # Safety
/// As [`slice`], and the bytes must be UTF-8.
unsafe fn text<'a>(ptr: *const u8, len: u32) -> Result<&'a str, std::str::Utf8Error> {
    std::str::from_utf8(slice(ptr, len))
}

fn with_store<T>(h: i32, f: impl FnOnce(&mut Store) -> Result<T, i32>) -> Result<T, i32> {
    STORES.with(|s| {
        let mut s = s.borrow_mut();
        match usize::try_from(h).ok().and_then(|i| s.get_mut(i)).and_then(|slot| slot.as_mut()) {
            Some(store) => f(store),
            None => Err(fail(format!("store handle {h} is not open"))),
        }
    })
}

// ── Attributes ──────────────────────────────────────────────────────────────

/// Decode `[[key, tag, value], ...]`. Order and duplicate keys survive, because the engine keeps
/// both and reconstruction depends on them.
fn decode_attrs(json: &[u8]) -> Result<Vec<(String, AttrValue)>, String> {
    if json.is_empty() {
        return Ok(Vec::new());
    }
    let v: serde_json::Value =
        serde_json::from_slice(json).map_err(|e| format!("attributes are not valid JSON: {e}"))?;
    let arr = v.as_array().ok_or("attributes must be an array of [key, tag, value]")?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        out.push(decode_attr_triple(item, i)?);
    }
    Ok(out)
}

/// Decode one `[key, tag, value]` triple. The only place a scalar tag is interpreted, so the
/// writer's attributes and a scan predicate's comparison value cannot drift apart in what `"u"`
/// or `"t"` means.
fn decode_attr_triple(item: &serde_json::Value, i: usize) -> Result<(String, AttrValue), String> {
    {
        let t = item.as_array().ok_or_else(|| format!("attribute {i} is not an array"))?;
        if t.len() != 3 {
            return Err(format!("attribute {i} needs exactly [key, tag, value]"));
        }
        let key = t[0].as_str().ok_or_else(|| format!("attribute {i} key is not a string"))?;
        let tag = t[1].as_str().ok_or_else(|| format!("attribute {i} tag is not a string"))?;
        let val = &t[2];
        let av = match tag {
            "s" => AttrValue::Str(
                val.as_str().ok_or_else(|| format!("attribute {key} is not a string"))?.to_string(),
            ),
            "i" => AttrValue::Int(
                match val {
                    serde_json::Value::Number(n) => n.as_i64(),
                    serde_json::Value::String(s) => s.parse::<i64>().ok(),
                    _ => None,
                }
                .ok_or_else(|| format!("attribute {key} is not an i64"))?,
            ),
            "f" => AttrValue::Float(
                match val {
                    serde_json::Value::Number(n) => n.as_f64(),
                    serde_json::Value::String(s) => s.parse::<f64>().ok(),
                    _ => None,
                }
                .ok_or_else(|| format!("attribute {key} is not an f64"))?,
            ),
            "b" => AttrValue::Bool(
                val.as_bool().ok_or_else(|| format!("attribute {key} is not a boolean"))?,
            ),
            "u" => AttrValue::UInt(
                match val {
                    serde_json::Value::Number(n) => n.as_u64(),
                    serde_json::Value::String(s) => s.parse::<u64>().ok(),
                    _ => None,
                }
                .ok_or_else(|| format!("attribute {key} is not a u64"))?,
            ),
            "x" => AttrValue::Bytes(
                val.as_array()
                    .ok_or_else(|| format!("attribute {key} binary value is not a byte array"))?
                    .iter()
                    .map(|byte| {
                        byte.as_u64()
                            .and_then(|byte| u8::try_from(byte).ok())
                            .ok_or_else(|| format!("attribute {key} contains a non-byte value"))
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            "t" => AttrValue::TimestampNs(
                match val {
                    serde_json::Value::Number(n) => n.as_i64(),
                    serde_json::Value::String(s) => s.parse::<i64>().ok(),
                    _ => None,
                }
                .ok_or_else(|| format!("attribute {key} is not a signed nanosecond timestamp"))?,
            ),
            "n" if val.is_null() => AttrValue::Null,
            other => return Err(format!("attribute {key} has unknown tag {other:?}")),
        };
        Ok((key.to_string(), av))
    }
}

fn encode_attrs(attrs: &[(String, AttrValue)]) -> serde_json::Value {
    serde_json::Value::Array(
        attrs
            .iter()
            .map(|(k, v)| {
                let (tag, val) = match v {
                    AttrValue::Str(s) => ("s", serde_json::Value::from(s.clone())),
                    // JSON numbers cross JavaScript as f64 and cannot represent every i64. Decimal
                    // text keeps the portable ABI exact; the JS wrapper returns a BigInt.
                    AttrValue::Int(i) => ("i", serde_json::Value::from(i.to_string())),
                    // A non-finite float has no JSON spelling. Carrying it across as a string
                    // keeps the value visible rather than silently turning it into null.
                    AttrValue::Float(f) => (
                        "f",
                        serde_json::Number::from_f64(*f)
                            .map(serde_json::Value::Number)
                            .unwrap_or_else(|| serde_json::Value::from(f.to_string())),
                    ),
                    AttrValue::Bool(b) => ("b", serde_json::Value::from(*b)),
                    AttrValue::UInt(i) => ("u", serde_json::Value::from(i.to_string())),
                    AttrValue::Bytes(bytes) => (
                        "x",
                        serde_json::Value::Array(
                            bytes.iter().map(|byte| serde_json::Value::from(*byte)).collect(),
                        ),
                    ),
                    AttrValue::TimestampNs(ns) => ("t", serde_json::Value::from(ns.to_string())),
                    AttrValue::Null => ("n", serde_json::Value::Null),
                };
                serde_json::json!([k, tag, val])
            })
            .collect(),
    )
}

// ── Lifecycle ───────────────────────────────────────────────────────────────

/// Machine-readable guarantees of this compiled WASI core.
#[no_mangle]
pub extern "C" fn tdb_capabilities() -> i32 {
    clear_err();
    match serde_json::to_vec(&turndb::capabilities::capabilities()) {
        Ok(v) => {
            set_out(&v);
            0
        }
        Err(e) => fail(format!("encode capability profile: {e}")),
    }
}

/// Open (or create) a store. Returns a handle, or -1.
///
/// Numeric options are 0 for the engine defaults.
///
/// # Safety
/// `dir` must be valid UTF-8 of `dir_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn tdb_open(
    dir: *const u8,
    dir_len: u32,
    block_target: u32,
    level: i32,
    max_record_bytes: u32,
    max_batch_bytes: u32,
    max_batch_records: u32,
    max_identifier_bytes: u32,
) -> i32 {
    // Keep the original ABI for direct embedders. New readers use `tdb_open_v3`; zero selects the
    // same compiled defaults here.
    unsafe {
        tdb_open_v2(
            dir,
            dir_len,
            block_target,
            level,
            max_record_bytes,
            max_batch_bytes,
            max_batch_records,
            max_identifier_bytes,
            0,
            0,
        )
    }
}

/// Open with explicit atomic persisted-frame admission. Numeric options are 0 for defaults.
///
/// # Safety
/// `dir` must be valid UTF-8 of `dir_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn tdb_open_v2(
    dir: *const u8,
    dir_len: u32,
    block_target: u32,
    level: i32,
    max_record_bytes: u32,
    max_batch_bytes: u32,
    max_batch_records: u32,
    max_identifier_bytes: u32,
    max_stored_frame_bytes: u32,
    max_decoded_frame_bytes: u32,
) -> i32 {
    unsafe {
        tdb_open_v3(
            dir,
            dir_len,
            block_target,
            level,
            max_record_bytes,
            max_batch_bytes,
            max_batch_records,
            max_identifier_bytes,
            max_stored_frame_bytes,
            max_decoded_frame_bytes,
            0,
            0,
            0,
        )
    }
}

/// Open with explicit atomic-frame and object-count admission. Numeric options are 0 for defaults.
///
/// # Safety
/// `dir` must be valid UTF-8 of `dir_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn tdb_open_v3(
    dir: *const u8,
    dir_len: u32,
    block_target: u32,
    level: i32,
    max_record_bytes: u32,
    max_batch_bytes: u32,
    max_batch_records: u32,
    max_identifier_bytes: u32,
    max_stored_frame_bytes: u32,
    max_decoded_frame_bytes: u32,
    max_directory_entries: u32,
    max_wal_frames: u32,
    max_fold_blocks: u32,
) -> i32 {
    clear_err();
    let dir = match text(dir, dir_len) {
        Ok(d) => d,
        Err(e) => return fail(format!("store path is not UTF-8: {e}")),
    };
    let mut cfg = FoldCfg::default();
    if block_target > 0 {
        cfg.block_target = block_target as usize;
    }
    if level != 0 {
        cfg.level = level;
    }
    let defaults = WriteLimits::default();
    let limits = WriteLimits {
        max_record_bytes: if max_record_bytes == 0 {
            defaults.max_record_bytes
        } else {
            u64::from(max_record_bytes)
        },
        max_batch_bytes: if max_batch_bytes == 0 {
            defaults.max_batch_bytes
        } else {
            u64::from(max_batch_bytes)
        },
        max_batch_records: if max_batch_records == 0 {
            defaults.max_batch_records
        } else {
            max_batch_records as usize
        },
        max_identifier_bytes: if max_identifier_bytes == 0 {
            defaults.max_identifier_bytes
        } else {
            max_identifier_bytes as usize
        },
    };
    let read_defaults = ReadLimits::default();
    let read_limits = ReadLimits {
        max_stored_frame_bytes: if max_stored_frame_bytes == 0 {
            read_defaults.max_stored_frame_bytes
        } else {
            u64::from(max_stored_frame_bytes)
        },
        max_decoded_frame_bytes: if max_decoded_frame_bytes == 0 {
            read_defaults.max_decoded_frame_bytes
        } else {
            u64::from(max_decoded_frame_bytes)
        },
        max_directory_entries: if max_directory_entries == 0 {
            read_defaults.max_directory_entries
        } else {
            u64::from(max_directory_entries)
        },
        max_wal_frames: if max_wal_frames == 0 {
            read_defaults.max_wal_frames
        } else {
            u64::from(max_wal_frames)
        },
        max_fold_blocks: if max_fold_blocks == 0 {
            read_defaults.max_fold_blocks
        } else {
            u64::from(max_fold_blocks)
        },
    };
    match Store::open_with_options(
        Path::new(dir),
        StoreOptions { fold: cfg, write_limits: limits, read_limits, ..StoreOptions::default() },
    ) {
        Ok(s) => STORES.with(|slot| {
            let mut slot = slot.borrow_mut();
            // Reuse a closed slot before growing, so a long-lived process that opens and closes
            // stores does not leak handles.
            let idx = slot.iter().position(|x| x.is_none()).unwrap_or_else(|| {
                slot.push(None);
                slot.len() - 1
            });
            slot[idx] = Some(s);
            idx as i32
        }),
        Err(e) => fail(e),
    }
}

/// Close a store, dropping the handle. Flushes nothing — call [`tdb_sync`] first if the writes
/// must survive.
///
/// Deliberately not "releases its writer lock": this binding is the WASI build, which holds no
/// advisory lock to release. Closing hands exclusion back to nobody, because the engine never had
/// it.
#[no_mangle]
pub extern "C" fn tdb_close(h: i32) -> i32 {
    clear_err();
    STORES.with(|s| {
        let mut s = s.borrow_mut();
        match usize::try_from(h).ok().and_then(|i| s.get_mut(i)) {
            Some(slot) if slot.is_some() => {
                *slot = None;
                0
            }
            _ => fail(format!("store handle {h} is not open")),
        }
    })
}

// ── Writes ──────────────────────────────────────────────────────────────────

/// Put one record, carved by the engine's default opinion.
///
/// # Safety
/// All pointer/length pairs must describe valid memory; `id` and `attrs` must be UTF-8.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn tdb_put_body(
    h: i32,
    id: *const u8,
    id_len: u32,
    body: *const u8,
    body_len: u32,
    attrs: *const u8,
    attrs_len: u32,
) -> i32 {
    clear_err();
    let id = match text(id, id_len) {
        Ok(v) => v,
        Err(e) => return fail(format!("id is not UTF-8: {e}")),
    };
    let attrs = match decode_attrs(slice(attrs, attrs_len)) {
        Ok(a) => a,
        Err(e) => return fail(e),
    };
    let body = slice(body, body_len);
    with_store(h, |s| s.put_body(id, body, attrs).map_err(fail)).map_or(-1, |_| 0)
}

/// Apply a whole batch atomically: `[["put", id, bodyBase64, attrs], ["del", id], ...]`.
///
/// One export call per export batch is the shape that matters — a batch replays all-or-nothing, so
/// a crash cannot leave a partial export committed.
///
/// # Safety
/// `json` must be valid UTF-8 JSON of `json_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn tdb_apply(h: i32, json: *const u8, json_len: u32) -> i32 {
    clear_err();
    let raw = slice(json, json_len);
    let v: serde_json::Value = match serde_json::from_slice(raw) {
        Ok(v) => v,
        Err(e) => return fail(format!("batch is not valid JSON: {e}")),
    };
    let items = match v.as_array() {
        Some(a) => a,
        None => return fail("batch must be an array"),
    };
    // Build the whole batch before touching the store: a malformed item must not leave half an
    // export applied.
    let mut batch = turndb::store::Batch::new();
    let mut n = 0usize;
    for (i, item) in items.iter().enumerate() {
        let t = match item.as_array() {
            Some(t) if !t.is_empty() => t,
            _ => return fail(format!("batch item {i} is not a non-empty array")),
        };
        match t[0].as_str() {
            Some("put") => {
                if t.len() != 4 {
                    return fail(format!("batch item {i}: put needs [op, id, body, attrs]"));
                }
                let id = match t[1].as_str() {
                    Some(s) => s,
                    None => return fail(format!("batch item {i}: id is not a string")),
                };
                let body = match t[2].as_str() {
                    Some(s) => match b64_decode(s) {
                        Ok(b) => b,
                        Err(e) => return fail(format!("batch item {i}: {e}")),
                    },
                    None => return fail(format!("batch item {i}: body is not base64 text")),
                };
                let attrs = match decode_attrs(t[3].to_string().as_bytes()) {
                    Ok(a) => a,
                    Err(e) => return fail(format!("batch item {i}: {e}")),
                };
                batch.put_body(id, &body, attrs);
                n += 1;
            }
            Some("del") => {
                if t.len() != 2 {
                    return fail(format!("batch item {i}: del needs [op, id]"));
                }
                match t[1].as_str() {
                    Some(id) => batch.delete(id),
                    None => return fail(format!("batch item {i}: id is not a string")),
                }
                n += 1;
            }
            _ => return fail(format!("batch item {i}: op must be \"put\" or \"del\"")),
        }
    }
    match with_store(h, |s| s.apply(batch).map_err(fail)) {
        Ok(()) => n as i32,
        Err(_) => -1,
    }
}

/// Delete one record (writes a tombstone).
///
/// # Safety
/// `id` must be valid UTF-8 of `id_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn tdb_delete(h: i32, id: *const u8, id_len: u32) -> i32 {
    clear_err();
    let id = match text(id, id_len) {
        Ok(v) => v,
        Err(e) => return fail(format!("id is not UTF-8: {e}")),
    };
    with_store(h, |s| s.delete(id).map_err(fail)).map_or(-1, |_| 0)
}

/// Make everything written so far durable. This is the ACK point.
#[no_mangle]
pub extern "C" fn tdb_sync(h: i32) -> i32 {
    clear_err();
    with_store(h, |s| s.sync().map_err(fail)).map_or(-1, |_| 0)
}

/// Seal the memtable into an immutable part. Reads through this handle do not need it — the writer
/// sees its own unflushed writes — but the columnar plane and any other reader do.
#[no_mangle]
pub extern "C" fn tdb_flush(h: i32) -> i32 {
    clear_err();
    with_store(h, |s| s.flush().map_err(fail)).map_or(-1, |_| 0)
}

/// Merge parts if the threshold is reached. Returns 1 if a merge ran, 0 if not.
#[no_mangle]
pub extern "C" fn tdb_auto_compact(h: i32) -> i32 {
    clear_err();
    with_store(h, |s| s.auto_compact().map_err(fail)).map_or(-1, |m| i32::from(m.is_some()))
}

/// Bounded compaction: if at least `trigger` parts are live, merge the oldest `run` of them.
/// Returns 1 if a merge ran, 0 if not.
///
/// This exists for single-threaded embedders. `tdb_auto_compact` runs a TOTAL merge whose wall
/// time is linear in the whole store; on this build that work happens on the caller's thread, so
/// an embedder with a latency budget needs a merge whose input — and therefore stall — it can
/// bound. `Store::maybe_compact` is the engine's own dial for exactly that; this only reaches it.
#[no_mangle]
pub extern "C" fn tdb_maybe_compact(h: i32, trigger: u32, run: u32) -> i32 {
    clear_err();
    with_store(h, |s| s.maybe_compact(trigger as usize, run as usize).map_err(fail))
        .map_or(-1, |m| i32::from(m.is_some()))
}

// ── Reads ───────────────────────────────────────────────────────────────────

/// Reconstruct a record's body byte-exact into the output buffer.
/// Returns 1 when found, 0 when absent or deleted, -1 on error.
///
/// # Safety
/// `id` must be valid UTF-8 of `id_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn tdb_reconstruct(h: i32, id: *const u8, id_len: u32) -> i32 {
    clear_err();
    let id = match text(id, id_len) {
        Ok(v) => v,
        Err(e) => return fail(format!("id is not UTF-8: {e}")),
    };
    match with_store(h, |s| s.reconstruct(id).map_err(fail)) {
        Ok(Some(b)) => {
            set_out(&b);
            1
        }
        Ok(None) => {
            set_out(&[]);
            0
        }
        Err(_) => -1,
    }
}

/// A record's id, body and attributes as JSON, body base64-encoded.
/// Returns 1 when found, 0 when absent, -1 on error.
///
/// # Safety
/// `id` must be valid UTF-8 of `id_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn tdb_get_record(h: i32, id: *const u8, id_len: u32) -> i32 {
    clear_err();
    let id_s = match text(id, id_len) {
        Ok(v) => v,
        Err(e) => return fail(format!("id is not UTF-8: {e}")),
    };
    let found = match with_store(h, |s| {
        let rec = s.get(id_s).map_err(fail)?;
        let body = s.reconstruct(id_s).map_err(fail)?;
        Ok((rec, body))
    }) {
        Ok(v) => v,
        Err(_) => return -1,
    };
    match found {
        (Some(rec), Some(body)) => {
            let v = serde_json::json!({
                "id": rec.id,
                "body": b64_encode(&body),
                "attrs": encode_attrs(&rec.attrs),
            });
            set_out(v.to_string().as_bytes());
            1
        }
        _ => {
            set_out(&[]);
            0
        }
    }
}

/// Live ids in `[from, to)` in id order, at most `limit`, as a JSON array of strings.
///
/// Empty `from`/`to` mean unbounded. Because ids sort lexicographically, a caller who designs ids
/// with the query in mind gets prefix-then-time paging out of this with no secondary index.
///
/// # Safety
/// `from`/`to` must be valid UTF-8 of their stated lengths.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn tdb_scan_ids(
    h: i32,
    from: *const u8,
    from_len: u32,
    to: *const u8,
    to_len: u32,
    limit: u32,
    reverse: u32,
) -> i32 {
    clear_err();
    let from = match text(from, from_len) {
        Ok(v) => v,
        Err(e) => return fail(format!("`from` is not UTF-8: {e}")),
    };
    let to = match text(to, to_len) {
        Ok(v) => v,
        Err(e) => return fail(format!("`to` is not UTF-8: {e}")),
    };
    let f = (!from.is_empty()).then_some(from);
    let t = (!to.is_empty()).then_some(to);
    match with_store(h, |s| s.scan_ids(f, t, limit as usize, reverse != 0).map_err(fail)) {
        Ok(ids) => {
            set_out(serde_json::Value::from(ids).to_string().as_bytes());
            0
        }
        Err(_) => -1,
    }
}

// ── Structured scan ─────────────────────────────────────────────────────────
//
// `Store::scan` carries no feature gate, so the predicates, projection, and checked cursors below
// are the same engine surface the native binding exposes — this ABI was simply not carrying them.
// What it cannot carry is the two request fields that need a clock or another thread: `deadline`
// and `cancellation` stay `None` here, and the JS layer documents them as native-only rather than
// accepting a `timeoutMs` this build would silently ignore.

/// Read a `usize` request bound, rejecting a value that is not a non-negative integer.
fn field_usize(v: &serde_json::Value, name: &str, default: usize) -> Result<usize, String> {
    match v.get(name) {
        None | Some(serde_json::Value::Null) => Ok(default),
        Some(serde_json::Value::Number(n)) => n
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| format!("`{name}` is not a non-negative integer")),
        Some(_) => Err(format!("`{name}` is not a number")),
    }
}

/// Read a `u64` request bound. Accepts a decimal string as well as a number, because a JSON number
/// crosses JavaScript as f64 and cannot carry every u64 exactly — the same reason the attribute
/// encoding sends `i`/`u` as text.
fn field_u64(v: &serde_json::Value, name: &str, default: u64) -> Result<u64, String> {
    match v.get(name) {
        None | Some(serde_json::Value::Null) => Ok(default),
        Some(serde_json::Value::Number(n)) => {
            n.as_u64().ok_or_else(|| format!("`{name}` is not a non-negative integer"))
        }
        Some(serde_json::Value::String(s)) => {
            s.parse::<u64>().map_err(|_| format!("`{name}` is not a u64 in decimal text"))
        }
        Some(_) => Err(format!("`{name}` is not a number or decimal string")),
    }
}

fn field_str(v: &serde_json::Value, name: &str) -> Result<Option<String>, String> {
    match v.get(name) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("`{name}` is not a string")),
    }
}

fn decode_compare(s: &str) -> Result<Compare, String> {
    match s {
        "eq" => Ok(Compare::Eq),
        "ne" => Ok(Compare::Ne),
        "lt" => Ok(Compare::Lt),
        "lte" => Ok(Compare::LtEq),
        "gt" => Ok(Compare::Gt),
        "gte" => Ok(Compare::GtEq),
        other => Err(format!("unknown comparison {other:?}")),
    }
}

fn decode_predicate(item: &serde_json::Value, i: usize) -> Result<Predicate, String> {
    let kind = item
        .get("kind")
        .and_then(|k| k.as_str())
        .ok_or_else(|| format!("predicate {i} has no `kind`"))?;
    let op = || -> Result<Compare, String> {
        let raw = item
            .get("op")
            .and_then(|o| o.as_str())
            .ok_or_else(|| format!("predicate {i} has no `op`"))?;
        decode_compare(raw)
    };
    let present = || -> Result<bool, String> {
        item.get("present")
            .and_then(|p| p.as_bool())
            .ok_or_else(|| format!("predicate {i} has no boolean `present`"))
    };
    let name = || -> Result<String, String> {
        item.get("name")
            .and_then(|n| n.as_str())
            .map(str::to_string)
            .ok_or_else(|| format!("predicate {i} has no `name`"))
    };
    match kind {
        "id" => {
            let value = item
                .get("value")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("predicate {i} has no string `value`"))?;
            Ok(Predicate::Id { op: op()?, value: value.to_string() })
        }
        "attr" => {
            let triple = item
                .get("attr")
                .ok_or_else(|| format!("predicate {i} has no `attr` [name, tag, value]"))?;
            let (name, value) = decode_attr_triple(triple, i)?;
            Ok(Predicate::Attr { name, op: op()?, value })
        }
        "attr_exists" => Ok(Predicate::AttrExists { name: name()?, present: present()? }),
        "content_exists" => Ok(Predicate::ContentExists { name: name()?, present: present()? }),
        other => Err(format!("predicate {i} has unknown kind {other:?}")),
    }
}

/// Every key this ABI understands. An unrecognised key is refused rather than ignored: a caller who
/// misspells `maxExamined` and silently gets the default has been told nothing went wrong.
const SCAN_REQUEST_KEYS: &[&str] = &[
    "from",
    "to",
    "direction",
    "cursor",
    "limit",
    "maxExamined",
    "maxResolutionEntries",
    "maxReconstructedBytes",
    "attrs",
    "contents",
    "predicates",
];

fn decode_scan_request(json: &[u8]) -> Result<ScanRequest, String> {
    let v: serde_json::Value = if json.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_slice(json).map_err(|e| format!("scan request is not valid JSON: {e}"))?
    };
    let obj = v.as_object().ok_or("scan request must be a JSON object")?;
    for key in obj.keys() {
        if !SCAN_REQUEST_KEYS.contains(&key.as_str()) {
            return Err(format!("scan request has unknown field {key:?}"));
        }
    }

    let defaults = ScanRequest::default();
    let direction = match v.get("direction") {
        None | Some(serde_json::Value::Null) => Direction::Forward,
        Some(serde_json::Value::String(s)) if s == "forward" => Direction::Forward,
        Some(serde_json::Value::String(s)) if s == "reverse" => Direction::Reverse,
        Some(serde_json::Value::String(s)) => {
            return Err(format!("unknown direction {s:?}"));
        }
        Some(_) => return Err("`direction` is not a string".into()),
    };

    let attrs = match v.get("attrs") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, a)| {
                a.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("projected attribute {i} is not a string"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err("`attrs` is not an array".into()),
    };

    let contents = match v.get("contents") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let name = c
                    .get("name")
                    .and_then(|n| n.as_str())
                    .ok_or_else(|| format!("content selection {i} has no `name`"))?;
                let mode = match c.get("mode").and_then(|m| m.as_str()) {
                    Some("metadata") => ContentMode::Metadata,
                    Some("bytes") => ContentMode::Bytes,
                    Some(other) => {
                        return Err(format!("content selection {i} has unknown mode {other:?}"))
                    }
                    None => {
                        return Err(format!(
                            "content selection {i} has no `mode` (\"metadata\" or \"bytes\")"
                        ))
                    }
                };
                Ok(ContentSelect { name: name.to_string(), mode })
            })
            .collect::<Result<Vec<_>, String>>()?,
        Some(_) => return Err("`contents` is not an array".into()),
    };

    let predicates = match v.get("predicates") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, p)| decode_predicate(p, i))
            .collect::<Result<Vec<_>, String>>()?,
        Some(_) => return Err("`predicates` is not an array".into()),
    };

    Ok(ScanRequest {
        from: field_str(&v, "from")?,
        to: field_str(&v, "to")?,
        direction,
        cursor: field_str(&v, "cursor")?,
        limit: field_usize(&v, "limit", defaults.limit)?,
        max_examined: field_usize(&v, "maxExamined", defaults.max_examined)?,
        max_resolution_entries: field_usize(
            &v,
            "maxResolutionEntries",
            defaults.max_resolution_entries,
        )?,
        max_reconstructed_bytes: field_u64(
            &v,
            "maxReconstructedBytes",
            defaults.max_reconstructed_bytes,
        )?,
        deadline: None,
        cancellation: None,
        attrs,
        contents,
        predicates,
    })
}

/// A counter that can exceed 2^53. Decimal text keeps it exact across JSON; the JS layer returns a
/// BigInt, matching what the native binding returns for the same field.
fn big(n: u64) -> serde_json::Value {
    serde_json::Value::from(n.to_string())
}

fn encode_scan_page(page: &ScanPage) -> serde_json::Value {
    let rows = page
        .rows
        .iter()
        .map(|row| {
            let contents = row
                .contents
                .iter()
                .map(|c| {
                    let mut o = serde_json::json!({ "name": c.name, "present": c.present });
                    let m = o.as_object_mut().expect("just built as an object");
                    if let Some(len) = c.len {
                        m.insert("len".into(), big(len));
                    }
                    if let Some(pieces) = c.pieces {
                        m.insert("pieces".into(), serde_json::Value::from(pieces));
                    }
                    if let Some(identity) = c.identity {
                        m.insert("identity".into(), serde_json::Value::from(identity.to_hex()));
                    }
                    if let Some(bytes) = &c.bytes {
                        m.insert("bytes".into(), serde_json::Value::from(b64_encode(bytes)));
                    }
                    o
                })
                .collect::<Vec<_>>();
            serde_json::json!({
                "id": row.id,
                "attrs": encode_attrs(&row.attrs),
                "contents": contents,
            })
        })
        .collect::<Vec<_>>();

    let s = &page.stats;
    let io = &s.io;
    let r = &s.resolution;
    serde_json::json!({
        "rows": rows,
        "next": page.next,
        "stats": {
            "durationNs": big(s.duration_ns),
            "examined": s.examined,
            "returned": s.returned,
            "duplicateAttrOccurrences": s.duplicate_attr_occurrences,
            "contentValuesReconstructed": s.content_values_reconstructed,
            "reconstructedBytes": big(s.reconstructed_bytes),
            "reconstructionBudgetExhausted": s.reconstruction_budget_exhausted,
            "io": {
                "partSectionsTouched": big(io.part_sections_touched as u64),
                "partSectionCacheHits": big(io.part_section_cache_hits),
                "partSectionCacheMisses": big(io.part_section_cache_misses),
                "partStoredBytesRead": big(io.part_stored_bytes_read),
                "partRawBytesDecoded": big(io.part_raw_bytes_decoded),
                "foldBlocksTouched": big(io.fold_blocks_touched as u64),
                "foldBlockCacheHits": big(io.fold_block_cache_hits),
                "foldBlockCacheMisses": big(io.fold_block_cache_misses),
                "foldStoredBytesRead": big(io.fold_stored_bytes_read),
                "foldRawBytesDecoded": big(io.fold_raw_bytes_decoded),
            },
            "resolution": {
                "physicalRows": big(r.physical_rows as u64),
                "supersededRows": big(r.superseded_rows as u64),
                "tombstones": big(r.tombstones as u64),
                "memtableEntries": big(r.memtable_entries as u64),
                "budgetExhausted": r.budget_exhausted,
            },
        },
    })
}

/// One structured page as JSON: projected attributes, named-content metadata or bytes, the checked
/// continuation cursor, and the page's exact work statistics.
///
/// # Safety
/// `json`/`json_len` must describe initialised memory valid for the call.
#[no_mangle]
pub unsafe extern "C" fn tdb_scan(h: i32, json: *const u8, json_len: u32) -> i32 {
    clear_err();
    let req = match decode_scan_request(slice(json, json_len)) {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    match with_store(h, |s| s.scan(&req).map_err(fail)) {
        Ok(page) => {
            set_out(encode_scan_page(&page).to_string().as_bytes());
            0
        }
        Err(_) => -1,
    }
}

/// Store shape as JSON: record count, part count, and the fold's committed tail.
#[no_mangle]
pub extern "C" fn tdb_stats(h: i32) -> i32 {
    clear_err();
    match with_store(h, |s| {
        let ids = s.ids().map_err(fail)?;
        Ok(serde_json::json!({ "records": ids.len(), "parts": s.parts().len() }))
    }) {
        Ok(v) => {
            set_out(v.to_string().as_bytes());
            0
        }
        Err(_) => -1,
    }
}

// ── base64 ──────────────────────────────────────────────────────────────────
//
// Hand-rolled rather than pulled in: it is thirty lines, it is on the boundary rather than in the
// engine, and the crate's dependency budget is spent on things that earn it.

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(B64[(n >> 18 & 63) as usize] as char);
        out.push(B64[(n >> 12 & 63) as usize] as char);
        out.push(if c.len() > 1 { B64[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if c.len() > 2 { B64[(n & 63) as usize] as char } else { '=' });
    }
    out
}

fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut rev = [255u8; 256];
    for (i, &c) in B64.iter().enumerate() {
        rev[c as usize] = i as u8;
    }
    let bytes: Vec<u8> = s.bytes().filter(|&b| b != b'=' && !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, &b) in chunk.iter().enumerate() {
            let v = rev[b as usize];
            if v == 255 {
                return Err(format!("body is not valid base64 (byte {b:#x})"));
            }
            n |= (v as u32) << (18 - 6 * i);
        }
        // A 4-char group carries 3 bytes, a 3-char group 2, a 2-char group 1. A lone trailing
        // character encodes nothing and is malformed.
        match chunk.len() {
            4 => out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]),
            3 => out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8]),
            2 => out.push((n >> 16) as u8),
            _ => return Err("body is not valid base64 (truncated group)".into()),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrips_every_length_and_byte() {
        for len in 0..64usize {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 % 256) as u8).collect();
            let enc = b64_encode(&data);
            assert_eq!(b64_decode(&enc).unwrap(), data, "len {len} did not round-trip");
        }
        // every byte value survives
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(b64_decode(&b64_encode(&all)).unwrap(), all);
    }

    #[test]
    fn base64_refuses_garbage_rather_than_guessing() {
        assert!(b64_decode("!!!!").is_err());
        assert!(b64_decode("A").is_err(), "a lone character encodes nothing");
    }

    #[test]
    fn attrs_keep_order_and_duplicate_keys() {
        // The property the tagged-array encoding exists for: a JSON object would silently collapse
        // these two `k` entries and reorder the rest, and reconstruction would stop being exact.
        let json = br#"[["k","s","first"],["z","i",-5],["k","s","second"],["f","f",1.5],["b","b",true],["u","u","18446744073709551615"],["x","x",[0,255,128]],["t","t","-9223372036854775808"],["n","n",null]]"#;
        let got = decode_attrs(json).unwrap();
        assert_eq!(got.len(), 9);
        assert_eq!(got[0].0, "k");
        assert_eq!(got[2].0, "k");
        assert!(matches!(&got[0].1, AttrValue::Str(s) if s == "first"));
        assert!(matches!(&got[2].1, AttrValue::Str(s) if s == "second"));
        assert!(matches!(got[1].1, AttrValue::Int(-5)));
        assert!(matches!(got[3].1, AttrValue::Float(f) if f == 1.5));
        assert!(matches!(got[4].1, AttrValue::Bool(true)));
        assert!(matches!(got[5].1, AttrValue::UInt(u64::MAX)));
        assert!(matches!(&got[6].1, AttrValue::Bytes(bytes) if bytes == &[0, 255, 128]));
        assert!(matches!(got[7].1, AttrValue::TimestampNs(i64::MIN)));
        assert!(matches!(got[8].1, AttrValue::Null));
        // and the encoding round-trips
        let back = decode_attrs(encode_attrs(&got).to_string().as_bytes()).unwrap();
        assert_eq!(back.len(), got.len());
        for (a, b) in back.iter().zip(&got) {
            assert_eq!(a.0, b.0);
        }
    }

    #[test]
    fn malformed_attrs_report_which_one() {
        for bad in [
            &br#"{"k":"v"}"#[..],
            &br#"[["k","s"]]"#[..],
            &br#"[["k","q","v"]]"#[..],
            &br#"[["k","i","not a number"]]"#[..],
            &br#"[[1,"s","v"]]"#[..],
        ] {
            assert!(decode_attrs(bad).is_err(), "{:?} must be refused", std::str::from_utf8(bad));
        }
        assert!(decode_attrs(b"").unwrap().is_empty(), "empty means no attributes");
    }

    #[test]
    fn json_boundary_keeps_i64_and_non_finite_f64_exact() {
        let attrs = vec![
            ("min".into(), AttrValue::Int(i64::MIN)),
            ("max".into(), AttrValue::Int(i64::MAX)),
            ("nan".into(), AttrValue::Float(f64::NAN)),
            ("pos".into(), AttrValue::Float(f64::INFINITY)),
            ("neg".into(), AttrValue::Float(f64::NEG_INFINITY)),
        ];
        let json = encode_attrs(&attrs);
        assert_eq!(json[0][2], i64::MIN.to_string());
        assert_eq!(json[1][2], i64::MAX.to_string());
        let got = decode_attrs(json.to_string().as_bytes()).unwrap();
        assert!(matches!(got[0].1, AttrValue::Int(i64::MIN)));
        assert!(matches!(got[1].1, AttrValue::Int(i64::MAX)));
        assert!(matches!(got[2].1, AttrValue::Float(v) if v.is_nan()));
        assert!(matches!(got[3].1, AttrValue::Float(v) if v == f64::INFINITY));
        assert!(matches!(got[4].1, AttrValue::Float(v) if v == f64::NEG_INFINITY));
    }
}
