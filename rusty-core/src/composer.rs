//! The composer plane: governed self-extension — the tools through which an
//! agent drafts new skills and tool definitions *by itself*, safely.
//!
//! Composition and publication are deliberately separate rungs, mirroring
//! the split the rest of the harness already draws between context and
//! action:
//!
//! 1. **Draft** ([`ComposeSkillTool`], [`ComposeToolDefinitionTool`] —
//!    [`Effect::Pure`]): assemble the proposed package or definition, run
//!    it through the *existing* validators — [`SkillPackage`]'s fail-closed
//!    construction and [`scan_package`] for skills, the tool-contract rules
//!    of [`crate::tool`] and the bounded-text rules of
//!    [`crate::connector::manifest`] for tool definitions — and return a
//!    structured [`DraftReceipt`]: machine-readable findings, the content
//!    hash, and suggested revision notes. A draft is evidence, never an
//!    action; nothing here registers anything.
//! 2. **Publish** ([`PublishComposedSkillTool`] —
//!    [`Effect::NonIdempotent`], the typed [`IrreversibleEffect`]
//!    [`PublishSkillEffect`]): writing into the shared
//!    [`crate::skill::SkillRegistry`] crosses a trust boundary, so it runs
//!    only behind an [`ApprovalToken`] scoped to the draft's derived effect
//!    id, checked through [`admit_irreversible`]. Only drafts that passed
//!    validation *and* scan in this session's [`ComposerSession`] draft
//!    store can publish — denied drafts are never stored, so publishing a
//!    refused content hash fails closed as an unknown draft.
//!
//! # The self-check loop
//!
//! Every compose call runs the full validator chain and reports what it
//! found; the receipt's `suggested_revision_notes` translate refusals into
//! the vocabulary the composing agent can act on, so a denied draft is a
//! correction, not a dead end. The composer never weakens a rule to make a
//! draft pass: it reuses the skill plane's and connector plane's
//! validators, so a package the composer accepts is one
//! [`SkillRegistry::register`] accepts.
//!
//! # The publish seam for tool definitions
//!
//! Skill publishing lands here because the skill registry is an in-process
//! value the composer can be handed. Tool definitions are different: their
//! door is the connector plane's manifest admission
//! ([`crate::connector::registry::ConnectorRegistry::register_manifest`]),
//! which an operator reaches by wrapping drafts into a
//! [`crate::connector::manifest::ConnectorManifest`]. That wrap is
//! deliberately out of this slice — [`ComposeToolDefinitionTool`] receipts
//! name the seam so callers route there instead of expecting the composer
//! to cross it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};

use crate::connector::canonical_json_hash;
use crate::connector::manifest::{validate_text_field, MAX_ARG_LEN, MAX_ARGS, MAX_COMMAND_LEN};
use crate::effects::{
    admit_irreversible, ApprovalToken, EffectId, IrreversibleEffect, TypedEffect,
};
use crate::error::{Result, RustyError};
use crate::record::{Effect, PayloadRef};
use crate::skill::{
    scan_package, ScanFinding, ScanKind, ScanReport, ScanSeverity, SkillError, SkillPackage,
    SkillRegistry, SkillSource,
};
use crate::tool::{validate_tool_contract, Tool};

/// The provenance name recorded for composer-published skills: the registry
/// that produced them is this plane.
pub const COMPOSER_SOURCE_NAME: &str = "composer";

/// The stable effect kind of [`PublishSkillEffect`] — part of the derived
/// effect id, so it must not change between deployments that share
/// approvals.
pub const PUBLISH_SKILL_EFFECT_KIND: &str = "publish_composed_skill";

/// The most reference members one composed skill may carry. The package's
/// own byte ceilings still apply; this bound keeps one compose call from
/// spending its whole budget on member bookkeeping.
pub const MAX_COMPOSED_REFERENCES: usize = 16;

/// The most drafts a session store holds. The store is scratch space for
/// one composition session, not a registry; the bound keeps a loop of
/// compose calls from growing without limit.
pub const MAX_SESSION_DRAFTS: usize = 256;

/// The largest a `template` recipe body may be, in bytes.
pub const MAX_RECIPE_TEMPLATE_BYTES: usize = 8 * 1024;

/// The largest a recipe path template may be, in bytes.
pub const MAX_RECIPE_PATH_BYTES: usize = 1024;

/// The HTTP methods a drafted `http` recipe may declare. Closed set: the
/// recipe is data, and the set is what the connector plane's bounded HTTP
/// surface can honor.
pub const RECIPE_HTTP_METHODS: [&str; 5] = ["GET", "POST", "PUT", "PATCH", "DELETE"];

/// One machine-readable observation from a compose call. Findings never
/// echo the offending content: `detail` names the rule and the location,
/// matching the skill plane's [`ScanFinding`] discipline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DraftFinding {
    /// Which stage of the pipeline reported it (`validation` or `scan`).
    pub stage: &'static str,
    /// `denial` refuses the draft; `warning` travels with it as evidence.
    pub severity: &'static str,
    /// The rule kind — a [`ScanKind`] for scan findings, the refused
    /// validation rule otherwise.
    pub kind: String,
    /// The package member or draft field the finding belongs to.
    pub location: String,
    /// Human-readable detail (rule text, offsets, counts).
    pub detail: String,
}

/// The structured outcome of a compose call. `valid` is the gate: only a
/// valid draft enters the session store and only a stored draft can
/// publish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DraftReceipt {
    /// `true` when the draft passed every validator and was stored.
    pub valid: bool,
    /// The content address of the validated draft. Present for scan-denied
    /// skill drafts too — the package parsed, so its hash is nameable — but
    /// a denied hash is never stored, so publishing it fails closed.
    pub content_hash: Option<String>,
    /// Every finding, in pipeline order.
    pub findings: Vec<DraftFinding>,
    /// Actionable corrections for the composing agent, one per distinct
    /// problem the findings name.
    pub suggested_revision_notes: Vec<String>,
}

/// The structured outcome of publishing one composed skill: the registry's
/// real [`crate::skill::Registration`] projected into a receipt, plus the
/// approval evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PublishReceipt {
    /// The registered skill name.
    pub name: String,
    /// The 1-based revision assigned under that name.
    pub revision: u64,
    /// The content address of the registered version.
    pub content_hash: String,
    /// `true` when identical content was already registered — re-publish is
    /// idempotent and returns the existing version without a new revision.
    pub already_registered: bool,
    /// Who approved the publish (from the consumed token).
    pub approved_by: String,
    /// Scan warnings recorded with the version (denials cannot exist here —
    /// a denied draft was never stored).
    pub warnings: usize,
}

/// A validated skill draft held by the session store: the package, its
/// clean-of-denials scan report, and the declared author.
struct SkillDraft {
    package: SkillPackage,
    scan: ScanReport,
    author: String,
}

/// One composition session: the scope publish effect ids derive in, and
/// the draft store keyed by content hash.
///
/// The store only ever holds drafts that passed validation *and* scan —
/// the fail-closed half of the publish contract lives here: a hash that is
/// not in this store is not publishable, whatever the caller claims about
/// having composed it.
pub struct ComposerSession {
    scope: String,
    drafts: Mutex<BTreeMap<String, SkillDraft>>,
}

impl std::fmt::Debug for ComposerSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let drafts = self.drafts.lock().unwrap_or_else(|e| e.into_inner());
        f.debug_struct("ComposerSession")
            .field("scope", &self.scope)
            .field("drafts", &drafts.len())
            .finish()
    }
}

impl ComposerSession {
    /// Start a session whose publish effect ids derive in `scope` (usually
    /// the run or thread id — the same value the approval minter uses).
    pub fn new(scope: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            scope: scope.into(),
            drafts: Mutex::new(BTreeMap::new()),
        })
    }

    /// The scope publish effect ids derive in.
    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// Store a validated, scan-clean draft. Fails closed at the session
    /// bound rather than evicting: an evicted draft publishing later would
    /// be a draft nobody re-validated.
    fn store_draft(&self, draft: SkillDraft) -> Result<()> {
        let mut drafts = self.drafts.lock().unwrap_or_else(|e| e.into_inner());
        let hash = draft.package.content_hash();
        if !drafts.contains_key(&hash) && drafts.len() >= MAX_SESSION_DRAFTS {
            return Err(RustyError::Tool(format!(
                "composer session holds {MAX_SESSION_DRAFTS} drafts, the session bound"
            )));
        }
        drafts.insert(hash, draft);
        Ok(())
    }

    /// A stored draft by content hash.
    fn draft(&self, content_hash: &str) -> Option<SkillDraft> {
        self.drafts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(content_hash)
            .map(|draft| SkillDraft {
                package: draft.package.clone(),
                scan: draft.scan.clone(),
                author: draft.author.clone(),
            })
    }

    /// The number of stored drafts.
    pub fn draft_count(&self) -> usize {
        self.drafts.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// The typed irreversible effect of publishing one composed skill into a
/// shared registry. Its input is the draft's content hash, so the derived
/// effect id — and therefore the approval token — is scoped to exactly one
/// draft, never to "publishing in general".
pub struct PublishSkillEffect {
    content_hash: String,
    input_hash: String,
}

impl PublishSkillEffect {
    /// Declare the publish of the draft addressed by `content_hash`.
    pub fn new(content_hash: impl Into<String>) -> Self {
        let content_hash = content_hash.into();
        let input_hash = PayloadRef::inline(json!({"content_hash": content_hash}))
            .content_hash()
            .expect("a serde_json::Value always serializes");
        Self {
            content_hash,
            input_hash,
        }
    }
    /// The content address of the draft this publish commits.
    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }
}

impl TypedEffect for PublishSkillEffect {
    const EFFECT: Effect = Effect::NonIdempotent;

    fn kind(&self) -> &str {
        PUBLISH_SKILL_EFFECT_KIND
    }

    fn input_hash(&self) -> &str {
        &self.input_hash
    }
}

impl IrreversibleEffect for PublishSkillEffect {}

/// Derive the effect id an approval token must be scoped to in order to
/// publish the draft addressed by `content_hash` within `scope`. Operators
/// mint tokens against this id; [`PublishComposedSkillTool`] re-derives it
/// at admission, so a token for one draft can never launder another.
pub fn publish_effect_id(scope: &str, content_hash: &str) -> EffectId {
    PublishSkillEffect::new(content_hash).effect_id(scope)
}

/// `compose_skill` — assemble a skill package from parts, validate it
/// through [`SkillPackage`], scan it through [`scan_package`], and return a
/// [`DraftReceipt`]. Valid drafts enter the session store under their
/// content hash; nothing is registered by this tool, ever.
///
/// [`Effect::Pure`]: the receipt is a deterministic function of the input,
/// and the session store is content-addressed scratch — re-running the same
/// compose writes the same entry.
pub struct ComposeSkillTool {
    session: Arc<ComposerSession>,
}

impl std::fmt::Debug for ComposeSkillTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComposeSkillTool")
            .field("session", &self.session)
            .finish()
    }
}

impl ComposeSkillTool {
    /// A composing tool bound to `session`.
    pub fn new(session: Arc<ComposerSession>) -> Self {
        Self { session }
    }
}

#[async_trait]
impl Tool for ComposeSkillTool {
    fn name(&self) -> &str {
        "compose_skill"
    }

    fn description(&self) -> &str {
        "Draft a new skill package from a name, description, body, and optional references; \
         returns a validation receipt with a content hash. Publishing is a separate \
         approval-gated step via publish_composed_skill."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "maxLength": 64},
                "description": {"type": "string", "maxLength": 1024},
                "body": {"type": "string", "maxLength": 262144},
                "references": {
                    "type": "object",
                    "additionalProperties": {"type": "string"},
                    "maxProperties": MAX_COMPOSED_REFERENCES
                },
                "author": {"type": "string", "maxLength": 128}
            },
            "required": ["name", "description", "body", "author"],
            "additionalProperties": false
        })
    }

    fn effect(&self) -> Effect {
        Effect::Pure
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let name = required_string(&args, "name")?;
        let description = required_string(&args, "description")?;
        let body = required_string(&args, "body")?;
        let author = required_string(&args, "author")?;

        let mut files = BTreeMap::new();
        let skill_md = format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n");
        files.insert("SKILL.md".to_owned(), skill_md.into_bytes());
        if let Some(references) = args.get("references") {
            let references = references.as_object().ok_or_else(|| {
                RustyError::Tool("`references` must be an object mapping paths to content".into())
            })?;
            if references.len() > MAX_COMPOSED_REFERENCES {
                return Err(RustyError::Tool(format!(
                    "compose_skill accepts at most {MAX_COMPOSED_REFERENCES} references"
                )));
            }
            for (path, content) in references {
                let content = content.as_str().ok_or_else(|| {
                    RustyError::Tool(format!("reference `{path}` content must be a string"))
                })?;
                let path = if path.starts_with("references/") {
                    path.clone()
                } else {
                    format!("references/{path}")
                };
                files.insert(path, content.as_bytes().to_vec());
            }
        }

        let package = match SkillPackage::from_files(files) {
            Ok(package) => package,
            Err(error) => {
                return receipt_json(DraftReceipt {
                    valid: false,
                    content_hash: None,
                    findings: vec![validation_finding(&error)],
                    suggested_revision_notes: revision_notes_for_error(&error),
                })
            }
        };

        let scan = scan_package(&package);
        let content_hash = package.content_hash();
        if scan.has_denials() {
            return receipt_json(DraftReceipt {
                valid: false,
                content_hash: Some(content_hash),
                findings: scan.findings.iter().map(scan_finding).collect(),
                suggested_revision_notes: revision_notes_for_scan(&scan),
            });
        }

        self.session.store_draft(SkillDraft {
            package,
            scan: scan.clone(),
            author: author.to_owned(),
        })?;
        receipt_json(DraftReceipt {
            valid: true,
            content_hash: Some(content_hash),
            findings: scan.warnings().map(scan_finding).collect(),
            suggested_revision_notes: revision_notes_for_scan(&scan),
        })
    }
}

/// `publish_composed_skill` — the approval-gated second rung: register a
/// session draft into the shared [`SkillRegistry`].
///
/// [`Effect::NonIdempotent`] with the typed [`PublishSkillEffect`] checked
/// through [`admit_irreversible`]: the call requires an approval token
/// scoped to the draft's derived effect id (mint it against
/// [`publish_effect_id`]). Fail-closed in both directions — an unknown or
/// denied draft hash refuses, and so does a missing or mis-scoped token.
pub struct PublishComposedSkillTool {
    session: Arc<ComposerSession>,
    registry: Arc<Mutex<SkillRegistry>>,
}

impl std::fmt::Debug for PublishComposedSkillTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublishComposedSkillTool")
            .field("session", &self.session)
            .finish()
    }
}

impl PublishComposedSkillTool {
    /// A publishing tool bound to `session` and writing into `registry`.
    pub fn new(session: Arc<ComposerSession>, registry: Arc<Mutex<SkillRegistry>>) -> Self {
        Self { session, registry }
    }
}

#[async_trait]
impl Tool for PublishComposedSkillTool {
    fn name(&self) -> &str {
        "publish_composed_skill"
    }

    fn description(&self) -> &str {
        "Register a validated compose_skill draft into the shared skill registry. \
         Requires an approval token scoped to the draft's publish effect id."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content_hash": {"type": "string", "minLength": 64, "maxLength": 64},
                "approval": {
                    "type": "object",
                    "properties": {
                        "effect_id": {"type": "string"},
                        "approved_by": {"type": "string"}
                    },
                    "required": ["effect_id", "approved_by"],
                    "additionalProperties": false
                }
            },
            "required": ["content_hash", "approval"],
            "additionalProperties": false
        })
    }

    fn effect(&self) -> Effect {
        Effect::NonIdempotent
    }

    fn effect_kind(&self) -> &str {
        PUBLISH_SKILL_EFFECT_KIND
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let content_hash = required_string(&args, "content_hash")?;
        let draft = self.session.draft(content_hash).ok_or_else(|| {
            RustyError::Tool(format!(
                "unknown draft `{content_hash}`: compose it with compose_skill first — \
                 drafts that failed validation or scan are never stored"
            ))
        })?;

        let approval_value = args.get("approval").cloned().ok_or_else(|| {
            RustyError::Tool("`approval` must name the scoped approval token".into())
        })?;
        let token: ApprovalToken = serde_json::from_value(approval_value).map_err(|error| {
            RustyError::Tool(format!("`approval` is not an approval token: {error}"))
        })?;
        let effect = PublishSkillEffect::new(content_hash);
        admit_irreversible(&effect, self.session.scope(), Some(&token)).map_err(|violation| {
            RustyError::Tool(format!("publish admission denied: {violation}"))
        })?;

        let registration = self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .register(
                draft.package,
                SkillSource::Registry {
                    name: COMPOSER_SOURCE_NAME.to_owned(),
                },
                draft.author,
            )
            .map_err(|error| RustyError::Tool(format!("registry refused the draft: {error}")))?;

        receipt_json(PublishReceipt {
            name: registration.version.name().to_owned(),
            revision: registration.version.revision(),
            content_hash: registration.version.content_hash().to_owned(),
            already_registered: registration.already_registered,
            approved_by: token.approved_by().to_owned(),
            warnings: draft.scan.warnings().count(),
        })
    }
}

/// `compose_tool_definition` — validate a proposed tool definition and
/// return a draft receipt. The definition is *data*, not code: the
/// `recipe` field is a bounded declarative form (`template` transform,
/// `http` call against a path template, or `cli` call against an
/// allowlisted command), never arbitrary executable content.
///
/// Validation reuses the planes that own the rules: the tool contract of
/// [`crate::tool`] for name, description, and parameter schema, and the
/// bounded-text rules of [`crate::connector::manifest`] for recipe fields.
/// Publishing is out of this slice by design — the receipt names the seam
/// (wrap the draft into a connector manifest; its registry admission is
/// the door).
///
/// [`Effect::Pure`]: a deterministic function of the input with no draft
/// store — there is nothing here to publish.
pub struct ComposeToolDefinitionTool {
    cli_allowlist: Vec<String>,
}

impl std::fmt::Debug for ComposeToolDefinitionTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComposeToolDefinitionTool")
            .field("cli_allowlist", &self.cli_allowlist)
            .finish()
    }
}

/// The publish seam every tool-definition receipt names.
const TOOL_DEFINITION_PUBLISH_SEAM: &str =
    "tool definition drafts do not publish through the composer: wrap the draft into a \
     ConnectorManifest and admit it through the connector plane's manifest registration";

impl ComposeToolDefinitionTool {
    /// A drafting tool whose `cli` recipes may name only commands in
    /// `cli_allowlist`. An empty allowlist refuses every `cli` recipe — the
    /// fail-closed default.
    pub fn new(cli_allowlist: Vec<String>) -> Result<Self> {
        for command in &cli_allowlist {
            validate_text_field("cli allowlist entry", command, MAX_COMMAND_LEN, false)
                .map_err(|error| RustyError::Tool(error.to_string()))?;
        }
        Ok(Self { cli_allowlist })
    }

    /// The commands a `cli` recipe may name.
    pub fn cli_allowlist(&self) -> &[String] {
        &self.cli_allowlist
    }
}

#[async_trait]
impl Tool for ComposeToolDefinitionTool {
    fn name(&self) -> &str {
        "compose_tool_definition"
    }

    fn description(&self) -> &str {
        "Draft a tool definition (name, description, parameter schema, effect class, and a \
         bounded declarative recipe: template transform, http path-template call, or \
         cli allowlisted call) and return a validation receipt. Publishing routes through \
         the connector plane's manifest registration, not the composer."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "maxLength": 128},
                "description": {"type": "string", "maxLength": 4096},
                "parameters_schema": {"type": "object"},
                "effect": {
                    "type": "string",
                    "enum": ["pure", "read_only", "idempotent", "compensatable", "non_idempotent"]
                },
                "recipe": {"type": "object"}
            },
            "required": ["name", "description", "parameters_schema", "effect", "recipe"],
            "additionalProperties": false
        })
    }

    fn effect(&self) -> Effect {
        Effect::Pure
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let name = required_string(&args, "name")?;
        let description = required_string(&args, "description")?;
        let schema = args.get("parameters_schema").cloned().ok_or_else(|| {
            RustyError::Tool("`parameters_schema` must be a JSON object".into())
        })?;
        let effect_name = required_string(&args, "effect")?;
        let recipe = args.get("recipe").cloned().ok_or_else(|| {
            RustyError::Tool("`recipe` must be a declarative recipe object".into())
        })?;

        let mut findings = Vec::new();
        if let Err(error) = validate_tool_contract(name, description, &schema) {
            findings.push(tool_finding("tool_contract", "definition", error.to_string()));
        }
        let effect: Option<Effect> =
            match serde_json::from_value(Value::String(effect_name.to_owned())) {
                Ok(effect) => Some(effect),
                Err(_) => {
                    findings.push(tool_finding(
                        "effect_classification",
                        "effect",
                        format!(
                            "effect `{effect_name}` is not a declared class (pure, read_only, \
                             idempotent, compensatable, non_idempotent)"
                        ),
                    ));
                    None
                }
            };
        if let Err(error) = validate_recipe(&recipe, &schema, &self.cli_allowlist) {
            findings.push(tool_finding("recipe", "recipe", error));
        }

        let valid = findings.is_empty();
        let content_hash = valid.then(|| {
            canonical_json_hash(&json!({
                "name": name,
                "description": description,
                "parameters_schema": schema,
                "effect": effect,
                "recipe": recipe,
            }))
        });
        let notes: Vec<String> = findings
            .iter()
            .map(|finding| format!("{}: {}", finding.location, finding.detail))
            .collect();
        receipt_json(json!({
            "valid": valid,
            "content_hash": content_hash,
            "findings": findings,
            "suggested_revision_notes": notes,
            "publish_seam": TOOL_DEFINITION_PUBLISH_SEAM,
        }))
    }
}

// --------------------------------------------------------------------- //
// Recipe validation
// --------------------------------------------------------------------- //

/// Validate one declarative recipe against the parameter schema it binds
/// to. Every placeholder `{param}` in a template, path, or argument must
/// name a declared schema property — a recipe that references undeclared
/// input is a definition the connector plane could never honor.
fn validate_recipe(
    recipe: &Value,
    parameters_schema: &Value,
    cli_allowlist: &[String],
) -> std::result::Result<(), String> {
    let kind = recipe
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "recipe must name a `kind`: template, http, or cli".to_owned())?;
    let declared: Vec<&str> = parameters_schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().map(String::as_str).collect())
        .unwrap_or_default();
    match kind {
        "template" => {
            let template = recipe
                .get("template")
                .and_then(Value::as_str)
                .ok_or_else(|| "a template recipe must carry a `template` string".to_owned())?;
            validate_text_field("recipe template", template, MAX_RECIPE_TEMPLATE_BYTES, false)
                .map_err(|error| error.to_string())?;
            if template.chars().any(|ch| ch.is_control() && ch != '\n' && ch != '\t') {
                return Err(
                    "recipe template must not contain control characters other than newline and tab"
                        .to_owned(),
                );
            }
            check_placeholders(template, &declared)
        }
        "http" => {
            let method = recipe
                .get("method")
                .and_then(Value::as_str)
                .ok_or_else(|| "an http recipe must carry a `method` string".to_owned())?;
            if !RECIPE_HTTP_METHODS.contains(&method) {
                return Err(format!(
                    "http recipe method `{method}` is not in the closed set {RECIPE_HTTP_METHODS:?}"
                ));
            }
            let path = recipe
                .get("path_template")
                .and_then(Value::as_str)
                .ok_or_else(|| "an http recipe must carry a `path_template` string".to_owned())?;
            if !path.starts_with('/') {
                return Err("http recipe path_template must start with `/`".to_owned());
            }
            if path.len() > MAX_RECIPE_PATH_BYTES
                || path.chars().any(char::is_control)
                || path.contains(char::is_whitespace)
            {
                return Err(format!(
                    "http recipe path_template must be whitespace- and control-free and at \
                     most {MAX_RECIPE_PATH_BYTES} bytes"
                ));
            }
            check_placeholders(path, &declared)
        }
        "cli" => {
            let command = recipe
                .get("command")
                .and_then(Value::as_str)
                .ok_or_else(|| "a cli recipe must carry a `command` string".to_owned())?;
            validate_text_field("recipe command", command, MAX_COMMAND_LEN, false)
                .map_err(|error| error.to_string())?;
            if !cli_allowlist.iter().any(|allowed| allowed == command) {
                return Err(format!(
                    "cli recipe command `{command}` is not in the tool's allowlist"
                ));
            }
            let empty = Vec::new();
            let args = recipe
                .get("args_template")
                .and_then(Value::as_array)
                .unwrap_or(&empty);
            if args.len() > MAX_ARGS {
                return Err(format!(
                    "cli recipe declares {} argument templates, above the {MAX_ARGS} cap",
                    args.len()
                ));
            }
            for arg in args {
                let arg = arg
                    .as_str()
                    .ok_or_else(|| "cli recipe args_template entries must be strings".to_owned())?;
                validate_text_field("recipe argument", arg, MAX_ARG_LEN, true)
                    .map_err(|error| error.to_string())?;
                check_placeholders(arg, &declared)?;
            }
            Ok(())
        }
        other => Err(format!(
            "recipe kind `{other}` is not supported: the recipe is data — template, http, \
             or cli, never arbitrary code"
        )),
    }
}

/// Refuse placeholders `{name}` that the parameter schema does not declare.
fn check_placeholders(template: &str, declared: &[&str]) -> std::result::Result<(), String> {
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            return Err("template has an unclosed `{` placeholder".to_owned());
        };
        let name = &after[..close];
        if name.is_empty() || !declared.contains(&name) {
            return Err(format!(
                "placeholder `{name}` is not a declared parameter of the definition's schema"
            ));
        }
        rest = &after[close + 1..];
    }
    Ok(())
}

// --------------------------------------------------------------------- //
// Findings and revision notes
// --------------------------------------------------------------------- //

/// Project a validation refusal into a draft finding. Validation failures
/// are always denials: an invalid draft is never stored.
fn validation_finding(error: &SkillError) -> DraftFinding {
    DraftFinding {
        stage: "validation",
        severity: "denial",
        kind: "package_validation".to_owned(),
        location: "SKILL.md".to_owned(),
        detail: error.to_string(),
    }
}

/// Project a [`ScanFinding`] into the draft receipt's shape.
fn scan_finding(finding: &ScanFinding) -> DraftFinding {
    DraftFinding {
        stage: "scan",
        severity: match finding.severity {
            ScanSeverity::Warning => "warning",
            ScanSeverity::Denial => "denial",
        },
        kind: serde_json::to_value(finding.kind)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{:?}", finding.kind)),
        location: finding.location.clone(),
        detail: finding.detail.clone(),
    }
}

/// Project a tool-definition refusal into a draft finding.
fn tool_finding(kind: &str, location: &str, detail: String) -> DraftFinding {
    DraftFinding {
        stage: "validation",
        severity: "denial",
        kind: kind.to_owned(),
        location: location.to_owned(),
        detail,
    }
}

/// Translate a package validation refusal into the correction a composing
/// agent can act on.
fn revision_notes_for_error(error: &SkillError) -> Vec<String> {
    let note = match error {
        SkillError::InvalidName { .. } => {
            "choose a kebab-case name: lowercase ASCII letters, digits, and single interior \
             hyphens, at most 64 bytes"
        }
        SkillError::InvalidDescription { .. } => {
            "write a non-empty, trimmed, control-free description of at most 1024 bytes"
        }
        SkillError::InvalidField { field, .. } => {
            return vec![format!("fix the frontmatter field `{field}`: it broke a declared rule")]
        }
        SkillError::MissingFrontmatter | SkillError::MalformedFrontmatter { .. } => {
            "keep the assembled frontmatter to the flat `key: value` subset"
        }
        SkillError::InvalidUtf8 => "write the skill in valid UTF-8",
        SkillError::EmptyBody => "add instructions to the body — a skill without them is not a skill",
        SkillError::BodyTooLarge { max, .. } => {
            return vec![format!("trim the body below the {max}-byte ceiling")]
        }
        SkillError::FileTooLarge { path, max, .. } => {
            return vec![format!("trim `{path}` below the {max}-byte per-file ceiling")]
        }
        SkillError::PackageTooLarge { max, .. } => {
            return vec![format!("trim the package below the {max}-byte ceiling")]
        }
        SkillError::InvalidPath { .. } => {
            "keep reference members beneath `references/` with clean, relative, \
             forward-separated paths"
        }
        SkillError::Io { .. } => "retry with readable package members",
        SkillError::InvalidAuthor { .. } => {
            "provide a non-empty, trimmed, control-free author of at most 128 bytes"
        }
        SkillError::ScanDenied { .. } => {
            return vec!["remove the content the security scan denied".to_owned()]
        }
    };
    vec![note.to_owned()]
}

/// Translate scan findings into corrections, one per distinct kind present.
fn revision_notes_for_scan(scan: &ScanReport) -> Vec<String> {
    let mut notes = Vec::new();
    let mut note = |kind: ScanKind, text: String| {
        if scan.findings.iter().any(|finding| finding.kind == kind) && !notes.contains(&text) {
            notes.push(text);
        }
    };
    for finding in &scan.findings {
        let text = match finding.kind {
            ScanKind::EmbeddedScript => {
                format!("remove the embedded `<script` tag from `{}`", finding.location)
            }
            ScanKind::CredentialedUrl => format!(
                "remove the credentialed URL from `{}` — credentials never belong in skill content",
                finding.location
            ),
            ScanKind::Base64Blob => format!(
                "replace the base64 blob in `{}` with an asset member or plain text",
                finding.location
            ),
        };
        note(finding.kind, text);
    }
    notes
}

// --------------------------------------------------------------------- //
// Shared helpers
// --------------------------------------------------------------------- //

fn required_string<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| RustyError::Tool(format!("`{name}` must be a string")))
}

fn receipt_json(receipt: impl Serialize) -> Result<Value> {
    serde_json::to_value(receipt)
        .map_err(|error| RustyError::Tool(format!("composer receipt did not serialize: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session() -> Arc<ComposerSession> {
        ComposerSession::new("test-scope")
    }

    fn valid_args() -> Value {
        json!({
            "name": "release-notes",
            "description": "Draft release notes from a changelog.",
            "body": "# Release Notes\n\nSummarize each merged change.\n",
            "author": "agent:rusty"
        })
    }

    #[tokio::test]
    async fn compose_valid_skill_stores_a_publishable_draft() {
        let session = session();
        let tool = ComposeSkillTool::new(Arc::clone(&session));
        assert_eq!(tool.effect(), Effect::Pure);

        let receipt = tool.call(valid_args()).await.unwrap();
        assert_eq!(receipt["valid"], json!(true));
        assert_eq!(receipt["findings"], json!([]));
        let hash = receipt["content_hash"].as_str().unwrap().to_owned();
        assert_eq!(hash.len(), 64);
        assert_eq!(session.draft_count(), 1);

        // Content-addressed scratch: re-composing identical content reuses
        // the entry.
        let again = tool.call(valid_args()).await.unwrap();
        assert_eq!(again["content_hash"], json!(hash));
        assert_eq!(session.draft_count(), 1);
    }

    #[tokio::test]
    async fn compose_invalid_skill_never_stores_a_draft() {
        let session = session();
        let tool = ComposeSkillTool::new(Arc::clone(&session));
        let receipt = tool
            .call(json!({
                "name": "Not Kebab",
                "description": "d",
                "body": "b",
                "author": "agent:rusty"
            }))
            .await
            .unwrap();
        assert_eq!(receipt["valid"], json!(false));
        assert_eq!(receipt["content_hash"], Value::Null);
        assert_eq!(receipt["findings"][0]["stage"], json!("validation"));
        assert!(!receipt["suggested_revision_notes"].as_array().unwrap().is_empty());
        assert_eq!(session.draft_count(), 0);
    }

    #[tokio::test]
    async fn scan_denial_names_the_hash_but_stores_nothing() {
        let session = session();
        let tool = ComposeSkillTool::new(Arc::clone(&session));
        let receipt = tool
            .call(json!({
                "name": "sneaky",
                "description": "d",
                "body": "run <script>alert(1)</script> now",
                "author": "agent:rusty"
            }))
            .await
            .unwrap();
        assert_eq!(receipt["valid"], json!(false));
        assert!(receipt["content_hash"].is_string());
        assert_eq!(receipt["findings"][0]["stage"], json!("scan"));
        assert_eq!(receipt["findings"][0]["severity"], json!("denial"));
        assert_eq!(session.draft_count(), 0);
    }

    #[test]
    fn publish_effect_id_is_scoped_to_scope_and_draft() {
        let a = publish_effect_id("run-1", "hash-a");
        assert_eq!(a, publish_effect_id("run-1", "hash-a"));
        assert_ne!(a, publish_effect_id("run-2", "hash-a"));
        assert_ne!(a, publish_effect_id("run-1", "hash-b"));
    }

    #[tokio::test]
    async fn recipe_placeholders_must_be_declared() {
        let tool = ComposeToolDefinitionTool::new(vec!["git".to_owned()]).unwrap();
        let receipt = tool
            .call(json!({
                "name": "get_thing",
                "description": "Fetch one thing.",
                "parameters_schema": {"type": "object", "properties": {"id": {"type": "string"}}},
                "effect": "read_only",
                "recipe": {"kind": "http", "method": "GET", "path_template": "/things/{missing}"}
            }))
            .await
            .unwrap();
        assert_eq!(receipt["valid"], json!(false));
        assert!(receipt["findings"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("placeholder `missing`"));
    }

    #[tokio::test]
    async fn cli_recipes_respect_the_allowlist() {
        let tool = ComposeToolDefinitionTool::new(vec![]).unwrap();
        let receipt = tool
            .call(json!({
                "name": "run_git",
                "description": "Run git.",
                "parameters_schema": {"type": "object", "properties": {}},
                "effect": "non_idempotent",
                "recipe": {"kind": "cli", "command": "git", "args_template": ["status"]}
            }))
            .await
            .unwrap();
        assert_eq!(receipt["valid"], json!(false));

        let tool = ComposeToolDefinitionTool::new(vec!["git".to_owned()]).unwrap();
        let receipt = tool
            .call(json!({
                "name": "run_git",
                "description": "Run git.",
                "parameters_schema": {"type": "object", "properties": {}},
                "effect": "non_idempotent",
                "recipe": {"kind": "cli", "command": "git", "args_template": ["status"]}
            }))
            .await
            .unwrap();
        assert_eq!(receipt["valid"], json!(true));
        assert!(receipt["content_hash"].is_string());
        assert!(receipt["publish_seam"].as_str().unwrap().contains("ConnectorManifest"));
    }
}
