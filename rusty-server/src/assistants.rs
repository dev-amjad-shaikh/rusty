//! Assistants: named aliases for a registered graph plus default config.
//!
//! An assistant binds a human-readable name and a free-form `config` /
//! `metadata` blob to a registered graph, so clients can create runs by
//! `assistant_id` instead of repeating a graph name and config on every
//! call. Records live in memory and are persisted as one JSON file per
//! assistant under `{store_path}/assistants/{assistant_id}.json`; they are
//! reloaded when the router is built.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use rusty_agent_runtime::record::sha256_hex;

const VERSION_PREFIX: &str = "av-";
pub(crate) const ASSISTANT_VERSION_LIMIT: usize = 256;
pub(crate) const ASSISTANT_VERSION_BYTES_LIMIT: usize = 64 * 1024;
pub(crate) const ASSISTANT_LINEAGE_BYTES_LIMIT: usize = 1024 * 1024;

/// One immutable configuration snapshot in an assistant's lineage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct AssistantVersionRecord {
    pub version_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_version_id: Option<String>,
    pub name: String,
    pub graph: String,
    #[serde(default)]
    pub config: Value,
    #[serde(default)]
    pub metadata: Value,
    pub created_at: DateTime<Utc>,
}

impl AssistantVersionRecord {
    pub(crate) fn new(
        parent_version_id: Option<String>,
        name: String,
        graph: String,
        config: Value,
        metadata: Value,
        created_at: DateTime<Utc>,
    ) -> Self {
        let version_id = version_id(
            parent_version_id.as_deref(),
            &name,
            &graph,
            &config,
            &metadata,
        );
        Self {
            version_id,
            parent_version_id,
            name,
            graph,
            config,
            metadata,
            created_at,
        }
    }

    pub(crate) fn storage_size(&self) -> usize {
        serde_json::to_vec(self)
            .expect("assistant version serialization is infallible")
            .len()
    }
}

/// Public assistant catalog shape. Version bodies are served only by the
/// version endpoints so the normal list remains bounded.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AssistantView {
    pub assistant_id: String,
    pub name: String,
    pub graph: String,
    pub config: Value,
    pub metadata: Value,
    pub created_at: DateTime<Utc>,
    pub active_version_id: String,
    pub version_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AssistantVersionView {
    #[serde(flatten)]
    pub version: AssistantVersionRecord,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AssistantVersionSummary {
    pub version_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_version_id: Option<String>,
    pub graph: String,
    pub created_at: DateTime<Utc>,
    pub active: bool,
}

#[derive(Debug, Clone)]
pub(crate) enum CreateVersionOutcome {
    Created {
        record: AssistantRecord,
        version: AssistantVersionRecord,
    },
    Existing {
        record: AssistantRecord,
        version: AssistantVersionRecord,
    },
    AssistantNotFound,
    Stale {
        active_version_id: String,
    },
    LimitReached,
    SizeLimitReached,
}

#[derive(Debug, Clone)]
pub(crate) enum ActivateVersionOutcome {
    Activated { record: AssistantRecord },
    AlreadyActive { record: AssistantRecord },
    AssistantNotFound,
    VersionNotFound,
    Stale { active_version_id: String },
}

/// One assistant: a named alias for a graph with default config metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AssistantRecord {
    pub assistant_id: String,
    pub name: String,
    /// Registered graph this assistant runs.
    pub graph: String,
    /// Free-form config metadata; `config.recursion_limit` is honored as a
    /// run default, everything else is stored verbatim.
    #[serde(default)]
    pub config: Value,
    #[serde(default)]
    pub metadata: Value,
    pub created_at: DateTime<Utc>,
    /// The currently served immutable snapshot. Legacy records omit this and
    /// are interpreted as one deterministic initial version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_version_id: Option<String>,
    /// Immutable lineage stored with the assistant but intentionally omitted
    /// from the ordinary catalog wire shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub versions: Vec<AssistantVersionRecord>,
}

impl AssistantRecord {
    pub(crate) fn new(
        assistant_id: String,
        name: String,
        graph: String,
        config: Value,
        metadata: Value,
        created_at: DateTime<Utc>,
    ) -> Self {
        let initial = AssistantVersionRecord::new(
            None,
            name.clone(),
            graph.clone(),
            config.clone(),
            metadata.clone(),
            created_at,
        );
        Self {
            assistant_id,
            name,
            graph,
            config,
            metadata,
            created_at,
            active_version_id: Some(initial.version_id.clone()),
            versions: vec![initial],
        }
    }

    pub(crate) fn ensure_version_history(&mut self) {
        if self.versions.is_empty() {
            self.versions.push(AssistantVersionRecord::new(
                None,
                self.name.clone(),
                self.graph.clone(),
                self.config.clone(),
                self.metadata.clone(),
                self.created_at,
            ));
            self.active_version_id = Some(self.versions[0].version_id.clone());
        }
    }

    pub(crate) fn validate_lineage(&self) -> Result<(), &'static str> {
        if self.versions.is_empty() {
            return if self.active_version_id.is_none() {
                Ok(())
            } else {
                Err("legacy assistant has an active pointer without a lineage")
            };
        }
        if self.versions.len() > ASSISTANT_VERSION_LIMIT {
            return Err("assistant lineage exceeds the version limit");
        }
        let mut ids = HashSet::with_capacity(self.versions.len());
        let mut roots = 0usize;
        for version in &self.versions {
            if version.storage_size() > ASSISTANT_VERSION_BYTES_LIMIT {
                return Err("assistant version exceeds the storage boundary");
            }
            if version.version_id
                != version_id(
                    version.parent_version_id.as_deref(),
                    &version.name,
                    &version.graph,
                    &version.config,
                    &version.metadata,
                )
            {
                return Err("assistant version content address does not match its body");
            }
            if !ids.insert(version.version_id.as_str()) {
                return Err("assistant lineage contains a duplicate version id");
            }
            if version.parent_version_id.is_none() {
                roots += 1;
            }
        }
        if roots != 1 {
            return Err("assistant lineage must contain exactly one root");
        }
        if self.lineage_storage_size() > ASSISTANT_LINEAGE_BYTES_LIMIT {
            return Err("assistant lineage exceeds the storage boundary");
        }
        for version in &self.versions {
            if let Some(parent) = &version.parent_version_id {
                if parent == &version.version_id || !ids.contains(parent.as_str()) {
                    return Err("assistant lineage contains an invalid parent");
                }
                let mut cursor = Some(parent.as_str());
                let mut visited = HashSet::new();
                while let Some(id) = cursor {
                    if !visited.insert(id) {
                        return Err("assistant lineage contains a cycle");
                    }
                    cursor = self
                        .versions
                        .iter()
                        .find(|candidate| candidate.version_id == id)
                        .and_then(|candidate| candidate.parent_version_id.as_deref());
                }
            }
        }
        let active_id = self
            .active_version_id
            .as_deref()
            .ok_or("assistant lineage has no active pointer")?;
        let active = self
            .versions
            .iter()
            .find(|version| version.version_id == active_id)
            .ok_or("assistant lineage active pointer is missing")?;
        if self.name != active.name
            || self.graph != active.graph
            || self.config != active.config
            || self.metadata != active.metadata
        {
            return Err("assistant serving fields do not match the active version");
        }
        Ok(())
    }

    pub(crate) fn active_version_id(&self) -> String {
        self.active_version_id
            .clone()
            .filter(|id| {
                self.versions
                    .iter()
                    .any(|version| &version.version_id == id)
            })
            .unwrap_or_else(|| {
                version_id(None, &self.name, &self.graph, &self.config, &self.metadata)
            })
    }

    pub(crate) fn version_history(&self) -> Vec<AssistantVersionRecord> {
        if self.versions.is_empty() {
            vec![AssistantVersionRecord::new(
                None,
                self.name.clone(),
                self.graph.clone(),
                self.config.clone(),
                self.metadata.clone(),
                self.created_at,
            )]
        } else {
            self.versions.clone()
        }
    }

    pub(crate) fn version(&self, version_id: &str) -> Option<AssistantVersionRecord> {
        if self.versions.is_empty() {
            let initial = AssistantVersionRecord::new(
                None,
                self.name.clone(),
                self.graph.clone(),
                self.config.clone(),
                self.metadata.clone(),
                self.created_at,
            );
            return (initial.version_id == version_id).then_some(initial);
        }
        self.versions
            .iter()
            .find(|version| version.version_id == version_id)
            .cloned()
    }

    pub(crate) fn version_summaries(&self) -> Vec<AssistantVersionSummary> {
        let active_id = self.active_version_id();
        let mut summaries: Vec<AssistantVersionSummary> = if self.versions.is_empty() {
            self.version(&active_id)
                .into_iter()
                .map(|version| AssistantVersionSummary {
                    active: true,
                    version_id: version.version_id,
                    parent_version_id: version.parent_version_id,
                    graph: version.graph,
                    created_at: version.created_at,
                })
                .collect()
        } else {
            self.versions
                .iter()
                .map(|version| AssistantVersionSummary {
                    active: version.version_id == active_id,
                    version_id: version.version_id.clone(),
                    parent_version_id: version.parent_version_id.clone(),
                    graph: version.graph.clone(),
                    created_at: version.created_at,
                })
                .collect()
        };
        summaries.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| right.version_id.cmp(&left.version_id))
        });
        summaries
    }

    pub(crate) fn lineage_storage_size_with(&self, version: &AssistantVersionRecord) -> usize {
        let mut history = self.version_history();
        if !history
            .iter()
            .any(|existing| existing.version_id == version.version_id)
        {
            history.push(version.clone());
        }
        serde_json::to_vec(&history)
            .expect("assistant lineage serialization is infallible")
            .len()
    }

    pub(crate) fn lineage_storage_size(&self) -> usize {
        serde_json::to_vec(&self.version_history())
            .expect("assistant lineage serialization is infallible")
            .len()
    }

    pub(crate) fn view(&self, assistant_id: String) -> AssistantView {
        AssistantView {
            assistant_id,
            name: self.name.clone(),
            graph: self.graph.clone(),
            config: self.config.clone(),
            metadata: self.metadata.clone(),
            created_at: self.created_at,
            active_version_id: self.active_version_id(),
            version_count: self.versions.len().max(1),
        }
    }
}

pub(crate) fn valid_version_id(value: &str) -> bool {
    value.len() == VERSION_PREFIX.len() + 64
        && value.starts_with(VERSION_PREFIX)
        && value[VERSION_PREFIX.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn version_id(
    parent_version_id: Option<&str>,
    name: &str,
    graph: &str,
    config: &Value,
    metadata: &Value,
) -> String {
    let body = serde_json::json!({
        "parent_version_id": parent_version_id,
        "name": name,
        "graph": graph,
        "config": canonicalize(config),
        "metadata": canonicalize(metadata),
    });
    let bytes = serde_json::to_vec(&canonicalize(&body))
        .expect("assistant version serialization is infallible");
    format!("{VERSION_PREFIX}{}", sha256_hex(&bytes))
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonicalize).collect()),
        Value::Object(values) => {
            let mut keys: Vec<&String> = values.keys().collect();
            keys.sort_unstable();
            let mut out = Map::new();
            for key in keys {
                out.insert(key.clone(), canonicalize(&values[key]));
            }
            Value::Object(out)
        }
        _ => value.clone(),
    }
}

/// The on-disk directory holding one JSON file per assistant.
pub(crate) fn dir(store_root: &Path) -> PathBuf {
    store_root.join("assistants")
}

/// Load all persisted assistants, skipping (with a warning) any file that
/// fails to parse. Tenant-scoped assistants live one directory deeper
/// (`assistants/{tenant}/{assistant_id}.json`), so the walk is recursive.
pub(crate) fn load(store_root: &Path) -> HashMap<String, AssistantRecord> {
    let mut out = HashMap::new();
    let mut files = Vec::new();
    collect_json_files(&dir(store_root), &mut files);
    for path in files {
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<AssistantRecord>(&raw).ok())
            .and_then(|record| record.validate_lineage().is_ok().then_some(record));
        match parsed {
            Some(record) => {
                out.insert(record.assistant_id.clone(), record);
            }
            None => {
                tracing::warn!(path = %path.display(), "skipping unreadable assistant file")
            }
        }
    }
    out
}

/// Recursively collect `*.json` files under `root` (tenant subdirectories
/// hold that tenant's records).
fn collect_json_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_json_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
            out.push(path);
        }
    }
}

/// Persist one assistant record (create or overwrite). The id may carry a
/// `{tenant}/` prefix, so the parent directory is created, not just the
/// flat assistants dir.
pub(crate) async fn persist(store_root: &Path, record: &AssistantRecord) -> std::io::Result<()> {
    let path = dir(store_root).join(format!("{}.json", record.assistant_id));
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let raw = serde_json::to_vec_pretty(record).expect("assistant serialization is infallible");
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("assistant.json");
    let tmp = path.with_file_name(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));
    if let Err(error) = async {
        tokio::fs::write(&tmp, raw).await?;
        tokio::fs::OpenOptions::new()
            .write(true)
            .open(&tmp)
            .await?
            .sync_all()
            .await?;
        tokio::fs::rename(&tmp, &path).await
    }
    .await
    {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(error);
    }
    if let Some(parent) = path.parent() {
        sync_directory(parent).await?;
    }
    Ok(())
}

#[cfg(unix)]
async fn sync_directory(path: &Path) -> io::Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || std::fs::File::open(path)?.sync_all())
        .await
        .map_err(io::Error::other)?
}

#[cfg(not(unix))]
async fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn record(name: &str) -> AssistantRecord {
        AssistantRecord::new(
            "tenant/scout".to_string(),
            name.to_string(),
            "pipeline".to_string(),
            serde_json::json!({"model": "stable"}),
            serde_json::json!({"owner": "quality"}),
            Utc.with_ymd_and_hms(2026, 8, 10, 0, 0, 0).unwrap(),
        )
    }

    #[tokio::test]
    async fn interrupted_temporary_write_never_replaces_the_durable_record() {
        let root = std::env::temp_dir().join(format!(
            "rusty-assistant-atomic-persist-{}",
            uuid::Uuid::new_v4()
        ));
        persist(&root, &record("Original")).await.unwrap();
        let assistant_dir = dir(&root).join("tenant");
        std::fs::write(assistant_dir.join(".scout.json.interrupted.tmp"), b"{").unwrap();

        assert_eq!(load(&root)["tenant/scout"].name, "Original");

        persist(&root, &record("Revised")).await.unwrap();
        assert_eq!(load(&root)["tenant/scout"].name, "Revised");
        let visible: Vec<_> = std::fs::read_dir(assistant_dir)
            .unwrap()
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name.ends_with(".json"))
            .collect();
        assert_eq!(visible, vec!["scout.json"]);

        let _ = std::fs::remove_dir_all(root);
    }
}
