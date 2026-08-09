//! The capsule manifest contract (R0.9 Rusty Capsules, wave 1): the
//! content-addressed declaration of what untrusted code may reach, and the
//! journaled payloads that make its enforcement attributable.
//!
//! The design doc is `docs/capsules-design.md`. Its capsule rule governs
//! everything here: **no code the runtime does not trust may reach the
//! filesystem, the network, a secret, the clock, a model, or another tool
//! unless its manifest declared that reach — and every capability use and
//! every denied attempt is journaled, budgeted, and attributable to the
//! exact manifest grant that allowed or refused it.** This module is the
//! declaration half; the capability host (`capsule_host`, feature `wasm`)
//! is the enforcement half; the registry (`rusty-agent-server`) is the
//! resolution half.
//!
//! - [`CapsuleManifest`] — the serde-versioned, golden-pinned declaration:
//!   identity, version, build digest, declared interface (a WIT world
//!   reference), the closed [`Effect`] classes the capsule may produce,
//!   the [`CapabilityGrant`] set (the whole reach), and the
//!   [`ResourceBudget`]. Identity is integrity: [`derive_capsule_id`] is
//!   `sha256` over the canonical content (the one hashing primitive shared
//!   with artifact references, journal heads, and candidate ids), so two
//!   builds of the same declaration converge on one [`CapsuleId`] and a
//!   tampered manifest fails its own address.
//! - [`CapabilityGrant`] — the closed set of grants. The host maps grants
//!   onto which of the declared world's imports it links: an ungranted
//!   capability is an import that does not exist (structural denial, not a
//!   runtime check), and a grant narrower than the world is enforced
//!   inside the host's import implementation. A manifest with an empty
//!   `capabilities` set — the default — describes a pure-compute guest,
//!   which is precisely what `wasm_node`'s ABI v0 already executes.
//! - [`CapsuleResolution`] / [`CapsuleUse`] / [`CapsuleDenial`] — the
//!   journaled payloads. Resolutions carry the registry's answer to a
//!   `RunManifest` capsule pin; uses carry a granted operation's summaries;
//!   denials name **the manifest grant that was absent**, so a refusal is
//!   attributable to a declaration, not to a stack trace. They journal
//!   through the additive [`RunEventKind`](crate::record::RunEventKind)
//!   variants `CapsuleResolved` / `CapsuleCall` / `CapsuleDenied` — the
//!   same evolution rule every variant since R0.6's `EffectReceipt`
//!   followed; old journals keep deserializing.
//! - [`CapsuleOverlay`] / [`intersect_grants`] / [`grants_beyond`] (R0.9
//!   wave 2) — the tenant-overlay contract: a grant ceiling whose
//!   application is structural intersection (no code path computes
//!   union), plus the advisory widening signal the server's Cedar
//!   overlay decision consumes. Wave 2 also extended the journaled
//!   payloads additively: resolutions pin the deciding policy version,
//!   the applied overlays, the effective grants, and the clamped budget;
//!   denials pin the policy version whose verdict refused the attempt.
//!
//! Golden-file tests under `tests/golden/` pin every wire shape in this
//! module; any accidental contract drift fails CI.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Result, RustyError};
use crate::record::{canonicalize_value, sha256_hex, CapsuleVersion, Effect};

fn invalid(message: impl Into<String>) -> RustyError {
    // Manifest validation is a contract check at a state boundary; the
    // invalid-update class covers it rather than growing the error
    // taxonomy for one module (the memory/learn convention).
    RustyError::InvalidUpdate(message.into())
}

/// The one WIT world R0.9 wave 1 instantiates against:
/// `rusty:capsule/world@0.1.0`. Worlds evolve additively — a later world
/// version may add imports or tighten types; old world versions keep
/// instantiating — so this constant joins a list rather than being
/// replaced when a second world lands.
pub const WORLD_V1: &str = "rusty:capsule/world@0.1.0";

/// The world versions the host admits, oldest first. Wave 1 ships exactly
/// one (the design's open question 6 owns the evolution rule for the
/// second).
pub const SUPPORTED_WORLDS: &[&str] = &[WORLD_V1];

/// A content-addressed capsule identity: lowercase hex SHA-256 over the
/// manifest's canonical content ([`derive_capsule_id`]).
///
/// Transparent newtype so the type system — not convention — keeps
/// capsule ids distinct from memory addresses, candidate ids, and other
/// digest strings (the [`CandidateId`](crate::learn::CandidateId)
/// precedent).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapsuleId(String);

impl CapsuleId {
    /// The hex digest.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CapsuleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for CapsuleId {
    /// Wrap a digest the caller already trusts — rehydration from storage,
    /// a resolution's answer. Minting ids from content is
    /// [`derive_capsule_id`]'s job alone; nothing here re-verifies.
    fn from(digest: String) -> Self {
        Self(digest)
    }
}

/// The capsule's name plus human-facing metadata. Not the address — the
/// address is derived from the whole declaration, never minted from the
/// name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsuleIdentity {
    /// The capsule's name — the key `RunManifest::capsules` pins versions
    /// under, and half of the registry's `(identity, version)` mapping.
    pub name: String,

    /// Human-facing description. Metadata, not identity: it travels inside
    /// the content address but is read by no enforcement point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The declared graph/node interface: the WIT world the component was
/// built against, plus the typed input/output the world exports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapsuleInterface {
    /// The WIT world reference (`rusty:capsule/world@x.y.z`). Admission
    /// accepts exactly the versions in [`SUPPORTED_WORLDS`] — an unknown
    /// world is a capsule from another interface era, refused rather than
    /// guessed at.
    pub world: String,

    /// JSON Schema (draft 2020-12, the dialect
    /// [`crate::durable::ArtifactContract::schema`] pinned in R0.7)
    /// describing the guest's input. Wave 1 pins the shape — the
    /// declaration is part of the content address, so schema drift is
    /// visible to every consumer — and the host enforces the structural
    /// half (the canonical ABI itself types the export); full draft-2020-12
    /// validation lands with the wave that adopts a validator, the same
    /// staging `ArtifactContract::schema` documented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,

    /// JSON Schema describing the guest's output, same staging as
    /// [`CapsuleInterface::input_schema`]. The host's output gate enforces
    /// well-formed JSON and `max_output_bytes` in wave 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
}

/// Filesystem access mode of a [`CapabilityGrant::Filesystem`] grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemMode {
    /// Read-only access under the granted prefixes.
    Read,
    /// Read and write access under the granted prefixes.
    ReadWrite,
}

/// One capability grant: the closed set of things a capsule may reach.
/// Serialized with internal tagging (`{"kind": "network", …}`) so the
/// grant's kind is part of the manifest's content address — a filesystem
/// grant and a network grant that happened to carry alike lists must
/// never converge (the [`crate::learn::CandidateContent`] rule).
///
/// The set is the whole reach. Grants narrower than the world (a
/// `network` grant naming one hostname) are enforced inside the host's
/// import implementation: the import exists, but the host matches the
/// grant's scope before anything executes, and a mismatch is a journaled
/// [`CapsuleDenial`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapabilityGrant {
    /// WASI-style preopened directories scoped to the granted path
    /// prefixes; nothing else on the filesystem exists for the guest.
    Filesystem {
        /// Absolute path prefixes the guest may touch.
        paths: Vec<String>,
        /// Read-only or read-write.
        mode: FilesystemMode,
    },

    /// Outbound calls through the host-side connector, matched on
    /// host + protocol + method before any socket opens (Deno's scoping).
    Network {
        /// Hostnames the guest may call.
        hosts: Vec<String>,
        /// Protocols the guest may speak (`https`, `http`, `wss`, …).
        protocols: Vec<String>,
        /// HTTP methods the guest may use (`GET`, `POST`, …).
        methods: Vec<String>,
    },

    /// Secret *handles* — names in the server's secret store. The guest
    /// receives opaque tokens; the host resolves them at use and the bytes
    /// never enter guest linear memory.
    Secret {
        /// The handles the guest may name.
        handles: Vec<String>,
    },

    /// Tools in the run's `ToolRegistry`. The guest's tool-call import is
    /// linked, dispatching through the host's tool executor — so the
    /// effect kernel's admission path applies unchanged.
    Tool {
        /// The tool names the guest may call.
        tools: Vec<String>,
    },

    /// Models the deployment serves. Usage accrues to the capsule's
    /// budget.
    Model {
        /// The model names the guest may call.
        models: Vec<String>,
    },

    /// Read the wall/monotonic clock through the host's clock import. A
    /// grant rather than ambient authority because the clock is a
    /// determinism boundary: a guest that can read wall time can branch on
    /// evidence the journal does not hold.
    Clock,
}

/// The capability class a [`CapabilityGrant`] (or an import of the
/// declared world) belongs to. Closed enum — the host maps world imports
/// onto kinds exhaustively, and an import naming no known kind fails
/// closed at instantiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    /// The filesystem.
    Filesystem,
    /// The network.
    Network,
    /// The secret store.
    Secret,
    /// Another tool.
    Tool,
    /// A model.
    Model,
    /// The clock.
    Clock,
}

impl CapabilityGrant {
    /// The capability class of this grant.
    pub fn capability_kind(&self) -> CapabilityKind {
        match self {
            CapabilityGrant::Filesystem { .. } => CapabilityKind::Filesystem,
            CapabilityGrant::Network { .. } => CapabilityKind::Network,
            CapabilityGrant::Secret { .. } => CapabilityKind::Secret,
            CapabilityGrant::Tool { .. } => CapabilityKind::Tool,
            CapabilityGrant::Model { .. } => CapabilityKind::Model,
            CapabilityGrant::Clock => CapabilityKind::Clock,
        }
    }

    /// The minimum [`Effect`] severity exercising this grant implies. The
    /// manifest's declared `effects` must top out at or above it — a
    /// capsule whose declared classes reach only `ReadOnly` is refused at
    /// admission when its grants imply writes (the declaration and the
    /// grants must agree; the host enforces the stricter of the two).
    ///
    /// The conservative readings are deliberate: a filesystem write is not
    /// idempotency-keyed, a tool call defaults to the class the runtime
    /// cannot prove otherwise, and a model call is `NonIdempotent`
    /// everywhere else in the taxonomy.
    pub fn implied_effect(&self) -> Effect {
        match self {
            CapabilityGrant::Filesystem { mode, .. } => match mode {
                FilesystemMode::Read => Effect::ReadOnly,
                FilesystemMode::ReadWrite => Effect::NonIdempotent,
            },
            CapabilityGrant::Network { methods, .. } => {
                if methods.iter().all(|m| is_read_only_method(m)) {
                    Effect::ReadOnly
                } else {
                    Effect::NonIdempotent
                }
            }
            CapabilityGrant::Secret { .. } => Effect::ReadOnly,
            CapabilityGrant::Tool { .. } => Effect::NonIdempotent,
            CapabilityGrant::Model { .. } => Effect::NonIdempotent,
            CapabilityGrant::Clock => Effect::ReadOnly,
        }
    }
}

/// `true` when `method` is a read-only HTTP method (no write semantics).
/// Shared by the grant's implied-effect computation and the host's
/// per-call effect classification of a granted fetch.
pub fn is_read_only_method(method: &str) -> bool {
    matches!(method, "GET" | "HEAD" | "OPTIONS")
}

/// `true` when any `network` grant in `grants` covers this exact call —
/// union semantics across grants (a grant set is the whole reach, so any
/// single covering grant permits). Called by the host's connector before
/// anything executes; a `false` answer is a journaled denial, never a
/// silent refusal.
pub fn network_grant_covers<'a>(
    grants: impl IntoIterator<Item = &'a CapabilityGrant>,
    protocol: &str,
    host: &str,
    method: &str,
) -> bool {
    grants.into_iter().any(|grant| match grant {
        CapabilityGrant::Network {
            hosts,
            protocols,
            methods,
        } => {
            hosts.iter().any(|h| h == host)
                && protocols.iter().any(|p| p == protocol)
                && methods.iter().any(|m| m == method)
        }
        _ => false,
    })
}

/// `true` when any grant in `grants` belongs to `kind` — the structural
/// question (should the import exist at all), distinct from the scoped
/// question [`network_grant_covers`] answers.
pub fn any_grant_of_kind<'a>(
    grants: impl IntoIterator<Item = &'a CapabilityGrant>,
    kind: CapabilityKind,
) -> bool {
    grants
        .into_iter()
        .any(|grant| grant.capability_kind() == kind)
}

// --------------------------------------------------------------------- //
// Tenant overlays (R0.9 wave 2)
// --------------------------------------------------------------------- //

/// A tenant overlay (R0.9 wave 2): the grant ceiling an operator attaches
/// to a tenant, further restricting what capsules in that tenant may do.
/// The contract half of the overlay plane lives here (golden-pinned, like
/// every wire shape); the authorization half — *who may author* an
/// overlay, against which capsules — is the server's Cedar decision
/// (`rusty-server/src/capsule_policy.rs`, feature `capsules`).
///
/// The narrowing rule is enforced twice, and the structural half comes
/// first: applying an overlay is [`intersect_grants`], a set intersection
/// that mechanically cannot add a grant; Cedar then decides the overlay's
/// legality. Policy decides legality; arithmetic decides narrowing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapsuleOverlay {
    /// The overlay's name — its registry key within the tenant.
    pub name: String,

    /// The capsule identity names the overlay applies to. `None` (the
    /// sparse wire shape) means every capsule in the tenant; an empty list
    /// is refused at validation for the same reason an empty grant scope
    /// is — an overlay applying to nothing carries no meaning worth
    /// storing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub targets: Option<Vec<String>>,

    /// The grant ceiling. The effective capability set of a manifest
    /// under this overlay is `intersect_grants(manifest, ceiling)` —
    /// never the manifest's grants alone and never the ceiling's alone.
    pub capabilities: BTreeSet<CapabilityGrant>,

    /// Human-facing context (why the ceiling exists). Metadata, read by
    /// no enforcement point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl CapsuleOverlay {
    /// Contract validation, run at every attach boundary: the name and
    /// any targets are in the identifier grammar, and every ceiling grant
    /// passes the manifest's per-grant rule.
    pub fn validate(&self) -> Result<()> {
        validate_token("overlay name", &self.name)?;
        if let Some(targets) = &self.targets {
            if targets.is_empty() {
                return Err(invalid(
                    "overlay targets: the list is empty — an overlay applying to no capsule \
                     carries no meaning (omit the field to target every capsule)",
                ));
            }
            for target in targets {
                validate_token("overlay target", target)?;
            }
        }
        for grant in &self.capabilities {
            validate_grant(grant)?;
        }
        Ok(())
    }

    /// `true` when this overlay governs the capsule named `capsule_name`
    /// (its identity name, the `RunManifest` pin key).
    pub fn applies_to(&self, capsule_name: &str) -> bool {
        self.targets
            .as_ref()
            .is_none_or(|targets| targets.iter().any(|target| target == capsule_name))
    }

    /// The effective grant set of `manifest_grants` under this overlay —
    /// [`intersect_grants`], the only composition this wave defines.
    pub fn effective_grants(
        &self,
        manifest_grants: &BTreeSet<CapabilityGrant>,
    ) -> BTreeSet<CapabilityGrant> {
        intersect_grants(manifest_grants, &self.capabilities)
    }
}

/// The overlay narrowing rule, reduced to one function (R0.9 wave 2):
/// the effective capability set of a manifest under an overlay is the
/// **intersection** of the two grant sets — every grant in the result is
/// a narrowing of one grant from *each* side, computed field-wise per
/// capability kind.
///
/// This is the only composition of two grant sets anywhere in the wave,
/// and it is deliberately written so union is unrepresentable: a reviewer
/// verifying "overlays cannot widen" reads this function and nothing
/// else. Grants of different kinds never meet (a network grant and a
/// clock grant share no member), and a same-kind pair whose scopes share
/// nothing contributes nothing — an overlay naming only hosts the
/// manifest never granted *removes* the capability rather than adding
/// the hosts. A grant whose intersection empties one scope list drops
/// out entirely: a `network` grant with no common hosts permits no call,
/// so carrying it would be the empty-scope lie manifests refuse at
/// validation.
pub fn intersect_grants(
    manifest_grants: &BTreeSet<CapabilityGrant>,
    overlay_grants: &BTreeSet<CapabilityGrant>,
) -> BTreeSet<CapabilityGrant> {
    /// The shared scope items of two lists, sorted and deduplicated — the
    /// canonical form the manifest contract already pins.
    fn common(a: &[String], b: &[String]) -> Vec<String> {
        let mut out: Vec<String> = a.iter().filter(|item| b.contains(item)).cloned().collect();
        out.sort();
        out.dedup();
        out
    }
    let mut out = BTreeSet::new();
    for manifest_grant in manifest_grants {
        for overlay_grant in overlay_grants {
            let shared = match (manifest_grant, overlay_grant) {
                (
                    CapabilityGrant::Filesystem {
                        paths: manifest_paths,
                        mode: manifest_mode,
                    },
                    CapabilityGrant::Filesystem {
                        paths: overlay_paths,
                        mode: overlay_mode,
                    },
                ) => {
                    let paths = common(manifest_paths, overlay_paths);
                    // Read is the narrower mode (declaration order), so
                    // `min` can only keep or tighten access.
                    (!paths.is_empty()).then(|| CapabilityGrant::Filesystem {
                        paths,
                        mode: (*manifest_mode).min(*overlay_mode),
                    })
                }
                (
                    CapabilityGrant::Network {
                        hosts: manifest_hosts,
                        protocols: manifest_protocols,
                        methods: manifest_methods,
                    },
                    CapabilityGrant::Network {
                        hosts: overlay_hosts,
                        protocols: overlay_protocols,
                        methods: overlay_methods,
                    },
                ) => {
                    let hosts = common(manifest_hosts, overlay_hosts);
                    let protocols = common(manifest_protocols, overlay_protocols);
                    let methods = common(manifest_methods, overlay_methods);
                    (!hosts.is_empty() && !protocols.is_empty() && !methods.is_empty()).then_some(
                        CapabilityGrant::Network {
                            hosts,
                            protocols,
                            methods,
                        },
                    )
                }
                (
                    CapabilityGrant::Secret {
                        handles: manifest_handles,
                    },
                    CapabilityGrant::Secret {
                        handles: overlay_handles,
                    },
                ) => {
                    let handles = common(manifest_handles, overlay_handles);
                    (!handles.is_empty()).then_some(CapabilityGrant::Secret { handles })
                }
                (
                    CapabilityGrant::Tool {
                        tools: manifest_tools,
                    },
                    CapabilityGrant::Tool {
                        tools: overlay_tools,
                    },
                ) => {
                    let tools = common(manifest_tools, overlay_tools);
                    (!tools.is_empty()).then_some(CapabilityGrant::Tool { tools })
                }
                (
                    CapabilityGrant::Model {
                        models: manifest_models,
                    },
                    CapabilityGrant::Model {
                        models: overlay_models,
                    },
                ) => {
                    let models = common(manifest_models, overlay_models);
                    (!models.is_empty()).then_some(CapabilityGrant::Model { models })
                }
                (CapabilityGrant::Clock, CapabilityGrant::Clock) => Some(CapabilityGrant::Clock),
                _ => None,
            };
            if let Some(grant) = shared {
                out.insert(grant);
            }
        }
    }
    out
}

/// The overlay grants asking for reach no manifest grant declares (R0.9
/// wave 2) — the advisory widening signal the server's Cedar overlay
/// decision consumes. Coverage is decided per capability kind against
/// the union of the manifest's scope items of that kind: a manifest
/// granting host A in one grant and host B in another covers an overlay
/// grant naming both, and a kind the manifest never granted leaves every
/// overlay grant of that kind beyond.
///
/// This is a *signal*, never an enforcement — it exists so an operator's
/// Cedar policy can refuse overlays that ask for more than a manifest
/// declared. The enforcement is [`intersect_grants`], exact and pairwise,
/// and it needs no signal: hand-crafting an overlay past the policy
/// plane still cannot widen the effective set.
pub fn grants_beyond(
    overlay_grants: &BTreeSet<CapabilityGrant>,
    manifest_grants: &BTreeSet<CapabilityGrant>,
) -> BTreeSet<CapabilityGrant> {
    /// `true` when every item of `needed` appears in some same-kind
    /// manifest grant's corresponding scope list.
    fn covered<'a>(
        needed: &[String],
        offered: impl Iterator<Item = &'a Vec<String>> + Clone,
    ) -> bool {
        needed
            .iter()
            .all(|item| offered.clone().any(|list| list.contains(item)))
    }
    overlay_grants
        .iter()
        .filter(|overlay_grant| {
            let same_kind = manifest_grants
                .iter()
                .filter(|m| m.capability_kind() == overlay_grant.capability_kind());
            let covered = match overlay_grant {
                CapabilityGrant::Filesystem { paths, mode } => {
                    let mode_covered = same_kind.clone().any(|m| match m {
                        CapabilityGrant::Filesystem {
                            mode: manifest_mode,
                            ..
                        } => manifest_mode >= mode,
                        _ => false,
                    });
                    mode_covered
                        && covered(
                            paths,
                            same_kind.filter_map(|m| match m {
                                CapabilityGrant::Filesystem { paths, .. } => Some(paths),
                                _ => None,
                            }),
                        )
                }
                CapabilityGrant::Network {
                    hosts,
                    protocols,
                    methods,
                } => {
                    let networks = same_kind.filter_map(|m| match m {
                        CapabilityGrant::Network {
                            hosts,
                            protocols,
                            methods,
                        } => Some((hosts, protocols, methods)),
                        _ => None,
                    });
                    covered(hosts, networks.clone().map(|(h, _, _)| h))
                        && covered(protocols, networks.clone().map(|(_, p, _)| p))
                        && covered(methods, networks.map(|(_, _, m)| m))
                }
                CapabilityGrant::Secret { handles } => covered(
                    handles,
                    same_kind.filter_map(|m| match m {
                        CapabilityGrant::Secret { handles } => Some(handles),
                        _ => None,
                    }),
                ),
                CapabilityGrant::Tool { tools } => covered(
                    tools,
                    same_kind.filter_map(|m| match m {
                        CapabilityGrant::Tool { tools } => Some(tools),
                        _ => None,
                    }),
                ),
                CapabilityGrant::Model { models } => covered(
                    models,
                    same_kind.filter_map(|m| match m {
                        CapabilityGrant::Model { models } => Some(models),
                        _ => None,
                    }),
                ),
                CapabilityGrant::Clock => same_kind.count() > 0,
            };
            !covered
        })
        .cloned()
        .collect()
}

/// The resource budget a capsule invocation may consume. Every field is
/// optional on the wire; `None` means the enclosing run's own budget
/// bounds apply — never an invented default, per the `AgentBudget`
/// convention.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResourceBudget {
    /// CPU budget in Wasmtime fuel units (deterministic, replay-stable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fuel: Option<u64>,

    /// Linear-memory cap in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory_bytes: Option<u64>,

    /// Wall-time bound in milliseconds, enforced by epoch interruption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_time_ms: Option<u64>,

    /// Token budget for model usage (evidence-grade, matching
    /// `AgentBudget`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,

    /// Cost budget in USD for model usage (evidence-grade `f64`, matching
    /// `AgentBudget`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,

    /// Upper bound on the guest's result payload in bytes, checked before
    /// the host accepts the output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_bytes: Option<u64>,
}

impl ResourceBudget {
    /// Field-wise minimum against an enclosing scope's ceiling: the
    /// effective budget is never wider than what either layer declared.
    /// `None` in one layer means that layer imposes no bound, so the other
    /// layer's bound (if any) applies. This is the admission clamp the
    /// design names — capsule budget ≤ the run's budgets ≤ tenant quotas
    /// — reduced to its wave-1 pair; the quota layers compose the same
    /// way when they arrive.
    pub fn clamp(&self, ceiling: &ResourceBudget) -> ResourceBudget {
        fn min_opt<T: Ord + Copy>(a: Option<T>, b: Option<T>) -> Option<T> {
            match (a, b) {
                (Some(x), Some(y)) => Some(x.min(y)),
                (a, b) => a.or(b),
            }
        }
        ResourceBudget {
            fuel: min_opt(self.fuel, ceiling.fuel),
            max_memory_bytes: min_opt(self.max_memory_bytes, ceiling.max_memory_bytes),
            wall_time_ms: min_opt(self.wall_time_ms, ceiling.wall_time_ms),
            max_tokens: min_opt(self.max_tokens, ceiling.max_tokens),
            max_cost_usd: match (self.max_cost_usd, ceiling.max_cost_usd) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
            max_output_bytes: min_opt(self.max_output_bytes, ceiling.max_output_bytes),
        }
    }

    fn validate(&self) -> Result<()> {
        if let Some(cost) = self.max_cost_usd {
            if !cost.is_finite() || cost < 0.0 {
                return Err(invalid(format!(
                    "resource budget: max_cost_usd must be finite and non-negative, got {cost}"
                )));
            }
        }
        Ok(())
    }
}

/// The capsule manifest: one serde-versioned struct, additive-evolution
/// only, golden pinned — the same discipline as `MemoryRecord` and
/// `Candidate`.
///
/// The address ([`derive_capsule_id`]) covers the **canonical** content:
/// every grant's scope lists are sorted and deduplicated, and the grant
/// set itself has set semantics (`BTreeSet`), so two declarations that
/// agree on substance converge on one id regardless of the order their
/// authors happened to write lists in. Every other field exists because a
/// downstream enforcement point needs it (see the field docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapsuleManifest {
    /// Capsule name plus human-facing metadata. Not the address.
    pub identity: CapsuleIdentity,

    /// The exact version string — the value the R0.7 [`CapsuleVersion`]
    /// pin already records in `RunManifest::capsules`. The registry
    /// resolves `(identity, version)` → [`CapsuleId`] at admission, so the
    /// version string stays the wire pin (additive, no migration) while
    /// the journaled resolution reaches the content address.
    pub version: String,

    /// SHA-256 (lowercase hex) of the guest artifact — the `.wasm`
    /// component bytes. Admission recomputes it; a manifest naming bytes
    /// it was not built from does not load. The digest, not the version
    /// string, is what the host caches compiled components under.
    pub build_digest: String,

    /// The declared interface: the WIT world reference plus the typed
    /// input/output the world exports.
    pub interface: CapsuleInterface,

    /// The closed [`Effect`] classes the capsule may produce. Must agree
    /// with the capability grants (see [`CapsuleManifest::validate`]):
    /// declaring less than the grants imply is refused; declaring more is
    /// harmless — the host grants nothing the capabilities do not name.
    pub effects: BTreeSet<Effect>,

    /// The capability grants. The set is the whole reach; the empty set
    /// (the default) is the pure-compute guest.
    pub capabilities: BTreeSet<CapabilityGrant>,

    /// The declared resource budget. Clamped at admission against the
    /// enclosing run's bounds ([`ResourceBudget::clamp`]); the host
    /// enforces the result.
    #[serde(default, skip_serializing_if = "ResourceBudget::is_empty")]
    pub budget: ResourceBudget,
}

impl ResourceBudget {
    /// `true` when no bound is declared (the sparse wire shape).
    pub fn is_empty(&self) -> bool {
        self.fuel.is_none()
            && self.max_memory_bytes.is_none()
            && self.wall_time_ms.is_none()
            && self.max_tokens.is_none()
            && self.max_cost_usd.is_none()
            && self.max_output_bytes.is_none()
    }
}

/// The content address of a capsule manifest: `sha256` over the canonical
/// serialization of its content — the one hashing primitive shared with
/// artifact references, journal heads, and candidate ids, over the
/// canonical `serde_json` serialization
/// [`crate::record::PayloadRef::content_hash`] also relies on (payload maps
/// are key-sorted before hashing; see `canonicalize_value`).
pub fn derive_capsule_id(manifest: &CapsuleManifest) -> Result<CapsuleId> {
    let canonical = manifest.canonicalized();
    Ok(CapsuleId(sha256_hex(&serde_json::to_vec(&canonical)?)))
}

impl CapsuleManifest {
    /// The manifest's content address.
    pub fn capsule_id(&self) -> Result<CapsuleId> {
        derive_capsule_id(self)
    }

    /// The canonical form the content address covers: every scope list
    /// sorted and deduplicated, so list order carries no identity, and the
    /// interface schemas key-sorted ([`canonicalize_value`]), so the map
    /// backend a build ends up with carries no identity either.
    fn canonicalized(&self) -> CapsuleManifest {
        fn sorted(mut list: Vec<String>) -> Vec<String> {
            list.sort();
            list.dedup();
            list
        }
        let interface = CapsuleInterface {
            world: self.interface.world.clone(),
            input_schema: self.interface.input_schema.as_ref().map(canonicalize_value),
            output_schema: self
                .interface
                .output_schema
                .as_ref()
                .map(canonicalize_value),
        };
        let capabilities = self
            .capabilities
            .iter()
            .map(|grant| match grant {
                CapabilityGrant::Filesystem { paths, mode } => CapabilityGrant::Filesystem {
                    paths: sorted(paths.clone()),
                    mode: *mode,
                },
                CapabilityGrant::Network {
                    hosts,
                    protocols,
                    methods,
                } => CapabilityGrant::Network {
                    hosts: sorted(hosts.clone()),
                    protocols: sorted(protocols.clone()),
                    methods: sorted(methods.clone()),
                },
                CapabilityGrant::Secret { handles } => CapabilityGrant::Secret {
                    handles: sorted(handles.clone()),
                },
                CapabilityGrant::Tool { tools } => CapabilityGrant::Tool {
                    tools: sorted(tools.clone()),
                },
                CapabilityGrant::Model { models } => CapabilityGrant::Model {
                    models: sorted(models.clone()),
                },
                CapabilityGrant::Clock => CapabilityGrant::Clock,
            })
            .collect();
        CapsuleManifest {
            identity: self.identity.clone(),
            version: self.version.clone(),
            build_digest: self.build_digest.clone(),
            interface,
            effects: self.effects.clone(),
            capabilities,
            budget: self.budget.clone(),
        }
    }

    /// Contract validation, run at every admission boundary (host
    /// construction, registry write, resolution).
    ///
    /// Refuses: an unknown world version; a malformed build digest;
    /// grant scope lists that are empty or carry out-of-grammar tokens; a
    /// declared effect set that tops out below what the grants imply; a
    /// non-finite or negative cost budget. What passes here is a
    /// declaration the host can enforce without further interpretation.
    pub fn validate(&self) -> Result<()> {
        validate_token("capsule name", &self.identity.name)?;
        validate_token("capsule version", &self.version)?;
        if !SUPPORTED_WORLDS.contains(&self.interface.world.as_str()) {
            return Err(invalid(format!(
                "unsupported WIT world `{}` (this release instantiates: {})",
                self.interface.world,
                SUPPORTED_WORLDS.join(", ")
            )));
        }
        let digest_ok = self.build_digest.len() == 64
            && self
                .build_digest
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
        if !digest_ok {
            return Err(invalid(format!(
                "build digest `{}` is not a lowercase hex SHA-256",
                self.build_digest
            )));
        }
        if self.effects.is_empty() {
            return Err(invalid(
                "declared effects are empty — even a pure-compute capsule declares `pure`",
            ));
        }
        for grant in &self.capabilities {
            validate_grant(grant)?;
        }
        let declared_max = self.effects.iter().max().copied().unwrap_or(Effect::Pure);
        let implied_max = self
            .capabilities
            .iter()
            .map(CapabilityGrant::implied_effect)
            .max()
            .unwrap_or(Effect::Pure);
        if declared_max < implied_max {
            return Err(invalid(format!(
                "declared effects top out at {declared_max:?} but the capability grants imply \
                 {implied_max:?} — the declaration and the grants must agree"
            )));
        }
        self.budget.validate()?;
        Ok(())
    }
}

/// Identifier grammar for names that become registry keys, pin names, and
/// path segments: `[A-Za-z0-9._-]`, 1..=128 chars (the policy registry's
/// version rule).
fn validate_token(what: &str, value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !valid {
        return Err(invalid(format!(
            "invalid {what} `{value}` (allowed: [A-Za-z0-9._-], 1..=128 chars)"
        )));
    }
    Ok(())
}

/// Scope-list validation per grant variant: every list non-empty (an
/// empty scope is a grant that permits nothing — refuse it at the
/// boundary rather than carrying a meaningless grant), every token in
/// grammar.
fn validate_grant(grant: &CapabilityGrant) -> Result<()> {
    fn non_empty<'a>(what: &str, list: &'a [String]) -> Result<&'a [String]> {
        if list.is_empty() {
            return Err(invalid(format!("{what}: the scope list is empty")));
        }
        Ok(list)
    }
    match grant {
        CapabilityGrant::Filesystem { paths, .. } => {
            for path in non_empty("filesystem grant", paths)? {
                if !path.starts_with('/') || path.split('/').any(|seg| seg == "..") {
                    return Err(invalid(format!(
                        "filesystem grant path `{path}` must be absolute and free of `..`"
                    )));
                }
            }
        }
        CapabilityGrant::Network {
            hosts,
            protocols,
            methods,
        } => {
            for host in non_empty("network grant hosts", hosts)? {
                let valid = !host.is_empty()
                    && host.len() <= 253
                    && host
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'));
                if !valid {
                    return Err(invalid(format!(
                        "network grant host `{host}` (allowed: [A-Za-z0-9.-], 1..=253 chars)"
                    )));
                }
            }
            for protocol in non_empty("network grant protocols", protocols)? {
                let valid =
                    !protocol.is_empty() && protocol.chars().all(|c| c.is_ascii_lowercase());
                if !valid {
                    return Err(invalid(format!(
                        "network grant protocol `{protocol}` (lowercase ASCII, e.g. `https`)"
                    )));
                }
            }
            for method in non_empty("network grant methods", methods)? {
                let valid = !method.is_empty() && method.chars().all(|c| c.is_ascii_uppercase());
                if !valid {
                    return Err(invalid(format!(
                        "network grant method `{method}` (uppercase ASCII, e.g. `GET`)"
                    )));
                }
            }
        }
        CapabilityGrant::Secret { handles } => {
            for handle in non_empty("secret grant", handles)? {
                validate_token("secret handle", handle)?;
            }
        }
        CapabilityGrant::Tool { tools } => {
            for tool in non_empty("tool grant", tools)? {
                validate_token("tool name", tool)?;
            }
        }
        CapabilityGrant::Model { models } => {
            for model in non_empty("model grant", models)? {
                validate_token("model name", model)?;
            }
        }
        CapabilityGrant::Clock => {}
    }
    Ok(())
}

/// A journaled registry resolution (R0.9 wave 1): the answer to one
/// `RunManifest` capsule pin, recorded as the output of a
/// [`RunEventKind::CapsuleResolved`](crate::record::RunEventKind::CapsuleResolved)
/// event at admission. This is the link that lets a checkpoint's version
/// *string* pin reach the content *address*: header pin → journaled
/// resolution → receipt.
///
/// Wave 2 (Cedar, overlays, budget composition) extended the payload
/// additively: the four optional fields are absent — on the wire and in
/// old journals — whenever the admission ran without the policy plane
/// (a build without the feature, a tenant with no active policy, no
/// overlays, no budget bounds), which is exactly the wave-1 shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapsuleResolution {
    /// The pin's name (the `RunManifest::capsules` key — the capsule's
    /// identity name).
    pub name: String,

    /// The pinned version string.
    pub version: CapsuleVersion,

    /// The content address the registry resolved the pin to.
    pub capsule_id: CapsuleId,

    /// The build digest the resolved manifest declares — what the host
    /// will recompute against the artifact bytes before instantiating.
    pub build_digest: String,

    /// The Cedar policy version this admission was decided under (R0.9
    /// wave 2): the tenant's active policy version at resolve time,
    /// pinned so a later audit can answer "which policy permitted this"
    /// without reconstructing history. `None` when no policy was active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_version: Option<String>,

    /// The tenant overlays applied at admission (R0.9 wave 2), sorted by
    /// name. `None` when none applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlays: Option<Vec<String>>,

    /// The effective grant set after overlay intersection (R0.9 wave 2)
    /// — what the host may actually link, always a subset of the
    /// manifest's declared grants ([`intersect_grants`]). Present exactly
    /// when an overlay applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_grants: Option<BTreeSet<CapabilityGrant>>,

    /// The budget after clamping against the enclosing run's bounds and
    /// the deployment's tenant ceiling (R0.9 wave 2), present exactly
    /// when the clamp changed the declared budget — the journaled
    /// evidence of the wave's budget composition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clamped_budget: Option<ResourceBudget>,
}

/// A journaled capability use (R0.9 wave 1): one granted import call,
/// recorded as the output of a
/// [`RunEventKind::CapsuleCall`](crate::record::RunEventKind::CapsuleCall)
/// event. Summaries, not transcripts: the request carries the matched
/// scope (protocol, host, method, path) and the response the status and
/// body — the body is the guest's own data, and the journal's inline/artifact
/// split applies when it is large.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapsuleUse {
    /// The capsule that exercised the capability.
    pub capsule_id: CapsuleId,

    /// The capability class of the import that was called.
    pub capability: CapabilityKind,

    /// The operation within the capability (`fetch`, `now_millis`).
    pub operation: String,

    /// The request summary — for `fetch`: `{protocol, host, method, path}`
    /// (never the body: request bodies are the capsule's business, and
    /// keeping them out of the use record keeps evidence lean).
    pub request: Value,

    /// The response summary — for `fetch`: `{status, body}`; for
    /// `now_millis`: `{millis}`.
    pub response: Value,
}

/// A journaled capability denial (R0.9 wave 1): one refused attempt,
/// recorded as the output of a
/// [`RunEventKind::CapsuleDenied`](crate::record::RunEventKind::CapsuleDenied)
/// event.
///
/// The payload exists so the denial is **attributable**: it names the
/// capsule, the capability class, and the exact grant that was absent —
/// the grant that would have permitted the attempt. Two shapes:
///
/// - **Structural** ([`CapsuleDenial::unscoped`]): the guest probed an
///   import its manifest grants never linked. The absent grant's scope
///   lists are empty — no grant at any scope existed, so none can be
///   named. (Empty scopes are refused in manifests by
///   [`CapsuleManifest::validate`]; in a denial they carry the "no scope
///   at all" meaning.)
/// - **Scoped** ([`CapsuleDenial::scoped`]): the guest called a linked
///   import outside its scope. The absent grant names the scope that was
///   missing — granted host A, attempted host B, so the absent grant is
///   `network` scoped to host B.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapsuleDenial {
    /// The capsule whose attempt was refused.
    pub capsule_id: CapsuleId,

    /// The capability class that was absent.
    pub capability: CapabilityKind,

    /// The grant that would have permitted the attempt.
    pub absent_grant: CapabilityGrant,

    /// Human-facing context: what was attempted, against which granted
    /// scope.
    pub detail: String,

    /// The Cedar policy version whose verdict refused this attempt (R0.9
    /// wave 2). Present on authorization refusals — an admission refused
    /// by policy and a revoked grant denied at its next use (the
    /// revocation edge: the denial is journaled against the *new* policy
    /// version, not the version the capsule was admitted under). Absent
    /// on wave-1 scope denials, which are the manifest's own verdict and
    /// carry no policy context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_version: Option<String>,
}

impl CapsuleDenial {
    /// The structural denial: the capability class is absent entirely (the
    /// import does not exist in the guest's instantiated world).
    pub fn unscoped(
        capsule_id: CapsuleId,
        capability: CapabilityKind,
        detail: impl Into<String>,
    ) -> Self {
        let absent_grant = match capability {
            CapabilityKind::Filesystem => CapabilityGrant::Filesystem {
                paths: Vec::new(),
                mode: FilesystemMode::Read,
            },
            CapabilityKind::Network => CapabilityGrant::Network {
                hosts: Vec::new(),
                protocols: Vec::new(),
                methods: Vec::new(),
            },
            CapabilityKind::Secret => CapabilityGrant::Secret {
                handles: Vec::new(),
            },
            CapabilityKind::Tool => CapabilityGrant::Tool { tools: Vec::new() },
            CapabilityKind::Model => CapabilityGrant::Model { models: Vec::new() },
            CapabilityKind::Clock => CapabilityGrant::Clock,
        };
        Self {
            capsule_id,
            capability,
            absent_grant,
            detail: detail.into(),
            policy_version: None,
        }
    }

    /// The scoped denial: the import exists, but the attempt fell outside
    /// every grant's scope. `absent_grant` names the scope that was
    /// missing.
    pub fn scoped(
        capsule_id: CapsuleId,
        absent_grant: CapabilityGrant,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            capsule_id,
            capability: absent_grant.capability_kind(),
            absent_grant,
            detail: detail.into(),
            policy_version: None,
        }
    }

    /// Pin the Cedar policy version whose verdict produced this denial
    /// (R0.9 wave 2 — see the field docs).
    pub fn with_policy_version(mut self, version: impl Into<String>) -> Self {
        self.policy_version = Some(version.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest() -> CapsuleManifest {
        CapsuleManifest {
            identity: CapsuleIdentity {
                name: "researcher".into(),
                description: None,
            },
            version: "1.4.0".into(),
            build_digest: sha256_hex(b"component-bytes"),
            interface: CapsuleInterface {
                world: WORLD_V1.into(),
                input_schema: None,
                output_schema: None,
            },
            effects: BTreeSet::from([Effect::ReadOnly]),
            capabilities: BTreeSet::new(),
            budget: ResourceBudget::default(),
        }
    }

    #[test]
    fn content_address_converges_and_detects_tampering() {
        let id_a = derive_capsule_id(&manifest()).unwrap();
        // Scope-list order carries no identity.
        let mut reordered = manifest();
        reordered.capabilities.insert(CapabilityGrant::Network {
            hosts: vec!["b.example".into(), "a.example".into()],
            protocols: vec!["https".into()],
            methods: vec!["GET".into()],
        });
        reordered.effects.insert(Effect::NonIdempotent);
        let mut same = reordered.clone();
        same.capabilities.insert(CapabilityGrant::Network {
            hosts: vec!["a.example".into(), "b.example".into(), "a.example".into()],
            protocols: vec!["https".into()],
            methods: vec!["GET".into()],
        });
        assert_eq!(
            derive_capsule_id(&reordered).unwrap(),
            derive_capsule_id(&same).unwrap()
        );
        // A tampered manifest fails its own address.
        let mut tampered = manifest();
        tampered.version = "9.9.9".into();
        assert_ne!(derive_capsule_id(&tampered).unwrap(), id_a);
    }

    #[test]
    fn validation_rejects_grants_above_declared_effects() {
        let mut m = manifest();
        m.capabilities.insert(CapabilityGrant::Tool {
            tools: vec!["search".into()],
        });
        // Declared effects top out at ReadOnly; a tool grant implies
        // NonIdempotent.
        let err = m.validate().unwrap_err();
        assert!(err.to_string().contains("must agree"), "got: {err}");
        m.effects.insert(Effect::NonIdempotent);
        m.validate().unwrap();
    }

    #[test]
    fn validation_rejects_empty_effects_and_bad_worlds() {
        let mut m = manifest();
        m.effects.clear();
        assert!(m.validate().is_err());
        let mut m = manifest();
        m.interface.world = "rusty:capsule/world@9.9.9".into();
        assert!(m.validate().is_err());
        let mut m = manifest();
        m.build_digest = "DEADBEEF".into();
        assert!(m.validate().is_err());
    }

    #[test]
    fn network_matching_is_exact_and_scoped() {
        let grants = [CapabilityGrant::Network {
            hosts: vec!["api.example".into()],
            protocols: vec!["https".into()],
            methods: vec!["GET".into()],
        }];
        assert!(network_grant_covers(&grants, "https", "api.example", "GET"));
        assert!(!network_grant_covers(
            &grants,
            "https",
            "evil.example",
            "GET"
        ));
        assert!(!network_grant_covers(&grants, "http", "api.example", "GET"));
        assert!(!network_grant_covers(
            &grants,
            "https",
            "api.example",
            "POST"
        ));
        assert!(any_grant_of_kind(&grants, CapabilityKind::Network));
        assert!(!any_grant_of_kind(&grants, CapabilityKind::Clock));
    }

    #[test]
    fn budget_clamp_takes_the_field_wise_minimum() {
        let declared = ResourceBudget {
            fuel: Some(100),
            wall_time_ms: Some(5000),
            ..Default::default()
        };
        let ceiling = ResourceBudget {
            fuel: Some(50),
            max_output_bytes: Some(1024),
            ..Default::default()
        };
        let clamped = declared.clamp(&ceiling);
        assert_eq!(clamped.fuel, Some(50));
        assert_eq!(clamped.wall_time_ms, Some(5000));
        assert_eq!(clamped.max_output_bytes, Some(1024));
        assert!(clamped.max_memory_bytes.is_none());
    }

    #[test]
    fn denial_payloads_name_the_absent_grant() {
        let id = CapsuleId::from("ab".repeat(32));
        let structural = CapsuleDenial::unscoped(id.clone(), CapabilityKind::Network, "probe");
        assert_eq!(structural.capability, CapabilityKind::Network);
        match &structural.absent_grant {
            CapabilityGrant::Network { hosts, .. } => assert!(hosts.is_empty()),
            other => panic!("expected network grant, got {other:?}"),
        }
        let scoped = CapsuleDenial::scoped(
            id,
            CapabilityGrant::Network {
                hosts: vec!["evil.example".into()],
                protocols: vec!["https".into()],
                methods: vec!["GET".into()],
            },
            "granted api.example, attempted evil.example",
        );
        assert_eq!(scoped.capability, CapabilityKind::Network);
        let value = serde_json::to_value(&scoped).unwrap();
        assert_eq!(value["absent_grant"]["hosts"], json!(["evil.example"]));
        // Wave-1 scope denials carry no policy context; the field is
        // absent from the wire (additive wave-2 evolution).
        assert!(value.get("policy_version").is_none());
        let pinned = scoped.with_policy_version("cedar-0123456789ab");
        let value = serde_json::to_value(&pinned).unwrap();
        assert_eq!(
            value["policy_version"],
            json!("cedar-0123456789ab"),
            "authorization refusals pin the deciding policy version"
        );
    }

    #[test]
    fn intersection_is_pairwise_and_cannot_widen() {
        let net = |hosts: &[&str]| CapabilityGrant::Network {
            hosts: hosts.iter().map(|h| h.to_string()).collect(),
            protocols: vec!["https".into()],
            methods: vec!["GET".into()],
        };
        let manifest = BTreeSet::from([net(&["a.example", "b.example"]), CapabilityGrant::Clock]);
        // An overlay hand-crafted to *widen*: it names a host the
        // manifest never granted and drops the clock.
        let overlay = BTreeSet::from([net(&["b.example", "evil.example"])]);
        let effective = intersect_grants(&manifest, &overlay);
        // The only survivor is the shared host; the ungranted host adds
        // nothing and the uncovered clock grant vanishes.
        assert_eq!(effective, BTreeSet::from([net(&["b.example"])]));
        // Every returned grant is a narrowing of a grant on each side —
        // the property the whole wave rests on, stated as an assertion.
        for grant in &effective {
            match grant {
                CapabilityGrant::Network {
                    hosts,
                    protocols,
                    methods,
                } => {
                    assert!(hosts.iter().all(|h| ["b.example"].contains(&h.as_str())));
                    assert_eq!(protocols, &vec!["https".to_string()]);
                    assert_eq!(methods, &vec!["GET".to_string()]);
                }
                other => panic!("unexpected grant survived: {other:?}"),
            }
        }
        // Disjoint scopes remove the capability entirely rather than
        // carrying an empty-scope grant.
        let disjoint = BTreeSet::from([net(&["c.example"])]);
        assert!(intersect_grants(&manifest, &disjoint).is_empty());
        // Kinds never mix: a clock ceiling does not revive an ungranted
        // network capability, and vice versa.
        assert!(intersect_grants(
            &BTreeSet::from([CapabilityGrant::Clock]),
            &BTreeSet::from([net(&["a.example"])]),
        )
        .is_empty());
    }

    #[test]
    fn intersection_combines_scope_fields_and_modes() {
        // Filesystem modes narrow to the stricter of the two; scope lists
        // intersect item-wise.
        let manifest = BTreeSet::from([CapabilityGrant::Filesystem {
            paths: vec!["/data".into(), "/tmp".into()],
            mode: FilesystemMode::ReadWrite,
        }]);
        let overlay = BTreeSet::from([CapabilityGrant::Filesystem {
            paths: vec!["/data".into()],
            mode: FilesystemMode::Read,
        }]);
        let effective = intersect_grants(&manifest, &overlay);
        assert_eq!(
            effective,
            BTreeSet::from([CapabilityGrant::Filesystem {
                paths: vec!["/data".into()],
                mode: FilesystemMode::Read,
            }])
        );
        // Network scoping is conjunctive across fields: sharing hosts but
        // not methods yields no grant.
        let manifest = BTreeSet::from([CapabilityGrant::Network {
            hosts: vec!["a.example".into()],
            protocols: vec!["https".into()],
            methods: vec!["POST".into()],
        }]);
        let overlay = BTreeSet::from([CapabilityGrant::Network {
            hosts: vec!["a.example".into()],
            protocols: vec!["https".into()],
            methods: vec!["GET".into()],
        }]);
        assert!(intersect_grants(&manifest, &overlay).is_empty());
    }

    #[test]
    fn grants_beyond_flags_only_uncovered_reach() {
        let net = |hosts: &[&str], methods: &[&str]| CapabilityGrant::Network {
            hosts: hosts.iter().map(|h| h.to_string()).collect(),
            protocols: vec!["https".into()],
            methods: methods.iter().map(|m| m.to_string()).collect(),
        };
        let manifest = BTreeSet::from([net(&["a.example"], &["GET"]), CapabilityGrant::Clock]);
        // Coverage is the union of same-kind scope items: two manifest
        // grants together cover an overlay naming both hosts.
        let manifest = manifest
            .into_iter()
            .chain([net(&["b.example"], &["GET"])])
            .collect();
        let within = BTreeSet::from([net(&["a.example", "b.example"], &["GET"])]);
        assert!(grants_beyond(&within, &manifest).is_empty());
        // A new host, a new method, and a kind the manifest never granted
        // are all beyond.
        let beyond = BTreeSet::from([
            net(&["a.example", "evil.example"], &["GET"]),
            net(&["a.example"], &["POST"]),
            CapabilityGrant::Secret {
                handles: vec!["api-key".into()],
            },
        ]);
        let flagged = grants_beyond(&beyond, &manifest);
        assert_eq!(flagged.len(), 3);
        // But the clock and the in-scope network grant are covered.
        let covered = BTreeSet::from([CapabilityGrant::Clock, net(&["a.example"], &["GET"])]);
        assert!(grants_beyond(&covered, &manifest).is_empty());
    }

    #[test]
    fn overlay_validation_and_targeting() {
        let overlay = CapsuleOverlay {
            name: "narrow-research".into(),
            targets: Some(vec!["researcher".into()]),
            capabilities: BTreeSet::from([CapabilityGrant::Clock]),
            note: None,
        };
        overlay.validate().unwrap();
        assert!(overlay.applies_to("researcher"));
        assert!(!overlay.applies_to("planner"));
        let tenant_wide = CapsuleOverlay {
            name: "tenant-wide".into(),
            targets: None,
            capabilities: BTreeSet::from([CapabilityGrant::Clock]),
            note: None,
        };
        tenant_wide.validate().unwrap();
        assert!(tenant_wide.applies_to("anything"));
        // An empty target list is refused (meaningless), as is an empty
        // grant scope.
        let mut bad = tenant_wide.clone();
        bad.targets = Some(Vec::new());
        assert!(bad.validate().is_err());
        let mut bad = tenant_wide.clone();
        bad.capabilities = BTreeSet::from([CapabilityGrant::Secret {
            handles: Vec::new(),
        }]);
        assert!(bad.validate().is_err());
        let mut bad = tenant_wide;
        bad.name = "has/slash".into();
        assert!(bad.validate().is_err());
    }
}
