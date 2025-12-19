/// Length codes 257-285: base length and extra bits
/// Index by (code - 257)
pub const LENGTH_TABLE: [(u16, u8); 29] = [
    // (base_length, extra_bits)
    (3, 0),   // 257
    (4, 0),   // 258
    (5, 0),   // 259
    (6, 0),   // 260
    (7, 0),   // 261
    (8, 0),   // 262
    (9, 0),   // 263
    (10, 0),  // 264
    (11, 1),  // 265
    (13, 1),  // 266
    (15, 1),  // 267
    (17, 1),  // 268
    (19, 2),  // 269
    (23, 2),  // 270
    (27, 2),  // 271
    (31, 2),  // 272
    (35, 3),  // 273
    (43, 3),  // 274
    (51, 3),  // 275
    (59, 3),  // 276
    (67, 4),  // 277
    (83, 4),  // 278
    (99, 4),  // 279
    (115, 4), // 280
    (131, 5), // 281
    (163, 5), // 282
    (195, 5), // 283
    (227, 5), // 284
    (258, 0), // 285 - special case
];

/// Distance codes 0-29: base distance and extra bits
pub const DISTANCE_TABLE: [(u16, u8); 30] = [
    // (base_distance, extra_bits)
    (1, 0),      // 0
    (2, 0),      // 1
    (3, 0),      // 2
    (4, 0),      // 3
    (5, 1),      // 4
    (7, 1),      // 5
    (9, 2),      // 6
    (13, 2),     // 7
    (17, 3),     // 8
    (25, 3),     // 9
    (33, 4),     // 10
    (49, 4),     // 11
    (65, 5),     // 12
    (97, 5),     // 13
    (129, 6),    // 14
    (193, 6),    // 15
    (257, 7),    // 16
    (385, 7),    // 17
    (513, 8),    // 18
    (769, 8),    // 19
    (1025, 9),   // 20
    (1537, 9),   // 21
    (2049, 10),  // 22
    (3073, 10),  // 23
    (4097, 11),  // 24
    (6145, 11),  // 25
    (8193, 12),  // 26
    (12289, 12), // 27
    (16385, 13), // 28
    (24577, 13), // 29
];

/// Order of code length alphabet for dynamic Huffman blocks
pub const CODE_LENGTH_ORDER: [usize; 19] =
    [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

/// Decode a length value from a length code (257-285) and extra bits
pub fn decode_length(code: u16, extra_bits: u32) -> Option<u16> {
    if !(257..=285).contains(&code) {
        return None;
    }
    let idx = (code - 257) as usize;
    let (base, _) = LENGTH_TABLE[idx];
    Some(base + extra_bits as u16)
}

/// Decode a distance value from a distance code (0-29) and extra bits
pub fn decode_distance(code: u16, extra_bits: u32) -> Option<u16> {
    if code > 29 {
        return None;
    }
    let (base, _) = DISTANCE_TABLE[code as usize];
    Some(base + extra_bits as u16)
}

/// Reverse lookup: find length code from length value
/// Returns (code, extra_value, extra_bits)
pub fn encode_length(length: u16) -> Option<(u16, u16, u8)> {
    if !(3..=258).contains(&length) {
        return None;
    }

    // Special case: length 258 uses code 285 (per RFC 1951)
    if length == 258 {
        return Some((285, 0, 0));
    }

    for (i, &(base, extra_bits)) in LENGTH_TABLE.iter().enumerate() {
        let code = (i as u16) + 257;
        let max_len = if extra_bits == 0 { base } else { base + (1 << extra_bits) - 1 };

        if length >= base && length <= max_len {
            let extra_value = length - base;
            return Some((code, extra_value, extra_bits));
        }
    }
    None
}

/// Reverse lookup: find distance code from distance value
/// Returns (code, extra_value, extra_bits)
pub fn encode_distance(distance: u16) -> Option<(u16, u16, u8)> {
    if !(1..=32768).contains(&distance) {
        return None;
    }

    for (code, &(base, extra_bits)) in DISTANCE_TABLE.iter().enumerate() {
        let max_dist = if extra_bits == 0 { base } else { base + (1 << extra_bits) - 1 };

        if distance >= base && distance <= max_dist {
            let extra_value = distance - base;
            return Some((code as u16, extra_value, extra_bits));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_length() {
        assert_eq!(decode_length(257, 0), Some(3));
        assert_eq!(decode_length(258, 0), Some(4));
        assert_eq!(decode_length(265, 0), Some(11));
        assert_eq!(decode_length(265, 1), Some(12));
        assert_eq!(decode_length(285, 0), Some(258));
    }

    #[test]
    fn test_decode_distance() {
        assert_eq!(decode_distance(0, 0), Some(1));
        assert_eq!(decode_distance(4, 0), Some(5));
        assert_eq!(decode_distance(4, 1), Some(6));
        assert_eq!(decode_distance(29, 0x1FFF), Some(32768));
    }

    #[test]
    fn test_encode_length() {
        assert_eq!(encode_length(3), Some((257, 0, 0)));
        assert_eq!(encode_length(4), Some((258, 0, 0)));
        assert_eq!(encode_length(11), Some((265, 0, 1)));
        assert_eq!(encode_length(12), Some((265, 1, 1)));
        assert_eq!(encode_length(258), Some((285, 0, 0)));
    }

    #[test]
    fn test_encode_distance() {
        assert_eq!(encode_distance(1), Some((0, 0, 0)));
        assert_eq!(encode_distance(5), Some((4, 0, 1)));
        assert_eq!(encode_distance(6), Some((4, 1, 1)));
    }

    #[test]
    fn test_length_roundtrip() {
        for len in 3..=258 {
            let (code, extra, _bits) = encode_length(len).unwrap();
            let decoded = decode_length(code, extra as u32).unwrap();
            assert_eq!(decoded, len, "Roundtrip failed for length {}", len);
        }
    }

    #[test]
    fn test_distance_roundtrip() {
        for dist in 1..=32768u16 {
            let (code, extra, _bits) = encode_distance(dist).unwrap();
            let decoded = decode_distance(code, extra as u32).unwrap();
            assert_eq!(decoded, dist, "Roundtrip failed for distance {}", dist);
        }
    }
}
