use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    // I/O errors
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    // Gzip header errors
    #[error("Invalid gzip magic bytes: expected 0x1f8b, got 0x{0:04x}")]
    InvalidGzipMagic(u16),

    #[error("Unsupported compression method: {0} (only DEFLATE/8 supported)")]
    UnsupportedCompressionMethod(u8),

    #[error("Gzip header CRC mismatch: expected 0x{expected:04x}, got 0x{found:04x}")]
    GzipHeaderCrcMismatch { expected: u16, found: u16 },

    // DEFLATE parsing errors
    #[error("Invalid DEFLATE block type: {0}")]
    InvalidBlockType(u8),

    #[error("Invalid Huffman code length: {0} (max 15)")]
    InvalidCodeLength(u8),

    #[error("Huffman code oversubscribed: more codes than possible for bit length")]
    HuffmanOversubscribed,

    #[error("Huffman code incomplete: not all codes assigned")]
    HuffmanIncomplete,

    #[error("Invalid Huffman symbol: {0}")]
    InvalidHuffmanSymbol(u16),

    #[error("Invalid length code: {0}")]
    InvalidLengthCode(u16),

    #[error("Invalid distance code: {0}")]
    InvalidDistanceCode(u16),

    #[error("Back-reference distance {distance} exceeds available window {available}")]
    InvalidBackReference { distance: u16, available: usize },

    #[error("Stored block length mismatch: LEN={len}, NLEN={nlen}")]
    StoredBlockLengthMismatch { len: u16, nlen: u16 },

    // BGZF errors
    #[error("BGZF block too large: {size} bytes exceeds maximum {max}")]
    BgzfBlockTooLarge { size: usize, max: usize },

    #[error("Compressed data exceeds BGZF block limit")]
    CompressedDataTooLarge,

    // Checksum errors
    #[error("CRC32 mismatch: expected 0x{expected:08x}, got 0x{found:08x}")]
    Crc32Mismatch { expected: u32, found: u32 },

    #[error("Size mismatch: expected {expected} bytes, got {found}")]
    SizeMismatch { expected: u32, found: u32 },

    // Internal errors
    #[error("Unexpected end of input")]
    UnexpectedEof,

    #[error("Internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, Error>;
