//! Read-only browser binding over the core's arbitrary positioned-source entrance.

#[cfg(target_arch = "wasm32")]
mod wasm {
    use anyhow::{anyhow, Result};
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine;
    use js_sys::{BigInt, Error, Function, Reflect, Uint8Array, JSON};
    use serde_json::{json, Value};
    use std::io;
    use std::sync::Arc;
    use turndb::fold::FoldCfg;
    use turndb::readat::ReadAt;
    use turndb::scan::{
        Compare, ContentMode, ContentSelect, Direction, Predicate, ScanExplanation, ScanPage,
        ScanRequest,
    };
    use turndb::schema::{AttrType, Schema};
    use turndb::types::AttrValue;
    use wasm_bindgen::prelude::*;

    struct CallbackSource {
        read: Function,
        len: u64,
    }

    // wasm32-unknown-unknown is single-threaded in this build (`threads: false`). ReadAt keeps its
    // cross-platform Send+Sync contract so native sources can be shared by query workers; the JS
    // function never crosses a thread here.
    unsafe impl Send for CallbackSource {}
    unsafe impl Sync for CallbackSource {}

    impl ReadAt for CallbackSource {
        fn read_exact_at(&self, into: &mut [u8], offset: u64) -> io::Result<()> {
            let offset_js = BigInt::from(offset);
            let length_js = JsValue::from_f64(into.len() as f64);
            let value = self
                .read
                .call2(&JsValue::UNDEFINED, &offset_js.into(), &length_js)
                .map_err(|error| {
                    io::Error::new(io::ErrorKind::Other, js_error("browser range callback", error))
                })?;
            if value.is_null() || value.is_undefined() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("TURNDB_RANGE:{offset}:{}", into.len()),
                ));
            }
            let bytes = Uint8Array::new(&value);
            if bytes.length() as usize != into.len() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "browser range callback returned {} bytes for {} at {offset}",
                        bytes.length(),
                        into.len()
                    ),
                ));
            }
            bytes.copy_to(into);
            Ok(())
        }

        fn len(&self) -> io::Result<u64> {
            Ok(self.len)
        }
    }

    fn js_error(context: &str, error: JsValue) -> String {
        error
            .as_string()
            .map(|message| format!("{context}: {message}"))
            .unwrap_or_else(|| context.to_string())
    }

    fn input(value: JsValue) -> Result<Value> {
        let text = JSON::stringify(&value)
            .map_err(|error| anyhow!(js_error("serialize browser request", error)))?
            .as_string()
            .ok_or_else(|| anyhow!("browser request is not JSON data"))?;
        Ok(serde_json::from_str(&text)?)
    }

    fn output(value: Value) -> std::result::Result<JsValue, JsValue> {
        JSON::parse(&value.to_string())
    }

    fn failure(error: anyhow::Error) -> JsValue {
        let rendered = Error::new(&format!("{error:#}"));
        let _ = Reflect::set(
            &rendered,
            &JsValue::from_str("code"),
            &JsValue::from_str(turndb::error::classify(&error).code()),
        );
        rendered.into()
    }

    #[wasm_bindgen]
    pub struct BrowserStore {
        store: turndb::store::ReadStore,
    }

    #[wasm_bindgen]
    impl BrowserStore {
        #[wasm_bindgen(js_name = open)]
        pub fn open(
            read: Function,
            length: u64,
            label: Option<String>,
        ) -> Result<BrowserStore, JsValue> {
            let source = Arc::new(CallbackSource { read, len: length });
            let store = turndb::store::open_read_container_source(
                source,
                label.as_deref().unwrap_or("browser://source"),
                FoldCfg::default(),
                turndb::read_limits::ReadLimits::default(),
            )
            .map_err(failure)?;
            Ok(BrowserStore { store })
        }

        pub fn capabilities() -> Result<JsValue, JsValue> {
            let compiled = turndb::capabilities::capabilities();
            output(json!({
                "contractVersion": 1,
                "profile": "browser",
                "operations": ["openSnapshot", "compiledCapabilities", "scan", "explainScan", "schema", "readContent", "close"],
                "partFormat": { "readMax": compiled.part_format_read_max },
                "writerExclusion": "read_only",
                "positionedIo": true,
                "threads": false,
                "columnar": true,
                "sql": false,
                "arrowIpc": false,
                "reclamation": "none",
                "cancellation": { "scan": false, "lifecycle": false },
                "transport": { "buffer": true, "blob": true, "httpRange": true }
            }))
        }

        pub fn scan(&self, request: JsValue) -> Result<JsValue, JsValue> {
            let request = decode_scan(input(request).map_err(failure)?).map_err(failure)?;
            let page = self.store.scan(&request).map_err(failure)?;
            output(encode_page(page))
        }

        #[wasm_bindgen(js_name = explainScan)]
        pub fn explain_scan(&self, request: JsValue) -> Result<JsValue, JsValue> {
            let request = decode_scan(input(request).map_err(failure)?).map_err(failure)?;
            output(encode_explanation(self.store.explain_scan(&request).map_err(failure)?))
        }

        pub fn schema(&self) -> Result<JsValue, JsValue> {
            output(encode_schema(self.store.schema().map_err(failure)?))
        }

        #[wasm_bindgen(js_name = readContent)]
        pub fn read_content(
            &self,
            id: String,
            name: String,
        ) -> Result<Option<Uint8Array>, JsValue> {
            self.store
                .reconstruct_content(&id, &name)
                .map(|bytes| bytes.map(|bytes| Uint8Array::from(bytes.as_slice())))
                .map_err(failure)
        }

        /// Release the wasm-owned snapshot and its caches deterministically.
        pub fn close(self) {}
    }

    fn decode_scan(value: Value) -> Result<ScanRequest> {
        if value.get("contractVersion").and_then(Value::as_u64) != Some(1) {
            return Err(anyhow!("scan request contractVersion must be 1"));
        }
        let mut request = ScanRequest::default();
        request.from = optional_string(&value, "from")?;
        request.to = optional_string(&value, "to")?;
        request.cursor = optional_string(&value, "cursor")?;
        request.direction =
            match value.get("direction").and_then(Value::as_str).unwrap_or("forward") {
                "forward" => Direction::Forward,
                "reverse" => Direction::Reverse,
                other => return Err(anyhow!("unknown scan direction {other:?}")),
            };
        if let Some(limit) = value.get("limit") {
            request.limit = number(limit, "limit")?;
        }
        if let Some(limit) = value.get("maxExamined") {
            request.max_examined = number(limit, "maxExamined")?;
        }
        if let Some(limit) = value.get("maxResolutionEntries") {
            request.max_resolution_entries = number(limit, "maxResolutionEntries")?;
        }
        if let Some(limit) = value.get("maxReconstructedBytes") {
            request.max_reconstructed_bytes = limit
                .as_str()
                .ok_or_else(|| anyhow!("maxReconstructedBytes must be decimal text"))?
                .parse()?;
        }
        request.attrs = strings(value.get("attrs"), "attrs")?;
        request.contents = value
            .get("contents")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|content| {
                Ok(ContentSelect {
                    name: field(content, "name")?.to_string(),
                    mode: match field(content, "mode")? {
                        "metadata" => ContentMode::Metadata,
                        "bytes" => ContentMode::Bytes,
                        other => return Err(anyhow!("unknown content mode {other:?}")),
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;
        request.predicates = value
            .get("predicates")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(predicate)
            .collect::<Result<Vec<_>>>()?;
        Ok(request)
    }

    fn predicate(value: &Value) -> Result<Predicate> {
        match field(value, "kind")? {
            "id" => Ok(Predicate::Id {
                op: compare(field(value, "op")?)?,
                value: field(value, "value")?.to_string(),
            }),
            "attr" => Ok(Predicate::Attr {
                name: field(value, "name")?.to_string(),
                op: compare(field(value, "op")?)?,
                value: scalar(
                    value.get("value").ok_or_else(|| anyhow!("attribute predicate needs value"))?,
                )?,
            }),
            "attrExists" => Ok(Predicate::AttrExists {
                name: field(value, "name")?.to_string(),
                present: boolean(value, "present")?,
            }),
            "contentExists" => Ok(Predicate::ContentExists {
                name: field(value, "name")?.to_string(),
                present: boolean(value, "present")?,
            }),
            other => Err(anyhow!("unknown predicate kind {other:?}")),
        }
    }

    fn scalar(value: &Value) -> Result<AttrValue> {
        match field(value, "type")? {
            "string" => Ok(AttrValue::Str(field(value, "value")?.to_string())),
            "i64" => Ok(AttrValue::Int(field(value, "decimal")?.parse()?)),
            "f64" => {
                let bits = field(value, "bitsHex")?;
                if bits.len() != 16
                    || !bits
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                {
                    return Err(anyhow!(
                        "f64 bitsHex must be sixteen lowercase hexadecimal digits"
                    ));
                }
                Ok(AttrValue::Float(f64::from_bits(u64::from_str_radix(bits, 16)?)))
            }
            "bool" => Ok(AttrValue::Bool(boolean(value, "value")?)),
            "u64" => Ok(AttrValue::UInt(field(value, "decimal")?.parse()?)),
            "binary" => Ok(AttrValue::Bytes(BASE64.decode(field(value, "base64")?)?)),
            "timestampNs" => Ok(AttrValue::TimestampNs(field(value, "decimal")?.parse()?)),
            "null" => Ok(AttrValue::Null),
            other => Err(anyhow!("unknown scalar type {other:?}")),
        }
    }

    fn compare(value: &str) -> Result<Compare> {
        match value {
            "eq" => Ok(Compare::Eq),
            "ne" => Ok(Compare::Ne),
            "lt" => Ok(Compare::Lt),
            "lte" => Ok(Compare::LtEq),
            "gt" => Ok(Compare::Gt),
            "gte" => Ok(Compare::GtEq),
            other => Err(anyhow!("unknown comparison {other:?}")),
        }
    }

    fn encode_scalar(value: &AttrValue) -> Value {
        match value {
            AttrValue::Str(value) => json!({ "type": "string", "value": value }),
            AttrValue::Int(value) => json!({ "type": "i64", "decimal": value.to_string() }),
            AttrValue::Float(value) => {
                json!({ "type": "f64", "bitsHex": format!("{:016x}", value.to_bits()) })
            }
            AttrValue::Bool(value) => json!({ "type": "bool", "value": value }),
            AttrValue::UInt(value) => json!({ "type": "u64", "decimal": value.to_string() }),
            AttrValue::Bytes(value) => json!({ "type": "binary", "base64": BASE64.encode(value) }),
            AttrValue::TimestampNs(value) => {
                json!({ "type": "timestampNs", "decimal": value.to_string() })
            }
            AttrValue::Null => json!({ "type": "null" }),
        }
    }

    fn encode_page(page: ScanPage) -> Value {
        let mut result = json!({
            "contractVersion": 1,
            "rows": page.rows.into_iter().map(|row| json!({
                "id": row.id,
                "attrs": row.attrs.into_iter().map(|(name, value)| json!({ "name": name, "value": encode_scalar(&value) })).collect::<Vec<_>>(),
                "contents": row.contents.into_iter().map(|content| {
                    let mut value = json!({ "name": content.name, "present": content.present });
                    if let Some(len) = content.len { value["len"] = Value::String(len.to_string()); }
                    if let Some(pieces) = content.pieces { value["pieces"] = Value::String(pieces.to_string()); }
                    if let Some(identity) = content.identity { value["identityHex"] = Value::String(identity.to_hex()); }
                    if let Some(bytes) = content.bytes { value["base64"] = Value::String(BASE64.encode(bytes)); }
                    value
                }).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "stats": {
                "durationNs": page.stats.duration_ns.to_string(),
                "examined": page.stats.examined.to_string(),
                "returned": page.stats.returned.to_string(),
                "duplicateAttrOccurrences": page.stats.duplicate_attr_occurrences.to_string(),
                "contentValuesReconstructed": page.stats.content_values_reconstructed.to_string(),
                "reconstructedBytes": page.stats.reconstructed_bytes.to_string(),
                "predicatePrunedRows": page.stats.predicate_pruned_rows.to_string(),
                "reconstructionBudgetExhausted": page.stats.reconstruction_budget_exhausted,
                "io": {
                    "partSectionsTouched": page.stats.io.part_sections_touched.to_string(),
                    "partSectionCacheHits": page.stats.io.part_section_cache_hits.to_string(),
                    "partSectionCacheMisses": page.stats.io.part_section_cache_misses.to_string(),
                    "partStoredBytesRead": page.stats.io.part_stored_bytes_read.to_string(),
                    "partRawBytesDecoded": page.stats.io.part_raw_bytes_decoded.to_string(),
                    "foldBlocksTouched": page.stats.io.fold_blocks_touched.to_string(),
                    "foldBlockCacheHits": page.stats.io.fold_block_cache_hits.to_string(),
                    "foldBlockCacheMisses": page.stats.io.fold_block_cache_misses.to_string(),
                    "foldStoredBytesRead": page.stats.io.fold_stored_bytes_read.to_string(),
                    "foldRawBytesDecoded": page.stats.io.fold_raw_bytes_decoded.to_string()
                },
                "resolution": {
                    "physicalRows": page.stats.resolution.physical_rows.to_string(),
                    "supersededRows": page.stats.resolution.superseded_rows.to_string(),
                    "tombstones": page.stats.resolution.tombstones.to_string(),
                    "memtableEntries": page.stats.resolution.memtable_entries.to_string(),
                    "budgetExhausted": page.stats.resolution.budget_exhausted
                }
            }
        });
        if let Some(next) = page.next {
            result["next"] = Value::String(next);
        }
        result
    }

    fn encode_explanation(value: ScanExplanation) -> Value {
        json!({
            "direction": match value.direction { Direction::Forward => "forward", Direction::Reverse => "reverse" },
            "usesCursor": value.uses_cursor,
            "effectiveFrom": value.effective_from,
            "effectiveTo": value.effective_to,
            "emptyRange": value.empty_range,
            "projectedAttrs": value.projected_attrs,
            "requiredAttrs": value.required_attrs,
            "predicateOnlyAttrs": value.predicate_only_attrs,
            "limit": value.limit,
            "physical": {
                "immutablePartsConsidered": value.physical.immutable_parts_considered.to_string(),
                "immutablePartsWithRows": value.physical.immutable_parts_with_rows.to_string(),
                "immutableRowsInBounds": value.physical.immutable_rows_in_bounds.to_string(),
            }
        })
    }

    fn encode_schema(schema: Schema) -> Value {
        fn kind(value: AttrType) -> &'static str {
            match value {
                AttrType::String => "string",
                AttrType::Int => "i64",
                AttrType::Float => "f64",
                AttrType::Bool => "bool",
                AttrType::UInt => "u64",
                AttrType::Binary => "binary",
                AttrType::TimestampNs => "timestampNs",
                AttrType::Null => "null",
            }
        }
        json!({
            "attributes": schema.attributes.into_iter().map(|attribute| json!({
                "name": attribute.name,
                "types": attribute.types.into_iter().map(kind).collect::<Vec<_>>()
            })).collect::<Vec<_>>(),
            "contents": schema.contents,
            "mayIncludeShadowedFields": schema.may_include_shadowed_fields,
        })
    }

    fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
        value.get(name).and_then(Value::as_str).ok_or_else(|| anyhow!("{name} must be a string"))
    }
    fn boolean(value: &Value, name: &str) -> Result<bool> {
        value.get(name).and_then(Value::as_bool).ok_or_else(|| anyhow!("{name} must be a boolean"))
    }
    fn number(value: &Value, name: &str) -> Result<usize> {
        value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| anyhow!("{name} must be a non-negative integer"))
    }
    fn optional_string(value: &Value, name: &str) -> Result<Option<String>> {
        value
            .get(name)
            .map(|value| {
                value.as_str().map(str::to_string).ok_or_else(|| anyhow!("{name} must be a string"))
            })
            .transpose()
    }
    fn strings(value: Option<&Value>, name: &str) -> Result<Vec<String>> {
        value.map_or_else(
            || Ok(Vec::new()),
            |value| {
                value
                    .as_array()
                    .ok_or_else(|| anyhow!("{name} must be an array"))?
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .map(str::to_string)
                            .ok_or_else(|| anyhow!("{name} values must be strings"))
                    })
                    .collect()
            },
        )
    }
}

#[cfg(target_arch = "wasm32")]
pub use wasm::*;
