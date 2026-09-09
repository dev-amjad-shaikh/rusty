//! Device pairing for the gateway (EP-04-S03): every device that joins
//! proves key possession through a challenge-nonce flow, and remote devices
//! stay `pending` until an operator approves them — "what can talk to my
//! agents" is an explicit, revocable roster, not whatever holds a URL.
//!
//! Discipline, per the story's acceptance criteria:
//!
//! - **AC1 — single-use, expiring challenges.** [`PairingPlane::hello`]
//!   issues a 256-bit nonce held in memory with a bounded TTL. Answering
//!   consumes it (replay finds no outstanding challenge and is denied
//!   `challenge_replayed`); answering after the TTL is denied
//!   `challenge_expired`. Challenges are deliberately not persisted: a
//!   restarted gateway simply re-issues, and no nonce outlives the process
//!   that minted it.
//! - **AC2 — metadata pinning.** The signed message is
//!   `rusty:pairing:v1\n{device_id}\n{platform}\n{nonce}` — a signature
//!   over the nonce alone does not verify, so the platform and device
//!   family are part of the proof, not claims alongside it. The platform
//!   verified against is the one pinned at `hello`, not one re-asserted at
//!   answer time.
//! - **AC3 — loopback may auto-approve, remote never may.** A verified
//!   answer from a loopback peer transitions `pending → paired` only when
//!   the deployment opted in
//!   (`ServerConfig::pairing_auto_approve_loopback`); a verified answer
//!   from any other peer leaves the row `pending` regardless of policy,
//!   until an operator-scoped principal approves it. Approval mints the
//!   token and delivers it once, to the approver's surface.
//! - **AC4 — the token is a bearer secret.** The roster stores only the
//!   token's SHA-256 hash; the token itself is delivered exactly once in
//!   the `Pairing::Paired` payload and never appears in snapshots, events,
//!   or the store. The `devices` row's only mutable state is
//!   `pending → paired → revoked`.
//! - **AC5 — revocation is immediate and audited.** Revoking a device
//!   cancels every connection attached with its token (the WS read loop
//!   selects on the connection's [`CancellationToken`]), rejects the token
//!   on reconnect with the typed reason `device_revoked`, and appends an
//!   actor-attributed entry to the audit namespace (NFR-15).
//!
//! Persistence: the roster lives in the server store's KV plane —
//! `gateway_devices` rows keyed `{tenant}.{device_id}` (device ids are
//! dot-free, so the last `.` segment always names the device) — and the
//! audit trail in `gateway_device_audit`, one append per transition.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rusty_agent_runtime::scope::{Scope, scope_authorizes};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::auth::TenantContext;
use crate::server_store::{ServerStore, StoreResult};

/// The KV namespace holding the device roster, one row per device.
pub(crate) const DEVICES_NAMESPACE: &str = "gateway_devices";
/// The KV namespace holding the append-only pairing audit trail (NFR-15).
pub(crate) const AUDIT_NAMESPACE: &str = "gateway_device_audit";
/// Default challenge lifetime; `ServerConfig::pairing_challenge_ttl`
/// overrides.
pub(crate) const DEFAULT_CHALLENGE_TTL: Duration = Duration::from_secs(60);

/// Denied: the nonce's bounded window lapsed before the answer arrived.
pub(crate) const DENIED_CHALLENGE_EXPIRED: &str = "challenge_expired";
/// Denied: the answer named a nonce already consumed or never issued.
pub(crate) const DENIED_CHALLENGE_REPLAYED: &str = "challenge_replayed";
/// Denied: the signature did not verify under the presented key over the
/// pinned message (nonce + platform + device family).
pub(crate) const DENIED_INVALID_SIGNATURE: &str = "invalid_signature";
/// Denied: the operator revoked this device; its token is dead.
pub(crate) const DENIED_DEVICE_REVOKED: &str = "device_revoked";
/// Denied: no pairing flow exists for this device id.
pub(crate) const DENIED_UNKNOWN_DEVICE: &str = "unknown_device";
/// Denied: the presented key material is not an Ed25519 verifying key.
pub(crate) const DENIED_INVALID_PUBLIC_KEY: &str = "invalid_public_key";
/// Denied: the device is not yet `paired`; there is no token to present.
pub(crate) const DENIED_DEVICE_NOT_PAIRED: &str = "device_not_paired";
/// Denied: the presented token does not match the device's issued token.
pub(crate) const DENIED_INVALID_DEVICE_TOKEN: &str = "invalid_device_token";

/// The scope an operator principal must hold to approve or revoke a
/// device. Checked at the WS dispatch layer; the plane itself is scope
/// blind.
pub(crate) const OPERATOR_SCOPE: &str = "gateway:operator";

/// `true` when the principal's granted scopes authorize operator actions
/// (approve / revoke) on the pairing surface.
pub(crate) fn is_operator(tenant: &TenantContext) -> bool {
    scope_authorizes(
        tenant.scopes(),
        &Scope::parse(OPERATOR_SCOPE).expect("the operator scope literal is valid"),
    )
}

/// The canonical message a pairing answer signs: the nonce bound to the
/// platform and the device family (AC2). The device signs these exact
/// bytes with the Ed25519 key it presented at `hello`.
pub(crate) fn pairing_message(device_id: &str, platform: &str, nonce: &str) -> Vec<u8> {
    format!("rusty:pairing:v1\n{device_id}\n{platform}\n{nonce}").into_bytes()
}

/// The only mutable pairing state a `devices` row carries (AC4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeviceState {
    /// Introduced itself; awaiting verification or operator approval.
    Pending,
    /// Verified and approved; holds an issued token (stored as a hash).
    Paired,
    /// Revoked by an operator; terminal — the device must re-pair as new
    /// only after the row is deleted out of band.
    Revoked,
}

/// One row of the device roster (`devices` per `contracts:gateway-protocol`
/// storage). The token never appears here — only its SHA-256 hash, and
/// only while the device is paired.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DeviceRecord {
    pub device_id: String,
    pub tenant: String,
    pub public_key: String,
    pub platform: String,
    pub state: DeviceState,
    /// Set when a challenge answer has verified but the device awaits
    /// operator approval; approval without a verified answer is refused.
    pub answer_verified: bool,
    /// SHA-256 of the issued bearer token, hex. `None` unless paired.
    pub token_hash: Option<String>,
    pub created_at: DateTime<Utc>,
    pub paired_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// An outstanding challenge. In-memory by design (see the module docs):
/// single-use by removal, unpredictable by 256 bits of UUIDv4 entropy,
/// expiring by `Instant` deadline.
struct PendingChallenge {
    nonce: String,
    expires_at: Instant,
    public_key: String,
    platform: String,
}

/// The verdict of [`PairingPlane::answer`], rendered onto the wire by the
/// WS dispatch layer in `Pairing` vocabulary.
pub(crate) enum AnswerOutcome {
    /// `pending → paired`; the issued token, delivered here and only here.
    Paired(String),
    /// Verified, but the peer is remote (or policy forbids auto-approval):
    /// the row stays `pending` until an operator approves it.
    PendingApproval,
    /// Refused with the reason `Pairing::Denied` carries.
    Denied(String),
}

/// The pairing plane: challenges, the roster, the audit trail, and the
/// live-connection registry revocation closes over. One per process,
/// constructed at router build.
pub(crate) struct PairingPlane {
    store: Arc<dyn ServerStore>,
    auto_approve_loopback: bool,
    challenge_ttl: Duration,
    /// `(tenant, device_id)` → the outstanding challenge, keyed by
    /// NUL-joined pair (in-memory only; never a store key).
    challenges: Mutex<HashMap<String, PendingChallenge>>,
    /// `{tenant}.{device_id}` → the close tokens of every connection
    /// currently attached with that device's token. Revocation cancels
    /// them all (AC5).
    connections: Mutex<HashMap<String, Vec<CancellationToken>>>,
}

impl PairingPlane {
    pub(crate) fn new(
        store: Arc<dyn ServerStore>,
        auto_approve_loopback: bool,
        challenge_ttl: Duration,
    ) -> Self {
        Self {
            store,
            auto_approve_loopback,
            challenge_ttl,
            challenges: Mutex::new(HashMap::new()),
            connections: Mutex::new(HashMap::new()),
        }
    }

    /// `Pairing::Hello` → `Pairing::Challenge` (AC1). Registers the device
    /// as `pending` on first introduction; a revoked device is refused.
    pub(crate) async fn hello(
        &self,
        tenant: &TenantContext,
        device_id: &str,
        public_key: &str,
        platform: &str,
    ) -> Result<String, String> {
        if !valid_device_id(device_id) {
            return Err("invalid device_id (allowed: [A-Za-z0-9_-], 1..=128 chars)".to_string());
        }
        if verifying_key(public_key).is_none() {
            return Err(DENIED_INVALID_PUBLIC_KEY.to_string());
        }
        if let Some(record) = self.load(tenant, device_id).await? {
            if record.state == DeviceState::Revoked {
                return Err(DENIED_DEVICE_REVOKED.to_string());
            }
        } else {
            let record = DeviceRecord {
                device_id: device_id.to_string(),
                tenant: tenant.tenant().to_string(),
                public_key: public_key.to_string(),
                platform: platform.to_string(),
                state: DeviceState::Pending,
                answer_verified: false,
                token_hash: None,
                created_at: Utc::now(),
                paired_at: None,
                revoked_at: None,
            };
            self.save(&record).await?;
        }
        let nonce = random_hex32();
        self.challenges.lock().expect("challenges poisoned").insert(
            challenge_key(tenant.tenant(), device_id),
            PendingChallenge {
                nonce: nonce.clone(),
                expires_at: Instant::now() + self.challenge_ttl,
                public_key: public_key.to_string(),
                platform: platform.to_string(),
            },
        );
        Ok(nonce)
    }

    /// `Pairing::Answer` → paired / pending / denied (AC1–AC3). The
    /// challenge is consumed on any answer — success or failure — so a
    /// replayed nonce meets `challenge_replayed`.
    pub(crate) async fn answer(
        &self,
        tenant: &TenantContext,
        device_id: &str,
        signature: &str,
        is_loopback: bool,
    ) -> Result<AnswerOutcome, String> {
        let Some(record) = self.load(tenant, device_id).await? else {
            return Ok(AnswerOutcome::Denied(DENIED_UNKNOWN_DEVICE.to_string()));
        };
        if record.state == DeviceState::Revoked {
            return Ok(AnswerOutcome::Denied(DENIED_DEVICE_REVOKED.to_string()));
        }
        // Removal is the single-use discipline: whatever the verdict, this
        // nonce never verifies again.
        let challenge = self
            .challenges
            .lock()
            .expect("challenges poisoned")
            .remove(&challenge_key(tenant.tenant(), device_id));
        let Some(challenge) = challenge else {
            return Ok(AnswerOutcome::Denied(DENIED_CHALLENGE_REPLAYED.to_string()));
        };
        if challenge.expires_at < Instant::now() {
            return Ok(AnswerOutcome::Denied(DENIED_CHALLENGE_EXPIRED.to_string()));
        }
        // The signed message pins the platform presented at `hello` (AC2) —
        // an answer cannot renegotiate the metadata it proves.
        let message = pairing_message(device_id, &challenge.platform, &challenge.nonce);
        if !verify(&challenge.public_key, &message, signature) {
            return Ok(AnswerOutcome::Denied(DENIED_INVALID_SIGNATURE.to_string()));
        }

        if is_loopback && self.auto_approve_loopback {
            let token = self.pair(record, "policy:loopback-auto-approve").await?;
            return Ok(AnswerOutcome::Paired(token));
        }
        // Remote devices never auto-approve (AC3): record the verified
        // answer and wait for the operator.
        let mut record = record;
        record.answer_verified = true;
        self.save(&record).await?;
        Ok(AnswerOutcome::PendingApproval)
    }

    /// Operator approval: a verified `pending` device transitions to
    /// `paired` and its token is minted and returned — to the approver's
    /// surface, exactly once (AC3). Refused without a verified answer or
    /// for a row not in `pending`.
    pub(crate) async fn approve(
        &self,
        tenant: &TenantContext,
        device_id: &str,
        actor: &str,
    ) -> Result<String, String> {
        let Some(record) = self.load(tenant, device_id).await? else {
            return Err(DENIED_UNKNOWN_DEVICE.to_string());
        };
        match record.state {
            DeviceState::Revoked => return Err(DENIED_DEVICE_REVOKED.to_string()),
            DeviceState::Paired => return Err("device already paired".to_string()),
            DeviceState::Pending => {}
        }
        if !record.answer_verified {
            return Err("no verified challenge answer on record".to_string());
        }
        self.pair(record, actor).await
    }

    /// Operator revocation (AC5): the row transitions to `revoked`, the
    /// token hash is dropped, every attached connection is cancelled, and
    /// the audit trail records the actor.
    pub(crate) async fn revoke(
        &self,
        tenant: &TenantContext,
        device_id: &str,
        actor: &str,
    ) -> Result<(), String> {
        let Some(mut record) = self.load(tenant, device_id).await? else {
            return Err(DENIED_UNKNOWN_DEVICE.to_string());
        };
        if record.state == DeviceState::Revoked {
            return Err(DENIED_DEVICE_REVOKED.to_string());
        }
        record.state = DeviceState::Revoked;
        record.token_hash = None;
        record.revoked_at = Some(Utc::now());
        self.save(&record).await?;
        self.audit(&record, "revoked", actor).await?;
        let key = record_key(tenant.tenant(), device_id);
        let tokens = self
            .connections
            .lock()
            .expect("connections poisoned")
            .remove(&key)
            .unwrap_or_default();
        for token in tokens {
            token.cancel();
        }
        Ok(())
    }

    /// Token authentication for subsequent connections (AC4): a paired
    /// device's presented token must hash-match the roster row. On success
    /// the connection's close token joins the registry revocation cancels
    /// (AC5).
    pub(crate) async fn attach(
        &self,
        tenant: &TenantContext,
        device_id: &str,
        device_token: &str,
        conn_close: CancellationToken,
    ) -> Result<(), String> {
        let Some(record) = self.load(tenant, device_id).await? else {
            return Err(DENIED_UNKNOWN_DEVICE.to_string());
        };
        match record.state {
            DeviceState::Revoked => return Err(DENIED_DEVICE_REVOKED.to_string()),
            DeviceState::Pending => return Err(DENIED_DEVICE_NOT_PAIRED.to_string()),
            DeviceState::Paired => {}
        }
        let Some(expected) = &record.token_hash else {
            return Err(DENIED_DEVICE_NOT_PAIRED.to_string());
        };
        if !constant_time_eq(expected.as_bytes(), token_hash(device_token).as_bytes()) {
            return Err(DENIED_INVALID_DEVICE_TOKEN.to_string());
        }
        self.connections
            .lock()
            .expect("connections poisoned")
            .entry(record_key(tenant.tenant(), device_id))
            .or_default()
            .push(conn_close);
        Ok(())
    }

    /// Read the roster row (the unit tests inspect state directly; the
    /// wire surface exposes state through method outcomes).
    #[cfg(test)]
    pub(crate) async fn record(
        &self,
        tenant: &TenantContext,
        device_id: &str,
    ) -> StoreResult<Option<DeviceRecord>> {
        self.load(tenant, device_id).await
    }

    /// The shared `pending → paired` transition: mint the token, persist
    /// only its hash, audit the transition with its actor.
    async fn pair(&self, mut record: DeviceRecord, actor: &str) -> Result<String, String> {
        let token = random_hex32();
        record.state = DeviceState::Paired;
        record.answer_verified = true;
        record.token_hash = Some(token_hash(&token));
        record.paired_at = Some(Utc::now());
        self.save(&record).await?;
        self.audit(&record, "paired", actor).await?;
        Ok(token)
    }

    async fn load(
        &self,
        tenant: &TenantContext,
        device_id: &str,
    ) -> StoreResult<Option<DeviceRecord>> {
        let item = self
            .store
            .kv_get(DEVICES_NAMESPACE, &record_key(tenant.tenant(), device_id))
            .await?;
        match item {
            Some(item) => serde_json::from_value(item.value)
                .map(Some)
                .map_err(|error| format!("gateway device row failed to decode: {error}")),
            None => Ok(None),
        }
    }

    async fn save(&self, record: &DeviceRecord) -> StoreResult<()> {
        let value = serde_json::to_value(record)
            .map_err(|error| format!("gateway device row failed to encode: {error}"))?;
        self.store
            .kv_put(
                DEVICES_NAMESPACE,
                &record_key(&record.tenant, &record.device_id),
                value,
            )
            .await?;
        Ok(())
    }

    /// Append one actor-attributed entry to the audit trail (NFR-15). The
    /// key is time-ordered; the entry never carries token material.
    async fn audit(&self, record: &DeviceRecord, action: &str, actor: &str) -> StoreResult<()> {
        let now = Utc::now();
        let key = format!(
            "{}.{}-{}",
            record_key(&record.tenant, &record.device_id),
            now.timestamp_millis(),
            Uuid::new_v4().simple()
        );
        self.store
            .kv_put(
                AUDIT_NAMESPACE,
                &key,
                json!({
                    "device_id": record.device_id,
                    "tenant": record.tenant,
                    "action": action,
                    "actor": actor,
                    "at": now,
                }),
            )
            .await?;
        Ok(())
    }
}

/// The roster key: `{tenant}.{device_id}` — always prefixed, so tenants
/// partition the namespace and dot-free device ids keep the key
/// unambiguous.
fn record_key(tenant: &str, device_id: &str) -> String {
    format!("{tenant}.{device_id}")
}

fn challenge_key(tenant: &str, device_id: &str) -> String {
    format!("{tenant}\0{device_id}")
}

/// Device ids are `[A-Za-z0-9_-]` (1–128): dot-free so the roster key's
/// last segment always names the device.
fn valid_device_id(device_id: &str) -> bool {
    !device_id.is_empty()
        && device_id.len() <= 128
        && device_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

/// Parse a hex-encoded Ed25519 verifying key.
fn verifying_key(hex: &str) -> Option<ed25519_dalek::VerifyingKey> {
    let bytes = hex_decode(hex)?;
    let array: [u8; 32] = bytes.try_into().ok()?;
    ed25519_dalek::VerifyingKey::from_bytes(&array).ok()
}

/// Verify `signature_hex` over `message` under the hex-encoded key.
fn verify(public_key_hex: &str, message: &[u8], signature_hex: &str) -> bool {
    use ed25519_dalek::Verifier;
    let Some(key) = verifying_key(public_key_hex) else {
        return false;
    };
    let Some(bytes) = hex_decode(signature_hex) else {
        return false;
    };
    let Ok(array) = <[u8; 64]>::try_from(bytes.as_slice()) else {
        return false;
    };
    key.verify(message, &ed25519_dalek::Signature::from_bytes(&array))
        .is_ok()
}

/// The SHA-256 of a bearer token, hex — the only token material the
/// roster ever holds.
fn token_hash(token: &str) -> String {
    hex_encode(&Sha256::digest(token.as_bytes()))
}

/// 256 bits of UUIDv4 entropy, hex-encoded: nonces and device tokens.
fn random_hex32() -> String {
    let mut bytes = Uuid::new_v4().as_bytes().to_vec();
    bytes.extend_from_slice(Uuid::new_v4().as_bytes());
    hex_encode(&bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Length-checked byte equality. The token hash is not a password-equivalent
/// secret (knowing the hash admits nothing), but there is no reason to
/// leak prefix length through early exit either.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Render a pairing method's refusal as the contract's `Pairing::Denied`
/// payload — the WS layer serializes this verbatim into the error
/// envelope, so the wire vocabulary stays the schema's.
pub(crate) fn denied_payload(reason: &str) -> Value {
    serde_json::to_value(rusty_api::gateway_protocol::Pairing::Denied {
        reason: reason.to_string(),
    })
    .expect("Pairing::Denied serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::DEFAULT_TENANT;
    use crate::server_store::JsonFileStore;
    use ed25519_dalek::{Signer, SigningKey};

    fn temp_root(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("rusty-pairing-test-{tag}-{}", Uuid::new_v4()))
    }

    fn tenant() -> TenantContext {
        TenantContext::new(
            DEFAULT_TENANT.to_string(),
            vec![Scope::parse("*:*:*").expect("valid")],
        )
    }

    fn plane(root: &std::path::Path, auto_approve: bool, ttl: Duration) -> PairingPlane {
        PairingPlane::new(Arc::new(JsonFileStore::load(root)), auto_approve, ttl)
    }

    fn device_key(seed: u8) -> (SigningKey, String) {
        let signing = SigningKey::from_bytes(&[seed; 32]);
        let public = hex_encode(signing.verifying_key().as_bytes());
        (signing, public)
    }

    fn sign(signing: &SigningKey, device_id: &str, platform: &str, nonce: &str) -> String {
        hex_encode(
            &signing
                .sign(&pairing_message(device_id, platform, nonce))
                .to_bytes(),
        )
    }

    #[tokio::test]
    async fn loopback_auto_approve_pairs_and_hashes_the_token() {
        let root = temp_root("loopback");
        let plane = plane(&root, true, Duration::from_secs(60));
        let tenant = tenant();
        let (signing, public) = device_key(7);

        let nonce = plane
            .hello(&tenant, "dev-1", &public, "macos")
            .await
            .expect("hello issues a challenge");
        let outcome = plane
            .answer(
                &tenant,
                "dev-1",
                &sign(&signing, "dev-1", "macos", &nonce),
                true,
            )
            .await
            .expect("answer succeeds");
        let AnswerOutcome::Paired(token) = outcome else {
            panic!("loopback with auto-approve pairs");
        };
        let record = plane
            .record(&tenant, "dev-1")
            .await
            .unwrap()
            .expect("the row exists");
        assert_eq!(record.state, DeviceState::Paired);
        assert_eq!(
            record.token_hash.as_deref(),
            Some(token_hash(&token).as_str())
        );
        // The token itself never reaches the store.
        let raw = std::fs::read_to_string(root.join("store/gateway_devices/default.dev-1.json"))
            .expect("the row file exists");
        assert!(!raw.contains(&token), "the token never persists");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn remote_never_auto_approves_even_when_policy_permits() {
        let root = temp_root("remote");
        let plane = plane(&root, true, Duration::from_secs(60));
        let tenant = tenant();
        let (signing, public) = device_key(8);

        let nonce = plane
            .hello(&tenant, "dev-2", &public, "linux")
            .await
            .unwrap();
        let outcome = plane
            .answer(
                &tenant,
                "dev-2",
                &sign(&signing, "dev-2", "linux", &nonce),
                false,
            )
            .await
            .unwrap();
        assert!(
            matches!(outcome, AnswerOutcome::PendingApproval),
            "remote stays pending under every policy"
        );
        let record = plane.record(&tenant, "dev-2").await.unwrap().unwrap();
        assert_eq!(record.state, DeviceState::Pending);
        assert!(record.answer_verified);

        // Approval without operator scope is refused at the dispatch layer;
        // here the plane transition itself is what is under test.
        let token = plane
            .approve(&tenant, "dev-2", "operator@default")
            .await
            .expect("a verified pending device approves");
        let record = plane.record(&tenant, "dev-2").await.unwrap().unwrap();
        assert_eq!(record.state, DeviceState::Paired);
        assert_eq!(
            record.token_hash.as_deref(),
            Some(token_hash(&token).as_str())
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn approval_requires_a_verified_answer() {
        let root = temp_root("unverified");
        let plane = plane(&root, false, Duration::from_secs(60));
        let tenant = tenant();
        let (_signing, public) = device_key(9);
        plane
            .hello(&tenant, "dev-3", &public, "macos")
            .await
            .unwrap();
        let error = plane
            .approve(&tenant, "dev-3", "operator@default")
            .await
            .expect_err("no verified answer yet");
        assert!(error.contains("no verified challenge answer"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn replayed_expired_and_unpinned_signatures_deny_distinctly() {
        let root = temp_root("denials");
        let plane = plane(&root, false, Duration::from_millis(40));
        let tenant = tenant();
        let (signing, public) = device_key(10);

        // Expired: answer after the TTL.
        let nonce = plane
            .hello(&tenant, "dev-4", &public, "macos")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let outcome = plane
            .answer(
                &tenant,
                "dev-4",
                &sign(&signing, "dev-4", "macos", &nonce),
                true,
            )
            .await
            .unwrap();
        assert!(
            matches!(&outcome, AnswerOutcome::Denied(r) if r == DENIED_CHALLENGE_EXPIRED),
            "expired nonce denies distinctly"
        );

        // Replayed: the consumed nonce (above) never verifies again.
        let outcome = plane
            .answer(
                &tenant,
                "dev-4",
                &sign(&signing, "dev-4", "macos", &nonce),
                true,
            )
            .await
            .unwrap();
        assert!(
            matches!(&outcome, AnswerOutcome::Denied(r) if r == DENIED_CHALLENGE_REPLAYED),
            "replayed nonce denies distinctly"
        );

        // Unpinned: a signature over the nonce alone omits the platform
        // binding and must not verify (AC2).
        let nonce = plane
            .hello(&tenant, "dev-4", &public, "macos")
            .await
            .unwrap();
        let nonce_only = hex_encode(&signing.sign(nonce.as_bytes()).to_bytes());
        let outcome = plane
            .answer(&tenant, "dev-4", &nonce_only, true)
            .await
            .unwrap();
        assert!(
            matches!(&outcome, AnswerOutcome::Denied(r) if r == DENIED_INVALID_SIGNATURE),
            "a nonce-only signature denies distinctly"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn revocation_rejects_the_token_and_audits_the_actor() {
        let root = temp_root("revoke");
        let plane = plane(&root, true, Duration::from_secs(60));
        let tenant = tenant();
        let (signing, public) = device_key(11);

        let nonce = plane
            .hello(&tenant, "dev-5", &public, "macos")
            .await
            .unwrap();
        let outcome = plane
            .answer(
                &tenant,
                "dev-5",
                &sign(&signing, "dev-5", "macos", &nonce),
                true,
            )
            .await
            .unwrap();
        let AnswerOutcome::Paired(token) = outcome else {
            panic!("paired");
        };

        let conn = CancellationToken::new();
        plane
            .attach(&tenant, "dev-5", &token, conn.clone())
            .await
            .expect("the issued token attaches");

        plane
            .revoke(&tenant, "dev-5", "operator@default")
            .await
            .expect("revocation commits");
        assert!(conn.is_cancelled(), "open connections close on revocation");

        let error = plane
            .attach(&tenant, "dev-5", &token, CancellationToken::new())
            .await
            .expect_err("a revoked device's token is rejected");
        assert_eq!(error, DENIED_DEVICE_REVOKED);

        // The audit trail records the transition with actor attribution.
        let entries = plane
            .store
            .kv_list(AUDIT_NAMESPACE)
            .await
            .expect("audit lists");
        let revoked = entries
            .iter()
            .filter(|e| e.value["action"] == json!("revoked"))
            .collect::<Vec<_>>();
        assert_eq!(revoked.len(), 1, "{entries:?}");
        assert_eq!(revoked[0].value["actor"], json!("operator@default"));
        let serialized =
            serde_json::to_string(&entries.iter().map(|e| &e.value).collect::<Vec<_>>()).unwrap();
        assert!(
            !serialized.contains(&token),
            "the audit never carries tokens"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn denied_payload_uses_the_contract_shape() {
        assert_eq!(
            denied_payload(DENIED_CHALLENGE_EXPIRED),
            json!({"step": "denied", "reason": "challenge_expired"})
        );
    }

    #[test]
    fn roster_keys_partition_tenants() {
        assert_eq!(record_key("default", "dev-1"), "default.dev-1");
        assert_eq!(record_key("acme", "dev-1"), "acme.dev-1");
    }
}
