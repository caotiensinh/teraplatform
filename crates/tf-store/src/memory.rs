use crate::{Claim, Result, Sealer, Store, StoreError};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use tf_core::*;
use tf_crypto::Crypto;

struct StoredUser {
    user: User,
    password_hash: String,
}

#[derive(Default)]
struct Inner {
    users: HashMap<Id, StoredUser>,
    orgs: HashMap<Id, Org>,
    members: Vec<(Id, Id, String)>, // org, user, role
    sessions: HashMap<Vec<u8>, (Id, u64)>,
    tokens: HashMap<Id, (ApiToken, Vec<u8>)>,
    email_tokens: HashMap<Vec<u8>, (Id, String, u64, bool)>, // user, purpose, expires, used
    workspaces: HashMap<Id, Workspace>,
    configs: HashMap<Id, ConfigVersion>,
    variables: HashMap<Id, Vec<Variable>>, // sensitive values sealed
    runs: HashMap<Id, Run>,
    run_order: Vec<Id>,
    plan_blobs: HashMap<Id, String>, // sealed
    logs: HashMap<Id, Vec<String>>,
    states: HashMap<Id, Vec<StateVersion>>, // content sealed
}

pub struct MemoryStore {
    inner: Mutex<Inner>,
    sealer: Sealer,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStore {
    /// A store with a throwaway key: data does not outlive the process anyway.
    pub fn new() -> Self {
        Self::with_crypto(Crypto::ephemeral())
    }

    pub fn with_crypto(c: Crypto) -> Self {
        Self { inner: Mutex::new(Inner::default()), sealer: Sealer::new(c) }
    }

    fn g(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Test seam: what is actually held for a workspace's current state.
    pub fn raw_state_content(&self, ws: &str) -> Option<String> {
        self.g().states.get(ws).and_then(|v| v.last()).map(|s| s.content.clone())
    }

    /// Test seam: the stored form of a sensitive variable.
    pub fn raw_variable_value(&self, ws: &str, key: &str) -> Option<String> {
        self.g().variables.get(ws).and_then(|v| v.iter().find(|x| x.key == key)).and_then(|v| v.value.clone())
    }

    pub fn raw_plan_blob(&self, run: &str) -> Option<String> {
        self.g().plan_blobs.get(run).cloned()
    }
}

fn nf(what: &str) -> StoreError {
    StoreError::NotFound(format!("{what} not found"))
}

fn run_lock_owner(run: &str) -> String {
    format!("run:{run}")
}

/// Applies an event and releases the workspace lock if the run no longer holds it.
fn transition(g: &mut Inner, run_id: &str, ev: Event) -> Result<Run> {
    let run = g.runs.get_mut(run_id).ok_or_else(|| nf("run"))?;
    let to = next(run.status, ev)?;
    let t = now();
    run.status = to;
    run.updated_at = t;
    run.status_timestamps.insert(to.as_str().to_string(), t);
    let out = run.clone();
    if !to.holds_lock() {
        if let Some(ws) = g.workspaces.get_mut(&out.workspace_id) {
            if ws.lock.as_ref().map(|l| l.owner == run_lock_owner(&out.id)).unwrap_or(false) {
                ws.lock = None;
            }
        }
    }
    Ok(out)
}

/// Longest drift check interval: a year.
pub const MAX_DRIFT_INTERVAL: u64 = 31_536_000;

/// Most characters a workspace description may have.
pub const MAX_DESCRIPTION: usize = 500;

/// A lock whose lease has run out is no lock: dropped before anything reads or changes locks.
fn expire_locks(g: &mut Inner) {
    let t = now();
    for w in g.workspaces.values_mut() {
        if w.lock.as_ref().and_then(|l| l.expires_at).map(|e| e <= t).unwrap_or(false) {
            w.lock = None;
        }
    }
}

fn remove_workspace(g: &mut Inner, w: &str) {
    let run_ids: Vec<Id> = g.runs.values().filter(|r| r.workspace_id == w).map(|r| r.id.clone()).collect();
    for r in &run_ids {
        g.runs.remove(r);
        g.plan_blobs.remove(r);
        g.logs.remove(r);
    }
    g.run_order.retain(|id| !run_ids.contains(id));
    g.configs.retain(|_, c| c.workspace_id != w);
    g.variables.remove(w);
    g.states.remove(w);
    g.workspaces.remove(w);
}

fn remove_org(g: &mut Inner, org: &str) {
    let ws_ids: Vec<Id> = g.workspaces.values().filter(|w| w.org == org).map(|w| w.id.clone()).collect();
    for w in &ws_ids {
        remove_workspace(g, w);
    }
    g.tokens.retain(|_, (t, _)| t.org_id != org);
    g.members.retain(|(o, _, _)| o != org);
    g.orgs.remove(org);
}

#[async_trait]
impl Store for MemoryStore {
    // ---- accounts ----
    async fn create_user(&self, email: &str, password_hash: &str, display_name: &str) -> Result<User> {
        let mut g = self.g();
        let lower = email.to_lowercase();
        if g.users.values().any(|u| u.user.email.to_lowercase() == lower) {
            return Err(StoreError::Conflict("an account with that email already exists".into()));
        }
        let user = User {
            id: new_id(),
            email: email.to_string(),
            display_name: display_name.to_string(),
            email_verified: false,
            created_at: now(),
        };
        g.users.insert(user.id.clone(), StoredUser { user: user.clone(), password_hash: password_hash.to_string() });
        Ok(user)
    }

    async fn find_user_by_email(&self, email: &str) -> Result<Option<(User, String)>> {
        let lower = email.to_lowercase();
        Ok(self
            .g()
            .users
            .values()
            .find(|u| u.user.email.to_lowercase() == lower)
            .map(|u| (u.user.clone(), u.password_hash.clone())))
    }

    async fn get_user(&self, id: &str) -> Result<User> {
        self.g().users.get(id).map(|u| u.user.clone()).ok_or_else(|| nf("user"))
    }

    async fn set_password(&self, user: &str, password_hash: &str) -> Result<()> {
        self.g().users.get_mut(user).ok_or_else(|| nf("user"))?.password_hash = password_hash.to_string();
        Ok(())
    }

    async fn mark_email_verified(&self, user: &str) -> Result<()> {
        self.g().users.get_mut(user).ok_or_else(|| nf("user"))?.user.email_verified = true;
        Ok(())
    }

    async fn delete_user(&self, user: &str) -> Result<()> {
        let mut g = self.g();
        if !g.users.contains_key(user) {
            return Err(nf("user"));
        }
        let mine: Vec<Id> = g.members.iter().filter(|(_, u, _)| u == user).map(|(o, _, _)| o.clone()).collect();
        for org in mine {
            let count = g.members.iter().filter(|(o, _, _)| o == &org).count();
            if count == 1 {
                remove_org(&mut g, &org);
            }
        }
        g.members.retain(|(_, u, _)| u != user);
        g.sessions.retain(|_, (u, _)| u != user);
        g.tokens.retain(|_, (t, _)| t.user_id.as_deref() != Some(user));
        g.email_tokens.retain(|_, (u, _, _, _)| u != user);
        g.users.remove(user);
        Ok(())
    }

    async fn create_org(&self, name: &str, owner: &str) -> Result<Org> {
        if !valid_workspace_name(name) {
            return Err(StoreError::Invalid("organization names use letters, numbers, dash and underscore, up to 90 characters".into()));
        }
        let mut g = self.g();
        if !g.users.contains_key(owner) {
            return Err(nf("user"));
        }
        if g.orgs.values().any(|o| o.name == name) {
            return Err(StoreError::Conflict(format!("an organization named {name} already exists")));
        }
        let org = Org { id: new_id(), name: name.to_string(), created_at: now() };
        g.orgs.insert(org.id.clone(), org.clone());
        g.members.push((org.id.clone(), owner.to_string(), "owner".into()));
        Ok(org)
    }

    async fn add_member(&self, org: &str, user: &str, role: &str) -> Result<()> {
        if !matches!(role, "owner" | "admin" | "member") {
            return Err(StoreError::Invalid("role must be owner, admin or member".into()));
        }
        let mut g = self.g();
        if !g.orgs.contains_key(org) {
            return Err(nf("organization"));
        }
        if !g.users.contains_key(user) {
            return Err(nf("user"));
        }
        if g.members.iter().any(|(o, u, _)| o == org && u == user) {
            return Err(StoreError::Conflict("already a member".into()));
        }
        g.members.push((org.to_string(), user.to_string(), role.to_string()));
        Ok(())
    }

    async fn orgs_for_user(&self, user: &str) -> Result<Vec<(Org, String)>> {
        let g = self.g();
        let mut v: Vec<_> = g
            .members
            .iter()
            .filter(|(_, u, _)| u == user)
            .filter_map(|(o, _, r)| g.orgs.get(o).map(|org| (org.clone(), r.clone())))
            .collect();
        v.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        Ok(v)
    }

    async fn member_role(&self, org: &str, user: &str) -> Result<Option<String>> {
        Ok(self.g().members.iter().find(|(o, u, _)| o == org && u == user).map(|(_, _, r)| r.clone()))
    }

    async fn create_session(&self, user: &str, token_hash: &[u8], expires_at: u64) -> Result<()> {
        let mut g = self.g();
        if !g.users.contains_key(user) {
            return Err(nf("user"));
        }
        g.sessions.insert(token_hash.to_vec(), (user.to_string(), expires_at));
        Ok(())
    }

    async fn session_user(&self, token_hash: &[u8], at: u64) -> Result<Option<Id>> {
        Ok(self.g().sessions.get(token_hash).filter(|(_, exp)| *exp >= at).map(|(u, _)| u.clone()))
    }

    async fn delete_session(&self, token_hash: &[u8]) -> Result<()> {
        self.g().sessions.remove(token_hash);
        Ok(())
    }

    async fn delete_user_sessions(&self, user: &str) -> Result<()> {
        self.g().sessions.retain(|_, (u, _)| u != user);
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
        let mut g = self.g();
        if !g.orgs.contains_key(org) {
            return Err(nf("organization"));
        }
        let t = ApiToken {
            id: new_id(),
            kind: kind.to_string(),
            user_id: user.map(String::from),
            org_id: org.to_string(),
            description: description.to_string(),
            created_at: now(),
            expires_at,
        };
        g.tokens.insert(t.id.clone(), (t.clone(), token_hash.to_vec()));
        Ok(t)
    }

    async fn find_api_token(&self, token_hash: &[u8], at: u64) -> Result<Option<ApiToken>> {
        Ok(self
            .g()
            .tokens
            .values()
            .find(|(t, h)| h == token_hash && t.expires_at.map(|e| e >= at).unwrap_or(true))
            .map(|(t, _)| t.clone()))
    }

    async fn list_api_tokens(&self, org: &str) -> Result<Vec<ApiToken>> {
        let mut v: Vec<_> = self.g().tokens.values().filter(|(t, _)| t.org_id == org).map(|(t, _)| t.clone()).collect();
        v.sort_by_key(|t| (t.created_at, t.id.clone()));
        Ok(v)
    }

    async fn delete_api_token(&self, org: &str, id: &str) -> Result<()> {
        let mut g = self.g();
        match g.tokens.get(id) {
            Some((t, _)) if t.org_id == org => {
                g.tokens.remove(id);
                Ok(())
            }
            _ => Err(nf("token")),
        }
    }

    async fn create_email_token(&self, user: &str, purpose: &str, token_hash: &[u8], expires_at: u64) -> Result<()> {
        let mut g = self.g();
        if !g.users.contains_key(user) {
            return Err(nf("user"));
        }
        g.email_tokens.insert(token_hash.to_vec(), (user.to_string(), purpose.to_string(), expires_at, false));
        Ok(())
    }

    async fn consume_email_token(&self, token_hash: &[u8], purpose: &str, at: u64) -> Result<Option<Id>> {
        let mut g = self.g();
        match g.email_tokens.get_mut(token_hash) {
            Some((user, p, exp, used)) if p == purpose && !*used && *exp >= at => {
                *used = true;
                Ok(Some(user.clone()))
            }
            _ => Ok(None),
        }
    }

    // ---- workspaces ----
    async fn create_workspace(&self, org: &str, name: &str) -> Result<Workspace> {
        if !valid_workspace_name(name) {
            return Err(StoreError::Invalid(
                "workspace names use letters, numbers, dash and underscore, up to 90 characters".into(),
            ));
        }
        let mut g = self.g();
        if !g.orgs.contains_key(org) {
            return Err(nf("organization"));
        }
        if g.workspaces.values().any(|w| w.org == org && w.name == name) {
            return Err(StoreError::Conflict(format!("a workspace named {name} already exists")));
        }
        let ws = Workspace {
            id: new_id(),
            org: org.to_string(),
            name: name.to_string(),
            lock: None,
            current_run: None,
            current_state_version: None,
            resource_count: 0,
            description: String::new(),
            auto_apply: false,
            drift_interval: None,
            drift: None,
            created_at: now(),
        };
        g.workspaces.insert(ws.id.clone(), ws.clone());
        Ok(ws)
    }

    async fn list_workspaces(&self, org: &str) -> Result<Vec<Workspace>> {
        let mut g = self.g();
        expire_locks(&mut g);
        let mut v: Vec<_> = g.workspaces.values().filter(|w| w.org == org).cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(v)
    }

    async fn get_workspace(&self, id: &str) -> Result<Workspace> {
        let mut g = self.g();
        expire_locks(&mut g);
        g.workspaces.get(id).cloned().ok_or_else(|| nf("workspace"))
    }

    async fn lock_workspace(&self, id: &str, owner: &str, reason: &str, info: Option<&str>, lease_secs: Option<u64>) -> Result<Workspace> {
        let mut g = self.g();
        expire_locks(&mut g);
        let ws = g.workspaces.get_mut(id).ok_or_else(|| nf("workspace"))?;
        if let Some(l) = &ws.lock {
            return Err(StoreError::Conflict(format!("workspace is already locked ({})", l.reason)));
        }
        let t = now();
        ws.lock = Some(Lock {
            owner: owner.to_string(),
            reason: reason.to_string(),
            since: t,
            info: info.map(String::from),
            expires_at: lease_secs.map(|l| t.saturating_add(l)),
        });
        Ok(ws.clone())
    }

    async fn heartbeat_lock(&self, id: &str, owner: &str, lease_secs: u64) -> Result<Workspace> {
        let mut g = self.g();
        expire_locks(&mut g);
        let ws = g.workspaces.get_mut(id).ok_or_else(|| nf("workspace"))?;
        match &mut ws.lock {
            Some(l) if l.owner == owner && l.expires_at.is_some() => {
                l.expires_at = Some(now().saturating_add(lease_secs));
                Ok(ws.clone())
            }
            Some(l) if l.owner == owner => Err(StoreError::Conflict("this lock has no lease, so there is nothing to renew".into())),
            Some(_) => Err(StoreError::Conflict("the workspace is locked by someone else".into())),
            None => Err(StoreError::Conflict("workspace is not locked (the lock may have lapsed)".into())),
        }
    }

    async fn update_workspace(&self, id: &str, patch: WorkspacePatch) -> Result<Workspace> {
        let mut g = self.g();
        let org = g.workspaces.get(id).ok_or_else(|| nf("workspace"))?.org.clone();
        if let Some(name) = &patch.name {
            if !valid_workspace_name(name) {
                return Err(StoreError::Invalid("workspace names use letters, numbers, dash and underscore, up to 90 characters".into()));
            }
            if g.workspaces.values().any(|w| w.org == org && w.id != id && &w.name == name) {
                return Err(StoreError::Conflict(format!("a workspace named {name} already exists")));
            }
        }
        if let Some(d) = &patch.description {
            if d.chars().count() > MAX_DESCRIPTION {
                return Err(StoreError::Invalid(format!("the description is limited to {MAX_DESCRIPTION} characters")));
            }
        }
        if let Some(Some(secs)) = patch.drift_interval {
            if secs == 0 || secs > MAX_DRIFT_INTERVAL {
                return Err(StoreError::Invalid(format!("the drift check interval must be from 1 second to {MAX_DRIFT_INTERVAL}")));
            }
        }
        let ws = g.workspaces.get_mut(id).ok_or_else(|| nf("workspace"))?;
        if let Some(d) = patch.drift_interval {
            ws.drift_interval = d;
        }
        if let Some(n) = patch.name {
            ws.name = n;
        }
        if let Some(d) = patch.description {
            ws.description = d;
        }
        if let Some(a) = patch.auto_apply {
            ws.auto_apply = a;
        }
        Ok(ws.clone())
    }

    async fn delete_workspace(&self, id: &str) -> Result<()> {
        let mut g = self.g();
        expire_locks(&mut g);
        let ws = g.workspaces.get(id).ok_or_else(|| nf("workspace"))?;
        if let Some(l) = &ws.lock {
            return Err(StoreError::Conflict(format!("the workspace is locked ({}); unlock it or let the run finish first", l.reason)));
        }
        if g.runs.values().any(|r| r.workspace_id == id && r.operation != Operation::PlanOnly && r.status.holds_lock())
            || g.runs.values().any(|r| r.workspace_id == id && matches!(r.status, RunStatus::Planning | RunStatus::Applying | RunStatus::ApplyQueued))
        {
            return Err(StoreError::Conflict("a run is working on this workspace; wait for it to finish or cancel it first".into()));
        }
        remove_workspace(&mut g, id);
        Ok(())
    }

    async fn unlock_workspace_owned(&self, id: &str, owner: &str) -> Result<Workspace> {
        let mut g = self.g();
        expire_locks(&mut g);
        let ws = g.workspaces.get_mut(id).ok_or_else(|| nf("workspace"))?;
        match &ws.lock {
            Some(l) if l.owner == owner => {
                ws.lock = None;
                Ok(ws.clone())
            }
            Some(_) => Err(StoreError::Conflict("the workspace is locked by someone else".into())),
            None => Err(StoreError::Conflict("workspace is not locked".into())),
        }
    }

    async fn unlock_workspace(&self, id: &str, force: bool) -> Result<Workspace> {
        let mut g = self.g();
        expire_locks(&mut g);
        let lock = g.workspaces.get(id).ok_or_else(|| nf("workspace"))?.lock.clone();
        let Some(lock) = lock else {
            return Err(StoreError::Conflict("workspace is not locked".into()));
        };
        if let Some(run_id) = lock.owner.strip_prefix("run:") {
            if !force {
                return Err(StoreError::Conflict("the lock is held by a run; discard or cancel the run, or force-unlock".into()));
            }
            // Force ends the run that holds the lock; otherwise it would keep working with no lock.
            match g.runs.get(run_id).map(|r| r.status) {
                Some(RunStatus::Applying) => {
                    return Err(StoreError::Conflict("a run is applying; wait for it to finish before unlocking".into()))
                }
                Some(RunStatus::NeedsConfirmation) => {
                    transition(&mut g, run_id, Event::Discard)?;
                }
                Some(RunStatus::Planning) | Some(RunStatus::ApplyQueued) => {
                    transition(&mut g, run_id, Event::Cancel)?;
                }
                _ => {}
            }
        }
        let ws = g.workspaces.get_mut(id).ok_or_else(|| nf("workspace"))?;
        ws.lock = None;
        Ok(ws.clone())
    }

    async fn create_config_version(&self, ws: &str, text: &str, source: &str) -> Result<ConfigVersion> {
        let mut g = self.g();
        if !g.workspaces.contains_key(ws) {
            return Err(nf("workspace"));
        }
        let cv = ConfigVersion {
            id: new_id(),
            workspace_id: ws.to_string(),
            source: source.to_string(),
            text: text.to_string(),
            status: "uploaded".into(),
            created_at: now(),
        };
        g.configs.insert(cv.id.clone(), cv.clone());
        Ok(cv)
    }

    async fn create_pending_config(&self, ws: &str, source: &str) -> Result<ConfigVersion> {
        let mut g = self.g();
        if !g.workspaces.contains_key(ws) {
            return Err(nf("workspace"));
        }
        let cv = ConfigVersion { id: new_id(), workspace_id: ws.to_string(), source: source.to_string(), text: String::new(), status: "pending".into(), created_at: now() };
        g.configs.insert(cv.id.clone(), cv.clone());
        Ok(cv)
    }

    async fn upload_config(&self, id: &str, text: &str) -> Result<ConfigVersion> {
        let mut g = self.g();
        let cv = g.configs.get_mut(id).ok_or_else(|| nf("configuration version"))?;
        if cv.status != "pending" {
            return Err(StoreError::Conflict("this configuration version already has its content".into()));
        }
        cv.text = text.to_string();
        cv.status = "uploaded".into();
        Ok(cv.clone())
    }

    async fn get_config_version(&self, id: &str) -> Result<ConfigVersion> {
        self.g().configs.get(id).cloned().ok_or_else(|| nf("configuration version"))
    }

    async fn latest_config(&self, ws: &str) -> Result<Option<ConfigVersion>> {
        let g = self.g();
        Ok(g.configs.values().filter(|c| c.workspace_id == ws && c.status == "uploaded").max_by_key(|c| (c.created_at, c.id.clone())).cloned())
    }

    // ---- variables ----
    async fn set_variable(&self, ws: &str, v: NewVariable) -> Result<Variable> {
        if !valid_variable_key(&v.key) {
            return Err(StoreError::Invalid("variable keys use letters, numbers and underscore and cannot start with a digit".into()));
        }
        if !matches!(v.category.as_str(), "terraform" | "env") {
            return Err(StoreError::Invalid("category must be terraform or env".into()));
        }
        let mut g = self.g();
        if !g.workspaces.contains_key(ws) {
            return Err(nf("workspace"));
        }
        let stored = if v.sensitive { self.sealer.seal_var(ws, &v.category, &v.key, &v.value) } else { v.value.clone() };
        let list = g.variables.entry(ws.to_string()).or_default();
        let existing = list.iter_mut().find(|x| x.category == v.category && x.key == v.key);
        let var = match existing {
            Some(x) => {
                x.hcl = v.hcl;
                x.sensitive = v.sensitive;
                x.value = Some(stored);
                x.description = v.description;
                x.clone()
            }
            None => {
                let x = Variable {
                    id: new_id(),
                    workspace_id: ws.to_string(),
                    key: v.key,
                    category: v.category,
                    hcl: v.hcl,
                    sensitive: v.sensitive,
                    value: Some(stored),
                    description: v.description,
                };
                list.push(x.clone());
                x
            }
        };
        Ok(redact(var))
    }

    async fn list_variables(&self, ws: &str) -> Result<Vec<Variable>> {
        let mut v: Vec<_> = self.g().variables.get(ws).cloned().unwrap_or_default().into_iter().map(redact).collect();
        v.sort_by(|a, b| (&a.category, &a.key).cmp(&(&b.category, &b.key)));
        Ok(v)
    }

    async fn delete_variable(&self, ws: &str, id: &str) -> Result<()> {
        let mut g = self.g();
        let list = g.variables.get_mut(ws).ok_or_else(|| nf("variable"))?;
        let before = list.len();
        list.retain(|v| v.id != id);
        if list.len() == before {
            return Err(nf("variable"));
        }
        Ok(())
    }

    async fn variables_for_run(&self, ws: &str) -> Result<Vec<Variable>> {
        let list = self.g().variables.get(ws).cloned().unwrap_or_default();
        list.into_iter()
            .map(|mut v| {
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
        let mut g = self.g();
        if !g.workspaces.contains_key(ws) {
            return Err(nf("workspace"));
        }
        match g.configs.get(config_version) {
            Some(c) if c.workspace_id == ws && c.status == "uploaded" => {}
            Some(c) if c.workspace_id == ws => return Err(StoreError::Invalid("the configuration version has no uploaded content yet".into())),
            _ => return Err(StoreError::NotFound("configuration version not found in this workspace".into())),
        }
        let t = now();
        let run = Run {
            id: new_id(),
            workspace_id: ws.to_string(),
            config_version_id: config_version.to_string(),
            status: RunStatus::Pending,
            operation,
            source: source.to_string(),
            message: message.to_string(),
            auto_apply,
            summary: None,
            changes: Vec::new(),
            input_state_serial: None,
            status_timestamps: [("pending".to_string(), t)].into_iter().collect(),
            lint: Vec::new(),
            skip_refresh: opts.skip_refresh,
            refresh_only: opts.refresh_only,
            created_at: t,
            updated_at: t,
        };
        g.runs.insert(run.id.clone(), run.clone());
        g.run_order.push(run.id.clone());
        if let Some(w) = g.workspaces.get_mut(ws) {
            w.current_run = Some(run.id.clone());
        }
        Ok(run)
    }

    async fn get_run(&self, id: &str) -> Result<Run> {
        self.g().runs.get(id).cloned().ok_or_else(|| nf("run"))
    }

    async fn list_runs(&self, ws: Option<&str>) -> Result<Vec<Run>> {
        let g = self.g();
        Ok(g.run_order
            .iter()
            .rev()
            .filter_map(|id| g.runs.get(id))
            .filter(|r| ws.map(|w| r.workspace_id == w).unwrap_or(true))
            .cloned()
            .collect())
    }

    async fn list_runs_in_org(&self, org: &str) -> Result<Vec<Run>> {
        let g = self.g();
        Ok(g.run_order
            .iter()
            .rev()
            .filter_map(|id| g.runs.get(id))
            .filter(|r| g.workspaces.get(&r.workspace_id).map(|w| w.org == org).unwrap_or(false))
            .cloned()
            .collect())
    }

    async fn apply_event(&self, run: &str, event: Event) -> Result<Run> {
        transition(&mut self.g(), run, event)
    }

    async fn set_plan(&self, run: &str, summary: PlanSummary, changes: Vec<ResourceChange>, blob: Vec<u8>) -> Result<Run> {
        let mut g = self.g();
        let r = g.runs.get_mut(run).ok_or_else(|| nf("run"))?;
        r.summary = Some(summary);
        r.changes = changes;
        let out = r.clone();
        g.plan_blobs.insert(run.to_string(), self.sealer.seal_plan(run, &blob));
        Ok(out)
    }

    async fn record_drift(&self, ws: &str, status: DriftStatus) -> Result<()> {
        self.g().workspaces.get_mut(ws).ok_or_else(|| nf("workspace"))?.drift = Some(status);
        Ok(())
    }

    async fn due_drift_checks(&self) -> Result<Vec<Workspace>> {
        let g = self.g();
        let t = now();
        let mut due: Vec<Workspace> = g
            .workspaces
            .values()
            .filter(|w| {
                let Some(every) = w.drift_interval else { return false };
                w.current_state_version.is_some()
                    && w.drift.as_ref().map(|d| d.checked_at.saturating_add(every) <= t).unwrap_or(true)
                    && !g.runs.values().any(|r| r.workspace_id == w.id && r.refresh_only && matches!(r.status, RunStatus::Pending | RunStatus::Planning))
            })
            .cloned()
            .collect();
        due.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(due)
    }

    async fn set_lint(&self, run: &str, findings: Vec<LintFinding>) -> Result<()> {
        self.g().runs.get_mut(run).ok_or_else(|| nf("run"))?.lint = findings;
        Ok(())
    }

    async fn get_plan_blob(&self, run: &str) -> Result<Vec<u8>> {
        let sealed = self.g().plan_blobs.get(run).cloned().ok_or_else(|| StoreError::NotFound("no saved plan for this run".into()))?;
        self.sealer.open_plan(run, &sealed)
    }

    async fn claim_next(&self) -> Result<Option<Claim>> {
        let mut g = self.g();
        expire_locks(&mut g);
        let order = g.run_order.clone();

        // Confirmed applies first: they already hold their workspace lock.
        if let Some(id) = order.iter().find(|id| g.runs[*id].status == RunStatus::ApplyQueued) {
            let run = transition(&mut g, id, Event::ApplyStart)?;
            return Ok(Some(Claim::Apply(run)));
        }

        // Then the oldest pending run per workspace, if that workspace is free.
        let mut seen_ws: HashSet<Id> = HashSet::new();
        for id in order {
            let (status, op, ws_id) = {
                let r = &g.runs[&id];
                (r.status, r.operation, r.workspace_id.clone())
            };
            if status != RunStatus::Pending {
                continue;
            }
            let serial = g.states.get(&ws_id).and_then(|v| v.last()).map(|s| s.serial);
            if op == Operation::PlanOnly {
                // Speculative plans never take the lock.
                let mut run = transition(&mut g, &id, Event::Start)?;
                if let Some(r) = g.runs.get_mut(&id) {
                    r.input_state_serial = serial;
                }
                run.input_state_serial = serial;
                return Ok(Some(Claim::Plan(run)));
            }
            if !seen_ws.insert(ws_id.clone()) {
                continue; // an older run on this workspace goes first
            }
            let free = g.workspaces.get(&ws_id).map(|w| w.lock.is_none()).unwrap_or(false);
            // belt and braces: never start a second lock-holding run next to an active one
            let busy = g.runs.values().any(|r| r.workspace_id == ws_id && r.operation != Operation::PlanOnly && r.status.holds_lock());
            if !free || busy {
                continue;
            }
            if let Some(w) = g.workspaces.get_mut(&ws_id) {
                w.lock = Some(Lock { owner: run_lock_owner(&id), reason: "run in progress".into(), since: now(), info: None, expires_at: None });
            }
            let mut run = transition(&mut g, &id, Event::Start)?;
            if let Some(r) = g.runs.get_mut(&id) {
                r.input_state_serial = serial;
            }
            run.input_state_serial = serial;
            return Ok(Some(Claim::Plan(run)));
        }
        Ok(None)
    }

    async fn recover_interrupted(&self) -> Result<usize> {
        let mut g = self.g();
        let stuck: Vec<(Id, RunStatus)> = g
            .run_order
            .iter()
            .filter_map(|id| g.runs.get(id).map(|r| (id.clone(), r.status)))
            .filter(|(_, s)| matches!(s, RunStatus::Planning | RunStatus::Applying))
            .collect();
        for (id, status) in &stuck {
            let ev = if *status == RunStatus::Planning { Event::PlanFail } else { Event::ApplyFail };
            transition(&mut g, id, ev)?;
            g.logs.entry(id.clone()).or_default().push("Error: interrupted: the server stopped while this run was working. Queue a new run.".into());
        }
        Ok(stuck.len())
    }

    async fn append_log(&self, run: &str, line: &str) -> Result<()> {
        self.g().logs.entry(run.to_string()).or_default().push(line.to_string());
        Ok(())
    }

    async fn get_log(&self, run: &str) -> Result<Vec<String>> {
        Ok(self.g().logs.get(run).cloned().unwrap_or_default())
    }

    async fn get_log_from(&self, run: &str, from: usize) -> Result<Vec<String>> {
        Ok(self.g().logs.get(run).map(|v| v.iter().skip(from).cloned().collect()).unwrap_or_default())
    }

    // ---- state ----
    async fn current_state(&self, ws: &str) -> Result<Option<StateVersion>> {
        let sv = self.g().states.get(ws).and_then(|v| v.last()).cloned();
        sv.map(|s| self.open(s)).transpose()
    }

    async fn get_state_version(&self, id: &str) -> Result<StateVersion> {
        let found = self.g().states.values().flatten().find(|s| s.id == id).cloned();
        self.open(found.ok_or_else(|| nf("state version"))?)
    }

    async fn list_states(&self, ws: &str) -> Result<Vec<StateVersion>> {
        let mut v = self.g().states.get(ws).cloned().unwrap_or_default();
        v.reverse();
        v.into_iter().map(|s| self.open(s)).collect()
    }

    async fn finalize_state(&self, ws: &str, run: &str, content: &str, resources: usize) -> Result<StateVersion> {
        let mut g = self.g();
        if !g.workspaces.contains_key(ws) {
            return Err(nf("workspace"));
        }
        let sealed = self.sealer.seal_state(ws, content);
        let list = g.states.entry(ws.to_string()).or_default();
        let serial = list.last().map(|s| s.serial + 1).unwrap_or(1);
        let sv = StateVersion {
            id: new_id(),
            workspace_id: ws.to_string(),
            run_id: if run.is_empty() { None } else { Some(run.to_string()) },
            serial,
            status: "finalized".into(),
            content: sealed,
            resource_count: resources,
            created_at: now(),
        };
        list.push(sv.clone());
        if let Some(w) = g.workspaces.get_mut(ws) {
            w.current_state_version = Some(sv.id.clone());
            w.resource_count = resources;
        }
        self.open(sv)
    }
}

impl MemoryStore {
    fn open(&self, mut s: StateVersion) -> Result<StateVersion> {
        s.content = self.sealer.open_state(&s.workspace_id, &s.content)?;
        Ok(s)
    }
}

fn redact(mut v: Variable) -> Variable {
    if v.sensitive {
        v.value = None;
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    async fn make() -> Option<Arc<dyn Store>> {
        Some(Arc::new(MemoryStore::new()))
    }

    crate::all_contract_tests!(make);

    #[tokio::test]
    async fn nothing_sensitive_rests_in_plaintext() {
        let s = MemoryStore::new();
        let u = s.create_user("a@example.com", "h", "A").await.unwrap();
        let o = s.create_org("acme", &u.id).await.unwrap();
        let ws = s.create_workspace(&o.id, "prod").await.unwrap();
        let cv = s.create_config_version(&ws.id, "cfg", "t").await.unwrap();
        let run = s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "", false, "t").await.unwrap();

        s.finalize_state(&ws.id, &run.id, "{\"password\":\"hunter2\"}", 1).await.unwrap();
        s.set_plan(&run.id, PlanSummary::default(), vec![], b"plan-with-hunter2".to_vec()).await.unwrap();
        s.set_variable(
            &ws.id,
            NewVariable { key: "TOKEN".into(), category: "env".into(), hcl: false, sensitive: true, value: "hunter2".into(), description: String::new() },
        )
        .await
        .unwrap();

        for raw in [
            s.raw_state_content(&ws.id).unwrap(),
            s.raw_plan_blob(&run.id).unwrap(),
            s.raw_variable_value(&ws.id, "TOKEN").unwrap(),
        ] {
            assert!(raw.starts_with("enc:v1:") && !raw.contains("hunter2"), "stored form must be sealed: {raw}");
        }
    }

    #[tokio::test]
    async fn state_sealed_for_one_workspace_cannot_be_read_as_another() {
        let a = MemoryStore::with_crypto(Crypto::from_hex(&"11".repeat(32)).unwrap());
        let b = MemoryStore::with_crypto(Crypto::from_hex(&"22".repeat(32)).unwrap());
        let (ua, ub) = (a.create_user("a@example.com", "h", "A").await.unwrap(), b.create_user("a@example.com", "h", "A").await.unwrap());
        let (oa, ob) = (a.create_org("acme", &ua.id).await.unwrap(), b.create_org("acme", &ub.id).await.unwrap());
        let (wa, wb) = (a.create_workspace(&oa.id, "w").await.unwrap(), b.create_workspace(&ob.id, "w").await.unwrap());
        a.finalize_state(&wa.id, "r", "secret", 0).await.unwrap();
        // move the sealed blob into another store (other key, other workspace id)
        let stolen = a.raw_state_content(&wa.id).unwrap();
        {
            let mut g = b.g();
            g.states.entry(wb.id.clone()).or_default().push(StateVersion {
                id: "x".into(),
                workspace_id: wb.id.clone(),
                run_id: None,
                serial: 1,
                status: "finalized".into(),
                content: stolen,
                resource_count: 0,
                created_at: 0,
            });
        }
        assert!(matches!(b.current_state(&wb.id).await, Err(StoreError::Internal(_))), "wrong key or workspace must not decrypt");
    }
}
