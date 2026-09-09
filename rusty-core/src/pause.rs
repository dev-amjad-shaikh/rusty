//! Governed pause commit (EP-03-S11, executor side).
//!
//! A plain [`crate::node::NodeContext::interrupt`] suspends a run with an
//! opaque payload and nothing else — fine for in-process HITL, invisible to
//! longevity and expiry governance. A **governed** interrupt
//! ([`crate::node::NodeContext::interrupt_governed`]) carries typed
//! [`RunObligation`]s alongside the payload; at the suspension point the
//! executor commits them, plus a [`PauseEnvelope`], through the attached
//! [`PauseSink`] — so the pause queue, the expiry sweep, and cancellation
//! observe every governed pause as data instead of depending on a surface
//! remembering to register it.
//!
//! The wire shape is deliberately boring: the interrupt payload becomes
//! `{"__rusty_pause": {"payload": ..., "obligations": [...], ...}}`. Nodes
//! never build that object by hand — [`governed_interrupt`] does — and the
//! executor unwraps it before surfacing the outcome, so callers of
//! [`crate::executor::Executor::run`] see the clean payload either way.
//!
//! Expiry policy is stamped at creation, never retroactively: the executor
//! applies [`RunConfig::default_obligation_ttl`](crate::executor::RunConfig)
//! to obligations that declare no explicit `expires_at`, using the run's own
//! clock, before the sink sees them. A sink that stores what it receives
//! therefore honors "policy applies to obligations created after the change"
//! by construction.

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};

use crate::error::{Result, RustyError};
use crate::record::{ObligationStatus, PauseEnvelope, RunObligation, ToolIdentityKey};

/// The object key marking an interrupt payload as a governed pause. Only
/// [`governed_interrupt`] writes it; only the executor reads it.
pub const GOVERNED_PAUSE_KEY: &str = "__rusty_pause";

/// Build the interrupt payload for a governed pause: the surface `payload`
/// plus the `obligations` the pause registers, plus the tool identities the
/// envelope must rebind on resume. Pass the result to
/// [`crate::node::NodeContext::interrupt`] — or use
/// [`crate::node::NodeContext::interrupt_governed`], which composes the two.
pub fn governed_interrupt(
    payload: Value,
    obligations: Vec<RunObligation>,
    tool_identities: Vec<ToolIdentityKey>,
) -> Value {
    json!({
        GOVERNED_PAUSE_KEY: {
            "payload": payload,
            "obligations": obligations,
            "tool_identities": tool_identities,
        }
    })
}

/// A governed pause decoded from an interrupt payload.
pub(crate) struct GovernedPause {
    /// The surface payload (what the caller of `run` should see).
    pub payload: Value,
    /// The obligations the pause registers.
    pub obligations: Vec<RunObligation>,
    /// The tool identities the envelope records for resume-time rebinding.
    pub tool_identities: Vec<ToolIdentityKey>,
}

/// Decode a governed interrupt payload. Returns `Ok(None)` for ordinary
/// interrupts — the marker key is absent and the payload is not governance's
/// business. A payload that carries the marker but does not parse is a bug
/// in the pausing node, not an ordinary interrupt: it fails loudly rather
/// than degrading into an unregistered pause.
pub(crate) fn decode_governed(value: &Value) -> Result<Option<GovernedPause>> {
    let Some(governed) = value.get(GOVERNED_PAUSE_KEY) else {
        return Ok(None);
    };
    let parse = || -> std::result::Result<GovernedPause, String> {
        let payload = governed
            .get("payload")
            .cloned()
            .ok_or_else(|| "missing `payload`".to_string())?;
        let obligations = match governed.get("obligations") {
            None => Vec::new(),
            Some(raw) => serde_json::from_value::<Vec<RunObligation>>(raw.clone())
                .map_err(|e| format!("malformed `obligations`: {e}"))?,
        };
        let tool_identities = match governed.get("tool_identities") {
            None => Vec::new(),
            Some(raw) => serde_json::from_value::<Vec<ToolIdentityKey>>(raw.clone())
                .map_err(|e| format!("malformed `tool_identities`: {e}"))?,
        };
        Ok(GovernedPause {
            payload,
            obligations,
            tool_identities,
        })
    };
    parse()
        .map(Some)
        .map_err(|detail| RustyError::Checkpoint(format!("governed pause payload is malformed ({detail}); a pause that cannot register its obligations must fail, not degrade")))
}

/// Stamp `now + default_ttl` onto every obligation that declares no explicit
/// `expires_at`. Obligations with an explicit expiry keep it; a `None`
/// default leaves undeclared obligations open-ended. Callers pass the
/// deployment's default at pause-commit time, so a later policy change can
/// never rewrite rows already committed.
pub fn stamp_expiry_defaults(
    obligations: &mut [RunObligation],
    now: DateTime<Utc>,
    default_ttl: Option<Duration>,
) {
    let Some(ttl) = default_ttl else { return };
    for obligation in obligations.iter_mut() {
        if obligation.expires_at.is_none() {
            obligation.expires_at = Some(now + ttl);
        }
    }
}

/// One governed pause, committed: the envelope (schema version, run and
/// session ids, log position, checkpoint id, expiry-stamped obligations,
/// tool identities) plus the surface payload the interrupt carried. Sinks
/// persist both halves; the envelope's `created_at` is the run-clock reading
/// at the suspension point.
#[derive(Debug, Clone)]
pub struct PauseCommit {
    /// The versioned pause snapshot, obligations included.
    pub envelope: PauseEnvelope,
    /// The payload the pausing node surfaced to its caller.
    pub payload: Value,
}

/// The storage seam a governed pause commits through. The executor calls
/// [`PauseSink::commit_pause`] once per governed interrupt, after the
/// suspension checkpoint is durable and before the run outcome surfaces.
/// The server implements this over its run-obligations store; tests
/// implement it over a `Vec`.
///
/// Implementations must be idempotent per obligation id: a commit retried
/// after a crash between checkpoint write and commit writes the same rows
/// again, never duplicates.
#[async_trait::async_trait]
pub trait PauseSink: Send + Sync {
    /// Persist one governed pause. An error fails the run loudly — a pause
    /// whose obligations never registered would be a corpse the sweep can
    /// never observe, so the executor propagates rather than swallows it.
    async fn commit_pause(&self, commit: PauseCommit) -> Result<()>;
}

/// Prepare a decoded governed pause for commit: force every obligation
/// `Open` (a pausing node has no business registering settled rows), stamp
/// expiry defaults from the run clock, and assemble the envelope. Returns
/// the commit the sink receives.
pub(crate) fn prepare_commit(
    mut governed: GovernedPause,
    envelope: &mut PauseEnvelope,
    now: DateTime<Utc>,
    default_ttl: Option<Duration>,
) -> PauseCommit {
    for obligation in &mut governed.obligations {
        obligation.status = ObligationStatus::Open;
    }
    stamp_expiry_defaults(&mut governed.obligations, now, default_ttl);
    envelope.created_at = now;
    envelope.obligations = governed.obligations;
    envelope.tool_identities = governed.tool_identities;
    PauseCommit {
        envelope: envelope.clone(),
        payload: governed.payload,
    }
}
