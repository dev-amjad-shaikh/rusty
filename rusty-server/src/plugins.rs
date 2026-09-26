//! Plugins: a distributable package of what the catalog already holds —
//! connector manifests and skill packages — installed as one transaction
//! and refused removal while an agent still names what it brought.
//!
//! A plugin is a directory (or archive) with a `plugin.json` naming its
//! contents by path; every member is parsed, validated and sealed *before*
//! anything registers, so a bad member fails the install whole. What
//! registers is content-addressed like everything else in the catalog: the
//! same manifest is the same record, the same skill text the same revision,
//! so installing twice is harmless and the record says exactly which hashes
//! and revisions this plugin accounts for. Records live one JSON file per
//! plugin under `{store_path}/plugins/`.
//!
//! Two sources: the packs a deployment ships (`ServerConfig::with_plugin_packs`,
//! listed by `GET /plugins/library`) and a URL — a GitHub repository, a folder
//! in one, or a `.tar.gz` — fetched over the deployment's egress policy the
//! way a skill import is, with exactly one `plugin.json` expected under the
//! folder named. The record's `source` says which.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as AxumPath, State as AxumState};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use rusty_agent_runtime::connector::ConnectorManifest;
use rusty_agent_runtime::skill::{SkillPackage, SkillSource};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;

/// What a plugin declares about itself: `plugin.json` at its root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Kebab-case, the record's id.
    pub id: String,
    pub name: String,
    pub version: String,
    pub publisher: String,
    #[serde(default)]
    pub description: String,
    /// Connector manifests, as paths relative to the plugin root.
    #[serde(default)]
    pub connectors: Vec<String>,
    /// Skill directories (holding `SKILL.md`), relative to the plugin root.
    #[serde(default)]
    pub skills: Vec<String>,
    /// The vendor documentation hosts its skills read with `web.fetch` —
    /// what an agent growing the runbooks from the vendors' pages needs
    /// reachable. Named here, allowed by a person: a host is a trust
    /// boundary, and a plugin proposes it, never opens it.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Knowledge the plugin ships: Markdown files, relative to the plugin
    /// root, registered as knowledge sources under the plugin's name so
    /// `search_knowledge` finds them.
    #[serde(default)]
    pub knowledge: Vec<String>,
}

/// One installed plugin: what it is, where it came from, and exactly what
/// it accounts for in the catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRecord {
    pub id: String,
    pub name: String,
    pub version: String,
    pub publisher: String,
    #[serde(default)]
    pub description: String,
    /// `library` for a shipped pack; the repository, ref and path (or the
    /// archive URL) for one installed from a URL.
    pub source: String,
    pub installed_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_by: Option<Value>,
    pub connectors: Vec<InstalledConnector>,
    pub skills: Vec<InstalledSkill>,
    /// The vendor documentation hosts the plugin named; whether the
    /// egress ceiling admits them is read live (`served`).
    #[serde(default)]
    pub hosts: Vec<String>,
    /// The knowledge sources it registered.
    #[serde(default)]
    pub knowledge: Vec<InstalledKnowledge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledKnowledge {
    pub source_id: String,
    pub title: String,
    pub content_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledConnector {
    pub id: String,
    pub display_name: String,
    pub version: String,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledSkill {
    pub name: String,
    pub revision: u64,
}

/// Where plugin records live: one JSON file per installed plugin.
#[derive(Debug)]
pub struct PluginPlane {
    root: PathBuf,
}

impl PluginPlane {
    pub fn new(store_path: &Path) -> Self {
        Self {
            root: store_path.join("plugins"),
        }
    }

    pub async fn persist(&self, scoped_id: &str, record: &PluginRecord) -> Result<(), String> {
        crate::connectors::persist_json(&self.root, scoped_id, record).await
    }

    pub fn load(&self, scoped_id: &str) -> Option<PluginRecord> {
        let bytes = std::fs::read(self.root.join(format!("{scoped_id}.json"))).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn remove(&self, scoped_id: &str) -> bool {
        std::fs::remove_file(self.root.join(format!("{scoped_id}.json"))).is_ok()
    }

    /// Every record with its scoped id, newest install first.
    pub fn list(&self) -> Vec<(String, PluginRecord)> {
        let mut records: Vec<(String, PluginRecord)> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                let scoped = name.strip_suffix(".json")?.to_owned();
                let bytes = std::fs::read(entry.path()).ok()?;
                Some((scoped, serde_json::from_slice(&bytes).ok()?))
            })
            .collect();
        records.sort_by_key(|b| std::cmp::Reverse(b.1.installed_at));
        records
    }
}

/// Everything a plugin brings, parsed and validated, nothing registered yet.
pub(crate) struct Prepared {
    pub manifest: PluginManifest,
    pub connectors: Vec<ConnectorManifest>,
    pub skills: Vec<(String, SkillPackage)>,
    /// Knowledge files: (path, title, body).
    pub knowledge: Vec<(String, String, String)>,
}

/// A knowledge file's title: its first heading, else its file name.
fn knowledge_title(path: &str, body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("# ").map(|t| t.trim().to_owned()))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| {
            path.rsplit('/')
                .next()
                .unwrap_or(path)
                .trim_end_matches(".md")
                .replace('-', " ")
        })
}

/// The source id a plugin's knowledge file registers under.
fn knowledge_source_id(plugin: &str, path: &str) -> String {
    let stem = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .trim_end_matches(".md");
    format!("plugin:{plugin}:{stem}")
}

/// Register the plugin's knowledge files as sources under its name.
async fn register_knowledge(
    state: &AppState,
    tenant: &TenantContext,
    plugin: &str,
    files: &[(String, String, String)],
) -> Result<Vec<InstalledKnowledge>, ApiError> {
    use rusty_agent_runtime::knowledge::{RetentionPolicy, SourceKind, SourceRegistration};
    use rusty_agent_runtime::memory::{MemoryScope, ScopeAddress};
    let base = crate::knowledge::knowledge_base(state, tenant);
    let mut out = Vec::new();
    for (path, title, body) in files {
        let registration = SourceRegistration {
            source_id: knowledge_source_id(plugin, path),
            scope: ScopeAddress::new(MemoryScope::Tenant, tenant.tenant()),
            kind: SourceKind::Markdown,
            title: title.clone(),
            author: format!("plugin:{plugin}"),
            confidence: 0.8,
            retention: RetentionPolicy::Pinned,
            // A plugin ships a vendor's documentation, not the organization's word.
            provenance: rusty_agent_runtime::knowledge::SourceProvenance::Vendor,
        };
        let source = base
            .register_source(registration, body, Utc::now())
            .await
            .map_err(|e| ApiError::bad_request(format!("knowledge `{path}`: {e}")))?;
        out.push(InstalledKnowledge {
            source_id: source.source_id,
            title: title.clone(),
            content_hash: source.content_hash,
        });
    }
    Ok(out)
}

/// Read a plugin out of its files (paths relative to the plugin root). A
/// member that does not parse or validate names itself and fails the whole
/// plugin — nothing half-installs.
pub(crate) fn prepare(files: &BTreeMap<String, Vec<u8>>) -> Result<Prepared, String> {
    let raw = files
        .get("plugin.json")
        .ok_or_else(|| "no plugin.json at the plugin's root".to_owned())?;
    let manifest: PluginManifest =
        serde_json::from_slice(raw).map_err(|e| format!("plugin.json: {e}"))?;
    if manifest.id.is_empty()
        || manifest.id.len() > 64
        || !manifest
            .id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "plugin id `{}` — kebab-case, at most 64 characters",
            manifest.id
        ));
    }
    if manifest.name.trim().is_empty()
        || manifest.version.trim().is_empty()
        || manifest.publisher.trim().is_empty()
    {
        return Err("plugin.json needs a name, a version and a publisher".to_owned());
    }
    if manifest.connectors.is_empty() && manifest.skills.is_empty() {
        return Err(
            "plugin.json names no connectors and no skills — nothing to install".to_owned(),
        );
    }
    let mut connectors = Vec::new();
    for path in &manifest.connectors {
        let bytes = files
            .get(path.trim_matches('/'))
            .ok_or_else(|| format!("connector `{path}` is named but not in the package"))?;
        let parsed: ConnectorManifest =
            serde_json::from_slice(bytes).map_err(|e| format!("connector `{path}`: {e}"))?;
        parsed
            .validate()
            .map_err(|e| format!("connector `{path}`: {e}"))?;
        let sealed = if parsed.hash.is_empty() {
            parsed
                .sealed()
                .map_err(|e| format!("connector `{path}`: {e}"))?
        } else if parsed.verify_hash() {
            parsed
        } else {
            return Err(format!(
                "connector `{path}`: its hash does not match its content"
            ));
        };
        connectors.push(sealed);
    }
    let mut knowledge = Vec::new();
    for path in &manifest.knowledge {
        let key = path.trim_matches('/').to_owned();
        let bytes = files
            .get(&key)
            .ok_or_else(|| format!("knowledge `{path}` is named but not in the package"))?;
        let body = String::from_utf8(bytes.clone())
            .map_err(|_| format!("knowledge `{path}` is not UTF-8 text"))?;
        if body.trim().is_empty() {
            return Err(format!("knowledge `{path}` is empty"));
        }
        knowledge.push((key.clone(), knowledge_title(&key, &body), body));
    }
    let mut skills = Vec::new();
    for dir in &manifest.skills {
        let root = format!("{}/", dir.trim_matches('/'));
        let mut members: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for (path, bytes) in files.range(root.clone()..) {
            if !path.starts_with(&root) {
                break;
            }
            members.insert(path[root.len()..].to_owned(), bytes.clone());
        }
        if !members.contains_key("SKILL.md") {
            return Err(format!("skill `{dir}` has no SKILL.md"));
        }
        let package =
            SkillPackage::from_files(members).map_err(|e| format!("skill `{dir}`: {e}"))?;
        skills.push((dir.clone(), package));
    }
    Ok(Prepared {
        manifest,
        connectors,
        skills,
        knowledge,
    })
}

/// Find the one plugin in an archive's files: the directory around its
/// `plugin.json`, confined to `subpath` when given (matched with and
/// without a forge archive's wrapping top-level directory). None is an
/// error that says so; more than one names them, so the person can name
/// the folder. Returns the plugin's root and its files keyed relative to it.
pub(crate) fn find_plugin(
    files: &BTreeMap<String, Vec<u8>>,
    subpath: Option<&str>,
) -> Result<(String, BTreeMap<String, Vec<u8>>), String> {
    let roots: Vec<String> = files
        .keys()
        .filter(|p| p.as_str() == "plugin.json" || p.ends_with("/plugin.json"))
        .map(|p| p[..p.len() - "plugin.json".len()].to_owned())
        .filter(|root| crate::skills_import::within(root, subpath))
        .collect();
    let root = match roots.as_slice() {
        [] => {
            return Err(match subpath {
                Some(sub) => format!("no plugin.json under `{sub}` in the archive"),
                None => "no plugin.json in the archive — a plugin declares itself at its root"
                    .to_owned(),
            });
        }
        [one] => one.clone(),
        many => {
            let names: Vec<String> = many
                .iter()
                .map(|r| format!("`{}`", r.trim_end_matches('/')))
                .collect();
            return Err(format!(
                "the archive holds {} plugins ({}) — name the folder of the one to install",
                many.len(),
                names.join(", ")
            ));
        }
    };
    let members: BTreeMap<String, Vec<u8>> = files
        .range(root.clone()..)
        .take_while(|(path, _)| path.starts_with(&root))
        .map(|(path, bytes)| (path[root.len()..].to_owned(), bytes.clone()))
        .collect();
    Ok((root.trim_end_matches('/').to_owned(), members))
}

/// Register everything a prepared plugin brings and write its record.
pub(crate) async fn install(
    state: &AppState,
    tenant: &TenantContext,
    prepared: Prepared,
    source: &str,
) -> Result<PluginRecord, ApiError> {
    let mut connectors = Vec::new();
    for manifest in &prepared.connectors {
        state
            .connectors
            .put_manifest(tenant.tenant(), manifest)
            .await
            .map_err(|e| ApiError::internal(format!("connector store: {e}")))?;
        connectors.push(InstalledConnector {
            id: manifest.id.clone(),
            display_name: manifest.display_name.clone(),
            version: manifest.version.clone(),
            hash: manifest.hash.clone(),
        });
    }
    let by = tenant.attribution();
    let author = by
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("plugin")
        .to_owned();
    let mut skills = Vec::new();
    for (dir, package) in prepared.skills {
        let registration = state
            .skills
            .register(
                tenant.tenant(),
                package,
                SkillSource::Registry {
                    name: format!("plugin:{}", prepared.manifest.id),
                },
                author.clone(),
            )
            .await
            .map_err(|e| ApiError::bad_request(format!("skill `{dir}`: {e}")))?;
        skills.push(InstalledSkill {
            name: registration.version.metadata().name.clone(),
            revision: registration.version.revision(),
        });
    }
    let record = PluginRecord {
        id: prepared.manifest.id.clone(),
        name: prepared.manifest.name.clone(),
        version: prepared.manifest.version.clone(),
        publisher: prepared.manifest.publisher.clone(),
        description: prepared.manifest.description.clone(),
        source: source.to_owned(),
        installed_at: Utc::now(),
        installed_by: Some(by),
        connectors,
        skills,
        hosts: prepared
            .manifest
            .hosts
            .iter()
            .filter_map(|h| crate::egress_ceiling::normalize_host(h).ok())
            .collect(),
        knowledge: register_knowledge(state, tenant, &prepared.manifest.id, &prepared.knowledge)
            .await?,
    };
    state
        .plugins
        .persist(&tenant.scope(&record.id), &record)
        .await
        .map_err(|e| ApiError::internal(format!("plugin store: {e}")))?;
    tracing::info!(id = %record.id, connectors = record.connectors.len(), skills = record.skills.len(), "plugin installed");
    Ok(record)
}

/// The packs this deployment ships, each read as a plugin manifest with
/// whether it is installed here.
fn library_entries(state: &AppState, tenant: &TenantContext) -> Vec<Value> {
    state
        .config
        .plugin_packs
        .iter()
        .filter_map(|pack| {
            let raw = pack.files.get("plugin.json")?;
            let manifest: PluginManifest = serde_json::from_slice(raw).ok()?;
            let installed = state.plugins.load(&tenant.scope(&manifest.id)).is_some();
            Some(json!({
                "id": manifest.id,
                "name": manifest.name,
                "version": manifest.version,
                "publisher": manifest.publisher,
                "description": manifest.description,
                "connectors": manifest.connectors.len(),
                "skills": manifest.skills.len(),
                "hosts": manifest.hosts.len(),
                "installed": installed,
            }))
        })
        .collect()
}

/// `GET /plugins/library` — the packs this deployment ships.
pub(crate) async fn list_library(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Json<Value> {
    Json(json!({ "plugins": library_entries(&state, &tenant) }))
}

/// `GET /plugins` — what is installed, newest first.
pub(crate) async fn list_installed(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Json<Value> {
    let records: Vec<PluginRecord> = state
        .plugins
        .list()
        .into_iter()
        .filter(|(scoped, _)| tenant.unscope(scoped).is_some())
        .map(|(_, record)| record)
        .collect();
    let served: Vec<Value> = records.iter().map(|r| served(&state, r)).collect();
    Json(json!({ "plugins": served }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct InstallPayload {
    /// The id of a pack in the library.
    #[serde(default)]
    library: Option<String>,
    /// A GitHub repository, a folder in one, or a `.tar.gz`.
    #[serde(default)]
    url: Option<String>,
    /// A git ref for a repository URL; `HEAD` when absent.
    #[serde(default, rename = "ref")]
    git_ref: Option<String>,
    /// The folder inside the archive holding the plugin.
    #[serde(default)]
    subpath: Option<String>,
}

/// `POST /plugins/install {library}` or `{url, ref?, subpath?}` — install a
/// plugin whole or not at all: from a shipped pack, or from an archive
/// fetched over the egress policy. Either way, every member is parsed,
/// validated and sealed before anything registers.
pub(crate) async fn install_plugin(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<InstallPayload>,
) -> Result<(StatusCode, Json<PluginRecord>), ApiError> {
    let library = payload
        .library
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let url = payload
        .url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let (prepared, source) = match (library, url) {
        (Some(id), None) => {
            let pack = state
                .config
                .plugin_packs
                .iter()
                .find(|pack| {
                    pack.files
                        .get("plugin.json")
                        .and_then(|raw| serde_json::from_slice::<PluginManifest>(raw).ok())
                        .is_some_and(|m| m.id == id)
                })
                .ok_or_else(|| ApiError::not_found(format!("no pack `{id}` in the library")))?;
            (
                prepare(&pack.files).map_err(ApiError::bad_request)?,
                "library".to_owned(),
            )
        }
        (None, Some(url)) => {
            let archive = crate::skills_import::archive_for(
                url,
                payload.git_ref.as_deref(),
                payload.subpath.as_deref(),
            )
            .map_err(ApiError::bad_request)?;
            let body =
                crate::skills_import::fetch_archive(&state, &archive, "plugins-install").await?;
            let subpath = archive.subpath.clone();
            let (root, files) = tokio::task::spawn_blocking(move || {
                let unpacked = crate::skills_import::unpack_files(&body)?;
                find_plugin(&unpacked.files, subpath.as_deref())
            })
            .await
            .map_err(|e| ApiError::internal(format!("unpack: {e}")))?
            .map_err(ApiError::bad_request)?;
            let prepared = prepare(&files).map_err(|e| {
                ApiError::bad_request(format!(
                    "{}: {e}",
                    if root.is_empty() {
                        archive.display.clone()
                    } else {
                        format!("{}/{root}", archive.display)
                    }
                ))
            })?;
            (prepared, archive.display)
        }
        _ => {
            return Err(ApiError::bad_request(
                "name either a pack in the library or a URL to install from".to_owned(),
            ));
        }
    };
    let id = prepared.manifest.id.clone();
    if state.plugins.load(&tenant.scope(&id)).is_some() {
        return Err(ApiError::conflict(format!(
            "`{id}` is already installed — remove it first to install it again"
        )));
    }
    let record = install(&state, &tenant, prepared, &source).await?;
    Ok((StatusCode::CREATED, Json(record)))
}

/// `DELETE /plugins/{id}` — refused while an agent names a tool or skill the
/// plugin brought, or a connection runs on one of its connectors. Otherwise
/// the record goes; what it registered stays in the library and the skill
/// registry, content-addressed and unattributed, and the reply says so.
pub(crate) async fn uninstall(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let scoped = tenant.scope(&id);
    let record = state
        .plugins
        .load(&scoped)
        .ok_or_else(|| ApiError::not_found(format!("plugin `{id}` is not installed")))?;
    let prefixes: Vec<String> = record
        .connectors
        .iter()
        .map(|c| format!("{}.", c.id))
        .collect();
    let skill_names: Vec<&str> = record.skills.iter().map(|s| s.name.as_str()).collect();
    let hashes: Vec<&str> = record.connectors.iter().map(|c| c.hash.as_str()).collect();

    let mut in_use: Vec<String> = Vec::new();
    for assistant in state
        .server_store
        .list_assistants()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
    {
        // Another tenant's agents are not this tenant's, and an archived
        // agent holds nothing.
        if tenant.unscope(&assistant.assistant_id).is_none() || assistant.archived_at.is_some() {
            continue;
        }
        let intent = assistant.config.get("studio_intent");
        let tools: Vec<&str> = intent
            .and_then(|i| i.get("tools"))
            .and_then(Value::as_array)
            .map(|t| {
                t.iter()
                    .filter_map(|x| x.get("name").and_then(Value::as_str))
                    .collect()
            })
            .unwrap_or_default();
        let skills: Vec<&str> = intent
            .and_then(|i| i.get("skills"))
            .and_then(Value::as_array)
            .map(|s| s.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let uses_tool = tools
            .iter()
            .any(|t| prefixes.iter().any(|p| t.starts_with(p)));
        let uses_skill = skills.iter().any(|s| skill_names.contains(s));
        if uses_tool || uses_skill {
            in_use.push(format!("agent {}", assistant.name));
        }
    }
    for instance in state
        .connectors
        .list_instances(tenant.tenant())
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
    {
        if hashes.contains(&instance.manifest_hash.as_str()) {
            in_use.push(format!("connection {}", instance.instance_id));
        }
    }
    if !in_use.is_empty() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "plugin_in_use",
            format!(
                "`{}` is still used by {} — retire those first",
                record.name,
                in_use.join(", ")
            ),
        ));
    }
    state.plugins.remove(&scoped);
    tracing::info!(id = %id, "plugin removed");
    Ok(Json(json!({
        "removed": id,
        "note": "its connectors stay in the library and its skills in the registry — content-addressed, no longer attributed to this plugin",
    })))
}

/// The vendor hosts a plugin names: on its record, or — for a library
/// pack installed before records carried them — on the pack it came from.
pub(crate) fn hosts_of(state: &AppState, record: &PluginRecord) -> Vec<String> {
    if !record.hosts.is_empty() || record.source != "library" {
        return record.hosts.clone();
    }
    state
        .config
        .plugin_packs
        .iter()
        .filter_map(|pack| {
            serde_json::from_slice::<PluginManifest>(pack.files.get("plugin.json")?).ok()
        })
        .find(|m| m.id == record.id)
        .map(|m| {
            m.hosts
                .iter()
                .filter_map(|h| crate::egress_ceiling::normalize_host(h).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// A plugin record as served: with its vendor hosts and which of them the
/// egress ceiling does not admit yet, so the page can say so and offer to
/// allow them.
pub(crate) fn served(state: &AppState, record: &PluginRecord) -> Value {
    let ceiling = crate::egress_ceiling::current(state);
    let hosts = hosts_of(state, record);
    let outside: Vec<&str> = if ceiling.open {
        Vec::new()
    } else {
        hosts
            .iter()
            .map(String::as_str)
            .filter(|h| !ceiling.fits(h))
            .collect()
    };
    let mut value = serde_json::to_value(record).unwrap_or(Value::Null);
    value["hosts"] = json!(hosts);
    value["hosts_outside"] = json!(outside);
    value["ceiling_open"] = json!(ceiling.open);
    // Knowledge the pack ships that this record has not registered — an
    // older library install, or a pack that gained files since.
    value["knowledge_shipped"] = json!(shipped_knowledge(state, record).len());
    value
}

/// The knowledge files a library pack ships for this plugin.
fn shipped_knowledge(state: &AppState, record: &PluginRecord) -> Vec<(String, String, String)> {
    if record.source != "library" {
        return Vec::new();
    }
    state
        .config
        .plugin_packs
        .iter()
        .find(|pack| {
            pack.files
                .get("plugin.json")
                .and_then(|raw| serde_json::from_slice::<PluginManifest>(raw).ok())
                .is_some_and(|m| m.id == record.id)
        })
        .and_then(|pack| prepare(&pack.files).ok())
        .map(|p| p.knowledge)
        .unwrap_or_default()
}

/// `POST /plugins/{id}/knowledge/load` — register the knowledge a library
/// pack ships for an installed plugin: for a record made before plugins
/// carried knowledge, or a pack that gained files. Content-addressed, so
/// loading again registers nothing new.
pub(crate) async fn load_knowledge(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    axum::extract::Path(plugin_id): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiError> {
    let scoped = tenant.scope(&plugin_id);
    let mut record = state
        .plugins
        .load(&scoped)
        .ok_or_else(|| ApiError::not_found(format!("plugin `{plugin_id}` is not installed")))?;
    let files = shipped_knowledge(&state, &record);
    if files.is_empty() {
        return Ok(Json(
            json!({"loaded": [], "note": "this plugin ships no knowledge", "plugin": served(&state, &record)}),
        ));
    }
    let loaded = register_knowledge(&state, &tenant, &record.id, &files).await?;
    record.knowledge = loaded.clone();
    state
        .plugins
        .persist(&scoped, &record)
        .await
        .map_err(|e| ApiError::internal(format!("plugin store: {e}")))?;
    Ok(Json(
        json!({"loaded": loaded, "plugin": served(&state, &record)}),
    ))
}

/// `POST /plugins/{id}/hosts/allow` — a person admits the plugin's vendor
/// hosts to the egress ceiling, each noted with the plugin that asked.
/// The ceiling's own rules hold (a closed ceiling stays closed; nothing
/// below a connection in use). Open ceiling: nothing to admit.
pub(crate) async fn allow_hosts(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    axum::extract::Path(plugin_id): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiError> {
    let record = state
        .plugins
        .load(&tenant.scope(&plugin_id))
        .ok_or_else(|| ApiError::not_found(format!("plugin `{plugin_id}` is not installed")))?;
    let before = crate::egress_ceiling::current(&state);
    if before.open {
        return Ok(Json(
            json!({"allowed": [], "note": "the egress ceiling is open: every host is reachable already", "plugin": served(&state, &record)}),
        ));
    }
    let now = Utc::now();
    let by = tenant.attribution();
    let mut hosts = before.hosts.clone();
    let mut allowed = Vec::new();
    for host in &hosts_of(&state, &record) {
        if hosts.iter().any(|h| h.host == *host) || before.fits(host) {
            continue;
        }
        hosts.push(crate::egress_ceiling::CeilingHost {
            host: host.clone(),
            added_by: by.clone(),
            added_at: now,
            note: Some(format!("plugin:{}", record.id)),
        });
        allowed.push(host.clone());
    }
    let ceiling = crate::egress_ceiling::EgressCeiling {
        open: false,
        hosts,
        updated_by: Some(by),
        updated_at: Some(now),
    };
    state
        .server_store
        .put_egress_ceiling(&ceiling)
        .await
        .map_err(ApiError::internal)?;
    crate::egress_ceiling::set(&state, ceiling);
    crate::connectors::refresh_connection_tools(&state).await;
    tracing::info!(plugin = %record.id, allowed = allowed.len(), "plugin hosts admitted to the egress ceiling");
    Ok(Json(
        json!({"allowed": allowed, "plugin": served(&state, &record)}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(paths: &[&str]) -> BTreeMap<String, Vec<u8>> {
        paths
            .iter()
            .map(|p| (p.to_string(), b"{}".to_vec()))
            .collect()
    }

    #[test]
    fn the_one_plugin_is_found_under_a_forge_wrapper_directory() {
        let all = files(&[
            "repo-abc123/README.md",
            "repo-abc123/plugin.json",
            "repo-abc123/connectors/cat-facts.json",
            "repo-abc123/skills/cite-the-source/SKILL.md",
        ]);
        let (root, members) = find_plugin(&all, None).unwrap();
        assert_eq!(root, "repo-abc123");
        let keys: Vec<&String> = members.keys().collect();
        assert_eq!(
            keys,
            vec![
                "README.md",
                "connectors/cat-facts.json",
                "plugin.json",
                "skills/cite-the-source/SKILL.md"
            ]
        );
    }

    #[test]
    fn a_plugin_at_the_archive_root_has_no_prefix() {
        let all = files(&["plugin.json", "connectors/x.json"]);
        let (root, members) = find_plugin(&all, None).unwrap();
        assert_eq!(root, "");
        assert!(members.contains_key("plugin.json"));
        assert!(members.contains_key("connectors/x.json"));
    }

    #[test]
    fn none_says_so_and_many_ask_for_the_folder() {
        let error = find_plugin(&files(&["repo-abc/README.md"]), None).unwrap_err();
        assert!(error.contains("no plugin.json"), "{error}");

        let two = files(&["repo-abc/a/plugin.json", "repo-abc/b/plugin.json"]);
        let error = find_plugin(&two, None).unwrap_err();
        assert!(
            error.contains("2 plugins")
                && error.contains("`repo-abc/a`")
                && error.contains("name the folder"),
            "{error}"
        );

        // The folder picks one, matched past the wrapper directory.
        let (root, _) = find_plugin(&two, Some("b")).unwrap();
        assert_eq!(root, "repo-abc/b");
        let error = find_plugin(&two, Some("c")).unwrap_err();
        assert!(error.contains("under `c`"), "{error}");
    }

    #[test]
    fn a_found_plugin_prepares_like_a_shipped_pack() {
        // The shipped Cat Facts starter, as a forge tarball would lay it out.
        let mut all: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        all.insert(
            "pack-1234/plugin.json".into(),
            include_bytes!("../../catalog/plugins/cat-facts-starter/plugin.json").to_vec(),
        );
        all.insert(
            "pack-1234/connectors/cat-facts.json".into(),
            include_bytes!("../../catalog/plugins/cat-facts-starter/connectors/cat-facts.json")
                .to_vec(),
        );
        all.insert(
            "pack-1234/skills/cite-the-source/SKILL.md".into(),
            include_bytes!(
                "../../catalog/plugins/cat-facts-starter/skills/cite-the-source/SKILL.md"
            )
            .to_vec(),
        );
        let (root, members) = find_plugin(&all, None).unwrap();
        assert_eq!(root, "pack-1234");
        let prepared = prepare(&members).unwrap();
        assert_eq!(prepared.manifest.id, "cat-facts-starter");
        assert_eq!(prepared.connectors.len(), 1);
        assert!(!prepared.connectors[0].hash.is_empty(), "sealed");
        assert_eq!(prepared.skills.len(), 1);
    }
}
