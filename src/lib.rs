//! Rustis: GTFS feeds → immutable memory-mapped snapshot → RAPTOR with lower-bound pruning.
pub mod build;
mod catalog;
mod engine;
mod feed;
pub mod format;
pub mod http;
pub mod inventory;
pub mod memory;
mod model;
mod raptor;
pub mod store;
mod tt;

pub use build::{BuildOptions, FeedReport, FeedSpec, SnapshotMeta};
pub use engine::{Snapshot, build_snapshot, import_gtfs, import_gtfs_with};
pub use model::*;

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
