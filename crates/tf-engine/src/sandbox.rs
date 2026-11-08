//! Process-level containment for the `tofu` child.
//!
//! What this gives, and what it does not:
//!
//! * **Clean environment.** The child gets an allow-list of variables, never the server's
//!   (`TF_MASTER_KEY`, `DATABASE_URL`, tokens). `HOME`, the CLI config and temp directories
//!   point into the run's own scratch directory, so the server user's saved credentials are
//!   not read.
//! * **Bounded time, output and memory.** Every step has a timeout; output is streamed with a
//!   cap on total bytes, lines and line length; on Windows the whole process tree lives in a
//!   job object with a memory ceiling and a process-count ceiling.
//! * **The whole tree dies.** On timeout, and when `tofu` exits, everything it started
//!   (providers, provisioners, background processes) is killed.
//!
//! In this mode it does **not** isolate the filesystem or the network: configuration that
//! reads `file("C:/...")` or runs `local-exec` still acts as the server's user. Set
//! `Sandbox::container` to run every step inside a throwaway Docker container instead (own
//! root filesystem, read-only, no capabilities, non-root, no network by default, memory,
//! CPU and process ceilings); then only the run's scratch directory is shared with it.

use crate::{emit, wait_canceled, CancelToken, EngineError, Log};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// Run each tofu step in a throwaway Docker container.
#[derive(Debug, Clone)]
pub struct Container {
    pub docker: String,
    /// Any image with a shell; the tofu binary is mounted into it. Prefer a small pinned one.
    pub image: String,
    /// A Linux tofu binary on the host (mounted read-only at /usr/local/bin/tofu).
    pub tofu_binary: PathBuf,
    /// `none` (default: no network, so provider downloads fail), `bridge`, or a network name.
    pub network: String,
    pub pids_limit: u32,
    pub cpus: Option<f32>,
    /// `uid:gid` inside the container.
    pub user: Option<String>,
}

impl Container {
    pub fn new(tofu_binary: impl Into<PathBuf>, image: impl Into<String>) -> Self {
        Self { docker: "docker".into(), image: image.into(), tofu_binary: tofu_binary.into(), network: "none".into(), pids_limit: 512, cpus: Some(2.0), user: Some("65534:65534".into()) }
    }
}

#[derive(Debug, Clone)]
pub struct Sandbox {
    pub init_timeout: Duration,
    pub plan_timeout: Duration,
    pub apply_timeout: Duration,
    /// When a run is canceled, how long tofu is given to stop by itself (it is asked to, like
    /// Ctrl-C) and write down what it finished, before everything is killed. A hard kill loses
    /// that record: tofu keeps state in memory and writes it out at the end or after a stop request.
    /// Windows process mode has no way to ask, so it kills at once; containers and Unix ask.
    pub cancel_grace: Duration,
    /// Bytes of tofu output kept in the log per step.
    pub max_output_bytes: usize,
    pub max_log_lines: usize,
    pub max_line_bytes: usize,
    /// Largest `show -json` document read back.
    pub max_plan_json_bytes: usize,
    /// Memory ceiling for the whole process tree (Windows job object).
    pub memory_limit: Option<u64>,
    /// Process-count ceiling for the whole tree (Windows job object).
    pub max_processes: u32,
    /// Parent of the per-run scratch directories.
    pub work_root: PathBuf,
    /// Names of server environment variables to pass through (e.g. `HTTPS_PROXY`).
    pub env_passthrough: Vec<String>,
    /// Run in a container instead of directly on the host.
    pub container: Option<Container>,
}

impl Default for Sandbox {
    fn default() -> Self {
        Self {
            init_timeout: Duration::from_secs(10 * 60),
            plan_timeout: Duration::from_secs(15 * 60),
            apply_timeout: Duration::from_secs(60 * 60),
            cancel_grace: Duration::from_secs(15),
            max_output_bytes: 8 << 20,
            max_log_lines: 20_000,
            max_line_bytes: 16 << 10,
            max_plan_json_bytes: 64 << 20,
            memory_limit: Some(2 << 30),
            max_processes: 64,
            work_root: std::env::temp_dir().join("tfclone-work"),
            env_passthrough: Vec::new(),
            container: None,
        }
    }
}

/// Server variables the child may see: just enough to find programs and run a shell.
const ENV_ALLOW: &[&str] = &["PATH", "PATHEXT", "SystemRoot", "SYSTEMROOT", "windir", "ComSpec", "LANG", "LC_ALL", "TZ", "SSL_CERT_FILE", "SSL_CERT_DIR"];

/// Variables the sandbox owns: a run's own variables can never override them.
/// What the docker client itself needs from the server's environment (never forwarded into the container).
const DOCKER_CLIENT_ENV: &[&str] = &["PATH", "SystemRoot", "SYSTEMROOT", "windir", "ComSpec", "USERPROFILE", "HOMEDRIVE", "HOMEPATH", "HOME", "APPDATA", "LOCALAPPDATA", "ProgramData", "ProgramFiles", "TEMP", "TMP", "XDG_RUNTIME_DIR", "DOCKER_HOST", "DOCKER_CONTEXT", "DOCKER_CONFIG", "DOCKER_TLS_VERIFY", "DOCKER_CERT_PATH"];

/// Variables given to the container explicitly (by value, none of them secret).
const CONTAINER_FIXED: &[&str] = &["HOME", "TEMP", "TMP", "TMPDIR", "TF_CLI_CONFIG_FILE", "PATH", "TF_IN_AUTOMATION", "TF_INPUT", "CHECKPOINT_DISABLE"];

const ENV_OWNED: &[&str] = &["HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "TEMP", "TMP", "TMPDIR", "TF_CLI_CONFIG_FILE", "TF_DATA_DIR", "TF_IN_AUTOMATION", "TF_INPUT", "CHECKPOINT_DISABLE", "TF_PLUGIN_CACHE_DIR"];

impl Sandbox {
    /// The complete environment for one tofu process, built from nothing, plus the names of
    /// run variables that were ignored because the sandbox owns them.
    pub fn environment(&self, dir: &Path, run_env: &BTreeMap<String, String>) -> (Vec<(String, String)>, Vec<String>) {
        let server: BTreeMap<String, String> = std::env::vars().collect();
        let mut env: Vec<(String, String)> = Vec::new();
        let put = |env: &mut Vec<(String, String)>, k: &str, v: String| {
            env.retain(|(ek, _)| !ek.eq_ignore_ascii_case(k));
            env.push((k.to_string(), v));
        };
        let in_container = self.container.is_some();
        // Names a run may not set: the sandbox's own, and (in a container) the docker client's.
        let reserved = |k: &str| {
            ENV_OWNED.iter().any(|o| o.eq_ignore_ascii_case(k))
                || (in_container && (DOCKER_CLIENT_ENV.iter().any(|o| o.eq_ignore_ascii_case(k)) || k.to_ascii_uppercase().starts_with("DOCKER_") || CONTAINER_FIXED.iter().any(|o| o.eq_ignore_ascii_case(k))))
        };
        if !in_container {
            for name in ENV_ALLOW.iter().map(|s| s.to_string()).chain(self.env_passthrough.iter().cloned()) {
                if let Some((k, v)) = server.iter().find(|(k, _)| k.eq_ignore_ascii_case(&name)) {
                    put(&mut env, k, v.clone());
                }
            }
        } else {
            for name in &self.env_passthrough {
                if let Some((k, v)) = server.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
                    put(&mut env, k, v.clone());
                }
            }
        }
        // the run's own variables (workspace variables, TF_VAR_*), then what the sandbox insists on
        let mut ignored = Vec::new();
        for (k, v) in run_env {
            if reserved(k) {
                ignored.push(k.clone());
            } else {
                put(&mut env, k, v.clone());
            }
        }
        let (home, tmp, rc) = if in_container {
            ("/work/home".to_string(), "/work/tmp".to_string(), "/work/home/tofurc".to_string())
        } else {
            (dir.join("home").to_string_lossy().to_string(), dir.join("tmp").to_string_lossy().to_string(), dir.join("home").join("tofurc").to_string_lossy().to_string())
        };
        let mut fixed: Vec<(&str, String)> = vec![
            ("HOME", home.clone()),
            ("TEMP", tmp.clone()),
            ("TMP", tmp.clone()),
            ("TMPDIR", tmp),
            ("TF_CLI_CONFIG_FILE", rc),
            ("TF_IN_AUTOMATION", "1".into()),
            ("TF_INPUT", "0".into()),
            ("CHECKPOINT_DISABLE", "1".into()),
        ];
        if in_container {
            fixed.push(("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()));
        } else {
            fixed.push(("USERPROFILE", home.clone()));
            fixed.push(("APPDATA", home.clone()));
            fixed.push(("LOCALAPPDATA", home));
        }
        for (k, v) in fixed {
            put(&mut env, k, v);
        }
        (env, ignored)
    }

    /// A fresh scratch directory for one run, with its own home and temp.
    pub async fn make_workdir(&self) -> Result<PathBuf, EngineError> {
        let dir = self.work_root.join(format!("tfclone-{}", tf_core::new_id()));
        for sub in ["home", "tmp"] {
            tokio::fs::create_dir_all(dir.join(sub)).await.map_err(|e| EngineError::Failed(format!("cannot create the scratch directory: {e}")))?;
        }
        tokio::fs::write(dir.join("home").join("tofurc"), "").await.map_err(|e| EngineError::Failed(e.to_string()))?;
        Ok(dir)
    }

    /// Removes containers a crashed server left behind. Call once at startup (one server instance).
    pub fn sweep_containers(&self) -> usize {
        let Some(c) = &self.container else { return 0 };
        let env = docker_client_env(&[]);
        let list = std::process::Command::new(&c.docker).args(["ps", "-aq", "--filter", "name=tfclone-run-"]).env_clear().envs(env.clone()).output();
        let Ok(list) = list else { return 0 };
        let ids: Vec<String> = String::from_utf8_lossy(&list.stdout).split_whitespace().map(String::from).collect();
        for id in &ids {
            let _ = std::process::Command::new(&c.docker).args(["rm", "-f", id]).env_clear().envs(env.clone()).output();
        }
        ids.len()
    }

    /// Removes scratch directories left behind by a crashed server.
    pub fn sweep_stale(&self, older_than: Duration) -> usize {
        let Ok(rd) = std::fs::read_dir(&self.work_root) else { return 0 };
        let mut n = 0;
        for e in rd.flatten() {
            let stale = e.file_name().to_string_lossy().starts_with("tfclone-")
                && e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map(|a| a >= older_than).unwrap_or(false);
            if stale && std::fs::remove_dir_all(e.path()).is_ok() {
                n += 1;
            }
        }
        n
    }
}

// ---------------------------------------------------------------------------
// containers
// ---------------------------------------------------------------------------

/// What the docker client needs to find its daemon, plus the run's variables (forwarded into
/// the container by name, so no secret ever appears on a command line).
fn docker_client_env(run_vars: &[(String, String)]) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::vars().filter(|(k, _)| DOCKER_CLIENT_ENV.iter().any(|a| a.eq_ignore_ascii_case(k))).collect();
    for (k, v) in run_vars {
        env.retain(|(ek, _)| !ek.eq_ignore_ascii_case(k));
        env.push((k.clone(), v.clone()));
    }
    env
}

struct DockerRun {
    args: Vec<String>,
    client_env: Vec<(String, String)>,
    name: String,
}

fn docker_run(sb: &Sandbox, c: &Container, args: &[&str], dir: &Path, env: &[(String, String)]) -> DockerRun {
    let name = format!("tfclone-run-{}", tf_core::new_id());
    let mut a: Vec<String> = vec!["run".into(), "--rm".into(), "--name".into(), name.clone(), "--read-only".into(), "--tmpfs".into(), "/tmp:rw,size=256m".into(), "--cap-drop".into(), "ALL".into(), "--security-opt".into(), "no-new-privileges".into(), "--pids-limit".into(), c.pids_limit.to_string(), "--network".into(), c.network.clone(), "--workdir".into(), "/work".into()];
    if let Some(m) = sb.memory_limit {
        let mb = (m >> 20).max(64);
        a.extend(["--memory".into(), format!("{mb}m"), "--memory-swap".into(), format!("{mb}m")]);
    }
    if let Some(cpus) = c.cpus {
        a.extend(["--cpus".into(), cpus.to_string()]);
    }
    if let Some(u) = &c.user {
        a.extend(["--user".into(), u.clone()]);
    }
    a.extend(["-v".into(), format!("{}:/work", dir.display()), "-v".into(), format!("{}:/usr/local/bin/tofu:ro", c.tofu_binary.display())]);
    let mut run_vars = Vec::new();
    for (k, v) in env {
        if CONTAINER_FIXED.iter().any(|f| f.eq_ignore_ascii_case(k)) {
            a.extend(["-e".into(), format!("{k}={v}")]);
        } else {
            a.extend(["-e".into(), k.clone()]); // by name: the value travels in the client's environment
            run_vars.push((k.clone(), v.clone()));
        }
    }
    a.extend(["--entrypoint".into(), "/usr/local/bin/tofu".into(), c.image.clone()]);
    a.extend(args.iter().map(|x| x.to_string()));
    DockerRun { args: a, client_env: docker_client_env(&run_vars), name }
}

/// Asks tofu to stop the way Ctrl-C does: it finishes bookkeeping, writes its state and exits.
/// Returns false when this platform has no way to ask (Windows without a container).
async fn ask_to_stop(container: &Option<(String, String, Vec<(String, String)>)>, child: &tokio::process::Child) -> bool {
    if let Some((docker, name, env)) = container {
        let (docker, name, env) = (docker.clone(), name.clone(), env.clone());
        return tokio::task::spawn_blocking(move || {
            std::process::Command::new(docker).args(["kill", "--signal=INT", &name]).env_clear().envs(env).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
        })
        .await
        .unwrap_or(false);
    }
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            return std::process::Command::new("kill").args(["-INT", &pid.to_string()]).status().map(|s| s.success()).unwrap_or(false);
        }
    }
    let _ = child;
    false
}

/// Make sure a container is gone, however its client process ended.
fn remove_container(docker: &str, name: &str, env: &[(String, String)]) {
    let _ = std::process::Command::new(docker).args(["rm", "-f", name]).env_clear().envs(env.iter().cloned()).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

// ---------------------------------------------------------------------------
// killing the whole process tree
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod tree {
    use std::io;
    use std::os::raw::c_void;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// A job object: every process the child starts joins it, and the job has ceilings.
    pub struct Tree(HANDLE);
    unsafe impl Send for Tree {}
    unsafe impl Sync for Tree {}

    impl Tree {
        pub fn new(memory: Option<u64>, max_processes: u32) -> io::Result<Tree> {
            unsafe {
                let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if (h as usize) == 0 {
                    return Err(io::Error::last_os_error());
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                let mut flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
                info.BasicLimitInformation.ActiveProcessLimit = max_processes;
                if let Some(m) = memory {
                    flags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
                    info.JobMemoryLimit = m as usize;
                }
                info.BasicLimitInformation.LimitFlags = flags;
                let ok = SetInformationJobObject(
                    h,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    let e = io::Error::last_os_error();
                    CloseHandle(h);
                    return Err(e);
                }
                Ok(Tree(h))
            }
        }

        pub fn attach(&self, child: &tokio::process::Child) -> io::Result<()> {
            let ph = child.raw_handle().ok_or_else(|| io::Error::other("the process already exited"))?;
            if unsafe { AssignProcessToJobObject(self.0, ph as HANDLE) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub fn kill(&self) {
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0); // KILL_ON_JOB_CLOSE ends anything still alive
            }
        }
    }
}

#[cfg(not(windows))]
mod tree {
    use std::io;

    /// A process group. Untested on this platform in this build: no memory or process-count
    /// ceilings here (use a container for those).
    pub struct Tree {
        pgid: std::sync::atomic::AtomicU32,
    }

    impl Tree {
        pub fn new(_memory: Option<u64>, _max_processes: u32) -> io::Result<Tree> {
            Ok(Tree { pgid: std::sync::atomic::AtomicU32::new(0) })
        }

        pub fn attach(&self, child: &tokio::process::Child) -> io::Result<()> {
            self.pgid.store(child.id().unwrap_or(0), std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        pub fn kill(&self) {
            let pgid = self.pgid.load(std::sync::atomic::Ordering::SeqCst);
            if pgid != 0 {
                let _ = std::process::Command::new("kill").args(["-KILL", "--", &format!("-{pgid}")]).status();
            }
        }
    }
}

use tree::Tree;

// ---------------------------------------------------------------------------
// bounded streaming
// ---------------------------------------------------------------------------

struct Budget {
    bytes: AtomicUsize,
    lines: AtomicUsize,
    truncated: AtomicBool,
}

/// Reads a stream, emits whole lines within the caps, and always drains it so the child never
/// blocks on a full pipe. When `capture` is set the bytes are kept too (up to its limit).
async fn pump<R: AsyncRead + Unpin>(mut r: R, log: Log, sb: Sandbox, budget: Arc<Budget>, capture: Option<Arc<Mutex<Vec<u8>>>>, capture_limit: usize, overflow: Arc<AtomicBool>) {
    let mut buf = [0u8; 8192];
    let mut line: Vec<u8> = Vec::new();
    let mut cut = false;
    loop {
        let n = match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if let Some(c) = &capture {
            let mut g = c.lock().unwrap_or_else(|p| p.into_inner());
            if g.len() + n <= capture_limit {
                g.extend_from_slice(&buf[..n]);
            } else {
                overflow.store(true, Ordering::SeqCst);
            }
        } else {
            let total = budget.bytes.fetch_add(n, Ordering::SeqCst) + n;
            for &b in &buf[..n] {
                if b == b'\n' {
                    flush(&mut line, &mut cut, &log, &sb, &budget, total);
                } else if line.len() < sb.max_line_bytes {
                    line.push(b);
                } else {
                    cut = true;
                }
            }
        }
    }
    if capture.is_none() && !line.is_empty() {
        let total = budget.bytes.load(Ordering::SeqCst);
        flush(&mut line, &mut cut, &log, &sb, &budget, total);
    }
}

fn flush(line: &mut Vec<u8>, cut: &mut bool, log: &Log, sb: &Sandbox, budget: &Budget, total_bytes: usize) {
    let mut text = String::from_utf8_lossy(line).trim_end_matches('\r').to_string();
    if *cut {
        text.push_str(" [line cut]");
    }
    line.clear();
    *cut = false;
    let n = budget.lines.fetch_add(1, Ordering::SeqCst) + 1;
    if total_bytes > sb.max_output_bytes || n > sb.max_log_lines {
        if !budget.truncated.swap(true, Ordering::SeqCst) {
            emit(log, "[output truncated: this step produced more than the log keeps; the run continues]");
        }
        return;
    }
    emit(log, text);
}

/// Runs one program under the sandbox. Output lines go to `log`. With `capture`, stdout is
/// returned instead of logged (for `show -json`). Fails on timeout, on a non-zero exit, and
/// when a captured document is larger than allowed.
pub async fn run(sb: &Sandbox, binary: &str, args: &[&str], dir: &Path, env: Vec<(String, String)>, timeout: Duration, log: &Log, capture: Option<usize>, cancel: Option<&CancelToken>) -> Result<String, EngineError> {
    if cancel.map(|c| c.is_canceled()).unwrap_or(false) {
        return Err(EngineError::Canceled { partial_state: None });
    }
    let mut container: Option<(String, String, Vec<(String, String)>)> = None; // (docker, name, client env)
    let mut cmd = match &sb.container {
        Some(c) => {
            let d = docker_run(sb, c, args, dir, &env);
            let mut cmd = Command::new(&c.docker);
            cmd.args(&d.args).env_clear().envs(d.client_env.iter().cloned());
            container = Some((c.docker.clone(), d.name, d.client_env));
            cmd
        }
        None => {
            let mut cmd = Command::new(binary);
            cmd.args(args).env_clear().envs(env);
            cmd
        }
    };
    cmd.current_dir(dir).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd.spawn().map_err(|e| EngineError::Spawn(format!("{}: {e}", if container.is_some() { sb.container.as_ref().map(|c| c.docker.as_str()).unwrap_or("docker") } else { binary })))?;
    let tree = Tree::new(if container.is_some() { None } else { sb.memory_limit }, sb.max_processes).map_err(|e| EngineError::Failed(format!("cannot contain the process: {e}")))?;
    if let Err(e) = tree.attach(&child) {
        let _ = child.kill().await;
        return Err(EngineError::Failed(format!("cannot contain the process: {e}")));
    }

    let budget = Arc::new(Budget { bytes: AtomicUsize::new(0), lines: AtomicUsize::new(0), truncated: AtomicBool::new(false) });
    let captured = capture.map(|_| Arc::new(Mutex::new(Vec::new())));
    let overflow = Arc::new(AtomicBool::new(false));
    let limit = capture.unwrap_or(0);
    let out = child.stdout.take().map(|s| tokio::spawn(pump(s, log.clone(), sb.clone(), budget.clone(), captured.clone(), limit, overflow.clone())));
    let err = child.stderr.take().map(|s| tokio::spawn(pump(s, log.clone(), sb.clone(), budget.clone(), None, 0, overflow.clone())));

    enum Ended {
        Exited(std::io::Result<std::process::ExitStatus>),
        TimedOut,
        Canceled,
    }
    let ended = tokio::select! {
        r = child.wait() => Ended::Exited(r),
        _ = tokio::time::sleep(timeout) => Ended::TimedOut,
        _ = wait_canceled(cancel) => Ended::Canceled,
    };
    // A canceled run is asked to stop first, so tofu can write down what it finished.
    let mut stopped_by_itself = false;
    if matches!(ended, Ended::Canceled) && ask_to_stop(&container, &child).await {
        emit(log, format!("[canceling: asked tofu to stop, waiting up to {} seconds for it to write down its state]", sb.cancel_grace.as_secs()));
        stopped_by_itself = tokio::time::timeout(sb.cancel_grace, child.wait()).await.is_ok();
    }
    // Whatever tofu started and left behind (daemons, runaway provisioners) ends with it.
    tree.kill();
    if !matches!(ended, Ended::Exited(_)) && !stopped_by_itself {
        let _ = child.kill().await;
    }
    // Killing the docker client does not stop the container: remove it explicitly.
    if let Some((docker, name, client_env)) = &container {
        let (docker, name, client_env) = (docker.clone(), name.clone(), client_env.clone());
        let _ = tokio::task::spawn_blocking(move || remove_container(&docker, &name, &client_env)).await;
    }
    for t in [out, err].into_iter().flatten() {
        let _ = tokio::time::timeout(Duration::from_secs(5), t).await;
    }

    let status = match ended {
        Ended::TimedOut => {
            emit(log, format!("[stopped: {} did not finish within {} seconds, and every process it started was killed]", args.first().unwrap_or(&"tofu"), timeout.as_secs()));
            return Err(EngineError::Failed(format!("tofu {} timed out after {}s", args.first().unwrap_or(&""), timeout.as_secs())));
        }
        Ended::Canceled => {
            let step = args.first().unwrap_or(&"tofu");
            if stopped_by_itself {
                emit(log, format!("[stopped: the run was canceled during {step}; tofu shut down cleanly]"));
            } else {
                emit(log, format!("[stopped: the run was canceled during {step}; tofu could not stop by itself in time, so every process it started was killed]"));
            }
            return Err(EngineError::Canceled { partial_state: None });
        }
        Ended::Exited(Err(e)) => return Err(EngineError::Failed(format!("tofu could not be waited for: {e}"))),
        Ended::Exited(Ok(s)) => s,
    };
    if overflow.load(Ordering::SeqCst) {
        return Err(EngineError::Failed(format!("tofu {} produced a document larger than {} MB", args.first().unwrap_or(&""), limit >> 20)));
    }
    if !status.success() {
        return Err(EngineError::Failed(format!("tofu {} exited with {}", args.first().unwrap_or(&""), status)));
    }
    Ok(captured.map(|c| String::from_utf8_lossy(&c.lock().unwrap_or_else(|p| p.into_inner())).to_string()).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_is_built_from_nothing() {
        std::env::set_var("PLATFORM_UNIT_SECRET", "s3");
        std::env::set_var("TF_MASTER_KEY", "k");
        let sb = Sandbox { env_passthrough: vec!["PLATFORM_UNIT_PASS".into()], ..Sandbox::default() };
        std::env::set_var("PLATFORM_UNIT_PASS", "ok");
        let dir = Path::new("scratch");
        let run: BTreeMap<String, String> = [("MY_VAR".to_string(), "v".to_string()), ("HOME".to_string(), "C:/evil".to_string()), ("tf_cli_config_file".to_string(), "x".to_string())].into();
        let (env, ignored) = sb.environment(dir, &run);
        let get = |k: &str| env.iter().find(|(ek, _)| ek.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone());
        assert_eq!(get("PLATFORM_UNIT_SECRET"), None, "server variables are not inherited");
        assert_eq!(get("TF_MASTER_KEY"), None);
        assert_eq!(get("DATABASE_URL"), None);
        assert_eq!(get("PLATFORM_UNIT_PASS").as_deref(), Some("ok"), "an explicit pass-through works");
        assert_eq!(get("MY_VAR").as_deref(), Some("v"), "the run's own variables arrive");
        assert!(get("HOME").unwrap().ends_with("home"), "a run cannot move HOME: {:?}", get("HOME"));
        assert!(get("TF_CLI_CONFIG_FILE").unwrap().ends_with("tofurc"), "nor the CLI config (case-insensitive)");
        assert_eq!(get("TF_INPUT").as_deref(), Some("0"));
        assert_eq!(env.iter().filter(|(k, _)| k.eq_ignore_ascii_case("HOME")).count(), 1, "no duplicates");
        assert!(ignored.iter().any(|k| k == "HOME") && ignored.iter().any(|k| k.eq_ignore_ascii_case("tf_cli_config_file")), "ignored names are reported: {ignored:?}");
    }

    #[tokio::test]
    async fn scratch_directories_are_private_and_swept() {
        let root = std::env::temp_dir().join(format!("tfclone-test-{}", tf_core::new_id()));
        let sb = Sandbox { work_root: root.clone(), ..Sandbox::default() };
        let a = sb.make_workdir().await.unwrap();
        let b = sb.make_workdir().await.unwrap();
        assert_ne!(a, b);
        assert!(a.join("home").join("tofurc").exists() && a.join("tmp").is_dir());
        assert_eq!(sb.sweep_stale(Duration::from_secs(3600)), 0, "fresh directories stay");
        assert_eq!(sb.sweep_stale(Duration::from_secs(0)), 2, "stale ones go");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn in_a_container_the_environment_uses_container_paths_and_reserves_docker_names() {
        std::env::set_var("PLATFORM_UNIT_SECRET2", "s3");
        let sb = Sandbox { container: Some(Container::new("/opt/tofu", "alpine:3")), ..Sandbox::default() };
        let run: BTreeMap<String, String> = [("MY_VAR".to_string(), "v".to_string()), ("DOCKER_HOST".to_string(), "tcp://evil:2375".to_string()), ("path".to_string(), "/evil".to_string()), ("HOME".to_string(), "/evil".to_string())].into();
        let (env, ignored) = sb.environment(Path::new("D:/scratch"), &run);
        let get = |k: &str| env.iter().find(|(ek, _)| ek.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone());
        assert_eq!(get("HOME").as_deref(), Some("/work/home"));
        assert_eq!(get("TF_CLI_CONFIG_FILE").as_deref(), Some("/work/home/tofurc"));
        assert_eq!(get("MY_VAR").as_deref(), Some("v"));
        assert_eq!(get("DOCKER_HOST"), None, "a run must never be able to point the docker client elsewhere");
        assert_eq!(get("PATH").unwrap(), "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin", "the host PATH is meaningless in the container");
        assert_eq!(get("PLATFORM_UNIT_SECRET2"), None);
        for n in ["DOCKER_HOST", "path", "HOME"] {
            assert!(ignored.iter().any(|k| k == n), "{n} should be reported as ignored: {ignored:?}");
        }
    }

    #[test]
    fn the_docker_command_is_locked_down_and_carries_no_secret() {
        let sb = Sandbox { memory_limit: Some(512 << 20), container: Some(Container::new("/opt/tofu", "alpine:3")), ..Sandbox::default() };
        let (env, _) = sb.environment(Path::new("/scratch/run1"), &[("API_SECRET".to_string(), "hunter2".to_string())].into());
        let d = docker_run(&sb, sb.container.as_ref().unwrap(), &["apply", "plan.bin"], Path::new("/scratch/run1"), &env);
        let line = d.args.join(" ");
        for must in ["--rm", "--read-only", "--cap-drop ALL", "no-new-privileges", "--network none", "--pids-limit 512", "--memory 512m", "--memory-swap 512m", "--user 65534:65534", "--entrypoint /usr/local/bin/tofu", "alpine:3 apply plan.bin"] {
            assert!(line.contains(must), "missing {must}: {line}");
        }
        assert!(line.contains("/scratch/run1:/work") && line.contains("/opt/tofu:/usr/local/bin/tofu:ro"));
        assert!(!line.contains("hunter2"), "the secret must not be on the command line: {line}");
        assert!(d.args.windows(2).any(|w| w[0] == "-e" && w[1] == "API_SECRET"), "it is forwarded by name");
        assert!(d.client_env.iter().any(|(k, v)| k == "API_SECRET" && v == "hunter2"), "and the value rides in the client's environment");
        assert!(d.name.starts_with("tfclone-run-"));
    }
}
