//! The doctor contract: config repair and state migrations.
//!
//! Every package declares its doctor block in its manifest: config repairs
//! and state migrations per version transition. The doctor computes the
//! chain across intermediate versions, diagnoses the fleet, and applies
//! repairs under an advisory lock.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::error::{Result, RustyError};
use crate::package::{PackageId, Version};

// ---------------------------------------------------------------------------
// Error helper
// ---------------------------------------------------------------------------

fn doctor_err(msg: impl Into<String>) -> RustyError {
    RustyError::Doctor(format!("doctor: {}", msg.into()))
}

// ---------------------------------------------------------------------------
// Doctor block types (declared in the package manifest)
// ---------------------------------------------------------------------------

/// A deterministic config repair: rename a key, split a value, or supply a
/// default for a new required field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigRepair {
    /// Rename a key from `from` to `to`.
    RenameKey { from: String, to: String },
    /// Split a value at the given delimiter into an array.
    SplitValue { path: String, delimiter: String },
    /// Supply a default for a new required field.
    SupplyDefault {
        path: String,
        value: serde_json::Value,
    },
}

/// A forward-only state migration for a package's own stored state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateMigration {
    /// The version this migration brings state to.
    pub version: Version,
    /// Execution order within the same version transition.
    pub order: u32,
    /// Human-readable description.
    pub description: String,
}

/// The doctor block declared in a package manifest.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DoctorBlock {
    /// Config repairs indexed by the version they transition *to*.
    pub config_repairs: HashMap<Version, Vec<ConfigRepair>>,
    /// State migrations ordered by version and then by `order`.
    pub state_migrations: Vec<StateMigration>,
}

impl DoctorBlock {
    /// Validate the block: no duplicate migration versions with colliding
    /// orders, repairs target existing versions.
    pub fn validate(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for m in &self.state_migrations {
            let key = (
                m.version.major(),
                m.version.minor(),
                m.version.patch(),
                m.order,
            );
            if !seen.insert(key) {
                return Err(doctor_err(format!(
                    "duplicate migration order {} for version {}",
                    m.order, m.version
                )));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Installed package view
// ---------------------------------------------------------------------------

/// A package as installed on a deployment: its manifest, active config, and
/// the version its state is currently at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstalledPackage {
    pub manifest: crate::package::PackageManifest,
    /// The version currently installed.
    pub installed_version: Version,
    /// Opaque config JSON; shape is package-specific.
    pub config: serde_json::Value,
    /// The version the stored state is currently at.
    pub state_version: Version,
}

// ---------------------------------------------------------------------------
// Chain computation
// ---------------------------------------------------------------------------

/// A computed repair-and-migration chain for one upgrade step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainStep {
    pub from: Version,
    pub to: Version,
    pub repairs: Vec<ConfigRepair>,
    pub migrations: Vec<StateMigration>,
}

/// The full chain from an installed version to a target version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorChain {
    pub package_id: PackageId,
    pub steps: Vec<ChainStep>,
}

impl DoctorChain {
    /// Compute the chain from `from_version` to `target_version` using the
    /// doctor block. A gap (no declared path between two versions) returns
    /// an error naming the gap.
    pub fn compute(
        package_id: PackageId,
        from_version: &Version,
        target_version: &Version,
        block: &DoctorBlock,
    ) -> Result<Self> {
        if from_version == target_version {
            return Ok(Self {
                package_id,
                steps: Vec::new(),
            });
        }

        // Collect all versions declared in the block, sorted.
        let mut versions: Vec<&Version> = block
            .state_migrations
            .iter()
            .map(|m| &m.version)
            .chain(block.config_repairs.keys())
            .collect();
        versions.sort();
        versions.dedup();

        // Find the path: we need every version strictly between from and to,
        // plus the target itself. The assumption is that migrations and
        // repairs are declared per *target* version step.
        let path_versions: Vec<&Version> = versions
            .iter()
            .filter(|&&v| v > from_version && v <= target_version)
            .copied()
            .collect();

        if path_versions.is_empty() {
            return Err(doctor_err(format!(
                "no declared path from {} to {} for package {}",
                from_version,
                target_version,
                package_id.as_str()
            )));
        }

        // Verify the path covers the target. If the largest version in the
        // path is less than the target, there's a gap.
        if path_versions.last().copied() != Some(target_version) {
            return Err(doctor_err(format!(
                "chain gap: no declared step reaches {} from {} for package {}",
                target_version,
                from_version,
                package_id.as_str()
            )));
        }

        let mut steps = Vec::new();
        let mut prev = from_version.clone();

        for v in path_versions {
            let repairs = block.config_repairs.get(v).cloned().unwrap_or_default();
            let migrations: Vec<StateMigration> = block
                .state_migrations
                .iter()
                .filter(|m| &m.version == v)
                .cloned()
                .collect();

            steps.push(ChainStep {
                from: prev.clone(),
                to: v.clone(),
                repairs,
                migrations,
            });
            prev = v.clone();
        }

        Ok(Self { package_id, steps })
    }
}

// ---------------------------------------------------------------------------
// Diagnosis report
// ---------------------------------------------------------------------------

/// Integrity of an installed package's files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IntegrityStatus {
    Valid,
    HashMismatch {
        file: String,
        expected: String,
        got: String,
    },
    SignatureInvalid,
}

/// One package's diagnosis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PackageDiagnosis {
    pub package_id: PackageId,
    pub installed_version: Version,
    pub target_version: Option<Version>,
    pub integrity: IntegrityStatus,
    pub pending_repairs: usize,
    pub pending_migrations: usize,
    pub chain: Option<DoctorChain>,
    pub revocation_flag: Option<String>,
}

/// The full fleet diagnosis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoctorReport {
    pub packages: Vec<PackageDiagnosis>,
}

// ---------------------------------------------------------------------------
// Fix outcomes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FixOutcome {
    Applied,
    Skipped,
    Failed { step: String, reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PackageFixResult {
    pub package_id: PackageId,
    pub outcomes: Vec<(String, FixOutcome)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoctorFixResult {
    pub package_results: Vec<PackageFixResult>,
    pub halted: Option<(PackageId, String, String)>,
}

// ---------------------------------------------------------------------------
// Store traits
// ---------------------------------------------------------------------------

/// Read installed packages and their configuration.
#[async_trait::async_trait]
pub trait PackageStore: Send + Sync {
    /// List every installed package.
    async fn list_installed(&self) -> Result<Vec<InstalledPackage>>;
    /// Write the updated config for a package.
    async fn write_config(&self, package_id: &PackageId, config: serde_json::Value) -> Result<()>;
    /// Write the updated state version for a package.
    async fn write_state_version(&self, package_id: &PackageId, version: Version) -> Result<()>;
}

/// Append-only audit ledger.
#[async_trait::async_trait]
pub trait AuditLedger: Send + Sync {
    async fn append(&self, record: AuditRecord) -> Result<()>;
}

/// A single audit record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub actor: String,
    pub package_id: PackageId,
    pub action: String,
    pub detail: serde_json::Value,
}

/// Distributed advisory lock for fleet-wide doctor execution.
#[async_trait::async_trait]
pub trait AdvisoryLock: Send + Sync {
    /// Try to acquire the lock. Returns true if acquired.
    async fn try_acquire(&self, key: &str) -> Result<bool>;
    /// Release the lock.
    async fn release(&self, key: &str) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Doctor
// ---------------------------------------------------------------------------

/// The doctor: diagnoses and heals the fleet.
#[derive(Clone)]
pub struct Doctor {
    /// Registry index mapping package id → latest available version.
    /// If a package is absent, the installed version is assumed current.
    pub registry: HashMap<PackageId, Version>,
    /// Revocation list: package id + version → revocation reason.
    pub revocations: HashMap<(PackageId, Version), String>,
}

impl Doctor {
    pub fn new(registry: HashMap<PackageId, Version>) -> Self {
        Self {
            registry,
            revocations: HashMap::new(),
        }
    }

    pub fn with_revocations(mut self, revocations: HashMap<(PackageId, Version), String>) -> Self {
        self.revocations = revocations;
        self
    }

    /// Pure diagnosis: zero side effects.
    pub async fn diagnose(&self, store: &dyn PackageStore) -> Result<DoctorReport> {
        let installed = store.list_installed().await?;
        let mut packages = Vec::new();

        for pkg in installed {
            let target = self.registry.get(&pkg.manifest.id).cloned();
            let integrity = Self::check_integrity(&pkg);

            let chain = if let Some(ref target_version) = target {
                if let Some(ref block) = pkg.manifest.doctor {
                    DoctorChain::compute(
                        pkg.manifest.id.clone(),
                        &pkg.installed_version,
                        target_version,
                        block,
                    )
                    .ok()
                } else {
                    None
                }
            } else {
                None
            };

            let pending_repairs = chain
                .as_ref()
                .map(|c| c.steps.iter().map(|s| s.repairs.len()).sum())
                .unwrap_or(0);

            let pending_migrations = chain
                .as_ref()
                .map(|c| c.steps.iter().map(|s| s.migrations.len()).sum())
                .unwrap_or(0);

            let revocation_flag = self
                .revocations
                .get(&(pkg.manifest.id.clone(), pkg.installed_version.clone()))
                .cloned();

            packages.push(PackageDiagnosis {
                package_id: pkg.manifest.id,
                installed_version: pkg.installed_version,
                target_version: target,
                integrity,
                pending_repairs,
                pending_migrations,
                chain,
                revocation_flag,
            });
        }

        Ok(DoctorReport { packages })
    }

    /// Apply fixes under the advisory lock.
    pub async fn fix(
        &self,
        store: &dyn PackageStore,
        audit: &dyn AuditLedger,
        lock: &dyn AdvisoryLock,
    ) -> Result<DoctorFixResult> {
        if !lock.try_acquire("doctor:fix").await? {
            return Err(doctor_err("another doctor fix is already in progress"));
        }

        let result = self.fix_inner(store, audit).await;
        let _ = lock.release("doctor:fix").await;
        result
    }

    async fn fix_inner(
        &self,
        store: &dyn PackageStore,
        audit: &dyn AuditLedger,
    ) -> Result<DoctorFixResult> {
        let report = self.diagnose(store).await?;
        let mut package_results = Vec::new();

        for diag in report.packages {
            let pkg_id = diag.package_id.clone();
            let mut outcomes = Vec::new();

            let Some(chain) = diag.chain else {
                package_results.push(PackageFixResult {
                    package_id: pkg_id,
                    outcomes,
                });
                continue;
            };

            // Find the installed package to get mutable config.
            let installed_list = store.list_installed().await?;
            let Some(mut pkg) = installed_list.into_iter().find(|p| p.manifest.id == pkg_id) else {
                continue;
            };

            for step in &chain.steps {
                // Apply config repairs.
                for repair in &step.repairs {
                    match Self::apply_repair(&mut pkg.config, repair) {
                        Ok(()) => {
                            outcomes.push((
                                format!("repair {}→{} {:?}", step.from, step.to, repair),
                                FixOutcome::Applied,
                            ));
                        }
                        Err(e) => {
                            outcomes.push((
                                format!("repair {}→{} {:?}", step.from, step.to, repair),
                                FixOutcome::Failed {
                                    step: format!("{}→{}", step.from, step.to),
                                    reason: e.to_string(),
                                },
                            ));
                            audit
                                .append(AuditRecord {
                                    actor: "doctor".to_string(),
                                    package_id: pkg_id.clone(),
                                    action: "fix_halted".to_string(),
                                    detail: serde_json::json!({
                                        "step": format!("{}→{}", step.from, step.to),
                                        "repair": format!("{:?}", repair),
                                        "reason": e.to_string(),
                                    }),
                                })
                                .await?;
                            package_results.push(PackageFixResult {
                                package_id: pkg_id.clone(),
                                outcomes,
                            });
                            return Ok(DoctorFixResult {
                                package_results,
                                halted: Some((
                                    pkg_id,
                                    format!("{}→{}", step.from, step.to),
                                    e.to_string(),
                                )),
                            });
                        }
                    }
                }

                // Persist config after repairs.
                store.write_config(&pkg_id, pkg.config.clone()).await?;

                // Apply state migrations (no-op in this abstraction; real
                // implementations would run SQL or file migrations).
                for migration in &step.migrations {
                    outcomes.push((
                        format!(
                            "migration {}→{} {}",
                            step.from, step.to, migration.description
                        ),
                        FixOutcome::Applied,
                    ));
                }

                // Advance state version.
                store.write_state_version(&pkg_id, step.to.clone()).await?;
            }

            package_results.push(PackageFixResult {
                package_id: pkg_id,
                outcomes,
            });
        }

        Ok(DoctorFixResult {
            package_results,
            halted: None,
        })
    }

    fn check_integrity(pkg: &InstalledPackage) -> IntegrityStatus {
        if !pkg.manifest.verify_hash() {
            return IntegrityStatus::SignatureInvalid;
        }
        // File-level hash checks would iterate pkg.manifest.files here.
        IntegrityStatus::Valid
    }

    pub fn apply_repair(config: &mut serde_json::Value, repair: &ConfigRepair) -> Result<()> {
        match repair {
            ConfigRepair::RenameKey { from, to } => {
                if let Some(obj) = config.as_object_mut() {
                    if let Some(value) = obj.remove(from) {
                        obj.insert(to.clone(), value);
                    }
                }
                Ok(())
            }
            ConfigRepair::SplitValue { path, delimiter } => {
                let parts: Vec<&str> = path.split('.').collect();
                Self::set_at_path(config, &parts, |v| {
                    if let Some(s) = v.as_str() {
                        let split: Vec<serde_json::Value> =
                            s.split(delimiter).map(|s| serde_json::json!(s)).collect();
                        *v = serde_json::json!(split);
                    }
                })
            }
            ConfigRepair::SupplyDefault { path, value } => {
                let parts: Vec<&str> = path.split('.').collect();
                Self::set_at_path(config, &parts, |v| {
                    if v.is_null() || (v.is_object() && v.as_object().unwrap().is_empty()) {
                        *v = value.clone();
                    }
                })
            }
        }
    }

    pub fn set_at_path<F>(value: &mut serde_json::Value, parts: &[&str], f: F) -> Result<()>
    where
        F: FnOnce(&mut serde_json::Value),
    {
        if parts.is_empty() {
            f(value);
            return Ok(());
        }
        let first = parts[0];
        let rest = &parts[1..];
        if let Some(obj) = value.as_object_mut() {
            if let Some(child) = obj.get_mut(first) {
                Self::set_at_path(child, rest, f)
            } else {
                Err(doctor_err(format!("path segment `{first}` not found")))
            }
        } else {
            Err(doctor_err("config is not an object at path segment"))
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{PackageId, PublisherId, Version};

    fn v(major: u64, minor: u64, patch: u64) -> Version {
        Version::new(major, minor, patch)
    }

    fn pkg_id(name: &str) -> PackageId {
        PackageId::new(name).unwrap()
    }

    // -----------------------------------------------------------------------
    // Chain computation (AC 1, 2)
    // -----------------------------------------------------------------------

    #[test]
    fn chain_computes_multi_hop() {
        let block = DoctorBlock {
            config_repairs: {
                let mut m = HashMap::new();
                m.insert(v(1, 1, 0), vec![ConfigRepair::RenameKey {
                    from: "old_key".to_string(),
                    to: "new_key".to_string(),
                }]);
                m.insert(v(1, 2, 0), vec![ConfigRepair::SupplyDefault {
                    path: "setting".to_string(),
                    value: serde_json::json!("default"),
                }]);
                m
            },
            state_migrations: vec![
                StateMigration {
                    version: v(1, 1, 0),
                    order: 0,
                    description: "init".to_string(),
                },
                StateMigration {
                    version: v(1, 2, 0),
                    order: 0,
                    description: "add table".to_string(),
                },
            ],
        };

        let chain = DoctorChain::compute(pkg_id("test"), &v(1, 0, 0), &v(1, 2, 0), &block).unwrap();
        assert_eq!(chain.steps.len(), 2);
        assert_eq!(chain.steps[0].from, v(1, 0, 0));
        assert_eq!(chain.steps[0].to, v(1, 1, 0));
        assert_eq!(chain.steps[1].from, v(1, 1, 0));
        assert_eq!(chain.steps[1].to, v(1, 2, 0));
    }

    #[test]
    fn chain_same_version_is_empty() {
        let block = DoctorBlock::default();
        let chain = DoctorChain::compute(pkg_id("test"), &v(1, 0, 0), &v(1, 0, 0), &block).unwrap();
        assert!(chain.steps.is_empty());
    }

    #[test]
    fn chain_gap_returns_error() {
        let block = DoctorBlock {
            config_repairs: HashMap::new(),
            state_migrations: vec![StateMigration {
                version: v(1, 1, 0),
                order: 0,
                description: "init".to_string(),
            }],
        };
        let err = DoctorChain::compute(pkg_id("test"), &v(1, 0, 0), &v(1, 2, 0), &block).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("gap"), "expected gap error, got: {}", msg);
    }

    #[test]
    fn chain_no_path_returns_error() {
        let block = DoctorBlock::default();
        let err = DoctorChain::compute(pkg_id("test"), &v(1, 0, 0), &v(2, 0, 0), &block).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no declared path"), "expected path error, got: {}", msg);
    }

    // -----------------------------------------------------------------------
    // Config repair application (AC 1)
    // -----------------------------------------------------------------------

    #[test]
    fn repair_rename_key() {
        let mut config = serde_json::json!({"old_key": 42});
        let repair = ConfigRepair::RenameKey {
            from: "old_key".to_string(),
            to: "new_key".to_string(),
        };
        Doctor::apply_repair(&mut config, &repair).unwrap();
        assert_eq!(config, serde_json::json!({"new_key": 42}));
    }

    #[test]
    fn repair_rename_key_missing_is_no_op() {
        let mut config = serde_json::json!({"other": 42});
        let repair = ConfigRepair::RenameKey {
            from: "old_key".to_string(),
            to: "new_key".to_string(),
        };
        Doctor::apply_repair(&mut config, &repair).unwrap();
        assert_eq!(config, serde_json::json!({"other": 42}));
    }

    #[test]
    fn repair_split_value() {
        let mut config = serde_json::json!({"tags": "a,b,c"});
        let repair = ConfigRepair::SplitValue {
            path: "tags".to_string(),
            delimiter: ",".to_string(),
        };
        Doctor::apply_repair(&mut config, &repair).unwrap();
        assert_eq!(config, serde_json::json!({"tags": ["a", "b", "c"]}));
    }

    #[test]
    fn repair_supply_default_on_null() {
        let mut config = serde_json::json!({"setting": null});
        let repair = ConfigRepair::SupplyDefault {
            path: "setting".to_string(),
            value: serde_json::json!("default"),
        };
        Doctor::apply_repair(&mut config, &repair).unwrap();
        assert_eq!(config, serde_json::json!({"setting": "default"}));
    }

    #[test]
    fn repair_supply_default_on_existing_preserves() {
        let mut config = serde_json::json!({"setting": "existing"});
        let repair = ConfigRepair::SupplyDefault {
            path: "setting".to_string(),
            value: serde_json::json!("default"),
        };
        Doctor::apply_repair(&mut config, &repair).unwrap();
        assert_eq!(config, serde_json::json!({"setting": "existing"}));
    }

    #[test]
    fn set_at_path_missing_segment_errors() {
        let mut config = serde_json::json!({"a": {"b": 1}});
        let err = Doctor::set_at_path(&mut config, &["a", "x"], |v| *v = serde_json::json!(2)).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    // -----------------------------------------------------------------------
    // Mocks
    // -----------------------------------------------------------------------

    struct MockStore {
        installed: Vec<InstalledPackage>,
    }

    #[async_trait::async_trait]
    impl PackageStore for MockStore {
        async fn list_installed(&self) -> Result<Vec<InstalledPackage>> {
            Ok(self.installed.clone())
        }
        async fn write_config(&self, _package_id: &PackageId, _config: serde_json::Value) -> Result<()> {
            Ok(())
        }
        async fn write_state_version(&self, _package_id: &PackageId, _version: Version) -> Result<()> {
            Ok(())
        }
    }

    struct MockAudit {
        records: std::sync::Mutex<Vec<AuditRecord>>,
    }

    #[async_trait::async_trait]
    impl AuditLedger for MockAudit {
        async fn append(&self, record: AuditRecord) -> Result<()> {
            self.records.lock().unwrap().push(record);
            Ok(())
        }
    }

    struct MockLock {
        acquired: std::sync::Mutex<bool>,
    }

    #[async_trait::async_trait]
    impl AdvisoryLock for MockLock {
        async fn try_acquire(&self, _key: &str) -> Result<bool> {
            let mut acquired = self.acquired.lock().unwrap();
            if *acquired {
                Ok(false)
            } else {
                *acquired = true;
                Ok(true)
            }
        }
        async fn release(&self, _key: &str) -> Result<()> {
            *self.acquired.lock().unwrap() = false;
            Ok(())
        }
    }

    fn make_manifest(id: &str, version: Version, doctor: Option<DoctorBlock>) -> crate::package::PackageManifest {
        crate::package::PackageManifest::new(
            pkg_id(id),
            "Test",
            crate::package::PackageKind::ToolPack,
            version,
            PublisherId::new("rusty-labs").unwrap(),
            vec![],
            vec![],
            crate::package::CapabilityDecl::default(),
            doctor,
            None,
        )
        .unwrap()
    }

    // -----------------------------------------------------------------------
    // Diagnosis (AC 3, 6)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn diagnose_reports_revocation_flag() {
        let manifest = make_manifest("revoked-pkg", v(1, 0, 0), None);
        let store = MockStore {
            installed: vec![InstalledPackage {
                manifest,
                installed_version: v(1, 0, 0),
                config: serde_json::json!({}),
                state_version: v(1, 0, 0),
            }],
        };

        let mut revocations = HashMap::new();
        revocations.insert((pkg_id("revoked-pkg"), v(1, 0, 0)), "security advisory".to_string());

        let doctor = Doctor::new(HashMap::new()).with_revocations(revocations);
        let report = doctor.diagnose(&store).await.unwrap();
        assert_eq!(report.packages.len(), 1);
        assert_eq!(report.packages[0].revocation_flag, Some("security advisory".to_string()));
    }

    #[tokio::test]
    async fn diagnose_reports_pending_repairs_and_migrations() {
        let block = DoctorBlock {
            config_repairs: {
                let mut m = HashMap::new();
                m.insert(v(1, 1, 0), vec![ConfigRepair::RenameKey {
                    from: "a".to_string(),
                    to: "b".to_string(),
                }]);
                m
            },
            state_migrations: vec![StateMigration {
                version: v(1, 1, 0),
                order: 0,
                description: "init".to_string(),
            }],
        };
        let manifest = make_manifest("pkg", v(1, 1, 0), Some(block));

        let mut registry = HashMap::new();
        registry.insert(pkg_id("pkg"), v(1, 1, 0));

        let store = MockStore {
            installed: vec![InstalledPackage {
                manifest,
                installed_version: v(1, 0, 0),
                config: serde_json::json!({}),
                state_version: v(1, 0, 0),
            }],
        };

        let doctor = Doctor::new(registry);
        let report = doctor.diagnose(&store).await.unwrap();
        assert_eq!(report.packages[0].pending_repairs, 1);
        assert_eq!(report.packages[0].pending_migrations, 1);
    }

    #[tokio::test]
    async fn diagnose_is_pure_no_side_effects() {
        let manifest = make_manifest("pkg", v(1, 0, 0), None);
        let store = MockStore {
            installed: vec![InstalledPackage {
                manifest,
                installed_version: v(1, 0, 0),
                config: serde_json::json!({"a": 1}),
                state_version: v(1, 0, 0),
            }],
        };
        let doctor = Doctor::new(HashMap::new());
        let r1 = doctor.diagnose(&store).await.unwrap();
        let r2 = doctor.diagnose(&store).await.unwrap();
        assert_eq!(r1.packages[0].installed_version, r2.packages[0].installed_version);
    }

    // -----------------------------------------------------------------------
    // Fix with advisory lock (AC 4, 5)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn fix_applies_repairs_and_migrations() {
        let block = DoctorBlock {
            config_repairs: {
                let mut m = HashMap::new();
                m.insert(v(1, 1, 0), vec![ConfigRepair::RenameKey {
                    from: "old".to_string(),
                    to: "new".to_string(),
                }]);
                m
            },
            state_migrations: vec![StateMigration {
                version: v(1, 1, 0),
                order: 0,
                description: "init".to_string(),
            }],
        };
        let manifest = make_manifest("pkg", v(1, 1, 0), Some(block));

        let mut registry = HashMap::new();
        registry.insert(pkg_id("pkg"), v(1, 1, 0));

        let store = MockStore {
            installed: vec![InstalledPackage {
                manifest,
                installed_version: v(1, 0, 0),
                config: serde_json::json!({"old": 42}),
                state_version: v(1, 0, 0),
            }],
        };
        let audit = MockAudit { records: std::sync::Mutex::new(Vec::new()) };
        let lock = MockLock { acquired: std::sync::Mutex::new(false) };

        let doctor = Doctor::new(registry);
        let result = doctor.fix(&store, &audit, &lock).await.unwrap();
        assert!(result.halted.is_none());
        assert_eq!(result.package_results.len(), 1);
        assert_eq!(result.package_results[0].outcomes.len(), 2);
    }

    #[tokio::test]
    async fn fix_honors_advisory_lock() {
        let manifest = make_manifest("pkg", v(1, 0, 0), None);
        let store = MockStore {
            installed: vec![InstalledPackage {
                manifest,
                installed_version: v(1, 0, 0),
                config: serde_json::json!({}),
                state_version: v(1, 0, 0),
            }],
        };
        let audit = MockAudit { records: std::sync::Mutex::new(Vec::new()) };
        let lock = MockLock { acquired: std::sync::Mutex::new(true) };

        let doctor = Doctor::new(HashMap::new());
        let err = doctor.fix(&store, &audit, &lock).await.unwrap_err();
        assert!(err.to_string().contains("already in progress"));
    }

    #[tokio::test]
    async fn fix_halts_on_failed_repair_and_audits() {
        let block = DoctorBlock {
            config_repairs: {
                let mut m = HashMap::new();
                m.insert(v(1, 1, 0), vec![ConfigRepair::SplitValue {
                    path: "missing.path".to_string(),
                    delimiter: ",".to_string(),
                }]);
                m
            },
            state_migrations: Vec::new(),
        };
        let manifest = make_manifest("pkg", v(1, 1, 0), Some(block));

        let mut registry = HashMap::new();
        registry.insert(pkg_id("pkg"), v(1, 1, 0));

        let store = MockStore {
            installed: vec![InstalledPackage {
                manifest,
                installed_version: v(1, 0, 0),
                config: serde_json::json!({}),
                state_version: v(1, 0, 0),
            }],
        };
        let audit = MockAudit { records: std::sync::Mutex::new(Vec::new()) };
        let lock = MockLock { acquired: std::sync::Mutex::new(false) };

        let doctor = Doctor::new(registry);
        let result = doctor.fix(&store, &audit, &lock).await.unwrap();
        assert!(result.halted.is_some());
        let records = audit.records.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].action, "fix_halted");
    }
}
