//! Compile-check for the README's library example. Kept so the front page cannot rot silently.
//! Runs against a temp path rather than the repository root, which the front page's reader will
//! forgive: the shape of the calls is the claim, not the path.
use turndb::{fold::FoldCfg, store::Store};

fn main() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join(format!("turndb-readme-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("mystore.turndb");
    let body = b"hello";
    let mut s = Store::open_file(&path, FoldCfg::default())?;
    s.put_body("trace:1#input", body, vec![])?; // carved by the engine's default opinion
    s.sync()?; // the ACK point — durable from here
    s.flush()?; // one superblock flip publishes the flush
    s.close()?; // settles the sidecar; one file remains
    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}
