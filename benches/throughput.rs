use bqrb::RingBuffer;
use criterion::{criterion_group, criterion_main, Criterion};

fn bench_throughput(c: &mut Criterion) {
    let rb = RingBuffer::new(1 << 20);

    c.bench_function("mp_sc_chunk_roundtrip_64b", |b| {
        b.iter(|| {
            let w = rb.begin_write(64, 0).unwrap();
            unsafe { std::ptr::write_bytes(w.ptr, 0x11, 64) };
            rb.commit_write(&w);
            let r = rb.begin_read(0).unwrap();
            rb.end_read(&r);
        })
    });
}

criterion_group!(benches, bench_throughput);
criterion_main!(benches);
