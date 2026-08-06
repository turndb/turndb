//! The queries a production trace UI actually issues, run against the store built by
//! `genai_dogfood`.
//!
//! usage: genai_query <store-dir> <genai.jsonl>
//!
//! Storage numbers are meaningless if the read path cannot answer. These are the three questions
//! a trace server asks — a member's page, a lookup by `responseId`, and an aggregate — plus the
//! one that matters more than any of them: does the stored context come back byte for byte.

use anyhow::{Context, Result};
use std::io::BufRead;
use std::path::PathBuf;
use std::time::Instant;
use turndb::fold::FoldCfg;
use turndb::query::table::TurndbTable;
use turndb::store::Store;

fn main() -> Result<()> {
    let mut a = std::env::args().skip(1);
    let dir = PathBuf::from(a.next().context("usage: genai_query <store-dir> <genai.jsonl>")?);
    let src = PathBuf::from(a.next().context("usage: genai_query <store-dir> <genai.jsonl>")?);

    // ---- byte-exactness first: the invariant everything else rests on ----
    let rs = Store::open_read(&dir, FoldCfg::default())?;
    let rdr = std::io::BufReader::with_capacity(1 << 22, std::fs::File::open(&src)?);
    let (mut checked, mut bytes) = (0u64, 0u64);
    for line in rdr.lines().take(400) {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let r: serde_json::Value = serde_json::from_str(&line)?;
        let (member, ts, rid) = (
            r["memberName"].as_str().unwrap_or(""),
            r["ts"].as_i64().unwrap_or(0),
            r["responseId"].as_str().unwrap_or(""),
        );
        for (kind, key) in [
            ("system", "systemInstructions"),
            ("input", "inputMessages"),
            ("output", "outputMessages"),
        ] {
            let body = &r[key];
            if body.is_null() || body.as_array().is_some_and(|a| a.is_empty()) {
                continue;
            }
            let want = serde_json::to_vec(body)?;
            let id = format!("{member}/{ts:013}/{rid}#{kind}");
            let got = rs.reconstruct(&id)?.with_context(|| format!("record {id} missing"))?;
            anyhow::ensure!(got == want, "BYTE DRIFT for {id}");
            bytes += got.len() as u64;
            checked += 1;
        }
    }
    println!(
        "byte-exact: {checked} records ({:.1} MiB of context) reconstruct verbatim\n",
        bytes as f64 / 1048576.0
    );

    // ---- the UI's queries ----
    let rs = Store::open_read(&dir, FoldCfg::default())?;
    let (ctx, table) = TurndbTable::context(rs, "genai")?;
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;

    let queries: Vec<(&str, String)> = vec![
        (
            "member page (ts desc, 50)",
            "SELECT id, \"csuite.ts\", \"gen_ai.request.model\" FROM genai \
             WHERE \"csuite.member\" = 'builder-8157' AND \"csuite.kind\" = 'input' \
             ORDER BY \"csuite.ts\" DESC LIMIT 50"
                .into(),
        ),
        (
            "lookup by responseId",
            "SELECT id, \"csuite.kind\" FROM genai WHERE \"gen_ai.response.id\" = '59db0bcd93694a70'".into(),
        ),
        (
            "aggregate: calls + tokens by model",
            "SELECT \"gen_ai.request.model\" AS model, count(*) AS calls \
             FROM genai WHERE \"csuite.kind\" = 'input' GROUP BY model ORDER BY calls DESC LIMIT 5"
                .into(),
        ),
        (
            "custom field filter (cost)",
            "SELECT count(*) AS n FROM genai WHERE \"csuite.cost_usd\" > 0.015".into(),
        ),
        (
            "custom field: objective join key",
            "SELECT \"csuite.objective_id\" AS obj, count(*) AS n FROM genai \
             GROUP BY obj ORDER BY n DESC LIMIT 3"
                .into(),
        ),
    ];

    for (label, sql) in queries {
        table.reset_stats();
        let t = Instant::now();
        let batches = rt.block_on(async {
            ctx.sql(&sql).await?.collect().await.map_err(anyhow::Error::from)
        })?;
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
        let st = table.stats();
        // `rows` is rows EMITTED by the scan, `rows_filtered` those a pushed-down predicate
        // excluded before any array was built. `fold_reads` is the one that matters here: zero
        // means no content block was opened to answer a metadata question.
        println!(
            "{label:<34} {ms:>7.1} ms  {rows:>3} rows  (emitted {}, filtered {}, fold reads {})",
            st.rows, st.rows_filtered, st.fold_reads
        );
        if label.starts_with("aggregate") || label.starts_with("custom field: objective") {
            println!(
                "{}",
                datafusion::arrow::util::pretty::pretty_format_batches(&batches)?
                    .to_string()
                    .lines()
                    .take(6)
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }
    Ok(())
}
