//! Operation execution: any declared operation as a bounded, schema-
//! checked call (EP-07-S05).
//!
//! [`execute_check`](super::check::execute_check) runs one operation —
//! the manifest's named `check` — and answers a verdict. This module is
//! the general case: execute *any* declared operation against a live
//! instance's config and return the parsed body, and mount one as an
//! ordinary tool ([`ConnectorOperationTool`]) so ingestion has no
//! privileged path — a connector call is schema-validated against the
//! operation's declared `params_schema`, guarded like any other tool
//! when the tool is mounted in a run, and its credentials stay sealed
//! until the host-side executor opens them for the single call.
//!
//! Rendering is shared with check (`render_operation_request`), so the
//! rules are identical: https only, CR/LF rejected, the first
//! fully-resolving auth alternative applies, and the byte ceiling is
//! enforced by the transport during the read. Call arguments overlay the
//! config in the render context with config keys winning collisions, so
//! a path template's `{window}` resolves from the call while a
//! credential placeholder can never be overwritten by an argument.

use async_trait::async_trait;
use serde_json::Value;

use super::check::{ConnectorTransport, render_operation_request};
use super::manifest::{ConnectorManifest, ConnectorOperation};
use super::{conn_err, validate_config};
use crate::error::Result;
use crate::llm::ToolCall;
use crate::record::Effect;
use crate::tool::{EffectClass, Tool};

/// The parsed reply of one executed operation.
#[derive(Debug, Clone, PartialEq)]
pub struct OperationResponse {
    /// The HTTP status code.
    pub status: u16,
    /// The response body parsed as JSON. A non-JSON body is a typed
    /// error at execution time — downstream consumers (ingestion)
    /// normalize records, and an unparseable body must not degrade into
    /// an empty corpus.
    pub body: Value,
    /// The body's byte length as received (pre-parse), for receipts.
    pub body_bytes: usize,
}

/// Execute one declared operation against a rendered config and return
/// the parsed body. Render and transport failures raise typed errors;
/// a non-2xx status raises with a sanitized excerpt (auth refusals echo
/// no body, the check discipline — an auth-failure page may quote the
/// credential's neighborhood back).
///
/// `params` are the call arguments: they validate against the
/// operation's `params_schema` before anything renders, and resolve
/// `{params.*}` placeholders only.
pub async fn execute_operation(
    manifest: &ConnectorManifest,
    operation: &ConnectorOperation,
    config: &Value,
    params: &Value,
    transport: &dyn ConnectorTransport,
) -> Result<OperationResponse> {
    if let Err(rejection) = validate_config(&operation.params_schema, params) {
        return Err(conn_err(format!(
            "operation `{}` arguments invalid: {rejection}",
            operation.name
        )));
    }
    let context = render_context(config, params);
    let request = render_operation_request(manifest, operation, &context)?;
    let response = transport.send(request).await?;
    if !(200..300).contains(&response.status) {
        if response.status == 401 || response.status == 403 {
            return Err(conn_err(format!(
                "operation `{}` was refused (HTTP {})",
                operation.name, response.status
            )));
        }
        let excerpt =
            super::check::sanitize_excerpt(&response.body, super::check::CHECK_ERROR_BODY_BYTES);
        return Err(conn_err(format!(
            "operation `{}` failed: HTTP {}{}",
            operation.name,
            response.status,
            if excerpt.is_empty() {
                String::new()
            } else {
                format!(": {excerpt}")
            }
        )));
    }
    let body_bytes = response.body.len();
    let body: Value = serde_json::from_slice(&response.body).map_err(|e| {
        conn_err(format!(
            "operation `{}` returned a non-JSON body ({body_bytes} bytes): {e}",
            operation.name
        ))
    })?;
    Ok(OperationResponse {
        status: response.status,
        body,
        body_bytes,
    })
}

/// The render context: the config overlaid with the call arguments.
/// Config keys win collisions — a call argument can shadow nothing the
/// config carries, secret or otherwise — so a manifest's `{window}`
/// resolves from the call while `{instance}` always renders the stored
/// instance.
fn render_context(config: &Value, params: &Value) -> Value {
    let mut context = config.clone();
    if let (Some(map), Some(params)) = (context.as_object_mut(), params.as_object()) {
        for (key, value) in params {
            if !map.contains_key(key) {
                map.insert(key.clone(), value.clone());
            }
        }
    }
    context
}

/// The host-side seam a [`ConnectorOperationTool`] drives: the caller
/// owns instance resolution, secret opening, and transport selection;
/// the tool owns schema validation and the effect declaration. One
/// executor is bound to one instance's operation at construction.
#[async_trait]
pub trait OperationExecutor: std::fmt::Debug + Send + Sync {
    /// Execute the bound operation with schema-validated arguments and
    /// return the parsed body.
    async fn execute(&self, params: Value) -> Result<Value>;
}

/// One connector operation mounted as an ordinary tool
/// (`<connector-id>/<operation>`): the ingestion path's guarantee that
/// a connector executes *as* a tool — arguments schema-validated against
/// the declared `params_schema`, effect classified from the manifest,
/// guarded through the ordinary pipeline when mounted in a run, and
/// journaled like any tool traffic.
///
/// `effect_class` is [`EffectClass::Egress`] — the call opens a network
/// connection. `sandbox_requirement` stays `None` because the network
/// boundary is enforced at the transport (the L7 egress policy and DNS
/// preflight run inside `ReqwestConnectorTransport` on every send), not
/// at a process sandbox the in-process caller may not have.
#[derive(Debug)]
pub struct ConnectorOperationTool {
    name: String,
    description: String,
    params_schema: Value,
    effect: Effect,
    executor: std::sync::Arc<dyn OperationExecutor>,
}

impl ConnectorOperationTool {
    /// Mount `operation` from `manifest` behind `executor`. The tool
    /// name is the catalog name (`<connector-id>/<operation>`).
    pub fn new(
        manifest: &ConnectorManifest,
        operation: &ConnectorOperation,
        executor: std::sync::Arc<dyn OperationExecutor>,
    ) -> Self {
        Self {
            name: format!("{}/{}", manifest.id, operation.name),
            description: operation.description.clone(),
            params_schema: operation.params_schema.clone(),
            effect: operation.effect.wire_effect(),
            executor,
        }
    }
}

#[async_trait]
impl Tool for ConnectorOperationTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        self.params_schema.clone()
    }

    fn effect(&self) -> Effect {
        self.effect
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::Egress
    }

    async fn call(&self, args: Value) -> Result<Value> {
        if let Err(rejection) = validate_config(&self.params_schema, &args) {
            return Err(conn_err(format!(
                "tool `{}` arguments invalid: {rejection}",
                self.name
            )));
        }
        self.executor.execute(args).await
    }

    fn effect_request(&self, call: &ToolCall) -> crate::effects::EffectRequest {
        // The default would do; spelling it out keeps the effect kind the
        // catalog name even if the wrapper is itself wrapped.
        crate::effects::EffectRequest::new(
            self.effect_kind(),
            self.effect(),
            &serde_json::json!({
                "arguments": &call.arguments,
                "tool_call_id": &call.id,
            }),
            self.idempotency_key(&call.arguments),
        )
    }
}
