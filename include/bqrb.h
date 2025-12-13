#pragma once

#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct bqrb bqrb_t;

typedef struct bqrb_write {
    uint8_t* ptr;
    uint32_t len;
    uint32_t _offset;
} bqrb_write_t;

typedef struct bqrb_read {
    const uint8_t* ptr;
    uint32_t len;
    uint32_t _offset;
} bqrb_read_t;

// Create a ring buffer with capacity bytes (must be > 0).
// On wrap-around, a contiguous allocation may leave an INVALID padding block.
// Returns NULL on allocation failure.
bqrb_t* bqrb_new(uint32_t capacity);

// Free a ring buffer.
void bqrb_free(bqrb_t* rb);

// Begin a contiguous write allocation of `len` bytes.
// timeout_us is a spin/yield budget in microseconds; pass 0 for default (100us).
// Returns true and fills out_w on success.
bool bqrb_begin_write(bqrb_t* rb, uint32_t len, uint32_t timeout_us, bqrb_write_t* out_w);

// Commit a previously begun write.
void bqrb_commit_write(bqrb_t* rb, const bqrb_write_t* w);

// Cancel/invalidate a previously begun write.
void bqrb_cancel_write(bqrb_t* rb, const bqrb_write_t* w);

// Begin reading the next available chunk.
// timeout_us is a spin/yield budget in microseconds; pass 0 for default (100us).
// Returns true and fills out_r on success.
bool bqrb_begin_read(bqrb_t* rb, uint32_t timeout_us, bqrb_read_t* out_r);

// Finish reading the current chunk and advance.
void bqrb_end_read(bqrb_t* rb, const bqrb_read_t* r);

// Convenience: read one chunk; returns len copied (<= dst_len) or 0 if no chunk within timeout.
uint32_t bqrb_read(bqrb_t* rb, uint8_t* dst, uint32_t dst_len, uint32_t timeout_us);

#ifdef __cplusplus
}
#endif
