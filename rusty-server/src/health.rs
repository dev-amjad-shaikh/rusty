//! Per-component health reporting (EP-10-S10 wave 1).
//!
//! Every long-running component exposes **liveness** (responding at all) and
//! **readiness** (able to serve now) as typed statuses on an aggregated
//! endpoint. There is no single global boolean that hides a degraded part
//! behind a green whole.
//!
//! The `GET /health` handler serves the report by pinging each component
//! and aggregating the results.

use std::sync::Arc;

use axum::extract::State as AxumState;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::routes::AppState;

/// The three possible health statuses for a component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    /// The component is healthy and ready to serve.
    Up,
    /// The component is responding but impaired.
    Degraded,
    /// The component is not responding or fundamentally broken.
    Down,
}

/// Health status of one named component.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentHealth {
    /// The component identifier.
    pub component: String,
    /// Current status.
    pub status: HealthStatus,
    /// Human-readable detail when not `Up`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The aggregate health report returned by `GET /health`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport {
    /// Overall status: `Up` only when every component is `Up`; `Degraded` when
    /// at least one component is `Degraded` and none are `Down`; `Down`
    /// otherwise.
    pub status: HealthStatus,
    /// Per-component results in deterministic (alphabetical) order.
    pub components: Vec<ComponentHealth>,
}

impl HealthReport {
    /// Build the report from a list of component healths.
    pub fn from_components(mut components: Vec<ComponentHealth>) -> Self {
        components.sort_by(|a, b| a.component.cmp(&b.component));
        let status = if components.iter().any(|c| c.status == HealthStatus::Down) {
            HealthStatus::Down
        } else if components
            .iter()
            .any(|c| c.status == HealthStatus::Degraded)
        {
            HealthStatus::Degraded
        } else {
            HealthStatus::Up
        };
        Self { status, components }
    }
}

/// `GET /health` — aggregated per-component health.
pub(crate) async fn health_check(
    AxumState(state): AxumState<Arc<AppState>>,
) -> Result<Json<HealthReport>, ApiError> {
    let mut components = Vec::new();

    // store — lightweight ping: list assistants (cheap on both backends).
    let store_status = match state.server_store.list_assistants().await {
        Ok(_) => ComponentHealth {
            component: "store".to_owned(),
            status: HealthStatus::Up,
            message: None,
        },
        Err(e) => ComponentHealth {
            component: "store".to_owned(),
            status: HealthStatus::Down,
            message: Some(format!("store ping failed: {e}")),
        },
    };
    components.push(store_status);

    // checkpointer — list a non-existent thread; a responsive backend
    // answers Ok(empty), a broken backend errors.
    let checkpointer_status = match state.checkpointer.list("__health_check__").await {
        Ok(_) => ComponentHealth {
            component: "checkpointer".to_owned(),
            status: HealthStatus::Up,
            message: None,
        },
        Err(e) => ComponentHealth {
            component: "checkpointer".to_owned(),
            status: HealthStatus::Down,
            message: Some(format!("checkpointer ping failed: {e}")),
        },
    };
    components.push(checkpointer_status);

    // broker — metadata list (cheap, no secrets unsealed).
    let broker_status = match state.broker.list("default").await {
        Ok(_) => ComponentHealth {
            component: "broker".to_owned(),
            status: HealthStatus::Up,
            message: None,
        },
        Err(e) => ComponentHealth {
            component: "broker".to_owned(),
            status: HealthStatus::Down,
            message: Some(format!("broker ping failed: {e}")),
        },
    };
    components.push(broker_status);

    // skills — always Up if the plane loaded at boot.
    components.push(ComponentHealth {
        component: "skills".to_owned(),
        status: HealthStatus::Up,
        message: None,
    });

    // connectors — list manifests.
    let connectors_status = match state.connectors.list_manifests("default").await {
        Ok(_) => ComponentHealth {
            component: "connectors".to_owned(),
            status: HealthStatus::Up,
            message: None,
        },
        Err(e) => ComponentHealth {
            component: "connectors".to_owned(),
            status: HealthStatus::Down,
            message: Some(format!("connectors ping failed: {e}")),
        },
    };
    components.push(connectors_status);

    // deployment — list environments through the store.
    let deployment_status = match state.server_store.list_environments("default").await {
        Ok(_) => ComponentHealth {
            component: "deployment".to_owned(),
            status: HealthStatus::Up,
            message: None,
        },
        Err(e) => ComponentHealth {
            component: "deployment".to_owned(),
            status: HealthStatus::Down,
            message: Some(format!("deployment ping failed: {e}")),
        },
    };
    components.push(deployment_status);

    // knowledge — list sources.
    let knowledge_status = match state.knowledge.all_sources("default").await {
        Ok(_) => ComponentHealth {
            component: "knowledge".to_owned(),
            status: HealthStatus::Up,
            message: None,
        },
        Err(e) => ComponentHealth {
            component: "knowledge".to_owned(),
            status: HealthStatus::Down,
            message: Some(format!("knowledge ping failed: {e}")),
        },
    };
    components.push(knowledge_status);

    // receipt_keyring — ping via store history read.
    let receipt_keyring_status = match state.server_store.list_receipt_keys().await {
        Ok(_) => ComponentHealth {
            component: "receipt_keyring".to_owned(),
            status: HealthStatus::Up,
            message: None,
        },
        Err(e) => ComponentHealth {
            component: "receipt_keyring".to_owned(),
            status: HealthStatus::Down,
            message: Some(format!("receipt keyring ping failed: {e}")),
        },
    };
    components.push(receipt_keyring_status);

    // artifact_retention — ping via artifact listing.
    let artifact_retention_status = match state.server_store.list_run_artifacts("default").await {
        Ok(_) => ComponentHealth {
            component: "artifact_retention".to_owned(),
            status: HealthStatus::Up,
            message: None,
        },
        Err(e) => ComponentHealth {
            component: "artifact_retention".to_owned(),
            status: HealthStatus::Down,
            message: Some(format!("artifact retention ping failed: {e}")),
        },
    };
    components.push(artifact_retention_status);

    // evaluation_state — structural Up (in-memory runtime, no cheap store ping).
    components.push(ComponentHealth {
        component: "evaluation_state".to_owned(),
        status: HealthStatus::Up,
        message: None,
    });

    Ok(Json(HealthReport::from_components(components)))
}
