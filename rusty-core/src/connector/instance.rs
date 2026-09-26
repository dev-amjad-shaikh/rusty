//! The connector instance record: one manifest hash plus one validated
//! config — secrets extracted and sealed, never persisted.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::conn_err;
use crate::broker::SealedCredential;
use crate::error::Result;

/// The id prefix the server mints instance ids under (`inst-{16 hex}`).
pub const INSTANCE_ID_PREFIX: &str = "inst-";

/// Maximum instance id length.
pub const MAX_INSTANCE_ID_LEN: usize = 64;

/// Maximum sealed fields per instance.
pub const MAX_SEALED_FIELDS: usize = 32;

/// A connector instance: the replay input of a restart. `config` holds
/// only non-secret values (the secret walk removed every `rusty_secret`
/// field); `sealed` maps each extracted dot path to its broker envelope.
/// Tenancy rides the store key (`{tenant}/{instance_id}`), the knowledge
/// plane's precedent — the record itself carries bare ids.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectorInstance {
    /// The server-minted instance id (`inst-…`).
    pub instance_id: String,
    /// The manifest this instance configures (content hash).
    pub manifest_hash: String,
    /// The non-secret config, schema-validated before persistence.
    pub config: Value,
    /// `dot-path → sealed secret` for every `rusty_secret` field the
    /// config carried. Ciphertext only — opening needs the deployment
    /// master key, host-side, at call time.
    pub sealed: BTreeMap<String, SealedCredential>,
    /// When the instance was registered.
    pub created_at: DateTime<Utc>,
}

impl ConnectorInstance {
    /// Construct and validate an instance record. Validation is the
    /// record's own structural discipline (id shape, caps); schema
    /// validity of `config` is the registration path's job — it ran
    /// before the secrets came out.
    pub fn new(
        instance_id: impl Into<String>,
        manifest_hash: impl Into<String>,
        config: Value,
        sealed: BTreeMap<String, SealedCredential>,
        created_at: DateTime<Utc>,
    ) -> Result<Self> {
        let instance_id = instance_id.into();
        let manifest_hash = manifest_hash.into();
        if instance_id.is_empty()
            || instance_id.len() > MAX_INSTANCE_ID_LEN
            || !instance_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(conn_err(format!(
                "instance id `{instance_id}` must be `[A-Za-z0-9_-]`, at most \
                 {MAX_INSTANCE_ID_LEN} bytes"
            )));
        }
        if manifest_hash.len() != 64 || !manifest_hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(conn_err(format!(
                "manifest hash `{manifest_hash}` is not a SHA-256 hex digest"
            )));
        }
        if !config.is_object() {
            return Err(conn_err("instance config must be a JSON object"));
        }
        if sealed.len() > MAX_SEALED_FIELDS {
            return Err(conn_err(format!(
                "instance seals {} fields, above the {MAX_SEALED_FIELDS} cap",
                sealed.len()
            )));
        }
        Ok(Self {
            instance_id,
            manifest_hash,
            config,
            sealed,
            created_at,
        })
    }
}
