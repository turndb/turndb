//! Positioned reads behind a trait — the seam every future backend goes through.
//!
//! Everything that READS store bytes — part sections, fold blocks — bottoms out in "give me `n`
//! bytes at offset `o`". That shape is exactly what a plain file provides, what an extent of a
//! pack file provides, and what an object store's range request provides. Putting the trait in now,
//! while it costs one indirection and nothing else, is what keeps those backends a new impl each
//! rather than a rewrite: a reader never learns whether its bytes came from a directory, a pack,
//! or a socket.
//!
//! Deliberately READ-ONLY and deliberately minimal. The write path stays on real files in a real
//! directory — append semantics, fsync, and rename atomicity are directory-store properties, and
//! no other backend is asked to fake them.

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

/// A bounded extent of another source — a file inside a pack. Offsets are relative to the slice,
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
}
