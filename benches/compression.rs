use std::io::{self, BufWriter, Read, Write};

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use tempfile::tempdir;

use bgzf::{CompressionLevel, Compressor, Reader, Writer, BGZF_BLOCK_SIZE};

/// Deterministic, effectively-incompressible bytes (xorshift64). Stresses the
/// full-size `compressed_buffer` path on read, since deflate can't shrink it.
fn incompressible(len: usize) -> Vec<u8> {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Deterministic random ACGT bases — realistic genomic-ish data that deflate
/// compresses ~4x, so compressed blocks are small.
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

/// Decompress a whole in-memory BGZF blob. This is the path that carries the
/// per-block zero-fills and `BytesMut` overhead we're trying to remove.
fn bench_reader(c: &mut Criterion) {
    let size = BGZF_BLOCK_SIZE * 100; // ~6.5 MB
    let datasets = [("incompressible", incompressible(size)), ("genomic", genomic(size))];

    let mut group = c.benchmark_group("reader");
    for (name, data) in &datasets {
        let blob = make_bgzf(data, 6);
        group.throughput(Throughput::Bytes(data.len() as u64));
        group.bench_function(*name, |b| {
            b.iter(|| {
                let mut reader = Reader::new(blob.as_slice());
                let mut out = Vec::with_capacity(data.len());
                reader.read_to_end(&mut out).unwrap();
                black_box(&out);
            })
        });
    }
    group.finish();
}

/// Read a BGZF blob and re-compress it to a new blob via `io::copy` — the
/// canonical "transcode" pipeline. Re-compresses at level 1 so deflate doesn't
/// completely swamp the read-side copy/memset overhead.
fn bench_roundtrip(c: &mut Criterion) {
    let size = BGZF_BLOCK_SIZE * 100; // ~6.5 MB
    let datasets = [("incompressible", incompressible(size)), ("genomic", genomic(size))];

    let mut group = c.benchmark_group("roundtrip");
    for (name, data) in &datasets {
        let blob = make_bgzf(data, 6);
        group.throughput(Throughput::Bytes(data.len() as u64));
        group.bench_function(*name, |b| {
            b.iter(|| {
                let mut reader = Reader::new(blob.as_slice());
                let mut out = Vec::with_capacity(blob.len());
                let mut writer = Writer::new(&mut out, CompressionLevel::new(1).unwrap());
                io::copy(&mut reader, &mut writer).unwrap();
                writer.finish().unwrap();
                black_box(&out);
            })
        });
    }
    group.finish();
}

/// Write store-only output (compression level 0 => DEFLATE stored blocks). This is
/// the path the store-only write fast path targets: framing plus a verbatim copy and
/// CRC, with no real deflate work.
fn bench_writer_store_only(c: &mut Criterion) {
    let input = genomic(BGZF_BLOCK_SIZE * 100); // ~6.5 MB; content is irrelevant to the store path

    let mut group = c.benchmark_group("writer_store_only");
    group.throughput(Throughput::Bytes(input.len() as u64));
    group.bench_function("write", |b| {
        b.iter(|| {
            // Store-only output is marginally larger than the input (framing overhead).
            let mut output = Vec::with_capacity(input.len() + input.len() / 16);
            let mut writer = Writer::new(&mut output, CompressionLevel::new(0).unwrap());
            writer.write_all(black_box(&input)).unwrap();
            writer.finish().unwrap();
            black_box(output);
        })
    });
    group.finish();
}

/// Read a store-only BGZF blob (level 0 => DEFLATE stored blocks). This is the path the
/// stored-block read fast path targets: no real inflate, just a verbatim copy and CRC.
fn bench_reader_store_only(c: &mut Criterion) {
    let data = genomic(BGZF_BLOCK_SIZE * 100); // ~6.5 MB
    let blob = make_bgzf(&data, 0);

    let mut group = c.benchmark_group("reader_store_only");
    group.throughput(Throughput::Bytes(data.len() as u64));
    group.bench_function("read", |b| {
        b.iter(|| {
            let mut reader = Reader::new(blob.as_slice());
            let mut out = Vec::with_capacity(data.len());
            reader.read_to_end(&mut out).unwrap();
            black_box(&out);
        })
    });
    group.finish();
}

/// Read a store-only blob and re-emit it store-only via `io::copy` — the store-only
/// transcode pipeline, exercising both fast paths end to end.
fn bench_roundtrip_store_only(c: &mut Criterion) {
    let data = genomic(BGZF_BLOCK_SIZE * 100); // ~6.5 MB
    let blob = make_bgzf(&data, 0);

    let mut group = c.benchmark_group("roundtrip_store_only");
    group.throughput(Throughput::Bytes(data.len() as u64));
    group.bench_function("copy", |b| {
        b.iter(|| {
            let mut reader = Reader::new(blob.as_slice());
            let mut out = Vec::with_capacity(blob.len());
            let mut writer = Writer::new(&mut out, CompressionLevel::new(0).unwrap());
            io::copy(&mut reader, &mut writer).unwrap();
            writer.finish().unwrap();
            black_box(&out);
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_compressor_single_block,
    bench_compressor_levels,
    bench_writer_throughput,
    bench_writer_file_io,
    bench_reader,
    bench_roundtrip,
    bench_writer_store_only,
    bench_reader_store_only,
    bench_roundtrip_store_only
);
criterion_main!(benches);
