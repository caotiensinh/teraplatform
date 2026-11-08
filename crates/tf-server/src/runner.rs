//! Executes claimed runs. Every step ends in a state-machine event, so a run
//! that was canceled or discarded while the engine worked is left alone.
//! Variable values reach the engine only as environment; sensitive values are
//! masked in every log line the runner stores.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tf_core::{Event, Operation, Run, RunStatus};
use tf_engine::{CancelToken, Engine, EngineError, EngineVars, Log};
use tf_store::{Claim, Store};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// The runs whose engine step is in progress, so a cancel can reach the step itself and not
/// only the run's status.
#[derive(Clone, Default)]
pub struct Cancels(Arc<Mutex<HashMap<String, (CancelToken, CancelToken)>>>);

impl Cancels {
    pub fn new() -> Self {
        Self::default()
    }

    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, (CancelToken, CancelToken)>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Called by the runner when it starts a step; the engine watches the returned token.
    fn register(&self, run: &str) -> CancelToken {
        let token = CancelToken::new();
        self.map().insert(run.to_string(), (token.clone(), CancelToken::new()));
        token
    }

    /// Called by the runner when the step is over, however it ended.
    fn forget(&self, run: &str) {
        if let Some((_, done)) = self.map().remove(run) {
            done.cancel();
        }
    }

    /// Asks the engine step of `run` to stop. False when no step is running.
    pub fn cancel(&self, run: &str) -> bool {
        match self.map().get(run) {
            Some((token, _)) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// Stops the step and waits (up to `limit`) until the runner has finished with it, which
    /// includes keeping any state the engine had already written.
    pub async fn cancel_and_wait(&self, run: &str, limit: Duration) -> bool {
        let done = match self.map().get(run) {
            Some((token, done)) => {
                token.cancel();
                done.clone()
            }
            None => return false,
        };
        tokio::time::timeout(limit, done.canceled()).await.is_ok()
    }

    /// Steps in progress right now.
    pub fn active(&self) -> usize {
        self.map().len()
    }

    /// True while the runner is still working on this run (it may yet write log lines).
    pub fn is_running(&self, run: &str) -> bool {
        self.map().contains_key(run)
    }
}

struct Registered<'a> {
    cancels: &'a Cancels,
    id: String,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        self.cancels.forget(&self.id);
    }
}

/// True while the run is still in the state the step was claimed for. A run canceled between
/// being claimed and starting its step is left alone.
async fn still_running(store: &Arc<dyn Store>, run: &Run, want: RunStatus) -> bool {
    matches!(store.get_run(&run.id).await, Ok(r) if r.status == want)
}

pub fn spawn(store: Arc<dyn Store>, engine: Arc<dyn Engine>, cancels: Cancels) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match store.claim_next().await {
                Ok(Some(claim)) => {
                    let (s, e, c) = (store.clone(), engine.clone(), cancels.clone());
                    tokio::spawn(async move { execute(s, e, c, claim).await });
                }
                Ok(None) => tokio::time::sleep(Duration::from_millis(50)).await,
                Err(e) => {
                    eprintln!("runner: claim failed: {e}");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    })
}

/// Replaces every sensitive value with a marker, longest first so a value that
/// contains another is fully hidden. Empty values are ignored.
pub fn scrub(line: &str, secrets: &[String]) -> String {
    let mut out = line.to_string();
    for s in secrets {
        if !s.is_empty() {
            out = out.replace(s.as_str(), "(sensitive)");
        }
    }
    out
}

/// A log sink that preserves line order by funnelling lines through one task.
fn sink(store: &Arc<dyn Store>, run_id: &str, secrets: Vec<String>) -> (Log, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let log: Log = Arc::new(move |line| {
        let _ = tx.send(line);
    });
    let (s, id) = (store.clone(), run_id.to_string());
    let task = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            let _ = s.append_log(&id, &scrub(&line, &secrets)).await;
        }
    });
    (log, task)
}

pub async fn execute(store: Arc<dyn Store>, engine: Arc<dyn Engine>, cancels: Cancels, claim: Claim) {
    match claim {
        Claim::Plan(run) => plan(store, engine, cancels, run).await,
        Claim::Apply(run) => apply(store, engine, cancels, run).await,
    }
}

struct Inputs {
    config: String,
    state: Option<String>,
    vars: EngineVars,
    secrets: Vec<String>,
}

async fn inputs(store: &Arc<dyn Store>, run: &Run) -> Result<Inputs, String> {
    let cfg = store.get_config_version(&run.config_version_id).await.map_err(|e| e.to_string())?;
    let state = store.current_state(&run.workspace_id).await.map_err(|e| e.to_string())?;
    let mut vars = EngineVars::default();
    let mut secrets = Vec::new();
    for v in store.variables_for_run(&run.workspace_id).await.map_err(|e| e.to_string())? {
        let value = v.value.unwrap_or_default();
        if v.sensitive {
            secrets.push(value.clone());
        }
        if v.category == "env" {
            vars.env.insert(v.key, value);
        } else {
            vars.terraform.insert(v.key, value);
        }
    }
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    Ok(Inputs { config: cfg.text, state: state.map(|s| s.content), vars, secrets })
}

async fn note(store: &Arc<dyn Store>, run: &str, secrets: &[String], line: &str) {
    let _ = store.append_log(run, &scrub(line, secrets)).await;
}

/// Runs the lint rules on the configuration, records the findings with the run and puts a short
/// account in its log. It never changes what happens next: a failing or panicking linter is
/// reported as skipped and the plan goes on.
async fn lint_step(store: &Arc<dyn Store>, run: &Run, config: &str, secrets: &[String]) {
    let text = config.to_string();
    let findings = match tokio::task::spawn_blocking(move || tf_lint::lint(&text)).await {
        Ok(f) => f,
        Err(_) => {
            note(store, &run.id, secrets, "Lint skipped: the linter failed on this configuration. Planning goes on.").await;
            return;
        }
    };
    if findings.is_empty() {
        note(store, &run.id, secrets, "Lint: no findings.").await;
    } else {
        let count = |sev: &str| findings.iter().filter(|f| f.severity == sev).count();
        note(store, &run.id, secrets, &format!("Lint: {} finding(s): {} warning(s), {} info.", findings.len(), count("warning") + count("error"), count("info"))).await;
        for f in &findings {
            let at = match (&f.file, f.line) {
                (Some(file), Some(l)) => format!(" ({file}:{l})"),
                (None, Some(l)) => format!(" (line {l})"),
                _ => String::new(),
            };
            note(store, &run.id, secrets, &format!("  lint {} {}{at}: {}", f.severity, f.rule, f.message)).await;
        }
    }
    let _ = store.set_lint(&run.id, findings).await;
}

async fn plan(store: Arc<dyn Store>, engine: Arc<dyn Engine>, cancels: Cancels, run: Run) {
    // Register first, then look at the status: a cancel either sees the registration or has
    // already changed the status, so no cancel is lost.
    let token = cancels.register(&run.id);
    let _registered = Registered { cancels: &cancels, id: run.id.clone() };
    if !still_running(&store, &run, RunStatus::Planning).await {
        return;
    }
    let inp = match inputs(&store, &run).await {
        Ok(v) => v,
        Err(e) => {
            note(&store, &run.id, &[], &format!("Error: {e}")).await;
            let _ = store.apply_event(&run.id, Event::PlanFail).await;
            return;
        }
    };
    let mut inp = inp;
    inp.vars.cancel = Some(token);
    inp.vars.skip_refresh = run.skip_refresh;
    inp.vars.refresh_only = run.refresh_only;
    lint_step(&store, &run, &inp.config, &inp.secrets).await;
    let (log, task) = sink(&store, &run.id, inp.secrets.clone());
    let result = engine.plan(&inp.config, inp.state.as_deref(), &inp.vars, &log).await;
    drop(log);
    let _ = task.await;
    match result {
        Ok(out) => {
            let has_changes = out.summary.has_changes();
            if run.refresh_only {
                crate::drift::record(&store, &run, Some(&out.summary)).await;
            }
            let _ = store.set_plan(&run.id, out.summary, out.changes, out.blob).await;
            let ev = Event::PlanOk { has_changes, auto_apply: run.auto_apply, plan_only: run.operation == Operation::PlanOnly };
            // Err means the run was canceled meanwhile: nothing more to do.
            let _ = store.apply_event(&run.id, ev).await;
        }
        // The status was already changed by whoever canceled; just say what happened.
        Err(EngineError::Canceled { .. }) => note(&store, &run.id, &inp.secrets, "The run was canceled and its planning step was stopped.").await,
        Err(e) => {
            note(&store, &run.id, &inp.secrets, &format!("Error: {e}")).await;
            if run.refresh_only {
                crate::drift::record(&store, &run, None).await;
            }
            let _ = store.apply_event(&run.id, Event::PlanFail).await;
        }
    }
}

async fn apply(store: Arc<dyn Store>, engine: Arc<dyn Engine>, cancels: Cancels, run: Run) {
    let token = cancels.register(&run.id);
    let _registered = Registered { cancels: &cancels, id: run.id.clone() };
    if !still_running(&store, &run, RunStatus::Applying).await {
        return;
    }
    let prepared = async {
        let inp = inputs(&store, &run).await?;
        let blob = store.get_plan_blob(&run.id).await.map_err(|e| e.to_string())?;
        Ok::<_, String>((inp, blob))
    }
    .await;
    let (inp, blob) = match prepared {
        Ok(v) => v,
        Err(e) => {
            note(&store, &run.id, &[], &format!("Error: {e}")).await;
            let _ = store.apply_event(&run.id, Event::ApplyFail).await;
            return;
        }
    };
    let mut inp = inp;
    inp.vars.cancel = Some(token);
    let (log, task) = sink(&store, &run.id, inp.secrets.clone());
    let result = engine.apply(&inp.config, &blob, inp.state.as_deref(), &inp.vars, &log).await;
    drop(log);
    let _ = task.await;
    match result {
        Ok(out) => {
            // State first, then the run result, so a finished run always has its state.
            match store.finalize_state(&run.workspace_id, &run.id, &out.state, out.resource_count).await {
                Ok(_) => {
                    let _ = store.apply_event(&run.id, Event::ApplyOk).await;
                }
                Err(e) => {
                    note(&store, &run.id, &inp.secrets, &format!("Error: could not store state: {e}")).await;
                    let _ = store.apply_event(&run.id, Event::ApplyFail).await;
                }
            }
        }
        Err(EngineError::Canceled { partial_state }) => {
            // Stopped part-way. Resources the engine had finished exist for real: keep that record
            // (before the stopper releases the lock) so the next plan does not create them again.
            match partial_state {
                Some(s) => match store.finalize_state(&run.workspace_id, &run.id, &s, tf_engine::resource_count(&s)).await {
                    Ok(v) => note(&store, &run.id, &inp.secrets, &format!("The run was force-canceled. The resources that had finished were recorded as state serial {} ({} resource(s)); anything that was in progress may exist without being recorded.", v.serial, v.resource_count)).await,
                    Err(e) => note(&store, &run.id, &inp.secrets, &format!("The run was force-canceled, and the state it had written could not be stored: {e}")).await,
                },
                None => note(&store, &run.id, &inp.secrets, "The run was force-canceled. The engine left no record of progress, so the state is unchanged; a resource that was being created may exist without being recorded.").await,
            }
        }
        Err(e) => {
            note(&store, &run.id, &inp.secrets, &format!("Error: {e}")).await;
            let _ = store.apply_event(&run.id, Event::ApplyFail).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{scrub, Cancels};
    use std::time::Duration;

    #[tokio::test]
    async fn cancels_reach_a_registered_step_and_wait_for_it_to_finish() {
        let c = Cancels::new();
        assert!(!c.cancel("nope"), "no step, nothing to stop");
        assert!(!c.cancel_and_wait("nope", Duration::from_millis(10)).await);
        let token = c.register("r1");
        assert_eq!(c.active(), 1);
        let waiter = { let c = c.clone(); tokio::spawn(async move { c.cancel_and_wait("r1", Duration::from_secs(5)).await }) };
        tokio::time::timeout(Duration::from_secs(2), token.canceled()).await.expect("the step is told to stop");
        assert!(!waiter.is_finished(), "the stopper waits until the runner is done");
        c.forget("r1");
        assert!(tokio::time::timeout(Duration::from_secs(2), waiter).await.expect("released").unwrap());
        assert_eq!(c.active(), 0);
        // a step that never finishes cannot hold the stopper forever
        let _t = c.register("r2");
        assert!(!c.cancel_and_wait("r2", Duration::from_millis(50)).await);
    }

    #[test]
    fn masks_every_occurrence_longest_first() {
        let secrets = vec!["hunter22".to_string(), "hunter2".to_string()];
        assert_eq!(scrub("pw=hunter22 and hunter2", &secrets), "pw=(sensitive) and (sensitive)");
        assert_eq!(scrub("nothing here", &secrets), "nothing here");
        assert_eq!(scrub("keep", &[String::new()]), "keep", "empty secrets are ignored");
    }
}
