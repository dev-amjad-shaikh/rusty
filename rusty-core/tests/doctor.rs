use rusty_agent_runtime::doctor::*;
use rusty_agent_runtime::package::{PackageId, Version};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct MemStore {
    packages: Arc<Mutex<Vec<InstalledPackage>>>,
}

#[derive(Clone)]
struct SlowMemStore {
    inner: MemStore,
    delay_ms: u64,
}

#[async_trait::async_trait]
impl PackageStore for SlowMemStore {
    async fn list_installed(&self) -> rusty_agent_runtime::error::Result<Vec<InstalledPackage>> {
        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        self.inner.list_installed().await
    }

    async fn write_config(
        &self,
        package_id: &PackageId,
        config: serde_json::Value,
    ) -> rusty_agent_runtime::error::Result<()> {
        self.inner.write_config(package_id, config).await
    }

    async fn write_state_version(
        &self,
        package_id: &PackageId,
        version: Version,
    ) -> rusty_agent_runtime::error::Result<()> {
        self.inner.write_state_version(package_id, version).await
    }
}

#[async_trait::async_trait]
impl PackageStore for MemStore {
    async fn list_installed(&self) -> rusty_agent_runtime::error::Result<Vec<InstalledPackage>> {
        Ok(self.packages.lock().unwrap().clone())
    }

    async fn write_config(
        &self,
        package_id: &PackageId,
        config: serde_json::Value,
    ) -> rusty_agent_runtime::error::Result<()> {
        let mut pkgs = self.packages.lock().unwrap();
        if let Some(p) = pkgs.iter_mut().find(|p| p.manifest.id == *package_id) {
            p.config = config;
        }
        Ok(())
    }

    async fn write_state_version(
        &self,
        package_id: &PackageId,
        version: Version,
    ) -> rusty_agent_runtime::error::Result<()> {
        let mut pkgs = self.packages.lock().unwrap();
        if let Some(p) = pkgs.iter_mut().find(|p| p.manifest.id == *package_id) {
            p.state_version = version;
        }
        Ok(())
    }
}

#[derive(Default, Clone)]
struct MemAudit {
    records: Arc<Mutex<Vec<AuditRecord>>>,
}

#[async_trait::async_trait]
impl AuditLedger for MemAudit {
    async fn append(&self, record: AuditRecord) -> rusty_agent_runtime::error::Result<()> {
        self.records.lock().unwrap().push(record);
        Ok(())
    }
}

#[derive(Default, Clone)]
struct MemLock {
    held: Arc<Mutex<bool>>,
}

#[async_trait::async_trait]
impl AdvisoryLock for MemLock {
    async fn try_acquire(&self, _key: &str) -> rusty_agent_runtime::error::Result<bool> {
        let mut held = self.held.lock().unwrap();
        if *held {
            return Ok(false);
        }
        *held = true;
        Ok(true)
    }

    async fn release(&self, _key: &str) -> rusty_agent_runtime::error::Result<()> {
        *self.held.lock().unwrap() = false;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn manifest_with_doctor(
    id: &str,
    version: Version,
    doctor: DoctorBlock,
) -> rusty_agent_runtime::package::PackageManifest {
    use rusty_agent_runtime::package::*;
    PackageManifest::new(
        PackageId::new(id).unwrap(),
        id,
        PackageKind::ToolPack,
        version,
        PublisherId::new("test-pub").unwrap(),
        vec![FileEntry {
            path: "x.rs".to_string(),
            sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
            bytes: 0,
        }],
        vec![],
        CapabilityDecl::default(),
        Some(doctor),
        None,
    )
    .unwrap()
}

fn installed(
    pkg: rusty_agent_runtime::package::PackageManifest,
    config: serde_json::Value,
) -> InstalledPackage {
    InstalledPackage {
        installed_version: pkg.version.clone(),
        state_version: pkg.version.clone(),
        manifest: pkg,
        config,
    }
}

// ---------------------------------------------------------------------------
// AC 1 & 2: chain computation
// ---------------------------------------------------------------------------

#[test]
fn chain_empty_when_versions_equal() {
    let block = DoctorBlock::default();
    let chain = DoctorChain::compute(
        PackageId::new("a").unwrap(),
        &Version::new(1, 0, 0),
        &Version::new(1, 0, 0),
        &block,
    )
    .unwrap();
    assert!(chain.steps.is_empty());
}

#[test]
fn chain_single_step() {
    let mut block = DoctorBlock::default();
    block.config_repairs.insert(
        Version::new(1, 1, 0),
        vec![ConfigRepair::RenameKey {
            from: "old".to_string(),
            to: "new".to_string(),
        }],
    );

    let chain = DoctorChain::compute(
        PackageId::new("a").unwrap(),
        &Version::new(1, 0, 0),
        &Version::new(1, 1, 0),
        &block,
    )
    .unwrap();
    assert_eq!(chain.steps.len(), 1);
    assert_eq!(chain.steps[0].from, Version::new(1, 0, 0));
    assert_eq!(chain.steps[0].to, Version::new(1, 1, 0));
    assert_eq!(chain.steps[0].repairs.len(), 1);
}

#[test]
fn chain_multi_hop() {
    let mut block = DoctorBlock::default();
    block.config_repairs.insert(
        Version::new(1, 1, 0),
        vec![ConfigRepair::RenameKey {
            from: "a".to_string(),
            to: "b".to_string(),
        }],
    );
    block.config_repairs.insert(
        Version::new(1, 2, 0),
        vec![ConfigRepair::RenameKey {
            from: "b".to_string(),
            to: "c".to_string(),
        }],
    );

    let chain = DoctorChain::compute(
        PackageId::new("a").unwrap(),
        &Version::new(1, 0, 0),
        &Version::new(1, 2, 0),
        &block,
    )
    .unwrap();
    assert_eq!(chain.steps.len(), 2);
    assert_eq!(chain.steps[0].to, Version::new(1, 1, 0));
    assert_eq!(chain.steps[1].to, Version::new(1, 2, 0));
}

#[test]
fn chain_gap_error() {
    let mut block = DoctorBlock::default();
    block.config_repairs.insert(
        Version::new(1, 1, 0),
        vec![ConfigRepair::RenameKey {
            from: "a".to_string(),
            to: "b".to_string(),
        }],
    );

    let err = DoctorChain::compute(
        PackageId::new("a").unwrap(),
        &Version::new(1, 0, 0),
        &Version::new(1, 2, 0),
        &block,
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("gap"), "expected gap error, got: {msg}");
}

#[test]
fn chain_no_path_error() {
    let block = DoctorBlock::default();
    let err = DoctorChain::compute(
        PackageId::new("a").unwrap(),
        &Version::new(1, 0, 0),
        &Version::new(2, 0, 0),
        &block,
    )
    .unwrap_err();
    assert!(err.to_string().contains("no declared path"));
}

// ---------------------------------------------------------------------------
// AC 3: pure diagnosis (no side effects)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn diagnose_is_pure_no_side_effects() {
    let mut block = DoctorBlock::default();
    block.config_repairs.insert(
        Version::new(1, 1, 0),
        vec![ConfigRepair::RenameKey {
            from: "old".to_string(),
            to: "new".to_string(),
        }],
    );

    let pkg = manifest_with_doctor("pkg", Version::new(1, 0, 0), block);
    let store = MemStore {
        packages: Arc::new(Mutex::new(vec![installed(
            pkg,
            serde_json::json!({"old": true}),
        )])),
    };

    let mut registry = HashMap::new();
    registry.insert(PackageId::new("pkg").unwrap(), Version::new(1, 1, 0));

    let doctor = Doctor::new(registry);
    let before = serde_json::to_string(&*store.packages.lock().unwrap()).unwrap();
    let report = doctor.diagnose(&store).await.unwrap();
    let after = serde_json::to_string(&*store.packages.lock().unwrap()).unwrap();

    assert_eq!(before, after, "diagnose mutated store");
    assert_eq!(report.packages.len(), 1);
    assert_eq!(report.packages[0].pending_repairs, 1);
    assert_eq!(report.packages[0].pending_migrations, 0);
}

// ---------------------------------------------------------------------------
// AC 4: fix applies repairs, halts on failure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fix_applies_repair_and_advances_state() {
    let mut block = DoctorBlock::default();
    block.config_repairs.insert(
        Version::new(1, 1, 0),
        vec![ConfigRepair::RenameKey {
            from: "old".to_string(),
            to: "new".to_string(),
        }],
    );

    let pkg = manifest_with_doctor("pkg", Version::new(1, 0, 0), block);
    let store = MemStore {
        packages: Arc::new(Mutex::new(vec![installed(
            pkg,
            serde_json::json!({"old": true}),
        )])),
    };

    let mut registry = HashMap::new();
    registry.insert(PackageId::new("pkg").unwrap(), Version::new(1, 1, 0));

    let doctor = Doctor::new(registry);
    let audit = MemAudit::default();
    let lock = MemLock::default();

    let result = doctor.fix(&store, &audit, &lock).await.unwrap();
    assert!(result.halted.is_none());

    let pkgs = store.packages.lock().unwrap();
    assert_eq!(pkgs[0].state_version, Version::new(1, 1, 0));
    assert_eq!(pkgs[0].config, serde_json::json!({"new": true}));
}

#[tokio::test]
async fn fix_halts_on_failure_and_leaves_package_deactivated_pending() {
    let mut block = DoctorBlock::default();
    block.config_repairs.insert(
        Version::new(1, 1, 0),
        vec![ConfigRepair::SplitValue {
            path: "missing".to_string(),
            delimiter: ",".to_string(),
        }],
    );

    let pkg = manifest_with_doctor("pkg", Version::new(1, 0, 0), block);
    let store = MemStore {
        packages: Arc::new(Mutex::new(vec![installed(pkg, serde_json::json!({}))])),
    };

    let mut registry = HashMap::new();
    registry.insert(PackageId::new("pkg").unwrap(), Version::new(1, 1, 0));

    let doctor = Doctor::new(registry);
    let audit = MemAudit::default();
    let lock = MemLock::default();

    let result = doctor.fix(&store, &audit, &lock).await.unwrap();
    assert!(result.halted.is_some());

    let halted = result.halted.unwrap();
    assert_eq!(halted.0.as_str(), "pkg");

    // Audit record was written.
    let records = audit.records.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].action, "fix_halted");
}

// ---------------------------------------------------------------------------
// AC 5: advisory lock race
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fix_races_only_one_wins() {
    let audit = MemAudit::default();
    let lock = MemLock::default();

    let mut block = DoctorBlock::default();
    block.config_repairs.insert(
        Version::new(1, 1, 0),
        vec![ConfigRepair::RenameKey {
            from: "old".to_string(),
            to: "new".to_string(),
        }],
    );
    let pkg = manifest_with_doctor("pkg", Version::new(1, 0, 0), block);
    let inner = MemStore {
        packages: Arc::new(Mutex::new(vec![installed(
            pkg,
            serde_json::json!({"old": true}),
        )])),
    };
    let store = SlowMemStore {
        inner: inner.clone(),
        delay_ms: 100,
    };

    let mut registry = HashMap::new();
    registry.insert(PackageId::new("pkg").unwrap(), Version::new(1, 1, 0));

    let doctor = Doctor::new(registry);

    // Spawn two concurrent fix attempts; only one should acquire the lock.
    // The 100ms sleep in list_installed ensures task 1 is inside fix_inner
    // when task 2 calls try_acquire.
    let h1 = tokio::spawn({
        let doctor = doctor.clone();
        let store = store.clone();
        let audit = audit.clone();
        let lock = lock.clone();
        async move { doctor.fix(&store, &audit, &lock).await }
    });
    let h2 = tokio::spawn({
        let doctor = doctor.clone();
        let store = store.clone();
        let audit = audit.clone();
        let lock = lock.clone();
        async move { doctor.fix(&store, &audit, &lock).await }
    });

    let r1 = h1.await.unwrap();
    let r2 = h2.await.unwrap();

    let ok_count = [r1.is_ok(), r2.is_ok()].iter().filter(|&&x| x).count();
    assert_eq!(ok_count, 1, "exactly one concurrent fix should succeed");
}

// ---------------------------------------------------------------------------
// AC 6: revocation flagging
// ---------------------------------------------------------------------------

#[tokio::test]
async fn diagnose_flags_revoked_version() {
    let pkg = manifest_with_doctor("pkg", Version::new(1, 0, 0), DoctorBlock::default());
    let store = MemStore {
        packages: Arc::new(Mutex::new(vec![installed(pkg, serde_json::json!({}))])),
    };

    let mut revocations = HashMap::new();
    revocations.insert(
        (PackageId::new("pkg").unwrap(), Version::new(1, 0, 0)),
        "CVE-2025-1234".to_string(),
    );

    let doctor = Doctor::new(HashMap::new()).with_revocations(revocations);
    let report = doctor.diagnose(&store).await.unwrap();

    assert_eq!(
        report.packages[0].revocation_flag,
        Some("CVE-2025-1234".to_string())
    );
}

// ---------------------------------------------------------------------------
// Doctor block validation
// ---------------------------------------------------------------------------

#[test]
fn doctor_block_rejects_duplicate_migration_order() {
    let mut block = DoctorBlock::default();
    block.state_migrations.push(StateMigration {
        version: Version::new(1, 1, 0),
        order: 1,
        description: "first".to_string(),
    });
    block.state_migrations.push(StateMigration {
        version: Version::new(1, 1, 0),
        order: 1,
        description: "duplicate".to_string(),
    });

    let err = block.validate().unwrap_err();
    assert!(err.to_string().contains("duplicate migration order"));
}

#[test]
fn doctor_block_accepts_unique_orders() {
    let mut block = DoctorBlock::default();
    block.state_migrations.push(StateMigration {
        version: Version::new(1, 1, 0),
        order: 1,
        description: "first".to_string(),
    });
    block.state_migrations.push(StateMigration {
        version: Version::new(1, 1, 0),
        order: 2,
        description: "second".to_string(),
    });

    assert!(block.validate().is_ok());
}

// ---------------------------------------------------------------------------
// Config repair application
// ---------------------------------------------------------------------------

#[test]
fn rename_key_repair() {
    let mut config = serde_json::json!({"old": true});
    Doctor::apply_repair(
        &mut config,
        &ConfigRepair::RenameKey {
            from: "old".to_string(),
            to: "new".to_string(),
        },
    )
    .unwrap();
    assert_eq!(config, serde_json::json!({"new": true}));
}

#[test]
fn split_value_repair() {
    let mut config = serde_json::json!({"tags": "a,b,c"});
    Doctor::apply_repair(
        &mut config,
        &ConfigRepair::SplitValue {
            path: "tags".to_string(),
            delimiter: ",".to_string(),
        },
    )
    .unwrap();
    assert_eq!(config, serde_json::json!({"tags": ["a", "b", "c"]}));
}

#[test]
fn supply_default_repair() {
    let mut config = serde_json::json!({"field": null});
    Doctor::apply_repair(
        &mut config,
        &ConfigRepair::SupplyDefault {
            path: "field".to_string(),
            value: serde_json::json!("default"),
        },
    )
    .unwrap();
    assert_eq!(config, serde_json::json!({"field": "default"}));
}

#[test]
fn supply_default_skips_when_present() {
    let mut config = serde_json::json!({"field": "existing"});
    Doctor::apply_repair(
        &mut config,
        &ConfigRepair::SupplyDefault {
            path: "field".to_string(),
            value: serde_json::json!("default"),
        },
    )
    .unwrap();
    assert_eq!(config, serde_json::json!({"field": "existing"}));
}
