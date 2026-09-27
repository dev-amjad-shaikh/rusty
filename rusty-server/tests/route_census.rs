//! The authorization census (G12).
//!
//! Every mounted route must declare the scope required to call it. A route
//! with no declaration is denied at runtime, which is safe but silent — this
//! test makes it loud at build time instead, so a new endpoint cannot ship
//! unreviewed or quietly unreachable.
//!
//! The census reads the router source rather than the built router because the
//! declaration and the mount are both source facts, and a drift between them
//! is exactly what this catches.

use std::collections::BTreeSet;

fn routes_source() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/routes.rs"))
        .expect("routes.rs is readable")
}

/// Axum's `{id}` and the older `:id` name the same wildcard.
fn normalize(path: &str) -> String {
    let mut out = String::new();
    for segment in path.split('/') {
        if segment.starts_with('{') || segment.starts_with(':') {
            out.push_str("/{}");
        } else if !segment.is_empty() {
            out.push('/');
            out.push_str(segment);
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// Every `(VERB, path)` a `.route(...)` call mounts, whether written on one
/// line or several. A regex over lines misses the multi-line form, and a
/// census that shares its blind spot with the code it audits is no census —
/// so this scans balanced parentheses.
fn mounted(src: &str) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    let bytes = src.as_bytes();
    let mut i = 0;
    while let Some(found) = src[i..].find(".route(") {
        let start = i + found + ".route(".len();
        let mut j = start;
        let mut depth = 1usize;
        while j < bytes.len() && depth > 0 {
            match bytes[j] {
                b'"' => {
                    j += 1;
                    while j < bytes.len() && bytes[j] != b'"' {
                        j += if bytes[j] == b'\\' { 2 } else { 1 };
                    }
                }
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            j += 1;
        }
        let body = &src[start..j.saturating_sub(1)];
        if let Some(path) = body.split('"').nth(1) {
            for verb in ["get", "post", "put", "patch", "delete"] {
                if body.contains(&format!("{verb}(")) {
                    out.insert((verb.to_uppercase(), normalize(path)));
                }
            }
        }
        i = j;
    }
    out
}

fn declared(src: &str) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    let mut pending_verb: Option<String> = None;
    for line in src.lines() {
        let trimmed = line.trim();
        // A route declared public is declared: `declare_public` is a decision
        // about who may call it, not an omission.
        if let Some(rest) = trimmed.strip_prefix("table.declare_public(") {
            let mut parts = rest.split('"').skip(1).step_by(2);
            if let (Some(verb), Some(path)) = (parts.next(), parts.next()) {
                out.insert((verb.to_string(), normalize(path)));
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("table.declare(") {
            // Single-line form: table.declare("GET", "/path", ...)
            let mut parts = rest.split('"').skip(1).step_by(2);
            if let (Some(verb), Some(path)) = (parts.next(), parts.next()) {
                out.insert((verb.to_string(), normalize(path)));
                continue;
            }
            pending_verb = None;
            continue;
        }
        // Multi-line form: the verb and path are on their own lines.
        if let Some(literal) = trimmed.strip_suffix(',').and_then(|t| t.strip_prefix('"')) {
            let value = literal.trim_end_matches('"');
            match &pending_verb {
                None if matches!(value, "GET" | "POST" | "PUT" | "PATCH" | "DELETE") => {
                    pending_verb = Some(value.to_string());
                }
                Some(verb) if value.starts_with('/') => {
                    out.insert((verb.clone(), normalize(value)));
                    pending_verb = None;
                }
                _ => {}
            }
        }
    }
    out
}

#[test]
fn every_mounted_route_declares_its_scope() {
    let src = routes_source();
    let undeclared: Vec<_> = mounted(&src).difference(&declared(&src)).cloned().collect();
    assert!(
        undeclared.is_empty(),
        "{} mounted route(s) have no scope declaration in build_scope_table, so they are \
         denied at runtime. Declare each in routes.rs:\n{}",
        undeclared.len(),
        undeclared
            .iter()
            .map(|(v, p)| format!(
                "  table.declare(\"{v}\", \"{p}\", Scope::parse(\"resource:action\").unwrap());"
            ))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}
