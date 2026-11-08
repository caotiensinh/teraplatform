use std::sync::Arc;
use std::time::Duration;
use tf_crypto::Crypto;
use tf_engine::{Engine, FakeEngine, TofuEngine};
use tf_server::auth::{Config, LogMailer};
use tf_store::{MemoryStore, PgStore, Store};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn flag(name: &str) -> bool {
    matches!(env(name).as_deref(), Some("1" | "true" | "yes"))
}

fn die(msg: &str) -> ! {
    eprintln!("tf-server: {msg}");
    std::process::exit(1)
}

#[tokio::main]
async fn main() {
    let addr = env("TF_ADDR").unwrap_or_else(|| "127.0.0.1:8080".into());
    let engine: Arc<dyn Engine> = match env("TF_ENGINE").as_deref() {
        Some("tofu") => {
            let mut sb = tf_engine::Sandbox::default();
            let secs = |name: &str| env(name).and_then(|v| v.parse::<u64>().ok()).map(Duration::from_secs);
            if let Some(d) = secs("TF_INIT_TIMEOUT_SECS") { sb.init_timeout = d }
            if let Some(d) = secs("TF_PLAN_TIMEOUT_SECS") { sb.plan_timeout = d }
            if let Some(d) = secs("TF_APPLY_TIMEOUT_SECS") { sb.apply_timeout = d }
            if let Some(d) = secs("TF_CANCEL_GRACE_SECS") { sb.cancel_grace = d }
            if let Some(mb) = env("TF_MEMORY_LIMIT_MB").and_then(|v| v.parse::<u64>().ok()) { sb.memory_limit = if mb == 0 { None } else { Some(mb << 20) } }
            if let Some(n) = env("TF_MAX_PROCESSES").and_then(|v| v.parse::<u32>().ok()) { sb.max_processes = n }
            if let Some(mb) = env("TF_MAX_LOG_MB").and_then(|v| v.parse::<usize>().ok()) { sb.max_output_bytes = mb << 20 }
            if let Some(dir) = env("TF_WORK_DIR") { sb.work_root = dir.into() }
            if let Some(list) = env("TF_ENV_PASSTHROUGH") { sb.env_passthrough = list.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect() }
            if env("TF_SANDBOX").as_deref() == Some("container") {
                let tofu = env("TF_CONTAINER_TOFU").unwrap_or_else(|| die("TF_SANDBOX=container needs TF_CONTAINER_TOFU: the path of a Linux tofu binary on this host"));
                if !std::path::Path::new(&tofu).is_file() {
                    die(&format!("TF_CONTAINER_TOFU {tofu} is not a file"));
                }
                let mut c = tf_engine::Container::new(tofu, env("TF_CONTAINER_IMAGE").unwrap_or_else(|| "alpine:3".into()));
                if let Some(d) = env("TF_DOCKER_BIN") { c.docker = d }
                if let Some(n) = env("TF_CONTAINER_NETWORK") { c.network = n }
                if let Some(n) = env("TF_CONTAINER_PIDS").and_then(|v| v.parse().ok()) { c.pids_limit = n }
                if let Some(x) = env("TF_CONTAINER_CPUS") { c.cpus = x.parse().ok() }
                if let Some(u) = env("TF_CONTAINER_USER") { c.user = Some(u) }
                if c.network != "none" {
                    eprintln!("warning: containers use the '{}' network: configurations can reach whatever that network reaches", c.network);
                }
                sb.container = Some(c);
            }
            let e = TofuEngine::with_sandbox(env("TF_TOFU_BIN").unwrap_or_else(|| "tofu".into()), sb);
            match e.version().await {
                Ok(v) if e.sandbox.container.is_some() => eprintln!("engine: {v}, each step in a throwaway container (read-only root, no capabilities, non-root, network '{}')", e.sandbox.container.as_ref().map(|c| c.network.as_str()).unwrap_or("none")),
                Ok(v) => eprintln!("engine: {v} (sandboxed: clean environment, time, output, memory and process ceilings; NO filesystem or network isolation: set TF_SANDBOX=container for that)"),
                Err(err) => die(&format!("TF_ENGINE=tofu but the binary does not work: {err}")),
            }
            let swept = e.sweep_stale();
            if swept > 0 {
                eprintln!("removed {swept} stale scratch director{} from an earlier run", if swept == 1 { "y" } else { "ies" });
            }
            Arc::new(e)
        }
        _ => {
            let ms = env("TF_FAKE_DELAY_MS").and_then(|v| v.parse().ok()).unwrap_or(400);
            Arc::new(FakeEngine::new(Duration::from_millis(ms)))
        }
    };

    // The master key protects state, saved plans and sensitive variables. With a
    // database it is mandatory: a random key would make stored data unreadable
    // after the next restart.
    let store: Arc<dyn Store> = match env("DATABASE_URL") {
        Some(url) => {
            let key = env("TF_MASTER_KEY").unwrap_or_else(|| die("DATABASE_URL is set, so TF_MASTER_KEY (64 hex characters) is required"));
            let crypto = Crypto::from_hex(&key).unwrap_or_else(|e| die(&e.to_string()));
            let s = PgStore::connect(&url, crypto).await.unwrap_or_else(|e| die(&format!("cannot start the database: {e}")));
            eprintln!("store: postgres");
            Arc::new(s)
        }
        None => {
            eprintln!("store: MEMORY (data is lost on restart; set DATABASE_URL and TF_MASTER_KEY for real use)");
            match env("TF_MASTER_KEY") {
                Some(k) => Arc::new(MemoryStore::with_crypto(Crypto::from_hex(&k).unwrap_or_else(|e| die(&e.to_string())))),
                None => Arc::new(MemoryStore::new()),
            }
        }
    };

    let cfg = Config {
        cookie_secure: flag("TF_COOKIE_SECURE"),
        require_verified: flag("TF_REQUIRE_VERIFIED"),
        base_url: env("TF_BASE_URL").unwrap_or_else(|| format!("http://{addr}")),
        session_ttl: tf_server::DEFAULT_TTL,
        api_rate_per_sec: env("TF_API_RATE").and_then(|v| v.parse().ok()).unwrap_or(30),
        api_lock_lease_secs: env("TF_API_LOCK_LEASE_SECS").and_then(|v| v.parse().ok()).unwrap_or(3600),
        cli_lock_lease_secs: env("TF_CLI_LOCK_LEASE_SECS").and_then(|v| v.parse().ok()).unwrap_or(12 * 3600),
        min_drift_interval_secs: env("TF_MIN_DRIFT_INTERVAL_SECS").and_then(|v| v.parse().ok()).unwrap_or(300),
        drift_tick_secs: env("TF_DRIFT_TICK_SECS").and_then(|v| v.parse().ok()).unwrap_or(60),
        github_api: env("TF_GITHUB_API").unwrap_or_else(|| "https://api.github.com".into()),
    };
    if !cfg.cookie_secure {
        eprintln!("warning: TF_COOKIE_SECURE is off; set it to 1 when serving over HTTPS");
    }
    eprintln!("mail: development mailer (links are printed here). Plug in a real provider before inviting anyone.");

    match store.recover_interrupted().await {
        Ok(0) => {}
        Ok(n) => eprintln!("recovered {n} run(s) interrupted by the last shutdown (marked errored, locks released)"),
        Err(e) => die(&format!("could not recover interrupted runs: {e}")),
    }
    let state = tf_server::AppState::new(store.clone(), engine.name(), Arc::new(LogMailer), cfg);
    tf_server::runner::spawn_with(store, engine.clone(), state.cancels.clone(), Some(state.reporter()));
    tf_server::drift::spawn(&state);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap_or_else(|e| die(&format!("cannot listen on {addr}: {e}")));
    eprintln!("tf-server listening on http://{addr} (engine: {})", engine.name());
    if let Err(e) = axum::serve(listener, tf_server::app(state)).await {
        eprintln!("server error: {e}");
    }
}
