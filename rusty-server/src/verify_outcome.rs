//! Outcome verification: a run is not done because the model stopped
//! calling tools. When a run completes, the server asks a judge model
//! whether the requested outcome is observably achieved — by the evidence
//! (the tool calls the turn made and what they returned), not by the
//! reply's claims — and records the verdict beside the run.
//!
//! The verdict is a judgment *about* the run, not part of it: it lives in
//! its own plane (`{store_path}/verifications/{run_id}.json`), is served on
//! the run's terminal payload and `GET /runs`, and never enters the journal,
//! so an exact replay of a verified run stays byte-for-byte. The deterministic
//! half — which tools ran, which wrote, which were refused, which went
//! unanswered — is computed from the thread and handed to the judge and to
//! the reader alike; the judge only says whether the reply is warranted by
//! it. Three verdicts, all honest: `verified`, `failed`, and `unverified`
//! when the judge could not tell or could not be read.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use rusty_agent_runtime::llm::{ChatMessage, ChatModel, Role};
use rusty_agent_runtime::react::{REPEATED_CALL_NOTICE, UNKNOWN_OUTCOME_NOTICE, UNRECORDED_READ_NOTICE};
use rusty_agent_runtime::tool::ToolCapability;

/// The judge's standing instructions. Pinned here so a wording change is a
/// reviewable diff, like the compaction prompt.
pub const VERIFIER_PROMPT: &str = "You are the verification step after an agent's turn. You \
are given the agent's charter, the person's request, the tool calls the agent made in this \
turn with what each returned, and the agent's final reply. Decide whether the requested \
outcome is observably achieved BY THE EVIDENCE — what the tools returned — and not merely \
claimed by the reply. Answer with exactly one JSON object and nothing else: \
{\"verdict\": \"verified\" | \"failed\" | \"unverified\", \"reason\": \"<one sentence naming the \
evidence or the gap>\"}. verified: the evidence shows the outcome, or the request was a \
question the reply answers correctly from the evidence or the charter. failed: the reply \
claims, implies or assumes an outcome the evidence contradicts or does not contain, or the \
reply does not address the request, or a call the turn needed was refused or lost. \
unverified: the evidence cannot decide it — say what would. A fact the reply states as looked \
up is verified only if a tool returned it in this turn; when every call errored or nothing was \
called, facts presented as looked up are the model's own and the verdict is failed, unless the \
reply says plainly that it could not look them up. Notes listed under REMEMBERED were read from \
memory at the start of the turn: a fact the reply gives as remembered that such a note states is \
the note's author's claim and counts as shown, not invented. A result marked as clipped cannot prove a \
fact absent: when the fact would sit in the part not shown, the verdict is unverified, never \
failed for that reason. When the charter says the agent \
proposes and a person decides — a specification, a plan, a draft for approval — a complete, \
honest proposal that names what it did and did not do IS the outcome; judge it as such. A \
charter may state a Goal: the standing objective the agent's runs are measured on. Read the \
reply against it as well as the request: when the request is met but the goal plainly is \
not — what the goal names is missing from the reply and the evidence — the verdict is \
failed and the reason names the goal's missing part; when one turn cannot decide the goal, \
do not hold it against the reply.";

/// The word the model gets, once, when the judge did not verify the turn:
/// the verdict's reason, and what to do about it. Pinned like the prompt.
/// It rides the thread as a system message, so the studio shows it for
/// what it is and the second verdict judges the whole turn.
pub const REPAIR_NOTICE_PREFIX: &str = "The verification step ";

/// The notes the agent read from memory in the run, handed to the judge as
/// one system message in the turn: what was remembered is evidence of a
/// claim someone wrote, with who wrote it and when — not of what a system
/// holds now. Its identifiers count as shown; a reply that gives a fact as
/// remembered is judged against the note, not sent back for inventing it.
pub const REMEMBERED_PREFIX: &str = "REMEMBERED — notes read from memory at the start of this turn, each someone's claim with who wrote it and when:";
const REMEMBERED_NOTES: usize = 40;

/// Every note a run read from memory — each journaled memory read's
/// assembly, each note once, in the order first read.
pub fn remembered_in(journal: &rusty_agent_runtime::journal::Journal) -> Vec<rusty_agent_runtime::memory::MemoryRecord> {
    use rusty_agent_runtime::record::{PayloadRef, RunEventKind};
    let snapshot = journal.snapshot();
    let mut seen = std::collections::HashSet::new();
    let mut notes = Vec::new();
    for event in snapshot.events.iter().filter(|e| e.kind == RunEventKind::MemoryRead) {
        let output = match &event.output {
            Some(PayloadRef::Inline(v)) => Some(v.clone()),
            Some(PayloadRef::Artifact(a)) => snapshot.artifacts.get(&a.sha256).cloned(),
            None => None,
        };
        let Some(assembly) = output.and_then(|v| serde_json::from_value::<rusty_agent_runtime::memory::MemoryAssembly>(v).ok()) else { continue };
        for record in assembly.records {
            if seen.insert(record.memory_id.clone()) {
                notes.push(record);
            }
        }
    }
    notes
}

/// The REMEMBERED message for the judge: one line per note in the order
/// read, each in full — the identifier check reads it whole; the judge
/// reads it as it reads a long result, excerpted around what the reply
/// cites — bounded by count; none when nothing was read. `names` gives an
/// authoring agent its name, so the judge reads *Incident Q&A*, not an id.
pub fn remembered_message(records: &[rusty_agent_runtime::memory::MemoryRecord], names: &std::collections::HashMap<String, String>) -> Option<ChatMessage> {
    use rusty_agent_runtime::memory::ProvenanceAuthor;
    use rusty_agent_runtime::record::PayloadRef;
    if records.is_empty() {
        return None;
    }
    let mut lines = vec![REMEMBERED_PREFIX.to_owned()];
    for record in records.iter().take(REMEMBERED_NOTES) {
        let text = match &record.content {
            PayloadRef::Inline(v) => v.get("text").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| v.to_string()),
            PayloadRef::Artifact(a) => format!("<note held as artifact {}>", &a.sha256[..12.min(a.sha256.len())]),
        };
        let who = match &record.provenance.author {
            ProvenanceAuthor::Agent { agent_id } => match names.get(agent_id) {
                Some(name) => format!("{name} (agent {agent_id})"),
                None => format!("agent {agent_id}"),
            },
            ProvenanceAuthor::Human { human_id } => format!("person {human_id}"),
            ProvenanceAuthor::Distiller { name } => name.clone(),
            ProvenanceAuthor::System => "the platform".to_owned(),
        };
        lines.push(format!("- {text} (written by {who}, {})", record.provenance.written_at.format("%Y-%m-%d %H:%M UTC")));
    }
    if records.len() > REMEMBERED_NOTES {
        lines.push(format!("- and {} more notes not listed", records.len() - REMEMBERED_NOTES));
    }
    Some(ChatMessage::system(lines.join("\n")))
}

/// Whether the turn carries a REMEMBERED message — memory was read.
fn remembered_in_turn(messages: &[ChatMessage]) -> bool {
    last_turn(messages).iter().any(|m| m.role == Role::System && m.content.as_deref().is_some_and(|c| c.starts_with(REMEMBERED_PREFIX)))
}

/// The repair notice for one verdict. `failed` is told so; `unverified` —
/// sent back only when nothing wrote and the agent has tools that could
/// have — is told what could not be confirmed.
pub fn repair_notice(verdict: &str, reason: &str) -> String {
    let reason = reason.trim().trim_end_matches('.');
    match verdict {
        "failed" => format!(
            "{REPAIR_NOTICE_PREFIX}judged this turn's outcome not achieved: {reason}. Finish the job \
now with your tools — every action you report must be a call that returned in this \
conversation — or answer plainly with what was and was not done."
        ),
        _ => format!(
            "{REPAIR_NOTICE_PREFIX}could not confirm this turn's outcome: {reason}. If anything you \
reported doing was not done by a call that returned in this conversation, do it now — or \
answer plainly with what was and was not done."
        ),
    }
}

/// Whether a verdict sends the model back for one repair turn: a failed
/// verdict; or one the judge chose (`read`) but could not settle while
/// nothing wrote and the agent has tools that write — the shape of a
/// claimed-but-absent write a weak judge lets through. Never when a person
/// declined a call this turn: the agent was told to stop, and did.
pub fn sends_back(verdict: &Verdict, could_write: bool) -> bool {
    if verdict.evidence.declined > 0 {
        return false;
    }
    let judge_read = verdict.judge.get("read").and_then(Value::as_bool).unwrap_or(false);
    verdict.verdict == "failed" || (verdict.verdict == "unverified" && judge_read && verdict.evidence.writes == 0 && could_write)
}

/// How long the judge may take. A verdict is worth one call, not a wait.
pub const JUDGE_TIMEOUT: Duration = Duration::from_secs(45);

/// The most characters of any one tool result shown to the judge. The
/// judge must see what the agent saw: a typical API object (a GitHub
/// repository, a ServiceNow record) is 4–8 KB, and a fact the reply quotes
/// often sits at the end of it. A result past this is clipped with a note
/// that says so, and the prompt forbids treating what was clipped as absent.
const RESULT_EXCERPT: usize = 12_000;

/// A judge model a deployment hands the server. Debug prints no model.
#[derive(Clone)]
pub struct Verifier(pub Arc<dyn ChatModel>);

impl std::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Verifier")
    }
}

/// One tool call of the turn, as the reader and the judge see it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceCall {
    pub tool: String,
    /// The declared effect, from the graph's catalog; `unknown` when the
    /// catalog does not name the tool.
    pub effect: String,
    /// `ok`, `error`, `refused` (a repeated call), `declined` (a person
    /// declined it at the gate), `lost` (no result was recorded), `unknown`
    /// (a write whose outcome was never recorded), or `unanswered`.
    pub outcome: String,
}

/// The deterministic half of a verdict: what the turn did.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Evidence {
    pub calls: Vec<EvidenceCall>,
    /// Notes read from memory in the run and shown to the agent — evidence
    /// of what was remembered, not of what a system holds now.
    #[serde(default)]
    pub remembered: usize,
    pub writes: usize,
    pub refused: usize,
    /// Calls a person declined at the approval gate: the agent was told
    /// not to retry or work around them, so a reply that says what it
    /// would have done and stops is the outcome.
    #[serde(default)]
    pub declined: usize,
    pub unanswered: usize,
    /// Calls that returned an error — no fact came back from them.
    pub errors: usize,
    /// Calls that returned a result the reply could draw on.
    pub succeeded: usize,
}

/// The verdict, as stored and served.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    /// `verified`, `failed`, or `unverified`.
    pub verdict: String,
    pub reason: String,
    pub evidence: Evidence,
    /// The judge's identity and raw answer, for audit.
    pub judge: Value,
    pub at: chrono::DateTime<chrono::Utc>,
    /// When this verdict came after one repair turn: the first verdict, the
    /// one that sent the model back (`{"verdict", "reason"}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repaired: Option<Value>,
    /// The graph the run executed, for the catalog a re-judging reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph: Option<String>,
}

/// How many of a thread's newest messages a verdict keeps.
pub const TRANSCRIPT_MESSAGES: usize = 60;

/// The messages a verdict keeps: the newest, bounded.
pub fn kept_transcript(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let start = messages.len().saturating_sub(TRANSCRIPT_MESSAGES);
    messages[start..].to_vec()
}

/// The messages of the last turn: from the last user message on.
fn last_turn(messages: &[ChatMessage]) -> &[ChatMessage] {
    match messages.iter().rposition(|m| m.role == Role::User) {
        Some(start) => &messages[start..],
        None => messages,
    }
}

/// What the turn did, from the thread and the catalog.
pub fn turn_evidence(messages: &[ChatMessage], catalog: &[ToolCapability]) -> Evidence {
    let turn = last_turn(messages);
    let effect_of = |tool: &str| {
        catalog
            .iter()
            .find(|c| c.name == tool)
            .and_then(|c| serde_json::to_value(c.effect).ok())
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| "unknown".to_owned())
    };
    // A write is any call whose effect changes state outside the run —
    // idempotent ones included: a remembered fact, an upsert, a keyed
    // create took effect once, however safely they could be repeated. The
    // retry rule (`is_freely_repeatable`) is about repeating, not about
    // whether anything happened. A tool the catalog does not name is not
    // counted either way.
    let writes_state = |tool: &str| {
        catalog
            .iter()
            .find(|c| c.name == tool)
            .is_some_and(|c| !matches!(c.effect, rusty_agent_runtime::record::Effect::Pure | rusty_agent_runtime::record::Effect::ReadOnly))
    };
    let mut calls = Vec::new();
    for (index, message) in turn.iter().enumerate() {
        if message.role != Role::Assistant || message.tool_calls.is_empty() {
            continue;
        }
        for call in &message.tool_calls {
            let result = turn[index + 1..]
                .iter()
                .take_while(|m| m.role == Role::Tool)
                .find(|m| m.tool_call_id.as_deref() == Some(call.id.as_str()));
            let outcome = match result.and_then(|m| m.content.as_deref()) {
                None => "unanswered",
                Some(REPEATED_CALL_NOTICE) => "refused",
                // A person declined it at the approval gate: nothing ran.
                Some(text) if text.starts_with(rusty_agent_runtime::react::DENIED_NOTICE) => "declined",
                Some(UNRECORDED_READ_NOTICE) => "lost",
                Some(UNKNOWN_OUTCOME_NOTICE) => "unknown",
                Some(text) if text.starts_with("ERROR") => "error",
                Some(_) => "ok",
            };
            calls.push(EvidenceCall {
                tool: call.name.clone(),
                effect: effect_of(&call.name),
                outcome: outcome.to_owned(),
            });
        }
    }
    let writes = calls
        .iter()
        .filter(|c| c.outcome == "ok" && writes_state(&c.tool))
        .count();
    let refused = calls.iter().filter(|c| c.outcome == "refused").count();
    let declined = calls.iter().filter(|c| c.outcome == "declined").count();
    let unanswered = calls.iter().filter(|c| c.outcome == "unanswered").count();
    let errors = calls.iter().filter(|c| c.outcome == "error").count();
    let succeeded = calls.iter().filter(|c| c.outcome == "ok").count();
    let remembered = last_turn(messages)
        .iter()
        .filter(|m| m.role == Role::System && m.content.as_deref().is_some_and(|c| c.starts_with(REMEMBERED_PREFIX)))
        .map(|m| m.content.as_deref().unwrap_or("").lines().skip(1).filter(|l| l.starts_with("- ")).count())
        .sum();
    Evidence { calls, remembered, writes, refused, declined, unanswered, errors, succeeded }
}

/// Record-like identifiers in a text — `INC0010097`, `P0000001`, `CHG0030001`:
/// one to six capitals followed by four or more digits, standing alone.
fn identifiers(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut found = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let bounded = i == 0 || !chars[i - 1].is_alphanumeric();
        if bounded && chars[i].is_ascii_uppercase() {
            let mut j = i;
            while j < chars.len() && chars[j].is_ascii_uppercase() && j - i < 6 {
                j += 1;
            }
            let letters = j - i;
            let mut k = j;
            while k < chars.len() && chars[k].is_ascii_digit() {
                k += 1;
            }
            let digits = k - j;
            let closed = k == chars.len() || !chars[k].is_alphanumeric();
            if (1..=6).contains(&letters) && digits >= 4 && closed {
                let id: String = chars[i..k].iter().collect();
                if !found.contains(&id) {
                    found.push(id);
                }
                i = k;
                continue;
            }
        }
        i += 1;
    }
    found
}

/// Identifiers the final reply names that nothing showed the model: no tool
/// result in the conversation, no message from the person, not the charter.
fn unshown_identifiers(messages: &[ChatMessage], charter: Option<&str>) -> Vec<String> {
    let reply = last_turn(messages)
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant && m.tool_calls.is_empty())
        .and_then(|m| m.content.as_deref())
        .unwrap_or("");
    let mut shown = charter.unwrap_or("").to_owned();
    for message in messages {
        if matches!(message.role, Role::Tool | Role::User | Role::System) {
            if let Some(content) = &message.content {
                // The verifier's own repair notice quotes the number it
                // refused; quoting it back is not showing it.
                if message.role == Role::System && content.starts_with(REPAIR_NOTICE_PREFIX) {
                    continue;
                }
                shown.push('\n');
                shown.push_str(content);
            }
        }
    }
    identifiers(reply)
        .into_iter()
        .filter(|id| !shown.contains(id.as_str()))
        .filter(|id| !disowned(reply, id))
        .collect()
}

/// Words a sentence uses to disown a number it names: naming a record
/// only to say it was wrong is a retraction, not a claim.
const DISOWNING: [&str; 10] = ["wrong", "incorrect", "not ", "never", "stale", "superseded", "retract", "no tool result", "mistaken", "should not have"];

/// Whether every sentence of the reply that names `id` disowns it.
fn disowned(reply: &str, id: &str) -> bool {
    let sentences: Vec<&str> = reply.split(['.', '!', '?', '\n']).collect();
    let naming: Vec<&str> = sentences.iter().copied().filter(|s| s.contains(id)).collect();
    !naming.is_empty()
        && naming.iter().all(|s| {
            let lower = s.to_lowercase();
            DISOWNING.iter().any(|w| lower.contains(w))
        })
}

/// How much of a long result is shown around an identifier the reply cites
/// from beyond the excerpt: a little before it and, since a record's fields
/// follow its number, enough after it to hold the record.
const WINDOW_BEFORE: usize = 200;
const WINDOW_AFTER: usize = 2_400;

/// A long result as the judge reads it: its head, then — for each identifier
/// the reply names that lies beyond the head — the text around it, so the
/// judge can check what the reply says about that record; then the names
/// of any other identifiers in the part not shown.
fn excerpt_for(text: &str, cited: &[String]) -> String {
    if text.len() <= RESULT_EXCERPT {
        return text.to_owned();
    }
    let mut cut = RESULT_EXCERPT;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let head = &text[..cut];
    let mut windows = String::new();
    for id in cited {
        if head.contains(id.as_str()) {
            continue;
        }
        if let Some(at) = text[cut..].find(id.as_str()).map(|i| i + cut) {
            let mut from = at.saturating_sub(WINDOW_BEFORE);
            let mut to = (at + id.len() + WINDOW_AFTER).min(text.len());
            while !text.is_char_boundary(from) {
                from -= 1;
            }
            while !text.is_char_boundary(to) {
                to += 1;
            }
            windows.push_str(&format!("\n[around {id}, from the part not shown: …{}…]", &text[from..to]));
        }
    }
    let shown = identifiers(head);
    let beyond: Vec<String> = identifiers(&text[cut..]).into_iter().filter(|id| !shown.contains(id) && !cited.contains(id)).collect();
    let tail = if beyond.is_empty() {
        String::new()
    } else {
        format!("; other identifiers in the part not shown: {}", beyond.iter().take(40).cloned().collect::<Vec<_>>().join(", "))
    };
    format!(
        "{head}… [{} more characters not shown — a fact absent above may be in them; that absence is not evidence{tail}]{windows}",
        text.len() - cut
    )
}

#[cfg(test)]
fn excerpt(text: &str) -> String {
    if text.len() <= RESULT_EXCERPT {
        return text.to_owned();
    }
    let mut cut = RESULT_EXCERPT;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    // The judge sees the head of a long result; the identifiers in its tail
    // are named, so a reply that cites a record from the part not shown is
    // not taken for a fabrication.
    let shown = identifiers(&text[..cut]);
    let beyond: Vec<String> = identifiers(&text[cut..]).into_iter().filter(|id| !shown.contains(id)).collect();
    let tail = if beyond.is_empty() {
        String::new()
    } else {
        format!("; identifiers in the part not shown: {}", beyond.iter().take(40).cloned().collect::<Vec<_>>().join(", "))
    };
    format!(
        "{}… [{} more characters not shown — a fact absent above may be in them; that absence is not evidence{tail}]",
        &text[..cut],
        text.len() - cut
    )
}

/// The turn as the judge reads it: request, calls with results, reply.
fn transcript(messages: &[ChatMessage]) -> (String, String, String) {
    let turn = last_turn(messages);
    let request = turn
        .first()
        .filter(|m| m.role == Role::User)
        .and_then(|m| m.content.clone())
        .unwrap_or_default();
    let reply = turn
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant && m.tool_calls.is_empty())
        .and_then(|m| m.content.clone())
        .unwrap_or_default();
    // The records the reply cites decide which parts of a long result the
    // judge must see.
    let cited = identifiers(&reply);
    let mut did = Vec::new();
    for message in turn {
        match message.role {
            Role::Assistant if !message.tool_calls.is_empty() => {
                for call in &message.tool_calls {
                    did.push(format!("CALLED {}({})", call.name, call.arguments));
                }
            }
            Role::Tool => did.push(format!(
                "RESULT: {}",
                excerpt_for(message.content.as_deref().unwrap_or(""), &cited)
            )),
            Role::System if message.content.as_deref().is_some_and(|c| c.starts_with(REMEMBERED_PREFIX)) => {
                did.push(excerpt_for(message.content.as_deref().unwrap_or(""), &cited));
            }
            _ => {}
        }
    }
    let did = if did.is_empty() {
        "(no tool calls this turn)".to_owned()
    } else {
        did.join("\n")
    };
    (request, did, reply)
}

/// The first JSON object in the judge's answer, if any.
fn parse_answer(answer: &str) -> Option<(String, String)> {
    let start = answer.find('{')?;
    let end = answer.rfind('}')?;
    let value: Value = serde_json::from_str(&answer[start..=end]).ok()?;
    let verdict = value.get("verdict")?.as_str()?.trim().to_ascii_lowercase();
    if !matches!(verdict.as_str(), "verified" | "failed" | "unverified") {
        return None;
    }
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    Some((verdict, reason))
}

/// Ask the judge. Never fails: a judge that cannot be reached or read is an
/// `unverified` verdict that says so.
pub async fn verify(
    judge: &dyn ChatModel,
    charter: Option<&str>,
    messages: &[ChatMessage],
    catalog: &[ToolCapability],
) -> Verdict {
    let evidence = turn_evidence(messages, catalog);
    // A record number the reply names that no tool result ever showed is the
    // model's own — a fabricated completion, and the commonest one. That is
    // decided here, not by the judge, which has taken such a reply at its word.
    let invented = unshown_identifiers(messages, charter);
    if let Some(first) = invented.first() {
        return Verdict {
            verdict: "failed".to_owned(),
            reason: format!(
                "the reply names {first}, which no tool result in this conversation contains{}",
                if invented.len() > 1 { format!(" (nor {})", invented[1..].join(", ")) } else { String::new() }
            ),
            evidence,
            judge: json!({ "rule": "unshown_identifier", "identifiers": invented }),
            at: chrono::Utc::now(),
            repaired: None,
            graph: None,
        };
    }
    let (request, did, reply) = transcript(messages);
    // Every identifier the reply names was checked against the full tool
    // results before the judge reads anything; when all are there, the judge
    // is told so, because its excerpts may not show them.
    let named = identifiers(&reply);
    let checked = if named.is_empty() || !unshown_identifiers(messages, charter).is_empty() {
        String::new()
    } else {
        format!(
            " IDENTIFIERS CHECKED: every identifier the reply names ({}) appears in a tool result — checked on the full results, not the excerpts above; do not fail the reply for an identifier you do not see.",
            named.iter().take(20).cloned().collect::<Vec<_>>().join(", ")
        )
    };
    let facts = format!(
        "{} call(s): {} returned a result, {} returned an error, {} write(s) took effect, {} refused as repeats, {} unanswered.{}{checked}",
        evidence.calls.len(),
        evidence.succeeded,
        evidence.errors,
        evidence.writes,
        evidence.refused,
        evidence.unanswered,
        if evidence.declined > 0 {
            " A PERSON DECLINED A CALL at the approval gate and the agent was told not to retry it or work around it. A reply that says what it would have done, that it was declined, and stops IS the outcome — verified. A reply that claims the declined action happened, or that does it another way, is failed. Do not fail it for not doing what was declined."
        } else if catalog.is_empty() {
            " THIS AGENT HAS NO TOOLS: its charter is its only evidence. Verified when the reply follows from the charter's facts and rules; failed when it contradicts them; unverified only when the charter does not settle it. Do not ask for evidence a tool would have given."
        } else if evidence.calls.is_empty() && evidence.remembered > 0 {
            " NO TOOL WAS CALLED, BUT NOTES WERE READ FROM MEMORY (listed under REMEMBERED): a fact the reply gives as remembered — from memory, from a note, from what another agent reported — that a note states is that note's author's claim, not the model's own; the reply is verified when it says so and answers the request from it. A remembered fact presented as looked up now, or as the current state of a system, is unverified — say that a fresh read would decide it. Anything else the reply says a system contains, lacks, did or did not do is the model's own — failed."
        } else if evidence.calls.is_empty() {
            " NO TOOL WAS CALLED: anything the reply says a system contains, lacks, did or did not do is the model's own — failed, unless the reply says plainly that it did not look."
        } else if evidence.succeeded == 0 {
            " NO CALL RETURNED A RESULT: any fact the reply states as looked up did not come from a tool."
        } else if evidence.writes == 0 {
            " NO WRITE TOOK EFFECT: if the reply says something was created, filed, sent, posted, updated or changed, it was not — the verdict is failed."
        } else {
            ""
        }
    );
    let user = format!(
        "CHARTER:\n{}\n\nREQUEST:\n{}\n\nWHAT THE AGENT DID ({facts}):\n{}\n\nFINAL REPLY:\n{}",
        charter.unwrap_or("(none)"),
        request,
        did,
        reply
    );
    let asked = vec![ChatMessage::system(VERIFIER_PROMPT), ChatMessage::user(user)];
    let at = chrono::Utc::now();
    let (verdict, reason, judge_record) =
        match tokio::time::timeout(JUDGE_TIMEOUT, judge.chat(&asked, &[])).await {
            Ok(Ok(response)) => {
                let answer = response.message.content.clone().unwrap_or_default();
                // `read`: the judge's word was a verdict — an unverified it
                // chose, not one minted for an answer nobody could read.
                match parse_answer(&answer) {
                    Some((verdict, reason)) => (verdict, reason, json!({ "model": response.model, "answer": answer, "read": true })),
                    None => (
                        "unverified".to_owned(),
                        "the judge's answer could not be read as a verdict".to_owned(),
                        json!({ "model": response.model, "answer": answer, "read": false }),
                    ),
                }
            }
            Ok(Err(error)) => (
                "unverified".to_owned(),
                format!("the judge could not be asked: {error}"),
                json!({ "error": error.to_string() }),
            ),
            Err(_) => (
                "unverified".to_owned(),
                format!("the judge did not answer within {}s", JUDGE_TIMEOUT.as_secs()),
                json!({ "error": "timeout" }),
            ),
        };
    // The floor under the judge: a run that called nothing produced no
    // evidence, and a verdict of `verified` means the evidence shows the
    // outcome. An agent with tools that touched none of them can be right,
    // but the run cannot show it — unverified, with the reason, however the
    // judge read the reply.
    let (verdict, reason) = if verdict == "verified" && evidence.calls.is_empty() && !catalog.is_empty() && !remembered_in_turn(messages) {
        (
            "unverified".to_owned(),
            format!("nothing was called, so the run holds no evidence for the reply; the judge had said: {reason}"),
        )
    } else {
        (verdict, reason)
    };
    Verdict {
        verdict,
        reason,
        evidence,
        judge: judge_record,
        at,
        repaired: None,
        graph: None,
    }
}

/// Where verdicts live: one JSON file per run under `{store_path}/verifications/`.
#[derive(Debug)]
pub struct VerificationPlane {
    root: PathBuf,
}

impl VerificationPlane {
    pub fn new(store_path: &Path) -> Self {
        Self {
            root: store_path.join("verifications"),
        }
    }

    pub async fn persist(&self, run_id: &str, verdict: &Verdict) {
        if let Err(error) = crate::connectors::persist_json(&self.root, run_id, verdict).await {
            tracing::warn!(%run_id, %error, "verdict not persisted");
        }
    }

    /// The verdict for a run, when one was recorded. Corrupt files read as
    /// absent, the plane convention.
    pub fn load(&self, run_id: &str) -> Option<Value> {
        let bytes = std::fs::read(self.root.join(format!("{run_id}.json"))).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// The verdict as a [`Verdict`], for judging again.
    pub fn load_verdict(&self, run_id: &str) -> Option<Verdict> {
        serde_json::from_value(self.load(run_id)?).ok()
    }

    /// What the judge read — the thread's messages as they stood, the
    /// newest [`TRANSCRIPT_MESSAGES`] of them — kept beside the verdict
    /// (`{run_id}.transcript.json`) so it can be judged again later, by
    /// another judge or by this one after a change. Never served with the
    /// verdict.
    pub async fn persist_transcript(&self, run_id: &str, messages: &[ChatMessage]) {
        let kept = kept_transcript(messages);
        if let Err(error) = crate::connectors::persist_json(&self.root, &format!("{run_id}.transcript"), &kept).await {
            tracing::warn!(%run_id, %error, "verdict transcript not persisted");
        }
    }

    pub fn load_transcript(&self, run_id: &str) -> Option<Vec<ChatMessage>> {
        let bytes = std::fs::read(self.root.join(format!("{run_id}.transcript.json"))).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn a_number_the_reply_names_only_to_retract_it_is_not_invented() {
        use rusty_agent_runtime::llm::ToolCall;
        let messages = vec![
            ChatMessage::user("How many open, and the newest?"),
            ChatMessage::assistant("Newest: INC0010004."),
            ChatMessage::user("check again"),
            ChatMessage::assistant_tool_calls(vec![ToolCall::new("c1", "list", json!({}))]),
            ChatMessage::tool_result("c1", "[{\"number\": \"INC0010106\"}]"),
            ChatMessage::assistant("Newest: INC0010106. What I got wrong: my previous reply cited INC0010004, which no tool result contains."),
        ];
        assert!(unshown_identifiers(&messages, None).is_empty(), "a retraction is not a claim");
        // Named as a fact in one sentence and disowned in another: still a claim.
        let mut claimed = messages.clone();
        claimed.pop();
        claimed.push(ChatMessage::assistant("Newest: INC0010004. Earlier I said INC0010004 was wrong."));
        assert_eq!(unshown_identifiers(&claimed, None), vec!["INC0010004".to_owned()]);
    }

    #[test]
    fn a_repair_notice_quoting_the_invented_number_does_not_make_it_shown() {
        let messages = vec![
            ChatMessage::system("File it."),
            ChatMessage::user("Please file the printer outage."),
            ChatMessage::assistant("Filed INC0099999 for you."),
            ChatMessage::system(repair_notice("failed", "the reply names INC0099999, which no tool result in this conversation contains")),
            ChatMessage::assistant("Filed INC0099999 for you."),
        ];
        assert_eq!(unshown_identifiers(&messages, Some("File it.")), vec!["INC0099999".to_owned()]);
    }
    use rusty_agent_runtime::llm::ToolCall;
    use rusty_agent_runtime::record::Effect;

    fn capability(name: &str, effect: Effect) -> ToolCapability {
        ToolCapability {
            name: name.to_owned(),
            description: "a tool".to_owned(),
            parameters_schema: json!({"type": "object"}),
            effect,
            reconcile: None,
        }
    }

    /// A judge that must never be consulted: the mechanical rules decide first.
    struct NeverAsked;

    #[async_trait::async_trait]
    impl ChatModel for NeverAsked {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _tools: &[Value],
        ) -> rusty_agent_runtime::error::Result<rusty_agent_runtime::llm::ChatResponse> {
            panic!("the judge must not be asked when a rule already decided");
        }
    }

    /// A judge that records what it was asked and answers verified.
    struct RecordingJudge(std::sync::Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl ChatModel for RecordingJudge {
        async fn chat(
            &self,
            messages: &[ChatMessage],
            _tools: &[Value],
        ) -> rusty_agent_runtime::error::Result<rusty_agent_runtime::llm::ChatResponse> {
            let asked = messages.iter().filter_map(|m| m.content.clone()).collect::<Vec<_>>().join("\n");
            self.0.lock().unwrap().push(asked);
            Ok(rusty_agent_runtime::llm::ChatResponse {
                message: ChatMessage::assistant(r#"{"verdict": "verified", "reason": "fine"}"#),
                model: Some("stub".to_owned()),
                usage: None,
            })
        }
    }

    #[tokio::test]
    async fn when_nothing_was_called_the_judge_is_told_the_reply_speaks_for_itself() {
        let messages = vec![
            ChatMessage::user("How do I set up my work email on my phone?"),
            ChatMessage::assistant("That topic is not covered in the knowledge base."),
        ];
        let catalog = vec![capability("servicenow.list-records", Effect::ReadOnly)];
        let judge = RecordingJudge(std::sync::Mutex::new(Vec::new()));
        let _ = verify(&judge, Some("Search kb_knowledge, then answer."), &messages, &catalog).await;
        let asked = judge.0.lock().unwrap().join("\n");
        assert!(asked.contains("NO TOOL WAS CALLED"), "{asked}");
        assert!(asked.contains("0 call(s)"), "{asked}");
    }

    #[tokio::test]
    async fn a_run_that_called_nothing_is_never_verified_whatever_the_judge_says() {
        let messages = vec![
            ChatMessage::user("How do I set up my work email on my phone?"),
            ChatMessage::assistant("I cannot find any relevant articles in the knowledge base."),
        ];
        let catalog = vec![capability("servicenow.list-records", Effect::ReadOnly)];
        let judge = RecordingJudge(std::sync::Mutex::new(Vec::new()));
        let verdict = verify(&judge, Some("Search kb_knowledge, then answer."), &messages, &catalog).await;
        assert_eq!(verdict.verdict, "unverified", "{}", verdict.reason);
        assert!(verdict.reason.contains("nothing was called"), "{}", verdict.reason);
        // An agent with no tools at all is judged on its words alone, and
        // the judge is told its charter is the evidence.
        let verdict = verify(&judge, Some("Answer from what you know."), &messages, &[]).await;
        assert_eq!(verdict.verdict, "verified");
        let asked = judge.0.lock().unwrap().last().cloned().unwrap_or_default();
        assert!(asked.contains("THIS AGENT HAS NO TOOLS"), "{asked}");
    }

    #[test]
    fn a_cut_result_names_the_identifiers_in_the_part_not_shown() {
        let mut text = "x".repeat(RESULT_EXCERPT + 10);
        text.push_str(" ... article KB0010006 and KB0010015 at the end");
        let shown = excerpt(&text);
        assert!(shown.contains("identifiers in the part not shown: KB0010006, KB0010015"), "{}", &shown[shown.len() - 200..]);
        let short = excerpt("KB0010001 fits");
        assert_eq!(short, "KB0010001 fits");
    }

    #[test]
    fn a_cut_result_shows_the_text_around_the_records_the_reply_cites() {
        let mut text = "x".repeat(RESULT_EXCERPT + 10);
        text.push_str(" {\"number\":\"KB0010006\",\"text\":\"Download the VPN client from the IT portal, install it, click Connect.\"} {\"number\":\"KB0010015\",\"text\":\"Mac: Cisco AnyConnect, vpn.company.com\"}");
        let shown = excerpt_for(&text, &["KB0010006".to_owned()]);
        assert!(shown.contains("[around KB0010006, from the part not shown: …"), "{}", &shown[shown.len() - 400..]);
        assert!(shown.contains("Download the VPN client from the IT portal"));
        // The record not cited is named, not shown.
        assert!(shown.contains("other identifiers in the part not shown: KB0010015"));
        // A cited record already in the head gets no window.
        let head_has = format!("KB0010001 {}", "y".repeat(RESULT_EXCERPT + 10));
        assert!(!excerpt_for(&head_has, &["KB0010001".to_owned()]).contains("[around"));
    }

    #[test]
    fn identifiers_are_capitals_then_four_or_more_digits_standing_alone() {
        let found = identifiers("Created P0000001; see INC0010097 and CHG0030001. Not V2, not 8am, not ABCDEFG1234, not x1234, not INC0010097again, INC0010097 once.");
        assert_eq!(found, vec!["P0000001", "INC0010097", "CHG0030001"]);
    }

    #[tokio::test]
    async fn a_number_no_tool_showed_is_a_fabricated_completion_decided_without_the_judge() {
        let messages = vec![
            ChatMessage::user("Problem report from Ivo. Subject: backups fail."),
            ChatMessage::assistant_tool_calls(vec![ToolCall::new("a", "servicenow.list-records", json!({"table": "problem"}))]),
            ChatMessage::tool_result("a", r#"{"result":[]}"#),
            ChatMessage::assistant("No existing problem found. Created new problem with number P0000001, state 'New'."),
        ];
        let catalog = vec![
            capability("servicenow.list-records", Effect::ReadOnly),
            capability("servicenow.create-record", Effect::Compensatable),
        ];
        let verdict = verify(&NeverAsked, Some("Follow the never-file-twice skill."), &messages, &catalog).await;
        assert_eq!(verdict.verdict, "failed");
        assert!(verdict.reason.contains("P0000001"), "{}", verdict.reason);
        assert_eq!(verdict.judge["rule"], json!("unshown_identifier"));
        assert_eq!(verdict.evidence.writes, 0);
    }

    #[test]
    fn a_number_the_person_named_or_a_tool_returned_is_not_invented() {
        let messages = vec![
            ChatMessage::user("what about INC0010093?"),
            ChatMessage::assistant_tool_calls(vec![ToolCall::new("a", "servicenow.list-records", json!({}))]),
            ChatMessage::tool_result("a", r#"{"result":[{"number":"INC0010097","state":"New"}]}"#),
            ChatMessage::assistant("INC0010097 is open."),
            ChatMessage::user("and the other one?"),
            ChatMessage::assistant("INC0010093 is closed; INC0010097 is still open, unlike PRB0040001."),
        ];
        assert_eq!(unshown_identifiers(&messages, Some("charter")), vec!["PRB0040001"]);
        assert_eq!(unshown_identifiers(&messages, Some("the charter mentions PRB0040001")), Vec::<String>::new());
    }

    #[test]
    fn a_call_a_person_declined_is_refused_not_ok() {
        let turn = vec![
            ChatMessage::assistant_tool_calls(vec![ToolCall::new("c1", "echo-board.post-notice", serde_json::json!({"text": "hi"}))]),
            ChatMessage::tool_result("c1", format!("{} (Bob: not now). Do not retry it or work around it; say what you would have done and finish.", rusty_agent_runtime::react::DENIED_NOTICE)),
        ];
        let catalog = vec![capability("read", Effect::ReadOnly), capability("send", Effect::NonIdempotent)];
        let evidence = turn_evidence(&turn, &catalog);
        assert_eq!(evidence.calls[0].outcome, "declined");
        assert_eq!((evidence.declined, evidence.refused, evidence.succeeded), (1, 0, 0));
    }

    fn verdict_with(verdict: &str, read: bool, evidence: Evidence) -> Verdict {
        Verdict { verdict: verdict.to_owned(), reason: String::new(), evidence, judge: json!({"read": read}), at: chrono::Utc::now(), repaired: None, graph: None }
    }

    #[test]
    fn what_sends_the_model_back() {
        let nothing = Evidence::default();
        assert!(sends_back(&verdict_with("failed", true, nothing.clone()), false));
        assert!(sends_back(&verdict_with("unverified", true, nothing.clone()), true));
        // Unsettled but the agent could not have written: left alone.
        assert!(!sends_back(&verdict_with("unverified", true, nothing.clone()), false));
        // An unverified minted for an unreadable judge is not the judge's word.
        assert!(!sends_back(&verdict_with("unverified", false, nothing.clone()), true));
        assert!(!sends_back(&verdict_with("verified", true, nothing.clone()), true));
        // Something wrote: the claim has evidence to be judged against.
        assert!(!sends_back(&verdict_with("unverified", true, Evidence { writes: 1, ..Evidence::default() }), true));
        // A person declined a call: the agent stopped as told; never sent back.
        assert!(!sends_back(&verdict_with("failed", true, Evidence { declined: 1, ..Evidence::default() }), true));
    }

    #[tokio::test]
    async fn the_judge_is_told_a_person_declined() {
        let messages = vec![
            ChatMessage::user("file a ticket for the nest"),
            ChatMessage::assistant_tool_calls(vec![ToolCall::new("c1", "desk.create-ticket", json!({"room": "2A"}))]),
            ChatMessage::tool_result("c1", format!("{} (Bob: not 2A). Do not retry it or work around it; say what you would have done and finish.", rusty_agent_runtime::react::DENIED_NOTICE)),
            ChatMessage::assistant("I would have filed it for room 2A; Bob declined that. Nothing was filed."),
        ];
        let catalog = vec![capability("desk.create-ticket", Effect::NonIdempotent)];
        let judge = RecordingJudge(std::sync::Mutex::new(Vec::new()));
        let verdict = verify(&judge, Some("charter"), &messages, &catalog).await;
        assert_eq!(verdict.evidence.declined, 1);
        let prompt = judge.0.lock().unwrap().last().cloned().expect("asked");
        assert!(prompt.contains("A PERSON DECLINED A CALL"), "{prompt}");
        assert!(prompt.contains("0 write(s) took effect"), "{prompt}");
    }

    /// A remembered fact, an upsert, a keyed create: a write that took
    /// effect once, however safely it could be repeated. Before this the
    /// verifier told the judge "no write took effect" after `memory.remember`
    /// and the judge failed a reply that said "noted".
    #[test]
    fn an_idempotent_write_that_took_effect_is_a_write() {
        let turn = vec![
            ChatMessage::user("my office is 2A"),
            ChatMessage::assistant_tool_calls(vec![ToolCall::new("m", "memory.remember", json!({"text": "My office is 2A."}))]),
            ChatMessage::tool_result("m", r#"{"remembered":"My office is 2A.","new":true}"#),
            ChatMessage::assistant("Noted."),
        ];
        let catalog = vec![capability("memory.remember", Effect::Idempotent), capability("read", Effect::ReadOnly)];
        let evidence = turn_evidence(&turn, &catalog);
        assert_eq!(evidence.calls[0].effect, "idempotent");
        assert_eq!((evidence.writes, evidence.succeeded), (1, 1));
        // A read is still no write.
        let read = vec![
            ChatMessage::user("what is open"),
            ChatMessage::assistant_tool_calls(vec![ToolCall::new("r", "read", json!({}))]),
            ChatMessage::tool_result("r", "data"),
            ChatMessage::assistant("one thing"),
        ];
        assert_eq!(turn_evidence(&read, &catalog).writes, 0);
    }

    #[test]
    fn remembered_notes_are_shown_identifiers_and_ride_the_transcript() {
        use rusty_agent_runtime::memory::{MemoryKind, MemoryProvenance, MemoryRecord, MemoryScope, ProvenanceAuthor, ScopeAddress, ValidityWindow};
        let now = chrono::Utc::now();
        let note = MemoryRecord::new(
            MemoryKind::Fact,
            ScopeAddress::new(MemoryScope::Agent, "desk"),
            MemoryProvenance { author: ProvenanceAuthor::Agent { agent_id: "clerk".to_owned() }, evidence: Default::default(), written_at: now },
            0.8,
            ValidityWindow { valid_from: now, valid_until: None },
            now,
            json!({"text": "The work you delegated to Clerk is done — it reported: the oldest open incident is INC0010001."}),
        )
        .unwrap();
        let names = std::collections::HashMap::from([("clerk".to_owned(), "Clerk".to_owned())]);
        let remembered = remembered_message(std::slice::from_ref(&note), &names).unwrap();
        assert!(remembered.content.as_deref().unwrap().contains("INC0010001"));
        assert!(remembered.content.as_deref().unwrap().contains("written by Clerk (agent clerk)"));
        assert!(remembered_message(&[note], &Default::default()).unwrap().content.as_deref().unwrap().contains("written by agent clerk"));
        let messages = vec![
            ChatMessage::system("You are the desk."),
            ChatMessage::user("What did the clerk find?"),
            remembered,
            ChatMessage::assistant("From memory (the clerk's report): the oldest open incident is INC0010001."),
        ];
        // The number came from a note, not from nowhere.
        assert!(unshown_identifiers(&messages, Some("You are the desk.")).is_empty());
        let (_, did, _) = transcript(&messages);
        assert!(did.starts_with(REMEMBERED_PREFIX), "{did}");
        assert!(did.contains("INC0010001"));
        let evidence = turn_evidence(&messages, &[]);
        assert_eq!(evidence.remembered, 1);
        assert!(evidence.calls.is_empty());
        // Nothing read: no message, nothing counted.
        assert!(remembered_message(&[], &Default::default()).is_none());
        assert_eq!(turn_evidence(&messages[..2], &[]).remembered, 0);
    }

    #[test]
    fn evidence_counts_what_the_turn_did_by_effect_and_outcome() {
        let messages = vec![
            ChatMessage::user("old turn"),
            ChatMessage::assistant_tool_calls(vec![ToolCall::new("z", "send", json!({}))]),
            ChatMessage::tool_result("z", "sent"),
            ChatMessage::user("do it"),
            ChatMessage::assistant_tool_calls(vec![
                ToolCall::new("a", "read", json!({})),
                ToolCall::new("b", "send", json!({})),
                ToolCall::new("c", "send", json!({})),
            ]),
            ChatMessage::tool_result("a", "data"),
            ChatMessage::tool_result("b", "ERROR: refused by policy"),
            ChatMessage::tool_result("c", REPEATED_CALL_NOTICE),
            ChatMessage::assistant_tool_calls(vec![ToolCall::new("d", "mystery", json!({}))]),
            ChatMessage::assistant("done"),
        ];
        let catalog = vec![capability("read", Effect::ReadOnly), capability("send", Effect::NonIdempotent)];
        let evidence = turn_evidence(&messages, &catalog);
        let outcomes: Vec<(&str, &str, &str)> = evidence
            .calls
            .iter()
            .map(|c| (c.tool.as_str(), c.effect.as_str(), c.outcome.as_str()))
            .collect();
        assert_eq!(
            outcomes,
            vec![
                ("read", "read_only", "ok"),
                ("send", "non_idempotent", "error"),
                ("send", "non_idempotent", "refused"),
                ("mystery", "unknown", "unanswered"),
            ]
        );
        assert_eq!((evidence.writes, evidence.refused, evidence.unanswered), (0, 1, 1));
        assert_eq!((evidence.succeeded, evidence.errors), (1, 1));
    }

    #[test]
    fn the_transcript_is_the_last_turn_only() {
        let messages = vec![
            ChatMessage::system("CHARTER"),
            ChatMessage::user("first"),
            ChatMessage::assistant("one"),
            ChatMessage::user("second"),
            ChatMessage::assistant_tool_calls(vec![ToolCall::new("a", "read", json!({"k": 1}))]),
            ChatMessage::tool_result("a", "x".repeat(RESULT_EXCERPT + 500)),
            ChatMessage::assistant("two"),
        ];
        let (request, did, reply) = transcript(&messages);
        assert_eq!(request, "second");
        assert!(did.starts_with("CALLED read({\"k\":1})\nRESULT: xxxx"));
        // A clipped result says so, and says the clip is not absence.
        assert!(did.contains("500 more characters not shown"));
        assert!(did.contains("that absence is not evidence"));
        assert_eq!(reply, "two");
    }

    #[test]
    fn a_verdict_is_read_from_the_first_json_object_and_nothing_else_counts() {
        assert_eq!(
            parse_answer("Sure: {\"verdict\": \"Verified\", \"reason\": \"echo said hello\"} ok"),
            Some(("verified".to_owned(), "echo said hello".to_owned()))
        );
        assert_eq!(parse_answer("{\"verdict\": \"maybe\"}"), None);
        assert_eq!(parse_answer("I think it worked."), None);
    }
}
