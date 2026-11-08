//! PostgreSQL implementation of `Store`. Plain runtime-checked SQL (no compile-time
//! database needed). The queue rule is enforced twice: by `claim_next` (SKIP LOCKED
//! plus a lock-owner update) and by the partial unique index in the migration.

use crate::{Claim, Result, Sealer, Store, StoreError};
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Serialize;
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool, Row};
use tf_core::*;
use tf_crypto::Crypto;

pub struct PgStore {
    pool: PgPool,
    sealer: Sealer,
}

fn db(e: sqlx::Error) -> StoreError {
    if let sqlx::Error::Database(d) = &e {
        // 23505 unique violation, 23503 foreign key violation, 23514 check violation
        match d.code().as_deref() {
            Some("23505") => return StoreError::Conflict("that already exists".into()),
            Some("23503") => return StoreError::NotFound("a referenced record does not exist".into()),
            Some("23514") => return StoreError::Invalid("a value was rejected by a database check".into()),
            _ => {}
        }
    }
    eprintln!("database error: {e}");
    StoreError::Internal("database error".into())
}

fn nf(what: &str) -> StoreError {
    StoreError::NotFound(format!("{what} not found"))
}

fn enum_str<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

fn parse_enum<T: DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|_| StoreError::Internal(format!("unknown stored value {s}")))
}

fn ts(secs: u64) -> i64 {
    secs as i64
}

const EPOCH: &str = "extract(epoch from";

fn user_cols() -> String {
    format!("id, email, display_name, email_verified, {EPOCH} created_at)::bigint as created_at")
}

fn run_cols() -> String {
    format!(
        "id, workspace_id, config_version_id, status, operation, source, message, auto_apply, summary, changes, \
         input_state_serial, status_timestamps, lint, skip_refresh, refresh_only, vcs, {EPOCH} created_at)::bigint as created_at, \
         {EPOCH} updated_at)::bigint as updated_at"
    )
}

fn ws_cols() -> String {
    format!(
        "id, org_id, name, lock_owner, lock_reason, lock_info, {EPOCH} lock_since)::bigint as lock_since, \
         {EPOCH} lock_expires_at)::bigint as lock_expires_at, current_run, \
         current_state_version, resource_count, description, auto_apply, drift_interval, drift_run, drift_status, drift_changes, \
         {EPOCH} drift_checked_at)::bigint as drift_checked_at, {EPOCH} created_at)::bigint as created_at"
    )
}

fn user_from(r: &PgRow) -> User {
    User {
        id: r.get("id"),
        email: r.get("email"),
        display_name: r.get("display_name"),
        email_verified: r.get("email_verified"),
        created_at: r.get::<i64, _>("created_at") as u64,
    }
}

fn ws_from(r: &PgRow) -> Workspace {
    let owner: Option<String> = r.get("lock_owner");
    Workspace {
        id: r.get("id"),
        org: r.get("org_id"),
        name: r.get("name"),
        lock: owner.map(|o| Lock {
            owner: o,
            reason: r.get::<Option<String>, _>("lock_reason").unwrap_or_default(),
            since: r.get::<Option<i64>, _>("lock_since").unwrap_or(0) as u64,
            info: r.get("lock_info"),
            expires_at: r.get::<Option<i64>, _>("lock_expires_at").map(|e| e as u64),
        }),
        current_run: r.get("current_run"),
        current_state_version: r.get("current_state_version"),
        resource_count: r.get::<i32, _>("resource_count") as usize,
        description: r.get("description"),
        auto_apply: r.get("auto_apply"),
        drift_interval: r.get::<Option<i64>, _>("drift_interval").map(|d| d as u64),
        drift: r.get::<Option<String>, _>("drift_status").map(|status| DriftStatus {
            checked_at: r.get::<Option<i64>, _>("drift_checked_at").unwrap_or(0) as u64,
            run_id: r.get("drift_run"),
            status,
            changes: r.get::<Option<i32>, _>("drift_changes").unwrap_or(0) as u32,
        }),
        created_at: r.get::<i64, _>("created_at") as u64,
    }
}

fn run_from(r: &PgRow) -> Result<Run> {
    let summary: Option<Json<PlanSummary>> = r.get("summary");
    let changes: Json<Vec<ResourceChange>> = r.get("changes");
    let stamps: Json<std::collections::BTreeMap<String, u64>> = r.get("status_timestamps");
    let lint: Json<Vec<LintFinding>> = r.get("lint");
    let vcs: Option<Json<VcsOrigin>> = r.get("vcs");
    Ok(Run {
        id: r.get("id"),
        workspace_id: r.get("workspace_id"),
        config_version_id: r.get("config_version_id"),
        status: parse_enum(&r.get::<String, _>("status"))?,
        operation: parse_enum(&r.get::<String, _>("operation"))?,
        source: r.get("source"),
        message: r.get("message"),
        auto_apply: r.get("auto_apply"),
        summary: summary.map(|j| j.0),
        changes: changes.0,
        input_state_serial: r.get::<Option<i64>, _>("input_state_serial").map(|s| s as u64),
        status_timestamps: stamps.0,
        lint: lint.0,
        skip_refresh: r.get("skip_refresh"),
        refresh_only: r.get("refresh_only"),
        vcs: vcs.map(|j| j.0),
        created_at: r.get::<i64, _>("created_at") as u64,
        updated_at: r.get::<i64, _>("updated_at") as u64,
    })
}

fn token_from(r: &PgRow) -> ApiToken {
    ApiToken {
        id: r.get("id"),
        kind: r.get("kind"),
        user_id: r.get("user_id"),
        org_id: r.get("org_id"),
        description: r.get("description"),
        created_at: r.get::<i64, _>("created_at") as u64,
        expires_at: r.get::<Option<i64>, _>("expires_at").map(|e| e as u64),
    }
}

fn var_from(r: &PgRow) -> Variable {
    Variable {
        id: r.get("id"),
        workspace_id: r.get("workspace_id"),
        key: r.get("key"),
        category: r.get("category"),
        hcl: r.get("hcl"),
        sensitive: r.get("sensitive"),
        value: Some(r.get("value")),
        description: r.get("description"),
    }
}

/// Moves a run through the state machine inside the caller's transaction and
/// releases the workspace lock if the run no longer holds it.
async fn transition(c: &mut PgConnection, run_id: &str, ev: Event) -> Result<Run> {
    let row = sqlx::query(&format!("select {} from runs where id = $1 for update", run_cols()))
        .bind(run_id)
        .fetch_optional(&mut *c)
        .await
        .map_err(db)?
        .ok_or_else(|| nf("run"))?;
    let mut run = run_from(&row)?;
    let to = next(run.status, ev)?;
    let t = now();
    run.status_timestamps.insert(to.as_str().to_string(), t);
    sqlx::query("update runs set status = $2, status_timestamps = $3, updated_at = now() where id = $1")
        .bind(run_id)
        .bind(to.as_str())
        .bind(Json(&run.status_timestamps))
        .execute(&mut *c)
        .await
        .map_err(db)?;
    if !to.holds_lock() {
        sqlx::query(
            "update workspaces set lock_owner = null, lock_reason = null, lock_since = null, lock_info = null, lock_expires_at = null \
             where id = $1 and lock_owner = $2",
        )
        .bind(&run.workspace_id)
        .bind(format!("run:{run_id}"))
        .execute(&mut *c)
        .await
        .map_err(db)?;
    }
    run.status = to;
    run.updated_at = t;
    Ok(run)
}

impl PgStore {
    pub async fn connect(url: &str, crypto: Crypto) -> Result<Self> {
        Self::connect_in_schema(url, None, crypto).await
    }

    /// `schema` isolates a test run inside a shared database.
    pub async fn connect_in_schema(url: &str, schema: Option<&str>, crypto: Crypto) -> Result<Self> {
        if let Some(s) = schema {
            let admin = PgPoolOptions::new().max_connections(1).connect(url).await.map_err(db)?;
            sqlx::query(&format!("create schema if not exists \"{s}\"")).execute(&admin).await.map_err(db)?;
            admin.close().await;
        }
        let schema_owned = schema.map(String::from);
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .after_connect(move |conn, _| {
                let s = schema_owned.clone();
                Box::pin(async move {
                    if let Some(s) = s {
                        sqlx::query(&format!("set search_path to \"{s}\"")).execute(conn).await?;
                    }
                    Ok(())
                })
            })
            .connect(url)
            .await
            .map_err(db)?;
        sqlx::migrate!("../../migrations").run(&pool).await.map_err(|e| StoreError::Internal(format!("migration failed: {e}")))?;
        Ok(Self { pool, sealer: Sealer::new(crypto) })
    }

    pub async fn drop_schema(url: &str, schema: &str) {
        if let Ok(admin) = PgPoolOptions::new().max_connections(1).connect(url).await {
            let _ = sqlx::query(&format!("drop schema if exists \"{schema}\" cascade")).execute(&admin).await;
        }
    }

    /// Test seam: the stored form of a column, bypassing the sealer.
    pub async fn raw_scalar(&self, sql: &str, bind: &str) -> Option<String> {
        sqlx::query_scalar::<_, String>(sql).bind(bind).fetch_optional(&self.pool).await.ok().flatten()
    }

    fn open_state(&self, r: &PgRow) -> Result<StateVersion> {
        let ws: String = r.get("workspace_id");
        let sealed: String = r.get("content");
        Ok(StateVersion {
            id: r.get("id"),
            run_id: r.get("run_id"),
            serial: r.get::<i64, _>("serial") as u64,
            status: r.get("status"),
            content: self.sealer.open_state(&ws, &sealed)?,
            resource_count: r.get::<i32, _>("resource_count") as usize,
            created_at: r.get::<i64, _>("created_at") as u64,
            workspace_id: ws,
        })
    }
}

const STATE_COLS: &str = "id, workspace_id, run_id, serial, status, content, resource_count, extract(epoch from created_at)::bigint as created_at";

impl PgStore {
    /// A lock whose lease has run out is no lock: cleared before anything reads or changes locks.
    /// One statement; a lock with no lease is never touched.
    async fn expire_locks(&self) -> Result<()> {
        sqlx::query(
            "update workspaces set lock_owner = null, lock_reason = null, lock_since = null, lock_info = null, lock_expires_at = null \
             where lock_expires_at is not null and lock_expires_at <= now()",
        )
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }
}

#[async_trait]
impl Store for PgStore {
    // ---- accounts ----
    async fn create_user(&self, email: &str, password_hash: &str, display_name: &str) -> Result<User> {
        let r = sqlx::query(&format!(
            "insert into users (email, password_hash, display_name) values ($1,$2,$3) returning {}",
            user_cols()
        ))
        .bind(email)
        .bind(password_hash)
        .bind(display_name)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| match db(e) {
            StoreError::Conflict(_) => StoreError::Conflict("an account with that email already exists".into()),
            other => other,
        })?;
        Ok(user_from(&r))
    }

    async fn find_user_by_email(&self, email: &str) -> Result<Option<(User, String)>> {
        let r = sqlx::query(&format!("select {}, password_hash from users where lower(email) = lower($1)", user_cols()))
            .bind(email)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?;
        Ok(r.map(|r| (user_from(&r), r.get("password_hash"))))
    }

    async fn get_user(&self, id: &str) -> Result<User> {
        let r = sqlx::query(&format!("select {} from users where id = $1", user_cols()))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?
            .ok_or_else(|| nf("user"))?;
        Ok(user_from(&r))
    }

    async fn set_password(&self, user: &str, password_hash: &str) -> Result<()> {
        let n = sqlx::query("update users set password_hash = $2 where id = $1").bind(user).bind(password_hash).execute(&self.pool).await.map_err(db)?;
        if n.rows_affected() == 0 { Err(nf("user")) } else { Ok(()) }
    }

    async fn mark_email_verified(&self, user: &str) -> Result<()> {
        let n = sqlx::query("update users set email_verified = true where id = $1").bind(user).execute(&self.pool).await.map_err(db)?;
        if n.rows_affected() == 0 { Err(nf("user")) } else { Ok(()) }
    }

    async fn delete_user(&self, user: &str) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let exists = sqlx::query("select 1 from users where id = $1 for update").bind(user).fetch_optional(&mut *tx).await.map_err(db)?;
        if exists.is_none() {
            return Err(nf("user"));
        }
        sqlx::query(
            "delete from orgs where id in (select m.org_id from org_members m where m.user_id = $1 \
             and (select count(*) from org_members x where x.org_id = m.org_id) = 1)",
        )
        .bind(user)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query("delete from users where id = $1").bind(user).execute(&mut *tx).await.map_err(db)?;
        tx.commit().await.map_err(db)
    }

    async fn create_org(&self, name: &str, owner: &str) -> Result<Org> {
        if !valid_workspace_name(name) {
            return Err(StoreError::Invalid("organization names use letters, numbers, dash and underscore, up to 90 characters".into()));
        }
        let mut tx = self.pool.begin().await.map_err(db)?;
        let r = sqlx::query("insert into orgs (name) values ($1) returning id, extract(epoch from created_at)::bigint as created_at")
            .bind(name)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| match db(e) {
                StoreError::Conflict(_) => StoreError::Conflict(format!("an organization named {name} already exists")),
                other => other,
            })?;
        let org = Org { id: r.get("id"), name: name.to_string(), created_at: r.get::<i64, _>("created_at") as u64 };
        sqlx::query("insert into org_members (org_id, user_id, role) values ($1,$2,'owner')")
            .bind(&org.id)
            .bind(owner)
            .execute(&mut *tx)
            .await
            .map_err(|e| match db(e) {
                StoreError::NotFound(_) => nf("user"),
                other => other,
            })?;
        tx.commit().await.map_err(db)?;
        Ok(org)
    }

    async fn add_member(&self, org: &str, user: &str, role: &str) -> Result<()> {
        if !matches!(role, "owner" | "admin" | "member") {
            return Err(StoreError::Invalid("role must be owner, admin or member".into()));
        }
        sqlx::query("insert into org_members (org_id, user_id, role) values ($1,$2,$3)")
            .bind(org)
            .bind(user)
            .bind(role)
            .execute(&self.pool)
            .await
            .map_err(|e| match db(e) {
                StoreError::Conflict(_) => StoreError::Conflict("already a member".into()),
                other => other,
            })?;
        Ok(())
    }

    async fn orgs_for_user(&self, user: &str) -> Result<Vec<(Org, String)>> {
        let rows = sqlx::query(
            "select o.id, o.name, extract(epoch from o.created_at)::bigint as created_at, m.role \
             from org_members m join orgs o on o.id = m.org_id where m.user_id = $1 order by o.name",
        )
        .bind(user)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows
            .iter()
            .map(|r| (Org { id: r.get("id"), name: r.get("name"), created_at: r.get::<i64, _>("created_at") as u64 }, r.get("role")))
            .collect())
    }

    async fn member_role(&self, org: &str, user: &str) -> Result<Option<String>> {
        sqlx::query_scalar("select role from org_members where org_id = $1 and user_id = $2")
            .bind(org)
            .bind(user)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)
    }

    async fn create_session(&self, user: &str, token_hash: &[u8], expires_at: u64) -> Result<()> {
        sqlx::query("insert into sessions (token_hash, user_id, expires_at) values ($1,$2,to_timestamp($3))")
            .bind(token_hash)
            .bind(user)
            .bind(ts(expires_at))
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn session_user(&self, token_hash: &[u8], at: u64) -> Result<Option<Id>> {
        sqlx::query_scalar("select user_id from sessions where token_hash = $1 and expires_at >= to_timestamp($2)")
            .bind(token_hash)
            .bind(ts(at))
            .fetch_optional(&self.pool)
            .await
            .map_err(db)
    }

    async fn delete_session(&self, token_hash: &[u8]) -> Result<()> {
        sqlx::query("delete from sessions where token_hash = $1").bind(token_hash).execute(&self.pool).await.map_err(db)?;
        Ok(())
    }

    async fn delete_user_sessions(&self, user: &str) -> Result<()> {
        sqlx::query("delete from sessions where user_id = $1").bind(user).execute(&self.pool).await.map_err(db)?;
        Ok(())
    }

    async fn create_api_token(
        &self,
        kind: &str,
        user: Option<&str>,
        org: &str,
        description: &str,
        token_hash: &[u8],
        expires_at: Option<u64>,
    ) -> Result<ApiToken> {
        if !matches!(kind, "user" | "org") || (kind == "user" && user.is_none()) {
            return Err(StoreError::Invalid("a token is either a user token (with a user) or an org token".into()));
        }
        let r = sqlx::query(
            "insert into api_tokens (kind, user_id, org_id, description, token_hash, expires_at) \
             values ($1,$2,$3,$4,$5, case when $6::bigint is null then null else to_timestamp($6::bigint) end) \
             returning id, kind, user_id, org_id, description, extract(epoch from created_at)::bigint as created_at, \
             extract(epoch from expires_at)::bigint as expires_at",
        )
        .bind(kind)
        .bind(user)
        .bind(org)
        .bind(description)
        .bind(token_hash)
        .bind(expires_at.map(ts))
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(token_from(&r))
    }

    async fn find_api_token(&self, token_hash: &[u8], at: u64) -> Result<Option<ApiToken>> {
        let r = sqlx::query(
            "select id, kind, user_id, org_id, description, extract(epoch from created_at)::bigint as created_at, \
             extract(epoch from expires_at)::bigint as expires_at from api_tokens \
             where token_hash = $1 and (expires_at is null or expires_at >= to_timestamp($2))",
        )
        .bind(token_hash)
        .bind(ts(at))
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(r.map(|r| token_from(&r)))
    }

    async fn list_api_tokens(&self, org: &str) -> Result<Vec<ApiToken>> {
        let rows = sqlx::query(
            "select id, kind, user_id, org_id, description, extract(epoch from created_at)::bigint as created_at, \
             extract(epoch from expires_at)::bigint as expires_at from api_tokens where org_id = $1 order by created_at, id",
        )
        .bind(org)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows.iter().map(token_from).collect())
    }

    async fn delete_api_token(&self, org: &str, id: &str) -> Result<()> {
        let n = sqlx::query("delete from api_tokens where id = $1 and org_id = $2").bind(id).bind(org).execute(&self.pool).await.map_err(db)?;
        if n.rows_affected() == 0 { Err(nf("token")) } else { Ok(()) }
    }

    async fn create_email_token(&self, user: &str, purpose: &str, token_hash: &[u8], expires_at: u64) -> Result<()> {
        sqlx::query("insert into email_tokens (token_hash, user_id, purpose, expires_at) values ($1,$2,$3,to_timestamp($4))")
            .bind(token_hash)
            .bind(user)
            .bind(purpose)
            .bind(ts(expires_at))
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn consume_email_token(&self, token_hash: &[u8], purpose: &str, at: u64) -> Result<Option<Id>> {
        sqlx::query_scalar(
            "update email_tokens set used_at = now() where token_hash = $1 and purpose = $2 \
             and used_at is null and expires_at >= to_timestamp($3) returning user_id",
        )
        .bind(token_hash)
        .bind(purpose)
        .bind(ts(at))
        .fetch_optional(&self.pool)
        .await
        .map_err(db)
    }

    // ---- workspaces ----
    async fn create_workspace(&self, org: &str, name: &str) -> Result<Workspace> {
        if !valid_workspace_name(name) {
            return Err(StoreError::Invalid("workspace names use letters, numbers, dash and underscore, up to 90 characters".into()));
        }
        let r = sqlx::query(&format!("insert into workspaces (org_id, name) values ($1,$2) returning {}", ws_cols()))
            .bind(org)
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| match db(e) {
                StoreError::Conflict(_) => StoreError::Conflict(format!("a workspace named {name} already exists")),
                StoreError::NotFound(_) => nf("organization"),
                other => other,
            })?;
        Ok(ws_from(&r))
    }

    async fn list_workspaces(&self, org: &str) -> Result<Vec<Workspace>> {
        self.expire_locks().await?;
        let rows = sqlx::query(&format!("select {} from workspaces where org_id = $1 order by name", ws_cols()))
            .bind(org)
            .fetch_all(&self.pool)
            .await
            .map_err(db)?;
        Ok(rows.iter().map(ws_from).collect())
    }

    async fn get_workspace(&self, id: &str) -> Result<Workspace> {
        self.expire_locks().await?;
        let r = sqlx::query(&format!("select {} from workspaces where id = $1", ws_cols()))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?
            .ok_or_else(|| nf("workspace"))?;
        Ok(ws_from(&r))
    }

    async fn lock_workspace(&self, id: &str, owner: &str, reason: &str, info: Option<&str>, lease_secs: Option<u64>) -> Result<Workspace> {
        self.expire_locks().await?;
        let r = sqlx::query(&format!(
            "update workspaces set lock_owner = $2, lock_reason = $3, lock_info = $4, lock_since = now(), \
             lock_expires_at = case when $5::bigint is null then null else now() + ($5::bigint * interval '1 second') end \
             where id = $1 and lock_owner is null returning {}",
            ws_cols()
        ))
        .bind(id)
        .bind(owner)
        .bind(reason)
        .bind(info)
        .bind(lease_secs.map(|l| l.min(i64::MAX as u64 / 4) as i64))
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        match r {
            Some(r) => Ok(ws_from(&r)),
            None => {
                let ws = self.get_workspace(id).await?;
                Err(StoreError::Conflict(format!("workspace is already locked ({})", ws.lock.map(|l| l.reason).unwrap_or_default())))
            }
        }
    }

    async fn heartbeat_lock(&self, id: &str, owner: &str, lease_secs: u64) -> Result<Workspace> {
        self.expire_locks().await?;
        let r = sqlx::query(&format!(
            "update workspaces set lock_expires_at = now() + ($3::bigint * interval '1 second') \
             where id = $1 and lock_owner = $2 and lock_expires_at is not null returning {}",
            ws_cols()
        ))
        .bind(id)
        .bind(owner)
        .bind(lease_secs.min(i64::MAX as u64 / 4) as i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        match r {
            Some(r) => Ok(ws_from(&r)),
            None => {
                let ws = self.get_workspace(id).await?;
                Err(StoreError::Conflict(match ws.lock {
                    Some(l) if l.owner == owner => "this lock has no lease, so there is nothing to renew".into(),
                    Some(_) => "the workspace is locked by someone else".into(),
                    None => "workspace is not locked (the lock may have lapsed)".into(),
                }))
            }
        }
    }

    async fn update_workspace(&self, id: &str, patch: WorkspacePatch) -> Result<Workspace> {
        if let Some(name) = &patch.name {
            if !valid_workspace_name(name) {
                return Err(StoreError::Invalid("workspace names use letters, numbers, dash and underscore, up to 90 characters".into()));
            }
        }
        if let Some(d) = &patch.description {
            if d.chars().count() > 500 {
                return Err(StoreError::Invalid("the description is limited to 500 characters".into()));
            }
        }
        if let Some(Some(secs)) = patch.drift_interval {
            if secs == 0 || secs > 31_536_000 {
                return Err(StoreError::Invalid("the drift check interval must be from 1 second to 31536000".into()));
            }
        }
        let name = patch.name.clone();
        let r = sqlx::query(&format!(
            "update workspaces set name = coalesce($2, name), description = coalesce($3, description), auto_apply = coalesce($4, auto_apply), \
             drift_interval = case when $5 then $6 else drift_interval end \
             where id = $1 returning {}",
            ws_cols()
        ))
        .bind(id)
        .bind(patch.name)
        .bind(patch.description)
        .bind(patch.auto_apply)
        .bind(patch.drift_interval.is_some())
        .bind(patch.drift_interval.flatten().map(|d| d as i64))
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| match db(e) {
            StoreError::Conflict(_) => StoreError::Conflict(format!("a workspace named {} already exists", name.unwrap_or_default())),
            other => other,
        })?;
        r.map(|r| ws_from(&r)).ok_or_else(|| nf("workspace"))
    }

    async fn delete_workspace(&self, id: &str) -> Result<()> {
        self.expire_locks().await?;
        let mut tx = self.pool.begin().await.map_err(db)?;
        let row = sqlx::query("select lock_reason, lock_owner from workspaces where id = $1 for update").bind(id).fetch_optional(&mut *tx).await.map_err(db)?.ok_or_else(|| nf("workspace"))?;
        if row.get::<Option<String>, _>("lock_owner").is_some() {
            let why: Option<String> = row.get("lock_reason");
            return Err(StoreError::Conflict(format!("the workspace is locked ({}); unlock it or let the run finish first", why.unwrap_or_default())));
        }
        let busy: bool = sqlx::query_scalar("select exists (select 1 from runs where workspace_id = $1 and status in ('planning','applying','apply_queued'))")
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?;
        if busy {
            return Err(StoreError::Conflict("a run is working on this workspace; wait for it to finish or cancel it first".into()));
        }
        sqlx::query("delete from workspaces where id = $1").bind(id).execute(&mut *tx).await.map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(())
    }

    async fn unlock_workspace_owned(&self, id: &str, owner: &str) -> Result<Workspace> {
        self.expire_locks().await?;
        let r = sqlx::query(&format!(
            "update workspaces set lock_owner = null, lock_reason = null, lock_since = null, lock_info = null, lock_expires_at = null \
             where id = $1 and lock_owner = $2 returning {}",
            ws_cols()
        ))
        .bind(id)
        .bind(owner)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        match r {
            Some(r) => Ok(ws_from(&r)),
            None => {
                let ws = self.get_workspace(id).await?;
                Err(StoreError::Conflict(if ws.lock.is_some() { "the workspace is locked by someone else".into() } else { "workspace is not locked".into() }))
            }
        }
    }

    async fn unlock_workspace(&self, id: &str, force: bool) -> Result<Workspace> {
        self.expire_locks().await?;
        let mut tx = self.pool.begin().await.map_err(db)?;
        let row = sqlx::query("select lock_owner from workspaces where id = $1 for update").bind(id).fetch_optional(&mut *tx).await.map_err(db)?.ok_or_else(|| nf("workspace"))?;
        let owner: Option<String> = row.get("lock_owner");
        let Some(owner) = owner else {
            return Err(StoreError::Conflict("workspace is not locked".into()));
        };
        if let Some(run_id) = owner.strip_prefix("run:") {
            if !force {
                return Err(StoreError::Conflict("the lock is held by a run; discard or cancel the run, or force-unlock".into()));
            }
            // Force ends the run that holds the lock; otherwise it would keep working with no lock.
            let status: Option<String> = sqlx::query_scalar("select status from runs where id = $1 for update").bind(run_id).fetch_optional(&mut *tx).await.map_err(db)?;
            match status.as_deref() {
                Some("applying") => return Err(StoreError::Conflict("a run is applying; wait for it to finish before unlocking".into())),
                Some("needs_confirmation") => {
                    transition(&mut tx, run_id, Event::Discard).await?;
                }
                Some("planning") | Some("apply_queued") => {
                    transition(&mut tx, run_id, Event::Cancel).await?;
                }
                _ => {}
            }
        }
        let r = sqlx::query(&format!(
            "update workspaces set lock_owner = null, lock_reason = null, lock_since = null, lock_info = null, lock_expires_at = null where id = $1 returning {}",
            ws_cols()
        ))
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(ws_from(&r))
    }

    async fn create_config_version(&self, ws: &str, text: &str, source: &str) -> Result<ConfigVersion> {
        let r = sqlx::query(
            "insert into configuration_versions (workspace_id, source, text) values ($1,$2,$3) \
             returning id, extract(epoch from created_at)::bigint as created_at",
        )
        .bind(ws)
        .bind(source)
        .bind(text)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| match db(e) {
            StoreError::NotFound(_) => nf("workspace"),
            other => other,
        })?;
        Ok(ConfigVersion { id: r.get("id"), workspace_id: ws.to_string(), source: source.to_string(), text: text.to_string(), status: "uploaded".into(), created_at: r.get::<i64, _>("created_at") as u64 })
    }

    async fn create_pending_config(&self, ws: &str, source: &str) -> Result<ConfigVersion> {
        let r = sqlx::query(
            "insert into configuration_versions (workspace_id, source, text, status) values ($1,$2,'','pending')              returning id, extract(epoch from created_at)::bigint as created_at",
        )
        .bind(ws)
        .bind(source)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| match db(e) {
            StoreError::NotFound(_) => nf("workspace"),
            other => other,
        })?;
        Ok(ConfigVersion { id: r.get("id"), workspace_id: ws.to_string(), source: source.to_string(), text: String::new(), status: "pending".into(), created_at: r.get::<i64, _>("created_at") as u64 })
    }

    async fn upload_config(&self, id: &str, text: &str) -> Result<ConfigVersion> {
        let done = sqlx::query("update configuration_versions set text = $2, status = 'uploaded' where id = $1 and status = 'pending'")
            .bind(id)
            .bind(text)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        if done.rows_affected() == 0 {
            // Either it does not exist or it was not pending; tell them apart.
            self.get_config_version(id).await?;
            return Err(StoreError::Conflict("this configuration version already has its content".into()));
        }
        self.get_config_version(id).await
    }

    async fn get_config_version(&self, id: &str) -> Result<ConfigVersion> {
        let r = sqlx::query("select id, workspace_id, source, text, status, extract(epoch from created_at)::bigint as created_at from configuration_versions where id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?
            .ok_or_else(|| nf("configuration version"))?;
        Ok(cv_from(&r))
    }

    async fn latest_config(&self, ws: &str) -> Result<Option<ConfigVersion>> {
        let r = sqlx::query("select id, workspace_id, source, text, status, extract(epoch from created_at)::bigint as created_at from configuration_versions where workspace_id = $1 and status = 'uploaded' order by created_at desc, id desc limit 1")
            .bind(ws)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?;
        Ok(r.map(|r| cv_from(&r)))
    }

    // ---- variables ----
    async fn set_variable(&self, ws: &str, v: NewVariable) -> Result<Variable> {
        if !valid_variable_key(&v.key) {
            return Err(StoreError::Invalid("variable keys use letters, numbers and underscore and cannot start with a digit".into()));
        }
        if !matches!(v.category.as_str(), "terraform" | "env") {
            return Err(StoreError::Invalid("category must be terraform or env".into()));
        }
        let stored = if v.sensitive { self.sealer.seal_var(ws, &v.category, &v.key, &v.value) } else { v.value.clone() };
        let r = sqlx::query(
            "insert into variables (workspace_id, key, category, hcl, sensitive, value, description) values ($1,$2,$3,$4,$5,$6,$7) \
             on conflict (workspace_id, category, key) do update set hcl = excluded.hcl, sensitive = excluded.sensitive, \
             value = excluded.value, description = excluded.description \
             returning id, workspace_id, key, category, hcl, sensitive, value, description",
        )
        .bind(ws)
        .bind(&v.key)
        .bind(&v.category)
        .bind(v.hcl)
        .bind(v.sensitive)
        .bind(&stored)
        .bind(&v.description)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| match db(e) {
            StoreError::NotFound(_) => nf("workspace"),
            other => other,
        })?;
        Ok(redact(var_from(&r)))
    }

    async fn list_variables(&self, ws: &str) -> Result<Vec<Variable>> {
        let rows = sqlx::query("select id, workspace_id, key, category, hcl, sensitive, value, description from variables where workspace_id = $1 order by category, key")
            .bind(ws)
            .fetch_all(&self.pool)
            .await
            .map_err(db)?;
        Ok(rows.iter().map(|r| redact(var_from(r))).collect())
    }

    async fn delete_variable(&self, ws: &str, id: &str) -> Result<()> {
        let n = sqlx::query("delete from variables where id = $1 and workspace_id = $2").bind(id).bind(ws).execute(&self.pool).await.map_err(db)?;
        if n.rows_affected() == 0 { Err(nf("variable")) } else { Ok(()) }
    }

    async fn variables_for_run(&self, ws: &str) -> Result<Vec<Variable>> {
        let rows = sqlx::query("select id, workspace_id, key, category, hcl, sensitive, value, description from variables where workspace_id = $1 order by category, key")
            .bind(ws)
            .fetch_all(&self.pool)
            .await
            .map_err(db)?;
        rows.iter()
            .map(|r| {
                let mut v = var_from(r);
                if v.sensitive {
                    let sealed = v.value.take().unwrap_or_default();
                    v.value = Some(self.sealer.open_var(ws, &v.category, &v.key, &sealed)?);
                }
                Ok(v)
            })
            .collect()
    }

    // ---- runs ----
    async fn create_run_with(
        &self,
        ws: &str,
        config_version: &str,
        operation: Operation,
        message: &str,
        auto_apply: bool,
        source: &str,
        opts: RunOptions,
    ) -> Result<Run> {
        if opts.refresh_only && (operation != Operation::PlanOnly || opts.skip_refresh) {
            return Err(StoreError::Invalid("a refresh-only run must be a plan-only run and cannot skip the refresh".into()));
        }
        let mut tx = self.pool.begin().await.map_err(db)?;
        let ok = sqlx::query("select status from configuration_versions where id = $1 and workspace_id = $2")
            .bind(config_version)
            .bind(ws)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?;
        if let Some(row) = &ok {
            if row.get::<String, _>("status") != "uploaded" {
                return Err(StoreError::Invalid("the configuration version has no uploaded content yet".into()));
            }
        }
        if ok.is_none() {
            let exists = sqlx::query("select 1 from workspaces where id = $1").bind(ws).fetch_optional(&mut *tx).await.map_err(db)?;
            return Err(if exists.is_none() { nf("workspace") } else { StoreError::NotFound("configuration version not found in this workspace".into()) });
        }
        let r = sqlx::query(&format!(
            "insert into runs (workspace_id, config_version_id, status, operation, source, message, auto_apply, skip_refresh, refresh_only, vcs, status_timestamps) \
             values ($1,$2,'pending',$3,$4,$5,$6,$7,$8,$9, jsonb_build_object('pending', extract(epoch from now())::bigint)) returning {}",
            run_cols()
        ))
        .bind(ws)
        .bind(config_version)
        .bind(enum_str(&operation))
        .bind(source)
        .bind(message)
        .bind(auto_apply)
        .bind(opts.skip_refresh)
        .bind(opts.refresh_only)
        .bind(opts.vcs.as_ref().map(Json))
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        let run = run_from(&r)?;
        sqlx::query("update workspaces set current_run = $2 where id = $1").bind(ws).bind(&run.id).execute(&mut *tx).await.map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(run)
    }

    async fn get_run(&self, id: &str) -> Result<Run> {
        let r = sqlx::query(&format!("select {} from runs where id = $1", run_cols()))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?
            .ok_or_else(|| nf("run"))?;
        run_from(&r)
    }

    async fn list_runs(&self, ws: Option<&str>) -> Result<Vec<Run>> {
        let rows = match ws {
            Some(w) => sqlx::query(&format!("select {} from runs where workspace_id = $1 order by seq desc", run_cols())).bind(w).fetch_all(&self.pool).await,
            None => sqlx::query(&format!("select {} from runs order by seq desc", run_cols())).fetch_all(&self.pool).await,
        }
        .map_err(db)?;
        rows.iter().map(run_from).collect()
    }

    async fn list_runs_in_org(&self, org: &str) -> Result<Vec<Run>> {
        let rows = sqlx::query(&format!(
            "select {} from runs where workspace_id in (select id from workspaces where org_id = $1) order by seq desc",
            run_cols()
        ))
        .bind(org)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.iter().map(run_from).collect()
    }

    async fn apply_event(&self, run: &str, event: Event) -> Result<Run> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let out = transition(&mut tx, run, event).await?;
        tx.commit().await.map_err(db)?;
        Ok(out)
    }

    async fn set_plan(&self, run: &str, summary: PlanSummary, changes: Vec<ResourceChange>, blob: Vec<u8>) -> Result<Run> {
        let sealed = self.sealer.seal_plan(run, &blob);
        let r = sqlx::query(&format!(
            "update runs set summary = $2, changes = $3, plan_blob = $4 where id = $1 returning {}",
            run_cols()
        ))
        .bind(run)
        .bind(Json(summary))
        .bind(Json(&changes))
        .bind(sealed)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?
        .ok_or_else(|| nf("run"))?;
        run_from(&r)
    }

    async fn set_vcs_link(&self, ws: &str, repo: &str, branch: &str, working_dir: &str, token: Option<&str>) -> Result<(VcsLink, String)> {
        crate::memory::check_vcs_input(repo, branch, working_dir, token)?;
        let secret = tf_crypto::random_token("whsec_");
        let sealed_secret = self.sealer.seal_vcs(ws, "webhook", &secret);
        let sealed_token = token.map(|t| self.sealer.seal_vcs(ws, "token", t));
        let dir = working_dir.trim_matches('/').to_string();
        let r = sqlx::query(
            "insert into vcs_links (workspace_id, repo, branch, working_dir, webhook_secret, token) values ($1,$2,$3,$4,$5,$6) \
             on conflict (workspace_id) do update set repo = $2, branch = $3, working_dir = $4, webhook_secret = $5, \
               token = coalesce($6, vcs_links.token), created_at = now() \
             returning token is not null as has_token, extract(epoch from created_at)::bigint as created_at",
        )
        .bind(ws)
        .bind(repo)
        .bind(branch)
        .bind(&dir)
        .bind(sealed_secret)
        .bind(sealed_token)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| match db(e) {
            StoreError::NotFound(_) => nf("workspace"),
            other => other,
        })?;
        let link = VcsLink {
            workspace_id: ws.to_string(),
            repo: repo.to_string(),
            branch: branch.to_string(),
            working_dir: dir,
            has_token: r.get("has_token"),
            created_at: r.get::<i64, _>("created_at") as u64,
        };
        Ok((link, secret))
    }

    async fn get_vcs_link(&self, ws: &str) -> Result<Option<VcsLink>> {
        self.get_workspace(ws).await?;
        let r = sqlx::query("select repo, branch, working_dir, token is not null as has_token, extract(epoch from created_at)::bigint as created_at from vcs_links where workspace_id = $1")
            .bind(ws)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?;
        Ok(r.map(|r| VcsLink {
            workspace_id: ws.to_string(),
            repo: r.get("repo"),
            branch: r.get("branch"),
            working_dir: r.get("working_dir"),
            has_token: r.get("has_token"),
            created_at: r.get::<i64, _>("created_at") as u64,
        }))
    }

    async fn vcs_credentials(&self, ws: &str) -> Result<Option<VcsCredentials>> {
        let r = sqlx::query("select repo, branch, working_dir, webhook_secret, token, extract(epoch from created_at)::bigint as created_at from vcs_links where workspace_id = $1")
            .bind(ws)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?;
        let Some(r) = r else { return Ok(None) };
        let token_sealed: Option<String> = r.get("token");
        let link = VcsLink {
            workspace_id: ws.to_string(),
            repo: r.get("repo"),
            branch: r.get("branch"),
            working_dir: r.get("working_dir"),
            has_token: token_sealed.is_some(),
            created_at: r.get::<i64, _>("created_at") as u64,
        };
        let webhook_secret = self.sealer.open_vcs(ws, "webhook", &r.get::<String, _>("webhook_secret"))?;
        let token = token_sealed.as_deref().map(|t| self.sealer.open_vcs(ws, "token", t)).transpose()?;
        Ok(Some(VcsCredentials { link, webhook_secret, token }))
    }

    async fn delete_vcs_link(&self, ws: &str) -> Result<()> {
        self.get_workspace(ws).await?;
        let done = sqlx::query("delete from vcs_links where workspace_id = $1").bind(ws).execute(&self.pool).await.map_err(db)?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound("this workspace is not connected to a repository".into()));
        }
        Ok(())
    }

    async fn record_drift(&self, ws: &str, status: DriftStatus) -> Result<()> {
        let done = sqlx::query("update workspaces set drift_checked_at = to_timestamp($2), drift_run = $3, drift_status = $4, drift_changes = $5 where id = $1")
            .bind(ws)
            .bind(status.checked_at as f64)
            .bind(&status.run_id)
            .bind(&status.status)
            .bind(status.changes as i32)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        if done.rows_affected() == 0 {
            return Err(nf("workspace"));
        }
        Ok(())
    }

    async fn due_drift_checks(&self) -> Result<Vec<Workspace>> {
        let rows = sqlx::query(&format!(
            "select {} from workspaces w where drift_interval is not null and current_state_version is not null \
             and (drift_checked_at is null or drift_checked_at + (drift_interval * interval '1 second') <= now()) \
             and not exists (select 1 from runs r where r.workspace_id = w.id and r.refresh_only and r.status in ('pending','planning')) \
             order by id",
            ws_cols()
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows.iter().map(ws_from).collect())
    }

    async fn set_lint(&self, run: &str, findings: Vec<LintFinding>) -> Result<()> {
        let done = sqlx::query("update runs set lint = $2 where id = $1").bind(run).bind(Json(&findings)).execute(&self.pool).await.map_err(db)?;
        if done.rows_affected() == 0 {
            return Err(nf("run"));
        }
        Ok(())
    }

    async fn get_plan_blob(&self, run: &str) -> Result<Vec<u8>> {
        let sealed: Option<Option<String>> = sqlx::query_scalar("select plan_blob from runs where id = $1").bind(run).fetch_optional(&self.pool).await.map_err(db)?;
        match sealed.flatten() {
            Some(s) => self.sealer.open_plan(run, &s),
            None => Err(StoreError::NotFound("no saved plan for this run".into())),
        }
    }

    async fn claim_next(&self) -> Result<Option<Claim>> {
        self.expire_locks().await?;
        let mut tx = self.pool.begin().await.map_err(db)?;

        // Confirmed applies first: they already hold their workspace lock.
        let apply: Option<String> = sqlx::query_scalar("select id from runs where status = 'apply_queued' order by seq limit 1 for update skip locked")
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?;
        if let Some(id) = apply {
            let run = transition(&mut tx, &id, Event::ApplyStart).await?;
            tx.commit().await.map_err(db)?;
            return Ok(Some(Claim::Apply(run)));
        }

        // The oldest pending run: speculative plans always, real runs only when their
        // workspace is free and no older real run is waiting on it.
        let pick = sqlx::query(
            "select r.id, r.workspace_id, r.operation from runs r join workspaces w on w.id = r.workspace_id \
             where r.status = 'pending' and (r.operation = 'plan_only' or (w.lock_owner is null \
               and not exists (select 1 from runs o where o.workspace_id = r.workspace_id and o.status = 'pending' \
                 and o.operation <> 'plan_only' and o.seq < r.seq) \
               and not exists (select 1 from runs h where h.workspace_id = r.workspace_id and h.operation <> 'plan_only' \
                 and h.status in ('planning','needs_confirmation','apply_queued','applying')))) \
             order by r.seq limit 1 for update of r skip locked",
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?;
        let Some(pick) = pick else {
            return Ok(None);
        };
        let (id, ws, op): (String, String, String) = (pick.get("id"), pick.get("workspace_id"), pick.get("operation"));
        if op != "plan_only" {
            let took = sqlx::query(
                "update workspaces set lock_owner = $2, lock_reason = 'run in progress', lock_since = now() \
                 where id = $1 and lock_owner is null",
            )
            .bind(&ws)
            .bind(format!("run:{id}"))
            .execute(&mut *tx)
            .await
            .map_err(db)?;
            if took.rows_affected() != 1 {
                return Ok(None); // lost a race; the runner polls again
            }
        }
        let mut run = transition(&mut tx, &id, Event::Start).await?;
        let serial: Option<i64> = sqlx::query_scalar("select max(serial) from state_versions where workspace_id = $1").bind(&ws).fetch_one(&mut *tx).await.map_err(db)?;
        sqlx::query("update runs set input_state_serial = $2 where id = $1").bind(&id).bind(serial).execute(&mut *tx).await.map_err(db)?;
        run.input_state_serial = serial.map(|s| s as u64);
        tx.commit().await.map_err(db)?;
        Ok(Some(Claim::Plan(run)))
    }

    async fn recover_interrupted(&self) -> Result<usize> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let rows = sqlx::query("select id, status from runs where status in ('planning','applying') order by seq for update").fetch_all(&mut *tx).await.map_err(db)?;
        for r in &rows {
            let id: String = r.get("id");
            let status: String = r.get("status");
            let ev = if status == "planning" { Event::PlanFail } else { Event::ApplyFail };
            transition(&mut tx, &id, ev).await?;
            sqlx::query("insert into run_logs (run_id, line) values ($1, $2)")
                .bind(&id)
                .bind("Error: interrupted: the server stopped while this run was working. Queue a new run.")
                .execute(&mut *tx)
                .await
                .map_err(db)?;
        }
        tx.commit().await.map_err(db)?;
        Ok(rows.len())
    }

    async fn append_log(&self, run: &str, line: &str) -> Result<()> {
        sqlx::query("insert into run_logs (run_id, line) values ($1,$2)").bind(run).bind(line).execute(&self.pool).await.map_err(db)?;
        Ok(())
    }

    async fn get_log(&self, run: &str) -> Result<Vec<String>> {
        sqlx::query_scalar("select line from run_logs where run_id = $1 order by id").bind(run).fetch_all(&self.pool).await.map_err(db)
    }

    async fn get_log_from(&self, run: &str, from: usize) -> Result<Vec<String>> {
        sqlx::query_scalar("select line from run_logs where run_id = $1 order by id offset $2").bind(run).bind(from as i64).fetch_all(&self.pool).await.map_err(db)
    }

    // ---- state ----
    async fn current_state(&self, ws: &str) -> Result<Option<StateVersion>> {
        let r = sqlx::query(&format!("select {STATE_COLS} from state_versions where workspace_id = $1 order by serial desc limit 1"))
            .bind(ws)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?;
        r.map(|r| self.open_state(&r)).transpose()
    }

    async fn get_state_version(&self, id: &str) -> Result<StateVersion> {
        let r = sqlx::query(&format!("select {STATE_COLS} from state_versions where id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?
            .ok_or_else(|| nf("state version"))?;
        self.open_state(&r)
    }

    async fn list_states(&self, ws: &str) -> Result<Vec<StateVersion>> {
        let rows = sqlx::query(&format!("select {STATE_COLS} from state_versions where workspace_id = $1 order by serial desc"))
            .bind(ws)
            .fetch_all(&self.pool)
            .await
            .map_err(db)?;
        rows.iter().map(|r| self.open_state(r)).collect()
    }

    async fn finalize_state(&self, ws: &str, run: &str, content: &str, resources: usize) -> Result<StateVersion> {
        let sealed = self.sealer.seal_state(ws, content);
        let mut tx = self.pool.begin().await.map_err(db)?;
        // Serialise writers per workspace so serials never collide.
        sqlx::query("select 1 from workspaces where id = $1 for update").bind(ws).fetch_optional(&mut *tx).await.map_err(db)?.ok_or_else(|| nf("workspace"))?;
        let serial: i64 = sqlx::query_scalar("select coalesce(max(serial), 0) + 1 from state_versions where workspace_id = $1").bind(ws).fetch_one(&mut *tx).await.map_err(db)?;
        let r = sqlx::query(&format!(
            "insert into state_versions (workspace_id, run_id, serial, content, resource_count) values ($1,$2,$3,$4,$5) returning {STATE_COLS}"
        ))
        .bind(ws)
        .bind(if run.is_empty() { None } else { Some(run) })
        .bind(serial)
        .bind(sealed)
        .bind(resources as i32)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        let sv = self.open_state(&r)?;
        sqlx::query("update workspaces set current_state_version = $2, resource_count = $3 where id = $1").bind(ws).bind(&sv.id).bind(resources as i32).execute(&mut *tx).await.map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(sv)
    }
}

fn cv_from(r: &PgRow) -> ConfigVersion {
    ConfigVersion {
        id: r.get("id"),
        workspace_id: r.get("workspace_id"),
        source: r.get("source"),
        text: r.get("text"),
        status: r.get("status"),
        created_at: r.get::<i64, _>("created_at") as u64,
    }
}

fn redact(mut v: Variable) -> Variable {
    if v.sensitive {
        v.value = None;
    }
    v
}
