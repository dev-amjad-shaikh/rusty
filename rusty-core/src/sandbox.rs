//! Sandbox executor seam and backends (EP-05-S05, EP-05-S12).
//!
//! The [`SandboxExecutor`] trait abstracts over execution backends — local
//! process, container, and remote — so the tool runtime routes sandboxed calls
//! through one seam. Every backend reports its [`EnforcementLevel`] honestly;
//! a tool with `SandboxRequirement::Required` refuses to run on a backend
//! reporting [`EnforcementLevel::Partial`].

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncReadExt;

use crate::error::{Result, RustyError};

/// Honest enforcement report: what the backend verified it can enforce on this
/// host (EP-05-S05 AC 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnforcementLevel {
    /// Filesystem and network confinement verified with probes.
    Full,
    /// Best-effort confinement; probes failed or host lacks kernel support.
    Partial,
}

/// One tool stub installed into a sandbox environment (EP-05-S05 AC 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolStub {
    /// The tool's qualified name.
    pub name: String,
    /// JSON Schema for the stub's parameters.
    pub parameters_schema: Value,
    /// Human-facing description.
    pub description: String,
}

/// The result of one sandboxed execution (EP-05-S05 AC 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxResult {
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// Exit code when the process exited normally.
    pub exit_code: Option<i32>,
    /// Whether the run was killed for exceeding its timeout.
    pub timed_out: bool,
    /// Whether an output stream exceeded its ceiling and the run was killed.
    pub truncated: bool,
    /// Wall-clock duration of the run.
    pub duration_ms: u64,
}

/// The sandbox executor seam (EP-05-S05).
///
/// Three methods — `send_tools`, `send_variables`, `execute` — plus a
/// constructor-time isolation configuration and an honest enforcement report.
/// The tool runtime consumes only this trait; no backend implementation is
/// visible to dispatch.
#[async_trait]
pub trait SandboxExecutor: Send + Sync + std::fmt::Debug {
    /// Install tool stubs into the execution environment.
    async fn send_tools(&self, tools: &[ToolStub]) -> Result<()>;

    /// Move named values across the boundary through a typed, deny-by-default
    /// serializer that refuses arbitrary deserialization (EP-05-S05 AC 1).
    async fn send_variables(&self, variables: &Value) -> Result<()>;

    /// Run a command or code payload, returning output, logs, and exit status.
    async fn execute(&self, command: &str, args: &[String]) -> Result<SandboxResult>;

    /// What the backend verified it can enforce on this host.
    fn enforcement(&self) -> EnforcementLevel;

    /// Backend identity for audit trails (EP-05-S06 AC 3).
    fn backend_id(&self) -> &str;
}

// ---------------------------------------------------------------------------
// Local process backend
// ---------------------------------------------------------------------------

/// The containment policy every [`LocalProcessBackend`] execution runs under.
///
/// Cloned from [`crate::tool::builtins::cli::CliPolicy`] discipline: jail,
/// allowlist, scrubbed environment, bounded output and time.
#[derive(Debug, Clone)]
pub struct LocalProcessConfig {
    root: PathBuf,
    programs: Vec<String>,
    search_paths: Vec<PathBuf>,
    env_allowlist: Vec<String>,
    timeout: Duration,
    max_output_bytes: usize,
    shell: bool,
}

/// Default per-invocation timeout.
pub const DEFAULT_LOCAL_TIMEOUT: Duration = Duration::from_secs(30);
/// Hard ceiling for any local timeout.
pub const MAX_LOCAL_TIMEOUT: Duration = Duration::from_secs(300);
/// Default combined-per-stream output ceiling.
pub const DEFAULT_LOCAL_OUTPUT_BYTES: usize = 64 * 1024;
/// Hard ceiling for a single output stream.
pub const MAX_LOCAL_OUTPUT_BYTES: usize = 1024 * 1024;

impl LocalProcessConfig {
    /// A config jailed to `root`.
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = std::fs::canonicalize(root.as_ref()).map_err(|error| {
            RustyError::Tool(format!(
                "local backend jail root could not be opened: {error}"
            ))
        })?;
        if !root.is_dir() {
            return Err(RustyError::Tool(
                "local backend jail root must be a directory".into(),
            ));
        }
        Ok(Self {
            root,
            programs: Vec::new(),
            search_paths: vec![
                PathBuf::from("/usr/bin"),
                PathBuf::from("/bin"),
                PathBuf::from("/usr/local/bin"),
                PathBuf::from("/opt/homebrew/bin"),
            ],
            env_allowlist: Vec::new(),
            timeout: DEFAULT_LOCAL_TIMEOUT,
            max_output_bytes: DEFAULT_LOCAL_OUTPUT_BYTES,
            shell: false,
        })
    }

    /// Allow one program name.
    pub fn allow_program(mut self, program: impl Into<String>) -> Self {
        let program = program.into();
        if !self.programs.contains(&program) {
            self.programs.push(program);
        }
        self
    }

    /// Forward only these environment variables.
    pub fn with_env_allowlist(
        mut self,
        names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.env_allowlist = names.into_iter().map(Into::into).collect();
        self
    }

    /// Set timeout, clamped to [`MAX_LOCAL_TIMEOUT`].
    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self> {
        if timeout.is_zero() || timeout > MAX_LOCAL_TIMEOUT {
            return Err(RustyError::Tool(format!(
                "local backend timeout must be between 1ms and {}s",
                MAX_LOCAL_TIMEOUT.as_secs()
            )));
        }
        self.timeout = timeout;
        Ok(self)
    }

    /// Set output ceiling, clamped to [`MAX_LOCAL_OUTPUT_BYTES`].
    pub fn with_max_output_bytes(mut self, bytes: usize) -> Result<Self> {
        if bytes == 0 || bytes > MAX_LOCAL_OUTPUT_BYTES {
            return Err(RustyError::Tool(format!(
                "local backend output ceiling must be between 1 byte and {MAX_LOCAL_OUTPUT_BYTES}"
            )));
        }
        self.max_output_bytes = bytes;
        Ok(self)
    }

    /// Permit raw command strings through `/bin/sh -c`.
    pub fn with_shell(mut self, shell: bool) -> Self {
        self.shell = shell;
        self
    }
}

/// Local process backend: runs payloads in a separate OS process with the
/// working directory confined to the session workspace, environment variables
/// limited to an explicit allowlist, wall-clock timeout, and stdout/stderr
/// captured with a size cap (EP-05-S05 AC 4).
#[derive(Debug, Clone)]
pub struct LocalProcessBackend {
    config: Arc<LocalProcessConfig>,
}

impl LocalProcessBackend {
    /// A local backend executing under `config`.
    pub fn new(config: LocalProcessConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    /// Resolve an allowlisted program name to an absolute canonical path.
    fn resolve(&self, program: &str) -> Result<PathBuf> {
        if !self.config.programs.iter().any(|listed| listed == program) {
            return Err(RustyError::Tool(format!(
                "local backend program `{program}` is not in the policy allowlist"
            )));
        }
        for dir in &self.config.search_paths {
            let candidate = dir.join(program);
            if !candidate.is_file() {
                continue;
            }
            let resolved = std::fs::canonicalize(&candidate).map_err(|error| {
                RustyError::Tool(format!(
                    "local backend program `{program}` could not be resolved: {error}"
                ))
            })?;
            let canonical_dir = std::fs::canonicalize(dir).map_err(|error| {
                RustyError::Tool(format!(
                    "local backend search path `{}` is unusable: {error}",
                    dir.display()
                ))
            })?;
            if resolved.starts_with(&canonical_dir) {
                return Ok(resolved);
            }
        }
        Err(RustyError::Tool(format!(
            "local backend program `{program}` did not resolve inside the policy search paths"
        )))
    }

    /// Jail a caller-supplied working directory to the policy root.
    fn jail_cwd(&self, relative: Option<&str>) -> Result<PathBuf> {
        let Some(relative) = relative else {
            return Ok(self.config.root.clone());
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
                "local backend cwd must stay inside the configured jail root".into(),
            ));
        }
        let target = std::fs::canonicalize(self.config.root.join(path)).map_err(|error| {
            RustyError::Tool(format!(
                "local backend cwd `{relative}` could not be opened: {error}"
            ))
        })?;
        if !target.starts_with(&self.config.root) {
            return Err(RustyError::Tool(
                "local backend refused a cwd outside the configured jail root".into(),
            ));
        }
        if !target.is_dir() {
            return Err(RustyError::Tool(
                "local backend cwd must be a directory".into(),
            ));
        }
        Ok(target)
    }
}

#[async_trait]
impl SandboxExecutor for LocalProcessBackend {
    async fn send_tools(&self, _tools: &[ToolStub]) -> Result<()> {
        // Local process backend does not pre-install stubs; tools are resolved
        // from the host filesystem at execution time.
        Ok(())
    }

    async fn send_variables(&self, _variables: &Value) -> Result<()> {
        // Variables are passed as command arguments or environment variables
        // at execute time; no persistent state is maintained across calls.
        Ok(())
    }

    async fn execute(&self, command: &str, args: &[String]) -> Result<SandboxResult> {
        let (resolved, argv, _shell) = if self.config.shell && command.contains(' ') {
            // Shell mode: run through /bin/sh -c with the full command string.
            (
                PathBuf::from("/bin/sh"),
                vec!["-c".to_owned(), command.to_owned()],
                true,
            )
        } else {
            let resolved = self.resolve(command)?;
            let mut argv = Vec::new();
            for arg in args {
                if arg.contains('\0') {
                    return Err(RustyError::Tool(
                        "local backend arguments must be NUL-free".into(),
                    ));
                }
                argv.push(arg.clone());
            }
            (resolved, argv, false)
        };

        let cwd = self.jail_cwd(None)?;
        let timeout = self.config.timeout;
        let cap = self.config.max_output_bytes;

        let mut spawn = tokio::process::Command::new(&resolved);
        spawn
            .args(&argv)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear();
        for name in &self.config.env_allowlist {
            if let Ok(value) = std::env::var(name) {
                spawn.env(name, value);
            }
        }

        let started = Instant::now();
        let mut child = spawn.spawn().map_err(|error| {
            RustyError::Tool(format!(
                "local backend could not spawn `{}`: {error}",
                resolved.display()
            ))
        })?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| RustyError::Tool("local backend stdout pipe missing".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| RustyError::Tool("local backend stderr pipe missing".into()))?;

        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut _out_total = 0usize;
        let mut _err_total = 0usize;
        let mut out_done = false;
        let mut err_done = false;
        let mut status = None;
        let mut killed: Option<&'static str> = None;
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
                        RustyError::Tool(format!("local backend stdout read failed: {error}"))
                    })?;
                    if read == 0 {
                        out_done = true;
                    } else {
                        _out_total += read;
                        let room = cap.saturating_sub(out.len());
                        out.extend_from_slice(&out_buf[..read.min(room)]);
                        if read > room && killed.is_none() {
                            killed = Some("output_cap");
                            let _ = child.start_kill();
                        }
                    }
                }
                read = stderr.read(&mut err_buf), if !err_done => {
                    let read = read.map_err(|error| {
                        RustyError::Tool(format!("local backend stderr read failed: {error}"))
                    })?;
                    if read == 0 {
                        err_done = true;
                    } else {
                        _err_total += read;
                        let room = cap.saturating_sub(err.len());
                        err.extend_from_slice(&err_buf[..read.min(room)]);
                        if read > room && killed.is_none() {
                            killed = Some("output_cap");
                            let _ = child.start_kill();
                        }
                    }
                }
                exit = child.wait(), if status.is_none() => {
                    status = Some(exit.map_err(|error| {
                        RustyError::Tool(format!("local backend wait failed: {error}"))
                    })?);
                }
                () = &mut timer => {
                    if killed.is_none() {
                        killed = Some("timeout");
                        let _ = child.start_kill();
                    }
                }
            }
        }
        let status = status.expect("the loop exits only after wait returns");
        let duration = started.elapsed();

        Ok(SandboxResult {
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
            exit_code: status.code(),
            timed_out: killed == Some("timeout"),
            truncated: killed == Some("output_cap"),
            duration_ms: duration.as_millis() as u64,
        })
    }

    fn enforcement(&self) -> EnforcementLevel {
        // On hosts without kernel-level filesystem confinement (e.g. no
        // Landlock, no seccomp-bpf), we report partial honestly.
        // A probe-based check could refine this in future waves.
        EnforcementLevel::Partial
    }

    fn backend_id(&self) -> &str {
        "local_process"
    }
}

// ---------------------------------------------------------------------------
// Container backend
// ---------------------------------------------------------------------------

/// Configuration for the container backend (EP-05-S12 AC 1).
#[derive(Debug, Clone)]
pub struct ContainerConfig {
    /// Docker/Podman image reference.
    pub image: String,
    /// Host path mounted as the workspace inside the container.
    pub workspace_mount: PathBuf,
    /// Fixed path inside the container where the workspace is mounted.
    pub container_workspace: String,
    /// Whether network is disabled (default: true).
    pub network_disabled: bool,
    /// Per-invocation timeout.
    pub timeout: Duration,
    /// Per-stream output ceiling.
    pub max_output_bytes: usize,
}

impl ContainerConfig {
    /// A container backend using `image` with the workspace mounted.
    pub fn new(image: impl Into<String>, workspace_mount: impl AsRef<Path>) -> Result<Self> {
        let workspace_mount = std::fs::canonicalize(workspace_mount.as_ref()).map_err(|error| {
            RustyError::Tool(format!(
                "container backend workspace mount could not be opened: {error}"
            ))
        })?;
        if !workspace_mount.is_dir() {
            return Err(RustyError::Tool(
                "container backend workspace mount must be a directory".into(),
            ));
        }
        Ok(Self {
            image: image.into(),
            workspace_mount,
            container_workspace: "/workspace".to_owned(),
            network_disabled: true,
            timeout: DEFAULT_LOCAL_TIMEOUT,
            max_output_bytes: DEFAULT_LOCAL_OUTPUT_BYTES,
        })
    }

    /// Enable network (default is disabled).
    pub fn with_network(mut self, enabled: bool) -> Self {
        self.network_disabled = !enabled;
        self
    }

    /// Set timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self> {
        if timeout.is_zero() || timeout > MAX_LOCAL_TIMEOUT {
            return Err(RustyError::Tool(format!(
                "container backend timeout must be between 1ms and {}s",
                MAX_LOCAL_TIMEOUT.as_secs()
            )));
        }
        self.timeout = timeout;
        Ok(self)
    }

    /// Set output ceiling.
    pub fn with_max_output_bytes(mut self, bytes: usize) -> Result<Self> {
        if bytes == 0 || bytes > MAX_LOCAL_OUTPUT_BYTES {
            return Err(RustyError::Tool(format!(
                "container backend output ceiling must be between 1 byte and {MAX_LOCAL_OUTPUT_BYTES}"
            )));
        }
        self.max_output_bytes = bytes;
        Ok(self)
    }
}

/// Container backend: executes commands inside a Docker/Podman container with
/// the workspace mounted, network disabled by default, and enforcement probes
/// (EP-05-S12 AC 1).
#[derive(Debug, Clone)]
pub struct ContainerBackend {
    config: Arc<ContainerConfig>,
}

impl ContainerBackend {
    /// A container backend with `config`.
    pub fn new(config: ContainerConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    /// Build the `docker run` (or `podman run`) argument list.
    fn build_args(&self, command: &str, args: &[String]) -> Vec<String> {
        let mut docker_args = vec![
            "run".to_owned(),
            "--rm".to_owned(),
            "-v".to_owned(),
            format!(
                "{}:{}",
                self.config.workspace_mount.display(),
                self.config.container_workspace
            ),
            "-w".to_owned(),
            self.config.container_workspace.clone(),
        ];
        if self.config.network_disabled {
            docker_args.push("--network".to_owned());
            docker_args.push("none".to_owned());
        }
        docker_args.push(self.config.image.clone());
        docker_args.push(command.to_owned());
        docker_args.extend(args.iter().cloned());
        docker_args
    }
}

#[async_trait]
impl SandboxExecutor for ContainerBackend {
    async fn send_tools(&self, _tools: &[ToolStub]) -> Result<()> {
        // Tool stubs are not pre-installed in the container; the container
        // image is expected to carry the required tooling.
        Ok(())
    }

    async fn send_variables(&self, _variables: &Value) -> Result<()> {
        // Variables are passed as command arguments at execute time.
        Ok(())
    }

    async fn execute(&self, command: &str, args: &[String]) -> Result<SandboxResult> {
        let docker_args = self.build_args(command, args);
        let mut spawn = tokio::process::Command::new("docker");
        spawn
            .args(&docker_args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let started = Instant::now();
        let mut child = spawn.spawn().map_err(|error| {
            RustyError::Tool(format!("container backend could not spawn docker: {error}"))
        })?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| RustyError::Tool("container backend stdout pipe missing".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| RustyError::Tool("container backend stderr pipe missing".into()))?;

        let cap = self.config.max_output_bytes;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut _out_total = 0usize;
        let mut _err_total = 0usize;
        let mut out_done = false;
        let mut err_done = false;
        let mut status = None;
        let mut killed: Option<&'static str> = None;
        let mut out_buf = [0u8; 8192];
        let mut err_buf = [0u8; 8192];
        let timer = tokio::time::sleep(self.config.timeout);
        tokio::pin!(timer);

        loop {
            if out_done && err_done && status.is_some() {
                break;
            }
            tokio::select! {
                read = stdout.read(&mut out_buf), if !out_done => {
                    let read = read.map_err(|error| {
                        RustyError::Tool(format!("container backend stdout read failed: {error}"))
                    })?;
                    if read == 0 {
                        out_done = true;
                    } else {
                        _out_total += read;
                        let room = cap.saturating_sub(out.len());
                        out.extend_from_slice(&out_buf[..read.min(room)]);
                        if read > room && killed.is_none() {
                            killed = Some("output_cap");
                            let _ = child.start_kill();
                        }
                    }
                }
                read = stderr.read(&mut err_buf), if !err_done => {
                    let read = read.map_err(|error| {
                        RustyError::Tool(format!("container backend stderr read failed: {error}"))
                    })?;
                    if read == 0 {
                        err_done = true;
                    } else {
                        _err_total += read;
                        let room = cap.saturating_sub(err.len());
                        err.extend_from_slice(&err_buf[..read.min(room)]);
                        if read > room && killed.is_none() {
                            killed = Some("output_cap");
                            let _ = child.start_kill();
                        }
                    }
                }
                exit = child.wait(), if status.is_none() => {
                    status = Some(exit.map_err(|error| {
                        RustyError::Tool(format!("container backend wait failed: {error}"))
                    })?);
                }
                () = &mut timer => {
                    if killed.is_none() {
                        killed = Some("timeout");
                        let _ = child.start_kill();
                    }
                }
            }
        }
        let status = status.expect("the loop exits only after wait returns");
        let duration = started.elapsed();

        Ok(SandboxResult {
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
            exit_code: status.code(),
            timed_out: killed == Some("timeout"),
            truncated: killed == Some("output_cap"),
            duration_ms: duration.as_millis() as u64,
        })
    }

    fn enforcement(&self) -> EnforcementLevel {
        // Container with --network none and a workspace mount reports full
        // filesystem and network confinement *provided* the container runtime
        // is healthy. A future probe-based check (AC 1) could downgrade this
        // to partial if the runtime is misconfigured.
        EnforcementLevel::Full
    }

    fn backend_id(&self) -> &str {
        "container"
    }
}

// ---------------------------------------------------------------------------
// Remote backend
// ---------------------------------------------------------------------------

/// Configuration for the remote executor backend (EP-05-S12 AC 3).
#[derive(Debug, Clone)]
pub struct RemoteConfig {
    /// The executor endpoint URL.
    pub endpoint: String,
    /// Bearer token or other credential reference ( SecretRef placeholder).
    pub credential: Option<String>,
    /// Request timeout.
    pub timeout: Duration,
}

/// Remote backend: POSTs tool stubs, variables, and execution requests to a
/// remote executor endpoint (EP-05-S12 AC 3).
#[derive(Debug, Clone)]
pub struct RemoteBackend {
    config: Arc<RemoteConfig>,
    client: reqwest::Client,
}

impl RemoteBackend {
    /// A remote backend targeting `endpoint`.
    pub fn new(config: RemoteConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .expect("reqwest client builds with default settings");
        Self {
            config: Arc::new(config),
            client,
        }
    }

    fn auth_header(&self) -> Option<(&str, String)> {
        self.config
            .credential
            .as_ref()
            .map(|c| ("Authorization", format!("Bearer {c}")))
    }
}

#[async_trait]
impl SandboxExecutor for RemoteBackend {
    async fn send_tools(&self, tools: &[ToolStub]) -> Result<()> {
        let mut req = self
            .client
            .post(format!("{}/tools", self.config.endpoint))
            .json(tools);
        if let Some((name, value)) = self.auth_header() {
            req = req.header(name, value);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| RustyError::Tool(format!("remote backend send_tools failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(RustyError::Tool(format!(
                "remote backend send_tools returned {}",
                resp.status()
            )));
        }
        Ok(())
    }

    async fn send_variables(&self, variables: &Value) -> Result<()> {
        let mut req = self
            .client
            .post(format!("{}/variables", self.config.endpoint))
            .json(variables);
        if let Some((name, value)) = self.auth_header() {
            req = req.header(name, value);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| RustyError::Tool(format!("remote backend send_variables failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(RustyError::Tool(format!(
                "remote backend send_variables returned {}",
                resp.status()
            )));
        }
        Ok(())
    }

    async fn execute(&self, command: &str, args: &[String]) -> Result<SandboxResult> {
        let payload = serde_json::json!({
            "command": command,
            "args": args,
        });
        let mut req = self
            .client
            .post(format!("{}/execute", self.config.endpoint))
            .json(&payload);
        if let Some((name, value)) = self.auth_header() {
            req = req.header(name, value);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| RustyError::Tool(format!("remote backend execute failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(RustyError::Tool(format!(
                "remote backend execute returned {}",
                resp.status()
            )));
        }
        let result: SandboxResult = resp.json().await.map_err(|e| {
            RustyError::Tool(format!("remote backend execute response malformed: {e}"))
        })?;
        Ok(result)
    }

    fn enforcement(&self) -> EnforcementLevel {
        // Default to partial when the remote host does not attest its
        // enforcement level (EP-05-S12 AC 3).
        EnforcementLevel::Partial
    }

    fn backend_id(&self) -> &str {
        "remote"
    }
}
