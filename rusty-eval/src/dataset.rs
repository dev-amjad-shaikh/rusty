//! Versioned evaluation datasets.
//!
//! A dataset is a named, versioned set of [`EvalCase`]s stored as JSONL:
//! line 1 is the header, every following non-blank line is one case.
//!
//! ```jsonl
//! {"kind":"header","format_version":1,"name":"math-tools","version":"1.0.0"}
//! {"kind":"case","id":"add","input":{"messages":[{"role":"user","content":"2+3?"}]},"expect":{"tool_trajectory":[{"name":"calculator","args":{"/op":"add"}}]},"tags":["smoke"]}
//! ```
//!
//! Loading validates the declared `format_version` against
//! [`DATASET_FORMAT_VERSION`] and refuses to guess across versions — a
//! dataset from the future is an error, never a silent misread. Serialization
//! is canonical (struct field order, sorted map keys, compact JSON), so
//! `load → save` is byte-stable and datasets diff cleanly in version control.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::assertion::Assertion;
use crate::error::{EvalError, Result};

/// The dataset format version this build loads and writes.
pub const DATASET_FORMAT_VERSION: u64 = 1;

/// One line of the JSONL dataset file. The `kind` tag is written first, so
/// headers and cases are greppable without a JSON parser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DatasetLine {
    /// Line 1: dataset identity and format version.
    Header {
        /// Schema version of the file; must equal [`DATASET_FORMAT_VERSION`].
        format_version: u64,
        /// Dataset name (e.g. `math-tools`).
        name: String,
        /// Dataset version (semver string, bumped by the author on any case
        /// or expectation change).
        version: String,
    },
    /// One evaluation case.
    Case(EvalCase),
}

/// One evaluation case: an input payload plus what a correct run looks like.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalCase {
    /// Stable case id, unique within the dataset. Reports and comparisons key
    /// on it, so renaming an id retires the old case's history.
    pub id: String,

    /// The run input payload. By default the experiment runner merges this
    /// into the run's initial [`rusty_agent_runtime::state::State`], so it
    /// must be a JSON object whose keys are state channels (e.g. `messages`
    /// for the prebuilt ReAct agent). Agent factories may interpret it
    /// differently by returning an explicit initial state.
    pub input: Value,

    /// What a correct run looks like, converted into [`Assertion`]s at
    /// evaluation time.
    #[serde(default)]
    pub expect: Expectation,

    /// Free-form labels for slicing reports (`smoke`, `regression`, ...).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// The declarative expectations of an [`EvalCase`].
///
/// Expectations stay data (JSON-serializable, diffable); they become
/// executable [`Assertion`]s via [`Expectation::assertions`]. Empty sections
/// are omitted on the wire, so a case declares only what it checks.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Expectation {
    /// The tool calls a correct run makes, as an ordered subsequence: each
    /// expected call must appear in order, but the run may make additional
    /// calls between them. Empty means "no trajectory requirement" — use
    /// `forbid_tools` to require tool-free runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_trajectory: Vec<ExpectedToolCall>,

    /// Predicates over the run's final state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub state: Vec<StatePredicate>,

    /// Tools a correct run never calls.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forbid_tools: Vec<String>,

    /// Upper bound on the run's total journaled cost in USD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,

    /// Upper bound on the run's wall latency in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_latency_ms: Option<u64>,
}

impl Expectation {
    /// `true` when the case declares no expectations at all.
    pub fn is_empty(&self) -> bool {
        self.tool_trajectory.is_empty()
            && self.state.is_empty()
            && self.forbid_tools.is_empty()
            && self.max_cost_usd.is_none()
            && self.max_latency_ms.is_none()
    }

    /// Convert into executable assertions. Each expectation section maps to
    /// at most one assertion (`state` maps to one per predicate), so a case
    /// with a trajectory, two state predicates, and a latency bound yields
    /// four assertions, each reported and aggregated separately.
    pub fn assertions(&self) -> Vec<Assertion> {
        let mut out = Vec::new();
        if !self.tool_trajectory.is_empty() {
            out.push(Assertion::ToolCallOrder {
                expected: self.tool_trajectory.clone(),
            });
        }
        for predicate in &self.state {
            out.push(Assertion::StatePredicate {
                pointer: predicate.pointer.clone(),
                expected: predicate.expected.clone(),
            });
        }
        if !self.forbid_tools.is_empty() {
            out.push(Assertion::NoToolCall {
                names: self.forbid_tools.clone(),
            });
        }
        if let Some(usd) = self.max_cost_usd {
            out.push(Assertion::MaxCost { usd });
        }
        if let Some(ms) = self.max_latency_ms {
            out.push(Assertion::MaxLatency { ms });
        }
        out
    }
}

/// One expected tool call in a trajectory: a name, plus optional matchers
/// against its arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExpectedToolCall {
    /// The tool name, exactly as journaled (the `tool` field of the canonical
    /// tool-call request payload).
    pub name: String,

    /// Argument matchers: JSON pointer (RFC 6901) into the call's arguments
    /// mapped to the expected value. `{"op": "add"}`-style checks are written
    /// `{"/op": "add"}`; the empty pointer `""` matches the whole arguments
    /// value. An empty map matches any arguments.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub args: Map<String, Value>,
}

impl ExpectedToolCall {
    /// Name-only expectation (any arguments).
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            args: Map::new(),
        }
    }

    /// `true` when `arguments` satisfies every matcher.
    pub fn matches_arguments(&self, arguments: &Value) -> bool {
        self.args
            .iter()
            .all(|(pointer, expected)| arguments.pointer(pointer) == Some(expected))
    }
}

/// One predicate over the run's final state: the value at a JSON pointer
/// must equal `expected`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatePredicate {
    /// JSON pointer (RFC 6901) into the final state object, e.g.
    /// `/messages/3/content`.
    pub pointer: String,

    /// The expected value, compared with `==`.
    pub expected: Value,
}

/// A named, versioned set of evaluation cases.
#[derive(Debug, Clone, PartialEq)]
pub struct Dataset {
    name: String,
    version: String,
    cases: Vec<EvalCase>,
}

impl Dataset {
    /// Build a dataset in memory. Case ids must be unique.
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        cases: Vec<EvalCase>,
    ) -> Result<Self> {
        let dataset = Self {
            name: name.into(),
            version: version.into(),
            cases,
        };
        dataset.validate_ids()?;
        Ok(dataset)
    }

    /// The dataset name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The dataset version.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The cases, in file order.
    pub fn cases(&self) -> &[EvalCase] {
        &self.cases
    }

    /// Parse a dataset from JSONL text. Blank lines are ignored; the first
    /// non-blank line must be the header.
    pub fn from_jsonl(text: &str) -> Result<Self> {
        let mut header: Option<(String, String)> = None;
        let mut cases = Vec::new();

        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let line_no = index + 1;
            let parsed: DatasetLine = serde_json::from_str(line)
                .map_err(|e| EvalError::Dataset(format!("line {line_no}: malformed JSON: {e}")))?;
            match parsed {
                DatasetLine::Header {
                    format_version,
                    name,
                    version,
                } => {
                    if header.is_some() || !cases.is_empty() {
                        return Err(EvalError::Dataset(format!(
                            "line {line_no}: header must be the first non-blank line"
                        )));
                    }
                    if format_version != DATASET_FORMAT_VERSION {
                        return Err(EvalError::UnsupportedVersion {
                            found: format_version,
                            supported: DATASET_FORMAT_VERSION,
                        });
                    }
                    header = Some((name, version));
                }
                DatasetLine::Case(case) => {
                    if header.is_none() {
                        return Err(EvalError::Dataset(format!(
                            "line {line_no}: case before the header"
                        )));
                    }
                    cases.push(case);
                }
            }
        }

        let (name, version) = header
            .ok_or_else(|| EvalError::Dataset("missing header line".to_owned()))?;
        Self::new(name, version, cases)
    }

    /// Load a dataset from a JSONL file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path.as_ref())?;
        Self::from_jsonl(&text)
            .map_err(|e| EvalError::Dataset(format!("{}: {e}", path.as_ref().display())))
    }

    /// Serialize to canonical JSONL (trailing newline on every line).
    pub fn to_jsonl(&self) -> String {
        let mut out = String::new();
        let header = DatasetLine::Header {
            format_version: DATASET_FORMAT_VERSION,
            name: self.name.clone(),
            version: self.version.clone(),
        };
        // Serialization of a struct we fully control cannot fail.
        out.push_str(&serde_json::to_string(&header).expect("dataset header serializes"));
        out.push('\n');
        for case in &self.cases {
            out.push_str(
                &serde_json::to_string(&DatasetLine::Case(case.clone()))
                    .expect("dataset case serializes"),
            );
            out.push('\n');
        }
        out
    }

    /// Save the dataset as canonical JSONL.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        std::fs::write(path.as_ref(), self.to_jsonl())?;
        Ok(())
    }

    fn validate_ids(&self) -> Result<()> {
        let mut seen = HashSet::with_capacity(self.cases.len());
        for case in &self.cases {
            if case.id.is_empty() {
                return Err(EvalError::Dataset("case id must not be empty".to_owned()));
            }
            if !seen.insert(case.id.as_str()) {
                return Err(EvalError::Dataset(format!(
                    "duplicate case id `{}`",
                    case.id
                )));
            }
        }
        Ok(())
    }
}
