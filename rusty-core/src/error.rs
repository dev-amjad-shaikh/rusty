//! Error types for Rusty Core.
//!
//! All fallible operations in the crate return [`Result<T>`] with
//! [`RustyError`] as the error type.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Why an LLM provider call failed, in retry-relevant terms.
///
/// `RustyError::Llm(String)` erases the difference between a rate limit and a
/// fatal auth failure; the provider clients already classify every failed
/// attempt for their own retry policy, and this enum is that classification
/// crossing the trait boundary instead of being thrown away. The durable-work
/// taxonomy consumes it via `From<LlmErrorClass> for
/// [`crate::durable::ErrorClass`], so an LLM failure inside a task retries
/// with the right policy rather than the stringly-typed default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmErrorClass {
    /// The provider asked to be slowed down (HTTP 429, `Retry-After`).
    RateLimited,
    /// The call never reached a response: connect failure or timeout.
    Timeout,
    /// The provider failed server-side (HTTP 5xx, 408).
    Server,
    /// The credentials were rejected (HTTP 401/403). Retrying the same
    /// request cannot help; the key, not the call, is wrong.
    Auth,
    /// The request itself was refused (other HTTP 4xx). Reissuing the same
    /// bytes fails the same way.
    InvalidRequest,
    /// The response could not be decoded into the expected shape.
    Decode,
    /// No finer classification applies (including every legacy
    /// [`RustyError::Llm`] raised by user implementations).
    Unknown,
}

impl std::fmt::Display for LlmErrorClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            LlmErrorClass::RateLimited => "rate_limited",
            LlmErrorClass::Timeout => "timeout",
            LlmErrorClass::Server => "server",
            LlmErrorClass::Auth => "auth",
            LlmErrorClass::InvalidRequest => "invalid_request",
            LlmErrorClass::Decode => "decode",
            LlmErrorClass::Unknown => "unknown",
        };
        f.write_str(s)
    }
}

/// The single error type for the whole crate.
///
/// Design note: `Interrupt` is modeled as an *error* because a node calls
/// `interrupt()` mid-execution to unwind out of the current super-step; the
/// executor catches this variant, persists a checkpoint, and surfaces the
/// payload to the caller. This mirrors LangGraph's `GraphInterrupt` and keeps
/// the control flow explicit in the type system.
#[derive(Debug, Error)]
pub enum RustyError {
    /// Structural graph problems: invalid builder usage, validation failures
    /// from `GraphBuilder::compile()` (unknown entry point, dangling edges), routing
    /// to unknown nodes at runtime, exceeded `max_steps`, etc.
    #[error("graph error: {0}")]
    Graph(String),

    /// A node failed during execution. The string should include the node
    /// name and the underlying failure description.
    #[error("node error: {0}")]
    Node(String),

    /// A node invoked `interrupt(payload)`. Carries the payload to surface
    /// to the caller; resumable via `RunConfig::resume` / `Command::resume`.
    ///
    /// This variant is **not** a failure — it is the suspend signal of the
    /// interrupt/resume protocol.
    #[error("graph interrupted")]
    Interrupt {
        /// The payload passed to `interrupt()`, surfaced to the caller
        /// (e.g. a human-in-the-loop approval request).
        value: Value,
    },

    /// Checkpoint persistence failures (IO, serialization, not found).
    #[error("checkpoint error: {0}")]
    Checkpoint(String),

    /// LLM provider failures (HTTP errors, malformed responses, auth).
    #[error("llm error: {0}")]
    Llm(String),

    /// LLM provider failure with a retry-relevant [`LlmErrorClass`].
    ///
    /// Produced by the built-in provider clients, whose retry classifiers
    /// already know *why* an attempt failed; [`RustyError::Llm`] stays for
    /// user implementations and classifies as [`LlmErrorClass::Unknown`].
    #[error("llm error ({class}): {message}")]
    LlmFailure {
        /// The retry-relevant classification.
        class: LlmErrorClass,
        /// Human-readable detail (status, truncated body, decode error).
        message: String,
    },

    /// Tool execution failures (unknown tool, bad arguments, runtime error).
    #[error("tool error: {0}")]
    Tool(String),

    /// Plugin kernel failures ([`crate::plugin`]): a duplicate identity, an
    /// `apply` that failed or panicked midway (its partial registrations
    /// already unwound), an unload of a plugin that is not active, or a
    /// hot reload whose old registrations survived unloading.
    #[error("plugin error: {0}")]
    Plugin(String),

    /// Doctor contract failures: missing migration chain, repair failure,
    /// halted upgrade.
    #[error("doctor error: {0}")]
    Doctor(String),

    /// Exact-replay failures (Flight Recorder, R0.5): the run diverged from
    /// the journaled evidence (request-hash mismatch, effect-order violation,
    /// unserved recorded effects), a journal snapshot or fixture failed
    /// integrity verification, or a replay was requested against an
    /// incompatible graph, fixture, or resumed-run journal.
    #[error("replay error: {0}")]
    Replay(String),

    /// JSON (de)serialization failures.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// A channel received an update it cannot accept — most commonly a
    /// `LastValue`-style channel written more than once in a single
    /// super-step (the LangGraph `InvalidUpdateError` class of bug), or a
    /// write to an undeclared channel.
    #[error("invalid state update: {0}")]
    InvalidUpdate(String),

    /// The run was cancelled cooperatively through
    /// [`crate::executor::RunConfig::cancellation`] (R0.6 wave 2c, drain):
    /// the executor stopped *at a super-step boundary*, so the boundary
    /// checkpoint is intact and the run resumes from exactly there.
    ///
    /// Like [`RustyError::Interrupt`], this variant is **not** a failure —
    /// it is control flow (the `cancelled` class of the Durable Work
    /// taxonomy: never retried, never dead-lettered). Unlike an interrupt,
    /// nothing is being asked of a human; whoever cancelled decides whether
    /// and when to re-drive the run from its last checkpoint.
    #[error("run cancelled: {0}")]
    Cancelled(String),
}

impl RustyError {
    /// Returns `true` if this error is an interrupt (suspend) signal rather
    /// than an actual failure.
    pub fn is_interrupt(&self) -> bool {
        matches!(self, RustyError::Interrupt { .. })
    }

    /// If this is an [`RustyError::Interrupt`], returns a reference to
    /// the interrupt payload.
    pub fn interrupt_value(&self) -> Option<&Value> {
        match self {
            RustyError::Interrupt { value } => Some(value),
            _ => None,
        }
    }

    /// The LLM failure classification: the carried class for
    /// [`RustyError::LlmFailure`], [`LlmErrorClass::Unknown`] for the legacy
    /// string-payload [`RustyError::Llm`] and for non-LLM errors.
    pub fn llm_class(&self) -> LlmErrorClass {
        match self {
            RustyError::LlmFailure { class, .. } => *class,
            _ => LlmErrorClass::Unknown,
        }
    }
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, RustyError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llm_error_class_serde_names_are_snake_case() {
        for (class, name) in [
            (LlmErrorClass::RateLimited, "rate_limited"),
            (LlmErrorClass::Timeout, "timeout"),
            (LlmErrorClass::Server, "server"),
            (LlmErrorClass::Auth, "auth"),
            (LlmErrorClass::InvalidRequest, "invalid_request"),
            (LlmErrorClass::Decode, "decode"),
            (LlmErrorClass::Unknown, "unknown"),
        ] {
            let value = serde_json::to_value(class).unwrap();
            assert_eq!(value, serde_json::json!(name));
            assert_eq!(format!("{class}"), name, "Display matches the serde name");
            let back: LlmErrorClass = serde_json::from_value(value).unwrap();
            assert_eq!(back, class);
        }
    }

    #[test]
    fn llm_class_surfaces_the_carried_class() {
        let err = RustyError::LlmFailure {
            class: LlmErrorClass::RateLimited,
            message: "chat completions returned 429: slow down".into(),
        };
        assert_eq!(err.llm_class(), LlmErrorClass::RateLimited);
        assert_eq!(
            err.to_string(),
            "llm error (rate_limited): chat completions returned 429: slow down"
        );
    }

    #[test]
    fn legacy_llm_error_classifies_as_unknown() {
        // User implementations raise the string-payload variant; the
        // classifying helpers must not guess at its contents.
        assert_eq!(
            RustyError::Llm("anything".into()).llm_class(),
            LlmErrorClass::Unknown
        );
        // Non-LLM errors classify as Unknown too.
        assert_eq!(
            RustyError::Tool("nope".into()).llm_class(),
            LlmErrorClass::Unknown
        );
    }
}
