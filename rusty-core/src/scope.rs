//! RBAC scope grammar: `resource:action` and `resource:<id>:action` with
//! `*` wildcards at any segment.
//!
//! The grammar is the authorization surface for every enforcement point —
//! REST, WS methods, and adapter-mapped actions alike. Evaluation is a pure
//! function with no side effects and no external state.
//!
//! # Grammar
//!
//! - `resource:action` — collection-level access (two segments)
//! - `resource:<id>:action` — instance-level access (three segments)
//! - `*` — wildcard that matches any value at its segment position
//!
//! Examples:
//! - `tasks:read` — read the tasks collection
//! - `tasks:task-123:read` — read a specific task
//! - `skills:*:promote` — promote any skill
//! - `*:*:read` — read anything (super-user)

use std::fmt;
use thiserror::Error;

/// Maximum length of a single scope string (defense against pathological input).
const MAX_SCOPE_LEN: usize = 256;

/// Maximum number of segments in a scope (resource:instance:action = 3).
const MAX_SEGMENTS: usize = 3;

/// A parsed RBAC scope.
///
/// Scopes are immutable once parsed. Clone is cheap (three small strings).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Scope {
    resource: String,
    instance: Option<String>,
    action: String,
}

impl Scope {
    /// Parse a scope from its canonical string representation.
    ///
    /// Accepts exactly 2 or 3 colon-separated segments. Empty segments are
    /// refused — `tasks::read` is an error. Leading or trailing colons are
    /// refused — `:tasks:read` and `tasks:read:` are errors.
    ///
    /// # Examples
    ///
    /// ```
    /// use rusty_agent_runtime::scope::Scope;
    ///
    /// let s = Scope::parse("tasks:read").unwrap();
    /// assert_eq!(s.resource(), "tasks");
    /// assert_eq!(s.instance(), None);
    /// assert_eq!(s.action(), "read");
    ///
    /// let s = Scope::parse("skills:skill-123:promote").unwrap();
    /// assert_eq!(s.instance(), Some("skill-123"));
    /// ```
    pub fn parse(s: &str) -> Result<Self, ScopeParseError> {
        if s.len() > MAX_SCOPE_LEN {
            return Err(ScopeParseError::TooLong);
        }
        if s.is_empty() {
            return Err(ScopeParseError::Empty);
        }

        let segments: Vec<&str> = s.split(':').collect();

        if segments.len() < 2 {
            return Err(ScopeParseError::TooFewSegments);
        }
        if segments.len() > MAX_SEGMENTS {
            return Err(ScopeParseError::TooManySegments);
        }

        // Refuse empty segments (catches `tasks::read`, `:tasks:read`, etc.)
        for seg in &segments {
            if seg.is_empty() {
                return Err(ScopeParseError::EmptySegment);
            }
        }

        let resource = segments[0].to_owned();
        let action = segments[segments.len() - 1].to_owned();
        let instance = if segments.len() == 3 {
            Some(segments[1].to_owned())
        } else {
            None
        };

        Ok(Scope {
            resource,
            instance,
            action,
        })
    }

    /// The resource segment (first).
    pub fn resource(&self) -> &str {
        &self.resource
    }

    /// The instance segment (middle), `None` for collection-level scopes.
    pub fn instance(&self) -> Option<&str> {
        self.instance.as_deref()
    }

    /// The action segment (last).
    pub fn action(&self) -> &str {
        &self.action
    }

    /// `true` if this is a collection-level scope (two segments).
    pub fn is_collection(&self) -> bool {
        self.instance.is_none()
    }

    /// `true` if this is an instance-level scope (three segments).
    pub fn is_instance(&self) -> bool {
        self.instance.is_some()
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(inst) = &self.instance {
            write!(f, "{}:{}:{}", self.resource, inst, self.action)
        } else {
            write!(f, "{}:{}", self.resource, self.action)
        }
    }
}

impl std::str::FromStr for Scope {
    type Err = ScopeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Scope::parse(s)
    }
}

/// Errors that can occur when parsing a scope string.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ScopeParseError {
    #[error("scope string is empty")]
    Empty,
    #[error("scope string exceeds maximum length of {MAX_SCOPE_LEN}")]
    TooLong,
    #[error("scope must have 2 or 3 colon-separated segments")]
    TooFewSegments,
    #[error("scope must have at most {MAX_SEGMENTS} segments")]
    TooManySegments,
    #[error("scope contains an empty segment")]
    EmptySegment,
}

/// Check whether a single granted scope matches a required scope.
///
/// Matching rules:
///
/// 1. Segment count must match exactly. A collection-level grant does not
///    authorize an instance-level requirement, and vice versa.
/// 2. Within the same segment count, each segment matches if it is equal or
///    either side is the wildcard `*`.
///
/// # Examples
///
/// ```
/// use rusty_agent_runtime::scope::{Scope, scope_matches};
///
/// let granted = Scope::parse("skills:*:promote").unwrap();
/// let required = Scope::parse("skills:skill-123:promote").unwrap();
/// assert!(scope_matches(&granted, &required));
///
/// // Collection-level exact match
/// let granted = Scope::parse("tasks:read").unwrap();
/// let required = Scope::parse("tasks:read").unwrap();
/// assert!(scope_matches(&granted, &required));
///
/// // Instance-level does not match collection-level
/// let granted = Scope::parse("tasks:task-123:read").unwrap();
/// let required = Scope::parse("tasks:read").unwrap();
/// assert!(!scope_matches(&granted, &required));
/// ```
pub fn scope_matches(granted: &Scope, required: &Scope) -> bool {
    // Segment count must match exactly.
    if granted.instance.is_none() != required.instance.is_none() {
        return false;
    }

    // Resource segment.
    if granted.resource != "*" && required.resource != "*" && granted.resource != required.resource
    {
        return false;
    }

    // Instance segment (only for 3-segment scopes).
    if let (Some(g_inst), Some(r_inst)) = (&granted.instance, &required.instance) {
        if g_inst != "*" && r_inst != "*" && g_inst != r_inst {
            return false;
        }
    }

    // Action segment.
    if granted.action != "*" && required.action != "*" && granted.action != required.action {
        return false;
    }

    true
}

/// Check whether any scope in `granted` authorizes `required`.
///
/// This is the entry point for enforcement: a token carries a set of granted
/// scopes, and the endpoint declares a required scope.
///
/// # Examples
///
/// ```
/// use rusty_agent_runtime::scope::{Scope, scope_authorizes};
///
/// let granted = vec![
///     Scope::parse("tasks:read").unwrap(),
///     Scope::parse("skills:*:promote").unwrap(),
/// ];
/// assert!(scope_authorizes(&granted, &Scope::parse("skills:skill-123:promote").unwrap()));
/// assert!(!scope_authorizes(&granted, &Scope::parse("agents:read").unwrap()));
/// ```
pub fn scope_authorizes(granted: &[Scope], required: &Scope) -> bool {
    granted.iter().any(|g| scope_matches(g, required))
}

/// Serialize a scope set to its canonical string representation.
///
/// Order is preserved; duplicates are not removed.
pub fn scope_set_to_strings(scopes: &[Scope]) -> Vec<String> {
    scopes.iter().map(|s| s.to_string()).collect()
}

/// Parse a scope set from whitespace-separated strings.
///
/// Common in JWT `scope` claims where spaces separate individual scopes.
/// Returns the first parse error encountered.
pub fn parse_scope_set(input: &str) -> Result<Vec<Scope>, ScopeParseError> {
    if input.trim().is_empty() {
        return Ok(Vec::new());
    }
    input
        .split_whitespace()
        .map(Scope::parse)
        .collect::<Result<Vec<_>, _>>()
}

// ---------------------------------------------------------------------------
// ScopeTable — machine-readable route-to-scope mapping
// ---------------------------------------------------------------------------

/// An HTTP method and path pattern identifying a routable surface.
///
/// Used as the key in [`ScopeTable`] to look up the required [`Scope`] for
/// a given REST or WebSocket method.
///
/// # Examples
///
/// ```
/// use rusty_agent_runtime::scope::RoutePattern;
///
/// let r = RoutePattern::new("GET", "/v1/tasks");
/// assert_eq!(r.method(), "GET");
/// assert_eq!(r.path(), "/v1/tasks");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RoutePattern {
    method: String,
    path: String,
}

impl RoutePattern {
    /// Create a new route pattern.
    ///
    /// `method` is normalized to uppercase (`get` → `GET`).
    pub fn new(method: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            method: method.into().to_ascii_uppercase(),
            path: path.into(),
        }
    }

    /// The HTTP method (uppercase).
    pub fn method(&self) -> &str {
        &self.method
    }

    /// The path pattern.
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// A scope declaration from a channel adapter: the action name (relative to
/// the adapter's mount prefix) and the platform scope it maps onto.
///
/// # Examples
///
/// ```
/// use rusty_agent_runtime::scope::{AdapterScopeDecl, Scope};
///
/// let decl = AdapterScopeDecl::new("events", Scope::parse("webhooks:receive").unwrap());
/// assert_eq!(decl.action(), "events");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterScopeDecl {
    action: String,
    scope: Scope,
}

impl AdapterScopeDecl {
    /// Create a new adapter scope declaration.
    pub fn new(action: impl Into<String>, scope: Scope) -> Self {
        Self {
            action: action.into(),
            scope,
        }
    }

    /// The adapter action name (relative to its mount prefix).
    pub fn action(&self) -> &str {
        &self.action
    }

    /// The required platform scope for this action.
    pub fn scope(&self) -> &Scope {
        &self.scope
    }
}

/// A machine-readable table mapping every mounted REST / WS route to the
/// [`Scope`] required to access it.
///
/// The table supports two kinds of entries:
///
/// 1. **Direct declarations** — routes declared by the platform itself
///    (e.g. `GET /v1/tasks` → `tasks:read`).
/// 2. **Adapter-mounted declarations** — scope mappings declared by a channel
///    adapter, merged into the table at mount time under a prefix
///    (e.g. adapter action `events` mounted at `/v1/slack` produces
///    `POST /v1/slack/events` → `webhooks:receive`).
///
/// The [`ScopeTable::census`] method returns every entry so that a CI test
/// can assert that no mounted route lacks a declared scope.
///
/// # Examples
///
/// ```
/// use rusty_agent_runtime::scope::{Scope, ScopeTable, RoutePattern};
///
/// let mut table = ScopeTable::new();
/// table.declare("GET", "/v1/tasks", Scope::parse("tasks:read").unwrap());
///
/// assert_eq!(table.required_scope("GET", "/v1/tasks"), Some(&Scope::parse("tasks:read").unwrap()));
/// assert_eq!(table.required_scope("POST", "/v1/tasks"), None);
/// ```
#[derive(Debug, Clone, Default)]
pub struct ScopeTable {
    direct: Vec<(RoutePattern, Scope)>,
    mounted: Vec<(RoutePattern, Scope, String)>, // (pattern, scope, adapter_name)
}

impl ScopeTable {
    /// Create an empty scope table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a direct route → scope mapping.
    ///
    /// Routes are stored in insertion order. Lookups prefer the first match.
    pub fn declare(&mut self, method: impl Into<String>, path: impl Into<String>, scope: Scope) {
        self.direct.push((RoutePattern::new(method, path), scope));
    }

    /// Mount a channel adapter's scope declarations under a path prefix.
    ///
    /// Each [`AdapterScopeDecl`] action is joined to `prefix` with a `/`
    /// separator (a trailing slash on `prefix` is collapsed) to produce the
    /// full route pattern. The `adapter_name` is recorded for census and
    /// diagnostic purposes.
    ///
    /// # Examples
    ///
    /// ```
    /// use rusty_agent_runtime::scope::{AdapterScopeDecl, Scope, ScopeTable};
    ///
    /// let mut table = ScopeTable::new();
    /// let decls = vec![
    ///     AdapterScopeDecl::new("events", Scope::parse("webhooks:receive").unwrap()),
    /// ];
    /// table.mount_adapter("/v1/slack", "slack", decls);
    ///
    /// assert_eq!(
    ///     table.required_scope("POST", "/v1/slack/events"),
    ///     Some(&Scope::parse("webhooks:receive").unwrap()),
    /// );
    /// ```
    pub fn mount_adapter(
        &mut self,
        prefix: impl AsRef<str>,
        adapter_name: impl Into<String>,
        declarations: Vec<AdapterScopeDecl>,
    ) {
        let prefix = prefix.as_ref().trim_end_matches('/');
        let adapter_name = adapter_name.into();
        for decl in declarations {
            let path = format!("{}/{}", prefix, decl.action());
            self.mounted.push((
                RoutePattern::new("POST", path),
                decl.scope,
                adapter_name.clone(),
            ));
        }
    }

    /// Look up the required scope for a concrete request.
    ///
    /// Checks direct declarations first (exact match), then mounted adapter
    /// routes (exact match). Returns `None` when no scope is declared.
    pub fn required_scope(&self, method: &str, path: &str) -> Option<&Scope> {
        let method = method.to_ascii_uppercase();

        // Direct declarations take precedence.
        for (pattern, scope) in &self.direct {
            if pattern.method == method && pattern.path == path {
                return Some(scope);
            }
        }

        // Then mounted adapter routes.
        for (pattern, scope, _adapter) in &self.mounted {
            if pattern.method == method && pattern.path == path {
                return Some(scope);
            }
        }

        None
    }

    /// Return every entry in the table for completeness auditing.
    ///
    /// The returned vector contains direct entries followed by mounted entries.
    /// A census test can iterate over this and assert that every mounted route
    /// has a non-empty scope.
    ///
    /// # Examples
    ///
    /// ```
    /// use rusty_agent_runtime::scope::{Scope, ScopeTable};
    ///
    /// let mut table = ScopeTable::new();
    /// table.declare("GET", "/v1/tasks", Scope::parse("tasks:read").unwrap());
    ///
    /// let census = table.census();
    /// assert_eq!(census.len(), 1);
    /// assert_eq!(census[0].1.to_string(), "tasks:read");
    /// ```
    pub fn census(&self) -> Vec<(&RoutePattern, &Scope)> {
        let mut out = Vec::with_capacity(self.direct.len() + self.mounted.len());
        for (pattern, scope) in &self.direct {
            out.push((pattern, scope));
        }
        for (pattern, scope, _adapter) in &self.mounted {
            out.push((pattern, scope));
        }
        out
    }

    /// `true` if the table contains no entries.
    pub fn is_empty(&self) -> bool {
        self.direct.is_empty() && self.mounted.is_empty()
    }

    /// Total number of entries (direct + mounted).
    pub fn len(&self) -> usize {
        self.direct.len() + self.mounted.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_collection_level() {
        let s = Scope::parse("tasks:read").unwrap();
        assert_eq!(s.resource(), "tasks");
        assert_eq!(s.instance(), None);
        assert_eq!(s.action(), "read");
        assert!(s.is_collection());
        assert!(!s.is_instance());
    }

    #[test]
    fn parse_instance_level() {
        let s = Scope::parse("skills:skill-123:promote").unwrap();
        assert_eq!(s.resource(), "skills");
        assert_eq!(s.instance(), Some("skill-123"));
        assert_eq!(s.action(), "promote");
        assert!(!s.is_collection());
        assert!(s.is_instance());
    }

    #[test]
    fn parse_with_wildcards() {
        let s = Scope::parse("*:*:read").unwrap();
        assert_eq!(s.resource(), "*");
        assert_eq!(s.instance(), Some("*"));
        assert_eq!(s.action(), "read");
    }

    #[test]
    fn display_roundtrips() {
        let cases = [
            "tasks:read",
            "skills:skill-123:promote",
            "*:*:read",
            "sessions:*:fork",
        ];
        for case in &cases {
            let s = Scope::parse(case).unwrap();
            assert_eq!(s.to_string(), *case);
        }
    }

    #[test]
    fn from_str_impl() {
        let s: Scope = "tasks:read".parse().unwrap();
        assert_eq!(s.resource(), "tasks");
    }

    #[test]
    fn parse_errors() {
        assert_eq!(Scope::parse(""), Err(ScopeParseError::Empty));
        assert_eq!(Scope::parse("tasks"), Err(ScopeParseError::TooFewSegments));
        assert_eq!(
            Scope::parse("a:b:c:d"),
            Err(ScopeParseError::TooManySegments)
        );
        assert_eq!(
            Scope::parse("tasks::read"),
            Err(ScopeParseError::EmptySegment)
        );
        assert_eq!(
            Scope::parse(":tasks:read"),
            Err(ScopeParseError::EmptySegment)
        );
        assert_eq!(
            Scope::parse("tasks:read:"),
            Err(ScopeParseError::EmptySegment)
        );
    }

    #[test]
    fn too_long_rejected() {
        let long = "a".repeat(MAX_SCOPE_LEN + 1);
        assert_eq!(
            Scope::parse(&format!("{long}:read")),
            Err(ScopeParseError::TooLong)
        );
    }

    // -- scope_matches --

    #[test]
    fn exact_collection_match() {
        let g = Scope::parse("tasks:read").unwrap();
        let r = Scope::parse("tasks:read").unwrap();
        assert!(scope_matches(&g, &r));
    }

    #[test]
    fn exact_instance_match() {
        let g = Scope::parse("skills:skill-123:promote").unwrap();
        let r = Scope::parse("skills:skill-123:promote").unwrap();
        assert!(scope_matches(&g, &r));
    }

    #[test]
    fn wildcard_resource() {
        let g = Scope::parse("*:read").unwrap();
        let r = Scope::parse("tasks:read").unwrap();
        assert!(scope_matches(&g, &r));
    }

    #[test]
    fn wildcard_instance() {
        let g = Scope::parse("skills:*:promote").unwrap();
        let r = Scope::parse("skills:skill-123:promote").unwrap();
        assert!(scope_matches(&g, &r));
    }

    #[test]
    fn wildcard_action() {
        let g = Scope::parse("tasks:*").unwrap();
        let r = Scope::parse("tasks:read").unwrap();
        assert!(scope_matches(&g, &r));
    }

    #[test]
    fn wildcard_all_three() {
        let g = Scope::parse("*:*:*").unwrap();
        let r = Scope::parse("skills:skill-123:promote").unwrap();
        assert!(scope_matches(&g, &r));
    }

    #[test]
    fn segment_count_mismatch_collection_vs_instance() {
        let g = Scope::parse("tasks:read").unwrap();
        let r = Scope::parse("tasks:task-123:read").unwrap();
        assert!(!scope_matches(&g, &r));
    }

    #[test]
    fn segment_count_mismatch_instance_vs_collection() {
        let g = Scope::parse("tasks:task-123:read").unwrap();
        let r = Scope::parse("tasks:read").unwrap();
        assert!(!scope_matches(&g, &r));
    }

    #[test]
    fn different_resource() {
        let g = Scope::parse("tasks:read").unwrap();
        let r = Scope::parse("skills:read").unwrap();
        assert!(!scope_matches(&g, &r));
    }

    #[test]
    fn different_action() {
        let g = Scope::parse("tasks:read").unwrap();
        let r = Scope::parse("tasks:write").unwrap();
        assert!(!scope_matches(&g, &r));
    }

    #[test]
    fn different_instance() {
        let g = Scope::parse("skills:skill-123:promote").unwrap();
        let r = Scope::parse("skills:skill-456:promote").unwrap();
        assert!(!scope_matches(&g, &r));
    }

    #[test]
    fn required_wildcard_matches_any() {
        // If the required scope itself has a wildcard, anything matching the
        // non-wildcard segments satisfies it.
        let g = Scope::parse("skills:skill-123:promote").unwrap();
        let r = Scope::parse("skills:*:promote").unwrap();
        assert!(scope_matches(&g, &r));
    }

    // -- scope_authorizes --

    #[test]
    fn authorizes_when_any_matches() {
        let granted = vec![
            Scope::parse("tasks:read").unwrap(),
            Scope::parse("skills:*:promote").unwrap(),
        ];
        assert!(scope_authorizes(
            &granted,
            &Scope::parse("skills:abc:promote").unwrap()
        ));
    }

    #[test]
    fn denies_when_none_matches() {
        let granted = vec![Scope::parse("tasks:read").unwrap()];
        assert!(!scope_authorizes(
            &granted,
            &Scope::parse("agents:read").unwrap()
        ));
    }

    #[test]
    fn empty_granted_denies_everything() {
        assert!(!scope_authorizes(&[], &Scope::parse("tasks:read").unwrap()));
    }

    // -- parse_scope_set --

    #[test]
    fn parse_scope_set_success() {
        let scopes = parse_scope_set("tasks:read skills:*:promote").unwrap();
        assert_eq!(scopes.len(), 2);
        assert_eq!(scopes[0].to_string(), "tasks:read");
        assert_eq!(scopes[1].to_string(), "skills:*:promote");
    }

    #[test]
    fn parse_scope_set_empty() {
        let scopes = parse_scope_set("").unwrap();
        assert!(scopes.is_empty());
        let scopes = parse_scope_set("   ").unwrap();
        assert!(scopes.is_empty());
    }

    #[test]
    fn parse_scope_set_error() {
        assert_eq!(
            parse_scope_set("tasks:read bad"),
            Err(ScopeParseError::TooFewSegments)
        );
    }

    // -- ScopeTable --

    #[test]
    fn table_declare_and_lookup() {
        let mut table = ScopeTable::new();
        table.declare("GET", "/v1/tasks", Scope::parse("tasks:read").unwrap());
        table.declare("POST", "/v1/tasks", Scope::parse("tasks:create").unwrap());
        table.declare(
            "GET",
            "/v1/tasks/:id",
            Scope::parse("tasks:task-123:read").unwrap(),
        );

        assert_eq!(
            table.required_scope("GET", "/v1/tasks"),
            Some(&Scope::parse("tasks:read").unwrap())
        );
        assert_eq!(
            table.required_scope("POST", "/v1/tasks"),
            Some(&Scope::parse("tasks:create").unwrap())
        );
        assert_eq!(
            table.required_scope("GET", "/v1/tasks/:id"),
            Some(&Scope::parse("tasks:task-123:read").unwrap())
        );
        assert_eq!(table.required_scope("DELETE", "/v1/tasks"), None);
        assert_eq!(table.required_scope("GET", "/v1/agents"), None);
    }

    #[test]
    fn table_method_normalization() {
        let mut table = ScopeTable::new();
        table.declare("get", "/v1/tasks", Scope::parse("tasks:read").unwrap());

        assert_eq!(
            table.required_scope("GET", "/v1/tasks"),
            Some(&Scope::parse("tasks:read").unwrap())
        );
        assert_eq!(
            table.required_scope("get", "/v1/tasks"),
            Some(&Scope::parse("tasks:read").unwrap())
        );
    }

    #[test]
    fn table_mount_adapter() {
        let mut table = ScopeTable::new();
        let decls = vec![
            AdapterScopeDecl::new("events", Scope::parse("webhooks:receive").unwrap()),
            AdapterScopeDecl::new("command", Scope::parse("webhooks:execute").unwrap()),
        ];
        table.mount_adapter("/v1/slack", "slack", decls);

        assert_eq!(
            table.required_scope("POST", "/v1/slack/events"),
            Some(&Scope::parse("webhooks:receive").unwrap())
        );
        assert_eq!(
            table.required_scope("POST", "/v1/slack/command"),
            Some(&Scope::parse("webhooks:execute").unwrap())
        );
        assert_eq!(table.required_scope("GET", "/v1/slack/events"), None);
    }

    #[test]
    fn table_mount_adapter_prefix_without_trailing_slash() {
        let mut table = ScopeTable::new();
        let decls = vec![AdapterScopeDecl::new(
            "hook",
            Scope::parse("webhooks:receive").unwrap(),
        )];
        table.mount_adapter("/v1/github/", "github", decls);

        assert_eq!(
            table.required_scope("POST", "/v1/github/hook"),
            Some(&Scope::parse("webhooks:receive").unwrap())
        );
    }

    #[test]
    fn table_direct_takes_precedence_over_mounted() {
        let mut table = ScopeTable::new();
        table.declare(
            "POST",
            "/v1/slack/events",
            Scope::parse("admin:override").unwrap(),
        );

        let decls = vec![AdapterScopeDecl::new(
            "events",
            Scope::parse("webhooks:receive").unwrap(),
        )];
        table.mount_adapter("/v1/slack", "slack", decls);

        // Direct declaration wins.
        assert_eq!(
            table.required_scope("POST", "/v1/slack/events"),
            Some(&Scope::parse("admin:override").unwrap())
        );
    }

    #[test]
    fn table_census_includes_all_entries() {
        let mut table = ScopeTable::new();
        table.declare("GET", "/v1/tasks", Scope::parse("tasks:read").unwrap());
        table.declare("POST", "/v1/tasks", Scope::parse("tasks:create").unwrap());

        let decls = vec![AdapterScopeDecl::new(
            "events",
            Scope::parse("webhooks:receive").unwrap(),
        )];
        table.mount_adapter("/v1/slack", "slack", decls);

        let census = table.census();
        assert_eq!(census.len(), 3);

        // Every entry has a scope.
        for (_pattern, scope) in &census {
            assert!(!scope.resource().is_empty());
            assert!(!scope.action().is_empty());
        }
    }

    #[test]
    fn table_census_no_mounted_route_lacks_scope() {
        // The census test: every entry returned by census() must have a
        // well-formed scope. An empty table trivially passes.
        let mut table = ScopeTable::new();
        table.declare("GET", "/v1/tasks", Scope::parse("tasks:read").unwrap());
        table.declare("POST", "/v1/tasks", Scope::parse("tasks:create").unwrap());
        table.declare(
            "DELETE",
            "/v1/tasks/:id",
            Scope::parse("tasks:delete").unwrap(),
        );

        let census = table.census();
        assert!(
            !census.is_empty(),
            "census must not be empty for a populated table"
        );

        // Assert: no entry lacks a scope (scope is always Some by construction,
        // but we verify the invariant explicitly).
        for (pattern, scope) in &census {
            assert!(
                !scope.to_string().is_empty(),
                "route {} {} lacks a scope",
                pattern.method(),
                pattern.path()
            );
        }
    }

    #[test]
    fn table_empty() {
        let table = ScopeTable::new();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert!(table.census().is_empty());
        assert_eq!(table.required_scope("GET", "/v1/tasks"), None);
    }

    #[test]
    fn table_adapter_with_zero_scopes_is_inert() {
        // An adapter that declares nothing mounts with zero scopes and can
        // perform no side-effecting action.
        let mut table = ScopeTable::new();
        table.mount_adapter("/v1/slack", "slack", vec![]);

        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert_eq!(table.required_scope("POST", "/v1/slack/anything"), None);
    }
}
