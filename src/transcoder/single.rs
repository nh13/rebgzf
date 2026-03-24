use super::boundary::BoundaryResolver;
use super::splitter::{BlockSplitter, DefaultSplitter, FastqSplitter};
use crate::bgzf::{BgzfBlockWriter, GziIndexBuilder};
use crate::bits::BitRead;
use crate::deflate::{DeflateParser, LZ77Token};
use crate::error::Result;
use crate::gzip::GzipHeader;
use crate::huffman::HuffmanEncoder;
use crate::{FormatProfile, TranscodeConfig, TranscodeStats, Transcoder};
use std::io::{BufReader, BufWriter, Read, Write};

/// Single-threaded transcoder implementation
pub struct SingleThreadedTranscoder {
    config: TranscodeConfig,
}

impl SingleThreadedTranscoder {
    pub fn new(config: TranscodeConfig) -> Self {
        Self { config }
    }

    /// Transcode from a byte slice (e.g., mmap'd file) to a writer.
    /// Uses `SliceBitReader` for maximum parsing performance.
    pub fn transcode_slice<W: Write>(&mut self, data: &[u8], output: W) -> Result<TranscodeStats> {
        let mut writer = BufWriter::with_capacity(self.config.buffer_size, output);

        // Parse gzip header from the raw bytes
        let header_size = parse_gzip_header_size(data)?;

        // Create DEFLATE parser using slice-backed bit reader
        let mut parser = DeflateParser::from_slice(data, header_size);
        let mut bgzf_writer = BgzfBlockWriter::new(&mut writer);

        let stats = self.transcode_core(&mut parser, &mut bgzf_writer)?;

        let _ = bgzf_writer.finish()?;
        Ok(stats)
    }

    /// Core transcoding loop, generic over the bit reader type.
    fn transcode_core<B: BitRead, W: Write>(
        &self,
        parser: &mut DeflateParser<B>,
        bgzf_writer: &mut BgzfBlockWriter<W>,
    ) -> Result<TranscodeStats> {
        let mut resolver = BoundaryResolver::new();
        let mut encoder = HuffmanEncoder::new(self.config.use_fixed_huffman());

        // Create splitter based on config
        let use_smart = self.config.use_smart_boundaries();
        let mut splitter: Box<dyn BlockSplitter> =
            if use_smart && self.config.format == FormatProfile::Fastq {
                Box::new(FastqSplitter::new())
            } else {
                Box::new(DefaultSplitter)
            };

        // Maximum block size with overshoot allowance for smart boundaries
        let max_block_size = if use_smart {
            (self.config.block_size as f64 * 1.1) as usize
        } else {
            self.config.block_size
        };

        // Accumulator for current BGZF block — larger initial capacity to reduce reallocs
        let mut pending_tokens: Vec<LZ77Token> = Vec::with_capacity(32768);
        let mut pending_uncompressed_size: usize = 0;
        let mut block_start_position: u64 = 0;

        // Optional index builder
        let mut index_builder =
            if self.config.build_index { Some(GziIndexBuilder::new()) } else { None };

        let mut stats = TranscodeStats::default();

        // Main transcoding loop — handles multiple gzip members
        loop {
            while let Some(deflate_block) = parser.parse_block()? {
                for token in deflate_block.tokens {
                    if matches!(token, LZ77Token::EndOfBlock) {
                        continue;
                    }

                    let token_size = token.uncompressed_size();
                    splitter.process_token(&token);

                    let should_emit = if use_smart {
                        let near_target =
                            pending_uncompressed_size + token_size >= self.config.block_size;
                        let at_good_split = splitter.is_good_split_point();
                        let exceeds_max = pending_uncompressed_size + token_size > max_block_size;

                        !pending_tokens.is_empty()
                            && ((near_target && at_good_split) || exceeds_max)
                    } else {
                        pending_uncompressed_size + token_size > self.config.block_size
                            && !pending_tokens.is_empty()
                    };

                    if should_emit {
                        emit_block(
                            &self.config,
                            &mut resolver,
                            &mut encoder,
                            bgzf_writer,
                            &pending_tokens,
                            block_start_position,
                            &mut stats,
                            &mut index_builder,
                        )?;

                        block_start_position = resolver.position();
                        pending_tokens.clear();
                        pending_uncompressed_size = 0;
                        splitter.reset();
                    }

                    pending_tokens.push(token);
                    pending_uncompressed_size += token_size;
                }
            }

            stats.input_bytes = parser.bytes_read();

            if !parser.read_trailer_and_check_next()? {
                break;
            }
        }

        // Flush remaining tokens
        if !pending_tokens.is_empty() {
            emit_block(
                &self.config,
                &mut resolver,
                &mut encoder,
                bgzf_writer,
                &pending_tokens,
                block_start_position,
                &mut stats,
                &mut index_builder,
            )?;
        }

        // Write EOF
        bgzf_writer.write_eof()?;
        stats.output_bytes += 28;

        let (resolved, _preserved) = resolver.stats();
        stats.boundary_refs_resolved = resolved;
        stats.index_entries = index_builder.map(|b| b.entries().to_vec());

        Ok(stats)
    }
}

impl Transcoder for SingleThreadedTranscoder {
    fn transcode<R: Read, W: Write>(&mut self, input: R, output: W) -> Result<TranscodeStats> {
        let mut reader = BufReader::with_capacity(self.config.buffer_size, input);
        let mut writer = BufWriter::with_capacity(self.config.buffer_size, output);

        // Parse first gzip header
        let _gzip_header = GzipHeader::parse(&mut reader)?;

        let mut parser = DeflateParser::new(&mut reader);
        let mut bgzf_writer = BgzfBlockWriter::new(&mut writer);

        let stats = self.transcode_core(&mut parser, &mut bgzf_writer)?;

        let _ = bgzf_writer.finish()?;
        Ok(stats)
    }
}

/// Emit a single BGZF block from pending tokens.
/// Uses fused resolve+encode for fixed Huffman (one pass, no intermediate Vec).
#[allow(clippy::too_many_arguments)]
fn emit_block<W: Write>(
    config: &TranscodeConfig,
    resolver: &mut BoundaryResolver,
    encoder: &mut HuffmanEncoder,
    bgzf_writer: &mut BgzfBlockWriter<W>,
    tokens: &[LZ77Token],
    block_start: u64,
    stats: &mut TranscodeStats,
    index_builder: &mut Option<GziIndexBuilder>,
) -> Result<()> {
    let (deflate_data, crc, uncompressed_size) = if config.use_fixed_huffman() {
        // Fused path: resolve + encode in one pass (no intermediate token Vec)
        resolver.resolve_and_encode_fixed(block_start, tokens, encoder)
    } else {
        // Two-pass path: resolve first, then encode (dynamic Huffman needs frequency pass)
        let (resolved, crc, uncompressed_size) = resolver.resolve_block(block_start, tokens);
        let deflate_data = encoder.encode(&resolved, true)?;
        (deflate_data, crc, uncompressed_size)
    };

    bgzf_writer.write_block_with_crc(&deflate_data, crc, uncompressed_size)?;

    let compressed_block_size = (18 + deflate_data.len() + 8) as u64;

    if let Some(ref mut builder) = index_builder {
        builder.add_block(compressed_block_size, uncompressed_size as u64);
    }

    stats.blocks_written += 1;
    stats.output_bytes += compressed_block_size;

    Ok(())
}

/// Parse a gzip header from raw bytes and return the byte offset where DEFLATE data starts.
pub fn parse_gzip_header_size(data: &[u8]) -> Result<usize> {
    use crate::error::Error;

    if data.len() < 10 {
        return Err(Error::UnexpectedEof);
    }

    let magic = u16::from_le_bytes([data[0], data[1]]);
    if magic != 0x8b1f {
        return Err(Error::InvalidGzipMagic(magic));
    }

    if data[2] != 8 {
        return Err(Error::UnsupportedCompressionMethod(data[2]));
    }

    let flags = data[3];

    // Reject reserved flag bits (bits 5-7). RFC 1952 section 2.3.1 says these are reserved
    // and a compliant decompressor must reject members with any of these bits set.
    if flags & 0xE0 != 0 {
        return Err(Error::Internal(format!("Reserved gzip flags set: 0x{:02x}", flags & 0xE0)));
    }

    let mut pos = 10; // Past fixed header

    const FHCRC: u8 = 1 << 1;
    const FEXTRA: u8 = 1 << 2;
    const FNAME: u8 = 1 << 3;
    const FCOMMENT: u8 = 1 << 4;

    // FEXTRA
    if flags & FEXTRA != 0 {
        if pos + 2 > data.len() {
            return Err(Error::UnexpectedEof);
        }
        let xlen = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2 + xlen;
    }

    // FNAME (null-terminated)
    if flags & FNAME != 0 {
        while pos < data.len() && data[pos] != 0 {
            pos += 1;
        }
        pos += 1; // Skip null terminator
    }

    // FCOMMENT (null-terminated)
    if flags & FCOMMENT != 0 {
        while pos < data.len() && data[pos] != 0 {
            pos += 1;
        }
        pos += 1;
    }

    // FHCRC
    if flags & FHCRC != 0 {
        pos += 2;
    }

    if pos > data.len() {
        return Err(Error::UnexpectedEof);
    }

    Ok(pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_transcode_simple() {
        use std::io::Write as IoWrite;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"Hello, World!").unwrap();
        let gzip_data = encoder.finish().unwrap();

        let config = TranscodeConfig::default();
        let mut transcoder = SingleThreadedTranscoder::new(config);

        let mut output = Vec::new();
        let stats = transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

        assert!(stats.blocks_written >= 1);
        assert!(!output.is_empty());

        assert_eq!(output[0], 0x1f);
        assert_eq!(output[1], 0x8b);
        assert_eq!(output[3] & 0x04, 0x04);
        assert_eq!(output[12], b'B');
        assert_eq!(output[13], b'C');
    }

    #[test]
    fn test_transcode_with_compression() {
        use std::io::Write as IoWrite;

        let data = b"ABCDABCDABCDABCDABCDABCDABCDABCD";

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        let gzip_data = encoder.finish().unwrap();

        let config = TranscodeConfig::default();
        let mut transcoder = SingleThreadedTranscoder::new(config);

        let mut output = Vec::new();
        let stats = transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

        assert!(stats.blocks_written >= 1);
    }

    #[test]
    fn test_transcode_slice() {
        use std::io::Write as IoWrite;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"Hello, World! This is a test of slice-based transcoding.").unwrap();
        let gzip_data = encoder.finish().unwrap();

        let config = TranscodeConfig::default();
        let mut transcoder = SingleThreadedTranscoder::new(config);

        let mut output = Vec::new();
        let stats = transcoder.transcode_slice(&gzip_data, &mut output).unwrap();

        assert!(stats.blocks_written >= 1);
        assert!(!output.is_empty());

        // Verify BGZF header
        assert_eq!(output[0], 0x1f);
        assert_eq!(output[1], 0x8b);
        assert_eq!(output[12], b'B');
        assert_eq!(output[13], b'C');
    }

    #[test]
    fn test_transcode_slice_matches_stream() {
        use std::io::Write as IoWrite;

        // Create test data with enough content to exercise Copy tokens
        let mut test_data = Vec::new();
        for i in 0..1000 {
            test_data.extend_from_slice(format!("Line {} ABCDEFGHIJKLMNOP\n", i).as_bytes());
        }

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&test_data).unwrap();
        let gzip_data = encoder.finish().unwrap();

        // Transcode via Read path
        let config = TranscodeConfig::default();
        let mut transcoder1 = SingleThreadedTranscoder::new(config.clone());
        let mut output1 = Vec::new();
        transcoder1.transcode(Cursor::new(&gzip_data), &mut output1).unwrap();

        // Transcode via slice path
        let mut transcoder2 = SingleThreadedTranscoder::new(config);
        let mut output2 = Vec::new();
        transcoder2.transcode_slice(&gzip_data, &mut output2).unwrap();

        // Outputs should be identical
        assert_eq!(
            output1, output2,
            "Slice and stream transcoding should produce identical output"
        );
    }
}
