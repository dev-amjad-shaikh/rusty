//! Server-side repair-record query tests (EP-10-S01 AC 3).

use std::sync::Arc;

use chrono::{Duration, TimeZone, Utc};
use rusty_agent_runtime::repair::{
    FileRepairLedger, RepairAction, RepairComponent, RepairLedger, RepairOutcome,
    RepairRecordBuilder, RepairRung, RepairTrigger,
};

fn tmp_dir() -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("rusty-repair-test-{}", uuid::Uuid::new_v4()));
    path
}

/// A file-backed ledger persists and serves queries.
#[test]
fn file_ledger_persists_and_queries() {
    let dir = tmp_dir();
    let ledger = FileRepairLedger::new(&dir);

    let start = Utc.with_ymd_and_hms(2026, 1, 15, 10, 0, 0).unwrap();
    let record = RepairRecordBuilder::new()
        .record_id("rr-test-1")
        .component(RepairComponent::ProviderSeam)
        .trigger(RepairTrigger::ProviderError {
            error_class: "transient".to_owned(),
            provider_call_id: "pc-1".to_owned(),
        })
        .action(RepairAction::ProviderRetry {
            rung: RepairRung::InTurn,
        })
        .outcome(RepairOutcome::Repaired)
        .start_time(start)
        .end_time(start + Duration::milliseconds(150))
        .build();

    ledger.append(record.clone()).unwrap();
    assert_eq!(ledger.len().unwrap(), 1);

    let queried = ledger.query(&Default::default()).unwrap();
    assert_eq!(queried.len(), 1);
    assert_eq!(queried[0].record_id, "rr-test-1");

    std::fs::remove_dir_all(&dir).ok();
}

/// Query by component filters to the audit store.
#[test]
fn file_ledger_query_by_component() {
    let dir = tmp_dir();
    let ledger = FileRepairLedger::new(&dir);

    let start = Utc.with_ymd_and_hms(2026, 1, 15, 10, 0, 0).unwrap();
    let r1 = RepairRecordBuilder::new()
        .record_id("rr-1")
        .component(RepairComponent::ProviderSeam)
        .trigger(RepairTrigger::ProviderError {
            error_class: "transient".to_owned(),
            provider_call_id: "pc-1".to_owned(),
        })
        .action(RepairAction::ProviderRetry {
            rung: RepairRung::InTurn,
        })
        .outcome(RepairOutcome::Repaired)
        .start_time(start)
        .end_time(start + Duration::milliseconds(10))
        .build();
    let r2 = RepairRecordBuilder::new()
        .record_id("rr-2")
        .component(RepairComponent::ToolPipeline)
        .trigger(RepairTrigger::ValidationFailure {
            tool_call_id: "tc-1".to_owned(),
        })
        .action(RepairAction::ConversationalRetry {
            rung: RepairRung::InTurn,
        })
        .outcome(RepairOutcome::Escalated)
        .start_time(start + Duration::minutes(1))
        .end_time(start + Duration::minutes(1) + Duration::milliseconds(10))
        .build();

    ledger.append(r1).unwrap();
    ledger.append(r2).unwrap();

    use rusty_agent_runtime::repair::RepairQuery;
    let filter = RepairQuery {
        components: vec![RepairComponent::ProviderSeam],
        ..Default::default()
    };
    let matched = ledger.query(&filter).unwrap();
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0].record_id, "rr-1");

    std::fs::remove_dir_all(&dir).ok();
}

/// A new FileRepairLedger loads previously persisted records.
#[test]
fn file_ledger_survives_reopen() {
    let dir = tmp_dir();
    let ledger = FileRepairLedger::new(&dir);

    let start = Utc.with_ymd_and_hms(2026, 1, 15, 10, 0, 0).unwrap();
    let record = RepairRecordBuilder::new()
        .record_id("rr-survive")
        .component(RepairComponent::OrphanSweep)
        .trigger(RepairTrigger::HeartbeatLost {
            attempt_id: "att-1".to_owned(),
        })
        .action(RepairAction::OrphanSweep {
            rung: RepairRung::Attempt,
        })
        .outcome(RepairOutcome::Repaired)
        .start_time(start)
        .end_time(start + Duration::milliseconds(50))
        .build();

    ledger.append(record).unwrap();

    // Re-open the same directory with a new ledger instance.
    let ledger2 = FileRepairLedger::new(&dir);
    let queried = ledger2.query(&Default::default()).unwrap();
    assert_eq!(queried.len(), 1);
    assert_eq!(queried[0].record_id, "rr-survive");

    std::fs::remove_dir_all(&dir).ok();
}

/// RepairLedger trait is object-safe (can be held in Arc<dyn>).
#[test]
fn repair_ledger_is_object_safe() {
    let dir = tmp_dir();
    let _ledger: Arc<dyn rusty_agent_runtime::repair::RepairLedger> =
        Arc::new(FileRepairLedger::new(&dir));
    std::fs::remove_dir_all(&dir).ok();
}
