//! SSE stream construction: event-log replay + live broadcast fan-out.
//!
//! Frame ids follow `{checkpoint_id}:{step}:{seq}` (see [`crate::runs`]).
//! Replay semantics live on the attach endpoint (`GET /runs/{id}/stream`):
//! a client that reconnects with a `Last-Event-ID` header skips every frame
//! whose sequence number it has already seen, then follows the live stream.
//! The run-create endpoint starts a fresh sequence per run and ignores the
//! header — applying a stale seq there would silently drop the new run's
//! first frames. The stream terminates after the run's `end` frame.

use std::convert::Infallible;

use axum::response::sse::Event;
use futures::{stream, Stream, StreamExt};
use tokio::sync::broadcast;

use crate::runs::SseFrame;

impl SseFrame {
    /// Render as an axum SSE [`Event`].
    pub(crate) fn into_event(self) -> Event {
        Event::default()
            .event(self.event)
            .id(self.id)
            .data(serde_json::to_string(&self.data).unwrap_or_else(|_| "{}".to_string()))
    }
}

struct Live {
    rx: broadcast::Receiver<SseFrame>,
    last_seq: u64,
    finished: bool,
}

/// Build the SSE item stream for a run: replayed log frames (after
/// `skip_through_seq`) followed by live broadcast frames, ending on the
/// run's `end` frame.
pub(crate) fn frame_stream(
    replay: Vec<SseFrame>,
    rx: broadcast::Receiver<SseFrame>,
    skip_through_seq: u64,
) -> impl Stream<Item = Result<Event, Infallible>> + Send {
    let replay: Vec<SseFrame> = replay
        .into_iter()
        .filter(|f| f.seq > skip_through_seq)
        .collect();
    let finished = replay.last().is_some_and(|f| f.event == "end");
    let last_seq = replay.last().map(|f| f.seq).unwrap_or(skip_through_seq);

    let replay_stream = stream::iter(replay.into_iter().map(|f| Ok(f.into_event())));
    let live = stream::unfold(
        Live {
            rx,
            last_seq,
            finished,
        },
        |mut st| async move {
            if st.finished {
                return None;
            }
            loop {
                match st.rx.recv().await {
                    Ok(frame) => {
                        if frame.seq <= st.last_seq {
                            continue; // already replayed from the log
                        }
                        st.last_seq = frame.seq;
                        if frame.event == "end" {
                            st.finished = true;
                        }
                        return Some((Ok(frame.into_event()), st));
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            skipped,
                            "SSE receiver lagged; frames dropped (reconnect with Last-Event-ID)"
                        );
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );
    replay_stream.chain(live)
}

/// Parse a `Last-Event-ID` header value into the per-run sequence number it
/// refers to (the trailing component of `{checkpoint_id}:{step}:{seq}`).
pub(crate) fn parse_last_event_id(raw: Option<&str>) -> u64 {
    raw.and_then(|s| s.rsplit(':').next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}
