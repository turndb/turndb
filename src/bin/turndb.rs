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
use turndb::store::{ReadStore, Store};

const USAGE: &str = "\
turndb — content-addressed columnar store for AI traces

usage: turndb <verb> [args]

  reading (STORE may be a store directory or a .turndb pack file):
    inspect   <STORE>            what is inside: manifest, parts, fold, snapshots
    ids       <STORE>            every live record id, one per line
    get       <STORE> <ID>       reconstruct one record's content to stdout, byte-exact
    verify    <STORE> [--deep]   integrity: structural checksums; --deep reconstructs everything
    query     <STORE> <SQL>      run SQL over the store (table name: t)

  operating (a store directory, writer role taken):
    compact   <DIR>              merge every live part into one
    refold    <DIR>              rewrite the fold, dropping content no live record references
    recover   <DIR>              promote the newest intact retained manifest over a damaged one
    snapshots <DIR>              list retained commits available to time travel

  shipping:
    pack      <DIR> <OUT>        the committed snapshot as one file
    unpack    <PACK> <OUTDIR>    extract back into an ordinary store directory
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args) {
        eprintln!("turndb: {e:#}");
        std::process::exit(1);
    }
}

fn open_read(path: &Path) -> Result<ReadStore> {
    if path.is_file() {
        turndb::store::open_read_pack(path, FoldCfg::default())
    } else {
        Store::open_read(path, FoldCfg::default())
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
            let c = turndb::store::recover_manifest(&arg(0, "DIR")?)?;
            println!("promoted retained commit {c} to MANIFEST");
            Ok(())
        }
        "snapshots" => {
            for c in turndb::store::retained_commits(&arg(0, "DIR")?) {
                println!("{c}");
            }
            Ok(())
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
    let kind = if path.is_file() { "pack" } else { "store" };
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
    if !path.is_file() {
        let snaps = turndb::store::retained_commits(path);
        if !snaps.is_empty() {
            println!(
                "snapshots: {}",
                snaps.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(", ")
            );
        }
    } else {
        let pk = turndb::pack::Pack::open(path)?;
        println!("pack files: {}", pk.names().count());
    }
    Ok(())
}

fn verify(path: &Path, deep: bool) -> Result<()> {
    // Structural first: every checksum that exists gets checked.
    if path.is_file() {
        let pk = turndb::pack::Pack::open(path)?;
        let n = pk.verify().context("pack verification failed")?;
        println!("pack: {n} files pass their checksums");
    }
    let rs = open_read(path)?;
    let mut sections = 0usize;
    for p in rs.parts() {
        sections += p.verify_sections()?;
    }
    println!("parts: {} sections pass their checksums", sections);
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
        println!("deep: {} records reconstruct byte-exact ({bytes} content bytes verified)", ids.len());
    }
    println!("ok");
    Ok(())
}

#[cfg(feature = "sql")]
fn query(path: &Path, sql: Option<&str>) -> Result<()> {
    let sql = sql.ok_or_else(|| anyhow::anyhow!("missing SQL\n\n{USAGE}"))?;
    let rs = open_read(path)?;
    let (ctx, table) = turndb::query::table::TurndbTable::context(rs, "t")?;
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let batches = rt.block_on(async {
        ctx.sql(sql).await?.collect().await.map_err(anyhow::Error::from)
    })?;
    println!("{}", datafusion::arrow::util::pretty::pretty_format_batches(&batches)?);
    let st = table.stats();
    eprintln!("({} rows scanned, {} fold reads)", st.rows, st.fold_reads);
    Ok(())
}

#[cfg(not(feature = "sql"))]
fn query(_: &Path, _: Option<&str>) -> Result<()> {
    bail!("this build has no SQL lens (the `sql` feature is off)");
}
