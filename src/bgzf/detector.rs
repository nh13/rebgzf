//! BGZF format detection and validation.
//!
//! Provides both quick detection (first block only) and strict validation
//! (all blocks) for BGZF files.

use crate::error::{Error, Result};
use std::io::{Read, Seek, SeekFrom};

/// Result of BGZF validation
#[derive(Clone, Debug, Default)]
pub struct BgzfValidation {
    /// Whether the input is valid BGZF
    pub is_valid_bgzf: bool,
    /// Number of BGZF blocks (only populated in strict mode)
    pub block_count: Option<u64>,
    /// Total uncompressed size across all blocks (only populated in strict mode)
    pub total_uncompressed_size: Option<u64>,
}

/// BGZF header constants
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const BGZF_SUBFIELD_ID: [u8; 2] = [b'B', b'C'];
const FEXTRA_FLAG: u8 = 0x04;
const MIN_HEADER_SIZE: usize = 18;

/// Quick check - only validates first block header.
///
/// This is a fast O(1) check that reads the first 18+ bytes to verify
/// the BGZF header signature is present.
pub fn is_bgzf<R: Read>(reader: &mut R) -> Result<bool> {
    let mut header = [0u8; MIN_HEADER_SIZE];

    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Ok(false);
        }
        Err(e) => return Err(Error::Io(e)),
    }

    Ok(validate_bgzf_header(&header))
}

/// Check if a header buffer contains valid BGZF header markers.
fn validate_bgzf_header(header: &[u8]) -> bool {
    if header.len() < MIN_HEADER_SIZE {
        return false;
    }

    // Check gzip magic bytes (0x1f 0x8b)
    if header[0..2] != GZIP_MAGIC {
        return false;
    }

    // Check compression method (8 = DEFLATE)
    if header[2] != 8 {
        return false;
    }

    // Check FEXTRA flag is set
    if header[3] & FEXTRA_FLAG == 0 {
        return false;
    }

    // Get XLEN (extra field length) at bytes 10-11
    let xlen = u16::from_le_bytes([header[10], header[11]]) as usize;

    // We need at least 6 bytes for the BC subfield (2 ID + 2 LEN + 2 BSIZE)
    if xlen < 6 {
        return false;
    }

    // Check for BC subfield at bytes 12-13
    if header[12..14] != BGZF_SUBFIELD_ID {
        return false;
    }

    // Check BC subfield length is 2 (bytes 14-15)
    let bc_len = u16::from_le_bytes([header[14], header[15]]);
    if bc_len != 2 {
        return false;
    }

    true
}

/// Full validation - iterates all blocks.
///
/// This performs thorough validation by reading every BGZF block header
/// and verifying the structure. It also counts blocks and accumulates
/// uncompressed sizes.
pub fn validate_bgzf_strict<R: Read + Seek>(reader: &mut R) -> Result<BgzfValidation> {
    // Start from beginning
    reader.seek(SeekFrom::Start(0))?;

    let mut block_count: u64 = 0;
    let mut total_uncompressed_size: u64 = 0;

    loop {
        let mut header = [0u8; MIN_HEADER_SIZE];

        match reader.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // End of file - check if we read any blocks
                if block_count == 0 {
                    return Ok(BgzfValidation {
                        is_valid_bgzf: false,
                        block_count: None,
                        total_uncompressed_size: None,
                    });
                }
                break;
            }
            Err(e) => return Err(Error::Io(e)),
        }

        // Validate this block's header
        if !validate_bgzf_header(&header) {
            return Ok(BgzfValidation {
                is_valid_bgzf: false,
                block_count: Some(block_count),
                total_uncompressed_size: Some(total_uncompressed_size),
            });
        }

        // Get BSIZE (total block size - 1) from bytes 16-17
        let bsize = u16::from_le_bytes([header[16], header[17]]) as u64;
        let block_size = bsize + 1;

        // Calculate remaining bytes to skip to next block
        // We've read 18 bytes, need to skip to end of block
        let remaining = block_size.saturating_sub(MIN_HEADER_SIZE as u64);

        // Read the footer to get ISIZE (uncompressed size)
        // Footer is last 8 bytes: 4 bytes CRC32 + 4 bytes ISIZE
        if remaining < 8 {
            // Block too small to have valid footer
            return Ok(BgzfValidation {
                is_valid_bgzf: false,
                block_count: Some(block_count),
                total_uncompressed_size: Some(total_uncompressed_size),
            });
        }

        // Skip to footer (remaining - 8 bytes of footer)
        let skip_to_footer = remaining - 8;
        if skip_to_footer > 0 {
            reader.seek(SeekFrom::Current(skip_to_footer as i64))?;
        }

        // Read footer
        let mut footer = [0u8; 8];
        reader.read_exact(&mut footer)?;

        // Get ISIZE (uncompressed size) from last 4 bytes
        let isize = u32::from_le_bytes([footer[4], footer[5], footer[6], footer[7]]);
        total_uncompressed_size += isize as u64;

        block_count += 1;

        // Check for EOF block (ISIZE = 0 and block_size = 28)
        if isize == 0 && block_size == 28 {
            // This is likely the EOF block, we're done
            break;
        }
    }

    // Seek back to start for potential fast-path copy
    reader.seek(SeekFrom::Start(0))?;

    Ok(BgzfValidation {
        is_valid_bgzf: true,
        block_count: Some(block_count),
        total_uncompressed_size: Some(total_uncompressed_size),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // Valid BGZF EOF block (28 bytes)
    const BGZF_EOF: [u8; 28] = [
        0x1f, 0x8b, 0x08, 0x04, // gzip magic, method, flags (FEXTRA)
        0x00, 0x00, 0x00, 0x00, // mtime
        0x00, 0xff, // xfl, os
        0x06, 0x00, // xlen = 6
        0x42, 0x43, // subfield ID "BC"
        0x02, 0x00, // subfield length = 2
        0x1b, 0x00, // BSIZE = 27 (28 - 1)
        0x03, 0x00, // empty deflate block
        0x00, 0x00, 0x00, 0x00, // CRC32 = 0
        0x00, 0x00, 0x00, 0x00, // ISIZE = 0
    ];

    #[test]
    fn test_is_bgzf_with_eof_block() {
        let mut cursor = Cursor::new(&BGZF_EOF);
        assert!(is_bgzf(&mut cursor).unwrap());
    }

    #[test]
    fn test_is_bgzf_with_plain_gzip() {
        // Plain gzip header (no FEXTRA, no BC subfield)
        let plain_gzip = [
            0x1f, 0x8b, 0x08, 0x00, // magic, method, flags (no FEXTRA)
            0x00, 0x00, 0x00, 0x00, // mtime
            0x00, 0xff, // xfl, os
                  // ... rest of gzip data
        ];
        let mut cursor = Cursor::new(&plain_gzip);
        assert!(!is_bgzf(&mut cursor).unwrap());
    }

    #[test]
    fn test_is_bgzf_with_empty_input() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        assert!(!is_bgzf(&mut cursor).unwrap());
    }

    #[test]
    fn test_is_bgzf_with_random_data() {
        let random = vec![0xde, 0xad, 0xbe, 0xef, 0x00, 0x01, 0x02, 0x03];
        let mut cursor = Cursor::new(&random);
        assert!(!is_bgzf(&mut cursor).unwrap());
    }

    #[test]
    fn test_validate_strict_eof_only() {
        let mut cursor = Cursor::new(&BGZF_EOF);
        let result = validate_bgzf_strict(&mut cursor).unwrap();

        assert!(result.is_valid_bgzf);
        assert_eq!(result.block_count, Some(1));
        assert_eq!(result.total_uncompressed_size, Some(0));
    }

    #[test]
    fn test_validate_strict_plain_gzip() {
        let plain_gzip = [
            0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let mut cursor = Cursor::new(&plain_gzip);
        let result = validate_bgzf_strict(&mut cursor).unwrap();

        assert!(!result.is_valid_bgzf);
    }
}
