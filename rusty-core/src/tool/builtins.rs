//! Small native tools for local harnesses, examples, and conformance tests.
//!
//! These tools are deliberately credential-free and deterministic. Network
//! search belongs behind a connector; this module supplies the safe local
//! capabilities needed to prove the complete agent loop without pretending a
//! network provider exists.

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::Tool;
use crate::error::{Result, RustyError};
use crate::record::Effect;

/// Default maximum document size returned by [`SandboxedDocumentReaderTool`].
pub const DEFAULT_DOCUMENT_BYTES: usize = 256 * 1024;
/// Maximum query length accepted by [`KnowledgeSearchTool`].
pub const MAX_SEARCH_QUERY_BYTES: usize = 512;
/// Maximum number of search results returned by [`KnowledgeSearchTool`].
pub const MAX_SEARCH_RESULTS: usize = 20;

/// Pure arithmetic over two finite numbers.
#[derive(Debug, Clone, Copy, Default)]
pub struct CalculatorTool;

#[async_trait]
impl Tool for CalculatorTool {
    fn name(&self) -> &str {
        "calculator"
    }

    fn description(&self) -> &str {
        "Perform one bounded arithmetic operation on two finite numbers."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {"type": "string", "enum": ["add", "subtract", "multiply", "divide"]},
                "left": {"type": "number"},
                "right": {"type": "number"}
            },
            "required": ["operation", "left", "right"],
            "additionalProperties": false
        })
    }

    fn effect(&self) -> Effect {
        Effect::Pure
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let operation = required_string(&args, "operation")?;
        let left = required_finite_number(&args, "left")?;
        let right = required_finite_number(&args, "right")?;
        let result = match operation {
            "add" => left + right,
            "subtract" => left - right,
            "multiply" => left * right,
            "divide" if right != 0.0 => left / right,
            "divide" => return Err(RustyError::Tool("calculator division by zero".into())),
            other => {
                return Err(RustyError::Tool(format!(
                    "calculator operation `{other}` is not supported"
                )))
            }
        };
        if !result.is_finite() {
            return Err(RustyError::Tool(
                "calculator result is outside the finite number range".into(),
            ));
        }
        Ok(json!({"result": result}))
    }
}

/// Pure statistics over caller-provided text.
#[derive(Debug, Clone, Copy, Default)]
pub struct TextInspectorTool;

#[async_trait]
impl Tool for TextInspectorTool {
    fn name(&self) -> &str {
        "inspect_text"
    }

    fn description(&self) -> &str {
        "Count words, Unicode characters, bytes, and lines in supplied text."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"text": {"type": "string", "maxLength": 262144}},
            "required": ["text"],
            "additionalProperties": false
        })
    }

    fn effect(&self) -> Effect {
        Effect::Pure
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let text = required_string(&args, "text")?;
        if text.len() > DEFAULT_DOCUMENT_BYTES {
            return Err(RustyError::Tool(format!(
                "inspect_text input exceeds {DEFAULT_DOCUMENT_BYTES} bytes"
            )));
        }
        Ok(json!({
            "words": text.split_whitespace().count(),
            "characters": text.chars().count(),
            "bytes": text.len(),
            "lines": text.lines().count(),
        }))
    }
}

/// One immutable document searched by [`KnowledgeSearchTool`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeDocument {
    pub id: String,
    pub title: String,
    pub text: String,
}

/// Bounded lexical search over an immutable local document collection.
#[derive(Debug, Clone)]
pub struct KnowledgeSearchTool {
    documents: Vec<KnowledgeDocument>,
}

impl KnowledgeSearchTool {
    /// Build a search tool after validating stable document identities and
    /// bounded content. Results preserve this source order for equal scores.
    pub fn new(documents: Vec<KnowledgeDocument>) -> Result<Self> {
        if documents.len() > 1_024 {
            return Err(RustyError::Tool(
                "knowledge search accepts at most 1024 documents".into(),
            ));
        }
        let mut seen = std::collections::HashSet::new();
        let mut total = 0usize;
        for document in &documents {
            if document.id.is_empty()
                || document.id.len() > 128
                || !document
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
            {
                return Err(RustyError::Tool(format!(
                    "knowledge document id `{}` is invalid",
                    document.id
                )));
            }
            if !seen.insert(document.id.as_str()) {
                return Err(RustyError::Tool(format!(
                    "knowledge document id `{}` appears more than once",
                    document.id
                )));
            }
            if document.title.is_empty() || document.title.len() > 512 {
                return Err(RustyError::Tool(format!(
                    "knowledge document `{}` needs a title up to 512 bytes",
                    document.id
                )));
            }
            total = total.saturating_add(document.title.len() + document.text.len());
        }
        if total > 2 * 1024 * 1024 {
            return Err(RustyError::Tool(
                "knowledge search corpus exceeds 2 MiB".into(),
            ));
        }
        Ok(Self { documents })
    }
}

#[async_trait]
impl Tool for KnowledgeSearchTool {
    fn name(&self) -> &str {
        "search_knowledge"
    }

    fn description(&self) -> &str {
        "Search the agent's local reference collection and return bounded cited excerpts."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "minLength": 1, "maxLength": MAX_SEARCH_QUERY_BYTES},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_SEARCH_RESULTS, "default": 5}
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let query = required_string(&args, "query")?.trim();
        if query.is_empty() || query.len() > MAX_SEARCH_QUERY_BYTES {
            return Err(RustyError::Tool(format!(
                "search query must contain 1..={MAX_SEARCH_QUERY_BYTES} bytes"
            )));
        }
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .clamp(1, MAX_SEARCH_RESULTS as u64) as usize;
        let terms = query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>();
        let mut matches = self
            .documents
            .iter()
            .enumerate()
            .filter_map(|(position, document)| {
                let haystack = format!("{}\n{}", document.title, document.text).to_lowercase();
                let score = terms
                    .iter()
                    .filter(|term| haystack.contains(term.as_str()))
                    .count();
                (score > 0).then_some((score, position, document))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
        let results = matches
            .into_iter()
            .take(limit)
            .map(|(score, _, document)| {
                json!({
                    "id": document.id,
                    "title": document.title,
                    "score": score,
                    "excerpt": bounded_excerpt(&document.text, 480),
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({"query": query, "results": results}))
    }
}

/// Root-confined UTF-8 document reader.
#[derive(Debug, Clone)]
pub struct SandboxedDocumentReaderTool {
    root: PathBuf,
    max_bytes: usize,
}

impl SandboxedDocumentReaderTool {
    /// Create a reader rooted at an existing directory.
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        Self::with_max_bytes(root, DEFAULT_DOCUMENT_BYTES)
    }

    /// Create a reader with an explicit positive response boundary.
    pub fn with_max_bytes(root: impl AsRef<Path>, max_bytes: usize) -> Result<Self> {
        if max_bytes == 0 || max_bytes > 8 * 1024 * 1024 {
            return Err(RustyError::Tool(
                "document reader limit must be between 1 byte and 8 MiB".into(),
            ));
        }
        let root = std::fs::canonicalize(root.as_ref()).map_err(|error| {
            RustyError::Tool(format!("document root could not be opened: {error}"))
        })?;
        if !root.is_dir() {
            return Err(RustyError::Tool(
                "document reader root must be a directory".into(),
            ));
        }
        Ok(Self { root, max_bytes })
    }
}

#[async_trait]
impl Tool for SandboxedDocumentReaderTool {
    fn name(&self) -> &str {
        "read_document"
    }

    fn description(&self) -> &str {
        "Read a UTF-8 text, Markdown, JSON, CSV, HTML, or XML document from the configured workspace root."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"path": {"type": "string", "minLength": 1, "maxLength": 1024}},
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let relative = required_string(&args, "path")?;
        let path = Path::new(relative);
        if path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(RustyError::Tool(
                "read_document path must stay inside the configured root".into(),
            ));
        }
        let target = tokio::fs::canonicalize(self.root.join(path))
            .await
            .map_err(|error| RustyError::Tool(format!("document could not be opened: {error}")))?;
        if !target.starts_with(&self.root) {
            return Err(RustyError::Tool(
                "read_document refused a path outside the configured root".into(),
            ));
        }
        let metadata = tokio::fs::metadata(&target)
            .await
            .map_err(|error| RustyError::Tool(format!("document metadata failed: {error}")))?;
        if !metadata.is_file() {
            return Err(RustyError::Tool(
                "read_document target must be a regular file".into(),
            ));
        }
        if metadata.len() > self.max_bytes as u64 {
            return Err(RustyError::Tool(format!(
                "document exceeds the {} byte read boundary",
                self.max_bytes
            )));
        }
        let kind = document_kind(&target)?;
        let bytes = tokio::fs::read(&target)
            .await
            .map_err(|error| RustyError::Tool(format!("document read failed: {error}")))?;
        if bytes.len() > self.max_bytes {
            return Err(RustyError::Tool(format!(
                "document exceeds the {} byte read boundary",
                self.max_bytes
            )));
        }
        let content = String::from_utf8(bytes)
            .map_err(|_| RustyError::Tool("document is not valid UTF-8 text".into()))?;
        Ok(json!({
            "path": relative,
            "kind": kind,
            "bytes": content.len(),
            "content": content,
        }))
    }
}

fn required_string<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| RustyError::Tool(format!("`{name}` must be a string")))
}

fn required_finite_number(args: &Value, name: &str) -> Result<f64> {
    let value = args
        .get(name)
        .and_then(Value::as_f64)
        .ok_or_else(|| RustyError::Tool(format!("`{name}` must be a number")))?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(RustyError::Tool(format!("`{name}` must be finite")))
    }
}

fn bounded_excerpt(text: &str, max_chars: usize) -> String {
    let mut excerpt = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars {
        excerpt.push('…');
    }
    excerpt
}

fn document_kind(path: &Path) -> Result<&'static str> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "txt" => Ok("text"),
        "md" | "markdown" => Ok("markdown"),
        "json" => Ok("json"),
        "csv" => Ok("csv"),
        "html" | "htm" => Ok("html"),
        "xml" => Ok("xml"),
        _ => Err(RustyError::Tool(
            "read_document supports text, Markdown, JSON, CSV, HTML, and XML files".into(),
        )),
    }
}
