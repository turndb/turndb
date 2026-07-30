//! The other interop direction: open a store a NATIVE build wrote and verify it byte-exact.
//!
//! Migration needs this. A backfill runs fastest as the native binary, but the process serving
//! reads is Node — so a store written by one must be wholly readable by the other, including its
//! zstd frames, which the two builds compress with different implementations of the same format.
use anyhow::Result;
use turndb::fold::FoldCfg;
use turndb::store::Store;

fn main() -> Result<()> {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "/store".into());
    let s = Store::open_read(std::path::Path::new(&dir), FoldCfg::default())?;

    let ids = s.ids()?;
    let mut bytes = 0usize;
    for id in &ids {
        let body = s.reconstruct(id)?.ok_or_else(|| anyhow::anyhow!("{id} missing"))?;
        // The store carries its own answer: every piece is checked against its BLAKE3 on read, so
        // reaching here at all means the content matched what was written.
        bytes += body.len();
    }
    // And the paged range read, which is what the binding will actually serve.
    let page = s.scan_ids(None, None, 5, false)?;
    println!(
        "OK  read {} records / {} content bytes written by the native build; first page {:?}",
        ids.len(),
        bytes,
        page.first()
    );
    Ok(())
}
