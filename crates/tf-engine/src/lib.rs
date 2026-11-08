//! The engine boundary. Phase 1 can drive the `tofu` binary (`TofuEngine`) or a
//! deterministic simulator (`FakeEngine`) that needs nothing installed. A native
//! Rust engine replaces both later behind the same trait.

mod fake;
mod sandbox;
mod tofu;

pub use fake::FakeEngine;
pub use sandbox::{Container, Sandbox};
pub use tofu::TofuEngine;

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tf_core::{PlanSummary, ResourceChange};

/// Receives engine output lines, in order.
pub type Log = Arc<dyn Fn(String) + Send + Sync>;

pub fn emit(log: &Log, line: impl Into<String>) {
    (log.as_ref())(line.into());
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    /// The configuration is invalid.
    Config(String),
    /// The engine ran and failed.
    Failed(String),
    /// The engine could not be started.
    Spawn(String),
    /// Stopped on request. For an apply, `partial_state` is whatever the engine had written
    /// down about the resources it finished, when that differs from the state it was given.
    Canceled { partial_state: Option<String> },
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::Config(m) => write!(f, "configuration error: {m}"),
            EngineError::Failed(m) => write!(f, "{m}"),
            EngineError::Spawn(m) => write!(f, "could not start engine: {m}"),
            EngineError::Canceled { .. } => write!(f, "canceled"),
        }
    }
}
impl std::error::Error for EngineError {}

/// A request to stop the step in progress. Cheap to clone; every clone sees the same request.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<CancelInner>);

#[derive(Default)]
struct CancelInner {
    flag: AtomicBool,
    notify: tokio::sync::Notify,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.flag.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    pub fn is_canceled(&self) -> bool {
        self.0.flag.load(Ordering::SeqCst)
    }

    /// Completes once `cancel` has been called (immediately if it already was).
    pub async fn canceled(&self) {
        loop {
            let waiting = self.0.notify.notified();
            tokio::pin!(waiting);
            waiting.as_mut().enable();
            if self.is_canceled() {
                return;
            }
            waiting.await;
        }
    }
}

impl fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CancelToken(canceled: {})", self.is_canceled())
    }
}

/// Completes when the optional token is canceled; never when there is none.
pub(crate) async fn wait_canceled(cancel: Option<&CancelToken>) {
    match cancel {
        Some(c) => c.canceled().await,
        None => std::future::pending().await,
    }
}

/// Variables handed to a run. Never written to disk or logs by the engine layer.
#[derive(Debug, Clone, Default)]
pub struct EngineVars {
    /// Stops the step in progress when canceled.
    pub cancel: Option<CancelToken>,
    /// Input variables (`terraform` category), by name.
    pub terraform: BTreeMap<String, String>,
    /// Environment variables for the engine process.
    pub env: BTreeMap<String, String>,
    /// Plan without refreshing the state first.
    pub skip_refresh: bool,
    /// Only refresh the state and report what drifted; there is nothing to apply afterwards.
    pub refresh_only: bool,
}

pub struct PlanOutput {
    pub summary: PlanSummary,
    pub changes: Vec<ResourceChange>,
    /// Opaque saved plan, handed back to `apply`.
    pub blob: Vec<u8>,
}

pub struct ApplyOutput {
    /// New state content, stored as-is.
    pub state: String,
    pub resource_count: usize,
}

#[async_trait]
pub trait Engine: Send + Sync {
    fn name(&self) -> &'static str;
    async fn plan(&self, config: &str, state: Option<&str>, vars: &EngineVars, log: &Log) -> Result<PlanOutput, EngineError>;
    async fn apply(
        &self,
        config: &str,
        plan: &[u8],
        state: Option<&str>,
        vars: &EngineVars,
        log: &Log,
    ) -> Result<ApplyOutput, EngineError>;
}

/// Number of resources in a state document, for either the simulator format
/// (`{"resources": {addr: hash}}`) or a real state file (`{"resources": [..]}`).
pub fn resource_count(state: &str) -> usize {
    match serde_json::from_str::<serde_json::Value>(state) {
        Ok(v) => match &v["resources"] {
            serde_json::Value::Object(m) => m.len(),
            serde_json::Value::Array(a) => a.len(),
            _ => 0,
        },
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn a_cancel_token_wakes_every_waiter_and_stays_canceled() {
        let t = CancelToken::new();
        assert!(!t.is_canceled());
        let waiters: Vec<_> = (0..3).map(|_| { let t = t.clone(); tokio::spawn(async move { t.canceled().await }) }).collect();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(waiters.iter().all(|w| !w.is_finished()), "nobody wakes before cancel");
        t.cancel();
        for w in waiters {
            tokio::time::timeout(Duration::from_secs(2), w).await.expect("woken").unwrap();
        }
        // already canceled: completes at once
        tokio::time::timeout(Duration::from_millis(200), t.canceled()).await.expect("immediate");
        assert!(t.is_canceled());
    }

    #[tokio::test]
    async fn no_token_never_cancels() {
        assert!(tokio::time::timeout(Duration::from_millis(50), wait_canceled(None)).await.is_err());
    }
}
