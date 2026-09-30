//! Token estimator shared between the binary and integration tests.
//!
//! This module is intentionally self-contained (std-only, no `crate::`
//! references) so integration tests can include the canonical implementation
//! directly via `#[path = "../src/token_estimate.rs"]` instead of keeping a
//! drift-prone copy.

/// Rough token estimate: ASCII word-ish chars pack ~4 per token, ASCII
/// structural chars ~2 per token, non-ASCII chars cost one token each.
pub fn estimate_tokens(text: &str) -> usize {
    let (wordish, structural, non_ascii) = token_components(text);
    wordish.div_ceil(4) + structural.div_ceil(2) + non_ascii
}

/// Unrounded counts allow cached JSONL fragments to be combined without
/// changing the estimate at fragment boundaries.
pub fn token_components(text: &str) -> (usize, usize, usize) {
    let mut wordish = 0usize;
    let mut structural = 0usize;
    let mut non_ascii = 0usize;
    for character in text.chars() {
        if !character.is_ascii() {
            non_ascii += 1;
        } else if character.is_ascii_alphanumeric() || character.is_ascii_whitespace() {
            wordish += 1;
        } else {
            structural += 1;
        }
    }
    (wordish, structural, non_ascii)
}
