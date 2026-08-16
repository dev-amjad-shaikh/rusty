//! Built-in service packs: curated `http-api` manifests for the services
//! the runtime ships out of the box.
//!
//! A pack is a *declaration*, not code: each constructor returns a
//! validated [`ConnectorManifest`] against the generic HTTP API provider,
//! so packs inherit the provider's execution discipline (transport seam,
//! slot-referencing auth, byte ceilings) rather than restating it. Every
//! operation set is a hand-curated slice of the service's published API —
//! the common, documented calls — never a generated surface.
//!
//! Three judgment calls recur across the packs, all declared rather than
//! hidden:
//!
//! - **POST-shaped reads are over-classified.** Linear and Notion serve
//!   reads over POST (a GraphQL body; a filter/sort DSL body). The effect
//!   taxonomy reserves `read_only` for GET, so these operations — which
//!   write nothing — are classified `compensatable`, the weakest write
//!   rung. The over-classification is deliberate: it costs an approval
//!   gate, never an unaudited write.
//! - **No invented idempotency.** ServiceNow's Table API and Google
//!   Calendar define no idempotency-key header, so their creates are
//!   `compensatable` (undo = delete), never `idempotent` — the plane only
//!   honors the claim when the wire does.
//!
//! Parameter names are declared in the wire's own spelling (camelCase
//! where the API spells it so, e.g. Google's `maxResults`): the plane's
//! parameter charset is `[a-zA-Z][a-zA-Z0-9_]*` precisely so a manifest
//! can say what the endpoint expects to read.

use serde_json::{json, Value};

use super::conn_err;
use super::manifest::{
    ConnectorManifest, CredentialSlot, HttpApiAuth, HttpApiOperation, HttpApiSpec, HttpMethod,
    OperationBody, OperationEffect, ProviderKind, ResponseExtraction,
};
use crate::error::Result;

// ---------------------------------------------------------------------------
// Declaration helpers
// ---------------------------------------------------------------------------

/// A bare operation: empty schema, no routing, no response or timeout
/// overrides. Pack sections start here and layer on what the operation
/// actually takes.
fn op(
    name: &str,
    description: &str,
    method: HttpMethod,
    path: &str,
    effect: OperationEffect,
) -> HttpApiOperation {
    HttpApiOperation {
        name: name.to_owned(),
        description: description.to_owned(),
        method,
        path: path.to_owned(),
        params_schema: json!({"type": "object"}),
        query_params: vec![],
        body: OperationBody::None,
        effect,
        response: ResponseExtraction {
            projection: None,
            max_bytes: None,
        },
        timeout_ms: None,
        idempotency_key_header: None,
    }
}

/// A read: GET and `ReadOnly`, the only pairing the taxonomy allows.
fn get(name: &str, description: &str, path: &str) -> HttpApiOperation {
    op(
        name,
        description,
        HttpMethod::Get,
        path,
        OperationEffect::ReadOnly,
    )
}

/// Declare the operation's params schema: a property for every routed
/// parameter plus the required subset.
fn schema(op: &mut HttpApiOperation, required: &[&str], properties: &[(&str, Value)]) {
    let props = properties
        .iter()
        .map(|(name, value)| (name.to_string(), value.clone()))
        .collect::<serde_json::Map<String, Value>>();
    op.params_schema = json!({"type": "object", "properties": props, "required": required});
}

/// Route parameters to the query string.
fn query(op: &mut HttpApiOperation, params: &[&str]) {
    op.query_params = params.iter().map(|p| p.to_string()).collect();
}

/// Route parameters into a JSON body object.
fn body(op: &mut HttpApiOperation, params: &[&str]) {
    op.body = OperationBody::Json {
        params: params.iter().map(|p| p.to_string()).collect(),
    };
}

/// Give the operation a GraphQL body template (`{{`/`}}` are literal
/// braces; `{param}` interpolates the JSON-encoded argument).
fn graphql(op: &mut HttpApiOperation, query: &str) {
    op.body = OperationBody::Graphql {
        query: query.to_owned(),
    };
}

/// Project the response through a JSON pointer before returning it.
fn project(op: &mut HttpApiOperation, pointer: &str) {
    op.response.projection = Some(pointer.to_owned());
}

/// JSON-schema type shorthands, named for the wire type.
fn string() -> Value {
    json!({"type": "string"})
}

fn integer() -> Value {
    json!({"type": "integer"})
}

fn boolean() -> Value {
    json!({"type": "boolean"})
}

fn object() -> Value {
    json!({"type": "object"})
}

fn strings() -> Value {
    json!({"type": "array", "items": {"type": "string"}})
}

fn objects() -> Value {
    json!({"type": "array", "items": {"type": "object"}})
}

/// Assemble and validate a pack manifest. The version is `"1"` for every
/// pack: the content hash is the identity, so the version string is a
/// human handle, not a semver promise.
#[allow(clippy::too_many_arguments)]
fn pack(
    id: &str,
    display_name: &str,
    description: &str,
    base_url: &str,
    auth: HttpApiAuth,
    default_headers: &[(&str, &str)],
    health_check: Option<&str>,
    operations: Vec<HttpApiOperation>,
    capabilities: &[&str],
    credential_slots: &[(&str, &str)],
) -> Result<ConnectorManifest> {
    ConnectorManifest::new(
        id,
        "1",
        display_name,
        description,
        ProviderKind::HttpApi(HttpApiSpec {
            base_url: base_url.to_owned(),
            auth: Some(auth),
            default_headers: default_headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            health_check: health_check.map(str::to_owned),
            operations,
        }),
        capabilities.iter().map(|entry| entry.to_string()).collect(),
        credential_slots
            .iter()
            .map(|(name, description)| CredentialSlot {
                name: name.to_string(),
                description: description.to_string(),
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// ServiceNow
// ---------------------------------------------------------------------------

/// The task fields the Table API accepts on write — a curated set shared
/// by incident, request, and KB records, not the whole dictionary (which
/// is per-instance configurable).
const SN_WRITE_FIELDS: &[&str] = &[
    "short_description",
    "description",
    "urgency",
    "impact",
    "priority",
    "category",
    "subcategory",
    "assignment_group",
    "caller_id",
    "requested_for",
    "state",
    "comments",
    "work_notes",
    "due_date",
];

/// The ServiceNow Table API pack for one instance
/// (`https://<instance>.service-now.com`).
///
/// `instance` becomes a hostname segment verbatim, so it must be a DNS
/// label — a full domain, scheme, or path is rejected rather than silently
/// composed into a URL.
pub fn servicenow(instance: &str) -> Result<ConnectorManifest> {
    if !is_dns_label(instance) {
        return Err(conn_err(format!(
            "servicenow instance `{instance}` must be a DNS label (`[a-z0-9]([a-z0-9-]*[a-z0-9])?`, at most 63 bytes)"
        )));
    }

    let mut list = get(
        "list-records",
        "List records from a ServiceNow table, with sysparm filtering and pagination.",
        "/api/now/table/{table}",
    );
    schema(
        &mut list,
        &["table"],
        &[
            ("table", string()),
            ("sysparm_query", string()),
            ("sysparm_fields", string()),
            ("sysparm_limit", integer()),
            ("sysparm_offset", integer()),
        ],
    );
    query(
        &mut list,
        &[
            "sysparm_query",
            "sysparm_fields",
            "sysparm_limit",
            "sysparm_offset",
        ],
    );
    project(&mut list, "/result");

    let mut get_record = get(
        "get-record",
        "Get one record from a ServiceNow table by sys_id.",
        "/api/now/table/{table}/{sys_id}",
    );
    schema(
        &mut get_record,
        &["table", "sys_id"],
        &[("table", string()), ("sys_id", string())],
    );
    project(&mut get_record, "/result");

    let write_properties = |extra: &[(&'static str, Value)]| -> Vec<(&'static str, Value)> {
        let mut properties = extra.to_vec();
        properties.extend(SN_WRITE_FIELDS.iter().map(|name| (*name, string())));
        properties
    };

    // Compensatable: the logical undo is deleting the created record.
    // ServiceNow's Table API defines no idempotency-key header, so
    // `idempotent` is not a claim the wire would honor.
    let mut create = op(
        "create-record",
        "Create a record in a ServiceNow table (incident, request, KB, …).",
        HttpMethod::Post,
        "/api/now/table/{table}",
        OperationEffect::Compensatable,
    );
    schema(
        &mut create,
        &["table"],
        &write_properties(&[("table", string())]),
    );
    body(&mut create, SN_WRITE_FIELDS);
    project(&mut create, "/result");

    // Compensatable: the undo is writing back the prior field values.
    let mut update = op(
        "update-record",
        "Update fields on one record in a ServiceNow table.",
        HttpMethod::Patch,
        "/api/now/table/{table}/{sys_id}",
        OperationEffect::Compensatable,
    );
    schema(
        &mut update,
        &["table", "sys_id"],
        &write_properties(&[("table", string()), ("sys_id", string())]),
    );
    body(&mut update, SN_WRITE_FIELDS);
    project(&mut update, "/result");

    let mut delete = op(
        "delete-record",
        "Delete one record from a ServiceNow table.",
        HttpMethod::Delete,
        "/api/now/table/{table}/{sys_id}",
        OperationEffect::Irreversible,
    );
    schema(
        &mut delete,
        &["table", "sys_id"],
        &[("table", string()), ("sys_id", string())],
    );

    // No health check: every read in this set takes a table argument, so
    // none is the parameterless GET connect would need.
    pack(
        "servicenow",
        "ServiceNow",
        &format!(
            "ServiceNow Table API for the `{instance}` instance: list, get, create, update, and delete records in any table."
        ),
        &format!("https://{instance}.service-now.com"),
        HttpApiAuth::Basic {
            username_slot: "username".to_owned(),
            password_slot: "password".to_owned(),
        },
        &[],
        None,
        vec![list, get_record, create, update, delete],
        &["servicenow table api"],
        &[
            ("username", "ServiceNow user name for basic authentication."),
            ("password", "ServiceNow password for basic authentication."),
        ],
    )
}

/// `[a-z0-9]([a-z0-9-]*[a-z0-9])?`, at most 63 bytes: the RFC 1035 label
/// shape a ServiceNow instance name must have to compose into a hostname.
fn is_dns_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !label.starts_with('-')
        && !label.ends_with('-')
}

// ---------------------------------------------------------------------------
// Gmail
// ---------------------------------------------------------------------------

/// The Gmail API pack: profile, message listing and reading, label
/// modification, and send.
pub fn gmail() -> Result<ConnectorManifest> {
    let get_profile = get(
        "get-profile",
        "Get the authenticated user's Gmail profile.",
        "/gmail/v1/users/me/profile",
    );

    // `labelIds` is deliberately absent: the real API takes it as a
    // *repeated* query parameter, which this plane cannot express — an
    // array-typed declaration would error before the wire every time.
    // Label filtering lands when the plane grows repeated/array query
    // params.
    let mut list = get(
        "list-messages",
        "List messages matching a Gmail search query.",
        "/gmail/v1/users/me/messages",
    );
    schema(
        &mut list,
        &[],
        &[
            ("q", string()),
            ("maxResults", integer()),
            ("pageToken", string()),
        ],
    );
    query(&mut list, &["q", "maxResults", "pageToken"]);

    let mut get_message = get(
        "get-message",
        "Get one message by id.",
        "/gmail/v1/users/me/messages/{message_id}",
    );
    schema(
        &mut get_message,
        &["message_id"],
        &[("message_id", string()), ("format", string())],
    );
    query(&mut get_message, &["format"]);

    // Compensatable: labels can be moved back with a second modify call.
    let mut modify = op(
        "modify-message",
        "Add and remove labels on a message.",
        HttpMethod::Post,
        "/gmail/v1/users/me/messages/{message_id}/modify",
        OperationEffect::Compensatable,
    );
    schema(
        &mut modify,
        &["message_id"],
        &[
            ("message_id", string()),
            ("addLabelIds", strings()),
            ("removeLabelIds", strings()),
        ],
    );
    body(&mut modify, &["addLabelIds", "removeLabelIds"]);

    // Irreversible: a sent email has no undo on the wire — no recall, only
    // a follow-up.
    let mut send = op(
        "send-message",
        "Send a base64url-encoded RFC 2822 message.",
        HttpMethod::Post,
        "/gmail/v1/users/me/messages/send",
        OperationEffect::Irreversible,
    );
    schema(&mut send, &["raw"], &[("raw", string())]);
    body(&mut send, &["raw"]);

    pack(
        "gmail",
        "Gmail",
        "Gmail API: read the profile, list and get messages, modify labels, and send mail.",
        "https://gmail.googleapis.com",
        HttpApiAuth::BearerToken {
            credential_slot: "access_token".to_owned(),
        },
        &[],
        Some("get-profile"),
        vec![get_profile, list, get_message, modify, send],
        &["email read/send"],
        &[(
            "access_token",
            "OAuth access token with the gmail.modify scope.",
        )],
    )
}

// ---------------------------------------------------------------------------
// Slack
// ---------------------------------------------------------------------------

/// The Slack Web API pack: list channels, read history, list users, post
/// messages, and add reactions.
///
/// Slack documents GET with query parameters for its read methods
/// (conversations.list, conversations.history, users.list), so the reads
/// here are honest GETs; the writes are POSTs with JSON bodies.
pub fn slack() -> Result<ConnectorManifest> {
    let mut list_channels = get(
        "list-channels",
        "List conversations in the workspace.",
        "/api/conversations.list",
    );
    schema(
        &mut list_channels,
        &[],
        &[
            ("limit", integer()),
            ("cursor", string()),
            ("types", string()),
        ],
    );
    query(&mut list_channels, &["limit", "cursor", "types"]);

    let mut history = get(
        "channel-history",
        "Fetch a conversation's message history.",
        "/api/conversations.history",
    );
    schema(
        &mut history,
        &["channel"],
        &[
            ("channel", string()),
            ("limit", integer()),
            ("cursor", string()),
            ("oldest", string()),
            ("latest", string()),
        ],
    );
    query(
        &mut history,
        &["channel", "limit", "cursor", "oldest", "latest"],
    );

    let mut list_users = get(
        "list-users",
        "List users in the workspace.",
        "/api/users.list",
    );
    schema(
        &mut list_users,
        &[],
        &[("limit", integer()), ("cursor", string())],
    );
    query(&mut list_users, &["limit", "cursor"]);

    // Compensatable: the undo is chat.delete of the posted message.
    let mut post = op(
        "post-message",
        "Post a message to a channel, optionally in a thread.",
        HttpMethod::Post,
        "/api/chat.postMessage",
        OperationEffect::Compensatable,
    );
    schema(
        &mut post,
        &["channel", "text"],
        &[
            ("channel", string()),
            ("text", string()),
            ("thread_ts", string()),
        ],
    );
    body(&mut post, &["channel", "text", "thread_ts"]);

    // Compensatable: the undo is reactions.remove of the same triple.
    let mut react = op(
        "add-reaction",
        "Add an emoji reaction to a message.",
        HttpMethod::Post,
        "/api/reactions.add",
        OperationEffect::Compensatable,
    );
    schema(
        &mut react,
        &["channel", "timestamp", "name"],
        &[
            ("channel", string()),
            ("timestamp", string()),
            ("name", string()),
        ],
    );
    body(&mut react, &["channel", "timestamp", "name"]);

    pack(
        "slack",
        "Slack",
        "Slack Web API: list channels and users, read channel history, post messages, and add reactions.",
        "https://slack.com",
        HttpApiAuth::BearerToken {
            credential_slot: "bot_token".to_owned(),
        },
        &[],
        Some("list-channels"),
        vec![list_channels, history, list_users, post, react],
        &["slack messaging"],
        &[(
            "bot_token",
            "Slack bot token (xoxb-…) with the scopes the operations need.",
        )],
    )
}

// ---------------------------------------------------------------------------
// Linear
// ---------------------------------------------------------------------------

/// The Linear GraphQL API pack: list teams and issues, get, create, and
/// update issues.
///
/// Linear serves everything — reads included — as POST `/graphql`; its
/// published API defines no GET query form. The read operations below
/// write nothing, but the effect kernel reserves `read_only` for GET, so
/// every operation here is conservatively classified `compensatable`.
pub fn linear() -> Result<ConnectorManifest> {
    let mut teams = op(
        "list-teams",
        "List teams in the workspace.",
        HttpMethod::Post,
        "/graphql",
        OperationEffect::Compensatable,
    );
    graphql(
        &mut teams,
        "query {{ teams {{ nodes {{ id name key }} }} }}",
    );
    project(&mut teams, "/data");

    // `first` is required even though Linear defaults it: the template
    // interpolates every declared parameter, so an absent argument fails
    // honestly rather than rendering a hole in the query.
    let mut issues = op(
        "list-issues",
        "List issues, one page of `first` at a time.",
        HttpMethod::Post,
        "/graphql",
        OperationEffect::Compensatable,
    );
    schema(&mut issues, &["first"], &[("first", integer())]);
    graphql(
        &mut issues,
        "query {{ issues(first: {first}) {{ nodes {{ id identifier title }} }} }}",
    );
    project(&mut issues, "/data");

    let mut issue = op(
        "get-issue",
        "Get one issue by id.",
        HttpMethod::Post,
        "/graphql",
        OperationEffect::Compensatable,
    );
    schema(&mut issue, &["id"], &[("id", string())]);
    graphql(
        &mut issue,
        "query {{ issue(id: {id}) {{ id identifier title description }} }}",
    );
    project(&mut issue, "/data");

    // Mutations are compensatable: create's undo is archiving the issue,
    // update's undo is writing back the prior field values. All declared
    // parameters are required — the interpolation rule above applies to
    // `description` even though Linear treats it as optional; pass an
    // empty string to skip it.
    let mut create = op(
        "create-issue",
        "Create an issue in a team.",
        HttpMethod::Post,
        "/graphql",
        OperationEffect::Compensatable,
    );
    schema(
        &mut create,
        &["title", "description", "team_id"],
        &[
            ("title", string()),
            ("description", string()),
            ("team_id", string()),
        ],
    );
    graphql(
        &mut create,
        "mutation {{ issueCreate(input: {{ title: {title}, description: {description}, teamId: {team_id} }}) {{ success issue {{ id identifier }} }} }}",
    );
    project(&mut create, "/data");

    let mut update = op(
        "update-issue",
        "Update an issue's title, description, or state.",
        HttpMethod::Post,
        "/graphql",
        OperationEffect::Compensatable,
    );
    schema(
        &mut update,
        &["id", "title", "description", "state_id"],
        &[
            ("id", string()),
            ("title", string()),
            ("description", string()),
            ("state_id", string()),
        ],
    );
    graphql(
        &mut update,
        "mutation {{ issueUpdate(id: {id}, input: {{ title: {title}, description: {description}, stateId: {state_id} }}) {{ success }} }}",
    );
    project(&mut update, "/data");

    // No health check: the endpoint is POST-only, and a health check must
    // be a parameterless read-only GET.
    pack(
        "linear",
        "Linear",
        "Linear GraphQL API: list teams and issues, get, create, and update issues.",
        "https://api.linear.app",
        // Personal API keys are documented as a bare
        // `Authorization: <key>` header; the Bearer style declared here is
        // the OAuth form. A key-only deployment should expect auth
        // failures and use OAuth instead.
        HttpApiAuth::BearerToken {
            credential_slot: "api_key".to_owned(),
        },
        &[],
        None,
        vec![teams, issues, issue, create, update],
        &["linear issue tracking"],
        &[(
            "api_key",
            "Linear OAuth access token, sent as a bearer token.",
        )],
    )
}

// ---------------------------------------------------------------------------
// Notion
// ---------------------------------------------------------------------------

/// The Notion API pack: search, page and database reads, block children,
/// and page create/update.
pub fn notion() -> Result<ConnectorManifest> {
    // `search` and `query-database` read the world but are POST-shaped —
    // the filter/sort DSL travels in a JSON body — so the GET-only
    // `read_only` rung cannot apply. Both are conservatively classified
    // `compensatable` (see the module docs).
    let mut search = op(
        "search",
        "Search pages and databases by title.",
        HttpMethod::Post,
        "/v1/search",
        OperationEffect::Compensatable,
    );
    schema(
        &mut search,
        &[],
        &[
            ("query", string()),
            ("filter", object()),
            ("sort", object()),
            ("page_size", integer()),
            ("start_cursor", string()),
        ],
    );
    body(
        &mut search,
        &["query", "filter", "sort", "page_size", "start_cursor"],
    );

    let mut get_page = get(
        "get-page",
        "Get a page's properties.",
        "/v1/pages/{page_id}",
    );
    schema(&mut get_page, &["page_id"], &[("page_id", string())]);

    let mut get_database = get(
        "get-database",
        "Get a database's schema.",
        "/v1/databases/{database_id}",
    );
    schema(
        &mut get_database,
        &["database_id"],
        &[("database_id", string())],
    );

    let mut query_database = op(
        "query-database",
        "Query a database with filters and sorts.",
        HttpMethod::Post,
        "/v1/databases/{database_id}/query",
        OperationEffect::Compensatable,
    );
    schema(
        &mut query_database,
        &["database_id"],
        &[
            ("database_id", string()),
            ("filter", object()),
            ("sorts", objects()),
            ("page_size", integer()),
            ("start_cursor", string()),
        ],
    );
    body(
        &mut query_database,
        &["filter", "sorts", "page_size", "start_cursor"],
    );

    let mut children = get(
        "list-block-children",
        "List a block's children.",
        "/v1/blocks/{block_id}/children",
    );
    schema(
        &mut children,
        &["block_id"],
        &[
            ("block_id", string()),
            ("page_size", integer()),
            ("start_cursor", string()),
        ],
    );
    query(&mut children, &["page_size", "start_cursor"]);

    // Compensatable: the undo is archiving the created page.
    let mut create = op(
        "create-page",
        "Create a page under a parent page or database.",
        HttpMethod::Post,
        "/v1/pages",
        OperationEffect::Compensatable,
    );
    schema(
        &mut create,
        &["parent", "properties"],
        &[
            ("parent", object()),
            ("properties", object()),
            ("children", objects()),
        ],
    );
    body(&mut create, &["parent", "properties", "children"]);

    // Compensatable: the undo is writing back the prior properties (or
    // un-archiving).
    let mut update = op(
        "update-page",
        "Update a page's properties, or archive it.",
        HttpMethod::Patch,
        "/v1/pages/{page_id}",
        OperationEffect::Compensatable,
    );
    schema(
        &mut update,
        &["page_id"],
        &[
            ("page_id", string()),
            ("properties", object()),
            ("archived", boolean()),
        ],
    );
    body(&mut update, &["properties", "archived"]);

    // No health check: the one parameterless operation (`search`) is
    // POST-shaped, and a health check must be a read-only GET.
    pack(
        "notion",
        "Notion",
        "Notion API: search the workspace, read pages, databases, and block children, and create or update pages.",
        "https://api.notion.com",
        HttpApiAuth::BearerToken {
            credential_slot: "integration_token".to_owned(),
        },
        // Required on every call. The documented spelling is kept: header
        // names are case-insensitive on the wire and validation accepts
        // any HTTP token.
        &[("Notion-Version", "2022-06-28")],
        None,
        vec![search, get_page, get_database, query_database, children, create, update],
        &["notion workspace"],
        &[(
            "integration_token",
            "Notion internal integration token for a workspace the integration is shared with.",
        )],
    )
}

// ---------------------------------------------------------------------------
// Google Calendar
// ---------------------------------------------------------------------------

/// The Google Calendar API pack: list calendars, list/get/create/update/
/// delete events.
pub fn google_calendar() -> Result<ConnectorManifest> {
    let list_calendars = get(
        "list-calendars",
        "List the authenticated user's calendars.",
        "/calendar/v3/users/me/calendarList",
    );

    let mut list = get(
        "list-events",
        "List events on a calendar, with time-window and search filtering.",
        "/calendar/v3/calendars/{calendar_id}/events",
    );
    schema(
        &mut list,
        &["calendar_id"],
        &[
            ("calendar_id", string()),
            ("timeMin", string()),
            ("timeMax", string()),
            ("q", string()),
            ("maxResults", integer()),
            ("singleEvents", boolean()),
            ("orderBy", string()),
            ("pageToken", string()),
        ],
    );
    query(
        &mut list,
        &[
            "timeMin",
            "timeMax",
            "q",
            "maxResults",
            "singleEvents",
            "orderBy",
            "pageToken",
        ],
    );

    let mut get_event = get(
        "get-event",
        "Get one event by id.",
        "/calendar/v3/calendars/{calendar_id}/events/{event_id}",
    );
    schema(
        &mut get_event,
        &["calendar_id", "event_id"],
        &[("calendar_id", string()), ("event_id", string())],
    );

    let event_properties = |extra: &[(&'static str, Value)]| -> Vec<(&'static str, Value)> {
        let mut properties = extra.to_vec();
        properties.extend([
            ("summary", string()),
            ("description", string()),
            ("location", string()),
            // `{"dateTime": <RFC 3339>, "timeZone": <IANA>}` objects.
            ("start", object()),
            ("end", object()),
            ("attendees", objects()),
        ]);
        properties
    };

    // Compensatable: the undo is deleting the event. Google Calendar
    // defines no idempotency-key header, so `idempotent` is unclaimable.
    let mut create = op(
        "create-event",
        "Create an event on a calendar.",
        HttpMethod::Post,
        "/calendar/v3/calendars/{calendar_id}/events",
        OperationEffect::Compensatable,
    );
    schema(
        &mut create,
        &["calendar_id", "start", "end"],
        &event_properties(&[("calendar_id", string())]),
    );
    body(
        &mut create,
        &[
            "summary",
            "description",
            "location",
            "start",
            "end",
            "attendees",
        ],
    );

    // Compensatable: the undo is writing back the prior field values.
    let mut update = op(
        "update-event",
        "Patch fields on one event.",
        HttpMethod::Patch,
        "/calendar/v3/calendars/{calendar_id}/events/{event_id}",
        OperationEffect::Compensatable,
    );
    schema(
        &mut update,
        &["calendar_id", "event_id"],
        &event_properties(&[("calendar_id", string()), ("event_id", string())]),
    );
    body(
        &mut update,
        &[
            "summary",
            "description",
            "location",
            "start",
            "end",
            "attendees",
        ],
    );

    let mut delete = op(
        "delete-event",
        "Delete one event from a calendar.",
        HttpMethod::Delete,
        "/calendar/v3/calendars/{calendar_id}/events/{event_id}",
        OperationEffect::Irreversible,
    );
    schema(
        &mut delete,
        &["calendar_id", "event_id"],
        &[("calendar_id", string()), ("event_id", string())],
    );

    pack(
        "google-calendar",
        "Google Calendar",
        "Google Calendar API: list calendars, and list, get, create, update, and delete events.",
        "https://www.googleapis.com",
        HttpApiAuth::BearerToken {
            credential_slot: "access_token".to_owned(),
        },
        &[],
        Some("list-calendars"),
        vec![list_calendars, list, get_event, create, update, delete],
        &["calendar read/write"],
        &[
            (
                "access_token",
                "OAuth access token with calendar read and event scopes (the health check lists calendars, so a read scope such as calendar.readonly is needed alongside calendar.events).",
            ),
        ],
    )
}
