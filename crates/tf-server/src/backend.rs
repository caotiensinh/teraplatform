//! The OpenTofu `http` backend protocol, so `tofu init/plan/apply` can keep their
//! state here, behind this platform's accounts, roles, locking and encryption.
//!
//! Wire details follow the client in `opentofu/internal/backend/remote-state/http`:
//!
//! | request | meaning | answers |
//! | --- | --- | --- |
//! | `GET /backend/{workspace}` | pull state | 200 + body, 404 when there is none |
//! | `POST` or `PUT /backend/{workspace}?ID=<lock id>` | push state; `Content-MD5` is base64 of the md5 | 200 |
//! | `LOCK` (or `POST`/`PUT` on `/lock`) with the CLI's lock JSON | take the lock | 200, or 423 + the holder's lock JSON |
//! | `UNLOCK` (or `DELETE` on `/lock`) with the lock JSON | release it | 200, 409 when not the holder |
//! | `DELETE /backend/{workspace}` | purge state | 405: delete the workspace from the dashboard |
//!
//! Authentication is HTTP Basic (the password is an API token; the username is ignored)
//! or a bearer token. State can only be pushed while holding the CLI lock, only moves
//! forward within one lineage, and is stored sealed like state produced by runs.

use crate::AppState;
use axum::body::to_bytes;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use serde_json::{json, Value};
use std::collections::HashMap;
use tf_core::*;
use tf_crypto::{base64_decode, base64_encode, md5};
use tf_store::StoreError;

/// Large estates have large state; this is a ceiling, not a target.
const MAX_STATE_BYTES: usize = 64 << 20;
const MAX_LOCK_BODY: usize = 4096;

fn text(status: StatusCode, msg: &str) -> Response {
    (status, msg.to_string()).into_response()
}

fn json_body(status: StatusCode, body: String) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

fn store_error(e: StoreError) -> Response {
    match e {
        StoreError::NotFound(m) => text(StatusCode::NOT_FOUND, &m),
        StoreError::Forbidden(m) => text(StatusCode::FORBIDDEN, &m),
        StoreError::Conflict(m) => text(StatusCode::CONFLICT, &m),
        StoreError::Invalid(m) => text(StatusCode::BAD_REQUEST, &m),
        StoreError::Internal(_) => text(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` for a unix time, without a date library.
pub fn iso8601(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// The holder's lock as the CLI's `LockInfo` JSON (all seven keys), whoever holds it.
fn lock_json(ws: &Workspace) -> String {
    let Some(l) = &ws.lock else { return "{}".into() };
    if let Some(info) = &l.info {
        return info.clone();
    }
    let (op, who) = if l.owner.starts_with("run:") { ("run", "the platform") } else { ("manual", "a user") };
    json!({ "ID": l.owner, "Operation": op, "Info": l.reason, "Who": who, "Version": "", "Created": iso8601(l.since), "Path": "" }).to_string()
}

async fn workspace(st: &AppState, p: &Principal, name: &str, action: Action) -> Result<Workspace, Response> {
    let list = st.store.list_workspaces(&p.org_id).await.map_err(store_error)?;
    let ws = list.into_iter().find(|w| w.name == name).ok_or_else(|| text(StatusCode::NOT_FOUND, "workspace not found"))?;
    if !p.can(action) {
        return Err(text(StatusCode::FORBIDDEN, "your role or token does not allow that"));
    }
    Ok(ws)
}

struct LockRequest {
    id: String,
    reason: String,
    raw: String,
}

fn parse_lock(body: &[u8]) -> Result<LockRequest, Response> {
    let bad = |m: &str| Err(text(StatusCode::BAD_REQUEST, m));
    let Ok(raw) = std::str::from_utf8(body) else { return bad("the lock body must be UTF-8 JSON") };
    let Ok(v) = serde_json::from_str::<Value>(raw) else { return bad("the lock body must be JSON") };
    let Some(id) = v.get("ID").and_then(Value::as_str) else { return bad("the lock body needs a string ID") };
    if id.is_empty() || id.chars().count() > 128 || id.chars().any(char::is_control) {
        return bad("the lock ID must be 1 to 128 printable characters");
    }
    let field = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("unknown").chars().filter(|c| !c.is_control()).take(80).collect::<String>();
    Ok(LockRequest { id: id.to_string(), reason: format!("{} by {}", field("Operation"), field("Who")), raw: raw.to_string() })
}

/// `/backend/{workspace}`
pub async fn state(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(name): Path<String>, Query(q): Query<HashMap<String, String>>, req: Request) -> Response {
    match req.method().as_str() {
        "GET" => get_state(&st, &p, &name).await,
        "POST" | "PUT" => put_state(&st, &p, &name, &q, req).await,
        "LOCK" => lock(&st, &p, &name, req).await,
        "UNLOCK" => unlock(&st, &p, &name, req).await,
        "DELETE" => text(StatusCode::METHOD_NOT_ALLOWED, "state cannot be purged through the backend; delete the workspace from the dashboard"),
        _ => text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
    }
}

/// `/backend/{workspace}/lock`
pub async fn lock_endpoint(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(name): Path<String>, req: Request) -> Response {
    match req.method().as_str() {
        "LOCK" | "POST" | "PUT" => lock(&st, &p, &name, req).await,
        "UNLOCK" | "DELETE" => unlock(&st, &p, &name, req).await,
        _ => text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
    }
}

async fn get_state(st: &AppState, p: &Principal, name: &str) -> Response {
    let ws = match workspace(st, p, name, Action::ReadState).await {
        Ok(w) => w,
        Err(r) => return r,
    };
    match st.store.current_state(&ws.id).await {
        Ok(Some(sv)) => {
            let md5 = base64_encode(&md5(sv.content.as_bytes()));
            (StatusCode::OK, [(header::CONTENT_TYPE, "application/json".to_string()), (header::HeaderName::from_static("content-md5"), md5)], sv.content).into_response()
        }
        Ok(None) => text(StatusCode::NOT_FOUND, "no state yet"),
        Err(e) => store_error(e),
    }
}

/// `(serial, lineage)` of a Terraform-format state document.
fn state_header(doc: &str) -> Option<(u64, String)> {
    let v: Value = serde_json::from_str(doc).ok()?;
    Some((v.get("serial")?.as_u64()?, v.get("lineage")?.as_str()?.to_string()))
}

async fn put_state(st: &AppState, p: &Principal, name: &str, q: &HashMap<String, String>, req: Request) -> Response {
    let ws = match workspace(st, p, name, Action::WriteState).await {
        Ok(w) => w,
        Err(r) => return r,
    };
    let headers: HeaderMap = req.headers().clone();
    let Ok(body) = to_bytes(req.into_body(), MAX_STATE_BYTES).await else {
        return text(StatusCode::PAYLOAD_TOO_LARGE, "state is larger than 64 MB");
    };

    // the lock comes first: nothing about a state is worth saying to someone who does not hold it
    let lock_id = q.get("ID").or_else(|| q.get("id"));
    match (&ws.lock, lock_id) {
        (Some(l), Some(id)) if l.owner == format!("cli:{id}") => {}
        (Some(_), _) => return json_body(StatusCode::LOCKED, lock_json(&ws)),
        (None, _) => return text(StatusCode::LOCKED, "lock the workspace before pushing state (is lock_address set?)"),
    }

    let md5_header = headers.get("content-md5").and_then(|v| v.to_str().ok());
    match store_state(st, &ws.id, &body, md5_header).await {
        Ok(()) => text(StatusCode::OK, ""),
        Err((status, msg)) => text(status, &msg),
    }
}

fn status_of(e: &StoreError) -> StatusCode {
    match e {
        StoreError::NotFound(_) => StatusCode::NOT_FOUND,
        StoreError::Forbidden(_) => StatusCode::FORBIDDEN,
        StoreError::Conflict(_) => StatusCode::CONFLICT,
        StoreError::Invalid(_) => StatusCode::BAD_REQUEST,
        StoreError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Validates a Terraform-format state document and stores it as the workspace's next version: the
/// `Content-MD5` (base64) must match the body, the document needs a numeric serial, version and a
/// lineage, and it may only move forward within one lineage. The same state again is accepted and
/// changes nothing. Used by the CLI backend and by upload URLs; the caller checks who may write.
pub async fn store_state(st: &AppState, ws_id: &str, body: &[u8], md5_b64: Option<&str>) -> Result<(), (StatusCode, String)> {
    let bad = |m: &str| Err((StatusCode::BAD_REQUEST, m.to_string()));
    if let Some(h) = md5_b64 {
        if base64_decode(h).as_deref() != Some(&md5(body)[..]) {
            return bad("Content-MD5 does not match the body");
        }
    }
    let Ok(doc) = String::from_utf8(body.to_vec()) else { return bad("state must be UTF-8 JSON") };
    let Ok(v) = serde_json::from_str::<Value>(&doc) else { return bad("state must be JSON") };
    let (Some(serial), Some(lineage)) = (v.get("serial").and_then(Value::as_u64), v.get("lineage").and_then(Value::as_str).filter(|l| !l.is_empty())) else {
        return bad("state needs a numeric serial and a lineage");
    };
    if !v.get("version").map(Value::is_number).unwrap_or(false) {
        return bad("state needs a numeric version");
    }

    match st.store.current_state(ws_id).await {
        Ok(Some(cur)) => {
            if let Some((cur_serial, cur_lineage)) = state_header(&cur.content) {
                if cur_lineage != lineage {
                    return Err((StatusCode::CONFLICT, "lineage mismatch: this is a different state history".into()));
                }
                if serial < cur_serial {
                    return Err((StatusCode::CONFLICT, "this state is older than the one stored".into()));
                }
                if serial == cur_serial {
                    return if cur.content == doc {
                        Ok(()) // the same state again: nothing to do
                    } else {
                        Err((StatusCode::CONFLICT, "a different state already exists at this serial".into()))
                    };
                }
            }
        }
        Ok(None) => {}
        Err(e) => return Err((status_of(&e), e.to_string())),
    }
    match st.store.finalize_state(ws_id, "", &doc, tf_engine::resource_count(&doc)).await {
        Ok(_) => Ok(()),
        Err(e) => Err((status_of(&e), e.to_string())),
    }
}

async fn lock(st: &AppState, p: &Principal, name: &str, req: Request) -> Response {
    let ws = match workspace(st, p, name, Action::WriteState).await {
        Ok(w) => w,
        Err(r) => return r,
    };
    let Ok(body) = to_bytes(req.into_body(), MAX_LOCK_BODY).await else {
        return text(StatusCode::BAD_REQUEST, "the lock body is too large");
    };
    let lr = match parse_lock(&body) {
        Ok(l) => l,
        Err(r) => return r,
    };
    match st.store.lock_workspace(&ws.id, &format!("cli:{}", lr.id), &lr.reason, Some(&lr.raw), (st.cfg.cli_lock_lease_secs > 0).then_some(st.cfg.cli_lock_lease_secs)).await {
        Ok(_) => json_body(StatusCode::OK, lr.raw),
        Err(StoreError::Conflict(_)) => match st.store.get_workspace(&ws.id).await {
            Ok(cur) => json_body(StatusCode::LOCKED, lock_json(&cur)),
            Err(e) => store_error(e),
        },
        Err(e) => store_error(e),
    }
}

async fn unlock(st: &AppState, p: &Principal, name: &str, req: Request) -> Response {
    let ws = match workspace(st, p, name, Action::WriteState).await {
        Ok(w) => w,
        Err(r) => return r,
    };
    let Ok(body) = to_bytes(req.into_body(), MAX_LOCK_BODY).await else {
        return text(StatusCode::BAD_REQUEST, "the lock body is too large");
    };
    let lr = match parse_lock(&body) {
        Ok(l) => l,
        Err(r) => return r,
    };
    match st.store.unlock_workspace_owned(&ws.id, &format!("cli:{}", lr.id)).await {
        Ok(_) => text(StatusCode::OK, ""),
        Err(StoreError::Conflict(_)) => match st.store.get_workspace(&ws.id).await {
            // The CLI shows the holder's ID and advises `tofu force-unlock <ID>`. For a lock that
            // did not come from a CLI that ID is "user" (dashboard) or "run:<id>" (a run), so honour
            // it: a dashboard lock needs the unlock right, ending a run needs the admin right.
            Ok(cur) if cur.lock.as_ref().map(|l| l.owner == lr.id).unwrap_or(false) => {
                let by_run = lr.id.starts_with("run:");
                if !p.can(if by_run { Action::ForceUnlock } else { Action::Unlock }) {
                    return text(StatusCode::FORBIDDEN, "your role does not allow releasing that lock");
                }
                match st.store.unlock_workspace(&ws.id, by_run).await {
                    Ok(_) => {
                        if let Some(run) = crate::lock_holder_run(&lr.id) {
                            st.cancels.cancel(run);
                        }
                        text(StatusCode::OK, "")
                    }
                    Err(e) => store_error(e),
                }
            }
            Ok(cur) if cur.lock.is_some() => json_body(StatusCode::CONFLICT, lock_json(&cur)),
            _ => text(StatusCode::CONFLICT, "the workspace is not locked"),
        },
        Err(e) => store_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_dates() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(951_782_400), "2000-02-29T00:00:00Z", "leap day");
        assert_eq!(iso8601(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(iso8601(4_102_444_799), "2099-12-31T23:59:59Z");
    }

    #[test]
    fn state_headers() {
        assert_eq!(state_header("{\"serial\":3,\"lineage\":\"x\"}"), Some((3, "x".into())));
        assert_eq!(state_header("{\"serial\":-1,\"lineage\":\"x\"}"), None);
        assert_eq!(state_header("{\"resources\":{}}"), None, "the simulator's own format has no header");
        assert_eq!(state_header("not json"), None);
    }

    #[test]
    fn lock_bodies() {
        assert!(parse_lock(br#"{"ID":"a","Who":"x@y","Operation":"plan"}"#).is_ok());
        for bad in [&b""[..], b"x", br#"{"ID":""}"#, br#"{"ID":5}"#, br#"{"Who":"x"}"#, &[0xff, 0xfe][..]] {
            assert!(parse_lock(bad).is_err());
        }
        let long = format!("{{\"ID\":\"{}\"}}", "a".repeat(129));
        assert!(parse_lock(long.as_bytes()).is_err());
        let l = parse_lock(br#"{"ID":"a","Who":"bo\u0007ell"}"#).unwrap();
        assert_eq!(l.reason, "unknown by boell", "control characters are dropped");
    }
}
