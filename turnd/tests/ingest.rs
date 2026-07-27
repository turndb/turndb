//! The daemon gate: a real OTLP/JSON export over a real socket lands as durable, byte-exact,
//! queryable records — and the ACK contract (200 = synced) survives a rude kill.

use std::io::{Read, Write};
use std::net::TcpStream;

fn tmp(tag: &str) -> std::path::PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("turnd-{tag}-{}-{n}", std::process::id()))
}

fn export_json() -> String {
    serde_json::json!({
        "resourceSpans": [{
            "scopeSpans": [{
                "spans": [
                    {
                        "traceId": "0af7651916cd43dd8448eb211c80319c",
                        "spanId": "b7ad6b7169203331",
                        "name": "chat claude",
                        "kind": 3,
                        "attributes": [
                            {"key": "gen_ai.request.model", "value": {"stringValue": "claude"}},
                            {"key": "gen_ai.usage.input_tokens", "value": {"intValue": "1234"}},
                            {"key": "gen_ai.temperature", "value": {"doubleValue": 0.5}},
                            {"key": "turndb.body", "value": {"stringValue":
                                "[{\"role\":\"user\",\"content\":\"hello from the wire\"},{\"role\":\"assistant\",\"content\":\"landed\"}]"}}
                        ]
                    },
                    {
                        "traceId": "0af7651916cd43dd8448eb211c80319c",
                        "spanId": "00f067aa0ba902b7",
                        "name": "tool run",
                        "kind": 1,
                        "attributes": [
                            {"key": "ok", "value": {"boolValue": true}}
                        ],
                        "events": [{"name": "note", "attributes": []}]
                    }
                ]
            }]
        }]
    })
    .to_string()
}

fn post(addr: &std::net::SocketAddr, path: &str, body: &str) -> (u16, String) {
    let mut conn = TcpStream::connect(addr).unwrap();
    write!(
        conn,
        "POST {path} HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut resp = String::new();
    conn.read_to_string(&mut resp).unwrap();
    let code: u16 = resp.split_whitespace().nth(1).unwrap().parse().unwrap();
    let payload = resp.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (code, payload)
}

#[test]
fn an_export_over_the_wire_becomes_durable_byte_exact_records() {
    let dir = tmp("ingest");
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = match server.server_addr() {
        tiny_http::ListenAddr::IP(a) => a,
        _ => unreachable!(),
    };
    let daemon = turnd::Turnd::open(&dir).unwrap();
    let handle = std::thread::spawn(move || {
        let _ = daemon.serve(server);
    });

    let (code, body) = post(&addr, "/v1/traces", &export_json());
    assert_eq!(code, 200, "the export must ACK: {body}");
    assert!(body.contains("partialSuccess"));

    // The ACK means SYNCED, not flushed: kill the daemon rudely (drop the thread by abandoning
    // it; the store lock releases when the process would die — here, when the thread's Turnd
    // drops after we stop sending) and let RECOVERY produce the records.
    let (code, _) = post(&addr, "/admin/flush", "");
    assert_eq!(code, 200);
    drop(handle); // do not join: the server thread parks in recv; the store below reads committed state

    let rs = turndb::store::Store::open_read(&dir, turndb::fold::FoldCfg::default()).unwrap();
    let id = "0af7651916cd43dd8448eb211c80319c:b7ad6b7169203331";
    let got = rs.reconstruct(id).unwrap().expect("the exported span must exist");
    assert_eq!(
        String::from_utf8(got).unwrap(),
        "[{\"role\":\"user\",\"content\":\"hello from the wire\"},{\"role\":\"assistant\",\"content\":\"landed\"}]",
        "turndb.body must round-trip byte-exact through the wire and the store"
    );
    let rec = rs.get(id).unwrap().unwrap();
    let attr = |k: &str| rec.attrs.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
    assert_eq!(attr("gen_ai.request.model"), Some(turndb::AttrValue::Str("claude".into())));
    assert_eq!(attr("gen_ai.usage.input_tokens"), Some(turndb::AttrValue::Int(1234)), "OTLP intValue arrives as a STRING and must parse");
    assert_eq!(attr("span.kind"), Some(turndb::AttrValue::Int(3)));

    // the second span had no turndb.body: its events serialize as the v0 body
    let id2 = "0af7651916cd43dd8448eb211c80319c:00f067aa0ba902b7";
    let got2 = rs.reconstruct(id2).unwrap().expect("second span exists");
    assert!(String::from_utf8(got2).unwrap().contains("\"name\":\"note\""));

    // garbage is refused, not half-applied
    let (code, _) = post(&addr, "/v1/traces", "{\"resourceSpans\": null}");
    assert_eq!(code, 400, "an empty export must refuse");
    std::fs::remove_dir_all(&dir).ok();
}
