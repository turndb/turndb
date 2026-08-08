//! The `turndb` CLI — the auditor's face of the format.
//!
//! A store you can inspect, verify, query, pack, and recover from a command line with no server
//! running is a store an auditor can trust. Every read verb takes either a store directory or a
//! pack file and treats them identically, because the readers underneath do.
//!
//! Argument parsing is by hand, on purpose: the crate's dependency discipline does not bend for
//! flag ergonomics.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use turndb::fold::FoldCfg;
use turndb::store::{ReadStore, SingleFileKind, Store};

const USAGE: &str = "\
turndb — content-addressed columnar store for AI traces

usage: turndb <verb> [args]

  reading (STORE may be a store directory, a sealed pack, or a .turndb container):
    inspect   <STORE>            what is inside: manifest, parts, fold, snapshots
    ids       <STORE>            every live record id, one per line
    get       <STORE> <ID>       reconstruct one record's content to stdout, byte-exact
    verify    <STORE> [--deep]   integrity: structural checksums; --deep reconstructs everything
    query     <STORE> <SQL>      run SQL over the store (table name: t)

  operating (a store directory, writer role taken):
    compact   <DIR>              merge every live part into one
    refold    <DIR>              rewrite the fold, dropping content no live record references
    punch     <DIR>              deallocate unreachable fold blocks IN PLACE — the cheap half of
                                 erasure; no offsets move, no parts are rebuilt
    recover   <DIR> [--max-rollback N]
                                 validate and promote a retained manifest; rollback defaults to 0
    snapshots <DIR>              list retained commits available to time travel
    erase     <DIR> (--id ID ... | --attr KEY=VALUE)
                                 tombstone, settle, and REWRITE until this store no longer
                                 references the content or metadata. Does not promise media-byte
                                 non-recoverability; prints the measurable result

  ingesting:
    import    <DIR> <JSONL>      ingest records ({\"body\": ..., attrs...} per line; - for stdin),
                                 carved by the engine's default opinion, batched per 1000

  shipping:
    pack      <DIR> <OUT>        the committed snapshot as one SEALED file
    unpack    <PACK> <OUTDIR>    extract back into an ordinary store directory
    checkpoint <DIR> <OUT.turndb>
                                 the committed snapshot as one GROWABLE file, created or grown in
                                 place; re-running it re-ingests only what changed
    write     <FILE.turndb> <JSONL>
                                 ingest INTO a single file, creating it if absent. Working state
                                 lives beside it in FILE.turndb-hot while open and is folded back
                                 in on close, so what remains is the one file
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

/// A store is a directory; a single file is a pack or a container. The discrimination is the
/// library's — [`turndb::store::single_file_kind`] — so the CLI and both bindings agree.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Directory,
    Pack,
    Container,
}

fn classify(path: &Path) -> Source {
    match turndb::store::single_file_kind(path) {
        Some(SingleFileKind::Container) => Source::Container,
        Some(SingleFileKind::Pack) => Source::Pack,
        // A regular file with no recognised magic is still reported as a pack so the pack opener
        // produces its own specific refusal rather than a vaguer one from here.
        None if path.is_file() => Source::Pack,
        None => Source::Directory,
    }
}

/// Digests one named member of whichever single-file form the path turned out to be.
///
/// A digest rather than the bytes: a part is the largest thing a store holds, and reading one
/// whole only to hash it is an unbounded allocation for no gain. The reader streams instead.
type MemberDigest = Box<dyn Fn(&str) -> Result<String>>;

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
    match classify(path) {
        Source::Directory => Store::open_read(path, FoldCfg::default()),
        Source::Pack => turndb::store::open_read_pack(path, FoldCfg::default()),
        Source::Container => turndb::store::open_read_container(path, FoldCfg::default()),
    }
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
            let mut s = Store::open(&arg(0, "DIR")?, FoldCfg::default())?;
            let n = s.part_count();
            match s.merge_range(0, n)? {
                Some(st) => {
                    println!(
                        "merged {} parts, {} records in, {} out ({} superseded, {} tombstones dropped)",
                        st.inputs, st.records_in, st.records_out, st.superseded, st.tombstones_dropped
                    );
                    Ok(())
                }
                None => {
                    println!("nothing to merge ({n} part{})", if n == 1 { "" } else { "s" });
                    Ok(())
                }
            }
        }
        "punch" => {
            let mut s = Store::open(&arg(0, "DIR")?, FoldCfg::default())?;
            s.flush()?;
            let st = s.punch_unreferenced()?;
            println!(
                "punched {} of {} unreachable blocks (the manifest names them; metadata residue \
                 remains in parts until a refold)",
                st.blocks_punched, st.blocks_examined
            );
            Ok(())
        }
        "refold" => {
            let mut s = Store::open(&arg(0, "DIR")?, FoldCfg::default())?;
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
            let report = turndb::store::recover_manifest(
                &arg(0, "DIR")?,
                FoldCfg::default(),
                turndb::store::RecoveryOptions { max_rollback_commits },
            )?;
            println!(
                "promoted retained commit {} to MANIFEST (rollback {}, {} records, {} content values verified)",
                report.commit, report.rollback_commits, report.records, report.content_values
            );
            Ok(())
        }
        "snapshots" => {
            for c in turndb::store::retained_commits(&arg(0, "DIR")?)? {
                println!("{c}");
            }
            Ok(())
        }
        "checkpoint" => {
            let stats =
                turndb::store::checkpoint_into_container(&arg(0, "DIR")?, &arg(1, "OUT.turndb")?)?;
            println!(
                "commit {}: {} members, {} bytes ingested, {} unchanged, {} superseded",
                stats.commit_seq,
                stats.members,
                stats.ingested_bytes,
                stats.skipped_members,
                stats.free_bytes
            );
            Ok(())
        }
        "erase" => erase(&arg(0, "DIR")?, &rest[1..]),
        "write" => {
            // The single-file shape's whole point: open a .turndb, write to it, close it, and
            // still have a file. Everything between is an ordinary store.
            let file = arg(0, "FILE.turndb")?;
            let source = arg(1, "JSONL")?;
            let mut cs = turndb::store::ContainerStore::open(&file, FoldCfg::default())?;
            let imported = import_into(cs.store(), &source)?;
            let stats = cs.close()?;
            println!(
                "{imported} records; container commit {} holds {} members ({} bytes ingested)",
                stats.commit_seq, stats.members, stats.ingested_bytes
            );
            Ok(())
        }
        "import" => {
            let dir = arg(0, "DIR")?;
            let src = arg(1, "JSONL")?;
            import(&dir, &src)
        }
        "pack" => {
            let st = turndb::pack::write(&arg(0, "DIR")?, &arg(1, "OUT")?)?;
            println!("packed {} files, {} bytes", st.files, st.bytes);
            Ok(())
        }
        "unpack" => {
            let n = turndb::pack::unpack(&arg(0, "PACK")?, &arg(1, "OUTDIR")?)?;
            println!("extracted {n} files");
            Ok(())
        }
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            Ok(())
        }
        other => bail!("unknown verb {other:?}\n\n{USAGE}"),
    }
}

fn inspect(path: &Path) -> Result<()> {
    let rs = open_read(path)?;
    let m = rs.manifest();
    let source = classify(path);
    let kind = match source {
        Source::Directory => "store",
        Source::Pack => "pack",
        Source::Container => "container",
    };
    println!("{kind}: {}", path.display());
    println!(
        "manifest: commit {}, next_seq {}, fold generation {}, tail (seg {}, off {})",
        m.commit, m.next_seq, m.fold_gen, m.fold_seg, m.fold_off
    );
    println!("parts: {}", m.parts.len());
    for p in &m.parts {
        println!("  {}  seq [{}, {}]  {} records", p.file, p.seq_lo, p.seq_hi, p.records);
    }
    let ids = rs.ids()?;
    println!("live records: {}", ids.len());
    match source {
        Source::Directory => {
            let snaps = turndb::store::retained_commits(path)?;
            if !snaps.is_empty() {
                println!(
                    "snapshots: {}",
                    snaps.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(", ")
                );
            }
        }
        Source::Pack => {
            let pk = turndb::pack::Pack::open(path)?;
            println!("pack files: {}", pk.names().count());
        }
        Source::Container => {
            let c = turndb::container::Container::open(path)?;
            println!(
                "container members: {}, commit {}, {} member bytes, {} superseded",
                c.len(),
                c.seq(),
                c.member_bytes(),
                c.free_bytes()
            );
        }
    }
    Ok(())
}

fn verify(path: &Path, deep: bool) -> Result<()> {
    // Structural first: every checksum that exists gets checked.
    match classify(path) {
        Source::Pack => {
            let pk = turndb::pack::Pack::open(path)?;
            let n = pk.verify().context("pack verification failed")?;
            println!("pack: {n} files pass their checksums");
        }
        Source::Container => {
            let c = turndb::container::Container::open(path)?;
            let n = c.verify().context("container verification failed")?;
            println!("container: {n} members pass their checksums");
        }
        Source::Directory => {}
    }
    let rs = open_read(path)?;
    let mut sections = 0usize;
    for p in rs.parts() {
        sections += p.verify_sections()?;
    }
    println!("parts: {} sections pass their checksums", sections);
    if path.is_dir() {
        let chain = turndb::store::verify_chain(path).context("chain verification failed")?;
        println!(
            "chain: {} links and {} part pins verified{}",
            chain.links,
            chain.part_digests,
            if chain.undigested > 0 {
                format!(" ({} parts predate digests)", chain.undigested)
            } else {
                String::new()
            }
        );
    } else {
        // A single-file store carries its manifest verbatim, so there is no retained chain to walk;
        // what can be checked is each part pin against the extent the file actually holds.
        let (kind, digest_member): (&str, MemberDigest) = match classify(path) {
            Source::Container => {
                let c = turndb::container::Container::open(path)?;
                let digest = move |name: &str| {
                    let extent = c
                        .extent(name)
                        .ok_or_else(|| anyhow::anyhow!("container does not hold {name}"))?;
                    digest_reader(&extent)
                };
                ("container", Box::new(digest))
            }
            _ => {
                let pk = turndb::pack::Pack::open(path)?;
                let digest = move |name: &str| {
                    let extent = pk
                        .file(name)
                        .ok_or_else(|| anyhow::anyhow!("pack does not hold {name}"))?;
                    digest_reader(&extent)
                };
                ("pack", Box::new(digest))
            }
        };
        let mut pins = 0usize;
        for p in &rs.manifest().parts {
            if let Some(want) = &p.b3 {
                let got = digest_member(&p.file)?;
                if *want != got {
                    bail!("{kind} part {} drifted from its manifest pin", p.file);
                }
                pins += 1;
            }
        }
        println!("chain: {pins} part pins verified inside the {kind}");
    }
    if deep {
        // The strongest check the format offers: reconstruct every live record, which verifies
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
fn erase(dir: &Path, args: &[&str]) -> Result<()> {
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

    let pre_manifest = blake3::hash(&std::fs::read(dir.join("MANIFEST"))?).to_hex().to_string();

    let mut s = Store::open(dir, FoldCfg::default())?;
    // ---- resolve an attribute request against the committed state ----
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
    drop(s);
    let post_manifest = blake3::hash(&std::fs::read(dir.join("MANIFEST"))?).to_hex().to_string();

    println!(
        "erased {} of {} requested ({} already absent)",
        stats.tombstoned, stats.requested, stats.absent
    );
    if let Some(r) = stats.refold {
        println!(
            "  dropped {} pieces, reclaimed {} bytes; parts rebuilt, snapshots purged",
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
fn import(dir: &Path, src: &Path) -> Result<()> {
    let mut s = Store::open(dir, FoldCfg::default())?;
    import_into(&mut s, src)?;
    Ok(())
}

/// The ingest itself, over a store someone else opened — a directory writer or a container's.
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
