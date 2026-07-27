//! usage: turnd <store-dir> [listen-addr]
//!
//! Defaults to 127.0.0.1:4318 — OTLP/HTTP's standard port. Kill it however you like: a 200 means
//! synced, and recovery is the store's simulation-tested job, not this process's.

fn main() -> anyhow::Result<()> {
    let mut a = std::env::args().skip(1);
    let dir = std::path::PathBuf::from(a.next().expect("usage: turnd <store-dir> [listen-addr]"));
    let addr = a.next().unwrap_or_else(|| "127.0.0.1:4318".into());
    let daemon = turnd::Turnd::open(&dir)?;
    let server = tiny_http::Server::http(&addr)
        .map_err(|e| anyhow::anyhow!("bind {addr}: {e}"))?;
    eprintln!("turnd: store {} listening on {addr} (POST /v1/traces)", dir.display());
    daemon.serve(server)
}
