use super::constants::*;
use crate::error::{Error, Result};
use std::io::Write;

/// Writes BGZF blocks with custom deflate data
pub struct BgzfBlockWriter<W: Write> {
    writer: W,
}

impl<W: Write> BgzfBlockWriter<W> {
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    /// Write a BGZF block with pre-encoded deflate data
    pub fn write_block(&mut self, deflate_data: &[u8], uncompressed: &[u8]) -> Result<()> {
        let block_size = BGZF_HEADER_SIZE + deflate_data.len() + BGZF_FOOTER_SIZE;

        if block_size > MAX_BGZF_BLOCK_SIZE {
            return Err(Error::BgzfBlockTooLarge { size: block_size, max: MAX_BGZF_BLOCK_SIZE });
        }

        // Calculate CRC32
        let crc = crc32fast::hash(uncompressed);

        // Write BGZF header
        self.write_header(block_size - 1)?; // BSIZE is block_size - 1

        // Write deflate data
        self.writer.write_all(deflate_data)?;

        // Write footer: CRC32 + ISIZE
        self.writer.write_all(&crc.to_le_bytes())?;
        self.writer.write_all(&(uncompressed.len() as u32).to_le_bytes())?;

        Ok(())
    }

    /// Write the BGZF header (18 bytes)
    fn write_header(&mut self, bsize: usize) -> Result<()> {
        let header = [
            0x1f,
            0x8b, // gzip magic
            0x08, // compression method (DEFLATE)
            0x04, // flags (FEXTRA)
            0x00,
            0x00,
            0x00,
            0x00, // mtime
            0x00, // extra flags
            0xff, // OS (unknown)
            0x06,
            0x00, // xlen = 6
            0x42,
            0x43, // subfield ID "BC"
            0x02,
            0x00,                        // subfield length = 2
            (bsize & 0xFF) as u8,        // BSIZE low byte
            ((bsize >> 8) & 0xFF) as u8, // BSIZE high byte
        ];
        self.writer.write_all(&header)?;
        Ok(())
    }

    /// Write the BGZF EOF marker
    pub fn write_eof(&mut self) -> Result<()> {
        self.writer.write_all(&BGZF_EOF)?;
        Ok(())
    }

    /// Flush and finish writing
    pub fn finish(mut self) -> Result<W> {
        self.writer.flush()?;
        Ok(self.writer)
    }

    /// Get a reference to the inner writer
    pub fn get_ref(&self) -> &W {
        &self.writer
    }

    /// Get a mutable reference to the inner writer
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.writer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_eof() {
        let mut output = Vec::new();
        let mut writer = BgzfBlockWriter::new(&mut output);
        writer.write_eof().unwrap();

        assert_eq!(output, BGZF_EOF);
    }

    #[test]
    fn test_write_block() {
        let mut output = Vec::new();
        let mut writer = BgzfBlockWriter::new(&mut output);

        // Simple deflate block (stored, empty)
        let deflate = vec![0x01, 0x00, 0x00, 0xff, 0xff]; // Empty stored block
        let uncompressed = vec![];

        writer.write_block(&deflate, &uncompressed).unwrap();

        // Check header
        assert_eq!(output[0], 0x1f); // gzip magic
        assert_eq!(output[1], 0x8b);
        assert_eq!(output[2], 0x08); // DEFLATE
        assert_eq!(output[3], 0x04); // FEXTRA

        // Check BC subfield
        assert_eq!(output[12], b'B');
        assert_eq!(output[13], b'C');

        // Check total size
        let bsize = u16::from_le_bytes([output[16], output[17]]) as usize + 1;
        assert_eq!(output.len(), bsize);
    }
}
