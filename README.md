# bqrb - Block-based Queue Ring Buffer

A pure Rust ring buffer implementation inspired by [Tencent/BqLog](https://github.com/Tencent/BqLog)'s core design.

## Features

- **Pure Rust** - No FFI, no C dependencies
- **Lock-free** - Multi-producer, single-consumer using atomic operations
- **Cache-aligned** - Fixed 64-byte block size aligned with CPU cache lines
- **BqLog-inspired** - Replicates core semantics from Tencent/BqLog ring buffer

## Design

The ring buffer follows BqLog's design principles:

- **Block-based allocation**: All allocations are rounded up to 64-byte blocks (cache line size)
- **Multi-producer, single-consumer**: Multiple threads can write concurrently; one thread reads
- **Producer workflow**: `alloc_write_chunk()` → write data → `commit_write_chunk()`
- **Consumer workflow**: `begin_read()` → repeated `read()` → `end_read()`
- **Consumer batching**: Reads advance a private cursor; only `end_read()` publishes the cursor back to producers

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
bqrb = "0.1"
```

### Basic Example

```rust
use bqrb::{RingBuffer, ResultCode};

// Create a ring buffer with 16 blocks (1KB)
let rb = RingBuffer::new(16).unwrap();

// Producer: allocate and write
let handle = rb.alloc_write_chunk(100).unwrap();
let data = b"hello world";
handle.write(data);
rb.commit_write_chunk(handle);

// Consumer: read in batch
rb.begin_read();
if let Ok(read_handle) = rb.read() {
    let data = read_handle.data();
    println!("Read {} bytes", data.len());
}
rb.end_read(); // Release memory to producers
```

### Multi-Producer Example

```rust
use bqrb::{RingBuffer, ResultCode};
use std::sync::Arc;
use std::thread;

let rb = Arc::new(RingBuffer::new(64).unwrap());

// Spawn multiple producer threads
for i in 0..4 {
    let rb_clone = Arc::clone(&rb);
    thread::spawn(move || {
        for j in 0..100 {
            let data = format!("producer {} message {}", i, j);
            loop {
                match rb_clone.alloc_write_chunk(data.len() as u32) {
                    Ok(handle) => {
                        handle.write(data.as_bytes());
                        rb_clone.commit_write_chunk(handle);
                        break;
                    }
                    Err(ResultCode::NotEnoughSpace) => {
                        // Wait for consumer to free space
                        thread::yield_now();
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
}

// Consumer reads in batches
loop {
    rb.begin_read();
    let mut batch_count = 0;
    while let Ok(read_handle) = rb.read() {
        // Process data
        batch_count += 1;
    }
    rb.end_read(); // Publish cursor to producers
    
    if batch_count == 0 {
        break; // No more data
    }
}
```

## API Reference

### `RingBuffer::new(num_blocks: u32) -> Result<RingBuffer, ResultCode>`

Create a new ring buffer with the specified number of 64-byte blocks.

### `alloc_write_chunk(size: u32) -> Result<WriteHandle, ResultCode>`

Allocate space for writing. Returns a handle for writing data.

**Result Codes**:
- `Success` - Allocation succeeded
- `AllocSizeInvalid` - Size is 0 or too large
- `NotEnoughSpace` - Buffer is full
- `AllocFailedByRaceCondition` - Too much contention, retry

### `commit_write_chunk(handle: WriteHandle)`

Commit a previously allocated chunk, making it available for reading.

### `begin_read()`

Begin a read session. Initializes the private read cursor.

### `read() -> Result<ReadHandle, ResultCode>`

Read the next chunk. Advances the private cursor but does not release memory to producers.

**Result Codes**:
- `Success` - Read succeeded
- `EmptyRingBuffer` - No data available

### `end_read()`

End the read session. Publishes the private cursor, releasing memory to producers.

### `WriteHandle::approximate_used_blocks_count() -> u32`

Get the approximate number of used blocks. Useful for producer-side heuristics.

## Result Codes

The library uses BqLog-aligned result codes:

- `Success` - Operation completed successfully
- `AllocSizeInvalid` - Requested size is invalid (0 or too large)
- `NotEnoughSpace` - Not enough space in the ring buffer
- `AllocFailedByRaceCondition` - Allocation failed due to contention, retry
- `EmptyRingBuffer` - Ring buffer is empty (no data to read)
- `BufferNotInited` - Buffer initialization failed

## Performance

The ring buffer is optimized for high throughput:

- Cache-line aligned allocations (64 bytes)
- Lock-free atomic operations
- Minimal synchronization overhead
- Batched reads reduce cursor updates

Run benchmarks with:

```bash
cargo bench --bench throughput
```

## Differences from BqLog

This implementation focuses on the **core ring buffer semantics** from BqLog:

- ✅ Block-based allocation (64-byte blocks)
- ✅ Multi-producer, single-consumer
- ✅ Producer: `alloc_write_chunk` → write → `commit_write_chunk`
- ✅ Consumer: `begin_read` → `read` → `end_read` with batching
- ✅ Result codes aligned with BqLog
- ✅ `approximate_used_blocks_count` for heuristics
- ❌ mmap/persistence support (future work)
- ❌ serialize_id recovery (future work)

## License

MIT OR Apache-2.0
