//! Conformance suite tests against the built-in backends.

use rusty_agent_runtime::journal::FileArtifactStore;
use rusty_store_conformance::artifact::ArtifactStoreConformance;
use rusty_store_conformance::ConformanceSuite;

/// The JSON-file artifact store must pass every artifact-store assertion.
#[tokio::test]
async fn file_artifact_store_conformance() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileArtifactStore::new(dir.path());
    let report = ArtifactStoreConformance::run(&store).await;
    report.assert_passed();
}
