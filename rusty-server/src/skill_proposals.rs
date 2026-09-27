//! Skills from episodes (SI3): a method that worked — the same tool
//! sequence, named by the post-run review — recurring across an agent's
//! verified runs is proposed as a skill: registered under the review's
//! name and filed as a version of the agent that follows it, which the
//! candidate gate judges like any proposal. A person applies it, or the
//! promotion policy does once the gate and the verifier stand.

use std::sync::Arc;

use chrono::Utc;
use rusty_agent_runtime::skill::{SkillPackage, SkillSource};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::TenantContext;
use crate::routes::AppState;

/// How many verified runs the same method must work in before it is
/// proposed as a skill.
pub const RECURRENCE: usize = 3;
/// The author skills proposed this way carry.
pub const AUTHOR: &str = "the post-run review";

/// A method the review named: the tools a turn called, in order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Procedure {
    /// kebab-case, as the review named it.
    pub name: String,
    /// When to use it, one sentence.
    pub when: String,
}

/// One method's tally for one agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProcedureTally {
    /// The tools the method uses, sorted and joined by ` + ` — the
    /// method's identity. The order varies run to run; the tools do not.
    pub key: String,
    /// The order the latest run called them in: the skill's steps.
    pub tools: Vec<String>,
    pub name: String,
    pub when: String,
    pub runs: Vec<String>,
    /// The skill it became, once proposed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
}

fn namespace(tenant: &str) -> String {
    format!("procedures:{tenant}")
}

pub(crate) async fn tallies_of(state: &AppState, tenant: &str, agent: &str) -> Vec<ProcedureTally> {
    state
        .server_store
        .kv_get(&namespace(tenant), agent)
        .await
        .ok()
        .flatten()
        .and_then(|item| serde_json::from_value(item.value).ok())
        .unwrap_or_default()
}

/// The tools a turn called, in order, a repeat of the previous call
/// folded: the method's shape.
pub fn tools_used(messages: &[rusty_agent_runtime::llm::ChatMessage]) -> Vec<String> {
    let start = messages
        .iter()
        .rposition(|m| m.role == rusty_agent_runtime::llm::Role::User)
        .unwrap_or(0);
    let mut out: Vec<String> = Vec::new();
    for m in &messages[start..] {
        for call in &m.tool_calls {
            if out.last() != Some(&call.name) {
                out.push(call.name.clone());
            }
        }
    }
    out
}

/// The method's identity: its distinct tools, sorted.
pub fn method_key(tools: &[String]) -> String {
    let mut set: Vec<&str> = tools.iter().map(String::as_str).collect();
    set.sort_unstable();
    set.dedup();
    set.join(" + ")
}

/// A skill name from the review's: kebab-case, letters, digits and dashes.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_matches('-').chars().take(48).collect()
}

/// A verified run's method, counted; at [`RECURRENCE`] the skill is
/// registered and proposed on the agent. Returns what happened, for the
/// review's report.
pub(crate) async fn record(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    agent: &str,
    run_id: &str,
    tools: Vec<String>,
    procedure: &Procedure,
) -> Value {
    if tools.len() < 2 || agent.is_empty() {
        return json!({"counted": false, "why": "a method is at least two tools"});
    }
    let key = method_key(&tools);
    let mut all = fold(tallies_of(state, tenant.tenant(), agent).await);
    // The same tools are the same method, whatever order this run called
    // them in; a tally kept under an older key folds into it.
    let at = match all.iter().position(|t| t.key == key) {
        Some(at) => at,
        None => {
            all.push(ProcedureTally {
                key: key.clone(),
                tools: tools.clone(),
                name: procedure.name.clone(),
                when: procedure.when.clone(),
                runs: Vec::new(),
                proposed: None,
                version_id: None,
            });
            all.len() - 1
        }
    };
    all[at].key = key.clone();
    if !all[at].runs.iter().any(|r| r == run_id) {
        all[at].runs.push(run_id.to_owned());
    }
    // The latest order wins (the review sees the newest turn), but the
    // first name the method was given stands: a review that calls the same
    // tools something else next time is naming the same method, and the
    // skill proposed at the third run should carry the name the tally kept.
    all[at].tools = tools.clone();
    if all[at].name.trim().is_empty() {
        all[at].name = procedure.name.clone();
    }
    if all[at].when.trim().is_empty() {
        all[at].when = procedure.when.clone();
    }
    let count = all[at].runs.len();
    let mut outcome = json!({"counted": true, "method": key, "name": all[at].name, "runs": count, "proposes_at": RECURRENCE});
    if slug(&procedure.name) != slug(&all[at].name) {
        outcome["named_now"] = json!(procedure.name);
    }
    if count >= RECURRENCE && all[at].proposed.is_none() {
        match propose(state, tenant, agent, &all[at]).await {
            Ok((skill, version_id)) => {
                all[at].proposed = Some(skill.clone());
                all[at].version_id = Some(version_id.clone());
                outcome["proposed"] = json!({"skill": skill, "version_id": version_id});
            }
            Err(why) => {
                outcome["not_proposed"] = json!(why);
            }
        }
    }
    all.truncate(50);
    if let Err(error) = state
        .server_store
        .kv_put(
            &namespace(tenant.tenant()),
            agent,
            serde_json::to_value(&all).unwrap_or(Value::Null),
        )
        .await
    {
        tracing::warn!(%error, agent, "procedure tally not kept");
    }
    outcome
}

/// Tallies by method: two kept under different keys for the same tools
/// (older records keyed by sequence) become one, their runs together.
fn fold(all: Vec<ProcedureTally>) -> Vec<ProcedureTally> {
    let mut out: Vec<ProcedureTally> = Vec::new();
    for mut t in all {
        t.key = method_key(&t.tools);
        match out.iter_mut().find(|o| o.key == t.key) {
            Some(o) => {
                for r in t.runs {
                    if !o.runs.contains(&r) {
                        o.runs.push(r);
                    }
                }
                if o.proposed.is_none() {
                    o.proposed = t.proposed;
                    o.version_id = t.version_id;
                }
            }
            None => out.push(t),
        }
    }
    out
}

/// Register the skill and file the version that follows it.
async fn propose(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    agent: &str,
    tally: &ProcedureTally,
) -> Result<(String, String), String> {
    let name = slug(&tally.name);
    if name.len() < 3 {
        return Err("the review gave the method no usable name".to_owned());
    }
    let scoped = crate::auth::scope_id(tenant.tenant(), agent);
    let record = match state.server_store.get_assistant(&scoped).await {
        Ok(Some(record)) => record,
        _ => match state.server_store.get_assistant(agent).await {
            Ok(Some(record)) => record,
            _ => return Err(format!("agent `{agent}` not found")),
        },
    };
    let following: Vec<String> = record
        .config
        .pointer("/studio_intent/skills")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if following.iter().any(|s| s == &name) {
        return Err(format!("the agent already follows `{name}`"));
    }
    // The skill: registered once; a name already on the plane is used as it is.
    if state.skills.resolve(tenant.tenant(), &name).await.is_none() {
        let steps: Vec<String> = tally
            .tools
            .iter()
            .enumerate()
            .map(|(i, t)| format!("{}. Call `{t}`.", i + 1))
            .collect();
        let skill_md = format!(
            "---\nname: {name}\ndescription: {}\nallowed-tools: {}\n---\n\n# {}\n\n## When to use\n{}\n\n## Method\n{}\n\n## Done when\nThe person has what the method produces, said in the reply with what each call returned.\n\n_Proposed by the post-run review: this method worked in {} verified runs of {}._\n",
            tally.when.replace('\n', " "),
            tally.tools.join(", "),
            tally.name.trim(),
            tally.when.trim(),
            steps.join("\n"),
            tally.runs.len(),
            record.name,
        );
        let package = SkillPackage::from_markdown(&skill_md).map_err(|e| e.to_string())?;
        state
            .skills
            .register(
                tenant.tenant(),
                package,
                SkillSource::Registry {
                    name: "review".to_owned(),
                },
                AUTHOR.to_owned(),
            )
            .await
            .map_err(|e| e.to_string())?;
    }
    // The version that follows it, proposed like the Coach's: the candidate
    // gate judges it, a person or the policy applies it.
    let mut config = record.config.clone();
    if config.pointer("/studio_intent").is_none() {
        config["studio_intent"] = json!({});
    }
    let mut skills = following;
    skills.push(name.clone());
    config["studio_intent"]["skills"] = json!(skills);
    let mut metadata = record.metadata.clone();
    metadata["proposed_by"] = json!({"principal_id": "review", "name": AUTHOR, "kind": "service"});
    metadata["why"] = json!(format!(
        "The same method worked in {} verified runs — {} — so it is kept as the skill `{name}`: {}",
        tally.runs.len(),
        tally.key,
        tally.when.trim()
    ));
    metadata["review_skill"] = json!(name);
    let active = record.active_version_id().to_owned();
    let version = crate::assistants::AssistantVersionRecord::new(
        Some(active.clone()),
        record.name.clone(),
        record.graph.clone(),
        config,
        metadata,
        Utc::now(),
    );
    let version_id = version.version_id.clone();
    state
        .server_store
        .create_assistant_version(&record.assistant_id, &active, &version)
        .await
        .map_err(|e| e.to_string())?;
    if let Ok(Some(filed)) = state.server_store.get_assistant(&record.assistant_id).await {
        let _ = crate::promotion::gate_candidate(
            state,
            tenant,
            &filed,
            &version_id,
            json!({"proposed_by": "review"}),
        )
        .await;
    }
    tracing::info!(agent = %record.name, skill = %name, version = %version_id, runs = tally.runs.len(), "a recurring method proposed as a skill");
    Ok((name, version_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_agent_runtime::llm::{ChatMessage, ToolCall};

    #[test]
    fn the_method_is_the_turns_tool_sequence_with_repeats_folded() {
        let messages = vec![
            ChatMessage::user("earlier"),
            ChatMessage::assistant_tool_calls(vec![ToolCall::new("a", "memory.recall", json!({}))]),
            ChatMessage::user("how many open incidents?"),
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "b",
                "servicenow.aggregate",
                json!({}),
            )]),
            ChatMessage::assistant_tool_calls(vec![
                ToolCall::new("c", "servicenow.list-records", json!({})),
                ToolCall::new("d", "servicenow.list-records", json!({})),
            ]),
            ChatMessage::assistant("2 open."),
        ];
        assert_eq!(
            tools_used(&messages),
            vec!["servicenow.aggregate", "servicenow.list-records"]
        );
        assert_eq!(
            method_key(&[
                "servicenow.list-records".to_owned(),
                "servicenow.aggregate".to_owned(),
                "servicenow.list-records".to_owned()
            ]),
            "servicenow.aggregate + servicenow.list-records",
            "the order and repeats do not change the method"
        );
        assert_eq!(slug("Count Open Incidents!"), "count-open-incidents");
        assert_eq!(slug("  count / list  "), "count-list");
    }
}
