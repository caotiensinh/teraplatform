//! The `tofu` driver against a REAL OpenTofu binary. Set `TF_TEST_TOFU_BIN` to its path.
//! Without it every case prints SKIPPED and passes, so check for SKIPPED lines before
//! trusting a green run. The configurations use only the built-in `terraform_data`
//! resource: no provider is downloaded and no cloud is touched.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tf_engine::{Engine, EngineError, EngineVars, Log, TofuEngine};

fn bin() -> Option<String> {
    std::env::var("TF_TEST_TOFU_BIN").ok().filter(|s| !s.is_empty())
}

fn sink() -> (Log, Arc<Mutex<Vec<String>>>) {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let l = lines.clone();
    (Arc::new(move |s| l.lock().unwrap().push(s)), lines)
}

macro_rules! need_tofu {
    () => {
        match bin() {
            Some(b) => b,
            None => {
                eprintln!("SKIPPED: TF_TEST_TOFU_BIN not set");
                return;
            }
        }
    };
}

/// `echo` that works with the shell tofu's local-exec uses on this platform.
fn echo_secrets_command() -> &'static str {
    if cfg!(windows) {
        "echo seen=%PLATFORM_SECRET_CANARY% key=%TF_MASTER_KEY% db=%DATABASE_URL%"
    } else {
        "echo seen=$PLATFORM_SECRET_CANARY key=$TF_MASTER_KEY db=$DATABASE_URL"
    }
}

#[tokio::test]
async fn provisioners_cannot_read_the_servers_secrets() {
    let bin = need_tofu!();
    // pretend to be a server holding secrets in its environment
    std::env::set_var("PLATFORM_SECRET_CANARY", "canary-8f3a91");
    std::env::set_var("TF_MASTER_KEY", "canary-master-77c2");
    std::env::set_var("DATABASE_URL", "postgres://canary-db-5d10");

    let cfg = format!("resource \"terraform_data\" \"x\" {{\n  provisioner \"local-exec\" {{\n    command = \"{}\"\n  }}\n}}\n", echo_secrets_command());
    let e = TofuEngine::new(bin);
    let (log, lines) = sink();
    let plan = e.plan(&cfg, None, &EngineVars::default(), &log).await.expect("plan");
    e.apply(&cfg, &plan.blob, None, &EngineVars::default(), &log).await.expect("apply");

    let out = lines.lock().unwrap().join("\n");
    assert!(out.contains("seen="), "the provisioner ran and printed: {out}");
    for canary in ["canary-8f3a91", "canary-master-77c2", "canary-db-5d10"] {
        assert!(!out.contains(canary), "the server's secret {canary} reached a provisioner:\n{out}");
    }
}

const GREETING: &str = "terraform_data";

fn cfg_with_var(extra: &str) -> String {
    format!(
        "variable \"msg\" {{\n  type = string\n}}\n\nresource \"{GREETING}\" \"a\" {{\n  input = var.msg\n}}\n\nresource \"{GREETING}\" \"b\" {{\n  input = \"static\"\n}}\n{extra}"
    )
}

fn vars(msg: &str) -> EngineVars {
    let mut v = EngineVars::default();
    v.terraform.insert("msg".into(), msg.into());
    v
}

#[tokio::test]
async fn a_full_cycle_works_with_a_real_binary() {
    let bin = need_tofu!();
    let e = TofuEngine::new(bin);
    let (log, lines) = sink();
    let cfg = cfg_with_var("");

    let p1 = e.plan(&cfg, None, &vars("one"), &log).await.expect("first plan");
    assert_eq!((p1.summary.add, p1.summary.change, p1.summary.destroy), (2, 0, 0));
    let applied = e.apply(&cfg, &p1.blob, None, &vars("one"), &log).await.expect("apply");
    assert_eq!(applied.resource_count, 2);
    assert!(applied.state.contains("\"lineage\""), "the state is tofu's own document");

    let again = e.plan(&cfg, Some(&applied.state), &vars("one"), &log).await.unwrap();
    assert!(!again.summary.has_changes(), "a second plan sees nothing to do");

    // a changed input variable is a change of one resource
    let p2 = e.plan(&cfg, Some(&applied.state), &vars("two"), &log).await.unwrap();
    assert_eq!((p2.summary.add, p2.summary.change, p2.summary.destroy), (0, 1, 0));
    let applied2 = e.apply(&cfg, &p2.blob, Some(&applied.state), &vars("two"), &log).await.unwrap();
    assert!(applied2.state.contains("two"));

    // removing everything destroys both
    let p3 = e.plan("", Some(&applied2.state), &EngineVars::default(), &log).await.unwrap();
    assert_eq!((p3.summary.add, p3.summary.change, p3.summary.destroy), (0, 0, 2));
    assert!(lines.lock().unwrap().iter().any(|l| l.contains("Plan: 2 to add")), "tofu's own output is in the log");
}

#[tokio::test]
async fn a_change_that_only_touches_outputs_is_still_a_change() {
    let bin = need_tofu!();
    let e = TofuEngine::new(bin);
    let (log, _) = sink();
    let cfg = cfg_with_var("\noutput \"shown\" {\n  value = var.msg\n}\n");
    let p1 = e.plan(&cfg, None, &vars("one"), &log).await.unwrap();
    let st = e.apply(&cfg, &p1.blob, None, &vars("one"), &log).await.unwrap().state;

    // only the output changes: the resource inputs stay the same
    let cfg2 = format!("{}\noutput \"extra\" {{\n  value = \"new output\"\n}}\n", cfg);
    let p = e.plan(&cfg2, Some(&st), &vars("one"), &log).await.unwrap();
    assert_eq!((p.summary.add, p.summary.change, p.summary.destroy), (0, 0, 0));
    assert!(p.summary.has_changes(), "a new output must not be reported as 'no changes' (it would never be applied)");
    let st2 = e.apply(&cfg2, &p.blob, Some(&st), &vars("one"), &log).await.unwrap().state;
    assert!(st2.contains("new output"), "and applying it records the output in state");
}

#[tokio::test]
async fn a_backend_block_in_the_configuration_cannot_redirect_state() {
    let bin = need_tofu!();
    let e = TofuEngine::new(bin);
    let (log, _) = sink();
    // the run must keep its state in the platform's own store, never wherever the config says
    let cfg = "terraform {\n  backend \"http\" {\n    address = \"http://127.0.0.1:1/unreachable\"\n  }\n}\n\nresource \"terraform_data\" \"x\" {\n  input = \"hi\"\n}\n";
    let p = e.plan(cfg, None, &EngineVars::default(), &log).await.expect("plan must not try to reach the configured backend");
    let a = e.apply(cfg, &p.blob, None, &EngineVars::default(), &log).await.expect("apply");
    assert_eq!(a.resource_count, 1);
}

#[tokio::test]
async fn invalid_configuration_is_reported_with_tofus_own_message() {
    let bin = need_tofu!();
    let e = TofuEngine::new(bin);
    let (log, lines) = sink();
    let r = e.plan("resource \"terraform_data\" \"x\" {\n  nonsense = 1\n}\n", None, &EngineVars::default(), &log).await;
    assert!(matches!(r, Err(EngineError::Failed(_))), "{:?}", r.err());
    let out = lines.lock().unwrap().join("\n");
    assert!(out.contains("nonsense"), "the log explains what is wrong: {out}");
}

#[tokio::test]
async fn a_missing_binary_is_a_clear_error_not_a_panic() {
    let e = TofuEngine::new("definitely-not-a-real-tofu-binary");
    let (log, _) = sink();
    let r = e.plan("", None, &EngineVars::default(), &log).await;
    assert!(matches!(r, Err(EngineError::Spawn(_))), "{:?}", r.err());
}

// ---------------------------------------------------------------------------
// containment: time, output, memory, processes, leftovers
// ---------------------------------------------------------------------------

use tf_engine::Sandbox;

/// A configuration whose only resource runs `command` through local-exec. On Windows the
/// command goes to PowerShell directly (`interpreter`), avoiding cmd's quoting; keep
/// double quotes out of such commands.
fn exec_cfg(command: &str) -> String {
    exec_cfg_with(command, false)
}

fn exec_cfg_with(command: &str, powershell: bool) -> String {
    let escaped = command.replace('\\', "\\\\").replace('"', "\\\"");
    let interpreter = if powershell && cfg!(windows) { "\n    interpreter = [\"powershell\", \"-NoProfile\", \"-Command\"]" } else { "" };
    format!("resource \"terraform_data\" \"x\" {{\n  provisioner \"local-exec\" {{{interpreter}\n    command = \"{escaped}\"\n  }}\n}}\n")
}

fn tmp_root(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("tfclone-test-{tag}-{}", std::process::id()))
}

async fn plan_then_apply(e: &TofuEngine, cfg: &str, vars: &EngineVars, log: &Log) -> Result<usize, EngineError> {
    let p = e.plan(cfg, None, vars, log).await?;
    Ok(e.apply(cfg, &p.blob, None, vars, log).await?.resource_count)
}

/// How many live processes have `marker` on their command line (not counting the checker).
fn running(marker: &str) -> usize {
    #[cfg(windows)]
    {
        let ps = format!("(Get-CimInstance Win32_Process | Where-Object {{ $_.CommandLine -like '*{marker}*' -and $_.Name -notlike 'powershell*' -and $_.Name -notlike 'pwsh*' }} | Measure-Object).Count");
        let out = std::process::Command::new("powershell").args(["-NoProfile", "-Command", &ps]).output().expect("powershell");
        String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
    }
    #[cfg(not(windows))]
    {
        let out = std::process::Command::new("pgrep").args(["-fc", marker]).output().expect("pgrep");
        String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
    }
}

fn sleeper(marker: &str) -> String {
    if cfg!(windows) { format!("ping -n 61 {marker} > nul") } else { format!("sleep 61 # {marker}") }
}

#[tokio::test]
async fn a_runaway_apply_is_stopped_and_so_is_everything_it_started() {
    let bin = need_tofu!();
    let sb = Sandbox { apply_timeout: Duration::from_secs(4), work_root: tmp_root("timeout"), ..Sandbox::default() };
    let e = TofuEngine::with_sandbox(bin, sb);
    let (log, lines) = sink();
    let marker = "127.0.0.77";
    let started = Instant::now();
    let r = plan_then_apply(&e, &exec_cfg(&sleeper(marker)), &EngineVars::default(), &log).await;
    let took = started.elapsed();
    match &r {
        Err(EngineError::Failed(m)) => assert!(m.contains("timed out"), "{m}"),
        other => panic!("expected a timeout, got {:?}", other.as_ref().err()),
    }
    assert!(took < Duration::from_secs(40), "the 61 second sleeper must not be waited for ({took:?})");
    assert!(lines.lock().unwrap().iter().any(|l| l.contains("every process it started was killed")), "the log says what happened");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(running(marker), 0, "the grandchild (the sleeper) must be dead too");
}

#[tokio::test]
async fn background_processes_left_by_a_provisioner_are_killed_when_the_run_ends() {
    let bin = need_tofu!();
    let e = TofuEngine::with_sandbox(bin, Sandbox { work_root: tmp_root("bg"), ..Sandbox::default() });
    let (log, _) = sink();
    let marker = "127.0.0.78";
    // a detached process that outlives the provisioner (no inherited pipes, so tofu itself returns at once)
    let spawn = if cfg!(windows) {
        format!("Start-Process -WindowStyle Hidden -FilePath ping -ArgumentList '-n','61','{marker}'")
    } else {
        format!("nohup sleep 61 >/dev/null 2>&1 </dev/null & echo {marker}")
    };
    let started = Instant::now();
    let n = plan_then_apply(&e, &exec_cfg_with(&spawn, true), &EngineVars::default(), &log).await.expect("the run itself succeeds");
    assert_eq!(n, 1);
    assert!(started.elapsed() < Duration::from_secs(40), "the run must not wait for the background process");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(running(marker), 0, "nothing may outlive the run");
}

#[tokio::test]
async fn output_floods_are_capped_and_the_run_still_finishes() {
    let bin = need_tofu!();
    let sb = Sandbox { max_log_lines: 300, work_root: tmp_root("flood"), ..Sandbox::default() };
    let e = TofuEngine::with_sandbox(bin, sb);
    let (log, lines) = sink();
    let flood = if cfg!(windows) { "for /L %i in (1,1,20000) do @echo flood line %i".to_string() } else { "seq 1 20000 | sed 's/^/flood line /'".to_string() };
    let n = plan_then_apply(&e, &exec_cfg(&flood), &EngineVars::default(), &log).await.expect("a noisy run still completes");
    assert_eq!(n, 1);
    let l = lines.lock().unwrap();
    assert!(l.len() < 400, "{} lines kept: the cap must hold", l.len());
    assert!(l.iter().any(|x| x.contains("output truncated")), "and it says so");
    assert!(l.iter().any(|x| x.contains("flood line 1")), "the start is kept");
}

#[tokio::test]
async fn one_enormous_line_is_cut() {
    let bin = need_tofu!();
    let sb = Sandbox { max_line_bytes: 1000, work_root: tmp_root("line"), ..Sandbox::default() };
    let e = TofuEngine::with_sandbox(bin, sb);
    let (log, lines) = sink();
    let long = if cfg!(windows) { "Write-Output ('x' * 50000)".to_string() } else { "head -c 50000 /dev/zero | tr '\\0' x".to_string() };
    plan_then_apply(&e, &exec_cfg_with(&long, true), &EngineVars::default(), &log).await.expect("apply");
    let l = lines.lock().unwrap();
    let longest = l.iter().map(|x| x.len()).max().unwrap_or(0);
    assert!(longest < 1200, "longest kept line is {longest} bytes");
    assert!(l.iter().any(|x| x.contains("[line cut]")), "a cut line says so");
    assert!(l.iter().any(|x| x.contains("xxxxxxxxxx")), "and the start of it is kept");
}

#[cfg(windows)]
#[tokio::test]
async fn a_memory_hog_hits_the_ceiling_but_the_same_command_runs_without_one() {
    let bin = need_tofu!();
    let hog = exec_cfg_with("$ErrorActionPreference = 'Stop'; $a = New-Object byte[] 1500MB; 'allocated-' + 'ok'", true);

    // control: with no ceiling the command really does allocate, so a failure below means the ceiling
    let free = TofuEngine::with_sandbox(&bin, Sandbox { memory_limit: None, work_root: tmp_root("mem-free"), ..Sandbox::default() });
    let (log, lines) = sink();
    plan_then_apply(&free, &hog, &EngineVars::default(), &log).await.expect("without a ceiling the allocation works");
    assert!(lines.lock().unwrap().iter().any(|l| l.contains("allocated-ok")), "the control command ran");

    let capped = TofuEngine::with_sandbox(&bin, Sandbox { memory_limit: Some(700 << 20), work_root: tmp_root("mem-cap"), ..Sandbox::default() });
    let (log, lines) = sink();
    let r = plan_then_apply(&capped, &hog, &EngineVars::default(), &log).await;
    assert!(r.is_err(), "allocating 1500 MB under a 700 MB ceiling must fail");
    assert!(lines.lock().unwrap().iter().any(|l| l.contains("OutOfMemory")), "and the reason is memory, not something else");
    assert!(!lines.lock().unwrap().iter().any(|l| l.contains("allocated-ok")), "the allocation must not have succeeded");
}

#[cfg(windows)]
#[tokio::test]
async fn a_process_count_ceiling_stops_fork_bombs() {
    let bin = need_tofu!();
    let cmd = "ping -n 1 127.0.0.1 > nul"; // cmd + ping under tofu: three processes in all
    let roomy = TofuEngine::with_sandbox(&bin, Sandbox { max_processes: 64, work_root: tmp_root("proc-a"), ..Sandbox::default() });
    let (log, _) = sink();
    assert!(plan_then_apply(&roomy, &exec_cfg(cmd), &EngineVars::default(), &log).await.is_ok(), "within the ceiling it works");
    let tight = TofuEngine::with_sandbox(&bin, Sandbox { max_processes: 2, work_root: tmp_root("proc-b"), ..Sandbox::default() });
    let r = plan_then_apply(&tight, &exec_cfg(cmd), &EngineVars::default(), &log).await;
    assert!(r.is_err(), "a third process under a ceiling of two must not run");
}

#[tokio::test]
async fn a_run_cannot_move_the_sandbox_home_and_its_scratch_directory_is_removed() {
    let bin = need_tofu!();
    let root = tmp_root("home");
    let e = TofuEngine::with_sandbox(bin, Sandbox { work_root: root.clone(), ..Sandbox::default() });
    let (log, lines) = sink();
    let mut v = EngineVars::default();
    v.env.insert(if cfg!(windows) { "USERPROFILE" } else { "HOME" }.into(), "/evil/elsewhere".into());
    v.env.insert("MY_OWN_VARIABLE".into(), "visible".into());
    let cmd = if cfg!(windows) { "echo home=%USERPROFILE% own=%MY_OWN_VARIABLE%" } else { "echo home=$HOME own=$MY_OWN_VARIABLE" };
    plan_then_apply(&e, &exec_cfg(cmd), &v, &log).await.expect("apply");
    let out = lines.lock().unwrap().join("\n");
    assert!(out.contains("own=visible"), "a run's own variables arrive: {out}");
    assert!(!out.contains("evil"), "but cannot move HOME/USERPROFILE: {out}");
    assert!(out.contains("home=") && out.contains("tfclone-"), "home is inside the run's scratch directory: {out}");
    let left = std::fs::read_dir(&root).map(|d| d.count()).unwrap_or(0);
    assert_eq!(left, 0, "scratch directories are deleted after the run");
    let _ = std::fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------
// canceling a step that is really running
// ---------------------------------------------------------------------------

fn leftovers(root: &std::path::Path) -> usize {
    std::fs::read_dir(root).map(|d| d.flatten().filter(|e| e.file_name().to_string_lossy().starts_with("tfclone-")).count()).unwrap_or(0)
}

#[tokio::test]
async fn canceling_a_running_apply_kills_tofu_and_its_children_and_reports_what_finished() {
    let bin = need_tofu!();
    let root = tmp_root("cancel-apply");
    let e = Arc::new(TofuEngine::with_sandbox(bin, Sandbox { work_root: root.clone(), ..Sandbox::default() }));
    let (log, lines) = sink();
    let marker = "127.0.0.78";
    let cfg = format!("resource \"terraform_data\" \"fast\" {{\n  input = \"quick\"\n}}\n{}", exec_cfg(&sleeper(marker)));
    let plan = e.plan(&cfg, None, &EngineVars::default(), &log).await.expect("plan");
    assert_eq!(plan.summary.add, 2);

    let token = tf_engine::CancelToken::new();
    let vars = EngineVars { cancel: Some(token.clone()), ..EngineVars::default() };
    let task = {
        let (e, cfg, blob, log) = (e.clone(), cfg.clone(), plan.blob.clone(), log.clone());
        tokio::spawn(async move { e.apply(&cfg, &blob, None, &vars, &log).await.map(|a| a.resource_count) })
    };
    // wait until the slow provisioner is really running, so the cancel lands mid-apply
    let waiting = Instant::now();
    while running(marker) == 0 {
        assert!(waiting.elapsed() < Duration::from_secs(90), "the provisioner never started");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let canceled_at = Instant::now();
    token.cancel();
    let r = tokio::time::timeout(Duration::from_secs(30), task).await.expect("apply stopped").unwrap();
    assert!(canceled_at.elapsed() < Duration::from_secs(15), "the 61 second sleeper was not waited for ({:?})", canceled_at.elapsed());
    let Err(EngineError::Canceled { partial_state }) = r else { panic!("expected Canceled, got {r:?}") };
    // Windows has no way to ask tofu to stop (no console to send Ctrl-C to), so it is killed at
    // once, and tofu has not yet written its in-memory state: nothing can be kept. With a
    // container or on Unix tofu is asked first and keeps the finished resources (see real_container.rs).
    let partial = if cfg!(windows) {
        assert!(partial_state.is_none(), "a hard kill leaves no state: {partial_state:?}");
        None
    } else {
        let p = partial_state.expect("tofu wrote down what it finished");
        assert!(tf_engine::resource_count(&p) >= 1, "{p}");
        Some(p)
    };

    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(running(marker), 0, "the provisioner's process is dead too");
    assert!(lines.lock().unwrap().iter().any(|l| l.contains("the run was canceled")), "the log says what happened");
    assert_eq!(leftovers(&root), 0, "the scratch directory is gone");

    // from what was kept (or from nothing, when a hard kill kept nothing) the next plan is sane
    let again = e.plan(&cfg, partial.as_deref(), &EngineVars::default(), &log).await.expect("plan after cancel");
    if partial.is_none() {
        assert_eq!(again.summary.add, 2, "nothing was recorded, so everything is planned again");
    } else {
        assert!(again.summary.has_changes(), "the unfinished resource still has to be done");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn canceling_a_step_that_has_just_begun_and_one_that_never_began() {
    let bin = need_tofu!();
    let root = tmp_root("cancel-early");
    let e = Arc::new(TofuEngine::with_sandbox(bin, Sandbox { work_root: root.clone(), ..Sandbox::default() }));
    let (log, _) = sink();

    // already canceled: nothing is started at all
    let token = tf_engine::CancelToken::new();
    token.cancel();
    let v = EngineVars { cancel: Some(token), ..EngineVars::default() };
    let t = Instant::now();
    assert_eq!(e.plan("", None, &v, &log).await.err(), Some(EngineError::Canceled { partial_state: None }));
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());

    // canceled a moment after tofu was started
    let token = tf_engine::CancelToken::new();
    let v = EngineVars { cancel: Some(token.clone()), ..EngineVars::default() };
    let task = {
        let (e, log) = (e.clone(), log.clone());
        tokio::spawn(async move { e.plan("resource \"terraform_data\" \"a\" {\n  input = \"x\"\n}\n", None, &v, &log).await.map(|_| ()) })
    };
    tokio::time::sleep(Duration::from_millis(250)).await;
    let t = Instant::now();
    token.cancel();
    let r = tokio::time::timeout(Duration::from_secs(20), task).await.expect("stopped").unwrap();
    assert_eq!(r, Err(EngineError::Canceled { partial_state: None }), "a plan has no state to keep");
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    assert_eq!(leftovers(&root), 0, "no scratch directory is left behind");
    let _ = std::fs::remove_dir_all(&root);
}
