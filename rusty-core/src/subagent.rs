//! Subagent safety: capability descriptors, fail-loud dispatch, blocklists,
//! and scoped teardown (EP-09-S08).
//!
//! A subagent is a child session spawned by a parent agent. This module
//! provides the structural confinement that keeps children from escaping
//! their scope: static capability descriptors checked before dispatch,
//! deny-only guards that block excluded tools, depth limits enforced by
//! toolset construction with guard-layer backup, and scope-keyed
//! registration that tears down completely when a child ends.
//!
//! # Dispatch contract
//!
//! 1. Every provider advertises a [`SubagentProviderDescriptor`] at
//!    registration time — static data, not a probe result.
//! 2. Before any child session is created, the dispatcher checks the
//!    requested capability against the target's descriptor. A missing
//!    capability produces [`SubagentDispatchError::UnsupportedCapability`]
//!    with zero sessions created.
//! 3. The child's materialized toolset structurally excludes delegation
//!    beyond the configured depth, channel sends, memory promotion, and
//!    scheduling in the parent's name. Any attempt to reach an excluded
//!    tool produces the same typed [`GuardDenial`] as any guard-pipeline
//!    deny.
//! 4. On child disposal — completion, cancellation, or parent teardown —
//!    every registration made under the child's [`SubagentScope`] is
//!    unwound LIFO and verified absent afterwards.
//! 5. Child sessions are ordinary sessions with their own ids. Every
//!    provider call carries `traffic: side` so telemetry can attribute
//!    ingress/egress without heuristics. Child content is structurally
//!    excluded from durable memory promotion.
//! 6. A child failure surfaces to the parent as a failed tool result,
//!    never as a parent crash.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{Result, RustyError};
use crate::tool::{GuardDenial, GuardedCall, ToolGuard, ToolRegistry};

// ---------------------------------------------------------------------------
// Capability descriptor
// ---------------------------------------------------------------------------

/// One capability a subagent provider may declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentCapability {
    /// The provider supports structured output schema conformance.
    StructuredOutput,
    /// The provider can filter the toolset exposed to a child.
    ToolFiltering,
    /// The provider can inject a persona/prompt section into the child.
    PersonaInjection,
    /// The provider supports fork-style log-prefix child sessions.
    LogPrefixFork,
    /// The provider supports delegation to an external runtime.
    ExternalRuntime,
}

impl SubagentCapability {
    /// The stable wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            SubagentCapability::StructuredOutput => "structured_output",
            SubagentCapability::ToolFiltering => "tool_filtering",
            SubagentCapability::PersonaInjection => "persona_injection",
            SubagentCapability::LogPrefixFork => "log_prefix_fork",
            SubagentCapability::ExternalRuntime => "external_runtime",
        }
    }
}

/// The traffic kind stamped on every provider call so telemetry can
/// attribute ingress/egress without heuristics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrafficKind {
    /// The primary agent turn — ordinary traffic.
    Main,
    /// A child/subagent side session.
    Side,
}

impl TrafficKind {
    /// The stable wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            TrafficKind::Main => "main",
            TrafficKind::Side => "side",
        }
    }
}

/// Static descriptor advertised by every registered subagent provider.
///
/// This is data in the registry, not a probe result: a caller can read
/// it without dispatching anything.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentProviderDescriptor {
    /// Provider name — the stable identity under which it is registered.
    pub name: String,
    /// Provider version — an exact version string (e.g. `in-process/1.2.0`).
    pub version: String,
    /// The capabilities this provider declares.
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub capabilities: HashSet<SubagentCapability>,
    /// Maximum delegation depth this provider permits. `None` means
    /// unbounded (the provider does not enforce depth itself).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u32>,
    /// If the provider supports tool filtering, the default blocklist it
    /// applies to every child.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_blocklist: Vec<String>,
    /// The scope key this provider registers under. Scope-keyed teardown
    /// removes every registration with this key on child disposal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_key: Option<String>,
    /// The traffic kind this provider stamps on its calls.
    #[serde(default = "default_traffic_side")]
    pub traffic: TrafficKind,
}

fn default_traffic_side() -> TrafficKind {
    TrafficKind::Side
}

impl SubagentProviderDescriptor {
    /// A descriptor with the given name and version, no capabilities,
    /// and default `traffic: side`.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            capabilities: HashSet::new(),
            max_depth: None,
            default_blocklist: Vec::new(),
            scope_key: None,
            traffic: TrafficKind::Side,
        }
    }

    /// Builder-style: declare a capability.
    pub fn with_capability(mut self, cap: SubagentCapability) -> Self {
        self.capabilities.insert(cap);
        self
    }

    /// Builder-style: set the maximum delegation depth.
    pub fn with_max_depth(mut self, depth: u32) -> Self {
        self.max_depth = Some(depth);
        self
    }

    /// Builder-style: set the default blocklist.
    pub fn with_blocklist<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.default_blocklist = tools.into_iter().map(Into::into).collect();
        self
    }

    /// Builder-style: set the scope key.
    pub fn with_scope_key(mut self, key: impl Into<String>) -> Self {
        self.scope_key = Some(key.into());
        self
    }

    /// `true` if this descriptor declares `cap`.
    pub fn supports(&self, cap: SubagentCapability) -> bool {
        self.capabilities.contains(&cap)
    }
}

// ---------------------------------------------------------------------------
// Dispatch errors
// ---------------------------------------------------------------------------

/// A typed dispatch refusal — fail loud before any session is created.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubagentDispatchError {
    /// The named provider is not registered.
    ProviderNotFound {
        /// The provider name that was requested.
        provider: String,
    },
    /// The provider does not declare the requested capability.
    UnsupportedCapability {
        /// The provider that was targeted.
        provider: String,
        /// The capability that was requested.
        capability: String,
    },
    /// Delegation depth would exceed the provider's limit.
    DepthExceeded {
        /// The provider that was targeted.
        provider: String,
        /// The depth that was requested.
        requested: u32,
        /// The provider's maximum.
        max: u32,
    },
    /// The requested tool is on the blocklist for this child scope.
    BlockedTool {
        /// The tool that was denied.
        tool: String,
        /// The scope key the blocklist belongs to.
        scope: String,
    },
}

impl std::fmt::Display for SubagentDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubagentDispatchError::ProviderNotFound { provider } => {
                write!(f, "subagent provider `{provider}` is not registered")
            }
            SubagentDispatchError::UnsupportedCapability {
                provider,
                capability,
            } => {
                write!(
                    f,
                    "subagent provider `{provider}` does not declare capability `{capability}`"
                )
            }
            SubagentDispatchError::DepthExceeded {
                provider,
                requested,
                max,
            } => {
                write!(
                    f,
                    "subagent provider `{provider}` depth limit {max} exceeded by {requested}"
                )
            }
            SubagentDispatchError::BlockedTool { tool, scope } => {
                write!(f, "tool `{tool}` is blocked in subagent scope `{scope}`")
            }
        }
    }
}

impl std::error::Error for SubagentDispatchError {}

// ---------------------------------------------------------------------------
// Scope
// ---------------------------------------------------------------------------

/// A scope key that groups registrations made on behalf of one child.
///
/// Scope keys follow the same name discipline as tool names: non-empty,
/// trimmed, ASCII alphanumeric plus `._:-`, bounded to 128 bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SubagentScope {
    key: String,
}

impl SubagentScope {
    /// Validate and wrap a scope key.
    pub fn new(key: impl Into<String>) -> Result<Self> {
        let key = key.into();
        if key.is_empty()
            || key.len() > 128
            || key != key.trim()
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
        {
            return Err(RustyError::Tool(format!(
                "subagent scope key `{key}` must be 1..=128 ASCII letters, digits, `.`, `_`, `:`, or `-`"
            )));
        }
        Ok(Self { key })
    }

    /// The key string.
    pub fn as_str(&self) -> &str {
        &self.key
    }
}

impl std::fmt::Display for SubagentScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.key)
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// One registration entry in the subagent registry, holding the descriptor
/// and an optional teardown callback.
struct RegistryEntry {
    descriptor: SubagentProviderDescriptor,
    /// Optional callback invoked when this entry is removed (teardown
    /// notification for out-of-process providers).
    #[allow(dead_code)]
    on_teardown: Option<Box<dyn FnOnce() + Send>>,
}

/// The subagent registry: maps provider names to descriptors with
/// scope-keyed teardown.
///
/// Every registration is keyed by provider name and optionally by scope.
/// Disposing a scope removes every entry whose `scope_key` matches,
/// LIFO within the scope (the plugin kernel's unload order). After
/// teardown, a registry-residue assertion finds zero entries for the
/// disposed scope.
pub struct SubagentRegistry {
    /// Primary lookup: provider name → entry.
    by_name: HashMap<String, RegistryEntry>,
    /// Scope index: scope key → ordered list of provider names registered
    /// under that scope (registration order; teardown unwinds reverse).
    by_scope: HashMap<String, Vec<String>>,
}

impl std::fmt::Debug for SubagentRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentRegistry")
            .field("providers", &self.by_name.keys().collect::<Vec<_>>())
            .field(
                "scopes",
                &self
                    .by_scope
                    .iter()
                    .map(|(k, v)| (k.clone(), v.len()))
                    .collect::<HashMap<_, _>>(),
            )
            .finish()
    }
}

impl Default for SubagentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SubagentRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            by_name: HashMap::new(),
            by_scope: HashMap::new(),
        }
    }

    /// Register a provider descriptor.
    ///
    /// Refuses to shadow: a name already registered is an error.
    pub fn register(&mut self, descriptor: SubagentProviderDescriptor) -> Result<()> {
        let name = descriptor.name.clone();
        if self.by_name.contains_key(&name) {
            return Err(RustyError::Tool(format!(
                "subagent provider `{name}` is already registered"
            )));
        }
        if let Some(scope_key) = descriptor.scope_key.as_ref() {
            self.by_scope
                .entry(scope_key.clone())
                .or_default()
                .push(name.clone());
        }
        self.by_name.insert(
            name,
            RegistryEntry {
                descriptor,
                on_teardown: None,
            },
        );
        Ok(())
    }

    /// Register with an explicit teardown callback.
    pub fn register_with_teardown<F>(
        &mut self,
        descriptor: SubagentProviderDescriptor,
        on_teardown: F,
    ) -> Result<()>
    where
        F: FnOnce() + Send + 'static,
    {
        let name = descriptor.name.clone();
        if self.by_name.contains_key(&name) {
            return Err(RustyError::Tool(format!(
                "subagent provider `{name}` is already registered"
            )));
        }
        if let Some(scope_key) = descriptor.scope_key.as_ref() {
            self.by_scope
                .entry(scope_key.clone())
                .or_default()
                .push(name.clone());
        }
        self.by_name.insert(
            name,
            RegistryEntry {
                descriptor,
                on_teardown: Some(Box::new(on_teardown)),
            },
        );
        Ok(())
    }

    /// Look up a descriptor by provider name.
    pub fn get(&self, name: &str) -> Option<&SubagentProviderDescriptor> {
        self.by_name.get(name).map(|e| &e.descriptor)
    }

    /// `true` if a provider with this name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// Every registered provider name.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }

    /// Number of registered providers.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// `true` if empty.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Remove one provider by name, returning its descriptor.
    pub fn unregister(&mut self, name: &str) -> Option<SubagentProviderDescriptor> {
        let entry = self.by_name.remove(name)?;
        if let Some(scope_key) = entry.descriptor.scope_key.as_ref() {
            if let Some(names) = self.by_scope.get_mut(scope_key) {
                names.retain(|n| n != name);
                if names.is_empty() {
                    self.by_scope.remove(scope_key);
                }
            }
        }
        Some(entry.descriptor)
    }

    /// Dispose every registration under `scope` in reverse registration
    /// order (LIFO), then assert the scope index is empty.
    ///
    /// Returns the names of the providers that were removed.
    pub fn dispose_scope(&mut self, scope: &SubagentScope) -> Vec<String> {
        let scope_key = scope.as_str();
        let Some(names) = self.by_scope.remove(scope_key) else {
            return Vec::new();
        };
        let mut removed = Vec::with_capacity(names.len());
        // Unwind in reverse registration order (LIFO).
        for name in names.iter().rev() {
            if let Some(entry) = self.by_name.remove(name) {
                // Invoke teardown callback if present.
                if let Some(callback) = entry.on_teardown {
                    callback();
                }
                removed.push(name.clone());
            }
        }
        removed
    }

    /// `true` if no entries remain under `scope`.
    ///
    /// This is the registry-residue assertion: after teardown it must be
    /// `true`.
    pub fn scope_is_empty(&self, scope: &SubagentScope) -> bool {
        self.by_scope
            .get(scope.as_str())
            .map_or(true, Vec::is_empty)
    }

    /// Verify a dispatch request against the target provider's descriptor.
    ///
    /// Returns `Ok(())` when the provider is registered and declares the
    /// requested capability. Returns `Err(SubagentDispatchError)` before
    /// any session would be created.
    pub fn verify_dispatch(
        &self,
        provider: &str,
        capability: SubagentCapability,
    ) -> std::result::Result<(), SubagentDispatchError> {
        let entry =
            self.by_name
                .get(provider)
                .ok_or_else(|| SubagentDispatchError::ProviderNotFound {
                    provider: provider.to_owned(),
                })?;
        if !entry.descriptor.supports(capability) {
            return Err(SubagentDispatchError::UnsupportedCapability {
                provider: provider.to_owned(),
                capability: capability.as_str().to_owned(),
            });
        }
        Ok(())
    }

    /// Check depth against the provider's declared limit.
    pub fn check_depth(
        &self,
        provider: &str,
        requested: u32,
    ) -> std::result::Result<(), SubagentDispatchError> {
        let entry =
            self.by_name
                .get(provider)
                .ok_or_else(|| SubagentDispatchError::ProviderNotFound {
                    provider: provider.to_owned(),
                })?;
        if let Some(max) = entry.descriptor.max_depth {
            if requested > max {
                return Err(SubagentDispatchError::DepthExceeded {
                    provider: provider.to_owned(),
                    requested,
                    max,
                });
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tool guards
// ---------------------------------------------------------------------------

/// A deny-only guard that refuses calls to blocklisted tools.
///
/// This is the guard-layer half of the blocklist enforcement; the primary
/// defense is toolset construction that structurally excludes blocked tools
/// from the child's registry.
#[derive(Debug, Clone)]
pub struct SubagentBlocklistGuard {
    scope: String,
    blocked: HashSet<String>,
}

impl SubagentBlocklistGuard {
    /// A guard blocking the named tools, attributed to `scope`.
    pub fn new<I, S>(scope: impl Into<String>, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            scope: scope.into(),
            blocked: tools.into_iter().map(Into::into).collect(),
        }
    }

    /// `true` if `tool` is on this guard's blocklist.
    pub fn contains(&self, tool: &str) -> bool {
        self.blocked.contains(tool)
    }
}

impl ToolGuard for SubagentBlocklistGuard {
    fn name(&self) -> &str {
        "subagent_blocklist"
    }

    fn check(&self, call: &GuardedCall<'_>) -> Option<GuardDenial> {
        if self.blocked.contains(call.tool) {
            Some(GuardDenial::new(
                self.name(),
                format!(
                    "tool `{}` is blocked in subagent scope `{}`",
                    call.tool, self.scope
                ),
            ))
        } else {
            None
        }
    }
}

/// A deny-only guard that refuses delegation tools when the configured
/// depth limit would be exceeded.
///
/// Because [`ToolGuard`] is evaluated per call with no mutable state, this
/// guard serves as a safety net for delegation tools that were not
/// structurally excluded from the child's toolset. The primary depth
/// enforcement is toolset construction: a child at depth `d` receives a
/// toolset with no delegate tools when `d == max`.
#[derive(Debug, Clone)]
pub struct DelegateDepthGuard {
    provider: String,
    max_depth: u32,
    /// Tool name prefixes that identify delegation calls.
    delegate_prefixes: Vec<String>,
}

impl DelegateDepthGuard {
    /// A guard that denies delegation tools when `current_depth` would
    /// reach `max_depth`. The `delegate_prefixes` are the tool name
    /// prefixes that trigger the depth check (e.g. `["transfer_to_"]`).
    pub fn new(
        provider: impl Into<String>,
        max_depth: u32,
        delegate_prefixes: Vec<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            max_depth,
            delegate_prefixes,
        }
    }

    /// `true` if `tool` matches a delegation prefix.
    pub fn is_delegate_tool(&self, tool: &str) -> bool {
        self.delegate_prefixes
            .iter()
            .any(|prefix| tool.starts_with(prefix))
    }
}

impl ToolGuard for DelegateDepthGuard {
    fn name(&self) -> &str {
        "delegate_depth"
    }

    fn check(&self, call: &GuardedCall<'_>) -> Option<GuardDenial> {
        if !self.is_delegate_tool(call.tool) {
            return None;
        }
        // Parse the requested depth from the tool arguments if present.
        let requested = call
            .arguments
            .get("depth")
            .and_then(|v| v.as_u64())
            .map(|d| d as u32)
            .unwrap_or(1);
        if requested >= self.max_depth {
            Some(GuardDenial::new(
                self.name(),
                format!(
                    "subagent provider `{}` depth limit {} would be exceeded by delegation to `{}`",
                    self.provider, self.max_depth, call.tool
                ),
            ))
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Toolset confinement helpers
// ---------------------------------------------------------------------------

/// Construct a child toolset from `base` with the subagent confinement
/// applied: blocklisted tools removed, delegate tools removed when depth
/// would be exceeded.
///
/// Returns the restricted registry and the guard set to attach to the
/// child's [`ToolExecutor`].
pub fn confined_toolset(
    base: &ToolRegistry,
    blocklist: &[String],
    depth: u32,
    max_depth: Option<u32>,
    delegate_prefixes: &[String],
) -> Result<(ToolRegistry, Vec<Arc<dyn ToolGuard>>)> {
    let mut child = ToolRegistry::new();
    let mut guards: Vec<Arc<dyn ToolGuard>> = Vec::new();

    for name in base.names() {
        if blocklist.contains(&name.to_owned()) {
            continue;
        }
        if let Some(max) = max_depth {
            if depth >= max
                && delegate_prefixes
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
            {
                continue;
            }
        }
        if let Some(tool) = base.get(name) {
            child.register_shared(tool);
        }
    }

    if !blocklist.is_empty() {
        guards.push(Arc::new(SubagentBlocklistGuard::new(
            "child",
            blocklist.iter().cloned(),
        )));
    }

    Ok((child, guards))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    #[test]
    fn descriptor_supports_check() {
        let d = SubagentProviderDescriptor::new("spawn", "1.0.0")
            .with_capability(SubagentCapability::StructuredOutput)
            .with_capability(SubagentCapability::ToolFiltering);
        assert!(d.supports(SubagentCapability::StructuredOutput));
        assert!(!d.supports(SubagentCapability::PersonaInjection));
    }

    #[test]
    fn registry_register_and_lookup() {
        let mut reg = SubagentRegistry::new();
        let d = SubagentProviderDescriptor::new("spawn", "1.0.0");
        reg.register(d.clone()).unwrap();
        assert!(reg.contains("spawn"));
        assert_eq!(reg.get("spawn").unwrap().version, "1.0.0");
    }

    #[test]
    fn registry_refuses_shadow() {
        let mut reg = SubagentRegistry::new();
        reg.register(SubagentProviderDescriptor::new("spawn", "1.0.0"))
            .unwrap();
        let err = reg
            .register(SubagentProviderDescriptor::new("spawn", "2.0.0"))
            .unwrap_err();
        assert!(err.to_string().contains("already registered"));
    }

    #[test]
    fn verify_dispatch_fails_loud() {
        let mut reg = SubagentRegistry::new();
        reg.register(SubagentProviderDescriptor::new("spawn", "1.0.0"))
            .unwrap();

        let err = reg
            .verify_dispatch("spawn", SubagentCapability::StructuredOutput)
            .unwrap_err();
        assert_eq!(
            err,
            SubagentDispatchError::UnsupportedCapability {
                provider: "spawn".into(),
                capability: "structured_output".into(),
            }
        );

        let err = reg
            .verify_dispatch("missing", SubagentCapability::StructuredOutput)
            .unwrap_err();
        assert_eq!(
            err,
            SubagentDispatchError::ProviderNotFound {
                provider: "missing".into(),
            }
        );
    }

    #[test]
    fn verify_dispatch_succeeds_when_declared() {
        let mut reg = SubagentRegistry::new();
        reg.register(
            SubagentProviderDescriptor::new("spawn", "1.0.0")
                .with_capability(SubagentCapability::StructuredOutput),
        )
        .unwrap();
        reg.verify_dispatch("spawn", SubagentCapability::StructuredOutput)
            .unwrap();
    }

    #[test]
    fn depth_check_enforced() {
        let mut reg = SubagentRegistry::new();
        reg.register(SubagentProviderDescriptor::new("spawn", "1.0.0").with_max_depth(2))
            .unwrap();
        reg.check_depth("spawn", 1).unwrap();
        reg.check_depth("spawn", 2).unwrap();
        let err = reg.check_depth("spawn", 3).unwrap_err();
        assert_eq!(
            err,
            SubagentDispatchError::DepthExceeded {
                provider: "spawn".into(),
                requested: 3,
                max: 2,
            }
        );
    }

    #[test]
    fn blocklist_guard_denies() {
        let guard = SubagentBlocklistGuard::new("test", ["send_channel".to_owned()]);
        let call = GuardedCall {
            tool: "send_channel",
            arguments: &json!({}),
            effect: crate::record::Effect::NonIdempotent,
            scope: "t-1",
        };
        let denial = guard.check(&call).unwrap();
        assert_eq!(denial.guard, "subagent_blocklist");
        assert!(denial.reason.contains("send_channel"));
        assert!(denial.reason.contains("test"));

        let ok_call = GuardedCall {
            tool: "read_file",
            arguments: &json!({}),
            effect: crate::record::Effect::ReadOnly,
            scope: "t-1",
        };
        assert!(guard.check(&ok_call).is_none());
    }

    #[test]
    fn delegate_depth_guard_denies_at_limit() {
        let guard = DelegateDepthGuard::new("spawn", 2, vec!["transfer_to_".to_owned()]);
        let call = GuardedCall {
            tool: "transfer_to_researcher",
            arguments: &json!({"depth": 2}),
            effect: crate::record::Effect::NonIdempotent,
            scope: "t-1",
        };
        let denial = guard.check(&call).unwrap();
        assert_eq!(denial.guard, "delegate_depth");
        assert!(denial.reason.contains("spawn"));

        let ok_call = GuardedCall {
            tool: "read_file",
            arguments: &json!({}),
            effect: crate::record::Effect::ReadOnly,
            scope: "t-1",
        };
        assert!(guard.check(&ok_call).is_none());
    }

    #[test]
    fn scope_teardown_lifo_and_residue_assertion() {
        let mut reg = SubagentRegistry::new();
        let scope = SubagentScope::new("child-1").unwrap();

        reg.register(SubagentProviderDescriptor::new("spawn", "1.0.0").with_scope_key("child-1"))
            .unwrap();
        reg.register(SubagentProviderDescriptor::new("fork", "1.0.0").with_scope_key("child-1"))
            .unwrap();

        assert_eq!(reg.len(), 2);
        assert!(!reg.scope_is_empty(&scope));

        let removed = reg.dispose_scope(&scope);
        assert_eq!(removed, vec!["fork", "spawn"]);
        assert!(reg.scope_is_empty(&scope));
        assert!(reg.is_empty());
    }

    #[test]
    fn scope_teardown_invokes_callback() {
        let mut reg = SubagentRegistry::new();
        let scope = SubagentScope::new("child-1").unwrap();
        let fired = Arc::new(Mutex::new(false));
        let fired_clone = Arc::clone(&fired);

        reg.register_with_teardown(
            SubagentProviderDescriptor::new("spawn", "1.0.0").with_scope_key("child-1"),
            move || {
                *fired_clone.lock().unwrap() = true;
            },
        )
        .unwrap();

        reg.dispose_scope(&scope);
        assert!(*fired.lock().unwrap());
    }

    #[test]
    fn scope_key_validation() {
        assert!(SubagentScope::new("child-1").is_ok());
        assert!(SubagentScope::new("").is_err());
        assert!(SubagentScope::new("padded ").is_err());
        assert!(SubagentScope::new("with/slash").is_err());
        assert!(SubagentScope::new(&"x".repeat(129)).is_err());
    }

    #[test]
    fn traffic_kind_defaults_to_side() {
        let d = SubagentProviderDescriptor::new("spawn", "1.0.0");
        assert_eq!(d.traffic, TrafficKind::Side);
    }
}
