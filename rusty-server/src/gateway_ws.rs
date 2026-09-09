//! The gateway WebSocket transport (EP-04-S01 residue): the wire surface
//! the schema pipeline (`crate::gateway_schema`) defines, carried over a
//! real socket.
//!
//! Discipline, per the story's acceptance criteria:
//!
//! - **AC1 — validate before dispatch.** Every inbound text frame is
//!   checked against the committed schema bundle's `Frame` schema
//!   (`schema/gateway-protocol.schema.json`, compiled in with
//!   `include_str!` so the validator can never drift from the artifact the
//!   drift gate pins) before it reaches a method handler. An invalid frame
//!   gets a typed `Frame::Response` error naming the schema violation;
//!   nothing invalid is dispatched.
//! - **AC2 — snapshot-on-connect, then monotonic `seq`.** A fresh
//!   connection receives one `snapshot` event (the tenant's sessions, run
//!   statuses, and open obligations) before any incremental event; every
//!   event carries a per-connection monotonic `seq`, assigned by the
//!   connection's single writer task so the sequence is strictly
//!   increasing by construction.
//! - **AC3 — re-snapshot on gap.** A client that detects a gap sends a
//!   `resnapshot` request; the gateway answers and serves a fresh snapshot
//!   with sequencing restarted. Server-side the same rule applies: when a
//!   connection's hub receiver lags (events were dropped before they could
//!   be sequenced), the gateway pushes a fresh snapshot and restarts
//!   sequencing rather than letting the client patch around the loss.
//! - **AC4 — named boundary events.** Run boundaries arrive as named
//!   `Event` frames — `run_admitted` (with the [`AdmissionReason`]
//!   verdict), `run_started`, `run_ended` (with the terminal status as the
//!   reason) — published at the exact scheduling branches in
//!   [`crate::runs`], never inferred from side effects. Incremental run
//!   traffic rides the same connection as `run_frame` events teed off the
//!   run's [`FrameSink`](crate::runs) — the same event source the SSE
//!   stream serves, not a parallel pipeline.
//!
//! Tenancy: the hub is process-wide, but every event carries its tenant
//! and each connection forwards only its own tenant's events; the snapshot
//! is built from tenant-owned runs only, so one tenant's traffic is
//! invisible to another's socket.
//!
//! Method surface (deliberately thin; the registry of side-effecting
//! methods lands with the stories that own them): `submit_turn` schedules
//! a run through the same admission path as `POST /threads/{id}/runs`, and
//! `resnapshot` is the AC3 control message. The pairing methods
//! (`pairing_hello` / `pairing_answer` / `pairing_approve` /
//! `pairing_revoke` / `device_attach`) are EP-04-S03's device-pairing
//! surface; [`crate::gateway_pairing`] owns the flow.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Extension, State as AxumState};
use axum::response::Response;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use rusty_api::gateway_protocol::{Frame, Pairing, ResponseOutcome};

use crate::auth::TenantContext;
use crate::gateway_pairing::{self, AnswerOutcome};
use crate::routes::AppState;
use crate::runs::SseFrame;

/// Inbound frame byte ceiling. The protocol's payloads are turn inputs and
/// control messages; anything beyond this is a transport-level refusal
/// (`invalid_frame`), not work. Axum's own message cap stays as the outer
/// backstop — this guard answers with a typed error instead of dropping
/// the connection.
pub(crate) const MAX_FRAME_BYTES: usize = 256 * 1024;

/// The snapshot event name: snapshot-on-connect and re-snapshot both
/// arrive as this frame, each restarting the connection's sequencing.
pub(crate) const EVENT_SNAPSHOT: &str = "snapshot";
/// Named run-boundary events (AC4), published from `crate::runs`.
pub(crate) const EVENT_RUN_ADMITTED: &str = "run_admitted";
/// See [`EVENT_RUN_ADMITTED`].
pub(crate) const EVENT_RUN_STARTED: &str = "run_started";
/// See [`EVENT_RUN_ADMITTED`].
pub(crate) const EVENT_RUN_ENDED: &str = "run_ended";
/// Incremental run traffic teed off the run's frame sink: the same frames
/// the SSE stream serves, as `{run_id, thread_id, event, run_seq, data}`.
pub(crate) const EVENT_RUN_FRAME: &str = "run_frame";

/// Hub broadcast capacity. A connection that falls this far behind has
/// lost events; its forwarder serves a fresh snapshot instead of a gapped
/// sequence (AC3, server side).
const HUB_CAPACITY: usize = 1024;

/// One gateway event on the hub: named per the contract vocabulary,
/// tenant-tagged so connections forward only their own traffic.
#[derive(Debug, Clone)]
pub(crate) struct GatewayEvent {
    tenant: String,
    name: &'static str,
    payload: Value,
}

/// The process-wide gateway event bus. Run lifecycle branches and frame
/// sinks publish; each WS connection subscribes and filters by tenant.
/// Cheap to clone (a broadcast sender); with no subscribers a publish is a
/// dropped send, so background runs pay nothing.
#[derive(Clone)]
pub(crate) struct GatewayHub {
    tx: broadcast::Sender<GatewayEvent>,
}

impl GatewayHub {
    pub(crate) fn new() -> Self {
        let (tx, _rx) = broadcast::channel(HUB_CAPACITY);
        Self { tx }
    }

    /// Publish one named event for one tenant. No live connections is the
    /// common case (SSE-only deployments) and is not an error.
    pub(crate) fn publish(&self, tenant: &str, name: &'static str, payload: Value) {
        let _ = self.tx.send(GatewayEvent {
            tenant: tenant.to_string(),
            name,
            payload,
        });
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<GatewayEvent> {
        self.tx.subscribe()
    }
}

/// The tee a run's [`FrameSink`](crate::runs) holds: every frame the run
/// pushes (the same frames its SSE stream serves) also fans out to gateway
/// connections as a [`EVENT_RUN_FRAME`] event. Constructed at schedule
/// time, when the run's identity and tenant are known.
#[derive(Clone)]
pub(crate) struct GatewayTee {
    hub: GatewayHub,
    run_id: String,
    wire_thread_id: String,
    tenant: String,
}

impl GatewayTee {
    pub(crate) fn new(
        hub: GatewayHub,
        run_id: String,
        wire_thread_id: String,
        tenant: String,
    ) -> Self {
        Self {
            hub,
            run_id,
            wire_thread_id,
            tenant,
        }
    }

    /// Forward one pushed frame to the hub.
    pub(crate) fn forward(&self, frame: &SseFrame) {
        self.hub.publish(
            &self.tenant,
            EVENT_RUN_FRAME,
            json!({
                "run_id": self.run_id,
                "thread_id": self.wire_thread_id,
                "event": frame.event,
                "run_seq": frame.seq,
                "data": frame.data,
            }),
        );
    }
}

/// The compiled `Frame` validators over the committed schema artifact —
/// built once per process from the exact bytes the drift gate pins. The
/// whole-frame validator decides validity; the per-tag branch validators
/// refine a rejection into the violation's actual name (a bare `oneOf`
/// failure does not name the missing property — AC1 requires the refusal
/// to name the schema violation).
struct FrameSchemas {
    whole: jsonschema::Validator,
    /// One validator per `frame` tag (`request` / `response` / `event`).
    branches: Vec<(String, jsonschema::Validator)>,
}

fn frame_schemas() -> &'static FrameSchemas {
    static SCHEMAS: OnceLock<FrameSchemas> = OnceLock::new();
    SCHEMAS.get_or_init(|| {
        let bundle: Value =
            serde_json::from_str(include_str!("../schema/gateway-protocol.schema.json"))
                .expect("the committed gateway schema bundle is JSON");
        let schema = bundle
            .get("types")
            .and_then(|types| types.get("Frame"))
            .expect("the committed bundle carries the Frame schema");
        let whole = jsonschema::draft7::new(schema).expect("the committed Frame schema compiles");
        let branches = schema["oneOf"]
            .as_array()
            .expect("the Frame schema is the tagged-union oneOf")
            .iter()
            .map(|branch| {
                let tag = branch["properties"]["frame"]["enum"][0]
                    .as_str()
                    .expect("each branch fixes its tag")
                    .to_string();
                let validator =
                    jsonschema::draft7::new(branch).expect("each Frame branch compiles");
                (tag, validator)
            })
            .collect();
        FrameSchemas { whole, branches }
    })
}

/// Render a rejection naming the actual violation: when the tag is known,
/// the branch validator's error names the offending property; when the tag
/// itself is unknown, say so against the closed tag set.
fn violation_message(value: &Value, error: &jsonschema::ValidationError) -> String {
    if let Some(tag) = value.get("frame").and_then(Value::as_str) {
        match frame_schemas()
            .branches
            .iter()
            .find(|(name, _)| name == tag)
        {
            Some((_, branch)) => {
                if let Err(detail) = branch.validate(value) {
                    return format!(
                        "frame fails the `{tag}` branch of the Frame schema: {}: {detail}",
                        detail.instance_path()
                    );
                }
            }
            None => {
                let known: Vec<&str> = frame_schemas()
                    .branches
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect();
                return format!("unknown frame tag `{tag}` (expected one of {known:?})");
            }
        }
    }
    format!(
        "frame fails the `Frame` schema: {}: {error}",
        error.instance_path()
    )
}

/// `GET /gateway/ws` — upgrade to the gateway protocol transport. Mounted
/// inside the authenticated router, so the API-key and scope middleware
/// run before the upgrade exactly as for any other route. The peer address
/// rides along: pairing policy keys on loopback vs remote (EP-04-S03 AC3).
pub(crate) async fn gateway_ws(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ws: WebSocketUpgrade,
) -> Response {
    let is_loopback = peer.ip().is_loopback();
    ws.on_upgrade(move |socket| serve_connection(state, tenant, socket, is_loopback))
}

/// Per-connection context the pairing surface keys on: whether the peer
/// is loopback (EP-04-S03 AC3) and the close handle revocation fires to
/// end this socket (AC5). Attaching a paired device registers the close
/// handle with the pairing plane.
struct ConnContext {
    is_loopback: bool,
    conn_close: CancellationToken,
}

impl ConnContext {
    fn new(is_loopback: bool) -> Self {
        Self {
            is_loopback,
            conn_close: CancellationToken::new(),
        }
    }
}

/// Everything one connection can send. All outbound frames flow through a
/// single queue to a single writer task, which owns the `seq` counter —
/// per-connection monotonicity by construction, and a snapshot's
/// sequencing restart is one flag on the frame, not a second writer.
enum Outbound {
    /// The answer to a `Request`, by correlation id (responses carry no
    /// `seq` — sequencing is the event stream's).
    Response { id: u64, outcome: ResponseOutcome },
    /// A server-pushed event. `restart` marks snapshot frames: the writer
    /// resets the counter first, so the snapshot is always `seq` 1 and the
    /// restart is visible on the wire (AC3's documented behavior).
    Event {
        name: String,
        payload: Value,
        restart: bool,
    },
}

async fn serve_connection(
    state: Arc<AppState>,
    tenant: TenantContext,
    socket: WebSocket,
    is_loopback: bool,
) {
    let ctx = ConnContext::new(is_loopback);
    let (sink, stream) = socket.split();
    let (out_tx, out_rx) = mpsc::channel::<Outbound>(256);
    let writer = tokio::spawn(write_loop(sink, out_rx));

    // Snapshot-on-connect (AC2): queued before the hub forwarder starts,
    // so no incremental event can overtake it on this connection.
    let snapshot = snapshot_payload(&state, &tenant).await;
    if out_tx
        .send(Outbound::Event {
            name: EVENT_SNAPSHOT.to_string(),
            payload: snapshot,
            restart: true,
        })
        .await
        .is_err()
    {
        writer.abort();
        return;
    }

    let forwarder = tokio::spawn(forward_hub(
        Arc::clone(&state),
        tenant.clone(),
        out_tx.clone(),
    ));
    read_loop(&state, &tenant, stream, &out_tx, &ctx).await;
    forwarder.abort();
    // Drain before teardown: queued frames (a revocation acknowledgement,
    // above all) must reach the wire even when revocation itself ended
    // the read loop. Dropping the last sender lets the writer finish its
    // queue and exit; a broken socket ends it via send error as before.
    drop(out_tx);
    let _ = writer.await;
}

/// The connection's single writer: owns `seq`, serializes frames, drives
/// the socket. Ends when every sender has dropped or the socket errors.
async fn write_loop(mut sink: SplitSink<WebSocket, Message>, mut rx: mpsc::Receiver<Outbound>) {
    let mut seq = 0u64;
    while let Some(outbound) = rx.recv().await {
        let frame = match outbound {
            Outbound::Response { id, outcome } => Frame::Response { id, outcome },
            Outbound::Event {
                name,
                payload,
                restart,
            } => {
                if restart {
                    seq = 0;
                }
                seq += 1;
                Frame::Event { seq, name, payload }
            }
        };
        let text = serde_json::to_string(&frame).expect("a protocol frame serializes");
        if sink.send(Message::Text(text.into())).await.is_err() {
            break;
        }
    }
}

/// Forward the tenant's hub events into the connection's outbound queue.
/// A lagged receiver means events were dropped before sequencing — the
/// honest answer is a fresh snapshot with restarted sequencing (AC3),
/// never a silently gapped stream.
async fn forward_hub(state: Arc<AppState>, tenant: TenantContext, tx: mpsc::Sender<Outbound>) {
    let mut rx = state.run_deps.gateway.subscribe();
    loop {
        match rx.recv().await {
            Ok(event) => {
                if event.tenant != tenant.tenant() {
                    continue;
                }
                let send = tx
                    .send(Outbound::Event {
                        name: event.name.to_string(),
                        payload: event.payload,
                        restart: false,
                    })
                    .await;
                if send.is_err() {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    skipped,
                    "gateway WS connection lagged the hub; serving a fresh snapshot"
                );
                let snapshot = snapshot_payload(&state, &tenant).await;
                let send = tx
                    .send(Outbound::Event {
                        name: EVENT_SNAPSHOT.to_string(),
                        payload: snapshot,
                        restart: true,
                    })
                    .await;
                if send.is_err() {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Read inbound frames until close, error, or revocation of the device
/// this connection attached as (the close token fires; AC5). Axum answers
/// pings itself; binary frames are not part of the protocol and earn a
/// typed refusal.
async fn read_loop(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    mut stream: SplitStream<WebSocket>,
    tx: &mpsc::Sender<Outbound>,
    ctx: &ConnContext,
) {
    loop {
        let message = tokio::select! {
            () = ctx.conn_close.cancelled() => break,
            message = stream.next() => match message {
                Some(message) => message,
                None => break,
            },
        };
        match message {
            Ok(Message::Text(text)) => {
                handle_text(state, tenant, &text, tx, ctx).await
            }
            Ok(Message::Binary(_)) => {
                send_error(
                    tx,
                    0,
                    "invalid_frame",
                    "binary frames are not part of the gateway protocol; frames are JSON text",
                )
                .await;
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(error) => {
                tracing::debug!(%error, "gateway WS receive ended");
                break;
            }
        }
    }
}

/// Validate one inbound text frame against the `Frame` schema, then
/// dispatch. Nothing invalid reaches a method handler (AC1).
async fn handle_text(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    text: &str,
    tx: &mpsc::Sender<Outbound>,
    ctx: &ConnContext,
) {
    if text.len() > MAX_FRAME_BYTES {
        send_error(
            tx,
            0,
            "invalid_frame",
            format!("frame exceeds the {MAX_FRAME_BYTES}-byte protocol ceiling"),
        )
        .await;
        return;
    }
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            send_error(
                tx,
                0,
                "invalid_frame",
                format!("frame is not JSON: {error}"),
            )
            .await;
            return;
        }
    };
    // Echo the correlation id when the bytes carry one, so the client can
    // match the refusal to its request even when the frame is malformed.
    let id = value.get("id").and_then(Value::as_u64).unwrap_or(0);
    if let Err(error) = frame_schemas().whole.validate(&value) {
        send_error(tx, id, "invalid_frame", violation_message(&value, &error)).await;
        return;
    }
    // The schema is generated from the type, so a validated frame always
    // deserializes; a failure here is a pipeline bug, surfaced loudly.
    let frame: Frame = match serde_json::from_value(value) {
        Ok(frame) => frame,
        Err(error) => {
            send_error(
                tx,
                id,
                "invalid_frame",
                format!("frame validated but did not decode (schema drift): {error}"),
            )
            .await;
            return;
        }
    };
    match frame {
        Frame::Request {
            id, method, params, ..
        } => dispatch(state, tenant, id, &method, params, tx, ctx).await,
        Frame::Response { id, .. } => {
            send_error(
                tx,
                id,
                "unexpected_frame",
                "clients send request frames; responses and events flow server-to-client",
            )
            .await;
        }
        Frame::Event { .. } => {
            send_error(
                tx,
                0,
                "unexpected_frame",
                "clients send request frames; responses and events flow server-to-client",
            )
            .await;
        }
    }
}

/// The `submit_turn` params: the target thread plus the ordinary run
/// payload — the same admission surface as `POST /threads/{id}/runs`.
#[derive(Debug, Deserialize)]
struct SubmitTurnParams {
    thread_id: String,
    #[serde(flatten)]
    payload: crate::runs::RunPayload,
}

/// `pairing_hello` params: the device's introduction (`Pairing::Hello`
/// without the step tag).
#[derive(Debug, Deserialize)]
struct PairingHelloParams {
    device_id: String,
    public_key: String,
    platform: String,
}

/// `pairing_answer` params: the signed challenge (`Pairing::Answer`
/// without the step tag).
#[derive(Debug, Deserialize)]
struct PairingAnswerParams {
    device_id: String,
    signature: String,
}

/// `pairing_approve` / `pairing_revoke` params: the target device plus an
/// optional actor label for the audit trail (default: the operator's
/// tenant identity — the attribution the middleware can prove).
#[derive(Debug, Deserialize)]
struct PairingOperatorParams {
    device_id: String,
    actor: Option<String>,
}

/// `device_attach` params: a paired device authenticating this connection
/// with its token (EP-04-S03 AC4).
#[derive(Debug, Deserialize)]
struct DeviceAttachParams {
    device_id: String,
    device_token: String,
}

/// Queue a pairing refusal: the error payload is the contract's
/// `Pairing::Denied` shape verbatim.
async fn send_denied(tx: &mpsc::Sender<Outbound>, id: u64, reason: &str) {
    send_response(
        tx,
        id,
        ResponseOutcome::Err {
            error: gateway_pairing::denied_payload(reason),
        },
    )
    .await;
}

/// The pairing method surface (EP-04-S03), dispatched from [`dispatch`].
/// Returns `true` when the method was one of these.
async fn dispatch_pairing(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    id: u64,
    method: &str,
    params: Value,
    tx: &mpsc::Sender<Outbound>,
    ctx: &ConnContext,
) -> bool {
    match method {
        "pairing_hello" => match serde_json::from_value::<PairingHelloParams>(params) {
            Ok(parsed) => {
                match state
                    .pairing
                    .hello(
                        tenant,
                        &parsed.device_id,
                        &parsed.public_key,
                        &parsed.platform,
                    )
                    .await
                {
                    Ok(nonce) => {
                        send_response(
                            tx,
                            id,
                            ResponseOutcome::Ok {
                                result: serde_json::to_value(Pairing::Challenge { nonce })
                                    .expect("Pairing::Challenge serializes"),
                            },
                        )
                        .await;
                    }
                    Err(reason) => send_denied(tx, id, &reason).await,
                }
            }
            Err(error) => {
                send_error(tx, id, "invalid_params", format!("pairing_hello: {error}")).await;
            }
        },
        "pairing_answer" => match serde_json::from_value::<PairingAnswerParams>(params) {
            Ok(parsed) => {
                match state
                    .pairing
                    .answer(tenant, &parsed.device_id, &parsed.signature, ctx.is_loopback)
                    .await
                {
                    Ok(AnswerOutcome::Paired(device_token)) => {
                        send_response(
                            tx,
                            id,
                            ResponseOutcome::Ok {
                                result: serde_json::to_value(Pairing::Paired { device_token })
                                    .expect("Pairing::Paired serializes"),
                            },
                        )
                        .await;
                    }
                    Ok(AnswerOutcome::PendingApproval) => {
                        send_response(
                            tx,
                            id,
                            ResponseOutcome::Ok {
                                result: json!({
                                    "step": "answer",
                                    "status": "pending_operator_approval",
                                    "device_id": parsed.device_id,
                                }),
                            },
                        )
                        .await;
                    }
                    Ok(AnswerOutcome::Denied(reason)) => send_denied(tx, id, &reason).await,
                    Err(reason) => send_denied(tx, id, &reason).await,
                }
            }
            Err(error) => {
                send_error(tx, id, "invalid_params", format!("pairing_answer: {error}")).await;
            }
        },
        "pairing_approve" => match serde_json::from_value::<PairingOperatorParams>(params) {
            Ok(parsed) => {
                if !gateway_pairing::is_operator(tenant) {
                    send_error(
                        tx,
                        id,
                        "unauthorized",
                        format!(
                            "pairing_approve requires the `{}` scope",
                            gateway_pairing::OPERATOR_SCOPE
                        ),
                    )
                    .await;
                    return true;
                }
                let actor = operator_actor(tenant, parsed.actor.as_deref());
                match state
                    .pairing
                    .approve(tenant, &parsed.device_id, &actor)
                    .await
                {
                    Ok(device_token) => {
                        // The token is delivered once, to the approver's
                        // surface; it never appears in snapshots or events.
                        send_response(
                            tx,
                            id,
                            ResponseOutcome::Ok {
                                result: serde_json::to_value(Pairing::Paired { device_token })
                                    .expect("Pairing::Paired serializes"),
                            },
                        )
                        .await;
                    }
                    Err(reason) => send_denied(tx, id, &reason).await,
                }
            }
            Err(error) => {
                send_error(tx, id, "invalid_params", format!("pairing_approve: {error}")).await;
            }
        },
        "pairing_revoke" => match serde_json::from_value::<PairingOperatorParams>(params) {
            Ok(parsed) => {
                if !gateway_pairing::is_operator(tenant) {
                    send_error(
                        tx,
                        id,
                        "unauthorized",
                        format!(
                            "pairing_revoke requires the `{}` scope",
                            gateway_pairing::OPERATOR_SCOPE
                        ),
                    )
                    .await;
                    return true;
                }
                let actor = operator_actor(tenant, parsed.actor.as_deref());
                match state
                    .pairing
                    .revoke(tenant, &parsed.device_id, &actor)
                    .await
                {
                    Ok(()) => {
                        send_response(
                            tx,
                            id,
                            ResponseOutcome::Ok {
                                result: json!({"revoked": parsed.device_id}),
                            },
                        )
                        .await;
                    }
                    Err(reason) => send_denied(tx, id, &reason).await,
                }
            }
            Err(error) => {
                send_error(tx, id, "invalid_params", format!("pairing_revoke: {error}")).await;
            }
        },
        "device_attach" => match serde_json::from_value::<DeviceAttachParams>(params) {
            Ok(parsed) => {
                match state
                    .pairing
                    .attach(
                        tenant,
                        &parsed.device_id,
                        &parsed.device_token,
                        ctx.conn_close.clone(),
                    )
                    .await
                {
                    Ok(()) => {
                        send_response(
                            tx,
                            id,
                            ResponseOutcome::Ok {
                                result: json!({"attached": parsed.device_id}),
                            },
                        )
                        .await;
                    }
                    Err(reason) => send_denied(tx, id, &reason).await,
                }
            }
            Err(error) => {
                send_error(tx, id, "invalid_params", format!("device_attach: {error}")).await;
            }
        },
        _ => return false,
    }
    true
}

/// The actor label recorded for operator actions: the caller's explicit
/// label, else the tenant identity the auth middleware resolved.
fn operator_actor(tenant: &TenantContext, actor: Option<&str>) -> String {
    actor
        .map(str::to_string)
        .unwrap_or_else(|| format!("operator@{}", tenant.tenant()))
}

/// Route one validated request to its method.
async fn dispatch(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    id: u64,
    method: &str,
    params: Value,
    tx: &mpsc::Sender<Outbound>,
    ctx: &ConnContext,
) {
    if dispatch_pairing(state, tenant, id, method, params.clone(), tx, ctx).await {
        return;
    }
    match method {
        "submit_turn" => match serde_json::from_value::<SubmitTurnParams>(params) {
            Ok(parsed) => {
                match crate::routes::schedule_for_thread(
                    state,
                    tenant,
                    &parsed.thread_id,
                    parsed.payload,
                )
                .await
                {
                    Ok(scheduled) => {
                        send_response(
                            tx,
                            id,
                            ResponseOutcome::Ok {
                                result: json!({
                                    "run_id": scheduled.run_id,
                                    "thread_id": parsed.thread_id,
                                    "status": scheduled.status.as_str(),
                                }),
                            },
                        )
                        .await;
                    }
                    // The refusal travels in the same `{error, message}`
                    // envelope the REST surface answers with.
                    Err(error) => {
                        send_response(
                            tx,
                            id,
                            ResponseOutcome::Err {
                                error: error.into_body(),
                            },
                        )
                        .await;
                    }
                }
            }
            Err(error) => {
                send_error(tx, id, "invalid_params", format!("submit_turn: {error}")).await;
            }
        },
        // AC3: answer the request, then serve a fresh snapshot with
        // sequencing restarted.
        "resnapshot" => {
            let snapshot = snapshot_payload(state, tenant).await;
            send_response(
                tx,
                id,
                ResponseOutcome::Ok {
                    result: json!({"resnapshotted": true}),
                },
            )
            .await;
            let _ = tx
                .send(Outbound::Event {
                    name: EVENT_SNAPSHOT.to_string(),
                    payload: snapshot,
                    restart: true,
                })
                .await;
        }
        other => {
            send_error(
                tx,
                id,
                "unknown_method",
                format!("no gateway method `{other}`"),
            )
            .await;
        }
    }
}

/// The snapshot payload (AC2): the tenant's sessions (threads with runs
/// this process holds), the runs themselves with their statuses, and the
/// open obligations attached to those runs — everything the client's
/// scopes may see, before any incremental event.
pub(crate) async fn snapshot_payload(state: &AppState, tenant: &TenantContext) -> Value {
    let mut sessions = serde_json::Map::new();
    let mut runs = Vec::new();
    let mut obligations = Vec::new();
    for (run_id, info) in state.run_deps.manager.list().await {
        if !tenant.owns(&info.thread_id) {
            continue;
        }
        let session = sessions
            .entry(info.wire_thread_id.clone())
            .or_insert_with(|| {
                json!({
                    "thread_id": info.wire_thread_id,
                    "graph": info.graph,
                    "run_ids": [],
                })
            });
        session["run_ids"]
            .as_array_mut()
            .expect("the session shape is built above")
            .push(json!(run_id));
        runs.push(json!({
            "run_id": run_id,
            "thread_id": info.wire_thread_id,
            "graph": info.graph,
            "status": info.status.as_str(),
        }));
        match state.server_store.list_obligations(Some(&run_id)).await {
            Ok(run_obligations) => {
                obligations.extend(run_obligations.into_iter().filter_map(|o| {
                    matches!(
                        o.status,
                        rusty_agent_runtime::record::ObligationStatus::Open
                    )
                    .then(|| {
                        json!({
                            "obligation_id": o.id,
                            "run_id": run_id,
                            "kind": o.kind,
                            "expires_at": o.expires_at,
                        })
                    })
                }));
            }
            Err(error) => {
                // The snapshot is best-effort current state; a store read
                // failure narrows it, it does not fail the connection.
                tracing::warn!(%run_id, %error, "snapshot obligation read failed");
            }
        }
    }
    json!({
        "kind": EVENT_SNAPSHOT,
        "sessions": Value::Array(sessions.into_iter().map(|(_, v)| v).collect()),
        "runs": runs,
        "obligations": obligations,
    })
}

/// Queue a typed error response (`{error, message}` inside
/// `ResponseOutcome::Err`).
async fn send_error(tx: &mpsc::Sender<Outbound>, id: u64, kind: &str, message: impl Into<String>) {
    send_response(
        tx,
        id,
        ResponseOutcome::Err {
            error: json!({"error": kind, "message": message.into()}),
        },
    )
    .await;
}

/// Queue a response frame. A full queue means the connection is wedged;
/// the writer's next send error (or the client's close) ends it, so a
/// dropped enqueue only accelerates teardown.
async fn send_response(tx: &mpsc::Sender<Outbound>, id: u64, outcome: ResponseOutcome) {
    let _ = tx.send(Outbound::Response { id, outcome }).await;
}
