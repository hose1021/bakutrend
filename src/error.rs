//! One classified error enum per boundary, so callers decide retry versus surface.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("feed parse failed: {0}")]
    Feed(String),
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("http {status} for {url}")]
    Http { status: u16, url: String },
    #[error("network error for {url}: {source}")]
    Network {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("parse error for {url}: {source}")]
    Parse {
        url: String,
        #[source]
        source: ParseError,
    },
    #[error("telegram preview for {handle} contained no posts")]
    EmptyPreview { handle: String },
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("read config {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse config {path}: {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("source `{name}` has unknown kind `{kind}`")]
    UnknownSourceKind { name: String, kind: String },
    /// A number that parsed but cannot mean anything. The message names the field and the
    /// range, because the value alone does not tell the reader what would have been accepted.
    #[error("config `{field}` is {value}, expected {expected}")]
    InvalidValue {
        field: String,
        value: String,
        expected: String,
    },
}
