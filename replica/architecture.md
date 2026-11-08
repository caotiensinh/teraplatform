# Architecture: Rust IaC run platform (phase 1)

Built from `recon.md` and `features.csv`. The user asked for Rust, so the
skill's default web stack (Next.js, Supabase, Vercel) is swapped out.
Decisions marked **verify** depend on pages the recon did not read.

## 1. Stack

| layer | choice | why |
| --- | --- | --- |
| language | Rust (stable, edition 2024) | user requirement; single static binary, safe concurrency for the run queue |
| HTTP server | axum + tokio + tower | standard async stack, middleware for auth and rate limit |
| database | PostgreSQL 15+ | one database; transactions and partial unique indexes enforce the queue and lock rules |
| DB access | sqlx (compile-time checked queries) + sqlx migrations | no ORM magic; SQL stays visible |
| web UI | server-rendered HTML with askama templates + htmx, SSE for logs | no JS build chain, fast, one binary serves everything |
| styling | plain CSS with design tokens (from replica-design) | small, no framework |
| auth | argon2id passwords, session cookies for UI, hashed bearer tokens for API | tokens stored as sha256, shown once |
| blob storage | `object_store` crate: local disk, S3, R2 behind one trait | state, configs, plans are blobs; pluggable fixes gap G6 |
| encryption | AES-256-GCM with a data key from env or KMS; per-workspace derived keys | state, plans and sensitive variables encrypted at rest (feature F-state-encryption) |
| engine (phase 1) | spawn the `tofu` binary in a sandboxed temp dir | gets plan/apply parity without writing an HCL engine; engine is behind a trait so a native one can replace it |
| jobs | Postgres queue (`FOR UPDATE SKIP LOCKED`) + in-process worker pool | no extra service |
| realtime | SSE from `run_logs` chunks, resume by `Last-Event-ID` | simple, proxy friendly |
| CLI | `tfc` crate in the same workspace: login, run, state, lock | thin client over the API |
| config / obs | `config` + env vars, `tracing` + OpenTelemetry | |
| hosting | any host that runs one container + Postgres (Fly, Render, a VM) | self-hostable is a selling point against the closed original |

Cargo workspace layout:

```
crates/
  core/       domain types, run state machine, permissions (pure, no IO)
  store/      sqlx queries, migrations, blob storage trait
  engine/     Engine trait; TofuProcess impl (phase 1); native impl (phase 2)
  api/        axum routes, JSON:API (de)serialisation, auth, rate limit
  web/        askama templates, htmx handlers, SSE
  runner/     job worker: executes run steps, streams logs
  policy/     own rule language parser + evaluator
  cli/        `tfc` binary
```

## 2. Schema

See `schema.sql`: **22 tables** (users, orgs, org_members, teams,
team_members, projects, access_grants, agent_pools, workspaces, tokens,
configuration_versions, runs, run_events, run_logs, state_versions,
variables, policy_sets, policy_set_targets, policy_results, notifications,
audit_log, jobs). Access rule: one authorisation check per query in the data
layer, always scoped by `org_id`; forbidden and missing both return 404.

Hard constraints enforced in the database, not the UI:

- `one_active_run_per_workspace`: partial unique index, so two runners can
  never execute the same workspace at once.
- `unique (workspace_id, serial)` on state versions: no duplicate serials.
- sensitive variables: check constraint forbids a plain value; write once.
- workspace lock carries a lease (`lock_lease_until`) so a crashed run frees
  itself (improves gap G12).

## 3. API

JSON:API under `/api/v2`, bearer tokens. 30 req/s per user. Page size default
20, max 100. Org tokens cannot plan or apply.

| method path | does | who | flow |
| --- | --- | --- | --- |
| POST /auth/login | session for UI | anyone | F01 |
| GET/POST /organizations | list, create | user | F01 |
| GET/POST /organizations/:org/projects | list, create | org member | F01 |
| GET/POST /organizations/:org/workspaces | list (filter tag, project), create | team read / org token create | F01 |
| GET/PATCH/DELETE /workspaces/:id | show, update, force-delete | read / admin | F01 |
| POST /workspaces/:id/actions/lock | lock with reason | lock permission | F05 |
| POST /workspaces/:id/actions/unlock | unlock; 503 while latest state pending | lock permission | F05 |
| POST /workspaces/:id/actions/force-unlock | admin unlock | admin | F05 |
| POST /workspaces/:id/actions/safe-delete | delete only if no resources | admin | F01 |
| POST /workspaces/:id/configuration-versions | create | write | F08 |
| PUT /configuration-versions/:id/upload | upload archive, auto-creates run | write | F08 |
| POST /runs | create run | plan/write, not org token | F02 |
| GET /runs/:id | show, include plan/apply/policy | read | F02 |
| GET /workspaces/:id/runs, GET /organizations/:org/runs | list with filters (status, operation, user, commit) | read | F02 |
| POST /runs/:id/actions/apply | confirm; 202 queues; 409 if not paused | write | F02 |
| POST /runs/:id/actions/discard | discard | write | F02 |
| POST /runs/:id/actions/cancel, /force-cancel, /force-execute | cancel variants | write / admin | F02 |
| GET /runs/:id/logs (SSE) | live plan/apply log | read | F02 |
| GET /workspaces/:id/current-state-version | latest state | read | F04, F07 |
| GET/POST /workspaces/:id/state-versions | list; create (workspace must be locked by caller) | read / write | F04 |
| GET /state-versions/:id/download | blob (single-use signed URL, 25 h) | read | F04 |
| PATCH /workspaces/:id/state-versions | rollback to chosen version | admin | F04 |
| GET/POST/PATCH/DELETE /workspaces/:id/vars | variables; sensitive write once | read / write | F01 |
| GET/POST/PATCH/DELETE /organizations/:org/policy-sets | policy sets | admin | F06 |
| POST /runs/:id/actions/override-policy | override soft failure | override permission | F06 |
| GET/POST/DELETE /tokens, /teams/:id/tokens, /organizations/:org/tokens | tokens | owner of the token kind | all |
| GET/POST /workspaces/:id/notification-configurations | notifications | write | F02 |
| GET /organizations/:org/audit-trail | audit log | audit token | all |
| HTTP backend: GET/POST/DELETE /state/:workspace, LOCK, UNLOCK | state backend for the OpenTofu `http` backend | write | F07, **verify** vs cloud-backend protocol |

Count: about 40 routes. Web UI routes (S01-S18) are server-rendered pages
that call the same handlers.

Webhooks in: GitHub push and pull request (signature checked, idempotent by
delivery id). Webhooks out: notification destinations on run events.
Only official public APIs with the user's own keys.

Background jobs: `run_step` (executes the next stage), `notify`,
`drift_check` (scheduled refresh-only run, free tier), `state_cleanup`
(retention), `lock_reaper` (expires dead leases, every 30 s).

## 4. Run state machine (crates/core)

Pure function `next(status, event) -> status` covering every state in
recon section 5. Unit-tested as a table. Runner steps:
fetch config -> `tofu init` -> `tofu plan -out` -> policy -> (cost) ->
wait for confirm -> `tofu apply plan` -> upload state -> finalize state
version -> unlock. Each step is idempotent and resumable from the job row.

## 5. The parts that bite

- **Sandboxing the engine.** Running user HCL means running arbitrary
  provider code. Run each job in a temp dir with a dropped-privilege user,
  no host env, egress rules, CPU/memory/time limits. Agent mode is the long
  term answer.
- **State consistency.** The same serial must never be written twice; unlock
  must wait for a finalized state version; state upload is a transaction
  (blob first, row second, pointer last).
- **Idempotency.** GitHub webhooks and API retries arrive twice: dedupe by
  delivery id and an `Idempotency-Key` header on run creation.
- **Races.** Queue ordering relies on the partial unique index plus
  `SKIP LOCKED`; covered by a concurrency test with many workers.
- **Secrets.** Sensitive variables and credentials are injected as env into
  the child process only, never written to disk, never logged (log scrubber
  on known secret values).
- **Time zones.** All timestamptz UTC; UI renders local time.
- **Log volume.** Logs are chunked rows with retention; large plans stream
  rather than buffer.
- **Rate limits.** tower governor per user; stricter on run-list endpoints.
- **Multi-tenancy.** Every query takes `org_id`; tested with a cross-tenant
  test that must return 404.
- **GDPR deletion.** User delete anonymises audit rows instead of deleting.
- **Provider licences.** Downloading providers from public registries is
  allowed; bundling them is not our concern in phase 1.
- **Licence of the engine.** Phase 1 shells out to OpenTofu (MPL-2.0), a
  separate process, so our code keeps its own licence. Do not depend on
  Terraform (BSL) for a competing product.

## 6. Policy language (own, replaces Sentinel)

Small declarative rules over the plan JSON, evaluated in Rust, e.g.
`deny when resource.type == "aws_s3_bucket" and not resource.after.versioning`.
Levels: advisory, soft-mandatory (override), hard-mandatory. Linting rules
(G1) run in the same pass.

## 7. Build order

1. **Vertical slice (M1).** Sign up, create org and workspace, upload a
   config, run `tofu plan` and `apply` through the queue, store state, show
   the log. Screens S01, S04, S05, S07. Tables users, orgs, org_members,
   projects, workspaces, configuration_versions, runs, run_logs,
   state_versions, jobs. Routes: login, workspaces, configuration-versions,
   runs, logs, current-state-version.
2. **Must-haves (M2).** Locking with lease, state versions and rollback,
   variables with sensitive values, tokens, full run state machine and
   actions, HTTP state backend, CLI `tfc`, state encryption, JSON:API rules.
   Screens S02, S03, S06, S09, S10, S11, S18.
3. **Should-haves (M3).** VCS webhooks and speculative PR plans, teams and
   permissions, policy sets and overrides, drift detection, audit log,
   notifications, agents. Screens S12 to S17.
4. **The fixes (M4).** After `/replica-entrepreneur` ranks real complaints:
   lint, partial planning for unknowns (G2), secret-free disk (G3), faster
   plan (G8), then the native Rust engine behind the `Engine` trait.

## 8. Open questions (verify before M2)

- OpenTofu `http` backend vs the `cloud` backend protocol for `tofu plan`
  streaming remote runs (the recon did not read the protocol page).
- Exact locking semantics from the OpenTofu locking page (not read).
- Which VCS to support first (GitHub assumed).
