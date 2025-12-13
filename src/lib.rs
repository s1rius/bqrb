//! Bounded queue ring buffer (bqrb)
//!
//! This crate implements a chunked ring buffer intended for single-producer /
//! single-consumer usage. The core correctness property is that the producer
//! publishes chunks and the consumer reads them without data races.

#![forbid(unsafe_op_in_unsafe_fn)]

use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::mem::{align_of, size_of};
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicU8, Ordering};

/// Chunk state machine.
///
/// The producer writes payload, then publishes by setting `state = READY` with
/// `Release`. The consumer waits/reads `state` with `Acquire` and only then may
/// read payload.
const STATE_EMPTY: u8 = 0;
const STATE_READY: u8 = 1;
const STATE_READING: u8 = 2;

#[repr(C, align(8))]
struct ChunkHdr {
    /// State is atomic to avoid producer/consumer data races.
    state: AtomicU8,
    /// Number of bytes written in the chunk.
    len: u32,
    /// Reserved/padding for 8-byte header alignment.
    _pad: u32,
}

// Safety / layout invariants used throughout:
// - ChunkHdr is 8-byte aligned.
// - ChunkHdr size is a multiple of 8.
const _: () = {
    assert!(align_of::<ChunkHdr>() == 8);
    assert!(size_of::<ChunkHdr>() % 8 == 0);
};

struct Chunk {
    hdr: UnsafeCell<ChunkHdr>,
    data: NonNull<u8>,
    cap: usize,
}

unsafe impl Send for Chunk {}
unsafe impl Sync for Chunk {}

impl Chunk {
    fn new(cap: usize) -> Self {
        // cap stays power-of-two. Caller ensures.
        let layout = Layout::from_size_align(cap, 8).expect("layout");
        // SAFETY: layout is non-zero size and aligned to 8.
        let data = unsafe {
            let p = std::alloc::alloc(layout);
            NonNull::new(p).expect("alloc")
        };
        Self {
            hdr: UnsafeCell::new(ChunkHdr {
                state: AtomicU8::new(STATE_EMPTY),
                len: 0,
                _pad: 0,
            }),
            data,
            cap,
        }
    }

    #[inline]
    fn hdr(&self) -> &ChunkHdr {
        // SAFETY: only interior mutability via atomics and single-producer/
        // single-consumer discipline for non-atomic fields.
        unsafe { &*self.hdr.get() }
    }

    #[inline]
    fn hdr_mut(&self) -> &mut ChunkHdr {
        // SAFETY: producer has exclusive access when writing header fields
        // before publish; consumer has exclusive access when resetting to empty
        // after reading. Ordering is enforced with atomics on `state`.
        unsafe { &mut *self.hdr.get() }
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.cap, 8).expect("layout");
        unsafe { std::alloc::dealloc(self.data.as_ptr(), layout) };
    }
}

/// Ring buffer.
///
/// This type is intended for SPSC usage.
pub struct ChunkRing {
    chunks: Vec<Chunk>,
    mask: usize,
    prod: usize,
    cons: usize,
}

impl ChunkRing {
    /// Create a ring with `capacity` chunks, each holding up to `chunk_bytes`.
    ///
    /// `capacity` must be a power of two.
    pub fn new(capacity: usize, chunk_bytes: usize) -> Self {
        assert!(capacity.is_power_of_two() && capacity > 0);
        assert!(chunk_bytes > 0);
        let chunks = (0..capacity).map(|_| Chunk::new(chunk_bytes)).collect();
        Self {
            chunks,
            mask: capacity - 1,
            prod: 0,
            cons: 0,
        }
    }

    #[inline]
    fn chunk(&self, idx: usize) -> &Chunk {
        &self.chunks[idx & self.mask]
    }

    /// Producer: attempt to reserve the next chunk for writing.
    /// Returns a mutable slice to write into, and a commit handle.
    pub fn producer_reserve(&mut self) -> Option<ProducerChunk<'_>> {
        let c = self.chunk(self.prod);
        if c.hdr().state.load(Ordering::Acquire) != STATE_EMPTY {
            return None;
        }
        // Mark as being written by producer (non-atomic len will be set before publish).
        c.hdr().state.store(STATE_READING, Ordering::Relaxed);
        Some(ProducerChunk { ring: self, idx: self.prod })
    }

    /// Consumer: attempt to acquire the next ready chunk.
    pub fn consumer_acquire(&mut self) -> Option<ConsumerChunk<'_>> {
        let c = self.chunk(self.cons);
        // Acquire pairs with producer's publish Release.
        if c.hdr().state.load(Ordering::Acquire) != STATE_READY {
            return None;
        }
        // Transition to READING so producer won't reuse until consumer releases.
        c.hdr().state.store(STATE_READING, Ordering::Relaxed);
        Some(ConsumerChunk { ring: self, idx: self.cons })
    }
}

pub struct ProducerChunk<'a> {
    ring: &'a mut ChunkRing,
    idx: usize,
}

impl<'a> ProducerChunk<'a> {
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        let c = self.ring.chunk(self.idx);
        unsafe { core::slice::from_raw_parts_mut(c.data.as_ptr(), c.cap) }
    }

    /// Publish written bytes.
    pub fn commit(self, len: usize) {
        let c = self.ring.chunk(self.idx);
        assert!(len <= c.cap);
        // Write len before publish.
        c.hdr_mut().len = len as u32;
        // Publish with Release so consumer Acquire sees payload and len.
        c.hdr().state.store(STATE_READY, Ordering::Release);
        self.ring.prod = self.ring.prod.wrapping_add(1);
    }

    /// Abort without publishing.
    pub fn abort(self) {
        let c = self.ring.chunk(self.idx);
        c.hdr_mut().len = 0;
        c.hdr().state.store(STATE_EMPTY, Ordering::Release);
    }
}

pub struct ConsumerChunk<'a> {
    ring: &'a mut ChunkRing,
    idx: usize,
}

impl<'a> ConsumerChunk<'a> {
    pub fn as_slice(&self) -> &[u8] {
        let c = self.ring.chunk(self.idx);
        // len is synchronized by acquire of READY.
        let len = c.hdr().len as usize;
        unsafe { core::slice::from_raw_parts(c.data.as_ptr(), len) }
    }

    pub fn release(self) {
        let c = self.ring.chunk(self.idx);
        // Reset len then mark empty with Release so producer Acquire observes.
        c.hdr_mut().len = 0;
        c.hdr().state.store(STATE_EMPTY, Ordering::Release);
        self.ring.cons = self.ring.cons.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spsc_basic_roundtrip() {
        let mut r = ChunkRing::new(8, 64);

        // Producer reserve/commit
        let mut p = r.producer_reserve().expect("reserve");
        p.as_mut_slice()[..5].copy_from_slice(b"hello");
        p.commit(5);

        // Consumer acquire/read/release
        let c = r.consumer_acquire().expect("acquire");
        assert_eq!(c.as_slice(), b"hello");
        c.release();

        // Now producer can reuse.
        assert!(r.producer_reserve().is_some());
    }

    #[test]
    fn ring_full_returns_none_no_panic() {
        let mut r = ChunkRing::new(2, 8);

        // Fill both chunks
        for _ in 0..2 {
            let mut p = r.producer_reserve().unwrap();
            p.as_mut_slice()[0] = 1;
            p.commit(1);
        }

        // No space
        assert!(r.producer_reserve().is_none());

        // Drain one
        let c = r.consumer_acquire().unwrap();
        assert_eq!(c.as_slice(), &[1]);
        c.release();

        // Space now available
        assert!(r.producer_reserve().is_some());
    }

    #[test]
    fn consumer_empty_returns_none_no_panic() {
        let mut r = ChunkRing::new(4, 16);
        assert!(r.consumer_acquire().is_none());

        let mut p = r.producer_reserve().unwrap();
        p.as_mut_slice()[0] = 7;
        p.commit(1);

        let c = r.consumer_acquire().unwrap();
        assert_eq!(c.as_slice(), &[7]);
        c.release();

        assert!(r.consumer_acquire().is_none());
    }
}
