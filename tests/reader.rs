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

fn gzip_compress_level(data: &[u8], level: flate2::Compression) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use std::io::Write;
    let mut e = GzEncoder::new(Vec::new(), level);
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

#[test]
fn test_small_file() {
    let original = b"Hello, world! This is a test.";
    let compressed = gzip_compress(original);
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), &compressed).unwrap();

    let mut reader = ParallelGzipReader::from_file(tmp.path(), 2).unwrap();
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();
    assert_eq!(output, original);
}

#[test]
fn test_large_file_parallel() {
    // 10 MB repetitive data that compresses well and produces back-references.
    let original: Vec<u8> = (0..10_000_000).map(|i| (i % 256) as u8).collect();
    let compressed = gzip_compress(&original);
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), &compressed).unwrap();

    let mut reader = ParallelGzipReader::from_file(tmp.path(), 4).unwrap();
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();
    assert_eq!(output.len(), original.len());
    assert_eq!(output, original);
}

#[test]
fn test_matches_flate2() {
    // DNA-like data.
    let bases = b"ACGTACGTNNNN";
    let original: Vec<u8> = (0..1_000_000).map(|i| bases[i % bases.len()]).collect();
    let compressed = gzip_compress(&original);
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), &compressed).unwrap();

    // Parallel reader.
    let mut par = ParallelGzipReader::from_file(tmp.path(), 4).unwrap();
    let mut par_out = Vec::new();
    par.read_to_end(&mut par_out).unwrap();

    // flate2 reference.
    let mut ref_reader = flate2::read::GzDecoder::new(std::io::BufReader::new(
        std::fs::File::open(tmp.path()).unwrap(),
    ));
    let mut ref_out = Vec::new();
    ref_reader.read_to_end(&mut ref_out).unwrap();

    assert_eq!(par_out, ref_out);
}

#[test]
fn test_streaming_fallback() {
    let original = b"Streaming fallback test";
    let compressed = gzip_compress(original);
    let cursor = std::io::Cursor::new(compressed);
    let mut reader = ParallelGzipReader::from_reader(cursor, 4).unwrap();
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();
    assert_eq!(output, original.to_vec());
}

#[test]
fn test_multi_member_gzip_streaming() {
    let part1 = b"First member data here";
    let part2 = b"Second member data here";
    let mut compressed = gzip_compress(part1);
    compressed.extend_from_slice(&gzip_compress(part2));

    // The streaming path (MultiGzDecoder) handles multi-member gzip correctly.
    let cursor = std::io::Cursor::new(compressed);
    let mut reader = ParallelGzipReader::from_reader(cursor, 4).unwrap();
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();

    let mut expected = part1.to_vec();
    expected.extend_from_slice(part2);
    assert_eq!(output, expected);
}

#[test]
fn test_multi_member_gzip_from_file() {
    // The parallel (mmap) path decompresses all gzip members in parallel.
    let part1 = b"First member data in a file";
    let part2 = b"Second member data in a file";
    let mut compressed = gzip_compress(part1);
    compressed.extend_from_slice(&gzip_compress(part2));

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), &compressed).unwrap();

    let mut reader = ParallelGzipReader::from_file(tmp.path(), 2).unwrap();
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();

    let mut expected = part1.to_vec();
    expected.extend_from_slice(part2);
    assert_eq!(output, expected);
}

#[test]
fn test_multi_member_gzip_many_members() {
    // Simulate pigz-style output: many small members concatenated.
    let members: Vec<Vec<u8>> = (0..50)
        .map(|i| format!("Member {} with some data to compress\n", i).into_bytes())
        .collect();

    let mut compressed = Vec::new();
    let mut expected = Vec::new();
    for member in &members {
        compressed.extend_from_slice(&gzip_compress(member));
        expected.extend_from_slice(member);
    }

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), &compressed).unwrap();

    let mut reader = ParallelGzipReader::from_file(tmp.path(), 4).unwrap();
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();

    assert_eq!(output, expected);
}

#[test]
fn test_multi_member_gzip_large_members() {
    // Members with enough data to exercise back-references within each member.
    let member1: Vec<u8> = b"ACGTACGTNNNN".iter().copied().cycle().take(100_000).collect();
    let member2: Vec<u8> = b"TTTAAAGGGCCC".iter().copied().cycle().take(100_000).collect();
    let member3: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();

    let mut compressed = gzip_compress(&member1);
    compressed.extend_from_slice(&gzip_compress(&member2));
    compressed.extend_from_slice(&gzip_compress(&member3));

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), &compressed).unwrap();

    let mut reader = ParallelGzipReader::from_file(tmp.path(), 4).unwrap();
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();

    let mut expected = member1;
    expected.extend_from_slice(&member2);
    expected.extend_from_slice(&member3);
    assert_eq!(output, expected);
}

#[test]
fn test_multi_member_heterogeneous_headers() {
    // Members compressed at different levels have different XFL bytes in the header.
    // The structural scanner must handle this (the old exact-match scanner would fail).
    let part1 = b"compressed with fast level";
    let part2 = b"compressed with best level";
    let part3 = b"compressed with default level";

    let mut compressed = gzip_compress_level(part1, flate2::Compression::fast());
    compressed.extend_from_slice(&gzip_compress_level(part2, flate2::Compression::best()));
    compressed.extend_from_slice(&gzip_compress_level(part3, flate2::Compression::default()));

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), &compressed).unwrap();

    let mut reader = ParallelGzipReader::from_file(tmp.path(), 2).unwrap();
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();

    let mut expected = part1.to_vec();
    expected.extend_from_slice(part2);
    expected.extend_from_slice(part3);
    assert_eq!(output, expected);
}

#[test]
fn test_multi_member_with_trailing_eof_member() {
    // Some tools (bgzip) append an empty EOF member. The reader should handle this.
    let part1 = b"data before eof member";
    let mut compressed = gzip_compress(part1);

    // Append a minimal empty gzip member (empty DEFLATE stream).
    let empty_member = gzip_compress(b"");
    compressed.extend_from_slice(&empty_member);

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), &compressed).unwrap();

    let mut reader = ParallelGzipReader::from_file(tmp.path(), 2).unwrap();
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();

    assert_eq!(output, part1.to_vec());
}

#[test]
fn test_bgzf_input() {
    use flate2::Compression;
    use flate2::GzBuilder;
    use std::io::Write;

    let original = b"BGZF test data with extra field";

    // BGZF extra field: SI1=B(0x42), SI2=C(0x43), SLEN=2, BSIZE placeholder
    let mut encoder = GzBuilder::new()
        .extra(vec![0x42, 0x43, 0x02, 0x00, 0x00, 0x00])
        .write(Vec::new(), Compression::default());
    encoder.write_all(original).unwrap();
    let compressed = encoder.finish().unwrap();

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), &compressed).unwrap();

    let mut reader = ParallelGzipReader::from_file(tmp.path(), 2).unwrap();
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();
    assert_eq!(output, original);
}

#[test]
fn test_gzip_with_filename() {
    use flate2::Compression;
    use flate2::GzBuilder;
    use std::io::Write;

    let original = b"data with filename in gzip header";
    let mut encoder =
        GzBuilder::new().filename("test.fastq").write(Vec::new(), Compression::default());
    encoder.write_all(original).unwrap();
    let compressed = encoder.finish().unwrap();

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), &compressed).unwrap();

    let mut reader = ParallelGzipReader::from_file(tmp.path(), 2).unwrap();
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();
    assert_eq!(output, original);
}

#[test]
fn test_gzip_with_comment() {
    use flate2::Compression;
    use flate2::GzBuilder;
    use std::io::Write;

    let original = b"data with comment in gzip header";
    let mut encoder = GzBuilder::new()
        .comment("This is a test comment")
        .write(Vec::new(), Compression::default());
    encoder.write_all(original).unwrap();
    let compressed = encoder.finish().unwrap();

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), &compressed).unwrap();

    let mut reader = ParallelGzipReader::from_file(tmp.path(), 2).unwrap();
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();
    assert_eq!(output, original);
}

#[test]
fn test_gzip_with_extra_field() {
    use flate2::Compression;
    use flate2::GzBuilder;
    use std::io::Write;

    let original = b"data with non-BGZF extra field";
    // Random extra data that is NOT a BC subfield
    let mut encoder = GzBuilder::new()
        .extra(vec![0x41, 0x41, 0x04, 0x00, 0xDE, 0xAD, 0xBE, 0xEF])
        .write(Vec::new(), Compression::default());
    encoder.write_all(original).unwrap();
    let compressed = encoder.finish().unwrap();

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), &compressed).unwrap();

    let mut reader = ParallelGzipReader::from_file(tmp.path(), 2).unwrap();
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();
    assert_eq!(output, original);
}

#[test]
fn test_empty_gzip() {
    let original = b"";
    let compressed = gzip_compress(original);

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), &compressed).unwrap();

    let mut reader = ParallelGzipReader::from_file(tmp.path(), 2).unwrap();
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();
    assert_eq!(output.len(), 0);
}

#[test]
fn test_multi_member_mixed_header_types() {
    use flate2::Compression;
    use flate2::GzBuilder;
    use std::io::Write;

    // Member 1: plain (no flags)
    let part1 = b"plain member";
    let compressed1 = gzip_compress(part1);

    // Member 2: with filename
    let part2 = b"member with filename";
    let mut encoder2 =
        GzBuilder::new().filename("reads.fastq").write(Vec::new(), Compression::default());
    encoder2.write_all(part2).unwrap();
    let compressed2 = encoder2.finish().unwrap();

    // Member 3: with comment
    let part3 = b"member with comment";
    let mut encoder3 =
        GzBuilder::new().comment("produced by tool X").write(Vec::new(), Compression::default());
    encoder3.write_all(part3).unwrap();
    let compressed3 = encoder3.finish().unwrap();

    let mut combined = compressed1;
    combined.extend_from_slice(&compressed2);
    combined.extend_from_slice(&compressed3);

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), &combined).unwrap();

    let mut reader = ParallelGzipReader::from_file(tmp.path(), 2).unwrap();
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();

    let mut expected = part1.to_vec();
    expected.extend_from_slice(part2);
    expected.extend_from_slice(part3);
    assert_eq!(output, expected);
}

#[test]
fn test_truncated_gzip() {
    let original = b"this data will be truncated";
    let compressed = gzip_compress(original);
    // Truncate: remove the last 10 bytes (part of DEFLATE data + trailer)
    let truncated = &compressed[..compressed.len() - 10];

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), truncated).unwrap();

    // Error may occur at construction or during read — either is acceptable.
    let reader_result = ParallelGzipReader::from_file(tmp.path(), 2);
    match reader_result {
        Err(_) => {} // construction failed, that's fine
        Ok(mut reader) => {
            let mut output = Vec::new();
            let result = reader.read_to_end(&mut output);
            // Either an error, or the output must be a prefix of the original
            // (partial decompression is acceptable, arbitrary corruption is not).
            assert!(
                result.is_err() || original.starts_with(&output),
                "truncated gzip produced non-prefix output: {} bytes",
                output.len()
            );
        }
    }
}

#[test]
fn test_corrupt_deflate_data() {
    let original = b"data that will be corrupted in the middle";
    let mut compressed = gzip_compress(original);
    // Corrupt some bytes in the DEFLATE data (after the 10-byte header)
    if compressed.len() > 15 {
        compressed[12] ^= 0xFF;
        compressed[13] ^= 0xFF;
    }

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), &compressed).unwrap();

    // Error may occur at construction or during read — either is acceptable.
    let reader_result = ParallelGzipReader::from_file(tmp.path(), 2);
    match reader_result {
        Err(_) => {} // construction failed with error, that's fine
        Ok(mut reader) => {
            let mut output = Vec::new();
            let result = reader.read_to_end(&mut output);
            assert!(result.is_err());
        }
    }
}

#[test]
fn test_not_gzip_file() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"this is plain text, not gzip").unwrap();

    let result = ParallelGzipReader::from_file(tmp.path(), 2);
    assert!(result.is_err());
}

#[test]
#[ignore] // Only run when test data is available: cargo test -- --ignored test_real_pigz
fn test_real_pigz_fastq_gz() {
    // Reads a multi-member gzip FASTQ file (e.g. compressed with pigz) and
    // compares decompressed output against flate2 as a reference.
    //
    // Set TEST_REAL_PIGZ_PATH to override the default path, e.g.:
    //   TEST_REAL_PIGZ_PATH=/path/to/sample.fastq.gz cargo test -- --ignored test_real_pigz
    let path = std::env::var("TEST_REAL_PIGZ_PATH").unwrap_or_else(|_| {
        "/Volumes/scratch-00001/fgumi-pipeline-bench/synthetic-pipeline-xlarge_1.fastq.gz"
            .to_string()
    });
    if !std::path::Path::new(&path).exists() {
        eprintln!("Skipping test_real_pigz_fastq_gz: file not found at {path}");
        return;
    }

    // Compute CRC32 from parallel reader
    let mut par_reader = ParallelGzipReader::from_file(&path, 8).unwrap();
    let mut par_hasher = crc32fast::Hasher::new();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut par_total = 0u64;
    loop {
        let n = par_reader.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        par_hasher.update(&buf[..n]);
        par_total += n as u64;
    }
    let par_crc = par_hasher.finalize();

    // Compute CRC32 from flate2 reference
    let file = std::fs::File::open(&path).unwrap();
    let mut ref_reader = flate2::read::MultiGzDecoder::new(std::io::BufReader::new(file));
    let mut ref_hasher = crc32fast::Hasher::new();
    let mut ref_total = 0u64;
    loop {
        let n = ref_reader.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        ref_hasher.update(&buf[..n]);
        ref_total += n as u64;
    }
    let ref_crc = ref_hasher.finalize();

    assert_eq!(par_total, ref_total, "decompressed size mismatch");
    assert_eq!(par_crc, ref_crc, "decompressed content CRC32 mismatch");
}
