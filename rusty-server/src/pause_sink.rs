//! The server's [`PauseSink`] (EP-03-S11): the executor's governed-pause
//! commit lands here, persisting obligations and the pause envelope through
//! the [`ServerStore`] — the same rows the expiry sweep, the cancel route,
//! and `GET /v1/approvals` already read. One commit, one consistent
//! snapshot: obligations first, then the envelope that embeds them.

use std::sync::Arc;

use rusty_agent_runtime::error::{Result, RustyError};
use rusty_agent_runtime::pause::{PauseCommit, PauseSink};

use crate::server_store::ServerStore;

/// Commits governed pauses into the server store. Constructed once per run
/// by the run driver; stateless beyond the store handle.
pub(crate) struct ServerPauseSink {
    store: Arc<dyn ServerStore>,
}

impl ServerPauseSink {
    pub(crate) fn new(store: Arc<dyn ServerStore>) -> Self {
        Self { store }
    }
}

/// Store errors surface as checkpoint-class failures: a pause that cannot
/// persist its obligations must fail the run loudly, never degrade into an
/// unregistered suspension.
fn store_err(detail: String) -> RustyError {
    RustyError::Checkpoint(format!("pause commit failed: {detail}"))
}

#[async_trait::async_trait]
impl PauseSink for ServerPauseSink {
    async fn commit_pause(&self, commit: PauseCommit) -> Result<()> {
        if !commit.envelope.obligations.is_empty() {
            self.store
                .put_obligations(&commit.envelope.run_id, &commit.envelope.obligations)
                .await
                .map_err(store_err)?;
        }
        self.store
            .put_pause_envelope(&commit.envelope)
            .await
            .map_err(store_err)?;
        Ok(())
    }
}
