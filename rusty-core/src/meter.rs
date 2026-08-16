//! The token/cost meter: usage analytics replayed out of a journal.
//!
//! The Flight Recorder already journals the raw evidence — every
//! [`RunEventKind::ModelCall`] carries the provider-reported [`Usage`] and,
//! for priced models, the recorded `cost_usd`; every
//! [`RunEventKind::ToolCall`] carries its declared [`Effect`] class. This
//! module is the read side: pure functions that fold a
//! [`JournalSnapshot`] into per-model and per-run aggregates, plus a pricing
//! hook ([`RunMeter::estimate_cost`]) that turns tokens into dollars against
//! caller-supplied rates.
//!
//! Two honesty rules, both inherited from the journal:
//!
//! - **Usage is counted only where the provider reported it.** A model call
//!   without journaled `tokens` raises the request count and nothing else —
//!   the meter never invents token counts from payload sizes.
//! - **Journaled cost and derived cost never blend.** `cost_usd` sums are
//!   the provider-attested evidence, reported as-is; [`CostEstimate`] is a
//!   recomputation against caller rates, labeled an estimate. Where both
//!   exist they are reported side by side, so a drift between them is
//!   visible instead of averaged away.
//!
//! Everything here is a pure function of journal data — no I/O, no clock —
//! so a meter run is itself reproducible evidence: same snapshot, same
//! totals.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::journal::JournalSnapshot;
use crate::llm::{ModelPricing, Usage};
use crate::record::{Effect, EventStatus, PayloadRef, RunEventKind};

/// The model bucket usage falls into when the journaled call reported no
/// model identity.
pub const UNREPORTED_MODEL: &str = "unreported";

/// Token usage folded over a set of journaled model calls.
///
/// `requests` counts every call; the token fields sum only the calls whose
/// provider reported usage (`requests_with_usage` — the honest denominator
/// for any average).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenTotals {
    /// Model calls observed.
    pub requests: u64,

    /// Calls whose provider reported token usage.
    pub requests_with_usage: u64,

    /// Summed prompt tokens.
    pub prompt_tokens: u64,

    /// Summed completion tokens.
    pub completion_tokens: u64,

    /// Summed billed tokens.
    pub total_tokens: u64,

    /// Summed cache-served prompt tokens (a subset of `prompt_tokens`).
    pub cached_tokens: u64,

    /// Summed reasoning tokens (a subset of `completion_tokens`).
    pub reasoning_tokens: u64,
}

impl TokenTotals {
    /// Fold one call's reported usage in.
    fn add(&mut self, usage: &Usage) {
        self.requests_with_usage += 1;
        self.prompt_tokens += usage.prompt_tokens;
        self.completion_tokens += usage.completion_tokens;
        self.total_tokens += usage.total_tokens;
        self.cached_tokens += usage.cached_tokens.unwrap_or(0);
        self.reasoning_tokens += usage.reasoning_tokens.unwrap_or(0);
    }

    /// The cost of these totals under `pricing`, via
    /// [`ModelPricing::cost_usd`] over the folded [`Usage`].
    fn cost_under(&self, pricing: &ModelPricing) -> f64 {
        pricing.cost_usd(&Usage {
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            total_tokens: self.total_tokens,
            cached_tokens: (self.cached_tokens > 0).then_some(self.cached_tokens),
            reasoning_tokens: (self.reasoning_tokens > 0).then_some(self.reasoning_tokens),
        })
    }
}

/// One model's aggregate within a run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelMeter {
    /// The reported model identity, or [`UNREPORTED_MODEL`].
    pub model: String,

    /// Folded token usage.
    pub tokens: TokenTotals,

    /// Summed journaled `cost_usd` — the provider-attested evidence. `None`
    /// when no call for this model journaled a cost (unpriced models journal
    /// nothing, and the meter reports that absence rather than a zero that
    /// would read as "free").
    pub journaled_cost_usd: Option<f64>,
}

/// Tool-call totals for one [`Effect`] class.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolClassTotals {
    /// Calls observed.
    pub calls: u64,

    /// Calls that ended in [`EventStatus::Error`].
    pub errors: u64,
}

/// A run's folded usage: the meter's answer for one journal.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunMeter {
    /// The metered run.
    pub run_id: String,

    /// The thread (session) the run belongs to.
    pub thread_id: String,

    /// Token usage across all models.
    pub tokens: TokenTotals,

    /// Per-model breakdown, keyed by reported model identity
    /// ([`UNREPORTED_MODEL`] when the call reported none).
    pub models: BTreeMap<String, ModelMeter>,

    /// Tool-call counts by declared effect class.
    pub tool_calls_by_effect: BTreeMap<Effect, ToolClassTotals>,

    /// Summed journaled `cost_usd` across all models; `None` when no call
    /// journaled a cost.
    pub journaled_cost_usd: Option<f64>,
}

impl RunMeter {
    /// Derived cost against caller-supplied per-model rates.
    ///
    /// `rates` answers the [`ModelPricing`] for a model identity, or `None`
    /// for a model the caller does not price — unpriced models are named in
    /// the estimate rather than silently zeroed. The crate ships no price
    /// list (rates are operator configuration; see [`ModelPricing`]), and
    /// the meter's hook keeps that true for analytics as well.
    pub fn estimate_cost(&self, rates: impl Fn(&str) -> Option<ModelPricing>) -> CostEstimate {
        let mut estimate = CostEstimate {
            per_model_usd: BTreeMap::new(),
            total_usd: 0.0,
            unpriced_models: Vec::new(),
            journaled_total_usd: self.journaled_cost_usd,
        };
        for (model, meter) in &self.models {
            match rates(model) {
                Some(pricing) => {
                    let cost = meter.tokens.cost_under(&pricing);
                    estimate.per_model_usd.insert(model.clone(), cost);
                    estimate.total_usd += cost;
                }
                None => estimate.unpriced_models.push(model.clone()),
            }
        }
        estimate
    }
}

/// A recomputed cost: tokens folded from the journal × caller rates.
///
/// An estimate, never evidence — the journaled half
/// ([`CostEstimate::journaled_total_usd`]) is what the providers actually
/// reported, and the two are kept side by side precisely so they can be
/// compared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostEstimate {
    /// Derived cost per priced model.
    pub per_model_usd: BTreeMap<String, f64>,

    /// Derived cost across all priced models.
    pub total_usd: f64,

    /// Models with journaled usage but no caller-supplied rate. Their tokens
    /// contribute nothing to `total_usd` — an undercount named, not hidden.
    pub unpriced_models: Vec<String>,

    /// The run's summed journaled cost evidence, when any was recorded.
    pub journaled_total_usd: Option<f64>,
}

/// Resolve a payload against the snapshot's artifact map (inline payloads
/// resolve to themselves). Mirrors the journal's own receipt lookup: a
/// missing artifact means a truncated snapshot, and the meter treats the
/// model identity as unreported rather than failing the fold.
fn resolve<'a>(snapshot: &'a JournalSnapshot, payload: &'a PayloadRef) -> Option<&'a serde_json::Value> {
    match payload {
        PayloadRef::Inline(value) => Some(value),
        PayloadRef::Artifact(reference) => snapshot.artifacts.get(&reference.sha256),
    }
}

/// The model identity a journaled model call reported: the `model` key of
/// the canonical response payload (see `crate::replay::model_call_response`),
/// or [`UNREPORTED_MODEL`].
fn model_of(snapshot: &JournalSnapshot, event: &crate::record::RunEvent) -> String {
    event
        .output
        .as_ref()
        .and_then(|payload| resolve(snapshot, payload))
        .and_then(|value| value.get("model"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(UNREPORTED_MODEL)
        .to_owned()
}

/// Fold a journal snapshot into its [`RunMeter`]. Pure: same snapshot, same
/// meter.
///
/// Only [`RunEventKind::ModelCall`] events feed token and cost aggregates
/// (a replayed run re-journals its served calls, so replaying a snapshot's
/// own run and metering that journal double-counts nothing here — one
/// journal in, one meter out). Tool-call counts read
/// [`RunEventKind::ToolCall`] events by their declared effect class.
pub fn meter_journal(snapshot: &JournalSnapshot) -> RunMeter {
    let mut meter = RunMeter {
        run_id: snapshot.run_id.clone(),
        thread_id: snapshot.thread_id.clone(),
        ..RunMeter::default()
    };
    for event in &snapshot.events {
        match event.kind {
            RunEventKind::ModelCall => {
                meter.tokens.requests += 1;
                let model = model_of(snapshot, event);
                let entry = meter.models.entry(model.clone()).or_insert_with(|| ModelMeter {
                    model,
                    ..ModelMeter::default()
                });
                entry.tokens.requests += 1;
                if let Some(usage) = &event.tokens {
                    meter.tokens.add(usage);
                    entry.tokens.add(usage);
                }
                if let Some(cost) = event.cost_usd {
                    *meter.journaled_cost_usd.get_or_insert(0.0) += cost;
                    *entry.journaled_cost_usd.get_or_insert(0.0) += cost;
                }
            }
            RunEventKind::ToolCall => {
                let entry = meter.tool_calls_by_effect.entry(event.effect).or_default();
                entry.calls += 1;
                if event.status == EventStatus::Error {
                    entry.errors += 1;
                }
            }
            _ => {}
        }
    }
    meter
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{Clock, EventDraft, Journal};
    use serde_json::json;

    fn snapshot_with_calls() -> JournalSnapshot {
        let journal = Journal::new("run-1", "thread-1", Clock::System);
        let usage_a = Usage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cached_tokens: Some(20),
            ..Usage::default()
        };
        let usage_b = Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            ..Usage::default()
        };
        journal.record(
            EventDraft::new(RunEventKind::ModelCall, Effect::NonIdempotent)
                .output(json!({"message": {}, "model": "gpt-mock", "usage": usage_a}))
                .tokens(usage_a)
                .cost_usd(0.001),
        );
        journal.record(
            EventDraft::new(RunEventKind::ModelCall, Effect::NonIdempotent)
                .output(json!({"message": {}, "model": "gpt-mock", "usage": usage_b}))
                .tokens(usage_b),
        );
        // A call whose provider reported neither identity nor usage: counted
        // as a request, folded into nothing else.
        journal.record(
            EventDraft::new(RunEventKind::ModelCall, Effect::NonIdempotent)
                .status(EventStatus::Error)
                .output(json!({"error": "rate limited"})),
        );
        journal.record(EventDraft::new(RunEventKind::ToolCall, Effect::ReadOnly));
        journal.record(EventDraft::new(RunEventKind::ToolCall, Effect::ReadOnly));
        journal.record(
            EventDraft::new(RunEventKind::ToolCall, Effect::NonIdempotent)
                .status(EventStatus::Error),
        );
        journal.record(EventDraft::new(RunEventKind::SuperStepEnd, Effect::Pure));
        journal.snapshot()
    }

    #[test]
    fn folds_usage_per_model_and_per_run() {
        let meter = meter_journal(&snapshot_with_calls());
        assert_eq!(meter.tokens.requests, 3);
        assert_eq!(meter.tokens.requests_with_usage, 2);
        assert_eq!(meter.tokens.prompt_tokens, 110);
        assert_eq!(meter.tokens.completion_tokens, 55);
        assert_eq!(meter.tokens.cached_tokens, 20);

        let mock = &meter.models["gpt-mock"];
        assert_eq!(mock.tokens.requests, 2);
        assert_eq!(mock.tokens.total_tokens, 165);
        assert_eq!(mock.journaled_cost_usd, Some(0.001));

        // The unreported call is visible by name, not silently dropped.
        let unreported = &meter.models[UNREPORTED_MODEL];
        assert_eq!(unreported.tokens.requests, 1);
        assert_eq!(unreported.tokens.requests_with_usage, 0);
        assert_eq!(unreported.journaled_cost_usd, None);

        assert_eq!(meter.journaled_cost_usd, Some(0.001));
    }

    #[test]
    fn counts_tool_calls_by_effect_class() {
        let meter = meter_journal(&snapshot_with_calls());
        assert_eq!(
            meter.tool_calls_by_effect[&Effect::ReadOnly],
            ToolClassTotals { calls: 2, errors: 0 }
        );
        assert_eq!(
            meter.tool_calls_by_effect[&Effect::NonIdempotent],
            ToolClassTotals { calls: 1, errors: 1 }
        );
        assert!(!meter.tool_calls_by_effect.contains_key(&Effect::Pure));
    }

    #[test]
    fn estimate_cost_uses_caller_rates_and_names_the_unpriced() {
        let meter = meter_journal(&snapshot_with_calls());
        let estimate = meter.estimate_cost(|model| match model {
            "gpt-mock" => Some(ModelPricing::new(10.0, 30.0).with_cached_input(1.0)),
            _ => None,
        });
        // 90 uncached @ 10/M + 20 cached @ 1/M + 55 out @ 30/M.
        let expected = (90.0 * 10.0 + 20.0 * 1.0 + 55.0 * 30.0) / 1_000_000.0;
        assert!((estimate.per_model_usd["gpt-mock"] - expected).abs() < 1e-12);
        assert!((estimate.total_usd - expected).abs() < 1e-12);
        assert_eq!(estimate.unpriced_models, vec![UNREPORTED_MODEL.to_owned()]);
        // Evidence and estimate ride side by side, never blended.
        assert_eq!(estimate.journaled_total_usd, Some(0.001));
    }

    #[test]
    fn empty_journal_meters_to_zeroes() {
        let journal = Journal::new("run-9", "thread-9", Clock::System);
        let meter = meter_journal(&journal.snapshot());
        assert_eq!(meter.run_id, "run-9");
        assert_eq!(meter.tokens.requests, 0);
        assert!(meter.models.is_empty());
        assert!(meter.journaled_cost_usd.is_none());
        let estimate = meter.estimate_cost(|_| None);
        assert_eq!(estimate.total_usd, 0.0);
        assert!(estimate.unpriced_models.is_empty());
    }
}
