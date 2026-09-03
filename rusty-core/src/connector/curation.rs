//! Curation: select, filter, and classify imported connector operations.
//!
//! After OpenAPI import, Dev curates the generated operations: selects a
//! subset, assigns effect classes (defaulting GET → ReadOnly and everything
//! else → Idempotent, overridable only toward stricter), and names
//! operations that failed import so the report is complete.

use serde::{Deserialize, Serialize};

use super::manifest::{ConnectorOperation, OperationEffect};

/// The result of curating an import.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CuratedConnector {
    /// Selected operations with their assigned effect classes.
    pub operations: Vec<CuratedOperation>,
    /// Operations that were imported but excluded by curation.
    pub excluded: Vec<String>,
    /// Operations that could not be imported, carried forward for visibility.
    pub unmapped: Vec<super::openapi::UnmappedOperation>,
}

/// One operation after curation: the original operation plus the effect
/// class the curator assigned (which may override the default).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CuratedOperation {
    /// The underlying connector operation.
    #[serde(flatten)]
    pub operation: ConnectorOperation,
    /// The assigned effect class.
    pub assigned_effect: OperationEffect,
    /// Whether this operation was explicitly overridden (vs default).
    pub overridden: bool,
}

/// A curator selection rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CurationRule {
    /// Operation names to include (empty = include all imported).
    pub include: Vec<String>,
    /// Operation names to exclude (applied after include).
    pub exclude: Vec<String>,
    /// Per-operation effect overrides. Keys are operation names; values
    /// must be stricter than or equal to the default.
    pub effect_overrides: std::collections::BTreeMap<String, OperationEffect>,
}

/// Apply curation rules to an OpenAPI import result.
///
/// - Filters operations by `include` / `exclude`.
/// - Applies effect overrides, validating that each override is stricter
///   than or equal to the default.
/// - Carries unmapped operations forward for visibility.
pub fn curate(
    imported: super::openapi::OpenApiImport,
    rule: &CurationRule,
) -> Result<CuratedConnector, String> {
    let mut operations = Vec::new();
    let mut excluded = Vec::new();

    for op in imported.mapped {
        let name = op.name.clone();

        // Inclusion filter.
        if !rule.include.is_empty() && !rule.include.contains(&name) {
            excluded.push(name);
            continue;
        }

        // Exclusion filter.
        if rule.exclude.contains(&name) {
            excluded.push(name);
            continue;
        }

        // Determine effect: default or overridden.
        let default = op.effect;
        let (assigned_effect, overridden) = match rule.effect_overrides.get(&name) {
            Some(override_effect) => {
                if !is_stricter_or_equal(*override_effect, default) {
                    return Err(format!(
                        "effect override for `{name}` ({:?}) is weaker than the default ({:?})",
                        override_effect, default
                    ));
                }
                (*override_effect, true)
            }
            None => (default, false),
        };

        operations.push(CuratedOperation {
            operation: op,
            assigned_effect,
            overridden,
        });
    }

    Ok(CuratedConnector {
        operations,
        excluded,
        unmapped: imported.unmapped,
    })
}

/// `true` when `candidate` is stricter than or equal to `baseline`.
///
/// Strictness order (most permissive → most restrictive):
/// ReadOnly < Idempotent < Compensatable < Irreversible
fn is_stricter_or_equal(candidate: OperationEffect, baseline: OperationEffect) -> bool {
    use OperationEffect::*;
    match (candidate, baseline) {
        // Equal is always OK.
        (a, b) if a == b => true,
        // ReadOnly is the most permissive; it cannot override anything stricter.
        (ReadOnly, _) => false,
        // Idempotent can override ReadOnly but nothing stricter.
        (Idempotent, ReadOnly) => true,
        (Idempotent, _) => false,
        // Compensatable can override ReadOnly or Idempotent.
        (Compensatable, ReadOnly | Idempotent) => true,
        (Compensatable, _) => false,
        // Irreversible can override anything.
        (Irreversible, _) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_op(
        name: &str,
        method: super::super::manifest::HttpMethod,
        effect: OperationEffect,
    ) -> ConnectorOperation {
        ConnectorOperation {
            name: name.to_string(),
            description: format!("{name} desc"),
            method,
            path: "/test".to_string(),
            effect,
            params_schema: json!({"type":"object"}),
            headers: Vec::new(),
            auth: Vec::new(),
            max_response_bytes: None,
        }
    }

    #[test]
    fn curation_selects_subset() {
        let imported = super::super::openapi::OpenApiImport {
            mapped: vec![
                sample_op(
                    "list-users",
                    super::super::manifest::HttpMethod::Get,
                    OperationEffect::ReadOnly,
                ),
                sample_op(
                    "create-user",
                    super::super::manifest::HttpMethod::Post,
                    OperationEffect::Idempotent,
                ),
                sample_op(
                    "delete-user",
                    super::super::manifest::HttpMethod::Delete,
                    OperationEffect::Irreversible,
                ),
            ],
            unmapped: Vec::new(),
        };
        let rule = CurationRule {
            include: vec!["list-users".to_string(), "create-user".to_string()],
            exclude: Vec::new(),
            effect_overrides: std::collections::BTreeMap::new(),
        };
        let result = curate(imported, &rule).unwrap();
        assert_eq!(result.operations.len(), 2);
        assert_eq!(result.excluded, vec!["delete-user"]);
    }

    #[test]
    fn curation_excludes_after_include() {
        let imported = super::super::openapi::OpenApiImport {
            mapped: vec![
                sample_op(
                    "a",
                    super::super::manifest::HttpMethod::Get,
                    OperationEffect::ReadOnly,
                ),
                sample_op(
                    "b",
                    super::super::manifest::HttpMethod::Get,
                    OperationEffect::ReadOnly,
                ),
            ],
            unmapped: Vec::new(),
        };
        let rule = CurationRule {
            include: vec!["a".to_string(), "b".to_string()],
            exclude: vec!["b".to_string()],
            effect_overrides: std::collections::BTreeMap::new(),
        };
        let result = curate(imported, &rule).unwrap();
        assert_eq!(result.operations.len(), 1);
        assert_eq!(result.excluded, vec!["b"]);
    }

    #[test]
    fn curation_allows_stricter_override() {
        let imported = super::super::openapi::OpenApiImport {
            mapped: vec![sample_op(
                "list",
                super::super::manifest::HttpMethod::Get,
                OperationEffect::ReadOnly,
            )],
            unmapped: Vec::new(),
        };
        let mut overrides = std::collections::BTreeMap::new();
        overrides.insert("list".to_string(), OperationEffect::Idempotent);
        let rule = CurationRule {
            include: Vec::new(),
            exclude: Vec::new(),
            effect_overrides: overrides,
        };
        let result = curate(imported, &rule).unwrap();
        assert_eq!(
            result.operations[0].assigned_effect,
            OperationEffect::Idempotent
        );
        assert!(result.operations[0].overridden);
    }

    #[test]
    fn curation_rejects_weaker_override() {
        let imported = super::super::openapi::OpenApiImport {
            mapped: vec![sample_op(
                "delete",
                super::super::manifest::HttpMethod::Delete,
                OperationEffect::Irreversible,
            )],
            unmapped: Vec::new(),
        };
        let mut overrides = std::collections::BTreeMap::new();
        overrides.insert("delete".to_string(), OperationEffect::ReadOnly);
        let rule = CurationRule {
            include: Vec::new(),
            exclude: Vec::new(),
            effect_overrides: overrides,
        };
        let err = curate(imported, &rule).unwrap_err();
        assert!(err.contains("weaker than the default"));
    }

    #[test]
    fn stricter_or_equal_matrix() {
        use OperationEffect::*;
        assert!(is_stricter_or_equal(ReadOnly, ReadOnly));
        assert!(is_stricter_or_equal(Idempotent, ReadOnly));
        assert!(!is_stricter_or_equal(ReadOnly, Idempotent));
        assert!(is_stricter_or_equal(Irreversible, ReadOnly));
        assert!(is_stricter_or_equal(Irreversible, Compensatable));
        assert!(!is_stricter_or_equal(Compensatable, Irreversible));
    }
}
