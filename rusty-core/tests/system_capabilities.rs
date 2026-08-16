//! System-execution capability packs: CLI, browser use, and computer use.
//!
//! Containment-first conformance: allowlists, jails, ceilings, timeouts,
//! disabled-by-default interaction, and approval-gated irreversible
//! effects — each proven to fail closed, not merely to succeed.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use rusty_agent_runtime::effects::{
    ApprovalToken, CompensationHandler, CompensationRegistry, EffectAdmissionContext,
};
use rusty_agent_runtime::llm::{ChatMessage, ToolCall};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::tool::builtins::browser::{
    BrowserClickTool, BrowserController, BrowserDriver, BrowserNavigateTool, BrowserPolicy,
    BrowserReadTool, BrowserScreenshotTool, BrowserTypeTool, CdpDriver, VirtualBrowserDriver,
    VirtualPage, VIRTUAL_SCREENSHOT,
};
use rusty_agent_runtime::tool::builtins::cli::{
    CliEvidenceSink, CliExecutionRecord, CliPolicy, CliTool,
};
use rusty_agent_runtime::tool::builtins::computer::{
    ComputerClickTool, ComputerController, ComputerPolicy, ComputerScreenshotTool,
    ComputerTypeTool, NullComputerDriver,
};
use rusty_agent_runtime::tool::{Tool, ToolExecutor, ToolRegistry};

fn temp_jail(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rusty-syscap-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("temp jail");
    dir
}

fn result_content(results: &[ChatMessage]) -> &str {
    results[0].content.as_deref().expect("tool result content")
}

fn hex_decode(encoded: &str) -> Vec<u8> {
    assert!(encoded.len() % 2 == 0, "hex payloads are byte-aligned");
    (0..encoded.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&encoded[index..index + 2], 16).expect("valid hex"))
        .collect()
}

// ---------------------------------------------------------------- CLI ----

#[tokio::test]
async fn cli_refuses_programs_outside_the_allowlist() {
    let jail = temp_jail("allowlist");
    let tool = CliTool::new(CliPolicy::new(&jail, ["cat"]).unwrap());

    let error = tool
        .call(json!({"program": "curl", "args": ["https://example.com"]}))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("allowlist"),
        "unlisted programs are refused: {error}"
    );
    // Names carrying path separators never reach resolution.
    let error = CliPolicy::new(&jail, ["../../bin/sh".to_string()]).unwrap_err();
    assert!(error.to_string().contains("bare name"), "{error}");

    std::fs::remove_dir_all(&jail).ok();
}

#[tokio::test]
async fn cli_jails_the_working_directory_to_the_root() {
    let jail = temp_jail("jail");
    std::fs::create_dir_all(jail.join("sub")).unwrap();
    std::fs::write(jail.join("sub").join("hello.txt"), "hello from the jail").unwrap();
    let tool = CliTool::new(CliPolicy::new(&jail, ["cat"]).unwrap());

    let output = tool
        .call(json!({"program": "cat", "args": ["hello.txt"], "cwd": "sub"}))
        .await
        .unwrap();
    assert_eq!(output["exit_code"], json!(0));
    assert_eq!(output["stdout"], json!("hello from the jail"));
    assert_eq!(output["cwd"], json!("sub"));

    for escape in ["..", "../..", "/etc"] {
        let error = tool
            .call(json!({"program": "cat", "args": ["passwd"], "cwd": escape}))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("jail root"),
            "cwd `{escape}` is refused: {error}"
        );
    }

    std::fs::remove_dir_all(&jail).ok();
}

#[tokio::test]
async fn cli_output_ceiling_truncates_and_kills() {
    let jail = temp_jail("ceiling");
    std::fs::write(jail.join("flood.txt"), "x".repeat(128 * 1024)).unwrap();
    let policy = CliPolicy::new(&jail, ["cat"])
        .unwrap()
        .with_max_output_bytes(4096)
        .unwrap();
    let tool = CliTool::new(policy);

    let output = tool
        .call(json!({"program": "cat", "args": ["flood.txt"]}))
        .await
        .unwrap();
    assert_eq!(output["truncated"], json!(true), "the flood was capped");
    assert_eq!(output["timed_out"], json!(false));
    assert_eq!(
        output["exit_code"],
        Value::Null,
        "killed processes have no exit code"
    );
    let stdout = output["stdout"].as_str().unwrap();
    assert!(stdout.len() <= 4096, "stored output respects the cap");
    assert!(
        output["stdout_bytes"].as_u64().unwrap() >= 4096,
        "observed byte counts cover the flood"
    );

    std::fs::remove_dir_all(&jail).ok();
}

#[tokio::test]
async fn cli_timeout_kills_the_process() {
    let jail = temp_jail("timeout");
    let tool = CliTool::new(CliPolicy::new(&jail, ["sleep"]).unwrap());

    let output = tool
        .call(json!({"program": "sleep", "args": ["5"], "timeout_ms": 300}))
        .await
        .unwrap();
    assert_eq!(output["timed_out"], json!(true));
    assert_eq!(output["exit_code"], Value::Null);
    assert!(
        output["duration_ms"].as_u64().unwrap() < 3_000,
        "the timeout actually cut the run short"
    );

    std::fs::remove_dir_all(&jail).ok();
}

#[tokio::test]
async fn cli_shell_mode_is_opt_in_and_still_jailed() {
    let jail = temp_jail("shell");
    std::fs::write(jail.join("note.txt"), "from the shell").unwrap();

    let bare = CliTool::new(CliPolicy::new(&jail, ["cat"]).unwrap());
    let error = bare
        .call(json!({"command": "cat note.txt"}))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("shell flag"),
        "raw commands need the policy opt-in: {error}"
    );

    let shelled = CliTool::new(CliPolicy::new(&jail, ["cat"]).unwrap().with_shell(true));
    let output = shelled
        .call(json!({"command": "cat note.txt"}))
        .await
        .unwrap();
    assert_eq!(output["shell"], json!(true));
    assert_eq!(output["stdout"], json!("from the shell"));
    // The jail still applies through the shell.
    let error = shelled
        .call(json!({"command": "cat note.txt", "cwd": ".."}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("jail root"), "{error}");

    std::fs::remove_dir_all(&jail).ok();
}

#[tokio::test]
async fn cli_irreversible_dispatch_requires_a_one_shot_approval() {
    let jail = temp_jail("approval");
    std::fs::write(jail.join("data.txt"), "approved bytes").unwrap();
    let tool = CliTool::new(CliPolicy::new(&jail, ["cat"]).unwrap());
    assert_eq!(tool.effect(), Effect::NonIdempotent);

    let scope = "run-cli-approval";
    let args = json!({"program": "cat", "args": ["data.txt"]});
    let call = ToolCall::new("c1", "run_cli", args);

    let mut registry = ToolRegistry::new();
    registry.register(tool);
    let request = registry.get("run_cli").unwrap().effect_request(&call);

    // No token: the body never runs.
    let denied = ToolExecutor::new(registry.clone())
        .with_effect_admission(EffectAdmissionContext::new(scope));
    let results = denied.execute_batch(std::slice::from_ref(&call)).await;
    assert!(result_content(&results).contains("effect admission denied"));

    // A token scoped to this exact occurrence admits exactly one dispatch.
    let approval = ApprovalToken::approve(request.effect_id(scope), "ops:amjad");
    let allowed = ToolExecutor::new(registry.clone())
        .with_effect_admission(EffectAdmissionContext::new(scope).with_approvals([approval]));
    let results = allowed.execute_batch(std::slice::from_ref(&call)).await;
    let output: Value = serde_json::from_str(result_content(&results)).unwrap();
    assert_eq!(output["stdout"], json!("approved bytes"));

    // The token was consumed: the identical occurrence is refused on retry.
    let results = allowed.execute_batch(&[call]).await;
    assert!(result_content(&results).contains("effect admission denied"));

    std::fs::remove_dir_all(&jail).ok();
}

#[tokio::test]
async fn cli_read_only_policy_skips_approval_but_keeps_the_jail() {
    let jail = temp_jail("readonly");
    std::fs::write(jail.join("listed.txt"), "visible").unwrap();
    let tool = CliTool::new(CliPolicy::new(&jail, ["ls"]).unwrap().with_read_only(true));
    assert_eq!(tool.effect(), Effect::ReadOnly);

    let mut registry = ToolRegistry::new();
    registry.register(tool);
    let executor = ToolExecutor::new(registry)
        .with_effect_admission(EffectAdmissionContext::new("run-cli-readonly"));
    let call = ToolCall::new("c1", "run_cli", json!({"program": "ls"}));
    let results = executor.execute_batch(&[call]).await;
    let output: Value = serde_json::from_str(result_content(&results)).unwrap();
    assert_eq!(output["exit_code"], json!(0));
    assert!(output["stdout"].as_str().unwrap().contains("listed.txt"));

    std::fs::remove_dir_all(&jail).ok();
}

#[tokio::test]
async fn cli_evidence_records_command_and_counts_never_env() {
    let jail = temp_jail("evidence");
    std::fs::write(jail.join("counted.txt"), "1234567890").unwrap();
    let records = Arc::new(Mutex::new(Vec::new()));
    let sink: CliEvidenceSink = {
        let records = Arc::clone(&records);
        Arc::new(move |record: &CliExecutionRecord| {
            records.lock().unwrap().push(record.clone());
        })
    };
    let tool = CliTool::new(
        CliPolicy::new(&jail, ["cat"])
            .unwrap()
            .with_env_allowlist(["PATH"]),
    )
    .with_evidence_sink(sink);

    let output = tool
        .call(json!({"program": "cat", "args": ["counted.txt"]}))
        .await
        .unwrap();
    assert_eq!(output["exit_code"], json!(0));

    let records = records.lock().unwrap();
    assert_eq!(records.len(), 1, "every run leaves exactly one record");
    let record = &records[0];
    assert_eq!(record.program, "cat");
    assert_eq!(record.args, vec!["counted.txt".to_owned()]);
    assert_eq!(record.exit_code, Some(0));
    assert!(!record.timed_out && !record.truncated);
    assert_eq!(record.stdout_bytes, 10);
    // The serialized evidence shape carries no environment whatsoever.
    let value = serde_json::to_value(record).unwrap();
    let object = value.as_object().unwrap();
    assert!(
        !object.contains_key("env"),
        "evidence never contains raw env"
    );
    for key in [
        "program",
        "resolved",
        "args",
        "cwd",
        "shell",
        "exit_code",
        "timed_out",
        "truncated",
        "duration_ms",
        "stdout_bytes",
        "stderr_bytes",
    ] {
        assert!(object.contains_key(key), "evidence carries `{key}`");
    }

    std::fs::remove_dir_all(&jail).ok();
}

// ------------------------------------------------------------ Browser ----

fn virtual_pages() -> VirtualBrowserDriver {
    VirtualBrowserDriver::new([
        (
            "https://example.com/",
            VirtualPage::new("Home", "Welcome to the home page")
                .with_link("#next", "https://example.com/next")
                .with_input("#q"),
        ),
        (
            "https://example.com/next",
            VirtualPage::new("Next", "Second page text"),
        ),
    ])
}

#[tokio::test]
async fn virtual_browser_full_flow_and_effect_classes() {
    let controller = BrowserController::new(
        Arc::new(virtual_pages()),
        BrowserPolicy::new(["https://example.com/"]).unwrap(),
    );
    let navigate = BrowserNavigateTool::new(controller.clone());
    let read = BrowserReadTool::new(controller.clone());
    let click = BrowserClickTool::new(controller.clone());
    let type_tool = BrowserTypeTool::new(controller.clone());
    let screenshot = BrowserScreenshotTool::new(controller.clone());

    // R0.7 classes: reads are ReadOnly, world-mutating input is Compensatable.
    assert_eq!(navigate.effect(), Effect::ReadOnly);
    assert_eq!(read.effect(), Effect::ReadOnly);
    assert_eq!(screenshot.effect(), Effect::ReadOnly);
    assert_eq!(click.effect(), Effect::Compensatable);
    assert_eq!(type_tool.effect(), Effect::Compensatable);

    let landed = navigate
        .call(json!({"url": "https://example.com/"}))
        .await
        .unwrap();
    assert_eq!(landed["title"], json!("Home"));

    let page = read.call(json!({})).await.unwrap();
    assert_eq!(page["text"], json!("Welcome to the home page"));
    assert_eq!(page["truncated"], json!(false));

    let typed = type_tool
        .call(json!({"selector": "#q", "text": "rust harness"}))
        .await
        .unwrap();
    assert_eq!(typed["typed"], json!(12));

    let clicked = click.call(json!({"selector": "#next"})).await.unwrap();
    assert_eq!(clicked["url"], json!("https://example.com/next"));
    let page = read.call(json!({})).await.unwrap();
    assert_eq!(page["text"], json!("Second page text"));

    let shot = screenshot.call(json!({})).await.unwrap();
    assert_eq!(shot["bytes"], json!(VIRTUAL_SCREENSHOT.len()));
    assert_eq!(
        hex_decode(shot["data_hex"].as_str().unwrap()),
        VIRTUAL_SCREENSHOT.to_vec(),
        "hex payloads round-trip"
    );

    // Typing into an unscripted element is an explicit refusal.
    let error = type_tool
        .call(json!({"selector": "#missing", "text": "nope"}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no input matching"), "{error}");
}

#[tokio::test]
async fn browser_url_policy_refuses_off_allowlist_navigation() {
    let controller = BrowserController::new(
        Arc::new(virtual_pages()),
        BrowserPolicy::new(["https://example.com/"]).unwrap(),
    );
    let navigate = BrowserNavigateTool::new(controller);

    let error = navigate
        .call(json!({"url": "https://evil.example/"}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("allowlist"), "{error}");
    // A bare prefix mismatch is refused even for scripted pages.
    let error = navigate
        .call(json!({"url": "http://example.com/"}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("allowlist"), "{error}");
}

#[tokio::test]
async fn browser_action_ceiling_counts_clicks_and_types() {
    let controller = BrowserController::new(
        Arc::new(virtual_pages()),
        BrowserPolicy::new(["https://example.com/"])
            .unwrap()
            .with_max_actions(1)
            .unwrap(),
    );
    let navigate = BrowserNavigateTool::new(controller.clone());
    let click = BrowserClickTool::new(controller.clone());
    let type_tool = BrowserTypeTool::new(controller);

    navigate
        .call(json!({"url": "https://example.com/"}))
        .await
        .unwrap();
    click.call(json!({"selector": "#next"})).await.unwrap();
    // The second world-mutating action — of either kind — hits the ceiling.
    let error = type_tool
        .call(json!({"selector": "#q", "text": "over the ceiling"}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("action ceiling"), "{error}");
}

#[tokio::test]
async fn browser_dom_text_is_byte_capped() {
    let driver = VirtualBrowserDriver::new([(
        "https://example.com/",
        VirtualPage::new("Big", "data ".repeat(2000)),
    )]);
    let controller = BrowserController::new(
        Arc::new(driver),
        BrowserPolicy::new(["https://example.com/"])
            .unwrap()
            .with_max_dom_bytes(1024)
            .unwrap(),
    );
    let navigate = BrowserNavigateTool::new(controller.clone());
    let read = BrowserReadTool::new(controller);

    navigate
        .call(json!({"url": "https://example.com/"}))
        .await
        .unwrap();
    let page = read.call(json!({})).await.unwrap();
    assert_eq!(page["truncated"], json!(true));
    assert_eq!(page["bytes"], json!(1024));
    assert_eq!(page["text"].as_str().unwrap().len(), 1024);
}

#[tokio::test]
async fn browser_compensatable_actions_need_a_registered_rollback() {
    let controller = BrowserController::new(
        Arc::new(virtual_pages()),
        BrowserPolicy::new(["https://example.com/"]).unwrap(),
    );
    let navigate = BrowserNavigateTool::new(controller.clone());
    navigate
        .call(json!({"url": "https://example.com/"}))
        .await
        .unwrap();

    let scope = "run-browser";
    let call = ToolCall::new("c1", "browser_click", json!({"selector": "#next"}));
    let mut registry = ToolRegistry::new();
    registry.register(BrowserClickTool::new(controller));

    // No rollback handler registered: the guarded executor refuses.
    let denied = ToolExecutor::new(registry.clone())
        .with_effect_admission(EffectAdmissionContext::new(scope));
    let results = denied.execute_batch(std::slice::from_ref(&call)).await;
    assert!(result_content(&results).contains("rollback handler"));

    // A registered handler for the effect kind admits the click.
    let mut compensations = CompensationRegistry::new();
    let rollback: CompensationHandler = Arc::new(|output| Ok(json!({"undone": output})));
    compensations.register("browser_click", rollback);
    let allowed = ToolExecutor::new(registry).with_effect_admission(
        EffectAdmissionContext::new(scope).with_compensations(compensations),
    );
    let results = allowed.execute_batch(&[call]).await;
    let output: Value = serde_json::from_str(result_content(&results)).unwrap();
    assert_eq!(output["url"], json!("https://example.com/next"));
}

#[tokio::test]
async fn cdp_driver_frame_commands_are_honestly_unsupported() {
    // Endpoint validation is fail-closed.
    assert!(CdpDriver::new("ftp://127.0.0.1:9222").is_err());

    let driver = CdpDriver::new("http://127.0.0.1:9222/").unwrap();
    // Frame commands refuse without touching the network: no ws transport
    // exists in this crate yet.
    let error = driver.navigate("https://example.com/").await.unwrap_err();
    let message = error.to_string();
    assert!(message.contains("unsupported"), "{message}");
    assert!(message.contains("WebSocket"), "{message}");
    assert_eq!(driver.current_url(), None);
}

// ------------------------------------------------------------ Computer ----

fn computer_jail(tag: &str) -> PathBuf {
    temp_jail(&format!("computer-{tag}"))
}

#[tokio::test]
async fn computer_interaction_is_disabled_by_default_even_when_approved() {
    let jail = computer_jail("disabled");
    let driver = Arc::new(NullComputerDriver::new(b"fake-png".to_vec()));
    let controller = ComputerController::new(driver.clone(), ComputerPolicy::new(&jail).unwrap());
    let click = ComputerClickTool::new(controller);
    assert_eq!(click.effect(), Effect::NonIdempotent);

    // Direct dispatch: the policy refuses before the driver is touched.
    let error = click.call(json!({"x": 10, "y": 20})).await.unwrap_err();
    assert!(error.to_string().contains("disabled by policy"), "{error}");

    // Guarded dispatch with a *valid* approval token: the policy refusal
    // sits inside the body, so approval cannot launder a disabled action.
    let scope = "run-computer-disabled";
    let call = ToolCall::new("c1", "computer_click", json!({"x": 10, "y": 20}));
    let mut registry = ToolRegistry::new();
    registry.register(click);
    let request = registry
        .get("computer_click")
        .unwrap()
        .effect_request(&call);
    let approval = ApprovalToken::approve(request.effect_id(scope), "ops:amjad");
    let executor = ToolExecutor::new(registry)
        .with_effect_admission(EffectAdmissionContext::new(scope).with_approvals([approval]));
    let results = executor.execute_batch(&[call]).await;
    assert!(result_content(&results).contains("disabled by policy"));
    assert!(
        driver.interaction_log().is_empty(),
        "no interaction ever reached the driver"
    );

    std::fs::remove_dir_all(&jail).ok();
}

#[tokio::test]
async fn computer_null_driver_bounds_and_rate_limit() {
    let jail = computer_jail("bounds");
    let driver = Arc::new(NullComputerDriver::new(b"fake-png".to_vec()));
    let policy = ComputerPolicy::new(&jail)
        .unwrap()
        .with_interaction(true)
        .with_screen_bounds(1920, 1080)
        .unwrap()
        .with_min_interval(Duration::from_millis(50))
        .unwrap();
    let controller = ComputerController::new(driver.clone(), policy);
    let click = ComputerClickTool::new(controller.clone());
    let type_tool = ComputerTypeTool::new(controller);

    let clicked = click.call(json!({"x": 10, "y": 20})).await.unwrap();
    assert_eq!(clicked["clicked"], json!([10, 20]));

    // Coordinates outside the declared bounds never reach the driver.
    let error = click.call(json!({"x": 2000, "y": 10})).await.unwrap_err();
    assert!(error.to_string().contains("bounds"), "{error}");
    let error = click.call(json!({"x": -1, "y": 10})).await.unwrap_err();
    assert!(error.to_string().contains("bounds"), "{error}");

    // Interactions are rate-limited across tools on one controller.
    let error = type_tool
        .call(json!({"text": "too fast"}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("rate limit"), "{error}");
    tokio::time::sleep(Duration::from_millis(70)).await;
    type_tool.call(json!({"text": "hello"})).await.unwrap();

    assert_eq!(
        driver.interaction_log(),
        vec!["click:10,20".to_owned(), "type:hello".to_owned()]
    );

    std::fs::remove_dir_all(&jail).ok();
}

#[tokio::test]
async fn computer_click_is_approval_gated_when_enabled() {
    let jail = computer_jail("approval");
    let driver = Arc::new(NullComputerDriver::new(b"fake-png".to_vec()));
    let policy = ComputerPolicy::new(&jail)
        .unwrap()
        .with_interaction(true)
        .with_screen_bounds(1920, 1080)
        .unwrap()
        .with_min_interval(Duration::ZERO)
        .unwrap();
    let controller = ComputerController::new(driver, policy);

    let scope = "run-computer-approval";
    let call = ToolCall::new("c1", "computer_click", json!({"x": 5, "y": 6}));
    let mut registry = ToolRegistry::new();
    registry.register(ComputerClickTool::new(controller));
    let request = registry
        .get("computer_click")
        .unwrap()
        .effect_request(&call);

    let denied = ToolExecutor::new(registry.clone())
        .with_effect_admission(EffectAdmissionContext::new(scope));
    let results = denied.execute_batch(std::slice::from_ref(&call)).await;
    assert!(result_content(&results).contains("effect admission denied"));

    let approval = ApprovalToken::approve(request.effect_id(scope), "ops:amjad");
    let allowed = ToolExecutor::new(registry)
        .with_effect_admission(EffectAdmissionContext::new(scope).with_approvals([approval]));
    let results = allowed.execute_batch(&[call]).await;
    let output: Value = serde_json::from_str(result_content(&results)).unwrap();
    assert_eq!(output["clicked"], json!([5, 6]));

    std::fs::remove_dir_all(&jail).ok();
}

#[tokio::test]
async fn computer_screenshot_is_capped_and_read_only() {
    let jail = computer_jail("screenshot");
    let driver = Arc::new(NullComputerDriver::new(vec![0x89, 0x50, 0x4e, 0x47]));
    let controller = ComputerController::new(driver, ComputerPolicy::new(&jail).unwrap());
    let screenshot = ComputerScreenshotTool::new(controller);
    assert_eq!(screenshot.effect(), Effect::ReadOnly);

    let shot = screenshot.call(json!({})).await.unwrap();
    assert_eq!(shot["bytes"], json!(4));
    assert_eq!(
        hex_decode(shot["data_hex"].as_str().unwrap()),
        vec![0x89, 0x50, 0x4e, 0x47]
    );

    // A driver returning more than the cap is refused, not truncated.
    let oversized = Arc::new(NullComputerDriver::new(vec![0u8; 16]));
    let controller = ComputerController::new(
        oversized,
        ComputerPolicy::new(&jail)
            .unwrap()
            .with_max_screenshot_bytes(8)
            .unwrap(),
    );
    let error = ComputerScreenshotTool::new(controller)
        .call(json!({}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cap"), "{error}");

    std::fs::remove_dir_all(&jail).ok();
}
