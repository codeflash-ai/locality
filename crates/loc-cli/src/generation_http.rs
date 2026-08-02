//! Compatibility exports for authenticated generation HTTP adapters.
//!
//! The implementation is daemon-owned because recurring generation delivery
//! runs in `localityd`. These exact exports retain the original CLI library
//! surface for downstream callers.

pub use localityd::generation_http::*;
