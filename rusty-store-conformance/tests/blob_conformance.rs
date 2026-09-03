//! Integration test: blob conformance suite against [`LocalBlobStore`].

use rusty_store::blob::LocalBlobStore;
use rusty_store_conformance::blob::BlobStoreConformance;
use rusty_store_conformance::ConformanceSuite;

#[tokio::test]
async fn local_blob_store_passes_conformance() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalBlobStore::new(dir.path()).unwrap();
    let report = BlobStoreConformance::run(&store).await;
    report.assert_passed();
}
