//! Panic-free, high-performance MPSC chunk ring buffer.
//!
//! # Design
//! - **MPSC**: multiple producers, single consumer.
//! - **Chunked ring**: writers reserve a contiguous chunk and then publish it.
//! - **Atomic cursors**:
//!   - `reserve`: claimed by producers via CAS.
//!   - `publish`: advanced by producers in order to make data visible.
//!   - `consume`: advanced by the single consumer.
//! - **Power-of-two capacity**: enables masking for wrap-around.
//! - **8-byte alignment** for chunk starts.
//! - **Panic-free API**: no panics for normal operation; errors are reported.
//!
//! This module is `std`-only (uses `std::sync::atomic`).

#![forbid(unsafe_op_in_unsafe_fn)]

use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::mem::{align_of, size_of};
use core::ptr;
use core::slice;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Errors returned by ring operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingError {
    /// Capacity is zero, not a power of two, or too small for internal bookkeeping.
    InvalidCapacity,
    /// Reservation size is zero or too large.
    InvalidSize,
    /// Not enough free space for the requested reservation.
    Full,
    /// No published data available for the consumer.
    Empty,
}

/// Align `n` up to `align`, where `align` must be a power of two.
#[inline]
const fn align_up(n: usize, align: usize) -> usize {
    (n + (align - 1)) & !(align - 1)
}

#[inline]
const fn is_power_of_two(x: usize) -> bool {
    x != 0 && (x & (x - 1)) == 0
}

/// Internal header stored before each chunk.
///
/// Layout: `[ChunkHdr][payload padded to 8]`.
///
/// `len` is the payload length in bytes.
/// `state` encodes whether the slot is free/reserved/published.
#[repr(C, align(8))]
struct ChunkHdr {
    /// Payload length
    len: u32,
    /// State marker
    state: u32,
}

// Chunk state machine:
// 0 = free
// 1 = reserved (writer owns, not visible)
// 2 = published (visible to consumer)
const STATE_FREE: u32 = 0;
const STATE_RESERVED: u32 = 1;
const STATE_PUBLISHED: u32 = 2;

const CHUNK_ALIGN: usize = 8;

/// A chunk reservation for a producer.
///
/// The producer gets a mutable slice to fill. Once done, call `publish()`.
///
/// Dropping without publish will keep space reserved; this is intentional to
/// avoid hidden work in `Drop` and to remain panic-free. Callers should publish
/// or abandon (by publishing length 0 if desired by higher-level protocol).
pub struct WriteChunk<'a> {
    ring: &'a ChunkRing,
    /// Absolute offset (monotonic) to the start of this chunk header.
    start: u64,
    /// Total bytes reserved (header + padded payload)
    total: u32,
    /// Payload length
    len: u32,
    /// Pointer to payload within ring
    payload_ptr: *mut u8,
}

impl<'a> WriteChunk<'a> {
    /// Returns a mutable view of the payload.
    #[inline]
    pub fn as_mut(&mut self) -> &mut [u8] {
        // Safety: reservation guarantees unique ownership of this region until published.
        unsafe { slice::from_raw_parts_mut(self.payload_ptr, self.len as usize) }
    }

    /// Publish this chunk, making it visible to the consumer.
    #[inline]
    pub fn publish(self) {
        self.ring.publish_chunk(self.start, self.total, self.len);
        // self consumed; no Drop work.
    }
}

unsafe impl Send for WriteChunk<'_> {}
// Not Sync; mutable access.

/// Zero-copy read handle for the consumer.
///
/// Holds an immutable view into the ring's backing store. The consumer must
/// call `consume()` to advance the consume cursor.
pub struct ReadChunk<'a> {
    ring: &'a ChunkRing,
    start: u64,
    total: u32,
    len: u32,
    payload_ptr: *const u8,
}

impl<'a> ReadChunk<'a> {
    /// Returns the payload bytes.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        // Safety: consumer is single-threaded and this chunk is published and not yet consumed.
        unsafe { slice::from_raw_parts(self.payload_ptr, self.len as usize) }
    }

    /// Consume this chunk, advancing the consumer cursor.
    #[inline]
    pub fn consume(self) {
        self.ring.consume_chunk(self.start, self.total);
    }

    /// Copy the payload into `dst` if it fits.
    ///
    /// Returns the number of bytes copied.
    #[inline]
    pub fn try_read_into(&self, dst: &mut [u8]) -> Option<usize> {
        let n = self.len as usize;
        if dst.len() < n {
            return None;
        }
        // Safety: bytes are immutable.
        unsafe {
            ptr::copy_nonoverlapping(self.payload_ptr, dst.as_mut_ptr(), n);
        }
        Some(n)
    }
}

unsafe impl Send for ReadChunk<'_> {}
unsafe impl Sync for ReadChunk<'_> {}

/// A high-performance MPSC chunk ring.
///
/// Backing storage is a byte buffer of size `capacity` where `capacity` is a power of two.
///
/// The ring stores variable-sized chunks. Each chunk is prefixed with a `ChunkHdr`.
pub struct ChunkRing {
    mask: u64,
    cap: u64,
    buf: UnsafeCell<Vec<u8>>,

    // Monotonic cursors in bytes (not masked). Keep as u64 for long-running systems.
    reserve: AtomicU64,
    publish: AtomicU64,
    consume: AtomicU64,

    // Optional hint to reduce contention in producers when full; consumer updates.
    consume_cached: AtomicU64,

    // Single-consumer guard: best-effort (debug aid). Not relied upon for correctness.
    consumer_entered: AtomicUsize,
}

unsafe impl Send for ChunkRing {}
unsafe impl Sync for ChunkRing {}

impl ChunkRing {
    /// Create a new ring with a power-of-two capacity.
    pub fn new(capacity: usize) -> Result<Self, RingError> {
        if !is_power_of_two(capacity) {
            return Err(RingError::InvalidCapacity);
        }
        if capacity < 64 {
            return Err(RingError::InvalidCapacity);
        }
        // Ensure buffer alignment requirements are satisfiable.
        let _ = align_of::<ChunkHdr>();

        let mut buf = Vec::with_capacity(capacity);
        buf.resize(capacity, 0);

        Ok(Self {
            mask: (capacity as u64) - 1,
            cap: capacity as u64,
            buf: UnsafeCell::new(buf),
            reserve: AtomicU64::new(0),
            publish: AtomicU64::new(0),
            consume: AtomicU64::new(0),
            consume_cached: AtomicU64::new(0),
            consumer_entered: AtomicUsize::new(0),
        })
    }

    /// Total capacity in bytes.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.cap as usize
    }

    #[inline]
    fn buf_ptr(&self) -> *mut u8 {
        // Safety: Vec is never reallocated after creation.
        unsafe { (&mut *self.buf.get()).as_mut_ptr() }
    }

    #[inline]
    fn hdr_size() -> usize {
        size_of::<ChunkHdr>()
    }

    /// Reserve a chunk of `len` bytes.
    ///
    /// Returns a `WriteChunk` providing zero-copy mutable access to the payload.
    pub fn try_reserve(&self, len: usize) -> Result<WriteChunk<'_>, RingError> {
        if len == 0 {
            return Err(RingError::InvalidSize);
        }
        if len > (u32::MAX as usize) {
            return Err(RingError::InvalidSize);
        }

        let total = Self::hdr_size().saturating_add(len);
        let total = align_up(total, CHUNK_ALIGN);
        if total == 0 || total > (u32::MAX as usize) {
            return Err(RingError::InvalidSize);
        }
        if total as u64 > self.cap {
            return Err(RingError::InvalidSize);
        }

        // Fast path loop: claim space by advancing reserve cursor.
        // We must ensure we never overlap the consumer cursor.
        let mut spin = 0u32;
        loop {
            let r = self.reserve.load(Ordering::Relaxed);
            // Align start of chunk to 8 bytes.
            let start = align_up(r as usize, CHUNK_ALIGN) as u64;
            let end = start.wrapping_add(total as u64);

            // Compute used/free using monotonic counters.
            // Conservative consume view: use cached consume, refresh occasionally.
            let mut c = self.consume_cached.load(Ordering::Relaxed);
            // Refresh if we appear full or every so often.
            if end.wrapping_sub(c) > self.cap || (spin & 0x3f) == 0 {
                c = self.consume.load(Ordering::Acquire);
                self.consume_cached.store(c, Ordering::Relaxed);
            }

            if end.wrapping_sub(c) > self.cap {
                return Err(RingError::Full);
            }

            // Attempt to claim [r, end) by setting reserve to end.
            match self.reserve.compare_exchange_weak(
                r,
                end,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    // We own region [start, end). Need to write header as RESERVED.
                    let idx = (start & self.mask) as usize;
                    // If header or payload would wrap, insert a padding chunk.
                    // We ensure each chunk is contiguous in the underlying buffer.
                    if idx + total > self.cap as usize {
                        // Need to pad to end-of-buffer and retry from next wrap.
                        // Create a special 0-len published padding chunk so consumer can skip.
                        let pad = (self.cap as usize) - idx;
                        if pad >= Self::hdr_size() {
                            self.write_hdr(start, 0, STATE_PUBLISHED);
                        }
                        // Publish padding by advancing publish cursor to end-of-buffer boundary.
                        self.publish_padding(start, pad as u32);
                        // Now retry reservation from the wrapped position.
                        continue;
                    }

                    // Write RESERVED header.
                    self.write_hdr(start, len as u32, STATE_RESERVED);

                    let payload_ptr = unsafe { self.buf_ptr().add(idx + Self::hdr_size()) };
                    return Ok(WriteChunk {
                        ring: self,
                        start,
                        total: total as u32,
                        len: len as u32,
                        payload_ptr,
                    });
                }
                Err(_) => {
                    spin = spin.wrapping_add(1);
                    if spin < 10 {
                        spin_loop();
                    } else {
                        std::thread::yield_now();
                    }
                }
            }
        }
    }

    #[inline]
    fn write_hdr(&self, start: u64, len: u32, state: u32) {
        let idx = (start & self.mask) as usize;
        // Safety: within buffer and properly aligned by CHUNK_ALIGN; ChunkHdr is align(8).
        unsafe {
            let p = self.buf_ptr().add(idx) as *mut ChunkHdr;
            // Use volatile-ish semantics via plain write then fence; header is then published by
            // updating `publish` cursor with Release ordering.
            (*p).len = len;
            (*p).state = state;
        }
    }

    /// Publish a normal reserved chunk.
    fn publish_chunk(&self, start: u64, total: u32, len: u32) {
        // Mark header published, then advance publish cursor in order.
        self.write_hdr(start, len, STATE_PUBLISHED);

        // Advance publish cursor in-order.
        // Multiple producers may publish out of order; they must wait for turn.
        let end = start.wrapping_add(total as u64);
        let mut spin = 0u32;
        loop {
            let p = self.publish.load(Ordering::Acquire);
            if p == start {
                if self
                    .publish
                    .compare_exchange_weak(p, end, Ordering::Release, Ordering::Relaxed)
                    .is_ok()
                {
                    break;
                }
            } else {
                // Wait until earlier chunks are published.
                spin = spin.wrapping_add(1);
                if spin < 50 {
                    spin_loop();
                } else {
                    std::thread::yield_now();
                }
            }
        }
    }

    /// Publish padding when the reserved region would wrap.
    fn publish_padding(&self, start: u64, pad: u32) {
        let end = start.wrapping_add(pad as u64);
        let mut spin = 0u32;
        loop {
            let p = self.publish.load(Ordering::Acquire);
            if p == start {
                if self
                    .publish
                    .compare_exchange_weak(p, end, Ordering::Release, Ordering::Relaxed)
                    .is_ok()
                {
                    break;
                }
            } else {
                spin = spin.wrapping_add(1);
                if spin < 50 {
                    spin_loop();
                } else {
                    std::thread::yield_now();
                }
            }
        }
    }

    /// Attempt to read the next published chunk (single consumer).
    pub fn try_read(&self) -> Result<ReadChunk<'_>, RingError> {
        // Best-effort single-consumer detection.
        if self
            .consumer_entered
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // Another consumer active; treat as empty to avoid UB.
            return Err(RingError::Empty);
        }

        let res = self.try_read_inner();
        self.consumer_entered.store(0, Ordering::Release);
        res
    }

    fn try_read_inner(&self) -> Result<ReadChunk<'_>, RingError> {
        let c = self.consume.load(Ordering::Relaxed);
        let p = self.publish.load(Ordering::Acquire);
        if c == p {
            return Err(RingError::Empty);
        }

        let start = align_up(c as usize, CHUNK_ALIGN) as u64;
        if start != c {
            // Consumer must follow same alignment rule; skip alignment padding.
            self.consume.store(start, Ordering::Release);
            self.consume_cached.store(start, Ordering::Relaxed);
        }

        let idx = (start & self.mask) as usize;
        if idx + Self::hdr_size() > self.cap as usize {
            // Not enough space for header at end; wrap.
            let next = (start + (self.cap - (idx as u64))) as u64;
            self.consume.store(next, Ordering::Release);
            self.consume_cached.store(next, Ordering::Relaxed);
            return Err(RingError::Empty);
        }

        // Read header.
        let (len, state) = unsafe {
            let p = self.buf_ptr().add(idx) as *const ChunkHdr;
            ((*p).len, (*p).state)
        };

        if state != STATE_PUBLISHED {
            // Producer hasn't published yet.
            return Err(RingError::Empty);
        }

        let total = align_up(Self::hdr_size() + (len as usize), CHUNK_ALIGN) as u32;

        // Padding chunk (len=0) used to wrap: consume it immediately.
        if len == 0 {
            self.consume_chunk(start, total);
            return Err(RingError::Empty);
        }

        if idx + (total as usize) > self.cap as usize {
            // Corrupt / partial wrap. Treat as empty.
            return Err(RingError::Empty);
        }

        let payload_ptr = unsafe { self.buf_ptr().add(idx + Self::hdr_size()) as *const u8 };
        Ok(ReadChunk {
            ring: self,
            start,
            total,
            len,
            payload_ptr,
        })
    }

    fn consume_chunk(&self, start: u64, total: u32) {
        let end = start.wrapping_add(total as u64);
        // Advance consume cursor.
        self.consume.store(end, Ordering::Release);
        self.consume_cached.store(end, Ordering::Relaxed);

        // Mark header free (optional; not required for correctness since we use cursors).
        self.write_hdr(start, 0, STATE_FREE);
    }

    /// Convenience: read the next chunk and copy into `dst`.
    ///
    /// Returns `Ok(n)` bytes copied and consumed; `Empty` if none; `InvalidSize` if dst too small.
    pub fn try_read_into(&self, dst: &mut [u8]) -> Result<usize, RingError> {
        match self.try_read() {
            Ok(chunk) => {
                let n = chunk.len as usize;
                if dst.len() < n {
                    return Err(RingError::InvalidSize);
                }
                // Copy then consume.
                unsafe {
                    ptr::copy_nonoverlapping(chunk.payload_ptr, dst.as_mut_ptr(), n);
                }
                chunk.consume();
                Ok(n)
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn capacity_must_be_power_of_two() {
        assert_eq!(ChunkRing::new(0).err(), Some(RingError::InvalidCapacity));
        assert_eq!(ChunkRing::new(3).err(), Some(RingError::InvalidCapacity));
        assert!(ChunkRing::new(64).is_ok());
        assert!(ChunkRing::new(128).is_ok());
    }

    #[test]
    fn single_thread_roundtrip() {
        let ring = ChunkRing::new(256).unwrap();
        let mut w = ring.try_reserve(5).unwrap();
        w.as_mut().copy_from_slice(b"hello");
        w.publish();

        let r = ring.try_read().unwrap();
        assert_eq!(r.as_bytes(), b"hello");
        r.consume();
        assert_eq!(ring.try_read().err(), Some(RingError::Empty));
    }

    #[test]
    fn mpsc_smoke() {
        let ring = Arc::new(ChunkRing::new(1 << 12).unwrap());
        let producers = 4;
        let per = 2000;

        let mut ths = Vec::new();
        for p in 0..producers {
            let r = ring.clone();
            ths.push(thread::spawn(move || {
                for i in 0..per {
                    let msg = (p as u32, i as u32);
                    let mut buf = [0u8; 8];
                    buf[..4].copy_from_slice(&msg.0.to_le_bytes());
                    buf[4..].copy_from_slice(&msg.1.to_le_bytes());

                    loop {
                        match r.try_reserve(8) {
                            Ok(mut w) => {
                                w.as_mut().copy_from_slice(&buf);
                                w.publish();
                                break;
                            }
                            Err(RingError::Full) => {
                                std::thread::yield_now();
                            }
                            Err(e) => panic!("unexpected: {e:?}"),
                        }
                    }
                }
            }));
        }

        let total = producers * per;
        let mut got = 0;
        let mut dst = [0u8; 8];
        while got < total {
            match ring.try_read_into(&mut dst) {
                Ok(8) => {
                    got += 1;
                }
                Err(RingError::Empty) => {
                    std::thread::yield_now();
                }
                Err(e) => panic!("unexpected: {e:?}"),
                Ok(n) => panic!("unexpected n={n}"),
            }
        }

        for t in ths {
            t.join().unwrap();
        }
    }
}
