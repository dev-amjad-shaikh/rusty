//! Connector operations as agent tools.
//!
//! A connector manifest already declares everything a tool needs: a name, a
//! description, an arguments schema, an effect class, a path template and its
//! auth. The manifest's own comment says as much — *one catalog tool per
//! operation* — but nothing built the bridge, so an agent could not call a
//! connector at all. This is that bridge.
//!
//! Nothing about the request is re-implemented here. Rendering goes through
//! [`render_operation_request`], which resolves the templates and auth, keeps
//! the URL https-only and refuses header injection; sending goes through the
//! same [`ConnectorTransport`] the connector check uses. What this adds is the
//! [`Tool`] contract: the schema the model sees, the effect class the runtime
//! admits on, and the arguments-to-template binding.
//!
//! Read operations only, for now. [`CheckRequest`] carries no body, so a write
//! cannot be expressed through this seam; [`ConnectorMethodTool::for_manifest`]
//! skips the manifest's check operation (a gate, not an action) rather than
//! pretending a write is available.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use super::check::{render_operation_request, ConnectorTransport};
use super::config::validate_config;
use super::conn_err;
use super::manifest::{ConnectorManifest, ConnectorOperation, HttpMethod};
use crate::error::Result;
use crate::record::Effect;
use crate::tool::ToolFailure;
use crate::tool::{EffectClass, SandboxRequirement, Tool};

/// Mints and refreshes an OAuth access token for a connector instance.
///
/// The token endpoint takes a form-encoded POST, which the read-only check
/// transport cannot express, so minting is its own seam. Implementations live
/// where the HTTP client does; core states the contract.
#[async_trait]
pub trait OAuthTokenSource: std::fmt::Debug + Send + Sync {
    /// Obtain an access token and the seconds it remains valid.
    async fn token(&self, config: &Value) -> Result<(String, u64)>;
}

/// A token held until shortly before it expires.
#[derive(Debug)]
struct TokenCache {
    source: Arc<dyn OAuthTokenSource>,
    held: Mutex<Option<(String, Instant)>>,
}

impl TokenCache {
    /// Refresh a minute early: a token that expires mid-flight fails the call
    /// for a reason that has nothing to do with the request.
    const EARLY: Duration = Duration::from_secs(60);

    async fn token(&self, config: &Value) -> Result<String> {
        if let Some((token, expires_at)) = self.held.lock().unwrap().clone() {
            if Instant::now() < expires_at {
                return Ok(token);
            }
        }
        let (token, expires_in) = self.source.token(config).await?;
        let lifetime = Duration::from_secs(expires_in).saturating_sub(Self::EARLY);
        *self.held.lock().unwrap() = Some((token.clone(), Instant::now() + lifetime));
        Ok(token)
    }
}

/// One connector operation, callable by an agent.
#[derive(Debug, Clone)]
pub struct ConnectorMethodTool {
    tool_name: String,
    manifest: Arc<ConnectorManifest>,
    operation: ConnectorOperation,
    /// The instance's configuration — the subdomain, and the resolved
    /// credential material the manifest's auth alternatives read. It is used
    /// for rendering and never returned to the model.
    config: Arc<Value>,
    transport: Arc<dyn ConnectorTransport>,
    /// Present when the instance authenticates by OAuth: the token is minted
    /// on first use and refreshed before it expires, so a long-lived agent
    /// does not start failing thirty minutes in.
    oauth: Option<Arc<TokenCache>>,
}

impl ConnectorMethodTool {
    /// The answer to a write was lost after it was sent. Instead of leaving
    /// the model to guess, run the operation's declared read-back with the
    /// write's own arguments: found — the write happened once, answer with
    /// what was found and say so; not found — one re-send is safe; the
    /// read-back itself failing leaves the outcome unknown as before.
    async fn reconcile_lost_answer(&self, args: &Value, detail: &str) -> Result<Value> {
        let read_back = self
            .operation
            .reconcile
            .as_ref()
            .expect("checked by the caller");
        let unknown = || {
            transport_failure(
                &self.tool_name,
                true,
                crate::error::RustyError::Transport {
                    sent: true,
                    detail: detail.to_owned(),
                },
            )
        };
        let reader = match Self::try_new(
            Arc::clone(&self.manifest),
            &read_back.operation,
            Arc::clone(&self.config),
            Arc::clone(&self.transport),
        ) {
            Ok(reader) => Self {
                oauth: self.oauth.clone(),
                ..reader
            },
            Err(_) => return Err(unknown().into_error()),
        };
        let arguments = render_read_back(&read_back.arguments, args);
        match reader.call(arguments.clone()).await {
            Ok(found) if !looks_empty(&found) => Ok(json!({
                "reconciled": true,
                "sent": true,
                "read_back": {"tool": reader.tool_name, "arguments": arguments},
                "found": found,
                "note": "the answer to this write was lost after it was sent; the record was read back and found — it happened exactly once. Answer from what was found; do not send the write again.",
            })),
            Ok(_) => Err(ToolFailure::new(
                "unknown_outcome",
                &self.tool_name,
                format!("the request was sent and the answer was lost ({detail}); the read-back `{}` found no record", reader.tool_name),
                true,
                true,
                "the record is not there: send the write once more",
            )
            .into_error()),
            Err(_) => Err(unknown().into_error()),
        }
    }

    /// Authenticate this tool's calls with OAuth, minting through `source`.
    pub fn with_oauth(mut self, source: Arc<dyn OAuthTokenSource>) -> Self {
        self.oauth = Some(Arc::new(TokenCache {
            source,
            held: Mutex::new(None),
        }));
        self
    }
    /// The tool name an agent calls: `<connector-id>.<operation>`.
    pub fn tool_name(connector_id: &str, operation: &str) -> String {
        format!("{connector_id}.{operation}")
    }

    /// Bridge one named operation.
    ///
    /// The config must validate against the manifest's own
    /// `connection_specification` — the same check the instantiation route
    /// applies. A connector configured in a shape it does not declare is
    /// configurable from nowhere but the code that wrote it, so this refuses
    /// rather than letting a private arrangement work.
    ///
    /// `None` when the manifest has no such operation, when it is not a read
    /// (a write has no seam yet), or when the config does not validate.
    pub fn new(
        manifest: Arc<ConnectorManifest>,
        operation_name: &str,
        config: Arc<Value>,
        transport: Arc<dyn ConnectorTransport>,
    ) -> Option<Self> {
        Self::try_new(manifest, operation_name, config, transport).ok()
    }

    /// As [`ConnectorMethodTool::new`], reporting why the bridge was refused.
    pub fn try_new(
        manifest: Arc<ConnectorManifest>,
        operation_name: &str,
        config: Arc<Value>,
        transport: Arc<dyn ConnectorTransport>,
    ) -> Result<Self> {
        validate_config(&manifest.connection_specification, &config).map_err(|why| {
            conn_err(format!(
                "the configuration for `{}` does not match its connection specification: {why}",
                manifest.id
            ))
        })?;
        let operation = manifest.operation(operation_name).cloned().ok_or_else(|| {
            conn_err(format!(
                "connector `{}` declares no operation `{operation_name}`",
                manifest.id
            ))
        })?;
        // The check is the setup gate — it proves a configuration and returns
        // nothing an agent would reason over — so it is not a tool. Every
        // other operation is, reads and writes alike: what governs a write is
        // the effect it declares, admitted by the executor, not a refusal
        // here.
        if operation.name == manifest.check {
            return Err(conn_err(format!(
                "operation `{operation_name}` is connector `{}`'s check — a gate, not an action",
                manifest.id
            )));
        }
        Ok(Self {
            tool_name: Self::tool_name(&manifest.id, &operation.name),
            manifest,
            operation,
            config,
            transport,
            oauth: None,
        })
    }

    /// The same tool under another name. A second connection to the same
    /// connector derives the same operations; naming one of them with its
    /// instance keeps the two distinct in a registry where a name is an
    /// identity.
    pub fn renamed(mut self, name: String) -> Self {
        self.tool_name = name;
        self
    }

    /// Every read operation a manifest offers, as tools.
    pub fn for_manifest(
        manifest: Arc<ConnectorManifest>,
        config: Arc<Value>,
        transport: Arc<dyn ConnectorTransport>,
    ) -> Vec<Self> {
        manifest
            .operations
            .iter()
            .filter_map(|op| {
                Self::new(
                    Arc::clone(&manifest),
                    &op.name,
                    Arc::clone(&config),
                    Arc::clone(&transport),
                )
            })
            .collect()
    }

    /// The rendering context: the instance's config with the call's arguments
    /// laid over it, so `{table}` in a path template resolves from what the
    /// model supplied. Arguments never overwrite credential material — the
    /// config wins on any key it already holds, so a model cannot redirect a
    /// call by naming `instance` or `password` as an argument.
    fn render_context(&self, args: &Value) -> Value {
        let mut ctx = Map::new();
        if let Some(supplied) = args.as_object() {
            for (k, v) in supplied {
                ctx.insert(k.clone(), v.clone());
            }
        }
        if let Some(config) = self.config.as_object() {
            for (k, v) in config {
                ctx.insert(k.clone(), v.clone());
            }
        }
        Value::Object(ctx)
    }

    /// Arguments the path template did not consume, as the JSON body of a
    /// write. Keys sorted, so the same call renders the same bytes — the
    /// journal hashes them.
    fn body_json(&self, args: &Value) -> Vec<u8> {
        let mut body = serde_json::Map::new();
        if let Some(supplied) = args.as_object() {
            for (k, v) in supplied {
                if !self.operation.path.contains(&format!("{{{k}}}")) {
                    body.insert(k.clone(), v.clone());
                }
            }
        }
        serde_json::to_vec(&Value::Object(body)).expect("a JSON object always serializes")
    }

    /// Arguments the path template did not consume become query parameters,
    /// in a stable order so the same call renders the same URL.
    fn query_string(&self, args: &Value) -> String {
        let Some(supplied) = args.as_object() else {
            return String::new();
        };
        let mut pairs: Vec<(String, String)> = supplied
            .iter()
            .filter(|(k, _)| !self.operation.path.contains(&format!("{{{k}}}")))
            .filter_map(|(k, v)| {
                let rendered = match v {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    // A list of scalars is one parameter with comma-joined
                    // values — how a forecast names its fields, a search its
                    // columns, a filter its states. A list holding objects
                    // or an object itself is a caller error, not guessed at.
                    Value::Array(items) => {
                        let mut parts = Vec::with_capacity(items.len());
                        for item in items {
                            match item {
                                Value::String(s) => parts.push(s.clone()),
                                Value::Number(n) => parts.push(n.to_string()),
                                Value::Bool(b) => parts.push(b.to_string()),
                                _ => return None,
                            }
                        }
                        parts.join(",")
                    }
                    _ => return None,
                };
                Some((k.clone(), rendered))
            })
            .collect();
        pairs.sort();
        pairs
            .iter()
            .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
            .collect::<Vec<_>>()
            .join("&")
    }
}

/// Percent-encode everything outside the unreserved set, so a query value
/// cannot end the parameter or the URL.
fn urlencode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[async_trait]
impl Tool for ConnectorMethodTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        &self.operation.description
    }

    fn parameters_schema(&self) -> Value {
        self.operation.params_schema.clone()
    }

    /// The effect the manifest declared for this operation, in the kernel's
    /// terms. This is what admission reads: a compensatable write is admitted
    /// with its compensation registered; an irreversible one is refused until
    /// an approval token names it.
    fn effect(&self) -> Effect {
        self.operation.effect.wire_effect()
    }

    /// The call leaves the process for a declared host, so it is egress and
    /// the egress policy decides whether it may go.
    fn effect_class(&self) -> EffectClass {
        EffectClass::Egress
    }

    fn sandbox_requirement(&self) -> SandboxRequirement {
        SandboxRequirement::Required
    }

    fn effect_kind(&self) -> &str {
        &self.tool_name
    }

    async fn call(&self, args: Value) -> Result<Value> {
        let mut context = self.render_context(&args);
        // An OAuth instance carries no standing secret in its config: the
        // token is minted here, held only in memory, and never rendered into
        // anything but the Authorization header.
        if let Some(oauth) = &self.oauth {
            let token = oauth.token(&self.config).await?;
            if let Some(obj) = context.as_object_mut() {
                let credentials = obj.entry("credentials").or_insert_with(|| json!({}));
                if let Some(creds) = credentials.as_object_mut() {
                    creds.insert("token".to_owned(), Value::String(token));
                }
            }
        }
        let mut request = render_operation_request(&self.manifest, &self.operation, &context)?;

        // Arguments the path did not consume travel as the query string on a
        // read, and as a JSON body on a write — which is where every REST
        // API this connects to expects them.
        match self.operation.method {
            HttpMethod::Get | HttpMethod::Delete => {
                let query = self.query_string(&args);
                if !query.is_empty() {
                    let separator = if request.url.contains('?') { '&' } else { '?' };
                    request.url = format!("{}{separator}{query}", request.url);
                }
            }
            HttpMethod::Post | HttpMethod::Put | HttpMethod::Patch => {
                request.body = Some(self.body_json(&args));
            }
        }

        let is_write = !matches!(self.operation.method, HttpMethod::Get);
        let response = match self.transport.send(request).await {
            Ok(response) => response,
            Err(crate::error::RustyError::Transport { sent: true, detail })
                if is_write && self.operation.reconcile.is_some() =>
            {
                return self.reconcile_lost_answer(&args, &detail).await;
            }
            Err(error) => {
                return Err(transport_failure(&self.tool_name, is_write, error).into_error())
            }
        };
        let body = String::from_utf8_lossy(&response.body).to_string();

        if !(200..300).contains(&response.status) {
            return Err(
                status_failure(&self.tool_name, response.status, &body, is_write).into_error(),
            );
        }

        Ok(serde_json::from_str(&body).unwrap_or_else(|_| json!({ "body": body })))
    }
}

/// What the system said, from a JSON error body when it has one (the
/// `error.message` + `error.detail` shape ServiceNow and many others use, a
/// bare `message` or `error`), else the first 200 characters.
fn said(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        let pick = |v: &Value| {
            v.as_str()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        };
        let nested = value.get("error").and_then(|e| {
            let message = e.get("message").and_then(pick);
            let detail = e.get("detail").and_then(pick);
            match (message, detail) {
                (Some(m), Some(d)) if d != m => Some(format!("{m} — {d}")),
                (Some(m), _) => Some(m),
                (None, Some(d)) => Some(d),
                _ => None,
            }
        });
        if let Some(text) = nested
            .or_else(|| value.get("message").and_then(pick))
            .or_else(|| value.get("error").and_then(pick))
        {
            return text.chars().take(300).collect();
        }
    }
    body.chars().take(200).collect()
}

/// The failure a non-2xx answer is: what the loop may do next depends on
/// the class, and for a write on whether it may have happened.
pub fn status_failure(tool: &str, status: u16, body: &str, is_write: bool) -> ToolFailure {
    let s = said(body);
    match status {
        400 | 422 => ToolFailure::new("invalid_arguments", tool, format!("HTTP {status}: {s}"), true, false, "fix the arguments as the system says, then call again"),
        // An auth refusal can echo the credential's neighborhood back, so
        // the status travels without the body.
        401 | 403 => ToolFailure::new("denied", tool, format!("HTTP {status}: the system refused this credential or action"), true, false, "do not retry; this credential or policy does not allow it — say so, or ask for the authority"),
        404 => ToolFailure::new("not_found", tool, format!("HTTP 404: {s}"), true, false, "what was named does not exist here (a table, a record, a path); check it, then call again"),
        408 => ToolFailure::new("transient", tool, format!("HTTP 408: {s}"), false, true, "the system did not read the request in time; call again once"),
        409 => ToolFailure::new("conflict", tool, format!("HTTP 409: {s}"), true, false, "the record changed or already exists; read it back before deciding"),
        429 => ToolFailure::new("rate_limited", tool, format!("HTTP 429: {s}"), true, false, "wait a moment, then call again once"),
        500..=599 if is_write => ToolFailure::new("dependency", tool, format!("HTTP {status}: {s}"), true, false, "the system failed on its side; read the record back before sending again"),
        500..=599 => ToolFailure::new("dependency", tool, format!("HTTP {status}: {s}"), true, true, "the system failed on its side; call again once"),
        _ => ToolFailure::new("unexpected", tool, format!("HTTP {status}: {s}"), true, false, "an answer this connector does not expect; say what it said"),
    }
}

/// The failure a wire error is: nothing sent is transient; a read whose
/// answer was lost is safe to repeat; a write whose answer was lost may
/// have happened and must be read back before it is sent again.
pub fn transport_failure(
    tool: &str,
    is_write: bool,
    error: crate::error::RustyError,
) -> ToolFailure {
    match error {
        crate::error::RustyError::Transport {
            sent: false,
            detail,
        } => ToolFailure::new(
            "transient",
            tool,
            detail,
            false,
            true,
            "nothing reached the system; call again once",
        ),
        crate::error::RustyError::Transport { sent: true, detail } if is_write => ToolFailure::new(
            "unknown_outcome",
            tool,
            format!("the request was sent and the answer was lost: {detail}"),
            true,
            false,
            "do not send it again — read the record back first to learn whether it happened",
        ),
        crate::error::RustyError::Transport { sent: true, detail } => ToolFailure::new(
            "transient",
            tool,
            format!("the answer was lost: {detail}"),
            true,
            true,
            "a read is safe to repeat; call again once",
        ),
        crate::error::RustyError::Tool(message)
            if message.contains("egress denied")
                || message.contains("egress ceiling")
                || message.contains("no endpoint policy") =>
        {
            ToolFailure::new(
                "denied",
                tool,
                message,
                false,
                false,
                "do not retry; this deployment's policy does not allow reaching that host — say so",
            )
        }
        crate::error::RustyError::Tool(message)
            if message.contains("DNS") || message.contains("resolution") =>
        {
            ToolFailure::new(
                "transient",
                tool,
                message,
                false,
                true,
                "the host did not resolve; nothing was sent — call again once, then say so",
            )
        }
        other => ToolFailure::new(
            "unexpected",
            tool,
            other.to_string(),
            false,
            false,
            "say what happened",
        ),
    }
}

/// The read-back's arguments from its template: every string that is
/// exactly `$name` becomes the write's argument `name` (whatever its type);
/// `$name` inside a longer string is replaced by that argument's text.
pub fn render_read_back(template: &Value, args: &Value) -> Value {
    match template {
        Value::String(s) => {
            if let Some(name) = s
                .strip_prefix('$')
                .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_alphanumeric() || c == '_'))
            {
                return args
                    .get(name)
                    .cloned()
                    .unwrap_or(Value::String(String::new()));
            }
            let mut out = s.clone();
            if let Some(map) = args.as_object() {
                for (key, value) in map {
                    let text = match value {
                        Value::String(t) => t.clone(),
                        other => other.to_string(),
                    };
                    out = out.replace(&format!("${key}"), &text);
                }
            }
            Value::String(out)
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| render_read_back(v, args)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), render_read_back(v, args)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Whether a read-back answered nothing: an empty array or object, or an
/// object whose only list (`result`, `items`, `records`, `data`, `value`)
/// is empty.
pub fn looks_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => {
            if map.is_empty() {
                return true;
            }
            for key in ["result", "items", "records", "data", "value", "results"] {
                if let Some(inner) = map.get(key) {
                    return looks_empty(inner);
                }
            }
            false
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::check::{CheckRequest, CheckResponse};
    use std::sync::Mutex;

    /// A transport that records what it was asked to send and replies with a
    /// canned body — the seam the check tests already use, reused here.
    #[derive(Debug, Default)]
    struct Recorder {
        seen: Mutex<Vec<String>>,
        status: u16,
        body: String,
    }

    #[async_trait]
    impl ConnectorTransport for Recorder {
        async fn send(&self, request: CheckRequest) -> Result<CheckResponse> {
            self.seen.lock().unwrap().push(request.url.clone());
            Ok(CheckResponse {
                status: self.status,
                body: self.body.clone().into_bytes(),
            })
        }
    }

    fn manifest_json() -> Value {
        json!({
            "id": "servicenow",
            "version": "1.0.0",
            "display_name": "ServiceNow",
            "description": "Table API",
            "documentation_url": "https://docs.example.com",
            "hash": "",
            "base_url": "https://{instance}.service-now.com",
            "check": "check-connection",
            "connection_specification": {
                "type": "object",
                "required": ["instance"],
                "additionalProperties": false,
                "properties": {
                    "instance": {"type": "string"},
                    "credentials": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {"token": {"type": "string", "rusty_secret": true}}
                    }
                }
            },
            "operations": [{
                "name": "list-records",
                "description": "List records in a table.",
                "method": "GET",
                "path": "/api/now/table/{table}",
                "effect": "read_only",
                "params_schema": {
                    "type": "object",
                    "required": ["table"],
                    "properties": {
                        "table": {"type": "string"},
                        "sysparm_query": {"type": "string"},
                        "sysparm_limit": {"type": "integer"}
                    }
                }
            }, {
                "name": "create-incident",
                "description": "Create an incident.",
                "method": "POST",
                "path": "/api/now/table/incident",
                "effect": "irreversible",
                "params_schema": {"type": "object", "properties": {}}
            }, {
                "name": "check-connection",
                "description": "Read one sys_user row.",
                "method": "GET",
                "path": "/api/now/table/sys_user?sysparm_limit=1",
                "effect": "read_only",
                "params_schema": {"type": "object"}
            }]
        })
    }

    fn tool(recorder: Arc<Recorder>) -> ConnectorMethodTool {
        let manifest: ConnectorManifest = serde_json::from_value(manifest_json()).unwrap();
        ConnectorMethodTool::new(
            Arc::new(manifest),
            "list-records",
            Arc::new(json!({"instance": "dev00001"})),
            recorder,
        )
        .expect("a read operation bridges")
    }

    #[test]
    fn a_config_the_connector_does_not_declare_is_refused() {
        let manifest: ConnectorManifest = serde_json::from_value(manifest_json()).unwrap();
        let manifest = Arc::new(manifest);
        let transport: Arc<dyn ConnectorTransport> = Arc::new(Recorder::default());

        // A shape invented by a caller rather than declared by the connector.
        let invented = Arc::new(json!({"credentials": {"oauth": {"client_id": "c"}}}));
        let refused = ConnectorMethodTool::try_new(
            Arc::clone(&manifest),
            "list-records",
            invented,
            Arc::clone(&transport),
        )
        .expect_err("an undeclared shape must not configure a connector");
        assert!(
            refused.to_string().contains("connection specification"),
            "the refusal must say what it failed against: {refused}"
        );

        // The declared shape configures it.
        assert!(ConnectorMethodTool::try_new(
            manifest,
            "list-records",
            Arc::new(json!({"instance": "dev00001"})),
            transport
        )
        .is_ok());
    }

    #[test]
    fn the_check_is_not_bridged_but_a_write_is() {
        let manifest: ConnectorManifest = serde_json::from_value(manifest_json()).unwrap();
        let manifest = Arc::new(manifest);
        let transport: Arc<dyn ConnectorTransport> = Arc::new(Recorder::default());
        let config = Arc::new(json!({"instance": "dev00001"}));

        // The check is the setup gate, not an action an agent takes.
        let refused = ConnectorMethodTool::try_new(
            Arc::clone(&manifest),
            "check-connection",
            Arc::clone(&config),
            Arc::clone(&transport),
        )
        .expect_err("the check is a gate");
        assert!(refused.to_string().contains("gate"), "{refused}");

        // A write is a tool, carrying the effect the manifest declared — which
        // is what admission reads. Irreversible here, so the executor will
        // refuse it without an approval token; that refusal is the seam.
        let write = ConnectorMethodTool::try_new(manifest, "create-incident", config, transport)
            .expect("a write bridges");
        assert_eq!(write.name(), "servicenow.create-incident");
        assert_eq!(write.effect(), Effect::NonIdempotent);
    }

    #[test]
    fn a_write_carries_its_arguments_as_a_json_body() {
        let manifest: ConnectorManifest = serde_json::from_value(manifest_json()).unwrap();
        let transport: Arc<dyn ConnectorTransport> = Arc::new(Recorder::default());
        let write = ConnectorMethodTool::try_new(
            Arc::new(manifest),
            "create-incident",
            Arc::new(json!({"instance": "dev00001"})),
            transport,
        )
        .unwrap();
        let body = write.body_json(&json!({"short_description": "Printer down", "urgency": "2"}));
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"short_description": "Printer down", "urgency": "2"})
        );
    }

    #[test]
    fn the_tool_advertises_the_operation_and_its_effect() {
        let t = tool(Arc::new(Recorder::default()));
        assert_eq!(t.description(), "List records in a table.");
        assert_eq!(t.effect(), Effect::ReadOnly);
        assert_eq!(t.effect_class(), EffectClass::Egress);
        assert_eq!(t.parameters_schema()["required"], json!(["table"]));
    }

    #[test]
    fn a_list_of_scalars_is_one_comma_joined_query_parameter() {
        let manifest: ConnectorManifest = serde_json::from_value(manifest_json()).unwrap();
        let transport: Arc<dyn ConnectorTransport> = Arc::new(Recorder::default());
        let read = ConnectorMethodTool::try_new(
            Arc::new(manifest),
            "list-records",
            Arc::new(json!({"instance": "dev00001"})),
            transport,
        )
        .unwrap();
        let query = read.query_string(&serde_json::json!({
            "daily": ["temperature_2m_max", "precipitation_sum"],
            "forecast_days": 2,
            "nested": [{"no": 1}],
            "flags": [true, false]
        }));
        assert_eq!(
            query,
            "daily=temperature_2m_max%2Cprecipitation_sum&flags=true%2Cfalse&forecast_days=2"
        );
    }

    #[tokio::test]
    async fn arguments_fill_the_path_and_the_query() {
        let recorder = Arc::new(Recorder {
            status: 200,
            body: r#"{"result":[{"number":"INC0001"}]}"#.to_owned(),
            ..Default::default()
        });
        let t = tool(Arc::clone(&recorder));
        let out = t
            .call(json!({"table": "incident", "sysparm_limit": 2, "sysparm_query": "active=true"}))
            .await
            .expect("the call succeeds");

        let url = recorder.seen.lock().unwrap()[0].clone();
        assert!(url.starts_with("https://dev00001.service-now.com/api/now/table/incident"));
        // The path consumed `table`; the rest became query parameters, encoded.
        assert!(url.contains("sysparm_limit=2"), "{url}");
        assert!(url.contains("sysparm_query=active%3Dtrue"), "{url}");
        assert!(!url.contains("table="), "the path consumed it: {url}");
        assert_eq!(out["result"][0]["number"], "INC0001");
    }

    #[tokio::test]
    async fn a_model_cannot_redirect_the_call_by_naming_config_keys() {
        let recorder = Arc::new(Recorder {
            status: 200,
            body: "{}".to_owned(),
            ..Default::default()
        });
        let t = tool(Arc::clone(&recorder));
        t.call(json!({"table": "incident", "instance": "attacker"}))
            .await
            .expect("the call succeeds");
        let url = recorder.seen.lock().unwrap()[0].clone();
        assert!(
            url.starts_with("https://dev00001.service-now.com/"),
            "{url}"
        );
    }

    /// A token source that counts how often it was asked, so the cache can be
    /// shown to hold rather than mint per call.
    #[derive(Debug, Default)]
    struct CountingTokens {
        minted: Mutex<u32>,
        expires_in: u64,
    }

    #[async_trait]
    impl OAuthTokenSource for CountingTokens {
        async fn token(&self, _config: &Value) -> Result<(String, u64)> {
            let mut n = self.minted.lock().unwrap();
            *n += 1;
            Ok((format!("token-{n}"), self.expires_in))
        }
    }

    #[tokio::test]
    async fn an_oauth_instance_mints_a_token_and_reuses_it() {
        let recorder = Arc::new(Recorder {
            status: 200,
            body: "{}".to_owned(),
            ..Default::default()
        });
        let tokens = Arc::new(CountingTokens {
            expires_in: 1800,
            ..Default::default()
        });
        let t = tool(Arc::clone(&recorder))
            .with_oauth(Arc::clone(&tokens) as Arc<dyn OAuthTokenSource>);

        t.call(json!({"table": "incident"})).await.unwrap();
        t.call(json!({"table": "problem"})).await.unwrap();

        // Two calls, one mint: the token is held until it is near expiry.
        assert_eq!(*tokens.minted.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn a_token_at_the_end_of_its_life_is_minted_again() {
        let recorder = Arc::new(Recorder {
            status: 200,
            body: "{}".to_owned(),
            ..Default::default()
        });
        // Shorter than the early-refresh margin, so it is always due.
        let tokens = Arc::new(CountingTokens {
            expires_in: 10,
            ..Default::default()
        });
        let t = tool(Arc::clone(&recorder))
            .with_oauth(Arc::clone(&tokens) as Arc<dyn OAuthTokenSource>);

        t.call(json!({"table": "incident"})).await.unwrap();
        t.call(json!({"table": "incident"})).await.unwrap();
        assert_eq!(*tokens.minted.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn a_refused_credential_does_not_echo_the_body_back() {
        let recorder = Arc::new(Recorder {
            status: 401,
            body: "Bearer abc123 was rejected".to_owned(),
            ..Default::default()
        });
        let t = tool(recorder);
        let err = t
            .call(json!({"table": "incident"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("401"), "{err}");
        assert!(
            !err.contains("abc123"),
            "the refusal must not quote the credential: {err}"
        );
    }

    /// A transport whose writes lose their answer after being sent, and
    /// whose reads answer what they are told to.
    #[derive(Debug)]
    struct LostWrite {
        rows: String,
        seen: std::sync::Mutex<Vec<String>>,
    }
    #[async_trait]
    impl ConnectorTransport for LostWrite {
        async fn send(&self, request: CheckRequest) -> Result<CheckResponse> {
            self.seen
                .lock()
                .unwrap()
                .push(format!("{:?} {}", request.method, request.url));
            match request.method {
                HttpMethod::Get => Ok(CheckResponse {
                    status: 200,
                    body: self.rows.clone().into_bytes(),
                }),
                _ => Err(crate::error::RustyError::Transport {
                    sent: true,
                    detail: "connection reset after the request was written".to_owned(),
                }),
            }
        }
    }

    fn manifest_with_read_back() -> Arc<ConnectorManifest> {
        let mut manifest = manifest_json();
        manifest["operations"] = json!([
            {"name": "check-connection", "description": "check", "method": "GET", "path": "/api/now/table/sys_user?sysparm_limit=1", "effect": "read_only", "params_schema": {"type": "object"}, "auth": []},
            {"name": "list-records", "description": "list", "method": "GET", "path": "/api/now/table/{table}", "effect": "read_only",
             "params_schema": {"type": "object", "properties": {"table": {"type": "string"}, "sysparm_query": {"type": "string"}, "sysparm_limit": {"type": "integer"}}}, "auth": []},
            {"name": "create-incident", "description": "create", "method": "POST", "path": "/api/now/table/incident", "effect": "compensatable",
             "params_schema": {"type": "object", "properties": {"short_description": {"type": "string"}}}, "auth": [],
             "reconcile": {"operation": "list-records", "arguments": {"table": "incident", "sysparm_query": "short_description=$short_description", "sysparm_limit": 3}}}
        ]);
        Arc::new(serde_json::from_value(manifest).expect("a manifest with a read-back"))
    }

    #[tokio::test]
    async fn a_write_whose_answer_was_lost_reads_itself_back_and_answers_with_what_it_found() {
        let transport = Arc::new(LostWrite { rows: r#"{"result": [{"number": "INC0010777", "short_description": "printer on 3 jams"}]}"#.to_owned(), seen: Default::default() });
        let tool = ConnectorMethodTool::new(
            manifest_with_read_back(),
            "create-incident",
            Arc::new(json!({"instance": "dev1"})),
            transport.clone(),
        )
        .unwrap();
        let answer = tool
            .call(json!({"short_description": "printer on 3 jams"}))
            .await
            .expect("reconciled, not failed");
        assert_eq!(answer["reconciled"], json!(true));
        assert_eq!(
            answer["found"]["result"][0]["number"],
            json!("INC0010777"),
            "{answer}"
        );
        assert_eq!(
            answer["read_back"]["tool"],
            json!("servicenow.list-records")
        );
        assert_eq!(
            answer["read_back"]["arguments"]["sysparm_query"],
            json!("short_description=printer on 3 jams")
        );
        let seen = transport.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "the write, then the read-back: {seen:?}");
        assert!(seen[1].contains("/api/now/table/incident"), "{seen:?}");
    }

    #[tokio::test]
    async fn a_write_whose_answer_was_lost_and_whose_read_back_found_nothing_may_be_sent_once_more()
    {
        let transport = Arc::new(LostWrite {
            rows: r#"{"result": []}"#.to_owned(),
            seen: Default::default(),
        });
        let tool = ConnectorMethodTool::new(
            manifest_with_read_back(),
            "create-incident",
            Arc::new(json!({"instance": "dev1"})),
            transport,
        )
        .unwrap();
        let error = tool
            .call(json!({"short_description": "printer on 3 jams"}))
            .await
            .expect_err("unknown, but safe to retry");
        let failure = ToolFailure::parse(&error.to_string()).expect("a structured failure");
        assert_eq!(failure.class, "unknown_outcome");
        assert!(failure.retry_safe, "{failure:?}");
        assert!(failure.detail.contains("found no record"), "{failure:?}");
    }

    #[test]
    fn read_back_templates_take_the_writes_arguments() {
        let args = json!({"short_description": "a & b", "urgency": 2});
        let rendered = render_read_back(
            &json!({"q": "short_description=$short_description", "u": "$urgency", "n": 3, "list": ["$urgency"]}),
            &args,
        );
        assert_eq!(
            rendered,
            json!({"q": "short_description=a & b", "u": 2, "n": 3, "list": [2]})
        );
        assert!(looks_empty(&json!({"result": []})));
        assert!(!looks_empty(&json!({"result": [{"number": "INC1"}]})));
        assert!(
            looks_empty(&json!([]))
                && looks_empty(&json!({}))
                && !looks_empty(&json!({"number": "INC1"}))
        );
    }
}

#[cfg(test)]
mod failure_tests {
    use super::*;
    use crate::error::RustyError;

    #[test]
    fn a_status_is_a_failure_the_loop_can_act_on() {
        let f = status_failure(
            "servicenow.list-records",
            400,
            r#"{"error":{"message":"Invalid table incdent","detail":"Table incdent does not exist"},"status":"failure"}"#,
            false,
        );
        assert_eq!(f.class, "invalid_arguments");
        assert!(
            f.detail.contains("Invalid table incdent") && f.detail.contains("does not exist"),
            "{}",
            f.detail
        );
        assert!(!f.retry_safe);
        assert!(f.next.contains("fix the arguments"));
        let denied = status_failure("t", 401, "Bearer abc123 was rejected", false);
        assert_eq!(denied.class, "denied");
        assert!(
            !denied.detail.contains("abc123"),
            "the body never travels on an auth refusal"
        );
        assert_eq!(
            status_failure("t", 404, r#"{"message":"no such record"}"#, false).class,
            "not_found"
        );
        assert_eq!(status_failure("t", 429, "", false).class, "rate_limited");
        let read = status_failure("t", 503, "down", false);
        assert_eq!(read.class, "dependency");
        assert!(
            read.retry_safe,
            "a read may be repeated after the system's own failure"
        );
        let write = status_failure("t", 503, "down", true);
        assert_eq!(write.class, "dependency");
        assert!(
            !write.retry_safe && write.next.contains("read the record back"),
            "{}",
            write.next
        );
        assert_eq!(
            status_failure("t", 409, r#"{"error":"exists"}"#, true).class,
            "conflict"
        );
    }

    #[test]
    fn a_wire_failure_says_whether_anything_may_have_happened() {
        let nothing = transport_failure(
            "t",
            true,
            RustyError::Transport {
                sent: false,
                detail: "connection refused".into(),
            },
        );
        assert_eq!(nothing.class, "transient");
        assert!(nothing.retry_safe && !nothing.sent);
        let lost_read = transport_failure(
            "t",
            false,
            RustyError::Transport {
                sent: true,
                detail: "timed out".into(),
            },
        );
        assert_eq!(lost_read.class, "transient");
        assert!(lost_read.retry_safe && lost_read.sent);
        let lost_write = transport_failure(
            "t",
            true,
            RustyError::Transport {
                sent: true,
                detail: "timed out".into(),
            },
        );
        assert_eq!(lost_write.class, "unknown_outcome");
        assert!(!lost_write.retry_safe && lost_write.next.contains("read the record back"));
        let ceiling = transport_failure(
            "t",
            false,
            RustyError::Tool("egress denied: no endpoint policy for x".into()),
        );
        assert_eq!(ceiling.class, "denied");
        let dns = transport_failure(
            "t",
            false,
            RustyError::Tool("egress: DNS resolution failed for x".into()),
        );
        assert_eq!(dns.class, "transient");
        // The failure round-trips through the tool error channel.
        let err = lost_write.clone().into_error().to_string();
        let parsed = ToolFailure::parse(&format!("ERROR: {err}")).expect("parses back");
        assert_eq!(parsed, lost_write);
    }
}
