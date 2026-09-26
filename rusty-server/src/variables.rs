//! `{{variables}}` in an agent's charter and skills: what the placeholders
//! take on a run, and their rendering. The agent declares them under
//! `studio_intent.variables` (name, source `setting` | `trigger`, a `value`
//! or an event `path`); a run brings values under `config.variables`.

/// The values the `{{name}}` placeholders take this run: the agent's own
/// settings (`studio_intent.variables[]` with a `value`), under what the run
/// sent (`config.variables`).
pub fn run_variables(
    config: &serde_json::Value,
    sent: Option<&std::collections::BTreeMap<String, String>>,
) -> std::collections::BTreeMap<String, String> {
    let mut values = std::collections::BTreeMap::new();
    for v in assistant_variables(config) {
        if let (Some(name), Some(value)) = (v.get("name").and_then(serde_json::Value::as_str), v.get("value").and_then(serde_json::Value::as_str)) {
            values.insert(name.to_owned(), value.to_owned());
        }
    }
    if let Some(sent) = sent {
        for (k, v) in sent {
            values.insert(k.clone(), v.clone());
        }
    }
    values
}

/// The variables an assistant declares (`studio_intent.variables`), each an
/// object with `name`, `source` (`setting` | `trigger`), and per source a
/// `value` or an event `path`.
pub fn assistant_variables(config: &serde_json::Value) -> Vec<serde_json::Value> {
    config
        .get("studio_intent")
        .and_then(|intent| intent.get("variables"))
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// `{{ name }}` → its value; a name with no value stays literal, so the
/// model (and the journal) sees the placeholder rather than nothing.
pub fn render_variables(text: &str, values: &std::collections::BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let name = after[..end].trim();
                match values.get(name) {
                    Some(v) => out.push_str(v),
                    None => out.push_str(&rest[start..start + 2 + end + 2]),
                }
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Render the placeholders in every string of a JSON value — a skills
/// section, whose procedures are text.
pub fn render_variables_in(value: &mut serde_json::Value, values: &std::collections::BTreeMap<String, String>) {
    match value {
        serde_json::Value::String(s) if s.contains("{{") => *s = render_variables(s, values),
        serde_json::Value::Array(items) => items.iter_mut().for_each(|v| render_variables_in(v, values)),
        serde_json::Value::Object(map) => map.values_mut().for_each(|v| render_variables_in(v, values)),
        _ => {}
    }
}
