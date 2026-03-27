use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use rebgzf::reader::ParallelGzipReader;
use std::io::Read;

fn gzip_compress(data: &[u8]) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    let mut e = GzEncoder::new(Vec::new(), Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn generate_dna_data(size: usize) -> Vec<u8> {
    let bases = b"ACGTACGTNNNNACGTACGT";
    (0..size).map(|i| bases[i % bases.len()]).collect()
}

fn bench_parallel_reader(c: &mut Criterion) {
    let original = generate_dna_data(10 * 1024 * 1024); // 10 MB
    let compressed = gzip_compress(&original);

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), &compressed).unwrap();

    let mut group = c.benchmark_group("parallel_reader");
    group.throughput(criterion::Throughput::Bytes(original.len() as u64));

    for threads in [1, 2, 4, 8] {
        group.bench_with_input(BenchmarkId::new("parallel", threads), &threads, |b, &threads| {
            b.iter(|| {
                let mut reader = ParallelGzipReader::from_file(tmp.path(), threads).unwrap();
                let mut output = Vec::new();
                reader.read_to_end(&mut output).unwrap();
                assert_eq!(output.len(), original.len());
            });
        });
    }

    group.bench_function("flate2_baseline", |b| {
        b.iter(|| {
            let file = std::fs::File::open(tmp.path()).unwrap();
            let mut reader = flate2::read::GzDecoder::new(std::io::BufReader::new(file));
            let mut output = Vec::new();
            reader.read_to_end(&mut output).unwrap();
            assert_eq!(output.len(), original.len());
        });
    });

    group.finish();
}

criterion_group!(benches, bench_parallel_reader);
criterion_main!(benches);
