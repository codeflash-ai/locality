//! Compatibility exports for authenticated generation HTTP adapters.
//!
//! The implementation is daemon-owned so a future recurring generation
//! delivery job can use it without putting transport logic in the CLI. No
//! daemon scheduler currently invokes it. These exact exports retain the
//! original CLI library surface for downstream callers.

pub use localityd::generation_http::*;
