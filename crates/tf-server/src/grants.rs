//! Single-use transfer URLs for state: `GET` one to download a state version, `PUT` to one to upload.
//!
//! The URL itself is the credential, so it must be hard to guess (a random 256-bit token), short
//! lived (5 minutes) and used once: the first request that presents it consumes it, whatever the
//! outcome. The token is not derived from anything; the server remembers it, which is why nothing
//! can be forged and why a grant does not survive a restart or reach another instance. A client
//! that finds its URL gone just asks again.

use axum::body::{to_bytes, Body};
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use std::collections::HashMap;
use std::sync::Mutex;
use tf_core::{now, Id};
use tf_crypto::{base64_encode, md5, random_token};

/// How long a URL stays valid.
pub const GRANT_TTL_SECS: u64 = 300;
/// Most grants waiting to be used at once.
const MAX_PENDING: usize = 10_000;
/// Largest state accepted through an upload URL (the same ceiling as the CLI backend).
pub const MAX_STATE_BYTES: usize = 64 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grant {
    Download { state_version: Id },
    /// Upload the next state of a workspace. `serial` and `md5` (hex) are what the client declared.
    Upload { workspace: Id, serial: u64, md5: Option<String> },
}

#[derive(Default)]
pub struct Grants(Mutex<HashMap<String, (Grant, u64)>>);

impl Grants {
    /// A new URL token for `grant`, valid for `ttl` seconds. `None` when too many are waiting.
    pub fn issue(&self, grant: Grant, ttl: u64) -> Option<String> {
        let t = now();
        let mut m = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if m.len() >= MAX_PENDING {
            m.retain(|_, (_, exp)| *exp > t);
            if m.len() >= MAX_PENDING {
                return None;
            }
        }
        let token = random_token("grant_");
        m.insert(token.clone(), (grant, t + ttl));
        Some(token)
    }

    /// Redeems a token: it is gone afterwards, used or not. `None` when unknown, used or expired.
    pub fn take(&self, token: &str) -> Option<Grant> {
        let mut m = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let (grant, exp) = m.remove(token)?;
        (exp > now()).then_some(grant)
    }

    pub fn pending(&self) -> usize {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).len()
    }
}

fn plain(status: StatusCode, msg: &str) -> Response {
    let mut r = (status, msg.to_string()).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// The same answer for an unknown, used and expired URL, so nothing can be learned from the difference.
fn gone() -> Response {
    plain(StatusCode::NOT_FOUND, "this transfer URL is not valid (unknown, already used, or expired); ask for a new one")
}

/// `GET /archivist/v1/object/{token}`: the state document of the version the URL was issued for.
pub async fn download(State(st): State<crate::AppState>, Path(token): Path<String>) -> Response {
    let Some(Grant::Download { state_version }) = st.grants.take(&token) else { return gone() };
    match st.store.get_state_version(&state_version).await {
        Ok(sv) => {
            let mut r = (StatusCode::OK, sv.content.clone()).into_response();
            let h = r.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            if let Ok(v) = HeaderValue::from_str(&base64_encode(&md5(sv.content.as_bytes()))) {
                h.insert("content-md5", v);
            }
            r
        }
        Err(_) => gone(),
    }
}

/// `PUT /archivist/v1/object/{token}`: stores the body as the workspace's next state version, with
/// the checks the CLI backend makes (serial and lineage only move forward, md5 matches) plus: the
/// body's serial and md5 are the ones that were declared, and nothing else is writing state.
pub async fn upload(State(st): State<crate::AppState>, Path(token): Path<String>, req: Request<Body>) -> Response {
    let Some(Grant::Upload { workspace, serial, md5: declared }) = st.grants.take(&token) else { return gone() };
    let Ok(body) = to_bytes(req.into_body(), MAX_STATE_BYTES).await else {
        return plain(StatusCode::PAYLOAD_TOO_LARGE, "state is larger than 64 MB");
    };
    let ws = match st.store.get_workspace(&workspace).await {
        Ok(w) => w,
        Err(_) => return gone(),
    };
    // A run or the CLI is writing state through its own lock; a second writer would race it.
    if ws.lock.as_ref().map(|l| l.owner.starts_with("run:") || l.owner.starts_with("cli:")).unwrap_or(false) {
        return plain(StatusCode::LOCKED, "a run or the OpenTofu CLI holds this workspace's lock; upload when it is done");
    }
    if let Some(want) = declared {
        if tf_crypto::hex(&md5(&body)) != want.to_lowercase() {
            return plain(StatusCode::BAD_REQUEST, "the md5 of the body is not the one declared when the URL was requested");
        }
    }
    let body_serial = std::str::from_utf8(&body).ok().and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()).and_then(|v| v.get("serial")?.as_u64());
    if body_serial != Some(serial) {
        return plain(StatusCode::BAD_REQUEST, "the serial in the body is not the one declared when the URL was requested");
    }
    match crate::backend::store_state(&st, &ws.id, &body, None).await {
        Ok(()) => plain(StatusCode::OK, ""),
        Err((status, msg)) => plain(status, &msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grant_is_used_once() {
        let g = Grants::default();
        let t = g.issue(Grant::Download { state_version: "sv1".into() }, 60).unwrap();
        assert_eq!(g.take(&t), Some(Grant::Download { state_version: "sv1".into() }));
        assert_eq!(g.take(&t), None, "a second use finds nothing");
        assert_eq!(g.take("grant_unknown"), None);
    }

    #[test]
    fn a_grant_expires_and_is_gone_even_when_late() {
        let g = Grants::default();
        let t = g.issue(Grant::Download { state_version: "sv1".into() }, 0).unwrap();
        assert_eq!(g.take(&t), None, "zero seconds is already over");
        assert_eq!(g.pending(), 0, "an expired grant is removed when presented");
    }

    #[test]
    fn tokens_are_distinct_and_long() {
        let g = Grants::default();
        let a = g.issue(Grant::Download { state_version: "x".into() }, 60).unwrap();
        let b = g.issue(Grant::Download { state_version: "x".into() }, 60).unwrap();
        assert_ne!(a, b);
        assert!(a.len() >= 64 + 6 && a.starts_with("grant_"));
    }
}
