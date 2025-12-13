#![allow(clippy::missing_safety_doc)]

use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT_US: u32 = 100;

// Header is 8 bytes: u32 len + u32 state
const HDR_LEN: u32 = 8;
const STATE_FREE: u32 = 0;
const STATE_USED: u32 = 1;
const STATE_INVALID: u32 = 2;

#[inline]
fn align8(x: u32) -> u32 {
    (x + 7) & !7
}

#[inline]
fn now_deadline(timeout_us: u32) -> Instant {
    let t = if timeout_us == 0 {
        DEFAULT_TIMEOUT_US
    } else {
        timeout_us
    };
    Instant::now() + Duration::from_micros(t as u64)
}

#[inline]
fn spin_wait(deadline: Instant, mut spins: u32) -> bool {
    // returns false if timed out
    while Instant::now() < deadline {
        if spins < 32 {
            core::hint::spin_loop();
            spins += 1;
        } else {
            std::thread::yield_now();
        }
        return true;
    }
    false
}

pub struct RingBuffer {
    cap: u32,
    buf: Box<[UnsafeCell<u8>]>,
    // head: producer allocation cursor
    head: AtomicU32,
    // tail: consumer cursor (single consumer)
    tail: AtomicU32,
}

unsafe impl Send for RingBuffer {}
unsafe impl Sync for RingBuffer {}

#[derive(Copy, Clone, Debug)]
pub struct WriteHandle {
    pub ptr: *mut u8,
    pub len: u32,
    offset: u32,
}

#[derive(Copy, Clone, Debug)]
pub struct ReadHandle {
    pub ptr: *const u8,
    pub len: u32,
    offset: u32,
}

impl RingBuffer {
    pub fn new(capacity: u32) -> Self {
        assert!(capacity > HDR_LEN + 8);
        // ensure cap is multiple of 8 for header alignment
        let cap = align8(capacity);
        let mut v = Vec::with_capacity(cap as usize);
        v.resize_with(cap as usize, || UnsafeCell::new(0u8));
        Self {
            cap,
            buf: v.into_boxed_slice(),
            head: AtomicU32::new(0),
            tail: AtomicU32::new(0),
        }
    }

    #[inline]
    fn buf_ptr(&self) -> *mut u8 {
        self.buf.as_ptr() as *mut UnsafeCell<u8> as *mut u8
    }

    #[inline]
    unsafe fn write_u32(&self, off: u32, v: u32) {
        let p = self.buf_ptr().add(off as usize) as *mut u32;
        ptr::write_unaligned(p, v);
    }

    #[inline]
    unsafe fn read_u32(&self, off: u32) -> u32 {
        let p = self.buf_ptr().add(off as usize) as *const u32;
        ptr::read_unaligned(p)
    }

    #[inline]
    fn used_bytes(head: u32, tail: u32, cap: u32) -> u32 {
        head.wrapping_sub(tail) % cap
    }

    // allocate a contiguous region; may insert INVALID padding header on wrap.
    pub fn begin_write(&self, len: u32, timeout_us: u32) -> Option<WriteHandle> {
        if len == 0 {
            return None;
        }
        let need = align8(HDR_LEN + len);
        if need >= self.cap {
            return None;
        }

        let deadline = now_deadline(timeout_us);
        let mut spins = 0u32;

        loop {
            let head0 = self.head.load(Ordering::Relaxed);
            let tail = self.tail.load(Ordering::Acquire);
            let used = Self::used_bytes(head0, tail, self.cap);
            let free = self.cap - used;
            if free <= need {
                if !spin_wait(deadline, spins) {
                    return None;
                }
                spins = spins.saturating_add(1);
                continue;
            }

            let off = head0 % self.cap;
            let to_end = self.cap - off;

            if need <= to_end {
                // allocate [head0, head0+need)
                let head1 = head0.wrapping_add(need);
                let prev = self.head.fetch_add(need, Ordering::AcqRel);
                // fetch_add winner check
                if prev != head0 {
                    // rollback attempt: only if no one else advanced beyond our reservation point
                    let expected = prev.wrapping_add(need);
                    let _ = self.head.compare_exchange(expected, prev, Ordering::AcqRel, Ordering::Relaxed);
                    continue;
                }

                unsafe {
                    // write len first
                    self.write_u32(off, len);
                    // state stays FREE until commit/cancel
                    self.write_u32(off + 4, STATE_FREE);
                }

                let ptr = unsafe { self.buf_ptr().add((off + HDR_LEN) as usize) };
                return Some(WriteHandle { ptr, len, offset: off });
            } else {
                // need wrap: we will create an INVALID padding chunk covering to_end
                // check we have room for padding + need
                if free <= to_end + need {
                    if !spin_wait(deadline, spins) {
                        return None;
                    }
                    spins = spins.saturating_add(1);
                    continue;
                }

                let total = to_end + need;
                let head1 = head0.wrapping_add(total);
                let prev = self.head.fetch_add(total, Ordering::AcqRel);
                if prev != head0 {
                    let expected = prev.wrapping_add(total);
                    let _ = self.head.compare_exchange(expected, prev, Ordering::AcqRel, Ordering::Relaxed);
                    continue;
                }

                // write INVALID padding at current off
                unsafe {
                    self.write_u32(off, to_end - HDR_LEN);
                    self.write_u32(off + 4, STATE_INVALID);
                }

                // actual chunk at offset 0
                unsafe {
                    self.write_u32(0, len);
                    self.write_u32(4, STATE_FREE);
                }

                let ptr = unsafe { self.buf_ptr().add(HDR_LEN as usize) };
                return Some(WriteHandle { ptr, len, offset: 0 });
            }
        }
    }

    pub fn commit_write(&self, w: &WriteHandle) {
        // Release so consumer sees payload after observing USED.
        unsafe { self.write_u32(w.offset + 4, STATE_USED) };
        core::sync::atomic::fence(Ordering::Release);
    }

    pub fn cancel_write(&self, w: &WriteHandle) {
        unsafe { self.write_u32(w.offset + 4, STATE_INVALID) };
        core::sync::atomic::fence(Ordering::Release);
    }

    pub fn begin_read(&self, timeout_us: u32) -> Option<ReadHandle> {
        let deadline = now_deadline(timeout_us);
        let mut spins = 0u32;

        loop {
            let tail0 = self.tail.load(Ordering::Relaxed);
            let head = self.head.load(Ordering::Acquire);
            if tail0 == head {
                if !spin_wait(deadline, spins) {
                    return None;
                }
                spins = spins.saturating_add(1);
                continue;
            }

            let off = tail0 % self.cap;
            // read header
            let len = unsafe { self.read_u32(off) };
            let state = unsafe { self.read_u32(off + 4) };

            if state == STATE_FREE {
                if !spin_wait(deadline, spins) {
                    return None;
                }
                spins = spins.saturating_add(1);
                continue;
            }

            // Acquire: ensure payload visibility after seeing USED/INVALID.
            core::sync::atomic::fence(Ordering::Acquire);

            let block = align8(HDR_LEN + len);

            if state == STATE_INVALID {
                // skip invalid
                self.tail.store(tail0.wrapping_add(block), Ordering::Release);
                continue;
            }
            if state != STATE_USED {
                // unknown state; treat as not ready
                if !spin_wait(deadline, spins) {
                    return None;
                }
                spins = spins.saturating_add(1);
                continue;
            }

            let ptr = unsafe { self.buf_ptr().add((off + HDR_LEN) as usize) as *const u8 };
            return Some(ReadHandle { ptr, len, offset: off });
        }
    }

    pub fn end_read(&self, r: &ReadHandle) {
        let block = align8(HDR_LEN + r.len);
        let tail0 = self.tail.load(Ordering::Relaxed);
        let off = tail0 % self.cap;
        debug_assert_eq!(off, r.offset);
        // mark FREE for debug clarity (not required)
        unsafe {
            self.write_u32(off + 4, STATE_FREE);
        }
        self.tail.store(tail0.wrapping_add(block), Ordering::Release);
    }

    pub fn read_into(&self, dst: &mut [u8], timeout_us: u32) -> usize {
        let Some(r) = self.begin_read(timeout_us) else {
            return 0;
        };
        let n = core::cmp::min(dst.len(), r.len as usize);
        unsafe {
            ptr::copy_nonoverlapping(r.ptr, dst.as_mut_ptr(), n);
        }
        self.end_read(&r);
        n
    }
}

// ---------------- C ABI ----------------

#[repr(C)]
pub struct bqrb {
    inner: RingBuffer,
}

#[repr(C)]
pub struct bqrb_write {
    pub ptr: *mut u8,
    pub len: u32,
    pub _offset: u32,
}

#[repr(C)]
pub struct bqrb_read {
    pub ptr: *const u8,
    pub len: u32,
    pub _offset: u32,
}

#[no_mangle]
pub extern "C" fn bqrb_new(capacity: u32) -> *mut bqrb {
    let rb = bqrb { inner: RingBuffer::new(capacity) };
    Box::into_raw(Box::new(rb))
}

#[no_mangle]
pub extern "C" fn bqrb_free(rb: *mut bqrb) {
    if rb.is_null() {
        return;
    }
    unsafe { drop(Box::from_raw(rb)) };
}

#[no_mangle]
pub extern "C" fn bqrb_begin_write(
    rb: *mut bqrb,
    len: u32,
    timeout_us: u32,
    out_w: *mut bqrb_write,
) -> bool {
    if rb.is_null() || out_w.is_null() {
        return false;
    }
    let inner = unsafe { &(*rb).inner };
    let Some(w) = inner.begin_write(len, timeout_us) else {
        return false;
    };
    unsafe {
        (*out_w).ptr = w.ptr;
        (*out_w).len = w.len;
        (*out_w)._offset = w.offset;
    }
    true
}

#[no_mangle]
pub extern "C" fn bqrb_commit_write(rb: *mut bqrb, w: *const bqrb_write) {
    if rb.is_null() || w.is_null() {
        return;
    }
    let inner = unsafe { &(*rb).inner };
    let wh = WriteHandle { ptr: unsafe { (*w).ptr }, len: unsafe { (*w).len }, offset: unsafe { (*w)._offset } };
    inner.commit_write(&wh);
}

#[no_mangle]
pub extern "C" fn bqrb_cancel_write(rb: *mut bqrb, w: *const bqrb_write) {
    if rb.is_null() || w.is_null() {
        return;
    }
    let inner = unsafe { &(*rb).inner };
    let wh = WriteHandle { ptr: unsafe { (*w).ptr }, len: unsafe { (*w).len }, offset: unsafe { (*w)._offset } };
    inner.cancel_write(&wh);
}

#[no_mangle]
pub extern "C" fn bqrb_begin_read(rb: *mut bqrb, timeout_us: u32, out_r: *mut bqrb_read) -> bool {
    if rb.is_null() || out_r.is_null() {
        return false;
    }
    let inner = unsafe { &(*rb).inner };
    let Some(r) = inner.begin_read(timeout_us) else {
        return false;
    };
    unsafe {
        (*out_r).ptr = r.ptr;
        (*out_r).len = r.len;
        (*out_r)._offset = r.offset;
    }
    true
}

#[no_mangle]
pub extern "C" fn bqrb_end_read(rb: *mut bqrb, r: *const bqrb_read) {
    if rb.is_null() || r.is_null() {
        return;
    }
    let inner = unsafe { &(*rb).inner };
    let rh = ReadHandle { ptr: unsafe { (*r).ptr }, len: unsafe { (*r).len }, offset: unsafe { (*r)._offset } };
    inner.end_read(&rh);
}

#[no_mangle]
pub extern "C" fn bqrb_read(rb: *mut bqrb, dst: *mut u8, dst_len: u32, timeout_us: u32) -> u32 {
    if rb.is_null() || dst.is_null() {
        return 0;
    }
    let inner = unsafe { &(*rb).inner };
    let mut v = vec![0u8; dst_len as usize];
    let n = inner.read_into(&mut v, timeout_us);
    unsafe {
        ptr::copy_nonoverlapping(v.as_ptr(), dst, n);
    }
    n as u32
}
