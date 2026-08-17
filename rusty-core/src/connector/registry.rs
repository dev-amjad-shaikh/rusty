//! The connector registry: manifests in, tenant-scoped instances out.
//!
//! The registry is the admission boundary. Manifests are validated and
//! hash-verified at registration — idempotently, by content hash — and
//! instances are minted per tenant with credentials resolved through the
//! [`CredentialBroker`] seam: a missing slot does not error the call, it
//! produces an instance in `failed` state with a reason naming the slot
//! and tenant, so review surfaces see the gap rather than an absence.
//!
//! Listings are deterministic (manifests by `(id, hash)`, instances by
//! `(tenant, instance id)`), and the health sweep is the one entry point
//! that re-checks live instances: success recovers a degraded instance
//! and refreshes its catalog generation; failure degrades after the
//! configured number of consecutive misses.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::conn_err;
use super::credential::{CredentialBroker, CredentialHandle};
use super::instance::{
    CatalogGeneration, CatalogPin, ConnectorInstance, DEFAULT_DEGRADE_AFTER_FAILURES,
    LifecycleState,
};
use super::manifest::ConnectorManifest;
use super::provider::{ConnectorProvider, ProviderSession, default_provider};
use crate::error::Result;

/// One registered manifest plus the provider that realizes it.
struct ManifestEntry {
    manifest: ConnectorManifest,
    provider: Arc<dyn ConnectorProvider>,
}

/// One instance plus its runtime baggage: the provider, the resolved
/// credential handles (opaque; secrets never sit on the instance struct),
/// and the live session once connected.
struct InstanceEntry {
    instance: ConnectorInstance,
    provider: Arc<dyn ConnectorProvider>,
    credentials: Vec<CredentialHandle>,
    session: Option<Box<dyn ProviderSession>>,
}

/// The outcome of one instance's health check within a sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepOutcome {
    /// The checked instance.
    pub instance_id: String,
    /// The state before the check.
    pub previous: LifecycleState,
    /// The state after the check.
    pub current: LifecycleState,
    /// `true` when the check minted a new catalog generation.
    pub catalog_bumped: bool,
}

/// The connector registry.
///
/// Not `Clone`: instance sessions own live children and transports. All
/// ordering guarantees come from `BTreeMap` keys plus explicit sorts at
/// listing time.
pub struct ConnectorRegistry {
    /// Manifests keyed by content hash — registration is idempotent by
    /// construction: the same bytes key the same slot.
    manifests: BTreeMap<String, ManifestEntry>,
    /// Instances keyed by registry-minted id (`inst-NNNNNN`; zero-padded,
    /// so key order is mint order).
    instances: BTreeMap<String, InstanceEntry>,
    next_instance_seq: u64,
    degrade_after: u32,
}

impl std::fmt::Debug for ConnectorRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectorRegistry")
            .field("manifests", &self.manifests.len())
            .field("instances", &self.instances.len())
            .field("degrade_after", &self.degrade_after)
            .finish()
    }
}

impl Default for ConnectorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectorRegistry {
    /// An empty registry with the default degradation threshold
    /// ([`DEFAULT_DEGRADE_AFTER_FAILURES`]).
    pub fn new() -> Self {
        Self {
            manifests: BTreeMap::new(),
            instances: BTreeMap::new(),
            next_instance_seq: 1,
            degrade_after: DEFAULT_DEGRADE_AFTER_FAILURES,
        }
    }

    /// Builder-style: consecutive health-check failures before a healthy
    /// instance degrades. Values below 1 clamp to 1 (every failure
    /// degrades immediately).
    pub fn with_degrade_after(mut self, degrade_after: u32) -> Self {
        self.degrade_after = degrade_after.max(1);
        self
    }

    /// Register a manifest with an explicit provider.
    ///
    /// Re-validates and re-verifies the content hash (deserialization
    /// bypasses the validating constructor, so admission re-checks).
    /// Idempotent by hash: re-registering the same content returns the
    /// existing hash without replacing the entry.
    pub fn register_manifest(
        &mut self,
        manifest: ConnectorManifest,
        provider: Arc<dyn ConnectorProvider>,
    ) -> Result<String> {
        manifest.validate()?;
        if !manifest.verify_hash() {
            return Err(conn_err(format!(
                "manifest `{}` hash does not match its content; recompute with `ConnectorManifest::new`",
                manifest.id
            )));
        }
        let hash = manifest.hash.clone();
        if !self.manifests.contains_key(&hash) {
            self.manifests
                .insert(hash.clone(), ManifestEntry { manifest, provider });
        }
        Ok(hash)
    }

    /// Register a manifest with the default provider for its kind
    /// ([`default_provider`]).
    pub fn register_manifest_with_default(
        &mut self,
        manifest: ConnectorManifest,
    ) -> Result<String> {
        let provider = default_provider(&manifest)?;
        self.register_manifest(manifest, provider)
    }

    /// A registered manifest by content hash.
    pub fn manifest(&self, hash: &str) -> Option<&ConnectorManifest> {
        self.manifests.get(hash).map(|entry| &entry.manifest)
    }

    /// All manifests, sorted by `(id, hash)` — deterministic regardless of
    /// registration order.
    pub fn list_manifests(&self) -> Vec<&ConnectorManifest> {
        let mut manifests: Vec<&ConnectorManifest> = self
            .manifests
            .values()
            .map(|entry| &entry.manifest)
            .collect();
        manifests.sort_by(|left, right| left.id.cmp(&right.id).then(left.hash.cmp(&right.hash)));
        manifests
    }

    /// Instantiate a manifest for one tenant.
    ///
    /// Every declared credential slot is resolved through `broker`. A slot
    /// the broker cannot answer does not error the call: the instance is
    /// minted in `failed` state with a reason naming the slot and tenant,
    /// so the gap is visible to review surfaces. A broker *error* (vault
    /// malfunction, not absence) does abort instantiation.
    ///
    /// Returns the minted instance id (`inst-NNNNNN`).
    pub fn instantiate(
        &mut self,
        manifest_hash: &str,
        tenant_id: &str,
        broker: &dyn CredentialBroker,
    ) -> Result<String> {
        self.instantiate_with_config(manifest_hash, tenant_id, BTreeMap::new(), broker)
    }

    /// Instantiate with the manifest's non-secret config params supplied.
    ///
    /// The config must name exactly the params the manifest declares — a
    /// missing or empty value and an undeclared key are both errors, never
    /// a partially configured instance. The values land on the instance
    /// and reach the provider at connect time, where base-url placeholders
    /// substitute from them.
    pub fn instantiate_with_config(
        &mut self,
        manifest_hash: &str,
        tenant_id: &str,
        config: BTreeMap<String, String>,
        broker: &dyn CredentialBroker,
    ) -> Result<String> {
        let entry = self.manifests.get(manifest_hash).ok_or_else(|| {
            conn_err(format!(
                "no manifest registered under hash `{manifest_hash}`"
            ))
        })?;

        for param in &entry.manifest.config_params {
            if !config.contains_key(&param.name) {
                return Err(conn_err(format!(
                    "config param `{}` requires a value in `config`",
                    param.name
                )));
            }
        }
        for key in config.keys() {
            if !entry.manifest.config_params.iter().any(|p| &p.name == key) {
                return Err(conn_err(format!(
                    "config key `{key}` is not a config param the manifest declares"
                )));
            }
        }

        let instance_id = format!("inst-{:06}", self.next_instance_seq);
        let mut instance =
            ConnectorInstance::new(&instance_id, &entry.manifest.id, manifest_hash, tenant_id)?
                .with_config(config)?;

        let mut credentials = Vec::with_capacity(entry.manifest.credential_slots.len());
        let mut missing = Vec::new();
        for slot in &entry.manifest.credential_slots {
            match broker.resolve(tenant_id, &slot.name)? {
                Some(handle) => credentials.push(handle),
                None => missing.push(slot.name.clone()),
            }
        }
        if !missing.is_empty() {
            let slots = missing
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ");
            instance.fail_pending(format!(
                "credential slot(s) {slots} unresolved for tenant `{tenant_id}`"
            ))?;
        }

        self.next_instance_seq += 1;
        self.instances.insert(
            instance_id.clone(),
            InstanceEntry {
                instance,
                provider: Arc::clone(&entry.provider),
                credentials,
                session: None,
            },
        );
        Ok(instance_id)
    }

    /// An instance by id.
    pub fn instance(&self, instance_id: &str) -> Option<&ConnectorInstance> {
        self.instances.get(instance_id).map(|entry| &entry.instance)
    }

    /// Instances — optionally one tenant's — sorted by
    /// `(tenant, instance id)`.
    pub fn list_instances(&self, tenant: Option<&str>) -> Vec<&ConnectorInstance> {
        let mut instances: Vec<&ConnectorInstance> = self
            .instances
            .values()
            .map(|entry| &entry.instance)
            .filter(|instance| tenant.is_none_or(|t| instance.tenant_id == t))
            .collect();
        instances.sort_by(|left, right| {
            left.tenant_id
                .cmp(&right.tenant_id)
                .then(left.instance_id.cmp(&right.instance_id))
        });
        instances
    }

    /// The current catalog generation of an instance, if it has ever been
    /// healthy.
    pub fn catalog(&self, instance_id: &str) -> Option<&CatalogGeneration> {
        self.instance(instance_id)
            .and_then(ConnectorInstance::catalog)
    }

    /// The pin a consumer should hold against the instance's current
    /// catalog generation.
    pub fn catalog_pin(&self, instance_id: &str) -> Option<CatalogPin> {
        self.instance(instance_id)
            .and_then(ConnectorInstance::catalog_pin)
    }

    /// Drive one instance through connection: `begin_connect`, provider
    /// connect, initial catalog derivation.
    ///
    /// Provider failures do not error this call — they land the instance
    /// in `failed` with the bounded reason, per the lifecycle. Guard
    /// violations (unknown id, disabled instance, already connecting) do
    /// error.
    pub async fn connect(&mut self, instance_id: &str, now_ms: u64) -> Result<()> {
        let entry = self
            .instances
            .get_mut(instance_id)
            .ok_or_else(|| conn_err(format!("no instance registered under id `{instance_id}`")))?;
        entry.instance.begin_connect()?;
        let manifest = &self
            .manifests
            .get(&entry.instance.manifest_hash)
            .ok_or_else(|| {
                conn_err(format!(
                    "instance `{instance_id}` pins an unregistered manifest"
                ))
            })?
            .manifest;

        match entry
            .provider
            .connect(manifest, &entry.credentials, entry.instance.config())
            .await
        {
            Err(error) => {
                entry.instance.record_connect_failure(error.to_string())?;
            }
            Ok(mut session) => match session.catalog().await {
                Ok(tools) => {
                    entry.instance.record_connect_success(now_ms, tools)?;
                    entry.session = Some(session);
                }
                Err(error) => {
                    let _ = session.shutdown().await;
                    entry.instance.record_connect_failure(error.to_string())?;
                }
            },
        }
        Ok(())
    }

    /// Re-check one `healthy` or `degraded` instance against its live
    /// session.
    pub async fn check_health(&mut self, instance_id: &str, now_ms: u64) -> Result<SweepOutcome> {
        let degrade_after = self.degrade_after;
        let entry = self
            .instances
            .get_mut(instance_id)
            .ok_or_else(|| conn_err(format!("no instance registered under id `{instance_id}`")))?;
        let previous = entry.instance.state().clone();
        if !matches!(
            previous,
            LifecycleState::Healthy | LifecycleState::Degraded { .. }
        ) {
            return Err(conn_err(format!(
                "instance `{instance_id}` is `{}`; health checks apply to `healthy` and `degraded`",
                previous.name()
            )));
        }

        let bumped = match entry.session.as_mut() {
            // Healthy without a session is a broken invariant; treat it as
            // a failed check rather than trusting the label.
            None => {
                entry.instance.record_health_failure(
                    "instance has no live session",
                    now_ms,
                    degrade_after,
                )?;
                false
            }
            Some(session) => match session.catalog().await {
                Ok(tools) => entry.instance.record_health_success(now_ms, tools)?,
                Err(error) => {
                    entry.instance.record_health_failure(
                        error.to_string(),
                        now_ms,
                        degrade_after,
                    )?;
                    false
                }
            },
        };
        Ok(SweepOutcome {
            instance_id: instance_id.to_owned(),
            previous,
            current: entry.instance.state().clone(),
            catalog_bumped: bumped,
        })
    }

    /// The health sweep: re-check every `healthy`/`degraded` instance, in
    /// instance-id order. Per-instance guard violations cannot occur here
    /// (states are filtered up front), so the sweep always covers the full
    /// set.
    pub async fn health_sweep(&mut self, now_ms: u64) -> Vec<SweepOutcome> {
        let ids: Vec<String> = self
            .instances
            .values()
            .filter(|entry| {
                matches!(
                    entry.instance.state(),
                    LifecycleState::Healthy | LifecycleState::Degraded { .. }
                )
            })
            .map(|entry| entry.instance.instance_id.clone())
            .collect();
        let mut outcomes = Vec::with_capacity(ids.len());
        for id in ids {
            // The state filter above makes this infallible.
            if let Ok(outcome) = self.check_health(&id, now_ms).await {
                outcomes.push(outcome);
            }
        }
        outcomes
    }

    /// Disable an instance, shutting its session down first.
    pub async fn disable(&mut self, instance_id: &str) -> Result<()> {
        let entry = self
            .instances
            .get_mut(instance_id)
            .ok_or_else(|| conn_err(format!("no instance registered under id `{instance_id}`")))?;
        entry.instance.disable()?;
        if let Some(session) = entry.session.take() {
            session.shutdown().await?;
        }
        Ok(())
    }

    /// Re-enable a disabled instance; it returns to `pending` and must
    /// connect again before serving.
    pub fn enable(&mut self, instance_id: &str) -> Result<()> {
        let entry = self
            .instances
            .get_mut(instance_id)
            .ok_or_else(|| conn_err(format!("no instance registered under id `{instance_id}`")))?;
        entry.instance.enable()
    }
}
