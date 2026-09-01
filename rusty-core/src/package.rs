//! The plugin packaging format: manifest, content hash, dependency ranges,
//! and capability declarations.
//!
//! Every catalog item — connector, tool pack, skill pack, blueprint template —
//! ships as one package with a declared manifest. The manifest is the
//! authoritative contract: it names what the package registers, what it
//! requires, and what it may reach. A package attempting a registration or
//! egress destination absent from its declaration is refused.
//!
//! The manifest's content hash (SHA-256 over canonical JSON of every field
//! except `hash` itself) is its durable identity. Files land in the
//! content-addressed blob store keyed by their own SHA-256, so every installed
//! version remains retrievable for rollback and audit without duplication.

use serde::{Deserialize, Serialize};

use crate::error::{Result, RustyError};
use crate::record::Effect;

/// Sandbox requirement for a tool capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxRequirement {
    None,
    Preferred,
    Required,
}

// ---------------------------------------------------------------------------
// Identity and validation
// ---------------------------------------------------------------------------

/// Maximum package id length.
pub const MAX_PACKAGE_ID_LEN: usize = 64;

/// Maximum publisher id length.
pub const MAX_PUBLISHER_ID_LEN: usize = 64;

/// Maximum display name length.
pub const MAX_PACKAGE_NAME_LEN: usize = 128;

/// Maximum version string length.
pub const MAX_VERSION_LEN: usize = 32;

/// Maximum file entries per manifest.
pub const MAX_FILE_ENTRIES: usize = 256;

/// Maximum dependency declarations per manifest.
pub const MAX_DEPENDENCIES: usize = 32;

/// Maximum capability declarations per manifest.
pub const MAX_CAPABILITIES: usize = 64;

/// Maximum path length in a file entry.
pub const MAX_FILE_PATH_LEN: usize = 512;

fn package_err(msg: impl Into<String>) -> RustyError {
    RustyError::Plugin(format!("package: {}", msg.into()))
}

/// Validate a package or publisher id: kebab-case, bounded, non-empty.
fn validate_id(id: &str, max: usize, what: &str) -> Result<()> {
    let legal = !id.is_empty()
        && id.len() <= max
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.starts_with('-')
        && !id.ends_with('-')
        && !id.contains("--");
    if legal {
        return Ok(());
    }
    Err(package_err(format!(
        "{what} `{id}` must be kebab-case (`[a-z0-9]+(-[a-z0-9]+)*`), at most {max} bytes"
    )))
}

/// A validated package identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PackageId(String);

impl PackageId {
    pub fn new(id: impl Into<String>) -> Result<Self> {
        let id = id.into();
        validate_id(&id, MAX_PACKAGE_ID_LEN, "package id")?;
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PackageId {
    type Error = RustyError;
    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl From<PackageId> for String {
    fn from(id: PackageId) -> Self {
        id.0
    }
}

impl std::fmt::Display for PackageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A validated publisher identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PublisherId(String);

impl PublisherId {
    pub fn new(id: impl Into<String>) -> Result<Self> {
        let id = id.into();
        validate_id(&id, MAX_PUBLISHER_ID_LEN, "publisher id")?;
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PublisherId {
    type Error = RustyError;
    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl From<PublisherId> for String {
    fn from(id: PublisherId) -> Self {
        id.0
    }
}

// ---------------------------------------------------------------------------
// Version
// ---------------------------------------------------------------------------

/// A simple semver-like version: `major.minor.patch` with optional
/// pre-release and build labels. Parsed strictly; no loose matching.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Option<String>,
    build: Option<String>,
}

impl Version {
    pub fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
            pre: None,
            build: None,
        }
    }

    pub fn with_pre(mut self, pre: impl Into<String>) -> Self {
        self.pre = Some(pre.into());
        self
    }

    pub fn with_build(mut self, build: impl Into<String>) -> Self {
        self.build = Some(build.into());
        self
    }

    pub fn parse(text: &str) -> Result<Self> {
        if text.len() > MAX_VERSION_LEN {
            return Err(package_err(format!(
                "version `{text}` exceeds {MAX_VERSION_LEN} bytes"
            )));
        }
        // Split build metadata (+)
        let (text, build) = text
            .split_once('+')
            .map(|(a, b)| (a, Some(b.to_string())))
            .unwrap_or((text, None));
        // Split pre-release (-)
        let (text, pre) = text
            .split_once('-')
            .map(|(a, b)| (a, Some(b.to_string())))
            .unwrap_or((text, None));

        let parts: Vec<&str> = text.split('.').collect();
        if parts.len() != 3 {
            return Err(package_err(format!(
                "version `{text}` must be `major.minor.patch`"
            )));
        }
        let major = parts[0]
            .parse()
            .map_err(|_| package_err(format!("version major `{}` is not a number", parts[0])))?;
        let minor = parts[1]
            .parse()
            .map_err(|_| package_err(format!("version minor `{}` is not a number", parts[1])))?;
        let patch = parts[2]
            .parse()
            .map_err(|_| package_err(format!("version patch `{}` is not a number", parts[2])))?;

        Ok(Self {
            major,
            minor,
            patch,
            pre,
            build,
        })
    }

    pub fn major(&self) -> u64 {
        self.major
    }
    pub fn minor(&self) -> u64 {
        self.minor
    }
    pub fn patch(&self) -> u64 {
        self.patch
    }
    pub fn pre(&self) -> Option<&str> {
        self.pre.as_deref()
    }
    pub fn build(&self) -> Option<&str> {
        self.build.as_deref()
    }
}

impl TryFrom<String> for Version {
    type Error = RustyError;
    fn try_from(value: String) -> Result<Self> {
        Self::parse(&value)
    }
}

impl From<Version> for String {
    fn from(v: Version) -> Self {
        format!("{}", v)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(pre) = &self.pre {
            write!(f, "-{pre}")?;
        }
        if let Some(build) = &self.build {
            write!(f, "+{build}")?;
        }
        Ok(())
    }
}

impl std::cmp::Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.major
            .cmp(&other.major)
            .then_with(|| self.minor.cmp(&other.minor))
            .then_with(|| self.patch.cmp(&other.patch))
            .then_with(|| cmp_pre(self.pre.as_deref(), other.pre.as_deref()))
    }
}

impl std::cmp::PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Pre-release comparison: a version with no pre-release is greater than
/// one with a pre-release (e.g., 1.0.0 > 1.0.0-alpha).
fn cmp_pre(a: Option<&str>, b: Option<&str>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(a), Some(b)) => cmp_pre_identifiers(a, b),
    }
}

/// Compare two pre-release strings dot-by-dot: numeric identifiers compare
/// as integers; alphanumeric identifiers compare lexicographically.
fn cmp_pre_identifiers(a: &str, b: &str) -> std::cmp::Ordering {
    let a_parts: Vec<&str> = a.split('.').collect();
    let b_parts: Vec<&str> = b.split('.').collect();
    for i in 0..a_parts.len().max(b_parts.len()) {
        let av = a_parts.get(i);
        let bv = b_parts.get(i);
        match (av, bv) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(a), Some(b)) => {
                let ord = match (a.parse::<u64>(), b.parse::<u64>()) {
                    (Ok(an), Ok(bn)) => an.cmp(&bn),
                    (Ok(_), Err(_)) => std::cmp::Ordering::Less, // numeric < alphanumeric
                    (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
                    (Err(_), Err(_)) => a.cmp(b),
                };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
        }
    }
    std::cmp::Ordering::Equal
}

// ---------------------------------------------------------------------------
// Package kind
// ---------------------------------------------------------------------------

/// The kind of catalog item a package contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageKind {
    /// An HTTP connector with declared operations.
    Connector,
    /// A collection of tools.
    ToolPack,
    /// A collection of skills.
    SkillPack,
    /// A blueprint template.
    BlueprintTemplate,
}

// ---------------------------------------------------------------------------
// File manifest
// ---------------------------------------------------------------------------

/// One file in the package: its path, size, and content hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Relative path inside the package (`tools/fs.rs`, `SKILL.md`).
    pub path: String,
    /// SHA-256 of the file's bytes.
    pub sha256: String,
    /// Byte length.
    pub bytes: u64,
}

impl FileEntry {
    /// Validate the entry shape.
    fn validate(&self) -> Result<()> {
        if self.path.is_empty() || self.path.len() > MAX_FILE_PATH_LEN {
            return Err(package_err(format!(
                "file path must be 1–{MAX_FILE_PATH_LEN} bytes"
            )));
        }
        if self.path.starts_with('/') || self.path.contains("..") {
            return Err(package_err(format!(
                "file path `{}` must be relative and contain no `..`",
                self.path
            )));
        }
        if self.sha256.len() != 64 {
            return Err(package_err(format!(
                "file `{}` sha256 must be 64 hex chars",
                self.path
            )));
        }
        if !self.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(package_err(format!(
                "file `{}` sha256 must be hexadecimal",
                self.path
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Dependencies
// ---------------------------------------------------------------------------

/// A declared dependency range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyRange {
    /// The package or ABI identifier this range applies to.
    pub id: String,
    /// The accepted version range as a human-readable constraint
    /// (`>=1.2.0, <2.0.0`, `^1.2.3`, `~1.2.0`).
    pub constraint: String,
}

impl DependencyRange {
    fn validate(&self) -> Result<()> {
        if self.id.is_empty() || self.id.len() > MAX_PACKAGE_ID_LEN {
            return Err(package_err("dependency id must be a valid package id"));
        }
        if self.constraint.is_empty() || self.constraint.len() > 128 {
            return Err(package_err("dependency constraint must be 1–128 bytes"));
        }
        Ok(())
    }

    /// Check whether `version` satisfies this range.
    ///
    /// Supports exact (`=1.2.3`), caret (`^1.2.3`), tilde (`~1.2.3`),
    /// greater/less-than (`>=1.2.0`, `<2.0.0`), and comma-anded ranges.
    pub fn satisfies(&self, version: &Version) -> bool {
        parse_constraint(&self.constraint).is_some_and(|c| c.satisfies(version))
    }
}

/// Internal parsed constraint for evaluation.
#[derive(Debug, Clone)]
enum Constraint {
    Exact(Version),
    Caret(Version),
    Tilde(Version),
    Gte(Version),
    Gt(Version),
    Lte(Version),
    Lt(Version),
    And(Box<Constraint>, Box<Constraint>),
}

impl Constraint {
    fn satisfies(&self, version: &Version) -> bool {
        match self {
            Constraint::Exact(v) => version == v,
            Constraint::Caret(v) => {
                version.major == v.major
                    && (version.major != 0 || version.minor == v.minor)
                    && version >= v
            }
            Constraint::Tilde(v) => {
                version.major == v.major && version.minor == v.minor && version >= v
            }
            Constraint::Gte(v) => version >= v,
            Constraint::Gt(v) => version > v,
            Constraint::Lte(v) => version <= v,
            Constraint::Lt(v) => version < v,
            Constraint::And(a, b) => a.satisfies(version) && b.satisfies(version),
        }
    }
}

fn parse_constraint(text: &str) -> Option<Constraint> {
    let text = text.trim();
    if let Some((left, right)) = text.split_once(',') {
        let a = parse_constraint(left)?;
        let b = parse_constraint(right)?;
        return Some(Constraint::And(Box::new(a), Box::new(b)));
    }
    if let Some(rest) = text.strip_prefix('=') {
        return Some(Constraint::Exact(Version::parse(rest).ok()?));
    }
    if let Some(rest) = text.strip_prefix('^') {
        return Some(Constraint::Caret(Version::parse(rest).ok()?));
    }
    if let Some(rest) = text.strip_prefix('~') {
        return Some(Constraint::Tilde(Version::parse(rest).ok()?));
    }
    if let Some(rest) = text.strip_prefix(">=") {
        return Some(Constraint::Gte(Version::parse(rest).ok()?));
    }
    if let Some(rest) = text.strip_prefix('>') {
        return Some(Constraint::Gt(Version::parse(rest).ok()?));
    }
    if let Some(rest) = text.strip_prefix("<=") {
        return Some(Constraint::Lte(Version::parse(rest).ok()?));
    }
    if let Some(rest) = text.strip_prefix('<') {
        return Some(Constraint::Lt(Version::parse(rest).ok()?));
    }
    // Bare version = exact match
    Some(Constraint::Exact(Version::parse(text).ok()?))
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

/// Declared effect class for a tool capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclaredEffect {
    Pure,
    ReadOnly,
    Idempotent,
    Compensatable,
    Write,
}

impl DeclaredEffect {
    pub fn to_runtime_effect(&self) -> Effect {
        match self {
            DeclaredEffect::Pure => Effect::Pure,
            DeclaredEffect::ReadOnly => Effect::ReadOnly,
            DeclaredEffect::Idempotent => Effect::Idempotent,
            DeclaredEffect::Compensatable => Effect::Compensatable,
            DeclaredEffect::Write => Effect::NonIdempotent,
        }
    }
}

/// One tool declared by a package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCapabilityDecl {
    pub name: String,
    pub effect: DeclaredEffect,
    pub sandbox: SandboxRequirement,
}

/// One egress destination declared by a package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressDestDecl {
    pub host: String,
    pub methods: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_patterns: Vec<String>,
}

/// One secret reference declared by a package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretRefDecl {
    pub store: String,
    pub key: String,
}

/// One REST scope required by a package surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestScopeDecl {
    pub family: String,
    pub actions: Vec<String>,
}

/// The complete capability declaration block of a package.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CapabilityDecl {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolCapabilityDecl>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backends: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blueprints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secret_refs: Vec<SecretRefDecl>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub egress: Vec<EgressDestDecl>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<RestScopeDecl>,
}

impl CapabilityDecl {
    fn validate(&self) -> Result<()> {
        if self.tool_count()
            + self.channels.len()
            + self.providers.len()
            + self.backends.len()
            + self.skills.len()
            + self.blueprints.len()
            + self.secret_refs.len()
            + self.egress.len()
            + self.scopes.len()
            > MAX_CAPABILITIES
        {
            return Err(package_err(format!(
                "capability declarations exceed {MAX_CAPABILITIES}"
            )));
        }
        for tool in &self.tools {
            if tool.name.is_empty() || tool.name.len() > 128 {
                return Err(package_err(format!(
                    "tool capability name exceeds {} bytes",
                    128
                )));
            }
        }
        for dest in &self.egress {
            if dest.host.is_empty() || dest.host.len() > 256 {
                return Err(package_err("egress host must be 1–256 bytes"));
            }
        }
        Ok(())
    }

    fn tool_count(&self) -> usize {
        self.tools.len()
    }
}

// ---------------------------------------------------------------------------
// Signature
// ---------------------------------------------------------------------------

/// A signature over a package manifest.
///
/// The signature covers the manifest's content hash. Verification uses the
/// publisher key pinned in the registry index (EP-15-S03).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageSignature {
    /// The signature bytes (Ed25519), hex-encoded.
    pub sig_hex: String,
    /// The publisher's public key that produced the signature, hex-encoded.
    pub pubkey_hex: String,
}

impl PackageSignature {
    /// Verify the signature against the manifest's content hash.
    ///
    /// Returns `Ok(())` on valid signature, `Err` on mismatch or malformed
    /// key/signature. Uses the `ed25519-dalek` crate already in the
    /// workspace for receipt signing.
    pub fn verify(&self, content_hash: &str) -> Result<()> {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};

        let pubkey_bytes = crate::broker::hex_decode(&self.pubkey_hex)
            .ok_or_else(|| package_err("pubkey hex decode failed".to_string()))?;
        let sig_bytes = crate::broker::hex_decode(&self.sig_hex)
            .ok_or_else(|| package_err("signature hex decode failed".to_string()))?;

        if pubkey_bytes.len() != 32 {
            return Err(package_err(format!(
                "pubkey must be 32 bytes, got {}",
                pubkey_bytes.len()
            )));
        }
        if sig_bytes.len() != 64 {
            return Err(package_err(format!(
                "signature must be 64 bytes, got {}",
                sig_bytes.len()
            )));
        }

        let pubkey: VerifyingKey =
            VerifyingKey::from_bytes(&pubkey_bytes.try_into().expect("length checked above"))
                .map_err(|e| package_err(format!("invalid Ed25519 pubkey: {e}")))?;
        let signature: Signature =
            Signature::from_bytes(&sig_bytes.try_into().expect("length checked above"));

        pubkey
            .verify(content_hash.as_bytes(), &signature)
            .map_err(|e| package_err(format!("signature verification failed: {e}")))?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// The canonical content view of a manifest: every field except `hash`.
#[derive(Serialize)]
struct ManifestContent<'a> {
    id: &'a PackageId,
    name: &'a str,
    kind: PackageKind,
    version: &'a Version,
    publisher: &'a PublisherId,
    files: &'a [FileEntry],
    dependencies: &'a [DependencyRange],
    capabilities: &'a CapabilityDecl,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<&'a PackageSignature>,
}

/// The plugin package manifest: the declared contract for every catalog item.
///
/// Construct through [`PackageManifest::new`] so validation and the content
/// hash happen at creation time. Deserialized manifests must call
/// [`PackageManifest::verify_hash`] before trust.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PackageManifest {
    pub id: PackageId,
    pub name: String,
    pub kind: PackageKind,
    pub version: Version,
    pub publisher: PublisherId,
    /// File manifest, sorted by path for deterministic hashing.
    pub files: Vec<FileEntry>,
    pub dependencies: Vec<DependencyRange>,
    pub capabilities: CapabilityDecl,
    pub signature: Option<PackageSignature>,
    /// SHA-256 of the canonical JSON of every field above.
    pub hash: String,
}

impl PackageManifest {
    /// Validate and construct a manifest, computing its content hash.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: PackageId,
        name: impl Into<String>,
        kind: PackageKind,
        version: Version,
        publisher: PublisherId,
        mut files: Vec<FileEntry>,
        dependencies: Vec<DependencyRange>,
        capabilities: CapabilityDecl,
        signature: Option<PackageSignature>,
    ) -> Result<Self> {
        let name = name.into();
        if name.is_empty() || name.len() > MAX_PACKAGE_NAME_LEN {
            return Err(package_err(format!(
                "package name must be 1–{MAX_PACKAGE_NAME_LEN} bytes"
            )));
        }
        if files.len() > MAX_FILE_ENTRIES {
            return Err(package_err(format!(
                "file manifest exceeds {MAX_FILE_ENTRIES} entries"
            )));
        }
        for entry in &files {
            entry.validate()?;
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));

        if dependencies.len() > MAX_DEPENDENCIES {
            return Err(package_err(format!(
                "dependency list exceeds {MAX_DEPENDENCIES} entries"
            )));
        }
        for dep in &dependencies {
            dep.validate()?;
        }

        capabilities.validate()?;

        let mut manifest = Self {
            id,
            name,
            kind,
            version,
            publisher,
            files,
            dependencies,
            capabilities,
            signature,
            hash: String::new(),
        };
        manifest.hash = manifest.compute_hash();
        Ok(manifest)
    }

    fn compute_hash(&self) -> String {
        let content = ManifestContent {
            id: &self.id,
            name: &self.name,
            kind: self.kind,
            version: &self.version,
            publisher: &self.publisher,
            files: &self.files,
            dependencies: &self.dependencies,
            capabilities: &self.capabilities,
            signature: self.signature.as_ref(),
        };
        let value =
            serde_json::to_value(&content).expect("the manifest content view always serializes");
        let canonical = crate::record::canonicalize_value(&value);
        let bytes = serde_json::to_vec(&canonical).expect("a serde_json::Value always serializes");
        crate::record::sha256_hex(&bytes)
    }

    /// `true` if the stored hash matches a recomputation over the current
    /// content.
    pub fn verify_hash(&self) -> bool {
        !self.hash.is_empty() && self.hash == self.compute_hash()
    }

    /// The content-addressed identity of this manifest.
    pub fn content_hash(&self) -> &str {
        &self.hash
    }

    /// Whether every tool the package declares is listed in its capability
    /// block. Used by the install gate to enforce declaration-before-use.
    pub fn declares_tool(&self, name: &str) -> bool {
        self.capabilities.tools.iter().any(|t| t.name == name)
    }

    /// Whether the package declares an egress destination covering `host`.
    pub fn declares_egress(&self, host: &str) -> bool {
        self.capabilities.egress.iter().any(|e| e.host == host)
    }
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// The outcome of resolving a package's dependencies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionOutcome {
    /// All dependencies satisfied.
    Satisfied,
    /// One or more dependencies could not be resolved.
    Conflicts(Vec<ResolutionConflict>),
}

/// One unresolved dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionConflict {
    pub dependency_id: String,
    pub requested: String,
    pub reason: String,
}

/// Resolve `manifest`'s dependencies against the available versions.
///
/// `available` maps package id → installed or available versions.
/// Returns [`ResolutionOutcome::Satisfied`] when every range is matched.
pub fn resolve_dependencies(
    manifest: &PackageManifest,
    available: &std::collections::HashMap<String, Vec<Version>>,
) -> ResolutionOutcome {
    let mut conflicts = Vec::new();
    for dep in &manifest.dependencies {
        let versions = match available.get(&dep.id) {
            Some(v) => v,
            None => {
                conflicts.push(ResolutionConflict {
                    dependency_id: dep.id.clone(),
                    requested: dep.constraint.clone(),
                    reason: "package not available".to_string(),
                });
                continue;
            }
        };
        if !versions.iter().any(|v| dep.satisfies(v)) {
            conflicts.push(ResolutionConflict {
                dependency_id: dep.id.clone(),
                requested: dep.constraint.clone(),
                reason: format!("no version among {:?} satisfies the range", versions),
            });
        }
    }
    if conflicts.is_empty() {
        ResolutionOutcome::Satisfied
    } else {
        ResolutionOutcome::Conflicts(conflicts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_fixture() -> PackageManifest {
        PackageManifest::new(
            PackageId::new("test-pack").unwrap(),
            "Test Pack",
            PackageKind::ToolPack,
            Version::new(1, 2, 3),
            PublisherId::new("rusty-labs").unwrap(),
            vec![
                FileEntry {
                    path: "tools/fs.rs".to_string(),
                    sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                        .to_string(),
                    bytes: 0,
                },
                FileEntry {
                    path: "tools/shell.rs".to_string(),
                    sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                        .to_string(),
                    bytes: 0,
                },
            ],
            vec![],
            CapabilityDecl::default(),
            None,
        )
        .unwrap()
    }

    #[test]
    fn manifest_constructs_and_hashes() {
        let m = manifest_fixture();
        assert_eq!(m.id.as_str(), "test-pack");
        assert_eq!(m.version.to_string(), "1.2.3");
        assert!(!m.hash.is_empty());
        assert!(m.verify_hash());
    }

    #[test]
    fn hash_changes_when_content_changes() {
        let m1 = manifest_fixture();
        let mut m2 = manifest_fixture();
        m2.name = "Different".to_string();
        m2.hash = m2.compute_hash();
        assert_ne!(m1.hash, m2.hash);
    }

    #[test]
    fn tampered_manifest_fails_verification() {
        let mut m = manifest_fixture();
        m.name = "Tampered".to_string();
        assert!(!m.verify_hash());
    }

    #[test]
    fn files_are_sorted_for_determinism() {
        let m1 = PackageManifest::new(
            PackageId::new("sort-test").unwrap(),
            "Sort",
            PackageKind::Connector,
            Version::new(1, 0, 0),
            PublisherId::new("p").unwrap(),
            vec![
                FileEntry {
                    path: "z".to_string(),
                    sha256: "a".repeat(64),
                    bytes: 1,
                },
                FileEntry {
                    path: "a".to_string(),
                    sha256: "b".repeat(64),
                    bytes: 2,
                },
            ],
            vec![],
            CapabilityDecl::default(),
            None,
        )
        .unwrap();
        assert_eq!(m1.files[0].path, "a");
        assert_eq!(m1.files[1].path, "z");
    }

    #[test]
    fn invalid_package_id_rejected() {
        assert!(PackageId::new("").is_err());
        assert!(PackageId::new("UPPER").is_err());
        assert!(PackageId::new("double--dash").is_err());
        assert!(PackageId::new("-leading").is_err());
        assert!(PackageId::new("trailing-").is_err());
    }

    #[test]
    fn version_parsing() {
        let v = Version::parse("1.2.3").unwrap();
        assert_eq!((v.major(), v.minor(), v.patch()), (1, 2, 3));

        let v = Version::parse("0.1.0-alpha").unwrap();
        assert_eq!(v.pre(), Some("alpha"));

        let v = Version::parse("2.0.0+build.42").unwrap();
        assert_eq!(v.build(), Some("build.42"));
    }

    #[test]
    fn version_ordering() {
        assert!(Version::parse("1.0.0").unwrap() > Version::parse("0.9.9").unwrap());
        assert!(Version::parse("1.0.0").unwrap() > Version::parse("1.0.0-alpha").unwrap());
        assert!(Version::parse("1.0.0-alpha").unwrap() < Version::parse("1.0.0-beta").unwrap());
        assert!(Version::parse("1.0.0-1").unwrap() < Version::parse("1.0.0-2").unwrap());
    }

    #[test]
    fn dependency_satisfies_exact() {
        let d = DependencyRange {
            id: "x".to_string(),
            constraint: "=1.2.3".to_string(),
        };
        assert!(d.satisfies(&Version::new(1, 2, 3)));
        assert!(!d.satisfies(&Version::new(1, 2, 4)));
    }

    #[test]
    fn dependency_satisfies_caret() {
        let d = DependencyRange {
            id: "x".to_string(),
            constraint: "^1.2.0".to_string(),
        };
        assert!(d.satisfies(&Version::new(1, 2, 0)));
        assert!(d.satisfies(&Version::new(1, 3, 0)));
        assert!(!d.satisfies(&Version::new(2, 0, 0)));
    }

    #[test]
    fn dependency_satisfies_range() {
        let d = DependencyRange {
            id: "x".to_string(),
            constraint: ">=1.2.0, <2.0.0".to_string(),
        };
        assert!(d.satisfies(&Version::new(1, 2, 0)));
        assert!(d.satisfies(&Version::new(1, 5, 0)));
        assert!(!d.satisfies(&Version::new(2, 0, 0)));
    }

    #[test]
    fn resolution_satisfied() {
        let m = manifest_fixture();
        let mut available = std::collections::HashMap::new();
        available.insert("test-pack".to_string(), vec![Version::new(1, 2, 3)]);
        assert_eq!(
            resolve_dependencies(&m, &available),
            ResolutionOutcome::Satisfied
        );
    }

    #[test]
    fn resolution_conflict_missing_package() {
        let mut m = manifest_fixture();
        m.dependencies.push(DependencyRange {
            id: "missing".to_string(),
            constraint: ">=1.0.0".to_string(),
        });
        let available = std::collections::HashMap::new();
        let outcome = resolve_dependencies(&m, &available);
        assert!(
            matches!(outcome, ResolutionOutcome::Conflicts(ref c) if c[0].dependency_id == "missing")
        );
    }

    #[test]
    fn resolution_conflict_unsatisfiable_range() {
        let mut m = manifest_fixture();
        m.dependencies.push(DependencyRange {
            id: "other".to_string(),
            constraint: ">=2.0.0".to_string(),
        });
        let mut available = std::collections::HashMap::new();
        available.insert("other".to_string(), vec![Version::new(1, 0, 0)]);
        let outcome = resolve_dependencies(&m, &available);
        assert!(
            matches!(outcome, ResolutionOutcome::Conflicts(ref c) if c[0].dependency_id == "other")
        );
    }

    #[test]
    fn declares_tool_and_egress() {
        let mut m = manifest_fixture();
        m.capabilities.tools.push(ToolCapabilityDecl {
            name: "fs_read".to_string(),
            effect: DeclaredEffect::ReadOnly,
            sandbox: SandboxRequirement::None,
        });
        m.capabilities.egress.push(EgressDestDecl {
            host: "api.example.com".to_string(),
            methods: vec!["GET".to_string()],
            path_patterns: vec!["/v1/*".to_string()],
        });
        assert!(m.declares_tool("fs_read"));
        assert!(!m.declares_tool("missing"));
        assert!(m.declares_egress("api.example.com"));
        assert!(!m.declares_egress("other.com"));
    }

    #[test]
    fn serde_round_trip() {
        let m1 = manifest_fixture();
        let json = serde_json::to_string(&m1).unwrap();
        let m2: PackageManifest = serde_json::from_str(&json).unwrap();
        // After round-trip the hash is preserved; verify it still matches.
        assert!(m2.verify_hash());
        assert_eq!(m1.id, m2.id);
        assert_eq!(m1.version, m2.version);
        assert_eq!(m1.files, m2.files);
    }
}
