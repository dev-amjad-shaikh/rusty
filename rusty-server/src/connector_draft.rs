//! A connector described by hand — a system with no OpenAPI document — as a
//! builder or the Composer describes it, and the manifest the server
//! registers from it. `docs/connector-standard.md` is the contract; this
//! only composes it, once, for the studio's form (`POST
//! /connectors/describe`) and the Composer's `connectors.register` alike.
//! Every operation becomes one tool, `{id}.{operation}`; a parameterless
//! `check` read is added so a connection can be tested before it is saved.

use axum::extract::State as AxumState;
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::sync::Arc;

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;
use rusty_agent_runtime::connector::ConnectorManifest;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthStyle {
    Bearer,
    Basic,
    Header,
    Query,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamDraft {
    pub name: String,
    /// `string`, `integer`, `number` or `boolean`.
    #[serde(default = "string_type")]
    pub r#type: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub description: String,
}

fn string_type() -> String {
    "string".to_owned()
}

/// A write's read-back, as a builder declares it in the by-hand form.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReadBackDraft {
    pub operation: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationDraft {
    pub name: String,
    pub description: String,
    /// `GET`, `POST`, `PUT`, `PATCH` or `DELETE`.
    pub method: String,
    pub path: String,
    /// `read_only`, `idempotent`, `compensatable` or `irreversible`.
    pub effect: String,
    /// For a write: the operation (by name, from this draft) that finds
    /// what the write made, and its arguments as a template over the
    /// write's (`"$short_description"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconcile: Option<ReadBackDraft>,
    #[serde(default)]
    pub params: Vec<ParamDraft>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorDraft {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub base_url: String,
    pub documentation_url: String,
    pub auth: AuthStyle,
    /// The header or query parameter name, for those two styles.
    #[serde(default)]
    pub auth_name: String,
    /// The parameterless read the server tests a connection with.
    #[serde(default = "root_path")]
    pub check_path: String,
    pub operations: Vec<OperationDraft>,
}

fn root_path() -> String {
    "/".to_owned()
}

pub fn kebab(text: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in text.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    out.trim_end_matches('-').to_owned()
}

/// What is still wrong, in the order a builder should fix it.
pub fn problems(d: &ConnectorDraft) -> Vec<String> {
    let mut out = Vec::new();
    if kebab(&d.name).is_empty() {
        out.push("Name the system.".to_owned());
    }
    if !d.base_url.trim().starts_with("https://") || d.base_url.trim().len() < 12 {
        out.push("The API root must start with https://.".to_owned());
    }
    if !(d.documentation_url.starts_with("https://") || d.documentation_url.starts_with("http://")) {
        out.push("Link to its documentation.".to_owned());
    }
    if matches!(d.auth, AuthStyle::Header | AuthStyle::Query) && d.auth_name.trim().is_empty() {
        out.push("Name the header or parameter the credential goes in.".to_owned());
    }
    if !d.check_path.trim().starts_with('/') {
        out.push("The check path starts with /.".to_owned());
    }
    if d.operations.is_empty() {
        out.push("Describe at least one operation.".to_owned());
    }
    let mut seen = std::collections::HashSet::new();
    for (i, op) in d.operations.iter().enumerate() {
        let label = if op.name.trim().is_empty() { format!("Operation {}", i + 1) } else { format!("Operation {}", op.name.trim()) };
        let id = kebab(&op.name);
        if id.is_empty() {
            out.push(format!("{label}: give it a name."));
        } else if !seen.insert(id.clone()) {
            out.push(format!("{label}: that name is used twice."));
        }
        if id == "check" {
            out.push(format!("{label}: `check` is the name of the test read the server adds."));
        }
        if op.description.trim().is_empty() {
            out.push(format!("{label}: say what it does — the model reads this to choose it."));
        }
        if !matches!(op.method.to_ascii_uppercase().as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE") {
            out.push(format!("{label}: the method is GET, POST, PUT, PATCH or DELETE."));
        }
        if !matches!(op.effect.as_str(), "read_only" | "idempotent" | "compensatable" | "irreversible") {
            out.push(format!("{label}: the effect is read_only, idempotent, compensatable or irreversible."));
        }
        if !op.path.trim().starts_with('/') {
            out.push(format!("{label}: the path starts with /."));
        }
        let mut rest = op.path.as_str();
        while let Some(start) = rest.find('{') {
            let Some(end) = rest[start..].find('}') else { break };
            let name = &rest[start + 1..start + end];
            if !op.params.iter().any(|p| p.name == name) {
                out.push(format!("{label}: {{{name}}} in the path needs a parameter of that name."));
            }
            rest = &rest[start + end + 1..];
        }
        for p in &op.params {
            let ok = !p.name.is_empty()
                && p.name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && p.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !ok {
                out.push(format!("{label}: “{}” is not a parameter name.", p.name));
            }
            if !matches!(p.r#type.as_str(), "string" | "integer" | "number" | "boolean") {
                out.push(format!("{label}: parameter {} has an unknown type.", p.name));
            }
        }
    }
    out
}

fn auth_for(d: &ConnectorDraft) -> Vec<Value> {
    match d.auth {
        AuthStyle::Bearer => vec![json!({"style": "bearer", "token": "{credentials.token}"})],
        AuthStyle::Basic => vec![json!({"style": "basic", "username": "{credentials.username}", "password": "{credentials.password}"})],
        AuthStyle::Header => vec![json!({"style": "header", "name": d.auth_name.trim(), "value_template": "{credentials.token}"})],
        AuthStyle::Query => vec![json!({"style": "query", "name": d.auth_name.trim(), "value_template": "{credentials.token}"})],
        AuthStyle::None => vec![],
    }
}

fn specification_for(d: &ConnectorDraft) -> Value {
    let credentials = match d.auth {
        AuthStyle::None => return json!({"type": "object", "properties": {}, "additionalProperties": false}),
        AuthStyle::Basic => json!({
            "username": {"type": "string", "description": "Username", "rusty_order": 1},
            "password": {"type": "string", "description": "Password", "rusty_secret": true, "rusty_order": 2}
        }),
        AuthStyle::Bearer => json!({"token": {"type": "string", "description": "API token (sent as a bearer token)", "rusty_secret": true}}),
        AuthStyle::Header | AuthStyle::Query => json!({"token": {"type": "string", "description": format!("Credential (sent as {} “{}”)", if d.auth == AuthStyle::Header { "header" } else { "query parameter" }, d.auth_name.trim()), "rusty_secret": true}}),
    };
    let required: Vec<String> = credentials.as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
    json!({
        "type": "object",
        "properties": {"credentials": {"type": "object", "properties": credentials, "required": required, "additionalProperties": false}},
        "required": ["credentials"],
        "additionalProperties": false
    })
}

/// The manifest the server registers, or the problems that stop it.
pub fn compose_manifest(d: &ConnectorDraft) -> Result<Value, Vec<String>> {
    let found = problems(d);
    if !found.is_empty() {
        return Err(found);
    }
    let auth = auth_for(d);
    let mut operations: Vec<Value> = d
        .operations
        .iter()
        .map(|op| {
            let mut properties = Map::new();
            for p in &op.params {
                let mut schema = Map::new();
                schema.insert("type".to_owned(), json!(p.r#type));
                if !p.description.trim().is_empty() {
                    schema.insert("description".to_owned(), json!(p.description.trim()));
                }
                properties.insert(p.name.clone(), Value::Object(schema));
            }
            let required: Vec<&str> = op.params.iter().filter(|p| p.required).map(|p| p.name.as_str()).collect();
            let mut params_schema = json!({"type": "object", "properties": properties});
            if !required.is_empty() {
                params_schema["required"] = json!(required);
            }
            let mut operation = json!({
                "name": kebab(&op.name),
                "description": op.description.trim(),
                "method": op.method.to_ascii_uppercase(),
                "path": op.path.trim(),
                "effect": op.effect,
                "params_schema": params_schema,
                "auth": auth,
            });
            if let Some(read_back) = op.reconcile.as_ref().filter(|r| !r.operation.trim().is_empty()) {
                operation["reconcile"] = json!({"operation": kebab(&read_back.operation), "arguments": read_back.arguments});
            }
            operation
        })
        .collect();
    operations.push(json!({
        "name": "check",
        "description": format!("Confirm the credentials reach {}.", d.name.trim()),
        "method": "GET",
        "path": d.check_path.trim(),
        "effect": "read_only",
        "params_schema": {"type": "object", "properties": {}},
        "auth": auth,
    }));
    let description = if d.description.trim().is_empty() {
        format!("{} over its API.", d.name.trim())
    } else {
        d.description.trim().to_owned()
    };
    Ok(json!({
        "id": kebab(&d.name),
        "version": "1",
        "display_name": d.name.trim(),
        "description": description,
        "documentation_url": d.documentation_url.trim(),
        "base_url": d.base_url.trim().trim_end_matches('/'),
        "connection_specification": specification_for(d),
        "operations": operations,
        "check": "check",
    }))
}

/// Compose, validate against the standard, seal (hash) and register. The
/// one path the studio's form and the Composer share.
pub(crate) async fn register_described(
    state: &AppState,
    tenant: &str,
    draft: &ConnectorDraft,
) -> Result<(ConnectorManifest, bool), String> {
    let value = compose_manifest(draft).map_err(|problems| problems.join(" "))?;
    let manifest: ConnectorManifest =
        serde_json::from_value(value).map_err(|e| format!("the composed manifest does not parse: {e}"))?;
    manifest.validate().map_err(|e| e.to_string())?;
    let manifest = manifest.sealed().map_err(|e| e.to_string())?;
    let registered = state
        .connectors
        .put_manifest(tenant, &manifest)
        .await
        .map_err(|e| format!("connector store: {e}"))?;
    Ok((manifest, registered))
}

/// `POST /connectors/describe` — a connector from a description, registered.
pub(crate) async fn describe(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(draft): Json<ConnectorDraft>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let (manifest, registered) = register_described(&state, tenant.tenant(), &draft)
        .await
        .map_err(ApiError::bad_request)?;
    let tools: Vec<String> = manifest
        .operations
        .iter()
        .filter(|op| op.name != manifest.check)
        .map(|op| format!("{}.{}", manifest.id, op.name))
        .collect();
    Ok((
        if registered { StatusCode::CREATED } else { StatusCode::OK },
        Json(json!({ "id": manifest.id, "hash": manifest.hash, "registered": registered, "tools": tools })),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> ConnectorDraft {
        ConnectorDraft {
            name: "Acme Billing".into(),
            description: "Invoices and customers.".into(),
            base_url: "https://api.acme.com/".into(),
            documentation_url: "https://docs.acme.com".into(),
            auth: AuthStyle::Bearer,
            auth_name: String::new(),
            check_path: "/me".into(),
            operations: vec![
                OperationDraft {
                    name: "List invoices".into(),
                    description: "List invoices for a customer.".into(),
                    method: "get".into(),
                    path: "/customers/{customer_id}/invoices".into(),
                    effect: "read_only".into(),
                    params: vec![
                        ParamDraft { name: "customer_id".into(), r#type: "string".into(), required: true, description: "The customer".into() },
                        ParamDraft { name: "limit".into(), r#type: "integer".into(), required: false, description: String::new() },
                    ],
                    reconcile: None,
                },
                OperationDraft {
                    name: "Void invoice".into(),
                    description: "Void an invoice.".into(),
                    method: "POST".into(),
                    path: "/invoices/{id}/void".into(),
                    effect: "irreversible".into(),
                    params: vec![ParamDraft { name: "id".into(), r#type: "string".into(), required: true, description: String::new() }],
                    reconcile: None,
                },
            ],
        }
    }

    #[test]
    fn problems_are_named_in_the_order_to_fix_them() {
        let mut d = draft();
        d.operations[0].params.clear();
        assert_eq!(problems(&d), vec!["Operation List invoices: {customer_id} in the path needs a parameter of that name."]);
        assert!(problems(&draft()).is_empty());
        let empty = ConnectorDraft { name: String::new(), description: String::new(), base_url: String::new(), documentation_url: String::new(), auth: AuthStyle::Bearer, auth_name: String::new(), check_path: "/".into(), operations: vec![] };
        assert_eq!(problems(&empty), vec!["Name the system.", "The API root must start with https://.", "Link to its documentation.", "Describe at least one operation."]);
    }

    #[test]
    fn the_composed_manifest_is_one_the_standard_accepts() {
        let value = compose_manifest(&draft()).unwrap();
        let manifest: ConnectorManifest = serde_json::from_value(value).unwrap();
        manifest.validate().unwrap();
        let sealed = manifest.sealed().unwrap();
        assert_eq!(sealed.id, "acme-billing");
        assert_eq!(sealed.base_url, "https://api.acme.com");
        assert_eq!(sealed.check, "check");
        let names: Vec<&str> = sealed.operations.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, vec!["check", "list-invoices", "void-invoice"]);
        assert_eq!(sealed.connection_specification["properties"]["credentials"]["properties"]["token"]["rusty_secret"], true);
        assert!(!sealed.hash.is_empty());
    }
}
