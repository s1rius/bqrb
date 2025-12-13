# bqrb

A high-performance **MPSC (multi-producer / single-consumer)** chunk ring buffer with a stable **C ABI**.

## Design

- Ring buffer stores variable-sized **contiguous chunks**.
- Producers allocate space by advancing a **u32 cursor** (`head`) using atomic `fetch_add`.
- If an allocation would wrap, a producer inserts an **INVALID padding block** up to the end of the buffer, then allocates at offset 0.
- Each chunk has an 8-byte header: `len: u32`, `state: u32` where state is `FREE/USED/INVALID`.
- `commit` publishes by writing `USED` with **Release** ordering.
- `cancel` publishes `INVALID` with **Release** ordering.
- Consumer reads header and state; when observing `USED/INVALID` it performs an **Acquire** fence before reading payload.
- `timeout_us` is a microsecond budget (default 100us) with a spin-then-yield backoff.

## Rust API

```rust
use bqrb::RingBuffer;

let rb = RingBuffer::new(1024);

let w = rb.begin_write(5, 0).unwrap();
unsafe { std::ptr::copy_nonoverlapping(b"hello".as_ptr(), w.ptr, 5); }
rb.commit_write(&w);

let r = rb.begin_read(0).unwrap();
// read r.ptr..r.ptr+r.len
rb.end_read(&r);
```

## C API

See `include/bqrb.h`.

Build:

```bash
cargo build --release
```

Link against the produced `libbqrb.{so|dylib|dll}`.
