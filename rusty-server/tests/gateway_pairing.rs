//! Device pairing over the gateway WS transport (EP-04-S03): the
//! challenge-nonce handshake, loopback auto-approval, operator approval
//! for remote devices, revocation, and the token's absence from snapshots
//! — driven as a real WS client over a real socket.
//!
//! Maps to the story's test verification:
//!
//! - `pairing_happy_path_loopback`: full hello → challenge → answer →
//!   paired over a loopback socket with auto-approve on; the token is
//!   issued, authenticates a subsequent connection (`device_attach`), and
//!   appears in no snapshot.
//! - `pairing_denials_are_distinct`: replayed nonce, expired nonce, and a
//!   signature omitting the platform binding each earn `Pairing::Denied`
//!   with their distinct reason.
//! - `remote_requires_operator`: the same flow over a non-loopback
//!   address stays `pending` even with auto-approve policy on, an
//!   under-scoped principal cannot approve, and an operator-scoped
//!   approval frame pairs the device.
//! - `revocation_closes_and_rejects`: revoking mid-connection closes the
//!   attached socket, and the token is refused on reconnect with the
//!   typed `device_revoked` reason.
//!
//! Plane-level transitions (approval without a verified answer, the audit
//! trail's actor attribution, the roster keying) are covered by the unit
//! tests in `src/gateway_pairing.rs`; these tests prove the wire surface.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};
use futures::{SinkExt, StreamExt};
use rusty_agent_server::{GraphRegistry, ServerConfig, serve_with_shutdown};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TIMEOUT: Duration = Duration::from_secs(5);

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-server-pairing-test-{}",
        uuid::Uuid::new_v4()
    ))
}

/// Bind the server on a free port of `ip` and spawn it. The returned
/// handle aborts the server at test end.
async fn spawn_server_on(ip: std::net::IpAddr, mut config: ServerConfig) -> SocketAddr {
    let probe = std::net::TcpListener::bind(SocketAddr::new(ip, 0)).unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    config.bind_addr = addr;
    tokio::spawn(serve_with_shutdown(
        GraphRegistry::new(),
        config,
        std::future::pending::<()>(),
    ));
    // Give the listener a moment to bind.
    tokio::time::sleep(Duration::from_millis(100)).await;
    addr
}

/// A loopback server with the given pairing policy.
async fn spawn_loopback(auto_approve: bool, challenge_ttl: Duration) -> (SocketAddr, PathBuf) {
    let store = temp_store();
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone());
    config.pairing_auto_approve_loopback = auto_approve;
    config.pairing_challenge_ttl = challenge_ttl;
    let addr = spawn_server_on(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), config).await;
    (addr, store)
}

/// A non-loopback interface address this host answers on, when one
/// exists. UDP `connect` performs a route lookup without sending
/// traffic, so the source address it reports is the interface a remote
/// peer would reach us on.
fn non_loopback_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:80").ok()?; // TEST-NET-1: unroutable, never sent
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback()).then_some(ip)
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Connect to `/gateway/ws`, optionally with an API key, and consume the
/// snapshot-on-connect frame.
async fn connect(addr: SocketAddr, key: Option<&str>) -> (Ws, Value) {
    let mut request = format!("ws://{addr}/gateway/ws")
        .into_client_request()
        .unwrap();
    if let Some(key) = key {
        request
            .headers_mut()
            .insert("x-api-key", key.parse().unwrap());
    }
    let (mut ws, _response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("WS upgrade succeeds");
    let snapshot = recv_frame(&mut ws).await;
    assert_eq!(snapshot["name"], json!("snapshot"), "{snapshot}");
    (ws, snapshot)
}

/// Receive the next text frame as JSON, skipping anything else.
async fn recv_frame(ws: &mut Ws) -> Value {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for a frame");
        let message = tokio::time::timeout(remaining, ws.next())
            .await
            .expect("frame arrives within the deadline")
            .expect("the stream is open")
            .expect("the frame is well-formed");
        if let Message::Text(text) = message {
            return serde_json::from_str(&text).expect("protocol frames are JSON");
        }
    }
}

/// Send a request frame and await its correlated response, skipping any
/// interleaved events.
async fn request(ws: &mut Ws, id: u64, method: &str, params: Value) -> Value {
    let frame = json!({"frame": "request", "id": id, "method": method, "params": params});
    ws.send(Message::Text(frame.to_string().into()))
        .await
        .expect("the frame sends");
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for response {id}");
        let frame = tokio::time::timeout(remaining, recv_frame(ws))
            .await
            .expect("response arrives within the deadline");
        if frame["frame"] == json!("response") && frame["id"] == json!(id) {
            return frame;
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// A deterministic Ed25519 device key: `(signing_key, public_key_hex)`.
fn device_key(seed: u8) -> (SigningKey, String) {
    let signing = SigningKey::from_bytes(&[seed; 32]);
    let public = hex_encode(signing.verifying_key().as_bytes());
    (signing, public)
}

/// The canonical challenge answer message — the wire contract the device
/// signs (`rusty-server/src/gateway_pairing.rs::pairing_message`).
fn sign_answer(signing: &SigningKey, device_id: &str, platform: &str, nonce: &str) -> String {
    let message = format!("rusty:pairing:v1\n{device_id}\n{platform}\n{nonce}");
    hex_encode(&signing.sign(message.as_bytes()).to_bytes())
}

/// Drive hello → answer up to the challenge, returning the nonce.
async fn hello(ws: &mut Ws, id: u64, device_id: &str, public_key: &str, platform: &str) -> String {
    let response = request(
        ws,
        id,
        "pairing_hello",
        json!({
            "device_id": device_id,
            "public_key": public_key,
            "platform": platform,
        }),
    )
    .await;
    let result = &response["ok"]["result"];
    assert_eq!(result["step"], json!("challenge"), "{response}");
    result["nonce"].as_str().expect("the nonce").to_string()
}

async fn answer(ws: &mut Ws, id: u64, device_id: &str, signature: &str) -> Value {
    request(
        ws,
        id,
        "pairing_answer",
        json!({"device_id": device_id, "signature": signature}),
    )
    .await
}

/// EP-04-S03 `pairing_happy_path_loopback`: hello → challenge → answer →
/// paired over loopback with auto-approve on; the token authenticates a
/// subsequent connection and appears in no snapshot.
#[tokio::test]
async fn pairing_happy_path_loopback() {
    let (addr, store) = spawn_loopback(true, Duration::from_secs(60)).await;
    let (signing, public) = device_key(1);

    let (mut ws, snapshot) = connect(addr, None).await;
    let nonce = hello(&mut ws, 1, "dev-loop", &public, "macos").await;
    let signature = sign_answer(&signing, "dev-loop", "macos", &nonce);
    let response = answer(&mut ws, 2, "dev-loop", &signature).await;
    let result = &response["ok"]["result"];
    assert_eq!(result["step"], json!("paired"), "{response}");
    let token = result["device_token"]
        .as_str()
        .expect("the token is issued")
        .to_string();
    assert_eq!(token.len(), 64, "256-bit token, hex-encoded");

    // The token authenticates a subsequent connection (AC4).
    let (mut second, _) = connect(addr, None).await;
    let response = request(
        &mut second,
        1,
        "device_attach",
        json!({"device_id": "dev-loop", "device_token": token}),
    )
    .await;
    assert_eq!(
        response["ok"]["result"],
        json!({"attached": "dev-loop"}),
        "{response}"
    );

    // Snapshot scan: the token appears in no snapshot (AC4) — not the
    // connect snapshot, and not a fresh one served on request.
    assert!(!snapshot.to_string().contains(&token));
    let _ = request(&mut second, 2, "resnapshot", json!({})).await;
    let fresh = loop {
        let frame = recv_frame(&mut second).await;
        if frame["name"] == json!("snapshot") {
            break frame;
        }
    };
    assert!(!fresh.to_string().contains(&token));

    let _ = std::fs::remove_dir_all(store);
}

/// EP-04-S03 negative tests: replayed nonce, expired nonce, and a
/// signature omitting the platform binding each deny with their distinct
/// reason.
#[tokio::test]
async fn pairing_denials_are_distinct() {
    let (addr, store) = spawn_loopback(true, Duration::from_millis(60)).await;
    let (signing, public) = device_key(2);
    let (mut ws, _) = connect(addr, None).await;

    // Expired: the answer lands after the challenge's bounded window.
    let nonce = hello(&mut ws, 1, "dev-neg", &public, "macos").await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    let response = answer(
        &mut ws,
        2,
        "dev-neg",
        &sign_answer(&signing, "dev-neg", "macos", &nonce),
    )
    .await;
    assert_eq!(
        response["err"]["error"],
        json!({"step": "denied", "reason": "challenge_expired"}),
        "{response}"
    );

    // Replayed: the consumed nonce never verifies again.
    let response = answer(
        &mut ws,
        3,
        "dev-neg",
        &sign_answer(&signing, "dev-neg", "macos", &nonce),
    )
    .await;
    assert_eq!(
        response["err"]["error"],
        json!({"step": "denied", "reason": "challenge_replayed"}),
        "{response}"
    );

    // Unpinned: a signature over the nonce alone omits the platform
    // binding and must not verify (AC2).
    let nonce = hello(&mut ws, 4, "dev-neg", &public, "macos").await;
    let nonce_only = hex_encode(&signing.sign(nonce.as_bytes()).to_bytes());
    let response = answer(&mut ws, 5, "dev-neg", &nonce_only).await;
    assert_eq!(
        response["err"]["error"],
        json!({"step": "denied", "reason": "invalid_signature"}),
        "{response}"
    );

    let _ = std::fs::remove_dir_all(store);
}

/// EP-04-S03 `remote_requires_operator`: over a non-loopback address the
/// verified device stays `pending` even with auto-approve policy on; an
/// under-scoped principal cannot approve; an operator-scoped approval
/// frame pairs the device.
#[tokio::test]
async fn remote_requires_operator() {
    let Some(ip) = non_loopback_ip() else {
        eprintln!("no non-loopback interface; plane-level coverage in gateway_pairing.rs");
        return;
    };
    let store = temp_store();
    let mut config = ServerConfig::new(SocketAddr::new(ip, 0), store.clone());
    // Auto-approve on: the point is that remote never qualifies anyway.
    config.pairing_auto_approve_loopback = true;
    config.api_keys = vec![
        ("device-key".to_string(), "default".to_string()),
        ("operator-key".to_string(), "default".to_string()),
    ];
    config
        .api_key_scopes
        .insert("device-key".to_string(), vec!["gateway:stream".to_string()]);
    config.api_key_scopes.insert(
        "operator-key".to_string(),
        vec!["gateway:stream".to_string(), "gateway:operator".to_string()],
    );
    let addr = spawn_server_on(ip, config).await;
    let (signing, public) = device_key(3);

    let (mut device, _) = connect(addr, Some("device-key")).await;
    let nonce = hello(&mut device, 1, "dev-remote", &public, "linux").await;
    let response = answer(
        &mut device,
        2,
        "dev-remote",
        &sign_answer(&signing, "dev-remote", "linux", &nonce),
    )
    .await;
    assert_eq!(
        response["ok"]["result"]["status"],
        json!("pending_operator_approval"),
        "{response}"
    );

    // An under-scoped principal cannot approve.
    let response = request(
        &mut device,
        3,
        "pairing_approve",
        json!({"device_id": "dev-remote"}),
    )
    .await;
    assert_eq!(
        response["err"]["error"]["error"],
        json!("unauthorized"),
        "{response}"
    );

    // An operator-scoped approval frame pairs the device and receives the
    // token once, on the approver's surface.
    let (mut operator, _) = connect(addr, Some("operator-key")).await;
    let response = request(
        &mut operator,
        1,
        "pairing_approve",
        json!({"device_id": "dev-remote", "actor": "maya"}),
    )
    .await;
    let result = &response["ok"]["result"];
    assert_eq!(result["step"], json!("paired"), "{response}");
    let token = result["device_token"].as_str().unwrap().to_string();

    // The issued token then authenticates the device's connection.
    let response = request(
        &mut device,
        4,
        "device_attach",
        json!({"device_id": "dev-remote", "device_token": token}),
    )
    .await;
    assert_eq!(
        response["ok"]["result"],
        json!({"attached": "dev-remote"}),
        "{response}"
    );

    let _ = std::fs::remove_dir_all(store);
}

/// EP-04-S03 `revocation_closes_and_rejects`: revoke mid-connection; the
/// attached socket closes and the token is refused on reconnect with the
/// typed `device_revoked` reason.
#[tokio::test]
async fn revocation_closes_and_rejects() {
    let (addr, store) = spawn_loopback(true, Duration::from_secs(60)).await;
    let (signing, public) = device_key(4);

    let (mut ws, _) = connect(addr, None).await;
    let nonce = hello(&mut ws, 1, "dev-rev", &public, "macos").await;
    let response = answer(
        &mut ws,
        2,
        "dev-rev",
        &sign_answer(&signing, "dev-rev", "macos", &nonce),
    )
    .await;
    let token = response["ok"]["result"]["device_token"]
        .as_str()
        .expect("paired")
        .to_string();
    let response = request(
        &mut ws,
        3,
        "device_attach",
        json!({"device_id": "dev-rev", "device_token": token}),
    )
    .await;
    assert!(response["ok"].is_object(), "{response}");

    // Open mode runs as the super-user, which carries the operator scope.
    let response = request(
        &mut ws,
        4,
        "pairing_revoke",
        json!({"device_id": "dev-rev"}),
    )
    .await;
    assert_eq!(
        response["ok"]["result"],
        json!({"revoked": "dev-rev"}),
        "{response}"
    );

    // The attached connection (this one) closes.
    let closed = tokio::time::timeout(TIMEOUT, async {
        while let Some(message) = ws.next().await {
            match message {
                Ok(Message::Close(_)) => return true,
                Ok(_) => {}
                Err(_) => return true,
            }
        }
        true
    })
    .await
    .expect("the revoked device's connection closes");
    assert!(closed);

    // Reconnect: the token is refused with the typed reason.
    let (mut ws, _) = connect(addr, None).await;
    let response = request(
        &mut ws,
        1,
        "device_attach",
        json!({"device_id": "dev-rev", "device_token": token}),
    )
    .await;
    assert_eq!(
        response["err"]["error"],
        json!({"step": "denied", "reason": "device_revoked"}),
        "{response}"
    );

    let _ = std::fs::remove_dir_all(store);
}
