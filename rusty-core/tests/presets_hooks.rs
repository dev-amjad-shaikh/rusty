//! Parity wave integration tests: permission presets and Claude Code hooks.
//!
//! - **Presets** — the closed vocabulary resolves into the real seams
//!   (guards, approval posture, CLI mode), a read-only preset makes a write
//!   tool unreachable through dispatch (journaled denial, body never runs),
//!   two presets intersect restrictively, and the ask posture fails closed
//!   without an answerer while journaling the asked/decided pair with one.
//! - **Hooks** — the Claude Code `hooks.json` contract end to end: the
//!   stdin payload, exit 0 / exit 2 / other-exit semantics, JSON decisions
//!   (modern and legacy shapes), the deny > ask > allow merge, the timeout
//!   and output-ceiling bounds, the scrubbed environment, and the guard-seam
//!   integration (a hook denial journals `ToolCallDenied` like any other).
//!
//! Hook fixtures are inline `/bin/sh -c` commands — the runtime spawns
//! exactly that, so the tests exercise the same interpreter the contract
//! documents, with no fixture files to keep in sync.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

use rusty_agent_runtime::capability::{
    CliPolicyMode, PermissionPreset, PresetResolution, RunConfigPresetExt,
};
use rusty_agent_runtime::error::Result as RustyResult;
use rusty_agent_runtime::executor::RunConfig;
use rusty_agent_runtime::hooks::{HookGuard, HookNoteKind, HooksConfig};
use rusty_agent_runtime::journal::{Clock, Journal};
use rusty_agent_runtime::llm::ToolCall;
use rusty_agent_runtime::record::{ApprovalDecision, Effect, PayloadRef, RunEventKind};
use rusty_agent_runtime::tool::approval::{ApprovalAnswerer, ApprovalGate};
use rusty_agent_runtime::tool::{GuardedCall, Tool, ToolExecutor, ToolGuard, ToolRegistry};

// ---------- shared fixtures ----------

/// A tool with an honest effect declaration and a call counter, so a
/// guard-blocked dispatch can prove the body never ran.
struct CountingTool {
    name: &'static str,
    effect: Effect,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "A counting fixture tool."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn effect(&self) -> Effect {
        self.effect
    }
    async fn call(&self, _args: Value) -> RustyResult<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!("ran"))
    }
}

fn counting_tool(name: &'static str, effect: Effect) -> (CountingTool, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    (
        CountingTool {
            name,
            effect,
            calls: calls.clone(),
        },
        calls,
    )
}

fn guarded_call<'a>(tool: &'a str, arguments: &'a Value, effect: Effect) -> GuardedCall<'a> {
    GuardedCall {
        tool,
        arguments,
        effect,
        scope: "t-parity",
    }
}

fn journal() -> Journal {
    Journal::new("run-parity", "t-parity", Clock::logical(0, 1))
}

fn inline_output(event: &rusty_agent_runtime::record::RunEvent) -> Value {
    match event.output.as_ref() {
        Some(PayloadRef::Inline(value)) => value.clone(),
        other => panic!("expected inline output payload, got {other:?}"),
    }
}

// ---------- presets ----------

#[test]
fn preset_wire_names_are_stable_snake_case() {
    let presets = [
        (PermissionPreset::ReadOnly, "read_only"),
        (PermissionPreset::WorkspaceAsk, "workspace_ask"),
        (PermissionPreset::Workspace, "workspace"),
        (PermissionPreset::FullAccess, "full_access"),
    ];
    for (preset, name) in presets {
        assert_eq!(serde_json::to_value(preset).unwrap(), json!(name));
        assert_eq!(
            serde_json::from_value::<PermissionPreset>(json!(name)).unwrap(),
            preset
        );
    }
    let modes = [
        (CliPolicyMode::Disabled, "disabled"),
        (CliPolicyMode::ReadOnly, "read_only"),
        (CliPolicyMode::Jailed, "jailed"),
        (CliPolicyMode::Shell, "shell"),
    ];
    for (mode, name) in modes {
        assert_eq!(serde_json::to_value(mode).unwrap(), json!(name));
    }
}

#[test]
fn presets_resolve_into_the_named_bundles() {
    let read_only = PermissionPreset::ReadOnly.resolve();
    assert_eq!(read_only.cli_mode(), CliPolicyMode::Disabled);
    assert_eq!(read_only.posture().as_str(), "auto_deny");

    let ask = PermissionPreset::WorkspaceAsk.resolve();
    assert_eq!(ask.cli_mode(), CliPolicyMode::Jailed);
    assert_eq!(ask.posture().as_str(), "ask");

    let workspace = PermissionPreset::Workspace.resolve();
    assert_eq!(workspace.cli_mode(), CliPolicyMode::Jailed);
    assert_eq!(workspace.posture().as_str(), "allow_once");

    // Full access materializes no guards at all — the allowlist and the
    // effect boundary are the only constraints, as named.
    let full = PermissionPreset::FullAccess.resolve();
    assert_eq!(full.cli_mode(), CliPolicyMode::Shell);
    assert!(full.guards().is_empty());
}

#[tokio::test]
async fn read_only_preset_makes_a_write_tool_unreachable() {
    let journal = journal();
    let (tool, calls) = counting_tool("write_file", Effect::NonIdempotent);
    let mut registry = ToolRegistry::new();
    registry.register(tool);
    let executor = ToolExecutor::new(registry)
        .with_tool_guards(PermissionPreset::ReadOnly.resolve().into_guards())
        .with_guard_journal(journal.clone(), "parent-0")
        .with_call_context("t-parity", "tools");

    let results = executor
        .execute_batch(&[ToolCall::new("c1", "write_file", json!({"path": "a.txt"}))])
        .await;

    assert_eq!(calls.load(Ordering::SeqCst), 0, "the body never ran");
    let content = results[0].content.as_deref().unwrap();
    assert!(content.contains("preset_effect_ceiling"), "got: {content}");
    assert!(content.contains("read_only"), "got: {content}");

    // The journaled denial names the tool and the guard.
    let snapshot = journal.snapshot();
    let denials: Vec<_> = snapshot
        .events
        .iter()
        .filter(|event| event.kind == RunEventKind::ToolCallDenied)
        .collect();
    assert_eq!(denials.len(), 1);
    let output = inline_output(denials[0]);
    assert_eq!(output["tool"], json!("write_file"));
    assert_eq!(output["effect"], json!("non_idempotent"));
    assert_eq!(
        output["denials"][0]["guard"],
        json!("preset_effect_ceiling")
    );
}

#[tokio::test]
async fn read_only_preset_leaves_read_tools_reachable() {
    let (tool, calls) = counting_tool("read_file", Effect::ReadOnly);
    let mut registry = ToolRegistry::new();
    registry.register(tool);
    let executor = ToolExecutor::new(registry)
        .with_tool_guards(PermissionPreset::ReadOnly.resolve().into_guards())
        .with_call_context("t-parity", "tools");

    let results = executor
        .execute_batch(&[ToolCall::new("c1", "read_file", json!({}))])
        .await;

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(results[0].content.as_deref(), Some("ran"));
}

#[test]
fn two_presets_intersect_restrictively() {
    // The preset-level intersection is the more restrictive of the pair.
    assert_eq!(
        PermissionPreset::Workspace.intersect(PermissionPreset::ReadOnly),
        PermissionPreset::ReadOnly
    );
    assert_eq!(
        PermissionPreset::ReadOnly.intersect(PermissionPreset::Workspace),
        PermissionPreset::ReadOnly
    );
    assert_eq!(
        PermissionPreset::Workspace.intersect(PermissionPreset::Workspace),
        PermissionPreset::Workspace
    );

    // The resolution-level intersection unions the guard sets (which can
    // only narrow) and takes the more restrictive mode of the pair: a write
    // the workspace preset alone permits stays unreachable.
    let merged = PresetResolution::intersect(
        PermissionPreset::Workspace.resolve(),
        PermissionPreset::ReadOnly.resolve(),
    );
    assert_eq!(merged.preset(), PermissionPreset::ReadOnly);
    assert_eq!(merged.cli_mode(), CliPolicyMode::Disabled);

    let args = json!({});
    let write = guarded_call("write_file", &args, Effect::NonIdempotent);
    let read = guarded_call("read_file", &args, Effect::ReadOnly);
    assert!(merged
        .guards()
        .iter()
        .any(|guard| guard.check(&write).is_some()));
    assert!(merged
        .guards()
        .iter()
        .all(|guard| guard.check(&read).is_none()));
}

#[test]
fn workspace_ask_without_an_answerer_fails_closed() {
    let resolution = PermissionPreset::WorkspaceAsk.resolve();
    let args = json!({});
    let write = guarded_call("write_file", &args, Effect::NonIdempotent);
    let keyed_write = guarded_call("upsert", &args, Effect::Idempotent);
    let read = guarded_call("read_file", &args, Effect::ReadOnly);

    let denials: Vec<_> = resolution
        .guards()
        .iter()
        .filter_map(|guard| guard.check(&write))
        .collect();
    assert_eq!(denials.len(), 1);
    assert_eq!(denials[0].guard, "preset_ask");
    assert!(denials[0].reason.contains("no answerer is wired"));

    // Freely repeatable effects never ask.
    assert!(resolution
        .guards()
        .iter()
        .all(|guard| guard.check(&keyed_write).is_none()));
    assert!(resolution
        .guards()
        .iter()
        .all(|guard| guard.check(&read).is_none()));
}

#[test]
fn workspace_ask_with_an_approving_answerer_grants_and_journals() {
    let journal = journal();
    let answerer: ApprovalAnswerer = Arc::new(|_request| ApprovalDecision::ApprovedOnce {
        approved_by: "test-operator".to_owned(),
    });
    let resolution = PermissionPreset::WorkspaceAsk
        .resolve_with(Some(answerer), Some(ApprovalGate::new(&journal)));

    let args = json!({"path": "a.txt"});
    let write = guarded_call("write_file", &args, Effect::NonIdempotent);
    assert!(resolution
        .guards()
        .iter()
        .all(|guard| guard.check(&write).is_none()));

    // The asked/decided pair is journaled in the closed vocabulary.
    let snapshot = journal.snapshot();
    let asked: Vec<_> = snapshot
        .events
        .iter()
        .filter(|event| event.kind == RunEventKind::ApprovalAsked)
        .collect();
    let decided: Vec<_> = snapshot
        .events
        .iter()
        .filter(|event| event.kind == RunEventKind::ApprovalDecided)
        .collect();
    assert_eq!(asked.len(), 1);
    assert_eq!(decided.len(), 1);
    let outcome = inline_output(decided[0]);
    assert_eq!(outcome["kind"], json!("write_file"));
    assert_eq!(outcome["decision"]["decision"], json!("approved_once"));
    assert_eq!(outcome["decision"]["approved_by"], json!("test-operator"));
}

#[test]
fn workspace_ask_with_a_rejecting_answerer_denies() {
    let answerer: ApprovalAnswerer = Arc::new(|_request| ApprovalDecision::Rejected {
        decided_by: "policy-engine".to_owned(),
        reason: Some("no writes during freeze".to_owned()),
    });
    let resolution = PermissionPreset::WorkspaceAsk.resolve_with(Some(answerer), None);

    let args = json!({});
    let write = guarded_call("write_file", &args, Effect::NonIdempotent);
    let denials: Vec<_> = resolution
        .guards()
        .iter()
        .filter_map(|guard| guard.check(&write))
        .collect();
    assert_eq!(denials.len(), 1);
    assert!(
        denials[0]
            .reason
            .contains("rejected by `policy-engine`: no writes during freeze"),
        "got: {}",
        denials[0].reason
    );
}

#[test]
fn cli_mode_guard_enforces_each_mode() {
    let argv_args = json!({"program": "git", "args": ["status"]});
    let shell_args = json!({"command": "git status"});

    // Disabled refuses the tool outright, even a read-only policy.
    let disabled = PermissionPreset::ReadOnly.resolve();
    let call = guarded_call("run_cli", &argv_args, Effect::ReadOnly);
    assert!(disabled
        .guards()
        .iter()
        .any(|guard| guard.check(&call).is_some()));

    // Jailed permits argv spawns and refuses shell payloads.
    let jailed = PermissionPreset::Workspace.resolve();
    let argv = guarded_call("run_cli", &argv_args, Effect::NonIdempotent);
    let shell = guarded_call("run_cli", &shell_args, Effect::NonIdempotent);
    assert!(jailed
        .guards()
        .iter()
        .all(|guard| guard.check(&argv).is_none()));
    let shell_denials: Vec<_> = jailed
        .guards()
        .iter()
        .filter_map(|guard| guard.check(&shell))
        .collect();
    assert_eq!(shell_denials.len(), 1);
    assert_eq!(shell_denials[0].guard, "preset_cli_mode");
    assert!(shell_denials[0].reason.contains("refuses shell payloads"));

    // The CLI guard never touches other tools.
    let other = guarded_call("write_file", &argv_args, Effect::NonIdempotent);
    let cli_denials: Vec<_> = jailed
        .guards()
        .iter()
        .filter_map(|guard| guard.check(&other))
        .filter(|denial| denial.guard == "preset_cli_mode")
        .collect();
    assert!(cli_denials.is_empty());
}

#[test]
fn run_config_extension_appends_preset_guards() {
    let config = RunConfig::new("t-parity").with_permission_preset(PermissionPreset::ReadOnly);
    assert!(!config.tool_guards.is_empty());

    let args = json!({});
    let write = guarded_call("write_file", &args, Effect::NonIdempotent);
    assert!(config
        .tool_guards
        .iter()
        .any(|guard| guard.check(&write).is_some()));

    // Composition with a caller's own guards is the restrictive union: both
    // are present, both evaluated.
    let config = config.with_permission_preset(PermissionPreset::Workspace);
    assert!(config.tool_guards.len() >= 2);
}

// ---------- hooks ----------

/// Parse a `PreToolUse`-only hooks config from group fragments.
fn hook_config(groups: &str) -> HooksConfig {
    let text = format!(r#"{{"hooks": {{"PreToolUse": [{groups}]}}}}"#);
    HooksConfig::from_json(&text).unwrap()
}

/// A single-group, single-command config.
fn one_hook(command: &str) -> HooksConfig {
    hook_config(&format!(
        r#"{{"matcher": "*", "hooks": [{{"type": "command", "command": {command:?}}}]}}"#
    ))
}

fn hook_guard(config: HooksConfig) -> HookGuard {
    HookGuard::new(config)
}

#[test]
fn exit_zero_allows() {
    let guard = hook_guard(one_hook("exit 0"));
    let args = json!({});
    let call = guarded_call("write_file", &args, Effect::NonIdempotent);
    assert!(guard.check(&call).is_none());
    assert!(guard.take_notes().is_empty());
}

#[test]
fn the_stdin_payload_carries_the_documented_fields() {
    let path = std::env::temp_dir().join(format!(
        "rusty-hook-stdin-{}-{}.json",
        std::process::id(),
        "parity"
    ));
    let path_string = path.display().to_string();
    let guard = hook_guard(one_hook(&format!("cat > '{path_string}'")));

    let args = json!({"path": "a.txt"});
    let call = guarded_call("write_file", &args, Effect::NonIdempotent);
    assert!(guard.check(&call).is_none());

    let payload: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(payload["session_id"], json!("t-parity"));
    assert_eq!(payload["hook_event_name"], json!("PreToolUse"));
    assert_eq!(payload["tool_name"], json!("write_file"));
    assert_eq!(payload["tool_input"], args);
    assert!(payload["cwd"].is_string());
}

#[test]
fn exit_two_is_a_blocking_deny_with_stderr_fed_back() {
    let guard = hook_guard(one_hook("echo 'no writes today' >&2; exit 2"));
    let args = json!({});
    let call = guarded_call("write_file", &args, Effect::NonIdempotent);
    let denial = guard.check(&call).expect("exit 2 blocks");
    assert_eq!(denial.guard, "claude_hooks");
    assert!(denial.reason.contains("no writes today"));
}

#[test]
fn other_nonzero_exits_are_non_blocking_notes() {
    let guard = hook_guard(one_hook("echo 'linter grumbled' >&2; exit 1"));
    let args = json!({});
    let call = guarded_call("write_file", &args, Effect::NonIdempotent);
    assert!(guard.check(&call).is_none(), "exit 1 must not block");

    let notes = guard.take_notes();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].kind, HookNoteKind::NonBlockingExit);
    assert!(notes[0].detail.contains("exit 1"));
    assert!(notes[0].detail.contains("linter grumbled"));
    // The ledger drains.
    assert!(guard.take_notes().is_empty());
}

#[test]
fn json_permission_decision_deny_blocks_with_the_reason() {
    let command = concat!(
        "printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",",
        "\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"policy says no\"}}'"
    );
    let guard = hook_guard(one_hook(command));
    let args = json!({});
    let call = guarded_call("write_file", &args, Effect::NonIdempotent);
    let denial = guard.check(&call).expect("a JSON deny blocks");
    assert!(denial.reason.contains("policy says no"));
}

#[test]
fn json_permission_decision_ask_without_an_answerer_denies_fail_closed() {
    let command = concat!(
        "printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",",
        "\"permissionDecision\":\"ask\",\"permissionDecisionReason\":\"confirm the write\"}}'"
    );
    let guard = hook_guard(one_hook(command));
    let args = json!({});
    let call = guarded_call("write_file", &args, Effect::NonIdempotent);
    let denial = guard.check(&call).expect("an unanswered ask denies");
    assert!(denial.reason.contains("confirm the write"));
    assert!(denial.reason.contains("no answerer is wired"));
}

#[test]
fn json_permission_decision_ask_with_an_answerer_uses_the_closed_vocabulary() {
    let command = concat!(
        "printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",",
        "\"permissionDecision\":\"ask\",\"permissionDecisionReason\":\"confirm the write\"}}'"
    );
    let journal = journal();
    let answerer: ApprovalAnswerer = Arc::new(|_request| ApprovalDecision::ApprovedOnce {
        approved_by: "test-operator".to_owned(),
    });
    let guard = hook_guard(one_hook(command))
        .with_answerer(answerer)
        .with_approval_gate(ApprovalGate::new(&journal));

    let args = json!({});
    let call = guarded_call("write_file", &args, Effect::NonIdempotent);
    assert!(guard.check(&call).is_none(), "the grant admits");

    let snapshot = journal.snapshot();
    let asked = snapshot
        .events
        .iter()
        .filter(|event| event.kind == RunEventKind::ApprovalAsked)
        .count();
    let decided: Vec<_> = snapshot
        .events
        .iter()
        .filter(|event| event.kind == RunEventKind::ApprovalDecided)
        .collect();
    assert_eq!(asked, 1);
    assert_eq!(decided.len(), 1);
    assert_eq!(
        inline_output(decided[0])["decision"]["decision"],
        json!("approved_once")
    );

    // A rejecting answerer denies with the vocabulary's phrasing.
    let rejecting: ApprovalAnswerer = Arc::new(|_request| ApprovalDecision::Rejected {
        decided_by: "operator".to_owned(),
        reason: None,
    });
    let guard = hook_guard(one_hook(command)).with_answerer(rejecting);
    let denial = guard.check(&call).expect("the rejection denies");
    assert!(denial.reason.contains("rejected by `operator`"));
}

#[test]
fn legacy_top_level_decision_block_is_honored() {
    let command = "printf '%s' '{\"decision\":\"block\",\"reason\":\"legacy no\"}'";
    let guard = hook_guard(one_hook(command));
    let args = json!({});
    let call = guarded_call("write_file", &args, Effect::NonIdempotent);
    let denial = guard.check(&call).expect("the legacy shape blocks");
    assert!(denial.reason.contains("legacy no"));
}

#[test]
fn multiple_hooks_merge_deny_over_ask_over_allow() {
    let allow = concat!(
        "printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",",
        "\"permissionDecision\":\"allow\"}}'"
    );
    let ask = concat!(
        "printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",",
        "\"permissionDecision\":\"ask\",\"permissionDecisionReason\":\"confirm\"}}'"
    );
    let groups = format!(
        r#"{{"matcher": "*", "hooks": [{{"type": "command", "command": {allow:?}}}]}},
           {{"matcher": "*", "hooks": [{{"type": "command", "command": "echo 'veto' >&2; exit 2"}}]}}"#
    );
    let guard = hook_guard(hook_config(&groups));
    let args = json!({});
    let call = guarded_call("write_file", &args, Effect::NonIdempotent);
    let denial = guard.check(&call).expect("deny beats allow");
    assert!(denial.reason.contains("veto"));

    // Ask beats allow: with no answerer wired, the merged ask denies.
    let groups = format!(
        r#"{{"matcher": "*", "hooks": [{{"type": "command", "command": {allow:?}}}]}},
           {{"matcher": "*", "hooks": [{{"type": "command", "command": {ask:?}}}]}}"#
    );
    let guard = hook_guard(hook_config(&groups));
    let denial = guard.check(&call).expect("ask beats allow");
    assert!(denial.reason.contains("no answerer is wired"));
}

#[test]
fn a_timed_out_hook_is_a_non_blocking_note() {
    let config = hook_config(
        r#"{"matcher": "*", "hooks": [{"type": "command", "command": "sleep 5", "timeout": 1}]}"#,
    );
    let guard = hook_guard(config);
    let args = json!({});
    let call = guarded_call("write_file", &args, Effect::NonIdempotent);
    assert!(guard.check(&call).is_none(), "a timeout must not block");

    let notes = guard.take_notes();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].kind, HookNoteKind::Timeout);
}

#[test]
fn oversized_output_is_discarded_into_a_note() {
    let guard = hook_guard(one_hook("yes a | head -c 5000"))
        .with_max_output_bytes(256)
        .unwrap();
    let args = json!({});
    let call = guarded_call("write_file", &args, Effect::NonIdempotent);
    assert!(guard.check(&call).is_none(), "a flood must not block");

    let notes = guard.take_notes();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].kind, HookNoteKind::OutputTruncated);
}

#[test]
fn matchers_fire_on_alternation_wildcard_and_exact_names_only() {
    let groups = r#"{"matcher": "read_file|write_file", "hooks": [{"type": "command", "command": "echo 'matched' >&2; exit 2"}]},
        {"matcher": "other_tool", "hooks": [{"type": "command", "command": "echo 'never' >&2; exit 2"}]}"#;
    let guard = hook_guard(hook_config(groups));

    let args = json!({});
    let write = guarded_call("write_file", &args, Effect::NonIdempotent);
    let read = guarded_call("read_file", &args, Effect::ReadOnly);
    let other = guarded_call("unrelated", &args, Effect::NonIdempotent);
    assert!(guard.check(&write).is_some(), "alternation matches");
    assert!(guard.check(&read).is_some(), "alternation matches");
    assert!(guard.check(&other).is_none(), "no matcher fires");

    // A missing matcher matches everything.
    let wildcard = hook_guard(hook_config(
        r#"{"hooks": [{"type": "command", "command": "echo 'all' >&2; exit 2"}]}"#,
    ));
    assert!(wildcard.check(&other).is_some());
}

#[test]
fn configs_reject_unsupported_hook_types_and_tolerate_other_events() {
    // Other events parse and drop: a mixed file stays loadable.
    let config = HooksConfig::from_json(
        r#"{"hooks": {"PostToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": "true"}]}]}}"#,
    )
    .unwrap();
    assert!(config.is_empty());

    // A non-command PreToolUse entry fails parsing rather than silently
    // skipping a blocking-intent hook.
    let error = HooksConfig::from_json(
        r#"{"hooks": {"PreToolUse": [{"matcher": "*", "hooks": [{"type": "http", "url": "https://example.test/hook"}]}]}}"#,
    )
    .unwrap_err();
    assert!(error.to_string().contains("unsupported type `http`"));

    // Malformed JSON fails closed at load.
    assert!(HooksConfig::from_json("{not json").is_err());
}

#[test]
fn hooks_never_see_the_process_environment() {
    std::env::set_var("RUSTY_HOOK_PARITY_SECRET", "hunter2");
    // If the secret leaks into the child's environment, the hook denies; a
    // scrubbed environment means silence.
    let command = concat!(
        "printenv RUSTY_HOOK_PARITY_SECRET >/dev/null && ",
        "printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",",
        "\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"secret leaked\"}}'; ",
        "exit 0"
    );
    let guard = hook_guard(one_hook(command));
    let args = json!({});
    let call = guarded_call("write_file", &args, Effect::NonIdempotent);
    assert!(
        guard.check(&call).is_none(),
        "the hook environment must be scrubbed"
    );
    std::env::remove_var("RUSTY_HOOK_PARITY_SECRET");
}

#[tokio::test]
async fn a_hook_denial_flows_through_the_guard_seam() {
    let journal = journal();
    let (tool, calls) = counting_tool("write_file", Effect::NonIdempotent);
    let mut registry = ToolRegistry::new();
    registry.register(tool);
    let guard = hook_guard(one_hook("echo 'hook veto' >&2; exit 2"));
    let executor = ToolExecutor::new(registry)
        .with_tool_guards(vec![Arc::new(guard)])
        .with_guard_journal(journal.clone(), "parent-0")
        .with_call_context("t-parity", "tools");

    let results = executor
        .execute_batch(&[ToolCall::new("c1", "write_file", json!({}))])
        .await;

    assert_eq!(calls.load(Ordering::SeqCst), 0, "the body never ran");
    let content = results[0].content.as_deref().unwrap();
    assert!(content.contains("claude_hooks"), "got: {content}");
    assert!(content.contains("hook veto"), "got: {content}");

    // The journaled denial is the shared guard evidence: the hook bridge
    // needs no evidence path of its own.
    let snapshot = journal.snapshot();
    let denials: Vec<_> = snapshot
        .events
        .iter()
        .filter(|event| event.kind == RunEventKind::ToolCallDenied)
        .collect();
    assert_eq!(denials.len(), 1);
    let output = inline_output(denials[0]);
    assert_eq!(output["tool"], json!("write_file"));
    assert_eq!(output["denials"][0]["guard"], json!("claude_hooks"));
    assert!(output["denials"][0]["reason"]
        .as_str()
        .unwrap()
        .contains("hook veto"));
}
