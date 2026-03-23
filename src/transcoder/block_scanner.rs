//! Fast DEFLATE block boundary scanner (rapidgzip-style).
//!
//! Six-stage filtering pipeline for finding valid dynamic Huffman DEFLATE blocks:
//! 1. 13-bit quick check: BTYPE=10, HLIT<=29
//! 2. Precode leaf-count check: virtual leaves must equal 64 or 128
//! 3. Precode Huffman tree construction
//! 4. RLE decode of literal/distance code lengths
//! 5. EndOfBlock symbol check: code_lengths[256] must be nonzero
//! 6. Full Huffman tree validity: both trees must be complete
//!
//! Based on rapidgzip's DynamicHuffman.hpp and CountAllocatedLeaves.hpp.
//! Reference: ~/work/git/rapidgzip/librapidarchive/src/rapidgzip/blockfinder/

use crate::bits::{BitRead, SliceBitReader};
use crate::huffman::HuffmanDecoder;

/// A valid DEFLATE block boundary found by the scanner.
pub struct BlockBoundary {
    /// Absolute bit offset from start of data.
    pub bit_offset: usize,
}

/// Maximum precode code length (RFC 1951: 7 bits)
const MAX_PRECODE_LENGTH: u32 = 7;

/// Precode alphabet reordering (RFC 1951 section 3.2.7)
const PRECODE_ALPHABET: [usize; 19] =
    [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

/// Scan for a valid DEFLATE block between `start_bit` and `end_bit`.
pub fn scan_for_block(data: &[u8], start_bit: usize, end_bit: usize) -> Option<BlockBoundary> {
    let max_bit = end_bit.min(data.len().saturating_sub(10) * 8);
    let mut bit_pos = start_bit;

    while bit_pos < max_bit {
        // Stage 1: 13-bit quick reject
        let skip = quick_reject_13bit(data, bit_pos);
        if skip > 0 {
            bit_pos += skip as usize;
            continue;
        }

        // Stages 2-6: deep validation
        if deep_validate(data, bit_pos) {
            return Some(BlockBoundary { bit_offset: bit_pos });
        }

        bit_pos += 1;
    }

    None
}

/// Stage 1: 13-bit structural rejection.
///
/// Checks BFINAL(1) + BTYPE(2) + HLIT(5) + HDIST(5) = 13 bits.
/// Returns 0 if candidate passes, or number of bits to skip.
#[inline(always)]
fn quick_reject_13bit(data: &[u8], bit_pos: usize) -> u8 {
    let byte_pos = bit_pos / 8;
    let bit_offset = bit_pos % 8;

    if byte_pos + 2 >= data.len() {
        return 1;
    }

    // Load 3 bytes, extract 13 bits at bit_offset
    let b0 = data[byte_pos] as u32;
    let b1 = data[byte_pos + 1] as u32;
    // Safety: byte_pos + 2 < data.len() is guaranteed by the early return above.
    let b2 = data[byte_pos + 2] as u32;
    let raw = b0 | (b1 << 8) | (b2 << 16);
    let bits = (raw >> bit_offset as u32) & 0x1FFF;

    // Bit 0: BFINAL — accept both 0 and 1. Final dynamic blocks are structurally
    // valid chunk starts; a thread starting at a BFINAL=1 block simply decodes that
    // one block and stops. Rejecting BFINAL=1 would create false negatives near EOF.

    // Bits 1-2: BTYPE must be 10 (dynamic Huffman)
    if (bits >> 1) & 3 != 2 {
        return 1;
    }
    // Bits 3-7: HLIT must be <= 29 (RFC 1951: HLIT + 257 literal/length codes, max 286)
    if (bits >> 3) & 0x1F > 29 {
        return 1;
    }
    // Bits 8-12: HDIST (RFC 1951: HDIST + 1 distance code lengths, 5-bit field).
    // We do NOT reject HDIST 30-31 here: while only 30 distance symbols (0-29) are
    // semantically meaningful, a valid encoder may emit HDIST=30 or 31 with trailing
    // unused codes set to length 0. Deep validation (Stage 6) catches invalid trees.

    0
}

/// Stages 2-6: Deep validation of a candidate position.
fn deep_validate(data: &[u8], bit_pos: usize) -> bool {
    let byte_pos = bit_pos / 8;
    let bit_offset = (bit_pos % 8) as u8;

    // Need enough data for header + precode
    if byte_pos + 12 >= data.len() {
        return false;
    }

    let mut bits = SliceBitReader::new(data);
    bits.set_bit_position(byte_pos, bit_offset);

    // Skip BFINAL(1) + BTYPE(2) = 3 bits (already validated)
    if bits.read_bits(3).is_err() {
        return false;
    }

    // Read HLIT, HDIST, HCLEN
    let hlit = match bits.read_bits(5) {
        Ok(v) => v as usize + 257,
        Err(_) => return false,
    };
    let hdist = match bits.read_bits(5) {
        Ok(v) => v as usize + 1,
        Err(_) => return false,
    };
    let hclen = match bits.read_bits(4) {
        Ok(v) => v as usize + 4,
        Err(_) => return false,
    };

    // Stage 2: Precode leaf-count check
    // Read the precode code lengths (3 bits each, up to 19 symbols)
    let mut precode_lengths = [0u8; 19];
    for i in 0..hclen {
        match bits.read_bits(3) {
            Ok(v) => precode_lengths[PRECODE_ALPHABET[i]] = v as u8,
            Err(_) => return false,
        }
    }

    if !check_precode_leaf_count(&precode_lengths) {
        return false;
    }

    // Stage 3: Build precode Huffman tree
    let precode_decoder = match HuffmanDecoder::from_code_lengths(&precode_lengths) {
        Ok(d) => d,
        Err(_) => return false,
    };

    // Stage 4: RLE decode of literal/distance code lengths
    let total_codes = hlit + hdist;
    let mut all_lengths = Vec::with_capacity(total_codes);

    while all_lengths.len() < total_codes {
        let sym = match precode_decoder.decode(&mut bits) {
            Ok(s) => s,
            Err(_) => return false,
        };

        match sym {
            0..=15 => all_lengths.push(sym as u8),
            16 => {
                let repeat = match bits.read_bits(2) {
                    Ok(v) => v as usize + 3,
                    Err(_) => return false,
                };
                let prev = match all_lengths.last() {
                    Some(&v) => v,
                    None => return false, // backreference with no previous value
                };
                for _ in 0..repeat {
                    all_lengths.push(prev);
                }
            }
            17 => {
                let repeat = match bits.read_bits(3) {
                    Ok(v) => v as usize + 3,
                    Err(_) => return false,
                };
                all_lengths.resize(all_lengths.len() + repeat, 0);
            }
            18 => {
                let repeat = match bits.read_bits(7) {
                    Ok(v) => v as usize + 11,
                    Err(_) => return false,
                };
                all_lengths.resize(all_lengths.len() + repeat, 0);
            }
            _ => return false,
        }

        // Guard against overshoot
        if all_lengths.len() > total_codes {
            return false;
        }
    }

    if all_lengths.len() != total_codes {
        return false;
    }

    let lit_lengths = &all_lengths[..hlit];
    let dist_lengths = &all_lengths[hlit..];

    // Stage 5: EndOfBlock symbol (256) must have a nonzero code length.
    // hlit is always >= 257 (computed as 5-bit field + 257), so lit_lengths[256] always exists.
    if lit_lengths[256] == 0 {
        return false;
    }

    // Stage 6: Both code length arrays must form valid Huffman trees
    if !check_huffman_code_lengths(lit_lengths, 15) {
        return false;
    }
    if !dist_lengths.iter().all(|&l| l == 0) && !check_huffman_code_lengths(dist_lengths, 15) {
        return false;
    }

    true
}

/// Stage 2: Check that precode code lengths form a valid Huffman tree.
///
/// Computes the "virtual leaf count" at maximum depth (7). A valid tree
/// must have exactly 128 leaves (full tree) or 64 (single symbol with
/// code length 1).
///
/// Based on rapidgzip's CountAllocatedLeaves::checkPrecode().
fn check_precode_leaf_count(lengths: &[u8; 19]) -> bool {
    let mut leaf_count: u32 = 0;
    for &cl in lengths {
        if cl > 0 {
            if cl as u32 > MAX_PRECODE_LENGTH {
                return false;
            }
            leaf_count += 1 << (MAX_PRECODE_LENGTH - cl as u32);
        }
    }
    // Full tree: 2^7 = 128 leaves. Single-symbol tree: 2^6 = 64 leaves.
    leaf_count == 128 || leaf_count == 64
}

/// Stage 6: Check that a set of Huffman code lengths forms a valid tree.
///
/// Projects all codes to max_code_length depth and checks that the total
/// virtual leaf count equals 2^max_code_length (complete tree) or
/// 2^(max_code_length-1) (single symbol with code length 1, no code > 1).
///
/// Based on rapidgzip's HuffmanCodingBase::checkHuffmanCodeLengths().
fn check_huffman_code_lengths(lengths: &[u8], max_code_length: u8) -> bool {
    let mut leaf_count: u64 = 0;
    for &cl in lengths {
        if cl > 0 {
            if cl > max_code_length {
                return false;
            }
            leaf_count += 1u64 << (max_code_length - cl);
        }
    }

    let full = 1u64 << max_code_length; // 32768 for max_code_length=15
    let half = full >> 1; // 16384

    if leaf_count == full {
        return true; // Complete tree
    }

    if leaf_count == half {
        // Single-symbol tree: no code length may exceed 1
        return lengths.iter().all(|&cl| cl <= 1);
    }

    false
}

/// Returns true if the byte is valid ASCII for bioinformatics data.
#[inline(always)]
pub fn is_ascii_bioinformatics(byte: u8) -> bool {
    (9..=13).contains(&byte) || (32..=126).contains(&byte)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_ascii_validation() {
        assert!(is_ascii_bioinformatics(9));
        assert!(is_ascii_bioinformatics(10));
        assert!(is_ascii_bioinformatics(13));
        assert!(is_ascii_bioinformatics(32));
        assert!(is_ascii_bioinformatics(b'A'));
        assert!(is_ascii_bioinformatics(126));
        assert!(!is_ascii_bioinformatics(0));
        assert!(!is_ascii_bioinformatics(127));
        assert!(!is_ascii_bioinformatics(255));
    }

    #[test]
    fn test_precode_leaf_count_valid() {
        // Two symbols with code length 1: 64 + 64 = 128 (full tree)
        let mut lens = [0u8; 19];
        lens[0] = 1;
        lens[1] = 1;
        assert!(check_precode_leaf_count(&lens));
    }

    #[test]
    fn test_precode_leaf_count_invalid() {
        // One symbol with code length 2: 32 leaves (not 64 or 128)
        let mut lens = [0u8; 19];
        lens[0] = 2;
        assert!(!check_precode_leaf_count(&lens));
    }

    #[test]
    fn test_precode_leaf_count_single_symbol() {
        // Single symbol with code length 1: 64 leaves (valid single-symbol tree)
        let mut lens = [0u8; 19];
        lens[0] = 1;
        assert!(check_precode_leaf_count(&lens));
    }

    #[test]
    fn test_check_huffman_code_lengths_valid() {
        // Two symbols: lengths [1, 1] → leaf count = 16384 + 16384 = 32768 = 2^15
        assert!(check_huffman_code_lengths(&[1, 1], 15));
    }

    #[test]
    fn test_check_huffman_code_lengths_invalid() {
        // One symbol with length 2: 8192 leaves (not 32768 or 16384)
        assert!(!check_huffman_code_lengths(&[2], 15));
    }

    #[test]
    fn test_quick_reject_filters_non_dynamic() {
        // BFINAL=1 with BTYPE=10 should pass (not rejected)
        // 0x05 = bits 101 -> BFINAL=1, BTYPE=10
        assert_eq!(quick_reject_13bit(&[0x05, 0, 0], 0), 0);
        assert!(quick_reject_13bit(&[0x00, 0, 0], 0) > 0); // BTYPE=00
        assert!(quick_reject_13bit(&[0x02, 0, 0], 0) > 0); // BTYPE=01
        assert!(quick_reject_13bit(&[0x06, 0, 0], 0) > 0); // BTYPE=11
    }

    #[test]
    fn test_scan_finds_block_at_deflate_start() {
        use std::io::Write;
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        for i in 0..10_000 {
            write!(encoder, "@read_{i}\nACGTACGTACGT\n+\nIIIIIIIIIIII\n").unwrap();
            if i % 500 == 499 {
                encoder.flush().unwrap();
            }
        }
        let compressed = encoder.finish().unwrap();
        let mut padded = compressed.clone();
        padded.extend_from_slice(&[0u8; 16]);

        let result = scan_for_block(&padded, 0, compressed.len() * 8);
        assert!(result.is_some(), "Should find block in compressed FASTQ");
    }

    #[test]
    fn test_scan_no_panic_on_random_data() {
        let mut data = vec![0u8; 8192 + 16];
        let mut state: u64 = 0xDEAD_BEEF_CAFE_BABEu64;
        for byte in data.iter_mut().take(8192) {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *byte = (state >> 33) as u8;
        }
        let _result = scan_for_block(&data, 0, 8192 * 8);
        // No assertion — false positives are handled by the orchestrator
    }
}
