use std::collections::BTreeSet;

use rusty_agent_runtime::skill::{DependencyDecl, SkillError, SkillPackage, MAX_DEPENDENCIES};
use rusty_agent_runtime::skills::{DependencyHygiene, DependencyIndex, FingerprintStatus};

fn skill_md_with_deps(dependencies: &str) -> String {
    format!(
        "---\nname: test-skill\ndescription: A test skill.\ndependencies: {dependencies}\n---\n\nBody text.\n"
    )
}

// ---------------------------------------------------------------------- //
// 1. Declaration parsing
// ---------------------------------------------------------------------- //

#[test]
fn parses_tool_dependency() {
    let md = skill_md_with_deps("tool:filesystem");
    let package = SkillPackage::from_markdown(&md).expect("valid package");
    assert_eq!(
        package.frontmatter().dependencies,
        vec![DependencyDecl::Tool {
            name: "filesystem".to_owned()
        }]
    );
}

#[test]
fn parses_connector_dependency() {
    let md = skill_md_with_deps("connector:ticketing");
    let package = SkillPackage::from_markdown(&md).expect("valid package");
    assert_eq!(
        package.frontmatter().dependencies,
        vec![DependencyDecl::Connector {
            id: "ticketing".to_owned()
        }]
    );
}

#[test]
fn parses_setting_dependency() {
    let md = skill_md_with_deps("setting:api.base_url");
    let package = SkillPackage::from_markdown(&md).expect("valid package");
    assert_eq!(
        package.frontmatter().dependencies,
        vec![DependencyDecl::Setting {
            path: "api.base_url".to_owned()
        }]
    );
}

#[test]
fn parses_multiple_dependencies() {
    let md = skill_md_with_deps("tool:filesystem, connector:ticketing, setting:api.base_url");
    let package = SkillPackage::from_markdown(&md).expect("valid package");
    assert_eq!(
        package.frontmatter().dependencies,
        vec![
            DependencyDecl::Tool {
                name: "filesystem".to_owned()
            },
            DependencyDecl::Connector {
                id: "ticketing".to_owned()
            },
            DependencyDecl::Setting {
                path: "api.base_url".to_owned()
            },
        ]
    );
}

#[test]
fn rejects_empty_dependency_entry() {
    let md = skill_md_with_deps("tool:filesystem, , connector:ticketing");
    let err = SkillPackage::from_markdown(&md).unwrap_err();
    assert!(matches!(err, SkillError::InvalidDependency { .. }));
}

#[test]
fn rejects_missing_kind_separator() {
    let md = skill_md_with_deps("filesystem");
    let err = SkillPackage::from_markdown(&md).unwrap_err();
    assert!(matches!(err, SkillError::InvalidDependency { .. }));
}

#[test]
fn rejects_empty_kind() {
    let md = skill_md_with_deps(":filesystem");
    let err = SkillPackage::from_markdown(&md).unwrap_err();
    assert!(matches!(err, SkillError::InvalidDependency { .. }));
}

#[test]
fn rejects_empty_name() {
    let md = skill_md_with_deps("tool:");
    let err = SkillPackage::from_markdown(&md).unwrap_err();
    assert!(matches!(err, SkillError::InvalidDependency { .. }));
}

#[test]
fn rejects_unknown_kind() {
    let md = skill_md_with_deps("widget:foo");
    let err = SkillPackage::from_markdown(&md).unwrap_err();
    assert!(matches!(err, SkillError::InvalidDependency { .. }));
}

#[test]
fn rejects_too_many_dependencies() {
    let deps: Vec<String> = (0..=MAX_DEPENDENCIES)
        .map(|i| format!("tool:tool-{i}"))
        .collect();
    let md = skill_md_with_deps(&deps.join(", "));
    let err = SkillPackage::from_markdown(&md).unwrap_err();
    assert!(matches!(err, SkillError::InvalidDependency { .. }));
}

#[test]
fn rejects_duplicate_dependencies_key() {
    let md = "---\nname: test-skill\ndescription: A test skill.\ndependencies: tool:a\ndependencies: tool:b\n---\n\nBody.\n";
    let err = SkillPackage::from_markdown(md).unwrap_err();
    assert!(matches!(err, SkillError::MalformedFrontmatter { .. }));
}

// ---------------------------------------------------------------------- //
// 2. Canonicalization stability
// ---------------------------------------------------------------------- //

#[test]
fn same_dependencies_same_hash() {
    let md = skill_md_with_deps("tool:filesystem, connector:ticketing");
    let a = SkillPackage::from_markdown(&md).expect("valid");
    let b = SkillPackage::from_markdown(&md).expect("valid");
    assert_eq!(a.content_hash(), b.content_hash());
}

#[test]
fn different_dependencies_different_hash() {
    let md_a = skill_md_with_deps("tool:filesystem");
    let md_b = skill_md_with_deps("tool:search");
    let a = SkillPackage::from_markdown(&md_a).expect("valid");
    let b = SkillPackage::from_markdown(&md_b).expect("valid");
    assert_ne!(a.content_hash(), b.content_hash());
}

#[test]
fn dependency_order_matters_in_hash() {
    let md_a = skill_md_with_deps("tool:a, tool:b");
    let md_b = skill_md_with_deps("tool:b, tool:a");
    let a = SkillPackage::from_markdown(&md_a).expect("valid");
    let b = SkillPackage::from_markdown(&md_b).expect("valid");
    assert_ne!(a.content_hash(), b.content_hash());
}

// ---------------------------------------------------------------------- //
// 3. Change detection
// ---------------------------------------------------------------------- //

#[test]
fn index_records_current_fingerprint() {
    let mut index = DependencyIndex::new();
    let decl = DependencyDecl::Tool {
        name: "filesystem".to_owned(),
    };
    let state = index.index_skill("my-skill", std::slice::from_ref(&decl), &|_| {
        Some(b"shape-v1".to_vec())
    });
    assert_eq!(state.fingerprints.len(), 1);
    assert_eq!(state.fingerprints[0].status, FingerprintStatus::Current);
    assert!(!state.fingerprints[0].fingerprint.is_empty());
}

#[test]
fn invalidate_marks_stale() {
    let mut index = DependencyIndex::new();
    let decl = DependencyDecl::Tool {
        name: "filesystem".to_owned(),
    };
    index.index_skill("my-skill", std::slice::from_ref(&decl), &|_| {
        Some(b"shape-v1".to_vec())
    });
    index.invalidate("tool:filesystem");
    let state = index.get("my-skill").expect("indexed");
    assert_eq!(state.fingerprints[0].status, FingerprintStatus::Stale);
}

#[test]
fn stale_query_returns_stale_entries() {
    let mut index = DependencyIndex::new();
    let decl = DependencyDecl::Tool {
        name: "filesystem".to_owned(),
    };
    index.index_skill("my-skill", std::slice::from_ref(&decl), &|_| {
        Some(b"shape-v1".to_vec())
    });
    index.invalidate("tool:filesystem");
    let stale = index.stale();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].0, "my-skill");
    assert_eq!(stale[0].1.status, FingerprintStatus::Stale);
}

// ---------------------------------------------------------------------- //
// 4. Secrecy redaction
// ---------------------------------------------------------------------- //

#[test]
fn fingerprint_never_sees_secrets() {
    let mut index = DependencyIndex::new();
    let decl = DependencyDecl::Connector {
        id: "slack".to_owned(),
    };
    // The caller redacts secrets before passing the shape.
    let state = index.index_skill("my-skill", &[decl], &|_| {
        Some(b"{\"host\":\"slack.com\"}".to_vec())
    });
    assert_eq!(
        state.fingerprints[0].fingerprint,
        rusty_agent_runtime::record::sha256_hex(b"{\"host\":\"slack.com\"}")
    );
}

// ---------------------------------------------------------------------- //
// 5. Reverse index
// ---------------------------------------------------------------------- //

#[test]
fn reverse_index_maps_dependency_to_skills() {
    let mut index = DependencyIndex::new();
    let decl = DependencyDecl::Tool {
        name: "filesystem".to_owned(),
    };
    index.index_skill("skill-a", std::slice::from_ref(&decl), &|_| {
        Some(b"shape".to_vec())
    });
    index.index_skill("skill-b", std::slice::from_ref(&decl), &|_| {
        Some(b"shape".to_vec())
    });
    let skills = index.skills_for("tool:filesystem").expect("reverse entry");
    assert!(skills.contains("skill-a"));
    assert!(skills.contains("skill-b"));
}

#[test]
fn reindexing_replaces_old_reverse_entries() {
    let mut index = DependencyIndex::new();
    let decl_a = DependencyDecl::Tool {
        name: "filesystem".to_owned(),
    };
    let decl_b = DependencyDecl::Tool {
        name: "search".to_owned(),
    };
    index.index_skill("my-skill", std::slice::from_ref(&decl_a), &|_| {
        Some(b"shape".to_vec())
    });
    index.index_skill("my-skill", std::slice::from_ref(&decl_b), &|_| {
        Some(b"shape".to_vec())
    });
    assert!(index.skills_for("tool:filesystem").is_none());
    let skills = index.skills_for("tool:search").expect("reverse entry");
    assert!(skills.contains("my-skill"));
}

// ---------------------------------------------------------------------- //
// 6. Hygiene flags
// ---------------------------------------------------------------------- //

#[test]
fn flags_undeclared_tool() {
    let index = DependencyIndex::new();
    let mut mentioned = BTreeSet::new();
    mentioned.insert("search".to_owned());
    let findings = index.hygiene("my-skill", &mentioned, &[]);
    assert_eq!(findings.len(), 1);
    assert!(matches!(
        &findings[0],
        DependencyHygiene::UndeclaredTool { tool } if tool == "search"
    ));
}

#[test]
fn flags_unused_declaration() {
    let index = DependencyIndex::new();
    let mentioned = BTreeSet::new();
    let decl = DependencyDecl::Tool {
        name: "filesystem".to_owned(),
    };
    let findings = index.hygiene("my-skill", &mentioned, &[decl]);
    assert_eq!(findings.len(), 1);
    assert!(matches!(
        &findings[0],
        DependencyHygiene::UnusedDeclaration { decl: DependencyDecl::Tool { name } } if name == "filesystem"
    ));
}

#[test]
fn clean_when_tools_match() {
    let index = DependencyIndex::new();
    let mut mentioned = BTreeSet::new();
    mentioned.insert("filesystem".to_owned());
    let decl = DependencyDecl::Tool {
        name: "filesystem".to_owned(),
    };
    let findings = index.hygiene("my-skill", &mentioned, &[decl]);
    assert!(findings.is_empty());
}

#[test]
fn non_tool_dependencies_ignored_for_hygiene() {
    let index = DependencyIndex::new();
    let mut mentioned = BTreeSet::new();
    mentioned.insert("filesystem".to_owned());
    let decls = vec![
        DependencyDecl::Tool {
            name: "filesystem".to_owned(),
        },
        DependencyDecl::Connector {
            id: "slack".to_owned(),
        },
    ];
    let findings = index.hygiene("my-skill", &mentioned, &decls);
    assert!(findings.is_empty());
}
