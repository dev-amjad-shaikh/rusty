//! The agent's memory tool surface (EP-06-S03): block writes and entry
//! appends as guarded, journaled tools — the agent persists what matters
//! through the same pipeline that governs every other action, with
//! write-time recall annotations attached at the moment the agent knows
//! them.
//!
//! Two tools, bound to one agent's scope by [`MemoryToolset`]:
//!
//! - **`memory_append_entry`** records a fact: content plus the optional
//!   annotations — a lookup `key`, `supersedes_key`, `trigger_phrases`,
//!   `importance` (0–10, schema-validated), and `confidence`. The
//!   annotations persist on the record itself: trigger phrases land as
//!   `tags` and importance as `priority`, so lane-one recall (EP-06-S04)
//!   reads them with zero model calls — annotation happens at write time.
//! - **`memory_replace_block`** edits the keyed slot the agent is committed
//!   to keep current: the write supersedes the slot's current record (the
//!   immutable chain is the version history), an optional
//!   `expected_memory_id` is the version guard (a mismatch names the
//!   current id — a model-visible conflict, never a silent overwrite), and
//!   the toolset's char limit is enforced with a model-visible refusal.
//!
//! Both tools declare [`EffectClass::Write`] + [`SandboxRequirement::None`]
//! (engine-state mutations executing in-process, per the placement rule)
//! and [`Effect::Idempotent`] (the store's content-addressed `put`
//! converges a replayed write by construction). They mount through the
//! in-process MCP bridge (EP-05-S09) once `Idempotent` joins the bridge's
//! allowed effects, and every call journals the receipted
//! ToolCall/ToolResult pair — a memory mutation is an auditable action,
//! never a silent side effect. Validation failures are model-visible
//! [`RustyError::Tool`] messages, and the catalog composes with
//! [`crate::tool_select::ValidatingTool`] so schema violations answer with
//! the conversational-repair envelope (EP-05-S03) before the tool runs.
//!
//! AC 5's side-session composition is construction, not instruction:
//! [`MemoryToolset::maintenance_registry`] holds exactly these tools, and a
//! consolidation or review-fork session's catalog is the `filtered`
//! narrowing of a fuller registry down to them — message sending and
//! scheduling are absent because nothing registered them.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::{Result, RustyError};
use crate::journal::Clock;
use crate::memory::{
    MemoryKind, MemoryProvenance, MemoryQuery, MemoryRecord, MemoryStore, ProvenanceAuthor,
    ScopeAddress, ValidityWindow,
};
use crate::record::Effect;
use crate::tool::{EffectClass, SandboxRequirement, Tool, ToolRegistry};

/// The entry-append tool's name (what the model emits in `tool_calls`).
pub const MEMORY_APPEND_ENTRY_TOOL: &str = "memory_append_entry";

/// The block-write tool's name.
pub const MEMORY_REPLACE_BLOCK_TOOL: &str = "memory_replace_block";

/// The inclusive importance band (0–10) the append tool accepts — stored
/// as the record's `priority`, lane-one recall's first rank input.
pub const MAX_IMPORTANCE: u32 = 10;

/// The default block char limit: the bounded, prompt-sized slot the
/// block-write tool enforces until a declared-block schema (EP-06-S01's
/// `DeclaredBlock` vocabulary) lands to carry one per block.
pub const DEFAULT_BLOCK_CHAR_LIMIT: usize = 2_000;

/// One argument-validation failure, model-visible: the field, the rule.
fn invalid_arg(field: &str, rule: impl std::fmt::Display) -> RustyError {
    RustyError::Tool(format!("`{field}` {rule}"))
}

/// The memory tool surface bound to one agent's scope: the store the tools
/// write through, the provenance they stamp, the clock they read.
///
/// Construction binds the identity — `agent_id` becomes every record's
/// [`ProvenanceAuthor::Agent`] and `scope` every record's address — so a
/// tool the agent holds can write only as that agent, at that scope: the
/// guard is what the tool *is*, not what it's told.
pub struct MemoryToolset {
    store: Arc<dyn MemoryStore>,
    author: ProvenanceAuthor,
    scope: ScopeAddress,
    clock: Clock,
    block_char_limit: usize,
}

impl std::fmt::Debug for MemoryToolset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryToolset")
            .field("author", &self.author.as_id_string())
            .field("scope", &self.scope)
            .field("clock", &self.clock)
            .field("block_char_limit", &self.block_char_limit)
            .finish()
    }
}

impl MemoryToolset {
    /// The surface for `agent_id` at `scope`, writing through `store` and
    /// timestamping through `clock` (the injected clock seam — logical in
    /// tests and recorded runs).
    pub fn new(
        store: Arc<dyn MemoryStore>,
        agent_id: impl Into<String>,
        scope: ScopeAddress,
        clock: Clock,
    ) -> Self {
        Self {
            store,
            author: ProvenanceAuthor::Agent {
                agent_id: agent_id.into(),
            },
            scope,
            clock,
            block_char_limit: DEFAULT_BLOCK_CHAR_LIMIT,
        }
    }

    /// Override the block-write char limit.
    pub fn with_block_char_limit(mut self, limit: usize) -> Self {
        self.block_char_limit = limit;
        self
    }

    /// The full agent-facing catalog: both memory tools, registered.
    pub fn registry(&self) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(MemoryAppendEntryTool {
            store: Arc::clone(&self.store),
            author: self.author.clone(),
            scope: self.scope.clone(),
            clock: self.clock.clone(),
        });
        registry.register(MemoryReplaceBlockTool {
            store: Arc::clone(&self.store),
            author: self.author.clone(),
            scope: self.scope.clone(),
            clock: self.clock.clone(),
            block_char_limit: self.block_char_limit,
        });
        registry
    }

    /// The side-session catalog (EP-06-S03 AC 5): the maintenance variants
    /// a consolidation or review-fork session mounts. Today the
    /// maintenance surface *is* the write pair — the catalog is narrow by
    /// construction (nothing else is registered), and a richer session
    /// registry narrows to it with `filtered` over [`maintenance_names`].
    pub fn maintenance_registry(&self) -> ToolRegistry {
        self.registry()
    }

    /// The shared store, for assertions and composition.
    pub fn store(&self) -> &Arc<dyn MemoryStore> {
        &self.store
    }
}

/// The names a side session's memory catalog holds — the `filtered`
/// predicate's name set.
pub fn maintenance_names() -> Vec<String> {
    vec![
        MEMORY_APPEND_ENTRY_TOOL.to_owned(),
        MEMORY_REPLACE_BLOCK_TOOL.to_owned(),
    ]
}

/// Read the args object field as a string.
fn arg_string(args: &Value, field: &str) -> Result<String> {
    args.get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| invalid_arg(field, "must be a string"))
}

/// The current (non-superseded, unexpired) record keyed `key` at `scope`,
/// when one exists.
async fn current_for_key(
    store: &Arc<dyn MemoryStore>,
    scope: &ScopeAddress,
    key: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<MemoryRecord>> {
    let query = MemoryQuery {
        scope: Some(scope.clone()),
        key: Some(key.to_owned()),
        ..MemoryQuery::default()
    };
    Ok(store.query(&query, now).await?.into_iter().next())
}

/// `memory_append_entry` — record a fact with its write-time recall
/// annotations.
///
/// [`Effect::Idempotent`]: the content address makes a replayed call
/// converge (`inserted: false`), so the journaled pair is the receipt.
struct MemoryAppendEntryTool {
    store: Arc<dyn MemoryStore>,
    author: ProvenanceAuthor,
    scope: ScopeAddress,
    clock: Clock,
}

impl std::fmt::Debug for MemoryAppendEntryTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryAppendEntryTool")
            .field("scope", &self.scope)
            .finish()
    }
}

#[async_trait]
impl Tool for MemoryAppendEntryTool {
    fn name(&self) -> &str {
        MEMORY_APPEND_ENTRY_TOOL
    }

    fn description(&self) -> &str {
        "Record a fact to memory: the content, an optional lookup `key`, an optional \
         `supersedes_key` naming the keyed record this one replaces, and the write-time \
         recall annotations — `trigger_phrases` (the situations where this should surface) \
         and `importance` (0–10). Annotate at the moment you know them: recall reads these \
         with zero model calls. Optional `confidence` (0, 1] defaults to 1.0; `kind` \
         defaults to `fact`."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "description": "What to remember — any JSON value, stored verbatim."
                },
                "kind": {
                    "type": "string",
                    "enum": ["fact", "preference", "example", "summary"],
                    "description": "What the record is; defaults to `fact`."
                },
                "key": {
                    "type": "string",
                    "description": "The named question this record answers — retrieval by key \
                                    never needs a model call."
                },
                "supersedes_key": {
                    "type": "string",
                    "description": "The keyed record this one replaces. The replaced record is \
                                    retained as evidence and filtered from default recall."
                },
                "trigger_phrases": {
                    "type": "array",
                    "items": {"type": "string", "minLength": 1},
                    "description": "The situations where this record should surface — stored \
                                    as recall tags."
                },
                "importance": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_IMPORTANCE,
                    "description": "0–10; stored as the record's priority, the assembly rank's \
                                    first input."
                },
                "confidence": {
                    "type": "number",
                    "exclusiveMinimum": 0,
                    "maximum": 1,
                    "description": "Your confidence in the claim, in (0, 1]; defaults to 1.0."
                }
            },
            "required": ["content"],
            "additionalProperties": false
        })
    }

    fn effect(&self) -> Effect {
        Effect::Idempotent
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::Write
    }

    fn sandbox_requirement(&self) -> SandboxRequirement {
        SandboxRequirement::None
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let content = args
            .get("content")
            .cloned()
            .ok_or_else(|| invalid_arg("content", "is required"))?;

        let kind = match args.get("kind") {
            None | Some(Value::Null) => MemoryKind::Fact,
            Some(value) => serde_json::from_value(value.clone()).map_err(|_| {
                invalid_arg("kind", "must be one of fact|preference|example|summary")
            })?,
        };

        let key = match args.get("key") {
            None | Some(Value::Null) => None,
            Some(_) => Some(arg_string(&args, "key")?),
        };

        let trigger_phrases = match args.get("trigger_phrases") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(phrases)) => {
                let mut out = Vec::with_capacity(phrases.len());
                for phrase in phrases {
                    let phrase = phrase
                        .as_str()
                        .ok_or_else(|| invalid_arg("trigger_phrases", "must be strings"))?;
                    if phrase.is_empty() {
                        return Err(invalid_arg("trigger_phrases", "must not be empty"));
                    }
                    out.push(phrase.to_owned());
                }
                out
            }
            Some(_) => {
                return Err(invalid_arg(
                    "trigger_phrases",
                    "must be an array of strings",
                ));
            }
        };

        let importance = match args.get("importance") {
            None | Some(Value::Null) => 0,
            Some(value) => {
                let importance = value
                    .as_u64()
                    .ok_or_else(|| invalid_arg("importance", "must be an integer in 0..=10"))?;
                u32::try_from(importance)
                    .ok()
                    .filter(|importance| *importance <= MAX_IMPORTANCE)
                    .ok_or_else(|| invalid_arg("importance", "must be in 0..=10"))?
            }
        };

        let confidence = match args.get("confidence") {
            None | Some(Value::Null) => 1.0,
            Some(value) => value
                .as_f64()
                .ok_or_else(|| invalid_arg("confidence", "must be a number in (0, 1]"))?,
        };

        let now = self.clock.now();
        let supersedes = match args.get("supersedes_key") {
            None | Some(Value::Null) => None,
            Some(_) => {
                let supersedes_key = arg_string(&args, "supersedes_key")?;
                let current =
                    current_for_key(&self.store, &self.scope, &supersedes_key, now).await?;
                Some(
                    current
                        .ok_or_else(|| {
                            RustyError::Tool(format!(
                                "`supersedes_key` names no live record: nothing keyed \
                                 `{supersedes_key}` at this scope — append first, or drop the \
                                 supersession"
                            ))
                        })?
                        .memory_id,
                )
            }
        };

        let mut record = MemoryRecord::new(
            kind,
            self.scope.clone(),
            MemoryProvenance {
                author: self.author.clone(),
                evidence: Default::default(),
                written_at: now,
            },
            confidence,
            ValidityWindow::starting(now),
            now,
            content,
        )?;
        if let Some(key) = key {
            record = record.with_key(key);
        }
        record.tags = trigger_phrases;
        record.priority = i64::from(importance);
        record.supersedes = supersedes;

        let inserted = self.store.put(&record).await?;
        Ok(json!({
            "memory_id": record.memory_id,
            "inserted": inserted,
            "key": record.key,
            "scope": record.scope,
            "supersedes": record.supersedes,
        }))
    }
}

/// `memory_replace_block` — rewrite the keyed slot, superseding its current
/// record under a version guard and a char limit.
struct MemoryReplaceBlockTool {
    store: Arc<dyn MemoryStore>,
    author: ProvenanceAuthor,
    scope: ScopeAddress,
    clock: Clock,
    block_char_limit: usize,
}

impl std::fmt::Debug for MemoryReplaceBlockTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryReplaceBlockTool")
            .field("scope", &self.scope)
            .field("block_char_limit", &self.block_char_limit)
            .finish()
    }
}

#[async_trait]
impl Tool for MemoryReplaceBlockTool {
    fn name(&self) -> &str {
        MEMORY_REPLACE_BLOCK_TOOL
    }

    fn description(&self) -> &str {
        "Rewrite the memory block keyed `key`: the new content supersedes the block's current \
         record (the chain is the version history; the old value stays as evidence). Pass \
         `expected_memory_id` — the id you last read — as the version guard: a mismatch means \
         someone wrote the block since, and the refusal names the current id so you can re-read \
         and retry. Content beyond the block's char limit is refused with the limit named."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The block's slot."
                },
                "content": {
                    "description": "The block's new value, replacing the current one."
                },
                "expected_memory_id": {
                    "type": "string",
                    "description": "The id you last read for this block — the version guard. A \
                                    mismatch refuses with the current id named."
                }
            },
            "required": ["key", "content"],
            "additionalProperties": false
        })
    }

    fn effect(&self) -> Effect {
        Effect::Idempotent
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::Write
    }

    fn sandbox_requirement(&self) -> SandboxRequirement {
        SandboxRequirement::None
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let key = arg_string(&args, "key")?;
        if key.is_empty() {
            return Err(invalid_arg("key", "must not be empty"));
        }
        let content = args
            .get("content")
            .cloned()
            .ok_or_else(|| invalid_arg("content", "is required"))?;

        let content_len = serde_json::to_string(&content)
            .map_err(|error| RustyError::Tool(format!("`content` does not serialize: {error}")))?
            .len();
        if content_len > self.block_char_limit {
            return Err(RustyError::Tool(format!(
                "the block's char limit is {} and this value is {content_len} — shorten it, or \
                 split what you keep across entries",
                self.block_char_limit
            )));
        }

        let now = self.clock.now();
        let current = current_for_key(&self.store, &self.scope, &key, now)
            .await?
            .ok_or_else(|| {
                RustyError::Tool(format!(
                    "no block keyed `{key}` at this scope — append the entry first"
                ))
            })?;

        if let Some(expected) = args.get("expected_memory_id") {
            let expected = expected
                .as_str()
                .ok_or_else(|| invalid_arg("expected_memory_id", "must be a string"))?;
            if expected != current.memory_id {
                return Err(RustyError::Tool(format!(
                    "version conflict on block `{key}`: expected `{expected}` but the current \
                     record is `{}` — re-read the block and retry",
                    current.memory_id
                )));
            }
        }

        // The edit preserves the block's kind and annotations: a rewrite
        // changes the value, not what the value is or when it surfaces.
        let mut record = MemoryRecord::new(
            current.kind,
            self.scope.clone(),
            MemoryProvenance {
                author: self.author.clone(),
                evidence: Default::default(),
                written_at: now,
            },
            current.confidence,
            ValidityWindow::starting(now),
            now,
            content,
        )?;
        record = record.with_key(key.clone());
        record.tags = current.tags.clone();
        record.priority = current.priority;
        record.supersedes = Some(current.memory_id.clone());

        let inserted = self.store.put(&record).await?;
        Ok(json!({
            "memory_id": record.memory_id,
            "inserted": inserted,
            "key": key,
            "scope": record.scope,
            "supersedes": record.supersedes,
        }))
    }
}
