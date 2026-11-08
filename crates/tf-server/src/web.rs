//! Server-rendered pages. Markup uses only the classes in replica/design/tokens.css.
//! Every dynamic string goes through `esc`. Every handler authorises through
//! `authz` before touching a record.

use crate::authz::{run_access, ws_access};
use crate::{queue_run, run_event, AppState};
use tf_core::RunOptions;
use axum::body::Bytes;
use axum::extract::{Form, Path, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Extension;
use serde::Deserialize;
use std::collections::HashMap;
use tf_core::*;
use tf_store::StoreError;

const CSS: &str = include_str!("../../../replica/design/tokens.css");

const EXTRA_CSS: &str = "form.inline{display:inline;margin:0}\
.mono{overflow-wrap:anywhere}\
textarea.code{width:100%;min-height:200px;padding:8px;font:12.5px/19px var(--font-mono);background:var(--color-bg);color:var(--color-text);border:1px solid var(--color-border-input);border-radius:var(--radius-md)}\
.form-row{display:flex;gap:8px;align-items:center;flex-wrap:wrap}\
.empty{padding:32px;text-align:center;color:var(--color-text-muted)}\
.err{margin-bottom:16px}\
select{height:var(--control);border:1px solid var(--color-border-input);border-radius:var(--radius-md);background:var(--color-bg);color:var(--color-text);padding:0 8px}\
h2.sec{font:var(--text-lg);margin:20px 0 8px}\
.public{max-width:520px;margin:0 auto;padding:24px}.stepper .dur{color:var(--color-text-muted);font-size:12px;margin-left:6px}.risk{display:flex;gap:8px;align-items:center;flex-wrap:wrap}";

const SAMPLE_CONFIG: &str = "resource \"aws_s3_bucket\" \"logs\" {\n  bucket = \"acme-logs\"\n}\n\nresource \"aws_iam_role\" \"ci\" {\n  name = \"ci-runner\"\n}\n";

// ---------- errors ----------

pub struct WebError(StoreError);

impl From<StoreError> for WebError {
    fn from(e: StoreError) -> Self {
        WebError(e)
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let (status, title, detail) = match &self.0 {
            StoreError::NotFound(m) => (StatusCode::NOT_FOUND, "Not found", m.clone()),
            StoreError::Conflict(m) => (StatusCode::CONFLICT, "That action is not possible right now", m.clone()),
            StoreError::Invalid(m) => (StatusCode::UNPROCESSABLE_ENTITY, "Invalid input", m.clone()),
            StoreError::Forbidden(m) => (StatusCode::FORBIDDEN, "Not allowed", m.clone()),
            StoreError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong", "The error was logged. Try again.".to_string()),
        };
        let body = format!(
            "<div class=\"page-header\"><h1>{}</h1></div><div class=\"banner banner-danger\" role=\"alert\">{}</div><p><a href=\"/workspaces\">Back to workspaces</a></p>",
            esc(title),
            esc(&detail)
        );
        (status, Html(layout_public(title, &body))).into_response()
    }
}

type Page = Result<Html<String>, WebError>;

// ---------- helpers ----------

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

fn ago(ts: u64) -> String {
    let d = now().saturating_sub(ts);
    match d {
        0..=4 => "just now".into(),
        5..=59 => format!("{d}s ago"),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86399 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

/// The drift stat of a workspace: when it was last checked and what was found.
fn drift_text(ws: &Workspace) -> String {
    match &ws.drift {
        None => match ws.drift_interval {
            Some(_) => "scheduled".into(),
            None => "never checked".into(),
        },
        Some(d) => {
            let what = match d.status.as_str() {
                "clean" => "none".to_string(),
                "drifted" => format!("{} changed", d.changes),
                _ => "check failed".to_string(),
            };
            format!("{what} <span class=\"muted\" style=\"font-size:12px\">{}</span>", ago(d.checked_at))
        }
    }
}

/// "in 5m", "in 3h": how long until a time, for lock leases.
fn until(ts: u64) -> String {
    let d = ts.saturating_sub(now());
    match d {
        0..=59 => "within a minute".into(),
        60..=3599 => format!("in {}m", d / 60),
        3600..=86399 => format!("in {}h", d / 3600),
        _ => format!("in {}d", d / 86400),
    }
}

/// How a page keeps itself current.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    No,
    /// Reload every 2 seconds.
    Meta,
    /// Reload every 2 seconds only when scripts are off (the page follows the run by stream).
    NoScript,
}

impl From<bool> for Refresh {
    fn from(b: bool) -> Self {
        if b {
            Refresh::Meta
        } else {
            Refresh::No
        }
    }
}

pub fn badge(s: RunStatus) -> String {
    let (class, icon) = match s {
        RunStatus::Pending => ("neutral", "…"),
        RunStatus::Planning | RunStatus::ApplyQueued | RunStatus::Applying => ("info", "◐"),
        RunStatus::NeedsConfirmation => ("warning", "✋"),
        RunStatus::Applied | RunStatus::PlannedAndFinished => ("success", "✓"),
        RunStatus::Errored => ("danger", "✕"),
        RunStatus::Discarded | RunStatus::Canceled => ("neutral", "∅"),
        RunStatus::ForceCanceled => ("danger", "⊘"),
    };
    format!("<span class=\"badge badge-{class}\" data-icon=\"{icon}\">{}</span>", s.label())
}

fn summary_text(s: &Option<PlanSummary>) -> String {
    match s {
        Some(p) if p.has_changes() && p.outputs > 0 => format!("+{} ~{} -{} outputs {}", p.add, p.change, p.destroy, p.outputs),
        Some(p) if p.has_changes() => format!("+{} ~{} -{}", p.add, p.change, p.destroy),
        Some(_) => "no changes".into(),
        None => "-".into(),
    }
}

fn run_ref(r: &Run) -> String {
    format!("<a class=\"mono\" href=\"/runs/{}\">run-{}</a>", esc(&r.id), esc(short(&r.id)))
}

fn head(title: &str, refresh: Refresh) -> String {
    let mut h = String::from("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">");
    match refresh {
        Refresh::Meta => h.push_str("<meta http-equiv=\"refresh\" content=\"2\">"),
        Refresh::NoScript => h.push_str("<noscript><meta http-equiv=\"refresh\" content=\"2\"></noscript>"),
        Refresh::No => {}
    }
    h.push_str(&format!("<title>{} · Tera Platform</title>", esc(title)));
    h.push_str("<link rel=\"stylesheet\" href=\"/assets/app.css\"><style>");
    h.push_str(EXTRA_CSS);
    h.push_str("</style><script>try{var t=localStorage.getItem('theme');if(t)document.documentElement.dataset.theme=t}catch(e){}</script></head><body>");
    h
}

const THEME_BUTTON: &str = "<button class=\"btn btn-ghost btn-sm\" type=\"button\" onclick=\"var r=document.documentElement;var n=r.dataset.theme==='dark'?'light':'dark';r.dataset.theme=n;try{localStorage.setItem('theme',n)}catch(e){}\">Theme</button>";

/// A page with no navigation, for sign-in, sign-up and errors.
pub fn layout_public(title: &str, body: &str) -> String {
    let mut h = head(title, Refresh::No);
    h.push_str("<div class=\"public\"><div class=\"brand\" style=\"margin-bottom:8px\"><i></i>Tera Platform</div>");
    h.push_str(body);
    h.push_str("</div></body></html>");
    h
}

#[allow(clippy::too_many_arguments)]
fn layout(title: &str, active: &str, need: usize, body: &str, refresh: Refresh, engine: &str, user: &str, org: &str) -> String {
    let nav = |href: &str, key: &str, label: &str, count: String| {
        let cur = if key == active { " aria-current=\"page\"" } else { "" };
        format!("<a class=\"nav-item\" href=\"{href}\"{cur}>{label}<span class=\"nav-count\">{count}</span></a>")
    };
    let need_text = if need > 0 { format!("{need} need you") } else { String::new() };
    let mut h = head(title, refresh);
    h.push_str("<div class=\"shell\"><header class=\"topbar\"><span class=\"brand\"><i></i>Tera Platform</span>");
    h.push_str(&format!("<span class=\"org-switch\">{}</span>", esc(org)));
    if !engine.is_empty() {
        h.push_str(&format!("<span class=\"chip hide-sm\" title=\"Engine behind plan and apply\">engine: {}</span>", esc(engine)));
    }
    h.push_str(&format!(
        "<span style=\"margin-left:auto;display:flex;gap:8px;align-items:center\"><span class=\"muted hide-sm\">{}</span>{THEME_BUTTON}\
<form class=\"inline\" method=\"post\" action=\"/logout\"><button class=\"btn btn-secondary btn-sm\" type=\"submit\">Sign out</button></form></span></header>",
        esc(user)
    ));
    h.push_str("<nav class=\"sidebar\" aria-label=\"Main\">");
    h.push_str(&nav("/workspaces", "workspaces", "Workspaces", String::new()));
    h.push_str(&nav("/runs", "runs", "Runs", need_text));
    h.push_str("<div class=\"nav-group\">You</div>");
    h.push_str(&nav("/tokens", "tokens", "API tokens", String::new()));
    h.push_str(&nav("/account", "account", "Account", String::new()));
    h.push_str("</nav><main class=\"content\">");
    h.push_str(body);
    h.push_str("</main></div></body></html>");
    h
}

pub async fn page(st: &AppState, p: &Principal, title: &str, active: &str, body: String, refresh: impl Into<Refresh>) -> Page {
    let need = st.store.list_runs_in_org(&p.org_id).await?.iter().filter(|r| r.status == RunStatus::NeedsConfirmation).count();
    let user = match &p.user_id {
        Some(u) => st.store.get_user(u).await.map(|u| u.email).unwrap_or_default(),
        None => "organization token".to_string(),
    };
    let org = st.store.orgs_for_user(p.user_id.as_deref().unwrap_or("")).await.ok().and_then(|v| v.into_iter().find(|(o, _)| o.id == p.org_id)).map(|(o, _)| o.name).unwrap_or_default();
    Ok(Html(layout(title, active, need, &body, refresh.into(), st.engine_name, &user, &org)))
}

// ---------- pages ----------

pub async fn home() -> Redirect {
    Redirect::to("/workspaces")
}

pub async fn healthz() -> &'static str {
    "ok"
}

pub async fn css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], CSS)
}

async fn workspaces_view(st: &AppState, p: &Principal, error: Option<String>, name: &str) -> Page {
    let list = st.store.list_workspaces(&p.org_id).await?;
    let mut rows = String::new();
    for w in &list {
        let latest = st.store.list_runs(Some(&w.id)).await?.into_iter().next();
        let badge_html = latest.as_ref().map(|r| badge(r.status)).unwrap_or_else(|| "<span class=\"badge badge-neutral\" data-icon=\"∅\">No runs yet</span>".into());
        let when = latest.as_ref().map(|r| ago(r.updated_at)).unwrap_or_else(|| "never".into());
        let lock = if w.lock.is_some() { " <span class=\"chip\">locked</span>" } else { "" };
        rows.push_str(&format!(
            "<tr><td><a class=\"mono\" href=\"/workspaces/{}\">{}</a>{lock}</td><td>{badge_html}</td><td>{}</td><td class=\"muted\">{when}</td></tr>",
            esc(&w.id), esc(&w.name), w.resource_count
        ));
    }
    let table = if list.is_empty() {
        "<div class=\"empty\">No workspaces yet. Create one to queue your first run.</div>".to_string()
    } else {
        format!("<div class=\"table-wrap\"><table class=\"table\"><thead><tr><th>Workspace</th><th>Current run</th><th>Resources</th><th>Last change</th></tr></thead><tbody>{rows}</tbody></table></div>")
    };
    let err = error.map(|e| format!("<div class=\"banner banner-danger err\" role=\"alert\">{}</div>", esc(&e))).unwrap_or_default();
    let create = if p.can(Action::CreateWorkspace) {
        format!(
            "<form method=\"post\" action=\"/workspaces\" class=\"toolbar\"><div class=\"field\"><label class=\"sr-only\" for=\"name\">Workspace name</label>\
<input id=\"name\" name=\"name\" value=\"{}\" placeholder=\"new-workspace-name\" maxlength=\"90\" required></div>\
<button class=\"btn btn-primary\" type=\"submit\">New workspace</button></form>",
            esc(name)
        )
    } else {
        String::new()
    };
    let body = format!("<div class=\"page-header\"><div><h1>Workspaces</h1></div></div>{err}{create}{table}");
    page(st, p, "Workspaces", "workspaces", body, false).await
}

pub async fn workspaces(State(st): State<AppState>, Extension(p): Extension<Principal>) -> Page {
    workspaces_view(&st, &p, None, "").await
}

#[derive(Deserialize)]
pub struct NewWs {
    name: String,
}

pub async fn create_workspace(State(st): State<AppState>, Extension(p): Extension<Principal>, Form(f): Form<NewWs>) -> Result<Response, WebError> {
    if !p.can(Action::CreateWorkspace) {
        return Err(StoreError::Forbidden("your role does not allow that".into()).into());
    }
    let name = f.name.trim().to_string();
    match st.store.create_workspace(&p.org_id, &name).await {
        Ok(ws) => Ok(Redirect::to(&format!("/workspaces/{}", ws.id)).into_response()),
        Err(e @ (StoreError::Invalid(_) | StoreError::Conflict(_))) => {
            let html = workspaces_view(&st, &p, Some(e.to_string()), &name).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, html).into_response())
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn workspace(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>) -> Page {
    let ws = ws_access(&st, &p, &id, Action::Read).await?;
    let runs = st.store.list_runs(Some(&id)).await?;
    let config = st.store.latest_config(&id).await?.map(|c| c.text).unwrap_or_else(|| SAMPLE_CONFIG.to_string());
    let state = st.store.current_state(&id).await?;
    let nvars = st.store.list_variables(&id).await?.len();
    let can_run = p.can(Action::QueueRun);
    let cli = if p.can(Action::WriteState) {
        let base = st.cfg.base_url.trim_end_matches('/');
        let snippet = format!(
            "terraform {{\n  backend \"http\" {{\n    address        = \"{base}/backend/{n}\"\n    lock_address   = \"{base}/backend/{n}/lock\"\n    unlock_address = \"{base}/backend/{n}/lock\"\n    username       = \"token\"\n  }}\n}}",
            n = ws.name
        );
        format!(
            "<div class=\"card\" style=\"margin-top:16px\"><div class=\"card-h\">Use from the OpenTofu CLI</div><div class=\"card-b\">\
<p class=\"muted\" style=\"margin-top:0\">Keep this workspace's state here. Create a personal token under <a href=\"/tokens\">API tokens</a>, then:</p>\
<pre class=\"log\" style=\"margin:0 0 12px\">{}</pre>\
<pre class=\"log\" style=\"margin:0\">export TF_HTTP_PASSWORD=&lt;your token&gt;\ntofu init\ntofu apply</pre></div></div>",
            esc(&snippet)
        )
    } else {
        String::new()
    };

    let lock_html = match &ws.lock {
        None if p.can(Action::Lock) => format!(
            "<form class=\"inline form-row\" method=\"post\" action=\"/workspaces/{}/lock\"><label class=\"sr-only\" for=\"reason\">Reason</label>\
<input id=\"reason\" name=\"reason\" placeholder=\"Reason for locking\" style=\"height:28px;border:1px solid var(--color-border-input);border-radius:6px;padding:0 8px;background:var(--color-bg);color:var(--color-text)\">\
<button class=\"btn btn-secondary btn-sm\" type=\"submit\">Lock</button></form>",
            esc(&ws.id)
        ),
        None => String::new(),
        Some(l) => {
            let by_run = l.owner.starts_with("run:");
            let who = if by_run {
                "a run"
            } else if l.owner.starts_with("cli:") {
                "the OpenTofu CLI"
            } else {
                "a user"
            };
            let button = if by_run && p.can(Action::ForceUnlock) {
                format!("<form class=\"inline\" method=\"post\" action=\"/workspaces/{}/unlock\"><input type=\"hidden\" name=\"force\" value=\"1\"><button class=\"btn btn-secondary btn-sm\" type=\"submit\">Force unlock</button></form>", esc(&ws.id))
            } else if !by_run && p.can(Action::Unlock) {
                format!("<form class=\"inline\" method=\"post\" action=\"/workspaces/{}/unlock\"><button class=\"btn btn-secondary btn-sm\" type=\"submit\">Unlock</button></form>", esc(&ws.id))
            } else {
                String::new()
            };
            format!(
                "<div class=\"banner banner-warning\" role=\"status\"><span>Locked by {who}: {}.{lease} Queued runs wait until it is released.</span><span style=\"margin-left:auto\">{button}</span></div>",
                esc(&l.reason),
                lease = match l.expires_at {
                    Some(e) => format!(" It lapses by itself {} unless its holder keeps renewing it.", until(e)),
                    None => String::new(),
                },
            )
        }
    };
    let lock_banner = if ws.lock.is_some() { lock_html.clone() } else { String::new() };
    let lock_form = if ws.lock.is_none() { lock_html } else { String::new() };

    let mut rows = String::new();
    for r in runs.iter().take(20) {
        rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td class=\"mono\">{}</td><td>{}</td><td class=\"muted\">{}</td></tr>",
            run_ref(r), badge(r.status), esc(&summary_text(&r.summary)), esc(&r.message), ago(r.created_at)
        ));
    }
    let runs_html = if runs.is_empty() {
        "<div class=\"empty\">No runs yet.</div>".to_string()
    } else {
        format!("<div class=\"table-wrap\"><table class=\"table\"><thead><tr><th>Run</th><th>Status</th><th>Changes</th><th>Message</th><th>Queued</th></tr></thead><tbody>{rows}</tbody></table></div>")
    };

    let queue = if can_run {
        format!(
            "<div class=\"card\"><div class=\"card-h\">Queue a run</div><div class=\"card-b\">\
<form method=\"post\" action=\"/workspaces/{id}/runs\">\
<div class=\"field\"><label for=\"config\">Configuration</label><textarea class=\"code\" id=\"config\" name=\"config\" spellcheck=\"false\">{config}</textarea>\
<div class=\"hint\">Simulated engine: reads resource \"type\" \"name\" {{ ... }} blocks and substitutes var.NAME from the variables. Add a line <span class=\"mono\">fail_plan</span> or <span class=\"mono\">fail_apply</span> to test errors.</div></div>\
<div class=\"form-row\" style=\"margin-top:12px\"><div class=\"field\" style=\"flex:1;min-width:200px\"><label class=\"sr-only\" for=\"message\">Message</label><input id=\"message\" name=\"message\" placeholder=\"Message (optional)\" maxlength=\"200\"></div>\
<label class=\"sr-only\" for=\"mode\">Mode</label><select id=\"mode\" name=\"mode\"><option value=\"plan_and_apply\">Plan and apply</option><option value=\"plan_only\">Plan only (speculative)</option></select>\
<label><input type=\"checkbox\" name=\"auto_apply\" value=\"on\"{auto}> Apply automatically after a clean plan</label>\
<label title=\"Plans faster but cannot see changes made outside the platform\"><input type=\"checkbox\" name=\"skip_refresh\" value=\"on\"> Skip the refresh</label>\
<button class=\"btn btn-primary\" type=\"submit\">Queue run</button></div></form></div></div>",
            id = esc(&ws.id),
            config = esc(&config),
            auto = if ws.auto_apply { " checked" } else { "" },
        )
    } else {
        String::new()
    };

    let body = format!(
        "<div class=\"page-header\"><div><div class=\"crumbs\"><a href=\"/workspaces\">Workspaces</a></div><h1 class=\"mono\">{name}</h1></div><div>{lock_form}</div></div>{lock_banner}\
<div class=\"stats\">\
<div class=\"stat\"><div class=\"k\">Resources</div><div class=\"v\">{res}</div></div>\
<div class=\"stat\"><div class=\"k\">State serial</div><div class=\"v\">{serial}</div></div>\
<div class=\"stat\"><div class=\"k\">Runs</div><div class=\"v\">{nruns}</div></div>\
<div class=\"stat\"><div class=\"k\">Variables</div><div class=\"v\">{nvars}</div></div>\
<div class=\"stat\"><div class=\"k\">Lock</div><div class=\"v\">{lockv}</div></div>\
<div class=\"stat\" id=\"drift\"><div class=\"k\">Drift</div><div class=\"v\">{driftv}</div></div></div>{drift_actions}{queue}{cli}\
<h2 class=\"sec\">Runs</h2>{runs_html}\
<p class=\"muted\" style=\"margin-top:16px\"><a href=\"/workspaces/{id}/variables\">Variables</a> · <a href=\"/workspaces/{id}/states\">State versions</a> · <a href=\"/workspaces/{id}/settings\">Settings</a></p>",
        name = esc(&ws.name),
        id = esc(&ws.id),
        res = ws.resource_count,
        serial = state.as_ref().map(|s| s.serial.to_string()).unwrap_or_else(|| "-".into()),
        nruns = runs.len(),
        lockv = if ws.lock.is_some() { "locked" } else { "free" },
        driftv = drift_text(&ws),
        drift_actions = if can_run && state.is_some() {
            format!("<form class=\"inline\" method=\"post\" action=\"/workspaces/{}/check-drift\" style=\"display:block;margin:-4px 0 12px\"><button class=\"btn btn-secondary btn-sm\" type=\"submit\">Check for drift now</button> <span class=\"muted\">A drift check only compares the state with the real infrastructure; it changes nothing.</span></form>", esc(&ws.id))
        } else {
            String::new()
        },
        cli = cli,
    );
    page(&st, &p, &ws.name, "workspaces", body, false).await
}

#[derive(Deserialize)]
pub struct NewRunForm {
    config: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    auto_apply: String,
    #[serde(default)]
    skip_refresh: String,
}

pub async fn create_run(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>, Form(f): Form<NewRunForm>) -> Result<Redirect, WebError> {
    ws_access(&st, &p, &id, Action::QueueRun).await?;
    let op = if f.mode == "plan_only" { Operation::PlanOnly } else { Operation::PlanAndApply };
    let opts = RunOptions { skip_refresh: f.skip_refresh == "on", refresh_only: false, vcs: None };
    let run = queue_run(&st.store, &id, &f.config, f.message.trim(), op, f.auto_apply == "on", "ui", opts).await?;
    Ok(Redirect::to(&format!("/runs/{}", run.id)))
}

#[derive(Deserialize)]
pub struct LockForm {
    #[serde(default)]
    reason: String,
    #[serde(default)]
    force: String,
}

pub async fn check_drift(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>) -> Result<Redirect, WebError> {
    ws_access(&st, &p, &id, Action::QueueRun).await?;
    let run = crate::drift::queue_check(&st.store, &id, "ui").await?;
    Ok(Redirect::to(&format!("/runs/{}", run.id)))
}

pub async fn lock(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>, Form(f): Form<LockForm>) -> Result<Redirect, WebError> {
    ws_access(&st, &p, &id, Action::Lock).await?;
    let reason = if f.reason.trim().is_empty() { "locked from the dashboard" } else { f.reason.trim() };
    st.store.lock_workspace(&id, "user", reason, None, None).await?;
    Ok(Redirect::to(&format!("/workspaces/{id}")))
}

pub async fn unlock(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>, Form(f): Form<LockForm>) -> Result<Redirect, WebError> {
    let force = f.force == "1";
    let before = ws_access(&st, &p, &id, if force { Action::ForceUnlock } else { Action::Unlock }).await?;
    st.store.unlock_workspace(&id, force).await?;
    if force {
        // The run that held the lock was ended; stop its engine step too.
        if let Some(run) = before.lock.as_ref().and_then(|l| crate::lock_holder_run(&l.owner)) {
            st.cancels.cancel(run);
        }
    }
    Ok(Redirect::to(&format!("/workspaces/{id}")))
}

pub async fn runs(State(st): State<AppState>, Extension(p): Extension<Principal>) -> Page {
    let runs = st.store.list_runs_in_org(&p.org_id).await?;
    let names: HashMap<String, String> = st.store.list_workspaces(&p.org_id).await?.into_iter().map(|w| (w.id, w.name)).collect();
    let mut rows = String::new();
    for r in &runs {
        let wname = names.get(&r.workspace_id).cloned().unwrap_or_default();
        rows.push_str(&format!(
            "<tr><td>{}</td><td><a class=\"mono\" href=\"/workspaces/{}\">{}</a></td><td>{}</td><td class=\"mono\">{}</td><td>{}</td><td class=\"muted\">{}</td></tr>",
            run_ref(r), esc(&r.workspace_id), esc(&wname), badge(r.status), esc(&summary_text(&r.summary)), esc(&r.message), ago(r.created_at)
        ));
    }
    let table = if runs.is_empty() {
        "<div class=\"empty\">No runs yet.</div>".to_string()
    } else {
        format!("<div class=\"table-wrap\"><table class=\"table\"><thead><tr><th>Run</th><th>Workspace</th><th>Status</th><th>Changes</th><th>Message</th><th>Queued</th></tr></thead><tbody>{rows}</tbody></table></div>")
    };
    let live = runs.iter().any(|r| !r.status.is_final() && r.status != RunStatus::NeedsConfirmation);
    let body = format!("<div class=\"page-header\"><div><h1>Runs</h1></div></div>{table}");
    page(&st, &p, "Runs", "runs", body, live).await
}

/// "<1s", "42s", "1m 05s", "2h 03m".
fn fmt_dur(secs: u64) -> String {
    match secs {
        0 => "<1s".into(),
        1..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// How long each stage took, from the times the run entered each status. A stage that has not
/// ended (or never happened) has none.
fn stage_secs(run: &Run) -> [Option<u64>; 4] {
    let t = |k: &str| run.status_timestamps.get(k).copied();
    let first = |ks: &[&str]| ks.iter().filter_map(|k| t(k)).min();
    let span = |a: Option<u64>, b: Option<u64>| match (a, b) {
        (Some(a), Some(b)) if b >= a => Some(b - a),
        _ => None,
    };
    let applied_started = t("applying").or(t("apply_queued"));
    // Auto-apply skips the pause: planning ends when the apply is queued.
    let plan_end = first(&["needs_confirmation", "planned_and_finished", "apply_queued"]).or_else(|| if applied_started.is_none() { first(&["errored", "canceled", "force_canceled", "discarded"]) } else { None });
    let confirm_end = first(&["apply_queued", "discarded", "canceled", "force_canceled"]);
    let apply_end = if applied_started.is_some() { first(&["applied", "errored", "force_canceled", "canceled"]) } else { None };
    [span(t("pending"), t("planning")), span(t("planning"), plan_end), span(t("needs_confirmation"), confirm_end), span(applied_started, apply_end)]
}

fn stepper(run: &Run) -> String {
    use RunStatus::*;
    let mut steps: Vec<(&str, &str)> = vec![("Queue", ""), ("Plan", ""), ("Confirm", ""), ("Apply", ""), ("Done", "")];
    let planned = run.summary.is_some();
    match run.status {
        Pending => steps[0].1 = "current",
        Planning => {
            steps[0].1 = "done";
            steps[1].1 = "current"
        }
        NeedsConfirmation => {
            steps[0].1 = "done";
            steps[1].1 = "done";
            steps[2].1 = "current"
        }
        ApplyQueued | Applying => {
            for s in steps.iter_mut().take(3) {
                s.1 = "done"
            }
            steps[3].1 = "current"
        }
        Applied => {
            for s in steps.iter_mut() {
                s.1 = "done"
            }
        }
        PlannedAndFinished => {
            steps[0].1 = "done";
            steps[1].1 = "done";
            steps[4].1 = "done"
        }
        Errored => {
            steps[0].1 = "done";
            if planned {
                for s in steps.iter_mut().take(3) {
                    s.1 = "done"
                }
                steps[3].1 = "failed"
            } else {
                steps[1].1 = "failed"
            }
        }
        Discarded | Canceled => {
            steps[0].1 = "done";
            if planned {
                steps[1].1 = "done"
            }
        }
        ForceCanceled => {
            steps[0].1 = "done";
            if planned {
                for s in steps.iter_mut().take(3) {
                    s.1 = "done"
                }
                steps[3].1 = "failed"
            } else {
                steps[1].1 = "failed"
            }
        }
    }
    let secs = stage_secs(run);
    let mut h = String::from("<ol class=\"stepper\" aria-label=\"Run stages\">");
    for (i, (label, class)) in steps.into_iter().enumerate() {
        let (cls, cur, mark) = match class {
            "done" => (" class=\"done\"", "", "✓ "),
            "failed" => (" class=\"failed\"", "", "✕ "),
            "current" => ("", " aria-current=\"step\"", ""),
            _ => ("", "", ""),
        };
        let took = match (class, secs.get(i).copied().flatten()) {
            ("done" | "failed", Some(s)) => format!("<span class=\"dur\">{}</span>", fmt_dur(s)),
            _ => String::new(),
        };
        h.push_str(&format!("<li{cls}{cur}>{mark}{label}{took}</li>"));
    }
    h.push_str("</ol>");
    h
}

/// Follows a working run: appends lines as they arrive and reloads the page when the status
/// changes (so the stepper, banners and buttons are the server's). A dropped connection
/// reconnects by itself and resumes from the last line seen.
const STREAM_JS: &str = "<script>(function(){var el=document.getElementById('run-stream');if(!el||!window.EventSource)return;\
var n=parseInt(el.dataset.lines,10)||0;var es=new EventSource(el.dataset.events+'?from='+n);\
function cls(l){var c=l.replace(/^\\s+/,'').charAt(0);if(c==='+')return'g';if(c==='~')return'y';if(c==='-')return'r';if(l.indexOf('Error')===0)return'r';return''}\
es.addEventListener('log',function(e){var first=el.querySelector('.ts');if(first&&el.childNodes.length===1)el.removeChild(first);\
var end=el.scrollHeight-el.scrollTop-el.clientHeight<40;var c=cls(e.data);var t=document.createTextNode(e.data+'\\n');\
if(c){var s=document.createElement('span');s.className=c;s.appendChild(t);el.appendChild(s)}else{el.appendChild(t)}\
if(end)el.scrollTop=el.scrollHeight});\
es.addEventListener('status',function(e){try{if(JSON.parse(e.data).status!==el.dataset.status){es.close();location.reload()}}catch(x){}});\
es.addEventListener('done',function(){es.close();if(el.dataset.status!==''){location.reload()}});\
})();</script>";

fn log_html(lines: &[String]) -> String {
    if lines.is_empty() {
        return "<span class=\"ts\">Waiting for output…</span>".into();
    }
    let mut h = String::new();
    for l in lines {
        let class = match l.trim_start().chars().next() {
            Some('+') => "g",
            Some('~') => "y",
            Some('-') => "r",
            Some('E') if l.starts_with("Error") => "r",
            _ => "",
        };
        if class.is_empty() {
            h.push_str(&esc(l));
        } else {
            h.push_str(&format!("<span class=\"{class}\">{}</span>", esc(l)));
        }
        h.push('\n');
    }
    h
}

pub async fn run(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>) -> Page {
    let (run, ws) = run_access(&st, &p, &id, Action::Read).await?;
    let log = st.store.get_log(&id).await?;
    let queued = st.store.list_runs(Some(&run.workspace_id)).await?.iter().filter(|r| r.status == RunStatus::Pending && r.id != run.id).count();
    let act = actions(run.status);
    let can_confirm = p.can(Action::ConfirmRun);

    let lock_banner = if run.status.holds_lock() && run.operation == Operation::PlanAndApply {
        format!("<div class=\"banner banner-warning\" role=\"status\"><span>This run holds the workspace lock. {queued} other run(s) are queued behind it.</span></div>")
    } else {
        String::new()
    };
    let failed_banner = if run.status == RunStatus::Errored {
        "<div class=\"banner banner-danger\" role=\"alert\"><span>This run failed. The log below shows why; the workspace lock has been released.</span></div>".to_string()
    } else {
        String::new()
    };

    let force_banner = if run.status == RunStatus::ForceCanceled {
        "<div class=\"banner banner-danger\" role=\"alert\"><span>This run was force-canceled while it was working. Resources it was creating may exist without being recorded; the log says what was kept. The workspace lock has been released.</span></div>".to_string()
    } else {
        String::new()
    };

    let (add, chg, del) = run.summary.map(|s| (s.add, s.change, s.destroy)).unwrap_or((0, 0, 0));
    let outputs = run.summary.map(|s| s.outputs).unwrap_or(0);
    let outputs_stat = if outputs > 0 { format!("<div class=\"stat\"><div class=\"k\">Outputs</div><div class=\"v\">{outputs}</div></div>") } else { String::new() };
    let stats = format!(
        "<div class=\"stats\">\
<div class=\"stat\"><div class=\"k\">Add</div><div class=\"v\" style=\"color:var(--color-success)\">{add}</div></div>\
<div class=\"stat\"><div class=\"k\">Change</div><div class=\"v\" style=\"color:var(--color-warning)\">{chg}</div></div>\
<div class=\"stat\"><div class=\"k\">Destroy</div><div class=\"v\" style=\"color:var(--color-danger)\">{del}</div></div>{outputs_stat}\
<div class=\"stat\"><div class=\"k\">Mode</div><div class=\"v\" style=\"font-size:16px\">{}</div></div></div>",
        if run.operation == Operation::PlanOnly { "plan only" } else { "plan and apply" }
    );

    let mut changes = String::new();
    for c in &run.changes {
        let (class, sym, word) = match c.action {
            ChangeAction::Create => ("add", "+", "create"),
            ChangeAction::Update => ("chg", "~", "update in place"),
            ChangeAction::Delete => ("del", "−", "destroy"),
            ChangeAction::Replace => ("del", "±", "replace"),
        };
        changes.push_str(&format!("<div class=\"diff-row {class}\"><span class=\"sym\">{sym}</span><span>{}</span><span class=\"muted\">{word}</span></div>", esc(&c.addr)));
    }
    let changes_html = if run.changes.is_empty() {
        let msg = match run.summary {
            Some(_) => "No resource changes.",
            None if run.status.is_final() => "No plan was produced.",
            None => "The plan will appear here when planning finishes.",
        };
        format!("<div class=\"empty\">{msg}</div>")
    } else {
        format!("<div class=\"diff\">{changes}</div>")
    };

    let states_note = run.input_state_serial.map(|s| format!("serial {s}")).unwrap_or_else(|| "none (first run)".into());
    let details = format!(
        "<div class=\"card\"><div class=\"card-h\">Details</div><div class=\"card-b\"><dl class=\"kv\" style=\"grid-template-columns:96px 1fr\">\
<dt>Workspace</dt><dd><a class=\"mono\" href=\"/workspaces/{wid}\">{wname}</a></dd>\
<dt>Source</dt><dd>{source}</dd>\
<dt>Message</dt><dd>{message}</dd>\
<dt>Config</dt><dd class=\"mono\">cv-{cv}</dd>\
<dt>State in</dt><dd class=\"mono\">{states_note}</dd>\
<dt>Engine</dt><dd class=\"mono\">{engine}</dd>\
<dt>Queued</dt><dd>{queued_ago}</dd></dl></div></div>",
        wid = esc(&ws.id),
        wname = esc(&ws.name),
        source = esc(&run.source),
        message = if run.message.is_empty() { "-".to_string() } else { esc(&run.message) },
        cv = esc(short(&run.config_version_id)),
        engine = esc(st.engine_name),
        queued_ago = ago(run.created_at),
    );

    let destroy_warn = if del > 0 {
        format!("<strong>Apply {del} destroy?</strong> <span class=\"muted\">Resources will be deleted.</span>")
    } else {
        "<strong>Apply this plan?</strong> <span class=\"muted\">The changes above will be made.</span>".to_string()
    };
    let rid = esc(&run.id);
    let actionbar = if act.is_confirmable && can_confirm {
        format!(
            "<div class=\"actionbar\"><span>{destroy_warn}</span><span class=\"spacer\"></span>\
<form class=\"inline\" method=\"post\" action=\"/runs/{rid}/discard\"><button class=\"btn btn-secondary\" type=\"submit\">Discard run</button></form>\
<form class=\"inline\" method=\"post\" action=\"/runs/{rid}/apply\"><button class=\"btn btn-primary\" type=\"submit\">Confirm &amp; apply</button></form></div>"
        )
    } else if act.is_cancelable && can_confirm {
        format!(
            "<div class=\"actionbar\"><span class=\"muted\">This run has not started applying yet.</span><span class=\"spacer\"></span>\
<form class=\"inline\" method=\"post\" action=\"/runs/{rid}/cancel\"><button class=\"btn btn-secondary\" type=\"submit\">Cancel run</button></form></div>"
        )
    } else if run.status == RunStatus::Applying && p.can(Action::ForceCancel) {
        format!(
            "<div class=\"actionbar\"><span class=\"muted\">This run is applying and cannot be canceled gently. Stopping it now is risky: resources it was creating may exist without being recorded.</span><span class=\"spacer\"></span>\
<form class=\"inline risk\" method=\"post\" action=\"/runs/{rid}/force-cancel\"><label><input type=\"checkbox\" name=\"confirm\" value=\"yes\"> I understand the risk</label><button class=\"btn btn-secondary\" type=\"submit\">Force cancel</button></form></div>"
        )
    } else if run.status == RunStatus::Applying {
        "<div class=\"actionbar\"><span class=\"muted\">This run is applying and cannot be canceled. An administrator can force-cancel it.</span></div>".to_string()
    } else {
        String::new()
    };

    // A working run is followed by stream (with a reload fallback when scripts are off); one
    // waiting for a person, or finished, is left alone so nothing reloads under the reader.
    let streaming = !run.status.is_final() && run.status != RunStatus::NeedsConfirmation;
    let refresh = if streaming { Refresh::NoScript } else { Refresh::No };
    let stream_attrs = if streaming { format!(" id=\"run-stream\" data-events=\"/runs/{rid}/events\"") } else { String::new() };
    let lint_html = if run.lint.is_empty() {
        String::new()
    } else {
        let mut rows = String::new();
        for f in &run.lint {
            let at = match (&f.file, f.line) {
                (Some(file), Some(l)) => format!("{}:{l}", esc(file)),
                (None, Some(l)) => format!("line {l}"),
                _ => String::new(),
            };
            let class = match f.severity.as_str() {
                "error" => "del",
                "warning" => "chg",
                _ => "",
            };
            rows.push_str(&format!(
                "<div class=\"diff-row {class}\"><span class=\"sym\">!</span><span>{} <span class=\"muted\">{}</span></span><span class=\"muted mono\">{}</span></div>",
                esc(&f.message),
                esc(&f.rule),
                at
            ));
        }
        format!("<h2 class=\"sec\">Lint ({})</h2><div class=\"diff\" id=\"lint\">{rows}</div><p class=\"muted\" style=\"margin:4px 0 0\">Findings never stop a run.</p>", run.lint.len())
    };
    let stream_js = if streaming { STREAM_JS } else { "" };
    let body = format!(
        "<div class=\"page-header\"><div><div class=\"crumbs\"><a href=\"/workspaces\">Workspaces</a> / <a href=\"/workspaces/{wid}\">{wname}</a> / runs</div>\
<h1><span class=\"mono\">run-{short}</span> {badge}</h1></div></div>{lock_banner}{failed_banner}{force_banner}{stepper}{stats}\
<div class=\"cols\"><div><h2 class=\"sec\" style=\"margin-top:0\">Resource changes</h2>{changes_html}\
{lint_html}<h2 class=\"sec\">Log</h2><div class=\"log\" role=\"log\" aria-live=\"off\"{stream_attrs} data-status=\"{status}\" data-lines=\"{nlines}\">{log}</div></div><aside>{details}</aside></div>{actionbar}{stream_js}",
        wid = esc(&ws.id),
        wname = esc(&ws.name),
        short = esc(short(&run.id)),
        badge = badge(run.status),
        stepper = stepper(&run),
        status = run.status.as_str(),
        nlines = log.len(),
        log = log_html(&log),
    );
    page(&st, &p, &format!("run-{}", short(&run.id)), "runs", body, refresh).await
}

/// A checkbox named `name` ticked in a url-encoded form body.
fn form_flag(body: &[u8], name: &str) -> bool {
    serde_urlencoded::from_bytes::<Vec<(String, String)>>(body).unwrap_or_default().iter().any(|(k, v)| k == name && matches!(v.as_str(), "yes" | "on" | "true" | "1"))
}

pub async fn run_action(State(st): State<AppState>, Extension(p): Extension<Principal>, Path((id, action)): Path<(String, String)>, body: Bytes) -> Result<Redirect, WebError> {
    let (ev, need) = match action.as_str() {
        "apply" => (Event::Confirm, Action::ConfirmRun),
        "discard" => (Event::Discard, Action::ConfirmRun),
        "cancel" => (Event::Cancel, Action::ConfirmRun),
        "force-cancel" => (Event::ForceCancel, Action::ForceCancel),
        _ => return Err(StoreError::NotFound("unknown action".into()).into()),
    };
    let (run, _) = run_access(&st, &p, &id, need).await?;
    if ev == Event::ForceCancel && run.status == RunStatus::Applying && !form_flag(&body, "confirm") {
        return Err(StoreError::Invalid("Tick the box to confirm: force-canceling a run that is applying can leave resources that exist but are not recorded.".into()).into());
    }
    match run_event(&st, &run, ev).await {
        Ok(_) => {}
        // A double click or a stale tab asks for what already happened: show the run, not an error.
        Err(StoreError::Conflict(m)) => {
            let now_status = st.store.get_run(&id).await?.status;
            let done = match ev {
                Event::Confirm => matches!(now_status, RunStatus::ApplyQueued | RunStatus::Applying | RunStatus::Applied),
                Event::Discard => now_status == RunStatus::Discarded,
                Event::Cancel => now_status == RunStatus::Canceled,
                Event::ForceCancel => now_status == RunStatus::ForceCanceled,
                _ => false,
            };
            if !done {
                return Err(StoreError::Conflict(m).into());
            }
        }
        Err(e) => return Err(e.into()),
    }
    Ok(Redirect::to(&format!("/runs/{id}")))
}

pub async fn states(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>) -> Page {
    let ws = ws_access(&st, &p, &id, Action::Read).await?;
    let list = st.store.list_states(&id).await?;
    let mut rows = String::new();
    for (i, s) in list.iter().enumerate() {
        let run = s.run_id.as_ref().map(|r| format!("<a class=\"mono\" href=\"/runs/{}\">run-{}</a>", esc(r), esc(short(r)))).unwrap_or_default();
        let cur = if i == 0 { " <span class=\"chip\">current</span>" } else { "" };
        rows.push_str(&format!(
            "<tr><td class=\"mono\">{}{cur}</td><td>{}</td><td>{run}</td><td><span class=\"badge badge-success\" data-icon=\"✓\">{}</span></td><td class=\"muted\">{}</td></tr>",
            s.serial, s.resource_count, esc(&s.status), ago(s.created_at)
        ));
    }
    let table = if list.is_empty() {
        "<div class=\"empty\">No state yet. The first applied run creates serial 1.</div>".to_string()
    } else {
        format!("<div class=\"table-wrap\"><table class=\"table\"><thead><tr><th>Serial</th><th>Resources</th><th>Run</th><th>Status</th><th>Created</th></tr></thead><tbody>{rows}</tbody></table></div><p class=\"muted\">State is encrypted at rest.</p>")
    };
    let body = format!(
        "<div class=\"page-header\"><div><div class=\"crumbs\"><a href=\"/workspaces\">Workspaces</a> / <a href=\"/workspaces/{}\">{}</a></div><h1>State versions</h1></div></div>{table}",
        esc(&ws.id), esc(&ws.name)
    );
    page(&st, &p, "State versions", "workspaces", body, false).await
}

// ---------- variables ----------

async fn variables_view(st: &AppState, p: &Principal, ws: &Workspace, error: Option<String>) -> Page {
    let list = st.store.list_variables(&ws.id).await?;
    let can = p.can(Action::ManageVariables);
    let mut rows = String::new();
    for v in &list {
        let value = if v.sensitive { "<span class=\"chip\">sensitive, write-only</span>".to_string() } else { format!("<span class=\"mono\">{}</span>", esc(v.value.as_deref().unwrap_or(""))) };
        let del = if can {
            format!("<form class=\"inline\" method=\"post\" action=\"/workspaces/{}/variables/{}/delete\"><button class=\"btn btn-ghost btn-sm\" type=\"submit\">Delete</button></form>", esc(&ws.id), esc(&v.id))
        } else {
            String::new()
        };
        rows.push_str(&format!("<tr><td class=\"mono\">{}</td><td><span class=\"chip\">{}</span></td><td>{value}</td><td>{}</td><td>{del}</td></tr>", esc(&v.key), esc(&v.category), esc(&v.description)));
    }
    let table = if list.is_empty() {
        "<div class=\"empty\">No variables yet.</div>".to_string()
    } else {
        format!("<div class=\"table-wrap\"><table class=\"table\"><thead><tr><th>Key</th><th>Kind</th><th>Value</th><th>Description</th><th></th></tr></thead><tbody>{rows}</tbody></table></div>")
    };
    let err = error.map(|e| format!("<div class=\"banner banner-danger err\" role=\"alert\">{}</div>", esc(&e))).unwrap_or_default();
    let form = if can {
        format!(
            "<div class=\"card\" style=\"margin-bottom:16px\"><div class=\"card-h\">Add or replace a variable</div><div class=\"card-b\">\
<form method=\"post\" action=\"/workspaces/{}/variables\" style=\"display:grid;gap:12px\">\
<div class=\"form-row\"><div class=\"field\"><label for=\"key\">Key</label><input id=\"key\" name=\"key\" maxlength=\"128\" required></div>\
<div class=\"field\"><label for=\"category\">Kind</label><select id=\"category\" name=\"category\"><option value=\"terraform\">Input variable</option><option value=\"env\">Environment variable</option></select></div>\
<div class=\"field\" style=\"flex:1\"><label for=\"value\">Value</label><input id=\"value\" name=\"value\" type=\"password\" autocomplete=\"off\"></div></div>\
<div class=\"form-row\"><label><input type=\"checkbox\" name=\"sensitive\" value=\"on\"> Sensitive (write-only, encrypted, hidden from logs)</label>\
<div class=\"field\" style=\"flex:1\"><label class=\"sr-only\" for=\"description\">Description</label><input id=\"description\" name=\"description\" placeholder=\"Description (optional)\" maxlength=\"200\"></div>\
<button class=\"btn btn-primary\" type=\"submit\">Save variable</button></div></form></div></div>",
            esc(&ws.id)
        )
    } else {
        String::new()
    };
    let body = format!(
        "<div class=\"page-header\"><div><div class=\"crumbs\"><a href=\"/workspaces\">Workspaces</a> / <a href=\"/workspaces/{}\">{}</a></div><h1>Variables</h1></div></div>{err}{form}{table}",
        esc(&ws.id), esc(&ws.name)
    );
    page(st, p, "Variables", "workspaces", body, false).await
}

pub async fn variables(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>) -> Page {
    let ws = ws_access(&st, &p, &id, Action::Read).await?;
    variables_view(&st, &p, &ws, None).await
}

#[derive(Deserialize)]
pub struct VarForm {
    key: String,
    category: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    sensitive: String,
    #[serde(default)]
    description: String,
}

pub async fn set_variable(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>, Form(f): Form<VarForm>) -> Result<Response, WebError> {
    let ws = ws_access(&st, &p, &id, Action::ManageVariables).await?;
    let nv = NewVariable { key: f.key.trim().to_string(), category: f.category, hcl: false, sensitive: f.sensitive == "on", value: f.value, description: f.description.trim().to_string() };
    match st.store.set_variable(&id, nv).await {
        Ok(_) => Ok(Redirect::to(&format!("/workspaces/{id}/variables")).into_response()),
        Err(e @ StoreError::Invalid(_)) => {
            let html = variables_view(&st, &p, &ws, Some(e.to_string())).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, html).into_response())
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn delete_variable(State(st): State<AppState>, Extension(p): Extension<Principal>, Path((id, var_id)): Path<(String, String)>) -> Result<Redirect, WebError> {
    ws_access(&st, &p, &id, Action::ManageVariables).await?;
    st.store.delete_variable(&id, &var_id).await?;
    Ok(Redirect::to(&format!("/workspaces/{id}/variables")))
}

// ---------- settings ----------

async fn settings_view(st: &AppState, p: &Principal, ws: &Workspace, error: Option<String>) -> Page {
    let can_edit = p.can(Action::UpdateWorkspace);
    let can_delete = p.can(Action::DeleteWorkspace);
    let err = error.map(|e| format!("<div class=\"banner banner-danger err\" role=\"alert\">{}</div>", esc(&e))).unwrap_or_default();
    let dis = if can_edit { "" } else { " disabled" };
    let form = format!(
        "<div class=\"card\"><div class=\"card-h\">General</div><div class=\"card-b\">\
<form method=\"post\" action=\"/workspaces/{id}/settings\" style=\"display:grid;gap:12px\">\
<div class=\"field\"><label for=\"name\">Name</label><input id=\"name\" name=\"name\" value=\"{name}\" maxlength=\"90\" required{dis}>\
<div class=\"hint\">Letters, numbers, dash and underscore. The OpenTofu backend address uses the name, so renaming means updating <span class=\"mono\">address</span> in the CLI configuration.</div></div>\
<div class=\"field\"><label for=\"description\">Description</label><input id=\"description\" name=\"description\" value=\"{desc}\" maxlength=\"500\"{dis}></div>\
<label><input type=\"checkbox\" name=\"auto_apply\" value=\"on\"{auto}{dis}> Apply automatically after a clean plan (the default for new runs; a run can still ask otherwise)</label>\
<div class=\"field\"><label for=\"drift_minutes\">Check for drift every (minutes)</label><input id=\"drift_minutes\" name=\"drift_minutes\" value=\"{drift}\" inputmode=\"numeric\" placeholder=\"off\" style=\"max-width:140px\"{dis}>\
<div class=\"hint\">Leave empty to check only when asked. A check refreshes the state against the real infrastructure and reports what changed outside the platform; it never changes anything. Shortest: {min} minutes.</div></div>\
{save}</form></div></div>",
        id = esc(&ws.id),
        name = esc(&ws.name),
        desc = esc(&ws.description),
        auto = if ws.auto_apply { " checked" } else { "" },
        drift = ws.drift_interval.map(|s| (s / 60).max(1).to_string()).unwrap_or_default(),
        min = (st.cfg.min_drift_interval_secs / 60).max(1),
        save = if can_edit { "<div><button class=\"btn btn-primary\" type=\"submit\">Save settings</button></div>" } else { "<p class=\"muted\">Only owners and admins can change these.</p>" },
    );
    let danger = if can_delete {
        format!(
            "<div class=\"card\" style=\"margin-top:16px\"><div class=\"card-h\">Delete this workspace</div><div class=\"card-b\">\
<p class=\"muted\" style=\"margin-top:0\">Deletes the workspace with its runs, variables and every state version. The infrastructure it manages is not touched: destroy it first, or it will be left running with no record. This cannot be undone.</p>\
<form method=\"post\" action=\"/workspaces/{id}/delete\" class=\"form-row\">\
<label class=\"sr-only\" for=\"confirm\">Type the workspace name to confirm</label><input id=\"confirm\" name=\"confirm\" placeholder=\"Type {name} to confirm\" autocomplete=\"off\" required>\
<button class=\"btn btn-danger\" type=\"submit\">Delete workspace</button></form></div></div>",
            id = esc(&ws.id),
            name = esc(&ws.name)
        )
    } else {
        String::new()
    };
    let repo_card = repo_card(st, p, ws).await?;
    let body = format!(
        "<div class=\"page-header\"><div><div class=\"crumbs\"><a href=\"/workspaces\">Workspaces</a> / <a href=\"/workspaces/{id}\">{name}</a></div><h1>Settings</h1></div></div>{err}{form}{repo_card}{danger}",
        id = esc(&ws.id),
        name = esc(&ws.name)
    );
    page(st, p, "Settings", "workspaces", body, false).await
}

/// The "Repository" card of the settings page: what the workspace is connected to, and the forms to change it.
async fn repo_card(st: &AppState, p: &Principal, ws: &Workspace) -> Result<String, WebError> {
    let link = st.store.get_vcs_link(&ws.id).await?;
    let can = p.can(Action::UpdateWorkspace);
    let hook = format!("{}/webhooks/github/{}", st.cfg.base_url.trim_end_matches('/'), ws.id);
    let status = match &link {
        Some(l) => format!(
            "<p style=\"margin-top:0\">Connected to <span class=\"mono\">{}</span>, branch <span class=\"mono\">{}</span>{}. A pull request is planned and the result is commented on it; a push to the branch queues a run to apply.</p>\
<p class=\"muted\">Webhook address: <span class=\"mono\">{}</span> · access token: {}</p>",
            esc(&l.repo),
            esc(&l.branch),
            if l.working_dir.is_empty() { String::new() } else { format!(", folder <span class=\"mono\">{}</span>", esc(&l.working_dir)) },
            esc(&hook),
            if l.has_token { "set" } else { "not set (needed for private repositories and for comments)" }
        ),
        None => "<p class=\"muted\" style=\"margin-top:0\">Not connected. Connect a GitHub repository to plan pull requests automatically. Pull requests from forks are never planned.</p>".to_string(),
    };
    let forms = if can {
        let disconnect = if link.is_some() {
            format!("<form class=\"inline\" method=\"post\" action=\"/workspaces/{}/vcs/disconnect\"><button class=\"btn btn-secondary btn-sm\" type=\"submit\">Disconnect</button></form>", esc(&ws.id))
        } else {
            String::new()
        };
        format!(
            "<form method=\"post\" action=\"/workspaces/{id}/vcs\" style=\"display:grid;gap:12px;margin-top:12px\">\
<div class=\"form-row\"><div class=\"field\"><label for=\"repo\">Repository (owner/name)</label><input id=\"repo\" name=\"repo\" value=\"{repo}\" required></div>\
<div class=\"field\"><label for=\"branch\">Branch</label><input id=\"branch\" name=\"branch\" value=\"{branch}\" required></div>\
<div class=\"field\"><label for=\"working_dir\">Folder (optional)</label><input id=\"working_dir\" name=\"working_dir\" value=\"{dir}\"></div></div>\
<div class=\"field\"><label for=\"token\">Access token</label><input id=\"token\" name=\"token\" type=\"password\" autocomplete=\"off\" placeholder=\"{ph}\">\
<div class=\"hint\">A GitHub token that can read the repository's contents and write pull request comments. It is stored encrypted and never shown again.</div></div>\
<div class=\"form-row\"><button class=\"btn btn-primary\" type=\"submit\">{verb}</button> {disconnect}</div>\
<div class=\"hint\">Connecting makes a new webhook secret and shows it once.</div></form>",
            id = esc(&ws.id),
            repo = esc(link.as_ref().map(|l| l.repo.as_str()).unwrap_or("")),
            branch = esc(link.as_ref().map(|l| l.branch.as_str()).unwrap_or("main")),
            dir = esc(link.as_ref().map(|l| l.working_dir.as_str()).unwrap_or("")),
            ph = if link.as_ref().map(|l| l.has_token).unwrap_or(false) { "leave empty to keep the stored token" } else { "ghp_..." },
            verb = if link.is_some() { "Save and make a new secret" } else { "Connect" },
        )
    } else {
        String::new()
    };
    Ok(format!("<div class=\"card\" style=\"margin-top:16px\" id=\"repository\"><div class=\"card-h\">Repository</div><div class=\"card-b\">{status}{forms}</div></div>"))
}

#[derive(Deserialize)]
pub struct RepoForm {
    repo: String,
    #[serde(default)]
    branch: String,
    #[serde(default)]
    working_dir: String,
    #[serde(default)]
    token: String,
}

pub async fn connect_repo(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>, Form(f): Form<RepoForm>) -> Result<Response, WebError> {
    let ws = ws_access(&st, &p, &id, Action::UpdateWorkspace).await?;
    let token = f.token.trim();
    let branch = if f.branch.trim().is_empty() { "main" } else { f.branch.trim() };
    match st.store.set_vcs_link(&id, f.repo.trim(), branch, f.working_dir.trim(), if token.is_empty() { None } else { Some(token) }).await {
        Ok((link, secret)) => {
            let hook = format!("{}/webhooks/github/{}", st.cfg.base_url.trim_end_matches('/'), ws.id);
            let body = format!(
                "<div class=\"page-header\"><div><div class=\"crumbs\"><a href=\"/workspaces\">Workspaces</a> / <a href=\"/workspaces/{id}\">{name}</a> / <a href=\"/workspaces/{id}/settings\">settings</a></div><h1>Repository connected</h1></div></div>\
<div class=\"banner banner-warning\" role=\"status\">Copy the secret now. It is not shown again; connecting again makes a new one.</div>\
<div class=\"card\"><div class=\"card-b\"><p style=\"margin-top:0\">In <span class=\"mono\">{repo}</span> on GitHub, open Settings, Webhooks, Add webhook:</p>\
<ul><li>Payload URL: <span class=\"mono\">{hook}</span></li><li>Content type: <span class=\"mono\">application/json</span></li>\
<li>Secret: <span class=\"mono\" id=\"secret\">{secret}</span></li><li>Events: <b>Pull requests</b> and <b>Pushes</b></li></ul>\
<p class=\"muted\">Branch <span class=\"mono\">{branch}</span>{dir}. Access token: {tok}.</p><p><a href=\"/workspaces/{id}/settings\">Back to settings</a></p></div></div>",
                id = esc(&ws.id),
                name = esc(&ws.name),
                repo = esc(&link.repo),
                hook = esc(&hook),
                secret = esc(&secret),
                branch = esc(&link.branch),
                dir = if link.working_dir.is_empty() { String::new() } else { format!(", folder <span class=\"mono\">{}</span>", esc(&link.working_dir)) },
                tok = if link.has_token { "stored" } else { "not set" },
            );
            Ok(page(&st, &p, "Repository connected", "workspaces", body, false).await?.into_response())
        }
        Err(e @ StoreError::Invalid(_)) => {
            let html = settings_view(&st, &p, &ws, Some(e.to_string())).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, html).into_response())
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn disconnect_repo(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>) -> Result<Redirect, WebError> {
    ws_access(&st, &p, &id, Action::UpdateWorkspace).await?;
    match st.store.delete_vcs_link(&id).await {
        Ok(()) | Err(StoreError::NotFound(_)) => Ok(Redirect::to(&format!("/workspaces/{id}/settings"))),
        Err(e) => Err(e.into()),
    }
}

pub async fn settings(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>) -> Page {
    let ws = ws_access(&st, &p, &id, Action::Read).await?;
    settings_view(&st, &p, &ws, None).await
}

#[derive(Deserialize)]
pub struct SettingsForm {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    auto_apply: String,
    #[serde(default)]
    drift_minutes: String,
}

pub async fn save_settings(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>, Form(f): Form<SettingsForm>) -> Result<Response, WebError> {
    let ws = ws_access(&st, &p, &id, Action::UpdateWorkspace).await?;
    let drift_interval = match f.drift_minutes.trim() {
        "" => Some(None),
        m => match m.parse::<u64>() {
            Ok(n) if n >= 1 && n.saturating_mul(60) >= st.cfg.min_drift_interval_secs => Some(Some(n * 60)),
            _ => {
                let msg = format!("The drift check interval is a whole number of minutes, at least {}.", (st.cfg.min_drift_interval_secs / 60).max(1));
                let html = settings_view(&st, &p, &ws, Some(msg)).await?;
                return Ok((StatusCode::UNPROCESSABLE_ENTITY, html).into_response());
            }
        },
    };
    let patch = WorkspacePatch { name: Some(f.name.trim().to_string()), description: Some(f.description.trim().to_string()), auto_apply: Some(f.auto_apply == "on"), drift_interval };
    match st.store.update_workspace(&id, patch).await {
        Ok(_) => Ok(Redirect::to(&format!("/workspaces/{id}/settings")).into_response()),
        Err(e @ (StoreError::Invalid(_) | StoreError::Conflict(_))) => {
            let html = settings_view(&st, &p, &ws, Some(e.to_string())).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, html).into_response())
        }
        Err(e) => Err(e.into()),
    }
}

#[derive(Deserialize)]
pub struct DeleteForm {
    #[serde(default)]
    confirm: String,
}

pub async fn delete_workspace(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>, Form(f): Form<DeleteForm>) -> Result<Response, WebError> {
    let ws = ws_access(&st, &p, &id, Action::DeleteWorkspace).await?;
    if f.confirm.trim() != ws.name {
        let html = settings_view(&st, &p, &ws, Some("Type the workspace name exactly to confirm the deletion.".into())).await?;
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, html).into_response());
    }
    match st.store.delete_workspace(&id).await {
        Ok(()) => Ok(Redirect::to("/workspaces").into_response()),
        Err(e @ StoreError::Conflict(_)) => {
            let html = settings_view(&st, &p, &ws, Some(e.to_string())).await?;
            Ok((StatusCode::CONFLICT, html).into_response())
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn run(status: RunStatus, stamps: &[(&str, u64)], planned: bool) -> Run {
        Run {
            id: "r".into(),
            workspace_id: "w".into(),
            config_version_id: "c".into(),
            status,
            operation: Operation::PlanAndApply,
            source: "t".into(),
            message: String::new(),
            auto_apply: false,
            summary: planned.then(PlanSummary::default),
            changes: vec![],
            input_state_serial: None,
            lint: vec![],
            skip_refresh: false,
            refresh_only: false,
            vcs: None,
            status_timestamps: stamps.iter().map(|(k, v)| (k.to_string(), *v)).collect::<BTreeMap<_, _>>(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn durations_are_short_and_readable() {
        assert_eq!(fmt_dur(0), "<1s");
        assert_eq!(fmt_dur(42), "42s");
        assert_eq!(fmt_dur(65), "1m 05s");
        assert_eq!(fmt_dur(3 * 3600 + 120), "3h 02m");
    }

    #[test]
    fn each_finished_stage_shows_how_long_it_took() {
        let r = run(RunStatus::Applied, &[("pending", 100), ("planning", 102), ("needs_confirmation", 107), ("apply_queued", 130), ("applying", 131), ("applied", 196)], true);
        assert_eq!(stage_secs(&r), [Some(2), Some(5), Some(23), Some(65)]);
        let h = stepper(&r);
        for want in ["Queue<span class=\"dur\">2s</span>", "Plan<span class=\"dur\">5s</span>", "Confirm<span class=\"dur\">23s</span>", "Apply<span class=\"dur\">1m 05s</span>"] {
            assert!(h.contains(want), "{want} in {h}");
        }
        // auto-apply has no confirmation stage
        let auto = run(RunStatus::Applied, &[("pending", 10), ("planning", 10), ("apply_queued", 14), ("applying", 14), ("applied", 20)], true);
        assert_eq!(stage_secs(&auto), [Some(0), Some(4), None, Some(6)]);
    }

    #[test]
    fn a_stage_that_has_not_ended_or_never_ran_shows_nothing() {
        let working = run(RunStatus::Planning, &[("pending", 1), ("planning", 3)], false);
        assert_eq!(stage_secs(&working), [Some(2), None, None, None]);
        assert!(!stepper(&working).contains("Plan<span"), "the current stage has no duration yet");
        // failed while planning: the plan stage shows how long it ran before failing
        let failed = run(RunStatus::Errored, &[("pending", 1), ("planning", 2), ("errored", 9)], false);
        assert_eq!(stage_secs(&failed)[1], Some(7));
        assert!(stepper(&failed).contains("class=\"failed\">✕ Plan<span class=\"dur\">7s</span>"));
        // errored during apply: the plan stage ended at the pause, the apply stage ended in error
        let apply_failed = run(RunStatus::Errored, &[("pending", 1), ("planning", 2), ("needs_confirmation", 5), ("apply_queued", 8), ("applying", 8), ("errored", 12)], true);
        assert_eq!(stage_secs(&apply_failed), [Some(1), Some(3), Some(3), Some(4)]);
        // force-canceled mid-apply
        let fc = run(RunStatus::ForceCanceled, &[("pending", 1), ("planning", 2), ("needs_confirmation", 5), ("apply_queued", 8), ("applying", 8), ("force_canceled", 20)], true);
        assert_eq!(stage_secs(&fc)[3], Some(12));
        assert!(stepper(&fc).contains("class=\"failed\">✕ Apply<span class=\"dur\">12s</span>"));
        // discarded at the pause: no apply stage
        let disc = run(RunStatus::Discarded, &[("pending", 1), ("planning", 2), ("needs_confirmation", 5), ("discarded", 9)], true);
        assert_eq!(stage_secs(&disc), [Some(1), Some(3), Some(4), None]);
    }

    #[test]
    fn form_flags_read_a_ticked_box_only() {
        assert!(form_flag(b"confirm=yes", "confirm"));
        assert!(form_flag(b"x=1&confirm=on", "confirm"));
        assert!(!form_flag(b"", "confirm"));
        assert!(!form_flag(b"confirm=no", "confirm"));
        assert!(!form_flag(b"confirmed=yes", "confirm"));
        assert!(!form_flag(b"%%%", "confirm"));
    }
}
