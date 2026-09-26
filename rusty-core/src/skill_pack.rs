//! Governed skill packs: the EP-15-S08 slice of the catalog.
//!
//! A skill pack is a catalog package (`PackageKind::SkillPack`) whose payload
//! is one or more conformant [`SkillPackage`]s plus a bundled eval suite per
//! skill. This module owns the pack's load-time shape and the install-time
//! lifecycle:
//!
//! - **Declared dependencies.** Each skill names what it needs — tools,
//!   connectors (by id and major version), gateway capabilities, and the
//!   reference documents it embeds — in `pack.json`. Invalidation is
//!   mechanical because dependencies are declared, not discovered.
//! - **Install gating.** Installing a pack registers each skill into the
//!   tenant's [`SkillRegistry`] as `Trial`, runs the bundled eval suite
//!   through a caller-supplied [`SkillGateRunner`], and promotes only on a
//!   pass. A failing suite leaves the skill `Trial` with the failing cases
//!   named — never a silent promotion.
//! - **Invalidation and revalidation.** A [`DependencyChange`] flags every
//!   affected skill `revalidation-pending` and re-runs its gate; a failure
//!   demotes to `Trial` with the triggering change named in the ledger.
//! - **Three-way updates.** A package update to a locally-patched skill
//!   surfaces shipped-old / shipped-new / local for a human to resolve; the
//!   update never silently overwrites local improvements.
//!
//! The ledger is append-only: every mutation by every actor lands as a
//! [`SkillPackLedgerEntry`], and provenance on every registered version cites
//! the shipping package (publisher, version, content hash) via
//! [`SkillSource::Package`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Result, RustyError};
use crate::package::{
    CapabilityDecl, FileEntry, PackageId, PackageKind, PackageManifest, PublisherId, Version,
};
use crate::skill::{SkillPackage, SkillPromotionStatus, SkillRegistry, SkillSource};

fn pack_err(msg: impl Into<String>) -> RustyError {
    RustyError::Catalog(format!("skill pack: {}", msg.into()))
}

// ---------------------------------------------------------------------------
// Declared dependencies
// ---------------------------------------------------------------------------

/// A gateway capability a skill's behavior contract relies on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayCapability {
    /// The gateway's scheduled autonomy (scheduled-digest fires on a cadence).
    ScheduledAutonomy,
    /// The structured-input obligation path (form-filling raises
    /// `StructuredInput` obligations for ungroundable fields).
    StructuredInput,
}

/// One declared dependency of a shipped skill. Closed enum: a dependency the
/// platform cannot name is a dependency it cannot invalidate mechanically.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SkillDependency {
    /// A tool the skill's instructions call.
    Tool {
        /// The tool's registry name.
        name: String,
    },
    /// A connector the skill reads or acts through.
    Connector {
        /// The connector's package id.
        id: String,
        /// The major version the skill was authored against.
        major: u64,
    },
    /// A gateway capability the skill's contract assumes.
    Gateway {
        /// The capability.
        capability: GatewayCapability,
    },
}

/// One skill inside a pack: its payload directory, its declared
/// dependencies, and the reference documents it embeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillPackEntry {
    /// Directory holding the skill payload (`SKILL.md` plus `references/`
    /// and `assets/` subtrees), relative to the pack root. `"."` for a
    /// single-skill pack rooted at the package root.
    pub dir: String,
    /// The tool, connector, and gateway dependencies the skill requires.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<SkillDependency>,
    /// Reference documents this skill embeds (payload-relative paths).
    /// Entries here are what reference-supersession invalidation watches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reference_docs: Vec<String>,
}

// ---------------------------------------------------------------------------
// The pack: manifest + payload + eval bundle
// ---------------------------------------------------------------------------

/// The wire shape of `pack.json` at the pack root.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PackFile {
    id: String,
    name: String,
    version: String,
    publisher: String,
    skills: Vec<SkillPackEntry>,
}

/// The eval suite bundled with one skill: the dataset, the gate policy, and
/// the recorded fixtures the suite runs against in catalog CI and at install
/// time. Kept as raw text so this crate stays independent of the eval crate —
/// the caller's [`SkillGateRunner`] parses and runs them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalBundle {
    /// The eval dataset, JSONL (`rusty-eval` `Dataset::from_jsonl` shape).
    pub dataset_jsonl: String,
    /// The gate policy, JSON (`rusty-eval` `GatePolicy::from_json` shape).
    pub gate_json: String,
    /// Recorded run evidence per case id (JSON-serialized `RunEvidence`).
    pub fixtures: BTreeMap<String, String>,
}

/// One loaded skill: its manifest entry, its validated payload, and its
/// bundled eval suite.
#[derive(Debug, Clone)]
pub struct LoadedSkill {
    /// The manifest entry (dependencies, reference docs).
    pub entry: SkillPackEntry,
    /// The validated, content-addressed payload.
    pub package: SkillPackage,
    /// The bundled eval suite.
    pub eval: EvalBundle,
}

impl LoadedSkill {
    /// The skill's registry name (frontmatter `name`).
    pub fn name(&self) -> &str {
        self.package.name()
    }
}

/// A loaded skill pack: the content-addressed package manifest plus every
/// skill's payload and eval bundle.
#[derive(Debug, Clone)]
pub struct SkillPack {
    /// The S01 package manifest. The file manifest covers every byte in the
    /// pack directory — payload, `pack.json`, and eval suite — so the
    /// package's content hash is the identity of the whole pack.
    pub manifest: PackageManifest,
    /// The skills in the pack, in manifest order.
    pub skills: Vec<LoadedSkill>,
}

impl SkillPack {
    /// Load a pack from a directory on disk.
    ///
    /// Fails closed: an unreadable member, an invalid `pack.json`, a payload
    /// that violates the closed [`SkillPackage`] shape, a missing eval suite,
    /// or a frontmatter `name` that disagrees with the manifest entry are all
    /// load errors — a pack that loads is internally consistent.
    pub fn from_dir(root: &Path) -> Result<Self> {
        let io = |what: &str, error: std::io::Error| {
            pack_err(format!("pack `{}`: {what}: {error}", root.display()))
        };
        let pack_text = std::fs::read_to_string(root.join("pack.json"))
            .map_err(|error| io("reading pack.json", error))?;
        let pack_file: PackFile = serde_json::from_str(&pack_text).map_err(|error| {
            pack_err(format!(
                "pack `{}`: parsing pack.json: {error}",
                root.display()
            ))
        })?;
        if pack_file.skills.is_empty() {
            return Err(pack_err(format!(
                "pack `{}`: declares no skills",
                root.display()
            )));
        }

        let id = PackageId::new(&pack_file.id)?;
        let version = Version::parse(&pack_file.version)?;
        let publisher = PublisherId::new(&pack_file.publisher)?;

        let mut skills = Vec::with_capacity(pack_file.skills.len());
        for entry in &pack_file.skills {
            skills.push(load_skill(root, entry)?);
        }

        // The package file manifest covers every regular file in the pack
        // directory — payload, pack.json, and eval suite alike.
        let mut files = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            let mut entries: Vec<_> = std::fs::read_dir(&dir)
                .map_err(|error| io("reading directory", error))?
                .collect::<std::result::Result<_, _>>()
                .map_err(|error| io("reading directory", error))?;
            entries.sort_by_key(|e| e.file_name());
            for dir_entry in entries {
                let path = dir_entry.path();
                let metadata = std::fs::symlink_metadata(&path)
                    .map_err(|error| io("reading member metadata", error))?;
                if metadata.file_type().is_symlink() {
                    return Err(pack_err(format!(
                        "pack `{}`: symlinks are not permitted (`{}`)",
                        root.display(),
                        path.display()
                    )));
                }
                if metadata.is_dir() {
                    pending.push(path);
                    continue;
                }
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| pack_err("pack member escapes the pack root"))?
                    .components()
                    .map(|component| {
                        component
                            .as_os_str()
                            .to_str()
                            .ok_or_else(|| pack_err("pack member names must be valid UTF-8"))
                    })
                    .collect::<Result<Vec<_>>>()?
                    .join("/");
                let bytes = std::fs::read(&path).map_err(|error| io("reading member", error))?;
                files.push(FileEntry {
                    path: relative,
                    sha256: crate::record::sha256_hex(&bytes),
                    bytes: bytes.len() as u64,
                });
            }
        }

        let manifest = PackageManifest::new(
            id,
            &pack_file.name,
            PackageKind::SkillPack,
            version,
            publisher,
            files,
            Vec::new(),
            CapabilityDecl::default(),
            None,
            None,
        )?;
        debug_assert!(manifest.verify_hash());
        Ok(Self { manifest, skills })
    }
}

/// Load one skill entry: payload, reference-doc presence, and eval bundle.
fn load_skill(root: &Path, entry: &SkillPackEntry) -> Result<LoadedSkill> {
    let dir = root.join(&entry.dir);
    let read = |relative: &str| -> Result<Vec<u8>> {
        std::fs::read(dir.join(relative)).map_err(|error| {
            pack_err(format!(
                "skill dir `{}`: reading `{relative}`: {error}",
                dir.display()
            ))
        })
    };

    let mut payload = BTreeMap::new();
    payload.insert("SKILL.md".to_owned(), read("SKILL.md")?);
    for subtree in ["references", "assets"] {
        let subtree_root = dir.join(subtree);
        if !subtree_root.is_dir() {
            continue;
        }
        let mut pending = vec![subtree_root.clone()];
        while let Some(current) = pending.pop() {
            let mut entries: Vec<_> = std::fs::read_dir(&current)
                .map_err(|error| {
                    pack_err(format!(
                        "skill dir `{}`: reading `{subtree}/`: {error}",
                        dir.display()
                    ))
                })?
                .collect::<std::result::Result<_, _>>()
                .map_err(|error| pack_err(format!("reading directory: {error}")))?;
            entries.sort_by_key(|e| e.file_name());
            for dir_entry in entries {
                let path = dir_entry.path();
                if path.is_dir() {
                    pending.push(path);
                    continue;
                }
                let relative = path
                    .strip_prefix(&dir)
                    .map_err(|_| pack_err("skill member escapes the skill dir"))?
                    .components()
                    .map(|component| {
                        component
                            .as_os_str()
                            .to_str()
                            .ok_or_else(|| pack_err("skill member names must be valid UTF-8"))
                    })
                    .collect::<Result<Vec<_>>>()?
                    .join("/");
                payload.insert(
                    relative,
                    std::fs::read(&path).map_err(|error| {
                        pack_err(format!("reading `{}`: {error}", path.display()))
                    })?,
                );
            }
        }
    }
    let package = SkillPackage::from_files(payload)
        .map_err(|error| pack_err(format!("skill payload: {error}")))?;

    for doc in &entry.reference_docs {
        if !package.references().contains_key(doc) {
            return Err(pack_err(format!(
                "skill `{}`: declared reference doc `{doc}` is not in the payload",
                package.name()
            )));
        }
    }

    let dataset_jsonl = String::from_utf8(read("eval/dataset.jsonl")?).map_err(|_| {
        pack_err(format!(
            "skill `{}`: eval dataset is not UTF-8",
            package.name()
        ))
    })?;
    let gate_json = String::from_utf8(read("eval/gate.json")?).map_err(|_| {
        pack_err(format!(
            "skill `{}`: eval gate is not UTF-8",
            package.name()
        ))
    })?;
    let mut fixtures = BTreeMap::new();
    let fixtures_root = dir.join("eval/fixtures");
    if fixtures_root.is_dir() {
        for dir_entry in std::fs::read_dir(&fixtures_root)
            .map_err(|error| pack_err(format!("reading eval fixtures: {error}")))?
        {
            let dir_entry =
                dir_entry.map_err(|error| pack_err(format!("reading eval fixtures: {error}")))?;
            let path = dir_entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let case_id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or_else(|| pack_err("eval fixture names must be valid UTF-8"))?
                .to_owned();
            let text = std::fs::read_to_string(&path)
                .map_err(|error| pack_err(format!("reading fixture `{case_id}`: {error}")))?;
            fixtures.insert(case_id, text);
        }
    }

    Ok(LoadedSkill {
        entry: entry.clone(),
        package,
        eval: EvalBundle {
            dataset_jsonl,
            gate_json,
            fixtures,
        },
    })
}

// ---------------------------------------------------------------------------
// The dependency environment and changes to it
// ---------------------------------------------------------------------------

/// What the installing tenant currently has: the registry-visible tools, the
/// installed connectors with their major versions, and the gateway
/// capabilities wired into this deployment.
#[derive(Debug, Clone, Default)]
pub struct DependencyEnvironment {
    /// Tool names present in the tenant's tool registries.
    pub tools: BTreeSet<String>,
    /// Connector id → installed major version.
    pub connectors: BTreeMap<String, u64>,
    /// Gateway capabilities wired into the deployment.
    pub gateway_capabilities: BTreeSet<GatewayCapability>,
}

impl DependencyEnvironment {
    /// Every dependency of `entry` the environment does not satisfy.
    pub fn missing(&self, entry: &SkillPackEntry) -> Vec<SkillDependency> {
        entry
            .dependencies
            .iter()
            .filter(|dependency| match dependency {
                SkillDependency::Tool { name } => !self.tools.contains(name),
                SkillDependency::Connector { id, major } => self.connectors.get(id) != Some(major),
                SkillDependency::Gateway { capability } => {
                    !self.gateway_capabilities.contains(capability)
                }
            })
            .cloned()
            .collect()
    }
}

/// A change to a tenant's dependency environment. Each variant names the
/// dependency kind it affects; matching a skill's declared dependencies to a
/// change is mechanical.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DependencyChange {
    /// A connector the skill depends on was removed.
    ConnectorRemoved {
        /// The connector's package id.
        id: String,
    },
    /// A connector's major version changed.
    ConnectorMajorChanged {
        /// The connector's package id.
        id: String,
        /// The newly installed major version.
        new_major: u64,
    },
    /// A tool disappeared from the tenant's registries.
    ToolRemoved {
        /// The tool's registry name.
        name: String,
    },
    /// A gateway capability was unwired from the deployment.
    GatewayCapabilityRemoved {
        /// The capability.
        capability: GatewayCapability,
    },
    /// A newer package version supersedes an embedded reference document.
    ReferenceSuperseded {
        /// The superseding package's id.
        package_id: String,
        /// The payload-relative reference path that changed.
        path: String,
    },
}

impl DependencyChange {
    /// `true` when this change touches one of the entry's declared
    /// dependencies or embedded reference documents.
    pub fn affects(&self, entry: &SkillPackEntry, package_id: &str) -> bool {
        match self {
            DependencyChange::ConnectorRemoved { id } => entry
                .dependencies
                .iter()
                .any(|dep| matches!(dep, SkillDependency::Connector { id: dep_id, .. } if dep_id == id)),
            DependencyChange::ConnectorMajorChanged { id, new_major } => entry
                .dependencies
                .iter()
                .any(|dep| matches!(dep, SkillDependency::Connector { id: dep_id, major } if dep_id == id && major != new_major)),
            DependencyChange::ToolRemoved { name } => entry
                .dependencies
                .iter()
                .any(|dep| matches!(dep, SkillDependency::Tool { name: dep_name } if dep_name == name)),
            DependencyChange::GatewayCapabilityRemoved { capability } => entry
                .dependencies
                .iter()
                .any(|dep| matches!(dep, SkillDependency::Gateway { capability: dep_cap } if dep_cap == capability)),
            DependencyChange::ReferenceSuperseded { package_id: pid, path } => {
                pid == package_id && entry.reference_docs.iter().any(|doc| doc == path)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The gate runner seam
// ---------------------------------------------------------------------------

/// The outcome of running one skill's bundled eval suite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateRunOutcome {
    /// The gate run id, recorded on the ledger entry and the promotion.
    pub run_id: String,
    /// `true` when the suite's gate allowed the candidate.
    pub passed: bool,
    /// The ids of the dataset cases that failed, named — a failing suite
    /// never reports a bare `false`.
    pub failing_cases: Vec<String>,
}

/// Runs a skill's bundled eval suite. Implemented by the caller (catalog CI,
/// the install pipeline) over `rusty-eval`; this crate defines the seam so
/// the lifecycle here stays testable and the eval engine stays swappable.
pub trait SkillGateRunner {
    /// Run `skill`'s bundled suite and report the gate verdict.
    fn run_gate(&self, skill: &LoadedSkill) -> Result<GateRunOutcome>;
}

// ---------------------------------------------------------------------------
// The ledger
// ---------------------------------------------------------------------------

/// One mutation in a skill's pack ledger. Closed enum; every variant carries
/// the evidence an auditor needs to reconstruct what happened and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SkillPackMutation {
    /// The pack installed the skill (enters as `Trial`).
    Install {
        /// The installed payload's content hash.
        content_hash: String,
    },
    /// The bundled eval suite gated the skill to `Promoted`.
    GatePassed {
        /// The gate run id.
        run_id: String,
    },
    /// The bundled eval suite failed; the skill stays `Trial`.
    GateFailed {
        /// The gate run id.
        run_id: String,
        /// The failing dataset cases, named.
        failing_cases: Vec<String>,
    },
    /// A dependency change flagged the skill revalidation-pending.
    Invalidated {
        /// The triggering change.
        trigger: DependencyChange,
    },
    /// Revalidation re-ran the gate and the skill held its state.
    Revalidated {
        /// The gate run id.
        run_id: String,
    },
    /// Revalidation failed; the skill demoted to `Trial`.
    Demoted {
        /// The gate run id.
        run_id: String,
        /// The dependency change that triggered revalidation.
        trigger: DependencyChange,
        /// The failing dataset cases, named.
        failing_cases: Vec<String>,
    },
    /// A tenant learning loop patched the installed skill.
    LocallyPatched {
        /// The patched payload's content hash.
        content_hash: String,
    },
    /// A package update arrived for a locally-patched skill: surfaced for
    /// human resolution, never silently applied.
    ThreeWayUpdate {
        /// The content hash the tenant installed from the old package.
        shipped_old: String,
        /// The content hash the new package ships.
        shipped_new: String,
        /// The locally-patched content hash.
        local: String,
    },
    /// A package update landed on an unpatched skill.
    Updated {
        /// The updated payload's content hash.
        content_hash: String,
    },
}

/// One append-only ledger row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillPackLedgerEntry {
    /// The 1-based position in this skill's ledger.
    pub seq: u64,
    /// Who performed the mutation (operator, install pipeline, curator).
    pub actor: String,
    /// What happened.
    pub mutation: SkillPackMutation,
    /// When the entry was recorded.
    pub recorded_at: DateTime<Utc>,
}

/// The installed state of one shipped skill in one tenant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledSkill {
    /// The skill's registry name.
    pub skill_name: String,
    /// The shipping package's id.
    pub package_id: String,
    /// The shipping package's publisher.
    pub publisher: String,
    /// The shipping package's version.
    pub package_version: String,
    /// The payload hash the shipping package carried at install or last
    /// applied update — the "shipped-old" side of a three-way update.
    pub shipped_content_hash: String,
    /// The tenant's current payload hash. Equals `shipped_content_hash`
    /// until a learning loop patches the skill.
    pub content_hash: String,
    /// The promotion state (`Trial` on install, `Promoted` only through the
    /// gate).
    pub status: SkillPromotionStatus,
    /// The dependency change awaiting revalidation, when flagged.
    pub revalidation_pending: Option<DependencyChange>,
    /// `true` once a tenant learning loop has patched the installed payload.
    pub locally_patched: bool,
    /// The append-only ledger.
    pub ledger: Vec<SkillPackLedgerEntry>,
}

impl InstalledSkill {
    fn record(&mut self, actor: &str, mutation: SkillPackMutation) {
        let seq = self.ledger.len() as u64 + 1;
        self.ledger.push(SkillPackLedgerEntry {
            seq,
            actor: actor.to_owned(),
            mutation,
            recorded_at: Utc::now(),
        });
    }
}

/// The per-tenant ledger of installed skill-pack skills, keyed by skill name.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SkillPackLedger {
    skills: BTreeMap<String, InstalledSkill>,
}

impl SkillPackLedger {
    /// An empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// The installed record for a skill, when present.
    pub fn get(&self, skill_name: &str) -> Option<&InstalledSkill> {
        self.skills.get(skill_name)
    }

    /// A mutable handle to the installed record for a skill, when present.
    pub fn get_mut(&mut self, skill_name: &str) -> Option<&mut InstalledSkill> {
        self.skills.get_mut(skill_name)
    }

    /// Every installed skill, name-sorted (deterministic iteration).
    pub fn list(&self) -> impl Iterator<Item = &InstalledSkill> {
        self.skills.values()
    }

    /// Record a tenant learning loop's patch to an installed skill. The
    /// mutation lands in the ledger like any other — shipped skills are
    /// ordinary skills.
    pub fn record_local_patch(
        &mut self,
        skill_name: &str,
        patched_content_hash: impl Into<String>,
        actor: &str,
    ) -> Result<()> {
        let record = self.skills.get_mut(skill_name).ok_or_else(|| {
            pack_err(format!("skill `{skill_name}` is not installed from a pack"))
        })?;
        let content_hash = patched_content_hash.into();
        record.content_hash = content_hash.clone();
        record.locally_patched = true;
        record.record(actor, SkillPackMutation::LocallyPatched { content_hash });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Install, invalidation, update
// ---------------------------------------------------------------------------

/// The install disposition of one skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillInstallDisposition {
    /// Installed as `Trial` and promoted: the gate passed.
    Promoted,
    /// Installed as `Trial`; the gate failed and the failing cases are named.
    TrialAfterGateFailure {
        /// The failing dataset cases.
        failing_cases: Vec<String>,
    },
    /// Not installed: declared dependencies the tenant does not satisfy.
    MissingDependencies {
        /// The unsatisfied dependencies.
        missing: Vec<SkillDependency>,
    },
}

/// The per-skill outcome of an install or update pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInstallOutcome {
    /// The skill's registry name.
    pub skill_name: String,
    /// What happened.
    pub disposition: SkillInstallDisposition,
}

/// Install every skill in a pack into a tenant: register the payload,
/// enter as `Trial`, run the bundled eval suite, promote only on a pass.
///
/// A skill whose declared dependencies the environment does not satisfy is
/// not installed at all — a stock agent must not come up believing it can
/// call tools the tenant does not have. The registry registration still
/// happens for satisfied skills even when their gate fails: the skill is
/// present, inspectable, and honestly `Trial`.
pub fn install_skill_pack(
    pack: &SkillPack,
    env: &DependencyEnvironment,
    runner: &dyn SkillGateRunner,
    registry: &mut SkillRegistry,
    ledger: &mut SkillPackLedger,
    actor: &str,
) -> Result<Vec<SkillInstallOutcome>> {
    let mut outcomes = Vec::with_capacity(pack.skills.len());
    for skill in &pack.skills {
        let missing = env.missing(&skill.entry);
        if !missing.is_empty() {
            outcomes.push(SkillInstallOutcome {
                skill_name: skill.name().to_owned(),
                disposition: SkillInstallDisposition::MissingDependencies { missing },
            });
            continue;
        }

        let source = SkillSource::Package {
            package_id: pack.manifest.id.as_str().to_owned(),
            publisher: pack.manifest.publisher.as_str().to_owned(),
            version: pack.manifest.version.to_string(),
        };
        registry
            .register(skill.package.clone(), source, actor)
            .map_err(|error| pack_err(format!("registering `{}`: {error}", skill.name())))?;

        let content_hash = skill.package.content_hash();
        let mut record = InstalledSkill {
            skill_name: skill.name().to_owned(),
            package_id: pack.manifest.id.as_str().to_owned(),
            publisher: pack.manifest.publisher.as_str().to_owned(),
            package_version: pack.manifest.version.to_string(),
            shipped_content_hash: content_hash.clone(),
            content_hash: content_hash.clone(),
            status: SkillPromotionStatus::Trial,
            revalidation_pending: None,
            locally_patched: false,
            ledger: Vec::new(),
        };
        record.record(actor, SkillPackMutation::Install { content_hash });

        let gate = runner.run_gate(skill)?;
        let disposition = if gate.passed {
            record.status = SkillPromotionStatus::Promoted;
            record.record(
                actor,
                SkillPackMutation::GatePassed {
                    run_id: gate.run_id,
                },
            );
            SkillInstallDisposition::Promoted
        } else {
            record.record(
                actor,
                SkillPackMutation::GateFailed {
                    run_id: gate.run_id,
                    failing_cases: gate.failing_cases.clone(),
                },
            );
            SkillInstallDisposition::TrialAfterGateFailure {
                failing_cases: gate.failing_cases,
            }
        };
        ledger.skills.insert(skill.name().to_owned(), record);
        outcomes.push(SkillInstallOutcome {
            skill_name: skill.name().to_owned(),
            disposition,
        });
    }
    Ok(outcomes)
}

/// The revalidation outcome for one skill flagged by a dependency change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevalidationOutcome {
    /// The gate re-ran and passed; the skill holds its state.
    Held {
        /// The skill's registry name.
        skill_name: String,
    },
    /// The gate re-ran and failed; the skill demoted to `Trial`.
    Demoted {
        /// The skill's registry name.
        skill_name: String,
        /// The failing dataset cases, named.
        failing_cases: Vec<String>,
    },
}

/// Apply a dependency change: flag every affected installed skill
/// revalidation-pending and re-run its gate. A failure demotes to `Trial`
/// with the triggering change named; a pass clears the flag.
///
/// The gate runner resolves each skill's bundled suite from the pack the
/// tenant has on file — `packs` maps skill name to the loaded skill that
/// installed it.
pub fn apply_dependency_change(
    change: DependencyChange,
    packs: &BTreeMap<String, &LoadedSkill>,
    ledger: &mut SkillPackLedger,
    runner: &dyn SkillGateRunner,
    actor: &str,
) -> Result<Vec<RevalidationOutcome>> {
    let mut affected = Vec::new();
    for (name, record) in ledger.skills.iter_mut() {
        let Some(skill) = packs.get(name) else {
            continue;
        };
        if !change.affects(&skill.entry, &record.package_id) {
            continue;
        }
        record.revalidation_pending = Some(change.clone());
        record.record(
            actor,
            SkillPackMutation::Invalidated {
                trigger: change.clone(),
            },
        );
        affected.push(name.clone());
    }

    let mut outcomes = Vec::new();
    for name in affected {
        let skill = packs
            .get(&name)
            .expect("affected names were drawn from the packs map");
        let gate = runner.run_gate(skill)?;
        let record = ledger
            .skills
            .get_mut(&name)
            .expect("affected names were drawn from the ledger");
        if gate.passed {
            record.revalidation_pending = None;
            record.record(
                actor,
                SkillPackMutation::Revalidated {
                    run_id: gate.run_id,
                },
            );
            outcomes.push(RevalidationOutcome::Held { skill_name: name });
        } else {
            record.status = SkillPromotionStatus::Trial;
            record.record(
                actor,
                SkillPackMutation::Demoted {
                    run_id: gate.run_id,
                    trigger: change.clone(),
                    failing_cases: gate.failing_cases.clone(),
                },
            );
            outcomes.push(RevalidationOutcome::Demoted {
                skill_name: name,
                failing_cases: gate.failing_cases,
            });
        }
    }
    Ok(outcomes)
}

/// The update disposition of one skill when a new package version arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillUpdateDisposition {
    /// The installed payload already matches the new package; nothing to do.
    AlreadyCurrent,
    /// The new revision registered and re-gated.
    Updated {
        /// `true` when the update's gate run kept the skill promoted.
        promoted: bool,
    },
    /// The skill was locally patched: the three-way situation is surfaced in
    /// the ledger (shipped-old, shipped-new, local) and the local payload is
    /// left untouched. Resolution is a human's.
    ThreeWay,
}

/// The per-skill outcome of a package update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillUpdateOutcome {
    /// The skill's registry name.
    pub skill_name: String,
    /// What happened.
    pub disposition: SkillUpdateDisposition,
}

/// Apply a newer version of an installed pack.
///
/// Unpatched skills take the update: the new revision registers, the ledger
/// records it, and the gate re-runs — a shipped update re-verifies like any
/// other change. Locally-patched skills never silently update: the ledger
/// records the three-way situation and the tenant's payload stands until a
/// human resolves it.
pub fn apply_pack_update(
    new_pack: &SkillPack,
    ledger: &mut SkillPackLedger,
    registry: &mut SkillRegistry,
    runner: &dyn SkillGateRunner,
    actor: &str,
) -> Result<Vec<SkillUpdateOutcome>> {
    let mut outcomes = Vec::with_capacity(new_pack.skills.len());
    for skill in &new_pack.skills {
        let new_hash = skill.package.content_hash();
        let Some(record) = ledger.skills.get_mut(skill.name()) else {
            return Err(pack_err(format!(
                "update for `{}` names skill `{}`, which is not installed",
                new_pack.manifest.id.as_str(),
                skill.name()
            )));
        };
        if record.content_hash == new_hash && !record.locally_patched {
            outcomes.push(SkillUpdateOutcome {
                skill_name: skill.name().to_owned(),
                disposition: SkillUpdateDisposition::AlreadyCurrent,
            });
            continue;
        }
        if record.locally_patched {
            record.record(
                actor,
                SkillPackMutation::ThreeWayUpdate {
                    shipped_old: record.shipped_content_hash.clone(),
                    shipped_new: new_hash,
                    local: record.content_hash.clone(),
                },
            );
            outcomes.push(SkillUpdateOutcome {
                skill_name: skill.name().to_owned(),
                disposition: SkillUpdateDisposition::ThreeWay,
            });
            continue;
        }

        let source = SkillSource::Package {
            package_id: new_pack.manifest.id.as_str().to_owned(),
            publisher: new_pack.manifest.publisher.as_str().to_owned(),
            version: new_pack.manifest.version.to_string(),
        };
        registry
            .register(skill.package.clone(), source, actor)
            .map_err(|error| pack_err(format!("registering `{}`: {error}", skill.name())))?;

        record.package_version = new_pack.manifest.version.to_string();
        record.shipped_content_hash = new_hash.clone();
        record.content_hash = new_hash.clone();
        record.record(
            actor,
            SkillPackMutation::Updated {
                content_hash: new_hash,
            },
        );
        let gate = runner.run_gate(skill)?;
        let promoted = gate.passed;
        if gate.passed {
            record.status = SkillPromotionStatus::Promoted;
            record.record(
                actor,
                SkillPackMutation::GatePassed {
                    run_id: gate.run_id,
                },
            );
        } else {
            record.status = SkillPromotionStatus::Trial;
            record.record(
                actor,
                SkillPackMutation::GateFailed {
                    run_id: gate.run_id,
                    failing_cases: gate.failing_cases,
                },
            );
        }
        outcomes.push(SkillUpdateOutcome {
            skill_name: skill.name().to_owned(),
            disposition: SkillUpdateDisposition::Updated { promoted },
        });
    }
    Ok(outcomes)
}
