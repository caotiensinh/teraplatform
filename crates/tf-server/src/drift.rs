//! Free drift detection: a refresh-only run, on a schedule or on request, compares the recorded
//! state with the real infrastructure and reports what changed outside the platform.
//!
//! A drift check is a speculative run (it takes no lock and cannot be applied), so it never gets in
//! the way of real runs. Its result is kept on the workspace: `clean`, `drifted` (with how many
//! resources) or `error`. It only reports; nothing is changed or "fixed".

use crate::{queue_run_on, AppState};
use std::sync::Arc;
use std::time::Duration;
use tf_core::{now, DriftStatus, Operation, PlanSummary, Run, RunOptions};
use tf_store::{Store, StoreError};

/// Queues a drift check on the workspace's latest configuration.
pub async fn queue_check(store: &Arc<dyn Store>, workspace: &str, source: &str) -> Result<Run, StoreError> {
    let cv = store
        .latest_config(workspace)
        .await?
        .ok_or_else(|| StoreError::Conflict("this workspace has no configuration yet, so there is nothing to check against".into()))?;
    if store.current_state(workspace).await?.is_none() {
        return Err(StoreError::Conflict("this workspace has no state yet, so there is nothing to refresh".into()));
    }
    queue_run_on(store, workspace, &cv.id, "drift check", Operation::PlanOnly, false, source, RunOptions { skip_refresh: false, refresh_only: true, vcs: None }).await
}

/// Writes the outcome of a finished drift check onto its workspace. `None`: the check failed.
pub async fn record(store: &Arc<dyn Store>, run: &Run, summary: Option<&PlanSummary>) {
    let (status, changes) = match summary {
        None => ("error", 0),
        Some(s) if s.has_changes() => ("drifted", s.change + s.destroy + s.add),
        Some(_) => ("clean", 0),
    };
    let _ = store.record_drift(&run.workspace_id, DriftStatus { checked_at: now(), run_id: Some(run.id.clone()), status: status.into(), changes }).await;
}

/// Starts the scheduler: every `drift_tick_secs` it queues the checks that are due. A failure to
/// queue one (no configuration, a race) is skipped; the next tick looks again.
pub fn spawn(state: &AppState) -> Option<tokio::task::JoinHandle<()>> {
    let tick = state.cfg.drift_tick_secs;
    if tick == 0 {
        return None;
    }
    let store = state.store.clone();
    Some(tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(tick)).await;
            let Ok(due) = store.due_drift_checks().await else { continue };
            for ws in due {
                if let Err(e) = queue_check(&store, &ws.id, "schedule").await {
                    // Nothing to check against (yet): note it as an error result so it is not retried every tick.
                    if matches!(e, StoreError::Conflict(_)) {
                        let _ = store.record_drift(&ws.id, DriftStatus { checked_at: now(), run_id: None, status: "error".into(), changes: 0 }).await;
                    }
                }
            }
        }
    }))
}
