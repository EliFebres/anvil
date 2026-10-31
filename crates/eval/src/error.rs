//! The error type of the eval harness.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Why an eval operation failed.
#[derive(Debug, Error)]
pub enum EvalError {
    /// A task folder is missing a part or a part is malformed.
    #[error("task {dir}: {reason}")]
    InvalidTask {
        /// The task folder.
        dir: PathBuf,
        /// What is wrong with it.
        reason: String,
    },

    /// No task under the tasks folder matched the tag filter.
    #[error("no tasks under {dir} match tags [{tags}]")]
    NoTasks {
        /// The tasks folder.
        dir: PathBuf,
        /// The filter, comma separated; empty means no filter.
        tags: String,
    },

    /// A filesystem operation failed.
    #[error("{action} {path}: {source}")]
    Io {
        /// What was attempted, such as `reading`.
        action: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },

    /// A TOML file does not parse.
    #[error("parsing {path}: {source}")]
    Toml {
        /// The file.
        path: PathBuf,
        /// The underlying error, boxed to keep the enum small.
        #[source]
        source: Box<toml::de::Error>,
    },

    /// A JSON file does not parse.
    #[error("parsing {path}: {source}")]
    Json {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: serde_json::Error,
    },

    /// A value could not be serialized.
    #[error("serializing {what}: {source}")]
    Serialize {
        /// What was being written.
        what: &'static str,
        /// The underlying error.
        #[source]
        source: serde_json::Error,
    },

    /// A result file was written by a different schema version.
    #[error("{path} has result schema {found}; this build reads {expected}")]
    Schema {
        /// The file.
        path: PathBuf,
        /// The version in the file.
        found: u32,
        /// The version this build reads.
        expected: u32,
    },

    /// A program could not be started.
    #[error("spawning {program}: {source}")]
    Spawn {
        /// The program.
        program: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },

    /// No bash to run `check.sh` with.
    #[error("bash not found on PATH or under Git for Windows; check.sh needs it")]
    BashNotFound,

    /// No Python interpreter answered.
    #[error("no working python3 or python on PATH; the Python tasks need one")]
    PythonNotFound,

    /// `--provider` names a provider the anvil configuration lacks.
    #[error("provider `{name}` is not in the anvil configuration")]
    UnknownProvider {
        /// The name as given.
        name: String,
    },

    /// No anvil binary to drive.
    #[error(
        "anvil not found: pass --anvil-bin, put anvil on PATH or build it with `cargo build -p anvil-cli`"
    )]
    AnvilNotFound,

    /// A git command failed.
    #[error("git {args}: {stderr}")]
    Git {
        /// The arguments, space separated.
        args: String,
        /// What git printed.
        stderr: String,
    },

    /// The anvil configuration could not be loaded.
    #[error("anvil configuration: {0}")]
    Config(#[source] Box<anvil_types::ConfigError>),

    /// `factory.toml` failed validation.
    #[error("factory.toml: {0}")]
    FactoryConfig(#[from] anvil_types::ValidationError),

    /// A numeric option is zero.
    #[error("{option} must be at least 1")]
    ZeroOption {
        /// The option name.
        option: &'static str,
    },
}

impl From<anvil_types::ConfigError> for EvalError {
    fn from(source: anvil_types::ConfigError) -> Self {
        Self::Config(Box::new(source))
    }
}

impl EvalError {
    /// An [`EvalError::Io`] for `path`.
    ///
    /// # Arguments
    ///
    /// * `action` - What was attempted, such as `reading`.
    /// * `path` - The path involved.
    /// * `source` - The underlying error.
    ///
    /// # Returns
    ///
    /// The error.
    pub fn io(action: &'static str, path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            action,
            path: path.to_owned(),
            source,
        }
    }

    /// An [`EvalError::Toml`] for `path`.
    ///
    /// # Arguments
    ///
    /// * `path` - The file that failed to parse.
    /// * `source` - The underlying error.
    ///
    /// # Returns
    ///
    /// The error.
    pub fn toml(path: &Path, source: toml::de::Error) -> Self {
        Self::Toml {
            path: path.to_owned(),
            source: Box::new(source),
        }
    }
}
