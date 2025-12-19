#![no_main]

use libfuzzer_sys::fuzz_target;
use rebgzf::verify_bgzf;
use std::io::Cursor;

fuzz_target!(|data: &[u8]| {
    // Only proceed if it looks like it might be BGZF (gzip with FEXTRA)
    if data.len() < 18 || data[0] != 0x1f || data[1] != 0x8b {
        return;
    }

    // Check FEXTRA flag is set
    if data[3] & 0x04 == 0 {
        return;
    }

    // Verification may fail on invalid input - that's OK
    // We're looking for panics/crashes, not errors
    let _ = verify_bgzf(&mut Cursor::new(data));
});
