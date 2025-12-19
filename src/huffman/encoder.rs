use crate::bits::BitWriter;
use crate::deflate::tables::{encode_distance, encode_length, CODE_LENGTH_ORDER};
use crate::deflate::tokens::LZ77Token;
use crate::error::Result;

/// Maximum code length for literal/length and distance alphabets (RFC 1951)
const MAX_CODE_LENGTH: u8 = 15;

/// Maximum code length for the code length alphabet
const MAX_CL_CODE_LENGTH: u8 = 7;

/// Frequency counter for dynamic Huffman code generation
#[derive(Clone, Debug)]
pub struct FrequencyCounter {
    /// Frequencies for literal (0-255), EOB (256), and length codes (257-285)
    pub literal_freq: [u32; 286],
    /// Frequencies for distance codes (0-29)
    pub distance_freq: [u32; 30],
}

impl FrequencyCounter {
    pub fn new() -> Self {
        Self { literal_freq: [0; 286], distance_freq: [0; 30] }
    }

    /// Count frequencies from tokens
    pub fn count_tokens(&mut self, tokens: &[LZ77Token]) {
        for token in tokens {
            match token {
                LZ77Token::Literal(byte) => {
                    self.literal_freq[*byte as usize] += 1;
                }
                LZ77Token::Copy { length, distance } => {
                    // Count the length code
                    if let Some((len_code, _, _)) = encode_length(*length) {
                        self.literal_freq[len_code as usize] += 1;
                    }
                    // Count the distance code
                    if let Some((dist_code, _, _)) = encode_distance(*distance) {
                        self.distance_freq[dist_code as usize] += 1;
                    }
                }
                LZ77Token::EndOfBlock => {
                    self.literal_freq[256] += 1;
                }
            }
        }
        // Always ensure EOB has at least one occurrence
        if self.literal_freq[256] == 0 {
            self.literal_freq[256] = 1;
        }
    }

    /// Get the number of literal/length codes needed (HLIT + 257)
    pub fn num_literal_codes(&self) -> usize {
        // Find last non-zero frequency, minimum 257 (for EOB)
        let mut last = 256; // EOB is always present
        for i in (257..286).rev() {
            if self.literal_freq[i] > 0 {
                last = i;
                break;
            }
        }
        last + 1
    }

    /// Get the number of distance codes needed (HDIST + 1)
    pub fn num_distance_codes(&self) -> usize {
        // Find last non-zero frequency, minimum 1
        let mut last = 0;
        for i in (0..30).rev() {
            if self.distance_freq[i] > 0 {
                last = i;
                break;
            }
        }
        // Always need at least 1 distance code
        (last + 1).max(1)
    }
}

impl Default for FrequencyCounter {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute optimal Huffman code lengths for given frequencies with a maximum length limit.
/// Uses a simplified package-merge algorithm.
///
/// Returns a vector of code lengths (0 for unused symbols).
pub fn compute_code_lengths(frequencies: &[u32], max_bits: u8) -> Vec<u8> {
    let n = frequencies.len();
    if n == 0 {
        return vec![];
    }

    // Collect symbols with non-zero frequency
    let symbols: Vec<(usize, u32)> =
        frequencies.iter().enumerate().filter(|(_, &f)| f > 0).map(|(i, &f)| (i, f)).collect();

    if symbols.is_empty() {
        return vec![0; n];
    }

    // Special case: only one symbol
    if symbols.len() == 1 {
        let mut lengths = vec![0u8; n];
        lengths[symbols[0].0] = 1;
        return lengths;
    }

    // Special case: two symbols
    if symbols.len() == 2 {
        let mut lengths = vec![0u8; n];
        lengths[symbols[0].0] = 1;
        lengths[symbols[1].0] = 1;
        return lengths;
    }

    // Build Huffman tree using a priority queue approach
    // Then limit lengths if needed
    let mut lengths = build_huffman_lengths(&symbols, n);

    // Limit code lengths to max_bits
    limit_code_lengths(&mut lengths, &symbols, max_bits);

    lengths
}

/// Build initial Huffman code lengths (may exceed max_bits)
fn build_huffman_lengths(symbols: &[(usize, u32)], n: usize) -> Vec<u8> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    #[derive(Clone)]
    struct Node {
        freq: u64,
        symbols: Vec<usize>, // Leaf symbols in this subtree
        depth: u8,
    }

    impl PartialEq for Node {
        fn eq(&self, other: &Self) -> bool {
            self.freq == other.freq
        }
    }
    impl Eq for Node {}
    impl PartialOrd for Node {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }
    impl Ord for Node {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            self.freq.cmp(&other.freq)
        }
    }

    let mut heap: BinaryHeap<Reverse<Node>> = symbols
        .iter()
        .map(|&(sym, freq)| Reverse(Node { freq: freq as u64, symbols: vec![sym], depth: 0 }))
        .collect();

    // Build tree by combining lowest frequency nodes
    while heap.len() > 1 {
        let Reverse(left) = heap.pop().unwrap();
        let Reverse(right) = heap.pop().unwrap();

        let mut combined_symbols = left.symbols;
        combined_symbols.extend(right.symbols);

        heap.push(Reverse(Node {
            freq: left.freq + right.freq,
            symbols: combined_symbols,
            depth: left.depth.max(right.depth) + 1,
        }));
    }

    // Extract depths by traversing from root
    let mut lengths = vec![0u8; n];

    if heap.pop().is_some() {
        // BFS to compute depths
        compute_depths_bfs(symbols, &mut lengths);
    }

    lengths
}

/// Compute code lengths using BFS on a reconstructed tree
fn compute_depths_bfs(symbols: &[(usize, u32)], lengths: &mut [u8]) {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    // Rebuild tree properly tracking depths
    #[derive(Clone)]
    enum TreeNode {
        Leaf(usize), // symbol index
        Internal(Box<TreeNode>, Box<TreeNode>),
    }

    #[derive(Clone)]
    struct HeapNode {
        freq: u64,
        node: TreeNode,
    }

    impl PartialEq for HeapNode {
        fn eq(&self, other: &Self) -> bool {
            self.freq == other.freq
        }
    }
    impl Eq for HeapNode {}
    impl PartialOrd for HeapNode {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }
    impl Ord for HeapNode {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            self.freq.cmp(&other.freq)
        }
    }

    let mut heap: BinaryHeap<Reverse<HeapNode>> = symbols
        .iter()
        .map(|&(sym, freq)| Reverse(HeapNode { freq: freq as u64, node: TreeNode::Leaf(sym) }))
        .collect();

    while heap.len() > 1 {
        let Reverse(left) = heap.pop().unwrap();
        let Reverse(right) = heap.pop().unwrap();

        heap.push(Reverse(HeapNode {
            freq: left.freq + right.freq,
            node: TreeNode::Internal(Box::new(left.node), Box::new(right.node)),
        }));
    }

    // Traverse tree to get depths
    fn traverse(node: &TreeNode, depth: u8, lengths: &mut [u8]) {
        match node {
            TreeNode::Leaf(sym) => {
                lengths[*sym] = depth.max(1); // Minimum code length is 1
            }
            TreeNode::Internal(left, right) => {
                traverse(left, depth + 1, lengths);
                traverse(right, depth + 1, lengths);
            }
        }
    }

    if let Some(Reverse(root)) = heap.pop() {
        // Start at depth 0, so direct children of root are at depth 1
        traverse(&root.node, 0, lengths);
    }
}

/// Limit code lengths to max_bits using the algorithm from RFC 1951
fn limit_code_lengths(lengths: &mut [u8], symbols: &[(usize, u32)], max_bits: u8) {
    // Check if any lengths exceed max_bits
    let max_len = lengths.iter().copied().max().unwrap_or(0);
    if max_len <= max_bits {
        return;
    }

    // Count codes at each length
    let mut bl_count = vec![0u32; max_len as usize + 1];
    for &(sym, _) in symbols {
        let len = lengths[sym];
        if len > 0 {
            bl_count[len as usize] += 1;
        }
    }

    // Move codes from lengths > max_bits down to max_bits
    // This requires redistributing to maintain Kraft inequality
    let mut overflow = 0u32;
    for bits in ((max_bits as usize + 1)..=max_len as usize).rev() {
        overflow += bl_count[bits];
        bl_count[bits] = 0;
    }

    // Redistribute overflow by moving codes to longer lengths
    bl_count[max_bits as usize] += overflow;

    // Now we need to shorten some codes to make room
    // Use a greedy approach: for each overflow bit at max_bits,
    // we need to split a shorter code
    while overflow > 0 {
        // Find the shortest length with codes that can be split
        for bits in (1..max_bits as usize).rev() {
            if bl_count[bits] > 0 {
                // Split this code: remove one code at 'bits', add two at 'bits+1'
                bl_count[bits] -= 1;
                bl_count[bits + 1] += 2;
                bl_count[max_bits as usize] -= 1;
                overflow -= 1;
                break;
            }
        }
        // Safety check to prevent infinite loop
        if bl_count[1..(max_bits as usize)].iter().all(|&c| c == 0) {
            break;
        }
    }

    // Reassign lengths based on new distribution
    // Sort symbols by frequency (descending) to assign shorter codes to more frequent
    let mut sorted_syms: Vec<(usize, u32)> = symbols.to_vec();
    sorted_syms.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    // Assign lengths starting from shortest
    let mut sym_idx = 0;
    for (bits, &count) in bl_count.iter().enumerate().skip(1).take(max_bits as usize) {
        for _ in 0..count {
            if sym_idx < sorted_syms.len() {
                lengths[sorted_syms[sym_idx].0] = bits as u8;
                sym_idx += 1;
            }
        }
    }
}

/// Huffman encoder for DEFLATE output
pub struct HuffmanEncoder {
    use_fixed: bool,
    /// Fixed literal/length codes (precomputed)
    fixed_lit_codes: Vec<(u32, u8)>,
    /// Fixed distance codes (precomputed)
    fixed_dist_codes: Vec<(u32, u8)>,
}

impl HuffmanEncoder {
    pub fn new(use_fixed: bool) -> Self {
        let fixed_lit_codes = build_fixed_literal_codes();
        let fixed_dist_codes = build_fixed_distance_codes();

        Self { use_fixed, fixed_lit_codes, fixed_dist_codes }
    }

    /// Encode LZ77 tokens to DEFLATE format
    pub fn encode(&mut self, tokens: &[LZ77Token], is_final: bool) -> Result<Vec<u8>> {
        let mut writer = BitWriter::with_capacity(tokens.len() * 2);

        // Write block header
        writer.write_bit(is_final); // BFINAL
        if self.use_fixed {
            writer.write_bits(1, 2); // BTYPE = 01 (fixed Huffman)
            self.encode_fixed(&mut writer, tokens)?;
        } else {
            writer.write_bits(2, 2); // BTYPE = 10 (dynamic Huffman)
            self.encode_dynamic(&mut writer, tokens)?;
        }

        Ok(writer.finish())
    }

    fn encode_fixed(&self, writer: &mut BitWriter, tokens: &[LZ77Token]) -> Result<()> {
        for token in tokens {
            match token {
                LZ77Token::Literal(byte) => {
                    let (code, len) = self.fixed_lit_codes[*byte as usize];
                    writer.write_bits_reversed(code, len);
                }
                LZ77Token::Copy { length, distance } => {
                    // Encode length
                    if let Some((len_code, extra_val, extra_bits)) = encode_length(*length) {
                        let (code, code_len) = self.fixed_lit_codes[len_code as usize];
                        writer.write_bits_reversed(code, code_len);
                        if extra_bits > 0 {
                            writer.write_bits(extra_val as u32, extra_bits);
                        }
                    }

                    // Encode distance
                    if let Some((dist_code, extra_val, extra_bits)) = encode_distance(*distance) {
                        let (code, code_len) = self.fixed_dist_codes[dist_code as usize];
                        writer.write_bits_reversed(code, code_len);
                        if extra_bits > 0 {
                            writer.write_bits(extra_val as u32, extra_bits);
                        }
                    }
                }
                LZ77Token::EndOfBlock => {
                    // Symbol 256 = end of block
                    let (code, len) = self.fixed_lit_codes[256];
                    writer.write_bits_reversed(code, len);
                }
            }
        }

        // Always write end of block
        let (code, len) = self.fixed_lit_codes[256];
        writer.write_bits_reversed(code, len);

        Ok(())
    }

    /// Encode tokens using dynamic Huffman codes
    fn encode_dynamic(&self, writer: &mut BitWriter, tokens: &[LZ77Token]) -> Result<()> {
        // Count frequencies
        let mut freq = FrequencyCounter::new();
        freq.count_tokens(tokens);

        // Compute optimal code lengths
        let num_lit = freq.num_literal_codes();
        let num_dist = freq.num_distance_codes();

        let mut lit_lengths = compute_code_lengths(&freq.literal_freq[..num_lit], MAX_CODE_LENGTH);
        let mut dist_lengths =
            compute_code_lengths(&freq.distance_freq[..num_dist], MAX_CODE_LENGTH);

        // Ensure EOB (symbol 256) has a valid code - it's always needed
        if lit_lengths.len() > 256 && lit_lengths[256] == 0 {
            lit_lengths[256] = 1;
        }

        // DEFLATE requires at least one distance code even if not used
        // If all distance lengths are 0, set the first one to 1
        if dist_lengths.iter().all(|&l| l == 0) {
            if dist_lengths.is_empty() {
                dist_lengths = vec![1];
            } else {
                dist_lengths[0] = 1;
            }
        }

        // Build codes from lengths
        let lit_codes = build_codes_from_lengths(&lit_lengths);
        let dist_codes = build_codes_from_lengths(&dist_lengths);

        // Write dynamic header
        self.write_dynamic_header(writer, &lit_lengths, &dist_lengths)?;

        // Encode tokens
        self.encode_with_codes(writer, tokens, &lit_codes, &dist_codes)?;

        // Write end of block
        let (code, len) = lit_codes[256];
        writer.write_bits_reversed(code, len);

        Ok(())
    }

    /// Write the dynamic Huffman block header (RFC 1951 section 3.2.7)
    fn write_dynamic_header(
        &self,
        writer: &mut BitWriter,
        lit_lengths: &[u8],
        dist_lengths: &[u8],
    ) -> Result<()> {
        let hlit = lit_lengths.len() - 257; // 0-29
        let hdist = dist_lengths.len() - 1; // 0-31

        // RLE encode the code lengths
        let combined_lengths: Vec<u8> =
            lit_lengths.iter().chain(dist_lengths.iter()).copied().collect();
        let rle_encoded = rle_encode_lengths(&combined_lengths);

        // Count frequencies of code length symbols (0-18)
        let mut cl_freq = [0u32; 19];
        for &(sym, _) in &rle_encoded {
            cl_freq[sym as usize] += 1;
        }

        // Compute code lengths for the code length alphabet (max 7 bits)
        let cl_lengths = compute_code_lengths(&cl_freq, MAX_CL_CODE_LENGTH);
        let cl_codes = build_codes_from_lengths(&cl_lengths);

        // Find HCLEN (number of code length codes to send - 4)
        // Code lengths are sent in special order, find last non-zero
        let mut hclen = 4usize; // Minimum is 4
        for i in (0..19).rev() {
            if cl_lengths[CODE_LENGTH_ORDER[i]] > 0 {
                hclen = i + 1;
                break;
            }
        }
        // Ensure at least 4
        hclen = hclen.max(4);

        // Write header fields
        writer.write_bits(hlit as u32, 5);
        writer.write_bits(hdist as u32, 5);
        writer.write_bits((hclen - 4) as u32, 4);

        // Write code length code lengths (3 bits each, in special order)
        for &sym in CODE_LENGTH_ORDER.iter().take(hclen) {
            writer.write_bits(cl_lengths[sym] as u32, 3);
        }

        // Write RLE-encoded literal/length and distance code lengths
        for &(sym, extra) in &rle_encoded {
            let (code, len) = cl_codes[sym as usize];
            writer.write_bits_reversed(code, len);

            // Write extra bits for RLE symbols
            match sym {
                16 => writer.write_bits(extra as u32, 2), // 3-6 repeats
                17 => writer.write_bits(extra as u32, 3), // 3-10 zeros
                18 => writer.write_bits(extra as u32, 7), // 11-138 zeros
                _ => {}
            }
        }

        Ok(())
    }

    /// Encode tokens using provided Huffman codes
    fn encode_with_codes(
        &self,
        writer: &mut BitWriter,
        tokens: &[LZ77Token],
        lit_codes: &[(u32, u8)],
        dist_codes: &[(u32, u8)],
    ) -> Result<()> {
        for token in tokens {
            match token {
                LZ77Token::Literal(byte) => {
                    let (code, len) = lit_codes[*byte as usize];
                    writer.write_bits_reversed(code, len);
                }
                LZ77Token::Copy { length, distance } => {
                    // Encode length
                    if let Some((len_code, extra_val, extra_bits)) = encode_length(*length) {
                        let (code, code_len) = lit_codes[len_code as usize];
                        writer.write_bits_reversed(code, code_len);
                        if extra_bits > 0 {
                            writer.write_bits(extra_val as u32, extra_bits);
                        }
                    }

                    // Encode distance
                    if let Some((dist_code, extra_val, extra_bits)) = encode_distance(*distance) {
                        let (code, code_len) = dist_codes[dist_code as usize];
                        writer.write_bits_reversed(code, code_len);
                        if extra_bits > 0 {
                            writer.write_bits(extra_val as u32, extra_bits);
                        }
                    }
                }
                LZ77Token::EndOfBlock => {
                    let (code, len) = lit_codes[256];
                    writer.write_bits_reversed(code, len);
                }
            }
        }
        Ok(())
    }
}

/// RLE encode code lengths using symbols 16, 17, 18
fn rle_encode_lengths(lengths: &[u8]) -> Vec<(u8, u8)> {
    let mut result = Vec::new();
    let mut i = 0;

    while i < lengths.len() {
        let len = lengths[i];

        // Count consecutive same values
        let mut run = 1;
        while i + run < lengths.len() && lengths[i + run] == len {
            run += 1;
        }

        if len == 0 {
            // Encode runs of zeros with symbols 17 or 18
            while run > 0 {
                if run >= 11 {
                    // Symbol 18: 11-138 zeros
                    let count = run.min(138);
                    result.push((18, (count - 11) as u8));
                    run -= count;
                } else if run >= 3 {
                    // Symbol 17: 3-10 zeros
                    let count = run.min(10);
                    result.push((17, (count - 3) as u8));
                    run -= count;
                } else {
                    // Output literal zeros
                    result.push((0, 0));
                    run -= 1;
                }
            }
        } else {
            // Output the first length
            result.push((len, 0));
            run -= 1;

            // Encode remaining with symbol 16 (repeat previous)
            while run > 0 {
                if run >= 3 {
                    let count = run.min(6);
                    result.push((16, (count - 3) as u8));
                    run -= count;
                } else {
                    result.push((len, 0));
                    run -= 1;
                }
            }
        }

        i += lengths[i..].iter().take_while(|&&l| l == len).count();
    }

    result
}

/// Build fixed Huffman codes for literals/lengths (RFC 1951 section 3.2.6)
fn build_fixed_literal_codes() -> Vec<(u32, u8)> {
    let lengths = super::tables::fixed_literal_lengths();
    build_codes_from_lengths(&lengths)
}

/// Build fixed Huffman codes for distances
fn build_fixed_distance_codes() -> Vec<(u32, u8)> {
    let lengths = super::tables::fixed_distance_lengths();
    build_codes_from_lengths(&lengths)
}

/// Build canonical Huffman codes from code lengths
fn build_codes_from_lengths(lengths: &[u8]) -> Vec<(u32, u8)> {
    let max_bits = *lengths.iter().max().unwrap_or(&0);

    // Count codes of each length
    let mut bl_count = vec![0u32; max_bits as usize + 1];
    for &len in lengths {
        if len > 0 {
            bl_count[len as usize] += 1;
        }
    }

    // Compute first code for each bit length
    let mut next_code = vec![0u32; max_bits as usize + 1];
    let mut code = 0u32;
    for bits in 1..=max_bits as usize {
        code = (code + bl_count[bits - 1]) << 1;
        next_code[bits] = code;
    }

    // Assign codes to symbols
    let mut codes = vec![(0u32, 0u8); lengths.len()];
    for (sym, &len) in lengths.iter().enumerate() {
        if len > 0 {
            codes[sym] = (next_code[len as usize], len);
            next_code[len as usize] += 1;
        }
    }

    codes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_fixed_literal_codes() {
        let codes = build_fixed_literal_codes();
        assert_eq!(codes.len(), 288);

        // Check some known codes (RFC 1951 section 3.2.6)
        // Symbols 0-143: 8 bits, codes 00110000 - 10111111
        assert_eq!(codes[0].1, 8); // 8-bit code
        assert_eq!(codes[143].1, 8); // 8-bit code

        // Symbols 144-255: 9 bits, codes 110010000 - 111111111
        assert_eq!(codes[144].1, 9);
        assert_eq!(codes[255].1, 9);

        // Symbols 256-279: 7 bits, codes 0000000 - 0010111
        assert_eq!(codes[256].1, 7); // End of block
        assert_eq!(codes[279].1, 7);

        // Symbols 280-287: 8 bits, codes 11000000 - 11000111
        assert_eq!(codes[280].1, 8);
        assert_eq!(codes[287].1, 8);
    }

    #[test]
    fn test_encode_literals() {
        let mut encoder = HuffmanEncoder::new(true);
        let tokens = vec![LZ77Token::Literal(b'H'), LZ77Token::Literal(b'i')];
        let data = encoder.encode(&tokens, true).unwrap();
        assert!(!data.is_empty());
    }

    #[test]
    fn test_encode_dynamic() {
        let mut encoder = HuffmanEncoder::new(false); // Use dynamic
        let tokens = vec![
            LZ77Token::Literal(b'H'),
            LZ77Token::Literal(b'e'),
            LZ77Token::Literal(b'l'),
            LZ77Token::Literal(b'l'),
            LZ77Token::Literal(b'o'),
        ];
        let data = encoder.encode(&tokens, true).unwrap();
        assert!(!data.is_empty());
        // Dynamic block type should be in the header (bits 1-2 = 10)
        // First byte contains BFINAL (bit 0) and BTYPE (bits 1-2)
        // BFINAL=1, BTYPE=10 -> binary: 101 = 5 (in first 3 bits, LSB first)
        assert_eq!(data[0] & 0x07, 0x05); // 101 binary = final + dynamic
    }

    #[test]
    fn test_frequency_counter() {
        let mut freq = FrequencyCounter::new();
        let tokens = vec![
            LZ77Token::Literal(b'a'),
            LZ77Token::Literal(b'a'),
            LZ77Token::Literal(b'b'),
            LZ77Token::Copy { length: 3, distance: 1 },
        ];
        freq.count_tokens(&tokens);

        assert_eq!(freq.literal_freq[b'a' as usize], 2);
        assert_eq!(freq.literal_freq[b'b' as usize], 1);
        assert_eq!(freq.literal_freq[256], 1); // EOB always counted
                                               // Length 3 -> code 257
        assert_eq!(freq.literal_freq[257], 1);
        // Distance 1 -> code 0
        assert_eq!(freq.distance_freq[0], 1);
    }

    #[test]
    fn test_compute_code_lengths() {
        // Simple case: 4 symbols with equal frequency
        let freqs = [1u32, 1, 1, 1];
        let lengths = compute_code_lengths(&freqs, 15);
        // All symbols should have codes (length > 0)
        assert!(lengths.iter().all(|&l| l > 0));
        // Code lengths should be reasonable (2-3 bits for 4 symbols)
        assert!(lengths.iter().all(|&l| l <= 3));
        // Should satisfy Kraft inequality: sum of 2^(-len) <= 1
        let kraft: f64 = lengths.iter().map(|&l| 2f64.powi(-(l as i32))).sum();
        assert!(kraft <= 1.0 + 0.001); // Allow small floating point error
    }

    #[test]
    fn test_compute_code_lengths_skewed() {
        // Skewed: one very common symbol
        let freqs = [100u32, 1, 1, 1];
        let lengths = compute_code_lengths(&freqs, 15);
        // Most frequent should have shortest code
        assert!(lengths[0] <= lengths[1]);
        assert!(lengths[0] <= lengths[2]);
        assert!(lengths[0] <= lengths[3]);
    }

    #[test]
    fn test_rle_encode_zeros() {
        // Test RLE encoding of zeros
        let lengths = vec![0u8; 20];
        let encoded = rle_encode_lengths(&lengths);
        // Should use symbol 18 (11-138 zeros) for run of 20
        assert_eq!(encoded.len(), 1);
        assert_eq!(encoded[0].0, 18); // Symbol 18
        assert_eq!(encoded[0].1, 9); // 20 - 11 = 9
    }

    #[test]
    fn test_rle_encode_repeat() {
        // Test RLE encoding of repeated non-zero values
        let lengths = vec![5u8; 10];
        let encoded = rle_encode_lengths(&lengths);
        // Should be: 5, then symbol 16 (repeat) twice
        // First 5, then repeat 6 (max for symbol 16), then repeat 3
        assert!(encoded.len() >= 2);
        assert_eq!(encoded[0].0, 5); // First literal
    }
}
