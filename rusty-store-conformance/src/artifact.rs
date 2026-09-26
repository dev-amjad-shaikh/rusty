//! Conformance suite for [`ArtifactStore`](`rusty_agent_runtime::journal::ArtifactStore`).
//!
//! Asserts the content-addressing contract: idempotent put, integrity-verified
//! get, existence check, and idempotent delete.

use rusty_agent_runtime::journal::ArtifactStore;

use crate::harness::report;
use crate::{ConformanceReport, ConformanceSuite};

/// The artifact-store conformance suite.
pub struct ArtifactStoreConformance;

#[async_trait::async_trait]
impl<B: ArtifactStore + Sync> ConformanceSuite<B> for ArtifactStoreConformance {
    async fn run(backend: &B) -> ConformanceReport {
        let mut r = report();

        // AC 1: put stores bytes and returns a content address.
        let bytes = b"hello artifact world";
        let put_result = backend.put(bytes).await;
        r = r.assert(
            "put_returns_artifact_ref",
            put_result.is_ok(),
            format!("put failed: {:?}", put_result.as_ref().err()),
        );

        let artifact = match put_result {
            Ok(a) => a,
            Err(_) => return r.finish(),
        };

        // The address should be a hex string.
        r = r.assert(
            "sha256_is_lowercase_hex",
            artifact.sha256.len() == 64 && artifact.sha256.chars().all(|c| c.is_ascii_hexdigit()),
            format!("sha256 `{}` is not 64-char lowercase hex", artifact.sha256),
        );

        // bytes field should match input length.
        r = r.assert(
            "artifact_ref_bytes_match_input",
            artifact.bytes == bytes.len() as u64,
            format!("expected {} bytes, got {}", bytes.len(), artifact.bytes),
        );

        // AC 1 (idempotency): identical bytes → same address.
        let put2 = backend.put(bytes).await;
        r = r.assert(
            "put_idempotent_same_address",
            put2.is_ok() && put2.as_ref().unwrap().sha256 == artifact.sha256,
            format!("idempotent put diverged: {:?}", put2.as_ref().err()),
        );

        // AC 2: get returns the original bytes.
        let get_result = backend.get(&artifact.sha256).await;
        r = r.assert(
            "get_returns_original_bytes",
            get_result.is_ok() && get_result.as_ref().unwrap().as_slice() == bytes.as_slice(),
            format!("get mismatch: {:?}", get_result.as_ref().err()),
        );

        // AC 2 (integrity): get on a wrong hash should fail.
        let bad_hash = "0".repeat(64);
        let get_bad = backend.get(&bad_hash).await;
        r = r.assert(
            "get_bad_hash_fails",
            get_bad.is_err(),
            "get with a non-existent hash should error",
        );

        // AC 3: contains reports true for stored address.
        let contains_result = backend.contains(&artifact.sha256).await;
        r = r.assert(
            "contains_true_for_stored",
            contains_result.as_ref().is_ok_and(|v| *v),
            format!(
                "contains failed for stored artifact: {:?}",
                contains_result.as_ref().err()
            ),
        );

        // contains reports false for unknown address.
        let contains_bad = backend.contains(&bad_hash).await;
        r = r.assert(
            "contains_false_for_unknown",
            contains_bad.as_ref().is_ok_and(|v| !*v),
            format!(
                "contains should be false for unknown: {:?}",
                contains_bad.as_ref().err()
            ),
        );

        // AC 4: delete removes the artifact.
        let del_result = backend.delete(&artifact.sha256).await;
        r = r.assert(
            "delete_returns_true_when_present",
            del_result.as_ref().is_ok_and(|v| *v),
            format!(
                "delete of present artifact failed: {:?}",
                del_result.as_ref().err()
            ),
        );

        // After delete, get should fail.
        let get_after_del = backend.get(&artifact.sha256).await;
        r = r.assert(
            "get_fails_after_delete",
            get_after_del.is_err(),
            "get after delete should fail",
        );

        // After delete, contains should be false.
        let contains_after_del = backend.contains(&artifact.sha256).await;
        r = r.assert(
            "contains_false_after_delete",
            contains_after_del.as_ref().is_ok_and(|v| !*v),
            format!(
                "contains after delete should be false: {:?}",
                contains_after_del.as_ref().err()
            ),
        );

        // AC 4 (idempotency): delete of already-deleted returns false.
        let del_again = backend.delete(&artifact.sha256).await;
        r = r.assert(
            "delete_idempotent_returns_false",
            del_again.as_ref().is_ok_and(|v| !*v),
            format!(
                "re-delete should return false: {:?}",
                del_again.as_ref().err()
            ),
        );

        // Distinct bytes → distinct address.
        let bytes_b = b"different content";
        let put_b = backend.put(bytes_b).await;
        if let Ok(ref artifact_b) = put_b {
            r = r.assert(
                "distinct_content_distinct_address",
                artifact_b.sha256 != artifact.sha256,
                "different content should yield different hash",
            );
        }

        r.finish()
    }
}
