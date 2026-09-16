use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Usage(String),
    #[error("unsafe output path {path}: {reason}")]
    UnsafePath { path: PathBuf, reason: String },
    #[error("another synchronization is already running for this destination")]
    Locked,
    #[error("I/O error while {context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid JSON from {context}")]
    Json { context: String },
    #[error("remote request failed: {0}")]
    Remote(String),
    #[error("remote resource was not found")]
    RemoteNotFound,
    #[error("GitHub rate limit is too low to continue safely (remaining: {0})")]
    RateLimited(u32),
    #[error("cache is inconsistent: {0}")]
    Inconsistent(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Error {
    let context = context.into();
    move |source| Error::Io { context, source }
}
