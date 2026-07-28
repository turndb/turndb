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
use std::fs::File;
use std::path::Path;

pub const FRAME_TAG: u8 = 0x57;
/// A DELETION. Its payload is the id alone — a tombstone has no body, no attributes and no content, so
/// it costs a frame and nothing else.
pub const TOMB_TAG: u8 = 0x58;
/// A BATCH COMMIT. Everything since the previous commit point that carries an in-batch tag is
/// applied by this frame, and is applied not at all without it. Its payload is the member count,
/// as a varint — redundant with what replay observed, and checked against it, because a marker
/// sealing a different number of frames than the writer put down means the log is not what was
/// written.
pub const BATCH_COMMIT_TAG: u8 = 0x59;
/// [`FRAME_TAG`], inside a batch: applied only when a [`BATCH_COMMIT_TAG`] seals it.
pub const BATCH_FRAME_TAG: u8 = 0x5A;
/// [`TOMB_TAG`], inside a batch.
pub const BATCH_TOMB_TAG: u8 = 0x5B;
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
    // `n > len - at`, never `at + n > len`: the left side cannot overflow (at <= len always), the
    // right side can — a huge declared length wrapped the sum and PASSED the check.
    if n > b.len() - *at {
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

/// Decode a record payload. Replay hands this only frames whose crc verified, but the crc is a
/// TORN-WRITE detector, not a validity proof — a buggy writer checksums its bugs perfectly. So
/// every count is capped by the bytes that would have to carry it before it sizes an allocation,
/// and every fixed-width read is bounds-checked: corrupt input is an error, never a panic.
pub fn decode_record(b: &[u8]) -> Result<(Record, Vec<(PieceHash, Vec<u8>)>)> {
    fn take<'a>(b: &'a [u8], at: &mut usize, n: usize) -> Result<&'a [u8]> {
        if n > b.len() - *at {
            bail!("wal: field of {n} bytes runs past the frame");
        }
        let s = &b[*at..*at + n];
        *at += n;
        Ok(s)
    }
    let mut at = 0usize;
    let id = String::from_utf8(get_bytes(b, &mut at)?.to_vec())?;
    let n_ops = get_varint(b, &mut at)? as usize;
    let mut body = Vec::with_capacity(n_ops.min(b.len()));
    for _ in 0..n_ops {
        let tag = *b.get(at).ok_or_else(|| anyhow::anyhow!("wal: truncated op"))?;
        at += 1;
        match tag {
            0 => body.push(BodyOp::Lit(get_bytes(b, &mut at)?.to_vec())),
            1 => {
                let mut h = [0u8; 32];
                h.copy_from_slice(take(b, &mut at, 32)?);
                let len = get_varint(b, &mut at)? as u32;
                body.push(BodyOp::Piece { hash: PieceHash(h), len });
            }
            t => bail!("wal: unknown body op tag {t}"),
        }
    }
    let n_attrs = get_varint(b, &mut at)? as usize;
    let mut attrs = Vec::with_capacity(n_attrs.min(b.len()));
    for _ in 0..n_attrs {
        let key = String::from_utf8(get_bytes(b, &mut at)?.to_vec())?;
        let tag = *b.get(at).ok_or_else(|| anyhow::anyhow!("wal: truncated attr"))?;
        at += 1;
        let v = match tag {
            0 => AttrValue::Str(String::from_utf8(get_bytes(b, &mut at)?.to_vec())?),
            1 => AttrValue::Int(i64::from_le_bytes(take(b, &mut at, 8)?.try_into().unwrap())),
            2 => AttrValue::Float(f64::from_bits(u64::from_le_bytes(
                take(b, &mut at, 8)?.try_into().unwrap(),
            ))),
            3 => AttrValue::Bool(take(b, &mut at, 1)?[0] != 0),
            t => bail!("wal: unknown attr type tag {t}"),
        };
        attrs.push((key, v));
    }
    let n_novel = get_varint(b, &mut at)? as usize;
    let mut novel = Vec::with_capacity(n_novel.min(b.len() / 33 + 1));
    for _ in 0..n_novel {
        let mut h = [0u8; 32];
        h.copy_from_slice(take(b, &mut at, 32)?);
        novel.push((PieceHash(h), get_bytes(b, &mut at)?.to_vec()));
    }
    Ok((Record { id, body, attrs }, novel))
}

/// Buffered writes are EXPLICIT here — an owned buffer flushed through the [`crate::vfs`] seam —
/// rather than a `BufWriter`, whose internal flushes would write to the file behind the seam and
/// leave the DST recorder blind to the very bytes whose crash behavior the log exists to test.
///
/// The file is deliberately NOT opened with `O_APPEND`: writes are positioned, and on Linux
/// `O_APPEND` makes `pwrite` ignore its offset and append anyway — a portability trap that would
/// turn a truncate-then-rewrite into interleaved garbage.
pub struct Wal {
    f: File,
    path: std::path::PathBuf,
    /// Bytes durably ordered in the FILE (not necessarily fsynced).
    file_len: u64,
    /// Frames appended but not yet written to the file.
    buf: Vec<u8>,
    scratch: Vec<u8>,
}

/// Flush the buffer once it holds this much — the same batching a BufWriter provided.
const BUF_FLUSH: usize = 1 << 20;

impl Wal {
    pub fn open(path: &Path) -> Result<Wal> {
        let (f, created) = crate::vfs::open_or_create(path)
            .with_context(|| format!("open wal {}", path.display()))?;
        if created {
            // A created file's NAME is volatile until its parent directory syncs — fsyncing the
            // file alone leaves a dirent a power loss can drop, and an ACK backed by a WAL that
            // can vanish is no ACK at all. Found by the DST harness under the strict-POSIX model:
            // sync() had fsynced the file, the dirent evaporated, and acked records were lost.
            if let Some(parent) = path.parent() {
                crate::vfs::sync_dir(parent).with_context(|| {
                    format!("fsync {} after creating the wal", parent.display())
                })?;
            }
        }
        let file_len = f.metadata()?.len();
        Ok(Wal { f, path: path.to_path_buf(), file_len, buf: Vec::new(), scratch: Vec::new() })
    }

    fn flush_buf(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        crate::vfs::write_all_at(&self.f, &self.path, &self.buf, self.file_len)?;
        self.file_len += self.buf.len() as u64;
        self.buf.clear();
        Ok(())
    }

    /// Frame `payload` under `tag` and append it. The CRC covers the HEADER as well as the payload —
    /// replay checks both, so omitting the header would make every frame read back as a torn write.
    fn append_frame(&mut self, tag: u8, seq: u64, payload: &[u8]) -> Result<()> {
        // The frame length is a u32 on disk. Truncating here would write a frame whose header
        // disagrees with its payload, which replay would read as a torn tail — silently losing every
        // record after it.
        if payload.len() as u64 > u32::MAX as u64 {
            bail!("wal frame payload of {} bytes exceeds the u32 length field", payload.len());
        }
        let mut hdr = Vec::with_capacity(HDR);
        hdr.push(tag);
        hdr.extend_from_slice(&seq.to_le_bytes());
        hdr.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        let mut crc = crc32fast::Hasher::new();
        crc.update(&hdr);
        crc.update(payload);
        let c = crc.finalize();
        self.buf.extend_from_slice(&hdr);
        self.buf.extend_from_slice(payload);
        self.buf.extend_from_slice(&c.to_le_bytes());
        if self.buf.len() >= BUF_FLUSH {
            self.flush_buf()?;
        }
        Ok(())
    }

    /// Log a deletion. Durable on the next [`Wal::sync`], exactly like a put.
    pub fn append_tomb(&mut self, seq: u64, id: &str) -> Result<()> {
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        scratch.extend_from_slice(id.as_bytes());
        let r = self.append_frame(TOMB_TAG, seq, &scratch);
        self.scratch = scratch;
        r
    }

    pub fn append(&mut self, seq: u64, r: &Record, novel: &[(PieceHash, Vec<u8>)]) -> Result<()> {
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        encode_record(&mut scratch, r, novel);
        let res = self.append_frame(FRAME_TAG, seq, &scratch);
        self.scratch = scratch;
        res
    }

    /// Append records and tombstones as ONE atomic unit: the members under in-batch tags, then the
    /// commit marker that seals them. Replay applies all of them or none — a crash anywhere before
    /// the marker lands leaves members that no marker seals, and they are discarded.
    ///
    /// `items`: `(record, novel, is_tombstone)`; a tombstone's record carries only its id.
    pub fn append_batch(
        &mut self,
        seq: u64,
        items: &[(Record, Vec<(PieceHash, Vec<u8>)>, bool)],
    ) -> Result<()> {
        for (r, novel, tomb) in items {
            let mut scratch = std::mem::take(&mut self.scratch);
            scratch.clear();
            let tag = if *tomb {
                scratch.extend_from_slice(r.id.as_bytes());
                BATCH_TOMB_TAG
            } else {
                encode_record(&mut scratch, r, novel);
                BATCH_FRAME_TAG
            };
            let res = self.append_frame(tag, seq, &scratch);
            self.scratch = scratch;
            res?;
        }
        let mut count = Vec::with_capacity(4);
        put_varint(&mut count, items.len() as u64);
        self.append_frame(BATCH_COMMIT_TAG, seq, &count)
    }

    /// The ACK point. Nothing may be reported durable before this returns.
    pub fn sync(&mut self) -> Result<()> {
        self.flush_buf()?;
        crate::vfs::sync_file(&self.f, &self.path).context("fsync wal")?;
        Ok(())
    }

    /// Drop every frame — called once the records they cover are committed in a part.
    pub fn truncate(&mut self) -> Result<()> {
        self.buf.clear();
        crate::vfs::set_len(&self.f, &self.path, 0)?;
        crate::vfs::sync_file(&self.f, &self.path)?;
        self.file_len = 0;
        Ok(())
    }

    pub fn bytes(&self) -> u64 {
        self.file_len + self.buf.len() as u64
    }

    /// Every intact, COMMITTED frame, in order. Stops at the first torn or corrupt one — a partial
    /// tail is the end of the log, not an error, because a crash mid-append leaves exactly that.
    ///
    /// Batch members ride in a holding pen until their commit marker seals them. The marker seals
    /// exactly the `count` members immediately before it; members before those belong to a batch
    /// whose marker never landed — an append that errored partway — and are discarded, exactly as
    /// an unsealed run at the end of the log is. A standalone frame arriving over a non-empty pen
    /// discards the pen the same way: whatever batch those members belonged to never committed.
    pub fn replay(path: &Path) -> Result<Vec<Frame>> {
        let f = match File::open(path) {
            Ok(f) => f,
            Err(_) => return Ok(Vec::new()),
        };
        let len = f.metadata()?.len();
        let mut out = Vec::new();
        let mut pending: Vec<Frame> = Vec::new();
        let mut off = 0u64;
        let mut hdr = [0u8; HDR];
        loop {
            if off + HDR as u64 + CRC as u64 > len
                || crate::sys::read_exact_at(&f, &mut hdr, off).is_err()
            {
                break;
            }
            // An unknown tag is AMBIGUOUS and the two readings are opposite: a crash mid-append
            // leaves garbage here (stop, the log ends), but a newer writer's frame type also lands
            // here (refuse, or silently discard committed records). Treating both as "end of log" —
            // which is what a bare `break` does — means a future frame type silently truncates the
            // log and loses every record after it.
            //
            // The crc disambiguates: a torn tail does not checksum, a well-formed future frame does.
            // So defer the decision until after the crc, below.
            let known = matches!(
                hdr[0],
                FRAME_TAG | TOMB_TAG | BATCH_COMMIT_TAG | BATCH_FRAME_TAG | BATCH_TOMB_TAG
            );
            let seq = u64::from_le_bytes(hdr[1..9].try_into().unwrap());
            let plen = u32::from_le_bytes(hdr[9..13].try_into().unwrap()) as usize;
            let end = off + HDR as u64 + plen as u64 + CRC as u64;
            if end > len {
                break;
            }
            let mut payload = vec![0u8; plen];
            if crate::sys::read_exact_at(&f, &mut payload, off + HDR as u64).is_err() {
                break;
            }
            let mut cb = [0u8; CRC];
            if crate::sys::read_exact_at(&f, &mut cb, off + HDR as u64 + plen as u64).is_err() {
                break;
            }
            let mut crc = crc32fast::Hasher::new();
            crc.update(&hdr);
            crc.update(&payload);
            if crc.finalize() != u32::from_le_bytes(cb) {
                break; // torn write: the log genuinely ends here
            }
            if !known {
                // Checksums correctly, so it was written deliberately — by a build that knows a frame
                // type this one does not. Refusing is the only safe reading; skipping it would apply a
                // suffix of the log without its prefix.
                bail!(
                    "wal frame tag {:#04x} is not a type this build knows, and it checksums — \
                     refusing rather than discarding committed records",
                    hdr[0]
                );
            }
            match hdr[0] {
                BATCH_COMMIT_TAG => {
                    let mut at = 0usize;
                    let Ok(n) = get_varint(&payload, &mut at) else { break };
                    if at != payload.len() {
                        break; // a malformed marker cannot say what it seals
                    }
                    let n = n as usize;
                    if n > pending.len() {
                        // The frame chain is unbroken back to the last commit point, so a marker
                        // sealing more members than preceded it means the log is not what a writer
                        // put down. That is corruption that CHECKSUMS, and it must not quietly
                        // shrink a batch.
                        bail!(
                            "wal batch commit seals {n} frames but only {} precede it",
                            pending.len()
                        );
                    }
                    let start = pending.len() - n;
                    // Members before the sealed run belong to a batch whose marker never landed.
                    out.extend(pending.drain(..).skip(start));
                }
                TOMB_TAG | BATCH_TOMB_TAG => {
                    let Ok(id) = String::from_utf8(payload) else { break };
                    let fr = Frame {
                        seq,
                        tomb: true,
                        record: Record { id, body: Vec::new(), attrs: Vec::new() },
                        novel: Vec::new(),
                    };
                    if hdr[0] == BATCH_TOMB_TAG {
                        pending.push(fr);
                    } else {
                        pending.clear();
                        out.push(fr);
                    }
                }
                _ => match decode_record(&payload) {
                    Ok((record, novel)) => {
                        let fr = Frame { seq, tomb: false, record, novel };
                        if hdr[0] == BATCH_FRAME_TAG {
                            pending.push(fr);
                        } else {
                            pending.clear();
                            out.push(fr);
                        }
                    }
                    Err(_) => break,
                },
            }
            off = end;
        }
        // An unsealed run at the end of the log is a batch that never committed.
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write;

    fn rec() -> (Record, Vec<(PieceHash, Vec<u8>)>) {
        let bytes = b"a novel piece".to_vec();
        let h = PieceHash::of(&bytes);
        (
            Record {
                id: "genai:aé#in".into(),
                body: vec![
                    BodyOp::Lit(b"[".to_vec()),
                    BodyOp::Piece { hash: h, len: bytes.len() as u32 },
                ],
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
    fn a_batch_replays_all_or_nothing() {
        let d = std::env::temp_dir().join(format!("turndb-walbatch-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("wal");
        let mk = |id: &str| {
            let (mut r, n) = rec();
            r.id = id.into();
            (r, n)
        };
        let (a, na) = mk("a");
        let (b, nb) = mk("b");
        {
            let mut w = Wal::open(&p).unwrap();
            w.append(1, &a, &na).unwrap();
            let tomb = Record { id: "z".into(), body: Vec::new(), attrs: Vec::new() };
            w.append_batch(
                2,
                &[
                    (a.clone(), na.clone(), false),
                    (tomb, Vec::new(), true),
                    (b.clone(), nb.clone(), false),
                ],
            )
            .unwrap();
            w.sync().unwrap();
        }
        let f = Wal::replay(&p).unwrap();
        assert_eq!(f.len(), 4, "standalone + three sealed members");
        assert!(f[2].tomb && f[2].record.id == "z");
        assert_eq!(f[3].record.id, "b");

        // Tear the marker off: the members are unsealed and contribute NOTHING.
        let flen = std::fs::metadata(&p).unwrap().len();
        let fh = OpenOptions::new().write(true).open(&p).unwrap();
        fh.set_len(flen - (HDR as u64 + 1 + CRC as u64)).unwrap();
        let f = Wal::replay(&p).unwrap();
        assert_eq!(f.len(), 1, "an unsealed batch must not replay a prefix of itself");

        // A sealed batch after the abandoned run: its marker seals ITS members, not the strays.
        {
            let mut w = Wal::open(&p).unwrap();
            w.append_batch(3, &[(b.clone(), nb.clone(), false)]).unwrap();
            w.sync().unwrap();
        }
        let f = Wal::replay(&p).unwrap();
        assert_eq!(
            f.len(),
            2,
            "abandoned members stay discarded: {:?}",
            f.iter().map(|x| &x.record.id).collect::<Vec<_>>()
        );
        assert_eq!(f[1].record.id, "b");

        // A marker claiming more members than precede it is corruption that checksums: refuse.
        let p2 = d.join("wal2");
        {
            let mut w = Wal::open(&p2).unwrap();
            let mut count = Vec::new();
            put_varint(&mut count, 5);
            w.append_frame(BATCH_COMMIT_TAG, 1, &count).unwrap();
            w.sync().unwrap();
        }
        assert!(Wal::replay(&p2).is_err(), "a marker sealing absent frames must refuse the log");
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
