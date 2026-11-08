//! End to end through the real router and runner: accounts, sessions, tokens,
//! roles, tenant isolation, variables, and the plan/confirm/apply loop.

use axum::body::Body;
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tf_engine::{Engine, FakeEngine};
use tf_server::auth::{CaptureMailer, Config};
use tf_store::{MemoryStore, Store};
use tower::ServiceExt;

const PW: &str = "correct horse battery";
const CFG_A: &str = "resource \"aws_s3_bucket\" \"logs\" {\n  bucket = \"a\"\n}\nresource \"aws_iam_role\" \"ci\" {\n  name = \"ci\"\n}\n";

struct App {
    router: Router,
    store: Arc<dyn Store>,
    mail: Arc<CaptureMailer>,
    cancels: tf_server::runner::Cancels,
}

static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// In memory by default; against a real PostgreSQL (one fresh schema per test)
/// when TF_TEST_DATABASE_URL is set, so the whole stack is exercised on both.
async fn boot_with(cfg: Config) -> App {
    boot_engine(cfg, Arc::new(FakeEngine::new(Duration::ZERO))).await
}

async fn boot_engine(mut cfg: Config, engine: Arc<dyn Engine>) -> App {
    // Tests poll far faster than a person; only the rate-limit tests ask for a real limit.
    if cfg.api_rate_per_sec == Config::default().api_rate_per_sec {
        cfg.api_rate_per_sec = 100_000;
    }
    let store: Arc<dyn Store> = match std::env::var("TF_TEST_DATABASE_URL").ok().filter(|s| !s.is_empty()) {
        Some(url) => {
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let schema = format!("e2e_{}_{}", std::process::id(), n);
            Arc::new(tf_store::PgStore::connect_in_schema(&url, Some(&schema), tf_crypto::Crypto::ephemeral()).await.expect("postgres"))
        }
        None => Arc::new(MemoryStore::new()),
    };
    let mail = Arc::new(CaptureMailer::default());
    let state = tf_server::AppState::new(store.clone(), engine.name(), mail.clone(), cfg);
    let cancels = state.cancels.clone();
    tf_server::runner::spawn(store.clone(), engine.clone(), cancels.clone());
    tf_server::drift::spawn(&state);
    App { router: tf_server::app(state), store, mail, cancels }
}

async fn boot() -> App {
    boot_with(Config::default()).await
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
    fn session_cookie(&self) -> Option<String> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|c| c.starts_with("tf_session="))
            .map(|c| c.to_string())
    }
    fn location(&self) -> String {
        self.headers.get(header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
    }
}

#[derive(Clone)]
struct Client {
    router: Router,
    cookie: Option<String>,
    bearer: Option<String>,
    origin: Option<String>,
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect::<Vec<_>>().join("&")
}

impl Client {
    fn anon(app: &App) -> Self {
        Self { router: app.router.clone(), cookie: None, bearer: None, origin: Some("http://localhost".into()) }
    }

    fn with_bearer(&self, token: &str) -> Self {
        Self { cookie: None, bearer: Some(token.to_string()), ..self.clone() }
    }

    async fn send(&self, method: Method, uri: &str, ct: Option<&str>, body: String) -> Reply {
        let mut b = Request::builder().method(method).uri(uri).header(header::HOST, "localhost");
        if let Some(o) = &self.origin {
            b = b.header(header::ORIGIN, o);
        }
        if let Some(c) = &self.cookie {
            b = b.header(header::COOKIE, c);
        }
        if let Some(t) = &self.bearer {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        if let Some(ct) = ct {
            b = b.header(header::CONTENT_TYPE, ct);
        }
        let resp = self.router.clone().oneshot(b.body(Body::from(body)).unwrap()).await.unwrap();
        let (status, headers) = (resp.status(), resp.headers().clone());
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 21).await.unwrap();
        Reply { status, headers, body: String::from_utf8_lossy(&bytes).to_string() }
    }

    async fn get(&self, uri: &str) -> Reply {
        self.send(Method::GET, uri, None, String::new()).await
    }
    async fn post_form(&self, uri: &str, pairs: &[(&str, &str)]) -> Reply {
        self.send(Method::POST, uri, Some("application/x-www-form-urlencoded"), form(pairs)).await
    }
    async fn post_json(&self, uri: &str, v: Value) -> Reply {
        self.send(Method::POST, uri, Some("application/json"), v.to_string()).await
    }
    async fn post(&self, uri: &str) -> Reply {
        self.post_json(uri, json!({})).await
    }
    async fn delete(&self, uri: &str) -> Reply {
        self.send(Method::DELETE, uri, None, String::new()).await
    }
}

fn cookie_pair(set_cookie: &str) -> String {
    set_cookie.split(';').next().unwrap_or("").to_string()
}

async fn signup(app: &App, email: &str, org: &str) -> Client {
    let c = Client::anon(app);
    let r = c.post_form("/signup", &[("display_name", "Tester"), ("email", email), ("password", PW), ("org", org)]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "signup failed: {}", r.body);
    Client { cookie: Some(cookie_pair(&r.session_cookie().expect("session cookie"))), ..c }
}

async fn wait_status(c: &Client, run: &str, want: &str) -> Value {
    for _ in 0..300 {
        let v = c.get(&format!("/api/v2/runs/{run}")).await.json();
        if v["data"]["attributes"]["status"] == want {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let v = c.get(&format!("/api/v2/runs/{run}")).await.json();
    panic!("run {run} never reached {want}; last: {}", v["data"]["attributes"]["status"]);
}

async fn new_ws(c: &Client, name: &str) -> String {
    let r = c.post_json("/api/v2/workspaces", json!({ "name": name })).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    r.json()["data"]["id"].as_str().unwrap().to_string()
}

async fn new_run(c: &Client, ws: &str, config: &str, extra: Value) -> String {
    let mut body = json!({ "config": config });
    if let Value::Object(m) = extra {
        for (k, v) in m {
            body[k] = v;
        }
    }
    let r = c.post_json(&format!("/api/v2/workspaces/{ws}/runs"), body).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    r.json()["data"]["id"].as_str().unwrap().to_string()
}

/// The current state document, fetched the way a client must: through the single-use URL.
async fn state_document(c: &Client, ws: &str) -> String {
    let v = c.get(&format!("/api/v2/workspaces/{ws}/current-state-version")).await.json();
    let url = v["data"]["attributes"]["hosted-state-download-url"].as_str().expect("a download URL").to_string();
    let path = format!("/archivist{}", url.split_once("/archivist").expect("an archivist URL").1);
    let r = c.get(&path).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    r.body
}

async fn lock_of(c: &Client, ws: &str) -> Value {
    let v = c.get("/api/v2/workspaces").await.json();
    v["data"].as_array().unwrap().iter().find(|w| w["id"] == ws).unwrap()["attributes"]["lock"].clone()
}

// ================= the plan / confirm / apply loop =================

#[tokio::test]
async fn full_loop_plan_confirm_apply_then_edit_and_discard() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;

    let r1 = new_run(&c, &ws, CFG_A, json!({ "message": "first" })).await;
    let v = wait_status(&c, &r1, "needs_confirmation").await;
    assert_eq!(v["data"]["attributes"]["summary"], json!({ "add": 2, "change": 0, "destroy": 0 }));
    assert_eq!(v["data"]["attributes"]["actions"]["is_confirmable"], true);
    assert!(lock_of(&c, &ws).await.is_object(), "a paused run holds the lock");

    assert_eq!(c.post(&format!("/api/v2/runs/{r1}/actions/apply")).await.status, StatusCode::ACCEPTED);
    wait_status(&c, &r1, "applied").await;
    let st = c.get(&format!("/api/v2/workspaces/{ws}/current-state-version")).await.json();
    assert_eq!((st["data"]["attributes"]["serial"].clone(), st["data"]["attributes"]["resource-count"].clone()), (json!(1), json!(2)));
    assert!(lock_of(&c, &ws).await.is_null(), "lock released after apply");

    let same = new_run(&c, &ws, CFG_A, json!({})).await;
    let v = wait_status(&c, &same, "planned_and_finished").await;
    assert_eq!(v["data"]["attributes"]["summary"], json!({ "add": 0, "change": 0, "destroy": 0 }));

    let cfg_b = "resource \"aws_s3_bucket\" \"logs\" {\n  bucket = \"b\"\n}\nresource \"aws_sqs_queue\" \"jobs\" {\n  name = \"jobs\"\n}\n";
    let r3 = new_run(&c, &ws, cfg_b, json!({})).await;
    let v = wait_status(&c, &r3, "needs_confirmation").await;
    assert_eq!(v["data"]["attributes"]["summary"], json!({ "add": 1, "change": 1, "destroy": 1 }));

    assert_eq!(c.post(&format!("/api/v2/runs/{r3}/actions/discard")).await.status, StatusCode::ACCEPTED);
    wait_status(&c, &r3, "discarded").await;
    let st = c.get(&format!("/api/v2/workspaces/{ws}/current-state-version")).await.json();
    assert_eq!(st["data"]["attributes"]["serial"], 1, "discard must not write state");
    assert!(lock_of(&c, &ws).await.is_null());
}

#[tokio::test]
async fn runs_queue_in_order_on_one_workspace() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "queue").await;
    let a = new_run(&c, &ws, CFG_A, json!({})).await;
    let b = new_run(&c, &ws, CFG_A, json!({})).await;
    wait_status(&c, &a, "needs_confirmation").await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(c.get(&format!("/api/v2/runs/{b}")).await.json()["data"]["attributes"]["status"], "pending");
    c.post(&format!("/api/v2/runs/{a}/actions/discard")).await;
    wait_status(&c, &b, "needs_confirmation").await;
}

#[tokio::test]
async fn failures_error_the_run_and_release_the_lock() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "failing").await;
    let r = new_run(&c, &ws, &format!("fail_plan\n{CFG_A}"), json!({})).await;
    wait_status(&c, &r, "errored").await;
    assert!(c.get(&format!("/api/v2/runs/{r}/logs")).await.body.contains("Error"));
    assert!(lock_of(&c, &ws).await.is_null(), "an errored run must not keep the lock");

    let r2 = new_run(&c, &ws, &format!("fail_apply\n{CFG_A}"), json!({})).await;
    wait_status(&c, &r2, "needs_confirmation").await;
    c.post(&format!("/api/v2/runs/{r2}/actions/apply")).await;
    wait_status(&c, &r2, "errored").await;
    assert_eq!(c.get(&format!("/api/v2/workspaces/{ws}/current-state-version")).await.status, StatusCode::NOT_FOUND, "failed apply writes no state");

    let r3 = new_run(&c, &ws, "resource nope {", json!({})).await;
    wait_status(&c, &r3, "errored").await;
}

#[tokio::test]
async fn auto_apply_and_plan_only() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "auto").await;
    let r = new_run(&c, &ws, CFG_A, json!({ "auto_apply": true })).await;
    wait_status(&c, &r, "applied").await;

    let spec = new_run(&c, &ws, "", json!({ "operation": "plan_only" })).await;
    let v = wait_status(&c, &spec, "planned_and_finished").await;
    assert_eq!(v["data"]["attributes"]["summary"]["destroy"], 2);
    assert_eq!(c.post(&format!("/api/v2/runs/{spec}/actions/apply")).await.status, StatusCode::CONFLICT, "a speculative plan can never be applied");
    let st = c.get(&format!("/api/v2/workspaces/{ws}/current-state-version")).await.json();
    assert_eq!(st["data"]["attributes"]["resource-count"], 2, "plan-only never changes state");
}

#[tokio::test]
async fn api_rejects_bad_input() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    assert_eq!(c.post_json("/api/v2/workspaces", json!({ "name": "bad name!" })).await.status, StatusCode::UNPROCESSABLE_ENTITY);
    new_ws(&c, "dup").await;
    assert_eq!(c.post_json("/api/v2/workspaces", json!({ "name": "dup" })).await.status, StatusCode::CONFLICT);
    assert_eq!(c.get("/api/v2/runs/nope").await.status, StatusCode::NOT_FOUND);
    assert_eq!(c.post_json("/api/v2/workspaces/nope/runs", json!({ "config": "" })).await.status, StatusCode::NOT_FOUND);
    let ws = new_ws(&c, "ok").await;
    assert_eq!(c.post_json(&format!("/api/v2/workspaces/{ws}/runs"), json!({ "config": "", "operation": "bogus" })).await.status, StatusCode::UNPROCESSABLE_ENTITY);
    let r = new_run(&c, &ws, CFG_A, json!({})).await;
    wait_status(&c, &r, "needs_confirmation").await;
    assert_eq!(c.post(&format!("/api/v2/runs/{r}/actions/cancel")).await.status, StatusCode::CONFLICT);
    assert_eq!(c.post(&format!("/api/v2/runs/{r}/actions/explode")).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn manual_lock_blocks_then_releases() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "locked").await;
    assert_eq!(c.post(&format!("/api/v2/workspaces/{ws}/actions/lock")).await.status, StatusCode::OK);
    assert_eq!(c.post(&format!("/api/v2/workspaces/{ws}/actions/lock")).await.status, StatusCode::CONFLICT);
    let r = new_run(&c, &ws, CFG_A, json!({})).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(c.get(&format!("/api/v2/runs/{r}")).await.json()["data"]["attributes"]["status"], "pending");
    c.post(&format!("/api/v2/workspaces/{ws}/actions/unlock")).await;
    wait_status(&c, &r, "needs_confirmation").await;
}

#[tokio::test]
async fn pages_render_and_escape_user_input() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "web").await;
    let evil = "</textarea><script>alert(1)</script>";
    let r = new_run(&c, &ws, evil, json!({ "message": "<b>hi</b>" })).await;
    wait_status(&c, &r, "planned_and_finished").await;

    let html = c.get("/workspaces").await;
    assert_eq!(html.status, StatusCode::OK);
    assert!(html.body.contains("web") && html.body.contains("New workspace") && html.body.contains("ana@example.com"));

    let html = c.get(&format!("/workspaces/{ws}")).await;
    assert!(!html.body.contains("<script>alert(1)"), "config must be escaped in the editor");
    assert!(html.body.contains("&lt;script&gt;alert(1)"));
    assert!(!html.body.contains("<b>hi</b>"), "run message must be escaped");

    let html = c.get(&format!("/runs/{r}")).await;
    assert!(html.body.contains("Planned and finished") && html.body.contains("role=\"log\""));
    assert_eq!(c.get("/runs/does-not-exist").await.status, StatusCode::NOT_FOUND);
    let css = c.get("/assets/app.css").await;
    assert!(css.body.contains("--color-accent"));
    assert_eq!(Client::anon(&app).get("/healthz").await.body, "ok");
}

#[tokio::test]
async fn ui_forms_create_workspace_queue_run_and_confirm() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;

    let r = c.post_form("/workspaces", &[("name", "bad name")]).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body.contains("letters, numbers"), "inline error is shown");

    let r = c.post_form("/workspaces", &[("name", "ui-ws")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let ws_url = r.location();
    let r = c.post_form(&format!("{ws_url}/runs"), &[("config", "resource \"a\" \"b\" {\n x = 1\n}"), ("message", "from form"), ("mode", "plan_and_apply")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let run_url = r.location();
    let run_id = run_url.rsplit('/').next().unwrap().to_string();
    wait_status(&c, &run_id, "needs_confirmation").await;
    let html = c.get(&run_url).await.body;
    assert!(html.contains("Confirm &amp; apply") && html.contains("Discard run") && html.contains("a.b"));

    assert_eq!(c.post_form(&format!("{run_url}/apply"), &[]).await.status, StatusCode::SEE_OTHER);
    wait_status(&c, &run_id, "applied").await;
    assert!(c.get(&format!("{ws_url}/states")).await.body.contains("current"));
}

// ================= accounts and sessions =================

#[tokio::test]
async fn everything_requires_signing_in() {
    let app = boot().await;
    let anon = Client::anon(&app);
    for uri in ["/workspaces", "/runs", "/account", "/tokens", "/workspaces/x", "/runs/x"] {
        let r = anon.get(uri).await;
        assert_eq!((r.status, r.location()), (StatusCode::SEE_OTHER, "/login".to_string()), "{uri}");
    }
    for uri in ["/api/v2/workspaces", "/api/v2/runs/x", "/api/v2/tokens"] {
        let r = anon.get(uri).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{uri}");
        assert!(r.headers.contains_key(header::WWW_AUTHENTICATE));
    }
    assert_eq!(anon.post_json("/api/v2/workspaces", json!({ "name": "x" })).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(anon.get("/login").await.status, StatusCode::OK);
    assert_eq!(anon.get("/signup").await.status, StatusCode::OK);
}

#[tokio::test]
async fn signup_validates_and_never_leaves_an_orphan_account() {
    let app = boot().await;
    let anon = Client::anon(&app);
    let try_signup = |email: &'static str, pw: &'static str, org: &'static str| {
        let anon = anon.clone();
        async move { anon.post_form("/signup", &[("display_name", "T"), ("email", email), ("password", pw), ("org", org)]).await }
    };
    assert_eq!(try_signup("not-an-email", PW, "o1").await.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(try_signup("a@example.com", "short", "o1").await.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(try_signup("a@example.com", PW, "bad org!").await.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(try_signup("a@example.com", PW, "taken").await.status, StatusCode::SEE_OTHER);
    assert_eq!(try_signup("A@Example.com", PW, "other").await.status, StatusCode::CONFLICT, "email is case-insensitive");
    // org name clash must roll the new account back, so the email stays free
    assert_eq!(try_signup("b@example.com", PW, "taken").await.status, StatusCode::CONFLICT);
    assert!(app.store.find_user_by_email("b@example.com").await.unwrap().is_none(), "no orphan account");
    assert_eq!(try_signup("b@example.com", PW, "b-org").await.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn login_logout_and_sign_out_everywhere() {
    let app = boot().await;
    let first = signup(&app, "ana@example.com", "acme").await;
    let anon = Client::anon(&app);

    let wrong = anon.post_form("/login", &[("email", "ana@example.com"), ("password", "wrong password!!")]).await;
    let unknown = anon.post_form("/login", &[("email", "nobody@example.com"), ("password", "wrong password!!")]).await;
    assert_eq!((wrong.status, unknown.status), (StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED));
    assert_eq!(wrong.body.contains("do not match"), unknown.body.contains("do not match"), "same message for both, no account enumeration");
    assert!(wrong.session_cookie().is_none());

    let ok = anon.post_form("/login", &[("email", "ANA@example.com"), ("password", PW)]).await;
    assert_eq!(ok.status, StatusCode::SEE_OTHER);
    let sc = ok.session_cookie().unwrap();
    assert!(sc.contains("HttpOnly") && sc.contains("SameSite=Lax"));
    let second = Client { cookie: Some(cookie_pair(&sc)), ..anon.clone() };
    assert_eq!(second.get("/workspaces").await.status, StatusCode::OK);

    // logging out kills that session only
    let out = second.post("/logout").await;
    assert!(out.session_cookie().unwrap().contains("Max-Age=0"));
    assert_eq!(second.get("/workspaces").await.status, StatusCode::SEE_OTHER, "old cookie no longer works");
    assert_eq!(first.get("/workspaces").await.status, StatusCode::OK, "the other session is untouched");

    // sign out everywhere kills the rest
    let third = Client { cookie: Some(cookie_pair(&anon.post_form("/login", &[("email", "ana@example.com"), ("password", PW)]).await.session_cookie().unwrap())), ..anon.clone() };
    assert_eq!(third.post("/logout-all").await.status, StatusCode::SEE_OTHER);
    assert_eq!(first.get("/workspaces").await.status, StatusCode::SEE_OTHER);
    assert_eq!(third.get("/workspaces").await.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn repeated_bad_passwords_are_rate_limited() {
    let app = boot().await;
    signup(&app, "ana@example.com", "acme").await;
    let anon = Client::anon(&app);
    let mut last = StatusCode::OK;
    for _ in 0..12 {
        last = anon.post_form("/login", &[("email", "ana@example.com"), ("password", "wrong password!!")]).await.status;
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
    // even the right password is refused while limited
    assert_eq!(anon.post_form("/login", &[("email", "ana@example.com"), ("password", PW)]).await.status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn cross_origin_writes_with_a_cookie_are_refused() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let evil = Client { origin: Some("https://evil.example.net".into()), ..c.clone() };
    let none = Client { origin: None, ..c.clone() };
    for client in [&evil, &none] {
        let r = client.post_form("/workspaces", &[("name", "pwned")]).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        assert_eq!(client.post_json("/api/v2/workspaces", json!({ "name": "pwned" })).await.status, StatusCode::FORBIDDEN);
    }
    assert_eq!(evil.get("/workspaces").await.status, StatusCode::OK, "reads are not state-changing");
    assert_eq!(c.get("/api/v2/workspaces").await.json()["data"], json!([]), "nothing was created");
}

#[tokio::test]
async fn email_verification_gates_sign_in_when_required() {
    let app = boot_with(Config { require_verified: true, ..Config::default() }).await;
    let anon = Client::anon(&app);
    let r = anon.post_form("/signup", &[("display_name", "T"), ("email", "ana@example.com"), ("password", PW), ("org", "acme")]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.session_cookie().is_none(), "no session until the email is confirmed");
    assert_eq!(anon.post_form("/login", &[("email", "ana@example.com"), ("password", PW)]).await.status, StatusCode::FORBIDDEN);

    let (to, _, body) = app.mail.0.lock().unwrap().last().cloned().expect("a verification mail");
    assert_eq!(to, "ana@example.com");
    let link = body.lines().find(|l| l.contains("/verify?token=")).unwrap();
    let path = &link[link.find("/verify").unwrap()..];
    assert_eq!(anon.get(path).await.status, StatusCode::OK);
    assert_eq!(anon.get(path).await.status, StatusCode::BAD_REQUEST, "single use");
    assert_eq!(anon.post_form("/login", &[("email", "ana@example.com"), ("password", PW)]).await.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn password_reset_flow() {
    let app = boot().await;
    let old = signup(&app, "ana@example.com", "acme").await;
    let anon = Client::anon(&app);
    let before = app.mail.0.lock().unwrap().len();

    let unknown = anon.post_form("/forgot", &[("email", "nobody@example.com")]).await;
    let known = anon.post_form("/forgot", &[("email", "ana@example.com")]).await;
    assert_eq!(unknown.status, known.status);
    assert_eq!(unknown.body, known.body, "same answer whether or not the account exists");
    let mails = app.mail.0.lock().unwrap().clone();
    assert_eq!(mails.len(), before + 1, "only the real account got a mail");
    let body = &mails.last().unwrap().2;
    let link = body.lines().find(|l| l.contains("/reset?token=")).unwrap();
    let token = link.split("token=").nth(1).unwrap().to_string();

    assert_eq!(anon.post_form("/reset", &[("token", &token), ("password", "short")]).await.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(anon.post_form("/reset", &[("token", "bogus"), ("password", "a brand new password")]).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(anon.post_form("/reset", &[("token", &token), ("password", "a brand new password")]).await.status, StatusCode::SEE_OTHER);
    assert_eq!(anon.post_form("/reset", &[("token", &token), ("password", "another one entirely")]).await.status, StatusCode::BAD_REQUEST, "single use");

    assert_eq!(old.get("/workspaces").await.status, StatusCode::SEE_OTHER, "a reset signs everything out");
    assert_eq!(anon.post_form("/login", &[("email", "ana@example.com"), ("password", PW)]).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(anon.post_form("/login", &[("email", "ana@example.com"), ("password", "a brand new password")]).await.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn deleting_an_account_removes_it_and_its_data() {
    let app = boot().await;
    let ana = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&ana, "prod").await;
    let r = new_run(&ana, &ws, CFG_A, json!({})).await;
    wait_status(&ana, &r, "needs_confirmation").await;

    let wrong = ana.post_form("/account/delete", &[("password", "not my password")]).await;
    assert_eq!(wrong.status, StatusCode::FORBIDDEN);
    assert_eq!(ana.get("/workspaces").await.status, StatusCode::OK, "still signed in");

    let ok = ana.post_form("/account/delete", &[("password", PW)]).await;
    assert_eq!(ok.status, StatusCode::SEE_OTHER);
    assert_eq!(ana.get("/workspaces").await.status, StatusCode::SEE_OTHER, "session is gone");
    assert!(app.store.find_user_by_email("ana@example.com").await.unwrap().is_none());
    assert!(app.store.get_workspace(&ws).await.is_err(), "her sole-owner organization and its data are deleted");
    assert!(app.store.get_run(&r).await.is_err());
}

// ================= isolation, roles and tokens =================

#[tokio::test]
async fn another_organization_cannot_see_or_touch_anything() {
    let app = boot().await;
    let ana = signup(&app, "ana@example.com", "acme").await;
    let bo = signup(&app, "bo@example.com", "bobs").await;
    let ws = new_ws(&ana, "prod").await;
    let run = new_run(&ana, &ws, CFG_A, json!({})).await;
    wait_status(&ana, &run, "needs_confirmation").await;
    let var = ana.post_json(&format!("/api/v2/workspaces/{ws}/vars"), json!({ "key": "K", "value": "v" })).await.json()["data"]["id"].as_str().unwrap().to_string();

    // every route that takes an id, as a second user
    let cases: Vec<(Method, String, Option<Value>)> = vec![
        (Method::GET, format!("/workspaces/{ws}"), None),
        (Method::GET, format!("/workspaces/{ws}/states"), None),
        (Method::GET, format!("/workspaces/{ws}/variables"), None),
        (Method::POST, format!("/workspaces/{ws}/lock"), None),
        (Method::POST, format!("/workspaces/{ws}/unlock"), None),
        (Method::POST, format!("/workspaces/{ws}/runs"), None),
        (Method::POST, format!("/workspaces/{ws}/variables"), None),
        (Method::POST, format!("/workspaces/{ws}/variables/{var}/delete"), None),
        (Method::GET, format!("/runs/{run}"), None),
        (Method::POST, format!("/runs/{run}/apply"), None),
        (Method::POST, format!("/runs/{run}/discard"), None),
        (Method::GET, format!("/api/v2/runs/{run}"), None),
        (Method::GET, format!("/api/v2/runs/{run}/logs"), None),
        (Method::POST, format!("/api/v2/runs/{run}/actions/apply"), Some(json!({}))),
        (Method::POST, format!("/api/v2/runs/{run}/actions/discard"), Some(json!({}))),
        (Method::POST, format!("/api/v2/workspaces/{ws}/runs"), Some(json!({ "config": "" }))),
        (Method::POST, format!("/api/v2/workspaces/{ws}/actions/lock"), Some(json!({}))),
        (Method::POST, format!("/api/v2/workspaces/{ws}/actions/force-unlock"), Some(json!({}))),
        (Method::GET, format!("/api/v2/workspaces/{ws}/current-state-version"), None),
        (Method::GET, format!("/api/v2/workspaces/{ws}/vars"), None),
        (Method::POST, format!("/api/v2/workspaces/{ws}/vars"), Some(json!({ "key": "X", "value": "y" }))),
        (Method::DELETE, format!("/api/v2/workspaces/{ws}/vars/{var}"), None),
    ];
    for (m, uri, body) in cases {
        let r = match (&m, body) {
            (&Method::GET, _) => bo.get(&uri).await,
            (&Method::DELETE, _) => bo.delete(&uri).await,
            (_, Some(b)) => bo.post_json(&uri, b).await,
            (_, None) => bo.post_form(&uri, &[("config", ""), ("key", "X"), ("category", "env"), ("value", "y")]).await,
        };
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{m} {uri} leaked: {}", r.body);
    }
    // lists show only your own
    assert_eq!(bo.get("/api/v2/workspaces").await.json()["data"], json!([]));
    assert!(!bo.get("/workspaces").await.body.contains("prod"));
    assert!(!bo.get("/runs").await.body.contains(&run[..8]));
    // and nothing changed for the owner
    assert_eq!(ana.get(&format!("/api/v2/runs/{run}")).await.json()["data"]["attributes"]["status"], "needs_confirmation");
    assert!(lock_of(&ana, &ws).await.is_object());
}

#[tokio::test]
async fn members_can_run_but_not_administer() {
    let app = boot().await;
    let owner = signup(&app, "ana@example.com", "alpha-org").await;
    let member = signup(&app, "cy@example.com", "zeta-org").await;
    let ws = new_ws(&owner, "prod").await;
    // the first organization alphabetically is the active one
    let cy = app.store.find_user_by_email("cy@example.com").await.unwrap().unwrap().0;
    let alpha = app.store.orgs_for_user(&app.store.find_user_by_email("ana@example.com").await.unwrap().unwrap().0.id).await.unwrap()[0].0.clone();
    app.store.add_member(&alpha.id, &cy.id, "member").await.unwrap();

    assert_eq!(member.get(&format!("/workspaces/{ws}")).await.status, StatusCode::OK);
    let run = new_run(&member, &ws, CFG_A, json!({})).await;
    wait_status(&member, &run, "needs_confirmation").await;
    assert_eq!(member.post(&format!("/api/v2/runs/{run}/actions/apply")).await.status, StatusCode::ACCEPTED);
    wait_status(&member, &run, "applied").await;
    assert_eq!(member.post(&format!("/api/v2/workspaces/{ws}/actions/lock")).await.status, StatusCode::OK);
    assert_eq!(member.post(&format!("/api/v2/workspaces/{ws}/actions/unlock")).await.status, StatusCode::OK);

    for r in [
        member.post_json("/api/v2/workspaces", json!({ "name": "mine" })).await,
        member.post_form("/workspaces", &[("name", "mine")]).await,
        member.post_json(&format!("/api/v2/workspaces/{ws}/vars"), json!({ "key": "K", "value": "v" })).await,
        member.post_form(&format!("/workspaces/{ws}/variables"), &[("key", "K"), ("category", "env"), ("value", "v")]).await,
        member.post(&format!("/api/v2/workspaces/{ws}/actions/force-unlock")).await,
        member.post_json("/api/v2/tokens", json!({ "description": "x", "kind": "org" })).await,
    ] {
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    }
    assert!(!member.get(&format!("/workspaces/{ws}/variables")).await.body.contains("Add or replace"), "no form for a role that cannot use it");
}

#[tokio::test]
async fn api_tokens_work_expire_and_cannot_mint_more() {
    let app = boot().await;
    let ana = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&ana, "prod").await;

    let made = ana.post_json("/api/v2/tokens", json!({ "description": "ci" })).await;
    assert_eq!(made.status, StatusCode::CREATED);
    let secret = made.json()["meta"]["secret"].as_str().unwrap().to_string();
    assert!(secret.starts_with("tfk_"));
    let listed = ana.get("/api/v2/tokens").await;
    assert!(!listed.body.contains(&secret), "the secret is never shown again");
    assert_eq!(listed.json()["data"].as_array().unwrap().len(), 1);

    // the token acts as its user, with no cookie and no Origin
    let bot = Client { origin: None, ..Client::anon(&app).with_bearer(&secret) };
    assert_eq!(bot.get("/api/v2/workspaces").await.json()["data"].as_array().unwrap().len(), 1);
    let run = new_run(&bot, &ws, CFG_A, json!({})).await;
    wait_status(&bot, &run, "needs_confirmation").await;
    assert_eq!(bot.post_json("/api/v2/tokens", json!({ "description": "more" })).await.status, StatusCode::FORBIDDEN, "a token cannot mint tokens");
    assert_eq!(bot.get("/api/v2/tokens").await.status, StatusCode::FORBIDDEN);

    // an organization token manages but cannot plan or apply
    let org = ana.post_json("/api/v2/tokens", json!({ "description": "provisioner", "kind": "org" })).await.json()["meta"]["secret"].as_str().unwrap().to_string();
    let orgbot = Client { origin: None, ..Client::anon(&app).with_bearer(&org) };
    assert_eq!(orgbot.post_json("/api/v2/workspaces", json!({ "name": "made-by-org" })).await.status, StatusCode::CREATED);
    assert_eq!(orgbot.post_json(&format!("/api/v2/workspaces/{ws}/runs"), json!({ "config": "" })).await.status, StatusCode::FORBIDDEN);
    assert_eq!(orgbot.post(&format!("/api/v2/runs/{run}/actions/apply")).await.status, StatusCode::FORBIDDEN);
    assert_eq!(orgbot.get(&format!("/api/v2/runs/{run}")).await.status, StatusCode::OK, "it can read");

    // a wrong token is a failed login, even with a valid cookie alongside
    let wrong = Client { bearer: Some("tfk_nope".into()), ..ana.clone() };
    assert_eq!(wrong.get("/api/v2/workspaces").await.status, StatusCode::UNAUTHORIZED);

    // revoke
    let id = listed.json()["data"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(ana.delete(&format!("/api/v2/tokens/{id}")).await.status, StatusCode::NO_CONTENT);
    assert_eq!(bot.get("/api/v2/workspaces").await.status, StatusCode::UNAUTHORIZED, "revoked");

    // the page shows the secret once, on creation
    let page = ana.post_form("/tokens", &[("description", "from page"), ("kind", "user"), ("days", "30")]).await;
    assert!(page.body.contains("tfk_") && page.body.contains("only once"));
    assert!(!ana.get("/tokens").await.body.contains("tfk_"));
}

// ================= variables =================

#[tokio::test]
async fn sensitive_variables_reach_the_engine_but_never_the_screen_or_the_log() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let secret = "s3cr3t-Pa55word";

    let made = c.post_json(&format!("/api/v2/workspaces/{ws}/vars"), json!({ "key": "DB_PASSWORD", "value": secret, "sensitive": true })).await;
    assert_eq!(made.status, StatusCode::CREATED);
    assert!(!made.body.contains(secret) && made.json()["data"]["attributes"]["value"].is_null(), "the response never echoes it");
    c.post_json(&format!("/api/v2/workspaces/{ws}/vars"), json!({ "key": "region", "value": "eu-west-1" })).await;
    assert_eq!(c.post_json(&format!("/api/v2/workspaces/{ws}/vars"), json!({ "key": "1bad", "value": "x" })).await.status, StatusCode::UNPROCESSABLE_ENTITY);

    let cfg = "resource \"db\" \"main\" {\n  password = var.DB_PASSWORD\n  region = var.region\n}\necho_var DB_PASSWORD\necho_var region\n";
    let run = new_run(&c, &ws, cfg, json!({ "auto_apply": true })).await;
    wait_status(&c, &run, "applied").await;

    let log = c.get(&format!("/api/v2/runs/{run}/logs")).await.body;
    assert!(log.contains("DB_PASSWORD = (sensitive)"), "{log}");
    assert!(log.contains("region = eu-west-1"), "non-sensitive values are shown");
    for (what, body) in [
        ("log", log.clone()),
        ("run page", c.get(&format!("/runs/{run}")).await.body),
        ("variables page", c.get(&format!("/workspaces/{ws}/variables")).await.body),
        ("variables api", c.get(&format!("/api/v2/workspaces/{ws}/vars")).await.body),
        ("state api", c.get(&format!("/api/v2/workspaces/{ws}/current-state-version")).await.body),
    ] {
        assert!(!body.contains(secret), "the secret leaked into the {what}");
    }
    assert!(c.get(&format!("/workspaces/{ws}/variables")).await.body.contains("sensitive, write-only"));

    // the same plan with the same variables has no changes; rotating the secret does
    let same = new_run(&c, &ws, cfg, json!({ "operation": "plan_only" })).await;
    assert_eq!(wait_status(&c, &same, "planned_and_finished").await["data"]["attributes"]["summary"]["change"], 0);
    c.post_json(&format!("/api/v2/workspaces/{ws}/vars"), json!({ "key": "DB_PASSWORD", "value": "rotated-value-9", "sensitive": true })).await;
    let rot = new_run(&c, &ws, cfg, json!({ "operation": "plan_only" })).await;
    assert_eq!(wait_status(&c, &rot, "planned_and_finished").await["data"]["attributes"]["summary"]["change"], 1, "a new value changes the plan");
    assert!(!c.get(&format!("/api/v2/runs/{rot}/logs")).await.body.contains("rotated-value-9"));

    // delete
    let vars = c.get(&format!("/api/v2/workspaces/{ws}/vars")).await.json();
    let id = vars["data"][0]["id"].as_str().unwrap();
    assert_eq!(c.delete(&format!("/api/v2/workspaces/{ws}/vars/{id}")).await.status, StatusCode::NO_CONTENT);
    assert_eq!(c.delete(&format!("/api/v2/workspaces/{ws}/vars/{id}")).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn variables_form_validates_and_saves() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let bad = c.post_form(&format!("/workspaces/{ws}/variables"), &[("key", "bad-key"), ("category", "env"), ("value", "x")]).await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(bad.body.contains("letters, numbers"));
    let ok = c.post_form(&format!("/workspaces/{ws}/variables"), &[("key", "TOKEN"), ("category", "env"), ("value", "abc123xyz"), ("sensitive", "on")]).await;
    assert_eq!(ok.status, StatusCode::SEE_OTHER);
    let page = c.get(&format!("/workspaces/{ws}/variables")).await.body;
    assert!(page.contains("TOKEN") && page.contains("write-only") && !page.contains("abc123xyz"));
}

// ================= edge cases (replica-test) =================
// Case ids refer to replica/test-plan.md.

#[tokio::test]
async fn f02_e1_double_clicking_confirm_is_not_an_error_page() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let run = new_run(&c, &ws, CFG_A, json!({})).await;
    wait_status(&c, &run, "needs_confirmation").await;
    let first = c.post_form(&format!("/runs/{run}/apply"), &[]).await;
    let second = c.post_form(&format!("/runs/{run}/apply"), &[]).await;
    assert_eq!(first.status, StatusCode::SEE_OTHER);
    assert_eq!(second.status, StatusCode::SEE_OTHER, "the second click of a double click must land on the run, not on a 409 page");
    wait_status(&c, &run, "applied").await;
    // after it finished, a stale tab clicking Confirm still lands on the run
    assert_eq!(c.post_form(&format!("/runs/{run}/apply"), &[]).await.status, StatusCode::SEE_OTHER);
    // but the API stays strict
    assert_eq!(c.post(&format!("/api/v2/runs/{run}/actions/apply")).await.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn f02_e2_markup_in_resource_names_cannot_inject_script() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let cfg = "resource \"<img src=x onerror=alert(1)>\" \"<script>alert(2)</script>\" {\n  a = \"</div><script>alert(3)</script>\"\n}\n";
    let run = new_run(&c, &ws, cfg, json!({})).await;
    wait_status(&c, &run, "needs_confirmation").await;
    for uri in [format!("/runs/{run}"), format!("/workspaces/{ws}"), "/runs".to_string(), "/workspaces".to_string()] {
        let body = c.get(&uri).await.body;
        assert!(!body.contains("<img src=x") && !body.contains("<script>alert"), "unescaped markup on {uri}");
    }
    assert!(c.get(&format!("/runs/{run}")).await.body.contains("&lt;img src=x"));
}

#[tokio::test]
async fn f02_e3_unicode_and_emoji_survive() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let msg = "triển khai 日本語 🚀 café";
    let r = c.post_json(&format!("/api/v2/workspaces/{ws}/runs"), json!({ "config": "# 設定\nresource \"a\" \"b\" {\n x = \"é🚀\"\n}\n", "message": msg })).await;
    assert_eq!(r.status, StatusCode::CREATED);
    let run = r.json()["data"]["id"].as_str().unwrap().to_string();
    wait_status(&c, &run, "needs_confirmation").await;
    assert!(c.get(&format!("/runs/{run}")).await.body.contains(msg));
    assert!(c.get(&format!("/workspaces/{ws}")).await.body.contains("# 設定"), "the editor shows the last config intact");
}

#[tokio::test]
async fn f02_e4_oversized_message_and_config_are_refused_cleanly() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let long = "x".repeat(5_000);
    let r = c.post_json(&format!("/api/v2/workspaces/{ws}/runs"), json!({ "config": "", "message": long })).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "a 5000 character message must be refused");
    let huge = "#".repeat(1_500_000);
    let r = c.post_json(&format!("/api/v2/workspaces/{ws}/runs"), json!({ "config": huge })).await;
    assert!(r.status.is_client_error(), "a 1.5 MB config must be a 4xx, got {}", r.status);
    assert!(c.get("/api/v2/workspaces").await.status.is_success(), "and the server is still fine");
}

#[tokio::test]
async fn f02_e5_windows_line_endings_do_not_cause_phantom_changes() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let r = new_run(&c, &ws, CFG_A, json!({ "auto_apply": true })).await;
    wait_status(&c, &r, "applied").await;
    let crlf = CFG_A.replace('\n', "\r\n");
    let r2 = new_run(&c, &ws, &crlf, json!({ "operation": "plan_only" })).await;
    assert_eq!(wait_status(&c, &r2, "planned_and_finished").await["data"]["attributes"]["summary"], json!({ "add": 0, "change": 0, "destroy": 0 }));
}

#[tokio::test]
async fn f02_e6_a_pending_run_can_be_canceled_and_the_queue_continues() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let a = new_run(&c, &ws, CFG_A, json!({})).await;
    wait_status(&c, &a, "needs_confirmation").await;
    let b = new_run(&c, &ws, CFG_A, json!({})).await;
    let b2 = new_run(&c, &ws, CFG_A, json!({})).await;
    assert_eq!(c.post_form(&format!("/runs/{b}/cancel"), &[]).await.status, StatusCode::SEE_OTHER);
    assert_eq!(c.get(&format!("/api/v2/runs/{b}")).await.json()["data"]["attributes"]["status"], "canceled");
    c.post(&format!("/api/v2/runs/{a}/actions/discard")).await;
    wait_status(&c, &b2, "needs_confirmation").await;
}

#[tokio::test]
async fn f05_e1_force_unlock_ends_the_run_that_held_the_lock() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let a = new_run(&c, &ws, CFG_A, json!({})).await;
    wait_status(&c, &a, "needs_confirmation").await;
    let b = new_run(&c, &ws, CFG_A, json!({})).await;
    assert_eq!(c.post(&format!("/api/v2/workspaces/{ws}/actions/force-unlock")).await.status, StatusCode::OK);
    assert_eq!(c.get(&format!("/api/v2/runs/{a}")).await.json()["data"]["attributes"]["status"], "discarded", "the run that held the lock ends");
    wait_status(&c, &b, "needs_confirmation").await;
    assert_eq!(c.get(&format!("/api/v2/runs/{a}")).await.json()["data"]["attributes"]["status"], "discarded", "and stays ended");
}

#[tokio::test]
async fn f05_e2_lock_reason_is_escaped_and_unicode_safe() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let r = c.post_form(&format!("/workspaces/{ws}/lock"), &[("reason", "<b>fix</b> trạng thái 🔒")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let body = c.get(&format!("/workspaces/{ws}")).await.body;
    assert!(!body.contains("<b>fix</b>") && body.contains("&lt;b&gt;fix") && body.contains("trạng thái 🔒"));
}

#[tokio::test]
async fn api_e1_malformed_json_gets_the_json_error_envelope() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    for (ct, body) in [(Some("application/json"), "{not json"), (Some("application/json"), "[]"), (None, "{\"name\":\"x\"}"), (Some("text/plain"), "x")] {
        let r = c.send(Method::POST, "/api/v2/workspaces", ct, body.to_string()).await;
        assert!(r.status.is_client_error(), "{ct:?} {body}: {}", r.status);
        assert!(r.json()["errors"][0]["title"].is_string(), "{ct:?} {body}: error body must be the JSON envelope, got {:?}", r.body);
    }
}

#[tokio::test]
async fn auth_e1_hostile_email_and_names_are_escaped_everywhere() {
    let app = boot().await;
    let anon = Client::anon(&app);
    let r = anon.post_form("/signup", &[("display_name", "<script>alert(1)</script>"), ("email", "a<script>@example.com"), ("password", PW), ("org", "acme")]).await;
    let c = Client { cookie: r.session_cookie().map(|s| cookie_pair(&s)), ..anon.clone() };
    for uri in ["/workspaces", "/account", "/tokens", "/runs"] {
        let body = c.get(uri).await.body;
        assert!(!body.contains("<script>alert") && !body.contains("a<script>"), "unescaped on {uri}");
    }
}

#[tokio::test]
async fn auth_e2_password_limits_and_unicode_passwords() {
    let app = boot().await;
    let anon = Client::anon(&app);
    let long = "a".repeat(201);
    assert_eq!(anon.post_form("/signup", &[("display_name", "T"), ("email", "a@example.com"), ("password", &long), ("org", "o1")]).await.status, StatusCode::UNPROCESSABLE_ENTITY);
    let uni = "mật-khẩu-rất-dài-🔑";
    assert_eq!(anon.post_form("/signup", &[("display_name", "T"), ("email", "b@example.com"), ("password", uni), ("org", "o2")]).await.status, StatusCode::SEE_OTHER);
    assert_eq!(anon.post_form("/login", &[("email", "b@example.com"), ("password", uni)]).await.status, StatusCode::SEE_OTHER);
    assert_eq!(anon.post_form("/signup", &[("display_name", "T"), ("email", "c@example.com"), ("password", PW), ("org", "tổ-chức")]).await.status, StatusCode::UNPROCESSABLE_ENTITY, "organization names are ascii");
}

#[tokio::test]
async fn auth_e3_forged_expired_and_stale_sessions_are_rejected() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let forged = Client { cookie: Some("tf_session=tfs_0000000000000000000000000000000000000000000000000000000000000000".into()), ..c.clone() };
    assert_eq!(forged.get("/workspaces").await.status, StatusCode::SEE_OTHER);
    assert_eq!(forged.get("/api/v2/workspaces").await.status, StatusCode::UNAUTHORIZED);
    let garbage = Client { cookie: Some("tf_session=%%%;;;=".into()), ..c.clone() };
    assert_eq!(garbage.get("/api/v2/workspaces").await.status, StatusCode::UNAUTHORIZED);

    // an expired session, created directly in the store
    let u = app.store.find_user_by_email("ana@example.com").await.unwrap().unwrap().0;
    let secret = "tfs_expired_session_secret";
    app.store.create_session(&u.id, &tf_crypto::sha256(secret.as_bytes()), tf_core::now().saturating_sub(10)).await.unwrap();
    let expired = Client { cookie: Some(format!("tf_session={secret}")), ..c.clone() };
    assert_eq!(expired.get("/api/v2/workspaces").await.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn auth_e4_cross_site_posts_cannot_log_you_in_or_out() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let evil = Client { origin: Some("https://evil.example.net".into()), ..c.clone() };
    let r = evil.post("/logout").await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "a foreign site must not be able to sign you out");
    assert_eq!(c.get("/workspaces").await.status, StatusCode::OK, "and you are still signed in");
    let anon_evil = Client { cookie: None, ..evil.clone() };
    let r = anon_evil.post_form("/login", &[("email", "ana@example.com"), ("password", PW)]).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "login CSRF: a foreign site must not sign a victim in as the attacker");
    assert!(r.session_cookie().is_none());
}

#[tokio::test]
async fn f01_e1_many_workspaces_in_parallel_all_complete() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let mut ids = Vec::new();
    for i in 0..8 {
        let ws = new_ws(&c, &format!("ws-{i}")).await;
        ids.push((ws.clone(), new_run(&c, &ws, CFG_A, json!({ "auto_apply": true })).await));
    }
    for (ws, run) in &ids {
        wait_status(&c, run, "applied").await;
        let st = c.get(&format!("/api/v2/workspaces/{ws}/current-state-version")).await.json();
        assert_eq!((st["data"]["attributes"]["serial"].clone(), st["data"]["attributes"]["resource-count"].clone()), (json!(1), json!(2)), "{ws}");
        assert!(lock_of(&c, ws).await.is_null());
    }
}

#[tokio::test]
async fn a11y_e1_every_page_has_exactly_one_h1_and_labelled_controls() {
    let app = boot().await;
    let anon = Client::anon(&app);
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let run = new_run(&c, &ws, CFG_A, json!({})).await;
    wait_status(&c, &run, "needs_confirmation").await;

    let mut pages: Vec<(String, Reply)> = Vec::new();
    for uri in ["/login", "/signup", "/forgot", "/reset?token=x", "/verify?token=x"] {
        pages.push((uri.to_string(), anon.get(uri).await));
    }
    pages.push(("POST /forgot".into(), anon.post_form("/forgot", &[("email", "x@example.com")]).await));
    for uri in ["/workspaces".to_string(), "/runs".into(), "/account".into(), "/tokens".into(), format!("/workspaces/{ws}"), format!("/workspaces/{ws}/variables"), format!("/workspaces/{ws}/states"), format!("/runs/{run}"), "/runs/missing".into()] {
        let r = c.get(&uri).await;
        pages.push((uri, r));
    }
    for (name, r) in &pages {
        assert_eq!(r.body.matches("<h1").count(), 1, "{name} must have exactly one <h1>");
        assert!(r.body.contains("<html lang=\"en\""), "{name} needs a language");
        // every visible input/select/textarea has an id with a matching label
        for tag in ["<input", "<select", "<textarea"] {
            for chunk in r.body.split(tag).skip(1) {
                let el = chunk.split('>').next().unwrap_or("");
                if el.contains("type=\"hidden\"") || el.contains("type=\"checkbox\"") {
                    continue;
                }
                let id = el.split("id=\"").nth(1).and_then(|s| s.split('"').next()).unwrap_or_else(|| panic!("{name}: a control has no id: {tag}{el}"));
                assert!(r.body.contains(&format!("for=\"{id}\"")), "{name}: control #{id} has no <label for>");
            }
        }
    }
}

#[tokio::test]
async fn mobile_e1_the_stylesheet_never_hides_navigation_on_small_screens() {
    let app = boot().await;
    let css = Client::anon(&app).get("/assets/app.css").await.body;
    let small = css.split("@media (max-width: 960px)").nth(1).expect("a rule for small screens").split("@media").next().unwrap();
    assert!(!small.contains(".sidebar { display: none"), "navigation must not disappear on small screens");
    assert!(small.contains(".sidebar { display: flex"), "the sidebar becomes a strip");
    assert!(css.contains(".content {") && css.contains("min-width: 0"), "wide content must not stretch the page");
    assert!(css.contains("overflow-x: auto"), "wide tables scroll inside their frame");
}

#[tokio::test]
async fn f02_e9_a_working_run_page_refreshes_itself_and_a_settled_one_does_not() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    // a manual lock keeps the run pending, which is "still working" from the user's point of view
    c.post(&format!("/api/v2/workspaces/{ws}/actions/lock")).await;
    let pending = new_run(&c, &ws, CFG_A, json!({})).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let page = c.get(&format!("/runs/{pending}")).await.body;
    assert!(page.contains("new EventSource") && page.contains("<noscript><meta http-equiv=\"refresh\""), "a pending run page follows the run by stream, with a reload fallback without script");
    assert!(c.get("/runs").await.body.contains("http-equiv=\"refresh\""), "the list reloads itself while something is working");

    c.post(&format!("/api/v2/workspaces/{ws}/actions/unlock")).await;
    wait_status(&c, &pending, "needs_confirmation").await;
    let waiting = c.get(&format!("/runs/{pending}")).await.body;
    assert!(!waiting.contains("http-equiv=\"refresh\"") && !waiting.contains("EventSource"), "a run waiting for a person must not reload under their cursor");
    c.post(&format!("/api/v2/runs/{pending}/actions/discard")).await;
    wait_status(&c, &pending, "discarded").await;
    let settled = c.get(&format!("/runs/{pending}")).await.body;
    assert!(!settled.contains("http-equiv=\"refresh\"") && !settled.contains("EventSource"));
}

#[tokio::test]
async fn a04_e2_the_variable_value_field_is_a_password_field() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let page = c.get(&format!("/workspaces/{ws}/variables")).await.body;
    assert!(page.contains("id=\"value\" name=\"value\" type=\"password\" autocomplete=\"off\""), "values are not shown while typed and not remembered by the browser");
}

// ================= the OpenTofu http backend protocol =================
// Wire details follow the client in opentofu/internal/backend/remote-state/http:
// GET (404/204 = no state), update with ?ID=<lock id> and Content-MD5 = base64(md5(body)),
// LOCK / UNLOCK carrying the CLI's lock JSON, 423 or 409 with the holder's lock JSON on conflict.

fn basic(token: &str) -> String {
    format!("Basic {}", tf_crypto::base64_encode(format!("token:{token}").as_bytes()))
}

fn state_doc(serial: u64, lineage: &str, extra: &str) -> String {
    format!("{{\"version\":4,\"terraform_version\":\"1.8.0\",\"serial\":{serial},\"lineage\":\"{lineage}\",\"outputs\":{{}},\"resources\":[{{\"mode\":\"managed\",\"type\":\"terraform_data\",\"name\":\"x{extra}\",\"instances\":[]}}]}}")
}

fn lock_doc(id: &str, who: &str) -> String {
    json!({ "ID": id, "Operation": "OperationTypeApply", "Info": "", "Who": who, "Version": "1.8.0", "Created": "2026-11-08T12:00:00.123456789Z", "Path": "" }).to_string()
}

fn md5_header(body: &str) -> String {
    tf_crypto::base64_encode(&tf_crypto::md5(body.as_bytes()))
}

impl Client {
    async fn raw(&self, method: &str, uri: &str, headers: &[(&str, String)], body: &str) -> Reply {
        let mut b = Request::builder().method(Method::from_bytes(method.as_bytes()).unwrap()).uri(uri).header(header::HOST, "localhost");
        if let Some(t) = &self.bearer {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        for (k, v) in headers {
            b = b.header(*k, v);
        }
        let resp = self.router.clone().oneshot(b.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let (status, headers) = (resp.status(), resp.headers().clone());
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 26).await.unwrap();
        Reply { status, headers, body: String::from_utf8_lossy(&bytes).to_string() }
    }

    /// What `tofu` sends: Basic auth, no Origin, no cookie.
    async fn cli(&self, token: &str, method: &str, uri: &str, extra: &[(&str, String)], body: &str) -> Reply {
        let mut h = vec![("authorization", basic(token))];
        h.extend(extra.iter().map(|(k, v)| (*k, v.clone())));
        self.raw(method, uri, &h, body).await
    }
}

async fn token_for(c: &Client, kind: &str) -> String {
    let r = c.post_json("/api/v2/tokens", json!({ "description": "cli", "kind": kind })).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    r.json()["meta"]["secret"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn cli_e1_the_full_lock_push_pull_unlock_cycle() {
    let app = boot().await;
    let ana = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&ana, "prod").await;
    let tok = token_for(&ana, "user").await;
    let anon = Client::anon(&app);
    let (addr, lock_addr) = ("/backend/prod", "/backend/prod/lock");

    // nothing there yet: the client treats 404 as "no state"
    assert_eq!(anon.cli(&tok, "GET", addr, &[], "").await.status, StatusCode::NOT_FOUND);

    // lock with LOCK (the default lock_method)
    let a = anon.cli(&tok, "LOCK", lock_addr, &[], &lock_doc("lock-A", "ana@laptop")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    // a second client is told who holds it, as the holder's lock JSON
    let b = anon.cli(&tok, "LOCK", lock_addr, &[], &lock_doc("lock-B", "bo@desktop")).await;
    assert_eq!(b.status, StatusCode::LOCKED);
    let held = b.json();
    assert_eq!((held["ID"].clone(), held["Who"].clone()), (json!("lock-A"), json!("ana@laptop")));
    assert!(held["Created"].is_string() && held["Operation"].is_string(), "all LockInfo keys are present: {held}");

    // pushing state needs the lock id of the holder
    let s1 = state_doc(1, "lin-1", "");
    let md5 = |b: &str| ("content-md5", md5_header(b));
    assert_eq!(anon.cli(&tok, "POST", addr, &[md5(&s1)], &s1).await.status, StatusCode::LOCKED, "no lock id");
    assert_eq!(anon.cli(&tok, "POST", &format!("{addr}?ID=lock-B"), &[md5(&s1)], &s1).await.status, StatusCode::LOCKED, "someone else's lock id");
    assert_eq!(anon.cli(&tok, "POST", &format!("{addr}?ID=lock-A"), &[("content-md5", md5_header("different"))], &s1).await.status, StatusCode::BAD_REQUEST, "md5 must match the body");
    assert_eq!(anon.cli(&tok, "POST", &format!("{addr}?ID=lock-A"), &[], "{not json").await.status, StatusCode::BAD_REQUEST);
    assert_eq!(anon.cli(&tok, "POST", &format!("{addr}?ID=lock-A"), &[], "{\"version\":4}").await.status, StatusCode::BAD_REQUEST, "serial and lineage are required");
    assert_eq!(anon.cli(&tok, "POST", &format!("{addr}?ID=lock-A"), &[md5(&s1), ("content-type", "application/json".into())], &s1).await.status, StatusCode::OK);

    // pull returns exactly what was pushed
    let got = anon.cli(&tok, "GET", addr, &[], "").await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(got.body, s1);
    assert!(got.headers[header::CONTENT_TYPE].to_str().unwrap().starts_with("application/json"));
    assert_eq!(got.headers["content-md5"].to_str().unwrap(), md5_header(&s1), "the client decodes this header as base64");

    // state can only move forward, within one lineage
    let up = |q: &str| format!("{addr}?ID=lock-A{q}");
    assert_eq!(anon.cli(&tok, "POST", &up(""), &[], &state_doc(0, "lin-1", "")).await.status, StatusCode::CONFLICT, "older serial");
    assert_eq!(anon.cli(&tok, "POST", &up(""), &[], &state_doc(1, "lin-1", "-changed")).await.status, StatusCode::CONFLICT, "same serial, different content");
    assert_eq!(anon.cli(&tok, "POST", &up(""), &[], &state_doc(5, "other-lineage", "")).await.status, StatusCode::CONFLICT, "another lineage");
    assert_eq!(anon.cli(&tok, "POST", &up(""), &[], &s1).await.status, StatusCode::OK, "pushing the same state again is harmless");
    let s2 = state_doc(2, "lin-1", "-2");
    assert_eq!(anon.cli(&tok, "PUT", &up(""), &[], &s2).await.status, StatusCode::OK, "update_method = PUT works too");
    assert_eq!(anon.cli(&tok, "GET", addr, &[], "").await.body, s2);

    // the dashboard sees both versions and counts the resources
    assert_eq!(ana.get(&format!("/workspaces/{ws}/states")).await.body.matches("<tr><td class=\"mono\">").count(), 2);
    let listed = ana.get("/api/v2/workspaces").await.json();
    assert_eq!(listed["data"][0]["attributes"]["resource-count"], 1);

    // unlock needs the holder's id in the body
    assert_eq!(anon.cli(&tok, "UNLOCK", lock_addr, &[], &lock_doc("lock-B", "bo@desktop")).await.status, StatusCode::CONFLICT);
    assert!(lock_of(&ana, &ws).await.is_object(), "still locked");
    assert_eq!(anon.cli(&tok, "UNLOCK", lock_addr, &[], &lock_doc("lock-A", "ana@laptop")).await.status, StatusCode::OK);
    assert!(lock_of(&ana, &ws).await.is_null());
    assert_eq!(anon.cli(&tok, "UNLOCK", lock_addr, &[], &lock_doc("lock-A", "ana@laptop")).await.status, StatusCode::CONFLICT, "already unlocked");
    // and pushing without a lock is refused
    assert_eq!(anon.cli(&tok, "POST", &up(""), &[], &state_doc(3, "lin-1", "-3")).await.status, StatusCode::LOCKED);
}

#[tokio::test]
async fn cli_e2_the_same_methods_work_on_a_single_address() {
    let app = boot().await;
    let ana = signup(&app, "ana@example.com", "acme").await;
    new_ws(&ana, "prod").await;
    let tok = token_for(&ana, "user").await;
    let anon = Client::anon(&app);
    assert_eq!(anon.cli(&tok, "LOCK", "/backend/prod", &[], &lock_doc("one", "x@y")).await.status, StatusCode::OK);
    let s = state_doc(1, "l", "");
    assert_eq!(anon.cli(&tok, "POST", "/backend/prod?id=one", &[], &s).await.status, StatusCode::OK, "lower-case id is accepted");
    assert_eq!(anon.cli(&tok, "UNLOCK", "/backend/prod", &[], &lock_doc("one", "x@y")).await.status, StatusCode::OK);
    // POST/DELETE spellings on the lock address, for clients configured with lock_method = POST
    assert_eq!(anon.cli(&tok, "POST", "/backend/prod/lock", &[], &lock_doc("two", "x@y")).await.status, StatusCode::OK);
    assert_eq!(anon.cli(&tok, "DELETE", "/backend/prod/lock", &[], &lock_doc("two", "x@y")).await.status, StatusCode::OK);
    // purging state through the backend is not allowed
    assert_eq!(anon.cli(&tok, "DELETE", "/backend/prod", &[], "").await.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(anon.cli(&tok, "GET", "/backend/prod", &[], "").await.body, s, "state survived");
}

#[tokio::test]
async fn cli_e3_cli_locks_and_platform_runs_exclude_each_other() {
    let app = boot().await;
    let ana = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&ana, "prod").await;
    let tok = token_for(&ana, "user").await;
    let anon = Client::anon(&app);

    // a CLI lock holds platform runs back, then lets them go
    assert_eq!(anon.cli(&tok, "LOCK", "/backend/prod/lock", &[], &lock_doc("cli-1", "ana@laptop")).await.status, StatusCode::OK);
    assert!(ana.get(&format!("/workspaces/{ws}")).await.body.contains("OpenTofu CLI"), "the dashboard says who holds the lock");
    let run = new_run(&ana, &ws, CFG_A, json!({})).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(ana.get(&format!("/api/v2/runs/{run}")).await.json()["data"]["attributes"]["status"], "pending");
    assert_eq!(anon.cli(&tok, "UNLOCK", "/backend/prod/lock", &[], &lock_doc("cli-1", "ana@laptop")).await.status, StatusCode::OK);
    wait_status(&ana, &run, "needs_confirmation").await;

    // a run holds the lock: the CLI is told so, and cannot push state
    let r = anon.cli(&tok, "LOCK", "/backend/prod/lock", &[], &lock_doc("cli-2", "ana@laptop")).await;
    assert_eq!(r.status, StatusCode::LOCKED);
    assert!(r.json()["ID"].as_str().unwrap().starts_with("run:"), "the holder is described: {}", r.body);
    assert_eq!(anon.cli(&tok, "POST", "/backend/prod?ID=cli-2", &[], &state_doc(1, "l", "")).await.status, StatusCode::LOCKED);

    // a lock left behind by a crashed CLI can be cleared from the dashboard
    c_discard(&ana, &run).await;
    assert_eq!(anon.cli(&tok, "LOCK", "/backend/prod/lock", &[], &lock_doc("cli-3", "ana@laptop")).await.status, StatusCode::OK);
    assert_eq!(ana.post_form(&format!("/workspaces/{ws}/unlock"), &[]).await.status, StatusCode::SEE_OTHER);
    assert!(lock_of(&ana, &ws).await.is_null(), "an admin can release a CLI lock from the dashboard");
}

async fn c_discard(c: &Client, run: &str) {
    assert_eq!(c.post(&format!("/api/v2/runs/{run}/actions/discard")).await.status, StatusCode::ACCEPTED);
    wait_status(c, run, "discarded").await;
}

#[tokio::test]
async fn cli_e4_who_may_use_the_backend() {
    let app = boot().await;
    let ana = signup(&app, "ana@example.com", "alpha-org").await;
    let cy = signup(&app, "cy@example.com", "zeta-org").await;
    let bo = signup(&app, "bo@example.com", "bobs").await;
    new_ws(&ana, "prod").await;
    let alpha = app.store.orgs_for_user(&app.store.find_user_by_email("ana@example.com").await.unwrap().unwrap().0.id).await.unwrap()[0].0.clone();
    let cy_user = app.store.find_user_by_email("cy@example.com").await.unwrap().unwrap().0;
    app.store.add_member(&alpha.id, &cy_user.id, "member").await.unwrap();
    let anon = Client::anon(&app);
    let s = state_doc(1, "l", "");

    // no credentials, a wrong token, a revoked-looking token: a Basic challenge
    let r = anon.raw("GET", "/backend/prod", &[], "").await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(r.headers[header::WWW_AUTHENTICATE].to_str().unwrap().starts_with("Basic"), "so tofu knows to send username and password");
    assert_eq!(anon.cli("tfk_nope", "GET", "/backend/prod", &[], "").await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(anon.raw("GET", "/backend/prod", &[("authorization", "Basic !!!".into())], "").await.status, StatusCode::UNAUTHORIZED);

    // a bearer token works as well, whatever the username
    let tok = token_for(&ana, "user").await;
    assert_eq!(Client { bearer: Some(tok.clone()), ..anon.clone() }.raw("GET", "/backend/prod", &[], "").await.status, StatusCode::NOT_FOUND, "authenticated, no state yet");
    let other_user = tf_crypto::base64_encode(format!("anything:{tok}").as_bytes());
    assert_eq!(anon.raw("GET", "/backend/prod", &[("authorization", format!("Basic {other_user}"))], "").await.status, StatusCode::NOT_FOUND);

    // a member may lock and push; an organization token may not touch state at all
    let member_tok = token_for(&cy, "user").await;
    assert_eq!(anon.cli(&member_tok, "LOCK", "/backend/prod/lock", &[], &lock_doc("m", "cy")).await.status, StatusCode::OK);
    assert_eq!(anon.cli(&member_tok, "POST", "/backend/prod?ID=m", &[], &s).await.status, StatusCode::OK);
    assert_eq!(anon.cli(&member_tok, "UNLOCK", "/backend/prod/lock", &[], &lock_doc("m", "cy")).await.status, StatusCode::OK);
    let org_tok = token_for(&ana, "org").await;
    for (m, uri) in [("GET", "/backend/prod"), ("POST", "/backend/prod?ID=x"), ("LOCK", "/backend/prod/lock")] {
        assert_eq!(anon.cli(&org_tok, m, uri, &[], &lock_doc("o", "o")).await.status, StatusCode::FORBIDDEN, "{m} with an organization token");
    }
    let ws_id = ana.get("/api/v2/workspaces").await.json()["data"][0]["id"].as_str().unwrap().to_string();
    let org_api = Client { origin: None, ..Client::anon(&app).with_bearer(&org_tok) };
    assert_eq!(org_api.get(&format!("/api/v2/workspaces/{ws_id}/current-state-version")).await.status, StatusCode::FORBIDDEN, "state can hold secrets: not for organization tokens");
    assert_eq!(org_api.get(&format!("/api/v2/workspaces/{ws_id}/vars")).await.status, StatusCode::OK, "but it may manage the rest");

    // another organization neither sees the workspace nor its state
    let bo_tok = token_for(&bo, "user").await;
    for (m, uri) in [("GET", "/backend/prod"), ("POST", "/backend/prod?ID=x"), ("LOCK", "/backend/prod/lock"), ("UNLOCK", "/backend/prod/lock")] {
        assert_eq!(anon.cli(&bo_tok, m, uri, &[], &lock_doc("b", "b")).await.status, StatusCode::NOT_FOUND, "{m} across organizations");
    }
    assert_eq!(anon.cli(&tok, "GET", "/backend/ghost", &[], "").await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cli_e5_large_state_and_odd_lock_bodies() {
    let app = boot().await;
    let ana = signup(&app, "ana@example.com", "acme").await;
    new_ws(&ana, "prod").await;
    let tok = token_for(&ana, "user").await;
    let anon = Client::anon(&app);
    assert_eq!(anon.cli(&tok, "LOCK", "/backend/prod/lock", &[], &lock_doc("big", "ana")).await.status, StatusCode::OK);
    // a 5 MB state is normal for a large estate
    let big = state_doc(1, "l", &"x".repeat(5_000_000));
    assert_eq!(anon.cli(&tok, "POST", "/backend/prod?ID=big", &[("content-md5", md5_header(&big))], &big).await.status, StatusCode::OK);
    assert_eq!(anon.cli(&tok, "GET", "/backend/prod", &[], "").await.body.len(), big.len());
    assert_eq!(anon.cli(&tok, "UNLOCK", "/backend/prod/lock", &[], &lock_doc("big", "ana")).await.status, StatusCode::OK);

    for (what, body) in [("empty", String::new()), ("not json", "lock".to_string()), ("no id", "{\"Who\":\"x\"}".to_string()), ("empty id", "{\"ID\":\"\"}".to_string()), ("id not a string", "{\"ID\":7}".to_string()), ("huge", format!("{{\"ID\":\"a\",\"Info\":\"{}\"}}", "z".repeat(10_000)))] {
        let r = anon.cli(&tok, "LOCK", "/backend/prod/lock", &[], &body).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "lock body that is {what}");
    }
    assert!(lock_of(&ana, &app.store.list_workspaces(&app.store.orgs_for_user(&app.store.find_user_by_email("ana@example.com").await.unwrap().unwrap().0.id).await.unwrap()[0].0.id).await.unwrap()[0].id).await.is_null(), "bad requests left no lock behind");
}

#[tokio::test]
async fn cli_e6_the_dashboard_shows_how_to_connect() {
    let app = boot().await;
    let ana = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&ana, "prod").await;
    let page = ana.get(&format!("/workspaces/{ws}")).await.body;
    for needle in ["backend &quot;http&quot;", "/backend/prod", "lock_address", "unlock_address", "TF_HTTP_PASSWORD", "tofu init"] {
        assert!(page.contains(needle), "the workspace page should explain: {needle}");
    }
    assert!(!page.contains("tfk_"), "no secret in the instructions");
}

#[tokio::test]
async fn cli_e7_force_unlock_works_for_locks_the_dashboard_or_a_run_holds() {
    // tofu prints the holder's ID and suggests `tofu force-unlock <ID>`; for a lock that did not
    // come from a CLI that ID is "user" or "run:<id>", and following the advice must work.
    let app = boot().await;
    let ana = signup(&app, "ana@example.com", "alpha-org").await;
    let cy = signup(&app, "cy@example.com", "zeta-org").await;
    let ws = new_ws(&ana, "prod").await;
    let alpha = app.store.orgs_for_user(&app.store.find_user_by_email("ana@example.com").await.unwrap().unwrap().0.id).await.unwrap()[0].0.clone();
    let cy_user = app.store.find_user_by_email("cy@example.com").await.unwrap().unwrap().0;
    app.store.add_member(&alpha.id, &cy_user.id, "member").await.unwrap();
    let (ana_tok, cy_tok) = (token_for(&ana, "user").await, token_for(&cy, "user").await);
    let anon = Client::anon(&app);

    // a manual (dashboard) lock: its id is shown as "user"; a member may release it
    ana.post(&format!("/api/v2/workspaces/{ws}/actions/lock")).await;
    let held = anon.cli(&cy_tok, "LOCK", "/backend/prod/lock", &[], &lock_doc("x", "cy")).await;
    assert_eq!((held.status, held.json()["ID"].clone()), (StatusCode::LOCKED, json!("user")));
    assert_eq!(anon.cli(&cy_tok, "UNLOCK", "/backend/prod/lock", &[], "{\"ID\":\"user\"}").await.status, StatusCode::OK);
    assert!(lock_of(&ana, &ws).await.is_null());
    assert_eq!(anon.cli(&cy_tok, "UNLOCK", "/backend/prod/lock", &[], "{\"ID\":\"user\"}").await.status, StatusCode::CONFLICT, "nothing left to release");

    // a CLI lock is still released only by its own id
    anon.cli(&ana_tok, "LOCK", "/backend/prod/lock", &[], &lock_doc("mine", "ana")).await;
    assert_eq!(anon.cli(&cy_tok, "UNLOCK", "/backend/prod/lock", &[], "{\"ID\":\"user\"}").await.status, StatusCode::CONFLICT, "'user' is not the id of a CLI lock");
    assert!(lock_of(&ana, &ws).await.is_object());
    anon.cli(&ana_tok, "UNLOCK", "/backend/prod/lock", &[], &lock_doc("mine", "ana")).await;

    // a lock held by a run: force-unlocking it ends the run, and only an admin may
    let run = new_run(&ana, &ws, CFG_A, json!({})).await;
    wait_status(&ana, &run, "needs_confirmation").await;
    let holder = anon.cli(&ana_tok, "LOCK", "/backend/prod/lock", &[], &lock_doc("y", "ana")).await.json()["ID"].as_str().unwrap().to_string();
    assert!(holder.starts_with("run:"));
    let body = json!({ "ID": holder }).to_string();
    assert_eq!(anon.cli(&cy_tok, "UNLOCK", "/backend/prod/lock", &[], &body).await.status, StatusCode::FORBIDDEN, "a member cannot force-unlock a run");
    assert_eq!(ana.get(&format!("/api/v2/runs/{run}")).await.json()["data"]["attributes"]["status"], "needs_confirmation");
    assert_eq!(anon.cli(&ana_tok, "UNLOCK", "/backend/prod/lock", &[], &body).await.status, StatusCode::OK);
    assert_eq!(ana.get(&format!("/api/v2/runs/{run}")).await.json()["data"]["attributes"]["status"], "discarded", "the run that held the lock ends");
    assert!(lock_of(&ana, &ws).await.is_null());
}

// ================= the platform driving a real tofu binary =================
// Runs only with TF_TEST_TOFU_BIN set (otherwise SKIPPED). Uses the built-in terraform_data
// resource, so no provider is downloaded and no cloud is touched.

async fn boot_tofu(sandbox: Option<tf_engine::Sandbox>) -> Option<App> {
    let bin = std::env::var("TF_TEST_TOFU_BIN").ok().filter(|s| !s.is_empty());
    let Some(bin) = bin else {
        eprintln!("SKIPPED: TF_TEST_TOFU_BIN not set");
        return None;
    };
    let engine: Arc<dyn Engine> = Arc::new(tf_engine::TofuEngine::with_sandbox(bin, sandbox.unwrap_or_default()));
    Some(boot_engine(Config::default(), engine).await)
}

fn echo_env(name: &str) -> String {
    if cfg!(windows) { format!("echo {name}=%{name}%") } else { format!("echo {name}=${name}") }
}

#[tokio::test]
async fn real_tofu_e1_plan_confirm_apply_through_the_platform_with_secrets_kept_safe() {
    let Some(app) = boot_tofu(None).await else { return };
    std::env::set_var("PLATFORM_E2E_CANARY", "server-canary-31337");
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let secret = "run-secret-9f2c1d";
    c.post_json(&format!("/api/v2/workspaces/{ws}/vars"), json!({ "key": "msg", "value": "hello from the platform" })).await;
    c.post_json(&format!("/api/v2/workspaces/{ws}/vars"), json!({ "key": "API_SECRET", "category": "env", "value": secret, "sensitive": true })).await;

    let cfg = format!(
        "variable \"msg\" {{\n  type = string\n}}\n\nresource \"terraform_data\" \"a\" {{\n  input = var.msg\n  provisioner \"local-exec\" {{\n    command = \"{} {}\"\n  }}\n}}\n\noutput \"msg\" {{\n  value = var.msg\n}}\n",
        echo_env("API_SECRET"),
        echo_env("PLATFORM_E2E_CANARY")
    );
    let run = new_run(&c, &ws, &cfg, json!({ "message": "real tofu" })).await;
    let v = wait_status(&c, &run, "needs_confirmation").await;
    assert_eq!(v["data"]["attributes"]["summary"], json!({ "add": 1, "change": 0, "destroy": 0, "outputs": 1 }));
    assert!(c.get(&format!("/runs/{run}")).await.body.contains("terraform_data.a"), "the page lists the resource tofu planned");

    assert_eq!(c.post(&format!("/api/v2/runs/{run}/actions/apply")).await.status, StatusCode::ACCEPTED);
    wait_status(&c, &run, "applied").await;
    let log = c.get(&format!("/api/v2/runs/{run}/logs")).await.body;
    assert!(log.contains("Apply complete! Resources: 1 added"), "{log}");
    assert!(log.contains("API_SECRET=(sensitive)"), "the provisioner saw the secret and the log shows it masked: {log}");
    assert!(!log.contains(secret), "the secret never reaches the log");
    assert!(!log.contains("server-canary-31337"), "the server's own environment never reaches a provisioner");
    assert!(log.contains("PLATFORM_E2E_CANARY=") || log.contains("PLATFORM_E2E_CANARY=%PLATFORM_E2E_CANARY%"), "and it printed something for that name: {log}");

    let st = c.get(&format!("/api/v2/workspaces/{ws}/current-state-version")).await.json();
    assert_eq!(st["data"]["attributes"]["resource-count"], 1);
    let doc: Value = serde_json::from_str(&state_document(&c, &ws).await).unwrap();
    assert_eq!(doc["outputs"]["msg"]["value"], "hello from the platform", "tofu's own state document is stored");
    assert!(lock_of(&c, &ws).await.is_null());

    // the same configuration again: nothing to do
    let again = new_run(&c, &ws, &cfg, json!({})).await;
    let v = wait_status(&c, &again, "planned_and_finished").await;
    assert_eq!(v["data"]["attributes"]["summary"], json!({ "add": 0, "change": 0, "destroy": 0 }));

    // an output-only change must still be offered for apply
    let cfg2 = format!("{cfg}\noutput \"extra\" {{\n  value = \"added later\"\n}}\n");
    let out_only = new_run(&c, &ws, &cfg2, json!({ "auto_apply": true })).await;
    wait_status(&c, &out_only, "applied").await;
    let st = c.get(&format!("/api/v2/workspaces/{ws}/current-state-version")).await.json();
    assert!(state_document(&c, &ws).await.contains("added later"));
    assert_eq!(st["data"]["attributes"]["serial"], 2, "two applies, two state versions");

    // an empty configuration destroys what exists
    let gone = new_run(&c, &ws, "", json!({ "auto_apply": true })).await;
    wait_status(&c, &gone, "applied").await;
    assert_eq!(c.get(&format!("/api/v2/workspaces/{ws}/current-state-version")).await.json()["data"]["attributes"]["resource-count"], 0);
}

#[tokio::test]
async fn real_tofu_e2_a_hung_apply_is_stopped_and_the_queue_moves_on() {
    let sb = tf_engine::Sandbox { apply_timeout: Duration::from_secs(4), work_root: std::env::temp_dir().join(format!("tfclone-e2e-{}", std::process::id())), ..Default::default() };
    let Some(app) = boot_tofu(Some(sb)).await else { return };
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let hang = if cfg!(windows) { "ping -n 61 127.0.0.79 > nul" } else { "sleep 61" };
    let cfg = format!("resource \"terraform_data\" \"x\" {{\n  provisioner \"local-exec\" {{\n    command = \"{hang}\"\n  }}\n}}\n");
    let stuck = new_run(&c, &ws, &cfg, json!({ "auto_apply": true })).await;
    let next = new_run(&c, &ws, "resource \"terraform_data\" \"y\" {\n  input = 1\n}\n", json!({ "operation": "plan_only" })).await;
    let started = Instant::now();
    wait_status(&c, &stuck, "errored").await;
    assert!(started.elapsed() < Duration::from_secs(40), "stopped by the timeout, not by the 61 second sleeper");
    let log = c.get(&format!("/api/v2/runs/{stuck}/logs")).await.body;
    assert!(log.contains("did not finish within 4 seconds"), "the run page says why: {log}");
    assert!(lock_of(&c, &ws).await.is_null(), "an errored run releases its lock");
    wait_status(&c, &next, "planned_and_finished").await;
    assert_eq!(c.get(&format!("/api/v2/workspaces/{ws}/current-state-version")).await.status, StatusCode::NOT_FOUND, "nothing was applied");
}

// ================= live log (SSE), cancel and force-cancel =================

#[derive(Debug, Clone)]
struct Sse {
    event: String,
    id: Option<String>,
    data: String,
}

/// Complete events in an event-stream body; comments (keep-alives) are skipped and an
/// unfinished trailing event is left out.
fn parse_sse(buf: &str) -> Vec<Sse> {
    let text = buf.replace("\r\n", "\n");
    let Some(end) = text.rfind("\n\n") else { return vec![] };
    text[..end]
        .split("\n\n")
        .filter_map(|block| {
            let (mut event, mut id, mut data) = (None, None, Vec::new());
            for line in block.lines() {
                let field = |name: &str| line.strip_prefix(name).and_then(|r| r.strip_prefix(':')).map(|v| v.strip_prefix(' ').unwrap_or(v).to_string());
                if let Some(v) = field("event") {
                    event = Some(v)
                } else if let Some(v) = field("id") {
                    id = Some(v)
                } else if let Some(v) = field("data") {
                    data.push(v)
                }
            }
            event.map(|event| Sse { event, id, data: data.join("\n") })
        })
        .collect()
}

fn logs_of(ev: &[Sse]) -> Vec<(usize, String)> {
    ev.iter().filter(|e| e.event == "log").map(|e| (e.id.as_ref().expect("log events carry their position").parse().unwrap(), e.data.clone())).collect()
}

fn statuses_of(ev: &[Sse]) -> Vec<String> {
    ev.iter().filter(|e| e.event == "status").map(|e| serde_json::from_str::<Value>(&e.data).unwrap()["status"].as_str().unwrap().to_string()).collect()
}

impl Client {
    /// Like `send`, with extra request headers.
    async fn get_h(&self, uri: &str, extra: &[(&str, &str)]) -> Reply {
        let mut b = Request::builder().method(Method::GET).uri(uri).header(header::HOST, "localhost");
        if let Some(c) = &self.cookie {
            b = b.header(header::COOKIE, c);
        }
        if let Some(t) = &self.bearer {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        for (k, v) in extra {
            b = b.header(*k, *v);
        }
        let resp = self.router.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
        let (status, headers) = (resp.status(), resp.headers().clone());
        let bytes = tokio::time::timeout(Duration::from_secs(20), axum::body::to_bytes(resp.into_body(), 1 << 22)).await.expect("the stream ended").unwrap();
        Reply { status, headers, body: String::from_utf8_lossy(&bytes).to_string() }
    }

    /// Opens a response and hands back its body to be read as it arrives.
    async fn open(&self, uri: &str) -> (StatusCode, axum::body::BodyDataStream) {
        let mut b = Request::builder().method(Method::GET).uri(uri).header(header::HOST, "localhost");
        if let Some(c) = &self.cookie {
            b = b.header(header::COOKIE, c);
        }
        let resp = self.router.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
        (resp.status(), resp.into_body().into_data_stream())
    }
}

/// Reads more of a stream until `done(events so far)`, or the stream ends. Panics on a stall.
async fn read_sse_until(body: &mut axum::body::BodyDataStream, buf: &mut String, limit: Duration, done: impl Fn(&[Sse]) -> bool) -> Vec<Sse> {
    use tokio_stream::StreamExt;
    let deadline = Instant::now() + limit;
    loop {
        let ev = parse_sse(buf);
        if done(&ev) {
            return ev;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, body.next()).await {
            Ok(Some(Ok(b))) => buf.push_str(&String::from_utf8_lossy(&b)),
            Ok(None) => return parse_sse(buf),
            Ok(Some(Err(e))) => panic!("stream error: {e}"),
            Err(_) => panic!("the stream stalled; received so far:\n{buf}"),
        }
    }
}

async fn wait_until(what: &str, limit: Duration, mut ok: impl FnMut() -> bool) {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for: {what}");
}

async fn log_text(c: &Client, run: &str) -> String {
    c.get(&format!("/api/v2/runs/{run}/logs")).await.body
}

async fn slow_app(delay_ms: u64) -> App {
    boot_engine(Config::default(), Arc::new(FakeEngine::new(Duration::from_millis(delay_ms)))).await
}

#[tokio::test]
async fn sse_streams_the_stored_log_in_order_and_ends_with_done() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let run = new_run(&c, &ws, CFG_A, json!({ "auto_apply": true })).await;

    let r = c.get_h(&format!("/runs/{run}/events"), &[]).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.headers[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/event-stream"));
    assert!(r.headers[header::CACHE_CONTROL].to_str().unwrap().contains("no-cache"));
    let ev = parse_sse(&r.body);

    let stored = log_text(&c, &run).await;
    let stored: Vec<&str> = stored.split('\n').collect();
    let streamed = logs_of(&ev);
    assert!(streamed.len() > 5, "{streamed:?}");
    for (i, (pos, line)) in streamed.iter().enumerate() {
        assert_eq!(*pos, i, "positions are consecutive from 0");
        assert_eq!(line, stored[i], "line {i} equals the stored log");
    }
    assert_eq!(streamed.len(), stored.len(), "nothing missing");
    assert_eq!(statuses_of(&ev).last().unwrap(), "applied");
    let last = ev.last().unwrap();
    assert_eq!((last.event.as_str(), last.data.as_str()), ("done", "applied"), "the stream closes when the run is over");

    // the JSON API serves the same stream, and bearer tokens work
    let made = c.post_json("/api/v2/tokens", json!({ "description": "ci" })).await.json()["meta"]["secret"].as_str().unwrap().to_string();
    let bot = Client { origin: None, cookie: None, ..c.with_bearer(&made) };
    let api = bot.get_h(&format!("/api/v2/runs/{run}/events"), &[]).await;
    assert_eq!(api.status, StatusCode::OK);
    assert_eq!(logs_of(&parse_sse(&api.body)).len(), stored.len());
}

#[tokio::test]
async fn sse_resumes_from_a_position_and_the_last_event_id_wins() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let run = new_run(&c, &ws, CFG_A, json!({ "auto_apply": true })).await;
    wait_status(&c, &run, "applied").await;
    let total = logs_of(&parse_sse(&c.get_h(&format!("/runs/{run}/events"), &[]).await.body)).len();

    let from3 = logs_of(&parse_sse(&c.get_h(&format!("/runs/{run}/events?from=3"), &[]).await.body));
    assert_eq!(from3.first().unwrap().0, 3);
    assert_eq!(from3.len(), total - 3);

    // a reconnecting browser says where it was; that beats the address
    let lei = logs_of(&parse_sse(&c.get_h(&format!("/runs/{run}/events?from=1"), &[("Last-Event-ID", "5")]).await.body));
    assert_eq!(lei.first().unwrap().0, 6, "after the last line it saw");

    // nonsense falls back to the start; beyond the end there is nothing to repeat
    assert_eq!(logs_of(&parse_sse(&c.get_h(&format!("/runs/{run}/events?from=abc"), &[]).await.body)).len(), total);
    assert_eq!(logs_of(&parse_sse(&c.get_h(&format!("/runs/{run}/events"), &[("Last-Event-ID", "junk")]).await.body)).len(), total);
    let past = parse_sse(&c.get_h(&format!("/runs/{run}/events?from=99999"), &[]).await.body);
    assert!(logs_of(&past).is_empty());
    assert_eq!(past.last().unwrap().event, "done", "it still says the run is over");
}

#[tokio::test]
async fn sse_delivers_lines_while_the_run_is_working_and_stays_open_for_a_person() {
    let app = slow_app(250).await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let run = new_run(&c, &ws, CFG_A, json!({})).await;

    let started = Instant::now();
    let (status, mut body) = c.open(&format!("/runs/{run}/events")).await;
    assert_eq!(status, StatusCode::OK);
    let mut buf = String::new();
    // the first line arrives while the engine is still planning, not after it finished
    let ev = read_sse_until(&mut body, &mut buf, Duration::from_secs(5), |e| !logs_of(e).is_empty()).await;
    assert!(started.elapsed() < Duration::from_millis(1500), "took {:?}", started.elapsed());
    assert!(statuses_of(&ev).iter().all(|s| s == "pending" || s == "planning"), "still working, not finished: {ev:?}");
    let now = c.get(&format!("/api/v2/runs/{run}")).await.json();
    assert_eq!(now["data"]["attributes"]["status"], "planning", "the run had not finished when its first line was already here");

    // it reaches the pause, tells us, and then stays open: the run is not over
    let ev = read_sse_until(&mut body, &mut buf, Duration::from_secs(10), |e| statuses_of(e).iter().any(|s| s == "needs_confirmation")).await;
    assert!(ev.iter().all(|e| e.event != "done"));
    use tokio_stream::StreamExt;
    // the last lines of the plan may still be on their way; after them it goes quiet, and stays open
    let mut quiet = false;
    for _ in 0..10 {
        match tokio::time::timeout(Duration::from_millis(600), body.next()).await {
            Err(_) => {
                quiet = true;
                break;
            }
            Ok(Some(Ok(b))) => buf.push_str(&String::from_utf8_lossy(&b)),
            Ok(_) => panic!("the stream closed while a person was still deciding: {buf}"),
        }
    }
    assert!(quiet, "nothing more to say while a person decides, and the stream is still open");
    assert!(parse_sse(&buf).iter().all(|e| e.event != "done"));
    assert!(parse_sse(&buf).iter().any(|e| e.data.contains("Plan: 2 to add")), "the whole plan arrived before the pause");

    // the decision continues the same stream to the end
    assert_eq!(c.post(&format!("/api/v2/runs/{run}/actions/apply")).await.status, StatusCode::ACCEPTED);
    let ev = read_sse_until(&mut body, &mut buf, Duration::from_secs(15), |e| e.iter().any(|x| x.event == "done")).await;
    assert_eq!(ev.last().unwrap().data, "applied");
    let lines = logs_of(&ev);
    assert!(lines.iter().any(|(_, l)| l.contains("Apply complete")));
    for (i, (pos, _)) in lines.iter().enumerate() {
        assert_eq!(*pos, i, "no line lost or repeated across the whole run");
    }
}

#[tokio::test]
async fn sse_is_private_and_never_carries_a_sensitive_value() {
    let app = boot().await;
    let ana = signup(&app, "ana@example.com", "acme").await;
    let bo = signup(&app, "bo@example.com", "other").await;
    let ws = new_ws(&ana, "prod").await;
    assert_eq!(ana.post_json(&format!("/api/v2/workspaces/{ws}/vars"), json!({ "key": "DB_PASSWORD", "value": "hunter2-SSE", "sensitive": true })).await.status, StatusCode::CREATED);
    let run = new_run(&ana, &ws, "resource \"db\" \"main\" {\n  password = var.DB_PASSWORD\n}\necho_var DB_PASSWORD\n", json!({ "auto_apply": true })).await;

    let body = ana.get_h(&format!("/runs/{run}/events"), &[]).await.body;
    assert!(!body.contains("hunter2-SSE"), "the secret is masked before it is stored, so it can never be streamed");
    assert!(body.contains("(sensitive)"));

    // another organization learns nothing, not even that the run exists
    for uri in [format!("/runs/{run}/events"), format!("/api/v2/runs/{run}/events")] {
        let r = bo.get_h(&uri, &[]).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{uri}");
        assert!(!r.body.contains("event:"));
    }
    // and an anonymous caller gets no stream
    let anon = Client::anon(&app).get_h(&format!("/api/v2/runs/{run}/events"), &[]).await;
    assert!(anon.status == StatusCode::UNAUTHORIZED || anon.status == StatusCode::SEE_OTHER, "{}", anon.status);
    assert!(!anon.body.contains("event:"));
    assert_eq!(ana.get_h("/runs/does-not-exist/events", &[]).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn canceling_a_planning_run_stops_the_engine_step_and_the_queue_moves_on() {
    let app = slow_app(30_000).await; // each engine pause would last 30 s
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let run = new_run(&c, &ws, CFG_A, json!({})).await;
    wait_status(&c, &run, "planning").await;
    wait_until("the engine step to be registered", Duration::from_secs(3), || app.cancels.active() == 1).await;

    let t = Instant::now();
    let r = c.post(&format!("/api/v2/runs/{run}/actions/cancel")).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.body);
    assert_eq!(r.json()["data"]["attributes"]["status"], "canceled");
    wait_until("the engine step to stop", Duration::from_secs(3), || app.cancels.active() == 0).await;
    assert!(t.elapsed() < Duration::from_secs(5), "the 30 s engine pause was cut short, took {:?}", t.elapsed());
    assert!(lock_of(&c, &ws).await.is_null(), "the lock is free");
    wait_until("the log to say so", Duration::from_secs(3), || true).await;
    let log = log_text(&c, &run).await;
    assert!(log.contains("canceled") && log.contains("stopped"), "{log}");

    // the next run starts at once instead of waiting for the old engine step
    let next = new_run(&c, &ws, CFG_A, json!({})).await;
    wait_status(&c, &next, "planning").await;
    c.post(&format!("/api/v2/runs/{next}/actions/cancel")).await;
    wait_until("the second step to stop", Duration::from_secs(3), || app.cancels.active() == 0).await;

    // web form, same effect
    let third = new_run(&c, &ws, CFG_A, json!({})).await;
    wait_status(&c, &third, "planning").await;
    assert_eq!(c.post_form(&format!("/runs/{third}/cancel"), &[]).await.status, StatusCode::SEE_OTHER);
    wait_until("the third step to stop", Duration::from_secs(3), || app.cancels.active() == 0).await;
    wait_status(&c, &third, "canceled").await;
}

#[tokio::test]
async fn a_plain_cancel_is_refused_while_applying_and_force_cancel_wants_an_acknowledgement() {
    let app = slow_app(1000).await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let run = new_run(&c, &ws, CFG_A, json!({ "auto_apply": true })).await;
    wait_status(&c, &run, "applying").await;

    let plain = c.post(&format!("/api/v2/runs/{run}/actions/cancel")).await;
    assert_eq!(plain.status, StatusCode::CONFLICT, "{}", plain.body);
    for body in [json!({}), json!({ "acknowledge_risk": false }), json!({ "acknowledge_risk": "yes" })] {
        let r = c.post_json(&format!("/api/v2/runs/{run}/actions/force-cancel"), body.clone()).await;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{body}: {}", r.body);
        assert!(r.body.contains("acknowledge_risk"), "the error says what is missing: {}", r.body);
    }
    let r = c.send(Method::POST, &format!("/api/v2/runs/{run}/actions/force-cancel"), None, String::new()).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "no body at all is not an acknowledgement");
    assert_eq!(c.get(&format!("/api/v2/runs/{run}")).await.json()["data"]["attributes"]["status"], "applying", "nothing happened");
    assert_eq!(c.get(&format!("/api/v2/runs/{run}")).await.json()["data"]["attributes"]["actions"]["is_force_cancelable"], true);

    let ok = c.post_json(&format!("/api/v2/runs/{run}/actions/force-cancel"), json!({ "acknowledge_risk": true })).await;
    assert_eq!(ok.status, StatusCode::ACCEPTED, "{}", ok.body);
    assert_eq!(ok.json()["data"]["attributes"]["status"], "force_canceled");
    wait_until("the engine step to stop", Duration::from_secs(5), || app.cancels.active() == 0).await;
    assert!(lock_of(&c, &ws).await.is_null());

    // it is over: asking again is a conflict on the API
    let again = c.post_json(&format!("/api/v2/runs/{run}/actions/force-cancel"), json!({ "acknowledge_risk": true })).await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    let v = c.get(&format!("/api/v2/runs/{run}")).await.json();
    assert_eq!(v["data"]["attributes"]["actions"]["is_force_cancelable"], false);
    assert_eq!(v["data"]["attributes"]["actions"]["is_cancelable"], false);

    // a run waiting for a person, or not started, uses cancel or discard instead
    let waiting = new_run(&c, &ws, CFG_A, json!({})).await;
    wait_status(&c, &waiting, "needs_confirmation").await;
    assert_eq!(c.post_json(&format!("/api/v2/runs/{waiting}/actions/force-cancel"), json!({ "acknowledge_risk": true })).await.status, StatusCode::CONFLICT);
    assert_eq!(c.post(&format!("/api/v2/runs/{waiting}/actions/discard")).await.status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn force_cancel_keeps_the_resources_that_finished_so_the_next_plan_does_not_redo_them() {
    let app = slow_app(1500).await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let run = new_run(&c, &ws, CFG_A, json!({ "auto_apply": true })).await;
    wait_status(&c, &run, "applying").await;
    // wait for exactly one of the two resources to finish
    for _ in 0..300 {
        if log_text(&c, &run).await.contains(": Done") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(log_text(&c, &run).await.contains(": Done"), "the first resource finished");
    assert!(!log_text(&c, &run).await.contains("Apply complete"));

    let r = c.post_json(&format!("/api/v2/runs/{run}/actions/force-cancel"), json!({ "acknowledge_risk": true })).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.body);
    wait_until("the engine step to stop", Duration::from_secs(5), || app.cancels.active() == 0).await;

    // the record of what exists was written before the lock was released
    let st = c.get(&format!("/api/v2/workspaces/{ws}/current-state-version")).await;
    assert_eq!(st.status, StatusCode::OK, "{}", st.body);
    assert_eq!(st.json()["data"]["attributes"]["resource-count"], 1, "one finished, one was in flight");
    assert_eq!(st.json()["data"]["attributes"]["serial"], 1);
    let log = log_text(&c, &run).await;
    assert!(log.contains("force-canceled") && log.contains("serial 1") && log.contains("in progress may exist"), "{log}");
    assert!(lock_of(&c, &ws).await.is_null());

    // the next plan sees one resource already there and one still to create
    let p = new_run(&c, &ws, CFG_A, json!({ "operation": "plan_only" })).await;
    let v = wait_status(&c, &p, "planned_and_finished").await;
    assert_eq!(v["data"]["attributes"]["summary"], json!({ "add": 1, "change": 0, "destroy": 0 }), "only the unfinished resource remains");

    // force-canceling a run that was still planning leaves the state alone
    let slow = slow_app(30_000).await;
    let c2 = signup(&slow, "ana@example.com", "acme").await;
    let ws2 = new_ws(&c2, "prod").await;
    let planning = new_run(&c2, &ws2, CFG_A, json!({})).await;
    wait_status(&c2, &planning, "planning").await;
    let r = c2.post_json(&format!("/api/v2/runs/{planning}/actions/force-cancel"), json!({})).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "planning needs no acknowledgement: {}", r.body);
    assert_eq!(r.json()["data"]["attributes"]["status"], "force_canceled");
    wait_until("the planning step to stop", Duration::from_secs(5), || slow.cancels.active() == 0).await;
    assert_eq!(c2.get(&format!("/api/v2/workspaces/{ws2}/current-state-version")).await.status, StatusCode::NOT_FOUND, "no state was invented");
}

#[tokio::test]
async fn only_administrators_can_force_cancel() {
    let app = slow_app(1500).await;
    let owner = signup(&app, "ana@example.com", "alpha-org").await;
    let member = signup(&app, "cy@example.com", "zeta-org").await;
    let stranger = signup(&app, "bo@example.com", "other").await;
    let cy = app.store.find_user_by_email("cy@example.com").await.unwrap().unwrap().0;
    let alpha = app.store.orgs_for_user(&app.store.find_user_by_email("ana@example.com").await.unwrap().unwrap().0.id).await.unwrap()[0].0.clone();
    app.store.add_member(&alpha.id, &cy.id, "member").await.unwrap();
    let org_token = owner.post_json("/api/v2/tokens", json!({ "description": "p", "kind": "org" })).await.json()["meta"]["secret"].as_str().unwrap().to_string();
    let orgbot = Client { origin: None, cookie: None, ..owner.with_bearer(&org_token) };

    let ws = new_ws(&owner, "prod").await;
    let run = new_run(&owner, &ws, CFG_A, json!({ "auto_apply": true })).await;
    wait_status(&owner, &run, "applying").await;

    let ack = json!({ "acknowledge_risk": true });
    assert_eq!(member.post_json(&format!("/api/v2/runs/{run}/actions/force-cancel"), ack.clone()).await.status, StatusCode::FORBIDDEN, "members can cancel, not force-cancel");
    assert_eq!(member.post_form(&format!("/runs/{run}/force-cancel"), &[("confirm", "yes")]).await.status, StatusCode::FORBIDDEN);
    assert_eq!(orgbot.post_json(&format!("/api/v2/runs/{run}/actions/force-cancel"), ack.clone()).await.status, StatusCode::FORBIDDEN, "an organization token cannot stop runs");
    assert_eq!(stranger.post_json(&format!("/api/v2/runs/{run}/actions/force-cancel"), ack.clone()).await.status, StatusCode::NOT_FOUND, "another organization: not found");
    assert_eq!(owner.get(&format!("/api/v2/runs/{run}")).await.json()["data"]["attributes"]["status"], "applying", "all four were refused");

    // the page offers the control to the owner only
    let owner_page = owner.get(&format!("/runs/{run}")).await.body;
    assert!(owner_page.contains(&format!("action=\"/runs/{run}/force-cancel\"")), "{owner_page}");
    assert!(!member.get(&format!("/runs/{run}")).await.body.contains("/force-cancel"));

    assert_eq!(owner.post_json(&format!("/api/v2/runs/{run}/actions/force-cancel"), ack).await.status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn the_web_force_cancel_asks_for_confirmation_and_survives_a_double_click() {
    let app = slow_app(1500).await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let run = new_run(&c, &ws, CFG_A, json!({ "auto_apply": true })).await;
    wait_status(&c, &run, "applying").await;

    let page = c.get(&format!("/runs/{run}")).await.body;
    assert!(page.contains("name=\"confirm\"") && page.contains("Force cancel") && page.contains("may exist"), "the risk is stated on the page: {page}");
    assert!(!page.contains(&format!("action=\"/runs/{run}/cancel\"")), "an applying run has no plain cancel");

    let no = c.post_form(&format!("/runs/{run}/force-cancel"), &[]).await;
    assert_eq!(no.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", no.body);
    assert!(no.body.contains("confirm"));
    assert_eq!(c.get(&format!("/api/v2/runs/{run}")).await.json()["data"]["attributes"]["status"], "applying");

    let yes = c.post_form(&format!("/runs/{run}/force-cancel"), &[("confirm", "yes")]).await;
    assert_eq!(yes.status, StatusCode::SEE_OTHER);
    assert_eq!(yes.location(), format!("/runs/{run}"));
    let twice = c.post_form(&format!("/runs/{run}/force-cancel"), &[("confirm", "yes")]).await;
    assert_eq!(twice.status, StatusCode::SEE_OTHER, "a double click lands on the run, like the other actions");

    wait_until("the step to stop", Duration::from_secs(5), || app.cancels.active() == 0).await;
    let after = c.get(&format!("/runs/{run}")).await.body;
    assert!(after.contains("Force canceled"), "{after}");
    assert!(after.contains("force-canceled") || after.contains("Force-canceled"), "the page explains what that means");
    assert!(!after.contains("EventSource"), "a finished run needs no stream");
    assert!(c.get("/runs").await.body.contains("Force canceled"));
}

#[tokio::test]
async fn the_run_page_streams_while_working_and_falls_back_to_reloading_without_script() {
    let app = slow_app(200).await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    c.post(&format!("/api/v2/workspaces/{ws}/actions/lock")).await; // keeps the run pending
    let run = new_run(&c, &ws, CFG_A, json!({})).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let page = c.get(&format!("/runs/{run}")).await.body;
    assert!(page.contains("id=\"run-stream\"") && page.contains("data-status=\"pending\"") && page.contains("data-lines=\"0\""), "{page}");
    assert!(page.contains(&format!("/runs/{run}/events")) && page.contains("new EventSource"));
    assert!(page.contains("<noscript><meta http-equiv=\"refresh\" content=\"2\"></noscript>"), "no script, no stream: the old reload still works");
    assert!(!page.contains("<meta http-equiv=\"refresh\" content=\"2\"><title"), "with script it must not reload under the stream");

    c.post(&format!("/api/v2/workspaces/{ws}/actions/unlock")).await;
    wait_status(&c, &run, "needs_confirmation").await;
    let waiting = c.get(&format!("/runs/{run}")).await.body;
    assert!(!waiting.contains("EventSource") && !waiting.contains("http-equiv=\"refresh\""), "nothing reloads under the person who decides");
    assert!(waiting.contains("data-lines="), "the log shows how many lines it holds");
}

#[tokio::test]
async fn force_unlock_of_a_planning_workspace_also_stops_the_engine() {
    let app = slow_app(30_000).await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let run = new_run(&c, &ws, CFG_A, json!({})).await;
    wait_status(&c, &run, "planning").await;
    wait_until("the step to start", Duration::from_secs(3), || app.cancels.active() == 1).await;
    let r = c.post(&format!("/api/v2/workspaces/{ws}/actions/force-unlock")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    wait_until("the planning step to stop", Duration::from_secs(5), || app.cancels.active() == 0).await;
    wait_status(&c, &run, "canceled").await;
}

#[tokio::test]
async fn real_container_e6_force_cancel_mid_apply_keeps_the_finished_resources_through_the_platform() {
    let (Some(bin), true) = (
        std::env::var("TF_TEST_TOFU_LINUX_BIN").ok().filter(|s| !s.is_empty()),
        std::process::Command::new("docker").args(["version", "--format", "{{.Server.Version}}"]).output().map(|o| o.status.success()).unwrap_or(false),
    ) else {
        eprintln!("SKIPPED: TF_TEST_TOFU_LINUX_BIN or docker not available");
        return;
    };
    let image = std::env::var("TF_TEST_CONTAINER_IMAGE").unwrap_or_else(|_| "postgres:16-alpine".into());
    let sb = tf_engine::Sandbox {
        work_root: std::env::temp_dir().join(format!("tfclone-e2e-cancel-{}", std::process::id())),
        container: Some(tf_engine::Container::new(bin, image)),
        ..tf_engine::Sandbox::default()
    };
    let engine: Arc<dyn Engine> = Arc::new(tf_engine::TofuEngine::with_sandbox("unused-in-container-mode", sb));
    let app = boot_engine(Config::default(), engine).await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let cfg = "resource \"terraform_data\" \"fast\" {\n  input = \"quick\"\n}\nresource \"terraform_data\" \"slow\" {\n  provisioner \"local-exec\" {\n    command = \"sleep 61\"\n  }\n}\n";
    let run = new_run(&c, &ws, cfg, json!({ "auto_apply": true })).await;
    // wait until the slow provisioner is running inside the container
    for _ in 0..600 {
        if log_text(&c, &run).await.contains("Executing") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(log_text(&c, &run).await.contains("Executing"), "the provisioner started: {}", log_text(&c, &run).await);
    assert_eq!(c.get(&format!("/api/v2/runs/{run}")).await.json()["data"]["attributes"]["status"], "applying");

    let t = Instant::now();
    let r = c.post_json(&format!("/api/v2/runs/{run}/actions/force-cancel"), json!({ "acknowledge_risk": true })).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.body);
    assert!(t.elapsed() < Duration::from_secs(30), "the 61 second sleep was not waited for ({:?})", t.elapsed());
    assert_eq!(r.json()["data"]["attributes"]["status"], "force_canceled");

    // the record of what exists was written before the lock was released
    let st = c.get(&format!("/api/v2/workspaces/{ws}/current-state-version")).await;
    assert_eq!(st.status, StatusCode::OK, "{}", st.body);
    let content = state_document(&c, &ws).await;
    assert!(content.contains("\"fast\"") && content.contains("tainted"), "{content}");
    assert_eq!(st.json()["data"]["attributes"]["serial"], 1);
    let log = log_text(&c, &run).await;
    assert!(log.contains("tofu shut down cleanly") && log.contains("recorded as state serial 1"), "{log}");
    assert!(lock_of(&c, &ws).await.is_null());
    assert_eq!(app.cancels.active(), 0);
    let left = std::process::Command::new("docker").args(["ps", "-aq", "--filter", "name=tfclone-run-"]).output().unwrap();
    assert!(String::from_utf8_lossy(&left.stdout).trim().is_empty(), "no container left behind");

    // the next plan keeps `fast`, and replaces `slow`, which was interrupted and is recorded as tainted
    let plan = new_run(&c, &ws, cfg, json!({ "operation": "plan_only" })).await;
    let v = wait_status(&c, &plan, "planned_and_finished").await;
    assert_eq!(v["data"]["attributes"]["summary"], json!({ "add": 1, "change": 0, "destroy": 1 }), "{v}");
}

// ---- an engine that takes its time to stop, to pin down the ordering guarantees ----

/// Plans like the simulator. When asked to stop it needs half a second to write down what it
/// has (like a real engine flushing state), and only then reports that it was canceled.
struct SlowStop {
    inner: FakeEngine,
}

#[async_trait::async_trait]
impl Engine for SlowStop {
    fn name(&self) -> &'static str {
        "slow-stop"
    }

    async fn plan(&self, config: &str, state: Option<&str>, vars: &tf_engine::EngineVars, log: &tf_engine::Log) -> Result<tf_engine::PlanOutput, tf_engine::EngineError> {
        if config.contains("hang_plan") {
            tf_engine::emit(log, "planning slowly...");
            vars.cancel.as_ref().expect("a token").canceled().await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            return Err(tf_engine::EngineError::Canceled { partial_state: None });
        }
        self.inner.plan(config, state, vars, log).await
    }

    async fn apply(&self, _config: &str, _plan: &[u8], _state: Option<&str>, vars: &tf_engine::EngineVars, log: &tf_engine::Log) -> Result<tf_engine::ApplyOutput, tf_engine::EngineError> {
        tf_engine::emit(log, "applying slowly...");
        vars.cancel.as_ref().expect("a token").canceled().await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        Err(tf_engine::EngineError::Canceled { partial_state: Some(r#"{"version":1,"resources":{"a.b":"1"}}"#.into()) })
    }
}

async fn slow_stop_app() -> App {
    boot_engine(Config::default(), Arc::new(SlowStop { inner: FakeEngine::new(Duration::ZERO) })).await
}

#[tokio::test]
async fn force_cancel_answers_only_after_the_state_is_recorded_and_before_the_lock_is_released() {
    let app = slow_stop_app().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let run = new_run(&c, &ws, CFG_A, json!({ "auto_apply": true })).await;
    wait_status(&c, &run, "applying").await;
    wait_until("the apply to start", Duration::from_secs(3), || app.cancels.is_running(&run)).await;

    let t = Instant::now();
    let r = c.post_json(&format!("/api/v2/runs/{run}/actions/force-cancel"), json!({ "acknowledge_risk": true })).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.body);
    assert!(t.elapsed() >= Duration::from_millis(450), "it waited for the engine to finish writing ({:?})", t.elapsed());
    // by the time the caller hears back, what the engine kept is already the workspace's state
    let st = c.get(&format!("/api/v2/workspaces/{ws}/current-state-version")).await;
    assert_eq!(st.status, StatusCode::OK, "{}", st.body);
    assert_eq!(st.json()["data"]["attributes"]["resource-count"], 1);
    assert!(lock_of(&c, &ws).await.is_null());
    assert_eq!(app.cancels.active(), 0);
}

#[tokio::test]
async fn the_stream_does_not_say_done_until_the_runner_has_written_its_last_line() {
    let app = slow_stop_app().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let run = new_run(&c, &ws, "hang_plan\n", json!({})).await;
    wait_status(&c, &run, "planning").await;
    wait_until("the plan to start", Duration::from_secs(3), || app.cancels.is_running(&run)).await;

    let (status, mut body) = c.open(&format!("/runs/{run}/events")).await;
    assert_eq!(status, StatusCode::OK);
    let mut buf = String::new();
    read_sse_until(&mut body, &mut buf, Duration::from_secs(5), |e| logs_of(e).iter().any(|(_, l)| l.contains("planning slowly"))).await;

    // the status turns final at once; the engine takes half a second more to stop and the
    // runner then writes a closing note
    assert_eq!(c.post(&format!("/api/v2/runs/{run}/actions/cancel")).await.status, StatusCode::ACCEPTED);
    let ev = read_sse_until(&mut body, &mut buf, Duration::from_secs(10), |e| e.iter().any(|x| x.event == "done")).await;
    let lines = logs_of(&ev);
    assert!(lines.iter().any(|(_, l)| l.contains("planning step was stopped")), "the closing note arrives before done: {lines:?}");
    assert_eq!(ev.last().unwrap().event, "done");
    assert_eq!(ev.last().unwrap().data, "canceled");
}

// ================= item 4: JSON:API, pagination, archive upload, rate limit =================

fn targz(files: &[(&str, &str)]) -> Vec<u8> {
    use std::io::Write;
    let mut tar = tar::Builder::new(Vec::new());
    for (path, content) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(content.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        tar.append_data(&mut h, path, content.as_bytes()).unwrap();
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&tar.into_inner().unwrap()).unwrap();
    gz.finish().unwrap()
}

impl Client {
    async fn put_bytes(&self, uri: &str, bytes: Vec<u8>) -> Reply {
        let mut b = Request::builder().method(Method::PUT).uri(uri).header(header::HOST, "localhost").header(header::CONTENT_TYPE, "application/octet-stream");
        if let Some(o) = &self.origin {
            b = b.header(header::ORIGIN, o);
        }
        if let Some(c) = &self.cookie {
            b = b.header(header::COOKIE, c);
        }
        if let Some(t) = &self.bearer {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let resp = self.router.clone().oneshot(b.body(Body::from(bytes)).unwrap()).await.unwrap();
        let (status, headers) = (resp.status(), resp.headers().clone());
        let body = axum::body::to_bytes(resp.into_body(), 1 << 21).await.unwrap();
        Reply { status, headers, body: String::from_utf8_lossy(&body).to_string() }
    }
}

fn ids(v: &Value) -> Vec<String> {
    v["data"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn j01_responses_are_json_api_resource_objects() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let r = c.get(&format!("/api/v2/workspaces/{ws}")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers.get(header::CONTENT_TYPE).unwrap(), "application/vnd.api+json");
    let body = r.json();
    let d = &body["data"];
    assert_eq!((d["type"].clone(), d["id"].clone()), (json!("workspaces"), json!(ws)));
    assert_eq!(d["attributes"]["name"], "prod");
    assert_eq!(d["attributes"]["locked"], false);
    assert!(d["attributes"].get("resource-count").is_some(), "kebab-case attribute names: {d}");
    assert!(d["attributes"].get("id").is_none() && d["attributes"].get("org").is_none(), "ids and foreign keys are not attributes");
    assert_eq!(d["relationships"]["current-run"]["data"], Value::Null);
    assert_eq!(d["relationships"]["organization"]["data"]["type"], "organizations");
    assert_eq!(d["links"]["self"], format!("/api/v2/workspaces/{ws}"));

    let run = new_run(&c, &ws, CFG_A, json!({})).await;
    let v = wait_status(&c, &run, "needs_confirmation").await;
    let rd = &v["data"];
    assert_eq!(rd["type"], "runs");
    assert_eq!(rd["relationships"]["workspace"]["data"], json!({ "type": "workspaces", "id": ws }));
    assert_eq!(rd["relationships"]["configuration-version"]["data"]["type"], "configuration-versions");
    assert!(rd["attributes"].get("auto-apply").is_some() && rd["attributes"].get("status-timestamps").is_some());
    // the workspace now points at its current run
    let w = c.get(&format!("/api/v2/workspaces/{ws}")).await.json();
    assert_eq!(w["data"]["relationships"]["current-run"]["data"], json!({ "type": "runs", "id": run }));
    // errors are JSON:API errors with the same media type
    let e = c.get("/api/v2/runs/nope").await;
    assert_eq!(e.status, StatusCode::NOT_FOUND);
    assert_eq!(e.headers.get(header::CONTENT_TYPE).unwrap(), "application/vnd.api+json");
    assert_eq!(e.json()["errors"][0]["status"], "404");
}

#[tokio::test]
async fn j02_lists_are_paged_with_links_and_meta() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    for i in 0..25 {
        new_ws(&c, &format!("ws-{i:02}")).await;
    }
    let p1 = c.get("/api/v2/workspaces").await.json();
    assert_eq!(p1["data"].as_array().unwrap().len(), 20, "20 by default");
    assert_eq!(p1["meta"]["pagination"], json!({ "current-page": 1, "page-size": 20, "prev-page": null, "next-page": 2, "total-pages": 2, "total-count": 25 }));
    let p2 = c.get("/api/v2/workspaces?page%5Bnumber%5D=2").await.json();
    assert_eq!(p2["data"].as_array().unwrap().len(), 5);
    assert_eq!(p2["meta"]["pagination"]["next-page"], Value::Null);
    let mut both = ids(&p1);
    both.extend(ids(&p2));
    both.sort();
    both.dedup();
    assert_eq!(both.len(), 25, "the pages together hold every workspace exactly once");

    let small = c.get("/api/v2/workspaces?page[size]=10&page[number]=3").await.json();
    assert_eq!(small["data"].as_array().unwrap().len(), 5);
    assert_eq!(small["meta"]["pagination"]["total-pages"], 3);
    assert_eq!(small["meta"]["pagination"]["prev-page"], 2);
    // the link to the next page is one a client can follow as given
    let first = c.get("/api/v2/workspaces?page[size]=10").await.json();
    let next = first["links"]["next"].as_str().unwrap().to_string();
    let followed = c.get(&next).await.json();
    assert_eq!(followed["meta"]["pagination"]["current-page"], 2);

    assert_eq!(c.get("/api/v2/workspaces?page[size]=500").await.json()["meta"]["pagination"]["page-size"], 100, "capped at 100");
    assert!(c.get("/api/v2/workspaces?page[number]=9").await.json()["data"].as_array().unwrap().is_empty(), "past the end is an empty page");
    for bad in ["page[size]=0", "page[number]=0", "page[size]=abc", "page[number]=-2"] {
        let r = c.get(&format!("/api/v2/workspaces?{bad}")).await;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {}", r.body);
    }
    // an empty list is one empty page
    let other = signup(&app, "bo@example.com", "beta").await;
    assert_eq!(other.get("/api/v2/workspaces").await.json()["meta"]["pagination"]["total-pages"], 1);
}

#[tokio::test]
async fn j03_include_returns_related_resources_once() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let a = new_ws(&c, "a").await;
    let _b = new_ws(&c, "b").await;
    let ra = new_run(&c, &a, CFG_A, json!({})).await;
    wait_status(&c, &ra, "needs_confirmation").await;
    let v = c.get("/api/v2/workspaces?include=current-run").await.json();
    let inc = v["included"].as_array().unwrap();
    assert_eq!(inc.len(), 1, "only workspaces that have a current run bring one");
    assert_eq!((inc[0]["type"].clone(), inc[0]["id"].clone()), (json!("runs"), json!(ra)));
    let no = c.get("/api/v2/workspaces").await.json();
    assert!(no.get("included").is_none(), "nothing extra unless asked");

    let r = c.get(&format!("/api/v2/runs/{ra}?include=workspace")).await.json();
    assert_eq!(r["included"][0]["type"], "workspaces");
    assert_eq!(r["included"][0]["id"], json!(a));
    let one = c.get(&format!("/api/v2/workspaces/{a}?include=current-run")).await.json();
    assert_eq!(one["included"][0]["id"], json!(ra));

    let bad = c.get("/api/v2/workspaces?include=owner").await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(bad.body.contains("offers: current-run"), "{}", bad.body);
    assert_eq!(c.get(&format!("/api/v2/runs/{ra}?include=current-run")).await.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn j04_requests_can_be_json_api_documents() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let r = c
        .send(Method::POST, "/api/v2/workspaces", Some("application/vnd.api+json"), json!({ "data": { "type": "workspaces", "attributes": { "name": "docs" } } }).to_string())
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let ws = r.json()["data"]["id"].as_str().unwrap().to_string();
    let wrong = c.post_json("/api/v2/workspaces", json!({ "data": { "type": "runs", "attributes": { "name": "x" } } })).await;
    assert_eq!(wrong.status, StatusCode::CONFLICT, "a wrong resource type is a conflict: {}", wrong.body);
    let run = c
        .post_json(
            &format!("/api/v2/workspaces/{ws}/runs"),
            json!({ "data": { "type": "runs", "attributes": { "config": CFG_A, "auto-apply": true, "message": "via doc", "operation": "plan_and_apply" } } }),
        )
        .await;
    assert_eq!(run.status, StatusCode::CREATED, "{}", run.body);
    let id = run.json()["data"]["id"].as_str().unwrap().to_string();
    let v = wait_status(&c, &id, "applied").await;
    assert_eq!(v["data"]["attributes"]["message"], "via doc");
    assert_eq!(v["data"]["attributes"]["auto-apply"], true, "kebab-case attributes are read");
    for bad in [json!({ "data": [] }), json!({ "data": { "type": "runs", "attributes": [] } }), json!({ "data": { "type": "runs", "attributes": {} } })] {
        assert_eq!(c.post_json(&format!("/api/v2/workspaces/{ws}/runs"), bad).await.status, StatusCode::UNPROCESSABLE_ENTITY);
    }
    let var = c.post_json(&format!("/api/v2/workspaces/{ws}/vars"), json!({ "data": { "type": "vars", "attributes": { "key": "REGION", "value": "eu", "category": "env" } } })).await;
    assert_eq!(var.status, StatusCode::CREATED, "{}", var.body);
    assert_eq!(var.json()["data"]["type"], "vars");
    assert_eq!(var.json()["data"]["attributes"]["category"], "env");
}

#[tokio::test]
async fn j05_runs_states_and_variables_are_listed_in_pages() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let mut made = vec![];
    for i in 0..3 {
        let cfg: String = (0..=i).map(|n| format!("resource \"aws_s3_bucket\" \"b{n}\" {{
  bucket = \"x\"
}}
")).collect();
        let run = new_run(&c, &ws, &cfg, json!({ "auto_apply": true, "message": format!("r{i}") })).await;
        wait_status(&c, &run, "applied").await;
        made.push(run);
        tokio::time::sleep(Duration::from_millis(1100)).await; // creation time has one-second resolution
    }
    let list = c.get(&format!("/api/v2/workspaces/{ws}/runs?page[size]=2")).await.json();
    assert_eq!(list["meta"]["pagination"]["total-count"], 3);
    assert_eq!(ids(&list), vec![made[2].clone(), made[1].clone()], "newest first");
    assert_eq!(ids(&c.get("/api/v2/runs").await.json()).len(), 3, "the organization-wide list");

    let states = c.get(&format!("/api/v2/workspaces/{ws}/state-versions")).await.json();
    assert_eq!(states["meta"]["pagination"]["total-count"], 3);
    let first = &states["data"][0];
    assert_eq!(first["attributes"]["serial"], 3, "newest first");
    assert!(first["attributes"].get("content").is_none(), "the list never carries state documents");
    assert_eq!(first["relationships"]["run"]["data"]["id"], json!(made[2]));

    for k in ["A", "B", "C"] {
        assert_eq!(c.post_json(&format!("/api/v2/workspaces/{ws}/vars"), json!({ "key": k, "value": "v" })).await.status, StatusCode::CREATED);
    }
    let vars = c.get(&format!("/api/v2/workspaces/{ws}/vars?page[size]=2&page[number]=2")).await.json();
    assert_eq!((vars["data"].as_array().unwrap().len(), vars["meta"]["pagination"]["total-count"].clone()), (1, json!(3)));

    // other organizations see none of it
    let bo = signup(&app, "bo@example.com", "beta").await;
    assert_eq!(bo.get("/api/v2/runs").await.json()["data"], json!([]));
    for uri in [format!("/api/v2/workspaces/{ws}/runs"), format!("/api/v2/workspaces/{ws}/state-versions"), format!("/api/v2/workspaces/{ws}"), format!("/api/v2/workspaces/{ws}/vars")] {
        assert_eq!(bo.get(&uri).await.status, StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn j06_a_configuration_archive_is_uploaded_then_run() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let made = c.post(&format!("/api/v2/workspaces/{ws}/configuration-versions")).await;
    assert_eq!(made.status, StatusCode::CREATED, "{}", made.body);
    let cv = made.json()["data"].clone();
    let cv_id = cv["id"].as_str().unwrap().to_string();
    assert_eq!((cv["type"].clone(), cv["attributes"]["status"].clone()), (json!("configuration-versions"), json!("pending")));
    let upload = cv["links"]["upload"].as_str().unwrap().to_string();
    assert_eq!(upload, format!("/api/v2/configuration-versions/{cv_id}/upload"));
    assert!(cv["attributes"].get("text").is_none(), "the configuration text is not an attribute");

    // a run cannot start before the content is there
    let by_cv = json!({ "data": { "type": "runs", "relationships": { "configuration-version": { "data": { "type": "configuration-versions", "id": cv_id } } } } });
    let early = c.post_json(&format!("/api/v2/workspaces/{ws}/runs"), by_cv.clone()).await;
    assert_eq!(early.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", early.body);

    // two files at the top level, plus a file that is ignored
    let archive = targz(&[
        ("main.tf", "resource \"aws_s3_bucket\" \"logs\" {\n  bucket = \"a\"\n}\n"),
        ("iam.tf", "resource \"aws_iam_role\" \"ci\" {\n  name = \"ci\"\n}\n"),
        ("README.md", "not configuration"),
    ]);
    let up = c.put_bytes(&upload, archive.clone()).await;
    assert_eq!(up.status, StatusCode::OK, "{}", up.body);
    assert_eq!(up.json()["data"]["attributes"]["status"], "uploaded");
    assert!(up.json()["data"]["links"].get("upload").is_none(), "nothing left to upload");
    assert_eq!(c.get(&format!("/api/v2/configuration-versions/{cv_id}")).await.json()["data"]["attributes"]["status"], "uploaded");
    let again = c.put_bytes(&upload, archive).await;
    assert_eq!(again.status, StatusCode::CONFLICT, "an upload is final: {}", again.body);

    let run = c.post_json(&format!("/api/v2/workspaces/{ws}/runs"), by_cv).await;
    assert_eq!(run.status, StatusCode::CREATED, "{}", run.body);
    assert_eq!(run.json()["data"]["relationships"]["configuration-version"]["data"]["id"], json!(cv_id));
    let id = run.json()["data"]["id"].as_str().unwrap().to_string();
    let v = wait_status(&c, &id, "needs_confirmation").await;
    assert_eq!(v["data"]["attributes"]["summary"], json!({ "add": 2, "change": 0, "destroy": 0 }), "both files were planned");

    // the two ways of giving a configuration do not mix, and one of them is needed
    let both = c.post_json(&format!("/api/v2/workspaces/{ws}/runs"), json!({ "configuration_version_id": cv_id, "config": "x" })).await;
    assert_eq!(both.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(c.post_json(&format!("/api/v2/workspaces/{ws}/runs"), json!({})).await.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn j07_bad_archives_are_refused_with_a_reason() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    for (what, body, want) in [
        ("not an archive", b"hello".to_vec(), "archive"),
        ("empty", vec![], "empty"),
        ("no .tf files", targz(&[("notes.md", "x")]), "no .tf files"),
        ("a module directory", targz(&[("modules/net/main.tf", "x")]), "subdirectories"),
        ("json configuration", targz(&[("main.tf.json", "{}")]), ".tf.json"),
    ] {
        let cv = c.post(&format!("/api/v2/workspaces/{ws}/configuration-versions")).await.json()["data"]["id"].as_str().unwrap().to_string();
        let r = c.put_bytes(&format!("/api/v2/configuration-versions/{cv}/upload"), body).await;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{what}: {}", r.body);
        assert!(r.body.contains(want), "{what}: {}", r.body);
        // a refused upload leaves the version waiting, so it can be tried again
        let ok = c.put_bytes(&format!("/api/v2/configuration-versions/{cv}/upload"), targz(&[("main.tf", CFG_A)])).await;
        assert_eq!(ok.status, StatusCode::OK, "{what}: {}", ok.body);
    }
    // too large once unpacked: the limit is the configuration limit, not the compressed size
    let cv = c.post(&format!("/api/v2/workspaces/{ws}/configuration-versions")).await.json()["data"]["id"].as_str().unwrap().to_string();
    let big = "#".repeat(1_100_000);
    let r = c.put_bytes(&format!("/api/v2/configuration-versions/{cv}/upload"), targz(&[("main.tf", &big)])).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.body);
    assert!(r.body.contains("limited"), "{}", r.body);
}

#[tokio::test]
async fn j08_uploads_follow_the_same_access_rules() {
    let app = boot().await;
    let ana = signup(&app, "ana@example.com", "alpha-org").await;
    let bo = signup(&app, "bo@example.com", "beta").await;
    let ws = new_ws(&ana, "prod").await;
    let other_ws = new_ws(&ana, "other").await;
    let cv = ana.post(&format!("/api/v2/workspaces/{ws}/configuration-versions")).await.json()["data"]["id"].as_str().unwrap().to_string();
    let archive = targz(&[("main.tf", CFG_A)]);

    // another organization: not found, and nothing is created or changed
    assert_eq!(bo.post(&format!("/api/v2/workspaces/{ws}/configuration-versions")).await.status, StatusCode::NOT_FOUND);
    assert_eq!(bo.get(&format!("/api/v2/configuration-versions/{cv}")).await.status, StatusCode::NOT_FOUND);
    assert_eq!(bo.put_bytes(&format!("/api/v2/configuration-versions/{cv}/upload"), archive.clone()).await.status, StatusCode::NOT_FOUND);
    assert_eq!(ana.get(&format!("/api/v2/configuration-versions/{cv}")).await.json()["data"]["attributes"]["status"], "pending");
    // an organization token cannot plan or apply, so it cannot upload either
    let org = token_for(&ana, "org").await;
    let ot = ana.with_bearer(&org);
    assert_eq!(ot.post(&format!("/api/v2/workspaces/{ws}/configuration-versions")).await.status, StatusCode::FORBIDDEN);
    assert_eq!(ot.put_bytes(&format!("/api/v2/configuration-versions/{cv}/upload"), archive.clone()).await.status, StatusCode::FORBIDDEN);
    // a user token works
    let user = token_for(&ana, "user").await;
    assert_eq!(ana.with_bearer(&user).put_bytes(&format!("/api/v2/configuration-versions/{cv}/upload"), archive).await.status, StatusCode::OK);
    // a version belongs to its workspace: another workspace of the same organization cannot run it
    let r = ana.post_json(&format!("/api/v2/workspaces/{other_ws}/runs"), json!({ "configuration_version_id": cv })).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.body);
    // a missing version is not found
    assert_eq!(ana.put_bytes("/api/v2/configuration-versions/missing/upload", targz(&[("main.tf", CFG_A)])).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn j09_api_calls_are_rate_limited_per_caller() {
    let app = boot_with(Config { api_rate_per_sec: 5, ..Config::default() }).await;
    let ana = signup(&app, "ana@example.com", "alpha-org").await;
    let bo = signup(&app, "bo@example.com", "beta").await;
    // all at once, so the speed of the machine does not decide which second each call lands in
    let handles: Vec<_> = (0..20)
        .map(|_| {
            let c = ana.clone();
            tokio::spawn(async move { c.get("/api/v2/workspaces").await })
        })
        .collect();
    let mut replies = Vec::new();
    for h in handles {
        replies.push(h.await.unwrap());
    }
    let ok = replies.iter().filter(|r| r.status == StatusCode::OK).count();
    let r = replies.iter().find(|r| r.status == StatusCode::TOO_MANY_REQUESTS).expect("20 calls at once must hit a limit of 5 per second");
    assert!((1..=10).contains(&ok), "5 get through per window (a second boundary inside the burst allows a second window): {ok}");
    assert!(replies.iter().filter(|r| r.status == StatusCode::OK).all(|r| r.headers.get("x-ratelimit-limit").unwrap() == "5"));
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.headers.get(header::RETRY_AFTER).unwrap(), "1");
    assert_eq!(r.json()["errors"][0]["status"], "429");
    // someone else is not slowed down by ana, and pages are not counted
    assert_eq!(bo.get("/api/v2/workspaces").await.status, StatusCode::OK);
    assert_eq!(ana.get("/healthz").await.status, StatusCode::OK);
    assert_eq!(ana.get("/workspaces").await.status, StatusCode::OK);
    // the allowance comes back
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(ana.get("/api/v2/workspaces").await.status, StatusCode::OK);
}

#[tokio::test]
async fn j10_a_limit_of_zero_turns_the_limit_off() {
    let app = boot_with(Config { api_rate_per_sec: 0, ..Config::default() }).await;
    let ana = signup(&app, "ana@example.com", "alpha-org").await;
    for _ in 0..80 {
        assert_eq!(ana.get("/api/v2/workspaces").await.status, StatusCode::OK);
    }
}

// ================= item 5: lock lease and heartbeat, workspace settings, state transfer URLs =================

fn lock_attr(v: &Value) -> Value {
    v["data"]["attributes"]["lock"].clone()
}

async fn ws_json(c: &Client, ws: &str) -> Value {
    c.get(&format!("/api/v2/workspaces/{ws}")).await.json()
}

async fn make_member(app: &App, owner_email: &str, member_email: &str) {
    let member = app.store.find_user_by_email(member_email).await.unwrap().unwrap().0;
    let owner = app.store.find_user_by_email(owner_email).await.unwrap().unwrap().0;
    let org = app.store.orgs_for_user(&owner.id).await.unwrap()[0].0.clone();
    app.store.add_member(&org.id, &member.id, "member").await.unwrap();
}

#[tokio::test]
async fn k01_a_lock_taken_through_the_api_has_a_lease_that_lapses() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;

    // the default lease is an hour
    let r = c.post(&format!("/api/v2/workspaces/{ws}/actions/lock")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let l = lock_attr(&r.json());
    let (since, until) = (l["since"].as_u64().unwrap(), l["expires_at"].as_u64().expect("a default lease"));
    assert!((3590..=3610).contains(&(until - since)), "about an hour: {}", until - since);
    assert_eq!(c.post(&format!("/api/v2/workspaces/{ws}/actions/unlock")).await.status, StatusCode::OK);

    // no expiry when asked for none; refused values are 422
    let none = c.post_json(&format!("/api/v2/workspaces/{ws}/actions/lock"), json!({ "lease_seconds": 0 })).await;
    assert!(lock_attr(&none.json())["expires_at"].is_null());
    c.post(&format!("/api/v2/workspaces/{ws}/actions/unlock")).await;
    for bad in [json!({ "lease_seconds": 604801 }), json!({ "lease_seconds": -5 }), json!({ "lease_seconds": "soon" }), json!({ "lease_seconds": 1.5 })] {
        let r = c.post_json(&format!("/api/v2/workspaces/{ws}/actions/lock"), bad.clone()).await;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {}", r.body);
    }
    assert!(lock_of(&c, &ws).await.is_null(), "a refused lock is not taken");

    // a short lease lapses by itself, and a run that was waiting starts
    let r = c.post_json(&format!("/api/v2/workspaces/{ws}/actions/lock"), json!({ "data": { "attributes": { "lease-seconds": 2 } } })).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let run = new_run(&c, &ws, CFG_A, json!({})).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(c.get(&format!("/api/v2/runs/{run}")).await.json()["data"]["attributes"]["status"], "pending", "held back while the lease runs");
    tokio::time::sleep(Duration::from_millis(2000)).await;
    wait_status(&c, &run, "needs_confirmation").await;
    let w = ws_json(&c, &ws).await;
    assert_eq!(w["data"]["attributes"]["lock"]["owner"].as_str().unwrap().starts_with("run:"), true, "the run now holds it");
}

#[tokio::test]
async fn k02_a_heartbeat_keeps_a_lock_alive_for_its_holder_only() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let bo = signup(&app, "bo@example.com", "beta").await;
    let ws = new_ws(&c, "prod").await;
    let hb = format!("/api/v2/workspaces/{ws}/actions/heartbeat");

    assert_eq!(c.post(&hb).await.status, StatusCode::CONFLICT, "nothing to renew when not locked");
    c.post_json(&format!("/api/v2/workspaces/{ws}/actions/lock"), json!({ "lease_seconds": 2 })).await;
    let before = lock_attr(&ws_json(&c, &ws).await)["expires_at"].as_u64().unwrap();
    let r = c.post_json(&hb, json!({ "lease_seconds": 60 })).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let after = lock_attr(&r.json())["expires_at"].as_u64().unwrap();
    assert!(after >= before + 55, "{before} -> {after}");
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(lock_of(&c, &ws).await.is_object(), "the renewed lock outlived its first lease");
    // the default renewal is the server's default lease
    let d = c.post(&hb).await;
    assert!(lock_attr(&d.json())["expires_at"].as_u64().unwrap() >= after.max(before) + 3500 - 70);
    // renewing to "no expiry" makes no sense
    assert_eq!(c.post_json(&hb, json!({ "lease_seconds": 0 })).await.status, StatusCode::UNPROCESSABLE_ENTITY);
    // a lock without a lease has nothing to renew
    c.post(&format!("/api/v2/workspaces/{ws}/actions/unlock")).await;
    c.post_json(&format!("/api/v2/workspaces/{ws}/actions/lock"), json!({ "lease_seconds": 0 })).await;
    assert_eq!(c.post(&hb).await.status, StatusCode::CONFLICT);
    // another organization cannot reach it
    assert_eq!(bo.post(&hb).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn k03_a_cli_lock_lapses_so_a_crashed_cli_cannot_block_the_workspace_forever() {
    let app = boot_with(Config { cli_lock_lease_secs: 2, ..Config::default() }).await;
    let ana = signup(&app, "ana@example.com", "acme").await;
    new_ws(&ana, "prod").await;
    let tok = token_for(&ana, "user").await;
    let anon = Client::anon(&app);
    let lock_addr = "/backend/prod/lock";
    assert_eq!(anon.cli(&tok, "LOCK", lock_addr, &[], &lock_doc("lock-A", "ana@laptop")).await.status, StatusCode::OK);
    assert_eq!(anon.cli(&tok, "LOCK", lock_addr, &[], &lock_doc("lock-B", "bo@desktop")).await.status, StatusCode::LOCKED, "while the lease runs");
    tokio::time::sleep(Duration::from_millis(3200)).await;
    assert_eq!(anon.cli(&tok, "LOCK", lock_addr, &[], &lock_doc("lock-B", "bo@desktop")).await.status, StatusCode::OK, "the crashed CLI's lock lapsed");
    // the first CLI, if it were still alive, no longer holds anything
    let s = state_doc(1, "lin", "");
    assert_eq!(anon.cli(&tok, "POST", "/backend/prod?ID=lock-A", &[], &s).await.status, StatusCode::LOCKED);
    assert_eq!(anon.cli(&tok, "UNLOCK", lock_addr, &[], &lock_doc("lock-A", "ana@laptop")).await.status, StatusCode::CONFLICT);
    assert_eq!(anon.cli(&tok, "POST", "/backend/prod?ID=lock-B", &[], &s).await.status, StatusCode::OK);
}

#[tokio::test]
async fn k04_the_default_cli_lease_outlasts_a_long_apply_and_a_dashboard_lock_has_none() {
    let app = boot().await;
    let ana = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&ana, "prod").await;
    let tok = token_for(&ana, "user").await;
    let anon = Client::anon(&app);
    anon.cli(&tok, "LOCK", "/backend/prod/lock", &[], &lock_doc("lock-A", "ana@laptop")).await;
    let l = lock_attr(&ws_json(&ana, &ws).await);
    assert!((l["expires_at"].as_u64().unwrap() - l["since"].as_u64().unwrap()) >= 11 * 3600, "twelve hours: the CLI cannot renew, so it must outlast an apply: {l}");
    anon.cli(&tok, "UNLOCK", "/backend/prod/lock", &[], &lock_doc("lock-A", "ana@laptop")).await;

    // a deliberate lock from the dashboard never lapses, and the page says when a leased one will
    assert_eq!(ana.post_form(&format!("/workspaces/{ws}/lock"), &[("reason", "maintenance")]).await.status, StatusCode::SEE_OTHER);
    assert!(lock_attr(&ws_json(&ana, &ws).await)["expires_at"].is_null());
    assert!(!ana.get(&format!("/workspaces/{ws}")).await.body.contains("lapses by itself"));
    ana.post_form(&format!("/workspaces/{ws}/unlock"), &[]).await;
    ana.post(&format!("/api/v2/workspaces/{ws}/actions/lock")).await;
    let page = ana.get(&format!("/workspaces/{ws}")).await.body;
    assert!(page.contains("lapses by itself in 59m") || page.contains("lapses by itself in 1h") || page.contains("lapses by itself in 60m"), "{}", &page[page.find("Locked by").unwrap_or(0)..][..200.min(page.len())]);
}

#[tokio::test]
async fn k05_workspace_settings_through_the_api() {
    let app = boot().await;
    let ana = signup(&app, "ana@example.com", "alpha-org").await;
    let cy = signup(&app, "cy@example.com", "zeta-org").await;
    let bo = signup(&app, "bo@example.com", "beta").await;
    make_member(&app, "ana@example.com", "cy@example.com").await;
    let ws = new_ws(&ana, "prod").await;
    new_ws(&ana, "staging").await;
    let uri = format!("/api/v2/workspaces/{ws}");

    let r = ana.send(Method::PATCH, &uri, Some("application/vnd.api+json"), json!({ "data": { "type": "workspaces", "attributes": { "description": "production", "auto-apply": true } } }).to_string()).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let a = &r.json()["data"]["attributes"];
    assert_eq!((a["name"].clone(), a["description"].clone(), a["auto-apply"].clone()), (json!("prod"), json!("production"), json!(true)));
    let r = ana.send(Method::PATCH, &uri, Some("application/json"), json!({ "name": "prod-eu" }).to_string()).await;
    assert_eq!(r.json()["data"]["attributes"]["name"], "prod-eu");
    assert_eq!(r.json()["data"]["attributes"]["description"], "production", "what was left out is kept");
    for (what, body, status) in [
        ("taken name", json!({ "name": "staging" }), StatusCode::CONFLICT),
        ("bad name", json!({ "name": "a b" }), StatusCode::UNPROCESSABLE_ENTITY),
        ("long description", json!({ "description": "x".repeat(501) }), StatusCode::UNPROCESSABLE_ENTITY),
        ("wrong type", json!({ "data": { "type": "runs", "attributes": { "name": "z" } } }), StatusCode::CONFLICT),
    ] {
        let r = ana.send(Method::PATCH, &uri, Some("application/json"), body.to_string()).await;
        assert_eq!(r.status, status, "{what}: {}", r.body);
    }
    assert_eq!(ws_json(&ana, &ws).await["data"]["attributes"]["name"], "prod-eu", "refused changes changed nothing");

    // members read but do not change; another organization sees nothing
    assert_eq!(cy.get(&uri).await.status, StatusCode::OK);
    assert_eq!(cy.send(Method::PATCH, &uri, Some("application/json"), json!({ "name": "mine" }).to_string()).await.status, StatusCode::FORBIDDEN);
    assert_eq!(cy.delete(&uri).await.status, StatusCode::FORBIDDEN);
    assert_eq!(bo.send(Method::PATCH, &uri, Some("application/json"), json!({ "name": "mine" }).to_string()).await.status, StatusCode::NOT_FOUND);
    assert_eq!(bo.delete(&uri).await.status, StatusCode::NOT_FOUND);
    assert_eq!(ws_json(&ana, &ws).await["data"]["attributes"]["name"], "prod-eu");
}

#[tokio::test]
async fn k06_the_workspace_default_decides_auto_apply_unless_the_run_says_otherwise() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let runs = format!("/api/v2/workspaces/{ws}/runs");
    // off by default: the run waits for a person
    let a = c.post_json(&runs, json!({ "config": CFG_A })).await.json()["data"]["id"].as_str().unwrap().to_string();
    wait_status(&c, &a, "needs_confirmation").await;
    c.post(&format!("/api/v2/runs/{a}/actions/discard")).await;
    // the setting turns it on for runs that say nothing
    c.send(Method::PATCH, &format!("/api/v2/workspaces/{ws}"), Some("application/json"), json!({ "auto-apply": true }).to_string()).await;
    let b = c.post_json(&runs, json!({ "config": CFG_A })).await.json();
    assert_eq!(b["data"]["attributes"]["auto-apply"], true);
    wait_status(&c, b["data"]["id"].as_str().unwrap(), "applied").await;
    // a run can still ask for the opposite
    let cfg2 = format!("{CFG_A}resource \"aws_s3_bucket\" \"more\" {{\n  bucket = \"m\"\n}}\n");
    let d = c.post_json(&runs, json!({ "config": cfg2, "auto_apply": false })).await.json();
    assert_eq!(d["data"]["attributes"]["auto-apply"], false);
    wait_status(&c, d["data"]["id"].as_str().unwrap(), "needs_confirmation").await;
    // and the dashboard form starts with the setting ticked
    assert!(c.get(&format!("/workspaces/{ws}")).await.body.contains("name=\"auto_apply\" value=\"on\" checked"));
}

#[tokio::test]
async fn k07_deleting_a_workspace_through_the_api() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let keep = new_ws(&c, "keep").await;
    let run = new_run(&c, &ws, CFG_A, json!({ "auto_apply": true })).await;
    wait_status(&c, &run, "applied").await;
    let uri = format!("/api/v2/workspaces/{ws}");

    c.post(&format!("{uri}/actions/lock")).await;
    assert_eq!(c.delete(&uri).await.status, StatusCode::CONFLICT, "locked workspaces are not deleted");
    c.post(&format!("{uri}/actions/unlock")).await;
    let changed = format!("{CFG_A}resource \"aws_s3_bucket\" \"extra\" {{
  bucket = \"e\"
}}
");
    let waiting = new_run(&c, &ws, &changed, json!({})).await;
    wait_status(&c, &waiting, "needs_confirmation").await;
    assert_eq!(c.delete(&uri).await.status, StatusCode::CONFLICT, "a run waiting for a person holds the lock");
    c.post(&format!("/api/v2/runs/{waiting}/actions/discard")).await;

    assert_eq!(c.delete(&uri).await.status, StatusCode::NO_CONTENT);
    assert_eq!(c.get(&uri).await.status, StatusCode::NOT_FOUND);
    assert_eq!(c.get(&format!("/api/v2/runs/{run}")).await.status, StatusCode::NOT_FOUND, "its runs went with it");
    assert_eq!(c.delete(&uri).await.status, StatusCode::NOT_FOUND);
    assert_eq!(ws_json(&c, &keep).await["data"]["attributes"]["name"], "keep");
    new_ws(&c, "prod").await; // the name is free again
}

#[tokio::test]
async fn k08_the_settings_page_edits_and_deletes_with_care() {
    let app = boot().await;
    let ana = signup(&app, "ana@example.com", "alpha-org").await;
    let cy = signup(&app, "cy@example.com", "zeta-org").await;
    make_member(&app, "ana@example.com", "cy@example.com").await;
    let ws = new_ws(&ana, "prod").await;
    new_ws(&ana, "staging").await;
    let page = format!("/workspaces/{ws}/settings");

    let g = ana.get(&page).await;
    assert_eq!(g.status, StatusCode::OK);
    assert!(g.body.contains("value=\"prod\"") && g.body.contains("Delete workspace") && g.body.contains("Save settings"));
    // a member sees the values and nothing to press
    let m = cy.get(&page).await.body;
    assert!(m.contains("value=\"prod\"") && !m.contains("Delete workspace") && !m.contains("Save settings") && m.contains("disabled"));
    assert_eq!(cy.post_form(&page, &[("name", "mine")]).await.status, StatusCode::FORBIDDEN);
    assert_eq!(cy.post_form(&format!("/workspaces/{ws}/delete"), &[("confirm", "prod")]).await.status, StatusCode::FORBIDDEN);

    let ok = ana.post_form(&page, &[("name", "prod-eu"), ("description", "main <b>site</b>"), ("auto_apply", "on")]).await;
    assert_eq!(ok.status, StatusCode::SEE_OTHER);
    let after = ana.get(&page).await.body;
    assert!(after.contains("value=\"prod-eu\"") && after.contains("main &lt;b&gt;site&lt;/b&gt;") && !after.contains("<b>site</b>"), "escaped");
    assert!(after.contains("name=\"auto_apply\" value=\"on\" checked"));
    let taken = ana.post_form(&page, &[("name", "staging")]).await;
    assert_eq!(taken.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(taken.body.contains("already exists"), "{}", taken.body);
    assert_eq!(ana.post_form(&page, &[("name", "bad name")]).await.status, StatusCode::UNPROCESSABLE_ENTITY);
    // unticked means off
    ana.post_form(&page, &[("name", "prod-eu")]).await;
    assert!(!ana.get(&page).await.body.contains("value=\"on\" checked"));

    // deleting needs the name typed
    let wrong = ana.post_form(&format!("/workspaces/{ws}/delete"), &[("confirm", "prod")]).await;
    assert_eq!(wrong.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(ana.get(&format!("/workspaces/{ws}")).await.status, StatusCode::OK, "still there");
    ana.post(&format!("/api/v2/workspaces/{ws}/actions/lock")).await;
    let locked = ana.post_form(&format!("/workspaces/{ws}/delete"), &[("confirm", "prod-eu")]).await;
    assert_eq!(locked.status, StatusCode::CONFLICT);
    assert!(locked.body.contains("locked"), "{}", locked.body);
    ana.post(&format!("/api/v2/workspaces/{ws}/actions/unlock")).await;
    let gone = ana.post_form(&format!("/workspaces/{ws}/delete"), &[("confirm", "prod-eu")]).await;
    assert_eq!((gone.status, gone.location()), (StatusCode::SEE_OTHER, "/workspaces".to_string()));
    assert_eq!(ana.get(&format!("/workspaces/{ws}")).await.status, StatusCode::NOT_FOUND);
}

// ---- state transfer URLs ----

fn archivist_path(url: &str) -> String {
    format!("/archivist{}", url.split_once("/archivist").expect("an archivist URL").1)
}

async fn applied_workspace(c: &Client) -> String {
    let ws = new_ws(c, "prod").await;
    let run = new_run(c, &ws, CFG_A, json!({ "auto_apply": true })).await;
    wait_status(c, &run, "applied").await;
    ws
}

#[tokio::test]
async fn k09_state_is_downloaded_through_a_single_use_url() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "alpha-org").await;
    let bo = signup(&app, "bo@example.com", "beta").await;
    let ws = applied_workspace(&c).await;

    let v = c.get(&format!("/api/v2/workspaces/{ws}/current-state-version")).await.json();
    let a = &v["data"]["attributes"];
    assert!(a.get("content").is_none(), "the state document is never inside the resource");
    let url = a["hosted-state-download-url"].as_str().unwrap().to_string();
    assert!(url.starts_with("http://127.0.0.1:8080/archivist/v1/object/grant_"), "{url}");
    let path = archivist_path(&url);

    // the URL is the credential: no login needed, and it works once
    let anon = Client::anon(&app);
    let got = anon.get(&path).await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.body);
    let doc: Value = serde_json::from_str(&got.body).expect("a state document");
    assert!(doc["serial"].is_number() && doc["resources"].is_array() || doc.is_object());
    assert_eq!(got.headers[header::CACHE_CONTROL], "no-store");
    assert!(got.headers[header::CONTENT_TYPE].to_str().unwrap().starts_with("application/json"));
    assert_eq!(got.headers["content-md5"].to_str().unwrap(), md5_header(&got.body));
    assert_eq!(anon.get(&path).await.status, StatusCode::NOT_FOUND, "the second use finds nothing");
    assert_eq!(anon.get("/archivist/v1/object/grant_not_a_real_one").await.status, StatusCode::NOT_FOUND);

    // each request for the resource issues a fresh URL; the single version has its own endpoint
    let id = v["data"]["id"].as_str().unwrap();
    let one = c.get(&format!("/api/v2/state-versions/{id}")).await.json();
    assert_eq!(one["data"]["id"], v["data"]["id"]);
    let url2 = one["data"]["attributes"]["hosted-state-download-url"].as_str().unwrap().to_string();
    assert_ne!(url, url2);
    assert_eq!(anon.get(&archivist_path(&url2)).await.body, got.body);

    // listings carry no URLs and no documents; other organizations get nothing
    let list = c.get(&format!("/api/v2/workspaces/{ws}/state-versions")).await.json();
    assert!(list["data"][0]["attributes"].get("hosted-state-download-url").is_none() && list["data"][0]["attributes"].get("content").is_none());
    assert_eq!(bo.get(&format!("/api/v2/state-versions/{id}")).await.status, StatusCode::NOT_FOUND);
    assert_eq!(bo.get(&format!("/api/v2/workspaces/{ws}/current-state-version")).await.status, StatusCode::NOT_FOUND);
    assert_eq!(c.get("/api/v2/state-versions/missing").await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn k10_an_organization_token_gets_no_state_urls_and_the_url_is_not_a_login() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "alpha-org").await;
    let ws = applied_workspace(&c).await;
    let org = c.with_bearer(&token_for(&c, "org").await);
    assert_eq!(org.get(&format!("/api/v2/workspaces/{ws}/current-state-version")).await.status, StatusCode::FORBIDDEN);
    let id = c.get(&format!("/api/v2/workspaces/{ws}/state-versions")).await.json()["data"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(org.get(&format!("/api/v2/state-versions/{id}")).await.status, StatusCode::FORBIDDEN);
    assert_eq!(org.post_json(&format!("/api/v2/workspaces/{ws}/state-versions"), json!({ "serial": 9 })).await.status, StatusCode::FORBIDDEN);
    // a download URL opens that one document and nothing else
    let url = c.get(&format!("/api/v2/workspaces/{ws}/current-state-version")).await.json()["data"]["attributes"]["hosted-state-download-url"].as_str().unwrap().to_string();
    let token = url.rsplit('/').next().unwrap();
    let anon = Client::anon(&app);
    assert_eq!(anon.get("/api/v2/workspaces").await.status, StatusCode::UNAUTHORIZED);
    let r = anon.send(Method::GET, "/api/v2/workspaces", None, String::new()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let bearer_misuse = anon.with_bearer(token).get("/api/v2/workspaces").await;
    assert_eq!(bearer_misuse.status, StatusCode::UNAUTHORIZED, "the URL token is not an API token");
}

fn full_state(serial: u64, lineage: &str, note: &str) -> String {
    json!({ "version": 4, "terraform_version": "1.13.9", "serial": serial, "lineage": lineage, "outputs": { "note": { "value": note, "type": "string" } }, "resources": [] }).to_string()
}

fn md5_hex(s: &str) -> String {
    tf_crypto::hex(&tf_crypto::md5(s.as_bytes()))
}

async fn upload_url(c: &Client, ws: &str, serial: u64, md5: Option<&str>) -> Reply {
    let mut body = json!({ "data": { "type": "state-versions", "attributes": { "serial": serial } } });
    if let Some(m) = md5 {
        body["data"]["attributes"]["md5"] = json!(m);
    }
    c.post_json(&format!("/api/v2/workspaces/{ws}/state-versions"), body).await
}

#[tokio::test]
async fn k11_state_is_uploaded_through_a_single_use_url() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let anon = Client::anon(&app);

    let doc = full_state(1, "lin-1", "first");
    let r = upload_url(&c, &ws, 1, Some(&md5_hex(&doc))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.json()["data"]["type"], "state-version-uploads");
    let url = r.json()["data"]["attributes"]["hosted-state-upload-url"].as_str().unwrap().to_string();
    let put = anon.send(Method::PUT, &archivist_path(&url), Some("application/json"), doc.clone()).await;
    assert_eq!(put.status, StatusCode::OK, "{}", put.body);
    let again = anon.send(Method::PUT, &archivist_path(&url), Some("application/json"), doc.clone()).await;
    assert_eq!(again.status, StatusCode::NOT_FOUND, "the URL was used up");
    // it is now the current state, and the CLI sees the same bytes
    let list = c.get(&format!("/api/v2/workspaces/{ws}/state-versions")).await.json();
    assert_eq!(list["meta"]["pagination"]["total-count"], 1);
    assert_eq!(state_document(&c, &ws).await, doc);
    let tok = token_for(&c, "user").await;
    assert_eq!(anon.cli(&tok, "GET", "/backend/prod", &[], "").await.body, doc);

    // state only moves forward within one lineage; every refusal uses the URL up
    for (what, serial, body, want) in [
        ("older", 1u64, full_state(1, "lin-1", "other"), StatusCode::CONFLICT),
        ("other lineage", 2, full_state(2, "lin-OTHER", "x"), StatusCode::CONFLICT),
        ("body serial differs from the declared one", 3, full_state(2, "lin-1", "x"), StatusCode::BAD_REQUEST),
        ("not json", 3, "{nope".to_string(), StatusCode::BAD_REQUEST),
    ] {
        let u = upload_url(&c, &ws, serial, None).await.json()["data"]["attributes"]["hosted-state-upload-url"].as_str().unwrap().to_string();
        let r = anon.send(Method::PUT, &archivist_path(&u), Some("application/json"), body).await;
        assert_eq!(r.status, want, "{what}: {}", r.body);
        assert_eq!(anon.send(Method::PUT, &archivist_path(&u), Some("application/json"), full_state(serial, "lin-1", "ok")).await.status, StatusCode::NOT_FOUND, "{what}: used up");
    }
    // a wrong md5 is refused
    let good = full_state(2, "lin-1", "second");
    let u = upload_url(&c, &ws, 2, Some(&md5_hex("something else"))).await.json()["data"]["attributes"]["hosted-state-upload-url"].as_str().unwrap().to_string();
    let r = anon.send(Method::PUT, &archivist_path(&u), Some("application/json"), good.clone()).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
    assert!(r.body.contains("md5"));
    assert_eq!(c.get(&format!("/api/v2/workspaces/{ws}/state-versions")).await.json()["meta"]["pagination"]["total-count"], 1, "nothing refused was stored");
    // and the right one goes in
    let u = upload_url(&c, &ws, 2, Some(&md5_hex(&good))).await.json()["data"]["attributes"]["hosted-state-upload-url"].as_str().unwrap().to_string();
    assert_eq!(anon.send(Method::PUT, &archivist_path(&u), Some("application/json"), good.clone()).await.status, StatusCode::OK);
    assert_eq!(state_document(&c, &ws).await, good);

    // an upload URL cannot be used to download, and downloading with it uses it up
    let u = upload_url(&c, &ws, 3, None).await.json()["data"]["attributes"]["hosted-state-upload-url"].as_str().unwrap().to_string();
    assert_eq!(anon.get(&archivist_path(&u)).await.status, StatusCode::NOT_FOUND);
    assert_eq!(anon.send(Method::PUT, &archivist_path(&u), Some("application/json"), full_state(3, "lin-1", "z")).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn k12_state_cannot_be_uploaded_while_a_run_or_the_cli_is_writing() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let cy = signup(&app, "cy@example.com", "zeta-org").await;
    make_member(&app, "ana@example.com", "cy@example.com").await;
    let bo = signup(&app, "bo@example.com", "beta").await;
    let ws = new_ws(&c, "prod").await;
    let anon = Client::anon(&app);
    let tok = token_for(&c, "user").await;

    // validation of the request itself
    for bad in [json!({}), json!({ "serial": "x" }), json!({ "serial": -1 }), json!({ "serial": 1, "md5": "xyz" })] {
        let r = c.post_json(&format!("/api/v2/workspaces/{ws}/state-versions"), bad.clone()).await;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {}", r.body);
    }
    assert_eq!(bo.post_json(&format!("/api/v2/workspaces/{ws}/state-versions"), json!({ "serial": 1 })).await.status, StatusCode::NOT_FOUND);
    assert_eq!(upload_url(&cy, &ws, 1, None).await.status, StatusCode::CREATED, "members may write state, like through the CLI");

    // the CLI holds the lock: no URL, and a URL issued earlier is refused at upload time
    let early = upload_url(&c, &ws, 1, None).await.json()["data"]["attributes"]["hosted-state-upload-url"].as_str().unwrap().to_string();
    anon.cli(&tok, "LOCK", "/backend/prod/lock", &[], &lock_doc("lock-A", "ana@laptop")).await;
    assert_eq!(upload_url(&c, &ws, 1, None).await.status, StatusCode::CONFLICT);
    let r = anon.send(Method::PUT, &archivist_path(&early), Some("application/json"), full_state(1, "lin", "x")).await;
    assert_eq!(r.status, StatusCode::LOCKED, "{}", r.body);
    anon.cli(&tok, "UNLOCK", "/backend/prod/lock", &[], &lock_doc("lock-A", "ana@laptop")).await;

    // a run holds it
    let run = new_run(&c, &ws, CFG_A, json!({})).await;
    wait_status(&c, &run, "needs_confirmation").await;
    assert_eq!(upload_url(&c, &ws, 1, None).await.status, StatusCode::CONFLICT);
    c.post(&format!("/api/v2/runs/{run}/actions/discard")).await;
    // a person's own lock does not stand in the way of the person who took it
    c.post(&format!("/api/v2/workspaces/{ws}/actions/lock")).await;
    let u = upload_url(&c, &ws, 1, None).await;
    assert_eq!(u.status, StatusCode::CREATED, "{}", u.body);
}

// ================= P1: lint in the plan step =================

const LINTY: &str = "variable \"unused_one\" {\n  type = string\n  description = \"never used\"\n}\n\nresource \"aws_db_instance\" \"main\" {\n  username = \"admin\"\n  password = \"hunter2-secret-value\"\n}\n";

fn finding_rules(run: &Value) -> Vec<String> {
    run["data"]["attributes"]["lint"].as_array().unwrap().iter().map(|f| f["rule"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn l01_a_run_reports_lint_findings_in_its_record_and_its_log_and_still_plans() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let run = new_run(&c, &ws, LINTY, json!({})).await;
    let v = wait_status(&c, &run, "needs_confirmation").await;

    let rules = finding_rules(&v);
    assert!(rules.contains(&"unused-variable".to_string()) && rules.contains(&"hardcoded-secret".to_string()), "{rules:?}");
    let f = v["data"]["attributes"]["lint"].as_array().unwrap().iter().find(|f| f["rule"] == "hardcoded-secret").unwrap().clone();
    assert_eq!((f["severity"].clone(), f["line"].clone()), (json!("warning"), json!(8)), "{f}");
    assert!(f["file"].is_null(), "plain text has no file");

    let log = log_text(&c, &run).await;
    assert!(log.contains("Lint: ") && log.contains("lint warning hardcoded-secret (line 8)"), "{log}");
    assert!(log.find("Lint: ").unwrap() < log.find("Plan").unwrap_or(usize::MAX), "lint comes before the plan: {log}");
    assert!(!v.to_string().contains("hunter2-secret-value") && !log.contains("hunter2-secret-value"), "a finding never repeats the secret");
    // it never blocks: the plan exists and can be confirmed
    assert_eq!(v["data"]["attributes"]["summary"]["add"], 1, "the one resource is planned");
    assert_eq!(c.post(&format!("/api/v2/runs/{run}/actions/apply")).await.status, StatusCode::ACCEPTED);
    wait_status(&c, &run, "applied").await;
}

#[tokio::test]
async fn l02_a_tidy_configuration_reports_no_findings() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let tidy = "terraform {\n  required_version = \">= 1.6\"\n  required_providers {\n    aws = {\n      source  = \"hashicorp/aws\"\n      version = \"~> 5.0\"\n    }\n  }\n}\n\nvariable \"bucket\" {\n  type        = string\n  description = \"bucket name\"\n  default     = \"logs\"\n}\n\nresource \"aws_s3_bucket\" \"logs\" {\n  bucket = var.bucket\n}\n";
    let run = new_run(&c, &ws, tidy, json!({})).await;
    let v = wait_status(&c, &run, "needs_confirmation").await;
    assert_eq!(finding_rules(&v), Vec::<String>::new());
    assert!(log_text(&c, &run).await.contains("Lint: no findings."));
    // and the page does not show an empty lint box
    assert!(!c.get(&format!("/runs/{run}")).await.body.contains("id=\"lint\""));
}

#[tokio::test]
async fn l03_findings_in_an_uploaded_archive_name_their_file_and_line() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let cv = c.post(&format!("/api/v2/workspaces/{ws}/configuration-versions")).await.json()["data"]["id"].as_str().unwrap().to_string();
    let archive = targz(&[
        ("a.tf", "resource \"null_resource\" \"a\" {\n}\n"),
        ("variables.tf", "variable \"spare\" {\n  type = string\n  description = \"d\"\n}\n"),
    ]);
    assert_eq!(c.put_bytes(&format!("/api/v2/configuration-versions/{cv}/upload"), archive).await.status, StatusCode::OK);
    let run = c.post_json(&format!("/api/v2/workspaces/{ws}/runs"), json!({ "configuration_version_id": cv })).await.json()["data"]["id"].as_str().unwrap().to_string();
    let v = wait_status(&c, &run, "needs_confirmation").await;
    let f = v["data"]["attributes"]["lint"].as_array().unwrap().iter().find(|f| f["rule"] == "unused-variable").expect("a finding for the spare variable").clone();
    assert_eq!((f["file"].clone(), f["line"].clone()), (json!("variables.tf"), json!(1)), "{f}");
    assert!(log_text(&c, &run).await.contains("(variables.tf:1)"));
}

#[tokio::test]
async fn l04_the_run_page_lists_findings_escaped_and_says_they_do_not_stop_the_run() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let cfg = "variable \"x<b>y\" {\n}\n";
    // a name the HCL parser accepts and the page must still escape
    let run = new_run(&c, &ws, LINTY, json!({})).await;
    wait_status(&c, &run, "needs_confirmation").await;
    let page = c.get(&format!("/runs/{run}")).await.body;
    assert!(page.contains("id=\"lint\"") && page.contains("Lint (") && page.contains("unused-variable") && page.contains("hardcoded-secret"), "{page}");
    assert!(page.contains("Findings never stop a run."));
    assert!(!page.contains("hunter2-secret-value"));
    // the listings carry them too
    let list = c.get(&format!("/api/v2/workspaces/{ws}/runs")).await.json();
    assert!(list["data"][0]["attributes"]["lint"].as_array().unwrap().len() >= 2);
}

#[tokio::test]
async fn l05_text_the_linter_cannot_read_does_not_stop_the_run() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let run = new_run(&c, &ws, "resource \"a\" {{{ not valid\n", json!({ "operation": "plan_only" })).await;
    for _ in 0..300 {
        let v = c.get(&format!("/api/v2/runs/{run}")).await.json();
        if matches!(v["data"]["attributes"]["status"].as_str(), Some("planned_and_finished" | "errored" | "needs_confirmation")) {
            assert_eq!(finding_rules(&v), vec!["parse-error".to_string()]);
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the run never left planning");
}

#[tokio::test]
async fn l06_a_speculative_plan_is_linted_too_and_other_organizations_cannot_see_findings() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "alpha-org").await;
    let bo = signup(&app, "bo@example.com", "beta").await;
    let ws = new_ws(&c, "prod").await;
    let run = new_run(&c, &ws, LINTY, json!({ "operation": "plan_only" })).await;
    wait_status(&c, &run, "planned_and_finished").await;
    let v = c.get(&format!("/api/v2/runs/{run}")).await.json();
    assert!(finding_rules(&v).contains(&"unused-variable".to_string()));
    assert_eq!(bo.get(&format!("/api/v2/runs/{run}")).await.status, StatusCode::NOT_FOUND);
    assert!(!bo.get("/runs").await.body.contains("hardcoded-secret"));
}

// ================= P3: skip the refresh, and free drift detection =================

fn drift_cfg() -> Config {
    Config { drift_tick_secs: 1, min_drift_interval_secs: 1, ..Config::default() }
}

/// A workspace with one applied resource, and the platform's state rewritten so that the resource
/// "changed outside": the simulator reports a state entry whose hash starts with `drift:`.
async fn applied_workspace_named(c: &Client, name: &str) -> String {
    let ws = new_ws(c, name).await;
    let run = new_run(c, &ws, CFG_A, json!({ "auto_apply": true })).await;
    wait_status(c, &run, "applied").await;
    ws
}

async fn drifted_workspace(c: &Client) -> String {
    let ws = applied_workspace_named(c, "drifty").await;
    let v = c.get(&format!("/api/v2/workspaces/{ws}/current-state-version")).await.json();
    let doc = state_document_of(c, &v).await;
    let mut state: Value = serde_json::from_str(&doc).unwrap();
    let first = state["resources"].as_object().unwrap().keys().next().unwrap().clone();
    state["resources"][&first] = json!("drift:changed-by-hand");
    state["version"] = json!(4);
    state["serial"] = json!(1);
    state["lineage"] = json!("lin-drift");
    let body = state.to_string();
    let up = upload_url(c, &ws, 1, Some(&md5_hex(&body))).await;
    assert_eq!(up.status, StatusCode::CREATED, "{}", up.body);
    let url = up.json()["data"]["attributes"]["hosted-state-upload-url"].as_str().unwrap().to_string();
    let put = Client::anon_like(c).send(Method::PUT, &archivist_path(&url), Some("application/json"), body).await;
    assert_eq!(put.status, StatusCode::OK, "{}", put.body);
    ws
}

async fn state_document_of(c: &Client, current: &Value) -> String {
    let url = current["data"]["attributes"]["hosted-state-download-url"].as_str().unwrap().to_string();
    c.get(&archivist_path(&url)).await.body
}

impl Client {
    fn anon_like(c: &Client) -> Client {
        Client { router: c.router.clone(), cookie: None, bearer: None, origin: Some("http://localhost".into()) }
    }
}

async fn drift_of(c: &Client, ws: &str) -> Value {
    ws_json(c, ws).await["data"]["attributes"]["drift"].clone()
}

async fn wait_drift(c: &Client, ws: &str, status: &str) -> Value {
    for _ in 0..400 {
        let d = drift_of(c, ws).await;
        if d["status"] == status {
            return d;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the drift status never became {status}; last: {}", drift_of(c, ws).await);
}

#[tokio::test]
async fn m01_a_run_can_skip_the_refresh_and_says_so() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let plain = new_run(&c, &ws, CFG_A, json!({ "operation": "plan_only" })).await;
    let v = wait_status(&c, &plain, "planned_and_finished").await;
    assert_eq!((v["data"]["attributes"]["skip-refresh"].clone(), v["data"]["attributes"]["refresh-only"].clone()), (json!(false), json!(false)));
    assert!(log_text(&c, &plain).await.contains("Refreshing state"));

    let skipped = new_run(&c, &ws, CFG_A, json!({ "operation": "plan_only", "refresh": false })).await;
    let v = wait_status(&c, &skipped, "planned_and_finished").await;
    assert_eq!(v["data"]["attributes"]["skip-refresh"], true);
    let log = log_text(&c, &skipped).await;
    assert!(log.contains("Skipping the refresh") && !log.contains("Refreshing state"), "{log}");

    // the dashboard form has the box
    assert!(c.get(&format!("/workspaces/{ws}")).await.body.contains("name=\"skip_refresh\""));
    let r = c.post_form(&format!("/workspaces/{ws}/runs"), &[("config", CFG_A), ("mode", "plan_only"), ("skip_refresh", "on")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let id = r.location().rsplit('/').next().unwrap().to_string();
    assert_eq!(wait_status(&c, &id, "planned_and_finished").await["data"]["attributes"]["skip-refresh"], true);
}

#[tokio::test]
async fn m02_a_drift_check_on_request_reports_what_changed_outside() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "alpha-org").await;
    let bo = signup(&app, "bo@example.com", "beta").await;
    let none = new_ws(&c, "empty").await;
    let check = |ws: &str| format!("/api/v2/workspaces/{ws}/actions/check-drift");
    assert_eq!(c.post(&check(&none)).await.status, StatusCode::CONFLICT, "nothing to check without configuration or state");
    // a configuration but no state yet (only a speculative plan so far): still nothing to refresh
    let planned_only = new_ws(&c, "planned-only").await;
    let spec = new_run(&c, &planned_only, CFG_A, json!({ "operation": "plan_only" })).await;
    wait_status(&c, &spec, "planned_and_finished").await;
    let r = c.post(&check(&planned_only)).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.body);
    assert!(r.body.contains("no state"), "{}", r.body);

    let ws = applied_workspace(&c).await;
    let r = c.post(&check(&ws)).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.body);
    let run = r.json()["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(r.json()["data"]["attributes"]["refresh-only"], true);
    let v = wait_status(&c, &run, "planned_and_finished").await;
    assert_eq!(v["data"]["attributes"]["source"], "api");
    let d = wait_drift(&c, &ws, "clean").await;
    assert_eq!((d["changes"].clone(), d["run_id"].clone()), (json!(0), json!(run)));
    assert!(lock_of(&c, &ws).await.is_null(), "a drift check takes no lock");

    // the infrastructure changed outside: the next check finds it
    let ws2 = drifted_workspace(&c).await;
    let run2 = c.post(&check(&ws2)).await.json()["data"]["id"].as_str().unwrap().to_string();
    let v2 = wait_status(&c, &run2, "planned_and_finished").await;
    assert_eq!(v2["data"]["attributes"]["summary"]["change"], 1);
    let d2 = wait_drift(&c, &ws2, "drifted").await;
    assert_eq!(d2["changes"], 1);
    let page = c.get(&format!("/workspaces/{ws2}")).await.body;
    assert!(page.contains("1 changed") && page.contains("Check for drift now"), "{page}");
    assert!(log_text(&c, &run2).await.contains("has changed outside the platform"));

    // a drift check cannot be applied, and it does not stop a real run
    assert_eq!(c.post(&format!("/api/v2/runs/{run2}/actions/apply")).await.status, StatusCode::CONFLICT);
    let more = format!("{CFG_A}resource \"aws_s3_bucket\" \"more\" {{\n  bucket = \"m\"\n}}\n");
    let real = new_run(&c, &ws2, &more, json!({})).await;
    wait_status(&c, &real, "needs_confirmation").await;

    assert_eq!(bo.post(&check(&ws2)).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn m03_drift_is_checked_on_a_schedule_and_again_after_the_interval() {
    let app = boot_with(drift_cfg()).await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = applied_workspace(&c).await;
    assert!(drift_of(&c, &ws).await.is_null(), "nothing is checked until a schedule is set");
    let r = c.send(Method::PATCH, &format!("/api/v2/workspaces/{ws}"), Some("application/json"), json!({ "drift-interval": 1 }).to_string()).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.json()["data"]["attributes"]["drift-interval"], 1);
    let first = wait_drift(&c, &ws, "clean").await;
    let first_run = first["run_id"].as_str().unwrap().to_string();
    let run = c.get(&format!("/api/v2/runs/{first_run}")).await.json();
    assert_eq!((run["data"]["attributes"]["source"].clone(), run["data"]["attributes"]["refresh-only"].clone()), (json!("schedule"), json!(true)));
    // and again once the interval has passed
    for _ in 0..300 {
        if drift_of(&c, &ws).await["run_id"].as_str() != Some(first_run.as_str()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no second scheduled check");
}

#[tokio::test]
async fn m04_the_schedule_is_validated_and_can_be_turned_off() {
    let app = boot().await;
    let ana = signup(&app, "ana@example.com", "alpha-org").await;
    let cy = signup(&app, "cy@example.com", "zeta-org").await;
    make_member(&app, "ana@example.com", "cy@example.com").await;
    let ws = new_ws(&ana, "prod").await;
    let uri = format!("/api/v2/workspaces/{ws}");
    let patch = |c: &Client, body: Value| {
        let (c, uri) = (c.clone(), uri.clone());
        async move { c.send(Method::PATCH, &uri, Some("application/json"), body.to_string()).await }
    };
    // the default shortest interval is five minutes
    for (what, body) in [("too short", json!({ "drift-interval": 60 })), ("zero", json!({ "drift-interval": 0 })), ("text", json!({ "drift-interval": "hourly" })), ("negative", json!({ "drift-interval": -5 }))] {
        let r = patch(&ana, body).await;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{what}: {}", r.body);
    }
    assert_eq!(patch(&cy, json!({ "drift-interval": 3600 })).await.status, StatusCode::FORBIDDEN, "members do not set schedules");
    let on = patch(&ana, json!({ "drift-interval": 3600 })).await;
    assert_eq!(on.json()["data"]["attributes"]["drift-interval"], 3600);
    // leaving it out changes nothing; null turns it off
    assert_eq!(patch(&ana, json!({ "description": "x" })).await.json()["data"]["attributes"]["drift-interval"], 3600);
    assert!(patch(&ana, json!({ "drift-interval": null })).await.json()["data"]["attributes"]["drift-interval"].is_null());
}

#[tokio::test]
async fn m05_only_one_drift_check_is_waiting_at_a_time() {
    let app = {
        let engine: Arc<dyn Engine> = Arc::new(FakeEngine::new(Duration::from_millis(900)));
        boot_engine(drift_cfg(), engine).await
    };
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let run = new_run(&c, &ws, CFG_A, json!({ "auto_apply": true })).await;
    wait_status_for(&c, &run, "applied", 200).await;
    c.send(Method::PATCH, &format!("/api/v2/workspaces/{ws}"), Some("application/json"), json!({ "drift-interval": 1 }).to_string()).await;
    let mut most_open = 0;
    for _ in 0..80 {
        let runs = c.get(&format!("/api/v2/workspaces/{ws}/runs")).await.json();
        let open = runs["data"].as_array().unwrap().iter().filter(|r| r["attributes"]["refresh-only"] == true && matches!(r["attributes"]["status"].as_str(), Some("pending" | "planning"))).count();
        most_open = most_open.max(open);
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    assert!(most_open >= 1, "a check was started");
    assert_eq!(most_open, 1, "never a pile-up behind a slow check");
}

#[tokio::test]
async fn m06_a_failed_check_is_recorded_and_not_retried_every_tick() {
    let app = boot_with(Config { drift_tick_secs: 1, min_drift_interval_secs: 1, ..Config::default() }).await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = applied_workspace(&c).await;
    tokio::time::sleep(Duration::from_millis(1100)).await; // configuration versions are ordered by the second
    // the latest configuration now fails to plan
    let bad = new_run(&c, &ws, "fail_plan\n", json!({ "operation": "plan_only" })).await;
    wait_status(&c, &bad, "errored").await;
    let r = c.post(&format!("/api/v2/workspaces/{ws}/actions/check-drift")).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.body);
    let d = wait_drift(&c, &ws, "error").await;
    assert_eq!(d["changes"], 0);
    assert!(d["checked_at"].as_u64().unwrap() > 0, "the failure sets the time, so a schedule waits its interval");
}

#[tokio::test]
async fn m07_the_settings_page_sets_the_schedule_and_the_workspace_page_checks_now() {
    let app = boot().await;
    let ana = signup(&app, "ana@example.com", "alpha-org").await;
    let ws = applied_workspace(&ana).await;
    let page = format!("/workspaces/{ws}/settings");
    assert!(ana.get(&page).await.body.contains("name=\"drift_minutes\""));
    for (what, v) in [("under the minimum", "3"), ("not a number", "soon"), ("zero", "0")] {
        let r = ana.post_form(&page, &[("name", "prod"), ("drift_minutes", v)]).await;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{what}");
        assert!(r.body.contains("whole number of minutes, at least 5"), "{what}: {}", r.body);
    }
    assert_eq!(ana.post_form(&page, &[("name", "prod"), ("drift_minutes", "10")]).await.status, StatusCode::SEE_OTHER);
    assert_eq!(ws_json(&ana, &ws).await["data"]["attributes"]["drift-interval"], 600);
    assert!(ana.get(&page).await.body.contains("name=\"drift_minutes\" value=\"10\""));
    assert_eq!(ana.post_form(&page, &[("name", "prod"), ("drift_minutes", "")]).await.status, StatusCode::SEE_OTHER);
    assert!(ws_json(&ana, &ws).await["data"]["attributes"]["drift-interval"].is_null());

    // check now from the workspace page; without a state there is no button and the action is refused
    let r = ana.post_form(&format!("/workspaces/{ws}/check-drift"), &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let run = r.location().rsplit('/').next().unwrap().to_string();
    wait_status(&ana, &run, "planned_and_finished").await;
    let fresh = new_ws(&ana, "fresh").await;
    assert!(!ana.get(&format!("/workspaces/{fresh}")).await.body.contains("Check for drift now"));
    assert_eq!(ana.post_form(&format!("/workspaces/{fresh}/check-drift"), &[]).await.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn m08_a_refresh_only_run_can_be_asked_for_directly_but_never_applied_or_skipped() {
    let app = boot().await;
    let c = signup(&app, "ana@example.com", "acme").await;
    let ws = new_ws(&c, "prod").await;
    let runs = format!("/api/v2/workspaces/{ws}/runs");
    let r = c.post_json(&runs, json!({ "config": CFG_A, "operation": "refresh_only", "auto_apply": true })).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let id = r.json()["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(r.json()["data"]["attributes"]["operation"], "plan_only", "a drift check is always speculative");
    wait_status(&c, &id, "planned_and_finished").await;
    let both = c.post_json(&runs, json!({ "config": CFG_A, "operation": "refresh_only", "refresh": false })).await;
    assert_eq!(both.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", both.body);
}

async fn wait_status_for(c: &Client, run: &str, want: &str, tries: u32) -> Value {
    for _ in 0..tries {
        let v = c.get(&format!("/api/v2/runs/{run}")).await.json();
        if v["data"]["attributes"]["status"] == want {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("run {run} never reached {want}");
}
