//! The gateway's schema-defined wire protocol (`contracts:gateway-protocol`,
//! EP-04-S01): the Rust types below are the single source of truth that
//! generates both the server-side JSON Schema validators and the TypeScript
//! client types, so a protocol change without a schema change cannot pass
//! the drift gate.
//!
//! The shapes below are the contract's verbatim: field names, serde tagging,
//! and optionality are normative. The one named-but-unspecified type is
//! [`ResponseOutcome`] — the contract fixes its flattening into
//! `Frame::Response` but not its internals; see its docs.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One wire frame: typed request/response/event with a mandatory
/// idempotency key on side-effecting methods, snapshot-on-connect, and
/// monotonic event sequencing (`contracts:gateway-protocol`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum Frame {
    /// A client call. `method` names the protocol method; `params` is its
    /// argument object.
    Request {
        /// The caller-chosen correlation id its `Response` echoes.
        id: u64,
        /// The protocol method name.
        method: String,
        /// The method's arguments.
        params: serde_json::Value,
        /// Mandatory on side-effecting methods; checked against a dedupe
        /// cache so a retried webhook cannot double-execute a turn.
        #[serde(skip_serializing_if = "Option::is_none")]
        idempotency_key: Option<String>,
    },
    /// The answer to a `Request`, by correlation id.
    Response {
        /// The request this answers.
        id: u64,
        /// The flattened outcome.
        #[serde(flatten)]
        outcome: ResponseOutcome,
    },
    /// A server-pushed event.
    Event {
        /// Monotonic per connection; a client detecting a gap re-snapshots.
        seq: u64,
        /// The event name (run-boundary and connection-lifetime events are
        /// named here, never inferred from side effects).
        name: String,
        /// The event payload.
        payload: serde_json::Value,
    },
}

/// The flattened payload of `Frame::Response`.
///
/// The contract names this type and fixes its `#[serde(flatten)]` placement
/// but leaves its internals unspecified; this is the minimal honest shape —
/// an ok result or an error, both arbitrary JSON until a contract revision
/// pins them. Kept deliberately thin so the pinned revision lands as a
/// compatible refinement, not a re-tagging.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResponseOutcome {
    /// The call succeeded; `result` is the method's return value.
    Ok {
        /// The method's return value.
        result: serde_json::Value,
    },
    /// The call failed; `error` carries the typed refusal.
    Err {
        /// The refusal payload.
        error: serde_json::Value,
    },
}

/// Device pairing: hello → challenge → signed response → paired
/// (`contracts:gateway-protocol`). Loopback devices may auto-approve;
/// remote devices always require operator approval. Tokens never travel in
/// snapshots.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum Pairing {
    /// The device introduces itself.
    Hello {
        /// The device's stable id.
        device_id: String,
        /// The device's public key (challenge answer signing).
        public_key: String,
        /// The device platform family.
        platform: String,
    },
    /// The gateway's challenge.
    Challenge {
        /// The nonce to sign.
        nonce: String,
    },
    /// The device's answer. The signature binds nonce, platform, and
    /// device family.
    Answer {
        /// The answering device's id.
        device_id: String,
        /// The signature over the challenge.
        signature: String,
    },
    /// Pairing completed; the token is delivered once, here, and never
    /// appears in snapshots.
    Paired {
        /// The device's bearer token.
        device_token: String,
    },
    /// Pairing refused.
    Denied {
        /// Why.
        reason: String,
    },
}

/// The exclusive per resolved-session lock serializing
/// load-history → run → flush (`contracts:gateway-protocol`). The lock key
/// is the resolved session ID, never the routing key.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TurnLease {
    /// The locked session.
    pub session_id: Uuid,
    /// The run holding the lease.
    pub holder_run_id: Uuid,
    /// When the lease was acquired.
    pub acquired_at: DateTime<Utc>,
    /// When the lease lapses.
    pub expires_at: DateTime<Utc>,
}

/// Steering verbs (`contracts:gateway-protocol`). Drain semantics: `Steer`
/// is drained at tool-launch boundaries only — before each sequential
/// launch and once before a parallel batch's atomic launch; a running tool
/// is never cancelled; skipped unstarted calls receive paired synthetic
/// tool results. `Inject` waits silently until a waking message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SteeringVerb {
    /// Queue behind the current turn.
    Followup,
    /// Redirect at the next drain boundary.
    Steer,
    /// Stage until a waking message.
    Inject,
}

/// The closed run-admission verdict (`contracts:gateway-protocol`):
/// decided at the exact branch that blocks or admits the run and carried
/// verbatim to clients — never reverse-engineered from errors.
/// Deliberately enumeration-safe about private agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionReason {
    /// Admitted to the session queue.
    Queued,
    /// Coalesced into an existing queued turn.
    Coalesced,
    /// Deferred by policy.
    Deferred,
    /// The runtime is offline.
    RuntimeOffline,
    /// The runtime is present but unusable.
    RuntimeUnusable,
    /// The request needs an agent runtime it cannot reach.
    AgentRuntimeRequired,
    /// Attribution requirements blocked the run.
    AttributionBlocked,
    /// The session already has an active run.
    AlreadyActive,
    /// The run would trigger itself.
    SelfTriggerSuppressed,
    /// Another run holds the turn lease.
    LeaseHeld,
    /// The caller lacks the required scope or credential.
    Unauthorized,
}

/// The protocol types the generation pipeline emits, in emission order.
/// The schema bundle and the TypeScript artifact both iterate this list;
/// adding a protocol type means adding it here, so an un-registered type
/// cannot silently escape the drift gate.
pub const PROTOCOL_TYPES: &[&str] = &[
    "Frame",
    "ResponseOutcome",
    "Pairing",
    "TurnLease",
    "SteeringVerb",
    "AdmissionReason",
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_frames_carry_the_contract_shape() {
        let frame = Frame::Request {
            id: 7,
            method: "submit_turn".to_owned(),
            params: json!({"text": "hi"}),
            idempotency_key: None,
        };
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(
            wire,
            json!({"frame": "request", "id": 7, "method": "submit_turn", "params": {"text": "hi"}}),
            "the key is absent, not null, when unsent"
        );
        let keyed = Frame::Request {
            id: 8,
            method: "submit_turn".to_owned(),
            params: json!({}),
            idempotency_key: Some("k-1".to_owned()),
        };
        let wire = serde_json::to_value(&keyed).unwrap();
        assert_eq!(wire["idempotency_key"], json!("k-1"));
        // Round-trips.
        let back: Frame = serde_json::from_value(wire).unwrap();
        assert!(matches!(back, Frame::Request { id: 8, .. }));
    }

    #[test]
    fn response_frames_flatten_the_outcome() {
        let ok = Frame::Response {
            id: 7,
            outcome: ResponseOutcome::Ok {
                result: json!({"queued": true}),
            },
        };
        assert_eq!(
            serde_json::to_value(&ok).unwrap(),
            json!({"frame": "response", "id": 7, "ok": {"result": {"queued": true}}})
        );
        let err = Frame::Response {
            id: 7,
            outcome: ResponseOutcome::Err {
                error: json!({"kind": "lease_held"}),
            },
        };
        let wire = serde_json::to_value(&err).unwrap();
        assert_eq!(wire["err"], json!({"error": {"kind": "lease_held"}}));
        assert!(serde_json::from_value::<Frame>(wire).is_ok());
    }

    #[test]
    fn event_frames_carry_monotonic_seq() {
        let event = Frame::Event {
            seq: 41,
            name: "run_ended".to_owned(),
            payload: json!({"reason": "done"}),
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["frame"], json!("event"));
        assert_eq!(wire["seq"], json!(41));
    }

    #[test]
    fn pairing_steps_are_tagged_per_the_contract() {
        let hello = Pairing::Hello {
            device_id: "dev-1".to_owned(),
            public_key: "pk".to_owned(),
            platform: "macos".to_owned(),
        };
        assert_eq!(
            serde_json::to_value(&hello).unwrap(),
            json!({"step": "hello", "device_id": "dev-1", "public_key": "pk", "platform": "macos"})
        );
        let paired = Pairing::Paired {
            device_token: "tok".to_owned(),
        };
        assert_eq!(
            serde_json::to_value(&paired).unwrap(),
            json!({"step": "paired", "device_token": "tok"})
        );
    }

    #[test]
    fn steering_verbs_and_admission_reasons_are_closed_snake_case_sets() {
        let verbs: Vec<SteeringVerb> = vec![
            SteeringVerb::Followup,
            SteeringVerb::Steer,
            SteeringVerb::Inject,
        ];
        let names: Vec<String> = verbs
            .iter()
            .map(|v| {
                serde_json::to_value(v)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(names, vec!["followup", "steer", "inject"]);

        let reasons = vec![
            AdmissionReason::Queued,
            AdmissionReason::Coalesced,
            AdmissionReason::Deferred,
            AdmissionReason::RuntimeOffline,
            AdmissionReason::RuntimeUnusable,
            AdmissionReason::AgentRuntimeRequired,
            AdmissionReason::AttributionBlocked,
            AdmissionReason::AlreadyActive,
            AdmissionReason::SelfTriggerSuppressed,
            AdmissionReason::LeaseHeld,
            AdmissionReason::Unauthorized,
        ];
        let names: Vec<String> = reasons
            .iter()
            .map(|r| {
                serde_json::to_value(r)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            names,
            vec![
                "queued",
                "coalesced",
                "deferred",
                "runtime_offline",
                "runtime_unusable",
                "agent_runtime_required",
                "attribution_blocked",
                "already_active",
                "self_trigger_suppressed",
                "lease_held",
                "unauthorized",
            ]
        );
    }

    #[test]
    fn a_turn_lease_round_trips_with_its_timestamps() {
        let lease = TurnLease {
            session_id: Uuid::from_u128(0x5e55),
            holder_run_id: Uuid::from_u128(0xb012),
            acquired_at: DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
            expires_at: DateTime::from_timestamp_millis(1_700_000_030_000).unwrap(),
        };
        let wire = serde_json::to_string(&lease).unwrap();
        let back: TurnLease = serde_json::from_str(&wire).unwrap();
        assert_eq!(back.session_id, lease.session_id);
        assert_eq!(back.expires_at, lease.expires_at);
    }

    #[test]
    fn every_protocol_type_generates_a_schema() {
        // The drift gate's upstream: each named type must schematize.
        for name in PROTOCOL_TYPES {
            let schema = match *name {
                "Frame" => schemars::schema_for!(Frame),
                "ResponseOutcome" => schemars::schema_for!(ResponseOutcome),
                "Pairing" => schemars::schema_for!(Pairing),
                "TurnLease" => schemars::schema_for!(TurnLease),
                "SteeringVerb" => schemars::schema_for!(SteeringVerb),
                "AdmissionReason" => schemars::schema_for!(AdmissionReason),
                other => panic!("unregistered protocol type: {other}"),
            };
            let value = serde_json::to_value(&schema).unwrap();
            assert!(value.is_object(), "{name} produced no schema");
        }
    }
}
