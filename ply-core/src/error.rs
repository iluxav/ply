use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("manifest error: {0}")]
    Manifest(String),

    #[error("{0}")]
    ImageName(String),

    #[error("build error: {0}")]
    Build(String),

    #[error("store error: {0}")]
    Store(String),

    #[error("source error: {0}")]
    Source(String),

    #[error("resolve error: {0}")]
    Resolve(String),

    #[error("runtime error: {0}")]
    Runtime(String),

    #[error("not implemented yet: {0}")]
    Unimplemented(&'static str),
}
