//! Seam catalog: machine-readable registry of interception seams.
//!
//! Every named seam has a dispatch mode, payload schema, return schema,
//! and decision variants. The catalog is generated from type definitions
//! and snapshot-guarded in CI (EP-02-S06).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How handlers for a seam are dispatched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DispatchMode {
    /// Handlers run in registration order; each receives payload and a `next()`
    /// continuation. A handler that returns without invoking `next()` owns the
    /// decision and later handlers do not run.
    Waterfall,
    /// Exactly one wrapping per registered handler, nested in registration
    /// order, with the innermost invocation being the operation body.
    Around,
    /// All handlers run in registration order and each sees accumulated state.
    Serial,
}

/// The verdict a seam handler may return.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DecisionVariant {
    /// Continue through the chain.
    Continue,
    /// Stop and fail with a structured reason.
    Reject,
    /// Skip remaining handlers and substitute a result.
    ShortCircuit,
}

/// One entry in the seam catalog.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SeamEntry {
    pub name: String,
    pub dispatch_mode: DispatchMode,
    /// JSON Schema (as a `serde_json::Value`) describing the payload type.
    pub payload_schema: serde_json::Value,
    /// JSON Schema (as a `serde_json::Value`) describing the return type.
    pub return_schema: serde_json::Value,
    pub decision_variants: Vec<DecisionVariant>,
}

/// The complete seam catalog.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SeamCatalog {
    pub version: String,
    pub entries: Vec<SeamEntry>,
}

// ---------------------------------------------------------------------------
// Schema-representative types
//
// These mirror the actual middleware payload types closely enough for
// contract documentation; they derive `JsonSchema` so the catalog can emit
// schemas without polluting the runtime types with a new derive.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, JsonSchema)]
enum RoleSchema {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct ChatMessageSchema {
    role: RoleSchema,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ToolCallSchema>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct ToolCallSchema {
    id: String,
    name: String,
    arguments: serde_json::Value,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct UsageSchema {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cached_tokens: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct NodeCallPayload {
    thread_id: String,
    node: String,
    step: usize,
    /// The state snapshot at the time of invocation.
    state: serde_json::Value,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct ModelCallPayload {
    thread_id: String,
    node: String,
    messages: Vec<ChatMessageSchema>,
    tools: Vec<serde_json::Value>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct ToolInvocationPayload {
    thread_id: String,
    node: String,
    call: ToolCallSchema,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct NodeOutputSchema {
    #[serde(default)]
    updates: std::collections::HashMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct ChatResponseSchema {
    message: ChatMessageSchema,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    usage: Option<UsageSchema>,
}

// ---------------------------------------------------------------------------
// Catalog generation
// ---------------------------------------------------------------------------

/// Generate the seam catalog from the current system's type definitions.
pub fn generate_catalog() -> SeamCatalog {
    SeamCatalog {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        entries: vec![
            SeamEntry {
                name: "node_run".to_owned(),
                dispatch_mode: DispatchMode::Around,
                payload_schema: serde_json::to_value(schemars::schema_for!(NodeCallPayload))
                    .unwrap(),
                return_schema: serde_json::to_value(schemars::schema_for!(NodeOutputSchema))
                    .unwrap(),
                decision_variants: vec![
                    DecisionVariant::Continue,
                    DecisionVariant::Reject,
                    DecisionVariant::ShortCircuit,
                ],
            },
            SeamEntry {
                name: "model_call".to_owned(),
                dispatch_mode: DispatchMode::Around,
                payload_schema: serde_json::to_value(schemars::schema_for!(ModelCallPayload))
                    .unwrap(),
                return_schema: serde_json::to_value(schemars::schema_for!(ChatResponseSchema))
                    .unwrap(),
                decision_variants: vec![
                    DecisionVariant::Continue,
                    DecisionVariant::Reject,
                    DecisionVariant::ShortCircuit,
                ],
            },
            SeamEntry {
                name: "tool_call".to_owned(),
                dispatch_mode: DispatchMode::Around,
                payload_schema: serde_json::to_value(schemars::schema_for!(ToolInvocationPayload))
                    .unwrap(),
                return_schema: serde_json::to_value(schemars::schema_for!(serde_json::Value))
                    .unwrap(),
                decision_variants: vec![
                    DecisionVariant::Continue,
                    DecisionVariant::Reject,
                    DecisionVariant::ShortCircuit,
                ],
            },
        ],
    }
}

/// Serialize the catalog to its canonical JSON form.
pub fn catalog_to_json(catalog: &SeamCatalog) -> serde_json::Result<String> {
    serde_json::to_string_pretty(catalog)
}

// ---------------------------------------------------------------------------
// Dispatch-site registration (AC 5)
// ---------------------------------------------------------------------------

/// Known dispatch sites in the current codebase.
///
/// These are the extension-relevant decision points that dispatch through
/// a cataloged seam. The conformance suite asserts that every registered
/// site corresponds to a catalog entry.
pub const KNOWN_DISPATCH_SITES: &[&str] = &["node_run", "model_call", "tool_call"];

/// Register a dispatch site.
///
/// In test builds this records the site name so the conformance suite can
/// verify that every dispatch point is cataloged. In release builds this
/// is a no-op.
#[macro_export]
macro_rules! register_dispatch_site {
    ($name:expr) => {
        #[cfg(test)]
        {
            let _ = $crate::seam_catalog::TEST_DISPATCH_SITES
                .lock()
                .unwrap()
                .push($name);
        }
    };
}

#[cfg(test)]
/// Test-only registry of dispatch sites observed at runtime.
pub static TEST_DISPATCH_SITES: std::sync::Mutex<Vec<&'static str>> =
    std::sync::Mutex::new(Vec::new());
