//! Benchmarks for rebgzf transcoding performance.
//!
//! Tests various data patterns and sizes to measure transcoding throughput.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use flate2::write::GzEncoder;
use flate2::Compression;
use rebgzf::{ParallelTranscoder, SingleThreadedTranscoder, TranscodeConfig, Transcoder};
use std::io::{Cursor, Write};

/// Generate random (incompressible) data
fn generate_random_data(size: usize) -> Vec<u8> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut data = Vec::with_capacity(size);
    let mut hasher = DefaultHasher::new();

    for i in 0..size {
        i.hash(&mut hasher);
        data.push((hasher.finish() & 0xFF) as u8);
    }
    data
}

/// Generate repetitive (highly compressible) data
fn generate_repetitive_data(size: usize) -> Vec<u8> {
    let pattern = b"ABCDABCDABCDABCD";
    let mut data = Vec::with_capacity(size);
    while data.len() < size {
        let remaining = size - data.len();
        let chunk_size = remaining.min(pattern.len());
        data.extend_from_slice(&pattern[..chunk_size]);
    }
    data
}

/// Generate DNA-like data (4 character alphabet, some patterns)
fn generate_dna_data(size: usize) -> Vec<u8> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let bases = [b'A', b'C', b'G', b'T'];
    let mut data = Vec::with_capacity(size);
    let mut hasher = DefaultHasher::new();

    // Mix of random and repetitive regions
    let mut i = 0;
    while data.len() < size {
        // Occasionally insert a repeat region
        if i % 1000 == 0 && data.len() + 50 <= size {
            let repeat = b"ATATATATAT";
            for _ in 0..5 {
                data.extend_from_slice(repeat);
            }
        } else {
            i.hash(&mut hasher);
            let idx = (hasher.finish() % 4) as usize;
            data.push(bases[idx]);
        }
        i += 1;
    }
    data.truncate(size);
    data
}

/// Generate FASTQ-like data
fn generate_fastq_data(num_reads: usize, read_length: usize) -> Vec<u8> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let bases = [b'A', b'C', b'G', b'T'];
    let quals = b"IIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIII"; // High quality scores

    let mut data = Vec::new();
    let mut hasher = DefaultHasher::new();

    for read_num in 0..num_reads {
        // Header
        data.extend_from_slice(format!("@READ_{}\n", read_num).as_bytes());

        // Sequence
        for j in 0..read_length {
            (read_num * 1000 + j).hash(&mut hasher);
            let idx = (hasher.finish() % 4) as usize;
            data.push(bases[idx]);
        }
        data.push(b'\n');

        // Plus line
        data.extend_from_slice(b"+\n");

        // Quality scores
        for _ in 0..read_length {
            data.push(quals[0]);
        }
        data.push(b'\n');
    }
    data
}

/// Compress data to gzip format
fn compress_to_gzip(data: &[u8], level: Compression) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), level);
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn bench_single_threaded(c: &mut Criterion) {
    let mut group = c.benchmark_group("single_threaded");

    // Test different data sizes
    for size in [1024, 64 * 1024, 256 * 1024, 1024 * 1024].iter() {
        let data = generate_dna_data(*size);
        let gzip_data = compress_to_gzip(&data, Compression::default());

        group.throughput(Throughput::Bytes(*size as u64));
        group.bench_with_input(BenchmarkId::new("dna_data", size), &gzip_data, |b, gzip_data| {
            let config = TranscodeConfig::default();
            b.iter(|| {
                let mut transcoder = SingleThreadedTranscoder::new(config.clone());
                let mut output = Vec::new();
                transcoder.transcode(Cursor::new(gzip_data), &mut output).unwrap();
                output
            });
        });
    }

    group.finish();
}

fn bench_parallel(c: &mut Criterion) {
    let mut group = c.benchmark_group("parallel");

    // Test with 1MB of data at different thread counts
    let size = 1024 * 1024;
    let data = generate_dna_data(size);
    let gzip_data = compress_to_gzip(&data, Compression::default());

    group.throughput(Throughput::Bytes(size as u64));

    for threads in [2, 4, 8].iter() {
        group.bench_with_input(BenchmarkId::new("threads", threads), &gzip_data, |b, gzip_data| {
            let config = TranscodeConfig { num_threads: *threads, ..Default::default() };
            b.iter(|| {
                let mut transcoder = ParallelTranscoder::new(config.clone());
                let mut output = Vec::new();
                transcoder.transcode(Cursor::new(gzip_data), &mut output).unwrap();
                output
            });
        });
    }

    group.finish();
}

fn bench_data_patterns(c: &mut Criterion) {
    let mut group = c.benchmark_group("data_patterns");
    let size = 256 * 1024; // 256KB

    // Random data
    let random_data = generate_random_data(size);
    let random_gzip = compress_to_gzip(&random_data, Compression::default());

    // Repetitive data
    let repetitive_data = generate_repetitive_data(size);
    let repetitive_gzip = compress_to_gzip(&repetitive_data, Compression::default());

    // DNA data
    let dna_data = generate_dna_data(size);
    let dna_gzip = compress_to_gzip(&dna_data, Compression::default());

    group.throughput(Throughput::Bytes(size as u64));

    group.bench_function("random", |b| {
        // Use smaller block size for incompressible data to avoid exceeding BGZF max
        let config = TranscodeConfig { block_size: 32768, ..Default::default() };
        b.iter(|| {
            let mut transcoder = SingleThreadedTranscoder::new(config.clone());
            let mut output = Vec::new();
            transcoder.transcode(Cursor::new(&random_gzip), &mut output).unwrap();
            output
        });
    });

    group.bench_function("repetitive", |b| {
        let config = TranscodeConfig::default();
        b.iter(|| {
            let mut transcoder = SingleThreadedTranscoder::new(config.clone());
            let mut output = Vec::new();
            transcoder.transcode(Cursor::new(&repetitive_gzip), &mut output).unwrap();
            output
        });
    });

    group.bench_function("dna", |b| {
        let config = TranscodeConfig::default();
        b.iter(|| {
            let mut transcoder = SingleThreadedTranscoder::new(config.clone());
            let mut output = Vec::new();
            transcoder.transcode(Cursor::new(&dna_gzip), &mut output).unwrap();
            output
        });
    });

    group.finish();
}

fn bench_compression_levels(c: &mut Criterion) {
    let mut group = c.benchmark_group("compression_levels");
    let size = 256 * 1024;
    let data = generate_dna_data(size);

    group.throughput(Throughput::Bytes(size as u64));

    for level in [1, 6, 9].iter() {
        let gzip_data = compress_to_gzip(&data, Compression::new(*level));

        group.bench_with_input(BenchmarkId::new("level", level), &gzip_data, |b, gzip_data| {
            let config = TranscodeConfig::default();
            b.iter(|| {
                let mut transcoder = SingleThreadedTranscoder::new(config.clone());
                let mut output = Vec::new();
                transcoder.transcode(Cursor::new(gzip_data), &mut output).unwrap();
                output
            });
        });
    }

    group.finish();
}

fn bench_fastq_realistic(c: &mut Criterion) {
    let mut group = c.benchmark_group("fastq_realistic");

    // Simulate different read counts (typical Illumina read length of 150bp)
    for num_reads in [1000, 10000, 50000].iter() {
        let data = generate_fastq_data(*num_reads, 150);
        let gzip_data = compress_to_gzip(&data, Compression::default());

        group.throughput(Throughput::Bytes(data.len() as u64));
        group.bench_with_input(BenchmarkId::new("reads", num_reads), &gzip_data, |b, gzip_data| {
            let config = TranscodeConfig::default();
            b.iter(|| {
                let mut transcoder = SingleThreadedTranscoder::new(config.clone());
                let mut output = Vec::new();
                transcoder.transcode(Cursor::new(gzip_data), &mut output).unwrap();
                output
            });
        });
    }

    group.finish();
}

fn bench_bgzf_detection(c: &mut Criterion) {
    use rebgzf::{is_bgzf, validate_bgzf_strict};

    let mut group = c.benchmark_group("bgzf_detection");

    // Create a BGZF file for testing
    let data = generate_dna_data(256 * 1024);
    let gzip_data = compress_to_gzip(&data, Compression::default());

    // Transcode to BGZF first
    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut bgzf_data = Vec::new();
    transcoder.transcode(Cursor::new(&gzip_data), &mut bgzf_data).unwrap();

    group.bench_function("is_bgzf_quick", |b| {
        b.iter(|| {
            let mut cursor = Cursor::new(&bgzf_data);
            is_bgzf(&mut cursor).unwrap()
        });
    });

    group.bench_function("validate_bgzf_strict", |b| {
        b.iter(|| {
            let mut cursor = Cursor::new(&bgzf_data);
            validate_bgzf_strict(&mut cursor).unwrap()
        });
    });

    group.bench_function("is_bgzf_gzip_input", |b| {
        b.iter(|| {
            let mut cursor = Cursor::new(&gzip_data);
            is_bgzf(&mut cursor).unwrap()
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_single_threaded,
    bench_parallel,
    bench_data_patterns,
    bench_compression_levels,
    bench_fastq_realistic,
    bench_bgzf_detection,
);
criterion_main!(benches);
