//! Compile-check for the README's library example. Kept so the front page cannot rot silently.
use turndb::{fold::FoldCfg, store::Store};

fn main() -> anyhow::Result<()> {
    let body = b"hello";
    let mut s = Store::open("mystore".as_ref(), FoldCfg::default())?;
    s.put_body("trace:1#input", body, vec![])?; // carved by the engine's default opinion
    s.sync()?; // the ACK point — durable from here
    s.flush()?; // seal into an immutable part
    Ok(())
}
