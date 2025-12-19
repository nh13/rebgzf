pub mod parser;
pub mod tables;
pub mod tokens;

pub use parser::DeflateParser;
pub use tokens::{LZ77Block, LZ77Token};
