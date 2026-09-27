//! A production server must not serve without authentication (G12).
//!
//! Dev mode grants every request super-user scopes. That is a reasonable local
//! default and an open door in production, so the boot refuses rather than
//! warns — the failure a deployment can see beats a log line it cannot.

use rusty_agent_server::ServerConfig;

#[tokio::test]
async fn production_without_auth_refuses_to_serve() {
    let mut config = ServerConfig::default().in_production(true);
    config.bind_addr = ([127, 0, 0, 1], 0).into();
    assert!(
        !config.auth_enabled(),
        "the case under test is a keyless config"
    );

    let err = rusty_agent_server::serve_with_shutdown(
        rusty_agent_server::GraphRegistry::new(),
        config,
        std::future::ready(()),
    )
    .await
    .expect_err("a keyless production boot must fail");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        err.to_string().contains("without authentication"),
        "the refusal must say why: {err}"
    );
}

#[tokio::test]
async fn production_with_a_key_is_allowed_to_start() {
    let mut config = ServerConfig::default()
        .in_production(true)
        .with_api_key("test-key");
    config.bind_addr = ([127, 0, 0, 1], 0).into();
    assert!(config.auth_enabled());

    // Shut down immediately: this asserts the guard lets an authenticated
    // production boot through, not that the server runs.
    rusty_agent_server::serve_with_shutdown(
        rusty_agent_server::GraphRegistry::new(),
        config,
        std::future::ready(()),
    )
    .await
    .expect("an authenticated production boot starts");
}
