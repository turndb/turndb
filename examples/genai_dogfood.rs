//! CommandSuite's `GenAiInference` records, stored in turndb — the dogfood slice.
//!
//! usage: genai_dogfood <genai.jsonl> <store-dir>
//!
//! The mapping, and the reasoning behind each choice:
//!
//! * **Three records per API call**, `#system` / `#input` / `#output`, rather than one record with
//!   three bodies. A record has one body by design, and the split is already proven by
//!   turndb-datasets' `kind=input` / `kind=output` convention.
//! * **The body is the message array verbatim**, which is what makes the whole thing work: the
//!   engine's carve splits a top-level JSON array at element boundaries, so turn *k*'s messages and
//!   turn *k+1*'s messages resolve to the same pieces. That is the quadratic-to-linear step.
//! * **Ids are `member/ts/responseId#kind`** with the timestamp zero-padded, so ids sort
//!   lexicographically into member-then-time order — which is the access pattern the UI actually
//!   has, and it makes the front-coded id column both compressible and range-scannable.
//! * **Attributes are flattened to gen_ai semconv names**, not stored as nested JSON. `usage`
//!   becomes four `gen_ai.usage.*` integers, so they are queryable columns rather than opaque text.
//! * **`finish_reasons` is a repeated attribute**, not a joined string — turndb preserves duplicate
//!   keys in order, so an array round-trips without inventing a separator that could collide.
//! * **Custom fields pass through with their types inferred.** Anything under `attributes` becomes
//!   a column; one column per (key, type), so a deployment adding its own fields costs nothing and
//!   needs no schema change anywhere.

use anyhow::{Context, Result};
use serde_json::Value;
use std::io::BufRead;
use std::path::PathBuf;
use turndb::fold::FoldCfg;
use turndb::store::{Batch, Store};
use turndb::AttrValue;

/// JSON scalar -> turndb attribute. Objects and arrays are deliberately NOT flattened
/// recursively here: the ones that matter (`usage`) are mapped explicitly with semconv names,
/// and silently stringifying the rest would create columns nobody can query.
fn attr_of(v: &Value) -> Option<AttrValue> {
    match v {
        Value::String(s) => Some(AttrValue::Str(s.clone())),
        Value::Bool(b) => Some(AttrValue::Bool(*b)),
        Value::Number(n) if n.is_i64() => Some(AttrValue::Int(n.as_i64().unwrap())),
        Value::Number(n) => n.as_f64().map(AttrValue::Float),
        _ => None,
    }
}

fn push_str(attrs: &mut Vec<(String, AttrValue)>, k: &str, v: &Value) {
    if let Some(s) = v.as_str() {
        attrs.push((k.into(), AttrValue::Str(s.into())));
    }
}

fn push_int(attrs: &mut Vec<(String, AttrValue)>, k: &str, v: &Value) {
    if let Some(i) = v.as_i64() {
        attrs.push((k.into(), AttrValue::Int(i)));
    }
}

fn main() -> Result<()> {
    let mut a = std::env::args().skip(1);
    let src = PathBuf::from(a.next().context("usage: genai_dogfood <genai.jsonl> <store-dir>")?);
    let dir = PathBuf::from(a.next().context("usage: genai_dogfood <genai.jsonl> <store-dir>")?);
    // Calls per flush. THE live-ness dial: a flush is when records become visible to a separate
    // reader, and also when the open fold block seals — and blocks sealed short compress worse.
    // The tradeoff is real and is measured rather than assumed; see the sweep in the session log.
    let flush_every: usize = a.next().and_then(|s| s.parse().ok()).unwrap_or(512);
    let _ = std::fs::remove_dir_all(&dir);

    let mut s = Store::open(&dir, FoldCfg::default())?;
    let rdr = std::io::BufReader::with_capacity(1 << 22, std::fs::File::open(&src)?);
    let (mut calls, mut records, mut logical) = (0u64, 0u64, 0u64);
    let t0 = std::time::Instant::now();
    let mut batch = Batch::new();

    for line in rdr.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let r: Value = serde_json::from_str(&line)?;
        let member = r["memberName"].as_str().unwrap_or("unknown");
        let ts = r["ts"].as_i64().unwrap_or(0);
        let rid = r["responseId"].as_str().unwrap_or("norid");

        // Attributes shared by all three records of one call.
        let mut base: Vec<(String, AttrValue)> = Vec::new();
        push_str(&mut base, "gen_ai.operation.name", &r["operationName"]);
        push_str(&mut base, "gen_ai.provider.name", &r["provider"]);
        push_str(&mut base, "gen_ai.request.model", &r["model"]);
        push_str(&mut base, "gen_ai.response.id", &r["responseId"]);
        push_str(&mut base, "csuite.query_source", &r["querySource"]);
        push_str(&mut base, "csuite.agent_name", &r["agentName"]);
        base.push(("csuite.member".into(), AttrValue::Str(member.into())));
        base.push(("csuite.ts".into(), AttrValue::Int(ts)));
        push_int(&mut base, "csuite.received_at", &r["receivedAt"]);
        // an ARRAY as repeated attributes, order preserved
        if let Some(fr) = r["finishReasons"].as_array() {
            for x in fr {
                if let Some(s) = x.as_str() {
                    base.push(("gen_ai.response.finish_reasons".into(), AttrValue::Str(s.into())));
                }
            }
        }
        let u = &r["usage"];
        push_int(&mut base, "gen_ai.usage.input_tokens", &u["inputTokens"]);
        push_int(&mut base, "gen_ai.usage.output_tokens", &u["outputTokens"]);
        push_int(&mut base, "gen_ai.usage.cache_read_input_tokens", &u["cacheReadInputTokens"]);
        push_int(
            &mut base,
            "gen_ai.usage.cache_creation_input_tokens",
            &u["cacheCreationInputTokens"],
        );
        // CUSTOM FIELDS: whatever the deployment adds, typed and queryable
        if let Some(obj) = r["attributes"].as_object() {
            for (k, v) in obj {
                if let Some(av) = attr_of(v) {
                    base.push((k.clone(), av));
                }
            }
        }

        for (kind, body_key) in [
            ("system", "systemInstructions"),
            ("input", "inputMessages"),
            ("output", "outputMessages"),
        ] {
            let body = &r[body_key];
            if body.is_null() || body.as_array().is_some_and(|a| a.is_empty()) {
                continue;
            }
            let bytes = serde_json::to_vec(body)?;
            logical += bytes.len() as u64;
            let mut attrs = base.clone();
            attrs.push(("csuite.kind".into(), AttrValue::Str(kind.into())));
            // member / zero-padded ts / responseId — lexicographic order IS member-then-time order
            batch.put_body(&format!("{member}/{ts:013}/{rid}#{kind}"), &bytes, attrs);
            records += 1;
        }
        calls += 1;
        if calls as usize % flush_every == 0 {
            s.apply(std::mem::take(&mut batch))?;
            s.sync()?;
            s.flush()?;
            s.auto_compact()?;
        }
    }
    if !batch.is_empty() {
        s.apply(batch)?;
        s.sync()?;
        s.flush()?;
    }
    while s.part_count() > 1 {
        if s.merge_range(0, s.part_count())?.is_none() {
            break;
        }
    }
    let el = t0.elapsed().as_secs_f64();

    let disk = dir_bytes(&dir);
    println!(
        "flush every {flush_every} calls | {calls} calls -> {records} records\n\
         logical  {:>8.1} MiB\n\
         turndb   {:>8.2} MiB   ({:.1}x)\n\
         ingest   {:>8.1} s     ({:.0} calls/s)\n\
         parts    {}",
        logical as f64 / 1048576.0,
        disk as f64 / 1048576.0,
        logical as f64 / disk as f64,
        el,
        calls as f64 / el.max(0.001),
        s.part_count(),
    );
    Ok(())
}

fn dir_bytes(d: &std::path::Path) -> u64 {
    std::fs::read_dir(d)
        .map(|rd| {
            rd.flatten()
                .map(|e| {
                    let p = e.path();
                    if p.is_dir() {
                        dir_bytes(&p)
                    } else {
                        e.metadata().map(|m| m.len()).unwrap_or(0)
                    }
                })
                .sum()
        })
        .unwrap_or(0)
}
