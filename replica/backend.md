# Backend

Updated 2026-11-07. Everything below is implemented and tested unless it says
**not done**. "Tested" means a test fails if the behaviour is removed: four of
the security checks were deliberately broken to prove their tests go red.

## What exists

| area | status | how it works |
| --- | --- | --- |
| Database | done | PostgreSQL via sqlx. `migrations/0001_init.sql` runs at startup. The `Store` trait has two implementations (in-memory, Postgres) that pass the same 20 contract tests. |
| Queue rule in the database | done | partial unique index `one_executing_run_per_workspace`, plus `FOR UPDATE SKIP LOCKED` in `claim_next`. A test inserts a second lock-holding run with raw SQL and expects the database to refuse it. |
| Sign up | done | email + password (10+ chars, argon2id), creates the user and their organization; an organization-name clash rolls the user back (no orphan account). |
| Email verification | done, dev mailer only | one-time token (sha256 stored), 24 h. `TF_REQUIRE_VERIFIED=1` blocks sign-in until verified. **Real email is not done**: `LogMailer` prints the link to the server log. |
| Password reset | done, dev mailer only | one-time token, 1 h, same answer whether or not the account exists, signs out every session. |
| Sessions | done | random 256-bit secret in an HttpOnly, SameSite=Lax cookie (`Secure` with `TF_COOKIE_SECURE=1`); only its sha256 is stored; 7 days. Sign out, sign out everywhere. |
| CSRF | done | cookie-authenticated writes need a same-origin `Origin` or `Referer`; bearer tokens are exempt. |
| Roles | done | owner, admin, member, and one function `tf_core::can(role, token_kind, action)`; every handler goes through `authz::ws_access` / `run_access`. |
| Tenant isolation | done | a record of another organization is 404 on all 22 id routes (a test walks them); a record of your own organization above your role is 403. |
| API tokens | done | personal (acts as you) and organization (cannot plan or apply); sha256 stored, secret shown once, expiry, revoke. Tokens cannot mint tokens: management needs a browser session. |
| Account deletion | done | needs the password; deletes the user, sessions, tokens, memberships and every organization where they were the only member, with its workspaces, runs and state. |
| Variables | done | input and environment variables, sensitive values write-only; reach the engine only as environment; masked as `(sensitive)` in every stored log line. |
| Encryption at rest | done | state, saved plans and sensitive variables are AES-256-GCM sealed (`enc:v1:`), key derived per purpose and per workspace/run with HKDF, workspace/run id bound as associated data (a blob moved elsewhere fails to open). Plaintext is refused on read. A check constraint stops plaintext sensitive variables reaching the table. |
| Rate limits | partial | login (10 per 15 min per email), sign-up (30/h overall), forgot (5/h per email), account delete (5 per 15 min). In memory, per process. **No general API rate limit yet.** |
| Payments (Stripe) | not applicable | billing is not part of this product's recon or architecture. Nothing built. |
| OAuth sign-in, magic links | **not done** | not in the recon map; needs the user's own Google/GitHub developer apps. |
| Seed script | **not done** | tests create their own fixtures. |
| Background jobs | partial | the in-process runner polls the queue. No retries, dead-letter log, drift or reaper jobs. |

## Configuration (`.env.example`)

`DATABASE_URL`, `TF_MASTER_KEY` (64 hex, required with a database: losing it
makes stored state unreadable), `TF_ADDR`, `TF_ENGINE`, `TF_COOKIE_SECURE`,
`TF_REQUIRE_VERIFIED`, `TF_BASE_URL`. `.env.local` (git-ignored) holds the dev
database URL and a generated key. No value is checked in.

## Security checklist

- [x] secrets only in env vars; `.env*` ignored; nothing in the page or client bundles (no JavaScript beyond a theme toggle)
- [~] input validated on the server: names, keys, emails, passwords, operations and categories are; request bodies use axum's default 2 MB limit; free-text fields (message, description) are length-limited in forms but not everywhere in the JSON API
- [x] authorisation on every read and write, tested with a second organization and with a lower role
- [~] rate limits on auth, sign-up and anything that sends email; **none on the rest of the API**
- [ ] webhooks verify signatures: **no webhooks exist yet** (VCS integration is M3)
- [ ] uploads: **none exist yet**
- [~] no user data in URLs or logs: one-time tokens are in verify/reset links by necessity; the dev mailer prints them, so it must never be used where logs are shared; engine errors are scrubbed of sensitive values
- [ ] dependencies audited: **`cargo audit` is not installed here; not run**
- [ ] privacy policy lists every processor: **not written**
- [ ] database backups: **the dev container has none**; enable the host's daily backups before real use
- [ ] TLS to PostgreSQL: **sqlx is built without a TLS feature**, so a database that requires TLS cannot be reached yet; add `tls-rustls` before pointing at a managed database
- [ ] cookie `Secure` flag: implemented and unit-tested, **never exercised behind real HTTPS**

## Known weaknesses (decide before launch)

- Sign-up reveals that an email is registered (duplicate email message). Login and reset do not.
- No breached-password check; length only.
- One active organization per user in the UI (the first by name). `Store::add_member` exists, but there is no invite or switch screen, so roles other than owner are only reachable through the store or tests.
- Workspace lock has no lease or heartbeat yet (gap G12): a crashed run holds its lock until someone force-unlocks.
- The simulated engine is the default; `TofuEngine` has never run against a real binary and is not sandboxed. Do not point it at untrusted configuration.
- Verification and reset links rely on the dev mailer; a real provider needs the user's own key (Resend or Postmark) and a verified sender domain.
- Sessions are never extended and old rows are not purged.

## How it was verified

- 92 tests: core 12, crypto 7, engine 7, server unit 5, end to end 21, store contract 20 (memory) and 20 (PostgreSQL 16). The end-to-end suite also runs against PostgreSQL with `TF_TEST_DATABASE_URL`.
- Mutation checks: removing the organization check, the CSRF check, the log masking, or widening the member role each makes exactly the intended test fail.
- Restart check on PostgreSQL: data created, server killed and restarted; the session cookie still works, state decrypts, the sensitive variable stays hidden; raw rows hold only `enc:v1:` ciphertext and 32-byte session hashes.
- In the browser: redirect to sign-in when signed out, sign-in with the form, workspace list from the database, variables page. Screenshots were unreliable in this environment, so these were checked through page text, not pixels.

## OpenTofu http backend (added 2026-11-08)

State for `tofu` projects can live here. Per workspace, in the project's `.tf`:

```hcl
terraform {
  backend "http" {
    address        = "https://HOST/backend/WORKSPACE"
    lock_address   = "https://HOST/backend/WORKSPACE/lock"
    unlock_address = "https://HOST/backend/WORKSPACE/lock"
    username       = "token"           # ignored; the password is what counts
  }
}
```

then `export TF_HTTP_PASSWORD=<personal API token>` and `tofu init`. The
workspace page shows this block with the right address.

| | |
| --- | --- |
| auth | HTTP Basic (password = API token) or bearer; unauthenticated requests get a `Basic` challenge |
| rights | reading and writing state need the state rights: owner, admin and member have them, **organization tokens do not** (state can hold secrets); this also applies to `GET /api/v2/workspaces/{id}/current-state-version` |
| lock | `LOCK` / `UNLOCK` (or POST/PUT and DELETE on `/lock`); the lock is `cli:<id>` and carries the CLI's own JSON back to other clients on conflict (423) |
| push | needs the lock (`?ID=`), checks `Content-MD5`, requires `version`, `serial` and `lineage`, only moves forward within one lineage, same serial with the same content is a no-op |
| versions | every push is a state version with no run, sealed like run state |
| purge | `DELETE` is refused (405); delete the workspace from the dashboard |
| force-unlock | `tofu force-unlock <ID>` works for CLI locks, for the dashboard lock (id `user`, needs the unlock right) and for a lock held by a run (id `run:...`, needs the admin right, and ends that run) |

Verified against the real client: OpenTofu v1.13.9 (windows amd64, official
release, sha256 matched the published checksum file; signatures not checked).
Run in a scratch project with the built-in `terraform_data` resource, so no
provider was downloaded and no cloud touched: `init`, `apply` (2 resources),
`plan` (no changes), edit and `apply` (state serial 1 to 2, matching tofu's
own), `output`, `state list`, a lock held from the dashboard (tofu printed the
holder and refused), a real session holding the lock via `tofu console` (a
second client was refused and shown the holder; `tofu force-unlock` released
it), `destroy` (serial 3, 0 resources). No local state file was ever written.

Known limits: the lock check and the write are two steps, so a force-unlock
landing between them could let one push through (milliseconds, admin-only);
the protocol is the `http` backend, not the `cloud` block or `tofu login`; the
simulated engine does not understand state pushed by the CLI (it is in
OpenTofu's own format), only the `tofu` driver does.

## Running `tofu` for real, and the sandbox (added 2026-11-08)

`TF_ENGINE=tofu` makes every run use a real OpenTofu binary instead of the
simulator. Anyone who can queue a run can then make that binary execute
arbitrary code (providers, `local-exec`, `external` data sources, `file()`), so
the driver contains it. Verified against OpenTofu 1.13.9 (Windows and Linux
official releases, checksums matched).

### What each run gets

A fresh scratch directory (`<work dir>/tfclone-<id>`, deleted afterwards) with:
the configuration as `main.tf`; `platform_override.tf` forcing a local backend,
so a `backend` or `cloud` block in the configuration can never send state
elsewhere; the state the platform holds; its own `HOME` and temp directory and
an empty CLI config file (the server user's `tofu login` credentials are never
read); stdin closed. Telemetry off (`CHECKPOINT_DISABLE`).

### Two levels of containment

| | process sandbox (default) | container sandbox (`TF_SANDBOX=container`) |
| --- | --- | --- |
| environment | built from nothing; the server's variables (`TF_MASTER_KEY`, `DATABASE_URL`, tokens) never reach tofu, providers or provisioners; run variables arrive; names the sandbox owns (`HOME`, `TF_CLI_CONFIG_FILE`, ... and in a container `DOCKER_*`, `PATH`) cannot be overridden and are reported in the log | same, and the container sees only variables passed by name |
| time | init 10 min, plan 15 min, apply 60 min (configurable); on timeout everything is killed | same; the container is removed explicitly |
| output | log keeps at most 8 MB, 20,000 lines, 16 KB per line, then says it truncated and keeps draining so the run finishes | same |
| memory, processes | Windows job object: 2 GB and 64 processes for the whole tree; the tree dies when tofu exits (a provisioner cannot leave a daemon behind) | `--memory`, `--pids-limit 512`, `--cpus 2` |
| filesystem | **not isolated**: configuration acts as the server's user | own read-only root filesystem, tmpfs `/tmp`, only the scratch directory shared; non-root (65534); no capabilities; no-new-privileges |
| network | **not isolated** | `--network none` by default (provider downloads then fail: use `bridge` or a mirror network on purpose) |
| platforms | Windows job objects tested; Unix process groups written but **not tested** here | Docker Desktop on Windows tested |

If authors are not trusted, use the container sandbox, or run the whole server
in a container or VM. The process sandbox alone only stops secret leaks and
runaway resource use.

### Settings

`TF_ENGINE=tofu`, `TF_TOFU_BIN` (default `tofu`), `TF_WORK_DIR`,
`TF_INIT_TIMEOUT_SECS`, `TF_PLAN_TIMEOUT_SECS`, `TF_APPLY_TIMEOUT_SECS`, `TF_CANCEL_GRACE_SECS` (how long a canceled step may take to stop by itself before it is killed; default 15),
`TF_MEMORY_LIMIT_MB` (0 = none), `TF_MAX_PROCESSES`, `TF_MAX_LOG_MB`,
`TF_ENV_PASSTHROUGH` (comma list of server variables to forward, e.g.
`HTTPS_PROXY`). For containers: `TF_SANDBOX=container`, `TF_CONTAINER_TOFU`
(a **Linux** tofu binary on the host, mounted read-only), `TF_CONTAINER_IMAGE`
(any image with a shell, default `alpine:3`), `TF_CONTAINER_NETWORK`,
`TF_CONTAINER_PIDS`, `TF_CONTAINER_CPUS`, `TF_CONTAINER_USER`, `TF_DOCKER_BIN`.
At startup the server runs `tofu version` (and stops if it fails), removes
scratch directories older than a day and containers a crashed server left.

### What was found by running the real binary, and fixed

- **S1** a provisioner could print `TF_MASTER_KEY` and `DATABASE_URL`: the child inherited the server's environment. Now a clean environment.
- **S2** a `backend` block in the configuration was honoured (state could be pushed to a server of the author's choosing, or the run broke): now overridden.
- **S3** a plan that only changed outputs was reported as "no changes" and never applied: output changes now count (`summary.outputs`, shown on the run page).
- tofu's output was only logged after the step finished and was fully buffered; it now streams line by line with the caps above.

### Tests

`crates/tf-engine/tests/real_tofu.rs` (13) needs `TF_TEST_TOFU_BIN`;
`real_container.rs` (9) needs `TF_TEST_TOFU_LINUX_BIN` and Docker;
two end-to-end cases in `tf-server/tests/e2e.rs` drive the platform with the
real binary. Without those variables the cases print SKIPPED and pass, so look
for SKIPPED before trusting a green run. Mutation checks (removing the
environment clean-up, the tree kill, the memory ceiling, the backend override,
the log cap, the output counting, each container flag) make the matching test
fail.

### Known limits

- The process sandbox shares the filesystem and network with the server (see the table).
- Windows: a process started by tofu in the instant before it joins the job object could escape it; tofu starts children much later, and memory and process ceilings still bind the rest.
- A provisioner that keeps `tofu`'s output pipe open (a background process that inherits stdout) holds the step until its timeout; this is tofu's behaviour.
- tofu itself prints nothing for output lines longer than about 64 KB (observed), so such lines never reach the log.
- Provider downloads are repeated every run (no shared plugin cache, deliberately: a shared cache would let one tenant poison another).
- Unix process groups and `--user` defaults on Linux hosts (bind-mount ownership) are untested.

## Live log, cancel and force-cancel (added 2026-11-08, parity item 3)

### Live log over Server-Sent Events

`GET /runs/{id}/events` (cookie) and `GET /api/v2/runs/{id}/events` (cookie or token), read right (`Read`), other organizations get 404.

| event | data | when |
| --- | --- | --- |
| `status` | `{"status","label"}` | first, and whenever the run's status changes |
| `log` | one stored line, `id:` = its 0-based position | as lines are stored |
| `done` | final status | after the run is final **and** the runner has finished with it, then the stream closes |

A run waiting for a person is not final, so the stream stays open across the pause. Resume with `Last-Event-ID` (browsers send it by themselves after a drop) or `?from=<position>`; the header wins; nonsense means "from the start". The lines are the stored ones, already masked, so a sensitive value cannot be streamed. `done` waits for the runner to finish with the run, because a canceled step writes its closing note after the status has already turned final. The server polls the store every 150 ms per viewer (no push channel from the runner yet: fine for one instance, to be replaced by a notifier before scaling out). Streams end by themselves after 30 minutes and the browser reconnects and resumes.

The run page follows a working run with a small inline script: lines are appended as they arrive; when the status changes the page reloads once so the stepper, banners and buttons are the server's. With scripts off, a `<noscript>` meta refresh keeps the old 2-second reload. A run waiting for a person, or finished, has neither. The stepper shows how long each finished stage took.

### Stopping a run

| action | who | allowed in | effect |
| --- | --- | --- | --- |
| `cancel` | owner, admin, member | pending, planning, apply queued | run ends `canceled`, lock freed, **the engine step is stopped** |
| `force-cancel` | owner, admin only (not org tokens) | planning, apply queued, **applying** | run ends `force_canceled`, lock freed after the engine has stopped and its state is recorded |
| (plain cancel while applying) | | | refused with 409: that is what force-cancel is for |

`POST /api/v2/runs/{id}/actions/force-cancel` on an *applying* run needs `{"acknowledge_risk": true}` in the body (the web form has a checkbox); anything else is 422. Reason: stopping an apply can leave resources that exist but are not recorded.

How a stop reaches the engine: the runner registers a cancel token per engine step (`runner::Cancels`); the handler changes the status first and then fires the token, and a step that starts in between re-reads the status and stands down, so no cancel is lost. For a force-cancel the order is: fire the token, **wait** (up to 45 s) for the runner to finish and record what the engine kept, then end the run and free the lock. A queued run therefore never plans against a state that is missing resources which exist. Force-unlock of a workspace (dashboard, API, `tofu force-unlock`) also stops the engine step of the run that held the lock.

What the engine does on a stop (`sandbox::run`):

1. **Asks tofu to stop** (the equivalent of Ctrl-C) and waits up to `cancel_grace` (15 s, `TF_CANCEL_GRACE_SECS`): tofu kills its provisioner, writes its state and exits. In a container this is `docker kill --signal=INT`; for a Unix process it is `kill -INT`. Verified with OpenTofu 1.13.9 in a container: exits in about a second, state keeps the finished resource and records the interrupted one as `tainted`, so the next plan replaces it.
2. If tofu has not exited, **kills** the whole process tree (job object / container removal).
3. Windows without a container has no way to send Ctrl-C to a hidden child, so it goes straight to the kill. **A killed tofu leaves an empty state file** (found while testing, B13): the platform then has no record of what the run created. The log says so.
4. Whatever valid state the working directory holds is returned as `Canceled { partial_state }`; the runner stores it as a new state version, logs which serial, and only then lets the lock go.

### Known limits

- Windows process mode cannot stop tofu gracefully (above); use the container sandbox, or Linux, for runs whose force-cancel must keep the record.
- The Unix process-mode interrupt (`kill -INT`) is written but was not run here; the container path (the recommended one) is.
- The event stream polls; many viewers of many runs means many small queries.
- No force-execute (running a queued run ahead of others), no ANSI colour in the live log, no "pending state version" rule while a stopped run's state is recorded.
- If the runner were stuck, a force-cancel would wait the full 45 s before ending the run.

### Tests

`tf-engine` unit tests (cancel token, simulator stop and partial state), `real_tofu.rs` (kill of a real apply and its children on Windows, early cancel), `real_container.rs` (graceful stop keeps the record; a tofu that does not stop in time is killed and the container removed), `tf-server` unit tests (resume rules, stage durations, form flags, cancel registry), end-to-end: SSE order, resume, privacy and masking, streaming while the engine works and staying open for a person, cancel stops the engine step, plain cancel refused while applying, acknowledgement required, finished resources kept and the next plan only does the rest, roles, the web form and double click, no reload under the reader, force-unlock stops the engine, an engine that is slow to stop (answer only after the record is stored; `done` only after the last line), and one end-to-end run through real tofu in a real container. Mutation checks (13) each turn at least one of these red: force-cancel while waiting for a person, members allowed, no acknowledgement, cancel not reaching the engine, no wait for the record, partial state dropped, `Last-Event-ID` ignored, `done` before the runner finished, no confirm box, simulator ignoring the cancel, tofu never asked to stop, no kill, partial state discarded.

## JSON:API, pagination, archive upload and rate limit (added 2026-11-08, parity item 4)

### Shape of the API (`jsonapi.rs`)

- Responses are JSON:API documents (`application/vnd.api+json`): `data` is a resource object with `id`, `type`, `attributes` (kebab-case, like the original: `resource-count`, `auto-apply`), `relationships` (foreign keys, never attributes) and `links.self`. Errors are `{"errors": [{"status", "title"}]}` with the same media type.
- Resource types: `workspaces`, `runs`, `state-versions`, `vars`, `configuration-versions`, `authentication-tokens`. A run's `actions` (what a person may do now) is an attribute.
- Lists: `GET /api/v2/workspaces`, `/workspaces/{id}/runs`, `/runs` (organization-wide), `/workspaces/{id}/state-versions`, `/workspaces/{id}/vars`. Newest first for runs and state versions. State lists never carry the state document (large, holds secrets); fetch one version at a time. The token list is not paged yet.
- Pagination: `page[number]` (from 1) and `page[size]` (default 20, a larger value is lowered to 100). Zero, negative or non-numeric values are 422, not guessed. A page past the end is empty. Every list has `links` (self, first, prev, next, last) and `meta.pagination` (current-page, page-size, prev-page, next-page, total-pages, total-count). Pagination slices in the server after reading the whole list: fine at this scale, not for a workspace with hundreds of thousands of runs.
- `include`: `current-run` on workspaces (list and single), `workspace` on a run. Anything else is a 422 that names what is offered. Included resources appear once.
- Requests: the JSON:API document form (`{"data": {"type", "attributes", "relationships"}}`, kebab-case or snake_case names) and, for convenience, the plain object. A wrong `data.type` is 409. A relationship with an id supplies `<name>_id`, so a run names its version with `relationships.configuration-version`.

### Configuration upload

1. `POST /api/v2/workspaces/{id}/configuration-versions` creates a version with status `pending`; the answer carries `links.upload`.
2. `PUT /api/v2/configuration-versions/{id}/upload` with the `.tar.gz` as the body (5 MB compressed at most). Status becomes `uploaded`; an upload is final (a second one is 409). A refused archive leaves the version pending so it can be retried.
3. `POST .../runs` with the version in `relationships.configuration-version` (or `configuration_version_id`). A pending version cannot be run (422); a version of another workspace is 404. Giving both `config` and a version, or neither, is 422.

What an archive may hold: `.tf` files at its top level, joined in name order into one configuration. Refused with a reason: `.tf.json`, `.tf` files in subdirectories (no local modules yet), links and special files, unsafe paths, more than 20 MB unpacked, more than 1 MB of configuration. Nothing is written to disk. Other files (README, lock file) are ignored. Rights: the same as queuing a run (members yes, organization tokens no, other organizations 404). Migration `0004` adds `configuration_versions.status`.

### Rate limit

At most `TF_API_RATE` (default 30) requests per second per caller on `/api/`; 0 turns it off. A user (session or user token) counts alone; organization tokens share one allowance per organization. Over the limit: 429, `Retry-After: 1`, an error document; every other answer carries `X-RateLimit-Limit`. Pages, `/healthz` and the tofu backend are not counted. Fixed one-second windows: a burst across a window edge can reach twice the limit. The counters live in the server process, so several instances each allow the full rate.

### Known limits

- Only the resource types the platform has; the original's API has many more (teams, projects, policies, ...). A client built for it will not work unchanged.
- No sparse fieldsets (`fields[...]`), no sorting or filtering parameters, no `include` on the state and variable lists.
- Archive upload: no local modules, no `.tf.json`, files are joined (error messages name the joined text, not the file; each file is marked with a `# file:` line).
- The unpacked-size cap on the gzip stream is a defense that no test discriminates (the per-entry size sum catches the cases tried).

### Tests

`jsonapi.rs` and `archive.rs` unit tests (13), contract test `a_pending_config_runs_only_after_its_upload` (both stores), e2e `j01` to `j10`, and a real-HTTP smoke test against the built binary (upload through `curl`, 90 calls on one connection: 30 answered, 60 got 429, and the allowance came back after a second). 12 mutation checks: 11 turn a test red; one (the gzip stream cap) does not.

## Lock leases, workspace settings and state transfer URLs (added 2026-11-08, parity item 5)

### Lock leases and heartbeat

- A lock can carry a lease (`Lock.expires_at`). When it runs out the lock is **no lock**: reading the workspace shows it unlocked, a new lock can be taken, and a queued run starts. In PostgreSQL one statement clears lapsed locks before every read or change of locks; the in-memory store does the same. Migration `0005` adds `lock_expires_at`.
- Who gets a lease: a lock taken through the API (default 3600 s, `TF_API_LOCK_LEASE_SECS`; the body may say `lease_seconds` from 1 to 604800, or 0 for none); a lock taken by the OpenTofu CLI (default 43200 s, `TF_CLI_LOCK_LEASE_SECS`). A run's lock never lapses (a crashed server is handled by the startup recovery). A lock taken on the dashboard never lapses: it is a person's deliberate act and the page offers Unlock.
- Why 12 hours for the CLI: the http backend protocol has no heartbeat, so the lease must outlast the longest apply. A CLI that crashes leaves its lock for at most 12 hours (an admin can force-unlock any time).
- `POST /api/v2/workspaces/{id}/actions/heartbeat` (optional `lease_seconds`, default the API lease) renews an API lock: it now lapses that long from now. 409 when not locked (the lock may have lapsed), locked by someone else (a run, the CLI), or the lock has no lease. The dashboard shows when a leased lock lapses.
- The lock owner for API and dashboard locks is the single name `user`, so any member can renew or release them, as before.

### Workspace settings

- Name (unique per organization), description (500 characters), default auto-apply. Runs that do not say whether to auto-apply use the workspace's setting (API: leave `auto_apply` out; dashboard: the box starts ticked). Rights: owners and admins, organization tokens included; members read.
- `PATCH /api/v2/workspaces/{id}`, `DELETE /api/v2/workspaces/{id}` (204), and the page `/workspaces/{id}/settings`. Delete removes runs, configuration, variables and state versions and is **refused (409) while the workspace is locked or a run is planning or applying**. It does not touch the infrastructure; the page says so and asks for the name to be typed.
- Renaming changes the OpenTofu backend address (`/backend/{name}`); the page warns.

### State transfer URLs (`grants.rs`)

- `GET /api/v2/workspaces/{id}/current-state-version` and `GET /api/v2/state-versions/{id}` return the version's metadata and `hosted-state-download-url`. **The state document is no longer inside any API answer.** Listings carry no URLs.
- `POST /api/v2/workspaces/{id}/state-versions` with `serial` (and optionally a hex `md5`) returns `hosted-state-upload-url`. `PUT` the state document there. The server checks what the CLI backend checks (valid Terraform state, serial and lineage only forward, same state again is accepted) plus that the body's serial and md5 are the declared ones, and refuses (423) while a run or the CLI holds the lock.
- A URL is `/archivist/v1/object/<random 256-bit token>`: no login (the URL is the credential), valid 5 minutes, **used once whatever the outcome**. Unknown, used and expired look the same (404). Responses carry `Cache-Control: no-store`; downloads carry `Content-MD5`. Requesting a URL needs the state rights (organization tokens do not have them).
- The grants live in the server's memory: they do not survive a restart and do not work across instances. A client that finds its URL gone asks for a new one. At most 10 000 are pending at once.

### Known limits

- No state retention policy and no rollback. No Terraform version pin, tags, projects or VCS settings on a workspace.
- Fixed leases: no per-lock owner identity beyond `user` and the CLI's lock id; no event record of a lapse.
- A lapse is noticed when something reads or changes locks, not by a timer: nothing observes it in between, which is also all that matters since every reader checks first.
- The CLI lease is a guess at "longer than any apply"; an apply longer than 12 hours would lose its lock, and a push after that is refused (423) rather than silently overwriting.

### Tests

Store contract tests (both stores): lease lapses and frees the workspace, takeover of a lapsed lock, heartbeat rules, settings validation, delete cascade, one state version fetch. Unit tests for grants (single use, expiry, token shape). e2e `k01` to `k12`. A real-HTTP smoke test against the built binary: download works once without a login, upload with md5 works once, the CLI backend then pulls the uploaded state, a 2-second lease is renewed by a heartbeat and still holds 3 seconds later, delete is refused while locked and succeeds after unlock. 17 mutation checks: all turn a test red (one needed a stronger test first: claiming a run had to notice a lapsed lock before anything else had read the workspace).
