//! The `turndb` CLI — the operator's porch on the product.
//!
//! A store you can inspect, verify, query, back up, and promote a retained manifest revision in from
//! a command line with no server
//! running is a store an auditor can trust. Every verb takes a `.turndb` file — the only layout a
//! store has.
//!
//! Argument parsing is by hand, on purpose: the crate's dependency discipline does not bend for
//! flag ergonomics.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use turndb::fold::FoldCfg;
use turndb::store::{ReadStore, Store};

const USAGE: &str = "\
turndb — the database for AI traces, in one file

usage: turndb <verb> [args]

  reading (STORE is a .turndb file):
    inspect   <STORE>            what is inside: manifest, parts, fold, members, retained revisions
    ids       <STORE>            every record id visible in the current read view, one per line
    get       <STORE> <ID>       reconstruct one record's content to stdout, byte-exact
    verify    <STORE> [--deep]   integrity: structural checksums, the manifest chain, every part
                                 pin; --deep reconstructs everything
    query     <STORE> <SQL>      run SQL over the store (table name: t)
    snapshots <STORE>            list retained manifest revisions available to time travel

  writing (OS writer lock on native container handle; creates the file if absent):
    import    <STORE> <JSONL>    ingest records ({\"body\": ..., attrs...} per line; - for stdin),
                                 carved by the engine's default opinion, batched per 1000

  operating (writer role):
    compact   <STORE>            merge every part referenced by the current manifest revision
    refold    <STORE>            rewrite the fold, dropping content no record in the current
                                 manifest revision requires
    content-punch <STORE>        declare unreachable fold blocks, then attempt to deallocate their
                                 payloads in place. No offsets move and no parts are rebuilt
    free-space-punch <STORE>     attempt to deallocate free-extent interiors older than the
                                 retention window. No logical state or offsets change
    erase     <STORE> (--id ID ... | --attr KEY=VALUE)
                                 delete, synchronize, publish, total-merge when needed, and refold
                                 until this store no longer references the content or metadata.
                                 Does not promise media-byte
                                 non-recoverability; prints the measurable result
    recover   <STORE> [--max-rollback N]
                                 validate and promote a retained manifest revision; rollback defaults to 0
    reclaim   <STORE>            rewrite the file without the extents nothing names any more —
                                 returns edge bytes and fragmentation in-place deallocation cannot

  shipping:
    backup    <STORE> <OUT>      current store authority, copied as one
                                 self-contained store without retained revisions and atomically
                                 installed only if OUT does not exist

  about this binary:
    version                      the crate version compiled in (also --version, -V)
";

fn main() {
    // A CLI that panics when its stdout pipe closes (`| head`) is broken by Unix's rules, not its
    // own: restore default SIGPIPE so truncated output ends the process quietly. Unix-only because
    // the signal is — WASI has no SIGPIPE, and nothing to restore.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args) {
        eprintln!("turndb: {e:#}");
        std::process::exit(1);
    }
}

/// BLAKE3 over a positioned reader, in a fixed window.
fn digest_reader(r: &dyn turndb::readat::ReadAt) -> Result<String> {
    let len = r.len()?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut at = 0u64;
    while at < len {
        let take = buf.len().min((len - at) as usize);
        r.read_exact_at(&mut buf[..take], at)?;
        hasher.update(&buf[..take]);
        at += take as u64;
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn open_read(path: &Path) -> Result<ReadStore> {
    turndb::store::open_read_container(path, FoldCfg::default())
}

/// Open the writer on a `.turndb` file — every mutating verb comes through here.
fn open_writer(path: &Path) -> Result<Store> {
    Store::open_file(path, FoldCfg::default())
}

fn run(args: &[String]) -> Result<()> {
    let mut it = args.iter().map(String::as_str);
    let verb = it.next().unwrap_or("help");
    let rest: Vec<&str> = it.collect();
    let arg = |i: usize, what: &str| -> Result<PathBuf> {
        rest.get(i).map(PathBuf::from).ok_or_else(|| anyhow::anyhow!("missing {what}\n\n{USAGE}"))
    };
    match verb {
        "inspect" => inspect(&arg(0, "STORE")?),
        "ids" => {
            let rs = open_read(&arg(0, "STORE")?)?;
            let mut out = std::io::stdout().lock();
            for id in rs.ids()? {
                writeln!(out, "{id}")?;
            }
            Ok(())
        }
        "get" => {
            let rs = open_read(&arg(0, "STORE")?)?;
            let id = rest.get(1).ok_or_else(|| anyhow::anyhow!("missing ID\n\n{USAGE}"))?;
            match rs.reconstruct(id)? {
                Some(b) => {
                    std::io::stdout().lock().write_all(&b)?;
                    Ok(())
                }
                None => bail!("no record {id:?}"),
            }
        }
        "verify" => verify(&arg(0, "STORE")?, rest.contains(&"--deep")),
        "query" => query(&arg(0, "STORE")?, rest.get(1).copied()),
        "compact" => {
            let mut s = open_writer(&arg(0, "STORE")?)?;
            s.sync()?;
            s.flush()?;
            let n = s.part_count();
            match s.merge_range(0, n)? {
                Some(st) => {
                    println!(
                        "merged {} parts, {} records in, {} out ({} superseded, {} tombstones dropped)",
                        st.inputs, st.records_in, st.records_out, st.superseded, st.tombstones_dropped
                    );
                }
                None => {
                    println!("nothing to merge ({n} part{})", if n == 1 { "" } else { "s" });
                }
            }
            // Every writer verb closes the store it opened: close leaves the store settled and removes the
            // WAL sidecar, and "a cleanly closed store is exactly one file" is the README's promise
            // (#122). Only `import` did this before.
            s.close()?;
            Ok(())
        }
        "content-punch" => {
            let mut s = open_writer(&arg(0, "STORE")?)?;
            s.flush()?;
            let st = s.punch_unreferenced()?;
            println!(
                "punched {} of {} unreachable blocks (the manifest names them; metadata residue \
                 remains in parts until a refold)",
                st.blocks_punched, st.blocks_examined
            );
            s.close()?;
            Ok(())
        }
        "free-space-punch" => {
            let mut s = open_writer(&arg(0, "STORE")?)?;
            let fp = s.punch_free_space()?;
            println!(
                "returned {} free bytes across {} extents ({} deferred inside the retention \
                 window, {} edge bytes await a reclaim)",
                fp.punched_bytes, fp.punched_extents, fp.deferred_extents, fp.edge_bytes
            );
            s.close()?;
            Ok(())
        }
        "refold" => {
            let mut s = open_writer(&arg(0, "STORE")?)?;
            s.sync()?;
            s.flush()?;
            let st = s.refold()?;
            println!(
                "kept {} records and {} pieces; dropped {} records and {} pieces; reclaimed {} bytes{}",
                st.records_kept,
                st.pieces_kept,
                st.records_dropped,
                st.pieces_dropped,
                st.bytes_reclaimed(),
                if st.stale_generation_left { " (stale generation still on disk)" } else { "" }
            );
            s.close()?;
            Ok(())
        }
        "recover" => {
            let max_rollback_commits = match rest.get(1..) {
                Some(["--max-rollback", value]) => value
                    .parse::<u64>()
                    .with_context(|| format!("invalid --max-rollback value {value:?}"))?,
                Some([]) | None => 0,
                _ => bail!("recover accepts only --max-rollback N\n\n{USAGE}"),
            };
            let report = turndb::store::promote_manifest_file(
                &arg(0, "STORE")?,
                FoldCfg::default(),
                turndb::store::ManifestPromotionOptions { max_rollback_commits },
            )?;
            println!(
                "promoted retained manifest revision {} to MANIFEST (rollback {}, {} records, {} content values verified)",
                report.commit, report.rollback_commits, report.records, report.content_values
            );
            Ok(())
        }
        "snapshots" => {
            for c in turndb::store::retained_commits_file(&arg(0, "STORE")?)? {
                println!("{c}");
            }
            Ok(())
        }
        "reclaim" => {
            let file = arg(0, "FILE.turndb")?;
            let stats = turndb::container::reclaim(&file)?;
            if stats.reclaimed == 0 {
                println!(
                    "nothing to reclaim ({} members, {} bytes)",
                    stats.members, stats.bytes_after
                );
            } else {
                println!(
                    "reclaimed {} bytes ({} -> {}), {} members carried across",
                    stats.reclaimed, stats.bytes_before, stats.bytes_after, stats.members
                );
            }
            Ok(())
        }

        "erase" => erase(&arg(0, "STORE")?, &rest[1..]),
        "import" => {
            // The single-file shape's whole point: open a .turndb, write into it, close it, and
            // still have one file. Created if absent, appended to if not.
            let file = arg(0, "STORE")?;
            let source = arg(1, "JSONL")?;
            let mut s = open_writer(&file)?;
            let imported = import_into(&mut s, &source)?;
            s.close()?;
            println!("{imported} records into {}", file.display());
            Ok(())
        }
        "backup" => {
            let mut s = open_writer(&arg(0, "STORE")?)?;
            let out = arg(1, "OUT")?;
            let st = s.backup(&out)?;
            let authority = if st.commit == 0 {
                "canonical origin".to_string()
            } else {
                format!("manifest revision {}", st.commit)
            };
            println!(
                "backed up {authority} into {}: {} members, {} bytes",
                out.display(),
                st.members,
                st.bytes
            );
            s.close()?;
            Ok(())
        }
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            Ok(())
        }
        // The crate version compiled in, not a string that could drift from it (#97). A bug report
        // that says "turndb 0.1.6" is worth more than "whatever npm gave me".
        "version" | "--version" | "-V" => {
            println!("turndb {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => bail!("unknown verb {other:?}\n\n{USAGE}"),
    }
}

fn inspect(path: &Path) -> Result<()> {
    // Transient names first, and without opening anything: the same recognizer a writer open
    // runs, read-only — so debris beside an absent store is still listed.
    let debris = turndb::store::debris_report(path)?;
    if !debris.entries.is_empty() {
        println!(
            "debris: {} transient file(s) beside {}; a writer open removes them when the store \
             is present, and refuses to create a store over a pending publish or reclaim \
             material when it is absent",
            debris.entries.len(),
            path.display()
        );
        for e in &debris.entries {
            println!("  {}  {:?}", e.path.display(), e.kind);
        }
    }
    let rs = open_read(path)?;
    let m = rs.manifest();
    let c = turndb::container::Container::open(path)?;
    println!("store: {}", path.display());
    if m.commit == 0 {
        println!("authority: canonical origin");
    } else {
        println!(
            "manifest revision: {}, next_seq {}, fold generation {}, tail (seg {}, off {})",
            m.commit, m.next_seq, m.fold_gen, m.fold_seg, m.fold_off
        );
    }
    println!("parts: {}", m.parts.len());
    for p in &m.parts {
        println!("  {}  seq [{}, {}]  {} records", p.member, p.seq_lo, p.seq_hi, p.records);
    }
    let ids = rs.ids()?;
    println!("records in current read view: {}", ids.len());
    println!(
        "members: {}, container state sequence {}, {} member bytes, {} free bytes",
        c.len(),
        c.seq(),
        c.member_bytes(),
        c.free_bytes()
    );
    let snaps = turndb::store::retained_commits_file(path)?;
    if !snaps.is_empty() {
        println!(
            "retained manifest revisions: {}",
            snaps.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ")
        );
    }
    Ok(())
}

fn verify(path: &Path, deep: bool) -> Result<()> {
    // Structural first: every member checksum the file records.
    let rs = open_read(path)?;
    {
        let c = turndb::container::Container::open(path)?;
        let n = c.verify().context("member verification failed")?;
        println!("members: {n} pass their checksums");
    }
    let mut sections = 0usize;
    for p in rs.parts() {
        sections += p.verify_sections()?;
    }
    println!("parts: {} sections pass their checksums", sections);
    // The chain: prev-links across the retained window and every part pin hashed against the
    // extents the file actually holds. A backup carries no retained manifest revisions, so its chain
    // is the current manifest revision's pins alone — and those are checked either way.
    let chain = turndb::store::verify_chain_file(path).context("chain verification failed")?;
    let mut pins = 0usize;
    {
        let c = turndb::container::Container::open(path)?;
        for p in &rs.manifest().parts {
            let extent = c
                .extent(&p.member)
                .ok_or_else(|| anyhow::anyhow!("the store does not hold {}", p.member))?;
            if digest_reader(&extent)? != p.b3 {
                bail!("part {} drifted from its manifest pin", p.member);
            }
            pins += 1;
        }
    }
    println!(
        "chain: {} retained links, {} retained pins, {pins} live pins verified",
        chain.links, chain.part_digests
    );
    if deep {
        // The strongest check the format offers: reconstruct every record visible in this read view, which verifies
        // every referenced piece against its BLAKE3 identity.
        let ids = rs.ids()?;
        let mut bytes = 0u64;
        for id in &ids {
            if let Some(b) = rs.reconstruct(id)? {
                bytes += b.len() as u64;
            }
        }
        println!(
            "deep: {} records reconstruct byte-exact ({bytes} content bytes verified)",
            ids.len()
        );
        // ... and the fold scrub covers what reconstruction cannot: blocks holding only retained
        // or unreferenced pieces.
        let fs = rs.fold().scrub().context("fold scrub failed")?;
        println!(
            "fold: {} blocks across {} segments verify ({} bytes){}",
            fs.blocks,
            fs.segments,
            fs.bytes,
            if fs.trailing_uncommitted > 0 {
                format!(
                    "; {} uncommitted trailing bytes await the next writer open",
                    fs.trailing_uncommitted
                )
            } else {
                String::new()
            }
        );
    }
    println!("ok");
    Ok(())
}

/// The erase verb: resolve the request to ids, run [`Store::erase_ids`], and leave behind an
/// erasure RECORD — canonical JSON documenting a process faithfully executed, digested so a copy
/// can be certified, worded to claim exactly what is true and nothing more. Signing is the
/// operator's PKI's job (`ssh-keygen -Y sign` works on any file); building a signer in would add
/// a crypto dependency to buy nothing a detached signature does not already provide.
fn erase(store: &Path, args: &[&str]) -> Result<()> {
    // ---- parse the request ----
    let mut ids: Vec<String> = Vec::new();
    let mut attr: Option<(String, String)> = None;
    let mut include_ids = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match *a {
            "--id" => ids
                .push(it.next().ok_or_else(|| anyhow::anyhow!("--id needs a value"))?.to_string()),
            "--attr" => {
                let kv = it.next().ok_or_else(|| anyhow::anyhow!("--attr needs KEY=VALUE"))?;
                let (k, v) =
                    kv.split_once('=').ok_or_else(|| anyhow::anyhow!("--attr needs KEY=VALUE"))?;
                attr = Some((k.to_string(), v.to_string()));
            }
            "--include-ids" => include_ids = true,
            other => bail!("unknown erase argument {other:?}"),
        }
    }
    if ids.is_empty() == attr.is_none() {
        bail!("erase needs exactly one of --id ... or --attr KEY=VALUE\n\n{USAGE}");
    }

    // The audit line hashes the MANIFEST member — the store's committed identity — read through
    // the container so the same line works before the writer role is taken and after it is gone.
    let manifest_hex = |path: &Path| -> Result<String> {
        let c = turndb::container::Container::open(path)?;
        let bytes = c.read_file_bounded("MANIFEST", turndb::store::MAX_MANIFEST_BYTES)?;
        Ok(blake3::hash(&bytes).to_hex().to_string())
    };
    let pre_manifest = manifest_hex(store)?;

    let mut s = open_writer(store)?;
    // ---- resolve an attribute request against the current manifest revision ----
    if let Some((k, v)) = &attr {
        for id in s.ids()? {
            let Some(rec) = s.get(&id)? else { continue };
            let hit = rec.attrs.iter().any(|(key, val)| {
                key == k
                    && match val {
                        turndb::AttrValue::Str(x) => x == v,
                        turndb::AttrValue::Int(x) => v.parse::<i64>() == Ok(*x),
                        turndb::AttrValue::Float(x) => {
                            v.parse::<f64>().is_ok_and(|p| p.to_bits() == x.to_bits())
                        }
                        turndb::AttrValue::Bool(x) => v.parse::<bool>() == Ok(*x),
                        turndb::AttrValue::UInt(x) => v.parse::<u64>() == Ok(*x),
                        turndb::AttrValue::TimestampNs(x) => v.parse::<i64>() == Ok(*x),
                        turndb::AttrValue::Null => v == "null",
                        // The string-only CLI selector has no binary literal syntax. Use the Rust
                        // or native structured API when exact binary selection is required.
                        turndb::AttrValue::Bytes(_) => false,
                    }
            });
            if hit {
                ids.push(id);
            }
        }
    }
    ids.sort();
    ids.dedup();

    // The resolved set is digested rather than listed by default: whatever captures this output
    // outlives the data, and re-stating the erased identifiers would re-leak what was erased.
    let mut h = blake3::Hasher::new();
    for id in &ids {
        h.update(id.as_bytes());
        h.update(&[0]);
    }
    let resolved_digest = h.finalize().to_hex().to_string();

    let stats = s.erase_ids(&ids)?;
    // Close, not drop: close leaves the store settled and removes the WAL sidecar (#122). The manifest digest below
    // is read after the store is fully released either way.
    s.close()?;
    let post_manifest = manifest_hex(store)?;

    println!(
        "erased {} of {} requested ({} already absent)",
        stats.tombstoned, stats.requested, stats.absent
    );
    if let Some(r) = stats.refold {
        println!(
            "  dropped {} pieces, reclaimed {} bytes; parts rebuilt, retained revisions purged",
            r.pieces_dropped,
            r.bytes_reclaimed()
        );
    }
    println!("  manifest blake3: {pre_manifest} -> {post_manifest}");
    println!("  resolved-set blake3: {resolved_digest}");
    if include_ids {
        for id in &ids {
            println!("  erased: {id}");
        }
    }
    Ok(())
}

/// JSONL in, records out: `body` is the record body (carved by the default opinion), every other
/// scalar field is an attribute, and `id` (or trace_id:span_id#kind, or a line counter) names it.
/// Batched per 1000 lines — each batch replays all-or-nothing — and flushed at the end.
/// The ingest itself, over the container store the caller opened.
fn import_into(s: &mut Store, src: &Path) -> Result<u64> {
    use std::io::BufRead;
    let reader: Box<dyn std::io::Read> = if src.as_os_str() == "-" {
        Box::new(std::io::stdin().lock())
    } else {
        Box::new(std::fs::File::open(src).with_context(|| format!("open {}", src.display()))?)
    };
    let reader = std::io::BufReader::with_capacity(1 << 22, reader);
    let mut pending: Vec<PendingRecord> = Vec::new();
    let mut est_bytes = 0u64;
    let (mut n, mut applied, mut skipped, mut refused_oversize, mut logical) =
        (0u64, 0u64, 0u64, 0u64, 0u64);
    let t = std::time::Instant::now();
    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            skipped += 1;
            continue;
        };
        let Some(body) = v.get("body").and_then(|b| b.as_str()) else {
            skipped += 1;
            continue;
        };
        let id = match (
            v.get("id").and_then(|x| x.as_str()),
            v.get("trace_id").and_then(|x| x.as_str()),
        ) {
            (Some(id), _) => id.to_string(),
            (None, Some(t)) => format!(
                "{t}:{}#{}",
                v.get("span_id").and_then(|x| x.as_str()).unwrap_or("s"),
                v.get("kind").and_then(|x| x.as_str()).unwrap_or("k")
            ),
            (None, None) => format!("import:{n:09}"),
        };
        let mut attrs = Vec::new();
        if let Some(obj) = v.as_object() {
            for (k, val) in obj {
                if k == "body" {
                    continue;
                }
                let av = match val {
                    serde_json::Value::String(x) => turndb::AttrValue::Str(x.clone()),
                    serde_json::Value::Bool(x) => turndb::AttrValue::Bool(*x),
                    serde_json::Value::Number(x) if x.is_i64() => {
                        turndb::AttrValue::Int(x.as_i64().unwrap())
                    }
                    serde_json::Value::Number(x) => {
                        turndb::AttrValue::Float(x.as_f64().unwrap_or(0.0))
                    }
                    _ => continue,
                };
                attrs.push((k.clone(), av));
            }
        }
        logical += body.len() as u64;
        // A conservative per-record charge against the atomic-batch admission ceiling: body and
        // attribute bytes plus generous framing slack. Batching here is a throughput vehicle, not
        // a semantic transaction — each input line is independent — so the batch closes before
        // its worst case could be refused, instead of importing real traces up to an arbitrary
        // record count and aborting at the engine's (correct) refusal.
        let record_est = (id.len() + body.len()) as u64
            + attrs
                .iter()
                .map(|(k, v)| {
                    k.len() as u64
                        + match v {
                            turndb::AttrValue::Str(s) => s.len() as u64,
                            turndb::AttrValue::Bytes(b) => b.len() as u64,
                            _ => 8,
                        }
                })
                .sum::<u64>()
            + 1024;
        if !pending.is_empty() && (pending.len() >= 1000 || est_bytes + record_est > EST_CEILING) {
            apply_pending(s, &mut pending, &mut applied, &mut refused_oversize)?;
            est_bytes = 0;
        }
        est_bytes += record_est;
        pending.push((id, body.as_bytes().to_vec(), attrs));
        n += 1;
    }
    if !pending.is_empty() {
        apply_pending(s, &mut pending, &mut applied, &mut refused_oversize)?;
    }
    let el = t.elapsed().as_secs_f64();
    println!(
        "imported {applied} of {n} records ({skipped} skipped, {refused_oversize} refused \
         oversize), {:.2} GiB logical, {:.1}s ({:.0} rec/s); parts: {}",
        logical as f64 / (1u64 << 30) as f64,
        el,
        applied as f64 / el.max(0.001),
        s.part_count()
    );
    Ok(applied)
}

const EST_CEILING: u64 = 128 << 20;

type PendingRecord = (String, Vec<u8>, Vec<(String, turndb::AttrValue)>);

/// Apply the pending records as one atomic batch; on an admission refusal, retry them singly so
/// one oversized record costs itself rather than the import. Only RESOURCE_EXHAUSTED downgrades
/// to a counted per-record refusal — any other failure aborts, because a disk or corruption error
/// mid-import must stop the run, not be skipped past.
fn apply_pending(
    s: &mut Store,
    pending: &mut Vec<PendingRecord>,
    applied: &mut u64,
    refused_oversize: &mut u64,
) -> Result<()> {
    let mut batch = turndb::store::Batch::new();
    for (id, body, attrs) in pending.iter() {
        batch.put_body(id, body, attrs.clone());
    }
    match s.apply(batch) {
        Ok(()) => *applied += pending.len() as u64,
        Err(error)
            if turndb::error::classify(&error) == turndb::error::ErrorClass::ResourceExhausted =>
        {
            for (id, body, attrs) in pending.drain(..) {
                let mut single = turndb::store::Batch::new();
                let body_len = body.len();
                single.put_body(&id, &body, attrs);
                match s.apply(single) {
                    Ok(()) => *applied += 1,
                    Err(error)
                        if turndb::error::classify(&error)
                            == turndb::error::ErrorClass::ResourceExhausted =>
                    {
                        eprintln!(
                            "refused oversize record ({body_len} body bytes) under the \
                             configured write admission limits"
                        );
                        *refused_oversize += 1;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        Err(error) => return Err(error),
    }
    pending.clear();
    s.sync()?;
    s.flush()?;
    Ok(())
}

#[cfg(feature = "sql")]
fn query(path: &Path, sql: Option<&str>) -> Result<()> {
    let sql = sql.ok_or_else(|| anyhow::anyhow!("missing SQL\n\n{USAGE}"))?;
    let rs = open_read(path)?;
    let (ctx, table) = turndb::query::table::TurndbTable::context(rs, "t")?;
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let batches =
        rt.block_on(async { ctx.sql(sql).await?.collect().await.map_err(anyhow::Error::from) })?;
    println!("{}", datafusion::arrow::util::pretty::pretty_format_batches(&batches)?);
    let st = table.stats();
    eprintln!("({} rows scanned, {} fold reads)", st.rows, st.fold_reads);
    Ok(())
}

#[cfg(not(feature = "sql"))]
fn query(_: &Path, _: Option<&str>) -> Result<()> {
    bail!("this build has no SQL lens (the `sql` feature is off)");
}
