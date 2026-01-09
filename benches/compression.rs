use std::io::{BufWriter, Write};

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use tempfile::tempdir;

use bgzf::{CompressionLevel, Compressor, Writer, BGZF_BLOCK_SIZE};

fn bench_compressor_single_block(c: &mut Criterion) {
    let input = vec![b'A'; BGZF_BLOCK_SIZE];
    let mut compressor = Compressor::new(CompressionLevel::new(6).unwrap());
    let mut buffer = Vec::with_capacity(BGZF_BLOCK_SIZE + 1024);

    let mut group = c.benchmark_group("compressor");
    group.throughput(Throughput::Bytes(input.len() as u64));

    group.bench_function("single_block_level6", |b| {
        b.iter(|| {
            buffer.clear();
            compressor.compress(black_box(&input), &mut buffer).unwrap();
            black_box(&buffer);
        })
    });

    group.finish();
}

fn bench_compressor_levels(c: &mut Criterion) {
    let input = vec![b'A'; BGZF_BLOCK_SIZE];
    let mut buffer = Vec::with_capacity(BGZF_BLOCK_SIZE + 1024);

    let mut group = c.benchmark_group("compressor_levels");
    group.throughput(Throughput::Bytes(input.len() as u64));

    for level in [1, 6, 9, 12] {
        let mut compressor = Compressor::new(CompressionLevel::new(level).unwrap());
        group.bench_function(format!("level_{}", level), |b| {
            b.iter(|| {
                buffer.clear();
                compressor.compress(black_box(&input), &mut buffer).unwrap();
                black_box(&buffer);
            })
        });
    }

    group.finish();
}

fn bench_writer_throughput(c: &mut Criterion) {
    let input: Vec<u8> = (0..BGZF_BLOCK_SIZE * 10).map(|i| (i % 256) as u8).collect();

    let mut group = c.benchmark_group("writer");
    group.throughput(Throughput::Bytes(input.len() as u64));

    group.bench_function("write_650kb", |b| {
        b.iter(|| {
            let mut output = Vec::with_capacity(input.len());
            let mut writer = Writer::new(&mut output, CompressionLevel::new(6).unwrap());
            writer.write_all(black_box(&input)).unwrap();
            writer.finish().unwrap();
            black_box(output);
        })
    });

    group.finish();
}

fn bench_writer_file_io(c: &mut Criterion) {
    let input: Vec<u8> = (0..BGZF_BLOCK_SIZE * 100).map(|i| (i % 256) as u8).collect();
    let dir = tempdir().unwrap();

    let mut group = c.benchmark_group("writer_file_io");
    group.throughput(Throughput::Bytes(input.len() as u64));

    group.bench_function("unbuffered", |b| {
        let path = dir.path().join("unbuffered.bgz");
        b.iter(|| {
            let file = std::fs::File::create(&path).unwrap();
            let mut writer = Writer::new(file, CompressionLevel::new(6).unwrap());
            writer.write_all(black_box(&input)).unwrap();
            writer.finish().unwrap();
        })
    });

    group.bench_function("bufwriter_256k", |b| {
        let path = dir.path().join("buffered.bgz");
        b.iter(|| {
            let file = std::fs::File::create(&path).unwrap();
            let file = BufWriter::with_capacity(256 * 1024, file);
            let mut writer = Writer::new(file, CompressionLevel::new(6).unwrap());
            writer.write_all(black_box(&input)).unwrap();
            writer.finish().unwrap();
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_compressor_single_block,
    bench_compressor_levels,
    bench_writer_throughput,
    bench_writer_file_io
);
criterion_main!(benches);
