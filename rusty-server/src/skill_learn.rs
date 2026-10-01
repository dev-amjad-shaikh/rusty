//! A skill learns from the system it follows.
//!
//! A skill's `SKILL.md` may carry a fenced ```` ```learn ```` block: the
//! reads that teach it — each a read-only tool of a connected system with
//! its arguments — and the reference file they fill. `POST
//! /skills/{name}/learn` runs those reads through the live connection (or
//! the reads in the request), renders what came back as a Markdown
//! reference, and registers a new revision of the skill carrying it. The
//! agents that follow the skill read the reference on demand through
//! `skills.read`. A new topic on a system is a new skill declaring new
//! reads; the mechanism does not change.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Path as AxumPath, State as AxumState};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::skill::{SkillPackage, SkillSource, SkillVersion};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;

/// The most a reference may hold, so a learned table stays readable.
const REFERENCE_BYTES: usize = 60_000;
/// Rows shown per read unless the read says otherwise.
const DEFAULT_ROWS: usize = 60;
/// A cell is cut past this many characters.
const CELL_CHARS: usize = 160;

/// One read that teaches the skill: a read-only tool and its arguments.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LearnRead {
    pub title: String,
    pub tool: String,
    #[serde(default)]
    pub arguments: Value,
    #[serde(default)]
    pub rows: Option<usize>,
}

/// What a skill declares it learns: the reference it fills and the reads.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LearnPlan {
    #[serde(default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub reads: Vec<LearnRead>,
}

/// The fenced ```learn block of a skill's body, when it carries one.
pub(crate) fn declared_plan(body: &str) -> Option<LearnPlan> {
    let start = body.find("```learn")?;
    let after = &body[start + "```learn".len()..];
    let after = after.trim_start_matches([' ', '\t']);
    let after = after.strip_prefix('\n').unwrap_or(after);
    let end = after.find("```")?;
    serde_json::from_str::<LearnPlan>(after[..end].trim()).ok()
}

fn cell(value: &Value) -> String {
    let text = match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    let text = text.replace('\n', " ").replace('|', "\\|");
    if text.chars().count() > CELL_CHARS {
        let head: String = text.chars().take(CELL_CHARS).collect();
        format!("{head}…")
    } else {
        text
    }
}

/// A read's answer as Markdown: a table when it is a list of records (the
/// ServiceNow shape `{"result": [...]}` or a bare list), else the JSON,
/// bounded.
pub(crate) fn render(title: &str, value: &Value, rows: usize) -> String {
    let list = value
        .get("result")
        .and_then(Value::as_array)
        .or_else(|| value.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = format!("## {title}\n\n");
    if list.is_empty() || !list.iter().all(Value::is_object) {
        let text = serde_json::to_string_pretty(value).unwrap_or_default();
        let shown: String = text.chars().take(8_000).collect();
        out.push_str("```json\n");
        out.push_str(&shown);
        if shown.len() < text.len() {
            out.push_str("\n… (cut)");
        }
        out.push_str("\n```\n\n");
        return out;
    }
    let mut columns: Vec<String> = Vec::new();
    for row in list.iter().take(40) {
        if let Some(obj) = row.as_object() {
            for key in obj.keys() {
                if !columns.contains(key) {
                    columns.push(key.clone());
                }
            }
        }
    }
    // Sorted, so the reference reads the same whether serde_json's map is a
    // BTreeMap or insertion-ordered (the `preserve_order` feature).
    columns.sort();
    out.push_str(&format!(
        "{} record(s){}.\n\n",
        list.len(),
        if list.len() > rows {
            format!(", the first {rows} shown")
        } else {
            String::new()
        }
    ));
    out.push_str(&format!("| {} |\n", columns.join(" | ")));
    out.push_str(&format!("|{}\n", "---|".repeat(columns.len())));
    for row in list.iter().take(rows) {
        let obj = row.as_object().expect("checked above");
        let cells: Vec<String> = columns
            .iter()
            .map(|c| cell(obj.get(c).unwrap_or(&Value::Null)))
            .collect();
        out.push_str(&format!("| {} |\n", cells.join(" | ")));
    }
    out.push('\n');
    out
}

/// Run the reads through the live connections; each must be a read-only
/// tool that exists. Returns the reference text and the rows it holds.
/// Run the reads, render the reference, and stamp what each one answered
/// so a later check can tell whether the system has moved.
pub(crate) async fn learn(
    state: &AppState,
    skill: &str,
    reads: &[LearnRead],
) -> Result<(String, usize, Vec<crate::freshness::ReadStamp>), String> {
    if reads.is_empty() {
        return Err(
            "nothing to learn: the skill declares no ```learn block and the request names no reads"
                .to_owned(),
        );
    }
    let Some(cell) = &state.connection_tools else {
        return Err("this server mounts no connection tools".to_owned());
    };
    use rusty_agent_runtime::tool::ToolSource as _;
    let tools = cell.tools();
    let mut text = format!(
        "# What `{skill}` learned from the system\n\nLearned {} by the platform through the live connection. Every table below is what the system answered; a value not here was not seen.\n\n",
        chrono::Utc::now().format("%Y-%m-%d %H:%M UTC")
    );
    let mut rows_total = 0usize;
    let mut stamps = Vec::with_capacity(reads.len());
    for read in reads {
        let Some(tool) = tools.iter().find(|t| t.name() == read.tool) else {
            return Err(format!(
                "`{}` is not a tool of any connected system; catalog.tools names them",
                read.tool
            ));
        };
        if !matches!(tool.effect(), Effect::ReadOnly) {
            return Err(format!(
                "`{}` is not read-only; a skill learns from reads only",
                read.tool
            ));
        }
        let answer = tool
            .call(read.arguments.clone())
            .await
            .map_err(|e| format!("`{}` ({}): {e}", read.title, read.tool))?;
        stamps.push(crate::freshness::stamp_read(read, &answer));
        let rows = read.rows.unwrap_or(DEFAULT_ROWS).clamp(1, 500);
        let count = answer
            .get("result")
            .and_then(Value::as_array)
            .map(Vec::len)
            .or_else(|| answer.as_array().map(Vec::len))
            .unwrap_or(0);
        rows_total += count.min(rows);
        text.push_str(&format!("<!-- {} {} -->\n", read.tool, read.arguments));
        text.push_str(&render(&read.title, &answer, rows));
        if text.len() > REFERENCE_BYTES {
            text.truncate(REFERENCE_BYTES);
            text.push_str("\n\n… (the reference reached its size; later reads were cut)\n");
            break;
        }
    }
    Ok((text, rows_total, stamps))
}

/// The `SKILL.md` of a version, as the registry would parse it again: the
/// frontmatter from its metadata, then its body.
pub(crate) fn skill_md_of(version: &SkillVersion) -> String {
    let m = version.metadata();
    let mut out = String::from("---\n");
    out.push_str(&format!("name: {}\n", m.name));
    out.push_str(&format!(
        "description: {}\n",
        m.description.replace('\n', " ")
    ));
    if let Some(license) = &m.license {
        out.push_str(&format!("license: {license}\n"));
    }
    if !m.allowed_tools.is_empty() {
        out.push_str(&format!("allowed-tools: {}\n", m.allowed_tools.join(", ")));
    }
    if let Some(gate) = &m.eval_gate {
        out.push_str(&format!("eval-gate: {gate}\n"));
    }
    if let Some(compat) = &m.compatibility {
        out.push_str(&format!("compatibility: {compat}\n"));
    }
    out.push_str("---\n");
    out.push_str(version.body());
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[derive(Debug, Deserialize)]
pub(crate) struct LearnPayload {
    /// The reference to fill; the declared one, or `learned/<skill>.md`.
    #[serde(default)]
    reference: Option<String>,
    /// Reads to run instead of the skill's declared ones.
    #[serde(default)]
    reads: Vec<LearnRead>,
}

/// `POST /skills/{name}/learn` — run the skill's reads (or the request's)
/// through the live connections and register a revision carrying what came
/// back as a reference. The agents that follow the skill read it next run.
pub(crate) async fn learn_skill(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(name): AxumPath<String>,
    Json(payload): Json<LearnPayload>,
) -> Response {
    let Some(version) = state.skills.get(tenant.tenant(), &name).await else {
        return ApiError::not_found(format!("no skill `{name}`")).into_response();
    };
    let declared = declared_plan(version.body());
    let reads: Vec<LearnRead> = if payload.reads.is_empty() {
        declared
            .as_ref()
            .map(|p| p.reads.clone())
            .unwrap_or_default()
    } else {
        payload.reads.clone()
    };
    let reference = payload
        .reference
        .or_else(|| declared.as_ref().and_then(|p| p.reference.clone()))
        .unwrap_or_else(|| format!("learned/{name}.md"));
    let reference = reference
        .trim()
        .trim_start_matches("references/")
        .trim_start_matches('/')
        .to_owned();
    if reference.is_empty() || reference.contains("..") {
        return ApiError::bad_request(
            "the reference path must be a plain path beneath references/".to_owned(),
        )
        .into_response();
    }
    let (text, rows, stamps) = match learn(&state, &name, &reads).await {
        Ok(learned) => learned,
        Err(reason) => return ApiError::unprocessable(reason).into_response(),
    };
    // The next revision: the same SKILL.md, every reference it had, and
    // this one — new or replaced.
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    files.insert("SKILL.md".to_owned(), skill_md_of(&version).into_bytes());
    let kept_references: Vec<String> = version.reference_paths().map(str::to_owned).collect();
    for path in kept_references {
        if let Some(bytes) = version.reference(&path) {
            let key = if path.starts_with("references/") {
                path.clone()
            } else {
                format!("references/{path}")
            };
            files.insert(key, bytes.to_vec());
        }
    }
    let kept_assets: Vec<String> = version.asset_paths().map(str::to_owned).collect();
    for path in kept_assets {
        if let Some(bytes) = version.asset(&path) {
            let key = if path.starts_with("assets/") {
                path.clone()
            } else {
                format!("assets/{path}")
            };
            files.insert(key, bytes.to_vec());
        }
    }
    files.insert(format!("references/{reference}"), text.into_bytes());
    let package = match SkillPackage::from_files(files) {
        Ok(package) => package,
        Err(error) => {
            return ApiError::bad_request(format!("the learned skill does not package: {error}"))
                .into_response()
        }
    };
    let author = tenant.principal().name.clone();
    match state
        .skills
        .register(
            tenant.tenant(),
            package,
            SkillSource::Registry {
                name: "rusty-server".to_owned(),
            },
            author,
        )
        .await
    {
        Ok(registration) => {
            // What the system answered now is the mark a later check
            // compares against: learning is what makes a reference current.
            crate::freshness::keep(
                &state,
                tenant.tenant(),
                &crate::freshness::Stamp {
                    skill: name.clone(),
                    revision: registration.version.revision(),
                    reference: reference.clone(),
                    learned_at: chrono::Utc::now(),
                    reads: stamps,
                    checked_at: None,
                    stale: false,
                    because: Vec::new(),
                },
            )
            .await;
            let gate = if registration.already_registered {
                Value::Null
            } else {
                json!(
                    crate::skills::after_registration(
                        &state,
                        &tenant,
                        registration.version.name(),
                        registration.version.revision()
                    )
                    .await
                    .0
                )
            };
            Json(json!({
                "name": name,
                "revision": registration.version.revision(),
                "content_hash": registration.version.content_hash(),
                "reference": reference,
                "rows": rows,
                "reads": reads.len(),
                "already_registered": registration.already_registered,
                "gate": gate,
            }))
            .into_response()
        }
        Err(error) => ApiError::bad_request(error.to_string()).into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReferenceQuery {
    path: String,
}

/// `GET /skills/{name}/reference?path=…` — one reference, as Markdown.
pub(crate) async fn get_skill_reference(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(name): AxumPath<String>,
    axum::extract::Query(query): axum::extract::Query<ReferenceQuery>,
) -> Response {
    let Some(version) = state.skills.get(tenant.tenant(), &name).await else {
        return ApiError::not_found(format!("no skill `{name}`")).into_response();
    };
    let wanted = query.path.trim_start_matches('/');
    let bytes = version
        .reference(wanted)
        .or_else(|| version.reference(&format!("references/{wanted}")));
    match bytes {
        Some(bytes) => {
            // A reference the last check found stale still reads, and says so.
            let (text, _) = crate::freshness::as_read(
                &state,
                tenant.tenant(),
                &name,
                String::from_utf8_lossy(bytes).into_owned(),
            )
            .await;
            (
                [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
                text.into_bytes(),
            )
                .into_response()
        }
        None => ApiError::not_found(format!("skill `{name}` has no reference `{wanted}`"))
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::{declared_plan, render};
    use serde_json::json;

    #[test]
    fn a_skill_declares_what_it_learns_in_a_learn_block() {
        let body = "# Skill\n\nText.\n\n```learn\n{\"reference\": \"servicenow/resolutions.md\", \"reads\": [{\"title\": \"Resolved incidents\", \"tool\": \"servicenow.list-records\", \"arguments\": {\"table\": \"incident\"}, \"rows\": 20}]}\n```\n\nMore.";
        let plan = declared_plan(body).expect("a plan");
        assert_eq!(plan.reference.as_deref(), Some("servicenow/resolutions.md"));
        assert_eq!(plan.reads.len(), 1);
        assert_eq!(plan.reads[0].tool, "servicenow.list-records");
        assert_eq!(plan.reads[0].rows, Some(20));
        assert!(declared_plan("no block here").is_none());
    }

    #[test]
    fn a_list_of_records_renders_as_a_bounded_table_and_anything_else_as_json() {
        let answer = json!({"result": [{"number": "INC1", "close_notes": "Rebooted the | switch"}, {"number": "INC2", "close_notes": "x".repeat(300)}]});
        let table = render("Resolved", &answer, 1);
        assert!(table.contains("## Resolved"));
        assert!(table.contains("2 record(s), the first 1 shown"));
        // Columns come out sorted, whatever order the rows' keys declare.
        assert!(table.contains("| close_notes | number |"), "{table}");
        assert!(table.contains("Rebooted the \\| switch"));
        assert!(!table.contains("INC2"));
        let other = render("Count", &json!({"result": {"stats": {"count": "12"}}}), 10);
        assert!(other.contains("```json") && other.contains("\"count\": \"12\""));
    }
}
