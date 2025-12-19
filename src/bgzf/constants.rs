/// Maximum uncompressed block size for BGZF
pub const BGZF_MAX_BLOCK_SIZE: usize = 65536; // 64KB

/// Default/recommended uncompressed block size
pub const BGZF_BLOCK_SIZE: usize = 65280;

/// BGZF header size (gzip header with extra field)
pub const BGZF_HEADER_SIZE: usize = 18;

/// BGZF footer size (CRC32 + ISIZE)
pub const BGZF_FOOTER_SIZE: usize = 8;

/// Maximum total BGZF block size
pub const MAX_BGZF_BLOCK_SIZE: usize = 65536;

/// BGZF EOF block (28 bytes)
pub const BGZF_EOF: [u8; 28] = [
    0x1f, 0x8b, 0x08, 0x04, // gzip magic, method, flags (FEXTRA)
    0x00, 0x00, 0x00, 0x00, // mtime
    0x00, 0xff, // xfl, os
    0x06, 0x00, // xlen = 6
    0x42, 0x43, // subfield ID "BC"
    0x02, 0x00, // subfield length = 2
    0x1b, 0x00, // BSIZE = 27 (28 - 1)
    0x03, 0x00, // empty deflate block
    0x00, 0x00, 0x00, 0x00, // CRC32 = 0
    0x00, 0x00, 0x00, 0x00, // ISIZE = 0
];
