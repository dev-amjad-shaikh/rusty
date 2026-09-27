//! The post-run review (MEM M12, first slice): a verified run teaches the
//! agent without a person. After the verdict, one bounded model call reads
//! the turn and answers three narrow questions — what the person revealed
//! about themselves or their situation, what they expect or prefer, and
//! what could not be answered from any tool — and the platform writes the
//! answers where they belong: a person's facts and preferences to that
//! person's memory, the agent's facts to the agent's, unanswered questions
//! to the gap ledger. Caps: one review per run (a marker file), a bounded
//! transcript, no review on a rehearsal or on an unverified run.

use std::sync::Arc;

use chrono::Utc;
use rusty_agent_runtime::llm::{ChatMessage, Role};
use serde_json::{json, Value};

use crate::auth::TenantContext;
use crate::routes::AppState;

/// The distiller name the review writes under; the prompt says "kept by
/// the post-run review" for it.
pub const REVIEW_AUTHOR: &str = "review";
/// How much of the turn the reviewer reads.
const TRANSCRIPT_CHARS: usize = 6_000;
const REVIEWS_DIR: &str = "reviews";

/// What the run hands the review when its verdict lands.
#[derive(Debug, Clone)]
pub(crate) struct ReviewInput {
    pub run_id: String,
    pub verdict: String,
    pub messages: Vec<ChatMessage>,
    pub assistant_id: Option<String>,
    pub person_id: Option<String>,
    pub rehearsal: bool,
}

/// Whether a run is reviewed at all: a verdict the verifier reached, on
/// live systems, for an agent.
pub(crate) fn eligible(verdict: &str, rehearsal: bool, assistant_id: Option<&str>) -> bool {
    !rehearsal && assistant_id.is_some() && matches!(verdict, "verified" | "failed")
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub(crate) struct Review {
    /// What the person revealed about themselves or their situation.
    #[serde(default)]
    pub person_facts: Vec<String>,
    /// What the person expects or prefers.
    #[serde(default)]
    pub preferences: Vec<String>,
    /// What the agent learned about its work, for every later run.
    #[serde(default)]
    pub work_facts: Vec<String>,
    /// Questions no tool could answer.
    #[serde(default)]
    pub unanswered: Vec<String>,
    /// The method the turn's tool calls formed, when they formed one that
    /// worked: named, with when to use it. Counted across verified runs;
    /// recurring, it is proposed as a skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub procedure: Option<crate::skill_proposals::Procedure>,
}

/// The reviewer's answer as JSON, whatever wrapping it came in.
pub(crate) fn parse_review(text: &str) -> Review {
    let trimmed = text.trim();
    let candidates = [
        trimmed.to_owned(),
        trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim()
            .to_owned(),
    ];
    for c in candidates {
        if let Ok(review) = serde_json::from_str::<Review>(&c) {
            return clean(review);
        }
        if let (Some(a), Some(b)) = (c.find('{'), c.rfind('}')) {
            if let Ok(review) = serde_json::from_str::<Review>(&c[a..=b]) {
                return clean(review);
            }
        }
    }
    Review::default()
}

fn clean(mut review: Review) -> Review {
    let tidy = |v: &mut Vec<String>| {
        v.retain(|s| {
            let t = s.trim();
            !t.is_empty() && !t.eq_ignore_ascii_case("none") && t.len() <= 400
        });
        v.truncate(5);
    };
    tidy(&mut review.person_facts);
    tidy(&mut review.preferences);
    tidy(&mut review.work_facts);
    tidy(&mut review.unanswered);
    if review.procedure.as_ref().is_some_and(|p| {
        p.name.trim().len() < 3
            || p.when.trim().is_empty()
            || p.name.len() > 80
            || p.when.len() > 300
    }) {
        review.procedure = None;
    }
    review
}

/// The turn as the reviewer reads it: the last user message and what
/// followed, roles in words, tool results cut short.
fn transcript(messages: &[ChatMessage]) -> String {
    let start = messages
        .iter()
        .rposition(|m| m.role == Role::User)
        .unwrap_or(0);
    let mut out = String::new();
    for m in &messages[start..] {
        let content = m.content.as_deref().unwrap_or("").trim();
        let line = match m.role {
            Role::User => format!("PERSON: {content}"),
            Role::Assistant if !m.tool_calls.is_empty() => format!(
                "AGENT called {}",
                m.tool_calls
                    .iter()
                    .map(|c| c.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Role::Assistant => format!("AGENT: {content}"),
            Role::Tool => format!(
                "TOOL RESULT: {}",
                content.chars().take(600).collect::<String>()
            ),
            Role::System => continue,
        };
        out.push_str(&line);
        out.push('\n');
        if out.len() > TRANSCRIPT_CHARS {
            out.truncate(TRANSCRIPT_CHARS);
            out.push_str("\n[cut]");
            break;
        }
    }
    out
}

const PROMPT: &str = "You review one finished conversation turn between a person and an agent, so the agent remembers what matters next time. Answer with JSON only, no prose, exactly these keys: \
{\"person_facts\": [...], \"preferences\": [...], \"work_facts\": [...], \"unanswered\": [...], \"procedure\": {\"name\": \"...\", \"when\": \"...\"} or null}. \
person_facts: what the person revealed about themselves or their situation — their team, their device, their role, a deadline — one plain sentence each, as they said it, at most five. \
preferences: what they expect or prefer of the agent — brevity, a format, a channel — one sentence each. \
work_facts: what the agent learned about its work that holds for every later run, whoever asks — a system's quirk, where a thing lives — one sentence each; never a person's data here. \
unanswered: a question the person asked that no tool could answer, one line each. \
procedure: when the agent's tool calls (the AGENT called lines) formed a method that worked for this kind of request — a way of doing it worth repeating — a kebab-case name for the method and one sentence on when to use it; null when the turn called no tools, or they did not add up to a method. \
Never include a password, a key or a token. Empty lists are fine; only what the turn actually shows.";

/// Run the review for one run and write what it found. Best-effort: a
/// failure is logged, never surfaced — the run is already over.
pub(crate) async fn review(state: Arc<AppState>, input: ReviewInput) {
    if !eligible(
        &input.verdict,
        input.rehearsal,
        input.assistant_id.as_deref(),
    ) {
        return;
    }
    let marker = state
        .config
        .store_path
        .join(REVIEWS_DIR)
        .join(format!("{}.json", input.run_id));
    if marker.exists() {
        return;
    }
    let Some(judge) = state.run_deps.verifier.clone() else {
        return;
    };
    let text = transcript(&input.messages);
    if text.trim().is_empty() {
        return;
    }
    let asked = vec![
        ChatMessage::system(PROMPT.to_owned()),
        ChatMessage::user(text),
    ];
    let answer = match tokio::time::timeout(
        std::time::Duration::from_secs(60),
        judge.0.chat(&asked, &[]),
    )
    .await
    {
        Ok(Ok(response)) => response.message.content.unwrap_or_default(),
        Ok(Err(error)) => {
            tracing::warn!(run = %input.run_id, %error, "post-run review: the reviewer failed");
            return;
        }
        Err(_) => {
            tracing::warn!(run = %input.run_id, "post-run review: the reviewer timed out");
            return;
        }
    };
    let review = parse_review(&answer);
    let tenant = TenantContext::new(crate::auth::DEFAULT_TENANT.to_owned(), Vec::new());
    let now = Utc::now();
    let mut written: Vec<Value> = Vec::new();
    let agent = input.assistant_id.clone().unwrap_or_default();
    // A method that worked, counted; recurring, proposed as a skill.
    let procedure = match (&review.procedure, input.verdict.as_str()) {
        (Some(p), "verified") => Some(
            crate::skill_proposals::record(
                &state,
                &tenant,
                &agent,
                &input.run_id,
                crate::skill_proposals::tools_used(&input.messages),
                p,
            )
            .await,
        ),
        _ => None,
    };
    // A person's facts and preferences go to that person; the work to the agent.
    let mut notes: Vec<(
        rusty_agent_runtime::memory::ScopeAddress,
        rusty_agent_runtime::memory::MemoryKind,
        &String,
    )> = Vec::new();
    if let Some(person) = input.person_id.as_deref() {
        for f in &review.person_facts {
            notes.push((
                rusty_agent_runtime::memory::ScopeAddress::new(
                    rusty_agent_runtime::memory::MemoryScope::User,
                    person,
                ),
                rusty_agent_runtime::memory::MemoryKind::Fact,
                f,
            ));
        }
        for p in &review.preferences {
            notes.push((
                rusty_agent_runtime::memory::ScopeAddress::new(
                    rusty_agent_runtime::memory::MemoryScope::User,
                    person,
                ),
                rusty_agent_runtime::memory::MemoryKind::Preference,
                p,
            ));
        }
    }
    for w in &review.work_facts {
        notes.push((
            rusty_agent_runtime::memory::ScopeAddress::new(
                rusty_agent_runtime::memory::MemoryScope::Agent,
                &agent,
            ),
            rusty_agent_runtime::memory::MemoryKind::Fact,
            w,
        ));
    }
    for (scope, kind, text) in notes {
        if rusty_agent_runtime::memory::looks_like_secret(text).is_some() {
            continue;
        }
        let key = format!(
            "review:{}",
            &rusty_agent_runtime::record::sha256_hex(text.to_lowercase().as_bytes())[..12]
        );
        let record = rusty_agent_runtime::memory::MemoryRecord::new(
            kind,
            scope.clone(),
            rusty_agent_runtime::memory::MemoryProvenance {
                author: rusty_agent_runtime::memory::ProvenanceAuthor::Distiller {
                    name: REVIEW_AUTHOR.to_owned(),
                },
                evidence: rusty_agent_runtime::memory::MemoryEvidence {
                    run_id: Some(input.run_id.clone()),
                    ..Default::default()
                },
                written_at: now,
            },
            0.7,
            rusty_agent_runtime::memory::ValidityWindow {
                valid_from: now,
                valid_until: None,
            },
            now,
            json!({ "text": text, "from_run": input.run_id }),
        );
        let Ok(record) = record else { continue };
        let mut record = record.with_key(key).with_priority(6).with_tags(["review"]);
        // What the review learned about a person is proposed to them, as
        // the agent's own notes about a person are (`memory.remember`).
        if scope.scope == rusty_agent_runtime::memory::MemoryScope::User {
            record = record.with_candidacy(rusty_agent_runtime::memory::Candidacy::Pending);
        }
        match state.server_store.put_memory(tenant.tenant(), &record, &json!({ "text": text, "from_run": input.run_id })).await {
            Ok(_) => written.push(json!({ "scope": scope.as_address(), "kind": kind, "text": text, "memory_id": record.memory_id })),
            Err(error) => tracing::warn!(run = %input.run_id, %error, "post-run review: note not written"),
        }
    }
    let mut gaps: Vec<String> = Vec::new();
    if !review.unanswered.is_empty() {
        let run_id = input.run_id.clone();
        let actor = format!("review:{agent}");
        let filed = crate::routes::mutate_gap_ledger(&state, &tenant, |ledger| {
            let mut ids = Vec::new();
            for q in &review.unanswered {
                let subject = match rusty_agent_runtime::gaps::GapSubject::question_shape(q) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                if let Ok(id) = ledger.file_gap(
                    subject,
                    format!("No tool could answer this in a verified run: {q}"),
                    vec![rusty_agent_runtime::gaps::Citation {
                        kind: rusty_agent_runtime::gaps::CitationKind::RunReceipt,
                        id: run_id.clone(),
                        note: Some("found by the post-run review".to_owned()),
                    }],
                    rusty_agent_runtime::gaps::GapOrigin::AgentDeclared,
                    rusty_agent_runtime::gaps::ClosureCriteria::FailureRateBelow {
                        threshold_millis: 50,
                    },
                    1,
                    0,
                    &actor,
                    now,
                ) {
                    ids.push(id);
                }
            }
            Ok(ids)
        })
        .await;
        if let Ok(ids) = filed {
            gaps = ids;
        }
    }
    let report = json!({
        "run_id": input.run_id,
        "stamp": now,
        "verdict": input.verdict,
        "review": review,
        "written": written,
        "gaps_filed": gaps,
        "procedure": procedure,
    });
    if let Err(error) = crate::connectors::persist_json(
        &state.config.store_path.join(REVIEWS_DIR),
        &input.run_id,
        &report,
    )
    .await
    {
        tracing::warn!(run = %input.run_id, %error, "post-run review: report not kept");
    }
    tracing::info!(run = %input.run_id, notes = report["written"].as_array().map_or(0, Vec::len), gaps = report["gaps_filed"].as_array().map_or(0, Vec::len), "post-run review done");
}

/// The review kept for a run, when one ran.
pub(crate) fn load(state: &AppState, run_id: &str) -> Option<Value> {
    let bytes = std::fs::read(
        state
            .config
            .store_path
            .join(REVIEWS_DIR)
            .join(format!("{run_id}.json")),
    )
    .ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_review_is_only_run_on_a_live_judged_run_of_an_agent() {
        assert!(eligible("verified", false, Some("a")));
        assert!(eligible("failed", false, Some("a")));
        assert!(!eligible("unverified", false, Some("a")));
        assert!(
            !eligible("verified", true, Some("a")),
            "a rehearsal teaches nothing real"
        );
        assert!(!eligible("verified", false, None));
    }

    #[test]
    fn the_reviewers_answer_parses_from_plain_or_fenced_json_and_none_is_empty() {
        let plain = r#"{"person_facts":["Amjad is on the Berlin team."],"preferences":["Short answers."],"work_facts":[],"unanswered":["none"]}"#;
        let r = parse_review(plain);
        assert_eq!(r.person_facts, vec!["Amjad is on the Berlin team."]);
        assert_eq!(r.preferences, vec!["Short answers."]);
        assert!(r.unanswered.is_empty(), "none is empty");
        let fenced = "Here you go:\n```json\n{\"work_facts\":[\"The catalog lists laptops under Standard Laptop.\"]}\n```";
        assert_eq!(
            parse_review(fenced).work_facts,
            vec!["The catalog lists laptops under Standard Laptop."]
        );
        assert_eq!(parse_review("not json at all"), Review::default());
    }

    #[test]
    fn the_transcript_starts_at_the_last_person_message_and_cuts_long_results() {
        let messages = vec![
            ChatMessage::user("earlier"),
            ChatMessage::assistant("old answer"),
            ChatMessage::user("I'm on the Berlin team"),
            ChatMessage::tool_result("c1", "x".repeat(2_000)),
            ChatMessage::assistant("Noted."),
        ];
        let t = transcript(&messages);
        assert!(t.starts_with("PERSON: I'm on the Berlin team"));
        assert!(!t.contains("old answer"));
        assert!(t.contains("TOOL RESULT: ") && t.len() < 1_000);
    }
}
