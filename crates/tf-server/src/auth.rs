//! Sign up, sign in, sessions, API tokens, password reset, account deletion.
//!
//! Sessions are random 256-bit secrets in an HttpOnly, SameSite=Lax cookie; only
//! their sha256 is stored. API tokens work the same way (`tfk_` prefix). Cookie
//! authenticated writes must come from the same origin (CSRF); bearer tokens are
//! exempt because a browser never attaches them by itself.

use crate::web::{esc, layout_public};
use crate::{AppState, DEFAULT_TTL};
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::{Form, Path, Query, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use tf_core::*;
use tf_crypto::{random_token, sha256};
use tf_store::StoreError;

pub const SESSION_COOKIE: &str = "tf_session";

pub struct Config {
    pub cookie_secure: bool,
    pub require_verified: bool,
    pub base_url: String,
    pub session_ttl: u64,
    /// Requests per second one caller may make to /api/. 0 turns the limit off.
    pub api_rate_per_sec: u32,
    /// How long a lock taken through the API lasts unless renewed (0: no expiry).
    pub api_lock_lease_secs: u64,
    /// How long a lock taken by the OpenTofu CLI lasts. The CLI cannot renew it, so this must outlast
    /// its longest apply (0: no expiry).
    pub cli_lock_lease_secs: u64,
    /// Shortest interval a workspace may ask for scheduled drift checks, in seconds.
    pub min_drift_interval_secs: u64,
    /// How often the scheduler looks for drift checks that are due, in seconds (0 turns scheduling off).
    pub drift_tick_secs: u64,
    /// Where GitHub's API lives (changed only to point at a stand-in during tests or for GitHub Enterprise).
    pub github_api: String,
}

impl Default for Config {
    fn default() -> Self {
        Self { cookie_secure: false, require_verified: false, base_url: "http://127.0.0.1:8080".into(), session_ttl: DEFAULT_TTL, api_rate_per_sec: 30, api_lock_lease_secs: 3600, cli_lock_lease_secs: 12 * 3600, min_drift_interval_secs: 300, drift_tick_secs: 60, github_api: "https://api.github.com".into() }
    }
}

// ---------- mail ----------

pub trait Mailer: Send + Sync {
    fn send(&self, to: &str, subject: &str, body: &str);
}

/// Development mailer: prints the message, including its one-time link, to stderr.
/// Never use it where logs are shared. A real provider (Resend, Postmark) is a
/// `Mailer` implementation that needs the user's own API key.
pub struct LogMailer;

impl Mailer for LogMailer {
    fn send(&self, to: &str, subject: &str, body: &str) {
        eprintln!("[dev mail] to={to} subject={subject}\n{body}\n");
    }
}

#[derive(Default)]
pub struct CaptureMailer(pub Mutex<Vec<(String, String, String)>>);

impl Mailer for CaptureMailer {
    fn send(&self, to: &str, subject: &str, body: &str) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).push((to.into(), subject.into(), body.into()));
    }
}

// ---------- rate limiting ----------

/// Fixed-window counter per key, in memory. Per process: behind several instances
/// each keeps its own count (move it to the database or a shared cache before scaling out).
#[derive(Default)]
pub struct RateLimiter(Mutex<HashMap<String, (u64, u32)>>);

impl RateLimiter {
    /// `true` when the call is allowed.
    pub fn hit(&self, key: &str, max: u32, window_secs: u64) -> bool {
        let t = now();
        let mut m = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if m.len() > 10_000 {
            m.retain(|_, (start, _)| t.saturating_sub(*start) < 86_400);
        }
        let e = m.entry(key.to_string()).or_insert((t, 0));
        if t.saturating_sub(e.0) >= window_secs {
            *e = (t, 0);
        }
        e.1 += 1;
        e.1 <= max
    }
}

// ---------- passwords ----------

pub async fn hash_password(pw: &str) -> Result<String, String> {
    let pw = pw.to_string();
    tokio::task::spawn_blocking(move || {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default().hash_password(pw.as_bytes(), &salt).map(|h| h.to_string()).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

pub async fn verify_password(hash: &str, pw: &str) -> bool {
    let (hash, pw) = (hash.to_string(), pw.to_string());
    tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash).ok().map(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok()).unwrap_or(false)
    })
    .await
    .unwrap_or(false)
}

/// Verified against when the email is unknown, so a miss costs the same time as a hit.
fn dummy_hash() -> &'static str {
    static H: OnceLock<String> = OnceLock::new();
    H.get_or_init(|| {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default().hash_password(b"no such account", &salt).map(|h| h.to_string()).unwrap_or_default()
    })
}

// ---------- cookies and middleware ----------

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').filter_map(|p| p.trim().split_once('=')).find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
}

fn session_cookie(token: &str, max_age: u64, secure: bool) -> String {
    format!("{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{}", if secure { "; Secure" } else { "" })
}

fn clear_cookie(secure: bool) -> String {
    session_cookie("", 0, secure)
}

fn see_other(to: &str, cookie: Option<String>) -> Response {
    let mut r = Redirect::to(to).into_response();
    if let Some(c) = cookie {
        if let Ok(v) = c.parse() {
            r.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    r
}

async fn resolve(st: &AppState, headers: &HeaderMap) -> Option<Principal> {
    if let Some(auth) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        // A bearer header that is wrong is a failed login, not a reason to try the cookie.
        // Bearer <token>, or HTTP Basic with the token as the password (what the OpenTofu
        // http backend sends: its `username` is ignored).
        let tok: String = if let Some(b) = auth.strip_prefix("Bearer ") {
            b.trim().to_string()
        } else if let Some(b) = auth.strip_prefix("Basic ") {
            let raw = String::from_utf8(tf_crypto::base64_decode(b.trim())?).ok()?;
            raw.split_once(':')?.1.to_string()
        } else {
            return None;
        };
        let t = st.store.find_api_token(&sha256(tok.as_bytes()), now()).await.ok()??;
        return match t.kind.as_str() {
            "org" => Some(Principal { user_id: None, org_id: t.org_id, role: "admin".into(), token_kind: Some("org".into()) }),
            _ => {
                let uid = t.user_id?;
                let role = st.store.member_role(&t.org_id, &uid).await.ok()??;
                Some(Principal { user_id: Some(uid), org_id: t.org_id, role, token_kind: Some("user".into()) })
            }
        };
    }
    let tok = cookie_value(headers, SESSION_COOKIE)?;
    let uid = st.store.session_user(&sha256(tok.as_bytes()), now()).await.ok()??;
    let user = st.store.get_user(&uid).await.ok()?;
    if st.cfg.require_verified && !user.email_verified {
        return None;
    }
    let (org, role) = st.store.orgs_for_user(&uid).await.ok()?.into_iter().next()?;
    Some(Principal { user_id: Some(uid), org_id: org.id, role, token_kind: None })
}

/// Runs on every request: resolves who is calling, if anyone.
pub async fn authenticate(State(st): State<AppState>, mut req: Request, next: Next) -> Response {
    // A browser names its Origin on every cross-site POST. Refuse a foreign (or opaque "null")
    // one before anything else, so login and logout cannot be forged from another site.
    let unsafe_method = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if unsafe_method {
        if let Some(origin) = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
            let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok());
            if host_of(origin).is_none() || host_of(origin) != host {
                return (StatusCode::FORBIDDEN, "cross-origin request refused").into_response();
            }
        }
    }
    if let Some(p) = resolve(&st, req.headers()).await {
        req.extensions_mut().insert(p);
    }
    next.run(req).await
}

fn host_of(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map(|(_, r)| r)?;
    Some(rest.split('/').next().unwrap_or(rest))
}

fn same_origin(h: &HeaderMap) -> bool {
    let Some(host) = h.get(header::HOST).and_then(|v| v.to_str().ok()) else { return false };
    let claimed = h
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .or_else(|| h.get(header::REFERER).and_then(|v| v.to_str().ok()));
    claimed.and_then(host_of).map(|o| o == host).unwrap_or(false)
}

/// Guards the protected routes.
pub async fn require_auth(req: Request, next: Next) -> Response {
    let Some(p) = req.extensions().get::<Principal>().cloned() else {
        if req.uri().path().starts_with("/backend/") {
            // tofu needs the Basic challenge to know it should send credentials
            return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Basic realm=\"platform\"")], "authentication required: set username and password (an API token) on the http backend").into_response();
        }
        return if req.uri().path().starts_with("/api/") {
            (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                Json(json!({ "errors": [{ "status": "401", "title": "authentication required" }] })),
            )
                .into_response()
        } else {
            Redirect::to("/login").into_response()
        };
    };
    let safe = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if p.token_kind.is_none() && !safe && !same_origin(req.headers()) {
        return (StatusCode::FORBIDDEN, "cross-origin request refused").into_response();
    }
    next.run(req).await
}

// ---------- pages ----------

fn form_page(title: &str, intro: &str, error: Option<&str>, fields: &str, button: &str, action: &str, footer: &str) -> String {
    let err = error.map(|e| format!("<div class=\"banner banner-danger err\" role=\"alert\">{}</div>", esc(e))).unwrap_or_default();
    format!(
        "<div class=\"card\" style=\"max-width:420px;margin:48px auto\"><h1 class=\"card-h\">{}</h1><div class=\"card-b\">\
<p class=\"muted\" style=\"margin-top:0\">{}</p>{err}<form method=\"post\" action=\"{action}\" style=\"display:grid;gap:12px\">{fields}\
<button class=\"btn btn-primary btn-lg\" type=\"submit\">{}</button></form><p style=\"margin-bottom:0\">{footer}</p></div></div>",
        esc(title),
        esc(intro),
        esc(button)
    )
}

fn field(id: &str, label: &str, kind: &str, extra: &str) -> String {
    format!("<div class=\"field\"><label for=\"{id}\">{}</label><input id=\"{id}\" name=\"{id}\" type=\"{kind}\" {extra} required></div>", esc(label))
}

fn public(title: &str, body: String) -> Html<String> {
    Html(layout_public(title, &body))
}

pub async fn login_page(Query(q): Query<HashMap<String, String>>) -> Html<String> {
    let notice = q.get("notice").map(|n| format!("<div class=\"banner banner-warning err\" role=\"status\">{}</div>", esc(n))).unwrap_or_default();
    let body = form_page(
        "Sign in",
        "Use the email and password you signed up with.",
        None,
        &(field("email", "Email", "email", "autocomplete=\"username\"") + &field("password", "Password", "password", "autocomplete=\"current-password\"")),
        "Sign in",
        "/login",
        "<a href=\"/signup\">Create an account</a> · <a href=\"/forgot\">Forgot your password?</a>",
    );
    public("Sign in", format!("{notice}{body}"))
}

#[derive(Deserialize)]
pub struct LoginForm {
    email: String,
    password: String,
}

fn render_login_error(msg: &str, status: StatusCode) -> Response {
    let body = form_page(
        "Sign in",
        "Use the email and password you signed up with.",
        Some(msg),
        &(field("email", "Email", "email", "autocomplete=\"username\"") + &field("password", "Password", "password", "autocomplete=\"current-password\"")),
        "Sign in",
        "/login",
        "<a href=\"/signup\">Create an account</a> · <a href=\"/forgot\">Forgot your password?</a>",
    );
    (status, public("Sign in", body)).into_response()
}

async fn start_session(st: &AppState, user: &str) -> Result<String, StoreError> {
    let token = random_token("tfs_");
    st.store.create_session(user, &sha256(token.as_bytes()), now() + st.cfg.session_ttl).await?;
    Ok(session_cookie(&token, st.cfg.session_ttl, st.cfg.cookie_secure))
}

pub async fn login(State(st): State<AppState>, Form(f): Form<LoginForm>) -> Response {
    let email = f.email.trim().to_lowercase();
    if !st.limiter.hit(&format!("login:{email}"), 10, 900) {
        return render_login_error("Too many attempts. Try again in a few minutes.", StatusCode::TOO_MANY_REQUESTS);
    }
    let found = st.store.find_user_by_email(&email).await.ok().flatten();
    let (hash, user) = match &found {
        Some((u, h)) => (h.as_str(), Some(u)),
        None => (dummy_hash(), None),
    };
    let ok = verify_password(hash, &f.password).await;
    let Some(user) = user.filter(|_| ok) else {
        return render_login_error("That email and password do not match.", StatusCode::UNAUTHORIZED);
    };
    if st.cfg.require_verified && !user.email_verified {
        return render_login_error("Verify your email first: open the link we sent when you signed up.", StatusCode::FORBIDDEN);
    }
    match start_session(&st, &user.id).await {
        Ok(cookie) => see_other("/workspaces", Some(cookie)),
        Err(_) => render_login_error("Could not sign you in. Try again.", StatusCode::INTERNAL_SERVER_ERROR),
    }
}

fn signup_form(error: Option<&str>) -> String {
    form_page(
        "Create your account",
        "One account, one organization to start. You can add people later.",
        error,
        &(field("display_name", "Your name", "text", "maxlength=\"80\" autocomplete=\"name\"")
            + &field("email", "Email", "email", "autocomplete=\"username\"")
            + &field("password", "Password (10 characters or more)", "password", "minlength=\"10\" maxlength=\"200\" autocomplete=\"new-password\"")
            + &field("org", "Organization name", "text", "maxlength=\"90\" pattern=\"[A-Za-z0-9_-]+\" title=\"letters, numbers, dash, underscore\"")),
        "Create account",
        "/signup",
        "<a href=\"/login\">I already have an account</a>",
    )
}

pub async fn signup_page() -> Html<String> {
    public("Create account", signup_form(None))
}

#[derive(Deserialize)]
pub struct SignupForm {
    display_name: String,
    email: String,
    password: String,
    org: String,
}

pub async fn signup(State(st): State<AppState>, Form(f): Form<SignupForm>) -> Response {
    let fail = |msg: &str, code: StatusCode| (code, public("Create account", signup_form(Some(msg)))).into_response();
    if !st.limiter.hit("signup:all", 30, 3600) {
        return fail("Too many sign-ups right now. Try again later.", StatusCode::TOO_MANY_REQUESTS);
    }
    let email = f.email.trim().to_lowercase();
    let name = f.display_name.trim();
    if !valid_email(&email) {
        return fail("That does not look like an email address.", StatusCode::UNPROCESSABLE_ENTITY);
    }
    if f.password.chars().count() < MIN_PASSWORD || f.password.len() > 200 {
        return fail("Use a password of at least 10 characters.", StatusCode::UNPROCESSABLE_ENTITY);
    }
    if name.is_empty() || name.chars().count() > 80 {
        return fail("Tell us your name (up to 80 characters).", StatusCode::UNPROCESSABLE_ENTITY);
    }
    if !valid_workspace_name(f.org.trim()) {
        return fail("Organization names use letters, numbers, dash and underscore.", StatusCode::UNPROCESSABLE_ENTITY);
    }
    let Ok(hash) = hash_password(&f.password).await else {
        return fail("Could not create the account. Try again.", StatusCode::INTERNAL_SERVER_ERROR);
    };
    let user = match st.store.create_user(&email, &hash, name).await {
        Ok(u) => u,
        Err(StoreError::Conflict(m)) => return fail(&m, StatusCode::CONFLICT),
        Err(_) => return fail("Could not create the account. Try again.", StatusCode::INTERNAL_SERVER_ERROR),
    };
    if let Err(e) = st.store.create_org(f.org.trim(), &user.id).await {
        let _ = st.store.delete_user(&user.id).await; // never leave an account without an organization
        let (msg, code) = match e {
            StoreError::Conflict(m) | StoreError::Invalid(m) => (m, StatusCode::CONFLICT),
            _ => ("Could not create the account. Try again.".to_string(), StatusCode::INTERNAL_SERVER_ERROR),
        };
        return fail(&msg, code);
    }
    let token = random_token("tfv_");
    if st.store.create_email_token(&user.id, "verify", &sha256(token.as_bytes()), now() + 86_400).await.is_ok() {
        st.mailer.send(
            &email,
            "Confirm your email",
            &format!("Open this link to confirm your email (valid for 24 hours):\n{}/verify?token={token}", st.cfg.base_url),
        );
    }
    if st.cfg.require_verified {
        let body = "<div class=\"card\" style=\"max-width:420px;margin:48px auto\"><h1 class=\"card-h\">Check your email</h1><div class=\"card-b\">We sent a confirmation link. Open it, then sign in.</div></div>".to_string();
        return public("Check your email", body).into_response();
    }
    match start_session(&st, &user.id).await {
        Ok(cookie) => see_other("/workspaces", Some(cookie)),
        Err(_) => see_other("/login", None),
    }
}

pub async fn verify(State(st): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let tok = q.get("token").map(String::as_str).unwrap_or("");
    let user = st.store.consume_email_token(&sha256(tok.as_bytes()), "verify", now()).await.ok().flatten();
    let (title, msg, code) = match user {
        Some(u) if st.store.mark_email_verified(&u).await.is_ok() => ("Email confirmed", "Your email is confirmed. You can sign in.", StatusCode::OK),
        _ => ("Link not valid", "That link is invalid, expired or already used.", StatusCode::BAD_REQUEST),
    };
    let body = format!("<div class=\"card\" style=\"max-width:420px;margin:48px auto\"><h1 class=\"card-h\">{title}</h1><div class=\"card-b\"><p>{msg}</p><a href=\"/login\">Sign in</a></div></div>");
    (code, public(title, body)).into_response()
}

pub async fn forgot_page() -> Html<String> {
    public(
        "Reset password",
        form_page("Reset your password", "We will email you a link if the account exists.", None, &field("email", "Email", "email", "autocomplete=\"username\""), "Send link", "/forgot", "<a href=\"/login\">Back to sign in</a>"),
    )
}

#[derive(Deserialize)]
pub struct ForgotForm {
    email: String,
}

pub async fn forgot(State(st): State<AppState>, Form(f): Form<ForgotForm>) -> Response {
    let email = f.email.trim().to_lowercase();
    if !st.limiter.hit(&format!("forgot:{email}"), 5, 3600) {
        return (StatusCode::TOO_MANY_REQUESTS, "Too many requests. Try again later.").into_response();
    }
    if let Ok(Some((user, _))) = st.store.find_user_by_email(&email).await {
        let token = random_token("tfr_");
        if st.store.create_email_token(&user.id, "reset", &sha256(token.as_bytes()), now() + 3600).await.is_ok() {
            st.mailer.send(
                &user.email,
                "Reset your password",
                &format!("Open this link to choose a new password (valid for 1 hour):\n{}/reset?token={token}\nIf you did not ask for this, ignore this message.", st.cfg.base_url),
            );
        }
    }
    // Same answer whether or not the account exists.
    let body = "<div class=\"card\" style=\"max-width:420px;margin:48px auto\"><h1 class=\"card-h\">Check your email</h1><div class=\"card-b\">If that account exists, we sent a reset link.</div></div>".to_string();
    public("Check your email", body).into_response()
}

pub async fn reset_page(Query(q): Query<HashMap<String, String>>) -> Html<String> {
    let tok = q.get("token").cloned().unwrap_or_default();
    let fields = format!(
        "<input type=\"hidden\" name=\"token\" value=\"{}\">{}",
        esc(&tok),
        field("password", "New password (10 characters or more)", "password", "minlength=\"10\" maxlength=\"200\" autocomplete=\"new-password\"")
    );
    public("Choose a new password", form_page("Choose a new password", "Signing in elsewhere will be undone.", None, &fields, "Save password", "/reset", ""))
}

#[derive(Deserialize)]
pub struct ResetForm {
    token: String,
    password: String,
}

pub async fn reset(State(st): State<AppState>, Form(f): Form<ResetForm>) -> Response {
    if f.password.chars().count() < MIN_PASSWORD || f.password.len() > 200 {
        return (StatusCode::UNPROCESSABLE_ENTITY, "Use a password of at least 10 characters.").into_response();
    }
    let Some(user) = st.store.consume_email_token(&sha256(f.token.as_bytes()), "reset", now()).await.ok().flatten() else {
        return (StatusCode::BAD_REQUEST, "That link is invalid, expired or already used.").into_response();
    };
    let Ok(hash) = hash_password(&f.password).await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "Could not save the password.").into_response();
    };
    if st.store.set_password(&user, &hash).await.is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, "Could not save the password.").into_response();
    }
    let _ = st.store.delete_user_sessions(&user).await; // a reset signs everything out
    see_other("/login?notice=Password+changed.+Sign+in+with+the+new+one.", None)
}

pub async fn logout(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(tok) = cookie_value(&headers, SESSION_COOKIE) {
        let _ = st.store.delete_session(&sha256(tok.as_bytes())).await;
    }
    see_other("/login", Some(clear_cookie(st.cfg.cookie_secure)))
}

pub async fn logout_all(State(st): State<AppState>, Extension(p): Extension<Principal>) -> Response {
    if let (Some(uid), None) = (&p.user_id, &p.token_kind) {
        let _ = st.store.delete_user_sessions(uid).await;
    }
    see_other("/login", Some(clear_cookie(st.cfg.cookie_secure)))
}

pub async fn account_page(State(st): State<AppState>, Extension(p): Extension<Principal>) -> Result<Html<String>, crate::web::WebError> {
    let Some(uid) = p.user_id.clone().filter(|_| p.token_kind.is_none()) else {
        return Err(StoreError::Forbidden("sign in with a browser session to manage your account".into()).into());
    };
    let user = st.store.get_user(&uid).await?;
    let body = format!(
        "<div class=\"page-header\"><div><h1>Account</h1></div></div>\
<div class=\"card\"><div class=\"card-h\">Profile</div><div class=\"card-b\"><dl class=\"kv\"><dt>Name</dt><dd>{}</dd><dt>Email</dt><dd>{} {}</dd><dt>Role</dt><dd>{}</dd></dl></div></div>\
<div class=\"card\" style=\"margin-top:16px\"><div class=\"card-h\">Sessions</div><div class=\"card-b\"><form method=\"post\" action=\"/logout-all\"><button class=\"btn btn-secondary\" type=\"submit\">Sign out everywhere</button></form></div></div>\
<div class=\"card\" style=\"margin-top:16px\"><div class=\"card-h\">Delete account</div><div class=\"card-b\">\
<p class=\"muted\" style=\"margin-top:0\">This permanently deletes your account, your tokens and every organization where you are the only member, with its workspaces, runs and state. It cannot be undone.</p>\
<form method=\"post\" action=\"/account/delete\" style=\"display:grid;gap:12px;max-width:360px\">{}<button class=\"btn btn-danger\" type=\"submit\">Delete my account</button></form></div></div>",
        esc(&user.display_name),
        esc(&user.email),
        if user.email_verified { "<span class=\"badge badge-success\" data-icon=\"✓\">verified</span>" } else { "<span class=\"badge badge-warning\" data-icon=\"!\">not verified</span>" },
        esc(&p.role),
        field("password", "Confirm with your password", "password", "autocomplete=\"current-password\"")
    );
    crate::web::page(&st, &p, "Account", "account", body, false).await
}

#[derive(Deserialize)]
pub struct DeleteForm {
    password: String,
}

pub async fn delete_account(State(st): State<AppState>, Extension(p): Extension<Principal>, Form(f): Form<DeleteForm>) -> Result<Response, crate::web::WebError> {
    let Some(uid) = p.user_id.clone().filter(|_| p.token_kind.is_none()) else {
        return Err(StoreError::Forbidden("sign in with a browser session to delete your account".into()).into());
    };
    let user = st.store.get_user(&uid).await?;
    let (_, hash) = st.store.find_user_by_email(&user.email).await?.ok_or_else(|| StoreError::NotFound("user not found".into()))?;
    if !st.limiter.hit(&format!("delete:{uid}"), 5, 900) || !verify_password(&hash, &f.password).await {
        return Err(StoreError::Forbidden("that password is not right".into()).into());
    }
    st.store.delete_user(&uid).await?;
    Ok(see_other("/login?notice=Your+account+was+deleted.", Some(clear_cookie(st.cfg.cookie_secure))))
}

// ---------- API tokens ----------

fn token_allowed(p: &Principal, kind: &str) -> Result<(), StoreError> {
    if p.token_kind.is_some() {
        return Err(StoreError::Forbidden("tokens can only be managed from a signed-in browser session".into()));
    }
    let need = if kind == "org" { Action::ManageTokens } else { Action::Read };
    if p.can(need) { Ok(()) } else { Err(StoreError::Forbidden("your role does not allow that".into())) }
}

async fn mint(st: &AppState, p: &Principal, kind: &str, description: &str, days: Option<u64>) -> Result<(ApiToken, String), StoreError> {
    token_allowed(p, kind)?;
    let secret = random_token("tfk_");
    let desc: String = description.trim().chars().take(100).collect();
    let tok = st
        .store
        .create_api_token(kind, if kind == "user" { p.user_id.as_deref() } else { None }, &p.org_id, &desc, &sha256(secret.as_bytes()), days.map(|d| now() + d * 86_400))
        .await?;
    Ok((tok, secret))
}

async fn visible_tokens(st: &AppState, p: &Principal) -> Result<Vec<ApiToken>, StoreError> {
    let all = st.store.list_api_tokens(&p.org_id).await?;
    Ok(if p.can(Action::ManageTokens) { all } else { all.into_iter().filter(|t| t.user_id == p.user_id).collect() })
}

pub async fn tokens_page(State(st): State<AppState>, Extension(p): Extension<Principal>) -> Result<Html<String>, crate::web::WebError> {
    token_allowed(&p, "user")?;
    tokens_view(&st, &p, None).await
}

async fn tokens_view(st: &AppState, p: &Principal, created: Option<(&ApiToken, &str)>) -> Result<Html<String>, crate::web::WebError> {
    let list = visible_tokens(st, p).await?;
    let mut rows = String::new();
    for t in &list {
        rows.push_str(&format!(
            "<tr><td>{}</td><td><span class=\"chip\">{}</span></td><td class=\"mono muted\">{}</td><td><form class=\"inline\" method=\"post\" action=\"/tokens/{}/delete\"><button class=\"btn btn-ghost btn-sm\" type=\"submit\">Revoke</button></form></td></tr>",
            esc(&t.description),
            esc(&t.kind),
            t.expires_at.map(|e| format!("expires in {}d", e.saturating_sub(now()) / 86_400)).unwrap_or_else(|| "no expiry".into()),
            esc(&t.id)
        ));
    }
    let table = if list.is_empty() {
        "<div class=\"empty\">No tokens yet.</div>".to_string()
    } else {
        format!("<div class=\"table-wrap\"><table class=\"table\"><thead><tr><th>Description</th><th>Kind</th><th>Expiry</th><th></th></tr></thead><tbody>{rows}</tbody></table></div>")
    };
    let shown = created
        .map(|(_, secret)| format!("<div class=\"banner banner-warning\" role=\"status\"><span>Copy this token now. It is shown only once: <span class=\"mono\">{}</span></span></div>", esc(secret)))
        .unwrap_or_default();
    let org_opt = if p.can(Action::ManageTokens) { "<option value=\"org\">Organization (cannot plan or apply)</option>" } else { "" };
    let body = format!(
        "<div class=\"page-header\"><div><h1>API tokens</h1></div></div>{shown}\
<form method=\"post\" action=\"/tokens\" class=\"toolbar\"><div class=\"field\"><label class=\"sr-only\" for=\"description\">Description</label><input id=\"description\" name=\"description\" placeholder=\"What is this token for?\" maxlength=\"100\" required></div>\
<label class=\"sr-only\" for=\"kind\">Kind</label><select id=\"kind\" name=\"kind\"><option value=\"user\">Personal (acts as you)</option>{org_opt}</select>\
<label class=\"sr-only\" for=\"days\">Expires</label><select id=\"days\" name=\"days\"><option value=\"30\">30 days</option><option value=\"90\">90 days</option><option value=\"365\">1 year</option></select>\
<button class=\"btn btn-primary\" type=\"submit\">Create token</button></form>{table}"
    );
    crate::web::page(st, p, "API tokens", "tokens", body, false).await
}

#[derive(Deserialize)]
pub struct NewTokenForm {
    description: String,
    kind: String,
    days: Option<u64>,
}

pub async fn create_token_form(State(st): State<AppState>, Extension(p): Extension<Principal>, Form(f): Form<NewTokenForm>) -> Result<Html<String>, crate::web::WebError> {
    let (tok, secret) = mint(&st, &p, &f.kind, &f.description, Some(f.days.unwrap_or(30).clamp(1, 365))).await?;
    tokens_view(&st, &p, Some((&tok, &secret))).await
}

pub async fn delete_token_form(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>) -> Result<Redirect, crate::web::WebError> {
    revoke(&st, &p, &id).await?;
    Ok(Redirect::to("/tokens"))
}

async fn revoke(st: &AppState, p: &Principal, id: &str) -> Result<(), StoreError> {
    token_allowed(p, "user")?;
    let mine = visible_tokens(st, p).await?.into_iter().find(|t| t.id == id).ok_or_else(|| StoreError::NotFound("token not found".into()))?;
    st.store.delete_api_token(&p.org_id, &mine.id).await
}

#[derive(Deserialize)]
pub struct ApiNewToken {
    pub description: String,
    #[serde(default = "user_kind")]
    pub kind: String,
    #[serde(default)]
    pub expires_in_days: Option<u64>,
}

fn user_kind() -> String {
    "user".into()
}

pub async fn api_create_token(State(st): State<AppState>, Extension(p): Extension<Principal>, crate::api::ApiJson(b): crate::api::ApiJson<ApiNewToken>) -> Result<(StatusCode, Json<serde_json::Value>), crate::api::ApiError> {
    let (tok, secret) = mint(&st, &p, &b.kind, &b.description, Some(b.expires_in_days.unwrap_or(30).clamp(1, 365))).await?;
    let res = crate::jsonapi::resource("authentication-tokens", &tok, &["user_id", "org_id"], &[("user", "users", tok.user_id.as_deref()), ("organization", "organizations", Some(&tok.org_id))], "/api/v2/tokens");
    Ok((StatusCode::CREATED, Json(json!({ "data": res, "meta": { "secret": secret } }))))
}

pub async fn api_list_tokens(State(st): State<AppState>, Extension(p): Extension<Principal>) -> Result<Json<serde_json::Value>, crate::api::ApiError> {
    token_allowed(&p, "user")?;
    let list = visible_tokens(&st, &p).await?;
    let res: Vec<_> = list.iter().map(|t| crate::jsonapi::resource("authentication-tokens", t, &["user_id", "org_id"], &[("user", "users", t.user_id.as_deref()), ("organization", "organizations", Some(&t.org_id))], "/api/v2/tokens")).collect();
    Ok(Json(json!({ "data": res })))
}

pub async fn api_delete_token(State(st): State<AppState>, Extension(p): Extension<Principal>, Path(id): Path<String>) -> Result<StatusCode, crate::api::ApiError> {
    revoke(&st, &p, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_counts_per_key_and_window() {
        let l = RateLimiter::default();
        assert!(l.hit("a", 2, 100) && l.hit("a", 2, 100));
        assert!(!l.hit("a", 2, 100), "third hit in the window is refused");
        assert!(l.hit("b", 2, 100), "other keys are independent");
        assert!(l.hit("c", 1, 0) && l.hit("c", 1, 0), "a zero-length window always resets");
    }

    #[test]
    fn cookies_parse_and_format() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "a=1; tf_session=abc; b=2".parse().unwrap());
        assert_eq!(cookie_value(&h, "tf_session").as_deref(), Some("abc"));
        assert_eq!(cookie_value(&h, "missing"), None);
        let c = session_cookie("t", 60, true);
        assert!(c.contains("HttpOnly") && c.contains("SameSite=Lax") && c.contains("Secure"));
        assert!(!session_cookie("t", 60, false).contains("Secure"));
        assert!(clear_cookie(false).contains("Max-Age=0"));
    }

    #[test]
    fn same_origin_rules() {
        let mut h = HeaderMap::new();
        assert!(!same_origin(&h), "no host, no origin");
        h.insert(header::HOST, "app.example.com".parse().unwrap());
        assert!(!same_origin(&h), "no Origin or Referer is refused");
        h.insert(header::ORIGIN, "https://app.example.com".parse().unwrap());
        assert!(same_origin(&h));
        h.insert(header::ORIGIN, "https://evil.example.net".parse().unwrap());
        assert!(!same_origin(&h));
        h.remove(header::ORIGIN);
        h.insert(header::REFERER, "https://app.example.com/workspaces".parse().unwrap());
        assert!(same_origin(&h), "Referer is the fallback");
    }

    #[tokio::test]
    async fn passwords_hash_and_verify() {
        let h = hash_password("correct horse battery").await.unwrap();
        assert!(h.starts_with("$argon2"));
        assert!(verify_password(&h, "correct horse battery").await);
        assert!(!verify_password(&h, "wrong").await);
        assert!(!verify_password("not a hash", "x").await);
        assert_ne!(h, hash_password("correct horse battery").await.unwrap(), "salted");
    }
}
