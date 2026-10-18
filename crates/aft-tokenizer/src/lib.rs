//! Claude tokenizer matching `ai-tokenizer`'s lookup/BPE hybrid encoding.
//!
//! [`count_tokens`] and [`encode`] return errors when pre-tokenization fails;
//! neither substitutes an estimate or returns a partial tokenization.

mod claude;

pub use claude::{count_tokens, encode};
pub use fancy_regex::Error as TokenizationError;
