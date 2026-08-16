//! The skill-plane suite: package parsing, progressive disclosure,
//! provenance, the security scan, immutable versioning, and registry
//! determinism — one pass over the whole contract.

use std::collections::BTreeMap;
use std::sync::Arc;

use rusty_agent_runtime::skill::{
    scan_package, ScanKind, ScanSeverity, SkillError, SkillPackage, SkillRegistry, SkillSource,
    SkillVersionSelector, BASE64_BLOB_MIN_CHARS, MAX_SKILL_BODY_BYTES, MAX_SKILL_DESCRIPTION_BYTES,
    MAX_SKILL_FILE_BYTES, MAX_SKILL_PACKAGE_BYTES,
};

/// A minimal valid `SKILL.md` for `name`.
fn skill_md(name: &str, description: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n")
}

/// A valid package with one reference and one asset.
fn full_package(name: &str, body: &str) -> SkillPackage {
    let mut files = BTreeMap::new();
    files.insert(
        "SKILL.md".to_owned(),
        skill_md(name, &format!("The {name} skill."), body).into_bytes(),
    );
    files.insert(
        "references/guide.md".to_owned(),
        b"# Guide\n\nDetails on demand.\n".to_vec(),
    );
    files.insert("assets/logo.bin".to_owned(), vec![0x89, 0x50, 0x4e, 0x47]);
    SkillPackage::from_files(files).expect("valid package")
}

fn local_source() -> SkillSource {
    SkillSource::LocalPath {
        path: "/skills/test".to_owned(),
    }
}

// --------------------------------------------------------------------- //
// Package round-trip
// --------------------------------------------------------------------- //

#[test]
fn valid_package_round_trips_through_the_registry() {
    let package = full_package("web-research", "Search, then summarize.");
    assert_eq!(package.name(), "web-research");
    assert_eq!(package.description(), "The web-research skill.");
    assert_eq!(package.body(), "Search, then summarize.\n");
    assert_eq!(
        package.references().keys().collect::<Vec<_>>(),
        ["references/guide.md"]
    );
    assert_eq!(
        package.assets().keys().collect::<Vec<_>>(),
        ["assets/logo.bin"]
    );

    let expected_hash = package.content_hash();
    let mut registry = SkillRegistry::new();
    let registration = registry
        .register(package, local_source(), "operator:ada")
        .expect("registration succeeds");
    assert!(!registration.already_registered);

    let version = registration.version;
    assert_eq!(version.revision(), 1);
    assert_eq!(version.content_hash(), expected_hash);
    assert_eq!(version.body(), "Search, then summarize.\n");
    assert_eq!(version.provenance().author, "operator:ada");
    assert_eq!(version.provenance().source, local_source());
    assert_eq!(version.provenance().content_hash, expected_hash);
    assert!(version.scan().is_clean());
    assert_eq!(
        version.reference("references/guide.md").unwrap(),
        b"# Guide\n\nDetails on demand.\n"
    );
    assert_eq!(
        version.asset("assets/logo.bin").unwrap(),
        &[0x89, 0x50, 0x4e, 0x47]
    );
    // The registry returns the same immutable value, not a copy.
    let fetched = registry.get("web-research").unwrap();
    assert!(Arc::ptr_eq(&fetched, &version));
}

#[test]
fn frontmatter_optional_fields_parse() {
    let text = "---\nname: code-review\ndescription: Reviews diffs.\nlicense: Apache-2.0\nallowed-tools: read_file, search_code\ncompatibility: \"rusty >= 0.12\"\n---\n\nReview the diff.\n";
    let package = SkillPackage::from_markdown(text).expect("valid");
    let frontmatter = package.frontmatter();
    assert_eq!(frontmatter.license.as_deref(), Some("Apache-2.0"));
    assert_eq!(
        frontmatter.allowed_tools,
        ["read_file".to_owned(), "search_code".to_owned()]
    );
    assert_eq!(frontmatter.compatibility.as_deref(), Some("rusty >= 0.12"));
}

// --------------------------------------------------------------------- //
// Progressive disclosure tiers
// --------------------------------------------------------------------- //

#[test]
fn metadata_listing_never_loads_bodies() {
    let mut registry = SkillRegistry::new();
    for name in ["alpha-skill", "beta-skill", "gamma-skill"] {
        let body = format!("Instructions for {name}, with a unique body marker: BODY-{name}.");
        registry
            .register(full_package(name, &body), local_source(), "operator:ada")
            .unwrap();
    }

    let metadata = registry.list();
    assert_eq!(metadata.len(), 3);
    // Tier 1 carries exactly name + description + revision + content hash
    // plus the small optional fields — and nothing else. The strongest check
    // available at runtime: the serialized catalog contains no body bytes.
    let catalog_json = serde_json::to_string(&metadata).unwrap();
    for name in ["alpha-skill", "beta-skill", "gamma-skill"] {
        assert!(!catalog_json.contains(&format!("BODY-{name}")));
        assert!(!catalog_json.contains("Details on demand"));
    }
    // Tier 2 and 3 are reached only through a resolved version handle.
    let version = registry.get("beta-skill").unwrap();
    assert!(version.body().contains("BODY-beta-skill"));
    assert_eq!(
        version.reference_paths().collect::<Vec<_>>(),
        ["references/guide.md"]
    );
    assert_eq!(
        version.asset_paths().collect::<Vec<_>>(),
        ["assets/logo.bin"]
    );
    assert!(version.reference("references/missing.md").is_none());
}

// --------------------------------------------------------------------- //
// Frontmatter and path violations
// --------------------------------------------------------------------- //

#[test]
fn frontmatter_violations_are_rejected() {
    let cases: Vec<(&str, &str)> = vec![
        // Missing frontmatter entirely.
        ("# Just markdown\n", "no frontmatter"),
        // No closing delimiter.
        ("---\nname: a\ndescription: b\n", "unterminated"),
        // Unknown key.
        (
            "---\nname: a\ndescription: b\ntrusted: yes\n---\n\nBody.\n",
            "unknown key",
        ),
        // Duplicate key.
        (
            "---\nname: a\ndescription: b\ndescription: c\n---\n\nBody.\n",
            "duplicate key",
        ),
        // A line that is not key: value.
        (
            "---\nname: a\ndescription: b\njust words\n---\n\nBody.\n",
            "not key: value",
        ),
        // Indented (nested) line.
        (
            "---\nname: a\ndescription: b\n  nested: x\n---\n\nBody.\n",
            "nested line",
        ),
        // Missing required keys.
        ("---\ndescription: b\n---\n\nBody.\n", "missing name"),
        ("---\nname: a\n---\n\nBody.\n", "missing description"),
        // Empty body.
        ("---\nname: a\ndescription: b\n---\n\n", "empty body"),
    ];
    for (text, label) in cases {
        assert!(
            SkillPackage::from_markdown(text).is_err(),
            "case `{label}` must be rejected"
        );
    }
}

#[test]
fn name_rules_are_enforced() {
    let long_name = "a".repeat(65);
    for (name, label) in [
        ("Web-Research", "uppercase"),
        ("web_research", "underscore"),
        ("-web", "leading hyphen"),
        ("web-", "trailing hyphen"),
        ("web--research", "double hyphen"),
        ("", "empty"),
        (long_name.as_str(), "over 64 bytes"),
    ] {
        let text = skill_md(name, "A description.", "A body.");
        let error = SkillPackage::from_markdown(&text);
        assert!(error.is_err(), "name case `{label}` must be rejected");
        if !name.is_empty() {
            assert!(
                matches!(error, Err(SkillError::InvalidName { .. })),
                "name case `{label}` must fail as InvalidName, got {error:?}"
            );
        }
    }
    // Boundary: exactly 64 bytes of valid kebab-case parses.
    let max_name = format!("a{}", "-b".repeat(31) + "c");
    assert_eq!(max_name.len(), 64);
    assert!(SkillPackage::from_markdown(&skill_md(&max_name, "A description.", "A body.")).is_ok());
}

#[test]
fn description_rules_are_enforced() {
    // Over the 1024-byte ceiling.
    let text = skill_md(
        "a-skill",
        &"x".repeat(MAX_SKILL_DESCRIPTION_BYTES + 1),
        "A body.",
    );
    assert!(matches!(
        SkillPackage::from_markdown(&text),
        Err(SkillError::InvalidDescription { .. })
    ));
    // Control characters.
    let text = skill_md("a-skill", "bad\tdescription", "A body.");
    assert!(matches!(
        SkillPackage::from_markdown(&text),
        Err(SkillError::InvalidDescription { .. })
    ));
    // Exactly at the ceiling parses.
    let text = skill_md(
        "a-skill",
        &"x".repeat(MAX_SKILL_DESCRIPTION_BYTES),
        "A body.",
    );
    assert!(SkillPackage::from_markdown(&text).is_ok());
}

#[test]
fn path_traversal_is_rejected() {
    let skill = skill_md("a-skill", "A description.", "A body.").into_bytes();
    for (path, label) in [
        ("references/../secret.md", "parent traversal"),
        ("references/../../etc/passwd", "deep traversal"),
        ("/etc/passwd", "absolute path"),
        ("references/./guide.md", "dot component"),
        ("references//guide.md", "empty component"),
        ("references\\guide.md", "backslash separator"),
        ("C:/secrets.md", "drive prefix"),
        ("notes/guide.md", "unknown top-level directory"),
        ("README.md", "unknown top-level file"),
        ("references", "directory without a member"),
    ] {
        let mut files = BTreeMap::new();
        files.insert("SKILL.md".to_owned(), skill.clone());
        files.insert(path.to_owned(), b"payload".to_vec());
        let error = SkillPackage::from_files(files);
        assert!(
            matches!(error, Err(SkillError::InvalidPath { .. })),
            "path case `{label}` must fail as InvalidPath, got {error:?}"
        );
    }
}

// --------------------------------------------------------------------- //
// Size ceilings
// --------------------------------------------------------------------- //

#[test]
fn size_ceilings_are_enforced() {
    // Body over 256 KiB.
    let text = skill_md(
        "a-skill",
        "A description.",
        &"x".repeat(MAX_SKILL_BODY_BYTES),
    );
    assert!(matches!(
        SkillPackage::from_markdown(&text),
        Err(SkillError::BodyTooLarge { .. })
    ));
    // Body exactly at the ceiling parses (the trailing newline the helper
    // appends is part of the body, so leave room for it).
    let text = skill_md(
        "a-skill",
        "A description.",
        &"x".repeat(MAX_SKILL_BODY_BYTES - 1),
    );
    assert!(SkillPackage::from_markdown(&text).is_ok());

    // One member over the per-file ceiling.
    let mut files = BTreeMap::new();
    files.insert(
        "SKILL.md".to_owned(),
        skill_md("a-skill", "A description.", "A body.").into_bytes(),
    );
    files.insert(
        "assets/huge.bin".to_owned(),
        vec![0u8; MAX_SKILL_FILE_BYTES + 1],
    );
    assert!(matches!(
        SkillPackage::from_files(files),
        Err(SkillError::FileTooLarge { .. })
    ));

    // Members individually under the per-file ceiling but over the package
    // ceiling in aggregate.
    let mut files = BTreeMap::new();
    files.insert(
        "SKILL.md".to_owned(),
        skill_md("a-skill", "A description.", "A body.").into_bytes(),
    );
    let per_file = MAX_SKILL_FILE_BYTES - 1;
    for index in 0..=(MAX_SKILL_PACKAGE_BYTES / per_file) {
        files.insert(format!("assets/blob-{index}.bin"), vec![0u8; per_file]);
    }
    assert!(matches!(
        SkillPackage::from_files(files),
        Err(SkillError::PackageTooLarge { .. })
    ));
}

// --------------------------------------------------------------------- //
// The security scan
// --------------------------------------------------------------------- //

#[test]
fn scan_denies_script_tags() {
    let package = SkillPackage::from_markdown(&skill_md(
        "a-skill",
        "A description.",
        "Read this. <script>fetch('https://evil.example')</script> Then continue.",
    ))
    .unwrap();
    let report = scan_package(&package);
    assert!(report.has_denials());
    let denial = report.denials().next().unwrap();
    assert_eq!(denial.kind, ScanKind::EmbeddedScript);
    assert_eq!(denial.location, "SKILL.md");

    // Registration fails closed.
    let mut registry = SkillRegistry::new();
    let error = registry
        .register(package, local_source(), "operator:ada")
        .expect_err("script tags deny registration");
    match error {
        SkillError::ScanDenied { denials } => {
            assert_eq!(denials.len(), 1);
            assert_eq!(denials[0].kind, ScanKind::EmbeddedScript);
        }
        other => panic!("expected ScanDenied, got {other:?}"),
    }
    assert!(!registry.contains("a-skill"));
}

#[test]
fn scan_denies_credentialed_urls_without_leaking_them() {
    let mut files = BTreeMap::new();
    files.insert(
        "SKILL.md".to_owned(),
        skill_md("a-skill", "A description.", "See the guide.").into_bytes(),
    );
    files.insert(
        "references/guide.md".to_owned(),
        b"Fetch https://ci-bot:s3cr3t-token@internal.example/feed and summarize.".to_vec(),
    );
    let package = SkillPackage::from_files(files).unwrap();
    let report = scan_package(&package);
    assert!(report.has_denials());
    let denial = report.denials().next().unwrap();
    assert_eq!(denial.kind, ScanKind::CredentialedUrl);
    assert_eq!(denial.location, "references/guide.md");
    // The finding names the host but never echoes the credential bytes.
    assert!(denial.detail.contains("internal.example"));
    assert!(!denial.detail.contains("s3cr3t-token"));
    assert!(!denial.detail.contains("ci-bot"));

    // Plain URLs and scheme-less addresses do not trip the rule.
    let clean = SkillPackage::from_markdown(&skill_md(
        "b-skill",
        "A description.",
        "See https://example.com/docs, mirror ftp://files.example.com/pub, or mail user@example.com.",
    ))
    .unwrap();
    assert!(scan_package(&clean).is_clean());
}

#[test]
fn scan_warns_on_base64_blobs_but_registers() {
    let blob = "QUJDREVGRw".repeat(BASE64_BLOB_MIN_CHARS / 10 + 1);
    let body = format!("Legitimate instructions.\n\n{blob}\n\nMore instructions.");
    let package =
        SkillPackage::from_markdown(&skill_md("a-skill", "A description.", &body)).unwrap();
    let report = scan_package(&package);
    assert!(!report.has_denials());
    let warnings: Vec<_> = report.warnings().collect();
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].severity, ScanSeverity::Warning);
    assert_eq!(warnings[0].kind, ScanKind::Base64Blob);

    // A warning does not block registration; it travels with the version.
    let mut registry = SkillRegistry::new();
    let registration = registry
        .register(package, local_source(), "operator:ada")
        .expect("warnings do not deny");
    assert_eq!(registration.version.scan(), &report);
}

// --------------------------------------------------------------------- //
// Immutable versions
// --------------------------------------------------------------------- //

#[test]
fn re_registration_is_idempotent() {
    let mut registry = SkillRegistry::new();
    let first = registry
        .register(
            full_package("a-skill", "Version one."),
            local_source(),
            "operator:ada",
        )
        .unwrap();
    let second = registry
        .register(
            full_package("a-skill", "Version one."),
            local_source(),
            "operator:ada",
        )
        .unwrap();
    assert!(!first.already_registered);
    assert!(second.already_registered);
    assert!(Arc::ptr_eq(&first.version, &second.version));
    assert_eq!(registry.history("a-skill").len(), 1);
    assert_eq!(registry.get("a-skill").unwrap().revision(), 1);
}

#[test]
fn changed_content_appends_a_revision_and_moves_latest_forward() {
    let mut registry = SkillRegistry::new();
    let first = registry
        .register(
            full_package("a-skill", "Version one."),
            local_source(),
            "operator:ada",
        )
        .unwrap();
    let second = registry
        .register(
            full_package("a-skill", "Version two, revised."),
            local_source(),
            "operator:ada",
        )
        .unwrap();
    assert!(!second.already_registered);
    assert_eq!(second.version.revision(), 2);
    assert_ne!(first.version.content_hash(), second.version.content_hash());

    // Latest points at revision 2; revision 1 stays reachable, unchanged.
    assert!(Arc::ptr_eq(
        &registry.get("a-skill").unwrap(),
        &second.version
    ));
    let pinned = registry
        .get_version("a-skill", SkillVersionSelector::Revision(1))
        .unwrap();
    assert!(Arc::ptr_eq(&pinned, &first.version));
    assert_eq!(pinned.body(), "Version one.\n");
    let by_hash = registry
        .get_version(
            "a-skill",
            SkillVersionSelector::ContentHash(first.version.content_hash().to_owned()),
        )
        .unwrap();
    assert!(Arc::ptr_eq(&by_hash, &first.version));
    assert!(registry
        .get_version("a-skill", SkillVersionSelector::Revision(3))
        .is_none());

    // History is the append-only truth, ascending.
    let history = registry.history("a-skill");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].revision, 1);
    assert_eq!(history[1].revision, 2);
    assert_eq!(history[0].content_hash, first.version.content_hash());
    assert_eq!(history[1].content_hash, second.version.content_hash());

    // Re-registering an older revision does not drag the pointer back.
    let again = registry
        .register(
            full_package("a-skill", "Version one."),
            local_source(),
            "operator:ada",
        )
        .unwrap();
    assert!(again.already_registered);
    assert_eq!(again.version.revision(), 1);
    assert_eq!(registry.get("a-skill").unwrap().revision(), 2);
}

// --------------------------------------------------------------------- //
// Determinism
// --------------------------------------------------------------------- //

#[test]
fn listing_order_and_content_hashes_are_deterministic() {
    let names = ["delta-skill", "alpha-skill", "charlie-skill", "bravo-skill"];
    let mut registry = SkillRegistry::new();
    for name in names {
        registry
            .register(
                full_package(name, "Instructions."),
                local_source(),
                "operator:ada",
            )
            .unwrap();
    }
    let listed: Vec<_> = registry
        .list()
        .iter()
        .map(|metadata| metadata.name.clone())
        .collect();
    assert_eq!(
        listed,
        ["alpha-skill", "bravo-skill", "charlie-skill", "delta-skill"]
    );

    // A second registry fed the same packages in a different order agrees on
    // every name's content hash and revision.
    let mut reordered = SkillRegistry::new();
    for name in ["bravo-skill", "delta-skill", "alpha-skill", "charlie-skill"] {
        reordered
            .register(
                full_package(name, "Instructions."),
                local_source(),
                "operator:ada",
            )
            .unwrap();
    }
    assert_eq!(registry.list(), reordered.list());
}

// --------------------------------------------------------------------- //
// The filesystem loader
// --------------------------------------------------------------------- //

/// A unique temp directory that removes itself on drop.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("rusty-skill-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn directory_loader_round_trips() {
    let temp = TempDir::new();
    let root = temp.0.join("web-research");
    std::fs::create_dir_all(root.join("references/nested")).unwrap();
    std::fs::create_dir_all(root.join("assets")).unwrap();
    std::fs::write(
        root.join("SKILL.md"),
        skill_md(
            "web-research",
            "The web-research skill.",
            "Search, then summarize.",
        ),
    )
    .unwrap();
    std::fs::write(root.join("references/guide.md"), b"# Guide\n").unwrap();
    std::fs::write(root.join("references/nested/deep.md"), b"# Deep\n").unwrap();
    std::fs::write(root.join("assets/logo.bin"), [0x89, 0x50]).unwrap();

    let package = SkillPackage::from_dir(&root).expect("directory loads");
    assert_eq!(package.name(), "web-research");
    assert_eq!(
        package.references().keys().collect::<Vec<_>>(),
        ["references/guide.md", "references/nested/deep.md"]
    );
    assert_eq!(
        package.assets().keys().collect::<Vec<_>>(),
        ["assets/logo.bin"]
    );

    let mut registry = SkillRegistry::new();
    let registration = registry
        .register(
            package,
            SkillSource::LocalPath {
                path: root.display().to_string(),
            },
            "operator:ada",
        )
        .unwrap();
    assert_eq!(registration.version.revision(), 1);
}

#[cfg(unix)]
#[test]
fn directory_loader_refuses_symlinks() {
    let temp = TempDir::new();
    let root = temp.0.join("a-skill");
    std::fs::create_dir_all(root.join("references")).unwrap();
    std::fs::write(
        root.join("SKILL.md"),
        skill_md("a-skill", "A description.", "A body."),
    )
    .unwrap();
    let outside = temp.0.join("secret.md");
    std::fs::write(&outside, b"not part of the package").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("references/link.md")).unwrap();

    let error = SkillPackage::from_dir(&root).expect_err("symlinks are refused");
    assert!(
        matches!(error, SkillError::InvalidPath { .. }),
        "got {error:?}"
    );
}

#[test]
fn directory_loader_refuses_unknown_top_level_entries() {
    let temp = TempDir::new();
    let root = temp.0.join("a-skill");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("SKILL.md"),
        skill_md("a-skill", "A description.", "A body."),
    )
    .unwrap();
    std::fs::write(root.join(".DS_Store"), b"junk").unwrap();

    let error = SkillPackage::from_dir(&root).expect_err("unknown members are refused");
    assert!(
        matches!(error, SkillError::InvalidPath { .. }),
        "got {error:?}"
    );
}
