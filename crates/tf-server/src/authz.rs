//! The one place that answers "may this principal do this to that record?".
//! A record in another organization is reported as not found, so existence
//! does not leak; a record in your own organization your role cannot touch is
//! reported as forbidden.

use crate::AppState;
use tf_core::*;
use tf_store::StoreError;

pub async fn ws_access(st: &AppState, p: &Principal, id: &str, action: Action) -> Result<Workspace, StoreError> {
    let ws = st.store.get_workspace(id).await?;
    if ws.org != p.org_id {
        return Err(StoreError::NotFound("workspace not found".into()));
    }
    if !p.can(action) {
        return Err(StoreError::Forbidden("your role does not allow that".into()));
    }
    Ok(ws)
}

pub async fn run_access(st: &AppState, p: &Principal, id: &str, action: Action) -> Result<(Run, Workspace), StoreError> {
    let run = st.store.get_run(id).await?;
    let ws = ws_access(st, p, &run.workspace_id, action).await.map_err(|e| match e {
        StoreError::NotFound(_) => StoreError::NotFound("run not found".into()),
        other => other,
    })?;
    Ok((run, ws))
}
