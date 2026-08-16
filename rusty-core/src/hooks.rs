//! Claude Code-compatible command hooks (parity wave): an existing user
//! `hooks.json` runs unmodified against the guard seam.
//!
//! # The wire contract honored
//!
//! - **Shape.** `{"hooks": {"PreToolUse": [{"matcher": "Bash|Write",
//!   "hooks": [{"type": "command", "command": "...", "timeout": 30}]}]}}`.
//!   Only the `PreToolUse` event is honored — it is the guarded moment this
//!   crate has. Other events parse and drop, so a file mixing events stays
//!   loadable. A `PreToolUse` entry of any `type` other than `command`
//!   *fails parsing* instead of being silently skipped: a skipped
//!   blocking-intent hook is a security hole, not a compatibility shim.
//! - **Matcher.** Missing, empty, or `"*"` matches every tool; otherwise the
//!   matcher is `|`-separated exact tool names. Upstream treats the matcher
//!   as a full regular expression; this crate carries no regex engine (no
//!   new dependencies), so patterns beyond alternation of exact names
//!   (`Bash(git:*)` and the like) are the documented divergence.
//! - **stdin.** One JSON object: `session_id` (the run scope), `cwd`,
//!   `hook_event_name` (`"PreToolUse"`), `tool_name`, and `tool_input` (the
//!   finalized arguments). `transcript_path` and `permission_mode` are
//!   omitted — this crate has no transcript plane, and permission posture
//!   is the preset layer's business. Tool arguments travel to the hook, as
//!   upstream; the process *environment* never does (see the bounds below).
//! - **Exit codes.** `0` allows (stdout is parsed for a JSON decision); `2`
//!   is a blocking deny with stderr fed back as the reason; any other code
//!   is a non-blocking note the embedder can drain with
//!   [`HookGuard::take_notes`]. Timeouts, spawn failures, and oversized
//!   output are likewise non-blocking notes — the same
//!   fail-open-on-hookup, fail-closed-on-decision split upstream documents.
//! - **JSON decisions** (exit 0). The modern shape,
//!   `{"hookSpecificOutput": {"hookEventName": "PreToolUse",
//!   "permissionDecision": "allow"|"deny"|"ask",
//!   "permissionDecisionReason": "..."}}`, and the legacy top-level
//!   `{"decision": "approve"|"block", "reason": "..."}` are both honored.
//!   `"defer"` maps to silence: the effect boundary below the guard seam is
//!   the permission system deferred to. `updatedInput` is not honored —
//!   rewriting a finalized call is the middleware layer's job, and guards
//!   never mutate. `continue: false` (stop the agent loop) has no
//!   run-control meaning at this seam and is ignored. Plain non-JSON stdout
//!   is a debug-log detail upstream and silence here.
//! - **Merge.** Multiple hooks on one call merge deny > ask > allow, and
//!   identical commands are deduplicated before running.
//!
//! # The ask mapping
//!
//! A hook `ask` maps onto the closed approval vocabulary
//! ([`crate::record::ApprovalDecision`]): the wired
//! [`crate::tool::approval::ApprovalAnswerer`] decides, the asked/decided
//! pair is journaled when an [`crate::tool::approval::ApprovalGate`] is
//! attached, and only `ApprovedOnce` grants. With no answerer wired the ask
//! denies — fail closed.
//!
//! # Bounds
//!
//! - Per-hook timeout (upstream default 60s, hard ceiling 600s), enforced
//!   by killing the child.
//! - Per-stream output ceilings (default 64 KiB, hard ceiling 1 MiB). The
//!   child is drained past the cap so a flooding hook cannot deadlock the
//!   dispatch path, but the overflow is discarded and the hook's verdict
//!   with it.
//! - Commands run through `/bin/sh -c` exactly as configured — tool
//!   arguments are *never* interpolated into the command string; they
//!   travel over stdin only.
//! - Hooks never see secrets: the child environment is scrubbed to a fixed
//!   `PATH` and nothing else.
//!
//! Hooks execute synchronously on the dispatch path
//! ([`crate::tool::ToolGuard::check`] is a synchronous seam), so a hook's
//! latency is run latency, bounded by its timeout. Keep hook timeouts
//! tight.

use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Result, RustyError};
use crate::record::ApprovalRequest;
use crate::tool::approval::{ask_detail, decision_summary, ApprovalAnswerer, ApprovalGate};
use crate::tool::{GuardDenial, GuardedCall, ToolGuard};

/// The only hook event this crate honors: the guarded moment before a tool
/// call dispatches.
pub const HOOK_EVENT_PRE_TOOL_USE: &str = "PreToolUse";

/// Upstream's default per-hook timeout.
pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(60);
/// Hard ceiling for any hook timeout — a config may ask for less, never
/// more.
pub const MAX_HOOK_TIMEOUT: Duration = Duration::from_secs(600);
/// Default per-stream output ceiling (stdout and stderr each).
pub const DEFAULT_HOOK_OUTPUT_BYTES: usize = 64 * 1024;
/// Hard ceiling for a single output stream — asks beyond this are refused.
pub const MAX_HOOK_OUTPUT_BYTES: usize = 1024 * 1024;
/// Maximum length of one hook command string.
pub const MAX_HOOK_COMMAND_BYTES: usize = 8192;

/// The non-blocking note ledger is bounded; the oldest notes drop first.
const MAX_HOOK_NOTES: usize = 64;
/// The fixed `PATH` a hook child receives — the entire environment it gets.
const HOOK_ENV_PATH: &str = "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin";

/// A parsed `hooks.json`: the honored `PreToolUse` matcher groups, in file
/// order.
#[derive(Debug, Clone, Default)]
pub struct HooksConfig {
    groups: Vec<MatcherGroup>,
}

/// One matcher group: a tool-name pattern and the command hooks that run
/// when it matches.
#[derive(Debug, Clone)]
pub struct MatcherGroup {
    matcher: Option<String>,
    hooks: Vec<CommandHook>,
}

/// One command hook: the `/bin/sh -c` payload and its timeout.
#[derive(Debug, Clone)]
pub struct CommandHook {
    command: String,
    timeout: Duration,
}

/// The raw wire shape (`hooks.json`), kept separate from the parsed config
/// so validation rules live in exactly one place.
#[derive(Debug, Deserialize)]
struct HooksFile {
    /// Event name → matcher groups. A `BTreeMap` so unrelated event ordering
    /// can never make the honored group order nondeterministic.
    #[serde(default)]
    hooks: BTreeMap<String, Vec<RawGroup>>,
}

#[derive(Debug, Deserialize)]
struct RawGroup {
    matcher: Option<String>,
    #[serde(default)]
    hooks: Vec<RawHook>,
}

#[derive(Debug, Deserialize)]
struct RawHook {
    #[serde(rename = "type", default)]
    kind: String,
    command: Option<String>,
    timeout: Option<u64>,
}

impl HooksConfig {
    /// Parse a `hooks.json` document.
    ///
    /// Only the `PreToolUse` event is honored; other events parse and drop.
    /// A `PreToolUse` entry of a type other than `command`, an empty or
    /// over-long command, or malformed JSON fails closed with a typed error
    /// — a misconfigured hook plane must fail loudly at load, not silently
    /// at the guarded moment.
    pub fn from_json(text: &str) -> Result<Self> {
        let file: HooksFile = serde_json::from_str(text)
            .map_err(|error| RustyError::Tool(format!("hooks.json did not parse: {error}")))?;
        let mut groups = Vec::new();
        for (event, raw_groups) in file.hooks {
            if event != HOOK_EVENT_PRE_TOOL_USE {
                continue;
            }
            for raw in raw_groups {
                let mut hooks = Vec::with_capacity(raw.hooks.len());
                for entry in raw.hooks {
                    if entry.kind != "command" {
                        return Err(RustyError::Tool(format!(
                            "hooks.json PreToolUse entry uses unsupported type `{}` (only `command` is honored)",
                            entry.kind,
                        )));
                    }
                    let command = entry.command.unwrap_or_default();
                    if command.is_empty() || command.len() > MAX_HOOK_COMMAND_BYTES {
                        return Err(RustyError::Tool(format!(
                            "hooks.json command must contain 1..={MAX_HOOK_COMMAND_BYTES} bytes"
                        )));
                    }
                    let timeout = match entry.timeout {
                        None | Some(0) => DEFAULT_HOOK_TIMEOUT,
                        Some(secs) => Duration::from_secs(secs).min(MAX_HOOK_TIMEOUT),
                    };
                    hooks.push(CommandHook { command, timeout });
                }
                groups.push(MatcherGroup {
                    matcher: raw.matcher.filter(|matcher| !matcher.is_empty()),
                    hooks,
                });
            }
        }
        Ok(Self { groups })
    }

    /// Load and parse a `hooks.json` file.
    pub fn from_file(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path.as_ref()).map_err(|error| {
            RustyError::Tool(format!(
                "hooks file `{}` could not be read: {error}",
                path.as_ref().display()
            ))
        })?;
        Self::from_json(&text)
    }

    /// The honored matcher groups, in file order.
    pub fn groups(&self) -> &[MatcherGroup] {
        &self.groups
    }

    /// `true` when no `PreToolUse` group was honored.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
}

impl MatcherGroup {
    /// The raw matcher pattern; `None` matches every tool.
    pub fn matcher(&self) -> Option<&str> {
        self.matcher.as_deref()
    }

    /// The group's command hooks.
    pub fn hooks(&self) -> &[CommandHook] {
        &self.hooks
    }

    /// Whether this group fires for `tool`: missing, empty, or `"*"`
    /// matches everything; otherwise the matcher is `|`-separated exact
    /// tool names.
    fn matches(&self, tool: &str) -> bool {
        match self.matcher.as_deref() {
            None | Some("") | Some("*") => true,
            Some(pattern) => pattern.split('|').any(|name| name == tool),
        }
    }
}

impl CommandHook {
    /// The `/bin/sh -c` payload.
    pub fn command(&self) -> &str {
        &self.command
    }

    /// The per-hook timeout (upstream default 60s, hard ceiling 600s).
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

/// Why a hook produced a note rather than a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookNoteKind {
    /// A non-zero, non-2 exit: upstream's non-blocking error.
    NonBlockingExit,
    /// The hook exceeded its timeout and was killed.
    Timeout,
    /// An output stream exceeded the byte ceiling; the hook's verdict (if
    /// any) was discarded with the overflow.
    OutputTruncated,
    /// The hook process could not be spawned at all.
    SpawnFailed,
}

/// A non-blocking observation from one hook execution: the hook ran (or
/// failed to) without producing a verdict the merge would honor. Drained
/// with [`HookGuard::take_notes`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookNote {
    /// The hook command, truncated to a bounded prefix for the ledger.
    pub command: String,
    /// Why the hook produced a note rather than a verdict.
    pub kind: HookNoteKind,
    /// The detail: the exit code and truncated stderr, the timeout, the
    /// spawn error.
    pub detail: String,
}

/// One hook's judged verdict on one call.
#[derive(Debug)]
enum HookVerdict {
    Allow,
    Ask(String),
    Deny(String),
}

/// The guard-seam bridge (parity wave): a [`ToolGuard`] that runs the
/// matching `PreToolUse` command hooks on every finalized call and merges
/// their verdicts deny > ask > allow.
///
/// Being a guard is the whole integration: a hook denial flows through the
/// same any-denial-denies composition and journals as
/// [`crate::record::RunEventKind::ToolCallDenied`] exactly like any other
/// guard's — journaled denials come free. Silence remains silence: a hook
/// `allow` cannot lift the effect boundary below the seam, it only means
/// this guard does not deny.
pub struct HookGuard {
    config: HooksConfig,
    answerer: Option<ApprovalAnswerer>,
    gate: Option<ApprovalGate>,
    notes: Mutex<Vec<HookNote>>,
    max_output_bytes: usize,
}

impl std::fmt::Debug for HookGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookGuard")
            .field("config", &self.config)
            .field("answerer", &self.answerer.is_some())
            .field("gate", &self.gate.is_some())
            .field("max_output_bytes", &self.max_output_bytes)
            .finish()
    }
}

impl HookGuard {
    /// A guard over `config` with no ask answerer: a hook `ask` denies
    /// (fail closed). Wire the decision source with
    /// [`HookGuard::with_answerer`].
    pub fn new(config: HooksConfig) -> Self {
        Self {
            config,
            answerer: None,
            gate: None,
            notes: Mutex::new(Vec::new()),
            max_output_bytes: DEFAULT_HOOK_OUTPUT_BYTES,
        }
    }

    /// Builder-style: wire the decision source for hook `ask` verdicts.
    pub fn with_answerer(mut self, answerer: ApprovalAnswerer) -> Self {
        self.answerer = Some(answerer);
        self
    }

    /// Builder-style: journal each answered ask's asked/decided pair
    /// through `gate`.
    pub fn with_approval_gate(mut self, gate: ApprovalGate) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Builder-style: set the per-stream output ceiling, bounded by
    /// [`MAX_HOOK_OUTPUT_BYTES`].
    pub fn with_max_output_bytes(mut self, bytes: usize) -> Result<Self> {
        if bytes == 0 || bytes > MAX_HOOK_OUTPUT_BYTES {
            return Err(RustyError::Tool(format!(
                "hook output ceiling must be between 1 byte and {MAX_HOOK_OUTPUT_BYTES}"
            )));
        }
        self.max_output_bytes = bytes;
        Ok(self)
    }

    /// The parsed config this guard runs.
    pub fn config(&self) -> &HooksConfig {
        &self.config
    }

    /// Drain the non-blocking note ledger (bounded; the oldest notes drop
    /// first). Notes are how timeouts, non-blocking exits, spawn failures,
    /// and truncated output stay observable without ever blocking a call.
    pub fn take_notes(&self) -> Vec<HookNote> {
        let mut ledger = self
            .notes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut *ledger)
    }

    /// The deduplicated hooks whose matchers fire for `tool`, in file
    /// order. Upstream deduplicates identical hook commands; so does this.
    fn matching_hooks(&self, tool: &str) -> Vec<CommandHook> {
        let mut seen = HashSet::new();
        let mut matched = Vec::new();
        for group in &self.config.groups {
            if !group.matches(tool) {
                continue;
            }
            for hook in &group.hooks {
                if seen.insert(hook.command.clone()) {
                    matched.push(hook.clone());
                }
            }
        }
        matched
    }

    /// Append notes to the bounded ledger, oldest dropping first.
    fn record_notes(&self, notes: Vec<HookNote>) {
        if notes.is_empty() {
            return;
        }
        let mut ledger = self
            .notes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for note in notes {
            if ledger.len() >= MAX_HOOK_NOTES {
                ledger.remove(0);
            }
            ledger.push(note);
        }
    }

    /// Resolve the merged ask through the closed approval vocabulary. With
    /// no answerer wired the ask denies — fail closed; with one, only
    /// `ApprovedOnce` grants, and the asked/decided pair is journaled when
    /// a gate is attached.
    fn resolve_ask(&self, call: &GuardedCall<'_>, reason: String) -> Option<GuardDenial> {
        let request = ApprovalRequest {
            kind: call.tool.to_owned(),
            effect_id: None,
            detail: Some(ask_detail([
                ("source", Value::from("claude_hook")),
                ("reason", Value::from(reason.clone())),
                ("arguments", call.arguments.clone()),
                ("scope", Value::from(call.scope)),
            ])),
        };
        let Some(answerer) = &self.answerer else {
            return Some(GuardDenial::new(
                self.name(),
                format!(
                    "hook asked for approval of `{}` ({reason}); no answerer is wired, so the ask denies (fail closed)",
                    call.tool,
                ),
            ));
        };
        let decision = answerer(&request);
        if let Some(gate) = &self.gate {
            gate.decide(&request, decision.clone());
        }
        if decision.grants() {
            None
        } else {
            Some(GuardDenial::new(
                self.name(),
                format!(
                    "hook ask for `{}` was not granted: {}",
                    call.tool,
                    decision_summary(&decision),
                ),
            ))
        }
    }
}

impl ToolGuard for HookGuard {
    fn name(&self) -> &str {
        "claude_hooks"
    }

    fn check(&self, call: &GuardedCall<'_>) -> Option<GuardDenial> {
        let hooks = self.matching_hooks(call.tool);
        if hooks.is_empty() {
            return None;
        }
        let payload =
            serde_json::to_vec(&hook_input(call)).expect("a serde_json::Value always serializes");
        let mut notes = Vec::new();
        let mut verdicts = Vec::with_capacity(hooks.len());
        for hook in &hooks {
            let run = run_hook(&hook.command, hook.timeout, &payload, self.max_output_bytes);
            verdicts.push(interpret(&run, &hook.command, &mut notes));
        }
        self.record_notes(notes);
        // The merge: deny > ask > allow.
        let denials: Vec<&str> = verdicts
            .iter()
            .filter_map(|verdict| match verdict {
                HookVerdict::Deny(reason) => Some(reason.as_str()),
                _ => None,
            })
            .collect();
        if !denials.is_empty() {
            return Some(GuardDenial::new(self.name(), denials.join("; ")));
        }
        let asks: Vec<&str> = verdicts
            .iter()
            .filter_map(|verdict| match verdict {
                HookVerdict::Ask(reason) => Some(reason.as_str()),
                _ => None,
            })
            .collect();
        if !asks.is_empty() {
            return self.resolve_ask(call, asks.join("; "));
        }
        None
    }
}

/// The stdin payload the contract documents, as this crate can honestly
/// supply it: `transcript_path` and `permission_mode` are omitted (no
/// transcript plane; permission posture is the preset layer's business).
fn hook_input(call: &GuardedCall<'_>) -> Value {
    json!({
        "session_id": call.scope,
        "cwd": std::env::current_dir()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default(),
        "hook_event_name": HOOK_EVENT_PRE_TOOL_USE,
        "tool_name": call.tool,
        "tool_input": call.arguments,
    })
}

/// The raw outcome of one hook execution, before interpretation.
#[derive(Debug)]
struct HookRun {
    /// The exit code; `None` when the process ended without one (killed,
    /// signalled, or the wait itself failed).
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
    truncated: bool,
    spawn_error: Option<String>,
}

/// Run one hook command under the contract's bounds: `/bin/sh -c` exactly
/// as configured, the payload on stdin, a scrubbed environment, a killed
/// child on timeout, and drained-but-capped output streams.
///
/// Arguments never enter the command string — they travel over stdin only,
/// so no unescaped interpolation is possible.
fn run_hook(command: &str, timeout: Duration, stdin_payload: &[u8], cap: usize) -> HookRun {
    let spawned = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .env("PATH", HOOK_ENV_PATH)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            return HookRun {
                exit_code: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                timed_out: false,
                truncated: false,
                spawn_error: Some(error.to_string()),
            };
        }
    };
    // Feed stdin from a thread: a hook that never reads its stdin must not
    // deadlock the dispatch path on a full pipe.
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let payload = stdin_payload.to_vec();
    std::thread::spawn(move || {
        let _ = stdin.write_all(&payload);
    });
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let out_reader = std::thread::spawn(move || read_capped(stdout, cap));
    let err_reader = std::thread::spawn(move || read_capped(stderr, cap));

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if Instant::now() >= deadline => {
                timed_out = true;
                let _ = child.kill();
                break child.wait().ok().and_then(|status| status.code());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => break None,
        }
    };
    let (stdout, out_truncated) = out_reader.join().unwrap_or_default();
    let (stderr, err_truncated) = err_reader.join().unwrap_or_default();
    HookRun {
        exit_code,
        stdout,
        stderr,
        timed_out,
        truncated: out_truncated || err_truncated,
        spawn_error: None,
    }
}

/// Drain a stream, keeping at most `cap` bytes and reporting whether any
/// were discarded. The drain never stops early: a hook that floods a pipe
/// it can no longer meaningfully write to must still be able to exit, or
/// every flood would degrade into a timeout-kill.
fn read_capped(mut reader: impl Read, cap: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let room = cap.saturating_sub(kept.len());
                if read > room {
                    truncated = true;
                }
                kept.extend_from_slice(&chunk[..read.min(room)]);
            }
        }
    }
    (kept, truncated)
}

/// Judge one hook run into a verdict, pushing non-blocking observations
/// onto `notes`.
fn interpret(run: &HookRun, command: &str, notes: &mut Vec<HookNote>) -> HookVerdict {
    if let Some(error) = &run.spawn_error {
        notes.push(HookNote {
            command: preview(command),
            kind: HookNoteKind::SpawnFailed,
            detail: error.clone(),
        });
        return HookVerdict::Allow;
    }
    if run.timed_out {
        notes.push(HookNote {
            command: preview(command),
            kind: HookNoteKind::Timeout,
            detail: "killed after exceeding its timeout".to_owned(),
        });
        return HookVerdict::Allow;
    }
    if run.truncated {
        notes.push(HookNote {
            command: preview(command),
            kind: HookNoteKind::OutputTruncated,
            detail: "an output stream exceeded the byte ceiling; the verdict was discarded"
                .to_owned(),
        });
        return HookVerdict::Allow;
    }
    let stderr = String::from_utf8_lossy(&run.stderr).trim().to_owned();
    match run.exit_code {
        Some(0) => parse_decision(&run.stdout),
        Some(2) => HookVerdict::Deny(if stderr.is_empty() {
            format!("hook `{}` blocked the call", preview(command))
        } else {
            stderr
        }),
        Some(code) => {
            notes.push(HookNote {
                command: preview(command),
                kind: HookNoteKind::NonBlockingExit,
                detail: format!("exit {code}: {stderr}"),
            });
            HookVerdict::Allow
        }
        None => {
            notes.push(HookNote {
                command: preview(command),
                kind: HookNoteKind::NonBlockingExit,
                detail: "terminated without an exit status".to_owned(),
            });
            HookVerdict::Allow
        }
    }
}

/// Parse an exit-0 stdout for a JSON decision. Both the modern
/// `hookSpecificOutput.permissionDecision` shape and the legacy top-level
/// `{"decision": "approve"|"block"}` shape are honored; anything else —
/// plain text included — is silence.
fn parse_decision(stdout: &[u8]) -> HookVerdict {
    let text = String::from_utf8_lossy(stdout);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return HookVerdict::Allow;
    }
    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
        return HookVerdict::Allow;
    };
    if let Some(specific) = value.get("hookSpecificOutput") {
        let reason = || decision_reason(specific.get("permissionDecisionReason"));
        match specific.get("permissionDecision").and_then(Value::as_str) {
            Some("allow") => return HookVerdict::Allow,
            Some("deny") => {
                return HookVerdict::Deny(reason().unwrap_or_else(|| "hook denied the call".into()))
            }
            Some("ask") => {
                return HookVerdict::Ask(
                    reason().unwrap_or_else(|| "hook asked for approval".into()),
                )
            }
            // `defer` defers to the permission system — the effect boundary
            // below this seam — which is silence at the guard layer.
            Some("defer") => return HookVerdict::Allow,
            _ => {}
        }
    }
    match value.get("decision").and_then(Value::as_str) {
        Some("block") => HookVerdict::Deny(
            decision_reason(value.get("reason")).unwrap_or_else(|| "hook blocked the call".into()),
        ),
        _ => HookVerdict::Allow,
    }
}

/// A non-empty reason string from a JSON field, when present.
fn decision_reason(field: Option<&Value>) -> Option<String> {
    field
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .map(str::to_owned)
}

/// A bounded prefix of a hook command for the note ledger.
fn preview(command: &str) -> String {
    const MAX_PREVIEW_BYTES: usize = 128;
    if command.len() <= MAX_PREVIEW_BYTES {
        return command.to_owned();
    }
    let mut end = MAX_PREVIEW_BYTES;
    while !command.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &command[..end])
}
