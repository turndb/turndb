//! The write-ahead log — the ACK point, and the only thing standing between a crash and lost data.
//!
//! A frame holds the **carved result**: the record's id, its body program, and its attributes. Never
//! the raw input, because replaying raw input would re-run the carve and make recovery depend on
//! whichever carve logic happened to be compiled in. Carved frames replay exactly, forever.
//!
//! Piece bytes ride along **only for dedup misses** — content that was new when it was written. A
//! hit's content was already durable (it is either below the last committed fold tail, or it is
//! carried by an earlier frame in this same log, which replay reaches first). That bounds the log to
//! distinct content rather than logical volume: on a corpus with 38x duplication, ~40x smaller.
//!
//! ```text
//! frame:  tag(1) | seq(8) | len(4) | payload(len) | crc32(4)
//! ```

use crate::part::idcol::{get_varint, put_varint};
use crate::types::{AttrValue, BodyOp, PieceHash, Record};
use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

pub const FRAME_TAG: u8 = 0x57;
/// A DELETION. Its payload is the id alone — a tombstone has no body, no attributes and no content, so
/// it costs a frame and nothing else.
pub const TOMB_TAG: u8 = 0x58;
const HDR: usize = 13; // tag + seq + len
const CRC: usize = 4;

/// A record plus the bytes of any piece that was new when it was written.
pub struct Frame {
    /// True when this frame deletes `record.id`. The record's body and attributes are empty.
    pub tomb: bool,
    pub seq: u64,
    pub record: Record,
    /// `(hash, bytes)` for pieces this frame introduced.
    pub novel: Vec<(PieceHash, Vec<u8>)>,
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

fn get_bytes<'a>(b: &'a [u8], at: &mut usize) -> Result<&'a [u8]> {
    let n = get_varint(b, at)? as usize;
    if *at + n > b.len() {
        bail!("wal: byte string runs past the frame");
    }
    let s = &b[*at..*at + n];
    *at += n;
    Ok(s)
}

pub fn encode_record(out: &mut Vec<u8>, r: &Record, novel: &[(PieceHash, Vec<u8>)]) {
    put_bytes(out, r.id.as_bytes());
    put_varint(out, r.body.len() as u64);
    for op in &r.body {
        match op {
            BodyOp::Lit(b) => {
                out.push(0);
                put_bytes(out, b);
            }
            BodyOp::Piece { hash, len } => {
                out.push(1);
                out.extend_from_slice(&hash.0);
                put_varint(out, *len as u64);
            }
        }
    }
    put_varint(out, r.attrs.len() as u64);
    for (k, v) in &r.attrs {
        put_bytes(out, k.as_bytes());
        out.push(v.type_tag());
        match v {
            AttrValue::Str(s) => put_bytes(out, s.as_bytes()),
            AttrValue::Int(i) => out.extend_from_slice(&i.to_le_bytes()),
            // bit pattern, not value — NaN payloads and -0.0 must replay exactly
            AttrValue::Float(f) => out.extend_from_slice(&f.to_bits().to_le_bytes()),
            AttrValue::Bool(b) => out.push(u8::from(*b)),
        }
    }
    put_varint(out, novel.len() as u64);
    for (h, bytes) in novel {
        out.extend_from_slice(&h.0);
        put_bytes(out, bytes);
    }
}

pub fn decode_record(b: &[u8]) -> Result<(Record, Vec<(PieceHash, Vec<u8>)>)> {
    let mut at = 0usize;
    let id = String::from_utf8(get_bytes(b, &mut at)?.to_vec())?;
    let n_ops = get_varint(b, &mut at)? as usize;
    let mut body = Vec::with_capacity(n_ops);
    for _ in 0..n_ops {
        let tag = *b.get(at).ok_or_else(|| anyhow::anyhow!("wal: truncated op"))?;
        at += 1;
        match tag {
            0 => body.push(BodyOp::Lit(get_bytes(b, &mut at)?.to_vec())),
            1 => {
                if at + 32 > b.len() {
                    bail!("wal: truncated piece hash");
                }
                let mut h = [0u8; 32];
                h.copy_from_slice(&b[at..at + 32]);
                at += 32;
                let len = get_varint(b, &mut at)? as u32;
                body.push(BodyOp::Piece { hash: PieceHash(h), len });
            }
            t => bail!("wal: unknown body op tag {t}"),
        }
    }
    let n_attrs = get_varint(b, &mut at)? as usize;
    let mut attrs = Vec::with_capacity(n_attrs);
    for _ in 0..n_attrs {
        let key = String::from_utf8(get_bytes(b, &mut at)?.to_vec())?;
        let tag = *b.get(at).ok_or_else(|| anyhow::anyhow!("wal: truncated attr"))?;
        at += 1;
        let v = match tag {
            0 => AttrValue::Str(String::from_utf8(get_bytes(b, &mut at)?.to_vec())?),
            1 => {
                let x = i64::from_le_bytes(b[at..at + 8].try_into()?);
                at += 8;
                AttrValue::Int(x)
            }
            2 => {
                let x = f64::from_bits(u64::from_le_bytes(b[at..at + 8].try_into()?));
                at += 8;
                AttrValue::Float(x)
            }
            3 => {
                let x = b[at] != 0;
                at += 1;
                AttrValue::Bool(x)
            }
            t => bail!("wal: unknown attr type tag {t}"),
        };
        attrs.push((key, v));
    }
    let n_novel = get_varint(b, &mut at)? as usize;
    let mut novel = Vec::with_capacity(n_novel);
    for _ in 0..n_novel {
        if at + 32 > b.len() {
            bail!("wal: truncated novel hash");
        }
        let mut h = [0u8; 32];
        h.copy_from_slice(&b[at..at + 32]);
        at += 32;
        novel.push((PieceHash(h), get_bytes(b, &mut at)?.to_vec()));
    }
    Ok((Record { id, body, attrs }, novel))
}

pub struct Wal {
    w: BufWriter<File>,
    path: std::path::PathBuf,
    len: u64,
    scratch: Vec<u8>,
}

impl Wal {
    pub fn open(path: &Path) -> Result<Wal> {
        let f = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)
            .with_context(|| format!("open wal {}", path.display()))?;
        let len = f.metadata()?.len();
        Ok(Wal { w: BufWriter::with_capacity(1 << 20, f), path: path.to_path_buf(), len, scratch: Vec::new() })
    }

    /// Log a deletion. Durable on the next [`Wal::sync`], exactly like a put.
    pub fn append_tomb(&mut self, seq: u64, id: &str) -> Result<()> {
        self.scratch.clear();
        self.scratch.extend_from_slice(id.as_bytes());
        let mut hdr = Vec::with_capacity(13);
        hdr.push(TOMB_TAG);
        hdr.extend_from_slice(&seq.to_le_bytes());
        hdr.extend_from_slice(&(self.scratch.len() as u32).to_le_bytes());
        // The CRC covers the HEADER as well as the payload — replay checks both, so omitting the
        // header here would make every tombstone frame read back as a torn write.
        let mut crc = crc32fast::Hasher::new();
        crc.update(&hdr);
        crc.update(&self.scratch);
        let c = crc.finalize();
        self.w.write_all(&hdr)?;
        self.w.write_all(&self.scratch)?;
        self.w.write_all(&c.to_le_bytes())?;
        self.len += (HDR + self.scratch.len() + CRC) as u64;
        Ok(())
    }

    pub fn append(&mut self, seq: u64, r: &Record, novel: &[(PieceHash, Vec<u8>)]) -> Result<()> {
        self.scratch.clear();
        encode_record(&mut self.scratch, r, novel);
        let mut hdr = Vec::with_capacity(HDR);
        hdr.push(FRAME_TAG);
        hdr.extend_from_slice(&seq.to_le_bytes());
        hdr.extend_from_slice(&(self.scratch.len() as u32).to_le_bytes());
        let mut crc = crc32fast::Hasher::new();
        crc.update(&hdr);
        crc.update(&self.scratch);
        self.w.write_all(&hdr)?;
        self.w.write_all(&self.scratch)?;
        self.w.write_all(&crc.finalize().to_le_bytes())?;
        self.len += (HDR + self.scratch.len() + CRC) as u64;
        Ok(())
    }

    /// The ACK point. Nothing may be reported durable before this returns.
    pub fn sync(&mut self) -> Result<()> {
        self.w.flush()?;
        self.w.get_ref().sync_all().context("fsync wal")?;
        Ok(())
    }

    /// Drop every frame — called once the records they cover are committed in a part.
    pub fn truncate(&mut self) -> Result<()> {
        self.w.flush()?;
        let f = OpenOptions::new().write(true).open(&self.path)?;
        f.set_len(0)?;
        f.sync_all()?;
        self.w = BufWriter::with_capacity(1 << 20, OpenOptions::new().append(true).open(&self.path)?);
        self.len = 0;
        Ok(())
    }

    pub fn bytes(&self) -> u64 {
        self.len
    }

    /// Every intact frame, in order. Stops at the first torn or corrupt one — a partial tail is the
    /// end of the log, not an error, because a crash mid-append leaves exactly that.
    pub fn replay(path: &Path) -> Result<Vec<Frame>> {
        let f = match File::open(path) {
            Ok(f) => f,
            Err(_) => return Ok(Vec::new()),
        };
        let len = f.metadata()?.len();
        let mut out = Vec::new();
        let mut off = 0u64;
        let mut hdr = [0u8; HDR];
        loop {
            if off + HDR as u64 + CRC as u64 > len || f.read_exact_at(&mut hdr, off).is_err() {
                break;
            }
            if hdr[0] != FRAME_TAG && hdr[0] != TOMB_TAG {
                break;
            }
            let seq = u64::from_le_bytes(hdr[1..9].try_into().unwrap());
            let plen = u32::from_le_bytes(hdr[9..13].try_into().unwrap()) as usize;
            let end = off + HDR as u64 + plen as u64 + CRC as u64;
            if end > len {
                break;
            }
            let mut payload = vec![0u8; plen];
            if f.read_exact_at(&mut payload, off + HDR as u64).is_err() {
                break;
            }
            let mut cb = [0u8; CRC];
            if f.read_exact_at(&mut cb, off + HDR as u64 + plen as u64).is_err() {
                break;
            }
            let mut crc = crc32fast::Hasher::new();
            crc.update(&hdr);
            crc.update(&payload);
            if crc.finalize() != u32::from_le_bytes(cb) {
                break; // torn write
            }
            if hdr[0] == TOMB_TAG {
                let Ok(id) = String::from_utf8(payload) else { break };
                out.push(Frame {
                    seq,
                    tomb: true,
                    record: Record { id, body: Vec::new(), attrs: Vec::new() },
                    novel: Vec::new(),
                });
            } else {
                match decode_record(&payload) {
                    Ok((record, novel)) => out.push(Frame { seq, tomb: false, record, novel }),
                    Err(_) => break,
                }
            }
            off = end;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> (Record, Vec<(PieceHash, Vec<u8>)>) {
        let bytes = b"a novel piece".to_vec();
        let h = PieceHash::of(&bytes);
        (
            Record {
                id: "genai:aé#in".into(),
                body: vec![BodyOp::Lit(b"[".to_vec()), BodyOp::Piece { hash: h, len: bytes.len() as u32 }],
                attrs: vec![
                    ("k".into(), AttrValue::Str("v".into())),
                    ("k".into(), AttrValue::Int(-5)),
                    ("f".into(), AttrValue::Float(f64::from_bits(0x7ff8_0000_0000_0001))),
                    ("z".into(), AttrValue::Float(-0.0)),
                    ("b".into(), AttrValue::Bool(true)),
                ],
            },
            vec![(h, bytes)],
        )
    }

    #[test]
    fn record_wire_roundtrips_exactly() {
        let (r, novel) = rec();
        let mut b = Vec::new();
        encode_record(&mut b, &r, &novel);
        let (got, gn) = decode_record(&b).unwrap();
        assert_eq!(got, r, "record must replay exactly, attr order and dupes included");
        assert_eq!(gn.len(), 1);
        assert_eq!(gn[0].1, novel[0].1);
        // float bit patterns, not values
        match (&got.attrs[2].1, &r.attrs[2].1) {
            (AttrValue::Float(a), AttrValue::Float(b)) => assert_eq!(a.to_bits(), b.to_bits()),
            _ => panic!(),
        }
    }

    #[test]
    fn log_replays_and_stops_at_a_torn_tail() {
        let d = std::env::temp_dir().join(format!("turndb-wal-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("wal");
        let (r, novel) = rec();
        {
            let mut w = Wal::open(&p).unwrap();
            for s in 1..=5 {
                w.append(s, &r, &novel).unwrap();
            }
            w.sync().unwrap();
        }
        assert_eq!(Wal::replay(&p).unwrap().len(), 5);

        // a crash mid-append: a frame header promising more than landed
        {
            let mut f = OpenOptions::new().append(true).open(&p).unwrap();
            f.write_all(&[FRAME_TAG, 9, 0, 0, 0, 0, 0, 0, 0, 255, 255, 0, 0]).unwrap();
            f.write_all(b"short").unwrap();
            f.sync_all().unwrap();
        }
        let frames = Wal::replay(&p).unwrap();
        assert_eq!(frames.len(), 5, "a torn tail is the end of the log, not an error");
        assert_eq!(frames[4].seq, 5);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn truncate_empties_the_log() {
        let d = std::env::temp_dir().join(format!("turndb-wal2-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("wal");
        let (r, novel) = rec();
        let mut w = Wal::open(&p).unwrap();
        w.append(1, &r, &novel).unwrap();
        w.sync().unwrap();
        assert!(w.bytes() > 0);
        w.truncate().unwrap();
        assert_eq!(w.bytes(), 0);
        assert!(Wal::replay(&p).unwrap().is_empty());
        std::fs::remove_dir_all(&d).ok();
    }
}
