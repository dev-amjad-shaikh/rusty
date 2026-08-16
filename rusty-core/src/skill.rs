//! The skill plane: governed `SKILL.md` packages — parsing, progressive
//! disclosure, provenance, security scanning, immutable versions, and the
//! registry the rest of the harness resolves skills through.
//!
//! A skill is versioned procedural knowledge, not an action surface: it
//! alters model context through progressive disclosure and carries neither
//! executable authority nor credentials (the capability-harness vocabulary:
//! tools and connectors provide action; skills and knowledge provide
//! context). The package format follows the emerging Agent Skills
//! convention — one `SKILL.md` with YAML frontmatter (`name` and
//! `description` required) plus a markdown body, with optional
//! `references/` and `assets/` members beside it.
//!
//! # Progressive disclosure
//!
//! The registry exposes three tiers so an agent can hold hundreds of skill
//! entries and pay only for what it actually loads:
//!
//! 1. **Metadata** — [`SkillMetadata`] (name, description, revision, content
//!    hash, and the small optional frontmatter fields). [`SkillRegistry::list`]
//!    and [`SkillRegistry::history`] return metadata projections only; the
//!    type has no body field, so a listing cannot pull bodies along by
//!    accident.
//! 2. **Body** — the `SKILL.md` instructions, reached through
//!    [`SkillVersion::body`] once a version handle is resolved.
//! 3. **References and assets** — enumerated on demand
//!    ([`SkillVersion::reference_paths`] / [`SkillVersion::asset_paths`]) and
//!    loaded one member at a time ([`SkillVersion::reference`] /
//!    [`SkillVersion::asset`]).
//!
//! # Governance invariants
//!
//! - **Fail-closed parsing.** [`SkillPackage`] validates at construction:
//!   frontmatter present and in the supported subset, kebab-case name,
//!   bounded description and body, package and per-member byte ceilings,
//!   and member-path hygiene (relative, `..`-free, symlink-free). An
//!   invalid package cannot exist as a value, so nothing downstream needs
//!   to re-check it.
//! - **Provenance is mandatory.** Every registered version records its
//!   [`SkillSource`], an author string, and the content hash — a package
//!   that cannot name its origin cannot be audited.
//! - **Deterministic local scan.** [`scan_package`] flags embedded HTML
//!   script tags and credentialed (userinfo) URLs as denials and large
//!   base64 blobs as warnings. Registration fails closed on any denial;
//!   warnings travel with the version as part of its recorded
//!   [`ScanReport`].
//! - **Immutable, content-addressed versions.** A version's identity is the
//!   SHA-256 of the package's canonical serialization
//!   ([`SkillPackage::content_hash`]). Re-registering identical content is
//!   idempotent; changed content under the same name appends a new
//!   revision and moves the latest pointer forward — never backward, never
//!   in place.
//!
//! # Evidence
//!
//! A run that discloses a skill body should journal that load — the skill
//! name, revision, and content hash are exactly the identifiers this module
//! exposes for it — so replay can pin the context the model saw. The event
//! kind belongs to the run-integration slice (resolved capability sets pin
//! skills into the run manifest the same wave their loads become journal
//! events); [`crate::record::RunEventKind`] is owned by the record plane and
//! is deliberately untouched here.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::record::sha256_hex;

/// The canonical-serialization domain prefix of a skill content hash. Bump
/// only on a breaking change to the package model; additive frontmatter
/// fields extend the canonical form instead.
pub const SKILL_SCHEMA_VERSION: &str = "skill-v1";

/// The longest a skill name may run, in bytes. Kebab-case, per the Agent
/// Skills convention.
pub const MAX_SKILL_NAME_LEN: usize = 64;

/// The largest a skill description may be, in bytes.
pub const MAX_SKILL_DESCRIPTION_BYTES: usize = 1024;

/// The largest a `SKILL.md` body may be, in bytes. The body is the tier-2
/// disclosure unit; the ceiling keeps one skill from dominating an agent's
/// context budget.
pub const MAX_SKILL_BODY_BYTES: usize = 256 * 1024;

/// The largest any single `references/` or `assets/` member may be, in
/// bytes.
pub const MAX_SKILL_FILE_BYTES: usize = 512 * 1024;

/// The largest a whole package may be — `SKILL.md` plus every reference and
/// asset — in bytes.
pub const MAX_SKILL_PACKAGE_BYTES: usize = 2 * 1024 * 1024;

/// The largest an optional frontmatter scalar (`license`, `compatibility`,
/// one `allowed-tools` entry) may be, in bytes.
pub const MAX_FRONTMATTER_VALUE_BYTES: usize = 256;

/// The most `allowed-tools` entries a frontmatter may declare.
pub const MAX_ALLOWED_TOOLS: usize = 32;

/// The longest an author string may run, in bytes.
pub const MAX_SKILL_AUTHOR_LEN: usize = 128;

/// The shortest run of base64-alphabet characters the security scan reports
/// as a blob. Prose never produces such runs; embedded payloads do — the
/// finding is a warning, not a denial, because legitimate documentation can
/// carry one.
pub const BASE64_BLOB_MIN_CHARS: usize = 256;

/// The most findings one scan kind reports per file. The scan is an audit
/// surface, not a flood channel; beyond the cap the detail string carries
/// the total count.
const MAX_FINDINGS_PER_KIND: usize = 8;

/// Every way skill-plane validation can refuse a package or a registration.
///
/// Module-local, mirroring [`crate::registry::RegistryError`]: the
/// fail-closed contract wants the refused rule named in the type, not
/// flattened into a message string at the crate boundary.
#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    /// `SKILL.md` does not open with a `---` frontmatter delimiter.
    #[error("SKILL.md has no frontmatter: the file must open with a `---` delimiter line")]
    MissingFrontmatter,

    /// The frontmatter block is outside the supported subset (no closing
    /// delimiter, an unknown or duplicated key, a line that is not
    /// `key: value`, or an indented/nested line).
    #[error("malformed frontmatter: {reason}")]
    MalformedFrontmatter {
        /// The rule the block broke.
        reason: String,
    },

    /// A skill name outside the naming rules (kebab-case, bounded).
    #[error("invalid skill name {name:?}: {reason}")]
    InvalidName {
        /// The refused name.
        name: String,
        /// The rule it broke.
        reason: &'static str,
    },

    /// A description outside its rules (non-empty, trimmed, control-free,
    /// bounded).
    #[error("invalid skill description: {reason}")]
    InvalidDescription {
        /// The rule it broke.
        reason: &'static str,
    },

    /// An optional frontmatter scalar outside its rules.
    #[error("invalid frontmatter field `{field}`: {reason}")]
    InvalidField {
        /// The field at fault.
        field: &'static str,
        /// The rule it broke.
        reason: &'static str,
    },

    /// `SKILL.md` is not valid UTF-8.
    #[error("SKILL.md is not valid UTF-8")]
    InvalidUtf8,

    /// The markdown body is empty; a skill without instructions is not a
    /// skill.
    #[error("SKILL.md body is empty: a skill package must carry instructions")]
    EmptyBody,

    /// The body exceeds [`MAX_SKILL_BODY_BYTES`].
    #[error("SKILL.md body is {bytes} bytes, above the {max}-byte ceiling")]
    BodyTooLarge {
        /// The refused size.
        bytes: usize,
        /// The ceiling.
        max: usize,
    },

    /// One package member exceeds [`MAX_SKILL_FILE_BYTES`].
    #[error("package member {path:?} is {bytes} bytes, above the {max}-byte per-file ceiling")]
    FileTooLarge {
        /// The refused member.
        path: String,
        /// Its size.
        bytes: usize,
        /// The ceiling.
        max: usize,
    },

    /// The whole package exceeds [`MAX_SKILL_PACKAGE_BYTES`].
    #[error("skill package is {bytes} bytes, above the {max}-byte package ceiling")]
    PackageTooLarge {
        /// The refused size.
        bytes: usize,
        /// The ceiling.
        max: usize,
    },

    /// A member path outside the hygiene rules (absolute, parent-traversing,
    /// backslash- or drive-separated, symlinked, or not under `references/`
    /// or `assets/`).
    #[error("invalid package member path {path:?}: {reason}")]
    InvalidPath {
        /// The refused path.
        path: String,
        /// The rule it broke.
        reason: &'static str,
    },

    /// A filesystem read failed in [`SkillPackage::from_dir`].
    #[error("could not read {path}: {message}")]
    Io {
        /// The path that failed.
        path: String,
        /// The underlying error.
        message: String,
    },

    /// An author string outside its rules (non-empty, trimmed, control-free,
    /// bounded).
    #[error("invalid skill author: {reason}")]
    InvalidAuthor {
        /// The rule it broke.
        reason: &'static str,
    },

    /// The security scan reported at least one denial; registration fails
    /// closed. The refused denials travel with the error so the caller can
    /// surface them verbatim.
    #[error("security scan denied the package: {} denial(s)", denials.len())]
    ScanDenied {
        /// The denials that refused the package.
        denials: Vec<ScanFinding>,
    },
}

/// `true` when `name` is kebab-case within the Agent Skills bound: lowercase
/// ASCII letters, digits, and single interior hyphens, at most
/// [`MAX_SKILL_NAME_LEN`] bytes.
fn is_valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SKILL_NAME_LEN
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
}

/// The parsed frontmatter of a `SKILL.md`. `name` and `description` are the
/// Agent Skills contract; the rest are optional and bounded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillFrontmatter {
    /// Kebab-case skill name; also the registry key.
    pub name: String,

    /// Model- and human-facing summary; the tier-1 discovery text.
    pub description: String,

    /// SPDX-style license identifier, when declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,

    /// Tool names the skill's instructions assume access to. Advisory at
    /// this layer — capability admission decides what a run actually gets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_tools: Vec<String>,

    /// Environment compatibility note (runtime versions, platforms), when
    /// declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<String>,
}

/// Strip one pair of matching surrounding quotes from a frontmatter scalar,
/// when present. The supported subset carries plain and simply-quoted
/// scalars; anything fancier (escapes, folded blocks) never reaches this
/// function because the line parser refuses it first.
fn unquote(value: &str) -> &str {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        let first = bytes[0];
        let last = bytes[value.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &value[1..value.len() - 1];
        }
    }
    value
}

/// Validate a bounded optional scalar (`license`, `compatibility`).
fn validate_optional_scalar(field: &'static str, value: &str) -> Result<(), SkillError> {
    if value.is_empty() {
        return Err(SkillError::InvalidField {
            field,
            reason: "must be non-empty when declared",
        });
    }
    if value.len() > MAX_FRONTMATTER_VALUE_BYTES {
        return Err(SkillError::InvalidField {
            field,
            reason: "exceeds the frontmatter value ceiling",
        });
    }
    if value.chars().any(char::is_control) {
        return Err(SkillError::InvalidField {
            field,
            reason: "must not contain control characters",
        });
    }
    Ok(())
}

/// Split `SKILL.md` into its validated frontmatter and its body.
///
/// The extractor deliberately supports only the flat `key: value` subset of
/// YAML frontmatter: a full YAML parser would accept constructs (anchors,
/// nested mappings, tags) this contract has no meaning for, so the strict
/// subset is the fail-closed choice — anything richer is malformed here
/// rather than silently half-read.
fn parse_skill_md(text: &str) -> Result<(SkillFrontmatter, String), SkillError> {
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return Err(SkillError::MissingFrontmatter);
    };

    // Locate the closing delimiter line, tracking byte offsets so the body
    // can be sliced without re-scanning.
    let mut cursor = 0;
    let mut close = None;
    for line in rest.split('\n') {
        let trimmed = line.strip_suffix('\r').unwrap_or(line);
        if trimmed == "---" {
            close = Some((cursor, cursor + line.len()));
            break;
        }
        cursor += line.len() + 1;
    }
    let Some((block_end, close_end)) = close else {
        return Err(SkillError::MalformedFrontmatter {
            reason: "no closing `---` delimiter line".to_owned(),
        });
    };
    let block = &rest[..block_end];
    // `close_end + 1` steps over the newline terminating the closing
    // delimiter; the split above guarantees one exists.
    let body = rest[close_end + 1..].trim_start_matches(['\r', '\n']).to_owned();

    let mut name = None;
    let mut description = None;
    let mut license = None;
    let mut compatibility = None;
    let mut allowed_tools = Vec::new();
    let mut allowed_tools_seen = false;

    for raw_line in block.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            return Err(SkillError::MalformedFrontmatter {
                reason: format!("indented line {line:?}: nested frontmatter is not supported"),
            });
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(SkillError::MalformedFrontmatter {
                reason: format!("line {line:?} is not `key: value`"),
            });
        };
        let key = key.trim();
        let value = unquote(value.trim());
        let assign = |slot: &mut Option<String>| -> Result<(), SkillError> {
            if slot.is_some() {
                return Err(SkillError::MalformedFrontmatter {
                    reason: format!("duplicate key `{key}`"),
                });
            }
            *slot = Some(value.to_owned());
            Ok(())
        };
        match key {
            "name" => assign(&mut name)?,
            "description" => assign(&mut description)?,
            "license" => assign(&mut license)?,
            "compatibility" => assign(&mut compatibility)?,
            "allowed-tools" => {
                if allowed_tools_seen {
                    return Err(SkillError::MalformedFrontmatter {
                        reason: "duplicate key `allowed-tools`".to_owned(),
                    });
                }
                allowed_tools_seen = true;
                for entry in value.split(',') {
                    let entry = entry.trim();
                    if entry.is_empty() {
                        return Err(SkillError::InvalidField {
                            field: "allowed-tools",
                            reason: "entries must be non-empty",
                        });
                    }
                    if entry.len() > MAX_FRONTMATTER_VALUE_BYTES
                        || !entry
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
                    {
                        return Err(SkillError::InvalidField {
                            field: "allowed-tools",
                            reason: "entries must be bounded tool names (ASCII letters, digits, `.`, `_`, `:`, `-`)",
                        });
                    }
                    allowed_tools.push(entry.to_owned());
                }
                if allowed_tools.len() > MAX_ALLOWED_TOOLS {
                    return Err(SkillError::InvalidField {
                        field: "allowed-tools",
                        reason: "too many entries",
                    });
                }
            }
            other => {
                return Err(SkillError::MalformedFrontmatter {
                    reason: format!("unknown key `{other}`"),
                });
            }
        }
    }

    let name = name.ok_or(SkillError::MalformedFrontmatter {
        reason: "missing required key `name`".to_owned(),
    })?;
    if !is_valid_skill_name(&name) {
        return Err(SkillError::InvalidName {
            name,
            reason: "must be kebab-case: lowercase ASCII letters, digits, and single interior hyphens, 1..=64 bytes",
        });
    }
    let description = description.ok_or(SkillError::MalformedFrontmatter {
        reason: "missing required key `description`".to_owned(),
    })?;
    if description.is_empty() {
        return Err(SkillError::InvalidDescription {
            reason: "must be non-empty",
        });
    }
    if description.len() > MAX_SKILL_DESCRIPTION_BYTES {
        return Err(SkillError::InvalidDescription {
            reason: "exceeds the 1024-byte ceiling",
        });
    }
    if description.chars().any(char::is_control) {
        return Err(SkillError::InvalidDescription {
            reason: "must not contain control characters",
        });
    }
    if let Some(license) = &license {
        validate_optional_scalar("license", license)?;
    }
    if let Some(compatibility) = &compatibility {
        validate_optional_scalar("compatibility", compatibility)?;
    }

    if body.trim().is_empty() {
        return Err(SkillError::EmptyBody);
    }
    if body.len() > MAX_SKILL_BODY_BYTES {
        return Err(SkillError::BodyTooLarge {
            bytes: body.len(),
            max: MAX_SKILL_BODY_BYTES,
        });
    }

    Ok((
        SkillFrontmatter {
            name,
            description,
            license,
            allowed_tools,
            compatibility,
        },
        body,
    ))
}

/// Which member directory a validated package path belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberKind {
    /// Under `references/` — markdown the disclosure flow loads into
    /// context on demand.
    Reference,
    /// Under `assets/` — binary or text payloads the skill's instructions
    /// consume by path.
    Asset,
}

/// Validate a package member path and classify its directory. The hygiene
/// rules: relative, forward-separated, no `.`/`..`/empty components, no
/// backslashes or drive colons, and exactly the `references/` or `assets/`
/// root with at least one component beneath it.
fn validate_member_path(path: &str) -> Result<MemberKind, SkillError> {
    let refuse = |reason: &'static str| SkillError::InvalidPath {
        path: path.to_owned(),
        reason,
    };
    if path.is_empty() || path.starts_with('/') {
        return Err(refuse("must be a relative path"));
    }
    if path.bytes().any(|byte| byte == b'\\' || byte == b':') || path.chars().any(char::is_control)
    {
        return Err(refuse(
            "must use forward separators only — no backslashes, drive colons, or control characters",
        ));
    }
    let components: Vec<&str> = path.split('/').collect();
    if components
        .iter()
        .any(|component| component.is_empty() || *component == "." || *component == "..")
    {
        return Err(refuse("must not contain empty, `.`, or `..` components"));
    }
    if components.len() < 2 {
        return Err(refuse("must name a file beneath `references/` or `assets/`"));
    }
    match components[0] {
        "references" => Ok(MemberKind::Reference),
        "assets" => Ok(MemberKind::Asset),
        _ => Err(refuse("must live under `references/` or `assets/`")),
    }
}

/// A validated skill package: the parsed `SKILL.md` plus its reference and
/// asset members.
///
/// Construction is the validation boundary — [`SkillPackage::from_markdown`],
/// [`SkillPackage::from_files`], and [`SkillPackage::from_dir`] all fail
/// closed, so a `SkillPackage` value is a proof that the package met every
/// rule this module states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPackage {
    frontmatter: SkillFrontmatter,
    body: String,
    references: BTreeMap<String, Vec<u8>>,
    assets: BTreeMap<String, Vec<u8>>,
}

impl SkillPackage {
    /// Parse a package from `SKILL.md` text alone (no members).
    pub fn from_markdown(skill_md: &str) -> Result<Self, SkillError> {
        if skill_md.len() > MAX_SKILL_PACKAGE_BYTES {
            return Err(SkillError::PackageTooLarge {
                bytes: skill_md.len(),
                max: MAX_SKILL_PACKAGE_BYTES,
            });
        }
        let (frontmatter, body) = parse_skill_md(skill_md)?;
        Ok(Self {
            frontmatter,
            body,
            references: BTreeMap::new(),
            assets: BTreeMap::new(),
        })
    }

    /// Parse a package from an in-memory file map: relative path → bytes,
    /// with `SKILL.md` at the root and every other entry beneath
    /// `references/` or `assets/`. Unknown top-level entries are refused —
    /// the package shape is closed.
    pub fn from_files(files: BTreeMap<String, Vec<u8>>) -> Result<Self, SkillError> {
        let mut total = 0usize;
        let mut skill_md = None;
        let mut references = BTreeMap::new();
        let mut assets = BTreeMap::new();
        for (path, bytes) in files {
            total = total.saturating_add(bytes.len());
            if total > MAX_SKILL_PACKAGE_BYTES {
                return Err(SkillError::PackageTooLarge {
                    bytes: total,
                    max: MAX_SKILL_PACKAGE_BYTES,
                });
            }
            if path == "SKILL.md" {
                skill_md = Some(bytes);
                continue;
            }
            if bytes.len() > MAX_SKILL_FILE_BYTES {
                return Err(SkillError::FileTooLarge {
                    path,
                    bytes: bytes.len(),
                    max: MAX_SKILL_FILE_BYTES,
                });
            }
            match validate_member_path(&path)? {
                MemberKind::Reference => references.insert(path, bytes),
                MemberKind::Asset => assets.insert(path, bytes),
            };
        }
        let skill_md = skill_md.ok_or(SkillError::MissingFrontmatter)?;
        let text = String::from_utf8(skill_md).map_err(|_| SkillError::InvalidUtf8)?;
        let (frontmatter, body) = parse_skill_md(&text)?;
        Ok(Self {
            frontmatter,
            body,
            references,
            assets,
        })
    }

    /// Load a package from a directory on disk. The walk refuses symlinks
    /// outright (a symlink's target escapes the hygiene rules by
    /// construction) and applies the same closed shape and ceilings as
    /// [`SkillPackage::from_files`].
    pub fn from_dir(root: &Path) -> Result<Self, SkillError> {
        let io = |path: &Path, error: std::io::Error| SkillError::Io {
            path: path.display().to_string(),
            message: error.to_string(),
        };
        let metadata = std::fs::symlink_metadata(root).map_err(|error| io(root, error))?;
        if !metadata.is_dir() {
            return Err(SkillError::InvalidPath {
                path: root.display().to_string(),
                reason: "package root must be a directory",
            });
        }
        let mut files = BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            let mut entries: Vec<_> = std::fs::read_dir(&dir)
                .map_err(|error| io(&dir, error))?
                .collect::<std::result::Result<_, _>>()
                .map_err(|error| io(&dir, error))?;
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                let metadata =
                    std::fs::symlink_metadata(&path).map_err(|error| io(&path, error))?;
                let file_type = metadata.file_type();
                if file_type.is_symlink() {
                    return Err(SkillError::InvalidPath {
                        path: path.display().to_string(),
                        reason: "symlinks are not permitted in a skill package",
                    });
                }
                if file_type.is_dir() {
                    pending.push(path);
                    continue;
                }
                if !file_type.is_file() {
                    return Err(SkillError::InvalidPath {
                        path: path.display().to_string(),
                        reason: "package members must be regular files",
                    });
                }
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| SkillError::InvalidPath {
                        path: path.display().to_string(),
                        reason: "member escapes the package root",
                    })?;
                let relative = relative
                    .components()
                    .map(|component| {
                        component.as_os_str().to_str().ok_or_else(|| SkillError::InvalidPath {
                            path: path.display().to_string(),
                            reason: "member names must be valid UTF-8",
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .join("/");
                let bytes = std::fs::read(&path).map_err(|error| io(&path, error))?;
                files.insert(relative, bytes);
            }
        }
        Self::from_files(files)
    }

    /// The parsed frontmatter.
    pub fn frontmatter(&self) -> &SkillFrontmatter {
        &self.frontmatter
    }

    /// The skill name (the registry key).
    pub fn name(&self) -> &str {
        &self.frontmatter.name
    }

    /// The tier-1 discovery text.
    pub fn description(&self) -> &str {
        &self.frontmatter.description
    }

    /// The tier-2 disclosure unit: the `SKILL.md` instructions.
    pub fn body(&self) -> &str {
        &self.body
    }

    /// The reference members, keyed by validated package-relative path.
    pub fn references(&self) -> &BTreeMap<String, Vec<u8>> {
        &self.references
    }

    /// The asset members, keyed by validated package-relative path.
    pub fn assets(&self) -> &BTreeMap<String, Vec<u8>> {
        &self.assets
    }

    /// The content address of the package: SHA-256 over the canonical
    /// serialization — the schema-version domain, then every frontmatter
    /// field, the body, and the path-sorted member bytes, each field
    /// length-prefixed so no two distinct packages can serialize to the same
    /// stream. The hash is the version's identity: changed content is a new
    /// version, identical content is the same one.
    pub fn content_hash(&self) -> String {
        sha256_hex(&canonical_bytes(self))
    }
}

/// The canonical serialization behind [`SkillPackage::content_hash`]. A byte
/// stream of `key:length\nvalue\n` fields; `BTreeMap` iteration supplies the
/// member ordering, so the digest is platform- and map-order-independent.
fn canonical_bytes(package: &SkillPackage) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(SKILL_SCHEMA_VERSION.as_bytes());
    out.push(b'\n');
    let mut field = |key: &str, value: &[u8]| {
        out.extend_from_slice(key.as_bytes());
        out.push(b':');
        out.extend_from_slice(value.len().to_string().as_bytes());
        out.push(b'\n');
        out.extend_from_slice(value);
        out.push(b'\n');
    };
    let frontmatter = &package.frontmatter;
    field("name", frontmatter.name.as_bytes());
    field("description", frontmatter.description.as_bytes());
    field("license", frontmatter.license.as_deref().unwrap_or("").as_bytes());
    for tool in &frontmatter.allowed_tools {
        field("allowed-tool", tool.as_bytes());
    }
    field(
        "compatibility",
        frontmatter.compatibility.as_deref().unwrap_or("").as_bytes(),
    );
    field("body", package.body.as_bytes());
    for (path, bytes) in &package.references {
        field("reference-path", path.as_bytes());
        field("reference-bytes", bytes);
    }
    for (path, bytes) in &package.assets {
        field("asset-path", path.as_bytes());
        field("asset-bytes", bytes);
    }
    out
}

// --------------------------------------------------------------------- //
// The security scan
// --------------------------------------------------------------------- //

/// How seriously a scan finding weighs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanSeverity {
    /// Suspicious but admissible; travels with the version as recorded
    /// evidence.
    Warning,
    /// A hard violation; registration fails closed.
    Denial,
}

/// What a scan finding found. Closed enum — report consumers match
/// exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanKind {
    /// An embedded HTML `<script` tag in model-facing text. Skill bodies
    /// render into contexts where embedded markup is an injection surface.
    EmbeddedScript,
    /// A URL whose authority carries userinfo (`scheme://user:secret@host`)
    /// — the shape of a credential exfiltration channel.
    CredentialedUrl,
    /// A run of at least [`BASE64_BLOB_MIN_CHARS`] base64-alphabet
    /// characters — an embedded payload hiding from review.
    Base64Blob,
}

/// One structured scan observation. Findings never echo the offending bytes:
/// `detail` names offsets, counts, and — for credentialed URLs — the host
/// only, so a report cannot leak the credential it caught.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanFinding {
    /// Warning or denial.
    pub severity: ScanSeverity,
    /// What was found.
    pub kind: ScanKind,
    /// The package member the finding belongs to (`SKILL.md` for the body).
    pub location: String,
    /// Human-readable detail (offsets, counts, redacted host).
    pub detail: String,
}

/// The outcome of scanning one package, in deterministic (location, kind,
/// offset) order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanReport {
    /// Every finding, warnings and denials together.
    pub findings: Vec<ScanFinding>,
}

impl ScanReport {
    /// `true` when nothing was found.
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }

    /// `true` when at least one denial is present; registration refuses the
    /// package in that case.
    pub fn has_denials(&self) -> bool {
        self.findings
            .iter()
            .any(|finding| finding.severity == ScanSeverity::Denial)
    }

    /// The denials, in report order.
    pub fn denials(&self) -> impl Iterator<Item = &ScanFinding> {
        self.findings
            .iter()
            .filter(|finding| finding.severity == ScanSeverity::Denial)
    }

    /// The warnings, in report order.
    pub fn warnings(&self) -> impl Iterator<Item = &ScanFinding> {
        self.findings
            .iter()
            .filter(|finding| finding.severity == ScanSeverity::Warning)
    }
}

/// Run the deterministic local scan over a package's model-facing text: the
/// `SKILL.md` body and every reference member. Assets are payloads consumed
/// by path, not text rendered into context, so they are outside the scan's
/// reach — their ceilings are the parser's, enforced at construction.
pub fn scan_package(package: &SkillPackage) -> ScanReport {
    let mut findings = Vec::new();
    scan_text("SKILL.md", &package.body, &mut findings);
    for (path, bytes) in &package.references {
        scan_text(path, &String::from_utf8_lossy(bytes), &mut findings);
    }
    ScanReport { findings }
}

/// Scan one text member, appending findings for each kind observed. Per
/// kind, at most [`MAX_FINDINGS_PER_KIND`] findings are recorded per file;
/// the first finding's detail carries the total count.
fn scan_text(location: &str, text: &str, findings: &mut Vec<ScanFinding>) {
    scan_scripts(location, text, findings);
    scan_credentialed_urls(location, text, findings);
    scan_base64_blobs(location, text, findings);
}

/// Emit findings for one kind in one file, capped at
/// [`MAX_FINDINGS_PER_KIND`]; when more occurrences exist, the last emitted
/// finding's detail names the unreported remainder. Reports stay bounded no
/// matter how hostile the input.
fn emit_occurrences(
    findings: &mut Vec<ScanFinding>,
    severity: ScanSeverity,
    kind: ScanKind,
    location: &str,
    occurrences: Vec<String>,
) {
    let total = occurrences.len();
    for (index, detail) in occurrences.into_iter().enumerate() {
        if index >= MAX_FINDINGS_PER_KIND {
            break;
        }
        let detail = if index == MAX_FINDINGS_PER_KIND - 1 && total > MAX_FINDINGS_PER_KIND {
            format!("{detail} ({} further occurrence(s) unreported)", total - MAX_FINDINGS_PER_KIND)
        } else {
            detail
        };
        findings.push(ScanFinding {
            severity,
            kind,
            location: location.to_owned(),
            detail,
        });
    }
}

/// Flag embedded HTML script tags (denial).
fn scan_scripts(location: &str, text: &str, findings: &mut Vec<ScanFinding>) {
    let lowered = text.to_lowercase();
    let mut occurrences = Vec::new();
    let mut start = 0;
    while let Some(offset) = lowered[start..].find("<script") {
        let at = start + offset;
        occurrences.push(format!("`<script` tag at byte offset {at}"));
        start = at + "<script".len();
    }
    emit_occurrences(
        findings,
        ScanSeverity::Denial,
        ScanKind::EmbeddedScript,
        location,
        occurrences,
    );
}

/// Flag URLs whose authority carries userinfo — `scheme://user:secret@host`
/// (denial). Only the host is reported; the credential bytes never enter the
/// finding.
fn scan_credentialed_urls(location: &str, text: &str, findings: &mut Vec<ScanFinding>) {
    let mut occurrences = Vec::new();
    let mut start = 0;
    while let Some(offset) = text[start..].find("://") {
        let authority_start = start + offset + "://".len();
        let authority_end = text[authority_start..]
            .find(|ch: char| {
                ch == '/' || ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '\'' | ')' | ']')
            })
            .map(|tail| authority_start + tail)
            .unwrap_or(text.len());
        let authority = &text[authority_start..authority_end];
        if authority.contains('@') {
            let host = authority.rsplit('@').next().unwrap_or(authority);
            let host: String = host.chars().take(64).collect();
            occurrences.push(format!(
                "URL with embedded credentials at byte offset {start}, host `{host}` (userinfo redacted)"
            ));
        }
        start = authority_end.max(authority_start);
    }
    emit_occurrences(
        findings,
        ScanSeverity::Denial,
        ScanKind::CredentialedUrl,
        location,
        occurrences,
    );
}

/// Flag base64-alphabet runs at or above [`BASE64_BLOB_MIN_CHARS`]
/// (warning).
fn scan_base64_blobs(location: &str, text: &str, findings: &mut Vec<ScanFinding>) {
    let bytes = text.as_bytes();
    let is_blob_char = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/';
    let mut occurrences = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if !is_blob_char(bytes[index]) {
            index += 1;
            continue;
        }
        let run_start = index;
        while index < bytes.len() && is_blob_char(bytes[index]) {
            index += 1;
        }
        let mut run_end = index;
        while run_end < bytes.len() && bytes[run_end] == b'=' {
            run_end += 1;
        }
        let run = run_end - run_start;
        if run >= BASE64_BLOB_MIN_CHARS {
            occurrences.push(format!("base64 blob of {run} characters at byte offset {run_start}"));
        }
        index = run_end.max(index);
    }
    emit_occurrences(
        findings,
        ScanSeverity::Warning,
        ScanKind::Base64Blob,
        location,
        occurrences,
    );
}

// --------------------------------------------------------------------- //
// Provenance and versions
// --------------------------------------------------------------------- //

/// Where a skill package came from. Closed enum; the attribution travels
/// with the version through every later consumer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SkillSource {
    /// Loaded from a local directory or file map: `local:{path}`.
    LocalPath {
        /// The package root as presented at load time.
        path: String,
    },
    /// Installed from a named registry (the ecosystem slice's taps):
    /// `registry:{name}`.
    Registry {
        /// The registry's name.
        name: String,
    },
}

impl SkillSource {
    /// The canonical id string (`local:{path}` / `registry:{name}`).
    pub fn as_id_string(&self) -> String {
        match self {
            SkillSource::LocalPath { path } => format!("local:{path}"),
            SkillSource::Registry { name } => format!("registry:{name}"),
        }
    }
}

/// A version's origin: where the package came from, who registered it, and
/// its content address. Mandatory — a version that cannot name its origin
/// cannot be audited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillProvenance {
    /// The load source.
    pub source: SkillSource,

    /// Who registered the package (an operator identity, an agent id, a
    /// pipeline name — application code chooses the vocabulary).
    pub author: String,

    /// The content address; equals [`SkillVersion::content_hash`]. Repeated
    /// here so a provenance record is self-contained evidence.
    pub content_hash: String,
}

/// The tier-1 disclosure unit: everything an agent needs to decide whether
/// a skill is relevant, and nothing it pays a body load for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillMetadata {
    /// The skill name (registry key).
    pub name: String,

    /// The tier-1 discovery text.
    pub description: String,

    /// The 1-based revision under this name; append-only.
    pub revision: u64,

    /// The content address of the version.
    pub content_hash: String,

    /// SPDX-style license identifier, when declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,

    /// Tool names the skill's instructions assume (advisory).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_tools: Vec<String>,

    /// Environment compatibility note, when declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<String>,
}

/// Selects one version of a skill for [`SkillRegistry::get_version`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillVersionSelector {
    /// The 1-based revision assigned at registration.
    Revision(u64),
    /// The content address of the version.
    ContentHash(String),
}

/// One immutable, content-addressed skill version — the registered form of
/// a validated, scanned package.
///
/// Identity is the content hash; nothing about a `SkillVersion` mutates
/// after registration. Changed content is a different value at a new
/// revision, which is what makes the registry's latest pointer honest: it
/// selects among immutable versions, it never edits one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillVersion {
    metadata: SkillMetadata,
    provenance: SkillProvenance,
    scan: ScanReport,
    body: String,
    references: BTreeMap<String, Vec<u8>>,
    assets: BTreeMap<String, Vec<u8>>,
}

impl SkillVersion {
    /// The tier-1 projection — cheap to hold in the hundreds.
    pub fn metadata(&self) -> SkillMetadata {
        self.metadata.clone()
    }

    /// The skill name.
    pub fn name(&self) -> &str {
        &self.metadata.name
    }

    /// The 1-based revision under this name.
    pub fn revision(&self) -> u64 {
        self.metadata.revision
    }

    /// The content address of this version.
    pub fn content_hash(&self) -> &str {
        &self.metadata.content_hash
    }

    /// Where this version came from.
    pub fn provenance(&self) -> &SkillProvenance {
        &self.provenance
    }

    /// The scan report recorded at registration. Clean of denials by
    /// construction (a denial refuses registration); warnings persist here
    /// as evidence.
    pub fn scan(&self) -> &ScanReport {
        &self.scan
    }

    /// The tier-2 disclosure unit: the `SKILL.md` instructions.
    pub fn body(&self) -> &str {
        &self.body
    }

    /// The reference member paths, sorted (tier-3 enumeration).
    pub fn reference_paths(&self) -> impl Iterator<Item = &str> {
        self.references.keys().map(String::as_str)
    }

    /// The asset member paths, sorted (tier-3 enumeration).
    pub fn asset_paths(&self) -> impl Iterator<Item = &str> {
        self.assets.keys().map(String::as_str)
    }

    /// Load one reference member by path (tier-3 disclosure).
    pub fn reference(&self, path: &str) -> Option<&[u8]> {
        self.references.get(path).map(Vec::as_slice)
    }

    /// Load one asset member by path (tier-3 disclosure).
    pub fn asset(&self, path: &str) -> Option<&[u8]> {
        self.assets.get(path).map(Vec::as_slice)
    }
}

/// The outcome of [`SkillRegistry::register`]: the version the package
/// resolved to, and whether registration created it.
#[derive(Debug, Clone)]
pub struct Registration {
    /// The resolved version (shared; registry and caller hold the same
    /// immutable value).
    pub version: Arc<SkillVersion>,

    /// `true` when the content was already registered — re-registration is
    /// idempotent and returns the existing version without a new revision.
    pub already_registered: bool,
}

/// The skill registry: names to append-only version histories, with a
/// forward-only latest pointer per name.
///
/// Determinism is structural: [`SkillRegistry::list`] iterates a `BTreeMap`
/// (name-sorted), revisions are 1-based positions in an append-only vector,
/// and version identity is content, so two registries fed the same
/// registrations in the same order agree byte-for-byte — and reordered
/// registrations still agree on every name's content.
#[derive(Default)]
pub struct SkillRegistry {
    skills: BTreeMap<String, Vec<Arc<SkillVersion>>>,
}

impl std::fmt::Debug for SkillRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillRegistry")
            .field("skills", &self.skills.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl SkillRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a validated package: scan it, then version it.
    ///
    /// Fails closed on any scan denial. Identical content under an existing
    /// name returns the existing version (`already_registered`), leaving the
    /// latest pointer untouched; changed content appends a revision and
    /// moves the latest pointer to it. The pointer only ever moves forward —
    /// there is no delete and no in-place update anywhere in the model.
    pub fn register(
        &mut self,
        package: SkillPackage,
        source: SkillSource,
        author: impl Into<String>,
    ) -> Result<Registration, SkillError> {
        let author = author.into();
        if author.is_empty() || author != author.trim() {
            return Err(SkillError::InvalidAuthor {
                reason: "must be non-empty and trimmed",
            });
        }
        if author.len() > MAX_SKILL_AUTHOR_LEN {
            return Err(SkillError::InvalidAuthor {
                reason: "exceeds the 128-byte ceiling",
            });
        }
        if author.chars().any(char::is_control) {
            return Err(SkillError::InvalidAuthor {
                reason: "must not contain control characters",
            });
        }

        let content_hash = package.content_hash();
        if let Some(versions) = self.skills.get(package.name()) {
            if let Some(existing) = versions
                .iter()
                .find(|version| version.content_hash() == content_hash)
            {
                return Ok(Registration {
                    version: existing.clone(),
                    already_registered: true,
                });
            }
        }

        let scan = scan_package(&package);
        if scan.has_denials() {
            return Err(SkillError::ScanDenied {
                denials: scan.denials().cloned().collect(),
            });
        }

        let versions = self.skills.entry(package.name().to_owned()).or_default();
        let revision = versions.len() as u64 + 1;
        let frontmatter = package.frontmatter.clone();
        let version = Arc::new(SkillVersion {
            metadata: SkillMetadata {
                name: frontmatter.name,
                description: frontmatter.description,
                revision,
                content_hash: content_hash.clone(),
                license: frontmatter.license,
                allowed_tools: frontmatter.allowed_tools,
                compatibility: frontmatter.compatibility,
            },
            provenance: SkillProvenance {
                source,
                author,
                content_hash,
            },
            scan,
            body: package.body,
            references: package.references,
            assets: package.assets,
        });
        versions.push(version.clone());
        Ok(Registration {
            version,
            already_registered: false,
        })
    }

    /// The latest version of a skill — the head of its append-only history.
    pub fn get(&self, name: &str) -> Option<Arc<SkillVersion>> {
        self.skills.get(name)?.last().cloned()
    }

    /// One selected version of a skill, by revision or content hash.
    pub fn get_version(
        &self,
        name: &str,
        selector: SkillVersionSelector,
    ) -> Option<Arc<SkillVersion>> {
        let versions = self.skills.get(name)?;
        match selector {
            SkillVersionSelector::Revision(revision) => versions
                .get(revision.checked_sub(1)? as usize)
                .cloned(),
            SkillVersionSelector::ContentHash(hash) => versions
                .iter()
                .find(|version| version.content_hash() == hash)
                .cloned(),
        }
    }

    /// The tier-1 catalog: latest-version metadata for every skill,
    /// name-sorted. Metadata has no body field, so listing hundreds of
    /// skills pays for name-and-description entries only.
    pub fn list(&self) -> Vec<SkillMetadata> {
        self.skills
            .values()
            .filter_map(|versions| versions.last())
            .map(|version| version.metadata())
            .collect()
    }

    /// Every revision of a skill as metadata, ascending. The history is the
    /// audit trail: revisions are immutable, so the list is the whole truth
    /// of how the name evolved.
    pub fn history(&self, name: &str) -> Vec<SkillMetadata> {
        self.skills
            .get(name)
            .map(|versions| versions.iter().map(|version| version.metadata()).collect())
            .unwrap_or_default()
    }

    /// `true` if a skill with this name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.skills.contains_key(name)
    }

    /// All registered skill names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.skills.keys().map(String::as_str)
    }

    /// Number of registered skills (names, not versions).
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// `true` if no skills are registered.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}
