//! Rustis: GTFS → immutable disk snapshot → Vulture RAPTOR routing.
pub mod format;
pub mod http;
mod import;
pub mod memory;
mod model;
mod routing;
mod storage;

pub use import::{ImportOptions, ImportReport, import_gtfs, import_gtfs_with};
pub use model::*;
pub use storage::Snapshot;

/// Errors from serving APIs: client mistakes vs. internal failures.
#[derive(Debug)]
pub enum Error {
    Invalid(String),
    Internal(anyhow::Error),
}

impl Error {
    pub(crate) fn invalid(msg: impl Into<String>) -> Self {
        Self::Invalid(msg.into())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) => write!(f, "invalid request: {m}"),
            Self::Internal(e) => write!(f, "internal error: {e:#}"),
        }
    }
}

impl std::error::Error for Error {}
