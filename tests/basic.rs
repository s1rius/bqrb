use bqrb::RingBuffer;

#[test]
fn basic_single_thread_roundtrip() {
    let rb = RingBuffer::new(1024);

    let w = rb.begin_write(5, 0).expect("write");
    unsafe {
        std::ptr::copy_nonoverlapping(b"hello".as_ptr(), w.ptr, 5);
    }
    rb.commit_write(&w);

    let r = rb.begin_read(0).expect("read");
    assert_eq!(r.len, 5);
    let mut out = [0u8; 5];
    unsafe { std::ptr::copy_nonoverlapping(r.ptr, out.as_mut_ptr(), 5) };
    assert_eq!(&out, b"hello");
    rb.end_read(&r);
}

#[test]
fn wrap_inserts_invalid_padding() {
    let rb = RingBuffer::new(128);

    // Consume space to force wrap for next allocation.
    let w1 = rb.begin_write(64, 0).unwrap();
    unsafe { std::ptr::write_bytes(w1.ptr, 0xAA, 64) };
    rb.commit_write(&w1);
    let r1 = rb.begin_read(0).unwrap();
    rb.end_read(&r1);

    // This should wrap and be readable.
    let w2 = rb.begin_write(16, 0).unwrap();
    unsafe { std::ptr::write_bytes(w2.ptr, 0xBB, 16) };
    rb.commit_write(&w2);
    let r2 = rb.begin_read(0).unwrap();
    assert_eq!(r2.len, 16);
    rb.end_read(&r2);
}
