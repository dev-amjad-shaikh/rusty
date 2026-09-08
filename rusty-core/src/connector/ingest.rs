//! Ingestion normalization: a ServiceNow-class source record becomes an
//! [`InteractionEvent`] (EP-07-S05).
//!
//! The corpus contract: an ingestion operation returns a JSON array of
//! record objects, each carrying a `stream` field naming the source
//! table (`sp_log`, `sys_cs_conversation`, `sc_request`, `incident`,
//! `sn_customerservice_case`, `sys_escalation`) and the record's own
//! fields. This module maps each record onto the demand schema —
//! channel from the stream, resolution path and outcome from the
//! record's state fields — under the corpus's one non-negotiable rule:
//! *preserve the failures*. A zero-result search stays `no_result`, a
//! reopened incident stays `reopened`; nothing is coerced to a success
//! value, and a record whose state the mapping does not recognize is a
//! typed error, never a guess.
//!
//! Timestamps are required: an event without an `occurred_at` the
//! source supplied is not demand evidence, it is a rumor.

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::gaps::{
    ActorRef, EventSource, InteractionChannel, InteractionEvent, InteractionOutcome, OriginClass,
    ResolutionPath,
};

/// A record the normalizer cannot honestly map.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IngestError {
    /// The record names a stream the mapping does not cover.
    #[error(
        "unknown stream `{0}` — add a mapping or fix the corpus; unknown streams are never guessed"
    )]
    UnknownStream(String),
    /// A required field is absent or the wrong type.
    #[error(
        "record `{record_id}` in stream `{stream}`: field `{field}` is missing or has the wrong type"
    )]
    MissingField {
        /// The source stream.
        stream: String,
        /// The source record id.
        record_id: String,
        /// The offending field.
        field: &'static str,
    },
    /// A state field carries a value the mapping does not recognize.
    #[error(
        "record `{record_id}` in stream `{stream}`: unrecognized {field} value `{value}` — unrecognized states are typed errors, never coerced"
    )]
    UnrecognizedState {
        /// The source stream.
        stream: String,
        /// The source record id.
        record_id: String,
        /// The offending field.
        field: &'static str,
        /// The unrecognized value.
        value: String,
    },
    /// A timestamp field does not parse as RFC 3339.
    #[error(
        "record `{record_id}` in stream `{stream}`: field `{field}` is not an RFC 3339 timestamp"
    )]
    BadTimestamp {
        /// The source stream.
        stream: String,
        /// The source record id.
        record_id: String,
        /// The offending field.
        field: &'static str,
    },
    /// The mapped record fails the event schema's own validation.
    #[error("record maps to an invalid interaction event: {0}")]
    InvalidEvent(String),
}

/// One source record, normalized. Returns a typed error rather than
/// coercing any failure class.
pub fn normalize_servicenow_record(
    system: &str,
    record: &Value,
) -> Result<InteractionEvent, IngestError> {
    let stream = record
        .get("stream")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let record_id = record
        .get("sys_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if record_id.is_empty() {
        return Err(IngestError::MissingField {
            stream,
            record_id,
            field: "sys_id",
        });
    }
    let missing = |field: &'static str| IngestError::MissingField {
        stream: stream.clone(),
        record_id: record_id.clone(),
        field,
    };
    let actor = ActorRef {
        role: string_field(record, "actor_role").unwrap_or_else(|| "employee".to_owned()),
        id: string_field(record, "caller_id")
            .or_else(|| string_field(record, "user"))
            .ok_or(missing("caller_id"))?,
    };
    let occurred_at = timestamp_field(record, "occurred_at").ok_or(missing("occurred_at"))??;
    let resolved_at = match optional_timestamp_field(record, "resolved_at") {
        None => None,
        Some(Ok(ts)) => ts,
        Some(Err(e)) => return Err(e),
    };
    let source = EventSource {
        system: system.to_owned(),
        stream: stream.clone(),
        record_id: record_id.clone(),
    };
    let (channel, utterance, resolution_path, outcome) = match stream.as_str() {
        "sp_log" | "sp_search" => search_shape(&stream, &record_id, record)?,
        "sys_cs_conversation" | "chat" => chat_shape(&stream, &record_id, record)?,
        "sc_request" | "sc_req_item" => request_shape(&stream, &record_id, record)?,
        "incident" => incident_shape(&stream, &record_id, record)?,
        "sn_customerservice_case" | "x_hr_case" | "case" => {
            case_shape(&stream, &record_id, record)?
        }
        "sys_escalation" | "escalation" => escalation_shape(&stream, &record_id, record)?,
        other => return Err(IngestError::UnknownStream(other.to_owned())),
    };
    let event = InteractionEvent::new(
        source,
        actor,
        channel,
        utterance,
        resolution_path,
        outcome,
        occurred_at,
        resolved_at,
        Vec::new(),
    )
    .map_err(|e| IngestError::InvalidEvent(e.to_string()))?;
    Ok(event)
}

/// Normalize a whole corpus and resolve cross-record links: a record's
/// `links` field names source record ids (`{stream}/{sys_id}`); when the
/// referenced record is in the same corpus its derived event id is
/// linked, so journeys land as data. References outside the corpus are
/// dropped — a link must cite an immutable row, and a row we did not
/// ingest is not one.
///
/// `origin_class` marks the whole corpus's trust (EP-07-S09 AC5): a
/// connector reading a third-party feed (a vendor mailbox, a scraped
/// page) ingests as `Untrusted`, and filings derived from those events
/// land as `UntrustedDerived` downstream.
pub fn normalize_corpus(
    system: &str,
    records: &[Value],
    origin_class: OriginClass,
) -> Result<Vec<InteractionEvent>, IngestError> {
    let mut events = Vec::with_capacity(records.len());
    for record in records {
        events.push(normalize_servicenow_record(system, record)?.with_origin_class(origin_class));
    }
    // Source ref → event id, for link resolution.
    let by_ref: std::collections::HashMap<String, String> = events
        .iter()
        .map(|event| {
            (
                format!("{}/{}", event.source.stream, event.source.record_id),
                event.event_id.clone(),
            )
        })
        .collect();
    for (event, record) in events.iter_mut().zip(records) {
        if let Some(links) = record.get("links").and_then(Value::as_array) {
            event.links = links
                .iter()
                .filter_map(Value::as_str)
                .filter_map(|reference| by_ref.get(reference).cloned())
                .collect();
        }
    }
    Ok(events)
}

/// Portal searches (`sp_log`): the failure classes are the signal —
/// zero-result and no-click searches are demand supply visibly missed.
fn search_shape(stream: &str, record_id: &str, record: &Value) -> Result<Shape, IngestError> {
    let utterance = string_field(record, "query").ok_or(IngestError::MissingField {
        stream: stream.into(),
        record_id: record_id.into(),
        field: "query",
    })?;
    let results =
        record
            .get("result_count")
            .and_then(Value::as_u64)
            .ok_or(IngestError::MissingField {
                stream: stream.into(),
                record_id: record_id.into(),
                field: "result_count",
            })?;
    let clicked = record
        .get("clicked")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (path, outcome) = if results == 0 {
        (ResolutionPath::Unresolved, InteractionOutcome::NoResult)
    } else if !clicked {
        (ResolutionPath::Unresolved, InteractionOutcome::NoClick)
    } else {
        (ResolutionPath::SelfService, InteractionOutcome::Resolved)
    };
    Ok((InteractionChannel::PortalSearch, utterance, path, outcome))
}

/// Chats (`sys_cs_conversation`): a virtual-agent conversation deflects,
/// abandons, or escalates to a human.
fn chat_shape(stream: &str, record_id: &str, record: &Value) -> Result<Shape, IngestError> {
    let utterance = string_field(record, "short_description")
        .or_else(|| string_field(record, "question"))
        .ok_or(IngestError::MissingField {
            stream: stream.into(),
            record_id: record_id.into(),
            field: "short_description",
        })?;
    let state = state_field(stream, record_id, record)?;
    let (path, outcome) = match state.as_str() {
        "deflected" | "resolved" => (ResolutionPath::Deflected, InteractionOutcome::Resolved),
        "abandoned" => (ResolutionPath::Abandoned, InteractionOutcome::Escalated),
        "escalated" | "handed_off" => {
            (ResolutionPath::HumanResolved, InteractionOutcome::Escalated)
        }
        other => {
            return Err(IngestError::UnrecognizedState {
                stream: stream.into(),
                record_id: record_id.into(),
                field: "state",
                value: other.to_owned(),
            });
        }
    };
    Ok((InteractionChannel::Chat, utterance, path, outcome))
}

/// Catalog requests (`sc_request`): fulfilled by automation or a human;
/// a cancelled request is demand that went unresolved.
fn request_shape(stream: &str, record_id: &str, record: &Value) -> Result<Shape, IngestError> {
    let utterance = string_field(record, "short_description").ok_or(IngestError::MissingField {
        stream: stream.into(),
        record_id: record_id.into(),
        field: "short_description",
    })?;
    let state = state_field(stream, record_id, record)?;
    let (path, outcome) = match state.as_str() {
        "closed_complete" | "fulfilled" => {
            (ResolutionPath::HumanResolved, InteractionOutcome::Resolved)
        }
        "closed_incomplete" | "cancelled" => {
            (ResolutionPath::Unresolved, InteractionOutcome::Escalated)
        }
        other => {
            return Err(IngestError::UnrecognizedState {
                stream: stream.into(),
                record_id: record_id.into(),
                field: "state",
                value: other.to_owned(),
            });
        }
    };
    Ok((InteractionChannel::Request, utterance, path, outcome))
}

/// Incidents (`incident`): reopen and multi-reassignment history are
/// first-class — a reopened incident's fix did not hold, and a ticket
/// bounced between queues is an escalation in fact.
fn incident_shape(stream: &str, record_id: &str, record: &Value) -> Result<Shape, IngestError> {
    let utterance = string_field(record, "short_description").ok_or(IngestError::MissingField {
        stream: stream.into(),
        record_id: record_id.into(),
        field: "short_description",
    })?;
    let state = state_field(stream, record_id, record)?;
    let reopen_count = record
        .get("reopen_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reassignments = record
        .get("reassignment_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let (path, outcome) = match state.as_str() {
        "resolved" | "closed" if reopen_count > 0 => {
            (ResolutionPath::HumanResolved, InteractionOutcome::Reopened)
        }
        "resolved" | "closed" if reassignments > 1 => {
            (ResolutionPath::HumanResolved, InteractionOutcome::Escalated)
        }
        "resolved" | "closed" => (ResolutionPath::HumanResolved, InteractionOutcome::Resolved),
        "unresolved" | "open" => (ResolutionPath::Unresolved, InteractionOutcome::Escalated),
        other => {
            return Err(IngestError::UnrecognizedState {
                stream: stream.into(),
                record_id: record_id.into(),
                field: "state",
                value: other.to_owned(),
            });
        }
    };
    Ok((InteractionChannel::Incident, utterance, path, outcome))
}

/// Cases (`sn_customerservice_case` / HR cases): the general-case shape;
/// escalations between tiers are the outcome, not a state to lose.
fn case_shape(stream: &str, record_id: &str, record: &Value) -> Result<Shape, IngestError> {
    let utterance = string_field(record, "short_description").ok_or(IngestError::MissingField {
        stream: stream.into(),
        record_id: record_id.into(),
        field: "short_description",
    })?;
    let state = state_field(stream, record_id, record)?;
    let (path, outcome) = match state.as_str() {
        "closed" | "resolved" => (ResolutionPath::HumanResolved, InteractionOutcome::Resolved),
        "escalated" => (ResolutionPath::HumanResolved, InteractionOutcome::Escalated),
        "abandoned" | "cancelled" => (ResolutionPath::Abandoned, InteractionOutcome::Escalated),
        other => {
            return Err(IngestError::UnrecognizedState {
                stream: stream.into(),
                record_id: record_id.into(),
                field: "state",
                value: other.to_owned(),
            });
        }
    };
    Ok((InteractionChannel::Case, utterance, path, outcome))
}

/// Escalations (`sys_escalation`): every record is one by construction.
fn escalation_shape(stream: &str, record_id: &str, record: &Value) -> Result<Shape, IngestError> {
    let utterance = string_field(record, "short_description")
        .or_else(|| string_field(record, "reason"))
        .ok_or(IngestError::MissingField {
            stream: stream.into(),
            record_id: record_id.into(),
            field: "short_description",
        })?;
    let state = state_field(stream, record_id, record)?;
    let path = match state.as_str() {
        "resolved" | "closed" => ResolutionPath::HumanResolved,
        "open" | "pending" => ResolutionPath::Unresolved,
        other => {
            return Err(IngestError::UnrecognizedState {
                stream: stream.into(),
                record_id: record_id.into(),
                field: "state",
                value: other.to_owned(),
            });
        }
    };
    Ok((
        InteractionChannel::Escalation,
        utterance,
        path,
        InteractionOutcome::Escalated,
    ))
}

/// The four mapped fields of a shape function.
type Shape = (
    InteractionChannel,
    String,
    ResolutionPath,
    InteractionOutcome,
);

fn string_field(record: &Value, field: &str) -> Option<String> {
    record
        .get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn state_field(stream: &str, record_id: &str, record: &Value) -> Result<String, IngestError> {
    string_field(record, "state").ok_or(IngestError::MissingField {
        stream: stream.into(),
        record_id: record_id.into(),
        field: "state",
    })
}

/// A required RFC 3339 timestamp field. `Ok(None)` means missing;
/// `Some(Err)` means present but unparseable.
fn timestamp_field(
    record: &Value,
    field: &'static str,
) -> Option<Result<DateTime<Utc>, IngestError>> {
    let value = record.get(field).and_then(Value::as_str)?;
    Some(parse_timestamp(record, field, value))
}

/// An optional RFC 3339 timestamp field: absent is `Ok(None)`, present
/// but unparseable is a typed error.
fn optional_timestamp_field(
    record: &Value,
    field: &'static str,
) -> Option<Result<Option<DateTime<Utc>>, IngestError>> {
    let value = record.get(field).and_then(Value::as_str)?;
    Some(parse_timestamp(record, field, value).map(Some))
}

fn parse_timestamp(
    record: &Value,
    field: &'static str,
    value: &str,
) -> Result<DateTime<Utc>, IngestError> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| IngestError::BadTimestamp {
            stream: record
                .get("stream")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            record_id: record
                .get("sys_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            field,
        })
}
