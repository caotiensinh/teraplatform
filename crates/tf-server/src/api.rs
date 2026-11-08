//! JSON API under /api/v2, in JSON:API form (see jsonapi.rs): resource objects with
//! relationships and links, `include`, and paginated lists. Every handler checks the
//! caller's organization and role (see authz.rs): other organizations' records
//! are 404, your own organization's records beyond your role are 403.

use crate::authz::{run_access, ws_access};
use crate::jsonapi::{self as ja, Doc};
use crate::{queue_run, queue_run_on, AppState};
use tf_core::RunOptions;
use axum::extract::{FromRequest, Path, RawQuery, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use tf_core::*;
use tf_store::StoreError;

#[derive(Debug)]
pub struct ApiError(pub StoreError);

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        ApiError(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, title) = match &self.0 {
            StoreError::NotFound(m) => (StatusCode::NOT_FOUND, m.clone()),
            StoreError::Conflict(m) => (StatusCode::CONFLICT, m.clone()),
            StoreError::Invalid(m) => (StatusCode::UNPROCESSABLE_ENTITY, m.clone()),
            StoreError::Forbidden(m) => (StatusCode::FORBIDDEN, m.clone()),
            StoreError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string()),
        };
        let mut r = (status, json!({ "errors": [{ "status": status.as_u16().to_string(), "title": title }] }).to_string()).into_response();
        r.headers_mut().insert(header::CONTENT_TYPE, axum::http::HeaderValue::from_static(ja::MEDIA));
        r
    }
}

type ApiResult = Result<Doc, ApiError>;

/// `Json`, but a malformed, mistyped or oversized body is answered with the API's
/// own error envelope instead of axum's plain text.
pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    T: serde::de::DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(ApiJson(v)),
            Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => Err(StoreError::Invalid("the request body is too large".into()).into()),
            Err(e) => Err(StoreError::Invalid(format!("invalid request body: {}", e.body_text())).into()),
        }
    }
}

// ---------- resources ----------

const WS: &str = "/api/v2/workspaces";
const RUNS: &str = "/api/v2/runs";

fn ws_res(w: &Workspace) -> Value {
    let r = ja::resource(
        "workspaces",
        w,
        &["org", "current_run", "current_state_version"],
        &[("organization", "organizations", Some(&w.org)), ("current-run", "runs", w.current_run.as_deref()), ("current-state-version", "state-versions", w.current_state_version.as_deref())],
        WS,
    );
    ja::with_attr(r, "locked", json!(w.lock.is_some()))
}

fn run_res(r: &Run) -> Value {
    let res = ja::resource(
        "runs",
        r,
        &["workspace_id", "config_version_id"],
        &[("workspace", "workspaces", Some(&r.workspace_id)), ("configuration-version", "configuration-versions", Some(&r.config_version_id))],
        RUNS,
    );
    ja::with_attr(res, "actions", json!(actions(r.status)))
}

/// A state version. The state document itself is never in the resource (it holds secrets and can be
/// large): `with_download` adds a single-use URL to fetch it from.
fn state_res(st: &AppState, sv: &StateVersion, with_download: bool) -> Result<Value, ApiError> {
    let r = ja::resource(
        "state-versions",
        sv,
        &["workspace_id", "run_id", "content"],
        &[("workspace", "workspaces", Some(&sv.workspace_id)), ("run", "runs", sv.run_id.as_deref())],
        "/api/v2/state-versions",
    );
    if !with_download {
        return Ok(r);
    }
    let token = st
        .grants
        .issue(crate::grants::Grant::Download { state_version: sv.id.clone() }, crate::grants::GRANT_TTL_SECS)
        .ok_or_else(|| StoreError::Conflict("too many transfers are waiting; retry shortly".into()))?;
    Ok(ja::with_attr(r, "hosted-state-download-url", json!(format!("{}/archivist/v1/object/{token}", st.cfg.base_url.trim_end_matches('/')))))
}

fn var_res(v: &Variable) -> Value {
    ja::resource("vars", v, &["workspace_id"], &[("workspace", "workspaces", Some(&v.workspace_id))], "/api/v2/vars")
}

fn cv_res(cv: &ConfigVersion) -> Value {
    let r = ja::resource(
        "configuration-versions",
        cv,
        &["workspace_id", "text"],
        &[("workspace", "workspaces", Some(&cv.workspace_id))],
        "/api/v2/configuration-versions",
    );
    if cv.status == "pending" {
        let up = format!("/api/v2/configuration-versions/{}/upload", cv.id);
        let mut r = ja::with_attr(r, "upload-url", json!(up));
        r["links"]["upload"] = json!(up);
        r
    } else {
        r
    }
}

fn denied() -> ApiError {
    StoreError::Forbidden("your role does not allow that".into()).into()
}

// ---------- workspaces ----------

pub async fn list_workspaces(State(st): State<AppState>, Extension(p): Extension<Principal>, RawQuery(q): RawQuery) -> ApiResult {
    if !p.can(Action::Read) {
        return Err(denied());
    }
    let q = ja::parse_query(q.as_deref(), &["current-run"])?;
    let all = st.store.list_workspaces(&p.org_id).await?;
    // Only the page being returned needs its includes.
    let page: Vec<&Workspace> = all.iter().skip((q.page.number - 1).saturating_mul(q.page.size)).take(q.page.size).collect();
    let mut included = Vec::new();
    if q.wants("current-run") {
        for w in page {
            if let Some(r) = &w.current_run {
                included.push(run_res(&st.store.get_run(r).await?));
            }
        }
    }
    Ok(ja::ok(ja::collection(WS, &q, all.iter().map(ws_res).collect(), included)))
}

pub async fn get_workspace(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>, RawQuery(q): RawQuery) -> ApiResult {
    let q = ja::parse_query(q.as_deref(), &["current-run"])?;
    let ws = ws_access(&st, &p, &id, Action::Read).await?;
    let mut included = Vec::new();
    if q.wants("current-run") {
        if let Some(r) = &ws.current_run {
            included.push(run_res(&st.store.get_run(r).await?));
        }
    }
    Ok(ja::ok(ja::single(ws_res(&ws), included)))
}

#[derive(Deserialize)]
pub struct WorkspaceChanges {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub auto_apply: Option<bool>,
    /// Seconds between scheduled drift checks; `null` turns them off; left out changes nothing.
    #[serde(default, deserialize_with = "double_option")]
    pub drift_interval: Option<Option<u64>>,
}

/// Tells a missing field (`None`) from an explicit `null` (`Some(None)`).
fn double_option<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<u64>>, D::Error> {
    Ok(Some(Option::<u64>::deserialize(d)?))
}

pub async fn update_workspace(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>, ApiJson(body): ApiJson<Value>) -> ApiResult {
    ws_access(&st, &p, &id, Action::UpdateWorkspace).await?;
    let b: WorkspaceChanges = ja::attributes(body, "workspaces")?;
    if let Some(Some(secs)) = b.drift_interval {
        if secs < st.cfg.min_drift_interval_secs {
            return Err(StoreError::Invalid(format!("drift checks can run at most every {} seconds", st.cfg.min_drift_interval_secs)).into());
        }
    }
    let ws = st.store.update_workspace(&id, WorkspacePatch { name: b.name, description: b.description, auto_apply: b.auto_apply, drift_interval: b.drift_interval }).await?;
    Ok(ja::ok(ja::single(ws_res(&ws), vec![])))
}

/// Deletes the workspace with its runs, variables and state. The infrastructure it describes is not
/// touched (destroy it first). Refused while locked or while a run is working.
pub async fn delete_workspace(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    ws_access(&st, &p, &id, Action::DeleteWorkspace).await?;
    st.store.delete_workspace(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct NewWorkspace {
    pub name: String,
}

pub async fn create_workspace(State(st): State<AppState>, Extension(p): Extension<Principal>, ApiJson(body): ApiJson<Value>) -> ApiResult {
    if !p.can(Action::CreateWorkspace) {
        return Err(denied());
    }
    let b: NewWorkspace = ja::attributes(body, "workspaces")?;
    let ws = st.store.create_workspace(&p.org_id, &b.name).await?;
    Ok(ja::created(ja::single(ws_res(&ws), vec![])))
}

// ---------- runs ----------

#[derive(Deserialize)]
pub struct NewRun {
    /// The configuration text; or name an uploaded version with `configuration_version_id`.
    #[serde(default)]
    pub config: Option<String>,
    #[serde(default)]
    pub configuration_version_id: Option<String>,
    #[serde(default)]
    pub message: String,
    /// "plan_and_apply" (default) or "plan_only"
    #[serde(default)]
    pub operation: Option<String>,
    /// Left out: the workspace's setting decides.
    #[serde(default)]
    pub auto_apply: Option<bool>,
    /// `false` plans without refreshing the state first (faster, blind to drift). Default `true`.
    #[serde(default)]
    pub refresh: Option<bool>,
}

pub async fn create_run(
    State(st): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<Value>,
) -> ApiResult {
    let workspace = ws_access(&st, &p, &ws, Action::QueueRun).await?;
    let b: NewRun = ja::attributes(body, "runs")?;
    let auto_apply = b.auto_apply.unwrap_or(workspace.auto_apply);
    let op = match b.operation.as_deref() {
        None | Some("plan_and_apply") => Operation::PlanAndApply,
        Some("plan_only") | Some("refresh_only") => Operation::PlanOnly,
        Some(o) => return Err(StoreError::Invalid(format!("unknown operation {o}")).into()),
    };
    let opts = RunOptions { skip_refresh: b.refresh == Some(false), refresh_only: b.operation.as_deref() == Some("refresh_only"), vcs: None };
    let run = match (&b.config, &b.configuration_version_id) {
        (Some(c), None) => queue_run(&st.store, &ws, c, &b.message, op, auto_apply, "api", opts).await?,
        (None, Some(cv)) => queue_run_on(&st.store, &ws, cv, &b.message, op, auto_apply, "api", opts).await?,
        (None, None) => return Err(StoreError::Invalid("give the configuration: either config, or a configuration-version relationship".into()).into()),
        (Some(_), Some(_)) => return Err(StoreError::Invalid("give either config or a configuration version, not both".into()).into()),
    };
    Ok(ja::created(ja::single(run_res(&run), vec![])))
}

pub async fn list_runs_in_workspace(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(ws): Path<String>, RawQuery(q): RawQuery) -> ApiResult {
    let q = ja::parse_query(q.as_deref(), &[])?;
    ws_access(&st, &p, &ws, Action::Read).await?;
    let mut runs = st.store.list_runs(Some(&ws)).await?;
    runs.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| b.id.cmp(&a.id)));
    Ok(ja::ok(ja::collection(&format!("{WS}/{ws}/runs"), &q, runs.iter().map(run_res).collect(), vec![])))
}

pub async fn list_runs(State(st): State<AppState>, Extension(p): Extension<Principal>, RawQuery(q): RawQuery) -> ApiResult {
    if !p.can(Action::Read) {
        return Err(denied());
    }
    let q = ja::parse_query(q.as_deref(), &[])?;
    let mut runs = st.store.list_runs_in_org(&p.org_id).await?;
    runs.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| b.id.cmp(&a.id)));
    Ok(ja::ok(ja::collection(RUNS, &q, runs.iter().map(run_res).collect(), vec![])))
}

pub async fn get_run(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>, RawQuery(q): RawQuery) -> ApiResult {
    let q = ja::parse_query(q.as_deref(), &["workspace"])?;
    let (run, ws) = run_access(&st, &p, &id, Action::Read).await?;
    let included = if q.wants("workspace") { vec![ws_res(&ws)] } else { vec![] };
    Ok(ja::ok(ja::single(run_res(&run), included)))
}

pub async fn run_action(
    State(st): State<AppState>,
    Extension(p): Extension<Principal>,
    Path((id, action)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> ApiResult {
    let (ev, need) = match action.as_str() {
        "apply" => (Event::Confirm, Action::ConfirmRun),
        "discard" => (Event::Discard, Action::ConfirmRun),
        "cancel" => (Event::Cancel, Action::ConfirmRun),
        "force-cancel" => (Event::ForceCancel, Action::ForceCancel),
        _ => return Err(StoreError::NotFound(format!("unknown action {action}")).into()),
    };
    let (run, _) = run_access(&st, &p, &id, need).await?;
    if ev == Event::ForceCancel && run.status == RunStatus::Applying && !acknowledged(&body)? {
        return Err(StoreError::Invalid(
            "stopping a run that is applying can leave resources that exist but are not recorded; send {\"acknowledge_risk\": true} to go ahead".into(),
        )
        .into());
    }
    let run = crate::run_event(&st, &run, ev).await?;
    // Accepted: applying happens asynchronously in the runner.
    Ok(ja::accepted(ja::single(run_res(&run), vec![])))
}

/// `{"acknowledge_risk": true}` in the body. No body, or anything but the boolean `true`, is
/// not an acknowledgement; a body that is not JSON is an error.
fn acknowledged(body: &[u8]) -> Result<bool, ApiError> {
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(false);
    }
    let v: Value = serde_json::from_slice(body).map_err(|e| StoreError::Invalid(format!("invalid request body: {e}")))?;
    Ok(v.get("acknowledge_risk") == Some(&Value::Bool(true)))
}

pub async fn run_logs(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>) -> Result<Response, ApiError> {
    run_access(&st, &p, &id, Action::Read).await?;
    let text = st.store.get_log(&id).await?.join("\n");
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
}

// ---------- state ----------

pub async fn current_state(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(ws): Path<String>) -> ApiResult {
    ws_access(&st, &p, &ws, Action::ReadState).await?;
    match st.store.current_state(&ws).await? {
        Some(sv) => Ok(ja::ok(ja::single(state_res(&st, &sv, true)?, vec![]))),
        None => Err(StoreError::NotFound("workspace has no state yet".into()).into()),
    }
}

/// The state history, newest first, without the state documents themselves (those are large and
/// hold secrets; fetch one version at a time).
pub async fn list_states(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(ws): Path<String>, RawQuery(q): RawQuery) -> ApiResult {
    let q = ja::parse_query(q.as_deref(), &[])?;
    ws_access(&st, &p, &ws, Action::ReadState).await?;
    let mut all = st.store.list_states(&ws).await?;
    all.sort_by(|a, b| b.serial.cmp(&a.serial));
    Ok(ja::ok(ja::collection(&format!("{WS}/{ws}/state-versions"), &q, all.iter().map(|s| state_res(&st, s, false)).collect::<Result<Vec<_>, _>>()?, vec![])))
}

pub async fn get_state_version(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>) -> ApiResult {
    let sv = st.store.get_state_version(&id).await?;
    ws_access(&st, &p, &sv.workspace_id, Action::ReadState).await.map_err(|e| match e {
        StoreError::NotFound(_) => StoreError::NotFound("state version not found".into()),
        other => other,
    })?;
    Ok(ja::ok(ja::single(state_res(&st, &sv, true)?, vec![])))
}

#[derive(Deserialize)]
pub struct NewStateUpload {
    pub serial: u64,
    /// Hex md5 of the state document, optional; checked against the body when given.
    #[serde(default)]
    pub md5: Option<String>,
}

/// Step one of a state upload: declare the serial (and optionally the md5). The answer holds a
/// single-use URL, valid for five minutes, to `PUT` the state document to. The workspace must not be
/// locked by a run or the CLI. The state only moves forward within its lineage (same rules as the CLI).
pub async fn create_state_upload(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(ws): Path<String>, ApiJson(body): ApiJson<Value>) -> Result<ja::Doc, ApiError> {
    let workspace = ws_access(&st, &p, &ws, Action::WriteState).await?;
    let b: NewStateUpload = ja::attributes(body, "state-versions")?;
    if let Some(m) = &b.md5 {
        if m.len() != 32 || !m.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(StoreError::Invalid("md5 must be 32 hexadecimal characters".into()).into());
        }
    }
    if workspace.lock.as_ref().map(|l| l.owner.starts_with("run:") || l.owner.starts_with("cli:")).unwrap_or(false) {
        return Err(StoreError::Conflict("a run or the OpenTofu CLI holds this workspace's lock; upload when it is done".into()).into());
    }
    let token = st
        .grants
        .issue(crate::grants::Grant::Upload { workspace: ws.clone(), serial: b.serial, md5: b.md5.clone() }, crate::grants::GRANT_TTL_SECS)
        .ok_or_else(|| StoreError::Conflict("too many transfers are waiting; retry shortly".into()))?;
    let url = format!("{}/archivist/v1/object/{token}", st.cfg.base_url.trim_end_matches('/'));
    Ok(ja::created(json!({ "data": {
        "id": format!("upload-{}-{}", ws, b.serial),
        "type": "state-version-uploads",
        "attributes": { "serial": b.serial, "md5": b.md5, "hosted-state-upload-url": url, "expires-in": crate::grants::GRANT_TTL_SECS },
        "relationships": { "workspace": ja::rel("workspaces", Some(&ws)) },
    } })))
}

// ---------- workspace actions ----------

/// Longest lease a lock may ask for: a week.
const MAX_LEASE_SECS: u64 = 7 * 86_400;

/// `{"lease_seconds": N}` from an action body. Left out: the server's default. 0: no expiry.
fn lease_from(body: &[u8], default: u64) -> Result<Option<u64>, ApiError> {
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok((default > 0).then_some(default));
    }
    let v: Value = serde_json::from_slice(body).map_err(|e| StoreError::Invalid(format!("invalid request body: {e}")))?;
    let v = v.get("data").and_then(|d| d.get("attributes")).unwrap_or(&v);
    match v.get("lease_seconds").or_else(|| v.get("lease-seconds")) {
        None | Some(Value::Null) => Ok((default > 0).then_some(default)),
        Some(n) => match n.as_u64() {
            Some(0) => Ok(None),
            Some(n) if n <= MAX_LEASE_SECS => Ok(Some(n)),
            _ => Err(StoreError::Invalid(format!("lease_seconds must be a whole number from 0 (no expiry) to {MAX_LEASE_SECS}")).into()),
        },
    }
}

pub async fn workspace_action(
    State(st): State<AppState>,
    Extension(p): Extension<Principal>,
    Path((id, action)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Result<Doc, ApiError> {
    if action == "check-drift" {
        ws_access(&st, &p, &id, Action::QueueRun).await?;
        let run = crate::drift::queue_check(&st.store, &id, "api").await?;
        return Ok(ja::accepted(ja::single(run_res(&run), vec![])));
    }
    let need = match action.as_str() {
        "lock" | "heartbeat" => Action::Lock,
        "unlock" => Action::Unlock,
        "force-unlock" => Action::ForceUnlock,
        _ => return Err(StoreError::NotFound(format!("unknown action {action}")).into()),
    };
    let before = ws_access(&st, &p, &id, need).await?;
    let ws = match action.as_str() {
        "lock" => {
            let lease = lease_from(&body, st.cfg.api_lock_lease_secs)?;
            st.store.lock_workspace(&id, "user", "locked via API", None, lease).await?
        }
        "heartbeat" => {
            let lease = lease_from(&body, st.cfg.api_lock_lease_secs)?.ok_or_else(|| StoreError::Invalid("a heartbeat needs a lease: give lease_seconds above 0".into()))?;
            st.store.heartbeat_lock(&id, "user", lease).await?
        }
        "unlock" => st.store.unlock_workspace(&id, false).await?,
        _ => {
            let ws = st.store.unlock_workspace(&id, true).await?;
            // The run that held the lock was ended; stop its engine step too.
            if let Some(run) = before.lock.as_ref().and_then(|l| crate::lock_holder_run(&l.owner)) {
                st.cancels.cancel(run);
            }
            ws
        }
    };
    Ok(ja::ok(ja::single(ws_res(&ws), vec![])))
}

// ---------- variables ----------

pub async fn list_vars(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(ws): Path<String>, RawQuery(q): RawQuery) -> ApiResult {
    let q = ja::parse_query(q.as_deref(), &[])?;
    ws_access(&st, &p, &ws, Action::Read).await?;
    let vars = st.store.list_variables(&ws).await?;
    Ok(ja::ok(ja::collection(&format!("{WS}/{ws}/vars"), &q, vars.iter().map(var_res).collect(), vec![])))
}

#[derive(Deserialize)]
pub struct NewVar {
    pub key: String,
    #[serde(default = "terraform")]
    pub category: String,
    pub value: String,
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default)]
    pub hcl: bool,
    #[serde(default)]
    pub description: String,
}

fn terraform() -> String {
    "terraform".into()
}

pub async fn set_var(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(ws): Path<String>, ApiJson(body): ApiJson<Value>) -> ApiResult {
    ws_access(&st, &p, &ws, Action::ManageVariables).await?;
    let b: NewVar = ja::attributes(body, "vars")?;
    let v = st
        .store
        .set_variable(&ws, NewVariable { key: b.key, category: b.category, hcl: b.hcl, sensitive: b.sensitive, value: b.value, description: b.description })
        .await?;
    Ok(ja::created(ja::single(var_res(&v), vec![])))
}

pub async fn delete_var(
    State(st): State<AppState>,
    Extension(p): Extension<Principal>,
    Path((ws, var_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    ws_access(&st, &p, &ws, Action::ManageVariables).await?;
    st.store.delete_variable(&ws, &var_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------- configuration versions (the archive upload) ----------

/// Step one: a configuration version that waits for its archive. The answer names where to PUT it.
pub async fn create_config_version(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(ws): Path<String>) -> ApiResult {
    ws_access(&st, &p, &ws, Action::QueueRun).await?;
    let cv = st.store.create_pending_config(&ws, "api-upload").await?;
    Ok(ja::created(ja::single(cv_res(&cv), vec![])))
}

pub async fn get_config_version(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>) -> ApiResult {
    let cv = st.store.get_config_version(&id).await?;
    ws_access(&st, &p, &cv.workspace_id, Action::Read).await.map_err(|e| match e {
        StoreError::NotFound(_) => StoreError::NotFound("configuration version not found".into()),
        other => other,
    })?;
    Ok(ja::ok(ja::single(cv_res(&cv), vec![])))
}

/// Largest upload accepted, as the compressed archive.
pub const MAX_UPLOAD_BYTES: usize = 5_000_000;

/// Step two: the `.tar.gz` as the request body. Once uploaded, a version cannot be replaced.
pub async fn upload_config_version(
    State(st): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult {
    let cv = st.store.get_config_version(&id).await?;
    ws_access(&st, &p, &cv.workspace_id, Action::QueueRun).await.map_err(|e| match e {
        StoreError::NotFound(_) => StoreError::NotFound("configuration version not found".into()),
        other => other,
    })?;
    if cv.status != "pending" {
        return Err(StoreError::Conflict("this configuration version already has its content".into()).into());
    }
    let text = crate::archive::extract_config(&body, crate::MAX_CONFIG_BYTES).map_err(StoreError::Invalid)?;
    let cv = st.store.upload_config(&id, &text).await?;
    Ok(ja::ok(ja::single(cv_res(&cv), vec![])))
}

// ---------- repository connection ----------

#[derive(Deserialize)]
pub struct VcsBody {
    pub repo: String,
    #[serde(default = "main_branch")]
    pub branch: String,
    #[serde(default)]
    pub working_directory: String,
    /// The access token used to download the repository and comment on pull requests. Left out: the
    /// one already stored stays.
    #[serde(default)]
    pub token: Option<String>,
}

fn main_branch() -> String {
    "main".into()
}

fn vcs_res(st: &AppState, link: &VcsLink) -> Value {
    let r = ja::resource("vcs-connections", link, &["workspace_id", "working_dir"], &[("workspace", "workspaces", Some(&link.workspace_id))], "/api/v2/workspaces");
    let r = ja::with_attr(r, "working-directory", json!(link.working_dir));
    let r = ja::with_attr(r, "webhook-url", json!(format!("{}/webhooks/github/{}", st.cfg.base_url.trim_end_matches('/'), link.workspace_id)));
    let mut r = r;
    r["id"] = json!(link.workspace_id);
    r["links"]["self"] = json!(format!("/api/v2/workspaces/{}/vcs", link.workspace_id));
    r
}

/// Connects the workspace to a repository. The answer carries the webhook secret **once**
/// (`meta.webhook-secret`); connecting again makes a new one.
pub async fn put_vcs(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>, ApiJson(body): ApiJson<Value>) -> Result<Doc, ApiError> {
    ws_access(&st, &p, &id, Action::UpdateWorkspace).await?;
    let b: VcsBody = ja::attributes(body, "vcs-connections")?;
    let (link, secret) = st.store.set_vcs_link(&id, &b.repo, &b.branch, &b.working_directory, b.token.as_deref()).await?;
    let mut doc = ja::single(vcs_res(&st, &link), vec![]);
    doc["meta"] = json!({ "webhook-secret": secret });
    Ok(ja::ok(doc))
}

pub async fn get_vcs(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>) -> ApiResult {
    ws_access(&st, &p, &id, Action::Read).await?;
    match st.store.get_vcs_link(&id).await? {
        Some(link) => Ok(ja::ok(ja::single(vcs_res(&st, &link), vec![]))),
        None => Err(StoreError::NotFound("this workspace is not connected to a repository".into()).into()),
    }
}

pub async fn delete_vcs(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    ws_access(&st, &p, &id, Action::UpdateWorkspace).await?;
    st.store.delete_vcs_link(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------- rate limit ----------

/// Per caller: at most `Config.api_rate_per_sec` requests in any one-second window across /api/.
/// A user (session or user token) is counted on its own; organization tokens share their
/// organization's allowance. Requests with no valid credentials are refused cheaply before this.
pub async fn rate_limit(State(st): State<AppState>, req: Request, next: axum::middleware::Next) -> Response {
    let limit = st.cfg.api_rate_per_sec;
    if limit == 0 || !req.uri().path().starts_with("/api/") {
        return next.run(req).await;
    }
    let Some(p) = req.extensions().get::<Principal>().cloned() else { return next.run(req).await };
    let key = format!("api:{}:{}", p.org_id, p.user_id.as_deref().unwrap_or("org-token"));
    if st.limiter.hit(&key, limit, 1) {
        let mut res = next.run(req).await;
        res.headers_mut().insert("x-ratelimit-limit", HeaderValue::from(limit));
        return res;
    }
    let mut res = (StatusCode::TOO_MANY_REQUESTS, json!({ "errors": [{ "status": "429", "title": format!("rate limit exceeded: {limit} requests per second; retry in a second") }] }).to_string()).into_response();
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(ja::MEDIA));
    res.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    res.headers_mut().insert("x-ratelimit-limit", HeaderValue::from(limit));
    res
}
