use bqrb::{ResultCode, RingBuffer};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn benchmark_single_threaded(num_operations: usize, chunk_size: u32) -> Duration {
    let rb = RingBuffer::new(256).unwrap();
    let data = vec![0u8; chunk_size as usize];

    let start = Instant::now();

    for _ in 0..num_operations {
        // Write
        let handle = rb.alloc_write_chunk(chunk_size).unwrap();
        handle.write(&data);
        rb.commit_write_chunk(handle);

        // Read
        rb.begin_read();
        let _ = rb.read().unwrap();
        rb.end_read();
    }

    start.elapsed()
}

fn benchmark_multi_producer(
    num_producers: usize,
    operations_per_producer: usize,
    chunk_size: u32,
) -> Duration {
    let rb = Arc::new(RingBuffer::new(1024).unwrap());
    let data = vec![0u8; chunk_size as usize];

    let start = Instant::now();

    // Spawn producer threads
    let mut producer_handles = vec![];
    for _ in 0..num_producers {
        let rb_clone = Arc::clone(&rb);
        let data_clone = data.clone();
        let handle = thread::spawn(move || {
            for _ in 0..operations_per_producer {
                loop {
                    match rb_clone.alloc_write_chunk(chunk_size) {
                        Ok(write_handle) => {
                            write_handle.write(&data_clone);
                            rb_clone.commit_write_chunk(write_handle);
                            break;
                        }
                        Err(ResultCode::NotEnoughSpace) => {
                            thread::yield_now();
                        }
                        Err(ResultCode::AllocFailedByRaceCondition) => {
                            continue;
                        }
                        Err(e) => panic!("Unexpected error: {:?}", e),
                    }
                }
            }
        });
        producer_handles.push(handle);
    }

    // Consumer thread
    let rb_consumer = Arc::clone(&rb);
    let total_expected = num_producers * operations_per_producer;
    let consumer_handle = thread::spawn(move || {
        let mut count = 0;
        while count < total_expected {
            rb_consumer.begin_read();
            loop {
                match rb_consumer.read() {
                    Ok(_) => count += 1,
                    Err(ResultCode::EmptyRingBuffer) => break,
                    Err(e) => panic!("Unexpected error: {:?}", e),
                }
            }
            rb_consumer.end_read();
        }
    });

    // Wait for all threads
    for handle in producer_handles {
        handle.join().unwrap();
    }
    consumer_handle.join().unwrap();

    start.elapsed()
}

fn main() {
    println!("=== Ring Buffer Throughput Benchmark ===\n");

    // Single-threaded benchmarks
    println!("Single-threaded (1M operations):");
    for chunk_size in [64, 256, 1024] {
        let duration = benchmark_single_threaded(1_000_000, chunk_size);
        let ops_per_sec = 1_000_000.0 / duration.as_secs_f64();
        let throughput_mb = (ops_per_sec * chunk_size as f64) / (1024.0 * 1024.0);
        println!(
            "  Chunk size {}: {:.2} M ops/sec, {:.2} MB/sec",
            chunk_size,
            ops_per_sec / 1_000_000.0,
            throughput_mb
        );
    }

    println!("\nMulti-producer (4 producers, 250K ops each):");
    for chunk_size in [64, 256, 1024] {
        let duration = benchmark_multi_producer(4, 250_000, chunk_size);
        let total_ops = 4 * 250_000;
        let ops_per_sec = total_ops as f64 / duration.as_secs_f64();
        let throughput_mb = (ops_per_sec * chunk_size as f64) / (1024.0 * 1024.0);
        println!(
            "  Chunk size {}: {:.2} M ops/sec, {:.2} MB/sec",
            chunk_size,
            ops_per_sec / 1_000_000.0,
            throughput_mb
        );
    }
}
