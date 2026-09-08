//! Supply-side crawl normalization: a connector-pulled knowledge record
//! becomes a [`SupplyArtifact`] (EP-07-S07).
//!
//! The corpus contract mirrors ingestion's: a crawl operation returns a
//! JSON array of record objects (or the table-API envelope's `result`
//! array), and the caller declares the artifact kind the stream holds —
//! `kb_article`, `sop`, `runbook`, `macro`, `catalog_item`, `skill`,
//! `memory_block` — because the table name alone does not honestly say
//! what a custom table contains. Every record maps under the corpus's
//! one non-negotiable rule: *never invent supply*. A record missing its
//! identity or title is a typed error, not a guessed artifact, and one
//! bad record fails the whole batch rather than silently narrowing the
//! supply the coverage map claims to know about.

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::induction::{ArtifactKind, SupplyArtifact};

/// A record the crawl normalizer cannot honestly map.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CrawlError {
    /// A required field is absent or the wrong type.
    #[error("record `{record_id}`: field `{field}` is missing or has the wrong type")]
    MissingField {
        /// The source record id, when one could be read.
        record_id: String,
        /// The offending field.
        field: &'static str,
    },
    /// A timestamp field does not parse as RFC 3339.
    #[error("record `{record_id}`: field `{field}` is not an RFC 3339 timestamp")]
    BadTimestamp {
        /// The source record id.
        record_id: String,
        /// The offending field.
        field: &'static str,
    },
    /// The payload named an artifact kind the vocabulary does not carry.
    #[error(
        "unknown artifact kind `{0}` — expected one of the coverage vocabulary: kb_article, sop, \
         runbook, macro, catalog_item, skill, memory_block"
    )]
    UnknownKind(String),
}

/// One knowledge record, normalized. `kind` comes from the caller (the
/// stream's declared contents); the record supplies identity, title,
/// body, revision date, and referenced systems under the ServiceNow
/// table-API field names, with the generic fallbacks a REST export
/// carries.
pub fn normalize_supply_record(
    kind: ArtifactKind,
    record: &Value,
) -> Result<SupplyArtifact, CrawlError> {
    let record_id = record
        .get("sys_id")
        .or_else(|| record.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("<unidentified>")
        .to_owned();

    let artifact_id = record
        .get("sys_id")
        .or_else(|| record.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| CrawlError::MissingField {
            record_id: record_id.clone(),
            field: "sys_id",
        })?
        .to_owned();

    let title = ["short_description", "name", "title"]
        .iter()
        .find_map(|field| {
            record
                .get(field)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .ok_or_else(|| CrawlError::MissingField {
            record_id: record_id.clone(),
            field: "short_description",
        })?
        .to_owned();

    // A body-less artifact can still exact-match by title; an absent
    // body is empty text, not an error.
    let body = ["text", "description", "body"]
        .iter()
        .find_map(|field| record.get(field).and_then(Value::as_str))
        .unwrap_or("")
        .to_owned();

    let last_revised = ["sys_updated_on", "updated_on", "last_revised"]
        .iter()
        .find_map(|field| {
            record
                .get(field)
                .and_then(Value::as_str)
                .map(|value| (*field, value))
        })
        .map(|(field, value)| {
            DateTime::parse_from_rfc3339(value)
                .map(|parsed| parsed.with_timezone(&Utc))
                .map_err(|_| CrawlError::BadTimestamp {
                    record_id: record_id.clone(),
                    field,
                })
        })
        .transpose()?;

    let systems_referenced = match ["systems_referenced", "u_systems", "systems"]
        .iter()
        .find_map(|field| record.get(field))
    {
        Some(Value::Array(systems)) => systems
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(Value::String(list)) => list
            .split(',')
            .map(str::trim)
            .filter(|system| !system.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    };

    Ok(SupplyArtifact {
        artifact_id,
        kind,
        title,
        body,
        last_revised,
        systems_referenced,
    })
}

/// Parse the payload's declared artifact kind. Typed refusal over a
/// guess: a kind the coverage vocabulary does not carry would emit
/// claims downstream consumers cannot read.
pub fn parse_artifact_kind(kind: &str) -> Result<ArtifactKind, CrawlError> {
    serde_json::from_value(Value::String(kind.to_owned()))
        .map_err(|_| CrawlError::UnknownKind(kind.to_owned()))
}

/// The whole corpus, all-or-nothing in record order: one unmappable
/// record fails the crawl — a coverage map computed over silently
/// dropped supply would overstate the gaps it reports.
pub fn normalize_supply_corpus(
    kind: ArtifactKind,
    records: &[Value],
) -> Result<Vec<SupplyArtifact>, CrawlError> {
    records
        .iter()
        .map(|record| normalize_supply_record(kind, record))
        .collect()
}
