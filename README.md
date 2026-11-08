# Tera Platform

Rust platform for plan/review/apply infrastructure runs: workspaces, state,
locking, a run queue and a web UI. Phase 1 drives an engine behind a trait
(a simulator today, the `tofu` binary optionally).

```bash
cargo test
cargo run -p tf-server        # http://127.0.0.1:8080, no auth: local use only
```

Environment: `TF_ADDR`, `TF_FAKE_DELAY_MS` (simulator speed), `TF_ENGINE=tofu`
with `TF_TOFU_BIN`.

Plans, research and the build log live in `replica/`.
