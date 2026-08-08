//! Golden-file dataset round-trip tests: the JSONL format loads, validates
//! its schema version, and re-serializes byte-identically.

use rusty_eval::{Dataset, EvalError, DATASET_FORMAT_VERSION};

const GOLDEN: &str = include_str!("golden/math_tools_v1.jsonl");

#[test]
fn golden_dataset_loads() {
    let dataset = Dataset::from_jsonl(GOLDEN).unwrap();
    assert_eq!(dataset.name(), "math-tools");
    assert_eq!(dataset.version(), "1.0.0");
    assert_eq!(dataset.cases().len(), 2);

    let add = &dataset.cases()[0];
    assert_eq!(add.id, "add-two-numbers");
    assert_eq!(add.tags, vec!["math", "smoke"]);
    assert_eq!(add.expect.tool_trajectory.len(), 1);
    assert_eq!(add.expect.tool_trajectory[0].name, "calculator");
    assert_eq!(
        add.expect.tool_trajectory[0].args.get("/op"),
        Some(&serde_json::json!("add"))
    );
    assert_eq!(add.expect.state.len(), 1);
    assert_eq!(add.expect.state[0].pointer, "/messages/3/content");

    let mul = &dataset.cases()[1];
    assert_eq!(mul.expect.tool_trajectory.len(), 2);
    assert_eq!(mul.expect.forbid_tools, vec!["shell"]);
    assert_eq!(mul.expect.max_latency_ms, Some(60_000));
    assert!(mul.expect.max_cost_usd.is_none());
}

#[test]
fn golden_dataset_round_trips_byte_exact() {
    let dataset = Dataset::from_jsonl(GOLDEN).unwrap();
    assert_eq!(dataset.to_jsonl(), GOLDEN);
}

#[test]
fn save_and_reload_preserves_the_dataset() {
    let dataset = Dataset::from_jsonl(GOLDEN).unwrap();
    let path = std::env::temp_dir().join(format!(
        "rusty-eval-dataset-{}-{}.jsonl",
        std::process::id(),
        "roundtrip"
    ));
    dataset.save(&path).unwrap();
    let reloaded = Dataset::load(&path).unwrap();
    std::fs::remove_file(&path).ok();
    assert_eq!(dataset, reloaded);
}

#[test]
fn unsupported_format_version_is_refused() {
    let text = GOLDEN.replacen(
        &format!("\"format_version\":{DATASET_FORMAT_VERSION}"),
        "\"format_version\":99",
        1,
    );
    let error = Dataset::from_jsonl(&text).unwrap_err();
    assert!(
        matches!(
            error,
            EvalError::UnsupportedVersion {
                found: 99,
                supported: DATASET_FORMAT_VERSION
            }
        ),
        "unexpected error: {error}"
    );
}

#[test]
fn missing_header_is_an_error() {
    let error = Dataset::from_jsonl("{\"kind\":\"case\",\"id\":\"x\",\"input\":{}}\n").unwrap_err();
    assert!(error.to_string().contains("line 1"), "{error}");
    assert!(error.to_string().contains("before the header"), "{error}");
}

#[test]
fn malformed_json_reports_the_line_number() {
    let text = format!("{GOLDEN}{{not json}}\n");
    let error = Dataset::from_jsonl(&text).unwrap_err();
    assert!(error.to_string().contains("line 4"), "{error}");
}

#[test]
fn duplicate_case_ids_are_rejected() {
    let text = concat!(
        "{\"kind\":\"header\",\"format_version\":1,\"name\":\"d\",\"version\":\"1\"}\n",
        "{\"kind\":\"case\",\"id\":\"x\",\"input\":{}}\n",
        "{\"kind\":\"case\",\"id\":\"x\",\"input\":{}}\n",
    );
    let error = Dataset::from_jsonl(text).unwrap_err();
    assert!(
        error.to_string().contains("duplicate case id `x`"),
        "{error}"
    );
}

#[test]
fn blank_lines_and_whitespace_are_tolerated() {
    let text = format!("\n  \n{GOLDEN}\n");
    let dataset = Dataset::from_jsonl(&text).unwrap();
    assert_eq!(dataset.cases().len(), 2);
}
