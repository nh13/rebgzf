use super::boundary::BoundaryResolver;
use super::window::SlidingWindow;
use crate::bgzf::BgzfBlockWriter;
use crate::deflate::{DeflateParser, LZ77Token};
use crate::error::Result;
use crate::gzip::GzipHeader;
use crate::huffman::HuffmanEncoder;
use crate::TranscodeConfig;
use crate::TranscodeStats;
use crate::Transcoder;
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

        // Phase 1: Parse gzip header
        let _gzip_header = GzipHeader::parse(&mut reader)?;

        // Phase 2: Initialize components
        let mut parser = DeflateParser::new(&mut reader);
        let mut resolver = BoundaryResolver::new();
        let mut encoder = HuffmanEncoder::new(self.config.use_fixed_huffman);
        let mut bgzf_writer = BgzfBlockWriter::new(&mut writer);

        // Accumulator for current BGZF block
        let mut pending_tokens: Vec<LZ77Token> = Vec::with_capacity(8192);
        let mut pending_uncompressed_size: usize = 0;
        let mut block_start_position: u64 = 0;

        // For collecting uncompressed data for CRC (reserved for future use)
        let _uncompressed_collector = SlidingWindow::new();
        let _block_uncompressed: Vec<u8> = Vec::with_capacity(self.config.block_size);

        // Statistics
        let mut stats = TranscodeStats::default();

        // Phase 3: Main transcoding loop
        while let Some(deflate_block) = parser.parse_block()? {
            // Process each token from the DEFLATE block
            for token in deflate_block.tokens.iter() {
                // Skip EndOfBlock tokens from input
                if matches!(token, LZ77Token::EndOfBlock) {
                    continue;
                }

                let token_size = token.uncompressed_size();

                // Check if adding this token would exceed BGZF block size
                if pending_uncompressed_size + token_size > self.config.block_size
                    && !pending_tokens.is_empty()
                {
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
                }

                // Add token to pending
                pending_tokens.push(token.clone());
                pending_uncompressed_size += token_size;
            }

            stats.input_bytes = parser.bytes_read();
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
        // Resolve cross-boundary references
        let resolved = resolver.resolve_block(block_start, tokens);

        // Collect uncompressed data for CRC
        let uncompressed = collect_uncompressed(&resolved);

        // Encode to DEFLATE (is_final = true for each BGZF block)
        let deflate_data = encoder.encode(&resolved, true)?;

        // Write BGZF block
        bgzf_writer.write_block(&deflate_data, &uncompressed)?;

        // Update stats
        stats.blocks_written += 1;
        stats.output_bytes += (18 + deflate_data.len() + 8) as u64;

        Ok(())
    }
}

/// Collect uncompressed bytes from resolved tokens (needed for CRC)
fn collect_uncompressed(tokens: &[LZ77Token]) -> Vec<u8> {
    let mut result = Vec::new();
    let mut window = SlidingWindow::new();

    for token in tokens {
        match token {
            LZ77Token::Literal(byte) => {
                result.push(*byte);
                window.push_byte(*byte);
            }
            LZ77Token::Copy { length, distance } => {
                let bytes = window.get(*distance, *length);
                for byte in &bytes {
                    result.push(*byte);
                    window.push_byte(*byte);
                }
            }
            LZ77Token::EndOfBlock => {}
        }
    }

    result
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
