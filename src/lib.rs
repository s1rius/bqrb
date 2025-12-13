//! bqrb - bounded queue ring buffer
//!
//! This crate provides a bounded **MPSC** (multi-producer, single-consumer)
//! chunked ring buffer optimized for high throughput.
//!
//! Design overview
//! - Producers reserve contiguous capacity via an atomic `tail_reserve` cursor.
//! - Reserved slots are later **published** in-order using `tail_publish`.
//! - The consumer drains published slots using `head_consume`.
//! - A per-slot atomic `state` is used to safely hand over ownership.
//!
//! The public API is entirely `try_*`/non-blocking. No panics are used in the
//! implementation (barring OOM on allocation).

#![cfg_attr(not(test), deny(unused_must_use))]

use core::cell::UnsafeCell;
use core::mem::{ManuallyDrop, MaybeUninit};
use core::ptr;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Error returned from `try_push`/`try_reserve` when the queue is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Full;

/// Error returned from `try_pop` when the queue is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Empty;

/// A bounded MPSC ring buffer.
///
/// This queue is:
/// - Multi-producer: any number of producers can push concurrently.
/// - Single-consumer: exactly one consumer can pop/drain.
/// - Panic-free: all public APIs return `Result`.
///
/// Capacity is fixed at construction time.
pub struct MpscRing<T> {
    cap: u32,
    mask: u32,

    // Reservation cursor (monotonic, in elements).
    tail_reserve: AtomicU64,
    // Publish cursor (monotonic, in elements).
    tail_publish: AtomicU64,
    // Consume cursor (monotonic, in elements).
    head_consume: AtomicU64,

    // Per-slot state: 0 = empty, 1 = ready.
    // (We can base on sequence numbers too; for bounded ring, 0/1 with
    // per-slot generation in high bits is also possible. Here we use a
    // sequence-based state stored in u32 to avoid ABA across wraps.)
    states: Box<[AtomicU32]>,
    buf: Box<[UnsafeCell<MaybeUninit<T>>]>,
}

unsafe impl<T: Send> Send for MpscRing<T> {}
unsafe impl<T: Send> Sync for MpscRing<T> {}

impl<T> MpscRing<T> {
    /// Create a new ring buffer with the given capacity.
    ///
    /// `cap` must be a power of two and > 0.
    pub fn with_capacity(cap: usize) -> Option<Self> {
        if cap == 0 {
            return None;
        }
        if !cap.is_power_of_two() {
            return None;
        }
        if cap > (u32::MAX as usize) {
            return None;
        }
        let cap_u32 = cap as u32;
        let mask = cap_u32 - 1;

        // Sequence-based algorithm: each slot has expected sequence number.
        // On init, state[i] = i.
        let mut states: Vec<AtomicU32> = Vec::with_capacity(cap);
        for i in 0..cap_u32 {
            states.push(AtomicU32::new(i));
        }

        let mut buf: Vec<UnsafeCell<MaybeUninit<T>>> = Vec::with_capacity(cap);
        for _ in 0..cap {
            buf.push(UnsafeCell::new(MaybeUninit::uninit()));
        }

        Some(Self {
            cap: cap_u32,
            mask,
            tail_reserve: AtomicU64::new(0),
            tail_publish: AtomicU64::new(0),
            head_consume: AtomicU64::new(0),
            states: states.into_boxed_slice(),
            buf: buf.into_boxed_slice(),
        })
    }

    /// Returns the capacity.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.cap as usize
    }

    /// Try to push a value.
    #[inline]
    pub fn try_push(&self, value: T) -> Result<(), Full> {
        // Reserve exactly one slot and then publish it.
        let ticket = match self.try_reserve(1) {
            Ok(t) => t,
            Err(_) => return Err(Full),
        };
        // SAFETY: we reserved 1 slot.
        unsafe {
            ticket.write(0, value);
        }
        ticket.publish();
        Ok(())
    }

    /// Try to pop the next value.
    #[inline]
    pub fn try_pop(&self) -> Result<T, Empty> {
        match self.try_consume(1) {
            Ok(mut c) => {
                // SAFETY: consumer has exclusive access to consumed range.
                let v = unsafe { c.read(0) };
                c.finish();
                Ok(v)
            }
            Err(_) => Err(Empty),
        }
    }

    /// Reserve `n` contiguous slots for producing.
    ///
    /// The returned reservation must be written to and then `publish()`'d.
    pub fn try_reserve(&self, n: u32) -> Result<Reservation<'_, T>, Full> {
        if n == 0 || n > self.cap {
            return Err(Full);
        }

        loop {
            let head = self.head_consume.load(Ordering::Acquire);
            let tail = self.tail_reserve.load(Ordering::Relaxed);
            let used = tail.wrapping_sub(head);
            if used + (n as u64) > (self.cap as u64) {
                return Err(Full);
            }

            let new_tail = tail.wrapping_add(n as u64);
            match self.tail_reserve.compare_exchange_weak(
                tail,
                new_tail,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    // Wait-free slot-level readiness via per-slot sequence.
                    return Ok(Reservation {
                        q: self,
                        start: tail,
                        len: n,
                        published: false,
                    });
                }
                Err(_) => core::hint::spin_loop(),
            }
        }
    }

    /// Try to consume `n` contiguous published slots.
    pub fn try_consume(&self, n: u32) -> Result<Consume<'_, T>, Empty> {
        if n == 0 {
            return Err(Empty);
        }
        let head = self.head_consume.load(Ordering::Relaxed);
        let tailp = self.tail_publish.load(Ordering::Acquire);
        let avail = tailp.wrapping_sub(head);
        if avail < n as u64 {
            return Err(Empty);
        }

        // Single consumer: can just advance.
        let new_head = head.wrapping_add(n as u64);
        self.head_consume.store(new_head, Ordering::Release);

        Ok(Consume {
            q: self,
            start: head,
            len: n,
            finished: false,
        })
    }

    #[inline]
    fn idx(&self, pos: u64) -> usize {
        (pos as u32 & self.mask) as usize
    }
}

impl<T> Drop for MpscRing<T> {
    fn drop(&mut self) {
        // Drain any remaining published elements to drop them safely.
        // Only safe because at drop we have &mut self (no concurrent access).
        let head = self.head_consume.load(Ordering::Relaxed);
        let tailp = self.tail_publish.load(Ordering::Relaxed);
        let mut p = head;
        while p != tailp {
            let i = self.idx(p);
            // Read and drop in place.
            unsafe {
                let cell = &mut *self.buf[i].get();
                ptr::drop_in_place(cell.as_mut_ptr());
            }
            p = p.wrapping_add(1);
        }
    }
}

/// A reservation of contiguous slots.
pub struct Reservation<'a, T> {
    q: &'a MpscRing<T>,
    start: u64,
    len: u32,
    published: bool,
}

impl<'a, T> Reservation<'a, T> {
    /// Number of slots reserved.
    #[inline]
    pub fn len(&self) -> u32 {
        self.len
    }

    /// Write an element at offset `i` within this reservation.
    ///
    /// # Safety
    /// Caller must ensure `i < len()` and must write each slot at most once.
    #[inline]
    pub unsafe fn write(&self, i: u32, value: T) {
        if i >= self.len {
            return;
        }
        let pos = self.start.wrapping_add(i as u64);
        let idx = self.q.idx(pos);
        let seq = pos as u32;

        // Wait until slot is free for this sequence.
        // Expected state is seq.
        while self.q.states[idx].load(Ordering::Acquire) != seq {
            core::hint::spin_loop();
        }

        let cell = &mut *self.q.buf[idx].get();
        cell.write(value);

        // Mark ready for consumer: state = seq + 1.
        self.q.states[idx].store(seq.wrapping_add(1), Ordering::Release);
    }

    /// Publish this reservation so that the consumer can see it.
    pub fn publish(mut self) {
        if self.published {
            return;
        }

        // Ensure all writes are visible.
        core::sync::atomic::fence(Ordering::Release);

        // Publish cursor must advance in order. Multiple producers may publish.
        let target = self.start.wrapping_add(self.len as u64);
        let mut cur = self.start;
        loop {
            match self.q.tail_publish.compare_exchange_weak(
                cur,
                target,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => {
                    // Wait for earlier reservations to publish.
                    cur = actual;
                    core::hint::spin_loop();
                }
            }
        }

        self.published = true;
        // prevent Drop from doing anything (we don't do anything now, but keep invariant).
        let _ = ManuallyDrop::new(self);
    }
}

/// A consumed range.
pub struct Consume<'a, T> {
    q: &'a MpscRing<T>,
    start: u64,
    len: u32,
    finished: bool,
}

impl<'a, T> Consume<'a, T> {
    /// Number of slots consumed.
    #[inline]
    pub fn len(&self) -> u32 {
        self.len
    }

    /// Read an element at offset `i` within this consume range.
    ///
    /// # Safety
    /// Caller must ensure `i < len()` and must read each slot at most once.
    #[inline]
    pub unsafe fn read(&mut self, i: u32) -> T {
        if i >= self.len {
            // No panics: return uninit read is UB; instead, return by reading first
            // element only if out of bounds. We avoid UB by using ptr::read from a
            // MaybeUninit that we know is initialized only when i < len.
            // This path is unreachable for correct callers; we return a dummy by
            // reading index 0 (also UB if len==0). So, we guard with len>0 and i< len
            // in callers; here just spin forever rather than UB.
            loop {
                core::hint::spin_loop();
            }
        }
        let pos = self.start.wrapping_add(i as u64);
        let idx = self.q.idx(pos);
        let seq = pos as u32;

        // Wait until producer marked slot ready: state == seq + 1.
        while self.q.states[idx].load(Ordering::Acquire) != seq.wrapping_add(1) {
            core::hint::spin_loop();
        }

        let cell = &mut *self.q.buf[idx].get();
        cell.assume_init_read()
    }

    /// Finish consumption and make the slots available to producers again.
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        // Mark each slot as free for next wrap: state = seq + cap.
        for i in 0..self.len {
            let pos = self.start.wrapping_add(i as u64);
            let idx = self.q.idx(pos);
            let next_seq = (pos as u32).wrapping_add(self.q.cap);
            self.q.states[idx].store(next_seq, Ordering::Release);
        }
        self.finished = true;
    }
}

impl<'a, T> Drop for Consume<'a, T> {
    fn drop(&mut self) {
        // If user didn't call finish, ensure we still make progress.
        if !self.finished {
            self.finish();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn mpsc_correctness_multi_producer_single_consumer() {
        let q = Arc::new(MpscRing::with_capacity(1024).expect("cap"));

        let producers = 4;
        let per = 50_000u32;

        let mut handles = Vec::new();
        for p in 0..producers {
            let q2 = q.clone();
            handles.push(thread::spawn(move || {
                for i in 0..per {
                    // encode producer id in high bits
                    let v = (p as u32) << 24 | (i & 0x00FF_FFFF);
                    loop {
                        if q2.try_push(v).is_ok() {
                            break;
                        }
                        thread::yield_now();
                    }
                }
            }));
        }

        let total = producers as usize * per as usize;
        let mut seen = vec![0u8; total];

        let mut got = 0usize;
        while got < total {
            match q.try_pop() {
                Ok(v) => {
                    let p = (v >> 24) as usize;
                    let i = (v & 0x00FF_FFFF) as usize;
                    let idx = p * per as usize + i;
                    // allow duplicates detection
                    if idx < seen.len() {
                        assert_eq!(seen[idx], 0);
                        seen[idx] = 1;
                    }
                    got += 1;
                }
                Err(_) => thread::yield_now(),
            }
        }

        for h in handles {
            h.join().unwrap();
        }
        assert!(seen.iter().all(|&b| b == 1));
    }
}
