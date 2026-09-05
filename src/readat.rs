//! Positioned reads behind a trait — the seam every future backend goes through.
//!
//! Everything that READS store bytes — part sections, fold blocks — bottoms out in "give me `n`
//! bytes at offset `o`". That shape is exactly what a plain file provides, what a container-member
//! extent provides, and what an object store's range request provides. Putting the trait in now,
//! while it costs one indirection and nothing else, is what keeps those backends a new impl each
//! rather than a rewrite: a reader never learns whether its bytes came from a container or a
//! socket.
//!
//! Deliberately READ-ONLY and deliberately minimal. The write path stays on real files in a real
//! container file — append semantics and fsync are local-file properties, and no other backend is
//! asked to fake them.

use std::fs::File;
use std::io;
use std::sync::Arc;

/// A source of bytes addressable by absolute offset. Implementations must be safe for concurrent
/// reads — scans share one source across partitions.
pub trait ReadAt: Send + Sync {
    /// Fill `buf` from `off`, exactly, or error. Short data is an error, not a partial read —
    /// every caller wants a struct's worth of bytes or the truth that they are not there.
    fn read_exact_at(&self, buf: &mut [u8], off: u64) -> io::Result<()>;

    /// Total addressable length.
    fn len(&self) -> io::Result<u64>;

    /// Whether the source holds no bytes at all — an empty file or a zero-length member extent.
    fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }
}

impl ReadAt for File {
    fn read_exact_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        crate::sys::read_exact_at(self, buf, off)
    }
    fn len(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }
}

impl<R: ReadAt + ?Sized> ReadAt for Arc<R> {
    fn read_exact_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        (**self).read_exact_at(buf, off)
    }
    fn len(&self) -> io::Result<u64> {
        (**self).len()
    }
}

impl<R: ReadAt + ?Sized> ReadAt for Box<R> {
    fn read_exact_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        (**self).read_exact_at(buf, off)
    }
    fn len(&self) -> io::Result<u64> {
        (**self).len()
    }
}

/// A bounded extent of another source — a member inside a container. Offsets are relative to the slice,
/// and a read past its end is an error even when the underlying source has more bytes: the whole
/// point of the bound is that a reader inside it cannot wander into a neighbour.
pub struct Slice<R> {
    inner: R,
    off: u64,
    len: u64,
}

impl<R: ReadAt> Slice<R> {
    pub fn new(inner: R, off: u64, len: u64) -> Slice<R> {
        Slice { inner, off, len }
    }
}

impl<R: ReadAt> ReadAt for Slice<R> {
    fn read_exact_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        // `buf.len() > self.len - off`, never `off + buf.len() > self.len`: the sum can overflow
        // on a hostile offset, and an overflowed check PASSES.
        if off > self.len || buf.len() as u64 > self.len - off {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "read of {} bytes at {off} exceeds the slice's {} bytes",
                    buf.len(),
                    self.len
                ),
            ));
        }
        self.inner.read_exact_at(buf, self.off + off)
    }
    fn len(&self) -> io::Result<u64> {
        Ok(self.len)
    }
}

/// A member scattered across extents of one underlying source, read as one logical range.
///
/// A container member that grew across commits is not one contiguous range: each commit that
/// extended it left an extent, with other members' bytes between them. This stitches those
/// extents into a single logical address space, so a part or fold-segment reader opens over it
/// with no idea the bytes are scattered — the translation lives here and nowhere else.
///
/// Extents are logically dense by construction: extent *k+1* begins where *k* ends. A member
/// staged whole has exactly one extent, and that case pays a single comparison — fresh backups
/// and reclaimed containers never hold anything else.
pub struct Extents<R> {
    inner: R,
    /// `(logical_start, physical_off, len)`, logical_start strictly ascending and dense.
    runs: Vec<(u64, u64, u64)>,
    len: u64,
}

impl<R: ReadAt> Extents<R> {
    /// Build from `(physical_off, len)` pairs in logical order. Zero-length extents are dropped —
    /// they address nothing and would only complicate the search.
    pub fn new(inner: R, extents: &[(u64, u64)]) -> Extents<R> {
        let mut runs = Vec::with_capacity(extents.len());
        let mut logical = 0u64;
        for &(off, len) in extents {
            if len == 0 {
                continue;
            }
            runs.push((logical, off, len));
            logical += len;
        }
        Extents { inner, runs, len: logical }
    }
}

impl<R: ReadAt> ReadAt for Extents<R> {
    fn read_exact_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        // Same overflow-safe form as Slice: never `off + buf.len()`, which a hostile offset can
        // wrap past the bound it exists to enforce.
        if off > self.len || buf.len() as u64 > self.len - off {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "read of {} bytes at {off} exceeds the member's {} bytes",
                    buf.len(),
                    self.len
                ),
            ));
        }
        // A zero-length read of an in-bounds offset is a no-op — and the only read an empty
        // member (no runs at all) can satisfy, so it must not reach the run search below.
        if buf.is_empty() {
            return Ok(());
        }
        if self.runs.len() == 1 {
            let (_, phys, _) = self.runs[0];
            return self.inner.read_exact_at(buf, phys + off);
        }
        // First run whose logical range contains `off`, then walk forward filling the buffer.
        let mut i = self.runs.partition_point(|&(start, _, _)| start <= off) - 1;
        let mut at = off;
        let mut filled = 0usize;
        while filled < buf.len() {
            let (start, phys, len) = self.runs[i];
            let within = at - start;
            let take = ((len - within) as usize).min(buf.len() - filled);
            self.inner.read_exact_at(&mut buf[filled..filled + take], phys + within)?;
            filled += take;
            at += take as u64;
            i += 1;
        }
        Ok(())
    }
    fn len(&self) -> io::Result<u64> {
        Ok(self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mem(Vec<u8>);
    impl ReadAt for Mem {
        fn read_exact_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
            let off = off as usize;
            if off > self.0.len() || buf.len() > self.0.len() - off {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "past the end"));
            }
            buf.copy_from_slice(&self.0[off..off + buf.len()]);
            Ok(())
        }
        fn len(&self) -> io::Result<u64> {
            Ok(self.0.len() as u64)
        }
    }

    #[test]
    fn a_slice_translates_and_bounds() {
        let m = Mem((0u8..100).collect());
        let s = Slice::new(m, 10, 20);
        let mut b = [0u8; 5];
        s.read_exact_at(&mut b, 0).unwrap();
        assert_eq!(b, [10, 11, 12, 13, 14]);
        s.read_exact_at(&mut b, 15).unwrap();
        assert_eq!(b, [25, 26, 27, 28, 29]);
        assert_eq!(s.len().unwrap(), 20);

        // one past the end fails, even though the UNDERLYING source has those bytes
        assert!(s.read_exact_at(&mut b, 16).is_err(), "a slice must not read its neighbour");
        // hostile offset must not overflow the bound into passing
        assert!(s.read_exact_at(&mut b, u64::MAX - 2).is_err());
    }

    #[test]
    fn extents_stitch_scattered_ranges_into_one_logical_space() {
        // Physical layout: [0..10) noise, [10..15) run A, [15..20) noise, [20..28) run B.
        let m = Mem((0u8..100).collect());
        let e = Extents::new(m, &[(10, 5), (20, 8)]);
        assert_eq!(e.len().unwrap(), 13);

        // Within one run.
        let mut b = [0u8; 3];
        e.read_exact_at(&mut b, 1).unwrap();
        assert_eq!(b, [11, 12, 13]);
        // Crossing the run boundary — logical 3..9 spans both extents.
        let mut b = [0u8; 6];
        e.read_exact_at(&mut b, 3).unwrap();
        assert_eq!(b, [13, 14, 20, 21, 22, 23]);
        // The whole member at once.
        let mut all = [0u8; 13];
        e.read_exact_at(&mut all, 0).unwrap();
        assert_eq!(&all[..5], &[10, 11, 12, 13, 14]);
        assert_eq!(&all[5..], &[20, 21, 22, 23, 24, 25, 26, 27]);

        // Past the end fails; a hostile offset must not overflow into passing.
        let mut b = [0u8; 2];
        assert!(e.read_exact_at(&mut b, 12).is_err());
        assert!(e.read_exact_at(&mut b, u64::MAX - 1).is_err());

        // Zero-length extents address nothing and are dropped.
        let m = Mem((0u8..100).collect());
        let e = Extents::new(m, &[(10, 0), (30, 4), (50, 0)]);
        assert_eq!(e.len().unwrap(), 4);
        let mut b = [0u8; 4];
        e.read_exact_at(&mut b, 0).unwrap();
        assert_eq!(b, [30, 31, 32, 33]);

        // An empty member reads nothing and refuses everything else.
        let e = Extents::new(Mem(Vec::new()), &[]);
        assert_eq!(e.len().unwrap(), 0);
        assert!(e.is_empty().unwrap());
        let mut b = [0u8; 1];
        assert!(e.read_exact_at(&mut b, 0).is_err());
    }
}
