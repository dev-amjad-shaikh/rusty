//! Org-level allowlists: Iris controls what may be installed.
//!
//! The allowlist gate sits before every install, update, and rollback.
//! In `curated` mode (the M4 enterprise default), only explicitly listed
//! packages may be installed. In `open` mode, any signed, non-revoked item
//! from a trusted registry proceeds. Revocation overrides every mode.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::error::{Result, RustyError};
use crate::package::{CapabilityDecl, PackageId, PackageKind, PackageManifest, PublisherId, Version};

// ---------------------------------------------------------------------------
// Error helper
// ---------------------------------------------------------------------------

fn _allowlist_err(msg: impl Into<String>) -> RustyError {
    RustyError::Plugin(format!("allowlist: {}", msg.into()))
}

// ---------------------------------------------------------------------------
// Policy mode
// ---------------------------------------------------------------------------

/// The organization-wide catalog policy mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    /// Any signed, non-revoked item from a trusted registry may install.
    Open,
    /// Only items matching the explicit allowlist may install (default).
    #[default]
    Curated,
}

// ---------------------------------------------------------------------------
// Capability constraints
// ---------------------------------------------------------------------------

/// A constraint on what capabilities an allowlisted entry permits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityConstraint {
    /// Only packages of this kind may match.
    Kind(PackageKind),
    /// Packages with no egress destinations.
    NoEgress,
    /// Packages with no secret references.
    NoSecretRefs,
}

// ---------------------------------------------------------------------------
// Allowlist entry
// ---------------------------------------------------------------------------

/// One entry in the org allowlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowlistEntry {
    /// Package id or wildcard (`*`).
    pub package_id_pattern: String,
    /// Optional publisher restriction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<PublisherId>,
    /// Version constraint string (e.g. `>=1.0.0, <2.0.0`).
    pub version_constraint: String,
    /// Optional capability constraints.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capability_constraints: Vec<CapabilityConstraint>,
}

impl AllowlistEntry {
    /// Whether this entry permits `manifest` at `version`.
    pub fn permits(&self, manifest: &PackageManifest, version: &Version) -> bool {
        // Package id pattern.
        if self.package_id_pattern != "*" && self.package_id_pattern != manifest.id.as_str() {
            return false;
        }

        // Publisher.
        if let Some(ref publisher) = self.publisher {
            if publisher.as_str() != manifest.publisher.as_str() {
                return false;
            }
        }

        // Version range.
        let range = crate::package::DependencyRange {
            id: manifest.id.as_str().to_string(),
            constraint: self.version_constraint.clone(),
        };
        if !range.satisfies(version) {
            return false;
        }

        // Capability constraints.
        for constraint in &self.capability_constraints {
            match constraint {
                CapabilityConstraint::Kind(kind) => {
                    if manifest.kind != *kind {
                        return false;
                    }
                }
                CapabilityConstraint::NoEgress => {
                    if !manifest.capabilities.egress.is_empty() {
                        return false;
                    }
                }
                CapabilityConstraint::NoSecretRefs => {
                    if !manifest.capabilities.secret_refs.is_empty() {
                        return false;
                    }
                }
            }
        }

        true
    }
}

// ---------------------------------------------------------------------------
// Org policy
// ---------------------------------------------------------------------------

/// The full org catalog policy.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OrgPolicy {
    pub mode: PolicyMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<AllowlistEntry>,
}

impl OrgPolicy {
    pub fn new(mode: PolicyMode, entries: Vec<AllowlistEntry>) -> Self {
        Self { mode, entries }
    }
}

// ---------------------------------------------------------------------------
// Allowlist checker
// ---------------------------------------------------------------------------

/// The outcome of an allowlist check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllowlistOutcome {
    /// The package is permitted.
    Permitted,
    /// The package is not allowlisted; an approval obligation is required.
    PendingApproval(ApprovalObligation),
    /// The package version is revoked; uninstallable in every mode.
    Revoked { reason: String },
}

/// An approval obligation created when an install is blocked by the allowlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalObligation {
    pub package_id: PackageId,
    pub package_name: String,
    pub publisher: PublisherId,
    pub version: Version,
    pub capabilities: CapabilityDecl,
    pub egress: Vec<String>,
    pub secret_refs: Vec<String>,
    pub eval_evidence_url: Option<String>,
}

/// The decision Iris makes on an approval obligation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// Approve this install exactly once.
    ApproveOnce {
        obligation_id: String,
        reason: String,
    },
    /// Approve this install and add a scoped allowlist entry.
    ApproveAndAllowlist {
        obligation_id: String,
        entry: AllowlistEntry,
        reason: String,
    },
    /// Reject the install with a reason.
    Reject {
        obligation_id: String,
        reason: String,
    },
}

/// A resolved approval record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub obligation_id: String,
    pub actor: String,
    pub decision: ApprovalDecision,
}

/// Checks packages against the org policy and revocation list.
pub struct AllowlistChecker {
    pub policy: OrgPolicy,
    pub revocations: HashMap<(PackageId, Version), String>,
}

impl AllowlistChecker {
    pub fn new(policy: OrgPolicy) -> Self {
        Self {
            policy,
            revocations: HashMap::new(),
        }
    }

    pub fn with_revocations(mut self, revocations: HashMap<(PackageId, Version), String>) -> Self {
        self.revocations = revocations;
        self
    }

    /// Check whether `manifest` at `version` may install.
    pub fn check(&self, manifest: &PackageManifest, version: &Version) -> AllowlistOutcome {
        // AC 6: revocation overrides every mode.
        if let Some(reason) = self.revocations.get(&(manifest.id.clone(), version.clone())) {
            return AllowlistOutcome::Revoked {
                reason: reason.clone(),
            };
        }

        match self.policy.mode {
            PolicyMode::Open => AllowlistOutcome::Permitted,
            PolicyMode::Curated => {
                let any_permits = self
                    .policy
                    .entries
                    .iter()
                    .any(|e| e.permits(manifest, version));
                if any_permits {
                    AllowlistOutcome::Permitted
                } else {
                    AllowlistOutcome::PendingApproval(ApprovalObligation {
                        package_id: manifest.id.clone(),
                        package_name: manifest.name.clone(),
                        publisher: manifest.publisher.clone(),
                        version: version.clone(),
                        capabilities: manifest.capabilities.clone(),
                        egress: manifest.capabilities.egress.iter().map(|e| e.host.clone()).collect(),
                        secret_refs: manifest
                            .capabilities
                            .secret_refs
                            .iter()
                            .map(|s| s.key.clone())
                            .collect(),
                        eval_evidence_url: None,
                    })
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Compliance flags (narrowing)
// ---------------------------------------------------------------------------

/// The status of an installed package after allowlist narrowing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComplianceStatus {
    Compliant,
    NonCompliant,
}

/// One package flagged by a narrowing check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComplianceFlag {
    pub package_id: PackageId,
    pub installed_version: Version,
    pub status: ComplianceStatus,
    pub reason: String,
}

/// Evaluate compliance of all installed packages against the current policy.
pub fn evaluate_compliance(
    policy: &OrgPolicy,
    installed: &[(PackageManifest, Version)],
) -> Vec<ComplianceFlag> {
    let checker = AllowlistChecker::new(policy.clone());
    installed
        .iter()
        .map(|(manifest, version)| {
            let outcome = checker.check(manifest, version);
            let (status, reason) = match outcome {
                AllowlistOutcome::Permitted => {
                    (ComplianceStatus::Compliant, "within allowlist".to_string())
                }
                AllowlistOutcome::PendingApproval(_) => (
                    ComplianceStatus::NonCompliant,
                    "no longer matches allowlist after narrowing".to_string(),
                ),
                AllowlistOutcome::Revoked { reason } => (
                    ComplianceStatus::NonCompliant,
                    format!("revoked: {}", reason),
                ),
            };
            ComplianceFlag {
                package_id: manifest.id.clone(),
                installed_version: version.clone(),
                status,
                reason,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Install-facing types (used by install.rs)
// ---------------------------------------------------------------------------

/// The result of checking an install request against the allowlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AllowlistCheckResult {
    /// The install may proceed.
    Allowed,
    /// The install is blocked with a reason.
    Blocked { reason: String },
    /// The install is pending approval.
    PendingApproval { obligation_id: String },
}

/// Store for approval obligations (persisted across the pause/resume cycle).
#[async_trait::async_trait]
pub trait ApprovalStore: Send + Sync {
    /// Create an approval obligation and return its id.
    async fn create_obligation(&self, obligation: ApprovalObligation) -> Result<String>;
}

impl AllowlistChecker {
    /// Check an install request, using the approval store when needed.
    pub async fn check_install(
        &self,
        _request: &crate::install::InstallRequest,
        manifest: &PackageManifest,
        revoked: Option<&str>,
        approval_store: &dyn ApprovalStore,
    ) -> Result<AllowlistCheckResult> {
        // Revocation check passed in as a pre-resolved string.
        if let Some(reason) = revoked {
            return Ok(AllowlistCheckResult::Blocked {
                reason: format!("revoked: {}", reason),
            });
        }

        let version = manifest.version.clone();
        match self.check(manifest, &version) {
            AllowlistOutcome::Permitted => Ok(AllowlistCheckResult::Allowed),
            AllowlistOutcome::Revoked { reason } => Ok(AllowlistCheckResult::Blocked {
                reason: format!("revoked: {}", reason),
            }),
            AllowlistOutcome::PendingApproval(obligation) => {
                let id = approval_store.create_obligation(obligation).await?;
                Ok(AllowlistCheckResult::PendingApproval { obligation_id: id })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn v(major: u64, minor: u64, patch: u64) -> Version {
        Version::new(major, minor, patch)
    }

    fn pkg_id(name: &str) -> PackageId {
        PackageId::new(name).unwrap()
    }

    fn publisher(name: &str) -> PublisherId {
        PublisherId::new(name).unwrap()
    }

    fn manifest_with_egress(id: &str, version: Version) -> PackageManifest {
        let mut cap = CapabilityDecl::default();
        cap.egress.push(crate::package::EgressDestDecl {
            host: "api.example.com".to_string(),
            methods: vec!["GET".to_string()],
            path_patterns: vec![],
        });
        PackageManifest::new(
            pkg_id(id),
            "Test",
            PackageKind::ToolPack,
            version,
            publisher("rusty-labs"),
            vec![],
            vec![],
            cap,
            None,
            None,
        )
        .unwrap()
    }

    fn manifest(id: &str, version: Version) -> PackageManifest {
        PackageManifest::new(
            pkg_id(id),
            "Test",
            PackageKind::ToolPack,
            version,
            publisher("rusty-labs"),
            vec![],
            vec![],
            CapabilityDecl::default(),
            None,
            None,
        )
        .unwrap()
    }

    // -----------------------------------------------------------------------
    // AC 1, 6: policy mode matrix
    // -----------------------------------------------------------------------

    #[test]
    fn open_mode_allows_anything() {
        let policy = OrgPolicy::new(PolicyMode::Open, vec![]);
        let checker = AllowlistChecker::new(policy);
        let m = manifest("any-pkg", v(1, 0, 0));
        assert_eq!(checker.check(&m, &v(1, 0, 0)), AllowlistOutcome::Permitted);
    }

    #[test]
    fn curated_mode_allows_listed() {
        let policy = OrgPolicy::new(
            PolicyMode::Curated,
            vec![AllowlistEntry {
                package_id_pattern: "listed-pkg".to_string(),
                publisher: None,
                version_constraint: ">=1.0.0".to_string(),
                capability_constraints: vec![],
            }],
        );
        let checker = AllowlistChecker::new(policy);
        let m = manifest("listed-pkg", v(1, 0, 0));
        assert_eq!(checker.check(&m, &v(1, 0, 0)), AllowlistOutcome::Permitted);
    }

    #[test]
    fn curated_mode_blocks_unlisted() {
        let policy = OrgPolicy::new(
            PolicyMode::Curated,
            vec![AllowlistEntry {
                package_id_pattern: "listed-pkg".to_string(),
                publisher: None,
                version_constraint: ">=1.0.0".to_string(),
                capability_constraints: vec![],
            }],
        );
        let checker = AllowlistChecker::new(policy);
        let m = manifest("other-pkg", v(1, 0, 0));
        let outcome = checker.check(&m, &v(1, 0, 0));
        assert!(
            matches!(outcome, AllowlistOutcome::PendingApproval(ref o) if o.package_id.as_str() == "other-pkg")
        );
    }

    #[test]
    fn wildcard_entry_matches_any_id() {
        let policy = OrgPolicy::new(
            PolicyMode::Curated,
            vec![AllowlistEntry {
                package_id_pattern: "*".to_string(),
                publisher: Some(publisher("rusty-labs")),
                version_constraint: ">=1.0.0".to_string(),
                capability_constraints: vec![],
            }],
        );
        let checker = AllowlistChecker::new(policy);
        let m = manifest("any-pkg", v(1, 0, 0));
        assert_eq!(checker.check(&m, &v(1, 0, 0)), AllowlistOutcome::Permitted);
    }

    #[test]
    fn publisher_mismatch_blocks() {
        let policy = OrgPolicy::new(
            PolicyMode::Curated,
            vec![AllowlistEntry {
                package_id_pattern: "*".to_string(),
                publisher: Some(publisher("other-pub")),
                version_constraint: ">=1.0.0".to_string(),
                capability_constraints: vec![],
            }],
        );
        let checker = AllowlistChecker::new(policy);
        let m = manifest("any-pkg", v(1, 0, 0));
        assert!(matches!(checker.check(&m, &v(1, 0, 0)), AllowlistOutcome::PendingApproval(_)));
    }

    #[test]
    fn version_range_enforced() {
        let policy = OrgPolicy::new(
            PolicyMode::Curated,
            vec![AllowlistEntry {
                package_id_pattern: "pkg".to_string(),
                publisher: None,
                version_constraint: ">=1.0.0, <2.0.0".to_string(),
                capability_constraints: vec![],
            }],
        );
        let checker = AllowlistChecker::new(policy);
        let m = manifest("pkg", v(2, 0, 0));
        assert!(matches!(checker.check(&m, &v(2, 0, 0)), AllowlistOutcome::PendingApproval(_)));
    }

    #[test]
    fn capability_constraint_no_egress() {
        let policy = OrgPolicy::new(
            PolicyMode::Curated,
            vec![AllowlistEntry {
                package_id_pattern: "*".to_string(),
                publisher: None,
                version_constraint: ">=1.0.0".to_string(),
                capability_constraints: vec![CapabilityConstraint::NoEgress],
            }],
        );
        let checker = AllowlistChecker::new(policy);
        let m_clean = manifest("pkg", v(1, 0, 0));
        let m_egress = manifest_with_egress("pkg", v(1, 0, 0));
        assert_eq!(checker.check(&m_clean, &v(1, 0, 0)), AllowlistOutcome::Permitted);
        assert!(matches!(checker.check(&m_egress, &v(1, 0, 0)), AllowlistOutcome::PendingApproval(_)));
    }

    // -----------------------------------------------------------------------
    // AC 6: revocation overrides every mode
    // -----------------------------------------------------------------------

    #[test]
    fn revocation_blocks_even_in_open_mode() {
        let policy = OrgPolicy::new(PolicyMode::Open, vec![]);
        let mut revocations = HashMap::new();
        revocations.insert((pkg_id("bad-pkg"), v(1, 0, 0)), "cve-2026-1234".to_string());
        let checker = AllowlistChecker::new(policy).with_revocations(revocations);
        let m = manifest("bad-pkg", v(1, 0, 0));
        let outcome = checker.check(&m, &v(1, 0, 0));
        assert!(matches!(outcome, AllowlistOutcome::Revoked { reason } if reason == "cve-2026-1234"));
    }

    #[test]
    fn revocation_blocks_even_when_allowlisted() {
        let policy = OrgPolicy::new(
            PolicyMode::Curated,
            vec![AllowlistEntry {
                package_id_pattern: "bad-pkg".to_string(),
                publisher: None,
                version_constraint: ">=1.0.0".to_string(),
                capability_constraints: vec![],
            }],
        );
        let mut revocations = HashMap::new();
        revocations.insert((pkg_id("bad-pkg"), v(1, 0, 0)), "cve-2026-1234".to_string());
        let checker = AllowlistChecker::new(policy).with_revocations(revocations);
        let m = manifest("bad-pkg", v(1, 0, 0));
        let outcome = checker.check(&m, &v(1, 0, 0));
        assert!(matches!(outcome, AllowlistOutcome::Revoked { .. }));
    }

    // -----------------------------------------------------------------------
    // AC 5: compliance flags on narrowing
    // -----------------------------------------------------------------------

    #[test]
    fn compliance_flags_non_compliant_after_narrowing() {
        let policy = OrgPolicy::new(
            PolicyMode::Curated,
            vec![AllowlistEntry {
                package_id_pattern: "kept-pkg".to_string(),
                publisher: None,
                version_constraint: ">=1.0.0".to_string(),
                capability_constraints: vec![],
            }],
        );
        let installed = vec![
            (manifest("kept-pkg", v(1, 0, 0)), v(1, 0, 0)),
            (manifest("dropped-pkg", v(1, 0, 0)), v(1, 0, 0)),
        ];
        let flags = evaluate_compliance(&policy, &installed);
        assert_eq!(flags[0].status, ComplianceStatus::Compliant);
        assert_eq!(flags[1].status, ComplianceStatus::NonCompliant);
    }

    // -----------------------------------------------------------------------
    // Approval obligation shape
    // -----------------------------------------------------------------------

    #[test]
    fn pending_includes_grant_summary() {
        let policy = OrgPolicy::new(PolicyMode::Curated, vec![]);
        let checker = AllowlistChecker::new(policy);
        let m = manifest_with_egress("svc", v(1, 0, 0));
        let outcome = checker.check(&m, &v(1, 0, 0));
        match outcome {
            AllowlistOutcome::PendingApproval(o) => {
                assert_eq!(o.package_id.as_str(), "svc");
                assert!(!o.egress.is_empty());
            }
            other => panic!("expected PendingApproval, got {:?}", other),
        }
    }
}
