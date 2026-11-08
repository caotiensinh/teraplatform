# Build log

## 2026-11-07: M1 vertical slice (Rust)

Workspace at `tfclone/`: `tf-core` (domain + run state machine), `tf-engine`
(Engine trait, simulator, tofu driver), `tf-store` (Store trait, in-memory),
`tf-server` (axum pages, JSON API, runner).

Run it: `cargo run -p tf-server` then open http://127.0.0.1:8080
(`TF_ADDR`, `TF_FAKE_DELAY_MS`, `TF_ENGINE=tofu` to use a real binary).

Verified: `cargo test` 32 tests pass (9 core, 6 engine, 9 store, 8 end to end);
manual run in the browser: create workspace, queue run, plan, confirm, apply,
Applied, state serial 1.

| screen | status | missing / notes |
| --- | --- | --- |
| S04 Workspace list | done (slice) | no tag filter, search, pagination |
| S05 Workspace overview | partial | stats, lock, queue-a-run form, runs table; no settings, no drift, no tags |
| S06 Runs list | partial | all runs, live refresh; no filters or pagination |
| S07 Run detail | done (slice) | stepper, stats, resource changes, log, details, confirm/discard/cancel; no policy/cost stages, no comments |
| S08 Plan diff | partial | resource list only, no attribute diff |
| S09 States | partial | version list; no download, rollback or outputs |
| S01, S02, S03, S10 to S18 | not started | auth, org, projects, variables, settings, VCS, teams, policies, tokens |

## Deviations from architecture.md (all deliberate, all to be reversed)

- **No Postgres yet.** Not installed here. `Store` is the contract; `MemoryStore` implements it. `schema.sql` is still unexecuted. Swap in sqlx without touching handlers (replica-backend).
- **Simulated engine by default.** No `tofu` binary on this machine. `FakeEngine` reads `resource "t" "n" { ... }` blocks and diffs them against state, so plan, apply, update and destroy behave like a real run. `TofuEngine` is written (init, plan -out, show -json, apply plan) and unit-tested only for plan-JSON parsing; it has never run against a real binary and is not sandboxed.
- **No askama/htmx.** Pages are plain Rust string rendering with one `esc()` for every dynamic value (test proves script injection is escaped). Live updates are a 2 s meta refresh, not SSE.
- **Edition 2021**, not 2024.
- **No auth.** Binds 127.0.0.1; single default org. Do not expose it.
- **State is not encrypted** and is lost on restart.

## Harder than expected / watch

- The queue rule (one executing run per workspace, FIFO, a paused run still blocks) lives in `claim_next`. In Postgres it must become a transaction with `FOR UPDATE SKIP LOCKED` plus the partial unique index from `schema.sql`; the concurrency test is the template.
- A run canceled or discarded while the engine works is protected by the state machine: late events are rejected and ignored by the runner.
- Applying cannot be canceled (partial state risk); force-cancel needs the admin work in M2.

## 2026-11-07: backend pass (replica-backend)

PostgreSQL store, accounts, sessions, tokens, roles, tenant isolation,
variables, encryption at rest. Details, checklist and known weaknesses in
`backend.md`. The in-memory store remains for tests and quick local runs.

Changes to earlier deviations:
- **No Postgres yet** is resolved: `PgStore` + `migrations/0001_init.sql`, tested on PostgreSQL 16. `schema.sql` is still the larger target; the migration is the implemented subset, with text ids and timestamptz columns.
- **No auth** is resolved: every page and API route except sign-in/up, verify, reset, health and the stylesheet needs a session or token.
- **State is not encrypted** is resolved for state, plans and sensitive variables.
- Still open: sandboxing the engine, real email, TLS to the database, SSE, lock leases.

## 2026-11-08: test pass (replica-test)

9 bugs found and fixed (1 S1, 2 S2, 4 S3, 2 S4), each with a test that failed first.
115 automated tests pass; the end-to-end suite and the store contract also run on
PostgreSQL. Details: bugs.md, test-plan.md. New behaviour worth knowing:
- force-unlock now ends the run holding the lock (discard / cancel) and refuses while applying;
- the server recovers runs interrupted by a restart at startup (single instance only);
- run messages are limited to 200 characters, configurations to 1 MB;
- every state-changing request with a foreign Origin is refused, including sign-in and sign-out;
- under 960 px the sidebar becomes a strip under the top bar (design source: replica/design/tokens.css).

## 2026-11-08: OpenTofu http backend (parity item 1)

New: `crates/tf-server/src/backend.rs`, migration `0002_lock_info.sql`, Basic auth
for API tokens, state rights separate from read rights, a CLI card on the
workspace page, `tofu force-unlock` working for dashboard and run locks.
Store contract gained `lock_workspace(.., info)`, `unlock_workspace_owned`, and
state without a run. 131 tests pass on memory and PostgreSQL; the protocol was
also exercised with the real tofu 1.13.9 (see backend.md).

## 2026-11-08: the tofu driver for real, and its sandbox (parity item 2)

`TofuEngine` now runs the real binary: scratch directory per run with a forced
local backend, clean environment, per-step timeouts, streamed and capped output,
Windows job object (memory, process count, tree kill) or a throwaway Docker
container per step (`TF_SANDBOX=container`). Startup checks `tofu version`,
sweeps stale scratch directories and containers. Output-only plan changes are
now changes. Three real defects found by running the binary (B10 to B12).
Details and limits: backend.md, bugs.md, test-plan.md (E01).
Incident: the build disk (D:) filled up (0 GB free); freed `target/debug/incremental`,
then `cargo clean` and switched the dev profile to `debug = "line-tables-only"`,
`incremental = false`: a full test build now needs about 1.1 GB instead of 7.6 GB.

## 2026-11-08: live log, cancel and force-cancel (parity item 3)

- **New state** `force_canceled` (migration `0003`), event `ForceCancel`, right `ForceCancel` (owner/admin, never an org token). Run states: 11.
- **Engine:** `CancelToken`, `EngineError::Canceled { partial_state }`. The simulator stops at its pause points and returns what it finished; `sandbox::run` selects over exit, timeout and cancel, asks tofu to stop (docker `kill --signal=INT` / Unix `kill -INT`), waits `cancel_grace`, then kills the tree and removes the container; `TofuEngine::apply` salvages `terraform.tfstate` when it is valid JSON and differs from the input.
- **Server:** `runner::Cancels` registry; `run_event` (cancel/force-cancel reach the engine step; a force-cancel waits for the runner to record the partial state before the lock is released); SSE at `/runs/{id}/events` and `/api/v2/runs/{id}/events` (`stream.rs`); run page follows a working run by stream with a `<noscript>` reload fallback; force-cancel form with a risk checkbox for admins; per-stage durations in the stepper; force-unlock also stops the engine step.
- **Store:** `get_log_from(run, offset)`; contract test for force-cancel on both stores.
- **Found by testing:** B13 (a hard kill loses the state). That changed the design from "kill" to "ask, wait, then kill".
- **Checked in the built-in browser** against a running server: lines arrive one at a time with no reload, the page reloads once per status change, the force-cancel form states the risk, the banner and `Force canceled` badge appear, the stepper shows durations, the state kept 5 resources.
- 191 tests (was 160); 13 mutation checks.

## 2026-11-08: JSON:API, pagination, archive upload, rate limit (parity item 4)

- **API:** every `/api/v2` answer is a JSON:API document (`jsonapi.rs`): resource objects, relationships, links, `include`, paged lists with `links` and `meta.pagination`, document-form requests. New: workspace GET, run lists (per workspace and organization-wide), state-version list, configuration versions.
- **Upload:** two-step `.tar.gz` upload (`archive.rs`, `tar` and `flate2`): `.tf` files at the top level are joined; modules in subdirectories, `.tf.json`, links and oversize archives are refused with a reason. Store: `create_pending_config`, `upload_config`, a pending version cannot be run (migration `0004`).
- **Rate limit:** 30 requests/s per caller on `/api/` (`TF_API_RATE`), 429 with `Retry-After`.
- **Changed shape:** attributes moved under `attributes` with kebab-case names; the token secret moved to `meta.secret`. The 61 older `data` assertions in the e2e tests were rewritten for it. One timing test (`force_cancel_keeps_...`) was flaky on PostgreSQL (its window was shorter than three database calls); its step delay went from 400 ms to 1500 ms.
- **Not done / limits:** see `backend.md` (item 4 section): only the platform's own resource types, no local modules in archives, fixed-window limiter in process memory, token list not paged.
- 211 tests (was 191); 12 mutation checks, 11 red, 1 not caught.

## 2026-11-08: lock leases, workspace settings, state transfer URLs (parity item 5)

- **Locks:** a lock can have a lease and lapses by itself; API locks default to 1 h and can be renewed (`actions/heartbeat`), CLI locks get 12 h (tofu cannot renew), run locks and dashboard locks have none. Store signature change: `lock_workspace(..., lease_secs)`. Migration `0005`.
- **Settings:** `update_workspace`, `delete_workspace` (refused while locked or a run works), default auto-apply used by new runs, `PATCH`/`DELETE` on the API and a settings page. New rights `UpdateWorkspace`, `DeleteWorkspace`.
- **State:** `GET/POST` state-version endpoints, single-use 5-minute transfer URLs under `/archivist/v1/object/`. **Breaking:** the state document is no longer in any API answer (6 older assertions now fetch it through the URL). The CLI backend's state checks moved into one shared function used by both paths.
- **Found by testing:** the first lease test passed with a mutation that removed the check in `claim_next`, because the test read the workspace (which already clears lapses) before claiming. Reordered so claiming has to notice on its own. The rate-limit test was speed-dependent under load (sequential calls); it now fires its calls at once.
- **Not done / limits:** see `backend.md` (item 5 section): no retention policy or rollback, grants in process memory, CLI lease is a 12 h guess.
- 238 tests (was 211); 17 mutation checks, all red.

## 2026-11-08: /replica-entrepreneur

- Collected 712 public rows (386 Hacker News comments through the Algolia API, 227 opening posts on HashiCorp Discuss through its public search API, 100 of the most-voted OpenTofu issues through the GitHub API), `reviews.csv`. No star reviews exist for this product; G2, Capterra, Trustpilot and Reddit were not collected (see `fixes.md`).
- Custom themes in `themes.json` (the default consumer-app themes matched unrelated text). Counts are keyword matches; samples were read by hand and the precision is stated in `fixes.md`.
- A first filter for hiring posts wrongly removed 71 real comments (a "$10k/mo" matched a salary pattern); found when a quote failed its exact-substring check; fixed.
- Output: `feedback.md`, `fixes.md` (three lists, fix plan P1 to P7, three angles), five new rows in `features.csv`, gap verdicts in `gaps.md`.

## 2026-11-08: lint in the plan step (fix P1)

- New crate `tf-lint` with nine rules; `Run.lint`, `Store::set_lint`, migration `0006`; the runner lints before every plan; the run page, log and API show the findings; they never stop a run.
- The first draft of the "provider not declared" rule fires on configurations with no `terraform` block at all, which is most of the sample configurations in the tests; it only adds log lines, and it is a deliberate warning (pinning providers is the habit this rule teaches).
- Test timing: lease tests used 1-second leases, but a lease counted in whole seconds can lapse after a fraction of one, so they flaked; they now use 2 seconds and wait 3.2.
- 12 mutation checks: 11 turn a test red (two needed a stronger test first: one configuration had to use two different resource types of the same provider); the twelfth, accepting a mixed template as a lone interpolation, is an equivalent mutant because the check for a closing brace already rejects it.

## 2026-11-08: skip the refresh, drift checks (fix P3)

- Run options `skip_refresh` and `refresh_only` (migration `0007`), `Store::create_run_with`, drift schedule and last result on the workspace, a scheduler task, the `check-drift` action, settings page and workspace page changes. The engine passes `-refresh=false` / `-refresh-only` to tofu and reads `resource_drift` for the drift list.
- **Found on the way:** `latest_config` returned a configuration version that was still waiting for its upload (empty text); a drift check would have planned against nothing. Fixed in both stores, with a contract test.
- Limits stated in `backend.md`: real drift not exercised with a real provider; the 131-vote request (skip refresh only for unchanged resources) is not what a whole-plan switch does.
- 277 tests (was 259), run on PostgreSQL with the real tofu and Docker cases, none skipped; 14 mutation checks, all turn a test red (one, queuing a check without a state, needed a stronger test first).
