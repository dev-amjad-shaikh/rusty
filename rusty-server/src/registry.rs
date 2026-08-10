//! Configuration-registry persistence (R0.11 Extension Plane, wave 1)
//! and admission resolution (wave 2).
//!
//! One directory under `{store_path}/registry/` (`registry` is a reserved
//! layout name, see [`crate::RESERVED_NAMES`]):
//!
//! - `artifacts/` holds one JSON file per
//!   [`ArtifactRecord`](rusty_agent_runtime::registry::ArtifactRecord).
//!   Artifact keys are surface keys — they contain `:` (`prompt:system`,
//!   tenant-prefixed surfaces), so the file is named by the key's SHA-256
//!   and the file body is an envelope carrying the true key — the version
//!   pointer layout's rule ([`crate::learn`]) applied to the registry's
//!   one new persisted entity. The record is rewritten on every commit,
//!   so the temp-write-then-rename discipline is what makes a crash
//!   mid-commit safe.
//!
//! Postgres keeps the same entity column-mapped
//! (`server_registry_artifacts`), with the commit append compare-and-
//! swapped inside one transaction, so a crash cannot leave a committed
//! candidate whose artifact history never grew (or the inverse).
//!
//! Wave 2 adds no persistence of its own — resolution is a *read* over
//! the wave-1 entities (the environment-tagged version pointer, the
//! candidate record) whose evidence journals into the run. What lives
//! here is the read's contract: [`RegistryRunBinding`], the run
//! payload's declaration of the named artifacts it uses and the
//! environment it targets, and [`resolve_admission`], the pure-over-
//! store composition the run machinery binds with — pointer lookup,
//! canary draw, integrity re-check, manifest pin — so every run
//! endpoint (HTTP, cron, trigger, bridge) resolves identically.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusty_agent_runtime::learn::{
    surface_for_kind, CandidateContent, CandidateKind, EnvironmentTag,
};
use rusty_agent_runtime::record::{sha256_hex, RunManifest};
use rusty_agent_runtime::registry::{
    pointer_admission, resolution_pin, ArtifactRecord, ConfigResolution,
};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::server_store::ServerStore;

/// The artifact directory under the store root
/// (`{store_path}/registry/artifacts`).
pub(crate) fn artifacts_dir(root: &Path) -> PathBuf {
    root.join("registry").join("artifacts")
}

/// The artifact file's body: the record plus the scoped surface key it
/// was written under. The key travels in the body because the filename is
/// the key's hash — surface keys are not path-safe, and a one-way
/// filename needs the true key recorded somewhere (the version-pointer
/// envelope's rule, verbatim).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArtifactFile {
    /// The tenant-scoped surface key (`{tenant}/prompt:system` for named
    /// tenants).
    key: String,
    /// The record itself.
    record: ArtifactRecord,
}

/// The file name for a scoped surface key: its SHA-256 hex — hashing
/// (rather than escaping) keeps every surface inside one fixed-shape,
/// collision-checked namespace; the envelope key check on load is what
/// catches a collision or a forged name.
fn artifact_file_name(scoped_key: &str) -> String {
    sha256_hex(scoped_key.as_bytes())
}

/// Persist one artifact record atomically (temp file + rename), named by
/// the scoped surface key's hash. The record is rewritten on every
/// commit — the most-rewritten file in this layout — so this is the write
/// whose crash-safety the temp+rename discipline buys.
pub(crate) async fn persist_artifact(
    root: &Path,
    scoped_key: &str,
    record: &ArtifactRecord,
) -> io::Result<()> {
    let dir = artifacts_dir(root);
    tokio::fs::create_dir_all(&dir).await?;
    let file = ArtifactFile {
        key: scoped_key.to_string(),
        record: record.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&file)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let name = artifact_file_name(scoped_key);
    let tmp = dir.join(format!("{name}.tmp"));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, dir.join(format!("{name}.json"))).await
}

/// Recursively collect `*.json` files under `root` — the candidate
/// loader's rule, kept local so this module's layout is self-describing.
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

/// Load all artifact records under `artifacts_dir`, keyed by the scoped
/// surface key carried in each file's envelope. A file whose envelope key
/// does not hash back to its filename is corrupt (or a hash collision)
/// and is skipped with a warning, same as an unparseable file: the
/// registry must never serve a record under a key it was not written
/// under — the version-pointer loader's fail-closed rule.
pub(crate) fn load_artifacts(root: &Path) -> HashMap<String, ArtifactRecord> {
    let dir = artifacts_dir(root);
    let mut out = HashMap::new();
    let mut files = Vec::new();
    collect_json_files(&dir, &mut files);
    for path in files {
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<ArtifactFile>(&raw).ok());
        let matches_name = parsed.as_ref().is_some_and(|file| {
            path.file_stem().and_then(|s| s.to_str()) == Some(&*artifact_file_name(&file.key))
        });
        match (parsed, matches_name) {
            (Some(file), true) => {
                out.insert(file.key, file.record);
            }
            _ => {
                tracing::warn!(path = %path.display(), "skipping unreadable artifact file")
            }
        }
    }
    out
}

// --------------------------------------------------------------------- //
// Admission resolution (R0.11 Extension Plane, wave 2)
// --------------------------------------------------------------------- //

/// One artifact a run declares it uses: `{family, name}` — the same
/// address the registry routes speak. Deserialized inside
/// [`RegistryRunBinding`], so an unknown family fails the request's JSON
/// parse (a malformed binding, not a resolution miss).
#[derive(Debug, Clone, Deserialize)]
pub struct RegistryArtifactRef {
    /// The registry family (the candidate kind the artifact indexes).
    pub family: CandidateKind,
    /// The artifact's name within the family.
    pub name: String,
}

/// The run payload's registry declaration (R0.11 wave 2): the named
/// artifacts the run uses, and the environment it targets. Every named
/// artifact resolves at admission through its environment-tagged
/// version pointer, and the resolved content is what the run's manifest
/// pins — so a version promoted *after* the run's admission never
/// reaches it (the conservatism every release since R0.7 has kept), and
/// a version promoted *without redeploying* binds the next run.
#[derive(Debug, Clone, Deserialize)]
pub struct RegistryRunBinding {
    /// The environment tag the run targets. Absent resolves the
    /// deployment's declared default tag
    /// ([`crate::ServerConfig::default_environment_tag`]) — declared
    /// configuration, never an invented per-run guess; when neither
    /// exists, the untagged surface.
    #[serde(default)]
    pub environment: Option<EnvironmentTag>,

    /// The artifacts the run uses. Must be non-empty — a binding naming
    /// no artifacts carries no meaning worth resolving (the empty-scope
    /// refusal manifests already enforce).
    pub artifacts: Vec<RegistryArtifactRef>,
}

/// What [`resolve_admission`] produced: one [`ConfigResolution`] per
/// declared artifact (the journal evidence, in declaration order) and
/// the [`RunManifest`] the run pins (the checkpoint-header evidence).
/// The two are one derivation — each resolution's `digest` is the
/// manifest's pin for its artifact by construction — journaled and
/// stamped by the run machinery.
#[derive(Debug, Clone)]
pub struct RegistryAdmission {
    /// The per-artifact resolutions, in the binding's declaration order.
    pub resolutions: Vec<ConfigResolution>,
    /// The manifest the resolved content pins.
    pub manifest: RunManifest,
}

/// Resolve a run's registry binding against the store: each declared
/// artifact through its environment-tagged [`VersionPointer`](rusty_agent_runtime::learn::VersionPointer)
/// to a candidate (the active version, or the canary when the run's
/// seeded draw admits), and the resolved content into the manifest
/// through the R0.7 pin functions, unchanged.
///
/// Failures are admission failures — the run never starts:
///
/// - `404` when an artifact's pointer does not exist or serves nothing.
///   Registry artifacts have no static fallback (that floor belongs to
///   learned policy); an unpromoted artifact is unresolvable, never an
///   invented default (the capsule-resolve precedent: an unresolvable
///   run stops the request).
/// - `422` when the declaration is malformed (empty, a duplicate
///   artifact, a second `model_settings` — the manifest's `model` slot
///   is singular), when the family has no manifest digest slot
///   ([`resolution_pin`]'s refusal), or when the registry itself reads
///   corrupt: the pointer naming a candidate the store does not hold, a
///   candidate surfaced somewhere else, or a candidate failing its own
///   content address (the capsule registry's integrity rule — tampering
///   is an admission error, never a journaled resolution).
pub(crate) async fn resolve_admission(
    store: &Arc<dyn ServerStore>,
    tenant: &str,
    default_tag: Option<&EnvironmentTag>,
    run_id: &str,
    binding: &RegistryRunBinding,
) -> Result<RegistryAdmission, ApiError> {
    if binding.artifacts.is_empty() {
        return Err(ApiError::unprocessable(
            "registry binding names no artifacts — a binding exists to bind; omit the field \
             to run unbound"
                .to_owned(),
        ));
    }
    let model_settings = binding
        .artifacts
        .iter()
        .filter(|artifact| artifact.family == CandidateKind::ModelSettings)
        .count();
    if model_settings > 1 {
        return Err(ApiError::unprocessable(format!(
            "registry binding names {model_settings} `model_settings` artifacts — the \
             manifest's `model` slot is singular; a run pins one model settings artifact"
        )));
    }
    let mut seen = std::collections::HashSet::new();
    for artifact in &binding.artifacts {
        if !seen.insert((artifact.family, artifact.name.clone())) {
            return Err(ApiError::unprocessable(format!(
                "registry binding names `{}:{}` twice — one artifact binds one version; a \
                 second naming is a configuration error, not a second pin",
                artifact.family.as_str(),
                artifact.name
            )));
        }
    }

    // The run's environment: its own declaration, else the deployment's
    // declared default — never an invented per-run guess.
    let tag = binding.environment.clone().or_else(|| default_tag.cloned());
    let internal = |e: String| ApiError::internal(format!("registry admission read: {e}"));
    let mut resolutions = Vec::with_capacity(binding.artifacts.len());
    let mut manifest = RunManifest::new();
    for artifact in &binding.artifacts {
        let surface = surface_for_kind(artifact.family, &artifact.name);
        let target = match &tag {
            Some(tag) => surface.tagged(tag),
            None => surface.clone(),
        };
        let pointer = store
            .get_version_pointer(tenant, target.as_str())
            .await
            .map_err(internal)?
            .ok_or_else(|| {
                ApiError::not_found(format!(
                    "artifact `{target}` has no version pointer — nothing was ever promoted \
                     for this environment; an unpromoted artifact is unresolvable"
                ))
            })?;
        let (candidate_id, slot) = pointer_admission(&pointer, run_id).ok_or_else(|| {
            ApiError::not_found(format!(
                "artifact `{target}` serves nothing — its pointer has no active version and \
                 this run's draw did not admit a canary"
            ))
        })?;
        let record = store
            .get_candidate(tenant, candidate_id.as_str())
            .await
            .map_err(internal)?
            .ok_or_else(|| {
                ApiError::unprocessable(format!(
                    "artifact `{target}` points at candidate `{candidate_id}`, which the store \
                     does not hold — the registry record is corrupt; re-commit the candidate"
                ))
            })?;
        // The integrity gate, run before anything pins: a candidate
        // failing its own content address — or surfaced somewhere other
        // than the artifact it was resolved for — is tampered evidence,
        // refused here rather than journaled (the capsule registry's
        // re-derivation rule).
        record.candidate.verify_address().map_err(|e| {
            ApiError::unprocessable(format!(
                "artifact `{target}` resolved to a candidate failing its own content address: \
                 {e} — the registry record is corrupt"
            ))
        })?;
        if record.candidate.surface() != surface {
            return Err(ApiError::unprocessable(format!(
                "artifact `{target}` resolved to a candidate surfacing at `{}` — the registry \
                 record is corrupt; the pointer and the candidate disagree",
                record.candidate.surface()
            )));
        }
        let (digest, model) = resolution_pin(&record.candidate)
            .map_err(|e| ApiError::unprocessable(e.to_string()))?;
        // The manifest pin, through the R0.7 functions unchanged — the
        // resolved *content* is what pins, so the journaled digest above
        // and the header's pin are one derivation.
        manifest = match &record.candidate.content {
            CandidateContent::Prompt { name, prompt } => {
                manifest.pin_prompt(name.clone(), prompt.as_str())
            }
            CandidateContent::ToolContract { tool, schema } => {
                manifest.pin_tool_schema(tool.clone(), schema)
            }
            CandidateContent::ModelSettings {
                model, parameters, ..
            } => manifest.pin_model(model.clone(), parameters),
            _ => unreachable!("resolution_pin refused every other kind"),
        };
        resolutions.push(ConfigResolution {
            surface,
            tag: tag.clone(),
            candidate_id,
            pointer: slot,
            digest,
            model,
        });
    }
    Ok(RegistryAdmission {
        resolutions,
        manifest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use rusty_agent_runtime::learn::CandidateKind;
    use rusty_agent_runtime::memory::ProvenanceAuthor;
    use rusty_agent_runtime::registry::ArtifactCommit;

    fn ts(millis: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
    }

    fn artifact() -> ArtifactRecord {
        let mut record = ArtifactRecord::new(
            CandidateKind::Prompt,
            "system",
            ProvenanceAuthor::Human {
                human_id: "amjad".into(),
            },
            ts(1_760_000_000_000),
        )
        .unwrap();
        record.commits.push(ArtifactCommit {
            candidate_id: rusty_agent_runtime::learn::CandidateId::from("a".repeat(64)),
            committed_at: ts(1_760_000_001_000),
        });
        record
    }

    #[tokio::test]
    async fn artifacts_round_trip_through_hashed_filenames() {
        let root =
            std::env::temp_dir().join(format!("rusty-registry-test-{}", uuid::Uuid::new_v4()));
        let artifact = artifact();
        let scoped = format!("acme/{}", artifact.surface.as_str());
        persist_artifact(&root, &scoped, &artifact).await.unwrap();

        // The filename is the key's hash — the raw surface (with its
        // `:`) appears nowhere in the directory listing.
        let listing: Vec<String> = std::fs::read_dir(artifacts_dir(&root))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            listing,
            vec![format!("{}.json", sha256_hex(scoped.as_bytes()))]
        );

        let loaded = load_artifacts(&root);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[&scoped], artifact);

        // An envelope whose key does not hash back to its filename is
        // corrupt (or a collision) and skipped, not served — the
        // version-pointer loader's fail-closed rule.
        std::fs::write(
            artifacts_dir(&root).join(format!("{}.json", sha256_hex(b"forged-name"))),
            serde_json::to_vec_pretty(&ArtifactFile {
                key: "prompt:other".into(),
                record: artifact.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        let loaded = load_artifacts(&root);
        assert_eq!(loaded.len(), 1, "the forged-name file is skipped");
        let _ = std::fs::remove_dir_all(root);
    }
}
