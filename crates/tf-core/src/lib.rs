//! Domain types and the run state machine. Pure: no IO, no async.

mod auth;
pub use auth::*;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

pub type Id = String;

pub fn new_id() -> Id {
    uuid::Uuid::new_v4().to_string()
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// First 8 characters of an id, for display ("run-8f2a1c3d").
pub fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// Workspace names: letters, numbers, dash, underscore; 1 to 90 characters.
pub fn valid_workspace_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 90 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Pending,
    Planning,
    NeedsConfirmation,
    ApplyQueued,
    Applying,
    Applied,
    PlannedAndFinished,
    Errored,
    Discarded,
    Canceled,
    /// Stopped by an administrator while it was working. Resources may exist that the run had
    /// not finished recording; whatever state tofu had written is kept.
    ForceCanceled,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Pending => "pending",
            RunStatus::Planning => "planning",
            RunStatus::NeedsConfirmation => "needs_confirmation",
            RunStatus::ApplyQueued => "apply_queued",
            RunStatus::Applying => "applying",
            RunStatus::Applied => "applied",
            RunStatus::PlannedAndFinished => "planned_and_finished",
            RunStatus::Errored => "errored",
            RunStatus::Discarded => "discarded",
            RunStatus::Canceled => "canceled",
            RunStatus::ForceCanceled => "force_canceled",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RunStatus::Pending => "Pending",
            RunStatus::Planning => "Planning",
            RunStatus::NeedsConfirmation => "Needs confirmation",
            RunStatus::ApplyQueued => "Apply queued",
            RunStatus::Applying => "Applying",
            RunStatus::Applied => "Applied",
            RunStatus::PlannedAndFinished => "Planned and finished",
            RunStatus::Errored => "Errored",
            RunStatus::Discarded => "Discarded",
            RunStatus::Canceled => "Canceled",
            RunStatus::ForceCanceled => "Force canceled",
        }
    }

    pub fn is_final(self) -> bool {
        matches!(
            self,
            RunStatus::Applied
                | RunStatus::PlannedAndFinished
                | RunStatus::Errored
                | RunStatus::Discarded
                | RunStatus::Canceled
                | RunStatus::ForceCanceled
        )
    }

    /// Statuses in which a run holds its workspace's lock.
    pub fn holds_lock(self) -> bool {
        matches!(
            self,
            RunStatus::Planning | RunStatus::NeedsConfirmation | RunStatus::ApplyQueued | RunStatus::Applying
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    PlanAndApply,
    PlanOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Start,
    PlanOk { has_changes: bool, auto_apply: bool, plan_only: bool },
    PlanFail,
    Confirm,
    Discard,
    Cancel,
    /// Administrator stop of a run that is planning, queued to apply, or applying.
    ForceCancel,
    ApplyStart,
    ApplyOk,
    ApplyFail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionError {
    pub from: RunStatus,
    pub event: String,
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot {} a run that is {}", self.event, self.from.label().to_lowercase())
    }
}
impl std::error::Error for TransitionError {}

/// The run state machine. Anything not listed is illegal.
pub fn next(s: RunStatus, e: Event) -> Result<RunStatus, TransitionError> {
    use Event::*;
    use RunStatus::*;
    let to = match (s, e) {
        (Pending, Start) => Planning,
        (Pending, Discard) => Discarded,
        (Pending, Cancel) => Canceled,
        (Planning, PlanOk { has_changes, auto_apply, plan_only }) => {
            if plan_only || !has_changes {
                PlannedAndFinished
            } else if auto_apply {
                ApplyQueued
            } else {
                NeedsConfirmation
            }
        }
        (Planning, PlanFail) => Errored,
        (Planning, Cancel) => Canceled,
        (NeedsConfirmation, Confirm) => ApplyQueued,
        (NeedsConfirmation, Discard) => Discarded,
        (ApplyQueued, ApplyStart) => Applying,
        (ApplyQueued, Cancel) => Canceled,
        (Planning | ApplyQueued | Applying, ForceCancel) => ForceCanceled,
        (Applying, ApplyOk) => Applied,
        (Applying, ApplyFail) => Errored,
        _ => {
            return Err(TransitionError { from: s, event: event_name(e).to_string() });
        }
    };
    Ok(to)
}

fn event_name(e: Event) -> &'static str {
    match e {
        Event::Start => "start",
        Event::PlanOk { .. } => "finish planning",
        Event::PlanFail => "fail planning",
        Event::Confirm => "confirm",
        Event::Discard => "discard",
        Event::Cancel => "cancel",
        Event::ForceCancel => "force-cancel",
        Event::ApplyStart => "start applying",
        Event::ApplyOk => "finish applying",
        Event::ApplyFail => "fail applying",
    }
}

/// What a user may do to a run right now.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Actions {
    pub is_confirmable: bool,
    pub is_discardable: bool,
    pub is_cancelable: bool,
    pub is_force_cancelable: bool,
}

pub fn actions(s: RunStatus) -> Actions {
    Actions {
        is_confirmable: next(s, Event::Confirm).is_ok(),
        is_discardable: next(s, Event::Discard).is_ok(),
        is_cancelable: next(s, Event::Cancel).is_ok(),
        is_force_cancelable: next(s, Event::ForceCancel).is_ok(),
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSummary {
    pub add: u32,
    pub change: u32,
    pub destroy: u32,
    /// Output values that change. A plan that only changes outputs still has to be applied.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub outputs: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl PlanSummary {
    pub fn has_changes(&self) -> bool {
        self.add + self.change + self.destroy + self.outputs > 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeAction {
    Create,
    Update,
    Delete,
    Replace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceChange {
    pub addr: String,
    pub action: ChangeAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lock {
    /// "user" for a manual lock, "run:<id>" when a run holds it, "cli:<id>" for a lock
    /// taken by the OpenTofu http backend (the id is the CLI's lock id).
    pub owner: String,
    pub reason: String,
    pub since: u64,
    /// The CLI's own lock description (JSON), returned verbatim to other CLIs on conflict.
    #[serde(default)]
    pub info: Option<String>,
    /// When the lock lapses by itself unless its holder keeps renewing it. `None`: never (a run's lock, or
    /// a person's deliberate lock).
    #[serde(default)]
    pub expires_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: Id,
    pub org: String,
    pub name: String,
    pub lock: Option<Lock>,
    pub current_run: Option<Id>,
    pub current_state_version: Option<Id>,
    pub resource_count: usize,
    pub description: String,
    /// New runs apply by themselves after a clean plan unless the request says otherwise.
    pub auto_apply: bool,
    pub created_at: u64,
}

/// Fields of a workspace that can be changed after it exists. `None` leaves a field alone.
#[derive(Debug, Clone, Default)]
pub struct WorkspacePatch {
    pub name: Option<String>,
    pub description: Option<String>,
    pub auto_apply: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigVersion {
    pub id: Id,
    pub workspace_id: Id,
    pub source: String,
    pub text: String,
    /// "pending" until its archive is uploaded, then "uploaded". Only an uploaded one can be run.
    pub status: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: Id,
    pub workspace_id: Id,
    pub config_version_id: Id,
    pub status: RunStatus,
    pub operation: Operation,
    pub source: String,
    pub message: String,
    pub auto_apply: bool,
    pub summary: Option<PlanSummary>,
    pub changes: Vec<ResourceChange>,
    pub input_state_serial: Option<u64>,
    pub status_timestamps: BTreeMap<String, u64>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateVersion {
    pub id: Id,
    pub workspace_id: Id,
    pub run_id: Option<Id>,
    pub serial: u64,
    pub status: String,
    pub content: String,
    pub resource_count: usize,
    pub created_at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [RunStatus; 11] = [
        RunStatus::Pending,
        RunStatus::Planning,
        RunStatus::NeedsConfirmation,
        RunStatus::ApplyQueued,
        RunStatus::Applying,
        RunStatus::Applied,
        RunStatus::PlannedAndFinished,
        RunStatus::Errored,
        RunStatus::Discarded,
        RunStatus::Canceled,
        RunStatus::ForceCanceled,
    ];
    const EVENTS: [Event; 10] = [
        Event::Start,
        Event::PlanOk { has_changes: true, auto_apply: false, plan_only: false },
        Event::PlanFail,
        Event::Confirm,
        Event::Discard,
        Event::Cancel,
        Event::ForceCancel,
        Event::ApplyStart,
        Event::ApplyOk,
        Event::ApplyFail,
    ];

    #[test]
    fn happy_path() {
        let s = next(RunStatus::Pending, Event::Start).unwrap();
        assert_eq!(s, RunStatus::Planning);
        let s = next(s, Event::PlanOk { has_changes: true, auto_apply: false, plan_only: false }).unwrap();
        assert_eq!(s, RunStatus::NeedsConfirmation);
        let s = next(s, Event::Confirm).unwrap();
        assert_eq!(s, RunStatus::ApplyQueued);
        let s = next(s, Event::ApplyStart).unwrap();
        assert_eq!(s, RunStatus::Applying);
        assert_eq!(next(s, Event::ApplyOk).unwrap(), RunStatus::Applied);
    }

    #[test]
    fn plan_only_and_no_change_runs_finish_without_apply() {
        let p = Event::PlanOk { has_changes: true, auto_apply: true, plan_only: true };
        assert_eq!(next(RunStatus::Planning, p).unwrap(), RunStatus::PlannedAndFinished);
        let n = Event::PlanOk { has_changes: false, auto_apply: false, plan_only: false };
        assert_eq!(next(RunStatus::Planning, n).unwrap(), RunStatus::PlannedAndFinished);
    }

    #[test]
    fn auto_apply_skips_confirmation() {
        let e = Event::PlanOk { has_changes: true, auto_apply: true, plan_only: false };
        assert_eq!(next(RunStatus::Planning, e).unwrap(), RunStatus::ApplyQueued);
    }

    #[test]
    fn final_states_reject_every_event() {
        for s in ALL.iter().filter(|s| s.is_final()) {
            for e in EVENTS {
                assert!(next(*s, e).is_err(), "{s:?} accepted {e:?}");
            }
        }
    }

    #[test]
    fn every_non_final_state_has_an_exit() {
        for s in ALL.iter().filter(|s| !s.is_final()) {
            assert!(EVENTS.iter().any(|e| next(*s, *e).is_ok()), "{s:?} is a dead end");
        }
    }

    #[test]
    fn applying_cannot_be_canceled() {
        assert!(next(RunStatus::Applying, Event::Cancel).is_err());
        assert!(!actions(RunStatus::Applying).is_cancelable);
    }

    #[test]
    fn force_cancel_stops_a_run_that_is_working_and_nothing_else() {
        for s in [RunStatus::Planning, RunStatus::ApplyQueued, RunStatus::Applying] {
            assert_eq!(next(s, Event::ForceCancel).unwrap(), RunStatus::ForceCanceled, "{s:?}");
            assert!(actions(s).is_force_cancelable);
        }
        for s in [RunStatus::Pending, RunStatus::NeedsConfirmation] {
            assert!(next(s, Event::ForceCancel).is_err(), "{s:?}: use cancel or discard");
            assert!(!actions(s).is_force_cancelable);
        }
        assert!(RunStatus::ForceCanceled.is_final() && !RunStatus::ForceCanceled.holds_lock());
        assert_eq!(RunStatus::ForceCanceled.as_str(), "force_canceled");
    }

    #[test]
    fn only_a_paused_run_can_be_confirmed() {
        for s in ALL {
            assert_eq!(actions(s).is_confirmable, s == RunStatus::NeedsConfirmation);
        }
    }

    #[test]
    fn lock_holders() {
        assert!(RunStatus::NeedsConfirmation.holds_lock());
        assert!(!RunStatus::Pending.holds_lock());
        assert!(!RunStatus::Applied.holds_lock());
    }

    #[test]
    fn workspace_names() {
        assert!(valid_workspace_name("prod-network_1"));
        assert!(!valid_workspace_name(""));
        assert!(!valid_workspace_name("has space"));
        assert!(!valid_workspace_name("slash/name"));
        assert!(!valid_workspace_name(&"a".repeat(91)));
        assert!(valid_workspace_name(&"a".repeat(90)));
    }
}
