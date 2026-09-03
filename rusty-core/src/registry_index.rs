//! The signed registry index: a deployment-fetched, trust-root-verified
//! enumeration of every catalog item available for install.
//!
//! The index is a signed document: the registry's Ed25519 keypair signs the
//! canonical JSON of the index content, and deployments verify against a
//! configured trust root. Tampered indexes are rejected before any package
//! metadata is trusted.
//!
//! Each index entry carries mirror-origin metadata so a deployment can consume
//! both the public registry and a private mirror without ambiguity.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::error::{Result, RustyError};
use crate::package::{
    CapabilityDecl, DependencyRange, PackageId, PackageKind, PublisherId, Version,
};

fn catalog_err(msg: impl Into<String>) -> RustyError {
    RustyError::Catalog(format!("registry: {}", msg.into()))
}

// ---------------------------------------------------------------------------
// Trust root and signature
// ---------------------------------------------------------------------------

/// The deployment's configured trust root for registry-index verification.
///
/// In production this is loaded from a config file or environment; in tests it
/// is constructed from a known verifying key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryTrustRoot {
    pub verifying_key_hex: String,
}

impl RegistryTrustRoot {
    pub fn new(verifying_key_hex: impl Into<String>) -> Self {
        Self {
            verifying_key_hex: verifying_key_hex.into(),
        }
    }

    /// Parse the hex verifying key into an Ed25519 [`ed25519_dalek::VerifyingKey`].
    fn _verifying_key(&self) -> Result<ed25519_dalek::VerifyingKey> {
        let bytes = crate::broker::hex_decode(&self.verifying_key_hex)
            .ok_or_else(|| catalog_err("trust root pubkey hex decode failed"))?;
        if bytes.len() != 32 {
            return Err(catalog_err(format!(
                "trust root pubkey must be 32 bytes, got {}",
                bytes.len()
            )));
        }
        let array: [u8; 32] = bytes.try_into().expect("length checked above");
        ed25519_dalek::VerifyingKey::from_bytes(&array)
            .map_err(|e| catalog_err(format!("invalid Ed25519 trust root pubkey: {e}")))
    }
}

/// A signature over the registry index, produced by the registry's signing key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexSignature {
    /// The signature bytes (Ed25519), hex-encoded.
    pub sig_hex: String,
    /// The registry's public key that produced the signature, hex-encoded.
    pub pubkey_hex: String,
}

impl IndexSignature {
    /// Verify the signature against the canonical JSON bytes of the index.
    pub fn verify(&self, index_bytes: &[u8]) -> Result<()> {
        use ed25519_dalek::Verifier;

        let pubkey_bytes = crate::broker::hex_decode(&self.pubkey_hex)
            .ok_or_else(|| catalog_err("index pubkey hex decode failed"))?;
        let sig_bytes = crate::broker::hex_decode(&self.sig_hex)
            .ok_or_else(|| catalog_err("index signature hex decode failed"))?;

        if pubkey_bytes.len() != 32 {
            return Err(catalog_err(format!(
                "index pubkey must be 32 bytes, got {}",
                pubkey_bytes.len()
            )));
        }
        if sig_bytes.len() != 64 {
            return Err(catalog_err(format!(
                "index signature must be 64 bytes, got {}",
                sig_bytes.len()
            )));
        }

        let pubkey: ed25519_dalek::VerifyingKey = ed25519_dalek::VerifyingKey::from_bytes(
            &pubkey_bytes.try_into().expect("length checked"),
        )
        .map_err(|e| catalog_err(format!("invalid Ed25519 pubkey: {e}")))?;
        let signature: ed25519_dalek::Signature =
            ed25519_dalek::Signature::from_bytes(&sig_bytes.try_into().expect("length checked"));

        pubkey
            .verify(index_bytes, &signature)
            .map_err(|e| catalog_err(format!("index signature verification failed: {e}")))?;
        Ok(())
    }

    /// Verify that the signature was produced by a key trusted by `root`.
    pub fn verify_against_trust_root(
        &self,
        index_bytes: &[u8],
        root: &RegistryTrustRoot,
    ) -> Result<()> {
        self.verify(index_bytes)?;
        if self.pubkey_hex != root.verifying_key_hex {
            return Err(catalog_err(format!(
                "index pubkey {} does not match trust root {}",
                self.pubkey_hex, root.verifying_key_hex
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Origin (public registry vs private mirror)
// ---------------------------------------------------------------------------

/// Where an index entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryOrigin {
    /// The public Rusty catalog registry.
    #[default]
    Public,
    /// A private mirror configured by the deployment.
    Mirror,
}

// ---------------------------------------------------------------------------
// Per-version metadata in the index
// ---------------------------------------------------------------------------

/// One version of a package as listed in the registry index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryVersion {
    /// The version string.
    pub version: Version,
    /// The content hash (SHA-256) of the package manifest.
    pub content_hash: String,
    /// The publisher's public key that signed this version, hex-encoded.
    pub publisher_pubkey_hex: String,
    /// Dependency metadata: package id → version range.
    pub dependencies: Vec<DependencyRange>,
    /// Declared capabilities for this version (for browsing and allowlist checks).
    #[serde(default)]
    pub capabilities: CapabilityDecl,
    /// Whether this version has been revoked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked: Option<String>,
    /// Pointer to eval evidence for this version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_evidence_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Per-package entry in the index
// ---------------------------------------------------------------------------

/// One package's metadata in the registry index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryEntry {
    /// The package identifier.
    pub id: PackageId,
    /// Human-readable name.
    pub name: String,
    /// The kind of catalog item.
    pub kind: PackageKind,
    /// The publisher identity.
    pub publisher: PublisherId,
    /// All known versions, newest first.
    pub versions: Vec<RegistryVersion>,
    /// URL to the package's documentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs_url: Option<String>,
    /// Origin of this entry in the merged index.
    #[serde(default)]
    pub origin: RegistryOrigin,
}

impl RegistryEntry {
    /// Find a version by exact version string.
    pub fn find_version(&self, version: &Version) -> Option<&RegistryVersion> {
        self.versions.iter().find(|v| &v.version == version)
    }

    /// The latest non-revoked version, if any.
    pub fn latest_safe_version(&self) -> Option<&RegistryVersion> {
        self.versions.iter().find(|v| v.revoked.is_none())
    }

    /// Whether the given version is revoked.
    pub fn is_revoked(&self, version: &Version) -> Option<&str> {
        self.find_version(version)
            .and_then(|v| v.revoked.as_deref())
    }
}

// ---------------------------------------------------------------------------
// The registry index
// ---------------------------------------------------------------------------

/// The signed registry index: a document enumerating all available packages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryIndex {
    /// Index format version (for evolution).
    pub format_version: u32,
    /// When the index was generated.
    pub generated_at: String,
    /// All entries, keyed by package id for fast lookup.
    pub entries: HashMap<String, RegistryEntry>,
    /// The registry's signature over this index.
    pub signature: IndexSignature,
}

impl RegistryIndex {
    /// Construct a new index. The signature must be supplied separately after
    /// canonical serialization.
    pub fn new(
        format_version: u32,
        generated_at: impl Into<String>,
        entries: Vec<RegistryEntry>,
    ) -> Self {
        let mut map = HashMap::new();
        for entry in entries {
            map.insert(entry.id.as_str().to_string(), entry);
        }
        Self {
            format_version,
            generated_at: generated_at.into(),
            entries: map,
            signature: IndexSignature {
                sig_hex: String::new(),
                pubkey_hex: String::new(),
            },
        }
    }

    /// The canonical JSON bytes used for signing and verification.
    ///
    /// Excludes the `signature` field so the signature can be verified over
    /// the content independently.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let content = RegistryIndexContent {
            format_version: self.format_version,
            generated_at: &self.generated_at,
            entries: &self.entries,
        };
        let value = serde_json::to_value(&content)
            .map_err(|e| catalog_err(format!("index serialization failed: {e}")))?;
        let canonical = crate::record::canonicalize_value(&value);
        serde_json::to_vec(&canonical)
            .map_err(|e| catalog_err(format!("index canonicalization failed: {e}")))
    }

    /// Verify the index signature against a trust root.
    pub fn verify(&self, trust_root: &RegistryTrustRoot) -> Result<()> {
        let bytes = self.canonical_bytes()?;
        self.signature.verify_against_trust_root(&bytes, trust_root)
    }

    /// Look up an entry by package id.
    pub fn get(&self, package_id: &PackageId) -> Option<&RegistryEntry> {
        self.entries.get(package_id.as_str())
    }

    /// All entries from a specific origin.
    pub fn by_origin(&self, origin: RegistryOrigin) -> Vec<&RegistryEntry> {
        self.entries
            .values()
            .filter(|e| e.origin == origin)
            .collect()
    }

    /// Merge another index into this one. Entries from `other` take precedence
    /// when their `origin` is `Mirror` (private overrides public).
    pub fn merge(&mut self, other: &RegistryIndex) {
        for (id, entry) in &other.entries {
            let should_replace = match self.entries.get(id) {
                Some(_existing) => entry.origin == RegistryOrigin::Mirror,
                None => true,
            };
            if should_replace {
                self.entries.insert(id.clone(), entry.clone());
            }
        }
    }
}

/// The content view of a registry index: every field except `signature`.
#[derive(Serialize)]
struct RegistryIndexContent<'a> {
    format_version: u32,
    generated_at: &'a str,
    entries: &'a HashMap<String, RegistryEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_keypair() -> (String, String) {
        use ed25519_dalek::SigningKey;
        use rand::RngCore;
        use rand::SeedableRng;
        use rand_chacha::ChaCha8Rng;

        let mut rng = ChaCha8Rng::from_seed([42u8; 32]);
        let secret: [u8; 32] = {
            let mut buf = [0u8; 32];
            rng.fill_bytes(&mut buf);
            buf
        };
        let signing_key = SigningKey::from_bytes(&secret);
        let verifying_key = signing_key.verifying_key();
        let pubkey_hex = crate::broker::hex_encode(verifying_key.as_bytes());
        let privkey_hex = crate::broker::hex_encode(&signing_key.to_bytes());
        (privkey_hex, pubkey_hex)
    }

    fn sign_index(index: &mut RegistryIndex, signing_key_hex: &str) {
        use ed25519_dalek::Signer;

        let bytes = index.canonical_bytes().unwrap();
        let key_bytes = crate::broker::hex_decode(signing_key_hex).unwrap();
        let signing_key: ed25519_dalek::SigningKey =
            ed25519_dalek::SigningKey::from_bytes(&key_bytes.try_into().unwrap());
        let signature = signing_key.sign(&bytes);
        index.signature = IndexSignature {
            sig_hex: crate::broker::hex_encode(&signature.to_bytes()),
            pubkey_hex: crate::broker::hex_encode(signing_key.verifying_key().as_bytes()),
        };
    }

    fn entry_fixture(id: &str, origin: RegistryOrigin) -> RegistryEntry {
        RegistryEntry {
            id: PackageId::new(id).unwrap(),
            name: id.to_string(),
            kind: PackageKind::ToolPack,
            publisher: PublisherId::new("rusty-labs").unwrap(),
            versions: vec![RegistryVersion {
                version: Version::new(1, 0, 0),
                content_hash: "a".repeat(64),
                publisher_pubkey_hex: "bb".repeat(32),
                dependencies: vec![],
                capabilities: CapabilityDecl::default(),
                revoked: None,
                eval_evidence_url: None,
            }],
            docs_url: None,
            origin,
        }
    }

    #[test]
    fn index_constructs_and_looks_up() {
        let entry = entry_fixture("test-pack", RegistryOrigin::Public);
        let index = RegistryIndex::new(1, "2026-01-01T00:00:00Z", vec![entry]);
        assert!(index.get(&PackageId::new("test-pack").unwrap()).is_some());
        assert!(index.get(&PackageId::new("missing").unwrap()).is_none());
    }

    #[test]
    fn index_verifies_against_trust_root() {
        let (privkey, pubkey) = fixture_keypair();
        let entry = entry_fixture("test-pack", RegistryOrigin::Public);
        let mut index = RegistryIndex::new(1, "2026-01-01T00:00:00Z", vec![entry]);
        sign_index(&mut index, &privkey);

        let root = RegistryTrustRoot::new(&pubkey);
        assert!(index.verify(&root).is_ok());
    }

    #[test]
    fn tampered_index_fails_verification() {
        let (privkey, pubkey) = fixture_keypair();
        let entry = entry_fixture("test-pack", RegistryOrigin::Public);
        let mut index = RegistryIndex::new(1, "2026-01-01T00:00:00Z", vec![entry]);
        sign_index(&mut index, &privkey);

        // Tamper: add a new entry after signing
        index.entries.insert(
            "evil".to_string(),
            entry_fixture("evil", RegistryOrigin::Public),
        );

        let root = RegistryTrustRoot::new(&pubkey);
        assert!(index.verify(&root).is_err());
    }

    #[test]
    fn wrong_trust_root_fails() {
        let (privkey, _pubkey) = fixture_keypair();
        let entry = entry_fixture("test-pack", RegistryOrigin::Public);
        let mut index = RegistryIndex::new(1, "2026-01-01T00:00:00Z", vec![entry]);
        sign_index(&mut index, &privkey);

        let wrong_root = RegistryTrustRoot::new("aa".repeat(32));
        assert!(index.verify(&wrong_root).is_err());
    }

    #[test]
    fn merge_prefers_mirror() {
        let public_entry = entry_fixture("test-pack", RegistryOrigin::Public);
        let mut public_index = RegistryIndex::new(1, "2026-01-01T00:00:00Z", vec![public_entry]);

        let mut mirror_entry = entry_fixture("test-pack", RegistryOrigin::Mirror);
        mirror_entry.name = "Overridden".to_string();
        let mirror_index = RegistryIndex::new(1, "2026-01-01T00:00:00Z", vec![mirror_entry]);

        public_index.merge(&mirror_index);
        let merged = public_index
            .get(&PackageId::new("test-pack").unwrap())
            .unwrap();
        assert_eq!(merged.name, "Overridden");
        assert_eq!(merged.origin, RegistryOrigin::Mirror);
    }

    #[test]
    fn by_origin_filters() {
        let public_entry = entry_fixture("public-pack", RegistryOrigin::Public);
        let mirror_entry = entry_fixture("mirror-pack", RegistryOrigin::Mirror);
        let index = RegistryIndex::new(1, "2026-01-01T00:00:00Z", vec![public_entry, mirror_entry]);
        assert_eq!(index.by_origin(RegistryOrigin::Public).len(), 1);
        assert_eq!(index.by_origin(RegistryOrigin::Mirror).len(), 1);
    }

    #[test]
    fn revoked_version_detected() {
        let mut entry = entry_fixture("test-pack", RegistryOrigin::Public);
        entry.versions.push(RegistryVersion {
            version: Version::new(1, 0, 1),
            content_hash: "b".repeat(64),
            publisher_pubkey_hex: "cc".repeat(32),
            dependencies: vec![],
            capabilities: CapabilityDecl::default(),
            revoked: Some("security advisory CVE-2026-0001".to_string()),
            eval_evidence_url: None,
        });
        assert!(entry.is_revoked(&Version::new(1, 0, 1)).is_some());
        assert!(entry.is_revoked(&Version::new(1, 0, 0)).is_none());
    }

    #[test]
    fn latest_safe_version_skips_revoked() {
        let mut entry = entry_fixture("test-pack", RegistryOrigin::Public);
        entry.versions.insert(
            0,
            RegistryVersion {
                version: Version::new(1, 0, 1),
                content_hash: "b".repeat(64),
                publisher_pubkey_hex: "cc".repeat(32),
                dependencies: vec![],
                capabilities: CapabilityDecl::default(),
                revoked: Some("bad".to_string()),
                eval_evidence_url: None,
            },
        );
        let safe = entry.latest_safe_version().unwrap();
        assert_eq!(safe.version, Version::new(1, 0, 0));
    }
}
