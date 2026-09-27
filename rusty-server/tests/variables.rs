//! `{{variables}}` at admission: the agent's settings under the run's values,
//! rendered into the charter and the skills; an unknown name stays literal.

use rusty_agent_server::variables::{render_variables, render_variables_in, run_variables};
use serde_json::json;

#[test]
fn settings_under_run_values_and_unknown_names_stay_literal() {
    let config = json!({ "studio_intent": { "variables": [
        { "name": "site", "source": "setting", "value": "Building 4" },
        { "name": "org", "source": "trigger", "path": "event.ticket.org" },
    ] } });
    let sent = [("org".to_owned(), "Acme".to_owned())]
        .into_iter()
        .collect();
    let values = run_variables(&config, Some(&sent));
    assert_eq!(values.get("site").map(String::as_str), Some("Building 4"));
    assert_eq!(values.get("org").map(String::as_str), Some("Acme"));

    let text = render_variables("At {{site}} for {{ org }}: {{tier}} and {{open", &values);
    assert_eq!(text, "At Building 4 for Acme: {{tier}} and {{open");

    let mut skills =
        json!([{ "name": "greet", "body": "Say hello from {{site}}", "tools": ["a"] }]);
    render_variables_in(&mut skills, &values);
    assert_eq!(skills[0]["body"], "Say hello from Building 4");
}

#[test]
fn a_test_value_on_the_run_overrides_the_setting() {
    let config = json!({ "studio_intent": { "variables": [{ "name": "site", "source": "setting", "value": "Building 4" }] } });
    let sent = [("site".to_owned(), "Test Site 9".to_owned())]
        .into_iter()
        .collect();
    assert_eq!(
        run_variables(&config, Some(&sent))
            .get("site")
            .map(String::as_str),
        Some("Test Site 9")
    );
}
