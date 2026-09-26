//! Error types for Rusty Eval.
//!
//! All fallible operations in the crate return [`Result<T>`] with
//! [`EvalError`] as the error type. A failed *run* is not an [`EvalError`] —
//! it is evidence, recorded as [`crate::evidence::RunStatus::Failed`] on the
//! case run. Errors here mean the experiment itself is broken: malformed
//! datasets, unsupported format versions, agent factories that cannot build,
//! judges that cannot answer.

use thiserror::Error;

/// The single error type for the whole crate.
#[derive(Debug, Error)]
pub enum EvalError {
    /// Malformed dataset content: bad JSONL, missing header, duplicate case
    /// ids, expectations that cannot convert into assertions. The message
    /// carries the line number when the failure is line-local.
    #[error("dataset error: {0}")]
    Dataset(String),

    /// The dataset's declared `format_version` is newer (or older) than this
    /// build supports. Loading refuses to guess across versions.
    #[error("unsupported dataset format version: found {found}, this build supports {supported}")]
    UnsupportedVersion {
        /// The version the dataset declares.
        found: u64,
        /// The version this build loads.
        supported: u64,
    },

    /// Malformed or invalid human-feedback queue state.
    #[error("feedback error: {0}")]
    Feedback(String),

    /// The feedback queue format is not supported by this build.
    #[error("unsupported feedback format version: found {found}, this build supports {supported}")]
    UnsupportedFeedbackVersion {
        /// The version the queue declares.
        found: u64,
        /// The version this build loads.
        supported: u64,
    },

    /// Malformed release-gate policy or decision evidence.
    #[error("gate error: {0}")]
    Gate(String),

    /// A gate policy or decision uses an unsupported format version.
    #[error(
        "unsupported gate {artifact} format version: found {found}, this build supports {supported}"
    )]
    UnsupportedGateVersion {
        /// Policy or decision.
        artifact: &'static str,
        /// Version declared by the artifact.
        found: u64,
        /// Version supported by this build.
        supported: u64,
    },

    /// The agent factory failed to build a graph for a case, or the initial
    /// state derived from the case input is not a JSON object.
    #[error("agent build error: {0}")]
    AgentBuild(String),

    /// A judge failed to produce a verdict. Judge failures abort the
    /// experiment rather than fabricating a score: a missing verdict is
    /// infrastructure failure, not evidence about the agent.
    #[error("judge error: {0}")]
    Judge(String),

    /// Invalid configuration or input validation failure.
    #[error("validation error: {0}")]
    Validation(String),

    /// Invalid configuration or incomparable evidence for a statistical test.
    #[error("statistics error: {0}")]
    Statistics(String),

    /// Invalid source evidence or a forged failure-cluster artifact.
    #[error("failure clustering error: {0}")]
    Clustering(String),

    /// A run-level failure from the core runtime that escaped the evidence
    /// path (factory-side graph compilation, journal setup).
    #[error("runtime error: {0}")]
    Runtime(#[from] rusty_agent_runtime::error::RustyError),

    /// Filesystem failures loading or saving datasets and reports.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON (de)serialization failures outside dataset parsing (report
    /// persistence).
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, EvalError>;
