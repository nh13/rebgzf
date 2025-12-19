use super::boundary::BoundaryResolver;
use super::splitter::{BlockSplitter, DefaultSplitter, FastqSplitter};
use crate::bgzf::BgzfBlockWriter;
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
}

impl Transcoder for SingleThreadedTranscoder {
    fn transcode<R: Read, W: Write>(&mut self, input: R, output: W) -> Result<TranscodeStats> {
        let mut reader = BufReader::with_capacity(self.config.buffer_size, input);
        let mut writer = BufWriter::with_capacity(self.config.buffer_size, output);

        // Phase 1: Parse first gzip header
        let _gzip_header = GzipHeader::parse(&mut reader)?;

        // Phase 2: Initialize components
        let mut parser = DeflateParser::new(&mut reader);
        let mut resolver = BoundaryResolver::new();
        let mut encoder = HuffmanEncoder::new(self.config.use_fixed_huffman());
        let mut bgzf_writer = BgzfBlockWriter::new(&mut writer);

        // Create splitter based on config
        let use_smart = self.config.use_smart_boundaries();
        let mut splitter: Box<dyn BlockSplitter> =
            if use_smart && self.config.format == FormatProfile::Fastq {
                Box::new(FastqSplitter::new())
            } else {
                Box::new(DefaultSplitter)
            };

        // Maximum block size with overshoot allowance for smart boundaries
        // Allow up to 10% overshoot to find a good split point
        let max_block_size = if use_smart {
            (self.config.block_size as f64 * 1.1) as usize
        } else {
            self.config.block_size
        };

        // Accumulator for current BGZF block
        let mut pending_tokens: Vec<LZ77Token> = Vec::with_capacity(8192);
        let mut pending_uncompressed_size: usize = 0;
        let mut block_start_position: u64 = 0;

        // Statistics
        let mut stats = TranscodeStats::default();

        // Phase 3: Main transcoding loop - handles multiple gzip members
        loop {
            // Process all DEFLATE blocks in current gzip member
            while let Some(deflate_block) = parser.parse_block()? {
                // Process each token from the DEFLATE block (take ownership to avoid cloning)
                for token in deflate_block.tokens {
                    // Skip EndOfBlock tokens from input
                    if matches!(token, LZ77Token::EndOfBlock) {
                        continue;
                    }

                    let token_size = token.uncompressed_size();

                    // Update splitter with this token
                    splitter.process_token(&token);

                    // Determine if we should emit a block
                    let should_emit = if use_smart {
                        // Smart mode: emit when at good split point near target size,
                        // or when we exceed max size
                        let near_target =
                            pending_uncompressed_size + token_size >= self.config.block_size;
                        let at_good_split = splitter.is_good_split_point();
                        let exceeds_max = pending_uncompressed_size + token_size > max_block_size;

                        !pending_tokens.is_empty()
                            && ((near_target && at_good_split) || exceeds_max)
                    } else {
                        // Simple mode: emit when exceeding target size
                        pending_uncompressed_size + token_size > self.config.block_size
                            && !pending_tokens.is_empty()
                    };

                    if should_emit {
                        // Emit current BGZF block
                        self.emit_block(
                            &mut resolver,
                            &mut encoder,
                            &mut bgzf_writer,
                            &pending_tokens,
                            block_start_position,
                            &mut stats,
                        )?;

                        block_start_position = resolver.position();
                        pending_tokens.clear();
                        pending_uncompressed_size = 0;
                        splitter.reset();
                    }

                    // Add token to pending (no clone needed - we own the token)
                    pending_tokens.push(token);
                    pending_uncompressed_size += token_size;
                }
            }

            stats.input_bytes = parser.bytes_read();

            // Check for another gzip member
            if !parser.read_trailer_and_check_next()? {
                break; // No more members, we're done
            }
            // Continue with next member - parser state has been reset
        }

        // Phase 5: Flush remaining tokens
        if !pending_tokens.is_empty() {
            self.emit_block(
                &mut resolver,
                &mut encoder,
                &mut bgzf_writer,
                &pending_tokens,
                block_start_position,
                &mut stats,
            )?;
        }

        // Phase 6: Write EOF
        bgzf_writer.write_eof()?;
        stats.output_bytes += 28; // EOF block size

        let (resolved, _preserved) = resolver.stats();
        stats.boundary_refs_resolved = resolved;

        // Flush writer
        let _ = bgzf_writer.finish()?;

        Ok(stats)
    }
}

impl SingleThreadedTranscoder {
    fn emit_block<W: Write>(
        &self,
        resolver: &mut BoundaryResolver,
        encoder: &mut HuffmanEncoder,
        bgzf_writer: &mut BgzfBlockWriter<W>,
        tokens: &[LZ77Token],
        block_start: u64,
        stats: &mut TranscodeStats,
    ) -> Result<()> {
        // Resolve cross-boundary references (also computes CRC)
        let (resolved, crc, uncompressed_size) = resolver.resolve_block(block_start, tokens);

        // Encode to DEFLATE (is_final = true for each BGZF block)
        let deflate_data = encoder.encode(&resolved, true)?;

        // Write BGZF block with pre-computed CRC
        bgzf_writer.write_block_with_crc(&deflate_data, crc, uncompressed_size)?;

        // Update stats
        stats.blocks_written += 1;
        stats.output_bytes += (18 + deflate_data.len() + 8) as u64;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_transcode_simple() {
        // Create a simple gzip file
        use std::io::Write as IoWrite;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"Hello, World!").unwrap();
        let gzip_data = encoder.finish().unwrap();

        // Transcode
        let config = TranscodeConfig::default();
        let mut transcoder = SingleThreadedTranscoder::new(config);

        let mut output = Vec::new();
        let stats = transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

        assert!(stats.blocks_written >= 1);
        assert!(!output.is_empty());

        // Verify it's valid BGZF by checking header
        assert_eq!(output[0], 0x1f);
        assert_eq!(output[1], 0x8b);
        assert_eq!(output[3] & 0x04, 0x04); // FEXTRA flag
        assert_eq!(output[12], b'B');
        assert_eq!(output[13], b'C');
    }

    #[test]
    fn test_transcode_with_compression() {
        use std::io::Write as IoWrite;

        // Create data with repeating patterns (will use LZ77)
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
}
