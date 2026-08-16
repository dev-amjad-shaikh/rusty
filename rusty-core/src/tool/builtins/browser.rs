//! Browser use behind a driver seam.
//!
//! Five tools — `browser_navigate`, `browser_read`, `browser_click`,
//! `browser_type`, `browser_screenshot` — speak to a
//! [`BrowserDriver`], never to a browser directly. Two drivers ship:
//!
//! - [`VirtualBrowserDriver`]: a deterministic in-memory fake over a
//!   scripted page map. Tests, offline flows, and conformance harnesses
//!   exercise the full tool surface without a browser process.
//! - [`CdpDriver`]: Chrome DevTools Protocol. The crate has no WebSocket
//!   client, so this ships the HTTP-only subset (`GET /json` target
//!   listing via [`CdpDriver::list_targets`]); every frame command returns
//!   an honest `Unsupported`-style error until a ws transport lands with a
//!   dependency decision.
//!
//! Containment lives in [`BrowserController`]: navigation is refused
//! unless the URL starts with a policy allowlist prefix, world-mutating
//! actions (click, type) are counted against a per-session ceiling, and
//! DOM text is byte-capped. Effect classes follow the R0.7 taxonomy:
//! navigate/read/screenshot are [`Effect::ReadOnly`]; click/type are
//! [`Effect::Compensatable`] — a guarded executor requires a registered
//! rollback handler for their kinds before dispatch.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{hex_encode, Tool};
use crate::error::{Result, RustyError};
use crate::record::Effect;

/// Default ceiling on world-mutating browser actions (click + type) per
/// controller session.
pub const DEFAULT_BROWSER_MAX_ACTIONS: usize = 50;
/// Hard cap on the action ceiling a policy may declare.
pub const MAX_BROWSER_ACTIONS: usize = 500;
/// Default DOM-text byte cap returned by `browser_read`.
pub const DEFAULT_DOM_TEXT_BYTES: usize = 256 * 1024;
/// Hard cap on the DOM-text ceiling a policy may declare.
pub const MAX_DOM_TEXT_BYTES: usize = 1024 * 1024;
/// Maximum URL length accepted by `browser_navigate`.
pub const MAX_BROWSER_URL_BYTES: usize = 2048;
/// Maximum selector length accepted by click/type.
pub const MAX_BROWSER_SELECTOR_BYTES: usize = 512;
/// Maximum text length accepted by `browser_type`.
pub const MAX_BROWSER_TYPE_BYTES: usize = 4096;
/// Maximum screenshot payload a controller will return.
pub const MAX_BROWSER_SCREENSHOT_BYTES: usize = 4 * 1024 * 1024;

/// The async seam between the browser tools and any browser transport.
///
/// Implementations own their session state (the current page) internally;
/// the tools layer policy on top through [`BrowserController`].
#[async_trait]
pub trait BrowserDriver: std::fmt::Debug + Send + Sync {
    /// Load `url`, returning the page title.
    async fn navigate(&self, url: &str) -> Result<String>;
    /// The current page's visible text content.
    async fn dom_text(&self) -> Result<String>;
    /// Click the element matching `selector`.
    async fn click(&self, selector: &str) -> Result<()>;
    /// Type `text` into the element matching `selector`.
    async fn type_text(&self, selector: &str, text: &str) -> Result<()>;
    /// A PNG (or driver-native image) encoding of the current viewport.
    async fn screenshot(&self) -> Result<Vec<u8>>;
    /// The URL of the page currently loaded, when any.
    fn current_url(&self) -> Option<String>;
}

/// The containment policy a [`BrowserController`] enforces on every call.
#[derive(Debug, Clone)]
pub struct BrowserPolicy {
    allowed_url_prefixes: Vec<String>,
    max_actions: usize,
    max_dom_bytes: usize,
}

impl BrowserPolicy {
    /// A policy allowing navigation only to URLs under `allowed_url_prefixes`.
    ///
    /// Prefixes are matched literally after trimming (`"https://docs.rs/"`
    /// admits `https://docs.rs/std` and refuses `https://evil.test/`). An
    /// empty allowlist is fail-closed: every navigation is refused.
    pub fn new(allowed_url_prefixes: impl IntoIterator<Item = impl Into<String>>) -> Result<Self> {
        let mut prefixes = Vec::new();
        for prefix in allowed_url_prefixes {
            let prefix: String = prefix.into();
            if prefix.is_empty()
                || prefix != prefix.trim()
                || prefix.len() > 256
                || !prefix.is_ascii()
            {
                return Err(RustyError::Tool(format!(
                    "browser URL prefix `{prefix}` must be trimmed, ASCII, and 1..=256 bytes"
                )));
            }
            prefixes.push(prefix);
        }
        Ok(Self {
            allowed_url_prefixes: prefixes,
            max_actions: DEFAULT_BROWSER_MAX_ACTIONS,
            max_dom_bytes: DEFAULT_DOM_TEXT_BYTES,
        })
    }

    /// Set the per-session ceiling on click + type actions.
    pub fn with_max_actions(mut self, max_actions: usize) -> Result<Self> {
        if max_actions == 0 || max_actions > MAX_BROWSER_ACTIONS {
            return Err(RustyError::Tool(format!(
                "browser action ceiling must be between 1 and {MAX_BROWSER_ACTIONS}"
            )));
        }
        self.max_actions = max_actions;
        Ok(self)
    }

    /// Set the DOM-text byte cap for `browser_read`.
    pub fn with_max_dom_bytes(mut self, max_dom_bytes: usize) -> Result<Self> {
        if max_dom_bytes == 0 || max_dom_bytes > MAX_DOM_TEXT_BYTES {
            return Err(RustyError::Tool(format!(
                "browser DOM cap must be between 1 byte and {MAX_DOM_TEXT_BYTES}"
            )));
        }
        self.max_dom_bytes = max_dom_bytes;
        Ok(self)
    }

    /// Whether `url` is admitted by the allowlist.
    fn admits(&self, url: &str) -> bool {
        self.allowed_url_prefixes
            .iter()
            .any(|prefix| url.starts_with(prefix.as_str()))
    }
}

/// Policy enforcement over a [`BrowserDriver`], shared by the five tools.
///
/// One controller is one browser session: the action ceiling counts clicks
/// and types across every tool built from it.
#[derive(Debug)]
pub struct BrowserController {
    driver: Arc<dyn BrowserDriver>,
    policy: BrowserPolicy,
    actions: Mutex<usize>,
}

impl BrowserController {
    /// A session over `driver` under `policy`.
    pub fn new(driver: Arc<dyn BrowserDriver>, policy: BrowserPolicy) -> Arc<Self> {
        Arc::new(Self {
            driver,
            policy,
            actions: Mutex::new(0),
        })
    }

    /// The policy this session enforces.
    pub fn policy(&self) -> &BrowserPolicy {
        &self.policy
    }

    /// The URL of the page currently loaded, when any.
    pub fn current_url(&self) -> Option<String> {
        self.driver.current_url()
    }

    fn check_url(&self, url: &str) -> Result<()> {
        if url.is_empty() || url.len() > MAX_BROWSER_URL_BYTES {
            return Err(RustyError::Tool(format!(
                "browser URLs must contain 1..={MAX_BROWSER_URL_BYTES} bytes"
            )));
        }
        if !self.policy.admits(url) {
            return Err(RustyError::Tool(format!(
                "browser_navigate refused `{url}`: not under the policy URL allowlist"
            )));
        }
        Ok(())
    }

    fn take_action(&self) -> Result<usize> {
        let mut actions = self.actions.lock().unwrap_or_else(|e| e.into_inner());
        if *actions >= self.policy.max_actions {
            return Err(RustyError::Tool(format!(
                "browser session action ceiling ({}) reached",
                self.policy.max_actions
            )));
        }
        *actions += 1;
        Ok(*actions)
    }

    fn check_selector(&self, selector: &str) -> Result<()> {
        if selector.is_empty() || selector.len() > MAX_BROWSER_SELECTOR_BYTES {
            return Err(RustyError::Tool(format!(
                "browser selectors must contain 1..={MAX_BROWSER_SELECTOR_BYTES} bytes"
            )));
        }
        Ok(())
    }

    async fn navigate(&self, url: &str) -> Result<Value> {
        self.check_url(url)?;
        let title = self.driver.navigate(url).await?;
        Ok(json!({"url": url, "title": title}))
    }

    async fn read(&self) -> Result<Value> {
        let text = self.driver.dom_text().await?;
        let url = self.driver.current_url();
        let truncated = text.len() > self.policy.max_dom_bytes;
        let bounded = if truncated {
            let mut end = self.policy.max_dom_bytes;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text[..end].to_owned()
        } else {
            text
        };
        Ok(json!({
            "url": url,
            "bytes": bounded.len(),
            "truncated": truncated,
            "text": bounded,
        }))
    }

    async fn click(&self, selector: &str) -> Result<Value> {
        self.check_selector(selector)?;
        self.take_action()?;
        self.driver.click(selector).await?;
        Ok(json!({
            "clicked": selector,
            "url": self.driver.current_url(),
        }))
    }

    async fn type_text(&self, selector: &str, text: &str) -> Result<Value> {
        self.check_selector(selector)?;
        if text.is_empty() || text.len() > MAX_BROWSER_TYPE_BYTES {
            return Err(RustyError::Tool(format!(
                "browser_type text must contain 1..={MAX_BROWSER_TYPE_BYTES} bytes"
            )));
        }
        self.take_action()?;
        self.driver.type_text(selector, text).await?;
        Ok(json!({"typed": text.len(), "selector": selector}))
    }

    async fn screenshot(&self) -> Result<Value> {
        let bytes = self.driver.screenshot().await?;
        if bytes.len() > MAX_BROWSER_SCREENSHOT_BYTES {
            return Err(RustyError::Tool(format!(
                "browser screenshot exceeds {MAX_BROWSER_SCREENSHOT_BYTES} bytes"
            )));
        }
        Ok(json!({
            "url": self.driver.current_url(),
            "bytes": bytes.len(),
            "data_hex": hex_encode(&bytes),
        }))
    }
}

macro_rules! browser_tool {
    ($tool:ident, $name:literal, $description:literal, $schema:expr, $effect:expr, $invoke:ident) => {
        #[derive(Debug, Clone)]
        pub struct $tool {
            controller: Arc<BrowserController>,
        }

        impl $tool {
            /// Build the tool over a shared session controller.
            pub fn new(controller: Arc<BrowserController>) -> Self {
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

fn required_arg<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| RustyError::Tool(format!("`{name}` must be a string")))
}

impl BrowserController {
    async fn invoke_navigate(&self, args: &Value) -> Result<Value> {
        self.navigate(required_arg(args, "url")?).await
    }

    async fn invoke_read(&self, _args: &Value) -> Result<Value> {
        self.read().await
    }

    async fn invoke_click(&self, args: &Value) -> Result<Value> {
        self.click(required_arg(args, "selector")?).await
    }

    async fn invoke_type(&self, args: &Value) -> Result<Value> {
        self.type_text(required_arg(args, "selector")?, required_arg(args, "text")?)
            .await
    }

    async fn invoke_screenshot(&self, _args: &Value) -> Result<Value> {
        self.screenshot().await
    }
}

browser_tool!(
    BrowserNavigateTool,
    "browser_navigate",
    "Navigate the browser session to an allowlisted URL and return the page title.",
    json!({
        "type": "object",
        "properties": {"url": {"type": "string", "minLength": 1, "maxLength": MAX_BROWSER_URL_BYTES}},
        "required": ["url"],
        "additionalProperties": false
    }),
    Effect::ReadOnly,
    invoke_navigate
);

browser_tool!(
    BrowserReadTool,
    "browser_read",
    "Read the current page's visible text, capped by policy.",
    json!({"type": "object", "additionalProperties": false}),
    Effect::ReadOnly,
    invoke_read
);

browser_tool!(
    BrowserClickTool,
    "browser_click",
    "Click the element matching a selector (counts against the session action ceiling).",
    json!({
        "type": "object",
        "properties": {"selector": {"type": "string", "minLength": 1, "maxLength": MAX_BROWSER_SELECTOR_BYTES}},
        "required": ["selector"],
        "additionalProperties": false
    }),
    Effect::Compensatable,
    invoke_click
);

browser_tool!(
    BrowserTypeTool,
    "browser_type",
    "Type text into the element matching a selector (counts against the session action ceiling).",
    json!({
        "type": "object",
        "properties": {
            "selector": {"type": "string", "minLength": 1, "maxLength": MAX_BROWSER_SELECTOR_BYTES},
            "text": {"type": "string", "minLength": 1, "maxLength": MAX_BROWSER_TYPE_BYTES}
        },
        "required": ["selector", "text"],
        "additionalProperties": false
    }),
    Effect::Compensatable,
    invoke_type
);

browser_tool!(
    BrowserScreenshotTool,
    "browser_screenshot",
    "Capture the current viewport as byte-capped hex-encoded image data.",
    json!({"type": "object", "additionalProperties": false}),
    Effect::ReadOnly,
    invoke_screenshot
);

/// One scripted page for [`VirtualBrowserDriver`].
#[derive(Debug, Clone)]
pub struct VirtualPage {
    /// The page title returned on navigation.
    pub title: String,
    /// The visible text `browser_read` returns.
    pub dom_text: String,
    /// Selector → URL navigation targets for clicks.
    pub links: BTreeMap<String, String>,
    /// Selectors that accept typed input.
    pub inputs: BTreeSet<String>,
    /// The bytes `browser_screenshot` returns.
    pub screenshot: Vec<u8>,
}

impl VirtualPage {
    /// A page with a title and visible text, no interactive elements.
    pub fn new(title: impl Into<String>, dom_text: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            dom_text: dom_text.into(),
            links: BTreeMap::new(),
            inputs: BTreeSet::new(),
            screenshot: VIRTUAL_SCREENSHOT.to_vec(),
        }
    }

    /// Register a click target: clicking `selector` navigates to `url`.
    pub fn with_link(mut self, selector: impl Into<String>, url: impl Into<String>) -> Self {
        self.links.insert(selector.into(), url.into());
        self
    }

    /// Register a typable element.
    pub fn with_input(mut self, selector: impl Into<String>) -> Self {
        self.inputs.insert(selector.into());
        self
    }

    /// Set the screenshot payload.
    pub fn with_screenshot(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.screenshot = bytes.into();
        self
    }
}

/// A deterministic 1×1 transparent PNG used as the default virtual
/// screenshot payload.
pub const VIRTUAL_SCREENSHOT: [u8; 8] = *b"RUSTYPX!";

#[derive(Debug, Default)]
struct VirtualState {
    pages: BTreeMap<String, VirtualPage>,
    current: Option<String>,
    typed: Vec<(String, String)>,
}

/// A deterministic in-memory [`BrowserDriver`] over a scripted page map.
///
/// Navigation succeeds only for scripted URLs; clicks follow scripted
/// links; typing succeeds only into scripted inputs. Everything else is an
/// explicit error — the fake refuses what the script does not declare,
/// which is exactly the contract tests and offline flows need.
#[derive(Debug)]
pub struct VirtualBrowserDriver {
    state: Mutex<VirtualState>,
}

impl VirtualBrowserDriver {
    /// A virtual browser over `pages` (URL → page), initially on no page.
    pub fn new(pages: impl IntoIterator<Item = (impl Into<String>, VirtualPage)>) -> Self {
        Self {
            state: Mutex::new(VirtualState {
                pages: pages
                    .into_iter()
                    .map(|(url, page)| (url.into(), page))
                    .collect(),
                ..VirtualState::default()
            }),
        }
    }

    /// Every `(selector, text)` typed this session, in order. Test evidence.
    pub fn typed_inputs(&self) -> Vec<(String, String)> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .typed
            .clone()
    }

    fn current_page(state: &VirtualState) -> Result<&VirtualPage> {
        let url = state.current.as_deref().ok_or_else(|| {
            RustyError::Tool("virtual browser has no page loaded; navigate first".into())
        })?;
        state
            .pages
            .get(url)
            .ok_or_else(|| RustyError::Tool(format!("virtual browser lost scripted page `{url}`")))
    }
}

#[async_trait]
impl BrowserDriver for VirtualBrowserDriver {
    async fn navigate(&self, url: &str) -> Result<String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let page = state.pages.get(url).ok_or_else(|| {
            RustyError::Tool(format!("virtual browser has no page scripted for `{url}`"))
        })?;
        let title = page.title.clone();
        state.current = Some(url.to_owned());
        Ok(title)
    }

    async fn dom_text(&self) -> Result<String> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Ok(Self::current_page(&state)?.dom_text.clone())
    }

    async fn click(&self, selector: &str) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let target = Self::current_page(&state)?
            .links
            .get(selector)
            .cloned()
            .ok_or_else(|| {
                RustyError::Tool(format!(
                    "virtual browser has no clickable element matching `{selector}`"
                ))
            })?;
        state.current = Some(target);
        Ok(())
    }

    async fn type_text(&self, selector: &str, text: &str) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !Self::current_page(&state)?.inputs.contains(selector) {
            return Err(RustyError::Tool(format!(
                "virtual browser has no input matching `{selector}`"
            )));
        }
        state.typed.push((selector.to_owned(), text.to_owned()));
        Ok(())
    }

    async fn screenshot(&self) -> Result<Vec<u8>> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Ok(Self::current_page(&state)?.screenshot.clone())
    }

    fn current_url(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .current
            .clone()
    }
}

/// Chrome DevTools Protocol over the HTTP-only subset.
///
/// `GET {endpoint}/json` (target listing) works today through
/// [`CdpDriver::list_targets`]. Every frame command — navigation, DOM
/// reads, input, screenshots — travels over the CDP WebSocket transport,
/// and this crate deliberately has no ws client: the trait methods return
/// an honest unsupported error until the transport lands with a
/// dependency decision.
#[derive(Debug, Clone)]
pub struct CdpDriver {
    endpoint: String,
    http: reqwest::Client,
}

impl CdpDriver {
    /// A driver for the CDP HTTP endpoint (`http://127.0.0.1:9222`).
    pub fn new(endpoint: impl Into<String>) -> Result<Self> {
        let endpoint: String = endpoint.into();
        let endpoint = endpoint.trim_end_matches('/').to_owned();
        if !(endpoint.starts_with("http://") || endpoint.starts_with("https://"))
            || endpoint.len() > 256
        {
            return Err(RustyError::Tool(format!(
                "cdp endpoint `{endpoint}` must be an http(s):// URL of at most 256 bytes"
            )));
        }
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|error| {
                RustyError::Tool(format!("cdp HTTP client failed to build: {error}"))
            })?;
        Ok(Self { endpoint, http })
    }

    /// `GET /json`: the live target list of the connected browser. This is
    /// the entire working surface until the WebSocket transport lands.
    pub async fn list_targets(&self) -> Result<Value> {
        let response = self
            .http
            .get(format!("{}/json", self.endpoint))
            .send()
            .await
            .map_err(|error| RustyError::Tool(format!("cdp target listing failed: {error}")))?;
        if !response.status().is_success() {
            return Err(RustyError::Tool(format!(
                "cdp target listing returned {}",
                response.status()
            )));
        }
        response
            .json::<Value>()
            .await
            .map_err(|error| RustyError::Tool(format!("cdp target listing was not JSON: {error}")))
    }

    fn unsupported(operation: &str) -> RustyError {
        RustyError::Tool(format!(
            "cdp `{operation}` is unsupported: frame commands need the CDP WebSocket transport, \
             which is deferred until a ws client dependency decision lands; the HTTP-only subset \
             exposes target listing via CdpDriver::list_targets"
        ))
    }
}

#[async_trait]
impl BrowserDriver for CdpDriver {
    async fn navigate(&self, _url: &str) -> Result<String> {
        Err(Self::unsupported("navigate"))
    }

    async fn dom_text(&self) -> Result<String> {
        Err(Self::unsupported("dom_text"))
    }

    async fn click(&self, _selector: &str) -> Result<()> {
        Err(Self::unsupported("click"))
    }

    async fn type_text(&self, _selector: &str, _text: &str) -> Result<()> {
        Err(Self::unsupported("type_text"))
    }

    async fn screenshot(&self) -> Result<Vec<u8>> {
        Err(Self::unsupported("screenshot"))
    }

    fn current_url(&self) -> Option<String> {
        None
    }
}
