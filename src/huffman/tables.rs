/// Fixed Huffman literal/length code lengths (RFC 1951 section 3.2.6)
pub fn fixed_literal_lengths() -> [u8; 288] {
    let mut lengths = [0u8; 288];
    lengths[0..=143].fill(8); // 0-143: 8 bits
    lengths[144..=255].fill(9); // 144-255: 9 bits
    lengths[256..=279].fill(7); // 256-279: 7 bits
    lengths[280..=287].fill(8); // 280-287: 8 bits
    lengths
}

/// Fixed Huffman distance code lengths (all 5 bits)
pub fn fixed_distance_lengths() -> [u8; 32] {
    [5u8; 32]
}
