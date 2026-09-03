//! Executable conformance suites for every Rusty store trait.
//!
//! A backend author implements a store trait and runs that trait's full suite
//! by supplying only a constructor for their backend (and a reset hook between
//! cases) — the suite carries its own fixtures, requires no knowledge of the
//! reference schema, and reports each contract assertion pass/fail by name.
//!
//! # Usage
//!
//! ```ignore
//! use rusty_store_conformance::artifact::ArtifactStoreConformance;
//! use rusty_agent_runtime::journal::FileArtifactStore;
//!
//! #[tokio::test]
//! async fn my_backend_passes_artifact_conformance() {
//!     let dir = tempfile::tempdir().unwrap();
//!     let store = FileArtifactStore::new(dir.path());
//!     ArtifactStoreConformance::run(&store).await.assert_passed();
//! }
//! ```

#![warn(missing_docs)]

pub mod artifact;

use std::fmt;

/// One named assertion result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assertion {
    /// The suite-unique assertion name.
    pub name: &'static str,
    /// Pass or fail.
    pub outcome: Outcome,
    /// Detail on failure.
    pub detail: Option<String>,
}

/// Pass or fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The assertion passed.
    Passed,
    /// The assertion failed.
    Failed,
}

/// A collected conformance run: every assertion in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConformanceReport {
    /// The assertions, in execution order.
    pub assertions: Vec<Assertion>,
}

impl ConformanceReport {
    /// `true` when every assertion passed.
    pub fn all_passed(&self) -> bool {
        self.assertions.iter().all(|a| a.outcome == Outcome::Passed)
    }

    /// How many passed.
    pub fn passed_count(&self) -> usize {
        self.assertions
            .iter()
            .filter(|a| a.outcome == Outcome::Passed)
            .count()
    }

    /// How many failed.
    pub fn failed_count(&self) -> usize {
        self.assertions
            .iter()
            .filter(|a| a.outcome == Outcome::Failed)
            .count()
    }

    /// Panic with a formatted summary if any assertion failed.
    pub fn assert_passed(&self) {
        if !self.all_passed() {
            panic!("conformance run failed: {}", self);
        }
    }
}

impl fmt::Display for ConformanceReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "ConformanceReport: {} passed, {} failed",
            self.passed_count(),
            self.failed_count()
        )?;
        for a in &self.assertions {
            match a.outcome {
                Outcome::Passed => writeln!(f, "  [PASS] {}", a.name)?,
                Outcome::Failed => writeln!(
                    f,
                    "  [FAIL] {}: {}",
                    a.name,
                    a.detail.as_deref().unwrap_or("no detail")
                )?,
            }
        }
        Ok(())
    }
}

/// A conformance suite that can run against any backend.
///
/// `B` is the store trait; `S` is the backend under test.
#[async_trait::async_trait]
pub trait ConformanceSuite<B> {
    /// Run every assertion against `backend`, returning a report.
    async fn run(backend: &B) -> ConformanceReport;
}

/// Helpers for writing assertions without boilerplate.
pub mod harness {
    use super::*;

    /// Start building a report.
    pub fn report() -> ReportBuilder {
        ReportBuilder {
            assertions: Vec::new(),
        }
    }

    /// Builder for [`ConformanceReport`].
    pub struct ReportBuilder {
        assertions: Vec<Assertion>,
    }

    impl ReportBuilder {
        /// Record a pass.
        pub fn pass(mut self, name: &'static str) -> Self {
            self.assertions.push(Assertion {
                name,
                outcome: Outcome::Passed,
                detail: None,
            });
            self
        }

        /// Record a fail.
        pub fn fail(mut self, name: &'static str, detail: impl Into<String>) -> Self {
            self.assertions.push(Assertion {
                name,
                outcome: Outcome::Failed,
                detail: Some(detail.into()),
            });
            self
        }

        /// Record based on a predicate.
        pub fn assert(
            mut self,
            name: &'static str,
            condition: bool,
            detail: impl Into<String>,
        ) -> Self {
            if condition {
                self.assertions.push(Assertion {
                    name,
                    outcome: Outcome::Passed,
                    detail: None,
                });
            } else {
                self.assertions.push(Assertion {
                    name,
                    outcome: Outcome::Failed,
                    detail: Some(detail.into()),
                });
            }
            self
        }

        /// Finish.
        pub fn finish(self) -> ConformanceReport {
            ConformanceReport {
                assertions: self.assertions,
            }
        }
    }
}
