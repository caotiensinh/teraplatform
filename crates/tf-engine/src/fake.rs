//! A deterministic simulator. It understands only blocks of the form
//!
//! ```text
//! resource "type" "name" {
//!   any = "body"
//! }
//! ```
//!
//! and diffs them against the last state, so plan and apply behave like a real
//! run (create, update, destroy) without any binary or cloud. Two switches help
//! with testing: a line `fail_plan` or `fail_apply` anywhere in the config, and
//! `echo_var NAME` to print a variable (so log scrubbing can be proven).
//! Input variables are substituted for `var.NAME` inside resource bodies.

use crate::{emit, resource_count, wait_canceled, ApplyOutput, Engine, EngineError, EngineVars, Log, PlanOutput};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::time::Duration;
use tf_core::{ChangeAction, PlanSummary, ResourceChange};

pub struct FakeEngine {
    pub delay: Duration,
}

impl FakeEngine {
    pub fn new(delay: Duration) -> Self {
        Self { delay }
    }

    /// A pause point: where a real engine would be waiting on a provider. Returns early, with
    /// `Canceled`, when the run is canceled.
    async fn pause(&self, vars: &EngineVars) -> Result<(), EngineError> {
        let token = vars.cancel.as_ref();
        if token.map(|t| t.is_canceled()).unwrap_or(false) {
            return Err(EngineError::Canceled { partial_state: None });
        }
        if !self.delay.is_zero() {
            tokio::select! {
                _ = tokio::time::sleep(self.delay) => {}
                _ = wait_canceled(token) => return Err(EngineError::Canceled { partial_state: None }),
            }
        }
        Ok(())
    }
}

/// FNV-1a, stable across builds (unlike `DefaultHasher`).
fn fnv(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn parse_config(cfg: &str, vars: &EngineVars) -> Result<BTreeMap<String, String>, EngineError> {
    let mut out = BTreeMap::new();
    let mut names: Vec<&String> = vars.terraform.keys().collect();
    names.sort_by_key(|k| std::cmp::Reverse(k.len()));
    let mut cur: Option<(String, Vec<String>)> = None;
    for (i, line) in cfg.lines().enumerate() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("resource ") {
            if cur.is_some() {
                return Err(EngineError::Config(format!("line {}: resource block inside a resource block", i + 1)));
            }
            let parts: Vec<&str> = rest.split('"').collect();
            if parts.len() < 4 || parts[1].is_empty() || parts[3].is_empty() {
                return Err(EngineError::Config(format!("line {}: expected resource \"type\" \"name\" {{", i + 1)));
            }
            cur = Some((format!("{}.{}", parts[1], parts[3]), Vec::new()));
        } else if t == "}" {
            if let Some((addr, body)) = cur.take() {
                if out.insert(addr.clone(), fnv(&body.join("\n"))).is_some() {
                    return Err(EngineError::Config(format!("duplicate resource {addr}")));
                }
            }
        } else if let Some((_, body)) = cur.as_mut() {
            let mut line = t.to_string();
            for k in &names {
                line = line.replace(&format!("var.{k}"), &vars.terraform[*k]);
            }
            body.push(line);
        }
    }
    if let Some((addr, _)) = cur {
        return Err(EngineError::Config(format!("resource {addr} is missing its closing brace")));
    }
    Ok(out)
}

fn parse_state(state: Option<&str>) -> BTreeMap<String, String> {
    state
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| v.get("resources").cloned())
        .and_then(|r| serde_json::from_value::<BTreeMap<String, String>>(r).ok())
        .unwrap_or_default()
}

fn state_json(resources: &BTreeMap<String, String>) -> String {
    serde_json::json!({ "version": 1, "resources": resources }).to_string()
}

#[async_trait]
impl Engine for FakeEngine {
    fn name(&self) -> &'static str {
        "simulated"
    }

    async fn plan(&self, config: &str, state: Option<&str>, vars: &EngineVars, log: &Log) -> Result<PlanOutput, EngineError> {
        emit(log, "Initializing the engine (simulated)...");
        self.pause(vars).await?;
        let desired = parse_config(config, vars)?;
        for l in config.lines() {
            if let Some(name) = l.trim().strip_prefix("echo_var ") {
                let v = vars.terraform.get(name.trim()).or_else(|| vars.env.get(name.trim()));
                emit(log, format!("{} = {}", name.trim(), v.map(String::as_str).unwrap_or("(unset)")));
            }
        }
        if config.lines().any(|l| l.trim() == "fail_plan") {
            emit(log, "Error: plan failed on request (fail_plan)");
            return Err(EngineError::Failed("plan failed on request".into()));
        }
        let current = parse_state(state);
        emit(log, format!("Refreshing state... [{} resources]", current.len()));
        self.pause(vars).await?;

        let mut changes = Vec::new();
        for (addr, h) in &desired {
            match current.get(addr) {
                None => changes.push(ResourceChange { addr: addr.clone(), action: ChangeAction::Create }),
                Some(old) if old != h => changes.push(ResourceChange { addr: addr.clone(), action: ChangeAction::Update }),
                _ => {}
            }
        }
        for addr in current.keys() {
            if !desired.contains_key(addr) {
                changes.push(ResourceChange { addr: addr.clone(), action: ChangeAction::Delete });
            }
        }
        let mut summary = PlanSummary::default();
        for c in &changes {
            match c.action {
                ChangeAction::Create => summary.add += 1,
                ChangeAction::Update => summary.change += 1,
                ChangeAction::Delete => summary.destroy += 1,
                ChangeAction::Replace => {
                    summary.add += 1;
                    summary.destroy += 1
                }
            }
            let sym = match c.action {
                ChangeAction::Create => "+",
                ChangeAction::Update => "~",
                ChangeAction::Delete => "-",
                ChangeAction::Replace => "-/+",
            };
            emit(log, format!("{sym} {}", c.addr));
            self.pause(vars).await?;
        }
        if summary.has_changes() {
            emit(
                log,
                format!("Plan: {} to add, {} to change, {} to destroy.", summary.add, summary.change, summary.destroy),
            );
        } else {
            emit(log, "No changes. Infrastructure matches the configuration.");
        }
        Ok(PlanOutput { summary, changes, blob: state_json(&desired).into_bytes() })
    }

    async fn apply(
        &self,
        config: &str,
        plan: &[u8],
        state: Option<&str>,
        vars: &EngineVars,
        log: &Log,
    ) -> Result<ApplyOutput, EngineError> {
        if config.lines().any(|l| l.trim() == "fail_apply") {
            emit(log, "Error: apply failed on request (fail_apply)");
            return Err(EngineError::Failed("apply failed on request".into()));
        }
        let planned = String::from_utf8_lossy(plan).to_string();
        let desired = parse_state(Some(&planned));
        let current = parse_state(state);
        // What the engine has recorded so far: a resource counts once it is finished.
        let mut recorded = current.clone();
        let stopped = |recorded: &BTreeMap<String, String>| {
            EngineError::Canceled { partial_state: if *recorded != current { Some(state_json(recorded)) } else { None } }
        };
        let (mut added, mut changed, mut destroyed) = (0, 0, 0);
        for (addr, h) in &desired {
            match current.get(addr) {
                None => {
                    emit(log, format!("{addr}: Creating..."));
                    added += 1
                }
                Some(old) if old != h => {
                    emit(log, format!("{addr}: Modifying..."));
                    changed += 1
                }
                _ => continue,
            }
            if self.pause(vars).await.is_err() {
                emit(log, "[stopped: the run was canceled]");
                return Err(stopped(&recorded));
            }
            recorded.insert(addr.clone(), h.clone());
            emit(log, format!("{addr}: Done"));
        }
        for addr in current.keys().filter(|a| !desired.contains_key(*a)) {
            emit(log, format!("{addr}: Destroying..."));
            if self.pause(vars).await.is_err() {
                emit(log, "[stopped: the run was canceled]");
                return Err(stopped(&recorded));
            }
            recorded.remove(addr);
            emit(log, format!("{addr}: Destruction complete"));
            destroyed += 1;
        }
        emit(log, format!("Apply complete! Resources: {added} added, {changed} changed, {destroyed} destroyed."));
        let state = state_json(&desired);
        let resource_count = resource_count(&state);
        Ok(ApplyOutput { state, resource_count })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn sink() -> (Log, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let l = lines.clone();
        (Arc::new(move |s| l.lock().unwrap().push(s)), lines)
    }

    fn none() -> EngineVars {
        EngineVars::default()
    }

    const A: &str = "resource \"aws_s3_bucket\" \"logs\" {\n  bucket = \"x\"\n}\n";

    #[tokio::test]
    async fn first_plan_creates_everything() {
        let e = FakeEngine::new(Duration::ZERO);
        let (log, lines) = sink();
        let cfg = format!("{A}resource \"aws_iam_role\" \"ci\" {{\n  name = \"ci\"\n}}\n");
        let out = e.plan(&cfg, None, &none(), &log).await.unwrap();
        assert_eq!(out.summary, PlanSummary { add: 2, change: 0, destroy: 0, outputs: 0 });
        assert!(lines.lock().unwrap().iter().any(|l| l.starts_with("Plan: 2 to add")));
    }

    #[tokio::test]
    async fn apply_then_replan_is_clean_and_edits_are_detected() {
        let e = FakeEngine::new(Duration::ZERO);
        let (log, _) = sink();
        let p = e.plan(A, None, &none(), &log).await.unwrap();
        let applied = e.apply(A, &p.blob, None, &none(), &log).await.unwrap();
        assert_eq!(applied.resource_count, 1);

        let again = e.plan(A, Some(&applied.state), &none(), &log).await.unwrap();
        assert!(!again.summary.has_changes(), "no-op plan must be empty");

        let edited = "resource \"aws_s3_bucket\" \"logs\" {\n  bucket = \"y\"\n}\n";
        let upd = e.plan(edited, Some(&applied.state), &none(), &log).await.unwrap();
        assert_eq!(upd.summary, PlanSummary { add: 0, change: 1, destroy: 0, outputs: 0 });

        let del = e.plan("", Some(&applied.state), &none(), &log).await.unwrap();
        assert_eq!(del.summary, PlanSummary { add: 0, change: 0, destroy: 1, outputs: 0 });
        assert_eq!(del.changes[0].action, ChangeAction::Delete);
    }

    #[tokio::test]
    async fn bad_configs_are_rejected() {
        let e = FakeEngine::new(Duration::ZERO);
        let (log, _) = sink();
        let dup = format!("{A}{A}");
        assert!(matches!(e.plan(&dup, None, &none(), &log).await, Err(EngineError::Config(_))));
        let open = "resource \"a\" \"b\" {\n  x = 1\n";
        assert!(matches!(e.plan(open, None, &none(), &log).await, Err(EngineError::Config(_))));
        let bad = "resource nope {\n}\n";
        assert!(matches!(e.plan(bad, None, &none(), &log).await, Err(EngineError::Config(_))));
    }

    #[tokio::test]
    async fn fail_switches() {
        let e = FakeEngine::new(Duration::ZERO);
        let (log, _) = sink();
        let cfg = format!("fail_plan\n{A}");
        assert!(matches!(e.plan(&cfg, None, &none(), &log).await, Err(EngineError::Failed(_))));
        let cfg = format!("fail_apply\n{A}");
        let p = e.plan(&cfg, None, &none(), &log).await.unwrap();
        assert!(matches!(e.apply(&cfg, &p.blob, None, &none(), &log).await, Err(EngineError::Failed(_))));
    }

    #[tokio::test]
    async fn variables_change_the_plan_and_can_be_echoed() {
        let e = FakeEngine::new(Duration::ZERO);
        let (log, lines) = sink();
        let cfg = "resource \"a\" \"b\" {
  region = var.region
}
echo_var region
";
        let mut v = EngineVars::default();
        v.terraform.insert("region".into(), "eu-west-1".into());
        let p1 = e.plan(cfg, None, &v, &log).await.unwrap();
        let applied = e.apply(cfg, &p1.blob, None, &v, &log).await.unwrap();
        assert!(lines.lock().unwrap().iter().any(|l| l == "region = eu-west-1"));
        assert!(!e.plan(cfg, Some(&applied.state), &v, &log).await.unwrap().summary.has_changes());
        v.terraform.insert("region".into(), "us-east-1".into());
        let p2 = e.plan(cfg, Some(&applied.state), &v, &log).await.unwrap();
        assert_eq!(p2.summary, PlanSummary { add: 0, change: 1, destroy: 0, outputs: 0 }, "changing a variable is a change");
    }

    fn token_vars() -> (EngineVars, crate::CancelToken) {
        let t = crate::CancelToken::new();
        (EngineVars { cancel: Some(t.clone()), ..EngineVars::default() }, t)
    }

    #[tokio::test]
    async fn a_canceled_plan_stops_at_once_and_has_no_state() {
        let e = FakeEngine::new(Duration::from_secs(30));
        let (log, _) = sink();
        let (v, t) = token_vars();
        let started = std::time::Instant::now();
        let task = tokio::spawn(async move { e.plan(A, None, &v, &log).await.map(|_| ()) });
        tokio::time::sleep(Duration::from_millis(100)).await;
        t.cancel();
        let r = tokio::time::timeout(Duration::from_secs(5), task).await.expect("stops quickly").unwrap();
        assert_eq!(r, Err(EngineError::Canceled { partial_state: None }));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_canceled_apply_keeps_what_it_finished() {
        // Three new resources, 400 ms each: cancel after the first two are done.
        let cfg = "resource \"a\" \"one\" {\n  x = 1\n}\nresource \"a\" \"three\" {\n  x = 3\n}\nresource \"a\" \"two\" {\n  x = 2\n}\n";
        let (log, lines) = sink();
        let plan = FakeEngine::new(Duration::ZERO).plan(cfg, None, &none(), &log).await.unwrap();
        let e = std::sync::Arc::new(FakeEngine::new(Duration::from_millis(400)));
        let (v, t) = token_vars();
        let (e2, blob, cfg2, log2) = (e.clone(), plan.blob.clone(), cfg.to_string(), log.clone());
        let task = tokio::spawn(async move { e2.apply(&cfg2, &blob, None, &v, &log2).await.map(|_| ()) });
        tokio::time::sleep(Duration::from_millis(1000)).await;
        t.cancel();
        let r = tokio::time::timeout(Duration::from_secs(5), task).await.expect("stops quickly").unwrap();
        let Err(EngineError::Canceled { partial_state: Some(s) }) = r else { panic!("expected a partial state, got {r:?}") };
        assert_eq!(resource_count(&s), 2, "two finished, one was in flight: {s}");
        assert!(lines.lock().unwrap().iter().any(|l| l.contains("canceled")));

        // canceled before anything finished: nothing to keep
        let (v2, t2) = token_vars();
        t2.cancel();
        let r = FakeEngine::new(Duration::from_millis(400)).apply(cfg, &plan.blob, None, &v2, &log).await.map(|_| ());
        assert_eq!(r, Err(EngineError::Canceled { partial_state: None }));
    }

    #[test]
    fn resource_count_reads_both_formats() {
        assert_eq!(resource_count(r#"{"resources":{"a.b":"1","c.d":"2"}}"#), 2);
        assert_eq!(resource_count(r#"{"resources":[{},{},{}]}"#), 3);
        assert_eq!(resource_count("not json"), 0);
    }
}
