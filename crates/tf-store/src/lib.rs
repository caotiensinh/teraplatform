//! The data layer. `Store` is the contract; `MemoryStore` (tests, quick local
//! runs) and `PgStore` (real use) both implement it and both pass the same
//! contract tests. Both seal state, saved plans and sensitive variables with
//! `Sealer` before anything is stored.

pub mod contract;
mod memory;
mod pg;
mod sealer;

pub use memory::MemoryStore;
pub use pg::PgStore;
pub use sealer::Sealer;

use async_trait::async_trait;
use std::fmt;
use tf_core::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    NotFound(String),
    Conflict(String),
    Invalid(String),
    /// Known principal, not allowed.
    Forbidden(String),
    /// Database or crypto failure. Details are logged, never shown to users.
    Internal(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::NotFound(m)
            | StoreError::Conflict(m)
            | StoreError::Invalid(m)
            | StoreError::Forbidden(m)
            | StoreError::Internal(m) => f.write_str(m),
        }
    }
}
impl std::error::Error for StoreError {}

impl From<TransitionError> for StoreError {
    fn from(e: TransitionError) -> Self {
        StoreError::Conflict(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Work handed to the runner.
#[derive(Debug, Clone)]
pub enum Claim {
    Plan(Run),
    Apply(Run),
}

#[async_trait]
pub trait Store: Send + Sync {
    // ---- accounts ----
    async fn create_user(&self, email: &str, password_hash: &str, display_name: &str) -> Result<User>;
    /// Email match ignores case. Returns the stored password hash with the user.
    async fn find_user_by_email(&self, email: &str) -> Result<Option<(User, String)>>;
    async fn get_user(&self, id: &str) -> Result<User>;
    async fn set_password(&self, user: &str, password_hash: &str) -> Result<()>;
    async fn mark_email_verified(&self, user: &str) -> Result<()>;
    /// Deletes the user, their sessions and tokens, their memberships, and every
    /// organization in which they were the only member (with all its data).
    async fn delete_user(&self, user: &str) -> Result<()>;
    /// Creates the organization with `owner` as its owner.
    async fn create_org(&self, name: &str, owner: &str) -> Result<Org>;
    /// Role is "owner", "admin" or "member". Conflict if already a member.
    async fn add_member(&self, org: &str, user: &str, role: &str) -> Result<()>;
    async fn orgs_for_user(&self, user: &str) -> Result<Vec<(Org, String)>>;
    async fn member_role(&self, org: &str, user: &str) -> Result<Option<String>>;

    async fn create_session(&self, user: &str, token_hash: &[u8], expires_at: u64) -> Result<()>;
    async fn session_user(&self, token_hash: &[u8], now: u64) -> Result<Option<Id>>;
    async fn delete_session(&self, token_hash: &[u8]) -> Result<()>;
    /// "Sign out everywhere".
    async fn delete_user_sessions(&self, user: &str) -> Result<()>;

    async fn create_api_token(
        &self,
        kind: &str,
        user: Option<&str>,
        org: &str,
        description: &str,
        token_hash: &[u8],
        expires_at: Option<u64>,
    ) -> Result<ApiToken>;
    async fn find_api_token(&self, token_hash: &[u8], now: u64) -> Result<Option<ApiToken>>;
    async fn list_api_tokens(&self, org: &str) -> Result<Vec<ApiToken>>;
    async fn delete_api_token(&self, org: &str, id: &str) -> Result<()>;

    /// One-time tokens for email verification ("verify") and password reset ("reset").
    async fn create_email_token(&self, user: &str, purpose: &str, token_hash: &[u8], expires_at: u64) -> Result<()>;
    /// Returns the user and burns the token, or `None` if unknown, used, expired or for another purpose.
    async fn consume_email_token(&self, token_hash: &[u8], purpose: &str, now: u64) -> Result<Option<Id>>;

    // ---- workspaces ----
    async fn create_workspace(&self, org: &str, name: &str) -> Result<Workspace>;
    async fn list_workspaces(&self, org: &str) -> Result<Vec<Workspace>>;
    async fn get_workspace(&self, id: &str) -> Result<Workspace>;
    /// Manual lock. Conflict if the workspace is already locked.
    /// `info` is the CLI's own lock description, kept so a conflicting CLI can be shown who holds it.
    /// `lease_secs`: the lock lapses by itself after that long unless renewed with `heartbeat_lock`.
    /// A lock that has lapsed counts as no lock everywhere: it can be taken over and a queued run may start.
    async fn lock_workspace(&self, id: &str, owner: &str, reason: &str, info: Option<&str>, lease_secs: Option<u64>) -> Result<Workspace>;
    /// Renews a lock that has a lease: it now lapses `lease_secs` from now. Conflict when the workspace is
    /// not locked, is locked by someone else, or the lock has no lease (a run's lock never does).
    async fn heartbeat_lock(&self, id: &str, owner: &str, lease_secs: u64) -> Result<Workspace>;
    /// Changes a workspace's name, description or default auto-apply. A taken or invalid name is refused.
    async fn update_workspace(&self, id: &str, patch: WorkspacePatch) -> Result<Workspace>;
    /// Deletes a workspace with its runs, configuration, variables and state versions. Refused (Conflict)
    /// while it is locked or a run is working: unlock or finish first.
    async fn delete_workspace(&self, id: &str) -> Result<()>;
    /// Release a lock only if `owner` holds it (the CLI's `unlock`). Conflict otherwise.
    async fn unlock_workspace_owned(&self, id: &str, owner: &str) -> Result<Workspace>;
    /// Unlock. A lock held by a run needs `force`, and force ends that run (discards
    /// it while it waits for confirmation, cancels it before it applies). A run that
    /// is applying cannot be unlocked from under itself.
    async fn unlock_workspace(&self, id: &str, force: bool) -> Result<Workspace>;

    async fn create_config_version(&self, ws: &str, text: &str, source: &str) -> Result<ConfigVersion>;
    /// A configuration version still waiting for its archive (the two-step upload).
    async fn create_pending_config(&self, ws: &str, source: &str) -> Result<ConfigVersion>;
    /// Fills a pending version. Conflict when it already has its content.
    async fn upload_config(&self, id: &str, text: &str) -> Result<ConfigVersion>;
    async fn get_config_version(&self, id: &str) -> Result<ConfigVersion>;
    async fn latest_config(&self, ws: &str) -> Result<Option<ConfigVersion>>;

    // ---- variables ----
    /// Insert or replace by (workspace, category, key). A sensitive value is write-only.
    async fn set_variable(&self, ws: &str, v: NewVariable) -> Result<Variable>;
    /// Sensitive values come back as `None`.
    async fn list_variables(&self, ws: &str) -> Result<Vec<Variable>>;
    async fn delete_variable(&self, ws: &str, id: &str) -> Result<()>;
    /// Internal use by the runner: every value, decrypted.
    async fn variables_for_run(&self, ws: &str) -> Result<Vec<Variable>>;

    // ---- runs ----
    async fn create_run(
        &self,
        ws: &str,
        config_version: &str,
        operation: Operation,
        message: &str,
        auto_apply: bool,
        source: &str,
    ) -> Result<Run>;
    async fn get_run(&self, id: &str) -> Result<Run>;
    /// Newest first. `None` lists every run.
    async fn list_runs(&self, ws: Option<&str>) -> Result<Vec<Run>>;
    async fn list_runs_in_org(&self, org: &str) -> Result<Vec<Run>>;
    /// Moves a run through the state machine and releases its lock when it stops holding it.
    async fn apply_event(&self, run: &str, event: Event) -> Result<Run>;
    async fn set_plan(&self, run: &str, summary: PlanSummary, changes: Vec<ResourceChange>, blob: Vec<u8>) -> Result<Run>;
    async fn get_plan_blob(&self, run: &str) -> Result<Vec<u8>>;
    /// Atomically picks the next piece of work. Enforces one executing run per workspace.
    async fn claim_next(&self) -> Result<Option<Claim>>;
    /// Call once at startup, before the runner: a run left `planning` or `applying`
    /// by a stopped process can never finish, so it is errored and its lock released.
    /// Assumes a single server instance. Returns how many runs were recovered.
    async fn recover_interrupted(&self) -> Result<usize>;

    async fn append_log(&self, run: &str, line: &str) -> Result<()>;
    async fn get_log(&self, run: &str) -> Result<Vec<String>>;
    /// Log lines from position `from` on (0-based), for streaming and resuming.
    async fn get_log_from(&self, run: &str, from: usize) -> Result<Vec<String>>;

    // ---- state ----
    async fn current_state(&self, ws: &str) -> Result<Option<StateVersion>>;
    async fn list_states(&self, ws: &str) -> Result<Vec<StateVersion>>;
    async fn get_state_version(&self, id: &str) -> Result<StateVersion>;
    /// Stores a new state version with the next serial and makes it current. An empty
    /// `run` means the state was pushed by the CLI rather than produced by a run.
    async fn finalize_state(&self, ws: &str, run: &str, content: &str, resources: usize) -> Result<StateVersion>;
}

/// Declares one `#[tokio::test]` per contract case, against the store `$make().await`
/// returns (`None` skips, with a message).
#[macro_export]
macro_rules! contract_tests {
    ($make:path; $($name:ident),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $name() {
                let Some(s) = $make().await else {
                    eprintln!("SKIPPED {}: backend not configured", stringify!($name));
                    return;
                };
                $crate::contract::$name(s).await;
            }
        )*
    };
}

#[macro_export]
macro_rules! all_contract_tests {
    ($make:path) => {
        $crate::contract_tests!($make;
            workspace_names_are_validated_and_unique_per_org,
            workspaces_are_listed_per_org,
            one_executing_run_per_workspace_and_fifo,
            manual_lock_blocks_runs_until_unlocked,
            speculative_plans_do_not_lock,
            applies_are_claimed_before_new_plans,
            illegal_transitions_are_conflicts,
            state_serials_increase_and_current_is_latest,
            concurrent_claims_never_double_start_a_workspace,
            run_must_use_a_config_from_its_own_workspace,
            a_pending_config_runs_only_after_its_upload,
            a_lock_with_a_lease_lapses_and_frees_the_workspace,
            a_lapsed_lock_can_be_taken_over_and_a_live_one_cannot,
            a_heartbeat_renews_only_its_own_leased_lock,
            workspace_settings_change_and_are_validated,
            a_workspace_is_deleted_with_everything_in_it,
            a_single_state_version_can_be_fetched,
            logs_keep_order,
            plans_and_state_round_trip_through_sealing,
            users_orgs_and_roles,
            sessions_expire_and_sign_out_everywhere,
            api_tokens,
            email_tokens_are_single_use,
            variables_upsert_redact_and_delete,
            deleting_a_user_deletes_what_only_they_owned,
            force_unlock_ends_the_run_that_held_the_lock,
            interrupted_runs_are_recovered,
            force_cancel_ends_a_working_run_and_frees_the_workspace,
            cli_locks_carry_their_info_and_only_their_owner_releases_them,
            state_pushed_without_a_run_has_no_run,
        );
    };
}
