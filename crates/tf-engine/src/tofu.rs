//! Drives the `tofu` binary in a throwaway directory, inside the process sandbox
//! (`sandbox.rs`). Verified against OpenTofu 1.13.9 (see `tests/real_tofu.rs`).
//!
//! Each run gets its own scratch directory holding the configuration, an override file that
//! pins the state to a local file (a `backend` block in the configuration can never redirect
//! it), the state handed in by the platform, its own `HOME` and temp directory, and nothing
//! else. State, saved plan and logs flow back through the `Engine` trait; the directory is
//! deleted afterwards.

use crate::sandbox::{run, Sandbox};
use crate::{emit, resource_count, ApplyOutput, CancelToken, Engine, EngineError, EngineVars, Log, PlanOutput};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tf_core::{ChangeAction, PlanSummary, ResourceChange};

/// Overrides any `backend` or `cloud` choice in the configuration: the platform owns state.
const BACKEND_OVERRIDE: &str = "terraform {\n  backend \"local\" {\n    path = \"terraform.tfstate\"\n  }\n}\n";

pub struct TofuEngine {
    pub binary: String,
    pub sandbox: Sandbox,
}

impl TofuEngine {
    pub fn new(binary: impl Into<String>) -> Self {
        Self::with_sandbox(binary, Sandbox::default())
    }

    pub fn with_sandbox(binary: impl Into<String>, sandbox: Sandbox) -> Self {
        Self { binary: binary.into(), sandbox }
    }

    /// `tofu version`, to fail fast at startup when the binary is missing or broken.
    pub async fn version(&self) -> Result<String, EngineError> {
        let dir = self.sandbox.make_workdir().await?;
        let nolog: Log = std::sync::Arc::new(|_| {});
        let env = self.run_env(&dir, &EngineVars::default(), &nolog);
        let out = run(&self.sandbox, &self.binary, &["version"], &dir, env, Duration::from_secs(30), &nolog, Some(64 << 10), None).await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
        Ok(out?.lines().next().unwrap_or("").trim().to_string())
    }

    /// Removes scratch directories a crashed server left behind. Returns how many.
    pub fn sweep_stale(&self) -> usize {
        self.sandbox.sweep_containers() + self.sandbox.sweep_stale(Duration::from_secs(24 * 3600))
    }

    async fn prepare(&self, config: &str, state: Option<&str>) -> Result<PathBuf, EngineError> {
        let dir = self.sandbox.make_workdir().await?;
        let io = |e: std::io::Error| EngineError::Failed(format!("cannot prepare the run: {e}"));
        tokio::fs::write(dir.join("main.tf"), config).await.map_err(io)?;
        tokio::fs::write(dir.join("platform_override.tf"), BACKEND_OVERRIDE).await.map_err(io)?;
        if let Some(s) = state {
            tokio::fs::write(dir.join("terraform.tfstate"), s).await.map_err(io)?;
        }
        Ok(dir)
    }

    fn run_env(&self, dir: &Path, vars: &EngineVars, log: &Log) -> Vec<(String, String)> {
        let mut run_env: BTreeMap<String, String> = vars.env.clone();
        for (k, v) in &vars.terraform {
            run_env.insert(format!("TF_VAR_{k}"), v.clone());
        }
        let (env, ignored) = self.sandbox.environment(dir, &run_env);
        if !ignored.is_empty() {
            emit(log, format!("[ignored environment variable names the sandbox reserves: {}]", ignored.join(", ")));
        }
        env
    }

    async fn step(&self, dir: &Path, args: &[&str], env: &[(String, String)], timeout: Duration, log: &Log, capture: Option<usize>, cancel: Option<&CancelToken>) -> Result<String, EngineError> {
        if capture.is_none() {
            emit(log, format!("$ tofu {}", args.join(" ")));
        }
        run(&self.sandbox, &self.binary, args, dir, env.to_vec(), timeout, log, capture, cancel).await
    }
}

/// `(summary, changes)` from `tofu show -json <plan>`. Output-only changes count too:
/// a plan that only adds an output still has to be applied for the output to exist.
fn parse_plan_json(json: &str) -> Result<(PlanSummary, Vec<ResourceChange>), EngineError> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| EngineError::Failed(format!("could not read the plan: {e}")))?;
    let mut summary = PlanSummary::default();
    let mut changes = Vec::new();
    for rc in v["resource_changes"].as_array().cloned().unwrap_or_default() {
        let addr = rc["address"].as_str().unwrap_or("?").to_string();
        let acts: Vec<String> = rc["change"]["actions"].as_array().cloned().unwrap_or_default().iter().filter_map(|a| a.as_str().map(String::from)).collect();
        let acts: Vec<&str> = acts.iter().map(String::as_str).collect();
        let action = match acts.as_slice() {
            ["create"] => ChangeAction::Create,
            ["update"] => ChangeAction::Update,
            ["delete"] => ChangeAction::Delete,
            ["delete", "create"] | ["create", "delete"] => ChangeAction::Replace,
            _ => continue,
        };
        match action {
            ChangeAction::Create => summary.add += 1,
            ChangeAction::Update => summary.change += 1,
            ChangeAction::Delete => summary.destroy += 1,
            ChangeAction::Replace => {
                summary.add += 1;
                summary.destroy += 1
            }
        }
        changes.push(ResourceChange { addr, action });
    }
    if let Some(outs) = v["output_changes"].as_object() {
        summary.outputs = outs.values().filter(|o| o["actions"].as_array().map(|a| a.iter().any(|x| x != "no-op")).unwrap_or(false)).count() as u32;
    }
    Ok((summary, changes))
}

#[async_trait]
impl Engine for TofuEngine {
    fn name(&self) -> &'static str {
        "tofu"
    }

    async fn plan(&self, config: &str, state: Option<&str>, vars: &EngineVars, log: &Log) -> Result<PlanOutput, EngineError> {
        let dir = self.prepare(config, state).await?;
        let env = self.run_env(&dir, vars, log);
        let sb = &self.sandbox;
        let result = async {
            let c = vars.cancel.as_ref();
            self.step(&dir, &["init", "-input=false", "-no-color"], &env, sb.init_timeout, log, None, c).await?;
            self.step(&dir, &["plan", "-input=false", "-no-color", "-out=plan.bin"], &env, sb.plan_timeout, log, None, c).await?;
            let json = self.step(&dir, &["show", "-json", "plan.bin"], &env, sb.plan_timeout, log, Some(sb.max_plan_json_bytes), c).await?;
            let (summary, changes) = parse_plan_json(&json)?;
            let blob = tokio::fs::read(dir.join("plan.bin")).await.map_err(|e| EngineError::Failed(format!("the plan was not saved: {e}")))?;
            Ok(PlanOutput { summary, changes, blob })
        }
        .await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
        result
    }

    async fn apply(&self, config: &str, plan: &[u8], state: Option<&str>, vars: &EngineVars, log: &Log) -> Result<ApplyOutput, EngineError> {
        let state_in = state;
        let dir = self.prepare(config, state).await?;
        let env = self.run_env(&dir, vars, log);
        let sb = &self.sandbox;
        let result = async {
            tokio::fs::write(dir.join("plan.bin"), plan).await.map_err(|e| EngineError::Failed(e.to_string()))?;
            let c = vars.cancel.as_ref();
            self.step(&dir, &["init", "-input=false", "-no-color"], &env, sb.init_timeout, log, None, c).await?;
            let applied = self.step(&dir, &["apply", "-input=false", "-no-color", "plan.bin"], &env, sb.apply_timeout, log, None, c).await;
            let state = tokio::fs::read_to_string(dir.join("terraform.tfstate")).await;
            if let Err(EngineError::Canceled { .. }) = &applied {
                // Stopped part-way: what tofu had already written down is the best record of what
                // exists. Keep it when it is a real state and differs from the one we started with.
                let partial = state.ok().filter(|s| serde_json::from_str::<serde_json::Value>(s).is_ok() && Some(s.as_str()) != state_in);
                return Err(EngineError::Canceled { partial_state: partial });
            }
            applied?;
            let state = state.map_err(|e| EngineError::Failed(format!("tofu left no state: {e}")))?;
            let resource_count = resource_count(&state);
            Ok(ApplyOutput { state, resource_count })
        }
        .await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plan_json_actions_and_outputs() {
        let json = r#"{"resource_changes":[
          {"address":"a.b","change":{"actions":["create"]}},
          {"address":"c.d","change":{"actions":["update"]}},
          {"address":"e.f","change":{"actions":["delete"]}},
          {"address":"g.h","change":{"actions":["delete","create"]}},
          {"address":"i.j","change":{"actions":["no-op"]}}],
          "output_changes":{"new":{"actions":["create"]},"same":{"actions":["no-op"]},"gone":{"actions":["delete"]}}}"#;
        let (s, c) = parse_plan_json(json).unwrap();
        assert_eq!(s, PlanSummary { add: 2, change: 1, destroy: 2, outputs: 2 });
        assert_eq!(c.len(), 4);
        assert_eq!(c[3].action, ChangeAction::Replace);
        assert!(s.has_changes());
    }

    #[test]
    fn an_outputs_only_plan_is_a_change() {
        let (s, c) = parse_plan_json(r#"{"output_changes":{"x":{"actions":["create"]}}}"#).unwrap();
        assert!(c.is_empty() && s.has_changes());
        assert!(!parse_plan_json(r#"{"output_changes":{"x":{"actions":["no-op"]}}}"#).unwrap().0.has_changes());
        assert!(parse_plan_json("not json").is_err());
    }
}
