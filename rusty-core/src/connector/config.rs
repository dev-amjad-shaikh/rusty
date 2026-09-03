//! Config validation against a manifest's `connection_specification`
//! (draft-07, via the `jsonschema` crate) and the secret walk that
//! extracts `rusty_secret` fields before persistence.
//!
//! The wire contract for a rejection is a single sentence naming the
//! failing schema path — `{path}: {reason}`, dot-separated
//! (`credentials.username: required property missing`) — because the
//! setup form pins field errors from exactly this string. Validation
//! reports the first failure; the form round-trips until clean.
//!
//! The secret walk covers the shipped idiom — object `properties`,
//! recursing, with `oneOf` variants resolved against the concrete config
//! value (validation has already passed, so exactly one variant
//! matches). A field flagged `rusty_secret: true` extracts as a whole
//! subtree; the stored record holds the remaining (non-secret) config
//! plus a `path → SealedCredential` map, and serving masks each sealed
//! path as `{"rusty_secret": true}` — secrets never render.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::broker::SealedCredential;

/// The schema extension key that marks a field secret: extracted from
/// the config before persistence, sealed through the credential broker,
/// masked at serving.
pub const SECRET_FLAG: &str = "rusty_secret";

/// Compile a `connection_specification` as a draft-07 schema. Manifest
/// declaration calls this before trusting the schema with anything.
pub fn compile_spec(spec: &Value) -> std::result::Result<jsonschema::Validator, String> {
    jsonschema::draft7::new(spec).map_err(|e| format!("connection_specification: {e}"))
}

/// Validate one config object against the schema, returning the first
/// rejection rendered as `{path}: {reason}` (dot-separated path; the
/// empty path renders as the bare reason). The caller maps this to a
/// 422 verbatim — Studio pins field errors from this exact format.
///
/// A `oneOf` mismatch is refined before rendering: when a variant's
/// discriminator (`const` fields) matches the config, the error reported
/// is that variant's own first failure, so a missing field inside an
/// auth variant names `credentials.username`, not the opaque
/// "not valid under any of the schemas" at `credentials`.
pub fn validate_config(spec: &Value, config: &Value) -> std::result::Result<(), String> {
    let validator = compile_spec(spec)?;
    validator
        .validate(config)
        .map_err(|e| refine_error(spec, config, &e))
}

/// Render the validator's first error, descending through discriminator-
/// matched `oneOf` variants for the specific inner failure.
fn refine_error(spec: &Value, config: &Value, error: &jsonschema::ValidationError) -> String {
    // Follow the error's instance path to the failing subschema and value.
    let mut node_schema = spec;
    let mut node_value = config;
    let mut prefix = String::new();
    for segment in error
        .instance_path()
        .to_string()
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.replace("~1", "/").replace("~0", "~"))
    {
        prefix = join_path(&prefix, &segment);
        let Some(value) = node_value.get(&segment) else {
            break;
        };
        node_value = value;
        let Some(schema) = subschema_for(node_schema, &segment, node_value) else {
            break;
        };
        node_schema = schema;
    }
    if matches!(
        error.kind(),
        jsonschema::error::ValidationErrorKind::OneOfNotValid { .. }
    ) {
        if let Some(variant) = discriminated_variant(node_schema, node_value) {
            if let Ok(inner_validator) = compile_spec(variant) {
                if let Err(inner) = inner_validator.validate(node_value) {
                    return describe_inner(variant, node_value, &inner, &prefix);
                }
            }
        }
    }
    describe_error_at(error, "")
}

/// Inner errors of a discriminator-matched variant, recursing through
/// nested `oneOf`s; paths are relative to the variant's root, so the
/// accumulated prefix leads them.
fn describe_inner(
    spec: &Value,
    value: &Value,
    error: &jsonschema::ValidationError,
    prefix: &str,
) -> String {
    if matches!(
        error.kind(),
        jsonschema::error::ValidationErrorKind::OneOfNotValid { .. }
    ) {
        if let Some(variant) = discriminated_variant(spec, value) {
            if let Ok(inner_validator) = compile_spec(variant) {
                if let Err(inner) = inner_validator.validate(value) {
                    return describe_inner(variant, value, &inner, prefix);
                }
            }
        }
    }
    describe_error_at(error, prefix)
}

/// The subschema governing `key` under `schema`: a declared property, or
/// the matching `oneOf` variant's property.
fn subschema_for<'a>(schema: &'a Value, key: &str, value: &Value) -> Option<&'a Value> {
    if let Some(sub) = schema.get("properties").and_then(|p| p.get(key)) {
        return Some(sub);
    }
    schema
        .get("oneOf")
        .and_then(Value::as_array)
        .and_then(|variants| {
            variants.iter().find_map(|variant| {
                let matches = compile_spec(variant)
                    .map(|v| v.validate(value).is_ok())
                    .unwrap_or(false);
                if matches {
                    variant.get("properties").and_then(|p| p.get(key))
                } else {
                    None
                }
            })
        })
}

/// The `oneOf` variant whose `const`-declared discriminator fields match
/// the config value — the branch the user visibly picked.
fn discriminated_variant<'a>(schema: &'a Value, value: &Value) -> Option<&'a Value> {
    let variants = schema.get("oneOf")?.as_array()?;
    variants
        .iter()
        .filter(|variant| {
            let consts = variant
                .get("properties")
                .and_then(Value::as_object)
                .map(|properties| {
                    properties
                        .iter()
                        .filter(|(_, sub)| sub.get("const").is_some())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            !consts.is_empty()
                && consts.iter().all(|(key, sub)| {
                    value
                        .get(*key)
                        .is_some_and(|v| v == sub.get("const").unwrap())
                })
        })
        .max_by_key(|variant| {
            // Prefer the most specific match (most discriminator fields).
            variant
                .get("properties")
                .and_then(Value::as_object)
                .map(|p| p.values().filter(|sub| sub.get("const").is_some()).count())
                .unwrap_or(0)
        })
}

/// Render one validation error as `{path}: {reason}`, with the path
/// prefixed by the enclosing variant's location.
///
/// Two kinds get hand-written reasons, because the validator's own
/// wording points at the wrong field for the form's purposes: a missing
/// required property reports against the *containing* object (the path
/// is completed with the property name), and an unknown property names
/// the extra key. Everything else keeps the validator's message — it is
/// the caller's own config echoed back, never stored state.
fn describe_error_at(error: &jsonschema::ValidationError, prefix: &str) -> String {
    let rel = dotted_path(&error.instance_path().to_string());
    let base = if prefix.is_empty() {
        rel
    } else if rel.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}.{rel}")
    };
    match error.kind() {
        jsonschema::error::ValidationErrorKind::Required { property } => {
            let property = property.as_str().unwrap_or("?");
            let path = join_path(&base, property);
            format!("{path}: required property missing")
        }
        jsonschema::error::ValidationErrorKind::AdditionalProperties { unexpected } => {
            let first = unexpected.first().map(String::as_str).unwrap_or("?");
            let path = join_path(&base, first);
            format!("{path}: unknown property")
        }
        _ => {
            if base.is_empty() {
                error.to_string()
            } else {
                format!("{base}: {error}")
            }
        }
    }
}

/// `/credentials/username` (JSON pointer) → `credentials.username`.
fn dotted_path(pointer: &str) -> String {
    pointer
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.replace("~1", "/").replace("~0", "~"))
        .collect::<Vec<_>>()
        .join(".")
}

fn join_path(base: &str, leaf: &str) -> String {
    if base.is_empty() {
        leaf.to_owned()
    } else {
        format!("{base}.{leaf}")
    }
}

// --------------------------------------------------------------------- //
// The secret walk
// --------------------------------------------------------------------- //

/// Extract every `rusty_secret: true` field present in `config`:
/// `(dot-path, value)` pairs, sorted by path. Call after
/// [`validate_config`] has passed; the walk resolves `oneOf` variants
/// against the concrete value, so the exact branch the config took is
/// the branch walked.
pub fn extract_secrets(spec: &Value, config: &Value) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    walk_secrets(spec, config, "", &mut out);
    out.sort_by(|left, right| left.0.cmp(&right.0));
    out
}

fn walk_secrets(schema: &Value, value: &Value, prefix: &str, out: &mut Vec<(String, Value)>) {
    if schema.get(SECRET_FLAG) == Some(&Value::Bool(true)) {
        out.push((prefix.trim_end_matches('.').to_owned(), value.clone()));
        return;
    }
    let Some(object) = value.as_object() else {
        return;
    };
    // The variant the config took, when this node is polymorphic —
    // validation already passed, so at most one matches.
    let variant = schema
        .get("oneOf")
        .and_then(Value::as_array)
        .and_then(|variants| {
            variants.iter().find(|variant| {
                jsonschema::draft7::new(variant)
                    .map(|v| v.validate(value).is_ok())
                    .unwrap_or(false)
            })
        });
    for (key, item) in object {
        let subschema = schema
            .get("properties")
            .and_then(|p| p.get(key))
            .or_else(|| variant.and_then(|v| v.get("properties").and_then(|p| p.get(key))));
        if let Some(subschema) = subschema {
            walk_secrets(subschema, item, &format!("{prefix}{key}."), out);
        }
    }
}

// --------------------------------------------------------------------- //
// Sealed and served shapes
// --------------------------------------------------------------------- //

/// The stored instance's non-secret config: `config` with every
/// extracted path removed (empty parents cleaned up — a `credentials`
/// object whose only fields were secrets disappears entirely).
pub fn without_secrets(mut config: Value, extracted: &[(String, Value)]) -> Value {
    for (path, _) in extracted {
        remove_path(&mut config, path);
    }
    config
}

fn remove_path(value: &mut Value, path: &str) {
    let (head, rest) = match path.split_once('.') {
        Some((head, rest)) => (head, Some(rest)),
        None => (path, None),
    };
    let prune = if let Some(object) = value.as_object_mut() {
        match rest {
            Some(rest) => {
                if let Some(child) = object.get_mut(head) {
                    remove_path(child, rest);
                }
                object
                    .get(head)
                    .is_some_and(|child| child.as_object().is_some_and(serde_json::Map::is_empty))
            }
            None => {
                object.remove(head);
                false
            }
        }
    } else {
        false
    };
    if prune {
        if let Some(object) = value.as_object_mut() {
            object.remove(head);
        }
    }
}

/// Rebuild the full config for execution: the stored non-secret config
/// plus each opened secret inserted back at its path.
pub fn insert_opened_secrets(mut config: Value, opened: &[(String, Value)]) -> Value {
    for (path, secret) in opened {
        insert_path(&mut config, path, secret.clone());
    }
    config
}

/// The served shape: each sealed path re-inserted as
/// `{"rusty_secret": true}` — "set, never rendered".
pub fn insert_masked_secrets(
    mut config: Value,
    sealed: &BTreeMap<String, SealedCredential>,
) -> Value {
    for path in sealed.keys() {
        insert_path(
            &mut config,
            path,
            Value::Object(serde_json::Map::from_iter([(
                SECRET_FLAG.to_owned(),
                Value::Bool(true),
            )])),
        );
    }
    config
}

fn insert_path(value: &mut Value, path: &str, leaf: Value) {
    let mut segments = path.split('.').peekable();
    let mut target = value;
    while let Some(segment) = segments.next() {
        if segments.peek().is_none() {
            if let Some(object) = target.as_object_mut() {
                object.insert(segment.to_owned(), leaf);
            }
            return;
        }
        if target.get(segment).is_none() {
            if let Some(object) = target.as_object_mut() {
                object.insert(segment.to_owned(), Value::Object(serde_json::Map::new()));
            }
        }
        let Some(next) = target.get_mut(segment) else {
            return;
        };
        target = next;
    }
}
