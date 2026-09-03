//! Induction: demand-side intent mining, supply-side coverage
//! reverse-engineering, the gap matrix, and the seeded ledger.
//!
//! The design lineage is the research library's cold-start document
//! (`reference/learning-loops-and-cold-start.md`), sections 3.2–3.4.
//! Where [`crate::gaps`] is the ledger of what the agent does not know,
//! this module is the machinery that fills it before the agent serves a
//! single turn:
//!
//! 1. **Intent mining** ([`mine_intents`]) clusters the interaction-event
//!    corpus into intents — recurring shapes of need — and derives for
//!    each: frequency and seasonality over the ingested window, the
//!    resolution-path distribution, failure indicators (reopen rate,
//!    reassignment count, abandonment rate, time-to-resolution against
//!    category norms), and a non-empty citation list into concrete
//!    events. The [`IntentMap`] ranks intents by volume × failure-cost
//!    and is a *projection*: recomputable from the events alone, never a
//!    store of record. Re-mining the same corpus with the same config
//!    reproduces it byte-for-byte.
//! 2. **Coverage reverse-engineering** ([`crawl_coverage`]) inverts the
//!    reachable supply: instead of "what knowledge do we have," it asks
//!    "what questions is this an answer to," expressed in the same
//!    intent vocabulary so the two maps join on `intent_id`. Every
//!    [`CoverageClaim`] carries a confidence grade (exact-signature
//!    coverage distinguished from keyword overlap) and a freshness
//!    assessment (staleness, references to retired systems), and every
//!    claim cites its artifact — an uncited claim is unrepresentable.
//! 3. **The gap matrix** ([`join_maps`]) joins the two maps so every
//!    intent lands in exactly one cell — working supply, failing supply
//!    (the knowledge exists and does not work), or the learn-now queue —
//!    and [`seed_ledger`] files the learn-now and failing-supply cells
//!    into the gap ledger with `origin: Induction`, cited evidence, and
//!    typed closure criteria. [`declared_blocks`] emits the declared
//!    memory-block schema for the top intents, empty where the matrix
//!    shows no supply: cold start as a checklist with owners, not a fog.
//!
//! Pure contracts, the crate's discipline: no IO, no clocks — timestamps
//! and corpora are injected. The crawl's *execution* (read-only
//! connectors under the guard pipeline, receipts, `side` stamps) is a
//! server concern; this module is the deterministic core it must
//! reproduce.

use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::gaps::{
    Citation, CitationKind, ClosureCriteria, GapError, GapLedger, GapOrigin, GapSubject,
    InteractionChannel, InteractionEvent, ResolutionPath,
};
use crate::record::sha256_hex;

/// The intent-map snapshot format version. Fail-closed: a map that
/// declares anything else is not read.
pub const INTENT_MAP_FORMAT_VERSION: u32 = 1;
/// The coverage-map snapshot format version.
pub const COVERAGE_MAP_FORMAT_VERSION: u32 = 1;
/// The gap-matrix snapshot format version.
pub const GAP_MATRIX_FORMAT_VERSION: u32 = 1;

/// Intent ids are content addresses with this prefix.
pub const INTENT_ID_PREFIX: &str = "in-";
/// Coverage-claim ids are content addresses with this prefix.
pub const CLAIM_ID_PREFIX: &str = "cc-";

/// See [`MAX_STATEMENT_BYTES`](crate::gaps::MAX_STATEMENT_BYTES).
pub const MAX_LABEL_BYTES: usize = 256;
/// The representative signature's size bound: the tokens a cluster is
/// matched and labeled by.
pub const MAX_SIGNATURE_TOKENS: usize = 10;
/// The default Jaccard threshold (per mille) for cluster assignment.
pub const DEFAULT_JACCARD_THRESHOLD_MILLIS: u32 = 400;
/// The default keyword-overlap threshold (per mille) for a weak
/// coverage claim.
pub const DEFAULT_KEYWORD_THRESHOLD_MILLIS: u32 = 250;
/// The default failure rate (per mille) at or above which covered
/// supply counts as failing.
pub const DEFAULT_FAILING_THRESHOLD_MILLIS: u64 = 200;
/// The default declared-block body limit.
pub const DEFAULT_BLOCK_CHAR_LIMIT: u32 = 2_000;

/// Every refusal induction produces is typed.
#[derive(Debug, Error)]
pub enum InductionError {
    /// A required text field was empty after trimming.
    #[error("empty field: {0}")]
    EmptyField(&'static str),
    /// A text field exceeded its byte bound.
    #[error("field too long: {field} ({len} > {max} bytes)")]
    FieldTooLong {
        /// The field that overflowed.
        field: &'static str,
        /// Its length in bytes.
        len: usize,
        /// The bound it exceeded.
        max: usize,
    },
    /// A snapshot declared a format version this build cannot read.
    #[error("unsupported format version: {0}")]
    UnsupportedFormat(u32),
    /// Ledger seeding was refused by the ledger.
    #[error("gap ledger refused seeding: {0}")]
    Gap(#[from] GapError),
    /// Serialization failed.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Induction's result type.
pub type Result<T> = std::result::Result<T, InductionError>;

fn check_field(field: &'static str, value: &str, max: usize) -> Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(InductionError::EmptyField(field));
    }
    if trimmed.len() > max {
        return Err(InductionError::FieldTooLong {
            field,
            len: trimmed.len(),
            max,
        });
    }
    Ok(())
}

// --------------------------------------------------------------------- //
// Tokenization
// --------------------------------------------------------------------- //

/// The stopword floor: closed-class English plus support-desk filler.
/// Deliberately small and fixed — the mining pass must be deterministic
/// across builds, so the list is part of the contract, not configuration.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "been", "but", "by", "can", "could", "did", "do",
    "does", "for", "from", "get", "got", "had", "has", "have", "help", "her", "him", "his", "how",
    "i", "if", "in", "into", "is", "it", "its", "me", "my", "need", "not", "of", "on", "or", "our",
    "please", "she", "so", "that", "the", "their", "them", "they", "this", "to", "was", "we",
    "what", "when", "where", "which", "who", "why", "will", "with", "you", "your",
];

/// Normalize free text into its content-token signature: lowercased,
/// split on non-alphanumerics, stopwords and short tokens removed,
/// sorted and deduped. Two phrasings of one need collapse onto one
/// signature; that collapse is the whole clustering primitive.
pub fn token_signature(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .map(|token| token.to_lowercase())
        .filter(|token| token.len() >= 3 && !STOPWORDS.contains(&token.as_str()))
        .collect();
    tokens.sort();
    tokens.dedup();
    tokens
}

/// Jaccard similarity of two signatures, per mille. Sorts defensively:
/// representative signatures are rank-ordered, not alphabetical, and
/// the two-pointer walk needs sorted inputs.
fn jaccard_millis(a: &[String], b: &[String]) -> u32 {
    if a.is_empty() && b.is_empty() {
        return 1000;
    }
    let mut a: Vec<&String> = a.iter().collect();
    let mut b: Vec<&String> = b.iter().collect();
    a.sort();
    b.sort();
    let (mut i, mut j, mut intersection) = (0, 0, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                intersection += 1;
                i += 1;
                j += 1;
            }
        }
    }
    let union = a.len() + b.len() - intersection;
    if union == 0 {
        return 0;
    }
    ((intersection as u64 * 1000) / union as u64) as u32
}

// --------------------------------------------------------------------- //
// Intent mining
// --------------------------------------------------------------------- //

/// How the corpus was clustered. `FullTextTaxonomy` is the mode this
/// crate implements; the map records which mode produced it so a later
/// vector-indexed pass is a distinct artifact, not a silent change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusteringMode {
    /// Token-signature clustering with channel fallback — the mode that
    /// needs nothing beyond the event store itself.
    FullTextTaxonomy,
}

/// The mining pass's configuration. Weights are milli-units: the
/// failure-cost vocabulary is carried opaquely, exactly as the ledger
/// carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MiningConfig {
    /// Which clustering produced the map.
    pub mode: ClusteringMode,
    /// The Jaccard threshold (per mille) at or above which an event
    /// joins an existing cluster instead of opening one.
    pub jaccard_threshold_millis: u32,
    /// Milli-cost per human-resolved event (human resolution minutes,
    /// flattened to a per-event weight).
    pub human_resolution_cost_millis: u64,
    /// Milli-cost per escalation (depth flattened to a per-event
    /// weight): being handed up a tier is the clearest supply-failure
    /// signal the record carries.
    pub escalation_cost_millis: u64,
    /// Milli-cost per abandoned or unresolved event.
    pub abandonment_cost_millis: u64,
    /// Milli-cost per median minute an intent's time-to-resolution runs
    /// over its channel's norm.
    pub ttr_over_norm_cost_millis: u64,
}

impl Default for MiningConfig {
    fn default() -> Self {
        Self {
            mode: ClusteringMode::FullTextTaxonomy,
            jaccard_threshold_millis: DEFAULT_JACCARD_THRESHOLD_MILLIS,
            human_resolution_cost_millis: 100,
            escalation_cost_millis: 500,
            abandonment_cost_millis: 300,
            ttr_over_norm_cost_millis: 10,
        }
    }
}

/// The resolution-path histogram: one field per variant, so the wire
/// shape is fixed and every consumer reads the same names.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionDistribution {
    /// Self-served against existing knowledge.
    pub self_service: u64,
    /// An automated surface answered without a human.
    pub deflected: u64,
    /// A human resolved it.
    pub human_resolved: u64,
    /// Never resolved.
    pub unresolved: u64,
    /// The user gave up.
    pub abandoned: u64,
}

impl ResolutionDistribution {
    fn record(&mut self, path: ResolutionPath) {
        match path {
            ResolutionPath::SelfService => self.self_service += 1,
            ResolutionPath::Deflected => self.deflected += 1,
            ResolutionPath::HumanResolved => self.human_resolved += 1,
            ResolutionPath::Unresolved => self.unresolved += 1,
            ResolutionPath::Abandoned => self.abandoned += 1,
        }
    }
}

/// The failure indicators the miner derives from the events themselves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureIndicators {
    /// Share of events whose outcome was `reopened`, per mille.
    pub reopen_rate_millis: u64,
    /// Events that arrived as escalations between tiers — the record's
    /// structural reassignment signal.
    pub reassignment_count: u64,
    /// Share of events abandoned or unresolved, per mille.
    pub abandonment_rate_millis: u64,
    /// Median minutes from occurrence to resolution (events with a
    /// `resolved_at`; `None` when none resolved).
    pub ttr_median_minutes: Option<u64>,
    /// The median across the whole corpus for this intent's channel —
    /// the norm the intent's own median is read against.
    pub ttr_category_norm_minutes: Option<u64>,
}

/// One mined intent: a recurring shape of need, ranked and cited.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    /// Content address: `in-<sha256>` over the seed signature — the
    /// token signature of the event that opened the cluster. Membership
    /// may change as the corpus grows; identity does not.
    pub intent_id: String,
    /// The human-readable shape: the cluster's top tokens.
    pub label: String,
    /// The representative signature the cluster matches and is labeled
    /// by (top tokens by document frequency). Evolves with membership.
    pub signature: Vec<String>,
    /// The events constituting this intent, sorted. Non-empty by
    /// construction — an intent without citations is not emitted.
    pub event_ids: Vec<String>,
    /// Event count over the ingested window.
    pub frequency: u64,
    /// Day-of-week histogram (Monday-first) over the window — the
    /// seasonality record (password-reset spikes after long weekends
    /// are visible here).
    pub seasonality_weekday: [u64; 7],
    /// The resolution-path distribution.
    pub resolution: ResolutionDistribution,
    /// The failure indicators.
    pub failure: FailureIndicators,
    /// The derived failure-cost estimate in milli-units.
    pub failure_cost_millis: u64,
}

impl Intent {
    /// The ranking function: volume × failure-cost, saturating. The
    /// demand-side loop's entire product is this ordering.
    pub fn rank_score(&self) -> u64 {
        self.frequency.saturating_mul(self.failure_cost_millis)
    }
}

/// Derive an intent's content address from its seed signature.
pub fn derive_intent_id(seed_signature: &[String]) -> String {
    format!(
        "{INTENT_ID_PREFIX}{}",
        sha256_hex(seed_signature.join(" ").as_bytes())
    )
}

/// The mined intent map. A projection, never a store of record:
/// deleting it and re-mining the same events with the same config
/// reproduces it byte-for-byte.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentMap {
    /// Snapshot format version ([`INTENT_MAP_FORMAT_VERSION`]).
    pub version: u32,
    /// When the pass ran (injected).
    pub mined_at: DateTime<Utc>,
    /// The corpus window: earliest and latest `occurred_at` (`None`
    /// for an empty corpus).
    pub window_start: Option<DateTime<Utc>>,
    /// See `window_start`.
    pub window_end: Option<DateTime<Utc>>,
    /// The clustering mode that produced the map.
    pub mode: ClusteringMode,
    /// The corpus size the map was mined from.
    pub event_count: u64,
    /// The intents, sorted by id for canonical serialization.
    pub intents: Vec<Intent>,
}

impl IntentMap {
    /// Validate the snapshot version — fail-closed on anything else.
    pub fn check(&self) -> Result<()> {
        if self.version != INTENT_MAP_FORMAT_VERSION {
            return Err(InductionError::UnsupportedFormat(self.version));
        }
        Ok(())
    }

    /// The intents in work order: rank score descending, id ascending —
    /// deterministic given the same store contents.
    pub fn ranked(&self) -> Vec<&Intent> {
        let mut ranked: Vec<&Intent> = self.intents.iter().collect();
        ranked.sort_by(|a, b| {
            b.rank_score()
                .cmp(&a.rank_score())
                .then_with(|| a.intent_id.cmp(&b.intent_id))
        });
        ranked
    }

    /// One intent by id.
    pub fn get(&self, intent_id: &str) -> Option<&Intent> {
        self.intents
            .iter()
            .find(|intent| intent.intent_id == intent_id)
    }

    /// Event id → intent id, for assignment diffs.
    fn assignments(&self) -> BTreeMap<&str, &str> {
        let mut out = BTreeMap::new();
        for intent in &self.intents {
            for event_id in &intent.event_ids {
                out.insert(event_id.as_str(), intent.intent_id.as_str());
            }
        }
        out
    }
}

/// One event's move between intents across two mining passes. Moves are
/// appended to the ledger as versioned reassignments (never in place);
/// downstream consumers observe the newest assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reassignment {
    /// The event that moved.
    pub event_id: String,
    /// Where it was (`None` when the event is new to the corpus).
    pub from_intent: Option<String>,
    /// Where the newer pass put it.
    pub to_intent: String,
}

/// Diff two maps' assignments: every event whose intent changed, in
/// event-id order. Events only in the older map are not reported — the
/// corpus is append-only, so disappearance means a rebuilt corpus, not
/// a move.
pub fn diff_assignments(prev: &IntentMap, next: &IntentMap) -> Vec<Reassignment> {
    let prev_assignments = prev.assignments();
    let mut out = Vec::new();
    for (event_id, to_intent) in next.assignments() {
        let from_intent = prev_assignments.get(event_id).map(|id| id.to_string());
        if from_intent.as_deref() != Some(to_intent) {
            out.push(Reassignment {
                event_id: event_id.to_string(),
                from_intent,
                to_intent: to_intent.to_string(),
            });
        }
    }
    out
}

/// The median of a sorted slice (lower median for even counts).
fn sorted_median(sorted: &[u64]) -> Option<u64> {
    if sorted.is_empty() {
        None
    } else {
        Some(sorted[(sorted.len() - 1) / 2])
    }
}

/// One cluster under construction.
struct ClusterBuilder {
    /// The seed event's signature — the cluster's identity.
    seed_signature: Vec<String>,
    /// Token document frequency across the cluster's events.
    token_df: BTreeMap<String, u64>,
    /// The member events, in processing order.
    events: Vec<InteractionEvent>,
}

impl ClusterBuilder {
    fn new(signature: Vec<String>, event: InteractionEvent) -> Self {
        let mut token_df = BTreeMap::new();
        for token in &signature {
            *token_df.entry(token.clone()).or_insert(0) += 1;
        }
        Self {
            seed_signature: signature,
            token_df,
            events: vec![event],
        }
    }

    fn assign(&mut self, signature: &[String], event: InteractionEvent) {
        for token in signature {
            *self.token_df.entry(token.clone()).or_insert(0) += 1;
        }
        self.events.push(event);
    }

    /// The representative signature: top tokens by document frequency,
    /// ties broken alphabetically — deterministic.
    fn representative(&self) -> Vec<String> {
        let mut tokens: Vec<(&String, &u64)> = self.token_df.iter().collect();
        tokens.sort_by(|(token_a, df_a), (token_b, df_b)| {
            df_b.cmp(df_a).then_with(|| token_a.cmp(token_b))
        });
        tokens
            .into_iter()
            .take(MAX_SIGNATURE_TOKENS)
            .map(|(token, _)| token.clone())
            .collect()
    }
}

/// Mine the interaction-event corpus into the ranked intent map.
///
/// Deterministic: events are processed in `(occurred_at, event_id)`
/// order, each joins the earliest-created cluster whose representative
/// signature clears the Jaccard threshold (else opens one), and the map
/// serializes canonically — re-mining the same corpus with the same
/// config reproduces it byte-for-byte. An event whose utterance carries
/// no content tokens falls back to a channel signature, so silence
/// clusters with silence on the same channel rather than fragmenting.
pub fn mine_intents(
    events: &[InteractionEvent],
    config: &MiningConfig,
    at: DateTime<Utc>,
) -> Result<IntentMap> {
    // The category norms: per-channel median time-to-resolution over the
    // whole corpus, computed before clustering.
    let mut ttr_by_channel: BTreeMap<InteractionChannel, Vec<u64>> = BTreeMap::new();
    for event in events {
        if let Some(ttr) = resolution_minutes(event) {
            ttr_by_channel.entry(event.channel).or_default().push(ttr);
        }
    }
    let channel_norms: BTreeMap<InteractionChannel, u64> = ttr_by_channel
        .into_iter()
        .filter_map(|(channel, mut ttrs)| {
            ttrs.sort();
            sorted_median(&ttrs).map(|median| (channel, median))
        })
        .collect();

    let mut sorted: Vec<&InteractionEvent> = events.iter().collect();
    sorted.sort_by(|a, b| {
        a.occurred_at
            .cmp(&b.occurred_at)
            .then_with(|| a.event_id.cmp(&b.event_id))
    });

    let mut clusters: Vec<ClusterBuilder> = Vec::new();
    for event in sorted {
        let mut signature = token_signature(&event.utterance);
        if signature.is_empty() {
            signature = vec![format!("channel:{}", channel_key(event.channel))];
        }
        // The best clearing cluster wins; ties go to the earliest
        // created, so assignment never depends on hash order.
        let mut best: Option<(usize, u32)> = None;
        for (index, cluster) in clusters.iter().enumerate() {
            let score = jaccard_millis(&signature, &cluster.representative());
            if score >= config.jaccard_threshold_millis
                && best.is_none_or(|(_, best_score)| score > best_score)
            {
                best = Some((index, score));
            }
        }
        match best {
            Some((index, _)) => clusters[index].assign(&signature, event.clone()),
            None => clusters.push(ClusterBuilder::new(signature, event.clone())),
        }
    }

    let mut intents = Vec::with_capacity(clusters.len());
    for cluster in clusters {
        let signature = cluster.representative();
        let label = signature
            .iter()
            .take(4)
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        check_field("intent.label", &label, MAX_LABEL_BYTES)?;

        let mut event_ids: Vec<String> = cluster
            .events
            .iter()
            .map(|event| event.event_id.clone())
            .collect();
        event_ids.sort();
        let frequency = cluster.events.len() as u64;

        let mut seasonality_weekday = [0u64; 7];
        let mut resolution = ResolutionDistribution::default();
        let mut reopened = 0u64;
        let mut reassignment_count = 0u64;
        let mut abandoned = 0u64;
        let mut ttrs = Vec::new();
        for event in &cluster.events {
            seasonality_weekday[event.occurred_at.weekday().num_days_from_monday() as usize] += 1;
            resolution.record(event.resolution_path);
            if event.outcome == crate::gaps::InteractionOutcome::Reopened {
                reopened += 1;
            }
            if event.channel == InteractionChannel::Escalation {
                reassignment_count += 1;
            }
            if matches!(
                event.resolution_path,
                ResolutionPath::Abandoned | ResolutionPath::Unresolved
            ) {
                abandoned += 1;
            }
            if let Some(ttr) = resolution_minutes(event) {
                ttrs.push(ttr);
            }
        }
        ttrs.sort();
        let ttr_median_minutes = sorted_median(&ttrs);
        let ttr_category_norm_minutes = channel_norms.get(&cluster.events[0].channel).copied();

        let failure_cost_millis = resolution
            .human_resolved
            .saturating_mul(config.human_resolution_cost_millis)
            .saturating_add(reassignment_count.saturating_mul(config.escalation_cost_millis))
            .saturating_add(abandoned.saturating_mul(config.abandonment_cost_millis))
            .saturating_add(match (ttr_median_minutes, ttr_category_norm_minutes) {
                (Some(median), Some(norm)) => median
                    .saturating_sub(norm)
                    .saturating_mul(config.ttr_over_norm_cost_millis),
                _ => 0,
            });

        intents.push(Intent {
            intent_id: derive_intent_id(&cluster.seed_signature),
            label,
            signature,
            event_ids,
            frequency,
            seasonality_weekday,
            resolution,
            failure: FailureIndicators {
                reopen_rate_millis: reopened.saturating_mul(1000) / frequency,
                reassignment_count,
                abandonment_rate_millis: abandoned.saturating_mul(1000) / frequency,
                ttr_median_minutes,
                ttr_category_norm_minutes,
            },
            failure_cost_millis,
        });
    }
    intents.sort_by(|a, b| a.intent_id.cmp(&b.intent_id));

    Ok(IntentMap {
        version: INTENT_MAP_FORMAT_VERSION,
        mined_at: at,
        window_start: events.iter().map(|event| event.occurred_at).min(),
        window_end: events.iter().map(|event| event.occurred_at).max(),
        mode: config.mode,
        event_count: events.len() as u64,
        intents,
    })
}

/// Minutes from occurrence to resolution, when both stamps exist and
/// the ordering is sane.
fn resolution_minutes(event: &InteractionEvent) -> Option<u64> {
    let resolved = event.resolved_at?;
    let minutes = (resolved - event.occurred_at).num_minutes();
    (minutes >= 0).then_some(minutes as u64)
}

/// The channel's stable string key (the serde repr).
fn channel_key(channel: InteractionChannel) -> &'static str {
    match channel {
        InteractionChannel::PortalSearch => "portal_search",
        InteractionChannel::Chat => "chat",
        InteractionChannel::Request => "request",
        InteractionChannel::Incident => "incident",
        InteractionChannel::Case => "case",
        InteractionChannel::Escalation => "escalation",
    }
}

// --------------------------------------------------------------------- //
// Coverage reverse-engineering
// --------------------------------------------------------------------- //

/// The kinds of reachable supply the crawl inverts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// A knowledge-base article.
    KbArticle,
    /// A standard operating procedure.
    Sop,
    /// A runbook.
    Runbook,
    /// An agent macro.
    Macro,
    /// A catalog item (with its fulfillment script).
    CatalogItem,
    /// A shipped skill.
    Skill,
    /// A memory block.
    MemoryBlock,
}

/// A pointer to a crawled artifact. The citation a coverage claim is
/// invalid without.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    /// What kind of artifact.
    pub kind: ArtifactKind,
    /// Its id in the source system.
    pub id: String,
}

/// One reachable artifact, reduced to what the inversion needs. The
/// crawl (connectors, receipts, `side` stamps) produces these; the
/// inversion itself is a pure function over them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplyArtifact {
    /// The artifact's id in the source system.
    pub artifact_id: String,
    /// Its kind.
    pub kind: ArtifactKind,
    /// Its title.
    pub title: String,
    /// Its body text.
    pub body: String,
    /// Its last revision date, when the source carries one.
    pub last_revised: Option<DateTime<Utc>>,
    /// The systems the artifact references (connector-extracted), used
    /// for retired-system detection.
    pub systems_referenced: Vec<String>,
}

impl SupplyArtifact {
    /// Construct an artifact descriptor, validating the fields the
    /// schema owns.
    pub fn new(
        artifact_id: impl Into<String>,
        kind: ArtifactKind,
        title: impl Into<String>,
        body: impl Into<String>,
        last_revised: Option<DateTime<Utc>>,
        systems_referenced: Vec<String>,
    ) -> Result<Self> {
        let artifact_id = artifact_id.into();
        let title = title.into();
        check_field("artifact_id", &artifact_id, MAX_LABEL_BYTES)?;
        check_field("title", &title, MAX_LABEL_BYTES)?;
        Ok(Self {
            artifact_id,
            kind,
            title,
            body: body.into(),
            last_revised,
            systems_referenced,
        })
    }

    /// The artifact's citation pointer.
    pub fn artifact_ref(&self) -> ArtifactRef {
        ArtifactRef {
            kind: self.kind,
            id: self.artifact_id.clone(),
        }
    }
}

/// The coverage crawl's configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageConfig {
    /// The deployment's known-retired systems (normalized lowercase):
    /// an artifact referencing one is stale-adjacent wherever it ranks.
    pub retired_systems: Vec<String>,
    /// An artifact whose last revision is older than this many days is
    /// stale.
    pub stale_after_days: u32,
    /// The Jaccard threshold (per mille) at or above which token overlap
    /// earns a keyword-grade claim.
    pub keyword_threshold_millis: u32,
}

impl Default for CoverageConfig {
    fn default() -> Self {
        Self {
            retired_systems: Vec::new(),
            stale_after_days: 180,
            keyword_threshold_millis: DEFAULT_KEYWORD_THRESHOLD_MILLIS,
        }
    }
}

/// How strongly an artifact covers an intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceGrade {
    /// The artifact carries the intent's full representative signature
    /// — an article that names the exact error message.
    ExactSignature,
    /// Token overlap above the keyword threshold — an article that
    /// shares three keywords.
    KeywordOverlap,
}

/// The freshness half of a coverage claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Freshness {
    /// The artifact's last revision date.
    pub last_revised: Option<DateTime<Utc>>,
    /// Whether the revision is older than the configured window.
    pub stale: bool,
    /// Whether the artifact references a retired system.
    pub references_retired_system: bool,
}

/// One coverage claim: an artifact that plausibly answers an intent,
/// graded, freshness-assessed, and cited.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageClaim {
    /// Content address: `cc-<sha256>` over intent × artifact.
    pub claim_id: String,
    /// The intent the artifact plausibly answers.
    pub intent_id: String,
    /// The artifact — the claim's citation. An uncited claim is
    /// unrepresentable.
    pub artifact: ArtifactRef,
    /// The confidence grade.
    pub confidence: ConfidenceGrade,
    /// The freshness assessment.
    pub freshness: Freshness,
}

/// Derive a claim's content address.
pub fn derive_claim_id(intent_id: &str, artifact: &ArtifactRef) -> String {
    #[derive(Serialize)]
    struct ClaimAddress<'a> {
        intent_id: &'a str,
        artifact: &'a ArtifactRef,
    }
    format!(
        "{CLAIM_ID_PREFIX}{}",
        sha256_hex(
            &serde_json::to_vec(&ClaimAddress {
                intent_id,
                artifact
            })
            .expect("ArtifactRef serialization is infallible")
        )
    )
}

/// The coverage map: intent → supporting artifacts → confidence →
/// freshness, every edge cited. A projection, recomputable from the
/// crawled artifacts and the intent map; never a store of record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageMap {
    /// Snapshot format version ([`COVERAGE_MAP_FORMAT_VERSION`]).
    pub version: u32,
    /// When the crawl ran (injected).
    pub crawled_at: DateTime<Utc>,
    /// The claims, sorted by claim id for canonical serialization.
    pub claims: Vec<CoverageClaim>,
    /// Artifacts no mined intent matched — the latent-capability cell's
    /// raw material, reported for advertise-or-retire decisions.
    pub latent_artifacts: Vec<ArtifactRef>,
}

impl CoverageMap {
    /// Validate the snapshot version — fail-closed on anything else.
    pub fn check(&self) -> Result<()> {
        if self.version != COVERAGE_MAP_FORMAT_VERSION {
            return Err(InductionError::UnsupportedFormat(self.version));
        }
        Ok(())
    }

    /// The claims behind one intent, in claim-id order.
    pub fn claims_for(&self, intent_id: &str) -> Vec<&CoverageClaim> {
        self.claims
            .iter()
            .filter(|claim| claim.intent_id == intent_id)
            .collect()
    }
}

/// Crawl the reachable supply and invert it into coverage claims:
/// "what questions is this an answer to," in the miner's vocabulary.
/// For each artifact × intent the best grade wins — full-signature
/// containment is exact coverage, clearing the keyword threshold is
/// weak coverage, anything less is no claim. Artifacts nothing matched
/// land in `latent_artifacts`.
pub fn crawl_coverage(
    artifacts: &[SupplyArtifact],
    intents: &IntentMap,
    config: &CoverageConfig,
    at: DateTime<Utc>,
) -> Result<CoverageMap> {
    let retired: Vec<String> = config
        .retired_systems
        .iter()
        .map(|system| system.to_lowercase())
        .collect();
    let mut claims = Vec::new();
    let mut latent_artifacts = Vec::new();

    for artifact in artifacts {
        let artifact_tokens = token_signature(&format!("{} {}", artifact.title, artifact.body));
        let freshness = Freshness {
            last_revised: artifact.last_revised,
            stale: artifact
                .last_revised
                .map(|revised| (at - revised).num_days() > i64::from(config.stale_after_days))
                .unwrap_or(false),
            references_retired_system: artifact
                .systems_referenced
                .iter()
                .any(|system| retired.contains(&system.to_lowercase())),
        };
        let mut claimed = false;
        for intent in &intents.intents {
            let exact = !intent.signature.is_empty()
                && intent
                    .signature
                    .iter()
                    .all(|token| artifact_tokens.contains(token));
            let confidence = if exact {
                ConfidenceGrade::ExactSignature
            } else if jaccard_millis(&intent.signature, &artifact_tokens)
                >= config.keyword_threshold_millis
            {
                ConfidenceGrade::KeywordOverlap
            } else {
                continue;
            };
            claimed = true;
            let artifact_ref = artifact.artifact_ref();
            claims.push(CoverageClaim {
                claim_id: derive_claim_id(&intent.intent_id, &artifact_ref),
                intent_id: intent.intent_id.clone(),
                artifact: artifact_ref,
                confidence,
                freshness,
            });
        }
        if !claimed {
            latent_artifacts.push(artifact.artifact_ref());
        }
    }
    claims.sort_by(|a, b| a.claim_id.cmp(&b.claim_id));
    latent_artifacts.sort();

    Ok(CoverageMap {
        version: COVERAGE_MAP_FORMAT_VERSION,
        crawled_at: at,
        claims,
        latent_artifacts,
    })
}

// --------------------------------------------------------------------- //
// The gap matrix and the seeded ledger
// --------------------------------------------------------------------- //

/// The cell one intent lands in. Every intent lands in exactly one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatrixCell {
    /// Demand with supply whose measured failure rate stays under the
    /// threshold — coverage that works.
    WorkingSupply,
    /// Demand with supply whose failure rate says the knowledge exists
    /// and does not work — the richest learning target: a missing
    /// article is an honest gap, a wrong article is a trap.
    FailingSupply,
    /// Demand without supply — the learn-now queue, the hunting loop's
    /// standing work order.
    LearnNow,
}

/// One intent's row in the matrix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatrixRow {
    /// The intent.
    pub intent_id: String,
    /// Its cell.
    pub cell: MatrixCell,
    /// The coverage claims behind the row (empty for learn-now).
    pub claim_ids: Vec<String>,
    /// The measured failure rate the subdivision read (reopen plus
    /// abandonment, per mille).
    pub failure_rate_millis: u64,
}

/// The joined map. A projection of the intent map and the coverage map;
/// the latent-capability cell rides along for the advertise-or-retire
/// report and seeds nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GapMatrix {
    /// Snapshot format version ([`GAP_MATRIX_FORMAT_VERSION`]).
    pub version: u32,
    /// When the join ran (injected).
    pub joined_at: DateTime<Utc>,
    /// Every intent's row, sorted by intent id.
    pub rows: Vec<MatrixRow>,
    /// Supply without demand — latent capability. Advertise it or let
    /// retention scoring retire it; nothing here seeds ledger entries.
    pub latent_artifacts: Vec<ArtifactRef>,
}

impl GapMatrix {
    /// Validate the snapshot version — fail-closed on anything else.
    pub fn check(&self) -> Result<()> {
        if self.version != GAP_MATRIX_FORMAT_VERSION {
            return Err(InductionError::UnsupportedFormat(self.version));
        }
        Ok(())
    }

    /// One intent's row.
    pub fn row(&self, intent_id: &str) -> Option<&MatrixRow> {
        self.rows.iter().find(|row| row.intent_id == intent_id)
    }
}

/// Join the two maps: every intent lands in exactly one cell, and the
/// demand-with-supply cell subdivides by measured failure rate —
/// reopen plus abandonment at or above the threshold is failing supply,
/// flagged distinctly from supply that works.
pub fn join_maps(
    intents: &IntentMap,
    coverage: &CoverageMap,
    failing_threshold_millis: u64,
    at: DateTime<Utc>,
) -> GapMatrix {
    let mut rows = Vec::with_capacity(intents.intents.len());
    for intent in &intents.intents {
        let claims = coverage.claims_for(&intent.intent_id);
        let failure_rate_millis = intent
            .failure
            .reopen_rate_millis
            .saturating_add(intent.failure.abandonment_rate_millis);
        let cell = if claims.is_empty() {
            MatrixCell::LearnNow
        } else if failure_rate_millis >= failing_threshold_millis {
            MatrixCell::FailingSupply
        } else {
            MatrixCell::WorkingSupply
        };
        rows.push(MatrixRow {
            intent_id: intent.intent_id.clone(),
            cell,
            claim_ids: claims.iter().map(|claim| claim.claim_id.clone()).collect(),
            failure_rate_millis,
        });
    }
    GapMatrix {
        version: GAP_MATRIX_FORMAT_VERSION,
        joined_at: at,
        rows,
        latent_artifacts: coverage.latent_artifacts.clone(),
    }
}

/// Seed the gap ledger from the matrix: the learn-now and
/// failing-supply cells file entries with `origin: Induction`, evidence
/// citing interaction events and coverage edges, priority derived from
/// volume × failure-cost, and typed closure criteria — `block_filled`
/// for missing knowledge, `failure_rate_below` for knowledge that does
/// not work. Working supply and the latent cell seed nothing. Dedupe is
/// the ledger's own rule: re-seeding reinforces rather than duplicates.
/// Returns the seeded gap ids, in row order.
pub fn seed_ledger(
    ledger: &mut GapLedger,
    matrix: &GapMatrix,
    intents: &IntentMap,
    at: DateTime<Utc>,
    failing_threshold_millis: u32,
) -> Result<Vec<String>> {
    let mut seeded = Vec::new();
    for row in &matrix.rows {
        let intent = intents
            .get(&row.intent_id)
            .expect("matrix rows are built from this intent map");
        let subject = GapSubject::Intent {
            intent_id: intent.intent_id.clone(),
        };
        match row.cell {
            MatrixCell::LearnNow => {
                let evidence: Result<Vec<Citation>> = intent
                    .event_ids
                    .iter()
                    .take(5)
                    .map(|event_id| {
                        Citation::new(
                            CitationKind::InteractionEvent,
                            event_id.clone(),
                            Some("demand behind this intent".to_string()),
                        )
                        .map_err(InductionError::from)
                    })
                    .collect();
                let gap_id = ledger.file_gap(
                    subject,
                    format!("No reachable knowledge answers: {}", intent.label),
                    evidence?,
                    GapOrigin::Induction,
                    ClosureCriteria::BlockFilled {
                        block_label: intent.label.clone(),
                    },
                    intent.frequency,
                    intent.failure_cost_millis,
                    "induction",
                    at,
                )?;
                seeded.push(gap_id);
            }
            MatrixCell::FailingSupply => {
                let mut evidence: Vec<Citation> = Vec::new();
                for event_id in intent.event_ids.iter().take(3) {
                    evidence.push(
                        Citation::new(
                            CitationKind::InteractionEvent,
                            event_id.clone(),
                            Some("demand failing against existing supply".to_string()),
                        )
                        .map_err(InductionError::from)?,
                    );
                }
                for claim_id in &row.claim_ids {
                    evidence.push(
                        Citation::new(
                            CitationKind::CoverageEdge,
                            claim_id.clone(),
                            Some("the supply that reads as an answer and fails as one".to_string()),
                        )
                        .map_err(InductionError::from)?,
                    );
                }
                let gap_id = ledger.file_gap(
                    subject,
                    format!(
                        "Reachable knowledge answers `{}` but the measured failure rate ({} per mille) says it does not work",
                        intent.label, row.failure_rate_millis
                    ),
                    evidence,
                    GapOrigin::Induction,
                    ClosureCriteria::FailureRateBelow {
                        threshold_millis: failing_threshold_millis,
                    },
                    intent.frequency,
                    intent.failure_cost_millis,
                    "induction",
                    at,
                )?;
                seeded.push(gap_id);
            }
            MatrixCell::WorkingSupply => {}
        }
    }
    Ok(seeded)
}

/// One declared memory block: label, behavior-bearing description,
/// char limit — the Letta-style schema the top intents mount as, empty
/// where the matrix shows no supply. An empty block is a visible,
/// queryable commitment, not a missing feature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredBlock {
    /// The block's label.
    pub label: String,
    /// What belongs in it — behavior-bearing, so the hunting loop and
    /// operators can see exactly what is supposed to be known.
    pub description: String,
    /// The body limit.
    pub char_limit: u32,
    /// The intent the block answers.
    pub intent_id: String,
    /// Whether it mounts empty (the matrix shows no supply).
    pub empty: bool,
}

/// Emit the declared-block schema for the top-ranked intents, in work
/// order. Mounting the blocks into the agent's memory is the server's
/// concern (EP-06's block surface per the blueprint's memory schema);
/// this list is the contract it mounts.
pub fn declared_blocks(
    matrix: &GapMatrix,
    intents: &IntentMap,
    top_n: usize,
    char_limit: u32,
) -> Vec<DeclaredBlock> {
    intents
        .ranked()
        .into_iter()
        .take(top_n)
        .map(|intent| {
            let empty = matrix
                .row(&intent.intent_id)
                .map(|row| row.cell == MatrixCell::LearnNow)
                .unwrap_or(true);
            DeclaredBlock {
                label: intent.label.clone(),
                description: format!(
                    "What the agent must know to answer `{}`: current state, known failure modes, and the escalation path.",
                    intent.label
                ),
                char_limit,
                intent_id: intent.intent_id.clone(),
                empty,
            }
        })
        .collect()
}
