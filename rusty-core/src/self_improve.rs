//! The self-improvement plane: the harness introspecting what it has and
//! what it lacks, keeping a backlog of the difference, and — for gaps whose
//! closure is *data* (a skill package, a tool definition) — drafting that
//! closure through the composer, behind the composer's unchanged approval
//! gate.
//!
//! Three concepts, deliberately separate:
//!
//! 1. **The capability catalog** ([`capability_catalog`]) is declarative:
//!    every entry names a capability the harness advertises or knows it
//!    lacks, the plane it belongs to, and a **probe** — a pure function over
//!    a [`CapabilityInspection`] snapshot returning [`CapabilityStatus`].
//!    Probes are honest by construction: they read only evidence the host
//!    put in the snapshot (a registered tool name, a plane flag, a declared
//!    feature), so a capability reports `Present` only when something real
//!    backs it, and the known gaps from the dsh parity review stay `Absent`
//!    until the stream that closes them lands. Nothing here claims presence
//!    by fiat — the worst a probe can do is report the snapshot faithfully.
//! 2. **The gap report** ([`assess`]) runs every probe over one snapshot and
//!    returns a deterministic, ordered [`GapReport`]. It is a pure function
//!    of catalog × snapshot: same inputs, same report, no clocks, no IO.
//! 3. **The backlog** ([`BacklogStore`]) persists what the report means for
//!    work: append-only entries with content-derived ids, a validated status
//!    machine (`proposed → approved → in_progress → done`, `rejected` from
//!    any open state), provenance (`operator:*` or `harness:self-improve`),
//!    and injected timestamps only.
//!
//! The self-build path ([`draft_skill_for_entry`], [`publish_staged_skill`],
//! and the [`BuildGapSkillTool`] wrapping them) closes the loop for
//! skill-shaped gaps *without* crossing any trust boundary: drafting
//! requires an `approved` backlog entry (the backlog disposes before the
//! composer drafts), the draft runs through [`ComposeSkillTool`]'s existing
//! validators, and publishing still requires an operator-minted
//! [`ApprovalToken`] scoped to the draft's publish effect id. The harness
//! proposes; the approval disposes. Self-improvement never bypasses the
//! gate — it queues behind it with better evidence.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::composer::{
    publish_effect_id, ComposeSkillTool, ComposerSession, PublishComposedSkillTool,
};
use crate::effects::{ApprovalToken, EffectId};
use crate::error::{Result, RustyError};
use crate::journal::Clock;
use crate::record::Effect;
use crate::skill::SkillRegistry;
use crate::tool::Tool;

/// The backlog entry id prefix: an entry id is `bl-` followed by the
/// lowercase hex SHA-256 of the entry's canonical identity (title +
/// addressed gaps) — the same digest convention capability sets and
/// manifest pins follow.
pub const BACKLOG_ENTRY_ID_PREFIX: &str = "bl-";

/// The persisted backlog file's format version. Reading a file that
/// declares anything else fails closed — a backlog the plane cannot
/// interpret is evidence to preserve, not to guess at.
pub const BACKLOG_FORMAT_VERSION: u32 = 1;

/// The provenance label every harness-proposed entry carries.
pub const HARNESS_PROVENANCE: &str = "harness:self-improve";

/// Bounds on backlog text fields. An entry is an operator-facing artifact;
/// the bounds keep one proposal from spending the backlog's readability.
pub const MAX_BACKLOG_TITLE_BYTES: usize = 128;
/// See [`MAX_BACKLOG_TITLE_BYTES`].
pub const MAX_BACKLOG_RATIONALE_BYTES: usize = 1024;
/// See [`MAX_BACKLOG_TITLE_BYTES`].
pub const MAX_BACKLOG_EVIDENCE_BYTES: usize = 1024;
/// The most gaps one entry may address. An entry addressing everything
/// addresses nothing; the bound keeps entries actionable.
pub const MAX_BACKLOG_GAPS: usize = 4;
/// The most entries one propose call may carry.
pub const MAX_PROPOSE_ENTRIES: usize = 8;

// --------------------------------------------------------------------- //
// Feature flags
// --------------------------------------------------------------------- //
//
// Subsystems with no tool/manifest face (a metering ledger, a sandbox
// backend) are detectable in a snapshot only as a host-declared flag. The
// flags are constants so probe and host agree on the spelling; the honesty
// contract is that a host sets a flag only for a subsystem it actually
// wired — the same contract plane flags already carry.

/// Host feature flag: capability sets and per-run tool allowlists are
/// admitted at run start.
pub const FEATURE_CAPABILITY_SETS: &str = "capability_sets";
/// Host feature flag: a derived conversation-surface projection over the
/// immutable journal (compaction that cannot corrupt evidence).
pub const FEATURE_SURFACE_COMPACTION: &str = "surface_compaction";
/// Host feature flag: token-level stream events captured between request
/// and response.
pub const FEATURE_STREAMING_CHUNK_CAPTURE: &str = "streaming_chunk_capture";
/// Host feature flag: a mirrored telemetry ledger with a redaction
/// waterfall derived from journal evidence.
pub const FEATURE_TELEMETRY_LEDGER: &str = "telemetry_ledger";
/// Host feature flag: journal-derived token metering (request pressure,
/// per-surface pricing).
pub const FEATURE_TOKEN_METER: &str = "token_meter";
/// Host feature flag: an RAII-guarded plugin kernel (registrations unwind
/// LIFO on unload).
pub const FEATURE_PLUGIN_KERNEL: &str = "plugin_kernel";
/// Host feature flag: OS-level confinement (sandbox-exec/bwrap) with
/// per-execution enforcement reports.
pub const FEATURE_OS_SANDBOX: &str = "os_sandbox";
/// Host feature flag: provider-neutral tool render intents derived from
/// the journal.
pub const FEATURE_RENDER_INTENTS: &str = "render_intents";
/// Host feature flag: named, reusable permission presets over capability
/// sets.
pub const FEATURE_PERMISSION_PRESETS: &str = "permission_presets";
/// Host feature flag: a durable steer/follow-up inbox for running or
/// parked runs.
pub const FEATURE_DURABLE_STEER_INBOX: &str = "durable_steer_inbox";
/// Host feature flag: code mode — model-authored code in a confined,
/// journaled environment.
pub const FEATURE_CODE_MODE: &str = "code_mode";
/// Host feature flag: a durable goals subsystem decomposing objectives
/// into runs.
pub const FEATURE_GOALS_SUBSYSTEM: &str = "goals_subsystem";
/// Host feature flag: Claude-Code/Codex `hooks.json` wire compatibility.
pub const FEATURE_HOOKS_COMPATIBILITY: &str = "hooks_compatibility";

/// The name prefix runbook skills carry — the registered-skill evidence
/// the `operator-runbooks` probe looks for.
pub const RUNBOOK_SKILL_PREFIX: &str = "runbook-";

// --------------------------------------------------------------------- //
// Planes and the inspection snapshot
// --------------------------------------------------------------------- //

/// The harness plane a capability belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Plane {
    /// Validated, scanned SKILL.md packages in a versioned registry.
    Skills,
    /// Content-addressed connector manifests and service packs.
    Connectors,
    /// Content-addressed sources with cited chunk retrieval.
    Knowledge,
    /// Governed memory with corrections and supersession.
    Memory,
    /// The Flight Recorder: journaled calls, exact replay, receipts.
    Evidence,
    /// Durable agents, mailboxes, coordination contracts.
    Agents,
    /// Tool execution (dispatch, allowlists, confinement).
    Tools,
    /// The middleware interception SDK.
    Middleware,
    /// Operator-facing surfaces (Studio and its renderings).
    Studio,
}

/// A point-in-time snapshot of what the harness can evidence, assembled by
/// the host from its real registries. Probes never look past this struct:
/// if the host cannot point at it here, a probe cannot claim it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityInspection {
    /// Names registered in the skill registry the host holds.
    pub skill_names: Vec<String>,
    /// Ids of the connector manifests the host loaded.
    pub connector_manifest_ids: Vec<String>,
    /// Names registered across the host's tool registries.
    pub tool_names: Vec<String>,
    /// The planes the host wired.
    pub planes: Vec<Plane>,
    /// Host-declared subsystem flags (see the `FEATURE_*` constants).
    pub features: Vec<String>,
}

impl CapabilityInspection {
    /// Normalize for probing: sorted, deduplicated members, so two hosts
    /// assembling the same reality in different orders probe identically.
    pub fn normalized(mut self) -> Self {
        self.skill_names.sort();
        self.skill_names.dedup();
        self.connector_manifest_ids.sort();
        self.connector_manifest_ids.dedup();
        self.tool_names.sort();
        self.tool_names.dedup();
        self.planes.sort();
        self.planes.dedup();
        self.features.sort();
        self.features.dedup();
        self
    }

    fn has_tool(&self, name: &str) -> bool {
        self.tool_names.iter().any(|tool| tool == name)
    }

    fn has_plane(&self, plane: Plane) -> bool {
        self.planes.contains(&plane)
    }

    fn has_feature(&self, feature: &str) -> bool {
        self.features.iter().any(|flag| flag == feature)
    }
}

// --------------------------------------------------------------------- //
// The capability model
// --------------------------------------------------------------------- //

/// What a probe found in the snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CapabilityStatus {
    /// Real evidence backs the capability.
    Present,
    /// Something real exists but does not close the capability; `note`
    /// names exactly what is missing.
    Partial {
        /// What stands between the evidence and `Present`.
        note: String,
    },
    /// No evidence. The honest default for every known gap.
    Absent,
}

impl CapabilityStatus {
    /// `true` when the capability is fully evidenced.
    pub fn is_present(&self) -> bool {
        matches!(self, CapabilityStatus::Present)
    }
}

/// How a gap would close, when it is a gap. `CoreStream` gaps need a
/// development stream in the core crates; `Skill` and `ToolDefinition`
/// gaps close with *data* and are therefore draftable through the composer
/// once a backlog entry for them is approved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildShape {
    /// A governed skill package (procedural knowledge) closes it.
    Skill,
    /// A composed tool definition (a bounded declarative recipe) closes it.
    ToolDefinition,
    /// Only a core development stream closes it; the composer is not the
    /// door.
    CoreStream,
}

/// One catalog entry: a capability, its plane, and the probe that detects
/// it honestly.
pub struct Capability {
    /// Stable kebab-case id; backlog entries reference these.
    pub id: &'static str,
    /// The plane the capability belongs to.
    pub plane: Plane,
    /// One line: what the capability is.
    pub description: &'static str,
    /// How the gap would close while the status is not `Present`.
    pub build: BuildShape,
    probe: fn(&CapabilityInspection) -> CapabilityStatus,
}

impl std::fmt::Debug for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Capability")
            .field("id", &self.id)
            .field("plane", &self.plane)
            .field("build", &self.build)
            .finish()
    }
}

impl Capability {
    /// Run the probe over `inspection` (normalized first, so probe results
    /// never depend on the host's assembly order).
    pub fn probe(&self, inspection: &CapabilityInspection) -> CapabilityStatus {
        (self.probe)(&inspection.clone().normalized())
    }
}

/// `Present` iff `plane` is flagged in the snapshot.
fn plane_probe(plane: Plane) -> fn(&CapabilityInspection) -> CapabilityStatus {
    match plane {
        Plane::Skills => |i| flag(i.has_plane(Plane::Skills)),
        Plane::Connectors => |i| flag(i.has_plane(Plane::Connectors)),
        Plane::Knowledge => |i| flag(i.has_plane(Plane::Knowledge)),
        Plane::Memory => |i| flag(i.has_plane(Plane::Memory)),
        Plane::Evidence => |i| flag(i.has_plane(Plane::Evidence)),
        Plane::Agents => |i| flag(i.has_plane(Plane::Agents)),
        Plane::Tools => |i| flag(i.has_plane(Plane::Tools)),
        Plane::Middleware => |i| flag(i.has_plane(Plane::Middleware)),
        Plane::Studio => |i| flag(i.has_plane(Plane::Studio)),
    }
}

fn flag(present: bool) -> CapabilityStatus {
    if present {
        CapabilityStatus::Present
    } else {
        CapabilityStatus::Absent
    }
}

/// `Present` iff `feature` is declared in the snapshot.
fn feature_probe(feature: &'static str) -> fn(&CapabilityInspection) -> CapabilityStatus {
    match feature {
        FEATURE_CAPABILITY_SETS => |i| flag(i.has_feature(FEATURE_CAPABILITY_SETS)),
        FEATURE_SURFACE_COMPACTION => |i| flag(i.has_feature(FEATURE_SURFACE_COMPACTION)),
        FEATURE_STREAMING_CHUNK_CAPTURE => {
            |i| flag(i.has_feature(FEATURE_STREAMING_CHUNK_CAPTURE))
        }
        FEATURE_TELEMETRY_LEDGER => |i| flag(i.has_feature(FEATURE_TELEMETRY_LEDGER)),
        FEATURE_TOKEN_METER => |i| flag(i.has_feature(FEATURE_TOKEN_METER)),
        FEATURE_PLUGIN_KERNEL => |i| flag(i.has_feature(FEATURE_PLUGIN_KERNEL)),
        FEATURE_OS_SANDBOX => |i| flag(i.has_feature(FEATURE_OS_SANDBOX)),
        FEATURE_RENDER_INTENTS => |i| flag(i.has_feature(FEATURE_RENDER_INTENTS)),
        FEATURE_PERMISSION_PRESETS => |i| flag(i.has_feature(FEATURE_PERMISSION_PRESETS)),
        FEATURE_DURABLE_STEER_INBOX => |i| flag(i.has_feature(FEATURE_DURABLE_STEER_INBOX)),
        FEATURE_CODE_MODE => |i| flag(i.has_feature(FEATURE_CODE_MODE)),
        FEATURE_GOALS_SUBSYSTEM => |i| flag(i.has_feature(FEATURE_GOALS_SUBSYSTEM)),
        FEATURE_HOOKS_COMPATIBILITY => |i| flag(i.has_feature(FEATURE_HOOKS_COMPATIBILITY)),
        _ => unreachable!("feature_probe is called with the declared constants only"),
    }
}

/// The drafting half of the composer lane: `compose_skill` registered is
/// the whole evidence.
fn probe_composer_drafting(inspection: &CapabilityInspection) -> CapabilityStatus {
    flag(inspection.has_tool("compose_skill"))
}

/// The gated second rung. A registry with drafting but no publish tool is
/// `Partial`: drafts accumulate that nothing can land.
fn probe_gated_publish(inspection: &CapabilityInspection) -> CapabilityStatus {
    if inspection.has_tool("publish_composed_skill") {
        CapabilityStatus::Present
    } else if inspection.has_tool("compose_skill") {
        CapabilityStatus::Partial {
            note: "compose_skill is registered but publish_composed_skill is not — drafting \
                   without the approval-gated publish path"
                .to_owned(),
        }
    } else {
        CapabilityStatus::Absent
    }
}

/// Agent-visible session query: an agent tool reading its own journaled
/// history. The journals existing (evidence plane) is not the capability —
/// the query tool is.
fn probe_session_query(inspection: &CapabilityInspection) -> CapabilityStatus {
    flag(inspection.has_tool("session_search") || inspection.has_tool("session_trace"))
}

/// OS-level confinement. `run_cli` under an allowlist and jail is real but
/// is not confinement; that combination is exactly `Partial`.
fn probe_os_sandbox(inspection: &CapabilityInspection) -> CapabilityStatus {
    if inspection.has_feature(FEATURE_OS_SANDBOX) {
        CapabilityStatus::Present
    } else if inspection.has_tool("run_cli") {
        CapabilityStatus::Partial {
            note: "run_cli runs under an allowlist and jail but no OS-level confinement \
                   (sandbox-exec/bwrap) with an enforcement report"
                .to_owned(),
        }
    } else {
        CapabilityStatus::Absent
    }
}

/// Hooks wire compatibility. The middleware SDK covers the *capability*;
/// the `hooks.json` wire protocol is the adoption play — middleware without
/// it is `Partial`, per the parity review.
fn probe_hooks_compatibility(inspection: &CapabilityInspection) -> CapabilityStatus {
    if inspection.has_feature(FEATURE_HOOKS_COMPATIBILITY) {
        CapabilityStatus::Present
    } else if inspection.has_plane(Plane::Middleware) {
        CapabilityStatus::Partial {
            note: "the middleware SDK covers the capability; the hooks.json wire protocol is \
                   not implemented"
                .to_owned(),
        }
    } else {
        CapabilityStatus::Absent
    }
}

/// Runbook skills: present only when a skill whose name carries the
/// runbook prefix is actually registered — a drafted-but-unpublished
/// runbook does not count, which is what keeps the approval gate visible
/// in the report.
fn probe_operator_runbooks(inspection: &CapabilityInspection) -> CapabilityStatus {
    flag(inspection
        .skill_names
        .iter()
        .any(|name| name.starts_with(RUNBOOK_SKILL_PREFIX)))
}

/// The declarative capability catalog: rusty's real planes first, then the
/// known gaps from the dsh parity review (`docs/review-deepseek-harness.md`),
/// in a stable declaration order that the gap report preserves.
///
/// The catalog is a function, not a static, so a host extending it builds
/// on this baseline rather than mutating shared state.
pub fn capability_catalog() -> Vec<Capability> {
    vec![
        // -- The planes rusty already has.
        Capability {
            id: "skill-plane",
            plane: Plane::Skills,
            description: "validated, scanned SKILL.md packages in a versioned registry",
            build: BuildShape::CoreStream,
            probe: plane_probe(Plane::Skills),
        },
        Capability {
            id: "connector-plane",
            plane: Plane::Connectors,
            description: "content-addressed connector manifests and service packs",
            build: BuildShape::CoreStream,
            probe: plane_probe(Plane::Connectors),
        },
        Capability {
            id: "knowledge-plane",
            plane: Plane::Knowledge,
            description: "content-addressed knowledge sources with cited chunk retrieval",
            build: BuildShape::CoreStream,
            probe: plane_probe(Plane::Knowledge),
        },
        Capability {
            id: "memory-plane",
            plane: Plane::Memory,
            description: "governed memory with corrections, supersession, and scoped retrieval",
            build: BuildShape::CoreStream,
            probe: plane_probe(Plane::Memory),
        },
        Capability {
            id: "flight-recorder",
            plane: Plane::Evidence,
            description: "journaled model/tool/remote calls with verified exact replay",
            build: BuildShape::CoreStream,
            probe: plane_probe(Plane::Evidence),
        },
        Capability {
            id: "agent-fabric",
            plane: Plane::Agents,
            description: "durable agents with stable identity, mailboxes, and coordination contracts",
            build: BuildShape::CoreStream,
            probe: plane_probe(Plane::Agents),
        },
        Capability {
            id: "composer-drafting",
            plane: Plane::Skills,
            description: "agent-drafted skill packages and tool definitions, validated and scanned",
            build: BuildShape::CoreStream,
            probe: probe_composer_drafting,
        },
        Capability {
            id: "approval-gated-publish",
            plane: Plane::Skills,
            description: "composer drafts cross into the shared registry only behind a scoped approval token",
            build: BuildShape::CoreStream,
            probe: probe_gated_publish,
        },
        Capability {
            id: "capability-sets",
            plane: Plane::Agents,
            description: "content-addressed capability sets resolved to exact per-run tool allowlists",
            build: BuildShape::CoreStream,
            probe: feature_probe(FEATURE_CAPABILITY_SETS),
        },
        // -- The known gaps (dsh parity review). Every one starts Absent in
        //    a snapshot that carries no evidence for it; each flips only
        //    when the stream that closes it lands something a probe can see.
        Capability {
            id: "surface-compaction",
            plane: Plane::Evidence,
            description: "derived conversation-surface projection (append/replace spans with \
                          source citations) over the immutable journal",
            build: BuildShape::CoreStream,
            probe: feature_probe(FEATURE_SURFACE_COMPACTION),
        },
        Capability {
            id: "streaming-chunk-capture",
            plane: Plane::Evidence,
            description: "token-level stream events between request and response for \
                          streaming-fidelity replay",
            build: BuildShape::CoreStream,
            probe: feature_probe(FEATURE_STREAMING_CHUNK_CAPTURE),
        },
        Capability {
            id: "telemetry-ledger",
            plane: Plane::Evidence,
            description: "mirrored telemetry ledger with a redaction waterfall derived from \
                          journal evidence",
            build: BuildShape::CoreStream,
            probe: feature_probe(FEATURE_TELEMETRY_LEDGER),
        },
        Capability {
            id: "token-meter",
            plane: Plane::Evidence,
            description: "journal-derived token metering: request pressure and per-surface pricing",
            build: BuildShape::CoreStream,
            probe: feature_probe(FEATURE_TOKEN_METER),
        },
        Capability {
            id: "agent-session-query",
            plane: Plane::Evidence,
            description: "agent-visible query over its own journaled history \
                          (session_search/session_trace); today journals are operator-visible only",
            build: BuildShape::ToolDefinition,
            probe: probe_session_query,
        },
        Capability {
            id: "plugin-kernel",
            plane: Plane::Agents,
            description: "RAII-guarded plugin registrations unwound LIFO on unload, with WASM \
                          capsules as the guest vehicle",
            build: BuildShape::CoreStream,
            probe: feature_probe(FEATURE_PLUGIN_KERNEL),
        },
        Capability {
            id: "os-sandbox-confinement",
            plane: Plane::Tools,
            description: "per-call OS confinement (sandbox-exec/bwrap) with honest enforcement \
                          facts on every execution receipt",
            build: BuildShape::CoreStream,
            probe: probe_os_sandbox,
        },
        Capability {
            id: "render-intents",
            plane: Plane::Studio,
            description: "provider-neutral tool render intents derived purely from the journal, \
                          so replay renders identically",
            build: BuildShape::CoreStream,
            probe: feature_probe(FEATURE_RENDER_INTENTS),
        },
        Capability {
            id: "permission-presets",
            plane: Plane::Agents,
            description: "named, reusable permission presets over capability sets",
            build: BuildShape::CoreStream,
            probe: feature_probe(FEATURE_PERMISSION_PRESETS),
        },
        Capability {
            id: "durable-steer-inbox",
            plane: Plane::Agents,
            description: "durable steer/follow-up inbox: messages queued into a running or \
                          parked run",
            build: BuildShape::CoreStream,
            probe: feature_probe(FEATURE_DURABLE_STEER_INBOX),
        },
        Capability {
            id: "code-mode",
            plane: Plane::Tools,
            description: "code mode: model-authored code executed in a confined, journaled \
                          environment",
            build: BuildShape::CoreStream,
            probe: feature_probe(FEATURE_CODE_MODE),
        },
        Capability {
            id: "goals-subsystem",
            plane: Plane::Agents,
            description: "durable goals: long-lived objectives decomposed into runs with \
                          progress evidence",
            build: BuildShape::CoreStream,
            probe: feature_probe(FEATURE_GOALS_SUBSYSTEM),
        },
        Capability {
            id: "hooks-compatibility",
            plane: Plane::Middleware,
            description: "Claude-Code/Codex hooks.json wire compatibility, so existing user \
                          hooks run unmodified",
            build: BuildShape::CoreStream,
            probe: probe_hooks_compatibility,
        },
        Capability {
            id: "operator-runbooks",
            plane: Plane::Skills,
            description: "recurring operator workflows captured as governed `runbook-*` skill \
                          packages instead of ad-hoc prompts",
            build: BuildShape::Skill,
            probe: probe_operator_runbooks,
        },
    ]
}

/// One catalog entry's probe outcome, in report order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapabilityAssessment {
    /// The catalog id.
    pub id: &'static str,
    /// The plane the capability belongs to.
    pub plane: Plane,
    /// The one-line capability description.
    pub description: &'static str,
    /// How the gap would close while the status is not `Present`.
    pub build: BuildShape,
    /// What the probe found.
    pub status: CapabilityStatus,
}

/// The deterministic result of probing the whole catalog over one
/// snapshot. Counts are derived, never claimed; `assessments` preserves
/// catalog order so reports diff stably.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GapReport {
    /// Entries whose probe found full evidence.
    pub present: usize,
    /// Entries with partial evidence.
    pub partial: usize,
    /// Entries with no evidence.
    pub absent: usize,
    /// Every entry's outcome, in catalog order.
    pub assessments: Vec<CapabilityAssessment>,
}

impl GapReport {
    /// The gaps — `Partial` and `Absent` entries, in catalog order.
    pub fn gaps(&self) -> impl Iterator<Item = &CapabilityAssessment> {
        self.assessments
            .iter()
            .filter(|assessment| !assessment.status.is_present())
    }
}

/// Run every probe in `catalog` over `inspection`. Pure: no clocks, no IO,
/// same inputs → same report.
pub fn assess(catalog: &[Capability], inspection: &CapabilityInspection) -> GapReport {
    let normalized = inspection.clone().normalized();
    let assessments: Vec<CapabilityAssessment> = catalog
        .iter()
        .map(|capability| CapabilityAssessment {
            id: capability.id,
            plane: capability.plane,
            description: capability.description,
            build: capability.build,
            status: (capability.probe)(&normalized),
        })
        .collect();
    let (mut present, mut partial, mut absent) = (0, 0, 0);
    for assessment in &assessments {
        match assessment.status {
            CapabilityStatus::Present => present += 1,
            CapabilityStatus::Partial { .. } => partial += 1,
            CapabilityStatus::Absent => absent += 1,
        }
    }
    GapReport {
        present,
        partial,
        absent,
        assessments,
    }
}

/// One catalog entry by id (`None` for an id the catalog does not know).
pub fn catalog_entry<'a>(catalog: &'a [Capability], id: &str) -> Option<&'a Capability> {
    catalog.iter().find(|capability| capability.id == id)
}

// --------------------------------------------------------------------- //
// The backlog
// --------------------------------------------------------------------- //

/// Backlog entry status. The machine: `proposed → approved → in_progress →
/// done`, with `rejected` reachable from every open state. `done` requires
/// an evidence note; `done` and `rejected` are terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BacklogStatus {
    /// Recorded, awaiting a decision.
    Proposed,
    /// Accepted for work — the state the self-build path requires before
    /// it drafts anything.
    Approved,
    /// Being built.
    InProgress,
    /// Closed with evidence.
    Done,
    /// Considered and refused (terminal, kept as evidence).
    Rejected,
}

/// Who created the entry. The vocabulary is closed so the audit question
/// "did the harness propose this to itself?" always has a typed answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BacklogProvenance {
    /// A human operator (`operator:{id}`).
    Operator {
        /// The operator id (the `*` in `operator:*`).
        operator: String,
    },
    /// The harness's own self-improvement loop.
    HarnessSelfImprove,
}

impl BacklogProvenance {
    /// An operator provenance; the id is shape-checked.
    pub fn operator(id: impl Into<String>) -> Result<Self> {
        let id = id.into();
        check_text("operator id", &id, MAX_BACKLOG_TITLE_BYTES)?;
        Ok(BacklogProvenance::Operator { operator: id })
    }

    /// The audit label: `operator:{id}` or `harness:self-improve`.
    pub fn label(&self) -> String {
        match self {
            BacklogProvenance::Operator { operator } => format!("operator:{operator}"),
            BacklogProvenance::HarnessSelfImprove => HARNESS_PROVENANCE.to_owned(),
        }
    }
}

/// One backlog entry. Identity is content-derived from the title and the
/// addressed gaps — re-proposing the same work converges on the same id —
/// while status, evidence, and timestamps are state over that identity, so
/// a transition never forks what the entry *is*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BacklogEntry {
    /// `bl-` + SHA-256 of the canonical identity (title + gap ids).
    pub id: String,
    /// What to do, one line.
    pub title: String,
    /// Why it is worth doing.
    pub rationale: String,
    /// Catalog gap ids the entry addresses, sorted.
    pub gap_ids: Vec<String>,
    /// Where the entry stands in the status machine.
    pub status: BacklogStatus,
    /// Who created it.
    pub provenance: BacklogProvenance,
    /// When it was proposed (injected clock).
    pub created_at: DateTime<Utc>,
    /// When it last changed state (injected clock).
    pub updated_at: DateTime<Utc>,
    /// The receipt for `done`; absent in every other state.
    pub evidence: Option<String>,
}

/// The wire shape, for id-verifying deserialization.
#[derive(Deserialize)]
struct BacklogEntryBody {
    id: String,
    title: String,
    rationale: String,
    gap_ids: Vec<String>,
    status: BacklogStatus,
    provenance: BacklogProvenance,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(default)]
    evidence: Option<String>,
}

impl<'de> Deserialize<'de> for BacklogEntry {
    /// Read an entry back and re-derive its id from the identity fields: a
    /// backlog file whose ids do not match their contents fails closed, the
    /// same discipline content-addressed candidates and capability sets
    /// keep.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let body = BacklogEntryBody::deserialize(deserializer)?;
        let derived = backlog_entry_id(&body.title, &body.gap_ids);
        if derived != body.id {
            return Err(serde::de::Error::custom(format!(
                "backlog entry id `{}` does not match its contents (derived `{derived}`); \
                 the file is corrupt or tampered",
                body.id
            )));
        }
        Ok(BacklogEntry {
            id: body.id,
            title: body.title,
            rationale: body.rationale,
            gap_ids: body.gap_ids,
            status: body.status,
            provenance: body.provenance,
            created_at: body.created_at,
            updated_at: body.updated_at,
            evidence: body.evidence,
        })
    }
}

/// The content-derived entry id: `bl-` + SHA-256 over the canonical
/// serialization of the sorted gap ids and the title.
fn backlog_entry_id(title: &str, gap_ids: &[String]) -> String {
    let mut gaps = gap_ids.to_vec();
    gaps.sort();
    let canonical = crate::record::canonicalize_value(&json!({
        "gap_ids": gaps,
        "title": title,
    }));
    let bytes = serde_json::to_vec(&canonical).expect("a serde_json::Value always serializes");
    format!(
        "{BACKLOG_ENTRY_ID_PREFIX}{}",
        crate::record::sha256_hex(&bytes)
    )
}

/// Bounded, control-free text — the shape rule every backlog string field
/// shares.
fn check_text(field: &str, value: &str, max: usize) -> Result<()> {
    if value.is_empty()
        || value != value.trim()
        || value.len() > max
        || value.chars().any(char::is_control)
    {
        return Err(RustyError::Tool(format!(
            "backlog {field} must be non-empty, trimmed, control-free, and at most {max} bytes"
        )));
    }
    Ok(())
}

impl BacklogEntry {
    /// Propose an entry. The id derives from the content, so the same
    /// proposal made twice is the same entry; `now` is the injected clock.
    pub fn new(
        title: impl Into<String>,
        rationale: impl Into<String>,
        gap_ids: &[String],
        provenance: BacklogProvenance,
        now: DateTime<Utc>,
    ) -> Result<Self> {
        let title = title.into();
        let rationale = rationale.into();
        check_text("title", &title, MAX_BACKLOG_TITLE_BYTES)?;
        check_text("rationale", &rationale, MAX_BACKLOG_RATIONALE_BYTES)?;
        if gap_ids.is_empty() || gap_ids.len() > MAX_BACKLOG_GAPS {
            return Err(RustyError::Tool(format!(
                "a backlog entry addresses between 1 and {MAX_BACKLOG_GAPS} gaps"
            )));
        }
        let mut gaps = gap_ids.to_vec();
        gaps.sort();
        for pair in gaps.windows(2) {
            if pair[0] == pair[1] {
                return Err(RustyError::Tool(format!(
                    "backlog entry names gap `{}` twice",
                    pair[0]
                )));
            }
        }
        for gap in &gaps {
            check_text("gap id", gap, MAX_BACKLOG_TITLE_BYTES)?;
        }
        let id = backlog_entry_id(&title, &gaps);
        Ok(BacklogEntry {
            id,
            title,
            rationale,
            gap_ids: gaps,
            status: BacklogStatus::Proposed,
            provenance,
            created_at: now,
            updated_at: now,
            evidence: None,
        })
        .and_then(|entry| entry.validate())
    }

    /// Structural invariants that must hold for any entry, however
    /// constructed — checked at the end of every constructor and
    /// transition.
    fn validate(self) -> Result<Self> {
        if self.status == BacklogStatus::Done && self.evidence.is_none() {
            return Err(RustyError::Tool(
                "a done backlog entry must carry its evidence note".to_owned(),
            ));
        }
        if self.status != BacklogStatus::Done && self.evidence.is_some() {
            return Err(RustyError::Tool(
                "only a done backlog entry carries an evidence note".to_owned(),
            ));
        }
        Ok(self)
    }

    /// Whether the transition `self.status → to` exists in the machine.
    fn transition_allowed(from: BacklogStatus, to: BacklogStatus) -> bool {
        match from {
            BacklogStatus::Proposed => {
                matches!(to, BacklogStatus::Approved | BacklogStatus::Rejected)
            }
            BacklogStatus::Approved => {
                matches!(to, BacklogStatus::InProgress | BacklogStatus::Rejected)
            }
            BacklogStatus::InProgress => {
                matches!(to, BacklogStatus::Done | BacklogStatus::Rejected)
            }
            BacklogStatus::Done | BacklogStatus::Rejected => false,
        }
    }

    /// Apply a status transition, returning the next entry. Illegal
    /// transitions fail closed naming both states; `done` requires a
    /// non-empty evidence note and every other target forbids one.
    pub fn transition(
        &self,
        to: BacklogStatus,
        evidence: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<Self> {
        if !Self::transition_allowed(self.status, to) {
            return Err(RustyError::Tool(format!(
                "backlog entry `{}` cannot move {:?} → {:?}: the machine is proposed → approved \
                 → in_progress → done, with rejected from any open state",
                self.id, self.status, to
            )));
        }
        let evidence = match (to, evidence) {
            (BacklogStatus::Done, Some(note)) => {
                check_text("evidence", &note, MAX_BACKLOG_EVIDENCE_BYTES)?;
                Some(note)
            }
            (BacklogStatus::Done, None) => {
                return Err(RustyError::Tool(
                    "closing a backlog entry as done requires an evidence note".to_owned(),
                ));
            }
            (_, Some(_)) => {
                return Err(RustyError::Tool(
                    "only the done transition carries an evidence note".to_owned(),
                ));
            }
            (_, None) => None,
        };
        BacklogEntry {
            status: to,
            updated_at: now,
            evidence,
            ..self.clone()
        }
        .validate()
    }
}

/// The persisted backlog file's envelope.
#[derive(Serialize, Deserialize)]
struct BacklogFile {
    format_version: u32,
    entries: Vec<BacklogEntry>,
}

/// Map an IO error into the store's error convention (the journal
/// artifact store's discipline).
fn backlog_io_error(context: String, e: std::io::Error) -> RustyError {
    RustyError::Serialization(serde_json::Error::io(std::io::Error::new(
        e.kind(),
        format!("{context}: {e}"),
    )))
}

/// The persisted, append-only backlog: one JSON file holding every entry,
/// rewritten atomically (temp-write-then-rename, the checkpointer's
/// discipline) on every accepted change. Append-only is a property of the
/// API, not the layout: there is no removal, insertion of an existing id
/// converges only when the content is identical, and a different entry
/// under an existing id fails — an id is a content address, so a mismatch
/// is a collision or tampering, never an update.
///
/// The in-memory index is rebuilt from the file at open; the file is the
/// truth and the index its projection.
#[derive(Debug)]
pub struct BacklogStore {
    path: PathBuf,
    entries: Mutex<BTreeMap<String, BacklogEntry>>,
}

impl BacklogStore {
    /// Open the backlog at `path`, creating nothing yet (the file appears
    /// on the first accepted write). A present file that does not parse,
    /// declares another format version, or fails id verification fails
    /// closed.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let entries = match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let file: BacklogFile = serde_json::from_slice(&bytes).map_err(|error| {
                    RustyError::Tool(format!(
                        "backlog file `{}` does not parse: {error}; refusing to guess at it",
                        path.display()
                    ))
                })?;
                if file.format_version != BACKLOG_FORMAT_VERSION {
                    return Err(RustyError::Tool(format!(
                        "backlog file `{}` declares format version {}, this plane reads \
                         {BACKLOG_FORMAT_VERSION}",
                        path.display(),
                        file.format_version
                    )));
                }
                let mut entries = BTreeMap::new();
                for entry in file.entries {
                    if entries.insert(entry.id.clone(), entry).is_some() {
                        return Err(RustyError::Tool(format!(
                            "backlog file `{}` holds a duplicate entry id",
                            path.display()
                        )));
                    }
                }
                entries
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => {
                return Err(backlog_io_error(
                    format!("read backlog `{}`", path.display()),
                    e,
                ))
            }
        };
        Ok(Self {
            path,
            entries: Mutex::new(entries),
        })
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, BacklogEntry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Write the current index back, atomically.
    async fn persist(&self, entries: &BTreeMap<String, BacklogEntry>) -> Result<()> {
        let file = BacklogFile {
            format_version: BACKLOG_FORMAT_VERSION,
            entries: entries.values().cloned().collect(),
        };
        let bytes = serde_json::to_vec(&file)?;
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| backlog_io_error(format!("create backlog dir `{}`", parent.display()), e))?;
        }
        crate::checkpoint::JsonFileCheckpointer::atomic_write(&self.path, &bytes).await
    }

    /// Insert a proposed entry. `true` when the entry is new; `false` when
    /// the id is already occupied (a converged re-proposal — the id *is*
    /// the identity, so the stored entry is never mutated by insertion,
    /// only by validated transitions). Identity fields that disagree with
    /// the id's occupant fail closed: with a content-derived id that means
    /// a hash collision or tampering, never an update.
    pub async fn insert(&self, entry: BacklogEntry) -> Result<bool> {
        let snapshot = {
            let mut entries = self.lock();
            match entries.get(&entry.id) {
                Some(existing) => {
                    if existing.title != entry.title || existing.gap_ids != entry.gap_ids {
                        return Err(RustyError::Tool(format!(
                            "backlog id `{}` is occupied by different identity fields — a \
                             content-address collision or tampering, not an update",
                            entry.id
                        )));
                    }
                    return Ok(false);
                }
                None => {
                    entries.insert(entry.id.clone(), entry);
                    entries.clone()
                }
            }
        };
        self.persist(&snapshot).await?;
        Ok(true)
    }

    /// Apply a validated status transition (see [`BacklogEntry::transition`])
    /// and persist the outcome. Returns the entry's new state.
    pub async fn transition(
        &self,
        id: &str,
        to: BacklogStatus,
        evidence: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<BacklogEntry> {
        let (next, snapshot) = {
            let mut entries = self.lock();
            let current = entries.get(id).ok_or_else(|| {
                RustyError::Tool(format!("unknown backlog entry `{id}`"))
            })?;
            let next = current.transition(to, evidence, now)?;
            entries.insert(id.to_owned(), next.clone());
            (next, entries.clone())
        };
        self.persist(&snapshot).await?;
        Ok(next)
    }

    /// One entry by id.
    pub fn get(&self, id: &str) -> Option<BacklogEntry> {
        self.lock().get(id).cloned()
    }

    /// Every entry, ordered by id (deterministic — ids are content
    /// addresses, so the order is stable across processes).
    pub fn list(&self) -> Vec<BacklogEntry> {
        self.lock().values().cloned().collect()
    }

    /// The number of entries.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// `true` when the backlog holds no entries.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}

// --------------------------------------------------------------------- //
// The self-build path
// --------------------------------------------------------------------- //

/// A proposed skill artifact: the parts [`ComposeSkillTool`] assembles into
/// a package. Kept as a struct (not loose args) because the self-build path
/// validates the *backlog half* before the composer ever sees the parts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillProposal {
    /// Kebab-case skill name.
    pub name: String,
    /// One-line description.
    pub description: String,
    /// The SKILL.md body (instructions).
    pub body: String,
    /// Optional reference members (paths relative to `references/`).
    #[serde(default)]
    pub references: BTreeMap<String, String>,
    /// The declared author (e.g. `harness:self-improve`).
    pub author: String,
}

/// A drafted skill staged behind the approval gate: the content address of
/// the session draft and the exact publish effect id an operator mints an
/// approval against. Holding a [`StagedSkill`] grants nothing — it is the
/// queue position, not the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StagedSkill {
    /// The backlog entry this draft addresses.
    pub entry_id: String,
    /// The content address of the validated session draft.
    pub content_hash: String,
    /// The scoped publish effect id — what an approval token must name.
    pub publish_effect_id: EffectId,
}

/// Draft a skill for an approved backlog entry through the composer, and
/// stage the publish.
///
/// Two gates, in order, both fail-closed:
///
/// 1. **The backlog disposes before the composer drafts.** The entry must
///    exist, be `approved`, and address at least one catalog gap whose
///    build shape is [`BuildShape::Skill`]. A `proposed` entry — including
///    one the harness proposed to itself — is not enough.
/// 2. **The composer's own validators.** The proposal runs through
///    [`ComposeSkillTool`], so a denied draft returns the receipt's
///    findings and revision notes as the error — a correction, not a dead
///    end — and nothing enters the session store.
///
/// Publishing is a separate call ([`publish_staged_skill`]) that still
/// requires the operator's approval token; this function never crosses
/// that boundary.
pub async fn draft_skill_for_entry(
    store: &BacklogStore,
    session: &Arc<ComposerSession>,
    entry_id: &str,
    proposal: &SkillProposal,
) -> Result<StagedSkill> {
    let entry = store.get(entry_id).ok_or_else(|| {
        RustyError::Tool(format!("unknown backlog entry `{entry_id}`"))
    })?;
    if entry.status != BacklogStatus::Approved {
        return Err(RustyError::Tool(format!(
            "backlog entry `{entry_id}` is {:?}, not approved — the harness proposes, the \
             backlog disposes, and only then does the composer draft",
            entry.status
        )));
    }
    let catalog = capability_catalog();
    let skill_shaped = entry
        .gap_ids
        .iter()
        .any(|gap| matches!(catalog_entry(&catalog, gap), Some(c) if c.build == BuildShape::Skill));
    if !skill_shaped {
        return Err(RustyError::Tool(format!(
            "backlog entry `{entry_id}` addresses no skill-shaped gap — gaps that close through \
             a core stream or a tool definition are not drafted as skills"
        )));
    }

    let receipt = ComposeSkillTool::new(Arc::clone(session))
        .call(json!({
            "name": proposal.name,
            "description": proposal.description,
            "body": proposal.body,
            "references": proposal.references,
            "author": proposal.author,
        }))
        .await?;
    if receipt["valid"] != json!(true) {
        return Err(RustyError::Tool(format!(
            "the composer refused the draft for backlog entry `{entry_id}`: {}",
            serde_json::to_string(&receipt["suggested_revision_notes"])
                .expect("revision notes serialize")
        )));
    }
    let content_hash = receipt["content_hash"]
        .as_str()
        .expect("a valid receipt names the content hash")
        .to_owned();
    Ok(StagedSkill {
        entry_id: entry.id,
        publish_effect_id: publish_effect_id(session.scope(), &content_hash),
        content_hash,
    })
}

/// Publish a staged draft — the only step that crosses the registry trust
/// boundary, and it crosses it exactly the way the composer defines:
/// behind an [`ApprovalToken`] scoped to the staged publish effect id.
///
/// `None` is not a convenience default; it is a refusal. Self-improvement
/// queues behind the gate, it never walks around it.
pub async fn publish_staged_skill(
    session: &Arc<ComposerSession>,
    registry: &Arc<Mutex<SkillRegistry>>,
    staged: &StagedSkill,
    approval: Option<&ApprovalToken>,
) -> Result<Value> {
    let token = approval.ok_or_else(|| {
        RustyError::Tool(format!(
            "refusing to publish `{}` for backlog entry `{}` without an approval token — \
             self-improvement stages, the operator disposes",
            staged.content_hash, staged.entry_id
        ))
    })?;
    PublishComposedSkillTool::new(Arc::clone(session), Arc::clone(registry))
        .call(json!({
            "content_hash": staged.content_hash,
            "approval": serde_json::to_value(token)
                .map_err(|e| RustyError::Tool(format!("approval token must serialize: {e}")))?,
        }))
        .await
}

// --------------------------------------------------------------------- //
// Tools — the loop as journaled tool calls
// --------------------------------------------------------------------- //

/// `inspect_capabilities` — assemble the snapshot through the host's probe
/// closure, run the catalog over it, return the [`GapReport`].
///
/// The closure is the honesty seam: the tool cannot see anything the host
/// did not wire into it, so the report is exactly as strong as the host's
/// assembly. [`Effect::ReadOnly`]: it reads live registries and changes
/// nothing.
pub struct InspectCapabilitiesTool {
    inspect: Arc<dyn Fn() -> CapabilityInspection + Send + Sync>,
}

impl std::fmt::Debug for InspectCapabilitiesTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InspectCapabilitiesTool").finish()
    }
}

impl InspectCapabilitiesTool {
    /// An inspection tool reading through `inspect` — the host's assembly
    /// of its own registries, called fresh on every invocation so the
    /// report reflects now.
    pub fn new(inspect: Arc<dyn Fn() -> CapabilityInspection + Send + Sync>) -> Self {
        Self { inspect }
    }
}

#[async_trait]
impl Tool for InspectCapabilitiesTool {
    fn name(&self) -> &str {
        "inspect_capabilities"
    }

    fn description(&self) -> &str {
        "Probe the harness's capability catalog against a live inspection snapshot and return \
         the gap report: present/partial/absent counts and every capability's status, in \
         catalog order."
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "additionalProperties": false})
    }

    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }

    async fn call(&self, _args: Value) -> Result<Value> {
        let report = assess(&capability_catalog(), &(self.inspect)());
        serde_json::to_value(report)
            .map_err(|e| RustyError::Tool(format!("gap report did not serialize: {e}")))
    }
}

/// `propose_backlog_entries` — record harness-proposed backlog entries for
/// named gaps. Provenance is always [`BacklogProvenance::HarnessSelfImprove`]
/// — the tool cannot impersonate an operator — and gap ids are checked
/// against the catalog before anything is written, so a typo'd proposal
/// fails closed rather than filing work against a gap that does not exist.
///
/// [`Effect::Idempotent`]: entry ids are content-derived, so re-proposing
/// the same entries converges on the same backlog.
pub struct ProposeBacklogTool {
    store: Arc<BacklogStore>,
    clock: Clock,
}

impl std::fmt::Debug for ProposeBacklogTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProposeBacklogTool")
            .field("clock", &self.clock)
            .finish()
    }
}

impl ProposeBacklogTool {
    /// A proposing tool writing to `store`, timestamping through `clock`
    /// (the injected clock seam — logical in tests and demos).
    pub fn new(store: Arc<BacklogStore>, clock: Clock) -> Self {
        Self { store, clock }
    }
}

#[async_trait]
impl Tool for ProposeBacklogTool {
    fn name(&self) -> &str {
        "propose_backlog_entries"
    }

    fn description(&self) -> &str {
        "Record backlog entries for catalog gaps, with harness:self-improve provenance. \
         Entries land as `proposed`; approval is a separate, operator act."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "entries": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_PROPOSE_ENTRIES,
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": {"type": "string", "maxLength": MAX_BACKLOG_TITLE_BYTES},
                            "rationale": {"type": "string", "maxLength": MAX_BACKLOG_RATIONALE_BYTES},
                            "gap_ids": {
                                "type": "array",
                                "minItems": 1,
                                "maxItems": MAX_BACKLOG_GAPS,
                                "items": {"type": "string"}
                            }
                        },
                        "required": ["title", "rationale", "gap_ids"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["entries"],
            "additionalProperties": false
        })
    }

    fn effect(&self) -> Effect {
        Effect::Idempotent
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let raw = args
            .get("entries")
            .and_then(Value::as_array)
            .ok_or_else(|| RustyError::Tool("`entries` must be an array".to_owned()))?;
        if raw.is_empty() || raw.len() > MAX_PROPOSE_ENTRIES {
            return Err(RustyError::Tool(format!(
                "propose_backlog_entries takes between 1 and {MAX_PROPOSE_ENTRIES} entries"
            )));
        }
        let catalog = capability_catalog();
        let mut entries = Vec::with_capacity(raw.len());
        for (index, value) in raw.iter().enumerate() {
            let text = |field: &str| -> Result<String> {
                value
                    .get(field)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        RustyError::Tool(format!("entries[{index}].`{field}` must be a string"))
                    })
            };
            let gap_ids: Vec<String> = value
                .get("gap_ids")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    RustyError::Tool(format!("entries[{index}].`gap_ids` must be an array"))
                })?
                .iter()
                .map(|gap| {
                    gap.as_str().map(str::to_owned).ok_or_else(|| {
                        RustyError::Tool(format!("entries[{index}].`gap_ids` must be strings"))
                    })
                })
                .collect::<Result<_>>()?;
            for gap in &gap_ids {
                if catalog_entry(&catalog, gap).is_none() {
                    return Err(RustyError::Tool(format!(
                        "entries[{index}] names gap `{gap}`, which the capability catalog does \
                         not know — nothing was recorded"
                    )));
                }
            }
            entries.push(BacklogEntry::new(
                text("title")?,
                text("rationale")?,
                &gap_ids,
                BacklogProvenance::HarnessSelfImprove,
                self.clock.now(),
            )?);
        }
        let mut recorded = Vec::with_capacity(entries.len());
        for entry in entries {
            let inserted = self.store.insert(entry.clone()).await?;
            recorded.push(json!({
                "id": entry.id,
                "title": entry.title,
                "gap_ids": entry.gap_ids,
                "status": entry.status,
                "provenance": entry.provenance.label(),
                // A re-proposal converges on the existing entry; report it
                // honestly rather than claiming a fresh record.
                "inserted": inserted,
            }));
        }
        Ok(json!({"recorded": recorded}))
    }
}

/// `build_gap_skill` — the journaled self-build step: for one *approved*
/// backlog entry addressing a skill-shaped gap, draft the proposed skill
/// through the composer and stage the publish. Returns the staged content
/// hash and the publish effect id an operator approval must name.
///
/// [`Effect::Pure`]: like `compose_skill`, the receipt is a deterministic
/// function of the input and the session draft store is content-addressed
/// scratch; the backlog read changes the verdict, never the state.
pub struct BuildGapSkillTool {
    store: Arc<BacklogStore>,
    session: Arc<ComposerSession>,
}

impl std::fmt::Debug for BuildGapSkillTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuildGapSkillTool")
            .field("session", &self.session)
            .finish()
    }
}

impl BuildGapSkillTool {
    /// A self-build tool reading `store` and drafting into `session`.
    pub fn new(store: Arc<BacklogStore>, session: Arc<ComposerSession>) -> Self {
        Self { store, session }
    }
}

#[async_trait]
impl Tool for BuildGapSkillTool {
    fn name(&self) -> &str {
        "build_gap_skill"
    }

    fn description(&self) -> &str {
        "Draft a skill closing an approved, skill-shaped backlog gap through the composer and \
         stage the publish behind its approval-gated effect id. Requires an approved backlog \
         entry for the gap; publishing itself stays with the operator."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "gap_id": {"type": "string"},
                "name": {"type": "string", "maxLength": 64},
                "description": {"type": "string", "maxLength": 1024},
                "body": {"type": "string", "maxLength": 262144},
                "references": {
                    "type": "object",
                    "additionalProperties": {"type": "string"}
                },
                "author": {"type": "string", "maxLength": 128}
            },
            "required": ["gap_id", "name", "description", "body", "author"],
            "additionalProperties": false
        })
    }

    fn effect(&self) -> Effect {
        Effect::Pure
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let string = |field: &str| -> Result<String> {
            args.get(field)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| RustyError::Tool(format!("`{field}` must be a string")))
        };
        let gap_id = string("gap_id")?;
        let catalog = capability_catalog();
        let capability = catalog_entry(&catalog, &gap_id).ok_or_else(|| {
            RustyError::Tool(format!(
                "gap `{gap_id}` is not in the capability catalog"
            ))
        })?;
        if capability.build != BuildShape::Skill {
            return Err(RustyError::Tool(format!(
                "gap `{gap_id}` closes through a {:?}, not a composed skill",
                capability.build
            )));
        }
        let approved: Vec<BacklogEntry> = self
            .store
            .list()
            .into_iter()
            .filter(|entry| {
                entry.status == BacklogStatus::Approved && entry.gap_ids.iter().any(|g| g == &gap_id)
            })
            .collect();
        let entry = match approved.as_slice() {
            [entry] => entry.clone(),
            [] => {
                return Err(RustyError::Tool(format!(
                    "no approved backlog entry addresses gap `{gap_id}` — the harness proposes, \
                     the operator approves, and only then does the composer draft"
                )));
            }
            _ => {
                return Err(RustyError::Tool(format!(
                    "{} approved backlog entries address gap `{gap_id}`; refuse to guess which \
                     one to build",
                    approved.len()
                )));
            }
        };

        let references: BTreeMap<String, String> = match args.get("references") {
            Some(value) => serde_json::from_value(value.clone()).map_err(|error| {
                RustyError::Tool(format!("`references` must map paths to strings: {error}"))
            })?,
            None => BTreeMap::new(),
        };
        let proposal = SkillProposal {
            name: string("name")?,
            description: string("description")?,
            body: string("body")?,
            references,
            author: string("author")?,
        };
        let staged = draft_skill_for_entry(&self.store, &self.session, &entry.id, &proposal).await?;
        Ok(json!({
            "entry_id": staged.entry_id,
            "entry_status": "approved",
            "content_hash": staged.content_hash,
            "publish_effect_id": staged.publish_effect_id,
            "publish_gate": "staged only — publishing requires an operator approval token \
                             scoped to publish_effect_id",
        }))
    }
}
