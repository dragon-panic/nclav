use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io error reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("yaml parse error in {path}: {source}")]
    YamlParse {
        path: String,
        #[source]
        source: serde_yaml::Error,
    },

    #[error("conversion error in {path}: {message}")]
    Conversion { path: String, message: String },

    #[error("domain error: {0}")]
    Domain(#[from] nclav_domain::DomainError),
}
