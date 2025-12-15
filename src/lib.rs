//! # bqrb - Block-based Queue Ring Buffer
//!
//! A pure Rust ring buffer implementation inspired by Tencent/BqLog's core design.
//!
//! ## Design
//!
//! - Fixed 64-byte block size aligned with cache lines
//! - Multi-producer, single-consumer architecture
//! - Lock-free operations using atomic primitives
//! - Producer workflow: `alloc_write_chunk` → write data → `commit_write_chunk`
//! - Consumer workflow: `begin_read` → repeated `read` → `end_read`
//! - Consumer batching: reads advance a private cursor; only `end_read` publishes to producers
//!
//! ## Example
//!
//! ```
//! use bqrb::{RingBuffer, ResultCode};
//!
//! // Create a ring buffer with 16 blocks (1KB)
//! let rb = RingBuffer::new(16).unwrap();
//!
//! // Producer: allocate and write
//! let handle = rb.alloc_write_chunk(100).unwrap();
//! let data = b"hello world";
//! handle.write(data);
//! rb.commit_write_chunk(handle);
//!
//! // Consumer: read in batch
//! rb.begin_read();
//! if let Ok(read_handle) = rb.read() {
//!     let data = read_handle.data();
//!     // process data...
//! }
//! rb.end_read(); // Release memory to producers
//! ```

#![warn(missing_docs)]

mod ring_buffer;

pub use ring_buffer::{ReadHandle, ResultCode, RingBuffer, WriteHandle};
