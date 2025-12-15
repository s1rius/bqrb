use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;

// # Memory Ordering Strategy
//
// This ring buffer uses atomic operations for synchronization:
//
// ## Cursors (write_cursor, read_cursor, private_read_cursor)
// - Use AtomicU32 with Acquire/Release ordering
// - Provide synchronization points for visibility of buffer modifications
//
// ## Chunk Headers (ChunkHeader with atomic fields)
// - length field: AtomicU32 with Relaxed ordering
// - state field: AtomicU8 with Release (write) and Acquire (read) ordering
// - Writers use atomic stores with Release ordering on state
// - Readers use atomic loads with Acquire ordering on state
//
// ## Memory Ordering Rationale
// - state field uses Release on write and Acquire on read
//   This synchronizes the visibility of the length field and payload data
// - length field can use Relaxed because its visibility is synchronized
//   through the state field's Acquire/Release semantics
// - This follows the "synchronizes-with" relationship pattern
// - No separate fences needed; atomic operations provide sufficient ordering
//
// ## Correctness Argument
// 1. Producer claims space via compare_exchange (AcqRel) on write_cursor
// 2. Producer writes header with atomic stores (length Relaxed, state Release)
// 3. Producer writes payload + commits with atomic store (state Release)
// 4. Consumer loads write_cursor (Acquire) → sees cursor update
// 5. Consumer loads state (Acquire) → sees header + payload
//
// The ordering ensures that when a consumer reads a Committed chunk,
// all writes to that chunk (header + payload) are visible.

/// Block size in bytes, aligned with cache line size (64 bytes)
pub const BLOCK_SIZE: u32 = 64;

/// Minimum chunk header size (length + state fields)
const CHUNK_HEADER_SIZE: u32 = 8;

/// Maximum payload size that can fit in a single allocation
const MAX_PAYLOAD_SIZE: u32 = u32::MAX - CHUNK_HEADER_SIZE;

/// Special length value that marks a skip chunk (for wrap-around)
const SKIP_MARKER: u32 = 0xFFFFFFFF;

/// Result codes for ring buffer operations, aligned with BqLog semantics
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultCode {
    /// Operation completed successfully
    Success,
    /// Requested allocation size is invalid (0 or too large)
    AllocSizeInvalid,
    /// Not enough space in the ring buffer
    NotEnoughSpace,
    /// Allocation failed due to race condition with other producers
    AllocFailedByRaceCondition,
    /// Ring buffer is empty (no data to read)
    EmptyRingBuffer,
    /// Buffer was not initialized properly
    BufferNotInited,
}

/// Chunk state for commit tracking
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkState {
    /// Chunk is reserved but not yet committed
    Reserved = 0,
    /// Chunk has been committed and is ready to read
    Committed = 1,
}

/// Chunk header with atomic fields for thread-safe access
#[repr(C)]
struct ChunkHeader {
    length: AtomicU32,
    state: AtomicU8,
    _padding: [u8; 3], // Maintain 8-byte header size
}

// Compile-time assertions to ensure alignment safety for atomic operations
const _: () = {
    use std::mem::{align_of, size_of};

    // Verify that BLOCK_SIZE provides sufficient alignment for ChunkHeader
    // ChunkHeader requires 4-byte alignment (from AtomicU32)
    // BLOCK_SIZE (64) is a multiple of 4, so all chunk offsets are properly aligned
    assert!((BLOCK_SIZE as usize) % align_of::<AtomicU32>() == 0);

    // Verify ChunkHeader size matches CHUNK_HEADER_SIZE
    assert!(size_of::<ChunkHeader>() == CHUNK_HEADER_SIZE as usize);
};

/// Handle for writing data to an allocated chunk
#[derive(Debug)]
pub struct WriteHandle {
    buffer: Arc<RingBufferInner>,
    offset: u32,
    payload_size: u32,
    approximate_used_blocks: u32,
}

impl WriteHandle {
    /// Write data to the allocated chunk
    ///
    /// # Panics
    /// Panics if data.len() exceeds the allocated payload size
    pub fn write(&self, data: &[u8]) {
        assert!(
            data.len() as u32 <= self.payload_size,
            "Data size {} exceeds allocated payload size {}",
            data.len(),
            self.payload_size
        );

        let payload_offset = self.offset + CHUNK_HEADER_SIZE;
        let write_pos = payload_offset as usize;

        // Safety: offset is validated during allocation
        unsafe {
            let dst = self.buffer.data.add(write_pos);
            std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
        }
    }

    /// Get the approximate number of used blocks in the ring buffer
    /// This can be used by producers for heuristics
    pub fn approximate_used_blocks_count(&self) -> u32 {
        self.approximate_used_blocks
    }
}

/// Handle for reading data from the ring buffer
#[derive(Debug)]
pub struct ReadHandle {
    buffer: Arc<RingBufferInner>,
    offset: u32,
    length: u32,
}

impl ReadHandle {
    /// Get a slice of the data in this chunk
    pub fn data(&self) -> &[u8] {
        let payload_offset = self.offset + CHUNK_HEADER_SIZE;
        let start = payload_offset as usize;

        // Safety: offset and length are validated during read
        unsafe { std::slice::from_raw_parts(self.buffer.data.add(start), self.length as usize) }
    }

    /// Get the length of the data in this chunk
    pub fn len(&self) -> u32 {
        self.length
    }

    /// Check if this chunk is empty
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }
}

struct RingBufferInner {
    /// Raw buffer data
    data: *mut u8,
    /// Total capacity in bytes (multiple of BLOCK_SIZE)
    capacity: u32,
    /// Write cursor (producers allocate from here)
    write_cursor: AtomicU32,
    /// Read cursor (consumer reads from here, updated by end_read)
    read_cursor: AtomicU32,
    /// Private read cursor for batching (updated during read, published by end_read)
    private_read_cursor: AtomicU32,
}

unsafe impl Send for RingBufferInner {}
unsafe impl Sync for RingBufferInner {}

impl std::fmt::Debug for RingBufferInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RingBufferInner")
            .field("data", &self.data)
            .field("capacity", &self.capacity)
            .field("write_cursor", &self.write_cursor)
            .field("read_cursor", &self.read_cursor)
            .field("private_read_cursor", &self.private_read_cursor)
            .finish()
    }
}

impl Drop for RingBufferInner {
    fn drop(&mut self) {
        if !self.data.is_null() {
            unsafe {
                // Deallocate the aligned buffer
                // Safety: This layout is guaranteed to be valid by the constructor
                let layout = std::alloc::Layout::from_size_align(
                    self.capacity as usize,
                    BLOCK_SIZE as usize,
                )
                .expect("Layout should be valid since it was created in constructor");
                std::alloc::dealloc(self.data, layout);
            }
        }
    }
}

/// Block-based ring buffer with BqLog-aligned semantics
///
/// - Multi-producer, single-consumer
/// - Fixed 64-byte block size
/// - Lock-free operations using atomics
#[derive(Debug)]
pub struct RingBuffer {
    inner: Arc<RingBufferInner>,
}

impl Clone for RingBuffer {
    fn clone(&self) -> Self {
        RingBuffer {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl RingBuffer {
    /// Create a new ring buffer with the specified number of blocks
    ///
    /// # Arguments
    /// * `num_blocks` - Number of 64-byte blocks to allocate
    ///
    /// # Returns
    /// * `Ok(RingBuffer)` on success
    /// * `Err(ResultCode::BufferNotInited)` if allocation fails or num_blocks is 0
    pub fn new(num_blocks: u32) -> Result<Self, ResultCode> {
        if num_blocks == 0 {
            return Err(ResultCode::BufferNotInited);
        }

        let capacity = num_blocks * BLOCK_SIZE;

        // Allocate aligned buffer
        let data = unsafe {
            let layout =
                std::alloc::Layout::from_size_align(capacity as usize, BLOCK_SIZE as usize)
                    .map_err(|_| ResultCode::BufferNotInited)?;
            let ptr = std::alloc::alloc_zeroed(layout);
            if ptr.is_null() {
                return Err(ResultCode::BufferNotInited);
            }
            ptr
        };

        let inner = RingBufferInner {
            data,
            capacity,
            write_cursor: AtomicU32::new(0),
            read_cursor: AtomicU32::new(0),
            private_read_cursor: AtomicU32::new(0),
        };

        Ok(RingBuffer {
            inner: Arc::new(inner),
        })
    }

    /// Allocate space for writing a chunk of the specified size
    ///
    /// This reserves space in the ring buffer but does not commit it.
    /// After writing data via the returned handle, call `commit_write_chunk`.
    ///
    /// # Arguments
    /// * `size` - Size of the payload in bytes
    ///
    /// # Returns
    /// * `Ok(WriteHandle)` with the allocated space
    /// * `Err(ResultCode)` if allocation fails
    pub fn alloc_write_chunk(&self, size: u32) -> Result<WriteHandle, ResultCode> {
        // Validate size
        if size == 0 || size > MAX_PAYLOAD_SIZE {
            return Err(ResultCode::AllocSizeInvalid);
        }

        // Calculate total size including header, rounded up to block boundary
        let total_size = (size + CHUNK_HEADER_SIZE + BLOCK_SIZE - 1) & !(BLOCK_SIZE - 1);

        // Try to allocate space using CAS loop
        let mut attempts = 0;
        const MAX_ATTEMPTS: u32 = 1000;

        loop {
            attempts += 1;
            if attempts > MAX_ATTEMPTS {
                return Err(ResultCode::AllocFailedByRaceCondition);
            }

            let write_pos = self.inner.write_cursor.load(Ordering::Acquire);
            let read_pos = self.inner.read_cursor.load(Ordering::Acquire);

            // Calculate available space
            let used = if write_pos >= read_pos {
                write_pos - read_pos
            } else {
                self.inner.capacity - read_pos + write_pos
            };

            let available = self.inner.capacity - used;

            // Check if we have enough space
            if available < total_size {
                return Err(ResultCode::NotEnoughSpace);
            }

            // Handle wrap-around: if chunk doesn't fit at end, wrap to beginning
            let mut alloc_offset = write_pos;
            let space_at_end = self.inner.capacity - write_pos;
            let needs_skip_marker = space_at_end < total_size;

            let new_write_pos = if needs_skip_marker {
                // Need to wrap around
                // Check if we have space at the beginning
                if read_pos < total_size {
                    return Err(ResultCode::NotEnoughSpace);
                }
                alloc_offset = 0;
                total_size
            } else {
                write_pos + total_size
            };

            // Try to claim this space with CAS
            match self.inner.write_cursor.compare_exchange(
                write_pos,
                new_write_pos,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // Successfully allocated
                    // The successful CAS above (with AcqRel ordering) already ensures that
                    // the new write_cursor value is visible to readers.
                    unsafe {
                        // If wrapping, write skip marker at the old write_pos
                        if needs_skip_marker {
                            // SAFETY: write_pos is always a multiple of BLOCK_SIZE (64 bytes),
                            // which is greater than ChunkHeader's alignment requirement (4 bytes)
                            let skip_header =
                                &*(self.inner.data.add(write_pos as usize) as *const ChunkHeader);
                            skip_header.length.store(SKIP_MARKER, Ordering::Relaxed);
                            skip_header
                                .state
                                .store(ChunkState::Committed as u8, Ordering::Release);
                        }

                        // Write the actual chunk header at alloc_offset
                        // SAFETY: alloc_offset is always a multiple of BLOCK_SIZE (64 bytes),
                        // which is greater than ChunkHeader's alignment requirement (4 bytes)
                        let header =
                            &*(self.inner.data.add(alloc_offset as usize) as *const ChunkHeader);
                        header.length.store(size, Ordering::Relaxed);
                        header
                            .state
                            .store(ChunkState::Reserved as u8, Ordering::Release);
                    }

                    // Calculate approximate used blocks
                    let used_after = if new_write_pos >= read_pos {
                        new_write_pos - read_pos
                    } else {
                        self.inner.capacity - read_pos + new_write_pos
                    };
                    let approximate_used_blocks = (used_after + BLOCK_SIZE - 1) / BLOCK_SIZE;

                    return Ok(WriteHandle {
                        buffer: Arc::clone(&self.inner),
                        offset: alloc_offset,
                        payload_size: size,
                        approximate_used_blocks,
                    });
                }
                Err(_) => {
                    // CAS failed, retry
                    continue;
                }
            }
        }
    }

    /// Commit a previously allocated chunk, making it available for reading
    ///
    /// # Arguments
    /// * `handle` - The write handle from `alloc_write_chunk`
    ///
    /// # Memory Ordering
    /// Uses atomic store with Release ordering to ensure:
    /// - All payload writes happen-before the state change to Committed
    /// - Readers with Acquire ordering will see payload data when they see Committed state
    pub fn commit_write_chunk(&self, handle: WriteHandle) {
        unsafe {
            // SAFETY: handle.offset is always a multiple of BLOCK_SIZE (64 bytes),
            // which is greater than ChunkHeader's alignment requirement (4 bytes)
            let header = &*(handle.buffer.data.add(handle.offset as usize) as *const ChunkHeader);
            header
                .state
                .store(ChunkState::Committed as u8, Ordering::Release);
        }
    }

    /// Begin a read session
    ///
    /// This initializes the private read cursor for batching.
    /// Call `read()` repeatedly, then `end_read()` to publish the cursor.
    pub fn begin_read(&self) {
        // Initialize private cursor from public cursor
        let read_pos = self.inner.read_cursor.load(Ordering::Acquire);
        self.inner
            .private_read_cursor
            .store(read_pos, Ordering::Release);
    }

    /// Read the next chunk from the ring buffer
    ///
    /// This advances the private read cursor but does not publish it to producers.
    /// Call `end_read()` to make the space available to producers.
    ///
    /// # Returns
    /// * `Ok(ReadHandle)` with the chunk data
    /// * `Err(ResultCode::EmptyRingBuffer)` if no data is available
    pub fn read(&self) -> Result<ReadHandle, ResultCode> {
        // Loop to handle skip markers without recursion
        loop {
            let private_read_pos = self.inner.private_read_cursor.load(Ordering::Acquire);
            let write_pos = self.inner.write_cursor.load(Ordering::Acquire);

            // Check if buffer is empty
            if private_read_pos == write_pos {
                return Err(ResultCode::EmptyRingBuffer);
            }

            // Read chunk header with proper memory ordering
            // Acquire ordering on state load synchronizes with Release ordering in writer
            // This ensures we see all writes to the header and payload
            let (length, state) = unsafe {
                // SAFETY: private_read_pos is always a multiple of BLOCK_SIZE (64 bytes),
                // which is greater than ChunkHeader's alignment requirement (4 bytes)
                let header =
                    &*(self.inner.data.add(private_read_pos as usize) as *const ChunkHeader);
                let state = header.state.load(Ordering::Acquire);
                let length = header.length.load(Ordering::Relaxed);
                (length, state)
            };

            // Check if chunk is committed
            if state != ChunkState::Committed as u8 {
                // Not yet committed by producer
                return Err(ResultCode::EmptyRingBuffer);
            }

            // Check for skip marker
            if length == SKIP_MARKER {
                // This is a skip marker, wrap to the beginning and continue reading
                self.inner.private_read_cursor.store(0, Ordering::Release);
                continue;
            }

            // Validate length
            if length == 0 || length > MAX_PAYLOAD_SIZE {
                return Err(ResultCode::EmptyRingBuffer);
            }

            // Calculate total chunk size
            let total_size = (length + CHUNK_HEADER_SIZE + BLOCK_SIZE - 1) & !(BLOCK_SIZE - 1);

            // Advance private read cursor with proper wrap-around check
            let new_private_read_pos = if private_read_pos + total_size >= self.inner.capacity {
                // Wrap around to beginning
                0
            } else {
                private_read_pos + total_size
            };

            self.inner
                .private_read_cursor
                .store(new_private_read_pos, Ordering::Release);

            return Ok(ReadHandle {
                buffer: Arc::clone(&self.inner),
                offset: private_read_pos,
                length,
            });
        }
    }

    /// End the read session and publish the private read cursor
    ///
    /// This makes the read chunks' space available to producers.
    pub fn end_read(&self) {
        // Publish private cursor to public cursor
        let private_read_pos = self.inner.private_read_cursor.load(Ordering::Acquire);
        self.inner
            .read_cursor
            .store(private_read_pos, Ordering::Release);
    }

    /// Get the capacity in bytes
    pub fn capacity(&self) -> u32 {
        self.inner.capacity
    }

    /// Get the capacity in blocks
    pub fn capacity_blocks(&self) -> u32 {
        self.inner.capacity / BLOCK_SIZE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_create_ring_buffer() {
        let rb = RingBuffer::new(16).unwrap();
        assert_eq!(rb.capacity(), 16 * BLOCK_SIZE);
        assert_eq!(rb.capacity_blocks(), 16);
    }

    #[test]
    fn test_create_with_zero_blocks() {
        let result = RingBuffer::new(0);
        assert_eq!(result.unwrap_err(), ResultCode::BufferNotInited);
    }

    #[test]
    fn test_alloc_size_invalid() {
        let rb = RingBuffer::new(16).unwrap();

        // Zero size
        let result = rb.alloc_write_chunk(0);
        assert_eq!(result.unwrap_err(), ResultCode::AllocSizeInvalid);

        // Too large
        let result = rb.alloc_write_chunk(MAX_PAYLOAD_SIZE + 1);
        assert_eq!(result.unwrap_err(), ResultCode::AllocSizeInvalid);
    }

    #[test]
    fn test_single_threaded_roundtrip() {
        let rb = RingBuffer::new(16).unwrap();

        // Write some data
        let handle = rb.alloc_write_chunk(100).unwrap();
        let data = b"Hello, world!";
        handle.write(data);
        rb.commit_write_chunk(handle);

        // Read it back
        rb.begin_read();
        let read_handle = rb.read().unwrap();
        assert_eq!(read_handle.len(), 100);
        assert_eq!(&read_handle.data()[..data.len()], data);
        rb.end_read();

        // Buffer should be empty now
        rb.begin_read();
        let result = rb.read();
        assert_eq!(result.unwrap_err(), ResultCode::EmptyRingBuffer);
        rb.end_read();
    }

    #[test]
    fn test_empty_ring_buffer() {
        let rb = RingBuffer::new(16).unwrap();

        rb.begin_read();
        let result = rb.read();
        assert_eq!(result.unwrap_err(), ResultCode::EmptyRingBuffer);
        rb.end_read();
    }

    #[test]
    fn test_not_enough_space() {
        // Create a small buffer and fill it
        let rb = RingBuffer::new(4).unwrap(); // 256 bytes

        // Allocate and commit first chunk (takes 128 bytes)
        let h1 = rb.alloc_write_chunk(100).unwrap();
        h1.write(b"data");
        rb.commit_write_chunk(h1);

        // Allocate and commit second chunk (takes another 128 bytes, now full)
        let h2 = rb.alloc_write_chunk(100).unwrap();
        h2.write(b"data");
        rb.commit_write_chunk(h2);

        // Try to allocate more - should fail with not enough space
        let result = rb.alloc_write_chunk(50);
        assert_eq!(result.unwrap_err(), ResultCode::NotEnoughSpace);

        // Now read one chunk to free space
        rb.begin_read();
        let _ = rb.read().unwrap();
        rb.end_read();

        // Should succeed now
        let h3 = rb.alloc_write_chunk(50).unwrap();
        rb.commit_write_chunk(h3);
    }

    #[test]
    fn test_batching_semantics() {
        let rb = RingBuffer::new(16).unwrap();

        // Write two chunks
        let h1 = rb.alloc_write_chunk(50).unwrap();
        h1.write(b"chunk1");
        rb.commit_write_chunk(h1);

        let h2 = rb.alloc_write_chunk(50).unwrap();
        h2.write(b"chunk2");
        rb.commit_write_chunk(h2);

        // Begin read but don't end it
        rb.begin_read();
        let r1 = rb.read().unwrap();
        assert_eq!(&r1.data()[..6], b"chunk1");

        // Space should NOT be released yet
        // This should work because we haven't called end_read yet
        let r2 = rb.read().unwrap();
        assert_eq!(&r2.data()[..6], b"chunk2");

        // Now end read to publish cursor
        rb.end_read();

        // Now space should be available for new writes
        let h3 = rb.alloc_write_chunk(50).unwrap();
        h3.write(b"chunk3");
        rb.commit_write_chunk(h3);
    }

    #[test]
    fn test_multi_producer_single_consumer() {
        let rb = Arc::new(RingBuffer::new(64).unwrap());
        let num_producers = 4;
        let chunks_per_producer = 10;

        // Spawn producer threads
        let mut handles = vec![];
        for i in 0..num_producers {
            let rb_clone = Arc::clone(&rb);
            let handle = thread::spawn(move || {
                for j in 0..chunks_per_producer {
                    let data = format!("producer_{}_chunk_{}", i, j);
                    loop {
                        match rb_clone.alloc_write_chunk(data.len() as u32) {
                            Ok(write_handle) => {
                                write_handle.write(data.as_bytes());
                                rb_clone.commit_write_chunk(write_handle);
                                break;
                            }
                            Err(ResultCode::NotEnoughSpace) => {
                                // Wait a bit for consumer to free space
                                thread::sleep(std::time::Duration::from_micros(10));
                            }
                            Err(ResultCode::AllocFailedByRaceCondition) => {
                                // Retry immediately
                                continue;
                            }
                            Err(e) => panic!("Unexpected error: {:?}", e),
                        }
                    }
                }
            });
            handles.push(handle);
        }

        // Consumer: read all chunks
        let mut count = 0;
        let expected_total = num_producers * chunks_per_producer;

        while count < expected_total {
            rb.begin_read();
            loop {
                match rb.read() {
                    Ok(read_handle) => {
                        // Verify data format
                        let data = std::str::from_utf8(read_handle.data()).unwrap();
                        assert!(data.starts_with("producer_"));
                        count += 1;
                    }
                    Err(ResultCode::EmptyRingBuffer) => break,
                    Err(e) => panic!("Unexpected error: {:?}", e),
                }
            }
            rb.end_read();

            if count < expected_total {
                thread::sleep(std::time::Duration::from_micros(100));
            }
        }

        // Wait for all producers
        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(count, expected_total);
    }

    #[test]
    fn test_approximate_used_blocks_count() {
        let rb = RingBuffer::new(16).unwrap();

        let handle = rb.alloc_write_chunk(100).unwrap();
        let used_blocks = handle.approximate_used_blocks_count();

        // 100 bytes + 8 byte header = 108 bytes
        // Rounded up to 64-byte blocks = 128 bytes = 2 blocks
        assert!(used_blocks >= 2);
    }

    #[test]
    fn test_wrap_around() {
        // Create a small buffer to easily trigger wrap-around
        // 8 blocks = 512 bytes
        let rb = RingBuffer::new(8).unwrap();

        // Write a chunk that takes up most of the buffer
        // 300 bytes + 8 header = 308 bytes, rounded to 320 bytes (5 blocks)
        let h1 = rb.alloc_write_chunk(300).unwrap();
        let data1 = b"First chunk that takes most of the buffer";
        h1.write(data1);
        rb.commit_write_chunk(h1);

        // Write another chunk that should fit in the remaining space
        // 100 bytes + 8 header = 108 bytes, rounded to 128 bytes (2 blocks)
        let h2 = rb.alloc_write_chunk(100).unwrap();
        let data2 = b"Second chunk";
        h2.write(data2);
        rb.commit_write_chunk(h2);

        // Read the first chunk to free space
        rb.begin_read();
        let r1 = rb.read().unwrap();
        assert_eq!(r1.len(), 300);
        assert_eq!(&r1.data()[..data1.len()], data1);
        rb.end_read();

        // Now write a chunk that will trigger wrap-around
        // This should wrap to position 0 and write a skip marker
        let h3 = rb.alloc_write_chunk(200).unwrap();
        let data3 = b"Third chunk triggers wrap";
        h3.write(data3);
        rb.commit_write_chunk(h3);

        // Read second chunk
        rb.begin_read();
        let r2 = rb.read().unwrap();
        assert_eq!(r2.len(), 100);
        assert_eq!(&r2.data()[..data2.len()], data2);

        // Read third chunk - should handle skip marker and wrap around
        let r3 = rb.read().unwrap();
        assert_eq!(r3.len(), 200);
        assert_eq!(&r3.data()[..data3.len()], data3);
        rb.end_read();

        // Buffer should be empty now
        rb.begin_read();
        let result = rb.read();
        assert_eq!(result.unwrap_err(), ResultCode::EmptyRingBuffer);
        rb.end_read();
    }

    #[test]
    fn test_clone() {
        let rb = RingBuffer::new(16).unwrap();

        // Clone the ring buffer
        let rb_clone = rb.clone();

        // Write via original
        let h1 = rb.alloc_write_chunk(50).unwrap();
        h1.write(b"test data");
        rb.commit_write_chunk(h1);

        // Read via clone
        rb_clone.begin_read();
        let r1 = rb_clone.read().unwrap();
        assert_eq!(&r1.data()[..9], b"test data");
        rb_clone.end_read();

        // Should be empty now from both
        rb.begin_read();
        let result = rb.read();
        assert_eq!(result.unwrap_err(), ResultCode::EmptyRingBuffer);
        rb.end_read();
    }
}
