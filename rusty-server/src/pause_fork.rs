//! Pause-state forking (EP-03-S09 AC 5): pause state forks with the
//! session.
//!
//! Forking a thread whose latest run is paused carries the suspension
//! into the fork **as data**: a paused run record under the fork's own
//! run id, copies of the open obligations, and the pause envelope —
//! everything rebound to the fork so the two sessions' pauses are
//! independent. Answering, expiring, or cancelling the parent's
//! obligations never touches the fork's copies, and vice versa:
//! obligations are keyed by run id and the copies carry fresh obligation
//! ids, so the store's id-addressed mutations cannot cross over.
//!
//! Only **open** obligations carry: a satisfied or rejected obligation is
//! a decision already made, and the fork re-asks nothing the parent
//! already settled. The envelope's embedded obligation list is replaced
//! with the rebound copies so a later resume of the fork reads one
//! consistent snapshot.

use std::sync::Arc;

use rusty_agent_runtime::record::{ObligationStatus, RunObligation};

use crate::runs::{RunManager, RunStatus};
use crate::server_store::{ServerStore, StoreResult};

/// What a fork carried from a paused source run.
pub(crate) struct ForkedPause {
    /// The fork's own run id — the paused run registered on the fork
    /// thread, and the key its obligation copies and envelope live under.
    pub run_id: String,
    /// How many open obligations were copied onto the fork.
    pub obligations_carried: usize,
}

/// Carry the source thread's pause state into a freshly forked thread.
///
/// `source_internal_thread` / `fork_internal_thread` are the
/// tenant-scoped ids the run registry keys by; `fork_wire_thread` is the
/// external id reported in the forked run's terminal JSON. Returns `None`
/// when the source thread has no paused run — the common case, an
/// ordinary fork of a finished session.
///
/// The source run is re-checked at registration time: a run that left
/// `Paused` between the listing and the fork (a resume or cancel landing
/// concurrently) is not forked — the fork then simply starts clean.
pub(crate) async fn fork_pause_state(
    store: &Arc<dyn ServerStore>,
    manager: &RunManager,
    source_internal_thread: &str,
    fork_internal_thread: &str,
    fork_wire_thread: &str,
    log_capacity: usize,
    shutdown: &tokio_util::sync::CancellationToken,
) -> StoreResult<Option<ForkedPause>> {
    // The latest paused run on the source thread, if any. A thread can
    // accumulate several paused runs over its lifetime; the newest one is
    // the suspension a fork observes.
    let source_run_id = {
        let mut paused: Vec<_> = manager
            .list()
            .await
            .into_iter()
            .filter(|(_, info)| {
                info.thread_id == source_internal_thread && info.status == RunStatus::Paused
            })
            .collect();
        paused.sort_by_key(|(_, info)| (info.created_at, info.attempt));
        paused.pop().map(|(run_id, _)| run_id)
    };
    let Some(source_run_id) = source_run_id else {
        return Ok(None);
    };

    // Read the pause data before registering anything, so a store error
    // leaves no half-carried fork behind.
    let open: Vec<RunObligation> = store
        .get_obligations(&source_run_id)
        .await?
        .into_iter()
        .filter(|o| o.status == ObligationStatus::Open)
        .map(|mut o| {
            // Fresh ids: obligation mutations are id-addressed across
            // runs, so a copy sharing the parent's id would let an answer
            // aimed at one session land on the other.
            o.id = uuid::Uuid::new_v4().to_string();
            o
        })
        .collect();
    let envelope = store.get_pause_envelope(&source_run_id).await?;

    let fork_run_id = uuid::Uuid::new_v4().to_string();
    let registered = manager
        .fork_paused(
            &source_run_id,
            fork_run_id.clone(),
            fork_internal_thread,
            fork_wire_thread,
            log_capacity,
            shutdown,
        )
        .await;
    if registered.is_none() {
        // The source left `Paused` between the listing and the fork; the
        // fork starts clean rather than carrying a stale suspension.
        return Ok(None);
    }

    if !open.is_empty() {
        store.put_obligations(&fork_run_id, &open).await?;
    }
    if let Some(mut envelope) = envelope {
        envelope.run_id = fork_run_id.clone();
        envelope.session_id = fork_internal_thread.to_string();
        envelope.obligations = open.clone();
        store.put_pause_envelope(&envelope).await?;
    }

    Ok(Some(ForkedPause {
        run_id: fork_run_id,
        obligations_carried: open.len(),
    }))
}

#[cfg(test)]
mod tests {
    //! `paused_fork_isolation` (EP-03-S09 test verification): fork a
    //! paused session over the real HTTP surface, answer the parent's
    //! obligation, assert the fork remains paused — and vice versa.
    //!
    //! The pause itself is staged through the `RunManager` + store
    //! directly: the executor's pause-obligation commit is EP-03-S11's
    //! wiring, not this story's. Everything under test — the fork route,
    //! the carry, the cancel route's obligation expiry, the status
    //! surface — is production code.

    use std::path::PathBuf;

    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use rusty_agent_runtime::prelude::*;
    use rusty_agent_runtime::record::{
        ObligationKind, ObligationStatus, PauseEnvelope, RunObligation,
    };
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use crate::routes::build_router;
    use crate::{GraphRegistry, ServerConfig};

    fn pipeline_graph() -> (Graph, StateSpec) {
        let spec = StateSpec::new().channel("log", Reducer::Append);
        let mut builder = GraphBuilder::new();
        builder.add_node("first", |_ctx: NodeContext| async {
            Ok(NodeOutput::update("log", json!("first")))
        });
        builder.add_node("second", |_ctx: NodeContext| async {
            Ok(NodeOutput::update("log", json!("second")))
        });
        builder.set_entry_point("first");
        builder.add_edge("first", "second");
        (builder.compile().unwrap(), spec)
    }

    fn temp_store() -> PathBuf {
        std::env::temp_dir().join(format!("rusty-pause-fork-test-{}", uuid::Uuid::new_v4()))
    }

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        let body = match body {
            Some(v) => {
                builder = builder.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let response = app
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    fn open_obligation(id: &str, tool_call_id: &str) -> RunObligation {
        RunObligation {
            id: id.into(),
            tool_call_id: Some(tool_call_id.into()),
            kind: ObligationKind::Approval {
                scope: "user".into(),
                sticky_allowed: false,
            },
            status: ObligationStatus::Open,
            expires_at: None,
            member_run_id: None,
        }
    }

    #[tokio::test]
    async fn paused_fork_isolation() {
        let store_dir = temp_store();
        let (graph, spec) = pipeline_graph();
        let mut registry = GraphRegistry::new();
        registry.register("pipeline", graph, spec);
        let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store_dir.clone());
        let parts = build_router(registry, config, tokio_util::sync::CancellationToken::new());
        let app = parts.router;

        // A thread with a completed turn (two checkpoints to fork).
        let (status, v) = call(&app, "POST", "/threads", Some(json!({"graph": "pipeline"}))).await;
        assert_eq!(status, StatusCode::CREATED, "thread creation failed: {v}");
        let thread = v["thread_id"].as_str().unwrap().to_string();
        let (status, v) = call(
            &app,
            "POST",
            &format!("/threads/{thread}/runs/wait"),
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "run/wait failed: {v}");
        assert_eq!(v["status"], json!("success"));

        // Stage the pause: the completed run goes paused with two open
        // obligations and a pause envelope, standing in for the
        // executor's pause commit (EP-03-S11's wiring).
        let runs = parts.run_manager.list().await;
        let run_id = runs
            .iter()
            .find(|(_, info)| info.thread_id == thread)
            .map(|(run_id, _)| run_id.clone())
            .expect("the thread has one run");
        parts
            .run_manager
            .finish(
                &run_id,
                crate::runs::RunStatus::Paused,
                json!({"run_id": run_id, "thread_id": thread, "status": "paused"}),
                false,
            )
            .await;
        let obligations = vec![
            open_obligation("obl-parent-1", "tc-1"),
            open_obligation("obl-parent-2", "tc-2"),
        ];
        parts
            .server_store
            .put_obligations(&run_id, &obligations)
            .await
            .unwrap();
        let mut envelope = PauseEnvelope::new(run_id.as_str(), thread.as_str(), 2, "chk-pause");
        envelope.obligations = obligations.clone();
        parts
            .server_store
            .put_pause_envelope(&envelope)
            .await
            .unwrap();

        // Fork the paused session over HTTP.
        let (status, v) = call(
            &app,
            "POST",
            &format!("/threads/{thread}/fork"),
            Some(json!({"new_thread_id": "fork-p"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "fork failed: {v}");
        assert_eq!(v["paused"], json!(true), "pause must fork with the session");
        let fork_run_id = v["pause_run_id"]
            .as_str()
            .expect("a paused fork names its own run id")
            .to_string();
        assert_eq!(v["obligations_carried"], json!(2));

        // The fork's run is paused, bound to the fork thread — visible
        // over the wire as well as in the registry.
        let info = parts.run_manager.info(&fork_run_id).await.unwrap();
        assert_eq!(info.status, crate::runs::RunStatus::Paused);
        assert_eq!(info.thread_id, "fork-p");
        let (status, v) = call(&app, "GET", &format!("/runs/{fork_run_id}"), None).await;
        assert_eq!(status, StatusCode::OK, "get fork run failed: {v}");
        assert_eq!(v["status"], json!("paused"));
        assert_eq!(v["thread_id"], json!("fork-p"));

        // The open obligations carried as copies: fresh ids, open status,
        // keyed under the fork's run id.
        let fork_obligations = parts
            .server_store
            .get_obligations(&fork_run_id)
            .await
            .unwrap();
        assert_eq!(fork_obligations.len(), 2);
        assert!(fork_obligations
            .iter()
            .all(|o| o.status == ObligationStatus::Open));
        assert!(
            fork_obligations
                .iter()
                .all(|o| o.id != "obl-parent-1" && o.id != "obl-parent-2"),
            "copies rebound under the fork must not share the parent's ids"
        );

        // The envelope rebound to the fork: its own run id, its own
        // session, its own obligation copies.
        let fork_envelope = parts
            .server_store
            .get_pause_envelope(&fork_run_id)
            .await
            .unwrap()
            .expect("the pause envelope forks with the session");
        assert_eq!(fork_envelope.run_id, fork_run_id);
        assert_eq!(fork_envelope.session_id, "fork-p");
        assert_eq!(fork_envelope.obligations.len(), 2);

        // Answering the parent's obligation does not resume the fork.
        let answered = parts
            .server_store
            .update_obligation_status("obl-parent-1", ObligationStatus::Satisfied)
            .await
            .unwrap();
        assert!(answered);
        let fork_after = parts
            .server_store
            .get_obligations(&fork_run_id)
            .await
            .unwrap();
        assert!(
            fork_after
                .iter()
                .all(|o| o.status == ObligationStatus::Open),
            "answering the parent must not touch the fork's copies"
        );
        assert_eq!(
            parts.run_manager.info(&fork_run_id).await.unwrap().status,
            crate::runs::RunStatus::Paused,
            "the fork remains paused"
        );

        // Vice versa: answering one of the fork's copies does not touch
        // the parent's remaining open obligation.
        let answered = parts
            .server_store
            .update_obligation_status(&fork_after[0].id, ObligationStatus::Satisfied)
            .await
            .unwrap();
        assert!(answered);
        let parent_after = parts.server_store.get_obligations(&run_id).await.unwrap();
        assert_eq!(
            parent_after[1].status,
            ObligationStatus::Open,
            "answering the fork must not touch the parent's obligations"
        );
        assert_eq!(
            parts.run_manager.info(&run_id).await.unwrap().status,
            crate::runs::RunStatus::Paused,
            "the parent remains paused"
        );

        // Cancelling the paused parent over HTTP expires only the
        // parent's obligations; the fork stays paused with its copy open.
        let (status, v) = call(&app, "POST", &format!("/runs/{run_id}/cancel"), None).await;
        assert_eq!(status, StatusCode::OK, "cancel failed: {v}");
        assert_eq!(v["obligations_expired"], json!(true));
        let parent_final = parts.server_store.get_obligations(&run_id).await.unwrap();
        assert_eq!(parent_final[1].status, ObligationStatus::Expired);
        let fork_final = parts
            .server_store
            .get_obligations(&fork_run_id)
            .await
            .unwrap();
        assert_eq!(fork_final[1].status, ObligationStatus::Open);
        assert_eq!(
            parts.run_manager.info(&fork_run_id).await.unwrap().status,
            crate::runs::RunStatus::Paused,
            "cancelling the parent must not resume or cancel the fork"
        );

        let _ = std::fs::remove_dir_all(store_dir);
    }

    #[tokio::test]
    async fn fork_without_pause_carries_nothing() {
        let store_dir = temp_store();
        let (graph, spec) = pipeline_graph();
        let mut registry = GraphRegistry::new();
        registry.register("pipeline", graph, spec);
        let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store_dir.clone());
        let parts = build_router(registry, config, tokio_util::sync::CancellationToken::new());
        let app = parts.router;

        let (status, v) = call(&app, "POST", "/threads", Some(json!({"graph": "pipeline"}))).await;
        assert_eq!(status, StatusCode::CREATED, "thread creation failed: {v}");
        let thread = v["thread_id"].as_str().unwrap().to_string();
        let (status, _) = call(
            &app,
            "POST",
            &format!("/threads/{thread}/runs/wait"),
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // An ordinary fork of a finished session reports no pause.
        let (status, v) = call(
            &app,
            "POST",
            &format!("/threads/{thread}/fork"),
            Some(json!({"new_thread_id": "fork-clean"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "fork failed: {v}");
        assert!(v.get("paused").is_none());
        assert!(v.get("pause_run_id").is_none());

        let _ = std::fs::remove_dir_all(store_dir);
    }
}
