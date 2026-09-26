//! A budget that follows a chain of agent-started work: the run that starts
//! the chain is its root, every task queued and every assignment round run
//! downstream carries the root and the cap, each such run adds what it
//! spent to the chain's ledger as it ends, and nothing more starts from the
//! chain once the cap is spent — the tools refuse with the words, a round
//! waits for a person. The cap is the delegating agent's (`chain_max_tokens`
//! on its working copy), or the platform's default.
use serde_json::Value;

use crate::server_store::ServerStore;

const NAMESPACE: &str = "chain_spend";
/// Tokens, all the delegated rounds and queued tasks of one chain together.
pub const CHAIN_MAX_TOKENS_DEFAULT: u64 = 400_000;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ChainSpend {
    pub spent_tokens: u64,
    pub runs: u64,
}

pub async fn read(store: &dyn ServerStore, root_run_id: &str) -> ChainSpend {
    store
        .kv_get(NAMESPACE, root_run_id)
        .await
        .ok()
        .flatten()
        .and_then(|item| serde_json::from_value(item.value).ok())
        .unwrap_or_default()
}

/// The cap an agent's configuration sets on the work it starts.
pub fn cap_of(config: &Value) -> u64 {
    config
        .pointer("/studio_intent/chain_max_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(CHAIN_MAX_TOKENS_DEFAULT)
}

/// The root and the cap of the chain a run sits in — the chain it carries,
/// or, when it starts one, itself with its agent's cap.
pub fn chain_of(
    carried: Option<&Value>,
    run_id: &str,
    agent_config: Option<&Value>,
) -> (String, u64) {
    match carried.and_then(|c| c.get("root_run_id").and_then(Value::as_str)) {
        Some(root) => (
            root.to_owned(),
            carried
                .and_then(|c| c.get("max_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(CHAIN_MAX_TOKENS_DEFAULT),
        ),
        None => (
            run_id.to_owned(),
            agent_config.map(cap_of).unwrap_or(CHAIN_MAX_TOKENS_DEFAULT),
        ),
    }
}

/// Whether the chain has spent its cap.
pub fn spent(spend: &ChainSpend, cap: u64) -> bool {
    spend.spent_tokens >= cap
}

pub fn words(spend: &ChainSpend, cap: u64) -> String {
    if cap == 0 {
        return "this agent may start no work: its cap on the work it starts is 0 tokens — do the work here or say what is left for a person, who can raise the cap on the agent (Model & behavior → work it starts may spend)".to_owned();
    }
    format!(
        "the chain's token budget is spent: {} of {} tokens across {} run{} of work this chain started; nothing more starts from it — say what is left for a person, who can raise the cap on the delegating agent (Model & behavior → work it starts may spend)",
        spend.spent_tokens,
        cap,
        spend.runs,
        if spend.runs == 1 { "" } else { "s" }
    )
}

/// A run in a chain ended: what it spent joins the chain's ledger.
pub async fn note_run_end(store: &dyn ServerStore, metadata: Option<&Value>, terminal: &Value) {
    let Some(root) = metadata
        .and_then(|m| m.pointer("/chain/root_run_id"))
        .and_then(Value::as_str)
    else {
        return;
    };
    let tokens = terminal
        .pointer("/spend/tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let mut spend = read(store, root).await;
    spend.spent_tokens += tokens;
    spend.runs += 1;
    if let Err(error) = store
        .kv_put(
            NAMESPACE,
            root,
            serde_json::to_value(&spend).unwrap_or(Value::Null),
        )
        .await
    {
        tracing::warn!(%error, %root, "chain spend not kept");
    } else {
        tracing::info!(%root, tokens, total = spend.spent_tokens, runs = spend.runs, "chain spend noted");
    }
}
