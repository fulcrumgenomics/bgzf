//! Benchmarks for the per-instance multithreaded reader/writer.
//!
//! The load-bearing comparison is **1-worker multithreaded vs single-threaded**: a
//! 1-worker `MultithreadedReader` must not be *slower* than the single-threaded `Reader`.
//! That is the regression guard for the read-ahead-depth decoupling (channels/pool sized to
//! `worker_count.max(8)`, not `worker_count`); if the `MIN_BUFFERS` floor is ever removed the
//! 1-worker numbers here will visibly regress.

use std::io::{Cursor, Read, Write};
use std::num::NonZero;

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};

use bgzf::{
    CompressionLevel, MultithreadedReader, MultithreadedWriter, Reader, Writer, BGZF_BLOCK_SIZE,
};

/// Deterministic random ACGT bases — realistic genomic-ish data that deflate compresses ~4x.
fn genomic(len: usize) -> Vec<u8> {
    const BASES: &[u8] = b"ACGT";
    let mut state: u64 = 0x0123_4567_89AB_CDEF;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        out.push(BASES[((state >> 33) & 3) as usize]);
    }
    out
}

/// BGZF-compress `data` at `level` into an in-memory blob.
fn make_bgzf(data: &[u8], level: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut writer = Writer::new(&mut out, CompressionLevel::new(level).unwrap());
    writer.write_all(data).unwrap();
    writer.finish().unwrap();
    out
}

const WORKER_COUNTS: [usize; 3] = [1, 2, 4];

/// Decode a large multi-block stream: single-threaded vs multithreaded at several worker
/// counts. `mt/1` is the one to watch — it must stay at or under `serial`.
fn bench_reader(c: &mut Criterion) {
    let size = BGZF_BLOCK_SIZE * 200; // ~13 MB uncompressed
    let data = genomic(size);

    for level in [0u8, 6] {
        // Leak so each iteration can wrap a fresh, cheap `Cursor` without recompressing.
        let blob: &'static [u8] = make_bgzf(&data, level).leak();

        let mut group = c.benchmark_group(format!("mt_reader/level{level}"));
        group.throughput(Throughput::Bytes(size as u64));

        group.bench_function("serial", |b| {
            b.iter(|| {
                let mut out = Vec::with_capacity(size);
                Reader::new(blob).read_to_end(&mut out).unwrap();
                black_box(&out);
            });
        });

        for workers in WORKER_COUNTS {
            group.bench_function(format!("mt/{workers}"), |b| {
                b.iter(|| {
                    let mut out = Vec::with_capacity(size);
                    MultithreadedReader::with_worker_count(
                        NonZero::new(workers).unwrap(),
                        Cursor::new(blob),
                    )
                    .read_to_end(&mut out)
                    .unwrap();
                    black_box(&out);
                });
            });
        }
        group.finish();
    }
}

/// Compress a large input: single-threaded vs multithreaded at several worker counts.
fn bench_writer(c: &mut Criterion) {
    let size = BGZF_BLOCK_SIZE * 200; // ~13 MB
    let data = genomic(size);

    for level in [0u8, 6] {
        let mut group = c.benchmark_group(format!("mt_writer/level{level}"));
        group.throughput(Throughput::Bytes(size as u64));

        group.bench_function("serial", |b| {
            b.iter(|| {
                let mut writer =
                    Writer::new(Vec::with_capacity(size), CompressionLevel::new(level).unwrap());
                writer.write_all(black_box(&data)).unwrap();
                black_box(writer.finish().unwrap());
            });
        });

        for workers in WORKER_COUNTS {
            group.bench_function(format!("mt/{workers}"), |b| {
                b.iter(|| {
                    let mut writer = MultithreadedWriter::with_worker_count(
                        NonZero::new(workers).unwrap(),
                        Vec::with_capacity(size),
                        CompressionLevel::new(level).unwrap(),
                    );
                    writer.write_all(black_box(&data)).unwrap();
                    black_box(writer.finish().unwrap());
                });
            });
        }
        group.finish();
    }
}

criterion_group!(benches, bench_reader, bench_writer);
criterion_main!(benches);
