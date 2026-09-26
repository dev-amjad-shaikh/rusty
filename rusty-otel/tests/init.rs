//! Process-global behavior of `rusty_otel::init`.
//!
//! The tracing subscriber is process-global, so these assertions live in a
//! single test to avoid ordering races between parallel tests in the same
//! binary. No live collector is required: init uses the fmt-only path.

use rusty_otel::{init, init_local, OTelConfig, OTelError};

#[test]
fn init_is_once_per_process_and_shutdown_is_idempotent() {
    // First init (fmt-only, no collector) succeeds.
    let mut guard = init_local("otel-test-service").expect("first init should succeed");

    // A second init — any flavor — must fail gracefully with a dedicated
    // error and leave the existing subscriber untouched.
    let second = init(OTelConfig {
        service_name: "otel-test-service".into(),
        otlp_endpoint: None,
        log_filter: Some("debug".into()),
    });
    assert!(
        matches!(second, Err(OTelError::SubscriberAlreadyInstalled)),
        "second init must report SubscriberAlreadyInstalled, got: {second:?}"
    );
    let third = init_local("otel-test-service-again");
    assert!(
        matches!(third, Err(OTelError::SubscriberAlreadyInstalled)),
        "init_local must also report SubscriberAlreadyInstalled, got: {third:?}"
    );

    // Shutdown is idempotent: repeated calls (and the final drop) are no-ops.
    guard.shutdown();
    guard.shutdown();
    guard.shutdown();
    // `guard` drops here — Drop calls shutdown() once more, also a no-op.
}
