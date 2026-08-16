//! Local command execution under an explicit containment policy.
//!
//! This is the most dangerous built-in capability in the crate, so the
//! design bar is containment first:
//!
//! - **Allowlist, not blocklist.** [`CliPolicy`] names the exact programs
//!   that may run. A requested program is resolved to an absolute,
//!   canonical path inside the policy's search directories; anything else —
//!   an unlisted name, a path with separators, a symlink escaping the
//!   search root — is refused before spawn.
//! - **No shell by default.** Programs are spawned as `argv` directly.
//!   Passing a raw `command` string through `/bin/sh -c` is possible only
//!   when the embedder sets [`CliPolicy::with_shell`]; the jail, ceilings,
//!   and timeout still apply.
//! - **Jailed cwd.** The working directory is canonicalized and must stay
//!   inside the policy root (default: the embedder's workspace directory).
//! - **Scrubbed environment.** The child starts with an empty environment;
//!   only variables named in the policy's env allowlist are forwarded.
//!   Evidence records never contain raw environment values.
//! - **Bounded output and time.** stdout and stderr are stream-capped; a
//!   process that floods a pipe is killed, not buffered. Every run has a
//!   timeout (default 30s, hard-capped).
//!
//! The tool declares [`Effect::NonIdempotent`] — an irreversible effect in
//! the R0.7 taxonomy — so dispatch through a guarded
//! [`crate::tool::ToolExecutor`] requires a one-shot
//! [`crate::effects::ApprovalToken`] per occurrence. Embedders that mark a
//! policy read-only ([`CliPolicy::with_read_only`], for tools like `ls` or
//! `git status`) get [`Effect::ReadOnly`] instead: still jailed and
//! ceilinged, but not approval-gated.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

use super::Tool;
use crate::error::{Result, RustyError};
use crate::record::Effect;

/// Default per-invocation timeout.
pub const DEFAULT_CLI_TIMEOUT: Duration = Duration::from_secs(30);
/// Hard ceiling for any CLI timeout — a policy or a call may ask for less,
/// never more.
pub const MAX_CLI_TIMEOUT: Duration = Duration::from_secs(300);
/// Default combined-per-stream output ceiling (stdout and stderr each).
pub const DEFAULT_CLI_OUTPUT_BYTES: usize = 64 * 1024;
/// Hard ceiling for a single output stream — asks beyond this are refused.
pub const MAX_CLI_OUTPUT_BYTES: usize = 1024 * 1024;
/// Maximum number of `argv` entries one call may carry.
pub const MAX_CLI_ARGS: usize = 64;
/// Maximum length of one `argv` entry.
pub const MAX_CLI_ARG_BYTES: usize = 4096;
/// Maximum length of a raw shell `command` string.
pub const MAX_CLI_COMMAND_BYTES: usize = 8192;

/// Directories searched when resolving an allowlisted program name.
pub const DEFAULT_CLI_SEARCH_PATHS: [&str; 4] =
    ["/usr/bin", "/bin", "/usr/local/bin", "/opt/homebrew/bin"];

/// The containment policy every [`CliTool`] executes under.
///
/// Construction canonicalizes the jail root and validates the allowlist, so
/// a misconfigured policy fails before any command can run.
#[derive(Debug, Clone)]
pub struct CliPolicy {
    root: PathBuf,
    programs: Vec<String>,
    search_paths: Vec<PathBuf>,
    env_allowlist: Vec<String>,
    timeout: Duration,
    max_output_bytes: usize,
    shell: bool,
    read_only: bool,
}

impl CliPolicy {
    /// A policy jailed to `root` that may run exactly `programs`.
    ///
    /// `root` must be an existing directory; it becomes the default (and
    /// the boundary) for every call's working directory. `programs` are
    /// bare names (`"ls"`, `"git"`) — absolute paths or names containing
    /// separators are rejected, because resolution is the policy's job.
    pub fn new(
        root: impl AsRef<Path>,
        programs: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self> {
        let root = std::fs::canonicalize(root.as_ref()).map_err(|error| {
            RustyError::Tool(format!("cli jail root could not be opened: {error}"))
        })?;
        if !root.is_dir() {
            return Err(RustyError::Tool("cli jail root must be a directory".into()));
        }
        let mut policy = Self {
            root,
            programs: Vec::new(),
            search_paths: DEFAULT_CLI_SEARCH_PATHS.iter().map(PathBuf::from).collect(),
            env_allowlist: Vec::new(),
            timeout: DEFAULT_CLI_TIMEOUT,
            max_output_bytes: DEFAULT_CLI_OUTPUT_BYTES,
            shell: false,
            read_only: false,
        };
        for program in programs {
            policy.allow_program(program.into())?;
        }
        Ok(policy)
    }

    /// Add one program name to the allowlist.
    pub fn allow_program(&mut self, program: String) -> Result<&mut Self> {
        if program.is_empty()
            || program.len() > 128
            || program
                .bytes()
                .any(|byte| !byte.is_ascii_alphanumeric() && !b"._+-".contains(&byte))
        {
            return Err(RustyError::Tool(format!(
                "cli program `{program}` must be a bare name of 1..=128 ASCII letters, digits, `.`, `_`, `+`, or `-`"
            )));
        }
        if !self.programs.contains(&program) {
            self.programs.push(program);
        }
        Ok(self)
    }

    /// Replace the directories searched when resolving program names.
    pub fn with_search_paths(
        mut self,
        paths: impl IntoIterator<Item = impl Into<PathBuf>>,
    ) -> Self {
        self.search_paths = paths.into_iter().map(Into::into).collect();
        self
    }

    /// Forward only these environment variables into the child. Anything
    /// not named here — credentials, tokens, `HOME` — is scrubbed.
    pub fn with_env_allowlist(
        mut self,
        names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.env_allowlist = names.into_iter().map(Into::into).collect();
        self
    }

    /// Set the default timeout, clamped to [`MAX_CLI_TIMEOUT`].
    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self> {
        if timeout.is_zero() || timeout > MAX_CLI_TIMEOUT {
            return Err(RustyError::Tool(format!(
                "cli timeout must be between 1ms and {}s",
                MAX_CLI_TIMEOUT.as_secs()
            )));
        }
        self.timeout = timeout;
        Ok(self)
    }

    /// Set the per-stream output ceiling, clamped to [`MAX_CLI_OUTPUT_BYTES`].
    pub fn with_max_output_bytes(mut self, bytes: usize) -> Result<Self> {
        if bytes == 0 || bytes > MAX_CLI_OUTPUT_BYTES {
            return Err(RustyError::Tool(format!(
                "cli output ceiling must be between 1 byte and {MAX_CLI_OUTPUT_BYTES}"
            )));
        }
        self.max_output_bytes = bytes;
        Ok(self)
    }

    /// Permit raw `command` strings executed through `/bin/sh -c`. Off by
    /// default: the embedder opts into shell interpolation explicitly, and
    /// the jail, ceilings, and timeout still apply.
    pub fn with_shell(mut self, shell: bool) -> Self {
        self.shell = shell;
        self
    }

    /// Declare every command under this policy read-only (e.g. `ls`,
    /// `git status`). The tool then reports [`Effect::ReadOnly`]: dispatch
    /// needs no approval token, but the jail, ceilings, and timeout are
    /// unchanged.
    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Whether this policy declares its commands read-only.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Resolve an allowlisted program name to an absolute canonical path
    /// inside a search directory.
    fn resolve(&self, program: &str) -> Result<PathBuf> {
        if !self.programs.iter().any(|listed| listed == program) {
            return Err(RustyError::Tool(format!(
                "cli program `{program}` is not in the policy allowlist"
            )));
        }
        for dir in &self.search_paths {
            let candidate = dir.join(program);
            if !candidate.is_file() || !is_executable(&candidate) {
                continue;
            }
            let resolved = std::fs::canonicalize(&candidate).map_err(|error| {
                RustyError::Tool(format!(
                    "cli program `{program}` could not be resolved: {error}"
                ))
            })?;
            let canonical_dir = std::fs::canonicalize(dir).map_err(|error| {
                RustyError::Tool(format!(
                    "cli search path `{}` is unusable: {error}",
                    dir.display()
                ))
            })?;
            if resolved.starts_with(&canonical_dir) {
                return Ok(resolved);
            }
        }
        Err(RustyError::Tool(format!(
            "cli program `{program}` did not resolve inside the policy search paths"
        )))
    }

    /// Jail a caller-supplied working directory to the policy root.
    fn jail_cwd(&self, relative: Option<&str>) -> Result<(PathBuf, String)> {
        let Some(relative) = relative else {
            return Ok((self.root.clone(), ".".to_owned()));
        };
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
                "run_cli cwd must stay inside the configured jail root".into(),
            ));
        }
        let target = std::fs::canonicalize(self.root.join(path)).map_err(|error| {
            RustyError::Tool(format!("cli cwd `{relative}` could not be opened: {error}"))
        })?;
        if !target.starts_with(&self.root) {
            return Err(RustyError::Tool(
                "run_cli refused a cwd outside the configured jail root".into(),
            ));
        }
        if !target.is_dir() {
            return Err(RustyError::Tool("run_cli cwd must be a directory".into()));
        }
        Ok((target, relative.to_owned()))
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// The structured evidence one [`CliTool`] execution leaves behind.
///
/// Recorded for every run — success, failure, timeout, or flood-kill — and
/// handed to the tool's evidence sink when one is attached. Deliberately
/// free of raw environment values: the scrubbed allowlist is policy, and
/// policy is not evidence of a specific execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliExecutionRecord {
    /// The allowlisted program name as requested.
    pub program: String,
    /// The absolute path the name resolved to.
    pub resolved: String,
    /// The argv entries (or the `/bin/sh -c` payload when `shell` is set).
    pub args: Vec<String>,
    /// The working directory, relative to the jail root.
    pub cwd: String,
    /// Whether the run went through `/bin/sh -c`.
    pub shell: bool,
    /// The exit code; `None` when the process was killed before exiting.
    pub exit_code: Option<i32>,
    /// Whether the run was killed for exceeding its timeout.
    pub timed_out: bool,
    /// Whether an output stream exceeded its ceiling and the run was killed.
    pub truncated: bool,
    /// Wall-clock duration of the run.
    pub duration_ms: u64,
    /// Total stdout bytes observed (including bytes past the cap).
    pub stdout_bytes: usize,
    /// Total stderr bytes observed (including bytes past the cap).
    pub stderr_bytes: usize,
}

/// Where [`CliExecutionRecord`]s go the moment a run finishes — the
/// effect-journal seam an embedder wires to its own ledger.
pub type CliEvidenceSink = Arc<dyn Fn(&CliExecutionRecord) + Send + Sync>;

/// `run_cli`: execute one local command under a [`CliPolicy`].
///
/// With a default policy the tool declares [`Effect::NonIdempotent`], so a
/// guarded [`crate::tool::ToolExecutor`] refuses dispatch without a
/// one-shot [`crate::effects::ApprovalToken`] for the exact occurrence.
/// Every executed run — including killed and timed-out ones — returns
/// structured output and is reported to the evidence sink.
#[derive(Clone)]
pub struct CliTool {
    policy: Arc<CliPolicy>,
    evidence: Option<CliEvidenceSink>,
}

impl std::fmt::Debug for CliTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliTool")
            .field("policy", &self.policy)
            .field("evidence", &self.evidence.is_some())
            .finish()
    }
}

impl CliTool {
    /// A CLI tool executing under `policy`.
    pub fn new(policy: CliPolicy) -> Self {
        Self {
            policy: Arc::new(policy),
            evidence: None,
        }
    }

    /// Attach the evidence sink (the effect journal, when one is present).
    pub fn with_evidence_sink(mut self, sink: CliEvidenceSink) -> Self {
        self.evidence = Some(sink);
        self
    }

    /// The policy this tool executes under.
    pub fn policy(&self) -> &CliPolicy {
        &self.policy
    }
}

/// Why a run was forcibly ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kill {
    None,
    Timeout,
    OutputCap,
}

#[async_trait]
impl Tool for CliTool {
    fn name(&self) -> &str {
        "run_cli"
    }

    fn description(&self) -> &str {
        "Run one allowlisted local command inside the configured jail, with a scrubbed environment, a timeout, and stream-capped output."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "program": {"type": "string", "minLength": 1, "maxLength": 128},
                "args": {"type": "array", "items": {"type": "string", "maxLength": MAX_CLI_ARG_BYTES}, "maxItems": MAX_CLI_ARGS},
                "command": {"type": "string", "minLength": 1, "maxLength": MAX_CLI_COMMAND_BYTES},
                "cwd": {"type": "string", "maxLength": 1024},
                "timeout_ms": {"type": "integer", "minimum": 1, "maximum": MAX_CLI_TIMEOUT.as_millis() as u64}
            },
            "additionalProperties": false
        })
    }

    fn effect(&self) -> Effect {
        if self.policy.read_only {
            Effect::ReadOnly
        } else {
            Effect::NonIdempotent
        }
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let command = args.get("command").and_then(Value::as_str);
        let (resolved, argv, display_args, shell) = match command {
            Some(command) => {
                if !self.policy.shell {
                    return Err(RustyError::Tool(
                        "run_cli `command` requires the policy shell flag; spawn argv directly instead"
                            .into(),
                    ));
                }
                if command.is_empty() || command.len() > MAX_CLI_COMMAND_BYTES {
                    return Err(RustyError::Tool(format!(
                        "run_cli command must contain 1..={MAX_CLI_COMMAND_BYTES} bytes"
                    )));
                }
                (
                    PathBuf::from("/bin/sh"),
                    vec!["-c".to_owned(), command.to_owned()],
                    vec![command.to_owned()],
                    true,
                )
            }
            None => {
                let program = args.get("program").and_then(Value::as_str).ok_or_else(|| {
                    RustyError::Tool("run_cli requires `program` (argv mode)".into())
                })?;
                let resolved = self.policy.resolve(program)?;
                let mut argv = Vec::new();
                let mut display = Vec::new();
                if let Some(entries) = args.get("args") {
                    let entries = entries.as_array().ok_or_else(|| {
                        RustyError::Tool("run_cli `args` must be an array".into())
                    })?;
                    if entries.len() > MAX_CLI_ARGS {
                        return Err(RustyError::Tool(format!(
                            "run_cli accepts at most {MAX_CLI_ARGS} arguments"
                        )));
                    }
                    for entry in entries {
                        let entry = entry.as_str().ok_or_else(|| {
                            RustyError::Tool("run_cli arguments must be strings".into())
                        })?;
                        if entry.len() > MAX_CLI_ARG_BYTES || entry.contains('\0') {
                            return Err(RustyError::Tool(format!(
                                "run_cli arguments must be NUL-free and at most {MAX_CLI_ARG_BYTES} bytes"
                            )));
                        }
                        argv.push(entry.to_owned());
                        display.push(entry.to_owned());
                    }
                }
                (resolved, argv, display, false)
            }
        };
        let cwd_arg = args.get("cwd").and_then(Value::as_str);
        let (cwd, cwd_display) = self.policy.jail_cwd(cwd_arg)?;
        let timeout = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .map(Duration::from_millis)
            .filter(|timeout| !timeout.is_zero())
            .unwrap_or(self.policy.timeout)
            .min(self.policy.timeout);

        let mut spawn = tokio::process::Command::new(&resolved);
        spawn
            .args(&argv)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear();
        for name in &self.policy.env_allowlist {
            if let Ok(value) = std::env::var(name) {
                spawn.env(name, value);
            }
        }
        let program_display = if shell {
            "sh".to_owned()
        } else {
            args.get("program")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };

        let started = Instant::now();
        let mut child = spawn.spawn().map_err(|error| {
            RustyError::Tool(format!(
                "run_cli could not spawn `{}`: {error}",
                resolved.display()
            ))
        })?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| RustyError::Tool("run_cli stdout pipe missing".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| RustyError::Tool("run_cli stderr pipe missing".into()))?;

        let cap = self.policy.max_output_bytes;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut out_total = 0usize;
        let mut err_total = 0usize;
        let mut out_done = false;
        let mut err_done = false;
        let mut status = None;
        let mut killed = Kill::None;
        let mut out_buf = [0u8; 8192];
        let mut err_buf = [0u8; 8192];
        let timer = tokio::time::sleep(timeout);
        tokio::pin!(timer);

        loop {
            if out_done && err_done && status.is_some() {
                break;
            }
            tokio::select! {
                read = stdout.read(&mut out_buf), if !out_done => {
                    let read = read.map_err(|error| {
                        RustyError::Tool(format!("run_cli stdout read failed: {error}"))
                    })?;
                    if read == 0 {
                        out_done = true;
                    } else {
                        out_total += read;
                        let room = cap.saturating_sub(out.len());
                        out.extend_from_slice(&out_buf[..read.min(room)]);
                        if read > room && killed == Kill::None {
                            killed = Kill::OutputCap;
                            let _ = child.start_kill();
                        }
                    }
                }
                read = stderr.read(&mut err_buf), if !err_done => {
                    let read = read.map_err(|error| {
                        RustyError::Tool(format!("run_cli stderr read failed: {error}"))
                    })?;
                    if read == 0 {
                        err_done = true;
                    } else {
                        err_total += read;
                        let room = cap.saturating_sub(err.len());
                        err.extend_from_slice(&err_buf[..read.min(room)]);
                        if read > room && killed == Kill::None {
                            killed = Kill::OutputCap;
                            let _ = child.start_kill();
                        }
                    }
                }
                exit = child.wait(), if status.is_none() => {
                    status = Some(exit.map_err(|error| {
                        RustyError::Tool(format!("run_cli wait failed: {error}"))
                    })?);
                }
                () = &mut timer => {
                    if killed == Kill::None {
                        killed = Kill::Timeout;
                        let _ = child.start_kill();
                    }
                }
            }
        }
        let status = status.expect("the loop exits only after wait returns");
        let duration = started.elapsed();
        let record = CliExecutionRecord {
            program: program_display.clone(),
            resolved: resolved.display().to_string(),
            args: display_args,
            cwd: cwd_display.clone(),
            shell,
            exit_code: status.code(),
            timed_out: killed == Kill::Timeout,
            truncated: killed == Kill::OutputCap,
            duration_ms: duration.as_millis() as u64,
            stdout_bytes: out_total,
            stderr_bytes: err_total,
        };
        if let Some(sink) = &self.evidence {
            sink(&record);
        }
        Ok(json!({
            "program": record.program,
            "resolved": record.resolved,
            "args": record.args,
            "cwd": record.cwd,
            "shell": record.shell,
            "exit_code": record.exit_code,
            "timed_out": record.timed_out,
            "truncated": record.truncated,
            "duration_ms": record.duration_ms,
            "stdout_bytes": record.stdout_bytes,
            "stderr_bytes": record.stderr_bytes,
            "stdout": String::from_utf8_lossy(&out),
            "stderr": String::from_utf8_lossy(&err),
        }))
    }
}
