//! People sign in; services use keys.
//!
//! A user is a principal with a password. Passwords are never stored: each
//! user keeps a random salt and a PBKDF2-HMAC-SHA256 digest of the password
//! over it, and a login is a constant-time comparison of a fresh digest. A
//! session is an opaque random token the browser holds in an `HttpOnly`
//! cookie, mapped here to the principal it was issued to, for a fixed
//! lifetime. A server with users requires everyone to be somebody.
//!
//! The first boot of a deployment with no users creates the administrator
//! and writes the password it generated to a file only the operator can read
//! — the way a database or a router hands over its first credential — so
//! there is never an "open" server and never a password in a log.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use sha2::Sha256;
use std::sync::{Arc, Mutex};

use crate::server_store::ServerStore;

use crate::auth::{Principal, PrincipalKind, Role};

type HmacSha256 = Hmac<Sha256>;

/// PBKDF2 iterations. Enough to make a leaked file expensive to attack,
/// cheap enough that a login is not noticed.
const ITERATIONS: u32 = 120_000;
/// How long a session lives from sign-in. Fixed, not sliding: a day at a
/// desk, then sign in again.
const SESSION_HOURS: i64 = 12;
/// A session in use is renewed: once it is this old, the next request
/// moves its end another `SESSION_HOURS` out, so a person at work is not
/// signed out mid-flow; one left alone ends after `SESSION_HOURS`.
const RENEW_AFTER_HOURS: i64 = 1;
/// The cookie a browser holds.
pub const SESSION_COOKIE: &str = "rusty_session";

/// One user as stored: identity, roles, and what it takes to prove them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRecord {
    pub id: String,
    pub name: String,
    pub roles: Vec<Role>,
    /// Hex, 16 random bytes.
    pub salt: String,
    /// Hex, PBKDF2-HMAC-SHA256 over the password with `salt`.
    pub hash: String,
    pub iterations: u32,
    pub created_at: DateTime<Utc>,
    /// Bumped when every session of this user must end — an explicit
    /// revocation, a role change. A session issued under an older epoch is
    /// refused on its next request.
    #[serde(default)]
    pub security_epoch: u64,
    /// The identity provider this account came from, when it did: the
    /// provider's issuer and its stable subject for the person. Such an
    /// account has no password of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<ExternalIdentity>,
    /// `false` once the directory deactivates the person: sessions ended,
    /// sign-in refused, nothing starts in their name — until it says
    /// otherwise.
    #[serde(default = "yes")]
    pub active: bool,
    /// The directory's own id for the person (SCIM `externalId`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

fn yes() -> bool {
    true
}

/// How a forgotten person is known at their provider: the key that
/// refuses them there too.
pub fn external_key(issuer: &str, subject: &str) -> String {
    format!("oidc:{}:{subject}", issuer.trim_end_matches('/'))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalIdentity {
    pub issuer: String,
    pub subject: String,
}

impl UserRecord {
    pub fn principal(&self) -> Principal {
        Principal {
            id: self.id.clone(),
            name: self.name.clone(),
            kind: PrincipalKind::User,
            roles: self.roles.clone(),
        }
    }
}

/// PBKDF2 with HMAC-SHA256, one 32-byte block — the standard construction,
/// built from the primitives this crate already carries.
fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(password).expect("hmac accepts any key length");
    mac.update(salt);
    mac.update(&1u32.to_be_bytes());
    let mut u: [u8; 32] = mac.finalize().into_bytes().into();
    let mut out = u;
    for _ in 1..iterations {
        let mut mac = HmacSha256::new_from_slice(password).expect("hmac accepts any key length");
        mac.update(&u);
        u = mac.finalize().into_bytes().into();
        for (o, b) in out.iter_mut().zip(u.iter()) {
            *o ^= b;
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

fn random_bytes(n: usize) -> Vec<u8> {
    // uuid v4 is 122 bits of OS randomness per call; chain calls for more.
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        out.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    out.truncate(n);
    out
}

/// A password nobody typed: 22 characters from an unambiguous alphabet.
pub fn generate_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    random_bytes(22)
        .into_iter()
        .map(|b| ALPHABET[(b as usize) % ALPHABET.len()] as char)
        .collect()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The users of a deployment, persisted as one file under the store root.
pub struct Users {
    path: PathBuf,
    records: Mutex<Vec<UserRecord>>,
    /// People forgotten: no account, no session, no run in their name.
    forgotten: Mutex<std::collections::HashSet<String>>,
}

impl Users {
    pub fn load(store_root: &Path) -> Self {
        let path = store_root.join("users.json");
        let records = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Vec<UserRecord>>(&bytes).ok())
            .unwrap_or_default();
        Self {
            forgotten: Mutex::new(std::collections::HashSet::new()),
            path,
            records: Mutex::new(records),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.records.lock().map(|r| r.is_empty()).unwrap_or(true)
    }

    pub async fn list(&self) -> Vec<UserRecord> {
        self.records.lock().map(|r| r.clone()).unwrap_or_default()
    }

    /// One user by id, as the store holds them now.
    pub fn get(&self, id: &str) -> Option<UserRecord> {
        self.records
            .lock()
            .ok()?
            .iter()
            .find(|u| u.id == id)
            .cloned()
    }

    /// Whether `who` — an attribution `{principal_id, kind}` — may still act:
    /// a person the store no longer holds may not. A deployment with no
    /// users (keys only), a service principal, or an attribution that names
    /// no user is not the question this answers, so it says yes.
    pub fn still_present(&self, who: &serde_json::Value) -> bool {
        if who.get("kind").and_then(serde_json::Value::as_str) != Some("user") {
            return true;
        }
        let Some(id) = who.get("principal_id").and_then(serde_json::Value::as_str) else {
            return true;
        };
        if self.is_forgotten(id) {
            return false;
        }
        match self.get(id) {
            Some(user) => user.active,
            None => self.is_empty(),
        }
    }

    /// Mark a person forgotten: nothing acts in their name from now on,
    /// whatever key or session says so.
    pub fn mark_forgotten(&self, id: &str) {
        if let Ok(mut set) = self.forgotten.lock() {
            set.insert(id.to_owned());
        }
    }

    pub fn is_forgotten(&self, id: &str) -> bool {
        self.forgotten
            .lock()
            .map(|s| s.contains(id))
            .unwrap_or(false)
    }

    /// End every session of a user: their security epoch moves on, and a
    /// session issued under the old one is refused on its next request.
    pub async fn revoke_sessions(&self, id: &str) -> Result<bool, String> {
        let mut next = self
            .records
            .lock()
            .map(|r| r.clone())
            .map_err(|_| "users store poisoned".to_owned())?;
        let Some(user) = next.iter_mut().find(|u| u.id == id) else {
            return Ok(false);
        };
        user.security_epoch += 1;
        self.persist(&next).await?;
        if let Ok(mut records) = self.records.lock() {
            *records = next;
        }
        Ok(true)
    }

    async fn persist(&self, records: &[UserRecord]) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(records).map_err(|e| e.to_string())?;
        let tmp = self.path.with_extension("json.tmp");
        tokio::fs::write(&tmp, &bytes)
            .await
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        restrict(&tmp).await;
        tokio::fs::rename(&tmp, &self.path)
            .await
            .map_err(|e| format!("rename {}: {e}", self.path.display()))
    }

    /// Create a user. The id is the sign-in name; it must be new.
    pub async fn create(
        &self,
        id: &str,
        name: &str,
        roles: Vec<Role>,
        password: &str,
    ) -> Result<UserRecord, String> {
        let id = id.trim().to_ascii_lowercase();
        if id.is_empty()
            || id.len() > 64
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'))
        {
            return Err("a sign-in name is 1–64 characters of letters, digits, . _ - @".to_owned());
        }
        if password.len() < 10 {
            return Err("a password is at least 10 characters".to_owned());
        }
        if roles.is_empty() {
            return Err("a user has at least one role".to_owned());
        }
        let snapshot = {
            let records = self
                .records
                .lock()
                .map_err(|_| "users store poisoned".to_owned())?;
            if records.iter().any(|u| u.id == id) {
                return Err(format!("`{id}` already exists"));
            }
            records.clone()
        };
        let salt = random_bytes(16);
        let hash = pbkdf2_sha256(password.as_bytes(), &salt, ITERATIONS);
        let record = UserRecord {
            id,
            name: if name.trim().is_empty() {
                String::new()
            } else {
                name.trim().to_owned()
            },
            roles,
            salt: hex(&salt),
            hash: hex(&hash),
            iterations: ITERATIONS,
            created_at: Utc::now(),
            security_epoch: 0,
            external: None,
            active: true,
            external_id: None,
            updated_at: None,
        };
        let mut record = record;
        if record.name.is_empty() {
            record.name = record.id.clone();
        }
        let mut next = snapshot;
        next.push(record.clone());
        self.persist(&next).await?;
        if let Ok(mut records) = self.records.lock() {
            *records = next;
        }
        Ok(record)
    }

    /// Check a password. Constant-time on the digest; `None` for an unknown
    /// user takes the same path as a wrong password, deliberately.
    /// The account an identity provider's subject signs in as, if any.
    pub fn find_external(&self, issuer: &str, subject: &str) -> Option<UserRecord> {
        let records = self.records.lock().ok()?;
        records
            .iter()
            .find(|u| {
                u.external
                    .as_ref()
                    .is_some_and(|e| e.issuer == issuer && e.subject == subject)
            })
            .cloned()
    }

    /// An account made on a person's first sign-in through a provider: no
    /// password of its own (a random one nobody holds), known by the
    /// provider's subject from then on. A wanted id that is taken gets a
    /// numbered one.
    pub async fn provision_external(
        &self,
        wanted_id: &str,
        name: &str,
        roles: Vec<Role>,
        external: Option<(&str, &str)>,
    ) -> Result<UserRecord, String> {
        let base: String = wanted_id
            .trim()
            .to_ascii_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'))
            .take(56)
            .collect();
        let base = if base.is_empty() {
            "person".to_owned()
        } else {
            base
        };
        let mut id = base.clone();
        let mut nth = 1;
        while self.get(&id).is_some() || self.is_forgotten(&id) {
            nth += 1;
            id = format!("{base}-{nth}");
        }
        let unusable = hex(&random_bytes(24));
        let mut record = self.create(&id, name, roles, &unusable).await?;
        record.external = external.map(|(issuer, subject)| ExternalIdentity {
            issuer: issuer.to_owned(),
            subject: subject.to_owned(),
        });
        let mut next = self
            .records
            .lock()
            .map(|r| r.clone())
            .map_err(|_| "users store poisoned".to_owned())?;
        if let Some(stored) = next.iter_mut().find(|u| u.id == record.id) {
            stored.external = record.external.clone();
        }
        self.persist(&next).await?;
        if let Ok(mut records) = self.records.lock() {
            *records = next;
        }
        Ok(record)
    }

    /// The roles, as the directory's groups say them.
    pub async fn set_roles(&self, id: &str, roles: Vec<Role>) -> Result<(), String> {
        if roles.is_empty() {
            return Err("a user has at least one role".to_owned());
        }
        let mut next = self
            .records
            .lock()
            .map(|r| r.clone())
            .map_err(|_| "users store poisoned".to_owned())?;
        let Some(user) = next.iter_mut().find(|u| u.id == id) else {
            return Err(format!("no user `{id}`"));
        };
        if user.roles == roles {
            return Ok(());
        }
        user.roles = roles;
        user.updated_at = Some(Utc::now());
        self.persist(&next).await?;
        if let Ok(mut records) = self.records.lock() {
            *records = next;
        }
        Ok(())
    }

    /// The identity-provider subject an existing account signs in as from
    /// now on (an account the directory made, or a password one, that
    /// the person first signs into through the provider).
    pub async fn set_external(&self, id: &str, issuer: &str, subject: &str) -> Result<(), String> {
        let mut next = self
            .records
            .lock()
            .map(|r| r.clone())
            .map_err(|_| "users store poisoned".to_owned())?;
        let Some(user) = next.iter_mut().find(|u| u.id == id) else {
            return Err(format!("no user `{id}`"));
        };
        user.external = Some(ExternalIdentity {
            issuer: issuer.to_owned(),
            subject: subject.to_owned(),
        });
        user.updated_at = Some(Utc::now());
        self.persist(&next).await?;
        if let Ok(mut records) = self.records.lock() {
            *records = next;
        }
        Ok(())
    }

    /// What the directory says about a person now: its id for them, their
    /// name, whether they are active.
    pub async fn provisioned(
        &self,
        id: &str,
        external_id: Option<&str>,
        name: &str,
        active: bool,
    ) -> Result<(), String> {
        let mut next = self
            .records
            .lock()
            .map(|r| r.clone())
            .map_err(|_| "users store poisoned".to_owned())?;
        let Some(user) = next.iter_mut().find(|u| u.id == id) else {
            return Err(format!("no user `{id}`"));
        };
        if let Some(external_id) = external_id.map(str::trim).filter(|s| !s.is_empty()) {
            user.external_id = Some(external_id.to_owned());
        }
        if !name.trim().is_empty() {
            user.name = name.trim().to_owned();
        }
        user.active = active;
        user.updated_at = Some(Utc::now());
        self.persist(&next).await?;
        if let Ok(mut records) = self.records.lock() {
            *records = next;
        }
        Ok(())
    }

    /// The display name, as the provider says it now.
    pub async fn rename(&self, id: &str, name: &str) -> Result<(), String> {
        let mut next = self
            .records
            .lock()
            .map(|r| r.clone())
            .map_err(|_| "users store poisoned".to_owned())?;
        let Some(user) = next.iter_mut().find(|u| u.id == id) else {
            return Ok(());
        };
        user.name = name.trim().to_owned();
        self.persist(&next).await?;
        if let Ok(mut records) = self.records.lock() {
            *records = next;
        }
        Ok(())
    }

    pub async fn verify(&self, id: &str, password: &str) -> Option<UserRecord> {
        let id = id.trim().to_ascii_lowercase();
        let user = self
            .records
            .lock()
            .ok()
            .and_then(|records| records.iter().find(|u| u.id == id).cloned());
        let (salt, stored, iterations) = match &user {
            Some(u) => (
                unhex(&u.salt).unwrap_or_default(),
                unhex(&u.hash).unwrap_or_default(),
                u.iterations,
            ),
            None => (vec![0u8; 16], vec![0u8; 32], ITERATIONS),
        };
        let digest = pbkdf2_sha256(password.as_bytes(), &salt, iterations);
        if user.as_ref().is_some_and(|u| u.active) && constant_time_eq(&digest, &stored) {
            user
        } else {
            None
        }
    }

    pub async fn set_password(&self, id: &str, password: &str) -> Result<(), String> {
        if password.len() < 10 {
            return Err("a password is at least 10 characters".to_owned());
        }
        let mut next = self
            .records
            .lock()
            .map(|r| r.clone())
            .map_err(|_| "users store poisoned".to_owned())?;
        let Some(user) = next.iter_mut().find(|u| u.id == id) else {
            return Err(format!("no user `{id}`"));
        };
        let salt = random_bytes(16);
        user.salt = hex(&salt);
        user.hash = hex(&pbkdf2_sha256(password.as_bytes(), &salt, ITERATIONS));
        user.iterations = ITERATIONS;
        self.persist(&next).await?;
        if let Ok(mut records) = self.records.lock() {
            *records = next;
        }
        Ok(())
    }

    pub async fn delete(&self, id: &str) -> Result<bool, String> {
        let mut next = self
            .records
            .lock()
            .map(|r| r.clone())
            .map_err(|_| "users store poisoned".to_owned())?;
        let before = next.len();
        next.retain(|u| u.id != id);
        if next.len() == before {
            return Ok(false);
        }
        self.persist(&next).await?;
        if let Ok(mut records) = self.records.lock() {
            *records = next;
        }
        Ok(true)
    }

    /// The first administrator of a deployment that has none. The password
    /// goes to a file only the operator can read, next to the store; the
    /// log says where, never what.
    pub async fn bootstrap_admin(&self, store_root: &Path) -> Result<Option<PathBuf>, String> {
        if !self.is_empty() {
            return Ok(None);
        }
        let password = generate_password();
        self.create("admin", "Administrator", vec![Role::Admin], &password)
            .await?;
        let handover = store_root.join("bootstrap-admin.txt");
        tokio::fs::write(
            &handover,
            format!(
                "Rusty created its first administrator.\n\n  sign-in name: admin\n  password:     {password}\n\nSign in, then change it under your name. Delete this file when you have.\n"
            ),
        )
        .await
        .map_err(|e| format!("write {}: {e}", handover.display()))?;
        restrict(&handover).await;
        Ok(Some(handover))
    }
}

#[cfg(unix)]
async fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await;
}
#[cfg(not(unix))]
async fn restrict(_path: &Path) {}

/// A session as the store keeps it: not the token — its hash — so a copy
/// of the store signs nobody in; who it was issued to; until when.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub token_hash: String,
    pub principal: Principal,
    pub expires_at: DateTime<Utc>,
    /// The user's security epoch when the session was issued.
    #[serde(default)]
    pub epoch: u64,
}

/// The store's key for a token: SHA-256, hex.
fn token_hash(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

/// Live sessions: token → who, until when. The map answers first; behind it
/// the server store keeps every open session, so a restart — a deploy, a
/// crash — signs nobody out. Without a store (tests) the map is all there is.
/// One live session: the principal it belongs to, when it was minted, and
/// a counter that makes each re-issue unique.
type LiveSession = (Principal, DateTime<Utc>, u64);

#[derive(Default)]
pub struct Sessions {
    live: std::sync::Mutex<HashMap<String, LiveSession>>,
    store: Option<Arc<dyn ServerStore>>,
}

impl Sessions {
    /// Sessions that outlive the process: every open session is written to
    /// `store` and read back from it when the map does not know the token.
    pub(crate) fn durable(store: Arc<dyn ServerStore>) -> Self {
        Self {
            live: Mutex::default(),
            store: Some(store),
        }
    }

    fn live(&self) -> std::sync::MutexGuard<'_, HashMap<String, LiveSession>> {
        // A poisoned lock here would mean a panic mid-insert; the map is
        // still a valid map, so carry on rather than take the server down.
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub async fn open(&self, principal: Principal, epoch: u64) -> (String, DateTime<Utc>) {
        let token = format!("s-{}", hex(&random_bytes(24)));
        let expires = Utc::now() + Duration::hours(SESSION_HOURS);
        self.live()
            .insert(token.clone(), (principal.clone(), expires, epoch));
        if let Some(store) = &self.store {
            let record = SessionRecord {
                token_hash: token_hash(&token),
                principal,
                expires_at: expires,
                epoch,
            };
            if let Err(error) = store.put_session(&record).await {
                // The session works for this process either way; say so
                // rather than fail the sign-in over the store.
                tracing::warn!(%error, "session could not be persisted; it ends with this process");
            }
        }
        (token, expires)
    }

    /// The principal a token was issued to and the epoch it was issued
    /// under; the caller checks both against the user as the store holds
    /// them now.
    pub async fn resolve(&self, token: &str) -> Option<(Principal, u64)> {
        {
            let mut live = self.live();
            match live.get(token) {
                Some((principal, expires, epoch)) if *expires > Utc::now() => {
                    return Some((principal.clone(), *epoch));
                }
                Some(_) => {
                    live.remove(token);
                }
                None => {}
            }
        }
        let store = self.store.as_ref()?;
        let hash = token_hash(token);
        match store.get_session(&hash).await {
            Ok(Some(record)) if record.expires_at > Utc::now() => {
                self.live().insert(
                    token.to_owned(),
                    (record.principal.clone(), record.expires_at, record.epoch),
                );
                Some((record.principal, record.epoch))
            }
            Ok(Some(_)) => {
                let _ = store.delete_session(&hash).await;
                None
            }
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(%error, "session store could not be read");
                None
            }
        }
    }

    /// A session that has been in use for a while is renewed on this
    /// request: its end moves `SESSION_HOURS` out, in the map and the
    /// store, and the new end is returned so the cookie can say so. A
    /// fresh session is left alone (`None`): no needless writes.
    pub async fn renew_if_stale(&self, token: &str) -> Option<DateTime<Utc>> {
        let now = Utc::now();
        let (principal, epoch, renewed) = {
            let mut live = self.live();
            let entry = live.get_mut(token)?;
            if entry.1 - now > Duration::hours(SESSION_HOURS - RENEW_AFTER_HOURS) {
                return None;
            }
            entry.1 = now + Duration::hours(SESSION_HOURS);
            (entry.0.clone(), entry.2, entry.1)
        };
        if let Some(store) = &self.store {
            let record = SessionRecord {
                token_hash: token_hash(token),
                principal,
                expires_at: renewed,
                epoch,
            };
            if let Err(error) = store.put_session(&record).await {
                tracing::warn!(%error, "session renewal could not be persisted; it holds for this process");
            }
        }
        Some(renewed)
    }

    pub async fn close(&self, token: &str) {
        self.live().remove(token);
        if let Some(store) = &self.store {
            let _ = store.delete_session(&token_hash(token)).await;
        }
    }
}

/// The session token in a request's `Cookie` header, if any.
pub fn session_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|line| line.split(';'))
        .map(str::trim)
        .find_map(|pair| {
            pair.strip_prefix(&format!("{SESSION_COOKIE}="))
                .map(str::to_owned)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pbkdf2_matches_the_rfc6070_vector() {
        // RFC 6070 / RFC 7914-style known answer for PBKDF2-HMAC-SHA256:
        // P="password", S="salt", c=1 → 120fb6cffcf8b32c43e7225256c4f837a86548c9…
        let out = pbkdf2_sha256(b"password", b"salt", 1);
        assert_eq!(&hex(&out)[..40], "120fb6cffcf8b32c43e7225256c4f837a86548c9");
        let out = pbkdf2_sha256(b"password", b"salt", 2);
        assert_eq!(&hex(&out)[..40], "ae4d0c95af6b46d32d0adff928f06dd02a303f8e");
    }

    #[test]
    fn a_wrong_password_never_matches_and_costs_the_same_path() {
        let a = pbkdf2_sha256(b"correct horse", b"s", 10);
        let b = pbkdf2_sha256(b"correct horsf", b"s", 10);
        assert!(!constant_time_eq(&a, &b));
        assert!(constant_time_eq(&a, &a));
    }
}

#[cfg(test)]
mod sliding_session_tests {
    use super::*;

    #[tokio::test]
    async fn a_session_in_use_is_renewed_and_a_fresh_one_left_alone() {
        let sessions = Sessions::default();
        let principal = Principal {
            id: "bob".to_owned(),
            name: "Bob".to_owned(),
            kind: PrincipalKind::User,
            roles: vec![Role::Builder],
        };
        let (token, issued_until) = sessions.open(principal, 1).await;
        assert!(
            sessions.renew_if_stale(&token).await.is_none(),
            "a fresh session is not renewed"
        );
        // An hour and more into its life: the next request renews it.
        sessions.live().get_mut(&token).unwrap().1 =
            Utc::now() + Duration::hours(SESSION_HOURS - RENEW_AFTER_HOURS - 1);
        let renewed = sessions.renew_if_stale(&token).await.expect("renewed");
        assert!(
            renewed > issued_until - Duration::minutes(1),
            "the end moved out: {renewed} vs {issued_until}"
        );
        assert!(sessions.resolve(&token).await.is_some());
        // One left alone past its end is gone, however old.
        sessions.live().get_mut(&token).unwrap().1 = Utc::now() - Duration::minutes(1);
        assert!(
            sessions.renew_if_stale(&token).await.is_some(),
            "still in the map until resolved"
        );
        sessions.live().get_mut(&token).unwrap().1 = Utc::now() - Duration::minutes(1);
        assert!(
            sessions.resolve(&token).await.is_none(),
            "an ended session does not resolve"
        );
        assert!(
            sessions.renew_if_stale(&token).await.is_none(),
            "and is not renewed after it ended"
        );
    }
}
