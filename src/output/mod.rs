use crate::util::{AppError, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::Write;
mod records;
mod search;
pub use crate::token_estimate::estimate_tokens;
pub use records::{annotate_summaries, output_stats, serialize_records, write_records};
pub use search::write_search_records;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputStats {
    pub chars: usize,
    pub bytes: usize,
    pub estimated_tokens: usize,
}

thread_local! {
    static CAPTURE: std::cell::RefCell<Option<Vec<Value>>> = const { std::cell::RefCell::new(None) };
}
pub(crate) fn is_capturing() -> bool {
    CAPTURE.with(|c| c.borrow().is_some())
}
pub(crate) fn capture(operation: impl FnOnce() -> Result<()>) -> Result<Vec<Value>> {
    if is_capturing() {
        return Err(AppError::new("nested output capture"));
    }
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            CAPTURE.with(|c| *c.borrow_mut() = None);
        }
    }
    CAPTURE.with(|c| *c.borrow_mut() = Some(Vec::new()));
    let _reset = Reset;
    operation()?;
    Ok(CAPTURE.with(|c| c.borrow_mut().take().unwrap()))
}
