//! Golden parity fixtures for the render-intent union.
//!
//! `tests/golden/render_intents.json` is the shared contract between the
//! Rust derivation (`rusty_core::render_intent::render_intent`) and Studio's
//! TypeScript mirror (`studio/ui/src/lib/api/renderIntents.ts`): the same
//! journaled `(tool, arguments, result)` triple in, the same serialized
//! intent out. Both sides run these cases; drift on either side fails CI.
//!
//! `UPDATE_GOLDEN=1` regenerates the file — the diff is then the contract
//! change under review, and the Studio mirror must be updated in the same
//! change.

use std::path::PathBuf;

use rusty_agent_runtime::render_intent::render_intent;
use serde::Serialize;
use serde_json::{json, Value};

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name)
}

fn assert_golden(name: &str, value: &impl Serialize) {
    let rendered = format!("{}\n", serde_json::to_string_pretty(value).unwrap());
    let path = golden_path(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &rendered).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden file `{}`: {e}", path.display()));
    assert_eq!(
        rendered,
        expected,
        "contract drift in `{}` — if intentional, re-run with UPDATE_GOLDEN=1 \
         and review the diff (the Studio mirror must move in the same change)",
        path.display()
    );
}

/// One parity case: journaled evidence in, serialized intent out. Keep
/// values integral and cells scalar — see the module docs of
/// `rusty_core::render_intent` for the float/object formatting divergences
/// the JSON text level cannot share.
fn parity_cases() -> Value {
    json!({
        "version": 1,
        "cases": [
            {
                "name": "run_cli argv renders as a terminal",
                "tool": "run_cli",
                "arguments": {"program": "git", "args": ["status", "--short"]},
                "result": {
                    "program": "git", "resolved": "/usr/bin/git", "args": ["status", "--short"],
                    "cwd": ".", "shell": false, "exit_code": 0, "timed_out": false,
                    "truncated": false, "duration_ms": 12, "stdout_bytes": 11,
                    "stderr_bytes": 0, "stdout": " M README.md\n", "stderr": ""
                }
            },
            {
                "name": "run_cli shell payload renders the command string",
                "tool": "run_cli",
                "arguments": {"command": "ls | head -5"},
                "result": {
                    "program": "sh", "resolved": "/bin/sh", "args": ["ls | head -5"],
                    "cwd": "docs", "shell": true, "exit_code": 1, "timed_out": false,
                    "truncated": false, "duration_ms": 4, "stdout_bytes": 3,
                    "stderr_bytes": 9, "stdout": "a\nb", "stderr": "exit 1"
                }
            },
            {
                "name": "search_knowledge renders as a search",
                "tool": "search_knowledge",
                "arguments": {"query": "effect kernel", "limit": 5},
                "result": {
                    "query": "effect kernel",
                    "results": [
                        {"id": "doc-1", "title": "Effect kernel", "score": 2, "excerpt": "the effect taxonomy"},
                        {"id": "doc-2", "title": "Replay", "score": 1, "excerpt": "served from the journal"}
                    ]
                }
            },
            {
                "name": "session_search renders as a search keyed by event",
                "tool": "session_search",
                "arguments": {"query": "allowlist"},
                "result": {
                    "results": [{
                        "run_id": "run-1", "thread_id": "thread-1", "event_id": "run-1:7",
                        "seq": 7, "kind": "tool_call", "field": "output", "score": 1,
                        "excerpt": "not in the policy allowlist"
                    }]
                }
            },
            {
                "name": "read_document renders as a read",
                "tool": "read_document",
                "arguments": {"path": "notes/design.md"},
                "result": {
                    "path": "notes/design.md", "kind": "markdown", "bytes": 15,
                    "content": "# Design\n\nbody"
                }
            },
            {
                "name": "browser_navigate renders as a link",
                "tool": "browser_navigate",
                "arguments": {"url": "https://docs.rs/serde"},
                "result": {"url": "https://docs.rs/serde", "title": "Serde"}
            },
            {
                "name": "browser_read renders as a web card",
                "tool": "browser_read",
                "arguments": {},
                "result": {
                    "url": "https://example.test/", "bytes": 13, "truncated": false,
                    "text": "Hello, world."
                }
            },
            {
                "name": "session_trace renders as a causal table",
                "tool": "session_trace",
                "arguments": {"run_id": "run-1", "event_id": "run-1:4"},
                "result": {
                    "target": {"event_id": "run-1:4", "seq": 4, "kind": "tool_call", "effect": "read_only", "node_id": "tools", "status": "ok", "latency_ms": 9, "parent": "run-1:3"},
                    "ancestors": [{"event_id": "run-1:3", "seq": 3, "kind": "model_call", "effect": "non_idempotent", "node_id": "agent", "status": "ok", "latency_ms": 30, "parent": "run-1:2"}],
                    "descendants": [{"event_id": "run-1:6", "seq": 6, "kind": "node_output", "effect": "pure", "node_id": "tools", "status": "ok", "latency_ms": null, "parent": "run-1:4"}],
                    "truncated": false
                }
            },
            {
                "name": "calculator renders as a one-row table",
                "tool": "calculator",
                "arguments": {"operation": "multiply", "left": 6, "right": 7},
                "result": {"result": 42}
            },
            {
                "name": "inspect_text renders as a metrics table",
                "tool": "inspect_text",
                "arguments": {"text": "hello world"},
                "result": {"words": 2, "characters": 11, "bytes": 11, "lines": 1}
            },
            {
                "name": "a before/after result renders as a diff for any tool",
                "tool": "acme/update_record",
                "arguments": {"id": 42},
                "result": {"path": "records/42.json", "before": "old", "after": "new"}
            },
            {
                "name": "a url result renders as a link for any tool",
                "tool": "acme/get_ticket",
                "arguments": {"id": 1},
                "result": {"url": "https://acme.test/tickets/1", "title": "Ticket 1"}
            },
            {
                "name": "an array of flat objects renders as a table",
                "tool": "acme/list_tickets",
                "arguments": {},
                "result": [
                    {"id": 1, "state": "open"},
                    {"id": 2, "state": "closed", "owner": "sam"}
                ]
            },
            {
                "name": "an unknown tool falls back to generic",
                "tool": "mystery_tool",
                "arguments": {},
                "result": {"answer": 42}
            },
            {
                "name": "browser_screenshot declines to generic",
                "tool": "browser_screenshot",
                "arguments": {},
                "result": {"url": null, "bytes": 4, "data_hex": "deadbeef"}
            }
        ]
    })
}

#[test]
fn render_intent_parity_cases_match_the_shared_golden() {
    let cases = parity_cases();
    let rendered: Vec<Value> = cases["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| {
            let mut with_intent = case.clone();
            let intent = render_intent(
                case["tool"].as_str().unwrap(),
                &case["arguments"],
                &case["result"],
            );
            with_intent["intent"] = serde_json::to_value(intent).unwrap();
            with_intent
        })
        .collect();
    let mut golden = cases;
    golden["cases"] = Value::Array(rendered);
    assert_golden("render_intents.json", &golden);
}
