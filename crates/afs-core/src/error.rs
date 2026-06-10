//! Core error types.
//!
//! User-facing CLI formatting should happen above this crate. Core errors carry
//! structured categories that map cleanly to stable CLI exit codes and JSON.

use std::fmt::{Display, Formatter};

use crate::conflict::ConflictSummary;
use crate::validation::ValidationIssue;

pub type AfsResult<T> = Result<T, AfsError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AfsError {
    Validation(Vec<ValidationIssue>),
    Conflict(ConflictSummary),
    Guardrail(String),
    InvalidState(String),
    Unsupported(&'static str),
    NotImplemented(&'static str),
    Io(String),
}

impl Display for AfsError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(issues) => write!(f, "{} validation issue(s)", issues.len()),
            Self::Conflict(summary) => write!(f, "conflict on {}", summary.path.display()),
            Self::Guardrail(message) => write!(f, "guardrail blocked push: {message}"),
            Self::InvalidState(message) => write!(f, "invalid state: {message}"),
            Self::Unsupported(feature) => write!(f, "unsupported feature: {feature}"),
            Self::NotImplemented(feature) => write!(f, "not implemented: {feature}"),
            Self::Io(message) => write!(f, "io error: {message}"),
        }
    }
}

impl std::error::Error for AfsError {}

impl From<std::io::Error> for AfsError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}
