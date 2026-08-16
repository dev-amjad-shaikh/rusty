//! Tenant-scoped connector instances and their lifecycle.
//!
//! A [`ConnectorInstance`] pins a manifest by content hash and carries the
//! lifecycle state machine
//! (`pending → connecting → healthy | degraded | failed`, plus
//! `disabled`). Transitions are explicit methods with guard rules — an
//! illegal transition is an error, never a silent no-op. All timestamps
//! are caller-injected logical milliseconds (`now_ms`), so instance
//! history is deterministic under replay and test.
//!
//! A healthy instance carries a [`CatalogGeneration`]: a monotonically
//! increasing per-instance number plus a content hash over the derived
//! catalog. Refresh bumps the generation only when the catalog bytes
//! change; consumers pin `(instance, generation, hash)` through a
//! [`CatalogPin`], never "latest".

use serde::{Deserialize, Serialize};

use super::conn_err;
use crate::error::Result;
use crate::tool::ToolCapability;

/// Maximum length of a tenant id.
pub const MAX_TENANT_ID_LEN: usize = 128;

/// Maximum size of the error string a `failed`/`degraded` state carries.
/// Longer reasons are truncated at a char boundary with a marker, so a
/// pathological provider error cannot turn instance state into an
/// unbounded payload.
pub const MAX_INSTANCE_ERROR_BYTES: usize = 512;

/// Default consecutive health-check failures before a healthy instance
/// degrades.
pub const DEFAULT_DEGRADE_AFTER_FAILURES: u32 = 3;

/// The lifecycle state of one connector instance.
///
/// `Failed` and `Degraded` carry the bounded last-error string. `Disabled`
/// is terminal until re-enabled: a disabled instance rejects connection
/// attempts rather than failing them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LifecycleState {
    /// Created, never connected.
    Pending,
    /// A connection attempt is in flight.
    Connecting,
    /// Connected and passing health checks.
    Healthy,
    /// Connected but failing health checks (at the configured threshold).
    Degraded {
        /// The bounded last health-check error.
        reason: String,
    },
    /// Connection or instantiation failed. Carries the bounded reason.
    Failed {
        /// The bounded failure reason.
        reason: String,
    },
    /// Administratively disabled; rejects connection attempts.
    Disabled,
}

impl LifecycleState {
    /// The state name as the serde tag spells it.
    pub fn name(&self) -> &'static str {
        match self {
            LifecycleState::Pending => "pending",
            LifecycleState::Connecting => "connecting",
            LifecycleState::Healthy => "healthy",
            LifecycleState::Degraded { .. } => "degraded",
            LifecycleState::Failed { .. } => "failed",
            LifecycleState::Disabled => "disabled",
        }
    }
}

/// One immutable catalog revision of a healthy instance.
///
/// `generation` is per-instance and monotonically increasing; `hash` is
/// the SHA-256 of the canonical serialization of `tools`. Two generations
/// with equal hashes cannot exist: refresh with unchanged bytes keeps the
/// current generation untouched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogGeneration {
    /// The per-instance generation number, starting at 1.
    pub generation: u64,
    /// SHA-256 over the canonical catalog serialization.
    pub hash: String,
    /// The derived tool catalog, sorted by tool name.
    pub tools: Vec<ToolCapability>,
    /// Logical time the generation was produced.
    pub produced_at_ms: u64,
}

/// A consumer's pin on one exact catalog generation.
///
/// The pin is the DeepSeek-Harness rule made concrete: a consumer inherits
/// the exact capability generation it was configured against, and
/// [`ConnectorInstance::verify_pin`] answers whether the instance still
/// serves exactly it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogPin {
    /// The pinned instance.
    pub instance_id: String,
    /// The pinned generation number.
    pub generation: u64,
    /// The pinned catalog content hash.
    pub hash: String,
}

/// A tenant-scoped instance of one connector manifest.
///
/// Identity fields are public and immutable after construction; state
/// moves only through the transition methods, which enforce the guard
/// rules of the lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorInstance {
    /// The registry-minted instance id (`inst-NNNNNN`).
    pub instance_id: String,
    /// The connector id from the manifest (denormalized for listing).
    pub connector_id: String,
    /// The content hash of the manifest this instance pins.
    pub manifest_hash: String,
    /// The tenant this instance serves.
    pub tenant_id: String,
    state: LifecycleState,
    consecutive_failures: u32,
    last_health_check_ms: Option<u64>,
    catalog: Option<CatalogGeneration>,
}

impl ConnectorInstance {
    /// A `pending` instance. `tenant_id` is validated here (bounded,
    /// control-free) since every later transition trusts it.
    pub fn new(
        instance_id: impl Into<String>,
        connector_id: impl Into<String>,
        manifest_hash: impl Into<String>,
        tenant_id: impl Into<String>,
    ) -> Result<Self> {
        let tenant_id = tenant_id.into();
        if tenant_id.is_empty()
            || tenant_id.len() > MAX_TENANT_ID_LEN
            || tenant_id.chars().any(char::is_control)
        {
            return Err(conn_err(format!(
                "tenant id must be non-empty, control-free, and at most {MAX_TENANT_ID_LEN} bytes"
            )));
        }
        Ok(Self {
            instance_id: instance_id.into(),
            connector_id: connector_id.into(),
            manifest_hash: manifest_hash.into(),
            tenant_id,
            state: LifecycleState::Pending,
            consecutive_failures: 0,
            last_health_check_ms: None,
            catalog: None,
        })
    }

    /// The current lifecycle state.
    pub fn state(&self) -> &LifecycleState {
        &self.state
    }

    /// Consecutive health/connection failures since the last success.
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// Logical time of the last completed health check, if any.
    pub fn last_health_check_ms(&self) -> Option<u64> {
        self.last_health_check_ms
    }

    /// The current catalog generation, if the instance has ever been
    /// healthy.
    pub fn catalog(&self) -> Option<&CatalogGeneration> {
        self.catalog.as_ref()
    }

    /// The pin a consumer should hold against this instance's current
    /// catalog, or `None` before the first healthy generation.
    pub fn catalog_pin(&self) -> Option<CatalogPin> {
        self.catalog.as_ref().map(|catalog| CatalogPin {
            instance_id: self.instance_id.clone(),
            generation: catalog.generation,
            hash: catalog.hash.clone(),
        })
    }

    /// `true` if `pin` names this instance and its *current* generation
    /// and hash — the exact-catalog check consumers run before relying on
    /// a previously reviewed tool set.
    pub fn verify_pin(&self, pin: &CatalogPin) -> bool {
        pin.instance_id == self.instance_id
            && self
                .catalog
                .as_ref()
                .is_some_and(|catalog| {
                    catalog.generation == pin.generation && catalog.hash == pin.hash
                })
    }

    /// `pending | failed | degraded → connecting`.
    ///
    /// Guard: a `disabled` instance cannot connect at all; a `healthy`
    /// instance is re-checked by health sweep, not reconnected; a
    /// `connecting` instance is already in flight.
    pub fn begin_connect(&mut self) -> Result<()> {
        match self.state {
            LifecycleState::Pending
            | LifecycleState::Failed { .. }
            | LifecycleState::Degraded { .. } => {
                self.state = LifecycleState::Connecting;
                Ok(())
            }
            LifecycleState::Disabled => Err(conn_err(format!(
                "instance `{}` is disabled and cannot connect",
                self.instance_id
            ))),
            LifecycleState::Healthy => Err(conn_err(format!(
                "instance `{}` is already healthy; use a health check to refresh it",
                self.instance_id
            ))),
            LifecycleState::Connecting => Err(conn_err(format!(
                "instance `{}` is already connecting",
                self.instance_id
            ))),
        }
    }

    /// `connecting → healthy` with the freshly derived catalog. Resets the
    /// failure counter and records the health-check time.
    pub fn record_connect_success(
        &mut self,
        now_ms: u64,
        tools: Vec<ToolCapability>,
    ) -> Result<()> {
        match self.state {
            LifecycleState::Connecting => {
                self.adopt_catalog(tools, now_ms);
                self.consecutive_failures = 0;
                self.last_health_check_ms = Some(now_ms);
                self.state = LifecycleState::Healthy;
                Ok(())
            }
            _ => Err(conn_err(format!(
                "instance `{}` is `{}`, not `connecting`",
                self.instance_id,
                self.state.name()
            ))),
        }
    }

    /// `connecting → failed` with the bounded reason.
    pub fn record_connect_failure(&mut self, reason: impl Into<String>) -> Result<()> {
        match self.state {
            LifecycleState::Connecting => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                self.state = LifecycleState::Failed {
                    reason: bound_reason(reason.into()),
                };
                Ok(())
            }
            _ => Err(conn_err(format!(
                "instance `{}` is `{}`, not `connecting`",
                self.instance_id,
                self.state.name()
            ))),
        }
    }

    /// `pending → failed`, for failures that precede any connection
    /// attempt — above all, unresolved credential slots at instantiation.
    pub fn fail_pending(&mut self, reason: impl Into<String>) -> Result<()> {
        match self.state {
            LifecycleState::Pending => {
                self.state = LifecycleState::Failed {
                    reason: bound_reason(reason.into()),
                };
                Ok(())
            }
            _ => Err(conn_err(format!(
                "instance `{}` is `{}`, not `pending`",
                self.instance_id,
                self.state.name()
            ))),
        }
    }

    /// A successful health check: `healthy | degraded → healthy`, the
    /// failure counter reset, the catalog refreshed. Returns `true` when
    /// the catalog bytes changed and a new generation was minted.
    pub fn record_health_success(
        &mut self,
        now_ms: u64,
        tools: Vec<ToolCapability>,
    ) -> Result<bool> {
        match self.state {
            LifecycleState::Healthy | LifecycleState::Degraded { .. } => {
                let bumped = self.adopt_catalog(tools, now_ms);
                self.consecutive_failures = 0;
                self.last_health_check_ms = Some(now_ms);
                self.state = LifecycleState::Healthy;
                Ok(bumped)
            }
            _ => Err(conn_err(format!(
                "instance `{}` is `{}`; health checks apply to `healthy` and `degraded`",
                self.instance_id,
                self.state.name()
            ))),
        }
    }

    /// A failed health check. The counter increments; at `degrade_after`
    /// consecutive failures the instance moves `healthy → degraded`
    /// (carrying the bounded reason). Returns `true` on the transition
    /// into `degraded`.
    pub fn record_health_failure(
        &mut self,
        reason: impl Into<String>,
        now_ms: u64,
        degrade_after: u32,
    ) -> Result<bool> {
        let reason = bound_reason(reason.into());
        match &self.state {
            LifecycleState::Healthy | LifecycleState::Degraded { .. } => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                self.last_health_check_ms = Some(now_ms);
                if self.consecutive_failures >= degrade_after.max(1) {
                    let was_degraded = matches!(self.state, LifecycleState::Degraded { .. });
                    self.state = LifecycleState::Degraded { reason };
                    Ok(!was_degraded)
                } else {
                    Ok(false)
                }
            }
            _ => Err(conn_err(format!(
                "instance `{}` is `{}`; health checks apply to `healthy` and `degraded`",
                self.instance_id,
                self.state.name()
            ))),
        }
    }

    /// Any state except `disabled` → `disabled`. The last-known catalog is
    /// retained for review surfaces, but the instance serves nothing.
    pub fn disable(&mut self) -> Result<()> {
        match self.state {
            LifecycleState::Disabled => Err(conn_err(format!(
                "instance `{}` is already disabled",
                self.instance_id
            ))),
            _ => {
                self.state = LifecycleState::Disabled;
                Ok(())
            }
        }
    }

    /// `disabled → pending`: the instance must connect again before it
    /// serves a catalog. The failure counter resets.
    pub fn enable(&mut self) -> Result<()> {
        match self.state {
            LifecycleState::Disabled => {
                self.consecutive_failures = 0;
                self.state = LifecycleState::Pending;
                Ok(())
            }
            _ => Err(conn_err(format!(
                "instance `{}` is `{}`, not `disabled`",
                self.instance_id,
                self.state.name()
            ))),
        }
    }

    /// Fold a freshly derived catalog into the generation chain. Returns
    /// `true` when the bytes changed and the generation advanced; equal
    /// bytes leave the generation — including its production time —
    /// untouched.
    fn adopt_catalog(&mut self, tools: Vec<ToolCapability>, now_ms: u64) -> bool {
        let value = serde_json::to_value(&tools)
            .expect("a ToolCapability vec always serializes");
        let hash = super::canonical_json_hash(&value);
        match &mut self.catalog {
            None => {
                self.catalog = Some(CatalogGeneration {
                    generation: 1,
                    hash,
                    tools,
                    produced_at_ms: now_ms,
                });
                true
            }
            Some(current) if current.hash == hash => false,
            Some(current) => {
                current.generation += 1;
                current.hash = hash;
                current.tools = tools;
                current.produced_at_ms = now_ms;
                true
            }
        }
    }
}

/// Bound an error string to [`MAX_INSTANCE_ERROR_BYTES`], truncating at a
/// char boundary with an explicit marker.
fn bound_reason(reason: String) -> String {
    if reason.len() <= MAX_INSTANCE_ERROR_BYTES {
        return reason;
    }
    const MARKER: &str = "…[truncated]";
    let budget = MAX_INSTANCE_ERROR_BYTES - MARKER.len();
    let mut end = budget;
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = reason[..end].to_owned();
    bounded.push_str(MARKER);
    bounded
}
