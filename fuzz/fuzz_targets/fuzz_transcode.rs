#![no_main]

use libfuzzer_sys::fuzz_target;
use rebgzf::{SingleThreadedTranscoder, TranscodeConfig, Transcoder};
use std::io::Cursor;

fuzz_target!(|data: &[u8]| {
    // Only proceed if it looks like it might be valid gzip
    if data.len() < 10 || data[0] != 0x1f || data[1] != 0x8b {
        return;
    }

    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut output = Vec::new();

    // Transcoding may fail on invalid input - that's OK
    // We're looking for panics/crashes, not errors
    let _ = transcoder.transcode(Cursor::new(data), &mut output);
});
