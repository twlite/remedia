//! Fixture application crate.

use fx_base::{Counter, Source};

/// Wraps a [`Counter`] from another crate.
pub struct Wrapper {
    /// The wrapped counter.
    pub inner: Counter,
}

impl Source for Wrapper {
    type Item = u64;

    fn next_item(&mut self) -> Option<Self::Item> {
        self.inner.next_item()
    }
}

/// Generic function with bounds and an associated-type return.
pub fn drain<S: Source>(source: &mut S) -> Vec<<S as Source>::Item> {
    let mut out = Vec::new();

    while let Some(item) = source.next_item() {
        out.push(item);
    }

    out
}
