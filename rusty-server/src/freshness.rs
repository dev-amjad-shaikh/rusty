//! A learned reference goes stale when the system moves.
//!
//! A skill learns from the system it follows (`skill_learn`): the reads it
//! declares run through the live connection and what came back is kept as
//! a reference every follower reads. Until now that reference carried the
//! date it was learned and nothing else, so a desk read last month's
//! incident form as though it were this morning's, and nothing noticed.
//!
//! Learning now stamps each read with what the system answered *at the
//! time*: how many records, the newest version field it carries
//! (`sys_updated_on` and its kin), and a digest of the whole answer. The
//! check re-runs the same reads and compares the stamps — a value
//! comparison, no model, the shape [`crate::promotion::stale_because`]
//! uses for evaluation evidence — and names each difference in words. A
//! reference the check found stale stays readable and says so wherever an
//! agent reads it: durable uncertainty, not a silent deletion. Re-learning
//! is what clears it.
use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Path as AxumPath, State as AxumState};
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;
use crate::skill_learn::LearnRead;

const NAMESPACE: &str = "skill-freshness";

/// The fields a system uses to say when a record last moved, in the order
/// they are looked for. The first one a row carries is the read's version
/// field; the largest value across the rows is its watermark.
const VERSION_FIELDS: [&str; 12] = [
    "sys_updated_on",
    "sys_created_on",
    "updated_at",
    "last_modified",
    "lastModified",
    "modified_at",
    "modified",
    "changed_at",
    "opened_at",
    "etag",
    "version",
    "revision",
];

/// What one read answered when it ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadStamp {
    pub title: String,
    pub tool: String,
    #[serde(default)]
    pub arguments: Value,
    pub records: usize,
    /// A digest of the whole answer: the one comparison that catches a
    /// change no counted field shows.
    pub digest: String,
    /// The field the rows date themselves by, when they carry one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_field: Option<String>,
    /// The largest value of that field across the rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest: Option<String>,
    /// When the read is a count by group, the count per group: what moved
    /// is then a group and a number rather than "the answer changed".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups: Option<BTreeMap<String, String>>,
}

/// What a reference was learned from, and what the last check made of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stamp {
    pub skill: String,
    pub revision: u64,
    pub reference: String,
    pub learned_at: DateTime<Utc>,
    pub reads: Vec<ReadStamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub stale: bool,
    /// Why, in words — empty while it is current.
    #[serde(default)]
    pub because: Vec<String>,
}

fn namespace(tenant: &str) -> String {
    format!("{NAMESPACE}:{tenant}")
}

/// The rows of an answer: the `{"result": [...]}` shape a connector
/// returns, or a bare list.
fn rows_of(answer: &Value) -> Vec<Value> {
    answer
        .get("result")
        .and_then(Value::as_array)
        .or_else(|| answer.as_array())
        .cloned()
        .unwrap_or_default()
}

/// The field these rows date themselves by, and the newest value of it.
fn watermark(rows: &[Value]) -> Option<(String, String)> {
    let field = VERSION_FIELDS.iter().find(|f| {
        rows.iter().any(|row| row.get(**f).is_some_and(|v| !v.is_null() && v.as_str() != Some("")))
    })?;
    let newest = rows
        .iter()
        .filter_map(|row| row.get(*field))
        .filter_map(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Null => None,
            other => Some(other.to_string()),
        })
        .filter(|s| !s.is_empty())
        .max()?;
    Some(((*field).to_owned(), newest))
}

/// A count-by-group answer as `{group: count}`: the shape an aggregate
/// returns, where every row names the field it grouped by and its count.
/// `None` for anything else, and then the digest is the comparison.
fn group_counts(rows: &[Value]) -> Option<BTreeMap<String, String>> {
    if rows.is_empty() {
        return None;
    }
    let mut out = BTreeMap::new();
    for row in rows {
        let count = row.pointer("/stats/count").map(|c| match c {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })?;
        let group = row
            .get("groupby_fields")
            .and_then(Value::as_array)
            .map(|fields| {
                fields
                    .iter()
                    .map(|f| {
                        let name = f.get("field").and_then(Value::as_str).unwrap_or("");
                        let value = f.get("value").and_then(Value::as_str).unwrap_or("");
                        if value.is_empty() { format!("{name} (empty)") } else { format!("{name} {value}") }
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|g| !g.is_empty())
            .unwrap_or_else(|| "all".to_owned());
        out.insert(group, count);
    }
    Some(out)
}

fn digest_of(answer: &Value) -> String {
    let canonical = serde_json::to_string(answer).unwrap_or_default();
    let bytes = Sha256::digest(canonical.as_bytes());
    bytes.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// What a read answered, recorded so the same read can be compared later.
pub(crate) fn stamp_read(read: &LearnRead, answer: &Value) -> ReadStamp {
    let rows = rows_of(answer);
    let (version_field, newest) = match watermark(&rows) {
        Some((field, newest)) => (Some(field), Some(newest)),
        None => (None, None),
    };
    ReadStamp {
        title: read.title.clone(),
        tool: read.tool.clone(),
        arguments: read.arguments.clone(),
        records: rows.len(),
        digest: digest_of(answer),
        version_field,
        newest,
        groups: group_counts(&rows),
    }
}

impl ReadStamp {
    /// The read that produced this stamp, so the check re-runs exactly it.
    pub(crate) fn as_read(&self) -> LearnRead {
        LearnRead { title: self.title.clone(), tool: self.tool.clone(), arguments: self.arguments.clone(), rows: None }
    }
}

/// Every way the system has moved since the reference was learned, in
/// words. Empty means nothing the reads can see has changed.
pub fn moved_since(then: &[ReadStamp], now: &[ReadStamp]) -> Vec<String> {
    let mut out = Vec::new();
    for fresh in now {
        let Some(old) = then.iter().find(|s| s.title == fresh.title && s.tool == fresh.tool) else {
            out.push(format!("{}: a read the reference does not cover", fresh.title));
            continue;
        };
        if old.digest == fresh.digest {
            continue;
        }
        let mut said = false;
        // For a count by group the record count is only the number of
        // groups, and the group lines below say it better.
        let grouped = old.groups.is_some() && fresh.groups.is_some();
        if old.records != fresh.records && !grouped {
            out.push(format!("{}: {} records → {}", fresh.title, old.records, fresh.records));
            said = true;
        }
        match (&old.newest, &fresh.newest) {
            (Some(before), Some(after)) if before != after => {
                let field = fresh.version_field.clone().unwrap_or_else(|| "version".to_owned());
                out.push(format!("{}: newest {field} {before} → {after}", fresh.title));
                said = true;
            }
            _ => {}
        }
        // A count by group says which group moved, and by how much.
        if let (Some(before), Some(after)) = (&old.groups, &fresh.groups) {
            for (group, count) in after {
                match before.get(group) {
                    Some(was) if was != count => {
                        out.push(format!("{}: {group} {was} → {count}", fresh.title));
                        said = true;
                    }
                    None => {
                        out.push(format!("{}: {group} is counted now and was not ({count})", fresh.title));
                        said = true;
                    }
                    _ => {}
                }
            }
            for group in before.keys().filter(|g| !after.contains_key(*g)) {
                out.push(format!("{}: {group} is no longer counted", fresh.title));
                said = true;
            }
        }
        if !said {
            out.push(format!("{}: the answer changed", fresh.title));
        }
    }
    for gone in then.iter().filter(|s| !now.iter().any(|f| f.title == s.title && f.tool == s.tool)) {
        out.push(format!("{}: the skill no longer declares this read", gone.title));
    }
    out
}

pub(crate) async fn load(state: &AppState, tenant: &str, skill: &str) -> Option<Stamp> {
    state
        .server_store
        .kv_get(&namespace(tenant), skill)
        .await
        .ok()
        .flatten()
        .and_then(|item| serde_json::from_value(item.value).ok())
}

pub(crate) async fn keep(state: &AppState, tenant: &str, stamp: &Stamp) {
    let Ok(value) = serde_json::to_value(stamp) else { return };
    if let Err(error) = state.server_store.kv_put(&namespace(tenant), &stamp.skill, value).await {
        tracing::warn!(skill = %stamp.skill, %error, "the freshness stamp was not kept");
    }
}

/// The notice an agent reads above a stale reference. Durable uncertainty:
/// the reference stays readable and says what moved under it.
pub fn notice(stamp: &Stamp) -> String {
    format!(
        "> **STALE — the system has moved since this was learned.** Checked {}: {}. What is below is what the system said on {}; treat it as a hint, not the current state, and read the system for anything that matters.\n\n",
        stamp.checked_at.unwrap_or(stamp.learned_at).format("%Y-%m-%d %H:%M UTC"),
        stamp.because.join("; "),
        stamp.learned_at.format("%Y-%m-%d")
    )
}

/// The reference's text as an agent should read it: the notice first when
/// the last check found it stale.
pub(crate) async fn as_read(state: &AppState, tenant: &str, skill: &str, text: String) -> (String, bool) {
    match load(state, tenant, skill).await {
        Some(stamp) if stamp.stale => (format!("{}{text}", notice(&stamp)), true),
        _ => (text, false),
    }
}

/// Re-run the skill's declared reads and compare them with the stamp.
/// Read-only, deterministic, no model.
pub(crate) async fn check(state: &AppState, tenant: &TenantContext, skill: &str) -> Result<Stamp, String> {
    let Some(mut stamp) = load(state, tenant.tenant(), skill).await else {
        return Err(format!("`{skill}` has learned nothing yet, so there is nothing to check"));
    };
    // The reads to re-run are the ones the reference was learned from,
    // not whatever the skill declares now: a comparison is only honest
    // between like and like. Learning again is what changes the recipe.
    let reads: Vec<LearnRead> = stamp.reads.iter().map(|s| s.as_read()).collect();
    if reads.is_empty() {
        return Err(format!("`{skill}` was learned from no reads, so there is nothing to re-read"));
    }
    let (_, _, now) = crate::skill_learn::learn(state, skill, &reads).await?;
    let because = moved_since(&stamp.reads, &now);
    stamp.checked_at = Some(Utc::now());
    stamp.stale = !because.is_empty();
    stamp.because = because;
    keep(state, tenant.tenant(), &stamp).await;
    if stamp.stale {
        tracing::info!(skill, reasons = stamp.because.len(), "a learned reference went stale");
    }
    Ok(stamp)
}

fn served(stamp: &Stamp) -> Value {
    json!({
        "skill": stamp.skill,
        "revision": stamp.revision,
        "reference": stamp.reference,
        "learned_at": stamp.learned_at,
        "checked_at": stamp.checked_at,
        "stale": stamp.stale,
        "because": stamp.because,
        "reads": stamp.reads.iter().map(|r| json!({
            "title": r.title,
            "tool": r.tool,
            "records": r.records,
            "version_field": r.version_field,
            "newest": r.newest,
        })).collect::<Vec<_>>(),
    })
}

/// `GET /skills/{name}/freshness` — what the last check made of it.
pub(crate) async fn get_freshness(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(match load(&state, tenant.tenant(), &name).await {
        Some(stamp) => json!({"learned": true, "freshness": served(&stamp)}),
        None => json!({"learned": false, "note": "this skill has learned nothing from a system yet"}),
    }))
}

/// `POST /skills/{name}/freshness` — re-read the system now and compare.
pub(crate) async fn post_freshness(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let stamp = check(&state, &tenant, &name).await.map_err(ApiError::unprocessable)?;
    Ok(Json(json!({"learned": true, "freshness": served(&stamp)})))
}

/// Every learned skill, re-read and compared: the nightly pass beside the
/// suites. A skill whose reads cannot run is left as it was, with why.
pub(crate) async fn check_all(state: &Arc<AppState>, tenant: &TenantContext) -> Vec<Value> {
    let mut out = Vec::new();
    let Ok(items) = state.server_store.kv_list(&namespace(tenant.tenant())).await else {
        return out;
    };
    let skills: Vec<String> = items
        .into_iter()
        .filter_map(|item| serde_json::from_value::<Stamp>(item.value).ok())
        .map(|stamp| stamp.skill)
        .collect();
    for skill in skills {
        match check(state, tenant, &skill).await {
            Ok(stamp) => out.push(json!({"skill": skill, "stale": stamp.stale, "because": stamp.because})),
            Err(reason) => out.push(json!({"skill": skill, "skipped": reason})),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(title: &str) -> LearnRead {
        serde_json::from_value(json!({"title": title, "tool": "servicenow.list-records", "arguments": {"table": "incident"}})).expect("a read")
    }

    #[test]
    fn a_stamp_records_the_count_the_watermark_and_a_digest() {
        let answer = json!({"result": [
            {"number": "INC1", "sys_updated_on": "2026-09-01 10:00:00"},
            {"number": "INC2", "sys_updated_on": "2026-09-03 08:30:00"},
        ]});
        let stamp = stamp_read(&read("Resolutions"), &answer);
        assert_eq!(stamp.records, 2);
        assert_eq!(stamp.version_field.as_deref(), Some("sys_updated_on"));
        assert_eq!(stamp.newest.as_deref(), Some("2026-09-03 08:30:00"));
        assert_eq!(stamp.digest.len(), 32);
        // The same answer stamps the same; rows with no version field
        // still carry a digest.
        assert_eq!(stamp_read(&read("Resolutions"), &answer), stamp);
        let plain = stamp_read(&read("Plain"), &json!({"result": [{"number": "INC1"}]}));
        assert_eq!(plain.version_field, None);
        assert_eq!(plain.records, 1);
        assert_eq!(plain.groups, None, "a list of records is not a count by group");
    }

    #[test]
    fn a_count_by_group_says_which_group_moved() {
        let by_priority = |p3: &str, p4: &str| {
            json!({"result": [
                {"groupby_fields": [{"field": "priority", "value": "3"}], "stats": {"count": p3}},
                {"groupby_fields": [{"field": "priority", "value": "4"}], "stats": {"count": p4}},
            ]})
        };
        let then = vec![stamp_read(&read("Open by priority"), &by_priority("4", "2"))];
        assert_eq!(then[0].groups.as_ref().expect("groups")["priority 3"], "4");
        assert!(moved_since(&then, &then).is_empty());
        // One more P3 open: the group and the numbers, not "something changed".
        let now = vec![stamp_read(&read("Open by priority"), &by_priority("5", "2"))];
        assert_eq!(moved_since(&then, &now), vec!["Open by priority: priority 3 4 → 5".to_owned()]);
        // A group that appears, and one that goes.
        let shifted = json!({"result": [{"groupby_fields": [{"field": "priority", "value": "1"}], "stats": {"count": "1"}}]});
        let because = moved_since(&then, &[stamp_read(&read("Open by priority"), &shifted)]);
        assert_eq!(because, vec![
            "Open by priority: priority 1 is counted now and was not (1)".to_owned(),
            "Open by priority: priority 3 is no longer counted".to_owned(),
            "Open by priority: priority 4 is no longer counted".to_owned(),
        ]);
    }

    #[test]
    fn the_check_names_what_moved_and_stays_quiet_when_nothing_did() {
        let before = json!({"result": [{"number": "INC1", "sys_updated_on": "2026-09-01 10:00:00"}]});
        let then = vec![stamp_read(&read("Resolutions"), &before)];
        assert!(moved_since(&then, &then).is_empty(), "an unmoved system says nothing");

        // A record added, and the watermark with it: both said, in words.
        let after = json!({"result": [
            {"number": "INC1", "sys_updated_on": "2026-09-01 10:00:00"},
            {"number": "INC2", "sys_updated_on": "2026-09-11 09:00:00"},
        ]});
        let now = vec![stamp_read(&read("Resolutions"), &after)];
        let because = moved_since(&then, &now);
        assert_eq!(because, vec![
            "Resolutions: 1 records → 2".to_owned(),
            "Resolutions: newest sys_updated_on 2026-09-01 10:00:00 → 2026-09-11 09:00:00".to_owned(),
        ]);

        // A record edited in place: the count and the watermark both hold,
        // and the digest is what catches it.
        let edited = json!({"result": [{"number": "INC1", "sys_updated_on": "2026-09-01 10:00:00", "state": "Closed"}]});
        assert_eq!(moved_since(&then, &[stamp_read(&read("Resolutions"), &edited)]), vec!["Resolutions: the answer changed".to_owned()]);

        // Reads that came and went are named too.
        let other = vec![stamp_read(&read("Catalog"), &before)];
        let both = moved_since(&then, &other);
        assert_eq!(both, vec![
            "Catalog: a read the reference does not cover".to_owned(),
            "Resolutions: the skill no longer declares this read".to_owned(),
        ]);
    }

    #[test]
    fn the_notice_says_what_moved_and_when_it_was_learned() {
        let stamp = Stamp {
            skill: "servicenow-resolutions".to_owned(),
            revision: 3,
            reference: "learned/x.md".to_owned(),
            learned_at: "2026-09-01T10:00:00Z".parse().expect("a time"),
            reads: Vec::new(),
            checked_at: Some("2026-09-11T02:00:00Z".parse().expect("a time")),
            stale: true,
            because: vec!["Resolutions: 12 records → 18".to_owned()],
        };
        let notice = notice(&stamp);
        assert!(notice.starts_with("> **STALE"), "{notice}");
        assert!(notice.contains("Resolutions: 12 records → 18"));
        assert!(notice.contains("2026-09-11 02:00 UTC") && notice.contains("2026-09-01"));
    }
}
