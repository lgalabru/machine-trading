use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

/// Errors reading the files and the environment `mtx-core`'s formats live
/// in: a mandate, the turn socket.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("environment variable {name}: {reason}")]
    Env { name: &'static str, reason: String },
    #[error("mandate {}: {reason}", path.display())]
    Mandate { path: PathBuf, reason: String },
}

pub(crate) fn io_error(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Error {
    let path = path.into();
    move |source| Error::Io { path, source }
}
