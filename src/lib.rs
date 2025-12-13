// This crate implements a bounded queue ring buffer (BQRB).
//
// Key goals for this revision:
// - Correct MPSC (multi-producer single-consumer) reservation semantics.
// - Atomic header (head/tail) state with proper acquire/release pairing.
// - Zero-allocation read path (bqrb_read) that ergonomically exposes slices.
//
// Notes:
// - Producers coordinate via a single atomic `tail` reservation counter.
//   Each producer performs a CAS loop to reserve a contiguous span.
// - Consumer advances `head` with release, producers observe it with acquire.
// - Producers publish written bytes by advancing a separate `commit` counter
//   (monotonic) using compare_exchange to preserve in-order visibility.
// - Consumer reads up to `commit` with acquire to see fully written data.
//
// This design avoids subtle bugs where multiple producers can overwrite each
// other due to non-atomic tail updates, and ensures bytes are visible to the
// reader only after being fully written.

#![forbid(unsafe_code)]

use core::cmp::min;
use core::ops::Range;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Error returned from write attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteError {
    /// Not enough space in the ring buffer to reserve `len` bytes.
    Full,
}

/// A bounded queue ring buffer.
///
/// The buffer supports multi-producer writes (MPSC) and a single consumer.
///
/// Internal counters are monotonically increasing and wrap via modulo when
/// indexing into the backing slice.
#[derive(Debug)]
pub struct Bqrb {
    buf: Vec<u8>,
    cap: usize,

    // Consumer-owned logical head (bytes consumed). Published with Release.
    head: AtomicUsize,

    // Producer reservation pointer (bytes reserved). Modified with CAS.
    tail: AtomicUsize,

    // Producer commit pointer (bytes fully written and visible to consumer).
    commit: AtomicUsize,
}

impl Bqrb {
    /// Create a new ring buffer with capacity `cap` bytes.
    pub fn new(cap: usize) -> Self {
        assert!(cap > 0);
        Self {
            buf: vec![0u8; cap],
            cap,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            commit: AtomicUsize::new(0),
        }
    }

    /// Total capacity in bytes.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Number of bytes currently committed (readable).
    #[inline]
    pub fn len(&self) -> usize {
        // commit must be observed with Acquire to see producer writes.
        let c = self.commit.load(Ordering::Acquire);
        // head can be Relaxed because it is written only by the consumer,
        // but producers read it with Acquire; for consumer's own view Relaxed ok.
        let h = self.head.load(Ordering::Relaxed);
        c.saturating_sub(h)
    }

    /// Remaining free space (for additional reservations).
    #[inline]
    pub fn remaining(&self) -> usize {
        let h = self.head.load(Ordering::Acquire);
        let t = self.tail.load(Ordering::Relaxed);
        self.cap.saturating_sub(t.saturating_sub(h))
    }

    /// Reserve `len` bytes for a producer write.
    ///
    /// On success returns the logical range `[start, start+len)` reserved.
    fn reserve(&self, len: usize) -> Result<Range<usize>, WriteError> {
        if len == 0 {
            // Reserve an empty range; consistent with monotonic counters.
            let t = self.tail.load(Ordering::Relaxed);
            return Ok(t..t);
        }
        if len > self.cap {
            return Err(WriteError::Full);
        }

        loop {
            // See latest head with Acquire because consumer releases head updates
            // after reading bytes.
            let head = self.head.load(Ordering::Acquire);
            let tail = self.tail.load(Ordering::Relaxed);
            let used = tail.saturating_sub(head);
            let free = self.cap - used;
            if len > free {
                return Err(WriteError::Full);
            }
            let new_tail = tail + len;
            match self.tail.compare_exchange_weak(
                tail,
                new_tail,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(tail..new_tail),
                Err(_) => core::hint::spin_loop(),
            }
        }
    }

    /// Copy `src` into the buffer at the reserved logical range.
    fn write_reserved(&self, range: Range<usize>, src: &[u8]) {
        debug_assert_eq!(range.end - range.start, src.len());
        let start = range.start;
        let len = src.len();
        if len == 0 {
            return;
        }

        let cap = self.cap;
        let start_idx = start % cap;
        let first = min(len, cap - start_idx);

        // Safe because indices are within bounds.
        self.buf[start_idx..start_idx + first].copy_from_slice(&src[..first]);
        if first < len {
            let rest = len - first;
            self.buf[0..rest].copy_from_slice(&src[first..]);
        }
    }

    /// Publish committed bytes in-order.
    ///
    /// Multiple producers may complete out of order; this method ensures `commit`
    /// advances strictly in reservation order, so the consumer never observes
    /// a hole of unwritten bytes.
    fn commit_range(&self, range: Range<usize>) {
        let end = range.end;

        // Publish written bytes: the copy occurred before this call, and we use
        // Release/AcqRel on commit CAS so the consumer (Acquire) sees the writes.
        loop {
            let cur = self.commit.load(Ordering::Acquire);
            if cur == end {
                return; // already committed (e.g., len == 0)
            }
            if cur == range.start {
                match self.commit.compare_exchange_weak(
                    cur,
                    end,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return,
                    Err(_) => core::hint::spin_loop(),
                }
            } else {
                // Someone earlier hasn't committed yet; wait.
                core::hint::spin_loop();
            }
        }
    }

    /// Write bytes into the ring buffer.
    ///
    /// This is MPSC-safe.
    pub fn write(&self, src: &[u8]) -> Result<(), WriteError> {
        let range = self.reserve(src.len())?;
        self.write_reserved(range.clone(), src);
        self.commit_range(range);
        Ok(())
    }

    /// Read up to `max_len` committed bytes.
    ///
    /// This returns at most two slices corresponding to the contiguous segments
    /// in the underlying ring. It performs zero allocations.
    ///
    /// The returned slices are only valid until the next call that mutates the
    /// buffer (e.g., `advance_read`, `read`, or any producer write that wraps and
    /// overwrites after sufficient advancement). With a single consumer, using
    /// these immediately is safe.
    pub fn bqrb_read(&self, max_len: usize) -> ReadSegments<'_> {
        let head = self.head.load(Ordering::Relaxed);
        let commit = self.commit.load(Ordering::Acquire);
        let available = commit.saturating_sub(head);
        let to_read = min(available, max_len);

        if to_read == 0 {
            return ReadSegments {
                first: &[],
                second: &[],
                total_len: 0,
            };
        }

        let cap = self.cap;
        let start_idx = head % cap;
        let first_len = min(to_read, cap - start_idx);

        let first = &self.buf[start_idx..start_idx + first_len];
        let second_len = to_read - first_len;
        let second = if second_len == 0 {
            &[]
        } else {
            &self.buf[0..second_len]
        };

        ReadSegments {
            first,
            second,
            total_len: to_read,
        }
    }

    /// Advance the consumer head by `n` bytes.
    ///
    /// Panics if `n` exceeds the currently committed readable bytes.
    pub fn advance_read(&self, n: usize) {
        if n == 0 {
            return;
        }
        let head = self.head.load(Ordering::Relaxed);
        let commit = self.commit.load(Ordering::Acquire);
        let available = commit.saturating_sub(head);
        assert!(n <= available, "advance_read beyond committed data");

        // Release: makes consumption visible to producers reserving space.
        self.head.store(head + n, Ordering::Release);
    }

    /// Convenience: read up to `max_len` and immediately advance the head.
    ///
    /// This copies into the provided `dst` and advances by the number of bytes
    /// copied.
    pub fn read_into(&self, dst: &mut [u8]) -> usize {
        let segs = self.bqrb_read(dst.len());
        let mut copied = 0;
        let f = segs.first;
        if !f.is_empty() {
            dst[..f.len()].copy_from_slice(f);
            copied += f.len();
        }
        let s = segs.second;
        if !s.is_empty() {
            dst[copied..copied + s.len()].copy_from_slice(s);
            copied += s.len();
        }
        self.advance_read(copied);
        copied
    }
}

/// Zero-allocation view of up to two ring segments.
#[derive(Debug, Clone, Copy)]
pub struct ReadSegments<'a> {
    pub first: &'a [u8],
    pub second: &'a [u8],
    pub total_len: usize,
}

impl<'a> ReadSegments<'a> {
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.total_len == 0
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.total_len
    }

    /// Iterate over contained slices in order.
    #[inline]
    pub fn slices(&self) -> impl Iterator<Item = &'a [u8]> {
        [self.first, self.second].into_iter().filter(|s| !s.is_empty())
    }
}
