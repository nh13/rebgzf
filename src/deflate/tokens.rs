/// Represents a single token in the LZ77 stream
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LZ77Token {
    /// A literal byte
    Literal(u8),
    /// A back-reference: copy `length` bytes from `distance` bytes back
    Copy { length: u16, distance: u16 },
    /// End of block marker
    EndOfBlock,
}

impl LZ77Token {
    /// Returns the uncompressed size this token represents
    pub fn uncompressed_size(&self) -> usize {
        match self {
            LZ77Token::Literal(_) => 1,
            LZ77Token::Copy { length, .. } => *length as usize,
            LZ77Token::EndOfBlock => 0,
        }
    }
}

/// Code length information for dynamic blocks (needed for re-encoding)
#[derive(Clone, Debug)]
pub struct CodeLengths {
    pub literal_lengths: Vec<u8>,  // Up to 286 symbols
    pub distance_lengths: Vec<u8>, // Up to 30 symbols
}

/// A block of LZ77 tokens with metadata
#[derive(Clone, Debug)]
pub struct LZ77Block {
    /// The tokens in this block
    pub tokens: Vec<LZ77Token>,
    /// Whether this is the final block in the deflate stream
    pub is_final: bool,
    /// Original block type (0=stored, 1=fixed, 2=dynamic)
    pub block_type: u8,
    /// For dynamic blocks: the code length sequences for reconstruction
    pub code_lengths: Option<CodeLengths>,
}

impl LZ77Block {
    pub fn new(tokens: Vec<LZ77Token>, is_final: bool, block_type: u8) -> Self {
        Self { tokens, is_final, block_type, code_lengths: None }
    }

    /// Total uncompressed size of this block
    pub fn uncompressed_size(&self) -> usize {
        self.tokens.iter().map(|t| t.uncompressed_size()).sum()
    }
}
