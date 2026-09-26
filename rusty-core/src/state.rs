//! Shared state, channels, and reducers.
//!
//! In Rusty Core (as in LangGraph) **every state key is a channel** and the
//! channel's [`Reducer`] defines how node updates merge into the shared
//! state. Nodes never call each other; they publish partial updates to
//! channels, and the engine applies the per-channel reducer at the super-step
//! barrier.
//!
//! Channel semantics modeled here:
//!
//! | Reducer        | LangGraph analog            | Multi-write per super-step? |
//! |----------------|-----------------------------|-----------------------------|
//! | [`Reducer::Overwrite`]  | `LastValue`        | **No** — second write is [`RustyError::InvalidUpdate`] |
//! | [`Reducer::Append`]     | `BinaryOperatorAggregate` (list concat) | Yes |
//! | [`Reducer::DeepMerge`]  | custom merge reducer | Yes |
//! | [`Reducer::AddMessages`]| `add_messages`    | Yes (ID-aware upsert + append) |
//!
//! [`StateSpec`] is the graph's state schema: channel name → reducer. It also
//! performs super-step write validation in [`StateSpec::apply_super_step`].

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::error::{Result, RustyError};

/// The channel map's concrete type. `BTreeMap`, not `serde_json::Map`:
/// serde_json's `Map` is only fully implemented for `Map<String, Value>`,
/// and without the `preserve_order` feature it is B-tree-backed — sorted
/// iteration order identical to this one's, which is what keeps the custom
/// serde impls below byte-identical to the pre-W4 transparent-Map shape.
type ChannelMap = BTreeMap<String, Arc<Value>>;

/// The shared graph state: channel name → value, with copy-on-write sharing
/// at channel granularity (R0.7 wave 4).
///
/// This is the "untyped typed-dict" of the engine: nodes read the full state
/// snapshot and return partial updates keyed by channel name. Type safety for
/// concrete applications is layered on top via serde (de)serialization of
/// individual channel values.
///
/// # Copy-on-write representation
///
/// Every channel value sits behind an [`Arc`], and the channel map itself
/// behind another. Cloning a `State` — the executor's per-super-step
/// snapshot, each node's private copy, the checkpoint's copy — is two
/// refcount bumps, O(1) in the state's size, where it used to be a deep
/// clone of every channel. A write (a reducer merge at the barrier, an
/// engine [`State::insert`]) touches only what it must: the map is cloned
/// shallowly when shared (one refcount bump per channel), and a channel's
/// value is cloned only when some other `State` still shares it —
/// `Arc::make_mut` semantics per channel. Unchanged channels stay shared
/// between the pre- and post-step states and every node's snapshot, which is
/// also what delta checkpoints diff against (see
/// [`crate::checkpoint::encode_delta`]): sharing and deltas are the same
/// structural observation.
///
/// The public contract is unchanged and pinned: [`State::get`] still returns
/// `Option<&Value>`, serde still sees the same JSON object (the custom
/// impls below serialize exactly like the previous transparent `Map`, so
/// checkpoints, the wire, the SDKs, and golden files stay byte-identical),
/// and reducer semantics are untouched. What changed is representation only.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    inner: Arc<ChannelMap>,
}

/// Serializes exactly like the `Map<String, Value>` this type used to wrap
/// transparently: a JSON object of channel name → value. Byte-identity with
/// the pre-W4 wire shape is the contract — checkpoints, goldens, and SDK
/// payloads must not drift because the interior changed.
impl Serialize for State {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.inner.len()))?;
        for (channel, value) in self.inner.iter() {
            map.serialize_entry(channel, value.as_ref())?;
        }
        map.end()
    }
}

/// Parses any JSON object, as the previous transparent `Map` did.
impl<'de> Deserialize<'de> for State {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Ok(Self::from_map(Map::<String, Value>::deserialize(
            deserializer,
        )?))
    }
}

impl State {
    /// An empty state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Wrap an existing JSON object.
    pub fn from_map(inner: Map<String, Value>) -> Self {
        Self {
            inner: Arc::new(inner.into_iter().map(|(k, v)| (k, Arc::new(v))).collect()),
        }
    }

    /// Build a state from any serializable value that is a JSON object.
    pub fn from_value(value: Value) -> Result<Self> {
        match value {
            Value::Object(inner) => Ok(Self::from_map(inner)),
            // Report only the type: the value itself may be a multi-MB blob.
            other => Err(RustyError::InvalidUpdate(format!(
                "state must be a JSON object, got {}",
                json_type_name(&other)
            ))),
        }
    }

    /// Serialize the whole state back into a [`Value::Object`].
    pub fn to_value(&self) -> Value {
        Value::Object(
            self.inner
                .iter()
                .map(|(k, v)| (k.clone(), v.as_ref().clone()))
                .collect(),
        )
    }

    /// Consume the state, returning the underlying map. Channel values that
    /// no other state shares are unwrapped without copying; shared ones are
    /// cloned (copy-on-write).
    pub fn into_map(self) -> Map<String, Value> {
        match Arc::try_unwrap(self.inner) {
            Ok(map) => map
                .into_iter()
                .map(|(k, v)| (k, Arc::unwrap_or_clone(v)))
                .collect(),
            Err(shared) => shared
                .iter()
                .map(|(k, v)| (k.clone(), v.as_ref().clone()))
                .collect(),
        }
    }

    /// Iterate over `(channel, value)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.inner.iter().map(|(k, v)| (k.as_str(), v.as_ref()))
    }

    /// Get a channel's current value.
    pub fn get(&self, channel: &str) -> Option<&Value> {
        self.inner.get(channel).map(Arc::as_ref)
    }

    /// `true` if the channel exists in the state (regardless of value).
    pub fn contains(&self, channel: &str) -> bool {
        self.inner.contains_key(channel)
    }

    /// Directly set a channel's value, bypassing reducer semantics.
    ///
    /// Intended for engine internals (initial state seeding, checkpoint
    /// restore). Nodes should always go through reducers.
    pub fn insert(&mut self, channel: impl Into<String>, value: Value) {
        self.insert_shared(channel, Arc::new(value));
    }

    /// Deserialize a channel into a concrete type.
    pub fn get_as<T: serde::de::DeserializeOwned>(&self, channel: &str) -> Result<Option<T>> {
        match self.get(channel) {
            None => Ok(None),
            // `&Value` implements `Deserializer`; no need to clone first.
            Some(v) => Ok(Some(T::deserialize(v)?)),
        }
    }

    /// Number of channels currently present.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// `true` if no channels are present.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// A shared handle to one channel's value. Test-only: production code
    /// reaches channels through `get` / `shared_channels`; the copy-on-write
    /// tests use this to assert pointer-level sharing.
    #[cfg(test)]
    pub(crate) fn shared_channel(&self, channel: &str) -> Option<Arc<Value>> {
        self.inner.get(channel).cloned()
    }

    /// All channels as shared values.
    pub(crate) fn shared_channels(&self) -> impl Iterator<Item = (&str, &Arc<Value>)> {
        self.inner.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Insert a channel value that is already shared, preserving sharing.
    /// Clones the channel map shallowly when other states share it.
    pub(crate) fn insert_shared(&mut self, channel: impl Into<String>, value: Arc<Value>) {
        Arc::make_mut(&mut self.inner).insert(channel.into(), value);
    }

    /// Build a state from shared channel values (delta encoding/decoding in
    /// `checkpoint.rs`), preserving sharing with the states they came from.
    pub(crate) fn from_shared_channels<I, S>(channels: I) -> Self
    where
        I: IntoIterator<Item = (S, Arc<Value>)>,
        S: Into<String>,
    {
        Self {
            inner: Arc::new(channels.into_iter().map(|(k, v)| (k.into(), v)).collect()),
        }
    }

    /// The channels whose values differ from `base`'s, as shared values —
    /// the channel-level delta a W4 delta checkpoint persists. Pointer
    /// equality short-circuits: a channel still shared with the base is
    /// unchanged without a value walk. Value equality still dedupes channels
    /// that were rebuilt with identical content (e.g. after a deserialize),
    /// so equal bytes are never written twice.
    pub(crate) fn channels_changed_since(&self, base: &State) -> Vec<(String, Arc<Value>)> {
        self.shared_channels()
            .filter(|(channel, value)| match base.inner.get(*channel) {
                Some(base_value) if Arc::ptr_eq(value, base_value) => false,
                Some(base_value) => value.as_ref() != base_value.as_ref(),
                None => true,
            })
            .map(|(channel, value)| (channel.to_owned(), value.clone()))
            .collect()
    }
}

impl From<Map<String, Value>> for State {
    fn from(inner: Map<String, Value>) -> Self {
        Self::from_map(inner)
    }
}

/// Per-channel merge semantics.
///
/// A reducer is conceptually a binary function
/// `reduce(current: Option<&Value>, update: Value) -> Value`, mirroring
/// LangGraph's `Annotated[T, reducer]` channel annotations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Reducer {
    /// `LastValue` semantics: the update replaces the current value.
    ///
    /// **At most one write per super-step** — a second write to the same
    /// channel within one super-step fails with
    /// [`RustyError::InvalidUpdate`]. This is the default for any
    /// channel and the classic production failure mode in parallel graphs;
    /// if you need fan-in, use [`Reducer::Append`], [`Reducer::DeepMerge`],
    /// or [`Reducer::AddMessages`].
    #[default]
    Overwrite,

    /// List-concat semantics: the current value is treated as an array
    /// (a missing current value starts as `[]`). If the update is an
    /// array it is extended onto the current array; otherwise the update is
    /// pushed as a single element.
    ///
    /// A **non-array current value** is rejected by
    /// [`StateSpec::apply_super_step`] with [`RustyError::InvalidUpdate`];
    /// only direct calls to [`Reducer::apply`] coerce it to `[]`.
    Append,

    /// Recursive object merge: two JSON objects are merged key-by-key
    /// (nested objects merge recursively); any non-object pair resolves to
    /// the update value. A missing current value resolves to the update.
    DeepMerge,

    /// LangGraph `add_messages` semantics over a message array.
    ///
    /// The current value is treated as an array of message objects. The
    /// update may be a single message object or an array of messages. Each
    /// incoming message is **upserted**: if it has an `"id"` field equal to
    /// an existing message's `"id"`, the existing message is replaced in
    /// place; otherwise the message is appended. Messages whose `"id"` is
    /// present but not a string are treated as id-less (always appended).
    ///
    /// Like [`Reducer::Append`], a non-array current value is rejected by
    /// [`StateSpec::apply_super_step`].
    AddMessages,
}

impl Reducer {
    /// Whether this channel accepts multiple writes within one super-step.
    ///
    /// Only `LastValue`-style channels ([`Reducer::Overwrite`]) are
    /// single-write; aggregating reducers exist precisely to support
    /// parallel fan-in.
    pub fn allows_multiple_writes(self) -> bool {
        !matches!(self, Reducer::Overwrite)
    }

    /// Apply one update to a channel's current value.
    ///
    /// `current` is `None` when the channel has never been written.
    ///
    /// For [`Reducer::Append`] and [`Reducer::AddMessages`], a non-array
    /// `current` is treated as `[]` (the update starts a fresh array).
    /// [`StateSpec::apply_super_step`] rejects that case with
    /// [`RustyError::InvalidUpdate`] before reducers run; the coercion
    /// here exists only for direct callers of this method.
    pub fn apply(&self, current: Option<&Value>, update: Value) -> Value {
        match self {
            Reducer::Overwrite => update,
            Reducer::Append => match current {
                Some(Value::Array(existing)) => {
                    let mut out = existing.clone();
                    append_in_place(&mut out, update);
                    Value::Array(out)
                }
                _ => match update {
                    Value::Array(items) => Value::Array(items),
                    single => Value::Array(vec![single]),
                },
            },
            Reducer::DeepMerge => match current {
                Some(cur) => deep_merge(cur, &update),
                None => update,
            },
            Reducer::AddMessages => add_messages(current, update),
        }
    }

    /// The copy-on-write twin of [`Reducer::apply`]: identical semantics,
    /// but operating on the channel's shared value so uniquely owned values
    /// mutate in place.
    ///
    /// When no snapshot, checkpoint, or sibling state shares the current
    /// value, an aggregating reducer (Append / DeepMerge / AddMessages)
    /// merges into it directly — an Append push onto a uniquely owned array
    /// is amortized O(1) instead of the O(N) clone [`Reducer::apply`] always
    /// pays. When the value is shared, only this channel is cloned: the
    /// merge never pays for channels it did not write, which is the whole
    /// point of the W4 representation. The result is wrapped in a fresh
    /// [`Arc`] only when a new value was produced; an in-place merge returns
    /// the same allocation it was given.
    pub(crate) fn apply_shared(&self, current: Option<Arc<Value>>, update: Value) -> Arc<Value> {
        match self {
            Reducer::Overwrite => Arc::new(update),
            Reducer::Append => match current {
                Some(mut shared) if shared.is_array() => match Arc::get_mut(&mut shared) {
                    Some(Value::Array(existing)) => {
                        append_in_place(existing, update);
                        shared
                    }
                    // Shared with a live snapshot or checkpoint: copy this
                    // channel only, then merge into the copy.
                    _ => {
                        let Value::Array(existing) = shared.as_ref() else {
                            unreachable!("guarded by is_array above")
                        };
                        let mut out = existing.clone();
                        append_in_place(&mut out, update);
                        Arc::new(Value::Array(out))
                    }
                },
                // Missing or non-array current (the latter is rejected by
                // `apply_super_step` validation; direct callers get the same
                // coercion `apply` documents): the update starts a fresh array.
                _ => Arc::new(match update {
                    Value::Array(items) => Value::Array(items),
                    single => Value::Array(vec![single]),
                }),
            },
            Reducer::DeepMerge => match current {
                Some(mut shared) => match Arc::get_mut(&mut shared) {
                    Some(current) => {
                        deep_merge_in_place(current, &update);
                        shared
                    }
                    None => Arc::new(deep_merge(shared.as_ref(), &update)),
                },
                None => Arc::new(update),
            },
            Reducer::AddMessages => match current {
                Some(mut shared) if shared.is_array() => match Arc::get_mut(&mut shared) {
                    Some(Value::Array(messages)) => {
                        upsert_messages_in_place(messages, update);
                        shared
                    }
                    _ => {
                        let Value::Array(existing) = shared.as_ref() else {
                            unreachable!("guarded by is_array above")
                        };
                        let mut messages = existing.clone();
                        upsert_messages_in_place(&mut messages, update);
                        Arc::new(Value::Array(messages))
                    }
                },
                _ => Arc::new(add_messages(None, update)),
            },
        }
    }
}

impl std::fmt::Display for Reducer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Reducer::Overwrite => "overwrite",
            Reducer::Append => "append",
            Reducer::DeepMerge => "deep_merge",
            Reducer::AddMessages => "add_messages",
        };
        f.write_str(name)
    }
}

/// Short type name for error messages (avoids embedding the value itself).
fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Recursive JSON object merge. Non-object pairs resolve to `b`.
fn deep_merge(a: &Value, b: &Value) -> Value {
    let mut merged = a.clone();
    deep_merge_in_place(&mut merged, b);
    merged
}

/// The in-place core of [`deep_merge`]: merge `b` into `a` without cloning
/// `a` first. Shared by the copy-on-write merge path, which calls this
/// directly when the channel value is uniquely owned.
fn deep_merge_in_place(a: &mut Value, b: &Value) {
    match (&mut *a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, v) in y {
                match x.get_mut(k) {
                    Some(cur) => deep_merge_in_place(cur, v),
                    None => {
                        x.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        _ => *a = b.clone(),
    }
}

/// The in-place core of [`Reducer::Append`]'s merge: extend an existing
/// array with the update (array updates concatenate; anything else pushes
/// as a single element).
fn append_in_place(existing: &mut Vec<Value>, update: Value) {
    match update {
        Value::Array(items) => existing.extend(items),
        single => existing.push(single),
    }
}

/// `add_messages` semantics: ID-aware upsert + append over a message array.
///
/// A message whose `"id"` is not a string (e.g. `{"id": 123}`) is treated as
/// id-less and always appended — it can never match an existing message.
fn add_messages(current: Option<&Value>, update: Value) -> Value {
    let mut messages: Vec<Value> = match current {
        Some(Value::Array(existing)) => existing.clone(),
        _ => Vec::new(),
    };
    upsert_messages_in_place(&mut messages, update);
    Value::Array(messages)
}

/// The in-place core of [`add_messages`]: upsert the update's messages into
/// an existing array by `"id"`, appending id-less and unknown-id messages.
fn upsert_messages_in_place(messages: &mut Vec<Value>, update: Value) {
    let incoming: Vec<Value> = match update {
        Value::Array(items) => items,
        single => vec![single],
    };
    // One pass to index existing ids; the upsert loop then runs O(1) per
    // incoming message instead of scanning the whole array each time.
    let mut index_of: HashMap<String, usize> = HashMap::with_capacity(messages.len());
    for (i, m) in messages.iter().enumerate() {
        if let Some(id) = m.get("id").and_then(Value::as_str) {
            index_of.entry(id.to_owned()).or_insert(i);
        }
    }
    for msg in incoming {
        let msg_id = msg.get("id").and_then(Value::as_str).map(str::to_owned);
        match msg_id.as_deref().and_then(|id| index_of.get(id).copied()) {
            Some(i) => messages[i] = msg,
            None => {
                if let Some(id) = msg_id {
                    index_of.insert(id, messages.len());
                }
                messages.push(msg);
            }
        }
    }
}

/// The graph's state schema: channel name → [`Reducer`].
///
/// The spec serves two roles:
///
/// 1. **Merge semantics**: at each super-step barrier, node updates are
///    merged into the shared state via the channel's reducer
///    ([`StateSpec::apply_super_step`]).
/// 2. **Write validation** (`LastValue` rule): within one super-step, a
///    single-write channel may receive at most one write across *all*
///    nodes that ran in parallel. A second write yields
///    [`RustyError::InvalidUpdate`], mirroring LangGraph's
///    `InvalidUpdateError: Can receive only one value per step`.
///
/// Writes to channels **not declared** in the spec are also rejected with
/// [`RustyError::InvalidUpdate`]; the spec is the complete schema.
#[derive(Debug, Clone, Default)]
pub struct StateSpec {
    channels: HashMap<String, Reducer>,
}

impl StateSpec {
    /// An empty spec (no channels declared).
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder-style: declare a channel with the given reducer.
    pub fn channel(mut self, name: impl Into<String>, reducer: Reducer) -> Self {
        self.channels.insert(name.into(), reducer);
        self
    }

    /// Mutable variant of [`StateSpec::channel`].
    pub fn add_channel(&mut self, name: impl Into<String>, reducer: Reducer) -> &mut Self {
        self.channels.insert(name.into(), reducer);
        self
    }

    /// The reducer for a channel, defaulting to [`Reducer::Overwrite`]
    /// (`LastValue` semantics) for undeclared channels.
    ///
    /// The default is a convenience for contexts where the channel is known
    /// to be declared. When the channel may be undeclared, use
    /// [`StateSpec::try_reducer_for`] and handle `None` — silently treating
    /// an undeclared channel as `Overwrite` is almost never the intent.
    /// [`StateSpec::apply_super_step`] always validates channels first, so
    /// the default is unreachable on that path.
    pub fn reducer_for(&self, channel: &str) -> Reducer {
        self.channels.get(channel).copied().unwrap_or_default()
    }

    /// The reducer for a channel, or `None` if the channel is not declared.
    pub fn try_reducer_for(&self, channel: &str) -> Option<Reducer> {
        self.channels.get(channel).copied()
    }

    /// All declared channel names.
    pub fn channel_names(&self) -> impl Iterator<Item = &str> {
        self.channels.keys().map(String::as_str)
    }

    /// `true` if the channel is declared in this spec.
    pub fn has_channel(&self, channel: &str) -> bool {
        self.channels.contains_key(channel)
    }

    /// Validate and merge **all writes of one super-step** into `state`.
    ///
    /// `writes` is the collection of `(node_name, updates)` pairs produced by
    /// the nodes that ran in this super-step. Each entry is one node's
    /// partial update map (`channel -> value`), as carried by
    /// [`crate::node::NodeOutput::updates`].
    ///
    /// Semantics, in order:
    ///
    /// 1. Every written channel must be declared in this spec
    ///    ([`RustyError::InvalidUpdate`] otherwise).
    /// 2. A channel whose reducer does **not** allow multiple writes
    ///    (i.e. [`Reducer::Overwrite`]) may appear in at most one node's
    ///    updates per super-step; a second write is
    ///    [`RustyError::InvalidUpdate`].
    /// 3. For [`Reducer::Append`] and [`Reducer::AddMessages`] channels, the
    ///    current state value (if present) must be an array; a non-array
    ///    current value indicates a type bug in the graph spec and is
    ///    rejected with [`RustyError::InvalidUpdate`] rather than
    ///    silently discarded.
    /// 4. Surviving writes are merged via the channel reducer in
    ///    **deterministic order: sorted by node name**. The executor
    ///    collects writes from concurrently completing tasks, so callers
    ///    cannot rely on input order; canonicalizing here keeps fan-in
    ///    results (and checkpoints derived from them) stable run-to-run.
    ///
    /// Validation completes before any mutation: on error the state is left
    /// entirely unmodified and the caller (executor) should abort the
    /// super-step — LangGraph treats a super-step as transactional.
    pub fn apply_super_step<I, S>(&self, state: &mut State, writes: I) -> Result<()>
    where
        I: IntoIterator<Item = (S, HashMap<String, Value>)>,
        S: AsRef<str>,
    {
        // Collect up front so the whole super-step is validated before a
        // single channel is touched — that is what makes failure
        // all-or-nothing. Also canonicalize the merge order: executors feed
        // writes in task-completion order, which is nondeterministic.
        let mut collected: Vec<(String, HashMap<String, Value>)> = writes
            .into_iter()
            .map(|(node, updates)| (node.as_ref().to_owned(), updates))
            .collect();
        collected.sort_by(|a, b| a.0.cmp(&b.0));

        let mut write_counts: HashMap<&str, usize> = HashMap::new();
        let mut first_writer: HashMap<&str, &str> = HashMap::new();

        for (node, updates) in &collected {
            for channel in updates.keys() {
                let Some(reducer) = self.try_reducer_for(channel) else {
                    return Err(RustyError::InvalidUpdate(format!(
                        "node `{node}` wrote to undeclared channel `{channel}`; \
                         declare it in the StateSpec"
                    )));
                };
                // Aggregating-over-array reducers must not silently discard
                // a mistyped current value; that is a spec bug, not data.
                if matches!(reducer, Reducer::Append | Reducer::AddMessages) {
                    if let Some(current) = state.get(channel) {
                        if !current.is_array() {
                            return Err(RustyError::InvalidUpdate(format!(
                                "node `{node}` wrote to channel `{channel}` (reducer: \
                                 {reducer}), but the current value is a {}; the \
                                 reducer requires an array",
                                json_type_name(current)
                            )));
                        }
                    }
                }
                let count = write_counts.entry(channel.as_str()).or_insert(0);
                *count += 1;
                first_writer
                    .entry(channel.as_str())
                    .or_insert(node.as_str());
                if *count > 1 && !reducer.allows_multiple_writes() {
                    return Err(RustyError::InvalidUpdate(format!(
                        "channel `{channel}` can receive only one value per super-step \
                         (reducer: {reducer}); already written by node `{}`, second write from \
                         node `{node}`. Use a multi-write reducer (Append/DeepMerge/\
                         AddMessages) to handle concurrent writes.",
                        first_writer[channel.as_str()],
                    )));
                }
            }
        }

        for (_node, updates) in collected {
            for (channel, update) in updates {
                let reducer = self.reducer_for(&channel);
                // Copy-on-write merge: the channel is taken OUT of the map
                // first, so a value no snapshot or checkpoint shares has
                // refcount 1 here and the reducer merges into it in place;
                // a shared value is cloned by the reducer — that channel
                // alone, never the whole state.
                let current = Arc::make_mut(&mut state.inner).remove(&channel);
                let merged = reducer.apply_shared(current, update);
                state.insert_shared(channel, merged);
            }
        }
        Ok(())
    }

    /// Convenience: merge a single node's updates (e.g. outside the parallel
    /// super-step path). Single-write validation trivially passes since only
    /// one writer is involved.
    pub fn apply_single(
        &self,
        state: &mut State,
        node: &str,
        updates: HashMap<String, Value>,
    ) -> Result<()> {
        self.apply_super_step(state, [(node, updates)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn updates(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn overwrite_replaces() {
        let mut state = State::new();
        let spec = StateSpec::new().channel("x", Reducer::Overwrite);
        spec.apply_single(&mut state, "n1", updates(&[("x", json!(1))]))
            .unwrap();
        spec.apply_single(&mut state, "n2", updates(&[("x", json!(2))]))
            .unwrap();
        assert_eq!(state.get("x"), Some(&json!(2)));
    }

    #[test]
    fn last_value_double_write_fails() {
        let mut state = State::new();
        let spec = StateSpec::new().channel("x", Reducer::Overwrite);
        let writes = vec![
            ("a".to_string(), updates(&[("x", json!(1))])),
            ("b".to_string(), updates(&[("x", json!(2))])),
        ];
        let err = spec.apply_super_step(&mut state, writes).unwrap_err();
        assert!(matches!(err, RustyError::InvalidUpdate(_)));
    }

    #[test]
    fn append_allows_fan_in() {
        let mut state = State::new();
        let spec = StateSpec::new().channel("xs", Reducer::Append);
        let writes = vec![
            ("a".to_string(), updates(&[("xs", json!([1, 2]))])),
            ("b".to_string(), updates(&[("xs", json!(3))])),
        ];
        spec.apply_super_step(&mut state, writes).unwrap();
        assert_eq!(state.get("xs"), Some(&json!([1, 2, 3])));
    }

    #[test]
    fn deep_merge_is_recursive() {
        let mut state = State::from_value(json!({"cfg": {"a": 1, "nested": {"x": 1}}})).unwrap();
        let spec = StateSpec::new().channel("cfg", Reducer::DeepMerge);
        spec.apply_single(
            &mut state,
            "n",
            updates(&[("cfg", json!({"nested": {"y": 2}}))]),
        )
        .unwrap();
        assert_eq!(
            state.get("cfg"),
            Some(&json!({"a": 1, "nested": {"x": 1, "y": 2}}))
        );
    }

    #[test]
    fn add_messages_upserts_by_id() {
        let mut state = State::from_value(json!({
            "messages": [{"id": "m1", "content": "old"}, {"content": "plain"}]
        }))
        .unwrap();
        let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
        spec.apply_single(
            &mut state,
            "n",
            updates(&[(
                "messages",
                json!([
                    {"id": "m1", "content": "new"},
                    {"content": "appended"}
                ]),
            )]),
        )
        .unwrap();
        assert_eq!(
            state.get("messages"),
            Some(&json!([
                {"id": "m1", "content": "new"},
                {"content": "plain"},
                {"content": "appended"}
            ]))
        );
    }

    #[test]
    fn undeclared_channel_rejected() {
        let mut state = State::new();
        let spec = StateSpec::new().channel("x", Reducer::Overwrite);
        let err = spec
            .apply_single(&mut state, "n", updates(&[("y", json!(1))]))
            .unwrap_err();
        assert!(matches!(err, RustyError::InvalidUpdate(_)));
    }

    #[test]
    fn fan_in_merge_order_is_deterministic() {
        // The executor feeds writes in task-completion order; the merge must
        // not depend on it.
        let spec = StateSpec::new()
            .channel("xs", Reducer::Append)
            .channel("messages", Reducer::AddMessages);
        let forward = vec![
            (
                "b".to_string(),
                updates(&[("xs", json!([2])), ("messages", json!({"id": "b", "v": 2}))]),
            ),
            (
                "a".to_string(),
                updates(&[("xs", json!([1])), ("messages", json!({"id": "a", "v": 1}))]),
            ),
        ];
        let mut reversed = forward.clone();
        reversed.reverse();

        let mut s1 = State::new();
        let mut s2 = State::new();
        spec.apply_super_step(&mut s1, forward).unwrap();
        spec.apply_super_step(&mut s2, reversed).unwrap();
        assert_eq!(s1, s2);
        assert_eq!(s1.get("xs"), Some(&json!([1, 2])));
        assert_eq!(
            s1.get("messages"),
            Some(&json!([{"id": "a", "v": 1}, {"id": "b", "v": 2}]))
        );
    }

    #[test]
    fn append_rejects_non_array_current_value() {
        let mut state = State::from_value(json!({"xs": {"oops": "object"}})).unwrap();
        let spec = StateSpec::new().channel("xs", Reducer::Append);
        let err = spec
            .apply_single(&mut state, "n", updates(&[("xs", json!([1]))]))
            .unwrap_err();
        assert!(matches!(err, RustyError::InvalidUpdate(_)));
        // All-or-nothing: the failed super-step leaves state untouched.
        assert_eq!(state.get("xs"), Some(&json!({"oops": "object"})));
    }

    #[test]
    fn add_messages_rejects_non_array_current_value() {
        let mut state = State::from_value(json!({"messages": 42})).unwrap();
        let spec = StateSpec::new().channel("messages", Reducer::AddMessages);
        let err = spec
            .apply_single(
                &mut state,
                "n",
                updates(&[("messages", json!({"id": "m1"}))]),
            )
            .unwrap_err();
        assert!(matches!(err, RustyError::InvalidUpdate(_)));
        assert_eq!(state.get("messages"), Some(&json!(42)));
    }

    // ---- Copy-on-write representation (R0.7 wave 4) ----

    #[test]
    fn clone_shares_channels_until_write() {
        let mut state = State::from_value(json!({"a": [1, 2, 3], "b": {"x": 1}})).unwrap();
        let snapshot = state.clone();

        // Cloning shares every channel (pointer-identical, no copies).
        for channel in ["a", "b"] {
            assert!(Arc::ptr_eq(
                &state.shared_channel(channel).unwrap(),
                &snapshot.shared_channel(channel).unwrap()
            ));
        }

        // A write to one channel copies only that channel; the other stays
        // shared with the snapshot.
        state.insert("a", json!([9]));
        assert!(!Arc::ptr_eq(
            &state.shared_channel("a").unwrap(),
            &snapshot.shared_channel("a").unwrap()
        ));
        assert!(Arc::ptr_eq(
            &state.shared_channel("b").unwrap(),
            &snapshot.shared_channel("b").unwrap()
        ));
        // Isolation: the snapshot is untouched.
        assert_eq!(snapshot.get("a"), Some(&json!([1, 2, 3])));
        assert_eq!(state.get("a"), Some(&json!([9])));
    }

    #[test]
    fn reducer_merge_is_copy_on_write_at_channel_granularity() {
        let spec = StateSpec::new()
            .channel("xs", Reducer::Append)
            .channel("ys", Reducer::Append);
        let mut state = State::from_value(json!({"xs": [1], "ys": ["keep"]})).unwrap();
        let snapshot = state.clone();

        // Merge into `xs` while `snapshot` is alive: `xs` is copied
        // (snapshot keeps the old array), `ys` is never touched.
        spec.apply_single(&mut state, "n", updates(&[("xs", json!(2))]))
            .unwrap();
        assert_eq!(state.get("xs"), Some(&json!([1, 2])));
        assert_eq!(snapshot.get("xs"), Some(&json!([1])));
        assert!(Arc::ptr_eq(
            &state.shared_channel("ys").unwrap(),
            &snapshot.shared_channel("ys").unwrap()
        ));
    }

    #[test]
    fn append_mutates_in_place_when_uniquely_owned() {
        let spec = StateSpec::new().channel("xs", Reducer::Append);
        let mut state = State::from_value(json!({"xs": [1]})).unwrap();
        let before = Arc::as_ptr(&state.shared_channel("xs").unwrap());
        spec.apply_single(&mut state, "n", updates(&[("xs", json!(2))]))
            .unwrap();
        // Unique ownership: the merge reused the same allocation instead of
        // cloning the array.
        assert_eq!(state.get("xs"), Some(&json!([1, 2])));
        assert_eq!(before, Arc::as_ptr(&state.shared_channel("xs").unwrap()));
    }

    #[test]
    fn deep_merge_mutates_in_place_when_uniquely_owned() {
        let spec = StateSpec::new().channel("cfg", Reducer::DeepMerge);
        let mut state = State::from_value(json!({"cfg": {"a": 1, "nested": {"x": 1}}})).unwrap();
        let before = Arc::as_ptr(&state.shared_channel("cfg").unwrap());
        spec.apply_single(
            &mut state,
            "n",
            updates(&[("cfg", json!({"nested": {"y": 2}}))]),
        )
        .unwrap();
        assert_eq!(
            state.get("cfg"),
            Some(&json!({"a": 1, "nested": {"x": 1, "y": 2}}))
        );
        assert_eq!(before, Arc::as_ptr(&state.shared_channel("cfg").unwrap()));
    }

    #[test]
    fn serde_is_byte_identical_to_plain_map() {
        let state = State::from_value(json!({
            "blob": "payload",
            "meta": {"kind": "test", "n": 42},
            "list": [1, 2.5, null, true]
        }))
        .unwrap();
        let plain = state.to_value();

        // Compact and pretty forms both match the plain-Value serialization
        // of the same object — the pre-W4 transparent-Map wire shape.
        assert_eq!(
            serde_json::to_string(&state).unwrap(),
            serde_json::to_string(&plain).unwrap()
        );
        assert_eq!(
            serde_json::to_string_pretty(&state).unwrap(),
            serde_json::to_string_pretty(&plain).unwrap()
        );

        // Deserialize round-trips through both representations.
        let from_state_json: State =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        let from_value_json: State = serde_json::from_value(plain).unwrap();
        assert_eq!(state, from_state_json);
        assert_eq!(state, from_value_json);
    }

    #[test]
    fn into_map_and_iter_round_trip() {
        let state = State::from_value(json!({"a": 1, "b": [2]})).unwrap();
        let iterated: HashMap<String, Value> = state
            .iter()
            .map(|(k, v)| (k.to_owned(), v.clone()))
            .collect();
        assert_eq!(iterated["a"], json!(1));
        assert_eq!(iterated["b"], json!([2]));

        // Shared with a clone: into_map still yields owned values.
        let snapshot = state.clone();
        let map = state.into_map();
        assert_eq!(map["a"], json!(1));
        assert_eq!(snapshot.get("b"), Some(&json!([2])));
    }

    #[test]
    fn channels_changed_since_diffs_by_pointer_then_value() {
        let base =
            State::from_value(json!({"same": 1, "changed": [1], "gone_from_delta": true})).unwrap();
        let mut next = base.clone();
        next.insert("changed", json!([1, 2]));
        next.insert("new", json!("hello"));
        // Rebuilt with identical content: value-equal but not pointer-equal.
        next.insert("same", json!(1));

        let delta = next.channels_changed_since(&base);
        let names: Vec<&str> = delta.iter().map(|(k, _)| k.as_str()).collect();
        // `changed` (new value) and `new` (absent from base) are in; the
        // value-equal rebuild of `same` is deduped; `gone_from_delta` is
        // unchanged and channels are never deleted.
        assert_eq!(names, ["changed", "new"]);

        // Against an empty base every channel is a change.
        assert_eq!(next.channels_changed_since(&State::new()).len(), 4);
    }
}
