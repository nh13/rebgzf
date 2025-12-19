use crate::error::{Error, Result};
use std::io::Read;

/// Gzip header flags (RFC 1952)
const FTEXT: u8 = 1 << 0;
const FHCRC: u8 = 1 << 1;
const FEXTRA: u8 = 1 << 2;
const FNAME: u8 = 1 << 3;
const FCOMMENT: u8 = 1 << 4;

/// Parsed gzip header (RFC 1952)
#[derive(Debug, Clone)]
pub struct GzipHeader {
    pub compression_method: u8,
    pub flags: u8,
    pub mtime: u32,
    pub extra_flags: u8,
    pub os: u8,
    pub extra: Option<Vec<u8>>,
    pub filename: Option<String>,
    pub comment: Option<String>,
    pub header_crc: Option<u16>,
}

impl GzipHeader {
    /// Parse a gzip header from a reader
    pub fn parse<R: Read>(reader: &mut R) -> Result<Self> {
        let mut buf = [0u8; 10];
        reader.read_exact(&mut buf).map_err(|_| Error::UnexpectedEof)?;

        // Check magic bytes
        let magic = u16::from_le_bytes([buf[0], buf[1]]);
        if magic != 0x8b1f {
            return Err(Error::InvalidGzipMagic(magic));
        }

        // Compression method (must be 8 for DEFLATE)
        let compression_method = buf[2];
        if compression_method != 8 {
            return Err(Error::UnsupportedCompressionMethod(compression_method));
        }

        let flags = buf[3];
        let mtime = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let extra_flags = buf[8];
        let os = buf[9];

        // Parse optional fields based on flags
        let extra = if flags & FEXTRA != 0 {
            let mut xlen_buf = [0u8; 2];
            reader.read_exact(&mut xlen_buf).map_err(|_| Error::UnexpectedEof)?;
            let xlen = u16::from_le_bytes(xlen_buf) as usize;

            let mut extra_data = vec![0u8; xlen];
            reader.read_exact(&mut extra_data).map_err(|_| Error::UnexpectedEof)?;
            Some(extra_data)
        } else {
            None
        };

        let filename =
            if flags & FNAME != 0 { Some(read_null_terminated_string(reader)?) } else { None };

        let comment =
            if flags & FCOMMENT != 0 { Some(read_null_terminated_string(reader)?) } else { None };

        let header_crc = if flags & FHCRC != 0 {
            let mut crc_buf = [0u8; 2];
            reader.read_exact(&mut crc_buf).map_err(|_| Error::UnexpectedEof)?;
            Some(u16::from_le_bytes(crc_buf))
        } else {
            None
        };

        Ok(GzipHeader {
            compression_method,
            flags,
            mtime,
            extra_flags,
            os,
            extra,
            filename,
            comment,
            header_crc,
        })
    }

    /// Check if the FTEXT flag is set
    pub fn is_text(&self) -> bool {
        self.flags & FTEXT != 0
    }

    /// Check if the FEXTRA flag is set
    pub fn has_extra(&self) -> bool {
        self.flags & FEXTRA != 0
    }

    /// Check if the FNAME flag is set
    pub fn has_filename(&self) -> bool {
        self.flags & FNAME != 0
    }

    /// Check if the FCOMMENT flag is set
    pub fn has_comment(&self) -> bool {
        self.flags & FCOMMENT != 0
    }

    /// Check if the FHCRC flag is set
    pub fn has_header_crc(&self) -> bool {
        self.flags & FHCRC != 0
    }
}

/// Gzip trailer (8 bytes at end of file)
#[derive(Debug, Clone)]
pub struct GzipTrailer {
    pub crc32: u32,
    pub isize: u32,
}

impl GzipTrailer {
    /// Parse a gzip trailer from a reader
    pub fn parse<R: Read>(reader: &mut R) -> Result<Self> {
        let mut buf = [0u8; 8];
        reader.read_exact(&mut buf).map_err(|_| Error::UnexpectedEof)?;

        let crc32 = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let isize = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);

        Ok(GzipTrailer { crc32, isize })
    }
}

/// Read a null-terminated string from a reader
fn read_null_terminated_string<R: Read>(reader: &mut R) -> Result<String> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];

    loop {
        reader.read_exact(&mut byte).map_err(|_| Error::UnexpectedEof)?;
        if byte[0] == 0 {
            break;
        }
        bytes.push(byte[0]);
    }

    // Gzip uses ISO-8859-1 (Latin-1), but we'll try UTF-8 first
    String::from_utf8(bytes.clone()).or_else(|_| Ok(bytes.iter().map(|&b| b as char).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_parse_minimal_header() {
        // Minimal gzip header: magic, method, flags=0, mtime=0, xfl, os
        let data = vec![
            0x1f, 0x8b, // magic
            0x08, // method (DEFLATE)
            0x00, // flags
            0x00, 0x00, 0x00, 0x00, // mtime
            0x00, // extra flags
            0xff, // OS (unknown)
        ];

        let mut cursor = Cursor::new(data);
        let header = GzipHeader::parse(&mut cursor).unwrap();

        assert_eq!(header.compression_method, 8);
        assert_eq!(header.flags, 0);
        assert_eq!(header.mtime, 0);
        assert!(header.extra.is_none());
        assert!(header.filename.is_none());
        assert!(header.comment.is_none());
    }

    #[test]
    fn test_parse_header_with_filename() {
        let data = vec![
            0x1f, 0x8b, // magic
            0x08, // method
            0x08, // flags (FNAME)
            0x00, 0x00, 0x00, 0x00, // mtime
            0x00, // extra flags
            0x03, // OS (Unix)
            b't', b'e', b's', b't', b'.', b't', b'x', b't', 0x00, // filename
        ];

        let mut cursor = Cursor::new(data);
        let header = GzipHeader::parse(&mut cursor).unwrap();

        assert!(header.has_filename());
        assert_eq!(header.filename.as_deref(), Some("test.txt"));
    }

    #[test]
    fn test_invalid_magic() {
        let data = vec![0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff];
        let mut cursor = Cursor::new(data);
        let result = GzipHeader::parse(&mut cursor);
        assert!(matches!(result, Err(Error::InvalidGzipMagic(_))));
    }

    #[test]
    fn test_trailer() {
        let data = vec![
            0x12, 0x34, 0x56, 0x78, // CRC32
            0x00, 0x10, 0x00, 0x00, // ISIZE (4096)
        ];
        let mut cursor = Cursor::new(data);
        let trailer = GzipTrailer::parse(&mut cursor).unwrap();

        assert_eq!(trailer.crc32, 0x78563412);
        assert_eq!(trailer.isize, 4096);
    }
}
