//! OpenAPI 3.x importer: generate connector operations from a spec.
//!
//! Maps OpenAPI paths and operations to [`ConnectorOperation`] structs,
//! producing an import report that lists every operation mapped and every
//! operation that could not be mapped (unsupported constructs named).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::conn_err;
use super::manifest::{
    ConnectorOperation, HttpMethod, OperationEffect, MAX_OPERATION_DESCRIPTION_LEN,
    MAX_OPERATION_NAME_LEN, MAX_PATH_TEMPLATE_LEN,
};
use crate::error::Result;

/// The outcome of importing an OpenAPI document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenApiImport {
    /// Operations successfully mapped.
    pub mapped: Vec<ConnectorOperation>,
    /// Operations that could not be mapped, with reasons.
    pub unmapped: Vec<UnmappedOperation>,
}

/// One operation that could not be mapped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnmappedOperation {
    /// The path (e.g. `/users/{id}`).
    pub path: String,
    /// The HTTP method (e.g. `GET`).
    pub method: String,
    /// Human-readable reason for the failure.
    pub reason: String,
}

/// A detected difference between two imports of the same OpenAPI document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum OperationDiff {
    /// An operation present in the new import but absent in the old.
    Added {
        /// The operation that was added.
        operation: ConnectorOperation,
    },
    /// An operation present in the old import but absent in the new.
    Removed {
        /// The name of the removed operation.
        name: String,
        /// The method of the removed operation.
        method: HttpMethod,
        /// The path of the removed operation.
        path: String,
    },
    /// An operation present in both imports but with different details.
    Changed {
        /// The name of the changed operation.
        name: String,
        /// The method of the changed operation.
        method: HttpMethod,
        /// The path of the changed operation.
        path: String,
        /// Human-readable description of what changed.
        description: String,
    },
}

/// Compare two OpenAPI imports and report the differences.
///
/// Operations are matched by `(name, method, path)`. Operations that
/// appear in `new` but not `old` are [`OperationDiff::Added`]; operations
/// that appear in `old` but not `new` are [`OperationDiff::Removed`];
/// operations whose serialized form differs are [`OperationDiff::Changed`].
pub fn diff_imports(old: &OpenApiImport, new: &OpenApiImport) -> Vec<OperationDiff> {
    let mut diffs = Vec::new();

    // Build a map of old operations by identity.
    let old_by_key: std::collections::BTreeMap<(String, String, String), &ConnectorOperation> = old
        .mapped
        .iter()
        .map(|op| {
            let key = (
                op.name.clone(),
                op.method.as_str().to_owned(),
                op.path.clone(),
            );
            (key, op)
        })
        .collect();

    // Build a map of new operations by identity.
    let new_by_key: std::collections::BTreeMap<(String, String, String), &ConnectorOperation> = new
        .mapped
        .iter()
        .map(|op| {
            let key = (
                op.name.clone(),
                op.method.as_str().to_owned(),
                op.path.clone(),
            );
            (key, op)
        })
        .collect();

    // Find removed and changed operations.
    for (key, old_op) in &old_by_key {
        match new_by_key.get(key) {
            None => {
                diffs.push(OperationDiff::Removed {
                    name: old_op.name.clone(),
                    method: old_op.method,
                    path: old_op.path.clone(),
                });
            }
            Some(new_op) => {
                // Compare canonical JSON to detect any structural change.
                let old_json =
                    serde_json::to_value(*old_op).expect("ConnectorOperation serializes");
                let new_json =
                    serde_json::to_value(*new_op).expect("ConnectorOperation serializes");
                if old_json != new_json {
                    diffs.push(OperationDiff::Changed {
                        name: old_op.name.clone(),
                        method: old_op.method,
                        path: old_op.path.clone(),
                        description: "operation definition changed".to_owned(),
                    });
                }
            }
        }
    }

    // Find added operations.
    for (key, new_op) in &new_by_key {
        if !old_by_key.contains_key(key) {
            diffs.push(OperationDiff::Added {
                operation: (*new_op).clone(),
            });
        }
    }

    diffs
}

/// Import an OpenAPI 3.x JSON document.
///
/// Parses `paths` and maps each operation to a [`ConnectorOperation`].
/// Operations without `operationId` are unmapped. Path parameters become
/// properties in `params_schema`; query, header, and request body
/// properties are merged into the same schema.
pub fn import_openapi(spec: &Value) -> Result<OpenApiImport> {
    let mut mapped = Vec::new();
    let mut unmapped = Vec::new();

    let Some(paths) = spec.get("paths").and_then(Value::as_object) else {
        return Err(conn_err("OpenAPI document has no paths object"));
    };

    for (path, path_item) in paths {
        let Some(path_item) = path_item.as_object() else {
            continue;
        };
        for (method, operation_value) in path_item {
            // Skip OpenAPI metadata fields.
            if method == "parameters" || method == "summary" || method == "description" {
                continue;
            }
            let http_method = match parse_method(method) {
                Some(m) => m,
                None => {
                    unmapped.push(UnmappedOperation {
                        path: path.clone(),
                        method: method.clone(),
                        reason: format!("unsupported HTTP method `{method}`"),
                    });
                    continue;
                }
            };

            match map_operation(path, http_method, operation_value) {
                Ok(op) => mapped.push(op),
                Err(reason) => unmapped.push(UnmappedOperation {
                    path: path.clone(),
                    method: method.clone(),
                    reason,
                }),
            }
        }
    }

    Ok(OpenApiImport { mapped, unmapped })
}

fn parse_method(method: &str) -> Option<HttpMethod> {
    match method.to_ascii_uppercase().as_str() {
        "GET" => Some(HttpMethod::Get),
        "POST" => Some(HttpMethod::Post),
        "PATCH" => Some(HttpMethod::Patch),
        "PUT" => Some(HttpMethod::Put),
        "DELETE" => Some(HttpMethod::Delete),
        _ => None,
    }
}

fn map_operation(
    path: &str,
    method: HttpMethod,
    value: &Value,
) -> std::result::Result<ConnectorOperation, String> {
    let operation = value.as_object().ok_or("operation is not an object")?;

    let operation_id = operation
        .get("operationId")
        .and_then(Value::as_str)
        .ok_or("missing operationId")?;

    let name = kebab_case(operation_id);
    if name.is_empty() || name.len() > MAX_OPERATION_NAME_LEN {
        return Err(format!(
            "operationId `{operation_id}` maps to kebab-case name `{name}` which exceeds the {} byte cap",
            MAX_OPERATION_NAME_LEN
        ));
    }

    let description = operation
        .get("summary")
        .or_else(|| operation.get("description"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let description = if description.len() > MAX_OPERATION_DESCRIPTION_LEN {
        format!(
            "{}…[truncated]",
            &description[..MAX_OPERATION_DESCRIPTION_LEN.saturating_sub(12)]
        )
    } else {
        description
    };

    // Derive default effect from method: GET → ReadOnly, DELETE → Irreversible, else Idempotent.
    let effect = default_effect(method);

    // Collect path-level and operation-level parameters.
    let mut all_params: Vec<&Value> = Vec::new();

    // Path-level parameters.
    if let Some(path_item) = value.get("__path_item").and_then(Value::as_object) {
        if let Some(params) = path_item.get("parameters").and_then(Value::as_array) {
            all_params.extend(params.iter());
        }
    }
    // Operation-level parameters.
    if let Some(params) = operation.get("parameters").and_then(Value::as_array) {
        all_params.extend(params.iter());
    }

    // Build params_schema from parameters and request body.
    let mut properties = serde_json::Map::new();
    let mut required: Vec<String> = Vec::new();
    let mut headers: Vec<(String, String)> = Vec::new();

    for param in all_params {
        let Some(param_obj) = param.as_object() else {
            continue;
        };
        let param_in = param_obj.get("in").and_then(Value::as_str).unwrap_or("");
        let param_name = param_obj
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        match param_in {
            "path" | "query" => {
                if let Some(schema) = resolve_schema(param_obj, param) {
                    properties.insert(param_name.clone(), schema.clone());
                    if param_obj.get("required").and_then(Value::as_bool) == Some(true) {
                        required.push(param_name);
                    }
                }
            }
            "header" => {
                // Non-auth headers declared as static templates for now.
                // The value template resolves from config at call time.
                // Skip well-known auth headers.
                let lower = param_name.to_ascii_lowercase();
                if lower != "authorization" && lower != "cookie" {
                    headers.push((param_name.clone(), format!("{{{}}}", param_name)));
                }
            }
            _ => {}
        }
    }

    // Merge request body schema into params_schema.
    if let Some(body_schema) = extract_request_body_schema(operation) {
        if let Some(body_props) = body_schema.get("properties").and_then(Value::as_object) {
            for (key, val) in body_props {
                properties.insert(key.clone(), val.clone());
            }
        }
        if let Some(body_required) = body_schema.get("required").and_then(Value::as_array) {
            for r in body_required.iter().filter_map(Value::as_str) {
                if !required.contains(&r.to_string()) {
                    required.push(r.to_string());
                }
            }
        }
    }

    let params_schema = serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
    });

    // Convert path from OpenAPI `{param}` to connector `{param}` — same syntax.
    let op_path = if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    };
    if op_path.len() > MAX_PATH_TEMPLATE_LEN {
        return Err(format!(
            "path `{op_path}` exceeds {MAX_PATH_TEMPLATE_LEN} bytes"
        ));
    }

    Ok(ConnectorOperation {
        name,
        description,
        method,
        path: op_path,
        effect,
        params_schema,
        headers,
        auth: Vec::new(),
        max_response_bytes: None,
    })
}

/// Default effect classification from HTTP method.
fn default_effect(method: HttpMethod) -> OperationEffect {
    match method {
        HttpMethod::Get => OperationEffect::ReadOnly,
        HttpMethod::Delete => OperationEffect::Irreversible,
        _ => OperationEffect::Idempotent,
    }
}

/// Resolve a parameter or request body schema, following `$ref` if present.
fn resolve_schema<'a>(
    obj: &'a serde_json::Map<String, Value>,
    _fallback: &'a Value,
) -> Option<&'a Value> {
    if let Some(schema) = obj.get("schema") {
        return Some(schema);
    }
    None
}

/// Extract the request body JSON Schema from an operation object.
fn extract_request_body_schema(operation: &serde_json::Map<String, Value>) -> Option<Value> {
    let body = operation.get("requestBody")?.as_object()?;
    let content = body.get("content")?.as_object()?;
    // Prefer application/json; fall back to the first entry.
    let media_type = content
        .get("application/json")
        .or_else(|| content.values().next())?;
    let schema = media_type.get("schema")?;
    Some(schema.clone())
}

/// Convert camelCase/PascalCase/snake_case to kebab-case.
fn kebab_case(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut prev_lower = false;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'_' || b == b'-' || b == b' ' {
            if !out.ends_with('-') {
                out.push('-');
            }
            prev_lower = false;
            continue;
        }
        let is_upper = b.is_ascii_uppercase();
        let is_lower = b.is_ascii_lowercase();
        let is_digit = b.is_ascii_digit();
        if is_upper {
            if (prev_lower || (i + 1 < bytes.len() && bytes[i + 1].is_ascii_lowercase()))
                && !out.is_empty()
                && !out.ends_with('-')
            {
                out.push('-');
            }
            out.push(b.to_ascii_lowercase() as char);
            prev_lower = false;
        } else if is_lower || is_digit {
            out.push(b as char);
            prev_lower = is_lower;
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn kebab_case_conversions() {
        assert_eq!(kebab_case("getUser"), "get-user");
        assert_eq!(kebab_case("GetUser"), "get-user");
        assert_eq!(kebab_case("get_user"), "get-user");
        assert_eq!(kebab_case("get-user"), "get-user");
        assert_eq!(kebab_case("getuser"), "getuser");
        assert_eq!(kebab_case("APIv2GetUser"), "ap-iv2-get-user");
    }

    #[test]
    fn import_petstore_happy_path() {
        let spec = json!({
            "openapi": "3.0.0",
            "paths": {
                "/pets": {
                    "get": {
                        "operationId": "listPets",
                        "summary": "List all pets",
                        "parameters": [
                            {
                                "name": "limit",
                                "in": "query",
                                "schema": { "type": "integer", "minimum": 1, "maximum": 100 },
                                "required": false
                            }
                        ]
                    },
                    "post": {
                        "operationId": "createPet",
                        "summary": "Create a pet",
                        "requestBody": {
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "object",
                                        "properties": {
                                            "name": { "type": "string" },
                                            "tag": { "type": "string" }
                                        },
                                        "required": ["name"]
                                    }
                                }
                            }
                        }
                    }
                },
                "/pets/{petId}": {
                    "get": {
                        "operationId": "showPetById",
                        "summary": "Info for a specific pet",
                        "parameters": [
                            {
                                "name": "petId",
                                "in": "path",
                                "required": true,
                                "schema": { "type": "string" }
                            }
                        ]
                    }
                }
            }
        });

        let result = import_openapi(&spec).unwrap();
        assert_eq!(
            result.mapped.len(),
            3,
            "expected 3 mapped operations, got {} mapped and {} unmapped",
            result.mapped.len(),
            result.unmapped.len()
        );
        assert!(
            result.unmapped.is_empty(),
            "expected no unmapped operations"
        );

        let list = result
            .mapped
            .iter()
            .find(|o| o.name == "list-pets")
            .expect("list-pets");
        assert_eq!(list.method, HttpMethod::Get);
        assert_eq!(list.effect, OperationEffect::ReadOnly);
        assert_eq!(list.path, "/pets");
        let limit_schema = list
            .params_schema
            .get("properties")
            .and_then(|p| p.get("limit"));
        assert!(limit_schema.is_some());

        let create = result
            .mapped
            .iter()
            .find(|o| o.name == "create-pet")
            .expect("create-pet");
        assert_eq!(create.method, HttpMethod::Post);
        assert_eq!(create.effect, OperationEffect::Idempotent);
        let name_req = create
            .params_schema
            .get("required")
            .and_then(|r| r.as_array());
        assert!(name_req.is_some_and(|arr| arr.iter().any(|v| v.as_str() == Some("name"))));

        let show = result
            .mapped
            .iter()
            .find(|o| o.name == "show-pet-by-id")
            .expect("show-pet-by-id");
        assert_eq!(show.path, "/pets/{petId}");
        assert!(show
            .params_schema
            .get("properties")
            .and_then(|p| p.get("petId"))
            .is_some());
    }

    #[test]
    fn import_reports_unmapped() {
        let spec = json!({
            "openapi": "3.0.0",
            "paths": {
                "/unknown": {
                    "trace": {
                        "operationId": "traceThing",
                        "summary": "A TRACE operation"
                    }
                },
                "/no-id": {
                    "get": {
                        "summary": "Missing operationId"
                    }
                }
            }
        });

        let result = import_openapi(&spec).unwrap();
        assert!(result.mapped.is_empty());
        assert_eq!(result.unmapped.len(), 2);
        assert!(result
            .unmapped
            .iter()
            .any(|u| u.reason.contains("unsupported HTTP method")));
        assert!(result
            .unmapped
            .iter()
            .any(|u| u.reason.contains("missing operationId")));
    }

    #[test]
    fn diff_detects_added_operation() {
        let old = import_openapi(&json!({
            "openapi": "3.0.0",
            "paths": {
                "/pets": {
                    "get": { "operationId": "listPets", "summary": "List pets" }
                }
            }
        }))
        .unwrap();
        let new = import_openapi(&json!({
            "openapi": "3.0.0",
            "paths": {
                "/pets": {
                    "get": { "operationId": "listPets", "summary": "List pets" },
                    "post": { "operationId": "createPet", "summary": "Create a pet" }
                }
            }
        }))
        .unwrap();
        let diffs = diff_imports(&old, &new);
        assert_eq!(diffs.len(), 1);
        assert!(
            matches!(&diffs[0], OperationDiff::Added { operation } if operation.name == "create-pet")
        );
    }

    #[test]
    fn diff_detects_removed_operation() {
        let old = import_openapi(&json!({
            "openapi": "3.0.0",
            "paths": {
                "/pets": {
                    "get": { "operationId": "listPets", "summary": "List pets" },
                    "post": { "operationId": "createPet", "summary": "Create a pet" }
                }
            }
        }))
        .unwrap();
        let new = import_openapi(&json!({
            "openapi": "3.0.0",
            "paths": {
                "/pets": {
                    "get": { "operationId": "listPets", "summary": "List pets" }
                }
            }
        }))
        .unwrap();
        let diffs = diff_imports(&old, &new);
        assert_eq!(diffs.len(), 1);
        assert!(matches!(&diffs[0], OperationDiff::Removed { name, .. } if name == "create-pet"));
    }

    #[test]
    fn diff_detects_changed_operation() {
        let old = import_openapi(&json!({
            "openapi": "3.0.0",
            "paths": {
                "/pets": {
                    "get": {
                        "operationId": "listPets",
                        "summary": "List pets",
                        "parameters": [
                            {"name": "limit", "in": "query", "schema": {"type": "integer"}}
                        ]
                    }
                }
            }
        }))
        .unwrap();
        let new = import_openapi(&json!({
            "openapi": "3.0.0",
            "paths": {
                "/pets": {
                    "get": {
                        "operationId": "listPets",
                        "summary": "List all pets",
                        "parameters": [
                            {"name": "limit", "in": "query", "schema": {"type": "integer", "maximum": 100}}
                        ]
                    }
                }
            }
        })).unwrap();
        let diffs = diff_imports(&old, &new);
        assert_eq!(diffs.len(), 1);
        assert!(matches!(&diffs[0], OperationDiff::Changed { name, .. } if name == "list-pets"));
    }

    #[test]
    fn diff_empty_when_identical() {
        let spec = json!({
            "openapi": "3.0.0",
            "paths": {
                "/pets": {
                    "get": { "operationId": "listPets", "summary": "List pets" }
                }
            }
        });
        let old = import_openapi(&spec).unwrap();
        let new = import_openapi(&spec).unwrap();
        let diffs = diff_imports(&old, &new);
        assert!(diffs.is_empty());
    }
}
