//! turnd — the ingest daemon: OTLP/HTTP in, Store batches down, ACK out.
//!
//! A server is a ROLE a process takes when it holds the writer lock, and this is that process.
//! The mapping from a request to durability is exact and short:
//!
//! ```text
//! one OTLP export request  ->  one Batch (all-or-nothing across a crash)
//! 200 response             ->  after Store::sync — the WAL's ACK point
//! ```
//!
//! So a client that got a 200 has durable spans, and a crash at ANY moment loses nothing acked —
//! not because turnd is careful, but because the store already is. turnd can be killed rudely;
//! recovery is the substrate's job and is simulation-tested there.
//!
//! # Endpoints
//!
//! * `POST /v1/traces` — OTLP/HTTP, JSON encoding (the standard path and port). Every SDK
//!   reaches this directly or through an OTel Collector (`otlphttp` exporter, `encoding: json`).
//! * `POST /admin/flush` — seal the memtable into a part and run the compaction policy.
//! * `GET  /healthz`
//!
//! # The v0 span mapping (provisional, and says so)
//!
//! * id: `traceId:spanId`
//! * body: the string attribute `turndb.body` when present (what the turndb-datasets transforms
//!   emit); otherwise the span's `events` array as canonical JSON; otherwise empty. Carved by the
//!   engine's default opinion.
//! * attributes: every scalar span attribute, plus `span.name` and `span.kind`.
//!
//! The real gen_ai semconv mapping (prompt/completion events, resource attributes) is design
//! work shared with the datasets specs; this mapping is the honest v0 and is named provisional
//! everywhere it appears.

use anyhow::{Context, Result};
use std::path::Path;
use tiny_http::{Header, Method, Response, Server};
use turndb::fold::FoldCfg;
use turndb::store::{Batch, Store};
use turndb::AttrValue;

/// Flush once the memtable holds this much — group commit stays per-request, parts stay chunky.
const FLUSH_BYTES: usize = 16 << 20;
/// Refuse request bodies past this size before reading them into memory.
const MAX_BODY: usize = 64 << 20;

pub struct Turnd {
    store: Store,
    pub requests: u64,
    pub records: u64,
}

impl Turnd {
    pub fn open(dir: &Path) -> Result<Turnd> {
        Ok(Turnd { store: Store::open(dir, FoldCfg::default())?, requests: 0, records: 0 })
    }

    /// Serve until the process dies. The store makes rude death safe; nothing here needs to be
    /// graceful, only correct before each ACK.
    pub fn serve(mut self, server: Server) -> Result<()> {
        for mut req in server.incoming_requests() {
            let outcome = self.handle(&mut req);
            let resp = match outcome {
                Ok(body) => Response::from_string(body).with_header(json_header()),
                Err(e) => Response::from_string(format!("{{\"error\":{:?}}}", e.to_string()))
                    .with_status_code(400)
                    .with_header(json_header()),
            };
            let _ = req.respond(resp);
        }
        Ok(())
    }

    /// One request, one outcome. Public so tests drive it without sockets.
    pub fn handle(&mut self, req: &mut tiny_http::Request) -> Result<String> {
        match (req.method().clone(), req.url().to_string().as_str()) {
            (Method::Get, "/healthz") => Ok("{\"ok\":true}".into()),
            (Method::Post, "/admin/flush") => {
                self.store.flush()?;
                let merged = self.store.auto_compact()?.is_some();
                Ok(format!("{{\"flushed\":true,\"compacted\":{merged}}}"))
            }
            (Method::Post, "/v1/traces") => {
                let len = req.body_length().unwrap_or(0);
                if len > MAX_BODY {
                    anyhow::bail!("request body of {len} bytes exceeds the {MAX_BODY} cap");
                }
                let mut body = Vec::with_capacity(len.min(MAX_BODY));
                req.as_reader().read_to_end(&mut body).context("read request body")?;
                let n = self.ingest(&body)?;
                self.requests += 1;
                self.records += n;
                // The OTLP success shape: an empty partialSuccess means everything landed.
                Ok("{\"partialSuccess\":{}}".into())
            }
            (m, u) => anyhow::bail!("no route {m} {u}"),
        }
    }

    /// One export request becomes one Batch; the 200 is only earned after sync.
    fn ingest(&mut self, body: &[u8]) -> Result<u64> {
        let v: serde_json::Value = serde_json::from_slice(body).context("OTLP/JSON parse")?;
        let mut batch = Batch::new();
        let mut n = 0u64;
        for rs in v.get("resourceSpans").and_then(|x| x.as_array()).unwrap_or(&Vec::new()) {
            for ss in rs.get("scopeSpans").and_then(|x| x.as_array()).unwrap_or(&Vec::new()) {
                for span in ss.get("spans").and_then(|x| x.as_array()).unwrap_or(&Vec::new()) {
                    let (id, body, attrs) = map_span(span);
                    batch.put_body(&id, &body, attrs);
                    n += 1;
                }
            }
        }
        if n == 0 {
            anyhow::bail!("export carried no spans");
        }
        self.store.apply(batch)?;
        self.store.sync()?; // <- the ACK is earned here
        if self.store.memtable_bytes() >= FLUSH_BYTES {
            self.store.flush()?;
            self.store.auto_compact()?;
        }
        Ok(n)
    }
}

fn json_header() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).expect("static header")
}

/// The v0 mapping. See the module docs; provisional, and structured so the refinement is a
/// function swap.
fn map_span(span: &serde_json::Value) -> (String, Vec<u8>, Vec<(String, AttrValue)>) {
    let s = |k: &str| span.get(k).and_then(|x| x.as_str()).unwrap_or("");
    let id = format!("{}:{}", s("traceId"), s("spanId"));

    let mut attrs: Vec<(String, AttrValue)> = Vec::new();
    if !s("name").is_empty() {
        attrs.push(("span.name".into(), AttrValue::Str(s("name").into())));
    }
    if let Some(k) = span.get("kind").and_then(|x| x.as_i64()) {
        attrs.push(("span.kind".into(), AttrValue::Int(k)));
    }
    let mut body: Option<Vec<u8>> = None;
    for a in span.get("attributes").and_then(|x| x.as_array()).unwrap_or(&Vec::new()) {
        let Some(key) = a.get("key").and_then(|x| x.as_str()) else { continue };
        let Some(val) = a.get("value") else { continue };
        // OTLP/JSON AnyValue: one of stringValue | intValue (an int64 AS A STRING, per spec) |
        // doubleValue | boolValue. Arrays and kvlists are skipped in v0, deliberately.
        let av = if let Some(x) = val.get("stringValue").and_then(|x| x.as_str()) {
            if key == "turndb.body" {
                body = Some(x.as_bytes().to_vec());
                continue;
            }
            AttrValue::Str(x.into())
        } else if let Some(x) = val.get("intValue") {
            let parsed = x.as_i64().or_else(|| x.as_str().and_then(|s| s.parse().ok()));
            match parsed {
                Some(i) => AttrValue::Int(i),
                None => continue,
            }
        } else if let Some(x) = val.get("doubleValue").and_then(|x| x.as_f64()) {
            AttrValue::Float(x)
        } else if let Some(x) = val.get("boolValue").and_then(|x| x.as_bool()) {
            AttrValue::Bool(x)
        } else {
            continue;
        };
        attrs.push((key.to_string(), av));
    }
    let body = body.unwrap_or_else(|| {
        span.get("events")
            .filter(|e| e.as_array().is_some_and(|a| !a.is_empty()))
            .map(|e| e.to_string().into_bytes())
            .unwrap_or_default()
    });
    (id, body, attrs)
}
