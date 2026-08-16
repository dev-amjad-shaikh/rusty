//! Provider-neutral render intents: a closed presentation union derived
//! purely from journaled tool evidence.
//!
//! A [`crate::record::RunEventKind::ToolCall`] event journals everything a
//! frontend needs — the tool name, the arguments, the status, and the
//! result — but every rich rendering of that evidence used to be per-tool UI
//! code living in each frontend. This module moves the *decision* into the
//! crate: [`render_intent`] maps one journaled tool call onto one variant of
//! the closed [`RenderIntent`] union (terminal, diff, search, read, table,
//! link, web, generic), and any frontend renders the union with zero
//! per-tool code.
//!
//! The load-bearing invariant is **replay identity**: derivation is a pure,
//! total function of the journaled `(tool name, arguments, result)` triple.
//! It performs no I/O, reads no clock, and never panics, so exact replay —
//! which serves the journaled result instead of re-executing the tool —
//! renders byte-identical intents. Frontends that mirror these rules (Studio
//! does, in TypeScript) render a replayed run exactly as the live run
//! rendered.
//!
//! Boundedness follows the crate's evidence discipline
//! ([`crate::connector::http_api`]'s sanitize/clamp precedent): every string
//! an intent carries is control-stripped and clamped at a char boundary with
//! the explicit [`TRUNCATION_MARKER`], and collections are clamped to fixed
//! ceilings with the structured `truncated` flag recording that clamping
//! happened. A card can never become an unbounded dump of the journal.
//!
//! When nothing specific fits — an unknown tool, a malformed result, a
//! binary payload — derivation falls back to [`RenderIntent::Generic`] with
//! an honest `reason`, never a guess presented as a richer card.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::record::{EventStatus, PayloadRef, RunEvent, RunEventKind};

/// The explicit marker appended to any clamped intent text.
pub const TRUNCATION_MARKER: &str = "…[truncated]";

/// Maximum length of a block-text field (terminal streams, read excerpts,
/// diff sides, web text), in characters.
pub const MAX_INTENT_EXCERPT_CHARS: usize = 2_048;

/// Maximum length of a single-line field (labels, titles, URLs, table
/// cells), in characters.
pub const MAX_INTENT_LABEL_CHARS: usize = 160;

/// Maximum length of the generic fallback's result summary, in characters.
pub const MAX_INTENT_SUMMARY_CHARS: usize = 512;

/// Maximum search hits one intent carries (mirrors the search tools' own
/// ceiling, [`crate::session_query::MAX_SEARCH_RESULTS`]).
pub const MAX_INTENT_SEARCH_HITS: usize = 20;

/// Maximum rows one table intent carries.
pub const MAX_INTENT_TABLE_ROWS: usize = 50;

/// Maximum columns one table intent carries.
pub const MAX_INTENT_TABLE_COLUMNS: usize = 12;

/// One search result as a card renders it: a human label, an optional
/// machine reference (document id, event id), a bounded excerpt, and the
/// score the tool reported.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHitView {
    /// The hit's display label (document title, event id).
    pub label: String,
    /// The hit's stable reference, when the result carries one.
    pub reference: Option<String>,
    /// A bounded, control-stripped excerpt.
    pub excerpt: String,
    /// The tool-reported rank, when present.
    pub score: Option<u64>,
}

/// How one journaled tool call should render. Closed union: frontends match
/// exhaustively, and a variant is added only here, so every surface renders
/// the same evidence the same way.
///
/// Serialized with an internal `kind` tag (`{"kind": "terminal", …}`); every
/// field is bounded at derivation time, so a deserialized intent is already
/// safe to render.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RenderIntent {
    /// The honest fallback: no specific card fits. `reason` says why, and
    /// `summary` is a bounded, control-stripped rendering of the result so
    /// the evidence is still visible.
    Generic {
        /// The tool that was called (empty when the journal could not say).
        tool: String,
        /// Why no specific intent was derived.
        reason: String,
        /// A bounded summary of the journaled result.
        summary: String,
    },
    /// A local command execution (`run_cli`): the command line as invoked,
    /// the streams, and how the run ended.
    Terminal {
        /// The command line (program + arguments, or the shell payload).
        command: String,
        /// The working directory relative to the jail root, when recorded.
        cwd: Option<String>,
        /// The process exit code; `None` when the run was killed.
        exit_code: Option<i32>,
        /// Whether the run was killed for exceeding its timeout.
        timed_out: bool,
        /// Whether any stream was clamped — by the tool's own ceiling or by
        /// this derivation's bound.
        truncated: bool,
        /// Bounded, control-stripped stdout.
        stdout: String,
        /// Bounded, control-stripped stderr.
        stderr: String,
    },
    /// A before/after pair over one path (registry diffs, edit tools): any
    /// result object carrying string `before` and `after` fields.
    Diff {
        /// The path the diff applies to (`"result"` when the tool says no).
        path: String,
        /// Bounded, control-stripped before text.
        before: String,
        /// Bounded, control-stripped after text.
        after: String,
        /// Whether either side was clamped by this derivation's bound.
        truncated: bool,
    },
    /// A query and its bounded hit list (`search_knowledge`,
    /// `session_search`).
    Search {
        /// The query as the tool saw it.
        query: String,
        /// The hits, clamped to [`MAX_INTENT_SEARCH_HITS`].
        hits: Vec<SearchHitView>,
        /// Whether the hit list was clamped by this derivation's bound.
        truncated: bool,
    },
    /// A document read (`read_document`): the path, the detected kind, and a
    /// bounded excerpt of the content.
    Read {
        /// The path as requested.
        path: String,
        /// The document format the tool detected, when it did.
        format: Option<String>,
        /// A bounded, control-stripped excerpt of the content.
        excerpt: String,
        /// Whether the excerpt was clamped by this derivation's bound.
        truncated: bool,
    },
    /// Rectangular data: `session_trace` walks, `inspect_text` metrics,
    /// `calculator` results, and any result that is a JSON array of flat
    /// objects (connector pack operations included).
    Table {
        /// Column headers, clamped to [`MAX_INTENT_TABLE_COLUMNS`].
        columns: Vec<String>,
        /// Rows aligned to `columns`, clamped to [`MAX_INTENT_TABLE_ROWS`].
        rows: Vec<Vec<String>>,
        /// Whether rows or columns were clamped by this derivation's bound.
        truncated: bool,
    },
    /// A navigable URL (`browser_navigate`, connector results carrying a
    /// `url`).
    Link {
        /// The URL, control-stripped and clamped.
        url: String,
        /// The page or resource title, when the result carries one.
        title: Option<String>,
    },
    /// A web page read (`browser_read`): the URL and a bounded excerpt of
    /// the visible text.
    Web {
        /// The page URL, when the driver reported one.
        url: Option<String>,
        /// The page title, when the result carries one.
        title: Option<String>,
        /// A bounded, control-stripped excerpt of the visible text.
        excerpt: String,
        /// Whether the text was clamped — by the tool's own DOM ceiling or
        /// by this derivation's bound.
        truncated: bool,
    },
}

/// Derive the render intent for one journaled tool call.
///
/// Pure and total: the same `(tool, arguments, result)` triple always
/// derives the same intent, which is what makes replayed runs render
/// identically to live ones. `arguments` is the model-supplied argument
/// object and `result` the tool's journaled result value, exactly as
/// [`crate::replay::tool_call_request`] and the tool's return value record
/// them. Anything unrecognized degrades to [`RenderIntent::Generic`] with an
/// honest reason.
pub fn render_intent(tool: &str, arguments: &Value, result: &Value) -> RenderIntent {
    named_intent(tool, arguments, result)
        .or_else(|| structural_intent(result))
        .unwrap_or_else(|| {
            generic(
                tool,
                "no render intent matches the journaled result shape",
                result,
            )
        })
}

/// Derive the render intent for one journaled event.
///
/// Returns `None` for anything that is not a
/// [`RunEventKind::ToolCall`]. For tool calls the journaled request
/// (`{"tool": …, "arguments": …}`) and result are unpacked and handed to
/// [`render_intent`]; content-addressed payloads, missing results, and
/// failed calls degrade to honest [`RenderIntent::Generic`] cards rather
/// than disappearing from the evidence view.
pub fn render_intent_from_event(event: &RunEvent) -> Option<RenderIntent> {
    if event.kind != RunEventKind::ToolCall {
        return None;
    }
    let Some(input) = event.input.as_ref() else {
        return Some(generic_empty(
            "the journaled tool call carries no request payload",
        ));
    };
    let PayloadRef::Inline(request) = input else {
        return Some(generic_empty(
            "the tool call request is content-addressed; its bytes live in the journal artifact map",
        ));
    };
    let tool = request
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let arguments = request.get("arguments").cloned().unwrap_or(Value::Null);
    if event.status == EventStatus::Error {
        let summary = event
            .output
            .as_ref()
            .and_then(inline_value)
            .and_then(|output| output.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("the journal records the failure without an error payload");
        return Some(RenderIntent::Generic {
            tool: clean_line(&tool, MAX_INTENT_LABEL_CHARS),
            reason: "the tool call failed; the journal carries the error".to_owned(),
            summary: clean_line(summary, MAX_INTENT_SUMMARY_CHARS),
        });
    }
    let Some(output) = event.output.as_ref() else {
        return Some(generic(
            &tool,
            "the journaled tool call carries no result payload",
            &Value::Null,
        ));
    };
    let Some(result) = inline_value(output) else {
        return Some(generic(
            &tool,
            "the tool call result is content-addressed; its bytes live in the journal artifact map",
            &Value::Null,
        ));
    };
    Some(render_intent(&tool, &arguments, result))
}

fn inline_value(payload: &PayloadRef) -> Option<&Value> {
    match payload {
        PayloadRef::Inline(value) => Some(value),
        PayloadRef::Artifact(_) => None,
    }
}

/// Name-keyed derivations for the crate's own tools. Each returns `None`
/// when the journaled result does not carry the shape the tool is documented
/// to return, so a partial or middleware-rewritten result degrades honestly
/// instead of fabricating a card.
fn named_intent(tool: &str, arguments: &Value, result: &Value) -> Option<RenderIntent> {
    match tool {
        "run_cli" => terminal_intent(arguments, result),
        "search_knowledge" => knowledge_search_intent(result),
        "session_search" => session_search_intent(arguments, result),
        "read_document" => read_intent(arguments, result),
        "browser_navigate" => navigate_intent(arguments, result),
        "browser_read" => browser_read_intent(result),
        "browser_screenshot" => Some(generic(
            tool,
            "binary screenshot payloads stay in the journal; render intents carry text only",
            &Value::Null,
        )),
        "session_trace" => session_trace_intent(result),
        "inspect_text" => inspect_text_intent(result),
        "calculator" => calculator_intent(arguments, result),
        _ => None,
    }
}

fn terminal_intent(arguments: &Value, result: &Value) -> Option<RenderIntent> {
    let command = if let Some(shell) = arguments.get("command").and_then(Value::as_str) {
        shell.to_owned()
    } else {
        let program = arguments.get("program").and_then(Value::as_str)?;
        let mut line = program.to_owned();
        if let Some(entries) = arguments.get("args").and_then(Value::as_array) {
            for entry in entries {
                line.push(' ');
                line.push_str(entry.as_str().unwrap_or_default());
            }
        }
        line
    };
    let (stdout, out_clamped) = block_field(result, "stdout");
    let (stderr, err_clamped) = block_field(result, "stderr");
    let tool_truncated = result
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(RenderIntent::Terminal {
        command: clean_line(&command, MAX_INTENT_LABEL_CHARS),
        cwd: result
            .get("cwd")
            .and_then(Value::as_str)
            .map(|cwd| clean_line(cwd, MAX_INTENT_LABEL_CHARS)),
        exit_code: result
            .get("exit_code")
            .and_then(Value::as_i64)
            .and_then(|code| i32::try_from(code).ok()),
        timed_out: result
            .get("timed_out")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        truncated: tool_truncated || out_clamped || err_clamped,
        stdout,
        stderr,
    })
}

fn knowledge_search_intent(result: &Value) -> Option<RenderIntent> {
    let query = result.get("query").and_then(Value::as_str)?;
    let hits = result.get("results").and_then(Value::as_array)?;
    Some(search_intent(
        query,
        hits.iter().map(|hit| SearchHitView {
            label: clean_line(
                hit.get("title").and_then(Value::as_str).unwrap_or_default(),
                MAX_INTENT_LABEL_CHARS,
            ),
            reference: hit
                .get("id")
                .and_then(Value::as_str)
                .map(|id| clean_line(id, MAX_INTENT_LABEL_CHARS)),
            excerpt: clean_block(
                hit.get("excerpt")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                MAX_INTENT_EXCERPT_CHARS,
            )
            .0,
            score: hit.get("score").and_then(Value::as_u64),
        }),
    ))
}

fn session_search_intent(arguments: &Value, result: &Value) -> Option<RenderIntent> {
    let hits = result.get("results").and_then(Value::as_array)?;
    let query = arguments.get("query").and_then(Value::as_str)?;
    Some(search_intent(
        query,
        hits.iter().map(|hit| SearchHitView {
            label: clean_line(
                hit.get("event_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                MAX_INTENT_LABEL_CHARS,
            ),
            reference: hit
                .get("run_id")
                .and_then(Value::as_str)
                .map(|run| clean_line(run, MAX_INTENT_LABEL_CHARS)),
            excerpt: clean_block(
                hit.get("excerpt")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                MAX_INTENT_EXCERPT_CHARS,
            )
            .0,
            score: hit.get("score").and_then(Value::as_u64),
        }),
    ))
}

fn search_intent(query: &str, hits: impl Iterator<Item = SearchHitView>) -> RenderIntent {
    let hits: Vec<SearchHitView> = hits.collect();
    let truncated = hits.len() > MAX_INTENT_SEARCH_HITS;
    RenderIntent::Search {
        query: clean_line(query, MAX_INTENT_LABEL_CHARS),
        hits: hits.into_iter().take(MAX_INTENT_SEARCH_HITS).collect(),
        truncated,
    }
}

fn read_intent(arguments: &Value, result: &Value) -> Option<RenderIntent> {
    let content = result.get("content").and_then(Value::as_str)?;
    let (excerpt, truncated) = clean_block(content, MAX_INTENT_EXCERPT_CHARS);
    let path = result
        .get("path")
        .and_then(Value::as_str)
        .or_else(|| arguments.get("path").and_then(Value::as_str))
        .unwrap_or_default();
    Some(RenderIntent::Read {
        path: clean_line(path, MAX_INTENT_LABEL_CHARS),
        format: result
            .get("kind")
            .and_then(Value::as_str)
            .map(|kind| clean_line(kind, MAX_INTENT_LABEL_CHARS)),
        excerpt,
        truncated,
    })
}

fn navigate_intent(arguments: &Value, result: &Value) -> Option<RenderIntent> {
    let url = result
        .get("url")
        .and_then(Value::as_str)
        .or_else(|| arguments.get("url").and_then(Value::as_str))?;
    Some(RenderIntent::Link {
        url: clean_line(url, MAX_INTENT_LABEL_CHARS),
        title: result
            .get("title")
            .and_then(Value::as_str)
            .map(|title| clean_line(title, MAX_INTENT_LABEL_CHARS)),
    })
}

fn browser_read_intent(result: &Value) -> Option<RenderIntent> {
    let text = result.get("text").and_then(Value::as_str)?;
    let (excerpt, clamped) = clean_block(text, MAX_INTENT_EXCERPT_CHARS);
    let tool_truncated = result
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(RenderIntent::Web {
        url: result
            .get("url")
            .and_then(Value::as_str)
            .map(|url| clean_line(url, MAX_INTENT_LABEL_CHARS)),
        title: None,
        excerpt,
        truncated: tool_truncated || clamped,
    })
}

fn session_trace_intent(result: &Value) -> Option<RenderIntent> {
    let target = result.get("target")?;
    let ancestors = result
        .get("ancestors")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let descendants = result
        .get("descendants")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let columns = ["role", "seq", "kind", "node", "status", "latency_ms"]
        .iter()
        .map(|column| (*column).to_owned())
        .collect::<Vec<_>>();
    let mut rows = Vec::new();
    let mut push = |role: &str, event: &Value| {
        rows.push(vec![
            role.to_owned(),
            event.get("seq").map(cell_text).unwrap_or_default(),
            event
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            event
                .get("node_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            event
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            event.get("latency_ms").map(cell_text).unwrap_or_default(),
        ]);
    };
    for event in &ancestors {
        push("ancestor", event);
    }
    push("target", target);
    for event in &descendants {
        push("descendant", event);
    }
    let tool_truncated = result
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(table_intent(columns, rows, tool_truncated))
}

fn inspect_text_intent(result: &Value) -> Option<RenderIntent> {
    let metrics = ["words", "characters", "bytes", "lines"];
    let mut rows = Vec::new();
    for metric in metrics {
        let value = result.get(metric)?;
        rows.push(vec![metric.to_owned(), cell_text(value)]);
    }
    Some(table_intent(
        vec!["metric".to_owned(), "value".to_owned()],
        rows,
        false,
    ))
}

fn calculator_intent(arguments: &Value, result: &Value) -> Option<RenderIntent> {
    let answer = result.get("result")?;
    let operation = arguments.get("operation").and_then(Value::as_str)?;
    let left = arguments.get("left")?;
    let right = arguments.get("right")?;
    Some(table_intent(
        ["operation", "left", "right", "result"]
            .iter()
            .map(|column| (*column).to_owned())
            .collect(),
        vec![vec![
            operation.to_owned(),
            cell_text(left),
            cell_text(right),
            cell_text(answer),
        ]],
        false,
    ))
}

/// Shape-keyed derivations for tools the crate does not know by name —
/// connector pack operations, MCP tools, embedder tools. A result that
/// *looks like* a diff, a link, or a rectangular dataset renders as one;
/// anything else stays generic.
fn structural_intent(result: &Value) -> Option<RenderIntent> {
    if let Some(object) = result.as_object() {
        let before = object.get("before").and_then(Value::as_str);
        let after = object.get("after").and_then(Value::as_str);
        if let (Some(before), Some(after)) = (before, after) {
            let (before, before_clamped) = clean_block(before, MAX_INTENT_EXCERPT_CHARS);
            let (after, after_clamped) = clean_block(after, MAX_INTENT_EXCERPT_CHARS);
            return Some(RenderIntent::Diff {
                path: object
                    .get("path")
                    .and_then(Value::as_str)
                    .map(|path| clean_line(path, MAX_INTENT_LABEL_CHARS))
                    .unwrap_or_else(|| "result".to_owned()),
                before,
                after,
                truncated: before_clamped || after_clamped,
            });
        }
        if let Some(url) = object.get("url").and_then(Value::as_str) {
            return Some(RenderIntent::Link {
                url: clean_line(url, MAX_INTENT_LABEL_CHARS),
                title: object
                    .get("title")
                    .and_then(Value::as_str)
                    .map(|title| clean_line(title, MAX_INTENT_LABEL_CHARS)),
            });
        }
    }
    let entries = result.as_array()?;
    if entries.is_empty() || !entries.iter().all(Value::is_object) {
        return None;
    }
    let mut columns: Vec<String> = Vec::new();
    for entry in entries.iter().take(MAX_INTENT_TABLE_ROWS) {
        for key in entry.as_object()?.keys() {
            if !columns.contains(key) {
                columns.push(key.clone());
            }
        }
    }
    let columns_clamped = columns.len() > MAX_INTENT_TABLE_COLUMNS;
    columns.truncate(MAX_INTENT_TABLE_COLUMNS);
    let rows_clamped = entries.len() > MAX_INTENT_TABLE_ROWS;
    let rows = entries
        .iter()
        .take(MAX_INTENT_TABLE_ROWS)
        .map(|entry| {
            columns
                .iter()
                .map(|column| {
                    entry
                        .as_object()
                        .and_then(|object| object.get(column))
                        .map(cell_text)
                        .unwrap_or_default()
                })
                .collect()
        })
        .collect();
    Some(table_intent(columns, rows, columns_clamped || rows_clamped))
}

fn table_intent(columns: Vec<String>, rows: Vec<Vec<String>>, truncated: bool) -> RenderIntent {
    let rows_clamped = rows.len() > MAX_INTENT_TABLE_ROWS;
    let columns_clamped = columns.len() > MAX_INTENT_TABLE_COLUMNS;
    let columns = columns
        .into_iter()
        .take(MAX_INTENT_TABLE_COLUMNS)
        .map(|column| clean_line(&column, MAX_INTENT_LABEL_CHARS))
        .collect::<Vec<_>>();
    let rows = rows
        .into_iter()
        .take(MAX_INTENT_TABLE_ROWS)
        .map(|row| {
            row.into_iter()
                .map(|cell| clean_line(&cell, MAX_INTENT_LABEL_CHARS))
                .collect()
        })
        .collect();
    RenderIntent::Table {
        columns,
        rows,
        truncated: truncated || rows_clamped || columns_clamped,
    }
}

/// One table cell: scalars render as their JSON text, strings as
/// themselves, and structured values as compact JSON — always clamped
/// downstream by [`clean_line`].
fn cell_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn generic(tool: &str, reason: &str, result: &Value) -> RenderIntent {
    let summary = match result {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    RenderIntent::Generic {
        tool: clean_line(tool, MAX_INTENT_LABEL_CHARS),
        reason: reason.to_owned(),
        summary: clean_line(&summary, MAX_INTENT_SUMMARY_CHARS),
    }
}

fn generic_empty(reason: &str) -> RenderIntent {
    generic("", reason, &Value::Null)
}

/// Extract and clean a block-text field, reporting whether clamping was
/// applied. A missing or non-string field yields an empty string.
fn block_field(result: &Value, name: &str) -> (String, bool) {
    match result.get(name).and_then(Value::as_str) {
        Some(text) => clean_block(text, MAX_INTENT_EXCERPT_CHARS),
        None => (String::new(), false),
    }
}

/// Clean a multi-line field: control characters flatten to spaces — a
/// journaled payload must not smuggle terminal escapes into a card — except
/// newlines and tabs, which block renderings keep. Clamped at a char
/// boundary with [`TRUNCATION_MARKER`].
fn clean_block(text: &str, max_chars: usize) -> (String, bool) {
    let cleaned: String = text
        .chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                ' '
            } else {
                c
            }
        })
        .collect();
    clamp_chars(&cleaned, max_chars)
}

/// Clean a single-line field: every control character flattens to a space.
fn clean_line(text: &str, max_chars: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    clamp_chars(&cleaned, max_chars).0
}

fn clamp_chars(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_owned(), false);
    }
    let budget = max_chars.saturating_sub(TRUNCATION_MARKER.chars().count());
    let mut excerpt: String = text.chars().take(budget).collect();
    excerpt.push_str(TRUNCATION_MARKER);
    (excerpt, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{Clock, EventDraft, Journal};
    use crate::record::Effect;
    use serde_json::json;

    fn cli_result() -> Value {
        json!({
            "program": "git",
            "resolved": "/usr/bin/git",
            "args": ["status", "--short"],
            "cwd": ".",
            "shell": false,
            "exit_code": 0,
            "timed_out": false,
            "truncated": false,
            "duration_ms": 12,
            "stdout_bytes": 10,
            "stderr_bytes": 0,
            "stdout": " M README.md",
            "stderr": "",
        })
    }

    #[test]
    fn run_cli_derives_a_terminal_card() {
        let intent = render_intent(
            "run_cli",
            &json!({"program": "git", "args": ["status", "--short"]}),
            &cli_result(),
        );
        let RenderIntent::Terminal {
            command,
            exit_code,
            stdout,
            truncated,
            ..
        } = intent
        else {
            panic!("run_cli must derive a terminal intent: {intent:?}");
        };
        assert_eq!(command, "git status --short");
        assert_eq!(exit_code, Some(0));
        assert_eq!(stdout, " M README.md");
        assert!(!truncated);
    }

    #[test]
    fn run_cli_shell_mode_uses_the_command_string() {
        let mut result = cli_result();
        result["shell"] = json!(true);
        let intent = render_intent("run_cli", &json!({"command": "ls | head -5"}), &result);
        let RenderIntent::Terminal { command, .. } = intent else {
            panic!("expected a terminal intent");
        };
        assert_eq!(command, "ls | head -5");
    }

    #[test]
    fn terminal_streams_are_control_stripped_and_clamped() {
        let flood = "x".repeat(MAX_INTENT_EXCERPT_CHARS * 2);
        let mut result = cli_result();
        result["stdout"] = json!(format!("{flood}\u{1b}[31m"));
        result["stderr"] = json!("bell\u{7}kept\nnewline");
        let intent = render_intent("run_cli", &json!({"program": "git"}), &result);
        let RenderIntent::Terminal {
            stdout,
            stderr,
            truncated,
            ..
        } = intent
        else {
            panic!("expected a terminal intent");
        };
        assert!(truncated);
        assert!(stdout.ends_with(TRUNCATION_MARKER));
        assert!(stdout.chars().count() <= MAX_INTENT_EXCERPT_CHARS);
        assert!(!stdout.contains('\u{1b}'));
        assert!(!stderr.contains('\u{7}'));
        assert!(stderr.contains("kept\nnewline"));
    }

    #[test]
    fn search_knowledge_derives_a_search_card() {
        let result = json!({
            "query": "effect kernel",
            "results": [
                {"id": "doc-1", "title": "Effect kernel", "score": 2, "excerpt": "the effect taxonomy"},
                {"id": "doc-2", "title": "Replay", "score": 1, "excerpt": "served from the journal"},
            ],
        });
        let intent = render_intent(
            "search_knowledge",
            &json!({"query": "effect kernel"}),
            &result,
        );
        let RenderIntent::Search {
            query,
            hits,
            truncated,
        } = intent
        else {
            panic!("expected a search intent");
        };
        assert_eq!(query, "effect kernel");
        assert!(!truncated);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].label, "Effect kernel");
        assert_eq!(hits[0].reference.as_deref(), Some("doc-1"));
        assert_eq!(hits[0].score, Some(2));
    }

    #[test]
    fn session_search_derives_a_search_card_from_arguments() {
        let result = json!({
            "results": [{
                "run_id": "run-1", "thread_id": "thread-1", "event_id": "run-1:7",
                "seq": 7, "kind": "model_call", "field": "output", "score": 1,
                "excerpt": "matched text",
            }],
        });
        let intent = render_intent("session_search", &json!({"query": "matched"}), &result);
        let RenderIntent::Search { query, hits, .. } = intent else {
            panic!("expected a search intent");
        };
        assert_eq!(query, "matched");
        assert_eq!(hits[0].label, "run-1:7");
        assert_eq!(hits[0].reference.as_deref(), Some("run-1"));
    }

    #[test]
    fn read_document_derives_a_read_card() {
        let result = json!({
            "path": "notes/design.md",
            "kind": "markdown",
            "bytes": 11,
            "content": "# Design\n\nbody",
        });
        let intent = render_intent(
            "read_document",
            &json!({"path": "notes/design.md"}),
            &result,
        );
        let RenderIntent::Read {
            path,
            format,
            excerpt,
            truncated,
        } = intent
        else {
            panic!("expected a read intent");
        };
        assert_eq!(path, "notes/design.md");
        assert_eq!(format.as_deref(), Some("markdown"));
        assert_eq!(excerpt, "# Design\n\nbody");
        assert!(!truncated);
    }

    #[test]
    fn browser_navigate_derives_a_link_card() {
        let intent = render_intent(
            "browser_navigate",
            &json!({"url": "https://docs.rs/serde"}),
            &json!({"url": "https://docs.rs/serde", "title": "Serde"}),
        );
        let RenderIntent::Link { url, title } = intent else {
            panic!("expected a link intent");
        };
        assert_eq!(url, "https://docs.rs/serde");
        assert_eq!(title.as_deref(), Some("Serde"));
    }

    #[test]
    fn browser_read_derives_a_web_card_and_honors_the_tool_ceiling() {
        let intent = render_intent(
            "browser_read",
            &json!({}),
            &json!({"url": "https://example.test/", "bytes": 5, "truncated": true, "text": "hello"}),
        );
        let RenderIntent::Web {
            url,
            excerpt,
            truncated,
            ..
        } = intent
        else {
            panic!("expected a web intent");
        };
        assert_eq!(url.as_deref(), Some("https://example.test/"));
        assert_eq!(excerpt, "hello");
        assert!(truncated, "the tool's own DOM ceiling must carry through");
    }

    #[test]
    fn session_trace_derives_a_causal_table() {
        let result = json!({
            "target": {"seq": 4, "kind": "tool_call", "node_id": "tools", "status": "ok", "latency_ms": 9},
            "ancestors": [{"seq": 2, "kind": "model_call", "node_id": "agent", "status": "ok", "latency_ms": 30}],
            "descendants": [{"seq": 6, "kind": "node_output", "node_id": "tools", "status": "ok", "latency_ms": null}],
            "truncated": false,
        });
        let intent = render_intent(
            "session_trace",
            &json!({"run_id": "run-1", "event_id": "run-1:4"}),
            &result,
        );
        let RenderIntent::Table {
            columns,
            rows,
            truncated,
        } = intent
        else {
            panic!("expected a table intent");
        };
        assert_eq!(
            columns,
            vec!["role", "seq", "kind", "node", "status", "latency_ms"]
        );
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][0], "ancestor");
        assert_eq!(rows[1][0], "target");
        assert_eq!(rows[2][0], "descendant");
        assert_eq!(rows[1][2], "tool_call");
        assert!(!truncated);
    }

    #[test]
    fn inspect_text_and_calculator_derive_tables() {
        let inspected = render_intent(
            "inspect_text",
            &json!({"text": "hello world"}),
            &json!({"words": 2, "characters": 11, "bytes": 11, "lines": 1}),
        );
        let RenderIntent::Table { columns, rows, .. } = inspected else {
            panic!("expected a table intent");
        };
        assert_eq!(columns, vec!["metric", "value"]);
        assert_eq!(rows[0], vec!["words".to_owned(), "2".to_owned()]);

        let calculated = render_intent(
            "calculator",
            &json!({"operation": "multiply", "left": 6, "right": 7}),
            &json!({"result": 42}),
        );
        let RenderIntent::Table { columns, rows, .. } = calculated else {
            panic!("expected a table intent");
        };
        assert_eq!(columns, vec!["operation", "left", "right", "result"]);
        assert_eq!(rows[0][3], "42");
    }

    #[test]
    fn a_before_after_result_derives_a_diff_card_for_any_tool() {
        let intent = render_intent(
            "acme/update_record",
            &json!({}),
            &json!({"path": "records/42.json", "before": "old", "after": "new"}),
        );
        let RenderIntent::Diff {
            path,
            before,
            after,
            truncated,
        } = intent
        else {
            panic!("expected a diff intent");
        };
        assert_eq!(path, "records/42.json");
        assert_eq!(before, "old");
        assert_eq!(after, "new");
        assert!(!truncated);
    }

    #[test]
    fn a_url_result_derives_a_link_card_for_any_tool() {
        let intent = render_intent(
            "acme/get_ticket",
            &json!({}),
            &json!({"url": "https://acme.test/tickets/1", "title": "Ticket 1"}),
        );
        let RenderIntent::Link { url, title } = intent else {
            panic!("expected a link intent");
        };
        assert_eq!(url, "https://acme.test/tickets/1");
        assert_eq!(title.as_deref(), Some("Ticket 1"));
    }

    #[test]
    fn an_array_of_flat_objects_derives_a_table_card() {
        let intent = render_intent(
            "acme/list_tickets",
            &json!({}),
            &json!([
                {"id": 1, "state": "open"},
                {"id": 2, "state": "closed", "owner": "sam"},
            ]),
        );
        let RenderIntent::Table { columns, rows, .. } = intent else {
            panic!("expected a table intent");
        };
        assert_eq!(columns, vec!["id", "state", "owner"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][2], "", "a missing key renders as an empty cell");
        assert_eq!(rows[1][2], "sam");
    }

    #[test]
    fn table_rows_and_columns_are_clamped_with_the_flag() {
        let rows: Vec<Value> = (0..(MAX_INTENT_TABLE_ROWS + 10))
            .map(|n| json!({"n": n}))
            .collect();
        let intent = render_intent("acme/flood", &json!({}), &json!(rows));
        let RenderIntent::Table {
            rows, truncated, ..
        } = intent
        else {
            panic!("expected a table intent");
        };
        assert_eq!(rows.len(), MAX_INTENT_TABLE_ROWS);
        assert!(truncated);
    }

    #[test]
    fn unknown_tools_fall_back_to_an_honest_generic_card() {
        let intent = render_intent("mystery_tool", &json!({}), &json!({"answer": 42}));
        let RenderIntent::Generic {
            tool,
            reason,
            summary,
        } = intent
        else {
            panic!("expected a generic intent: {intent:?}");
        };
        assert_eq!(tool, "mystery_tool");
        assert!(!reason.is_empty(), "the fallback must say why");
        assert!(
            summary.contains("42"),
            "the evidence stays visible: {summary}"
        );
    }

    #[test]
    fn screenshots_decline_to_a_generic_card_with_a_reason() {
        let intent = render_intent(
            "browser_screenshot",
            &json!({}),
            &json!({"url": null, "bytes": 4, "data_hex": "deadbeef"}),
        );
        let RenderIntent::Generic {
            reason, summary, ..
        } = intent
        else {
            panic!("expected a generic intent");
        };
        assert!(reason.contains("binary"), "{reason}");
        assert!(
            !summary.contains("deadbeef"),
            "binary payloads stay in the journal"
        );
    }

    #[test]
    fn malformed_named_results_degrade_instead_of_panicking() {
        for tool in [
            "run_cli",
            "search_knowledge",
            "session_search",
            "read_document",
            "browser_navigate",
            "browser_read",
            "session_trace",
            "inspect_text",
            "calculator",
        ] {
            for result in [Value::Null, json!({}), json!("nope"), json!([1, 2])] {
                // Every outcome is acceptable except a panic; a malformed
                // shape must never reach a rich card with fabricated data.
                let intent = render_intent(tool, &Value::Null, &result);
                if let RenderIntent::Generic { reason, .. } = &intent {
                    assert!(!reason.is_empty());
                }
            }
        }
    }

    #[test]
    fn derivation_is_replay_identical() {
        let cases: Vec<(&str, Value, Value)> = vec![
            (
                "run_cli",
                json!({"program": "git", "args": ["status"]}),
                cli_result(),
            ),
            (
                "search_knowledge",
                json!({"query": "q"}),
                json!({"query": "q", "results": [{"id": "d", "title": "t", "score": 1, "excerpt": "e"}]}),
            ),
            (
                "read_document",
                json!({"path": "a.md"}),
                json!({"path": "a.md", "kind": "markdown", "bytes": 1, "content": "x"}),
            ),
            (
                "calculator",
                json!({"operation": "add", "left": 1, "right": 2}),
                json!({"result": 3}),
            ),
            ("unknown", json!({}), json!({"anything": true})),
        ];
        for (tool, args, result) in cases {
            let first = render_intent(tool, &args, &result);
            let second = render_intent(tool, &args, &result);
            assert_eq!(
                serde_json::to_value(&first).unwrap(),
                serde_json::to_value(&second).unwrap(),
                "the same journaled evidence must derive the same intent"
            );
        }
    }

    #[test]
    fn event_derivation_unpacks_the_journaled_request_shape() {
        let journal = Journal::new("run-1", "thread-1", Clock::System);
        journal.record(
            EventDraft::new(RunEventKind::ToolCall, Effect::ReadOnly)
                .input(crate::replay::tool_call_request(
                    "search_knowledge",
                    &json!({"query": "kernel"}),
                ))
                .output(json!({
                    "query": "kernel",
                    "results": [{"id": "d1", "title": "Kernel", "score": 1, "excerpt": "hit"}],
                })),
        );
        let snapshot = journal.snapshot();
        let intent = render_intent_from_event(&snapshot.events[0]).expect("a tool call derives");
        let RenderIntent::Search { query, hits, .. } = intent else {
            panic!("expected a search intent from the journaled event");
        };
        assert_eq!(query, "kernel");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn event_derivation_surfaces_failures_honestly() {
        let journal = Journal::new("run-1", "thread-1", Clock::System);
        journal.record(
            EventDraft::new(RunEventKind::ToolCall, Effect::NonIdempotent)
                .input(crate::replay::tool_call_request(
                    "run_cli",
                    &json!({"program": "rm"}),
                ))
                .status(EventStatus::Error)
                .output(json!({"error": "run_cli program `rm` is not in the policy allowlist"})),
        );
        let snapshot = journal.snapshot();
        let intent = render_intent_from_event(&snapshot.events[0]).expect("a tool call derives");
        let RenderIntent::Generic {
            tool,
            reason,
            summary,
        } = intent
        else {
            panic!("a failed call must render generically: {intent:?}");
        };
        assert_eq!(tool, "run_cli");
        assert!(reason.contains("failed"), "{reason}");
        assert!(summary.contains("allowlist"), "{summary}");
    }

    #[test]
    fn event_derivation_ignores_non_tool_events() {
        let journal = Journal::new("run-1", "thread-1", Clock::System);
        journal.record(EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure));
        let snapshot = journal.snapshot();
        assert!(render_intent_from_event(&snapshot.events[0]).is_none());
    }

    #[test]
    fn the_union_serializes_with_a_closed_kind_tag() {
        let intent = render_intent("run_cli", &json!({"program": "ls"}), &cli_result());
        let value = serde_json::to_value(&intent).unwrap();
        assert_eq!(value.get("kind").and_then(Value::as_str), Some("terminal"));
        let round: RenderIntent = serde_json::from_value(value).unwrap();
        assert_eq!(round, intent);
    }
}
