//! HTTP surface: server-rendered pages (web), a JSON API (api), accounts (auth)
//! and the runner.

pub mod api;
pub mod archive;
pub mod auth;
pub mod authz;
pub mod backend;
pub mod grants;
pub mod jsonapi;
pub mod runner;
pub mod stream;
pub mod web;

use axum::middleware::{from_fn, from_fn_with_state};
use axum::extract::DefaultBodyLimit;
use axum::routing::{any, delete, get, post, put};
use axum::Router;
use std::sync::Arc;
use std::time::Duration;
use tf_core::{actions, Event, Operation, Run};
use tf_store::{Result as StoreResult, Store, StoreError};

/// Longest run message, in characters.
pub const MAX_MESSAGE: usize = 200;
/// Largest configuration accepted for a run, in bytes.
pub const MAX_CONFIG_BYTES: usize = 1_000_000;

/// Seven days.
pub const DEFAULT_TTL: u64 = 7 * 86_400;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn Store>,
    pub engine_name: &'static str,
    pub mailer: Arc<dyn auth::Mailer>,
    pub limiter: Arc<auth::RateLimiter>,
    pub cfg: Arc<auth::Config>,
    /// Lets a cancel reach the engine step. Give the same registry to `runner::spawn`.
    pub cancels: runner::Cancels,
    /// Single-use state download and upload URLs.
    pub grants: Arc<grants::Grants>,
}

impl AppState {
    pub fn new(store: Arc<dyn Store>, engine_name: &'static str, mailer: Arc<dyn auth::Mailer>, cfg: auth::Config) -> Self {
        Self { store, engine_name, mailer, limiter: Arc::new(auth::RateLimiter::default()), cfg: Arc::new(cfg), cancels: runner::Cancels::new(), grants: Arc::new(grants::Grants::default()) }
    }
}

/// Saves the configuration and queues a run on it.
pub async fn queue_run(
    store: &Arc<dyn Store>,
    workspace_id: &str,
    config: &str,
    message: &str,
    operation: Operation,
    auto_apply: bool,
    source: &str,
) -> StoreResult<Run> {
    if message.chars().count() > MAX_MESSAGE {
        return Err(StoreError::Invalid(format!("the message is limited to {MAX_MESSAGE} characters")));
    }
    if config.len() > MAX_CONFIG_BYTES {
        return Err(StoreError::Invalid("the configuration is limited to 1 MB".into()));
    }
    let cv = store.create_config_version(workspace_id, config, source).await?;
    store.create_run(workspace_id, &cv.id, operation, message, auto_apply, source).await
}

/// Queues a run on a configuration version that was uploaded earlier. The store checks that the
/// version belongs to this workspace and has its content.
pub async fn queue_run_on(
    store: &Arc<dyn Store>,
    workspace_id: &str,
    config_version_id: &str,
    message: &str,
    operation: Operation,
    auto_apply: bool,
    source: &str,
) -> StoreResult<Run> {
    if message.chars().count() > MAX_MESSAGE {
        return Err(StoreError::Invalid(format!("the message is limited to {MAX_MESSAGE} characters")));
    }
    store.create_run(workspace_id, config_version_id, operation, message, auto_apply, source).await
}

/// How long a force-cancel waits for the runner to finish with the stopped step before the
/// lock is released. The engine is first asked to stop and given its cancel grace (15 s by
/// default) to write down its state; then there is the kill and the recording of that state.
pub const FORCE_CANCEL_WAIT: Duration = Duration::from_secs(45);

/// Applies a run action. Canceling also reaches the engine step itself, so the process stops
/// and the next run is not working against a ghost.
///
/// A force-cancel stops the step first and only then ends the run: the runner records what
/// the engine had already finished, and the lock is released after that, so a queued run never
/// plans against a state that is missing resources which exist.
pub async fn run_event(st: &AppState, run: &Run, ev: Event) -> StoreResult<Run> {
    if ev == Event::ForceCancel && actions(run.status).is_force_cancelable {
        st.cancels.cancel_and_wait(&run.id, FORCE_CANCEL_WAIT).await;
    }
    let res = st.store.apply_event(&run.id, ev).await;
    // After the status change, so a step that starts in between sees a canceled run and stands down.
    if matches!(ev, Event::Cancel | Event::ForceCancel) && res.is_ok() {
        st.cancels.cancel(&run.id);
    }
    res
}

/// The run that holds a workspace's lock, if the lock is a run's.
pub fn lock_holder_run(lock_owner: &str) -> Option<&str> {
    lock_owner.strip_prefix("run:")
}

pub fn app(state: AppState) -> Router {
    let public = Router::new()
        .route("/", get(web::home))
        .route("/assets/app.css", get(web::css))
        .route("/healthz", get(web::healthz))
        .route("/login", get(auth::login_page).post(auth::login))
        .route("/signup", get(auth::signup_page).post(auth::signup))
        .route("/verify", get(auth::verify))
        .route("/forgot", get(auth::forgot_page).post(auth::forgot))
        .route("/reset", get(auth::reset_page).post(auth::reset))
        .route("/logout", post(auth::logout))
        // single-use state transfer: the unguessable URL is the credential
        .route("/archivist/v1/object/{token}", get(grants::download).put(grants::upload).layer(DefaultBodyLimit::max(grants::MAX_STATE_BYTES)));

    let protected = Router::new()
        // pages
        .route("/workspaces", get(web::workspaces).post(web::create_workspace))
        .route("/workspaces/{id}", get(web::workspace))
        .route("/workspaces/{id}/runs", post(web::create_run))
        .route("/workspaces/{id}/lock", post(web::lock))
        .route("/workspaces/{id}/unlock", post(web::unlock))
        .route("/workspaces/{id}/states", get(web::states))
        .route("/workspaces/{id}/settings", get(web::settings).post(web::save_settings))
        .route("/workspaces/{id}/delete", post(web::delete_workspace))
        .route("/workspaces/{id}/variables", get(web::variables).post(web::set_variable))
        .route("/workspaces/{id}/variables/{var_id}/delete", post(web::delete_variable))
        .route("/runs", get(web::runs))
        .route("/runs/{id}", get(web::run))
        .route("/runs/{id}/events", get(stream::run_events))
        .route("/runs/{id}/{action}", post(web::run_action))
        .route("/account", get(auth::account_page))
        .route("/account/delete", post(auth::delete_account))
        .route("/logout-all", post(auth::logout_all))
        // OpenTofu http backend
        .route("/backend/{name}", any(backend::state))
        .route("/backend/{name}/lock", any(backend::lock_endpoint))
        .route("/tokens", get(auth::tokens_page).post(auth::create_token_form))
        .route("/tokens/{id}/delete", post(auth::delete_token_form))
        // JSON API
        .route("/api/v2/workspaces", get(api::list_workspaces).post(api::create_workspace))
        .route("/api/v2/workspaces/{id}", get(api::get_workspace).patch(api::update_workspace).delete(api::delete_workspace))
        .route("/api/v2/workspaces/{id}/runs", get(api::list_runs_in_workspace).post(api::create_run))
        .route("/api/v2/workspaces/{id}/configuration-versions", post(api::create_config_version))
        .route("/api/v2/workspaces/{id}/state-versions", get(api::list_states).post(api::create_state_upload))
        .route("/api/v2/state-versions/{id}", get(api::get_state_version))
        .route("/api/v2/configuration-versions/{id}", get(api::get_config_version))
        .route("/api/v2/configuration-versions/{id}/upload", put(api::upload_config_version).layer(DefaultBodyLimit::max(api::MAX_UPLOAD_BYTES)))
        .route("/api/v2/runs", get(api::list_runs))
        .route("/api/v2/workspaces/{id}/actions/{action}", post(api::workspace_action))
        .route("/api/v2/workspaces/{id}/current-state-version", get(api::current_state))
        .route("/api/v2/workspaces/{id}/vars", get(api::list_vars).post(api::set_var))
        .route("/api/v2/workspaces/{id}/vars/{var_id}", delete(api::delete_var))
        .route("/api/v2/runs/{id}", get(api::get_run))
        .route("/api/v2/runs/{id}/logs", get(api::run_logs))
        .route("/api/v2/runs/{id}/events", get(stream::run_events))
        .route("/api/v2/runs/{id}/actions/{action}", post(api::run_action))
        .route("/api/v2/tokens", get(auth::api_list_tokens).post(auth::api_create_token))
        .route("/api/v2/tokens/{id}", delete(auth::api_delete_token))
        .route_layer(from_fn(auth::require_auth))
        .route_layer(from_fn_with_state(state.clone(), api::rate_limit));

    public.merge(protected).layer(from_fn_with_state(state.clone(), auth::authenticate)).with_state(state)
}
