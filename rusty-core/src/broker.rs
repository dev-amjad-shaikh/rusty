//! The credential/connection broker contracts (R0.11 Extension Plane,
//! wave 3): the connection entity, the sealed-storage shape, the handle
//! lifecycle, and the seams that keep credential bytes out of tool code.
//!
//! The design doc is `docs/extension-plane-design.md` ("The
//! credential/connection broker"). Its governing claim: **no tool ever
//! holds a raw credential — credentials live in one broker, and tools
//! receive short-lived, opaque capability handles, non-serializable and
//! scope-checked at use.** This module is the pure-contract half both
//! sides agree on (the `capsule.rs` / `registry.rs` posture); the store
//! backends, the envelope cryptography, the master key, and the HTTP
//! surface live in `rusty-agent-server`.
//!
//! - [`ConnectionRecord`] — the one entity: stable id, provider kind, the
//!   per-user subject (absent for service-level connections), the consent
//!   scope set (the ceiling everything downstream may only narrow — the
//!   `CapsuleOverlay` stance applied to credentials), status, and health.
//! - [`TokenMaterial`] — the plaintext secret. It exists in this contract
//!   for exactly one reason: to be *sealed*. The store persists
//!   [`StoredConnection`] — the record plus a [`SealedCredential`] —
//!   and plaintext enters the store abstraction on neither backend, ever
//!   (the design's named deviation: ciphertext may enter the abstraction
//!   because connections are numerous, refreshed, and queried by id; the
//!   R0.9 principle survives — a store leak must not leak credentials,
//!   and ciphertext without the host-local master key is not a
//!   credential).
//! - [`CredentialHandle`] — what a tool receives: an opaque token bound
//!   to the connection, a *narrowed* scope set, the tenant, the run, and
//!   a short TTL. Redacted in `Debug`, no `Serialize` impl, carrying no
//!   bytes — the R0.9 capsule secret handle generalized from guest
//!   linear memory to all tool code. Validity (expiry, scope binding) is
//!   self-contained in the signed claims; only the connection liveness
//!   check hits the store at resolution, which is what makes revocation
//!   effective at the *next* tool call rather than the next deploy.
//! - [`BrokerDenial`] — the typed refusal: revoked connection (naming the
//!   revoked grant), expired handle, scope beyond the narrowed set,
//!   `needs_reauth`, unknown handle or connection, and
//!   broker-unavailable. Every failure mode fails closed, and every
//!   denial is attributable to a declaration — never a stack trace, and
//!   never the bytes.
//! - [`CredentialBroker`] — the seam the server implements (the
//!   `NetworkConnector` boundary, drawn for the same reason: core owns
//!   the journaled *shape*; the deployment owns key custody and
//!   persistence). [`CredentialMediator`] + [`MediatedTool`] are the
//!   `ToolExecutor` integration: a credential-requiring tool wrapped at
//!   registration is issued a handle at first use and re-issued as the
//!   TTL turns over; the tool presents the handle to the host-side
//!   connector, and resolution returns the credential bytes to the
//!   *connector* — they never enter tool code. Behind `wasm`,
//!   [`BrokeredCapsuleHost`] is the capsule half: a manifest's `Secret`
//!   grants are brokered into issued handle tokens the guest receives in
//!   its input, the R0.9 "the guest receives opaque tokens" precedent
//!   extended from names to broker-issued handles.
//!
//! Golden-file tests under `tests/golden/` pin every wire shape in this
//! module; any accidental contract drift fails CI.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::durable::ErrorClass;
use crate::error::{Result, RustyError};
use crate::record::Effect;
use crate::tool::Tool;

fn invalid(message: impl Into<String>) -> RustyError {
    // Contract validation at a state boundary; the invalid-update class
    // covers it rather than growing the error taxonomy for one module
    // (the capsule/memory convention).
    RustyError::InvalidUpdate(message.into())
}

/// Lowercase hex encoding (the `record::sha256_hex` output discipline),
/// kept dependency-free so the token codec and the sealed envelope share
/// one byte-to-text rule without pulling a hex dependency into core.
/// Public because the sealed envelope's hex fields are the storage
/// contract — the server's key-custody layer writes and reads them.
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The inverse of [`hex_encode`]; `None` on odd length or non-hex input.
pub fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

// --------------------------------------------------------------------- //
// The connection model
// --------------------------------------------------------------------- //

/// The provider kind of a [`ConnectionRecord`]. Closed enum, additive
/// evolution — a fifth kind lands as a new variant, never a rewrite of
/// pinned shapes (the `CandidateKind` rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionProvider {
    /// An OAuth 2.0 authorization-code grant: a human consented at the
    /// provider, and the broker recorded what was granted.
    Oauth2AuthorizationCode,
    /// An OAuth 2.0 client-credentials grant (service-to-service).
    Oauth2ClientCredentials,
    /// A static API key presented as a bearer or header credential.
    ApiKey,
    /// HTTP basic authentication (username:password material).
    Basic,
}

/// The lifecycle status of a [`ConnectionRecord`]. Closed enum, additive
/// evolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    /// Resolutions may proceed.
    Active,
    /// The refresh path failed terminally (an expired refresh token, a
    /// provider `invalid_grant`): calls fail closed with a typed re-auth
    /// signal until a new consent act is recorded — never silent retries
    /// with stale material, because a stale credential retried looks
    /// exactly like an attack retried.
    NeedsReauth,
    /// Revoked. Outstanding handles fail at their next use with a typed,
    /// journaled denial naming the revoked grant.
    Revoked,
}

/// One classified failure on the connection's health record: the last
/// refresh or resolution-time provider failure, classified under the
/// R0.6 [`ErrorClass`] taxonomy so the retry policy plane reads it
/// unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClassifiedFailure {
    /// The retry-taxonomy class of the failure.
    pub class: ErrorClass,

    /// Human-facing context (the provider's error, never credential
    /// material).
    pub detail: String,

    /// When the failure was observed.
    pub at: DateTime<Utc>,
}

/// The health half of a [`ConnectionRecord`]: refresh bookkeeping and
/// failure classification, metadata by construction — nothing here can
/// carry credential bytes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConnectionHealth {
    /// When the token material was last refreshed (a recorded consent act
    /// counts — it replaces the material).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh_at: Option<DateTime<Utc>>,

    /// The most recent classified failure, when one was observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<ClassifiedFailure>,

    /// Consecutive failures since the last success. Reset by any
    /// successful refresh or consent.
    #[serde(default)]
    pub consecutive_failures: u32,
}

/// The connection: a named, tenant-scoped record binding a provider
/// account (and optionally a user subject) to its consent scope set and
/// lifecycle state. The credential bytes are *not* here — they live in
/// the [`SealedCredential`] half of [`StoredConnection`], so the record
/// is safe to journal, list, and serve whole.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionRecord {
    /// The stable id, minted at registration (`conn-{32 hex}`). Stable
    /// across rotation beneath it: credential rotation changes nothing a
    /// run pinned, because the pin names this governed relationship, not
    /// the secret of the moment.
    pub connection_id: String,

    /// The provider kind.
    pub provider: ConnectionProvider,

    /// The per-user binding — a user id within the tenant. Absent for
    /// service-level (shared) connections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,

    /// The consent scope set, in provider semantics (`repo`,
    /// `https://…/auth/drive.readonly`): recorded when the human (or the
    /// service enrollment) consents. This is the ceiling — handle
    /// issuance may only narrow against it, and there is no code path
    /// that widens a handle past it.
    pub scopes: BTreeSet<String>,

    /// The lifecycle status.
    pub status: ConnectionStatus,

    /// Refresh and failure bookkeeping.
    #[serde(default)]
    pub health: ConnectionHealth,

    /// When the connection was registered.
    pub created_at: DateTime<Utc>,

    /// When the record last changed (consent, refresh, revocation).
    pub updated_at: DateTime<Utc>,
}

/// The connection id prefix; the id is the prefix plus 32 lowercase hex
/// chars (one `uuid` v4 draw of OS entropy, the receipt-key generation
/// precedent).
pub const CONNECTION_ID_PREFIX: &str = "conn-";

/// The handle id prefix, same shape.
pub const HANDLE_ID_PREFIX: &str = "hdl-";

/// Mint a connection id. Ids are minted, never caller-chosen: the caller
/// naming a connection would make id collisions a confused-deputy vector
/// across tenants.
pub fn new_connection_id() -> String {
    format!("{}{}", CONNECTION_ID_PREFIX, uuid::Uuid::new_v4().simple())
}

/// Mint a handle id.
pub fn new_handle_id() -> String {
    format!("{}{}", HANDLE_ID_PREFIX, uuid::Uuid::new_v4().simple())
}

/// `true` when `id` is `{prefix}` plus 32 lowercase hex chars.
fn minted_id_ok(prefix: &str, id: &str) -> bool {
    let Some(hex) = id.strip_prefix(prefix) else {
        return false;
    };
    hex.len() == 32
        && hex
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// Free-text field validation shared by subjects and scopes: non-empty,
/// bounded, no control characters, and no `/` (the tenant separator —
/// the registry's artifact-name rule, applied here for the same reason).
fn validate_free_text(what: &str, value: &str, max: usize) -> Result<()> {
    let ok = !value.is_empty()
        && value.len() <= max
        && !value.contains('/')
        && value.chars().all(|c| !c.is_control());
    if !ok {
        return Err(invalid(format!(
            "invalid {what} `{value}` (non-empty, at most {max} chars, no `/` or control characters)"
        )));
    }
    Ok(())
}

impl ConnectionRecord {
    /// Contract validation, run at every write boundary (registration,
    /// consent, load). What passes here is a record the broker can serve
    /// without further interpretation.
    pub fn validate(&self) -> Result<()> {
        if !minted_id_ok(CONNECTION_ID_PREFIX, &self.connection_id) {
            return Err(invalid(format!(
                "connection id `{}` is not a minted id (`{CONNECTION_ID_PREFIX}` + 32 lowercase hex)",
                self.connection_id
            )));
        }
        validate_connection_fields(self.subject.as_deref(), &self.scopes)
    }

    /// `true` when resolutions may proceed against this record.
    pub fn is_active(&self) -> bool {
        self.status == ConnectionStatus::Active
    }
}

/// The scopes of `requested` that `ceiling` does not cover, sorted. The
/// narrowing check reduced to one pure function — a reviewer verifying
/// "issuance can only narrow against consent" reads this and the two
/// call sites (issuance, resolution) and nothing else. An empty
/// `requested` asks for nothing and is always covered.
pub fn scopes_missing(ceiling: &BTreeSet<String>, requested: &BTreeSet<String>) -> Vec<String> {
    requested.difference(ceiling).cloned().collect()
}

/// The caller-supplied-field half of [`ConnectionRecord::validate`]:
/// subject and scope free-text, without a record. HTTP boundaries run
/// this to answer `422` for bad input before the broker mints anything;
/// the record-level validation at the write boundary stays the backstop.
pub fn validate_connection_fields(subject: Option<&str>, scopes: &BTreeSet<String>) -> Result<()> {
    if let Some(subject) = subject {
        validate_free_text("connection subject", subject, 256)?;
    }
    for scope in scopes {
        validate_free_text("connection scope", scope, 512)?;
    }
    Ok(())
}

// --------------------------------------------------------------------- //
// Token material and the sealed envelope
// --------------------------------------------------------------------- //

/// The plaintext credential. This type exists to be **sealed**: it is
/// the input to the server's envelope encryption and the output of
/// handle resolution, and it must never appear in a store row, a
/// journal, a manifest, or a log line. `Debug` is redacted by hand
/// (the receipt signing key's posture); there is intentionally no
/// `Display`.
///
/// `Serialize`/`Deserialize` exist because sealing serializes — the
/// encrypted form is what the store holds. Any code serializing a
/// `TokenMaterial` for another purpose is the bug this type's docs warn
/// against.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenMaterial {
    /// The access token presented to the provider.
    pub access_token: String,

    /// The refresh token, where the provider issues one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,

    /// The access token's expiry, when the provider declares one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

impl std::fmt::Debug for TokenMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenMaterial")
            .field("access_token", &"[redacted]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[redacted]"),
            )
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// The sealed-envelope format version. Version 1 is XChaCha20-Poly1305
/// with the connection id as associated data; a future algorithm or
/// key-source change lands as a new version, and old envelopes keep
/// opening (the additive-evolution rule applied to cryptography).
pub const SEALED_FORMAT_VERSION: u32 = 1;

/// The sealed form of a connection's token material: everything the
/// store persists about the secret. Envelope encryption — the material
/// is encrypted under a per-connection data key (random, minted at
/// registration), and the data key is wrapped by the deployment master
/// key — so the store holds ciphertext and the wrapped key only, on
/// both backends. All binary fields are lowercase hex.
///
/// The `key_id` names the master key that wrapped the data key, so
/// master-key rotation (the design's open question 3: lazy re-wrap,
/// KMS/HSM as the R1.0 plug point) lands additively — the cryptography
/// is key-source-agnostic from the start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SealedCredential {
    /// The envelope format version ([`SEALED_FORMAT_VERSION`]).
    pub format_version: u32,

    /// The id of the master key that wrapped the data key.
    pub key_id: String,

    /// The data key, wrapped by the master key (hex).
    pub wrapped_data_key: String,

    /// The nonce the data key was wrapped under (hex).
    pub wrap_nonce: String,

    /// The nonce the token material was sealed under (hex). Fresh per
    /// seal: a consent act re-sealing the same connection reuses the
    /// data key but never a nonce.
    pub nonce: String,

    /// The sealed token material (hex).
    pub ciphertext: String,

    /// When this envelope was sealed.
    pub sealed_at: DateTime<Utc>,
}

/// What both store backends hold for one connection: the servable
/// record plus the sealed credential. Plaintext on neither backend,
/// ever — a Postgres dump of this row contains no credential.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredConnection {
    /// The connection record (metadata; safe to serve and journal).
    pub record: ConnectionRecord,

    /// The sealed token material.
    pub credential: SealedCredential,
}

// --------------------------------------------------------------------- //
// Handles
// --------------------------------------------------------------------- //

/// The signed claims a [`CredentialHandle`] carries. Validity is
/// self-contained: expiry and the scope binding read from here, and only
/// the connection liveness check hits the store at resolution (the
/// design's open question 5 leaning — "fails closed at the next tool
/// call" forbids caching revocation decisions). Journaled on issuance;
/// safe to journal throughout — claims name the connection and the
/// narrowed grant, never the bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandleClaims {
    /// The handle id (`hdl-{32 hex}`).
    pub handle_id: String,

    /// The connection this handle resolves against.
    pub connection_id: String,

    /// The tenant the handle was issued to. Bound in the claims (and
    /// covered by the signature) so a handle presented outside its
    /// tenant is a forgery, not a cross-tenant read.
    pub tenant: String,

    /// The run the handle was issued for, when the issuance was
    /// run-bound. The run's evidence pins — through the issuance event —
    /// the connection id and the consent scope set it resolved; rotation
    /// beneath the stable connection id changes nothing pinned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,

    /// The narrowed scope set: always a subset of the connection's
    /// consent set at issuance (the whole set when the request asked for
    /// no narrowing). Resolution may check any subset of *this* set —
    /// the second, per-operation narrowing.
    pub scopes: BTreeSet<String>,

    /// When the handle was issued.
    pub issued_at: DateTime<Utc>,

    /// When the handle stops resolving. Handles live for minutes and are
    /// never pinned anywhere.
    pub expires_at: DateTime<Utc>,
}

impl HandleClaims {
    /// `true` at or past the expiry instant.
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        now >= self.expires_at
    }
}

/// The wire prefix of a handle token (`v1.{hex claims}.{hex signature}`).
pub const HANDLE_TOKEN_PREFIX: &str = "v1";

/// An opaque capability handle: what a tool receives and presents at
/// use. Carrying no bytes — only the signed claims and their signature —
/// redacted in `Debug`, and with no `Serialize`/`Deserialize` impl: the
/// wire form is [`CredentialHandle::token`], minted by a broker and
/// verified by one, never a serde shape tool code can construct (the
/// R0.9 non-serializability posture).
#[derive(Clone, PartialEq)]
pub struct CredentialHandle {
    claims: HandleClaims,
    signature: String,
}

impl std::fmt::Debug for CredentialHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialHandle")
            .field("claims", &self.claims)
            .field("signature", &"[redacted]")
            .finish()
    }
}

impl CredentialHandle {
    /// Assemble a handle from its signed parts. Only a broker mints
    /// handles — the signature is an HMAC under key material the broker
    /// holds — so this constructor existing does not make handles
    /// forgeable; it makes them *passable* (across the tool boundary,
    /// into a capsule's input) without a serde impl.
    pub fn from_parts(claims: HandleClaims, signature: String) -> Self {
        Self { claims, signature }
    }

    /// The signed claims.
    pub fn claims(&self) -> &HandleClaims {
        &self.claims
    }

    /// The signature (hex), for verification at resolution.
    pub fn signature(&self) -> &str {
        &self.signature
    }

    /// The opaque wire token: `v1.{hex(json claims)}.{hex signature}`.
    /// Opaque to tools — they store and present it, and nothing in it
    /// helps them reach the credential: the bytes are not here, and a
    /// modified token fails the signature check at resolution.
    pub fn token(&self) -> String {
        let claims = serde_json::to_vec(&self.claims).unwrap_or_default();
        format!(
            "{}.{}.{}",
            HANDLE_TOKEN_PREFIX,
            hex_encode(&claims),
            self.signature
        )
    }

    /// Parse a wire token back into its unsigned parts. This is *not*
    /// verification: the broker recomputes the signature over the parsed
    /// claims before anything else happens. A malformed token is an
    /// [`BrokerDenialReason::UnknownHandle`] — parse failure and forgery
    /// are the same refusal.
    // result_large_err: shrinking the denial would buy nothing here —
    // the Ok half (claims plus signature) is the larger variant, so the
    // Result is already sized by the success payload.
    #[allow(clippy::result_large_err)]
    pub fn parse_token(token: &str) -> std::result::Result<(HandleClaims, String), BrokerDenial> {
        let unknown = |detail: &str| BrokerDenial::unknown_handle(detail.to_owned());
        let mut parts = token.splitn(3, '.');
        if parts.next() != Some(HANDLE_TOKEN_PREFIX) {
            return Err(unknown("not a broker handle token"));
        }
        let claims_hex = parts.next().unwrap_or_default();
        let signature = parts.next().unwrap_or_default().to_owned();
        let claims_bytes =
            hex_decode(claims_hex).ok_or_else(|| unknown("the claims half is not valid hex"))?;
        let claims: HandleClaims = serde_json::from_slice(&claims_bytes)
            .map_err(|_| unknown("the claims half is not a handle claims document"))?;
        if !minted_id_ok(HANDLE_ID_PREFIX, &claims.handle_id)
            || !minted_id_ok(CONNECTION_ID_PREFIX, &claims.connection_id)
            || claims.tenant.is_empty()
            || claims.expires_at <= claims.issued_at
        {
            return Err(unknown("the claims fail handle grammar"));
        }
        Ok((claims, signature))
    }
}

// --------------------------------------------------------------------- //
// Denials
// --------------------------------------------------------------------- //

/// Why a handle issuance or resolution was refused. Closed enum,
/// additive evolution; serialized with internal tagging so the reason is
/// part of the journaled evidence (the `CapabilityGrant` rule).
///
/// Every variant is a fail-closed answer: a scope check that cannot be
/// performed is a check that fails ([`BrokerDenialReason::BrokerUnavailable`]),
/// and there is no degraded mode that skips it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum BrokerDenialReason {
    /// The presented token is not a handle this broker minted — malformed,
    /// or the signature did not verify. Parse failure and forgery are
    /// deliberately the same refusal: a broker that distinguishes them
    /// tells a prober which half to fix.
    UnknownHandle,

    /// The handle's TTL ran out. Handles are short-lived; the tool
    /// re-issues at its next use.
    HandleExpired {
        /// When the handle expired.
        expires_at: DateTime<Utc>,
    },

    /// The request named scopes outside the bound set — the consent set
    /// at issuance (beyond what the human granted), the handle's narrowed
    /// set at resolution (scope escalation refused).
    ScopeNotGranted {
        /// The scopes that were not granted, sorted.
        missing: Vec<String>,
    },

    /// The connection was revoked; outstanding handles fail at their
    /// next use. The grant travels in the denial — the release proof's
    /// "naming the connection and the revoked grant".
    ConnectionRevoked {
        /// The revoked consent scope set.
        grant: Vec<String>,
    },

    /// The refresh path failed terminally; a human must re-consent. A
    /// typed re-auth signal, never a silent retry with stale material.
    ConnectionNeedsReauth,

    /// No connection by that id in this tenant (unknown and cross-tenant
    /// are indistinguishable — 404, never 403).
    UnknownConnection,

    /// The broker could not perform the check (the store read failed,
    /// the evidence could not be journaled). Fail closed: the call is
    /// denied because the check did not happen.
    BrokerUnavailable,
}

/// A typed, journaled refusal. Attributable — it names the connection,
/// the handle, and the grant (or missing scope) that decided it — and it
/// can never carry credential bytes: no field of this type is
/// credential-shaped, by construction (the `MemoryForgetTombstone`
/// discipline: the receipt of an erasure cannot leak what was erased).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrokerDenial {
    /// The connection the denial concerns, when known (absent for an
    /// unparseable handle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,

    /// The handle the denial concerns, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle_id: Option<String>,

    /// The tenant the denial concerns, when known. Carried because the
    /// broker's evidence chain is deployment-wide and the tenant is what
    /// makes a denial attributable across it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,

    /// Why the call was refused. Flattened, so the internally-tagged
    /// reason sits at the denial's top level — `{"reason":
    /// "connection_revoked", "grant": […], …}` — one flat evidence
    /// object, never a `reason.reason` nesting.
    #[serde(flatten)]
    pub reason: BrokerDenialReason,

    /// Human-facing context: what was attempted, against which grant.
    pub detail: String,
}

impl BrokerDenial {
    /// An unparseable or badly signed token.
    pub fn unknown_handle(detail: String) -> Self {
        Self {
            connection_id: None,
            handle_id: None,
            tenant: None,
            reason: BrokerDenialReason::UnknownHandle,
            detail,
        }
    }

    /// A valid handle past its TTL.
    pub fn handle_expired(claims: &HandleClaims) -> Self {
        Self {
            connection_id: Some(claims.connection_id.clone()),
            handle_id: Some(claims.handle_id.clone()),
            tenant: Some(claims.tenant.clone()),
            reason: BrokerDenialReason::HandleExpired {
                expires_at: claims.expires_at,
            },
            detail: format!(
                "handle `{}` expired at {} — handles are short-lived; re-issue at the next use",
                claims.handle_id, claims.expires_at
            ),
        }
    }

    /// A request beyond the bound scope set (issuance against consent,
    /// or resolution against the handle's narrowed set). The missing
    /// scopes join the detail: a denial must name what was refused.
    pub fn scope_not_granted(
        claims_or_connection: Option<(&str, Option<&str>, &str)>,
        missing: Vec<String>,
        detail: String,
    ) -> Self {
        let (connection_id, handle_id, tenant) = match claims_or_connection {
            Some((connection, handle, tenant)) => (
                Some(connection.to_owned()),
                handle.map(str::to_owned),
                Some(tenant.to_owned()),
            ),
            None => (None, None, None),
        };
        Self {
            connection_id,
            handle_id,
            tenant,
            detail: format!("{detail} — missing scopes: {}", missing.join(", ")),
            reason: BrokerDenialReason::ScopeNotGranted { missing },
        }
    }

    /// A use against a revoked connection, naming the revoked grant.
    pub fn connection_revoked(record: &ConnectionRecord, tenant: &str, handle_id: &str) -> Self {
        Self {
            connection_id: Some(record.connection_id.clone()),
            handle_id: Some(handle_id.to_owned()),
            tenant: Some(tenant.to_owned()),
            reason: BrokerDenialReason::ConnectionRevoked {
                grant: record.scopes.iter().cloned().collect(),
            },
            detail: format!(
                "connection `{}` is revoked — the grant {} no longer holds; the next use fails \
                 closed, not the next deploy",
                record.connection_id,
                serde_json::to_string(&record.scopes).unwrap_or_default(),
            ),
        }
    }

    /// A use against a connection whose refresh path failed terminally.
    pub fn connection_needs_reauth(
        record: &ConnectionRecord,
        tenant: &str,
        handle_id: &str,
    ) -> Self {
        Self {
            connection_id: Some(record.connection_id.clone()),
            handle_id: Some(handle_id.to_owned()),
            tenant: Some(tenant.to_owned()),
            reason: BrokerDenialReason::ConnectionNeedsReauth,
            detail: format!(
                "connection `{}` needs re-authentication — a human must record a new consent act",
                record.connection_id
            ),
        }
    }

    /// A handle naming a connection the tenant does not hold.
    pub fn unknown_connection(claims: &HandleClaims) -> Self {
        Self {
            connection_id: Some(claims.connection_id.clone()),
            handle_id: Some(claims.handle_id.clone()),
            tenant: Some(claims.tenant.clone()),
            reason: BrokerDenialReason::UnknownConnection,
            detail: format!(
                "connection `{}` is unknown to tenant `{}`",
                claims.connection_id, claims.tenant
            ),
        }
    }

    /// The check itself could not be performed — fail closed.
    pub fn unavailable(detail: String) -> Self {
        Self {
            connection_id: None,
            handle_id: None,
            tenant: None,
            reason: BrokerDenialReason::BrokerUnavailable,
            detail,
        }
    }
}

impl std::fmt::Display for BrokerDenial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tag = match &self.reason {
            BrokerDenialReason::UnknownHandle => "unknown_handle",
            BrokerDenialReason::HandleExpired { .. } => "handle_expired",
            BrokerDenialReason::ScopeNotGranted { .. } => "scope_not_granted",
            BrokerDenialReason::ConnectionRevoked { .. } => "connection_revoked",
            BrokerDenialReason::ConnectionNeedsReauth => "connection_needs_reauth",
            BrokerDenialReason::UnknownConnection => "unknown_connection",
            BrokerDenialReason::BrokerUnavailable => "broker_unavailable",
        };
        write!(f, "{tag}: {}", self.detail)
    }
}

impl std::error::Error for BrokerDenial {}

// --------------------------------------------------------------------- //
// Journaled payloads
// --------------------------------------------------------------------- //

/// The journaled consent act (output of
/// [`RunEventKind::ConnectionConsented`](crate::record::RunEventKind::ConnectionConsented)):
/// the human's grant at the provider, recorded. Scope widening is *only*
/// ever this — a new consent act, journaled; there is no API path that
/// widens a connection's consented set silently.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionConsent {
    /// The connection the consent governs.
    pub connection_id: String,

    /// The subject the consent binds, when per-user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,

    /// The consent scope set as recorded — the new ceiling.
    pub scopes: BTreeSet<String>,

    /// When the consent was recorded.
    pub recorded_at: DateTime<Utc>,
}

/// The journaled token-material update (output of
/// [`RunEventKind::ConnectionRefreshed`](crate::record::RunEventKind::ConnectionRefreshed)):
/// new material beneath the same consent set — a recorded credential
/// rotation. Names the connection and the new expiry; never the bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionRefresh {
    /// The connection whose material rotated.
    pub connection_id: String,

    /// When the new material was sealed.
    pub refreshed_at: DateTime<Utc>,

    /// The new access token's expiry, when the provider declares one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

/// The journaled revocation (output of
/// [`RunEventKind::ConnectionRevoked`](crate::record::RunEventKind::ConnectionRevoked)).
/// The status flip and this event commit together; outstanding handles
/// fail at their next use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionRevocation {
    /// The revoked connection.
    pub connection_id: String,

    /// The consent scope set that stopped holding — the revoked grant.
    pub grant: BTreeSet<String>,

    /// When the revocation was recorded.
    pub revoked_at: DateTime<Utc>,

    /// The operator's reason, when given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The journaled handle issuance (output of
/// [`RunEventKind::CredentialHandleIssued`](crate::record::RunEventKind::CredentialHandleIssued)):
/// the full claims, so the run's evidence pins the connection id and the
/// consent scope set it resolved — credential rotation beneath the
/// stable connection id changes nothing pinned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandleIssuance {
    /// The issued claims.
    pub claims: HandleClaims,
}

/// The journaled resolution (output of
/// [`RunEventKind::CredentialUse`](crate::record::RunEventKind::CredentialUse)):
/// metadata — handle, connection, the scopes checked — never bytes (the
/// `CapsuleUse` precedent).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CredentialUse {
    /// The handle that resolved.
    pub handle_id: String,

    /// The connection the credential came from.
    pub connection_id: String,

    /// The tenant the resolution served.
    pub tenant: String,

    /// The run the handle was bound to, when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,

    /// The scopes this call required and the handle covered.
    pub scopes_checked: BTreeSet<String>,

    /// When the resolution happened.
    pub used_at: DateTime<Utc>,
}

// --------------------------------------------------------------------- //
// The broker seam
// --------------------------------------------------------------------- //

/// What a tool (or capsule, or MCP/A2A client call) declares it needs:
/// the connection to draw on and the scopes *this call* requires. The
/// declaration is the unit of narrowing — issuance checks it against the
/// consent ceiling, resolution checks it against the handle's narrowed
/// set, and a tool asking beyond either is denied with the missing
/// scopes named.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialRequirement {
    /// The connection to draw on.
    pub connection_id: String,

    /// The scopes this call requires, in provider semantics. An empty
    /// set asks for no narrowing: issuance binds the handle to the
    /// connection's whole consent set (the capsule `Secret` grant's
    /// posture — the grant names the connection wholesale).
    pub scopes: BTreeSet<String>,
}

/// One handle issuance request against a [`CredentialBroker`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRequest {
    /// The tenant the handle is issued to.
    pub tenant: String,

    /// The run to bind the handle to, when the issuance is run-bound.
    pub run_id: Option<String>,

    /// The declared need.
    pub requirement: CredentialRequirement,
}

/// What resolution hands to the **connector** — the host's HTTP/tool-call
/// boundary — and never to tool code: the decrypted credential plus the
/// metadata of the resolution it came from. Redacted in `Debug`; no
/// `Serialize` impl, because nothing about a resolved credential is
/// evidence — the [`CredentialUse`] event is.
#[derive(Clone)]
pub struct ResolvedCredential {
    /// The connection the credential came from.
    pub connection_id: String,

    /// The handle that resolved.
    pub handle_id: String,

    /// The handle's narrowed scope set (the ceiling this resolution was
    /// checked against).
    pub scopes: BTreeSet<String>,

    /// The decrypted token material. The connector injects it into the
    /// outbound request; tool code never sees this struct.
    pub material: TokenMaterial,
}

impl std::fmt::Debug for ResolvedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedCredential")
            .field("connection_id", &self.connection_id)
            .field("handle_id", &self.handle_id)
            .field("scopes", &self.scopes)
            .field("material", &"[redacted]")
            .finish()
    }
}

/// The broker seam: where a deployment plugs credential custody. Core
/// owns the contracts and the journaled shapes; the server owns key
/// custody, envelope encryption, and persistence — the `NetworkConnector`
/// boundary, drawn for the same reason.
///
/// Implementations MUST journal: every issuance, every resolution, and
/// every denial, typed and naming the connection and grant — never the
/// bytes. A use that cannot journal must fail closed
/// ([`BrokerDenialReason::BrokerUnavailable`]): evidence is not
/// optional on this surface.
#[async_trait]
pub trait CredentialBroker: std::fmt::Debug + Send + Sync {
    /// Issue a short-lived handle for one declared need. Issuance checks
    /// the connection's live state (active, not revoked or
    /// `needs_reauth`) and the consent ceiling (the requested scopes
    /// must be covered — a tool requesting beyond the consented set is
    /// denied *here*, not at the provider).
    async fn issue(
        &self,
        request: &IssueRequest,
    ) -> std::result::Result<CredentialHandle, BrokerDenial>;

    /// Resolve a handle for one operation: verify the signature, check
    /// expiry and scope coverage from the self-contained claims, then
    /// read the connection's *live* state (revocation takes effect at
    /// the next call, never the next deploy), and return the credential
    /// to the connector. Denials are typed and journaled.
    async fn resolve(
        &self,
        token: &str,
        scopes: &BTreeSet<String>,
    ) -> std::result::Result<ResolvedCredential, BrokerDenial>;
}

// --------------------------------------------------------------------- //
// ToolExecutor integration
// --------------------------------------------------------------------- //

/// A tool that draws on a broker connection: a [`Tool`] plus the two
/// things the broker needs — the per-call declaration of what it
/// requires, and an entry point that receives the issued handle. The
/// handle is all `call_authenticated` gets: the tool presents it to the
/// host-side connector, and the connector (holding a
/// [`CredentialBroker`]) resolves it into the credential it injects —
/// the bytes never enter tool code.
#[async_trait]
pub trait CredentialTool: Tool {
    /// The connection and scopes this concrete call requires. Per-call
    /// because the operation decides the scope — a `search.read` call and
    /// a `drive.write` call through the same tool must not share a
    /// ceiling.
    fn credential_requirement(&self, args: &Value) -> CredentialRequirement;

    /// Execute with the issued handle. The handle is opaque: present it
    /// to the connector; there is nothing here to exfiltrate.
    async fn call_authenticated(&self, args: Value, handle: &CredentialHandle) -> Result<Value>;
}

/// The mediator's handle cache: `(run, connection, scopes)` → a live
/// handle and the instant to re-issue at (three quarters through the
/// TTL, so a handle never expires mid-call the way a boundary-checked
/// one could).
type HandleCache =
    HashMap<(Option<String>, String, BTreeSet<String>), (CredentialHandle, DateTime<Utc>)>;

/// The run-scoped issuance half of the `ToolExecutor` integration: one
/// broker, one tenant, an optional run binding, and a small cache of
/// live handles so a tool "receives a handle at first use" and reuses it
/// through its TTL rather than minting one per call. The cache only
/// defers *issuance* — every resolution still reads live connection
/// state, so caching a handle cannot cache a revocation.
#[derive(Clone)]
pub struct CredentialMediator {
    broker: Arc<dyn CredentialBroker>,
    tenant: String,
    run_id: Option<String>,
    handles: Arc<Mutex<HandleCache>>,
}

impl std::fmt::Debug for CredentialMediator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialMediator")
            .field("tenant", &self.tenant)
            .field("run_id", &self.run_id)
            .finish()
    }
}

impl CredentialMediator {
    /// A mediator over `broker` issuing for `tenant`, unbound to a run.
    pub fn new(broker: Arc<dyn CredentialBroker>, tenant: impl Into<String>) -> Self {
        Self {
            broker,
            tenant: tenant.into(),
            run_id: None,
            handles: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The same mediator bound to a run. The cache is shared (keyed on
    /// the run binding), so rebinding never strands a live handle.
    pub fn for_run(&self, run_id: Option<String>) -> Self {
        Self {
            broker: Arc::clone(&self.broker),
            tenant: self.tenant.clone(),
            run_id,
            handles: Arc::clone(&self.handles),
        }
    }

    /// The run this mediator binds handles to, when bound.
    pub fn run_id(&self) -> Option<&str> {
        self.run_id.as_deref()
    }

    /// The underlying broker (the connector's resolution path).
    pub fn broker(&self) -> &Arc<dyn CredentialBroker> {
        &self.broker
    }

    /// Issue (or serve from the live cache) a handle for one declared
    /// need. Denials propagate typed — the caller fails the tool call
    /// closed.
    pub async fn issue(
        &self,
        requirement: &CredentialRequirement,
    ) -> std::result::Result<CredentialHandle, BrokerDenial> {
        let key = (
            self.run_id.clone(),
            requirement.connection_id.clone(),
            requirement.scopes.clone(),
        );
        if let Some((handle, renew_after)) = self
            .handles
            .lock()
            .ok()
            .and_then(|cache| cache.get(&key).cloned())
        {
            if Utc::now() < renew_after {
                return Ok(handle);
            }
        }
        let handle = self
            .broker
            .issue(&IssueRequest {
                tenant: self.tenant.clone(),
                run_id: self.run_id.clone(),
                requirement: requirement.clone(),
            })
            .await?;
        let claims = handle.claims();
        // Re-issue at three quarters of the TTL: the cached handle is
        // always comfortably live when served, and the re-issue path is
        // exercised routinely rather than only at the boundary.
        let ttl = claims.expires_at - claims.issued_at;
        let renew_after = claims.issued_at + ttl * 3 / 4;
        if let Ok(mut cache) = self.handles.lock() {
            cache.insert(key, (handle.clone(), renew_after));
        }
        Ok(handle)
    }

    /// Wrap one credential-requiring tool so ordinary `ToolExecutor`
    /// dispatch mediates it: the executor needs no changes — issuance
    /// happens inside the tool's own `call`, scope-checked at the
    /// broker, with the effect boundary transparently delegated.
    pub fn mediate(&self, tool: Arc<dyn CredentialTool>) -> MediatedTool {
        MediatedTool {
            inner: tool,
            mediator: self.clone(),
        }
    }
}

/// The [`Tool`] wrapper [`CredentialMediator::mediate`] produces. Every
/// reflective method delegates to the inner tool — a wrapper must remain
/// transparent to the effect boundary (`Tool::effect_request`'s own
/// rule) — and `call` inserts exactly one step: issue, then hand the
/// inner tool the handle.
pub struct MediatedTool {
    inner: Arc<dyn CredentialTool>,
    mediator: CredentialMediator,
}

impl std::fmt::Debug for MediatedTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediatedTool")
            .field("tool", &self.inner.name())
            .finish()
    }
}

#[async_trait]
impl Tool for MediatedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> Value {
        self.inner.parameters_schema()
    }

    fn effect(&self) -> Effect {
        self.inner.effect()
    }

    fn effect_kind(&self) -> &str {
        self.inner.effect_kind()
    }

    fn idempotency_key(&self, args: &Value) -> Option<String> {
        self.inner.idempotency_key(args)
    }

    fn effect_request(&self, call: &crate::llm::ToolCall) -> crate::effects::EffectRequest {
        self.inner.effect_request(call)
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let requirement = self.inner.credential_requirement(&args);
        let handle =
            self.mediator.issue(&requirement).await.map_err(|denial| {
                RustyError::Tool(format!("credential issuance denied: {denial}"))
            })?;
        self.inner.call_authenticated(args, &handle).await
    }
}

// --------------------------------------------------------------------- //
// Capsule-host integration (feature `wasm`)
// --------------------------------------------------------------------- //

/// The capsule half of the broker (R0.9 secret-handle precedent,
/// extended): a [`CapsuleHost`](crate::capsule_host::CapsuleHost)
/// wrapper that brokers the manifest's `Secret` grants into issued
/// handle tokens before the guest runs.
///
/// The v1 world has no secret import, so the tokens travel the one
/// channel a guest already has: its input. Each granted name — now a
/// broker connection id — is issued a handle (run-bound when the
/// invocation journals into a run) and injected under the reserved
/// `secrets` key. The guest receives opaque tokens and nothing else; a
/// later world version's credential import resolves them through the
/// same broker the host wired here. Two fail-closed edges, by
/// construction: an issuance denial refuses the invocation before guest
/// code runs (the broker journals it), and an input that already
/// carries a `secrets` key is refused — a guest smuggling its own
/// "tokens" is indistinguishable from one the host issued, so the
/// collision is an admission error, never an overwrite.
#[cfg(feature = "wasm")]
#[derive(Clone)]
pub struct BrokeredCapsuleHost {
    host: crate::capsule_host::CapsuleHost,
    mediator: CredentialMediator,
}

#[cfg(feature = "wasm")]
impl std::fmt::Debug for BrokeredCapsuleHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokeredCapsuleHost")
            .field("host", &self.host)
            .field("mediator", &self.mediator)
            .finish()
    }
}

#[cfg(feature = "wasm")]
impl BrokeredCapsuleHost {
    /// Wrap `host` with credential mediation.
    pub fn new(host: crate::capsule_host::CapsuleHost, mediator: CredentialMediator) -> Self {
        Self { host, mediator }
    }

    /// The underlying host.
    pub fn host(&self) -> &crate::capsule_host::CapsuleHost {
        &self.host
    }

    /// Invoke with brokered secrets: issue one handle per granted
    /// connection, inject the tokens into the guest's input, then run
    /// the host's ordinary invocation — whose journaled `WasmCall` input
    /// is itself the evidence that the guest received tokens, and whose
    /// capability enforcement is untouched.
    pub async fn invoke(
        &self,
        invocation: crate::capsule_host::CapsuleInvocation,
    ) -> Result<crate::capsule_host::CapsuleOutcome> {
        use crate::capsule::CapabilityGrant;
        use std::collections::BTreeMap;
        let connections: Vec<String> = self
            .host
            .manifest()
            .capabilities
            .iter()
            .flat_map(|grant| match grant {
                CapabilityGrant::Secret { handles } => handles.as_slice(),
                _ => &[],
            })
            .cloned()
            .collect();
        if connections.is_empty() {
            return self.host.invoke(invocation).await;
        }
        // The smuggling guard runs before any issuance: an input that
        // already carries `secrets` is refused with nothing minted.
        let mut input = invocation.input.clone();
        match &input {
            Value::Object(map) => {
                if map.contains_key("secrets") {
                    return Err(RustyError::Node(
                        "capsule input already carries a `secrets` key — the host issues those \
                         tokens; a guest supplying its own is refused at admission"
                            .to_owned(),
                    ));
                }
            }
            _ => {
                return Err(RustyError::Node(
                    "capsule input must be a JSON object when the manifest grants secrets — \
                     the host injects handle tokens under the reserved `secrets` key"
                        .to_owned(),
                ));
            }
        }
        // The invocation's run is the binding of choice; an embedder's
        // mediator run binding is the fallback, never an invented one.
        let run_id = invocation
            .journal
            .as_ref()
            .map(|journal| journal.run_id().to_owned())
            .or_else(|| self.mediator.run_id().map(str::to_owned));
        let mediator = self.mediator.for_run(run_id);
        let mut tokens = BTreeMap::new();
        for connection_id in connections {
            let requirement = CredentialRequirement {
                connection_id: connection_id.clone(),
                // The `Secret` grant names the connection wholesale —
                // no narrowing exists in the grant model, so the handle
                // binds the whole consent set.
                scopes: BTreeSet::new(),
            };
            let handle = mediator.issue(&requirement).await.map_err(|denial| {
                RustyError::Node(format!(
                    "capsule secret grant `{connection_id}` failed at the broker: {denial}"
                ))
            })?;
            tokens.insert(connection_id, handle.token());
        }
        if let Value::Object(map) = &mut input {
            let value = serde_json::to_value(&tokens).map_err(RustyError::Serialization)?;
            map.insert("secrets".to_owned(), value);
        }
        self.host
            .invoke(crate::capsule_host::CapsuleInvocation {
                input,
                journal: invocation.journal,
                parent: invocation.parent,
                budget: invocation.budget,
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(millis: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
    }

    fn record() -> ConnectionRecord {
        ConnectionRecord {
            connection_id: format!("{}{}", CONNECTION_ID_PREFIX, "a".repeat(32)),
            provider: ConnectionProvider::Oauth2AuthorizationCode,
            subject: Some("user-7".into()),
            scopes: BTreeSet::from(["drive.readonly".to_owned(), "drive.write".to_owned()]),
            status: ConnectionStatus::Active,
            health: ConnectionHealth::default(),
            created_at: ts(1_800_000_000_000),
            updated_at: ts(1_800_000_000_000),
        }
    }

    fn claims() -> HandleClaims {
        HandleClaims {
            handle_id: format!("{}{}", HANDLE_ID_PREFIX, "b".repeat(32)),
            connection_id: format!("{}{}", CONNECTION_ID_PREFIX, "a".repeat(32)),
            tenant: "acme".into(),
            run_id: Some("run-1".into()),
            scopes: BTreeSet::from(["drive.readonly".to_owned()]),
            issued_at: ts(1_800_000_000_000),
            expires_at: ts(1_800_000_300_000),
        }
    }

    #[test]
    fn minted_ids_fit_their_grammar() {
        let connection = new_connection_id();
        let handle = new_handle_id();
        assert!(minted_id_ok(CONNECTION_ID_PREFIX, &connection));
        assert!(minted_id_ok(HANDLE_ID_PREFIX, &handle));
        assert!(record().validate().is_ok());
        // Hand-shaped ids fail: the grammar is the mint's, not the
        // caller's.
        let mut bad = record();
        bad.connection_id = "conn-XYZ".into();
        assert!(bad.validate().is_err());
        // The tenant separator is refused in subjects and scopes.
        let mut bad = record();
        bad.subject = Some("acme/user".into());
        assert!(bad.validate().is_err());
        let mut bad = record();
        bad.scopes.insert("a/b".into());
        assert!(bad.validate().is_err());
    }

    #[test]
    fn scope_narrowing_names_exactly_the_missing() {
        let ceiling = BTreeSet::from(["a".to_owned(), "b".to_owned()]);
        assert!(scopes_missing(&ceiling, &BTreeSet::new()).is_empty());
        assert!(scopes_missing(&ceiling, &BTreeSet::from(["a".to_owned()])).is_empty());
        assert_eq!(
            scopes_missing(&ceiling, &BTreeSet::from(["a".to_owned(), "c".to_owned()])),
            vec!["c".to_owned()]
        );
    }

    #[test]
    fn token_material_debug_never_shows_bytes() {
        let material = TokenMaterial {
            access_token: "sk-live-MARKER".into(),
            refresh_token: Some("rt-MARKER".into()),
            expires_at: Some(ts(1_800_000_300_000)),
        };
        let rendered = format!("{material:?}");
        assert!(!rendered.contains("sk-live-MARKER"), "got: {rendered}");
        assert!(!rendered.contains("rt-MARKER"), "got: {rendered}");
        assert!(rendered.contains("[redacted]"));
    }

    #[test]
    fn handle_token_round_trips_and_refuses_forged_shapes() {
        let handle = CredentialHandle::from_parts(claims(), "cd".repeat(32));
        let token = handle.token();
        let (parsed, signature) = CredentialHandle::parse_token(&token).unwrap();
        assert_eq!(parsed, claims());
        assert_eq!(signature, "cd".repeat(32));
        // Malformed tokens are the same refusal as forgeries.
        for bad in [
            "",
            "v2.whatever.sig",
            "v1.zzzz.sig",
            "v1.aGV4.sig",
            &token.replace("v1.", "v0."),
        ] {
            let err = CredentialHandle::parse_token(bad).unwrap_err();
            assert!(
                matches!(err.reason, BrokerDenialReason::UnknownHandle),
                "{bad:?} -> {err:?}"
            );
        }
        // Claims failing the handle grammar (expiry not after issuance)
        // are refused at parse, before any signature check.
        let mut bad_claims = claims();
        bad_claims.expires_at = bad_claims.issued_at;
        let bad = CredentialHandle::from_parts(bad_claims, "cd".repeat(32)).token();
        assert!(CredentialHandle::parse_token(&bad).is_err());
        // The handle's Debug shows claims (metadata) but never the
        // signature.
        let rendered = format!("{handle:?}");
        assert!(rendered.contains("handle_id"));
        assert!(!rendered.contains(&"cd".repeat(32)), "got: {rendered}");
    }

    #[test]
    fn expiry_is_at_the_instant() {
        let claims = claims();
        assert!(!claims.is_expired(ts(1_800_000_299_999)));
        assert!(claims.is_expired(ts(1_800_000_300_000)));
    }

    #[test]
    fn denial_payloads_are_attributable_and_byte_free() {
        let denial = BrokerDenial::connection_revoked(&record(), "acme", "hdl-x");
        match &denial.reason {
            BrokerDenialReason::ConnectionRevoked { grant } => {
                assert_eq!(
                    grant,
                    &vec!["drive.readonly".to_owned(), "drive.write".to_owned()]
                )
            }
            other => panic!("expected revoked, got {other:?}"),
        }
        let value = serde_json::to_value(&denial).unwrap();
        assert_eq!(value["reason"], serde_json::json!("connection_revoked"));
        assert_eq!(
            value["grant"],
            serde_json::json!(["drive.readonly", "drive.write"]),
            "the internally-tagged reason flattens the revoked grant beside it"
        );
        assert_eq!(value["tenant"], serde_json::json!("acme"));
    }
}
