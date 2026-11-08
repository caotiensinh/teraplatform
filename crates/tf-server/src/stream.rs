//! Live run log over Server-Sent Events.
//!
//! `GET /runs/{id}/events` (cookie) and `GET /api/v2/runs/{id}/events` (cookie or token)
//! send, in order:
//!
//! * `status` - `{"status": "...", "label": "..."}` whenever the run's status is different
//!   from the last one sent (always once at the start);
//! * `log` - one stored log line, with `id:` set to its 0-based position;
//! * `done` - the final status, then the stream closes. A run waiting for a person is not
//!   final, so the stream stays open across the pause.
//!
//! A client resumes with the standard `Last-Event-ID` header (browsers send it by
//! themselves after a dropped connection) or `?from=<position>`; the header wins. The lines
//! are the stored ones, already masked, so a sensitive value can never be streamed. This
//! polls the store a few times a second; there is no push channel between runner and server
//! yet, which is fine for a single server instance.

use crate::api::ApiError;
use crate::authz::run_access;
use crate::AppState;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use serde::Deserialize;
use std::convert::Infallible;
use std::time::{Duration, Instant};
use tf_core::{Action, Principal};
use tokio_stream::wrappers::ReceiverStream;

/// How often the store is looked at for new lines.
const POLL: Duration = Duration::from_millis(150);
/// A stream ends by itself after this long; a browser reconnects and resumes where it was.
const MAX_STREAM: Duration = Duration::from_secs(30 * 60);

#[derive(Deserialize)]
pub struct FromQuery {
    from: Option<String>,
}

/// Where to resume: the line after `Last-Event-ID`, else `?from=`, else the start.
fn start_position(headers: &HeaderMap, q: &FromQuery) -> usize {
    headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<usize>().ok())
        .map(|n| n.saturating_add(1))
        .or_else(|| q.from.as_deref().and_then(|s| s.trim().parse::<usize>().ok()))
        .unwrap_or(0)
}

pub async fn run_events(
    State(st): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
    Query(q): Query<FromQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    run_access(&st, &p, &id, Action::Read).await?;
    let from = start_position(&headers, &q);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<SseEvent, Infallible>>(256);

    tokio::spawn(async move {
        let started = Instant::now();
        let (mut pos, mut last) = (from, None);
        macro_rules! send {
            ($e:expr) => {
                if tx.send(Ok($e)).await.is_err() {
                    return; // the client went away
                }
            };
        }
        loop {
            // Asked before reading: if the runner is already finished, everything it wrote is
            // in the lines read below.
            let running = st.cancels.is_running(&id);
            let Ok(run) = st.store.get_run(&id).await else { return };
            if last != Some(run.status) {
                last = Some(run.status);
                send!(SseEvent::default().event("status").data(serde_json::json!({ "status": run.status.as_str(), "label": run.status.label() }).to_string()));
            }
            for line in st.store.get_log_from(&id, pos).await.unwrap_or_default() {
                send!(SseEvent::default().event("log").id(pos.to_string()).data(line.replace(['\r', '\n'], " ")));
                pos += 1;
            }
            if run.status.is_final() && !running {
                send!(SseEvent::default().event("done").data(run.status.as_str()));
                return;
            }
            if started.elapsed() > MAX_STREAM {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    });

    let sse = Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)));
    // Tell reverse proxies not to hold the stream back.
    let mut resp = sse.into_response();
    resp.headers_mut().insert(HeaderName::from_static("x-accel-buffering"), HeaderValue::from_static("no"));
    resp.headers_mut().entry(header::CACHE_CONTROL).or_insert(HeaderValue::from_static("no-cache"));
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(v: Option<&str>) -> HeaderMap {
        let mut m = HeaderMap::new();
        if let Some(v) = v {
            m.insert("last-event-id", HeaderValue::from_str(v).unwrap());
        }
        m
    }
    fn q(v: Option<&str>) -> FromQuery {
        FromQuery { from: v.map(String::from) }
    }

    #[test]
    fn the_resume_position_rules() {
        assert_eq!(start_position(&h(None), &q(None)), 0);
        assert_eq!(start_position(&h(None), &q(Some("7"))), 7);
        assert_eq!(start_position(&h(Some("4")), &q(None)), 5, "the line after the last one seen");
        assert_eq!(start_position(&h(Some("4")), &q(Some("1"))), 5, "the header wins");
        assert_eq!(start_position(&h(Some("junk")), &q(None)), 0);
        assert_eq!(start_position(&h(None), &q(Some("-3"))), 0);
        assert_eq!(start_position(&h(Some(" 9 ")), &q(None)), 10);
        assert_eq!(start_position(&h(Some("18446744073709551615")), &q(None)), usize::MAX, "no overflow");
    }
}
