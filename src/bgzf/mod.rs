pub mod constants;
pub mod detector;
pub mod writer;

pub use constants::*;
pub use detector::{is_bgzf, validate_bgzf_strict, BgzfValidation};
pub use writer::BgzfBlockWriter;
