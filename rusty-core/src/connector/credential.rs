//! Credential handles and the broker seam.
//!
//! No connector instance ever holds a raw secret on its struct. Secrets
//! arrive at instantiation from a [`CredentialBroker`] — a deliberately
//! minimal seam (a synchronous lookup by `(tenant, slot)`) that the
//! server's credential vault and the existing `broker` module's machinery
//! can stand behind — and travel as opaque [`CredentialHandle`]s whose
//! `Debug` and `Serialize` forms are redacted by construction. Host-side
//! provider code reads the bytes through [`CredentialHandle::secret`] at
//! the moment of use; tool code never sees them.

use std::collections::BTreeMap;

use serde::{Serialize, Serializer};

use super::conn_err;
use crate::error::Result;

/// Maximum size of one credential secret, in bytes.
pub const MAX_SECRET_BYTES: usize = 4 * 1024;

/// The redaction marker emitted where a secret would otherwise appear.
pub const REDACTED: &str = "[redacted]";

/// An opaque handle on one resolved credential.
///
/// Deliberately not `Clone`: handles are issued at instance creation and
/// held by exactly the instance entry that resolved them, so duplication
/// paths never silently multiply secret copies. `Debug` and `Serialize`
/// emit the tenant, the slot, and [`REDACTED`] — never secret bytes — so
/// a handle captured in a log line, a panic message, or a Studio payload
/// is safe by construction.
pub struct CredentialHandle {
    tenant: String,
    slot: String,
    secret: String,
}

impl CredentialHandle {
    /// A handle over `secret` resolved for `(tenant, slot)`.
    ///
    /// Secrets are bounded (`MAX_SECRET_BYTES`) and non-empty: an empty
    /// secret is an unresolved slot wearing a handle's clothes, and it
    /// fails here rather than at the first provider call.
    pub fn new(
        tenant: impl Into<String>,
        slot: impl Into<String>,
        secret: impl Into<String>,
    ) -> Result<Self> {
        let tenant = tenant.into();
        let slot = slot.into();
        let secret = secret.into();
        if tenant.is_empty() || slot.is_empty() {
            return Err(conn_err("credential handles require a tenant and a slot"));
        }
        if secret.is_empty() || secret.len() > MAX_SECRET_BYTES {
            return Err(conn_err(format!(
                "credential for slot `{slot}` must be non-empty and at most {MAX_SECRET_BYTES} bytes"
            )));
        }
        Ok(Self {
            tenant,
            slot,
            secret,
        })
    }

    /// The tenant the credential was resolved for.
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// The slot the credential was resolved under.
    pub fn slot(&self) -> &str {
        &self.slot
    }

    /// The secret bytes. Host-side provider code only: this is the one
    /// door the secret leaves through, at the moment of use (e.g. as an
    /// auth header value), never into tool code or serialized evidence.
    pub fn secret(&self) -> &str {
        &self.secret
    }
}

impl std::fmt::Debug for CredentialHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialHandle")
            .field("tenant", &self.tenant)
            .field("slot", &self.slot)
            .field("secret", &REDACTED)
            .finish()
    }
}

impl Serialize for CredentialHandle {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("CredentialHandle", 3)?;
        state.serialize_field("tenant", &self.tenant)?;
        state.serialize_field("slot", &self.slot)?;
        state.serialize_field("secret", &REDACTED)?;
        state.end()
    }
}

/// The credential seam: resolve the secret for one `(tenant, slot)` pair.
///
/// Synchronous and total: instantiation is a registry-side decision that
/// must not await vault IO, so brokers answer from whatever cache or
/// sealed store they maintain. `Ok(None)` is the ordinary "no credential
/// under this name" answer and drives the instance to `failed` with a
/// reason naming the slot; `Err` is a broker malfunction and aborts the
/// instantiation itself.
pub trait CredentialBroker: std::fmt::Debug + Send + Sync {
    /// Resolve `(tenant, slot)` into a handle, or `None` when the tenant
    /// holds no credential under that slot name.
    fn resolve(&self, tenant: &str, slot: &str) -> Result<Option<CredentialHandle>>;
}

/// An in-memory broker for tests and local wiring.
///
/// Secrets live in the map; every [`CredentialBroker::resolve`] mints a
/// fresh handle, so the map's copy is the only long-lived one. Its
/// `Debug` lists the known `(tenant, slot)` pairs with values redacted.
#[derive(Default)]
pub struct InMemoryCredentialBroker {
    secrets: BTreeMap<(String, String), String>,
}

impl InMemoryCredentialBroker {
    /// An empty broker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Store `secret` under `(tenant, slot)`, replacing any prior value.
    pub fn insert(
        &mut self,
        tenant: impl Into<String>,
        slot: impl Into<String>,
        secret: impl Into<String>,
    ) -> &mut Self {
        self.secrets
            .insert((tenant.into(), slot.into()), secret.into());
        self
    }
}

impl std::fmt::Debug for InMemoryCredentialBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryCredentialBroker")
            .field("slots", &self.secrets.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl CredentialBroker for InMemoryCredentialBroker {
    fn resolve(&self, tenant: &str, slot: &str) -> Result<Option<CredentialHandle>> {
        self.secrets
            .get(&(tenant.to_owned(), slot.to_owned()))
            .map(|secret| CredentialHandle::new(tenant, slot, secret.clone()))
            .transpose()
    }
}
