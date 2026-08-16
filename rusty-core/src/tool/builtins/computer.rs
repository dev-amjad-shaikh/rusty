//! Desktop computer use behind a driver seam.
//!
//! Three tools — `computer_screenshot`, `computer_click`, `computer_type`
//! — speak to a [`ComputerDriver`], never to the desktop directly. Two
//! drivers ship:
//!
//! - [`MacOsComputerDriver`] (macOS only, `std::process` only): screenshots
//!   via `/usr/sbin/screencapture -x` into a jailed temp path, click/type
//!   via `osascript` System Events. Requires macOS accessibility and
//!   screen-recording permissions at the OS level; the runtime neither
//!   grants nor checks those — it fails on the OS error.
//! - [`NullComputerDriver`]: a headless fake for tests — scripted
//!   screenshot bytes, logged clicks and keystrokes.
//!
//! Containment is deliberately stacked:
//!
//! - **Interaction is disabled by default.** [`ComputerPolicy`] starts with
//!   `interaction_enabled: false`; click/type refuse until the embedder
//!   opts in with [`ComputerPolicy::with_interaction`]. A policy refusal
//!   fires inside the tool body, so not even a valid approval token admits
//!   a disabled interaction.
//! - **Every mutating call is irreversible.** `computer_click` and
//!   `computer_type` declare [`Effect::NonIdempotent`]: a guarded executor
//!   requires a one-shot [`crate::effects::ApprovalToken`] per occurrence.
//!   `computer_screenshot` is [`Effect::ReadOnly`].
//! - **Bounds, rate limits, and byte caps.** Coordinates are validated
//!   against embedder-declared screen bounds, interactions are rate-limited
//!   (default 250ms between actions), and screenshot payloads are
//!   byte-capped.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{hex_encode, Tool};
use crate::error::{Result, RustyError};
use crate::record::Effect;

/// Default screenshot byte cap.
pub const DEFAULT_SCREENSHOT_BYTES: usize = 8 * 1024 * 1024;
/// Hard cap on the screenshot ceiling a policy may declare.
pub const MAX_SCREENSHOT_BYTES: usize = 32 * 1024 * 1024;
/// Maximum text length accepted by `computer_type`.
pub const MAX_COMPUTER_TYPE_BYTES: usize = 4096;
/// Default minimum interval between interactions.
pub const DEFAULT_COMPUTER_ACTION_INTERVAL: Duration = Duration::from_millis(250);

/// The async seam between the computer-use tools and any desktop backend.
#[async_trait]
pub trait ComputerDriver: std::fmt::Debug + Send + Sync {
    /// A PNG encoding of the screen.
    async fn screenshot(&self) -> Result<Vec<u8>>;
    /// Click at display coordinates `(x, y)`.
    async fn click(&self, x: i32, y: i32) -> Result<()>;
    /// Type `text` as keystrokes.
    async fn type_text(&self, text: &str) -> Result<()>;
}

/// The containment policy a [`ComputerController`] enforces.
///
/// Construction canonicalizes the temp jail used for screenshot captures.
/// Interaction starts **disabled**: the embedder opts in explicitly.
#[derive(Debug, Clone)]
pub struct ComputerPolicy {
    temp_root: PathBuf,
    interaction_enabled: bool,
    screen_bounds: Option<(u32, u32)>,
    max_screenshot_bytes: usize,
    min_interval: Duration,
}

impl ComputerPolicy {
    /// A policy with interaction disabled, screenshots captured under the
    /// existing directory `temp_root`, and no declared screen bounds
    /// (clicks are refused until bounds are declared).
    pub fn new(temp_root: impl AsRef<Path>) -> Result<Self> {
        let temp_root = std::fs::canonicalize(temp_root.as_ref()).map_err(|error| {
            RustyError::Tool(format!("computer temp root could not be opened: {error}"))
        })?;
        if !temp_root.is_dir() {
            return Err(RustyError::Tool(
                "computer temp root must be a directory".into(),
            ));
        }
        Ok(Self {
            temp_root,
            interaction_enabled: false,
            screen_bounds: None,
            max_screenshot_bytes: DEFAULT_SCREENSHOT_BYTES,
            min_interval: DEFAULT_COMPUTER_ACTION_INTERVAL,
        })
    }

    /// Opt into click/type. Off by default; enabling is the embedder's
    /// explicit decision, and each call still needs an approval token at
    /// a guarded executor.
    pub fn with_interaction(mut self, enabled: bool) -> Self {
        self.interaction_enabled = enabled;
        self
    }

    /// Declare the display size; click coordinates are validated against
    /// it. Without declared bounds every click is refused.
    pub fn with_screen_bounds(mut self, width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(RustyError::Tool(
                "computer screen bounds must be positive".into(),
            ));
        }
        self.screen_bounds = Some((width, height));
        Ok(self)
    }

    /// Set the screenshot byte cap.
    pub fn with_max_screenshot_bytes(mut self, bytes: usize) -> Result<Self> {
        if bytes == 0 || bytes > MAX_SCREENSHOT_BYTES {
            return Err(RustyError::Tool(format!(
                "computer screenshot cap must be between 1 byte and {MAX_SCREENSHOT_BYTES}"
            )));
        }
        self.max_screenshot_bytes = bytes;
        Ok(self)
    }

    /// Set the minimum interval between interactions (max 60s).
    pub fn with_min_interval(mut self, interval: Duration) -> Result<Self> {
        if interval > Duration::from_secs(60) {
            return Err(RustyError::Tool(
                "computer action interval must be at most 60s".into(),
            ));
        }
        self.min_interval = interval;
        Ok(self)
    }

    /// The jailed temp root screenshot captures are written under.
    pub fn temp_root(&self) -> &Path {
        &self.temp_root
    }

    /// The screenshot byte cap.
    pub fn max_screenshot_bytes(&self) -> usize {
        self.max_screenshot_bytes
    }
}

/// Policy enforcement over a [`ComputerDriver`], shared by the three tools.
#[derive(Debug)]
pub struct ComputerController {
    driver: Arc<dyn ComputerDriver>,
    policy: ComputerPolicy,
    last_action: Mutex<Option<Instant>>,
}

impl ComputerController {
    /// A session over `driver` under `policy`.
    pub fn new(driver: Arc<dyn ComputerDriver>, policy: ComputerPolicy) -> Arc<Self> {
        Arc::new(Self {
            driver,
            policy,
            last_action: Mutex::new(None),
        })
    }

    /// The policy this session enforces.
    pub fn policy(&self) -> &ComputerPolicy {
        &self.policy
    }

    fn check_interaction(&self) -> Result<()> {
        if !self.policy.interaction_enabled {
            return Err(RustyError::Tool(
                "computer interaction is disabled by policy; the embedder must opt in with \
                 ComputerPolicy::with_interaction(true)"
                    .into(),
            ));
        }
        Ok(())
    }

    fn rate_limit(&self) -> Result<()> {
        let mut last = self.last_action.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(previous) = *last {
            let elapsed = previous.elapsed();
            if elapsed < self.policy.min_interval {
                return Err(RustyError::Tool(format!(
                    "computer interaction rate limit: {}ms minimum between actions, {}ms elapsed",
                    self.policy.min_interval.as_millis(),
                    elapsed.as_millis()
                )));
            }
        }
        *last = Some(Instant::now());
        Ok(())
    }

    async fn screenshot(&self) -> Result<Value> {
        let bytes = self.driver.screenshot().await?;
        if bytes.len() > self.policy.max_screenshot_bytes {
            return Err(RustyError::Tool(format!(
                "computer screenshot exceeds the {} byte cap",
                self.policy.max_screenshot_bytes
            )));
        }
        Ok(json!({
            "bytes": bytes.len(),
            "data_hex": hex_encode(&bytes),
        }))
    }

    async fn click(&self, x: i32, y: i32) -> Result<Value> {
        self.check_interaction()?;
        let (width, height) = self.policy.screen_bounds.ok_or_else(|| {
            RustyError::Tool(
                "computer_click refused: the policy declares no screen bounds to validate against"
                    .into(),
            )
        })?;
        if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
            return Err(RustyError::Tool(format!(
                "computer_click ({x}, {y}) is outside the declared {width}x{height} bounds"
            )));
        }
        self.rate_limit()?;
        self.driver.click(x, y).await?;
        Ok(json!({"clicked": [x, y]}))
    }

    async fn type_text(&self, text: &str) -> Result<Value> {
        self.check_interaction()?;
        if text.is_empty() || text.len() > MAX_COMPUTER_TYPE_BYTES {
            return Err(RustyError::Tool(format!(
                "computer_type text must contain 1..={MAX_COMPUTER_TYPE_BYTES} bytes"
            )));
        }
        self.rate_limit()?;
        self.driver.type_text(text).await?;
        Ok(json!({"typed": text.len()}))
    }

    async fn invoke_screenshot(&self, _args: &Value) -> Result<Value> {
        self.screenshot().await
    }

    async fn invoke_click(&self, args: &Value) -> Result<Value> {
        let coordinate = |name: &str| -> Result<i32> {
            let value = args
                .get(name)
                .and_then(Value::as_i64)
                .ok_or_else(|| RustyError::Tool(format!("`{name}` must be an integer")))?;
            i32::try_from(value)
                .map_err(|_| RustyError::Tool(format!("`{name}` is outside the i32 range")))
        };
        self.click(coordinate("x")?, coordinate("y")?).await
    }

    async fn invoke_type(&self, args: &Value) -> Result<Value> {
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| RustyError::Tool("`text` must be a string".into()))?;
        self.type_text(text).await
    }
}

macro_rules! computer_tool {
    ($tool:ident, $name:literal, $description:literal, $schema:expr, $effect:expr, $invoke:ident) => {
        #[derive(Debug, Clone)]
        pub struct $tool {
            controller: Arc<ComputerController>,
        }

        impl $tool {
            /// Build the tool over a shared session controller.
            pub fn new(controller: Arc<ComputerController>) -> Self {
                Self { controller }
            }
        }

        #[async_trait]
        impl Tool for $tool {
            fn name(&self) -> &str {
                $name
            }

            fn description(&self) -> &str {
                $description
            }

            fn parameters_schema(&self) -> Value {
                $schema
            }

            fn effect(&self) -> Effect {
                $effect
            }

            async fn call(&self, args: Value) -> Result<Value> {
                self.controller.$invoke(&args).await
            }
        }
    };
}

computer_tool!(
    ComputerScreenshotTool,
    "computer_screenshot",
    "Capture the screen as byte-capped hex-encoded PNG data.",
    json!({"type": "object", "additionalProperties": false}),
    Effect::ReadOnly,
    invoke_screenshot
);

computer_tool!(
    ComputerClickTool,
    "computer_click",
    "Click at display coordinates (disabled unless the policy opts in; approval-gated).",
    json!({
        "type": "object",
        "properties": {
            "x": {"type": "integer", "minimum": 0},
            "y": {"type": "integer", "minimum": 0}
        },
        "required": ["x", "y"],
        "additionalProperties": false
    }),
    Effect::NonIdempotent,
    invoke_click
);

computer_tool!(
    ComputerTypeTool,
    "computer_type",
    "Type text as keystrokes (disabled unless the policy opts in; approval-gated).",
    json!({
        "type": "object",
        "properties": {"text": {"type": "string", "minLength": 1, "maxLength": MAX_COMPUTER_TYPE_BYTES}},
        "required": ["text"],
        "additionalProperties": false
    }),
    Effect::NonIdempotent,
    invoke_type
);

/// A headless [`ComputerDriver`] for tests: scripted screenshot bytes and
/// an ordered log of every click and keystroke.
#[derive(Debug)]
pub struct NullComputerDriver {
    screenshot_bytes: Vec<u8>,
    log: Mutex<Vec<String>>,
}

impl NullComputerDriver {
    /// A null driver whose screenshots return `screenshot_bytes`.
    pub fn new(screenshot_bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            screenshot_bytes: screenshot_bytes.into(),
            log: Mutex::new(Vec::new()),
        }
    }

    /// Every interaction, in order (`click:x,y` / `type:<text>`).
    pub fn interaction_log(&self) -> Vec<String> {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[async_trait]
impl ComputerDriver for NullComputerDriver {
    async fn screenshot(&self) -> Result<Vec<u8>> {
        Ok(self.screenshot_bytes.clone())
    }

    async fn click(&self, x: i32, y: i32) -> Result<()> {
        self.log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("click:{x},{y}"));
        Ok(())
    }

    async fn type_text(&self, text: &str) -> Result<()> {
        self.log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("type:{text}"));
        Ok(())
    }
}

/// macOS desktop driver using only `std::process` facilities:
/// `/usr/sbin/screencapture -x` for screenshots and `osascript` System
/// Events for click/type.
///
/// The driver performs no permission brokering: macOS decides whether the
/// hosting process may capture the screen or post events, and a denial
/// surfaces as the OS error. Screenshot captures land as files inside the
/// policy's jailed temp root and are removed after reading.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone)]
pub struct MacOsComputerDriver {
    temp_root: PathBuf,
    max_screenshot_bytes: usize,
}

#[cfg(target_os = "macos")]
impl MacOsComputerDriver {
    const SCREENCAPTURE: &'static str = "/usr/sbin/screencapture";
    const OSASCRIPT: &'static str = "/usr/bin/osascript";

    /// A driver rooted at the (canonical) temp jail from the policy.
    pub fn new(policy: &ComputerPolicy) -> Result<Self> {
        if !Path::new(Self::SCREENCAPTURE).is_file() {
            return Err(RustyError::Tool(
                "macOS computer driver requires /usr/sbin/screencapture".into(),
            ));
        }
        if !Path::new(Self::OSASCRIPT).is_file() {
            return Err(RustyError::Tool(
                "macOS computer driver requires /usr/bin/osascript".into(),
            ));
        }
        Ok(Self {
            temp_root: policy.temp_root().to_owned(),
            max_screenshot_bytes: policy.max_screenshot_bytes(),
        })
    }

    async fn run(program: &str, args: &[String]) -> Result<()> {
        let output = tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|error| {
                RustyError::Tool(format!(
                    "computer driver could not run `{program}`: {error}"
                ))
            })?;
        if !output.status.success() {
            let mut detail = String::from_utf8_lossy(&output.stderr).into_owned();
            detail.truncate(512);
            return Err(RustyError::Tool(format!(
                "computer driver `{program}` exited with {}: {detail}",
                output.status
            )));
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
#[async_trait]
impl ComputerDriver for MacOsComputerDriver {
    async fn screenshot(&self) -> Result<Vec<u8>> {
        let path = self
            .temp_root
            .join(format!("rusty-screenshot-{}.png", uuid::Uuid::new_v4()));
        let path_display = path.display().to_string();
        let capture = Self::run(
            Self::SCREENCAPTURE,
            &["-x".to_owned(), path_display.clone()],
        )
        .await;
        if let Err(error) = capture {
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        let metadata = std::fs::metadata(&path).map_err(|error| {
            RustyError::Tool(format!("screenshot capture metadata failed: {error}"))
        })?;
        if metadata.len() > self.max_screenshot_bytes as u64 {
            let _ = std::fs::remove_file(&path);
            return Err(RustyError::Tool(format!(
                "screenshot capture exceeds the {} byte cap",
                self.max_screenshot_bytes
            )));
        }
        let bytes = std::fs::read(&path).map_err(|error| {
            RustyError::Tool(format!("screenshot capture read failed: {error}"))
        })?;
        let _ = std::fs::remove_file(&path);
        if bytes.len() > self.max_screenshot_bytes {
            return Err(RustyError::Tool(format!(
                "screenshot capture exceeds the {} byte cap",
                self.max_screenshot_bytes
            )));
        }
        Ok(bytes)
    }

    async fn click(&self, x: i32, y: i32) -> Result<()> {
        Self::run(
            Self::OSASCRIPT,
            &[
                "-e".to_owned(),
                format!("tell application \"System Events\" to click at {{{x}, {y}}}"),
            ],
        )
        .await
    }

    async fn type_text(&self, text: &str) -> Result<()> {
        // The text crosses into an AppleScript string literal: escape the
        // two characters that could break out of it.
        let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
        Self::run(
            Self::OSASCRIPT,
            &[
                "-e".to_owned(),
                format!("tell application \"System Events\" to keystroke \"{escaped}\""),
            ],
        )
        .await
    }
}
