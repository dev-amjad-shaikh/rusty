//! The capability host (R0.9 Rusty Capsules, wave 1): governed execution
//! of untrusted WASM Component Model guests behind the declared manifest.
//!
//! Where [`crate::wasm_node`] runs pure-compute core-WASM guests with no
//! imports at all, the capability host runs *components* whose imports
//! exist only when granted. ABI v0 stays untouched beside this — trusted
//! pure compute keeps the fast path; capsules are the untrusted-with-
//! capabilities case. There is no migration.
//!
//! # Structural denial, and what it does and does not prove
//!
//! A capsule instantiates against its manifest's declared WIT world
//! ([`crate::capsule::WORLD_V1`]), and the host links exactly the imports
//! the granted capabilities name. Before instantiation the host walks the
//! component's import list: an import whose capability class carries no
//! grant is never linked, and the invocation is refused with a journaled
//! [`CapsuleDenial`]. This is the object-capability property enforced by
//! the linker: a component built without the `net` import cannot reach
//! the network even in-process — there is no symbol to call, no handle to
//! forge. What it proves: a guest *without* the import cannot attempt the
//! capability at all. What it does not prove: anything about a guest
//! *with* the import — a linked import is a door, and the grant says how
//! far it opens.
//!
//! That second half is scoped denial. A `network` grant naming one
//! hostname links the `fetch` import, but the host's import implementation
//! matches host + protocol + method against the grant set before anything
//! executes, and a mismatch is refused in-band and journaled as a
//! [`CapsuleDenial`] naming the absent grant (granted host A, attempted
//! host B — the absent grant is `network` scoped to host B). A scoped
//! denial is a runtime check, honestly: it can be evaluated only when the
//! attempt arrives. Both denials journal through the same event kind,
//! distinguished by their payloads — structural denials carry an
//! empty-scope absent grant (no grant at any scope existed), scoped
//! denials name the scope that was missing.
//!
//! # Resource governance
//!
//! Three wasmtime mechanisms, one per budget axis: **fuel** is the CPU
//! budget (deterministic, replay-stable — the `wasm_node` inheritance);
//! **epoch interruption** is the wall-time budget's enforcement arm — a
//! per-host ticker advances the engine epoch every `EPOCH_TICK_MS`
//! milliseconds, and each store's deadline is set in ticks, so a guest
//! that stops yielding is preempted (the fuel-only model cannot express
//! this, which is why capsules need their own host); the
//! [`ResourceLimiter`] carries the memory cap. Budget breaches abort the
//! invocation and are journaled on the invocation event with the budget
//! that bit, when it is confidently attributable (fuel reads the store's
//! remaining fuel; wall time reads the clock; memory and other traps are
//! reported unattributed rather than guessed).
//!
//! # Output gate
//!
//! The guest's result crosses the trust boundary as a string. Before the
//! host accepts it: `max_output_bytes` is enforced, and the payload must
//! parse as JSON. Full draft-2020-12 validation against the manifest's
//! declared `output_schema` is pinned in the contract and lands with the
//! wave that adopts a validator — the same staging
//! [`crate::durable::ArtifactContract::schema`] documented in R0.7. The
//! canonical ABI itself has already typed the export (`result<string,
//! string>`): an untrusted component's output is a claim to validate,
//! never a structure to trust.
//!
//! # Journaling
//!
//! Given a [`Journal`], every invocation records: the invocation itself
//! (a [`RunEventKind::WasmCall`] event — a capsule invocation *is* a WASM
//! guest call; the payload names the capsule id), every granted
//! capability use ([`RunEventKind::CapsuleCall`], the capsule rule's
//! "every use is journaled" half), and every denial
//! ([`RunEventKind::CapsuleDenied`]). Capability events chain causally
//! from the run event the caller passes as `parent`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, ResourceLimiter, Store};

use crate::capsule::{
    any_grant_of_kind, is_read_only_method, network_grant_covers, CapabilityGrant, CapabilityKind,
    CapsuleDenial, CapsuleId, CapsuleManifest, CapsuleUse, ResourceBudget,
};
use crate::error::{Result, RustyError};
use crate::journal::{EventDraft, Journal};
use crate::record::{sha256_hex, Effect, EventStatus, RunEventKind};

/// The wave-1 world's network import instance. Hand-written components
/// import this exact name; the linker provides it when — and only when —
/// the manifest grants `network`.
const NET_INSTANCE: &str = "rusty:capsule/net@0.1.0";

/// The wave-1 world's clock import instance, linked when the manifest
/// grants `clock`.
const CLOCK_INSTANCE: &str = "rusty:capsule/clock@0.1.0";

/// The epoch ticker's cadence. Wall-time deadlines are set in ticks, so
/// enforcement granularity is one tick — coarse on purpose (a budget is a
/// bound, not a schedule), cheap enough to run per capsule host.
const EPOCH_TICK_MS: u64 = 5;

/// Map a component import name onto the capability class it belongs to.
/// The mapping is total over the wave-1 world: an import naming nothing
/// here is outside every supported world and fails closed.
fn capability_for_import(name: &str) -> Option<CapabilityKind> {
    match name {
        NET_INSTANCE => Some(CapabilityKind::Network),
        CLOCK_INSTANCE => Some(CapabilityKind::Clock),
        _ => None,
    }
}

/// One outbound network call, as the host's `fetch` import receives it.
/// The connector never sees the guest — it sees this validated request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRequest {
    /// The protocol (`https`, `http`, …) — matched against the grant.
    pub protocol: String,
    /// The hostname — matched against the grant.
    pub host: String,
    /// The HTTP method — matched against the grant.
    pub method: String,
    /// The request path (not grant-matched; the grant scopes the origin).
    pub path: String,
    /// The request body, when the guest sent one.
    pub body: Option<String>,
}

/// The connector's answer to a [`FetchRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchResponse {
    /// The HTTP status code.
    pub status: u16,
    /// The response body.
    pub body: String,
}

/// The egress seam: where a deployment plugs its outbound path for
/// granted capsule network calls.
///
/// Core owns the trait, applications own the implementation — the
/// `CandidateEvaluator` boundary, drawn for the same reason: the runtime
/// owns the journaled *shape* of the call (scope match, use record,
/// denial) and the deployment owns performing it (its proxy, its auth,
/// its TLS posture). No default ships in wave 1: a manifest granting
/// `network` against a host built without a connector is a configuration
/// error, not an implicit egress path.
pub trait NetworkConnector: std::fmt::Debug + Send + Sync {
    /// Perform one already-grant-matched call. Errors are the connector's
    /// own (DNS, TLS, status handling) — they are journaled as failed
    /// uses, never retried by the host.
    fn fetch(&self, request: &FetchRequest) -> std::result::Result<FetchResponse, String>;
}

/// The memory-growth cap, store-local (the `wasm_node` pattern).
struct StoreLimits {
    max_memory_bytes: Option<usize>,
}

impl ResourceLimiter for StoreLimits {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> std::result::Result<bool, wasmtime::Error> {
        Ok(match self.max_memory_bytes {
            Some(cap) => desired <= cap,
            None => true,
        })
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> std::result::Result<bool, wasmtime::Error> {
        // Guests get tables for function pointers; cap generously but finite.
        Ok(desired <= 100_000)
    }
}

/// What one invocation needs in the store: the effective grants, the
/// connectors, and the journaling cursor. Every invocation builds a fresh
/// store — guests keep no state across calls (the engine's idempotency
/// contract, inherited from `wasm_node`).
struct StoreData {
    limiter: StoreLimits,
    grants: Vec<CapabilityGrant>,
    capsule_id: CapsuleId,
    connector: Option<Arc<dyn NetworkConnector>>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    journal: Option<Journal>,
    parent: Option<String>,
    uses: Vec<CapsuleUse>,
    denials: Vec<CapsuleDenial>,
}

impl StoreData {
    /// Record one capability event, chaining causally from the previous
    /// one (the first chains from the run event the caller named). No
    /// journal means the payloads are still collected on the store data —
    /// evidence survives a journal-less invocation in the outcome.
    fn journal_event(
        &mut self,
        kind: RunEventKind,
        effect: Effect,
        output: Value,
        status: EventStatus,
    ) {
        if let Some(journal) = &self.journal {
            let mut draft = EventDraft::new(kind, effect).output(output).status(status);
            if let Some(parent) = &self.parent {
                draft = draft.parent(parent.clone());
            }
            self.parent = Some(journal.record(draft));
        }
    }
}

/// The inputs to one capsule invocation. Built via
/// [`CapsuleInvocation::new`] plus the builder methods; the budget
/// declared here is the *enclosing run's* bound, clamped field-wise
/// against the manifest's own declaration at invocation
/// ([`ResourceBudget::clamp`]) — the effective budget is never wider than
/// either layer.
#[derive(Debug, Clone)]
pub struct CapsuleInvocation {
    /// The guest's input (serialized to a JSON string at the boundary).
    pub input: Value,

    /// The run's journal, when the invocation should leave evidence.
    pub journal: Option<Journal>,

    /// The causal parent: the run event that caused this invocation.
    pub parent: Option<String>,

    /// The enclosing run's budget bounds. `None` fields mean the run
    /// imposes no bound on that axis — the manifest's declaration (if
    /// any) then governs, and where both are silent the axis is unbounded
    /// (never an invented default).
    pub budget: ResourceBudget,
}

impl CapsuleInvocation {
    /// An invocation with input `input` and no journal, parent, or bounds.
    pub fn new(input: Value) -> Self {
        Self {
            input,
            journal: None,
            parent: None,
            budget: ResourceBudget::default(),
        }
    }

    /// Journal into `journal`, causally under the run event `parent`.
    pub fn with_journal(mut self, journal: Journal, parent: Option<String>) -> Self {
        self.journal = Some(journal);
        self.parent = parent;
        self
    }

    /// The enclosing run's budget bounds.
    pub fn with_budget(mut self, budget: ResourceBudget) -> Self {
        self.budget = budget;
        self
    }
}

/// What a completed invocation produced: the validated output plus the
/// evidence the host collected.
#[derive(Debug)]
pub struct CapsuleOutcome {
    /// The guest's result, parsed as JSON and past the output gate.
    pub output: Value,

    /// Fuel consumed by the guest (the CPU budget's accounting).
    pub fuel_consumed: u64,

    /// Every granted capability use, in call order. Journaled as
    /// `CapsuleCall` events when a journal was attached; carried here
    /// either way.
    pub uses: Vec<CapsuleUse>,

    /// Every scoped denial during the invocation. Structural denials
    /// cannot appear here — they refuse the invocation before it starts.
    pub denials: Vec<CapsuleDenial>,
}

/// The shared interior of a [`CapsuleHost`]: the compiled component plus
/// the manifest it was admitted under.
struct HostInner {
    engine: Engine,
    component: Component,
    manifest: CapsuleManifest,
    capsule_id: CapsuleId,
    connector: Option<Arc<dyn NetworkConnector>>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// Set once the epoch ticker starts; the ticker holds this flag and
    /// exits when it clears (host dropped), so hosts do not leak threads.
    ticker_alive: Arc<AtomicBool>,
    ticker_started: AtomicBool,
}

impl Drop for HostInner {
    fn drop(&mut self) {
        self.ticker_alive.store(false, Ordering::Relaxed);
    }
}

/// A WASM Component Model host enforcing one capsule manifest.
///
/// Cheap to clone (the engine and compiled component are `Arc`-backed);
/// the component is compiled once at admission and instantiation is per
/// invocation.
#[derive(Clone)]
pub struct CapsuleHost {
    inner: Arc<HostInner>,
}

impl std::fmt::Debug for CapsuleHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapsuleHost")
            .field("capsule", &self.inner.manifest.identity.name)
            .field("capsule_id", &self.inner.capsule_id)
            .finish()
    }
}

impl CapsuleHost {
    /// Admit a capsule: validate the manifest, verify the build digest
    /// against the artifact bytes, compile the component.
    ///
    /// Admission is where integrity is decided. A manifest naming bytes it
    /// was not built from does not load (the digest is recomputed, not
    /// trusted); a manifest failing [`CapsuleManifest::validate`] does not
    /// compile. What passes here can be instantiated without further
    /// interpretation.
    pub fn from_bytes(manifest: CapsuleManifest, bytes: impl AsRef<[u8]>) -> Result<Self> {
        let bytes = bytes.as_ref();
        let name = manifest.identity.name.clone();
        let host_err = |msg: String| RustyError::Node(format!("capsule host '{name}': {msg}"));

        manifest.validate()?;
        let actual = sha256_hex(bytes);
        if actual != manifest.build_digest {
            return Err(host_err(format!(
                "build digest mismatch: the manifest names {} but the artifact hashes to \
                 {actual} — a manifest is admitted against the bytes it was built from",
                manifest.build_digest
            )));
        }

        let mut config = Config::new();
        config.consume_fuel(true);
        config.epoch_interruption(true);
        let engine =
            Engine::new(&config).map_err(|e| host_err(format!("failed to create engine: {e}")))?;
        let component = Component::new(&engine, bytes)
            .map_err(|e| host_err(format!("failed to compile component: {e:#}")))?;

        // The wave-1 world exports exactly `run`; refuse components that
        // do not provide it, at admission rather than at invocation (the
        // `wasm_node` ABI check's discipline).
        let exports_run = matches!(
            component
                .component_type()
                .get_export(&engine, "run")
                .map(|export| export.ty),
            Some(wasmtime::component::types::ComponentItem::ComponentFunc(_))
        );
        if !exports_run {
            return Err(host_err(
                "the component does not export `run` — the declared world requires \
                 `run: func(input: string) -> result<string, string>`"
                    .into(),
            ));
        }

        let capsule_id = manifest.capsule_id()?;
        Ok(Self {
            inner: Arc::new(HostInner {
                engine,
                component,
                manifest,
                capsule_id,
                connector: None,
                clock: Arc::new(default_clock),
                ticker_alive: Arc::new(AtomicBool::new(true)),
                ticker_started: AtomicBool::new(false),
            }),
        })
    }

    /// Plug the egress seam (required before invoking a capsule whose
    /// manifest grants `network`; see [`NetworkConnector`]).
    pub fn with_connector(mut self, connector: Arc<dyn NetworkConnector>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("with_connector before the host is shared")
            .connector = Some(connector);
        self
    }

    /// Override the clock the `clock` import reads (milliseconds). The
    /// default is the system wall clock; tests and seeded runs pin their
    /// own — the clock is a determinism boundary, which is why it is a
    /// grant and not ambient authority.
    pub fn with_clock(mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("with_clock before the host is shared")
            .clock = Arc::new(clock);
        self
    }

    /// The manifest this host was admitted under.
    pub fn manifest(&self) -> &CapsuleManifest {
        &self.inner.manifest
    }

    /// The admitted manifest's content address.
    pub fn capsule_id(&self) -> &CapsuleId {
        &self.inner.capsule_id
    }

    /// Run one invocation. The guest call is synchronous and CPU-bound; it
    /// runs on a blocking thread, the `wasm_node` discipline.
    pub async fn invoke(&self, invocation: CapsuleInvocation) -> Result<CapsuleOutcome> {
        let inner = Arc::clone(&self.inner);
        let name = inner.manifest.identity.name.clone();
        tokio::task::spawn_blocking(move || run_invocation(&inner, invocation))
            .await
            .map_err(|e| {
                RustyError::Node(format!("capsule host '{name}': task join failed: {e}"))
            })?
    }
}

/// The system wall clock, in milliseconds — the default clock import.
fn default_clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One invocation, start to finish: structural gate, resource setup,
/// guest call, output gate, invocation evidence.
fn run_invocation(inner: &HostInner, invocation: CapsuleInvocation) -> Result<CapsuleOutcome> {
    let manifest = &inner.manifest;
    let name = &manifest.identity.name;
    let host_err = |msg: String| RustyError::Node(format!("capsule host '{name}': {msg}"));
    let budget = manifest.budget.clamp(&invocation.budget);

    let mut journal = invocation.journal;
    let mut parent = invocation.parent;

    // The structural gate. Walk the component's imports before anything is
    // linked: an import whose capability class carries no grant is never
    // linked — the invocation is refused here, and the denial is journaled
    // with an empty-scope absent grant (no grant at any scope existed).
    // An import outside every supported world fails closed the same way.
    for (import, _extern) in inner.component.component_type().imports(&inner.engine) {
        match capability_for_import(import) {
            Some(kind) if any_grant_of_kind(&manifest.capabilities, kind) => {}
            Some(kind) => {
                let denial = CapsuleDenial::unscoped(
                    inner.capsule_id.clone(),
                    kind,
                    format!(
                        "the component imports `{import}` but the manifest grants no \
                         `{kind:?}` capability — the import does not exist for this guest"
                    ),
                );
                journal_capability_event(
                    &mut journal,
                    &mut parent,
                    RunEventKind::CapsuleDenied,
                    Effect::Pure,
                    &denial,
                    EventStatus::Ok,
                );
                return Err(host_err(format!("structural denial: {}", denial.detail)));
            }
            None => {
                return Err(host_err(format!(
                    "the component imports `{import}`, which no supported world declares — \
                     fail closed rather than guess"
                )));
            }
        }
    }

    let grants: Vec<CapabilityGrant> = manifest.capabilities.iter().cloned().collect();
    let wants_network = any_grant_of_kind(&grants, CapabilityKind::Network);
    if wants_network && inner.connector.is_none() {
        return Err(host_err(
            "the manifest grants `network` but the host was built without a connector — \
             a granted egress path is explicit, never ambient"
                .into(),
        ));
    }

    // Resource setup: fuel is the CPU budget, the epoch deadline is the
    // wall-time budget in ticks, the limiter is the memory budget.
    let mut store = Store::new(
        &inner.engine,
        StoreData {
            limiter: StoreLimits {
                max_memory_bytes: budget
                    .max_memory_bytes
                    .map(|bytes| bytes.try_into().unwrap_or(usize::MAX)),
            },
            grants: grants.clone(),
            capsule_id: inner.capsule_id.clone(),
            connector: inner.connector.clone(),
            clock: Arc::clone(&inner.clock),
            journal: journal.clone(),
            parent: parent.clone(),
            uses: Vec::new(),
            denials: Vec::new(),
        },
    );
    store.limiter(|s| &mut s.limiter);
    let fuel_set = budget.fuel.unwrap_or(u64::MAX);
    store.set_fuel(fuel_set).map_err(|e| {
        host_err(format!(
            "failed to set fuel (engine needs consume_fuel): {e}"
        ))
    })?;
    let deadline_ticks = match budget.wall_time_ms {
        Some(ms) => {
            ensure_ticker(inner);
            (ms / EPOCH_TICK_MS).max(1)
        }
        None => u64::MAX,
    };
    store.set_epoch_deadline(deadline_ticks);

    // Link exactly the imports the granted capabilities name.
    let mut linker = Linker::new(&inner.engine);
    if wants_network {
        linker
            .instance(NET_INSTANCE)
            .map_err(|e| host_err(format!("failed to define `{NET_INSTANCE}`: {e}")))?
            .func_wrap(
                "fetch",
                |mut ctx: wasmtime::StoreContextMut<'_, StoreData>,
                 (protocol, host, method, path, body): (
                    String,
                    String,
                    String,
                    String,
                    Option<String>,
                )| {
                    Ok((fetch_impl(&mut ctx, protocol, host, method, path, body),))
                },
            )
            .map_err(|e| host_err(format!("failed to link `fetch`: {e}")))?;
    }
    if any_grant_of_kind(&grants, CapabilityKind::Clock) {
        linker
            .instance(CLOCK_INSTANCE)
            .map_err(|e| host_err(format!("failed to define `{CLOCK_INSTANCE}`: {e}")))?
            .func_wrap(
                "now-millis",
                |mut ctx: wasmtime::StoreContextMut<'_, StoreData>, (): ()| {
                    Ok((now_millis_impl(&mut ctx),))
                },
            )
            .map_err(|e| host_err(format!("failed to link `now-millis`: {e}")))?;
    }

    let input = serde_json::to_string(&invocation.input)?;
    let started = Instant::now();
    let invocation_effect = manifest
        .effects
        .iter()
        .max()
        .copied()
        .unwrap_or(Effect::Pure);

    let outcome = (|| -> std::result::Result<(Value, u64), String> {
        let instance = linker
            .instantiate(&mut store, &inner.component)
            .map_err(|e| format!("instantiation failed: {e}"))?;
        let run = instance
            .get_typed_func::<(String,), (std::result::Result<String, String>,)>(&mut store, "run")
            .map_err(|e| {
                format!(
                    "bad `run` export (the world declares `run: func(input: string) -> \
                     result<string, string>`): {e}"
                )
            })?;
        let (result,) = run
            .call(&mut store, (input,))
            .map_err(|e| format!("guest run trapped: {e}"))?;
        let fuel_consumed = fuel_set.saturating_sub(store.get_fuel().unwrap_or(0));
        let output =
            result.map_err(|guest_err| format!("the guest returned an error: {guest_err}"))?;

        // The output gate: size first (the payload is untrusted), then
        // well-formed JSON. Schema validation is contract-pinned, not yet
        // enforced (module docs).
        if let Some(cap) = budget.max_output_bytes {
            if output.len() as u64 > cap {
                return Err(format!(
                    "guest output is {} bytes, over the {}-byte max_output_bytes budget",
                    output.len(),
                    cap
                ));
            }
        }
        let parsed: Value = serde_json::from_str(&output)
            .map_err(|e| format!("guest output is not valid JSON: {e}"))?;
        Ok((parsed, fuel_consumed))
    })();

    // Read the fuel accounting before consuming the store — the error
    // path's breach attribution needs it.
    let remaining_fuel = store.get_fuel().ok();
    let data = store.into_data();
    // The invocation event: a capsule invocation is a WASM guest call, so
    // it journals under the shipped `WasmCall` kind with the capsule id in
    // the payload — capability uses and denials chain alongside it from
    // the same parent.
    match outcome {
        Ok((output, fuel_consumed)) => {
            if let Some(journal) = &journal {
                let mut draft = EventDraft::new(RunEventKind::WasmCall, invocation_effect)
                    .input(json!({
                        "capsule_id": inner.capsule_id,
                        "input": invocation.input,
                    }))
                    .output(json!({
                        "capsule_id": inner.capsule_id,
                        "output": output,
                        "fuel_consumed": fuel_consumed,
                    }));
                if let Some(parent) = &parent {
                    draft = draft.parent(parent.clone());
                }
                journal.record(draft);
            }
            Ok(CapsuleOutcome {
                output,
                fuel_consumed,
                uses: data.uses,
                denials: data.denials,
            })
        }
        Err(message) => {
            // Budget attribution, stated only when confident: fuel reads
            // the store's own accounting; wall time reads the clock.
            // Anything else is reported unattributed rather than guessed.
            let breach = if budget.fuel.is_some() && remaining_fuel == Some(0) {
                Some("fuel")
            } else if budget
                .wall_time_ms
                .is_some_and(|ms| started.elapsed() >= Duration::from_millis(ms))
            {
                Some("wall_time")
            } else {
                None
            };
            if let Some(journal) = &journal {
                let mut output = json!({
                    "capsule_id": inner.capsule_id,
                    "error": message,
                });
                if let Some(breach) = breach {
                    output["budget_breach"] = json!(breach);
                }
                let mut draft = EventDraft::new(RunEventKind::WasmCall, invocation_effect)
                    .input(json!({
                        "capsule_id": inner.capsule_id,
                        "input": invocation.input,
                    }))
                    .output(output)
                    .status(EventStatus::Error);
                if let Some(parent) = &parent {
                    draft = draft.parent(parent.clone());
                }
                journal.record(draft);
            }
            Err(host_err(message))
        }
    }
}

/// Start the epoch ticker exactly once per host: a thread advancing the
/// engine's epoch every `EPOCH_TICK_MS` until the host drops. Store
/// deadlines are set in ticks, so every wall-budgeted invocation on this
/// engine preempts within one tick of its own deadline — no cross-talk
/// between concurrent invocations (each store's deadline is an absolute
/// point on the shared counter, not a peer's timer).
fn ensure_ticker(inner: &HostInner) {
    if inner
        .ticker_started
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    let engine = inner.engine.clone();
    let alive = Arc::downgrade(&inner.ticker_alive);
    std::thread::spawn(move || {
        while let Some(alive) = alive.upgrade() {
            std::thread::sleep(Duration::from_millis(EPOCH_TICK_MS));
            if !alive.load(Ordering::Relaxed) {
                break;
            }
            engine.increment_epoch();
        }
    });
}

/// The `fetch` import: scope match first, connector second, evidence
/// always. Returns the guest-visible `result<string, string>` — denials
/// and connector failures are in-band errors the guest can react to, and
/// both are journaled regardless.
fn fetch_impl(
    ctx: &mut wasmtime::StoreContextMut<'_, StoreData>,
    protocol: String,
    host: String,
    method: String,
    path: String,
    body: Option<String>,
) -> std::result::Result<String, String> {
    let data = ctx.data_mut();
    if !network_grant_covers(&data.grants, &protocol, &host, &method) {
        let denial = CapsuleDenial::scoped(
            data.capsule_id.clone(),
            CapabilityGrant::Network {
                hosts: vec![host.clone()],
                protocols: vec![protocol.clone()],
                methods: vec![method.clone()],
            },
            format!(
                "fetch {method} {protocol}://{host}{path} names no granted network scope — \
                 the absent grant is `network` covering {method} {protocol}://{host}"
            ),
        );
        data.denials.push(denial.clone());
        let detail = denial.detail.clone();
        if let Ok(output) = serde_json::to_value(&denial) {
            data.journal_event(
                RunEventKind::CapsuleDenied,
                Effect::Pure,
                output,
                EventStatus::Ok,
            );
        }
        return Err(format!("denied: {detail}"));
    }

    let request = FetchRequest {
        protocol: protocol.clone(),
        host: host.clone(),
        method: method.clone(),
        path: path.clone(),
        body,
    };
    let connector = data
        .connector
        .clone()
        .expect("the structural gate refuses network grants without a connector");
    let outcome = connector.fetch(&request);
    let effect = if is_read_only_method(&method) {
        Effect::ReadOnly
    } else {
        Effect::NonIdempotent
    };
    let (response, status, guest_result) = match &outcome {
        Ok(resp) => (
            json!({ "status": resp.status, "body": resp.body }),
            EventStatus::Ok,
            Ok(resp.body.clone()),
        ),
        Err(message) => (
            json!({ "error": message }),
            EventStatus::Error,
            Err(format!("connector failed: {message}")),
        ),
    };
    let data = ctx.data_mut();
    let use_record = CapsuleUse {
        capsule_id: data.capsule_id.clone(),
        capability: CapabilityKind::Network,
        operation: "fetch".into(),
        request: json!({
            "protocol": protocol,
            "host": host,
            "method": method,
            "path": path,
        }),
        response,
    };
    data.uses.push(use_record.clone());
    if let Ok(output) = serde_json::to_value(&use_record) {
        data.journal_event(RunEventKind::CapsuleCall, effect, output, status);
    }
    guest_result
}

/// The `now-millis` import: read the host clock, journal the use.
fn now_millis_impl(ctx: &mut wasmtime::StoreContextMut<'_, StoreData>) -> u64 {
    let data = ctx.data_mut();
    let millis = (data.clock)();
    let use_record = CapsuleUse {
        capsule_id: data.capsule_id.clone(),
        capability: CapabilityKind::Clock,
        operation: "now_millis".into(),
        request: json!({}),
        response: json!({ "millis": millis }),
    };
    data.uses.push(use_record.clone());
    if let Ok(output) = serde_json::to_value(&use_record) {
        data.journal_event(
            RunEventKind::CapsuleCall,
            Effect::ReadOnly,
            output,
            EventStatus::Ok,
        );
    }
    millis
}

/// Journal one capability event outside a store (the structural gate runs
/// before any store exists). Chains causally from `parent`, then advances
/// it — the same cursor [`StoreData::journal_event`] keeps.
fn journal_capability_event<T: Serialize>(
    journal: &mut Option<Journal>,
    parent: &mut Option<String>,
    kind: RunEventKind,
    effect: Effect,
    payload: &T,
    status: EventStatus,
) {
    let Some(journal) = journal.as_ref() else {
        return;
    };
    let Ok(output) = serde_json::to_value(payload) else {
        return;
    };
    let mut draft = EventDraft::new(kind, effect).output(output).status(status);
    if let Some(parent) = &parent {
        draft = draft.parent(parent.clone());
    }
    *parent = Some(journal.record(draft));
}
