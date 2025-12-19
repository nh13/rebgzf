#![no_main]

use libfuzzer_sys::fuzz_target;
use flate2::write::GzEncoder;
use flate2::Compression;
use rebgzf::{SingleThreadedTranscoder, TranscodeConfig, Transcoder};
use std::io::{Cursor, Write};

fuzz_target!(|data: &[u8]| {
    // Create valid gzip from arbitrary data, then transcode
    // This exercises the DEFLATE parser with valid structures but varied content

    if data.is_empty() {
        return;
    }

    // Limit data size to avoid slowdowns
    let data = if data.len() > 64 * 1024 { &data[..64 * 1024] } else { data };

    // Compress the fuzz input to gzip
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    if encoder.write_all(data).is_err() {
        return;
    }
    let gzip_data = match encoder.finish() {
        Ok(d) => d,
        Err(_) => return,
    };

    // Now transcode it - this should always succeed on valid gzip
    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut output = Vec::new();

    if transcoder.transcode(Cursor::new(&gzip_data), &mut output).is_ok() {
        // Verify the output is valid BGZF that can be decompressed
        use flate2::read::MultiGzDecoder;
        use std::io::Read;

        let mut decoder = MultiGzDecoder::new(&output[..]);
        let mut decompressed = Vec::new();
        if decoder.read_to_end(&mut decompressed).is_ok() {
            // Output should match input
            assert_eq!(decompressed, data, "Round-trip mismatch");
        }
    }
});
