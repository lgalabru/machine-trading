pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A component that didn't load: not a component, not of a world the
    /// host knows, or past the host's limits.
    #[error("wasm strategy {0}")]
    Wasm(String),
}
