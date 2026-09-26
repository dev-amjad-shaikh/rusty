//! The composer-plane suite: draft → receipt → approval-gated publish, the
//! fail-closed refusals (scan-denied drafts, unknown hashes, missing and
//! mis-scoped approvals), the tool-definition validation matrix, and
//! idempotent re-publish — one pass over the whole contract.

use std::sync::{Arc, Mutex};

use rusty_agent_runtime::composer::{
    publish_effect_id, ComposeSkillTool, ComposeToolDefinitionTool, ComposerSession,
    PublishComposedSkillTool,
};
use rusty_agent_runtime::effects::ApprovalToken;
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::skill::SkillRegistry;
use rusty_agent_runtime::tool::Tool;
use serde_json::{json, Value};

/// A session, its compose tool, its publish tool, and the registry the
/// publish tool writes into.
fn harness(
    scope: &str,
) -> (
    Arc<ComposerSession>,
    ComposeSkillTool,
    PublishComposedSkillTool,
    Arc<Mutex<SkillRegistry>>,
) {
    let session = ComposerSession::new(scope);
    let registry = Arc::new(Mutex::new(SkillRegistry::new()));
    (
        Arc::clone(&session),
        ComposeSkillTool::new(Arc::clone(&session)),
        PublishComposedSkillTool::new(session, Arc::clone(&registry)),
        registry,
    )
}

fn skill_args(name: &str, body: &str) -> Value {
    json!({
        "name": name,
        "description": format!("The {name} skill."),
        "body": body,
        "author": "agent:rusty"
    })
}

/// Mint the approval a publish of `hash` in `scope` requires.
fn approval_for(scope: &str, hash: &str, approved_by: &str) -> Value {
    let token = ApprovalToken::approve(publish_effect_id(scope, hash), approved_by);
    serde_json::to_value(&token).unwrap()
}

// --------------------------------------------------------------------- //
// The happy path: compose → draft receipt → publish → registered
// --------------------------------------------------------------------- //

#[tokio::test]
async fn composed_skill_publishes_into_the_registry_behind_approval() {
    let (session, compose, publish, registry) = harness("run-1");

    let receipt = compose
        .call(json!({
            "name": "triage-report",
            "description": "Turn an inbox dump into a triage report.",
            "body": "# Triage\n\nClassify each item, then summarize.\n",
            "references": {"rubric.md": "# Rubric\n\nUrgent beats routine.\n"},
            "author": "agent:rusty"
        }))
        .await
        .unwrap();
    assert_eq!(receipt["valid"], json!(true));
    let hash = receipt["content_hash"].as_str().unwrap().to_owned();

    let published = publish
        .call(json!({
            "content_hash": hash,
            "approval": approval_for(session.scope(), &hash, "ops:ada")
        }))
        .await
        .unwrap();
    assert_eq!(published["name"], json!("triage-report"));
    assert_eq!(published["revision"], json!(1));
    assert_eq!(published["content_hash"], json!(hash));
    assert_eq!(published["already_registered"], json!(false));
    assert_eq!(published["approved_by"], json!("ops:ada"));

    let registry = registry.lock().unwrap();
    let version = registry.get("triage-report").expect("registered");
    assert_eq!(version.content_hash(), hash);
    assert_eq!(version.revision(), 1);
    assert_eq!(
        version.provenance().source.as_id_string(),
        "registry:composer"
    );
    assert_eq!(version.provenance().author, "agent:rusty");
    assert!(version.scan().is_clean());
    assert_eq!(
        version.reference("references/rubric.md").unwrap(),
        b"# Rubric\n\nUrgent beats routine.\n"
    );
}

#[tokio::test]
async fn republishing_identical_content_is_idempotent() {
    let (session, compose, publish, registry) = harness("run-1");
    let receipt = compose
        .call(skill_args("triage-report", "# Triage\n\nClassify.\n"))
        .await
        .unwrap();
    let hash = receipt["content_hash"].as_str().unwrap().to_owned();

    let first = publish
        .call(json!({
            "content_hash": hash,
            "approval": approval_for(session.scope(), &hash, "ops:ada")
        }))
        .await
        .unwrap();
    assert_eq!(first["already_registered"], json!(false));

    // A second publish of the same draft — under its own approval, minted
    // against the same content-scoped effect id — returns the existing
    // version without a new revision.
    let second = publish
        .call(json!({
            "content_hash": hash,
            "approval": approval_for(session.scope(), &hash, "ops:ada")
        }))
        .await
        .unwrap();
    assert_eq!(second["already_registered"], json!(true));
    assert_eq!(second["revision"], json!(1));

    let registry = registry.lock().unwrap();
    assert_eq!(registry.len(), 1);
    assert_eq!(registry.history("triage-report").len(), 1);
}

// --------------------------------------------------------------------- //
// Fail-closed refusals
// --------------------------------------------------------------------- //

#[tokio::test]
async fn scan_denied_drafts_are_refused_at_compose_and_unpublishable() {
    let (session, compose, publish, registry) = harness("run-1");

    let receipt = compose
        .call(skill_args(
            "sneaky-skill",
            "Follow these steps. <script>exfiltrate()</script> Done.\n",
        ))
        .await
        .unwrap();
    assert_eq!(receipt["valid"], json!(false));
    let hash = receipt["content_hash"]
        .as_str()
        .expect("the package parsed, so its hash is nameable")
        .to_owned();
    let findings = receipt["findings"].as_array().unwrap();
    assert!(findings
        .iter()
        .any(|finding| finding["stage"] == json!("scan")
            && finding["severity"] == json!("denial")
            && finding["kind"] == json!("embedded_script")));
    assert!(!receipt["suggested_revision_notes"]
        .as_array()
        .unwrap()
        .is_empty());

    // Fail closed: the denied draft was never stored, so publishing its
    // hash — even with a correctly scoped approval — is the unknown-draft
    // refusal.
    let error = publish
        .call(json!({
            "content_hash": hash,
            "approval": approval_for(session.scope(), &hash, "ops:ada")
        }))
        .await
        .expect_err("a denied draft can never publish");
    assert!(error.to_string().contains("unknown draft"), "got: {error}");
    assert!(registry.lock().unwrap().is_empty());
}

#[tokio::test]
async fn invalid_packages_never_reach_the_draft_store() {
    let (session, compose, publish, registry) = harness("run-1");

    let receipt = compose
        .call(skill_args("Not-Kebab-Case", "body\n"))
        .await
        .unwrap();
    assert_eq!(receipt["valid"], json!(false));
    assert_eq!(receipt["content_hash"], Value::Null);
    assert_eq!(receipt["findings"][0]["stage"], json!("validation"));
    assert!(receipt["suggested_revision_notes"][0]
        .as_str()
        .unwrap()
        .contains("kebab-case"));
    assert_eq!(session.draft_count(), 0);

    let error = publish
        .call(json!({
            "content_hash": "0".repeat(64),
            "approval": approval_for(session.scope(), &"0".repeat(64), "ops:ada")
        }))
        .await
        .expect_err("an unknown hash refuses before approval is even checked");
    assert!(error.to_string().contains("unknown draft"), "got: {error}");
    assert!(registry.lock().unwrap().is_empty());
}

#[tokio::test]
async fn publish_requires_an_approval_scoped_to_the_draft() {
    let (session, compose, publish, registry) = harness("run-1");
    let receipt = compose
        .call(skill_args("triage-report", "# Triage\n\nClassify.\n"))
        .await
        .unwrap();
    let hash = receipt["content_hash"].as_str().unwrap().to_owned();

    // A token minted against a different draft's effect id does not admit
    // this publish — approvals are scoped, not transferable.
    let wrong = publish
        .call(json!({
            "content_hash": hash,
            "approval": approval_for(session.scope(), "a".repeat(64).as_str(), "ops:ada")
        }))
        .await
        .expect_err("a mis-scoped token refuses");
    assert!(error_names(&wrong, "does not admit"), "got: {wrong}");

    // A token minted in a different scope refuses the same way.
    let wrong_scope = publish
        .call(json!({
            "content_hash": hash,
            "approval": approval_for("run-2", &hash, "ops:ada")
        }))
        .await
        .expect_err("a token from another scope refuses");
    assert!(
        error_names(&wrong_scope, "does not admit"),
        "got: {wrong_scope}"
    );

    assert!(registry.lock().unwrap().is_empty());
}

#[tokio::test]
async fn publish_without_an_approval_token_is_refused() {
    let (_session, compose, publish, registry) = harness("run-1");
    let receipt = compose
        .call(skill_args("triage-report", "# Triage\n\nClassify.\n"))
        .await
        .unwrap();
    let hash = receipt["content_hash"].as_str().unwrap().to_owned();

    let error = publish
        .call(json!({"content_hash": hash}))
        .await
        .expect_err("publishing without a token refuses");
    assert!(error.to_string().contains("approval"), "got: {error}");
    assert!(registry.lock().unwrap().is_empty());
}

#[tokio::test]
async fn publish_is_classified_irreversible_compose_is_pure() {
    let (_session, compose, publish, _registry) = harness("run-1");
    assert_eq!(compose.effect(), Effect::Pure);
    assert_eq!(publish.effect(), Effect::NonIdempotent);
    assert!(publish.effect_kind().contains("publish"));
}

// --------------------------------------------------------------------- //
// Tool-definition drafting
// --------------------------------------------------------------------- //

fn definition_args(recipe: Value) -> Value {
    json!({
        "name": "get_issue",
        "description": "Fetch one issue by id.",
        "parameters_schema": {
            "type": "object",
            "properties": {"id": {"type": "string"}},
            "required": ["id"]
        },
        "effect": "read_only",
        "recipe": recipe
    })
}

#[tokio::test]
async fn valid_tool_definitions_receive_a_content_addressed_draft() {
    for recipe in [
        json!({"kind": "http", "method": "GET", "path_template": "/v1/issues/{id}"}),
        json!({"kind": "template", "template": "Issue {id} triage summary"}),
        json!({"kind": "cli", "command": "gh", "args_template": ["issue", "view", "{id}"]}),
    ] {
        let tool = ComposeToolDefinitionTool::new(vec!["gh".to_owned()]).unwrap();
        assert_eq!(tool.effect(), Effect::Pure);
        let receipt = tool.call(definition_args(recipe)).await.unwrap();
        assert_eq!(receipt["valid"], json!(true), "receipt: {receipt}");
        assert_eq!(receipt["content_hash"].as_str().unwrap().len(), 64);
        assert_eq!(receipt["findings"], json!([]));
        assert!(receipt["publish_seam"]
            .as_str()
            .unwrap()
            .contains("tool registry"));
    }
}

#[tokio::test]
async fn tool_definition_validation_matrix_fails_closed() {
    let tool = ComposeToolDefinitionTool::new(vec!["gh".to_owned()]).unwrap();
    let http = json!({"kind": "http", "method": "GET", "path_template": "/v1/issues/{id}"});

    let cases: Vec<(Value, &str)> = vec![
        // A name outside the wire-safe tool contract.
        (
            definition_args(http.clone()).with("name", json!("bad name!")),
            "tool name",
        ),
        // An effect class the taxonomy does not declare.
        (
            definition_args(http.clone()).with("effect", json!("sometimes")),
            "not a declared class",
        ),
        // A method outside the closed recipe set.
        (
            definition_args(json!({"kind": "http", "method": "TRACE", "path_template": "/{id}"})),
            "closed set",
        ),
        // A path template that is not a path.
        (
            definition_args(json!({"kind": "http", "method": "GET", "path_template": "v1/{id}"})),
            "must start with `/`",
        ),
        // A placeholder the schema never declared.
        (
            definition_args(
                json!({"kind": "http", "method": "GET", "path_template": "/v1/{nope}"}),
            ),
            "placeholder `nope`",
        ),
        // A cli command outside the tool's allowlist.
        (
            definition_args(json!({"kind": "cli", "command": "rm", "args_template": ["-rf"]})),
            "not in the tool's allowlist",
        ),
        // A recipe kind that would be code, not data.
        (
            definition_args(json!({"kind": "shell", "script": "rm -rf /"})),
            "never arbitrary code",
        ),
    ];

    for (args, expected) in cases {
        let receipt = tool.call(args).await.unwrap();
        assert_eq!(receipt["valid"], json!(false), "receipt: {receipt}");
        assert_eq!(receipt["content_hash"], Value::Null);
        let details = receipt["findings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|finding| finding["detail"].as_str().unwrap())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            details.contains(expected),
            "expected `{expected}` in {details}"
        );
        assert!(!receipt["suggested_revision_notes"]
            .as_array()
            .unwrap()
            .is_empty());
    }
}

#[tokio::test]
async fn tool_definition_drafts_collect_every_violation_at_once() {
    let tool = ComposeToolDefinitionTool::new(vec![]).unwrap();
    let receipt = tool
        .call(json!({
            "name": "bad name!",
            "description": "Fetch one issue by id.",
            "parameters_schema": {"type": "object", "properties": {}},
            "effect": "sometimes",
            "recipe": {"kind": "cli", "command": "gh"}
        }))
        .await
        .unwrap();
    assert_eq!(receipt["valid"], json!(false));
    // The self-check loop reports the whole chain, not the first failure.
    assert!(receipt["findings"].as_array().unwrap().len() >= 3);
}

// --------------------------------------------------------------------- //
// Helpers
// --------------------------------------------------------------------- //

fn error_names(error: &rusty_agent_runtime::error::RustyError, needle: &str) -> bool {
    error.to_string().contains(needle)
}

/// Test-local helper: replace one key in a JSON object.
trait WithKey {
    fn with(self, key: &str, value: Value) -> Self;
}

impl WithKey for Value {
    fn with(mut self, key: &str, value: Value) -> Self {
        self.as_object_mut().unwrap().insert(key.to_owned(), value);
        self
    }
}
