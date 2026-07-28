//! The id column: front-coded, restart-blocked, binary-searchable.
//!
//! Record ids in a trace store share enormous prefixes (`genai:<trace>:<span>#input`), so each id
//! stores only the bytes that differ from its predecessor. Every `RESTART` ids the sharing resets and
//! a full id is written, which is what keeps the column *searchable*: a lookup binary-searches the
//! restart points, then walks at most `RESTART` entries.
//!
//! ```text
//! entry:   varint shared | varint suffix_len | suffix bytes
//! restart: shared is always 0, so the entry carries a whole id
//! ```

use anyhow::{bail, Result};

/// Ids between restart points. Larger shares more prefix; smaller searches faster.
pub const RESTART: usize = 16;

pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub fn get_varint(b: &[u8], at: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *b.get(*at).ok_or_else(|| anyhow::anyhow!("varint truncated"))?;
        *at += 1;
        v |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
        if shift > 63 {
            bail!("varint overflow");
        }
    }
}

fn shared_prefix(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

/// Encode a **sorted, distinct** id list. Returns `(stream, restart_offsets)`.
pub fn build(ids: &[String]) -> Result<(Vec<u8>, Vec<u32>)> {
    let mut stream = Vec::new();
    let mut restarts = Vec::new();
    let mut prev: &[u8] = b"";
    for (i, id) in ids.iter().enumerate() {
        let cur = id.as_bytes();
        if i > 0 && cur <= prev {
            bail!("id column requires strictly increasing ids: {:?} then {:?}", prev, id);
        }
        let shared = if i % RESTART == 0 {
            restarts.push(stream.len() as u32);
            0
        } else {
            shared_prefix(prev, cur)
        };
        put_varint(&mut stream, shared as u64);
        put_varint(&mut stream, (cur.len() - shared) as u64);
        stream.extend_from_slice(&cur[shared..]);
        prev = cur;
    }
    Ok((stream, restarts))
}

/// A decoded, borrowed view over an id column.
pub struct IdCol<'a> {
    stream: &'a [u8],
    restarts: &'a [u32],
    len: usize,
}

impl<'a> IdCol<'a> {
    pub fn new(stream: &'a [u8], restarts: &'a [u32], len: usize) -> IdCol<'a> {
        IdCol { stream, restarts, len }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The id at index `i`, decoded by walking from its restart point.
    pub fn get(&self, i: usize) -> Result<Vec<u8>> {
        if i >= self.len {
            bail!("id index {i} out of range ({} ids)", self.len);
        }
        let group = i / RESTART;
        let mut at = *self
            .restarts
            .get(group)
            .ok_or_else(|| anyhow::anyhow!("missing restart {group}"))? as usize;
        let mut cur: Vec<u8> = Vec::new();
        for _ in 0..=(i % RESTART) {
            let shared = get_varint(self.stream, &mut at)? as usize;
            let suffix_len = get_varint(self.stream, &mut at)? as usize;
            // `suffix_len > len - at` and not `at + suffix_len > len`: the sum can overflow.
            if shared > cur.len() || suffix_len > self.stream.len() - at {
                bail!("corrupt id column entry");
            }
            cur.truncate(shared);
            cur.extend_from_slice(&self.stream[at..at + suffix_len]);
            at += suffix_len;
        }
        Ok(cur)
    }

    /// Index of `needle`, or `None`. Binary-searches restart points, then walks within the group.
    pub fn find(&self, needle: &[u8]) -> Result<Option<usize>> {
        if self.len == 0 {
            return Ok(None);
        }
        // largest group whose first id is <= needle
        let (mut lo, mut hi) = (0usize, self.restarts.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            let first = self.get(mid * RESTART)?;
            if first.as_slice() <= needle {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            return Ok(None); // needle sorts before every id
        }
        let group = lo - 1;
        let start = group * RESTART;
        let end = (start + RESTART).min(self.len);
        for i in start..end {
            let id = self.get(i)?;
            match id.as_slice().cmp(needle) {
                std::cmp::Ordering::Equal => return Ok(Some(i)),
                std::cmp::Ordering::Greater => return Ok(None),
                std::cmp::Ordering::Less => {}
            }
        }
        Ok(None)
    }

    /// A streaming cursor over the ids, in order — one id resident at a time. `iter` materializes
    /// the whole column; a merge over parts of millions of rows must not.
    pub fn cursor(&self) -> IdCursor<'_> {
        IdCursor { stream: self.stream, len: self.len, at: 0, done: 0, cur: Vec::new() }
    }

    /// Index of the first id `>= needle` — the lower bound a range scan starts from.
    ///
    /// The same binary search over restart points that [`IdCol::find`] uses, but answering "where
    /// would it go" rather than "is it here". That difference is what turns the id column into a
    /// range index at no additional storage cost: ids are sorted, so a range is a contiguous run,
    /// and finding its start is all a paged query needs.
    pub fn lower_bound(&self, needle: &[u8]) -> Result<usize> {
        if self.len == 0 {
            return Ok(0);
        }
        // largest restart group whose first id is < needle
        let (mut lo, mut hi) = (0usize, self.restarts.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.get(mid * RESTART)?.as_slice() < needle {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        // `lo` is the first group starting at or after `needle`; the answer is in the group before
        // it (if any), because that group straddles the boundary.
        let start = lo.saturating_sub(1) * RESTART;
        for i in start..self.len {
            if self.get(i)?.as_slice() >= needle {
                return Ok(i);
            }
        }
        Ok(self.len)
    }

    /// Every id in order — the scan path.
    pub fn iter(&self) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::with_capacity(self.len);
        let mut at = 0usize;
        let mut cur: Vec<u8> = Vec::new();
        for _ in 0..self.len {
            let shared = get_varint(self.stream, &mut at)? as usize;
            let suffix_len = get_varint(self.stream, &mut at)? as usize;
            if shared > cur.len() || suffix_len > self.stream.len() - at {
                bail!("corrupt id column entry");
            }
            cur.truncate(shared);
            cur.extend_from_slice(&self.stream[at..at + suffix_len]);
            at += suffix_len;
            out.push(cur.clone());
        }
        Ok(out)
    }
}

/// Sequential decoder holding one id of state. `next()` yields the next id or `None` at the end;
/// a decode error surfaces as `Err`, never as a silent stop.
pub struct IdCursor<'a> {
    stream: &'a [u8],
    len: usize,
    at: usize,
    done: usize,
    cur: Vec<u8>,
}

impl<'a> IdCursor<'a> {
    /// A cursor straight over a stream — for callers holding section bytes without an [`IdCol`].
    pub fn new(stream: &'a [u8], len: usize) -> IdCursor<'a> {
        IdCursor { stream, len, at: 0, done: 0, cur: Vec::new() }
    }

    pub fn next_id(&mut self) -> Result<Option<&[u8]>> {
        if self.done >= self.len {
            return Ok(None);
        }
        let shared = get_varint(self.stream, &mut self.at)? as usize;
        let suffix_len = get_varint(self.stream, &mut self.at)? as usize;
        if shared > self.cur.len() || suffix_len > self.stream.len() - self.at {
            bail!("corrupt id column entry");
        }
        self.cur.truncate(shared);
        self.cur.extend_from_slice(&self.stream[self.at..self.at + suffix_len]);
        self.at += suffix_len;
        self.done += 1;
        Ok(Some(&self.cur))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for t in 0..12 {
            for s in 0..7 {
                v.push(format!("genai:trace{t:04}:span{s:04}#input"));
                v.push(format!("genai:trace{t:04}:span{s:04}#output"));
            }
        }
        v.push("zzz-last".into());
        v.push("aaa-first".into());
        v.sort();
        v
    }

    #[test]
    fn roundtrips_every_id() {
        let ids = ids();
        let (stream, restarts) = build(&ids).unwrap();
        let c = IdCol::new(&stream, &restarts, ids.len());
        for (i, want) in ids.iter().enumerate() {
            assert_eq!(c.get(i).unwrap(), want.as_bytes(), "id {i} decoded wrong");
        }
        let all = c.iter().unwrap();
        assert_eq!(all.len(), ids.len());
        for (got, want) in all.iter().zip(&ids) {
            assert_eq!(got, want.as_bytes());
        }
        // and the streaming cursor sees exactly the same sequence
        let mut cur = c.cursor();
        for want in &ids {
            assert_eq!(cur.next_id().unwrap().unwrap(), want.as_bytes());
        }
        assert!(cur.next_id().unwrap().is_none(), "cursor must end exactly at len");
    }

    #[test]
    fn finds_present_and_rejects_absent() {
        let ids = ids();
        let (stream, restarts) = build(&ids).unwrap();
        let c = IdCol::new(&stream, &restarts, ids.len());
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(c.find(id.as_bytes()).unwrap(), Some(i), "lookup failed for {id}");
        }
        for absent in ["", "genai:trace0000:span0000", "genai:zzz", "aaa", "zzzz", "\u{7f}"] {
            assert_eq!(c.find(absent.as_bytes()).unwrap(), None, "{absent:?} must not be found");
        }
    }

    #[test]
    fn front_coding_actually_shrinks() {
        let ids = ids();
        let raw: usize = ids.iter().map(|s| s.len()).sum();
        let (stream, _) = build(&ids).unwrap();
        assert!(stream.len() * 2 < raw, "front coding must roughly halve highly-shared ids: {} vs {}", stream.len(), raw);
    }

    #[test]
    fn unsorted_or_duplicate_ids_refuse() {
        assert!(build(&["b".to_string(), "a".to_string()]).is_err(), "unsorted must refuse");
        assert!(build(&["a".to_string(), "a".to_string()]).is_err(), "duplicates must refuse");
    }

    #[test]
    fn empty_and_single() {
        let (s, r) = build(&[]).unwrap();
        let c = IdCol::new(&s, &r, 0);
        assert!(c.is_empty());
        assert_eq!(c.find(b"x").unwrap(), None);

        let one = vec!["only".to_string()];
        let (s, r) = build(&one).unwrap();
        let c = IdCol::new(&s, &r, 1);
        assert_eq!(c.get(0).unwrap(), b"only");
        assert_eq!(c.find(b"only").unwrap(), Some(0));
        assert_eq!(c.find(b"other").unwrap(), None);
    }
}
