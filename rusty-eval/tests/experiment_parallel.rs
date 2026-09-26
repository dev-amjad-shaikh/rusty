use std::cell::Cell;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusty_agent_runtime::error::Result as RuntimeResult;
use rusty_agent_runtime::graph::GraphBuilder;
use rusty_agent_runtime::node::NodeOutput;
use rusty_agent_runtime::state::StateSpec;
use rusty_eval::{
    Dataset, EvalCase, EvalError, Expectation, ExperimentConfig, ExperimentRunner, JudgeModel,
    JudgeRequest, JudgeVerdict, PreparedRun,
};
use serde_json::json;

#[derive(Default)]
struct ConcurrencyProbe {
    active: AtomicUsize,
    peak: AtomicUsize,
    completed: AtomicUsize,
}

struct ActiveRun {
    probe: Arc<ConcurrencyProbe>,
}

impl ActiveRun {
    fn start(probe: Arc<ConcurrencyProbe>) -> Self {
        let active = probe.active.fetch_add(1, Ordering::SeqCst) + 1;
        probe.peak.fetch_max(active, Ordering::SeqCst);
        Self { probe }
    }
}

impl Drop for ActiveRun {
    fn drop(&mut self) {
        self.probe.active.fetch_sub(1, Ordering::SeqCst);
        self.probe.completed.fetch_add(1, Ordering::SeqCst);
    }
}

fn dataset(case_count: usize) -> Dataset {
    let cases = (0..case_count)
        .map(|index| EvalCase {
            id: format!("case-{index}"),
            input: json!({"delay_ms": 8 + (case_count - index) as u64 * 4}),
            expect: Expectation::default(),
            tags: Vec::new(),
        })
        .collect();
    Dataset::new("parallel-suite", "1", cases).unwrap()
}

fn prepared_run(delay: Duration, probe: Arc<ConcurrencyProbe>) -> RuntimeResult<PreparedRun> {
    let mut builder = GraphBuilder::new();
    builder.add_node("work", move |_context| {
        let probe = Arc::clone(&probe);
        async move {
            let _active = ActiveRun::start(probe);
            tokio::time::sleep(delay).await;
            Ok(NodeOutput::empty())
        }
    });
    builder.set_entry_point("work");
    Ok(PreparedRun::new(builder.compile()?, StateSpec::new()))
}

#[tokio::test]
async fn parallel_runs_are_bounded_isolated_and_reported_in_source_order() {
    let dataset = dataset(4);
    let probe = Arc::new(ConcurrencyProbe::default());
    let journal_ids = Arc::new(Mutex::new(BTreeSet::new()));
    let runner = ExperimentRunner::new(
        ExperimentConfig::new()
            .with_runs_per_case(2)
            .with_max_concurrency(3),
    );

    let report = runner
        .run(&dataset, |case, journal| {
            assert!(journal_ids
                .lock()
                .unwrap()
                .insert(journal.run_id().to_owned()));
            prepared_run(
                Duration::from_millis(case.input["delay_ms"].as_u64().unwrap()),
                Arc::clone(&probe),
            )
        })
        .await
        .unwrap();

    assert_eq!(probe.peak.load(Ordering::SeqCst), 3);
    assert_eq!(probe.active.load(Ordering::SeqCst), 0);
    assert_eq!(probe.completed.load(Ordering::SeqCst), 8);
    assert_eq!(journal_ids.lock().unwrap().len(), 8);
    assert_eq!(report.max_concurrency, 3);
    assert_eq!(
        report
            .cases
            .iter()
            .map(|case| case.case_id.as_str())
            .collect::<Vec<_>>(),
        vec!["case-0", "case-1", "case-2", "case-3"]
    );
    assert!(report
        .cases
        .iter()
        .all(|case| { case.runs.iter().map(|run| run.repetition).eq([0, 1]) }));
}

#[tokio::test]
async fn sequential_default_never_overlaps_case_runs() {
    let dataset = dataset(3);
    let probe = Arc::new(ConcurrencyProbe::default());
    let runner = ExperimentRunner::new(
        ExperimentConfig::default()
            .with_runs_per_case(2)
            .with_max_concurrency(0),
    );

    let report = runner
        .run(&dataset, |case, _journal| {
            prepared_run(
                Duration::from_millis(case.input["delay_ms"].as_u64().unwrap()),
                Arc::clone(&probe),
            )
        })
        .await
        .unwrap();

    assert_eq!(report.summary.runs, 6);
    assert_eq!(report.max_concurrency, 1);
    assert_eq!(probe.peak.load(Ordering::SeqCst), 1);
    assert_eq!(probe.completed.load(Ordering::SeqCst), 6);
}

#[tokio::test]
async fn preparation_factory_retains_its_non_sync_api() {
    let dataset = dataset(2);
    let builds = Cell::new(0);
    let runner = ExperimentRunner::new(ExperimentConfig::new().with_max_concurrency(2));

    let report = runner
        .run(&dataset, |_case, _journal| {
            builds.set(builds.get() + 1);
            prepared_run(Duration::ZERO, Arc::new(ConcurrencyProbe::default()))
        })
        .await
        .unwrap();

    assert_eq!(builds.get(), 2);
    assert_eq!(report.summary.runs, 2);
}

struct FailingJudge {
    completed: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl JudgeModel for FailingJudge {
    async fn judge(&self, request: &JudgeRequest) -> rusty_eval::Result<JudgeVerdict> {
        let delay = if request.case_id == "case-0" { 30 } else { 1 };
        tokio::time::sleep(Duration::from_millis(delay)).await;
        self.completed.fetch_add(1, Ordering::SeqCst);
        Err(EvalError::Judge(format!(
            "{} judge unavailable",
            request.case_id
        )))
    }
}

#[tokio::test]
async fn concurrent_failures_return_the_earliest_source_coordinate() {
    let dataset = dataset(4);
    let completed = Arc::new(AtomicUsize::new(0));
    let judge = Arc::new(FailingJudge {
        completed: Arc::clone(&completed),
    });
    let runner = ExperimentRunner::new(
        ExperimentConfig::new()
            .with_max_concurrency(2)
            .with_judge(judge),
    );

    let error = runner
        .run(&dataset, |_case, _journal| {
            prepared_run(Duration::ZERO, Arc::new(ConcurrencyProbe::default()))
        })
        .await
        .unwrap_err();

    assert_eq!(completed.load(Ordering::SeqCst), 2);
    assert!(error.to_string().contains("case `case-0`"));
    assert!(!error.to_string().contains("case-1 judge unavailable"));
}

#[tokio::test]
async fn sequential_infrastructure_failure_still_fails_fast() {
    let dataset = dataset(2);
    let completed = Arc::new(AtomicUsize::new(0));
    let judge = Arc::new(FailingJudge {
        completed: Arc::clone(&completed),
    });
    let runner = ExperimentRunner::new(ExperimentConfig::new().with_judge(judge));

    let error = runner
        .run(&dataset, |_case, _journal| {
            prepared_run(Duration::ZERO, Arc::new(ConcurrencyProbe::default()))
        })
        .await
        .unwrap_err();

    assert_eq!(completed.load(Ordering::SeqCst), 1);
    assert!(error.to_string().contains("case `case-0`"));
}
