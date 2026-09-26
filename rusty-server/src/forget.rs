//! Forgetting a person: everything kept in their name goes, and their key
//! with it.
//!
//! An account can be deleted and its sessions ended (Story 34); that keeps
//! the person's history for the deployment. Forgetting is the other
//! thing: the memory that was theirs is forgotten (tombstoned, the way
//! `/memory/forget_scope` does it), the connections granted in their
//! name are revoked, their conversations' state and records are removed,
//! the runs that acted for them — sealed under their key — are removed
//! from the box, and the key is destroyed, so any copy of those records
//! anywhere is ciphertext from now on. What remains is a tombstone: the
//! principal id, when, by whom, why. Approvals they decided keep their
//! attribution: an audit trail is not a person's record to take.
use std::sync::Arc;

use axum::extract::{Path, State as AxumState};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::Utc;
use rusty_agent_runtime::memory::{MemoryQuery, MemoryScope, ScopeAddress, plan_forget};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;

#[derive(Debug, Deserialize)]
pub struct ForgetPayload {
    pub reason: String,
}

/// `POST /users/{id}/forget {reason}` — an administrator forgets a person.
pub(crate) async fn forget_person(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Path(user_id): Path<String>,
    Json(payload): Json<ForgetPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let reason = payload.reason.trim().to_owned();
    if reason.is_empty() {
        return Err(ApiError::bad_request(
            "say why — the reason is what the tombstone keeps".to_owned(),
        ));
    }
    let person = user_id.trim().to_owned();
    if person.is_empty() {
        return Err(ApiError::bad_request("name the person".to_owned()));
    }
    if tenant.principal().id == person {
        return Err(ApiError::bad_request(
            "you cannot forget the account you are signed in as".to_owned(),
        ));
    }
    let tenant_id = tenant.tenant().to_owned();
    if state.vault.is_forgotten(&tenant_id, &person) {
        return Err(ApiError::conflict(format!(
            "`{person}` was already forgotten"
        )));
    }
    let account = state.users.get(&person);
    let name = account.as_ref().map(|u| u.name.clone());
    let external = account
        .as_ref()
        .and_then(|u| u.external.as_ref())
        .map(|e| (e.issuer.clone(), e.subject.clone()));

    // The account: sessions end, the record goes.
    let mut had_account = false;
    if account.is_some() {
        let _ = state.users.revoke_sessions(&person).await;
        had_account = state
            .users
            .delete(&person)
            .await
            .map_err(ApiError::internal)?;
    }
    state.users.mark_forgotten(&person);
    if let Some((issuer, subject)) = &external {
        state
            .users
            .mark_forgotten(&crate::users::external_key(issuer, subject));
    }

    // Memory that was theirs: forgotten with tombstones, the plan the
    // memory plane uses for a scope.
    let universe = state
        .server_store
        .query_memory(
            &tenant_id,
            &MemoryQuery {
                include_expired: true,
                include_superseded: true,
                include_candidates: true,
                ..MemoryQuery::default()
            },
            Utc::now(),
        )
        .await
        .map_err(ApiError::internal)?;
    let scope = ScopeAddress {
        scope: MemoryScope::User,
        id: person.clone(),
    };
    let targets: Vec<String> = universe
        .iter()
        .filter(|r| r.scope == scope)
        .map(|r| r.memory_id.clone())
        .collect();
    let plan = plan_forget(&universe, &targets);
    for memory_id in plan.forgotten.iter().chain(plan.invalidated.iter()) {
        state
            .server_store
            .delete_memory(&tenant_id, memory_id)
            .await
            .map_err(ApiError::internal)?;
    }

    // The curated blocks of every agent that name them — the `person`
    // block an agent keeps about whoever it talks to, or any other: the
    // lines that carry their id or name go, the block stays, a new
    // version in the platform's name over the old. Erasure reaches what
    // an agent wrote about them in its own notes, not only what was
    // filed under their scope.
    let mut blocks_scrubbed: Vec<Value> = Vec::new();
    let needles: Vec<String> = std::iter::once(person.clone())
        .chain(name.clone())
        .map(|n| n.trim().to_lowercase())
        .filter(|n| n.chars().count() >= 3)
        .collect();
    let names = |text: &str| {
        text.lines().any(|line| {
            let l = line.to_lowercase();
            needles.iter().any(|n| l.contains(n.as_str()))
        })
    };
    for assistant in state
        .server_store
        .list_assistants()
        .await
        .map_err(ApiError::internal)?
    {
        let Some(agent_id) = tenant.unscope(&assistant.assistant_id).map(str::to_owned) else {
            continue;
        };
        let live = crate::platform_tools::block_records(&state, &tenant_id, &agent_id)
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?;
        for block in live {
            let text = crate::platform_tools::block_value(&block);
            let label = block
                .key
                .as_deref()
                .and_then(|k| k.strip_prefix("block."))
                .unwrap_or("")
                .to_owned();
            let mut removed = 0usize;
            if names(&text) {
                let kept: Vec<&str> = text
                    .lines()
                    .filter(|line| {
                        let l = line.to_lowercase();
                        !needles.iter().any(|n| l.contains(n.as_str()))
                    })
                    .collect();
                removed = text.lines().count() - kept.len();
                crate::platform_tools::write_block(
                    &state,
                    &tenant_id,
                    &agent_id,
                    &label,
                    &kept.join("\n"),
                    rusty_agent_runtime::memory::ProvenanceAuthor::System,
                    Some(&block),
                    false,
                )
                .await
                .map_err(|e| ApiError::internal(e.to_string()))?;
            }
            // Every earlier version of the block that named them goes too:
            // a version the store keeps as evidence is a version a person
            // can read back and restore, and erasure admits no way back.
            let versions = universe.iter().filter(|r| {
                r.scope.scope == MemoryScope::Agent
                    && r.scope.id == agent_id
                    && r.key.as_deref() == Some(format!("block.{label}").as_str())
            });
            let mut versions_removed = 0usize;
            for version in versions {
                if names(&crate::platform_tools::block_value(version)) {
                    state
                        .server_store
                        .delete_memory(&tenant_id, &version.memory_id)
                        .await
                        .map_err(ApiError::internal)?;
                    versions_removed += 1;
                }
            }
            if removed > 0 || versions_removed > 0 {
                blocks_scrubbed.push(json!({"agent_id": agent_id, "label": label, "lines_removed": removed, "versions_removed": versions_removed}));
            }
        }
    }

    // Connections granted in their name.
    let mut connections_revoked = 0u64;
    if let Ok(connections) = state.broker.list(&tenant_id).await {
        for record in connections
            .into_iter()
            .filter(|c| c.subject.as_deref() == Some(person.as_str()))
        {
            if state
                .broker
                .revoke(
                    &tenant_id,
                    &record.connection_id,
                    Some(format!("forgotten: {reason}")),
                )
                .await
                .is_ok()
            {
                connections_revoked += 1;
            }
        }
    }

    // Their conversations: the state under the thread, and the thread.
    let mut threads_removed = 0u64;
    let mut wire_threads = Vec::new();
    for (internal_id, record) in state
        .server_store
        .list_threads()
        .await
        .map_err(ApiError::internal)?
    {
        if crate::auth::tenant_of_internal(&internal_id) != tenant_id {
            continue;
        }
        let theirs = crate::routes::run_person(Some(&record.metadata)) == Some(person.as_str())
            || state
                .thread_persons
                .read()
                .ok()
                .and_then(|m| m.get(&record.thread_id).cloned())
                .as_deref()
                == Some(person.as_str());
        if !theirs {
            continue;
        }
        let checkpoints = state.config.store_path.join(&internal_id);
        match tokio::fs::remove_dir_all(&checkpoints).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(ApiError::internal(format!(
                    "the state of thread {} could not be removed: {e}",
                    record.thread_id
                )));
            }
        }
        if state
            .server_store
            .delete_thread(&internal_id)
            .await
            .map_err(ApiError::internal)?
        {
            threads_removed += 1;
        }
        wire_threads.push(record.thread_id.clone());
    }
    if let Ok(mut map) = state.thread_persons.write() {
        map.retain(|_, who| who != &person);
    }

    // The runs that acted for them: the record and the journal, sealed or
    // not, leave the box.
    let mut runs_removed = 0u64;
    for record in state
        .server_store
        .list_accepted_runs()
        .await
        .map_err(ApiError::internal)?
    {
        if record.tenant != tenant_id
            || crate::routes::run_person(record.payload.metadata.as_ref()) != Some(person.as_str())
        {
            continue;
        }
        let _ = state.server_store.delete_journal(&record.run_id).await;
        let _ = state.run_deps.manager.forget(&record.run_id).await;
        if state
            .server_store
            .delete_accepted_run(&record.run_id)
            .await
            .map_err(ApiError::internal)?
        {
            runs_removed += 1;
        }
    }

    // Assignments they delegated.
    let mut assignments_removed = 0u64;
    for a in state
        .server_store
        .list_assignments(Some(&tenant_id))
        .await
        .map_err(ApiError::internal)?
    {
        if a.owner.get("principal_id").and_then(Value::as_str) == Some(person.as_str())
            && state
                .server_store
                .delete_assignment(&a.assignment_id)
                .await
                .map_err(ApiError::internal)?
        {
            assignments_removed += 1;
        }
    }

    // The key: destroyed. From here every sealed record of theirs, on the
    // box or in a data archive, is ciphertext without a key.
    let tombstone = state
        .vault
        .destroy(
            &tenant_id,
            &person,
            name,
            tenant.attribution(),
            &reason,
            external,
        )
        .map_err(|e| ApiError::internal(format!("the key could not be destroyed: {e}")))?;
    tracing::warn!(tenant = %tenant_id, principal = %person, by = %tenant.principal().id, %reason, "person forgotten");
    Ok((
        StatusCode::OK,
        Json(json!({
            "forgotten": tombstone,
            "had_account": had_account,
            "memory": {"forgotten": plan.forgotten.len(), "invalidated": plan.invalidated.len()},
            "blocks_scrubbed": blocks_scrubbed,
            "connections_revoked": connections_revoked,
            "threads_removed": threads_removed,
            "runs_removed": runs_removed,
            "assignments_removed": assignments_removed,
            "key_destroyed": true,
        })),
    ))
}
