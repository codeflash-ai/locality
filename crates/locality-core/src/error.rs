//! Core error types.
//!
//! User-facing CLI formatting should happen above this crate. Core errors carry
//! structured categories that map cleanly to stable CLI exit codes and JSON.

use std::fmt::{Display, Formatter};
use std::time::Duration;

use crate::conflict::ConflictSummary;
use crate::validation::ValidationIssue;

pub type LocalityResult<T> = Result<T, LocalityError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalityError {
    Validation(Vec<ValidationIssue>),
    Conflict(ConflictSummary),
    Guardrail(String),
    RemoteNotFound(String),
    RateLimited {
        provider: String,
        retry_after: Duration,
        message: String,
    },
    InvalidState(String),
    Unsupported(&'static str),
    NotImplemented(&'static str),
    Io(String),
}

impl Display for LocalityError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(issues) => write!(f, "{} validation issue(s)", issues.len()),
            Self::Conflict(summary) => write!(f, "conflict on {}", summary.path.display()),
            Self::Guardrail(message) => write!(f, "guardrail blocked push: {message}"),
            Self::RemoteNotFound(message) => write!(f, "remote object not found: {message}"),
            Self::RateLimited {
                provider,
                retry_after,
                message,
            } => write!(
                f,
                "{provider} rate limited for {}ms: {message}",
                retry_after.as_millis()
            ),
            Self::InvalidState(message) => write!(f, "invalid state: {message}"),
            Self::Unsupported(feature) => write!(f, "unsupported feature: {feature}"),
            Self::NotImplemented(feature) => write!(f, "not implemented: {feature}"),
            Self::Io(message) => write!(f, "io error: {message}"),
        }
    }
}

impl std::error::Error for LocalityError {}

impl From<std::io::Error> for LocalityError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}
