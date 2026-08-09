//! The capsule authorization plane (R0.9 Rusty Capsules, wave 2): Cedar
//! policies, tenant overlays, and the versioning store both backends keep.
//!
//! The design doc is `docs/capsules-design.md` ("Cedar policies"). Three
//! decisions need authorization, all made here:
//!
//! 1. **Capsule admission** — may this tenant load this capsule
//!    (identity, build digest) at all? Evaluated at registration
//!    (`POST /capsules`) and again at resolution (`POST
//!    /capsules/resolve`) — a policy change between the two must bite.
//! 2. **Grant checks** — does policy permit *these* capability grants
//!    for this tenant? A manifest may declare fewer grants than policy
//!    permits (narrowing is always safe); it may not run with grants
//!    policy forbids. One Cedar request per declared grant; any denied
//!    grant refuses the admission.
//! 3. **Tenant overlays** — may this author attach this overlay against
//!    these capsules? The narrowing itself is never Cedar's call:
//!    application is structural intersection
//!    ([`rusty_agent_runtime::capsule::intersect_grants`]), a set
//!    operation that mechanically cannot add a grant. Policy decides
//!    legality; arithmetic decides narrowing.
//!
//! ## Versioning and the active pointer
//!
//! Policies are operator-authored `.cedar` text registered as immutable,
//! versioned bodies — the executor policy plane's discipline
//! (`crate::policy`) applied to authorization. Registration converges
//! on identical text and conflicts on changed text under one version; the
//! active version is a single pointer per tenant, moved explicitly
//! (`POST /capsule_policies/active`), never implicitly by registration.
//! Both backends version the plane: `{store}/capsule_policies/` on the
//! JSON backend (immutable version files plus an append-style pointer
//! file per tenant), a column-mapped `server_capsule_policies` table on
//! Postgres (the active flag is a real column — the active-version
//! lookup and the startup preload read it directly). The active version
//! is pinned into every capsule admission event (the `capsule_resolved`
//! payload's `policy_version`).
//!
//! ## The unconfigured posture, stated plainly
//!
//! A tenant with **no active capsule policy** admits capsules the wave-1
//! way: registration validates the manifest, resolution answers the pin.
//! This is the one place the wave is not default-deny, and it is
//! deliberate — retrofitting authorization onto an already-admitted
//! registry must not brick deployments that upgrade before their
//! operators author policies. Enforcement begins per tenant the moment
//! the operator activates the first policy version. A server built
//! *without* the `capsules` feature is the opposite posture: the whole
//! plane (this file's routes) refuses with a typed `503
//! capsule_policy_unavailable` — fail closed, never a silent skip.
//!
//! ## Revocation and the recheck cache
//!
//! Cedar evaluates static policy at admission; it cannot un-admit a
//! capsule already running. The wave's answer is revocation at the next
//! capability use through core's `GrantRecheck` seam, served by
//! `CapsulePolicyPlane`: a per-tenant cache of the parsed active
//! engine, refreshed **eagerly in-process** on every policy mutation and
//! on every admission (the admission read doubles as the refresh), plus
//! a best-effort startup preload across tenants so a restarted process
//! re-arm the seam without waiting for traffic. The honest cache epoch:
//! within one process, revocation is effective as soon as the activating
//! request completes; across processes sharing one store, a revocation
//! lands at the other processes' next restart (there is no cross-process
//! invalidation — the epoch is the process lifetime, bounded by deploy
//! cadence). A recheck whose evaluation *fails* refuses closed and
//! journals against the cached version.

// The records and the JSON-file layout are the plane's persistence half:
// consumed by the feature's routes and by `server_store`'s capsule-policy
// methods, so — like the engine — they compile only with the feature.
// The glob re-export keeps every consumer's path
// (`crate::capsule_policy::CapsulePolicyRecord` and friends) unchanged.
#[cfg(feature = "capsules")]
mod store_layout {
    use std::collections::HashMap;
    use std::io;
    use std::path::{Path, PathBuf};

    use chrono::{DateTime, Utc};
    use rusty_agent_runtime::capsule::CapsuleOverlay;
    use serde::{Deserialize, Serialize};

    // --------------------------------------------------------------------- //
    // Registry records
    // --------------------------------------------------------------------- //

    /// One immutable Cedar policy body. Immutability is enforced at the write
    /// seam ([`crate::server_store::ServerStore::put_capsule_policy`]): the
    /// same version naming the same text converges (the idempotent create),
    /// the same version naming different text conflicts — a version string is
    /// a commitment to one exact policy set, which is what makes
    /// `policy_version` on a journaled admission meaningful.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub(crate) struct CapsulePolicyRecord {
        /// The version this body is registered under (`cedar-{hash12}` for
        /// content-derived bodies; operator-chosen names are accepted for the
        /// API source, validated path-safe).
        pub version: String,

        /// The Cedar source text — the whole body. The engine parses it at
        /// evaluation time; the store holds the text, never a compiled form,
        /// so what served is always what was registered.
        pub policy_text: String,

        /// When the body was first registered (a converged re-registration
        /// keeps the original instant — the record is immutable).
        pub registered_at: DateTime<Utc>,
    }

    /// The result of a policy registration
    /// ([`ServerStore::put_capsule_policy`](crate::server_store::ServerStore::put_capsule_policy)):
    /// created, converged (the version already names exactly this text — the
    /// idempotent create), or conflicted (the version names different text —
    /// registry immutability refuses the overwrite).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum CapsulePolicyWrite {
        /// The version is new; the body is stored.
        Created,
        /// The version already named exactly this body; nothing changed.
        Converged,
        /// The version already names a different body. Refused — immutability
        /// is what makes a version string a commitment.
        Conflict,
    }

    /// The active-version pointer (R0.9 wave 2): "from `activated_at` on, new
    /// admissions are decided under `version`." One per tenant, replaced
    /// atomically on activation (temp-write-then-rename on the JSON backend;
    /// a two-statement transaction flipping the `active` column on Postgres).
    /// Unlike the executor plane's append-only activation log this keeps no
    /// history: the wave's evidence requirement is *which version decided
    /// each admission*, and that is pinned on the admission events
    /// themselves — the pointer only answers "what serves now."
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub(crate) struct CapsulePolicyActivation {
        /// The version that became active.
        pub version: String,

        /// When the move happened.
        pub activated_at: DateTime<Utc>,
    }

    /// One attached tenant overlay: the core contract
    /// ([`CapsuleOverlay`]) plus the provenance the audit trail needs.
    /// Overlays are operator configuration, not content-addressed registry
    /// entries: re-attaching under the same name replaces the ceiling (the
    /// way a cron upsert replaces bookkeeping), and narrowing at the *next*
    /// admission is the semantic — an overlay tightened mid-run takes effect
    /// when the run's capsules are next resolved, exactly like a policy
    /// activation.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub(crate) struct CapsuleOverlayRecord {
        /// The attached overlay (name, optional targets, grant ceiling).
        pub overlay: CapsuleOverlay,

        /// The tenant whose operator attached it. Server-side provenance,
        /// never client-supplied: the route fills it from the authenticated
        /// tenant, so a crafted body cannot claim another tenant's
        /// authorship.
        pub author: String,

        /// When the overlay was first attached (a replacement keeps a fresh
        /// instant — the record tracks the ceiling as it now stands).
        pub attached_at: DateTime<Utc>,
    }

    // --------------------------------------------------------------------- //
    // The JSON-file layout
    // --------------------------------------------------------------------- //

    /// The plane's directory under the store root (`{store_path}/capsule_policies`;
    /// `capsule_policies` is a reserved layout name, see
    /// [`crate::RESERVED_NAMES`]).
    fn capsule_policies_dir(root: &Path) -> PathBuf {
        root.join("capsule_policies")
    }

    /// The immutable version bodies (`{store_path}/capsule_policies/versions`).
    pub(crate) fn capsule_policy_versions_dir(root: &Path) -> PathBuf {
        capsule_policies_dir(root).join("versions")
    }

    /// The per-tenant active pointer (`{store_path}/capsule_policies/active`).
    pub(crate) fn capsule_policy_active_dir(root: &Path) -> PathBuf {
        capsule_policies_dir(root).join("active")
    }

    /// The attached overlays (`{store_path}/capsule_policies/overlays`).
    pub(crate) fn capsule_overlays_dir(root: &Path) -> PathBuf {
        capsule_policies_dir(root).join("overlays")
    }

    /// Recursively collect `*.json` files under `root` (tenant subdirectories
    /// hold that tenant's records) — the loader walk every layout here shares.
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

    /// The path-derived scoped name of a record file under `dir`
    /// (`{tenant}/{name}` for named tenants, the bare name for the default
    /// tenant) — the path-keyed tenancy rule every layout here shares.
    fn path_scoped_name(dir: &Path, path: &Path) -> Option<String> {
        path.strip_prefix(dir)
            .ok()
            .map(|relative| relative.with_extension(""))
            .map(|relative| {
                relative
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/")
            })
    }

    /// Persist one JSON record atomically (temp file + rename) under `dir`,
    /// named by `scoped_name` — the durability discipline every file record
    /// in the server shares.
    async fn persist_record<T: Serialize>(
        dir: &Path,
        scoped_name: &str,
        record: &T,
        what: &str,
    ) -> io::Result<()> {
        tokio::fs::create_dir_all(dir).await?;
        let bytes = serde_json::to_vec_pretty(record)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let path = dir.join(format!("{scoped_name}.json"));
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = dir.join(format!("{scoped_name}.{what}.tmp"));
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::rename(&tmp, path).await
    }

    /// Load every record of type `T` under `dir`, keyed by path-derived
    /// scoped name. Files that fail to parse are skipped with a warning (the
    /// corrupt-tolerance rule every loader here shares).
    fn load_records<T: serde::de::DeserializeOwned>(dir: &Path, what: &str) -> HashMap<String, T> {
        let mut out = HashMap::new();
        let mut files = Vec::new();
        collect_json_files(dir, &mut files);
        for path in files {
            let scoped = path_scoped_name(dir, &path);
            let parsed = std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<T>(&raw).ok());
            match (scoped, parsed) {
                (Some(name), Some(record)) => {
                    out.insert(name, record);
                }
                _ => {
                    tracing::warn!(path = %path.display(), "skipping unreadable {what} file")
                }
            }
        }
        out
    }

    /// Persist one policy body, named by its tenant-scoped version
    /// (`{tenant}/{version}.json` — versions are validated path-safe).
    pub(crate) async fn persist_capsule_policy(
        root: &Path,
        scoped_version: &str,
        record: &CapsulePolicyRecord,
    ) -> io::Result<()> {
        persist_record(
            &capsule_policy_versions_dir(root),
            scoped_version,
            record,
            "policy",
        )
        .await
    }

    /// Load all policy bodies, keyed by tenant-scoped version.
    pub(crate) fn load_capsule_policies(root: &Path) -> HashMap<String, CapsulePolicyRecord> {
        load_records(&capsule_policy_versions_dir(root), "capsule policy")
    }

    /// The file name of a tenant's active-version pointer: the tenant-scoped
    /// fixed name `active`, so default-tenant deployments keep the legacy
    /// unprefixed shape (`active.json`) and named tenants get
    /// `{tenant}/active.json`.
    pub(crate) fn activation_scoped_name(tenant: &str) -> String {
        crate::auth::scope_id(tenant, "active")
    }

    /// Move the tenant's active pointer (atomic replace: the pointer is one
    /// file, temp-write-then-renamed).
    pub(crate) async fn persist_capsule_policy_activation(
        root: &Path,
        tenant: &str,
        activation: &CapsulePolicyActivation,
    ) -> io::Result<()> {
        persist_record(
            &capsule_policy_active_dir(root),
            &activation_scoped_name(tenant),
            activation,
            "activation",
        )
        .await
    }

    /// Load every tenant's active pointer, keyed by the path-derived scoped
    /// file name (`active` for the default tenant, `{tenant}/active` for
    /// named tenants).
    pub(crate) fn load_capsule_policy_activations(
        root: &Path,
    ) -> HashMap<String, CapsulePolicyActivation> {
        load_records(
            &capsule_policy_active_dir(root),
            "capsule policy activation",
        )
    }

    /// Persist one overlay, named by its tenant-scoped name
    /// (`{tenant}/{name}.json` — overlay names are validated path-safe).
    pub(crate) async fn persist_capsule_overlay(
        root: &Path,
        scoped_name: &str,
        record: &CapsuleOverlayRecord,
    ) -> io::Result<()> {
        persist_record(&capsule_overlays_dir(root), scoped_name, record, "overlay").await
    }

    /// Load all overlays, keyed by tenant-scoped overlay name.
    pub(crate) fn load_capsule_overlays(root: &Path) -> HashMap<String, CapsuleOverlayRecord> {
        load_records(&capsule_overlays_dir(root), "capsule overlay")
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use rusty_agent_runtime::capsule::CapabilityGrant;
        use std::collections::BTreeSet;

        fn ts(millis: i64) -> DateTime<Utc> {
            DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
        }

        #[tokio::test]
        async fn layout_round_trips_policies_pointer_and_overlays() {
            let root = std::env::temp_dir().join(format!(
                "rusty-capsule-policy-test-{}",
                uuid::Uuid::new_v4()
            ));
            let record = CapsulePolicyRecord {
                version: "cedar-0123456789ab".into(),
                policy_text: "permit(principal, action, resource);".into(),
                registered_at: ts(1_000),
            };
            persist_capsule_policy(&root, "cedar-0123456789ab", &record)
                .await
                .unwrap();
            persist_capsule_policy(&root, "acme/cedar-0123456789ab", &record)
                .await
                .unwrap();
            let activation = CapsulePolicyActivation {
                version: "cedar-0123456789ab".into(),
                activated_at: ts(2_000),
            };
            persist_capsule_policy_activation(&root, "default", &activation)
                .await
                .unwrap();
            persist_capsule_policy_activation(&root, "acme", &activation)
                .await
                .unwrap();
            let overlay = CapsuleOverlayRecord {
                overlay: CapsuleOverlay {
                    name: "ceiling".into(),
                    targets: None,
                    capabilities: BTreeSet::from([CapabilityGrant::Clock]),
                    note: None,
                },
                author: "acme".into(),
                attached_at: ts(3_000),
            };
            persist_capsule_overlay(&root, "acme/ceiling", &overlay)
                .await
                .unwrap();
            std::fs::create_dir_all(capsule_policy_versions_dir(&root)).unwrap();
            std::fs::write(
                capsule_policy_versions_dir(&root).join("broken.json"),
                b"{nope",
            )
            .unwrap();

            let policies = load_capsule_policies(&root);
            assert_eq!(policies.len(), 2, "corrupt files are skipped, not fatal");
            assert!(policies.contains_key("cedar-0123456789ab"));
            assert!(policies.contains_key("acme/cedar-0123456789ab"));
            let activations = load_capsule_policy_activations(&root);
            assert_eq!(activations.len(), 2);
            assert!(activations.contains_key("active"));
            assert!(activations.contains_key("acme/active"));
            let overlays = load_capsule_overlays(&root);
            assert_eq!(overlays.len(), 1);
            assert!(overlays.contains_key("acme/ceiling"));
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

#[cfg(feature = "capsules")]
pub(crate) use store_layout::*;

// --------------------------------------------------------------------- //
// The Cedar engine (feature `capsules`)
// --------------------------------------------------------------------- //

#[cfg(feature = "capsules")]
pub use engine::{CapsulePolicyError, CapsulePolicyPlane};

#[cfg(feature = "capsules")]
pub(crate) use engine::{
    activation, authorize_overlay_attach, authorize_registration, compose_admission,
    derive_capsule_policy_version, load_config_policies, preload_active_policies,
    prospective_record, validate_capsule_policy_version, AdmissionRefusal, CedarEngine,
};

#[cfg(feature = "capsules")]
mod engine {
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::str::FromStr;
    use std::sync::{Arc, RwLock};

    use cedar_policy::{
        Authorizer, Context, Decision, Entities, EntityId, EntityTypeName, EntityUid, PolicySet,
        Request,
    };
    use chrono::Utc;
    use rusty_agent_runtime::capsule::{
        grants_beyond, CapabilityGrant, CapsuleId, CapsuleManifest, CapsuleOverlay, ResourceBudget,
    };
    use rusty_agent_runtime::capsule_host::{GrantRecheck, RecheckDenial};
    use rusty_agent_runtime::record::sha256_hex;
    use serde_json::{json, Value};

    use super::{CapsulePolicyActivation, CapsulePolicyRecord, CapsulePolicyWrite};
    use crate::capsules::CapsuleRecord;
    use crate::server_store::ServerStore;

    /// The Cedar actions the plane defines. Policies match on
    /// `Action::"AdmitCapsule"` (decision 1), `Action::"UseCapability"`
    /// (decision 2, one request per declared grant), and
    /// `Action::"AttachOverlay"` (decision 3). There is no Cedar schema:
    /// every request is constructed by the typed functions below, so the
    /// wire-shape checking a schema buys has no untrusted input to bite
    /// on — the operator's policies are the only free-form text, and
    /// they are parse-checked at registration.
    const ACTION_ADMIT: &str = "AdmitCapsule";
    const ACTION_USE: &str = "UseCapability";
    const ACTION_ATTACH: &str = "AttachOverlay";

    /// A failure of the authorization machinery itself — parse,
    /// evaluation, or request construction. These are typed, never
    /// unwraps: a malformed operator policy is a `422` at registration
    /// and a `500` anywhere it surfaces later; an evaluation failure on
    /// the recheck path refuses closed.
    #[derive(Debug)]
    pub enum CapsulePolicyError {
        /// The Cedar source failed to parse.
        Parse(String),
        /// Cedar reported evaluation errors for a request.
        Evaluation(String),
        /// A request, entity, or context could not be constructed.
        Request(String),
    }

    impl std::fmt::Display for CapsulePolicyError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                CapsulePolicyError::Parse(e) => write!(f, "cedar policy parse error: {e}"),
                CapsulePolicyError::Evaluation(e) => {
                    write!(f, "cedar policy evaluation error: {e}")
                }
                CapsulePolicyError::Request(e) => {
                    write!(f, "cedar request construction error: {e}")
                }
            }
        }
    }

    impl std::error::Error for CapsulePolicyError {}

    /// Build an entity uid from the plane's closed type set. Both halves
    /// are validated upstream (tenant ids and capsule ids are grammar-
    /// checked before they reach here), but Cedar construction is still
    /// fallible and the failure is typed, never unwrapped.
    fn entity_uid(entity_type: &str, id: &str) -> Result<EntityUid, CapsulePolicyError> {
        let entity_type = EntityTypeName::from_str(entity_type).map_err(|e| {
            CapsulePolicyError::Request(format!("entity type `{entity_type}`: {e}"))
        })?;
        let id = EntityId::from_str(id)
            .map_err(|e| CapsulePolicyError::Request(format!("entity id `{id}`: {e}")))?;
        Ok(EntityUid::from_type_name_and_id(entity_type, id))
    }

    /// The serialized form of one grant as decision-2 context: the kind
    /// plus its scope lists, so policies can match
    /// `context.kind == "network"` or test scope items. No field is ever
    /// null — Cedar's value set has none.
    fn grant_context(grant: &CapabilityGrant) -> Value {
        match grant {
            CapabilityGrant::Filesystem { paths, mode } => json!({
                "kind": "filesystem",
                "paths": paths,
                "mode": mode,
            }),
            CapabilityGrant::Network {
                hosts,
                protocols,
                methods,
            } => json!({
                "kind": "network",
                "hosts": hosts,
                "protocols": protocols,
                "methods": methods,
            }),
            CapabilityGrant::Secret { handles } => json!({
                "kind": "secret",
                "handles": handles,
            }),
            CapabilityGrant::Tool { tools } => json!({
                "kind": "tool",
                "tools": tools,
            }),
            CapabilityGrant::Model { models } => json!({
                "kind": "model",
                "models": models,
            }),
            CapabilityGrant::Clock => json!({ "kind": "clock" }),
        }
    }

    /// The capability-kind wire name of one grant (the serde
    /// discriminant), for overlay context.
    fn kind_name(grant: &CapabilityGrant) -> &'static str {
        match grant {
            CapabilityGrant::Filesystem { .. } => "filesystem",
            CapabilityGrant::Network { .. } => "network",
            CapabilityGrant::Secret { .. } => "secret",
            CapabilityGrant::Tool { .. } => "tool",
            CapabilityGrant::Model { .. } => "model",
            CapabilityGrant::Clock => "clock",
        }
    }

    /// One Cedar verdict: allow/deny plus the ids of the policies that
    /// determined the outcome (the `forbid` that fired, or the `permit`
    /// that matched — Cedar's diagnostics), carried into refusal details
    /// so an operator can trace a refusal to the text that made it.
    #[derive(Debug)]
    pub(crate) struct Verdict {
        pub(crate) allowed: bool,
        pub(crate) reasons: Vec<String>,
    }

    /// The parsed form of one policy version: Cedar's authorizer plus the
    /// policy set. Parsing happens at registration (the operator's text
    /// is checked before it is stored) and again on every load — the
    /// store holds text, so evaluation always starts from what was
    /// registered.
    pub(crate) struct CedarEngine {
        authorizer: Authorizer,
        policies: PolicySet,
    }

    impl CedarEngine {
        /// Parse one policy body. A parse failure names the operator's
        /// text, not an internal assumption.
        pub(crate) fn parse(text: &str) -> Result<Self, CapsulePolicyError> {
            let policies =
                PolicySet::from_str(text).map_err(|e| CapsulePolicyError::Parse(e.to_string()))?;
            Ok(Self {
                authorizer: Authorizer::new(),
                policies,
            })
        }

        /// One authorization request: principal `Tenant::"{tenant}"`,
        /// action `Action::"{action}"`, the given resource, and the given
        /// context. `entities` carries the JSON entity rows the request
        /// references (the principal always; the resource when it is a
        /// capsule). Evaluation errors are typed failures — a policy that
        /// errors is not a policy that permits.
        fn decide(
            &self,
            tenant: &str,
            action: &str,
            resource: EntityUid,
            entities: Vec<Value>,
            context: Value,
        ) -> Result<Verdict, CapsulePolicyError> {
            let request = Request::new(
                entity_uid("Tenant", tenant)?,
                entity_uid("Action", action)?,
                resource,
                Context::from_json_value(context, None)
                    .map_err(|e| CapsulePolicyError::Request(e.to_string()))?,
                None,
            )
            .map_err(|e| CapsulePolicyError::Request(e.to_string()))?;
            let entities = Entities::from_json_value(Value::Array(entities), None)
                .map_err(|e| CapsulePolicyError::Request(e.to_string()))?;
            let response = self
                .authorizer
                .is_authorized(&request, &self.policies, &entities);
            let errors: Vec<String> = response
                .diagnostics()
                .errors()
                .map(ToString::to_string)
                .collect();
            if !errors.is_empty() {
                return Err(CapsulePolicyError::Evaluation(errors.join("; ")));
            }
            Ok(Verdict {
                allowed: response.decision() == Decision::Allow,
                reasons: response
                    .diagnostics()
                    .reason()
                    .map(ToString::to_string)
                    .collect(),
            })
        }

        /// The tenant entity row every request's entity set starts with.
        fn tenant_entity(tenant: &str) -> Value {
            json!({"uid": {"type": "Tenant", "id": tenant}, "attrs": {}, "parents": []})
        }

        /// The capsule entity row: attributes carry the identity,
        /// version, and build digest so policies can match on them.
        fn capsule_entity(record: &CapsuleRecord) -> Value {
            json!({
                "uid": {"type": "Capsule", "id": record.capsule_id.as_str()},
                "attrs": {
                    "name": record.manifest.identity.name,
                    "version": record.manifest.version,
                    "build_digest": record.manifest.build_digest,
                },
                "parents": [],
            })
        }

        /// Decision 1: may this tenant load this capsule at all?
        pub(crate) fn authorize_admission(
            &self,
            tenant: &str,
            record: &CapsuleRecord,
        ) -> Result<Verdict, CapsulePolicyError> {
            self.decide(
                tenant,
                ACTION_ADMIT,
                entity_uid("Capsule", record.capsule_id.as_str())?,
                vec![Self::tenant_entity(tenant), Self::capsule_entity(record)],
                json!({
                    "name": record.manifest.identity.name,
                    "version": record.manifest.version,
                    "build_digest": record.manifest.build_digest,
                }),
            )
        }

        /// Decision 2 for one grant: does policy permit this capability
        /// for this tenant against this capsule?
        fn authorize_grant(
            &self,
            tenant: &str,
            capsule_id: &CapsuleId,
            capsule_entities: Vec<Value>,
            grant: &CapabilityGrant,
        ) -> Result<Verdict, CapsulePolicyError> {
            self.decide(
                tenant,
                ACTION_USE,
                entity_uid("Capsule", capsule_id.as_str())?,
                capsule_entities,
                grant_context(grant),
            )
        }

        /// Decision 2 over a declared grant set: every grant policy
        /// forbids. Declaring fewer than permitted is always fine — this
        /// only ever looks at what the manifest *did* declare.
        pub(crate) fn forbidden_grants(
            &self,
            tenant: &str,
            record: &CapsuleRecord,
        ) -> Result<Vec<CapabilityGrant>, CapsulePolicyError> {
            let entities = vec![Self::tenant_entity(tenant), Self::capsule_entity(record)];
            let mut forbidden = Vec::new();
            for grant in &record.manifest.capabilities {
                let verdict =
                    self.authorize_grant(tenant, &record.capsule_id, entities.clone(), grant)?;
                if !verdict.allowed {
                    forbidden.push(grant.clone());
                }
            }
            Ok(forbidden)
        }

        /// Decision 2 for the recheck seam: one exact scope against the
        /// live policy. Same question as [`Self::authorize_grant`], but
        /// the resource entity set is rebuilt per call — the recheck
        /// path has no registry record to hand, only the content address.
        pub(crate) fn authorize_use(
            &self,
            tenant: &str,
            capsule_id: &CapsuleId,
            grant: &CapabilityGrant,
        ) -> Result<Verdict, CapsulePolicyError> {
            self.decide(
                tenant,
                ACTION_USE,
                entity_uid("Capsule", capsule_id.as_str())?,
                vec![
                    Self::tenant_entity(tenant),
                    json!({
                        "uid": {"type": "Capsule", "id": capsule_id.as_str()},
                        "attrs": {},
                        "parents": [],
                    }),
                ],
                grant_context(grant),
            )
        }

        /// Decision 3: may this tenant's operator attach this overlay
        /// against this target? `target` is `Some(manifest)` when the
        /// overlay applies to a registered capsule (the `widens` signal
        /// is the structural beyond-check the intersection would render
        /// harmless anyway — the policy decides legality *before* the
        /// arithmetic makes it moot), `None` when no registered capsule
        /// matches (nothing to widen yet; the resource is the tenant).
        pub(crate) fn authorize_overlay(
            &self,
            tenant: &str,
            overlay: &CapsuleOverlay,
            target: Option<&CapsuleRecord>,
        ) -> Result<Verdict, CapsulePolicyError> {
            let (resource, entities, widens, capsule_name) = match target {
                Some(record) => (
                    entity_uid("Capsule", record.capsule_id.as_str())?,
                    vec![Self::tenant_entity(tenant), Self::capsule_entity(record)],
                    !grants_beyond(&overlay.capabilities, &record.manifest.capabilities).is_empty(),
                    record.manifest.identity.name.clone(),
                ),
                None => (
                    entity_uid("Tenant", tenant)?,
                    vec![Self::tenant_entity(tenant)],
                    false,
                    String::new(),
                ),
            };
            self.decide(
                tenant,
                ACTION_ATTACH,
                resource,
                entities,
                json!({
                    "name": overlay.name,
                    "targets": overlay.targets.clone().unwrap_or_default(),
                    "capsule": capsule_name,
                    "widens": widens,
                    "grant_kinds": overlay
                        .capabilities
                        .iter()
                        .map(kind_name)
                        .collect::<Vec<_>>(),
                }),
            )
        }
    }

    /// The content-derived version of a policy body: `cedar-{hash12}` —
    /// the executor plane's derived-name convention
    /// (`derive_policy_version`'s `policy-{hash12}`), so identical text
    /// converges on one version on any backend.
    pub(crate) fn derive_capsule_policy_version(text: &str) -> String {
        format!("cedar-{}", &sha256_hex(text.as_bytes())[..12])
    }

    /// `true` when `version` is a valid version string: path-safe (it
    /// becomes a filename segment) — the executor plane's version rule.
    pub(crate) fn validate_capsule_policy_version(version: &str) -> Result<(), String> {
        let valid = !version.is_empty()
            && version.len() <= 128
            && version
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
        if !valid {
            return Err(format!(
                "invalid capsule policy version `{version}` (allowed: [A-Za-z0-9._-], 1..=128 chars)"
            ));
        }
        Ok(())
    }

    /// One tenant's cached engine: the parsed active policy plus the
    /// version it was parsed from, so a recheck denial can name the
    /// verdict that made it.
    struct CachedEngine {
        version: String,
        engine: CedarEngine,
    }

    /// The revocation cache (R0.9 wave 2): per-tenant parsed engines
    /// serving core's `GrantRecheck` seam, refreshed eagerly on every
    /// policy mutation and every admission, and preloaded at startup —
    /// see the module docs for the honest cache epoch.
    ///
    /// This is the public seam embedders compose with: the server has no
    /// capsule-invocation route (invocation is core's host), so a
    /// deployment embedding `rusty-agent-server` builds its own
    /// `CapsuleHost`s and plugs [`CapsulePolicyPlane::rechecker`] into
    /// them — admission's verdict and the capsule's per-call rechecks
    /// then run against the same parsed engine.
    pub struct CapsulePolicyPlane {
        engines: RwLock<std::collections::HashMap<String, CachedEngine>>,
    }

    impl std::fmt::Debug for CapsulePolicyPlane {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let tenants: Vec<String> = self
                .engines
                .read()
                .map(|engines| engines.keys().cloned().collect())
                .unwrap_or_default();
            f.debug_struct("CapsulePolicyPlane")
                .field("cached_tenants", &tenants)
                .finish()
        }
    }

    impl Default for CapsulePolicyPlane {
        fn default() -> Self {
            Self::new()
        }
    }

    impl CapsulePolicyPlane {
        /// An empty plane: every tenant uncached, every recheck permitted
        /// (the unconfigured posture — enforcement begins when the first
        /// policy version is activated and installed).
        pub fn new() -> Self {
            Self {
                engines: RwLock::new(std::collections::HashMap::new()),
            }
        }

        /// Install (or replace) the engine cached for `tenant`: the
        /// parsed form of one policy version. The server's policy routes
        /// call this on every activation, and admission calls it with the
        /// version it just decided under, so the recheck seam serves
        /// exactly the verdict admission computed. Embedders composing
        /// their own admission path call it with the active record's
        /// text. A parse failure installs nothing — the previous engine
        /// (if any) keeps serving, and the error surfaces.
        pub fn install(
            &self,
            tenant: &str,
            version: &str,
            policy_text: &str,
        ) -> Result<(), CapsulePolicyError> {
            let engine = CedarEngine::parse(policy_text)?;
            let mut engines = self
                .engines
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            engines.insert(
                tenant.to_string(),
                CachedEngine {
                    version: version.to_string(),
                    engine,
                },
            );
            Ok(())
        }

        /// Drop the tenant's cached engine: no active policy, so
        /// rechecks permit (the unconfigured posture).
        pub fn clear(&self, tenant: &str) {
            let mut engines = self
                .engines
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            engines.remove(tenant);
        }

        /// The version the tenant's cache currently serves (`None` when
        /// uncached) — diagnostics for the "which verdict refused this"
        /// question.
        pub fn cached_version(&self, tenant: &str) -> Option<String> {
            self.engines
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(tenant)
                .map(|cached| cached.version.clone())
        }

        /// The revocation seam for hosts admitted under this plane: a
        /// `GrantRecheck` reading the tenant's cached engine on every
        /// granted capability use.
        pub fn rechecker(self: &Arc<Self>, tenant: &str) -> Arc<dyn GrantRecheck> {
            Arc::new(PlaneRecheck {
                plane: Arc::clone(self),
                tenant: tenant.to_string(),
            })
        }

        /// Refresh the tenant's cache from the store's active pointer:
        /// install the active version's engine, or clear the cache when
        /// nothing is active. Called by the activation route; admission
        /// reads skip it (they install directly from the record they
        /// already loaded).
        pub(crate) async fn refresh(
            &self,
            store: &Arc<dyn ServerStore>,
            tenant: &str,
        ) -> Result<(), String> {
            match store.active_capsule_policy(tenant).await? {
                Some(record) => self
                    .install(tenant, &record.version, &record.policy_text)
                    .map_err(|e| {
                        format!(
                            "active capsule policy `{}` for tenant `{tenant}` no longer parses: {e}",
                            record.version
                        )
                    }),
                None => {
                    self.clear(tenant);
                    Ok(())
                }
            }
        }
    }

    /// The [`GrantRecheck`] implementation the plane hands out: per-call
    /// re-authorization against the tenant's cached engine.
    #[derive(Debug)]
    struct PlaneRecheck {
        plane: Arc<CapsulePolicyPlane>,
        tenant: String,
    }

    impl GrantRecheck for PlaneRecheck {
        fn recheck(
            &self,
            capsule_id: &CapsuleId,
            grant: &CapabilityGrant,
        ) -> Option<RecheckDenial> {
            let engines = self
                .plane
                .engines
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let cached = engines.get(&self.tenant)?;
            let kind = kind_name(grant);
            match cached.engine.authorize_use(&self.tenant, capsule_id, grant) {
                Ok(verdict) if verdict.allowed => None,
                Ok(verdict) => Some(RecheckDenial {
                    policy_version: cached.version.clone(),
                    detail: format!(
                        "policy version `{}` forbids the `{kind}` grant for tenant `{}` \
                         (deciding policies: {})",
                        cached.version,
                        self.tenant,
                        verdict.reasons.join(", ")
                    ),
                }),
                // An evaluation failure refuses closed: a policy that
                // errors is not a policy that permits, and the denial
                // names the version whose evaluation failed.
                Err(error) => Some(RecheckDenial {
                    policy_version: cached.version.clone(),
                    detail: format!(
                        "policy version `{}` failed to evaluate the `{kind}` recheck ({error}) — \
                         refusing closed",
                        cached.version
                    ),
                }),
            }
        }
    }

    /// What a successful admission composition produces (R0.9 wave 2) —
    /// the additive fields of the journaled `CapsuleResolution`.
    #[derive(Debug, Default)]
    pub(crate) struct AdmissionOutcome {
        /// The active policy version the admission was decided under
        /// (`None` in the unconfigured posture).
        pub(crate) policy_version: Option<String>,
        /// The overlays applied, sorted by name (`None` when none).
        pub(crate) overlays: Option<Vec<String>>,
        /// The effective grants after intersection (present exactly when
        /// an overlay applied).
        pub(crate) effective_grants: Option<BTreeSet<CapabilityGrant>>,
        /// The clamped budget (present exactly when clamping changed the
        /// declaration).
        pub(crate) clamped_budget: Option<ResourceBudget>,
    }

    /// A refused admission, shaped by where the refusal came from.
    #[derive(Debug)]
    pub(crate) enum AdmissionRefusal {
        /// Cedar forbade the admission or some declared grants. The route
        /// journals one `capsule_denied` per forbidden grant (pinned to
        /// `version`) and answers `403`.
        Policy {
            /// The active version whose verdict refused.
            version: String,
            /// The declared grants policy forbids (empty for a pure
            /// decision-1 refusal, which is not capability-shaped).
            forbidden: Vec<CapabilityGrant>,
            /// Human-facing context.
            detail: String,
        },
        /// The declared budget exceeds a refuse-field bound (`422`).
        Budget {
            /// The budget field that exceeded (`max_tokens` / `max_cost_usd`).
            field: &'static str,
            /// What the manifest declared.
            declared: String,
            /// The tightest enclosing bound.
            bound: String,
        },
        /// The store or the engine failed (500-class; a typed internal
        /// error, never a panic).
        Internal(String),
    }

    /// The tightest value of one budget field across the enclosing layers
    /// that declare it (`None` when no layer bounds the field).
    fn tightest_u64(
        layers: &[Option<&ResourceBudget>],
        pick: impl Fn(&ResourceBudget) -> Option<u64>,
    ) -> Option<u64> {
        layers
            .iter()
            .filter_map(|layer| layer.and_then(&pick))
            .min()
    }

    /// The tightest cost bound across the enclosing layers that declare
    /// one (evidence-grade `f64`, matching `ResourceBudget`).
    fn tightest_cost(layers: &[Option<&ResourceBudget>]) -> Option<f64> {
        layers
            .iter()
            .filter_map(|layer| layer.and_then(|budget| budget.max_cost_usd))
            .reduce(f64::min)
    }

    /// The admission composition (R0.9 wave 2): Cedar decisions 1 and 2,
    /// then the structural overlay narrowing, then the budget clamp —
    /// in that order, because Cedar judges the guest's *declaration*
    /// (what the capsule asks for) while the arithmetic narrows what it
    /// actually gets; the effective set can only be a subset of the
    /// declared grants policy just permitted, so the composition is
    /// sound.
    pub(crate) async fn compose_admission(
        store: &Arc<dyn ServerStore>,
        plane: &Arc<CapsulePolicyPlane>,
        tenant: &str,
        record: &CapsuleRecord,
        run_budget: Option<&ResourceBudget>,
        tenant_ceiling: Option<&ResourceBudget>,
    ) -> Result<AdmissionOutcome, AdmissionRefusal> {
        // Decisions 1 and 2 — only when the tenant has an active policy
        // (the unconfigured posture: module docs).
        let mut policy_version = None;
        if let Some(active) = store
            .active_capsule_policy(tenant)
            .await
            .map_err(AdmissionRefusal::Internal)?
        {
            let engine = CedarEngine::parse(&active.policy_text).map_err(|e| {
                AdmissionRefusal::Internal(format!(
                    "active capsule policy `{}` no longer parses: {e}",
                    active.version
                ))
            })?;
            let verdict = engine
                .authorize_admission(tenant, record)
                .map_err(|e| AdmissionRefusal::Internal(e.to_string()))?;
            if !verdict.allowed {
                return Err(AdmissionRefusal::Policy {
                    version: active.version.clone(),
                    forbidden: Vec::new(),
                    detail: format!(
                        "policy version `{}` refuses to admit capsule `{}` (identity `{}` \
                         version `{}`) to tenant `{tenant}` (deciding policies: {})",
                        active.version,
                        record.capsule_id,
                        record.manifest.identity.name,
                        record.manifest.version,
                        verdict.reasons.join(", ")
                    ),
                });
            }
            let forbidden = engine
                .forbidden_grants(tenant, record)
                .map_err(|e| AdmissionRefusal::Internal(e.to_string()))?;
            if !forbidden.is_empty() {
                let kinds: Vec<&str> = forbidden.iter().map(kind_name).collect();
                return Err(AdmissionRefusal::Policy {
                    version: active.version.clone(),
                    forbidden,
                    detail: format!(
                        "policy version `{}` forbids grants the manifest declares ({}) for \
                         tenant `{tenant}` — a capsule may declare fewer grants than policy \
                         permits, never more",
                        active.version,
                        kinds.join(", ")
                    ),
                });
            }
            // The admission read doubles as the revocation cache's
            // refresh: rechecks serve exactly the verdict admission
            // computed, parsed from the same registered text.
            plane
                .install(tenant, &active.version, &active.policy_text)
                .map_err(|e| AdmissionRefusal::Internal(e.to_string()))?;
            policy_version = Some(active.version);
        }

        // The overlay narrowing: structural intersection with every
        // applicable overlay, independent of whether Cedar spoke. This
        // is the double enforcement's arithmetic half — an overlay
        // hand-crafted past the policy plane still cannot widen the
        // effective set.
        let mut tenant_overlays = store
            .list_capsule_overlays(tenant)
            .await
            .map_err(AdmissionRefusal::Internal)?;
        tenant_overlays.sort_by(|a, b| a.overlay.name.cmp(&b.overlay.name));
        let mut overlays_applied = Vec::new();
        let mut effective = record.manifest.capabilities.clone();
        for overlay_record in &tenant_overlays {
            if overlay_record
                .overlay
                .applies_to(&record.manifest.identity.name)
            {
                effective = overlay_record.overlay.effective_grants(&effective);
                overlays_applied.push(overlay_record.overlay.name.clone());
            }
        }
        let (overlays, effective_grants) = if overlays_applied.is_empty() {
            (None, None)
        } else {
            (Some(overlays_applied), Some(effective))
        };

        // Budget composition: the declared budget clamped field-wise
        // against the run's bounds and the tenant ceiling. Fuel, memory,
        // wall time, and output size *clamp* — they are enforcement-
        // local resources the host bounds regardless, so the tighter
        // value is what would execute anyway and journaling the clamp
        // is the whole job. Tokens and cost *refuse* when the
        // declaration exceeds the bound: they are tenant-shared
        // accounting axes, and silently granting the capsule a cheaper
        // accounting basis than it declared would mis-report where the
        // budget went — the refusal is the operator-visible shape of
        // "this manifest and this run disagree about money."
        let declared = &record.manifest.budget;
        let layers = [run_budget, tenant_ceiling];
        let mut clamped = declared.clone();
        for layer in layers.iter().flatten() {
            clamped = clamped.clamp(layer);
        }
        if let (Some(want), Some(bound)) = (
            declared.max_tokens,
            tightest_u64(&layers, |budget| budget.max_tokens),
        ) {
            if want > bound {
                return Err(AdmissionRefusal::Budget {
                    field: "max_tokens",
                    declared: want.to_string(),
                    bound: bound.to_string(),
                });
            }
        }
        if let (Some(want), Some(bound)) = (declared.max_cost_usd, tightest_cost(&layers)) {
            if want > bound {
                return Err(AdmissionRefusal::Budget {
                    field: "max_cost_usd",
                    declared: want.to_string(),
                    bound: bound.to_string(),
                });
            }
        }
        let clamped_budget = if &clamped != declared {
            Some(clamped)
        } else {
            None
        };

        Ok(AdmissionOutcome {
            policy_version,
            overlays,
            effective_grants,
            clamped_budget,
        })
    }

    /// The registration-time authorization (decisions 1 and 2 over the
    /// manifest as submitted). Runs the same engine the resolution path
    /// uses, without the resolution's journaling (registration has no
    /// run context — the refusal is the typed `403` alone).
    pub(crate) async fn authorize_registration(
        store: &Arc<dyn ServerStore>,
        plane: &Arc<CapsulePolicyPlane>,
        tenant: &str,
        record: &CapsuleRecord,
    ) -> Result<(), AdmissionRefusal> {
        let Some(active) = store
            .active_capsule_policy(tenant)
            .await
            .map_err(AdmissionRefusal::Internal)?
        else {
            return Ok(());
        };
        let engine = CedarEngine::parse(&active.policy_text).map_err(|e| {
            AdmissionRefusal::Internal(format!(
                "active capsule policy `{}` no longer parses: {e}",
                active.version
            ))
        })?;
        let verdict = engine
            .authorize_admission(tenant, record)
            .map_err(|e| AdmissionRefusal::Internal(e.to_string()))?;
        if !verdict.allowed {
            return Err(AdmissionRefusal::Policy {
                version: active.version.clone(),
                forbidden: Vec::new(),
                detail: format!(
                    "policy version `{}` refuses to admit capsule `{}` to tenant `{tenant}` \
                     (deciding policies: {})",
                    active.version,
                    record.capsule_id,
                    verdict.reasons.join(", ")
                ),
            });
        }
        let forbidden = engine
            .forbidden_grants(tenant, record)
            .map_err(|e| AdmissionRefusal::Internal(e.to_string()))?;
        if !forbidden.is_empty() {
            let kinds: Vec<&str> = forbidden.iter().map(kind_name).collect();
            return Err(AdmissionRefusal::Policy {
                version: active.version.clone(),
                forbidden,
                detail: format!(
                    "policy version `{}` forbids grants the manifest declares ({}) for tenant \
                     `{tenant}`",
                    active.version,
                    kinds.join(", ")
                ),
            });
        }
        // Registration is an admission too: the recheck cache learns the
        // verdict here rather than waiting for a resolution.
        plane
            .install(tenant, &active.version, &active.policy_text)
            .map_err(|e| AdmissionRefusal::Internal(e.to_string()))?;
        Ok(())
    }

    /// Decision 3 at attach time: the overlay's legality against every
    /// registered capsule it would narrow. With no active policy the
    /// unconfigured posture allows the attach — and the intersection
    /// still cannot widen anything, which is precisely the wave's
    /// double-enforcement claim. With zero matching capsules the
    /// overlay is evaluated once against the tenant itself (nothing to
    /// widen yet).
    pub(crate) async fn authorize_overlay_attach(
        store: &Arc<dyn ServerStore>,
        tenant: &str,
        overlay: &CapsuleOverlay,
    ) -> Result<(), AdmissionRefusal> {
        let Some(active) = store
            .active_capsule_policy(tenant)
            .await
            .map_err(AdmissionRefusal::Internal)?
        else {
            return Ok(());
        };
        let engine = CedarEngine::parse(&active.policy_text).map_err(|e| {
            AdmissionRefusal::Internal(format!(
                "active capsule policy `{}` no longer parses: {e}",
                active.version
            ))
        })?;
        let capsules = store
            .list_capsules(tenant)
            .await
            .map_err(AdmissionRefusal::Internal)?;
        let mut matched = false;
        for record in &capsules {
            if !overlay.applies_to(&record.manifest.identity.name) {
                continue;
            }
            matched = true;
            let verdict = engine
                .authorize_overlay(tenant, overlay, Some(record))
                .map_err(|e| AdmissionRefusal::Internal(e.to_string()))?;
            if !verdict.allowed {
                return Err(AdmissionRefusal::Policy {
                    version: active.version.clone(),
                    forbidden: Vec::new(),
                    detail: format!(
                        "policy version `{}` refuses overlay `{}` against capsule `{}` \
                         (identity `{}`) for tenant `{tenant}` (deciding policies: {})",
                        active.version,
                        overlay.name,
                        record.capsule_id,
                        record.manifest.identity.name,
                        verdict.reasons.join(", ")
                    ),
                });
            }
        }
        if !matched {
            let verdict = engine
                .authorize_overlay(tenant, overlay, None)
                .map_err(|e| AdmissionRefusal::Internal(e.to_string()))?;
            if !verdict.allowed {
                return Err(AdmissionRefusal::Policy {
                    version: active.version.clone(),
                    forbidden: Vec::new(),
                    detail: format!(
                        "policy version `{}` refuses overlay `{}` for tenant `{tenant}` \
                         (deciding policies: {})",
                        active.version,
                        overlay.name,
                        verdict.reasons.join(", ")
                    ),
                });
            }
        }
        Ok(())
    }

    /// Register the operator's config-file policies (best effort, at
    /// startup): each file parses before it registers — a file that
    /// fails either is logged and skipped, never fatal, so a typo'd
    /// policy cannot keep the server down (the plane simply starts
    /// unenforced, which the unconfigured posture already prices in).
    /// Config files register into the **default tenant**: they are the
    /// single-tenant/dev posture; multi-tenant deployments register
    /// per-tenant policies through the API.
    pub(crate) async fn load_config_policies(store: Arc<dyn ServerStore>, files: Vec<PathBuf>) {
        for file in files {
            let text = match std::fs::read_to_string(&file) {
                Ok(text) => text,
                Err(error) => {
                    tracing::warn!(path = %file.display(), %error, "capsule policy file unreadable; skipped");
                    continue;
                }
            };
            if let Err(error) = CedarEngine::parse(&text) {
                tracing::warn!(path = %file.display(), %error, "capsule policy file does not parse; skipped");
                continue;
            }
            let record = CapsulePolicyRecord {
                version: derive_capsule_policy_version(&text),
                policy_text: text,
                registered_at: Utc::now(),
            };
            match store
                .put_capsule_policy(crate::auth::DEFAULT_TENANT, &record)
                .await
            {
                Ok(CapsulePolicyWrite::Created) => {
                    tracing::info!(
                        path = %file.display(),
                        version = %record.version,
                        "registered config-file capsule policy (inactive until activated)"
                    );
                }
                Ok(CapsulePolicyWrite::Converged) => {}
                Ok(CapsulePolicyWrite::Conflict) => {
                    tracing::warn!(
                        path = %file.display(),
                        version = %record.version,
                        "capsule policy version names a different body; config file skipped"
                    );
                }
                Err(error) => {
                    tracing::warn!(path = %file.display(), %error, "failed to register capsule policy file");
                }
            }
        }
    }

    /// Warm the revocation cache from the store at startup (best
    /// effort): every tenant's active engine is installed, so a
    /// restarted process re-arms the recheck seam without waiting for
    /// the next policy mutation or admission. See the module docs for
    /// the cache-epoch honesty.
    pub(crate) async fn preload_active_policies(
        store: Arc<dyn ServerStore>,
        plane: Arc<CapsulePolicyPlane>,
    ) {
        match store.list_active_capsule_policies().await {
            Ok(actives) => {
                for (tenant, record) in actives {
                    if let Err(error) = plane.install(&tenant, &record.version, &record.policy_text)
                    {
                        tracing::warn!(
                            %tenant,
                            version = %record.version,
                            %error,
                            "active capsule policy failed to parse at preload; the tenant's \
                             rechecks stay unenforced until the next admission"
                        );
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "capsule policy preload failed; rechecks start uncached")
            }
        }
    }

    /// The activation record routes build and the store persists.
    pub(crate) fn activation(version: &str) -> CapsulePolicyActivation {
        CapsulePolicyActivation {
            version: version.to_string(),
            activated_at: Utc::now(),
        }
    }

    /// A manifest-shaped record for the registration path, which has no
    /// stored [`CapsuleRecord`] yet — the record the registry *would*
    /// store, so registration and resolution evaluate the same entity.
    pub(crate) fn prospective_record(
        capsule_id: CapsuleId,
        manifest: &CapsuleManifest,
    ) -> CapsuleRecord {
        CapsuleRecord {
            capsule_id,
            manifest: manifest.clone(),
            registered_at: Utc::now(),
        }
    }
}

/// The typed refusal every capsule-policy route answers in a build
/// without the `capsules` feature: `503 capsule_policy_unavailable`,
/// naming the remedy. Fail closed, never a silent skip — and never a
/// panic: a server without the feature is a complete server with a
/// smaller surface, and the missing half says so honestly.
#[cfg(not(feature = "capsules"))]
pub(crate) fn plane_unavailable() -> crate::ApiError {
    crate::ApiError::new(
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "capsule_policy_unavailable",
        "the capsule policy plane requires the `capsules` feature — rebuild rusty-agent-server \
         with `--features capsules`; capsule workloads fail closed rather than running \
         ungoverned"
            .into(),
    )
}
