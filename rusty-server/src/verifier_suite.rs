//! The verifier's own suite (SI6, "verifier trust first"): a person says
//! whether a run's verdict was right, and those judgments are the cases
//! the verifier is measured on. Judging the verifier runs every reviewed
//! run's kept transcript past the judge again and counts where it agrees
//! with the person. Auto-promotion on a verdict is only as good as this
//! number; nothing auto-applies until it stands.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;

/// The most reviewed runs one judging reads: the newest.
pub const CASES_PER_EVALUATION: usize = 30;
/// The floor under auto-promotion: this many verdicts reviewed by people,
/// and the judge agreeing with them this often on its latest judging.
/// Below it a proposal the gate passes still waits for a person.
pub const FLOOR_REVIEWS: usize = 5;
pub const FLOOR_AGREEMENT: f64 = 0.9;

/// Whether the verifier's evidence stands under auto-promotion, and why not.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Floor {
    pub stands: bool,
    pub reviews: usize,
    pub needs_reviews: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agreement: Option<f64>,
    pub needs_agreement: f64,
    pub why: String,
}

pub(crate) async fn floor(state: &AppState, tenant: &str) -> Floor {
    let reviews = reviews_of(state, tenant).await.len();
    let latest = latest_evaluation(state, tenant)
        .await
        .filter(|e| e.status == "done");
    let agreement = latest
        .as_ref()
        .filter(|e| e.judged > 0)
        .map(|e| e.agreed as f64 / e.judged as f64);
    let (stands, why) = if reviews < FLOOR_REVIEWS {
        (
            false,
            format!(
                "people have reviewed {reviews} verdict{} and auto-apply needs {FLOOR_REVIEWS}",
                if reviews == 1 { "" } else { "s" }
            ),
        )
    } else {
        match (latest.as_ref(), agreement) {
            (None, _) | (_, None) => (
                false,
                "the verifier has not been judged since the reviews came in — judge it in Observe"
                    .to_owned(),
            ),
            (Some(e), Some(_)) if e.judged < FLOOR_REVIEWS => (
                false,
                format!(
                    "the judge answered only {} of {} cases on its latest judging; it needs {FLOOR_REVIEWS}",
                    e.judged, e.total
                ),
            ),
            (Some(e), Some(a)) if a < FLOOR_AGREEMENT => (
                false,
                format!(
                    "the judge agrees with people on {} of {} ({:.0}%); auto-apply needs {:.0}%",
                    e.agreed,
                    e.judged,
                    a * 100.0,
                    FLOOR_AGREEMENT * 100.0
                ),
            ),
            (Some(e), Some(a)) => (
                true,
                format!(
                    "the judge agrees with people on {} of {} ({:.0}%) over {reviews} reviewed verdicts",
                    e.agreed,
                    e.judged,
                    a * 100.0
                ),
            ),
        }
    };
    Floor {
        stands,
        reviews,
        needs_reviews: FLOOR_REVIEWS,
        agreement,
        needs_agreement: FLOOR_AGREEMENT,
        why,
    }
}
/// The most reviews kept per tenant.
const REVIEWS_KEPT: usize = 200;

/// A person's word on a run's verdict.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VerdictReview {
    pub run_id: String,
    /// The verdict the platform gave.
    pub verdict_then: String,
    /// The verdict the person says is right: `verified` or `failed`.
    pub verdict_right: String,
    pub agree: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub by: Value,
    pub at: DateTime<Utc>,
}

/// One reviewed run, judged again.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VerifierCase {
    pub run_id: String,
    pub asked: String,
    pub person: String,
    pub then: String,
    pub now: String,
    pub now_reason: String,
    pub agrees: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

/// One judging of the verifier.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VerifierEvaluation {
    pub evaluation_id: String,
    pub judge: String,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    /// running | done | error
    pub status: String,
    pub agreed: usize,
    /// Cases the judge answered: the ratio's denominator. A case the judge
    /// could not answer (a timeout, a transcript gone) is skipped, not
    /// counted against it.
    #[serde(default)]
    pub judged: usize,
    pub total: usize,
    pub cases: Vec<VerifierCase>,
}

fn reviews_ns(tenant: &str) -> String {
    format!("verdict_reviews:{tenant}")
}
fn evaluations_ns(tenant: &str) -> String {
    format!("verifier_evaluations:{tenant}")
}

pub(crate) async fn reviews_of(state: &AppState, tenant: &str) -> Vec<VerdictReview> {
    state
        .server_store
        .kv_get(&reviews_ns(tenant), "all")
        .await
        .ok()
        .flatten()
        .and_then(|item| serde_json::from_value(item.value).ok())
        .unwrap_or_default()
}

async fn keep_reviews(
    state: &AppState,
    tenant: &str,
    all: &[VerdictReview],
) -> Result<(), ApiError> {
    state
        .server_store
        .kv_put(
            &reviews_ns(tenant),
            "all",
            serde_json::to_value(all).unwrap_or(Value::Null),
        )
        .await
        .map(|_| ())
        .map_err(ApiError::internal)
}

pub(crate) async fn latest_evaluation(
    state: &AppState,
    tenant: &str,
) -> Option<VerifierEvaluation> {
    state
        .server_store
        .kv_get(&evaluations_ns(tenant), "latest")
        .await
        .ok()
        .flatten()
        .and_then(|item| serde_json::from_value(item.value).ok())
}

async fn keep_evaluation(state: &AppState, tenant: &str, evaluation: &VerifierEvaluation) {
    if let Err(error) = state
        .server_store
        .kv_put(
            &evaluations_ns(tenant),
            "latest",
            serde_json::to_value(evaluation).unwrap_or(Value::Null),
        )
        .await
    {
        tracing::warn!(%error, "verifier evaluation not kept");
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReviewPayload {
    /// Whether the platform's verdict was right.
    pub agree: bool,
    /// When it was wrong, the right one: `verified` or `failed`. Defaults
    /// to the opposite of what the platform said.
    #[serde(default)]
    pub verdict: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /runs/{id}/verdict/review` — a person says whether the verdict
/// was right. One review per run, the latest word kept.
pub(crate) async fn review_verdict(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::Extension(tenant): axum::Extension<TenantContext>,
    axum::extract::Path(run_id): axum::extract::Path<String>,
    axum::Json(payload): axum::Json<ReviewPayload>,
) -> Result<axum::Json<Value>, ApiError> {
    let Some(verdict) = state.verifications.load_verdict(&run_id) else {
        return Err(ApiError::not_found(format!(
            "run `{run_id}` has no verdict to review"
        )));
    };
    if state
        .verifications
        .load_transcript(&run_id)
        .is_none_or(|t| t.is_empty())
    {
        return Err(ApiError::unprocessable(format!(
            "run `{run_id}`'s verdict kept no transcript (judged before transcripts were kept); it cannot be judged again, so it is not a case"
        )));
    }
    let then = verdict.verdict.clone();
    let right = match payload
        .verdict
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        Some(v) if v == "verified" || v == "failed" => v.to_owned(),
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "the right verdict is `verified` or `failed`, not `{other}`"
            )));
        }
        None if payload.agree => then.clone(),
        None => {
            if then == "verified" {
                "failed".to_owned()
            } else {
                "verified".to_owned()
            }
        }
    };
    let review = VerdictReview {
        run_id: run_id.clone(),
        verdict_then: then,
        verdict_right: right,
        agree: payload.agree,
        note: payload
            .note
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty()),
        by: tenant.attribution(),
        at: Utc::now(),
    };
    let mut all = reviews_of(&state, tenant.tenant()).await;
    all.retain(|r| r.run_id != run_id);
    all.insert(0, review.clone());
    all.truncate(REVIEWS_KEPT);
    keep_reviews(&state, tenant.tenant(), &all).await?;
    Ok(axum::Json(json!({"review": review, "reviews": all.len()})))
}

/// `GET /verifier/evidence` — how many verdicts people reviewed, and the
/// verifier's latest judging against them.
pub(crate) async fn get_evidence(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::Extension(tenant): axum::Extension<TenantContext>,
) -> Result<axum::Json<Value>, ApiError> {
    let reviews = reviews_of(&state, tenant.tenant()).await;
    let latest = latest_evaluation(&state, tenant.tenant()).await;
    let disagreed = reviews.iter().filter(|r| !r.agree).count();
    let floor = floor(&state, tenant.tenant()).await;
    Ok(axum::Json(json!({
        "reviews": reviews.len(),
        "disagreed": disagreed,
        "floor": floor,
        "judge": state.run_deps.verifier.as_ref().map(|_| state.model_handle.as_ref().map(|h| h.label()).unwrap_or_else(|| "the deployment's judge".to_owned())),
        "latest": latest,
        "recent": reviews.iter().take(10).collect::<Vec<_>>(),
    })))
}

/// `POST /verifier/evaluations` — judge the verifier: every reviewed run's
/// kept transcript goes past the judge again; the answer is where it
/// agrees with the person. Answers at once with the running record.
pub(crate) async fn start_evaluation(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::Extension(tenant): axum::Extension<TenantContext>,
) -> Result<(axum::http::StatusCode, axum::Json<VerifierEvaluation>), ApiError> {
    if state.run_deps.verifier.is_none() {
        return Err(ApiError::unprocessable(
            "this deployment has no judge: nothing verifies runs, so there is nothing to judge"
                .to_owned(),
        ));
    }
    let reviews: Vec<VerdictReview> = reviews_of(&state, tenant.tenant())
        .await
        .into_iter()
        .take(CASES_PER_EVALUATION)
        .collect();
    if reviews.is_empty() {
        return Err(ApiError::unprocessable("nobody has reviewed a verdict yet: open a run in Observe and say whether its verdict was right".to_owned()));
    }
    let evaluation = VerifierEvaluation {
        evaluation_id: uuid::Uuid::new_v4().to_string(),
        judge: state
            .model_handle
            .as_ref()
            .map(|h| h.label())
            .unwrap_or_else(|| "the deployment's judge".to_owned()),
        started_at: Utc::now(),
        finished_at: None,
        status: "running".to_owned(),
        agreed: 0,
        judged: 0,
        total: reviews.len(),
        cases: Vec::new(),
    };
    keep_evaluation(&state, tenant.tenant(), &evaluation).await;
    let started = evaluation.clone();
    let tenant_name = tenant.tenant().to_owned();
    tokio::spawn(async move {
        let mut evaluation = evaluation;
        for review in reviews {
            let case = judge_again(&state, &review).await;
            if case.skipped.is_none() {
                evaluation.judged += 1;
            }
            if case.agrees {
                evaluation.agreed += 1;
            }
            evaluation.cases.push(case);
            keep_evaluation(&state, &tenant_name, &evaluation).await;
        }
        evaluation.status = "done".to_owned();
        evaluation.finished_at = Some(Utc::now());
        keep_evaluation(&state, &tenant_name, &evaluation).await;
    });
    Ok((axum::http::StatusCode::ACCEPTED, axum::Json(started)))
}

/// One reviewed run past the judge again.
async fn judge_again(state: &AppState, review: &VerdictReview) -> VerifierCase {
    let mut case = VerifierCase {
        run_id: review.run_id.clone(),
        asked: String::new(),
        person: review.verdict_right.clone(),
        then: review.verdict_then.clone(),
        now: String::new(),
        now_reason: String::new(),
        agrees: false,
        skipped: None,
    };
    let Some(kept) = state.verifications.load_verdict(&review.run_id) else {
        case.skipped = Some("the verdict is gone".to_owned());
        return case;
    };
    let Some(transcript) = state
        .verifications
        .load_transcript(&review.run_id)
        .filter(|t| !t.is_empty())
    else {
        case.skipped = Some("the verdict kept no transcript".to_owned());
        return case;
    };
    case.asked = transcript
        .iter()
        .rev()
        .find(|m| m.role == rusty_agent_runtime::llm::Role::User)
        .and_then(|m| m.content.clone())
        .map(|c| c.chars().take(160).collect())
        .unwrap_or_default();
    let Some(judge) = state.run_deps.verifier.as_ref() else {
        case.skipped = Some("no judge".to_owned());
        return case;
    };
    let charter = transcript
        .first()
        .filter(|m| m.role == rusty_agent_runtime::llm::Role::System)
        .and_then(|m| m.content.clone());
    let catalog = kept
        .graph
        .as_deref()
        .map(|g| state.run_deps.registry.tool_capabilities(g))
        .unwrap_or_default();
    let now =
        crate::verify_outcome::verify(judge.0.as_ref(), charter.as_deref(), &transcript, &catalog)
            .await;
    case.now = now.verdict.clone();
    case.now_reason = now.reason.chars().take(300).collect();
    // `unverified` is the judge not answering (a timeout, no answer it
    // could parse) — not a verdict, so not a disagreement.
    if case.now == "unverified" {
        case.skipped = Some(format!("the judge gave no verdict: {}", case.now_reason));
    }
    case.agrees = case.skipped.is_none() && case.now == case.person;
    case
}
