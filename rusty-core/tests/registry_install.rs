use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rusty_agent_runtime::doctor::{DoctorBlock, InstalledPackage};
use rusty_agent_runtime::error::Result;
use rusty_agent_runtime::allowlist::{
    AllowlistChecker, MemoryApprovalStore, OrgPolicy, PolicyMode,
};
use rusty_agent_runtime::install::{
    scope_grants, BlobStore, CapabilityRegistrar, CatalogAuditLedger, CatalogAuditRecord,
    CatalogScope, EvalRunner, IdempotencyKey, InstallOutcome, InstallRequest, InstallStep,
    InstallStore, Installer, RollbackOutcome, RollbackRequest, UpdateOutcome, UpdateRequest,
};
use rusty_agent_runtime::package::{PackageId, PackageKind, PackageManifest, PublisherId, Version};
use rusty_agent_runtime::registry_index::{
    IndexSignature, RegistryEntry, RegistryIndex, RegistryOrigin, RegistryVersion,
};

// ---------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct InMemoryInstallStore {
    installed: Arc<Mutex<HashMap<String, InstalledPackage>>>,
    outcomes: Arc<Mutex<HashMap<String, InstallOutcome>>>,
}

#[async_trait::async_trait]
impl InstallStore for InMemoryInstallStore {
    async fn record_outcome(&self, key: &IdempotencyKey, outcome: &InstallOutcome) -> Result<()> {
        self.outcomes
            .lock()
            .unwrap()
            .insert(key.0.clone(), outcome.clone());
        Ok(())
    }

    async fn get_outcome(&self, key: &IdempotencyKey) -> Result<Option<InstallOutcome>> {
        Ok(self.outcomes.lock().unwrap().get(&key.0).cloned())
    }

    async fn put_installed(&self, package: InstalledPackage) -> Result<()> {
        self.installed
            .lock()
            .unwrap()
            .insert(package.manifest.id.as_str().to_string(), package);
        Ok(())
    }

    async fn remove_installed(&self, package_id: &PackageId) -> Result<()> {
        self.installed.lock().unwrap().remove(package_id.as_str());
        Ok(())
    }

    async fn list_installed(&self) -> Result<Vec<InstalledPackage>> {
        Ok(self.installed.lock().unwrap().values().cloned().collect())
    }
}

#[derive(Default, Clone)]
struct InMemoryAudit {
    records: Arc<Mutex<Vec<CatalogAuditRecord>>>,
}

#[async_trait::async_trait]
impl CatalogAuditLedger for InMemoryAudit {
    async fn append(&self, record: CatalogAuditRecord) -> Result<()> {
        self.records.lock().unwrap().push(record);
        Ok(())
    }
}

#[derive(Default, Clone)]
struct NoopBlobStore;

#[async_trait::async_trait]
impl BlobStore for NoopBlobStore {
    async fn put(&self, _bytes: Vec<u8>) -> Result<String> {
        Ok("deadbeef".to_string())
    }

    async fn get(&self, _hash: &str) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

#[derive(Default, Clone)]
struct NoopRegistrar {
    registered: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl CapabilityRegistrar for NoopRegistrar {
    async fn register(&self, manifest: &PackageManifest) -> Result<()> {
        self.registered
            .lock()
            .unwrap()
            .push(manifest.id.as_str().to_string());
        Ok(())
    }

    async fn unregister(&self, package_id: &PackageId) -> Result<()> {
        self.registered
            .lock()
            .unwrap()
            .retain(|id| id != package_id.as_str());
        Ok(())
    }
}

#[derive(Default, Clone)]
struct NoopEval;

#[async_trait::async_trait]
impl EvalRunner for NoopEval {
    async fn run(&self, _manifest: &PackageManifest) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn fixture_keypair() -> (String, String) {
    use ed25519_dalek::SigningKey;
    use rand::RngCore;
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;

    let mut rng = ChaCha8Rng::from_seed([42u8; 32]);
    let mut secret = [0u8; 32];
    rng.fill_bytes(&mut secret);
    let signing_key = SigningKey::from_bytes(&secret);
    let verifying_key = signing_key.verifying_key();
    let pubkey_hex = rusty_agent_runtime::broker::hex_encode(verifying_key.as_bytes());
    let privkey_hex = rusty_agent_runtime::broker::hex_encode(&signing_key.to_bytes());
    (privkey_hex, pubkey_hex)
}

fn sign_index(index: &mut RegistryIndex, signing_key_hex: &str) {
    use ed25519_dalek::Signer;

    let bytes = index.canonical_bytes().unwrap();
    let key_bytes = rusty_agent_runtime::broker::hex_decode(signing_key_hex).unwrap();
    let signing_key: ed25519_dalek::SigningKey =
        ed25519_dalek::SigningKey::from_bytes(&key_bytes.try_into().unwrap());
    let signature = signing_key.sign(&bytes);
    index.signature = IndexSignature {
        sig_hex: rusty_agent_runtime::broker::hex_encode(&signature.to_bytes()),
        pubkey_hex: rusty_agent_runtime::broker::hex_encode(signing_key.verifying_key().as_bytes()),
    };
}

fn make_index_with_entry(privkey: &str, pubkey: &str) -> RegistryIndex {
    let entry = RegistryEntry {
        id: PackageId::new("test-pack").unwrap(),
        name: "Test Pack".to_string(),
        kind: PackageKind::ToolPack,
        publisher: PublisherId::new("rusty-labs").unwrap(),
        versions: vec![RegistryVersion {
            version: Version::new(1, 0, 0),
            content_hash: "a".repeat(64),
            publisher_pubkey_hex: pubkey.to_string(),
            dependencies: vec![],
            capabilities: Default::default(),
            revoked: None,
            eval_evidence_url: None,
        }],
        docs_url: None,
        origin: RegistryOrigin::Public,
    };
    let mut index = RegistryIndex::new(1, "2026-01-01T00:00:00Z", vec![entry]);
    sign_index(&mut index, privkey);
    index
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------


fn open_checker() -> AllowlistChecker {
    AllowlistChecker::new(OrgPolicy::new(PolicyMode::Open, vec![]))
}
#[tokio::test]
async fn install_success() {
    let (privkey, pubkey) = fixture_keypair();
    let index = make_index_with_entry(&privkey, &pubkey);
    let installer = Installer::new(index);

    let request = InstallRequest {
        package_id: PackageId::new("test-pack").unwrap(),
        version: Version::new(1, 0, 0),
        idempotency_key: IdempotencyKey::new("key-1"),
        actor: "maya".to_string(),
    };

    let store = InMemoryInstallStore::default();
    let audit = InMemoryAudit::default();
    let blobs = NoopBlobStore;
    let registrar = NoopRegistrar::default();
    let eval = NoopEval;

    let checker = open_checker();
    let approval_store = MemoryApprovalStore::default();
    let outcome = installer
        .install(
            &request,
            &["catalog:install".to_string()],
            &store,
            &audit,
            &blobs,
            &registrar,
            &eval,
            &checker,
            &approval_store,
        )
        .await
        .unwrap();

    match outcome {
        InstallOutcome::Installed {
            package_id,
            version,
            steps,
        } => {
            assert_eq!(package_id.as_str(), "test-pack");
            assert_eq!(version, Version::new(1, 0, 0));
            assert_eq!(steps.len(), 8);
            assert!(steps.iter().any(|s| s.step == InstallStep::Activate));
        }
        other => panic!("expected Installed, got {:?}", other),
    }

    // Audit log has 8 entries (one per step).
    assert_eq!(audit.records.lock().unwrap().len(), 8);

    // Package is in the installed store.
    let installed = store.list_installed().await.unwrap();
    assert_eq!(installed.len(), 1);
}

#[tokio::test]
async fn install_idempotent() {
    let (privkey, pubkey) = fixture_keypair();
    let index = make_index_with_entry(&privkey, &pubkey);
    let installer = Installer::new(index);

    let request = InstallRequest {
        package_id: PackageId::new("test-pack").unwrap(),
        version: Version::new(1, 0, 0),
        idempotency_key: IdempotencyKey::new("key-1"),
        actor: "maya".to_string(),
    };

    let store = InMemoryInstallStore::default();
    let audit = InMemoryAudit::default();
    let blobs = NoopBlobStore;
    let registrar = NoopRegistrar::default();
    let eval = NoopEval;

    let checker = open_checker();
    let approval_store = MemoryApprovalStore::default();
    let first = installer
        .install(
            &request,
            &["catalog:install".to_string()],
            &store,
            &audit,
            &blobs,
            &registrar,
            &eval,
            &checker,
            &approval_store,
        )
        .await
        .unwrap();

    let second = installer
        .install(
            &request,
            &["catalog:install".to_string()],
            &store,
            &audit,
            &blobs,
            &registrar,
            &eval,
            &checker,
            &approval_store,
        )
        .await
        .unwrap();

    assert_eq!(format!("{:?}", first), format!("{:?}", second));

    // Only one package installed (no double-install).
    let installed = store.list_installed().await.unwrap();
    assert_eq!(installed.len(), 1);
}

#[tokio::test]
async fn install_scope_denied() {
    let (privkey, pubkey) = fixture_keypair();
    let index = make_index_with_entry(&privkey, &pubkey);
    let installer = Installer::new(index);

    let request = InstallRequest {
        package_id: PackageId::new("test-pack").unwrap(),
        version: Version::new(1, 0, 0),
        idempotency_key: IdempotencyKey::new("key-1"),
        actor: "maya".to_string(),
    };

    let store = InMemoryInstallStore::default();
    let audit = InMemoryAudit::default();
    let blobs = NoopBlobStore;
    let registrar = NoopRegistrar::default();
    let eval = NoopEval;

    let checker = open_checker();
    let approval_store = MemoryApprovalStore::default();
    let result = installer
        .install(
            &request,
            &["catalog:read".to_string()],
            &store,
            &audit,
            &blobs,
            &registrar,
            &eval,
            &checker,
            &approval_store,
        )
        .await;

    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("scope"));
}

#[tokio::test]
async fn install_revoked_version_blocked() {
    let (privkey, pubkey) = fixture_keypair();
    let mut index = make_index_with_entry(&privkey, &pubkey);

    // Revoke the version.
    index.entries.get_mut("test-pack").unwrap().versions[0].revoked =
        Some("CVE-2026-0001".to_string());
    sign_index(&mut index, &privkey);

    let installer = Installer::new(index);
    let request = InstallRequest {
        package_id: PackageId::new("test-pack").unwrap(),
        version: Version::new(1, 0, 0),
        idempotency_key: IdempotencyKey::new("key-1"),
        actor: "maya".to_string(),
    };

    let store = InMemoryInstallStore::default();
    let audit = InMemoryAudit::default();
    let blobs = NoopBlobStore;
    let registrar = NoopRegistrar::default();
    let eval = NoopEval;

    let checker = open_checker();
    let approval_store = MemoryApprovalStore::default();
    let result = installer
        .install(
            &request,
            &["catalog:install".to_string()],
            &store,
            &audit,
            &blobs,
            &registrar,
            &eval,
            &checker,
            &approval_store,
        )
        .await;

    match result {
        Ok(InstallOutcome::Failed { reason, .. }) => {
            assert!(reason.contains("revoked"));
        }
        other => panic!(
            "expected Failed outcome with revoked reason, got {:?}",
            other
        ),
    }
}

#[tokio::test]
async fn update_runs_doctor_chain() {
    let (privkey, pubkey) = fixture_keypair();
    let index = make_index_with_entry(&privkey, &pubkey);
    let installer = Installer::new(index);

    // Pre-install version 1.0.0 with a doctor block.
    let manifest = PackageManifest::new(
        PackageId::new("test-pack").unwrap(),
        "Test Pack",
        PackageKind::ToolPack,
        Version::new(1, 0, 0),
        PublisherId::new("rusty-labs").unwrap(),
        vec![],
        vec![],
        Default::default(),
        Some(DoctorBlock::default()),
        None,
    )
    .unwrap();

    let store = InMemoryInstallStore::default();
    store
        .put_installed(InstalledPackage {
            manifest: manifest.clone(),
            installed_version: Version::new(1, 0, 0),
            config: serde_json::json!({}),
            state_version: Version::new(1, 0, 0),
        })
        .await
        .unwrap();

    let audit = InMemoryAudit::default();
    let blobs = NoopBlobStore;
    let registrar = NoopRegistrar::default();
    let eval = NoopEval;

    let update_req = UpdateRequest {
        package_id: PackageId::new("test-pack").unwrap(),
        target_version: Version::new(1, 0, 0), // same version = idempotent
        idempotency_key: IdempotencyKey::new("update-1"),
        actor: "maya".to_string(),
    };

    let checker = open_checker();
    let approval_store = MemoryApprovalStore::default();
    let outcome = installer
        .update(
            &update_req,
            &["catalog:install".to_string()],
            &store,
            &audit,
            &blobs,
            &registrar,
            &eval,
            &checker,
            &approval_store,
        )
        .await
        .unwrap();

    match outcome {
        UpdateOutcome::Updated {
            package_id,
            from_version,
            to_version,
            ..
        } => {
            assert_eq!(package_id.as_str(), "test-pack");
            assert_eq!(from_version, Version::new(1, 0, 0));
            assert_eq!(to_version, Version::new(1, 0, 0));
        }
        other => panic!("expected Updated, got {:?}", other),
    }
}

#[tokio::test]
async fn rollback_refused_when_migrations_exist() {
    let (privkey, pubkey) = fixture_keypair();
    let index = make_index_with_entry(&privkey, &pubkey);
    let installer = Installer::new(index);

    // Install version 2.0.0 with a forward migration.
    use rusty_agent_runtime::doctor::StateMigration;
    let mut block = DoctorBlock::default();
    block.state_migrations.push(StateMigration {
        version: Version::new(2, 0, 0),
        order: 1,
        description: "add table".to_string(),
    });

    let manifest = PackageManifest::new(
        PackageId::new("test-pack").unwrap(),
        "Test Pack",
        PackageKind::ToolPack,
        Version::new(2, 0, 0),
        PublisherId::new("rusty-labs").unwrap(),
        vec![],
        vec![],
        Default::default(),
        Some(block),
        None,
    )
    .unwrap();

    let store = InMemoryInstallStore::default();
    store
        .put_installed(InstalledPackage {
            manifest: manifest.clone(),
            installed_version: Version::new(2, 0, 0),
            config: serde_json::json!({}),
            state_version: Version::new(2, 0, 0),
        })
        .await
        .unwrap();

    let audit = InMemoryAudit::default();
    let registrar = NoopRegistrar::default();

    let rollback_req = RollbackRequest {
        package_id: PackageId::new("test-pack").unwrap(),
        target_version: Version::new(1, 0, 0),
        idempotency_key: IdempotencyKey::new("rollback-1"),
        actor: "maya".to_string(),
    };

    let outcome = installer
        .rollback(
            &rollback_req,
            &["catalog:install".to_string()],
            &store,
            &audit,
            &registrar,
        )
        .await
        .unwrap();

    match outcome {
        RollbackOutcome::Refused { reason, .. } => {
            assert!(reason.contains("rollback refused"));
            assert!(reason.contains("restore from backup"));
        }
        other => panic!("expected Refused, got {:?}", other),
    }
}

#[tokio::test]
async fn rollback_success_when_no_migrations() {
    let (privkey, pubkey) = fixture_keypair();
    let index = make_index_with_entry(&privkey, &pubkey);
    let installer = Installer::new(index);

    let manifest = PackageManifest::new(
        PackageId::new("test-pack").unwrap(),
        "Test Pack",
        PackageKind::ToolPack,
        Version::new(2, 0, 0),
        PublisherId::new("rusty-labs").unwrap(),
        vec![],
        vec![],
        Default::default(),
        Some(DoctorBlock::default()),
        None,
    )
    .unwrap();

    let store = InMemoryInstallStore::default();
    store
        .put_installed(InstalledPackage {
            manifest: manifest.clone(),
            installed_version: Version::new(2, 0, 0),
            config: serde_json::json!({"key": "value"}),
            state_version: Version::new(2, 0, 0),
        })
        .await
        .unwrap();

    let audit = InMemoryAudit::default();
    let registrar = NoopRegistrar::default();

    let rollback_req = RollbackRequest {
        package_id: PackageId::new("test-pack").unwrap(),
        target_version: Version::new(1, 0, 0),
        idempotency_key: IdempotencyKey::new("rollback-1"),
        actor: "maya".to_string(),
    };

    let outcome = installer
        .rollback(
            &rollback_req,
            &["catalog:install".to_string()],
            &store,
            &audit,
            &registrar,
        )
        .await
        .unwrap();

    match outcome {
        RollbackOutcome::RolledBack {
            from_version,
            to_version,
            ..
        } => {
            assert_eq!(from_version, Version::new(2, 0, 0));
            assert_eq!(to_version, Version::new(1, 0, 0));
        }
        other => panic!("expected RolledBack, got {:?}", other),
    }

    // Installed version is now 1.0.0.
    let installed = store.list_installed().await.unwrap();
    assert_eq!(installed[0].installed_version, Version::new(1, 0, 0));
}

#[test]
fn scope_grants_wildcard() {
    assert!(scope_grants(
        &["catalog:*".to_string()],
        &CatalogScope::install("catalog")
    ));
    assert!(scope_grants(
        &["*:install".to_string()],
        &CatalogScope::install("catalog")
    ));
    assert!(!scope_grants(
        &["catalog:read".to_string()],
        &CatalogScope::install("catalog")
    ));
}
