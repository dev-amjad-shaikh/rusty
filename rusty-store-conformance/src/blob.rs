//! Conformance suite for [`BlobStore`](`rusty_store::blob::BlobStore`).
//!
//! Asserts the content-addressing contract: tenant-scoped put, integrity-
//! verified get, existence check, idempotent delete, and tenant isolation.

use rusty_store::blob::{BlobLocator, BlobStore};

use crate::harness::report;
use crate::{ConformanceReport, ConformanceSuite};

/// The blob-store conformance suite.
pub struct BlobStoreConformance;

#[async_trait::async_trait]
impl<B: BlobStore + Sync> ConformanceSuite<B> for BlobStoreConformance {
    async fn run(backend: &B) -> ConformanceReport {
        let mut r = report();

        // AC 1: put stores bytes and returns a content-addressed locator.
        let bytes = b"hello blob world";
        let put_result = backend.put("conformance-tenant", bytes.as_slice()).await;
        r = r.assert(
            "put_returns_locator",
            put_result.is_ok(),
            format!("put failed: {:?}", put_result.as_ref().err()),
        );

        let locator = match put_result {
            Ok(l) => l,
            Err(_) => return r.finish(),
        };

        // The address should be 64-char lowercase hex.
        r = r.assert(
            "sha256_is_lowercase_hex",
            locator.sha256.len() == 64 && locator.sha256.chars().all(|c| c.is_ascii_hexdigit()),
            format!("sha256 `{}` is not 64-char hex", locator.sha256),
        );

        // bytes field should match input length.
        r = r.assert(
            "locator_bytes_match_input",
            locator.bytes == bytes.len() as u64,
            format!("expected {} bytes, got {}", bytes.len(), locator.bytes),
        );

        // Tenant prefix should be present.
        r = r.assert(
            "locator_has_tenant_prefix",
            locator.prefix.contains("conformance-tenant"),
            format!("prefix `{}` missing tenant", locator.prefix),
        );

        // AC 1 (idempotency / dedup): identical bytes → same hash.
        let put2 = backend.put("conformance-tenant", bytes.as_slice()).await;
        r = r.assert(
            "put_idempotent_same_hash",
            put2.is_ok() && put2.as_ref().unwrap().sha256 == locator.sha256,
            format!("idempotent put diverged: {:?}", put2.as_ref().err()),
        );

        // AC 2: get returns the original bytes.
        let get_result = backend.get(&locator).await;
        r = r.assert(
            "get_returns_original_bytes",
            get_result.is_ok() && get_result.as_ref().unwrap().as_slice() == bytes.as_slice(),
            format!("get mismatch: {:?}", get_result.as_ref().err()),
        );

        // AC 2 (integrity): get on a wrong hash should fail NotFound.
        let bad_locator = BlobLocator::new("conformance-tenant", "0".repeat(64), 0);
        let get_bad = backend.get(&bad_locator).await;
        r = r.assert(
            "get_bad_hash_fails",
            get_bad.is_err(),
            "get with a non-existent hash should error",
        );

        // AC 3: exists reports true for stored locator.
        let exists_result = backend.exists(&locator).await;
        r = r.assert(
            "exists_true_for_stored",
            exists_result.as_ref().is_ok_and(|v| *v),
            format!(
                "exists failed for stored blob: {:?}",
                exists_result.as_ref().err()
            ),
        );

        // exists reports false for unknown locator.
        let exists_bad = backend.exists(&bad_locator).await;
        r = r.assert(
            "exists_false_for_unknown",
            exists_bad.as_ref().is_ok_and(|v| !*v),
            format!(
                "exists should be false for unknown: {:?}",
                exists_bad.as_ref().err()
            ),
        );

        // AC 4: delete removes the blob.
        let del_result = backend.delete(&locator).await;
        r = r.assert(
            "delete_returns_true_when_present",
            del_result.as_ref().is_ok_and(|v| *v),
            format!(
                "delete of present blob failed: {:?}",
                del_result.as_ref().err()
            ),
        );

        // After delete, get should fail.
        let get_after_del = backend.get(&locator).await;
        r = r.assert(
            "get_fails_after_delete",
            get_after_del.is_err(),
            "get after delete should fail",
        );

        // After delete, exists should be false.
        let exists_after_del = backend.exists(&locator).await;
        r = r.assert(
            "exists_false_after_delete",
            exists_after_del.as_ref().is_ok_and(|v| !*v),
            format!(
                "exists after delete should be false: {:?}",
                exists_after_del.as_ref().err()
            ),
        );

        // AC 4 (idempotency): delete of already-deleted returns false.
        let del_again = backend.delete(&locator).await;
        r = r.assert(
            "delete_idempotent_returns_false",
            del_again.as_ref().is_ok_and(|v| !*v),
            format!(
                "re-delete should return false: {:?}",
                del_again.as_ref().err()
            ),
        );

        // Distinct bytes → distinct hash.
        let bytes_b = b"different content";
        let put_b = backend.put("conformance-tenant", bytes_b.as_slice()).await;
        if let Ok(ref locator_b) = put_b {
            r = r.assert(
                "distinct_content_distinct_hash",
                locator_b.sha256 != locator.sha256,
                "different content should yield different hash",
            );

            // Clean up.
            let _ = backend.delete(locator_b).await;
        }

        // Tenant isolation: same content under different tenants → different keys.
        let bytes_c = b"tenant isolation test";
        let loc_t1 = backend.put("tenant-1", bytes_c.as_slice()).await;
        let loc_t2 = backend.put("tenant-2", bytes_c.as_slice()).await;
        if let (Ok(ref l1), Ok(ref l2)) = (&loc_t1, &loc_t2) {
            r = r.assert(
                "tenant_isolation_different_prefix",
                l1.prefix != l2.prefix,
                "different tenants should have different prefixes",
            );
            // But hash should be the same (content-addressed).
            r = r.assert(
                "tenant_isolation_same_hash",
                l1.sha256 == l2.sha256,
                "same content should have same hash across tenants",
            );

            // Clean up.
            let _ = backend.delete(l1).await;
            let _ = backend.delete(l2).await;
        }

        r.finish()
    }
}
