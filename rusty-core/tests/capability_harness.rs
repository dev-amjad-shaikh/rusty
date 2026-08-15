use std::fs;

use async_trait::async_trait;
use rusty_agent_runtime::error::{Result, RustyError};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::tool::builtins::{
    CalculatorTool, KnowledgeDocument, KnowledgeSearchTool, SandboxedDocumentReaderTool,
    TextInspectorTool,
};
use rusty_agent_runtime::tool::{Tool, ToolRegistry};
use serde_json::{json, Value};

#[test]
fn executable_catalog_is_sorted_and_exact() {
    let mut tools = ToolRegistry::new();
    tools.register(TextInspectorTool);
    tools.register(CalculatorTool);

    let catalog = tools.capabilities().expect("valid built-in catalog");
    assert_eq!(
        catalog
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["calculator", "inspect_text"]
    );
    assert_eq!(catalog[0].effect, Effect::Pure);
    assert_eq!(
        catalog[0].parameters_schema["required"],
        json!(["operation", "left", "right"])
    );
    assert!(catalog[1].description.contains("Unicode"));
}

#[tokio::test]
async fn native_pack_executes_with_structured_results() {
    let calculator = CalculatorTool;
    assert_eq!(
        calculator
            .call(json!({"operation": "multiply", "left": 7, "right": 6}))
            .await
            .unwrap(),
        json!({"result": 42.0})
    );
    assert!(calculator
        .call(json!({"operation": "divide", "left": 1, "right": 0}))
        .await
        .unwrap_err()
        .to_string()
        .contains("division by zero"));

    let inspector = TextInspectorTool;
    assert_eq!(
        inspector
            .call(json!({"text": "one two\nthree"}))
            .await
            .unwrap(),
        json!({"words": 3, "characters": 13, "bytes": 13, "lines": 2})
    );
}

#[tokio::test]
async fn knowledge_search_is_bounded_ranked_and_cited() {
    let search = KnowledgeSearchTool::new(vec![
        KnowledgeDocument {
            id: "runtime".into(),
            title: "Rusty runtime".into(),
            text: "Durable graphs execute typed tools and record exact evidence.".into(),
        },
        KnowledgeDocument {
            id: "studio".into(),
            title: "Rusty Studio".into(),
            text: "Studio creates agents and opens their run traces.".into(),
        },
    ])
    .unwrap();

    let result = search
        .call(json!({"query": "Rusty tools evidence", "limit": 1}))
        .await
        .unwrap();
    assert_eq!(result["results"].as_array().unwrap().len(), 1);
    assert_eq!(result["results"][0]["id"], json!("runtime"));
    assert!(result["results"][0]["excerpt"]
        .as_str()
        .unwrap()
        .contains("exact evidence"));
}

#[tokio::test]
async fn document_reader_accepts_text_formats_and_refuses_escape() {
    let root = std::env::temp_dir().join(format!("rusty-doc-reader-{}", uuid::Uuid::new_v4()));
    let outside = std::env::temp_dir().join(format!("rusty-doc-outside-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("guide.md"), "# Guide\nUse exact evidence.").unwrap();
    fs::write(
        root.join("inventory.csv"),
        "name,effect\nreader,read_only\n",
    )
    .unwrap();
    fs::write(root.join("policy.json"), r#"{"approval":"required"}"#).unwrap();
    fs::write(&outside, "private").unwrap();
    let reader = SandboxedDocumentReaderTool::new(&root).unwrap();

    let result = reader.call(json!({"path": "guide.md"})).await.unwrap();
    assert_eq!(result["kind"], json!("markdown"));
    assert_eq!(result["content"], json!("# Guide\nUse exact evidence."));
    assert_eq!(
        reader.call(json!({"path": "inventory.csv"})).await.unwrap()["kind"],
        json!("csv")
    );
    assert_eq!(
        reader.call(json!({"path": "policy.json"})).await.unwrap()["kind"],
        json!("json")
    );

    let error = reader
        .call(json!({"path": format!("../{}", outside.file_name().unwrap().to_string_lossy())}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("inside the configured root"));

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(outside);
}

struct InvalidTool;

#[async_trait]
impl Tool for InvalidTool {
    fn name(&self) -> &str {
        "bad tool"
    }

    fn description(&self) -> &str {
        "Not advertisable."
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }

    async fn call(&self, _args: Value) -> Result<Value> {
        Err(RustyError::Tool("must not execute".into()))
    }
}

#[test]
fn invalid_executable_contract_never_becomes_catalog_truth() {
    let mut tools = ToolRegistry::new();
    tools.register(InvalidTool);
    let error = tools.capabilities().unwrap_err();
    assert!(error.to_string().contains("tool name"));
}
