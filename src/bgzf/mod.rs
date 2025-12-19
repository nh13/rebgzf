pub mod constants;
pub mod detector;
pub mod index;
pub mod writer;

pub use constants::*;
pub use detector::{is_bgzf, validate_bgzf_streaming, validate_bgzf_strict, BgzfValidation};
pub use index::{GziEntry, GziIndexBuilder};
pub use writer::BgzfBlockWriter;
