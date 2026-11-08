//! The container sandbox against REAL Docker and a REAL Linux tofu binary.
//! Needs `TF_TEST_TOFU_LINUX_BIN` (path of a Linux tofu on this host) and a working `docker`;
//! `TF_TEST_CONTAINER_IMAGE` picks the base image (default `postgres:16-alpine`, which
//! only has to contain a shell). Without them each case prints SKIPPED and passes.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tf_engine::{Container, Engine, EngineError, EngineVars, Log, Sandbox, TofuEngine};

/// One container test at a time: they share the docker daemon and count containers.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn linux_bin() -> Option<String> {
    std::env::var("TF_TEST_TOFU_LINUX_BIN").ok().filter(|s| !s.is_empty())
}

fn docker_ok() -> bool {
    std::process::Command::new("docker").arg("version").arg("--format").arg("{{.Server.Version}}").output().map(|o| o.status.success()).unwrap_or(false)
}

fn sink() -> (Log, Arc<Mutex<Vec<String>>>) {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let l = lines.clone();
    (Arc::new(move |s| l.lock().unwrap().push(s)), lines)
}

/// An engine that runs every step in a container, or `None` (with a SKIPPED line) when the
/// environment cannot.
fn engine(tag: &str, network: &str, tweak: impl FnOnce(&mut Sandbox)) -> Option<TofuEngine> {
    let Some(bin) = linux_bin() else {
        eprintln!("SKIPPED: TF_TEST_TOFU_LINUX_BIN not set");
        return None;
    };
    if !docker_ok() {
        eprintln!("SKIPPED: docker is not usable");
        return None;
    }
    let image = std::env::var("TF_TEST_CONTAINER_IMAGE").unwrap_or_else(|_| "postgres:16-alpine".into());
    let mut c = Container::new(bin, image);
    c.network = network.into();
    let mut sb = Sandbox { work_root: std::env::temp_dir().join(format!("tfclone-ctest-{tag}-{}", std::process::id())), container: Some(c), ..Sandbox::default() };
    tweak(&mut sb);
    Some(TofuEngine::with_sandbox("unused-in-container-mode", sb))
}

fn exec_cfg(command: &str) -> String {
    let escaped = command.replace('\\', "\\\\").replace('"', "\\\"");
    format!("resource \"terraform_data\" \"x\" {{\n  provisioner \"local-exec\" {{\n    command = \"{escaped}\"\n  }}\n}}\n")
}

async fn plan_then_apply(e: &TofuEngine, cfg: &str, vars: &EngineVars, log: &Log) -> Result<usize, EngineError> {
    let p = e.plan(cfg, None, vars, log).await?;
    Ok(e.apply(cfg, &p.blob, None, vars, log).await?.resource_count)
}

fn leftover_containers() -> usize {
    let o = std::process::Command::new("docker").args(["ps", "-aq", "--filter", "name=tfclone-run-"]).output().expect("docker ps");
    String::from_utf8_lossy(&o.stdout).split_whitespace().count()
}

#[tokio::test]
async fn a_full_cycle_runs_inside_containers() {
    let _serial = ONE_AT_A_TIME.lock().await;
    let Some(e) = engine("cycle", "none", |_| {}) else { return };
    let (log, lines) = sink();
    let cfg = "variable \"msg\" {\n  type = string\n}\nresource \"terraform_data\" \"a\" {\n  input = var.msg\n}\noutput \"o\" {\n  value = var.msg\n}\n";
    let mut v = EngineVars::default();
    v.terraform.insert("msg".into(), "one".into());
    let p = e.plan(cfg, None, &v, &log).await.expect("plan");
    assert_eq!((p.summary.add, p.summary.outputs), (1, 1));
    let a = e.apply(cfg, &p.blob, None, &v, &log).await.expect("apply");
    assert_eq!(a.resource_count, 1);
    let again = e.plan(cfg, Some(&a.state), &v, &log).await.unwrap();
    assert!(!again.summary.has_changes(), "state travelled through the container and back intact");
    v.terraform.insert("msg".into(), "two".into());
    assert_eq!(e.plan(cfg, Some(&a.state), &v, &log).await.unwrap().summary.change, 1);
    assert!(lines.lock().unwrap().iter().any(|l| l.contains("Apply complete")));
    assert_eq!(leftover_containers(), 0, "every container is removed");
}

#[tokio::test]
async fn the_container_has_its_own_read_only_filesystem_and_an_unprivileged_user() {
    let _serial = ONE_AT_A_TIME.lock().await;
    let Some(e) = engine("fs", "none", |_| {}) else { return };
    let (log, lines) = sink();
    let probe = "echo uid=$(id -u); head -1 /etc/os-release; touch /usr/x 2>&1 | head -1; echo caps=$(grep CapBnd /proc/self/status); ls /c /Users /host /mnt/c 2>&1 | head -2; echo marker-end";
    plan_then_apply(&e, &exec_cfg(probe), &EngineVars::default(), &log).await.expect("the probe runs");
    let out = lines.lock().unwrap().join("\n");
    assert!(out.contains("uid=65534"), "not root: {out}");
    assert!(out.contains("Alpine Linux"), "its own operating system, not the host's: {out}");
    assert!(out.contains("Read-only file system"), "the root filesystem is read-only: {out}");
    assert!(out.contains("CapBnd:\t0000000000000000") || out.contains("CapBnd: 0000000000000000"), "no capabilities, not even in the bounding set: {out}");
    assert!(out.contains("No such file"), "host directories are not there: {out}");
}

#[tokio::test]
async fn no_network_by_default_and_the_same_request_works_when_allowed() {
    let _serial = ONE_AT_A_TIME.lock().await;
    let Some(port) = free_listener() else { return };
    let cmd = format!("wget -q -T 3 -O - http://host.docker.internal:{port}/ >/dev/null 2>&1 && echo net-reached || echo net-blocked");
    let (log_a, lines_a) = sink();
    let Some(isolated) = engine("net-none", "none", |_| {}) else { return };
    plan_then_apply(&isolated, &exec_cfg(&cmd), &EngineVars::default(), &log_a).await.expect("apply");
    assert!(lines_a.lock().unwrap().iter().any(|l| l.trim_end().ends_with("net-blocked")), "no network: {:?}", lines_a.lock().unwrap().iter().filter(|l| l.contains("net-")).collect::<Vec<_>>());

    // control: the same command on a network reaches the host, so "blocked" above means the isolation worked
    let (log_b, lines_b) = sink();
    let open = engine("net-bridge", "bridge", |_| {}).unwrap();
    plan_then_apply(&open, &exec_cfg(&cmd), &EngineVars::default(), &log_b).await.expect("apply");
    assert!(lines_b.lock().unwrap().iter().any(|l| l.trim_end().ends_with("net-reached")), "control: {:?}", lines_b.lock().unwrap().iter().filter(|l| l.contains("net-")).collect::<Vec<_>>());
}

/// A throwaway HTTP responder on the host, so a container can prove whether it reaches the host.
fn free_listener() -> Option<u16> {
    if linux_bin().is_none() || !docker_ok() {
        eprintln!("SKIPPED: needs the Linux tofu binary and docker");
        return None;
    }
    let l = TcpListener::bind("0.0.0.0:0").ok()?;
    let port = l.local_addr().ok()?.port();
    std::thread::spawn(move || {
        for s in l.incoming().take(20).flatten() {
            let mut s = s;
            let mut buf = [0u8; 512];
            let _ = s.read(&mut buf);
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        }
    });
    Some(port)
}

#[tokio::test]
async fn the_servers_environment_never_enters_the_container_and_docker_variables_are_reserved() {
    let _serial = ONE_AT_A_TIME.lock().await;
    let Some(e) = engine("env", "none", |_| {}) else { return };
    std::env::set_var("PLATFORM_CONTAINER_CANARY", "canary-c0ffee");
    std::env::set_var("DATABASE_URL", "postgres://canary-db-4242");
    let (log, lines) = sink();
    let mut v = EngineVars::default();
    v.env.insert("MY_SECRET_VAR".into(), "visible-to-the-run".into());
    v.env.insert("DOCKER_HOST".into(), "tcp://evil.example:2375".into());
    let cmd = "echo own=$MY_SECRET_VAR canary=$PLATFORM_CONTAINER_CANARY db=$DATABASE_URL dh=$DOCKER_HOST home=$HOME";
    plan_then_apply(&e, &exec_cfg(cmd), &v, &log).await.expect("apply");
    let out = lines.lock().unwrap().join("\n");
    assert!(out.contains("own=visible-to-the-run"), "the run's variables arrive: {out}");
    assert!(!out.contains("canary-c0ffee") && !out.contains("canary-db-4242"), "no server secret: {out}");
    assert!(!out.contains("evil.example"), "a run cannot point the docker client elsewhere: {out}");
    assert!(out.contains("ignored environment variable names the sandbox reserves: DOCKER_HOST"), "and the log says it was ignored: {out}");
    assert!(out.contains("home=/work/home"), "HOME is inside the shared scratch directory: {out}");
}

#[tokio::test]
async fn a_memory_hog_is_killed_by_the_container_ceiling_but_runs_without_one() {
    let _serial = ONE_AT_A_TIME.lock().await;
    // doubling a string 27 times needs about 128 MB (and 256 MB while copying), a bounded amount
    let hog = exec_cfg("a=x; for i in $(seq 1 27); do a=\"$a$a\"; done; echo grew-$(printf %s \"$a\" | wc -c)");
    let Some(free) = engine("mem-free", "none", |sb| sb.memory_limit = None) else { return };
    let (log, lines) = sink();
    plan_then_apply(&free, &hog, &EngineVars::default(), &log).await.expect("control: with no ceiling the command completes");
    assert!(lines.lock().unwrap().iter().any(|l| l.contains("grew-134217728")), "control: it really built the 128 MB string");

    let capped = engine("mem-cap", "none", |sb| sb.memory_limit = Some(64 << 20)).unwrap();
    let (log, lines) = sink();
    let started = Instant::now();
    let r = plan_then_apply(&capped, &hog, &EngineVars::default(), &log).await;
    assert!(r.is_err(), "the same command under a 64 MB ceiling must be killed");
    assert!(!lines.lock().unwrap().iter().any(|l| l.contains("grew-1342")), "and must not have finished");
    assert!(started.elapsed() < Duration::from_secs(120));
    assert_eq!(leftover_containers(), 0);
}

#[tokio::test]
async fn a_process_ceiling_holds_inside_the_container() {
    let _serial = ONE_AT_A_TIME.lock().await;
    let Some(mut e) = engine("pids", "none", |_| {}) else { return };
    e.sandbox.container.as_mut().unwrap().pids_limit = 40;
    let (log, _) = sink();
    let spawn = |n: u32| exec_cfg(&format!("for i in $(seq 1 {n}); do sleep 2 & done; sleep 1; echo spawned-{n}"));
    // control: a few processes are fine under the same ceiling, so the failure below is the ceiling
    plan_then_apply(&e, &spawn(10), &EngineVars::default(), &log).await.expect("10 processes fit under a ceiling of 40");
    let r = plan_then_apply(&e, &spawn(200), &EngineVars::default(), &log).await;
    assert!(r.is_err(), "200 processes must not fit under a ceiling of 40 (the shell cannot fork)");
    assert_eq!(leftover_containers(), 0);
}

#[tokio::test]
async fn a_runaway_run_is_stopped_and_its_container_removed() {
    let _serial = ONE_AT_A_TIME.lock().await;
    let Some(e) = engine("timeout", "none", |sb| sb.apply_timeout = Duration::from_secs(5)) else { return };
    let (log, lines) = sink();
    let started = Instant::now();
    let r = plan_then_apply(&e, &exec_cfg("sleep 61"), &EngineVars::default(), &log).await;
    match &r {
        Err(EngineError::Failed(m)) => assert!(m.contains("timed out"), "{m}"),
        other => panic!("expected a timeout, got {:?}", other.as_ref().err()),
    }
    assert!(started.elapsed() < Duration::from_secs(50), "{:?}", started.elapsed());
    assert!(lines.lock().unwrap().iter().any(|l| l.contains("did not finish within 5 seconds")));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(leftover_containers(), 0, "the container (and the sleeper in it) is gone");
}

#[tokio::test]
async fn stale_containers_from_a_crashed_server_are_swept() {
    let _serial = ONE_AT_A_TIME.lock().await;
    let Some(e) = engine("sweep", "none", |_| {}) else { return };
    let img = std::env::var("TF_TEST_CONTAINER_IMAGE").unwrap_or_else(|_| "postgres:16-alpine".into());
    let started = std::process::Command::new("docker").args(["run", "-d", "--name", "tfclone-run-stale-test", "--entrypoint", "sleep", &img, "300"]).output().expect("docker run");
    assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
    assert_eq!(leftover_containers(), 1);
    assert_eq!(e.sweep_stale().min(1), 1, "the leftover is removed");
    assert_eq!(leftover_containers(), 0);
}

#[tokio::test]
async fn an_unusable_docker_or_image_is_a_clear_error() {
    let _serial = ONE_AT_A_TIME.lock().await;
    let Some(mut e) = engine("bad", "none", |_| {}) else { return };
    e.sandbox.container.as_mut().unwrap().image = "tfclone-no-such-image:never".into();
    let (log, _) = sink();
    let r = e.plan("", None, &EngineVars::default(), &log).await;
    assert!(matches!(r, Err(EngineError::Failed(_))), "{:?}", r.err());
    e.sandbox.container.as_mut().unwrap().docker = "definitely-not-docker".into();
    assert!(matches!(e.plan("", None, &EngineVars::default(), &log).await, Err(EngineError::Spawn(_))));
}

/// Starts `apply` in a container with a `fast` resource and one that sleeps, waits until the
/// sleeper is really running (the container exists and the log shows the provisioner), and
/// returns the engine, the config, the log and the running apply.
async fn apply_in_flight(e: Arc<TofuEngine>, token: tf_engine::CancelToken) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<Result<usize, EngineError>>, Log) {
    let cfg = format!("resource \"terraform_data\" \"fast\" {{\n  input = \"quick\"\n}}\n{}", exec_cfg("sleep 61"));
    let (log, lines) = sink();
    let plan = e.plan(&cfg, None, &EngineVars::default(), &log).await.expect("plan");
    assert_eq!(plan.summary.add, 2);
    let vars = EngineVars { cancel: Some(token), ..EngineVars::default() };
    let task = {
        let (e, cfg, log) = (e.clone(), cfg.clone(), log.clone());
        tokio::spawn(async move { e.apply(&cfg, &plan.blob, None, &vars, &log).await.map(|a| a.resource_count) })
    };
    let waiting = Instant::now();
    while !lines.lock().unwrap().iter().any(|l| l.contains("Executing")) {
        assert!(waiting.elapsed() < Duration::from_secs(90), "the provisioner never started: {:?}", lines.lock().unwrap());
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    (cfg, lines, task, log)
}

#[tokio::test]
async fn canceling_asks_tofu_to_stop_so_the_finished_resources_are_kept() {
    let _serial = ONE_AT_A_TIME.lock().await;
    let Some(e) = engine("cancel-graceful", "none", |_| {}) else { return };
    let e = Arc::new(e);
    let token = tf_engine::CancelToken::new();
    let (cfg, lines, task, log) = apply_in_flight(e.clone(), token.clone()).await;
    assert!(leftover_containers() >= 1, "the step is running in a container");

    let t = Instant::now();
    token.cancel();
    let r = tokio::time::timeout(Duration::from_secs(40), task).await.expect("stopped").unwrap();
    assert!(t.elapsed() < Duration::from_secs(20), "tofu was asked to stop and did, in {:?}", t.elapsed());
    let Err(EngineError::Canceled { partial_state: Some(state) }) = r else { panic!("expected the state tofu wrote on its way out, got {r:?}") };
    assert!(state.contains("\"fast\""), "the finished resource is recorded: {state}");
    assert!(state.contains("tainted"), "the one that was in progress is recorded as tainted, so the next plan replaces it: {state}");
    let log_text = lines.lock().unwrap().join("\n");
    assert!(log_text.contains("tofu shut down cleanly"), "{log_text}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(leftover_containers(), 0, "the container is gone");

    // the next plan keeps the finished resource and replaces the interrupted one
    let again = e.plan(&cfg, Some(&state), &EngineVars::default(), &log).await.expect("plan after cancel");
    assert_eq!((again.summary.add, again.summary.destroy), (1, 1), "{:?}", again.changes);
    assert!(again.changes.iter().any(|c| c.addr == "terraform_data.x" && c.action == tf_core::ChangeAction::Replace));
}

#[tokio::test]
async fn a_tofu_that_does_not_stop_in_time_is_killed_and_the_container_removed() {
    let _serial = ONE_AT_A_TIME.lock().await;
    // a grace period too short for tofu to answer: the fallback must still end the step
    let Some(e) = engine("cancel-kill", "none", |sb| sb.cancel_grace = Duration::from_millis(1)) else { return };
    let e = Arc::new(e);
    let token = tf_engine::CancelToken::new();
    let (_, lines, task, _) = apply_in_flight(e.clone(), token.clone()).await;
    let t = Instant::now();
    token.cancel();
    let r = tokio::time::timeout(Duration::from_secs(40), task).await.expect("stopped").unwrap();
    assert!(t.elapsed() < Duration::from_secs(20), "{:?}", t.elapsed());
    assert!(matches!(r, Err(EngineError::Canceled { .. })), "{r:?}");
    assert!(lines.lock().unwrap().iter().any(|l| l.contains("every process it started was killed")), "{:?}", lines.lock().unwrap());
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(leftover_containers(), 0, "no container left running a 61 second sleep");
}
