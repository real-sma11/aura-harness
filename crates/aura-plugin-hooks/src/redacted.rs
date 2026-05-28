//! Redaction wrapper for sensitive hook context fields.
//!
//! ## Invariants ([rules.md §13])
//!
//! - [`Redacted<T>::fmt`] / `impl Debug` MUST emit the literal
//!   `"<REDACTED>"` for the wrapped value. Accidental
//!   `tracing::debug!(?ctx)` of a [`Redacted`] field never spills the
//!   underlying secret into structured logs.
//! - [`Redacted::into_inner`] consumes the wrapper and returns `T`.
//!   This is the explicit escape hatch for handlers that legitimately
//!   need the value (e.g. a `PreToolUse` hook that inspects redacted
//!   tool args). The opt-in shape forces a deliberate `.into_inner()`
//!   call rather than letting the value leak through `Display` or
//!   `Debug`.
//! - [`Redacted`] is `Clone` when `T: Clone` so consumers can keep
//!   their own copy of the wrapped value if needed; we do not derive
//!   `Copy` because the most common wrapped type is `String`.

use std::fmt;

/// Wrapper that redacts a sensitive value when formatted via
/// [`fmt::Debug`]. Use [`Redacted::into_inner`] (or [`Redacted::peek`])
/// to read the wrapped value when a handler explicitly needs it.
#[derive(Clone)]
pub struct Redacted<T> {
    inner: T,
}

impl<T> Redacted<T> {
    /// Wrap a value. The wrapper redacts `Debug` output.
    #[must_use]
    pub const fn new(inner: T) -> Self {
        Self { inner }
    }

    /// Consume the wrapper and return the wrapped value.
    pub fn into_inner(self) -> T {
        self.inner
    }

    /// Borrow the inner value without consuming the wrapper. Use
    /// sparingly — leaking the borrow into structured logs defeats
    /// the redaction guarantee.
    pub const fn peek(&self) -> &T {
        &self.inner
    }
}

impl<T> fmt::Debug for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<REDACTED>")
    }
}

impl<T: PartialEq> PartialEq for Redacted<T> {
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

impl<T: Eq> Eq for Redacted<T> {}

impl<T> From<T> for Redacted<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_is_redacted() {
        let secret = Redacted::new("sk-ant-very-secret".to_string());
        let dbg = format!("{secret:?}");
        assert!(!dbg.contains("sk-ant"), "got {dbg}");
        assert_eq!(dbg, "<REDACTED>");
    }

    #[test]
    fn into_inner_yields_value() {
        let r = Redacted::new(42_u32);
        assert_eq!(r.into_inner(), 42);
    }

    #[test]
    fn peek_does_not_consume() {
        let r = Redacted::new("hi".to_string());
        assert_eq!(r.peek(), "hi");
        assert_eq!(r.into_inner(), "hi".to_string());
    }

    #[test]
    fn nested_struct_with_redacted_field_redacts_in_debug() {
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Wrapper {
            visible: u32,
            hidden: Redacted<String>,
        }
        let w = Wrapper {
            visible: 7,
            hidden: Redacted::new("password".into()),
        };
        let dbg = format!("{w:?}");
        assert!(!dbg.contains("password"));
        assert!(dbg.contains("<REDACTED>"));
        assert!(dbg.contains('7'));
    }
}
